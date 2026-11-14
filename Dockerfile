# AmberSight in a container.
#
#   docker build -t ambersight .
#   docker compose -f docker/compose.yaml up -d
#
# See docs/docker.md for volumes, networking, hardware decode and updates.

# ---- build ----
FROM rust:1-trixie AS build
WORKDIR /src
COPY . .
RUN RUSTFLAGS='--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo=~/.cargo' \
    cargo build --release --locked \
 && install -m 755 target/release/ambersight /ambersight

# ---- run ----
FROM debian:trixie-slim
# ffmpeg: recording, clips and cameras the built-in reader can't take.
# Debian's ffmpeg decodes H.264 and H.265. Hardware decode (VAAPI/QSV) needs
# the drivers below and /dev/dri passed in.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg ca-certificates tzdata tini mesa-va-drivers fonts-dejavu-core \
 && if [ "$(dpkg --print-architecture)" = amd64 ]; then \
      apt-get install -y --no-install-recommends intel-media-va-driver; fi \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 1000 --user-group --home-dir /data --no-create-home ambersight \
 && mkdir -p /config /data \
 && chown ambersight:ambersight /config /data
COPY --from=build /ambersight /usr/local/bin/ambersight
COPY docker/entrypoint.sh /usr/local/bin/ambersight-entrypoint
COPY LICENSE THIRD-PARTY-NOTICES.txt /usr/share/doc/ambersight/

# Starts as root only to fix folder ownership, then runs as PUID/PGID
# (default 1000); see docker/entrypoint.sh.
ENV HOME=/data PUID=1000 PGID=1000
VOLUME ["/config", "/data"]
# Web interface and API (HTTP, or HTTPS once turned on in Settings).
EXPOSE 8090
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/ambersight-entrypoint"]
