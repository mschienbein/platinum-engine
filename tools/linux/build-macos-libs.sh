#!/bin/sh
# Build the public macOS arm64 native dependencies in a new directory.
# Usage: sh tools/linux/build-macos-libs.sh NEW_OUTPUT_DIR
set -eu

[ "$#" -eq 1 ] || { echo 'usage: build-macos-libs.sh NEW_OUTPUT_DIR' >&2; exit 2; }
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || {
  echo 'macOS arm64 is required' >&2; exit 1;
}
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
mkdir "$1"
out=$(CDPATH= cd -- "$1" && pwd)
mkdir -p "$out/archives" "$out/lib" "$out/relink" "$out/receipt" \
  "$out/source/native" "$out/source/linux/patches/lame" "$out/source/tools/linux" "$out/work"

# Freeze the repository-owned inputs before they are used.
cp "$root/native/taglib_bridge.cpp" "$root/native/taglib_bridge.h" "$out/source/native/"
cp "$root/linux/patches/lame/0001-remove-obsolete-export.patch" "$out/source/linux/patches/lame/"
cp "$root/tools/linux/build-macos-libs.sh" "$out/source/tools/linux/"

if [ -n "${PN_NATIVE_ARCHIVES_DIR:-}" ]; then
  cp "$PN_NATIVE_ARCHIVES_DIR/taglib.tar.gz" "$PN_NATIVE_ARCHIVES_DIR/flac.tar.xz" \
    "$PN_NATIVE_ARCHIVES_DIR/lame.tar.gz" "$out/archives/"
else
  curl -fLsS https://github.com/taglib/taglib/archive/e36a9cabb9882e61276161c23834d966d62073b7.tar.gz -o "$out/archives/taglib.tar.gz"
  curl -fLsS https://github.com/xiph/flac/releases/download/1.4.2/flac-1.4.2.tar.xz -o "$out/archives/flac.tar.xz"
  curl -fLsS https://ftp.debian.org/debian/pool/main/l/lame/lame_3.100.orig.tar.gz -o "$out/archives/lame.tar.gz"
fi
(
  cd "$out/archives"
  printf '%s\n' \
    '5c80b1e5273849122ef11b5a9116e4b7f182b899cfdb28aefb704087216e1360  taglib.tar.gz' \
    'e322d58a1f48d23d9dd38f432672865f6f79e73a6f9cc5a5f57fcaa83eb5a8e4  flac.tar.xz' \
    'ddfe36cab873794038ae2c1210557ad34857a4b6bdc515785d1da9e175b1da1e  lame.tar.gz' | shasum -a 256 -c -
)
tar -xzf "$out/archives/taglib.tar.gz" -C "$out/work"
tar -xf "$out/archives/flac.tar.xz" -C "$out/work"
tar -xzf "$out/archives/lame.tar.gz" -C "$out/work"

cmake -S "$out/work/taglib-e36a9cabb9882e61276161c23834d966d62073b7" \
  -B "$out/work/build-taglib" -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_OSX_ARCHITECTURES=arm64 -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
  -DCMAKE_INSTALL_PREFIX="$out/work/taglib-install" \
  -DBUILD_SHARED_LIBS=OFF -DBUILD_BINDINGS=OFF -DBUILD_TESTS=OFF -DBUILD_EXAMPLES=OFF
cmake --build "$out/work/build-taglib" --parallel 4
cmake --install "$out/work/build-taglib"
# The shipped object is the object actually used to link the shipped bridge.
xcrun clang++ -std=c++17 -fobjc-arc -DTAGLIB_STATIC \
  -I"$out/work/taglib-install/include/taglib" -x objective-c++ -c \
  "$out/source/native/taglib_bridge.cpp" -o "$out/relink/taglib_bridge.o"
xcrun clang++ -std=c++17 -fobjc-arc -O0 -dynamiclib "$out/relink/taglib_bridge.o" \
  "$out/work/taglib-install/lib/libtag.a" \
  -framework Foundation -framework AppKit -framework CoreGraphics \
  -framework ImageIO -framework CoreServices -lz -o "$out/lib/libpn-tag-bridge.dylib"
cp "$out/work/taglib-install/lib/libtag.a" "$out/relink/libtag.a"

cmake -S "$out/work/flac-1.4.2" -B "$out/work/build-flac" \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_ARCHITECTURES=arm64 \
  -DBUILD_SHARED_LIBS=ON -DWITH_OGG=OFF -DBUILD_CXXLIBS=OFF \
  -DBUILD_PROGRAMS=OFF -DBUILD_TESTING=OFF -DBUILD_EXAMPLES=OFF \
  -DBUILD_DOCS=OFF -DBUILD_UTILS=OFF
cmake --build "$out/work/build-flac" --parallel 4
cp -P "$out/work/build-flac/src/libFLAC/libFLAC.12.0.0.dylib" \
  "$out/work/build-flac/src/libFLAC/libFLAC.12.dylib" \
  "$out/work/build-flac/src/libFLAC/libFLAC.dylib" "$out/lib/"

patch -d "$out/work/lame-3.100" -p1 -N < "$out/source/linux/patches/lame/0001-remove-obsolete-export.patch"
printf '%s  %s\n' 0f4570fddb684f87b04ae6e36a7919d9c6928423d2a761d53a90cd21edb76ecd \
  "$out/work/lame-3.100/include/libmp3lame.sym" | shasum -a 256 -c -
(
  cd "$out/work/lame-3.100"
  ./configure --prefix="$out/work/lame-install" --enable-shared --disable-static \
    --disable-frontend --disable-analyzer-hooks --disable-dependency-tracking \
    CFLAGS='-O2 -ffp-contract=off'
  make -j4
)
cp -P "$out/work/lame-3.100/libmp3lame/.libs/libmp3lame.0.dylib" \
  "$out/work/lame-3.100/libmp3lame/.libs/libmp3lame.dylib" "$out/lib/"
cp "$out/work/build-taglib/CMakeCache.txt" "$out/receipt/taglib-CMakeCache.txt"
cp "$out/work/build-flac/CMakeCache.txt" "$out/receipt/flac-CMakeCache.txt"
cp "$out/work/build-flac/config.h" "$out/receipt/flac-config.h"
cp "$out/work/lame-3.100/config.h" "$out/receipt/lame-config.h"
cp "$out/work/lame-3.100/config.log" "$out/receipt/lame-config.log"
{
  printf '%s\n' 'macOS arm64; public TagLib 1.11.1, FLAC 1.4.2 and LAME 3.100' \
    'Settings and source URLs: source/tools/linux/build-macos-libs.sh and docs/BUILD.md' \
    'Bridge compilation and linkage use the object in relink/taglib_bridge.o.'
  sw_vers
  xcrun clang++ --version
  cmake --version
  xcrun --show-sdk-path
} > "$out/receipt/build.txt"
