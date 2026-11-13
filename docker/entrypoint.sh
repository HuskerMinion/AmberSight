#!/bin/sh
# Start AmberSight with its settings in /config and everything else in /data.
#
# Started as root, it makes /config and /data belong to the AmberSight user
# (PUID/PGID, default 1000), finds the groups of the video devices in
# /dev/dri, and runs AmberSight as that user. PUID=0 runs it as root (for
# rootless Docker or Podman, where root in the container is your own user).
set -eu
CONFIG="${AMBERSIGHT_CONFIG:-/config/config.toml}"
PUID="${PUID:-1000}"
PGID="${PGID:-1000}"

if [ "$(id -u)" = 0 ] && [ "$PUID" != 0 ]; then
    for d in /config /data; do
        # A folder that belongs to someone else (a first start, a new PUID, or
        # files copied in as root) is handed over with everything in it once;
        # after that the owner already matches and nothing is walked.
        if [ "$(stat -c %u:%g "$d")" != "$PUID:$PGID" ]; then
            chown -R "$PUID:$PGID" "$d" || echo "warning: could not give $d to $PUID:$PGID; AmberSight may not be able to write there" >&2
        fi
    done
    groups=""
    for dev in /dev/dri/renderD* /dev/dri/card*; do
        [ -e "$dev" ] || continue
        g="$(stat -c %g "$dev")"
        [ "$g" = 0 ] && continue
        case ",$groups," in *",$g,"*) ;; *) groups="${groups:+$groups,}$g" ;; esac
    done
    if [ -n "$groups" ]; then
        exec setpriv --reuid="$PUID" --regid="$PGID" --groups="$groups" "$0" "$@"
    fi
    exec setpriv --reuid="$PUID" --regid="$PGID" --clear-groups "$0" "$@"
fi

if [ ! -f "$CONFIG" ]; then
    # Written whole or not at all: a stop half-way leaves no config without
    # data_dir = /data.
    (umask 077; ambersight init "$CONFIG.new")
    sed -i 's|^data_dir = .*|data_dir = "/data"|' "$CONFIG.new"
    mv "$CONFIG.new" "$CONFIG"
fi
exec ambersight run --config "$CONFIG" "$@"
