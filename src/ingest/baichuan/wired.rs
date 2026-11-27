//! Continuous live video from an always-on Reolink (wired or Wi-Fi camera,
//! hub or recorder channel) over Baichuan on TCP port 9000: another way in
//! besides RTSP (see `engine::connect_way` for when it is used).
//!
//! One session per device (host:port), shared by every stream and channel
//! asked of it: Reolink firmware allows only about two Baichuan sessions, and
//! the Reolink app needs one. The same session is meant to carry camera
//! controls and talk later (commands go through `bc::write_msg`'s allow-list).
//!
//! The session thread signs in when a stream is first asked for and signs out
//! a few seconds after the last one stops, so a camera that falls back to
//! RTSP is not held by a second way at the same time.
//!
//! What it guards against:
//!   * preview replies are matched to the request (the camera answers with
//!     the request's message number, channel and stream byte), so one
//!     stream's packets never reach another;
//!   * after damaged or cut media no picture is passed on until the next
//!     keyframe;
//!   * a stream with no video is ended by the reader's stall check; a session
//!     with nothing at all from the camera for SILENT_LIMIT, or no video on
//!     any stream for VIDEO_LIMIT, is ended and signed in again;
//!   * a missed keep-alive is not an error while media flows: only silence is;
//!   * a refused sign-in is never tried again with the same credentials
//!     (Reolink locks an account out after a few);
//!   * a hub or recorder channel is streamed only when the device's own API
//!     shows it is not a battery camera (continuous video would drain it).
//!
//! Audio: AAC is passed on; ADPCM (some models) is not yet.
//!
//! Left open for later steps (not built): a hub session that stays signed in
//! on its own (a lease instead of IDLE_SIGN_OUT) to subscribe to motion and
//! open a battery channel's preview only on motion or a viewer, closing it
//! after a short idle. The channel check (`channel_kind`) is what keeps
//! battery channels out of continuous streaming today; such an on-demand
//! path would be a separate request kind, never the continuous `start`.

use super::avcc::AvccBridge;
use super::bc;
use super::client::{is_wouldblock, next_ticks, Session, SignInRefused};
use super::media::Demux;
use super::talk::{TalkCmd, TalkHandle, Talks};
use super::transport::TcpTransport;
use crate::ingest::recorder::{MediaItem, VideoParams};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

/// Which of a channel's encodings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamKind {
    Main,
    Sub,
    /// A third profile some models have.
    Extern,
}

impl StreamKind {
    fn xml(self) -> &'static str {
        match self {
            StreamKind::Main => "mainStream",
            StreamKind::Sub => "subStream",
            StreamKind::Extern => "externStream",
        }
    }
    /// The request's `handle` (as the official client sends it).
    fn handle(self) -> u32 {
        match self {
            StreamKind::Main => 0,
            StreamKind::Sub => 256,
            StreamKind::Extern => 1024,
        }
    }
    /// The header's stream byte; the camera's replies carry the same.
    fn code(self) -> u8 {
        match self {
            StreamKind::Main => 0,
            StreamKind::Sub => 1,
            StreamKind::Extern => 2,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            StreamKind::Main => "main",
            StreamKind::Sub => "sub",
            StreamKind::Extern => "extern",
        }
    }
}

/// One stream to read: the device, its channel (0-based: the lens of a
/// dual-lens camera, or the hub/recorder channel) and the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub channel: u8,
    pub stream: StreamKind,
    /// The camera (its name) that reads it: one device's session is shared
    /// by its cameras, and each may only stop or replace what is its own.
    pub owner: String,
}

impl Target {
    pub fn key(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
    pub fn describe(&self) -> String {
        format!("Baichuan {}:{} channel {} {} stream", self.host, self.port, self.channel, self.stream.label())
    }
}

/// The Baichuan target for a camera's RTSP address: the same host, the
/// Baichuan port, the channel (set, or from Reolink's `Preview_NN` /
/// `h264Preview_NN` path: NN-1), and the stream from its `_main` / `_sub` /
/// `_ext` ending (or `default`).
pub fn target_for(rtsp_url: &str, port: u16, channel: Option<u8>, default: StreamKind) -> Option<Target> {
    let rest = rtsp_url.split_once("://")?.1;
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let auth = &rest[..auth_end];
    let hostport = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
    let host = if let Some(b) = hostport.strip_prefix('[') {
        b.split_once(']')?.0.to_string()
    } else {
        hostport.split(':').next()?.to_string()
    };
    if host.is_empty() {
        return None;
    }
    let host = norm_host(&host);
    let path = rest[auth_end..].to_ascii_lowercase();
    let stream = if path.contains("_sub") {
        StreamKind::Sub
    } else if path.contains("_ext") {
        StreamKind::Extern
    } else if path.contains("_main") {
        StreamKind::Main
    } else {
        default
    };
    let channel = channel.or_else(|| {
        let i = path.find("preview_")? + "preview_".len();
        let n: String = path[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
        n.parse::<u16>().ok().filter(|&n| (1..=256).contains(&n)).map(|n| (n - 1) as u8)
    });
    Some(Target { host, port, channel: channel.unwrap_or(0), stream, owner: String::new() })
}

/// Whether a device needs the hub check before a channel is streamed: all
/// but a camera of its own (a type of the "ipc" kind, such as `ipc` or the
/// E1 Pro's `wifi_solo_ipc`, with one channel or its channel count not
/// given). A hub or recorder, an unknown type, or an ipc with several
/// channels (a dual-lens camera, refused for now: the safe side) is asked.
pub fn needs_hub_check(device_type: Option<&str>, channels: Option<u32>) -> bool {
    let t = device_type.unwrap_or_default().trim().to_ascii_lowercase();
    let ipc = t.contains("ipc") && !t.contains("hub") && !t.contains("nvr");
    !(ipc && channels.is_none_or(|n| n <= 1))
}

/// A host as one device's sessions are keyed (video and controls alike):
/// lower case, an IPv6 address in its one short form, no brackets.
pub fn norm_host(host: &str) -> String {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    match h.parse::<std::net::Ipv6Addr>() {
        Ok(a) => a.to_string(),
        Err(_) => h.to_ascii_lowercase(),
    }
}

// ---------------------------------------------------------------- hub channels

/// What a hub or recorder says about one of its channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelKind {
    /// Online, and the device lists no battery for it.
    Wired,
    /// A battery camera (or one that sleeps): never streamed continuously.
    Battery(String),
    /// Cannot tell (offline, or the device's API did not answer): not
    /// streamed either, since it might be a battery camera.
    Unknown(String),
}

/// Read a hub's `GetChannelstatus` and `GetAbility` answers (the Reolink
/// HTTP API, served by the hub without waking anything) for `channel`. A
/// channel counts as wired only when it is online, not asleep, and its
/// abilities list no battery.
pub fn channel_kind(answers: &[serde_json::Value], channel: u8) -> ChannelKind {
    let find = |cmd: &str| answers.iter().find(|a| a["cmd"] == cmd && a["code"].as_i64() == Some(0));
    let Some(status) = find("GetChannelstatus") else {
        return ChannelKind::Unknown("the hub did not give its channel list".into());
    };
    let Some(entry) = status["value"]["status"].as_array().and_then(|l| l.iter().find(|e| e["channel"].as_u64() == Some(channel as u64))) else {
        return ChannelKind::Unknown(format!("the hub does not list channel {channel}"));
    };
    if entry["sleep"].as_i64() == Some(1) {
        return ChannelKind::Battery(format!("channel {channel} is asleep, so it is a battery camera"));
    }
    if entry["online"].as_i64() != Some(1) {
        return ChannelKind::Unknown(format!("channel {channel} is offline, so the hub cannot say whether it is a battery camera"));
    }
    let Some(ab) = find("GetAbility") else {
        return ChannelKind::Unknown("the hub did not give its abilities".into());
    };
    let Some(chn) = ab["value"]["Ability"]["abilityChn"].as_array().and_then(|l| l.get(channel as usize)) else {
        return ChannelKind::Unknown(format!("the hub gives no abilities for channel {channel}"));
    };
    let battery = &chn["battery"];
    if battery.is_null() {
        return ChannelKind::Unknown(format!("the hub does not say whether channel {channel} has a battery"));
    }
    if battery["ver"].as_i64().unwrap_or(1) > 0 || battery["permit"].as_i64().unwrap_or(1) > 0 {
        return ChannelKind::Battery(format!("the hub lists a battery for channel {channel}"));
    }
    ChannelKind::Wired
}

/// The words a refusal for a battery channel always has (the worker looks
/// for them: such a refusal is final, never retried).
pub const BATTERY_REFUSAL: &str = "continuous Baichuan video is not for battery cameras";

pub fn is_battery_refusal(e: &str) -> bool {
    e.contains(BATTERY_REFUSAL)
}

/// Ask a hub's HTTP API what `channel` is (blocking, a few seconds at most).
fn ask_hub(host: &str, user: &str, pass: &str, channel: u8) -> ChannelKind {
    let mut c = crate::reolink::ReolinkClient::new(host, user, pass);
    let cmds = serde_json::json!([
        { "cmd": "GetChannelstatus", "action": 0, "param": {} },
        { "cmd": "GetAbility", "action": 0, "param": { "User": { "userName": user } } }
    ]);
    let r = c.batch(&cmds);
    c.logout();
    match r {
        Ok(a) => channel_kind(&a, channel),
        Err(e) => ChannelKind::Unknown(format!("the hub's API did not answer ({})", e.to_string().lines().next().unwrap_or_default())),
    }
}

// ---------------------------------------------------------------- the client

/// What the session is doing, for status and the camera report.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub signed_in: bool,
    /// The last session's encryption: Some(true) AES, Some(false) not AES.
    pub aes: Option<bool>,
    /// The device's type ("ipc", "homehub", "nvr"...) and channel count.
    pub device_type: Option<String>,
    pub channels: Option<u32>,
    pub firmware: Option<String>,
    /// The sign-in was refused: never asked again with these credentials.
    pub refused: Option<String>,
    pub last_error: Option<String>,
    pub streams: usize,
    /// The cameras with a stream running (or waiting to start) on it.
    pub owners: Vec<String>,
    /// Signing in now, for this camera's first stream.
    pub signing_in_for: Option<String>,
    pub sessions: u64,
    /// The camera reported a battery when it signed in: it is a battery
    /// camera, never streamed continuously.
    pub battery: bool,
    /// Preview packets that matched no request (dropped).
    pub unmatched: u64,
    /// Times damaged media held a stream's picture until the next keyframe.
    pub damaged: u64,
}

struct Shared {
    status: Mutex<Status>,
    alive: AtomicBool,
    /// Signed out and the thread gone (for `release`).
    done: AtomicBool,
    next_id: AtomicU64,
    /// Audio configs (AudioSpecificConfig) seen per channel+stream: a later
    /// stream does not wait for audio to learn it again.
    audio: Mutex<HashMap<(u8, StreamKind), Option<Vec<u8>>>>,
    /// Hub channels checked: (when, verdict).
    channel_checks: Mutex<HashMap<u8, (Instant, ChannelKind)>>,
    /// Talks (two-way audio) on this session.
    talks: Mutex<Talks>,
    /// Hub channels being asked about now (off the session thread).
    checking: Mutex<std::collections::HashSet<u8>>,
}

enum Cmd {
    Start { channel: u8, stream: StreamKind, owner: String, reply: SyncSender<Result<WiredPreview, String>> },
    Stop { id: u64 },
    /// A camera control's exchange (`ctl.rs`).
    Control(super::ctl::Request),
    /// Talk (see `super::talk`).
    Talk(TalkCmd),
    Shutdown,
}

pub struct WiredClient {
    user: String,
    /// A fingerprint of the password (never the password) to tell a
    /// settings change from the same settings.
    pass_tag: u64,
    cmd_tx: Mutex<Sender<Cmd>>,
    shared: Arc<Shared>,
}

/// Item channel of one preview.
type Items = Result<MediaItem, String>;

/// One running stream. Dropping it stops the stream on the camera.
pub struct WiredPreview {
    pub rx: UnboundedReceiver<Items>,
    params: Arc<Mutex<Option<VideoParams>>>,
    audio: Arc<Mutex<Option<Vec<u8>>>>,
    id: u64,
    client: Arc<WiredClient>,
}

impl WiredPreview {
    pub fn video_params(&self) -> Option<VideoParams> {
        self.params.lock().unwrap().clone()
    }
    /// The AAC AudioSpecificConfig, once audio has come.
    pub fn audio_config(&self) -> Option<Vec<u8>> {
        self.audio.lock().unwrap().clone()
    }
}

impl Drop for WiredPreview {
    fn drop(&mut self) {
        let _ = self.client.cmd_tx.lock().unwrap().send(Cmd::Stop { id: self.id });
    }
}

fn tag_of(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

fn table() -> &'static Mutex<HashMap<String, Arc<WiredClient>>> {
    static T: OnceLock<Mutex<HashMap<String, Arc<WiredClient>>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// What to do about the running session when `owner` asks for one with
/// these credentials.
#[derive(Debug, PartialEq, Eq)]
enum Reuse {
    Reuse,
    /// Other credentials, and no other camera uses the session: replace it
    /// (a settings change).
    Replace,
    /// Other credentials while another camera uses it: refused, or the two
    /// would replace each other's session over and over.
    Conflict(String),
}

fn reuse(same_creds: bool, alive: bool, st: &Status, owner: &str) -> Reuse {
    if same_creds && alive {
        return Reuse::Reuse;
    }
    if !alive {
        return Reuse::Replace;
    }
    let others: Vec<&String> = st.owners.iter().chain(st.signing_in_for.iter()).filter(|o| o.as_str() != owner).collect();
    match others.first() {
        Some(o) => Reuse::Conflict(format!(
            "another camera ('{o}') uses different credentials for this device; give both the same user name and password"
        )),
        None => Reuse::Replace,
    }
}

/// Keep a client's refusal so a new client with the same credentials does
/// not sign in again.
fn keep_refusal(key: &str, c: &WiredClient) {
    if let Some(r) = c.shared.status.lock().unwrap().refused.clone() {
        REFUSED.lock().unwrap().get_or_insert_with(Default::default).insert(key.to_string(), (c.user.clone(), c.pass_tag, r));
    }
}

/// The session for `host:port`, started if needed, for camera `owner`.
/// Other credentials than the running session's replace it when no other
/// camera uses it (a settings change: the old one is signed out first),
/// and are refused when another does.
pub fn client_for(host: &str, port: u16, user: &str, pass: &str, owner: &str) -> Result<Arc<WiredClient>, String> {
    let key = format!("{host}:{port}");
    let old = {
        let mut t = table().lock().unwrap();
        match t.get(&key) {
            None => None,
            Some(c) => {
                let st = c.shared.status.lock().unwrap().clone();
                match reuse(c.user == user && c.pass_tag == tag_of(pass), c.shared.alive.load(Ordering::Relaxed), &st, owner) {
                    Reuse::Reuse => return Ok(c.clone()),
                    Reuse::Conflict(why) => return Err(why),
                    Reuse::Replace => t.remove(&key),
                }
            }
        }
    };
    if let Some(c) = old {
        // Never two sessions at once: the old one signs out first.
        c.shutdown();
        let until = Instant::now() + Duration::from_secs(8);
        while !c.shared.done.load(Ordering::Relaxed) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        keep_refusal(&key, &c);
    }
    let mut t = table().lock().unwrap();
    if let Some(c) = t.get(&key) {
        if c.user == user && c.pass_tag == tag_of(pass) && c.shared.alive.load(Ordering::Relaxed) {
            return Ok(c.clone()); // another stream of this camera got there first
        }
        return Err("the Baichuan session to this device is being replaced; trying again shortly".into());
    }
    let c = WiredClient::spawn(host, port, user, pass);
    t.insert(key, c.clone());
    Ok(c)
}

/// The camera's settings were saved (its worker starts again): a refused
/// sign-in to `host:port` may be tried once more. Never done on its own.
pub fn forget_refusal(host: &str, port: u16) {
    let key = format!("{host}:{port}");
    if let Some(m) = REFUSED.lock().unwrap().as_mut() {
        m.remove(&key);
    }
    let gone = {
        let mut t = table().lock().unwrap();
        let refused = t.get(&key).is_some_and(|c| c.shared.status.lock().unwrap().refused.is_some());
        if refused { t.remove(&key) } else { None }
    };
    if let Some(c) = gone {
        c.shutdown();
    }
}

/// Longer than the slowest way the session can answer a talk: connect (5 s)
/// and sign in (up to 12 s), the hub check (HUB_CHECK_WAIT), then the talk
/// question (`talk::ASK_TIMEOUT`).
const TALK_OPEN_WAIT: Duration = Duration::from_secs(45);

/// Sign the session to `host:port` out now and wait (up to `wait`) until it
/// has: for a one-off tool (`talk-test`) that must not leave a session behind.
pub fn sign_out(host: &str, port: u16, wait: Duration) -> bool {
    let Some(c) = table().lock().unwrap().remove(&format!("{host}:{port}")) else { return true };
    c.shutdown();
    let until = Instant::now() + wait;
    while !c.shared.done.load(Ordering::Relaxed) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    c.shared.done.load(Ordering::Relaxed)
}

/// The session's status for `host:port`, if one was started.
pub fn status(host: &str, port: u16) -> Option<Status> {
    table().lock().unwrap().get(&format!("{host}:{port}")).map(|c| c.shared.status.lock().unwrap().clone())
}

/// What `release` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Released {
    /// No Baichuan session of this camera is signed in.
    Free,
    /// The session stays for other cameras of the same device (other hub
    /// channels); none of this camera's streams are on it.
    OthersOnly,
    /// This camera's streams (or its sign-in) did not end in time.
    Busy,
}

/// Make sure camera `owner` has nothing signed in on the Baichuan session to
/// `host:port` before another way (RTSP) is used. A session with no streams
/// left is signed out now and awaited (up to `wait`). One serving other
/// cameras of the same device (or signing in for one) is left alone.
pub fn release(host: &str, port: u16, owner: &str, wait: Duration) -> Released {
    let key = format!("{host}:{port}");
    let c = {
        let t = table().lock().unwrap();
        match t.get(&key) {
            Some(c) => c.clone(),
            None => return Released::Free,
        }
    };
    let until = Instant::now() + wait;
    let mine = |st: &Status| st.owners.iter().chain(st.signing_in_for.iter()).any(|o| o == owner);
    let others = |st: &Status| st.owners.iter().chain(st.signing_in_for.iter()).any(|o| o != owner);
    // A preview just dropped reaches the thread a moment later.
    loop {
        let st = c.shared.status.lock().unwrap().clone();
        if !mine(&st) {
            if others(&st) || st.streams > 0 {
                return Released::OthersOnly;
            }
            break;
        }
        if Instant::now() >= until {
            return Released::Busy;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // A session the camera controls use carries no video: left alone (it
    // signs out by itself a minute after the last control).
    if super::ctl::recent(&key) {
        return Released::OthersOnly;
    }
    {
        let mut t = table().lock().unwrap();
        if t.get(&key).is_some_and(|x| Arc::ptr_eq(x, &c)) {
            t.remove(&key);
        }
    }
    c.shutdown();
    while !c.shared.done.load(Ordering::Relaxed) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    // A refusal is kept across the release: the same credentials are not
    // tried again (the next client_for gets it back).
    keep_refusal(&key, &c);
    if c.shared.done.load(Ordering::Relaxed) {
        Released::Free
    } else {
        Released::Busy
    }
}

/// Refused sign-ins kept across a released session: (user, password tag, why).
static REFUSED: Mutex<Option<HashMap<String, (String, u64, String)>>> = Mutex::new(None);

impl WiredClient {
    fn spawn(host: &str, port: u16, user: &str, pass: &str) -> Arc<WiredClient> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let pass_tag = tag_of(pass);
        let key = format!("{host}:{port}");
        let refused = REFUSED
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .get(&key)
            .filter(|(u, t, _)| u == user && *t == pass_tag)
            .map(|(_, _, why)| why.clone());
        let shared = Arc::new(Shared {
            status: Mutex::new(Status { refused, ..Default::default() }),
            alive: AtomicBool::new(true),
            done: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            audio: Mutex::new(HashMap::new()),
            channel_checks: Mutex::new(HashMap::new()),
            talks: Mutex::new(Talks::new(&key)),
            checking: Mutex::new(Default::default()),
        });
        let c = Arc::new(WiredClient {
            user: user.to_string(),
            pass_tag,
            cmd_tx: Mutex::new(cmd_tx),
            shared: shared.clone(),
        });
        let (h, u, p) = (host.to_string(), user.to_string(), pass.to_string());
        let weak = Arc::downgrade(&c);
        std::thread::Builder::new()
            .name(format!("baichuan tcp {host}:{port}"))
            .spawn(move || {
                run(&h, port, &u, &p, cmd_rx, &shared, weak);
                shared.done.store(true, Ordering::Relaxed);
            })
            .expect("spawn baichuan tcp thread");
        c
    }

    /// Start a stream. Blocks (call off the async runtime) until the camera
    /// sends it, refuses it, or START_TIMEOUT passes.
    pub fn start(self: &Arc<Self>, channel: u8, stream: StreamKind, owner: &str) -> Result<WiredPreview, String> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.cmd_tx
            .lock()
            .unwrap()
            .send(Cmd::Start { channel, stream, owner: owner.to_string(), reply })
            .map_err(|_| "the Baichuan session has stopped".to_string())?;
        rx.recv_timeout(START_TIMEOUT + Duration::from_secs(10))
            .map_err(|_| format!("the camera did not start the {} stream within {} s", stream.label(), START_TIMEOUT.as_secs()))?
    }

    /// One camera-control exchange on this session (signed in for it when
    /// needed). Blocks (call off the async runtime) until the camera answers,
    /// or the sign-in and the answer took too long.
    /// `urgent`: ahead of what waits (the siren's stop). `keep`: keeps a
    /// controls-only session signed in a while (not for a page's state read).
    #[allow(clippy::too_many_arguments)]
    pub fn control(&self, msg_id: u32, channel: u8, ext: String, body: String, write: bool, urgent: bool, keep: bool) -> Result<super::ctl::Answer, String> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        // (Never sent past this: then nobody waits for it any more.)
        let deadline = Instant::now() + START_TIMEOUT + super::ctl::ANSWER_WAIT;
        self.cmd_tx
            .lock()
            .unwrap()
            .send(Cmd::Control(super::ctl::Request { msg_id, channel, ext, body, write, urgent, keep, deadline, reply, then: Vec::new(), hub_ok: false }))
            .map_err(|_| format!("the Baichuan session has stopped; {}", super::ctl::NOT_SENT))?;
        rx.recv_timeout(START_TIMEOUT + super::ctl::ANSWER_WAIT + Duration::from_secs(2)).map_err(|_| "the camera did not answer in time".to_string())?
    }

    /// A control exchange for the siren's sound: `then` are more messages of
    /// the same job (each sent once the one before was answered with 200,
    /// nothing else in between), all done within `within` (past the sign-in).
    /// `hub_ok`: it may go to a channel of a hub (the caller saw it awake).
    /// `keep`: it keeps a controls-only session signed in (not a page's read).
    /// The answer is the last message's, or the first that was not 200.
    #[allow(clippy::too_many_arguments)]
    pub fn control_job(&self, msg_id: u32, channel: u8, ext: String, body: String, then: Vec<super::ctl::Step>, write: bool, hub_ok: bool, keep: bool, within: Duration) -> Result<super::ctl::Answer, String> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        let deadline = Instant::now() + START_TIMEOUT + within;
        self.cmd_tx
            .lock()
            .unwrap()
            .send(Cmd::Control(super::ctl::Request { msg_id, channel, ext, body, write, urgent: false, keep, deadline, reply, then, hub_ok }))
            .map_err(|_| format!("the Baichuan session has stopped; {}", super::ctl::NOT_SENT))?;
        rx.recv_timeout(START_TIMEOUT + within + Duration::from_secs(2)).map_err(|_| "the camera did not answer in time".to_string())?
    }

    /// What the session knows of the device (after a sign-in).
    pub fn session_status(&self) -> Status {
        self.shared.status.lock().unwrap().clone()
    }

    /// Open a talk on `channel`: asks the camera what talk audio it takes
    /// (read-only); the talk itself opens on the first sound. Blocks (call
    /// off the async runtime) for a few seconds at most.
    pub fn talk_open(self: &Arc<Self>, channel: u8) -> Result<TalkHandle, String> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.cmd_tx
            .lock()
            .unwrap()
            .send(Cmd::Talk(TalkCmd::Open { channel, reply }))
            .map_err(|_| "the Baichuan session has stopped".to_string())?;
        let o = rx
            .recv_timeout(TALK_OPEN_WAIT)
            .map_err(|_| format!("the camera did not answer about talk within {} s", TALK_OPEN_WAIT.as_secs()))??;
        let c = self.clone();
        Ok(TalkHandle { id: o.id, format: o.format, stats: o.stats, send: Box::new(move |cmd| c.cmd_tx.lock().unwrap().send(Cmd::Talk(cmd)).is_ok()) })
    }

    pub fn shutdown(&self) {
        self.shared.alive.store(false, Ordering::Relaxed);
        let _ = self.cmd_tx.lock().unwrap().send(Cmd::Shutdown);
    }
}

/// How long a stream may take to start (sign-in included).
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// Sign out once no stream has been running this long.
const IDLE_SIGN_OUT: Duration = Duration::from_secs(5);
/// Nothing at all from the camera this long: the session is dead.
const SILENT_LIMIT: Duration = Duration::from_secs(20);
/// Streams running but no video on any of them this long: sign in again.
const VIDEO_LIMIT: Duration = Duration::from_secs(20);
/// Send a keep-alive of our own when the camera has been quiet this long.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(5);
/// How long a hub's word on a channel is trusted.
const CHANNEL_CHECK_FOR: Duration = Duration::from_secs(10 * 60);

fn run(host: &str, port: u16, user: &str, pass: &str, cmd_rx: Receiver<Cmd>, shared: &Arc<Shared>, me: std::sync::Weak<WiredClient>) {
    let mut backoff = Duration::from_secs(2);
    let mut retry_at: Option<Instant> = None;
    while shared.alive.load(Ordering::Relaxed) {
        let first = match cmd_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(c @ (Cmd::Start { .. } | Cmd::Control(_))) | Ok(c @ Cmd::Talk(TalkCmd::Open { .. })) => c,
            Ok(Cmd::Stop { .. }) | Ok(Cmd::Talk(_)) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Ok(Cmd::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let refuse = |c: Cmd, why: String| match c {
            Cmd::Start { reply, .. } => {
                let _ = reply.send(Err(why));
            }
            Cmd::Control(r) => r.fail(why),
            Cmd::Talk(TalkCmd::Open { reply, .. }) => {
                let _ = reply.send(Err(why));
            }
            _ => {}
        };
        if let Some(r) = shared.status.lock().unwrap().refused.clone() {
            refuse(first, r);
            continue;
        }
        if retry_at.is_some_and(|t| Instant::now() < t) {
            let e = shared.status.lock().unwrap().last_error.clone().unwrap_or_default();
            refuse(first, format!("Baichuan session to {host}:{port} is waiting to reconnect ({e})"));
            continue;
        }
        if let Cmd::Start { owner, .. } = &first {
            shared.status.lock().unwrap().signing_in_for = Some(owner.clone());
        }
        let session = TcpTransport::connect(host, port, Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("Baichuan TCP connect to {host}:{port} failed: {e}"))
            .and_then(|t| Session::sign_in(Box::new(t), host, user, pass));
        shared.status.lock().unwrap().signing_in_for = None;
        match session {
            Ok(mut s) => {
                backoff = Duration::from_secs(2);
                retry_at = None;
                {
                    let mut st = shared.status.lock().unwrap();
                    st.signed_in = true;
                    st.aes = Some(s.uses_aes);
                    st.sessions += 1;
                    st.device_type = bc::xml_tag(&s.login_xml, "type").map(str::to_string);
                    st.channels = bc::xml_tag(&s.login_xml, "channelNum").and_then(|v| v.parse().ok());
                    st.firmware = bc::xml_tag(&s.login_xml, "firmVersion").map(str::to_string);
                    st.battery = s.has_battery();
                    st.last_error = None;
                }
                let mut subs = Vec::new();
                let mut ctl = super::ctl::Queue::new(&format!("{host}:{port}"));
                let why = serve(&mut s, &mut subs, &mut ctl, first, &cmd_rx, shared, host, user, pass, &me);
                shared.talks.lock().unwrap().end_all(&match &why {
                    Ok(w) => format!("the Baichuan session ended ({w})"),
                    Err(e) => format!("the Baichuan session ended: {e}"),
                });
                ctl.fail_all(&match &why {
                    Ok(w) => format!("the Baichuan session ended ({w})"),
                    Err(e) => e.clone(),
                });
                if let Err(e) = &why {
                    // Every stream is told why it ended.
                    for sub in subs.drain(..) {
                        match sub.reply {
                            Some((r, _)) => {
                                let _ = r.send(Err(e.clone()));
                            }
                            None => {
                                let _ = sub.tx.send(Err(e.clone()));
                            }
                        }
                    }
                }
                let _ = s.logout();
                std::thread::sleep(Duration::from_millis(100));
                let mut st = shared.status.lock().unwrap();
                st.signed_in = false;
                st.streams = 0;
                st.owners.clear();
                match why {
                    Ok(w) => info!("baichuan {host}:{port}: signed out ({w})"),
                    Err(e) => {
                        warn!("baichuan {host}:{port}: session ended: {e}");
                        st.last_error = Some(e);
                        // Reconnect soon, but never in a tight loop.
                        retry_at = Some(Instant::now() + Duration::from_secs(1));
                    }
                }
            }
            Err(e) => {
                let refused = e.downcast_ref::<SignInRefused>().is_some();
                let msg = format!("{e:#}");
                let mut st = shared.status.lock().unwrap();
                if refused {
                    warn!("baichuan {host}:{port}: {msg}; not asking again with these credentials");
                    st.refused = Some(msg.clone());
                } else {
                    debug!("baichuan {host}:{port}: {msg}; next try in {} s", backoff.as_secs());
                    retry_at = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                st.last_error = Some(msg.clone());
                drop(st);
                refuse(first, msg);
            }
        }
    }
    shared.alive.store(false, Ordering::Relaxed);
}

/// One running stream on the session.
struct Sub {
    id: u64,
    owner: String,
    channel: u8,
    stream: StreamKind,
    msg_num: u16,
    tx: UnboundedSender<Items>,
    /// Waiting for the camera's first answer, until this time.
    reply: Option<(SyncSender<Result<WiredPreview, String>>, Instant)>,
    /// The handle, sent with the first answer.
    handle: Option<WiredPreview>,
    params: Arc<Mutex<Option<VideoParams>>>,
    audio_cfg: Arc<Mutex<Option<Vec<u8>>>>,
    pipe: Pipe,
}

/// The per-stream media path: BcMedia -> AVCC frames with 90 kHz times and
/// AAC frames, with the keyframe hold and the wait for audio.
struct Pipe {
    demux: Demux,
    bridge: AvccBridge,
    last_ticks: i64,
    last_micros: Option<u32>,
    /// Pass no picture until a keyframe (at the start, and after damage).
    need_key: bool,
    /// Times a hold was started by damage.
    pub holds: u64,
    /// Before the first item goes out: what the camera's audio is.
    /// Some(None) = it has none (or it is not known to have any).
    audio_known: Option<Option<Vec<u8>>>,
    /// Items held while audio is waited for (from the first keyframe).
    warm: Vec<MediaItem>,
    released: bool,
    audio_rate: u32,
    audio_ticks: Option<i64>,
    pub got_video: bool,
    pub last_video: Instant,
}

/// The longest wait for audio before the stream starts without it.
const AUDIO_WAIT_TICKS: i64 = 90_000 * 3 / 2;

impl Pipe {
    fn new(audio_known: Option<Option<Vec<u8>>>, now: Instant) -> Self {
        let mut demux = Demux::new();
        demux.keep_audio = true;
        Pipe {
            demux,
            bridge: AvccBridge::new(false, 0, 0),
            last_ticks: -1,
            last_micros: None,
            need_key: true,
            holds: 0,
            audio_known,
            warm: Vec::new(),
            released: false,
            audio_rate: 0,
            audio_ticks: None,
            got_video: false,
            last_video: now,
        }
    }

    /// Damage: no picture until a keyframe that comes after it.
    fn hold(&mut self) {
        if !self.need_key {
            self.holds += 1;
        }
        self.need_key = true;
    }

    /// The media stream broke (a damaged BC message): hold the picture,
    /// and start the demuxer over (its half-read frame is lost).
    fn damaged(&mut self) {
        self.hold();
        self.demux = {
            let mut d = Demux::new();
            d.keep_audio = true;
            d.width = self.demux.width;
            d.height = self.demux.height;
            d
        };
    }

    /// Feed one message's payload; returns what is ready to go out, and
    /// whether the video parameters or audio config changed.
    fn push(&mut self, pay: &[u8], params: &Mutex<Option<VideoParams>>, audio_cfg: &Mutex<Option<Vec<u8>>>, now: Instant) -> Vec<MediaItem> {
        let mut out = Vec::new();
        let frames = self.demux.push(pay);
        for frame in frames {
            // Damage is placed frame by frame: only a keyframe that comes
            // after it ends the hold.
            if frame.after_damage {
                self.hold();
            }
            if frame.cut {
                // Cut short (its size field says more than came before the
                // next frame): never shown, even a keyframe. Its time still
                // counts below, so the next frames keep their place.
                self.hold();
                let ticks = next_ticks(self.last_ticks, self.last_micros, frame.micros);
                self.last_ticks = ticks;
                self.last_micros = Some(frame.micros);
                continue;
            }
            if self.demux.h265 != self.bridge.is_h265() {
                self.bridge = AvccBridge::new(self.demux.h265, self.demux.width, self.demux.height);
            }
            self.bridge.set_dimensions(self.demux.width, self.demux.height);
            let ticks = next_ticks(self.last_ticks, self.last_micros, frame.micros);
            self.last_ticks = ticks;
            self.last_micros = Some(frame.micros);
            let Some(bf) = self.bridge.push_frame(&frame.annexb) else { continue };
            if bf.new_params {
                *params.lock().unwrap() = self.bridge.video_params();
            }
            if self.need_key {
                if !bf.key || self.bridge.video_params().is_none() {
                    continue; // no picture until a whole keyframe
                }
                self.need_key = false;
            }
            self.got_video = true;
            self.last_video = now;
            // After a hold the first keyframe re-signals nothing new: the
            // parameters are the same (or new_params says they are not).
            out.push(MediaItem::Video { ticks, cts: 0, data: bf.data, key: bf.key, new_params: bf.new_params });
        }
        for adts in self.demux.take_audio() {
            let Some((asc, rate)) = adts_config(&adts) else { continue };
            let Some(raw) = crate::ingest::aac::raw_frame(&adts) else { continue };
            if audio_cfg.lock().unwrap().as_deref() != Some(&asc) {
                *audio_cfg.lock().unwrap() = Some(asc.clone());
            }
            if self.audio_known.is_none() || matches!(self.audio_known, Some(None)) {
                self.audio_known = Some(Some(asc));
            }
            if !self.got_video {
                continue; // audio before the first picture: dropped
            }
            // Timed by its samples (1024 a frame), started where the video
            // is; put back in step if they drift apart by over 2 s.
            let video_at = self.last_ticks * rate as i64 / 90_000;
            let t = match self.audio_ticks {
                Some(t) if rate == self.audio_rate && (t - video_at).abs() <= 2 * rate as i64 => t,
                _ => video_at,
            };
            self.audio_rate = rate;
            self.audio_ticks = Some(t + 1024);
            out.push(MediaItem::Audio { ticks: t, data: raw.to_vec() });
        }
        if self.released {
            return out;
        }
        // Until the first items go out: wait (from the first keyframe) a
        // moment for audio, so the recording and live view know it has audio.
        self.warm.extend(out);
        let first = self.warm.iter().find_map(|i| if let MediaItem::Video { ticks, .. } = i { Some(*ticks) } else { None });
        let span = first.map(|f| self.last_ticks - f).unwrap_or(0);
        // (Known: audio seen, or known from an earlier stream to be there or not.)
        if self.audio_known.is_some() || (first.is_some() && span >= AUDIO_WAIT_TICKS) || self.warm.len() > 200 {
            self.released = true;
            return std::mem::take(&mut self.warm);
        }
        Vec::new()
    }
}

/// The AudioSpecificConfig and sample rate of an ADTS frame.
pub fn adts_config(adts: &[u8]) -> Option<(Vec<u8>, u32)> {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    if adts.len() < 7 || adts[0] != 0xff || adts[1] & 0xf6 != 0xf0 {
        return None;
    }
    let object = (adts[2] >> 6) + 1;
    let sr = (adts[2] >> 2) & 0x0f;
    let ch = ((adts[2] & 1) << 2) | (adts[3] >> 6);
    let rate = *RATES.get(sr as usize)?;
    Some((vec![(object << 3) | (sr >> 1), ((sr & 1) << 7) | (ch << 3)], rate))
}

/// The preview request for a channel's stream.
fn preview_xml(channel: u8, stream: StreamKind) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Preview version=\"1.1\">\n<channelId>{channel}</channelId>\n<handle>{}</handle>\n<streamType>{}</streamType>\n</Preview>\n</body>\n",
        stream.handle(),
        stream.xml()
    )
}

fn stop_xml(channel: u8, stream: StreamKind) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Preview version=\"1.1\">\n<channelId>{channel}</channelId>\n<handle>{}</handle>\n</Preview>\n</body>\n",
        stream.handle()
    )
}

/// Which running stream a preview message is for: the same message number,
/// channel and stream byte as its request.
fn match_sub(subs: &[Sub], h: &bc::Header) -> Option<usize> {
    subs.iter().position(|s| s.msg_num == h.msg_num && s.channel == h.channel && s.stream.code() == h.stream)
}

/// Find where the next BC message could start (its magic) after a damaged
/// one, so the session can go on; None if no magic is in `buf`.
fn resync(buf: &[u8]) -> Option<usize> {
    let m = bc::MAGIC.to_le_bytes();
    let r = bc::MAGIC_REV.to_le_bytes();
    (1..buf.len().saturating_sub(3)).find(|&i| buf[i..i + 4] == m || buf[i..i + 4] == r)
}

/// Serve a signed-in session until it fails (Err) or is idle/shut down (Ok).
#[allow(clippy::too_many_arguments)]
fn serve(
    s: &mut Session,
    subs: &mut Vec<Sub>,
    ctl: &mut super::ctl::Queue,
    first: Cmd,
    cmd_rx: &Receiver<Cmd>,
    shared: &Arc<Shared>,
    host: &str,
    user: &str,
    pass: &str,
    me: &std::sync::Weak<WiredClient>,
) -> Result<String, String> {
    let mut pending = Some(first);
    let mut rbuf: Vec<u8> = Vec::new();
    let mut tmp = vec![0u8; 256 * 1024];
    let mut last_rx = Instant::now();
    let mut last_ka = Instant::now();
    let mut idle_since = Instant::now();
    let mut said_unmatched = false;
    let (channels, battery, hub) = {
        let st = shared.status.lock().unwrap();
        (st.channels, st.battery, needs_hub_check(st.device_type.as_deref(), st.channels))
    };
    // Streams waiting for the hub's word on their channel.
    let mut waiting: Vec<Waiting> = Vec::new();
    s.set_read_timeout(Duration::from_millis(100));
    loop {
        if !shared.alive.load(Ordering::Relaxed) {
            return Ok("shutting down".into());
        }
        while let Some(cmd) = pending.take().or_else(|| cmd_rx.try_recv().ok()) {
            match cmd {
                Cmd::Start { channel, stream, owner, reply } => {
                    if battery {
                        let _ = reply.send(Err(format!("{BATTERY_REFUSAL}: the camera reports a battery. Set it up as a battery camera")));
                        continue;
                    }
                    if let Some(n) = channels.filter(|&n| channel as u32 >= n.max(1)) {
                        let _ = reply.send(Err(format!("the device has {n} channel(s); channel {channel} does not exist")));
                        continue;
                    }
                    // A stream whose reader is gone is stopped first; one still
                    // in use is not started twice (its stop would end both).
                    for sub in subs.iter().filter(|x| x.channel == channel && x.stream == stream && x.reply.is_none() && x.tx.is_closed()) {
                        let _ = s.send_xml_on(bc::MSG_PREVIEW_STOP, sub.channel, sub.stream.code(), &stop_xml(sub.channel, sub.stream));
                    }
                    subs.retain(|x| !(x.channel == channel && x.stream == stream && x.reply.is_none() && x.tx.is_closed()));
                    if let Some(o) = busy_owner(subs, &waiting, channel, stream) {
                        let _ = reply.send(Err(format!("channel {channel} {} stream of this device is already being read (by camera '{o}')", stream.label())));
                        continue;
                    }
                    if hub {
                        // A hub, recorder or unknown device: only a channel it
                        // shows as wired. It is asked off this thread, so the
                        // other channels' streams and keep-alives go on.
                        waiting.push(Waiting { channel, stream, owner, reply, until: Instant::now() + HUB_CHECK_WAIT });
                        ask_hub_soon(shared, host, user, pass, channel);
                        continue;
                    }
                    open_sub(s, subs, shared, me, host, channel, stream, owner, reply)?;
                }
                Cmd::Stop { id } => {
                    if let Some(i) = subs.iter().position(|x| x.id == id) {
                        let sub = subs.remove(i);
                        let _ = s.send_xml_on(bc::MSG_PREVIEW_STOP, sub.channel, sub.stream.code(), &stop_xml(sub.channel, sub.stream));
                        remember_audio(shared, &sub);
                    }
                }
                Cmd::Control(r) => ctl.accept(r, battery, super::ctl::is_multi(shared.status.lock().unwrap().device_type.as_deref(), channels)),
                Cmd::Talk(tc) => talk_cmd(s, tc, shared, host, user, pass, channels, battery, hub),
                Cmd::Shutdown => {
                    shared.alive.store(false, Ordering::Relaxed);
                    return Ok("shutting down".into());
                }
            }
        }
        // Streams whose hub check is done (or took too long).
        let mut i = 0;
        while i < waiting.len() {
            let w = &waiting[i];
            let verdict = shared.channel_checks.lock().unwrap().get(&w.channel).filter(|(t, _)| t.elapsed() < CHANNEL_CHECK_FOR).map(|(_, k)| k.clone());
            let late = Instant::now() >= w.until;
            if verdict.is_none() && !late {
                i += 1;
                continue;
            }
            let w = waiting.remove(i);
            match verdict {
                Some(ChannelKind::Wired) => open_sub(s, subs, shared, me, host, w.channel, w.stream, w.owner, w.reply)?,
                Some(ChannelKind::Battery(why)) | Some(ChannelKind::Unknown(why)) => {
                    let _ = w.reply.send(Err(format!("{BATTERY_REFUSAL}: {why}. Use RTSP for this camera (or set it up as a battery camera)")));
                }
                None => {
                    let _ = w.reply.send(Err(format!("{BATTERY_REFUSAL}: the hub did not say in time whether channel {} is a battery camera", w.channel)));
                }
            }
        }
        {
            let mut st = shared.status.lock().unwrap();
            st.streams = subs.len();
            st.owners = subs.iter().map(|x| x.owner.clone()).chain(waiting.iter().map(|w| w.owner.clone())).collect();
        }
        // (A session wanted for controls stays a while after the last one.)
        if !subs.is_empty() || !waiting.is_empty() || ctl.busy() || shared.talks.lock().unwrap().busy() {
            idle_since = Instant::now();
        } else if idle_since.elapsed() >= IDLE_SIGN_OUT {
            return Ok("no stream wanted".into());
        }

        match s.read(&mut tmp) {
            Ok(0) => return Err("the camera closed the Baichuan connection".into()),
            Ok(n) => {
                rbuf.extend_from_slice(&tmp[..n]);
                last_rx = Instant::now();
            }
            Err(e) if is_wouldblock(&e) => {}
            Err(e) => return Err(format!("Baichuan connection lost: {e}")),
        }
        loop {
            match bc::parse_msg(&rbuf) {
                Ok(Some((used, m))) => {
                    rbuf.drain(..used);
                    if ctl.take(s, &m) {
                        continue;
                    }
                    dispatch(s, &m, subs, shared, &mut said_unmatched, host);
                }
                Ok(None) => break,
                Err(e) => {
                    // A damaged message: skip to the next one; every stream
                    // waits for a keyframe.
                    debug!("baichuan {host}: damaged message ({e}); resyncing");
                    match resync(&rbuf) {
                        Some(i) => {
                            rbuf.drain(..i);
                        }
                        None => rbuf.clear(),
                    }
                    for sub in subs.iter_mut() {
                        sub.pipe.damaged();
                    }
                    shared.status.lock().unwrap().damaged += 1;
                }
            }
        }

        ctl.pump(s)?;
        let now = Instant::now();
        // Streams the camera did not answer in time.
        subs.retain_mut(|sub| match &sub.reply {
            Some((_, until)) if now >= *until => {
                if let Some((r, _)) = sub.reply.take() {
                    let _ = r.send(Err(format!("the camera did not send channel {} {} stream within {} s", sub.channel, sub.stream.label(), START_TIMEOUT.as_secs())));
                }
                let _ = s.send_xml_on(bc::MSG_PREVIEW_STOP, sub.channel, sub.stream.code(), &stop_xml(sub.channel, sub.stream));
                false
            }
            _ => true,
        });
        // Streams whose reader is gone (dropped without its Stop reaching us).
        for sub in subs.iter().filter(|x| x.reply.is_none() && x.tx.is_closed()) {
            let _ = s.send_xml_on(bc::MSG_PREVIEW_STOP, sub.channel, sub.stream.code(), &stop_xml(sub.channel, sub.stream));
        }
        subs.retain(|sub| sub.reply.is_some() || !sub.tx.is_closed());
        // Talk: paced sends, set-up and let-go times (never a session error).
        talk_hub_checks(s, shared);
        shared.talks.lock().unwrap().tick(s, now);
        if now.duration_since(last_rx) >= KEEPALIVE_EVERY && now.duration_since(last_ka) >= KEEPALIVE_EVERY {
            // Quiet: ask whether the camera is still there. A keep-alive
            // that goes unanswered is not an error by itself; only silence
            // for SILENT_LIMIT is.
            let _ = s.send_keepalive();
            last_ka = now;
        }
        if now.duration_since(last_rx) >= SILENT_LIMIT {
            return Err(format!("nothing from the camera for {} s", SILENT_LIMIT.as_secs()));
        }
        let started: Vec<&Sub> = subs.iter().filter(|x| x.reply.is_none()).collect();
        if !started.is_empty() && started.iter().all(|x| now.duration_since(x.pipe.last_video) >= VIDEO_LIMIT) {
            return Err(format!("no video on any stream for {} s", VIDEO_LIMIT.as_secs()));
        }
    }
}

/// A talk command on the session. A talk is refused for a device that
/// reports a battery, and on a hub, recorder or unknown device it waits for
/// the hub's word that the channel is wired (asked off this thread, as for
/// video): talk never wakes a battery camera.
#[allow(clippy::too_many_arguments)]
fn talk_cmd(s: &mut Session, tc: TalkCmd, shared: &Arc<Shared>, host: &str, user: &str, pass: &str, channels: Option<u32>, battery: bool, hub: bool) {
    let now = Instant::now();
    match tc {
        TalkCmd::Open { channel, reply } => {
            if battery {
                let _ = reply.send(Err("talk over Baichuan is not for battery cameras (the camera reports a battery)".into()));
            } else if let Some(n) = channels.filter(|&n| channel as u32 >= n.max(1)) {
                let _ = reply.send(Err(format!("the device has {n} channel(s); channel {channel} does not exist")));
            } else if hub {
                shared.talks.lock().unwrap().wait(channel, reply, now + HUB_CHECK_WAIT);
                ask_hub_soon(shared, host, user, pass, channel);
            } else {
                shared.talks.lock().unwrap().ask(s, channel, reply, now);
            }
        }
        TalkCmd::Data { id, block, sound } => shared.talks.lock().unwrap().data(s, id, block, sound, now),
        TalkCmd::End { id } => shared.talks.lock().unwrap().end(id),
    }
}

/// Talks waiting for the hub's word on their channel: asked on, or refused.
fn talk_hub_checks(s: &mut Session, shared: &Shared) {
    let verdict = |ch: u8| -> Option<Result<(), String>> {
        match shared.channel_checks.lock().unwrap().get(&ch).filter(|(t, _)| t.elapsed() < CHANNEL_CHECK_FOR).map(|(_, k)| k.clone())? {
            ChannelKind::Wired => Some(Ok(())),
            ChannelKind::Battery(w) | ChannelKind::Unknown(w) => Some(Err(format!("talk over Baichuan is not for battery cameras: {w}"))),
        }
    };
    shared.talks.lock().unwrap().resolve(s, verdict, Instant::now());
}

/// Who already reads (or is about to read) this channel's stream: a second
/// request for it is refused, since stopping one would end both.
fn busy_owner(subs: &[Sub], waiting: &[Waiting], channel: u8, stream: StreamKind) -> Option<String> {
    subs.iter()
        .find(|x| x.channel == channel && x.stream == stream)
        .map(|x| x.owner.clone())
        .or_else(|| waiting.iter().find(|w| w.channel == channel && w.stream == stream).map(|w| w.owner.clone()))
}

/// A stream waiting for the hub's word on its channel.
struct Waiting {
    channel: u8,
    stream: StreamKind,
    owner: String,
    reply: SyncSender<Result<WiredPreview, String>>,
    until: Instant,
}

/// How long a stream waits for the hub check (two API calls of up to 8 s).
const HUB_CHECK_WAIT: Duration = Duration::from_secs(20);

/// Ask the hub about `channel` on a thread of its own, unless the answer is
/// fresh or already being asked for.
fn ask_hub_soon(shared: &Arc<Shared>, host: &str, user: &str, pass: &str, channel: u8) {
    if shared.channel_checks.lock().unwrap().get(&channel).is_some_and(|(t, _)| t.elapsed() < CHANNEL_CHECK_FOR) {
        return;
    }
    if !shared.checking.lock().unwrap().insert(channel) {
        return;
    }
    let (sh, h, u, p) = (shared.clone(), host.to_string(), user.to_string(), pass.to_string());
    let spawned = std::thread::Builder::new().name(format!("hub check {host}")).spawn(move || {
        let k = ask_hub(&h, &u, &p, channel);
        info!("baichuan {h}: hub channel {channel}: {k:?}");
        sh.channel_checks.lock().unwrap().insert(channel, (Instant::now(), k));
        sh.checking.lock().unwrap().remove(&channel);
    });
    if spawned.is_err() {
        shared.checking.lock().unwrap().remove(&channel);
    }
}

/// Ask the camera for a channel's stream and add it to the running ones.
/// Err only when the session itself failed (sending did not work).
#[allow(clippy::too_many_arguments)]
fn open_sub(
    s: &mut Session,
    subs: &mut Vec<Sub>,
    shared: &Arc<Shared>,
    me: &std::sync::Weak<WiredClient>,
    host: &str,
    channel: u8,
    stream: StreamKind,
    owner: String,
    reply: SyncSender<Result<WiredPreview, String>>,
) -> Result<(), String> {
    let Some(client) = me.upgrade() else {
        let _ = reply.send(Err("the Baichuan session has stopped".into()));
        return Ok(());
    };
    let msg_num = s.send_xml_on(bc::MSG_PREVIEW, channel, stream.code(), &preview_xml(channel, stream)).map_err(|e| format!("sending the preview request: {e}"))?;
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let params = Arc::new(Mutex::new(None));
    let audio_cfg = Arc::new(Mutex::new(None));
    let known = shared.audio.lock().unwrap().get(&(channel, stream)).cloned();
    let handle = WiredPreview { rx, params: params.clone(), audio: audio_cfg.clone(), id, client };
    debug!("baichuan {host}: preview channel {channel} {} (msg {msg_num})", stream.label());
    subs.push(Sub {
        id,
        owner,
        channel,
        stream,
        msg_num,
        tx,
        reply: Some((reply, Instant::now() + START_TIMEOUT)),
        handle: Some(handle),
        params,
        audio_cfg,
        pipe: Pipe::new(known, Instant::now()),
    });
    Ok(())
}

fn remember_audio(shared: &Shared, sub: &Sub) {
    if sub.pipe.released {
        let cfg = sub.audio_cfg.lock().unwrap().clone();
        shared.audio.lock().unwrap().insert((sub.channel, sub.stream), cfg);
    }
}

fn dispatch(s: &mut Session, m: &bc::Msg, subs: &mut Vec<Sub>, shared: &Shared, said_unmatched: &mut bool, host: &str) {
    match m.header.msg_id {
        bc::MSG_KEEPALIVE => {
            // The camera's own keep-alive asks for an answer; an answer to
            // ours needs nothing.
            if m.header.status == 0 {
                let _ = s.ack_keepalive(m.header.channel, m.header.msg_num);
            }
        }
        bc::MSG_PREVIEW => {
            let Some(i) = match_sub(subs, &m.header) else {
                shared.status.lock().unwrap().unmatched += 1;
                if !*said_unmatched {
                    *said_unmatched = true;
                    debug!(
                        "baichuan {host}: preview data for no running stream (channel {} stream {} msg {}): dropped",
                        m.header.channel, m.header.stream, m.header.msg_num
                    );
                }
                return;
            };
            let now = Instant::now();
            if m.header.status != 200 {
                let sub = subs.remove(i);
                let why = format!("the camera refused channel {} {} stream (Baichuan status {})", sub.channel, sub.stream.label(), m.header.status);
                match sub.reply {
                    Some((r, _)) => {
                        let _ = r.send(Err(why));
                    }
                    None => {
                        let _ = sub.tx.send(Err(why));
                    }
                }
                return;
            }
            let (_, pay) = s.decode_msg(&m.header, &m.body);
            let sub = &mut subs[i];
            if pay.is_empty() {
                return;
            }
            let holds = sub.pipe.holds;
            let items = sub.pipe.push(&pay, &sub.params, &sub.audio_cfg, now);
            if sub.pipe.holds != holds {
                shared.status.lock().unwrap().damaged += 1;
            }
            if sub.reply.is_some() && sub.pipe.released {
                // The stream works: hand it over (its first items follow).
                if let (Some((r, _)), Some(h)) = (sub.reply.take(), sub.handle.take()) {
                    if r.send(Ok(h)).is_err() {
                        // The caller stopped waiting; the handle's drop stops it.
                    }
                }
            }
            for item in items {
                if sub.tx.send(Ok(item)).is_err() {
                    break;
                }
            }
        }
        bc::MSG_TALK_ABILITY | bc::MSG_TALK_CONFIG | bc::MSG_TALK => {
            shared.talks.lock().unwrap().on_msg(|| s.reply_xml(&m.header, &m.body), &m.header, Instant::now());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_device_one_host_key() {
        let a = target_for("rtsp://[FD00::5]:554/Preview_01_sub", 9000, None, StreamKind::Main).unwrap();
        let b = target_for("rtsp://u:p@[fd00:0:0::5]/Preview_01_main", 9000, None, StreamKind::Main).unwrap();
        assert_eq!((a.host.as_str(), a.key()), ("fd00::5", b.key()));
        assert_eq!(target_for("rtsp://Cam.LAN/x", 9000, None, StreamKind::Main).unwrap().host, "cam.lan");
        assert_eq!(norm_host("192.0.2.5"), "192.0.2.5");
    }

    #[test]
    fn targets_come_from_reolink_rtsp_addresses() {
        let t = target_for("rtsp://192.0.2.22:554/Preview_02_sub", 9000, None, StreamKind::Main).unwrap();
        assert_eq!(t, Target { host: "192.0.2.22".into(), port: 9000, channel: 1, stream: StreamKind::Sub, owner: String::new() });
        let t = target_for("rtsp://admin:x@cam.lan/h264Preview_01_main", 9001, None, StreamKind::Sub).unwrap();
        assert_eq!((t.host.as_str(), t.port, t.channel, t.stream), ("cam.lan", 9001, 0, StreamKind::Main));
        let t = target_for("rtsp://192.0.2.5/Preview_01_ext", 9000, Some(3), StreamKind::Main).unwrap();
        assert_eq!((t.channel, t.stream), (3, StreamKind::Extern), "a set channel wins");
        let t = target_for("rtsp://192.0.2.5/live", 9000, None, StreamKind::Sub).unwrap();
        assert_eq!((t.channel, t.stream), (0, StreamKind::Sub), "unknown path: the default stream");
        assert_eq!(target_for("rtsp://[fd00::5]:554/Preview_01_main", 9000, None, StreamKind::Main).unwrap().host, "fd00::5");
        assert!(target_for("nothing", 9000, None, StreamKind::Main).is_none());
    }

    fn hdr(num: u16, ch: u8, st: u8) -> bc::Header {
        bc::Header { msg_id: bc::MSG_PREVIEW, body_len: 0, channel: ch, stream: st, msg_num: num, status: 200, class: 0, payload_offset: Some(0) }
    }

    fn sub(id: u64, channel: u8, stream: StreamKind, msg_num: u16) -> Sub {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Sub {
            id,
            owner: format!("cam{id}"),
            channel,
            stream,
            msg_num,
            tx,
            reply: None,
            handle: None,
            params: Default::default(),
            audio_cfg: Default::default(),
            pipe: Pipe::new(None, Instant::now()),
        }
    }

    #[test]
    fn only_a_plain_camera_skips_the_hub_check() {
        assert!(!needs_hub_check(Some("ipc"), Some(1)));
        assert!(!needs_hub_check(Some("IPC"), None), "a camera that does not give its channel count");
        assert!(needs_hub_check(Some("homehub"), Some(8)));
        assert!(needs_hub_check(Some("homehub"), None), "a hub without <channelNum> is still asked");
        assert!(needs_hub_check(Some("nvr"), Some(1)), "any type but ipc is asked");
        assert!(needs_hub_check(None, Some(1)), "no type given: cannot tell");
        assert!(needs_hub_check(None, None));
        assert!(needs_hub_check(Some("ipc"), Some(2)), "dual-lens: asked (and refused for now)");
        assert!(!needs_hub_check(Some("wifi_solo_ipc"), Some(1)), "an E1 Pro is a camera of its own");
        assert!(!needs_hub_check(Some("wifi_solo_ipc"), None));
        assert!(needs_hub_check(Some("wifi_solo_ipc"), Some(4)));
        assert!(needs_hub_check(Some("hub"), Some(1)));
    }

    #[test]
    fn a_second_request_for_a_running_stream_is_refused() {
        let subs = vec![sub(1, 0, StreamKind::Main, 3), sub(2, 1, StreamKind::Sub, 4)];
        assert_eq!(busy_owner(&subs, &[], 0, StreamKind::Main).as_deref(), Some("cam1"));
        assert_eq!(busy_owner(&subs, &[], 1, StreamKind::Sub).as_deref(), Some("cam2"));
        assert_eq!(busy_owner(&subs, &[], 0, StreamKind::Sub), None);
        let (reply, _rx) = std::sync::mpsc::sync_channel(1);
        let waiting = vec![Waiting { channel: 2, stream: StreamKind::Main, owner: "door".into(), reply, until: Instant::now() }];
        assert_eq!(busy_owner(&subs, &waiting, 2, StreamKind::Main).as_deref(), Some("door"), "one waiting for its hub check counts");
    }

    #[test]
    fn other_credentials_replace_a_session_only_when_no_other_camera_uses_it() {
        let st = |owners: &[&str], signing: Option<&str>| Status {
            owners: owners.iter().map(|s| s.to_string()).collect(),
            signing_in_for: signing.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(reuse(true, true, &st(&["a"], None), "b"), Reuse::Reuse);
        assert_eq!(reuse(false, true, &st(&[], None), "a"), Reuse::Replace, "nobody on it");
        assert_eq!(reuse(false, true, &st(&["a"], None), "a"), Reuse::Replace, "the same camera's settings changed");
        assert!(matches!(reuse(false, true, &st(&["a"], None), "b"), Reuse::Conflict(w) if w.contains("'a'") && w.contains("different credentials")));
        assert!(matches!(reuse(false, true, &st(&[], Some("a")), "b"), Reuse::Conflict(_)), "mid sign-in for another camera");
        assert_eq!(reuse(false, false, &st(&["a"], None), "b"), Reuse::Replace, "a stopped session");
    }

    #[test]
    fn preview_replies_reach_only_the_stream_asked_for() {
        let subs = vec![sub(1, 0, StreamKind::Main, 3), sub(2, 0, StreamKind::Sub, 4), sub(3, 1, StreamKind::Sub, 5)];
        assert_eq!(match_sub(&subs, &hdr(3, 0, 0)), Some(0));
        assert_eq!(match_sub(&subs, &hdr(4, 0, 1)), Some(1));
        assert_eq!(match_sub(&subs, &hdr(5, 1, 1)), Some(2));
        // Same number, other channel or stream: not ours (an earlier
        // stream's late packets, or another client's).
        assert_eq!(match_sub(&subs, &hdr(5, 0, 1)), None);
        assert_eq!(match_sub(&subs, &hdr(4, 0, 0)), None);
        assert_eq!(match_sub(&subs, &hdr(9, 0, 0)), None, "a stopped stream's number");
    }

    // ---- BcMedia building blocks for the pipe tests
    fn vframe(key: bool, micros: u32, annexb: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(if key { b"00dc" } else { b"01dc" });
        v.extend_from_slice(b"H264");
        v.extend_from_slice(&(annexb.len() as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&micros.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(annexb);
        while v.len() % 8 != 0 {
            v.push(0);
        }
        v
    }
    fn nal(b: &[u8]) -> Vec<u8> {
        [&[0, 0, 0, 1][..], b].concat()
    }
    fn idr_au() -> Vec<u8> {
        [nal(&[0x67, 0x64, 0, 0x1f, 0xac, 0xd9]), nal(&[0x68, 0xeb, 0xe3]), nal(&[0x65, 0x88, 0x84, 0x10])].concat()
    }
    fn p_au(n: u8) -> Vec<u8> {
        nal(&[0x41, 0x9a, n, 0x20])
    }
    fn info() -> Vec<u8> {
        let mut v = b"1002".to_vec();
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&640u32.to_le_bytes());
        v.extend_from_slice(&360u32.to_le_bytes());
        v.resize(32, 0);
        v
    }
    fn aac(n: u8) -> Vec<u8> {
        // ADTS: AAC-LC, 16 kHz (index 8), mono, one block, 7-byte header + 4 bytes.
        let len = 11u16;
        let adts = vec![0xff, 0xf1, (1 << 6) | (8 << 2), (1 << 6) | ((len >> 11) as u8 & 3), (len >> 3) as u8, ((len & 7) << 5) as u8 | 0x1f, 0xfc, n, n, n, n];
        let mut v = b"05wb".to_vec();
        v.extend_from_slice(&(adts.len() as u16).to_le_bytes());
        v.extend_from_slice(&(adts.len() as u16).to_le_bytes());
        v.extend_from_slice(&adts);
        while v.len() % 8 != 0 {
            v.push(0);
        }
        v
    }

    fn videos(items: &[MediaItem]) -> Vec<(i64, bool)> {
        items.iter().filter_map(|i| if let MediaItem::Video { ticks, key, .. } = i { Some((*ticks, *key)) } else { None }).collect()
    }

    #[test]
    fn no_picture_until_the_first_keyframe_then_none_after_damage_until_the_next() {
        let (params, audio) = (Mutex::new(None), Mutex::new(None));
        let now = Instant::now();
        let mut p = Pipe::new(Some(None), now); // known: no audio, so nothing waits
        // P-frames before any keyframe: dropped.
        let out = p.push(&[info(), vframe(false, 0, &p_au(1)), vframe(false, 66_000, &p_au(2))].concat(), &params, &audio, now);
        assert!(videos(&out).is_empty());
        let out = p.push(&[vframe(true, 132_000, &idr_au()), vframe(false, 198_000, &p_au(3))].concat(), &params, &audio, now);
        let v = videos(&out);
        assert!(!p.warm.is_empty() || !v.is_empty());
        // Push enough to pass the audio wait (1.5 s).
        let mut all = v;
        for k in 0..30u32 {
            all.extend(videos(&p.push(&vframe(false, 264_000 + k * 66_000, &p_au(4)), &params, &audio, now)));
        }
        assert!(all[0].1, "the first picture out is the keyframe");
        assert!(params.lock().unwrap().is_some());
        // Damage: garbage in the middle of the stream.
        let mut bad = vframe(false, 3_000_000, &p_au(5));
        bad.truncate(20);
        let mut stream = bad;
        stream.extend_from_slice(&[0x55; 40]);
        stream.extend_from_slice(&vframe(false, 3_066_000, &p_au(6)));
        stream.extend_from_slice(&vframe(false, 3_132_000, &p_au(7)));
        let out = p.push(&stream, &params, &audio, now);
        assert!(videos(&out).is_empty(), "held after damage: {:?}", videos(&out));
        assert_eq!(p.holds, 1);
        let out = p.push(&[vframe(true, 3_198_000, &idr_au()), vframe(false, 3_264_000, &p_au(8))].concat(), &params, &audio, now);
        let v = videos(&out);
        assert!(v[0].1 && v.len() == 2, "back at the keyframe: {v:?}");
    }

    fn vframe_sized(key: bool, micros: u32, annexb: &[u8], claimed: u32) -> Vec<u8> {
        let mut v = vframe(key, micros, annexb);
        v[8..12].copy_from_slice(&claimed.to_le_bytes());
        v
    }

    #[test]
    fn a_cut_keyframe_is_never_shown_and_only_a_keyframe_after_the_damage_ends_the_hold() {
        let (params, audio) = (Mutex::new(None), Mutex::new(None));
        let now = Instant::now();
        let mut p = Pipe::new(Some(None), now);
        let out = p.push(&[info(), vframe(true, 0, &idr_au()), vframe(false, 66_000, &p_au(1))].concat(), &params, &audio, now);
        assert_eq!(videos(&out).len(), 2);
        // One push: a P-frame, damage, then a keyframe AFTER it and a P.
        // The hold ends at that keyframe, not before it.
        // (Damage here: a frame header no camera sends, skipped by the demuxer.)
        let bad = || {
            let mut b = vframe(false, 0, &p_au(9));
            b[12..16].copy_from_slice(&0x7fff_0000u32.to_le_bytes());
            b
        };
        let stream = [vframe(false, 132_000, &p_au(2)), bad(), vframe(true, 198_000, &idr_au()), vframe(false, 264_000, &p_au(3))].concat();
        let v = videos(&p.push(&stream, &params, &audio, now));
        assert_eq!(v.len(), 3, "the P before the damage, then the keyframe after it and its P: {v:?}");
        assert!(!v[0].1 && v[1].1 && !v[2].1);
        // One push: a keyframe BEFORE the damage cannot end a hold that the
        // damage starts: the P-frames after it stay held.
        let stream = [vframe(true, 330_000, &idr_au()), bad(), vframe(false, 396_000, &p_au(4)), vframe(false, 462_000, &p_au(5))].concat();
        let v = videos(&p.push(&stream, &params, &audio, now));
        assert_eq!(v.len(), 1, "only the keyframe from before the damage: {v:?}");
        assert!(v[0].1);
        // A keyframe cut short (its size says more than came before the next
        // frame) is dropped, and the hold goes on until a whole one.
        let stream = [vframe_sized(true, 528_000, &idr_au(), 5_000), vframe(false, 594_000, &p_au(6))].concat();
        assert!(videos(&p.push(&stream, &params, &audio, now)).is_empty(), "a cut keyframe is not shown, nor what follows");
        let v = videos(&p.push(&[vframe(true, 660_000, &idr_au()), vframe(false, 726_000, &p_au(7))].concat(), &params, &audio, now));
        assert_eq!(v.len(), 2);
        assert!(v[0].1);
    }

    #[test]
    fn times_step_through_the_camera_counter_wrapping() {
        let (params, audio) = (Mutex::new(None), Mutex::new(None));
        let now = Instant::now();
        let mut p = Pipe::new(Some(None), now);
        let start = u32::MAX - 100_000;
        let mut out = p.push(&vframe(true, start, &idr_au()), &params, &audio, now);
        for k in 1..40u32 {
            out.extend(p.push(&vframe(false, start.wrapping_add(k * 66_666), &p_au(1)), &params, &audio, now));
        }
        let v = videos(&out);
        assert!(v.windows(2).all(|w| w[1].0 - w[0].0 == 5_999), "even steps across the wrap: {v:?}");
    }

    #[test]
    fn aac_audio_is_passed_on_with_its_config_and_waited_for_at_the_start() {
        let (params, audio) = (Mutex::new(None), Mutex::new(None));
        let now = Instant::now();
        let mut p = Pipe::new(None, now); // audio not known yet
        let out = p.push(&[info(), vframe(true, 0, &idr_au())].concat(), &params, &audio, now);
        assert!(out.is_empty(), "held a moment for audio");
        let out = p.push(&[aac(1), vframe(false, 66_000, &p_au(1)), aac(2)].concat(), &params, &audio, now);
        assert_eq!(audio.lock().unwrap().as_deref(), Some(&[0x14, 0x08][..]), "AAC-LC 16 kHz mono");
        let kinds: Vec<&str> = out.iter().map(|i| match i { MediaItem::Video { .. } => "v", MediaItem::Audio { .. } => "a", _ => "?" }).collect();
        assert_eq!(kinds, ["v", "v", "a", "a"]);
        let ats: Vec<i64> = out.iter().filter_map(|i| if let MediaItem::Audio { ticks, data } = i { assert_eq!(data.len(), 4, "ADTS header taken off"); Some(*ticks) } else { None }).collect();
        assert_eq!(ats[1] - ats[0], 1024);
    }

    #[test]
    fn a_stream_without_audio_starts_after_a_short_wait() {
        let (params, audio) = (Mutex::new(None), Mutex::new(None));
        let now = Instant::now();
        let mut p = Pipe::new(None, now);
        let mut n = 0;
        let mut k = 0u32;
        while n == 0 && k < 100 {
            n += videos(&p.push(&vframe(k == 0, k * 66_666, if k == 0 { idr_au() } else { p_au(1) }.as_slice()), &params, &audio, now)).len();
            k += 1;
        }
        assert!((22..=25).contains(&k), "released after about 1.5 s of video ({k} frames)");
        assert_eq!(n, k as usize, "nothing lost while waiting");
    }

    #[test]
    fn adts_headers_give_the_audio_config() {
        // AAC-LC 16 kHz mono; AAC-LC 22.05 kHz stereo.
        assert_eq!(adts_config(&[0xff, 0xf1, 0x60, 0x40, 0x01, 0x7f, 0xfc]), Some((vec![0x14, 0x08], 16000)));
        assert_eq!(adts_config(&[0xff, 0xf1, 0x5c, 0x80, 0, 0, 0]), Some((vec![0x13, 0x90], 22050)));
        assert!(adts_config(&[0x00, 0xf1, 0x50, 0x80, 0, 0, 0]).is_none());
    }

    #[test]
    fn resync_finds_the_next_message() {
        let mut b = vec![1, 2, 3];
        b.extend_from_slice(&bc::MAGIC.to_le_bytes());
        assert_eq!(resync(&b), Some(3));
        assert_eq!(resync(&[0; 10]), None);
    }

    fn answers(sleep: i64, online: i64, battery_ver: Option<i64>) -> Vec<serde_json::Value> {
        let mut chn = serde_json::json!({ "waterMark": { "permit": 6, "ver": 1 } });
        if let Some(v) = battery_ver {
            chn["battery"] = serde_json::json!({ "permit": if v > 0 { 6 } else { 0 }, "ver": v });
        }
        vec![
            serde_json::json!({ "cmd": "GetChannelstatus", "code": 0, "value": { "count": 2, "status": [
                { "channel": 0, "online": online, "sleep": sleep },
                { "channel": 1, "online": 1, "sleep": 0 } ] } }),
            serde_json::json!({ "cmd": "GetAbility", "code": 0, "value": { "Ability": { "abilityChn": [chn.clone(), chn] } } }),
        ]
    }

    #[test]
    fn hub_channels_are_streamed_only_when_shown_wired() {
        assert_eq!(channel_kind(&answers(0, 1, Some(0)), 0), ChannelKind::Wired);
        assert!(matches!(channel_kind(&answers(1, 1, Some(0)), 0), ChannelKind::Battery(_)), "asleep");
        assert!(matches!(channel_kind(&answers(0, 1, Some(1)), 0), ChannelKind::Battery(_)), "battery ability");
        assert!(matches!(channel_kind(&answers(0, 0, Some(0)), 0), ChannelKind::Unknown(_)), "offline: cannot tell");
        assert!(matches!(channel_kind(&answers(0, 1, None), 0), ChannelKind::Unknown(_)), "no battery field");
        assert!(matches!(channel_kind(&answers(0, 1, Some(0)), 5), ChannelKind::Unknown(_)), "not listed");
        assert!(matches!(channel_kind(&[], 0), ChannelKind::Unknown(_)), "no answer");
        let refused = format!("{BATTERY_REFUSAL}: x");
        assert!(is_battery_refusal(&refused));
    }

    // ---- a whole session over a scripted transport

    fn msg(id: u32, ch: u8, st: u8, num: u16, status: u16, body: &[u8]) -> Vec<u8> {
        let h = bc::Header { msg_id: id, body_len: body.len() as u32, channel: ch, stream: st, msg_num: num, status, class: 0x0000, payload_offset: Some(0) };
        [h.encode(), body.to_vec()].concat()
    }

    #[test]
    fn tcp_framing_survives_partial_reads_several_messages_per_read_and_a_big_login_reply() {
        use super::super::transport::ScriptTransport;
        // An upgrade reply with a nonce, then a 7.3 KB sign-in reply (as the
        // Duo 2 PoE / RLC-811A send) split over many small reads, then two
        // messages in one read.
        let nonce = bc::xor_crypt(0, b"<?xml version=\"1.0\"?><body><Encryption version=\"1.1\"><type>md5</type><nonce>N0NCE</nonce></Encryption></body>");
        let up = {
            let h = bc::Header { msg_id: bc::MSG_LOGIN, body_len: nonce.len() as u32, channel: 0, stream: 0, msg_num: 1, status: 0xdd12, class: bc::CLASS_LEGACY, payload_offset: None };
            [h.encode(), nonce].concat()
        };
        let mut xml = String::from("<?xml version=\"1.0\"?><body><DeviceInfo version=\"1.1\"><type>ipc</type><channelNum>1</channelNum>");
        while xml.len() < 7300 {
            xml.push_str("<padding>0123456789abcdef</padding>");
        }
        xml.push_str("</DeviceInfo></body>");
        // (The sign-in reply is BC-encrypted, like the request.)
        let login = msg(bc::MSG_LOGIN, 0, 0, 2, 200, &bc::xor_crypt(0, xml.as_bytes()));
        assert!(login.len() > 7300);
        let mut chunks: Vec<Vec<u8>> = vec![up[..10].to_vec(), up[10..].to_vec(), vec![]];
        for c in login.chunks(1000) {
            chunks.push(c.to_vec());
        }
        chunks.push(vec![]);
        let t = ScriptTransport::new(chunks);
        let s = Session::sign_in(Box::new(t), "test", "admin", "pw").unwrap();
        assert!(s.uses_aes);
        assert!(s.login_xml.contains("<channelNum>1</channelNum>") && s.login_xml.len() > 7300, "the whole reply was read");

        // Several messages in one read, and one split mid-header.
        let a = msg(bc::MSG_PREVIEW, 0, 1, 3, 200, b"abc");
        let b = msg(bc::MSG_KEEPALIVE, 0, 0, 9, 0, b"");
        let c = msg(bc::MSG_PREVIEW, 0, 1, 3, 200, b"defgh");
        let mut buf = [a.clone(), b.clone(), c[..7].to_vec()].concat();
        let mut got = Vec::new();
        while let Ok(Some((used, m))) = bc::parse_msg(&buf) {
            buf.drain(..used);
            got.push((m.header.msg_id, m.body));
        }
        assert_eq!(got, vec![(bc::MSG_PREVIEW, b"abc".to_vec()), (bc::MSG_KEEPALIVE, vec![])]);
        buf.extend_from_slice(&c[7..]);
        let (_, m) = bc::parse_msg(&buf).unwrap().unwrap();
        assert_eq!(m.body, b"defgh");
    }

    #[test]
    fn a_refused_sign_in_is_a_refusal() {
        use super::super::transport::ScriptTransport;
        let nonce = bc::xor_crypt(0, b"<body><Encryption><nonce>N</nonce></Encryption></body>");
        let up = {
            let h = bc::Header { msg_id: bc::MSG_LOGIN, body_len: nonce.len() as u32, channel: 0, stream: 0, msg_num: 1, status: 0xdd12, class: bc::CLASS_LEGACY, payload_offset: None };
            [h.encode(), nonce].concat()
        };
        // Any answer that is not a yes is a refusal (never asked again).
        for status in [400, 401, 500] {
            let refused = msg(bc::MSG_LOGIN, 0, 0, 2, status, b"");
            let t = ScriptTransport::new(vec![up.clone(), vec![], refused, vec![]]);
            let e = Session::sign_in(Box::new(t), "test", "admin", "wrong").err().unwrap();
            assert!(e.downcast_ref::<SignInRefused>().is_some(), "{status}: {e:#}");
            assert!(crate::engine::native::is_auth_error(&format!("{e:#}")), "the worker sees it as a refused password: {e:#}");
        }
        // A refusal stays a refusal even with other "200" messages around it.
        let other = msg(78, 0, 0, 0, 200, b"");
        let refused = msg(bc::MSG_LOGIN, 0, 0, 2, 400, b"");
        let t = ScriptTransport::new(vec![up.clone(), vec![], [other.clone(), refused, other].concat(), vec![]]);
        let e = Session::sign_in(Box::new(t), "test", "admin", "wrong").err().unwrap();
        assert!(e.downcast_ref::<SignInRefused>().is_some(), "{e:#}");
        // No answer at all is not a refusal (a network failure: tried again later).
        let t = ScriptTransport::new(vec![up, vec![]]);
        let e = Session::sign_in(Box::new(t), "test", "admin", "pw").err().unwrap();
        assert!(e.downcast_ref::<SignInRefused>().is_none(), "{e:#}");
    }
}
