//! Live view: re-packages a camera stream as fragmented MP4 for browsers
//! (Media Source Extensions), without decoding or re-encoding.
//!
//! Each camera stream has a `LiveHub`. The stream session feeds a
//! `LiveMuxer`, which emits one small fragment per video frame (plus the
//! audio that arrived with it) and publishes it on the hub. A viewer gets
//! the init segment, then the fragments since the latest keyframe (so the
//! picture appears immediately), then live fragments as they come.

use super::fmp4::{self, Sample, TrackInfo, TrackKind, TrackRun};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Codec description for the browser's MediaSource.isTypeSupported().
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct LiveInfo {
    /// e.g. `video/mp4; codecs="avc1.640033, mp4a.40.2"`
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub audio: bool,
    /// With audio: the codecs of the picture-only form of this stream (what
    /// a muted view asks for with `audio=0`). A problem in a camera's audio
    /// then cannot stop the picture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_mime: Option<String>,
}

#[derive(Clone, Debug)]
pub enum LiveMsg {
    /// A media fragment (moof + mdat).
    Fragment(Arc<Vec<u8>>),
    /// The stream restarted with new parameters: viewers must reconnect.
    Reset,
}

struct HubState {
    init: Option<Arc<Vec<u8>>>,
    /// The init segment with a low-delay SPS (see `sps.rs`), once the
    /// stream's frames showed it never reorders: what new viewers get.
    low_delay: Option<Arc<Vec<u8>>>,
    /// Init segment of the picture-only form (streams with audio), and its
    /// low-delay variant.
    quiet_init: Option<Arc<Vec<u8>>>,
    quiet_low_delay: Option<Arc<Vec<u8>>>,
    info: Option<LiveInfo>,
    /// Average time between video frames (seconds) and how many were seen,
    /// for the frame rate a transcode is planned with.
    frame_secs: f64,
    frames: u32,
    /// A frame was shown later than decoded (B-frames) since the start.
    reordered: bool,
    /// Fragments since the most recent keyframe (bounded).
    gop: Vec<Arc<Vec<u8>>>,
    gop_bytes: usize,
}

/// Fan-out point for one camera stream's live fragments.
pub struct LiveHub {
    tx: broadcast::Sender<LiveMsg>,
    state: Mutex<HubState>,
}

/// GOP cache limit: a camera with a very long keyframe interval should not
/// pin unbounded memory.
const MAX_GOP_BYTES: usize = 16 * 1024 * 1024;

impl Default for LiveHub {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        LiveHub { tx, state: Mutex::new(HubState { init: None, low_delay: None, quiet_init: None, quiet_low_delay: None, info: None, frame_secs: 0.0, frames: 0, reordered: false, gop: Vec::new(), gop_bytes: 0 }) }
    }

    pub fn info(&self) -> Option<LiveInfo> {
        self.state.lock().unwrap().info.clone()
    }

    /// The stream's own init segment (None while it is not running).
    pub fn init(&self) -> Option<Arc<Vec<u8>>> {
        self.state.lock().unwrap().init.clone()
    }

    /// Video frames a second, once a few frames have shown it.
    pub fn fps(&self) -> Option<f64> {
        let st = self.state.lock().unwrap();
        (st.frames >= 10 && st.frame_secs > 0.0).then(|| 1.0 / st.frame_secs)
    }

    /// The stream has shown at least `min` frames since it started and
    /// none of them reordered (a decoder may then hand each frame on at
    /// once). Reordering is recognized within 90 frames of RTSP (see
    /// dts.rs), so ask for more than that.
    pub fn never_reordered(&self, min: u32) -> bool {
        let st = self.state.lock().unwrap();
        st.frames >= min && !st.reordered
    }

    fn note_reorder(&self) {
        self.state.lock().unwrap().reordered = true;
    }

    fn note_frame(&self, secs: f64) {
        if !(0.001..=10.0).contains(&secs) {
            return;
        }
        let mut st = self.state.lock().unwrap();
        st.frame_secs = if st.frames == 0 { secs } else { st.frame_secs * 0.9 + secs * 0.1 };
        st.frames = st.frames.saturating_add(1);
    }

    /// Subscribe: returns the init segment and cached fragments to send
    /// first, then the receiver for live fragments. None while the stream is
    /// not running.
    #[cfg(test)]
    pub fn subscribe(&self) -> Option<(Arc<Vec<u8>>, Vec<Arc<Vec<u8>>>, broadcast::Receiver<LiveMsg>)> {
        self.subscribe_for(false)
    }

    /// Like `subscribe`; with `quiet`, for a stream with audio, the init
    /// segment and cached fragments of the picture-only form. The fragments
    /// that follow are made quiet by the viewer's own task with
    /// `fmp4::video_only`, so nothing is built for viewers that are not
    /// there, and a slow one cannot hold up the muxer or other viewers.
    pub fn subscribe_for(&self, quiet: bool) -> Option<(Arc<Vec<u8>>, Vec<Arc<Vec<u8>>>, broadcast::Receiver<LiveMsg>)> {
        let st = self.state.lock().unwrap();
        let init = st.init.as_ref()?;
        let init = st.low_delay.clone().unwrap_or_else(|| init.clone());
        // Subscribe while holding the lock so no fragment falls between the
        // cached ones and the live ones.
        let rx = self.tx.subscribe();
        if quiet {
            if let Some(q) = st.quiet_init.as_ref() {
                let q = st.quiet_low_delay.clone().unwrap_or_else(|| q.clone());
                let gop = st.gop.iter().map(|f| fmp4::video_only(f).map(Arc::new).unwrap_or_else(|| f.clone())).collect();
                return Some((q, gop, rx));
            }
        }
        Some((init, st.gop.clone(), rx))
    }

    pub(crate) fn start(&self, init: Vec<u8>, info: LiveInfo, quiet_init: Option<Vec<u8>>) {
        let mut st = self.state.lock().unwrap();
        let had = st.init.is_some();
        st.frame_secs = 0.0;
        st.frames = 0;
        st.reordered = false;
        st.init = Some(Arc::new(init));
        st.low_delay = None;
        st.quiet_init = quiet_init.map(Arc::new);
        st.quiet_low_delay = None;
        st.info = Some(info);
        st.gop.clear();
        st.gop_bytes = 0;
        if had {
            let _ = self.tx.send(LiveMsg::Reset);
        }
    }

    pub(crate) fn publish(&self, frag: Vec<u8>, keyframe: bool) {
        let frag = Arc::new(frag);
        let mut st = self.state.lock().unwrap();
        if keyframe {
            st.gop.clear();
            st.gop_bytes = 0;
        }
        if st.gop_bytes + frag.len() <= MAX_GOP_BYTES {
            st.gop_bytes += frag.len();
            st.gop.push(frag.clone());
        }
        let _ = self.tx.send(LiveMsg::Fragment(frag));
    }

    /// New viewers get this init segment (a low-delay one), or, with None,
    /// the stream's own again. Viewers that got a low-delay one when it
    /// stops being right (B-frames after all) are told to reconnect.
    fn set_low_delay(&self, init: Option<Vec<u8>>, quiet: Option<Vec<u8>>) {
        let mut st = self.state.lock().unwrap();
        let had = st.low_delay.is_some();
        st.low_delay = init.map(Arc::new);
        st.quiet_low_delay = quiet.map(Arc::new);
        if had && st.low_delay.is_none() {
            let _ = self.tx.send(LiveMsg::Reset);
        }
    }

    /// The stream stopped: viewers are told to reconnect, new ones wait.
    pub fn stop(&self) {
        let mut st = self.state.lock().unwrap();
        if st.init.take().is_some() {
            let _ = self.tx.send(LiveMsg::Reset);
        }
        st.info = None;
        st.low_delay = None;
        st.quiet_init = None;
        st.quiet_low_delay = None;
        st.gop.clear();
        st.gop_bytes = 0;
    }

    pub fn viewers(&self) -> usize {
        self.tx.receiver_count()
    }
}

struct PendingVideo {
    /// Decode time.
    ticks: i64,
    /// Presentation minus decode time (B-frames).
    cts: i32,
    data: Vec<u8>,
    key: bool,
}

/// Turns frames from one stream session into live fragments on a hub.
pub struct LiveMuxer {
    hub: Arc<LiveHub>,
    audio: Option<usize>,
    audio_ts: u32,
    seq: u32,
    video_first: Option<i64>,
    /// Audio ticks that correspond to video decode time 0 (arrival-aligned).
    audio_zero: Option<i64>,
    started: std::time::Instant,
    video_first_at: f64,
    pending: Option<PendingVideo>,
    audio_buf: Vec<(i64, Vec<u8>)>,
    audio_next: Option<u64>,
    last_video_dur: u32,
    seen_key: bool,
    /// H.264: watches the frames for reordering; with the low-delay init
    /// segment to offer once they never reorder (and whether it is offered).
    reorder: Option<(super::sps::ReorderCheck, Vec<u8>, bool)>,
    /// The low-delay variant of the picture-only init segment.
    quiet_ld: Option<Vec<u8>>,
    /// Facts about the first audio frames, for one log line (see aac.rs).
    diag: Option<super::aac::Diag>,
    audio_entry: Option<Vec<u8>>,
    video_ts: u32,
    /// Subtracted from the camera's audio ticks: what the drift correction
    /// has taken out (dropped audio) or put in (repeated frames) so far.
    audio_shift: i64,
    /// Audio ticks still to drop (sound runs ahead of the picture) or to
    /// fill with repeated frames (sound runs behind: a gap in the times
    /// would not do, browsers play the frames back to back regardless).
    audio_skip: i64,
    audio_fill: i64,
    /// Ticks dropped since the last kept frame (the next one moves up by
    /// them), the previous frame's camera ticks, and the usual frame step.
    dropped: i64,
    audio_prev_ticks: Option<i64>,
    frame_len: Option<i64>,
    /// The previous frame was dropped: its length (the step to this one) is
    /// counted when this one comes.
    drop_open: bool,
    /// Recent steps between frames; the usual frame length is their (upper) median
    /// (one odd step, a camera nudging its times or a lost packet, must not
    /// make a repeat fill take thousands of copies, or far too few).
    recent_steps: std::collections::VecDeque<i64>,
    /// How early each track's frames arrive against their own times (the
    /// least lateness seen lately), and the correction made so far (for
    /// the log).
    video_late: Lateness,
    audio_late: Lateness,
    drift_fixed: f64,
    drift_logged: f64,
    name: String,
    /// Tests: the time now, in seconds since the start.
    test_now: Option<f64>,
}

/// Sound and picture are lined up once, by arrival, when a stream starts;
/// after that each follows its own camera clock. Some cameras' audio clock
/// runs a little fast or slow against their video clock (one ran 4 s an
/// hour fast): over a long session the sound drifted far from the picture,
/// and a viewer that started with sound waited for the picture to reach it.
/// So each track's lateness (arrival time minus its own time) is watched,
/// the least seen over the last LATENESS_SPAN..2*LATENESS_SPAN seconds
/// (delays in the network or in bursts only ever add to it); when the two
/// differ by more than AUDIO_MAX_GAP, the gap is closed: sound that runs
/// ahead has frames dropped, sound that runs behind has a frame repeated.
const AUDIO_MAX_GAP: f64 = 0.15;
const LATENESS_SPAN: f64 = 10.0;
const AUDIO_MAX_STEP: f64 = 1.0;
/// A gap past this is a camera whose times are not live (see correct_drift).
const AUDIO_SANE_SECS: f64 = 30.0;
/// A step between audio frames longer than this is a pause in the sound,
/// not a frame.
const AUDIO_PAUSE_SECS: f64 = 0.5;

/// The least lateness over the current and the previous span.
#[derive(Clone, Copy)]
pub(crate) struct Lateness {
    cur: f64,
    prev: f64,
    since: Option<f64>,
}

impl Lateness {
    pub(crate) const NONE: Lateness = Lateness { cur: f64::INFINITY, prev: f64::INFINITY, since: None };

    /// A frame of time `t` arrived at `now`. True when a span ended.
    pub(crate) fn note(&mut self, now: f64, t: f64) -> bool {
        let since = *self.since.get_or_insert(now);
        let rolled = now - since >= LATENESS_SPAN;
        if rolled {
            self.prev = self.cur;
            self.cur = f64::INFINITY;
            self.since = Some(now);
        }
        self.cur = self.cur.min(now - t);
        rolled
    }

    /// Known once a whole span has been seen.
    pub(crate) fn least(&self) -> Option<f64> {
        self.prev.is_finite().then(|| self.prev.min(self.cur))
    }
}

/// Frames (two keyframes among them) a live stream must show in decode
/// order before new viewers get a low-delay init segment.
const LOW_DELAY_MIN_FRAMES: u32 = 30;

impl LiveMuxer {
    /// `tracks[0]` must be the video track (as built by
    /// `recorder::build_tracks`). `rfc6381` are the codec strings in track
    /// order.
    pub fn start(hub: Arc<LiveHub>, tracks: &[TrackInfo], rfc6381: &[String]) -> Self {
        let init = fmp4::init_segment(tracks);
        let quiet_init = tracks.iter().any(|t| t.kind == TrackKind::Audio).then(|| fmp4::init_segment(&tracks[..1]));
        let quiet_ld = quiet_init.as_ref().and_then(|q| super::sps::low_delay_init(q));
        let reorder = super::sps::low_delay_init(&init).and_then(|ld| Some((super::sps::ReorderCheck::new(super::sps::init_avcc(&init)?), ld, false)));
        let audio = tracks.iter().position(|t| t.kind == TrackKind::Audio);
        let info = LiveInfo {
            mime: format!("video/mp4; codecs=\"{}\"", rfc6381.join(", ")),
            width: tracks[0].width,
            height: tracks[0].height,
            audio: audio.is_some(),
            video_mime: audio.map(|_| format!("video/mp4; codecs=\"{}\"", rfc6381[0])),
        };
        hub.start(init, info, quiet_init);
        LiveMuxer {
            hub,
            audio,
            audio_ts: audio.map(|a| tracks[a].timescale).unwrap_or(1),
            seq: 0,
            video_first: None,
            audio_zero: None,
            started: std::time::Instant::now(),
            video_first_at: 0.0,
            pending: None,
            audio_buf: Vec::new(),
            audio_next: None,
            last_video_dur: 3000,
            seen_key: false,
            reorder,
            quiet_ld,
            diag: None,
            audio_entry: audio.map(|a| tracks[a].sample_entry.clone()),
            video_ts: tracks[0].timescale.max(1),
            audio_shift: 0,
            audio_skip: 0,
            audio_fill: 0,
            dropped: 0,
            audio_prev_ticks: None,
            frame_len: None,
            drop_open: false,
            recent_steps: std::collections::VecDeque::new(),
            video_late: Lateness::NONE,
            audio_late: Lateness::NONE,
            test_now: None,
            drift_fixed: 0.0,
            drift_logged: 0.0,
            name: String::new(),
        }
    }

    /// Name the camera for the one-time log line about its audio.
    pub fn with_name(mut self, name: &str) -> Self {
        self.diag = self.audio_entry.take().and_then(|e| super::aac::Diag::new(name, &e));
        self.name = name.to_string();
        self
    }

    /// A video frame in decode order: `ticks` is its decode time, `cts` how
    /// much later it is shown (B-frames; 0 otherwise). Fragment decode times
    /// and durations follow the decode times; browsers show each frame at
    /// decode time + `cts`.
    pub fn push_video(&mut self, ticks: i64, cts: i32, data: &[u8], key: bool) {
        if !self.seen_key {
            if !key {
                return;
            }
            self.seen_key = true;
        }
        if self.video_first.is_none() {
            self.video_first = Some(ticks);
            self.video_first_at = self.now();
        }
        if let Some(first) = self.video_first {
            let now = self.now();
            self.video_late.note(now, (ticks - first) as f64 / self.video_ts as f64);
        }
        // The previous frame's duration is known now: emit it.
        if let Some(prev) = self.pending.take() {
            let d = (ticks - prev.ticks).clamp(1, u32::MAX as i64) as u32;
            self.last_video_dur = d;
            self.hub.note_frame(d as f64 / self.video_ts as f64);
            self.emit(prev, d);
        }
        if cts != 0 {
            self.hub.note_reorder();
        }
        self.watch_order(data, cts);
        self.pending = Some(PendingVideo { ticks, cts, data: data.to_vec(), key });
    }

    /// Offer new viewers the low-delay init segment once the frames have
    /// shown the stream never reorders; take it back (viewers reconnect) if
    /// one ever is (before it reaches any viewer).
    fn watch_order(&mut self, data: &[u8], cts: i32) {
        let Some((check, init, offered)) = self.reorder.as_mut() else { return };
        check.frame(data, cts);
        if check.reorders {
            if *offered {
                tracing::info!("live: frames are reordered after all: new viewers get the camera's own stream header");
                self.hub.set_low_delay(None, None);
            }
            self.reorder = None;
        } else if !*offered && check.keyframes >= 2 && check.never_reorders(LOW_DELAY_MIN_FRAMES) {
            *offered = true;
            let ld = std::mem::take(init);
            self.hub.set_low_delay(Some(ld), self.quiet_ld.take());
        }
    }

    pub fn push_audio(&mut self, ticks: i64, data: &[u8]) {
        if let Some(d) = self.diag.as_mut() {
            d.frame(data);
        }
        if self.audio.is_none() || self.video_first.is_none() {
            return;
        }
        if self.audio_zero.is_none() {
            // Line audio up with video by arrival time.
            let since_video = self.now() - self.video_first_at;
            self.audio_zero = Some(ticks - (since_video * self.audio_ts as f64) as i64);
        }
        // The step from the previous frame: one frame's length, unless the
        // camera paused its audio (more than AUDIO_PAUSE_SECS).
        let step = self.audio_prev_ticks.map(|p| ticks - p);
        self.audio_prev_ticks = Some(ticks);
        let pause = step.is_some_and(|s| s <= 0 || s as f64 > AUDIO_PAUSE_SECS * self.audio_ts as f64);
        if let (Some(s), false) = (step, pause) {
            if self.recent_steps.len() == 8 {
                self.recent_steps.pop_front();
            }
            self.recent_steps.push_back(s);
            let mut v: Vec<i64> = self.recent_steps.iter().copied().collect();
            v.sort_unstable();
            self.frame_len = v.get(v.len() / 2).copied();
        }
        if std::mem::take(&mut self.drop_open) {
            // The dropped frame's own length: the step to this frame (after a
            // pause, the usual length: the pause keeps its place).
            let len = match step {
                Some(s) if !pause => s,
                _ => self.frame_len.unwrap_or(0),
            };
            self.dropped += len;
            self.audio_skip -= len;
        }
        if self.audio_skip > 0 {
            if self.frame_len.is_some() && !pause {
                // Dropped: the frames after it move up by its length.
                self.drop_open = true;
                return;
            }
            // A pause, or no frame length yet: the drop ends here.
            self.audio_skip = 0;
        }
        self.audio_shift += std::mem::take(&mut self.dropped);
        if self.audio_fill > 0 {
            if let Some(len) = self.frame_len.filter(|&l| l > 0) {
                // Repeat this frame to fill: whole frames, back to back.
                let most = (AUDIO_MAX_STEP * self.audio_ts as f64 / len as f64).ceil() as i64 + 1;
                let n = ((self.audio_fill + len / 2) / len).clamp(1, most.clamp(1, 100));
                let at = ticks - self.audio_shift;
                for k in 0..n {
                    self.audio_buf.push((at + k * len, data.to_vec()));
                }
                self.audio_shift -= n * len;
            }
            self.audio_fill = 0;
        }
        let at = ticks - self.audio_shift;
        self.correct_drift(at);
        self.audio_buf.push((at, data.to_vec()));
    }

    fn now(&self) -> f64 {
        self.test_now.unwrap_or_else(|| self.started.elapsed().as_secs_f64())
    }

    /// Watch how far the sound is from the picture; close a gap past
    /// AUDIO_MAX_GAP (see there).
    fn correct_drift(&mut self, ticks: i64) {
        let Some(zero) = self.audio_zero else { return };
        let now = self.now();
        if !self.audio_late.note(now, (ticks - zero) as f64 / self.audio_ts as f64) {
            return;
        }
        let (Some(v), Some(a)) = (self.video_late.least(), self.audio_late.least()) else { return };
        // Sound ahead of the picture arrives less late against its own time.
        let gap = v - a;
        // (Past AUDIO_SANE_SECS the camera's times are not to be followed:
        // stood still or ran wild. Left alone, as recordings do.)
        if gap.abs() <= AUDIO_MAX_GAP || gap.abs() > AUDIO_SANE_SECS {
            return;
        }
        // A camera whose clock jumped would lose (or wait for) all of it at
        // once: a second at most per check.
        let gap = gap.clamp(-AUDIO_MAX_STEP, AUDIO_MAX_STEP);
        let n = (gap * self.audio_ts as f64).round() as i64;
        if n > 0 {
            self.audio_skip = n;
        } else {
            self.audio_fill = -n;
        }
        // Measured again from here, with the moved times.
        self.audio_late = Lateness::NONE;
        self.drift_fixed += gap;
        // (Once past a second, then every 10 s: a camera that drifts a few
        // seconds an hour logs a few lines a day.)
        let every = if self.drift_logged == 0.0 { 1.0 } else { 10.0 };
        if (self.drift_fixed - self.drift_logged).abs() >= every {
            self.drift_logged = self.drift_fixed;
            tracing::info!(
                "camera '{}': live view: the camera's sound runs {} its picture; moved it {:+.1} s so far to keep them together",
                self.name,
                if self.drift_fixed > 0.0 { "ahead of" } else { "behind" },
                -self.drift_fixed
            );
        }
    }

    fn emit(&mut self, v: PendingVideo, duration: u32) {
        let base = (v.ticks - self.video_first.unwrap()).max(0) as u64;
        let mut runs = vec![TrackRun {
            track_index: 0,
            base_decode_time: base,
            samples: vec![Sample { duration, data: v.data, is_sync: v.key, composition_offset: v.cts }],
        }];
        if let (Some(ai), Some(zero)) = (self.audio, self.audio_zero) {
            // Audio frames whose duration is known (all but the newest).
            if self.audio_buf.len() > 1 {
                let last = self.audio_buf.pop().unwrap();
                let frames = std::mem::take(&mut self.audio_buf);
                self.audio_buf.push(last);
                let first_dt = (frames[0].0 - zero).max(0) as u64;
                let start = self.audio_next.map(|n| n.max(first_dt)).unwrap_or(first_dt);
                let mut samples = Vec::with_capacity(frames.len());
                let next_ticks: Vec<i64> = frames.iter().skip(1).map(|f| f.0).chain(std::iter::once(self.audio_buf[0].0)).collect();
                let mut total = 0u64;
                for ((t, data), n) in frames.into_iter().zip(next_ticks) {
                    let d = (n - t).clamp(1, u32::MAX as i64) as u32;
                    total += d as u64;
                    samples.push(Sample::new(d, data, true));
                }
                self.audio_next = Some(start + total);
                runs.push(TrackRun { track_index: ai, base_decode_time: start, samples });
            }
        }
        self.seq += 1;
        self.hub.publish(fmp4::fragment(self.seq, &runs), v.key);
    }

    /// Stream ended cleanly: flush the held frame (dropping the muxer then
    /// tells viewers to reconnect).
    pub fn finish(mut self) {
        if let Some(prev) = self.pending.take() {
            let d = self.last_video_dur;
            self.emit(prev, d);
        }
    }
}

impl Drop for LiveMuxer {
    /// However the session ends (error, stall, parameter change), viewers
    /// must not be left waiting on a stream that will never continue.
    fn drop(&mut self) {
        self.hub.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_track() -> TrackInfo {
        let mut entry = vec![0, 0, 0, 16];
        entry.extend_from_slice(b"avc1");
        entry.extend_from_slice(&[0; 8]);
        TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: entry, width: 640, height: 480 }
    }

    #[test]
    fn late_viewer_gets_init_and_gop_since_last_keyframe() {
        let hub = Arc::new(LiveHub::new());
        assert!(hub.subscribe().is_none(), "nothing before the stream starts");
        let mut m = LiveMuxer::start(hub.clone(), &[video_track()], &["avc1.640033".into()]);
        assert_eq!(hub.info().unwrap().mime, "video/mp4; codecs=\"avc1.640033\"");
        // Leading non-keyframe is dropped; keyframes at 0 and 10.
        m.push_video(0, 0, &[9], false);
        for i in 0..14i64 {
            m.push_video(1000 + i * 9000, 0, &[i as u8; 10], i % 10 == 0);
        }
        // Fragments 1..=13 emitted (frame 13 is held for its duration);
        // the cache holds frames 10, 11, 12.
        let (init, gop, mut rx) = hub.subscribe().unwrap();
        assert_eq!(&init[4..8], b"ftyp");
        assert_eq!(gop.len(), 3);
        // 9000 ticks at 90 kHz: 10 frames a second.
        assert!((hub.fps().unwrap() - 10.0).abs() < 0.01);
        assert_eq!(hub.init().unwrap(), init);
        // A new frame reaches the live receiver.
        m.push_video(1000 + 14 * 9000, 0, &[1; 10], false);
        match rx.try_recv().unwrap() {
            LiveMsg::Fragment(f) => assert_eq!(&f[4..8], b"moof"),
            LiveMsg::Reset => panic!("unexpected reset"),
        }
        m.finish();
        // Final flush, then the reset that tells viewers to reconnect.
        assert!(matches!(rx.try_recv().unwrap(), LiveMsg::Fragment(_)));
        assert!(matches!(rx.try_recv().unwrap(), LiveMsg::Reset));
        assert!(hub.subscribe().is_none());
    }

    fn sps_of(init: &[u8]) -> crate::ingest::sps::Sps {
        let a = crate::ingest::sps::init_avcc(init).unwrap();
        crate::ingest::sps::parse_sps(&a[8..8 + u16::from_be_bytes([a[6], a[7]]) as usize]).unwrap()
    }

    #[test]
    fn viewers_get_a_low_delay_header_once_frames_never_reorder() {
        use crate::ingest::sps::tests::{h264_sample_entry, slice};
        let track = TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: h264_sample_entry(), width: 640, height: 360 };
        // P frames only, 10-frame groups.
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[track.clone()], &["avc1.640033".into()]);
        let frame = |i: u32, st: u32| slice(i % 10 == 0, true, if i % 10 == 0 { 7 } else { st }, i % 10, (i % 10) * 2);
        for i in 0..15 {
            m.push_video(i as i64 * 9000, 0, &frame(i, 5), i % 10 == 0);
        }
        // Not yet: too few frames seen.
        assert_eq!(sps_of(&hub.subscribe().unwrap().0).max_num_reorder, None);
        let (_, _, mut rx) = hub.subscribe().unwrap();
        for i in 15..40 {
            m.push_video(i as i64 * 9000, 0, &frame(i, 5), i % 10 == 0);
        }
        let (init, _, _) = hub.subscribe().unwrap();
        assert_eq!(sps_of(&init).max_num_reorder, Some(0));
        assert!(collect(&mut rx).len() > 20);
        // A B frame after all: the header goes back, and viewers reconnect
        // before the B frame reaches them.
        m.push_video(40 * 9000, 0, &frame(41, 6), false);
        assert!(matches!(rx.try_recv().unwrap(), LiveMsg::Fragment(_)), "the frame before");
        assert!(matches!(rx.try_recv().unwrap(), LiveMsg::Reset));
        assert_eq!(sps_of(&hub.subscribe().unwrap().0).max_num_reorder, None);
        drop(m);

        // B-frames from the start, or composition offsets: never.
        for (st, cto) in [(6, 0), (5, 3000)] {
            let hub = Arc::new(LiveHub::new());
            let mut m = LiveMuxer::start(hub.clone(), &[track.clone()], &["avc1.640033".into()]);
            for i in 0..60 {
                m.push_video(i as i64 * 9000, if i % 10 == 0 { 0 } else { cto }, &frame(i, if i % 3 == 2 { st } else { 5 }), i % 10 == 0);
            }
            assert_eq!(sps_of(&hub.subscribe().unwrap().0).max_num_reorder, None);
        }
    }

    /// (decode time, duration, composition offset) of each video fragment.
    fn video_timing(frags: &[Arc<Vec<u8>>]) -> Vec<(u64, u32, i32)> {
        frags
            .iter()
            .map(|f| {
                let p = f.windows(4).position(|w| w == b"tfdt").unwrap();
                let tfdt = u64::from_be_bytes(f[p + 8..p + 16].try_into().unwrap());
                let (_, _, s) = crate::ingest::fmp4::tests::parse_trun(f);
                assert_eq!(s.len(), 1);
                (tfdt, s[0].0, s[0].3)
            })
            .collect()
    }

    fn collect(rx: &mut broadcast::Receiver<LiveMsg>) -> Vec<Arc<Vec<u8>>> {
        let mut out = Vec::new();
        while let Ok(LiveMsg::Fragment(f)) = rx.try_recv() {
            out.push(f);
        }
        out
    }

    /// I P B B P B B ... in decode order: presentation times in frame units.
    fn ipbb(groups: i64) -> Vec<i64> {
        let mut pts = vec![0];
        for g in 0..groups {
            pts.extend([g * 3 + 3, g * 3 + 1, g * 3 + 2]);
        }
        pts
    }

    #[test]
    fn b_frames_from_flv_play_in_decode_time_with_offsets() {
        // As an RTMP/FLV sender gives them: decode time one frame behind,
        // composition offset = presentation - decode.
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track()], &["avc1.640033".into()]);
        let (_, _, mut rx) = hub.subscribe().unwrap();
        let pts = ipbb(10);
        for (n, p) in pts.iter().enumerate() {
            let dts = 5000 + n as i64 * 3000;
            let pts = 5000 + (p + 1) * 3000;
            m.push_video(dts, (pts - dts) as i32, &[n as u8; 8], n % 15 == 0);
        }
        m.finish();
        let t = video_timing(&collect(&mut rx));
        assert_eq!(t.len(), pts.len());
        for (n, (dts, dur, cto)) in t.iter().enumerate() {
            assert_eq!(*dts, n as u64 * 3000, "decode times start at 0 and step a frame");
            assert_eq!(*dur, 3000, "durations from decode-time deltas");
            assert_eq!(*dts as i64 + *cto as i64, (pts[n] + 1) * 3000);
        }
    }

    #[test]
    fn b_frames_from_rtsp_presentation_times_get_monotonic_decode_times() {
        // RTSP gives presentation times only; decode times are derived.
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track()], &["avc1.640033".into()]);
        let (_, _, mut rx) = hub.subscribe().unwrap();
        let mut d = crate::ingest::dts::DtsFromPts::new(90_000);
        let pts = ipbb(30);
        let base = 1_000_000i64;
        for (n, p) in pts.iter().enumerate() {
            let (dts, cts) = d.push(base + p * 3000, n % 30 == 0);
            m.push_video(dts, cts, &[1; 8], n % 30 == 0);
        }
        m.finish();
        let t = video_timing(&collect(&mut rx));
        assert_eq!(t.len(), pts.len());
        // Reordering is recognized at the third group of B-frames (frame 8);
        // the frames before pass through as sent.
        for w in t[7..].windows(2) {
            assert!(w[1].0 > w[0].0, "decode times increase: {t:?}");
        }
        for (n, (dts, _, cto)) in t.iter().enumerate() {
            assert_eq!(*dts as i64 + *cto as i64, pts[n] * 3000, "shown at its presentation time");
        }
        // Once settled: one frame per 3000 ticks, as sent.
        assert!(t[16..t.len() - 1].iter().all(|x| x.1 == 3000), "{t:?}");
    }

    #[test]
    fn frames_without_b_frames_have_no_offsets() {
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track()], &["avc1.640033".into()]);
        let (_, _, mut rx) = hub.subscribe().unwrap();
        for i in 0..5i64 {
            m.push_video(i * 3000, 0, &[1; 8], i == 0);
        }
        m.finish();
        let frags = collect(&mut rx);
        for f in &frags {
            assert_eq!(crate::ingest::fmp4::tests::parse_trun(f).0, 0, "version 0 trun as before");
        }
        assert_eq!(video_timing(&frags).iter().map(|x| x.0).collect::<Vec<_>>(), vec![0, 3000, 6000, 9000, 12000]);
    }

    fn audio_track(ch: u8) -> TrackInfo {
        let asc = [0x11, 0x80 | (ch << 3)];
        TrackInfo { kind: TrackKind::Audio, timescale: 48000, sample_entry: crate::ingest::flv::aac_sample_entry(&asc).unwrap(), width: 0, height: 0 }
    }

    fn trafs(f: &[u8]) -> usize {
        f.windows(4).filter(|w| *w == b"traf").count()
    }

    #[test]
    fn a_stream_with_sound_has_a_picture_only_form_for_muted_viewers() {
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track(), audio_track(1)], &["avc1.640033".into(), "mp4a.40.2".into()]);
        let info = hub.info().unwrap();
        assert!(info.audio);
        assert_eq!(info.mime, "video/mp4; codecs=\"avc1.640033, mp4a.40.2\"");
        assert_eq!(info.video_mime.as_deref(), Some("video/mp4; codecs=\"avc1.640033\""));
        for i in 0..30i64 {
            m.push_video(i * 3600, 0, &[1; 8], i % 10 == 0);
            m.push_audio(i * 1920, &[0x21, 9, 9, 9]);
        }
        let tracks = |init: &[u8]| init.windows(4).filter(|w| *w == b"trak").count();
        let (full_init, full_gop, _rx1) = hub.subscribe_for(false).unwrap();
        let (quiet_init, quiet_gop, _rx2) = hub.subscribe_for(true).unwrap();
        assert_eq!((tracks(&full_init), tracks(&quiet_init)), (2, 1));
        assert_eq!(full_gop.len(), quiet_gop.len());
        assert!(full_gop.iter().any(|f| trafs(f) == 2), "audio in the full form");
        assert!(quiet_gop.iter().all(|f| trafs(f) == 1), "none in the picture-only form");
        // A live fragment is made quiet by the viewer's own task.
        let (_, _, mut rx) = hub.subscribe_for(true).unwrap();
        m.push_video(30 * 3600, 0, &[1; 8], false);
        m.push_audio(30 * 1920, &[0x21, 9, 9, 9]);
        m.push_video(31 * 3600, 0, &[1; 8], false);
        m.push_audio(31 * 1920, &[0x21, 9, 9, 9]);
        let LiveMsg::Fragment(f) = rx.try_recv().unwrap() else { panic!("fragment") };
        assert_eq!(trafs(&f), 2);
        assert_eq!(trafs(&fmp4::video_only(&f).unwrap()), 1);
    }

    #[test]
    fn a_stream_without_sound_serves_the_same_to_everyone() {
        let hub = Arc::new(LiveHub::new());
        let _m = LiveMuxer::start(hub.clone(), &[video_track()], &["avc1.640033".into()]);
        assert!(hub.info().unwrap().video_mime.is_none());
        let a = hub.subscribe_for(true).unwrap();
        assert_eq!(&a.0[4..8], b"ftyp");
        assert_eq!(a.0, hub.subscribe_for(false).unwrap().0);
    }

    #[test]
    fn what_the_muxer_publishes_does_not_depend_on_who_watches() {
        let run = |watch: bool| {
            let hub = Arc::new(LiveHub::new());
            let mut m = LiveMuxer::start(hub.clone(), &[video_track(), audio_track(2)], &["avc1.640033".into(), "mp4a.40.2".into()]);
            let sub = watch.then(|| hub.subscribe_for(true).unwrap());
            for i in 0..12i64 {
                // (Simulated time: on a busy machine the real clock would line
                // the sound up a tick apart in the two runs.)
                m.test_now = Some(i as f64 * 0.04);
                m.push_video(i * 3600, 0, &[1; 8], i == 0);
                m.push_audio(i * 1920, &[0x21, 9]);
            }
            std::mem::drop(sub);
            hub.subscribe_for(false).unwrap().1.iter().map(|f| f.to_vec()).collect::<Vec<_>>()
        };
        assert_eq!(run(false), run(true));
    }

    /// (video, audio) decode times in seconds of each fragment with both.
    fn both_times(frags: &[Arc<Vec<u8>>]) -> Vec<(f64, f64)> {
        frags
            .iter()
            .filter_map(|f| {
                let at: Vec<usize> = f.windows(4).enumerate().filter(|(_, w)| *w == b"tfdt").map(|(i, _)| i).collect();
                let t = |p: usize| u64::from_be_bytes(f[p + 8..p + 16].try_into().unwrap());
                (at.len() == 2).then(|| (t(at[0]) as f64 / 90_000.0, t(at[1]) as f64 / 48_000.0))
            })
            .collect()
    }

    /// `secs` of 25 fps video and 40 ms audio frames arriving together; the
    /// camera's audio clock runs `rate` times its video clock.
    fn drifting(secs: i64, rate: f64) -> (Vec<(f64, f64)>, LiveMuxer) {
        drifting_in_bursts(secs, rate, 1)
    }

    /// The same with the audio sent `burst` frames at a time.
    fn drifting_in_bursts(secs: i64, rate: f64, burst: i64) -> (Vec<(f64, f64)>, LiveMuxer) {
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track(), audio_track(1)], &["avc1.640033".into(), "mp4a.40.2".into()]);
        let (_, _, mut rx) = hub.subscribe_for(false).unwrap();
        let mut frags = Vec::new();
        for i in 0..secs * 25 {
            // Arrivals a little late now and then (up to 0.3 s).
            m.test_now = Some(i as f64 * 0.04 + if i % 7 == 3 { 0.3 } else { 0.0 });
            m.push_video(i * 3600, 0, &[1; 8], i % 50 == 0);
            m.test_now = Some(i as f64 * 0.04 + if i % 11 == 5 { 0.3 } else { 0.0 });
            if (i + 1) % burst == 0 {
                for k in i + 1 - burst..=i {
                    m.push_audio((k as f64 * 1920.0 * rate).round() as i64, &[0x21, 9, 9, 9]);
                }
            }
            if i % 10 == 0 {
                frags.extend(collect(&mut rx));
            }
        }
        frags.extend(collect(&mut rx));
        (both_times(&frags), m)
    }

    fn check_sound_follows(times: &[(f64, f64)]) {
        assert!(times.len() > 1000);
        for w in times.windows(2) {
            assert!(w[1].1 > w[0].1, "sound times only go forward: {w:?}");
        }
        let worst = times.iter().map(|(v, a)| (a - v).abs()).fold(0.0, f64::max);
        // (Between checks a fast drift adds up to a span or two of it.)
        assert!(worst < AUDIO_MAX_GAP + 0.2, "sound stays with the picture (worst {worst:.3} s)");
    }

    /// Each fragment's audio (decode time, total duration), in ticks.
    fn audio_runs(frags: &[Arc<Vec<u8>>]) -> Vec<(u64, u64)> {
        frags
            .iter()
            .filter_map(|f| {
                let at: Vec<usize> = f.windows(4).enumerate().filter(|(_, w)| *w == b"traf").map(|(i, _)| i).collect();
                let a = *at.get(1)?;
                let p = a + f[a..].windows(4).position(|w| w == b"tfdt").unwrap();
                let tfdt = u64::from_be_bytes(f[p + 8..p + 16].try_into().unwrap());
                let (_, _, s) = crate::ingest::fmp4::tests::parse_trun(&f[a..]);
                Some((tfdt, s.iter().map(|x| x.0 as u64).sum()))
            })
            .collect()
    }

    /// The sound's times have no gaps and no overlaps: browsers play audio
    /// frames back to back whatever their times say.
    fn check_gapless(frags: &[Arc<Vec<u8>>]) {
        let runs = audio_runs(frags);
        assert!(runs.len() > 100);
        for w in runs.windows(2) {
            assert_eq!(w[0].0 + w[0].1, w[1].0, "audio continues where the last fragment's ended: {w:?}");
        }
    }

    fn drifting_frags(secs: i64, rate: f64) -> Vec<Arc<Vec<u8>>> {
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track(), audio_track(1)], &["avc1.640033".into(), "mp4a.40.2".into()]);
        let (_, _, mut rx) = hub.subscribe_for(false).unwrap();
        let mut frags = Vec::new();
        for i in 0..secs * 25 {
            m.test_now = Some(i as f64 * 0.04);
            m.push_video(i * 3600, 0, &[1; 8], i % 50 == 0);
            m.push_audio((i as f64 * 1920.0 * rate).round() as i64, &[0x21, 9, 9, 9]);
            if i % 10 == 0 {
                frags.extend(collect(&mut rx));
            }
        }
        frags.extend(collect(&mut rx));
        frags
    }

    #[test]
    fn corrections_leave_no_gaps_in_the_sound() {
        // Fast sound has frames dropped, slow sound frames repeated: either
        // way the times stay back to back.
        check_gapless(&drifting_frags(600, 1.005));
        check_gapless(&drifting_frags(600, 0.995));
    }

    #[test]
    fn a_pause_in_the_sound_while_dropping_keeps_its_place() {
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track(), audio_track(1)], &["avc1.640033".into(), "mp4a.40.2".into()]);
        let push = |m: &mut LiveMuxer, i: i64, audio: Option<i64>| {
            m.test_now = Some(i as f64 * 0.04);
            m.push_video(i * 3600, 0, &[1; 8], i % 50 == 0);
            if let Some(a) = audio {
                m.push_audio(a, &[0x21, 9, 9, 9]);
            }
        };
        for i in 0..100 {
            push(&mut m, i, Some(i * 1920));
        }
        // A drop of 0.2 s is under way when the camera's sound pauses for
        // 60 s (its clock running on).
        m.audio_skip = 9600;
        push(&mut m, 100, Some(100 * 1920));
        for i in 101..1600 {
            push(&mut m, i, None);
        }
        for i in 1600..1700 {
            push(&mut m, i, Some(i * 1920));
        }
        // Only the one frame dropped before the pause moved the sound.
        assert_eq!(m.audio_shift, 1920, "the pause was not closed up");
    }

    #[test]
    fn one_short_step_does_not_make_a_long_fill() {
        // A camera nudging its audio times leaves a 4-tick step into the frame
        // a 1 s fill falls on: the copies are counted in usual frames, not in
        // 4 ticks; a lost packet (a double step) does not halve them either.
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track(), audio_track(1)], &["avc1.640033".into(), "mp4a.40.2".into()]);
        let mut a = 0;
        for i in 0..50i64 {
            m.test_now = Some(i as f64 * 0.04);
            m.push_video(i * 3600, 0, &[1; 8], i % 50 == 0);
            m.push_audio(a, &[0x21, 9, 9, 9]);
            a += if i == 49 { 4 } else { 1920 };
        }
        m.audio_fill = 48_000;
        let before = m.audio_buf.len();
        m.push_audio(a, &[0x21, 9, 9, 9]);
        let copies = m.audio_buf.len() - before;
        assert!((25..=27).contains(&copies), "{copies} frames for 1 s of 40 ms frames");
        m.push_audio(a + 2 * 1920, &[0x21, 9, 9, 9]);
        m.audio_fill = 48_000;
        let before = m.audio_buf.len();
        m.push_audio(a + 3 * 1920, &[0x21, 9, 9, 9]);
        let copies = m.audio_buf.len() - before;
        assert!((25..=27).contains(&copies), "after a lost packet: {copies}");
    }

    #[test]
    fn a_dropped_frame_is_counted_by_its_own_length() {
        // Frames of 1024 and 2048 ticks in turn (sizes vary on some
        // cameras): after a dropped one, the next kept frame takes its place
        // exactly, whatever the usual length says.
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track(), audio_track(1)], &["avc1.640033".into(), "mp4a.40.2".into()]);
        m.test_now = Some(0.0);
        m.push_video(0, 0, &[1; 8], true);
        let mut t = 0;
        let mut times = Vec::new();
        for k in 0..20 {
            times.push(t);
            t += if k % 2 == 0 { 1024 } else { 2048 };
        }
        for &x in &times[..10] {
            m.push_audio(x, &[0x21, 9]);
        }
        // Frame 10 (1024 long, shorter than the usual 2048) is dropped; frame
        // 11 must go where it began.
        m.audio_skip = 1;
        m.push_audio(times[10], &[0x21, 9]);
        m.push_audio(times[11], &[0x21, 9]);
        let last = m.audio_buf.last().unwrap().0;
        assert_eq!(last, times[10], "placed at the dropped frame's start");
        assert_eq!(m.audio_shift, 1024);
    }

    #[test]
    fn timestamps_that_run_wild_are_left_alone() {
        // Video standing still against its sound: no correction chases it.
        let hub = Arc::new(LiveHub::new());
        let mut m = LiveMuxer::start(hub.clone(), &[video_track(), audio_track(1)], &["avc1.640033".into(), "mp4a.40.2".into()]);
        for i in 0..25 * 300 {
            m.test_now = Some(i as f64 * 0.04);
            m.push_video(i * 3600 / 100, 0, &[1; 8], i % 50 == 0);
            m.push_audio(i * 1920, &[0x21, 9, 9, 9]);
        }
        assert!(m.drift_fixed.abs() <= AUDIO_MAX_STEP * 3.0 + 0.01, "moved {}", m.drift_fixed);
    }

    #[test]
    fn steady_clocks_are_left_alone() {
        let (times, m) = drifting(1200, 1.0);
        check_sound_follows(&times);
        assert_eq!((m.audio_shift, m.drift_fixed), (0, 0.0));
    }

    #[test]
    fn sound_sent_in_bursts_is_lined_up_once() {
        // Half a second of audio at a time: each frame arrives up to 0.5 s
        // after the picture it belongs with. The first one (which lines
        // sound up with the picture) came 0.44 s late: moved once, then left.
        let (times, m) = drifting_in_bursts(600, 1.0, 12);
        // (A fragment carries the audio that came with its picture: up to a
        // burst older than it, never ahead.)
        let late = &times[times.len() / 10..];
        assert!(late.iter().all(|(v, a)| a - v <= AUDIO_MAX_GAP && a - v > -0.6), "{:?}", late.iter().map(|(v, a)| a - v).fold((9.0f64, -9.0f64), |(l, h), g| (l.min(g), h.max(g))));
        assert_eq!(m.audio_shift, 21_120);
        assert!((m.drift_fixed - 0.44).abs() < 0.01, "{}", m.drift_fixed);
    }

    #[test]
    fn sound_running_fast_is_kept_with_the_picture() {
        // 0.5 % fast: 6 s in 20 minutes without the correction.
        let (times, m) = drifting(1200, 1.005);
        check_sound_follows(&times);
        assert!(m.audio_shift > 5 * 48_000, "frames were dropped: {}", m.audio_shift);
    }

    #[test]
    fn sound_running_slow_is_kept_with_the_picture() {
        let (times, m) = drifting(1200, 0.995);
        check_sound_follows(&times);
        assert!(m.audio_shift < -5 * 48_000, "gaps were put in: {}", m.audio_shift);
    }
}
