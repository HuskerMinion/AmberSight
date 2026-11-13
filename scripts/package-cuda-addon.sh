#!/usr/bin/env sh
# Build the NVIDIA add-on for the AI on 64-bit Linux PCs:
#   dist/ambersight-cuda-<version>-linux-x86_64.tar.gz
# holding ONNX Runtime's two CUDA libraries from the Linux x86_64 build
# (scripts/build-linux-x86_64.sh leaves them in target/trixie/release):
#   libonnxruntime_providers_shared.so  libonnxruntime_providers_cuda.so
# They come from the same ONNX Runtime the program has built in, so the
# add-on belongs to this version only. Packages leave them out (about
# 116 MB); AmberSight downloads this file from the release when it finds an
# NVIDIA card, checked against the signed SHA256SUMS (src/gpu_addon.rs).
# CUDA_LIBS_DIR=<folder> takes them from elsewhere.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
libs=${CUDA_LIBS_DIR:-target/trixie/release}
name="ambersight-cuda-$version-linux-x86_64"
for f in libonnxruntime_providers_shared.so libonnxruntime_providers_cuda.so; do
  # (cargo may leave links to its download cache: those must resolve.)
  if [ ! -f "$libs/$f" ]; then
    echo "$libs/$f is missing: run scripts/build-linux-x86_64.sh first." >&2
    exit 1
  fi
done
rm -rf "dist/$name"
mkdir -p "dist/$name"
for f in libonnxruntime_providers_shared.so libonnxruntime_providers_cuda.so; do
  cp -L "$libs/$f" "dist/$name/$f"
  chmod 644 "dist/$name/$f"
done
[ -f THIRD-PARTY-NOTICES.txt ] && cp THIRD-PARTY-NOTICES.txt "dist/$name/"
cat > "dist/$name/README.txt" <<TXT
AmberSight $version: NVIDIA add-on for the AI (64-bit Linux)

ONNX Runtime's CUDA libraries for AmberSight $version (this version only).
AmberSight downloads this file by itself when it finds an NVIDIA card and
keeps the two libraries in <data folder>/tools/cuda-$version/. To install it
by hand (a computer without internet), unpack the two .so files there.

The card also needs NVIDIA's driver, the CUDA 13 runtime libraries
(cudart, cuBLAS, cuRAND) and cuDNN 9 for CUDA 13: see the install guide.
TXT
if tar --version 2>/dev/null | grep -q GNU; then
  tar -C dist --owner=0 --group=0 --numeric-owner --mode=u+rwX,go+rX,go-w --sort=name -czf "dist/$name.tar.gz" "$name"
else
  tar -C dist --uid 0 --gid 0 --uname root --gname wheel -czf "dist/$name.tar.gz" "$name"
fi
rm -rf "dist/$name"
echo "dist/$name.tar.gz"
