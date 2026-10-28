# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
# The manifest digest includes both linux/arm64 and linux/amd64.
FROM rust:1.99.1-slim-bookworm@sha256:24707e70a4f1e23d9a7b25202ec43126d28a52294b96aad5c670df1fd1f1d400 AS deps

ARG DEBIAN_SNAPSHOT=20261025T000000Z
RUN printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/%s/ bookworm main\ndeb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/%s/ bookworm-security main\n' "$DEBIAN_SNAPSHOT" "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list \
 && rm -f /etc/apt/sources.list.d/debian.sources \
 && apt-get -o Acquire::Check-Valid-Until=false update \
 && apt-get install -y --no-install-recommends build-essential clang cmake ca-certificates curl pkg-config python3 xz-utils zlib1g-dev \
 && rm -rf /var/lib/apt/lists/*

ARG CC=clang
ARG CXX=clang++
ARG FLAC_FLAGS=-O3\ -DNDEBUG
ARG FLAC_ASM=ON
ARG LAME_FLAGS=-O2\ -ffp-contract=off
# On x86, apple-profile disables config-selected SSE, fast-log and the
# IEEE754 takehiro optimization to investigate the arm64 Apple build. It is
# an experimental variant, not a promise of equivalence.
ARG LAME_PROFILE=apple-profile
ENV CC=${CC} CXX=${CXX} LC_ALL=C
WORKDIR /build
RUN curl -fL https://github.com/taglib/taglib/archive/e36a9cabb9882e61276161c23834d966d62073b7.tar.gz -o taglib.tar.gz \
 && echo '5c80b1e5273849122ef11b5a9116e4b7f182b899cfdb28aefb704087216e1360  taglib.tar.gz' | sha256sum -c - \
 && curl -fL https://github.com/xiph/flac/releases/download/1.4.2/flac-1.4.2.tar.xz -o flac.tar.xz \
 && echo 'e322d58a1f48d23d9dd38f432672865f6f79e73a6f9cc5a5f57fcaa83eb5a8e4  flac.tar.xz' | sha256sum -c - \
 && curl -fL https://ftp.debian.org/debian/pool/main/l/lame/lame_3.100.orig.tar.gz -o lame.tar.gz \
 && echo 'ddfe36cab873794038ae2c1210557ad34857a4b6bdc515785d1da9e175b1da1e  lame.tar.gz' | sha256sum -c - \
 && tar --no-same-owner -xzf taglib.tar.gz && tar --no-same-owner -xf flac.tar.xz && tar --no-same-owner -xzf lame.tar.gz
COPY linux/patches/ /build/patches/
COPY src/portable/math.rs /build/src/portable/math.rs
COPY tools/portable/math/shim/ /build/tools/portable/math/shim/
# This snapshot is the cross-architecture-verified codec math contract.
# Repin deliberately only after rerunning the raw-bit and codec corpora.
RUN sha256sum -c /build/patches/codec-math.sha256
RUN mkdir -p /opt/platinum-native/lib \
 && CARGO_TARGET_DIR=/build/codec-math-target RUSTFLAGS="-C relocation-model=pic" cargo build --offline --locked --release \
      --manifest-path /build/tools/portable/math/shim/Cargo.toml \
 && cp /build/codec-math-target/release/libplatinum_codec_math.a /opt/platinum-native/lib/libpn_codec_math.a \
 && patch -d flac-1.4.2 -p1 --forward < /build/patches/flac/0001-apple-arm64-fp-order.patch \
 && patch -d flac-1.4.2 -p1 --forward < /build/patches/flac/0002-apple-fixed-predictor.patch \
 && patch -d lame-3.100 -p1 --forward < /build/patches/lame/0001-remove-obsolete-export.patch \
 && patch -d lame-3.100 -p1 --forward < /build/patches/lame/0002-apple-builtin-calls.patch \
 && patch -d lame-3.100 -p1 --forward < /build/patches/lame/0003-apple-intrinsic-order.patch
RUN cmake -S taglib-e36a9cabb9882e61276161c23834d966d62073b7 -B taglib-build \
      -DCMAKE_BUILD_TYPE=Release -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
      -DCMAKE_INSTALL_PREFIX=/opt/platinum-native -DCMAKE_INSTALL_LIBDIR=lib \
      -DBUILD_SHARED_LIBS=OFF -DBUILD_BINDINGS=OFF -DBUILD_TESTS=OFF -DBUILD_EXAMPLES=OFF \
 && cmake --build taglib-build -j4 && cmake --install taglib-build
RUN cmake -S flac-1.4.2 -B flac-build -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_C_FLAGS_RELEASE="${FLAC_FLAGS} -ffp-contract=off -I/build/tools/portable/math/shim -include /build/patches/flac/codec-math.h" \
      -DHAVE_ASSOC_MATH=OFF -DPN_CODEC_MATH_LIBRARY=/opt/platinum-native/lib/libpn_codec_math.a \
      -DCMAKE_INSTALL_PREFIX=/opt/platinum-native \
      -DCMAKE_INSTALL_LIBDIR=lib -DBUILD_SHARED_LIBS=ON -DWITH_OGG=OFF -DWITH_ASM=${FLAC_ASM} \
      -DBUILD_CXXLIBS=OFF -DBUILD_PROGRAMS=OFF -DBUILD_TESTING=OFF \
      -DBUILD_EXAMPLES=OFF -DBUILD_DOCS=OFF -DBUILD_UTILS=OFF \
 && cmake --build flac-build -j4 && cmake --install flac-build
RUN echo '0f4570fddb684f87b04ae6e36a7919d9c6928423d2a761d53a90cd21edb76ecd  lame-3.100/include/libmp3lame.sym' | sha256sum -c - \
 && cd lame-3.100 \
 && ./configure --prefix=/opt/platinum-native --enable-shared --disable-static \
      --disable-frontend --disable-analyzer-hooks --disable-dependency-tracking \
      CFLAGS="${LAME_FLAGS} -ffp-contract=off -I/build/tools/portable/math/shim -include /build/patches/lame/codec-math.h" \
      LDFLAGS="-Wl,--exclude-libs,ALL" LIBS="/opt/platinum-native/lib/libpn_codec_math.a -lpthread -ldl -lm" \
 && if [ "$LAME_PROFILE" = apple-profile ]; then \
      python3 -c 'from pathlib import Path; p=Path("config.h"); b=p.read_text(); names=("HAVE_XMMINTRIN_H","TAKEHIRO_IEEE754_HACK","USE_FAST_LOG"); [(exec("global b; b=b.replace(\"#define "+n+" 1\", \"/* #undef "+n+" */\")")) for n in names]; p.write_text(b)'; \
    elif [ "$LAME_PROFILE" != system ]; then echo 'unknown LAME_PROFILE' >&2; exit 1; fi \
 && make -j4 && make install
RUN mkdir -p /opt/platinum-native/receipt \
 && cp /build/flac-build/CMakeCache.txt /build/flac-build/config.h /opt/platinum-native/receipt/ \
 && cp /build/lame-3.100/config.log /opt/platinum-native/receipt/lame-config.log \
 && cp /build/lame-3.100/config.h /opt/platinum-native/receipt/lame-config.h \
 && sha256sum /build/patches/codec-math.sha256 /build/patches/flac/0001-apple-arm64-fp-order.patch /build/patches/flac/0002-apple-fixed-predictor.patch /build/patches/flac/codec-math.h /build/src/portable/math.rs /build/tools/portable/math/shim/Cargo.toml /build/tools/portable/math/shim/Cargo.lock /build/tools/portable/math/shim/src/lib.rs /build/tools/portable/math/shim/pn_codec_math.h /build/patches/lame/0001-remove-obsolete-export.patch /build/patches/lame/0002-apple-builtin-calls.patch /build/patches/lame/0003-apple-intrinsic-order.patch /build/patches/lame/codec-math.h > /opt/platinum-native/receipt/codec-input-sha256.txt \
 && { printf 'CC=%s\nCXX=%s\nFLAC_FLAGS=%s\nFLAC_ASM=%s\nLAME_FLAGS=%s\nLAME_PROFILE=%s\n' "$CC" "$CXX" "$FLAC_FLAGS" "$FLAC_ASM" "$LAME_FLAGS" "$LAME_PROFILE"; "$CC" --version; cmake --version; rustc --version; } > /opt/platinum-native/receipt/build.txt
ENV LD_LIBRARY_PATH=/opt/platinum-native/lib

# Build this target only after a real portable ABI-5 source exists. Never fall
# back to the macOS Objective-C++ bridge or a stub.
FROM deps AS bridge
COPY native/taglib_bridge_portable.cpp native/taglib_bridge.h /build/native/
COPY tools/portable/bridge/bridge.map /build/native/bridge.map
RUN "$CXX" -std=c++17 -O2 -fPIC -fvisibility=hidden -shared -DTAGLIB_STATIC \
      -I/opt/platinum-native/include/taglib /build/native/taglib_bridge_portable.cpp \
      /opt/platinum-native/lib/libtag.a -Wl,--exclude-libs,ALL -Wl,--version-script=/build/native/bridge.map -lz -o /opt/platinum-native/lib/libpn-tag-bridge.so
