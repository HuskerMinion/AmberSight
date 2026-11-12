//! No-motion alert: a camera with `no_motion_minutes` set sends a
//! notification when nothing has moved in front of it for that long (a pet,
//! or a person living alone), and another once something moves again.
//! Counts any motion over the threshold, armed or not. Watching starts when
//! the camera is online and inside its `no_motion_schedule` window, so a
//! window that opens at 07:00 first alerts at 07:00 plus the minutes.
//! Offline cameras are left to the offline alert (a short drop does not
//! restart the count). During an alert pause nothing is judged; a quiet
//! stretch is reported once the pause ends.

use super::*;
use std::collections::HashMap;
use std::time::Duration;

/// One watched camera.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Watch {
    /// Watching since (epoch s).
    since: i64,
    /// When reported: the start of the quiet stretch that was reported.
    reported: Option<i64>,
}

#[derive(Debug, PartialEq)]
enum Say {
    Nothing,
    /// Quiet this many seconds.
    Quiet(i64),
    /// Moving again after this many quiet seconds.
    Moving(i64),
}

fn judge(w: &mut Watch, now: i64, last_activity: i64, limit_secs: i64) -> Say {
    let start = w.since.max(last_activity);
    match w.reported {
        None if now - start >= limit_secs => {
            w.reported = Some(start);
            Say::Quiet(now - start)
        }
        Some(b) if last_activity > b => {
            w.reported = None;
            Say::Moving(last_activity - b)
        }
        _ => Say::Nothing,
    }
}

/// "45 min", "2 h 5 min".
fn dur(secs: i64) -> String {
    let m = (secs / 60).max(1);
    if m < 60 {
        format!("{m} min")
    } else if m % 60 == 0 {
        format!("{} h", m / 60)
    } else {
        format!("{} h {} min", m / 60, m % 60)
    }
}

pub fn inactivity_loop(engine: Arc<Engine>) {
    let mut watches: HashMap<String, Watch> = HashMap::new();
    let mut last_online: HashMap<String, i64> = HashMap::new();
    // Let cameras connect after a start before judging anything.
    thread::sleep(Duration::from_secs(60));
    loop {
        let now = Utc::now().timestamp();
        let local = chrono::Local::now();
        let minute = chrono::Timelike::hour(&local) * 60 + chrono::Timelike::minute(&local);
        let handles: Vec<Arc<CameraHandle>> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
        let mut seen = Vec::new();
        for h in handles {
            let cfg = h.running.read().unwrap().clone();
            let Some(limit) = cfg.no_motion_minutes.filter(|&m| m > 0) else { continue };
            let (online, sleeping, last) = {
                let st = h.state.read().unwrap();
                (st.online, st.sleeping, st.last_activity_at)
            };
            let window = cfg.no_motion_schedule.as_deref().and_then(crate::config::parse_schedule);
            if !h.active.load(Ordering::Relaxed) || !crate::config::schedule_active(window, minute) {
                continue;
            }
            // Offline for a moment (a reconnect): the watch carries on, not judged meanwhile.
            seen.push(cfg.name.clone());
            if !online || sleeping {
                // Away longer than a reconnect: start over when it is back.
                if last_online.get(&cfg.name).is_some_and(|t| now - t > 5 * 60) {
                    watches.remove(&cfg.name);
                }
                continue;
            }
            last_online.insert(cfg.name.clone(), now);
            // While alerts are paused nothing is judged, so nothing is lost:
            // a quiet stretch is reported once the pause ends.
            if engine.alerts_paused_until(&cfg.name).is_some() {
                continue;
            }
            let w = watches.entry(cfg.name.clone()).or_insert(Watch { since: now, reported: None });
            let (title, body, urgent) = match judge(w, now, last, i64::from(limit) * 60) {
                Say::Nothing => continue,
                Say::Quiet(q) => {
                    let from = chrono::Local::now() - chrono::Duration::seconds(q);
                    (format!("No motion at {} for {}", cfg.name, dur(q)), format!("Nothing has moved at {} since {}.", cfg.name, from.format("%-I:%M %p")), true)
                }
                Say::Moving(q) => (format!("Motion again at {}", cfg.name), format!("Something moved at {} after {} with no motion.", cfg.name, dur(q)), false),
            };
            info!("no-motion alert: {title}");
            let alerts = engine.config.lock().unwrap().alerts.clone();
            let title = format!("{}: {title}", crate::links::own_name(&engine));
            thread::spawn(move || {
                system_alerts::send_for(&alerts, &title, &body, urgent, &|ch| cfg.uses_channel(ch), Some(&cfg.name));
            });
        }
        // Offline, outside the window, or no longer watched: start over next time.
        watches.retain(|k, _| seen.contains(k));
        last_online.retain(|k, _| seen.contains(k));
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_then_moving_once_each() {
        let mut w = Watch { since: 1000, reported: None };
        assert_eq!(judge(&mut w, 1500, 0, 600), Say::Nothing);
        assert_eq!(judge(&mut w, 1600, 0, 600), Say::Quiet(600));
        assert_eq!(judge(&mut w, 1700, 0, 600), Say::Nothing);
        assert_eq!(judge(&mut w, 1800, 1750, 600), Say::Moving(750));
        assert_eq!(judge(&mut w, 1900, 1750, 600), Say::Nothing);
        // Quiet again, counted from the last movement.
        assert_eq!(judge(&mut w, 2350, 1750, 600), Say::Quiet(600));
    }

    #[test]
    fn movement_keeps_resetting_the_count() {
        let mut w = Watch { since: 0, reported: None };
        for t in (60..3600).step_by(60) {
            assert_eq!(judge(&mut w, t, t - 30, 600), Say::Nothing);
        }
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(dur(45 * 60), "45 min");
        assert_eq!(dur(120 * 60), "2 h");
        assert_eq!(dur(125 * 60), "2 h 5 min");
    }
}
