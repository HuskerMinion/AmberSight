"""List the _hap._tcp records on the network (name, host, port, addresses, sf/ci/id/md).

usage: python browse.py [seconds]
"""
import sys, time
from zeroconf import Zeroconf, ServiceBrowser, ServiceStateChange
secs = float(sys.argv[1]) if len(sys.argv) > 1 else 10
found = {}
zc = Zeroconf()
def h(zeroconf, service_type, name, state_change):
    if state_change in (ServiceStateChange.Added, ServiceStateChange.Updated):
        found[name] = True
ServiceBrowser(zc, "_hap._tcp.local.", handlers=[h])
time.sleep(secs)
for n in found:
    i = zc.get_service_info("_hap._tcp.local.", n, 3000)
    if i:
        print(n, i.server, i.port, i.parsed_addresses(), {k.decode(): (v or b"").decode() for k, v in i.properties.items() if k in (b"sf", b"ci", b"id", b"md")})
    else:
        print(n, "(no info)")
zc.close()
