# Running AmberSight in Docker

The image is built from this repository. It holds the AmberSight program,
ffmpeg and the Intel/AMD video drivers; your settings and recordings live in
two folders you mount.

## Quick start

```sh
git clone https://github.com/HuskerMinion/AmberSight.git
cd AmberSight
docker compose -f docker/compose.yaml up -d --build
```

Then open `http://<this-computer>:8090`. The first build takes several minutes.

Add the first user (Settings > Users & security) before anything else.
Until then, as with any new install, anyone who can reach port 8090 can
watch the cameras and change every setting. With host networking that is
every network the computer is on. Don't forward the port on your router;
use a VPN such as Tailscale or WireGuard to reach it from outside.

Edit `docker/compose.yaml` first if you want the settings and recordings
somewhere other than `docker/config` and `docker/data`, and set `TZ` to your
time zone.

Without Compose:

```sh
docker build -t ambersight .
docker run -d --name ambersight --restart unless-stopped --stop-timeout 30 --network host \
  -e TZ=America/New_York \
  -v /srv/ambersight/config:/config -v /srv/ambersight/data:/data \
  --device /dev/dri ambersight
```

## Folders

| In the container | What is in it |
|---|---|
| `/config` | `config.toml`, written with starter settings on the first start |
| `/data` | the database, recordings, AI models, backups |

Keep `/data` on a local disk: the database is SQLite and should not be on a
network share. To record to a bigger or separate drive, mount it as another
folder (for example `-v /mnt/cameras:/recordings`) and choose it in
Settings > Recording.

AmberSight runs as user and group 1000. Set `PUID` and `PGID` to run it as
another user. When `/config` or `/data` belongs to someone else (the first
start, a changed `PUID`), the container gives that folder and everything in
it to that user when it starts. Files copied in later as root, into a folder
that already belongs to the user, are not changed: fix their owner by hand.
Use `PUID`/`PGID` rather than `--user`: with `--user` the folders and the
video devices' groups are not set up. `PUID=0` runs AmberSight as root, for
rootless Docker or Podman, where root in the container is your own user.

## Network

**Host networking** (`network_mode: host`, or `--network host`) is the
simplest and is what the examples use. It is needed for:

- **Find cameras**: ONVIF discovery uses multicast, which does not reach a
  container on Docker's own network.
- **Battery Reolink cameras** (Argus): they are found with a UDP broadcast,
  and their motion push arrives on port 80 (see the install guide).

On Docker's own network (bridge), publish the port (`-p 8090:8090`) and add
cameras by address. RTSP, ONVIF events, PTZ and everything else work the same.

Ports, if you publish them one by one:

| Port | For |
|---|---|
| 8090/tcp | the web interface, API and live video (HTTP or HTTPS) |
| 1935/tcp | only if cameras push RTMP to AmberSight |
| 80/tcp | only for battery Reolink cameras' motion push |

Port 80: AmberSight does not run as root (unless `PUID=0`), so listening on port 80 needs
`--sysctl net.ipv4.ip_unprivileged_port_start=80` on a bridge network. With
host networking that setting is the host's own.

## Hardware video decode

Pass `/dev/dri` through (`--device /dev/dri`) for Intel or AMD hardware
decode (VAAPI); the container joins the groups that own those devices by
itself. Then set it per camera in `config.toml`:

```toml
hwaccel = "vaapi"                         # the first graphics device, /dev/dri/renderD128
hwaccel = "vaapi:/dev/dri/renderD129"     # a particular one, when there are several
```

`ls -l /dev/dri/by-path` on the host shows which `renderD` belongs to which
graphics card (an NVIDIA card's node does not do VAAPI). Without `hwaccel`,
motion analysis decodes on the processor.

## Updates

The built-in updater does not install updates in a container (an update
would be lost when the container is recreated); it still tells you when a new
version is out. To update, pull the new source and rebuild:

```sh
git pull
docker compose -f docker/compose.yaml up -d --build
```

Settings and recordings stay in the mounted folders.

## Restarts and stopping

Turning HTTPS on or off asks for a restart (**Restart now**): AmberSight
exits and Docker starts it again, so keep `restart: unless-stopped` (or
`--restart unless-stopped`). Without a restart policy it stays stopped until
you start it.

On stop, AmberSight finishes the recordings and alerts in progress, which can
take longer than Docker's default 10 seconds. The examples allow 30
(`stop_grace_period: 30s`, `--stop-timeout 30`).

## Building

The image builds with plain `docker build`; BuildKit is not needed. The
build stage compiles AmberSight from source (a couple of minutes on a fast
machine, longer on a Raspberry Pi), and the final image is Debian 13 slim
with ffmpeg. It builds for the machine it is built on; it has been tested on
`amd64` (`arm64`, such as a Raspberry Pi 5, should work but is not tested yet).
