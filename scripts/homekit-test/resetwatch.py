"""How long after Reset (or HomeKit off and on) the new record is seen over mDNS.

usage: python resetwatch.py <web base url> <admin user> <password> <hap port> [reset|toggle]
"""
import sys, time, json, base64, urllib.request
from zeroconf import Zeroconf, ServiceBrowser, ServiceStateChange
base, user, pw, port = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]); action = sys.argv[5] if len(sys.argv) > 5 else "reset"
def api(m, p, b=None):
    r = urllib.request.Request(base + p, method=m, data=None if b is None else json.dumps(b).encode())
    r.add_header("Authorization", "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode()); r.add_header("Content-Type", "application/json")
    return json.loads(urllib.request.urlopen(r, timeout=20).read() or b"null")
ev = []
t0 = [time.time()]
def h(zeroconf, service_type, name, state_change):
    ev.append((time.time() - t0[0], name, state_change.name))
zc = Zeroconf()
ServiceBrowser(zc, "_hap._tcp.local.", handlers=[h])
time.sleep(5)
ours = [e for e in ev if e[1].startswith("AmberSight HK Test")]
print("before:", ours)
t0[0] = time.time(); n0 = len(ev)
if action == "reset":
    api("POST", "/api/homekit/reset", {})
else:
    api("PUT", "/api/homekit", {"enabled": False}); time.sleep(3); api("PUT", "/api/homekit", {"enabled": True})
time.sleep(40)
for e in ev[n0:]:
    if e[1].startswith("AmberSight HK Test"):
        i = zc.get_service_info("_hap._tcp.local.", e[1], 1500) if e[2] != "Removed" else None
        print(f"{e[0]:6.1f}s {e[2]} {e[1]} port={i.port if i else '?'}")
zc.close()
