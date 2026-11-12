//! Unusual activity: what normally happens at a camera, hour by hour of the
//! week, and how rare an event is against that.
//!
//! The history is kept small: for every camera, label ("motion" for any
//! event, plus each AI label such as person or car) and local day, the set
//! of hours with an event (a 24-bit mask). That is what the baseline is
//! made from, and it outlives the events themselves (events expire with the
//! recordings). A label's rarity at an hour of the week is the share of
//! those weekdays on which it happened in that hour (and the hours either
//! side of it, since an arrival a little early is not unusual).

use std::collections::BTreeMap;

/// Hours in a week (Monday 00:00 = 0).
pub const WEEK_HOURS: usize = 168;

/// The history kept, in days.
pub const WINDOW_DAYS: i64 = 90;
/// Default days of history before anything is flagged.
pub const DEFAULT_MIN_DAYS: u32 = 14;
/// Activity (hours with an event, all labels) needed before flagging.
const MIN_ACTIVE_HOURS: u32 = 10;

/// Local day number (days since 1970-01-01) and hour of a time, for a
/// local offset from UTC in seconds.
pub fn day_hour(ts: i64, offset_secs: i64) -> (i64, u32) {
    let t = ts + offset_secs;
    (t.div_euclid(86400), (t.rem_euclid(86400) / 3600) as u32)
}

/// Monday = 0 for a day number.
pub fn weekday(day: i64) -> usize {
    (day + 3).rem_euclid(7) as usize
}

/// Hour of the week (Monday 00:00 = 0) of a day and hour.
pub fn how(day: i64, hour: u32) -> usize {
    weekday(day) * 24 + hour as usize
}

/// One stored row: a label's active hours on a day.
#[derive(Debug, Clone, PartialEq)]
pub struct DayRow {
    pub day: i64,
    pub label: String,
    pub hours: u32,
}

/// What is normal at one camera.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Baseline {
    /// Days observed, per weekday (Monday = 0).
    pub weekday_days: [u32; 7],
    /// Days observed in all.
    pub span_days: u32,
    /// Per label: days with activity in each hour of the week.
    pub labels: BTreeMap<String, Vec<u16>>,
}

/// Build the baseline from the stored rows, for the days `first_day` up to
/// but not including `end_day`.
pub fn baseline(rows: &[DayRow], first_day: i64, end_day: i64) -> Baseline {
    let mut b = Baseline::default();
    if end_day <= first_day {
        return b;
    }
    for d in first_day..end_day {
        b.weekday_days[weekday(d)] += 1;
    }
    b.span_days = (end_day - first_day) as u32;
    for r in rows.iter().filter(|r| r.day >= first_day && r.day < end_day) {
        let v = b.labels.entry(r.label.clone()).or_insert_with(|| vec![0; WEEK_HOURS]);
        for h in 0..24u32 {
            if r.hours & (1 << h) != 0 {
                v[how(r.day, h)] += 1;
            }
        }
    }
    b
}

impl Baseline {
    /// Activity in all, in hours with an event summed over days ("motion").
    pub fn active_hours(&self) -> u32 {
        self.labels.get("motion").map_or(0, |v| v.iter().map(|&c| c as u32).sum())
    }

    /// Enough history to judge by?
    pub fn ready(&self, min_days: u32) -> bool {
        self.span_days >= min_days.max(1) && self.active_hours() >= MIN_ACTIVE_HOURS
    }

    /// How often `label` normally happens at hour-of-week `h`: 0.0 (never
    /// seen) to 1.0 (every such day). None for a label never seen at all.
    pub fn share(&self, label: &str, h: usize) -> Option<f32> {
        let v = self.labels.get(label)?;
        let h = h % WEEK_HOURS;
        // The same hour of the week, with its neighbours at half weight.
        let (mut num, mut den) = (0f32, 0f32);
        for (off, w) in [(WEEK_HOURS - 1, 0.5f32), (0, 1.0), (1, 0.5)] {
            let i = (h + off) % WEEK_HOURS;
            num += w * v[i] as f32;
            den += w * self.weekday_days[i / 24] as f32;
        }
        let by_week = if den > 0.0 { num / den } else { 0.0 };
        // The same hour of any day: a steadier figure while the weeks are few.
        let hod = h % 24;
        let (mut num, mut den) = (0f32, 0f32);
        for (off, w) in [(23usize, 0.5f32), (0, 1.0), (1, 0.5)] {
            let hh = (hod + off) % 24;
            for wd in 0..7 {
                num += w * v[wd * 24 + hh] as f32;
                den += w * self.weekday_days[wd] as f32;
            }
        }
        let by_hour = if den > 0.0 { num / den } else { 0.0 };
        Some(by_week.max(0.5 * by_hour).min(1.0))
    }
}

/// The highest share of days that still counts as unusual, for a
/// sensitivity of 1 (only what has never happened then) to 5 (touchy).
pub fn limit(sensitivity: u8) -> f32 {
    [0.005, 0.02, 0.05, 0.10, 0.18][(sensitivity.clamp(1, 5) - 1) as usize]
}

/// A judged event.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// The label that was the most unusual.
    pub label: String,
    /// 0..1: 1 = never seen at this time.
    pub rarity: f32,
}

/// Is an event with these labels unusual at this hour of the week? Only
/// `watched` labels are judged ("motion" stands for an event with no AI
/// label); the rarest one decides. None: usual, or nothing to judge.
pub fn judge(b: &Baseline, labels: &[String], watched: &[String], h: usize, sensitivity: u8) -> Option<Verdict> {
    let mut judged: Vec<String> = labels.iter().map(|l| l.to_lowercase()).filter(|l| watched.iter().any(|w| w.eq_ignore_ascii_case(l))).collect();
    if labels.is_empty() && watched.iter().any(|w| w.eq_ignore_ascii_case("motion")) {
        judged.push("motion".into());
    }
    let max = limit(sensitivity);
    let mut worst: Option<Verdict> = None;
    for l in judged {
        // A label never seen on this camera: nothing is "normal" for it.
        let share = b.share(&l, h).unwrap_or(0.0);
        if share <= max && worst.as_ref().is_none_or(|w| 1.0 - share > w.rarity) {
            worst = Some(Verdict { label: l, rarity: 1.0 - share });
        }
    }
    worst
}

/// The labels one event adds to the history: "motion" and its AI labels
/// (not tags made by rules, zones or plates, nor "unusual"). An event made
/// by a zone (loitering, line crossing) is not motion: only its AI labels count.
pub fn history_labels(objects: &[String]) -> Vec<String> {
    let mut v = Vec::new();
    if !objects.iter().any(|o| crate::tracker::is_zone_tag(o)) {
        v.push("motion".to_string());
    }
    for o in objects {
        let o = o.trim().to_lowercase();
        if !o.is_empty() && !crate::tracker::is_tag_label(&o) && !v.contains(&o) {
            v.push(o);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1970-01-05 is a Monday: day 4.
    const MON: i64 = 4;

    fn row(day: i64, label: &str, hours: &[u32]) -> DayRow {
        DayRow { day, label: label.into(), hours: hours.iter().fold(0, |m, h| m | (1 << h)) }
    }

    /// Four weeks: a person at the gate at 8 and 17 on weekdays, a car at 17,
    /// nothing at night.
    fn month() -> Vec<DayRow> {
        let mut rows = Vec::new();
        for d in MON..MON + 28 {
            if weekday(d) < 5 {
                rows.push(row(d, "motion", &[8, 17]));
                rows.push(row(d, "person", &[8, 17]));
                rows.push(row(d, "car", &[17]));
            } else {
                rows.push(row(d, "motion", &[11]));
            }
        }
        rows
    }

    fn base() -> Baseline {
        baseline(&month(), MON, MON + 28)
    }

    #[test]
    fn days_and_weekdays() {
        assert_eq!(weekday(MON), 0);
        assert_eq!(weekday(MON + 6), 6);
        assert_eq!(day_hour(0, 0), (0, 0));
        // 23:30 UTC the day before, in a +2 h zone, is 01:30 local.
        assert_eq!(day_hour(86400 - 1800, 7200), (1, 1));
        assert_eq!(day_hour(-1800, -3600 * 5), (-1, 18));
        assert_eq!(how(MON, 5), 5);
        assert_eq!(how(MON + 2, 3), 51);
    }

    #[test]
    fn baseline_counts_days_per_hour_of_the_week() {
        let b = base();
        assert_eq!(b.span_days, 28);
        assert_eq!(b.weekday_days, [4; 7]);
        let person = &b.labels["person"];
        assert_eq!(person[how(MON, 8)], 4);
        assert_eq!(person[how(MON, 3)], 0);
        assert_eq!(person[how(MON + 5, 8)], 0);
        assert!(b.ready(14));
        assert!(!b.ready(30));
    }

    #[test]
    fn usual_hours_are_not_flagged_and_a_night_visit_is() {
        let b = base();
        let watched = vec!["person".to_string()];
        let person = vec!["person".to_string()];
        // Monday 8:12 and 17:40: every week.
        assert_eq!(judge(&b, &person, &watched, how(MON, 8), 3), None);
        assert_eq!(judge(&b, &person, &watched, how(MON, 17), 3), None);
        // An arrival an hour early or late is still near a usual hour.
        assert_eq!(judge(&b, &person, &watched, how(MON, 9), 3), None);
        // 3 AM Wednesday: never.
        let v = judge(&b, &person, &watched, how(MON + 2, 3), 3).unwrap();
        assert_eq!(v.label, "person");
        assert!((v.rarity - 1.0).abs() < 1e-6);
        // Saturday at 8: the weekday pattern does not cover the weekend, but
        // the same hour on other days speaks for it, so it passes.
        assert_eq!(judge(&b, &person, &watched, how(MON + 5, 8), 3), None);
    }

    #[test]
    fn only_watched_labels_count() {
        let b = base();
        let night = how(MON + 2, 3);
        let car = vec!["car".to_string()];
        assert_eq!(judge(&b, &car, &["person".to_string()], night, 3), None);
        assert!(judge(&b, &car, &["person".to_string(), "car".to_string()], night, 3).is_some());
        // Plain motion (no AI label) is judged as "motion" when watched.
        assert!(judge(&b, &[], &["motion".to_string()], night, 3).is_some());
        assert_eq!(judge(&b, &[], &["person".to_string()], night, 3), None);
    }

    #[test]
    fn a_new_label_is_unusual_and_the_rarest_decides() {
        let b = base();
        let watched = vec!["person".to_string(), "dog".to_string(), "car".to_string()];
        let v = judge(&b, &["dog".to_string()], &watched, how(MON, 8), 3).unwrap();
        assert_eq!(v.label, "dog");
        // A person at 17 is usual, but the car at 8 is not.
        let v = judge(&b, &["person".to_string(), "car".to_string()], &watched, how(MON, 8), 3).unwrap();
        assert_eq!(v.label, "car");
    }

    #[test]
    fn sensitivity_moves_the_line() {
        // A person at 20:00 on one weekday in four weeks.
        let mut rows = month();
        rows.push(row(MON + 1, "person", &[20]));
        rows.push(row(MON + 1, "motion", &[20]));
        let b = baseline(&rows, MON, MON + 28);
        let watched = vec!["person".to_string()];
        let person = vec!["person".to_string()];
        let h = how(MON + 8, 20);
        assert!(judge(&b, &person, &watched, h, 5).is_some());
        assert!(judge(&b, &person, &watched, h, 2).is_none());
        assert!(limit(1) < limit(3) && limit(3) < limit(5));
    }

    #[test]
    fn learning_until_enough_history() {
        let rows = month();
        // Seven days of the month: not ready for a 14-day minimum.
        let b = baseline(&rows, MON, MON + 7);
        assert!(!b.ready(14));
        assert!(b.ready(7));
        // No activity at all: never ready.
        let empty = baseline(&[], MON, MON + 60);
        assert!(!empty.ready(14));
        // Nothing observed.
        assert_eq!(baseline(&rows, MON, MON), Baseline::default());
    }

    #[test]
    fn history_labels_skip_the_tag_and_repeat() {
        let l = history_labels(&["Person".into(), "unusual".into(), "person".into(), "Car".into()]);
        assert_eq!(l, ["motion", "person", "car"]);
        assert_eq!(history_labels(&[]), ["motion"]);
        // Rule and plate tags do not make a row each; a zone event is not motion.
        let l = history_labels(&["car".into(), "plate ABC123 (Mom)".into(), "unknown plate XYZ".into(), "rule: Delivery".into(), "quiet: Mine".into()]);
        assert_eq!(l, ["motion", "car"]);
        assert_eq!(history_labels(&["loitering: Gate".into(), "person".into()]), ["person"]);
        assert!(history_labels(&["crossed: Gate (in)".into()]).is_empty());
    }
}
