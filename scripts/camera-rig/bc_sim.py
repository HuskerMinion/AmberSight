#!/usr/bin/env python3
"""A fake Reolink camera on Baichuan TCP (Reolink's own protocol, port 9000)
for the camera controls over port 9000: sign-in (nonce, hashed user name and
password), what it supports (message 199), the account's abilities (151), the
infrared and status lights (208 read, 209 write) and the siren (263).

The read answers are shaped as the lab's E1 Pro answered them (read-only,
2026-11-19); the writes follow public descriptions of the protocol and are
proven only against this fake.

Encryption: AES-128-CFB when the `cryptography` package is there, else the
camera picks the old XOR cipher (AmberSight accepts either, and warns about
the second). Otherwise standard library only.

    python3 scripts/camera-rig/bc_sim.py --port 18682 --state-port 18683 \
        [--user admin --password secret] [--noop] [--battery] [--channels 1] [--led-ctrl 65]

GET http://127.0.0.1:<state-port>/sim/state shows the lights, the siren, the
sign-ins and the log of messages, for checks.
"""

import argparse
import hashlib
import json
import socket
import struct
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

try:
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
except ImportError:  # noqa: SIM105
    Cipher = None

MAGIC = 0x0ABCDEF0
LEGACY, MODERN_OFS = 0x6514, 0x6414
XOR_KEY = bytes([0x1F, 0x2D, 0x3C, 0x4B, 0x5A, 0x69, 0x78, 0xFF])
IV = b"0123456789abcdef"
NONCE = "S1MN0NCE"


def xor(offset, data):
    return bytes(b ^ XOR_KEY[(offset + i) % 8] ^ (offset & 0xFF) for i, b in enumerate(data))


def md5_31(s):
    return hashlib.md5(s.encode()).hexdigest().upper()[:31]


class Crypt:
    def __init__(self, password):
        self.key = hashlib.md5(f"{NONCE}-{password}".encode()).hexdigest().upper()[:16].encode()

    def run(self, data, enc):
        c = Cipher(algorithms.AES(self.key), modes.CFB(IV))
        x = c.encryptor() if enc else c.decryptor()
        return x.update(data) + x.finalize()


def tag(x, name):
    a, b = x.find(f"<{name}>"), x.find(f"</{name}>")
    return x[a + len(name) + 2:b].strip() if a >= 0 and b > a else None


class Cam:
    def __init__(self, a):
        self.lock = threading.Lock()
        self.a = a
        self.ir, self.light = "auto", "close"
        # The siren as two things: a timed play that ends by itself after its
        # rounds (playMode 0), and the manual switch (playMode 2) that stays on.
        self.siren_timed_until = 0.0
        self.siren_manual = False
        self.logins = 0
        self.refused = 0
        # Connections open now and the most ever open at once: AmberSight may sign
        # in again after an idle sign-out, but never holds two sessions at once.
        self.open_now = 0
        self.max_open = 0
        self.log = []

    def snapshot(self):
        with self.lock:
            siren = self.siren_manual or time.time() < self.siren_timed_until
            return {"ir": self.ir, "light": self.light, "siren": siren, "siren_manual": self.siren_manual, "logins": self.logins, "refused": self.refused,
                    "open_now": self.open_now, "max_open": self.max_open,
                    "aes": Cipher is not None, "log": list(self.log)}


def frame(msg_id, num, status, class_, body, channel=0):
    h = struct.pack("<IIIBBHHH", MAGIC, msg_id, len(body), channel, 0, num, status, class_)
    if class_ == 0:
        h += struct.pack("<I", 0)
    return h + body


def serve(conn, cam):
    a = cam.a
    crypt = None
    buf = b""
    conn.settimeout(120)
    with cam.lock:
        cam.open_now += 1
        cam.max_open = max(cam.max_open, cam.open_now)
    try:
        while True:
            d = conn.recv(65536)
            if not d:
                return
            buf += d
            while len(buf) >= 20:
                magic, msg_id, blen, ch, _st, num, status, class_ = struct.unpack("<IIIBBHHH", buf[:20])
                if magic != MAGIC:
                    return
                hl = 24 if class_ in (MODERN_OFS, 0) else 20
                if len(buf) < hl + blen:
                    break
                off = struct.unpack("<I", buf[20:24])[0] if hl == 24 else 0
                body = buf[hl:hl + blen]
                buf = buf[hl + blen:]
                if msg_id == 1 and class_ == LEGACY:
                    x = f'<?xml version="1.0" encoding="UTF-8" ?>\n<body>\n<Encryption version="1.1">\n<type>md5</type>\n<nonce>{NONCE}</nonce>\n</Encryption>\n</body>\n'
                    conn.sendall(frame(1, num, 0xDD12 if Cipher else 0xDD01, LEGACY, xor(0, x.encode())))
                    continue
                if msg_id == 1:
                    x = xor(0, body).decode(errors="replace")
                    ok = tag(x, "userName") == md5_31(a.user + NONCE) and tag(x, "password") == md5_31(a.password + NONCE)
                    with cam.lock:
                        cam.logins += 1
                        if not ok:
                            cam.refused += 1
                    if not ok:
                        conn.sendall(frame(1, num, 401, 0, b""))
                        continue
                    dev = f'<?xml version="1.0" encoding="UTF-8" ?>\n<body>\n<DeviceInfo version="1.1">\n<type>wifi_solo_ipc</type>\n<channelNum>{a.channels}</channelNum>\n</DeviceInfo>\n</body>\n'
                    if a.battery:
                        dev = dev.replace("</DeviceInfo>", "<batteryPercent>80</batteryPercent>\n</DeviceInfo>")
                    crypt = Crypt(a.password) if Cipher else None
                    conn.sendall(frame(1, num, 200, 0, xor(0, dev.encode())))
                    continue

                def dec(p):
                    if not p:
                        return ""
                    return (crypt.run(p, False) if crypt else xor(ch, p)).decode(errors="replace")

                text = dec(body[:off]) + dec(body[off:])
                answer = handle(cam, msg_id, status, text)
                if answer is None:
                    continue
                st, x = answer
                out = (crypt.run(x.encode(), True) if crypt else xor(ch, x.encode())) if x else b""
                conn.sendall(frame(msg_id, num, st, 0, out, ch))
    except OSError:
        return
    finally:
        conn.close()
        with cam.lock:
            cam.open_now -= 1


def handle(cam, msg_id, status, text):
    a = cam.a
    with cam.lock:
        if msg_id not in (234,):
            cam.log.append({"id": msg_id, "xml": text})
        if msg_id == 234:
            return (200, "") if status == 0 else None
        if msg_id == 2:
            return None
        if msg_id == 199:
            return 200, (f'<?xml version="1.0" encoding="UTF-8" ?>\n<body>\n<Support version="1.1">\n<channelNum>{a.channels}</channelNum>\n'
                         f'<audioAlarm>1</audioAlarm>\n<item>\n<chnID>0</chnID>\n<battery>{1 if a.battery else 0}</battery>\n'
                         f'<audioVersion>31</audioVersion>\n<ledCtrl>{a.led_ctrl}</ledCtrl>\n</item>\n</Support>\n</body>\n')
        if msg_id == 151:
            return 200, ('<?xml version="1.0" encoding="UTF-8" ?>\n<body>\n<AbilityInfo version="1.1">\n'
                         f'<userName>{a.user}</userName>\n<image>\n<subModule>\n<channelId>0</channelId>\n'
                         '<abilityValue>ispBasic_rw, ispAdvance_rw, ledState_rw</abilityValue>\n</subModule>\n</image>\n</AbilityInfo>\n</body>\n')
        if msg_id == 208:
            return 200, ('<?xml version="1.0" encoding="UTF-8" ?>\n<body>\n<LedState version="1.1">\n<channelId>0</channelId>\n'
                         f'<ledVersion>2</ledVersion>\n<state>{cam.ir}</state>\n<lightState>{cam.light}</lightState>\n</LedState>\n</body>\n')
        if msg_id == 209:
            if not a.noop:
                cam.ir = tag(text, "state") or cam.ir
                cam.light = tag(text, "lightState") or cam.light
            return 200, ""
        if msg_id == 263:
            on = tag(text, "onOff") == "1"
            if tag(text, "playMode") == "0":
                rounds = int(tag(text, "playTimes") or "1")
                cam.siren_timed_until = time.time() + rounds * a.round_secs
            else:
                cam.siren_manual = on
                if not on:
                    cam.siren_timed_until = 0.0
            return 200, ""
        return 400, ""


class State(BaseHTTPRequestHandler):
    cam = None

    def do_GET(self):  # noqa: N802
        body = json.dumps(self.cam.snapshot()).encode() if self.path == "/sim/state" else b"{}"
        self.send_response(200 if self.path == "/sim/state" else 404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=18682)
    p.add_argument("--state-port", type=int, default=18683)
    p.add_argument("--user", default="admin")
    p.add_argument("--password", default="secret")
    p.add_argument("--noop", action="store_true", help="writes answer done and change nothing")
    p.add_argument("--battery", action="store_true", help="report a battery (AmberSight must ask nothing)")
    p.add_argument("--channels", type=int, default=1)
    p.add_argument("--led-ctrl", type=int, default=65, help="199 ledCtrl (65: E1 Pro, 7: with a spotlight)")
    p.add_argument("--round-secs", type=float, default=10.0, help="one round of a timed siren")
    a = p.parse_args()
    cam = Cam(a)
    State.cam = cam
    threading.Thread(target=ThreadingHTTPServer(("127.0.0.1", a.state_port), State).serve_forever, daemon=True).start()
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", a.port))
    s.listen(8)
    while True:
        c, _ = s.accept()
        threading.Thread(target=serve, args=(c, cam), daemon=True).start()


if __name__ == "__main__":
    main()
