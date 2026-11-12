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
