//! The RTSP server: other apps (Home Assistant, Scrypted, VLC, another NVR)
//! connect here instead of to the cameras. Media goes out as RTP interleaved
//! on the RTSP connection (TCP); UDP is not offered, and clients that ask
//! for it are told so and use TCP. Accounts sign in with HTTP Basic: the
//! passwords are stored hashed, which rules out Digest.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use super::hub::{ClientGuard, Msg, RestreamHub, StreamInfo, Unit};
use super::rtp;
use crate::engine::{CameraHandle, Engine};

/// Most RTSP connections open at once.
const MAX_CONNECTIONS: usize = 48;
/// A client has this long to send its first request, and between requests
/// until it plays.
const FIRST_REQUEST: Duration = Duration::from_secs(10);
const IDLE_BEFORE_PLAY: Duration = Duration::from_secs(60);
/// A client that cannot take a write for this long is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long DESCRIBE waits for a stream that is still starting.
const WAIT_FOR_STREAM: Duration = Duration::from_secs(6);
const MAX_REQUEST: usize = 16 * 1024;
const SR_EVERY: Duration = Duration::from_secs(5);

/// Bumped whenever the listener is replaced: older connections end.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static OPEN: AtomicUsize = AtomicUsize::new(0);

/// The state of the listener, for the settings page.
#[derive(Clone, Default)]
pub struct Status {
    pub listening: bool,
    pub error: Option<String>,
}

static STATUS: std::sync::Mutex<Status> = std::sync::Mutex::new(Status { listening: false, error: None });

pub fn status() -> Status {
    STATUS.lock().unwrap().clone()
}

fn set_status(listening: bool, error: Option<String>) {
    *STATUS.lock().unwrap() = Status { listening, error };
}

/// Open the port while sharing is on; follow changes of the settings.
pub async fn run(engine: Arc<Engine>) {
    let mut last_error = String::new();
    loop {
        let (enabled, bind, port) = settings(&engine);
        if !enabled {
            set_status(false, None);
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        match TcpListener::bind((bind.as_str(), port)).await {
            Ok(l) => {
                tracing::info!("RTSP sharing listening on {bind}:{port}");
                last_error.clear();
                set_status(true, None);
                let gen = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
                tokio::select! {
                    _ = accept_loop(engine.clone(), l, gen) => {}
                    _ = settings_changed(&engine, (enabled, bind.clone(), port)) => {}
                }
                // Connections of the old listener end on their next tick.
                GENERATION.fetch_add(1, Ordering::Relaxed);
                set_status(false, None);
            }
            Err(e) => {
                let msg = format!("cannot listen on {bind}:{port}: {e}");
                if msg != last_error {
                    tracing::warn!("RTSP sharing: {msg}");
                    last_error = msg.clone();
                }
                set_status(false, Some(msg));
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

fn settings(engine: &Engine) -> (bool, String, u16) {
    let c = engine.config.lock().unwrap();
    (c.restream.enabled && c.restream.port != 0, c.server.bind.clone(), c.restream.port)
}

async fn settings_changed(engine: &Engine, cur: (bool, String, u16)) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if settings(engine) != cur {
            return;
        }
    }
}

/// Connections from one address at a time.
const MAX_PER_IP: usize = 8;

/// Count a connection from `ip` in (`open`: false when the address already
/// has the most allowed) or out.
fn per_ip(ip: std::net::IpAddr, open: bool) -> bool {
    static COUNTS: std::sync::Mutex<Option<std::collections::HashMap<std::net::IpAddr, usize>>> = std::sync::Mutex::new(None);
    let mut g = COUNTS.lock().unwrap();
    let m = g.get_or_insert_with(Default::default);
    if open {
        let n = m.entry(ip).or_insert(0);
        if *n >= MAX_PER_IP {
            return false;
        }
        *n += 1;
    } else if let Some(n) = m.get_mut(&ip) {
        *n -= 1;
        if *n == 0 {
            m.remove(&ip);
        }
    }
    true
}

async fn accept_loop(engine: Arc<Engine>, l: TcpListener, gen: u64) {
    loop {
        match l.accept().await {
            Ok((sock, peer)) => {
                if OPEN.load(Ordering::Relaxed) >= MAX_CONNECTIONS || crate::auth::is_locked(peer.ip()) || !per_ip(peer.ip(), true) {
                    drop(sock);
                    continue;
                }
                OPEN.fetch_add(1, Ordering::Relaxed);
                let e = engine.clone();
                tokio::spawn(async move {
                    if let Err(err) = connection(e, sock, peer, gen).await {
                        tracing::debug!("RTSP {peer}: {err:#}");
                    }
                    per_ip(peer.ip(), false);
                    OPEN.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(e) => {
                tracing::warn!("RTSP accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

// ---- parsing ---------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub url: String,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq)]
pub enum Item {
    /// Interleaved data from the client (receiver reports): ignored.
    Skip,
    Req(Request),
}

/// Take the next complete message off the front of `buf`: a request, or an
/// interleaved `$` frame. None = more data needed.
pub fn parse_next(buf: &mut Vec<u8>) -> Result<Option<Item>> {
    // Some clients put an extra line break after a request.
    let lead = buf.iter().take_while(|b| **b == b'\r' || **b == b'\n').count();
    buf.drain(..lead);
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] == b'$' {
        if buf.len() < 4 {
            return Ok(None);
        }
        let n = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if buf.len() < 4 + n {
            return Ok(None);
        }
        buf.drain(..4 + n);
        return Ok(Some(Item::Skip));
    }
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        if buf.len() > MAX_REQUEST {
            return Err(anyhow!("request too long"));
        }
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let (Some(method), Some(url), Some(ver)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(anyhow!("not an RTSP request"));
    };
    if !ver.starts_with("RTSP/") {
        return Err(anyhow!("not an RTSP request"));
    }
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let body: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if body > MAX_REQUEST {
        return Err(anyhow!("request body too long"));
    }
    if buf.len() < end + 4 + body {
        return Ok(None);
    }
    let req = Request { method: method.to_ascii_uppercase(), url: url.to_string(), headers };
    buf.drain(..end + 4 + body);
    Ok(Some(Item::Req(req)))
}

/// Where a request URL points: camera slug, main or sub stream, and the
/// track named at the end (`trackID=N`).
#[derive(Debug, PartialEq)]
pub struct Path {
    pub slug: String,
    pub sub: bool,
    pub track: Option<usize>,
}

/// `rtsp://host:port/<slug>[/sub][/trackID=N]` (credentials in the URL, a
/// query and a trailing slash are tolerated).
pub fn parse_path(url: &str) -> Option<Path> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = rest.split_once('/').map(|(_, p)| p)?;
    let path = path.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_encoding::percent_decode_str(path).decode_utf8_lossy().into_owned();
    let mut segs: Vec<&str> = decoded.split('/').filter(|s| !s.is_empty()).collect();
    let track = match segs.last().and_then(|s| s.strip_prefix("trackID=")) {
        Some(n) => {
            let n = n.parse().ok()?;
            segs.pop();
            Some(n)
        }
        None => None,
    };
    let sub = segs.len() == 2 && segs[1] == "sub";
    if segs.is_empty() || segs.len() > 2 || (segs.len() == 2 && !sub) {
        return None;
    }
    Some(Path { slug: segs[0].to_ascii_lowercase(), sub, track })
}

/// The request URL without credentials and with a closing slash: the
/// Content-Base that track controls resolve against.
pub fn base_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("rtsp", url));
    let (auth, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
    let path = path.split(['?', '#']).next().unwrap_or("").trim_end_matches('/');
    if path.is_empty() { format!("{scheme}://{host}/") } else { format!("{scheme}://{host}/{path}/") }
}

/// `interleaved=a-b` (or just `a`) from a Transport header's TCP option.
/// None: no TCP option (UDP only).
pub fn parse_transport(h: &str) -> Option<(u8, u8)> {
    for opt in h.split(',') {
        let lower = opt.to_ascii_lowercase();
        if !lower.contains("rtp/avp/tcp") && !(lower.contains("interleaved=") && !lower.contains("client_port")) {
            continue;
        }
        let ch = lower.split(';').find_map(|p| p.trim().strip_prefix("interleaved=").map(str::to_string));
        return Some(match ch {
            Some(c) => {
                let mut it = c.split('-');
                let a: u8 = it.next()?.trim().parse().ok()?;
                let b: u8 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(a.checked_add(1)?);
                (a, b)
            }
            None => (u8::MAX, u8::MAX),
        });
    }
    None
}

fn response(code: u16, cseq: &str, extra: &[(&str, String)], body: Option<(&str, &str)>) -> Vec<u8> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        454 => "Session Not Found",
        455 => "Method Not Valid in This State",
        461 => "Unsupported Transport",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut s = format!("RTSP/1.0 {code} {reason}\r\nCSeq: {cseq}\r\nServer: AmberSight\r\n");
    for (k, v) in extra {
        s += &format!("{k}: {v}\r\n");
    }
    if let Some((ct, b)) = body {
        s += &format!("Content-Type: {ct}\r\nContent-Length: {}\r\n\r\n{b}", b.len());
    } else {
        s += "\r\n";
    }
    s.into_bytes()
}

// ---- one client ------------------------------------------------------------

struct Track {
    rtp: u8,
    rtcp: u8,
    ssrc: u32,
    seq: u16,
    ts_off: u32,
    packets: u32,
    octets: u32,
    last_sr: Option<Instant>,
}

struct Target {
    hub: Arc<RestreamHub>,
    info: Arc<StreamInfo>,
    name: String,
    slug: String,
}

struct Play {
    info: Arc<StreamInfo>,
    need_key: bool,
    _guard: ClientGuard,
}

struct Conn {
    engine: Arc<Engine>,
    peer: SocketAddr,
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    buf: Vec<u8>,
    user: Option<String>,
    /// The account's generation when this connection signed in.
    user_generation: u64,
    target: Option<Target>,
    tracks: [Option<Track>; 2],
    session: String,
    play: Option<Play>,
}

enum Next {
    Continue,
    Close,
    /// Start sending (PLAY answered).
    Play(broadcast::Receiver<Msg>, Vec<Arc<Unit>>),
}

async fn connection(engine: Arc<Engine>, sock: TcpStream, peer: SocketAddr, gen: u64) -> Result<()> {
    sock.set_nodelay(true).ok();
    // A client that vanished without closing is noticed in the end.
    let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(30)).with_interval(Duration::from_secs(10));
    socket2::SockRef::from(&sock).set_tcp_keepalive(&ka).ok();
    let (rd, wr) = sock.into_split();
    let c = Conn {
        engine,
        peer,
        rd,
        wr,
        buf: Vec::new(),
        user: None,
        user_generation: 0,
        target: None,
        tracks: [None, None],
        session: format!("{:08x}", rand::random::<u32>()),
        play: None,
    };
    c.serve(gen).await
}

/// The next message on the hub, or never while there is no receiver.
async fn recv(rx: &mut Option<broadcast::Receiver<Msg>>) -> std::result::Result<Msg, broadcast::error::RecvError> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

impl Conn {
    async fn serve(mut self, gen: u64) -> Result<()> {
        let started = Instant::now();
        let mut last_request = Instant::now();
        let mut got_request = false;
        let mut rx: Option<broadcast::Receiver<Msg>> = None;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut ticks = 0u32;
        loop {
            tokio::select! {
                n = self.rd.read_buf(&mut self.buf) => {
                    if n? == 0 {
                        return Ok(());
                    }
                    while let Some(item) = parse_next(&mut self.buf)? {
                        let Item::Req(req) = item else { continue };
                        // (Only whole requests count: a byte now and then
                        // must not keep an idle connection open.)
                        last_request = Instant::now();
                        got_request = true;
                        match self.handle(&req).await? {
                            Next::Continue => {}
                            Next::Close => return Ok(()),
                            Next::Play(r, backlog) => {
                                rx = Some(r);
                                for u in backlog {
                                    self.send_unit(&u).await?;
                                }
                            }
                        }
                    }
                }
                m = recv(&mut rx) => {
                    match m {
                        Ok(Msg::Unit(u)) => self.send_unit(&u).await?,
                        Ok(Msg::Reset) | Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Too slow: skip to the next keyframe.
                            if let Some(p) = self.play.as_mut() {
                                p.need_key = true;
                            }
                        }
                    }
                }
                _ = tick.tick() => {
                    if GENERATION.load(Ordering::Relaxed) != gen {
                        return Ok(());
                    }
                    ticks = ticks.wrapping_add(1);
                    if ticks % 2 == 0 && !self.still_allowed() {
                        return Ok(());
                    }
                    if self.play.is_none() {
                        let limit = if got_request { IDLE_BEFORE_PLAY } else { FIRST_REQUEST };
                        // Signed in (or open) within a minute, or gone.
                        let unsigned = self.user.is_none() && started.elapsed() > IDLE_BEFORE_PLAY;
                        if unsigned || last_request.elapsed() > limit || started.elapsed() > Duration::from_secs(3600) {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// The account still exists and may still see the camera (checked now and
    /// then, so a removed account or a changed camera list ends its streams).
    fn still_allowed(&self) -> bool {
        let Some(user) = self.user.as_deref() else { return true };
        let cfg = self.engine.config.lock().unwrap();
        if !crate::auth::still_allowed(&cfg, user, self.user_generation) {
            return false;
        }
        let Some(t) = self.target.as_ref() else { return true };
        user == "(open)" || crate::auth::camera_limits(&cfg, user).is_none_or(|allowed| allowed.contains(&t.slug))
    }

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        match tokio::time::timeout(WRITE_TIMEOUT, self.wr.write_all(data)).await {
            Ok(r) => Ok(r?),
            Err(_) => Err(anyhow!("client too slow")),
        }
    }

    /// The account behind a request: Ok(user), or the answer to send.
    async fn authenticate(&mut self, req: &Request, cseq: &str) -> std::result::Result<(String, Option<u64>), (Vec<u8>, bool)> {
        let accounts = crate::auth::accounts(&self.engine.config.lock().unwrap());
        if accounts.is_empty() {
            return Ok(("(open)".to_string(), None));
        }
        let ip = self.peer.ip();
        let challenge = |cseq: &str| response(401, cseq, &[("WWW-Authenticate", "Basic realm=\"AmberSight\"".into())], None);
        // (A device token is no guess: a lockout does not stop one.)
        let h = req.header("authorization");
        if crate::auth::is_locked(ip) && !h.is_some_and(|h| crate::auth::token_identity(h, &accounts).is_some()) {
            return Err((response(403, cseq, &[], None), true));
        }
        let Some(h) = h else { return Err((challenge(cseq), false)) };
        let cred = match crate::auth::check_authorization_queued(h, &accounts, ip).await {
            Ok(c) => c,
            // (Not counted: nothing was checked.)
            Err(crate::auth::NotChecked::Busy) => return Err((response(503, cseq, &[], None), false)),
            Err(crate::auth::NotChecked::Locked) => return Err((response(403, cseq, &[], None), true)),
        };
        match cred {
            crate::auth::Cred::Password(id, generation) => {
                crate::auth::record_success(ip);
                // A password sent directly by a program: noted once per network.
                crate::devices::note_direct(&self.engine, &id.user, ip);
                Ok((id.user, Some(generation)))
            }
            // RTSP here is not encrypted: anyone on the way can read the header.
            // That must never be a token with control rights.
            crate::auth::Cred::Token(id, _) if id.admin => {
                let _ = self.engine.db.insert_audit(&id.user, &ip.to_string(), "RTSP refused a device token with control rights", "", 403);
                Err((response(403, cseq, &[], Some(("text/plain", "RTSP is not encrypted: use a watch-only device token here"))), true))
            }
            crate::auth::Cred::Token(id, generation) => Ok((id.user, Some(generation))),
            // The right password, but it needs a code that RTSP cannot ask for.
            crate::auth::Cred::TwoStepNeeded => Err((response(401, cseq, &[("WWW-Authenticate", "Basic realm=\"AmberSight\"".into())], Some(("text/plain", crate::auth::USE_A_TOKEN))), true)),
            crate::auth::Cred::Wrong => {
                let locked = crate::auth::record_failure(ip);
                // The path only (no user info or query), and short: the URL is the client's.
                let path = parse_path(&req.url).map(|p| if p.sub { format!("/{}/sub", p.slug) } else { format!("/{}", p.slug) }).unwrap_or_else(|| "/?".into());
                let path: String = path.chars().take(100).collect();
                let _ = self.engine.db.insert_audit("?", &ip.to_string(), "sign-in failed (rtsp)", &path, 401);
                Err((challenge(cseq), locked))
            }
        }
    }

    /// The stream a path names, for the signed-in user: 404 for anything
    /// that is not shared or not theirs to see (it looks the same).
    fn resolve(&self, user: &str, p: &Path) -> std::result::Result<(Arc<CameraHandle>, Arc<RestreamHub>, String), u16> {
        let cfg = self.engine.config.lock().unwrap();
        if !cfg.restream.enabled {
            return Err(404);
        }
        if let Some(allowed) = crate::auth::camera_limits(&cfg, user) {
            if !allowed.contains(&p.slug) {
                return Err(404);
            }
        }
        drop(cfg);
        let cams: Vec<Arc<CameraHandle>> = self.engine.cameras.read().unwrap().iter().flatten().cloned().collect();
        let h = cams.into_iter().find(|h| crate::config::slug(&h.cfg.read().unwrap().name) == p.slug).ok_or(404u16)?;
        let (share, name, has_sub) = {
            let c = h.cfg.read().unwrap();
            // A mask or overlay added after sharing was turned on still
            // keeps the unmasked picture from going out.
            (c.share && super::http::blocker(&c).is_none(), c.name.clone(), c.sub_url.as_deref().is_some_and(|u| !u.is_empty()))
        };
        if !share || (p.sub && !has_sub) {
            return Err(404);
        }
        let hub = if p.sub { h.restream_sub.clone() } else { h.restream_main.clone() };
        Ok((h, hub, name))
    }

    async fn handle(&mut self, req: &Request) -> Result<Next> {
        let cseq = req.header("cseq").unwrap_or("0").to_string();
        let cseq = cseq.as_str();
        match req.method.as_str() {
            "OPTIONS" => {
                let r = response(200, cseq, &[("Public", "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER, SET_PARAMETER".into())], None);
                self.write(&r).await?;
                return Ok(Next::Continue);
            }
            "DESCRIBE" | "SETUP" | "PLAY" | "TEARDOWN" | "GET_PARAMETER" | "SET_PARAMETER" | "PAUSE" => {}
            _ => {
                self.write(&response(405, cseq, &[], None)).await?;
                return Ok(Next::Continue);
            }
        }
        // (A connection signed in earlier: not if its account changed since.)
        if self.user.is_some() && !self.still_allowed() {
            return Ok(Next::Close);
        }
        let user = match self.user.clone() {
            Some(u) => u,
            None => match self.authenticate(req, cseq).await {
                Ok((u, generation)) => {
                    self.user_generation = generation.unwrap_or_else(|| crate::auth::generation(&u));
                    self.user = Some(u.clone());
                    u
                }
                Err((r, close)) => {
                    self.write(&r).await?;
                    return Ok(if close { Next::Close } else { Next::Continue });
                }
            },
        };
        match req.method.as_str() {
            "TEARDOWN" => {
                self.write(&response(200, cseq, &[], None)).await?;
                Ok(Next::Close)
            }
            "GET_PARAMETER" | "SET_PARAMETER" => {
                self.write(&response(200, cseq, &[("Session", self.session.clone())], None)).await?;
                Ok(Next::Continue)
            }
            "PAUSE" => {
                self.write(&response(455, cseq, &[], None)).await?;
                Ok(Next::Continue)
            }
            "DESCRIBE" => self.describe(req, cseq, &user).await,
            "SETUP" => self.setup(req, cseq, &user).await,
            _ => self.play(req, cseq).await,
        }
    }

    async fn describe(&mut self, req: &Request, cseq: &str, user: &str) -> Result<Next> {
        let Some(p) = parse_path(&req.url) else {
            self.write(&response(404, cseq, &[], None)).await?;
            return Ok(Next::Continue);
        };
        let (h, hub, name) = match self.resolve(user, &p) {
            Ok(x) => x,
            Err(code) => {
                self.write(&response(code, cseq, &[], None)).await?;
                return Ok(Next::Continue);
            }
        };
        // A battery camera wakes for this; a stream still starting gets a moment.
        if h.running.read().unwrap().on_demand {
            self.engine.wake_camera(h.id);
        }
        let deadline = Instant::now() + WAIT_FOR_STREAM;
        let info = loop {
            if let Some(i) = hub.info() {
                break Some(i);
            }
            if Instant::now() >= deadline {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        let Some(info) = info else {
            self.write(&response(503, cseq, &[("Retry-After", "5".into())], None)).await?;
            return Ok(Next::Continue);
        };
        let sdp = rtp::sdp(&name, &info, info.generation);
        let r = response(200, cseq, &[("Content-Base", base_url(&req.url))], Some(("application/sdp", &sdp)));
        self.target = Some(Target { hub, info, name, slug: p.slug });
        self.write(&r).await?;
        Ok(Next::Continue)
    }

    async fn setup(&mut self, req: &Request, cseq: &str, user: &str) -> Result<Next> {
        if self.play.is_some() {
            self.write(&response(455, cseq, &[], None)).await?;
            return Ok(Next::Continue);
        }
        let Some(p) = parse_path(&req.url) else {
            self.write(&response(404, cseq, &[], None)).await?;
            return Ok(Next::Continue);
        };
        // (SETUP without a DESCRIBE first works too.)
        if self.target.as_ref().is_none_or(|t| t.slug != p.slug || t.hub.info().is_none()) {
            let (_, hub, name) = match self.resolve(user, &p) {
                Ok(x) => x,
                Err(code) => {
                    self.write(&response(code, cseq, &[], None)).await?;
                    return Ok(Next::Continue);
                }
            };
            let Some(info) = hub.info() else {
                self.write(&response(503, cseq, &[], None)).await?;
                return Ok(Next::Continue);
            };
            self.target = Some(Target { hub, info, name, slug: p.slug.clone() });
            self.tracks = [None, None];
        }
        let info = self.target.as_ref().unwrap().info.clone();
        let Some(n) = p.track.filter(|n| *n < 2) else {
            self.write(&response(400, cseq, &[], None)).await?;
            return Ok(Next::Continue);
        };
        if n == 1 && info.audio.is_none() {
            self.write(&response(404, cseq, &[], None)).await?;
            return Ok(Next::Continue);
        }
        let Some((a, b)) = req.header("transport").and_then(parse_transport) else {
            // UDP is not offered; clients fall back to TCP.
            self.write(&response(461, cseq, &[], None)).await?;
            return Ok(Next::Continue);
        };
        let other = self.tracks[1 - n].as_ref().map(|t| (t.rtp, t.rtcp));
        let (a, b) = if a == u8::MAX {
            // No channels named: next free pair.
            match other {
                Some((o, _)) => (o.wrapping_add(2), o.wrapping_add(3)),
                None => (0, 1),
            }
        } else {
            (a, b)
        };
        if a == b || other.is_some_and(|(x, y)| [x, y].contains(&a) || [x, y].contains(&b)) {
            self.write(&response(400, cseq, &[], None)).await?;
            return Ok(Next::Continue);
        }
        let t = Track { rtp: a, rtcp: b, ssrc: rand::random(), seq: rand::random(), ts_off: rand::random(), packets: 0, octets: 0, last_sr: None };
        let reply = format!("RTP/AVP/TCP;unicast;interleaved={a}-{b};ssrc={:08X}", t.ssrc);
        self.tracks[n] = Some(t);
        self.write(&response(200, cseq, &[("Transport", reply), ("Session", format!("{};timeout=60", self.session))], None)).await?;
        Ok(Next::Continue)
    }

    async fn play(&mut self, req: &Request, cseq: &str) -> Result<Next> {
        if self.play.is_some() {
            self.write(&response(200, cseq, &[("Session", self.session.clone())], None)).await?;
            return Ok(Next::Continue);
        }
        let Some(t) = self.target.as_ref().filter(|_| self.tracks.iter().any(|t| t.is_some())) else {
            self.write(&response(455, cseq, &[], None)).await?;
            return Ok(Next::Continue);
        };
        // The share may have been switched off since SETUP.
        let sub = t.hub.subscribe().filter(|s| s.info.generation == t.info.generation);
        let Some(sub) = sub else {
            self.write(&response(404, cseq, &[], None)).await?;
            return Ok(Next::Close);
        };
        let base = req.url.split_once("://").map(|(s, r)| format!("{s}://{}", r.rsplit_once('@').map(|(_, h)| h).unwrap_or(r))).unwrap_or_else(|| req.url.clone());
        let base = base.trim_end_matches('/');
        let rtp_info: Vec<String> = self
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.as_ref().map(|t| format!("url={base}/trackID={i};seq={}", t.seq)))
            .collect();
        let r = response(
            200,
            cseq,
            &[("Range", "npt=0.000-".into()), ("Session", self.session.clone()), ("RTP-Info", rtp_info.join(","))],
            None,
        );
        let guard = t.hub.client_guard();
        tracing::info!("RTSP: {} from {} is watching '{}'", self.user.as_deref().unwrap_or("?"), self.peer.ip(), t.name);
        self.play = Some(Play { info: sub.info.clone(), need_key: true, _guard: guard });
        self.write(&r).await?;
        Ok(Next::Play(sub.rx, sub.backlog))
    }

    /// Send one frame as RTP (preceded by a sender report when due).
    async fn send_unit(&mut self, u: &Unit) -> Result<()> {
        let Some(play) = self.play.as_mut() else { return Ok(()) };
        let idx = if u.video { 0 } else { 1 };
        if u.video {
            if play.need_key && !u.key {
                return Ok(());
            }
            if u.key {
                play.need_key = false;
            }
        }
        let info = play.info.clone();
        let Some(track) = self.tracks[idx].as_mut() else { return Ok(()) };
        let ts = (u.pts as u32).wrapping_add(track.ts_off);
        let mut out = Vec::with_capacity(u.data.len() + 256);
        if track.last_sr.is_none_or(|t| t.elapsed() >= SR_EVERY) {
            track.last_sr = Some(Instant::now());
            let sr = rtp::rtcp_sender_report(track.ssrc, u.wall, ts, track.packets, track.octets);
            rtp::interleave(&mut out, track.rtcp, &sr);
        }
        let (pt, payloads) = if u.video {
            let v = &info.video;
            let nals = rtp::split_nals(&u.data, v.len_size);
            let mut list: Vec<&[u8]> = Vec::with_capacity(nals.len() + 3);
            // Parameter sets before every keyframe, unless the frame has them.
            if u.key && !nals.first().is_some_and(|n| rtp::is_param_set(n, v.h265)) {
                list.extend(v.param_sets.iter().map(|p| p.as_slice()));
            }
            list.extend(nals);
            (rtp::VIDEO_PT, if v.h265 { rtp::packetize_h265(&list, rtp::MAX_PAYLOAD) } else { rtp::packetize_h264(&list, rtp::MAX_PAYLOAD) })
        } else {
            (rtp::AUDIO_PT, rtp::packetize_aac(&u.data).into_iter().collect())
        };
        let n = payloads.len();
        for (i, p) in payloads.iter().enumerate() {
            let pkt = rtp::rtp_packet(pt, i + 1 == n, track.seq, ts, track.ssrc, p);
            track.seq = track.seq.wrapping_add(1);
            track.packets = track.packets.wrapping_add(1);
            track.octets = track.octets.wrapping_add(p.len() as u32);
            rtp::interleave(&mut out, track.rtp, &pkt);
        }
        if out.is_empty() {
            return Ok(());
        }
        self.write(&out).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn parses_a_request() {
        let mut b = feed("DESCRIBE rtsp://h:8554/front_yard RTSP/1.0\r\nCSeq: 2\r\nAuthorization: Basic abc\r\nAccept: application/sdp\r\n\r\n");
        let Some(Item::Req(r)) = parse_next(&mut b).unwrap() else { panic!() };
        assert_eq!(r.method, "DESCRIBE");
        assert_eq!(r.url, "rtsp://h:8554/front_yard");
        assert_eq!(r.header("cseq"), Some("2"));
        assert_eq!(r.header("authorization"), Some("Basic abc"));
        assert!(b.is_empty());
    }

    #[test]
    fn waits_for_the_rest_and_handles_pipelining() {
        let mut b = feed("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n");
        assert_eq!(parse_next(&mut b).unwrap(), None);
        b.extend_from_slice(b"\r\nOPTIONS * RTSP/1.0\r\nCSeq: 2\r\n\r\n");
        assert!(matches!(parse_next(&mut b).unwrap(), Some(Item::Req(r)) if r.header("cseq") == Some("1")));
        assert!(matches!(parse_next(&mut b).unwrap(), Some(Item::Req(r)) if r.header("cseq") == Some("2")));
        assert_eq!(parse_next(&mut b).unwrap(), None);
    }

    #[test]
    fn skips_interleaved_frames_and_bodies() {
        let mut b = vec![b'$', 1, 0, 3, 9, 9, 9];
        b.extend_from_slice(b"SET_PARAMETER rtsp://h/x RTSP/1.0\r\nCSeq: 5\r\nContent-Length: 4\r\n\r\nab");
        assert_eq!(parse_next(&mut b).unwrap(), Some(Item::Skip));
        assert_eq!(parse_next(&mut b).unwrap(), None);
        b.extend_from_slice(b"cd");
        assert!(matches!(parse_next(&mut b).unwrap(), Some(Item::Req(_))));
        assert!(b.is_empty());
        // A partial interleaved header waits.
        let mut p = vec![b'$', 0];
        assert_eq!(parse_next(&mut p).unwrap(), None);
    }

    #[test]
    fn refuses_garbage_and_huge_requests() {
        assert!(parse_next(&mut feed("GET / HTTP/1.1\r\n\r\n")).is_err());
        assert!(parse_next(&mut vec![b'x'; MAX_REQUEST + 10]).is_err());
        assert!(parse_next(&mut feed("PLAY u RTSP/1.0\r\nContent-Length: 999999\r\n\r\n")).is_err());
    }

    #[test]
    fn reads_paths() {
        let p = parse_path("rtsp://host:8554/front_yard").unwrap();
        assert_eq!((p.slug.as_str(), p.sub, p.track), ("front_yard", false, None));
        let p = parse_path("rtsp://user:pa%40ss@host/Front_Yard/sub/").unwrap();
        assert_eq!((p.slug.as_str(), p.sub, p.track), ("front_yard", true, None));
        let p = parse_path("rtsp://host:1/cam/sub/trackID=1").unwrap();
        assert_eq!((p.slug.as_str(), p.sub, p.track), ("cam", true, Some(1)));
        let p = parse_path("rtsp://host/cam/trackID=0?x=1").unwrap();
        assert_eq!((p.slug.as_str(), p.sub, p.track), ("cam", false, Some(0)));
        assert!(parse_path("rtsp://host").is_none());
        assert!(parse_path("rtsp://host/").is_none());
        assert!(parse_path("rtsp://host/a/b").is_none());
        assert!(parse_path("rtsp://host/a/sub/c").is_none());
        assert!(parse_path("*").is_none());
    }

    #[test]
    fn content_base_has_no_credentials() {
        assert_eq!(base_url("rtsp://u:p@host:8554/cam/sub"), "rtsp://host:8554/cam/sub/");
        assert_eq!(base_url("rtsp://host/cam/"), "rtsp://host/cam/");
        assert_eq!(base_url("rtsp://host/cam?x=1"), "rtsp://host/cam/");
    }

    #[test]
    fn reads_transport() {
        assert_eq!(parse_transport("RTP/AVP/TCP;unicast;interleaved=0-1"), Some((0, 1)));
        assert_eq!(parse_transport("RTP/AVP/TCP;unicast;interleaved=4-5;mode=play"), Some((4, 5)));
        assert_eq!(parse_transport("RTP/AVP/TCP;unicast;interleaved=2"), Some((2, 3)));
        assert_eq!(parse_transport("RTP/AVP;unicast;client_port=5000-5001"), None);
        assert_eq!(parse_transport("RTP/AVP;unicast;client_port=5000-5001, RTP/AVP/TCP;unicast;interleaved=0-1"), Some((0, 1)));
        assert_eq!(parse_transport("RTP/AVP/TCP;unicast"), Some((u8::MAX, u8::MAX)));
    }

    #[test]
    fn responses_carry_cseq_and_body() {
        let r = String::from_utf8(response(200, "7", &[("Session", "abc".into())], Some(("application/sdp", "v=0\r\n")))).unwrap();
        assert!(r.starts_with("RTSP/1.0 200 OK\r\nCSeq: 7\r\n"));
        assert!(r.contains("Session: abc\r\n"));
        assert!(r.contains("Content-Length: 5\r\n\r\nv=0\r\n"));
        let r = String::from_utf8(response(401, "1", &[("WWW-Authenticate", "Basic realm=\"x\"".into())], None)).unwrap();
        assert!(r.starts_with("RTSP/1.0 401 Unauthorized\r\n"));
        assert!(r.ends_with("\r\n\r\n"));
    }

    #[tokio::test]
    async fn a_password_on_rtsp_is_noted_once_per_network_like_a_basic_one() {
        use base64::Engine as _;
        let mut cfg: crate::config::Config = toml::from_str("").unwrap();
        cfg.server.users.push(crate::config::UserConfig { disabled: false, username: "rt-ann".into(), password: crate::auth::hash_password("pw-rt-ann-1"), admin: false, cameras: Vec::new() });
        let e = Engine::for_test(cfg);
        // The account has a browser known already, so a new network is news.
        e.db.device_seen("rt-ann", "web", "Edge on Windows", 1, 50).unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
        let (sock, _) = l.accept().await.unwrap();
        let (rd, wr) = sock.into_split();
        let mut conn = Conn { engine: e.clone(), peer: "192.0.2.44:5000".parse().unwrap(), rd, wr, buf: Vec::new(), user: None, user_generation: 0, target: None, tracks: [None, None], session: String::new(), play: None };
        let h = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("rt-ann:pw-rt-ann-1"));
        let req = Request { method: "DESCRIBE".into(), url: "rtsp://h/x".into(), headers: vec![("authorization".into(), h)] };
        assert!(conn.authenticate(&req, "1").await.is_ok());
        let mut told = Vec::new();
        for _ in 0..100 {
            told = crate::account_http::sent_log().lock().unwrap().iter().filter(|(t, _)| t.contains("rt-ann")).cloned().collect::<Vec<_>>();
            if !told.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].1.contains("192.0.2.44") && told[0].1.contains("sent directly by a program") && !told[0].1.contains("sign that device out"), "{}", told[0].1);
        assert_eq!(e.db.known_device_count("rt-ann"), 2);
        drop(client);
    }
}
