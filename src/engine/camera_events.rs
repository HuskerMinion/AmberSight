//! Camera-side detection (ONVIF events): motion, person, vehicle, animal and
//! doorbell presses reported by the camera itself. Depending on the camera's
//! `trigger` setting they start events ("camera"), add to AmberSight's own
//! motion detection ("both"), or only label events ("video", the default).

use super::*;
use crate::onvif::{EventKind, OnvifEvent, OnvifEventSource};

/// What the camera currently reports: topic -> (label, active).
#[derive(Default, Debug, Clone)]
pub struct CameraDetections {
    topics: std::collections::HashMap<String, (EventKind, bool)>,
    /// A synthetic motion pulse from a battery camera's push (pushx): counts as
    /// activity until this epoch millisecond. 0 = none.
    pulse_until_ms: i64,
}

impl CameraDetections {
    pub fn apply(&mut self, ev: &OnvifEvent) {
        self.topics.insert(ev.topic.clone(), (ev.kind.clone(), ev.active));
    }

    /// Count a battery camera's pushed PIR motion as activity for the next
    /// `seconds` (a further push extends it). This wakes an on-demand camera
    /// and, with trigger "camera"/"both", records the woken stream.
    /// Returns when the (possibly extended) pulse now ends, epoch ms.
    pub fn pulse(&mut self, seconds: u32) -> i64 {
        let until = chrono::Utc::now().timestamp_millis() + (seconds as i64) * 1000;
        self.pulse_until_ms = self.pulse_until_ms.max(until);
        self.pulse_until_ms
    }

    /// When the current pulse ends (epoch ms; 0 = never pulsed).
    pub fn pulse_until(&self) -> i64 {
        self.pulse_until_ms
    }

    /// A pushed PIR motion is active now (only the pulse, not camera topics).
    pub fn pulse_active(&self) -> bool {
        chrono::Utc::now().timestamp_millis() < self.pulse_until_ms
    }

    /// Anything the camera counts as activity is happening now.
    pub fn active(&self) -> bool {
        self.topics.values().any(|(k, a)| *a && !matches!(k, EventKind::Other(_)))
            || chrono::Utc::now().timestamp_millis() < self.pulse_until_ms
    }

    /// Labels of the object kinds currently detected (not plain motion).
    pub fn labels(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .topics
            .values()
            .filter(|(k, a)| *a && !matches!(k, EventKind::Motion | EventKind::Other(_)))
            .map(|(k, _)| k.label().to_string())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Subscribe to the camera's events for as long as this camera handle is
/// the active occupant of its slot.
pub(super) fn spawn_camera_events(engine: Arc<Engine>, handle: Arc<CameraHandle>, cfg: &CameraConfig) {
    if let Some(uid) = cfg.uid.clone().filter(|u| !u.is_empty() && cfg.url.starts_with("baichuan://")) {
        spawn_baichuan_poller(engine, handle, cfg, uid);
        return;
    }
    if let Some(ch) = cfg.reolink_channel {
        spawn_reolink_poller(engine, handle, cfg, ch);
        return;
    }
    let Some(port) = cfg.onvif_port else { return };
    let Some(host) = crate::onvif::host_from_rtsp_url(&cfg.url) else {
        warn!("camera '{}': cannot tell the camera's address from its URL; camera events off", cfg.name);
        return;
    };
    let (user, pass) = if cfg.username.is_empty() {
        crate::onvif::credentials_from_url(&cfg.url).unwrap_or_default()
    } else {
        (cfg.username.clone(), cfg.password.clone())
    };
    let name = cfg.name.clone();
    let only_source = cfg.onvif_source.clone().filter(|s| !s.is_empty());
    thread::spawn(move || {
        let stop = Arc::new(AtomicBool::new(false));
        let h = handle.clone();
        let cam = name.clone();
        let eng = engine.clone();
        let source = OnvifEventSource::spawn(
            host,
            port,
            user,
            pass,
            move |ev: OnvifEvent| {
                // Hubs report every channel on one connection: keep ours.
                if let Some(want) = &only_source {
                    if !ev.source.is_empty() && &ev.source != want {
                        return;
                    }
                }
                // A camera's digital input (a door contact, an alarm wired
                // to it): a signal lane named after the camera.
                if let EventKind::Other(t) = &ev.kind {
                    if t.contains("DigitalInput") {
                        eng.set_signal(&format!("{cam} input"), ev.active, "camera input");
                    }
                }
                let mut st = h.state.write().unwrap();
                st.camera_detections.apply(&ev);
                if !ev.initial {
                    tracing::debug!("camera '{cam}': camera reports {} {}", ev.kind.label(), if ev.active { "on" } else { "off" });
                }
            },
            stop.clone(),
        );
        info!("camera '{name}': listening for camera events (ONVIF port {port})");
        // Stop when the camera is disabled, removed or restarted.
        loop {
            thread::sleep(std::time::Duration::from_secs(1));
            let current = engine.camera(handle.id).is_some_and(|c| Arc::ptr_eq(&c, &handle));
            if !current || !handle.active.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
                let _ = source.join();
                return;
            }
        }
    });
}

/// A battery camera's charge as it last reported it (`GET /api/cameras`
/// `battery`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct BatteryLevel {
    /// 0 to 100.
    pub percent: u8,
    pub charging: bool,
    /// When it was read (epoch ms).
    pub at_ms: i64,
}

impl BatteryLevel {
    /// None: the report has no usable level (a negative percentage).
    pub fn of(b: &crate::ingest::baichuan::client::Battery, at_ms: i64) -> Option<BatteryLevel> {
        (b.percent >= 0).then(|| BatteryLevel { percent: b.percent.min(100) as u8, charging: b.charging, at_ms })
    }
}

/// Keep a battery report as the camera's latest level.
pub fn note_battery(handle: &CameraHandle, b: &crate::ingest::baichuan::client::Battery, at_ms: i64) {
    if let Some(level) = BatteryLevel::of(b, at_ms) {
        handle.state.write().unwrap().battery = Some(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_battery_report_is_kept_as_the_latest_level() {
        use crate::ingest::baichuan::client::Battery;
        let engine = Engine::for_test(toml::from_str("").unwrap());
        let h = engine.add_test_camera("Garden");
        assert_eq!(h.state.read().unwrap().battery, None, "unknown until the camera says");
        note_battery(&h, &Battery { percent: 87, charging: true, ..Default::default() }, 1000);
        assert_eq!(h.state.read().unwrap().battery, Some(BatteryLevel { percent: 87, charging: true, at_ms: 1000 }));
        // A report without a level keeps the last one; one past 100 is 100.
        note_battery(&h, &Battery { percent: -1, ..Default::default() }, 2000);
        assert_eq!(h.state.read().unwrap().battery.unwrap().at_ms, 1000);
        note_battery(&h, &Battery { percent: 140, charging: false, ..Default::default() }, 3000);
        assert_eq!(h.state.read().unwrap().battery, Some(BatteryLevel { percent: 100, charging: false, at_ms: 3000 }));
    }

    fn ev(kind: EventKind, topic: &str, active: bool) -> OnvifEvent {
        OnvifEvent { kind, active, topic: topic.into(), utc_time: None, initial: false, source: String::new() }
    }

    #[test]
    fn tracks_active_detections_and_labels() {
        let mut d = CameraDetections::default();
        d.apply(&ev(EventKind::Motion, "m", false));
        d.apply(&ev(EventKind::Other("pkg".into()), "p", true));
        assert!(!d.active(), "unknown topics do not count as activity");
        d.apply(&ev(EventKind::Person, "people", true));
        d.apply(&ev(EventKind::Visitor, "visitor", true));
        assert!(d.active());
        assert_eq!(d.labels(), vec!["person".to_string(), "visitor".to_string()]);
        d.apply(&ev(EventKind::Person, "people", false));
        d.apply(&ev(EventKind::Visitor, "visitor", false));
        assert!(!d.active() && d.labels().is_empty());
    }

    #[test]
    fn a_push_pulse_counts_as_activity_and_a_later_push_extends_it() {
        let mut d = CameraDetections::default();
        assert!(!d.pulse_active() && !d.active() && d.pulse_until() == 0);
        let first = d.pulse(20);
        assert!(d.pulse_active() && d.active(), "a push counts as camera activity");
        assert!(d.labels().is_empty(), "a push is plain motion, not an object label");
        assert_eq!(d.pulse_until(), first);
        // A later, longer push moves the end: the first push's clear no longer owns it.
        let second = d.pulse(40);
        assert!(second > first && d.pulse_until() == second);
        // A shorter push never cuts an existing window short.
        assert_eq!(d.pulse(1), second);
    }
}

/// Poll a Reolink hub/recorder channel about once a second: motion, AI
/// detections and battery sleep state, per channel.
fn spawn_reolink_poller(engine: Arc<Engine>, handle: Arc<CameraHandle>, cfg: &CameraConfig, channel: u32) {
    let Some(host) = crate::onvif::host_from_rtsp_url(&cfg.url) else {
        warn!("camera '{}': cannot tell the camera's address from its URL; Reolink events off", cfg.name);
        return;
    };
    let (user, pass) = if cfg.username.is_empty() {
        crate::onvif::credentials_from_url(&cfg.url).unwrap_or_default()
    } else {
        (cfg.username.clone(), cfg.password.clone())
    };
    let name = cfg.name.clone();
    // Counted with the camera sessions so shutdown waits for the sign-out
    // (devices allow only a few signed-in sessions; a token left behind
    // holds one for an hour).
    engine.live_sessions.fetch_add(1, Ordering::Relaxed);
    thread::spawn(move || {
        struct Done(Arc<Engine>);
        impl Drop for Done {
            fn drop(&mut self) {
                self.0.live_sessions.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _done = Done(engine.clone());
        let pause = |d: std::time::Duration| {
            let until = std::time::Instant::now() + d;
            while std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
                thread::sleep(std::time::Duration::from_millis(100));
            }
        };
        let mut client = crate::reolink::ReolinkClient::new(&host, &user, &pass);
        let mut last: Option<crate::reolink::ChannelState> = None;
        let mut failing_since: Option<std::time::Instant> = None;
        info!("camera '{name}': reading motion and AI state from {host} channel {channel}");
        loop {
            let current = engine.camera(handle.id).is_some_and(|c| Arc::ptr_eq(&c, &handle));
            if !current || !handle.active.load(Ordering::Relaxed) {
                client.logout();
                return;
            }
            match client.channel_state(channel) {
                Ok(s) => {
                    if failing_since.take().is_some() {
                        info!("camera '{name}': Reolink state readable again");
                    }
                    if last.as_ref() != Some(&s) {
                        let mut st = handle.state.write().unwrap();
                        for (kind, topic, on) in [
                            (EventKind::Motion, "reolink/motion", s.motion),
                            (EventKind::Person, "reolink/people", s.people),
                            (EventKind::Vehicle, "reolink/vehicle", s.vehicle),
                            (EventKind::Animal, "reolink/animal", s.animal),
                        ] {
                            st.camera_detections.apply(&OnvifEvent {
                                kind,
                                active: on,
                                topic: topic.into(),
                                utc_time: None,
                                initial: false,
                                source: String::new(),
                            });
                        }
                        st.device_sleeping = s.sleeping;
                        last = Some(s);
                    }
                    pause(std::time::Duration::from_millis(1000));
                }
                Err(e) => {
                    if failing_since.is_none() {
                        warn!("camera '{name}': cannot read Reolink state ({e:#}); retrying");
                        failing_since = Some(std::time::Instant::now());
                    }
                    pause(std::time::Duration::from_secs(5));
                }
            }
        }
    });
}

/// Keep a persistent Baichuan session to a battery Argus (no hub): it reports
/// motion and battery and streams video on demand. Registers the client so the
/// ingest reader (`recorder::open_stream`) can pull frames from the same
/// session. The hub-less counterpart of `spawn_reolink_poller`.
fn spawn_baichuan_poller(engine: Arc<Engine>, handle: Arc<CameraHandle>, cfg: &CameraConfig, uid: String) {
    let Some((host, _)) = crate::ingest::baichuan::parse_url(&cfg.url) else {
        warn!("camera '{}': a battery Argus needs a baichuan://<host> URL; events off", cfg.name);
        return;
    };
    let (user, pass) = (cfg.username.clone(), cfg.password.clone());
    let name = cfg.name.clone();
    if cfg.overlay || !cfg.privacy_masks.is_empty() {
        warn!(
            "camera '{name}': privacy masks and burned-in text are not applied to battery Argus video \
             (its recordings and live view show the whole picture)"
        );
    }
    // Counted with the camera sessions so shutdown waits for the sign-out.
    engine.live_sessions.fetch_add(1, Ordering::Relaxed);
    thread::spawn(move || {
        struct Done(Arc<Engine>);
        impl Drop for Done {
            fn drop(&mut self) {
                self.0.live_sessions.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _done = Done(engine.clone());
        // (Here, not before the spawn: the caller holds the camera list's write lock.)
        let dup = engine.cameras.read().unwrap().iter().flatten().any(|h| {
            h.id != handle.id && h.cfg.read().unwrap().uid.as_deref() == Some(uid.as_str())
        });
        if dup {
            warn!("camera '{name}': another camera has the same uid; only one of them will work");
        }
        use crate::ingest::baichuan::client::{BaichuanClient, CamEvent, MotionKind};
        use crate::ingest::baichuan::registry;

        let current = || engine.camera(handle.id).is_some_and(|c| Arc::ptr_eq(&c, &handle));
        // Stop the session and wait (a few seconds at most) for its sign-out,
        // so a battery camera is not left awake by a shutdown or edit.
        let stop = |client: &Arc<BaichuanClient>| {
            registry::unregister(&uid, client);
            client.shutdown();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while client.is_connected() && std::time::Instant::now() < until {
                thread::sleep(std::time::Duration::from_millis(100));
            }
        };
        'session: loop {
        let client = BaichuanClient::spawn(host.clone(), uid.clone(), user.clone(), pass.clone());
        registry::register(&uid, client.clone());
        info!("camera '{name}': battery Argus session on {host}");
        loop {
            if !current() {
                stop(&client);
                return;
            }
            if !handle.active.load(Ordering::Relaxed) {
                // Turned off: end the session, and start a new one if the
                // camera is turned back on (the video reader needs it).
                // While parked it holds no camera session, so it does not
                // count as one (shutdown turns every camera off and waits for
                // the count to reach zero).
                stop(&client);
                engine.live_sessions.fetch_sub(1, Ordering::Relaxed);
                while !handle.active.load(Ordering::Relaxed) {
                    if !current() {
                        engine.live_sessions.fetch_add(1, Ordering::Relaxed); // _done takes it off again
                        return;
                    }
                    thread::sleep(std::time::Duration::from_secs(1));
                }
                engine.live_sessions.fetch_add(1, Ordering::Relaxed);
                continue 'session;
            }
            match client.next_event(std::time::Duration::from_secs(1)) {
                Some(CamEvent::Motion { kind, active }) => {
                    let (ek, topic) = match kind {
                        MotionKind::Motion => (EventKind::Motion, "baichuan/motion"),
                        MotionKind::Person => (EventKind::Person, "baichuan/people"),
                        MotionKind::Vehicle => (EventKind::Vehicle, "baichuan/vehicle"),
                        MotionKind::Animal => (EventKind::Animal, "baichuan/animal"),
                    };
                    let mut st = handle.state.write().unwrap();
                    st.camera_detections.apply(&OnvifEvent {
                        kind: ek,
                        active,
                        topic: topic.into(),
                        utc_time: None,
                        initial: false,
                        source: String::new(),
                    });
                }
                Some(CamEvent::Connected(c)) => {
                    // While our session holds the camera it is effectively awake.
                    handle.state.write().unwrap().device_sleeping = Some(!c);
                }
                Some(CamEvent::Battery(b)) => {
                    tracing::debug!(
                        "camera '{name}': battery {}% adapter={} {}mV {}C",
                        b.percent, b.adapter, b.voltage_mv, b.temp_c
                    );
                    note_battery(&handle, &b, Utc::now().timestamp_millis());
                }
                None => {}
            }
        }
        }
    });
}
