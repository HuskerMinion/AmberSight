//! Sharing camera streams with other apps (Home Assistant, Scrypted, VLC,
//! another NVR) over RTSP, so they use AmberSight as their video source
//! instead of connecting to the cameras themselves.
//!
//! The camera sessions already receive every frame; with sharing on they also
//! publish each frame to a `RestreamHub` (hub.rs), and the RTSP server
//! (server.rs) turns it into RTP for each connected client (rtp.rs) without
//! decoding or re-encoding anything. No extra connection to a camera is made.
//! Off by default: `[restream] enabled`, plus `share = true` on each camera.

pub mod hub;
pub mod http;
pub mod rtp;
pub mod server;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::engine::Engine;

fn default_port() -> u16 {
    8554
}

/// `[restream]`: the RTSP server for other apps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestreamConfig {
    /// Share the cameras that have `share = true`. Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// The RTSP port (default 8554).
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for RestreamConfig {
    fn default() -> Self {
        RestreamConfig { enabled: false, port: default_port() }
    }
}

impl RestreamConfig {
    pub fn is_default(&self) -> bool {
        *self == RestreamConfig::default()
    }

    /// The port must be one this server can listen on besides its others.
    pub fn check_port(port: u16, server: &crate::config::ServerConfig) -> Result<(), String> {
        if port == 0 {
            return Err("the RTSP port must be between 1 and 65535".into());
        }
        if port == server.port || (server.rtmp_port != 0 && port == server.rtmp_port) {
            return Err(format!("port {port} is already used by AmberSight (web page or RTMP input); choose another"));
        }
        Ok(())
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether sharing is switched on (what new camera handles start with).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Make every camera's hubs follow the settings: shared while sharing is on
/// and the camera's switch is on, otherwise stopped (clients are cut off).
pub fn refresh(engine: &Engine) {
    let on = engine.config.lock().unwrap().restream.enabled;
    ENABLED.store(on, Ordering::Relaxed);
    let cams: Vec<_> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
    for h in cams {
        let share = { let c = h.cfg.read().unwrap(); c.share && http::blocker(&c).is_none() };
        h.restream_main.set_wanted(on && share);
        h.restream_sub.set_wanted(on && share);
    }
}

/// Start the RTSP server task (it opens the port only while sharing is on).
pub fn start(engine: Arc<Engine>) {
    refresh(&engine);
    tokio::spawn(server::run(engine));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_not_written() {
        let c = RestreamConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.port, 8554);
        assert!(c.is_default());
        let from_empty: RestreamConfig = toml::from_str("").unwrap();
        assert_eq!(from_empty, c);
        let on: RestreamConfig = toml::from_str("enabled = true").unwrap();
        assert!(on.enabled && on.port == 8554 && !on.is_default());
    }

    #[test]
    fn port_must_be_free_of_ours() {
        let s = crate::config::ServerConfig::default();
        assert!(RestreamConfig::check_port(8554, &s).is_ok());
        assert!(RestreamConfig::check_port(0, &s).is_err());
        assert!(RestreamConfig::check_port(s.port, &s).is_err());
        assert!(RestreamConfig::check_port(s.rtmp_port, &s).is_err());
    }
}
