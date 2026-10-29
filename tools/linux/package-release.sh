#!/bin/sh
# Build a rootless release payload and archive in a new output directory.
# Usage: sh tools/linux/package-release.sh PLATFORM VERSION NEW_OUTPUT_DIR
#   PLATFORM: linux-amd64, linux-arm64, macos-arm64
# This never publishes.
set -eu

usage() {
  echo 'usage: package-release.sh {linux-amd64|linux-arm64|macos-arm64} VERSION NEW_OUTPUT_DIR' >&2
  exit 2
}
[ "$#" -eq 3 ] || usage
platform=$1
version=$2
output=$3
case "$platform" in linux-amd64|linux-arm64|macos-arm64) ;; *) usage ;; esac
case "$version" in v?*) ;; *) usage ;; esac
case "$version" in *[!A-Za-z0-9_.-]*) usage ;; esac

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
# Do not overwrite an existing package, work tree, archive, or intermediate.
mkdir "$output"
out=$(CDPATH= cd -- "$output" && pwd)
mkdir "$out/payload"
payload=$out/payload

case "$platform" in
  linux-*)
    arch=${platform#linux-}
    daemon=$(docker info --format '{{.OSType}}/{{.Architecture}}')
    case "$arch:$daemon" in
      amd64:linux/amd64|amd64:linux/x86_64|arm64:linux/arm64|arm64:linux/aarch64) ;;
      *) echo "Docker daemon is $daemon, not a native linux/$arch builder" >&2; exit 1 ;;
    esac
    # Snapshot precisely the inputs consumed by the pinned bridge target.
    context=$out/context
    mkdir -p "$context/native" "$context/linux" "$context/src/portable" \
      "$context/tools/portable/bridge" "$context/tools/portable/math/shim/src"
    cp "$root/linux/Dockerfile" "$context/Dockerfile"
    cp -R "$root/linux/patches" "$context/linux/patches"
    cp "$root/native/taglib_bridge_portable.cpp" "$root/native/taglib_bridge.h" "$context/native/"
    cp "$root/src/portable/math.rs" "$context/src/portable/math.rs"
    cp "$root/tools/portable/bridge/bridge.map" "$context/tools/portable/bridge/"
    cp "$root/tools/portable/math/shim/Cargo.toml" "$root/tools/portable/math/shim/Cargo.lock" \
      "$root/tools/portable/math/shim/pn_codec_math.h" "$context/tools/portable/math/shim/"
    cp "$root/tools/portable/math/shim/src/lib.rs" "$context/tools/portable/math/shim/src/"
    # Docker COPY retains source modes but makes root the owner. The build
    # container's unprivileged packaging step must be able to read its public
    # inputs and copy read-only frozen source directories into the payload.
    chmod -R u+rwX,go+rX "$context"
    docker build --platform "linux/$arch" --target bridge --iidfile "$out/native-image-id.txt" "$context"
    image=$(cat "$out/native-image-id.txt")
    # This helper builds in the same pinned glibc image with the portable feature.
    PN_LINUX_VENDOR_DIR= PN_LINUX_REGISTRY_DIR= PN_LINUX_RUN_TESTS=0 \
      sh "$root/tools/linux/build-cli.sh" "$image" "$out/cli"
    mkdir -p "$out/native/lib" "$out/native/archives" "$out/native/relink" \
      "$out/native/receipt" "$out/native/source/linux" "$out/native/source/native" \
      "$out/native/source/src/portable" "$out/native/source/tools/portable/bridge" \
      "$out/native/source/tools/portable/math" "$out/native/rust-license/licenses"
    docker run --rm --read-only --user "$(id -u):$(id -g)" \
      --tmpfs /tmp:exec,mode=1777 -e CARGO_HOME=/out/cli/cargo \
      -e CARGO_TARGET_DIR=/out/cli/target -e RUSTUP_TOOLCHAIN=1.99.1 \
      -v "$root:/src:ro" -v "$out:/out" -w /src "$image" sh -eu -c '
        cp -P /opt/platinum-native/lib/libFLAC.so* /out/native/lib/
        cp -P /opt/platinum-native/lib/libmp3lame.so* /out/native/lib/
        cp /opt/platinum-native/lib/libpn-tag-bridge.so /out/native/lib/
        cp /opt/platinum-native/lib/libtag.a /opt/platinum-native/lib/libpn_codec_math.a /out/native/relink/
        cp /build/taglib.tar.gz /build/flac.tar.xz /build/lame.tar.gz /out/native/archives/
        cp -R /opt/platinum-native/receipt/. /out/native/receipt/
        cp -R /build/patches /out/native/source/linux/patches
        cp /build/native/taglib_bridge_portable.cpp /build/native/taglib_bridge.h /out/native/source/native/
        cp /build/native/bridge.map /out/native/source/tools/portable/bridge/
        cp /build/src/portable/math.rs /out/native/source/src/portable/
        cp -R /build/tools/portable/math/shim /out/native/source/tools/portable/math/shim
        # Recreate the original compilation unit from the image inputs and
        # require that linking it recreates the actual bridge in the image.
        "$CXX" -std=c++17 -O2 -fPIC -fvisibility=hidden -DTAGLIB_STATIC \
          -I/opt/platinum-native/include/taglib -c /build/native/taglib_bridge_portable.cpp \
          -o /out/native/relink/taglib_bridge.o
        "$CXX" -std=c++17 -O2 -fPIC -fvisibility=hidden -shared -DTAGLIB_STATIC \
          -I/opt/platinum-native/include/taglib /out/native/relink/taglib_bridge.o \
          /opt/platinum-native/lib/libtag.a -Wl,--exclude-libs,ALL \
          -Wl,--version-script=/build/native/bridge.map -lz -o /tmp/relinked-bridge.so
        cmp /opt/platinum-native/lib/libpn-tag-bridge.so /tmp/relinked-bridge.so
        cargo metadata --locked --format-version 1 > /out/cli/cargo-metadata.json
        { rustc --version; cargo --version; uname -sm; } > /out/native/receipt/rust-build.txt
        rust_docs="$(rustc --print sysroot)/share/doc/rust"
        cp "$rust_docs/COPYRIGHT-library.html" /out/native/rust-license/
        cp -R "$rust_docs/licenses/." /out/native/rust-license/licenses/
      '
    cp "$out/context/Dockerfile" "$out/native/source/linux/Dockerfile"
    cp "$out/native-image-id.txt" "$out/native/receipt/image-id.txt"
    ;;
  macos-arm64)
    [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || {
      echo 'macOS arm64 is required' >&2; exit 1;
    }
    sh "$root/tools/linux/build-macos-libs.sh" "$out/native"
    mkdir -p "$out/cli/cargo" "$out/cli/target" "$out/native/rust-license/licenses"
    (
      cd "$root"
      export CARGO_HOME="$out/cli/cargo" CARGO_TARGET_DIR="$out/cli/target" RUSTUP_TOOLCHAIN=1.99.1
      cargo build --locked --release --bin platinum
      cargo metadata --locked --format-version 1 > "$out/cli/cargo-metadata.json"
      { rustc --version; cargo --version; uname -sm; } > "$out/native/receipt/rust-build.txt"
      rust_docs="$(rustc --print sysroot)/share/doc/rust"
      cp "$rust_docs/COPYRIGHT-library.html" "$out/native/rust-license/"
      cp -R "$rust_docs/licenses/." "$out/native/rust-license/licenses/"
    )
    ;;
esac
native=$out/native

# Refuse any missing, mismatched or substituted upstream source archive.
python3 - "$native/archives" <<'PY'
import hashlib, pathlib, sys
root = pathlib.Path(sys.argv[1])
expected = {
    'taglib.tar.gz': '5c80b1e5273849122ef11b5a9116e4b7f182b899cfdb28aefb704087216e1360',
    'flac.tar.xz': 'e322d58a1f48d23d9dd38f432672865f6f79e73a6f9cc5a5f57fcaa83eb5a8e4',
    'lame.tar.gz': 'ddfe36cab873794038ae2c1210557ad34857a4b6bdc515785d1da9e175b1da1e',
}
for name, digest in expected.items():
    data = (root / name).read_bytes()
    if hashlib.sha256(data).hexdigest() != digest:
        raise SystemExit(f'{name}: upstream SHA-256 mismatch')
PY

mkdir -p "$payload/bin" "$payload/lib" "$payload/licenses/taglib" "$payload/licenses/flac" \
  "$payload/licenses/lame" "$payload/licenses/apple-alac" "$payload/licenses/rust" \
  "$payload/corresponding-source/archives" "$payload/corresponding-source/relink" \
  "$payload/corresponding-source/receipt" "$payload/corresponding-source/src/portable/audio" \
  "$payload/corresponding-source/tools/portable/mp3dec"
cp "$out/cli/target/release/platinum" "$payload/bin/platinum"
cp -R "$native/lib/." "$payload/lib/"
cp "$root/LICENSE" "$payload/LICENSE"
cp -R "$native/rust-license" "$payload/licenses/rust-standard-library"
cp "$root/Cargo.toml" "$root/Cargo.lock" "$root/rust-toolchain.toml" "$payload/corresponding-source/"
cp "$root/src/portable/audio/alac.rs" "$payload/corresponding-source/src/portable/audio/"
cp "$root/src/portable/audio/mp3_tables.rs" "$payload/corresponding-source/src/portable/audio/"
cp "$root/tools/portable/mp3dec/gen_tables.py" "$payload/corresponding-source/tools/portable/mp3dec/"
cp -R "$native/source/." "$payload/corresponding-source/"
cp -R "$native/archives/." "$payload/corresponding-source/archives/"
cp -R "$native/relink/." "$payload/corresponding-source/relink/"
cp -R "$native/receipt/." "$payload/corresponding-source/receipt/"

# For the corresponding-source bundle, retain the original recipe and provide
# an equivalent recipe that reads the bundled archives rather than fetching
# them again. Refuse to generate it if the recipe's download block has changed.
if [ "$platform" != macos-arm64 ]; then
  python3 - "$payload/corresponding-source/linux/Dockerfile" \
    "$payload/corresponding-source/linux/Dockerfile.bundled-sources" <<'PY'
import pathlib, sys
source, dest = map(pathlib.Path, sys.argv[1:])
text = source.read_text()
old = """RUN curl -fL https://github.com/taglib/taglib/archive/e36a9cabb9882e61276161c23834d966d62073b7.tar.gz -o taglib.tar.gz \\
 && echo '5c80b1e5273849122ef11b5a9116e4b7f182b899cfdb28aefb704087216e1360  taglib.tar.gz' | sha256sum -c - \\
 && curl -fL https://github.com/xiph/flac/releases/download/1.4.2/flac-1.4.2.tar.xz -o flac.tar.xz \\
 && echo 'e322d58a1f48d23d9dd38f432672865f6f79e73a6f9cc5a5f57fcaa83eb5a8e4  flac.tar.xz' | sha256sum -c - \\
 && curl -fL https://ftp.debian.org/debian/pool/main/l/lame/lame_3.100.orig.tar.gz -o lame.tar.gz \\
 && echo 'ddfe36cab873794038ae2c1210557ad34857a4b6bdc515785d1da9e175b1da1e  lame.tar.gz' | sha256sum -c - \\
 && tar --no-same-owner -xzf taglib.tar.gz && tar --no-same-owner -xf flac.tar.xz && tar --no-same-owner -xzf lame.tar.gz"""
new = """COPY archives/ /build/
RUN echo '5c80b1e5273849122ef11b5a9116e4b7f182b899cfdb28aefb704087216e1360  taglib.tar.gz' | sha256sum -c - \\
 && echo 'e322d58a1f48d23d9dd38f432672865f6f79e73a6f9cc5a5f57fcaa83eb5a8e4  flac.tar.xz' | sha256sum -c - \\
 && echo 'ddfe36cab873794038ae2c1210557ad34857a4b6bdc515785d1da9e175b1da1e  lame.tar.gz' | sha256sum -c - \\
 && tar --no-same-owner -xzf taglib.tar.gz && tar --no-same-owner -xf flac.tar.xz && tar --no-same-owner -xzf lame.tar.gz"""
if text.count(old) != 1:
    raise SystemExit('native Dockerfile source-fetch block changed; review bundled-sources recipe')
dest.write_text(text.replace(old, new, 1))
PY
fi

case "$platform" in
  linux-*)
    for file in libFLAC.so libFLAC.so.12 libmp3lame.so libmp3lame.so.0; do
      [ -L "$payload/lib/$file" ] && [ -f "$payload/lib/$file" ] || {
        echo "missing or broken native SONAME link: $file" >&2; exit 1;
      }
    done
    [ -f "$payload/lib/libpn-tag-bridge.so" ] || exit 1
    flac=libFLAC.so.12; lame=libmp3lame.so.0; bridge=libpn-tag-bridge.so
    ;;
  macos-arm64)
    for file in libFLAC.dylib libFLAC.12.dylib libmp3lame.dylib; do
      [ -L "$payload/lib/$file" ] && [ -f "$payload/lib/$file" ] || {
        echo "missing or broken native dylib link: $file" >&2; exit 1;
      }
    done
    [ -f "$payload/lib/libpn-tag-bridge.dylib" ] && [ -f "$payload/lib/libmp3lame.0.dylib" ] || exit 1
    flac=libFLAC.12.dylib; lame=libmp3lame.0.dylib; bridge=libpn-tag-bridge.dylib
    ;;
esac

# Copy the license texts out of the very same verified public archives.
tar -xOzf "$native/archives/taglib.tar.gz" taglib-e36a9cabb9882e61276161c23834d966d62073b7/COPYING.LGPL > "$payload/licenses/taglib/COPYING.LGPL"
tar -xOzf "$native/archives/taglib.tar.gz" taglib-e36a9cabb9882e61276161c23834d966d62073b7/COPYING.MPL > "$payload/licenses/taglib/COPYING.MPL"
tar -xOf "$native/archives/flac.tar.xz" flac-1.4.2/COPYING.Xiph > "$payload/licenses/flac/COPYING.Xiph"
tar -xOzf "$native/archives/lame.tar.gz" lame-3.100/COPYING > "$payload/licenses/lame/COPYING"
tar -xOzf "$native/archives/lame.tar.gz" lame-3.100/LICENSE > "$payload/licenses/lame/LICENSE"

# Verify the immutable upstream Git blobs, including Apple's full Apache text.
curl -fLsS https://raw.githubusercontent.com/macosforge/alac/c38887c5c5e64a4b31108733bd79ca9b2496d987/LICENSE -o "$payload/licenses/apple-alac/LICENSE"
curl -fLsS https://raw.githubusercontent.com/madsmtm/objc2/d7d2fa23ceaa5e6096c923b081040e5d81b3b9df/LICENSE.md -o "$out/objc2-upstream-LICENSE.md"
python3 - "$payload/licenses/apple-alac/LICENSE" "$out/objc2-upstream-LICENSE.md" <<'PY'
import hashlib, pathlib, sys
for path, expected in zip(sys.argv[1:], ('c9500ba9e08ad58e1dd52e3ab91dbdf5ac4cb296', '540ae4c81adb5f64a9f61e222b688880da24300e')):
    data = pathlib.Path(path).read_bytes()
    actual = hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest()
    if actual != expected:
        raise SystemExit(f'{path}: upstream Git blob identity mismatch')
PY

# Include actual notices and license texts for every locked Cargo package, not
# only those active on this platform. Some objc2 crates omit their workspace
# license from crates.io; their exact-revision upstream statement is supplied.
python3 - "$out/cli/cargo-metadata.json" "$payload/licenses/rust" "$out" "$platform" "$out/objc2-upstream-LICENSE.md" <<'PY'
import json, pathlib, shutil, sys
metadata, dest, out, platform, objc_license = sys.argv[1:]
root = pathlib.Path(out)
packages = json.loads(pathlib.Path(metadata).read_text())['packages']
known = {('objc2', '0.6.5'), ('block2', '0.6.2'), ('objc2-encode', '4.1.0'), ('dispatch2', '0.3.1')}
records = []
for package in sorted(packages, key=lambda p: (p['name'], p['version'])):
    if not package.get('source'):
        continue
    manifest = pathlib.Path(package['manifest_path'])
    if platform.startswith('linux-'):
        if not str(manifest).startswith('/out/'):
            raise SystemExit(f'unexpected container manifest path: {manifest}')
        manifest = root / manifest.relative_to('/out')
    directory = manifest.parent
    target = pathlib.Path(dest) / (package['name'] + '-' + package['version'])
    target.mkdir()
    shutil.copyfile(manifest, target / 'Cargo.toml')
    files = sorted(p for p in directory.iterdir() if p.is_file() and
                   p.name.lower().startswith(('license', 'copying', 'notice')))
    if package.get('license_file'):
        license_file = directory / package['license_file']
        if not license_file.is_file():
            raise SystemExit(f"missing declared license file for {package['name']}")
        if license_file not in files:
            files.append(license_file)
    if not files:
        if (package['name'], package['version']) not in known:
            raise SystemExit(f"no packaged license for {package['name']} {package['version']}")
        shutil.copyfile(objc_license, target / 'UPSTREAM-LICENSE.md')
        # The published crate manifest gives the authors; preserve it and the
        # upstream statement rather than inventing a copyright notice.
        readme = directory / 'README.md'
        if readme.is_file():
            shutil.copyfile(readme, target / 'README.md')
    for file in files:
        shutil.copyfile(file, target / file.name)
    if not package.get('license') and not package.get('license_file'):
        raise SystemExit(f"missing license declaration for {package['name']}")
    records.append({k: package.get(k) for k in ('name', 'version', 'license', 'authors', 'repository', 'source')})
(pathlib.Path(dest) / 'inventory.json').write_text(json.dumps(records, indent=2, ensure_ascii=False) + '\n')
PY
# Full MIT permission terms accompany the objc2 upstream statement and crate
# manifests; no unverified copyright-holder attribution is invented.
cat > "$payload/licenses/rust/MIT-PERMISSION-TERMS.txt" <<'EOF'
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
EOF

cat > "$payload/NOTICE" <<'EOF'
Platinum Engine: project-owned contributions are under the accompanying MIT
license, subject to third-party terms and rights review.

TagLib 1.11.1 (revision e36a9cabb9882e61276161c23834d966d62073b7) is
statically linked into the separately loaded tag bridge; its LGPL 2.1 and MPL
license texts are in licenses/taglib. Its unmodified public source archive and
relinkable bridge object are included in corresponding-source. You may modify
TagLib and reverse engineer the application for debugging your modifications,
to the extent required by the applicable LGPL terms.

FLAC 1.4.2 uses the Xiph/BSD-3-Clause license; see licenses/flac. LAME
3.100 uses the GNU Library GPL version 2 or later; see licenses/lame. Both
public source archives are included. Linux applies the supplied FLAC/LAME
patches and codec math shim; macOS applies only the supplied LAME export-list
correction. Build recipes, configuration receipts, sources, and the Linux shim
archive are included. See corresponding-source/RELINK.txt before modifying
or rebuilding them.

The portable ALAC encoder/decoder in src/portable/audio/alac.rs was independently
written but reference-informed by Apple's macosforge/alac revision
c38887c5c5e64a4b31108733bd79ca9b2496d987; no Apple code was pasted. Apple
reference source Copyright (c) 2011 Apple Inc. All rights reserved. The upstream
reference license is Apache-2.0, included in licenses/apple-alac/LICENSE. The
upstream tree at that revision contains no NOTICE file. Derivative status and any applicable
attribution requirements require maintainer/license review; this notice is not
a clearance conclusion.

The opt-in MP3 decoder (Linux and portable builds) in src/portable/audio/mp3*.rs
is project-owned Layer III code written from ISO/IEC 11172-3 and 13818-3; it
links no decoder library and contains no code from LAME, mpglib, mpg123, libmad,
minimp3 or FFmpeg. Its Huffman and synthesis-window tables are standard data;
corresponding-source/tools/portable/mp3dec/gen_tables.py transcribes those
numbers, not code, from the LAME 3.100 archive included in
corresponding-source/archives (LGPL-2.0-or-later) into
corresponding-source/src/portable/audio/mp3_tables.rs. Whether numeric standard
data taken from that source needs attribution beyond this notice requires
maintainer/license review; this notice is not a clearance conclusion.

Rust dependency license texts, notices, authors and declarations are in
licenses/rust. Rust standard-library notices and referenced license texts are
in licenses/rust-standard-library. The objc2 upstream statement warns that
parts may be derived from Apple SDKs; it is included and requires review.
No claim of comprehensive legal clearance or universal byte parity is made.
EOF

cat > "$payload/corresponding-source/RELINK.txt" <<'EOF'
Corresponding native source and relinking
========================================
The archives in archives/ are the exact checksum-verified public TagLib,
FLAC and LAME sources used by these builds; they are included, not a written
offer. The source tree here contains the bridge, its header and, on Linux,
the exact codec patches, codec-math headers and Rust shim source from the
native build image. receipt/ records the build configuration and tools.
relink/taglib_bridge.o is the corresponding, relinkable bridge object;
relink/libtag.a is the original static TagLib build. Linux also includes
relink/libpn_codec_math.a, linked into the patched codecs. The Rust CLI uses
shared libraries selected at runtime; replacing a codec requires updating its
configured path and SHA-256 pin. Linux additionally requires the library digest
to appear in the architecture-specific allowlist compiled into the CLI.

To rebuild a modified TagLib and relink the bridge, let SRC be the absolute
path of this corresponding-source directory, choose a new, empty WORK directory
and a new absolute PREFIX. The following starts with the bundled archive; make
your desired modifications inside the unpacked TagLib source before CMake:

  cd "$WORK"
  tar -xzf "$SRC/archives/taglib.tar.gz"
  TAGLIB=taglib-e36a9cabb9882e61276161c23834d966d62073b7
  # On macOS arm64 set ARCH_FLAG=-DCMAKE_OSX_ARCHITECTURES=arm64;
  # on Linux set ARCH_FLAG= (the default native architecture is used).
  cmake -S "$TAGLIB" -B taglib-build -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DCMAKE_INSTALL_PREFIX="$PREFIX" \
    -DCMAKE_INSTALL_LIBDIR=lib ${ARCH_FLAG:-} -DBUILD_SHARED_LIBS=OFF \
    -DBUILD_BINDINGS=OFF -DBUILD_TESTS=OFF -DBUILD_EXAMPLES=OFF
  cmake --build taglib-build --parallel 4
  cmake --install taglib-build

Use a static PIC library compatible with the original TagLib 1.11.1
interfaces. Then link on Linux:

  c++ -shared "$SRC/relink/taglib_bridge.o" "$PREFIX/lib/libtag.a" \
    -Wl,--exclude-libs,ALL -Wl,--version-script="$SRC/tools/portable/bridge/bridge.map" \
    -lz -o libpn-tag-bridge.so

On macOS arm64:

  xcrun clang++ -std=c++17 -fobjc-arc -O0 -dynamiclib "$SRC/relink/taglib_bridge.o" "$PREFIX/lib/libtag.a" \
    -framework Foundation -framework AppKit -framework CoreGraphics \
    -framework ImageIO -framework CoreServices -lz -o libpn-tag-bridge.dylib

On macOS, point libraries.tag_bridge to the newly linked trusted library in
your own state. The object and source can also be rebuilt against compatible
modified headers; the original link and build flags are in the native recipes.
On Linux, the distributed CLI refuses a bridge, LAME or libFLAC with a digest
not in its per-architecture allowlist. To use a modified library, obtain the
MIT-licensed public CLI source at https://github.com/mschienbein/platinum-engine,
add its digest for the target architecture and library in src/linux_libraries.rs,
and rebuild the CLI. Set the library path in your state; for LAME and libFLAC
also set the corresponding configured SHA-256. These changes permit a user
build to load the modified library but do not establish output parity.
Rebuilt libraries are not promised to match the distributed libraries.
EOF
if [ "$platform" = macos-arm64 ]; then
  cat >> "$payload/corresponding-source/RELINK.txt" <<'EOF'

To rebuild on macOS with the bundled archives, from this directory run:
  PN_NATIVE_ARCHIVES_DIR="$PWD/archives" sh tools/linux/build-macos-libs.sh /absolute/path/to/a/new/output-directory
The script verifies those archives and uses the recorded settings. The LAME
export-list patch is linux/patches/lame/0001-remove-obsolete-export.patch.
EOF
else
  cat >> "$payload/corresponding-source/RELINK.txt" <<'EOF'

The original pinned Linux recipe is linux/Dockerfile. To rebuild using
the supplied archives instead of re-fetching them, run from this directory:
  docker build --target bridge -f linux/Dockerfile.bundled-sources .

This equivalent recipe replaces only the archive downloads with COPY, keeps
their SHA-256 checks and the original codec source-hash gates, and otherwise
retains the original build steps. The base image and Debian build dependencies
still need to be available, either cached or from their pinned network sources.
Both recipes specify patch order, config.h changes, configure/CMake options
and shim linkage. The exact patched sources are obtained by unpacking archives/
and applying linux/patches/{flac,lame} in Dockerfile order; the math shim is
built from tools/portable/math/shim and src/portable/math.rs. If you modify a
hash-pinned input for your own rebuild, adjust its check to the modified input.
Linux receipt/ includes FLAC and LAME configuration, flags, input hashes and
compiler versions.
EOF
fi
cat >> "$payload/corresponding-source/RELINK.txt" <<'EOF'

For upstream archive SHA-256 values, see the package SHA256SUMS. The archives
also contain upstream copyrights and notices.
EOF

cat > "$payload/README.txt" <<EOF
Platinum Engine $version ($platform)

This package contains the CLI and independently built public native libraries.
Sampled comparisons do not establish universal byte parity, and the included
source and notices do not constitute legal clearance. Review NOTICE and
corresponding-source/RELINK.txt before distribution.

Verify files from this directory using sha256sum -c SHA256SUMS on Linux or
shasum -a 256 -c SHA256SUMS on macOS. SHA256SUMS covers regular files and uses
paths relative to this directory; it does not authenticate its own contents or
symlink targets. Obtain the release from a trusted source.

The CLI does not automatically alter its state or trust bundled code. After
reviewing the libraries, configure their absolute paths and codec file hashes:

  json() { python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "\$1"; }
  hash() { python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "\$1"; }
  pn="\$PWD/bin/platinum"
  flac="\$PWD/lib/$flac"
  lame="\$PWD/lib/$lame"
  "\$pn" config set libraries.tag_bridge "\$(json "\$PWD/lib/$bridge")"
  "\$pn" config set libraries.flac "\$(json "\$flac")"
  "\$pn" config set libraries.flac_sha256 "\$(json "\$(hash "\$flac")")"
  "\$pn" config set libraries.lame "\$(json "\$lame")"
  "\$pn" config set libraries.lame_sha256 "\$(json "\$(hash "\$lame")")"

Use --state-dir before config to isolate state, and a separate output directory
with backups for real media. Replacing a codec requires a new configured pin.
On Linux a changed bridge or codec also requires rebuilding the public CLI with
its digest in src/linux_libraries.rs; see corresponding-source/RELINK.txt.
EOF
archive="$out/platinum-engine-$version-$platform.tar.gz"

# Normalize archive metadata and preserve only safe in-payload symbolic links.
# The source and executable bytes are not claimed to be host-independent.
python3 - "$payload" "$archive" <<'PY'
import gzip, hashlib, os, pathlib, stat, sys, tarfile
root = pathlib.Path(sys.argv[1]).resolve()
archive = pathlib.Path(sys.argv[2])
paths = sorted(root.rglob('*'), key=lambda p: p.relative_to(root).as_posix())
for p in paths:
    if p.is_symlink():
        target = os.readlink(p)
        resolved = (p.parent / target).resolve(strict=True)
        if os.path.isabs(target) or not resolved.is_relative_to(root) or not resolved.is_file():
            raise SystemExit(f'unsafe or broken package symlink: {p.relative_to(root)}')
    elif not (p.is_file() or p.is_dir()):
        raise SystemExit(f'unsupported package entry: {p.relative_to(root)}')
with (root / 'SHA256SUMS').open('x', encoding='utf-8', newline='\n') as sums:
    for p in paths:
        if p.is_file() and not p.is_symlink():
            h = hashlib.sha256()
            with p.open('rb') as f:
                for block in iter(lambda: f.read(1024 * 1024), b''):
                    h.update(block)
            sums.write(f'{h.hexdigest()}  {p.relative_to(root).as_posix()}\n')
paths = sorted(root.rglob('*'), key=lambda p: p.relative_to(root).as_posix())
with archive.open('xb') as raw, gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as gz:
    with tarfile.open(fileobj=gz, mode='w', format=tarfile.PAX_FORMAT) as tf:
        for p in paths:
            info = tf.gettarinfo(str(p), arcname=p.relative_to(root).as_posix())
            info.uid = info.gid = 0
            info.uname = info.gname = ''
            info.mtime = 0
            info.mode = 0o755 if info.isdir() or (info.isfile() and p.stat().st_mode & stat.S_IXUSR) else 0o644
            if info.isfile():
                with p.open('rb') as f:
                    tf.addfile(info, f)
            else:
                tf.addfile(info)
print(archive)
PY
