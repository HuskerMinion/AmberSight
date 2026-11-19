# Camera guide

Stream addresses, ports and settings for common camera brands, and the problems we know of.

**How to read this guide.** Every entry is marked one of two ways:

- **Tested: \<model\>, firmware \<x\>** means we ran that camera with AmberSight ourselves. When a
  model or firmware wasn't written down at the time, the entry says "not recorded" instead of
  guessing.
- **Reported by users / from manufacturer documentation, not tested by us** means we collected it
  from the maker's own pages and from other open-source projects
  ([go2rtc](https://github.com/AlexxIT/go2rtc), [Frigate's camera notes](https://docs.frigate.video/configuration/camera_specific)).
  It may be wrong for your model or firmware. The sources are linked under each entry.

Firmware changes paths and defaults, so treat every address here as a starting point. When the
camera has ONVIF, **Settings > Cameras > Find cameras** asks it for its own stream addresses, which
beats anything in this table.

Examples use `192.168.1.50` for the camera and `user` / `pass` for its sign-in. In the camera's
Stream settings, the address, port and protocol, the **User name** and **Password**, and the main
and sub stream paths each have a field; the **Brand** list there fills the paths below. Put the
user name and password in their own fields, not in the address, unless the camera wants them in
the query string (some do; those entries say so, and need **Use full addresses**).

## When a camera doesn't work

1. Try the same address in VLC (Media > Open Network Stream). If VLC can't play it either, the
   address, the sign-in or a camera setting is wrong.
2. In AmberSight, open **Settings > Cameras > (the camera) > Network > Camera report**. It saves a
   text file with what AmberSight sees: the stream description (SDP) the camera sends, what ffmpeg
   finds in each stream, the ONVIF profiles and the camera's recent log lines. User names and
   passwords are removed from it. Attach it to a
   [GitHub issue](https://github.com/HuskerMinion/AmberSight/issues/new/choose).
3. AmberSight reads RTSP itself. A camera it can't play after a few tries is switched to ffmpeg
   automatically; the report says which reader is in use and why.
4. **Mac: every camera says "No route to host"** ("macOS blocked AmberSight from the local
   network"), while VLC or `nc -vz <camera address> 554` in Terminal reach it: macOS 15 and later
   block programs from the local network until you allow them. Turn on `ambersight` in **System
   Settings > Privacy & Security > Local Network**. If it is not in that list, see the macOS
   section of the [install guide](install.md#4-allow-access-to-the-local-network-macos-15-and-later).

## Quick table

| Brand | Main stream | Sub stream | Status |
|---|---|---|---|
| Reolink (wired, Wi-Fi) | `rtsp://192.168.1.50:554/Preview_01_main` | `.../Preview_01_sub` | Tested (see below) |
| Reolink battery (Argus 2, Argus 3 Pro) | `baichuan://192.168.1.50` | (one stream) | Tested |
| Amcrest | `rtsp://192.168.1.50:554/cam/realmonitor?channel=1&subtype=0` | `...&subtype=1` | Tested (IP8M-2496E-V2) |
| Kuna | from the camera's settings | | Tested (model not recorded) |
| Wyze (RTSP firmware) | `rtsp://192.168.1.50/live` | | Tested (model not recorded) |
| Hikvision | `rtsp://192.168.1.50:554/Streaming/Channels/101` | `.../102` | Not tested by us |
| Dahua, and brands that sell Dahua cameras (Lorex, Empiretech and others) | `rtsp://192.168.1.50:554/cam/realmonitor?channel=1&subtype=0` | `...&subtype=1` | Not tested by us |
| Axis | `rtsp://192.168.1.50/axis-media/media.amp` | stream profile or `?resolution=` | Not tested by us |
| Tapo / TP-Link | `rtsp://192.168.1.50:554/stream1` | `.../stream2` | Not tested by us |
| Uniview | `rtsp://192.168.1.50:554/media/video1` | `.../media/video2` | Not tested by us |
| Annke | see entry | | Not tested by us |
| Foscam | `rtsp://192.168.1.50:88/videoMain` | `.../videoSub` | Not tested by us |
| Ubiquiti UniFi Protect | copied from Protect | | Not tested by us |
| Eufy | copied from the Eufy app | | Not tested by us |

---

## Cameras we tested

### Reolink (wired and Wi-Fi)

**Tested:** Duo Floodlight PoE, Duo Floodlight WiFi v2, E1 Outdoor SE PoE, E1 Zoom, a Reolink
doorbell and the Reolink Home Hub with cameras on it. Firmware not recorded.

- **Paths:** main `/Preview_01_main`, sub `/Preview_01_sub`, port 554. The older spelling
  `/h264Preview_01_main` and `/h264Preview_01_sub` is widely used too, but Reolink's own page lists
  only `Preview_01_*`. On a Home
  Hub or NVR, channel N is `Preview_0N_main` (the second camera on a hub is `Preview_02_main`).
- **HTTP-FLV:** `http://192.168.1.50/flv?port=1935&app=bcs&stream=channel0_main.bcs&user=user&password=pass`
  (sub: `channel0_ext.bcs`). The sign-in goes in the query string here. AmberSight reads FLV itself
  and has no ffmpeg fallback for it. We use it for battery cameras on a Home Hub.
- **Turning RTSP and ONVIF on:** Reolink app or web page, Settings > Network > Advanced > Server
  Settings (in the app: More Features > Advanced Network Settings). On several models RTSP, RTMP and
  ONVIF are off out of the box. Cameras on a hub or NVR use the hub's settings and sign-in.
- **What we saw:**
  - On Windows, a Reolink camera that wasn't read for about 2 seconds cut packets short, and the
    stream went out of step ("RTSP framing error"). AmberSight now uses a larger receive buffer on
    camera connections.
  - The Duo Floodlight changes the timing fields of its H.264 settings (SPS) during the stream.
    AmberSight ignores changes that only touch timing, so recordings no longer split.
  - A Home Hub can stop answering for a while when many sessions to it are restarted at once.
- **Reolink's own protocol (Baichuan, TCP port 9000) instead of RTSP:** set the camera's
  **Connect with** (Stream settings, More settings; `connect` in the config) to:
  - **RTSP**: the default, as before.
  - **Reolink port 9000 (Baichuan)** (`connect = "baichuan"`): video and AAC audio come over
    Reolink's own protocol from the same address as the RTSP one. The RTSP addresses stay in the
    settings: their host gives the camera, `_main` / `_sub` (or `_ext`) the stream, and on a hub or
    NVR `Preview_0N` the channel (or set `reolink_channel` / `baichuan_channel`, 0-based). One
    session to the device carries all its streams. The port is 9000 unless `baichuan_port` says
    otherwise.
  - **Auto** (`connect = "auto"`): port 9000 first, RTSP as the backup. After port 9000 fails 3
    times in a row, or sends no video for 2 minutes, AmberSight switches to RTSP and says why
    ("Baichuan failed 3 times: ...; using RTSP"). On RTSP it tries port 9000 again every 30
    minutes, at the end of a recording segment, and goes back to it when it works. It never uses
    both at once. Find cameras picks Auto for a Reolink that answers on port 9000 (you can change
    it). Cameras without the setting stay on RTSP as before.
  - Reolink's RTSP timestamps jitter (on one sub stream, frame steps of 26 to 260 ms at about
    9 fps). AmberSight smooths them on RTSP (within 50 ms of the camera's own time), so
    recordings play evenly. Port 9000's timestamps are smooth already.
  - A wrong user name or password stops both ways and shows the error until the settings change:
    Reolink locks the account after a few failed sign-ins.
  - Not for battery cameras: they keep their on-demand path (`on_demand`, or a UID). On a Home Hub
    or NVR, AmberSight first asks the device's web API (`GetChannelstatus`, `GetAbility`) about the
    channel and streams it only when the hub shows it online, awake, and with no battery. An
    offline channel, or a hub whose web API is off, is refused too (it might be a battery camera).
  - Some cameras pick a session that is not AES-encrypted (BC/XOR). AmberSight accepts it, logs
    it once per session, and says so in the camera report. Use it on a trusted network only.
  - Not yet: ADPCM audio (some models send that instead of AAC), and controls over this session.
  - **Talk** (the Talk button) goes over this same session when the camera's video comes over
    Baichuan, or when a Reolink has no RTSP audio backchannel. The camera's talk format is read from
    it (16 kHz ADPCM on the cameras we asked). A talk opens on the first sound, not on silence, and is
    let go 2 s after the last sound, so the Reolink app can talk again. If the camera refuses (for
    example while the Reolink app is talking), AmberSight waits 5 s and tries again; video and
    recording go on. Not for battery cameras or hub battery channels. **Untested on a real camera
    so far** (tested against a simulated camera only).
  - **Tested** (2026-11-19, main and sub): Duo Floodlight PoE (H.265 4608x1728 main), E1 Outdoor SE
    PoE (H.265 3840x2160 main), E1 Pro (H.264), and a wired doorbell on a Home Hub (channel 1).
- **Sources:** our tests;
  [Reolink: Introduction to RTSP](https://support.reolink.com/hc/en-us/articles/900000630706-Introduction-to-RTSP/),
  [Reolink: port settings](https://support.reolink.com/hc/en-us/articles/900000621783-How-to-Configure-Reolink-Ports-Settings).

### Reolink battery cameras without a hub (Argus)

**Tested:** Argus 2, firmware 0112_262_237_24 and 0326_713_572_47. Argus 3 Pro, firmware not
recorded.

- **In the camera form:** pick **Reolink battery (Argus, no hub)** under Brand (Settings > Cameras >
  the camera > Stream). Type the camera's address and its **UID** (Reolink app > Device info: 16
  letters and digits). AmberSight writes the address as `baichuan://192.168.1.50`, keeps the UID in
  its own setting, and turns on **Battery camera** with an idle time of 30 seconds and motion-only
  recording (on a camera added before: **Use battery settings**). The camera's motion message
  always starts an event, whatever the camera's trigger setting. A `baichuan://192.168.1.50?uid=<UID>`
  address pasted into the address field works too (the UID goes into the UID field). In
  `config.toml` the UID is the `uid` setting (it wins) or the `?uid=` in the address.
- **Why not RTSP:** these cameras don't speak RTSP when no hub is used; AmberSight talks Reolink's
  own protocol to them. There is no ffmpeg fallback for these. If the camera refuses RTSP and
  nothing else works, this is the type to try.
- **Motion:** the camera announces motion to `pushx.reolink.com`. Pointing that name at AmberSight
  on your network (see "Battery Reolink cameras without a hub" in the
  [install guide](install.md#battery-reolink-cameras-without-a-hub-argus)) lets AmberSight wake it
  and record. **Push notifications must be on** for the camera in the Reolink app, or it sends
  nothing. If push won't turn on, turn it on before the DNS entry is in place, or restart the camera
  after turning it on.
  - While the camera is awake it sends no motion messages; AmberSight uses the picture's motion
    then. Anything asking for its picture (a viewer, `/api/snapshot`, a Home Assistant camera
    entity) keeps it awake for another idle time; the log line `waking up for ...` says what woke
    it.
- **What we saw:**
  - Firmware 0326 announced motion for every clip it saved to its card. Firmware 0112 announced
    about half of them.
  - Firmware 0112 refuses encrypted requests after sign-in; AmberSight uses whatever encryption the
    camera chose.
  - Keeping a session open keeps the camera awake and drains its battery. AmberSight signs out
    shortly after the picture is no longer wanted. An idle timeout of 30 seconds suits these cameras.
  - The card search these cameras' apps use (Baichuan messages 14/15 and 272) answered 400 on both
    firmware versions. Asking for a recording download from a moment on does work, and the first
    bytes name the recording's start and end, so AmberSight lists the card that way. Firmware 0112
    writes those times in UTC, firmware 0326 in the camera's local time; AmberSight works out which.
    A download window may not cross the camera's local midnight (the camera answers 400).
- **Missed events from the card:** recordings on the card that AmberSight has no event for are
  imported later as events marked "from card", without a notification once they are older than 5
  minutes (see the [install guide](install.md#battery-reolink-cameras-without-a-hub-argus)).
  **Untested** on a real camera: the listing and import are built from what the probes above showed.
- **Sources:** our tests.

### Kuna

**Tested:** model and firmware not recorded. The camera's RTSP server identified itself as live555.

- **Path:** take the address the camera's app or settings show.
- **What we saw:**
  - The camera sends AAC audio with an ADTS header on every frame, plus an empty frame before each
    one. MP4 can't hold either, so players refused the recordings. AmberSight now drops the empty
    frames and strips the headers.
  - Sessions ended after a few minutes while AmberSight sent no RTCP receiver reports. AmberSight
    sends them with every keepalive now. Settings changed on the camera at the same time, so we
    can't say which change fixed it.
- **Sources:** our tests.

### G.711 audio cameras whose RTP clock doesn't match their audio

**Tested:** two cameras at one of our sites, brand, model and firmware not recorded. They send G.711
(A-law) audio.

- **What we saw:** each packet holds 320 samples, but its RTP timestamp moves by 600. Played by the
  timestamps, the audio ran 1.875 times too long and some players stuttered. AmberSight now learns
  each camera's clock over its first seconds of audio and times audio by its samples when the clock
  doesn't match.
- Browsers can't play G.711, so AmberSight turns it into FLAC for live view and recordings. A
  browser without FLAC support shows the picture without sound.
- **Sources:** our tests.

### Wyze (RTSP firmware)

**Tested:** a Wyze PTZ model and one other Wyze camera, models and firmware not recorded.

- **Path:** `rtsp://192.168.1.50/live`, with the camera's own RTSP user name and password (set in
  the Wyze app).
- **What we saw:**
  - Its RTP timestamps aren't regular. AmberSight accepts them (permissive timestamps).
  - After the RTSP password was changed in the app, the camera closed port 554 until RTSP was turned
    on again in the app, or the camera was restarted. Before that, it sometimes reset the connection
    after a correct sign-in.
- **Sources:** our tests. For Wyze's own notes see below.

---

## Brands we have not tested

**Everything in this section is reported by users or taken from manufacturer documentation, not
tested by us.** If one of these works or doesn't for you, a GitHub issue with the camera report
helps us correct this page.

### Hikvision

- **Paths:** main `/Streaming/Channels/101`, sub `/Streaming/Channels/102`, port 554. The number is
  the channel then the stream (01 main, 02 sub). Some models offer a third stream at `103`.
- **Turning ONVIF on and making a user:** reportedly in the web page under Configuration > Network
  > Advanced Settings > Integration Protocol: enable ONVIF, then add a user (Media User, Operator or
  Administrator).
- **Reported quirks:** some newer models are said to need digest authentication with MD5 for RTSP.
  ONVIF is reportedly off by default after some firmware updates.
- **Sources:**
  [Hikvision: RTSP address](https://supportusa.hikvision.com/support/solutions/articles/17000129064),
  [Hikvision: enable ONVIF](https://supportusa.hikvision.com/support/solutions/articles/17000128730-how-do-i-enable-onvif-on-a-hikvision-camera-),
  [Frigate](https://docs.frigate.video/configuration/camera_specific).

### Dahua

Brands that sell Dahua cameras under their own name (Lorex, Empiretech and others) are reported to
use the same paths and the same ONVIF setup. We have not run any of them ourselves; treat what
follows as likely, not checked.

- **Paths:** main `/cam/realmonitor?channel=1&subtype=0`, sub `subtype=1` (`subtype=2` is a third
  stream on some models), port 554.
- **Turning ONVIF on and making a user:** reportedly System > Safety > System Service (ONVIF), with
  ONVIF users under the account settings. On some firmware the ONVIF user is separate from the
  admin account.
- **Find cameras says "ONVIF login refused":** the camera might have ONVIF turned off, or might want
  an ONVIF user of its own. Find cameras then asks for the stream above with the same login; when
  that works, the camera is added by its stream address with ONVIF events and PTZ off. You can also
  add it by hand: Settings > Cameras > Add, brand Dahua, address of the camera, your user and password.
- **Reported quirks:** Dahua's own page says the password can't contain `#`. On doorbells, a stream
  with the two-way audio track may interfere with the call button (go2rtc suggests leaving the
  backchannel off on the other streams).
- **Sources:** [Dahua wiki: RTSP via VLC](https://dahuawiki.com/Remote_Access/RTSP_via_VLC),
  [go2rtc RTSP notes](https://github.com/AlexxIT/go2rtc/blob/master/internal/rtsp/README.md),
  [Frigate](https://docs.frigate.video/configuration/camera_specific).

### Amcrest

**Tested:** two Amcrest IP8M-2496E-V2 cameras: ONVIF sign-in, the stream paths below, and Find
cameras. Amcrest cameras are a Dahua design.

- **Paths:** `/cam/realmonitor?channel=1&subtype=0` and `subtype=1`, port 554.
  Amcrest's smart-home line is documented with `&authbasic=64` added.
- **Reported quirks:** on Amcrest doorbells, a stream with two-way audio may break the call button
  (go2rtc).
- **Sources:** our tests;
  [Amcrest: RTSP](https://support.amcrest.com/hc/en-us/articles/360059019972-Accessing-Amcrest-Products-Using-RTSP),
  [Amcrest smart home: RTSP](https://support.amcrest.com/hc/en-us/articles/360058619531-Accessing-Amcrest-Smart-Home-Products-Using-RTSP),
  [go2rtc RTSP notes](https://github.com/AlexxIT/go2rtc/blob/master/internal/rtsp/README.md).

### Axis

- **Path:** `/axis-media/media.amp`, port 554. There is no fixed sub stream path: choose a stream
  profile (`?streamprofile=<name>`) or a size (`?resolution=640x360`). `videocodec=h264` or `h265`
  picks the codec.
- **User:** the RTSP user needs Operator or Administrator rights. On recent firmware, ONVIF works
  only after an ONVIF user is made (System > ONVIF).
- **Sources:**
  [Axis: stream profiles](https://developer.axis.com/video-streaming-and-recording/video-streaming/how-to-guides/stream-profiles/),
  [Axis: initial device access changes (PDF)](https://www.axis.com/dam/public/86/91/02/initial-device-access-changes-en-US-115245.pdf).

### Tapo / TP-Link

- **Paths:** main `/stream1`, sub `/stream2`, port 554. ONVIF port 2020. Dual-lens models put the
  second lens on `/stream6` and `/stream7`.
- **Making the user:** Tapo app, the camera > Settings > Advanced Settings > Camera Account. RTSP
  uses this camera account, not your Tapo cloud sign-in.
- **Reported quirks:** TP-Link says most battery models have no RTSP, and some doorbells offer it
  only when wired. ONVIF on Tapo is Profile S only (no two-way audio).
- **Sources:** [Tapo FAQ: RTSP and ONVIF](https://www.tapo.com/en/faq/34/),
  [go2rtc Tapo notes](https://github.com/AlexxIT/go2rtc/blob/master/internal/tapo/README.md).

### Uniview

- **Paths:** main `/media/video1`, sub `/media/video2`, third stream `/media/video3`, port 554.
- **Sign-in:** needed in the address only when RTSP authentication is on in the camera.
- **Source:**
  [Uniview: how to get the RTSP stream (PDF)](https://www.uniview.com/res/202310/26/20231026_1890310_How%20to%20Get%20a%20Uniview%20Camera's%20RTSP%20Stream_974039_168459_0.pdf).

### Annke

- **Paths:** no official page found. Frigate's example for the Annke C800 uses
  `/H264/ch1/main/av_stream`. Annke cameras are often said to be Hikvision-compatible
  (`/Streaming/Channels/101`), which we could not confirm.
- **Reported quirks:** the C800 sends H.265 only, which some browsers can't play without converting.
- **Source:** [Frigate](https://docs.frigate.video/configuration/camera_specific).

### Foscam

**Not tested by us.** Foscam documents these paths:

- **Paths:** main `/videoMain`, sub `/videoSub`. On most of these models RTSP uses the camera's
  HTTP port (88 by default); Foscam lists a few models (C1, C1 Lite, FI9800P) on port 554.
- **Source:** [Foscam FAQ](https://www.foscam.com/faqs/view.html?id=81).

### Ubiquiti UniFi Protect

- **Address:** turn RTSP on per camera in Protect (camera settings, Advanced) and copy the address
  it shows, for example `rtsps://192.168.1.1:7441/<token>?enableSrtp`. The token in the address is
  the sign-in; no user is made.
- **Reported quirks:** other tools drop `?enableSrtp`. Plain RTSP on port 7447 (without the second
  "s") is reported to work on some Protect versions and not on others. Newer cameras reportedly need
  a Protect console for this.
- **Sources:**
  [Ubiquiti: required ports](https://help.ui.com/hc/en-us/articles/218506997-Required-Ports-Reference),
  [go2rtc RTSP notes](https://github.com/AlexxIT/go2rtc/blob/master/internal/rtsp/README.md),
  [Frigate](https://docs.frigate.video/configuration/camera_specific).

### Eufy

- **Address:** in the Eufy app, the camera's settings > NAS (RTSP); choose continuous streaming and
  copy the address the app makes. The app makes the user name and password too.
- **Reported quirks:** only some models have this setting (battery models generally don't). In
  event mode the camera streams only during an event. Eufy lists a limit of 4 viewers at once.
- **Sources:**
  [Eufy: NAS (RTSP) setup](https://service.eufy.com/article-description/Device-NAS-RTSP-Configuration-Guide),
  [Eufy: which cameras support RTSP](https://service.eufy.com/article-description/Does-the-camera-support-RTSP-NAS11).

### Wyze (what Wyze and others document)

- **Firmware:** Wyze offers RTSP as separate beta firmware for some models, and says it may block
  newer app features. Wyze describes it as not committed to further updates.
- **Turning it on:** Wyze app, the camera > Settings > Advanced Settings > RTSP: set a user name
  and password, then generate the address.
- **Without RTSP firmware:** [docker-wyze-bridge](https://github.com/mrlt8/docker-wyze-bridge) or
  go2rtc's Wyze source can turn a stock Wyze camera into an RTSP stream that AmberSight reads.
- **Sources:**
  [Wyze: RTSP firmware](https://support.wyze.com/hc/en-us/articles/360026245231-Wyze-Cam-RTSP),
  [go2rtc Wyze notes](https://github.com/AlexxIT/go2rtc/blob/master/internal/wyze/README.md).

---

## Ports at a glance

These are common defaults. Your camera may use others; its web page or app shows them.

| Port | Used for |
|---|---|
| 554 | RTSP on most cameras |
| 80 | camera web page; ONVIF on many cameras |
| 88 | RTSP and web page on many older Foscam models (from Foscam's FAQ) |
| 1935 | RTMP, and the port named in Reolink's HTTP-FLV address |
| 2020 | ONVIF on Tapo (from TP-Link's FAQ) |
| 7441 / 7447 | RTSPS / RTSP on UniFi Protect (from Ubiquiti's port list) |
| 8000 | ONVIF on many Reolink cameras (reported, not confirmed in our notes) |
| 9000 | Reolink's own protocol (battery cameras over `baichuan://`; always-on cameras with Connect with = Baichuan or Auto) |
