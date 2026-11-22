# HomeKit end-to-end tests

Tests AmberSight's Apple Home (HomeKit) support with an open-source HomeKit
controller (aiohomekit, as Home Assistant uses) instead of an iPhone, and an
ffmpeg SRTP receiver for live video. For a real iPhone round see
`docs/homekit-test-checklist.md`.

Needs a Linux computer on a network with multicast (mDNS), Docker (for the
MediaMTX test camera), ffmpeg with libx264 and libopus, ffprobe, and Python
3.12 with aiohomekit and HAP-python (for its TLV8 encoder):

```sh
uv venv --python 3.12 ~/hk-test
VIRTUAL_ENV=~/hk-test uv pip install aiohomekit HAP-python
```

The tests announce test accessories on your network for a few minutes (named
"AmberSight HK Test ..."). They need nothing from the network beyond mDNS.

Apple devices (and these scripts) can pair only while the pairing window is
open: Settings, Apple Home, **Pair a device**, or
`POST /api/homekit/pairing {"action": "open"}` (`"camera": "<name>"` in
standalone mode). The scripts open it themselves before they pair; it stays
open 5 minutes and closes when a pairing succeeds.

## Run

```sh
cargo build --release
dir=/tmp/hk-e2e
apw() { cat $dir/apw; }; vpw() { cat $dir/vpw; }

# Bridged: pairing, accessories, characteristics, events, snapshots, limits,
# then live video.
sh scripts/homekit-test/setup.sh target/release/ambersight bridged
sleep 15
~/hk-test/bin/python -I scripts/homekit-test/hk_e2e.py http://127.0.0.1:18290 hkadmin "$(apw)" hkview "$(vpw)" 18291
curl -s -u "hkadmin:$(apw)" -X POST -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:18290/api/homekit/reset >/dev/null
~/hk-test/bin/python -I scripts/homekit-test/hk_stream.py http://127.0.0.1:18290 hkadmin "$(apw)" bridged 18291 $dir/work
kill <the two PIDs setup.sh printed>; docker stop hk-e2e-mtx

# Standalone (each camera its own accessory): live video.
rm -rf $dir/data
sh scripts/homekit-test/setup.sh target/release/ambersight standalone
sleep 15
~/hk-test/bin/python -I scripts/homekit-test/hk_stream.py http://127.0.0.1:18290 hkadmin "$(apw)" standalone 18291 $dir/work
kill <PIDs>; docker stop hk-e2e-mtx; rm -rf $dir
```

AmberSight on another machine (Windows, a Mac, Docker): run `setup.sh`'s
config there by hand, then point the scripts at it. `HK_HOST=<its address>`
for `hk_e2e.py`'s raw checks; for `hk_stream.py`, `HK_RECV=<this machine's
address>` (where the receiver listens; the Apple device's address in a real
setup) and `HK_RTP_BASE=<first of 5 free UDP ports>` (default 18270).

Helpers: `browse.py [seconds]` lists the `_hap._tcp` records it sees;
`resetwatch.py` measures how long after Reset (or HomeKit off and on) the new
record shows up; `direct.py` pairs by address without mDNS (Docker's bridge
network), takes a snapshot and unpairs.

macOS: the system Python is 3.9; get 3.12 with uv (`uv venv --python 3.12`).
On Intel Macs, aiohomekit 4.0.1 needs `cryptography==48.0.1` (newer wheels
are not built for Intel): `uv pip install aiohomekit==4.0.1
cryptography==48.0.1 HAP-python`.

Cameras read by ffmpeg: `HK_CAMERA=Texted HK_CHECK=text` streams the camera
with burned-in text, checks that ffmpeg draws it, and that changing the text
while it streams ends the stream.

To test live video over an IPv6 link-local address, give one of this
computer's (see `ip -6 addr show scope link`) with its interface index:
`HK_LINKLOCAL='fe80::...%2' python ... hk_stream.py ...`.

A third argument to `setup.sh` picks the video encoder (`software` for
libx264, `nvenc`, `vaapi`, ...; default `auto`).

Each script prints PASS/FAIL lines and a summary, and exits 1 when anything
failed. `hk_stream.py` checks: the accessory is found over mDNS and pairs;
SetupEndpoints to another address than the controller's is refused; a start
at 1280x720, 30 fps, 1000 kbps gives decodable H.264 of that size and Opus
sound at the receiver; the receiver's SRTCP keeps it running past 30 s; end
stops it; with no RTCP coming back under the
session's key (plain RTCP, and one SRTCP packet sent again and again, do not
count) the stream stops by itself after about 30 s; closing the HomeKit connection stops its
stream. It prints the sender ffmpeg's CPU use. The receiver's address is the
one the controller reached AmberSight from (found over mDNS).
