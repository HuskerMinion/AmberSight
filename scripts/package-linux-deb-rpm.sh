#!/usr/bin/env sh
# Build the Linux installer packages with Docker, from programs already built:
#   dist/ambersight_<version>_amd64.deb      dist/ambersight-<version>-1.x86_64.rpm
#   dist/ambersight_<version>_arm64.deb      dist/ambersight-<version>-1.aarch64.rpm
# x86_64:  target/trixie/release/ambersight (scripts/build-linux-x86_64.sh;
#          built in Debian 13 so it runs on systems older than this one),
#          else target/release/ambersight
# aarch64: target/cross/aarch64-unknown-linux-gnu/release/ambersight
#          (scripts/package-linux-arm64.sh)
# X86_64_BIN=<file> / AARCH64_BIN=<file> choose others. A missing program
# skips that kind of computer.
#
# The packages install /usr/bin/ambersight, /etc/ambersight/config.toml
# (data_dir /var/lib/ambersight), the system service "ambersight" running as
# the user "ambersight" (enabled and started on install), and the
# AmberSight menu entry and icon. Removing them keeps the recordings.
set -eu
# Package files and folders: no group or other write.
umask 022
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
work=target/linux-packages
rm -rf "$work"
mkdir -p "$work" dist

x86_bin=${X86_64_BIN:-}
if [ -z "$x86_bin" ]; then
  if [ -f target/trixie/release/ambersight ]; then
    x86_bin=target/trixie/release/ambersight
  else
    x86_bin=target/release/ambersight
    [ -f "$x86_bin" ] && echo "note: packaging $x86_bin; build with scripts/build-linux-x86_64.sh so it also runs on older systems" >&2
  fi
fi
arm_bin=${AARCH64_BIN:-target/cross/aarch64-unknown-linux-gnu/release/ambersight}

# Newest C library version the program needs (from its symbol versions).
glibc_of() {
  grep -ao 'GLIBC_2\.[0-9]*' "$1" | sort -u -t. -k2,2n | tail -1 | sed 's/GLIBC_//'
}

# The installed file tree for one program, in $work/root-<arch>.
stage() {
  root="$work/root-$2"
  install -D -m 755 "$1" "$root/usr/bin/ambersight"
  mkdir -p "$root/etc/ambersight" "$root/var/lib/ambersight" "$root/usr/share/ambersight"
  # The settings file is not a package file: AmberSight rewrites it when
  # settings change in the web page, so a package-owned copy would make
  # every upgrade ask about it. The template is shipped; installing creates
  # /etc/ambersight/config.toml from it only when there is none yet.
  sed 's|^data_dir = .*|data_dir = "/var/lib/ambersight"|' config.example.toml > "$root/usr/share/ambersight/config.toml"
  grep -q '^data_dir = "/var/lib/ambersight"$' "$root/usr/share/ambersight/config.toml"
  install -D -m 644 scripts/linux/ambersight.service "$root/usr/lib/systemd/system/ambersight.service"
  install -D -m 644 scripts/linux/ambersight.sysusers "$root/usr/lib/sysusers.d/ambersight.conf"
  install -D -m 644 scripts/linux/ambersight.desktop "$root/usr/share/applications/ambersight.desktop"
  install -D -m 644 static/icons/icon-512.png "$root/usr/share/icons/hicolor/512x512/apps/ambersight.png"
  install -D -m 644 static/icons/icon.svg "$root/usr/share/icons/hicolor/scalable/apps/ambersight.svg"
  # (The README without its pictures: those files are not installed, and it links nowhere that needs an account.)
  mkdir -p "$root/usr/share/doc/ambersight"
  sed -E '/^<p align="center">$/,/^<\/p>$/d; /^## Screenshots/,/^## Install/{/^## Install/!d;}; /docs\/images/d; /^More screenshots:/d; s,\[([^]]*)\]\(docs/[^)]*\),\1,g' README.md > "$root/usr/share/doc/ambersight/README.md"
  chmod 644 "$root/usr/share/doc/ambersight/README.md"
  install -D -m 644 LICENSE "$root/usr/share/doc/ambersight/copyright"
  install -D -m 644 THIRD-PARTY-NOTICES.txt "$root/usr/share/doc/ambersight/THIRD-PARTY-NOTICES.txt"
}

# $1 program, $2 Debian architecture
deb_control() {
  root="$work/root-$2"
  mkdir -p "$root/DEBIAN"
  cp scripts/linux/deb/postinst scripts/linux/deb/prerm scripts/linux/deb/postrm "$root/DEBIAN/"
  chmod 755 "$root/DEBIAN/postinst" "$root/DEBIAN/prerm" "$root/DEBIAN/postrm"
  size=$(du -sk --exclude=DEBIAN "$root" | cut -f1)
  # The AI runtime needs the C++ library of GCC 13 or newer.
  cat > "$root/DEBIAN/control" <<CTL
Package: ambersight
Version: $version
Architecture: $2
Maintainer: HuskerMinion <28571744+HuskerMinion@users.noreply.github.com>
Homepage: https://github.com/HuskerMinion/AmberSight
Installed-Size: $size
Depends: ffmpeg, passwd, libc6 (>= $(glibc_of "$1")), libstdc++6 (>= 13), libgcc-s1
Section: video
Priority: optional
Description: self-hosted camera recorder (NVR) used from a web browser
 AmberSight records IP cameras (RTSP, ONVIF, HTTP), USB cameras and capture
 cards, detects motion and objects (built-in AI) and is used from a web
 browser on port 8090. It runs as the system service "ambersight"; settings
 are in /etc/ambersight/config.toml, recordings in /var/lib/ambersight.
CTL
}

debs=""
rpms=""
for pair in "x86_64:amd64:$x86_bin" "aarch64:arm64:$arm_bin"; do
  arch=${pair%%:*}; rest=${pair#*:}; debarch=${rest%%:*}; bin=${rest#*:}
  if [ ! -f "$bin" ]; then
    echo "skipping $arch: $bin not found" >&2
    continue
  fi
  stage "$bin" "$arch"
  cp -a "$work/root-$arch" "$work/root-$debarch"
  deb_control "$bin" "$debarch"
  debs="$debs $debarch"
  rpms="$rpms $arch"
done
if [ -z "$debs" ]; then
  echo "no programs to package" >&2
  exit 1
fi

uid=$(id -u); gid=$(id -g)
docker run --rm --platform linux/amd64 -v "$(pwd)":/src -w /src debian:trixie-slim sh -eu -c "
  for a in $debs; do
    dpkg-deb --root-owner-group -Zxz --build $work/root-\$a dist/ambersight_${version}_\$a.deb
  done
  chown $uid:$gid dist/ambersight_${version}_*.deb
"
docker run --rm --platform linux/amd64 -v "$(pwd)":/src -w /src fedora:latest sh -eu -c "
  dnf install -y -q rpm-build >/dev/null 2>&1
  for a in $rpms; do
    rpmbuild -bb --quiet --target \$a-linux \
      --define '_topdir /tmp/rpm' --define 'source_date_epoch_from_changelog 0' --define 'stage /src/$work/root-'\$a \
      --define 'pkgversion $version' scripts/linux/ambersight.spec
    cp /tmp/rpm/RPMS/\$a/ambersight-$version-1.\$a.rpm dist/
  done
  chown $uid:$gid dist/ambersight-$version-1.*.rpm
"
for a in $debs; do echo "dist/ambersight_${version}_$a.deb"; done
for a in $rpms; do echo "dist/ambersight-$version-1.$a.rpm"; done
