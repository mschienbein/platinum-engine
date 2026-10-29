#!/bin/sh
# Build the pinned libopus 1.6.1 shared library in new directories.
#
# usage: sh build-libopus.sh SOURCE_TARBALL EXPECTED_SHA256 NEW_WORKDIR PREFIX [CC]
#
#   SOURCE_TARBALL   opus-1.6.1.tar.gz (the pinned release tarball)
#   EXPECTED_SHA256  its SHA-256; the build stops on any mismatch
#   NEW_WORKDIR      must not exist; the tarball is extracted into it and the
#                    out-of-tree build and receipts live beneath it
#   PREFIX           must not exist (or be an empty directory); `make install`
#                    target (PREFIX/lib/libopus.{0.dylib,so.0}, PREFIX/include/opus)
#   CC               optional C compiler (default: $CC, else cc)
#
# environment (all optional):
#   OPUS_VARIANT        product (default) | simd | float | conformance
#                         product:     the shipped configuration, see PRODUCT_FLAGS
#                         simd:        product but with the default asm/intrinsics/RTCD
#                         float:       float build (no fixed point), no SIMD/asm/RTCD
#                         conformance: product + extra programs (opus_demo, opus_compare)
#   OPUS_EXTRA_CFLAGS   appended after the product CFLAGS "-O2 -ffp-contract=off -ffile-prefix-map=..." (e.g. -O0, -O3, -flto)
#   OPUS_EXTRA_LDFLAGS  extra linker flags (e.g. -flto)
#   OPUS_EXTRA_CONFIGURE extra ./configure options for experiments (e.g. --disable-rtcd)
#   JOBS                parallel make jobs (default: online CPUs)
#
# No autogen.sh and no network access: only the pinned tarball is used.
# The script never modifies anything outside NEW_WORKDIR and PREFIX.
#
# The library bytes do not depend on NEW_WORKDIR or PREFIX: libopus embeds __FILE__
# (hardening checks), so CFLAGS carries -ffile-prefix-map for the work and source
# trees, and the build is configured with a fixed neutral --prefix (/opt/libopus-1.6.1,
# never created) and installed with DESTDIR into <work>/stage before being copied to
# PREFIX (otherwise the Mach-O install-name header padding would vary with the path).
set -eu

usage() {
  echo 'usage: build-libopus.sh SOURCE_TARBALL EXPECTED_SHA256 NEW_WORKDIR PREFIX [CC]' >&2
  exit 2
}
[ "$#" -ge 4 ] && [ "$#" -le 5 ] || usage
tarball=$1
expected=$2
workdir=$3
prefix=$4
cc=${5:-${CC:-cc}}
variant=${OPUS_VARIANT:-product}
extra_cflags=${OPUS_EXTRA_CFLAGS:-}
extra_ldflags=${OPUS_EXTRA_LDFLAGS:-}
extra_configure=${OPUS_EXTRA_CONFIGURE:-}

[ -f "$tarball" ] || { echo "missing tarball: $tarball" >&2; exit 1; }
case "$expected" in
  *[!0-9a-f]*|'') echo 'EXPECTED_SHA256 must be 64 lowercase hex digits' >&2; exit 2 ;;
esac
[ "${#expected}" -eq 64 ] || { echo 'EXPECTED_SHA256 must be 64 lowercase hex digits' >&2; exit 2; }

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

actual=$(sha256_of "$tarball")
if [ "$actual" != "$expected" ]; then
  echo "SHA-256 mismatch for $tarball" >&2
  echo "  expected $expected" >&2
  echo "  actual   $actual" >&2
  exit 1
fi

# The product configuration: libopus fixed point with the 24-bit internal
# resolution that 1.6 enables by default, no float API, no SIMD, no run-time CPU
# detection, no docs, no extra programs, shared library only.
PRODUCT_FLAGS='--enable-fixed-point --disable-float-api --disable-float-approx --disable-asm --disable-intrinsics --disable-rtcd --disable-doc --disable-extra-programs --enable-shared --disable-static'
case "$variant" in
  product)     flags=$PRODUCT_FLAGS ;;
  simd)        flags='--enable-fixed-point --disable-float-api --disable-float-approx --disable-doc --disable-extra-programs --enable-shared --disable-static' ;;
  float)       flags='--disable-float-approx --disable-asm --disable-intrinsics --disable-rtcd --disable-doc --disable-extra-programs --enable-shared --disable-static' ;;
  conformance) flags=$(printf '%s' "$PRODUCT_FLAGS" | sed 's/--disable-extra-programs/--enable-extra-programs/') ;;
  *) echo "unknown OPUS_VARIANT: $variant" >&2; exit 2 ;;
esac

flags="$flags${extra_configure:+ $extra_configure}"

[ ! -e "$workdir" ] || { echo "NEW_WORKDIR already exists: $workdir" >&2; exit 1; }
if [ -e "$prefix" ]; then
  [ -d "$prefix" ] && [ -z "$(ls -A "$prefix")" ] || { echo "PREFIX exists and is not empty: $prefix" >&2; exit 1; }
fi
mkdir -p "$workdir" "$prefix"
workdir=$(CDPATH= cd -- "$workdir" && pwd)
prefix=$(CDPATH= cd -- "$prefix" && pwd)
receipt=$workdir/receipt
mkdir "$receipt" "$workdir/build"

os=$(uname -s)
jobs=${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}

tar -xzf "$tarball" -C "$workdir"
src=$workdir/opus-1.6.1
[ -f "$src/configure" ] || { echo 'tarball does not contain opus-1.6.1/configure' >&2; exit 1; }
[ "$(sed -n 's/^PACKAGE_VERSION="\(.*\)"/\1/p' "$src/package_version")" = 1.6.1 ] || {
  echo 'unexpected libopus version in tarball' >&2; exit 1; }

# libopus embeds __FILE__ in its hardening checks, so without a prefix map the
# library bytes depend on the build directory. Map the (absolute) source tree to
# a fixed name, and anything else under the work directory to "work".
case "$workdir" in
  *[[:space:]]*) echo 'NEW_WORKDIR must not contain whitespace' >&2; exit 1 ;;
esac
cflags="-O2 -ffp-contract=off -ffile-prefix-map=$workdir=work -ffile-prefix-map=$src=opus-1.6.1${extra_cflags:+ $extra_cflags}"
ldflags=$extra_ldflags
if [ "$os" = Linux ]; then
  # Export only the opus_* API: version script plus --exclude-libs. The script
  # also removes the linker-provided _init/_fini/_edata/__bss_start/_end.
  cat > "$workdir/opus.map" <<'EOF'
OPUS_1 {
  global: opus_*;
  local: *;
};
EOF
  ldflags="${ldflags:+$ldflags }-Wl,--exclude-libs,ALL -Wl,--version-script=$workdir/opus.map"
fi

# Configure with a fixed, neutral prefix and install into a staging directory;
# the result is then copied to PREFIX. Nothing in the library (not even the
# Mach-O install-name header padding) therefore depends on NEW_WORKDIR or PREFIX.
fixed_prefix=/opt/libopus-1.6.1
stage=$workdir/stage
{
  printf 'variant=%s\nos=%s\nmachine=%s\ncc=%s\n' "$variant" "$os" "$(uname -m)" "$cc"
  printf 'configure=%s/configure --prefix=%s %s CC=%s CFLAGS="%s"%s\n' \
    "$src" "$fixed_prefix" "$flags" "$cc" "$cflags" "${ldflags:+ LDFLAGS=\"$ldflags\"}"
  printf 'install=make install DESTDIR=%s, then copy lib/ and include/ to PREFIX\n' "$stage"
} > "$receipt/configure-line.txt"
{ "$cc" --version 2>&1 || true; "$cc" -v 2>&1 || true; } > "$receipt/compiler.txt"

(
  cd "$workdir/build"
  # shellcheck disable=SC2086
  "$src/configure" --prefix="$fixed_prefix" $flags CC="$cc" CFLAGS="$cflags" \
    ${ldflags:+LDFLAGS="$ldflags"} > "$receipt/configure.log" 2>&1
  make V=1 -j "$jobs" > "$receipt/make.log" 2>&1
  make install DESTDIR="$stage" > "$receipt/install.log" 2>&1
)
mkdir -p "$prefix/lib" "$prefix/include"
cp -RP "$stage$fixed_prefix"/lib/libopus* "$prefix/lib/"
rm -f "$prefix"/lib/*.la
cp -R "$stage$fixed_prefix/include/opus" "$prefix/include/"
cp "$workdir/build/config.h" "$workdir/build/config.log" "$receipt/"
grep -E '^(CC|CFLAGS|LDFLAGS|DEFS|CPPFLAGS|LIBM) *=' "$workdir/build/Makefile" > "$receipt/make-vars.txt" || true
(cd "$workdir/build" && find . -name '*.lo' | sed 's|^\./||; s|/\.libs/.*||' | sort -u) > "$receipt/object-dirs.txt" || true
(cd "$workdir/build" && find . -name '*.lo' | sed 's|^\./||' | sort) > "$receipt/objects.txt"

lib=
case "$os" in
  Darwin)
    lib=$prefix/lib/libopus.0.dylib
    # Neutral install name, as for the other bundled dylibs, then ad-hoc sign.
    install_name_tool -id @rpath/libopus.0.dylib "$lib"
    codesign --force --sign - "$lib"
    codesign --verify --strict "$lib"
    { otool -D "$lib"; otool -L "$lib"; codesign -dv "$lib" 2>&1; } > "$receipt/dylib-info.txt"
    nm -gU "$lib" | awk 'NF == 3 && $2 ~ /^[TDSBR]$/ { sub(/^_/, "", $3); print $3 }' | LC_ALL=C sort > "$receipt/exports.txt"
    nm -u "$lib" | awk 'NF == 1 { sub(/^_/, "", $1); print $1 }' | LC_ALL=C sort > "$receipt/imports.txt"
    ;;
  Linux)
    lib=$prefix/lib/libopus.so.0
    [ -e "$lib" ] || { echo "missing $lib after install" >&2; exit 1; }
    { readelf -d "$lib" | grep -E 'SONAME|NEEDED|RPATH|RUNPATH'; } > "$receipt/dynamic.txt" || true
    grep -q 'Library soname: \[libopus.so.0\]' "$receipt/dynamic.txt" || { echo 'SONAME is not libopus.so.0' >&2; exit 1; }
    # Code and data symbols only: the version node itself (OPUS_1) is listed as type A.
    nm -D --defined-only "$lib" | awk 'NF == 3 && $2 ~ /^[TDBRVWGS]$/ { print $3 }' | sed 's/@.*//' | LC_ALL=C sort -u > "$receipt/exports.txt"
    nm -D --undefined-only "$lib" | awk '{ print $NF }' | sed 's/@.*//' | LC_ALL=C sort -u > "$receipt/imports.txt"
    ;;
  *) echo "unsupported OS: $os" >&2; exit 1 ;;
esac
sha256_of "$lib" > "$receipt/library.sha256"

# Public API: every OPUS_EXPORT function declared in the installed headers.
cat "$prefix"/include/opus/*.h | grep 'OPUS_EXPORT' | grep -v '#' \
  | sed -n 's/.*[ *]\(opus_[a-z0-9_]*\) *(.*/\1/p' | LC_ALL=C sort -u > "$receipt/declared.txt"
LC_ALL=C comm -13 "$receipt/declared.txt" "$receipt/exports.txt" > "$receipt/exported-not-declared.txt"
LC_ALL=C comm -23 "$receipt/declared.txt" "$receipt/exports.txt" > "$receipt/declared-not-exported.txt"
if [ -s "$receipt/exported-not-declared.txt" ]; then
  echo 'ERROR: library exports symbols that are not public opus_* API:' >&2
  cat "$receipt/exported-not-declared.txt" >&2
  exit 1
fi
for sym in opus_encoder_create opus_encoder_destroy opus_encoder_ctl opus_decoder_create opus_decoder_destroy \
           opus_decoder_ctl opus_get_version_string opus_strerror opus_packet_get_nb_samples \
           opus_packet_get_nb_frames opus_packet_get_samples_per_frame opus_packet_get_bandwidth \
           opus_packet_get_nb_channels opus_decoder_get_nb_samples; do
  grep -qx "$sym" "$receipt/exports.txt" || { echo "missing required export: $sym" >&2; exit 1; }
done
case "$variant" in
  product|simd|conformance)
    for sym in opus_encode24 opus_decode24; do
      grep -qx "$sym" "$receipt/exports.txt" || { echo "missing required export: $sym" >&2; exit 1; }
    done
    for sym in opus_encode_float opus_decode_float; do
      ! grep -qx "$sym" "$receipt/exports.txt" || { echo "unexpected float API export: $sym" >&2; exit 1; }
    done ;;
  float)
    for sym in opus_encode_float opus_decode_float; do
      grep -qx "$sym" "$receipt/exports.txt" || { echo "missing required export: $sym" >&2; exit 1; }
    done ;;
esac

if [ "$variant" = conformance ]; then
  for tool in opus_demo opus_compare; do
    [ -x "$workdir/build/$tool" ] || { echo "missing $tool" >&2; exit 1; }
  done
fi

echo "variant:  $variant"
echo "library:  $lib"
echo "sha256:   $(cat "$receipt/library.sha256")"
echo "receipt:  $receipt"
echo "exports:  $(wc -l < "$receipt/exports.txt" | tr -d ' ') opus_* symbols; $(wc -l < "$receipt/declared-not-exported.txt" | tr -d ' ') declared but not compiled in (see declared-not-exported.txt)"
