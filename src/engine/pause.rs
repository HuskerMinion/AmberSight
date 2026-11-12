//! Arm delay (the shield turns amber and arms a little later, so you can
//! leave first) and per-camera alert pauses (notifications for one camera
//! stop for a while; motion, recording and events carry on).

use super::*;
use std::sync::atomic::Ordering;

/// Taken while the shield changes, so a finishing arm delay and a disarm
/// cannot interleave.
pub(super) static ARMING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// ui_kv scope that keeps the pauses across restarts.
const SCOPE: &str = "alert_pause";

/// Whether a pause that ends at `until` is still running at `now`.
fn running(until: i64, now: i64) -> bool {
    until > now
}

impl Engine {
    /// Whether motion detection is armed now. Finishes a due arm delay.
    pub fn armed_now(&self) -> bool {
        if self.shield.load(Ordering::Relaxed) {
            return true;
        }
        let at = self.arm_at.load(Ordering::Relaxed);
        if at > 0 && Utc::now().timestamp() >= at {
            let done = {
                let _g = ARMING.lock().unwrap();
                // Still due (not cancelled or disarmed meanwhile)?
                if self.arm_at.load(Ordering::Relaxed) == at {
                    self.arm_at.store(0, Ordering::Relaxed);
                    self.shield.store(true, Ordering::Relaxed);
                    Some(self.arm_by.lock().unwrap().clone())
                } else {
                    None
                }
            };
            if let Some(by) = done {
                self.record_change(&by, "shield: armed", "arm delay over");
                return true;
            }
            return self.shield.load(Ordering::Relaxed);
        }
        false
    }

    /// When a running arm delay ends (seconds), if one is running.
    pub fn arming_until(&self) -> Option<i64> {
        let at = self.arm_at.load(Ordering::Relaxed);
        (at > 0 && !self.shield.load(Ordering::Relaxed) && !self.armed_now()).then_some(at)
    }

    /// Arm after `secs` seconds (at once when 0). Until then motion
    /// detection stays off. Returns when it arms.
    pub fn arm_after(&self, secs: u32, by: &str) -> i64 {
        let now = Utc::now().timestamp();
        if secs == 0 {
            self.set_shield(true, by, "");
            return now;
        }
        let at = now + i64::from(secs);
        {
            // (Under the arm delay's lock, with who started it, so a delay
            // ending meanwhile records the right one.)
            let _g = ARMING.lock().unwrap();
            // Already armed: arming again changes nothing.
            if self.shield.load(Ordering::Relaxed) {
                return now;
            }
            *self.arm_by.lock().unwrap() = by.to_string();
            self.arm_at.store(at, Ordering::Relaxed);
        }
        info!("shield arms in {secs} s (by {by})");
        at
    }

    /// Read the pauses kept in the database (at start).
    pub(super) fn load_alert_pauses(&self) {
        let now = Utc::now().timestamp();
        let rows = self.db.ui_scope(SCOPE).unwrap_or_default();
        let mut m = self.alert_pauses.lock().unwrap();
        for (cam, v) in rows {
            if let Ok(until) = v.parse::<i64>() {
                if running(until, now) {
                    m.insert(cam, until);
                }
            }
        }
    }

    /// Pause notifications for camera `name`: for `minutes`, or until
    /// resumed when None. `Some(0)` resumes. Returns the end (seconds;
    /// i64::MAX = until resumed), or None when resumed.
    pub fn pause_alerts(&self, name: &str, minutes: Option<u32>) -> Option<i64> {
        let until = match minutes {
            Some(0) => None,
            Some(m) => Some(Utc::now().timestamp() + i64::from(m.min(7 * 24 * 60)) * 60),
            None => Some(i64::MAX),
        };
        let mut m = self.alert_pauses.lock().unwrap();
        match until {
            Some(u) => {
                m.insert(name.to_string(), u);
                let _ = self.db.ui_put(SCOPE, name, &u.to_string());
                info!("alerts paused for {name} {}", if u == i64::MAX { "until resumed".to_string() } else { format!("for {} min", minutes.unwrap_or(0)) });
            }
            None => {
                if m.remove(name).is_some() {
                    info!("alerts resumed for {name}");
                }
                // An ended pause is stored as 0 (ui_kv has no delete).
                let _ = self.db.ui_put(SCOPE, name, "0");
            }
        }
        until
    }

    /// When camera `name`'s alert pause ends (i64::MAX = until resumed), if
    /// one is running. A pause that has run out is dropped.
    pub fn alerts_paused_until(&self, name: &str) -> Option<i64> {
        let now = Utc::now().timestamp();
        let mut m = self.alert_pauses.lock().unwrap();
        match m.get(name).copied() {
            Some(u) if running(u, now) => Some(u),
            Some(_) => {
                m.remove(name);
                info!("alert pause for {name} is over");
                None
            }
            None => None,
        }
    }

    /// Carry a pause across a camera rename.
    pub fn rename_alert_pause(&self, old: &str, new: &str) {
        let u = self.alert_pauses.lock().unwrap().get(old).copied();
        if let Some(u) = u {
            self.pause_alerts(old, Some(0));
            self.alert_pauses.lock().unwrap().insert(new.to_string(), u);
            let _ = self.db.ui_put(SCOPE, new, &u.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::running;

    #[test]
    fn pause_runs_until_its_end() {
        assert!(running(100, 99));
        assert!(!running(100, 100));
        assert!(running(i64::MAX, 1_900_000_000));
    }
}
