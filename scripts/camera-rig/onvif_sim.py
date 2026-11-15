#!/usr/bin/env python3
"""Virtual ONVIF cameras for the camera rig.

Each camera listens on its own HTTP port and answers the Device, Media,
Events (pull point) and PTZ calls AmberSight makes. One WS-Discovery
responder answers probes for all of them, but only probes sent from this
machine, so other computers on the network never see the virtual cameras.

Calls are checked with WS-Security UsernameToken (PasswordDigest) and logged
to a JSON-lines file. Motion goes on for 5 s every 15 s on cameras with events.

  onvif_sim.py --cams cams.json --log onvif.jsonl

cams.json: [{"port": 18001, "name": "RigCam-1", "user": "u", "password": "p",
             "ptz": true, "events": true,
             "profiles": [{"token": "main", "w": 1280, "h": 720, "enc": "H264", "uri": "rtsp://..."}, ...]}]
"""
import argparse
import base64
import datetime
import hashlib
import http.server
import json
import re
import socket
import struct
import threading
import time
import uuid
import xml.etree.ElementTree as ET

LOG = None
LOCK = threading.Lock()


def log(**kv):
    kv["t"] = round(time.time(), 3)
    with LOCK:
        LOG.write(json.dumps(kv) + "\n")
        LOG.flush()


NS = {
    "s": "http://www.w3.org/2003/05/soap-envelope",
    "wsse": "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd",
    "wsu": "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd",
}
ENV_NS = (
    'xmlns:s="http://www.w3.org/2003/05/soap-envelope" '
    'xmlns:tds="http://www.onvif.org/ver10/device/wsdl" '
    'xmlns:trt="http://www.onvif.org/ver10/media/wsdl" '
    'xmlns:tt="http://www.onvif.org/ver10/schema" '
    'xmlns:tev="http://www.onvif.org/ver10/events/wsdl" '
    'xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" '
    'xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" '
    'xmlns:wsa5="http://www.w3.org/2005/08/addressing" '
    'xmlns:tns1="http://www.onvif.org/ver10/topics" '
    'xmlns:ter="http://www.onvif.org/ver10/error"'
)


def envelope(body):
    return f'<?xml version="1.0" encoding="UTF-8"?><s:Envelope {ENV_NS}><s:Body>{body}</s:Body></s:Envelope>'


def fault(code, sub, text):
    return envelope(
        f"<s:Fault><s:Code><s:Value>s:{code}</s:Value><s:Subcode><s:Value>{sub}</s:Value></s:Subcode></s:Code>"
        f'<s:Reason><s:Text xml:lang="en">{text}</s:Text></s:Reason></s:Fault>'
    )


def iso(t):
    return datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def motion_now(t):
    return (int(t) % 15) < 5


class Cam:
    def __init__(self, c):
        self.__dict__.update(c)
        self.uuid = str(uuid.uuid5(uuid.NAMESPACE_URL, "camrig:" + c["name"]))
        self.subs = {}
        self.next_sub = 1
        self.presets = {"1": "Gate", "2": "Drive"}

    def base(self, host):
        return f"http://{host}:{self.port}"

    def auth_ok(self, root):
        tok = root.find(".//wsse:UsernameToken", NS)
        if tok is None:
            return False
        user = tok.findtext("wsse:Username", "", NS)
        pw = tok.find("wsse:Password", NS)
        nonce = tok.findtext("wsse:Nonce", "", NS)
        created = tok.findtext("wsu:Created", "", NS)
        if user != self.user or pw is None:
            return False
        if "PasswordDigest" in (pw.get("Type") or ""):
            want = base64.b64encode(hashlib.sha1(base64.b64decode(nonce) + created.encode() + self.password.encode()).digest()).decode()
            return pw.text == want
        return pw.text == self.password

    def profiles_xml(self):
        out = []
        for p in self.profiles:
            ptz = ""
            if self.ptz and p["token"] == self.profiles[0]["token"]:
                ptz = ('<tt:PTZConfiguration token="ptz0"><tt:Name>PTZ</tt:Name><tt:UseCount>1</tt:UseCount>'
                       "<tt:NodeToken>node0</tt:NodeToken></tt:PTZConfiguration>")
            out.append(
                f'<trt:Profiles token="{p["token"]}" fixed="true"><tt:Name>{p["token"]}</tt:Name>'
                f'<tt:VideoSourceConfiguration token="vsc0"><tt:Name>vsc</tt:Name><tt:UseCount>2</tt:UseCount>'
                f'<tt:SourceToken>vs0</tt:SourceToken><tt:Bounds x="0" y="0" width="{self.profiles[0]["w"]}" height="{self.profiles[0]["h"]}"/></tt:VideoSourceConfiguration>'
                f'<tt:VideoEncoderConfiguration token="venc_{p["token"]}"><tt:Name>venc</tt:Name><tt:UseCount>1</tt:UseCount>'
                f'<tt:Encoding>{p["enc"]}</tt:Encoding><tt:Resolution><tt:Width>{p["w"]}</tt:Width><tt:Height>{p["h"]}</tt:Height></tt:Resolution>'
                f"<tt:Quality>5</tt:Quality></tt:VideoEncoderConfiguration>{ptz}</trt:Profiles>"
            )
        return "".join(out)

    def handle(self, op, el, root, host):
        b = self.base(host)
        if op == "GetSystemDateAndTime":
            n = datetime.datetime.now(datetime.timezone.utc)
            return ("<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:DateTimeType>NTP</tt:DateTimeType>"
                    "<tt:DaylightSavings>false</tt:DaylightSavings><tt:UTCDateTime>"
                    f"<tt:Time><tt:Hour>{n.hour}</tt:Hour><tt:Minute>{n.minute}</tt:Minute><tt:Second>{n.second}</tt:Second></tt:Time>"
                    f"<tt:Date><tt:Year>{n.year}</tt:Year><tt:Month>{n.month}</tt:Month><tt:Day>{n.day}</tt:Day></tt:Date>"
                    "</tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>")
        if not self.auth_ok(root):
            return None
        if op == "GetDeviceInformation":
            return (f"<tds:GetDeviceInformationResponse><tds:Manufacturer>CamRig</tds:Manufacturer><tds:Model>{self.name}</tds:Model>"
                    f"<tds:FirmwareVersion>1.0-rig</tds:FirmwareVersion><tds:SerialNumber>RIG{self.port}</tds:SerialNumber>"
                    "<tds:HardwareId>rig</tds:HardwareId></tds:GetDeviceInformationResponse>")
        if op == "GetCapabilities":
            ev = f"<tt:Events><tt:XAddr>{b}/onvif/events_service</tt:XAddr><tt:WSSubscriptionPolicySupport>false</tt:WSSubscriptionPolicySupport><tt:WSPullPointSupport>true</tt:WSPullPointSupport><tt:WSPausableSubscriptionManagerInterfaceSupport>false</tt:WSPausableSubscriptionManagerInterfaceSupport></tt:Events>" if self.events else ""
            ptz = f"<tt:PTZ><tt:XAddr>{b}/onvif/ptz_service</tt:XAddr></tt:PTZ>" if self.ptz else ""
            return (f"<tds:GetCapabilitiesResponse><tds:Capabilities><tt:Device><tt:XAddr>{b}/onvif/device_service</tt:XAddr></tt:Device>"
                    f"{ev}<tt:Media><tt:XAddr>{b}/onvif/media_service</tt:XAddr></tt:Media>{ptz}</tds:Capabilities></tds:GetCapabilitiesResponse>")
        if op == "GetServices":
            svcs = [("http://www.onvif.org/ver10/device/wsdl", "device_service"), ("http://www.onvif.org/ver10/media/wsdl", "media_service")]
            if self.events:
                svcs.append(("http://www.onvif.org/ver10/events/wsdl", "events_service"))
            if self.ptz:
                svcs.append(("http://www.onvif.org/ver20/ptz/wsdl", "ptz_service"))
            return "<tds:GetServicesResponse>" + "".join(
                f"<tds:Service><tds:Namespace>{ns}</tds:Namespace><tds:XAddr>{b}/onvif/{p}</tds:XAddr>"
                "<tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version></tds:Service>" for ns, p in svcs) + "</tds:GetServicesResponse>"
        if op == "GetProfiles":
            return f"<trt:GetProfilesResponse>{self.profiles_xml()}</trt:GetProfilesResponse>"
        if op == "GetStreamUri":
            tok = el.findtext(".//{http://www.onvif.org/ver10/media/wsdl}ProfileToken", "")
            p = next((p for p in self.profiles if p["token"] == tok), None)
            if p is None:
                return "FAULT:ter:NoProfile"
            uri = p["uri"].replace("{host}", host)
            return (f"<trt:GetStreamUriResponse><trt:MediaUri><tt:Uri>{uri.replace('&', '&amp;')}</tt:Uri>"
                    "<tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>"
                    "<tt:Timeout>PT0S</tt:Timeout></trt:MediaUri></trt:GetStreamUriResponse>")
        if op == "GetSnapshotUri":
            return "FAULT:ter:ActionNotSupported"
        if op == "GetEventProperties" and self.events:
            return ("<tev:GetEventPropertiesResponse><tev:TopicNamespaceLocation>http://www.onvif.org/onvif/ver10/topics/topicns.xml</tev:TopicNamespaceLocation>"
                    "<wsnt:FixedTopicSet>true</wsnt:FixedTopicSet><wstop:TopicSet xmlns:wstop=\"http://docs.oasis-open.org/wsn/t-1\">"
                    "<tns1:RuleEngine><CellMotionDetector><Motion wstop:topic=\"true\"/></CellMotionDetector></tns1:RuleEngine></wstop:TopicSet>"
                    "</tev:GetEventPropertiesResponse>")
        if op == "CreatePullPointSubscription" and self.events:
            sid = self.next_sub
            self.next_sub += 1
            self.subs[sid] = {"sent": None}
            now = time.time()
            return (f"<tev:CreatePullPointSubscriptionResponse><tev:SubscriptionReference><wsa5:Address>{b}/onvif/subscription?id={sid}</wsa5:Address>"
                    f"</tev:SubscriptionReference><wsnt:CurrentTime>{iso(now)}</wsnt:CurrentTime><wsnt:TerminationTime>{iso(now + 60)}</wsnt:TerminationTime>"
                    "</tev:CreatePullPointSubscriptionResponse>")
        if op == "PullMessages" and self.events:
            return self.pull(root)
        if op in ("Renew",):
            now = time.time()
            return f"<wsnt:RenewResponse><wsnt:TerminationTime>{iso(now + 60)}</wsnt:TerminationTime><wsnt:CurrentTime>{iso(now)}</wsnt:CurrentTime></wsnt:RenewResponse>"
        if op == "Unsubscribe":
            return "<wsnt:UnsubscribeResponse/>"
        if self.ptz and op == "GetNodes":
            return ('<tptz:GetNodesResponse><tptz:PTZNode token="node0" FixedHomePosition="false"><tt:Name>node0</tt:Name><tt:SupportedPTZSpaces>'
                    "<tt:ContinuousPanTiltVelocitySpace><tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace</tt:URI>"
                    "<tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange><tt:YRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:YRange></tt:ContinuousPanTiltVelocitySpace>"
                    "<tt:ContinuousZoomVelocitySpace><tt:URI>http://www.onvif.org/ver10/tptz/ZoomSpaces/VelocityGenericSpace</tt:URI>"
                    "<tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange></tt:ContinuousZoomVelocitySpace>"
                    "<tt:RelativePanTiltTranslationSpace><tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/TranslationGenericSpace</tt:URI>"
                    "<tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange><tt:YRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:YRange></tt:RelativePanTiltTranslationSpace>"
                    "</tt:SupportedPTZSpaces><tt:MaximumNumberOfPresets>16</tt:MaximumNumberOfPresets><tt:HomeSupported>true</tt:HomeSupported>"
                    "</tptz:PTZNode></tptz:GetNodesResponse>")
        if self.ptz and op == "GetPresets":
            return "<tptz:GetPresetsResponse>" + "".join(
                f'<tptz:Preset token="{t}"><tt:Name>{n}</tt:Name></tptz:Preset>' for t, n in self.presets.items()) + "</tptz:GetPresetsResponse>"
        if self.ptz and op == "SetPreset":
            t = str(len(self.presets) + 1)
            self.presets[t] = el.findtext(".//{http://www.onvif.org/ver20/ptz/wsdl}PresetName", "p")
            return f"<tptz:SetPresetResponse><tptz:PresetToken>{t}</tptz:PresetToken></tptz:SetPresetResponse>"
        if self.ptz and op == "GetStatus":
            return ('<tptz:GetStatusResponse><tptz:PTZStatus><tt:Position><tt:PanTilt x="0" y="0"/><tt:Zoom x="0"/></tt:Position>'
                    f"<tt:MoveStatus><tt:PanTilt>IDLE</tt:PanTilt><tt:Zoom>IDLE</tt:Zoom></tt:MoveStatus><tt:UtcTime>{iso(time.time())}</tt:UtcTime>"
                    "</tptz:PTZStatus></tptz:GetStatusResponse>")
        if self.ptz and op in ("ContinuousMove", "Stop", "GotoPreset", "RelativeMove", "AbsoluteMove", "RemovePreset", "GotoHomePosition"):
            return f"<tptz:{op}Response/>"
        return "FAULT:ter:ActionNotSupported"

    def pull(self, root):
        # Long-poll: answer when the motion state differs from what this
        # subscription last got, or after 2 s with nothing.
        to = root.find(".//{http://www.w3.org/2005/08/addressing}To")
        sid = 0
        if to is not None and to.text and "id=" in to.text:
            sid = int(re.sub(r"\D", "", to.text.split("id=")[1]) or 0)
        sub = self.subs.setdefault(sid, {"sent": None})
        deadline = time.time() + 2
        msgs = ""
        while time.time() < deadline:
            m = motion_now(time.time())
            if sub["sent"] != m:
                op = "Initialized" if sub["sent"] is None else "Changed"
                sub["sent"] = m
                log(cam=self.name, event="motion", value=m)
                msgs = ("<wsnt:NotificationMessage><wsnt:Topic Dialect=\"http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet\">"
                        "tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message>"
                        f'<tt:Message UtcTime="{iso(time.time())}" PropertyOperation="{op}"><tt:Source>'
                        '<tt:SimpleItem Name="VideoSourceConfigurationToken" Value="vsc0"/><tt:SimpleItem Name="Rule" Value="MyMotionDetectorRule"/>'
                        f'</tt:Source><tt:Data><tt:SimpleItem Name="IsMotion" Value="{"true" if m else "false"}"/></tt:Data></tt:Message>'
                        "</wsnt:Message></wsnt:NotificationMessage>")
                break
            time.sleep(0.2)
        now = time.time()
        return f"<tev:PullMessagesResponse><tev:CurrentTime>{iso(now)}</tev:CurrentTime><tev:TerminationTime>{iso(now + 60)}</tev:TerminationTime>{msgs}</tev:PullMessagesResponse>"


def make_handler(cam):
    class H(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):
            pass

        def do_POST(self):
            n = int(self.headers.get("Content-Length", "0") or 0)
            body = self.rfile.read(n)
            host = (self.headers.get("Host") or "127.0.0.1").rsplit(":", 1)[0]
            try:
                root = ET.fromstring(body)
                b = root.find("s:Body", NS)
                el = next(iter(b)) if b is not None and len(b) else None
                op = el.tag.split("}")[-1] if el is not None else ""
            except ET.ParseError:
                root, el, op = None, None, ""
            if root is None or not op:
                return self.send(400, fault("Sender", "ter:WellFormed", "bad request"))
            r = cam.handle(op, el, root, host)
            log(cam=cam.name, op=op, path=self.path, ok=r is not None and not str(r).startswith("FAULT"))
            if r is None:
                return self.send(400, fault("Sender", "ter:NotAuthorized", "Sender not Authorized"))
            if r.startswith("FAULT:"):
                return self.send(400, fault("Sender", r[6:], "not supported"))
            self.send(200, envelope(r))

        def do_GET(self):
            self.send(405, "")

        def send(self, code, text):
            data = text.encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/soap+xml; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    return H


def local_ips():
    """This machine's IPv4 addresses (the LOCAL entries of the routing trie)."""
    ips = {"127.0.0.1"}
    last = None
    try:
        for line in open("/proc/net/fib_trie"):
            m = re.search(r"\|-- (\d+\.\d+\.\d+\.\d+)", line)
            if m:
                last = m.group(1)
            elif "/32 host LOCAL" in line and last:
                ips.add(last)
    except OSError:
        pass
    return ips


def discovery(cams, allowed):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
    except OSError:
        pass
    s.bind(("0.0.0.0", 3702))
    for ip in allowed:
        if ip.startswith("127.") or ":" in ip:
            continue
        try:
            s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton("239.255.255.250") + socket.inet_aton(ip))
        except OSError:
            pass
    log(event="discovery listening", ifaces=sorted(allowed))
    while True:
        data, addr = s.recvfrom(65536)
        if addr[0] not in allowed:
            continue  # only this machine's own probes
        text = data.decode("utf-8", "replace")
        if "Probe" not in text or "ProbeMatches" in text:
            continue
        mid = re.search(r"MessageID[^>]*>([^<]+)<", text)
        relates = mid.group(1) if mid else ""
        log(event="probe", sender=addr[0])
        out = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            out.bind((addr[0], 0))
        except OSError:
            pass
        for c in cams:
            msg = (
                '<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" '
                'xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" '
                'xmlns:dn="http://www.onvif.org/ver10/network/wsdl" xmlns:tds="http://www.onvif.org/ver10/device/wsdl">'
                f"<s:Header><a:MessageID>uuid:{uuid.uuid4()}</a:MessageID><a:RelatesTo>{relates}</a:RelatesTo>"
                "<a:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:To>"
                "<a:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</a:Action></s:Header>"
                f"<s:Body><d:ProbeMatches><d:ProbeMatch><a:EndpointReference><a:Address>urn:uuid:{c.uuid}</a:Address></a:EndpointReference>"
                "<d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types>"
                f"<d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/{c.name} "
                f"onvif://www.onvif.org/hardware/CamRig onvif://www.onvif.org/location/rig</d:Scopes>"
                f"<d:XAddrs>http://{addr[0]}:{c.port}/onvif/device_service</d:XAddrs><d:MetadataVersion>1</d:MetadataVersion>"
                "</d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>"
            )
            out.sendto(msg.encode(), addr)
        out.close()


def main():
    global LOG
    p = argparse.ArgumentParser()
    p.add_argument("--cams", required=True)
    p.add_argument("--log", required=True)
    a = p.parse_args()
    LOG = open(a.log, "a")
    cams = [Cam(c) for c in json.load(open(a.cams))]
    for c in cams:
        srv = http.server.ThreadingHTTPServer(("0.0.0.0", c.port), make_handler(c))
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        log(event="listening", cam=c.name, port=c.port)
    discovery(cams, local_ips())


if __name__ == "__main__":
    main()
