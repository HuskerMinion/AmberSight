//! RTP clock jumps.
//!
//! Recording times are this computer's clock at the session's start plus
//! media time, and media time comes from the camera's RTP timestamps. When a
//! camera's clock jumps (it set its time, or restarted its encoder without
//! dropping the session) the RTP timestamps jump too, and without this every
//! later segment would be dated by the jump: an hour forward puts the
//! recordings an hour into the future.
//!
//! [`ClockJump`] watches one track's timestamps against the time the frames
//! arrive. Both are measured from the highest timestamp seen so far (not
//! the previous frame, which B-frame reordering puts behind it). A step
//! forward far beyond the time that has passed, or a step back further than
//! any reordering can go, is a jump: from then on the track's timestamps are
//! moved so they continue from the last good one by the time that really
//! passed. Ordinary timing is never touched: B-frame reordering (up to 16
//! frames, seconds at a low frame rate), a camera that skipped frames (media
//! time and arrival time move together), and frames that arrive in a burst
//! after a network stall (media time moves less than arrival time).

/// Media time this far ahead of the time that passed since the highest
/// timestamp arrived is a jump forward.
const FORWARD_SECS: f64 = 10.0;
/// A jump back: a step to below the highest timestamp by more than twice
/// the deepest reordering seen in the last `DEPTH_FRAMES` frames, plus
/// `BACK_MARGIN_SECS`, and at least `BACK_MIN_SECS` (so a clock set back by
/// a few seconds is caught on a stream without B-frames). Before any
/// reordering has been seen, a step back of up to `BACK_FRAMES` frame
/// intervals (H.264/H.265 reorder at most 16 frames) is taken for the
/// first reordering, not a jump.
const BACK_MIN_SECS: f64 = 1.0;
const BACK_MARGIN_SECS: f64 = 0.25;
const BACK_FRAMES: f64 = 16.0;
const DEPTH_FRAMES: usize = 64;
/// Frame intervals kept for the median.
const STEPS_KEPT: usize = 31;

#[derive(Default)]
pub struct ClockJump {
    /// Ticks added to the camera's timestamps.
    offset: i64,
    /// The highest timestamp as given (camera ticks) and when it arrived
    /// (seconds on the caller's monotonic clock).
    raw_high: Option<(i64, f64)>,
    /// The latest steps (ticks) by which the highest timestamp grew.
    steps: std::collections::VecDeque<i64>,
    /// How far (ticks) each of the latest frames was behind the highest
    /// timestamp (0: it was the highest): the reordering depth.
    behind: std::collections::VecDeque<i64>,
    /// Frames in a row that were not reordered: past `DEPTH_FRAMES` the
    /// stream has no B-frames, the back limit is `BACK_MIN_SECS`, and a
    /// small step back no longer counts as reordering.
    in_order: usize,
    /// The largest timestamp handed out so far, moved (for re-anchoring
    /// after a jump: never behind anything already handed out).
    high: Option<i64>,
    jumps: u32,
}

impl ClockJump {
    /// The timestamp to use for a frame with camera timestamp `ticks`
    /// (`rate` ticks a second) that arrived at `now` seconds. Returns the
    /// moved timestamp and, when this frame showed a jump, its size in
    /// seconds (camera time minus the time that passed).
    pub fn map(&mut self, ticks: i64, rate: u32, now: f64) -> (i64, Option<f64>) {
        let rate_f = f64::from(rate.max(1));
        let mut jumped = None;
        if let Some((top, at)) = self.raw_high {
            let media = (ticks - top) as f64 / rate_f;
            let passed = (now - at).max(0.0);
            let depth = self.behind.iter().copied().max().unwrap_or(0) as f64 / rate_f;
            let back_limit = if depth > 0.0 {
                BACK_MIN_SECS.max(2.0 * depth + BACK_MARGIN_SECS)
            } else if self.in_order >= DEPTH_FRAMES {
                BACK_MIN_SECS
            } else {
                BACK_MIN_SECS.max(BACK_FRAMES * self.median_step() as f64 / rate_f)
            };
            if media > passed + FORWARD_SECS || media < -back_limit {
                // Continue from the last good timestamp by the time that
                // passed (at least one tick, so times keep increasing).
                let high = self.high.unwrap_or(top + self.offset);
                let next = high + ((passed * rate_f).round() as i64).max(1);
                self.offset = next - ticks;
                self.jumps += 1;
                jumped = Some(media - passed);
                // The camera's new clock starts over.
                self.raw_high = None;
                self.steps.clear();
                self.behind.clear();
            }
        }
        if self.behind.len() == DEPTH_FRAMES {
            self.behind.pop_front();
        }
        let behind = self.raw_high.map_or(0, |(top, _)| (top - ticks).max(0));
        // (A stream long without reordering does not start "reordering" by
        // a small step back: its limit stays the fixed one.)
        let behind = if self.in_order >= DEPTH_FRAMES { 0 } else { behind };
        self.in_order = if behind > 0 { 0 } else { self.in_order.saturating_add(1) };
        self.behind.push_back(behind);
        match self.raw_high {
            Some((top, _)) if ticks <= top => {}
            Some((top, _)) => {
                if self.steps.len() == STEPS_KEPT {
                    self.steps.pop_front();
                }
                self.steps.push_back(ticks - top);
                self.raw_high = Some((ticks, now));
            }
            None => self.raw_high = Some((ticks, now)),
        }
        let out = ticks + self.offset;
        self.high = Some(self.high.map_or(out, |h| h.max(out)));
        (out, jumped)
    }

    /// The median step of the highest timestamp (ticks; 0 before any).
    fn median_step(&self) -> i64 {
        let mut s: Vec<i64> = self.steps.iter().copied().collect();
        s.sort_unstable();
        s.get(s.len() / 2).copied().unwrap_or(0)
    }

    /// Move a timestamp of this track that is not a frame (a sender report)
    /// the same way as the frames.
    pub fn apply(&self, ticks: i64) -> i64 {
        ticks + self.offset
    }

    /// Jumps seen so far.
    #[cfg(test)]
    pub fn jumps(&self) -> u32 {
        self.jumps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames at 10 fps on a 90 kHz clock, arriving on time.
    fn run(c: &mut ClockJump, from: u32, n: u32, base: i64) -> Vec<i64> {
        (from..from + n).map(|i| c.map(base + i64::from(i) * 9000, 90_000, f64::from(i) / 10.0).0).collect()
    }

    #[test]
    fn steady_timestamps_pass_unchanged() {
        let mut c = ClockJump::default();
        let out = run(&mut c, 0, 100, 5000);
        assert_eq!(out, (0..100).map(|i| 5000 + i * 9000).collect::<Vec<_>>());
        assert_eq!(c.jumps(), 0);
    }

    #[test]
    fn a_jump_forward_is_taken_out() {
        let mut c = ClockJump::default();
        let a = run(&mut c, 0, 50, 0);
        // The camera's clock moves one hour ahead at frame 50.
        let hour = 3600 * 90_000;
        let (t, j) = c.map(hour + 50 * 9000, 90_000, 5.0);
        assert!(j.is_some_and(|s| (s - 3600.0).abs() < 1.0), "{j:?}");
        assert_eq!(t, *a.last().unwrap() + 9000, "continues by the time that passed");
        let b: Vec<i64> = (51..60).map(|i| c.map(hour + i * 9000, 90_000, i as f64 / 10.0).0).collect();
        assert_eq!(b[0], t + 9000);
        assert!(b.windows(2).all(|w| w[1] - w[0] == 9000));
        assert_eq!(c.jumps(), 1);
        // A sender report on the camera's new clock is moved the same way.
        assert_eq!(c.apply(hour + 55 * 9000), t + 5 * 9000);
    }

    #[test]
    fn a_jump_back_is_taken_out() {
        let mut c = ClockJump::default();
        let a = run(&mut c, 0, 300, 30 * 90_000);
        // Back 30 s at frame 300.
        let (t, j) = c.map(300 * 9000, 90_000, 30.0);
        assert!(j.is_some_and(|s| (s + 30.1).abs() < 0.2), "{j:?}");
        assert_eq!(t, *a.last().unwrap() + 9000);
    }

    #[test]
    fn reordering_skipped_frames_and_bursts_are_not_jumps() {
        let mut c = ClockJump::default();
        // B-frames: presentation times go back a few frames.
        for (i, pts) in [0i64, 3, 1, 2, 6, 4, 5, 9, 7, 8].iter().enumerate() {
            let (t, j) = c.map(pts * 9000, 90_000, i as f64 / 10.0);
            assert_eq!((t, j), (pts * 9000, None));
        }
        // The camera sends nothing for 20 s, then goes on (both clocks moved).
        assert_eq!(c.map(209 * 9000, 90_000, 20.9), (209 * 9000, None));
        // The network stalls 15 s, then the held-back frames arrive at once.
        for i in 210..220i64 {
            assert_eq!(c.map(i * 9000, 90_000, 36.0), (i * 9000, None));
        }
        assert_eq!(c.jumps(), 0);
    }

    /// Frames in decode order (presentation times as given) arriving at
    /// `fps`, each group of `gop` frames repeating `order` (offsets in frames).
    fn reordered(c: &mut ClockJump, fps: f64, groups: i64, order: &[i64]) -> Vec<i64> {
        let n = order.len() as i64;
        let tick = (90_000.0 / fps) as i64;
        let mut out = Vec::new();
        for g in 0..groups {
            for (k, o) in order.iter().enumerate() {
                let i = g * n + k as i64;
                let pts = (g * n + o) * tick + 1000;
                let (t, j) = c.map(pts, 90_000, i as f64 / fps);
                assert_eq!(j, None, "frame {i}: a reordering taken for a jump");
                out.push(t - pts);
            }
        }
        out
    }

    #[test]
    fn low_frame_rate_b_frames_are_not_jumps() {
        // 1 fps, I P B B (P shown after the two B-frames: back 2 s).
        let mut c = ClockJump::default();
        assert!(reordered(&mut c, 1.0, 40, &[0, 3, 1, 2]).iter().all(|d| *d == 0));
        assert_eq!(c.jumps(), 0);
        // H.265 hierarchical B (GOP of 8 in decode order) at 3 fps: back 2.3 s.
        let mut c = ClockJump::default();
        assert!(reordered(&mut c, 3.0, 30, &[0, 8, 4, 2, 1, 3, 6, 5, 7]).iter().all(|d| *d == 0));
        assert_eq!(c.jumps(), 0);
    }

    #[test]
    fn a_clock_set_back_by_seconds_is_a_jump() {
        // 10 fps, no B-frames; the clock is set back 3 s (as NTP does).
        let mut c = ClockJump::default();
        let a = run(&mut c, 0, 100, 0);
        let (t, j) = c.map(97 * 9000 - 3 * 90_000 + 3 * 9000, 90_000, 10.0);
        assert!(j.is_some_and(|s| (s + 3.1).abs() < 0.2), "{j:?}");
        assert_eq!(t, *a.last().unwrap() + 9000);
        // With B-frames at 10 fps (reordering 0.2 s), 3 s back is a jump too.
        let mut c = ClockJump::default();
        reordered(&mut c, 10.0, 25, &[0, 3, 1, 2]);
        let (_, j) = c.map(97 * 9000 - 3 * 90_000 + 1000, 90_000, 10.0);
        assert!(j.is_some(), "3 s back with 0.2 s of reordering");
    }

    #[test]
    fn a_clock_set_back_at_one_frame_a_second_is_a_jump() {
        // 1 fps, no B-frames: after 64 frames in order the limit is 1 s.
        for back in [3i64, 10] {
            let mut c = ClockJump::default();
            for i in 0..100i64 {
                assert_eq!(c.map(i * 90_000, 90_000, i as f64).1, None);
            }
            let (_, j) = c.map((100 - back) * 90_000, 90_000, 100.0);
            assert!(j.is_some_and(|s| (s + back as f64).abs() < 0.2), "{back} s back: {j:?}");
        }
        // A step back under the limit does not raise it for the next one.
        let mut c = ClockJump::default();
        for i in 0..100i64 {
            c.map(i * 90_000, 90_000, i as f64);
        }
        assert_eq!(c.map(99 * 90_000 - 45_000, 90_000, 100.0).1, None, "half a second back is not a jump");
        for i in 100..105i64 {
            c.map(i * 90_000, 90_000, i as f64 + 1.0);
        }
        assert!(c.map(101 * 90_000, 90_000, 106.0).1.is_some(), "3 s back after it is still one");
    }

    #[test]
    fn a_jump_back_is_still_seen_with_b_frames() {
        let mut c = ClockJump::default();
        reordered(&mut c, 1.0, 20, &[0, 3, 1, 2]);
        // 80 frames at 1 fps; the clock goes back 60 s.
        let (_, j) = c.map(20 * 90_000, 90_000, 80.0);
        assert!(j.is_some(), "a minute back must count");
        assert_eq!(c.jumps(), 1);
    }
}
