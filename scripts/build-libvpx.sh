#!/usr/bin/env bash
# Build libvpx (static, position-independent, VP8 only) for a Rust
# target into $PREFIX, for vs-record's WebM recordings. Release binaries
# link it statically, so nobody needs libvpx installed to run vs.
#
#   scripts/build-libvpx.sh <rust-target> <prefix>
#
# Then build with VPX_LIB_DIR=<prefix>/lib VPX_INCLUDE_DIR=<prefix>/include
# VPX_VERSION=<version> VPX_STATIC=1.
set -euo pipefail
target="$1"
prefix="$2"
version="${VPX_SRC_VERSION:-1.15.2}"
case "$target" in
  aarch64-apple-darwin) vt=arm64-darwin20-gcc ;;
  x86_64-apple-darwin) vt=x86_64-darwin20-gcc ;;
  x86_64-unknown-linux-gnu) vt=x86_64-linux-gcc ;;
  aarch64-unknown-linux-gnu) vt=arm64-linux-gcc ;;
  *) echo "build-libvpx.sh: no libvpx target for $target" >&2; exit 2 ;;
esac
if [ -f "$prefix/lib/libvpx.a" ]; then
  echo "libvpx $version already in $prefix"
  exit 0
fi
work="$(mktemp -d)"
git clone --quiet --depth 1 --branch "v$version" https://chromium.googlesource.com/webm/libvpx "$work/libvpx"
cd "$work/libvpx"
./configure --target="$vt" --prefix="$prefix" \
  --enable-pic --enable-static --disable-shared \
  --disable-examples --disable-tools --disable-docs --disable-unit-tests \
  --disable-vp9 --enable-vp8
make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
make install
echo "libvpx $version built for $target in $prefix"
