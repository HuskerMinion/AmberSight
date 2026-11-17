//! Listing a battery camera's card recordings over Baichuan.
//!
//! These cameras have no working file search (cmd 14/15 and 272 answer 400 on
//! the firmware tried), but the recording download (cmd 143) answers a time
//! window with the first recording in it, and that recording's info header
//! names its start and end. So the card is walked: ask from a time, read the
//! first header, stop the stream (an empty cmd 144), ask again from just after
//! that recording's end. Nothing is downloaded.
//!
//! Clocks: the card is indexed by the camera's own local time (cmd 104), and a
//! 143 window must stay within one local day (crossing midnight answers 400).
//! The info headers are on that local time on newer firmware but on UTC on
//! older firmware, so each answer has two readings. The walk settles which:
//! a reading that does not fit the asked window is wrong; when both fit, the
//! earlier one is taken, and if it was wrong the next ask (from inside the
//! real recording) brings back the same recording again, which proves it.
//!
//! This module is the walk only (no I/O): `ask` does one request.

use chrono::{Duration, NaiveDateTime, NaiveTime};

/// One recording on the card, on the card's clock (the camera's local time).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardClip {
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    /// Its header clock was settled (false: the walk ended before it was).
    pub sure: bool,
}

/// What one ask (cmd 143 from a time) brought back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The first recording's info header: start and end as written in it.
    Clip { start: NaiveDateTime, end: NaiveDateTime },
    /// Nothing in the window (the end-of-recording message, or refused).
    Nothing,
    /// No answer in time, or the session failed.
    Silent,
    /// Stopped for live video, which comes first.
    GaveWay,
}

/// How a walk went.
#[derive(Debug, Default, PartialEq)]
pub struct Listing {
    pub clips: Vec<CardClip>,
    /// The whole window was walked.
    pub complete: bool,
    /// Stopped for live video (try again later; says nothing about the card).
    pub gave_way: bool,
    /// Stopped at the limit of one walk (recordings or asks): the rest is
    /// for the next walk. Any other early end is the camera's doing.
    pub ran_out: bool,
    /// Whether the headers are on local time (true) or UTC (false), when settled.
    pub header_local: Option<bool>,
    /// Why it ended early, for the log.
    pub why: Option<&'static str>,
    pub asks: u32,
}

/// A header naming a recording longer than this, or one that ends before it
/// starts, is not believed (a battery camera records a minute or two).
pub const MAX_CLIP_SECS: i64 = 30 * 60;
/// Readings this close to the asked window still fit it (clock rounding).
const SLACK_SECS: i64 = 2;

/// Walk the card from `from` to `to` (card clock), asking at most `max_asks`
/// times and listing at most `max_clips`. `card_offset_s` is the camera's
/// clock minus UTC; `header_local` what an earlier walk settled, if any.
pub fn walk(
    from: NaiveDateTime,
    to: NaiveDateTime,
    card_offset_s: i64,
    header_local: Option<bool>,
    max_clips: usize,
    max_asks: u32,
    mut ask: impl FnMut(NaiveDateTime, NaiveDateTime) -> Answer,
) -> Listing {
    let shift = |local: bool| Duration::seconds(if local { 0 } else { card_offset_s });
    // Both readings are the same when the camera is on UTC.
    let mut known = if card_offset_s == 0 { Some(true) } else { header_local };
    let mut out = Listing { header_local: known, ..Default::default() };
    // The last recording, read the earlier of two ways that both fit:
    // (its header times, its index in `clips`, the other reading).
    let mut pending: Option<(NaiveDateTime, NaiveDateTime, usize, bool)> = None;
    let mut at = from;
    let mut silent = 0;
    let fits = |c: &CardClip, at: NaiveDateTime, win_to: NaiveDateTime| {
        c.end >= at - Duration::seconds(SLACK_SECS) && c.start <= win_to + Duration::seconds(SLACK_SECS)
    };
    loop {
        if at > to {
            out.complete = true;
            break;
        }
        if out.clips.len() >= max_clips {
            out.why = Some("listed as many recordings as one check takes");
            out.ran_out = true;
            break;
        }
        if out.asks >= max_asks {
            out.why = Some("asked the camera as often as one check may");
            out.ran_out = true;
            break;
        }
        let day_end = at.date().and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap());
        let win_to = to.min(day_end);
        out.asks += 1;
        match ask(at, win_to) {
            Answer::GaveWay => {
                out.gave_way = true;
                out.why = Some("gave way to live video");
                break;
            }
            Answer::Silent => {
                silent += 1;
                if silent >= 2 {
                    out.why = Some("the camera stopped answering");
                    break;
                }
            }
            Answer::Nothing => {
                silent = 0;
                // The earlier reading was right: a wrong one would have
                // brought back the same recording, not nothing.
                if let Some((_, _, i, other)) = pending.take() {
                    known = Some(!other);
                    out.clips[i].sure = true;
                }
                if win_to >= to {
                    out.complete = true;
                    break;
                }
                at = (at.date() + Duration::days(1)).and_time(NaiveTime::MIN);
            }
            Answer::Clip { start, end } => {
                silent = 0;
                if end < start || (end - start).num_seconds() > MAX_CLIP_SECS {
                    out.why = Some("the camera named a recording that cannot be");
                    break;
                }
                if let Some((ps, pe, i, other)) = pending.take() {
                    if (ps, pe) == (start, end) {
                        // Asked from inside the real recording: the other reading.
                        known = Some(other);
                        let fixed = CardClip { start: start + shift(other), end: end + shift(other), sure: true };
                        out.clips[i] = fixed;
                        let next = fixed.end + Duration::seconds(1);
                        if next <= at {
                            out.why = Some("the camera's answers made no progress");
                            break;
                        }
                        at = next;
                        continue;
                    }
                    known = Some(!other);
                    out.clips[i].sure = true;
                }
                let read = |local: bool| CardClip { start: start + shift(local), end: end + shift(local), sure: true };
                let clip = match known {
                    Some(local) => {
                        let c = read(local);
                        if !fits(&c, at, win_to) {
                            out.why = Some("the camera's answer does not fit the time asked");
                            // What an earlier walk settled may be what is wrong
                            // (the camera's firmware or clock changed): the next
                            // walk settles it afresh.
                            known = None;
                            break;
                        }
                        c
                    }
                    None => {
                        let (l, u) = (read(true), read(false));
                        match (fits(&l, at, win_to), fits(&u, at, win_to)) {
                            (true, false) => {
                                known = Some(true);
                                l
                            }
                            (false, true) => {
                                known = Some(false);
                                u
                            }
                            (false, false) => {
                                out.why = Some("the camera's answer does not fit the time asked");
                                break;
                            }
                            (true, true) => {
                                let (first, other) = if l.end <= u.end { (l, false) } else { (u, true) };
                                pending = Some((start, end, out.clips.len(), other));
                                CardClip { sure: false, ..first }
                            }
                        }
                    }
                };
                // The same recording twice (it is listed once).
                if out.clips.last().is_some_and(|p| p.start == clip.start && p.end == clip.end) {
                    out.why = Some("the camera's answers made no progress");
                    break;
                }
                out.clips.push(clip);
                let next = clip.end + Duration::seconds(1);
                if next <= at {
                    out.why = Some("the camera's answers made no progress");
                    break;
                }
                at = next;
            }
        }
    }
    out.header_local = known;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    /// A fake card: recordings on the camera's local clock; headers written
    /// in local time or UTC (`offset_s` = local minus UTC). Answers like the
    /// camera: the first recording that ends at or after the asked start and
    /// begins by the asked end; a window across midnight is refused.
    struct Card {
        clips: Vec<(NaiveDateTime, NaiveDateTime)>,
        offset_s: i64,
        header_local: bool,
        asked: Vec<(NaiveDateTime, NaiveDateTime)>,
    }

    impl Card {
        fn new(clips: &[(&str, &str)], offset_s: i64, header_local: bool) -> Card {
            Card { clips: clips.iter().map(|(a, b)| (t(a), t(b))).collect(), offset_s, header_local, asked: Vec::new() }
        }

        fn ask(&mut self, a: NaiveDateTime, b: NaiveDateTime) -> Answer {
            self.asked.push((a, b));
            if a.date() != b.date() {
                return Answer::Nothing; // 400
            }
            let h = Duration::seconds(if self.header_local { 0 } else { -self.offset_s });
            match self.clips.iter().find(|(s, e)| *e >= a && *s <= b) {
                Some((s, e)) => Answer::Clip { start: *s + h, end: *e + h },
                None => Answer::Nothing,
            }
        }
    }

    fn spans(l: &Listing) -> Vec<(String, String, bool)> {
        l.clips.iter().map(|c| (c.start.format("%H:%M:%S").to_string(), c.end.format("%H:%M:%S").to_string(), c.sure)).collect()
    }

    const CLIPS: [(&str, &str); 3] = [
        ("2026-11-16 08:00:00", "2026-11-16 08:00:40"),
        ("2026-11-16 09:30:10", "2026-11-16 09:31:05"),
        ("2026-11-16 11:59:00", "2026-11-16 11:59:30"),
    ];

    #[test]
    fn a_card_with_local_headers_is_listed_without_downloading() {
        let mut card = Card::new(&CLIPS, -6 * 3600, true);
        let l = walk(t("2026-11-16 06:00:00"), t("2026-11-16 12:00:00"), -6 * 3600, None, 50, 50, |a, b| card.ask(a, b));
        assert!(l.complete, "{l:?}");
        assert_eq!(
            spans(&l),
            vec![("08:00:00".into(), "08:00:40".into(), true), ("09:30:10".into(), "09:31:05".into(), true), ("11:59:00".into(), "11:59:30".into(), true)]
        );
        assert_eq!(l.header_local, Some(true));
        // One ask per recording and one that finds nothing more.
        assert_eq!(l.asks, 4);
        assert_eq!(card.asked[1].0, t("2026-11-16 08:00:41"), "the next ask starts just after the last recording");
    }

    #[test]
    fn utc_headers_are_recognized_and_nothing_is_skipped() {
        // Older firmware: headers in UTC, camera on UTC-6. Both readings fit a
        // 12-hour window, so the walk takes the earlier one first; when that
        // was wrong the camera answers with the same recording again.
        let mut card = Card::new(&CLIPS, -6 * 3600, false);
        let l = walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), -6 * 3600, None, 50, 50, |a, b| card.ask(a, b));
        assert!(l.complete, "{l:?}");
        assert_eq!(l.header_local, Some(false));
        assert_eq!(
            spans(&l),
            vec![("08:00:00".into(), "08:00:40".into(), true), ("09:30:10".into(), "09:31:05".into(), true), ("11:59:00".into(), "11:59:30".into(), true)]
        );
        assert_eq!(l.asks, 4, "the earlier reading was the right one here: no extra ask");
    }

    #[test]
    fn a_camera_east_of_utc_with_utc_headers_works_too() {
        let mut card = Card::new(&CLIPS, 5 * 3600 + 1800, false);
        let l = walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), 5 * 3600 + 1800, None, 50, 50, |a, b| card.ask(a, b));
        assert!(l.complete, "{l:?}");
        assert_eq!(spans(&l).len(), 3);
        assert!(l.clips.iter().zip(CLIPS.iter()).all(|(c, (a, b))| c.start == t(a) && c.end == t(b) && c.sure), "{l:?}");
        // Here the earlier reading was wrong: one extra ask settled it.
        assert_eq!((l.asks, l.header_local), (5, Some(false)));
        // Settled, a later walk needs no extra ask.
        let mut again = Card::new(&CLIPS, 5 * 3600 + 1800, false);
        let l2 = walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), 5 * 3600 + 1800, Some(false), 50, 50, |a, b| again.ask(a, b));
        assert_eq!((spans(&l2), l2.asks), (spans(&l), 4));
    }

    #[test]
    fn the_walk_never_asks_across_midnight() {
        let clips = [("2026-11-15 23:10:00", "2026-11-15 23:11:00"), ("2026-11-16 00:20:00", "2026-11-16 00:20:30")];
        let mut card = Card::new(&clips, 0, true);
        let l = walk(t("2026-11-15 22:00:00"), t("2026-11-16 03:00:00"), 0, None, 50, 50, |a, b| card.ask(a, b));
        assert!(l.complete && l.clips.len() == 2, "{l:?}");
        assert!(card.asked.iter().all(|(a, b)| a.date() == b.date()), "{:?}", card.asked);
        // An empty day moves on to the next one.
        assert!(card.asked.iter().any(|(a, _)| *a == t("2026-11-16 00:00:00")));
    }

    #[test]
    fn a_recording_asked_partway_into_is_listed_whole() {
        let mut card = Card::new(&CLIPS, 0, true);
        let l = walk(t("2026-11-16 09:30:30"), t("2026-11-16 10:00:00"), 0, None, 50, 50, |a, b| card.ask(a, b));
        assert_eq!(spans(&l), vec![("09:30:10".into(), "09:31:05".into(), true)]);
    }

    #[test]
    fn limits_hold_and_bad_answers_end_the_walk() {
        // At most this many recordings and asks.
        let mut card = Card::new(&CLIPS, 0, true);
        let l = walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), 0, None, 2, 50, |a, b| card.ask(a, b));
        assert_eq!((l.clips.len(), l.complete, l.ran_out), (2, false, true));
        let mut card = Card::new(&CLIPS, 0, true);
        let l = walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), 0, None, 50, 1, |a, b| card.ask(a, b));
        assert_eq!((l.asks, l.clips.len(), l.complete, l.ran_out), (1, 1, false, true));
        // A camera that keeps naming the same recording, or one from long
        // before the ask, or a recording of days, ends it at once.
        let same = || Answer::Clip { start: t("2026-11-16 08:00:00"), end: t("2026-11-16 08:00:40") };
        let l = walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), 0, None, 50, 50, |_, _| same());
        assert!(!l.complete && !l.ran_out && l.clips.len() == 1 && l.asks == 2, "{l:?}");
        let l = walk(t("2026-11-16 09:00:00"), t("2026-11-16 18:00:00"), 0, None, 50, 50, |_, _| same());
        assert!(!l.complete && !l.ran_out && l.clips.is_empty(), "{l:?}");
        let long = |_, _| Answer::Clip { start: t("2026-11-16 08:00:00"), end: t("2026-11-18 08:00:00") };
        assert!(walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), 0, None, 50, 50, long).clips.is_empty());
        let backwards = |_, _| Answer::Clip { start: t("2026-11-16 08:00:00"), end: t("2026-11-16 07:00:00") };
        assert!(walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), 0, None, 50, 50, backwards).clips.is_empty());
        // Silence twice ends it; live video wanted ends it and says so.
        let l = walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), 0, None, 50, 50, |_, _| Answer::Silent);
        assert!(!l.complete && !l.ran_out && l.asks == 2);
        let l = walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), 0, None, 50, 50, |_, _| Answer::GaveWay);
        assert!(l.gave_way && !l.complete && !l.ran_out && l.asks == 1);
    }

    #[test]
    fn a_settled_header_clock_that_no_longer_fits_is_forgotten() {
        // An earlier walk settled local headers; the camera now writes UTC
        // ones far enough off that they do not fit the window asked.
        let mut card = Card::new(&CLIPS[..1], -6 * 3600, false);
        let l = walk(t("2026-11-16 07:00:00"), t("2026-11-16 09:00:00"), -6 * 3600, Some(true), 50, 50, |a, b| card.ask(a, b));
        assert!(!l.complete && !l.ran_out && l.clips.is_empty(), "{l:?}");
        assert_eq!(l.header_local, None, "the next walk settles it afresh");
        // And it does.
        let mut card = Card::new(&CLIPS[..1], -6 * 3600, false);
        let l = walk(t("2026-11-16 07:00:00"), t("2026-11-16 09:00:00"), -6 * 3600, l.header_local, 50, 50, |a, b| card.ask(a, b));
        assert!(l.complete && l.clips.len() == 1, "{l:?}");
        assert_eq!(l.header_local, Some(false));
    }

    #[test]
    fn a_reading_left_unsettled_is_marked() {
        // Only one recording, then the walk stops (one ask allowed): both
        // readings fit, so it is not sure which one is right.
        let mut card = Card::new(&CLIPS[..1], -6 * 3600, false);
        let l = walk(t("2026-11-16 00:00:00"), t("2026-11-16 18:00:00"), -6 * 3600, None, 50, 1, |a, b| card.ask(a, b));
        assert_eq!(l.clips.len(), 1);
        assert!(!l.clips[0].sure && l.header_local.is_none());
    }
}
