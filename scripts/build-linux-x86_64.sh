#!/usr/bin/env sh
# Build the 64-bit Intel/AMD Linux program inside Debian 13 (trixie) with
# Docker: target/trixie/release/ambersight
# A program built on a newer distribution needs that distribution's newer C
# library (glibc) and does not start on older ones; built in trixie it runs
# on Debian 13, Ubuntu 25.04, Fedora 42 and newer (glibc 2.41 or newer).
# scripts/package-linux-deb-rpm.sh packages this one for x86_64.
set -eu
cd "$(dirname "$0")/.."
# Made here first, so Docker does not create it owned by root.
mkdir -p target/trixie
docker run --rm \
  -v "$(pwd)":/src -w /src \
  -v ambersight-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/src/target/trixie \
  rust:1-trixie sh -eu -c "
    export RUSTFLAGS='--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo=~/.cargo'
    cargo build --release
    chown -R $(id -u):$(id -g) target/trixie
  "
echo target/trixie/release/ambersight
