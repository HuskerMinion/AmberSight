//! Pre-roll buffer and triggered recording.
//!
//! The camera stream is always received. While nothing is recording, the
//! last few seconds of encoded frames are kept in memory, always starting at
//! a video keyframe so they can be written out on their own. When a trigger
//! arrives, the buffered frames are written first (with their original
//! timestamps), so the recording includes the moments before the trigger.

use super::fmp4::{TrackInfo, TrackKind};
use super::recorder::{SegmentInfo, SegmentWriter};
use std::collections::VecDeque;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub enum Frame {
    /// `ticks`: decode time; `cts`: presentation minus decode time (B-frames).
    Video { ticks: i64, cts: i32, data: Vec<u8>, is_key: bool },
    Audio { ticks: i64, data: Vec<u8> },
}

/// Holds at least `seconds` of recent frames, starting at a video keyframe.
pub struct PreRollBuffer {
    seconds: f64,
    video_timescale: f64,
    frames: VecDeque<Frame>,
    bytes: usize,
}

impl PreRollBuffer {
    pub fn new(seconds: u64, video_timescale: u32) -> Self {
        PreRollBuffer { seconds: seconds as f64, video_timescale: video_timescale.max(1) as f64, frames: VecDeque::new(), bytes: 0 }
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn push(&mut self, f: Frame) {
        let (is_key, ticks) = match &f {
            Frame::Video { is_key, ticks, .. } => (*is_key, Some(*ticks)),
            Frame::Audio { .. } => (false, None),
        };
        if self.frames.is_empty() && !is_key {
            return; // the buffer must start at a keyframe
        }
        self.bytes += frame_len(&f);
        self.frames.push_back(f);
        if let Some(now) = ticks {
            self.trim(now);
        }
    }

    /// Drop whole GOPs from the front while the remainder, starting at the
    /// next keyframe, still covers `seconds` before `now_ticks`.
    pub fn trim(&mut self, now_ticks: i64) {
        loop {
            // Position and timestamp of the second keyframe in the buffer.
            let next_key = self.frames.iter().enumerate().skip(1).find_map(|(i, f)| match f {
                Frame::Video { is_key: true, ticks, .. } => Some((i, *ticks)),
                _ => None,
            });
            let Some((i, t)) = next_key else { return };
            if (now_ticks - t) as f64 / self.video_timescale < self.seconds {
                return;
            }
            for f in self.frames.drain(..i) {
                self.bytes -= frame_len(&f);
            }
        }
    }

    /// Timestamp of the newest audio frame held.
    fn last_audio_ticks(&self) -> Option<i64> {
        self.frames.iter().rev().find_map(|f| match f {
            Frame::Audio { ticks, .. } => Some(*ticks),
            _ => None,
        })
    }

    fn drain(&mut self) -> Vec<Frame> {
        self.bytes = 0;
        self.frames.drain(..).collect()
    }
}

fn frame_len(f: &Frame) -> usize {
    match f {
        Frame::Video { data, .. } | Frame::Audio { data, .. } => data.len(),
    }
}

/// Records only while triggered, with pre-roll from the buffer and a
/// post-roll after the last trigger.
pub struct TriggeredRecorder {
    dir: PathBuf,
    prefix: String,
    tracks: Vec<TrackInfo>,
    segment_seconds: u64,
    post_roll_secs: f64,
    video_timescale: f64,
    buffer: PreRollBuffer,
    writer: Option<SegmentWriter>,
    /// Video timestamp (ticks) until which recording continues.
    record_until: Option<i64>,
    last_video_ticks: Option<i64>,
    prev_video_ticks: Option<i64>,
    closed: Vec<SegmentInfo>,
    /// Latest sender report per track kind, handed to each new writer so it
    /// aligns audio and video from its first file.
    sender_reports: Vec<(TrackKind, i64, f64)>,
}

impl TriggeredRecorder {
    pub fn new(
        dir: PathBuf,
        prefix: &str,
        tracks: Vec<TrackInfo>,
        segment_seconds: u64,
        pre_roll_secs: u64,
        post_roll_secs: u64,
        video_timescale: u32,
    ) -> Self {
        TriggeredRecorder {
            dir,
            prefix: prefix.to_string(),
            tracks,
            segment_seconds,
            post_roll_secs: post_roll_secs as f64,
            video_timescale: video_timescale.max(1) as f64,
            buffer: PreRollBuffer::new(pre_roll_secs, video_timescale),
            writer: None,
            record_until: None,
            last_video_ticks: None,
            prev_video_ticks: None,
            closed: Vec::new(),
            sender_reports: Vec::new(),
        }
    }

    /// Record an RTCP sender report (see
    /// [`SegmentWriter::observe_sender_report`]); applied to the running
    /// writer, and kept for the next one. Feed every report, also while
    /// idle, so the pre-roll is aligned too.
    pub fn observe_sender_report(&mut self, kind: TrackKind, rtp_ticks: i64, ntp_epoch_secs: f64) {
        self.sender_reports.retain(|r| r.0 != kind);
        self.sender_reports.push((kind, rtp_ticks, ntp_epoch_secs));
        if let Some(w) = self.writer.as_mut() {
            w.observe_sender_report(kind, rtp_ticks, ntp_epoch_secs);
        }
    }

    #[cfg(test)]
    pub fn is_recording(&self) -> bool {
        self.writer.is_some()
    }

    /// Start (or extend) a recording: it runs until `post_roll` seconds after
    /// the most recent trigger. Call repeatedly while motion continues.
    pub fn trigger(&mut self) -> std::io::Result<()> {
        let Some(now) = self.last_video_ticks else { return Ok(()) };
        self.record_until = Some(now + (self.post_roll_secs * self.video_timescale) as i64);
        if self.writer.is_none() {
            // Measure the pre-roll back from the trigger moment (about one
            // frame after the last frame received), not from that frame.
            let frame = self.prev_video_ticks.map(|p| (now - p).max(0)).unwrap_or(0);
            self.buffer.trim(now + frame);
            // `{camera}-{trigger_ms}-rec-seg000000.mp4`, the motion-only naming the
            // clip builder and storage code recognize.
            let run = format!("{}-{}-rec", self.prefix, chrono::Utc::now().timestamp_millis());
            let mut w = SegmentWriter::new(&self.dir, &run, self.tracks.clone(), self.segment_seconds)?;
            // The newest buffered frames arrived just now: they set the clock.
            w.anchor_latest(TrackKind::Video, now);
            if let Some(a) = self.buffer.last_audio_ticks() {
                w.anchor_latest(TrackKind::Audio, a);
            }
            for &(kind, ticks, ntp) in &self.sender_reports {
                w.observe_sender_report(kind, ticks, ntp);
            }
            for f in self.buffer.drain() {
                feed(&mut w, f)?;
            }
            self.writer = Some(w);
        }
        Ok(())
    }

    pub fn push(&mut self, f: Frame) -> std::io::Result<()> {
        if let Frame::Video { ticks, is_key, .. } = &f {
            self.prev_video_ticks = self.last_video_ticks;
            self.last_video_ticks = Some(*ticks);
            // Stop at a keyframe once the post-roll is over, so the next
            // pre-roll buffer starts cleanly at that keyframe.
            if *is_key && self.record_until.is_some_and(|u| *ticks >= u) {
                self.stop()?;
            }
        }
        match self.writer.as_mut() {
            Some(w) => {
                feed(w, f)?;
                self.closed.extend(w.take_closed());
            }
            None => self.buffer.push(f),
        }
        Ok(())
    }

    /// Finish any recording in progress (camera stopping).
    pub fn stop(&mut self) -> std::io::Result<()> {
        if let Some(mut w) = self.writer.take() {
            w.finish()?;
            self.closed.extend(w.take_closed());
        }
        self.record_until = None;
        Ok(())
    }

    pub fn take_closed(&mut self) -> Vec<SegmentInfo> {
        std::mem::take(&mut self.closed)
    }
}

fn feed(w: &mut SegmentWriter, f: Frame) -> std::io::Result<()> {
    match f {
        Frame::Video { ticks, cts, data, is_key } => w.push_video_cts(ticks, cts, data, is_key),
        Frame::Audio { ticks, data } => {
            w.push_audio(ticks, data);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::fmp4::TrackKind;

    const TS: i64 = 90000;

    fn video_track() -> TrackInfo {
        let mut entry = vec![0, 0, 0, 16];
        entry.extend_from_slice(b"avc1");
        entry.extend_from_slice(&[0; 8]);
        TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: entry, width: 64, height: 48 }
    }

    /// 10 fps, keyframe every 10 frames (1 s GOP).
    fn vframe(i: i64) -> Frame {
        Frame::Video { ticks: i * TS / 10, cts: 0, data: vec![i as u8; 50], is_key: i % 10 == 0 }
    }

    fn first_ticks(b: &PreRollBuffer) -> i64 {
        match b.frames.front().unwrap() {
            Frame::Video { ticks, is_key, .. } => {
                assert!(*is_key, "buffer must start at a keyframe");
                *ticks
            }
            _ => panic!("buffer must start with video"),
        }
    }

    #[test]
    fn buffer_keeps_at_least_preroll_and_starts_at_keyframe() {
        let mut b = PreRollBuffer::new(3, 90000);
        b.push(Frame::Audio { ticks: 0, data: vec![1] }); // ignored: no keyframe yet
        assert_eq!(b.len(), 0);
        for i in 0..100 {
            b.push(vframe(i));
            let now = i * TS / 10;
            let start = first_ticks(&b);
            if i >= 30 {
                // Covers >= 3 s, but never more than 3 s + one GOP.
                assert!(now - start >= 3 * TS, "frame {i}: only {} s", (now - start) as f64 / TS as f64);
                assert!(now - start < 4 * TS, "frame {i}: kept too much");
            }
        }
        assert_eq!(b.bytes(), b.len() * 50);
    }

    #[test]
    fn triggered_recording_includes_preroll_and_post_roll() {
        let dir = std::env::temp_dir().join(format!("amber-preroll-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // pre-roll 3 s, post-roll 2 s, long segments so there is one file.
        let mut r = TriggeredRecorder::new(dir.clone(), "cam", vec![video_track()], 60, 3, 2, 90000);
        let mut trigger_ms = 0;
        for i in 0..200 {
            if i == 100 {
                trigger_ms = chrono::Utc::now().timestamp_millis();
                r.trigger().unwrap(); // trigger at t = 10 s
            }
            r.push(vframe(i)).unwrap();
        }
        r.stop().unwrap();
        let segs = r.take_closed();
        assert_eq!(segs.len(), 1);
        let s = &segs[0];
        // Starts at the keyframe 3 s before the trigger (t = 7 s), ends at
        // the first keyframe at/after trigger + 2 s (t = 12 s).
        assert_eq!(s.end_ms - s.start_ms, 5000);
        assert_eq!(s.video_frames, 50);
        // Dated before the trigger by the pre-roll (the newest buffered frame,
        // t = 9.9 s, arrived at the trigger), not starting at the trigger.
        let before = trigger_ms - s.start_ms;
        assert!((2800..=3100).contains(&before), "starts {before} ms before the trigger");
        assert!(!r.is_recording());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retrigger_extends_recording() {
        let dir = std::env::temp_dir().join(format!("amber-retrigger-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut r = TriggeredRecorder::new(dir.clone(), "cam", vec![video_track()], 60, 1, 2, 90000);
        for i in 0..200 {
            if i == 50 || i == 70 {
                r.trigger().unwrap(); // second trigger at 7 s keeps it going to 9 s
            }
            r.push(vframe(i)).unwrap();
        }
        r.stop().unwrap();
        let segs = r.take_closed();
        assert_eq!(segs.len(), 1);
        // 4 s (pre-roll from 4 s) .. 9 s
        assert_eq!(segs[0].end_ms - segs[0].start_ms, 5000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
