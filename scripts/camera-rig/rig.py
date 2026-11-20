#!/usr/bin/env python3
"""Camera compatibility rig for AmberSight. See README.md.

  python3 rig.py [--binary PATH] [--wait 120] [--keep] [--only REGEX]
  python3 rig.py --cleanup      stop anything a crashed run left behind

Runs on a Linux machine with Docker. Starts the simulated cameras (Docker, host network), a
temporary AmberSight on 127.0.0.1:18190 with a camera for every profile and
case, checks each one through the API and ffprobe, prints a PASS/FAIL table,
writes ~/camrig/results/<time>/, and stops everything it started.
"""
import argparse
import base64
import glob
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
HOME = os.path.expanduser("~/camrig")
STATE = os.path.join(HOME, "run", "state.json")
MTX_IMAGE = "bluenviron/mediamtx:1.21.3@sha256:59e43552f3be43695b70c19424dc2a42fcec18075f88d91561a24ac5d70a586c"
HELPER_IMAGE = "camrig-helper:1"
ONVIF_GO = os.path.join(HOME, "dl", "onvif-go-v1.3.0", "onvif-server")
AS_PORT = 18190
SERVERS = {
    "mtx-basic": 18554, "mtx-digest": 18555, "mtx-udp": 18556,
    "gst-basic": 18564, "gst-digest": 18565, "stub": 18570,
    "off": 18096,  # nothing listens here: a camera that is switched off
}
MTX_API = {"mtx-basic": 18997, "mtx-digest": 18998, "mtx-udp": 18999}
ONVIF_PORTS = [18001, 18002, 18003]
ONVIF_GO_PORT = 18010
NO_ONVIF_PORT = 18099  # nothing listens: stops AmberSight probing other ports on this machine
PUBLISHER = ("publisher", "pub-secret-9")
# Fake Reolink web APIs for the camera controls (reolink_sim.py): a good one, one that changes nothing.
REOLINK_PORTS = {"reolink-ctl": 18680, "reolink-noop": 18681}
REOLINK_LOGIN = ("admin", "sim-secret-3")
# A fake Reolink on Baichuan TCP (bc_sim.py, controls over port 9000): (its port, its /sim/state port).
BC_SIM = ("baichuan-ctl", 18682, 18683)
ALL_PORTS = list(SERVERS.values()) + list(MTX_API.values()) + ONVIF_PORTS + [ONVIF_GO_PORT, AS_PORT] + list(range(18600, 18606)) + list(REOLINK_PORTS.values()) + [BC_SIM[1], BC_SIM[2]]
DEADLINE_S = 15 * 60

STARTED = time.time()
state = {"containers": [], "pids": [], "tmp": None}


def say(*a):
    print(f"[{time.time() - STARTED:6.1f}s]", *a, flush=True)


def save_state():
    os.makedirs(os.path.dirname(STATE), exist_ok=True)
    json.dump(state, open(STATE, "w"))


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


# ---------------------------------------------------------------- clean-up

def cleanup(keep_tmp=False):
    st = state
    if os.path.exists(STATE):
        try:
            old = json.load(open(STATE))
            for k in ("containers", "pids"):
                st[k] = sorted(set(st.get(k, []) + old.get(k, [])), key=str)
            st["tmp"] = st.get("tmp") or old.get("tmp")
        except (OSError, ValueError):
            pass
    # Each recorded process leads its own process group (start_new_session).
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for pid in st.get("pids", []):
            try:
                if os.getpgid(pid) == pid:
                    os.killpg(pid, sig)
            except (ProcessLookupError, PermissionError):
                pass
        if sig == signal.SIGTERM:
            time.sleep(2)
    for name in st.get("containers", []):
        if not name.startswith("camrig-"):
            continue  # never anything that is not ours
        run(["docker", "rm", "-f", name])
    if st.get("tmp") and not keep_tmp and st["tmp"].startswith(tempfile.gettempdir() + "/camrig-"):
        shutil.rmtree(st["tmp"], ignore_errors=True)
    st.update(containers=[], pids=[], tmp=None)
    try:
        os.remove(STATE)
    except OSError:
        pass


def spawn(cmd, log_path, env=None):
    f = open(log_path, "ab")
    p = subprocess.Popen(cmd, stdout=f, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, env=env, start_new_session=True)
    state["pids"].append(p.pid)
    save_state()
    return p


def container(name, args, cmd=()):
    assert name.startswith("camrig-")
    run(["docker", "rm", "-f", name])
    r = run(["docker", "run", "-d", "--rm", "--name", name, "--network", "host"] + args + list(cmd))
    if r.returncode != 0:
        raise SystemExit(f"could not start {name}: {r.stderr.strip()}")
    state["containers"].append(name)
    save_state()


# ---------------------------------------------------------------- servers

def mtx_config(kind, logins):
    """MediaMTX settings (JSON is valid YAML)."""
    port = SERVERS[kind]
    idx = list(MTX_API).index(kind)
    users = [
        {"user": PUBLISHER[0], "pass": PUBLISHER[1], "ips": [], "permissions": [{"action": "publish"}, {"action": "read"}]},
        {"user": "any", "pass": "", "ips": ["127.0.0.1", "::1"], "permissions": [{"action": "api"}]},
        {"user": "any", "pass": "", "ips": [], "permissions": [{"action": "read", "path": "stream0"}]},
    ]
    for key in ("viewer", "special_mtx"):
        users.append({"user": logins[key]["user"], "pass": logins[key]["password"], "ips": [], "permissions": [{"action": "read"}]})
    upstream = f"rtsp://{PUBLISHER[0]}:{PUBLISHER[1]}@127.0.0.1:{SERVERS['mtx-basic']}"
    if kind == "mtx-basic":
        paths = {
            "cam/realmonitor": {"source": f"{upstream}/h264-720-aac/main", "sourceOnDemand": True},
            "stream0": {"source": f"{upstream}/h264-720-pcma/main", "sourceOnDemand": True},
            "all_others": {},
        }
        transports = ["udp", "tcp"]
        methods = ["basic"]
    else:
        # Everything is pulled from the Basic server when someone reads it.
        paths = {"~^(.+)$": {"source": upstream + "/$G1", "sourceOnDemand": True}}
        transports = ["udp"] if kind == "mtx-udp" else ["udp", "tcp"]
        methods = ["digest"] if kind == "mtx-digest" else ["basic"]
    return {
        "logLevel": "info",
        "authMethod": "internal",
        "authInternalUsers": users,
        "api": True, "apiAddress": f"127.0.0.1:{MTX_API[kind]}",
        "rtsp": True, "rtspTransports": transports, "rtspAuthMethods": methods,
        "rtspAddress": f":{port}", "rtpAddress": f":{18600 + 2 * idx}", "rtcpAddress": f":{18601 + 2 * idx}",
        "rtmp": False, "hls": False, "webrtc": False, "srt": False, "moq": False, "playback": False,
        "metrics": False, "pprof": False,
        "paths": paths,
    }


def feeder_cmd(p, which):
    """ffmpeg command publishing one stream of a profile to MediaMTX."""
    main = which == "main"
    size = p["size"] if main else p["sub"]
    codec = p["codec"] if main else p.get("sub_codec", "h264")
    fps = p["fps"] if main else min(p["fps"], 10)
    gop = p["gop"] if main else fps * 2
    w, h = size.split("x")
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin",
           "-re", "-f", "lavfi", "-i", f"testsrc2=size={size}:rate={fps}"]
    audio = p.get("audio", "none") if main else "none"
    if audio != "none":
        rate = p.get("audio_rate", 8000)
        cmd += ["-re", "-f", "lavfi", "-i", f"sine=frequency=880:sample_rate={rate}:beep_factor=4"]
    br = max(300, int(int(w) * int(h) * fps / 9000))
    if codec == "h264":
        bf = p.get("bframes", 0) if main else 0
        cmd += ["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", str(gop), "-keyint_min", str(gop),
                "-sc_threshold", "0", "-bf", str(bf), "-b:v", f"{br}k", "-maxrate", f"{br}k", "-bufsize", f"{br * 2}k"]
        if bf == 0:
            cmd += ["-tune", "zerolatency"]
    else:
        cmd += ["-c:v", "libx265", "-preset", "ultrafast", "-tune", "zerolatency", "-pix_fmt", "yuv420p",
                "-x265-params", f"keyint={gop}:min-keyint={gop}:scenecut=0:bframes=0:open-gop={1 if p.get('open_gop') and main else 0}:log-level=error", "-b:v", f"{br}k"]
    if audio == "aac":
        cmd += ["-c:a", "aac", "-ac", "1", "-b:a", "48k"]
    elif audio == "pcmu":
        cmd += ["-c:a", "pcm_mulaw", "-ac", "1"]
    elif audio == "pcma":
        cmd += ["-c:a", "pcm_alaw", "-ac", "1"]
    cmd += ["-f", "rtsp", "-rtsp_transport", "tcp",
            f"rtsp://{PUBLISHER[0]}:{PUBLISHER[1]}@127.0.0.1:{SERVERS['mtx-basic']}/{p['name']}/{which}"]
    return cmd


def prepare_media(d):
    os.makedirs(d, exist_ok=True)
    jobs = {
        "v.h264": ["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=10", "-t", "30", "-c:v", "libx264", "-preset", "veryfast",
                   "-profile:v", "main", "-bf", "0", "-g", "10", "-x264-params", "aud=1:repeat-headers=1", "-pix_fmt", "yuv420p", "-f", "h264"],
        "a.adts": ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000", "-t", "30", "-ac", "1", "-c:a", "aac", "-b:a", "32k", "-f", "adts"],
        "a.ulaw": ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=8000", "-t", "30", "-ac", "1", "-f", "mulaw"],
    }
    for name, args in jobs.items():
        out = os.path.join(d, name)
        if not os.path.exists(out):
            r = run(["ffmpeg", "-v", "error", "-y"] + args + [out])
            if r.returncode:
                raise SystemExit(f"could not make {name}: {r.stderr}")


def onvif_cams(logins, host_ip):
    v = logins["viewer"]
    mtx = f"rtsp://{{host}}:{SERVERS['mtx-basic']}"
    return [
        {"port": ONVIF_PORTS[0], "name": "RigCam-1", "user": v["user"], "password": v["password"], "ptz": True, "events": True,
         "profiles": [{"token": "main", "w": 1280, "h": 720, "enc": "H264", "uri": f"{mtx}/h264-720-aac/main"},
                      {"token": "sub", "w": 640, "h": 360, "enc": "H264", "uri": f"{mtx}/h264-720-aac/sub"}]},
        {"port": ONVIF_PORTS[1], "name": "RigCam-2", "user": v["user"], "password": v["password"], "ptz": False, "events": True,
         "profiles": [{"token": "p0", "w": 640, "h": 360, "enc": "H264", "uri": f"{mtx}/h265-1080/sub"},
                      {"token": "p1", "w": 1920, "h": 1080, "enc": "H265", "uri": f"{mtx}/h265-1080/main"}]},
        {"port": ONVIF_PORTS[2], "name": "RigCam-3", "user": v["user"], "password": v["password"], "ptz": False, "events": False,
         "profiles": [{"token": "only", "w": 1280, "h": 720, "enc": "H264", "uri": f"{mtx}/h264-720-nosub/main"}]},
    ]


def lan_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))  # no packet is sent
        return s.getsockname()[0]
    finally:
        s.close()


def port_busy(port):
    for proto in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
        s = socket.socket(socket.AF_INET, proto)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)  # TIME_WAIT from a last run is fine
        try:
            s.bind(("0.0.0.0", port))
        except OSError:
            return True
        finally:
            s.close()
    return False


# ---------------------------------------------------------------- AmberSight

def tq(s):
    return json.dumps(s)  # a JSON string is a valid TOML basic string here


def build_cameras(cfg):
    logins = cfg["logins"]
    cams = []
    for p in cfg.get("profile", []):
        cams.append({
            "name": p["name"], "server": "mtx-basic", "path": f"{p['name']}/main",
            "sub": f"{p['name']}/sub" if p.get("sub") else None, "creds": "fields", "login": "viewer",
            "audio": p.get("audio", "none") != "none", "codec": p["codec"], "expect": "play", "profile": p,
        })
    for c in cfg.get("case", []):
        d = dict(c)
        d.setdefault("expect", "play")
        d.setdefault("creds", "none")
        d.setdefault("sub", None)
        d.setdefault("audio", False)
        cams.append(d)
    for c in cams:
        c.setdefault("check", [])
        c.setdefault("expect_ingest", "native" if c.get("ingest", "auto") != "ffmpeg" else "ffmpeg")
    return cams


def cam_url(c, logins, path):
    port = SERVERS[c["server"]]
    auth = ""
    if c["creds"] == "url":
        lg = logins[c["login"]]
        auth = urllib.parse.quote(lg["user"], safe="") + ":" + urllib.parse.quote(lg["password"], safe="") + "@"
    return f"rtsp://{auth}127.0.0.1:{port}/{path}"


def as_config(cams, logins, data_dir, admin_pw):
    out = [
        "[server]", 'bind = "127.0.0.1"', f"port = {AS_PORT}", "tls = false", "rtmp_port = 0",
        "", "[[server.users]]", 'username = "rigadmin"', f"password = {tq(admin_pw)}",
        "", "[storage]", f"data_dir = {tq(data_dir)}", "segment_seconds = 5", "retention_days = 1", "max_mb = 3000",
        "min_free_percent = 1",
        "", "[update]", "check = false",
        "", "[restream]", "enabled = false",
        "",
    ]
    for c in cams:
        out += ["[[cameras]]", f"name = {tq(c['name'])}", f"url = {tq(cam_url(c, logins, c['path']))}"]
        if c.get("sub"):
            out.append(f"sub_url = {tq(cam_url(c, logins, c['sub']))}")
        elif c.get("sub_raw"):
            out.append(f"sub_url = {tq(c['sub_raw'])}")
        if c["creds"] == "fields":
            lg = logins[c["login"]]
            out += [f"username = {tq(lg['user'])}", f"password = {tq(lg['password'])}"]
        out += ["fps = 10", "scale = 640", "record = true", "motion = true", "enabled = true", "direct = true",
                f"audio = {'true' if c['audio'] else 'false'}", f"onvif_port = {c.get('onvif_port', NO_ONVIF_PORT)}"]
        if c.get("ingest"):
            out.append(f"ingest = {tq(c['ingest'])}")
        if c.get("overlay"):
            out.append("overlay = true")
        if c.get("trigger"):
            out.append(f"trigger = {tq(c['trigger'])}")
        out.append("")
    # Camera controls: cameras whose web API is a fake Reolink (no video: their stream port is off).
    for name, port in REOLINK_PORTS.items():
        out += ["[[cameras]]", f"name = {tq(name)}", "url = " + tq("rtsp://127.0.0.1:%d/h264Preview_01_main" % SERVERS["off"]),
                f"username = {tq(REOLINK_LOGIN[0])}", f"password = {tq(REOLINK_LOGIN[1])}", "record = false", "motion = false",
                f"onvif_port = {NO_ONVIF_PORT}", "", "[cameras.controls]", 'scheme = "http"', f"http_port = {port}", ""]
    # Camera controls over Reolink port 9000 (bc_sim.py; no video: its stream port is off).
    out += ["[[cameras]]", f"name = {tq(BC_SIM[0])}", "url = " + tq("rtsp://127.0.0.1:%d/Preview_01_sub" % SERVERS["off"]),
            f"username = {tq(REOLINK_LOGIN[0])}", f"password = {tq(REOLINK_LOGIN[1])}", "record = false", "motion = false",
            f"onvif_port = {NO_ONVIF_PORT}", f"baichuan_port = {BC_SIM[1]}", "", "[cameras.controls]", 'scheme = "baichuan"', ""]
    # More settings for a run by hand, e.g. CAMRIG_EXTRA_TOML=$'[transcode]\nencoder = "software"'.
    if os.environ.get("CAMRIG_EXTRA_TOML"):
        out.append(os.environ["CAMRIG_EXTRA_TOML"])
    return "\n".join(out)


class Api:
    def __init__(self, pw):
        self.auth = "Basic " + base64.b64encode(f"rigadmin:{pw}".encode()).decode()

    def req(self, method, path, body=None, timeout=20, raw=False):
        data = json.dumps(body).encode() if body is not None else None
        r = urllib.request.Request(f"http://127.0.0.1:{AS_PORT}{path}", data=data, method=method)
        r.add_header("Authorization", self.auth)
        if data is not None:
            r.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(r, timeout=timeout) as resp:
                b = resp.read()
                return resp.status, (b if raw else json.loads(b or b"null")), resp.headers
        except urllib.error.HTTPError as e:
            b = e.read()
            try:
                return e.code, json.loads(b), e.headers
            except ValueError:
                return e.code, b.decode("utf-8", "replace"), e.headers
        except (urllib.error.URLError, TimeoutError, ConnectionError, socket.timeout) as e:
            return 0, str(e), {}

    def get(self, path, **kw):
        return self.req("GET", path, **kw)

    def stream_bytes(self, path, seconds, limit=8_000_000):
        r = urllib.request.Request(f"http://127.0.0.1:{AS_PORT}{path}")
        r.add_header("Authorization", self.auth)
        buf = b""
        try:
            with urllib.request.urlopen(r, timeout=10) as resp:
                end = time.time() + seconds
                while time.time() < end and len(buf) < limit:
                    chunk = resp.read1(65536) if hasattr(resp, "read1") else resp.read(65536)
                    if not chunk:
                        break
                    buf += chunk
        except Exception as e:  # noqa: BLE001
            return buf, str(e)
        return buf, None


# ---------------------------------------------------------------- checks

def ffprobe_json(path, *extra):
    r = run(["ffprobe", "-v", "error", "-of", "json", *extra, path], timeout=60)
    try:
        return json.loads(r.stdout or "{}"), r.stderr.strip()
    except ValueError:
        return {}, r.stderr.strip()


def stream_ends(path):
    """Per stream: (codec, first pts, last pts + duration) in seconds."""
    meta, err = ffprobe_json(path, "-show_streams")
    out = {}
    for s in meta.get("streams", []):
        idx = s["index"]
        pk, _ = ffprobe_json(path, "-select_streams", str(idx), "-show_entries", "packet=pts_time,duration_time")
        pts = [(float(p["pts_time"]), float(p.get("duration_time") or 0)) for p in pk.get("packets", []) if p.get("pts_time") not in (None, "N/A")]
        if pts:
            out[s["codec_type"]] = (s["codec_name"], min(p for p, _ in pts), max(p + d for p, d in pts), len(pts), max(d for _, d in pts))
    return out, err


def segments_of(data_dir, name):
    """Complete recording segments of a camera (the newest one may still be written)."""
    slug = "".join(ch.lower() if ch.isascii() and ch.isalnum() else "_" for ch in name)  # as config::slug
    files = glob.glob(os.path.join(data_dir, "cameras", slug, "segments", "*.mp4"))
    files.sort(key=lambda f: (os.path.getmtime(f), f))
    now = time.time()
    # (The newest is left out too: ffmpeg writes a fragment only at a keyframe,
    # so a segment it is writing can stay at its first bytes for seconds.)
    return [f for f in files[:-1] if now - os.path.getmtime(f) > 3]


def check_segments(data_dir, c, results):
    segs = segments_of(data_dir, c["name"])
    main = segs  # (record_sub is off: every segment is the main stream)
    if len(main) < 2:
        results.append(("recording", False, f"{len(main)} complete segments found"))
        return
    worst, notes, bad, allowed = 0.0, [], [], 0.1
    for s in main[-3:]:
        ends, err = stream_ends(s)
        v = ends.get("video")
        a = ends.get("audio")
        if not v:
            bad.append(f"{os.path.basename(s)}: no video ({err[:120]})")
            continue
        dur = v[2] - v[1]
        if dur <= 0 or dur > 60:
            bad.append(f"{os.path.basename(s)}: video {dur:.1f} s long")
        dec = run(["ffmpeg", "-v", "error", "-nostdin", "-i", s, "-f", "null", "-"], timeout=120)
        if dec.stderr.strip():
            bad.append(f"{os.path.basename(s)}: decode errors: {dec.stderr.strip().splitlines()[0][:140]}")
        if c["audio"]:
            if not a:
                bad.append(f"{os.path.basename(s)}: no audio")
            else:
                # Within 0.1 s, or within one audio frame when frames are longer
                # (G.711 at 8 kHz is stored as FLAC in 1024-sample, 128 ms frames).
                diff = abs(a[2] - v[2])
                allowed = max(allowed, 0.1, a[4] + 0.005)
                worst = max(worst, diff)
                notes.append(f"{a[0]} {diff:.3f}s")
        if c.get("codec") and v[0] != {"h264": "h264", "h265": "hevc"}[c["codec"]]:
            bad.append(f"video is {v[0]}")
    ok = not bad and worst <= allowed
    detail = f"{len(main)} segs; " + ("; ".join(bad) if bad else "ok")
    if c["audio"]:
        detail += f"; worst A/V end gap {worst:.3f}s (allowed {allowed:.3f}s)"
    results.append(("recording", ok, detail))


def mtx_readers(server, path):
    port = MTX_API.get(server)
    if not port:
        return None
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/v3/paths/get/{urllib.parse.quote(path.split('?')[0])}", timeout=5) as r:
            return len(json.loads(r.read()).get("readers", []))
    except Exception:  # noqa: BLE001
        return 0


def readable(err, plain=None):
    """Whether a camera's error can be shown to a person: the plain form when
    there is one, and the raw one must not hold a password or debug dumps."""
    if not err:
        return False, "no last_error"
    if "\n" in err.strip() or "conn: " in err:
        return False, f"raw last_error has debug lines: {err.strip().splitlines()[0][:120]} ... (plain: {plain})"
    if re.search(r"rtsp://[^\s/]*:[^\s/]*@", err) or "password=" in err.lower() and "q1w2e3" in err:
        return False, "last_error shows a password"
    if len(err) > 400:
        return False, "last_error too long"
    return True, err


def jsonl(path):
    out = []
    try:
        for line in open(path):
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    except OSError:
        pass
    return out


# ---------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", help="AmberSight program to test (default: the latest release, see fetch.sh release)")
    ap.add_argument("--profiles", default=os.path.join(HERE, "profiles.toml"))
    ap.add_argument("--wait", type=int, default=120, help="seconds the cameras run before the checks")
    ap.add_argument("--only", help="regex: only cameras whose name matches")
    ap.add_argument("--keep", action="store_true", help="keep the temporary folder")
    ap.add_argument("--cleanup", action="store_true", help="stop what an earlier run left behind and exit")
    a = ap.parse_args()

    if a.cleanup:
        cleanup()
        print("cleaned up")
        return 0

    binary = a.binary
    if not binary:
        cands = sorted(glob.glob(os.path.join(HOME, "bin", "ambersight-*", "ambersight")), key=os.path.getmtime)
        if not cands:
            raise SystemExit("no --binary and no release in ~/camrig/bin (run fetch.sh release)")
        binary = cands[-1]
    binary = os.path.abspath(binary)

    cfg = tomllib.load(open(a.profiles, "rb"))
    logins = cfg["logins"]
    cams = build_cameras(cfg)
    if a.only:
        cams = [c for c in cams if re.search(a.only, c["name"])]

    if os.path.exists(STATE):
        say("an earlier run left things behind; cleaning up first")
        cleanup()
    busy = [p for p in ALL_PORTS if port_busy(p)]
    if busy:
        raise SystemExit(f"ports in use: {busy}")
    if run(["docker", "image", "inspect", HELPER_IMAGE]).returncode:
        say("building the helper image")
        r = run(["docker", "build", "-q", "-t", HELPER_IMAGE, os.path.join(HERE, "image")], timeout=900)
        if r.returncode:
            raise SystemExit(r.stderr)

    tmp = tempfile.mkdtemp(prefix="camrig-")
    state["tmp"] = tmp
    save_state()
    stamp = time.strftime("%Y%m%d-%H%M%S")
    res_dir = os.path.join(HOME, "results", stamp)
    os.makedirs(res_dir, exist_ok=True)
    logs = os.path.join(tmp, "logs")
    os.makedirs(logs)
    hard_stop = STARTED + DEADLINE_S - 90

    def on_signal(*_):
        cleanup(a.keep)
        sys.exit(130)
    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)

    try:
        return run_rig(a, binary, cfg, logins, cams, tmp, logs, res_dir, hard_stop)
    finally:
        for name in list(state["containers"]):
            r = run(["docker", "logs", name])
            open(os.path.join(res_dir, f"{name}.log"), "w").write((r.stdout + r.stderr)[-200_000:])
        for f in glob.glob(os.path.join(logs, "*")):
            try:
                shutil.copy(f, res_dir)
            except OSError:
                pass
        cleanup(a.keep)
        say(f"cleaned up; results in {res_dir}")


def sim_state(port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/sim/state", timeout=3) as r:
            return json.loads(r.read())
    except Exception:  # noqa: BLE001
        return {}


def check_controls(api, listed, add):
    """Camera controls against the fake Reolink APIs: what is found, a timed light given
    back, the siren limited and stopping, a camera that changes nothing reported."""
    good, noop = listed.get("reolink-ctl"), listed.get("reolink-noop")
    if not good or not noop:
        add("controls", "listed", False, "the fake Reolink cameras are not in /api/cameras")
        return
    port = REOLINK_PORTS["reolink-ctl"]
    path = f"/api/cameras/{good['id']}/controls"
    s, v, _ = api.get(path, timeout=30)
    ok = s == 200 and isinstance(v, dict) and v.get("supported") and v.get("light") and v.get("siren") and v.get("ir")
    add("controls", "abilities", bool(ok), f"status {s} {str(v)[:200]}")
    s, v, _ = api.req("POST", path, {"action": "light_on", "seconds": 3, "brightness": 40}, timeout=30)
    on = (sim_state(port).get("light") or {})
    add("controls", "light on", s == 200 and on.get("state") == 1 and on.get("bright") == 40, f"status {s}; camera {on}")
    end = time.time() + 15
    while time.time() < end:
        back = sim_state(port).get("light") or {}
        if back.get("state") == 0 and back.get("bright") == 85 and back.get("mode") == 1:
            break
        time.sleep(0.5)
    add("controls", "light given back", back.get("state") == 0 and back.get("bright") == 85 and back.get("mode") == 1, f"camera {back}")
    s, v, _ = api.req("POST", path, {"action": "siren", "seconds": 2}, timeout=30)
    st = sim_state(port)
    plays = [e for e in st.get("log", []) if e.get("cmd") == "AudioAlarmPlay" and (e.get("param") or {}).get("alarm_mode") == "times"]
    add("controls", "siren", s == 200 and bool(plays), f"status {s}; {len(plays)} plays")
    s2, _, _ = api.req("POST", path, {"action": "siren", "seconds": 2}, timeout=30)
    add("controls", "siren limited", s2 == 429, f"second siren: status {s2}")
    end = time.time() + 15
    stops = []
    while time.time() < end:
        stops = [e for e in sim_state(port).get("log", []) if e.get("cmd") == "AudioAlarmPlay" and (e.get("param") or {}).get("manual_switch") == 0 and (e.get("param") or {}).get("alarm_mode") == "manu"]
        if stops:
            break
        time.sleep(0.5)
    add("controls", "siren stopped on time", bool(stops), f"{len(stops)} stops")
    s, v, _ = api.req("POST", f"/api/cameras/{noop['id']}/controls", {"action": "light_on", "seconds": 3}, timeout=30)
    add("controls", "no-op camera reported", s == 502 and "nothing changed" in str(v), f"status {s} {str(v)[:160]}")
    s, a, _ = api.get("/api/audit?limit=50")
    n = sum(1 for e in (a.get("entries", []) if isinstance(a, dict) else []) if e.get("action") == "camera control")
    add("controls", "audit", n >= 4, f"{n} camera control entries")


def check_bc_controls(api, listed, add):
    """Camera controls over Reolink port 9000 against bc_sim.py: what is found, the infrared
    lights changed and read back, the siren played as rounds and stopped when its time is up."""
    cam = listed.get(BC_SIM[0])
    if not cam:
        add("controls-9000", "listed", False, "the fake Baichuan camera is not in /api/cameras")
        return
    port = BC_SIM[2]
    path = f"/api/cameras/{cam['id']}/controls"
    s, v, _ = api.get(path, timeout=40)
    ok = s == 200 and isinstance(v, dict) and v.get("via") == "port9000" and v.get("siren") and v.get("ir") and v.get("status_led") and not v.get("light")
    add("controls-9000", "abilities", bool(ok), f"status {s} {str(v)[:200]}")
    s, v, _ = api.req("POST", path, {"action": "ir", "state": "Off"}, timeout=30)
    st = sim_state(port)
    add("controls-9000", "infrared off", s == 200 and st.get("ir") == "close" and (v.get("state") or {}).get("ir") == "Off", f"status {s}; camera {st.get('ir')}")
    s, v, _ = api.req("POST", path, {"action": "siren", "seconds": 2}, timeout=30)
    plays = [e for e in sim_state(port).get("log", []) if e.get("id") == 263 and "<playMode>0</playMode>" in e.get("xml", "")]
    add("controls-9000", "siren", s == 200 and bool(plays), f"status {s}; {len(plays)} plays")
    end = time.time() + 15
    stops = []
    while time.time() < end:
        stops = [e for e in sim_state(port).get("log", []) if e.get("id") == 263 and "<onOff>0</onOff>" in e.get("xml", "")]
        if stops:
            break
        time.sleep(0.5)
    add("controls-9000", "siren stopped on time", bool(stops), f"{len(stops)} stops")
    st = sim_state(port)
    # A controls-only session signs out about 60 s after its last exchange (it
    # frees one of the camera's ~2 session slots), so a later action signs in
    # again: what must hold is one session at a time and no refused sign-in.
    add("controls-9000", "one session at a time", st.get("max_open") == 1 and st.get("logins", 0) >= 1 and st.get("refused") == 0,
        f"{st.get('logins')} sign-ins, at most {st.get('max_open')} at once, {st.get('refused')} refused")


def run_rig(a, binary, cfg, logins, cams, tmp, logs, res_dir, hard_stop):
    host_ip = lan_ip()
    media = os.path.join(HOME, "media")
    prepare_media(media)

    # MediaMTX x3
    for kind in MTX_API:
        conf = os.path.join(tmp, f"{kind}.yml")
        json.dump(mtx_config(kind, logins), open(conf, "w"), indent=1)
        container(f"camrig-{kind}", ["-v", f"{conf}:/mediamtx.yml:ro"], [MTX_IMAGE])
    # GStreamer RTSP server, the stub, the ONVIF cameras
    container("camrig-gst", ["-v", f"{HERE}:/rig:ro"], [HELPER_IMAGE, "python3", "/rig/gst_server.py", "--port", str(SERVERS["gst-basic"])]
              + [x for k in ("viewer", "special") for x in ("--login", f"{logins[k]['user']}:{logins[k]['password']}")])
    container("camrig-stub", ["-v", f"{HERE}:/rig:ro", "-v", f"{media}:/media:ro", "-v", f"{logs}:/out"],
              [HELPER_IMAGE, "python3", "/rig/rtsp_stub.py", "--port", str(SERVERS["stub"]), "--media", "/media",
               "--log", "/out/stub.jsonl", "--password", logins["stub"]["password"]])
    ocams = onvif_cams(logins, host_ip)
    json.dump(ocams, open(os.path.join(tmp, "onvif-cams.json"), "w"))
    container("camrig-onvif", ["-v", f"{HERE}:/rig:ro", "-v", f"{tmp}:/cfg:ro", "-v", f"{logs}:/out"],
              [HELPER_IMAGE, "python3", "/rig/onvif_sim.py", "--cams", "/cfg/onvif-cams.json", "--log", "/out/onvif.jsonl"])
    if os.path.exists(ONVIF_GO):
        container("camrig-onvifgo", ["-v", f"{os.path.dirname(ONVIF_GO)}:/opt/og:ro"],
                  [HELPER_IMAGE, "/opt/og/onvif-server", "-host", "0.0.0.0", "-port", str(ONVIF_GO_PORT),
                   "-username", logins["viewer"]["user"], "-password", logins["viewer"]["password"], "-profiles", "3"])
    end = time.time() + 20
    while time.time() < end:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{MTX_API['mtx-basic']}/v3/paths/list", timeout=2).read()
            break
        except Exception:  # noqa: BLE001
            time.sleep(0.5)
    time.sleep(1)
    up = set(run(["docker", "ps", "--format", "{{.Names}}"]).stdout.split())
    gone = [n for n in state["containers"] if n not in up]
    if gone:
        raise SystemExit(f"servers stopped at start: {gone} (run one by hand without --rm to see why)")

    # Feeders
    for p in cfg.get("profile", []):
        for which in ("main", "sub"):
            if which == "sub" and not p.get("sub"):
                continue
            spawn(feeder_cmd(p, which), os.path.join(logs, f"feed-{p['name']}-{which}.log"))
    say("servers and feeders started; waiting for streams")
    want = [f"{p['name']}/{w}" for p in cfg.get("profile", []) for w in ("main", "sub") if w == "main" or p.get("sub")]
    end = time.time() + 40
    ready = set()
    while time.time() < end:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{MTX_API['mtx-basic']}/v3/paths/list", timeout=3) as r:
                items = json.loads(r.read()).get("items", [])
            ready = {i["name"] for i in items if i.get("ready")}
        except Exception:  # noqa: BLE001
            pass
        if all(w in ready for w in want):
            break
        time.sleep(1)
    missing = [w for w in want if w not in ready]
    if missing:
        say(f"WARNING streams not ready: {missing}")

    # Fake Reolink web APIs (camera controls)
    for name, port in REOLINK_PORTS.items():
        spawn([sys.executable, os.path.join(HERE, "reolink_sim.py"), "--port", str(port), "--user", REOLINK_LOGIN[0], "--password", REOLINK_LOGIN[1]]
              + (["--noop"] if name == "reolink-noop" else []), os.path.join(logs, f"{name}.log"))
    spawn([sys.executable, os.path.join(HERE, "bc_sim.py"), "--port", str(BC_SIM[1]), "--state-port", str(BC_SIM[2]),
           "--user", REOLINK_LOGIN[0], "--password", REOLINK_LOGIN[1]], os.path.join(logs, f"{BC_SIM[0]}.log"))

    # AmberSight
    admin_pw = "rig-" + base64.b32encode(os.urandom(10)).decode().lower()
    data_dir = os.path.join(tmp, "data")
    os.makedirs(data_dir)
    conf = os.path.join(tmp, "config.toml")
    open(conf, "w").write(as_config(cams, logins, data_dir, admin_pw))
    # (For looking at a run by hand: AmberSight replaces the password in its config with a hash.)
    with open(os.open(os.path.join(tmp, "admin-password.txt"), os.O_WRONLY | os.O_CREAT, 0o600), "w") as f:
        f.write(admin_pw)
    os.chmod(conf, 0o600)
    env = dict(os.environ, RUST_LOG=os.environ.get("CAMRIG_RUST_LOG", "info"))
    as_log = os.path.join(logs, "ambersight.log")
    proc = spawn([binary, "run", "--config", conf], as_log, env=env)
    api = Api(admin_pw)
    end = time.time() + 60
    while time.time() < end:
        s, body, _ = api.get("/api/status", timeout=3)
        if s == 200:
            version = body.get("version", "?")
            break
        if proc.poll() is not None:
            raise SystemExit("AmberSight exited at start; see " + as_log)
        time.sleep(1)
    else:
        raise SystemExit("AmberSight did not answer in 60 s")
    t0 = time.time()
    say(f"AmberSight {version} up with {len(cams)} cameras")

    rows = []  # (camera, check, ok, detail)

    def add(cam, check, ok, detail=""):
        rows.append((cam, check, ok, detail))

    # ONVIF discovery (while the cameras connect)
    names = [c["name"] for c in ocams]
    r = run([binary, "discover", "--seconds", "4", "--user", logins["viewer"]["user"], "--password", logins["viewer"]["password"]], timeout=90)
    out = r.stdout
    open(os.path.join(logs, "discover.txt"), "w").write(out + r.stderr)
    found = [n for n in names if n in out]
    add("onvif", "cli discover finds all", len(found) == len(names), f"found {found}")
    sub_ok = "sub_url" in out and "h264-720-aac/sub" in out
    add("onvif", "cli discover main+sub", sub_ok and "h264-720-aac/main" in out, "toml block has sub_url" if sub_ok else out[-300:])
    s, body, _ = api.get("/api/discover?seconds=4", timeout=30)
    devs = body.get("devices", body) if isinstance(body, dict) else body
    txt = json.dumps(devs)
    add("onvif", "api discover finds all", s == 200 and all(n in txt for n in names), f"status {s}; {sum(n in txt for n in names)}/{len(names)}")
    for oc in ocams:
        s, body, _ = api.req("POST", "/api/discover/inspect", {"xaddr": f"http://{host_ip}:{oc['port']}/onvif/device_service",
                                                                "username": oc["user"], "password": oc["password"]}, timeout=40)
        sug = body.get("suggested", {}) if isinstance(body, dict) else {}
        main_p = max(oc["profiles"], key=lambda p: p["w"] * p["h"])
        sub_p = min(oc["profiles"], key=lambda p: p["w"] * p["h"])
        want_main = main_p["uri"].split("}", 1)[1]
        ok = s == 200 and want_main in str(sug.get("url", ""))
        if len(oc["profiles"]) > 1:
            ok = ok and sub_p["uri"].split("}", 1)[1] in str(sug.get("sub_url", ""))
        else:
            ok = ok and not sug.get("sub_url")
        add("onvif", f"inspect {oc['name']}", ok, f"status {s}; url={sug.get('url')} sub={sug.get('sub_url')}" if s == 200 else f"status {s}: {str(body)[:200]}")
    if os.path.exists(ONVIF_GO):
        s, body, _ = api.req("POST", "/api/discover/inspect", {"xaddr": f"http://127.0.0.1:{ONVIF_GO_PORT}/onvif/device_service",
                                                                "username": logins["viewer"]["user"], "password": logins["viewer"]["password"]}, timeout=40)
        n = len(body.get("profiles", [])) if isinstance(body, dict) else 0
        add("onvif", "inspect onvif-go", s == 200 and n >= 3, f"status {s}; {n} profiles" if s == 200 else f"status {s}: {str(body)[:200]}")

    # Let the cameras run.
    wait_until = t0 + a.wait
    while time.time() < wait_until:
        time.sleep(2)
        if proc.poll() is not None:
            add("ambersight", "still running", False, f"exited with {proc.returncode}")
            break
    say("checking cameras")

    s, body, _ = api.get("/api/cameras")
    listed = {c["name"]: c for c in body.get("cameras", [])} if s == 200 else {}
    s, health, _ = api.get("/api/health", timeout=30)
    hcams = {c["name"]: c for c in (health.get("cameras", []) if isinstance(health, dict) else [])}
    stub_log_before = jsonl(os.path.join(logs, "stub.jsonl"))

    # The wrong-password camera: count the tries, then fix the password.
    fix = {}
    for c in cams:
        if "authcount" in c["check"]:
            tries = [e for e in stub_log_before if e.get("path") == "authcount" and e.get("auth") == "bad"]
            ran = time.time() - t0
            fix[c["name"]] = (len(tries), ran)
            lc = listed.get(c["name"], {})
            lg = logins["stub"]
            st, b, _ = api.req("PUT", f"/api/cameras/{lc.get('id')}", {"username": lg["user"], "password": lg["password"]})
            fix[c["name"]] += (st, str(b)[:200], time.time())

    for c in cams:
        if time.time() > hard_stop:
            add(c["name"], "time", False, "rig ran out of time")
            continue
        name = c["name"]
        lc = listed.get(name)
        hc = hcams.get(name, {})
        if lc is None:
            add(name, "listed", False, "not in /api/cameras")
            continue
        cid = lc["id"]
        ingest = lc.get("ingest")
        err = hc.get("last_error")
        plain = hc.get("last_error_plain")
        if c["expect"] == "fail":
            add(name, "fails", not lc.get("online") or not lc.get("frames"), f"online={lc.get('online')} frames={lc.get('frames')}")
            ok, why = readable(err, plain)
            if ok and c.get("expect_error"):
                ok = re.search(c["expect_error"], err) is not None
                why = err if ok else f"unexpected wording: {err}"
            add(name, "readable error", ok, why)
            add(name, "ingest", ingest == c["expect_ingest"], f"{ingest} (want {c['expect_ingest']})")
            continue
        if "authcount" in c["check"]:
            n, ran, st, b, fixed_at = fix[name]
            add(name, "wrong pw backoff", 1 <= n <= max(3, int(ran / 20)), f"{n} refused tries in {ran:.0f} s")
            okerr, why = readable(err, plain)
            add(name, "wrong pw error", okerr and bool(re.search("(?i)password", err or "")), why)
            add(name, "fix applied", st == 200, f"PUT {st} {b if st != 200 else ''}")
            on = False
            # (Other cameras were checked since the fix: look at least once.)
            while True:
                s2, b2, _ = api.get("/api/cameras")
                x = next((y for y in b2.get("cameras", []) if y["name"] == name), {}) if s2 == 200 else {}
                if x.get("online") and x.get("frames"):
                    on = True
                    break
                if time.time() > fixed_at + 45:
                    break
                time.sleep(2)
            s2, segs_after, _ = api.get(f"/api/recordings?camera={urllib.parse.quote(name)}&from={int(fixed_at * 1000)}")
            n_after = segs_after.get("segments", 0) if isinstance(segs_after, dict) else 0
            add(name, "plays after fix", on and n_after > 0,
                f"online, {n_after} segments recorded since the fix" if on else f"not online 45 s after the fix ({x})")
            continue
        add(name, "online", bool(lc.get("online")) and bool(lc.get("frames")), f"frames={lc.get('frames')} err={err}")
        add(name, "ingest", ingest == c["expect_ingest"], f"{ingest} (want {c['expect_ingest']})")
        seg_rows = []
        check_segments(os.path.join(tmp, "data"), c, seg_rows)
        for ck, ok, d in seg_rows:
            add(name, ck, ok, d)
        # Live view
        st, img, hdr = api.get(f"/api/snapshot/{cid}", raw=True)
        add(name, "snapshot", st == 200 and isinstance(img, bytes) and img[:2] == b"\xff\xd8", f"status {st}, {len(img) if isinstance(img, bytes) else 0} bytes")
        if ingest and ingest.startswith("native"):
            # A camera that drops its stream (the fault cameras) has no live
            # stream for the ~2 s reconnect and until its next keyframe, by
            # design (viewers wait): look again for a few seconds, as a page does.
            for _try in range(6):
                st, info, _ = api.get(f"/api/live/{cid}/info")
                main_i = info.get("main") if isinstance(info, dict) else None
                sub_i = info.get("sub") if isinstance(info, dict) else None
                if main_i:
                    break
                time.sleep(1)
            buf, e = api.stream_bytes(f"/api/live/{cid}/stream?q=main", 3)
            lp = os.path.join(tmp, f"live-{cid}.mp4")
            open(lp, "wb").write(buf)
            ends, perr = stream_ends(lp) if buf else ({}, "")
            ok = bool(main_i) and "video" in ends and (not c["audio"] or "audio" in ends)
            add(name, "live stream", ok, f"{main_i.get('mime') if main_i else None}; {len(buf)} bytes; streams {sorted(ends)} {perr[:80]}")
            if c.get("sub"):
                rd = mtx_readers(c["server"], c["sub"])
                add(name, "sub used for motion", bool(sub_i) and (rd is None or rd >= 1),
                    f"live sub={'yes' if sub_i else 'no'}; readers on sub path={rd if rd is not None else 'n/a'}")
        elif c.get("sub") and ingest and ingest.startswith("ffmpeg"):
            # The ffmpeg pipeline decodes the sub stream for motion too.
            rd = mtx_readers(c["server"], c["sub"])
            dec = hc.get("decode") or {}
            add(name, "sub used for motion", dec.get("stream") == "sub" and (rd is None or rd >= 1),
                f"decoding {dec.get('stream')}; readers on sub path={rd if rd is not None else 'n/a'}")
        if "drop" in c["check"]:
            rc = hc.get("reconnects", 0)
            okerr, why = readable(err, plain)
            add(name, "reconnects after drop", rc >= 2 and bool(lc.get("online")), f"reconnects={rc}")
            add(name, "drop error readable", okerr, why)
        if "tsjump" in c["check"]:
            s2, rec, _ = api.get(f"/api/recordings?camera={urllib.parse.quote(name)}")
            spans = rec.get("spans", []) if isinstance(rec, dict) else []
            now_ms = time.time() * 1000
            last_end = max((sp[1] for sp in spans), default=0)
            first = min((sp[0] for sp in spans), default=0)
            ok = spans and last_end <= now_ms + 60_000 and first >= t0 * 1000 - 120_000
            add(name, f"recording times sane ({c['path']})", bool(ok), f"{len(spans)} spans; last end {(last_end - now_ms) / 1000:+.0f} s from now; first {(first / 1000 - t0):+.0f} s from start")
            # The jump itself was seen and taken out (forward one hour, or back 30 s).
            want = r"clock jumped \+3[56]\d\d\.\d s" if c["path"] == "tsjump" else r"clock jumped -(29|30|31)\.\d s"
            seen = re.findall(want, open(as_log, errors="replace").read())
            add(name, f"clock jump taken out ({c['path']})", bool(seen), f"{len(seen)} matching log lines")
        if "onvif-ptz" in c["check"]:
            st, caps, _ = api.get(f"/api/cameras/{cid}/ptz/caps", timeout=30)
            add(name, "ptz caps", st == 200 and isinstance(caps, dict) and bool(caps.get("ptz", caps.get("pan_tilt"))), f"status {st} {str(caps)[:150]}")
            st, b, _ = api.req("POST", f"/api/cameras/{cid}/ptz", {"action": "move", "pan": 0.5, "tilt": 0, "zoom": 0, "timeout_ms": 500}, timeout=30)
            add(name, "ptz move", st == 200, f"status {st} {str(b)[:150]}")
            st, b, _ = api.get(f"/api/cameras/{cid}/ptz/presets", timeout=30)
            add(name, "ptz presets", st == 200, f"status {st} {str(b)[:150]}")
            if c.get("onvif_port") in ONVIF_PORTS:
                ol = jsonl(os.path.join(logs, "onvif.jsonl"))
                moves = [e for e in ol if e.get("op") == "ContinuousMove" and e.get("ok")]
                add(name, "ptz reached camera", bool(moves), f"{len(moves)} ContinuousMove calls")
        if "onvif-events" in c["check"]:
            ol = jsonl(os.path.join(logs, "onvif.jsonl"))
            pulls = [e for e in ol if e.get("op") == "PullMessages" and e.get("ok")]
            add(name, "event subscription", bool(pulls), f"{len(pulls)} PullMessages")
            st, ev, _ = api.get(f"/api/events?camera={urllib.parse.quote(name)}&limit=50&all=1")
            evs = ev.get("events", ev) if isinstance(ev, (dict, list)) else []
            add(name, "camera motion makes events", st == 200 and len(evs) >= 1, f"{len(evs)} events (trigger = camera)")

    if time.time() < hard_stop:
        check_controls(api, listed, add)
        check_bc_controls(api, listed, add)

    # AmberSight's own log: panics, and fallbacks no case expected.
    log_text = open(as_log, errors="replace").read()
    panics = [ln for ln in log_text.splitlines() if "panicked" in ln]
    add("ambersight", "no panics", not panics, panics[0][:200] if panics else "")
    add("ambersight", "still running", proc.poll() is None, "" if proc.poll() is None else f"exit {proc.returncode}")

    # Report
    width = max(len(r[0]) for r in rows) + 1
    cw = max(len(r[1]) for r in rows) + 1
    lines = [f"AmberSight camera rig  {time.strftime('%Y-%m-%d %H:%M')}  binary {binary}  ({version})", ""]
    lines.append(f"{'camera'.ljust(width)} {'check'.ljust(cw)} result  detail")
    for cam, ck, ok, d in rows:
        lines.append(f"{cam.ljust(width)} {ck.ljust(cw)} {'PASS' if ok else 'FAIL'}    {str(d)[:220]}")
    fails = [r for r in rows if not r[2]]
    lines += ["", f"{len(rows) - len(fails)} PASS, {len(fails)} FAIL, {len(cams)} cameras, {time.time() - STARTED:.0f} s"]
    report = "\n".join(lines)
    print(report)
    open(os.path.join(res_dir, "summary.txt"), "w").write(report + "\n")
    json.dump([{"camera": r[0], "check": r[1], "ok": r[2], "detail": r[3]} for r in rows], open(os.path.join(res_dir, "results.json"), "w"), indent=1)
    s, lg, _ = api.get("/api/logs?level=warn&limit=2000")
    if s == 200:
        open(os.path.join(res_dir, "ambersight-warnings.txt"), "w").write("\n".join(map(str, lg.get("lines", []))))
    if a.keep:
        say(f"kept {tmp}")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
