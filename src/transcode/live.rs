//! Live transcodes: one ffmpeg per camera stream and output, fed the
//! camera's live fragments (no second connection to the camera), its
//! output cut into fragments on a hub of its own that every viewer of that
//! output shares. It stops a few seconds after the last viewer leaves, and
//! when the camera's stream restarts (viewers reconnect and start a new one).

use super::mp4::{self, Piece, Splitter, VideoCodec};
use super::{Encoder, OutCodec, Slot, Target, UserLease};
use crate::ingest::live::{LiveHub, LiveInfo, LiveMsg};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// What viewers share: one camera stream made into one output. (Not the
/// bit rate, which follows the measured frame rate: it is set when the
/// transcode starts.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub camera: usize,
    pub sub: bool,
    pub codec: OutCodec,
    pub width: u32,
    pub height: u32,
    pub fps: Option<u32>,
    pub audio: bool,
}

impl Key {
    pub fn new(camera: usize, sub: bool, t: &Target, audio: bool) -> Key {
        Key { camera, sub, codec: t.codec, width: t.width, height: t.height, fps: t.fps, audio }
    }
}

pub struct Session {
    pub hub: Arc<LiveHub>,
    /// What it makes (with the bit rate it started with).
    pub target: Target,
    /// The encoder in use (API name), once it is running.
    pub encoder: Mutex<Option<String>>,
    stop: AtomicBool,
    /// Ended (no more output will come).
    done: AtomicBool,
    /// Each viewer's last fragment taken (ms since the session began).
    viewers: Mutex<Vec<Weak<AtomicU64>>>,
    began: Instant,
    ever_viewed: AtomicBool,
    /// When a viewer last found it (set under the sessions lock): a
    /// session is not stopped for want of viewers just after one joined.
    last_join: Mutex<Instant>,
}

/// A viewer of a session: note each fragment it takes with `seen`.
pub struct Viewer(Arc<AtomicU64>, Instant);

impl Viewer {
    pub fn seen(&self) {
        self.0.store(self.1.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

/// A viewer that has taken no fragment for this long (a stalled
/// connection) no longer keeps the transcode running.
const STALLED: Duration = Duration::from_secs(10);

impl Session {
    pub fn ended(&self) -> bool {
        self.done.load(Ordering::SeqCst) || self.stop.load(Ordering::SeqCst)
    }

    pub fn join(&self) -> Viewer {
        let v = Arc::new(AtomicU64::new(self.began.elapsed().as_millis() as u64));
        self.viewers.lock().unwrap().push(Arc::downgrade(&v));
        Viewer(v, self.began)
    }

    /// Viewers still taking fragments.
    pub fn active_viewers(&self) -> usize {
        let now = self.began.elapsed().as_millis() as u64;
        let mut v = self.viewers.lock().unwrap();
        v.retain(|w| w.strong_count() > 0);
        let n = v.iter().filter_map(|w| w.upgrade()).filter(|a| now.saturating_sub(a.load(Ordering::Relaxed)) < STALLED.as_millis() as u64).count();
        if n > 0 {
            self.ever_viewed.store(true, Ordering::SeqCst);
        }
        n
    }
}

/// Ends a session however its thread ends (a panic too), so its key is
/// never left to a session that will not run.
struct EndGuard {
    sess: Arc<Session>,
    key: Key,
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        self.sess.done.store(true, Ordering::SeqCst);
        self.sess.hub.stop();
        if let Ok(mut m) = SESSIONS.lock() {
            if let Some(m) = m.as_mut() {
                if m.get(&self.key).is_some_and(|s| Arc::ptr_eq(s, &self.sess)) {
                    m.remove(&self.key);
                }
            }
        }
    }
}

static SESSIONS: Mutex<Option<HashMap<Key, Arc<Session>>>> = Mutex::new(None);

/// Stops this long after the last viewer leaves.
const LINGER: Duration = Duration::from_secs(5);
/// ...and this long after its first picture when nobody came to watch it.
const NO_VIEWER: Duration = Duration::from_secs(3);
/// An ffmpeg that has made no picture by then has failed (the next way is
/// tried).
const FIRST_OUTPUT: Duration = Duration::from_secs(8);
/// A viewer waits this long for the first picture before it is sent the
/// best untouched stream instead (players give up after about 8 s).
const VIEWER_WAIT: Duration = Duration::from_secs(7);
/// An attempt that made fewer fragments than this before it ended did not
/// really work (the next way is tried).
const MIN_FRAGMENTS: usize = 10;
/// Frames a stream must show without reordering before its decoder is
/// told not to wait for reordered frames (reordering is recognized within
/// 90 frames of RTSP).
const NO_REORDER_FRAMES: u32 = 120;

/// The running session for this key, if any.
pub fn existing(key: &Key) -> Option<Arc<Session>> {
    SESSIONS.lock().unwrap().as_ref()?.get(key).filter(|s| !s.ended()).cloned()
}

/// The running session for this key, or a new one with these encoders
/// (best first) when a place is free. A new one takes the place `starter`
/// gives (the starting account's, held while it runs; asked for only when
/// a new one is needed and a place on the encoder is had). None: busy, or
/// the account may not start one.
#[allow(clippy::too_many_arguments)]
pub fn get_or_start(key: Key, target: Target, camera: &str, source: Arc<LiveHub>, encoders: &[Encoder], fps: f64, starter: impl FnOnce() -> Option<UserLease>) -> Option<Arc<Session>> {
    let mut map = SESSIONS.lock().unwrap();
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&key).filter(|s| !s.ended()) {
        *s.last_join.lock().unwrap() = Instant::now();
        return Some(s.clone());
    }
    // The place on the encoder first: the account's start is counted only when one is had.
    let (chosen, slot) = super::reserve(encoders)?;
    let lease = starter()?;
    // On the graphics card, software stays a way out (if it worked in the
    // test encode) when the card will not start this one.
    let software_after = slot.hardware && encoders.iter().any(|e| e.hw.is_none());
    let sess = Arc::new(Session {
        hub: Arc::new(LiveHub::new()),
        target,
        encoder: Mutex::new(None),
        stop: AtomicBool::new(false),
        done: AtomicBool::new(false),
        viewers: Mutex::new(Vec::new()),
        began: Instant::now(),
        ever_viewed: AtomicBool::new(false),
        last_join: Mutex::new(Instant::now()),
    });
    let s2 = sess.clone();
    let camera = camera.to_string();
    let k2 = key.clone();
    std::thread::Builder::new()
        .name("live-transcode".into())
        .spawn(move || run(s2, k2, camera, source, chosen, software_after, slot, lease, fps))
        .ok()?;
    map.insert(key, sess.clone());
    Some(sess)
}

/// Wait for the session's first output; then subscribe like to a camera hub.
pub async fn wait_ready(s: &Arc<Session>) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < VIEWER_WAIT {
        if s.hub.info().is_some() {
            return true;
        }
        if s.done.load(Ordering::SeqCst) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    false
}

/// (encoder, decode on the graphics card) to try, in order.
pub(crate) fn attempts(encoders: &[Encoder]) -> Vec<(Encoder, bool)> {
    let mut v = Vec::new();
    for e in encoders {
        if e.hw.is_some() {
            v.push((e.clone(), true));
        }
        v.push((e.clone(), false));
    }
    v
}

#[allow(clippy::too_many_arguments)]
fn run(sess: Arc<Session>, key: Key, camera: String, source: Arc<LiveHub>, encoders: Vec<Encoder>, software_after: bool, slot: Slot, lease: UserLease, fps: f64) {
    let s2 = sess.clone();
    let listed = super::list_active("live", &format!("{}{}", camera, if key.sub { " (sub)" } else { "" }), sess.target, Box::new(move || s2.active_viewers()));
    let _end = EndGuard { sess: sess.clone(), key: key.clone() };
    let mut slot = slot;
    let mut list = attempts(&encoders);
    let mut i = 0;
    // Set once the stream showed reordered frames: no low-delay decoding.
    let mut plain = false;
    let mut first = true;
    loop {
        if sess.stop.load(Ordering::SeqCst) {
            break;
        }
        // Before another try: nobody watching, and nobody found it within the time a viewer
        // waits for the first picture (they have all been sent another stream): stop rather
        // than hold a place for a minute of tries.
        if !first && sess.active_viewers() == 0 && stop_unless_joined(&sess, &key, VIEWER_WAIT) {
            break;
        }
        first = false;
        if i >= list.len() {
            // Every way on the card failed: software, if it has a place.
            match (software_after && slot.hardware).then(super::reserve_software).flatten() {
                Some(sw) => {
                    slot = sw;
                    list.push((Encoder::software(), false));
                }
                None => break,
            }
        }
        let (e, hw_decode) = list[i].clone();
        *listed.0.encoder.lock().unwrap() = e.name(key.codec).to_string();
        match attempt(&sess, &key, &camera, &source, &e, hw_decode, fps, plain, &listed) {
            Outcome::Ran | Outcome::NoSource => break,
            Outcome::Reordered => {
                tracing::info!("transcode: {camera}: the stream reorders frames; decoding again without low delay");
                plain = true;
            }
            Outcome::Failed(why) => {
                let how = if hw_decode { "decoding on the graphics card" } else { "decoding in software" };
                super::warn_limited(&format!("live {camera} {} {hw_decode}", e.name(key.codec)), &format!("transcode: {camera}: {} ({how}) did not start: {why}", e.name(key.codec)));
                i += 1;
            }
        }
    }
    drop(_end);
    drop(slot);
    drop(lease);
    drop(listed);
}

enum Outcome {
    /// It made pictures (and has ended now).
    Ran,
    Failed(String),
    /// The camera's stream is not running.
    NoSource,
    /// Decoding with low delay, a reordered frame came: again without.
    Reordered,
}

/// The ffmpeg arguments for a live transcode reading fragments on stdin.
/// `low_delay`: the stream never reorders frames, so the decoder may hand
/// each frame on at once (else an HEVC decoder holds frames for the reorder
/// buffer its stream header allows: measured 0.77 s more delay for a
/// 4608x1728 HEVC camera).
pub fn args(e: &Encoder, hw_decode: bool, t: &Target, fps: f64, depth: u8, audio: bool, low_delay: bool) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error"].iter().map(|s| s.to_string()).collect();
    a.extend(super::device_args(e, hw_decode));
    if low_delay {
        a.extend(["-flags".to_string(), "low_delay".to_string()]);
    }
    // (Not -fflags nobuffer: it drops the packets read while probing, the
    // keyframe among them, and the picture then waits for the next one.)
    a.extend(["-probesize", "65536", "-analyzeduration", "0", "-f", "mp4", "-i", "pipe:0", "-map", "0:v:0"].iter().map(|s| s.to_string()));
    if audio {
        a.extend(["-map", "0:a:0?", "-c:a", "copy"].iter().map(|s| s.to_string()));
    } else {
        a.push("-an".into());
    }
    a.extend(["-vf".to_string(), super::filters(e, hw_decode, t, depth)]);
    a.extend(super::encoder_args(e, t, fps, true));
    a.extend(
        ["-f", "mp4", "-movflags", "empty_moov+default_base_moof+frag_every_frame", "-flush_packets", "1", "-max_interleave_delta", "500000", "pipe:1"]
            .iter()
            .map(|s| s.to_string()),
    );
    a
}

/// Whether a fragment of this stream shows a frame later than it is
/// decoded.
fn reorders(f: &[u8], d: &mp4::VideoDesc) -> bool {
    mp4::boxes(f).into_iter().find(|(k, _)| k == b"moof").is_some_and(|(_, m)| mp4::moof_video(m, d.track_id, d.trex_flags, d.trex_duration).reorders)
}

#[allow(clippy::too_many_arguments)]
fn attempt(sess: &Arc<Session>, key: &Key, camera: &str, source: &Arc<LiveHub>, e: &Encoder, hw_decode: bool, fps: f64, plain: bool, listed: &super::Listed) -> Outcome {
    let Some((init, gop, rx)) = source.subscribe_for(!key.audio) else { return Outcome::NoSource };
    let src_info = source.info();
    let desc = mp4::probe(&init);
    let depth = desc.as_ref().map(|d| d.bit_depth).unwrap_or(8);
    // Low delay only on positive evidence: an HEVC stream that has shown
    // enough frames, none reordered (H.264 needs no flag: the hub's
    // low-delay header already says so to the decoder when it is right).
    let low_delay = !plain
        && desc.as_ref().is_some_and(|d| d.codec == VideoCodec::Hevc && !gop.iter().any(|f| reorders(f, d)))
        && source.never_reordered(NO_REORDER_FRAMES);
    let a = args(e, hw_decode, &sess.target, fps, depth, key.audio, low_delay);
    let child = match crate::child::ffmpeg().args(&a).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(err) => return Outcome::Failed(format!("ffmpeg: {err}")),
    };
    crate::child::adopt(&child);
    *listed.0.pid.lock().unwrap() = Some(child.id());
    let child = Arc::new(Mutex::new(child));
    let (stdin, mut stdout, stderr) = {
        let mut c = child.lock().unwrap();
        (c.stdin.take(), c.stdout.take().unwrap(), c.stderr.take().unwrap())
    };
    let errs = keep_tail(stderr);
    let reordered = Arc::new(AtomicBool::new(false));
    // Feed: the stream's header, its fragments since the last keyframe, then
    // live fragments until the stream restarts or ffmpeg goes. With low
    // delay, a reordered frame stops this attempt (before ffmpeg sees it).
    if let Some(mut stdin) = stdin {
        let mut rx = rx;
        let quiet = !key.audio;
        let (child, reordered) = (child.clone(), reordered.clone());
        let watch = desc.clone().filter(|_| low_delay);
        std::thread::spawn(move || {
            for b in std::iter::once(init).chain(gop) {
                if stdin.write_all(&b).is_err() {
                    return;
                }
            }
            loop {
                match rx.blocking_recv() {
                    Ok(LiveMsg::Fragment(f)) => {
                        if watch.as_ref().is_some_and(|d| reorders(&f, d)) {
                            reordered.store(true, Ordering::SeqCst);
                            let _ = child.lock().unwrap().kill();
                            return;
                        }
                        let f = if quiet { crate::ingest::fmp4::video_only(&f).unwrap_or_else(|| f.to_vec()) } else { f.to_vec() };
                        if stdin.write_all(&f).is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Ok(LiveMsg::Reset) | Err(_) => return,
                }
            }
        });
    }
    let got_init = Arc::new(AtomicBool::new(false));
    let frags = Arc::new(AtomicUsize::new(0));
    // Watch: no output in time (failed), or nobody watching for a while.
    let watching = Arc::new(AtomicBool::new(true));
    {
        let (child, got_init, watching, sess, key) = (child.clone(), got_init.clone(), watching.clone(), sess.clone(), key.clone());
        let t0 = Instant::now();
        std::thread::spawn(move || {
            let mut idle_since: Option<Instant> = None;
            while watching.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(250));
                let kill = if sess.stop.load(Ordering::SeqCst) {
                    true
                } else if !got_init.load(Ordering::SeqCst) {
                    t0.elapsed() > FIRST_OUTPUT
                } else if sess.active_viewers() == 0 {
                    let since = *idle_since.get_or_insert_with(Instant::now);
                    let wait = if sess.ever_viewed.load(Ordering::SeqCst) { LINGER } else { NO_VIEWER };
                    // Stopped under the sessions lock, so a viewer finding
                    // it now is not cut off (it would have just joined).
                    since.elapsed() >= wait && stop_unless_joined(&sess, &key, wait)
                } else {
                    idle_since = None;
                    false
                };
                if kill {
                    let _ = child.lock().unwrap().kill();
                    return;
                }
            }
        });
    }
    let t0 = Instant::now();
    let mut split = Splitter::default();
    let mut pieces = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut video: Option<mp4::VideoDesc> = None;
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if split.push(&buf[..n], &mut pieces).is_err() {
            break;
        }
        for p in pieces.drain(..) {
            match p {
                Piece::Init(init) => {
                    let moov = mp4::boxes(&init).into_iter().find(|(k, _)| k == b"moov").map(|(_, b)| b);
                    let Some(v) = moov.and_then(mp4::video_desc) else { continue };
                    let vcodec = v.codec_string.clone().unwrap_or_else(|| if key.codec == OutCodec::H264 { "avc1.640033".into() } else { "hvc1.1.6.L153.B0".into() });
                    // Sound is passed through: its codec is the camera stream's.
                    let audio_codec = src_info.as_ref().filter(|i| i.audio && key.audio).and_then(|i| {
                        let inside = i.mime.split_once("codecs=\"")?.1.trim_end_matches('"');
                        inside.split(',').nth(1).map(|c| c.trim().to_string())
                    });
                    let has_audio = audio_codec.is_some() && mp4::boxes(moov.unwrap_or(&[])).iter().filter(|(k, _)| k == b"trak").count() > 1;
                    let mime = match (&audio_codec, has_audio) {
                        (Some(a), true) => format!("video/mp4; codecs=\"{vcodec}, {a}\""),
                        _ => format!("video/mp4; codecs=\"{vcodec}\""),
                    };
                    let info = LiveInfo { mime, width: v.width, height: v.height, audio: has_audio, video_mime: None };
                    tracing::info!(
                        "transcode: {camera}: {}x{} {} on {}{}{} after {} ms",
                        v.width,
                        v.height,
                        key.codec.name(),
                        e.name(key.codec),
                        if hw_decode { " (decoding on the graphics card)" } else { "" },
                        if low_delay { ", low delay" } else { "" },
                        t0.elapsed().as_millis()
                    );
                    *sess.encoder.lock().unwrap() = Some(e.name(key.codec).to_string());
                    sess.hub.start(init, info, None);
                    video = Some(v);
                    got_init.store(true, Ordering::SeqCst);
                }
                Piece::Fragment(f) => {
                    let Some(v) = video.as_ref() else { continue };
                    let key_frame = mp4::boxes(&f).into_iter().find(|(k, _)| k == b"moof").map(|(_, m)| mp4::moof_video(m, v.track_id, v.trex_flags, v.trex_duration)).is_some_and(|fv| fv.has_video && fv.first_sync);
                    sess.hub.publish(f, key_frame);
                    frags.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
    watching.store(false, Ordering::SeqCst);
    {
        let mut c = child.lock().unwrap();
        let _ = c.kill();
        let _ = c.wait();
    }
    if reordered.load(Ordering::SeqCst) && !sess.stop.load(Ordering::SeqCst) {
        return Outcome::Reordered;
    }
    if frags.load(Ordering::Relaxed) >= MIN_FRAGMENTS || sess.stop.load(Ordering::SeqCst) {
        return Outcome::Ran;
    }
    // The camera's stream ended (restart): not this encoder's fault.
    if got_init.load(Ordering::SeqCst) && source.info().is_none() {
        return Outcome::NoSource;
    }
    std::thread::sleep(Duration::from_millis(50));
    Outcome::Failed(tail(&errs))
}

/// The session has had no viewer for `wait`: stop it and take it off the
/// list, unless a viewer found it within that time. Under the sessions
/// lock (where viewers find it). True: stopped.
fn stop_unless_joined(sess: &Arc<Session>, key: &Key, wait: Duration) -> bool {
    let mut m = SESSIONS.lock().unwrap();
    if sess.last_join.lock().unwrap().elapsed() < wait {
        return false;
    }
    sess.stop.store(true, Ordering::SeqCst);
    if let Some(m) = m.as_mut() {
        if m.get(key).is_some_and(|s| Arc::ptr_eq(s, sess)) {
            m.remove(key);
        }
    }
    true
}

/// Reads ffmpeg's messages as they come (so it never waits on a full
/// pipe), keeping the last few kB for the log.
pub(crate) fn keep_tail(stderr: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let errs = Arc::new(Mutex::new(String::new()));
    let e2 = errs.clone();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            let mut e = e2.lock().unwrap();
            e.push_str(&line);
            e.push('\n');
            if e.len() > 4096 {
                let cut = e.len() - 4096;
                let cut = (cut..e.len()).find(|i| e.is_char_boundary(*i)).unwrap_or(0);
                e.drain(..cut);
            }
        }
    });
    errs
}

/// The last lines of ffmpeg's messages, on one line.
pub(crate) fn tail(errs: &Mutex<String>) -> String {
    let e = errs.lock().unwrap().clone();
    let t = e.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
    if t.is_empty() { "no output".into() } else { t }
}

/// Stop a child and reap it.
pub(crate) fn stop_child(c: &mut std::process::Child) {
    let _ = c.kill();
    let _ = c.wait();
}
