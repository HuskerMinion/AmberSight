//! Direct-to-disk segment recorder: takes encoded frames from the native
//! RTSP reader and writes fragmented MP4 segment files without re-encoding.
//!
//! - A new file starts at a video keyframe once the current one has reached
//!   `segment_seconds`.
//! - A fragment (moof+mdat) is written at every keyframe, or after
//!   `MAX_FRAGMENT_SECS` of buffered video for cameras with very long GOPs,
//!   so a crash loses at most that much.
//! - Audio and video are aligned on a shared time base. Each stream's RTP
//!   clock starts at an arbitrary value, so each track is first anchored to
//!   the local arrival time of its first frame (accurate to network jitter
//!   only). RTCP sender reports, fed in with
//!   [`SegmentWriter::observe_sender_report`], map each stream's RTP clock to
//!   the camera's wall clock; they refine the audio alignment exactly (see
//!   that method) and provide the segments' wall-clock times.

use super::fmp4::{self, Sample, TrackInfo, TrackKind, TrackRun};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_FRAGMENT_SECS: f64 = 2.0;

/// How long the first fragment of a file may be held back waiting for
/// sender reports (cameras send the first one a few seconds after PLAY).
const SR_WAIT_SECS: f64 = 5.0;

/// A sender-report alignment that moves a track by more than this from its
/// arrival-time anchor is treated as a bogus camera clock and ignored.
const MAX_SR_SKEW_SECS: f64 = 2.0;

/// Camera clock drift: a camera's media clock can run a little fast or slow
/// (0.05% is 43 s a day), and segment times are this computer's clock at the
/// session's start plus media time. At each segment boundary the gap to this
/// computer's clock is measured and the next segment is moved toward it by
/// at most this fraction of its length (500 ppm, as Moonfire NVR does): real
/// drift is cancelled, network jitter barely moves anything.
const MAX_DRIFT_FIX: f64 = 0.0005;
/// Drift is only corrected once a session has run this long (seconds of
/// media), and a gap larger than DRIFT_SANE_SECS means the input is not live
/// (a file, a test) and is left alone.
const DRIFT_AFTER_SECS: f64 = 120.0;
const DRIFT_SANE_SECS: f64 = 30.0;

/// A file-name prefix without its start-time part, so every writer of one
/// camera ("porch-1794148581609-rec", "porch-1794148616808-rec") shares a key.
fn camera_key(prefix: &str) -> String {
    prefix.split('-').filter(|p| !(p.len() >= 10 && p.bytes().all(|b| b.is_ascii_digit()))).collect::<Vec<_>>().join("-")
}

/// True when the ignored-alignment warning for this camera has not been
/// logged within the last hour (and records that it is being logged now).
fn sr_warn_due(prefix: &str, now: std::time::Instant) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<HashMap<String, std::time::Instant>>> = OnceLock::new();
    let mut m = LAST.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let key = camera_key(prefix);
    match m.get(&key) {
        Some(t) if now.duration_since(*t) < std::time::Duration::from_secs(3600) => false,
        _ => {
            m.insert(key, now);
            true
        }
    }
}

/// Convert a 64-bit NTP timestamp (32.32 fixed point, seconds since 1900)
/// to seconds since the Unix epoch.
pub fn ntp_to_epoch_secs(ntp: u64) -> f64 {
    (i128::from(ntp) - i128::from(retina::UNIX_EPOCH.0)) as f64 / 4_294_967_296.0
}

/// A finished segment file, for the recordings index.
#[derive(Clone, Debug)]
pub struct SegmentInfo {
    pub path: PathBuf,
    /// Wall-clock start/end in milliseconds since the Unix epoch.
    pub start_ms: i64,
    pub end_ms: i64,
    pub bytes: u64,
    pub video_frames: u64,
    pub fragments: u32,
}

struct Pending {
    /// Track-local decode timestamp in ticks of the track timescale.
    ticks: i64,
    /// Presentation minus decode time (B-frames), in ticks.
    cts: i32,
    data: Vec<u8>,
    is_sync: bool,
}

struct Track {
    info: TrackInfo,
    /// Seconds to add to `ticks / timescale` to get the shared time base
    /// (seconds since the recorder's anchor). Set on the first frame.
    offset: Option<f64>,
    /// The offset from first-frame arrival, kept as the sanity reference
    /// for sender-report alignment.
    arrival_offset: Option<f64>,
    /// Latest sender report: (RTP ticks as passed to the push methods,
    /// camera wall clock in epoch seconds).
    sr: Option<(i64, f64)>,
    pending: Vec<Pending>,
    /// Next decode time in the current file, in ticks.
    next_decode: Option<u64>,
    last_duration: u32,
}

impl Track {
    fn secs(&self, ticks: i64) -> f64 {
        ticks as f64 / self.info.timescale as f64 + self.offset.unwrap_or(0.0)
    }
}

struct OpenFile {
    path: PathBuf,
    file: std::fs::File,
    /// Shared-time-base second at which this file starts (its first keyframe).
    start: f64,
    /// Wall-clock start when known from the previous segment's end.
    start_ms: Option<i64>,
    bytes: u64,
    video_frames: u64,
    fragments: u32,
}

pub struct SegmentWriter {
    dir: PathBuf,
    prefix: String,
    segment_seconds: f64,
    tracks: Vec<Track>,
    video: usize,
    /// Local wall-clock time (epoch seconds) of the shared time base's zero;
    /// used for segment times until the video track has a sender report.
    anchor_epoch: f64,
    anchor_instant: std::time::Instant,
    file: Option<OpenFile>,
    next_index: u32,
    seen_keyframe: bool,
    closed: Vec<SegmentInfo>,
    /// End time of the last closed segment, which is the next one's start.
    last_end_ms: Option<i64>,
    clock_warned: bool,
    /// Seconds added to segment times so far to follow this computer's clock
    /// (see MAX_DRIFT_FIX); and the amount last logged.
    drift: f64,
    drift_logged: f64,
    /// Bytes written, for the status panel's disk write rate.
    written: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Writing footage recorded earlier (see `new_recorded_at`): times come
    /// from the frames alone, not from when they are written.
    recorded: bool,
}

impl SegmentWriter {
    /// A writer for footage recorded earlier (a camera's own SD clip): tick 0
    /// of the first frame is `start_epoch_secs`, and segment times follow the
    /// frames' own timestamps, not this computer's clock as they are written.
    pub fn new_recorded_at(dir: &Path, prefix: &str, tracks: Vec<TrackInfo>, segment_seconds: u64, start_epoch_secs: f64) -> std::io::Result<Self> {
        let mut w = Self::new(dir, prefix, tracks, segment_seconds)?;
        w.anchor_epoch = start_epoch_secs;
        w.recorded = true;
        Ok(w)
    }

    /// `tracks` must contain exactly one video track; `prefix` becomes the
    /// file name stem (`{prefix}-seg000000.mp4`, ...).
    pub fn new(dir: &Path, prefix: &str, tracks: Vec<TrackInfo>, segment_seconds: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let video = tracks
            .iter()
            .position(|t| t.kind == TrackKind::Video)
            .ok_or_else(|| std::io::Error::other("segment writer needs a video track"))?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        Ok(SegmentWriter {
            dir: dir.to_path_buf(),
            prefix: prefix.to_string(),
            segment_seconds: segment_seconds.max(1) as f64,
            tracks: tracks
                .into_iter()
                .map(|info| Track { info, offset: None, arrival_offset: None, sr: None, pending: Vec::new(), next_decode: None, last_duration: 0 })
                .collect(),
            video,
            anchor_epoch: now,
            anchor_instant: std::time::Instant::now(),
            file: None,
            next_index: 0,
            seen_keyframe: false,
            closed: Vec::new(),
            last_end_ms: None,
            clock_warned: false,
            drift: 0.0,
            drift_logged: 0.0,
            written: crate::telemetry::write_counter(dir),
            recorded: false,
        })
    }

    /// Segments closed since the last call.
    pub fn take_closed(&mut self) -> Vec<SegmentInfo> {
        std::mem::take(&mut self.closed)
    }

    /// Place a track on the clock by a frame that arrived just now, before
    /// writing older ones: pre-roll frames from memory start the writer, and
    /// anchoring by the oldest of them would date the whole recording as
    /// late as the pre-roll is long.
    pub fn anchor_latest(&mut self, kind: TrackKind, ticks: i64) {
        if let Some(i) = self.tracks.iter().position(|t| t.info.kind == kind) {
            self.anchor_track(i, ticks);
        }
    }

    fn anchor_track(&mut self, i: usize, ticks: i64) {
        if self.tracks[i].offset.is_none() {
            let arrival = if self.recorded { 0.0 } else { self.anchor_instant.elapsed().as_secs_f64() };
            let t = &mut self.tracks[i];
            let off = arrival - ticks as f64 / t.info.timescale as f64;
            t.offset = Some(off);
            t.arrival_offset = Some(off);
        }
    }

    /// Record an RTCP sender report: at RTP time `rtp_ticks` of the `kind`
    /// stream (ticks since that stream's first packet, the same value as
    /// the `ticks` given to `push_video`/`push_audio`) the camera's wall
    /// clock read `ntp_epoch_secs` (seconds since the Unix epoch; see
    /// [`ntp_to_epoch_secs`]). Ignored for kinds the writer has no track for
    /// and for implausible times. Call for every report, in any order
    /// relative to the frames.
    ///
    /// - Segment `start_ms`/`end_ms` use the video track's latest report, so
    ///   they are the camera's clock rather than the local one (when no
    ///   report has arrived they fall back to local arrival time).
    /// - Audio is placed on the video clock exactly: its offset is
    ///   `ntp_audio - ticks_audio/rate - K`, where K maps the shared time
    ///   base to wall clock via the video report. The new audio offset is
    ///   adopted only while no audio of the current file has been written
    ///   (at file open, or before the first audio fragment), never later in
    ///   a file, so written fragments stay valid and decode times stay
    ///   monotonic within a file. A report arriving mid-file therefore
    ///   takes effect from the next rotation. The video offset is never changed.
    pub fn observe_sender_report(&mut self, kind: TrackKind, rtp_ticks: i64, ntp_epoch_secs: f64) {
        if !ntp_epoch_secs.is_finite() || ntp_epoch_secs < 1.0e9 {
            return;
        }
        if let Some(t) = self.tracks.iter_mut().find(|t| t.info.kind == kind) {
            t.sr = Some((rtp_ticks, ntp_epoch_secs));
        }
        self.check_camera_clock();
        self.adopt_sr_alignment_if_unwritten();
    }

    /// Seconds to add to the shared time base to get epoch seconds, from the
    /// video track's sender report. None until that report and the video
    /// anchor both exist.
    fn sr_wall_k(&self) -> Option<f64> {
        let v = &self.tracks[self.video];
        let ((ticks, ntp), off) = (v.sr?, v.offset?);
        Some(ntp - ticks as f64 / v.info.timescale as f64 - off)
    }

    /// Seconds to add to the shared time base to get epoch seconds. Always
    /// this machine's clock: event times come from it too, and camera clocks
    /// are often minutes (or a daylight-saving hour) off. Sender reports
    /// only align audio with video.
    fn wall_k(&self) -> f64 {
        self.anchor_epoch + self.drift
    }

    /// After a segment ending at media time `end` (just now): move the
    /// segment times a little toward this computer's clock (MAX_DRIFT_FIX).
    fn follow_clock(&mut self, end: f64, seg_len: f64) {
        if self.recorded || end < DRIFT_AFTER_SECS {
            return;
        }
        // This computer's (monotonic) clock against the media clock.
        let gap = self.anchor_instant.elapsed().as_secs_f64() - (end + self.drift);
        if gap.abs() > DRIFT_SANE_SECS {
            return;
        }
        let step = MAX_DRIFT_FIX * seg_len.max(0.0);
        self.drift += gap.clamp(-step, step);
        if (self.drift - self.drift_logged).abs() >= 1.0 {
            self.drift_logged = self.drift;
            tracing::info!(
                "{}: camera clock drift: recordings moved {:+.1} s in {:.1} h to follow this computer's clock",
                self.prefix,
                self.drift,
                end / 3600.0
            );
        }
    }

    /// Warn once if the camera's clock (from its sender reports) is far from
    /// this machine's.
    fn check_camera_clock(&mut self) {
        if self.clock_warned {
            return;
        }
        if let Some(k) = self.sr_wall_k() {
            let skew = k - self.anchor_epoch;
            if skew.abs() > 2.0 {
                // Reported per camera in the health page; log once quietly.
                tracing::info!(
                    "{}: camera clock is {skew:+.1} s off this computer's clock (recordings use this computer's time)",
                    self.prefix
                );
                self.clock_warned = true;
            }
        }
    }


    fn adopt_sr_alignment_if_unwritten(&mut self) {
        if self.tracks.iter().any(|t| t.info.kind == TrackKind::Audio && t.next_decode.is_none()) {
            self.adopt_sr_alignment();
        }
    }

    /// True while an audio track exists and a sender report is missing for
    /// it or for the video track.
    fn awaiting_sender_reports(&self) -> bool {
        self.tracks.iter().any(|t| t.info.kind == TrackKind::Audio) && self.tracks.iter().any(|t| t.sr.is_none())
    }

    /// Re-place audio on the video clock using sender reports; called when a
    /// new file is about to open.
    fn adopt_sr_alignment(&mut self) {
        let Some(k) = self.sr_wall_k() else { return };
        for t in self.tracks.iter_mut().filter(|t| t.info.kind == TrackKind::Audio) {
            let (Some((ticks, ntp)), Some(arrival)) = (t.sr, t.arrival_offset) else { continue };
            let aligned = ntp - ticks as f64 / t.info.timescale as f64 - k;
            if (aligned - arrival).abs() <= MAX_SR_SKEW_SECS {
                if (aligned - t.offset.unwrap_or(arrival)).abs() > 0.001 {
                    tracing::debug!(
                        "audio aligned by sender reports: {:+.1} ms against arrival anchoring",
                        (aligned - arrival) * 1000.0
                    );
                }
                t.offset = Some(aligned);
            } else {
                // Tried again for every audio frame until the first audio
                // fragment is written: say so once per camera per hour.
                let skew = aligned - arrival;
                if sr_warn_due(&self.prefix, std::time::Instant::now()) {
                    tracing::warn!(
                        "{}: ignoring sender-report alignment {skew:+.2} s from arrival anchoring (audio is placed by arrival time; logged once an hour)",
                        self.prefix
                    );
                } else {
                    tracing::debug!("{}: ignoring sender-report alignment {skew:+.2} s", self.prefix);
                }
            }
        }
    }

    /// Add a video frame shown at its decode time (no B-frames).
    #[cfg(test)]
    pub fn push_video(&mut self, ticks: i64, data: Vec<u8>, is_key: bool) -> std::io::Result<()> {
        self.push_video_cts(ticks, 0, data, is_key)
    }

    /// Add a video frame (length-prefixed NALs), in decode order: `ticks` is
    /// its decode time in ticks of the video timescale, `cts` how much later
    /// it is shown (B-frames; 0 otherwise).
    pub fn push_video_cts(&mut self, ticks: i64, cts: i32, data: Vec<u8>, is_key: bool) -> std::io::Result<()> {
        let v = self.video;
        self.anchor_track(v, ticks);
        if !self.seen_keyframe {
            if !is_key {
                return Ok(()); // a file must start on a keyframe
            }
            self.seen_keyframe = true;
        }
        let t = self.tracks[v].secs(ticks);
        let buffered_from = self.tracks[v].pending.first().map(|p| self.tracks[v].secs(p.ticks));
        let long_gop = buffered_from.is_some_and(|s| t - s >= MAX_FRAGMENT_SECS);
        if is_key || long_gop {
            self.flush_fragment(ticks, false)?;
        }
        if is_key {
            let rotate = match &self.file {
                None => true,
                // 1 ms tolerance: a keyframe exactly on the boundary must cut.
                Some(f) => t - f.start >= self.segment_seconds - 0.001,
            };
            if rotate {
                self.close_file(t)?;
                self.adopt_sr_alignment();
                self.open_file(t)?;
            }
        }
        self.tracks[v].pending.push(Pending { ticks, cts, data, is_sync: is_key });
        Ok(())
    }

    /// Add an audio frame (raw AAC) with its RTP timestamp in ticks of the
    /// audio timescale. Ignored when the writer has no audio track.
    pub fn push_audio(&mut self, ticks: i64, data: Vec<u8>) {
        let Some(a) = self.tracks.iter().position(|t| t.info.kind == TrackKind::Audio) else { return };
        self.anchor_track(a, ticks);
        if self.tracks[a].next_decode.is_none() {
            // Nothing of this track is in the current file yet, so it can
            // still be re-placed without touching written data.
            self.adopt_sr_alignment();
        }
        if !self.seen_keyframe {
            return;
        }
        self.tracks[a].pending.push(Pending { ticks, cts: 0, data, is_sync: true });
    }

    /// Write buffered samples as one fragment. `video_end_ticks` is the
    /// timestamp of the frame that follows the buffered video (it gives the
    /// last video sample its duration).
    ///
    /// The first fragment of a recording with audio is held back (unless
    /// `force`) for up to `SR_WAIT_SECS` while the sender reports needed to
    /// align audio are still missing, since audio placement is fixed once its
    /// first fragment is written. Nothing is lost: the held frames go out in
    /// the next fragment.
    fn flush_fragment(&mut self, video_end_ticks: i64, force: bool) -> std::io::Result<()> {
        let Some((file_start, fragments)) = self.file.as_ref().map(|f| (f.start, f.fragments)) else {
            for t in &mut self.tracks {
                t.pending.clear();
            }
            return Ok(());
        };
        if !force && fragments == 0 && self.awaiting_sender_reports() {
            let span = self.tracks[self.video].secs(video_end_ticks) - file_start;
            if span < SR_WAIT_SECS.min(self.segment_seconds) - 0.001 {
                return Ok(());
            }
        }
        let v = self.video;
        let cut = self.tracks[v].secs(video_end_ticks);
        let mut runs = Vec::new();
        let mut video_frames = 0u64;
        for (i, track) in self.tracks.iter_mut().enumerate() {
            let is_video = i == v;
            // Which samples go in this fragment: all buffered video; audio
            // that starts before the cut and whose duration is known (it
            // has a successor), so durations never have to be guessed.
            let take = if is_video {
                track.pending.len()
            } else {
                let mut n = 0;
                while n + 1 < track.pending.len() && track.secs(track.pending[n].ticks) < cut {
                    n += 1;
                }
                n
            };
            if take == 0 {
                continue;
            }
            let rest = track.pending.split_off(take);
            let batch = std::mem::replace(&mut track.pending, rest);
            let first_ticks = batch[0].ticks;
            let next_ticks = if is_video { Some(video_end_ticks) } else { track.pending.first().map(|p| p.ticks) };
            let mut samples = Vec::with_capacity(batch.len());
            for (k, p) in batch.iter().enumerate() {
                let following = batch.get(k + 1).map(|q| q.ticks).or(next_ticks);
                let d = match following {
                    Some(n) if n > p.ticks => (n - p.ticks).min(u32::MAX as i64) as u32,
                    _ => track.last_duration.max(1),
                };
                track.last_duration = d;
                samples.push(Sample { duration: d, data: Vec::new(), is_sync: p.is_sync, composition_offset: p.cts });
            }
            for (s, p) in samples.iter_mut().zip(batch) {
                s.data = p.data;
            }
            let base = match track.next_decode {
                Some(b) => b,
                None => {
                    // First run of this track in the file: place it relative
                    // to the file start. Audio that predates the file's first
                    // keyframe is clamped to 0.
                    let first = track.secs(first_ticks);
                    ((first - file_start).max(0.0) * track.info.timescale as f64).round() as u64
                }
            };
            let total: u64 = samples.iter().map(|s| s.duration as u64).sum();
            track.next_decode = Some(base + total);
            if is_video {
                video_frames = samples.len() as u64;
            }
            runs.push(TrackRun { track_index: i, base_decode_time: base, samples });
        }
        if runs.is_empty() {
            return Ok(());
        }
        let f = self.file.as_mut().unwrap();
        f.fragments += 1;
        let frag = fmp4::fragment(f.fragments, &runs);
        f.file.write_all(&frag)?;
        f.file.flush()?;
        f.bytes += frag.len() as u64;
        self.written.fetch_add(frag.len() as u64, std::sync::atomic::Ordering::Relaxed);
        f.video_frames += video_frames;
        Ok(())
    }

    fn open_file(&mut self, start: f64) -> std::io::Result<()> {
        let path = self.dir.join(format!("{}-seg{:06}.mp4", self.prefix, self.next_index));
        self.next_index += 1;
        let infos: Vec<TrackInfo> = self.tracks.iter().map(|t| t.info.clone()).collect();
        let init = fmp4::init_segment(&infos);
        let mut file = std::fs::File::create(&path)?;
        file.write_all(&init)?;
        self.written.fetch_add(init.len() as u64, std::sync::atomic::Ordering::Relaxed);
        for t in &mut self.tracks {
            t.next_decode = None;
        }
        let start_ms = self.last_end_ms.take();
        self.file = Some(OpenFile { path, file, start, start_ms, bytes: init.len() as u64, video_frames: 0, fragments: 0 });
        Ok(())
    }

    fn close_file(&mut self, end: f64) -> std::io::Result<()> {
        if let Some(mut f) = self.file.take() {
            f.file.flush()?;
            let k = self.wall_k();
            let to_ms = |s: f64| ((k + s) * 1000.0).round() as i64;
            let end_ms = to_ms(end);
            self.last_end_ms = Some(end_ms);
            self.follow_clock(end, end - f.start);
            let local_ms = ((self.anchor_epoch + end) * 1000.0).round() as i64;
            tracing::debug!(
                "segment {} closed: end {end_ms} ms from {}, {:+} ms against local arrival time",
                f.path.display(),
                if self.sr_wall_k().is_some() { "sender report" } else { "local time" },
                end_ms - local_ms,
            );
            self.closed.push(SegmentInfo {
                path: f.path,
                start_ms: f.start_ms.unwrap_or_else(|| to_ms(f.start)),
                end_ms,
                bytes: f.bytes,
                video_frames: f.video_frames,
                fragments: f.fragments,
            });
        }
        Ok(())
    }

    /// Flush what is buffered and close the current file (stream ended or
    /// the camera is being stopped).
    pub fn finish(&mut self) -> std::io::Result<()> {
        let v = self.video;
        let end_ticks = match self.tracks[v].pending.last() {
            Some(p) => p.ticks + self.tracks[v].last_duration.max(1) as i64,
            None => {
                let end = self.file.as_ref().map(|f| f.start).unwrap_or(0.0);
                return self.close_file(end);
            }
        };
        self.flush_fragment(end_ticks, true)?;
        let end = self.tracks[v].secs(end_ticks);
        self.close_file(end)
    }
}

/// One item from a playing stream, whichever reader produced it.
#[derive(Debug)]
pub enum MediaItem {
    /// Length-prefixed NAL units; `ticks` is the decode time in the video
    /// clock (90 kHz), `cts` the presentation time minus the decode time
    /// (non-zero only with B-frames). Frames come in decode order.
    Video { ticks: i64, cts: i32, data: Vec<u8>, key: bool, new_params: bool },
    /// Raw AAC; `ticks` in the audio sample rate.
    Audio { ticks: i64, data: Vec<u8> },
    /// Maps a track's ticks to the camera's wall clock (an RTCP sender
    /// report, or the FLV reader's shared timestamps). track 0 = video,
    /// 1 = audio.
    SenderReport { track: usize, ticks: i64, epoch_secs: f64 },
}

/// Video codec parameters, once known.
#[derive(Clone, Debug)]
pub struct VideoParams {
    /// RFC 6381 codec string (avc1.* / hvc1.*).
    pub codec: String,
    pub h265: bool,
    /// AVC/HEVC decoder configuration record.
    pub extra_data: Vec<u8>,
    pub sample_entry: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub clock_rate: u32,
}

/// AAC parameters, once known.
#[derive(Clone, Debug)]
pub struct AudioParams {
    pub codec: String,
    pub sample_entry: Vec<u8>,
    pub clock_rate: u32,
}

/// Samples in one AAC-LC frame (the audio timescale is the sample rate).
const AAC_FRAME: i64 = 1024;

enum Reader {
    /// `g711`: the audio track is G.711, converted to FLAC frames on the way.
    /// `aac_next`: where the next AAC frame starts at the earliest (see below).
    Rtsp { demuxed: retina::client::Demuxed, video_i: usize, audio_i: Option<usize>, g711: Option<super::g711::ToFlac>, aac_next: Option<i64> },
    Flv(Box<super::flv::FlvStream>),
    /// A battery Argus over the clean-room Baichuan client: already-converted
    /// `MediaItem::Video` frames arrive on a channel; the handle stops preview
    /// when dropped. Video only (no audio track yet).
    Baichuan(super::baichuan::client::PreviewHandle),
}

/// A playing stream (RTSP, or HTTP-FLV) with its video and optional audio.
pub struct OpenStream {
    reader: Reader,
    /// How long silence means the stream is dead (longer while a battery
    /// camera is still waking up).
    pub stall_limit: std::time::Duration,
    /// When the last video frame arrived (or PLAY, before the first one).
    /// Only video counts as alive: a stuck device may keep sending RTCP.
    pub last_video: std::time::Instant,
    /// Record/forward the audio track (it may be set up only because some
    /// devices stream nothing unless every track is set up).
    pub use_audio: bool,
    /// Pass audio frames on even when they are not recorded (the loud-sound
    /// trigger listens to them; see `audio_codec`).
    pub tap_audio: bool,
    /// What the current video parameters amount to (see `material_key`).
    video_key: Option<MaterialKey>,
    /// Decode times for RTSP video (RTP gives presentation times only).
    video_dts: super::dts::DtsFromPts,
    /// `video_dts` has been told what the codec configuration allows.
    video_dts_checked: bool,
}

/// The parts of the video parameters that matter for decoding and for the
/// MP4 files: codec, picture size and the parameter sets other than the SPS
/// (H.264: the PPS; H.265: the VPS and PPS, plus the format fields of the
/// configuration record). Some cameras re-send their SPS at every keyframe
/// with only the optional timing fields changed (their frame-rate
/// estimate); treating that as new parameters would restart the recording
/// file and the live view each time (for a motion recording: a new
/// one-second recording per keyframe, and a live view that keeps
/// restarting).
type MaterialKey = (String, u32, u32, Vec<u8>);

fn material_key(v: &VideoParams) -> MaterialKey {
    (v.codec.clone(), v.width, v.height, if v.h265 { hvcc_material(&v.extra_data) } else { avcc_pps(&v.extra_data) })
}

/// An HEVCDecoderConfigurationRecord without its SPS and frame-rate fields:
/// profile, tier, level, chroma format and bit depths, the NAL length size,
/// and every parameter-set array except the SPS (NAL type 33). The whole
/// record if it cannot be read.
fn hvcc_material(hvcc: &[u8]) -> Vec<u8> {
    let read = || -> Option<Vec<u8>> {
        // Bytes 0..19: version up to bitDepthChromaMinus8; 19..21 is the
        // average frame rate; 21 holds constantFrameRate, the temporal
        // layers and lengthSizeMinusOne; 22 the number of arrays.
        let mut out = hvcc.get(..19)?.to_vec();
        out.push(*hvcc.get(21)? & 0x03);
        let n_arrays = *hvcc.get(22)?;
        let mut i = 23;
        for _ in 0..n_arrays {
            let start = i;
            let nal_type = hvcc.get(i)? & 0x3f;
            let n_nals = u16::from_be_bytes([*hvcc.get(i + 1)?, *hvcc.get(i + 2)?]);
            i += 3;
            for _ in 0..n_nals {
                let l = u16::from_be_bytes([*hvcc.get(i)?, *hvcc.get(i + 1)?]) as usize;
                i += 2 + l;
            }
            let array = hvcc.get(start..i)?;
            if nal_type != 33 {
                out.extend_from_slice(array);
            }
        }
        Some(out)
    };
    read().unwrap_or_else(|| hvcc.to_vec())
}

/// The PPS part of an AVCDecoderConfigurationRecord (count, lengths and
/// bytes; empty if malformed).
fn avcc_pps(avcc: &[u8]) -> Vec<u8> {
    let mut i = 6;
    let Some(n_sps) = avcc.get(5).map(|b| b & 0x1f) else { return Vec::new() };
    for _ in 0..n_sps {
        let Some(l) = avcc.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) else { return Vec::new() };
        i += 2 + l;
    }
    avcc.get(i..).map(|r| r.to_vec()).unwrap_or_default()
}

impl OpenStream {
    /// Next item, or None when the stream ended. Cancel-safe.
    pub async fn next(&mut self) -> Option<anyhow::Result<MediaItem>> {
        use futures::StreamExt;
        use retina::codec::CodecItem;
        loop {
            match &mut self.reader {
                Reader::Rtsp { demuxed, video_i, audio_i, g711, aac_next } => {
                    let item = match demuxed.next().await? {
                        Ok(i) => i,
                        // Keep retina's text: it says whether one packet was bad.
                        Err(e) => return Some(Err(anyhow::Error::msg(e.to_string()))),
                    };
                    match item {
                        CodecItem::VideoFrame(f) if f.stream_id() == *video_i => {
                            if f.has_new_parameters() || !self.video_dts_checked {
                                // Baseline H.264 has no B-frames: pass timestamps on as they are.
                                use retina::codec::ParametersRef;
                                let never = match demuxed.streams()[*video_i].parameters() {
                                    Some(ParametersRef::Video(v)) => {
                                        v.rfc6381_codec().starts_with("avc1") && super::dts::avcc_never_reorders(v.extra_data())
                                    }
                                    _ => false,
                                };
                                self.video_dts.set_never_reorders(never);
                                self.video_dts_checked = true;
                            }
                            // RTP timestamps are presentation times.
                            let (ticks, cts) = self.video_dts.push(f.timestamp().elapsed(), f.is_random_access_point());
                            let item = MediaItem::Video {
                                ticks,
                                cts,
                                key: f.is_random_access_point(),
                                new_params: f.has_new_parameters(),
                                data: f.into_data(),
                            };
                            return Some(Ok(self.settle_params(item)));
                        }
                        CodecItem::AudioFrame(f) if (self.use_audio || self.tap_audio) && Some(f.stream_id()) == *audio_i => {
                            let mut ticks = f.timestamp().elapsed();
                            let data = match g711.as_mut() {
                                Some(c) => c.convert(f.data()),
                                None => {
                                    let Some(d) = super::aac::raw_frame(f.data()) else { continue };
                                    // An AAC frame is 1024 samples. Some cameras give
                                    // two frames one time (a Kuna sends pairs 2048
                                    // apart): the second starts where the first ends.
                                    // A big jump back (a restart) is taken as it is.
                                    if let Some(n) = *aac_next {
                                        if ticks < n && n - ticks <= 4 * AAC_FRAME {
                                            ticks = n;
                                        }
                                    }
                                    *aac_next = Some(ticks + AAC_FRAME);
                                    d.to_vec()
                                }
                            };
                            return Some(Ok(MediaItem::Audio { ticks, data }));
                        }
                        CodecItem::Rtcp(r) => {
                            let track = match r.stream_id() {
                                i if i == *video_i => 0,
                                i if Some(i) == *audio_i => 1,
                                _ => continue,
                            };
                            let Some(ts) = r.rtp_timestamp() else { continue };
                            let Some(Ok(Some(sr))) = r.pkts().next().map(|p| p.as_sender_report()) else { continue };
                            return Some(Ok(MediaItem::SenderReport {
                                track,
                                ticks: ts.elapsed(),
                                epoch_secs: ntp_to_epoch_secs(sr.ntp_timestamp().0),
                            }));
                        }
                        _ => continue,
                    }
                }
                Reader::Flv(f) => {
                    use super::flv::FlvItem;
                    let item = match f.next().await? {
                        Ok(i) => i,
                        Err(e) => return Some(Err(e)),
                    };
                    return Some(Ok(match item {
                        FlvItem::Video { ticks, cts, data, key, new_params } => {
                            self.settle_params(MediaItem::Video { ticks, cts, data, key, new_params })
                        }
                        FlvItem::Audio { ticks, data } => {
                            if !self.use_audio && !self.tap_audio {
                                continue;
                            }
                            MediaItem::Audio { ticks, data }
                        }
                        FlvItem::Timing { track, ticks, epoch_secs } => {
                            if track == 1 && !self.use_audio {
                                continue;
                            }
                            MediaItem::SenderReport { track, ticks, epoch_secs }
                        }
                    }));
                }
                Reader::Baichuan(h) => {
                    // The client already produced MediaItem::Video (AVCC);
                    // None means preview ended / the session dropped.
                    let item = h.rx.recv().await?;
                    return Some(Ok(self.settle_params(item)));
                }
            }
        }
    }

    /// Keep `new_params` only for material changes (see `material_key`).
    fn settle_params(&mut self, item: MediaItem) -> MediaItem {
        if !matches!(item, MediaItem::Video { new_params: true, .. }) {
            return item;
        }
        let now = self.video_params().ok().flatten();
        settle_new_params(&mut self.video_key, now.as_ref(), item)
    }
}

/// `new_params` of a video item stays set only when the parameters `now`
/// differ materially from the last ones (`last`, updated).
fn settle_new_params(last: &mut Option<MaterialKey>, now: Option<&VideoParams>, item: MediaItem) -> MediaItem {
    let MediaItem::Video { ticks, cts, data, key, new_params: true } = item else { return item };
    let now = now.map(material_key);
    let material = now.is_none() || now != *last;
    if material {
        *last = now;
    }
    MediaItem::Video { ticks, cts, data, key, new_params: material }
}

impl OpenStream {

    /// Video parameters, once the camera has sent them.
    pub fn video_params(&self) -> anyhow::Result<Option<VideoParams>> {
        use retina::codec::ParametersRef;
        match &self.reader {
            Reader::Rtsp { demuxed, video_i, .. } => {
                let streams = demuxed.streams();
                let Some(ParametersRef::Video(v)) = streams[*video_i].parameters() else { return Ok(None) };
                let (width, height) = v.pixel_dimensions();
                let codec = v.rfc6381_codec().to_string();
                Ok(Some(VideoParams {
                    h265: !codec.starts_with("avc"),
                    codec,
                    extra_data: v.extra_data().to_vec(),
                    sample_entry: v.mp4_sample_entry().build().map_err(|e| anyhow::anyhow!("video sample entry: {e}"))?,
                    width,
                    height,
                    clock_rate: streams[*video_i].clock_rate_hz(),
                }))
            }
            Reader::Flv(f) => {
                let Some(v) = f.video() else { return Ok(None) };
                let codec = super::flv::codec_string(v.h265, &v.record)
                    .ok_or_else(|| anyhow::anyhow!("unreadable video configuration"))?;
                let (w, h) = (u16::try_from(v.width)?, u16::try_from(v.height)?);
                Ok(Some(VideoParams {
                    codec,
                    h265: v.h265,
                    extra_data: v.record.clone(),
                    sample_entry: super::flv::video_sample_entry(v.h265, &v.record, w, h),
                    width: v.width,
                    height: v.height,
                    clock_rate: 90_000,
                }))
            }
            Reader::Baichuan(h) => Ok(h.video_params()),
        }
    }

    /// How to decode the audio frames, whether or not they are recorded:
    /// None until known, or when the stream has no audio we can read.
    pub fn audio_codec(&self) -> Option<crate::sound::AudioCodec> {
        use crate::sound::AudioCodec;
        use retina::codec::ParametersRef;
        match &self.reader {
            Reader::Rtsp { g711: Some(c), .. } => Some(AudioCodec::Flac16 { rate: c.rate() }),
            Reader::Rtsp { demuxed, audio_i, .. } => {
                let Some(ParametersRef::Audio(ap)) = demuxed.streams()[(*audio_i)?].parameters() else { return None };
                ap.rfc6381_codec()?.starts_with("mp4a.").then(|| AudioCodec::Aac { config: ap.extra_data().to_vec() })
            }
            Reader::Flv(f) => f.audio_config().map(|asc| AudioCodec::Aac { config: asc.to_vec() }),
            Reader::Baichuan(_) => None,
        }
    }

    /// Audio parameters when audio is in use, recordable, and known.
    pub fn audio_params(&self) -> Option<AudioParams> {
        use retina::codec::ParametersRef;
        if !self.use_audio {
            return None;
        }
        match &self.reader {
            Reader::Rtsp { g711: Some(c), .. } => Some(AudioParams {
                codec: "flac".to_string(),
                sample_entry: super::g711::flac_sample_entry(c.rate()),
                clock_rate: c.rate(),
            }),
            Reader::Rtsp { demuxed, audio_i, .. } => {
                let Some(ParametersRef::Audio(ap)) = demuxed.streams()[(*audio_i)?].parameters() else { return None };
                match ap.mp4_sample_entry().build() {
                    Ok(entry) => Some(AudioParams {
                        codec: ap.rfc6381_codec().unwrap_or("mp4a.40.2").to_string(),
                        sample_entry: entry,
                        clock_rate: ap.clock_rate(),
                    }),
                    Err(e) => {
                        tracing::warn!("audio not recordable, recording video only: {e}");
                        None
                    }
                }
            }
            Reader::Flv(f) => {
                let asc = f.audio_config()?;
                let (rate, _) = super::flv::parse_asc(asc)?;
                let object_type = asc[0] >> 3;
                Some(AudioParams {
                    codec: format!("mp4a.40.{object_type}"),
                    sample_entry: super::flv::aac_sample_entry(asc)?,
                    clock_rate: rate,
                })
            }
            Reader::Baichuan(_) => None,
        }
    }
}

/// Open a camera stream: HTTP-FLV for `http(s)://host/flv?...` URLs,
/// otherwise RTSP (DESCRIBE/SETUP/PLAY over TCP with MP4 frame format:
/// length-prefixed NALs, raw AAC, which both the recorder and the decoder
/// consume).
pub async fn open_stream(src: &super::StreamSource, with_audio: bool) -> anyhow::Result<OpenStream> {
    use anyhow::{anyhow, Context};
    use retina::client::{PlayOptions, Session, SetupOptions, Transport};
    use retina::codec::FrameFormat;

    let opened = |reader| OpenStream {
        reader,
        stall_limit: std::time::Duration::from_secs(10),
        last_video: std::time::Instant::now(),
        use_audio: with_audio,
        tap_audio: false,
        video_key: None,
        video_dts: super::dts::DtsFromPts::new(90_000),
        video_dts_checked: false,
    };
    if super::baichuan::is_baichuan_url(&src.url) {
        // The persistent session is created and kept connected by the event
        // poller (engine::camera_events); here we only ask it for frames.
        let (_host, uid) = super::baichuan::parse_url(&src.url).ok_or_else(|| anyhow!("bad baichuan URL"))?;
        let uid = uid.ok_or_else(|| anyhow!("battery camera URL has no uid"))?;
        let client = super::baichuan::registry::get(&uid)
            .ok_or_else(|| anyhow!("battery camera session is not up yet"))?;
        // Signing in to a sleeping camera takes seconds: wait off this runtime.
        let h = tokio::task::spawn_blocking(move || client.start_preview())
            .await
            .ok()
            .flatten()
            .ok_or_else(|| anyhow!("battery camera did not start preview"))?;
        return Ok(opened(Reader::Baichuan(h)));
    }
    if super::flv::is_flv_url(&src.url) {
        let f = super::flv::FlvStream::open(&src.url, &src.username, &src.password).await?;
        return Ok(opened(Reader::Flv(Box::new(f))));
    }
    if let Some(key) = super::push::push_key(&src.url) {
        let r = super::push::take_stream(key, std::time::Duration::from_secs(15))
            .await
            .ok_or_else(|| anyhow!("waiting for a stream to be pushed to this camera"))?;
        let f = super::flv::FlvStream::from_io(r).await?;
        return Ok(opened(Reader::Flv(Box::new(f))));
    }

    let url = url::Url::parse(&src.url).context("invalid stream URL")?;
    let mut session = Session::describe(url, super::session_options(src))
        .await
        .map_err(|e| anyhow!("DESCRIBE failed: {e}"))?;
    let video_i = session
        .streams()
        .iter()
        .position(|s| s.media() == "video" && matches!(s.encoding_name(), "h264" | "h265"))
        .ok_or_else(|| anyhow!("no H.264/H.265 video stream offered"))?;
    // AAC as is; else mono G.711 (made browser-playable as FLAC, see g711.rs).
    let (audio_i, g711) = if with_audio {
        let st = session.streams();
        match st.iter().position(|s| s.media() == "audio" && s.encoding_name() == "mpeg4-generic") {
            Some(i) => (Some(i), None),
            None => match st.iter().position(|s| {
                s.media() == "audio"
                    && super::g711::Law::from_encoding(s.encoding_name()).is_some()
                    && s.channels().map_or(1, |c| c.get()) == 1
                    && s.clock_rate_hz() > 0
                    && s.clock_rate_hz() <= 65535
            }) {
                Some(i) => {
                    let law = super::g711::Law::from_encoding(st[i].encoding_name()).unwrap();
                    (Some(i), Some(super::g711::ToFlac::new(law, st[i].clock_rate_hz())))
                }
                None => (None, None),
            },
        }
    } else {
        (None, None)
    };
    let setup = || {
        SetupOptions::default()
            .transport(Transport::Tcp(Default::default()))
            .frame_format(FrameFormat::MP4)
    };
    session.setup(video_i, setup()).await.map_err(|e| anyhow!("SETUP video failed: {e}"))?;
    if let Some(a) = audio_i {
        session.setup(a, setup()).await.map_err(|e| anyhow!("SETUP audio failed: {e}"))?;
    }
    // Some devices (a hub relaying a battery camera) send nothing unless
    // every track is set up, so set up the rest too; their frames are
    // ignored. Best effort: a track that cannot be set up is skipped.
    if with_audio {
        let extra: Vec<usize> = (0..session.streams().len())
            .filter(|&i| i != video_i && Some(i) != audio_i && session.streams()[i].media() == "audio")
            .collect();
        for i in extra {
            let _ = session.setup(i, SetupOptions::default().transport(Transport::Tcp(Default::default()))).await;
        }
    }
    let demuxed = session
        .play(PlayOptions::default().initial_timestamp(retina::client::InitialTimestampPolicy::Permissive))
        .await
        .map_err(|e| anyhow!("PLAY failed: {e}"))?
        .demuxed()
        .map_err(|e| anyhow!("demux setup failed: {e}"))?;
    Ok(opened(Reader::Rtsp { demuxed, video_i, audio_i, g711, aac_next: None }))
}

/// Track descriptions for the MP4 writer, once codec parameters are known.
/// None until the video parameters have arrived.
pub fn build_tracks(s: &OpenStream) -> anyhow::Result<Option<Vec<TrackInfo>>> {
    let Some(v) = s.video_params()? else { return Ok(None) };
    let mut tracks = vec![TrackInfo {
        kind: TrackKind::Video,
        timescale: v.clock_rate,
        sample_entry: v.sample_entry,
        width: v.width,
        height: v.height,
    }];
    if let Some(a) = s.audio_params() {
        tracks.push(TrackInfo { kind: TrackKind::Audio, timescale: a.clock_rate, sample_entry: a.sample_entry, width: 0, height: 0 });
    }
    Ok(Some(tracks))
}

/// How `record_stream` decides what to keep.
#[derive(Clone, Copy, Debug)]
pub enum RecordMode {
    /// Record everything.
    Continuous,
    /// Buffer `pre_roll` seconds in memory and record only from a trigger
    /// fired `trigger_after` into the session (a stand-in for motion),
    /// continuing `post_roll` seconds after it.
    Triggered { trigger_after: std::time::Duration, pre_roll: u64, post_roll: u64 },
}

enum Sink {
    Continuous(SegmentWriter),
    Triggered(super::preroll::TriggeredRecorder),
}

impl Sink {
    fn push(&mut self, f: super::preroll::Frame) -> std::io::Result<()> {
        use super::preroll::Frame;
        match self {
            Sink::Continuous(w) => match f {
                Frame::Video { ticks, cts, data, is_key } => w.push_video_cts(ticks, cts, data, is_key),
                Frame::Audio { ticks, data } => {
                    w.push_audio(ticks, data);
                    Ok(())
                }
            },
            Sink::Triggered(r) => r.push(f),
        }
    }
    fn observe_sender_report(&mut self, kind: TrackKind, rtp_ticks: i64, ntp_epoch_secs: f64) {
        match self {
            Sink::Continuous(w) => w.observe_sender_report(kind, rtp_ticks, ntp_epoch_secs),
            Sink::Triggered(r) => r.observe_sender_report(kind, rtp_ticks, ntp_epoch_secs),
        }
    }
    fn finish(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Continuous(w) => w.finish(),
            Sink::Triggered(r) => r.stop(),
        }
    }
    fn take_closed(&mut self) -> Vec<SegmentInfo> {
        match self {
            Sink::Continuous(w) => w.take_closed(),
            Sink::Triggered(r) => r.take_closed(),
        }
    }
}

/// Connect to an RTSP stream and record it direct-to-disk for `seconds`
/// (0 = until the stream ends). Video is required; AAC audio is recorded
/// when the camera offers it and `with_audio` is set.
pub async fn record_stream(
    src: &super::StreamSource,
    dir: &Path,
    prefix: &str,
    segment_seconds: u64,
    seconds: u64,
    with_audio: bool,
    mode: RecordMode,
) -> anyhow::Result<Vec<SegmentInfo>> {
    use anyhow::anyhow;

    let mut stream = open_stream(src, with_audio).await?;

    // The writer is created at the first keyframe, once the codec
    // parameters are known (some cameras only send them in-band).
    let mut writer: Option<Sink> = None;
    let mut closed = Vec::new();
    let started = tokio::time::Instant::now();
    let started_std = started.into_std();
    let mut triggered = false;
    // Latest sender report per kind (video, audio), kept for a writer that
    // does not exist yet, plus counts and first-seen times for the log.
    let mut latest_sr: [Option<(i64, f64)>; 2] = [None, None];
    let mut sr_counts = [0u32; 2];
    let mut sr_first: [Option<std::time::Instant>; 2] = [None, None];
    let deadline = (seconds > 0).then(|| tokio::time::Instant::now() + std::time::Duration::from_secs(seconds));
    loop {
        let next = match deadline {
            Some(d) => match tokio::time::timeout_at(d, stream.next()).await {
                Ok(n) => n,
                Err(_) => break,
            },
            None => stream.next().await,
        };
        let item = match next {
            None => break,
            Some(Err(e)) => {
                if let Some(w) = writer.as_mut() {
                    w.finish()?;
                    closed.extend(w.take_closed());
                }
                return Err(anyhow!("stream error: {e}"));
            }
            Some(Ok(i)) => i,
        };
        match item {
            MediaItem::Video { ticks, cts, data, key, new_params } => {
                if new_params && writer.is_some() {
                    // Resolution/codec change: close out and start fresh.
                    let mut w = writer.take().unwrap();
                    w.finish()?;
                    closed.extend(w.take_closed());
                }
                if writer.is_none() {
                    if !key {
                        continue;
                    }
                    let Some(tracks) = build_tracks(&stream)? else { continue };
                    let run = format!("{}-{}", prefix, chrono::Utc::now().timestamp_millis());
                    let timescale = tracks[0].timescale;
                    writer = Some(match mode {
                        RecordMode::Continuous => Sink::Continuous(SegmentWriter::new(dir, &run, tracks, segment_seconds)?),
                        RecordMode::Triggered { pre_roll, post_roll, .. } => Sink::Triggered(super::preroll::TriggeredRecorder::new(
                            dir.to_path_buf(), prefix, tracks, segment_seconds, pre_roll, post_roll, timescale,
                        )),
                    });
                    for (n, kind) in [TrackKind::Video, TrackKind::Audio].into_iter().enumerate() {
                        if let (Some((ticks, ntp)), Some(w)) = (latest_sr[n], writer.as_mut()) {
                            w.observe_sender_report(kind, ticks, ntp);
                        }
                    }
                }
                let w = writer.as_mut().unwrap();
                if let (RecordMode::Triggered { trigger_after, .. }, Sink::Triggered(r)) = (mode, &mut *w) {
                    if !triggered && started.elapsed() >= trigger_after {
                        triggered = true;
                        r.trigger()?;
                        tracing::info!("trigger fired at {:.1} s", started.elapsed().as_secs_f64());
                    }
                }
                w.push(super::preroll::Frame::Video { ticks, cts, data, is_key: key })?;
                closed.extend(w.take_closed());
            }
            MediaItem::Audio { ticks, data } => {
                if let Some(w) = writer.as_mut() {
                    w.push(super::preroll::Frame::Audio { ticks, data })?;
                }
            }
            MediaItem::SenderReport { track: n, ticks, epoch_secs: ntp } => {
                let kind = if n == 0 { TrackKind::Video } else { TrackKind::Audio };
                tracing::debug!("{kind:?} sender report: rtp {ticks} ntp {ntp:.3}");
                latest_sr[n] = Some((ticks, ntp));
                sr_counts[n] += 1;
                sr_first[n].get_or_insert_with(std::time::Instant::now);
                if let Some(w) = writer.as_mut() {
                    w.observe_sender_report(kind, ticks, ntp);
                }
            }
        }
    }
    if let Some(w) = writer.as_mut() {
        w.finish()?;
        closed.extend(w.take_closed());
    }
    tracing::info!(
        "sender reports received: video {}, audio {} (first video after {:?}, first audio after {:?})",
        sr_counts[0],
        sr_counts[1],
        sr_first[0].map(|t| t.duration_since(started_std)),
        sr_first[1].map(|t| t.duration_since(started_std)),
    );
    Ok(closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_warning_is_once_an_hour_per_camera() {
        assert_eq!(camera_key("porch-1794148581609-rec"), "porch-rec");
        assert_eq!(camera_key("porch-1794148616808-rec"), "porch-rec");
        assert_eq!(camera_key("garage_west-sub-1794154292528"), "garage_west-sub");
        let t0 = std::time::Instant::now();
        assert!(sr_warn_due("unit_a-1794148581609", t0));
        assert!(!sr_warn_due("unit_a-1794148999999", t0 + std::time::Duration::from_secs(60)));
        assert!(sr_warn_due("unit_b-1794148581609", t0 + std::time::Duration::from_secs(60)), "another camera");
        assert!(sr_warn_due("unit_a-1794149999999", t0 + std::time::Duration::from_secs(3601)));
    }

    fn video_track() -> TrackInfo {
        // Minimal stand-in sample entry; the writer never parses it.
        let mut entry = vec![0, 0, 0, 16];
        entry.extend_from_slice(b"avc1");
        entry.extend_from_slice(&[0; 8]);
        TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: entry, width: 640, height: 480 }
    }

    #[test]
    fn recordings_follow_this_computers_clock_when_the_camera_clock_drifts() {
        let dir = std::env::temp_dir().join(format!("amber-drift-test-{}", std::process::id()));
        let mut w = SegmentWriter::new(&dir, "cam-d", vec![video_track()], 10).unwrap();
        let start = std::time::Instant::now();
        // A camera clock 0.1% fast: 10.01 s of media per 10 s of real time,
        // for 30 minutes of 10 s segments. Uncorrected that is 1.8 s off.
        let mut media = 0.0;
        for _ in 0..180 {
            media += 10.01;
            let real = media / 1.001;
            w.anchor_instant = start.checked_sub(std::time::Duration::from_secs_f64(real)).unwrap_or(start);
            w.follow_clock(media, 10.01);
        }
        let real = media / 1.001;
        let off = (media + w.drift) - real;
        // Up to 500 ppm is followed: 0.1% is beyond that, so it is halved
        // (once past the first 2 minutes); never pushed past real time.
        assert!(off > 0.0 && off < 1.8 * 0.6, "still {off:.2} s off");
        // A clock that is right is left alone (jitter only).
        let mut w2 = SegmentWriter::new(&dir, "cam-e", vec![video_track()], 10).unwrap();
        let mut media = 0.0;
        for k in 0..180 {
            media += 10.0;
            let jitter = if k % 2 == 0 { 0.2 } else { -0.2 };
            w2.anchor_instant = start.checked_sub(std::time::Duration::from_secs_f64(media + jitter)).unwrap_or(start);
            w2.follow_clock(media, 10.0);
        }
        assert!(w2.drift.abs() < 0.01, "moved {:.3} s on jitter alone", w2.drift);
        // Not live input (a file, much faster than real time): left alone.
        let mut w3 = SegmentWriter::new(&dir, "cam-f", vec![video_track()], 10).unwrap();
        w3.follow_clock(600.0, 10.0);
        assert_eq!(w3.drift, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn footage_recorded_earlier_is_dated_by_when_it_was_recorded() {
        let dir = std::env::temp_dir().join(format!("amber-recorded-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A clip recorded 3 minutes ago, 4 s long at 10 fps, written now in a burst.
        let start = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() - 180.0;
        let mut w = SegmentWriter::new_recorded_at(&dir, "cam-sd", vec![video_track()], 60, start).unwrap();
        for i in 0..40i64 {
            w.push_video_cts(i * 9000, 0, vec![0, 0, 0, 1, 0x65], i == 0).unwrap();
        }
        w.finish().unwrap();
        let segs = w.take_closed();
        assert_eq!(segs.len(), 1);
        let want = (start * 1000.0).round() as i64;
        assert!((segs[0].start_ms - want).abs() <= 1, "start {} vs recorded {want}", segs[0].start_ms);
        assert!((segs[0].end_ms - (want + 4000)).abs() <= 150, "end {} vs {}", segs[0].end_ms, want + 4000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotates_on_keyframe_after_segment_length_and_drops_leading_non_key() {
        let dir = std::env::temp_dir().join(format!("amber-rec-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = SegmentWriter::new(&dir, "cam-1", vec![video_track()], 2).unwrap();
        // A non-keyframe before the first keyframe must be dropped.
        w.push_video(0, vec![9; 10], false).unwrap();
        // 10 fps, keyframe every 10 frames (1 s GOP), 5 s total.
        for i in 0..50i64 {
            w.push_video(9000 + i * 9000, vec![1; 100], i % 10 == 0).unwrap();
        }
        w.finish().unwrap();
        let segs = w.take_closed();
        // 2 s segments cut at 1 s GOP boundaries: 2 + 2 + 1 seconds.
        let frames: Vec<u64> = segs.iter().map(|s| s.video_frames).collect();
        assert_eq!(frames, vec![20, 20, 10]);
        let durs: Vec<i64> = segs.iter().map(|s| s.end_ms - s.start_ms).collect();
        assert_eq!(durs, vec![2000, 2000, 1000]);
        // Segments are contiguous.
        assert_eq!(segs[0].end_ms, segs[1].start_ms);
        assert_eq!(segs[1].end_ms, segs[2].start_ms);
        // One fragment per GOP.
        assert_eq!(segs[0].fragments, 2);
        for s in &segs {
            let bytes = std::fs::read(&s.path).unwrap();
            assert_eq!(bytes.len() as u64, s.bytes);
            assert_eq!(&bytes[4..8], b"ftyp");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn b_frame_recordings_use_decode_times_and_composition_offsets() {
        let dir = std::env::temp_dir().join(format!("amber-rec-bframes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = SegmentWriter::new(&dir, "cam-b", vec![video_track()], 2).unwrap();
        // 30 fps I P B B P B B ..., keyframe every 30 frames, 5 s; decode
        // times one frame behind presentation (as an FLV sender gives them).
        let mut pts = Vec::new();
        for gop in 0..5i64 {
            pts.push(gop * 30);
            for g in 0..29i64 {
                // P then two B: P shows 3 later; only whole triples fit.
                let b = gop * 30 + (g / 3) * 3;
                pts.push(match g % 3 {
                    0 => (b + 3).min(gop * 30 + 29),
                    1 => b + 1,
                    _ => b + 2,
                });
            }
        }
        for (n, p) in pts.iter().enumerate() {
            let dts = n as i64 * 3000;
            w.push_video_cts(dts, ((p + 1) * 3000 - dts) as i32, vec![n as u8; 20], n % 30 == 0).unwrap();
        }
        w.finish().unwrap();
        let segs = w.take_closed();
        assert_eq!(segs.iter().map(|s| s.video_frames).collect::<Vec<_>>(), vec![60, 60, 30]);
        assert_eq!(segs.iter().map(|s| s.end_ms - s.start_ms).collect::<Vec<_>>(), vec![2000, 2000, 1000]);
        let mut n = 0usize;
        for s in &segs {
            let bytes = std::fs::read(&s.path).unwrap();
            let mut i = 0;
            let mut next_dts = 0u64;
            while let Some(p) = bytes[i..].windows(4).position(|w| w == b"moof") {
                let at = i + p - 4;
                let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                let moof = &bytes[at..at + size];
                let t = moof.windows(4).position(|w| w == b"tfdt").unwrap();
                let tfdt = u64::from_be_bytes(moof[t + 8..t + 16].try_into().unwrap());
                assert_eq!(tfdt, next_dts, "fragments follow on in decode time");
                let (version, _, samples) = crate::ingest::fmp4::tests::parse_trun(moof);
                assert_eq!(version, 1);
                let file_first = (n / 60 * 60) as i64 * 3000;
                for (k, smp) in samples.iter().enumerate() {
                    assert_eq!(smp.0, 3000, "durations are decode-time steps");
                    let shown = tfdt as i64 + k as i64 * 3000 + smp.3 as i64;
                    assert_eq!(shown, (pts[n] + 1) * 3000 - file_first);
                    n += 1;
                }
                next_dts = tfdt + samples.len() as u64 * 3000;
                i = at + size;
            }
        }
        assert_eq!(n, pts.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn audio_track() -> TrackInfo {
        let mut entry = vec![0, 0, 0, 16];
        entry.extend_from_slice(b"mp4a");
        entry.extend_from_slice(&[0; 8]);
        TrackInfo { kind: TrackKind::Audio, timescale: 16000, sample_entry: entry, width: 0, height: 0 }
    }

    /// (track_id, base decode time) of every fragment in the file, in order.
    fn tfdt_boxes(bytes: &[u8]) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        let mut track_id = 0;
        for i in 4..bytes.len().saturating_sub(24) {
            if &bytes[i..i + 4] == b"tfhd" {
                track_id = u32::from_be_bytes(bytes[i + 8..i + 12].try_into().unwrap());
            } else if &bytes[i..i + 4] == b"tfdt" {
                out.push((track_id, u64::from_be_bytes(bytes[i + 8..i + 16].try_into().unwrap())));
            }
        }
        out
    }

    const NTP0: f64 = 1_700_000_000.0;

    /// 5 s of 10 fps video (1 s GOP, 2 s segments) plus 10 audio frames per
    /// second, which both start at the same local instant. With `sr_at`,
    /// sender reports say the audio is `AUDIO_LAG` later than the video; they
    /// are given before frame `sr_at`. Returns the segments and, per segment,
    /// the audio base decode time of its first audio fragment.
    const AUDIO_LAG: f64 = 0.120;

    fn run(name: &str, sr_at: Option<usize>, segment_seconds: u64, frames: usize) -> (Vec<SegmentInfo>, Vec<u64>) {
        let dir = std::env::temp_dir().join(format!("amber-sr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = SegmentWriter::new(&dir, "cam", vec![video_track(), audio_track()], segment_seconds).unwrap();
        for i in 0..frames {
            if sr_at == Some(i) {
                // Video tick 9000 and audio tick 1600 are the same instant
                // locally; the camera clock puts audio 120 ms later.
                w.observe_sender_report(TrackKind::Video, 9000, NTP0);
                w.observe_sender_report(TrackKind::Audio, 1600, NTP0 + AUDIO_LAG);
            }
            w.push_video(9000 + i as i64 * 9000, vec![1; 100], i % 10 == 0).unwrap();
            w.push_audio(1600 + i as i64 * 1600, vec![2; 20]);
        }
        w.finish().unwrap();
        let segs = w.take_closed();
        let bases = segs
            .iter()
            .map(|s| {
                let boxes = tfdt_boxes(&std::fs::read(&s.path).unwrap());
                boxes.iter().find(|b| b.0 == 2).expect("audio fragment").1
            })
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        (segs, bases)
    }

    #[test]
    fn sender_reports_before_first_file_align_audio_and_wall_clock() {
        let (_, plain) = run("plain-a", None, 2, 50);
        // Arrival anchoring puts both first frames at about the same instant
        // (up to 50 ms of scheduling delay: tests run in parallel, and a
        // loaded Windows machine was seen exceeding a tighter bound).
        assert!(plain[0] < 800, "arrival-anchored audio base {}", plain[0]);

        let (segs, sr) = run("sr-a", Some(0), 2, 50);
        // 120 ms at 16 kHz = 1920 ticks (give or take scheduling delay).
        assert!(sr[0].abs_diff(1920) < 800, "audio base {}", sr[0]);
        // Segment times follow this machine's clock (the synthetic camera
        // clock NTP0 is far away), and segments stay contiguous.
        let now_ms = std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
        assert!((segs[0].start_ms - now_ms).abs() < 60_000, "start {} vs now {now_ms}", segs[0].start_ms);
        assert_ne!(segs[0].start_ms, (NTP0 * 1000.0) as i64);
        assert_eq!(segs[0].end_ms, segs[1].start_ms);
        assert_eq!(segs[1].end_ms, segs[2].start_ms);
        assert_eq!(segs[2].end_ms - segs[0].start_ms, 5000);
    }

    #[test]
    fn first_fragment_waits_for_sender_reports() {
        // Reports at 2.5 s, 2 s segments: the first file (held until its
        // 2 s are complete) is written unaligned, the second one aligned
        // even though the report came after it opened.
        let (_, sr) = run("sr-c", Some(25), 2, 50);
        assert!(sr[0] < 100, "{}", sr[0]);
        assert!(sr[1].abs_diff(320) < 100, "{}", sr[1]);
        // Reports at 0.3 s with 10 s segments: held until the report, so
        // the first file is aligned although it opened before.
        let (_, sr) = run("sr-d", Some(3), 10, 60);
        assert!(sr[0].abs_diff(1920) < 100, "{}", sr[0]);
    }

    #[test]
    fn sender_report_mid_file_applies_from_next_file_only() {
        let (_, plain) = run("plain-b", None, 10, 130);
        let (segs, sr) = run("sr-b", Some(65), 10, 130);
        // First file was already written with arrival alignment.
        assert!(sr[0].abs_diff(plain[0]) < 100, "{} vs {}", sr[0], plain[0]);
        // The next file uses the report: its first audio frame started 100 ms
        // (1600 ticks) before the cut, so the 120 ms lag leaves it 320 ticks
        // into the file, where arrival anchoring clamps it to 0.
        assert!(plain[1] < 100, "{}", plain[1]);
        assert!(sr[1].abs_diff(320) < 100, "{}", sr[1]);
        assert_ne!(segs[0].start_ms, (NTP0 * 1000.0) as i64);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].end_ms - segs[0].start_ms, 13000);
    }

    #[test]
    fn timing_only_sps_change_is_not_material() {
        // Two parameter sets from a Reolink Duo Floodlight WiFi that differ
        // only in the SPS timing fields, with the same PPS.
        let hex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        let a = hex("01640033ffe1001567640033ac1514a060049a1000004f60000c67007f01000468ee3cb0");
        let b = hex("01640033ffe1001567640033ac1514a060049a1000004f60000c67004001000468ee3cb0");
        assert_eq!(avcc_pps(&a), hex("01000468ee3cb0"));
        let p = |x: &[u8]| VideoParams {
            codec: "avc1.640033".into(),
            h265: false,
            extra_data: x.to_vec(),
            sample_entry: vec![],
            width: 1536,
            height: 576,
            clock_rate: 90000,
        };
        assert_eq!(material_key(&p(&a)), material_key(&p(&b)));
        let mut c = p(&a);
        c.width = 1920;
        assert_ne!(material_key(&p(&a)), material_key(&c));
    }

    /// Configuration records of a 4K H.265 camera that re-sends its SPS at
    /// every keyframe with only the VUI timing changed (one byte differs).
    const HVCC_A: &str = "01016000000000000000000096f000fcfdf8f800000f03a00001001840010c01ffff01600000030000030000030000030096ac09a10001002a42010101600000030000030000030000030096a001e020021c7f8aad3ba24bb804000013d800018ce009a2000100094401c072f0941ef648";
    const HVCC_B: &str = "01016000000000000000000096f000fcfdf8f800000f03a00001001840010c01ffff01600000030000030000030000030096ac09a10001002a42010101600000030000030000030000030096a001e020021c7f8aad3ba24bb804000013d800018ce008a2000100094401c072f0941ef648";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn hevc(record: &[u8]) -> VideoParams {
        VideoParams {
            codec: "hvc1.1.6.L150.B0".into(),
            h265: true,
            extra_data: record.to_vec(),
            sample_entry: vec![],
            width: 3840,
            height: 2160,
            clock_rate: 90000,
        }
    }

    #[test]
    fn hevc_timing_only_sps_change_is_not_material() {
        let (a, b) = (hex(HVCC_A), hex(HVCC_B));
        assert_ne!(a, b);
        assert_eq!(material_key(&hevc(&a)), material_key(&hevc(&b)));
        // The VPS and PPS are still compared.
        let mut pps = a.clone();
        *pps.last_mut().unwrap() ^= 1;
        assert_ne!(material_key(&hevc(&a)), material_key(&hevc(&pps)));
        // So are the bit depth and the picture size.
        let mut depth = a.clone();
        depth[17] = 0xfa; // 10-bit luma
        assert_ne!(material_key(&hevc(&a)), material_key(&hevc(&depth)));
        let mut wide = hevc(&a);
        wide.width = 2560;
        assert_ne!(material_key(&hevc(&a)), material_key(&wide));
        // A record that cannot be read is compared whole.
        assert_eq!(hvcc_material(&a[..30]), a[..30].to_vec());
    }

    /// The same camera's stream as `OpenStream::next` sees it: keyframes
    /// every second, each with "new parameters" whose SPS differs only in its
    /// timing. Only the first keyframe may count as new parameters, or the
    /// main-stream recording and live view restart every second.
    #[test]
    fn hevc_resent_sps_does_not_restart_the_stream() {
        let mut last = None;
        let mut restarts = 0;
        for i in 0..200i64 {
            // 20 fps, 90 kHz, a keyframe every second.
            let key = i % 20 == 0;
            let record = hex(if (i / 20) % 2 == 0 { HVCC_A } else { HVCC_B });
            let item = MediaItem::Video { ticks: i * 4500, cts: 0, data: vec![0; 8], key, new_params: key };
            let item = settle_new_params(&mut last, Some(&hevc(&record)), item);
            if let MediaItem::Video { new_params: true, .. } = item {
                restarts += 1;
            }
        }
        assert_eq!(restarts, 1, "only the first keyframe starts the stream");
    }
}
