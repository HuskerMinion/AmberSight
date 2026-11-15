#!/usr/bin/env python3
"""GStreamer RTSP server (gst-rtsp-server) for the camera rig: a second RTSP
implementation next to MediaMTX. Test patterns encoded live.

  gst_server.py --port 18564 --login viewer:simple1 [--login 'admin:p@ss#w:rd/ x1']

One server with Basic auth on --port, one with Digest auth on --port + 1.
Mounts: /h264aac (H.264 1280x720 + AAC), /h265 (H.265 1280x720),
/h264pcmu (H.264 640x360 + G.711 mu-law), /sub (H.264 640x360).
"""
import argparse
import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstRtspServer", "1.0")
gi.require_version("GstRtsp", "1.0")
from gi.repository import Gst, GstRtsp, GstRtspServer, GLib  # noqa: E402

V = "videotestsrc is-live=true pattern=ball ! video/x-raw,width={w},height={h},framerate={fps}/1 ! videoconvert ! "
X264 = "x264enc tune=zerolatency speed-preset=ultrafast key-int-max={gop} bitrate=1500 ! rtph264pay name=pay0 pt=96 config-interval=-1 "
X265 = "x265enc tune=zerolatency speed-preset=ultrafast key-int-max={gop} bitrate=1500 option-string=open-gop=0 ! rtph265pay name=pay0 pt=96 config-interval=-1 "
AAC = "audiotestsrc is-live=true wave=sine freq=440 ! audio/x-raw,rate=16000,channels=1 ! audioconvert ! avenc_aac ! rtpmp4gpay name=pay1 pt=97 "
PCMU = "audiotestsrc is-live=true wave=sine freq=440 ! audio/x-raw,rate=8000,channels=1 ! audioconvert ! mulawenc ! rtppcmupay name=pay1 pt=0 "

MOUNTS = {
    "/h264aac": "( " + V.format(w=1280, h=720, fps=15) + X264.format(gop=30) + AAC + ")",
    "/h265": "( " + V.format(w=1280, h=720, fps=15) + X265.format(gop=30) + ")",
    "/h264pcmu": "( " + V.format(w=640, h=360, fps=10) + X264.format(gop=20) + PCMU + ")",
    "/sub": "( " + V.format(w=640, h=360, fps=10) + X264.format(gop=20) + ")",
}


def server(port, logins, method):
    s = GstRtspServer.RTSPServer()
    s.set_service(str(port))
    auth = GstRtspServer.RTSPAuth()
    auth.set_supported_methods(method)
    token = GstRtspServer.RTSPToken()
    token.set_string("media.factory.role", "viewer")
    for user, password in logins:
        if method == GstRtsp.RTSPAuthMethod.BASIC:
            auth.add_basic(GstRtspServer.RTSPAuth.make_basic(user, password), token)
        else:
            auth.add_digest(user, password, token)
    s.set_auth(auth)
    mounts = s.get_mount_points()
    for path, launch in MOUNTS.items():
        f = GstRtspServer.RTSPMediaFactory()
        f.set_launch(launch)
        f.set_shared(True)
        f.add_role_from_structure(Gst.Structure.new_from_string(
            "viewer, media.factory.access=(boolean)true, media.factory.construct=(boolean)true"))
        mounts.add_factory(path, f)
    s.attach(None)
    print(f"gst-rtsp-server on {port} ({method.value_nick})", flush=True)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=18564)
    p.add_argument("--login", action="append", default=[], help="user:password (the first colon splits)")
    a = p.parse_args()
    Gst.init(None)
    logins = [tuple(x.split(":", 1)) for x in (a.login or ["viewer:simple1"])]
    server(a.port, logins, GstRtsp.RTSPAuthMethod.BASIC)
    server(a.port + 1, logins, GstRtsp.RTSPAuthMethod.DIGEST)
    GLib.MainLoop().run()


if __name__ == "__main__":
    main()
