"""Live video end-to-end test of AmberSight's HomeKit accessory with aiohomekit + an ffmpeg SRTP receiver.

usage: python hk_stream.py <web base url> <admin user> <admin password> <mode standalone|bridged> <hap base port> <workdir>
"""
import asyncio, base64, hashlib, hmac, json, os, socket, struct, subprocess, sys, time, urllib.request

from zeroconf.asyncio import AsyncZeroconf, AsyncServiceBrowser
from aiohomekit import Controller
from pyhap import tlv
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

BASE, ADMIN, APW, MODE, PORT, WORK = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5]), sys.argv[6]
results = []
SETUP, SELECTED, STATUS = 17, 15, 16
# The receiver's ports (HK_RTP_BASE, +2, +4).
_B = int(os.environ.get("HK_RTP_BASE", "18270"))
VPORT, APORT, RTCP_FROM = _B, _B + 2, _B + 4
# Optional: test over an IPv6 link-local address of this computer ("fe80::...%<interface index>").
LINKLOCAL = os.environ.get("HK_LINKLOCAL", "")
# The camera to stream (default "Live"), and what to check in its picture:
# HK_CHECK=text (ffmpeg draws the burned-in text; changing it ends the stream).
CAMERA = os.environ.get("HK_CAMERA", "Live")
CHECK = os.environ.get("HK_CHECK", "")
SCOPE = int(LINKLOCAL.split("%")[1]) if "%" in LINKLOCAL else 0


def ok(name, cond, detail=""):
    results.append((name, bool(cond)))
    print(("PASS " if cond else "FAIL ") + name + (f" -- {detail}" if detail else ""), flush=True)


def api(method, path, body=None):
    req = urllib.request.Request(BASE + path, method=method, data=None if body is None else json.dumps(body).encode())
    req.add_header("Authorization", "Basic " + base64.b64encode(f"{ADMIN}:{APW}".encode()).decode())
    if body is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read() or b"null")


def setup_tlv(session, ip, key, salt):
    v6 = b"\x01" if ":" in ip else b"\x00"
    addr = tlv.encode(b"\x01", v6, b"\x02", ip.encode(), b"\x03", struct.pack("<H", VPORT), b"\x04", struct.pack("<H", APORT))
    srtp = tlv.encode(b"\x01", b"\x00", b"\x02", key, b"\x03", salt)
    return base64.b64encode(tlv.encode(b"\x01", session, b"\x03", addr, b"\x04", srtp, b"\x05", srtp)).decode()


def selected_tlv(session, command, w=1280, h=720, fps=30, kbps=1000):
    ctl = tlv.encode(b"\x01", session, b"\x02", bytes([command]))
    if command not in (1, 4):
        return base64.b64encode(tlv.encode(b"\x01", ctl)).decode()
    params = tlv.encode(b"\x01", b"\x02", b"\x02", b"\x02", b"\x03", b"\x00")
    attrs = tlv.encode(b"\x01", struct.pack("<H", w), b"\x02", struct.pack("<H", h), b"\x03", bytes([fps]))
    rtp = tlv.encode(b"\x01", bytes([99]), b"\x02", struct.pack("<I", 1234), b"\x03", struct.pack("<H", kbps), b"\x04", struct.pack("<f", 0.5), b"\x05", struct.pack("<H", 1378))
    video = tlv.encode(b"\x01", b"\x00", b"\x02", params, b"\x03", attrs, b"\x04", rtp)
    ap = tlv.encode(b"\x01", b"\x01", b"\x02", b"\x00", b"\x03", b"\x02", b"\x04", bytes([20]))
    ar = tlv.encode(b"\x01", bytes([110]), b"\x02", struct.pack("<I", 4321), b"\x03", struct.pack("<H", 24), b"\x04", struct.pack("<f", 5.0))
    audio = tlv.encode(b"\x01", b"\x03", b"\x02", ap, b"\x03", ar, b"\x04", b"\x00")
    return base64.b64encode(tlv.encode(b"\x01", ctl, b"\x02", video, b"\x03", audio)).decode()


def write_sdp(path, ip, key, salt):
    inline = base64.b64encode(key + salt).decode()
    with open(path, "w") as f:
        fam = "IP6" if ":" in ip else "IP4"
        f.write(f"v=0\no=- 0 0 IN {fam} {ip}\ns=hk\nc=IN {fam} {ip}\nt=0 0\n"
                f"m=video {VPORT} RTP/SAVP 99\na=rtpmap:99 H264/90000\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{inline}\na=rtcp-mux\n")
        # (The texted test camera has no sound: the receiver would wait for it.)
        if not CHECK:
            f.write(f"m=audio {APORT} RTP/SAVP 110\na=rtpmap:110 opus/48000/2\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{inline}\na=rtcp-mux\n")


def srtcp_auth_key(key, salt):
    # RFC 3711 key derivation, label 4 (SRTCP authentication), rate 0.
    x = bytearray(salt)
    x[7] ^= 4
    enc = Cipher(algorithms.AES(key), modes.ECB()).encryptor()
    return (enc.update(bytes(x) + b"\0\0") + enc.update(bytes(x) + b"\0\1"))[:20]


def rtcp_keepalive(ip, port, stop_at, key, salt, forged=False):
    if ":" in ip:
        s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
        s.bind((ip, RTCP_FROM, 0, SCOPE))
        dest = (ip, port, 0, SCOPE)
    else:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.bind((ip, RTCP_FROM))
        dest = (ip, port)
    # An empty receiver report as SRTCP (unencrypted, E = 0, but
    # authenticated: AmberSight counts only RTCP under the session's key).
    rr = bytes([0x80, 0xC9, 0x00, 0x01]) + struct.pack(">I", 1234)
    ak = srtcp_auth_key(key, salt)
    index = 0
    while time.time() < stop_at[0]:
        signed = rr + struct.pack(">I", index)
        packet = signed + hmac.new(ak, signed, hashlib.sha1).digest()[:10]
        if forged:
            # What someone else on the network can send from the controller's
            # address: plain RTCP, and the same SRTCP packet again and again
            # (it counts once).
            s.sendto(rr, dest)
            s.sendto(packet, dest)
        else:
            s.sendto(packet, dest)
            index += 1
        time.sleep(1)
    s.close()


async def find_live(controller):
    deadline = time.time() + 30
    while time.time() < deadline:
        async for d in controller.async_discover():
            p, name = d.description.port, d.description.name
            if MODE == "standalone" and p > PORT and p < PORT + 64 and name.startswith(CAMERA + " "):
                return d
            if MODE == "bridged" and p == PORT:
                return d
        await asyncio.sleep(1)
    return None


async def put(pairing, aid, iid, value):
    r = await pairing.put_characteristics([(aid, iid, value)])
    return r.get((aid, iid), {}).get("status", 0)


async def main():
    hk = api("GET", "/api/homekit")
    ok(f"[{MODE}] pairing closed at first: no setup code", "setup_code" not in hk and not any(a.get("setup_uri") for a in hk["accessories"]), str([(a["kind"], a.get("camera"), a["port"]) for a in hk["accessories"]]))
    if MODE == "standalone":
        ok("standalone: one accessory per shown camera, no bridge", sorted(a.get("camera") for a in hk["accessories"]) == ["Bars", "Live", "Texted"] and all(a["kind"] == "camera" for a in hk["accessories"]))
    else:
        ok("bridged: just the bridge", [a["kind"] for a in hk["accessories"]] == ["bridge"])
    # Open the pairing window (Settings, Pair a device) for the camera, or the bridge.
    hk = api("POST", "/api/homekit/pairing", {"action": "open", "camera": CAMERA} if MODE == "standalone" else {"action": "open"})
    code = hk.get("setup_code")
    shown = [a.get("camera") or a["kind"] for a in hk["accessories"] if a.get("setup_uri")]
    ok(f"[{MODE}] pairing opened: setup code shown, QR code for that accessory only", code and shown == ([CAMERA] if MODE == "standalone" else ["bridge"]), str(shown))
    azc = AsyncZeroconf()
    AsyncServiceBrowser(azc.zeroconf, ["_hap._tcp.local.", "_hap._udp.local."], handlers=[lambda **kw: None])
    controller = Controller(async_zeroconf_instance=azc)
    await controller.async_start()
    try:
        disc = await find_live(controller)
        ok("discovered", disc is not None, f"{disc.description.name} ci={int(disc.description.category)} port={disc.description.port}" if disc else "")
        if not disc:
            return
        ok("category", int(disc.description.category) == (17 if MODE == "standalone" else 2))
        acc_ip = disc.description.address
        # The receiver's address (HK_RECV): by default the address AmberSight is reached
        # at, which is this computer's when both run here.
        ip = os.environ.get("HK_RECV", acc_ip)
        print(f"  accessory at {acc_ip}, receiver {ip}")
        if LINKLOCAL:
            # Connect over this link-local address instead (HK_LINKLOCAL="fe80::...%<index>").
            import dataclasses
            from aiohomekit.controller.ip.discovery import IpDiscovery
            disc = IpDiscovery(controller, dataclasses.replace(disc.description, address=LINKLOCAL, addresses=[LINKLOCAL]))
            ip = acc_ip = LINKLOCAL.split("%")[0]
            print(f"  over link-local {LINKLOCAL}")
        finish = await disc.async_start_pairing("s")
        pairing = await finish(code)
        accs = await pairing.list_accessories_and_characteristics()
        aid = None
        for a in accs:
            names = [c.get("value") for s in a["services"] for c in s["characteristics"] if c["type"].upper().startswith("00000023")]
            if CAMERA in names:
                aid = a["aid"]
        ok(f"the {CAMERA} camera is listed", aid is not None, f"aid {aid}, accessories {[a['aid'] for a in accs]}")
        st = await pairing.get_characteristics([(aid, STATUS)])
        ok("StreamingStatus available", base64.b64decode(st[(aid, STATUS)]["value"]) == b"\x01\x01\x00", st[(aid, STATUS)])

        # Refused: video to an address that is not ours.
        s0 = os.urandom(16)
        stt = await put(pairing, aid, SETUP, setup_tlv(s0, "192.0.2.55", os.urandom(16), os.urandom(14)))
        ok("SetupEndpoints to another address refused", stt == -70410, f"status {stt}")

        # A stream to our receiver.
        key, salt = os.urandom(16), os.urandom(14)
        sess = os.urandom(16)
        stt = await put(pairing, aid, SETUP, setup_tlv(sess, ip, key, salt))
        ok("SetupEndpoints accepted", stt == 0, f"status {stt}")
        r = await pairing.get_characteristics([(aid, SETUP)])
        ans = tlv.decode(base64.b64decode(r[(aid, SETUP)]["value"]))
        addr = tlv.decode(ans[b"\x03"])
        our_vport = struct.unpack("<H", addr[b"\x03"])[0]
        ok("answer: success, our address and ports, keys echoed", ans[b"\x02"] == b"\x00" and addr[b"\x02"].decode() in (acc_ip, ip) and tlv.decode(ans[b"\x04"])[b"\x02"] == key, f"{addr[b'\x02'].decode()}:{our_vport}")
        sdp = os.path.join(WORK, "recv.sdp")
        write_sdp(sdp, ip, key, salt)
        out = os.path.join(WORK, f"recv-{MODE}.mkv")
        if os.path.exists(out):
            os.remove(out)
        recv = subprocess.Popen(["timeout", "60", "ffmpeg", "-hide_banner", "-loglevel", "warning", "-protocol_whitelist", "file,udp,rtp,srtp,crypto", "-i", sdp, "-t", "8", "-c", "copy", "-y", out], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        stop_at = [time.time() + 60]
        loop = asyncio.get_running_loop()
        ka = loop.run_in_executor(None, rtcp_keepalive, ip, our_vport, stop_at, key, salt)
        await asyncio.sleep(0.5)
        t0 = time.time()
        stt = 1
        for _ in range(20):
            stt = await put(pairing, aid, SELECTED, selected_tlv(sess, 1))
            if stt != -70403:
                break
            await asyncio.sleep(1)  # the encoders' start-up test
        ok("start 1280x720 30 fps 1000 kbps", stt == 0, f"status {stt}")
        await asyncio.sleep(2)
        hk = api("GET", "/api/homekit")
        ok("one live stream running", hk["live_streams"] == 1, hk["live_streams"])
        await asyncio.sleep(3)
        ps = subprocess.run(["ps", "-eo", "pcpu,args"], capture_output=True, text=True).stdout.splitlines()
        senders = [l for l in ps if "srtp_out_suite" in l and "ffmpeg" in l]
        cpu = [l.split()[0] for l in senders]
        print(f"  sender ffmpeg CPU %: {cpu}")
        if CHECK == "text":
            ok("burned-in text drawn into the live view", any("drawtext=" in l for l in senders))
        try:
            err = await asyncio.wait_for(loop.run_in_executor(None, recv.communicate), 60)
        except asyncio.TimeoutError:
            recv.kill()
            err = (b"", b"timeout")
        probe = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_name,width,height,sample_rate,nb_read_frames", "-count_frames", "-of", "json", out], capture_output=True, text=True)
        try:
            streams = json.loads(probe.stdout).get("streams", [])
        except Exception:
            streams = []
        v = [s for s in streams if s.get("codec_name") == "h264"]
        a = [s for s in streams if s.get("codec_name") == "opus"]
        ok("received decodable H.264 at 1280x720", v and v[0].get("width") == 1280 and v[0].get("height") == 720 and int(v[0].get("nb_read_frames", 0)) > 30, f"{v} first frame after {time.time() - t0:.1f}s; receiver: {err[1][-300:]!r}")
        if not CHECK:
            ok("received Opus audio", bool(a), str(a))
        # Reconfigure is accepted and changes nothing (the stream keeps running).
        stt = await put(pairing, aid, SELECTED, selected_tlv(sess, 4, 640, 360, 15, 300))
        await asyncio.sleep(1)
        ok("reconfigure accepted, stream kept", stt == 0 and api("GET", "/api/homekit")["live_streams"] == 1, f"status {stt}")
        # Past the RTCP timeout (30 s) with the receiver's SRTCP coming: still running.
        while time.time() - t0 < 36:
            await asyncio.sleep(1)
            await pairing.get_characteristics([(aid, STATUS)])
        ok("kept running past 30 s by the receiver's SRTCP", api("GET", "/api/homekit")["live_streams"] == 1, f"after {time.time() - t0:.0f}s")
        # End.
        stt = await put(pairing, aid, SELECTED, selected_tlv(sess, 0))
        await asyncio.sleep(2)
        hk = api("GET", "/api/homekit")
        ok("end stops it", stt == 0 and hk["live_streams"] == 0, f"status {stt}, {hk['live_streams']} running")
        stop_at[0] = 0
        await ka

        # No RTCP from the controller (only plain RTCP and one SRTCP packet
        # sent again and again): stops by itself after ~30 s. (The SRTCP one
        # counts once, so the stream knows its reports can be checked and
        # never falls back to taking the plain ones.)
        sess2 = os.urandom(16)
        await put(pairing, aid, SETUP, setup_tlv(sess2, ip, key, salt))
        r = await pairing.get_characteristics([(aid, SETUP)])
        port2 = struct.unpack("<H", tlv.decode(tlv.decode(base64.b64decode(r[(aid, SETUP)]["value"]))[b"\x03"])[b"\x03"])[0]
        stop2 = [time.time() + 45]
        fk = loop.run_in_executor(None, rtcp_keepalive, ip, port2, stop2, key, salt, True)
        stt = await put(pairing, aid, SELECTED, selected_tlv(sess2, 1, 640, 360, 15, 300))
        ok("second start (only plain or replayed RTCP will come)", stt == 0, f"status {stt}")
        t1 = time.time()
        gone = None
        while time.time() - t1 < 50:
            await asyncio.sleep(1)
            n = api("GET", "/api/homekit")["live_streams"]
            if n == 0:
                gone = time.time() - t1
                break
            # Keep the HAP session busy so only RTCP is missing.
            await pairing.get_characteristics([(aid, STATUS)])
        stop2[0] = 0
        await fk
        ok("stopped without real RTCP after ~30 s", gone is not None and 25 < gone < 40, f"after {gone:.1f}s" if gone else "still running")

        # Suspend ends the session; resuming it then is refused (Home sets up a new one).
        sess4 = os.urandom(16)
        await put(pairing, aid, SETUP, setup_tlv(sess4, ip, key, salt))
        stt = await put(pairing, aid, SELECTED, selected_tlv(sess4, 1, 640, 360, 15, 300))
        await asyncio.sleep(2)
        stt2 = await put(pairing, aid, SELECTED, selected_tlv(sess4, 2))
        await asyncio.sleep(1)
        n = api("GET", "/api/homekit")["live_streams"]
        stt3 = await put(pairing, aid, SELECTED, selected_tlv(sess4, 3))
        ok("suspend ends the session, resume refused", stt == 0 and stt2 == 0 and n == 0 and stt3 != 0, f"start {stt}, suspend {stt2}, running {n}, resume {stt3}")

        if CHECK == "text":
            # Changing the burned-in text while it streams ends the stream (no view with the old text).
            cams = api("GET", "/api/cameras")["cameras"]
            cam = [c for c in cams if c["name"] == CAMERA][0]
            sess5 = os.urandom(16)
            await put(pairing, aid, SETUP, setup_tlv(sess5, ip, key, salt))
            stt = await put(pairing, aid, SELECTED, selected_tlv(sess5, 1, 640, 360, 15, 300))
            await asyncio.sleep(3)
            n_before = api("GET", "/api/homekit")["live_streams"]
            old = cam.get("overlay_text") or ""
            api("PUT", f"/api/cameras/{cam['id']}", {"overlay_text": old + " changed"})
            t2 = time.time()
            gone = None
            while time.time() - t2 < 10:
                await asyncio.sleep(0.25)
                if api("GET", "/api/homekit")["live_streams"] == 0:
                    gone = time.time() - t2
                    break
            ok("text changed while streaming ends the stream", stt == 0 and n_before == 1 and gone is not None and gone < 3, f"after {gone:.1f}s" if gone is not None else f"start {stt}, running {n_before}, still running")
            api("PUT", f"/api/cameras/{cam['id']}", {"overlay_text": old})
            # (The camera restarts with its settings: wait until it is back.)
            await asyncio.sleep(2)
            for _ in range(30):
                if [c for c in api("GET", "/api/cameras")["cameras"] if c["name"] == CAMERA][0].get("online"):
                    break
                await asyncio.sleep(1)
            await asyncio.sleep(2)

        # Closing the HAP connection ends its stream.
        sess3 = os.urandom(16)
        await put(pairing, aid, SETUP, setup_tlv(sess3, ip, key, salt))
        stt = await put(pairing, aid, SELECTED, selected_tlv(sess3, 1, 640, 360, 15, 300))
        await asyncio.sleep(2)
        n_before = api("GET", "/api/homekit")["live_streams"]
        await pairing.close()
        await asyncio.sleep(2)
        n_after = api("GET", "/api/homekit")["live_streams"]
        ok("closing the connection stops its stream", stt == 0 and n_before == 1 and n_after == 0, f"{n_before} -> {n_after}")
        # Remove the pairing from Settings (per camera in standalone mode).
        if MODE == "standalone":
            hk = api("POST", "/api/homekit/unpair", {"camera": CAMERA})
            live = [a for a in hk["accessories"] if a.get("camera") == CAMERA][0]
            ok("Settings: remove this camera's pairings (pairing stays closed)", not live["paired"] and not live.get("setup_uri") and not live["pairing_open"], str({k: live.get(k) for k in ("paired", "port", "pairing_open")}))
        else:
            hk = api("POST", "/api/homekit/reset", {})
            ok("Settings: reset", not hk["paired"])
    finally:
        await controller.async_stop()
        await azc.async_close()
    failed = [n for n, c in results if not c]
    print(f"\n{len(results) - len(failed)} passed, {len(failed)} failed" + (": " + ", ".join(failed) if failed else ""))
    sys.exit(1 if failed else 0)


asyncio.run(main())
