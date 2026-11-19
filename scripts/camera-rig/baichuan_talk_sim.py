#!/usr/bin/env python3
"""A fake Reolink on Baichuan TCP for talk tests (no sound anywhere).

It signs a client in (choosing BC/XOR encryption, so no AES library is
needed), answers TalkAbility (msg 10) with 16 kHz ADPCM, answers TalkConfig
(msg 201) with 200 or, with --refuse, 400, and records every talk audio
message (msg 202). When the client signs out (or --seconds pass) it decodes
the ADPCM it got, writes it as a 16 kHz WAV, checks each block header, and
with --tone measures the SNR of the decoded audio against a sine.

    python3 scripts/camera-rig/baichuan_talk_sim.py --port 19000 --out /tmp/talk &
    python3 scripts/camera-rig/baichuan_talk_sim.py --make-tone /tmp/tone.wav
    ambersight talk-test --config cfg.toml --camera Fake --baichuan --wav /tmp/tone.wav --seconds 4

Written for AmberSight from public protocol notes; standard library only.
"""

import argparse
import json
import math
import socket
import struct
import sys
import time
import wave

MAGIC = 0x0ABCDEF0
KEY = bytes([0x1F, 0x2D, 0x3C, 0x4B, 0x5A, 0x69, 0x78, 0xFF])
ABILITY = (
    '<?xml version="1.0" encoding="UTF-8" ?><body><TalkAbility version="1.1"><duplexList><duplex>FDX</duplex></duplexList>'
    "<audioStreamModeList><audioStreamMode>followVideoStream</audioStreamMode></audioStreamModeList><audioConfigList><audioConfig>"
    "<priority>0</priority><audioType>adpcm</audioType><sampleRate>16000</sampleRate><samplePrecision>16</samplePrecision>"
    "<lengthPerEncoder>1024</lengthPerEncoder><soundTrack>mono</soundTrack></audioConfig></audioConfigList></TalkAbility></body>"
)
STEPS = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143,
    157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552,
    1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487,
    12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
]
INDEX_STEP = [-1, -1, -1, -1, 2, 4, 6, 8]


def xor(offset, data):
    return bytes(b ^ KEY[(offset + i) % 8] ^ (offset & 0xFF) for i, b in enumerate(data))


def header(msg_id, length, ch, st, num, status, cls, offset):
    h = struct.pack("<IIIBBHHH", MAGIC, msg_id, length, ch, st, num, status, cls)
    if offset is not None:
        h += struct.pack("<I", offset)
    return h


def reply(msg_id, ch, num, status, body=b""):
    return header(msg_id, len(body), ch, 0, num, status, 0x0000, 0) + body


def ima_block(payload):
    """Decode one ADPCM packet's inner bytes (from '00 01')."""
    nib = struct.unpack_from("<H", payload, 2)[0]
    pred, index = struct.unpack_from("<hB", payload, 4)
    index = min(index, 88)
    out = []
    for b in payload[8:8 + nib]:
        for code in (b & 15, b >> 4):
            step = STEPS[index]
            d = step >> 3
            if code & 4:
                d += step
            if code & 2:
                d += step >> 1
            if code & 1:
                d += step >> 2
            pred = max(-32768, min(32767, pred - d if code & 8 else pred + d))
            index = max(0, min(88, index + INDEX_STEP[code & 7]))
            out.append(pred)
    return out, nib


def sine_snr(x, freq, fs):
    w = 2 * math.pi * freq / fs
    ss = sc = cc = xs = xc = 0.0
    for i, v in enumerate(x):
        s, c = math.sin(w * i), math.cos(w * i)
        ss += s * s
        sc += s * c
        cc += c * c
        xs += v * s
        xc += v * c
    det = ss * cc - sc * sc
    a = (xs * cc - xc * sc) / det
    b = (xc * ss - xs * sc) / det
    sig = noise = 0.0
    for i, v in enumerate(x):
        f = a * math.sin(w * i) + b * math.cos(w * i)
        sig += f * f
        noise += (v - f) ** 2
    return 10 * math.log10(sig / max(noise, 1e-9))


def make_tone(path, freq=1000.0, rate=48000, secs=3.0, silence=1.0):
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        n0 = int(silence * rate)
        frames = [0] * n0 + [int(8000 * math.sin(2 * math.pi * freq * i / rate)) for i in range(int(secs * rate))]
        w.writeframes(struct.pack("<%dh" % len(frames), *frames))


def serve(conn, args, rec):
    conn.settimeout(0.2)
    buf = b""
    start = time.time()
    while time.time() - start < args.seconds:
        try:
            data = conn.recv(65536)
            if not data:
                return
            buf += data
        except socket.timeout:
            continue
        while len(buf) >= 20:
            magic, msg_id, length, ch, st, num, status, cls = struct.unpack_from("<IIIBBHHH", buf)
            if magic != MAGIC:
                rec["errors"].append("bad magic")
                return
            hl = 24 if cls in (0x6414, 0x0000) else 20
            if len(buf) < hl + length:
                break
            off = struct.unpack_from("<I", buf, 20)[0] if hl == 24 else 0
            body = buf[hl:hl + length]
            buf = buf[hl + length:]
            rec["ids"].append(msg_id)
            if msg_id == 1 and cls == 0x6514:
                nonce = xor(0, b'<?xml version="1.0"?><body><Encryption version="1.1"><type>md5</type><nonce>SIMNONCE</nonce></Encryption></body>')
                conn.sendall(header(1, len(nonce), 0, 0, num, 0xDD00, 0x6514, None) + nonce)
            elif msg_id == 1:
                rec["logins"] += 1
                x = b'<?xml version="1.0"?><body><DeviceInfo version="1.1"><type>ipc</type><channelNum>1</channelNum></DeviceInfo></body>'
                conn.sendall(reply(1, 0, num, 200, xor(0, x)))
            elif msg_id == 2:
                return
            elif msg_id == 234 and status == 0:
                conn.sendall(reply(234, ch, num, 200))
            elif msg_id == 10:
                rec["abilities"] += 1
                conn.sendall(reply(10, ch, num, 200, xor(ch, ABILITY.encode())))
            elif msg_id == 201:
                rec["configs"].append(xor(ch, body[off:]).decode(errors="replace"))
                time.sleep(0.3)
                conn.sendall(reply(201, ch, num, 400 if args.refuse else 200))
            elif msg_id == 202:
                ext = xor(ch, body[:off]).decode(errors="replace")
                rec["talk_ext_ok"] = rec.get("talk_ext_ok", True) and "<binaryData>1</binaryData>" in ext
                rec["talk"].append(body[off:])
                rec["talk_times"].append(time.time())
            else:
                rec["other"].append(msg_id)


def report(args, rec):
    pcm, headers_ok, packets = [], True, 0
    for pay in rec["talk"]:
        i = 0
        while i + 16 <= len(pay):
            if pay[i:i + 4] != b"01wb" or pay[i + 8:i + 10] != b"\x00\x01":
                headers_ok = False
                break
            inner, inner2 = struct.unpack_from("<HH", pay, i + 4)
            samples, nib = ima_block(pay[i + 8:i + 8 + inner])
            headers_ok &= inner == inner2 == 8 + nib and nib == 512
            pcm += samples
            packets += 1
            i += 8 + inner + (-(8 + inner) % 8)
    with wave.open(args.out + ".wav", "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(struct.pack("<%dh" % len(pcm), *pcm))
    t = rec["talk_times"]
    # Pacing: audio sent ahead of real time, at its worst.
    ahead = 0.0
    if t:
        for k, at in enumerate(t):
            ahead = max(ahead, (k + 1) * 1024 / 16000 - (at - t[0]))
    out = {
        "logins": rec["logins"],
        "abilities": rec["abilities"],
        "talk_configs": len(rec["configs"]),
        "talk_messages": len(rec["talk"]),
        "adpcm_packets": packets,
        "block_headers_ok": headers_ok,
        "talk_extension_ok": rec.get("talk_ext_ok", None),
        "seconds_of_audio": round(len(pcm) / 16000, 2),
        "most_audio_ahead_s": round(ahead, 2),
        "other_ids": sorted(set(rec["other"])),
    }
    loud = [i for i, v in enumerate(pcm) if abs(v) > 1000]
    if args.tone and loud and loud[-1] - loud[0] > 2000:
        # The tone part only (the silence sent until the talk is let go is
        # not the tone), less its first and last 16 ms.
        span = pcm[loud[0] + 256:loud[-1] - 256]
        out["tone_seconds"] = round(len(span) / 16000, 2)
        out["snr_db"] = round(sine_snr(span, args.tone, 16000), 1)
    print(json.dumps(out))
    json.dump(out, open(args.out + ".json", "w"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=19000)
    ap.add_argument("--out", default="/tmp/bctalk")
    ap.add_argument("--refuse", action="store_true", help="refuse TalkConfig (status 400), as when the Reolink app talks")
    ap.add_argument("--seconds", type=float, default=60)
    ap.add_argument("--tone", type=float, default=1000.0, help="measure SNR against a sine of this frequency (0: off)")
    ap.add_argument("--make-tone", metavar="WAV", help="write a 48 kHz WAV (1 s silence, 3 s of a 1 kHz tone) and exit")
    args = ap.parse_args()
    if args.make_tone:
        make_tone(args.make_tone)
        return
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", args.port))
    s.listen(1)
    s.settimeout(args.seconds)
    rec = {"ids": [], "logins": 0, "abilities": 0, "configs": [], "talk": [], "talk_times": [], "other": [], "errors": []}
    try:
        conn, _ = s.accept()
        serve(conn, args, rec)
    except socket.timeout:
        pass
    report(args, rec)


if __name__ == "__main__":
    sys.exit(main())
