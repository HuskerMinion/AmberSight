# AmberSight API guide for viewer apps

> **Status: pre-1.0.** The API is not versioned and may change between releases. There is no
> `/v1/` prefix and no compatibility promise yet. `GET /api/status` reports the server's
> `version`; check it on connect and treat unknown JSON fields as normal (new ones get added).

AmberSight is a self-hosted NVR. It serves a web app and a JSON/HTTP API from the same port.
The web app uses only this API, so a phone app can do everything a viewer does in the browser:
list cameras, watch live video (fragmented MP4 over WebSocket, or lighter JPEG streams), browse
events with thumbnails, play event clips, scrub recordings, and (for admin accounts) control
PTZ, arming, profiles and alert pauses. Sign-in uses a session cookie (password or one-time
pairing code). HTTP Basic auth also works. Android apps can get push notifications through
UnifiedPush (end-to-end encrypted, see "Push notifications for apps"); MQTT and the server-side
alert channels are there too.

All examples use the placeholder `https://nvr.example:8090`. 8090 is the default port.

---

## Quick reference

| Method | Path | Who | Purpose |
|---|---|---|---|
| GET | `/tls/ca.pem`, `/tls/ca.crt` | anyone (no auth) | Download the server's private CA certificate |
| POST | `/api/login` | anyone | Password sign-in; sets the `as_session` cookie, or asks for a two-step code |
| POST | `/api/login/totp` | anyone | The two-step code that goes with a password; sets the `as_session` cookie |
| POST | `/api/pair` | anyone | Sign in with a one-time pairing (QR) code |
| GET | `/api/whoami` | anyone | Who am I, is sign-in required |
| POST | `/api/logout` | anyone | End the session |
| GET | `/api/status` | viewer | Server name, version, armed state, active profile |
| GET | `/api/cameras` | viewer | Camera list (includes cameras from linked servers) |
| GET | `/api/live/:id/info` | viewer | Which live streams exist and their codecs |
| GET (WS) | `/api/live/:id/ws?q=main\|sub` | viewer | Live fragmented MP4 over WebSocket |
| GET | `/api/live/:id/stream?q=main\|sub` | viewer | Live fragmented MP4 over plain HTTP |
| GET (WS) | `/api/live/:id/jpeg?fps=N` | viewer | Live JPEG frames over WebSocket (light) |
| GET | `/api/live/:id` | viewer | MJPEG (`multipart/x-mixed-replace`) |
| GET | `/api/snapshot/:id` | viewer | Latest JPEG |
| RTSP | `rtsp://host:8554/<camera>[/sub]` | account (Basic) | Shared camera streams for other apps (off by default; separate port) |
| GET / PUT | `/api/restream` | **admin** | Sharing settings (on/off, port, which cameras) and state |
| POST | `/api/cameras/:id/wake` | viewer | Wake a battery/on-demand camera |
| GET | `/api/events` | viewer | Event (alert) list with filters and paging |
| GET | `/api/events/:id/thumb` | viewer | Event thumbnail (JPEG) |
| POST | `/api/events/:id/review` | **admin** | Mark an event a false alarm / OK, or clear the mark |
| GET | `/api/clips/:id` | viewer | Event clip (MP4, Range supported) |
| GET | `/api/timeline/events` | viewer | Compact event marks for a timeline |
| GET | `/api/events/:id/ai` | viewer | What the AI saw for an event |
| GET | `/api/plates`, `/api/plates/recent` | viewer (their cameras) | License plate readings / one line per plate seen |
| GET | `/api/plates/:id/img` | viewer (their cameras) | A reading's plate picture (JPEG) |
| GET | `/api/plates/known` | **admin** | The library of known plates |
| POST / PUT / DELETE | `/api/plates/known[/:id]` | **admin** | Add / change / remove a known plate |
| DELETE | `/api/plates/:id` | **admin** | Delete a reading |
| GET | `/api/smart/follow` | viewer (their cameras) | Follow a person or vehicle from an event to other cameras ("Where did they go?") |
| GET / POST | `/api/alert-rules` | **admin** | Alert rules in plain words: list / add or change |
| DELETE | `/api/alert-rules/:id` | **admin** | Delete a rule |
| POST | `/api/alert-rules/preview` | **admin** | Which recent events a rule's words would have matched, with scores |
| GET | `/api/recordings` | viewer | Recorded spans per camera |
| GET | `/api/playback` | viewer | Any stretch of recording as MP4 (Range supported) |
| GET | `/api/still`, `/api/preview` | viewer | Full-size / small JPEG at a moment |
| GET | `/api/seek` | viewer | Which recording covers a moment |
| GET | `/api/export` | viewer | ZIP of event clips + CSV (by filter or by event ids) |
| GET, POST | `/api/ai/model` | viewer / **admin** | Built-in AI models (`tiny`, `s`, `l`), the one loaded, and download progress; POST `{"model": "l"}` downloads (checksum-checked), loads and switches to it. `l` (Large, about 217 MB) is the most accurate and about five times slower per check than `s`; `ai.p50_ms` in `/api/stats` shows the real time |
| GET | `/api/stats` | viewer | Live machine, camera, storage, AI, network and GPU figures |
| GET | `/api/cameras/:id/ptz/caps`, `/ptz/presets` | viewer | PTZ capabilities and presets |
| POST | `/api/cameras/:id/ptz`, `/ptz/aim` | **admin** | Move / zoom / presets / tours |
| POST | `/api/shield` | **admin** | Arm / disarm |
| GET / POST | `/api/profiles` / `/api/profile` | viewer / **admin** | List / switch profiles |
| GET / POST | `/api/overlay` / `/api/overlay/:name` | viewer / **admin** | Named overlay variables shown in the camera text overlay as `{v:NAME}` (weather, a temperature, …). POST body is the plain value, or `{"value":"…","ttl":<seconds>}` to blank it after ttl; an empty body clears it. Also settable over MQTT (`<prefix>/overlay/<name>/set`), like the shield and profile. Live view only; not burned into recordings. Values live in memory: a restart clears them (a retained MQTT message sets them again) |
| POST | `/api/cameras/:id/pause` | **admin** | Pause a camera's notifications |
| GET | `/api/noisy` | **admin** | Cameras that sent too many notifications in 24 h, with suggestions |
| POST | `/api/noisy/:id/dismiss` | **admin** | Hide that camera's suggestion for 7 days |
| POST | `/api/clips/record` | **admin** | Record a manual clip |
| GET | `/api/cameras/:id/tamper` | **admin** | Tamper detection state, reference pictures, recent alerts |
| POST | `/api/cameras/:id/tamper/reference` | **admin** | Take the camera's picture now as its reference |
| GET | `/api/cameras/:id/tamper/ref/:light`, `/tamper/incidents/:n/:which` | viewer | Reference (`day`/`night`) and alert pictures (JPEG) |
| GET / POST | `/api/cameras/:id/unusual`, `/unusual/refresh` | **admin** | Unusual-activity learning state and typical week |
| GET / POST | `/api/cameras/:id/spots`, `/spots/forget` | **admin** | How many fixed spots the camera's AI remembers; forget them |
| GET / POST | `/api/digest/preview`, `/api/digest/send` | **admin** | Morning summary: build one now / build and send it now |
| GET | `/api/digests`, `/api/digests/:id` | viewer | Summaries kept (`latest` for the newest), cut down to the cameras the account may see |
| GET (WS) | `/api/cameras/:id/talk?rate=N` | **admin** | Two-way audio (PCM up to the camera) |
| GET / POST | `/api/links`, `/api/links/:id/open` | viewer | Linked servers / switch to one |
| POST | `/api/presence` | viewer (counted accounts) | This phone is at home / away (geofencing) |
| GET | `/api/presence` | viewer / **admin** | Own phones (and whether anyone is home, if the account counts) / every phone and the rule's state |
| DELETE | `/api/presence/:device_id[?user=]` | viewer (own) / **admin** (any) | Stop counting a phone |
| GET | `/api/push/key` | viewer | The server's VAPID public key (for UnifiedPush register) |
| POST | `/api/push/subscribe` | viewer | Get notifications on this app (`"kind": "app"`) or browser |
| POST | `/api/push/unsubscribe` | viewer (own) | Stop notifications on a device |
| GET | `/api/push/devices` | viewer (own) | The account's devices with notifications on |
| POST | `/api/push/test` | viewer (own) | Send a test notification to the account's devices |
| GET | `/api/tls` | viewer | HTTPS mode, certificate names, CA fingerprint, expiry |
| GET / POST | `/api/settings` (`server` section) | **admin** | HTTPS on/off, extra certificate names, own certificate files |
| POST | `/api/system/restart` | **admin** | Restart AmberSight (only when it runs as a service) |

"viewer" means any signed-in account (or anyone, when the server has no accounts). "admin" means
an admin account.

The server also has many admin endpoints for configuration: cameras, users, settings, storage,
offsite backup, capture helpers, discovery, faces, logs, health, and so on. They are not for
viewer apps and are not covered here.
Updates (`GET /api/update`, `POST /api/update/check`, `POST /api/update/install`) and `/api/system/*` are admin-only for reading too (403 for viewers).
`/link/*`, `/helper/*` and `/push/:key` (no `/api`
prefix) are server-to-server or device-ingest endpoints, not for apps.

---

## 1. Connecting

### Base URL, ports, HTTP versions

- One port serves both the web app and the API. The default is **8090**; the owner may change it.
- API paths start with `/api/`.
- With HTTPS turned on (server setting `tls`), the same port accepts TLS and plain HTTP. A plain
  HTTP request gets a **307 redirect** (temporary, so the http:// address works again if HTTPS is
  turned off) to `https://<same host>:<port><path>`. The only exceptions
  are `GET /tls/ca.pem` and `GET /tls/ca.crt`, which are answered over plain HTTP so a device can
  fetch the CA before it trusts the server.
- With HTTPS off (the default when nothing is configured), the server speaks plain HTTP only.
  Session cookies and Basic credentials then travel unencrypted. Your app should warn the user.
- Over TLS the server offers HTTP/2 and HTTP/1.1 (ALPN `h2`, `http/1.1`). Plain HTTP is HTTP/1.1.
  For WebSockets, use an HTTP/1.1 connection (WebSocket over HTTP/2, RFC 8441, is not
  promised).

### Trusting the certificate

When HTTPS is on and no certificate files are configured, AmberSight creates its own
**private certificate authority** and issues the server certificate from it. The CA is
name-constrained to home-network names and addresses: private IPv4/IPv6 ranges, `localhost`,
`.local`, `.lan`, `.home.arpa`, `.internal`, the computer's name, its Tailscale network, and any
extra names the owner configured. A public DNS name that is not in that list will not validate
against this CA.

| Path | Content-Type | Body |
|---|---|---|
| `GET /tls/ca.pem` | `application/x-pem-file` | CA certificate, PEM |
| `GET /tls/ca.crt` | `application/x-x509-ca-cert` | CA certificate, DER |

Both return 404 when the server uses certificate files supplied by the owner. In that case the
certificate chains to whatever CA issued it, often a public one.

Suggested app flow:

1. Fetch `/tls/ca.pem` (over plain `http://` if needed).
2. Show the user its SHA-256 fingerprint. The server writes it as colon-separated uppercase hex
   (`AB:CD:...`). After sign-in, `GET /api/tls` returns `ca_fingerprint_sha256` in the same format,
   along with `mode` (`"private-ca"`, `"files"`, or `"off"`), `names`, `expires`, `secure` and
   `note` (usually `null`; text when the owner's own certificate could not be loaded at start and
   the private CA is used instead).
3. Pin that CA for this server: trust it only for this server's host, not system-wide.
   - **Android:** a custom `X509TrustManager` built from the PEM, scoped to this server's OkHttp
     client.
   - **iOS:** evaluate the challenge's `SecTrust` with `SecTrustSetAnchorCertificates` in
     `urlSession(_:didReceive:completionHandler:)`.

The server reissues its certificate automatically (same CA) when it nears expiry or its
addresses change. Pin the **CA**, not the leaf certificate.

#### Video players use the system trust store

A pinned CA only covers requests made through the client you pinned it in. A media player
that is given a URL opens its own connection and uses the **system** trust store. That covers
Android's ExoPlayer/Media3 and Flutter's `video_player`, which uses ExoPlayer, as well as
AVPlayer on iOS. So the rest of the app works over HTTPS, but `/api/playback`, `/api/clips` and
`/api/live/:id/stream` fail. ExoPlayer reports a "Source error", often seen as
`PlatformException(VideoError, ... Source error)`. Pick one of these fixes:

- **Feed the player yourself (recommended).** Fetch the bytes with the HTTP client that already
  trusts the CA, and hand them to the player:
  - **Android:** a custom ExoPlayer `DataSource`, or a small loopback HTTP proxy on `127.0.0.1`.
  - **iOS:** an `AVAssetResourceLoaderDelegate`.

  Nothing extra is needed on the phone, and the session cookie is sent the same way as for every
  other request.
- **Android: trust user-installed CAs.** Apps targeting API 24 and up ignore CAs the user
  installs unless the app opts in. Add `res/xml/network_security_config.xml`:
  ```xml
  <network-security-config>
    <base-config cleartextTrafficPermitted="true">
      <trust-anchors>
        <certificates src="system"/>
        <certificates src="user"/>
      </trust-anchors>
    </base-config>
  </network-security-config>
  ```
  Then point the `<application>` tag at it with
  `android:networkSecurityConfig="@xml/network_security_config"`. Keep
  `cleartextTrafficPermitted="true"` if the app also connects over plain `http://`. Once this file
  exists, the manifest's `usesCleartextTraffic` setting is ignored. Each user then installs the
  server's CA once:
  1. Download `/tls/ca.crt`.
  2. Open Settings → Security → Encryption & credentials → Install a certificate → CA certificate.

  This makes the app trust **every** CA the user has installed, not only AmberSight's. That is
  usually acceptable for a viewer app. To trust user CAs only for home-network addresses, put the
  `user` anchor in a `<domain-config>` for those hosts instead of in `base-config`.
- **iOS:** the user installs `/tls/ca.crt` as a profile, then turns on full trust for it in
  Settings → General → About → Certificate Trust Settings.

The player must also send the session: pass `Cookie: as_session=...` (or an HTTP Basic
`Authorization` header) in the player's request headers, or use the proxy approach above.

### Turning HTTPS on or off (admin)

The owner turns HTTPS on in **Settings > Users & security > HTTPS**. An admin app can do the same
through the `server` section of the settings:

`GET /api/settings` returns, under `server`:

| Field | Meaning |
|---|---|
| `port`, `rtmp_port` | Ports (read-only here) |
| `tls` | HTTPS as saved (what the next start uses) |
| `tls_names` | Extra names/addresses for the private-CA certificate |
| `tls_cert`, `tls_key` | Paths of the owner's own certificate files on the server (`""` = none) |
| `running` | `{"tls": bool, "mode": "private-ca" \| "files" \| "off", "note": string \| null}`: what the listener uses now |
| `restart_needed` | `true` when the saved HTTPS mode or certificate files differ from what is running (or own certificate files could not be loaded at start) |
| `linked_servers`, `paired_helpers` | How many linked servers and paired capture helpers there are (both pin the private CA) |
| `restarts_itself` | `true` when `POST /api/system/restart` can restart it (it runs as a service) |

`POST /api/settings` with `{"server": {"tls": true, "tls_names": ["nvr.lan"],
"tls_cert": "", "tls_key": ""}}` changes them. Fields left out keep their value; `port` and
`bind` cannot be changed this way (ignored). Checks (refused with `{"error": "HTTPS: ..."}`):

- `tls_names`: host names or IP addresses only (no `http://`, path, port or spaces), at most 50,
  each at most 253 characters. Stored lower-case without duplicates.
- `tls_cert` / `tls_key`: both empty or both set; both regular files (not a folder or a device),
  at most 1 MB each, readable on the server, a PEM certificate chain and the private key that
  belongs to it. `""` removes them.
- Turning HTTPS on with the private CA (no `tls_cert`): the server checks that it can make and
  write `<data_dir>/tls`, and refuses the save with a plain error if it cannot.

The answer is `{"result": "success", "restart_needed": bool}`, sometimes with a `warning` text
(the save still happened):

- a name the private CA cannot cover. The CA is limited to names ending in `.lan`, `.local`,
  `.home.arpa` or `.internal`, the computer's own name, private, link-local and Tailscale
  (`100.64.0.0/10`) addresses, and the names in `tls_names` when it was made (a `*.ts.net` name
  then covers its whole tailnet). Other names, such as a `*.ts.net` name added later, need the
  owner's own certificate.
- own certificate files saved on a server with linked servers or paired capture helpers: they pin
  the private CA, so they stop working after the restart and cannot be linked or paired again
  until the private CA is used again (it is kept, so switching back restores them).

New extra names apply at once; turning HTTPS on or off and changing the certificate files apply
after a restart. A renewed own certificate (new file contents at the same paths) is also only
loaded at a restart. If the own certificate files cannot be loaded at start, AmberSight starts with
the private CA instead (logged, and `note` in `GET /api/tls` and `running.note`).

`POST /api/system/restart` (admin, body `{}`): when AmberSight runs as a service (Windows service,
systemd, launchd), answers `{"ok": true}` and restarts about a second later; the service manager
starts it again, usually within 10 to 30 seconds. Otherwise **409**
`{"error": "AmberSight is not running as a service here, so it cannot restart itself; restart it by hand"}`,
and **409** `{"error": "an update is being installed; try again when it has finished"}` while an
update is downloading, being checked or installing.
After turning HTTPS on, the server answers at `https://` on the same port (plain `http://` gets a
redirect, and `GET /tls/ca.pem` over plain HTTP answers once it is up, which an app can poll).
Sessions may need a new sign-in at the new address. Before moving a browser from `https://` to
`http://`, sign out (`POST /api/logout`) over `https://`: the session cookie set over HTTPS is
`Secure`, and browsers do not let an `http://` answer replace or clear it, so sign-in on the
`http://` address would not stick. Browser notification subscriptions belong to an address
(origin): they must be turned on again at the new address.

### Security headers and CORS

Every response carries `X-Content-Type-Options: nosniff`, `X-Frame-Options: SAMEORIGIN`,
`Referrer-Policy: same-origin` and a strict `Content-Security-Policy`. The server sends **no
CORS headers**. A native HTTP client is unaffected. A web-view app served from another origin
cannot call the API from page scripts.

Every response also carries `x-ambersight-ui: <12 hex characters>`, a short name for the web
interface build the server has (it changes with every update, and is not a version number). The
web page compares it with the one it was loaded with to offer a reload after an update. An app
can do the same to notice that the server was updated, or ignore it.

---

## 2. Sign-in and sessions

### Modes

| Server state | Behavior |
|---|---|
| **No accounts configured** ("open" mode) | Every request is allowed with full admin rights, no credentials needed. The `Host` header must name the server: an IP address, `localhost`, the computer's name (or `<name>.local`), or one of its configured certificate names. Any other host name gets **403** "unknown host name". This guards against DNS rebinding and also affects apps that connect through a dynamic-DNS name. |
| **Accounts configured** | Every `/api/` request except the four sign-in endpoints needs a session cookie or HTTP Basic credentials. |

`GET /api/whoami` tells you which mode you are in:

```json
{ "signed_in": true, "user": "viewer1", "admin": false, "auth_required": true, "session_cookie": true }
```

| Field | Meaning |
|---|---|
| `signed_in` | The request carried valid credentials, or the server has no accounts |
| `user` | Account name (`""` in open mode or when not signed in) |
| `admin` | Admin rights (always `true` in open mode) |
| `auth_required` | `true` when the server has accounts |
| `session_cookie` | Whether any `as_session` cookie arrived (for troubleshooting) |
| `two_step_required` | The request had the right password in Basic, for an account that needs a device token instead |
| `by_token` | The request was let in by a device token |

### Password sign-in: `POST /api/login`

Request body (JSON, at most 64 KB):

```json
{ "username": "viewer1", "password": "••••••" }
```

Success: **200** with JSON `{"user": "viewer1", "admin": false}` and a cookie:

```
Set-Cookie: as_session=<64 hex chars>; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000; Secure
```

- Cookie name: **`as_session`**. `Secure` is set when this request arrived over HTTPS (not
  from the saved setting), the same for `/api/pair` and for the clearing cookie of `/api/logout`.
- Lifetime: **30 days** from sign-in. It is a fixed expiry; use does not extend it. Sessions
  survive server restarts.
- A session ends early when the account is deleted, its password or permissions change, or its
  role changes. You then get 401 and must sign in again.
- Store the cookie and send `Cookie: as_session=<token>` on every request, including WebSocket
  upgrades and media URLs.
- **Also keep the `as_device` cookie** (set by the same answers, about two years) and send it with
  every sign-in and request. It names this app or browser to the server. An app that signs in on each
  run without it looks like a new device every time: admins get an alert each time, and after 50
  such sign-ins the real browsers of the account are pushed out of the list of known devices (and
  alert again). An app that keeps its sign-in should keep both cookies; a script or home-automation
  program that signs in over and over should use a device token (`/api/me/tokens`) instead.
- A request that began before another device of the account was signed out, or before a password
  changed, can get **409** `this account changed meanwhile: try again`. Repeat it.

Failures:

| Status | Body (plain text) |
|---|---|
| 400 | `expected {"username", "password"}` |
| 401 | `wrong username or password` (or `...; too many tries, wait a few minutes`) |
| 429 | `too many failed sign-ins; try again in a few minutes` |

### Pairing codes (QR sign-in): `POST /api/pair`

An admin creates a one-time sign-in code for an account in the web app. The admin endpoint is
`POST /api/users/:name/pair`. The result is a QR code containing a URL of this shape:

```
https://nvr.example:8090/#pair=<code>
```

`<code>` is 64 hex characters. The code works **once**, expires after **10 minutes**, and creating
a new code for the same account cancels the previous one.

To redeem a code, your app scans the QR, takes the origin and the `pair=` value from the
fragment, trusts the CA (section 1), then sends:

```http
POST /api/pair
Content-Type: application/json

{ "code": "9f3c…e1" }
```

The body is limited to 4 KB. The response matches `/api/login`: **200** `{"user", "admin"}` plus
the `as_session` cookie. An expired or already-used code returns **401**
`this sign-in code has expired or was already used; ask for a new one`. Lockout returns
**429**. The code itself grants the account's role. The server never asks for the password.

### Two-step codes (accounts with an authenticator app)

Any account can turn on six-digit codes from an authenticator app. **Your app must handle this:**
for such an account a right password to `POST /api/login` makes no session and sets no cookie:

```json
{ "totp_required": true, "ticket": "MTc4…Q.kP2x…w", "expires_in": 300, "recovery_only": false }
```

`recovery_only: true` means six-digit codes are locked for this address or account (someone is
trying): the ticket takes only a `recovery_code`, and your app must say so.

Check for `totp_required` before you treat a 200 as signed in. Then ask for the code and send:

```
POST /api/login/totp
{ "ticket": "MTc4…Q.kP2x…w", "code": "123456" }        (or "recovery_code": "abcd-efgh-jkmn")
```

| Answer | Meaning |
|---|---|
| **200** `{"user", "admin"}` + `as_session` cookie | Signed in. With a recovery code also `"recovery_left": n`: tell the person how many are left. |
| **401** `that code is wrong` | Ask again. The ticket stays good. |
| **401** `this sign-in has expired; enter your password again` | The ticket is used, older than 5 minutes, or not valid, the account changed, or the server restarted. Start over at `/api/login`. Treat a ticket as an opaque string: it can be a few hundred characters. A ticket that was never given out or was altered counts as a failed sign-in for your address; a late or used one counts against nobody. |
| **409** `this code is still being checked` | The same ticket is being tried in another request (a form sent twice). Wait for that answer. |
| **400** `enter the code` | Nothing was typed. Counts against nobody. |
| **429** from `/api/login` `too many sign-ins from here` | More than 10 right passwords a minute for one account with two-step from one address (IPv6 by its /64). Other accounts behind the same address are not affected. Any number of tickets can be open at one time: giving one out stores nothing, so none is ever refused for being too many or pushed out by another. |
| **429** | Too many wrong six-digit codes at sign-in: for this account from this address, IPv6 by its /64 (5, then 10 minutes), or for this account from all addresses together (30 in an hour, then an hour). The same ticket still takes a `recovery_code`. Or, for a recovery code: 10 tries per account and address in 10 minutes. |
| **503** | The server could not check, or the recovery code did not get its turn: two are checked at one time and the rest wait in line, for up to 20 seconds. Allow for that wait in your timeout. Nothing was tried and the try does not count. Try again. The ticket stays good. |

A code is good once, ever (also across server restarts), and a code older than the newest one used
is refused too, so someone signing in twice within 30 seconds waits for the next code. Accept digits
only and use a numeric keyboard; on Android and iOS use autofill for one-time codes.

When codes lock, admins get a system alert saying that someone has the password (one per hour for an
address lock and one for an account lock). Those locks are for sign-in only. From a session that
already exists, a six-digit code has its own limit (5 in 10 minutes, 10 a day, **429**), so the owner
can always respond: `POST /api/me/password {current, new, code}`. A password change always needs the
current password, and the code when two-step is on. Any password change on such an account sends admins
a system alert, and turning two-step off does too.

The authenticator app shows the account as `AmberSight (server name)`.

The password alone gets nowhere else on such an account. HTTP Basic answers **401** `this account
uses two-step codes, so its password does not work here: use a device token ...` on `/api/` and on
RTSP, and `/api/whoami` answers `"signed_in": false, "two_step_required": true`. A wrong password
looks exactly as it does for any account, so `/api/login` only reveals two-step to someone who has
the password. Sign-in from a linked server is not carried over (`"code": null` from
`/api/links/:id/open`, even on a link set to `"all"`): open that server's own sign-in.

**Exception:** a phone sign-in QR code (`POST /api/users/:name/pair`, `POST /api/pair`) still signs
in without a password or code: an admin made it and vouches for that person. It is written in the
audit log, and one for an admin account takes the admin's own password to make.

Sessions that already exist stay valid when the account turns on two-step, except that its other
sessions end at that moment (the one turning it on stays).

### Device tokens (for programs that watch or do everyday things)

A device token is a long-lived credential for one program or device: no password stored in it, no
code to ask for, and the owner can revoke just that device. A person makes one in **Your account**
(user menu) and gives it to your program, or your app makes one after a sign-in (below). Use it as:

```
Authorization: Bearer ast_0123…(68 characters)
```

or, for clients that only do Basic (RTSP players, older programs), as the password: `user:ast_0123…`.
They start with `ast_` so they can be recognized. The server keeps only a hash: **a token is shown
once and can't be read again, by anyone.** There is no Argon2 on this path, so tokens are fast.

**Tokens can do only what is listed here.** Every other request, including any route added later,
gets **403** `sign in with a password for this`.

| Scope | Allowed |
|---|---|
| `"viewer"` (default) | `GET`: `/api/cameras`, `/status`, `/live/:id` and `/live/:id/info`, `/stream`, `/ws`, `/jpeg`, `/snapshot/:id`, `/events`, `/events/:id/thumb`, `/timeline/events`, `/clips`, `/clips/:id`, `/recordings`, `/playback`, `/playback/picture`, `/seek`, `/still`, `/preview`, `/filmstrip`. `POST`: `/cameras/:id/wake`, `/signals`. Also RTSP. |
| `"admin"` (only for an admin account; "day-to-day control") | All of the above except RTSP, plus `POST`: `/shield`, `/profile`, `/cameras/:id/ptz` (moves and go-to-preset; **403** for `set_preset`, `remove_preset` and `tour`), `/cameras/:id/ptz/aim`, `/cameras/:id/pause`, `/clips/record`, `/events/:id/review`, `/events/:id/protect`. `GET`: `/profiles`, `/cameras/:id/ptz/presets`, `/cameras/:id/ptz/caps`. A token holds a profile or a pause for 24 hours at most, whatever it asks. Admins get a system alert when a token disarms, pauses or sets a profile, and the audit log names the token. |

`GET /api/whoami` also answers a token (`"by_token": true`).

So no token, whatever its scope, can read or change settings, add or change cameras, reach accounts
(`/api/users…`, `/api/me…`), linked servers, helpers, discovery, updates, restart, logs, the audit
log, health, off-site backups, faces, or alert rules. The camera list a token gets has no camera
logins or addresses, as for a viewer.

**Tokens cannot use `/api/push/…` or `/api/presence`.** Nothing would remove a phone's notifications
or stop it counting as someone at home after its token was revoked. An app that needs notifications
or presence signs in with a session (password and code, or a QR code).

**RTSP is not encrypted**, so RTSP refuses a token with the `"admin"` scope (**403**). Use a
`"viewer"` token there, on a network you trust.

A token never does more than its account can do **now**: limits to some cameras apply as for the
account.

| What happens | The token |
|---|---|
| Revoked | **401** from then on. Video the account has open ends within seconds (the account's other video is cut too: reconnect, a session or other token is not affected). If it cannot be removed from the database the revoke fails (**500**) and it stays listed. |
| Password changed | **Keeps working**: that is what tokens are for. Revoke it to cut off a device. |
| Account demoted from admin | Its `"admin"` tokens are deleted for good; being made an admin again does not bring them back. `"viewer"` tokens stay. |
| Account disabled or deleted | All its tokens are deleted for good. Enabling again does not bring them back. |
| Two-step turned on or off | Keeps working. |

A wrong token counts as a failed sign-in for the lockout (**401**); a live token is not stopped by a
lockout.

### Your account: `/api/me/…` (session or password only)

All need a signed-in person (a session, or Basic on an account without two-step). `password` is
always the account's password. `code` may be `recovery_code` instead wherever it appears.

| Request | What it does |
|---|---|
| `GET /api/me/account` | `{user, admin, two_step: {on, recovery_left, codes_locked}, tokens: [{id, name, scope, created, last_used}], token_limit, sessions: [{id, this, label, ip, signed_in, last_seen}]}` (`last_used` is updated at most once an hour). `sessions` are this account's signed-in devices, the most recently used first: `id` is `ses-…` (it is not the cookie and does not give it away), `this` marks the session of this request, `label` a fixed kind such as `Edge on Windows` (never the raw User-Agent), `ip` the address of its last request, `signed_in` and `last_seen` in epoch seconds. A session made before the list existed has `signed_in: null` and an empty `label`. |
| `POST /api/me/sessions/:id/sign-out` | Sign one of the account's own devices out: its session is gone at once (also from the database) and the account's open video and sockets end (they reconnect, for the devices that are still signed in). `{"ended": id, "this_device": bool}`; for this device the cookie is cleared, as for `/api/logout`. **404** for an id that is not one of this account's (someone else's too: the answers are the same). **500**: it could not be removed from the database and is still signed in. No body, no password. Another device's sign-out also forgets its browser (the next sign-in from it is told to admins again) and deletes the push subscriptions that were turned on from it (subscriptions made before the link was recorded cannot be matched). For an admin account, the devices losing alerts are told (from a list read before; the request does not wait for them) and the admins after. A second session of the browser making the request forgets nothing. Signing out *this* device keeps its browser known, like `/api/logout`. Open video on the account's other devices stops for a moment and reconnects. |
| `POST /api/me/sessions/sign-out-others` | Every device of the account but this one. `{"ended": N}`. Forgets the ended devices' browsers and push subscriptions as above. **500** `…could return at a restart…` if some could not be removed from the database. |
| `POST /api/me/tokens {name, scope?, password, code?}` | `{"token": "ast_…", "info": {...}}`. `code` is needed when two-step is on. At most 20 per account (400). |
| `DELETE /api/me/tokens/:id` | Revoke one |
| `POST /api/me/two-step/start {password}` | `{secret, uri, svg, expires_in, setup_id}`: a new secret as text, `otpauth://` link and QR picture. Not on yet. 409 if it already is. |
| `POST /api/me/two-step/confirm {code}` | On. `{"on": true, "recovery_codes": [10 codes]}`, shown this once. Other sessions end. **403**: wrong code (limited like sign-in codes, **429**); **409**: nothing waiting (start again after 15 minutes). |
| `POST /api/me/two-step/cancel {setup_id}` | Forget the secret that start gave out (`setup_id` is in its answer), if it is still waiting for its first code. Never touches codes that are on, or a newer start. |
| `POST /api/me/two-step/off {password, code}` | Off; recovery codes gone; other sessions end |
| `POST /api/me/two-step/recovery {password, code}` | `{"recovery_codes": [...]}`: ten new ones, the old ones stop working |
| `POST /api/me/password {current, new, code?}` | Always `current`, and `code` when two-step is on. All known browsers except this one are forgotten (a password changed by an admin or from the command line forgets all of them), so each is told to admins once at its next sign-in; the list of networks a direct password was seen from starts again. The sessions it ends lose their push subscriptions too (a device with no session keeps its subscriptions). For an admin account those devices are told from a list read before the change (the request does not wait for their push service). **500** says whether other sessions or only the list of known browsers could not be cleared. **403** says which was wrong. **409**: the account changed meanwhile, try again. |

Admins, for others (`/api/users/:name/…`): `GET …/tokens` lists names and dates (never the values),
`DELETE …/tokens/:id` revokes, `POST …/two-step/off {my_password, my_code?}` turns codes off for someone
who lost their phone (not for yourself; it ends that account's sessions). All are audited. `GET /api/users` has `two_step`
(bool) and `tokens` (a count) per account.

An admin changing **their own** password through `POST /api/users` gets **403** while two-step is on:
it would skip the code.

**Changes that take the admin's own password.** A stolen admin session alone cannot do these. The
same request carries `my_password`, and `my_code` (or `my_recovery_code`) when that admin has two-step
on: they are checked like any other code, with the same limits and audit.

| Request | When |
|---|---|
| `POST /api/users` | Creating an admin (`"admin": true`), promoting a viewer, enabling a disabled admin, or setting **another** account's `password`. |
| `POST /api/users/:name/two-step/off` | Always. |
| `POST /api/users/:name/pair` | When `:name` is an admin account (also your own) or has two-step on. |
| `POST /api/settings` | When it sets or changes `alerts.action_command` or `odt.command` (programs this server runs; not when left out, unchanged, masked or being cleared), or changes where or whether alerts arrive: any alert destination, changed or cleared (webhook, image post, Telegram and its API address, Home Assistant, ntfy, Pushover, MQTT, email); `alerts.link_url`, `alerts.system_alerts`, `alerts.system_alert_minutes`, `alerts.pushover_sound`; a server taken off `push.app_hosts` (adding one needs nothing); `digest.enabled` going off, `digest.channels`, `digest.cameras`; `watchdog.enabled` going on, or a higher `watchdog.minutes` or `watchdog.max_per_day` (the watchdog holds back the plain offline alert while it works on a camera). Masked and unchanged values and the defaults spelled out need nothing. The old destinations are told first. `alerts.link_url` must be `http://` or `https://` with no user name or password in it: otherwise `{"error": ...}`, before anything is asked. |
| `POST /api/offsite/test` | When it tries a destination other than the saved one. |
| `PUT /api/offsite` | When where backups go changes: kind, folder, S3 settings and keys, or passphrase. Not on/off or schedule. Fields go in the same object. |
| `POST /api/links`, `POST /api/links/code` | Always. |
| `PUT /api/links/:id` | When `signin` is set to `"all"`. |

A new account's `username` (`POST /api/users`, **400** otherwise): at most 64 characters, no control or
invisible characters, not starting with `(`, no `:`. An account that exists keeps working, whatever its
name. Admins are also told, with no password asked, when an admin device is added or removed
(`/api/push/subscribe`, `/api/push/unsubscribe`, naming the account and device) and when a restart is
asked for (`POST /api/system/restart`, naming who). The device of an admin being disabled, deleted or
demoted, or of one that is being removed, is told first.

Admins are also told of: a viewer made an admin; a device token with control rights being made; a
password changed on an admin account, by anyone; new recovery codes; a server being linked; and an
admin giving up their own rights.

Without them: **403** `this change needs your own password...`. Wrong: **403** `your password is wrong`
or `that two-step code is wrong`; too many tries at a code: **429**. If the account changed during the
check: **409**, or **401** if the caller can no longer do this. These and every `/api/me/…` that asks
for a password count wrong tries per session: after 5 in 10 minutes **403** `too many wrong tries:
this session has been signed out`, the session ends, and admins get a system alert.

`POST /api/users` refuses a `password` for your own name for everyone (**400**): use `/api/me/password`.
A QR code for a viewer gives a session that never gains admin rights, even if the account is promoted
later. Admins get a system alert when an admin is disabled, enabled, demoted or deleted by another admin,
when any account is created (a burst of new viewers in one message naming them all), when a recovery code
signs in, and when `reset-two-step` was used.

These need nothing extra: the first account of a server with none, creating or editing viewers
without setting an existing account's password, disabling anyone (an admin must be able to lock someone
out fast), enabling a viewer, and pair codes for viewers without two-step. Device tokens are refused on all of `/api/users…`. The
command line (`ambersight user ...`) is for someone with access to the computer and asks for neither.

What an admin can still do with this: make a phone sign-in QR code for another account (which skips
that account's password and code, and is written in the audit log), and create new admin accounts.
Keep admin accounts few, and turn two-step codes on for them.

### HTTP Basic auth

`Authorization: Basic base64(user:pass)` works on every `/api/` endpoint, including WebSocket
upgrades. Not with the password of an account that has two-step codes on: use a device token.

- The session cookie is checked first; Basic is used only if there is no valid cookie.
- The server checks passwords with Argon2, which is deliberately slow. A successful check is
  cached for **5 minutes**. Prefer the session cookie for apps.
- A wrong Basic password counts as a failed sign-in for the lockout.
- 401 responses carry **no `WWW-Authenticate` header**. Clients that wait for a challenge before
  sending credentials, such as some media players, will never send them. Send the header (or
  cookie) pre-emptively.

### Sign out: `POST /api/logout`

Ends every session named by the request's `as_session` cookie(s) and clears the cookie (with
`Secure` when the request came over HTTPS; a browser only clears a `Secure` cookie from an HTTPS
answer). It always returns **200** with body `{}`.

### Lockout (brute-force protection)

Five failed sign-ins (password, pairing code or Basic) from one IP address within 10 minutes lock
that address out for **10 minutes**. While locked out, `/api/login` and `/api/pair` return
**429**, and so do requests that carry an `Authorization` header but no valid session. A request
with a valid session cookie is never blocked.

### Roles and camera restrictions

| Role | Can do |
|---|---|
| **Admin** | Everything |
| **Viewer** (non-admin) | All `GET` endpoints in this guide. The only writes allowed are waking a camera (`POST /api/cameras/:id/wake`), opening a linked server (`POST /api/links/:id/open`), reporting and removing their own phones' presence (`POST /api/presence`, `DELETE /api/presence/:device_id`), turning notifications on and off for their own devices and sending them a test (`POST /api/push/subscribe`, `POST /api/push/unsubscribe`, `POST /api/push/test`), sending a signal (`POST /api/signals`), and a few personal-settings endpoints. Every other `POST`/`PUT`/`DELETE`, including PTZ moves, arm/disarm, profile switching, alert pause and manual record, returns **403** `admin access required for changes`. Admin-only reads (settings, logs, health, users, faces, ...) return **403** `admin access required`. Talk (two-way audio) is admin-only. |

An admin can limit a viewer account to some cameras. For such a **restricted viewer**:

- `/api/cameras` lists only the allowed cameras, and **no linked-server (remote) cameras**.
- Any single-camera request for another camera (`/api/live/:id/...`, `/api/snapshot/:id`,
  `/api/cameras/:id/...`, `/api/events/:id/...`, `/api/clips/:id`, or a `camera=` query
  parameter naming it) returns **404 `not found`**, as if the camera did not exist.
- Lists (`/api/events`, `/api/recordings`, `/api/timeline/events`, `/api/clips`) are filtered to
  the allowed cameras.
- `/api/export` requires a `camera` the viewer may see (400 `choose a camera to export`
  otherwise).

Viewers also get a trimmed camera list. Credentials, storage folders and capture/watchdog
details are removed, and `url`/`sub_url` show only the kind of source (for example
`rtsp://(hidden)`), never the camera's address.

### Origin rule (CSRF protection)

If a request carries an `Origin` header, it must match the `Host` (`host:port`) the request was
sent to. This applies to every non-GET/HEAD request and to WebSocket upgrades. A mismatch returns
**403** `request from another web site refused`. Native HTTP and WebSocket clients usually send
no `Origin` header, which is accepted. If your WebSocket library adds one, set it to
`https://<host>:<port>` exactly as in the URL.

---

## 3. Cameras and server status

### `GET /api/cameras`

```json
{
  "cameras": [
    {
      "id": 0,
      "name": "Front Door",
      "short_name": "Door",
      "group": "Outside",
      "groups": ["Outside"],
      "enabled": true,
      "online": true,
      "audio": true,
      "ptz": false,
      "on_demand": false,
      "sleeping": false,
      "device_sleeping": null,
      "record": true,
      "motion": false,
      "alerts_paused": false,
      "alerts_paused_until": null,
      "packages": true,
      "overlay_text": "{name} {date} {time}",
      "overlay_pos": "bottom-left",
      "url": "rtsp://(hidden)"
    }
  ]
}
```

The list carries many more per-camera settings (zones, schedules, AI thresholds, and so on) that
a viewer app can ignore. The fields useful to a viewer:

| Field | Type | Meaning |
|---|---|---|
| `id` | int | Camera id used in URLs. **Not stable across server restarts or camera removal**: it is the camera's slot number. Key your local data (layouts, favorites) on `name`, and refresh ids from this list on every connect. |
| `name` | string | Display name; unique on this server. Events, recordings and playback refer to cameras **by name**. |
| `short_name` | string | Short label for dense layouts (may be `""`) |
| `groups` / `group` | string[] / string | Groups the camera is in / its first group (`""` if none). There is no separate groups endpoint: build the group list from these fields (a camera can be in several groups). |
| `enabled` | bool | Camera turned on |
| `online` | bool | Stream currently connected |
| `motion` | bool | Motion detected right now |
| `motion_enabled` | bool | The camera's motion-detection **setting** (`motion` above is motion happening now) |
| `ai_last` | object or null | The last wanted object the AI saw live: `{"label": "person", "score": 0.87, "age_ms": 1200}` (`age_ms` = how long ago). `null` until one is seen. |
| `zone_polys`, `ignore_polys`, `privacy_masks` | arrays | Detection zones and ignore areas drawn as polygons (`{"points": [[x, y], ...]}`), and privacy masks as rectangles (`{"x", "y", "w", "h"}`). All coordinates are fractions of the picture (0 to 1). A viewer app can usually ignore them. |
| `named_zones` | array | Alert zones for loitering and line crossing (see "Alert zones" under Events). Each: `name`, `kind` (`area` or `line`), `points` (`[[x, y], ...]`, fractions of the picture; an area has 3 or more, a line 2 or more, first to last point), `enabled`, `labels` (objects that count, e.g. `["person"]`), `seconds` (area: time inside before it is loitering), `direction` (line: `ab` = along the arrow, called "in"; `ba` = "out"; `both`), `alert` (send notifications), `ignore_parked` (default true: vehicles parked for minutes do not count), `record` (save events with a clip), `schedule` (`"HH:MM-HH:MM"` or absent). Saved with the camera's other settings (names must differ, at most 24 per camera). A viewer app can usually ignore them. |
| `zones_occupied` | string[] | Names of the alert areas someone is loitering in right now. |
| `audio` | bool | Audio is recorded for this camera. Whether the **live** stream has audio comes from `/api/live/:id/info`. |
| `ptz` | bool or null | PTZ available; `null` = not checked yet. A camera with no `onvif_port` is searched for one automatically (common ONVIF ports, using the camera's own login), so `ptz` can turn true without any setting. |
| `onvif_port` | number or null | The ONVIF port saved for the camera, if any |
| `on_demand` | bool | Battery or on-demand camera that connects only while watched or triggered |
| `sleeping` | bool | On-demand camera currently disconnected on purpose |
| `device_sleeping` | bool or null | What the camera or hub itself reports about sleeping |
| `record` | bool | Recording on |
| `alert_ignore_polys` | list | Areas that never send a notification: `[{"points": [[x, y], ...]}]`, 3 or more corners, 0..1 of the picture (same shape as `zone_polys`; at most 12). An ordinary notification is skipped when every object that confirmed the event had its box center inside one of them. Detection, events, recordings, search and MQTT are not affected. Rules and zone alerts are not filtered. Set it with `PUT /api/cameras/:id`. |
| `alert_labels` | list or null | "Notify only for" objects (`person`, `vehicle`, `animal`, or labels such as `car`, `dog`); `null` = every object. Other objects are still detected, recorded, searchable and sent to MQTT; they just send no notification. Rules and zone alerts are not filtered, and doorbell presses (`visitor`), sounds (`sound`) and packages (`package`, `package picked up`) always notify, also when other labels are present. With AI on, an event with no object stays silent; with AI off, it notifies, since nothing could have named an object. Set it with the camera's settings (`PUT /api/cameras/:id`, the same field). |
| `alerts_paused`, `alerts_paused_until` | bool, int or null | Notifications paused; end time in epoch seconds, `null` = until resumed |
| `packages` | bool | Package detection on |
| `overlay_text`, `overlay_pos` | string or null | Optional caption the web app draws over live video (placeholders `{name} {short} {site} {date} {time}`, and `{v:NAME}` for an overlay variable) and its corner |
| `push_motion` | bool | A battery camera's motion push is in its window now (motion even before the picture arrives) |
| `remote` | object | Present only on cameras from a linked server; see section 9 |

Next to `cameras`, the reply has `overlay_vars`: the current overlay variables as `{"NAME": "value"}` (the same as `GET /api/overlay`).

### `GET /api/cameras/:id/netinfo[?refresh=1]` (admin)

The camera's network details, for a "Network" page in your app. Viewers get **403**; an unknown camera is **404**. Never contains a user name or password. Slow answers (maker/model
and MAC) are remembered for 5 minutes; `refresh=1` asks again. May take up to about 8 seconds
when the camera does not answer.

```json
{ "id": 1, "host": "10.0.0.5", "ip": "10.0.0.5", "mac": "aa:bb:cc:00:11:22",
  "scheme": "rtsp", "stream_port": 554, "onvif_port": 8000,
  "stream_path": "/live/ch0", "sub": { "port": 554, "path": "/live/ch1" },
  "ingest": "native", "online": true, "fps_setting": 10,
  "reachable": true, "connect_ms": 3, "error": null,
  "maker": "Acme", "model": "X100", "firmware": "1.2.3", "serial": "123" }
```

`host` is as configured, `ip` what it resolves to. `mac` is `null` unless the camera is on the
same network as the server. `maker`, `model`, `firmware` and `serial` are `null` unless
`onvif_port` is set and the camera answers. A camera that is not on the network (for example a
computer's camera) returns `host: null` and a `note`.

### `GET /api/status`

```json
{
  "app": "AmberSight",
  "name": "Home NVR",
  "version": "0.9.0",
  "uptime_seconds": 86400,
  "cameras": 6,
  "online": 6,
  "armed": true,
  "arming_until": null,
  "arm_delay": 30,
  "events": 1234,
  "webhook": false,
  "storage": { "…": "…" },
  "profile": { "…": "…" },
  "ffmpeg": { "…": "…" }
}
```

`armed` is the global arm state ("shield"). `arming_until` is the epoch-seconds time an arm
delay will finish, or null. `profile` is the active profile. Viewers do not receive `data_dir`
or storage folder paths. The exact shapes of `storage`, `profile` and `ffmpeg` are internal and
not described here.

`GET /api/health` (cameras, disks, warnings) is **admin-only**. It returns
`{"ok", "warnings", "short", "fixes", "problems", "cameras", "disks", "new_cameras"}`; `short` has a few words for each entry of `warnings` (same order, for a small badge); `warnings` are in plain words with no stream address, and each `cameras[]` entry has `last_error_plain` (the camera's last error in words, no address) beside the raw `last_error`; each `problems` entry is
`{"key", "text"}`. `new_cameras` lists what the hourly new-camera check (`[discovery] watch`)
found that is not a camera on this server yet and was not ignored: `{"ip", "kind"` (`onvif` or
`rtsp`)`, "name", "first_seen"}` (Unix seconds). It is information, not a problem. A folder that is full while the oldest recordings make room is not a
warning; disk warnings come when free space is below what the drive should keep (a problem
only when it cannot be freed, or under 2% free). Besides camera and disk problems, `warnings` may contain `AI object detection
is stalled: no check has worked for the last two minutes` (problem key `ai`).

### `GET /api/stats`

Live figures for the web app's status panel. Available to viewers (restricted viewers see only
their cameras; folder `path` values are left out for non-admins). It is meant for that panel, so
treat the shapes as stable but not promised. Optional `since` asks for history from a sequence
number on. Main keys:

| Key | Contents |
|---|---|
| `cpu`, `mem`, `fps`, `uptime_s` | Machine and program load, frames per second, uptime |
| `cameras[]` | Per camera: `id`, `name`, `enabled`, `online`, `sleeping`, `fps_in` (frames per second arriving), `cfg_fps` (frames per second it is set to), `kbps` (measured video bit rate, 0 when offline), `dropped_1m` (frames the decoder dropped in the last minute), `reconnects` (stream drops since start) |
| `storage[]` | Per recording folder: `name`, `used`, `cap`, `free`, `total` (bytes, for the drive), `tier` (`new` = where recordings land, `stored` = the archive), `write_bps` (bytes per second written), plus `pct`, `rolls`, `auto` (the cap is automatic: what the drive can spare while keeping `target_free` bytes free), `target_free`, and `level` (`""`, `info` = full and pruning keeps up, `warn`, `crit`) with its short `label` (e.g. `Full · keeping 9 days`) |
| `ai` | `state` (`off`, `idle`, `ok` or `stalled`), `device`, `checks_1m`, `errors_1m`, `p50_ms`, `p95_ms`, `last_ok_s` (seconds since the last good check; may be null) |
| `net` | `{"rx_bps", "tx_bps"}` bytes per second, or `null` where the machine cannot report it |
| `gpu` | `null` where unreadable, else `{"util", "adapters": [{"name", "util", "vram_used", "vram_total"}]}`; `util` is percent busy (the busiest card at the top level). The VRAM fields may be null. |
| `history`, `history_1h` | Recent samples. `history` is the short, fine-grained series; `history_1h` has one averaged point per minute for up to an hour (`step_s` is the spacing) with series `cpu`, `mem`, `fps`, `ai_p50`, `ai_checks`, `gpu`, `rx`, `tx` and `writes` (a map of folder name to bytes per second) |

---

## 4. Live video

### Choosing a method

| Method | Format | Best for | Notes |
|---|---|---|---|
| `/api/live/:id/ws` | fMP4 (H.264/H.265, optional audio) over WebSocket | Full-quality single view | Needs the camera on the native pipeline (see `/info`) |
| `/api/live/:id/stream` | Same fMP4 over one endless HTTP response | Players that take a URL | **Not available for linked-server cameras** (404) |
| `/api/live/:id/jpeg` | JPEG frames over WebSocket | Grids of many cameras | Analysis-size frames (about 640 px wide by default); works for every camera; does not wake battery cameras |
| `/api/live/:id` | MJPEG `multipart/x-mixed-replace; boundary=frame` | Simple image views | Polled at about 10 fps; for linked-server cameras it returns a single JPEG |
| `/api/snapshot/:id` | Single JPEG | Thumbnails, posters | Latest analysis frame; 503 `no frame captured yet` if none |

### `GET /api/live/:id/info`

```json
{
  "main": { "mime": "video/mp4; codecs=\"avc1.640033, mp4a.40.2\"", "width": 2560, "height": 1440, "audio": true },
  "sub":  { "mime": "video/mp4; codecs=\"avc1.4d401f\"", "width": 640, "height": 360, "audio": false },
  "viewers": 1,
  "waking": false
}
```

- `main` / `sub` are `null` when that stream is not running. Both are `null` for cameras on the
  ffmpeg pipeline (no fMP4 live) and for offline cameras. Then use JPEG/MJPEG.
- `mime` contains RFC 6381 codec strings:
  - video: `avc1.*` (H.264) or `hvc1.*` (H.265)
  - audio, if present: `mp4a.40.*` (AAC), or `flac` (the server converts G.711 camera audio to FLAC)
- `video_mime` appears only for a stream with audio: the codecs of its picture-only form. Add
  `&audio=0` to `/ws` or `/stream` to get that form (no audio track, less data). Use it for muted
  views, and as the fallback when a player stops on the audio track: one audio frame a decoder
  rejects ends playback of the whole stream in most players. Use `video_mime` as the MIME type
  with `audio=0`. A stream without audio is the same either way.
- `waking: true` means a battery camera is still connecting. Asking for `/info` wakes it. Ask
  again in about 1 s.
- Unknown camera: 404.

### `GET /api/live/:id/ws?q=main|sub` (WebSocket)

- `q` defaults to `main`. If the requested stream is not running, the server falls back to the
  other one. Use `info[q] || info[other]` to know which codec you will get.
- Every message is **binary**. The first message is the **init segment** (`ftyp`+`moov`). Next
  come cached fragments starting at the latest keyframe, then one `moof`+`mdat` fragment per
  message as they arrive.
- The server ignores anything the client sends, apart from close.
- The server **closes** the socket when:
  - the camera's stream restarts (parameters may change)
  - the client falls too far behind (the server buffers about 256 fragments per viewer)
  - no stream is available
  - for linked-server cameras, when sharing is turned off

  Reconnect, and fetch `/info` again first. The web app retries with backoff from 0.5 s up to
  10 s.
- For on-demand (battery) cameras the server wakes the camera and waits **up to 60 s** for the
  stream before closing.

### `GET /api/live/:id/stream?q=main|sub`

The same bytes as the WebSocket, as one endless `Content-Type: video/mp4` body, with
`Cache-Control: no-store` and header `X-Live-Mime: <mime with codecs>`. The response ends when
the stream restarts or the client lags. Failure statuses: 404 for an unknown camera, 503
`no live stream (camera offline or on the ffmpeg pipeline)`.

### `GET /api/live/:id/jpeg?fps=N` (WebSocket)

Binary messages, one complete JPEG each, sent only when a new frame exists, at most `fps` per
second (default **6**, clamped to 1–15).

### Sharing streams with other apps (RTSP)

Home Assistant, Scrypted, VLC, another NVR or any RTSP client can use AmberSight as the video
source instead of connecting to each camera. This is a separate RTSP server (not part of
`/api`), **off by default**; the owner turns it on in **Settings > Connections > Share streams**.

- Address: `rtsp://USER:PASSWORD@host:8554/<camera>` is the main stream and
  `rtsp://.../<camera>/sub` the sub stream (only for cameras that have one). `<camera>` is the
  camera name in lower case with every character other than a letter or digit replaced by `_`
  (the `slug` in `GET /api/restream`; "Front Yard" is `front_yard`). The port is configurable
  (`[restream] port`, default 8554).
- Video (H.264 or H.265) is passed on exactly as the camera sends it, with no decoding or
  re-encoding, and no extra connection to the camera. A client starts at the latest keyframe
  (the parameter sets are sent in front of it). AAC audio is included when the camera has audio
  turned on; G.711 camera audio is not shared (video only). Cameras read by ffmpeg cannot be shared.
  A camera with a privacy mask or text overlay is served only once the admin confirms it
  (`unmasked_ok`), because the shared video shows the picture without them.
- Transport is RTP interleaved on the RTSP TCP connection. A request for UDP is answered `461
  Unsupported Transport`, which makes common clients (ffmpeg, VLC, Home Assistant) use TCP.
- Sign-in is HTTP **Basic** with an AmberSight account (Digest is not possible: passwords are
  stored hashed). `DESCRIBE`, `SETUP` and `PLAY` get `401` with `WWW-Authenticate: Basic
  realm="AmberSight"` until the credentials are sent, so clients that wait for a challenge work.
  A wrong password counts toward the same lockout as web sign-in. Accounts limited to some
  cameras only see those; a camera that is not shared or not allowed answers `404`, as does
  a wrong path. It is not encrypted: use it on a trusted network or over a VPN. Without
  accounts, the streams need no sign-in.
- Several clients per stream are fine. A client that cannot keep up skips to the next keyframe,
  and one that stops reading is disconnected after 10 s; neither affects the camera, the
  recordings or other clients. `503` means the camera is not streaming yet (battery cameras are
  woken and waited for, up to 6 s).
- Audio and video are tied together with RTCP sender reports.

For Home Assistant's **Generic Camera**, use the main or sub RTSP address as the stream source
(RTSP transport TCP) and `GET /api/snapshot/:id` as the still image, or `GET /api/live/:id`
(MJPEG) as a stream source, with the account in the integration's user name and password
fields (basic authentication; it is sent with every request, as that integration does). No
extra token is needed: Basic works on every `/api/` endpoint (see "HTTP Basic auth"). Note
that the snapshot and MJPEG addresses use the camera `id` from `/api/cameras`.

#### `GET /api/restream` (admin)

```json
{
  "enabled": true, "port": 8554, "listening": true, "error": null, "auth": true,
  "cameras": [
    { "id": 0, "name": "Front Yard", "slug": "front_yard", "share": true, "has_sub": true,
      "blocked": null, "masked": false, "unmasked_ok": false, "waiting": null,
      "main": { "live": true, "clients": 1, "codec": "H.264", "width": 2560, "height": 1920, "audio": true },
      "sub":  { "live": true, "clients": 0, "codec": "H.264", "width": 640, "height": 480, "audio": true } }
  ]
}
```

`listening` says whether the port is open; `error` explains why not (port in use). `auth` is
`false` when the server has no accounts. `blocked` is a reason a camera can never be shared (or
`null`). `masked` is true when a privacy mask or text overlay is on; such a camera is served only
when `unmasked_ok` is true (the admin confirmed sharing it without them). `waiting` is the reason
a shared camera is not served now (or `null`). `sub` is `null` for cameras without a sub stream; `live` is whether the stream is
currently being published (it is not while the camera is offline).

#### `PUT /api/restream` (admin)

`{"enabled": true, "port": 8554, "cameras": {"Front Yard": true, "Garage": false}, "unmasked": {"Garage": true}}`.
All fields are optional; cameras left out keep their setting (`share` and `share_unmasked` in the
camera's config). `unmasked` confirms sharing a masked camera without its mask or overlay. The answer is
the same as `GET`. `400` with `{"error": ...}` for an unknown camera name or a port that is 0 or
already used by the web page or the RTMP input. Turning sharing off, or a camera's share off,
ends its clients' sessions at once.

### Battery / on-demand cameras: `POST /api/cameras/:id/wake`

Any signed-in user who can see the camera may call this. The body is ignored. Response:

```json
{ "woken": true, "sleeping": true, "awake_until": 1762170000 }
```

The camera stays awake until `awake_until` (epoch seconds, at least the camera's idle timeout,
minimum 30 s). Opening `/info`, `/ws`, `/stream`, `/api/live/:id` or `/api/snapshot/:id` also
wakes it. Call `wake` periodically while the user keeps watching. Returns 404 for unknown cameras
and for linked-server cameras.

### Playing it on phones (suggestions only)

- **Android:** ExoPlayer/Media3 can play fragmented MP4 from a custom `DataSource` that feeds the
  WebSocket messages in order: the init segment first, then the fragments. The `/stream` URL also
  works directly with a progressive source, given the cookie header. Check `MediaCodecList` for
  HEVC before choosing `main` on H.265 cameras.
- **iOS:** AVPlayer does not play an endless fMP4 HTTP body. Options:
  - feed the fragments to a local HLS (fMP4) playlist served in-process
  - decode with VideoToolbox (`VTDecompressionSession`) from the `avcC`/`hvcC` in the init segment
  - for grids, show the JPEG WebSocket

  FLAC audio in MP4 may need a separate audio path.
- Grids of many cameras: use `/jpeg` or `q=sub` per tile, and `q=main` only for the enlarged
  view.

### Two-way talk: `GET /api/cameras/:id/talk?rate=48000` (WebSocket, admin only)

The client sends **binary messages of raw PCM, 16-bit little-endian, mono**, at `rate` Hz
(default 48000; 8000–96000 allowed, otherwise 400). The server resamples to 8 kHz G.711 and plays
it through the camera's ONVIF RTSP audio backchannel. Only one talker per camera is allowed;
others are closed with code **4409**. Camera errors close with **1011** and a reason. Non-admins
get 403. Whether a camera supports a backchannel is only discovered when the session opens.

---

## 5. Events and alerts

An **event** is a stretch of motion, sound or a package change on one camera. AI may have
confirmed what was seen. Event times are **epoch seconds** (UTC).

### `GET /api/events`

| Param | Type | Default | Meaning |
|---|---|---|---|
| `camera` | string | all | Camera **name**, case-insensitive exact match |
| `since` | int (s) | 0 | Events that end at or after this time |
| `until` | int (s) | none | Events that start at or before this time |
| `object` | string | none | Events with a label containing this text (case-insensitive substring, e.g. `person`, `car`, `package`) |
| `limit` | int | 100 | Page size, at most 2000 (larger values are reduced to 2000) |
| `offset` | int | 0 | Skip this many (newest first) |
| `all` | bool | false | Also include events that AI looked at and found nothing that counts, on cameras set to hide those |
| `cameras` | string | none | Comma-separated camera **names** (up to 200). Names a restricted viewer may not see are dropped; if none are left the list is empty. |
| `protect` | bool | false | `1` (or `true`): only protected events |
| `objects` | string | none | Comma-separated labels (up to 20): events with **any** of them (substring match like `object`) |
| `review` | string | any | Comma-separated verdicts to include: `false_alarm`, `ok`, `none` (not reviewed yet). `any`, or leaving it out, means no filter. Unknown words are ignored. |
| `ids` | string | none | Comma-separated event ids: only these events (up to 200). Ids that cannot be event ids are skipped; with none left, no events are returned |

All filters combine (AND). A restricted viewer who gives no camera gets their allowed cameras.

The list is sorted newest first. There is no "changed since" cursor. To poll for new events, ask
with `since=<newest start you have>` every 15–60 s while the app is in the foreground, and drop
duplicates by `id`.

```json
{
  "events": [
    {
      "id": "front_door-1762166400",
      "camera_id": 0,
      "camera": "Front Door",
      "start": 1762166400,
      "end": 1762166431,
      "score": 0.42,
      "clip": "/api/clips/front_door-1762166400",
      "thumb": "front_door-1762166400",
      "objects": ["person"],
      "protect": false,
      "review": null,
      "reviewed_at": null
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `id` | Event id: `<camera slug>-<start>`, where the slug is the name lowercased with non-alphanumerics turned to `_`. Package events add a suffix: `-pkg` or `-pkgoff`. Treat it as opaque. |
| `camera` | Camera name (use this, not `camera_id`) |
| `camera_id` | Camera slot id when the event happened; may not match today's ids |
| `start`, `end` | Epoch seconds |
| `score` | Motion score |
| `clip` | Non-null means the event has video, playable at this address (always `/api/clips/{id}`). |
| `thumb` | Non-null means a thumbnail exists at `/api/events/{id}/thumb` |
| `objects` | Labels, e.g. `person`, `car`, `dog`, `package`, `package picked up`; may be empty |
| `protect` | Locked against automatic deletion |
| `review` | `"false_alarm"`, `"ok"`, or `null` (not reviewed). A person's verdict, not the AI's. |
| `reviewed_at` | Epoch seconds when the verdict was set, or `null` |

### Alert zones: loitering and line crossing

A camera can have named **areas** and **lines** (set in the camera's zone editor; `named_zones` in
`GET /api/cameras`). They need AI object checks. While there is motion, the server follows the
objects the AI sees (a light tracker) and judges them by their feet (the bottom center of the box):

- **Area**: an object of the zone's `labels` that stays inside longer than `seconds` is
  loitering: one event per visit.
- **Line**: an object whose feet cross the line, in the zone's `direction`, is one event. The
  line has an arrow; walking from the first point (A) to the last (B), **in** is towards the
  right-hand side on the picture, **out** the other way. The feet must end a small distance past
  the line, so a wobbling box at the line crosses nothing.

Each event is an ordinary event (`GET /api/events`, the clip, the thumbnail, notifications) with
two labels in `objects`: a tag, then what it was. Tags are `loitering: <zone name>` and
`crossed: <zone name> (in)` or `(out)`, so `objects=loitering` or `objects=Gate` finds them.
`start` is when the object came into the area (loitering) or the crossing; `end` is when it was
reported. A zone with `record: false` sends the notification but saves no event. Zone events are
not held back by the alert interval (they have their own cooldowns) and follow the arm state,
profiles and the camera's alert settings like other alerts.

### `POST /api/events/:id/review` (admin)

Body `{"verdict": "false_alarm"}`, `{"verdict": "ok"}`, or `{"verdict": null}` to clear it.
Any other verdict is **400**. Success is **200** with
`{"id", "review", "reviewed_at", "found": true, "marked"}`; an unknown event is **404** with
`{"id", "found": false}`. Like other changes it needs an admin account (or a device token with
the `"admin"` scope), and a camera restriction applies (the event id names its camera; a camera
the account may not see is 404). The verdict labels the event for filtering; it does not delete anything.

`"false_alarm"` with a `"label"` ("not a person") also tells the AI it was wrong, e.g.
`{"verdict": "false_alarm", "label": "person"}` (any vehicle label means the vehicles; animals
likewise). The event's boxes of that kind that sat still (their center moved less than 3% of the
picture between the event's checks) are remembered on that camera as a fixed spot for 30 days: the same kind of object there does not
alert again unless it moves (see "Remembered spots" below). A box that moved between checks
(someone walking) is never remembered. With that kind seen in one check only, nothing is: too
little to tell what stood still. Without a `label`, nothing is remembered. `marked` is then
`{"camera": "Driveway", "label": "person", "spots": 1}`; with `spots` 0 it has `why`: `"one_look"`,
`"moved"` or `"none"`. It is `null` with no label or no AI review.
Sending `"false_alarm"` again with another `label` adds that one. `"ok"` or `null` takes the
event's marks back. A label that is not a plain object name is **400**. Each mark is written to
the audit log.

### `GET /api/events/:id/thumb`

Returns a JPEG. 404 `no thumbnail`, or 400 `bad event id` if the id has characters other than
letters, digits, `_` and `-`.

### `GET /api/clips/:id[?quality=low][&download=1]`

The event's video as `video/mp4`: from the event start minus pre-roll to the end plus post-roll.

- Supports `Range` requests (206 with `Content-Range`, 416 when unsatisfiable), `HEAD`, and
  `Accept-Ranges: bytes`. An open-ended range (`bytes=N-`) returns everything to the end
  (before 0.2.0 it stopped at 8 MB, which Android's ExoPlayer took as the end of the file:
  clips over 8 MB failed with "Source error").
- `quality=low` uses the sub stream where it was recorded (data saver).
- `download=1` adds `Content-Disposition: attachment`.
- Errors: 404 `clip not found`, 500 `clip failed (details are in the server log)`.

`GET /api/clips` lists the latest 500 events that have video, as
`{"clips": [{"id", "camera", "start", "size"}]}`, newest first.

### `GET /api/timeline/events`

This is a compact form for drawing marks on a timeline. All three parameters below are needed.

| Param | Meaning |
|---|---|
| `since`, `until` | Epoch seconds (both required) |
| `cameras` | Comma-separated camera **names** (at most 200) |
| `bucket` | Optional slot size in seconds (0–604800). Events of one camera and kind in one slot come back as one row with a count. |

```json
{
  "cameras": ["Front Door", "Driveway"],
  "rows": [
    ["front_door-1762166400", 0, 1762166400, 1762166431, ["person"], true, 1]
  ],
  "more": false
}
```

Each row is `[event id, index into cameras, start, end, objects, has thumbnail, count]`.
When a bucket is used, events are grouped by camera and kind in each slot. Package and face events
now get their own rows instead of being folded into a person row.
Without a bucket, at most 20,000 rows come back, and `more: true` means there were more.
Unconfirmed events on cameras set to hide them are always left out here.

### AI review: `GET /api/events/:id/ai`

```json
{
  "event": { "id": "front_door-1762166400", "camera": "Front Door", "start": 1762166400, "end": 1762166431, "objects": ["person"], "thumb": true },
  "summary": { "kind": "alerted", "best_wanted": { "label": "person", "score": 0.87 }, "best_other": null, "best_parked": null, "checks": 4, "camera_labels": [] },
  "review": { "…": "…" },
  "camera": { "…": "…" },
  "ai": { "enabled": true, "model": true, "server": false }
}
```

`summary.kind` is one of: `alerted`, `quiet_person`, `known_only` (the camera alerts only for strangers and everyone seen was known), `quiet_rule` (a quiet alert rule dropped the alert), `ai_failed`, `alerts_off`, `strict`, `parked`,
`fixed_spot`, `camera_only`, `near_miss`, `other_object`, `nothing`. Other values may exist for cases not traced here. The
frames the AI checked are at `/api/events/:id/ai/:n` (JPEG). `summary` and `review` are `null`
for events without an AI check. The full `review` structure is internal and may change.

`fixed_spot`: what the AI saw stood in a spot where the same kind of object had been seen in
several earlier events, or where the owner marked an event "not a ..."; `summary.best_spot` is
`{"label", "score", "marked"}` (`marked` true for the owner's mark). In `review.checks[]`,
`parked[i]` is true for every detection that did not count because it sat still, and `spot[i]`
is `"remembered"`, `"marked"` or `null` for why.

`camera_only`: the camera itself reported a person, vehicle or animal (`review.camera_unconfirmed`
lists them) and the AI, looking at the same event, did not see that kind of object at all, down to the
review floor (0.25, also boxes a remembered spot held). With AI on for a camera, the
camera's own detection of a person, vehicle or animal confirms an event only when the AI agrees,
or when the AI could not look (no picture, the AI failing, no check that worked after the camera's
report). When the camera's report comes, the AI gets a fresh set of checks, and keeps checking up to
18 while it has not seen that kind. Doorbell presses, sounds, packages and
other labels confirm as before. The event keeps the camera's labels and is listed as before. A
camera's `ai_confirm_camera: true` turns this on; unset (or false) means off, and the camera's own
detection confirms on its own. It is off by default: the AI misses people and animals some cameras
see well themselves (a camera looking straight down, someone lying on a sofa).

#### Remembered spots

With "ignore objects that have not moved" on (`ai.ignore_parked`, or the camera's
`ai_ignore_parked`; on by default), each camera's AI remembers where it keeps seeing the same
kind of object: people; vehicles together; animals together. A spot is remembered after the
same kind has been seen there, at or above the camera's minimum, in 3 separate events within
24 hours (a person- or animal-shaped one must also have been seen there in 2 other events only
under the minimum, never surely). It lasts 24 hours from when it was learned, then has to be
learned again; checks that it holds back neither teach it nor keep it. Weak-only events count an
hour apart at least. A person box taller than 11% of the picture is never held (fixed things taken
for a person were smaller). A vehicle is held only if the spot was last seen occupied when the
event began (within 12 hours): last seen empty, it is an arrival and confirms. A detection on a remembered spot does not confirm the event; everything else is
unchanged: the event is recorded, keeps its labels (so `GET /api/events?objects=person` still
finds it) and is not "unconfirmed" for `ai.hide_unconfirmed`. Something in the spot that moves
between two looks, or is clearly bigger or smaller, counts as usual, as does anything elsewhere.
Spots not seen for 24 hours are forgotten. The owner's marks (above) work even with the setting off.

`GET /api/cameras/:id/spots` (admin) is `{"remembered": 2, "marked": 1, "learning": 7, "on": true}`
(`learning`: spots seen but not remembered yet; `on`: the camera's setting).
`POST /api/cameras/:id/spots/forget` (admin) forgets them all, the marks too: `{"forgot": 10}`.
Device tokens cannot use either.

### License plates: `GET /api/plates`

When license plate reading is on for a camera (Settings, License plates), each event with a
vehicle gets its plates read. The event's `objects` then hold labels such as
`plate ABC123 (Mom's car)` (a plate in the known list) or `unknown plate XYZ789`, so the
event list, search (`/api/events?objects=plate`) and the alert text show them. The readings
themselves, with a small picture each, are here. Viewers see readings of the cameras their
account may see; known-plate changes and deleting a reading are admin only.

`GET /api/plates` (newest first). Query: `event` (one event's readings), `camera` (name),
`q` (part of a plate, any case or spacing, or part of a known plate's name), `known` (`1` only
known plates, `0` only unknown), `since` / `until` (epoch milliseconds), `limit` (default 100,
at most 1000), `offset`.

```json
{
  "plates": [
    { "id": 41, "event": "driveway-1762166400", "camera": "driveway", "camera_name": "Driveway",
      "at_ms": 1762166412000, "text": "ABC123", "confidence": 0.97, "width": 112,
      "known": true, "known_id": 3, "name": "Mom's car", "notes": "blue sedan" }
  ],
  "known_plate": true,
  "unknown_plate": false
}
```

`text` is upper case letters and digits only. `confidence` is 0-1 (the reader's mean sureness
over the characters), `width` the plate's width in pixels in the full-size picture. `name` and
`notes` are `null` for an unknown plate. With `event=`, `known_plate` / `unknown_plate` say
whether a plate in / not in the known list was read in that event (a later rule can alert on
strangers only).

`GET /api/plates/recent` takes the same query and returns one line per plate text, most
recently seen first: `text`, `count`, `first_ms`, `last_ms`, `id` (a reading to take the picture
from), `confidence` (the best), `known`, `known_id`, `name`, `notes`, `cameras` (slugs) and
`camera_names`.

`GET /api/plates/:id/img` is the plate's picture (JPEG, about 240 px wide); 404 for a camera
the account may not see.

`GET /api/plates/known` (admin) is `{"known": [{"id", "text", "name", "notes", "created_ms", "seen",
"last_ms"}]}`. Admins change it: `POST /api/plates/known` with `{"text": "ABC 123", "name":
"Mom's car", "notes": ""}` (text is normalized; 2 to 12 letters and digits; name 1 to 60
characters; notes up to 300; 409 if the plate is already listed) answers `{"id": N}`;
`PUT /api/plates/known/:id` takes any of `text`, `name`, `notes`; `DELETE /api/plates/known/:id`
removes it (its earlier readings stay, as unknown). Naming a plate makes its earlier readings of
exactly that text known.

### Follow across cameras: `GET /api/smart/follow`

"Where did they go?": from an event, find the same person or vehicle on other cameras around
that moment. It needs smart search on (503 otherwise) and uses the fingerprints smart search
keeps of the people and vehicles AI found (events not searched yet have none: 404). The score
is **approximate**: CLIP fingerprints tell what something looks like (shape, colors, clothes,
paint), not who or which it is. Someone dressed alike can match, and the same person from
another angle, in other light or far away can score low. The score is raised when the two events
show the same recognized face (+0.12, people) or the same plate (+0.25, vehicles; one misread
character allowed). Show it as a guide, hide weak matches by default (the web page starts at
0.80 and has a slider), and let people look.

Query: `event` (required), `pic` (the person or vehicle to follow: a picture id from `source` /
`others`; default the largest in the event), `before` / `after` (seconds around the moment,
default 600 each, at most 21600), `same_camera=1` (also sightings on the same camera),
`min` (weakest score returned, default 0.60), `limit` (default 80, at most 200).

```json
{
  "source": { "event_id": "front-door-1762166400", "camera": "Front Door", "at_ms": 1762166412000,
              "pic": 311, "box": [0.41, 0.2, 0.18, 0.7], "label": "person", "sim": 1.0, "boost": 0.0,
              "score": 1.0, "face": false, "plate": false, "source": true },
  "others": [ /* other people / vehicles in the same event, same fields */ ],
  "path": [ /* the matches in time order, the start included (source: true) */
    { "event_id": "side-yard-1762166500", "camera": "Side Yard", "at_ms": 1762166501000, "pic": 340,
      "box": [0.1, 0.3, 0.2, 0.6], "label": "person", "sim": 0.86, "boost": 0.12, "score": 0.98,
      "face": true, "plate": false, "source": false } ],
  "scanned": 57, "before": 600, "after": 600, "floor": 0.6, "default_threshold": 0.8
}
```

`score` is what results are ranked and cut by (`sim` + `boost`, at most 1); `pic` is fetched
with `GET /api/smart/pic/:pic` (JPEG; `box` is x, y, width, height as fractions of it). To play
the moment, open that camera's recording at `at_ms` (`/api/playback`). To carry on from a
sighting, call again with its `event_id` and `pic`. Viewers get only sightings on cameras their
account may see; an event on a camera they may not see answers 404. 422: no person or vehicle
was found in that event.

### Alert rules in plain words: `/api/alert-rules` (admin)

Rules like "a delivery truck stopped out front". Every new event's pictures are compared with
the rule's words (smart search must be on; rules do nothing otherwise). A match tags the event
`rule: <name>` and, by the action, sends an alert titled "<name> at <camera>" through the usual
channels (push, ntfy, Telegram, email, webhook ...). Rules look at events as they get their
pictures checked (a few seconds after the ordinary alert; events older than 15 minutes are
left alone), respect profiles, paused cameras and each camera's alert window, and alert again for
the same camera only after `cooldown_min`.

`GET /api/alert-rules` answers `{"rules": [...], "smart_enabled", "smart_loaded", "supported",
"default_threshold"}`. A rule:

```json
{ "id": "r18f3a", "name": "Delivery truck out front", "text": "a delivery truck stopped in front of the house",
  "kind": "alert", "cameras": ["Front Door"], "threshold": 0.03, "action": "alert_high",
  "schedule": "08:00-20:00", "enabled": true, "cooldown_min": 5, "hold_secs": 30 }
```

`kind`: `alert` or `quiet`. `action` (alert rules): `alert_high`, `alert`, `notice` (low
priority) or `tag` (only tag the event). `cameras`: names, empty = all. `schedule`: optional
`HH:MM-HH:MM` for when the event happened. `threshold` is the match score at which the rule
fires (-0.2 to 0.6; the score is how much more a picture looks like the words than like words
saying nothing in particular: about 0 for unrelated pictures, 0.03 to 0.10 for good matches).
`POST /api/alert-rules` with no `id` adds a rule (at most 40), with an `id` replaces it; it
answers `{"rule": {...}}` or 400 `{"error"}`. `DELETE /api/alert-rules/:id`.

`POST /api/alert-rules/preview` with `{"text", "cameras": [], "hours": 72}` (hours 1 to 720)
answers `{"scanned", "hours", "median", "items": [{"event_id", "camera", "at_ms", "pic",
"box", "label", "score"}]}`: the 60 recent events that score highest for the words, so a
threshold can be set between the events that should match and the ones that should not.

**Quiet rules** (`kind: "quiet"`, for example "my own car in the driveway") do the opposite: a
matching event is tagged `quiet: <name>` and its alerts are not sent, neither the ordinary one
nor another rule's. Because the pictures are checked after the event, ordinary alerts of the
cameras a quiet rule covers are held for up to `hold_secs` (5 to 90, default 30) until the
event is checked; if it is not checked in time the alert goes out as usual. Cameras no quiet
rule covers are never delayed, and nothing else is affected (recording, the event list, MQTT).
When a quiet rule drops an alert, the event's AI review says so (`alert.why` `quiet_rule`). An
alert rule's own alert is not sent for an event whose ordinary alert was kept quiet because the
person was known (a camera that alerts only for strangers, a person marked "no alerts", or the
object detector's "only known" setting). Renaming a camera renames it in the rules; removing it
takes it out of them (a rule that named only that camera is turned off). Object filters
(`object=`, `objects=`) match what was seen, not the names inside `rule:`, `quiet:`,
`loitering:`, `crossed:` and `plate` labels, unless the search word itself starts that way
(`object=loitering: Gate`).


How a reading counts as known: the same text, or at most `max_diff` characters different (default
1; plates under 5 characters must match exactly), with look-alike characters (O/0, I/1, B/8, S/5,
Z/2, G/6, D/0, Q/0) counting as equal when `confusions` is on. Both are in the `lpr` section of
`GET /api/settings` (admin), with `enabled`, `min_confidence`, `min_plate_px` and `cameras`
(a list of `{"name", "min_confidence", "min_plate_px", "full_frame"}` for the cameras plates are
read on; none = off) and `keep_days` (1 to 3650, default 180: readings older than that are
deleted, known plates' included). `GET /api/plates/status` (admin) says whether the models are ready
and loaded and the download's progress; `POST /api/plates/models` (admin) starts the download
of the plate reader (it also starts when plates are turned on without it). The plate finder is
AmberSight's own model, built into the program, so only the reader is downloaded.

### Getting notified

There are three ways to get alerts out of the server:

- **Push notifications for apps (UnifiedPush)**, below: end-to-end encrypted notifications to an
  Android app, per signed-in account, with no server-side setup beyond allowing the push server.
- **MQTT**, if the owner configured a broker (further down).
- **Server-side alert channels** the admin sets up (email, Telegram, ntfy, Pushover, webhook,
  Home Assistant and others).

Browsers use the same push endpoints with their own push services (Web Push); the web app does
that itself and an app does not need to.

### Push notifications for apps (UnifiedPush)

[UnifiedPush](https://unifiedpush.org) delivers standard Web Push messages (RFC 8030, encrypted
with RFC 8291, signed with VAPID, RFC 8292) to Android apps. A **distributor** app on the phone,
usually [ntfy](https://ntfy.sh), keeps one connection to its push server and hands each message
to your app. Your app's connector library makes the encryption keys and decrypts. The push
server only ever sees ciphertext: the message is encrypted on the AmberSight server for the keys
on the phone.

- **Android only.** iOS has no UnifiedPush; an iOS app would need Apple's push service (APNs)
  through a relay run by the app's developer. AmberSight does not provide one. (Safari on
  iPhone can get browser notifications from the web app instead.)
- The server sends camera alerts to every account allowed to see that camera, and system
  problems to admin accounts, exactly as for browsers. A camera whose alert settings leave out
  "push" sends to neither browsers nor apps.
- Each account can have at most 25 devices (browsers and apps together).

#### Which push servers are allowed

The server only posts to push servers an admin allowed, so that a signed-in account cannot make
it send requests to other machines on the network. The admin edits the list under
**Settings > Detection & alerts > Alerts > Browser and phone push > Push servers for phone apps**
(`[push] app_hosts` in `config.toml`; default `["ntfy.sh"]`).

- Entries are exact host names, optionally with a port: `ntfy.sh`, `ntfy.example.com`,
  `ntfy.home.lan:8080`, `192.168.1.20:2586`. No wildcards; a subdomain must be listed itself. An
  entry without a port allows the normal port only (443 for https, 80 for http).
- The server may POST to **any path** on a listed host and port (the path is the app's topic).
  So list a self-hosted ntfy as `host:port` and give ntfy a host name or port of its own, not one
  shared with a router page, a NAS or another web app.
- The browsers' push services (Google, Mozilla, Apple, Microsoft) are always allowed, so a
  distributor built on one of them (for example Sunup, which uses Mozilla's) needs nothing listed.
- **https** is required, except that plain **http** works for a listed server on the home
  network, loopback or Tailscale (10/8, 172.16/12, 192.168/16, 127/8, 100.64/10, fc00::/7,
  ::1). For plain http only those private addresses of the name are used: a home name that also
  has a global IPv6 address works, and the message goes to its home address. A name with no
  private address is refused.
- A home-network address is only ever used for a host the admin listed. The public `ntfy.sh`
  (and the browsers' services) must resolve to public addresses; if one ever points at a private
  address the server refuses (that is what a DNS rebinding attack looks like). Link-local
  addresses (169.254.x.x, fe80::), multicast, reserved and special-purpose ranges (192.0.0.0/24,
  198.18.0.0/15, 192.88.99.0/24, Teredo 2001::/32, local NAT64 64:ff9b:1::/48) are never used;
  IPv6 forms that carry an IPv4 address (`::ffff:a.b.c.d`, `::a.b.c.d`, 6to4 `2002::/16`, NAT64
  `64:ff9b::/96`) are judged by that IPv4 address.
- The name is looked up at every send and the connection goes to the checked addresses only.
  Redirects are not followed.
- The certificate must be trusted publicly (webpki roots). For a self-hosted ntfy use a real
  certificate (Let's Encrypt, or `tailscale cert` for a `*.ts.net` name) or plain http on the
  home network.

**Self-hosted ntfy notes.** Set `base-url` in ntfy's `server.yml` to the address phones (and
AmberSight) reach it at: the push address your app receives is built from it. In the ntfy app,
choose that server for UnifiedPush (Settings, UnifiedPush / default server). If ntfy has
`auth-default-access: deny-all`, allow anonymous writes to UnifiedPush topics:
`ntfy access everyone 'up*' write-only`. That is safe because every message is end-to-end
encrypted for the phone and the topic names are long and random, so a stranger can neither read
the messages nor guess where to send one. Keep the ntfy server on the home network or Tailscale
and do not port-forward it to the internet. Then ask the AmberSight admin to add it as
`host:port` (for example `ntfy.home.lan:8080`, or the Tailscale name `ntfy.tail1234.ts.net`),
on a host name or port used only by ntfy. On Tailscale the AmberSight server must be on the
tailnet too.

#### Step by step (Flutter or native Android)

Use the [`unifiedpush`](https://pub.dev/packages/unifiedpush) Flutter package, or the
`org.unifiedpush.android:connector` library for Kotlin/Java. Names below follow those libraries
loosely; check their documentation for the version you use.

1. **Get the server's VAPID key** (any signed-in account):

   ```http
   GET /api/push/key
   ```

   ```json
   {"key": "BPx3...base64url, 65 bytes uncompressed P-256..."}
   ```

   It stays the same for the life of the server's database. If it changes (a restored or new
   server), register again.

2. **Register with the distributor**, passing that key as the VAPID key
   (`UnifiedPush.register(..., vapid: key)`). If the phone has no distributor, tell the user to
   install ntfy (or another distributor) and pick it.

3. **On a new endpoint** (`onNewEndpoint`): send it to AmberSight with the keys the library made:

   ```http
   POST /api/push/subscribe
   Content-Type: application/json

   {
     "subscription": {
       "endpoint": "https://ntfy.sh/upAbC123xyz?up=1",
       "keys": {"p256dh": "BNc...(65 bytes, base64url)", "auth": "q2...(16 bytes, base64url)"}
     },
     "kind": "app",
     "device": "Pixel 8"
   }
   ```

   ```json
   {"ok": true, "key": "3f9a0c1d2e4b5a69", "device": "Pixel 8", "kind": "app"}
   ```

   - `kind`: `"app"` for apps (default `"browser"`).
   - `device`: the name shown in the device list, up to 64 characters, no control or invisible
     characters (zero-width spaces and joiners, direction marks, soft hyphen, byte order mark).
     Without it the device is called "Phone app".
   - Sending the same endpoint again from the same account updates it (keys and name). An
     endpoint already registered to another account answers **409** `this push address belongs
     to another account`, unless the caller is an admin (an admin's request moves it to the
     admin's account). After signing in as another account, unsubscribe first (while still
     signed in as the old one) or register a new endpoint with the distributor. Send it on every
     new endpoint and once in a while (for example on app start) in case the server forgot it.
   - `key` is a short fingerprint of the endpoint; `GET /api/push/devices` lists the same value,
     so the app can find itself in the list.

4. **On unregistered** (`onUnregistered`), when the user turns notifications off in the app, or
   at sign-out:

   ```http
   POST /api/push/unsubscribe
   Content-Type: application/json

   {"endpoint": "https://ntfy.sh/upAbC123xyz?up=1"}
   ```

   Answers `{"removed": true}` (or `false` if it was not there). `{"id": 12}` works too, with an
   id from the device list. An account can only remove its own devices. If the push server says
   an endpoint is gone, the server removes it on its own: on 404 or 410 from the browsers'
   services and `ntfy.sh`, but only on **410** from a push server the admin listed (a
   self-hosted one). A 404 from such a server is logged and the device kept, so a misconfigured
   proxy or a changed ntfy `base-url` does not wipe every device.

5. **On a message** (`onMessage`): the library has already decrypted it (with the
   `org.unifiedpush.android:connector` library, check that the message is marked decrypted).
   Parse the JSON below and show a notification: `title` and `body` as text; group or replace by
   `tag`; a high-importance channel when `urgent` is true or `type` is `event` or `problem`.

6. **The picture**: if `thumb` is set, fetch it with the session (cookie or Basic auth), for
   example `GET https://nvr.example:8090/api/events/<id>/thumb`. Use the path as given; do not
   build it from `event_id`. The picture is never in the push message itself.

7. **On tap**: for `type` `event`, open the event: `GET /api/events?ids=<event_id>` gives the
   event, then play `/api/clips/<event_id>`. For other types, open the camera (`camera`) or the
   server status. `url` is the path the web app would open (`/?alert=...&cam=...&at=...`, or
   `/#settings/system/status`); an app can ignore it.

8. **Test**: `POST /api/push/test` (body `{}`) sends a test notification (`type` `test`) to all
   of the account's devices, browsers and apps, and answers
   `{"devices": 2, "sent": 2, "failed": 0, "removed": 0}`. The web app's **Send a test** button
   does the same. At most one test every 10 seconds per account: sooner gets **429** with
   `Retry-After` (seconds) and `a test notification was just sent; try again in N seconds`.

`GET /api/push/devices` lists the account's devices:

```json
{"devices": [
  {"id": 12, "device": "Pixel 8", "kind": "app", "key": "3f9a0c1d2e4b5a69",
   "added_ms": 1762400000000, "last_ok_ms": 1762430000000,
   "server": "ntfy.sh", "allowed": true},
  {"id": 9, "device": "Chrome on Windows", "kind": "browser", "key": "77b1...",
   "added_ms": 1762300000000, "last_ok_ms": null, "server": null, "allowed": true}
]}
```

`allowed: false` means the admin has since removed the app's push server from the list: nothing
is sent to it until the server is allowed again or the app registers through another one.

#### The message

What an app receives (UTF-8 JSON, at most 3,000 bytes before encryption):

```json
{
  "v": 1,
  "type": "event",
  "title": "Person at Front Door",
  "body": "Thu 9:01:12 PM · person, dog",
  "camera": "Front Door",
  "camera_id": 3,
  "actions": [{"action": "pause", "title": "Pause 1 h", "minutes": 60}],
  "event_id": "front_door-1762480872",
  "labels": ["person", "dog"],
  "at_ms": 1762480872000,
  "sent_ms": 1762480874120,
  "thumb": "/api/events/front_door-1762480872/thumb",
  "url": "/?alert=3&cam=Front%20Door&at=1762480872000",
  "tag": "cam-front-door",
  "urgent": false
}
```

A system problem:

```json
{"v": 1, "type": "problem", "title": "Disk nearly full", "body": "Recordings: 97% used",
 "camera": null, "camera_id": null, "actions": [], "event_id": null, "labels": [], "at_ms": 1762480900000, "sent_ms": 1762480900000,
 "thumb": null, "url": "/#settings/system/status", "tag": "system", "urgent": true}
```

| Field | Meaning |
|---|---|
| `v` | Format version, `1`. A later format gets a new number; ignore fields you do not know. |
| `type` | `event` (a camera alert), `camera` (a note about one camera, e.g. it stopped sending or is back), `problem` (a system problem; also the system test from Settings), `notice` (other news: a problem cleared, new cameras found), `test` (the test button). Treat unknown types like `notice`. |
| `title`, `body` | Ready to show. Local server time is in `body`. |
| `camera` | Camera name (`event`, `camera`), else `null`. |
| `camera_id` | Camera id (`event` only), else `null`. Use it with `actions`. |
| `actions` | Buttons to show on the notification (`event` to an admin account's device, or with no accounts; otherwise `[]`). Today one: `{"action": "pause", "title": "Pause 1 h", "minutes": 60}`. When the user taps it, call `POST /api/cameras/<camera_id>/pause` with `{"minutes": 60}` and your token or session. Then confirm in the app, e.g. "Notifications from Front Door paused for 1 hour. It still records and detects." Ignore actions you do not know. |
| `event_id` | The event (`event` only), as in `/api/events`. |
| `labels` | What was seen, e.g. `["person"]` (`event`; may be empty). |
| `at_ms` | When it happened (epoch ms); for other types when it was sent. |
| `sent_ms` | When the server sent it (epoch ms). Compare with `at_ms` to spot a late delivery. |
| `thumb` | Path of the event picture on this server, or `null`. |
| `url` | Path the web app opens on tap. |
| `tag` | A newer notification with the same tag may replace the older one. |
| `urgent` | The camera's alert priority is high or urgent, or a system problem. |

Long messages are cut to fit: `body` first, then `labels` and `thumb`. As a last resort every
field is still there, but `title` is shortened, `body` is empty, `labels` is `[]`, `camera`,
`event_id` and `thumb` are `null`, and `url` and `tag` are the fixed values `"/"` and
`"ambersight"` (`v`, `type`, `at_ms`, `sent_ms` and `urgent` are kept). Text fields can end in
`…`.

Delivery: a time to live of 24 hours (a phone that is off longer misses the message), and
`Urgency: high` for `event`, `camera`, `problem` and `test` (others `normal`). No `Topic` header
is sent, so queued messages are not collapsed.

#### Errors from `POST /api/push/subscribe`

All are plain-text messages you can show; they are **400** unless the table says otherwise:

| Message (examples) | Why |
|---|---|
| `this push server (push.example.org) is not allowed on this AmberSight; an admin can add it under Settings, Notifications, Browser and phone push` | The endpoint's host (and port) is not in the list |
| `this push server (ntfy.sh) needs an https:// address` | http to a public server |
| `plain http:// is only allowed for a push server on the home network or Tailscale, and ntfy.example.com is on the internet; use its https:// address` | http to a host that resolves to a public address |
| `ntfy.sh is a public push server but points to a home-network address; refused (...)` | Looks like DNS tampering |
| `could not look up ntfy.home.lan` | The server cannot resolve the host |
| `the app's keys are not valid (...)` | `p256dh` must be a 65-byte P-256 key and `auth` 16 bytes, base64url |
| `the device name is too long (at most 64 characters)`, `the device name must not hold control or invisible characters` | Bad `device` |
| `kind must be "browser" or "app"` | Bad `kind` |
| `too many devices; remove one first` | 25 devices on this account already |
| `expected {"subscription": ...}` | Body is not the JSON above (bodies over 8 KB get 413) |
| **409** `this push address belongs to another account` | The endpoint is registered to another account (an admin may take it over) |

`POST /api/push/test` answers **429** `a test notification was just sent; try again in N seconds`
(with `Retry-After`) when the account sent one less than 10 seconds ago.

A viewer account may use all of `/api/push/*` for its own devices. On a server without accounts
anyone on the network can.

### Unusual events

When a camera has unusual-activity flagging on and its history is long enough, an event that is
out of pattern for that hour of the week carries the extra label `unusual` in `objects` (alongside
the real labels such as `person`). Filter for it with `GET /api/events?object=unusual`. The
notification text for such an event reads "Unusual: person at Back Gate at 3:12 AM" (push
`title`; the `labels` field includes `unusual`). Apps that show the labels should show it as a
warning, not as an object. When a stranger alert is also unusual, the text is the stranger
headline and the priority is still raised (high in mode `alert`, low otherwise). Mode `alert`
alerts for an unusual event even on a camera that alerts only for strangers, and even for a
known person.

### Camera tampering

A camera with tamper detection on sends a normal-looking camera notification when it is covered,
blurred or moved ("Camera tampering: Garage looks covered or blacked out", with the current
picture), and "Camera view restored: Garage" later. Over push it arrives as a `camera` message
(no `event_id`; `image` is `/api/cameras/:id/tamper/incidents/:n/cur`). Tamper alerts are not
rows in `/api/events`; they are kept per camera (the last 30) and listed by the admin endpoint
below.

### `GET /api/cameras/:id/tamper` (admin)

```json
{
  "enabled": true,
  "state": "ok",
  "refs": { "day": { "set_at": 1762160000 }, "night": null },
  "score": { "covered": 0.03, "blurred": 0.2, "moved": 0.0 },
  "active": null,
  "bad_for": null,
  "light": "day",
  "incidents": [ { "id": 7, "at": 1762100000, "kind": "covered", "light": "night", "cleared_at": 1762100400 } ]
}
```

`state` is `off`, `learning` (no reference for the current light yet, nothing is judged), `ok`,
or the condition that is on: `covered`, `blurred`, `moved` (then `active` is
`{"since": ..., "kind": ...}`). `score` values at 1.0 or more mean the condition is met at the
camera's sensitivity (it must also last the camera's `tamper.seconds`). `bad_for` is how many
seconds the current wrong view has lasted.

`POST /api/cameras/:id/tamper/reference` (admin, no body) stores the camera's picture of this
moment as the reference for the light it shows and clears any active alert:
`{"ok": true, "light": "day"}`; 400 with `error` when there is no picture or it shows too
little. `GET /api/cameras/:id/tamper/ref/day` and `/night` return the reference JPEG (404 when
none). `GET /api/cameras/:id/tamper/incidents/:n/ref` and `/cur` return the reference and the
picture an alert was raised with. These pictures follow the camera access rules like snapshots.

Camera settings (`GET /api/cameras`, `PUT /api/cameras/:id`, admin): `tamper`:
`{"enabled": false, "sensitivity": 3, "seconds": 60}` (sensitivity 1 to 5, seconds 10 to 3600) or
`null`; `unusual`: `{"enabled": false, "sensitivity": 3, "labels": ["person"], "mode": "notice",
"min_days": 14}` or `null`. `labels` are AI labels or `motion` (an event with no label); `mode` is
`tag` (only the label), `notice` (low priority) or `alert` (high priority).

### `GET /api/cameras/:id/unusual` (admin)

```json
{
  "enabled": true,
  "state": "learning",
  "days": 6,
  "days_needed": 14,
  "active_hours": 41,
  "typical": { "person": [0.0, 0.0, "... 168 numbers, Monday 00:00 first ..."] },
  "seen": ["motion", "person", "car"]
}
```

`state` is `off`, `learning` or `ready`. `typical[label]` is, for each hour of the week (index 0 =
Monday 00:00, 167 = Sunday 23:00), the share (0 to 1) of days on which the label usually happens
in that hour (smoothed over the neighboring hours). `POST /api/cameras/:id/unusual/refresh`
reads the events not yet in the history now (this is also done hourly) and returns the same JSON.

### Stranger alerts

A camera can alert only for unknown people and vehicles. Camera setting `strangers` (`GET
/api/cameras`, `PUT /api/cameras/:id`, admin), or `null` for off:

```json
{ "enabled": true, "unidentified_is_unknown": true, "schedule": "22:00-06:00",
  "outside": "normal", "first_seen": false, "first_seen_days": 30 }
```

With `enabled`, an event with a person or vehicle sends an alert only when a face nobody named
was found, an unknown plate was read, or (`unidentified_is_unknown`) a person had no face seen /
a vehicle had no plate read. Events of known faces and known plates are still recorded and
labeled; they just send no alert. `schedule` ("HH:MM-HH:MM", may cross midnight; empty or
absent = all day) limits the filter to those hours; `outside` is what happens outside them:
`normal` (alerts as usual) or `quiet` (no alerts for people and vehicles). `first_seen` alerts
only the first time an unknown face or plate appears within `first_seen_days` (1 to 60) and
implies the unknown-only filter. The alert's title reads "Unknown person at Front Door" or
"Unknown car ABC123 in Driveway" (push `title`; the event's `objects` keep their usual labels).
The AI review (`GET /api/events/:id/ai`) says `known_only` when this filter kept an event quiet.

### Morning summary

One notification a day with what happened since the last summary, worked out from the server's
own events (no cloud). Settings section `digest` (`GET`/`POST /api/settings`, admin; sent only
when it differs from the defaults):

```json
{ "enabled": false, "time": "07:00", "period": "overnight", "night_start": "22:00",
  "channels": null, "cameras": [], "skip_if_empty": true }
```

`period` is `overnight` (from `night_start` to `time`), `24h`, or `since_last` (at most 48 hours).
`channels` is a list of service ids (`push`, `ntfy`, `telegram`, `email`, `pushover`,
`webhook`) or `null` for every service that is set up; `cameras` empty = all. It is sent within
3 hours after `time` (local time of the server, also past midnight), once a day. Events reviewed
as false alarms and unconfirmed events of cameras that hide them are left out. A scheduled summary
that cannot be saved is not sent.

`GET /api/digest/preview` (admin) builds the summary for now without sending it.
`POST /api/digest/send` (admin) builds and sends one now through the chosen services (also while
`enabled` is false) and answers like the preview plus `sent`: `[{"service": "ntfy", "ok": true}]`.
`GET /api/digests` lists the summaries kept (`id`, `created_ms`, `from_ms`, `to_ms`, `auto`,
`range`); `GET /api/digests/:id` (or `latest`) returns one, for the cameras the account may see:

```json
{ "id": 7, "title": "Morning summary: 2 unknown people, 1 package delivered",
  "period_name": "Overnight", "range": "Sat Nov 7, 10:00 PM to 7:00 AM",
  "digest": { "from_ms": 0, "to_ms": 0, "period": "overnight", "overview": "...",
    "cameras": [ { "camera": "Front Door", "events": 4, "line": "People: Anna, 2 unknown; ...",
      "people_known": [{"name": "Anna", "count": 1}], "people_unknown": 2, "people_unidentified": 0,
      "vehicles_known": [], "plates_unknown": [], "vehicles_other": 0, "packages_delivered": 1,
      "packages_picked_up": 0, "loitering": 0, "crossings": 0, "unusual": 0, "tamper": 0,
      "offline_count": 0, "offline_minutes": 0,
      "highlights": [ { "kind": "person", "text": "Unknown person at 3:12 AM", "at_ms": 0,
        "event_id": "front-door-1700000000", "thumb": "front-door-1700000000",
        "url": "/?alert=1&cam=Front%20Door&at=1700000000000" } ] } ] } }
```

Counts are events. A highlight's `url` is a path on this server that opens the event (the same
form as in push messages); `thumb` is an event id for `GET /api/events/:id/thumb`. The push
message is short (title = the summary's one-line overview, body = one line per camera, cut to fit)
and its `url` is `/#digest?id=7`, the web app's page for the full summary; its `type` is
`notice`. Email gets an HTML version with the highlight pictures inline.

### MQTT

**MQTT** (if the owner configured a broker). The topic prefix defaults to `ambersight`.
`<cam>` is the camera slug (lowercase, non-alphanumerics → `_`).

| Topic | Payload | Retained |
|---|---|---|
| `<prefix>/status` | `online` | yes |
| `<prefix>/<cam>/event` | Event JSON (same fields as `/api/events`) | no |
| `<prefix>/<cam>/objects` | Comma-separated labels of the last event | yes |
| `<prefix>/<cam>/snapshot` | JPEG thumbnail of the last event | yes |
| `<prefix>/<cam>/motion`, `/online` | `ON` / `OFF` | yes |
| `<prefix>/<cam>/person`, `/vehicle`, `/animal` | `ON` / `OFF` (what the camera itself reports) | yes |
| `<prefix>/<cam>/asleep` | `ON` / `OFF` (on-demand cameras) | yes |
| `<prefix>/<cam>/package` | `ON` / `OFF` (package present; package-detection cameras) | yes |
| `<prefix>/<cam>/tamper` | `ON` / `OFF` (covered, blurred or moved; cameras with tamper detection; Home Assistant `tamper` binary sensor) | yes |
| `<prefix>/<cam>/faces`, `/face`, `<prefix>/faces`, `<prefix>/faces/event` | Face recognition results (JSON) | mixed |
| `<prefix>/<cam>/zone/<zone>/state` | `ON` while someone is loitering in the area (one per alert area) | yes |
| `<prefix>/<cam>/zone/<zone>`, `<prefix>/<cam>/zone` | A zone event as JSON: `{"event_type": "loitering" \| "in" \| "out", "camera", "zone", "label", "event_id", "at"}` (the second topic carries every zone of the camera) | no |
| `<prefix>/<cam>/plates`, `/plate`, `<prefix>/plates`, `<prefix>/plates/event` | License plate readings (JSON: `state` = first plate with the known name if any, `plates` [{text, confidence, name, known}], `known`, `unknown`, `camera`, `event_id`, `time`; the `/plate` event types are `known` / `unknown`) | mixed |
| `<prefix>/shield/state` | `ON` / `OFF` (armed) | yes |
| `<prefix>/profile/state`, `<prefix>/profile/active` | Manual profile (or `Auto`) / active profile | yes |
| `<prefix>/problems`, `<prefix>/problems/list` | `ON`/`OFF`, `{"problems": [...]}` | yes |

MQTT commands the server accepts:

| Topic | Payload |
|---|---|
| `<prefix>/shield/set` | `ON`/`OFF`/`ARM`/`DISARM`/`1`/`0` |
| `<prefix>/<cam>/trigger` | Seconds to record (default 20, 1–3600) |
| `<prefix>/<cam>/wake` | Wake an on-demand camera |
| `<prefix>/profile/set` | Profile name, or `Auto` |
| `<prefix>/signal/<name>/set` | `ON`/`OFF` |

Notes:

- The `event` topic is published for each event the server would alert on. That roughly means
  AI confirmed it (or AI is off) and the active profile allows alerts. It is published even
  while the camera's notifications are paused or throttled. Events that are kept but not
  alerted on are not published.
- Home Assistant discovery messages go under `homeassistant/...` when enabled.
- Only plain `mqtt://` brokers are supported (no TLS).

**Server-side alert channels** are configured by the admin, not through this API: email,
Telegram, ntfy, Pushover, generic webhook, Home Assistant, image POST, and "action". A phone app
could, for example, have the user point ntfy or a webhook at the app's own push service.

---

### Too many notifications: `GET /api/noisy` (admin)

Once a camera has sent more than 30 notifications in 24 hours and one kind of object in one part
of the picture explains at least half of them (typically passing cars in the top strip of a
street-facing camera), the server suggests what to do. Nothing changes by itself.

```json
{ "suggestions": [ {
  "camera": "Front Door", "camera_id": 3, "sent": 295, "top": "car", "share": 0.8,
  "labels": [["car", 240], ["person", 40]],
  "region": [[0, 0], [1, 0], [1, 0.34], [0, 0.34]], "region_name": "top third",
  "examples": [ { "event_id": "front-door-1700000000" } ],
  "not_real": 4,
  "actions": ["people_only", "ignore_area", "not_now"],
  "text": "Front Door sent 295 notifications in the last 24 hours. 4 of 5 were cars in the top third of the picture, probably passing traffic." } ] }
```

`share` is the part of all notifications that were that object in that region (0..1); `region`
is the suggested area (corners, 0..1), `null` when they did not gather in one place;
`examples[].event_id` works with `GET /api/events/:id/thumb`; `not_real` counts "Not a real alert"
taps on those events; `held_back` counts the camera's events in the same 24 hours that did not
alert because of a remembered spot (AI review `fixed_spot`). The actions are applied with the normal camera call: `people_only` is
`PUT /api/cameras/:id {"alert_labels": ["person"]}`, `ignore_area` adds `region` to
`alert_ignore_polys` (send the old list plus the new one), and `not_now` is
`POST /api/noisy/:id/dismiss` (hides it for 7 days; apps should also call it after applying one of
the others). A skipped notification is recorded in the event's AI review: `alert.why` is
`notify_area` (inside an area that does not notify; never for doorbell presses, sounds or packages) or
`notify_objects` ("Notify me for" objects), and `summary.kind` says the same. `paused`, `alert_window`
and `alert_interval` say a pause, the camera's alert window or the alert interval held it back
(`alert.sent` is false; `summary.kind` is `alerts_off`). Only notifications that were not held back count
in `sent` and in the AI review's `alerted`. The answer can be up to a minute old, unless a camera's
notification settings or a "Not now" changed it. The morning summary carries a `notes` list with the same text.

## 6. Recordings and playback

Recording times are **epoch milliseconds**. This differs from events, which use seconds.

### `GET /api/recordings?camera=&from=&to=`

| Param | Meaning |
|---|---|
| `camera` | Camera name. Without it, every camera the caller may see. |
| `from`, `to` | Epoch ms. Defaults: 0 / no end. |

With `camera`:

```json
{ "camera": "Front Door", "spans": [[1762160000000, 1762163600000], [1762163610000, 1762167200000]], "segments": 1440, "bytes": 2147483648 }
```

Without `camera`, the response is `{"cameras": [ <the object above>, ... ]}`. `spans` are
recorded stretches, with gaps of 1.5 s or less merged.

### Which days have recordings: `GET /api/recordings/days`

For a calendar. You send the days (in your own time zone, so daylight saving is yours to handle); the server only looks each one up.

| Param | Meaning |
|---|---|
| `starts` | Comma-separated epoch ms of the first moment of each day, rising, at most 62, no more than 2 days apart, none after tomorrow or so |
| `end` | Epoch ms where the last day ends (after the last start, within 2 days of it) |
| `cameras` | Optional comma-separated camera **names**. Without it, every camera the caller may see. Unknown names and cameras the caller may not see are ignored. |

```json
{ "days": [ { "start": 1762128000000, "rec": true, "alerts": 3 }, { "start": 1762214400000, "rec": false, "alerts": 0 } ], "oldest": 1759276800000 }
```

`rec` is true when any of those cameras has a recording that begins in that day or runs into it. `alerts` is the number of events that began in the day, counted up to 100 per camera (so a busy day reads as a large number, not an exact one); unconfirmed events on cameras set to hide them are not counted. `oldest` is the start of the oldest recording of those cameras, or `null`. A bad `starts` or `end` answers **400**. It is a few indexed look-ups per day and camera, so it is quick even with a million recordings.

### `GET /api/playback`

This returns any stretch of recording as one MP4. It is suitable for a plain video player URL;
the web app uses it directly as `<video src>`. Over HTTPS with the private CA, see
[Video players use the system trust store](#video-players-use-the-system-trust-store).

| Param | Required | Meaning |
|---|---|---|
| `camera` | yes | Camera name |
| `from`, `to` | yes | Epoch ms. `from >= 0` and `to > from`, otherwise 400. The window is capped at **2 hours** (normal) or **24 hours** (fast playback). |
| `quality` | no | `low` = sub stream where recorded |
| `speed` | no | 2–1024: fast playback (keyframes only, retimed) |
| `frame_ms` | no | Fast playback: ms each picture shows, 100–1000 (default 100) |
| `reverse` | no | `1`: the window played backwards. Capped at 10 s, scaled down to at most 960 px wide; `speed` is ignored. |
| `download` | no | `1`: attachment, with the camera name and time burned in as captions (normal speed only) |
| `quiet` | no | `1`: when nothing is recorded in the window, answer `204` with the header `x-none: 1` instead of `404` (the web app uses this so a browser's console stays free of errors; `/api/filmstrip` and `/api/playback/picture` take it too) |

Response: `video/mp4`, with `Range`/206/416 and `HEAD` handled as for clips. Extra headers:

| Header | Meaning |
|---|---|
| `x-clip-start-ms` | Wall-clock time (epoch ms) of the clip's first frame. Keyframe-aligned, so it can be before `from`. For `reverse=1`, clip time 0 is the window's **end**. |
| `x-clip-end-ms` | Wall-clock end of the covered stretch |
| `x-clip-sub` | `1` if the clip is the sub stream |
| `x-clip-reverse` | `1` for reversed clips |

Map player time to wall-clock time as `x-clip-start-ms + position`. The web app sends a 1-byte
`Range: bytes=0-0` probe first to read these headers. It plays **10-minute** windows and loads
the next one before the current one ends. A clip may cover less than the requested window where
the recording has breaks.

Errors:

| Status | Body or header |
|---|---|
| 400 | `camera, from and to are required` / `bad time window` |
| 404 | `no recording covers that window` |
| 503 | `busy, try again`, with `Retry-After: 1` |
| 500 | `playback failed (details are in the server log)` |

### Pictures from recordings

| Endpoint | Params | Returns |
|---|---|---|
| `GET /api/still` | `camera`, `at` (ms), `download=1` optional | Full-resolution JPEG at that moment. 404 `no recording at that time`. |
| `GET /api/preview` | `camera`, `at` (ms), `w` (px, default 320, clamped 64–640) | Small JPEG of the keyframe at or before `at`. Header `x-preview-ms` gives that keyframe's time. `Cache-Control: private, max-age=3600`. 503 with `Retry-After: 2` when busy. |
| `GET /api/filmstrip` | `camera`, `from` (ms), `step` (ms), `n` (default 60), `w` (default 160) | One JPEG grid of `n` pictures. Headers `x-sprite-cols`, `x-sprite-w`, `x-sprite-h`, and `x-sprite-times` (comma-separated ms). |

### `GET /api/seek?camera=&at=`

`at` is in **seconds** here. It returns which recording file covers that moment:

```json
{ "found": true, "segment": "1762166400000.mp4", "start_ms": 1762166400000, "end_ms": 1762166700000, "offset_ms": 12000, "size": 52428800 }
```

or `{"found": false}`. Most apps will not need this; `/api/playback` does the work.

### `GET /api/export`

Uses the same `camera`, `since`, `until` (seconds) and `object` filters as `/api/events`. It
also takes the newer list filters `cameras`, `protect`, `objects` and `review` (same meaning as
there), or `ids=a,b,c` to export exactly those events (at most 200; none valid is **400**
`no event ids given`). When any of these is used, only events the account may see are included
and a camera name is not required. It returns `application/zip` (`ambersight-events.zip`): one MP4 per event plus `events.csv`.

- Unconfirmed events are included.
- At most **500** events (**200** when `ids` is used). After about **1 GB** of video, the remaining events are listed in the
  CSV without their video.
- The ZIP is prepared in full before the download starts (it then arrives with a
  `Content-Length`), so expect a wait for big ranges. Two exports run at a time; a third gets
  **503** with `Retry-After: 30`.
- Restricted viewers must name a camera, or use `cameras`, `ids` or the other list filters.

### Recordings whose files are gone: `/api/storage/missing` (admin)

For an admin tool: finds recordings the database lists whose files are no longer on disk and
removes them from the database (never a file). Admins only, also for reading (403 for viewers
and device tokens); the POSTs follow the origin rule.

- `GET /api/storage/missing`: `{detected, job, scan, applied}`. `detected` is what the start-up
  check noticed (`{at_ms, notes: [{name, label, path, state, sampled, missing, text}]}`, `state`
  `not_found`, `not_mounted`, `mostly_missing` or `not_configured`; checked again in the
  background when the folders change).
  `job` is `{state: idle|scanning|applying|failed, phase, done, total, error}`. `scan` is the
  last dry run: `{id, made_ms, took_ms, rows, missing, missing_bytes, events_no_video,
  protected_no_video, events_part_video, events_no_video_all, stale, folders: [{name, label,
  path, configured, exists, state, not_mounted, rows, bytes, missing, missing_bytes, unknown,
  case_differs, oldest_missing_ms, newest_missing_ms, absolute_paths, confirm_why,
  needs_confirm, whole_cameras, cameras: [{camera, rows, missing, missing_bytes}]}]}`. Folder
  `state` is `ok`, `some_missing`, `all_missing`, `not_found` or `not_configured`; `confirm_why`
  lists why it needs naming: `not_found`, `not_configured`, `all_missing`, `most_missing`,
  `cameras_all_missing` (see `whole_cameras`), `not_mounted` (Linux: on a drive that is set to be
  mounted there but is not). `events_no_video` counts the folders that need no naming, `events_no_video_all` all
  of them. `applied` is the last removal: `{scan_id, removed, removed_bytes, found_again, moved,
  case_differs, skipped_folders, skipped_rows, events_marked, backup, took_ms}` (`backup` is
  `null` when there was nothing to remove).
- `POST /api/storage/missing/scan`: start a dry run (reads only). **202**; **409** while one runs
  (here or `ambersight storage prune-missing` on the command line).
- `POST /api/storage/missing/apply` with `{"scan_id": "...", "include": ["stored"]}`: back up the
  database, then remove the missing recordings that dry run found (each is looked for again
  first). Folders with `needs_confirm` are left alone unless named in `include` (`""` = the data
  folder). **202**; **409** when `scan_id` is not the latest dry run, it is over an hour old, the
  folders changed since, or something is running. Poll `GET` for the result.

Events whose video was all in the removed recordings keep their row and picture; their `clip`
becomes `null` (shown as "No clip"). Protected events are not changed.

---

## 7. Control

Every write in this section is **admin-only** (403 for viewers). The exception is reading PTZ
capabilities and presets.

### PTZ

| Request | Body / params | Response |
|---|---|---|
| `GET /api/cameras/:id/ptz/caps[?refresh=1]` | none | `{"ptz": true, "zoom": true, "aim": false, "pan_tilt": true, "presets": [{"token": "1", "name": "Gate"}]}`, plus `"error": "…"` if something failed |
| `GET /api/cameras/:id/ptz/presets` | none | `{"presets": [{"token", "name"}]}` |
| `GET /api/cameras/:id/onvif` | none | **admin**. `{"state": "configured" or "found" or "searching" or "none" or "na", "configured": 80, "found": null}`. `found` is a port AmberSight located itself when none is saved (it is used for PTZ but not written to the config; save it by updating the camera with `onvif_port`). `none` = nothing answered (the camera does not offer ONVIF); `na` = not a network camera. A port with a real SOAP answer is preferred over one that only says 401. |
| `POST /api/cameras/:id/onvif/find` | none | **admin**. Searches the common ONVIF ports now (can take up to about 15 s) and returns the same as the GET. |
| `POST /api/cameras/:id/ptz` | JSON chosen by `action`, see below | `{"ok": true}` unless noted |
| `POST /api/cameras/:id/ptz/aim` | `{"x":0.7,"y":0.4}` or `{"x","y","w","h"}`: fractions of the picture (point, or box center and size) | `{"ok": true}`. Needs `aim` in caps. |

`POST /api/cameras/:id/ptz` actions:

```json
{ "action": "move", "pan": 0.5, "tilt": 0, "zoom": 0, "timeout_ms": 500 }
{ "action": "stop" }
{ "action": "absolute", "pan": 0.1, "tilt": -0.2, "zoom": 0 }
{ "action": "status" }
{ "action": "goto", "preset": "1" }
{ "action": "set_preset", "name": "Gate" }
{ "action": "remove_preset", "preset": "1" }
{ "action": "tour", "presets": ["1", "2"], "dwell_s": 10 }
{ "action": "stop_tour" }
```

- `move` is continuous velocity. Values are in the ONVIF -1..1 range (not range-checked by the
  server). The movement always stops on its own after `timeout_ms` (default 500, clamped
  100–10000), so send `move` repeatedly while the user holds a control.
- `status` returns `{"pan", "tilt", "zoom", "moving", "tour"}`.
- `set_preset` returns `{"ok": true, "token": "…"}`.
- `stop_tour` returns `{"ok": true, "was_running": bool}`.
- `tour` dwell is clamped 2–3600 s.
- Errors are JSON `{"error": "…"}`: 404 no such camera, 400 no PTZ / bad request, 502 camera
  error.
- PTZ is not available for linked-server cameras.

### Arm / disarm: `POST /api/shield`

| Body | Effect | Response |
|---|---|---|
| `{"armed": true}` | Arm now | `{"armed": true}` |
| `{"armed": false}` | Disarm (motion detection off; events stop) | `{"armed": false}` |
| `{"armed": true, "delay": true}` | Arm after the configured arm delay | `{"armed": false, "arming_until": <epoch s>}`, or `{"armed": true, "arming_until": null}` if there is no delay |

`armed` defaults to `true` when omitted. The current state is in `/api/status` (`armed`,
`arming_until`).

### Profiles

`GET /api/profiles` (viewers allowed):

```json
{
  "profiles": ["Home", "Away", "Night"],
  "active": "Home",
  "scheduled": "Home",
  "manual": null,
  "default_profile": "Home",
  "schedule": [],
  "sun": { "sunrise": "06:58", "sunset": "17:12" }
}
```

`manual` is `{"name", "until"}` (`until` in epoch seconds or `null`) while a profile is held by
hand. The structure of `schedule` entries is configuration and is not described here.

`POST /api/profile` (admin) takes `{"name": "Away", "minutes": 60}`. Omit `minutes` (or send
null) to hold the profile until it is changed. Send `{"name": null}` to return to the schedule.
`minutes` is clamped to 1–525600. The response is the same JSON as `GET /api/profiles`. An
unknown name returns 400 `unknown profile '…'`.

### Pause a camera's notifications: `POST /api/cameras/:id/pause` (admin)

Body `{"minutes": 60}`. `0` resumes; omitted or `null` pauses until resumed. Motion, recording
and events carry on; only notifications stop. Event notifications offer this as a "Pause 1 h"
button (see `actions` under "Push notifications for apps"). `GET /api/cameras` shows the state in
`alerts_paused` and `alerts_paused_until`: show it as "Notifications paused" (never just "Paused":
the camera is still recording).

```json
{ "camera": "Front Door", "alerts_paused": true, "alerts_paused_until": 1762170000 }
```

### Record a clip now: `POST /api/clips/record` (admin)

Body `{"camera": 0, "seconds": 20}`. `camera` is the camera **id** and defaults to 0; `seconds`
defaults to 15, minimum 5. This forces motion on the camera for that long, so a normal event and
clip are made. Response: `{"recording": true, "camera": 0, "seconds": 20}`. An unknown camera
returns **200** with `{"error": "camera not found"}`.

---

## 8. Presence (geofencing)

A phone app can tell the server whether the phone is at home. When every phone is away, the
server arms (or switches to an "away" profile); when someone comes back, it switches back. This
is an alternative to Home Assistant presence (MQTT `select.ambersight_profile` / the shield
switch) for people without Home Assistant. **Any signed-in account can report**, viewers and
restricted accounts included; no admin needed, unless the admin limits which accounts count
(`accounts` below). Each account only reports, sees and removes its own phones.

The admin turns it on and picks the actions in Settings > Schedules & presence > Presence
(phones), or in `config.toml`:

```toml
[presence]
enabled = true
away_profile = ""          # profile when everyone has left ("" = none)
home_profile = ""          # profile when someone is back ("" = back to the schedule)
arm_when_away = true       # default: true when away_profile is ""
disarm_when_home = false
away_delay_minutes = 10    # 0-120
stale_hours = 12           # 1-168
accounts = []              # accounts whose phones count ([] = every account)
```

Phones keep reporting while `enabled` is false. Nothing acts until it is true. With `accounts`
set, only those accounts' phones count, and a report from any other account gets **403**
`this account does not report presence on this server` (nothing is stored). Apps should then
stop reporting and tell the user. On a server without accounts, `accounts` is ignored.

### `POST /api/presence`

```json
{ "device_id": "6f1c2a9e-3b7d-4a51-9e0f-2c8d4b7a1e55", "name": "Ann's Pixel", "home": true }
```

- `device_id`: 1–64 characters `A-Z a-z 0-9 - _`. Generate a random UUID on first run and keep
  it on the phone. It is per account: the same id under two accounts is two phones.
- `name`: shown to admins. Let the user choose it, defaulting to the phone's model. It is
  trimmed, up to 64 characters, and empty becomes `Phone`.
- `home`: `true` inside the home geofence, `false` outside. Required.

Response:

```json
{ "ok": true, "enabled": true, "home_now": false, "state": "away", "rule": "armed" }
```

`home_now` is whether anyone is home after this report (`null` when unknown). `rule` is what
leaving does: `"armed"`, `"profile:<name>"`, `"profile:<name>+armed"` or `"none"` (also when
presence is off). Errors: 400 bad `device_id` or `name`, 403 this account's phones do not
count (see `accounts`), 409 too many phones (20 per account, 200 per server). The text says
which. Sending the same `device_id` again only updates it.

This is a POST, so the [Origin rule](#origin-rule-csrf-protection) applies. Native HTTP clients
send no `Origin` header, which is fine. Sign in as usual, with the session cookie or HTTP Basic.

### `GET /api/presence`

Viewers get their own phones and, if their account's phones count, the overall state:

```json
{
  "enabled": true,
  "anyone_home": true,
  "state": "home",
  "stale_hours": 12,
  "devices": [
    { "user": "ann", "device_id": "6f1c…", "name": "Ann's Pixel", "home": true,
      "updated_ms": 1762430000000, "created_ms": 1762000000000, "stale": false, "counted": true }
  ]
}
```

A viewer whose account does not count gets no `anyone_home` or `state` (only their own phones,
if any). `counted` is false for a phone whose account does not count (not in `accounts`, or no
longer an account). Admins get every account's phones plus `"pending_away_at"` (epoch ms when
the away action is due, or `null`), `"last_action"` (`{"at_ms", "event": "away"|"home", "did",
"reason"}` or `null`), `"rule"` and `"accounts"`. Phones that have not reported for 30 days are
removed.

### `DELETE /api/presence/:device_id`

Removes one of your phones (do this when the user turns presence off in the app or signs out).
Admins may add `?user=<account>` to remove another account's phone. A viewer naming another
account gets 403. Response `{"ok": true, "deleted": "<device_id>"}`, or 404 if there was no
such phone. Deleting an account removes its phones.

### How the server decides

- **Who counts:** a phone of a counted account (every account, or those in `accounts`) that
  reported within `stale_hours` (default 12). Older reports are ignored. Someone is home if any
  counted phone says `home: true`, so any counted account's phone coming back triggers the home
  actions. If no phone counts, the state is **unknown**: nothing acts then, and a running away
  timer is called off.
- **Only changes act.** When the state goes from home to away, a timer of
  `away_delay_minutes` (default 10) starts. If someone comes back before it ends, nothing
  happens. When it ends, the server switches to `away_profile` (if set) and arms (if
  `arm_when_away`, which is the default when there is no away profile). Arming here is
  immediate: the shield's own arm delay does not apply.
- **Coming back** acts at once. The server switches to `home_profile` if one is set; it then
  holds that profile until it is changed by hand or the next away. If it is not set, and the
  away profile set by presence is still the one chosen, it goes back to the schedule. It
  disarms only if `disarm_when_home` is true. The away action replaces a profile chosen by
  hand.
- **A change by hand stays.** Reports that do not change the state (heartbeats) never repeat an
  action. So if the user disarms while everyone is away, it stays disarmed until the next
  away → home → away.
- **The last known state is remembered** through unknown spells and across restarts. A trip
  longer than `stale_hours` still ends with the home actions when the first phone reports home.
  An away timer that ran out while the server was down acts at the first check after the start,
  if everyone is still away. Only the very first state after presence is turned on is a
  starting point; turning presence off forgets it. Changing `away_delay_minutes` applies to a
  running timer.
- Every arm, disarm and profile change is in the audit log with who did it: `(presence)`,
  `(mqtt)`, `(schedule)`, `(startup)`, or the account name. Account names cannot start with
  `(`. Presence reports themselves are not audited.
- With MQTT on, the server publishes `<prefix>/presence/state` (`home`, `away` or `unknown`,
  retained). With discovery on and presence enabled, Home Assistant gets a binary sensor
  "Someone home" (device class presence). It is unavailable while the state is unknown.

### Client approach (suggestions)

Report on every geofence enter and exit, and send a heartbeat every few hours with the current
state, so the phone does not go stale. If a report fails, retry with backoff and send the
latest state, not a queue of old ones.

**Android**

- `GeofencingClient` (Google Play services location), with `ACCESS_FINE_LOCATION` and
  `ACCESS_BACKGROUND_LOCATION` ("Allow all the time"). Ask for background location in a
  separate step, after foreground location, as Android requires.
- One circular geofence around home, radius **150 m or more** (smaller ones fire late or not at
  all). Transitions `GEOFENCE_TRANSITION_ENTER | GEOFENCE_TRANSITION_EXIT`, with
  `setInitialTrigger(INITIAL_TRIGGER_ENTER)` so a phone already at home reports once.
- In the `BroadcastReceiver`, enqueue a `WorkManager` one-time job (network constraint) that
  POSTs the report. Do not do network work in the receiver itself.
- A `PeriodicWorkRequest` every 2–4 h as the heartbeat. It sends the last known state, or asks
  `getCurrentLocation` and compares it with the home point.
- Geofences are cleared on reboot, on app update, and when location is turned off. Register them
  again on `BOOT_COMPLETED` and `MY_PACKAGE_REPLACED`, and when the app starts.

**iOS**

- `CLLocationManager` region monitoring with a `CLCircularRegion` (radius 150 m or more,
  `notifyOnEntry` and `notifyOnExit`). Needs "Always" location permission and the
  `NSLocationAlwaysAndWhenInUseUsageDescription` string.
- Report in `locationManager(_:didEnterRegion:)` / `didExitRegion`. iOS wakes the app briefly,
  so start the request at once, inside a background task (`beginBackgroundTask`).
- Use `requestState(for:)` at launch to get the current state.
- Heartbeat: `startMonitoringSignificantLocationChanges()` and/or a `BGAppRefreshTask`. Either
  one sends the current state now and then. iOS decides how often, so a `stale_hours` of 12 or
  more suits iPhones.

**Both:** let the user choose the phone's name; keep `device_id` in app storage (a new install
means a new id, and the admin can remove the old one); offer "Stop reporting" that calls
`DELETE /api/presence/:device_id`. Location never leaves the phone: only `home: true/false` is
sent.

**Privacy:** explain the background-location use plainly before asking for it (what it is for,
that only home or away is sent, and to which server). Report only `home` true/false, never
coordinates. When the user turns reporting off, call `DELETE /api/presence/:device_id` so the
phone stops counting. Tell users that other signed-in accounts whose phones count can see
whether anyone is home (not who, or where).

---

## 9. Linked sites (remote cameras)

Two AmberSight servers can be linked by their admins. Each side then decides whether to share its
cameras and whether to show the other's. Shown cameras appear in this server's `/api/cameras`:

```json
{
  "id": 1010003,
  "name": "Doorbell (Cabin)",
  "short_name": "Bell",
  "remote": { "site": "Cabin", "link": "a1b2c3", "camera": "Doorbell" },
  "groups": [], "group": "",
  "online": true, "sleeping": false, "enabled": true, "on_demand": false, "audio": false,
  "overlay_text": null, "overlay_pos": null, "overlay": false,
  "ptz": false, "record": false, "motion": false, "clip_count": 0
}
```

- Remote camera ids are **1,000,000 and up**. The name has the site appended in parentheses.
  `remote.camera` is the name on the other server.
- **Works through this server:** `/api/live/:id/info`, `/api/live/:id/ws`, `/api/live/:id/jpeg`,
  `/api/snapshot/:id`, and `/api/live/:id` (which returns a single JPEG for remote cameras). The
  server fetches them from the other server only while someone watches.
- **Does not work:** `/api/live/:id/stream`, wake, PTZ, talk, events, recordings, playback and
  clips. Those stay on the server that owns the camera.
- Restricted viewers never see remote cameras.
- To see a remote camera's events and recordings, switch to its server:
  1. `GET /api/links` returns `{"name": "<this server>", "links": [{"id", "name", "online"}]}`.
  2. `POST /api/links/:id/open` (allowed for viewers) returns
     `{"url": "https://other.example:8090/#pair=<code>", "name": "Cabin"}`.
  3. Redeem the code on the other server with `POST /api/pair`, exactly as in section 2. The new
     session there gets the same account name, never admin unless the user is admin here too.
     The other server has its own CA, which your app must trust separately.
     Each server's admin chooses, per link, whether such sign-ins are allowed: for everyone,
     never as admin, or not at all. When they are off, `open` answers **403** and the user must
     sign in on the other server directly.

  If the other server has no accounts, the URL has no `#pair=` part.

---

## 10. Errors and limits

**Error bodies are not uniform.**

- Most errors are short **plain-text** messages (for example `authentication required`,
  `camera not found`).
- PTZ, AI review and a few other endpoints return JSON `{"error": "…"}`.
- A few older endpoints return **200** with `{"error": "…"}`, for example `/api/clips/record`.

Use the status code first, then show the text.

| Status | Meaning |
|---|---|
| 400 | Missing or bad parameter |
| 401 | Not signed in / session expired / wrong Basic password or token (`authentication required`); or the right password of a two-step account where only a device token works (`this account uses two-step codes...`) |
| 403 | Admin required, other-origin request, or unknown host name in open mode |
| 404 | Not found, **or** a camera the account may not see |
| 409 | Presence: too many phones (20 per account, 200 per server); push: the endpoint belongs to another account; restart: not running as a service, or an update is being installed |
| 416 | Range not satisfiable |
| 429 | Sign-in lockout (5 failures in 10 min → 10 min); push test sooner than 10 s after the last (`Retry-After`) |
| 502 / 504 | Camera (PTZ) or linked server did not answer properly |
| 503 | Busy (`Retry-After` given), no live stream, or no frame yet |

Limits:

| Item | Limit |
|---|---|
| `/api/login` body | 64 KB |
| `/api/pair` body | 4 KB |
| Pairing code lifetime | 10 min, single use |
| Session lifetime | 30 days |
| Basic-auth result cache | 5 min |
| Playback window | 2 h (normal), 24 h (fast), 10 s (reverse) |
| Open-ended Range reply | to the end of the clip |
| JPEG live fps | 1–15 (default 6) |
| Timeline marks | 20,000 rows unbucketed; at most 200 cameras |
| Export | 500 events (200 by `ids`), about 1 GB of video |
| Live WebSocket backlog | about 256 fragments, then the socket is closed |
| `/api/events` page | 2,000 events |
| MJPEG (`/api/live/:id`) | a slow client skips pictures (none are queued) |
| Remote cameras per linked server | 256 |
| Presence phones | 20 per account, 200 per server; `device_id` and `name` up to 64 characters |
| Push devices | 25 per account (browsers and apps); `device` up to 64 characters; message 3,000 bytes before encryption |

There is no request-rate limiting apart from the sign-in lockout; please keep polling modest.

---

## 11. Minimal client walkthrough

```text
# 0. (HTTPS with private CA) fetch and pin the CA
GET  http://nvr.example:8090/tls/ca.pem

# 1. Sign in
POST https://nvr.example:8090/api/login
     Content-Type: application/json
     {"username":"viewer1","password":"…"}
  -> 200 {"user":"viewer1","admin":false}
     Set-Cookie: as_session=…   (send it as "Cookie: as_session=…" from now on)

# 2. List cameras; remember name -> id for this connection
GET  /api/cameras
  -> {"cameras":[{"id":0,"name":"Front Door","online":true,…}, …]}

# 3. Live view of one camera
GET  /api/live/0/info
  -> {"main":{"mime":"video/mp4; codecs=\"avc1.640033\"",…},"sub":{…},"waking":false}
WS   wss://nvr.example:8090/api/live/0/ws?q=main      (Cookie header on the upgrade)
  <- binary: init segment (ftyp+moov)
  <- binary: moof+mdat, moof+mdat, …                   (reconnect when closed)
  (if main and sub are both null: use WS /api/live/0/jpeg?fps=6 instead)

# 4. Today's events (epoch seconds)
GET  /api/events?since=1762128000&limit=50
  -> {"events":[{"id":"front_door-1762166400","camera":"Front Door","start":1762166400,
                 "end":1762166431,"clip":"/api/clips/front_door-1762166400",
                 "thumb":"front_door-1762166400","objects":["person"],…}]}
GET  /api/events/front_door-1762166400/thumb          -> image/jpeg

# 5. Play the event clip (player URL; send the cookie; Range requests work)
GET  /api/clips/front_door-1762166400                 (add ?quality=low on mobile data)
  -> 200/206 video/mp4

# 6. Optional: scrub around it in the full recording (epoch ms)
GET  /api/playback?camera=Front%20Door&from=1762166380000&to=1762166980000
     Range: bytes=0-0     -> read x-clip-start-ms, then play the same URL
```

---

## 12. Changelog

**New in 0.8.0**

- `[server] trusted_proxies`: requests from a listed reverse proxy are taken to come from the address it puts last in `X-Forwarded-For` (plain, with a port, or in brackets), and its `X-Forwarded-Proto: https` marks cookies `Secure`. Sign-in limits, `GET /api/me/account` `sessions` addresses, the audit log and new-device alerts then show each visitor rather than the proxy.
- `[ai] gpu_memory_mb` and the GPU on Linux (NVIDIA, CUDA 13) and Apple silicon: `GET /api/settings` `ai_status.gpu_note` says why the AI is not on the card (or `null`), and `ai_status.gpus` now lists NVIDIA cards on Linux and the Apple chip on Macs.
- New admin `GET /api/storage/missing`, `POST /api/storage/missing/scan` and `POST /api/storage/missing/apply`: find and remove recordings whose files are gone. An event whose video was all in them keeps its row with `clip: null`. See section 6.

**Since 0.6.0**

- New `GET /api/recordings/days?starts=&end=[&cameras=]` (device tokens may use it): which days hold recordings and alerts, for a calendar. See "Which days have recordings" under section 6.

**Since 0.5.0**

- **Remembered spots.** A fixed object that the AI keeps taking for a person (or a car parked between events) stops alerting after 3 events in one place; see "Remembered spots" under the AI review. AI review `summary.kind` can be `fixed_spot` (with `summary.best_spot`), and `review.checks[]` has `spot`. `GET /api/noisy` suggestions have `held_back`. New `GET /api/cameras/:id/spots` and `POST /api/cameras/:id/spots/forget` (admin).
- `POST /api/events/:id/review` with `"false_alarm"` and a `label` also remembers the event's boxes of that kind that sat still as a fixed spot for 30 days, and answers with `marked`. Without a label nothing is remembered.
- **The camera's own detection needs AI to agree.** With AI on, a person, vehicle or animal the camera reports confirms only when the AI sees that kind too (or cannot look). AI review `summary.kind` `camera_only`, `review.camera_unconfirmed`. Camera setting `ai_confirm_camera` (off unless set to `true`).
- `GET /api/ai/reviews` (admin) takes `kind`: only events with that `summary.kind` (or `noalert`, none that sent an alert) in `events`, looked for through the whole period rather than only the newest events, at most `list_limit` (300) of them. The answer has `list_limit` and `kind` (the kind used, or `null`). The `camera` object of `GET /api/events/:id/ai` has `confirm_camera` (the camera's `ai_confirm_camera` now). `GET /api/ai/model` `models[]` have `downloaded`. `GET /api/health` `cameras[]` have `last_error_at` (epoch seconds of `last_error`, or `null`).

**Since 0.3.0**

- **New device sign-in alerts and the list of signed-in devices.** A sign-in answer (`POST /api/login`, `/api/login/totp`, `/api/pair`) also sets a long-lived `as_device` cookie (HttpOnly, SameSite=Strict, about two years, Secure over TLS). It gives no access: it only lets the server say "this browser signed in to this account before". Keep your client's cookie jar if it has one; without it, each sign-in from a fresh jar is a new device and admins are told. `GET /api/me/account` has `sessions`, and `POST /api/me/sessions/:id/sign-out` and `POST /api/me/sessions/sign-out-others` sign devices out (session only; device tokens get 403). A password used directly in HTTP Basic or on RTSP is noted once per account and network (an IPv6 /64) without a cookie; behind a reverse proxy that is one network (unless the proxy is listed in `[server] trusted_proxies`). A client that signs in on every run, without sending `as_device` back, alerts admins every time: keep the cookie, or use a device token. Database schema 24; a build from before it still runs on the same database.
- **Doorbell presses, sounds and packages always notify**, whatever `alert_labels` and `alert_ignore_polys` say. `alert_labels` no longer silences a camera with AI off.
- AI review `alert.why` can be `paused`, `alert_window` or `alert_interval`, with `alert.sent` false; `summary.kind` is then `alerts_off`, and such events no longer count as sent in `GET /api/noisy` or `alerted`. Rows from before this keep what they say.
- `GET /api/noisy` is kept on the server for up to a minute. `GET /api/cameras/:id/onvif` is admin only. `GET /api/cameras/:id/netinfo` shows the maker and model with a port AmberSight found itself, and its `onvif_port` can be that port.
- `POST /api/discover/preview` with a saved login's username and a blank password uses the saved password, for a host that login signed in to through `POST /api/discover/autologin` in the last hour; for any other host it answers **403** and does not try the camera.
- Event push `actions` are `[]` on a viewer's devices. ONVIF stream addresses that are not rtsp, rtsps, http or https are dropped from `POST /api/discover/inspect` and `autologin`.
- `POST /api/settings`: everything is checked before a password is asked for or the old alert destinations are told, so a change that would be refused gets its refusal first.
- **Two-step codes.** `POST /api/login` can answer 200 with `{"totp_required": true, "ticket"}` and no cookie: finish at `POST /api/login/totp`. Check for it. The password of such an account no longer works in HTTP Basic or RTSP (401), and `/api/whoami` has `two_step_required`.
- **Some admin changes take the admin's own password**, in the same request (`my_password`, `my_code`): creating or promoting an admin and setting another account's password (`POST /api/users`), `POST /api/users/:name/two-step/off`, `POST /api/users/:name/pair` for an admin or two-step account, `POST /api/settings` changing a command or where or whether alerts arrive (including `alerts.link_url`, `alerts.system_alerts`, `alerts.system_alert_minutes`, `alerts.pushover_sound`, a server off `push.app_hosts`, and `digest` and `watchdog` settings that stop or delay delivery), `PUT /api/offsite` changing where backups go, `POST /api/links`, `POST /api/links/code`, and `PUT /api/links/:id` with `"signin": "all"`. **403** without them. See "Your account".
- **Password checks wait in line.** `POST /api/login`, a password in HTTP Basic, and `/api/whoami` with one can answer **503** `many sign-ins are being checked right now`: try again in a moment. It does not count as a wrong password. A Basic password checked in the last 5 minutes needs no check. RTSP answers 503 the same way.
- `POST /api/users` refuses your own password (400). `POST /api/clips/record` records an hour at most. `POST /api/login` can answer `"recovery_only": true`. `POST /api/login/totp` can take up to 20 seconds with a recovery code, and can answer **409** and **503**. New account names are limited (see "Changes that take the admin's own password").
- **Device tokens.** `Authorization: Bearer ast_…`, or as the password in Basic for RTSP (viewer scope only) and old clients. Per device, revocable, survive password changes. They may use only the listed routes, everything else is 403 `sign in with a password for this`, and that includes `/api/push/…` and `/api/presence`. `/api/whoami` has `by_token`. New `/api/me/account`, `/api/me/tokens`, `/api/me/two-step/…`, and for admins `/api/users/:name/tokens` and `/api/users/:name/two-step/off`. `POST /api/me/password` takes `code`. `GET /api/users` has `two_step` and `tokens`.
- Push payloads for events have `camera_id` and `actions` (a "Pause 1 h" button your app should show); see "Push notifications for apps".
- New camera field `alert_labels` ("Notify only for" objects).
- `GET /api/cameras/:id/netinfo` (admin): a camera's network details.
- `ptz` can turn true by itself: cameras with no `onvif_port` are searched for one. New `onvif_port` field, `GET /api/cameras/:id/onvif` and `POST /api/cameras/:id/onvif/find`.
- New camera field `alert_ignore_polys`; `GET /api/noisy`, `POST /api/noisy/:id/dismiss`; AI review `alert.why` and `summary.kind` can be `notify_area` or `notify_objects`; the morning summary has an optional `notes` list.

- `POST /api/discover/preview {url, sub_url?, username, password}` (admin): one JPEG frame from a camera that has not been added, RTSP addresses only.
- Accounts can be disabled: `POST /api/users {"username", "disabled": true}` (admin; `GET /api/users` lists `disabled`). A disabled account gets 401 everywhere, its sessions and sign-in codes end, running video stops within seconds, and it can only be enabled by an admin. Your app should treat 401 as "signed out". Any change to an account (password, role, cameras) also ends video that account has open: reconnect after it.
- A linked server's sign-in does not reach an account that is an admin here unless an admin set that link to `"signin": "all"` (default `"viewers"`): `/api/links/:id/open` then returns the address without a sign-in code. `GET /api/whoami` with a wrong Basic password now counts toward the lockout and answers 429 while locked.
- Recording folders in `GET/POST /api/settings` `storage.folders[]` have an optional `private` (true, false, or absent = never touched): only administrators and AmberSight can open the folder. It is applied in the background after the save. `GET /api/storage/privacy` (admin) returns `folders[]` with the real `state` of each (`private`, `open`, `applying`, `failed`, `refused`, `unsupported`, `missing`) and a `detail` for all but the first two; `program` and `data` are read-only lines for the program folder and the data folder, `{path, state, detail}`, where `state` can also be `skipped` (left alone, e.g. a program folder shared with other programs) or `na` (the program folder on Linux and macOS); `?path=` adds a `probe` for a folder not saved yet (`exists`, `empty`, `default_private`, `state`, `detail`). A folder added without `private` gets true when it is new or empty and false when it already holds files. Turning it off needs no password.
- `GET /api/storage/plan` is served from a one-minute cache (`age_seconds` says how old); add `?force=true` to recalculate. `GET /api/ai/reviews`: each camera's `total` and `alerted` now count the whole period; `sampled` is how many of its newest events (at most `per_camera_limit`) the other counts come from.
- Saving only notification settings of a camera (`alert_labels`, `alert_ignore_polys`, `alert_interval`, `alert_schedule`, `alert_channels`, `alert_priority`, groups) no longer restarts its stream.

**2026-11-09**

- `GET /api/health`: new `short` (a few words per warning) and `cameras[].last_error_plain`; warnings no longer contain stream addresses or the watchdog's technical wording.
- `GET /api/storage/plan`: new top-level `days_kept` (days kept counting each capped folder together with the archive it moves old recordings to, limited by `retention_days`) and `folders[].days_total` (that figure for one folder, with no retention limit).
- `GET /api/playback`, `/api/filmstrip`, `/api/playback/picture`: new `quiet=1` (nothing recorded answers `204` with `x-none: 1`, not `404`).
- Settings JSON no longer prints 32-bit numbers with extra digits (`ai.min_score` is `0.7`, not `0.699999988079071`; also `ai_min_score`, `audio_margin_db`, `audio_min_db`, faces `match_score`, plates `min_confidence`).
- A recording folder with no `private` value whose folder is closed by hand is opened again when `private` is set to `false`; one that is open is left as it is.
- `GET /api/ai/reviews`: answered from a small summary table (filled in the background after an upgrade); the same fields as before.

**0.2.1 (clips after an ffmpeg fallback)**

- `GET /api/clips/{id}` now has a clip for events a camera recorded through ffmpeg. When the requested stream's recordings cannot be used, the clip comes from the other stream, so its resolution (and whether it has audio) can differ from the usual one.

**2026-11-07 (review fixes)**

- AI review `alert.why` can be `quiet_rule` (and `summary.kind` `quiet_rule`) when a quiet alert rule drops a held alert.
- Camera rename/removal updates `alert_rules[].cameras` and `lpr.cameras`.
- `object` filters no longer match names inside rule, zone and plate labels; MQTT `objects` replaces commas inside a label with a space.

**2026-11-07 (clip ranges)**

- `GET /api/clips/:id` and recorded-video downloads: an open-ended `Range: bytes=N-` now answers
  with the whole rest of the clip instead of at most 8 MB. Android's ExoPlayer read only the
  first 8 MB of an assembled clip and failed with "Source error" on longer clips.

**2026-11-07 (follow, alert rules)**

- New `GET /api/smart/follow` (viewer, their cameras): the same person or vehicle on other cameras around an event, as a time-ordered path with scores.
- New `/api/alert-rules` (admin): rules in plain words with test preview, quiet rules; events get `rule: <name>` / `quiet: <name>` in `objects`. New `alert_rules` list in the settings file.

**2026-11-07 (sharing streams)**

- New RTSP server for other apps (Home Assistant, Scrypted, VLC, other NVRs): `rtsp://host:8554/<camera>[/sub]`, H.264/H.265 passed through, AAC audio, TCP interleaved, HTTP Basic with AmberSight accounts (see "Sharing streams with other apps"). Off by default: `[restream] enabled`, `port`, and `share = true` per camera.
- New `GET` / `PUT /api/restream` (admin) for the settings and state. `/restream` is admin-only like `/settings`.

**2026-11-07 (alert zones)**

- `GET /api/cameras`: new `named_zones` (loitering areas and crossing lines) and
  `zones_occupied`. Saving a camera accepts `named_zones` (checked: names, points, direction,
  seconds, labels, window). New `[ai]` settings `zone_interval_ms` and `zone_hold_seconds`.
- Zone events are normal events tagged `loitering: <name>` or `crossed: <name> (in|out)`.
- MQTT: `<prefix>/<cam>/zone/<zone>` and `/zone` (events) and `<prefix>/<cam>/zone/<zone>/state`
  (areas); Home Assistant discovery adds an occupancy sensor per area and an event entity per line.

**2026-11-07 (license plates)**

- New license plate reading (off by default; per camera): events with a vehicle get plate
  labels in `objects` (`plate ABC123 (Mom's car)`, `unknown plate XYZ789`), and the readings
  are in new `GET /api/plates`, `/api/plates/recent`, `/api/plates/:id/img` and
  `/api/plates/known` (admin; the other plate reads are for viewers, limited to their cameras); admins manage known plates and the
  `lpr` settings section. MQTT adds `<prefix>/<cam>/plates`, `/plate`, `<prefix>/plates`,
  `<prefix>/plates/event` and Home Assistant entities "Last plate" and "Plate".

**2026-11-07 (stranger alerts, morning summary)**

- New camera setting `strangers` (only alert for unknown faces / plates, optional hours, optional
  first-time-seen); alert titles "Unknown person at ..." / "Unknown car ABC123 in ...".
  `GET /api/events/:id/ai` can answer `known_only`.
- New morning summary: settings section `digest`, `GET /api/digest/preview`,
  `POST /api/digest/send`, `GET /api/digests`, `GET /api/digests/:id`. Push `url` of a summary is
  `/#digest?id=N`. Database schema 20.

**2026-11-07 (tamper detection, unusual activity)**

- New camera tamper detection: settings `tamper` per camera, `GET /api/cameras/:id/tamper`,
  `POST /api/cameras/:id/tamper/reference`, reference and alert pictures, MQTT
  `<prefix>/<cam>/tamper` and a Home Assistant sensor. Alerts go through the camera's usual
  notification services.
- New unusual-activity flagging: settings `unusual` per camera,
  `GET /api/cameras/:id/unusual`, `POST /api/cameras/:id/unusual/refresh`. Out-of-pattern events
  get the label `unusual` (filter with `object=unusual`) and "Unusual: ..." notification text.

**2026-11-06 (HTTPS settings)**

- `GET /api/settings`: the `server` section adds `tls_names`, `tls_cert`, `tls_key`, `running`
  (`tls`, `mode`), `restart_needed` and `restarts_itself`. `POST /api/settings` accepts
  `server` with `tls`, `tls_names`, `tls_cert`, `tls_key` (admin; checked, see "Turning HTTPS
  on or off"). `restart_needed` in the answer is now `true`/`false` (was an always-empty list).
- New `POST /api/system/restart` (admin): restarts AmberSight when it runs as a service; 409
  otherwise, and 409 while an update is being installed.
- The `as_session` cookie's `Secure` flag (sign-in, QR sign-in and sign-out) follows whether the
  request came over HTTPS, not the saved `tls` setting.
- Own certificate files: regular files of at most 1 MB; unreadable at start, the private CA is
  used instead (`note` in `GET /api/tls`, `running.note` in the settings). Turning HTTPS on with
  the private CA checks `<data_dir>/tls` first. Settings `server` adds `linked_servers` and
  `paired_helpers`; saving own certificate files with either warns (`warning`).
- Plain HTTP to an HTTPS server now gets a 307 (temporary) redirect instead of 308.

**2026-11-06**

- Push notifications for Android apps through UnifiedPush: `POST /api/push/subscribe` takes
  `"kind": "app"` and `"device"`; apps get their own JSON message (`v`, `type`, `camera`,
  `event_id`, `labels`, `at_ms`, `thumb`, ...); `GET /api/push/devices` adds `kind`, `server`
  and `allowed`; `GET /api/push/key` documented for apps. New setting `[push] app_hosts`
  (default `["ntfy.sh"]`): the push servers apps may use. An endpoint registered to another
  account gets 409 for non-admins; `POST /api/push/test` at most every 10 s per account (429);
  device names may not hold invisible characters; a self-hosted push server removes a device
  only on 410.
- New presence (geofencing): `POST /api/presence` (any signed-in account), `GET /api/presence`,
  `DELETE /api/presence/:device_id`; `[presence]` settings; MQTT `<prefix>/presence/state` and
  the Home Assistant binary sensor "Someone home".
- Arming, disarming and profile changes from the web page, the API, MQTT, the schedule and
  presence are now in the audit log, with who did it (`(presence)`, `(mqtt)`, `(schedule)`,
  `(startup)` or the account).
- Presence: `[presence] accounts` limits which accounts' phones count (others get 403); the
  last known home/away state is kept through unknown spells and restarts; `counted` on each
  phone; phones silent for 30 days are removed. New account names cannot start with `(`.

**2026-11-03**

- `GET /api/cameras`: new `ai_last`, `motion_enabled`, `zone_polys`, `ignore_polys`, `privacy_masks`.
- `GET /api/events`: new fields `review` and `reviewed_at`; new filters `cameras`, `protect`,
  `objects`, `review`, `ids`.
- New `POST /api/events/:id/review` (admin).
- `GET /api/export`: same new filters, and `ids=` (up to 200 events).
- `GET /api/stats`: new `ai`, `net`, `gpu` and `history_1h`; per camera `fps_in`, `cfg_fps`,
  `kbps`, `dropped_1m`, `reconnects`; per folder `free`, `total`, `tier`, `write_bps`.
- `GET /api/health`: may warn that AI object detection is stalled (problem key `ai`).
- `GET /api/timeline/events`: with `bucket`, package and face events are grouped separately.
