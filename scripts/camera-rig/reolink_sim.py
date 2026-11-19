#!/usr/bin/env python3
"""A fake Reolink camera's web API (/cgi-bin/api.cgi) for the camera controls:
sign-in, GetAbility, the spotlight (GetWhiteLed/SetWhiteLed), the siren
(AudioAlarmPlay), the infrared lights (GetIrLights/SetIrLights) and the status
light (GetPowerLed/SetPowerLed). Shapes follow the vendor's API guide and what
real cameras answer (floodLight reads 0 even on floodlights, GetWhiteLed answers
on cameras with no light, the range lists no light modes).

Plain HTTP on 127.0.0.1. Nothing is installed; standard library only.

    python3 scripts/camera-rig/reolink_sim.py --port 18680 [--no-light] [--no-siren]
        [--no-brightness] [--noop] [--mode 1] [--user admin --password secret]

GET /sim/state (no sign-in) shows the current state and the log of changes, for checks.
"""

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse


class Cam:
    def __init__(self, a):
        self.lock = threading.Lock()
        self.light = None if a.no_light else {"state": 0, "mode": a.mode, "bright": 85}
        self.brightness = not a.no_brightness
        self.siren = not a.no_siren
        self.ir = "Auto"
        self.power_led = "On"
        self.noop = a.noop
        self.user, self.password = a.user, a.password
        self.token = "simtok"
        self.siren_until = 0.0
        self.log = []


def ab(on):
    return {"permit": 6 if on else 0, "ver": 1 if on else 0}


def ok(cmd, value, rng=None):
    r = {"cmd": cmd, "code": 0, "value": value}
    if rng is not None:
        r["range"] = rng
    return r


def fail(cmd, code, detail):
    return {"cmd": cmd, "code": 1, "error": {"rspCode": code, "detail": detail}}


def answer(cam, query, body):
    try:
        cmds = json.loads(body or b"[]")
    except ValueError:
        cmds = []
    token = (query.get("token") or [""])[0]
    out = []
    with cam.lock:
        for c in cmds:
            cmd = c.get("cmd", "")
            p = c.get("param") or {}
            if cmd.startswith("Set") or cmd == "AudioAlarmPlay":
                cam.log.append({"t": time.time(), "cmd": cmd, "param": p})
            if cmd == "Login":
                u = (p.get("User") or {})
                good = u.get("userName") == cam.user and u.get("password") == cam.password
                out.append(ok(cmd, {"Token": {"name": cam.token, "leaseTime": 3600}}) if good else fail(cmd, -7, "login failed"))
                continue
            if token != cam.token:
                out.append(fail(cmd, -6, "please login first"))
                continue
            light = cam.light is not None
            if cmd == "Logout":
                out.append(ok(cmd, {"rspCode": 200}))
            elif cmd == "GetDevInfo":
                out.append(ok(cmd, {"DevInfo": {"type": "IPC", "model": "Simulated camera"}}))
            elif cmd == "GetAbility":
                out.append(ok(cmd, {"Ability": {"alarmAudio": ab(cam.siren), "abilityChn": [{
                    "floodLight": ab(False), "supportFLswitch": ab(light), "supportFLBrightness": ab(light and cam.brightness),
                    "supportFLIntelligent": ab(light), "supportFLSchedule": ab(light), "alarmAudio": ab(cam.siren),
                    "ledControl": ab(True), "powerLed": ab(False)}]}}))
            elif cmd == "GetWhiteLed":
                w = cam.light or {"state": 0, "mode": 1, "bright": 100}
                out.append(ok(cmd, {"WhiteLed": dict(channel=0, LightingSchedule={"StartHour": 18, "StartMin": 0, "EndHour": 6, "EndMin": 0},
                                                     wlAiDetectType={"people": 1, "vehicle": 0, "dog_cat": 0}, **w)},
                              {"WhiteLed": {"bright": {"min": 0, "max": 100}}}))
            elif cmd == "SetWhiteLed":
                if not light:
                    out.append(fail(cmd, -9, "not support"))
                else:
                    w = p.get("WhiteLed") or {}
                    if not cam.noop:
                        for k in ("state", "mode", "bright"):
                            if k in w:
                                cam.light[k] = int(w[k])
                    out.append(ok(cmd, {"rspCode": 200}))
            elif cmd == "AudioAlarmPlay":
                if not cam.siren:
                    out.append(fail(cmd, -9, "not support"))
                else:
                    if not cam.noop:
                        if p.get("alarm_mode") == "times":
                            cam.siren_until = time.time() + 5 * max(1, int(p.get("times", 1)))
                        elif int(p.get("manual_switch", 0)) == 1:
                            cam.siren_until = time.time() + 3600
                        else:
                            cam.siren_until = 0.0
                    out.append(ok(cmd, {"rspCode": 200}))
            elif cmd == "GetIrLights":
                out.append(ok(cmd, {"IrLights": {"channel": 0, "state": cam.ir}}, {"IrLights": {"state": ["Auto", "Off"]}}))
            elif cmd == "SetIrLights":
                if not cam.noop:
                    cam.ir = (p.get("IrLights") or {}).get("state", cam.ir)
                out.append(ok(cmd, {"rspCode": 200}))
            elif cmd == "GetPowerLed":
                out.append(ok(cmd, {"PowerLed": {"channel": 0, "state": cam.power_led}}, {"PowerLed": {"state": ["On", "Off"]}}))
            elif cmd == "SetPowerLed":
                if not cam.noop:
                    cam.power_led = (p.get("PowerLed") or {}).get("state", cam.power_led)
                out.append(ok(cmd, {"rspCode": 200}))
            else:
                out.append(fail(cmd, -9, "not support"))
    return out


def handler(cam):
    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def send(self, obj):
            b = json.dumps(obj).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)

        def do_GET(self):
            if self.path == "/sim/state":
                with cam.lock:
                    self.send({"light": cam.light, "ir": cam.ir, "power_led": cam.power_led,
                               "siren_playing": cam.siren_until > time.time(), "log": cam.log})
            else:
                self.send_error(404)

        def do_POST(self):
            u = urlparse(self.path)
            n = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(n) if n else b""
            if u.path not in ("/cgi-bin/api.cgi", "/api.cgi"):
                self.send_error(404)
                return
            self.send(answer(cam, parse_qs(u.query), body))
    return H


def main():
    a = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    a.add_argument("--port", type=int, default=18680)
    a.add_argument("--user", default="admin")
    a.add_argument("--password", default="secret")
    a.add_argument("--mode", type=int, default=1)
    a.add_argument("--no-light", action="store_true")
    a.add_argument("--no-siren", action="store_true")
    a.add_argument("--no-brightness", action="store_true")
    a.add_argument("--noop", action="store_true", help="answer 'done' and change nothing")
    args = a.parse_args()
    srv = ThreadingHTTPServer(("127.0.0.1", args.port), handler(Cam(args)))
    print(f"fake Reolink API on 127.0.0.1:{args.port}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
