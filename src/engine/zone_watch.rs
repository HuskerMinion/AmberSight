//! Named zones in the engine: loitering in areas and crossing of lines.
//!
//! Watching costs AI looks, so it is bounded: it runs only while the camera
//! is in a motion event (the checks the event makes anyway are reused) or,
//! after the motion has stopped, while someone is still standing inside an
//! area (for at most `ai.zone_hold_seconds`). Between the event's own checks
//! it takes its own looks, no closer together than `ai.zone_interval_ms` and
//! never more than about half of the time (a look that took 400 ms is
//! followed by at least 800 ms without one). Judging is in `crate::tracker`.

use super::*;
use crate::ai::Detection;
use crate::config::NamedZone;
use crate::tracker::{Hit, HitKind, ZoneWatch};
use std::time::{Duration, Instant};

/// What one camera's zone watching remembers (kept across events).
#[derive(Default)]
pub(super) struct Runtime {
    watch: ZoneWatch,
    last_detect: Option<Instant>,
    /// How long the last own look took (ms).
    took_ms: u64,
    /// Since when it has been watching on after the motion stopped.
    hold_since: Option<Instant>,
    seq: u32,
}

impl Runtime {
    fn reset(&mut self, handle: &CameraHandle) {
        self.watch.clear();
        self.hold_since = None;
        publish_occupied(handle, Vec::new());
    }
    fn idle(&self) -> bool {
        self.watch.tracker.tracks().is_empty() && !self.watch.someone_inside()
    }
}

/// The least time between two of our own looks: the setting, and twice what
/// a look takes (so the detector is busy for us at most half of the time).
/// With only already-reported loiterers left (watching for them to leave,
/// nothing else going on) looks come four times as far apart, 3 s at least.
pub(super) fn look_gap_ms(interval_ms: u32, took_ms: u64, idle: bool) -> u64 {
    let gap = u64::from(interval_ms.clamp(250, 5000)).max(took_ms.saturating_mul(2));
    if idle { (gap * 4).max(3000) } else { gap }
}

/// The zones watching now: enabled and inside their window.
fn watching_now(cfg: &CameraConfig, minute: u32) -> Vec<NamedZone> {
    cfg.named_zones
        .iter()
        .filter(|z| z.enabled)
        .filter(|z| z.schedule.as_deref().and_then(crate::config::parse_schedule).is_none_or(|w| crate::config::schedule_active(Some(w), minute)))
        .cloned()
        .collect()
}

/// Whether the camera has named zones that may need watching (cheap check
/// for the frame loop).
pub(super) fn has_zones(cfg: &CameraConfig) -> bool {
    cfg.named_zones.iter().any(|z| z.enabled)
}

fn publish_occupied(handle: &CameraHandle, mut names: Vec<String>) {
    names.sort();
    let mut st = handle.state.write().unwrap();
    if st.zones_occupied != names {
        st.zones_occupied = names;
    }
}

/// Called for every frame of a camera with AI on. `fed` is what the event's
/// own AI check saw in this frame, if it took one.
pub(super) fn on_frame(engine: &Engine, handle: &CameraHandle, cfg: &CameraConfig, ai: &mut super::ai_check::AiState, event_active: bool, fed: Option<Vec<Detection>>, jpeg: &[u8]) {
    let rt = &mut ai.zones;
    let local = chrono::Local::now();
    let zones = watching_now(cfg, local.hour() * 60 + local.minute());
    if zones.is_empty() || !engine.armed_now() {
        if !rt.idle() {
            rt.reset(handle);
        }
        return;
    }
    let (interval, hold_max, min_score) = {
        let c = engine.config.lock().unwrap();
        (c.ai.zone_interval_ms, u64::from(c.ai.zone_hold_seconds), cfg.ai_min_score.unwrap_or(c.ai.min_score))
    };
    // Motion going on, or someone still standing in an area (for a while).
    let watching = if event_active {
        rt.hold_since = None;
        true
    } else if rt.watch.someone_inside() {
        rt.hold_since.get_or_insert_with(Instant::now).elapsed().as_secs() < hold_max
    } else {
        false
    };
    if !watching {
        if !rt.idle() {
            rt.reset(handle);
        }
        return;
    }
    let dets: Vec<Detection> = match fed {
        Some(d) => {
            rt.last_detect = Some(Instant::now());
            d
        }
        None => {
            if rt.last_detect.is_some_and(|t| t.elapsed() < Duration::from_millis(look_gap_ms(interval, rt.took_ms, !event_active && !rt.watch.waiting_inside()))) {
                return;
            }
            let t0 = Instant::now();
            rt.last_detect = Some(t0);
            match engine.ai_detect(jpeg, min_score) {
                Ok(d) => {
                    rt.took_ms = t0.elapsed().as_millis() as u64;
                    d
                }
                Err(e) => {
                    tracing::debug!("camera '{}': zone look failed: {e:#}", cfg.name);
                    return;
                }
            }
        }
    };
    let dets: Vec<Detection> = dets.into_iter().filter(|d| d.score >= min_score).collect();
    // Vehicles that sit still for minutes are parked (the AI's memory of where
    // things are, also kept up by this look); zones may ignore them.
    let ignore_parked = cfg.ai_ignore_parked.unwrap_or_else(|| engine.config.lock().unwrap().ai.ignore_parked);
    let now = Instant::now();
    let parked: Vec<bool> = dets.iter().map(|d| ignore_parked && super::ai_check::is_vehicle(&d.label) && ai.parked.seen(d, now)).collect();
    if let Ok(r) = image::io::Reader::new(std::io::Cursor::new(jpeg)).with_guessed_format().and_then(|r| r.into_dimensions().map_err(std::io::Error::other)) {
        rt.watch.tracker.set_aspect(f64::from(r.0) / f64::from(r.1.max(1)));
    }
    let hits = rt.watch.step_parked(&zones, &dets, &parked, Utc::now().timestamp_millis());
    publish_occupied(handle, rt.watch.occupied().into_iter().collect());
    for hit in hits {
        let Some(z) = zones.iter().find(|z| z.name == hit.zone) else { continue };
        rt.seq += 1;
        announce(engine, handle, cfg, z, &hit, jpeg, rt.seq);
    }
}

/// An event for a loitering or a crossing: events, timeline, alerts, MQTT.
fn announce(engine: &Engine, handle: &CameraHandle, cfg: &CameraConfig, z: &NamedZone, hit: &Hit, jpeg: &[u8], seq: u32) {
    let at = Utc::now().timestamp();
    let (start, kind) = match hit.kind {
        HitKind::Loiter { .. } => ((hit.since_ms / 1000).min(at), "loitering"),
        HitKind::Cross { inward } => (at, if inward { "in" } else { "out" }),
    };
    let id = format!("{}-{}-zone{}", slug(&cfg.name), start, seq);
    let thumb_path = engine.data_dir.join("thumbs").join(format!("{id}.jpg"));
    let thumb = !jpeg.is_empty() && std::fs::write(&thumb_path, jpeg).is_ok();
    let tag = hit.tag();
    info!("camera '{}': {tag} ({})", cfg.name, hit.label);
    if let Some(m) = engine.mqtt.get() {
        m.publish_zone(&cfg.name, &z.name, kind, &hit.label, &id, at);
    }
    let ev = Event {
        id: id.clone(),
        camera_id: handle.id,
        camera: cfg.name.clone(),
        start,
        end: at,
        score: 1.0,
        clip: z.record.then(|| format!("/api/clips/{id}")),
        thumb: thumb.then(|| id.clone()),
        objects: vec![tag, hit.label.clone()],
        protect: false,
    };
    let alert = z.alert && engine.alerts_enabled(cfg);
    if z.record {
        engine.record_event(ev, alert);
    } else if alert {
        // Not kept: the notification only. Its picture goes after a while.
        engine.fire_alert(&ev);
        if thumb {
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(600));
                let _ = std::fs::remove_file(thumb_path);
            });
        }
    } else if thumb {
        let _ = std::fs::remove_file(thumb_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_keep_the_gap_and_leave_the_detector_half_free() {
        assert_eq!(look_gap_ms(700, 100, false), 700);
        assert_eq!(look_gap_ms(700, 400, false), 800, "a slow detector is asked less often");
        assert_eq!(look_gap_ms(10, 0, false), 250, "never faster than four looks a second");
        assert_eq!(look_gap_ms(60_000, 0, false), 5000);
        assert_eq!(look_gap_ms(700, 100, true), 3000, "only waiting for a loiterer to leave: slow");
        assert_eq!(look_gap_ms(1500, 100, true), 6000);
    }

    #[test]
    fn schedule_picks_the_zones_watching_now() {
        let mk = |name: &str, enabled: bool, sched: Option<&str>| NamedZone {
            name: name.into(),
            kind: "area".into(),
            points: vec![[0.1, 0.1], [0.5, 0.1], [0.5, 0.5]],
            enabled,
            labels: vec!["person".into()],
            seconds: 30,
            direction: "both".into(),
            alert: true,
            record: true,
            ignore_parked: true,
            schedule: sched.map(String::from),
        };
        let mut cfg: CameraConfig = toml::from_str("name = \"Yard\"\nurl = \"rtsp://x/y\"\n").unwrap();
        cfg.named_zones = vec![mk("Always", true, None), mk("Off", false, None), mk("Night", true, Some("20:00-06:00")), mk("Day", true, Some("08:00-18:00"))];
        let names = |minute: u32| watching_now(&cfg, minute).into_iter().map(|z| z.name).collect::<Vec<_>>();
        assert_eq!(names(12 * 60), vec!["Always", "Day"]);
        assert_eq!(names(23 * 60), vec!["Always", "Night"]);
        assert!(has_zones(&cfg));
        cfg.named_zones.truncate(2);
        cfg.named_zones.remove(0);
        assert!(!has_zones(&cfg));
    }
}
