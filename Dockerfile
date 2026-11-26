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
# curl: alerts (webhook, Telegram, ntfy, Pushover, Home Assistant) and the AI
# server option send with it; the build stops if either program is missing.
# Debian's ffmpeg decodes H.264 and H.265. Hardware decode (VAAPI/QSV) needs
# the drivers below and /dev/dri passed in; libmfx-gen1.2 is Intel's Quick
# Sync runtime (Gen12+), without which QSV fails and Intel falls back to VAAPI.
# amd64 also gets Mesa's Vulkan driver for AMD (RADV) for the AI on AMD
# graphics (src/ai_amd.rs; the add-on is downloaded at run time like outside
# Docker, x86_64 only). dpkg leaves out Mesa's other Vulkan drivers and
# layers, so only RADV (about 14 MB) is added. pci.ids names the AMD chip
# (src/gpu.rs) when libdrm's list does not, as on the computer itself.
COPY docker/radv-only.cfg /tmp/radv-only.cfg
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg curl ca-certificates tzdata tini mesa-va-drivers fonts-dejavu-core \
 && if [ "$(dpkg --print-architecture)" = amd64 ]; then \
      mv /tmp/radv-only.cfg /etc/dpkg/dpkg.cfg.d/ambersight-radv-only \
      && apt-get install -y --no-install-recommends intel-media-va-driver libmfx-gen1.2 mesa-vulkan-drivers pci.ids \
      && ls /usr/share/vulkan/icd.d/radeon_icd*.json /usr/lib/x86_64-linux-gnu/libvulkan_radeon.so >/dev/null \
      && ! ls /usr/lib/x86_64-linux-gnu/libvulkan_lvp.so 2>/dev/null; \
    else rm /tmp/radv-only.cfg; fi \
 && curl --version >/dev/null && ffmpeg -version >/dev/null \
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
