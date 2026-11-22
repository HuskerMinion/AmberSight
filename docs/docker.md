# Running AmberSight in Docker

A prebuilt image is published with each release:

| Image | For |
|---|---|
| `ghcr.io/huskerminion/ambersight:latest` | the newest release, `amd64` and `arm64` (such as a Raspberry Pi 5) |
| `ghcr.io/huskerminion/ambersight:<version>` | one release, e.g. `0.11.0` |
| `ghcr.io/huskerminion/ambersight:cuda` | the newest release with the AI on an NVIDIA card, `amd64` only (also `<version>-cuda`) |
| `ghcr.io/huskerminion/ambersight:openvino` | the newest release with the AI on Intel graphics, `amd64` only (also `<version>-openvino`) |

It holds the AmberSight program, ffmpeg and the Intel/AMD video drivers; your
settings and recordings live in two folders you mount. You can also build the
image yourself from this repository ([Building](#building)).

Unraid: see [unraid/README.md](../unraid/README.md).

## Quick start

Save [`docker/compose.yaml`](../docker/compose.yaml) in a folder of its own and
run, in that folder:

```sh
docker compose -f compose.yaml up -d
```

(From a clone of this repository: `docker compose -f docker/compose.yaml up -d`.)
Then open `http://<this-computer>:8090`.

Add the first user (Settings > Users & security) before anything else.
Until then, as with any new install, anyone who can reach port 8090 can
watch the cameras and change every setting. With host networking that is
every network the computer is on. Don't forward the port on your router;
use a VPN such as Tailscale or WireGuard to reach it from outside.

Edit `compose.yaml` first if you want the settings and recordings somewhere
other than `config` and `data` next to it, and set `TZ` to your time zone.

Without Compose:

```sh
docker run -d --name ambersight --restart unless-stopped --stop-timeout 30 --network host \
  -e TZ=America/New_York \
  -v /srv/ambersight/config:/config -v /srv/ambersight/data:/data \
  --device /dev/dri ghcr.io/huskerminion/ambersight:latest
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
`/recordings` is handed to that user only when it is new: owned by root (what
Docker makes of a host folder that did not exist) and empty. Any other folder
you record to, including an existing share full of recordings, is left as it
is and must already be writable by `PUID:PGID`. Other new, empty mount points
can be named in `RECORDINGS_DIRS` (separated by colons, for example
`/cams1:/cams2`) to get the same treatment; a symlink is left alone. When
AmberSight cannot write to a recording folder, the health page says so, and a
camera recorded through ffmpeg shows it as its last error too.
Use `PUID`/`PGID` rather than `--user`: with `--user` the folders and the
video devices' groups are not set up. `PUID=0` runs AmberSight as root, for
rootless Docker or Podman, where root in the container is your own user.
On Unraid, use `PUID=99` and `PGID=100` (`nobody:users`, who own the shares).

`TZ` sets the time zone (for example `America/Chicago`); schedules and quiet
hours use it. Without it the container runs on UTC.

## Network

**Host networking** (`network_mode: host`, or `--network host`) is the
simplest and is what the examples use. It is needed for:

- **Find cameras**: ONVIF discovery uses multicast, which does not reach a
  container on Docker's own network.
- **Battery Reolink cameras** (Argus): they are found with a UDP broadcast,
  and their motion push arrives on port 80 (see the install guide).
- **Apple Home (HomeKit)**: Apple devices find the bridge over mDNS
  (Bonjour, multicast on UDP 5353), which does not cross Docker's bridge
  network. Unraid's default (bridge) does not work for it; choose host
  networking. (On a bridge network the Docker host itself still sees the
  record, at the container's 172.17.x address; that is harmless. Other
  machines cannot discover it, but a controller can still pair by the host's
  address when the HomeKit port is published; live video then also needs the
  UDP ports, which change each time, so use host networking.) HomeKit
  has been tested with an iPhone and a HomePod mini hub, not yet in Docker;
  reports are welcome in GitHub Discussions.

On Docker's own network (bridge), publish the port (`-p 8090:8090`) and add
cameras by address. RTSP, ONVIF events, PTZ and everything else work the same.

**Behind a reverse proxy or tunnel** (nginx, Caddy, Traefik, cloudflared,
Tailscale Funnel), every request reaches AmberSight from the proxy, so one
visitor's failed sign-ins would lock out everyone. Tell AmberSight the proxy's
own address in `config.toml` and it takes each visitor's address from
`X-Forwarded-For`:

```toml
[server]
trusted_proxies = ["172.17.0.1"]   # the proxy as the container sees it (Docker's bridge gateway)
```

List only the proxy, never a LAN or internet range: anyone in a listed range
could claim any address. The proxy must add the visitor to `X-Forwarded-For`
(Caddy, Traefik, cloudflared and Tailscale do; nginx needs
`proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`).

Ports, if you publish them one by one:

| Port | For |
|---|---|
| 8090/tcp | the web interface, API and live video (HTTP or HTTPS) |
| 1935/tcp | only if cameras push RTMP to AmberSight |
| 80/tcp | only for battery Reolink cameras' motion push |
| 51830-51894/tcp | only for Apple Home (HomeKit), with host networking (mDNS needs it): the bridge, and one port per camera after it |
| 51900-51999/udp | only for Apple Home live video: RTCP back from the Apple device (`[homekit] rtp_ports`) |

Port 80: AmberSight does not run as root (unless `PUID=0`), so listening on port 80 needs
`--sysctl net.ipv4.ip_unprivileged_port_start=80` on a bridge network. With
host networking that setting is the host's own.

## Hardware video decode

Pass `/dev/dri` through (`--device /dev/dri`) for Intel or AMD hardware
decode (VAAPI, and Intel Quick Sync, whose runtime the image includes); the container joins the groups that own those devices by
itself. Motion detection then decodes on the graphics card by itself: at
start AmberSight test-decodes a short H.264 and H.265 sample on each render
node and uses the first that decodes it right (Status > Cameras shows GPU or
CPU per camera; a card that fails is named in Status). Without `/dev/dri`
everything decodes on the processor, as before.

To choose per camera (camera settings > Stream > More settings > Hardware
decoding, or in `config.toml`):

```toml
hwaccel = "off"                           # the processor
hwaccel = "vaapi"                         # VAAPI on the first render node that passed the test
hwaccel = "vaapi:/dev/dri/renderD129"     # a particular one, when there are several
hwaccel = "qsv:/dev/dri/renderD128"       # Intel Quick Sync
```

`ls -l /dev/dri/by-path` on the host shows which `renderD` belongs to which
graphics card (an NVIDIA card's node does not do VAAPI).

## AI on an NVIDIA card

The plain image runs the built-in AI on the processor. For an NVIDIA card,
use the CUDA image instead (`:cuda`, built from `Dockerfile.cuda`: NVIDIA's
CUDA 13 + cuDNN 9 runtime libraries with AmberSight and its NVIDIA add-on; it
is large, about 4 GB, and `amd64` only):

```sh
docker run -d --name ambersight --restart unless-stopped --stop-timeout 30 --gpus all --network host \
  -v ./config:/config -v ./data:/data ghcr.io/huskerminion/ambersight:cuda
```

The host needs NVIDIA's driver and the
[NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)
(`--gpus all` passes the cards in). With compose, use
`image: ghcr.io/huskerminion/ambersight:cuda` and add:

```yaml
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: all
              capabilities: [gpu]
```

Settings > Detection & alerts > Objects (AI) then shows *Running on: NVIDIA
...*. The AI's memory pool on the card is capped at 1.5 GB (`gpu_memory_mb`
in `[ai]`). The plain image given `--gpus all` finds the card but has no CUDA
libraries: it stays on the processor, downloads nothing, and says why in
Settings (*libcudart.so.13 not found*).

## AI on Intel graphics

For Intel graphics (an Arc card, or the graphics built into a recent Intel
processor), use the OpenVINO image (`:openvino`, built from
`Dockerfile.openvino`: Ubuntu 26.04 with Intel's compute driver and
AmberSight's Intel graphics add-on; `amd64` only). It needs only the card's
device files:

```sh
docker run -d --name ambersight --restart unless-stopped --stop-timeout 30 --device /dev/dri --network host \
  -v ./config:/config -v ./data:/data ghcr.io/huskerminion/ambersight:openvino
```

With compose, use `image: ghcr.io/huskerminion/ambersight:openvino` and the
same `devices: - /dev/dri:/dev/dri` as for hardware decoding. The container
user gets the group of the device files by itself. Settings > Detection &
alerts > Objects (AI) then shows *Running on: Intel(R) Arc(TM) ...
(OpenVINO)*. The plain image has no Intel compute driver (Debian has no
package of it): given `/dev/dri` it decodes video on the card but runs the AI
on the processor, and Settings says to use the `:openvino` image.

## Recordings whose files are gone

After moving recordings or replacing a drive, Settings > Recording > Drives & folders can find and
remove the ones whose files are gone. From the command line, run it as the container's user (not
root):

```sh
docker exec -u 1000:1000 ambersight ambersight storage prune-missing --config /config/config.toml
docker exec -u 1000:1000 ambersight ambersight storage prune-missing --config /config/config.toml --apply
```

(use your `PUID:PGID` if you set them; the first line only reports). See the install guide's
"Moving recordings or moving to a new computer".

## Updates

The built-in updater does not install updates in a container (an update
would be lost when the container is recreated); it still tells you when a new
version is out. To update, pull the new image and recreate the container:

```sh
docker compose pull && docker compose up -d
```

(add `-f compose.yaml`, or `-f docker/compose.yaml` in a clone, when you are
not in the folder that holds it). Without Compose: `docker pull
ghcr.io/huskerminion/ambersight:latest`, then remove the container and run it
again with the same `docker run` line. An image you built yourself: `git pull`
and `docker compose -f docker/compose.yaml up -d --build`.

`:latest`, `:cuda` and `:openvino` always point at the newest release; use a version tag
(e.g. `ghcr.io/huskerminion/ambersight:0.11.0`) to stay on one release and
update when you choose. Settings and recordings stay in the mounted folders.

## Cameras fail with "Resource temporarily unavailable"

A camera note like `the system would not start another process or thread
(Resource temporarily unavailable)`, or ffmpeg lines such as
`pthread_create() failed` or `Error while opening decoder: Resource
temporarily unavailable` (followed by `error code: -22`), mean the container
ran out of processes and threads. A container started with a low
`--pids-limit` (or `pids_limit:` in Compose) allows only that many in all;
a camera that AmberSight re-encodes (an overlay or
`ingest = "ffmpeg"`) runs an ffmpeg with a dozen or more threads, and 4K
cameras need the most.

- Raise or remove the limit: `--pids-limit -1` (no limit), or a few hundred
  per 4K camera; `pids_limit: -1` in Compose. On Unraid, remove `--pids-limit`
  from Extra Parameters.
- Or move the camera back to the built-in reader (turn off the overlay,
  remove any `ingest = "ffmpeg"` line), which needs no
  ffmpeg process per camera. The camera report says why a camera is on ffmpeg.
- The camera report (Network section of the camera's settings) shows the
  container's limit and how much of it is in use under **This server**.

## Restarts and stopping

Turning HTTPS on or off asks for a restart (**Restart now**): AmberSight
exits and Docker starts it again, so keep `restart: unless-stopped` (or
`--restart unless-stopped`). Without a restart policy it stays stopped until
you start it.

On stop, AmberSight finishes the recordings and alerts in progress, which can
take longer than Docker's default 10 seconds. The examples allow 30
(`stop_grace_period: 30s`, `--stop-timeout 30`).

## Building

To build the image from this repository instead of pulling it:

```sh
git clone https://github.com/HuskerMinion/AmberSight.git
cd AmberSight
docker build -t ambersight .                        # or: -f Dockerfile.cuda -t ambersight:cuda
                                                    # or: -f Dockerfile.openvino -t ambersight:openvino
```

With Compose, replace the `image:` line in `docker/compose.yaml` with the
commented `build: ..` and `image: ambersight` lines, and run
`docker compose -f docker/compose.yaml up -d --build`. The first build takes
several minutes.

The image builds with plain `docker build`; BuildKit is not needed. The
build stage compiles AmberSight from source (a couple of minutes on a fast
machine, longer on a Raspberry Pi), and the final image is Debian 13 slim
with ffmpeg. It builds for the machine it is built on.
