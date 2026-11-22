//! One HomeKit accessory server: its TCP port, connections, the encrypted
//! sessions after pair-verify, the HAP resources and the event
//! notifications. An `Instance` is one advertised accessory with its own
//! state and port: today the bridge with every camera shown in Apple Home; a
//! camera published on its own later (as Home prefers for cameras) would be
//! another instance with a one-camera database.
//!
//! Connections that have not pair-verified are kept apart: few at once (and
//! fewer per address), each gets a short time to make progress in pairing
//! (answers like 470 do not count), and when they are all taken the oldest
//! gives way. Verified sessions have their own room, so unverified
//! connections can never push them out. One controller has only so many
//! sessions (its oldest gives way to its new one), and when the room is
//! full, a new session pushes out the least recently used one of the
//! controller with the most: never an admin's for a non-admin, and for an
//! admin a non-admin's first. Sessions in use (with event subscriptions, or
//! sent events lately, like a home hub's) go last. One paired controller
//! cannot keep the others, or the admin, out.
//!
//! Events: each session gets at most a few event messages at once, then one
//! per interval; changes meanwhile wait merged (the latest value of each
//! characteristic, one entry each, capped), so a controller changing values
//! as fast as it can, or one reading slowly, makes no backlog.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::accessories::{iid, Database, Role};
use super::crypto::Session;
use super::http::{self, Parse, Request};
use super::pairing::{self, After, ConnPairing, Slot, Timing, Window};
use super::state::State;
use super::stream::Streams;
use super::Host;

/// Limits and timeouts (tests make them small).
#[derive(Clone, Debug)]
pub struct Limits {
    /// Connections that have not pair-verified, in all and from one address.
    pub max_unverified: usize,
    pub max_unverified_per_ip: usize,
    /// Verified sessions, in all and of one controller.
    pub max_verified: usize,
    pub max_verified_per_controller: usize,
    /// An unverified connection is closed after this long without progress
    /// in pairing (longer while it holds the pair-setup: `timing.setup_timeout`).
    pub unverified_idle: Duration,
    /// A verified session with no request for this long is closed (the
    /// controller reconnects when it needs to; TCP keepalive finds dead ones).
    pub verified_idle: Duration,
    /// A request has this long to arrive whole once it started.
    pub partial: Duration,
    /// A controller that cannot take a write for this long is dropped.
    pub write: Duration,
    /// Event messages one session gets: this many at once, then one per
    /// `event_every` (changes meanwhile are merged, the latest value wins).
    pub event_burst: u32,
    pub event_every: Duration,
    pub timing: Timing,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_unverified: 8,
            max_unverified_per_ip: 3,
            max_verified: 16,
            // (One Apple account's devices share one controller id: the
            // owner's phone, tablet, Mac and home hubs count together.)
            max_verified_per_controller: 8,
            unverified_idle: Duration::from_secs(10),
            verified_idle: Duration::from_secs(12 * 3600),
            partial: Duration::from_secs(10),
            write: Duration::from_secs(10),
            event_burst: 10,
            event_every: Duration::from_millis(100),
            timing: Timing::default(),
        }
    }
}

/// Most characteristics one session may subscribe to, and one request may name.
const MAX_SUBSCRIPTIONS: usize = 256;
const MAX_IDS: usize = 128;
/// Characteristics with a change waiting for one session, at most (one
/// entry each: a newer value replaces the one waiting).
const EVENT_BACKLOG: usize = MAX_SUBSCRIPTIONS;

/// The changes waiting to be sent to one session: the latest value of each
/// characteristic, so a controller that changes values faster than events
/// may be sent (or a slow reader) makes them merge, never pile up.
#[derive(Default)]
struct Events {
    waiting: Mutex<BTreeMap<(u64, u64), Value>>,
    /// Woken when something is waiting.
    ready: Notify,
}

impl Events {
    /// Wait to send these (a newer value replaces one waiting). Whether any
    /// was taken (none past the backlog's cap).
    fn put(&self, changes: impl IntoIterator<Item = ((u64, u64), Value)>) -> bool {
        let mut w = self.waiting.lock().unwrap();
        let mut any = false;
        for (k, v) in changes {
            if w.len() >= EVENT_BACKLOG && !w.contains_key(&k) {
                continue;
            }
            w.insert(k, v);
            any = true;
        }
        drop(w);
        if any {
            self.ready.notify_one();
        }
        any
    }
    fn is_waiting(&self) -> bool {
        !self.waiting.lock().unwrap().is_empty()
    }
    fn take(&self) -> BTreeMap<(u64, u64), Value> {
        std::mem::take(&mut *self.waiting.lock().unwrap())
    }
    /// The session no longer wants this one (it unsubscribed).
    fn forget(&self, k: &(u64, u64)) {
        self.waiting.lock().unwrap().remove(k);
    }
}

/// How many event messages one session may be sent: `burst` at once, then
/// one more every `every`.
struct Bucket {
    tokens: u32,
    burst: u32,
    every: Duration,
    /// When the next one comes back (while not full).
    refill_at: Instant,
}

impl Bucket {
    fn new(burst: u32, every: Duration, now: Instant) -> Bucket {
        let burst = burst.max(1);
        Bucket { tokens: burst, burst, every, refill_at: now + every }
    }
    fn refill(&mut self, now: Instant) {
        while self.tokens < self.burst && self.refill_at <= now {
            self.tokens += 1;
            self.refill_at += self.every;
        }
        if self.tokens >= self.burst {
            self.refill_at = now + self.every;
        }
    }
    /// When a message may be sent (now, or when one comes back).
    fn ready_at(&mut self, now: Instant) -> Instant {
        self.refill(now);
        if self.tokens > 0 {
            now
        } else {
            self.refill_at
        }
    }
    fn take(&mut self, now: Instant) {
        self.refill(now);
        self.tokens = self.tokens.saturating_sub(1);
    }
}

/// HAP status codes in JSON answers.
pub mod status {
    pub const OK: i64 = 0;
    pub const INSUFFICIENT_PRIVILEGES: i64 = -70401;
    pub const UNABLE_TO_COMMUNICATE: i64 = -70402;
    pub const BUSY: i64 = -70403;
    pub const READ_ONLY: i64 = -70404;
    pub const WRITE_ONLY: i64 = -70405;
    pub const NO_NOTIFICATION: i64 = -70406;
    pub const OUT_OF_RESOURCES: i64 = -70407;
    pub const NOT_FOUND: i64 = -70409;
    pub const INVALID_VALUE: i64 = -70410;
}

/// One verified session, for events and for ending it.
struct SessionEntry {
    controller: String,
    subs: HashSet<(u64, u64)>,
    events: Arc<Events>,
    end: CancellationToken,
    /// Its last request (or when it began).
    used: Instant,
    /// When an event was last sent to it.
    evented: Option<Instant>,
}

/// Events sent this recently keep a session counted as in use.
const RECENT_EVENTS: Duration = Duration::from_secs(3600);

impl SessionEntry {
    fn new(controller: String, events: Arc<Events>, end: CancellationToken) -> SessionEntry {
        SessionEntry { controller, subs: HashSet::new(), events, end, used: Instant::now(), evented: None }
    }

    /// In use: it has event subscriptions, or got events lately (a home
    /// hub may make no requests for hours and still be needed).
    fn in_use(&self, now: Instant) -> bool {
        !self.subs.is_empty() || self.evented.is_some_and(|t| now.duration_since(t) < RECENT_EVENTS)
    }
}

/// Which of `pool` gives way: a session not in use when there is one, of
/// the controller with the most such sessions, its least recently used.
fn give_way<'a>(pool: impl Iterator<Item = (&'a u64, &'a SessionEntry)>, now: Instant) -> Option<u64> {
    let pool: Vec<_> = pool.collect();
    let idle: Vec<_> = pool.iter().copied().filter(|(_, e)| !e.in_use(now)).collect();
    let pool = if idle.is_empty() { pool } else { idle };
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (_, e) in &pool {
        *counts.entry(e.controller.as_str()).or_default() += 1;
    }
    let most = counts.values().copied().max()?;
    pool.into_iter().filter(|(_, e)| counts[e.controller.as_str()] == most).min_by_key(|(_, e)| e.used).map(|(k, _)| *k)
}

/// One connection that has not pair-verified yet.
struct Unverified {
    /// Its accessory server and its id there.
    inst: u64,
    id: u64,
    ip: IpAddr,
    opened: Instant,
    end: CancellationToken,
    /// That server's pair-setup (a connection in pair-setup is never pushed out).
    slot: Arc<Mutex<Slot>>,
}

impl Unverified {
    fn in_setup(&self) -> bool {
        self.slot.lock().unwrap().holder() == Some(self.id)
    }
}

/// What an accessory server publishes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The bridge, with every camera shown in Apple Home.
    Bridge,
    /// One camera on its own (by its name).
    Camera(String),
}

/// Unverified connections of all accessory servers together (each
/// accessory also has its own limits; verified sessions only the
/// accessory's own, so many cameras with many Apple devices never run out).
const MAX_UNVERIFIED_ALL: usize = 24;

/// The unverified connections of every accessory server of one supervisor:
/// one address's are counted across them, and when they are all taken the
/// oldest anywhere gives way.
#[derive(Default)]
pub struct Gate {
    list: Mutex<Vec<Unverified>>,
}

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

pub struct Instance {
    pub kind: Kind,
    /// The name the accessory is advertised with.
    pub name: String,
    pub port: u16,
    pub limits: Limits,
    pub state: Arc<Mutex<State>>,
    pub db: Mutex<Database>,
    setup: Arc<Mutex<Slot>>,
    /// The pairing window, shared by every accessory server.
    pub window: Arc<Mutex<Window>>,
    sessions: Mutex<HashMap<u64, SessionEntry>>,
    gate: Arc<Gate>,
    uid: u64,
    /// Woken when the mDNS record must change (pairing state, c#).
    pub advertise: Notify,
    next_conn: AtomicU64,
    /// Live video sessions.
    pub streams: Streams,
}

impl Instance {
    /// One accessory server on its own (tests).
    #[cfg(test)]
    pub fn new(kind: Kind, name: String, port: u16, state: Arc<Mutex<State>>, limits: Limits) -> Instance {
        Instance::new_in(kind, name, port, state, limits, Arc::new(Gate::default()), Arc::new(Mutex::new(Window::default())))
    }

    /// One of several accessory servers sharing the unverified-connection
    /// limits and the pairing window.
    pub fn new_in(kind: Kind, name: String, port: u16, state: Arc<Mutex<State>>, limits: Limits, gate: Arc<Gate>, window: Arc<Mutex<Window>>) -> Instance {
        let db = match kind {
            Kind::Bridge => Database::bridge(&name, &state.lock().unwrap().device_id, &[]),
            Kind::Camera(_) => Database { accessories: Vec::new() },
        };
        Instance {
            kind,
            name,
            port,
            limits,
            state,
            db: Mutex::new(db),
            setup: Arc::new(Mutex::new(Slot::default())),
            window,
            sessions: Mutex::new(HashMap::new()),
            gate,
            uid: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            advertise: Notify::new(),
            next_conn: AtomicU64::new(1),
            streams: Streams::default(),
        }
    }

    /// The accessory category (mDNS `ci`, setup URI): bridge 2, camera 17.
    pub fn category(&self) -> u8 {
        match self.kind {
            Kind::Bridge => 2,
            Kind::Camera(_) => 17,
        }
    }

    /// Forget this accessory's controllers (same identity and code: it can
    /// be added again).
    pub fn unpair(&self) -> Result<()> {
        self.setup.lock().unwrap().clear();
        {
            let mut st = self.state.lock().unwrap();
            st.unpair();
            st.save()?;
        }
        self.end_all();
        self.advertise.notify_one();
        Ok(())
    }

    /// The pair-setup in progress: the address it comes from, and how long ago it began.
    pub fn pairing_now(&self) -> Option<(IpAddr, Duration)> {
        self.setup.lock().unwrap().exchange().map(|(ip, at)| (ip, at.elapsed()))
    }

    /// End the pair-setup in progress (only one from `ip`, when given) and
    /// close its connection. Its address, when one was ended.
    pub fn end_setup(&self, ip: Option<IpAddr>) -> Option<IpAddr> {
        let (from, conn) = {
            let mut slot = self.setup.lock().unwrap();
            let from = slot.exchange()?.0;
            (from, slot.stop(ip)?)
        };
        for e in self.gate.list.lock().unwrap().iter().filter(|e| e.inst == self.uid && e.id == conn) {
            e.end.cancel();
        }
        Some(from)
    }

    /// Whether Apple devices may pair it now: unpaired, and the pairing
    /// window is open for it.
    pub fn pairable(&self) -> bool {
        self.state.lock().unwrap().pairable() && self.window.lock().unwrap().open_for(&self.kind)
    }

    /// Send changed values to the sessions subscribed to them (not to
    /// `except`, the connection that made the change).
    pub fn notify(&self, changes: &[(u64, u64, Value)], except: Option<u64>) {
        if changes.is_empty() {
            return;
        }
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        for (conn, s) in sessions.iter_mut() {
            if Some(*conn) == except {
                continue;
            }
            let mine = changes.iter().filter(|(a, i, _)| s.subs.contains(&(*a, *i))).map(|(a, i, v)| ((*a, *i), v.clone()));
            // (Sent by its connection, at most so often: a controller that
            // does not keep up, or one making changes as fast as it can, gets
            // them merged, the latest value of each.)
            if s.events.put(mine) {
                s.evented = Some(now);
            }
        }
    }

    /// End the sessions of one controller (its pairing was removed).
    pub fn end_controller(&self, id: &str) {
        for s in self.sessions.lock().unwrap().values().filter(|s| s.controller == id) {
            s.end.cancel();
        }
    }

    /// End every session.
    pub fn end_all(&self) {
        for s in self.sessions.lock().unwrap().values() {
            s.end.cancel();
        }
    }

    /// Paired controllers' open sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// Forget every pairing and become a new accessory (new code too, or
    /// `code` when given: the one all standalone cameras share).
    pub fn reset(&self, code: Option<&str>) -> Result<()> {
        self.setup.lock().unwrap().clear();
        self.streams.end_all();
        {
            let mut st = self.state.lock().unwrap();
            st.reset();
            if let Some(c) = code {
                st.setup_code = c.to_string();
            }
            st.save()?;
            if self.kind == Kind::Bridge {
                let fresh = Database::bridge(&self.name, &st.device_id, &self.db.lock().unwrap().cameras());
                let mut db = self.db.lock().unwrap();
                let old = std::mem::replace(&mut *db, fresh);
                db.keep_values_from(&old);
                st.db_hash = db.fingerprint();
                st.save()?;
            }
        }
        self.end_all();
        self.advertise.notify_one();
        Ok(())
    }

    /// Let a new connection in as unverified, making room by closing the
    /// oldest unverified one (never one in pair-setup) when needed: this
    /// accessory's when it has its most, else the oldest of all.
    fn admit(&self, id: u64, ip: IpAddr, end: CancellationToken) -> bool {
        let mut u = self.gate.list.lock().unwrap();
        if u.iter().filter(|e| e.ip == ip).count() >= self.limits.max_unverified_per_ip {
            return false;
        }
        let mine = u.iter().filter(|e| e.inst == self.uid).count() >= self.limits.max_unverified;
        if mine || u.len() >= MAX_UNVERIFIED_ALL {
            let Some(i) = u.iter().enumerate().filter(|(_, e)| (!mine || e.inst == self.uid) && !e.in_setup()).min_by_key(|(_, e)| e.opened).map(|(i, _)| i) else {
                return false;
            };
            u.remove(i).end.cancel();
        }
        u.push(Unverified { inst: self.uid, id, ip, opened: Instant::now(), end, slot: self.setup.clone() });
        true
    }

    /// The connection is no longer unverified (verified, or gone).
    fn leave_unverified(&self, id: u64) {
        self.gate.list.lock().unwrap().retain(|e| !(e.inst == self.uid && e.id == id));
    }

    /// A connection pair-verified: it leaves the unverified ones and gets
    /// its session (false: no room). Room is made when needed by closing one
    /// session (see `give_way`): the controller's own when it has its most,
    /// else (all taken) a non-admin's, or for an admin, an admin's when no
    /// non-admin has one. Admin rights are read from the pairings as they
    /// are now, so adding, changing or removing them counts for sessions
    /// already open.
    fn promote(&self, id: u64, entry: SessionEntry) -> bool {
        self.leave_unverified(id);
        let admins: HashSet<String> = self.state.lock().unwrap().pairings.iter().filter(|p| p.admin).map(|p| p.id.clone()).collect();
        let mut s = self.sessions.lock().unwrap();
        let now = Instant::now();
        let mine = s.values().filter(|e| e.controller == entry.controller).count();
        let victim = if mine >= self.limits.max_verified_per_controller {
            give_way(s.iter().filter(|(_, e)| e.controller == entry.controller), now)
        } else if s.len() >= self.limits.max_verified {
            let admin = |e: &SessionEntry| admins.contains(&e.controller);
            if s.values().any(|e| !admin(e)) {
                give_way(s.iter().filter(|(_, e)| !admin(e)), now)
            } else if admins.contains(&entry.controller) {
                give_way(s.iter(), now)
            } else {
                return false;
            }
        } else {
            None
        };
        if let Some(e) = victim.and_then(|k| s.remove(&k)) {
            tracing::debug!("HomeKit: a session gave way to a newer one");
            e.end.cancel();
        }
        s.insert(id, entry);
        true
    }

    /// A verified session made a request.
    fn touch(&self, id: u64) {
        if let Some(e) = self.sessions.lock().unwrap().get_mut(&id) {
            e.used = Instant::now();
        }
    }

    fn holds_setup(&self, id: u64) -> bool {
        self.setup.lock().unwrap().holder() == Some(id)
    }
}

/// Accept connections until `stop`.
pub async fn accept_loop(inst: Arc<Instance>, host: Arc<dyn Host>, l: TcpListener, stop: CancellationToken) {
    loop {
        let accepted = tokio::select! {
            _ = stop.cancelled() => return,
            a = l.accept() => a,
        };
        match accepted {
            Ok((sock, peer)) => {
                let id = inst.next_conn.fetch_add(1, Ordering::Relaxed);
                let end = stop.child_token();
                // (An IPv4 peer on the dual-stack socket counts as itself.)
                let ip = match peer.ip() {
                    IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
                    v4 => v4,
                };
                if !inst.admit(id, ip, end.clone()) {
                    drop(sock);
                    continue;
                }
                let (i, h) = (inst.clone(), host.clone());
                tokio::spawn(async move {
                    // (An IPv6 link-local peer's interface, for live video to it.)
                    let scope = match peer {
                        SocketAddr::V6(v6) => v6.scope_id(),
                        _ => 0,
                    };
                    if let Err(err) = connection(i, h, sock, id, ip, scope, end).await {
                        tracing::debug!("HomeKit {peer}: {err:#}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("HomeKit accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// One connection's own state.
struct Conn {
    pairing: ConnPairing,
    /// Our address on this connection (where the controller reached us).
    local: IpAddr,
    /// The peer's IPv6 scope id (its interface; 0 for others).
    scope: u32,
    session: Option<Session>,
    controller: Option<String>,
}

/// Removes the connection's traces when it ends, however it ends.
struct ConnGuard {
    inst: Arc<Instance>,
    id: u64,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        // (A poisoned lock is taken as it is: this may run while unwinding
        // from a panic, and a second panic would abort the whole program.)
        self.inst.gate.list.lock().unwrap_or_else(|p| p.into_inner()).retain(|e| !(e.inst == self.inst.uid && e.id == self.id));
        self.inst.sessions.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
        self.inst.setup.lock().unwrap_or_else(|p| p.into_inner()).release(self.id);
        // Its live streams end with it.
        self.inst.streams.end_conn(self.id);
    }
}

fn keepalive(sock: &TcpStream) {
    let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(60)).with_interval(Duration::from_secs(15));
    let _ = socket2::SockRef::from(sock).set_tcp_keepalive(&ka);
    let _ = sock.set_nodelay(true);
}

#[allow(clippy::too_many_arguments)]
async fn connection(inst: Arc<Instance>, host: Arc<dyn Host>, sock: TcpStream, id: u64, ip: IpAddr, scope: u32, end: CancellationToken) -> Result<()> {
    keepalive(&sock);
    let _guard = ConnGuard { inst: inst.clone(), id };
    let local = sock.local_addr().map(|a| super::stream::plain_ip(a.ip())).unwrap_or(IpAddr::from([0u8; 4]));
    let lim = inst.limits.clone();
    let (mut rd, mut wr) = sock.into_split();
    let events = Arc::new(Events::default());
    let mut bucket = Bucket::new(lim.event_burst, lim.event_every, Instant::now());
    let mut c = Conn { pairing: ConnPairing::new(id, ip), local, scope, session: None, controller: None };
    let mut raw: Vec<u8> = Vec::new();
    let mut plain: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 8192];
    // Unverified: the last step forward in pairing. Verified: the last request.
    let mut last_progress = Instant::now();
    let mut partial_since: Option<Instant> = None;
    loop {
        // Answer every whole request there is (none once the connection was
        // ended: its pairing was removed, HomeKit stopped, ...).
        loop {
            if end.is_cancelled() {
                return Ok(());
            }
            let src = if c.session.is_some() { &mut plain } else { &mut raw };
            match http::parse(src) {
                Parse::Done(req, used) => {
                    src.drain(..used);
                    partial_since = None;
                    if c.session.is_some() {
                        last_progress = Instant::now();
                        inst.touch(id);
                    }
                    let (resp, after, progressed) = handle(&inst, &host, &mut c, req).await;
                    for a in &after {
                        if let After::Delay(d) = a {
                            tokio::time::sleep(*d).await;
                        }
                    }
                    // (Counted from the answer: after a wrong code's wait.)
                    if progressed {
                        last_progress = Instant::now();
                    }
                    send(&mut wr, &mut c, &resp, lim.write).await?;
                    for a in after {
                        match a {
                            After::Verified { shared, controller } => {
                                if c.session.is_some() {
                                    return Err(anyhow!("verified twice"));
                                }
                                if !inst.promote(id, SessionEntry::new(controller.clone(), events.clone(), end.clone())) {
                                    return Err(anyhow!("too many HomeKit sessions"));
                                }
                                c.session = Some(Session::new(&shared.0));
                                c.controller = Some(controller);
                                last_progress = Instant::now();
                                // Anything already read is the first encrypted frame.
                                let mut rest = std::mem::take(&mut raw);
                                if let Some(s) = c.session.as_mut() {
                                    s.decrypt(&mut rest, &mut plain)?;
                                }
                                raw = rest;
                            }
                            After::Advertise => inst.advertise.notify_one(),
                            After::EndController(who) => inst.end_controller(&who),
                            After::EndAll => inst.end_all(),
                            After::Delay(_) => {}
                        }
                    }
                }
                Parse::Bad(code) => {
                    let _ = send(&mut wr, &mut c, &http::response(code, None, b""), lim.write).await;
                    return Err(anyhow!("refused a request ({code})"));
                }
                Parse::Incomplete => break,
            }
        }
        let pending = !plain.is_empty() || !raw.is_empty();
        if !pending {
            partial_since = None;
        } else if partial_since.is_none() {
            partial_since = Some(Instant::now());
        }
        let idle = if c.session.is_some() {
            last_progress + lim.verified_idle
        } else if inst.holds_setup(id) {
            last_progress + lim.timing.setup_timeout
        } else {
            last_progress + lim.unverified_idle
        };
        let deadline = match partial_since {
            Some(t) => idle.min(t + lim.partial),
            None => idle,
        };
        // Changes waiting: sent together in one event message, when the
        // session may be sent one.
        let verified = c.session.is_some();
        let ev_waiting = verified && events.is_waiting();
        let ev_at = if ev_waiting { bucket.ready_at(Instant::now()) } else { deadline };
        tokio::select! {
            // (In this order: an ended connection answers nothing more, even
            // when a request is already there to read; events due go before
            // the next read, so a controller that keeps sending requests holds
            // them back only while the requests already read (one read's
            // worth, pipelined) are answered.)
            biased;
            _ = end.cancelled() => return Ok(()),
            _ = tokio::time::sleep_until(deadline) => return Err(anyhow!("timed out")),
            _ = tokio::time::sleep_until(ev_at), if ev_waiting => {
                bucket.take(Instant::now());
                let mine: Vec<Value> = events.take().into_iter().map(|((a, i), v)| json!({ "aid": a, "iid": i, "value": v })).collect();
                if !mine.is_empty() {
                    let body = serde_json::to_vec(&json!({ "characteristics": mine })).unwrap_or_default();
                    send(&mut wr, &mut c, &http::event(&body), lim.write).await?;
                }
            }
            // (Something to send now: look again. Its wake-up is kept if it
            // came before this waits.)
            _ = events.ready.notified(), if verified && !ev_waiting => {}
            r = rd.read(&mut buf) => {
                let n = r?;
                if n == 0 {
                    return Ok(());
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(s) = c.session.as_mut() {
                    s.decrypt(&mut raw, &mut plain)?;
                }
                // One request at most: a head and a body (frames add 18 bytes per KB).
                if raw.len() > http::MAX_HEAD + http::MAX_BODY + 4096 || plain.len() > http::MAX_HEAD + http::MAX_BODY + 4 {
                    return Err(anyhow!("request too large"));
                }
            }
        }
    }
}

async fn send(wr: &mut tokio::net::tcp::OwnedWriteHalf, c: &mut Conn, bytes: &[u8], limit: Duration) -> Result<()> {
    let out = match c.session.as_mut() {
        Some(s) => s.encrypt(bytes)?,
        None => bytes.to_vec(),
    };
    tokio::time::timeout(limit, wr.write_all(&out)).await.map_err(|_| anyhow!("write timed out"))??;
    Ok(())
}

fn hap_json(code: u16, v: &Value) -> Vec<u8> {
    http::response(code, Some("application/hap+json"), &serde_json::to_vec(v).unwrap_or_default())
}

fn tlv_response(body: &[u8]) -> Vec<u8> {
    http::response(200, Some("application/pairing+tlv8"), body)
}

fn hap_status(code: u16, s: i64) -> Vec<u8> {
    hap_json(code, &json!({ "status": s }))
}

/// A wrong setup code's answer (M4 with an authentication error). The
/// connection may try again, as a phone does when the code was mistyped, so
/// it counts as a step forward: its idle time starts again.
fn wrong_code(body: &[u8]) -> bool {
    use super::tlv::{err, t, Tlv};
    Tlv::decode(body).is_some_and(|m| m.byte(t::STATE) == Some(4) && m.byte(t::ERROR) == Some(err::AUTHENTICATION))
}

/// The answer, what to do after it, and whether it was a step forward in
/// pairing (which keeps an unverified connection open).
async fn handle(inst: &Arc<Instance>, host: &Arc<dyn Host>, c: &mut Conn, req: Request) -> (Vec<u8>, Vec<After>, bool) {
    let verified = c.session.is_some();
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/pair-setup") if !verified => {
            let setup = pairing::Setup { state: &inst.state, slot: &inst.setup, window: &inst.window, kind: &inst.kind, timing: &inst.limits.timing };
            let (body, after) = pairing::pair_setup(&setup, &mut c.pairing, &req.body).await;
            let ok = pairing::is_success(&body) || wrong_code(&body);
            (tlv_response(&body), after, ok)
        }
        ("POST", "/pair-verify") if !verified => {
            let (body, after) = pairing::pair_verify(&inst.state, &mut c.pairing, &req.body).await;
            let ok = pairing::is_success(&body);
            (tlv_response(&body), after, ok)
        }
        ("POST", "/identify") if !verified => {
            if inst.state.lock().unwrap().paired() {
                (hap_status(400, status::INSUFFICIENT_PRIVILEGES), vec![], false)
            } else {
                tracing::info!("HomeKit: identify requested");
                (http::response(204, None, b""), vec![], false)
            }
        }
        _ if !verified => (hap_status(470, status::INSUFFICIENT_PRIVILEGES), vec![], false),
        ("POST", "/pairings") => {
            let who = c.controller.clone().unwrap_or_default();
            let mut st = inst.state.lock().unwrap();
            let (body, after) = pairing::pairings(&mut st, &who, &req.body);
            (tlv_response(&body), after, false)
        }
        ("GET", "/accessories") => (hap_json(200, &inst.db.lock().unwrap().json()), vec![], false),
        ("GET", "/characteristics") => (get_characteristics(inst, c.pairing.id, &req.query), vec![], false),
        ("PUT", "/characteristics") => {
            let at = Writer { conn: c.pairing.id, peer: c.pairing.ip, scope: c.scope, local: c.local, host };
            (put_characteristics(inst, &at, &req.body), vec![], false)
        }
        // Timed writes: nothing here needs them; accept the preparation.
        ("PUT", "/prepare") => (hap_status(200, status::OK), vec![], false),
        ("POST", "/resource") => (resource(inst, host, &req.body).await, vec![], false),
        _ => (hap_status(404, status::NOT_FOUND), vec![], false),
    }
}

fn parse_id(s: &str) -> Option<(u64, u64)> {
    let (a, i) = s.split_once('.')?;
    Some((a.parse().ok()?, i.parse().ok()?))
}

/// `GET /characteristics?id=1.2,2.9[&meta=1&perms=1&type=1&ev=1]`.
fn get_characteristics(inst: &Instance, conn: u64, query: &str) -> Vec<u8> {
    let q = http::query_pairs(query);
    let flag = |k: &str| q.iter().any(|(n, v)| *n == k && *v == "1");
    let Some(ids) = q.iter().find(|(n, _)| *n == "id").map(|(_, v)| *v) else { return hap_status(400, status::INVALID_VALUE) };
    let ids: Vec<&str> = ids.split(',').collect();
    if ids.len() > MAX_IDS {
        return hap_status(400, status::OUT_OF_RESOURCES);
    }
    let subs: HashSet<(u64, u64)> = inst.sessions.lock().unwrap().get(&conn).map(|s| s.subs.clone()).unwrap_or_default();
    let db = inst.db.lock().unwrap();
    let mut any_error = false;
    let mut out: Vec<(Value, i64)> = Vec::new();
    for raw in ids {
        let Some((aid, iid)) = parse_id(raw) else {
            any_error = true;
            out.push((json!({ "aid": 0, "iid": 0 }), status::INVALID_VALUE));
            continue;
        };
        match db.get(aid, iid) {
            None => {
                any_error = true;
                out.push((json!({ "aid": aid, "iid": iid }), status::NOT_FOUND));
            }
            Some(ch) if !ch.perms.read => {
                any_error = true;
                out.push((json!({ "aid": aid, "iid": iid }), status::WRITE_ONLY));
            }
            Some(ch) => {
                let ev = flag("ev").then(|| subs.contains(&(aid, iid)));
                let mut j = ch.json(Some(aid), flag("meta"), flag("perms"), flag("type"), ev);
                if ch.iid == iid::SETUP_ENDPOINTS {
                    j["value"] = setup_answer(inst, conn, aid);
                }
                out.push((j, status::OK));
            }
        }
    }
    let list: Vec<Value> = out
        .into_iter()
        .map(|(mut v, s)| {
            if any_error {
                v["status"] = s.into();
            }
            v
        })
        .collect();
    hap_json(if any_error { 207 } else { 200 }, &json!({ "characteristics": list }))
}

pub(crate) fn as_bool(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| v.as_u64().and_then(|n| match n {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }))
}

/// `PUT /characteristics {"characteristics":[{aid, iid, value?, ev?, r?}]}`.
/// Who writes: the connection, its peer and our address on it.
struct Writer<'a> {
    conn: u64,
    peer: IpAddr,
    scope: u32,
    local: IpAddr,
    host: &'a Arc<dyn Host>,
}

fn put_characteristics(inst: &Instance, at: &Writer, body: &[u8]) -> Vec<u8> {
    let conn = at.conn;
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return hap_status(400, status::INVALID_VALUE) };
    let Some(items) = v.get("characteristics").and_then(|c| c.as_array()) else { return hap_status(400, status::INVALID_VALUE) };
    if items.len() > MAX_IDS {
        return hap_status(400, status::OUT_OF_RESOURCES);
    }
    let mut results: Vec<Value> = Vec::new();
    let mut all_ok = true;
    let mut changes: Vec<(u64, u64, Value)> = Vec::new();
    for item in items {
        let (Some(aid), Some(iid)) = (item.get("aid").and_then(Value::as_u64), item.get("iid").and_then(Value::as_u64)) else {
            all_ok = false;
            results.push(json!({ "aid": 0, "iid": 0, "status": status::INVALID_VALUE }));
            continue;
        };
        let mut st = status::OK;
        let mut answer: Option<Value> = None;
        let found = inst.db.lock().unwrap().get(aid, iid).cloned();
        match found {
            None => st = status::NOT_FOUND,
            Some(ch) => {
                if let Some(ev) = item.get("ev") {
                    match as_bool(ev) {
                        None => st = status::INVALID_VALUE,
                        Some(_) if !ch.perms.events => st = status::NO_NOTIFICATION,
                        Some(on) => {
                            let mut sessions = inst.sessions.lock().unwrap();
                            if let Some(s) = sessions.get_mut(&conn) {
                                if on && s.subs.len() >= MAX_SUBSCRIPTIONS && !s.subs.contains(&(aid, iid)) {
                                    st = status::OUT_OF_RESOURCES;
                                } else if on {
                                    s.subs.insert((aid, iid));
                                } else {
                                    s.subs.remove(&(aid, iid));
                                    // (No event for it after it was turned off.)
                                    s.events.forget(&(aid, iid));
                                }
                            }
                        }
                    }
                }
                if st == status::OK {
                    if let Some(value) = item.get("value") {
                        if !ch.perms.write {
                            st = status::READ_ONLY;
                        } else if !ch.accepts(value) {
                            st = status::INVALID_VALUE;
                        } else {
                            match write(inst, at, aid, iid, value) {
                                Ok(changed) => {
                                    if changed {
                                        let nv = if iid == iid::SETUP_ENDPOINTS { Some(setup_answer(inst, conn, aid)) } else { inst.db.lock().unwrap().get(aid, iid).map(|c| c.value.clone()) };
                                        if let Some(nv) = nv {
                                            if ch.perms.events {
                                                changes.push((aid, iid, nv.clone()));
                                            }
                                            if item.get("r").and_then(as_bool) == Some(true) && ch.perms.read {
                                                answer = Some(nv);
                                            }
                                        }
                                    }
                                }
                                Err(code) => st = code,
                            }
                        }
                    }
                }
            }
        }
        if st != status::OK || answer.is_some() {
            all_ok = false;
        }
        let mut r = json!({ "aid": aid, "iid": iid, "status": st });
        if let Some(a) = answer {
            r["value"] = a;
        }
        results.push(r);
    }
    inst.notify(&changes, Some(conn));
    if all_ok {
        http::response(204, None, b"")
    } else {
        hap_json(207, &json!({ "characteristics": results }))
    }
}

/// SetupEndpoints as this connection reads it: its own answer.
fn setup_answer(inst: &Instance, conn: u64, aid: u64) -> Value {
    base64::engine::general_purpose::STANDARD.encode(inst.streams.answer(conn, aid)).into()
}

/// A controller's write; Ok(true) when the stored value changed.
fn write(inst: &Instance, at: &Writer, aid: u64, iid: u64, value: &Value) -> Result<bool, i64> {
    let decode = |v: &Value| v.as_str().and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok()).ok_or(status::INVALID_VALUE);
    let camera = match inst.db.lock().unwrap().accessory(aid).map(|a| a.role.clone()) {
        Some(Role::Camera { id, name }) => Some((id, name)),
        Some(Role::Bridge) => None,
        None => return Err(status::NOT_FOUND),
    };
    let set = |v: Value| -> Result<bool, i64> {
        let mut db = inst.db.lock().unwrap();
        let ch = db.get_mut(aid, iid).ok_or(status::NOT_FOUND)?;
        let changed = ch.value != v;
        ch.value = v;
        Ok(changed)
    };
    match (iid, camera) {
        (iid::IDENTIFY, _) => {
            tracing::info!("HomeKit: identify requested for accessory {aid}");
            Ok(false)
        }
        (iid::ACTIVE, Some(_)) => set(value.as_u64().unwrap_or(0).into()),
        (iid::MUTE, Some(_)) => set(super::server::as_bool(value).unwrap_or(false).into()),
        (iid::SETUP_ENDPOINTS, Some(_)) => {
            let asked = decode(value)?;
            // (Kept per connection by the streams: each controller reads its own answer.)
            inst.streams.setup(at.conn, aid, at.peer, at.scope, at.local, &asked)?;
            // (True: a write with "r" gets the answer back at once.)
            Ok(true)
        }
        (iid::SELECTED_RTP, Some((id, name))) => {
            let asked = decode(value)?;
            let host = at.host.clone();
            // Home's Mute of the camera's Microphone: no sound in the streams it starts.
            let muted = inst.db.lock().unwrap().get(aid, iid::MUTE).is_some_and(|c| c.value == Value::Bool(true));
            inst.streams.select(at.conn, aid, &asked, muted, &move |w, h| host.live_source(id, &name, w, h))?;
            set(value.clone())?;
            Ok(true)
        }
        _ => Err(status::READ_ONLY),
    }
}

/// `POST /resource {"resource-type":"image","image-width":W,"image-height":H,"aid":N}`:
/// the camera's latest picture, made smaller to fit the size asked for.
async fn resource(inst: &Instance, host: &Arc<dyn Host>, body: &[u8]) -> Vec<u8> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return hap_status(400, status::INVALID_VALUE) };
    if v.get("resource-type").and_then(Value::as_str) != Some("image") {
        return hap_status(400, status::INVALID_VALUE);
    }
    let w = v.get("image-width").and_then(Value::as_u64).unwrap_or(0).min(7680) as u32;
    let h = v.get("image-height").and_then(Value::as_u64).unwrap_or(0).min(4320) as u32;
    let Some(aid) = v.get("aid").and_then(Value::as_u64) else { return hap_status(404, status::NOT_FOUND) };
    let target = inst.db.lock().unwrap().accessory(aid).map(|a| a.role.clone());
    let Some(Role::Camera { id, name }) = target else { return hap_status(404, status::NOT_FOUND) };
    // Only a camera that is (still) shown in Apple Home.
    let Some(frame) = host.frame(id, &name) else { return hap_status(404, status::NOT_FOUND) };
    if frame.is_empty() {
        return hap_status(503, status::UNABLE_TO_COMMUNICATE);
    }
    let pic = tokio::task::spawn_blocking(move || fit_jpeg(frame, w, h)).await.unwrap_or_default();
    if pic.is_empty() {
        return hap_status(500, status::UNABLE_TO_COMMUNICATE);
    }
    http::response(200, Some("image/jpeg"), &pic)
}

/// Shrink a JPEG to fit w x h (never enlarged; 0 = no limit). The input
/// as it is when it already fits or cannot be read.
pub fn fit_jpeg(jpeg: Vec<u8>, w: u32, h: u32) -> Vec<u8> {
    if w == 0 && h == 0 {
        return jpeg;
    }
    let Ok(img) = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg) else { return jpeg };
    let (iw, ih) = (img.width(), img.height());
    let (mw, mh) = (if w == 0 { iw } else { w }, if h == 0 { ih } else { h });
    if iw <= mw && ih <= mh {
        return jpeg;
    }
    let small = img.resize(mw.max(16), mh.max(16), image::imageops::FilterType::Triangle);
    let mut out = Vec::new();
    match image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&small) {
        Ok(()) => out,
        Err(_) => jpeg,
    }
}

/// What the listener is bound to, so mDNS announces only what it serves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Listen {
    /// IPv6 and IPv4 on every interface.
    Dual,
    /// IPv4 on every interface.
    V4,
    /// One address.
    Addr(IpAddr),
}

fn listen_on(addr: SocketAddr, v6only: Option<bool>) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if let Some(v) = v6only {
        s.set_only_v6(v)?;
    }
    // (Windows: SO_REUSEADDR would let another program share the port.)
    #[cfg(unix)]
    s.set_reuse_address(true)?;
    s.bind(&addr.into())?;
    s.listen(128)?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

fn bind_error(port: u16, e: &std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        anyhow!("port {port} is already in use by another program (Homebridge, for one, uses 51826); choose another HomeKit port")
    } else {
        anyhow!("could not open port {port}: {e}")
    }
}

/// Open the HomeKit port. `bind`: "[::]" (the default: IPv6 and IPv4, or
/// IPv4 alone where there is no IPv6), "0.0.0.0", or one address.
pub fn bind(bind: &str, port: u16) -> Result<(TcpListener, Listen)> {
    let b = bind.trim().trim_start_matches('[').trim_end_matches(']');
    let ip: IpAddr = if b.is_empty() { IpAddr::from([0u16; 8]) } else { b.parse().map_err(|_| anyhow!("[homekit] bind '{bind}' is not an address"))? };
    let (std, how) = if ip.is_unspecified() && ip.is_ipv6() {
        match listen_on(SocketAddr::new(ip, port), Some(false)) {
            Ok(l) => (l, Listen::Dual),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Err(bind_error(port, &e)),
            // No IPv6 here: IPv4 alone.
            Err(_) => (listen_on(SocketAddr::new(IpAddr::from([0, 0, 0, 0]), port), None).map_err(|e| bind_error(port, &e))?, Listen::V4),
        }
    } else if ip.is_unspecified() {
        (listen_on(SocketAddr::new(ip, port), None).map_err(|e| bind_error(port, &e))?, Listen::V4)
    } else {
        let v6only = ip.is_ipv6().then_some(true);
        (listen_on(SocketAddr::new(ip, port), v6only).map_err(|e| bind_error(port, &e))?, Listen::Addr(ip))
    };
    Ok((TcpListener::from_std(std)?, how))
}

/// The address the listener has, for logs.
pub fn local(l: &TcpListener) -> Option<SocketAddr> {
    l.local_addr().ok()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::homekit::testclient::{Hap, SetupClient};
    use crate::homekit::tlv::{err, t, Tlv};
    use crate::homekit::CamNow;

    #[test]
    fn ids_parse() {
        assert_eq!(parse_id("1.2"), Some((1, 2)));
        assert_eq!(parse_id("1"), None);
        assert_eq!(parse_id("a.2"), None);
        assert_eq!(parse_id("1.-2"), None);
    }

    #[test]
    fn jpeg_is_shrunk_to_fit_never_enlarged() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&image::RgbImage::from_pixel(640, 360, image::Rgb([200, 100, 50]))).unwrap();
        let small = fit_jpeg(jpeg.clone(), 320, 240);
        let s = image::load_from_memory(&small).unwrap();
        assert_eq!((s.width(), s.height()), (320, 180));
        assert_eq!(fit_jpeg(jpeg.clone(), 1920, 1080), jpeg);
        assert_eq!(fit_jpeg(jpeg.clone(), 0, 0), jpeg);
        assert_eq!(fit_jpeg(b"not a jpeg".to_vec(), 10, 10), b"not a jpeg".to_vec());
    }

    /// A host with two cameras, one shown in Apple Home.
    pub struct FakeHost {
        pub dir: std::path::PathBuf,
        pub settings: Mutex<crate::homekit::Settings>,
    }

    pub fn tiny_jpeg() -> Vec<u8> {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&image::RgbImage::from_pixel(64, 36, image::Rgb([9, 9, 9]))).unwrap();
        jpeg
    }

    impl Host for FakeHost {
        fn settings(&self) -> crate::homekit::Settings {
            self.settings.lock().unwrap().clone()
        }
        fn live_source(&self, _id: usize, _name: &str, _w: u32, _h: u32) -> Option<crate::homekit::stream::Source> {
            None
        }
        fn state_dir(&self) -> std::path::PathBuf {
            self.dir.clone()
        }
        fn cameras(&self) -> Vec<CamNow> {
            vec![
                CamNow { id: 0, name: "Shown".into(), homekit: true, motion: false, online: true, streamable: true, picture: String::new() },
                CamNow { id: 1, name: "Hidden".into(), homekit: false, motion: false, online: true, streamable: true, picture: String::new() },
            ]
        }
        fn frame(&self, id: usize, name: &str) -> Option<Vec<u8>> {
            self.cameras().into_iter().find(|c| c.id == id && c.name == name && c.homekit).map(|_| tiny_jpeg())
        }
        fn mdns(&self) -> bool {
            false
        }
        fn firewall(&self) -> bool {
            false
        }
    }

    fn fast_limits() -> Limits {
        Limits {
            max_unverified: 20,
            max_unverified_per_ip: 20,
            max_verified: 2,
            unverified_idle: Duration::from_millis(600),
            partial: Duration::from_millis(600),
            timing: Timing { setup_timeout: Duration::from_secs(3), m3_window: Duration::from_millis(400) },
            ..Limits::default()
        }
    }

    struct Rig {
        inst: Arc<Instance>,
        port: u16,
        stop: CancellationToken,
        dir: std::path::PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop.cancel();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn rig(name: &str, limits: Limits) -> Rig {
        let dir = std::env::temp_dir().join(format!("as-hk-srv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let host: Arc<dyn Host> = Arc::new(FakeHost { dir: dir.clone(), settings: Mutex::new(crate::homekit::Settings { enabled: true, rtp: (51900, 51999), bind: "127.0.0.1".into(), port: 0, name: "Test".into(), standalone: false }) });
        let st = State::load_or_create(&dir).unwrap();
        let inst = Arc::new(Instance::new(Kind::Bridge, "Test".into(), 0, Arc::new(Mutex::new(st)), limits));
        super::super::sync_with(&inst, &host.cameras());
        // (Pairing needs an open window: an admin opened one.)
        inst.window.lock().unwrap().open(Kind::Bridge, pairing::WINDOW);
        let (l, _) = bind("127.0.0.1", 0).unwrap();
        let port = local(&l).unwrap().port();
        let stop = CancellationToken::new();
        tokio::spawn(accept_loop(inst.clone(), host, l, stop.clone()));
        Rig { inst, port, stop, dir }
    }

    impl Rig {
        fn code(&self) -> String {
            self.inst.state.lock().unwrap().setup_code.clone()
        }
        fn ltpk(&self) -> [u8; 32] {
            self.inst.state.lock().unwrap().key().unwrap().public()
        }
        /// A paired, verified connection for `c`.
        async fn session(&self, c: &SetupClient) -> Hap {
            let mut h = Hap::connect(self.port).await;
            h.pair_verify(c, &self.ltpk()).await.expect("verify");
            h
        }
    }

    fn err_of(body: &[u8]) -> Option<u8> {
        Tlv::decode(body).and_then(|t| t.byte(t::ERROR))
    }

    /// Store `who` as a paired controller (its key does not matter here).
    fn paired(inst: &Instance, who: &str, admin: bool) {
        let mut st = inst.state.lock().unwrap();
        st.pairings.retain(|p| p.id != who);
        st.pairings.push(crate::homekit::state::Pairing { id: who.into(), ltpk: "02".repeat(32), admin });
    }

    #[test]
    fn unverified_limits_span_every_accessory() {
        let dir = std::env::temp_dir().join(format!("as-hk-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = || Arc::new(Mutex::new(State::load_or_create(&dir).unwrap()));
        let gate = Arc::new(Gate::default());
        let lim = Limits { max_unverified: 100, max_unverified_per_ip: 2, ..Limits::default() };
        let a = Instance::new_in(Kind::Camera("A".into()), "A".into(), 0, st(), lim.clone(), gate.clone(), Arc::default());
        let b = Instance::new_in(Kind::Camera("B".into()), "B".into(), 0, st(), lim, gate.clone(), Arc::default());
        let ip = |n: u8| IpAddr::from([192, 0, 2, n]);
        // One address's connections count across accessories.
        assert!(a.admit(1, ip(1), CancellationToken::new()));
        assert!(b.admit(1, ip(1), CancellationToken::new()));
        assert!(!a.admit(2, ip(1), CancellationToken::new()));
        // When all are taken, the oldest anywhere gives way.
        let mut n = 10u8;
        while gate.list.lock().unwrap().len() < MAX_UNVERIFIED_ALL {
            assert!(a.admit(n as u64, ip(n), CancellationToken::new()));
            n += 1;
        }
        let oldest = gate.list.lock().unwrap()[0].end.clone();
        assert!(b.admit(99, ip(200), CancellationToken::new()));
        assert!(oldest.is_cancelled(), "the oldest of all was closed");
        assert_eq!(gate.list.lock().unwrap().len(), MAX_UNVERIFIED_ALL);
        a.leave_unverified(10);
        assert!(gate.list.lock().unwrap().iter().all(|e| !(e.inst == a.uid && e.id == 10)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn one_controller_cannot_take_every_session() {
        let dir = std::env::temp_dir().join(format!("as-hk-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = Arc::new(Mutex::new(State::load_or_create(&dir).unwrap()));
        let lim = Limits { max_verified: 16, max_verified_per_controller: 4, ..Limits::default() };
        let inst = Instance::new(Kind::Bridge, "T".into(), 0, st, lim);
        let mut next = 0u64;
        let mut open = |who: &str| {
            next += 1;
            let end = CancellationToken::new();
            let tx = Arc::new(Events::default());
            let admin = who.starts_with("admin");
            paired(&inst, who, admin);
            let ok = inst.promote(next, SessionEntry::new(who.into(), tx, end.clone()));
            assert!(ok || !admin, "an admin always gets in");
            (next, end)
        };
        let count = |who: &str| inst.sessions.lock().unwrap().values().filter(|e| e.controller == who).count();
        // A guest opening many: it keeps its cap, its oldest give way.
        let guest: Vec<_> = (0..20).map(|_| open("guest")).collect();
        assert_eq!(count("guest"), 4);
        assert!(guest[..16].iter().all(|(_, e)| e.is_cancelled()) && guest[16..].iter().all(|(_, e)| !e.is_cancelled()));
        // Three more controllers fill the room.
        let mut others = Vec::new();
        for who in ["u2", "u3", "u4"] {
            for _ in 0..4 {
                others.push(open(who));
            }
        }
        assert_eq!(inst.session_count(), 16);
        // The admin still gets in: the least recently used session of a
        // controller with the most gives way (here the guest's oldest left).
        inst.touch(guest[17].0);
        inst.touch(guest[18].0);
        inst.touch(guest[19].0);
        let (_, admin) = open("admin");
        assert!(!admin.is_cancelled());
        assert_eq!((inst.session_count(), count("admin"), count("guest")), (16, 1, 3));
        assert!(guest[16].1.is_cancelled(), "the guest's least recently used one");
        // Again and again: the admin is never kept out, and its own sessions
        // are not pushed out by the others while it has fewer.
        for _ in 0..3 {
            open("admin");
        }
        assert_eq!(count("admin"), 4);
        for _ in 0..10 {
            open("guest");
        }
        assert_eq!((count("admin"), count("guest")), (4, 4), "the guest takes from the others, not the admin");
        // Room taken by admins only: a guest is refused, never an admin.
        drop(open);
        let lim = Limits { max_verified: 4, max_verified_per_controller: 4, ..Limits::default() };
        let inst2 = Instance::new(Kind::Bridge, "T2".into(), 0, Arc::new(Mutex::new(State::load_or_create(&dir).unwrap())), lim);
        let entry = |who: &str, admin: bool| {
            paired(&inst2, who, admin);
            SessionEntry::new(who.into(), Arc::new(Events::default()), CancellationToken::new())
        };
        for i in 0..4 {
            assert!(inst2.promote(i, entry("admin", true)));
        }
        assert!(!inst2.promote(9, entry("guest", false)));
        assert!(inst2.promote(10, entry("admin2", true)), "another admin may push one out");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sessions_in_use_and_admins_give_way_last() {
        let dir = std::env::temp_dir().join(format!("as-hk-inuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = Arc::new(Mutex::new(State::load_or_create(&dir).unwrap()));
        let inst = Instance::new(Kind::Bridge, "T".into(), 0, st, Limits { max_verified: 5, max_verified_per_controller: 4, ..Limits::default() });
        let open = |id: u64, who: &str, admin: bool| {
            // (Distinct times, for which is least recently used.)
            std::thread::sleep(Duration::from_millis(2));
            let end = CancellationToken::new();
            paired(&inst, who, admin);
            assert!(inst.promote(id, SessionEntry::new(who.into(), Arc::new(Events::default()), end.clone())));
            end
        };
        let alive = |id: u64| inst.sessions.lock().unwrap().contains_key(&id);
        // The admin's home hub: its oldest and least recently used session,
        // idle but subscribed to events.
        let hub = open(1, "admin", true);
        inst.sessions.lock().unwrap().get_mut(&1).unwrap().subs.insert((2, 10));
        open(2, "admin", true);
        for id in 3..=5 {
            open(id, "guest", false);
        }
        // The room is full: an admin's new session closes a guest's first,
        // though the admin's own are used less recently.
        open(6, "admin", true);
        assert!(!alive(3) && alive(1) && alive(2));
        // A guest never closes an admin's, even an idle one: only a guest's.
        open(7, "guest", false);
        assert!(!alive(4) && alive(1) && alive(2) && alive(6));
        open(8, "admin", true);
        assert!(!alive(5) && alive(7));
        // The admin at its most: its own sessions not in use go first, the
        // hub stays, and so does one that only got events lately.
        inst.sessions.lock().unwrap().get_mut(&6).unwrap().evented = Some(Instant::now());
        open(10, "admin", true);
        assert!(!alive(2) && alive(1) && alive(6) && alive(8));
        open(11, "admin", true);
        assert!(!alive(8) && alive(1) && alive(6) && !hub.is_cancelled());
        // Every one in use: then the least recently used of them gives way.
        for id in [10, 11] {
            inst.sessions.lock().unwrap().get_mut(&id).unwrap().subs.insert((2, 10));
        }
        open(12, "admin", true);
        assert!(hub.is_cancelled() && alive(6) && alive(10) && alive(11));
        // Another admin: the guest's last session first, then an admin's
        // not in use; with admins only, a guest is refused.
        open(13, "admin2", true);
        assert!(!alive(7) && alive(12));
        open(14, "admin2", true);
        assert!(!alive(12) && alive(6) && alive(10) && alive(11) && alive(13));
        assert!(!inst.promote(15, SessionEntry::new("guest".into(), Arc::new(Events::default()), CancellationToken::new())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn admin_rights_changed_after_verify_count_for_open_sessions() {
        let dir = std::env::temp_dir().join(format!("as-hk-rights-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = Arc::new(Mutex::new(State::load_or_create(&dir).unwrap()));
        let inst = Instance::new(Kind::Bridge, "T".into(), 0, st, Limits { max_verified: 3, max_verified_per_controller: 3, ..Limits::default() });
        let open = |id: u64, who: &str| {
            std::thread::sleep(Duration::from_millis(2));
            inst.promote(id, SessionEntry::new(who.into(), Arc::new(Events::default()), CancellationToken::new()))
        };
        let alive = |id: u64| inst.sessions.lock().unwrap().contains_key(&id);
        // A change made the way a controller makes it: POST /pairings by the admin "a".
        let change = |who: &str, perm: Option<u8>| {
            let mut m = Tlv::new();
            m.push_u8(t::STATE, 1);
            match perm {
                Some(p) => m.push_u8(t::METHOD, 3).push(t::IDENTIFIER, who.as_bytes()).push(t::PUBLIC_KEY, &[2; 32]).push_u8(t::PERMISSIONS, p),
                None => m.push_u8(t::METHOD, 4).push(t::IDENTIFIER, who.as_bytes()),
            };
            let (body, _) = pairing::pairings(&mut inst.state.lock().unwrap(), "a", &m.encode());
            assert_eq!(err_of(&body), None, "{who} {perm:?}");
        };
        paired(&inst, "a", true);
        paired(&inst, "b", true);
        assert!(open(1, "a") && open(2, "b") && open(3, "b"));
        // Only admins: a guest is refused.
        paired(&inst, "g", false);
        assert!(!open(4, "g"));
        // "b" loses its admin rights: its open sessions are a guest's now.
        change("b", Some(0));
        assert!(open(4, "g"));
        assert!(!alive(2) && alive(1) && alive(3));
        // "g" is made an admin: its open session is an admin's now, and a
        // new guest closes the other guest's instead.
        change("g", Some(1));
        paired(&inst, "c", false);
        assert!(open(5, "c"));
        assert!(!alive(3) && alive(4));
        assert!(open(6, "c"));
        assert!(!alive(5) && alive(1) && alive(4));
        // "a" is removed (its sessions are being ended): no longer an admin's.
        change("a", None);
        assert!(open(7, "c"));
        assert!(!alive(1) && alive(4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_guest_holding_every_session_does_not_keep_the_admin_out() {
        let r = rig("sessions", Limits { max_verified: 3, max_verified_per_controller: 2, ..fast_limits() }).await;
        let admin = SetupClient::new(&r.code(), "C1", 1);
        let mut h = Hap::connect(r.port).await;
        h.pair_setup(&admin).await.unwrap();
        drop(h);
        let mut a = r.session(&admin).await;
        let guest = SetupClient::new("", "G1", 2);
        let mut add = Tlv::new();
        add.push_u8(t::STATE, 1).push_u8(t::METHOD, 3).push(t::IDENTIFIER, b"G1").push(t::PUBLIC_KEY, &guest.ltk.public()).push_u8(t::PERMISSIONS, 0);
        assert_eq!(err_of(&a.tlv("/pairings", &add.encode()).await.unwrap().1), None);
        drop(a);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The guest opens more than its share: its oldest give way.
        let mut g = Vec::new();
        for _ in 0..4 {
            let mut s = r.session(&guest).await;
            assert_eq!(s.request("GET", "/accessories", "x", b"").await.unwrap().0, 200);
            g.push(s);
        }
        assert!(g[0].closed_within(500).await && g[1].closed_within(500).await);
        assert_eq!(r.inst.session_count(), 2);
        // The admin gets in, and in again (the room is full then).
        let mut a1 = r.session(&admin).await;
        assert_eq!(a1.request("GET", "/accessories", "x", b"").await.unwrap().0, 200);
        let mut a2 = r.session(&admin).await;
        assert_eq!(a2.request("GET", "/accessories", "x", b"").await.unwrap().0, 200);
        assert_eq!(r.inst.session_count(), 3);
    }

    #[tokio::test]
    async fn unverified_gets_470_and_its_timer_is_not_reset_by_it() {
        let r = rig("470", fast_limits()).await;
        let mut h = Hap::connect(r.port).await;
        for path in ["/accessories", "/characteristics?id=1.2"] {
            let (code, _) = h.request("GET", path, "application/hap+json", b"").await.unwrap();
            assert_eq!(code, 470, "{path}");
        }
        // Keep asking: the connection still closes after the unverified limit.
        let t0 = std::time::Instant::now();
        loop {
            match h.request("GET", "/accessories", "application/hap+json", b"").await {
                Some((470, _)) if t0.elapsed() < Duration::from_secs(3) => tokio::time::sleep(Duration::from_millis(100)).await,
                Some(other) => panic!("still answered after {:?}: {}", t0.elapsed(), other.0),
                None => break,
            }
        }
        assert!(t0.elapsed() < Duration::from_millis(1500), "closed after {:?}", t0.elapsed());
    }

    #[tokio::test]
    async fn a_wrong_code_leaves_the_connection_its_idle_time_to_try_again() {
        let r = rig("wrong", fast_limits()).await;
        let mut h = Hap::connect(r.port).await;
        let mut wrong = SetupClient::new("111-22-333", "W1", 3);
        let (_, m2) = h.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
        assert_eq!(err_of(&m2), None);
        // Typing the code takes longer than the unverified idle time (the
        // pair-setup holder may), then the code is wrong.
        tokio::time::sleep(Duration::from_millis(900)).await;
        let m3 = wrong.m3(&m2).unwrap();
        let (_, m4) = h.tlv("/pair-setup", &m3).await.unwrap();
        assert_eq!(err_of(&m4), Some(err::AUTHENTICATION));
        // The connection stays open to try again, and may.
        assert!(!h.closed_within(300).await, "closed at once after a wrong code");
        let right = SetupClient::new(&r.code(), "W1", 3);
        h.pair_setup(&right).await.expect("a second try on the same connection");
        // Without a further step it still closes after its idle time.
        let mut h2 = Hap::connect(r.port).await;
        let (_, m2) = h2.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
        assert!(err_of(&m2).is_some(), "paired now: M1 refused");
        assert!(h2.closed_within(1500).await);
    }

    #[tokio::test]
    async fn pairing_end_to_end_and_hidden_camera_never_shown() {
        let r = rig("e2e", fast_limits()).await;
        let c = SetupClient::new(&r.code(), "C1", 1);
        let mut h = Hap::connect(r.port).await;
        h.pair_setup(&c).await.unwrap();
        let mut s = r.session(&c).await;
        let (code, body) = s.request("GET", "/accessories", "application/hap+json", b"").await.unwrap();
        assert_eq!(code, 200);
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("Shown") && !text.contains("Hidden"), "{text}");
        let j: Value = serde_json::from_str(&text).unwrap();
        let aids: Vec<u64> = j["accessories"].as_array().unwrap().iter().map(|a| a["aid"].as_u64().unwrap()).collect();
        assert_eq!(aids, vec![1, 2]);
        // A snapshot of the shown camera; nothing for any other aid.
        let (code, pic) = s.request("POST", "/resource", "application/json", br#"{"resource-type":"image","image-width":32,"image-height":18,"aid":2}"#).await.unwrap();
        assert_eq!((code, &pic[..2]), (200, &b"\xff\xd8"[..]));
        for aid in [1, 3, 99] {
            let (code, _) = s.request("POST", "/resource", "application/json", format!(r#"{{"resource-type":"image","aid":{aid}}}"#).as_bytes()).await.unwrap();
            assert_eq!(code, 404, "aid {aid}");
        }
        // Malformed bodies get errors on a live session.
        for body in [&b"not json"[..], br#"{"characteristics":5}"#, br#"{"characteristics":[{"aid":"x"}]}"#] {
            let (code, _) = s.request("PUT", "/characteristics", "application/hap+json", body).await.unwrap();
            assert!(code == 400 || code == 207, "{code}");
        }
        let (code, _) = s.request("GET", "/characteristics?id=9.9,zz", "application/hap+json", b"").await.unwrap();
        assert_eq!(code, 207);
        // A non-admin controller cannot change pairings.
        let user = SetupClient::new("", "U1", 2);
        let mut add = Tlv::new();
        add.push_u8(t::STATE, 1).push_u8(t::METHOD, 3).push(t::IDENTIFIER, b"U1").push(t::PUBLIC_KEY, &user.ltk.public()).push_u8(t::PERMISSIONS, 0);
        let (_, b) = s.tlv("/pairings", &add.encode()).await.unwrap();
        assert_eq!(err_of(&b), None);
        let mut us = r.session(&user).await;
        let mut rm = Tlv::new();
        rm.push_u8(t::STATE, 1).push_u8(t::METHOD, 4).push(t::IDENTIFIER, b"C1");
        let (_, b) = us.tlv("/pairings", &rm.encode()).await.unwrap();
        assert_eq!(err_of(&b), Some(err::AUTHENTICATION));
        let mut add2 = Tlv::new();
        add2.push_u8(t::STATE, 1).push_u8(t::METHOD, 3).push(t::IDENTIFIER, b"X").push(t::PUBLIC_KEY, &[7; 32]).push_u8(t::PERMISSIONS, 1);
        let (_, b) = us.tlv("/pairings", &add2.encode()).await.unwrap();
        assert_eq!(err_of(&b), Some(err::AUTHENTICATION));
        // The admin removes the user: its session ends, and it can no longer verify.
        let mut rmu = Tlv::new();
        rmu.push_u8(t::STATE, 1).push_u8(t::METHOD, 4).push(t::IDENTIFIER, b"U1");
        let (_, b) = s.tlv("/pairings", &rmu.encode()).await.unwrap();
        assert_eq!(err_of(&b), None);
        assert!(us.closed_within(1000).await);
        let mut again = Hap::connect(r.port).await;
        assert!(again.pair_verify(&user, &r.ltpk()).await.is_err());
        // The admin removes itself: unpaired, same code, pairable again.
        let mut rm1 = Tlv::new();
        rm1.push_u8(t::STATE, 1).push_u8(t::METHOD, 4).push(t::IDENTIFIER, b"C1");
        let _ = s.tlv("/pairings", &rm1.encode()).await;
        assert!(s.closed_within(1000).await);
        // Pairing again needs a window again (it closed when it first paired).
        let mut h2 = Hap::connect(r.port).await;
        assert_eq!(h2.pair_setup(&c).await, Err(err::UNAVAILABLE));
        r.inst.window.lock().unwrap().open(Kind::Bridge, pairing::WINDOW);
        let mut h3 = Hap::connect(r.port).await;
        h3.pair_setup(&c).await.expect("pairs again with the same code");
    }

    /// A connection ended (here: the admin removed its own pairing) answers
    /// nothing more, not even a request that came in the same packet.
    #[tokio::test]
    async fn an_ended_connection_answers_no_more_requests() {
        let r = rig("ended", Limits::default()).await;
        let c1 = SetupClient::new(&r.code(), "C1", 1);
        let mut h = Hap::connect(r.port).await;
        h.pair_setup(&c1).await.unwrap();
        let mut s1 = r.session(&c1).await;
        // A second admin, which then removes itself with a request behind.
        let c2 = SetupClient::new("", "C2", 2);
        let mut add = Tlv::new();
        add.push_u8(t::STATE, 1).push_u8(t::METHOD, 3).push(t::IDENTIFIER, b"C2").push(t::PUBLIC_KEY, &c2.ltk.public()).push_u8(t::PERMISSIONS, 1);
        let (_, b) = s1.tlv("/pairings", &add.encode()).await.unwrap();
        assert_eq!(err_of(&b), None);
        let mut s2 = r.session(&c2).await;
        let mut rm = Tlv::new();
        rm.push_u8(t::STATE, 1).push_u8(t::METHOD, 4).push(t::IDENTIFIER, b"C2");
        let rm = rm.encode();
        let mut both = format!("POST /pairings HTTP/1.1\r\nHost: x\r\nContent-Type: application/pairing+tlv8\r\nContent-Length: {}\r\n\r\n", rm.len()).into_bytes();
        both.extend_from_slice(&rm);
        both.extend_from_slice(b"GET /accessories HTTP/1.1\r\nHost: x\r\n\r\n");
        s2.send_raw(&both).await.unwrap();
        let (code, b) = s2.read_message().await.expect("the removal is answered");
        assert_eq!((code, err_of(&b)), (200, None));
        assert!(s2.read_message().await.is_none(), "the request behind it is not answered");
        // The other admin's session goes on.
        assert_eq!(s1.request("GET", "/accessories", "application/hap+json", b"").await.unwrap().0, 200);
    }

    #[tokio::test]
    async fn malformed_inputs_get_errors() {
        let r = rig("bad", fast_limits()).await;
        // Bad TLV in each pairing call.
        for path in ["/pair-setup", "/pair-verify"] {
            for body in [&[0x06u8][..], &[0x06, 0x05, 1], &[0xff; 300], &[]] {
                let mut h = Hap::connect(r.port).await;
                let (code, b) = h.tlv(path, body).await.unwrap();
                assert_eq!(code, 200);
                assert!(err_of(&b).is_some(), "{path} {body:?}");
            }
        }
        // HTTP the parser refuses, and too much of it.
        for raw in [&b"GET / HTTP/2.0\r\n\r\n"[..], b"PUT / HTTP/1.1\r\nContent-Length: 999999\r\n\r\n", b"PUT / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"] {
            let mut h = Hap::connect(r.port).await;
            h.send_raw(raw).await.unwrap();
            let (code, _) = h.read_message().await.unwrap();
            assert!(code >= 400, "{code}");
            assert!(h.closed_within(1000).await);
        }
        let mut h = Hap::connect(r.port).await;
        h.send_raw(&vec![b'A'; 20_000]).await.unwrap();
        assert!(h.closed_within(1500).await);
        // The server still works.
        let mut h = Hap::connect(r.port).await;
        assert_eq!(h.request("GET", "/accessories", "x", b"").await.unwrap().0, 470);
    }

    #[tokio::test]
    async fn repeated_m1_and_setup_slot_takeover() {
        let r = rig("slot", fast_limits()).await;
        // (Linux: the whole of 127/8 is this computer, so a second address can be used.)
        let other: std::net::IpAddr = if cfg!(target_os = "linux") { [127, 0, 0, 2].into() } else { [127, 0, 0, 1].into() };
        let mut a = Hap::connect_from(r.port, other).await;
        let (_, b) = a.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
        assert_eq!(err_of(&b), None);
        let (_, b) = a.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
        assert!(err_of(&b).is_some(), "a second M1 on one connection");
        let (_, b) = a.tlv("/pair-verify", &crate::homekit::testclient::VerifyClient::new().m1()).await.unwrap();
        assert_eq!(err_of(&b), None);
        let (_, b) = a.tlv("/pair-verify", &crate::homekit::testclient::VerifyClient::new().m1()).await.unwrap();
        assert!(err_of(&b).is_some(), "a second pair-verify M1");
        // The holder keeps its connection longer than the unverified limit.
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(!a.closed_within(50).await, "the pair-setup holder stays open");
        // No M3 within the window: another connection may take it over...
        let mut b2 = Hap::connect(r.port).await;
        let (_, b) = b2.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
        assert_eq!(err_of(&b), None, "takeover");
        // ... and once it is free, the address that lost it may start again.
        drop(b2);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut c = Hap::connect_from(r.port, other).await;
        let (_, b) = c.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
        assert_eq!(err_of(&b), None);
    }

    #[tokio::test]
    async fn stop_or_open_just_after_the_window_lapsed_still_end_its_pair_setup() {
        let r = rig("lapsed", Limits::default()).await;
        let ad = |i: &Instance| {
            let n = i.advertise.notified();
            futures::FutureExt::now_or_never(n).is_some()
        };
        for again in [false, true] {
            // A device holds the pair-setup; the window lapses before the
            // supervisor looks (every 500 ms).
            r.inst.window.lock().unwrap().open(Kind::Bridge, pairing::WINDOW);
            let mut hog = Hap::connect(r.port).await;
            let (_, b) = hog.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
            assert_eq!(err_of(&b), None);
            r.inst.window.lock().unwrap().age(pairing::WINDOW);
            ad(&r.inst);
            // The admin presses Stop, or Open again for the same accessory.
            let was = if again { r.inst.window.lock().unwrap().open(Kind::Bridge, pairing::WINDOW).1 } else { r.inst.window.lock().unwrap().close() };
            assert!(was.is_some(), "the lapsed window is handed back");
            let now = again.then_some(Kind::Bridge);
            assert_eq!(crate::homekit::window_left(was, now.as_ref(), [&r.inst].into_iter()), Some(Kind::Bridge));
            // Its pair-setup ended, its connection closed, announced again.
            assert!(r.inst.pairing_now().is_none());
            assert!(hog.closed_within(500).await, "its connection is closed");
            assert!(ad(&r.inst), "announced again");
            assert_eq!(r.inst.pairable(), again);
        }
        // A window still open for the same accessory is left alone.
        let mut hog = Hap::connect(r.port).await;
        let (_, b) = hog.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
        assert_eq!(err_of(&b), None);
        let was = r.inst.window.lock().unwrap().open(Kind::Bridge, pairing::WINDOW).1;
        assert_eq!(crate::homekit::window_left(was, Some(&Kind::Bridge), [&r.inst].into_iter()), None);
        assert!(r.inst.pairing_now().is_some());
    }

    #[tokio::test]
    async fn stop_and_start_again_ends_the_exchange_and_closes_its_connection() {
        let r = rig("restart", fast_limits()).await;
        let other: std::net::IpAddr = if cfg!(target_os = "linux") { [127, 0, 0, 2].into() } else { [127, 0, 0, 1].into() };
        // A device holds the pair-setup.
        let mut hog = Hap::connect_from(r.port, other).await;
        let (_, b) = hog.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
        assert_eq!(err_of(&b), None);
        assert_eq!(r.inst.pairing_now().map(|p| p.0), Some(other));
        // Only the exchange from the address shown is ended.
        if cfg!(target_os = "linux") {
            assert_eq!(r.inst.end_setup(Some([127, 0, 0, 1].into())), None);
            assert!(r.inst.pairing_now().is_some());
        }
        // The admin stops it: the exchange ends and its connection closes.
        r.inst.window.lock().unwrap().restart(other, pairing::WINDOW).unwrap();
        assert_eq!(r.inst.end_setup(Some(other)), Some(other));
        assert!(r.inst.pairing_now().is_none());
        assert!(hog.closed_within(500).await, "its connection is closed");
        assert!(r.inst.pairable());
        if cfg!(target_os = "linux") {
            // It may not start again; the owner (another address) pairs.
            let mut again = Hap::connect_from(r.port, other).await;
            let (_, b) = again.tlv("/pair-setup", &SetupClient::m1()).await.unwrap();
            assert_eq!(err_of(&b), Some(err::UNAVAILABLE));
            let mut owner = Hap::connect(r.port).await;
            owner.pair_setup(&SetupClient::new(&r.code(), "Owner", 1)).await.expect("the owner pairs");
            assert!(!r.inst.pairable() && r.inst.window.lock().unwrap().current().is_none());
        }
    }

    #[tokio::test]
    async fn caps_keep_sessions_and_evict_the_oldest_unverified() {
        let r = rig("caps", Limits { max_unverified: 4, max_unverified_per_ip: 3, ..fast_limits() }).await;
        let c = SetupClient::new(&r.code(), "C1", 1);
        let mut h = Hap::connect(r.port).await;
        h.pair_setup(&c).await.unwrap();
        drop(h);
        let mut s = r.session(&c).await;
        // Fill the unverified room (3 per address here: all are 127.0.0.1).
        let mut u: Vec<Hap> = Vec::new();
        for _ in 0..3 {
            u.push(Hap::connect(r.port).await);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut extra = Hap::connect(r.port).await;
        assert!(extra.closed_within(300).await, "per-address cap");
        // The verified session is untouched.
        assert_eq!(s.request("GET", "/accessories", "x", b"").await.unwrap().0, 200);
        // With the total cap reached, the oldest unverified gives way.
        let lim = Limits { max_unverified: 2, max_unverified_per_ip: 5, ..fast_limits() };
        let r2 = rig("caps2", lim).await;
        let mut first = Hap::connect(r2.port).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _second = Hap::connect(r2.port).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut third = Hap::connect(r2.port).await;
        assert!(first.closed_within(300).await, "the oldest unverified was closed");
        assert_eq!(third.request("GET", "/accessories", "x", b"").await.unwrap().0, 470);
        drop(u);
    }

    #[test]
    fn events_merge_and_their_backlog_is_capped() {
        let e = Events::default();
        // The same characteristic again: the latest value replaces the one waiting.
        for v in 0..1000u64 {
            assert!(e.put([((2, 20), json!(v % 2 == 0))]));
        }
        assert!(e.put([((2, 18), json!(1))]));
        assert!(e.put([((2, 18), json!(0))]));
        let w = e.take();
        assert_eq!(w.len(), 2);
        assert_eq!((w[&(2, 20)].clone(), w[&(2, 18)].clone()), (json!(false), json!(0)));
        assert!(!e.is_waiting());
        // Never more than the cap waiting, however many characteristics.
        for i in 0..(EVENT_BACKLOG as u64 * 4) {
            e.put([((3, i), json!(i))]);
        }
        assert_eq!(e.take().len(), EVENT_BACKLOG);
        // Turned off: what was waiting for it is not sent.
        e.put([((2, 20), json!(true)), ((2, 18), json!(1))]);
        e.forget(&(2, 20));
        assert_eq!(e.take().keys().copied().collect::<Vec<_>>(), vec![(2, 18)]);
    }

    #[test]
    fn the_event_bucket_allows_a_burst_then_one_per_interval() {
        let t0 = Instant::now();
        let every = Duration::from_millis(100);
        let mut b = Bucket::new(3, every, t0);
        for _ in 0..3 {
            assert_eq!(b.ready_at(t0), t0);
            b.take(t0);
        }
        assert_eq!(b.ready_at(t0), t0 + every);
        // Over 1 s from empty: 10 more, no matter how often it is asked.
        let mut sent = 0;
        let mut now = t0;
        while now < t0 + Duration::from_secs(1) {
            if b.ready_at(now) <= now {
                b.take(now);
                sent += 1;
            }
            now += Duration::from_millis(1);
        }
        assert!((9..=10).contains(&sent), "{sent}");
        // After a long quiet time it is full again, never more.
        let later = now + Duration::from_secs(60);
        for _ in 0..3 {
            assert_eq!(b.ready_at(later), later);
            b.take(later);
        }
        assert!(b.ready_at(later) > later);
    }

    /// A paired guest changing a value as fast as it can does not make the
    /// server send events as fast: a subscribed session gets at most the
    /// burst, then one message per interval, each with the latest value of
    /// each characteristic once, and the last value it gets is the last one
    /// written.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_guest_flooding_writes_cannot_flood_events() {
        let (burst, every) = (3u32, Duration::from_millis(200));
        let r = rig("evflood", Limits { max_verified: 4, event_burst: burst, event_every: every, ..fast_limits() }).await;
        let admin = SetupClient::new(&r.code(), "C1", 1);
        let mut h = Hap::connect(r.port).await;
        h.pair_setup(&admin).await.unwrap();
        drop(h);
        let mut a = r.session(&admin).await;
        let guest = SetupClient::new("", "G1", 2);
        let mut add = Tlv::new();
        add.push_u8(t::STATE, 1).push_u8(t::METHOD, 3).push(t::IDENTIFIER, b"G1").push(t::PUBLIC_KEY, &guest.ltk.public()).push_u8(t::PERMISSIONS, 0);
        assert_eq!(err_of(&a.tlv("/pairings", &add.encode()).await.unwrap().1), None);
        let sub = br#"{"characteristics":[{"aid":2,"iid":20,"ev":true},{"aid":2,"iid":18,"ev":true}]}"#;
        assert_eq!(a.request("PUT", "/characteristics", "application/hap+json", sub).await.unwrap().0, 204);
        // The admin's session reads every event until 1 s of quiet.
        let reader = tokio::spawn(async move {
            let mut got: Vec<Value> = Vec::new();
            while let Ok(Some((_, body))) = tokio::time::timeout(Duration::from_secs(1), a.read_message()).await {
                got.push(serde_json::from_slice(&body).unwrap());
            }
            got
        });
        let mut g = r.session(&guest).await;
        // Each request toggles Mute 128 times; 2.5 s of them.
        let flip: Vec<Value> = (0..128).map(|i| json!({ "aid": 2, "iid": 20, "value": i % 2 == 0 })).collect();
        let body = serde_json::to_vec(&json!({ "characteristics": flip })).unwrap();
        let start = Instant::now();
        let mut writes = 0u64;
        while start.elapsed() < Duration::from_millis(2500) {
            assert_eq!(g.request("PUT", "/characteristics", "application/hap+json", &body).await.unwrap().0, 204);
            writes += 128;
        }
        // The last write: Mute on, Active off.
        let last = br#"{"characteristics":[{"aid":2,"iid":20,"value":true},{"aid":2,"iid":18,"value":0}]}"#;
        assert_eq!(g.request("PUT", "/characteristics", "application/hap+json", last).await.unwrap().0, 204);
        let flood_end = start.elapsed();
        let got = tokio::time::timeout(Duration::from_secs(30), reader).await.unwrap().unwrap();
        // (Up to the last write, and the events still due after it.)
        let allowed = burst as u64 + (flood_end + every).as_millis() as u64 / every.as_millis() as u64 + 1;
        assert!(!got.is_empty() && (got.len() as u64) <= allowed, "{} event messages for {writes} changes in {flood_end:?} (at most {allowed})", got.len());
        assert!(writes > 100 * allowed, "the guest wrote fast enough to show the bound ({writes} changes)");
        // Each message names each characteristic once, and the latest values win.
        for m in &got {
            let items = m["characteristics"].as_array().unwrap();
            let mut keys: Vec<(u64, u64)> = items.iter().map(|c| (c["aid"].as_u64().unwrap(), c["iid"].as_u64().unwrap())).collect();
            let n = keys.len();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), n, "{m}");
        }
        let latest = |iid: u64| got.iter().rev().flat_map(|m| m["characteristics"].as_array().unwrap().clone()).find(|c| c["iid"] == iid).map(|c| c["value"].clone());
        assert_eq!((latest(20), latest(18)), (Some(json!(true)), Some(json!(0))));
    }

    /// A connection's clean-up works with poisoned locks (it may run while
    /// unwinding: a second panic there would abort the program).
    #[test]
    fn connection_cleanup_survives_poisoned_locks() {
        let dir = std::env::temp_dir().join(format!("as-hk-poison-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = Arc::new(Mutex::new(State::load_or_create(&dir).unwrap()));
        let inst = Arc::new(Instance::new(Kind::Bridge, "T".into(), 0, st, Limits::default()));
        assert!(inst.admit(7, IpAddr::from([192, 0, 2, 1]), CancellationToken::new()));
        let i = inst.clone();
        let _ = std::thread::spawn(move || {
            let _s = i.setup.lock().unwrap();
            let _v = i.sessions.lock().unwrap();
            let _g = i.gate.list.lock().unwrap();
            panic!("poison them");
        })
        .join();
        inst.streams.poison();
        assert!(inst.setup.is_poisoned() && inst.sessions.is_poisoned());
        drop(ConnGuard { inst: inst.clone(), id: 7 });
        assert!(inst.gate.list.lock().unwrap_or_else(|p| p.into_inner()).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
