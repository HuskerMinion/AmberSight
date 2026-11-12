# AmberSight Capture helper: protocol (v1)

The capture helper runs on another computer (or on the server itself, in the
signed-in user's session) and sends what is on its screen to AmberSight. It is
the same program as the server: `ambersight capture`.

The helper always connects out to the server. Nothing listens on the helper
machine.

## Pairing

1. An admin opens Settings > Cameras > Add camera > Source: Another computer (or
   Settings > Capture helpers) and presses "Add a computer". The server makes a
   one-time code: `POST /api/helpers/pair` returns
   `{"code": "...", "expires_in": 900, "server": "https://host:port"}`.
2. On the other computer the user downloads the helper from
   `GET /helper/download/ambersight.exe?code=<code>` (and, on Windows, ffmpeg from
   `GET /helper/download/ffmpeg.exe?code=<code>` when the server has one). Both
   downloads need a code that is still valid.
   On the computer that also runs the server (Windows), the helper runs from the
   program folder in the signed-in user's own session, so that folder keeps a
   read-and-run entry for ordinary accounts on `ambersight.exe`, its libraries and
   `ffmpeg.exe`, and nothing else (see "Who can open AmberSight's folders" in
   install.md). If the service uses the ffmpeg it downloaded into its private data
   folder, it also grants Users read-and-run on `ffmpeg.exe` and `ffprobe.exe`
   there (those files only) and writes `ffmpeg-path.txt` beside the program naming
   it; the helper looks for ffmpeg next to itself, then in that note, then on the PATH.
3. The user runs `ambersight capture --pair https://host:port CODE` (the
   download page shows the exact line). The helper:
   - fetches `GET /tls/ca.pem` and shows its SHA-256 fingerprint; the admin page
     shows the same fingerprint (`/api/tls` `ca_fingerprint_sha256`). The helper
     pins this CA for every later connection (TLS is verified against it, never
     against the system store, never skipped).
   - calls `POST /helper/register` with `{"code": "...", "machine": "<hostname>",
     "os": "windows|linux|macos", "version": "<helper version>"}`.
     Reply: `{"id": "<helper id>", "token": "<secret>", "name": "<machine>"}`.
     The code is used up. A wrong or expired code is a 401 and counts toward the
     sign-in lockout for that address.
   - saves `{server, id, token, ca_pem}` to its config file
     (Windows: `%APPDATA%\AmberSight\capture.toml`; others:
     `$XDG_CONFIG_HOME/ambersight/capture.toml`), readable by the user only.
   - turns on start at sign-in (Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`,
     value `AmberSight Capture`, command `"<exe>" capture`).
4. `ambersight capture --unpair` removes the config and the start-at-sign-in entry.

The server stores only a SHA-256 of the token. An admin can remove a helper
(`DELETE /api/helpers/<id>`); its token stops working at once and its
connection is closed.

## Control connection

`GET /helper/ws` upgraded to a WebSocket, with `Authorization: Bearer <token>`
and `X-Helper-Id: <id>`. Text frames carry JSON objects with a `type`. The
helper reconnects with backoff (2 s doubling to 60 s) when the connection drops.

Helper to server, first message:
`{"type": "hello", "id": "...", "version": "...", "machine": "...", "os": "windows"}`

Server to helper:
- `{"type": "list", "req": 7}`: report what can be captured.
- `{"type": "thumb", "req": 8, "source": <Source>, "width": 320}`: one small
  JPEG of a source.
- `{"type": "start", "camera": "<name>", "key": "<push key>", "source": <Source>,
  "fps": 10, "max_kbps": 2500, "max_width": 1920}`: capture this source and push
  it to the camera. Starting a camera that is already running restarts it with
  the new settings.
- `{"type": "stop", "camera": "<name>"}`
- `{"type": "ping"}` every 30 s; the helper answers `{"type": "pong"}`.

- `{"type": "desired", "cameras": ["<name>", ...]}`: the complete list of
  cameras this helper should be running. Sent after `hello` and whenever the
  server's cameras change, always followed by a `start` for each listed camera.
  The helper stops any running camera that is not in the list.

A `start` for a camera that is already running with the same settings is a
no-op (it does not restart the capture).

Helper to server:
- `{"type": "sources", "req": 7, "monitors": [{"index": 0, "name": "...",
  "x": 0, "y": 0, "w": 2560, "h": 1440, "primary": true}], "windows": [{"title":
  "...", "exe": "chrome.exe", "w": 1280, "h": 720}]}`
- `{"type": "thumb", "req": 8, "jpeg": "<base64>"}` or
  `{"type": "thumb", "req": 8, "error": "..."}`
- `{"type": "status", "camera": "...", "state": "running|starting|stopped|error",
  "error": "..."}` whenever a capture changes state.
- `{"type": "pong"}`

## Sources

```json
{"kind": "desktop"}
{"kind": "monitor", "index": 0}
{"kind": "window", "title": "Exact window title", "exe": "app.exe"}
{"kind": "region", "x": 0, "y": 0, "w": 1280, "h": 720}
```

A window is found by title (and exe, when given). If it is not open, the
capture reports `error` "window not open" and retries every 10 s.

## Video

The helper runs ffmpeg to capture and encode (H.264, keyframe every 2 s, the
camera's fps, scaled to at most `max_width`, bitrate capped at `max_kbps`,
hardware encoder when available, else libx264 veryfast) with FLV written to its
stdout. The helper streams those bytes to the server itself:
`PUT /push/<key>` over the pinned TLS connection, chunked transfer encoding.
(ffmpeg never talks to the server directly, so its own TLS settings do not
matter.) If the upload ends, the helper restarts it with backoff.

Windows capture: `ddagrab` for monitors (falls back to `gdigrab -i desktop`),
`gdigrab -i title=<title>` for windows, `gdigrab` with `-offset_x/-offset_y/
-video_size` for regions. Linux: `x11grab`. macOS: `avfoundation` (later).

## Privacy

While any capture runs, the helper shows a tray icon ("AmberSight is recording
this screen") with Pause all / Resume and Quit. Pausing reports `stopped` for
every camera until resumed. The helper never captures the webcam or microphone.
