# Apple Home (HomeKit): test checklist

A step list for the first round with a real iPhone and a home hub (HomePod mini or Apple TV).
AmberSight's HomeKit support has been tested with an iPhone and a HomePod mini hub (one camera
per accessory, on a Linux server); a round on your setup shows what differs there. Note the AmberSight version, the
iPhone's iOS version and the hub's software version first.

## Before you start

- AmberSight and the iPhone on the same network. In Settings, Connections, Apple Home: turn on
  **Apple Home (HomeKit)**, switch on one or two cameras (one with sound, if you have one).
- Logs: Settings, System, Logs (search for "HomeKit"). On Linux also `journalctl -u ambersight`;
  on Windows the log files in the data folder; in Docker `docker logs`; started by hand with
  `ambersight run`, its console. Each step below says what to look for.
- When something fails: a screenshot of the Home app's message, the time it happened, and the
  HomeKit lines of the log from that minute. Do not post the setup code or the QR code.

## 1. Pairing, each camera on its own (the default)

1. Before pressing anything: Home app, **+**, **Add Accessory**, **More options**. AmberSight's
   cameras should not be listed (pairing is closed). Cancel.
2. Settings, Apple Home: press **Pair a device** for the first camera. Its QR code and the setup
   code show, with the time left (5 minutes). Log: "pairing window opened".
3. Home app: **+**, **Add Accessory**, scan that QR code. While Home works on it, the Settings page
   shows "Pairing now" with the iPhone's address. Note what Home says about the accessory (it
   should say it is not certified; choose **Add Anyway**).
4. Once added, the page shows the camera "In Apple Home" and pairing is closed. Log: "paired with
   a controller; the pairing window is closed".
5. **Pair a device** for the second camera; add it with **More options** and the setup code typed
   in.
6. **Pair a device** for a camera, then **Stop**: the code and QR code go away, and Home (**More
   options**) no longer lists it within a few seconds.
7. **Pair a device**, start adding in the Home app, and while "Pairing now" shows the iPhone's
   address press **Stop and start again**: Home should fail to add it. Then that iPhone cannot
   add it again until the window closes; check that the page says so (the address is listed under
   "Stopped in this window"). Press **Stop**, then **Pair a device** again, and add it normally.
8. **Pair a device** and wait 5 minutes without adding: pairing closes by itself.

Note: how long adding took, any "accessory not responding" or "unable to add" message, and
whether the address shown under "Pairing now" was the iPhone's (or a hub's).

## 2. Tile and snapshot

1. The Home app shows a camera tile for each camera with a recent picture.
2. Leave the Home app open for a minute: the picture should refresh (every ~10 s).

Note: a black or gray tile, "No response", or a tile that never updates.

## 3. Live view, at home

1. Tap a camera tile. Live video should start within a few seconds.
2. Unmute (if the camera has sound): do you hear it?
3. Watch for a minute; then close the live view. In Settings, Apple Home, the camera's live view
   count goes back to 0 within a few seconds.
4. Open the same camera on two devices at once (iPhone and iPad/Mac): both should play. A third
   should be told the camera is busy.

5. A camera with burned-in text ("Also burn it into recordings"), if you use it: its live view
   shows the text. Areas you need hidden must be hidden with the camera's own privacy mask:
   AmberSight draws no masks.
6. Sound check: if the sound is garbled, choppy, too fast or slow, or missing while the camera has
   sound, note it exactly. (Sound is sent as Opus in 20 ms packets with the standard 48 kHz RTP
   clock; Home might expect something else.)
7. Lock the iPhone during a live view and unlock it: Home pauses ("suspends") the stream; AmberSight
   then ends it and Home should start a new one. Note any frozen picture after unlocking.

Log: "live stream WIDTHxHEIGHT at N fps ... on ENCODER" when it starts, "a live stream ended" when
it stops. Note: the size and encoder in that line, how long until the picture showed, stutter,
picture quality, sound, and any "the camera is busy" / "not responding".

After watching one camera for more than a minute, please send every log line that contains
"Apple Home live view" (it says whether AmberSight could check the reports Home sends back
while you watch; if it could not, and the view stops after about 30 s, say so).

## 4. Live view, away from home (through the hub)

1. Turn off Wi-Fi on the iPhone (mobile data only). Open a camera tile and tap it.
2. Video should play through the HomePod/Apple TV. It may be smaller than at home.

Note: whether it plays at all, how long it takes, the size in the log line.

## 5. Motion notification

1. Home app: the camera's settings, turn on notifications for motion (with a snapshot).
2. Walk in front of the camera. A notification with a picture should arrive.
3. Also check that the motion sensor in Home turns on and off.

Note: how long after the motion the notification came, whether it had a picture.

## 6. Remove and add again

1. Home app: remove one camera. Settings, Apple Home: it shows **Pair a device** again.
2. Press it and add the camera again with the same code. It should work without a reset.
3. Settings, Apple Home: **Remove pairings** on a camera. The Home app should show it as not
   responding; remove it there and add it again (after **Pair a device**).
4. Home app, Home settings, **Invite People**: share the home with a second Apple account. The
   other person sees the cameras without anyone pressing **Pair a device** (Home adds them from
   the iPhone that is already paired).

## 7. Reset

1. Settings, Apple Home, **Reset pairing**. A new setup code is made (shown after **Pair a
   device**).
2. The Home app's cameras stop responding. Remove them in the Home app, then add them with the new
   code, one **Pair a device** at a time.

## 8. Behind one bridge

1. Settings, Apple Home, **Cameras in Home**: choose **Behind one bridge**. Remove the old cameras
   in the Home app.
2. **Pair a device** for the bridge, and add it with its QR code. Home should offer all cameras
   in one go.
3. Repeat steps 2 and 3 (tile, snapshot, live view) for one camera.

Note: whether live view works as well as in standalone mode (Apple is known to prefer cameras on
their own).

## 9. Streaming status

While a camera's live view is open on two devices, a third device should see the camera as busy.
A camera that is offline in AmberSight should show as unavailable in Home. Note what Home shows
in each case (the camera's status is sent as "available", "in use" or "unavailable").

## What to send back

- For each step: works / does not work / partly, with the notes above.
- Screenshots of any error, with the time.
- The HomeKit log lines (Settings, System, Logs, search "HomeKit") for the minutes around each
  failure. They hold no keys or codes.
