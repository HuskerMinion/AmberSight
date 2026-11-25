#!/usr/bin/env sh
# Build the AMD graphics add-on for the AI on 64-bit Linux PCs:
#   dist/ambersight-amd-<version>-linux-x86_64.tar.gz
# holding the helper program AmberSight starts for AMD graphics
# (tools/ai-amd: ONNX Runtime 1.31 with its WebGPU provider, Dawn over
# Vulkan/Mesa) and Dawn's library:
#   ambersight-ai-amd  libwebgpu_dawn.so
# It is a program of its own because that ONNX Runtime cannot also run the
# NVIDIA and Intel add-ons. Built in Debian 13 (trixie) with Docker, the
# same C library baseline as the main program (scripts/build-linux-x86_64.sh),
# so it runs wherever the main program does. Packages leave it out;
# AmberSight downloads this file from the release when it finds AMD
# graphics, checked against the signed SHA256SUMS (src/gpu_addon.rs).
# PREBUILT_AMD=<folder> packs an already built ambersight-ai-amd and
# libwebgpu_dawn.so from there instead of building.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name="ambersight-amd-$version-linux-x86_64"
bin=${PREBUILT_AMD:-tools/ai-amd/target/trixie/release}
if [ -z "${PREBUILT_AMD:-}" ]; then
  mkdir -p tools/ai-amd/target/trixie
  docker run --rm \
    -v "$(pwd)":/src -w /src/tools/ai-amd \
    -v ambersight-cargo-registry:/usr/local/cargo/registry \
    -v ambersight-ort-cache:/root/.cache/ort.pyke.io \
    -e CARGO_TARGET_DIR=/src/tools/ai-amd/target/trixie \
    rust:1-trixie sh -eu -c "
      export RUSTFLAGS='--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo=~/.cargo'
      cargo build --release --locked
      cd target/trixie/release
      cp -L libwebgpu_dawn.so libwebgpu_dawn.so.real
      mv libwebgpu_dawn.so.real libwebgpu_dawn.so
      cd /src/tools/ai-amd
      chown -R $(id -u):$(id -g) target
    "
fi
for f in ambersight-ai-amd libwebgpu_dawn.so; do
  [ -f "$bin/$f" ] || { echo "$bin/$f is missing" >&2; exit 1; }
done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
out="$work/$name"
mkdir -p "$out/licenses"
cp -L "$bin/ambersight-ai-amd" "$out/ambersight-ai-amd"
cp -L "$bin/libwebgpu_dawn.so" "$out/libwebgpu_dawn.so"
chmod 755 "$out/ambersight-ai-amd"
chmod 644 "$out/libwebgpu_dawn.so"
cp tools/ai-amd/licenses/dawn-LICENSE "$out/licenses/dawn-LICENSE"
chmod 644 "$out/licenses/dawn-LICENSE"
[ -f THIRD-PARTY-NOTICES.txt ] && cp THIRD-PARTY-NOTICES.txt "$out/"
cat > "$out/README.txt" <<TXT
AmberSight $version: AMD graphics add-on for the AI (64-bit Linux)

The helper program AmberSight starts to run the AI on AMD graphics
(ONNX Runtime 1.31 with its WebGPU provider) and Dawn's library, for
AmberSight $version (this version only). AmberSight downloads this file by
itself when it finds AMD graphics and keeps the two files in
<data folder>/tools/amd-$version/. To install it by hand (a computer
without internet), put the two files there.

The card also needs a Vulkan driver: Mesa's (mesa-vulkan-drivers on Debian,
Ubuntu and Fedora). See the install guide.

ONNX Runtime: Copyright Microsoft Corporation, MIT License.
Dawn: Copyright The Dawn & Tint Authors, BSD 3-Clause License
(licenses/dawn-LICENSE).
TXT
mkdir -p dist
if tar --version 2>/dev/null | grep -q GNU; then
  tar -C "$work" --owner=0 --group=0 --numeric-owner --mode=u+rwX,go+rX,go-w --sort=name -czf "dist/$name.tar.gz" "$name"
else
  tar -C "$work" --uid 0 --gid 0 --uname root --gname wheel -czf "dist/$name.tar.gz" "$name"
fi
echo "dist/$name.tar.gz"
