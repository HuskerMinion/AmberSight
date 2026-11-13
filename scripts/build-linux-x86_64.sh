#!/usr/bin/env sh
# Build the 64-bit Intel/AMD Linux program inside Debian 13 (trixie) with
# Docker: target/trixie/release/ambersight
# A program built on a newer distribution needs that distribution's newer C
# library (glibc) and does not start on older ones; built in trixie it runs
# on Debian 13, Ubuntu 25.04, Fedora 42 and newer (glibc 2.41 or newer).
# scripts/package-linux-deb-rpm.sh packages this one for x86_64.
# ONNX Runtime's CUDA libraries for the NVIDIA add-on land beside it
# (libonnxruntime_providers_shared.so and _cuda.so, as real files: cargo
# leaves links into the container's download cache);
# scripts/package-cuda-addon.sh packs them.
set -eu
cd "$(dirname "$0")/.."
# Made here first, so Docker does not create it owned by root.
mkdir -p target/trixie
docker run --rm \
  -v "$(pwd)":/src -w /src \
  -v ambersight-cargo-registry:/usr/local/cargo/registry \
  -v ambersight-ort-cache:/root/.cache/ort.pyke.io \
  -e CARGO_TARGET_DIR=/src/target/trixie \
  rust:1-trixie sh -eu -c "
    export RUSTFLAGS='--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo=~/.cargo'
    cargo build --release
    cd target/trixie/release
    for f in libonnxruntime_providers_shared.so libonnxruntime_providers_cuda.so; do
      cp -L \$f \$f.real && rm -f \$f && mv \$f.real \$f
    done
    # (TensorRT parts: not used, not shipped.)
    rm -f libonnxruntime_providers_tensorrt.so libonnxruntime_providers_nv_tensorrt_rtx.so
    cd /src
    chown -R $(id -u):$(id -g) target/trixie
  "
echo target/trixie/release/ambersight
