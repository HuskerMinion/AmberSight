//! Sign-in: password hashing, sessions, brute-force lockout.
//!
//! - User passwords in config.toml are stored as Argon2 hashes; plain-text
//!   passwords found at startup are hashed and saved back.
//! - The web page signs in with `POST /api/login` and gets an HttpOnly
//!   session cookie (works for video, images and downloads too).
//! - HTTP Basic auth still works for scripts; verified results are cached
//!   for a few minutes because Argon2 is deliberately slow.
//! - Too many failed sign-ins from one address lock it out for a while.
//! - An account with two-step codes (twostep.rs) signs in with a password and
//!   a code; its password alone opens nothing else (HTTP Basic, RTSP, linked
//!   servers). Apps and other programs use a device token instead.

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SESSION_COOKIE: &str = "as_session";
pub const SESSION_DAYS: i64 = 30;
const BASIC_CACHE: Duration = Duration::from_secs(300);
const FAIL_WINDOW: Duration = Duration::from_secs(600);
const MAX_FAILS: usize = 5;
const LOCKOUT: Duration = Duration::from_secs(600);
/// How long a password stays good for typing the code that goes with it.
pub const TICKET_SECS: i64 = 300;
/// Right passwords that lead to a ticket, per account and address: an address
/// may be shared (behind a reverse proxy or NAT, all clients are one address),
/// so someone with one viewer's password does not hold up anyone else.
const MAX_TICKETS_PER_MINUTE: usize = 10;
/// Tries at a code for one account from one address, before that address's
/// codes for it are refused for `CODE_LOCK`. Keyed on both, so that someone
/// who has the password cannot keep the owner out from somewhere else.
const MAX_CODE_TRIES: usize = 5;
const CODE_LOCK: Duration = Duration::from_secs(600);
/// And for the account from all addresses together: a guesser with many
/// addresses gets this few tries an hour out of a million codes.
const MAX_ACCOUNT_CODE_TRIES: usize = 30;
const ACCOUNT_CODE_WINDOW: Duration = Duration::from_secs(3600);
const ACCOUNT_CODE_LOCK: Duration = Duration::from_secs(3600);
/// Tries at a code from a session that exists (the sign-in locks do not apply there).
const MAX_SESSION_CODE_TRIES: usize = 5;
const MAX_SESSION_CODE_TRIES_DAY: usize = 10;
const DAY: Duration = Duration::from_secs(86400);
/// Tries at a recovery code per account and address: past all locks. Per
/// address, so nobody can use them up for the owner; the processor is protected
/// by `MAX_RECOVERY_CHECKS` instead. (60 random bits cannot be guessed at any rate.)
const MAX_RECOVERY_TRIES: usize = 10;
/// Tries from all addresses together that count as a burst worth telling admins of.
const RECOVERY_BURST: usize = 20;
/// Recovery checks running at one time (each can take ten Argon2 runs). Others wait their turn.
const MAX_RECOVERY_CHECKS: usize = 2;
/// How long a recovery check waits for its turn.
const RECOVERY_WAIT: Duration = Duration::from_secs(20);
/// Recovery checks of one account from one address, running or waiting: the
/// queue stays short whoever fills it, so the owner's turn comes soon.
const MAX_RECOVERY_WAITING: usize = 2;
/// Wrong passwords and codes from one session before it is ended.
const MAX_PROOF_TRIES: usize = 5;
/// Warnings remembered at one time (past this they simply are not held back).
const MAX_NOTES: usize = 10_000;
/// Password checks (Argon2, about 19 MiB each) running at one time, whoever asks.
/// Others wait their turn, first come first served.
const MAX_PASSWORD_CHECKS: usize = 4;
/// Checks running or waiting. Past this they are told to come back: the line stays
/// short enough that one who gets in is soon next.
const MAX_PASSWORD_WAITING: usize = 32;
/// How long a check waits for its turn.
const PASSWORD_WAIT: Duration = Duration::from_secs(15);
/// Device tokens start with this, so they can be recognised (in a leak, or in
/// a password field).
pub const TOKEN_PREFIX: &str = "ast_";
pub const MAX_TOKENS: usize = 20;
/// "Last used" is written at most this often, not on every request.
const TOKEN_TOUCH_SECS: i64 = 3600;

#[derive(Clone, Debug, PartialEq)]
pub struct Identity {
    pub user: String,
    pub admin: bool,
}

pub fn is_hashed(p: &str) -> bool {
    p.starts_with("$argon2")
}

pub fn hash_password(plain: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .unwrap_or_default()
}

/// Check a password against a stored value (Argon2 hash, or legacy plain
/// text compared in constant time).
pub fn verify_password(plain: &str, stored: &str) -> bool {
    if is_hashed(stored) {
        match PasswordHash::new(stored) {
            Ok(h) => Argon2::default().verify_password(plain.as_bytes(), &h).is_ok(),
            Err(_) => false,
        }
    } else {
        use subtle::ConstantTimeEq;
        !stored.is_empty() && bool::from(plain.as_bytes().ct_eq(stored.as_bytes()))
    }
}

/// Hash every plain-text user password in the config. Returns true when
/// something changed (the caller saves config.toml).
pub fn hash_config_passwords(cfg: &mut crate::config::Config) -> bool {
    let mut changed = false;
    if let Some(a) = cfg.server.auth.as_mut() {
        if !a.password.is_empty() && !is_hashed(&a.password) {
            a.password = hash_password(a.password.trim());
            changed = true;
        }
    }
    for u in cfg.server.users.iter_mut() {
        if !u.password.is_empty() && !is_hashed(&u.password) {
            u.password = hash_password(u.password.trim());
            changed = true;
        }
    }
    changed
}

/// The accounts that may sign in: (username, stored password, admin).
/// Disabled accounts are left out, so every way in (password, HTTP Basic,
/// sessions, sign-in codes, RTSP, linked servers, notifications, presence)
/// treats them as if they did not exist.
///
/// An empty list means the server has no accounts at all and is open. So when
/// accounts exist but all are disabled (the API refuses that, but the config
/// file can say so), the list holds one account nobody can sign in to: the
/// server stays locked, it does not fall open.
pub fn accounts(cfg: &crate::config::Config) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    if let Some(a) = cfg.server.auth.as_ref() {
        out.push((a.username.trim().to_string(), a.password.trim().to_string(), true));
    }
    // (An old-style account with the name of a disabled one is disabled too.)
    out.retain(|(n, _, _)| !cfg.server.users.iter().any(|u| u.disabled && u.username.trim() == n));
    for u in cfg.server.users.iter().filter(|u| !u.disabled) {
        out.push((u.username.trim().to_string(), u.password.trim().to_string(), u.admin));
    }
    if out.is_empty() && !cfg.server.users.is_empty() {
        out.push((LOCKED_ACCOUNT.to_string(), LOCKED_HASH.to_string(), false));
    }
    out
}

/// See `accounts`. (User names cannot start with "(", and this is not a valid hash, so no password matches.)
const LOCKED_ACCOUNT: &str = "(locked)";
const LOCKED_HASH: &str = "$argon2id$locked";

/// Whether something opened by `user` may keep running: an account that was
/// disabled or deleted since, or an open server that has accounts now, ends it.
/// `generation` is the account's `generation` when it started: any change to
/// the account since (even disabled and enabled again) ends it too.
pub fn still_allowed(cfg: &crate::config::Config, user: &str, generation_then: u64) -> bool {
    let acc = accounts(cfg);
    if user == "(open)" {
        return acc.is_empty();
    }
    generation(user) == generation_then && acc.iter().any(|(u, _, _)| u == user)
}

/// Goes up each time an account changes (see `forget_user`), so something
/// that began before the change can tell.
pub fn generation(user: &str) -> u64 {
    state().lock().unwrap().generations.get(user).copied().unwrap_or(0)
}

/// Camera slugs a non-admin user is limited to (None = all cameras).
pub fn camera_limits(cfg: &crate::config::Config, user: &str) -> Option<std::collections::HashSet<String>> {
    let u = cfg.server.users.iter().find(|u| u.username.trim() == user)?;
    if u.admin || u.cameras.is_empty() {
        return None;
    }
    Some(u.cameras.iter().map(|c| crate::config::slug(c)).collect())
}

pub fn check_login(accounts: &[(String, String, bool)], user: &str, pass: &str) -> Option<Identity> {
    match accounts.iter().find(|(u, _, _)| u == user) {
        Some((u, p, admin)) => verify_password(pass, p).then(|| Identity { user: u.clone(), admin: *admin }),
        None => {
            // Same work as a wrong password, so timing does not tell which
            // user names exist.
            static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            let _ = verify_password(pass, DUMMY.get_or_init(|| hash_password("not-a-real-account")));
            None
        }
    }
}

struct Session {
    id: Identity,
    /// Unix seconds.
    expires: i64,
    meta: SessionMeta,
}

/// What the list of signed-in devices shows about a session. A session from
/// before the list existed has `created` 0 and no label.
#[derive(Clone, Default)]
pub struct SessionMeta {
    pub created: i64,
    /// The address it was last used from.
    pub ip: Option<IpAddr>,
    /// A fixed label such as "Edge on Windows", never the raw User-Agent.
    pub label: String,
    /// The hash of the browser cookie it was made from (see devices.rs).
    pub device: String,
    pub last_seen: i64,
    /// When `last_seen` was last written to the database.
    saved_seen: i64,
}

impl SessionMeta {
    /// For a session being made now (see `new_session_with`).
    pub fn new(ip: IpAddr, label: String, device: String) -> SessionMeta {
        SessionMeta { ip: Some(ip), label, device, ..Default::default() }
    }

    /// Note a use of the session. True when the database should be told too
    /// (not more often than every `SEEN_SAVE_SECS`).
    fn note_use(&mut self, ip: IpAddr, now: i64) -> bool {
        self.ip = Some(ip);
        self.last_seen = now;
        let due = now - self.saved_seen >= SEEN_SAVE_SECS;
        if due {
            self.saved_seen = now;
        }
        due
    }
}

/// A stored session as kept in memory. Those made before the list of devices
/// existed have no creation time, label or address.
fn session_from_row(r: crate::db::StoredSession) -> (String, Session) {
    let meta = SessionMeta { created: r.created, ip: r.ip.parse().ok(), label: r.label, device: r.device, last_seen: r.last_seen, saved_seen: r.last_seen };
    (r.hash, Session { id: Identity { user: r.user, admin: r.admin }, expires: r.expires, meta })
}

/// How often a session's last use is written to the database: never per request.
pub const SEEN_SAVE_SECS: i64 = 300;

/// What `GET /api/me/account` lists for each session of the signed-in account.
pub struct SessionInfo {
    pub id: String,
    pub meta: SessionMeta,
    pub this: bool,
}

/// Names a session in the list and in sign-out requests, without being the
/// token or its stored hash (neither can be worked out from it).
fn session_id(hash: &str) -> String {
    let d = Sha256::digest(format!("session-id:{hash}").as_bytes());
    format!("ses-{}", d.iter().take(8).map(|x| format!("{x:02x}")).collect::<String>())
}

/// What is signed in a ticket (see `make_ticket`).
struct Ticket {
    user: String,
    generation: u64,
    expires: i64,
    recovery_only: bool,
    nonce: String,
}

/// The right password was given and a code is being tried with this ticket. Only
/// one try at a time can hold a ticket; it can be tried again when this is dropped,
/// unless `burn` used it up.
pub struct TicketHold {
    pub user: String,
    pub generation: u64,
    /// Six-digit codes were locked when it was given: only a recovery code works on it.
    pub recovery_only: bool,
    nonce: String,
    expires: i64,
    burned: bool,
}

impl TicketHold {
    /// Used: it never works again.
    pub fn burn(mut self) {
        self.burned = true;
        let mut s = state().lock().unwrap();
        s.spent_tickets.insert(self.nonce.clone(), self.expires);
        s.tickets_trying.remove(&self.nonce);
    }
}

impl Drop for TicketHold {
    fn drop(&mut self) {
        if !self.burned {
            state().lock().unwrap().tickets_trying.remove(&self.nonce);
        }
    }
}

/// What a ticket turned out to be.
pub enum TicketTry {
    Held(TicketHold),
    /// Another try with it is running right now (a form sent twice).
    Busy,
    /// One this server gave out: used, run out, or the account changed since. Late, not guessing.
    Late,
    /// Not one this server gave out (since it started): guessing.
    MadeUp,
}

#[derive(Clone)]
struct Token {
    id: String,
    user: String,
    name: String,
    admin: bool,
    created_at: i64,
    last_used: Option<i64>,
}

/// A device token for a list: never its value.
#[derive(Clone, serde::Serialize)]
pub struct TokenInfo {
    pub id: String,
    pub name: String,
    /// "viewer" or "admin": never more than the account may do now.
    pub scope: &'static str,
    pub created: i64,
    pub last_used: Option<i64>,
}

impl From<&Token> for TokenInfo {
    fn from(t: &Token) -> Self {
        TokenInfo { id: t.id.clone(), name: t.name.clone(), scope: if t.admin { "admin" } else { "viewer" }, created: t.created_at, last_used: t.last_used }
    }
}

/// Which limit a wrong code just reached.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CodeLock {
    Address,
    Account,
}

/// Why a token was not made.
#[derive(Debug, PartialEq)]
pub enum AddTokenError {
    TooMany,
    NotSaved,
}

/// What an `Authorization` header amounts to.
#[derive(Debug, PartialEq)]
pub enum Cred {
    /// A password, with the account's generation from before it was checked:
    /// a change during the slow check still ends what this request starts.
    Password(Identity, u64),
    /// A device token, with the account's generation read together with it:
    /// a token revoked a moment later still ends what this request starts.
    Token(Identity, u64),
    /// The right password, for an account that needs a code with it.
    TwoStepNeeded,
    Wrong,
}

/// What to tell someone using the password of a two-step account where only a
/// device token will do.
pub const USE_A_TOKEN: &str = "this account uses two-step codes, so its password does not work here: use a device token (Your account, Device tokens)";

/// How long a phone sign-in QR code stays valid.
pub const PAIR_SECS: i64 = 600;

#[derive(Default)]
struct State {
    /// How many times each account has changed (see `generation`).
    generations: HashMap<String, u64>,
    /// Keyed by sha256(token): the token itself is only ever in the cookie.
    sessions: HashMap<String, Session>,
    basic: HashMap<[u8; 32], (Identity, Instant)>,
    fails: HashMap<IpAddr, Vec<Instant>>,
    locked: HashMap<IpAddr, Instant>,
    /// One-time sign-in codes (sha256 → user, expiry).
    /// code hash -> (user, expires, may be admin, came from a linked server)
    pairs: HashMap<String, (String, i64, bool, bool)>,
    /// Passwords that were right for an account that also needs a code:
    /// remembered so old scripts do not run Argon2 on every request.
    basic_refused: HashMap<[u8; 32], (String, Instant)>,
    /// Accounts seen using a password in HTTP Basic, from which network, since
    /// the server started (so the database is asked once, not per request).
    basic_nets: std::collections::HashSet<(String, IpAddr)>,
    /// Tickets are signed and nothing is kept for them when given, so nothing can
    /// fill up or be pushed out. Kept: those used up, by nonce, until they would
    /// have expired (so as many as real codes were checked right)...
    spent_tickets: HashMap<String, i64>,
    /// ...and those being tried right now.
    tickets_trying: std::collections::HashSet<String>,
    /// Tries at a code per account and address, and for the account as a whole.
    code_tries: HashMap<(String, IpAddr), Vec<Instant>>,
    code_locks: HashMap<(String, IpAddr), Instant>,
    account_tries: HashMap<String, Vec<Instant>>,
    account_locks: HashMap<String, Instant>,
    /// Tries at a six-digit code per session (keyed by sha256 of the session).
    session_code_tries: HashMap<String, Vec<Instant>>,
    recovery_tries: HashMap<(String, IpAddr), Vec<Instant>>,
    recovery_all: HashMap<String, Vec<Instant>>,
    /// Recovery checks running or waiting, per account and address.
    recovery_waiting: HashMap<(String, IpAddr), usize>,
    /// Tickets given per account and address in the last minute.
    ticket_rate: HashMap<(String, IpAddr), Vec<Instant>>,
    /// Wrong passwords and codes per account and session.
    proof_tries: HashMap<(String, String), Vec<Instant>>,
    /// Until when a warning about something waits (see `note_due`).
    notes: HashMap<String, Instant>,
    /// Keyed by sha256(token): the token itself is only ever with the device.
    tokens: HashMap<String, Token>,
    /// Accounts that count as having two-step on in tests (no database there).
    #[cfg(test)]
    two_step_test: std::collections::HashSet<String>,
    /// Where sessions are kept across restarts.
    store: Option<std::sync::Arc<crate::db::Db>>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|x| format!("{x:02x}")).collect()
}

fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Keep sessions in the database so a restart does not sign everyone out.
/// Loads the sessions that are still valid.
pub fn attach_store(db: std::sync::Arc<crate::db::Db>) {
    let now = now_secs();
    let _ = db.delete_expired_sessions(now);
    let loaded = db.load_sessions().unwrap_or_default();
    let mut s = state().lock().unwrap();
    for r in loaded {
        let (hash, session) = session_from_row(r);
        s.sessions.insert(hash, session);
    }
    match db.token_all() {
        Ok(rows) => {
            for t in rows {
                s.tokens.insert(t.hash, Token { id: t.id, user: t.user, name: t.name, admin: t.admin, created_at: t.created_at, last_used: t.last_used });
            }
        }
        Err(e) => tracing::warn!("could not load device tokens: {e:#}"),
    }
    // A secret someone started setting up and left. Nothing else is ever removed
    // here: rows of an account missing from the config file give no access, and
    // a config file restored or edited by mistake must not cost anyone their codes.
    let _ = db.two_step_drop_waiting(now - 86400);
    s.store = Some(db);
}

fn store() -> Option<std::sync::Arc<crate::db::Db>> {
    state().lock().unwrap().store.clone()
}

/// Whether signing in to this account needs a code. Err: cannot tell, and
/// callers then refuse. Read from the database each time, so that the command
/// line can reset it (`ambersight user reset-two-step`) while the server runs.
pub fn two_step_on(user: &str) -> Result<bool, ()> {
    #[cfg(test)]
    if state().lock().unwrap().two_step_test.contains(user) {
        return Ok(true);
    }
    match store() {
        Some(db) => db.two_step_enabled(user).map_err(|e| tracing::warn!("two-step state unreadable: {e:#}")),
        None if cfg!(test) => Ok(false),
        None => Err(()),
    }
}

/// Whether a right password from this address may get a ticket for this account
/// now: a few a minute. False: refused, and nobody else is touched.
pub fn ticket_allowed(user: &str, ip: IpAddr) -> bool {
    let (key, now) = ((user.to_string(), net_key(ip)), Instant::now());
    let mut s = state().lock().unwrap();
    if s.ticket_rate.len() > 10_000 {
        s.ticket_rate.retain(|_, l| l.iter().any(|t| now.duration_since(*t) < Duration::from_secs(60)));
    }
    let list = s.ticket_rate.entry(key).or_default();
    list.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
    if list.len() >= MAX_TICKETS_PER_MINUTE {
        return false;
    }
    list.push(now);
    true
}

/// The key tickets are signed with, made at start: a restart ends open tickets.
fn ticket_key() -> &'static ring::hmac::Key {
    static KEY: OnceLock<ring::hmac::Key> = OnceLock::new();
    KEY.get_or_init(|| {
        use rand::RngCore;
        let mut b = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut b);
        ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &b)
    })
}

/// `<fields, base64>.<signature, base64>`. Signed as the text it is sent as, so
/// that one ticket has only one way of being written.
fn make_ticket(t: &Ticket) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let fields = b64.encode(format!("{}.{}.{}.{}.{}", t.expires, t.generation, u8::from(t.recovery_only), t.nonce, t.user));
    let sig = ring::hmac::sign(ticket_key(), fields.as_bytes());
    format!("{fields}.{}", b64.encode(sig.as_ref()))
}

/// The fields of a ticket this server signed. (ring compares in constant time.)
fn read_ticket(ticket: &str) -> Option<Ticket> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (fields, sig) = ticket.rsplit_once('.')?;
    ring::hmac::verify(ticket_key(), fields.as_bytes(), &b64.decode(sig).ok()?).ok()?;
    let text = String::from_utf8(b64.decode(fields).ok()?).ok()?;
    // (Last: the user name may hold anything.)
    let mut part = text.splitn(5, '.');
    Some(Ticket {
        expires: part.next()?.parse().ok()?,
        generation: part.next()?.parse().ok()?,
        recovery_only: part.next()? == "1",
        nonce: part.next()?.to_string(),
        user: part.next()?.to_string(),
    })
}

/// A ticket for the code that goes with a right password: none if the account
/// changed while the password was being checked. Nothing is stored.
pub fn new_ticket(user: &str, generation_then: u64, recovery_only: bool) -> Option<String> {
    if generation(user) != generation_then {
        return None;
    }
    Some(make_ticket(&Ticket { user: user.to_string(), generation: generation_then, expires: now_secs() + TICKET_SECS, recovery_only, nonce: random_token() }))
}

/// Hold a ticket for one try.
pub fn take_ticket(ticket: &str) -> TicketTry {
    let Some(t) = read_ticket(ticket) else { return TicketTry::MadeUp };
    let now = now_secs();
    let mut s = state().lock().unwrap();
    s.spent_tickets.retain(|_, until| *until > now);
    if t.expires <= now || s.spent_tickets.contains_key(&t.nonce) || s.generations.get(&t.user).copied().unwrap_or(0) != t.generation {
        return TicketTry::Late;
    }
    if !s.tickets_trying.insert(t.nonce.clone()) {
        return TicketTry::Busy;
    }
    TicketTry::Held(TicketHold { user: t.user, generation: t.generation, recovery_only: t.recovery_only, nonce: t.nonce, expires: t.expires, burned: false })
}

/// The address tries are counted under: IPv6 by its /64, which is one
/// household or one customer (who has all of it to move around in), and an
/// IPv4 address written as IPv6 as itself.
pub fn net_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = a.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
        v4 => v4,
    }
}

impl State {
    fn code_blocked_now(&mut self, user: &str, ip: IpAddr) -> bool {
        let now = Instant::now();
        let key = (user.to_string(), net_key(ip));
        if self.code_locks.get(&key).is_some_and(|until| *until <= now) {
            self.code_locks.remove(&key);
        }
        if self.account_locks.get(user).is_some_and(|until| *until <= now) {
            self.account_locks.remove(user);
        }
        self.code_locks.contains_key(&key) || self.account_locks.contains_key(user)
    }
}

/// True while codes for this account from this address are refused.
pub fn code_blocked(user: &str, ip: IpAddr) -> bool {
    state().lock().unwrap().code_blocked_now(user, ip)
}

/// Count a try at a code, before checking it: false if none may be made now.
/// Being counted first, requests sent in parallel get no more than the limit.
pub fn code_try(user: &str, ip: IpAddr) -> bool {
    let now = Instant::now();
    let mut s = state().lock().unwrap();
    if s.code_blocked_now(user, ip) {
        return false;
    }
    // Many addresses (IPv6) must not grow these maps without end.
    if s.code_tries.len() > 10_000 {
        s.code_tries.retain(|_, l| l.iter().any(|t| now.duration_since(*t) < CODE_LOCK));
        s.code_locks.retain(|_, until| *until > now);
    }
    let list = s.code_tries.entry((user.to_string(), net_key(ip))).or_default();
    list.retain(|t| now.duration_since(*t) < CODE_LOCK);
    if list.len() >= MAX_CODE_TRIES {
        return false;
    }
    list.push(now);
    let all = s.account_tries.entry(user.to_string()).or_default();
    all.retain(|t| now.duration_since(*t) < ACCOUNT_CODE_WINDOW);
    if all.len() >= MAX_ACCOUNT_CODE_TRIES {
        return false;
    }
    all.push(now);
    true
}

/// A counted try turned out wrong: which limit it reached, if it reached one
/// now. Codes are then refused for a while (from this address, or from all).
pub fn code_wrong(user: &str, ip: IpAddr) -> Option<CodeLock> {
    let now = Instant::now();
    let mut s = state().lock().unwrap();
    let key = (user.to_string(), net_key(ip));
    if s.account_tries.get(user).is_some_and(|l| l.len() >= MAX_ACCOUNT_CODE_TRIES) && !s.account_locks.contains_key(user) {
        s.account_locks.insert(user.to_string(), now + ACCOUNT_CODE_LOCK);
        return Some(CodeLock::Account);
    }
    if s.code_tries.get(&key).is_some_and(|l| l.len() >= MAX_CODE_TRIES) && !s.code_locks.contains_key(&key) {
        s.code_locks.insert(key, now + CODE_LOCK);
        return Some(CodeLock::Address);
    }
    None
}

/// A counted try was no wrong guess (it was right, or could not be checked):
/// this address starts again, and the try does not count for the account.
pub fn code_ok(user: &str, ip: IpAddr) {
    let mut s = state().lock().unwrap();
    s.code_tries.remove(&(user.to_string(), net_key(ip)));
    if let Some(all) = s.account_tries.get_mut(user) {
        all.pop();
    }
}

/// The password changed or the codes were reset: whoever kept trying no longer
/// has what they had, so nobody has to wait any more.
pub fn code_clear(user: &str) {
    let mut s = state().lock().unwrap();
    s.code_tries.retain(|(u, _), _| u != user);
    s.code_locks.retain(|(u, _), _| u != user);
    s.account_tries.remove(user);
    s.account_locks.remove(user);
}

/// Whether six-digit codes for this account are locked anywhere: someone has its password.
pub fn code_locked_any(user: &str) -> bool {
    let now = Instant::now();
    let s = state().lock().unwrap();
    s.account_locks.get(user).is_some_and(|t| *t > now) || s.code_locks.iter().any(|((u, _), t)| u == user && *t > now)
}

/// Count a try at a six-digit code from this session, before checking it: false
/// if none may be made now.
pub fn session_code_try(session: &str) -> bool {
    let now = Instant::now();
    let mut s = state().lock().unwrap();
    if s.session_code_tries.len() > 10_000 {
        s.session_code_tries.retain(|_, l| l.iter().any(|t| now.duration_since(*t) < DAY));
    }
    let list = s.session_code_tries.entry(token_hash(session)).or_default();
    list.retain(|t| now.duration_since(*t) < DAY);
    let recent = list.iter().filter(|t| now.duration_since(**t) < CODE_LOCK).count();
    if recent >= MAX_SESSION_CODE_TRIES || list.len() >= MAX_SESSION_CODE_TRIES_DAY {
        return false;
    }
    list.push(now);
    true
}

/// A counted try was no wrong guess: it does not count.
pub fn session_code_ok(session: &str) {
    if let Some(l) = state().lock().unwrap().session_code_tries.get_mut(&token_hash(session)) {
        l.pop();
    }
}

/// Count a try at a recovery code from this address: false if none may be made now.
pub fn recovery_try(user: &str, ip: IpAddr) -> bool {
    let now = Instant::now();
    let mut s = state().lock().unwrap();
    if s.recovery_tries.len() > 10_000 {
        s.recovery_tries.retain(|_, l| l.iter().any(|t| now.duration_since(*t) < LOCKOUT));
    }
    let all = s.recovery_all.entry(user.to_string()).or_default();
    all.retain(|t| now.duration_since(*t) < LOCKOUT);
    // (All that is asked of it is whether a burst was reached.)
    if all.len() < RECOVERY_BURST {
        all.push(now);
    }
    let list = s.recovery_tries.entry((user.to_string(), net_key(ip))).or_default();
    list.retain(|t| now.duration_since(*t) < LOCKOUT);
    if list.len() >= MAX_RECOVERY_TRIES {
        return false;
    }
    list.push(now);
    true
}

/// A counted try at a recovery code that was never checked: it does not count.
pub fn recovery_refund(user: &str, ip: IpAddr) {
    let mut s = state().lock().unwrap();
    if let Some(l) = s.recovery_tries.get_mut(&(user.to_string(), net_key(ip))) {
        l.pop();
    }
    if let Some(l) = s.recovery_all.get_mut(user) {
        l.pop();
    }
}

/// Whether so many recovery codes were tried for this account, from anywhere,
/// that admins should hear of it.
pub fn recovery_burst(user: &str) -> bool {
    state().lock().unwrap().recovery_all.get(user).is_some_and(|l| l.len() >= RECOVERY_BURST)
}

/// A place in the queue for this account and address, and when its turn comes,
/// a place among the recovery checks running now. First come, first served.
/// None: this address has enough waiting already, or the turn did not come in
/// time: nothing was checked. The permit goes along with the check: a client that
/// leaves does not free it while the processor is still working for it.
pub async fn recovery_turn(user: &str, ip: IpAddr) -> Option<RecoveryTurn> {
    static CHECKS: OnceLock<std::sync::Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let sem = CHECKS.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_RECOVERY_CHECKS))).clone();
    turn_among(sem, user, ip, RECOVERY_WAIT).await
}

async fn turn_among(sem: std::sync::Arc<tokio::sync::Semaphore>, user: &str, ip: IpAddr, wait: Duration) -> Option<RecoveryTurn> {
    let key = (user.to_string(), net_key(ip));
    {
        let mut s = state().lock().unwrap();
        let n = s.recovery_waiting.entry(key.clone()).or_insert(0);
        if *n >= MAX_RECOVERY_WAITING {
            return None;
        }
        *n += 1;
    }
    let seat = RecoverySeat(key);
    let permit = tokio::time::timeout(wait, sem.acquire_owned()).await.ok()?.ok()?;
    Some(RecoveryTurn { _seat: seat, _permit: permit })
}

/// Held for as long as a recovery check runs.
pub struct RecoveryTurn {
    _seat: RecoverySeat,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

struct RecoverySeat((String, IpAddr));

impl Drop for RecoverySeat {
    fn drop(&mut self) {
        let mut s = state().lock().unwrap();
        if let Some(n) = s.recovery_waiting.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                s.recovery_waiting.remove(&self.0);
            }
        }
    }
}

/// Count a try at a password or code from this session, before checking it:
/// false if its tries are used up (the session is to end).
pub fn proof_begin(user: &str, session: &str) -> bool {
    let now = Instant::now();
    let mut s = state().lock().unwrap();
    if s.proof_tries.len() > 10_000 {
        s.proof_tries.retain(|_, l| l.iter().any(|t| now.duration_since(*t) < LOCKOUT));
    }
    let list = s.proof_tries.entry((user.to_string(), token_hash(session))).or_default();
    list.retain(|t| now.duration_since(*t) < LOCKOUT);
    if list.len() >= MAX_PROOF_TRIES {
        return false;
    }
    list.push(now);
    true
}

/// The try was right, or nothing was tried: it does not count.
pub fn proof_refund(user: &str, session: &str) {
    if let Some(l) = state().lock().unwrap().proof_tries.get_mut(&(user.to_string(), token_hash(session))) {
        l.pop();
    }
}

/// After a wrong try: whether this session has used up its tries.
pub fn proof_used_up(user: &str, session: &str) -> bool {
    state().lock().unwrap().proof_tries.get(&(user.to_string(), token_hash(session))).is_some_and(|l| l.len() >= MAX_PROOF_TRIES)
}

/// A warning could not be sent: the next one may go out at once.
pub fn note_forget(key: &str) {
    state().lock().unwrap().notes.remove(key);
}

/// Whether a warning about `key` may go out now (at most once in `secs`). What
/// is remembered goes when its wait is over; nothing here grows without end.
pub fn note_due(key: &str, secs: u64) -> bool {
    let now = Instant::now();
    let mut s = state().lock().unwrap();
    s.notes.retain(|_, until| *until > now);
    if s.notes.contains_key(key) {
        return false;
    }
    if s.notes.len() < MAX_NOTES {
        s.notes.insert(key.to_string(), now + Duration::from_secs(secs));
    }
    true
}

/// A new device token for `user`: its value, shown once, and how it is listed.
/// None if the account changed since `generation_then` (it may have been
/// disabled while the password was checked, and must not come back with a
/// token when enabled again). Err: at the limit, or it could not be saved.
pub fn add_token(user: &str, name: &str, admin: bool, generation_then: u64) -> Result<Option<(String, TokenInfo)>, AddTokenError> {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let id: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let value = format!("{TOKEN_PREFIX}{}", random_token());
    let hash = token_hash(&value);
    let now = now_secs();
    let mut s = state().lock().unwrap();
    if s.generations.get(user).copied().unwrap_or(0) != generation_then {
        return Ok(None);
    }
    if s.tokens.values().filter(|t| t.user == user).count() >= MAX_TOKENS {
        return Err(AddTokenError::TooMany);
    }
    let t = Token { id: id.clone(), user: user.to_string(), name: name.to_string(), admin, created_at: now, last_used: None };
    if let Some(db) = &s.store {
        db.token_insert(&crate::db::TokenRow { id, user: user.to_string(), name: name.to_string(), admin, hash: hash.clone(), created_at: now, last_used: None }).map_err(|e| {
            tracing::error!("could not save a device token: {e:#}");
            AddTokenError::NotSaved
        })?;
    }
    let info = TokenInfo::from(&t);
    s.tokens.insert(hash, t);
    Ok(Some((value, info)))
}

/// The name of the live token an `Authorization` header carries.
pub fn token_name(header: &str) -> Option<String> {
    let (_, value) = token_in_header(header)?;
    state().lock().unwrap().tokens.get(&token_hash(&value)).map(|t| t.name.clone())
}

/// The tokens of an account, oldest first.
pub fn list_tokens(user: &str) -> Vec<TokenInfo> {
    let s = state().lock().unwrap();
    let mut v: Vec<TokenInfo> = s.tokens.values().filter(|t| t.user == user).map(TokenInfo::from).collect();
    v.sort_by_key(|t| (t.created, t.id.clone()));
    v
}

pub fn token_count(user: &str) -> usize {
    state().lock().unwrap().tokens.values().filter(|t| t.user == user).count()
}

/// Revoke a token of `user`: it stops working at once, and video the account
/// has open ends (what began with this token cannot be told from the rest; the
/// account's other devices simply reconnect). Sessions stay signed in.
/// Ok(false): no such token on that account. Err: it could not be removed from
/// the database, so it would come back at a restart: it stays, and stays listed.
pub fn revoke_token(user: &str, id: &str) -> Result<bool, ()> {
    let mut s = state().lock().unwrap();
    let Some(hash) = s.tokens.iter().find(|(_, t)| t.user == user && t.id == id).map(|(k, _)| k.clone()) else { return Ok(false) };
    if let Some(db) = &s.store {
        db.token_delete(id).map_err(|e| tracing::error!("could not delete a device token: {e:#}"))?;
    }
    s.tokens.remove(&hash);
    *s.generations.entry(user.to_string()).or_insert(0) += 1;
    Ok(true)
}

/// Delete every token of an account (disabled). False: they could not be
/// removed from the database, and nothing changed.
pub fn delete_user_tokens(user: &str) -> bool {
    let mut s = state().lock().unwrap();
    if let Some(db) = &s.store {
        if let Err(e) = db.token_delete_user(user) {
            tracing::error!("could not delete an account's device tokens: {e:#}");
            return false;
        }
    }
    s.tokens.retain(|_, t| t.user != user);
    *s.generations.entry(user.to_string()).or_insert(0) += 1;
    true
}

/// Delete a demoted admin's control tokens, for good: being made an admin
/// again does not bring them back. False: nothing changed.
pub fn delete_admin_tokens(user: &str) -> bool {
    let mut s = state().lock().unwrap();
    if let Some(db) = &s.store {
        if let Err(e) = db.token_delete_admin(user) {
            tracing::error!("could not delete a demoted admin's device tokens: {e:#}");
            return false;
        }
    }
    s.tokens.retain(|_, t| t.user != user || !t.admin);
    *s.generations.entry(user.to_string()).or_insert(0) += 1;
    true
}

/// Remove all that is kept for an account (deleted, or a new one with an old
/// name): two-step secret, recovery codes, tokens. False: nothing changed.
pub fn clear_account_data(user: &str) -> bool {
    let mut s = state().lock().unwrap();
    if let Some(db) = &s.store {
        if let Err(e) = db.account_data_delete(user) {
            tracing::error!("could not remove an account's two-step codes and tokens: {e:#}");
            return false;
        }
    }
    s.tokens.retain(|_, t| t.user != user);
    *s.generations.entry(user.to_string()).or_insert(0) += 1;
    true
}

/// The token an `Authorization` header carries, as a Bearer token or as the
/// password in Basic (`user:token`, for RTSP and old clients), if it is a live
/// one of an account that may sign in. Rights: the token's scope, never more
/// than the account has now. No Argon2 here: tokens are random and long.
pub fn token_identity(header: &str, accounts: &[(String, String, bool)]) -> Option<(Identity, u64)> {
    let (claimed, value) = token_in_header(header)?;
    let hash = token_hash(&value);
    let now = now_secs();
    let mut s = state().lock().unwrap();
    let t = s.tokens.get_mut(&hash)?;
    if claimed.as_deref().is_some_and(|c| c != t.user) {
        return None;
    }
    let (user, acc_admin) = accounts.iter().find(|(u, _, _)| *u == t.user).map(|(u, _, a)| (u.clone(), *a))?;
    let touch = (now - t.last_used.unwrap_or(0) >= TOKEN_TOUCH_SECS).then(|| t.id.clone());
    if touch.is_some() {
        t.last_used = Some(now);
    }
    let id = Identity { user: user.clone(), admin: acc_admin && t.admin };
    let generation = s.generations.get(&user).copied().unwrap_or(0);
    if let (Some(token_id), Some(db)) = (touch, s.store.clone()) {
        // (Not on this thread: this is every request's path.)
        std::thread::spawn(move || {
            let _ = db.token_touch(&token_id, now);
        });
    }
    Some((id, generation))
}

/// (Basic's user name if any, token), when this header holds a token at all.
fn token_in_header(header: &str) -> Option<(Option<String>, String)> {
    if let Some(v) = header.strip_prefix("Bearer ").or_else(|| header.strip_prefix("bearer ")) {
        let v = v.trim();
        return v.starts_with(TOKEN_PREFIX).then(|| (None, v.to_string()));
    }
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, header.strip_prefix("Basic ")?.trim()).ok()?;
    let raw = String::from_utf8(raw).ok()?;
    let (user, pass) = raw.split_once(':')?;
    pass.starts_with(TOKEN_PREFIX).then(|| (Some(user.to_string()), pass.to_string()))
}

/// Who an `Authorization` header says is calling. (Lockouts are for the caller
/// to check first; a valid device token passes them, see web.rs auth_mw.)
///
/// Takes a while, on whatever thread it is called: from a request, use
/// `check_authorization_queued`.
pub fn check_authorization(header: &str, accounts: &[(String, String, bool)]) -> Cred {
    if let Some(known) = check_without_hashing(header, accounts) {
        return known;
    }
    let Some((user, pass)) = basic_parts(header) else { return Cred::Wrong };
    let key: [u8; 32] = Sha256::digest(header.as_bytes()).into();
    let (user, pass) = (user.as_str(), pass.as_str());
    let then = generation(user);
    let Some(id) = check_login(accounts, user, pass) else { return Cred::Wrong };
    // (Read after the password check, which takes a while. If it cannot be read: refused.)
    if two_step_on(&id.user) != Ok(false) {
        let mut s = state().lock().unwrap();
        let now = Instant::now();
        s.basic_refused.retain(|_, (_, at)| now.duration_since(*at) < BASIC_CACHE);
        s.basic_refused.insert(key, (id.user.clone(), now));
        let store = s.store.clone();
        drop(s);
        if let Some(db) = store {
            let _ = db.insert_audit(&id.user, "", "password refused: this account needs a device token here", "", 401);
        }
        return Cred::TwoStepNeeded;
    }
    // (Not remembered if the account changed while the password was checked.)
    let mut s = state().lock().unwrap();
    if s.generations.get(user).copied().unwrap_or(0) == then {
        s.basic.insert(key, (id.clone(), Instant::now()));
    }
    Cred::Password(id, then)
}

fn basic_parts(header: &str) -> Option<(String, String)> {
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, header.strip_prefix("Basic ")?.trim()).ok().and_then(|r| String::from_utf8(r).ok())?;
    let (user, pass) = raw.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// What can be told without a password check: a token, a password checked a
/// moment ago, or something that is no password at all. None: it takes a check.
fn check_without_hashing(header: &str, accounts: &[(String, String, bool)]) -> Option<Cred> {
    if let Some((id, generation)) = token_identity(header, accounts) {
        return Some(Cred::Token(id, generation));
    }
    if !header.starts_with("Basic ") {
        return Some(Cred::Wrong);
    }
    let key: [u8; 32] = Sha256::digest(header.as_bytes()).into();
    let s = state().lock().unwrap();
    if let Some((id, at)) = s.basic.get(&key) {
        if at.elapsed() < BASIC_CACHE && accounts.iter().any(|(u, _, a)| *u == id.user && *a == id.admin) {
            // (Any change to the account empties this cache under this same lock.)
            return Some(Cred::Password(id.clone(), s.generations.get(&id.user).copied().unwrap_or(0)));
        }
    }
    if let Some((user, at)) = s.basic_refused.get(&key) {
        if at.elapsed() < BASIC_CACHE && accounts.iter().any(|(u, _, _)| u == user) {
            return Some(Cred::TwoStepNeeded);
        }
    }
    drop(s);
    if basic_parts(header).is_none() {
        return Some(Cred::Wrong);
    }
    None
}

/// Why a password was not checked. It does not count as a wrong one.
#[derive(Debug, PartialEq)]
pub enum NotChecked {
    /// Too many being checked: come back in a moment.
    Busy,
    /// This address was locked out while it waited.
    Locked,
}

/// What a client is told when its password was not checked for being too many.
pub const PASSWORD_BUSY: &str = "many sign-ins are being checked right now, and this one was not: try again in a moment";

/// `check_authorization` for a request: what takes no password check at once,
/// the rest in line and off the async workers, so that any number of them
/// costs a few checks at a time and holds up nothing else.
pub async fn check_authorization_queued(header: &str, accounts: &[(String, String, bool)], ip: IpAddr) -> Result<Cred, NotChecked> {
    if let Some(known) = check_without_hashing(header, accounts) {
        return Ok(known);
    }
    let (h, acc) = (header.to_string(), accounts.to_vec());
    password_check(ip, move || check_authorization(&h, &acc)).await
}

/// Run a password check in its turn. The turn goes along with the check: a client
/// that leaves frees nothing while the processor still works for it. An address
/// locked out while it waited is checked no further.
pub async fn password_check<T: Send + 'static>(ip: IpAddr, check: impl FnOnce() -> T + Send + 'static) -> Result<T, NotChecked> {
    static LINE: OnceLock<(std::sync::Arc<tokio::sync::Semaphore>, std::sync::Arc<tokio::sync::Semaphore>)> = OnceLock::new();
    let (places, checks) = LINE.get_or_init(|| (std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_PASSWORD_WAITING)), std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_PASSWORD_CHECKS))));
    let turn = password_turn_among(places.clone(), checks.clone(), PASSWORD_WAIT).await.ok_or(NotChecked::Busy)?;
    tokio::task::spawn_blocking(move || {
        let _turn = turn;
        if is_locked(ip) {
            return Err(NotChecked::Locked);
        }
        Ok(check())
    })
    .await
    .unwrap_or(Err(NotChecked::Busy))
}

async fn password_turn_among(places: std::sync::Arc<tokio::sync::Semaphore>, checks: std::sync::Arc<tokio::sync::Semaphore>, wait: Duration) -> Option<PasswordTurn> {
    let place = places.try_acquire_owned().ok()?;
    let check = tokio::time::timeout(wait, checks.acquire_owned()).await.ok()?.ok()?;
    Some(PasswordTurn { _place: place, _check: check })
}

struct PasswordTurn {
    _place: tokio::sync::OwnedSemaphorePermit,
    _check: tokio::sync::OwnedSemaphorePermit,
}

/// A session for a sign-in that began at this `generation` of the account:
/// none if the account changed meanwhile (a sign-in racing a disable must
/// not leave a session that comes back to life when it is enabled again).
pub fn new_session_if_current(id: Identity, generation_then: u64, meta: SessionMeta) -> Option<String> {
    if state().lock().unwrap().generations.get(&id.user).copied().unwrap_or(0) != generation_then {
        return None;
    }
    let token = new_session_with(id.clone(), meta);
    // (Changed in the moment between the two: forget_user may have run before the insert.)
    if state().lock().unwrap().generations.get(&id.user).copied().unwrap_or(0) != generation_then {
        end_session(&token);
        return None;
    }
    Some(token)
}

#[cfg(test)]
pub fn new_session(id: Identity) -> String {
    new_session_with(id, SessionMeta::default())
}

/// A session; `meta` has what is known of where it was made (created and last
/// seen are set here).
pub fn new_session_with(id: Identity, mut meta: SessionMeta) -> String {
    let token = random_token();
    let hash = token_hash(&token);
    let now = now_secs();
    let expires = now + SESSION_DAYS * 86400;
    meta.created = now;
    meta.last_seen = now;
    meta.saved_seen = now;
    let mut s = state().lock().unwrap();
    s.sessions.retain(|_, v| v.expires > now);
    if let Some(db) = &s.store {
        let row = crate::db::StoredSession { hash: hash.clone(), user: id.user.clone(), admin: id.admin, expires, created: now, ip: meta.ip.map(|i| i.to_string()).unwrap_or_default(), label: meta.label.clone(), device: meta.device.clone(), last_seen: now };
        if let Err(e) = db.save_session(&row) {
            tracing::error!("could not keep a new session in the database (it ends at a restart): {e:#}");
        }
    }
    s.sessions.insert(hash, Session { id, expires, meta });
    token
}

/// A signed-in request was made with this session, from `ip`: noted in memory
/// at once, and written to the database at most every `SEEN_SAVE_SECS`.
pub fn session_seen(token: &str, ip: IpAddr) {
    let hash = token_hash(token);
    let now = now_secs();
    let (store, due) = {
        let mut s = state().lock().unwrap();
        let Some(sess) = s.sessions.get_mut(&hash) else { return };
        let due = sess.meta.note_use(ip, now);
        (s.store.clone(), due)
    };
    // (The database is written without holding the lock.)
    if let (true, Some(db)) = (due, store) {
        if let Err(e) = db.touch_session(&hash, &ip.to_string(), now) {
            tracing::warn!("could not note when a session was last used: {e:#}");
        }
    }
}

/// An account's live sessions, most recently used first. `current`: the
/// token of the request, so that its session is marked.
pub fn list_sessions(user: &str, current: Option<&str>) -> Vec<SessionInfo> {
    let now = now_secs();
    let mine = current.map(token_hash);
    let s = state().lock().unwrap();
    let mut list: Vec<SessionInfo> = s
        .sessions
        .iter()
        .filter(|(_, v)| v.id.user == user && v.expires > now)
        .map(|(h, v)| SessionInfo { id: session_id(h), meta: v.meta.clone(), this: Some(h) == mine.as_ref() })
        .collect();
    list.sort_by(|a, b| b.meta.last_seen.max(b.meta.created).cmp(&a.meta.last_seen.max(a.meta.created)).then_with(|| a.id.cmp(&b.id)));
    list
}

/// True the first time since the server started that this account is seen
/// using its password in HTTP Basic from this network (see `net_key`).
pub fn basic_net_first(user: &str, ip: IpAddr) -> bool {
    let mut s = state().lock().unwrap();
    // (Only passwords that were right get here, so this stays small; a cap anyway.)
    if s.basic_nets.len() >= 2000 {
        s.basic_nets.clear();
    }
    s.basic_nets.insert((user.to_string(), net_key(ip)))
}

/// The device hash of the session a request carries, if it has one that is live.
pub fn session_device_from_headers(headers: &axum::http::HeaderMap, accounts: &[(String, String, bool)]) -> Option<String> {
    let (token, _) = session_from_headers(headers, accounts)?;
    let s = state().lock().unwrap();
    s.sessions.get(&token_hash(token)).map(|v| v.meta.device.clone()).filter(|d| !d.is_empty())
}

/// Does the account have a live session?
pub fn has_sessions(user: &str) -> bool {
    let now = now_secs();
    state().lock().unwrap().sessions.values().any(|v| v.id.user == user && v.expires > now)
}

/// Sign one of the account's own devices out. None: the account has no such
/// session (also when the id is another account's). Err: it could not be
/// removed from the database, so nothing was changed. Open video and
/// sockets of the account end with it (they begin again by themselves, for
/// the devices that are still signed in), as when a device token is revoked.
pub fn end_session_of(user: &str, id: &str, current: Option<&str>) -> Result<Option<SessionInfo>, ()> {
    let now = now_secs();
    let mine = current.map(token_hash);
    let mut s = state().lock().unwrap();
    let Some(hash) = s.sessions.iter().find(|(h, v)| v.id.user == user && v.expires > now && session_id(h) == id).map(|(h, _)| h.clone()) else { return Ok(None) };
    let forget = device_forgotten_with(&s, &hash, mine.as_ref());
    if let Some(db) = &s.store {
        db.end_session_row(user, &hash, &forget).map_err(|e| tracing::error!("could not remove a session from the database: {e:#}"))?;
    }
    let sess = s.sessions.remove(&hash).expect("found above");
    *s.generations.entry(user.to_string()).or_insert(0) += 1;
    Ok(Some(SessionInfo { id: id.to_string(), meta: sess.meta, this: Some(&hash) == mine.as_ref() }))
}

/// Signing out another device also makes its browser (and its notifications) new
/// again: the person who has it signs in again, and admins hear of it. Not for this
/// device (a plain sign-out keeps its row), and not for a second session of the browser
/// this request comes from (it would forget the browser in use). Empty: nothing.
fn device_forgotten_with(s: &State, hash: &str, mine: Option<&String>) -> String {
    let Some(sess) = s.sessions.get(hash) else { return String::new() };
    let now = now_secs();
    // Not while another live session of the account (the one making this request among
    // them) is of the same browser: that browser is still signed in.
    let still_in = s.sessions.iter().any(|(h, v)| h != hash && v.id.user == sess.id.user && v.expires > now && v.meta.device == sess.meta.device);
    if Some(&hash.to_string()) == mine || still_in {
        String::new()
    } else {
        sess.meta.device.clone()
    }
}

/// What signing this session out would forget (see `device_forgotten_with`): the browser's
/// hash, to look up notifications that go with it before it happens. Empty: nothing.
pub fn device_forgotten_by_sign_out(user: &str, id: &str, current: Option<&str>) -> String {
    let now = now_secs();
    let mine = current.map(token_hash);
    let s = state().lock().unwrap();
    let Some(hash) = s.sessions.iter().find(|(h, v)| v.id.user == user && v.expires > now && session_id(h) == id).map(|(h, _)| h.clone()) else { return String::new() };
    device_forgotten_with(&s, &hash, mine.as_ref())
}

/// The browsers of the account's sessions that end when all but `keep` do, without the
/// one `keep` is from (a second session of the same browser).
pub fn devices_ending(user: &str, keep: Option<&str>) -> Vec<String> {
    let s = state().lock().unwrap();
    devices_ending_in(&s, user, keep.map(token_hash).as_ref())
}

fn devices_ending_in(s: &State, user: &str, keep: Option<&String>) -> Vec<String> {
    // (Only the kept session stays live; a browser it shares is not forgotten.)
    let kept = keep.and_then(|k| s.sessions.get(k)).filter(|v| v.id.user == user && v.expires > now_secs()).map(|v| v.meta.device.clone()).unwrap_or_default();
    let mut v: Vec<String> = s.sessions.iter().filter(|(h, x)| x.id.user == user && Some(*h) != keep && !x.meta.device.is_empty() && x.meta.device != kept).map(|(_, x)| x.meta.device.clone()).collect();
    v.sort();
    v.dedup();
    v
}

pub fn end_session(token: &str) {
    let hash = token_hash(token);
    let mut s = state().lock().unwrap();
    s.sessions.remove(&hash);
    if let Some(db) = &s.store {
        let _ = db.delete_session(&hash);
    }
}

/// Sessions of users that no longer exist (or changed role) stop working.
pub fn session_identity(token: &str, accounts: &[(String, String, bool)]) -> Option<Identity> {
    let s = state().lock().unwrap();
    let sess = s.sessions.get(&token_hash(token)).filter(|v| v.expires > now_secs())?;
    let (u, _, admin) = accounts.iter().find(|(u, _, _)| *u == sess.id.user)?;
    // A session never gains admin rights it was not started with (e.g. one
    // that came from a linked server as a viewer).
    Some(Identity { user: u.clone(), admin: *admin && sess.id.admin })
}

/// A one-time code that signs `user` in on another device (the phone QR).
/// Any earlier unused code for the same user stops working.
#[cfg(test)]
pub fn new_pair_code(user: &str) -> String {
    new_pair_code_capped(user, true)
}

/// A sign-in code whose session is never an admin one unless `admin_ok`
/// (used when arriving from a linked server where the user is not an admin).
#[cfg(test)]
pub fn new_pair_code_capped(user: &str, admin_ok: bool) -> String {
    let code = random_token();
    let now = now_secs();
    let mut s = state().lock().unwrap();
    s.pairs.retain(|_, (u, exp, _, _)| *exp > now && u != user);
    s.pairs.insert(token_hash(&code), (user.to_string(), now + PAIR_SECS, admin_ok, false));
    code
}

/// As `new_pair_code_capped`, but none if the account changed since
/// `generation_then` (it may have got two-step codes or lost its admin rights
/// after the caller looked).
pub fn new_pair_code_if_current(user: &str, admin_ok: bool, generation_then: u64) -> Option<String> {
    let code = random_token();
    let now = now_secs();
    let mut s = state().lock().unwrap();
    if s.generations.get(user).copied().unwrap_or(0) != generation_then {
        return None;
    }
    s.pairs.retain(|_, (u, exp, _, _)| *exp > now && u != user);
    s.pairs.insert(token_hash(&code), (user.to_string(), now + PAIR_SECS, admin_ok, false));
    Some(code)
}

/// A one-time sign-in code for a user arriving from a linked server. Unlike
/// the phone QR code it does not cancel the user's other pending codes.
/// None if the account changed since `generation_then` (two-step may have
/// come on after the caller checked).
pub fn new_link_signin_code(user: &str, admin_ok: bool, generation_then: u64) -> Option<String> {
    let code = random_token();
    let now = now_secs();
    let mut s = state().lock().unwrap();
    if s.generations.get(user).copied().unwrap_or(0) != generation_then {
        return None;
    }
    s.pairs.retain(|_, (_, exp, _, _)| *exp > now);
    s.pairs.insert(token_hash(&code), (user.to_string(), now + PAIR_SECS, admin_ok, true));
    Some(code)
}

/// Use up a sign-in code: the user it was made for and whether the session
/// may be an admin one, if still valid.
#[cfg(test)]
pub fn take_pair_code(code: &str) -> Option<(String, bool)> {
    take_pair_code_from(code).map(|(u, a, _)| (u, a))
}

/// As `take_pair_code`, and whether the code was made for a user arriving
/// from a linked server.
pub fn take_pair_code_from(code: &str) -> Option<(String, bool, bool)> {
    let mut s = state().lock().unwrap();
    let (user, exp, admin_ok, link) = s.pairs.remove(&token_hash(code))?;
    (exp > now_secs()).then_some((user, admin_ok, link))
}

/// Verify an `Authorization: Basic ...` password (cached for a few minutes).
#[cfg(test)]
pub fn basic_identity(header: &str, accounts: &[(String, String, bool)]) -> Option<Identity> {
    match check_authorization(header, accounts) {
        Cred::Password(id, _) => Some(id),
        _ => None,
    }
}

/// After a password change, deletion or permission change: forget cached
/// Basic logins and end the user's sessions (and, by the new generation, open
/// tickets), except `keep`, the session making the change. Device tokens stay:
/// they end on purpose (`revoke_token`, `delete_user_tokens`).
/// False: the sessions are ended now but are still in the database and would
/// return at a restart. Logged; a caller promising they ended must say so.
pub fn forget_user(user: &str, keep: Option<&str>) -> bool {
    forget_user_inner(user, keep, false)
}

/// As `forget_user`, and the browsers of the sessions that end are forgotten too
/// (with their notifications): signing out all other devices.
pub fn forget_user_and_their_devices(user: &str, keep: Option<&str>) -> bool {
    forget_user_inner(user, keep, true)
}

/// A password was changed: other sessions end (as `forget_user`) and the account's known
/// browsers are forgotten, except the one the person changed it from (`keep`, their
/// own session), so that a thief who signed in before and logged out is a new browser
/// at the next sign-in.
/// Sessions that end lose their notifications too (a phone with no live session keeps
/// getting alerts). The list of networks a direct password was seen from starts again.
pub fn password_changed(user: &str, keep: Option<&str>) -> Result<(), PwFail> {
    let (store, device) = {
        let mut s = state().lock().unwrap();
        s.basic_nets.retain(|(u, _)| u != user);
        (s.store.clone(), keep.and_then(|t| s.sessions.get(&token_hash(t)).map(|v| v.meta.device.clone())).unwrap_or_default())
    };
    if !forget_user_inner(user, keep, true) {
        return Err(PwFail::Sessions);
    }
    match store {
        Some(db) => db.devices_clear(user, &device).map_err(|e| {
            tracing::error!("could not forget an account's known browsers: {e:#}");
            PwFail::Browsers
        }),
        None => Ok(()),
    }
}

/// What could not be done after a password change.
#[derive(Debug, PartialEq)]
pub enum PwFail {
    /// Other sessions could not be removed from the database.
    Sessions,
    /// The sessions are gone, but the known browsers could not be cleared.
    Browsers,
}

fn forget_user_inner(user: &str, keep: Option<&str>, drop_devices: bool) -> bool {
    let mut s = state().lock().unwrap();
    *s.generations.entry(user.to_string()).or_insert(0) += 1;
    s.basic.retain(|_, (id, _)| id.user != user);
    s.basic_refused.retain(|_, (u, _)| u != user);
    let keep = keep.map(token_hash);
    let gone: Vec<String> = if drop_devices { devices_ending_in(&s, user, keep.as_ref()) } else { Vec::new() };
    s.sessions.retain(|h, sess| sess.id.user != user || Some(h) == keep.as_ref());
    s.pairs.retain(|_, (u, _, _, _)| u != user);
    if let Some(db) = &s.store {
        if let Err(e) = db.delete_user_sessions(user, keep.as_deref(), &gone) {
            tracing::error!("could not remove an account's sessions from the database: {e:#}");
            return false;
        }
    }
    true
}

/// True while this address is locked out after too many failures.
pub fn is_locked(ip: IpAddr) -> bool {
    let ip = net_key(ip);
    let mut s = state().lock().unwrap();
    match s.locked.get(&ip) {
        Some(until) if *until > Instant::now() => true,
        Some(_) => {
            s.locked.remove(&ip);
            false
        }
        None => false,
    }
}

/// Record a failed sign-in; returns true if the address is now locked out.
pub fn record_failure(ip: IpAddr) -> bool {
    let ip = net_key(ip);
    let mut s = state().lock().unwrap();
    let now = Instant::now();
    // Many addresses (IPv6) must not grow these maps without end.
    if s.fails.len() > 10_000 {
        s.fails.retain(|_, l| l.iter().any(|t| now.duration_since(*t) < FAIL_WINDOW));
    }
    if s.locked.len() > 10_000 {
        s.locked.retain(|_, until| *until > now);
    }
    let list = s.fails.entry(ip).or_default();
    list.retain(|t| now.duration_since(*t) < FAIL_WINDOW);
    list.push(now);
    if list.len() >= MAX_FAILS {
        list.clear();
        s.locked.insert(ip, now + LOCKOUT);
        return true;
    }
    false
}

pub fn record_success(ip: IpAddr) {
    state().lock().unwrap().fails.remove(&net_key(ip));
}

/// Value of the session cookie in a Cookie header.
#[cfg(test)]
pub fn cookie_token(cookie_header: &str) -> Option<&str> {
    cookie_tokens_in(cookie_header).next()
}

fn cookie_tokens_in(cookie_header: &str) -> impl Iterator<Item = &str> {
    cookie_header
        .split(';')
        .map(|c| c.trim())
        .filter_map(|c| c.strip_prefix(SESSION_COOKIE).and_then(|r| r.strip_prefix('=')))
        .filter(|t| !t.is_empty())
}

/// Every session-cookie value the request carries. Over HTTP/2 a browser may
/// split cookies across several `Cookie` headers, and a stale cookie of the
/// same name (another path, an older install) can come first, so callers try
/// them all.
pub fn cookie_tokens(headers: &axum::http::HeaderMap) -> Vec<&str> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(cookie_tokens_in)
        .collect()
}

/// The first session cookie that names a live session, with its identity.
pub fn session_from_headers<'a>(
    headers: &'a axum::http::HeaderMap,
    accounts: &[(String, String, bool)],
) -> Option<(&'a str, Identity)> {
    cookie_tokens(headers).into_iter().find_map(|t| session_identity(t, accounts).map(|id| (t, id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_and_legacy_plaintext() {
        let h = hash_password("s3cret pw");
        assert!(is_hashed(&h));
        assert!(verify_password("s3cret pw", &h));
        assert!(!verify_password("wrong", &h));
        assert!(verify_password("plain", "plain"));
        assert!(!verify_password("", ""));
    }

    #[test]
    fn config_passwords_upgraded_once() {
        let mut cfg: crate::config::Config = toml::from_str(
            "[[server.users]]\nusername = \"admin\"\npassword = \"pw1\"\n[[server.users]]\nusername = \"guest\"\npassword = \"pw2\"\nadmin = false\n",
        )
        .unwrap();
        assert!(hash_config_passwords(&mut cfg));
        assert!(cfg.server.users.iter().all(|u| is_hashed(&u.password)));
        assert!(!hash_config_passwords(&mut cfg)); // already hashed
        let acc = accounts(&cfg);
        assert_eq!(check_login(&acc, "guest", "pw2"), Some(Identity { user: "guest".into(), admin: false }));
        assert_eq!(check_login(&acc, "guest", "pw1"), None);
    }

    fn users(toml_text: &str) -> crate::config::Config {
        toml::from_str(toml_text).unwrap()
    }
    const THREE: &str = "[[server.users]]\nusername = \"boss\"\npassword = \"pw-boss-1\"\n[[server.users]]\nusername = \"other\"\npassword = \"pw-other-1\"\n[[server.users]]\nusername = \"off\"\npassword = \"pw-off-1\"\nadmin = false\ndisabled = true\n";

    #[test]
    fn a_disabled_account_cannot_get_in_any_way() {
        let cfg = users(THREE);
        let acc = accounts(&cfg);
        assert!(!acc.iter().any(|(u, _, _)| u == "off"), "left out of the list every way in reads");
        // Password, with the right password.
        assert!(check_login(&acc, "off", "pw-off-1").is_none());
        // HTTP Basic, with the right password.
        let basic = format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "off:pw-off-1"));
        assert!(basic_identity(&basic, &acc).is_none());
        // A session from before it was disabled.
        let t = new_session(Identity { user: "off".into(), admin: false });
        assert!(session_identity(&t, &acc).is_none());
        // A sign-in code from before it was disabled: the code resolves, the
        // account check after it (web.rs pair) finds nothing.
        let code = new_pair_code("off");
        let (who, _) = take_pair_code(&code).unwrap();
        assert!(!acc.iter().any(|(u, _, _)| *u == who));
        // Running video stops, and the others keep going.
        assert!(!still_allowed(&cfg, "off", 0));
        assert!(still_allowed(&cfg, "boss", generation("boss")));
    }

    #[test]
    fn disabling_forgets_cached_passwords_sessions_and_codes() {
        let mut cfg = users(THREE);
        let enabled = cfg.server.users.iter_mut().find(|u| u.username == "other").unwrap();
        enabled.password = hash_password("pw-other-1");
        let acc = accounts(&cfg);
        let basic = format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "other:pw-other-1"));
        assert!(basic_identity(&basic, &acc).is_some(), "now cached");
        let t = new_session(Identity { user: "other".into(), admin: true });
        let code = new_pair_code("other");
        cfg.server.users.iter_mut().find(|u| u.username == "other").unwrap().disabled = true;
        let acc = accounts(&cfg);
        assert!(basic_identity(&basic, &acc).is_none(), "the cached password does not help");
        assert!(session_identity(&t, &acc).is_none());
        forget_user("other", None);
        assert!(take_pair_code(&code).is_none(), "the code is gone");
        // Turned back on: nothing old comes back to life.
        cfg.server.users.iter_mut().find(|u| u.username == "other").unwrap().disabled = false;
        let acc = accounts(&cfg);
        assert!(session_identity(&t, &acc).is_none(), "old sessions stay ended");
        assert!(check_login(&acc, "other", "pw-other-1").is_some(), "the password works again");
    }

    #[test]
    fn an_account_change_ends_what_began_before_it() {
        let cfg = users(THREE);
        let then = generation("boss");
        assert!(still_allowed(&cfg, "boss", then));
        forget_user("boss", None);
        assert!(!still_allowed(&cfg, "boss", then), "disabled and enabled again within a check still ends it");
        assert!(still_allowed(&cfg, "boss", generation("boss")));
        // A sign-in that was checking the password while the account changed.
        let racing = generation("race");
        forget_user("race", None);
        assert!(new_session_if_current(Identity { user: "race".into(), admin: true }, racing, SessionMeta::default()).is_none());
        assert!(new_session_if_current(Identity { user: "race".into(), admin: true }, generation("race"), SessionMeta::default()).is_some());
    }

    #[test]
    fn an_old_style_account_cannot_get_round_the_flag() {
        let cfg = users("[server.auth]\nusername = \"x\"\npassword = \"pw-x-12345\"\n[[server.users]]\nusername = \"x\"\npassword = \"pw-x-12345\"\ndisabled = true\n[[server.users]]\nusername = \"y\"\npassword = \"pw-y-12345\"\n");
        let acc = accounts(&cfg);
        assert!(!acc.iter().any(|(u, _, _)| u == "x"));
        assert!(acc.iter().any(|(u, _, _)| u == "y"));
    }

    #[test]
    fn all_accounts_disabled_stays_locked_it_never_falls_open() {
        let cfg = users("[[server.users]]\nusername = \"a\"\npassword = \"pw-a-12345\"\ndisabled = true\n");
        let acc = accounts(&cfg);
        assert!(!acc.is_empty(), "empty would mean an open server");
        for (u, p) in [("a", "pw-a-12345"), (LOCKED_ACCOUNT, ""), (LOCKED_ACCOUNT, LOCKED_HASH), (LOCKED_ACCOUNT, "locked")] {
            assert!(check_login(&acc, u, p).is_none(), "{u}");
        }
        assert!(!acc[0].2, "and it is not an admin");
        assert!(!still_allowed(&cfg, "(open)", 0), "open streams from before end");
        // A server with no accounts at all is still the open kind.
        assert!(accounts(&users("")).is_empty());
    }

    fn basic(user: &str, pass: &str) -> String {
        format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{user}:{pass}")))
    }

    fn set_two_step(user: &str) {
        state().lock().unwrap().two_step_test.insert(user.to_string());
    }

    fn held(t: &str) -> TicketHold {
        match take_ticket(t) {
            TicketTry::Held(h) => h,
            _ => panic!("not held"),
        }
    }

    fn late(t: &str) -> bool {
        matches!(take_ticket(t), TicketTry::Late)
    }

    #[test]
    fn a_ticket_works_once_and_dies_with_any_account_change() {
        let g = generation("tk1");
        let t = new_ticket("tk1", g, false).unwrap();
        let h = held(&t);
        assert_eq!(h.user, "tk1");
        assert!(matches!(take_ticket(&t), TicketTry::Busy), "one try at a time");
        // A mistyped code: the person may try again without the password.
        drop(h);
        held(&t).burn();
        assert!(late(&t), "used");
        // Any change ends it, also one made while it was held.
        let t2 = new_ticket("tk1", generation("tk1"), false).unwrap();
        let h2 = held(&t2);
        forget_user("tk1", None);
        drop(h2);
        assert!(late(&t2));
        let t3 = new_ticket("tk1", generation("tk1"), false).unwrap();
        forget_user("tk1", None);
        assert!(late(&t3), "dies with the account change");
        // A sign-in racing the change gets none at all.
        let then = generation("tk1");
        forget_user("tk1", None);
        assert!(new_ticket("tk1", then, false).is_none());
    }

    #[test]
    fn an_expired_ticket_is_late_a_made_up_or_changed_one_is_guessing() {
        let g = generation("tk2");
        let old = make_ticket(&Ticket { user: "tk2".into(), generation: g, expires: now_secs() - 1, recovery_only: false, nonce: "n1".into() });
        assert!(late(&old), "signed by this server, and late");
        for bad in ["", ".", "not-a-ticket", "made.up", &"0".repeat(64)] {
            assert!(matches!(take_ticket(bad), TicketTry::MadeUp), "{bad}");
        }
        let good = new_ticket("tk2", g, false).unwrap();
        let (fields, sig) = good.rsplit_once('.').unwrap();
        let flip = |c: char| if c == 'A' { 'B' } else { 'A' };
        // Every character of the signature and of the fields counts.
        for i in 0..sig.len() - 1 {
            let t = format!("{fields}.{}{}{}", &sig[..i], flip(sig.as_bytes()[i] as char), &sig[i + 1..]);
            assert!(matches!(take_ticket(&t), TicketTry::MadeUp), "signature {i}");
        }
        for i in 0..fields.len() - 1 {
            let t = format!("{}{}{}.{sig}", &fields[..i], flip(fields.as_bytes()[i] as char), &fields[i + 1..]);
            assert!(matches!(take_ticket(&t), TicketTry::MadeUp), "fields {i}");
        }
        // Another account's signature on these fields, or another key: guessing too.
        let other = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[7u8; 32]);
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let t = format!("{fields}.{}", b64.encode(ring::hmac::sign(&other, fields.as_bytes()).as_ref()));
        assert!(matches!(take_ticket(&t), TicketTry::MadeUp));
        // The real one was not used up by all that.
        held(&good).burn();
    }

    #[test]
    fn a_ticket_carries_any_name_and_stores_nothing_until_used() {
        for name in ["with.dots.in.it", "a:b", "Zoë", "odd\u{202e}name\n"] {
            let t = new_ticket(name, generation(name), true).unwrap();
            let h = held(&t);
            assert_eq!(h.user, name);
            assert!(h.recovery_only);
            h.burn();
        }
        // A thousand tickets from everywhere: none is kept, so none is refused or pushed out.
        let g = generation("tk3");
        let first = new_ticket("tk3", g, false).unwrap();
        let before = state().lock().unwrap().spent_tickets.len();
        let all: Vec<String> = (0..1000).map(|_| new_ticket("tk3", g, false).unwrap()).collect();
        assert_eq!(all.iter().collect::<std::collections::HashSet<_>>().len(), 1000, "each its own");
        drop(held(&first));
        drop(held(&all[0]));
        drop(held(all.last().unwrap()));
        // (Other tests use tickets too: nothing of these thousand is left, whatever they did.)
        let s = state().lock().unwrap();
        assert!(!s.spent_tickets.contains_key(&read_ticket(&first).unwrap().nonce));
        assert!(!s.tickets_trying.contains(&read_ticket(&first).unwrap().nonce), "held tries end");
        let _ = before;
    }

    #[test]
    fn a_used_ticket_is_forgotten_when_it_would_have_run_out() {
        let g = generation("tk6");
        let t = make_ticket(&Ticket { user: "tk6".into(), generation: g, expires: now_secs() + 1000, recovery_only: false, nonce: "tk6-spent".into() });
        held(&t).burn();
        assert!(state().lock().unwrap().spent_tickets.contains_key("tk6-spent"));
        state().lock().unwrap().spent_tickets.insert("tk6-spent".into(), now_secs() - 1);
        take_ticket("made up");
        let _ = new_ticket("tk6", g, false).map(|x| take_ticket(&x));
        assert!(!state().lock().unwrap().spent_tickets.contains_key("tk6-spent"));
    }

    #[test]
    fn right_passwords_are_limited_per_account_and_address() {
        let a = IpAddr::from([198, 51, 100, 7]);
        for i in 0..MAX_TICKETS_PER_MINUTE {
            assert!(ticket_allowed("tr1", a), "{i}");
        }
        assert!(!ticket_allowed("tr1", a));
        assert!(ticket_allowed("tr2", a), "another account behind the same address");
        assert!(ticket_allowed("tr1", IpAddr::from([198, 51, 100, 8])), "the owner elsewhere");
        // A /64 is one address.
        let v6 = |n: u16| IpAddr::from([0x2001, 0xdb8, 0, 1, 0, 0, 0, n]);
        for _ in 0..MAX_TICKETS_PER_MINUTE {
            assert!(ticket_allowed("tr3", v6(1)));
        }
        assert!(!ticket_allowed("tr3", v6(2)));
    }

    #[test]
    fn a_pair_code_is_not_made_if_the_account_changed() {
        let then = generation("pc1");
        forget_user("pc1", None);
        assert!(new_pair_code_if_current("pc1", false, then).is_none());
        let c = new_pair_code_if_current("pc1", false, generation("pc1")).unwrap();
        assert_eq!(take_pair_code(&c), Some(("pc1".to_string(), false)));
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, n])
    }

    fn wrong_tries(user: &str, a: IpAddr, n: usize) -> Vec<Option<CodeLock>> {
        (0..n).map(|_| { assert!(code_try(user, a)); code_wrong(user, a) }).collect()
    }

    #[test]
    fn wrong_codes_lock_that_address_not_the_owner_elsewhere() {
        let got = wrong_tries("cf1", ip(1), MAX_CODE_TRIES);
        assert_eq!(got.last().unwrap(), &Some(CodeLock::Address));
        assert!(got[..MAX_CODE_TRIES - 1].iter().all(Option::is_none));
        assert!(code_blocked("cf1", ip(1)));
        assert!(!code_try("cf1", ip(1)), "not even the right code from there");
        assert!(!code_blocked("cf1", ip(2)), "the owner, somewhere else");
        assert!(code_try("cf1", ip(2)));
        assert!(!code_blocked("cf2", ip(1)), "and another account from that address");
    }

    #[test]
    fn many_addresses_together_hit_the_account_limit() {
        let mut last = None;
        for i in 0..MAX_ACCOUNT_CODE_TRIES {
            let a = ip((i / MAX_CODE_TRIES + 1) as u8);
            assert!(code_try("cf4", a), "{i}");
            last = code_wrong("cf4", a).or(last);
        }
        assert_eq!(last, Some(CodeLock::Account));
        assert!(code_blocked("cf4", ip(200)), "from everywhere now");
        assert!(!code_try("cf4", ip(200)));
        code_clear("cf4");
        assert!(code_try("cf4", ip(200)), "a new password ends the wait");
    }

    #[test]
    fn parallel_tries_cannot_pass_the_limit() {
        // Counted when they start: all in flight at once, none has an answer yet.
        let started = (0..MAX_CODE_TRIES + 3).filter(|_| code_try("cf5", ip(1))).count();
        assert_eq!(started, MAX_CODE_TRIES);
    }

    #[test]
    fn a_right_code_lets_an_ordinary_person_go_on() {
        for _ in 0..MAX_ACCOUNT_CODE_TRIES * 2 {
            assert!(code_try("cf3", ip(1)));
            code_ok("cf3", ip(1));
        }
        assert!(!code_blocked("cf3", ip(1)));
        wrong_tries("cf3", ip(1), MAX_CODE_TRIES - 1);
        code_ok("cf3", ip(1));
        assert_eq!(wrong_tries("cf3", ip(1), 1), vec![None], "starts again after a right code");
    }

    #[test]
    fn ipv6_tries_count_by_their_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(net_key(a), net_key(b));
        assert_ne!(net_key(a), net_key(other));
        let mapped: IpAddr = "::ffff:192.0.2.9".parse().unwrap();
        assert_eq!(net_key(mapped), "192.0.2.9".parse::<IpAddr>().unwrap());
        assert_eq!(net_key(ip(9)), ip(9));
        // Moving around inside the /64 does not help.
        for _ in 0..MAX_CODE_TRIES {
            assert!(code_try("v61", a));
            code_wrong("v61", b);
        }
        assert!(code_blocked("v61", b));
        assert!(!code_blocked("v61", other));
        let c: IpAddr = "2001:db8:9:9:1::1".parse().unwrap();
        let d: IpAddr = "2001:db8:9:9:2::2".parse().unwrap();
        for _ in 0..MAX_FAILS {
            record_failure(c);
        }
        assert!(is_locked(d), "and the same for wrong passwords");
    }

    #[test]
    fn a_session_has_its_own_count_of_codes() {
        for i in 0..MAX_SESSION_CODE_TRIES {
            assert!(session_code_try("s-1"), "{i}");
        }
        assert!(!session_code_try("s-1"));
        assert!(session_code_try("s-2"), "another session");
        // The sign-in locks do not come into it.
        wrong_tries("sc1", ip(1), MAX_CODE_TRIES);
        assert!(code_blocked("sc1", ip(1)));
        assert!(session_code_try("s-3"));
        // A right code does not count, and there is a limit per day too.
        for _ in 0..20 {
            assert!(session_code_try("s-4"));
            session_code_ok("s-4");
        }
        let now = Instant::now();
        let long_ago = now.checked_sub(CODE_LOCK + Duration::from_secs(5)).unwrap();
        state().lock().unwrap().session_code_tries.insert(token_hash("s-5"), vec![long_ago; MAX_SESSION_CODE_TRIES_DAY - 1]);
        assert!(session_code_try("s-5"), "the tenth today");
        assert!(!session_code_try("s-5"), "that is all for today");
    }

    #[test]
    fn recovery_codes_are_past_every_lock_under_a_limit_per_address() {
        wrong_tries("rc1", ip(1), MAX_CODE_TRIES);
        assert!(code_blocked("rc1", ip(1)));
        for i in 0..MAX_RECOVERY_TRIES {
            assert!(recovery_try("rc1", ip(1)), "{i}");
        }
        assert!(!recovery_try("rc1", ip(1)));
        assert!(recovery_try("rc1", ip(2)), "one address cannot use them up for the owner");
        assert!(recovery_try("rc2", ip(1)));
        for _ in 0..RECOVERY_BURST {
            recovery_try("rc3", ip(3));
        }
        assert!(recovery_burst("rc3"));
        assert!(!recovery_burst("rc2"));
    }

    #[tokio::test]
    async fn recovery_checks_wait_their_turn_and_the_first_in_line_goes_first() {
        use std::time::Duration as D;
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_RECOVERY_CHECKS));
        let turn = |n: u8, wait: D| {
            let sem = sem.clone();
            async move { turn_among(sem, "rq1", ip(n), wait).await }
        };
        let (one, two) = (turn(31, D::from_secs(5)).await.unwrap(), turn(32, D::from_secs(5)).await.unwrap());
        assert!(turn(33, D::from_millis(50)).await.is_none(), "no turn in time: nothing was checked");
        assert!(!state().lock().unwrap().recovery_waiting.contains_key(&("rq1".to_string(), ip(33))), "and no place left behind");
        // First come, first served: the second gets in line well after the first.
        let order = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut waits = Vec::new();
        for n in [33u8, 34] {
            let (t, order) = (turn(n, D::from_secs(5)), order.clone());
            waits.push(tokio::spawn(async move {
                let got = t.await;
                order.lock().unwrap().push(n);
                got
            }));
            tokio::time::sleep(D::from_millis(50)).await;
        }
        // A permit stays taken for as long as it is held, wherever that is.
        tokio::task::spawn_blocking(move || drop(one)).await.unwrap();
        tokio::time::sleep(D::from_millis(100)).await;
        assert_eq!(*order.lock().unwrap(), vec![33], "one freed: the first in line goes");
        drop(two);
        for w in waits {
            assert!(w.await.unwrap().is_some());
        }
        assert_eq!(*order.lock().unwrap(), vec![33, 34]);
        assert_eq!(sem.available_permits(), MAX_RECOVERY_CHECKS);
    }

    #[tokio::test]
    async fn the_limit_holds_while_someone_who_left_is_still_being_checked() {
        use std::time::Duration as D;
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let turn = turn_among(sem.clone(), "rq3", ip(37), D::from_secs(5)).await.unwrap();
        // As a check does: the turn goes along into the work, and the one who asked is gone.
        let check = tokio::task::spawn_blocking(move || {
            let _turn = turn;
            std::thread::sleep(D::from_millis(200));
        });
        drop(check);
        assert!(turn_among(sem.clone(), "rq3", ip(38), D::from_millis(20)).await.is_none(), "still running");
        assert!(turn_among(sem, "rq3", ip(38), D::from_secs(5)).await.is_some(), "and then free");
    }

    #[tokio::test]
    async fn one_address_holds_only_so_many_places_in_the_queue() {
        use std::time::Duration as D;
        let a = ip(35);
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_RECOVERY_WAITING));
        let mut held = Vec::new();
        for _ in 0..MAX_RECOVERY_WAITING {
            held.push(turn_among(sem.clone(), "rq2", a, D::from_secs(5)).await.unwrap());
        }
        assert!(turn_among(sem.clone(), "rq2", a, D::from_secs(30)).await.is_none(), "refused at once");
        assert!(turn_among(sem.clone(), "rq2other", a, D::from_millis(10)).await.is_none(), "no permit left, but it was let in line");
        assert_eq!(state().lock().unwrap().recovery_waiting.get(&("rq2".to_string(), a)).copied(), Some(MAX_RECOVERY_WAITING));
        drop(held);
        assert!(!state().lock().unwrap().recovery_waiting.contains_key(&("rq2".to_string(), a)), "nothing left behind");
    }

    #[tokio::test]
    async fn password_checks_wait_in_line_and_the_line_is_bounded() {
        use std::time::Duration as D;
        use std::sync::Arc;
        use tokio::sync::Semaphore;
        let (places, checks) = (Arc::new(Semaphore::new(3)), Arc::new(Semaphore::new(1)));
        let turn = |wait: D| password_turn_among(places.clone(), checks.clone(), wait);
        let first = turn(D::from_secs(5)).await.unwrap();
        assert!(turn(D::from_millis(20)).await.is_none(), "its wait ran out");
        assert_eq!(places.available_permits(), 2, "and it holds no place after that");
        let mut waiting = Vec::new();
        for _ in 0..2 {
            let t = turn(D::from_secs(5));
            waiting.push(tokio::spawn(async move { t.await.is_some() }));
            tokio::time::sleep(D::from_millis(20)).await;
        }
        let t0 = std::time::Instant::now();
        assert!(turn(D::from_secs(30)).await.is_none());
        assert!(t0.elapsed() < D::from_secs(5), "the line is full: refused at once");
        // A check that outlives whoever asked keeps its turn.
        let done = tokio::task::spawn_blocking(move || {
            let _turn = first;
            std::thread::sleep(D::from_millis(100));
        });
        drop(done);
        assert_eq!(checks.available_permits(), 0);
        for w in waiting {
            assert!(w.await.unwrap());
        }
        assert_eq!((places.available_permits(), checks.available_permits()), (3, 1), "nothing left behind");
    }

    #[tokio::test]
    async fn a_locked_address_gets_no_check_and_busy_is_no_wrong_password() {
        let a = IpAddr::from([198, 51, 100, 43]);
        for _ in 0..MAX_FAILS {
            record_failure(a);
        }
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r2 = ran.clone();
        assert_eq!(password_check(a, move || r2.store(true, std::sync::atomic::Ordering::SeqCst)).await, Err(NotChecked::Locked));
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        let b = IpAddr::from([198, 51, 100, 44]);
        assert_eq!(password_check(b, || 7).await, Ok(7));
        assert!(!is_locked(b));
    }

    #[tokio::test]
    async fn a_password_checked_a_moment_ago_needs_no_turn() {
        let acc = vec![("bq1".to_string(), hash_password("pw-bq-1234"), false)];
        let b64 = |s: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, s);
        let right = format!("Basic {}", b64("bq1:pw-bq-1234"));
        assert!(check_without_hashing(&right, &acc).is_none(), "not checked yet");
        let ip = IpAddr::from([198, 51, 100, 45]);
        assert!(matches!(check_authorization_queued(&right, &acc, ip).await, Ok(Cred::Password(..))));
        assert!(matches!(check_without_hashing(&right, &acc), Some(Cred::Password(..))), "cached");
        assert!(matches!(check_without_hashing("Basic !!!", &acc), Some(Cred::Wrong)));
        assert!(matches!(check_without_hashing(&format!("Basic {}", b64("no-colon")), &acc), Some(Cred::Wrong)));
        assert!(matches!(check_without_hashing("Bearer nothing", &acc), Some(Cred::Wrong)));
        assert!(matches!(check_authorization_queued(&format!("Basic {}", b64("bq1:wrong-one-12")), &acc, ip).await, Ok(Cred::Wrong)));
    }

    #[test]
    fn a_recovery_try_never_checked_does_not_count() {
        let a = ip(36);
        for i in 0..MAX_RECOVERY_TRIES {
            assert!(recovery_try("rr1", a), "{i}");
            if i == MAX_RECOVERY_TRIES - 1 {
                recovery_refund("rr1", a);
            }
        }
        assert!(recovery_try("rr1", a), "the refunded one");
        assert!(!recovery_try("rr1", a));
    }

    #[test]
    fn the_count_of_recovery_tries_for_an_account_stops_at_a_burst() {
        for i in 0..(RECOVERY_BURST * 5) {
            recovery_try("rr2", IpAddr::from([198, 51, (i / 200) as u8, (i % 200) as u8]));
        }
        assert!(recovery_burst("rr2"));
        assert_eq!(state().lock().unwrap().recovery_all.get("rr2").unwrap().len(), RECOVERY_BURST);
    }

    #[test]
    fn a_session_has_only_so_many_wrong_proofs() {
        for _ in 0..20 {
            assert!(proof_begin("pf1", "s"));
            proof_refund("pf1", "s");
        }
        for i in 0..MAX_PROOF_TRIES {
            assert!(!proof_used_up("pf1", "s"), "{i}");
            assert!(proof_begin("pf1", "s"));
        }
        assert!(proof_used_up("pf1", "s"));
        assert!(!proof_begin("pf1", "s"), "parallel tries get no further");
        assert!(proof_begin("pf1", "other session"));
        assert!(proof_begin("pf2", "s"));
    }

    #[test]
    fn a_recovery_only_ticket_says_so() {
        let t = new_ticket("ro1", generation("ro1"), true).unwrap();
        assert!(held(&t).recovery_only);
        let t = new_ticket("ro1", generation("ro1"), false).unwrap();
        assert!(!held(&t).recovery_only);
    }

    #[test]
    fn a_warning_goes_out_once_in_a_while() {
        assert!(note_due("note1", 3600));
        assert!(!note_due("note1", 3600));
        assert!(note_due("note2", 3600));
        note_forget("note1");
        assert!(note_due("note1", 3600), "one that could not be sent does not use up the wait");
        // What is remembered goes when its wait is over.
        assert!(note_due("note3", 0));
        assert!(note_due("note4", 3600));
        assert!(!state().lock().unwrap().notes.contains_key("note3"));
    }

    #[test]
    fn demotion_ends_control_tokens_for_good() {
        let acc = vec![("dm1".to_string(), "x".to_string(), true)];
        let (a, _) = add_token("dm1", "control", true, generation("dm1")).unwrap().unwrap();
        let (v, _) = add_token("dm1", "watch", false, generation("dm1")).unwrap().unwrap();
        assert!(delete_admin_tokens("dm1"));
        assert!(token_identity(&format!("Bearer {a}"), &acc).is_none(), "gone, also when made an admin again");
        assert!(token_identity(&format!("Bearer {v}"), &acc).is_some(), "the watch-only one stays");
    }

    #[test]
    fn a_new_account_with_an_old_name_inherits_nothing() {
        let acc = vec![("nw1".to_string(), "x".to_string(), false)];
        let (t, _) = add_token("nw1", "old", false, generation("nw1")).unwrap().unwrap();
        assert!(clear_account_data("nw1"));
        assert!(token_identity(&format!("Bearer {t}"), &acc).is_none());
    }

    #[test]
    fn the_password_of_a_two_step_account_opens_nothing_but_sign_in() {
        let acc = vec![("ts1".to_string(), hash_password("pw-ts-1234"), true)];
        let h = basic("ts1", "pw-ts-1234");
        // The generation comes from before the slow check.
        let g = generation("ts1");
        assert_eq!(check_authorization(&h, &acc), Cred::Password(Identity { user: "ts1".into(), admin: true }, g), "before: now cached");
        assert_eq!(check_authorization(&h, &acc), Cred::Password(Identity { user: "ts1".into(), admin: true }, g), "cached");
        // Turning it on forgets what was cached.
        set_two_step("ts1");
        forget_user("ts1", None);
        assert_eq!(check_authorization(&h, &acc), Cred::TwoStepNeeded);
        assert_eq!(check_authorization(&h, &acc), Cred::TwoStepNeeded, "remembered, no Argon2 each time");
        assert_eq!(check_authorization(&basic("ts1", "wrong-pass"), &acc), Cred::Wrong, "a wrong one looks like any wrong one");
        assert_eq!(check_authorization("Bearer nonsense", &acc), Cred::Wrong);
        assert_eq!(check_authorization("Digest x", &acc), Cred::Wrong);
    }

    #[test]
    fn a_link_code_is_not_made_if_two_step_came_on_meanwhile() {
        let then = generation("lk1");
        assert!(new_link_signin_code("lk1", false, then).is_some());
        forget_user("lk1", None);
        assert!(new_link_signin_code("lk1", false, then).is_none());
    }

    #[test]
    fn device_tokens_are_shown_once_and_hashed() {
        let acc = vec![("dt1".to_string(), hash_password("pw-dt-1234"), false)];
        let (value, info) = add_token("dt1", "Pixel", false, generation("dt1")).unwrap().unwrap();
        assert!(value.starts_with(TOKEN_PREFIX) && value.len() == TOKEN_PREFIX.len() + 64, "256 bits");
        let json = serde_json::to_string(&list_tokens("dt1")).unwrap();
        assert!(!json.contains(&value) && !json.contains(&value[4..]), "a list never holds it");
        assert_eq!(info.scope, "viewer");
        assert!(!state().lock().unwrap().tokens.contains_key(&value), "kept by its hash");
        let bearer = format!("Bearer {value}");
        let Cred::Token(id, g) = check_authorization(&bearer, &acc) else { panic!() };
        assert_eq!((id.user.as_str(), id.admin, g), ("dt1", false, generation("dt1")));
        // As the password in Basic, for RTSP and old clients, under its own name only.
        assert!(matches!(check_authorization(&basic("dt1", &value), &acc), Cred::Token(..)));
        assert_eq!(check_authorization(&basic("dt2", &value), &acc), Cred::Wrong);
        assert_eq!(check_authorization("Bearer ast_0000", &acc), Cred::Wrong);
    }

    #[test]
    fn a_token_never_exceeds_the_account() {
        let admin = vec![("dt3".to_string(), "x".to_string(), true)];
        let viewer = vec![("dt3".to_string(), "x".to_string(), false)];
        let (a, _) = add_token("dt3", "script", true, generation("dt3")).unwrap().unwrap();
        let (v, _) = add_token("dt3", "phone", false, generation("dt3")).unwrap().unwrap();
        let rights = |tok: &str, acc: &[(String, String, bool)]| token_identity(&format!("Bearer {tok}"), acc).map(|x| x.0.admin);
        assert_eq!(rights(&a, &admin), Some(true));
        assert_eq!(rights(&v, &admin), Some(false), "a viewer token of an admin stays a viewer");
        assert_eq!(rights(&a, &viewer), Some(false), "admin rights go with the account's, at once");
        assert_eq!(rights(&a, &[]), None, "gone with the account");
        assert_eq!(list_tokens("dt3").len(), 2);
        assert_eq!(token_count("dt3"), 2);
    }

    #[test]
    fn revoking_a_token_ends_it_and_what_is_open_but_not_sessions() {
        let cfg = users("[[server.users]]\nusername = \"dt4\"\npassword = \"x\"\n");
        let acc = accounts(&cfg);
        let session = new_session(Identity { user: "dt4".into(), admin: true });
        let (t, info) = add_token("dt4", "HA", false, generation("dt4")).unwrap().unwrap();
        let (other, _) = add_token("dt4", "Other", false, generation("dt4")).unwrap().unwrap();
        let (_, g) = token_identity(&format!("Bearer {t}"), &acc).unwrap();
        assert!(still_allowed(&cfg, "dt4", g), "a stream opened with it");
        assert!(!revoke_token("someone-else", &info.id).unwrap(), "only the account's own");
        assert!(revoke_token("dt4", &info.id).unwrap());
        assert!(token_identity(&format!("Bearer {t}"), &acc).is_none());
        assert!(!still_allowed(&cfg, "dt4", g), "the stream ends");
        assert!(!revoke_token("dt4", &info.id).unwrap(), "already gone");
        assert!(session_identity(&session, &acc).is_some(), "still signed in");
        assert!(token_identity(&format!("Bearer {other}"), &acc).is_some(), "other tokens work");
    }

    #[test]
    fn tokens_survive_a_password_change_but_not_the_account() {
        let acc = vec![("dt5".to_string(), "x".to_string(), false)];
        let (t, _) = add_token("dt5", "HA", false, generation("dt5")).unwrap().unwrap();
        forget_user("dt5", None);
        assert!(token_identity(&format!("Bearer {t}"), &acc).is_some());
        delete_user_tokens("dt5");
        assert!(token_identity(&format!("Bearer {t}"), &acc).is_none());
        assert_eq!(token_count("dt5"), 0);
    }

    #[test]
    fn a_token_is_not_made_for_an_account_that_changed_meanwhile() {
        let then = generation("dt6");
        forget_user("dt6", None);
        assert!(add_token("dt6", "x", false, then).unwrap().is_none());
    }

    #[test]
    fn an_account_has_at_most_so_many_tokens() {
        for i in 0..MAX_TOKENS {
            add_token("dt7", &format!("t{i}"), false, generation("dt7")).unwrap().unwrap();
        }
        assert!(add_token("dt7", "one more", false, generation("dt7")).is_err());
    }

    #[test]
    fn a_stale_token_does_not_open_a_disabled_account() {
        let cfg = users("[[server.users]]\nusername = \"dt8\"\npassword = \"x\"\ndisabled = true\n[[server.users]]\nusername = \"dt9\"\npassword = \"pw-dt9-123\"\n");
        let (t, _) = add_token("dt8", "x", false, generation("dt8")).unwrap().unwrap();
        assert!(token_identity(&format!("Bearer {t}"), &accounts(&cfg)).is_none());
    }

    #[test]
    fn disabled_survives_saving_and_is_off_by_default() {
        let cfg = users(THREE);
        assert!(!cfg.server.users[0].disabled);
        assert!(cfg.server.users[2].disabled);
        let again = toml::to_string(&cfg).unwrap();
        assert_eq!(again.matches("disabled = true").count(), 1, "only where it is on");
        let back: crate::config::Config = toml::from_str(&again).unwrap();
        assert!(back.server.users[2].disabled);
    }

    #[test]
    fn sessions_basic_cache_and_lockout() {
        let acc = vec![("admin".to_string(), hash_password("pw"), true)];
        let t = new_session(Identity { user: "admin".into(), admin: true });
        assert_eq!(session_identity(&t, &acc).map(|i| i.admin), Some(true));
        assert!(session_identity(&t, &[]).is_none(), "deleted user loses access");
        end_session(&t);
        assert!(session_identity(&t, &acc).is_none());
        let hdr = format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "admin:pw"));
        assert!(basic_identity(&hdr, &acc).is_some());
        assert!(basic_identity(&hdr, &acc).is_some()); // cached
        assert!(basic_identity("Basic YWRtaW46eA==", &acc).is_none()); // admin:x
        // A password change forgets the cached login and other sessions.
        let keep = new_session(Identity { user: "admin".into(), admin: true });
        let other = new_session(Identity { user: "admin".into(), admin: true });
        forget_user("admin", Some(&keep));
        assert!(session_identity(&keep, &acc).is_some());
        assert!(session_identity(&other, &acc).is_none());
        let acc2 = vec![("admin".to_string(), hash_password("newpw"), true)];
        assert!(basic_identity(&hdr, &acc2).is_none(), "old password no longer accepted");
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        for i in 0..MAX_FAILS {
            assert_eq!(record_failure(ip), i + 1 == MAX_FAILS);
        }
        assert!(is_locked(ip));
        // Sign-in codes work once, for their user only.
        let c1 = new_pair_code("viewer");
        let c2 = new_pair_code("viewer");
        assert_eq!(take_pair_code(&c1), None, "a newer code replaces the old one");
        assert_eq!(take_pair_code(&c2), Some(("viewer".to_string(), true)));
        assert_eq!(take_pair_code(&c2), None, "used up");
        // From a linked server as a non-admin: the session is capped.
        let c3 = new_pair_code_capped("admin", false);
        assert_eq!(take_pair_code(&c3), Some(("admin".to_string(), false)));
        let capped = new_session(Identity { user: "admin".into(), admin: false });
        let acc3 = vec![("admin".to_string(), hash_password("x"), true)];
        assert!(!session_identity(&capped, &acc3).unwrap().admin, "a capped session stays non-admin");
        assert_eq!(cookie_token("a=1; as_session=abc; b=2"), Some("abc"));
        assert_eq!(cookie_token("as_session="), None);
        let mut h = axum::http::HeaderMap::new();
        h.append(axum::http::header::COOKIE, "a=1; as_session=stale".parse().unwrap());
        h.append(axum::http::header::COOKIE, "as_session=fresh".parse().unwrap());
        assert_eq!(cookie_tokens(&h), vec!["stale", "fresh"]);
    }

    #[test]
    fn last_use_is_noted_in_memory_but_written_rarely() {
        let ip: IpAddr = "192.0.2.5".parse().unwrap();
        let mut m = SessionMeta { saved_seen: 1000, last_seen: 1000, ..Default::default() };
        assert!(!m.note_use(ip, 1001), "a request does not write");
        assert_eq!((m.last_seen, m.ip), (1001, Some(ip)));
        assert!(!m.note_use("192.0.2.6".parse().unwrap(), 1000 + SEEN_SAVE_SECS - 1));
        assert_eq!(m.ip, Some("192.0.2.6".parse().unwrap()), "the address is current in memory");
        assert!(m.note_use(ip, 1000 + SEEN_SAVE_SECS), "every few minutes");
        assert!(!m.note_use(ip, 1000 + SEEN_SAVE_SECS + 1), "and then not again at once");
        assert!(m.note_use(ip, 1000 + 2 * SEEN_SAVE_SECS));
    }

    #[test]
    fn sessions_are_listed_by_an_id_that_is_not_the_token_or_its_hash() {
        let acc = vec![("ls1".to_string(), "x".to_string(), true), ("ls2".to_string(), "x".to_string(), true)];
        let a = new_session_with(Identity { user: "ls1".into(), admin: true }, SessionMeta { label: "Edge on Windows".into(), ..Default::default() });
        let b = new_session(Identity { user: "ls1".into(), admin: true });
        let other = new_session(Identity { user: "ls2".into(), admin: true });
        let list = list_sessions("ls1", Some(&a));
        assert_eq!(list.len(), 2);
        assert_eq!(list.iter().filter(|s| s.this).count(), 1);
        let mine = list.iter().find(|s| s.this).unwrap();
        assert_eq!(mine.meta.label, "Edge on Windows");
        for s in &list {
            assert!(s.id.starts_with("ses-") && s.id.len() == 20, "{}", s.id);
            for secret in [&a, &b, &token_hash(&a), &token_hash(&b)] {
                assert!(!s.id.contains(secret.as_str()));
            }
        }
        // Another account's id is no id of this one's; nothing changes.
        let others_id = list_sessions("ls2", None)[0].id.clone();
        assert!(matches!(end_session_of("ls1", &others_id, Some(&a)), Ok(None)));
        assert!(session_identity(&other, &acc).is_some());
        // Its own: gone, and what was open for the account ends.
        let g = generation("ls1");
        let gone = end_session_of("ls1", &mine.id, Some(&a)).unwrap().expect("there");
        assert!(gone.this);
        assert!(session_identity(&a, &acc).is_none());
        assert!(session_identity(&b, &acc).is_some());
        assert_ne!(generation("ls1"), g);
        assert!(matches!(end_session_of("ls1", &mine.id, Some(&a)), Ok(None)), "once");
    }

    #[test]
    fn a_session_from_before_the_list_has_no_details_and_keeps_working() {
        let acc = vec![("ls3".to_string(), "x".to_string(), false)];
        let (hash, session) = session_from_row(crate::db::StoredSession { hash: "oldhash".into(), user: "ls3".into(), admin: false, expires: now_secs() + 1000, created: 0, ip: String::new(), label: String::new(), device: String::new(), last_seen: 0 });
        assert_eq!((session.meta.created, session.meta.label.as_str(), session.meta.ip), (0, "", None));
        state().lock().unwrap().sessions.insert(hash, session);
        assert!(list_sessions("ls3", None).iter().any(|s| s.meta.created == 0 && s.meta.label.is_empty()));
        // (A session token whose hash that is cannot be made up here; use is noted by its hash.)
        let t = new_session(Identity { user: "ls3".into(), admin: false });
        session_seen(&t, "192.0.2.9".parse().unwrap());
        let seen = list_sessions("ls3", Some(&t)).into_iter().find(|s| s.this).unwrap();
        assert_eq!(seen.meta.ip, Some("192.0.2.9".parse().unwrap()));
        assert!(session_identity(&t, &acc).is_some());
    }
}
