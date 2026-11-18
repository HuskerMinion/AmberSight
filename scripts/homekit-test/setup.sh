#!/bin/sh
# Starts what the HomeKit tests need: MediaMTX (Docker, host network) with a
# test camera pushed by ffmpeg (testsrc2 + a sine as AAC), and AmberSight with
# four cameras: "Bars", "Live" (built-in reader) and "Texted" (read by ffmpeg:
# burned-in text) shown in Apple Home, "Hidden" not.
#
#   sh setup.sh <ambersight binary> <standalone|bridged> [auto|software|nvenc|...]
#
# Work folder: $HK_DIR (default /tmp/hk-e2e). Ports: 18290 (web page),
# 18291 (HomeKit; standalone cameras 18292 on), 18253 (RTSP sharing, off),
# 18254-18257 (MediaMTX). The two processes stop by themselves after 15
# minutes; it prints their PIDs ("pub", "as"). Stop everything with
# kill <pids>; docker stop hk-e2e-mtx.
set -eu
d=${HK_DIR:-/tmp/hk-e2e}
mkdir -p "$d/data" "$d/work"
cat > "$d/mtx.yml" <<EOF
logLevel: warn
api: false
metrics: false
pprof: false
playback: false
rtmp: false
hls: false
webrtc: false
srt: false
rtsp: true
rtspTransports: [tcp]
rtspAddress: 127.0.0.1:18254
rtpAddress: :18256
rtcpAddress: :18257
paths:
  all_others:
EOF
docker rm -f hk-e2e-mtx >/dev/null 2>&1 || true
docker run -d --rm --name hk-e2e-mtx --network host --stop-timeout 2 -v "$d/mtx.yml:/mediamtx.yml:ro" bluenviron/mediamtx:1.21.3 >/dev/null
sleep 2
nohup timeout 900 ffmpeg -hide_banner -loglevel error -re -f lavfi -i testsrc2=size=1920x1080:rate=25 -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -preset veryfast -tune zerolatency -g 50 -pix_fmt yuv420p -c:a aac -b:a 64k -f rtsp -rtsp_transport tcp rtsp://127.0.0.1:18254/cam > "$d/pub.log" 2>&1 &
echo "pub $!"
if [ ! -f "$d/apw" ]; then
  head -c 12 /dev/urandom | base64 | tr -dc A-Za-z0-9 > "$d/apw"
  head -c 12 /dev/urandom | base64 | tr -dc A-Za-z0-9 > "$d/vpw"
fi
apw=$(cat "$d/apw"); vpw=$(cat "$d/vpw")
cat > "$d/config.toml" <<EOF
[server]
bind = "0.0.0.0"
port = 18290
rtmp_port = 0
name = "HK Test"

[[server.users]]
username = "hkadmin"
password = "$apw"

[[server.users]]
username = "hkview"
password = "$vpw"
admin = false

[storage]
data_dir = "$d/data"

[transcode]
encoder = "${3:-auto}"

[restream]
port = 18253

[homekit]
enabled = true
port = 18291
name = "AmberSight HK Test"
cameras = "$2"

[[cameras]]
name = "Bars"
url = "lavfi://smptebars=size=640x360:rate=10"
record = false
homekit = true

[[cameras]]
name = "Live"
url = "rtsp://127.0.0.1:18254/cam"
record = false
audio = true
homekit = true

# Read by ffmpeg: burned-in text.
[[cameras]]
name = "Texted"
url = "rtsp://127.0.0.1:18254/cam"
record = false
homekit = true
overlay = true
overlay_text = "{name} {time}"

[[cameras]]
name = "Hidden"
url = "lavfi://smptebars=size=320x180:rate=5"
record = false
EOF
sleep 3
cd "$d"
nohup timeout 900 "$1" run --config "$d/config.toml" > "$d/log" 2>&1 &
echo "as $!"
