#!/bin/sh
# Build the macOS comparison dylib of the portable tag bridge: the library that `--features portable` builds load in the
# library-gated test lanes, and the one the native-vs-portable parity matrices compare with the macOS production bridge
# (which this script never builds or replaces). The Linux bridge is built by linux/Dockerfile (target `bridge`).
#
# Usage: sh tools/portable/bridge/build-macos-portable.sh TAGLIB_INSTALL_DIR NEW_OUTPUT_DIR
#
#   TAGLIB_INSTALL_DIR  holds include/taglib and lib/libtag.a of the pinned public TagLib (e36a9cab, static, PIC)
#   NEW_OUTPUT_DIR      must not exist; receives libpn-tag-bridge-portable.dylib and build.txt
#
# Same hardening as the Linux bridge where the platform has it: -O2, -fstack-protector-strong, -D_FORTIFY_SOURCE=2 (the
# relro/now link flags are ELF only). The install name and the output file name are fixed, so the bytes do not depend on
# where the library was built.
set -eu
[ "$#" -eq 2 ] || { echo 'usage: build-macos-portable.sh TAGLIB_INSTALL_DIR NEW_OUTPUT_DIR' >&2; exit 2; }
[ "$(uname -s)" = Darwin ] || { echo 'macOS is required' >&2; exit 1; }
root=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
taglib=$(CDPATH= cd -- "$1" && pwd)
[ -f "$taglib/lib/libtag.a" ] && [ -d "$taglib/include/taglib" ] || { echo 'TagLib install directory needs lib/libtag.a and include/taglib' >&2; exit 1; }
mkdir "$2"
out=$(CDPATH= cd -- "$2" && pwd)
cd "$out"
xcrun clang++ -std=c++17 -O2 -fstack-protector-strong -D_FORTIFY_SOURCE=2 -fvisibility=hidden -dynamiclib -DTAGLIB_STATIC \
  -Wno-deprecated-declarations -I"$taglib/include/taglib" "$root/native/taglib_bridge_portable.cpp" "$taglib/lib/libtag.a" -lz \
  -install_name @rpath/libpn-tag-bridge-portable.dylib \
  -Wl,-exported_symbols_list,"$root/tools/portable/bridge/bridge.exports" -o libpn-tag-bridge-portable.dylib
{
  printf 'source native/taglib_bridge_portable.cpp %s\n' "$(shasum -a 256 "$root/native/taglib_bridge_portable.cpp" | cut -d ' ' -f 1)"
  printf 'libtag.a %s\n' "$(shasum -a 256 "$taglib/lib/libtag.a" | cut -d ' ' -f 1)"
  printf 'libpn-tag-bridge-portable.dylib %s\n' "$(shasum -a 256 libpn-tag-bridge-portable.dylib | cut -d ' ' -f 1)"
  xcrun clang++ --version
} > build.txt
