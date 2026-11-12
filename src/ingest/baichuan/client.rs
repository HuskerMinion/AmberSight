//! The engine-facing Baichuan client for one battery Argus camera.
//!
//! A single background thread owns the UDP session and does everything the
//! camera needs on one connection: it signs in, subscribes to motion, answers
//! keep-alives, reads battery/solar state, and — on demand — starts the preview
//! stream and feeds decoded frames out. This is the hub-less equivalent of the
//! always-on relay a Reolink hub gives a hub-joined battery camera: the session
//! stays up so motion can wake recording, while video is only pulled when a
//! viewer, a click-to-wake, or motion asks for it.
//!
//! The engine reaches the client two ways, both via the per-UID registry
//! (`super::registry`): the event poller (`engine::camera_events`) drains
//! [`BaichuanClient::next_event`] onto the camera handle, and the ingest reader
//! (`recorder::Reader::Baichuan`) calls [`BaichuanClient::start_preview`] for a
//! stream of [`MediaItem`]s.
//!
//! Nothing here has been run against a camera yet (that is Phase 3); it is
//! built to compile and to match the probe's proven protocol exactly.

use super::avcc::AvccBridge;
use super::bc::{self, Header, Msg};
use super::media::Demux;
use super::udp::UdpTransport;
use crate::ingest::recorder::{MediaItem, VideoParams};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info};

/// How the camera classifies activity (mapped to the engine's `EventKind` by
/// the poller, so this module needs no engine types).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    Motion,
    Person,
    Vehicle,
    Animal,
}

/// Battery / solar state as the camera reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Battery {
    pub percent: i32,
    pub charging: bool,
    pub adapter: String,
    pub voltage_mv: i32,
    pub temp_c: i32,
}

/// Something the camera told us, drained by the event poller.
#[derive(Debug, Clone)]
pub enum CamEvent {
    Motion { kind: MotionKind, active: bool },
    Battery(Battery),
    Connected(bool),
}

/// Thread-safe view the poller and the reader read without talking to the loop.
struct Shared {
    params: Mutex<Option<VideoParams>>,
    connected: AtomicBool,
    alive: AtomicBool,
    /// The camera's clock minus UTC, in seconds (its time zone plus any
    /// drift), read from the first info header of a live stream; i64::MIN until
    /// known. The camera files its SD recordings by this clock.
    clock_offset_s: std::sync::atomic::AtomicI64,
    /// The camera's own clock minus UTC, in seconds, as it reported it (cmd
    /// 104) before the last SD request; i64::MIN until read. The card is
    /// indexed by this local time. (The media headers may differ: older
    /// firmware stamps them in UTC, newer in local time.)
    card_offset_s: std::sync::atomic::AtomicI64,
    /// Set by start_preview() until the session thread takes the request: a
    /// running SD download stops early so live video is not kept waiting.
    preview_wanted: AtomicBool,
}

/// The camera's clock minus UTC from its cmd 104 reply (`SystemGeneral`): its
/// local date/time fields against `utc_now`, rounded to the second; or, when
/// those are missing, its `timeZone` (seconds WEST of UTC, so negated).
fn card_offset_s(xml: &str, utc_now: NaiveDateTime) -> Option<i64> {
    let num = |tag: &str| bc::xml_tag(xml, tag).and_then(|v| v.trim().parse::<i64>().ok());
    let local = (|| {
        let d = chrono::NaiveDate::from_ymd_opt(num("year")? as i32, num("month")? as u32, num("day")? as u32)?;
        d.and_hms_opt(num("hour")? as u32, num("minute")? as u32, num("second")? as u32)
    })();
    match local {
        Some(l) => Some((l - utc_now).num_seconds()),
        None => num("timeZone").map(|tz| -tz),
    }
}

/// The camera's clock offset from its live stream's info header (`camera_now`,
/// the camera's local time when preview started) and the time it was read.
fn clock_offset_s(camera_now: NaiveDateTime, utc_now: NaiveDateTime) -> i64 {
    (camera_now - utc_now).num_seconds()
}

enum Cmd {
    /// Start preview; the reply carries the frame receiver.
    StartPreview(SyncSender<UnboundedReceiver<MediaItem>>),
    StopPreview,
    /// Stream the SD-card recording for a time range (epoch ms, converted to
    /// the camera's clock just before asking). Runs once no preview is active,
    /// on the same session.
    Download { from_ms: i64, to_ms: i64, reply: SyncSender<Download> },
    Shutdown,
}

use chrono::{Datelike, NaiveDateTime, Timelike};

/// What an SD request brought back.
pub struct Download {
    /// Raw BcMedia bytes; empty if the card had nothing then.
    pub bytes: Vec<u8>,
    /// The camera's clock minus UTC (s) the request was made by, if known.
    pub card_offset_s: Option<i64>,
    /// Cut short for live video: empty bytes then say nothing about the card.
    pub aborted: bool,
}

/// `ms` (epoch) on a clock `offset_s` from UTC; with no offset known, this
/// server's local time (the usual case: camera and server in one zone).
fn on_camera_clock(ms: i64, offset_s: Option<i64>) -> Option<NaiveDateTime> {
    use chrono::TimeZone;
    match offset_s {
        Some(o) => chrono::DateTime::from_timestamp_millis(ms + o * 1000).map(|t| t.naive_utc()),
        None => chrono::Local.timestamp_millis_opt(ms).single().map(|t| t.naive_local()),
    }
}

/// How long one SD download may run, and how much it may bring back.
const DOWNLOAD_LIMIT: Duration = Duration::from_secs(45);
const MAX_DOWNLOAD_BYTES: usize = 32 << 20;

/// The cmd 143 request: the main-stream recording between `from` and `to`
/// (camera local time; time parts are separate elements, not zero-padded).
fn download_xml(from: NaiveDateTime, to: NaiveDateTime) -> String {
    let t = |tag: &str, d: NaiveDateTime| {
        format!(
            "<{tag}>\n<year>{}</year>\n<month>{}</month>\n<day>{}</day>\n<hour>{}</hour>\n<minute>{}</minute>\n<second>{}</second>\n</{tag}>\n",
            d.year(), d.month(), d.day(), d.hour(), d.minute(), d.second()
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<FileInfoList version=\"1.1\">\n<FileInfo>\n\
         <logicChnBitmap>255</logicChnBitmap>\n<channelId>0</channelId>\n<supportSub>0</supportSub>\n\
         <streamType>mainStream</streamType>\n{}{}</FileInfo>\n</FileInfoList>\n</body>\n",
        t("startTime", from),
        t("endTime", to)
    )
}

/// A running client for one camera. Cloneable handle (`Arc`), owned by the
/// registry while the camera is configured.
pub struct BaichuanClient {
    cmd_tx: Sender<Cmd>,
    events: Mutex<Receiver<CamEvent>>,
    shared: Arc<Shared>,
}

impl BaichuanClient {
    /// Spawn the session thread for `host`/`uid` and return the handle. The
    /// thread reconnects on its own until [`BaichuanClient::shutdown`].
    pub fn spawn(host: String, uid: String, user: String, pass: String) -> Arc<BaichuanClient> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            params: Mutex::new(None),
            clock_offset_s: std::sync::atomic::AtomicI64::new(i64::MIN),
            card_offset_s: std::sync::atomic::AtomicI64::new(i64::MIN),
            preview_wanted: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        });
        let s2 = shared.clone();
        std::thread::Builder::new()
            .name(format!("baichuan {host}"))
            .spawn(move || run(host, uid, user, pass, cmd_rx, event_tx, s2))
            .expect("spawn baichuan thread");
        Arc::new(BaichuanClient { cmd_tx, events: Mutex::new(event_rx), shared })
    }

    /// Start the preview stream and return a handle yielding frames. The handle
    /// stops preview when dropped; the session then signs out shortly after, so
    /// the camera can sleep. Signs in first if needed (a few seconds on a
    /// sleeping battery camera). `None` if it could not in time.
    pub fn start_preview(self: &Arc<Self>) -> Option<PreviewHandle> {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.shared.preview_wanted.store(true, Ordering::Relaxed);
        self.cmd_tx.send(Cmd::StartPreview(reply_tx)).ok()?;
        let rx = reply_rx.recv_timeout(START_TIMEOUT).ok()?;
        Some(PreviewHandle { rx, client: self.clone() })
    }

    fn stop_preview(&self) {
        let _ = self.cmd_tx.send(Cmd::StopPreview);
    }

    /// The camera's own SD-card recording between `from_ms` and `to_ms`
    /// (epoch); signs in if needed and waits for any running preview to finish
    /// first. Blocks; call from a worker thread. `None` if the session could
    /// not be had; empty bytes if nothing came.
    pub fn download(&self, from_ms: i64, to_ms: i64) -> Option<Download> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.cmd_tx.send(Cmd::Download { from_ms, to_ms, reply }).ok()?;
        rx.recv_timeout(Duration::from_secs(15 * 60)).ok()
    }

    /// Whether the session is signed in to the camera right now.
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    /// Stop the session thread and sign out.
    pub fn shutdown(&self) {
        self.shared.alive.store(false, Ordering::Relaxed);
        let _ = self.cmd_tx.send(Cmd::Shutdown);
    }

    /// Next pending camera event, or `None` after `timeout` with nothing.
    pub fn next_event(&self, timeout: Duration) -> Option<CamEvent> {
        self.events.lock().unwrap().recv_timeout(timeout).ok()
    }

    pub fn video_params(&self) -> Option<VideoParams> {
        self.shared.params.lock().unwrap().clone()
    }

    /// The camera's clock minus UTC in seconds, as read from its live stream;
    /// `None` until it has streamed once. Its SD recordings follow this clock.
    pub fn camera_clock_offset_s(&self) -> Option<i64> {
        let v = self.shared.clock_offset_s.load(Ordering::Relaxed);
        (v != i64::MIN).then_some(v)
    }
}

impl Drop for BaichuanClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The frame stream for one preview; stops preview on drop.
pub struct PreviewHandle {
    pub rx: UnboundedReceiver<MediaItem>,
    client: Arc<BaichuanClient>,
}

impl PreviewHandle {
    pub fn video_params(&self) -> Option<VideoParams> {
        self.client.video_params()
    }
}

impl Drop for PreviewHandle {
    fn drop(&mut self) {
        self.client.stop_preview();
    }
}

// ----------------------------------------------------------------- the thread

fn run(
    host: String,
    uid: String,
    user: String,
    pass: String,
    cmd_rx: Receiver<Cmd>,
    event_tx: Sender<CamEvent>,
    shared: Arc<Shared>,
) {
    // On demand: a battery camera stays asleep (and keeps posting its PIR push)
    // only while nothing is connected to it, so stay disconnected until video
    // is wanted (a push, the wake button, a viewer), then sign in, serve, and
    // sign out once preview has been idle for IDLE_SIGN_OUT.
    //
    // Back off on repeated sign-in failures so a wrong password or an
    // unreachable camera cannot hammer the account into a lockout: during the
    // backoff a request is refused at once (the worker retries later).
    let mut backoff = Duration::from_secs(10);
    let mut retry_at: Option<Instant> = None;
    while shared.alive.load(Ordering::Relaxed) {
        let first = match cmd_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(c @ Cmd::StartPreview(_)) => {
                // Taken: whatever comes of it, a download need not give way.
                shared.preview_wanted.store(false, Ordering::Relaxed);
                c
            }
            Ok(c @ Cmd::Download { .. }) => c,
            Ok(Cmd::StopPreview) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Ok(Cmd::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if retry_at.is_some_and(|t| Instant::now() < t) {
            continue; // reply dropped: start_preview() returns None
        }
        // NOTE: no motion subscription (msg 31): it made the Argus 2 refuse
        // Preview, and battery cameras report motion by their push (pushx).
        match Session::login(&host, &uid, &user, &pass) {
            Ok(mut session) => {
                backoff = Duration::from_secs(10);
                retry_at = None;
                shared.connected.store(true, Ordering::Relaxed);
                let _ = event_tx.send(CamEvent::Connected(true));
                if let Some(b) = session.battery.take() {
                    let _ = event_tx.send(CamEvent::Battery(b));
                }
                serve(&mut session, Some(first), &cmd_rx, &event_tx, &shared);
                let _ = session.logout();
                shared.connected.store(false, Ordering::Relaxed);
                let _ = event_tx.send(CamEvent::Connected(false));
                info!("baichuan '{host}': signed out (idle), camera may sleep");
            }
            Err(e) => {
                debug!("baichuan '{host}': connect failed: {e:#}; next try in {} s", backoff.as_secs());
                let _ = event_tx.send(CamEvent::Connected(false));
                retry_at = Some(Instant::now() + backoff);
                // Capped at a minute: tries happen only when video is wanted,
                // so this is gentle on the account and a camera back on the
                // network is not refused for long.
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
    debug!("baichuan '{host}': session thread exiting");
}

/// How long start_preview() waits: signing in to a sleeping battery camera
/// (discovery + wake + login) takes a few seconds.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// Sign out once preview has been stopped this long, so the camera can sleep.
const IDLE_SIGN_OUT: Duration = Duration::from_secs(15);

/// Serve one connected session until it dies, shuts down, or has had no
/// preview for IDLE_SIGN_OUT. `first` is the request that caused the sign-in.
fn serve(session: &mut Session, first: Option<Cmd>, cmd_rx: &Receiver<Cmd>, event_tx: &Sender<CamEvent>, shared: &Shared) {
    // SD downloads asked for while preview runs wait here until it stops.
    let mut downloads: std::collections::VecDeque<(i64, i64, SyncSender<Download>)> = Default::default();
    let mut previewing = false;
    // The camera often ignores the first Preview request over UDP, so resend it
    // until media actually flows (`flowing`), the same fix the probe needed.
    let mut flowing = false;
    let mut last_start = Instant::now();
    let mut demux = Demux::new();
    let mut clock_read = false;
    let mut bridge = AvccBridge::new(false, 0, 0);
    let mut frame_tx: Option<UnboundedSender<MediaItem>> = None;
    let mut last_ticks: i64 = -1;
    let mut last_micros: Option<u32> = None;
    let mut rbuf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 16384];
    let mut last_batt = Instant::now();
    // When preview last stopped (or the session began): sign out after IDLE_SIGN_OUT.
    let mut idle_since = Instant::now();
    let mut pending = first;
    session.set_read_timeout(Duration::from_millis(200));

    while shared.alive.load(Ordering::Relaxed) {
        // No preview: run a waiting SD download now, on this session.
        if !previewing {
            if let Some((from_ms, to_ms, reply)) = downloads.pop_front() {
                // The card is indexed by the camera's own clock: read it now.
                if let Some(off) = session.read_card_clock() {
                    shared.card_offset_s.store(off, Ordering::Relaxed);
                }
                let card = Some(shared.card_offset_s.load(Ordering::Relaxed))
                    .filter(|&v| v != i64::MIN)
                    .or_else(|| Some(shared.clock_offset_s.load(Ordering::Relaxed)).filter(|&v| v != i64::MIN));
                let bytes = match (on_camera_clock(from_ms, card), on_camera_clock(to_ms, card)) {
                    (Some(from), Some(to)) => {
                        let b = session.download(from, to, DOWNLOAD_LIMIT, &shared.preview_wanted);
                        info!("baichuan: SD recording {from} - {to} (camera clock): {} KB", b.len() / 1024);
                        b
                    }
                    _ => Vec::new(),
                };
                let aborted = shared.preview_wanted.load(Ordering::Relaxed);
                let _ = reply.send(Download { bytes, card_offset_s: card, aborted });
                idle_since = Instant::now();
                continue;
            }
        }
        // Idle: sign out so the camera can sleep. (A request already queued in
        // the channel is not lost: run() picks it up and signs in again.)
        if !previewing && pending.is_none() && downloads.is_empty() && idle_since.elapsed() >= IDLE_SIGN_OUT {
            return;
        }
        // Commands first, so a start/stop takes effect before more media.
        while let Some(cmd) = pending.take().or_else(|| cmd_rx.try_recv().ok()) {
            match cmd {
                Cmd::StartPreview(reply) => {
                    shared.preview_wanted.store(false, Ordering::Relaxed);
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    if session.start_preview().is_ok() {
                        demux = Demux::new();
                        clock_read = false;
                        bridge = AvccBridge::new(false, 0, 0);
                        frame_tx = Some(tx);
                        previewing = true;
                        flowing = false;
                        last_start = Instant::now();
                        last_ticks = -1;
                        last_micros = None;
                        if reply.send(rx).is_err() {
                            // The caller gave up waiting (a sign-in or SD
                            // download took too long): nobody would ever stop
                            // this preview, and the camera would stream on.
                            let _ = session.stop_preview();
                            previewing = false;
                            frame_tx = None;
                            idle_since = Instant::now();
                        }
                    }
                    // On failure the reply is dropped; start_preview() times out.
                }
                Cmd::StopPreview => {
                    if previewing {
                        let _ = session.stop_preview();
                        idle_since = Instant::now();
                    }
                    previewing = false;
                    frame_tx = None;
                }
                // A few at most (one per event); more are refused (reply dropped).
                Cmd::Download { from_ms, to_ms, reply } if downloads.len() < 4 => downloads.push_back((from_ms, to_ms, reply)),
                Cmd::Download { .. } => {}
                Cmd::Shutdown => {
                    shared.alive.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }

        if last_batt.elapsed() >= Duration::from_secs(300) {
            let _ = session.request_battery();
            last_batt = Instant::now();
        }

        // One read; dispatch every complete message in the buffer.
        match session.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => rbuf.extend_from_slice(&tmp[..n]),
            Err(e) if is_wouldblock(&e) => {}
            Err(_) => return, // connection aborted -> reconnect
        }
        loop {
            match bc::parse_msg(&rbuf) {
                Ok(Some((consumed, m))) => {
                    rbuf.drain(..consumed);
                    dispatch(session, &m, event_tx, shared, previewing, &mut demux, &mut bridge, &frame_tx, &mut last_ticks, &mut last_micros, &mut flowing);
                }
                Ok(None) => break,
                Err(_) => {
                    rbuf.clear();
                    break;
                }
            }
        }

        // The live stream's first info header carries the camera's clock:
        // learn its offset from UTC once per preview (SD recordings use it).
        if previewing && !clock_read {
            if let Some(now) = demux.clip_start {
                let off = clock_offset_s(now, chrono::Utc::now().naive_utc());
                shared.clock_offset_s.store(off, Ordering::Relaxed);
                debug!("baichuan: camera clock is UTC{:+} s", off);
                clock_read = true;
            }
        }

        // Resend the Preview request until media actually flows (first request
        // is often dropped by the camera over UDP).
        if previewing && !flowing && last_start.elapsed() >= Duration::from_secs(2) {
            debug!("baichuan: resending Preview request (no media yet)");
            let _ = session.start_preview();
            last_start = Instant::now();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    session: &mut Session,
    m: &Msg,
    event_tx: &Sender<CamEvent>,
    shared: &Shared,
    previewing: bool,
    demux: &mut Demux,
    bridge: &mut AvccBridge,
    frame_tx: &Option<UnboundedSender<MediaItem>>,
    last_ticks: &mut i64,
    last_micros: &mut Option<u32>,
    flowing: &mut bool,
) {
    if !*flowing && m.header.msg_id != bc::MSG_KEEPALIVE {
        debug!("baichuan rx msg_id={} status={} body={}", m.header.msg_id, m.header.status, m.body.len());
    }
    match m.header.msg_id {
        bc::MSG_KEEPALIVE => {
            let _ = session.ack_keepalive(m.header.channel, m.header.msg_num);
        }
        bc::MSG_MOTION => {
            let (ext, _) = session.decode_msg(&m.header, &m.body);
            for ev in parse_motion(&String::from_utf8_lossy(&ext)) {
                let _ = event_tx.send(ev);
            }
        }
        bc::MSG_BATTERY => {
            let (ext, _) = session.decode_msg(&m.header, &m.body);
            if let Some(b) = parse_battery(&String::from_utf8_lossy(&ext)) {
                let _ = event_tx.send(CamEvent::Battery(b));
            }
        }
        bc::MSG_PREVIEW if previewing => {
            let (_, pay) = session.decode_msg(&m.header, &m.body);
            if !*flowing {
                debug!("baichuan preview msg: body={} off={:?} pay={}", m.body.len(), m.header.payload_offset, pay.len());
            }
            if pay.is_empty() {
                return;
            }
            for frame in demux.push(&pay) {
                if demux.h265 && !bridge.is_h265() {
                    *bridge = AvccBridge::new(true, demux.width, demux.height); // reports H.265 as unsupported
                }
                bridge.set_dimensions(demux.width, demux.height);
                let Some(bf) = bridge.push_frame(&frame.annexb) else { continue };
                if bf.new_params {
                    *shared.params.lock().unwrap() = bridge.video_params();
                }
                let ticks = next_ticks(*last_ticks, *last_micros, frame.micros);
                *last_ticks = ticks;
                *last_micros = Some(frame.micros);
                if let Some(tx) = frame_tx {
                    if tx.send(MediaItem::Video { ticks, cts: 0, data: bf.data, key: bf.key, new_params: bf.new_params }).is_ok() {
                        *flowing = true;
                    }
                }
            }
        }
        _ => {}
    }
}

/// A live frame's 90 kHz time: the previous one's plus the camera's
/// microsecond step (its counter is a u32 that wraps every ~71 minutes, so
/// steps, not absolute values, are used). The first frame is 0; a step of
/// zero or over 2 s (a stalled or reset counter) counts as one ~15 fps frame.
fn next_ticks(last_ticks: i64, last_micros: Option<u32>, micros: u32) -> i64 {
    match last_micros {
        None => 0,
        Some(p) => {
            let step = micros.wrapping_sub(p) as i64;
            last_ticks + if step == 0 || step > 2_000_000 { 6000 } else { step * 9 / 100 }
        }
    }
}

/// The encryption a camera chose in its reply to the login upgrade: the
/// response code is 0xddXX with XX = 0x12 full AES, 0x01 BC (XOR), 0x00 none.
/// Some(true) = AES, Some(false) = not AES, None = not that reply.
fn session_uses_aes(status: u16) -> Option<bool> {
    (status >> 8 == 0xdd).then_some(status & 0xff == 0x12)
}

/// A message's payload in the clear. When its extension carries
/// `<encryptLen>n</encryptLen>`, only the FIRST n bytes of the payload are
/// encrypted and the rest is sent as-is; without it the payload is plain.
/// (Decrypting all of it and cutting at n dropped everything past n: on an SD
/// download the first message of every frame lost ~8 KB and nothing decoded.)
fn decode_payload(ext: &[u8], pay_enc: &[u8], dec: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    match bc::xml_tag(&String::from_utf8_lossy(ext), "encryptLen").and_then(|s| s.trim().parse::<usize>().ok()) {
        Some(n) => {
            let n = n.min(pay_enc.len());
            let mut d = dec(&pay_enc[..n]);
            d.extend_from_slice(&pay_enc[n..]);
            d
        }
        None => pay_enc.to_vec(),
    }
}

fn is_wouldblock(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

/// Parse an AlarmEventList (msg 33) into motion/AI events. `status` "MD" means
/// motion; `AItype` names a detected object.
fn parse_motion(xml: &str) -> Vec<CamEvent> {
    let status = bc::xml_tag(xml, "status").unwrap_or("none");
    let ai = bc::xml_tag(xml, "AItype").unwrap_or("none").to_lowercase();
    let motion = status.eq_ignore_ascii_case("MD") || status.eq_ignore_ascii_case("alarm");
    vec![
        CamEvent::Motion { kind: MotionKind::Motion, active: motion },
        CamEvent::Motion { kind: MotionKind::Person, active: ai.contains("people") || ai.contains("person") },
        CamEvent::Motion { kind: MotionKind::Vehicle, active: ai.contains("vehicle") },
        CamEvent::Motion { kind: MotionKind::Animal, active: ai.contains("animal") || ai.contains("dog") || ai.contains("cat") },
    ]
}

fn parse_battery(xml: &str) -> Option<Battery> {
    let num = |t: &str| bc::xml_tag(xml, t).and_then(|v| v.trim().parse::<i32>().ok());
    let pct = num("batteryPercent")?;
    let adapter = bc::xml_tag(xml, "adapterStatus").unwrap_or("").to_string();
    let charge = bc::xml_tag(xml, "chargeStatus").unwrap_or("");
    Some(Battery {
        percent: pct,
        charging: charge.eq_ignore_ascii_case("charging") || charge.eq_ignore_ascii_case("chargeComplete"),
        adapter,
        voltage_mv: num("voltage").unwrap_or(0),
        temp_c: num("temperature").unwrap_or(0),
    })
}

// -------------------------------------------------- the low-level BC session

/// One signed-in BC session over the UDP transport. The protocol steps are the
/// ones proven in the `argus-probe` lab tool; see `super::bc` for the wire
/// format and crypto.
struct Session {
    sock: UdpTransport,
    aes: Option<bc::Aes>,
    msg_num: u16,
    battery: Option<Battery>,
}

impl Session {
    fn next_num(&mut self) -> u16 {
        self.msg_num = self.msg_num.wrapping_add(1);
        self.msg_num
    }

    fn set_read_timeout(&mut self, d: Duration) {
        self.sock.set_read_timeout(d);
    }

    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Read;
        self.sock.read(buf)
    }

    /// Discover/wake the camera, sign in (AES), and capture the battery state
    /// the camera pushes at login.
    fn login(host: &str, uid: &str, user: &str, pass: &str) -> anyhow::Result<Session> {
        use anyhow::{anyhow, bail, Context};
        let sock = UdpTransport::connect(host, uid, Duration::from_secs(15))
            .with_context(|| format!("UDP discovery to {host} (uid {uid})"))?;
        let mut s = Session { sock, aes: None, msg_num: 0, battery: None };

        // Step 1: empty "login upgrade" requesting AES; the camera returns an
        // <Encryption> nonce. No credentials sent, so this cannot lock out.
        let h = Header {
            msg_id: bc::MSG_LOGIN, body_len: 0, channel: 0, stream: 0,
            msg_num: s.next_num(), status: 0xdc12, class: bc::CLASS_LEGACY, payload_offset: None,
        };
        bc::write_msg(&mut s.sock, &h, &[]).context("sending login-upgrade")?;
        let raw = s.drain(Duration::from_secs(6))?;
        if raw.is_empty() {
            bail!("camera accepted the connection but sent nothing (the app may hold its single session)");
        }
        let mut nonce = None;
        // The encryption the camera chose for the session (see `session_uses_aes`).
        let mut uses_aes = true;
        let mut cur = std::io::Cursor::new(raw);
        while let Ok(reply) = bc::read_msg(&mut cur) {
            if reply.header.msg_id == bc::MSG_LOGIN {
                if let Some(aes) = session_uses_aes(reply.header.status) {
                    uses_aes = aes;
                }
            }
            for cand in [bc::xor_crypt(reply.header.channel as u32, &reply.body), bc::xor_crypt(0, &reply.body)] {
                if let Some(x) = bc::xml_tag(&String::from_utf8_lossy(&cand), "nonce") {
                    nonce = Some(x.to_string());
                }
            }
        }
        let nonce = nonce.ok_or_else(|| anyhow!("camera sent no login nonce"))?;

        // Step 2: modern sign-in. Body is BC/XOR (not AES during login).
        let body_xml = modern_login_xml(user, &nonce, pass);
        let enc = bc::xor_crypt(0, body_xml.as_bytes());
        let h = Header {
            msg_id: bc::MSG_LOGIN, body_len: 0, channel: 0, stream: 0,
            msg_num: s.next_num(), status: 0, class: bc::CLASS_MODERN_OFS, payload_offset: Some(0),
        };
        bc::write_msg(&mut s.sock, &h, &enc).context("sending sign-in")?;
        let raw = s.drain(Duration::from_secs(6))?;
        let aes = bc::Aes::new(&nonce, pass);
        let mut ok = false;
        let mut cur = std::io::Cursor::new(raw);
        while let Ok(reply) = bc::read_msg(&mut cur) {
            let off = reply.header.payload_offset.unwrap_or(0) as usize;
            let (e, p): (&[u8], &[u8]) = if off > 0 && off <= reply.body.len() {
                (&reply.body[..off], &reply.body[off..])
            } else {
                (&reply.body[..], &[])
            };
            let dec = |d: &[u8]| -> String {
                let x = String::from_utf8_lossy(&bc::xor_crypt(reply.header.channel as u32, d)).to_string();
                if x.contains('<') { x } else { String::from_utf8_lossy(&aes.decrypt(d)).to_string() }
            };
            let txt = format!("{}{}", dec(e), dec(p));
            if reply.header.status == 200 || txt.contains("DeviceInfo") {
                ok = true;
            }
            if txt.contains("batteryPercent") {
                s.battery = parse_battery(&txt);
            }
        }
        if !ok {
            bail!("sign-in not accepted (no 200 / DeviceInfo)");
        }
        // Encrypt from now on the way the camera chose: AES only if it said so.
        // Older firmware (Argus 2 0112) picks BC/XOR and cannot read AES requests
        // (it answered Preview 400 or not at all).
        s.aes = uses_aes.then_some(aes);
        info!("baichuan '{host}': signed in ({})", if uses_aes { "AES" } else { "BC/XOR" });
        Ok(s)
    }

    /// Read bytes until the socket is quiet for a moment or closes.
    fn drain(&mut self, total: Duration) -> anyhow::Result<Vec<u8>> {
        use std::io::Read;
        self.sock.set_read_timeout(Duration::from_millis(600));
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + total;
        while Instant::now() < deadline && raw.len() < 1 << 20 {
            match self.sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e) if is_wouldblock(&e) => {
                    if !raw.is_empty() {
                        break;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(raw)
    }

    /// Split a message body into (decrypted extension, payload); see the probe
    /// for the AES/BC rules. `off == 0` means no extension, the whole body is
    /// payload (a mid-frame video continuation).
    fn decode_msg(&self, header: &Header, body: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let off = header.payload_offset.unwrap_or(0) as usize;
        let (ext_enc, pay_enc): (&[u8], &[u8]) = if off <= body.len() {
            (&body[..off], &body[off..])
        } else {
            (&body[..], &[])
        };
        let dec = |data: &[u8]| -> Vec<u8> {
            match &self.aes {
                Some(a) => a.decrypt(data),
                None => bc::xor_crypt(header.channel as u32, data),
            }
        };
        let ext = dec(ext_enc);
        let pay = decode_payload(&ext, pay_enc, dec);
        (ext, pay)
    }

    fn start_preview(&mut self) -> anyhow::Result<()> {
        // Sub stream, official-client form: handle 256, stream byte 1.
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Preview version=\"1.1\">\n\
                   <channelId>0</channelId>\n<handle>256</handle>\n<streamType>subStream</streamType>\n</Preview>\n</body>\n";
        self.send_xml(bc::MSG_PREVIEW, 1, xml)
    }

    fn stop_preview(&mut self) -> anyhow::Result<()> {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Preview version=\"1.1\">\n\
                   <channelId>0</channelId>\n<handle>0</handle>\n</Preview>\n</body>\n";
        self.send_xml(bc::MSG_PREVIEW_STOP, 0, xml)
    }

    fn logout(&mut self) -> anyhow::Result<()> {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LoginUser version=\"1.1\"/>\n</body>\n";
        self.send_xml(bc::MSG_LOGOUT, 0, xml)
    }

    #[allow(dead_code)] // kept for models that actually push motion (not Argus 2)
    fn subscribe_motion(&mut self) -> anyhow::Result<()> {
        self.send_modern(bc::MSG_MOTION_REQUEST, "", "")
    }

    fn request_battery(&mut self) -> anyhow::Result<()> {
        let ext = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<channelId>0</channelId>\n</Extension>\n";
        self.send_modern(bc::MSG_BATTERY, ext, "")
    }

    /// Stream the SD-card recording for [from, to] (camera local time) over
    /// cmd 143 and return the raw BcMedia bytes, until the camera's empty
    /// end-of-recording message, `limit`, or MAX_DOWNLOAD_BYTES. A camera
    /// still waking answers 400 at first: asked again a few times. Stopped
    /// early, the stream is ended with an empty cmd 144 and its answer awaited;
    /// without that the camera stops answering anything else on the session.
    /// `abort` cuts it short (live video is wanted: it comes first).
    fn download(&mut self, from: NaiveDateTime, to: NaiveDateTime, limit: Duration, abort: &AtomicBool) -> Vec<u8> {
        let mut out = Vec::new();
        let until = Instant::now() + limit;
        let (mut buf, mut tmp) = (Vec::new(), [0u8; 16384]);
        let mut asks = 0;
        'ask: while asks < 4 && Instant::now() < until {
            asks += 1;
            if self.send_xml(bc::MSG_DOWNLOAD, 0, &download_xml(from, to)).is_err() {
                return out;
            }
            while Instant::now() < until && out.len() < MAX_DOWNLOAD_BYTES && !abort.load(Ordering::Relaxed) {
                match self.read(&mut tmp) {
                    Ok(0) => return out,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(e) if is_wouldblock(&e) => {}
                    Err(_) => return out,
                }
                loop {
                    match bc::parse_msg(&buf) {
                        Ok(Some((used, m))) => {
                            buf.drain(..used);
                            if m.header.msg_id == bc::MSG_KEEPALIVE {
                                let _ = self.ack_keepalive(m.header.channel, m.header.msg_num);
                                continue;
                            }
                            if m.header.msg_id != bc::MSG_DOWNLOAD {
                                continue;
                            }
                            if m.header.status == 400 && out.is_empty() {
                                // Waking, or nothing recorded then: ask again shortly.
                                std::thread::sleep(Duration::from_millis(1500));
                                continue 'ask;
                            }
                            if m.body.is_empty() {
                                return out; // end of the recording
                            }
                            let (_, pay) = self.decode_msg(&m.header, &m.body);
                            if pay.is_empty() {
                                if m.header.status != 200 {
                                    return out; // refused
                                }
                                continue; // a control reply, not media
                            }
                            out.extend_from_slice(&pay);
                        }
                        Ok(None) => break,
                        Err(_) => {
                            buf.clear();
                            break;
                        }
                    }
                }
            }
            // Out of time or room before the end: stop the stream properly.
            self.stop_download();
            return out;
        }
        out
    }

    /// End a cmd 143 stream early: an empty cmd 144, then wait (up to 3 s)
    /// for its answer, dropping the stream's tail meanwhile.
    fn stop_download(&mut self) {
        if self.send_modern(bc::MSG_DOWNLOAD_STOP, "", "").is_err() {
            return;
        }
        let until = Instant::now() + Duration::from_secs(3);
        let (mut buf, mut tmp) = (Vec::new(), [0u8; 16384]);
        while Instant::now() < until {
            match self.read(&mut tmp) {
                Ok(0) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e) if is_wouldblock(&e) => {}
                Err(_) => return,
            }
            while let Ok(Some((used, m))) = bc::parse_msg(&buf) {
                buf.drain(..used);
                match m.header.msg_id {
                    bc::MSG_KEEPALIVE => {
                        let _ = self.ack_keepalive(m.header.channel, m.header.msg_num);
                    }
                    bc::MSG_DOWNLOAD_STOP => return,
                    _ => {}
                }
            }
        }
    }

    /// Ask the camera for its clock (cmd 104) and return its offset from UTC
    /// in seconds; `None` if it did not answer within 3 s.
    fn read_card_clock(&mut self) -> Option<i64> {
        self.send_modern(bc::MSG_GENERAL, "", "").ok()?;
        let until = Instant::now() + Duration::from_secs(3);
        let (mut buf, mut tmp) = (Vec::new(), [0u8; 16384]);
        while Instant::now() < until {
            match self.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e) if is_wouldblock(&e) => {}
                Err(_) => return None,
            }
            while let Ok(Some((used, m))) = bc::parse_msg(&buf) {
                buf.drain(..used);
                if m.header.msg_id == bc::MSG_KEEPALIVE {
                    let _ = self.ack_keepalive(m.header.channel, m.header.msg_num);
                    continue;
                }
                if m.header.msg_id != bc::MSG_GENERAL || m.header.status != 200 {
                    continue;
                }
                // A plain XML reply: the whole body is encrypted, no extension.
                let (ext, _) = self.decode_msg(&m.header, &m.body);
                let xml = if ext.is_empty() {
                    match &self.aes {
                        Some(a) => a.decrypt(&m.body),
                        None => bc::xor_crypt(m.header.channel as u32, &m.body),
                    }
                } else {
                    ext
                };
                let off = card_offset_s(&String::from_utf8_lossy(&xml), chrono::Utc::now().naive_utc());
                debug!("baichuan: camera clock (card) is UTC{:+?} s", off);
                return off;
            }
        }
        None
    }

    fn ack_keepalive(&mut self, channel: u8, msg_num: u16) -> anyhow::Result<()> {
        let h = Header {
            msg_id: bc::MSG_KEEPALIVE, body_len: 0, channel, stream: 0, msg_num,
            status: 200, class: bc::CLASS_MODERN_OFS, payload_offset: Some(0),
        };
        bc::write_msg(&mut self.sock, &h, &[])
    }

    fn send_modern(&mut self, msg_id: u32, ext_xml: &str, pay_xml: &str) -> anyhow::Result<()> {
        let enc = |s: &str, aes: &Option<bc::Aes>| -> Vec<u8> {
            if s.is_empty() {
                return Vec::new();
            }
            match aes {
                Some(a) => a.encrypt(s.as_bytes()),
                None => bc::xor_crypt(0, s.as_bytes()),
            }
        };
        let ext = enc(ext_xml, &self.aes);
        let pay = enc(pay_xml, &self.aes);
        let mut body = ext.clone();
        body.extend_from_slice(&pay);
        let num = self.next_num();
        let h = Header {
            msg_id, body_len: 0, channel: 0, stream: 0, msg_num: num,
            status: 0, class: bc::CLASS_MODERN_OFS, payload_offset: Some(ext.len() as u32),
        };
        bc::write_msg(&mut self.sock, &h, &body)
    }

    fn send_xml(&mut self, msg_id: u32, stream_code: u8, xml: &str) -> anyhow::Result<()> {
        let body = match &self.aes {
            Some(a) => a.encrypt(xml.as_bytes()),
            None => bc::xor_crypt(0, xml.as_bytes()),
        };
        let num = self.next_num();
        let h = Header {
            msg_id, body_len: 0, channel: 0, stream: stream_code, msg_num: num,
            status: 0, class: bc::CLASS_MODERN_OFS, payload_offset: Some(0),
        };
        bc::write_msg(&mut self.sock, &h, &body)
    }
}

/// The modern sign-in body: userName/password = MD5upper(value+nonce)[..31],
/// concatenated with no separator (the "-" form is only the AES key).
fn modern_login_xml(user: &str, nonce: &str, pass: &str) -> String {
    let user = bc::md5_31_upper(&format!("{user}{nonce}"));
    let pw = bc::md5_31_upper(&format!("{pass}{nonce}"));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LoginUser version=\"1.1\">\n\
         <userName>{user}</userName>\n<password>{pw}</password>\n<userVer>1</userVer>\n</LoginUser>\n\
         <LoginNet version=\"1.1\">\n<type>LAN</type>\n<udpPort>0</udpPort>\n</LoginNet>\n</body>\n"
    )
}

#[cfg(test)]
mod clock_tests {
    use super::clock_offset_s;

    #[test]
    fn live_frame_times_survive_the_camera_counter_wrapping() {
        use super::next_ticks;
        assert_eq!(next_ticks(-1, None, 123_456), 0);
        assert_eq!(next_ticks(0, Some(1_000_000), 1_066_666), 5_999);
        // Across the u32 wrap: 66 ms later, not 71 minutes earlier.
        assert_eq!(next_ticks(9_000, Some(u32::MAX - 33_000), 33_000), 9_000 + 66_001 * 9 / 100);
        assert_eq!(next_ticks(9_000, Some(5), 5), 15_000, "a stalled counter");
        assert_eq!(next_ticks(9_000, Some(0), 10_000_000), 15_000, "a reset counter");
    }

    #[test]
    fn the_card_clock_comes_from_the_general_reply() {
        use super::card_offset_s;
        let d = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        // An Argus 2 reply (older firmware, camera set to UTC-7).
        let xml = "<body>\n    <SystemGeneral version=\"1.1\">\n        <timeZone>25200</timeZone>\n        <osdFormat>MDY</osdFormat>\n        \
                   <timeFormat>1</timeFormat>\n        <year>2026</year>\n        <month>11</month>\n        <day>11</day>\n        \
                   <hour>16</hour>\n        <minute>26</minute>\n        <second>38</second>\n    </SystemGeneral>\n</body>\n";
        assert_eq!(card_offset_s(xml, d("2026-11-11 23:26:36")), Some(-7 * 3600 + 2), "clock fields, drift included");
        assert_eq!(card_offset_s("<timeZone>21600</timeZone>", d("2026-11-11 23:26:36")), Some(-6 * 3600), "zone only");
        assert_eq!(card_offset_s("<x/>", d("2026-11-11 23:26:36")), None);
    }

    #[test]
    fn the_camera_clock_offset_includes_zone_and_drift() {
        let d = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        // A camera on UTC-7 read at 19:27:09 UTC, its clock 2 s fast.
        assert_eq!(clock_offset_s(d("2026-11-11 12:27:11"), d("2026-11-11 19:27:09")), -7 * 3600 + 2);
    }
}

#[cfg(test)]
mod download_tests {
    use super::download_xml;

    #[test]
    fn the_download_request_has_both_times_unpadded() {
        let d = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        let x = download_xml(d("2026-11-11 09:05:07"), d("2026-11-11 09:05:37"));
        assert!(x.contains("<FileInfoList version=\"1.1\">") && x.contains("<streamType>mainStream</streamType>"));
        let start = x.split("<startTime>").nth(1).unwrap().split("</startTime>").next().unwrap();
        assert!(start.contains("<year>2026</year>") && start.contains("<hour>9</hour>") && start.contains("<minute>5</minute>") && start.contains("<second>7</second>"), "{start}");
        assert!(x.contains("<endTime>") && x.contains("<second>37</second>"));
    }
}

#[cfg(test)]
mod encryption_tests {
    use super::session_uses_aes;

    #[test]
    fn the_session_uses_the_encryption_the_camera_chose() {
        assert_eq!(session_uses_aes(0xdd12), Some(true), "newer firmware: full AES");
        assert_eq!(session_uses_aes(0xdd01), Some(false), "Argus 2 0112: BC/XOR, no AES");
        assert_eq!(session_uses_aes(0xdd00), Some(false), "no encryption");
        assert_eq!(session_uses_aes(200), None, "not the login-upgrade reply");
    }
}

#[cfg(test)]
mod payload_tests {
    use super::decode_payload;

    // A stand-in cipher: flip every byte.
    fn flip(d: &[u8]) -> Vec<u8> {
        d.iter().map(|b| !b).collect()
    }

    #[test]
    fn only_the_first_encrypt_len_bytes_are_decrypted_and_the_rest_is_kept() {
        let plain: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        let mut wire = flip(&plain[..37]);
        wire.extend_from_slice(&plain[37..]);
        let ext = b"<Extension><encryptLen>37</encryptLen></Extension>";
        assert_eq!(decode_payload(ext, &wire, flip), plain, "all 8192 bytes, first 37 decrypted");
    }

    #[test]
    fn no_encrypt_len_means_plain_and_a_large_one_decrypts_everything() {
        let data = vec![7u8; 100];
        assert_eq!(decode_payload(b"<Extension></Extension>", &data, flip), data);
        let ext = b"<Extension><encryptLen>4096</encryptLen></Extension>";
        assert_eq!(decode_payload(ext, &flip(&data), flip), data, "n past the end is capped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_motion_maps_status_and_ai() {
        let xml = "<status>MD</status><AItype>people</AItype>";
        let evs = parse_motion(xml);
        let active: Vec<(MotionKind, bool)> = evs
            .iter()
            .map(|e| match e {
                CamEvent::Motion { kind, active } => (*kind, *active),
                _ => unreachable!(),
            })
            .collect();
        assert!(active.contains(&(MotionKind::Motion, true)));
        assert!(active.contains(&(MotionKind::Person, true)));
        assert!(active.contains(&(MotionKind::Vehicle, false)));
        assert!(active.contains(&(MotionKind::Animal, false)));
    }

    #[test]
    fn parse_motion_none_is_all_inactive() {
        for e in parse_motion("<status>none</status><AItype>none</AItype>") {
            assert!(matches!(e, CamEvent::Motion { active: false, .. }));
        }
    }

    #[test]
    fn parse_battery_reads_fields() {
        let xml = "<batteryPercent>89</batteryPercent><chargeStatus>charging</chargeStatus>\
                   <adapterStatus>solarPanel</adapterStatus><voltage>3845</voltage><temperature>16</temperature>";
        let b = parse_battery(xml).unwrap();
        assert_eq!(b.percent, 89);
        assert!(b.charging);
        assert_eq!(b.adapter, "solarPanel");
        assert_eq!((b.voltage_mv, b.temp_c), (3845, 16));
    }

    #[test]
    fn login_xml_has_hashed_credentials() {
        let xml = modern_login_xml("admin", "nonce123", "secret");
        assert!(xml.contains("<userName>") && xml.contains("<password>"));
        assert!(!xml.contains("secret"), "the password never appears in the clear");
    }
}
