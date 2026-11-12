//! Fan-out point for one camera stream shared over RTSP. The camera's stream
//! session publishes the frames it already receives (nothing new is asked of
//! the camera); every RTSP client gets its own receiver. A client that cannot
//! keep up loses frames (it is told, and waits for the next keyframe); it
//! never slows the session, which only does a non-blocking send.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use super::rtp;

#[derive(Clone, Debug, PartialEq)]
pub struct VideoInfo {
    pub h265: bool,
    pub clock_rate: u32,
    pub width: u32,
    pub height: u32,
    /// Bytes in the length prefix of each NAL unit in the frames.
    pub len_size: usize,
    /// VPS / SPS / PPS NAL units, in the order of the stream's configuration.
    pub param_sets: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioInfo {
    /// AAC AudioSpecificConfig.
    pub config: Vec<u8>,
    pub rate: u32,
    pub channels: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    pub video: VideoInfo,
    pub audio: Option<AudioInfo>,
    /// Changes whenever the stream restarts (clients of an older one leave).
    pub generation: u64,
}

pub struct Unit {
    pub video: bool,
    /// Presentation time in the track's clock.
    pub pts: i64,
    pub key: bool,
    /// When it arrived (Unix seconds): ties the two tracks' clocks together
    /// for RTCP sender reports.
    pub wall: f64,
    pub data: Arc<[u8]>,
}

#[derive(Clone)]
pub enum Msg {
    Unit(Arc<Unit>),
    /// The stream stopped or restarted: clients end their session.
    Reset,
}

struct State {
    info: Option<Arc<StreamInfo>>,
    /// Units since the latest keyframe (so a new client starts at once).
    gop: Vec<Arc<Unit>>,
    gop_bytes: usize,
    /// The cached units start with a keyframe and none was dropped.
    gop_ok: bool,
}

/// What a new client gets: the stream's description, the units since the
/// last keyframe (possibly none), and the live receiver.
pub struct Subscription {
    pub info: Arc<StreamInfo>,
    pub backlog: Vec<Arc<Unit>>,
    pub rx: broadcast::Receiver<Msg>,
}

pub struct RestreamHub {
    wanted: AtomicBool,
    tx: broadcast::Sender<Msg>,
    state: Mutex<State>,
    clients: AtomicUsize,
}

/// Cached bytes / units a keyframe interval may take. A camera with a very
/// long interval then just makes new clients wait for the next keyframe.
const MAX_GOP_BYTES: usize = 8 * 1024 * 1024;
const MAX_GOP_UNITS: usize = 900;
/// Units a client may fall behind before it loses some (about 8 s of 30 fps video).
const CHANNEL_UNITS: usize = 256;

static GENERATION: AtomicU64 = AtomicU64::new(1);

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl RestreamHub {
    pub fn new(wanted: bool) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_UNITS);
        RestreamHub {
            wanted: AtomicBool::new(wanted),
            tx,
            state: Mutex::new(State { info: None, gop: Vec::new(), gop_bytes: 0, gop_ok: false }),
            clients: AtomicUsize::new(0),
        }
    }

    /// Whether this stream is shared right now (the camera's share switch
    /// and the global one). The session publishes nothing otherwise.
    pub fn wanted(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    pub fn set_wanted(&self, on: bool) {
        if self.wanted.swap(on, Ordering::Relaxed) && !on {
            self.stop();
        }
    }

    pub fn info(&self) -> Option<Arc<StreamInfo>> {
        self.state.lock().unwrap().info.clone()
    }

    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::Relaxed)
    }

    /// Count a client until the guard is dropped.
    pub fn client_guard(self: &Arc<Self>) -> ClientGuard {
        self.clients.fetch_add(1, Ordering::Relaxed);
        ClientGuard(self.clone())
    }

    /// The stream (re)started. Sessions of the previous one end.
    pub fn start(&self, video: VideoInfo, audio: Option<AudioInfo>) {
        let mut st = self.state.lock().unwrap();
        let had = st.info.is_some();
        st.info = Some(Arc::new(StreamInfo { video, audio, generation: GENERATION.fetch_add(1, Ordering::Relaxed) }));
        st.gop.clear();
        st.gop_bytes = 0;
        st.gop_ok = false;
        if had {
            let _ = self.tx.send(Msg::Reset);
        }
    }

    /// The stream stopped: clients end, new ones are told it is not there.
    pub fn stop(&self) {
        let mut st = self.state.lock().unwrap();
        if st.info.take().is_some() {
            let _ = self.tx.send(Msg::Reset);
        }
        st.gop.clear();
        st.gop_bytes = 0;
        st.gop_ok = false;
    }

    pub fn subscribe(&self) -> Option<Subscription> {
        let st = self.state.lock().unwrap();
        let info = st.info.clone()?;
        // Under the lock, so no unit falls between the cached and live ones.
        let rx = self.tx.subscribe();
        let backlog = if st.gop_ok { st.gop.clone() } else { Vec::new() };
        Some(Subscription { info, backlog, rx })
    }

    /// A video frame (length-prefixed NAL units). `pts` in the video clock.
    pub fn publish_video(&self, pts: i64, key: bool, data: &[u8]) {
        let mut st = self.state.lock().unwrap();
        if st.info.is_none() {
            return;
        }
        let unit = Arc::new(Unit { video: true, pts, key, wall: now_secs(), data: Arc::from(data) });
        if key {
            st.gop.clear();
            st.gop_bytes = 0;
            st.gop_ok = true;
        }
        Self::cache(&mut st, &unit);
        let _ = self.tx.send(Msg::Unit(unit));
    }

    /// One AAC frame. `pts` in the audio clock. Ignored without an audio track.
    pub fn publish_audio(&self, pts: i64, data: &[u8]) {
        let mut st = self.state.lock().unwrap();
        if st.info.as_ref().is_none_or(|i| i.audio.is_none()) {
            return;
        }
        let unit = Arc::new(Unit { video: false, pts, key: false, wall: now_secs(), data: Arc::from(data) });
        Self::cache(&mut st, &unit);
        let _ = self.tx.send(Msg::Unit(unit));
    }

    fn cache(st: &mut State, unit: &Arc<Unit>) {
        if !st.gop_ok {
            return;
        }
        if st.gop_bytes + unit.data.len() > MAX_GOP_BYTES || st.gop.len() >= MAX_GOP_UNITS {
            // Too long a keyframe interval to keep: new clients wait for the next one.
            st.gop.clear();
            st.gop_bytes = 0;
            st.gop_ok = false;
            return;
        }
        st.gop_bytes += unit.data.len();
        st.gop.push(unit.clone());
    }
}

pub struct ClientGuard(Arc<RestreamHub>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Ends the shared stream when a camera session ends (however it ends).
pub struct StopOnDrop(pub Arc<RestreamHub>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// What to publish for a stream, from its codec configuration (the
/// AVC/HEVC decoder configuration record), or None when it cannot be shared.
pub fn video_info(h265: bool, extra_data: &[u8], clock_rate: u32, width: u32, height: u32) -> Option<VideoInfo> {
    let (param_sets, len_size) = if h265 { rtp::hvcc_param_sets(extra_data)? } else { rtp::avcc_param_sets(extra_data)? };
    if param_sets.is_empty() || clock_rate == 0 {
        return None;
    }
    Some(VideoInfo { h265, clock_rate, width, height, len_size, param_sets })
}

/// AAC audio for sharing, from its AudioSpecificConfig.
pub fn audio_info(config: &[u8]) -> Option<AudioInfo> {
    let (rate, ch) = crate::ingest::flv::parse_asc(config)?;
    ((1..=8).contains(&ch) && rate > 0).then(|| AudioInfo { config: config.to_vec(), rate, channels: ch })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vinfo() -> VideoInfo {
        VideoInfo { h265: false, clock_rate: 90000, width: 640, height: 480, len_size: 4, param_sets: vec![vec![0x67, 1, 2, 3], vec![0x68, 1]] }
    }

    fn drain(rx: &mut broadcast::Receiver<Msg>) -> Vec<Arc<Unit>> {
        let mut v = Vec::new();
        while let Ok(Msg::Unit(u)) = rx.try_recv() {
            v.push(u);
        }
        v
    }

    #[test]
    fn nothing_is_kept_before_start_or_when_stopped() {
        let h = RestreamHub::new(true);
        assert!(h.subscribe().is_none());
        h.publish_video(0, true, &[1, 2, 3]);
        h.start(vinfo(), None);
        assert!(h.subscribe().unwrap().backlog.is_empty());
        h.stop();
        assert!(h.subscribe().is_none());
        assert!(h.info().is_none());
    }

    #[test]
    fn new_clients_get_the_units_since_the_last_keyframe() {
        let h = RestreamHub::new(true);
        h.start(vinfo(), None);
        h.publish_video(0, true, &[1]);
        h.publish_video(3000, false, &[2]);
        h.publish_video(6000, true, &[3]);
        h.publish_video(9000, false, &[4]);
        let s = h.subscribe().unwrap();
        let pts: Vec<i64> = s.backlog.iter().map(|u| u.pts).collect();
        assert_eq!(pts, vec![6000, 9000]);
        assert!(s.backlog[0].key);
        let mut rx = s.rx;
        h.publish_video(12000, false, &[5]);
        assert_eq!(drain(&mut rx).len(), 1);
    }

    #[test]
    fn frames_before_the_first_keyframe_are_not_replayed() {
        let h = RestreamHub::new(true);
        h.start(vinfo(), None);
        h.publish_video(0, false, &[1]);
        assert!(h.subscribe().unwrap().backlog.is_empty());
    }

    #[test]
    fn audio_needs_an_audio_track() {
        let h = RestreamHub::new(true);
        h.start(vinfo(), None);
        h.publish_video(0, true, &[1]);
        h.publish_audio(0, &[9]);
        assert_eq!(h.subscribe().unwrap().backlog.len(), 1);
        h.start(vinfo(), Some(AudioInfo { config: vec![0x12, 0x10], rate: 44100, channels: 2 }));
        h.publish_video(0, true, &[1]);
        h.publish_audio(0, &[9]);
        assert_eq!(h.subscribe().unwrap().backlog.len(), 2);
    }

    #[test]
    fn restart_and_stop_tell_clients() {
        let h = RestreamHub::new(true);
        h.start(vinfo(), None);
        let mut rx = h.subscribe().unwrap().rx;
        let g = h.info().unwrap().generation;
        h.start(vinfo(), None);
        assert!(matches!(rx.try_recv(), Ok(Msg::Reset)));
        assert_ne!(h.info().unwrap().generation, g);
        let mut rx = h.subscribe().unwrap().rx;
        h.set_wanted(false);
        assert!(matches!(rx.try_recv(), Ok(Msg::Reset)));
        assert!(!h.wanted());
    }

    #[test]
    fn a_slow_client_loses_frames_without_blocking_the_publisher() {
        let h = RestreamHub::new(true);
        h.start(vinfo(), None);
        let mut rx = h.subscribe().unwrap().rx;
        for i in 0..(CHANNEL_UNITS as i64 * 3) {
            h.publish_video(i, i == 0, &[0; 10]);
        }
        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Lagged(_))));
    }

    #[test]
    fn an_overlong_keyframe_interval_is_not_cached() {
        let h = RestreamHub::new(true);
        h.start(vinfo(), None);
        h.publish_video(0, true, &[0; 10]);
        for i in 1..=(MAX_GOP_UNITS as i64 + 5) {
            h.publish_video(i, false, &[0; 10]);
        }
        assert!(h.subscribe().unwrap().backlog.is_empty());
        h.publish_video(10_000, true, &[0; 10]);
        assert_eq!(h.subscribe().unwrap().backlog.len(), 1);
    }

    #[test]
    fn counts_clients() {
        let h = Arc::new(RestreamHub::new(true));
        let a = h.client_guard();
        let _b = h.client_guard();
        assert_eq!(h.clients(), 2);
        drop(a);
        assert_eq!(h.clients(), 1);
    }

    #[test]
    fn stream_info_from_configuration_records() {
        let mut avcc = vec![1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 2, 0x67, 1, 1, 0, 2, 0x68, 2];
        let v = video_info(false, &avcc, 90000, 10, 10).unwrap();
        assert_eq!(v.param_sets.len(), 2);
        avcc.truncate(6);
        assert!(video_info(false, &avcc, 90000, 10, 10).is_none());
        let a = audio_info(&[0x12, 0x10]).unwrap();
        assert_eq!((a.rate, a.channels), (44100, 2));
        assert!(audio_info(&[0x12]).is_none());
    }
}
