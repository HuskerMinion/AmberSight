#!/usr/bin/env sh
# Build the Raspberry Pi / 64-bit ARM Linux package on an x86_64 Linux
# machine with Docker: dist/ambersight-<version>-linux-aarch64.tar.gz
# The program is cross-compiled inside Debian 13 (trixie): the AI runtime
# needs GCC 13+ libraries, so it runs on
# Raspberry Pi OS (64-bit) trixie and newer, Ubuntu 25.04+ and similar
# (glibc 2.41 or newer). Pi 4 / Pi 5 / Pi 400 / CM4 with a 64-bit OS.
set -eu
cd "$(dirname "$0")/.."
target=aarch64-unknown-linux-gnu
# Made here first, so Docker does not create it owned by root.
mkdir -p target/cross
docker run --rm \
  -v "$(pwd)":/src -w /src \
  -v ambersight-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/src/target/cross \
  rust:1-trixie sh -eu -c "
    apt-get update -qq && apt-get install -y -qq gcc-aarch64-linux-gnu g++-aarch64-linux-gnu >/dev/null
    rustup target add $target >/dev/null
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
    export RUSTFLAGS='--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo=~/.cargo'
    cargo build --release --target $target
    chown -R $(id -u):$(id -g) target/cross
  "
PREBUILT_TARGET=$target PREBUILT_BIN=target/cross/$target/release/ambersight exec scripts/package.sh
