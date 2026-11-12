//! Signals: named on/off states from outside the cameras (a door contact, an
//! alarm, a camera's own input), recorded with each change and shown as lanes
//! on the timeline. Set over HTTP (`POST /api/signals`), MQTT
//! (`<prefix>/signal/<name>/set`) or by a camera's digital input (ONVIF).

use super::*;

/// Names up to this long.
pub const MAX_NAME: usize = 60;

impl Engine {
    /// Signal `name` is now `on` (or off). Returns false for a bad name.
    pub fn set_signal(&self, name: &str, on: bool, source: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(|c| c.is_control()) {
            return false;
        }
        match self.db.set_signal(name, Utc::now().timestamp_millis(), on) {
            Ok(true) => info!("signal '{name}': {} ({source})", if on { "on" } else { "off" }),
            Ok(false) => {}
            Err(e) => {
                warn!("signal '{name}': could not save: {e:#}");
                return false;
            }
        }
        true
    }
}
