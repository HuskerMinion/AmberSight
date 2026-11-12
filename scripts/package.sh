#!/usr/bin/env sh
# Build a release archive for this machine (Linux or macOS):
#   dist/ambersight-<version>-<os>-<arch>.tar.gz
# containing the program, an example config and the README.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
# PREBUILT_TARGET=<rust target>: package a program already built for another
# machine (scripts/package-linux-arm64.sh), e.g. aarch64-unknown-linux-gnu.
if [ -n "${PREBUILT_TARGET:-}" ]; then
  arch=${PREBUILT_TARGET%%-*}
  bin="${PREBUILT_BIN:-target/$PREBUILT_TARGET/release/ambersight}"
elif [ "$os" = linux ] && [ "$arch" = x86_64 ]; then
  # The Debian 13 build (scripts/build-linux-x86_64.sh), never this machine's
  # own: a program built here needs this system's newer C library and would
  # not start on the systems the README promises.
  bin="${PREBUILT_BIN:-target/trixie/release/ambersight}"
  if [ ! -x "$bin" ]; then
    echo "$bin is missing: run scripts/build-linux-x86_64.sh first." >&2
    exit 1
  fi
else
  # Keep build-machine paths (user names) out of the program.
  RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~ --remap-path-prefix=$(pwd)=." cargo build --release
  bin="target/release/ambersight"
fi
# (Apple calls it arm64: the name the install guide uses.)
[ "$os" = darwin ] && [ "$arch" = aarch64 ] && arch=arm64
name="ambersight-$version-$os-$arch"
rm -rf "dist/$name"
mkdir -p "dist/$name"
cp "$bin" "dist/$name/"
# (The README without its pictures: those files are not in the package, and it links nowhere that needs an account.)
cp config.example.toml "dist/$name/"
sed -E '/^<p align="center">$/,/^<\/p>$/d; /^## Screenshots/,/^## Install/{/^## Install/!d;}; /docs\/images/d; /^More screenshots:/d; s,\[([^]]*)\]\(docs/[^)]*\),\1,g' README.md > "dist/$name/README.md"
[ -f LICENSE ] && cp LICENSE "dist/$name/"
[ -f THIRD-PARTY-NOTICES.txt ] && cp THIRD-PARTY-NOTICES.txt "dist/$name/"
cat > "dist/$name/INSTALL.txt" <<TXT
AmberSight $version

1. ffmpeg: on Linux install it (apt install ffmpeg; on Fedora add RPM Fusion
   first, then dnf install ffmpeg --allowerasing). On a Mac nothing to do:
   AmberSight downloads it by itself on first start (one time, about 30 MB).
2. Install it:        ./ambersight setup            (for you; starts when you sign in)
                  or: sudo ./ambersight setup --system   (whole computer; starts at boot)
   Setup copies the program into place, writes the settings, starts the
   background service and opens http://localhost:8090. Add cameras there
   (Settings > Cameras > Find cameras).
3. Add a user account (Settings > Users & security) and turn on HTTPS
   before using it on a shared network.
4. Optional AI:       Settings > Detection & alerts > Objects (AI) in the web page.
5. Remove it again:   ./ambersight setup --uninstall  (settings and recordings stay)
TXT
# (Files owned by root, no build-machine user names in the archive.)
if tar --version 2>/dev/null | grep -q GNU; then
  tar -C dist --owner=0 --group=0 --numeric-owner --mode=u+rwX,go+rX,go-w --sort=name -czf "dist/$name.tar.gz" "$name"
else
  tar -C dist --uid 0 --gid 0 --uname root --gname wheel -czf "dist/$name.tar.gz" "$name"
fi
echo "dist/$name.tar.gz"
