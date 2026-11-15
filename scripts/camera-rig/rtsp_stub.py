#!/usr/bin/env python3
"""A small RTSP server for the camera cases MediaMTX and GStreamer cannot make.

RTP over TCP (interleaved) only. It sends H.264 from a prepared Annex B file
(and AAC or G.711 audio from prepared files) in real time, with RTCP sender
reports, and logs every request to a JSON-lines file the runner reads.

Paths (anything else answers 404):
  /drop             plays, then closes the connection after --drop-after seconds
  /silent           answers DESCRIBE, SETUP and PLAY, then sends no packets
  /tsjump           video RTP time jumps forward one hour after --jump-after seconds
  /tsback           video RTP time jumps back 30 s after --jump-after seconds
  /authcount        Digest auth (admin / --password); every try is logged
  /cam/realmonitor  Dahua style: needs ?channel=1&subtype=0|1, Content-Base keeps the query
  /stream0          needs ?username=admin&password=<--query-password> (no RTSP auth)
  /oddclock         H.264 + G.711 mu-law whose RTP time steps 1.875x its samples
  /adts             H.264 + AAC frames that keep their ADTS header, each after an empty frame
"""
import argparse
import asyncio
import base64
import hashlib
import json
import os
import random
import struct
import time
import urllib.parse

ARGS = None
LOG = None


def log(**kv):
    kv["t"] = round(time.time(), 3)
    LOG.write(json.dumps(kv) + "\n")
    LOG.flush()


# ---------------------------------------------------------------- media

def split_nals(data):
    """NAL units of an Annex B stream."""
    out, i, n = [], 0, len(data)
    starts = []
    while True:
        j = data.find(b"\x00\x00\x01", i)
        if j < 0:
            break
        starts.append(j + 3)
        i = j + 3
    for k, s in enumerate(starts):
        e = starts[k + 1] - 3 if k + 1 < len(starts) else n
        nal = data[s:e]
        while nal.endswith(b"\x00"):
            nal = nal[:-1]
        if nal:
            out.append(nal)
    return out


def access_units(nals):
    """Group NAL units into frames, split at access unit delimiters (type 9)."""
    aus, cur = [], []
    for nal in nals:
        t = nal[0] & 0x1F
        if t == 9:
            if cur:
                aus.append(cur)
            cur = []
            continue
        cur.append(nal)
    if cur:
        aus.append(cur)
    return aus


def adts_frames(data):
    out, i = [], 0
    while i + 7 <= len(data):
        if data[i] != 0xFF or data[i + 1] & 0xF0 != 0xF0:
            i += 1
            continue
        ln = ((data[i + 3] & 3) << 11) | (data[i + 4] << 3) | (data[i + 5] >> 5)
        if ln < 7:
            break
        out.append(data[i:i + ln])
        i += ln
    return out


class Media:
    def __init__(self, d, fps):
        self.fps = fps
        self.aus = access_units(split_nals(open(os.path.join(d, "v.h264"), "rb").read()))
        sps = pps = None
        for au in self.aus:
            for nal in au:
                if nal[0] & 0x1F == 7 and sps is None:
                    sps = nal
                if nal[0] & 0x1F == 8 and pps is None:
                    pps = nal
        self.sps, self.pps = sps, pps
        self.adts = adts_frames(open(os.path.join(d, "a.adts"), "rb").read())
        self.ulaw = open(os.path.join(d, "a.ulaw"), "rb").read()


MEDIA = None


def rtp(pt, marker, seq, ts, ssrc, payload):
    return struct.pack("!BBHII", 0x80, (0x80 if marker else 0) | pt, seq & 0xFFFF, ts & 0xFFFFFFFF, ssrc) + payload


def h264_packets(au, mtu=1400):
    """(marker, payload) RTP payloads of one frame: single NAL or FU-A."""
    pk = []
    for k, nal in enumerate(au):
        last = k == len(au) - 1
        if len(nal) <= mtu:
            pk.append((last, nal))
            continue
        ind = (nal[0] & 0xE0) | 28
        typ = nal[0] & 0x1F
        body = nal[1:]
        pos = 0
        while pos < len(body):
            chunk = body[pos:pos + mtu]
            s = pos == 0
            e = pos + mtu >= len(body)
            hdr = (0x80 if s else 0) | (0x40 if e else 0) | typ
            pk.append((last and e, bytes([ind, hdr]) + chunk))
            pos += mtu
    return pk


def sender_report(ssrc, rtp_ts, packets, octets):
    now = time.time() + 2208988800
    sec = int(now)
    frac = int((now - sec) * (1 << 32))
    return struct.pack("!BBHIIIIII", 0x80, 200, 6, ssrc, sec, frac, rtp_ts & 0xFFFFFFFF, packets, octets)


# ---------------------------------------------------------------- RTSP

def parse_auth(h):
    out = {}
    if not h.lower().startswith("digest "):
        return out
    for part in h[7:].split(","):
        if "=" in part:
            k, v = part.strip().split("=", 1)
            out[k.strip().lower()] = v.strip().strip('"')
    return out


def md5(s):
    return hashlib.md5(s.encode()).hexdigest()


class Conn:
    def __init__(self, reader, writer):
        self.r, self.w = reader, writer
        self.peer = writer.get_extra_info("peername")
        self.session = "%08x" % random.getrandbits(32)
        self.nonce = "%032x" % random.getrandbits(128)
        self.channels = {}  # track -> interleaved RTP channel
        self.kind = None
        self.stream_task = None
        self.closed = False

    def send(self, data):
        if not self.closed:
            self.w.write(data)

    def reply(self, cseq, code, reason, headers=None, body=b""):
        lines = [f"RTSP/1.0 {code} {reason}", f"CSeq: {cseq}", "Server: camrig-stub"]
        for k, v in (headers or {}).items():
            lines.append(f"{k}: {v}")
        if body:
            lines.append(f"Content-Length: {len(body)}")
        self.send(("\r\n".join(lines) + "\r\n\r\n").encode() + body)

    async def read_request(self):
        while True:
            b = await self.r.readexactly(1)
            if b == b"$":
                hdr = await self.r.readexactly(3)
                ln = struct.unpack("!H", hdr[1:])[0]
                await self.r.readexactly(ln)  # RTCP receiver reports
                continue
            rest = await self.r.readuntil(b"\r\n\r\n")
            text = (b + rest).decode("latin-1")
            lines = text.split("\r\n")
            method, url, _ = (lines[0].split(" ") + ["", "", ""])[:3]
            hdrs = {}
            for ln in lines[1:]:
                if ":" in ln:
                    k, v = ln.split(":", 1)
                    hdrs[k.strip().lower()] = v.strip()
            if int(hdrs.get("content-length", "0") or 0):
                await self.r.readexactly(int(hdrs["content-length"]))
            return method, url, hdrs

    def route(self, url):
        u = urllib.parse.urlsplit(url)
        path = u.path.rstrip("/")
        for name in ("drop", "silent", "tsjump", "tsback", "authcount", "oddclock", "adts", "stream0"):
            if path == "/" + name or path.startswith("/" + name + "/"):
                return name, u
        if path.startswith("/cam/realmonitor"):
            return "realmonitor", u
        return None, u

    def sdp(self, kind, base_host):
        m = MEDIA
        sprop = base64.b64encode(m.sps).decode() + "," + base64.b64encode(m.pps).decode()
        plid = m.sps[1:4].hex()
        s = [
            "v=0", f"o=- {int(time.time())} 1 IN IP4 {base_host}", "s=camrig stub", "c=IN IP4 0.0.0.0", "t=0 0",
            "m=video 0 RTP/AVP 96", "a=rtpmap:96 H264/90000",
            f"a=fmtp:96 packetization-mode=1;profile-level-id={plid};sprop-parameter-sets={sprop}",
            f"a=framerate:{m.fps}", "a=control:trackID=0",
        ]
        if kind == "oddclock":
            s += ["m=audio 0 RTP/AVP 0", "a=rtpmap:0 PCMU/8000", "a=control:trackID=1"]
        if kind == "adts":
            s += ["m=audio 0 RTP/AVP 97", "a=rtpmap:97 MPEG4-GENERIC/16000/1",
                  "a=fmtp:97 streamtype=5;profile-level-id=15;mode=AAC-hbr;config=1408;sizelength=13;indexlength=3;indexdeltalength=3",
                  "a=control:trackID=1"]
        return ("\r\n".join(s) + "\r\n").encode()

    def check_access(self, kind, u, method, url, hdrs):
        """None when allowed, else (code, reason, headers)."""
        q = urllib.parse.parse_qs(u.query, keep_blank_values=True)
        if kind == "realmonitor":
            ch = q.get("channel", [""])[0]
            st = q.get("subtype", [""])[0].split("/")[0]
            if ch != "1" or st not in ("0", "1"):
                return (400, "Bad Request", {})
        if kind == "stream0" and method == "DESCRIBE":
            if q.get("username", [""])[0] != "admin" or q.get("password", [""])[0] != ARGS.query_password:
                return (401, "Unauthorized", {})
        if kind == "authcount":
            a = parse_auth(hdrs.get("authorization", ""))
            ok = False
            if a:
                ha1 = md5(f"{a.get('username')}:camrig:{ARGS.password}")
                ha2 = md5(f"{method}:{a.get('uri', '')}")
                ok = a.get("username") == "admin" and a.get("response") == md5(f"{ha1}:{a.get('nonce')}:{ha2}")
            log(path=kind, method=method, auth="ok" if ok else ("bad" if a else "none"), peer=str(self.peer))
            if not ok:
                return (401, "Unauthorized", {"WWW-Authenticate": f'Digest realm="camrig", nonce="{self.nonce}"'})
        return None

    async def run(self):
        try:
            while True:
                method, url, hdrs = await self.read_request()
                cseq = hdrs.get("cseq", "0")
                kind, u = self.route(url)
                log(path=kind or u.path, method=method, url=_redact(url), peer=str(self.peer))
                if method == "OPTIONS":
                    self.reply(cseq, 200, "OK", {"Public": "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER"})
                    continue
                if kind is None:
                    self.reply(cseq, 404, "Not Found")
                    continue
                denied = self.check_access(kind, u, method, url, hdrs)
                if denied:
                    self.reply(cseq, denied[0], denied[1], denied[2])
                    continue
                self.kind = kind
                if method == "DESCRIBE":
                    host = u.hostname or "127.0.0.1"
                    base = url if url.endswith("/") else url + "/"
                    self.reply(cseq, 200, "OK", {"Content-Type": "application/sdp", "Content-Base": base}, self.sdp(kind, host))
                elif method == "SETUP":
                    tr = hdrs.get("transport", "")
                    track = 1 if "trackid=1" in url.lower() else 0
                    if "TCP" not in tr.upper() or "interleaved=" not in tr:
                        self.reply(cseq, 461, "Unsupported Transport")
                        continue
                    ch = int(tr.split("interleaved=")[1].split("-")[0].split(";")[0])
                    self.channels[track] = ch
                    self.reply(cseq, 200, "OK", {
                        "Transport": f"RTP/AVP/TCP;unicast;interleaved={ch}-{ch + 1};ssrc={0x1000 + track:08X}",
                        "Session": f"{self.session};timeout=60"})
                elif method == "PLAY":
                    self.reply(cseq, 200, "OK", {"Session": self.session, "Range": "npt=0.000-"})
                    if self.stream_task is None and kind != "silent":
                        self.stream_task = asyncio.create_task(self.stream())
                elif method in ("GET_PARAMETER", "SET_PARAMETER"):
                    self.reply(cseq, 200, "OK", {"Session": self.session})
                elif method == "TEARDOWN":
                    self.reply(cseq, 200, "OK", {"Session": self.session})
                    break
                else:
                    self.reply(cseq, 405, "Method Not Allowed")
                await self.w.drain()
        except (asyncio.IncompleteReadError, ConnectionError, asyncio.LimitOverrunError):
            pass
        finally:
            self.closed = True
            if self.stream_task:
                self.stream_task.cancel()
            try:
                self.w.close()
            except Exception:
                pass

    async def stream(self):
        m = MEDIA
        kind = self.kind
        vch = self.channels.get(0, 0)
        ach = self.channels.get(1)
        vssrc, assrc = 0x1000, 0x1001
        vseq, aseq = random.randrange(65536), random.randrange(65536)
        vts0 = random.getrandbits(32)
        ats0 = random.getrandbits(32)
        start = time.monotonic()
        vframe = 0
        apkt = 0
        vcount = vbytes = acount = abytes = 0
        jumped = False
        jump = 0
        next_sr = start
        try:
            while not self.closed:
                now = time.monotonic()
                el = now - start
                if kind == "drop" and el >= ARGS.drop_after:
                    log(path=kind, event="dropped")
                    self.closed = True
                    self.w.close()
                    return
                if kind in ("tsjump", "tsback") and not jumped and el >= ARGS.jump_after:
                    jumped = True
                    jump = 3600 * 90000 if kind == "tsjump" else -30 * 90000
                    log(path=kind, event="jumped", ticks=jump)
                vdue = start + vframe / m.fps
                adue = None
                if ach is not None:
                    adue = start + apkt * (0.040 if kind == "oddclock" else 1024 / 16000)
                if now >= vdue:
                    au = m.aus[vframe % len(m.aus)]
                    ts = vts0 + vframe * (90000 // m.fps) + jump
                    for marker, payload in h264_packets(au):
                        pkt = rtp(96, marker, vseq, ts, vssrc, payload)
                        vseq += 1
                        vcount += 1
                        vbytes += len(payload)
                        self.send(b"$" + bytes([vch]) + struct.pack("!H", len(pkt)) + pkt)
                    vframe += 1
                if adue is not None and now >= adue:
                    if kind == "oddclock":
                        n = 320
                        off = (apkt * n) % (len(m.ulaw) - n)
                        payload = m.ulaw[off:off + n]
                        ts = ats0 + apkt * 600  # 1.875x the 320 samples sent
                        pkt = rtp(0, False, aseq, ts, assrc, payload)
                        aseq += 1
                        self.send(b"$" + bytes([ach]) + struct.pack("!H", len(pkt)) + pkt)
                    else:
                        fr = m.adts[apkt % len(m.adts)]
                        ts = ats0 + apkt * 1024
                        # An empty AU, then the frame with its ADTS header kept.
                        for au in (b"", fr):
                            hdr = struct.pack("!HH", 16, (len(au) << 3) & 0xFFFF)
                            pkt = rtp(97, True, aseq, ts, assrc, hdr + au)
                            aseq += 1
                            self.send(b"$" + bytes([ach]) + struct.pack("!H", len(pkt)) + pkt)
                    acount += 1
                    abytes += 300
                    apkt += 1
                if now >= next_sr:
                    vts_now = vts0 + int(el * 90000) + jump
                    sr = sender_report(vssrc, vts_now, vcount, vbytes)
                    self.send(b"$" + bytes([vch + 1]) + struct.pack("!H", len(sr)) + sr)
                    if ach is not None:
                        rate = 15000 if kind == "oddclock" else 16000
                        sr = sender_report(assrc, ats0 + int(el * rate), acount, abytes)
                        self.send(b"$" + bytes([ach + 1]) + struct.pack("!H", len(sr)) + sr)
                    next_sr = now + 1.0
                await self.w.drain()
                await asyncio.sleep(0.005)
        except (ConnectionError, asyncio.CancelledError):
            pass


def _redact(url):
    u = urllib.parse.urlsplit(url)
    q = "&".join(p if not p.lower().startswith("password=") else "password=***" for p in u.query.split("&")) if u.query else ""
    host = u.hostname or ""
    return urllib.parse.urlunsplit((u.scheme, host + (f":{u.port}" if u.port else ""), u.path, q, ""))


async def main():
    global ARGS, LOG, MEDIA
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=18570)
    p.add_argument("--media", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--fps", type=int, default=10)
    p.add_argument("--password", default="right-pass")
    p.add_argument("--query-password", default="q1w2e3")
    p.add_argument("--drop-after", type=float, default=20)
    p.add_argument("--jump-after", type=float, default=20)
    ARGS = p.parse_args()
    LOG = open(ARGS.log, "a")
    MEDIA = Media(ARGS.media, ARGS.fps)
    srv = await asyncio.start_server(lambda r, w: Conn(r, w).run(), "0.0.0.0", ARGS.port)
    log(event="listening", port=ARGS.port, frames=len(MEDIA.aus), adts=len(MEDIA.adts))
    async with srv:
        await srv.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
