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
        self.set_signal_for(name, on, source, None)
    }

    /// Like `set_signal`, and says the signal belongs to `camera` (its lane
    /// then shows under that camera's timeline only).
    pub fn set_signal_for(&self, name: &str, on: bool, source: &str, camera: Option<&str>) -> bool {
        self.set_signal_checked(name, on, source, camera).is_ok()
    }

    /// As [`Engine::set_signal_for`], saying why a signal was not saved.
    pub fn set_signal_checked(&self, name: &str, on: bool, source: &str, camera: Option<&str>) -> Result<(), SignalRefused> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(|c| c.is_control()) {
            return Err(SignalRefused::BadName);
        }
        let cam = camera.map(str::trim).filter(|c| !c.is_empty() && c.chars().count() <= MAX_NAME);
        match self.db.set_signal(name, Utc::now().timestamp_millis(), on) {
            Ok(changed) => {
                if changed {
                    info!("signal '{name}': {} ({source})", if on { "on" } else { "off" });
                }
                // Only for a signal that exists (the cap is checked above).
                if let Some(cam) = cam {
                    if let Err(e) = self.db.set_signal_camera(name, cam) {
                        warn!("signal '{name}': could not save its camera: {e:#}");
                    }
                }
                Ok(())
            }
            Err(e) if e.downcast_ref::<crate::db::TooManySignals>().is_some() => {
                warn!("signal '{name}': {e}");
                Err(SignalRefused::TooMany)
            }
            Err(e) => {
                warn!("signal '{name}': could not save: {e:#}");
                Err(SignalRefused::Failed)
            }
        }
    }
}

/// Why a signal was not saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalRefused {
    /// Empty, too long, or with control characters.
    BadName,
    /// A new signal over the cap.
    TooMany,
    /// The database would not take it.
    Failed,
}
