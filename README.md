<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/logo-dark.svg">
    <img src="docs/images/logo-light.svg" alt="AmberSight" width="410">
  </picture>
</p>

<p align="center"><strong>Self-hosted camera recording and AI detection, written in Rust.</strong><br>
Linux · macOS · Windows</p>

<p align="center">
  <a href="https://github.com/HuskerMinion/AmberSight/releases/latest"><img src="https://img.shields.io/github/v/release/HuskerMinion/AmberSight?label=release&color=e9a23b" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-PolyForm%20Noncommercial-blue" alt="PolyForm Noncommercial license"></a>
  <a href="https://buymeacoffee.com/huskerminion"><img src="https://img.shields.io/badge/Buy%20Me%20a%20Coffee-ffdd00?logo=buymeacoffee&logoColor=black" alt="Buy Me a Coffee"></a>
</p>

AmberSight is a self-hosted camera security and recording server written in Rust.
It runs on Linux, macOS, and Windows. It ingests RTSP (and other ffmpeg-readable)
camera streams, detects motion, records video segments, builds motion clips with
pre/post roll, and serves a web dashboard plus a JSON API.

Step-by-step installation for Windows, macOS and Linux is in the
[install guide](docs/install.md). The API for app developers is in the
[API guide](docs/api-guide.md).

<p align="center"><img src="docs/images/screenshots/1-live.jpg" alt="AmberSight live view: eight cameras with AI detections" width="900"></p>

More screenshots: [Screenshots](#screenshots).

## Screenshots

| | |
|---|---|
| <img src="docs/images/screenshots/2-timeline.jpg" alt="Timeline: all cameras in sync with alert markers"><br>**Timeline**: every camera in sync, alerts by kind, mark and export a clip | <img src="docs/images/screenshots/3-events.jpg" alt="Events: list beside the player"><br>**Events**: filter chips, list beside the player, the AI's verdict |
| <img src="docs/images/screenshots/4-camera-panel.jpg" alt="Camera settings in a side panel over the live view"><br>**Camera settings**: a side panel over Live, with a live picture | <img src="docs/images/screenshots/5-zones.jpg" alt="Zones editor with live motion boxes"><br>**Zones**: detect and ignore zones, privacy masks, live motion |
| <img src="docs/images/screenshots/6-performance.jpg" alt="Performance charts and per-camera table"><br>**Performance**: the last hour of CPU, AI, network and disk | <img src="docs/images/screenshots/7-live-light.jpg" alt="Live view in the light theme"><br>**Light theme** |

<p align="center"><img src="docs/images/screenshots/8-phone.jpg" alt="Live view on a phone" width="260"><br><b>On a phone</b></p>

<sub>The cameras in these screenshots are public-domain video from Grand Canyon
National Park's B-roll library (National Park Service), looped into a demo
AmberSight server. The National Park Service does not endorse AmberSight.</sub>

## Install

Download the package for your system from the
[releases page](https://github.com/HuskerMinion/AmberSight/releases), or build
from source (next section). Every release has a signed `SHA256SUMS` list.
Step-by-step instructions for every system are in the
[install guide](docs/install.md).

| System | Package | What it does |
|---|---|---|
| Windows 10/11, 64-bit | `ambersight-<version>-windows-x86_64-setup.exe` | Installs to `C:\Program Files\AmberSight NVR`, adds the "AmberSight" Windows service (starts at boot), a firewall rule for port 8090 and Start menu and desktop shortcuts. Settings and recordings go to `C:\ProgramData\AmberSight`. |
| Debian, Ubuntu, Linux Mint, Raspberry Pi OS (64-bit) | `ambersight_<version>_amd64.deb` or `_arm64.deb` | `sudo apt install ./ambersight_<version>_amd64.deb`. Installs `/usr/bin/ambersight`, `/etc/ambersight/config.toml`, data in `/var/lib/ambersight`, a systemd service running as the user `ambersight`, an app menu entry, and ffmpeg. |
| Fedora | `ambersight-<version>-1.x86_64.rpm` or `.aarch64.rpm` | Add [RPM Fusion](https://rpmfusion.org/Configuration) first, then `sudo dnf install ./ambersight-<version>-1.x86_64.rpm --allowerasing`. Same layout as the .deb. |
| macOS and other Linux | `.tar.gz` (Windows also has a `.zip`) | Unpack, then run `./ambersight setup` (see below). |
| Docker | build from this repository | `docker compose -f docker/compose.yaml up -d --build`. See [docs/docker.md](docs/docker.md) for folders, networking and hardware decode. |

The packages need Debian 13, Ubuntu 25.04, Fedora 42 or newer (and 64-bit
Raspberry Pi OS, trixie or newer). Intel Macs have no built-in AI; use an AI
server or an external detector.

Once running, open **http://localhost:8090**. The first time, the Live page
shows a **Getting started** checklist: add cameras (Settings > Cameras > Find
cameras), get alerts on your phone, add a sign-in account, choose where
recordings go (Settings > Recording), and optionally HTTPS and AI. Updating a package keeps your settings.

### ambersight setup

From an unpacked `.tar.gz` or `.zip`, the program installs itself:

```bash
./ambersight setup               # for you only (macOS, Linux)
sudo ./ambersight setup --system # for the whole computer, starts at boot
./ambersight setup --uninstall   # removes it; settings and recordings stay
```

On Windows run `ambersight.exe setup` in an Administrator terminal. Setup
copies the program, creates `config.toml` from the starter settings (an
existing file is never overwritten), installs and starts the service, opens the
firewall port on Windows (on Linux it prints the command if ufw or firewalld is
on), adds a shortcut and opens the page. Other options: `--here` (keep the
program and config where they are), `--dry-run` (only print the plan) and
`--no-browser`. Use the same `--system` or `--here` with `--uninstall`.

| System | Program | Settings and data |
|---|---|---|
| macOS | `~/Applications/AmberSight` | `~/Library/Application Support/AmberSight` |
| macOS `--system` | `/Applications/AmberSight` | `/Library/Application Support/AmberSight` |
| Linux | `~/.local/share/ambersight` | `~/.config/ambersight`, data in `~/.local/share/ambersight/data` |
| Linux `--system` | `/opt/ambersight` | `/etc/ambersight/config.toml`, data in `/var/lib/ambersight` |

### ffmpeg

- **Windows and macOS:** nothing to install. If none is found at start, AmberSight
  downloads one once (see Cross-platform notes).
- **Linux:** ffmpeg is required. The .deb installs it. Otherwise
  `sudo apt install ffmpeg` or `sudo pacman -S ffmpeg`. On Fedora the standard
  `ffmpeg-free` cannot decode H.265 cameras: use RPM Fusion's ffmpeg.

### Background service commands

`ambersight setup` and the installers already do this. To install only the
service, which starts at boot and restarts on failure:

```bash
ambersight service install --config /path/to/config.toml
ambersight service status
ambersight service stop | start
ambersight service uninstall
```

The config path is stored as an absolute path and the service runs with the
config file's directory as its working directory, so a relative
`data_dir = "amberdata"` keeps resolving next to the config. On Linux,
`ffmpeg` must be on the service's `PATH`.

- **Linux (systemd)**: by default a per-user unit is written to
  `~/.config/systemd/user/ambersight.service` (no root needed) and enabled
  with `systemctl --user`. To have it start at boot without logging in, run
  `loginctl enable-linger $USER` once. `--system` installs
  `/etc/systemd/system/ambersight.service` running as the current user; it
  needs root and prints the exact `sudo` command if you are not. The installer's
  `PATH` is copied into the unit. Logs: `journalctl --user -u ambersight -f`.
- **macOS (launchd)**: a LaunchAgent at
  `~/Library/LaunchAgents/com.ambersight.server.plist` (or a LaunchDaemon in
  `/Library/LaunchDaemons` with `--system`, via sudo), loaded with
  `launchctl bootstrap`. `service stop` unloads it (launchd would otherwise
  restart it); `service start` loads it again. Logs are
  `ambersight.out.log` and `ambersight.err.log` in the config's directory.
- **Windows**: registers the "AmberSight" service (display name "AmberSight NVR",
  automatic start, restart on failure) that runs
  `ambersight.exe service-run --config <path>`. Run `service install` and
  `service uninstall` from an **Administrator** terminal. Logs go to
  `ambersight.log` in the config's directory. The service runs as LocalSystem
  and sees the system-wide `PATH`; when it finds no ffmpeg there it downloads
  its own (see below).

## Build from source

You need a recent stable Rust toolchain (`rustup update stable`). On Linux you
also need ffmpeg (`apt install ffmpeg`).

```bash
cargo build --release
```

The binary is `target/release/ambersight`. Quick start:

```bash
./ambersight init config.toml      # create a starter config
# edit config.toml: add your cameras
./ambersight run --config config.toml
```

Open `http://localhost:8090`. A new install has no camera: use Find cameras
(Settings > Cameras) or add one by address. To try AmberSight without
hardware, add a synthetic camera:

```toml
[[cameras]]
name = "Test Pattern"
url = "lavfi://mandelbrot=size=640x360:rate=10"
```

## Before using it on a network

- **Add a user account** (Settings > Users & security). With no accounts, anyone who can
  reach the port can watch every camera and change every setting.
- **Turn on HTTPS** (`[server] tls = true`). AmberSight runs its own private
  certificate authority; install it once per device from the account menu
  ("Trust this server") or `/tls/ca.crt`.
- **Optional AI object detection:** Settings > Detection & alerts > Objects (AI) downloads a model (Tiny, Small or
  Large) and turns it on; from the command line,
  `ambersight ai-model --config config.toml` downloads it and shows the two
  lines to add to the config.

## Features

- **Live view:** many cameras at once. Each tile plays fragmented-MP4 video in
  the browser (Media Source); a light mode sends JPEG frames instead for large
  grids and slow links. Corner labels (name, site, date, time) and AI detection
  chips on each tile. Layouts, groups, paging, full screen, a phone layout, and
  a Data saver mode that plays the sub stream.
- **Streams that fit the device:** apps get the best quality their device can
  decode: streams too big or in a codec the device can't play are converted on
  the graphics card (or in software when there is none).
- **Recording:** continuous segments per camera, or a sub stream all the time
  with the main stream only around events. Motion clips with pre-roll and
  post-roll. By default recordings use what the disk can spare and keep 10% of
  it free, deleting the oldest first; or give each folder a fixed size. Several
  storage folders, an archive tier, and retention in days. Footage around
  events is deleted last. A camera can also save each event's clip (or only
  events AI confirmed) as an .mp4 to a second folder, such as a OneDrive or Google Drive
  folder, shortly after the event: off-site copies from the same stream.
- **Timeline:** all cameras in sync, alert markers by kind, fast and reverse
  playback, and export of a time range.
- **Events:** a list beside the player, filters, false alarms, protect (kept
  from cleanup), and export. Events are stored in a SQLite database.
- **AI:** built-in YOLOX object detection on CPU or GPU (DirectML on Windows,
  NVIDIA CUDA on Linux, Core ML on Apple silicon Macs),
  with a Large model for the most accurate checks, an AI server option, or an
  external detector command. AI review explains why an event did or did not
  alert. AI remembers fixed spots it keeps mistaking for a person or a car (a
  bush, a bin, a parked car) and stops alerting for them. Parked vehicles are ignored, package
  detection, face recognition, smart search by description (CLIP), and a
  loud-sound trigger.
- **Zones and masks:** rectangle and polygon detect and ignore zones, and
  privacy masks.
- **Alerts:** push, Telegram, ntfy, Pushover, email, webhooks, Home Assistant,
  MQTT with Home Assistant discovery, and action commands. System alerts for a
  camera offline, recordings not written or a drive nearly full.
- **Profiles and schedules:** named sets of overrides picked by a weekly
  schedule, including sunrise and sunset.
- **Camera watchdog:** restarts a camera that stops sending video, or power
  cycles it through a smart plug or PoE port.
- **PTZ and two-way talk** through ONVIF.
- **Time-lapse:** each day of a camera in a short video.
- **Off-site backup:** to a folder or S3, encrypted.
- **Find cameras:** ONVIF discovery on this subnet and any other subnets you
  list, sign-in with your usual camera login, and an optional hourly check
  that tells you when a new camera appears.
- **Linked servers:** several sites in one interface.
- **Users and per-camera access.** Three roles: viewers watch; operators also arm and
  disarm, move PTZ cameras, talk, record clips, keep and mark events and tune their cameras'
  detection and alerts (each right can be turned off); admins do everything. Viewers and operators
  can be limited to some cameras. Two-step codes (authenticator app, with recovery
  codes) and per-device tokens for apps and RTSP. HTTPS with a private certificate authority.
- **Updates:** signed self-update. AmberSight checks for new releases every
  12 hours (it asks api.github.com and sends only `AmberSight/<version>`;
  `[update] check = false` stops it); an admin installs one with a click (or
  `ambersight update`).
- **Interface:** keyboard shortcuts, Ctrl+K command palette, and health and
  performance charts (CPU, AI, network, disk).
- **Command line:** `ambersight --help` lists the commands (`init`, `run`,
  `setup`, `service`, `cameras`, `events`, `snapshot`, `ai-model`, `discover`,
  `ptz`, `backup`, and more).

## Configuration

Most settings are in the web page under **Settings**. Its sections are
Cameras, Recording, Detection & alerts, Smart features, Schedules & presence,
Users & security, Connections and System. They are saved to `config.toml`. See `config.example.toml` for
every option. Camera URLs:

- `rtsp://...` - RTSP cameras (forced over TCP)
- `http://...` / file paths - anything ffmpeg can read
- `lavfi://...` - synthetic test source (e.g. `lavfi://testsrc=size=640x360:rate=10`)

## Data layout

Inside `data_dir` (default `amberdata`; `/var/lib/ambersight` for the packages):

```
ambersight.db                   # SQLite: events and the recordings index
cameras/<slug>/segments/*.mp4   # continuous segments
clips/<id>.mp4                  # motion clips
thumbs/                         # event pictures
models/                         # downloaded AI models
timelapse/<slug>/               # time-lapse videos
backups/                        # config.toml and database backups
tools/                          # ffmpeg downloaded by AmberSight (Windows, macOS)
```

A storage folder set in the config holds its own `cameras/` tree.

The database stores each recording as its folder's name plus a path inside
that folder, so a moved folder (another drive, another computer, Windows to
Linux) only needs its new path set in Settings > Recording > Drives & folders.
Recordings whose files are gone (a folder emptied, a drive reformatted) are
noticed at start and can be removed from the database there, or with
`ambersight storage prune-missing` (a dry run unless `--apply`; it backs the
database up first and never deletes files). See "Moving recordings or moving
to a new computer" in [docs/install.md](docs/install.md).

## API

The web page uses a JSON API that you can use too: status, cameras, events,
clips, snapshots, live streams and settings. Sign in with HTTP Basic auth or a
session. See [docs/api-guide.md](docs/api-guide.md) for the endpoints,
authentication and examples. For example, `GET /api/status` returns server
status and `GET /api/snapshot/{id}` the latest JPEG from a camera.

## Home Assistant

**MQTT.** In `config.toml`, under the alerts section, set:

```toml
mqtt_url = "mqtt://user:pass@broker:1883"
mqtt_prefix = "ambersight"   # optional, default "ambersight"
mqtt_discovery = true        # publish Home Assistant discovery messages
```

With discovery on, Home Assistant creates these entities (all retained, with availability from `<prefix>/status`):

- Per camera (one device each): Motion, Person, Vehicle, Animal (from what the camera itself reports), Online, Snapshot, and a "Record clip" button. Battery (on-demand) cameras also get an Asleep sensor and a Wake button.
- AmberSight server device: Shield switch, Profile select ("Auto" plus your profiles), an Active profile sensor, and a Problem sensor (on while a camera is offline, recordings are not being written or a drive is nearly full; the list is in its attributes).

Command topics (`<cam>` is the camera name in lower case with `_` for spaces):

| Topic | Payload |
|---|---|
| `<prefix>/shield/set` | `ON` / `OFF` |
| `<prefix>/<cam>/trigger` | clip length in seconds (default 20) |
| `<prefix>/<cam>/wake` | anything; wakes a battery camera |
| `<prefix>/profile/set` | a profile name, or `Auto` to follow the schedule |

State topics: `<prefix>/<cam>/{motion,person,vehicle,animal,online,asleep}` (`ON`/`OFF`), `<prefix>/shield/state`, `<prefix>/profile/state`, `<prefix>/profile/active`, `<prefix>/problems` (`ON`/`OFF`) and `<prefix>/problems/list` (JSON `{"problems": [...]}`).

**Live video.** Add the Generic Camera integration per camera, with the camera's numeric `id` from `/api/cameras` (8090 is the default port):

- Still image URL: `http://user:pass@HOST:8090/api/snapshot/ID`
- Stream source: `http://user:pass@HOST:8090/api/live/ID/stream` (fragmented MP4; add `?q=sub` for the sub stream). This works for cameras on the built-in reader (RTSP and HTTP-FLV URLs, the default); cameras re-encoded through ffmpeg (masks, overlay, `ingest = "ffmpeg"`) have no live stream.

HTTP Basic auth is accepted on these endpoints, so the credentials go in the URL. Use an account that can see the camera. Snapshot and stream requests wake a sleeping battery camera.

## Cross-platform notes

### ffmpeg

AmberSight looks for ffmpeg in this order: next to the AmberSight program,
then `<data_dir>/tools` (its own downloaded copy), then the system `PATH`.
The status page (Settings > System > Status & updates, Server card) shows which version is in use.

- **macOS and Windows**: if no working ffmpeg is found when the server starts,
  AmberSight downloads ffmpeg 8.1.2 once (about 27-33 MB on a Mac, 105 MB on
  Windows), checks it against a fixed checksum, and keeps just the ffmpeg
  program in `<data_dir>/tools`. If that download fails, the message says how
  to install it by hand (`brew install ffmpeg` / `winget install ffmpeg`, or
  put the ffmpeg program next to AmberSight).
  - Mac builds: [Martin Riedl's build server](https://ffmpeg.martin-riedl.de)
    (Apple silicon and Intel; signed with the builder's Developer ID).
  - Windows build: [gyan.dev](https://www.gyan.dev/ffmpeg/builds/) "essentials",
    from its versioned GitHub releases.
  - License: these ffmpeg builds are GPL v3 (FFmpeg itself is LGPL/GPL). They
    are not part of AmberSight; your computer downloads them from their builders.
- **Linux**: install it with your package manager (`sudo apt install ffmpeg`,
  `sudo pacman -S ffmpeg`); the system build knows your graphics drivers for
  hardware decoding. On Fedora the standard `ffmpeg-free` cannot decode H.265
  cameras: add RPM Fusion (https://rpmfusion.org/Configuration) first, then
  `sudo dnf install ffmpeg --allowerasing`. AmberSight says so clearly if
  ffmpeg is missing.
- An ffmpeg you install yourself (`brew install ffmpeg`, `winget install ffmpeg`,
  `choco install ffmpeg`) is used as well; nothing is downloaded then.

The code uses portable crates; the only platform-specific code is the service
installer (see "Background service commands"). Build with `cargo build --release` on each target.

## License

AmberSight is **source-available**, not open source. It is licensed under the
[PolyForm Noncommercial License 1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0)
(see `LICENSE`).

In plain words (the license text is what counts):

- You may use AmberSight for any **noncommercial** purpose: at home, for a
  hobby, for study, at a school, charity, public research or government
  organization.
- You may change it and share your changes, for noncommercial purposes, as
  long as you pass on the license and its `Required Notice` line.
- You may **not** sell it, charge for installing or running it, bundle it in a
  product or service you sell, or use it in a business. Businesses that want
  to use it can ask the copyright holder for a separate license.

Contributions are welcome; see `CONTRIBUTING.md` for the terms.

Third-party code keeps its own license (all notices are collected in
`THIRD-PARTY-NOTICES.txt`, which every package includes): the vendored RTSP library in
`vendor/retina` (MIT or Apache-2.0), the Rust crates AmberSight builds on
(their licenses are listed in `Cargo.lock` / on crates.io), the AI models
AmberSight downloads (each with its own license, named where it is set up),
and ffmpeg (see above).
