# Camera rig

An automated camera-compatibility test, run before each release. It
simulates the protocol-level variety of real cameras (codecs, sizes, audio
formats, logins, odd RTSP servers, ONVIF, faults), runs a temporary
AmberSight with one camera for each case, and checks what AmberSight does
with each one. It does not simulate firmware quirks it has no example of.

It runs on a Linux machine with Docker, ffmpeg/ffprobe and Python 3.11+
(no root needed). Nothing is installed system-wide.

## Running it

```sh
# once (and again after changing image/Dockerfile): downloads and the helper image
scripts/camera-rig/fetch.sh            # add "release" to also fetch the latest release binary

# a run: about 4 minutes, at most 15
python3 scripts/camera-rig/rig.py --binary target/release/ambersight
python3 scripts/camera-rig/rig.py      # the latest release fetched by fetch.sh release
```

Options: `--wait N` (seconds the cameras run before the checks, default 120),
`--only REGEX` (only some cameras), `--keep` (keep the temporary folder),
`--cleanup` (stop what a crashed run left behind). `CAMRIG_RUST_LOG` sets
AmberSight's log filter (default `info`); `CAMRIG_EXTRA_TOML` is added to the end of
AmberSight's config (e.g. a `[transcode]` table).

The run prints a PASS/FAIL table and writes `~/camrig/results/<time>/`:
`summary.txt` (the table), `results.json`, AmberSight's log and warnings, the
server and stub logs. The exit code is 1 when any check failed.

Everything it starts is named or recorded: Docker containers are named
`camrig-*`, processes are recorded in `~/camrig/run/state.json` and stopped
by process group. AmberSight's config and recordings go to `/tmp/camrig-*`,
which is deleted at the end (recordings are capped at 3 GB while it runs).

### Ports

All in the 18000 range, on the host network:

| Port | What |
|---|---|
| 18190 | AmberSight under test (127.0.0.1) |
| 18554 / 18555 / 18556 | MediaMTX: Basic auth / Digest auth / UDP only |
| 18600-18605 | MediaMTX RTP/RTCP over UDP |
| 18997-18999 | MediaMTX APIs (127.0.0.1) |
| 18564 / 18565 | GStreamer RTSP server: Basic / Digest |
| 18570 | the rig's RTSP stub |
| 18001-18003 | virtual ONVIF cameras (the rig's simulator) |
| 18010 | onvif-go virtual camera |
| 18096, 18099 | nothing listens: a camera that is off; the ONVIF port given to non-ONVIF cameras so AmberSight does not probe other ports on the machine |
| 3702/udp | WS-Discovery (shared, multicast) |

The run refuses to start when any of these TCP/UDP ports is in use.

## What is simulated

`profiles.toml` holds the list. Each `[[profile]]` is fed by ffmpeg (lavfi
`testsrc2` and a beeping sine) into MediaMTX as `<name>/main` and
`<name>/sub`, and becomes one camera. Each `[[case]]` is one more camera on
any server.

- **Profiles:** H.264 and H.265, 360p sub to 4K main, 10 to 25 fps, GOP of 2 to
  10 s, B-frames, H.265 sub stream, no sub stream, H.265 with CRA keyframes
  (open GOP). Audio: none, AAC (16 and 48 kHz), G.711 mu-law (8 and 16 kHz)
  and A-law.
- **Logins and transport:** Basic and Digest; the login in the URL or in
  AmberSight's fields; special characters in passwords (MediaMTX allows `@ # &
  $` only, so `: /` and space are tested against GStreamer); a UDP-only server;
  Dahua-style `cam/realmonitor?channel=1&subtype=0`; `stream0?username=&password=`.
- **Second RTSP implementation:** GStreamer's gst-rtsp-server (H.264 + AAC,
  H.265, H.264 + G.711, Basic and Digest).
- **RTSP stub** (`rtsp_stub.py`, for what the servers cannot make): Dahua
  Content-Base with the query kept, query-string login, G.711 whose RTP time
  steps 1.875 times its samples, AAC frames with their ADTS header and an empty
  frame before each (as a Kuna camera sends), RTP time jumping forward one hour
  or back 30 s, a stream that drops after 20 s, a camera that answers DESCRIBE
  but sends nothing, 404, and a Digest login that counts every try.
- **ONVIF:** three virtual cameras (`onvif_sim.py`): WS-Discovery, Device,
  Media (main and sub profiles pointing at MediaMTX), pull-point Events
  (motion on for 5 s every 15 s), PTZ (moves, presets, nodes). Plus onvif-go's
  virtual camera (Device, Media, PTZ) as a second implementation.

## What is checked

For every camera that should play:

- online, with frames, and read by the expected pipeline (`ingest` in
  `GET /api/cameras`: `native` unless the case expects ffmpeg);
- the last three complete recording segments: video codec, length, decodes
  with no errors, and with audio, video and audio end within 0.1 s of each
  other (last packet pts + duration, by ffprobe);
- live view: a snapshot, and 3 s of `/api/live/:id/stream` that ffprobe can read
  (with audio when expected);
- with a sub stream: the live sub stream runs and, on MediaMTX, the sub path
  has a reader (motion is decoded from the sub stream).

For failure cases: the camera stays offline, `last_error` is plain words
(no password, no debug lines) and matches the case's wording, and the camera
is not moved to ffmpeg. Fault cases also check reconnects after a drop, sane
recording times after a timestamp jump, and for a wrong password: few tries
(AmberSight waits 1 to 10 minutes between them), then the password is fixed
through `PUT /api/cameras/:id` and the camera must play and record. ONVIF:
`ambersight discover` and `GET /api/discover` find all three cameras,
`POST /api/discover/inspect` gives the right main and sub URLs (also for
onvif-go), PTZ caps, moves and presets reach the camera, and camera motion
(trigger = "camera") makes events. Finally: no panics, AmberSight still runs.

## Limits

- **Firmware quirks are only simulated where we know them** (Kuna ADTS, the
  1.875x G.711 clock, Dahua query paths). Unknown camera bugs will not show up
  here; a user's camera report is the way to add them.
- **One machine:** every camera is on 127.0.0.1 or this machine's address, so
  per-host behavior (several cameras behind one IP, MAC look-ups, ARP) and
  real network loss or jitter are not tested.
- **WS-Discovery** needs host networking. The ONVIF simulator answers only
  probes sent from this machine, so other computers on the network never see
  the virtual cameras; discovery across subnets is not tested.
- **MediaMTX logins** cannot contain `:`, `/` or spaces; those are tested
  against GStreamer only.
- **UDP:** the UDP-only server case checks `transport = "auto"` (TCP, then UDP
  after the server's 461) on the built-in reader, on one machine with no
  packet loss; UDP over a real network (loss, reordering, NAT) is not tested.
- **Battery cameras, Reolink Baichuan, HTTP-FLV, RTMP push, two-way audio and
  hardware decoding** are not covered.
- The G.711 1.875x clock and ADTS cases are the rig's own re-creations of what
  those cameras send, not captures.

## Licenses and maintenance (checked 2026-11-15)

The rig is a test tool and is not shipped with AmberSight.

| Tool | License | Status | Used |
|---|---|---|---|
| MediaMTX (bluenviron/mediamtx) | MIT | very active (20k stars, releases monthly) | yes, Docker image 1.21.3 pinned by digest |
| GStreamer gst-rtsp-server | LGPL-2.1 | maintained in the GStreamer monorepo (the old separate repo is archived) | yes, Ubuntu 26.04 packages in the helper image |
| onvif-go (0x524a/onvif-go) | MIT | active, small (v1.3.0, 2026-10-29) | yes, release binary checked against checksums.txt. Its virtual camera has no events and no WS-Discovery responder in the command-line tool, and its stream URIs always point at port 8554, so it is used for Device/Media/PTZ only |
| KoynovStas/onvif_srvd | GPL-2.0 | last change 2025-01 | no: GPL, slow-moving, and needs building with gSOAP |
| live555 testOnDemandRTSPServer | LGPL-3.0 | maintained | no: not packaged in Ubuntu 26.04 (livemedia-utils is gone); GStreamer was easier to run in Docker |
| ffmpeg / ffprobe | LGPL/GPL | maintained | the machine's own packages |

`rtsp_stub.py` and `onvif_sim.py` are the rig's own code.

## Files

- `rig.py`: the runner
- `profiles.toml`: profiles, cases and logins
- `rtsp_stub.py`: RTSP server for quirks and faults
- `onvif_sim.py`: virtual ONVIF cameras and WS-Discovery
- `gst_server.py`: GStreamer RTSP server
- `image/Dockerfile`: helper image (GStreamer + Python, Ubuntu packages only)
- `fetch.sh`: downloads (MediaMTX image, onvif-go, optionally the latest release)
