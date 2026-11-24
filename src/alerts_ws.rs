//! Notifications over a WebSocket held open by an app (`GET /api/alerts/ws`):
//! a phone app that keeps its own connection to the server gets every message
//! the account's push devices get (the same audience, snoozes and filters:
//! `push::send` hands each message here too), with no push service between.
//! Documented in docs/api-guide.md, "Notifications over a WebSocket".
//!
//! Each message has an id that only grows. Messages of the last
//! `REPLAY_SECS` are kept in memory, so an app that was away (a dropped
//! connection, a phone that slept) asks for those after the last id it saw.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message as WsMsg, WebSocket};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::engine::Engine;
use crate::push::{Audience, Message, Rights};

/// How long messages are kept for an app that comes back.
pub const REPLAY_SECS: u64 = 600;
/// At most this many are kept.
const REPLAY_MAX: usize = 500;
/// The server pings this often (NAT and VPN paths stay open).
pub const PING_SECS: u64 = 30;
/// A connection that sent nothing (not even a pong) for this long is closed.
const SILENT_SECS: u64 = 75;
/// How often the session is checked (signed out or expired: closed with 4401).
const SESSION_CHECK: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 10 });
/// A frame not taken in this long closes the connection.
const SEND_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 10 });
/// Connections per account, and in all.
const PER_ACCOUNT: usize = 25;
const IN_ALL: usize = 1000;
/// Messages waiting for one slow connection; more closes it (the app comes
/// back and asks for what it missed).
const QUEUE: usize = 64;
/// The close code when the session ended: the app signs in again.
pub const CLOSE_SIGNED_OUT: u16 = 4401;

/// Who a kept message is for.
#[derive(Clone)]
enum Target {
    Aud(Audience),
    /// One account only (the test button).
    User(String),
}

struct Kept {
    id: u64,
    at: Instant,
    /// When it was first sent (ms since 1970): a message sent again says so.
    sent_ms: i64,
    target: Target,
    /// Accounts that had snoozed the camera when it was sent.
    snoozed: HashSet<String>,
    msg: Message,
}

struct Conn {
    user: String,
    /// None: it fell behind and is being closed (it still counts until its
    /// task ends: Leave removes it).
    tx: Option<tokio::sync::mpsc::Sender<String>>,
}

#[derive(Default)]
struct Hub {
    last_id: u64,
    /// Ids up to this one may be gone (dropped from the list, or sent before
    /// this server started).
    lost_upto: u64,
    kept: VecDeque<Arc<Kept>>,
    conns: HashMap<u64, Conn>,
    next_conn: u64,
    /// When the last connection closed (messages are kept for a while after).
    last_left: Option<Instant>,
}

fn hub() -> &'static Mutex<Hub> {
    static HUB: OnceLock<Mutex<Hub>> = OnceLock::new();
    HUB.get_or_init(|| {
        let now = now_ms();
        Mutex::new(Hub { last_id: now, lost_upto: now, ..Default::default() })
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Hub {
    /// A new id: above every earlier one, and (across restarts) about the
    /// time in ms, so an id from before a restart is below the new ones.
    fn next_id(&mut self) -> u64 {
        self.last_id = (self.last_id + 1).max(now_ms());
        self.last_id
    }

    fn prune(&mut self, now: Instant) {
        while let Some(k) = self.kept.front() {
            if self.kept.len() > REPLAY_MAX || now.duration_since(k.at) > Duration::from_secs(REPLAY_SECS) {
                self.lost_upto = self.lost_upto.max(k.id);
                self.kept.pop_front();
            } else {
                break;
            }
        }
    }
}

impl Hub {
    /// One is connected, or was within the replay time (it may come back
    /// and ask for what it missed).
    fn listening(&self) -> bool {
        !self.conns.is_empty() || self.last_left.is_some_and(|t| t.elapsed() < Duration::from_secs(REPLAY_SECS))
    }
}

/// Whether messages are worth making for sockets (see Hub::listening).
pub fn listening() -> bool {
    hub().lock().unwrap_or_else(|e| e.into_inner()).listening()
}

/// Taken for the whole of a delivery, from its id to the last queue: each
/// connection gets messages in id order (an app that keeps the highest id it
/// saw never skips one). Taken before the hub or the settings lock, never
/// while holding either.
static SENDING: Mutex<()> = Mutex::new(());

/// Whether `user` gets a message for `target`.
fn gets(cfg: &crate::config::Config, user: &str, target: &Target, snoozed: &HashSet<String>) -> bool {
    match target {
        Target::User(u) => u == user && account_live(cfg, user),
        Target::Aud(aud) => crate::push::wants(cfg, user, aud) && !snoozed.contains(user),
    }
}

/// A live account that is not the demo one (or no accounts at all).
fn account_live(cfg: &crate::config::Config, user: &str) -> bool {
    let acc = crate::auth::accounts(cfg);
    acc.is_empty() || acc.iter().any(|(u, p, _)| u == user && p != crate::auth::DEMO_HASH)
}

/// The text a connection of an account with `rights` gets.
fn text(m: &Message, rights: Rights, id: u64, sent_ms: i64) -> String {
    crate::push::socket_payload(m, rights, id, sent_ms)
}

/// Sends `m` to the connected apps of the accounts that get it for `aud`, and
/// keeps it for those that come back. Returns how many connections got it.
pub fn publish(engine: &Engine, aud: &Audience, m: &Message) -> usize {
    // (Read before any lock is taken.)
    let snoozed = match aud {
        Audience::Camera(name) => crate::snooze::snoozed_users(engine, name),
        Audience::Admins => Default::default(),
    };
    deliver(engine, Target::Aud(aud.clone()), snoozed, m)
}

/// Sends `m` to `user`'s connected apps only (the test button).
pub fn publish_to_user(engine: &Engine, user: &str, m: &Message) -> usize {
    deliver(engine, Target::User(user.to_string()), HashSet::new(), m)
}

/// How many apps `user` has connected (also those being closed).
#[cfg(test)]
pub fn connected(user: &str) -> usize {
    hub().lock().unwrap_or_else(|e| e.into_inner()).conns.values().filter(|c| c.user == user).count()
}

/// Of those, how many are taking messages (not being closed for falling behind).
pub fn taking(user: &str) -> usize {
    hub().lock().unwrap_or_else(|e| e.into_inner()).conns.values().filter(|c| c.user == user && c.tx.is_some()).count()
}

fn deliver(engine: &Engine, target: Target, snoozed: HashSet<String>, m: &Message) -> usize {
    let _order = SENDING.lock().unwrap_or_else(|e| e.into_inner());
    let sent_ms = now_ms() as i64;
    let mut msg = m.clone();
    // (A message sent again later keeps when it happened.)
    msg.at_ms = Some(msg.at_ms.unwrap_or(sent_ms));
    let (id, conns) = {
        let mut h = hub().lock().unwrap_or_else(|e| e.into_inner());
        let id = h.next_id();
        if !h.listening() {
            // Not kept (no app connected for a while): one that comes back
            // with an id from before this is told some may be gone. (Decided
            // under the same lock as join, so an app joining now either gets
            // it or is told so.)
            h.lost_upto = id;
            return 0;
        }
        let now = Instant::now();
        // (The test button's message is not sent again: an old test popping up after a reconnect only confuses.)
        if !matches!(target, Target::User(_)) {
            h.kept.push_back(Arc::new(Kept { id, at: now, sent_ms, target: target.clone(), snoozed: snoozed.clone(), msg: msg.clone() }));
        }
        h.prune(now);
        let conns: Vec<(u64, String, tokio::sync::mpsc::Sender<String>)> = h.conns.iter().filter_map(|(k, c)| Some((*k, c.user.clone(), c.tx.clone()?))).collect();
        (id, conns)
    };
    if conns.is_empty() {
        return 0;
    }
    // Who gets it, and with which buttons (worked out once per account).
    let mut per_user: HashMap<String, Option<String>> = HashMap::new();
    {
        let cfg = engine.config.lock().unwrap();
        let mut bodies: HashMap<Rights, String> = HashMap::new();
        for (_, user, _) in &conns {
            if per_user.contains_key(user) {
                continue;
            }
            let body = gets(&cfg, user, &target, &snoozed).then(|| {
                let r = crate::push::rights_of(&cfg, user);
                bodies.entry(r).or_insert_with(|| text(&msg, r, id, sent_ms)).clone()
            });
            per_user.insert(user.clone(), body);
        }
    }
    let mut sent = 0;
    let mut slow = Vec::new();
    for (k, user, tx) in conns {
        let Some(Some(body)) = per_user.get(&user) else { continue };
        match tx.try_send(body.clone()) {
            Ok(()) => sent += 1,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => slow.push(k),
            Err(_) => {}
        }
    }
    if !slow.is_empty() {
        // (Dropping its sender closes it; the app comes back and asks for what it missed.)
        let mut h = hub().lock().unwrap_or_else(|e| e.into_inner());
        for k in slow {
            if let Some(c) = h.conns.get_mut(&k).filter(|c| c.tx.is_some()) {
                c.tx = None;
                tracing::info!("notifications socket of {} fell behind; closed", c.user);
            }
        }
    }
    sent
}

#[derive(serde::Deserialize, Default)]
pub struct WsQ {
    /// The last message id the app saw: what came after it (within
    /// REPLAY_SECS) is sent first.
    since: Option<u64>,
}

/// `GET /api/alerts/ws[?since=<id>]`: see the module comment.
pub async fn alerts_ws(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<crate::web::Caller>,
    Query(q): Query<WsQ>,
    headers: HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    let user = caller.0.clone();
    let (token, demo) = {
        let cfg = engine.config.lock().unwrap();
        let acc = crate::auth::accounts(&cfg);
        (crate::auth::session_from_headers(&headers, &acc).map(|(t, _)| t.to_string()), crate::auth::is_demo(&acc, &user))
    };
    if demo {
        return (StatusCode::FORBIDDEN, crate::web::DEMO_REFUSED).into_response();
    }
    if hub().lock().unwrap_or_else(|e| e.into_inner()).full(&user) {
        return (StatusCode::TOO_MANY_REQUESTS, TOO_MANY).into_response();
    }
    let who = Some((engine.clone(), user.clone(), caller.1));
    ws.max_message_size(64 << 10).on_upgrade(move |socket| crate::web::while_account_allowed(who, run(engine, user, token, q.since, socket)))
}

const TOO_MANY: &str = "too many notification connections for this account";
/// The close code when the limit was reached while connecting (many at once).
pub const CLOSE_TOO_MANY: u16 = 4429;

impl Hub {
    fn full(&self, user: &str) -> bool {
        self.conns.len() >= IN_ALL || self.conns.values().filter(|c| c.user == user).count() >= PER_ACCOUNT
    }
}

/// Removes the connection from the hub when the session ends, however it ends.
struct Leave(u64);

impl Drop for Leave {
    fn drop(&mut self) {
        let mut h = hub().lock().unwrap_or_else(|e| e.into_inner());
        h.conns.remove(&self.0);
        if h.conns.is_empty() {
            h.last_left = Some(Instant::now());
        }
    }
}

/// What a connecting app is sent first: the messages after `since` that it gets.
struct Start {
    key: u64,
    rx: tokio::sync::mpsc::Receiver<String>,
    last_id: u64,
    complete: bool,
    replay: Vec<Arc<Kept>>,
}

/// Joins the hub and collects what to send again, under one lock: every
/// message is either in the replay or sent on the queue, never both or neither.
/// None: the limit was reached meanwhile (checked again here, where the
/// connection is added: many opened at once all passed the first check).
fn join(user: &str, since: Option<u64>) -> Option<Start> {
    let (tx, rx) = tokio::sync::mpsc::channel(QUEUE);
    let (key, last_id, complete, replay) = hub().lock().unwrap_or_else(|e| e.into_inner()).admit(user, since, tx)?;
    Some(Start { key, rx, last_id, complete, replay })
}

impl Hub {
    /// Adds a connection: (its key, the last id, whether nothing after
    /// `since` can be missing, the kept messages after `since`).
    fn admit(&mut self, user: &str, since: Option<u64>, tx: tokio::sync::mpsc::Sender<String>) -> Option<(u64, u64, bool, Vec<Arc<Kept>>)> {
        if self.full(user) {
            return None;
        }
        self.prune(Instant::now());
        if !self.listening() {
            // Messages since the last app left (more than the replay time
            // ago) were not kept: one with an id from before now may have
            // missed some. (Also messages that never got this far, e.g. an
            // alert not made at all because no device would take it.)
            let id = self.next_id();
            self.lost_upto = id;
        }
        self.next_conn += 1;
        let key = self.next_conn;
        self.conns.insert(key, Conn { user: user.to_string(), tx: Some(tx) });
        let last_id = self.last_id;
        let (complete, replay) = match since {
            None => (true, Vec::new()),
            // (An id above any sent: not from this server's list; all it has is sent.)
            Some(s) if s > last_id => (false, self.kept.iter().cloned().collect()),
            Some(s) => (s >= self.lost_upto, self.kept.iter().filter(|k| k.id > s).cloned().collect()),
        };
        Some((key, last_id, complete, replay))
    }
}

async fn run(engine: Arc<Engine>, user: String, token: Option<String>, since: Option<u64>, mut socket: WebSocket) {
    let Some(start) = join(&user, since) else {
        let _ = cannot_send(&mut socket, WsMsg::Close(Some(axum::extract::ws::CloseFrame { code: CLOSE_TOO_MANY, reason: TOO_MANY.into() }))).await;
        return;
    };
    let _leave = Leave(start.key);
    let mut rx = start.rx;
    let replay: Vec<String> = {
        let cfg = engine.config.lock().unwrap();
        let r = crate::push::rights_of(&cfg, &user);
        start.replay.iter().filter(|k| gets(&cfg, &user, &k.target, &k.snoozed)).map(|k| text(&k.msg, r, k.id, k.sent_ms)).collect()
    };
    let hello = serde_json::json!({
        "type": "hello",
        "v": 1,
        "last_id": start.last_id,
        "replayed": replay.len(),
        // False: messages after `since` may be gone (older than the replay
        // time, or sent before the server restarted): fetch /api/events.
        "complete": start.complete,
        "ping_s": PING_SECS,
    });
    if cannot_send(&mut socket, WsMsg::Text(hello.to_string())).await {
        return;
    }
    for t in replay {
        if cannot_send(&mut socket, WsMsg::Text(t)).await {
            return;
        }
    }
    let mut ping = tokio::time::interval(Duration::from_secs(PING_SECS));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut check = tokio::time::interval(SESSION_CHECK);
    check.tick().await;
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            m = rx.recv() => match m {
                Some(t) => {
                    if cannot_send(&mut socket, WsMsg::Text(t)).await {
                        return;
                    }
                }
                // (Dropped by the hub: it fell behind.)
                None => {
                    let _ = cannot_send(&mut socket, WsMsg::Close(None)).await;
                    return;
                }
            },
            _ = ping.tick() => {
                if cannot_send(&mut socket, WsMsg::Ping(Vec::new())).await {
                    return;
                }
            }
            _ = check.tick() => {
                if heard.elapsed() > Duration::from_secs(SILENT_SECS) {
                    let _ = cannot_send(&mut socket, WsMsg::Close(None)).await;
                    return;
                }
                if !session_alive(&engine, token.as_deref()) {
                    let _ = cannot_send(&mut socket, WsMsg::Close(Some(axum::extract::ws::CloseFrame { code: CLOSE_SIGNED_OUT, reason: "session ended; sign in again".into() }))).await;
                    return;
                }
            }
            got = socket.recv() => match got {
                Some(Ok(WsMsg::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(WsMsg::Text(t))) => {
                    heard = Instant::now();
                    // (The app may ping too.)
                    if t.contains("\"ping\"") && cannot_send(&mut socket, WsMsg::Text(r#"{"type":"pong"}"#.into())).await {
                        return;
                    }
                }
                Some(Ok(_)) => heard = Instant::now(),
            },
        }
    }
}

/// Sends one frame; true when it could not (an error, or a phone that stops
/// reading: a full connection would otherwise hold the task, its pings and
/// session checks, and its place, for as long as TCP keeps trying).
async fn cannot_send(socket: &mut WebSocket, m: WsMsg) -> bool {
    !matches!(tokio::time::timeout(SEND_TIMEOUT, socket.send(m)).await, Ok(Ok(())))
}

/// Whether the session the socket was opened with still is one (None: it was
/// opened another way, which the account check covers).
fn session_alive(engine: &Engine, token: Option<&str>) -> bool {
    let Some(t) = token else { return true };
    let acc = crate::auth::accounts(&engine.config.lock().unwrap());
    crate::auth::session_identity(t, &acc).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_only_grow_and_replay_is_exact() {
        let mut h = Hub { last_id: 5, lost_upto: 5, ..Default::default() };
        let a = h.next_id();
        let b = h.next_id();
        assert!(b > a && a > 5);
        // Kept messages are dropped by age and count; what was dropped is marked lost.
        let t0 = Instant::now();
        for _ in 0..(REPLAY_MAX + 3) {
            let id = h.next_id();
            h.kept.push_back(Arc::new(Kept { id, at: t0, sent_ms: 0, target: Target::User("u".into()), snoozed: HashSet::new(), msg: Message::default() }));
            h.prune(t0);
        }
        assert_eq!(h.kept.len(), REPLAY_MAX);
        assert_eq!(h.lost_upto, h.kept.front().unwrap().id - 1);
    }

    /// An app that comes back after more than the replay time with no app
    /// connected is told some may be gone, even when nothing was sent
    /// meanwhile (an alert no device would take is never made).
    #[test]
    fn coming_back_after_a_quiet_spell_is_never_complete() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut h = Hub { last_id: 100, lost_upto: 100, ..Default::default() };
        let (k, seen, complete, _) = h.admit("u", None, tx.clone()).unwrap();
        assert!(complete);
        // Connected now and again soon after: nothing can be missing.
        h.conns.remove(&k);
        h.last_left = Some(Instant::now());
        assert!(h.admit("u", Some(seen), tx.clone()).unwrap().2);
        // Gone longer than the replay time.
        h.conns.clear();
        h.last_left = Instant::now().checked_sub(Duration::from_secs(REPLAY_SECS + 5));
        if h.last_left.is_some() {
            assert!(!h.admit("u", Some(seen), tx.clone()).unwrap().2);
        }
        // And the limit holds where the connection is added.
        let mut h = Hub::default();
        for _ in 0..PER_ACCOUNT {
            assert!(h.admit("u", None, tx.clone()).is_some());
        }
        assert!(h.admit("u", None, tx.clone()).is_none());
        assert!(h.admit("other", None, tx).is_some());
    }

    /// A phone that stops reading is closed, and counts against the limit
    /// until it is.
    #[tokio::test]
    async fn a_client_that_never_reads_is_closed_and_counted_until_then() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut cfg: crate::config::Config = toml::from_str("").unwrap();
        cfg.server.users.push(crate::config::UserConfig { username: "ws-slow".into(), password: crate::auth::hash_password("pw-slow-1"), admin: true, cameras: vec![], disabled: false, role: String::new(), perms: Default::default() });
        let e = crate::engine::Engine::for_test(cfg);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = crate::web::build_router(e.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, svc).await.unwrap() });
        let mut req = format!("ws://{addr}/api/alerts/ws").into_client_request().unwrap();
        use base64::Engine as _;
        req.headers_mut().insert("authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("ws-slow:pw-slow-1")).parse().unwrap());
        // (Connected, and then never read from again.)
        let (_never_read, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        for _ in 0..50 {
            if connected("ws-slow") == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(connected("ws-slow"), 1);
        let big = "x".repeat(2500);
        let e2 = e.clone();
        tokio::task::spawn_blocking(move || {
            for _ in 0..20_000 {
                publish_to_user(&e2, "ws-slow", &Message { title: "flood".into(), body: big.clone(), kind: crate::push::MsgType::Test, ..Default::default() });
            }
        })
        .await
        .unwrap();
        // Fallen behind: closed (within the send time limit when its sending is stuck).
        let t0 = Instant::now();
        while connected("ws-slow") > 0 && t0.elapsed() < Duration::from_secs(15) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(connected("ws-slow"), 0, "closed after {:?}", t0.elapsed());
    }

    /// The whole way through a real server: who gets what, with which
    /// buttons, the replay after an id, and a session that ends.
    #[tokio::test]
    async fn a_connected_app_gets_what_its_push_devices_would() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as T};
        let mut cfg: crate::config::Config = toml::from_str("[[cameras]]\nname = \"Porch\"\nurl = \"rtsp://x/\"\n\n[[cameras]]\nname = \"Garage\"\nurl = \"rtsp://y/\"\n").unwrap();
        cfg.server.users.push(crate::config::UserConfig { username: "ws-boss".into(), password: crate::auth::hash_password("pw-boss-1"), admin: true, cameras: vec![], disabled: false, role: String::new(), perms: Default::default() });
        cfg.server.users.push(crate::config::UserConfig { username: "ws-vic".into(), password: crate::auth::hash_password("pw-vic-1"), admin: false, cameras: vec!["Porch".into()], disabled: false, role: String::new(), perms: Default::default() });
        let e = crate::engine::Engine::for_test(cfg);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = crate::web::build_router(e.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, svc).await.unwrap() });
        let vic_session = crate::auth::new_session(crate::auth::Identity { user: "ws-vic".into(), admin: false });
        let open = |query: &str, basic: Option<&str>, cookie: Option<String>| {
            let mut req = format!("ws://{addr}/api/alerts/ws{query}").into_client_request().unwrap();
            if let Some(b) = basic {
                use base64::Engine as _;
                req.headers_mut().insert("authorization", format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(b)).parse().unwrap());
            }
            if let Some(c) = cookie {
                req.headers_mut().insert("cookie", format!("{}={c}", crate::auth::SESSION_COOKIE).parse().unwrap());
            }
            async move { tokio_tungstenite::connect_async(req).await.map(|(s, _)| s) }
        };
        async fn next_json<S: futures::Stream<Item = Result<T, tokio_tungstenite::tungstenite::Error>> + Unpin>(s: &mut S) -> serde_json::Value {
            loop {
                match tokio::time::timeout(Duration::from_secs(5), s.next()).await.expect("a message in time").expect("open").unwrap() {
                    T::Text(t) => return serde_json::from_str(&t).unwrap(),
                    T::Ping(_) | T::Pong(_) => continue,
                    other => panic!("{other:?}"),
                }
            }
        }
        async fn nothing<S: futures::Stream<Item = Result<T, tokio_tungstenite::tungstenite::Error>> + Unpin>(s: &mut S) {
            assert!(tokio::time::timeout(Duration::from_millis(300), s.next()).await.is_err(), "nothing more");
        }
        // Without signing in: refused before the upgrade.
        assert!(open("", None, None).await.is_err());
        let mut boss = open("", Some("ws-boss:pw-boss-1"), None).await.unwrap();
        let hello = next_json(&mut boss).await;
        assert_eq!((hello["type"].as_str(), hello["complete"].as_bool(), hello["replayed"].as_u64()), (Some("hello"), Some(true), Some(0)));
        let mut vic = open("", None, Some(vic_session.clone())).await.unwrap();
        assert_eq!(next_json(&mut vic).await["type"], "hello");
        let event = |cam: &str, id: usize| Message {
            title: format!("{cam}: person"),
            body: "Seen".into(),
            kind: crate::push::MsgType::Event,
            camera: Some(cam.into()),
            camera_id: Some(id),
            event_id: Some(format!("ev-{cam}")),
            labels: vec!["person".into()],
            at_ms: Some(1_700_000_000_000),
            ..Default::default()
        };
        // Garage: vic may not see it.
        let e2 = e.clone();
        let n = tokio::task::spawn_blocking(move || publish(&e2, &Audience::Camera("Garage".into()), &event("Garage", 1))).await.unwrap();
        assert_eq!(n, 1);
        let g = next_json(&mut boss).await;
        assert_eq!((g["type"].as_str(), g["camera"].as_str(), g["event_id"].as_str(), g["at_ms"].as_i64()), (Some("event"), Some("Garage"), Some("ev-Garage"), Some(1_700_000_000_000)));
        let garage_id = g["id"].as_u64().unwrap();
        nothing(&mut vic).await;
        // Porch: both, each with the buttons its account may use.
        let e2 = e.clone();
        assert_eq!(tokio::task::spawn_blocking(move || publish(&e2, &Audience::Camera("Porch".into()), &event("Porch", 0))).await.unwrap(), 2);
        let pb = next_json(&mut boss).await;
        let pv = next_json(&mut vic).await;
        assert_eq!(pb["id"], pv["id"]);
        assert!(pb["id"].as_u64().unwrap() > garage_id);
        let acts = |v: &serde_json::Value| v["actions"].as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(acts(&pb), ["snooze", "ok", "pause"]);
        assert_eq!(acts(&pv), ["snooze"]);
        // A system problem: admins only. The test button: that account only.
        let e2 = e.clone();
        tokio::task::spawn_blocking(move || {
            publish(&e2, &Audience::Admins, &Message { title: "Disk nearly full".into(), kind: crate::push::MsgType::Problem, urgent: true, ..Default::default() });
            publish_to_user(&e2, "ws-vic", &Message { title: "test".into(), kind: crate::push::MsgType::Test, ..Default::default() });
        })
        .await
        .unwrap();
        let sys = next_json(&mut boss).await;
        assert_eq!((sys["type"].as_str(), sys["urgent"].as_bool()), (Some("problem"), Some(true)));
        assert!(sys["at_ms"].as_i64().unwrap() > 1_700_000_000_000, "when it was sent");
        assert_eq!(next_json(&mut vic).await["type"], "test");
        nothing(&mut boss).await;
        nothing(&mut vic).await;
        // Back after the Garage message: the Porch one and the problem again, with the same ids.
        let mut again = open(&format!("?since={garage_id}"), Some("ws-boss:pw-boss-1"), None).await.unwrap();
        let h = next_json(&mut again).await;
        assert_eq!((h["complete"].as_bool(), h["replayed"].as_u64()), (Some(true), Some(2)));
        let r1 = next_json(&mut again).await;
        assert_eq!((&r1["id"], &r1["sent_ms"]), (&pb["id"], &pb["sent_ms"]), "sent again as it was first sent");
        assert_eq!(next_json(&mut again).await["id"], sys["id"]);
        nothing(&mut again).await;
        // Sent from many threads at once: each connection gets them in id order.
        let e2 = e.clone();
        tokio::task::spawn_blocking(move || {
            std::thread::scope(|sc| {
                for _ in 0..4 {
                    sc.spawn(|| {
                        for _ in 0..10 {
                            publish(&e2, &Audience::Admins, &Message { title: "n".into(), kind: crate::push::MsgType::Notice, ..Default::default() });
                        }
                    });
                }
            })
        })
        .await
        .unwrap();
        let mut last = sys["id"].as_u64().unwrap();
        for _ in 0..40 {
            let id = next_json(&mut again).await["id"].as_u64().unwrap();
            assert!(id > last, "{id} after {last}");
            last = id;
        }
        // Many opened at once: never more than the limit.
        let many = futures::future::join_all((0..PER_ACCOUNT + 5).map(|_| open("", Some("ws-boss:pw-boss-1"), None))).await;
        let mut open_now = Vec::new();
        for s in many.into_iter().flatten() {
            let mut s = s;
            if let Ok(Some(Ok(T::Text(_)))) = tokio::time::timeout(Duration::from_secs(5), s.next()).await {
                open_now.push(s);
            }
        }
        assert!(connected("ws-boss") <= PER_ACCOUNT, "{}", connected("ws-boss"));
        assert!(open_now.len() >= PER_ACCOUNT - 2, "{} open", open_now.len());
        drop(open_now);
        for _ in 0..50 {
            if connected("ws-boss") <= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(connected("ws-boss") <= 2, "closed ones leave: {}", connected("ws-boss"));
        // An id from before this server started (or never sent): told some may be gone.
        let mut old = open("?since=5", Some("ws-boss:pw-boss-1"), None).await.unwrap();
        assert_eq!(next_json(&mut old).await["complete"], false);
        // The app may ping too.
        boss.send(T::Text(r#"{"type":"ping"}"#.into())).await.unwrap();
        // (After the 40 notices it has queued.)
        let mut kinds = Vec::new();
        loop {
            let t = next_json(&mut boss).await["type"].as_str().unwrap().to_string();
            if t == "pong" {
                break;
            }
            kinds.push(t);
        }
        assert_eq!(kinds.len(), 40);
        // The session signed out: closed with 4401, so the app signs in again.
        crate::auth::end_session(&vic_session);
        let closed = loop {
            match tokio::time::timeout(Duration::from_secs(5), vic.next()).await.expect("closed in time") {
                Some(Ok(T::Close(f))) => break f.map(|f| u16::from(f.code)),
                Some(Ok(T::Ping(_))) => continue,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(closed, Some(CLOSE_SIGNED_OUT));
        // And the signed-out session cannot open one.
        assert!(open("", None, Some(vic_session)).await.is_err());
        assert_eq!(connected("ws-vic"), 0);
    }
}
