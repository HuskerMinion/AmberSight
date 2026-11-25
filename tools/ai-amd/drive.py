#!/usr/bin/env python3
"""Dev test driver for ambersight-ai-amd: speaks src/ai_amd_proto.rs.

  drive.py <ambersight-ai-amd> <model.onnx> [checks] [--kill] [--cpu]

Loads the model, runs `checks` (default 30) checks on a fixed test picture,
prints median ms per check and the pipe cost alone (a check refused for its
shape, which never reaches the model). --cpu compares the output with
onnxruntime on the CPU when the onnxruntime and numpy modules are there.
--kill kills the helper (SIGKILL) in the middle of a check.
Set VK_DRIVER_FILES to pick the graphics card's Vulkan driver.
"""
import os, signal, struct, subprocess, sys, time

def frame(kind, payload=b""):
    return kind + struct.pack("<I", len(payload)) + payload

def read_exact(f, n):
    b = b""
    while len(b) < n:
        c = f.read(n - len(b))
        if not c:
            raise EOFError("helper closed its output")
        b += c
    return b

def read_frame(f):
    h = read_exact(f, 5)
    return h[:1], read_exact(f, struct.unpack("<I", h[1:])[0])

def tensor(shape, data):
    return struct.pack("<I", len(shape)) + b"".join(struct.pack("<Q", d) for d in shape) + data

def untensor(b):
    n = struct.unpack("<I", b[:4])[0]
    shape = struct.unpack("<%dQ" % n, b[4:4 + 8 * n])
    return shape, b[4 + 8 * n:]

def picture(s):
    # A fixed, varied test picture (no camera needed).
    import random
    r = random.Random(7)
    vals = [114.0 if (i // 7) % 5 else r.uniform(0, 255) for i in range(3 * s * s)]
    return struct.pack("<%df" % len(vals), *vals)

def main():
    a = [x for x in sys.argv[1:] if not x.startswith("--")]
    flags = [x for x in sys.argv[1:] if x.startswith("--")]
    exe, model = a[0], a[1]
    n = int(a[2]) if len(a) > 2 else 30
    p = subprocess.Popen([exe], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    k, h = read_frame(p.stdout)
    print("HELLO", k, h.decode().replace("\n", " "))
    t = time.time()
    p.stdin.write(frame(b"L", b"model=" + model.encode() + b"\n")); p.stdin.flush()
    try:
        k, r = read_frame(p.stdout)
    except EOFError:
        print("helper exited without an answer, code", p.wait()); return 1
    print("LOAD ->", k, r.decode().replace("\n", " "), "(%.0f ms)" % ((time.time() - t) * 1e3))
    if k != b"R":
        print("exit code", p.wait()); return 1
    s = int(dict(l.split("=", 1) for l in r.decode().split("\n") if l)["input"])
    pic = picture(s)
    req = frame(b"C", tensor([1, 3, s, s], pic))
    if "--kill" in flags:
        p.stdin.write(req); p.stdin.flush()
        time.sleep(0.01)
        os.kill(p.pid, signal.SIGKILL)
        print("killed mid-check; exit", p.wait())
        return 0
    ms, out = [], None
    for _ in range(n):
        t = time.time()
        p.stdin.write(req); p.stdin.flush()
        k, o = read_frame(p.stdout)
        ms.append((time.time() - t) * 1e3)
        if k != b"O":
            print("check failed:", k, o); return 1
        out = o
    ms.sort()
    print("check median %.1f ms (fastest %.1f, slowest %.1f, %d runs)" % (ms[len(ms) // 2], ms[0], ms[-1], n))
    # Pipe alone: wrong-shaped input is refused before the model, so this
    # is the framing, the 5 MB tensor each way's worth of copying and the
    # tensor decode (the answer is a short error).
    bad = frame(b"C", tensor([1, 3, s, s + 1], pic[:4 * 3 * s * (s + 1)] if len(pic) >= 4 * 3 * s * (s + 1) else pic + b"\0" * (4 * 3 * s * (s + 1) - len(pic))))
    pm = []
    for _ in range(n):
        t = time.time()
        p.stdin.write(bad); p.stdin.flush()
        k, o = read_frame(p.stdout)
        pm.append((time.time() - t) * 1e3)
        assert k == b"E", k
    pm.sort()
    print("pipe alone (refused check, input sent, no model run): median %.1f ms" % pm[len(pm) // 2])
    shape, data = untensor(out)
    print("output shape", shape)
    if "--cpu" in flags:
        try:
            import numpy as np, onnxruntime as ort
            x = np.frombuffer(pic, dtype="<f4").reshape(1, 3, s, s)
            sess = ort.InferenceSession(model, providers=["CPUExecutionProvider"])
            c = sess.run(None, {sess.get_inputs()[0].name: x})[0]
            g = np.frombuffer(data, dtype="<f4").reshape(c.shape)
            print("max difference to CPU onnxruntime: %.5f" % float(np.abs(g - c).max()))
        except ImportError:
            print("no numpy/onnxruntime here: --cpu skipped")
    p.stdin.write(frame(b"Q")); p.stdin.flush()
    print("QUIT; exit", p.wait())
    return 0

sys.exit(main())
