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
# the drivers below and /dev/dri passed in; libmfx-gen1.2 is Intel's Quick
# Sync runtime (Gen12+), without which QSV fails and Intel falls back to VAAPI.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg ca-certificates tzdata tini mesa-va-drivers fonts-dejavu-core \
 && if [ "$(dpkg --print-architecture)" = amd64 ]; then \
      apt-get install -y --no-install-recommends intel-media-va-driver libmfx-gen1.2; fi \
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
# Image labels (OCI). The release workflow (.github/workflows/docker.yml)
# passes the real version, commit and date; a local build gets these defaults.
# Last, so a new version does not rebuild the layers above.
ARG VERSION=dev
ARG REVISION=unknown
ARG CREATED=unknown
LABEL org.opencontainers.image.title="AmberSight" \
      org.opencontainers.image.description="Self-hosted camera recording and AI detection" \
      org.opencontainers.image.source="https://github.com/HuskerMinion/AmberSight" \
      org.opencontainers.image.url="https://github.com/HuskerMinion/AmberSight" \
      org.opencontainers.image.licenses="PolyForm-Noncommercial-1.0.0" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION" \
      org.opencontainers.image.created="$CREATED"
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/ambersight-entrypoint"]
