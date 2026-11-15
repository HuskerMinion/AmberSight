# AmberSight on Unraid

`ambersight.xml` is an Unraid container template for the prebuilt image
`ghcr.io/huskerminion/ambersight:latest`. It is not in Community Apps yet;
until it is, add it by hand. Unraid support is new and has not been tested on
Unraid yet: please report what works and what doesn't in
[Discussions](https://github.com/HuskerMinion/AmberSight/discussions).

## Adding the container

1. Save the template among your own templates. In the Unraid terminal
   (the `>_` icon at the top right):

   ```sh
   wget -O /boot/config/plugins/dockerMan/templates-user/my-AmberSight.xml \
     https://raw.githubusercontent.com/HuskerMinion/AmberSight/master/unraid/ambersight.xml
   ```

2. Open **Docker**, then **Add Container**, and choose **AmberSight** under
   **Template** (in the user templates).
3. Check the settings below, then **Apply**.
4. Open the web interface (the container's **WebUI**, port 8090) and add the
   first user under Settings > Users & security before anything else.

## Settings

| Setting | Default | Notes |
|---|---|---|
| Web UI port | 8090 | web interface, API and live video |
| RTMP port | 1935 | only when cameras push RTMP; can be removed |
| Config | `/mnt/user/appdata/ambersight/config` | `config.toml` |
| Data | `/mnt/user/appdata/ambersight/data` | database, AI models, backups, and recordings unless you add a recordings folder |
| Recordings | empty | optional share for recordings, e.g. `/mnt/user/cameras` |
| Video device | empty | `/dev/dri` for Intel or AMD hardware video decode |
| PUID / PGID | 99 / 100 | Unraid's `nobody:users` |

**Keep the database off FUSE.** The database is SQLite, which runs best
without Unraid's `/mnt/user` FUSE layer. On Unraid 6.12 or later, set the
`appdata` share to Exclusive access (this bypasses FUSE), or point Data at your
pool's own path, `/mnt/<your pool>/appdata/ambersight/data`. Do not use
`/mnt/cache/...` unless a pool named "cache" exists: otherwise Docker creates
the folder in RAM and everything is lost at reboot.

**Recordings on the array.** Map a share to `/recordings`, start the
container, then add `/recordings` in Settings > Recording > Drives & folders.
For a new, empty folder **Keep this folder private** is on by default, which
shuts out other accounts; turn it off if you want to browse the recordings
over the network share.

**Ownership.** The container runs AmberSight as `PUID:PGID`. On the first
start (or after changing them) it gives `/config` and `/data` and everything
in them to that user. It does not change `/recordings`: Unraid shares already
belong to `nobody:users` (99:100), so the defaults can write there.

**Time zone.** Unraid passes the server's time zone (`TZ`) to every container;
schedules and quiet hours use it. To use another, add a variable `TZ`, for
example `America/Chicago`.

**Network.** Bridge (the default) works for cameras added by address.
Camera discovery (ONVIF) and battery Reolink cameras need **Network Type:
Host**; with host networking AmberSight uses port 8090 on the server
directly.

**NVIDIA card.** For the AI on an NVIDIA card, change the repository to
`ghcr.io/huskerminion/ambersight:cuda`, install the Nvidia Driver plugin, add
`--runtime=nvidia` to **Extra Parameters**, and add a variable
`NVIDIA_VISIBLE_DEVICES` set to `all` (or the card's GPU ID).

## Updates

The built-in updater only tells you about new versions in a container. Unraid
shows an update for the container when a new image is out: apply it from the
Docker page. Settings and recordings stay in the mapped folders.

More about the image: [docs/docker.md](../docs/docker.md).
