//! Camera tamper detection: watches each camera that has it turned on for a
//! covered, blurred or moved view (the picture maths is in crate::tamper).
//!
//! Every few seconds one of the camera's analysis pictures (the same ones
//! motion detection uses) is compared with the camera's reference picture
//! for the same light: one by day (colour) and one by night (grey, infrared).
//! References are taken by themselves when the scene has been still for a
//! while, refreshed slowly (so the daylight and the seasons can move on),
//! and can be set by hand. A wrong view that lasts the camera's set time
//! sends one alert with the reference and the current picture; when the view
//! is right again for a while, "view restored".

use super::*;
use crate::tamper::{Change, Feat, Judge, Kind, Light, Verdict};
use std::collections::{HashMap, VecDeque};

/// Seconds between two looks at a camera.
const INTERVAL: i64 = 5;
/// A reference older than this is replaced when the scene is still and fine.
const REFRESH_SECS: i64 = 6 * 3600;
/// Still pictures needed (one per look) before a reference is taken.
const STEADY_LOOKS: usize = 6;
/// Alerts (with their pictures) kept for each camera.
const KEEP_ALERTS: usize = 30;

struct Ref {
    feat: Feat,
    jpeg: Vec<u8>,
    set_at: i64,
}

/// What is known about one watched camera.
#[derive(Default)]
pub struct Mon {
    last_look: i64,
    loaded: bool,
    refs: HashMap<Light, Ref>,
    /// The latest still-looking pictures of one light.
    recent: VecDeque<(i64, Feat, Vec<u8>)>,
    judge: Judge,
    /// The logged alert that is on.
    incident: Option<i64>,
    last: Option<(i64, Light, Verdict)>,
}

pub type Board = Mutex<HashMap<usize, Mon>>;

/// What a look decided to do once the board is unlocked.
enum Action {
    Raise { kind: Kind, light: Light, cur: Vec<u8>, reference: Vec<u8>, since: i64 },
    Clear { incident: Option<i64> },
    Save { light: Light, jpeg: Vec<u8> },
}

fn dir(engine: &Engine) -> std::path::PathBuf {
    engine.data_dir.join("tamper")
}

fn ref_path(engine: &Engine, name: &str, light: Light) -> std::path::PathBuf {
    dir(engine).join(format!("{}-{}.jpg", slug(name), light.word()))
}

fn incident_path(engine: &Engine, id: i64, which: &str) -> std::path::PathBuf {
    dir(engine).join(format!("inc-{id}-{which}.jpg"))
}

fn load_refs(engine: &Engine, name: &str, m: &mut Mon) {
    m.loaded = true;
    for light in [Light::Day, Light::Night] {
        let p = ref_path(engine, name, light);
        let Ok(jpeg) = std::fs::read(&p) else { continue };
        let Some(feat) = crate::tamper::features_jpeg(&jpeg) else { continue };
        let set_at = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
        m.refs.insert(light, Ref { feat, jpeg, set_at });
    }
}

fn save_ref(engine: &Engine, name: &str, light: Light, jpeg: &[u8]) {
    let d = dir(engine);
    if std::fs::create_dir_all(&d).is_err() || std::fs::write(ref_path(engine, name, light), jpeg).is_err() {
        warn!("camera '{name}': could not save the {} reference picture", light.word());
    }
}

impl Engine {
    /// Called with each analysis picture of a camera; looks at one every few
    /// seconds, and only on cameras that have tamper detection on.
    pub(super) fn tamper_feed(self: &Arc<Self>, handle: &Arc<CameraHandle>) {
        let tc = handle.running.read().unwrap().tamper.clone().filter(|t| t.enabled);
        let Some(tc) = tc else {
            // Turned off: forget what was known.
            let gone = self.tamper.lock().unwrap().remove(&handle.id);
            if let Some(m) = gone {
                self.tamper_forget(m);
            }
            return;
        };
        let now = Utc::now().timestamp();
        {
            let mut b = self.tamper.lock().unwrap();
            let m = b.entry(handle.id).or_default();
            if now - m.last_look < INTERVAL {
                return;
            }
            m.last_look = now;
        }
        // (Only once per look, not for every frame.)
        let cfg = handle.running.read().unwrap().clone();
        let frame = handle.state.read().unwrap().latest_frame.clone();
        let frame = frame.as_slice();
        let Some(feat) = crate::tamper::features_jpeg(frame) else { return };
        let moving = {
            let st = handle.state.read().unwrap();
            st.motion || now - st.last_motion_at < 20
        };
        let action = {
            let mut b = self.tamper.lock().unwrap();
            let m = b.entry(handle.id).or_default();
            if !m.loaded {
                load_refs(self, &cfg.name, m);
            }
            self.tamper_look(m, feat, frame, moving, now, tc.sensitivity(), tc.seconds())
        };
        match action {
            None => {}
            Some(Action::Save { light, jpeg }) => save_ref(self, &cfg.name, light, &jpeg),
            Some(Action::Clear { incident }) => self.tamper_cleared(handle, &cfg, incident, now),
            Some(Action::Raise { kind, light, cur, reference, since }) => self.tamper_raised(handle, &cfg, kind, light, cur, reference, since),
        }
    }

    /// One look: judge the picture, take or refresh references.
    #[allow(clippy::too_many_arguments)]
    fn tamper_look(&self, m: &mut Mon, feat: Feat, frame: &[u8], moving: bool, now: i64, sensitivity: u8, seconds: i64) -> Option<Action> {
        let light = feat.light;
        // The reference of the same light; without one, a covered lens can
        // still be told from whichever reference exists.
        let (verdict, ref_light) = match m.refs.get(&light) {
            Some(r) => (Some(crate::tamper::compare(&r.feat, &feat, sensitivity)), light),
            None => match m.refs.iter().next() {
                Some((l, r)) => (Some(crate::tamper::compare_covered_only(&r.feat, &feat, sensitivity)), *l),
                None => (None, light),
            },
        };
        // Someone walking about is not a moved camera.
        let bad = verdict.and_then(|v| v.bad()).filter(|k| !(moving && *k == Kind::Moved));
        if let Some(v) = verdict {
            m.last = Some((now, light, v));
        }
        let mut action = None;
        if verdict.is_some() {
            match m.judge.update(now, bad, seconds) {
                Change::Raised(kind) => {
                    let since = now - m.judge.bad_for(now).unwrap_or(0);
                    let reference = m.refs.get(&ref_light).map(|r| r.jpeg.clone()).unwrap_or_default();
                    action = Some(Action::Raise { kind, light, cur: frame.to_vec(), reference, since });
                }
                Change::Cleared => action = Some(Action::Clear { incident: m.incident.take() }),
                Change::Nothing => {}
            }
        }
        // Reference pictures: only from a still scene, never while something is wrong.
        if m.recent.back().is_some_and(|(_, f, _)| f.light != light) {
            m.recent.clear();
        }
        if crate::tamper::has_detail(&feat) && !moving && bad.is_none() && m.judge.active.is_none() && m.judge.bad_for(now).is_none() {
            m.recent.push_back((now, feat, frame.to_vec()));
            while m.recent.len() > STEADY_LOOKS {
                m.recent.pop_front();
            }
        } else {
            m.recent.clear();
        }
        if action.is_none() && m.recent.len() == STEADY_LOOKS {
            let feats: Vec<&Feat> = m.recent.iter().map(|(_, f, _)| f).collect();
            let still = crate::tamper::steady(&feats);
            let due = match m.refs.get(&light) {
                None => true,
                Some(r) => now - r.set_at >= REFRESH_SECS && crate::tamper::same_scene(&r.feat, feats[feats.len() - 1]),
            };
            if still && due {
                let (_, f, jpeg) = m.recent.back().cloned().unwrap();
                m.refs.insert(light, Ref { feat: f, jpeg: jpeg.clone(), set_at: now });
                m.recent.clear();
                action = Some(Action::Save { light, jpeg });
            }
        }
        action
    }

    fn tamper_raised(self: &Arc<Self>, handle: &Arc<CameraHandle>, cfg: &CameraConfig, kind: Kind, light: Light, cur: Vec<u8>, reference: Vec<u8>, since: i64) {
        let name = cfg.name.clone();
        let id = match self.db.tamper_add(&name, since, kind.word(), light.word()) {
            Ok(id) => id,
            Err(e) => {
                warn!("camera '{name}': could not log the tamper alert: {e:#}");
                return;
            }
        };
        let _ = std::fs::create_dir_all(dir(self));
        let _ = std::fs::write(incident_path(self, id, "cur"), &cur);
        let _ = std::fs::write(incident_path(self, id, "ref"), &reference);
        if let Some(m) = self.tamper.lock().unwrap().get_mut(&handle.id) {
            m.incident = Some(id);
        }
        if let Ok(old) = self.db.tamper_prune(KEEP_ALERTS) {
            for o in old {
                let _ = std::fs::remove_file(incident_path(self, o, "cur"));
                let _ = std::fs::remove_file(incident_path(self, o, "ref"));
            }
        }
        info!("camera '{name}': tamper alert: {}", kind.sentence(&name));
        let at = chrono::DateTime::<Utc>::from_timestamp(since, 0).map(|t| t.with_timezone(&chrono::Local).format("%-I:%M %p").to_string()).unwrap_or_default();
        let body = format!("Since {at}. The picture is not what this camera normally shows ({}). Open the camera's settings to compare it with the reference picture.", kind.word());
        let pic = incident_path(self, id, "cur");
        let url = format!("/api/cameras/{}/tamper/incidents/{id}/cur", handle.id);
        self.tamper_notify(cfg, format!("Camera tampering: {}", kind.sentence(&name)), body, true, Some((pic, url)));
    }

    fn tamper_cleared(self: &Arc<Self>, _handle: &Arc<CameraHandle>, cfg: &CameraConfig, incident: Option<i64>, now: i64) {
        if let Some(id) = incident {
            let _ = self.db.tamper_cleared(id, now);
        }
        info!("camera '{}': tamper alert over: view restored", cfg.name);
        self.tamper_notify(cfg, format!("Camera view restored: {}", cfg.name), format!("{} shows its normal view again.", cfg.name), false, None);
    }

    /// Send through the camera's own notification settings: off when its
    /// alerts are off (or by the profile), paused, or the system is disarmed.
    fn tamper_notify(self: &Arc<Self>, cfg: &CameraConfig, title: String, body: String, urgent: bool, pic: Option<(std::path::PathBuf, String)>) {
        if cfg.alerts == Some(false) || !self.alerts_enabled(cfg) || self.alerts_paused_until(&cfg.name).is_some() || !self.armed_now() {
            tracing::debug!("tamper alert for {} not sent (alerts off, paused or disarmed)", cfg.name);
            return;
        }
        let alerts = self.config.lock().unwrap().alerts.clone();
        let title = format!("{}: {title}", crate::links::own_name(self));
        let cfg = cfg.clone();
        thread::spawn(move || {
            let pic = pic.as_ref().map(|(p, u)| (p.as_path(), u.as_str()));
            system_alerts::send_for_with(&alerts, &title, &body, urgent, &|ch| cfg.uses_channel(ch), Some(&cfg.name), pic);
        });
    }

    fn tamper_forget(&self, m: Mon) {
        if let Some(id) = m.incident {
            let _ = self.db.tamper_cleared(id, Utc::now().timestamp());
        }
    }

    /// Whether a tamper alert is on for this camera (MQTT, status).
    pub fn tamper_active(&self, id: usize) -> bool {
        self.tamper.lock().unwrap().get(&id).is_some_and(|m| m.judge.active.is_some())
    }

    /// Take the camera's picture of this moment as its reference (for the
    /// light it shows). Returns which reference it replaced.
    pub fn tamper_set_reference(&self, id: usize) -> Result<Light, String> {
        let h = self.camera(id).ok_or("no such camera")?;
        let name = h.running.read().unwrap().name.clone();
        let frame = h.state.read().unwrap().latest_frame.clone();
        if frame.is_empty() {
            return Err("the camera has no picture right now".into());
        }
        let feat = crate::tamper::features_jpeg(&frame).ok_or("the picture could not be read")?;
        if !crate::tamper::has_detail(&feat) {
            return Err("the picture shows too little to be a reference (too dark or blank)".into());
        }
        let light = feat.light;
        save_ref(self, &name, light, &frame);
        let mut b = self.tamper.lock().unwrap();
        let m = b.entry(id).or_default();
        if !m.loaded {
            load_refs(self, &name, m);
        }
        m.refs.insert(light, Ref { feat, jpeg: frame, set_at: Utc::now().timestamp() });
        m.recent.clear();
        if let Some(inc) = m.incident.take() {
            let _ = self.db.tamper_cleared(inc, Utc::now().timestamp());
        }
        m.judge.reset();
        Ok(light)
    }

    /// A camera's tamper detection, for its settings window.
    pub fn tamper_status(&self, id: usize) -> Option<serde_json::Value> {
        let h = self.camera(id)?;
        let cfg = h.running.read().unwrap().clone();
        let tc = cfg.tamper.clone().unwrap_or_default();
        let now = Utc::now().timestamp();
        let mut b = self.tamper.lock().unwrap();
        // The pictures are known once the camera has been watched; for a camera
        // not watched, say which are on disk.
        let on_disk = |light: Light| ref_path(self, &cfg.name, light).exists();
        let (refs, state, score, active, bad_for, light) = match b.get_mut(&id) {
            Some(m) => {
                if !m.loaded {
                    load_refs(self, &cfg.name, m);
                }
                let age = |l: Light| m.refs.get(&l).map(|r| serde_json::json!({ "set_at": r.set_at }));
                let state = match (m.judge.active, &m.last) {
                    (Some((_, k)), _) => k.word().to_string(),
                    (None, Some((t, ..))) if now - t < 120 => "ok".to_string(),
                    _ => "learning".to_string(),
                };
                let score = m.last.as_ref().map(|(_, _, v)| serde_json::json!(v));
                let active = m.judge.active.map(|(t, k)| serde_json::json!({ "since": t, "kind": k.word() }));
                (
                    serde_json::json!({ "day": age(Light::Day), "night": age(Light::Night) }),
                    state,
                    score,
                    active,
                    m.judge.bad_for(now),
                    m.last.as_ref().map(|(_, l, _)| l.word()),
                )
            }
            None => (
                serde_json::json!({ "day": on_disk(Light::Day).then(|| serde_json::json!({})), "night": on_disk(Light::Night).then(|| serde_json::json!({})) }),
                "learning".to_string(),
                None,
                None,
                None,
                None,
            ),
        };
        drop(b);
        let incidents: Vec<serde_json::Value> = self
            .db
            .tamper_list(&cfg.name, 10)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, at, kind, light, cleared)| serde_json::json!({ "id": id, "at": at, "kind": kind, "light": light, "cleared_at": cleared }))
            .collect();
        Some(serde_json::json!({
            "enabled": tc.enabled,
            "state": if tc.enabled { state } else { "off".to_string() },
            "refs": refs,
            "score": score,
            "active": active,
            "bad_for": bad_for,
            "light": light,
            "incidents": incidents,
        }))
    }

    /// A reference picture (JPEG) of a camera.
    pub fn tamper_ref_jpeg(&self, id: usize, light: Light) -> Option<Vec<u8>> {
        let h = self.camera(id)?;
        let name = h.running.read().unwrap().name.clone();
        std::fs::read(ref_path(self, &name, light)).ok()
    }

    /// A picture of a logged alert ("ref" or "cur") of this camera.
    pub fn tamper_incident_jpeg(&self, id: usize, incident: i64, which: &str) -> Option<Vec<u8>> {
        if which != "ref" && which != "cur" {
            return None;
        }
        let h = self.camera(id)?;
        let name = h.running.read().unwrap().name.clone();
        if self.db.tamper_owner(incident).ok().flatten()? != name {
            return None;
        }
        std::fs::read(incident_path(self, incident, which)).ok()
    }
}
