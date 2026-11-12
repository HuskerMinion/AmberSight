//! Unusual-activity flagging: keeps each camera's activity history (which
//! hours of which days had an event, per label), and judges new events
//! against what is normal for that hour of the week (crate::unusual has the
//! maths). The history is read from the events table once an hour for every
//! camera, whether the feature is on or not, so turning it on later does not
//! start from nothing; it is kept for 90 days, longer than the events live.

use super::*;
use crate::unusual::{self, Baseline, DayRow};
use chrono::TimeZone;
use std::collections::HashMap;
use std::time::Duration;

/// Events are read this long after they happen, so one that is still being
/// saved is never missed.
const LAG_SECS: i64 = 3600;

/// Seconds east of UTC at a time, in this computer's zone.
fn offset_at(ts: i64) -> i64 {
    chrono::Local.timestamp_opt(ts, 0).single().map_or(0, |t| i64::from(chrono::Offset::fix(t.offset()).local_minus_utc()))
}

fn day_hour_local(ts: i64) -> (i64, u32) {
    unusual::day_hour(ts, offset_at(ts))
}

impl Engine {
    /// Read the events of a camera not yet in its history. Returns the number read.
    pub fn activity_ingest(&self, camera: &str) -> usize {
        let until = Utc::now().timestamp() - LAG_SECS;
        let from = match self.db.activity_meta(camera) {
            Ok(Some((done, _))) => done,
            Ok(None) => match self.db.first_event_start(camera) {
                Ok(Some(t)) => t,
                _ => return 0,
            },
            Err(_) => return 0,
        };
        let mut from = from;
        let mut read = 0;
        while from < until {
            let to = (from + 14 * 86400).min(until);
            let events = match self.db.events_between(camera, from, to) {
                Ok(e) => e,
                Err(e) => {
                    warn!("activity history of {camera}: {e:#}");
                    return read;
                }
            };
            let mut hours: HashMap<(i64, String), u32> = HashMap::new();
            let mut first_day = i64::MAX;
            for (ts, objects) in &events {
                let (day, hour) = day_hour_local(*ts);
                first_day = first_day.min(day);
                for l in unusual::history_labels(objects) {
                    *hours.entry((day, l)).or_insert(0) |= 1 << hour;
                }
            }
            let rows: Vec<DayRow> = hours.into_iter().map(|((day, label), hours)| DayRow { day, label, hours }).collect();
            if first_day == i64::MAX {
                first_day = day_hour_local(from).0;
            }
            if let Err(e) = self.db.activity_add(camera, &rows, to, first_day) {
                warn!("activity history of {camera}: {e:#}");
                return read;
            }
            read += events.len();
            from = to;
        }
        read
    }

    /// What is normal at this camera now, and how many days it is built from.
    fn activity_baseline(&self, camera: &str) -> Option<Baseline> {
        let (done, first_day) = self.db.activity_meta(camera).ok().flatten()?;
        // Whole days only: the day being read is still partial.
        let end_day = day_hour_local(done).0;
        let start = first_day.max(end_day - unusual::WINDOW_DAYS);
        let rows = self.db.activity_rows(camera, start).ok()?;
        Some(unusual::baseline(&rows, start, end_day))
    }

    /// Is this event out of the camera's pattern? Some(label, rarity) if so.
    pub(super) fn unusual_check(&self, cfg: &CameraConfig, start: i64, labels: &[String]) -> Option<unusual::Verdict> {
        let uc = cfg.unusual.as_ref().filter(|u| u.enabled)?;
        let b = self.activity_baseline(&cfg.name)?;
        if !b.ready(uc.min_days()) {
            return None;
        }
        let (day, hour) = day_hour_local(start);
        unusual::judge(&b, labels, &uc.labels, unusual::how(day, hour), uc.sensitivity())
    }

    /// A camera's unusual-activity learning, for its settings window.
    pub fn unusual_status(&self, id: usize) -> Option<serde_json::Value> {
        let h = self.camera(id)?;
        let cfg = h.running.read().unwrap().clone();
        let uc = cfg.unusual.clone().unwrap_or_default();
        let b = self.activity_baseline(&cfg.name).unwrap_or_default();
        let ready = b.ready(uc.min_days());
        let state = if !uc.enabled {
            "off"
        } else if ready {
            "ready"
        } else {
            "learning"
        };
        // How often each watched label normally happens, per hour of the week
        // (Monday 00:00 first), as a share of days 0..1.
        let mut typical = serde_json::Map::new();
        let mut shown: Vec<String> = uc.labels.iter().map(|l| l.to_lowercase()).collect();
        shown.dedup();
        for l in shown.iter().take(6) {
            let week: Vec<f32> = (0..unusual::WEEK_HOURS).map(|h| (b.share(l, h).unwrap_or(0.0) * 100.0).round() / 100.0).collect();
            typical.insert(l.clone(), serde_json::json!(week));
        }
        Some(serde_json::json!({
            "enabled": uc.enabled,
            "state": state,
            "days": b.span_days,
            "days_needed": uc.min_days(),
            "active_hours": b.active_hours(),
            "typical": typical,
            "seen": b.labels.keys().collect::<Vec<_>>(),
        }))
    }
}

pub fn unusual_loop(engine: Arc<Engine>) {
    // Let the first events of a start settle.
    thread::sleep(Duration::from_secs(90));
    loop {
        let names: Vec<String> = engine.config.lock().unwrap().cameras.iter().map(|c| c.name.clone()).collect();
        for n in &names {
            let read = engine.activity_ingest(n);
            if read > 0 {
                tracing::debug!("activity history: read {read} events of {n}");
            }
        }
        let today = day_hour_local(Utc::now().timestamp()).0;
        if let Err(e) = engine.db.activity_prune(today - unusual::WINDOW_DAYS, &names) {
            warn!("activity history: {e:#}");
        }
        thread::sleep(Duration::from_secs(3600));
    }
}
