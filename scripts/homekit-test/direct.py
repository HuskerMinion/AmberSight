"""Pair by address, without mDNS (for example with Docker's bridge network), take a snapshot, unpair.

usage: python direct.py <web base url> <admin user> <password> <hap host> <hap port>
"""
import asyncio, base64, json, sys, urllib.request
from zeroconf.asyncio import AsyncZeroconf, AsyncServiceBrowser
from aiohomekit import Controller
from aiohomekit.zeroconf import HomeKitService
from aiohomekit.model import Categories
from aiohomekit.model.feature_flags import FeatureFlags
from aiohomekit.model.status_flags import StatusFlags
from aiohomekit.controller.ip.discovery import IpDiscovery
base, user, pw, host, port = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
def api(m, p, b=None):
    r = urllib.request.Request(base + p, method=m, data=None if b is None else json.dumps(b).encode())
    r.add_header("Authorization", "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode()); r.add_header("Content-Type", "application/json")
    return json.loads(urllib.request.urlopen(r, timeout=20).read() or b"null")
async def main():
    # Open the pairing window first (Settings, Pair a device; bridged): pair-setup is refused without it.
    code = api("POST", "/api/homekit/pairing", {"action": "open"})["setup_code"]
    azc = AsyncZeroconf(interfaces=["127.0.0.1"])
    AsyncServiceBrowser(azc.zeroconf, ["_hap._tcp.local.", "_hap._udp.local."], handlers=[lambda **kw: None])
    c = Controller(async_zeroconf_instance=azc)
    await c.async_start()
    desc = HomeKitService(name="direct", id="00:00:00:00:00:01", model="x", feature_flags=FeatureFlags(0), status_flags=StatusFlags(1), config_num=1, state_num=1, category=Categories.BRIDGE, protocol_version="1.1", type="_hap._tcp.local.", address=host, addresses=[host], port=port)
    d = IpDiscovery(c, desc)
    finish = await d.async_start_pairing("direct")
    p = await finish(code)
    accs = await p.list_accessories_and_characteristics()
    print("PASS direct pair; aids", [a["aid"] for a in accs])
    img = await p.image(2, 320, 180)
    print("PASS snapshot" if img and img[:2] == b"\xff\xd8" else "FAIL snapshot", len(img or b""))
    await p.remove_pairing(p.pairing_data["iOSPairingId"])
    await p.close()
    await c.async_stop(); await azc.async_close()
asyncio.run(main())
