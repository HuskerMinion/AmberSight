"""End-to-end test of AmberSight's HomeKit bridge with the aiohomekit controller.

usage: python hk_e2e.py <web base url> <admin user> <admin password> <viewer user> <viewer password> <hap port>
"""
import asyncio, base64, json, os, socket, sys, time, urllib.request, urllib.error
# The HomeKit port's host for the raw checks (another machine: its address).
HOST = os.environ.get("HK_HOST", "127.0.0.1")

from zeroconf.asyncio import AsyncZeroconf, AsyncServiceBrowser
from aiohomekit import Controller
from aiohomekit.exceptions import AuthenticationError, UnavailableError, AccessoryNotFoundError
from aiohomekit.controller.ip.discovery import IpDiscovery

BASE, ADMIN, APW, VIEWER, VPW, HAP_PORT = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], int(sys.argv[6])
results = []


def ok(name, cond, detail=""):
    results.append((name, bool(cond)))
    print(("PASS " if cond else "FAIL ") + name + (f" -- {detail}" if detail else ""), flush=True)


def api(method, path, body=None, user=ADMIN, pw=APW):
    req = urllib.request.Request(BASE + path, method=method, data=None if body is None else json.dumps(body).encode())
    req.add_header("Authorization", "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode())
    if body is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, None


def jpeg_size(b):
    i = 2
    while i + 9 < len(b):
        if b[i] != 0xFF:
            return None
        m, ln = b[i + 1], int.from_bytes(b[i + 2:i + 4], "big")
        if m in (0xC0, 0xC1, 0xC2):
            return int.from_bytes(b[i + 7:i + 9], "big"), int.from_bytes(b[i + 5:i + 7], "big")
        i += 2 + ln
    return None


def raw_http(data):
    s = socket.create_connection((HOST, HAP_PORT), timeout=5)
    s.sendall(data)
    out = b""
    try:
        while b"\r\n\r\n" not in out:
            chunk = s.recv(4096)
            if not chunk:
                break
            out += chunk
    except socket.timeout:
        pass
    s.close()
    return out


async def find(controller, device_id, timeout=20):
    return await controller.async_find(device_id, timeout=timeout)


def open_window(camera=None):
    """Open the pairing window (Settings, Pair a device): pair-setup is refused without it."""
    body = {"action": "open"} if camera is None else {"action": "open", "camera": camera}
    return api("POST", "/api/homekit/pairing", body)


SF_NOW = r"""
import sys, time
from zeroconf import Zeroconf, ServiceBrowser
zc = Zeroconf(); names = set()
ServiceBrowser(zc, "_hap._tcp.local.", handlers=[lambda zeroconf, service_type, name, state_change: names.add(name)])
time.sleep(3)
for n in names:
    i = zc.get_service_info("_hap._tcp.local.", n, 1500)
    if i and i.port == int(sys.argv[1]):
        print(i.properties.get(b"sf", b"").decode())
zc.close()
"""


def sf_now():
    """The sf of our record as the network says it now. (A fresh process: aiohomekit's
    cache can miss a TXT record that changes back to a value it saw before.)"""
    import subprocess
    out = subprocess.run([sys.executable, "-I", "-c", SF_NOW, str(HAP_PORT)], capture_output=True, text=True, timeout=30).stdout.split()
    return int(out[0]) if out and out[0].isdigit() else None


async def wait_sf(controller, device_id, want, timeout=15):
    """The discovery once the network says sf is `want` (None when it never was)."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if sf_now() == want:
            try:
                return await find(controller, device_id, timeout=5)
            except Exception:
                pass
        await asyncio.sleep(1)
    return None


async def main():
    st, hk = api("GET", "/api/homekit")
    ok("admin GET /api/homekit", st == 200 and hk["enabled"] and hk["listening"], f"status {st}")
    acc0 = (hk.get("accessories") or [{}])[0]
    ok("pairing closed at first: no setup code or QR code", hk.get("pairing") is None and "setup_code" not in hk and "qr_svg" not in acc0 and acc0.get("pairing_open") is False, str(hk.get("pairing")))
    st, _ = api("GET", "/api/homekit", user=VIEWER, pw=VPW)
    ok("viewer GET /api/homekit refused", st == 403, f"status {st}")
    st, _ = api("POST", "/api/homekit/reset", {}, user=VIEWER, pw=VPW)
    ok("viewer reset refused", st == 403, f"status {st}")
    st, _ = api("POST", "/api/homekit/pairing", {"action": "open"}, user=VIEWER, pw=VPW)
    ok("viewer may not open pairing", st == 403, f"status {st}")
    st, _ = api("POST", "/api/homekit/pairing", {"action": "open", "camera": "Bars"})
    ok("bridged: open pairing for a camera refused", st == 400, f"status {st}")
    st, hk = open_window()
    acc0 = (hk.get("accessories") or [{}])[0] if hk else {}
    ok("pairing opened: setup code and QR code shown", st == 200 and hk.get("setup_code") and acc0.get("setup_uri", "").startswith("X-HM://") and "<svg" in (acc0.get("qr_svg") or "") and 290 <= hk["pairing"]["seconds_left"] <= 300, f"status {st}")
    code = hk["setup_code"]
    st, hk = api("POST", "/api/homekit/pairing", {"action": "stop"})
    ok("pairing stopped: no setup code", st == 200 and hk.get("pairing") is None and "setup_code" not in hk, f"status {st}")

    # Before pair-verify only pairing is served.
    r = raw_http(b"GET /accessories HTTP/1.1\r\nHost: x\r\n\r\n")
    ok("unverified /accessories -> 470", r.startswith(b"HTTP/1.1 470"), r[:40])
    r = raw_http(b"GET /accessories HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")
    ok("chunked request refused", r.startswith(b"HTTP/1.1 501"), r[:40])

    r = raw_http(b"PUT /characteristics HTTP/1.1\r\nContent-Length: 70000\r\n\r\n")
    ok("oversized body refused", r.startswith(b"HTTP/1.1 413"), r[:40])
    # At most 6 connections from one address.
    socks = [socket.create_connection((HOST, HAP_PORT), timeout=2) for _ in range(8)]
    time.sleep(0.5)
    closed = 0
    for so in socks:
        so.settimeout(0.3)
        try:
            if so.recv(1) == b"":
                closed += 1
        except socket.timeout:
            pass
        except OSError:
            closed += 1
    for so in socks:
        so.close()
    ok("per-address cap on unverified connections", closed == 5, f"{closed} of 8 refused")
    time.sleep(0.5)

    azc = AsyncZeroconf()
    browser = AsyncServiceBrowser(azc.zeroconf, ["_hap._tcp.local.", "_hap._udp.local."], handlers=[lambda **kw: None])
    controller = Controller(async_zeroconf_instance=azc)
    await controller.async_start()
    try:
        # Discovery: find any _hap._tcp record of ours (on the port we run).
        disc = None
        deadline = time.time() + 30
        while time.time() < deadline and disc is None:
            async for d in controller.async_discover():
                if d.description.port == HAP_PORT:
                    disc = d
                    break
            if disc is None:
                await asyncio.sleep(1)
        ok("discovered over mDNS", disc is not None)
        if disc is None:
            return
        desc = disc.description
        print(f"  record: name={desc.name!r} id={desc.id} c#={desc.config_num} sf={int(desc.status_flags)} ci={int(desc.category)} addr={desc.address}:{desc.port}")
        ok("TXT: closed, not pairable (sf=0), bridge category", int(desc.status_flags) & 1 == 0 and int(desc.category) == 2, f"sf={int(desc.status_flags)}")
        device_id = desc.id

        # With pairing closed, pair-setup is refused, even with the right code.
        try:
            finish = await disc.async_start_pairing("closed")
            await finish(code)
            ok("closed: pairing refused", False, "it paired")
        except Exception as e:
            ok("closed: pairing refused", isinstance(e, UnavailableError), f"{type(e).__name__}: {e}")
        controller.pairings.pop("closed", None)

        # Open it: announced as pairable.
        st, _ = open_window()
        d1 = await wait_sf(controller, device_id, 1)
        ok("open: TXT sf=1", st == 200 and d1 is not None, f"status {st}")
        disc = d1 or disc

        # A wrong code is refused (and counted).
        finish = await disc.async_start_pairing("wrong")
        try:
            await finish("111-22-333" if code != "111-22-333" else "111-22-334")
            ok("wrong setup code refused", False)
        except AuthenticationError as e:
            ok("wrong setup code refused", True, type(e).__name__)
        except Exception as e:
            ok("wrong setup code refused", True, f"{type(e).__name__}: {e}")
        # (Everyone waits a moment after a wrong code.)
        await asyncio.sleep(1.5)

        # Pair with the right code.
        t0 = time.time()
        finish = await disc.async_start_pairing("test")
        pairing = await finish(code)
        ok("paired with the setup code", pairing is not None, f"{time.time() - t0:.1f}s")
        st, hk = api("GET", "/api/homekit")
        ok("pairing window closed once paired", hk.get("pairing") is None, str(hk.get("pairing")))

        accs = await pairing.list_accessories_and_characteristics()
        aids = [a["aid"] for a in accs]
        names = {}
        for a in accs:
            for s in a["services"]:
                for c in s["characteristics"]:
                    if c["type"].upper().startswith("00000023"):
                        names[a["aid"]] = c.get("value")
        print(f"  accessories: {names}")
        ok("bridge + exactly the HomeKit cameras", sorted(names.values()) == sorted(["AmberSight HK Test", "Bars", "Live", "Texted"]) and names.get(2) == "Bars", str(names))
        first_aids = sorted(aids)
        cam_services = [s["type"] for s in accs[aids.index(2)]["services"]]
        ok("camera has info, motion sensor, RTP stream service", any(t.upper().startswith("0000003E") for t in cam_services) and any(t.upper().startswith("00000085") for t in cam_services) and any(t.upper().startswith("00000110") for t in cam_services), str(cam_services))

        vals = await pairing.get_characteristics([(1, 5), (2, 5), (2, 9), (2, 10), (2, 16), (2, 2)])
        print(f"  read: {vals}")
        ok("read name, motion and status", vals[(2, 5)].get("value") == "Bars" and vals[(2, 9)].get("value") is False and vals[(2, 10)].get("value") in (True, False))
        ok("write-only Identify read gives an error status", vals[(2, 2)].get("status") == -70405, str(vals[(2, 2)]))
        r = await pairing.put_characteristics([(2, 2, True)])
        ok("identify write", r == {} or all(v.get("status", 0) == 0 for v in r.values()), str(r))
        r = await pairing.put_characteristics([(2, 9, True)])
        ok("write to read-only refused", r.get((2, 9), {}).get("status") == -70404, str(r))

        # Events: subscribe to MotionDetected and make motion.
        events = []
        pairing.dispatcher_connect(lambda ev: events.append((time.time(), ev)))
        await pairing.subscribe([(2, 9)])
        st, r = api("POST", "/api/clips/record", {"camera": 0, "seconds": 5})
        ok("triggered motion (manual clip)", st == 200 and r.get("recording"), str(r))
        t0 = time.time()
        on_at = off_at = None
        while time.time() - t0 < 40 and off_at is None:
            await asyncio.sleep(0.25)
            for t, ev in events:
                v = ev.get((2, 9), {}).get("value")
                if v is True and on_at is None:
                    on_at = t - t0
                if v is False and on_at is not None and off_at is None:
                    off_at = t - t0
        ok("motion event: started", on_at is not None, f"after {on_at:.1f}s" if on_at is not None else "")
        ok("motion event: ended", off_at is not None, f"after {off_at:.1f}s" if off_at is not None else "")

        img = await pairing.image(2, 320, 180)
        ok("snapshot via /resource, fitted to 320x180", img and img[:2] == b"\xff\xd8" and jpeg_size(img) == (320, 180), f"{len(img) if img else 0} bytes, {jpeg_size(img) if img else None}")
        img_big = await pairing.image(2, 1920, 1080)
        ok("snapshot not enlarged", img_big and jpeg_size(img_big) == (640, 360), str(jpeg_size(img_big) if img_big else None))
        img_none = await pairing.image(1, 320, 180)
        ok("no snapshot for the bridge", not img_none)

        # Stream setup is answered "busy" (no live video yet), without breaking the session.
        r = await pairing.put_characteristics([(2, 15, base64.b64encode(bytes([1, 21, 1, 16]) + bytes(16) + bytes([2, 1, 1])).decode())])
        ok("stream start without SetupEndpoints refused", r.get((2, 15), {}).get("status") == -70410, str(r))
        vals = await pairing.get_characteristics([(2, 9)])
        ok("session still fine after", (2, 9) in vals)

        # Pairing again while paired is refused.
        st, hk = api("GET", "/api/homekit")
        ok("API: paired, no setup code now", hk["paired"] and "setup_code" not in hk and len(hk["accessories"][0]["controllers"]) == 1, str(hk["accessories"][0]["controllers"]))
        sf = sf_now()
        ok("TXT: sf=0 once paired", sf == 0, f"sf={sf}")

        # List pairings, then remove ours (the only admin): unpaired, same code and identity.
        pl = await pairing.list_pairings()
        ok("list pairings", len(pl) == 1 and pl[0].get("controllerType") == "admin", str(pl))
        my_id = pairing.pairing_data["iOSPairingId"]
        await pairing.remove_pairing(my_id)
        await asyncio.sleep(1)
        st, hk = api("GET", "/api/homekit")
        ok("API: unpaired after removal, pairing still closed", not hk["paired"] and "setup_code" not in hk and hk.get("pairing") is None, str({k: hk.get(k) for k in ("paired", "pairing")}))
        try:
            await pairing.close()
        except Exception:
            pass
        controller.pairings.pop("test", None)
        await asyncio.sleep(2)
        # Pairs again with the same code, same identity, once pairing is opened again.
        try:
            sf = sf_now()
            ok("unpaired but closed: TXT sf=0", sf == 0, f"sf={sf}")
            st, hk = open_window()
            ok("opened again: the same code", st == 200 and hk.get("setup_code") == code, f"status {st}")
            d3 = await wait_sf(controller, device_id, 1)
            ok("TXT: sf=1 again, same id", d3 is not None and d3.description.id == device_id, f"sf={sf_now()}")
            finish = await d3.async_start_pairing("again")
            p3 = await finish(code)
            ok("pairs again with the same code", p3 is not None)
            await p3.remove_pairing(p3.pairing_data["iOSPairingId"])
            try:
                await p3.close()
            except Exception:
                pass
            controller.pairings.pop("again", None)
        except Exception as e:
            ok("pairs again with the same code", False, repr(e))

        st, hk = api("POST", "/api/homekit/reset", {})
        ok("reset", st == 200, f"status {st}")
        st, hk = open_window()
        new_code = hk.get("setup_code") if hk else None
        ok("reset: a new code once pairing is opened", st == 200 and new_code and new_code != code, f"status {st}")
        await asyncio.sleep(3)
        # A new identity: find the record on our port again.
        disc4 = None
        deadline = time.time() + 30
        while time.time() < deadline and disc4 is None:
            async for d in controller.async_discover():
                if d.description.port == HAP_PORT and d.description.id != device_id and int(d.description.status_flags) & 1:
                    disc4 = d
                    break
            if disc4 is None:
                await asyncio.sleep(1)
        ok("new identity advertised after reset", disc4 is not None)
        if disc4:
            finish = await disc4.async_start_pairing("after-reset")
            p2 = await finish(new_code)
            accs = await p2.list_accessories_and_characteristics()
            ok("pairs again after reset; cameras keep their aids", sorted(a["aid"] for a in accs) == first_aids)
            await p2.remove_pairing(p2.pairing_data["iOSPairingId"])
            try:
                await p2.close()
            except Exception:
                pass
        # Turning HomeKit off closes the port.
        st, hk = api("PUT", "/api/homekit", {"enabled": False})
        time.sleep(3)
        try:
            socket.create_connection((HOST, HAP_PORT), timeout=2).close()
            ok("off: port closed", False)
        except OSError:
            ok("off: port closed", st == 200 and not hk["listening"])
        st, hk = api("PUT", "/api/homekit", {"enabled": True})
        ok("on again", st == 200 and hk["listening"], str(hk.get("error")))
    finally:
        await controller.async_stop()
        await azc.async_close()

    failed = [n for n, c in results if not c]
    print(f"\n{len(results) - len(failed)} passed, {len(failed)} failed" + (": " + ", ".join(failed) if failed else ""))
    sys.exit(1 if failed else 0)


asyncio.run(main())
