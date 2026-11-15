#!/bin/sh
# Start AmberSight with its settings in /config and everything else in /data.
#
# Started as root, it makes /config and /data belong to the AmberSight user
# (PUID/PGID, default 1000), finds the groups of the video devices in
# /dev/dri, and runs AmberSight as that user. PUID=0 runs it as root (for
# rootless Docker or Podman, where root in the container is your own user).
#
# /recordings (and any folder listed in RECORDINGS_DIRS, separated by colons;
# a symlink is left alone) is handed over only when it is new: empty (or holding only
# lost+found) and owned by root, which is what Docker makes of a host folder
# that did not exist. A share that already has recordings is never re-owned.
set -eu
CONFIG="${AMBERSIGHT_CONFIG:-/config/config.toml}"
PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
for v in "$PUID" "$PGID"; do
    case "$v" in
        '' | *[!0-9]*) echo "PUID and PGID must be numbers, not '$PUID' and '$PGID'" >&2; exit 1 ;;
    esac
done

if [ "$(id -u)" = 0 ] && [ "$PUID" != 0 ]; then
    for d in /config /data; do
        # A folder that belongs to someone else (a first start, a new PUID, or
        # files copied in as root) is handed over with everything in it once;
        # after that the owner already matches and nothing is walked.
        if [ "$(stat -c %u:%g "$d")" != "$PUID:$PGID" ]; then
            chown -R "$PUID:$PGID" "$d" || echo "warning: could not give $d to $PUID:$PGID; AmberSight may not be able to write there" >&2
        fi
    done
    printf '%s\n' /recordings "$(printf '%s' "${RECORDINGS_DIRS:-}" | tr ':' '\n')" | while IFS= read -r d; do
        # A trailing slash would hide a symlink from the -L test.
        while [ "${d%/}" != "$d" ] && [ "$d" != / ]; do d="${d%/}"; done
        case "$d" in /?*) ;; *) continue ;; esac
        [ -d "$d" ] && [ ! -L "$d" ] || continue
        [ "$(stat -c %u "$d")" = 0 ] || continue
        if [ -n "$(find "$d" -mindepth 1 -maxdepth 1 ! -name lost+found 2>/dev/null | head -n 1)" ]; then
            continue
        fi
        chown "$PUID:$PGID" "$d" || echo "warning: could not give $d to $PUID:$PGID; AmberSight may not be able to record there" >&2
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
