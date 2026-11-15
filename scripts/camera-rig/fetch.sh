#!/bin/sh
# Downloads for the camera rig, each into its own folder under ~/camrig.
# Nothing is installed system-wide and no install script is run.
#
#   fetch.sh            MediaMTX image, onvif-go, helper image
#   fetch.sh release    also the latest AmberSight release (Linux x86_64)
set -eu
CAMRIG="$HOME/camrig"
HERE="$(cd "$(dirname "$0")" && pwd)"

# MediaMTX, pinned by digest (MIT, github.com/bluenviron/mediamtx).
docker pull -q bluenviron/mediamtx:1.21.3@sha256:59e43552f3be43695b70c19424dc2a42fcec18075f88d91561a24ac5d70a586c

# onvif-go v1.3.0 (MIT, github.com/0x524a/onvif-go): the release tarball,
# checked against the release's checksums.txt.
OG="$CAMRIG/dl/onvif-go-v1.3.0"
if [ ! -x "$OG/onvif-server" ]; then
  mkdir -p "$OG"
  cd "$OG"
  curl -fsSLO https://github.com/0x524a/onvif-go/releases/download/v1.3.0/onvif-go-v1.3.0-linux-amd64.tar.gz
  curl -fsSLO https://github.com/0x524a/onvif-go/releases/download/v1.3.0/checksums.txt
  grep ' onvif-go-v1.3.0-linux-amd64.tar.gz$' checksums.txt | sha256sum -c -
  tar xzf onvif-go-v1.3.0-linux-amd64.tar.gz ./onvif-server ./LICENSE
fi

# Helper image: GStreamer RTSP server + Python, Ubuntu packages only.
docker image inspect camrig-helper:1 >/dev/null 2>&1 || docker build -q -t camrig-helper:1 "$HERE/image"

if [ "${1:-}" = "release" ]; then
  # The latest AmberSight release, checked against its SHA256SUMS.
  TAG=$(curl -fsSL https://api.github.com/repos/HuskerMinion/AmberSight/releases/latest | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')
  V=${TAG#v}
  D="$CAMRIG/bin/ambersight-$V"
  if [ ! -x "$D/ambersight" ]; then
    mkdir -p "$D/dl"
    cd "$D/dl"
    curl -fsSLO "https://github.com/HuskerMinion/AmberSight/releases/download/$TAG/ambersight-$V-linux-x86_64.tar.gz"
    curl -fsSLO "https://github.com/HuskerMinion/AmberSight/releases/download/$TAG/SHA256SUMS"
    grep " ambersight-$V-linux-x86_64.tar.gz\$" SHA256SUMS | sha256sum -c -
    tar xzf "ambersight-$V-linux-x86_64.tar.gz"
    BIN=$(find . -type f -name ambersight -perm -u+x | head -1)
    cp "$BIN" "$D/ambersight"
  fi
  echo "release binary: $D/ambersight"
fi
