# Helper image for the camera rig: GStreamer's RTSP server (the second RTSP
# implementation) plus Python 3 for the rig's own stub servers. Only Ubuntu
# archive packages; nothing else is downloaded.
FROM ubuntu:26.04
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      python3 python3-gi gir1.2-gst-rtsp-server-1.0 gir1.2-gstreamer-1.0 \
      gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
      gstreamer1.0-plugins-ugly gstreamer1.0-libav \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /rig
