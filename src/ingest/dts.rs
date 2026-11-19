//! Decode times for video that only carries presentation times.
//!
//! RTP timestamps (RTSP cameras) are presentation times, and frames arrive in
//! decode order. With B-frames the presentation times go back and forth
//! (I0 P3 B1 B2 P6 B4 B5 ...), but MP4 needs a decode time that only ever
//! increases, with each sample's presentation time given as an offset from
//! it. This derives one without parsing the bitstream or holding frames back.
//!
//! Most cameras have no B-frames, and some send uneven or repeated
//! timestamps. So by default timestamps pass through unchanged (decode =
//! presentation time, exactly as before), and only repeated evidence of
//! reordering switches derivation on: several backward steps within a short
//! window, each a plausible reorder (not on a keyframe, at least an eighth
//! of a frame, at most 16 frames). After a long run without any, it switches
//! off again.
//!
//! While on: the decode times of a stream are its sorted presentation times,
//! shifted by the reorder depth `d` (how many frames a picture can be decoded
//! ahead of being shown). Frame `n`'s decode time is the `(n-d)`-th smallest
//! presentation time, and with depth `d` that value is always among the
//! frames received so far: so keep the presentation times not yet used, and
//! take the smallest whenever more than `d` are held. Decode times stay
//! strictly increasing, including across switching on and off; while the
//! depth settles (a few frames) they step by one tick.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

/// H.264/H.265 allow at most 16 frames of reordering.
const MAX_DEPTH: usize = 16;
/// Recent presentation times kept to judge steps and the depth.
const HISTORY: usize = 48;
/// Backward steps needed within `EVIDENCE_WINDOW` frames to switch on.
/// B-frame streams step back in every group of B-frames (every 2-4 frames).
const EVIDENCE_STEPS: usize = 3;
const EVIDENCE_WINDOW: u64 = 90;
/// Frames without a backward step after which derivation switches off.
const DECAY_FRAMES: u64 = 300;

pub struct DtsFromPts {
    /// Derivation on (the stream has B-frames).
    on: bool,
    /// The codec configuration says pictures are never reordered.
    never: bool,
    /// Presentation times not yet used as a decode time (min-heap).
    held: BinaryHeap<Reverse<i64>>,
    depth: usize,
    /// The last decode time handed out.
    last: Option<i64>,
    /// The last presentation time taken from `held` as a decode time.
    popped: Option<i64>,
    /// Recent presentation times, in decode order.
    recent: VecDeque<i64>,
    /// Frame numbers of recent plausible backward steps.
    steps: VecDeque<u64>,
    frame: u64,
    /// A presentation time this far before the previous one is a timestamp
    /// reset (the camera restarted its clock), not reordering.
    jump_back: i64,
}

impl DtsFromPts {
    /// `clock_rate`: ticks per second of the timestamps.
    pub fn new(clock_rate: u32) -> Self {
        DtsFromPts {
            on: false,
            never: false,
            held: BinaryHeap::new(),
            depth: 0,
            last: None,
            popped: None,
            recent: VecDeque::new(),
            steps: VecDeque::new(),
            frame: 0,
            jump_back: i64::from(clock_rate.max(1)),
        }
    }

    /// The codec configuration rules reordering out (or no longer does):
    /// timestamps then always pass through unchanged.
    pub fn set_never_reorders(&mut self, never: bool) {
        if never && !self.never {
            self.switch_off();
        }
        self.never = never;
    }

    fn switch_off(&mut self) {
        self.on = false;
        self.held.clear();
        self.depth = 0;
        self.popped = None;
    }

    /// Median of the recent forward steps: about one frame interval without
    /// B-frames, a few with.
    fn typical_step(&self) -> Option<i64> {
        let mut fwd: Vec<i64> = self.recent.iter().zip(self.recent.iter().skip(1)).map(|(a, b)| b - a).filter(|d| *d > 0).collect();
        if fwd.is_empty() {
            return None;
        }
        fwd.sort_unstable();
        Some(fwd[fwd.len() / 2])
    }

    /// How many earlier frames (of the recent ones) show after one shown
    /// before them, at most: the reorder depth they need.
    fn depth_seen(&self) -> usize {
        let r: Vec<i64> = self.recent.iter().copied().collect();
        (0..r.len()).map(|i| r[..i].iter().filter(|&&p| p > r[i]).count()).max().unwrap_or(0).clamp(1, MAX_DEPTH)
    }

    /// The decode time of the next frame (in decode order) shown at `pts`,
    /// and its composition offset (`pts - dts`).
    pub fn push(&mut self, pts: i64, key: bool) -> (i64, i32) {
        self.frame += 1;
        let prev = self.recent.back().copied();
        if prev.is_some_and(|p| pts < p - self.jump_back) {
            // Timestamps restarted: start over (passing them on as before).
            self.switch_off();
            self.recent.clear();
            self.steps.clear();
            self.last = None;
        }
        if let Some(p) = self.recent.back().copied() {
            let back = p - pts;
            let plausible = !key && back > 0 && self.typical_step().is_some_and(|s| back * 8 >= s && back <= 16 * s);
            if plausible {
                self.steps.push_back(self.frame);
            }
        }
        while self.steps.front().is_some_and(|&f| f + EVIDENCE_WINDOW <= self.frame) {
            self.steps.pop_front();
        }
        self.recent.push_back(pts);
        if self.recent.len() > HISTORY {
            self.recent.pop_front();
        }
        if self.on {
            let quiet = self.steps.back().is_none_or(|&f| self.frame - f >= DECAY_FRAMES);
            // Off again once nothing has been reordered for a long time; only
            // where passing `pts` on keeps decode times increasing.
            if quiet && self.last.is_none_or(|l| pts > l) {
                self.switch_off();
            }
        } else if !self.never && self.steps.len() >= EVIDENCE_STEPS {
            self.on = true;
            self.depth = self.depth_seen();
            self.held.clear();
            self.popped = None;
            // Strictly after everything handed out so far.
            self.last = self.recent.iter().copied().max().max(self.last);
        }
        if !self.on {
            self.last = Some(pts);
            return (pts, 0);
        }
        if self.popped.is_some_and(|p| pts < p) && self.depth < MAX_DEPTH && !key {
            // Shown before a time already used as a decode time: reordering
            // is deeper than assumed.
            self.depth += 1;
        }
        self.held.push(Reverse(pts));
        let next = if self.held.len() > self.depth { self.held.pop().map(|r| r.0) } else { None };
        if next.is_some() {
            self.popped = next;
        }
        let dts = match (self.last, next) {
            (None, n) => n.unwrap_or(pts),
            (Some(l), Some(n)) => n.max(l + 1),
            (Some(l), None) => l + 1,
        };
        self.last = Some(dts);
        (dts, (pts - dts).clamp(i32::MIN as i64, i32::MAX as i64) as i32)
    }

    /// Frames of reordering assumed (0: none, timestamps pass through).
    #[cfg(test)]
    pub fn depth(&self) -> usize {
        if self.on {
            self.depth
        } else {
            0
        }
    }
}

/// True when an H.264 decoder configuration record (avcC) says the stream
/// cannot have B-frames: Baseline profile (66), which has no B slices.
/// (Constrained Baseline is profile 66 too.) Other profiles may or may not
/// reorder; their VUI is not read.
pub fn avcc_never_reorders(avcc: &[u8]) -> bool {
    avcc.first() == Some(&1) && avcc.get(1) == Some(&66)
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: i64 = 3000;

    /// (dts, cts) for presentation times in frame units, keyframe every 30.
    fn run(d: &mut DtsFromPts, base: i64, pts: &[i64]) -> Vec<(i64, i32)> {
        pts.iter().enumerate().map(|(n, p)| d.push(base + p * F, n % 30 == 0)).collect()
    }

    fn assert_increasing(out: &[(i64, i32)]) {
        for w in out.windows(2) {
            assert!(w[1].0 > w[0].0, "decode times must increase: {out:?}");
        }
    }

    #[test]
    fn without_b_frames_decode_time_is_presentation_time() {
        let mut d = DtsFromPts::new(90_000);
        for i in 0..100 {
            assert_eq!(d.push(1234 + i * F, i % 30 == 0), (1234 + i * F, 0));
        }
        assert_eq!(d.depth(), 0);
    }

    #[test]
    fn jittery_and_repeated_timestamps_pass_through_unchanged() {
        let mut d = DtsFromPts::new(90_000);
        let mut pts: Vec<i64> = (0..2000).map(|i| 50_000 + i * F).collect();
        // Uneven RTP timing: one or two ticks back now and then, a repeated
        // timestamp, and one frame half a frame early.
        for i in (40..2000).step_by(37) {
            pts[i] = pts[i - 1] - 1 - (i as i64 % 2);
        }
        for i in (55..2000).step_by(101) {
            pts[i] = pts[i - 1];
        }
        pts[500] = pts[499] - F / 2;
        pts[1500] = pts[1499] - F / 2;
        for (n, p) in pts.iter().enumerate() {
            assert_eq!(d.push(*p, n % 30 == 0), (*p, 0), "frame {n}");
        }
        assert_eq!(d.depth(), 0);
        // A keyframe stepping back is not reordering either.
        let mut d = DtsFromPts::new(90_000);
        for i in 0..300i64 {
            let p = if i % 30 == 0 { i * F - 2 * F } else { i * F };
            assert_eq!(d.push(p, i % 30 == 0), (p, 0));
        }
    }

    #[test]
    fn b_frames_get_increasing_decode_times_and_offsets() {
        // I P B B P B B ..., decode order; presentation in frame units.
        let mut pts = vec![0i64];
        for g in 0..60 {
            pts.extend([g * 3 + 3, g * 3 + 1, g * 3 + 2]);
        }
        let mut d = DtsFromPts::new(90_000);
        let out = run(&mut d, 0, &pts);
        // Detected after three groups; from then on decode times increase,
        // one frame apart once settled, never after the picture is shown.
        let from = 3 * 3 + 1;
        assert_increasing(&out[from - 1..]);
        for (p, (dts, cts)) in pts.iter().zip(&out) {
            assert_eq!(dts + *cts as i64, p * F, "offset gives back the presentation time");
        }
        for w in out[from + 6..].windows(2) {
            assert_eq!(w[1].0 - w[0].0, F);
        }
        assert!(out[from + 6..].iter().all(|(_, cts)| *cts >= 0));
        assert_eq!(d.depth(), 1);
    }

    #[test]
    fn x264_b_pyramid_order_settles_after_a_few_frames() {
        // Presentation order of libx264 -bf 3 (B-pyramid), as decoded, in frames.
        let pts = [
            0, 3, 1, 2, 7, 5, 4, 6, 10, 8, 9, 12, 11, 16, 14, 13, 15, 20, 18, 17, 19, 24, 22, 21, 23, 28, 26, 25, 27, 29, 30, 34, 32,
            31, 33, 38, 36, 35, 37, 42, 40, 39, 41, 46, 44, 43, 45, 47, 51, 49, 48, 50, 52, 53, 54, 55, 56, 59, 57, 58,
        ];
        let mut d = DtsFromPts::new(90_000);
        let out = run(&mut d, 44, &pts);
        assert_eq!(d.depth(), 2);
        // On from frame 6 (the third backward step).
        assert_increasing(&out[5..]);
        for w in out[14..].windows(2) {
            assert_eq!(w[1].0 - w[0].0, F, "one frame per step once settled: {out:?}");
        }
        assert!(out[14..].iter().all(|(_, cts)| *cts >= 0));
        for (p, (dts, cts)) in pts.iter().zip(&out) {
            assert_eq!(dts + *cts as i64, 44 + p * F);
        }
    }

    #[test]
    fn switches_off_after_a_long_run_without_reordering() {
        let mut d = DtsFromPts::new(90_000);
        let mut pts = vec![0i64];
        for g in 0..20 {
            pts.extend([g * 3 + 3, g * 3 + 1, g * 3 + 2]);
        }
        let last = *pts.iter().max().unwrap();
        // The encoder stops using B-frames.
        pts.extend((1..=400).map(|i| last + i));
        let out = run(&mut d, 0, &pts);
        assert_increasing(&out[9..]);
        assert_eq!(d.depth(), 0);
        let tail = &out[out.len() - 50..];
        assert!(tail.iter().all(|(_, cts)| *cts == 0), "decode = presentation time again");
        // And back on when B-frames return.
        let mut more = Vec::new();
        let b = last + 400;
        for g in 0..10 {
            more.extend([b + g * 3 + 3, b + g * 3 + 1, b + g * 3 + 2]);
        }
        let out2: Vec<(i64, i32)> = more.iter().map(|p| d.push(p * F, false)).collect();
        assert_eq!(d.depth(), 1);
        assert_increasing(&out2[6..]);
    }

    #[test]
    fn a_timestamp_reset_starts_over() {
        let mut d = DtsFromPts::new(90_000);
        let mut pts = vec![0i64];
        for g in 0..10 {
            pts.extend([g * 3 + 3, g * 3 + 1, g * 3 + 2]);
        }
        run(&mut d, 1_000_000, &pts);
        assert_eq!(d.depth(), 1);
        assert_eq!(d.push(500, true), (500, 0));
        assert_eq!(d.push(3500, false), (3500, 0));
        assert_eq!(d.depth(), 0);
    }

    #[test]
    fn the_codec_configuration_can_rule_reordering_out() {
        assert!(avcc_never_reorders(&[1, 66, 0xc0, 30, 0xff]));
        assert!(!avcc_never_reorders(&[1, 100, 0, 51, 0xff]));
        assert!(!avcc_never_reorders(&[]));
        let mut d = DtsFromPts::new(90_000);
        d.set_never_reorders(true);
        let mut pts = vec![0i64];
        for g in 0..10 {
            pts.extend([g * 3 + 3, g * 3 + 1, g * 3 + 2]);
        }
        for (n, p) in pts.iter().enumerate() {
            assert_eq!(d.push(p * F, n == 0), (p * F, 0));
        }
    }
}

// ---------------------------------------------------------------------------
// Smoothing jittery RTSP video timestamps
// ---------------------------------------------------------------------------
//
// Some cameras' RTP video timestamps jitter a lot: a Reolink sub stream at
// about 9 fps stepped anywhere from 2391 to 23371 ticks (26 to 260 ms) from
// one frame to the next, straight off the camera (ffprobe shows the same).
// Played or decoded as they are, the frames bunch up and spread out, and
// ffmpeg reports "non monotonically increasing dts" where steps round to the
// same value. The camera's frames are really evenly spaced; only their
// stamps are not.
//
// `Smooth` keeps an even line through the camera's stamps: it moves on by
// the nominal frame step (the camera's average step over the last few
// seconds) and is pulled slowly toward the stamps. Each frame gets the time
// on that line, but never more than MAX_OFF (50 ms) from the camera's own
// stamp, so sound (whose timestamps are untouched) stays in step; within
// that, frames stay at least MIN_STEP_FRACTION of a frame apart, so no two
// land on the same moment. (Measured on the lab Reolink: its stamps stray
// from an even line by -68 to +83 ms, 1st to 99th percentile, so a few
// frames a minute sit at the 50 ms limit.) A jump of more than SNAP (500 ms:
// a gap, a reset, a reconnect) goes straight to the camera's time. It works
// only while the measured jitter is large (a steady camera's times pass
// through unchanged), and never on a stream with reordered frames
// (B-frames: its decode times are derived, see `DtsFromPts`).

/// At most this far from the camera's own time (90 kHz ticks: 50 ms).
const MAX_OFF: i64 = 4_500;
/// A jump this large is followed at once (500 ms).
const SNAP: i64 = 45_000;
/// Recent stamps kept for the nominal step and the jitter measure.
const STAMPS: usize = 64;
/// Stamps needed before smoothing can start.
const MIN_STAMPS: usize = 9;
/// Frames are kept at least this part of a frame apart (when the 50 ms
/// limit allows).
const MIN_STEP_FRACTION: f64 = 0.6;
/// How fast the line follows the stamps (a part of each frame's difference).
const FOLLOW: f64 = 1.0 / 16.0;

#[derive(Default)]
pub struct Smooth {
    last_in: Option<i64>,
    last_out: i64,
    stamps: VecDeque<i64>,
    /// The even line's time for the next frame.
    line: Option<f64>,
    /// Smoothing in use (the jitter is large).
    active: bool,
}

impl Smooth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the history (the next frame passes through).
    pub fn reset(&mut self) {
        self.last_in = None;
        self.stamps.clear();
        self.line = None;
        self.active = false;
    }

    /// The nominal step (the average over the kept stamps) and whether the
    /// steps jitter a lot: the middle 80% of them spread over more than half
    /// a frame.
    fn measure(&self) -> Option<(f64, bool)> {
        let n = self.stamps.len();
        if n < MIN_STAMPS {
            return None;
        }
        let nominal = (self.stamps[n - 1] - self.stamps[0]) as f64 / (n - 1) as f64;
        let mut steps: Vec<i64> = self.stamps.iter().zip(self.stamps.iter().skip(1)).map(|(a, b)| b - a).collect();
        steps.sort_unstable();
        let m = steps.len();
        let spread = (steps[m * 9 / 10] - steps[m / 10]) as f64;
        Some((nominal, nominal > 0.0 && spread * 2.0 > nominal))
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The output time of a frame the camera stamped `t` (in decode order,
    /// no reordering).
    pub fn push(&mut self, t: i64) -> i64 {
        let Some(prev) = self.last_in else {
            self.last_in = Some(t);
            self.last_out = t;
            self.stamps.push_back(t);
            return t;
        };
        let step = t - prev;
        self.last_in = Some(t);
        if step < 0 || step > SNAP || (t - self.last_out).abs() > SNAP {
            // A gap, a reset or a reconnect: the camera's time, history gone.
            self.stamps.clear();
            self.stamps.push_back(t);
            self.line = None;
            self.active = false;
            self.last_out = t;
            return t;
        }
        self.stamps.push_back(t);
        if self.stamps.len() > STAMPS {
            self.stamps.pop_front();
        }
        let measured = self.measure();
        self.active = measured.is_some_and(|(_, jittery)| jittery);
        let out = match measured {
            Some((nominal, true)) => {
                // The line moves on one frame and follows the stamps slowly.
                let line = self.line.map_or(t as f64, |l| l + nominal);
                let line = line + (t as f64 - line) * FOLLOW;
                self.line = Some(line);
                let hi = t + MAX_OFF;
                // At least part of a frame after the last one, if the limit allows.
                let lo = (t - MAX_OFF).max(self.last_out + (nominal * MIN_STEP_FRACTION) as i64).min(hi);
                (line.round() as i64).clamp(lo, hi)
            }
            _ => {
                self.line = None;
                t
            }
        };
        // Always increasing (a camera's repeated stamp included).
        let out = out.max(self.last_out + 1);
        self.last_out = out;
        out
    }
}

/// Decode times for RTSP video: `DtsFromPts` for reordering, then `Smooth`
/// for jitter on streams without reordering. The one place RTSP video times
/// are set.
pub struct RtspVideoTimes {
    dts: DtsFromPts,
    smooth: Smooth,
    /// The last decode time handed out by the smoothing (it can run up to
    /// MAX_OFF ahead of the camera's time).
    last_smoothed: Option<i64>,
    /// Derived decode times are kept after this (the smoothing's last one)
    /// right after reordering is found, until they are past it.
    floor: Option<i64>,
}

impl RtspVideoTimes {
    pub fn new(clock_rate: u32) -> Self {
        RtspVideoTimes { dts: DtsFromPts::new(clock_rate), smooth: Smooth::new(), last_smoothed: None, floor: None }
    }

    pub fn set_never_reorders(&mut self, never: bool) {
        self.dts.set_never_reorders(never);
    }

    /// Smoothing is in use now (for the log).
    pub fn smoothing(&self) -> bool {
        self.smooth.is_active()
    }

    /// (decode time, composition offset) of the next frame, shown at `pts`.
    pub fn push(&mut self, pts: i64, key: bool) -> (i64, i32) {
        let (dts, cts) = self.dts.push(pts, key);
        if self.dts.on || cts != 0 {
            // Reordered frames: their times are derived; never smoothed.
            self.smooth.reset();
            // Found only after smoothing ran (ahead of the camera's time, by
            // up to MAX_OFF): the derived times start after its last one.
            if let Some(last) = self.last_smoothed.take() {
                self.floor = Some(last);
            }
            if let Some(f) = self.floor {
                // (Not across a timestamp reset: far below is the camera's new clock.)
                if dts <= f && f - dts <= 2 * MAX_OFF {
                    let out = f + 1;
                    self.floor = Some(out);
                    let pts = dts + i64::from(cts);
                    return (out, (pts - out).clamp(i32::MIN as i64, i32::MAX as i64) as i32);
                }
                self.floor = None;
            }
            return (dts, cts);
        }
        self.floor = None;
        let out = self.smooth.push(dts);
        self.last_smoothed = Some(out);
        (out, 0)
    }
}

#[cfg(test)]
mod smooth_tests {
    use super::*;

    /// A camera at 9 fps (10 000 ticks a frame) whose stamps jitter between
    /// 2391 and 23371 ticks a step, as measured, around the true times.
    fn jittery(n: usize) -> Vec<i64> {
        let mut out = Vec::new();
        let mut seed: u64 = 7;
        let mut t = 1_000_000i64;
        for i in 0..n {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            // The stamp is the true time plus up to +-42 ms of noise, so the
            // steps span about 2400..17600 (never backwards, as on the camera).
            let _ = i;
            let noise = ((seed >> 33) % 7_601) as i64 - 3_800;
            let stamp = t + noise;
            out.push(stamp);
            t += 10_000;
        }
        out
    }

    #[test]
    fn jittery_stamps_come_out_even_and_close_to_the_camera() {
        let ins = jittery(600);
        let steps_in: Vec<i64> = ins.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(*steps_in.iter().min().unwrap() < 4_000 && *steps_in.iter().max().unwrap() > 16_000, "the test input jitters like the camera");
        let mut s = Smooth::new();
        let outs: Vec<i64> = ins.iter().map(|&t| s.push(t)).collect();
        assert!(s.is_active());
        for (o, i) in outs.iter().zip(&ins) {
            assert!((o - i).abs() <= MAX_OFF, "within 50 ms of the camera: {o} vs {i}");
        }
        assert!(outs.windows(2).all(|w| w[1] > w[0]), "always increasing");
        // After it settles, steps stay near one frame (10 000 ticks).
        let steps: Vec<i64> = outs[40..].windows(2).map(|w| w[1] - w[0]).collect();
        let (lo, hi) = (*steps.iter().min().unwrap(), *steps.iter().max().unwrap());
        assert!(lo >= 6_000 && hi <= 14_500, "even steps: {lo}..{hi}");
    }

    #[test]
    fn stamps_straying_like_the_lab_camera_keep_frames_apart() {
        // Even 9.6 fps frames whose stamps stray up to -68..+83 ms (as
        // measured on the lab Reolink), with a big swing now and then.
        let mut s = Smooth::new();
        let mut seed: u64 = 11;
        let (mut outs, mut ins) = (Vec::new(), Vec::new());
        let mut prev_stray = 0i64;
        for k in 0..3_000i64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let r = ((seed >> 33) % 1000) as i64;
            let stray = if r < 20 { 7_400 } else if r < 40 { -6_100 } else { (r - 500) * 6 };
            // (Never a stamp before the one before it: the camera's
            // smallest step was 308 ticks.)
            let stray = stray.max(prev_stray - 9_000);
            prev_stray = stray;
            let t = 1_000_000 + k * 9_322 + stray;
            ins.push(t);
            outs.push(s.push(t));
        }
        // (A stamp before its predecessor would be a reset: none here.)
        assert!(ins.windows(2).all(|w| w[1] > w[0]));
        assert!(outs.iter().zip(&ins).all(|(o, i)| (o - i).abs() <= MAX_OFF), "within 50 ms");
        let steps: Vec<i64> = outs[60..].windows(2).map(|w| w[1] - w[0]).collect();
        let short = steps.iter().filter(|&&d| d < 4_500).count();
        assert!(short * 1000 < steps.len(), "frames under 50 ms apart: {short} of {}", steps.len());
    }

    #[test]
    fn a_steady_camera_passes_through_unchanged() {
        let mut s = Smooth::new();
        for k in 0..200i64 {
            let t = 5_000 + k * 3_000 + if k % 2 == 0 { 0 } else { 30 };
            assert_eq!(s.push(t), t);
        }
        assert!(!s.is_active());
    }

    #[test]
    fn a_gap_or_a_reset_goes_to_the_camera_time() {
        let ins = jittery(100);
        let mut s = Smooth::new();
        for &t in &ins {
            s.push(t);
        }
        assert!(s.is_active());
        let last = *ins.last().unwrap();
        // A 2 s gap (a dropped connection, a pause).
        assert_eq!(s.push(last + 180_000), last + 180_000);
        assert!(!s.is_active(), "history starts over");
        // A reset (the camera restarted its clock).
        assert_eq!(s.push(1_000), 1_000);
        assert_eq!(s.push(11_000), 11_000);
    }

    #[test]
    fn never_more_than_50_ms_from_the_camera_even_when_its_rate_drifts() {
        // A camera whose frames speed up and slow down (its real rate
        // changes), with jitter on top: the output follows within 50 ms.
        let mut s = Smooth::new();
        let mut t = 0i64;
        let mut seed: u64 = 3;
        for k in 0..2_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let step = if (k / 200) % 2 == 0 { 9_000 } else { 13_000 };
            let jitter = ((seed >> 33) % 9_000) as i64 - 4_500;
            t += step;
            let o = s.push(t + jitter);
            assert!((o - (t + jitter)).abs() <= MAX_OFF);
        }
    }

    #[test]
    fn repeated_stamps_still_increase() {
        let mut s = Smooth::new();
        let ins = jittery(60);
        let mut last = i64::MIN;
        for &t in &ins {
            let o = s.push(t);
            assert!(o > last);
            last = o;
            let o2 = s.push(t + 1); // a camera stamping two frames alike
            assert!(o2 > last);
            last = o2;
        }
    }

    #[test]
    fn streams_with_b_frames_are_never_smoothed() {
        // I0 P3 B1 B2 P6 B4 B5 ...: reordering, so DtsFromPts derives the
        // decode times and they pass on exactly as it gives them.
        const F: i64 = 3000;
        let mut pts = vec![0i64];
        for g in 0..40i64 {
            let base = g * 3 * F;
            pts.extend([base + 3 * F, base + F, base + 2 * F]);
        }
        let mut a = RtspVideoTimes::new(90_000);
        let mut b = DtsFromPts::new(90_000);
        let mut reordered = false;
        for (i, &p) in pts.iter().enumerate() {
            let key = i == 0;
            let got = a.push(p, key);
            assert_eq!(got, b.push(p, key), "frame {i}");
            reordered |= got.1 != 0;
        }
        assert!(reordered, "the derivation switched on");
        assert!(!a.smoothing());
    }

    #[test]
    fn reordering_found_after_smoothing_started_never_steps_back() {
        // Jittery stamps (smoothing on) with a frame stamped before the one
        // ahead of it every 12 frames: reordering is found at the third,
        // right after a smoothed frame that may run ahead of the camera.
        let mut checked = 0;
        for shift in 0..40usize {
            let mut ins = jittery(200 + shift);
            ins.drain(..shift);
            for i in (12..ins.len() - 1).step_by(12) {
                ins[i + 1] = ins[i] - 3_000;
            }
            let mut v = RtspVideoTimes::new(90_000);
            let mut prev: Option<i64> = None;
            for (i, &t) in ins.iter().enumerate() {
                let (dts, cts) = v.push(t, i % 60 == 0);
                if v.dts.on {
                    if let Some(p) = prev {
                        assert!(dts > p, "shift {shift}, frame {i}: decode time {dts} after {p}");
                    }
                    assert_eq!(dts + i64::from(cts), t, "the offset gives back the camera's time");
                    checked += 1;
                }
                prev = Some(dts);
            }
        }
        assert!(checked > 0, "reordering was found");
    }

    #[test]
    fn rtsp_video_times_smooth_a_jittery_stream_without_b_frames() {
        let mut v = RtspVideoTimes::new(90_000);
        v.set_never_reorders(true);
        let ins = jittery(300);
        let outs: Vec<(i64, i32)> = ins.iter().enumerate().map(|(i, &t)| v.push(t, i % 30 == 0)).collect();
        assert!(v.smoothing());
        assert!(outs.iter().all(|o| o.1 == 0));
        assert!(outs.windows(2).all(|w| w[1].0 > w[0].0));
        assert!(outs.iter().zip(&ins).all(|(o, i)| (o.0 - i).abs() <= MAX_OFF));
    }
}
