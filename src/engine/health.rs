//! Health report: camera connection state, recording activity, disk space,
//! and plain-language warnings (`GET /api/health`).

use super::*;

impl Engine {
    pub fn health(self: &Arc<Engine>) -> serde_json::Value {
        let now = Utc::now().timestamp();
        let seg_secs = self.config.lock().unwrap().storage.segment_seconds as i64;
        let mut warnings: Vec<String> = Vec::new();
        // A few words for each warning, same order (the header chip shows these).
        let mut shorts: Vec<String> = Vec::new();
        // One-click fixes offered next to a warning: {warning, camera id, kind}.
        let mut fixes: Vec<serde_json::Value> = Vec::new();
        // The warnings worth a notification, with a key that stays the same
        // while the problem lasts (see system_alerts).
        let mut problems: Vec<serde_json::Value> = Vec::new();
        let mut problem = |key: String, text: &str| problems.push(serde_json::json!({ "key": key, "text": text }));
        let mut cams = Vec::new();
        let handles: Vec<Arc<CameraHandle>> = self.cameras.read().unwrap().iter().flatten().cloned().collect();
        for h in handles {
            let enabled = h.active.load(Ordering::Relaxed);
            let run = h.running.read().unwrap().clone();
            let st = h.state.read().unwrap();
            let last_rec = self.db.last_segment_end(&slug(&run.name)).ok().flatten();
            // A sleeping battery camera is not offline.
            if enabled && !st.sleeping {
                if let Some(since) = st.offline_since.filter(|_| !st.online) {
                    if now - since > 60 {
                        // The camera watchdog may be restarting it (the plain
                        // alert waits), or may have given up (its own words).
                        let dog = self.watchdog_note(h.id);
                        let w = format!(
                            "{} has been offline for {} min{}{}",
                            run.name,
                            (now - since) / 60,
                            st.last_error.as_ref().map(|e| plain_error(e)).filter(|e| !e.is_empty()).map(|e| format!(" ({e})")).unwrap_or_default(),
                            dog.as_ref().and_then(|d| d.doing.as_ref()).map(|d| format!("; {d}")).unwrap_or_default()
                        );
                        shorts.push(format!("{} is offline", run.name));
                        // Pushed streams (a screen sent from another computer,
                        // a phone, OBS) stop whenever their sender is off.
                        if !run.url.starts_with("push://") {
                            match dog {
                                Some(d) if d.withhold => {}
                                Some(watchdog::Note { alert: Some(text), .. }) => problem(format!("offline:{}", h.id), &text),
                                _ => problem(format!("offline:{}", h.id), &format!("{} is offline", run.name)),
                            }
                        }
                        warnings.push(w);
                    }
                }
                // Continuous recording should close a segment at least every
                // few segment lengths while the camera is online.
                let online_for = st.online_since.map(|s| now - s).unwrap_or(0);
                if st.online && run.record && !run.record_motion_only && !run.on_demand && online_for > 4 * seg_secs.max(10) {
                    // A camera recording its main stream all the time: judge
                    // by that (a sub-stream copy still being written must not
                    // hide a main recording that stopped).
                    let last = if run.record_sub { last_rec } else { self.db.last_main_segment_end(&slug(&run.name)).ok().flatten() };
                    let age = last.map(|ms| now - ms / 1000).unwrap_or(i64::MAX);
                    if age > 4 * seg_secs.max(10) {
                        let w = format!("{} is online but no recording has been written for {}", run.name, ago(age));
                        problem(format!("norec:{}", h.id), &w);
                        shorts.push(format!("{} is not recording", run.name));
                        warnings.push(w);
                    }
                }
            }
            if enabled {
                if let Some(skew) = st.clock_skew.filter(|s| s.abs() > 30.0) {
                    fixes.push(serde_json::json!({ "warning": warnings.len(), "camera": h.id, "kind": "timesync" }));
                    shorts.push(format!("{}'s clock is off", run.name));
                    warnings.push(format!(
                        "{}'s clock is {} {}; turn on time sync (NTP) in the camera so its on-screen time is right",
                        run.name,
                        human_secs(skew.abs()),
                        if skew > 0.0 { "fast" } else { "slow" }
                    ));
                }
            }
            cams.push(serde_json::json!({
                "name": run.name,
                "clock_skew_s": st.clock_skew.map(|s| (s * 10.0).round() / 10.0),
                "enabled": enabled,
                "online": st.online,
                "sleeping": st.sleeping,
                "ingest": st.ingest,
                "online_since": st.online_since,
                "offline_since": st.offline_since,
                "reconnects": st.reconnects,
                "last_error": st.last_error,
                "last_error_plain": st.last_error.as_deref().map(plain_error),
                "last_error_at": st.last_error_at,
                "frames": st.frames,
                "last_recording_end_ms": last_rec,
                "live_viewers": h.live_main.viewers() + h.live_sub.viewers(),
            }));
        }
        let offsite = self.config.lock().unwrap().offsite.clone();
        if let Some(w) = crate::offsite::problem(&offsite) {
            problem("offsite".into(), &w);
            shorts.push("Offsite copy needs a look".into());
            warnings.push(w);
        }
        if self.watchdog.lock().unwrap().network_down {
            let w = "most cameras are offline at once: the network or the power may be down (the watchdog is not restarting anything)";
            problem("network".into(), w);
            shorts.push("Network may be down".into());
            warnings.push(w.into());
        }
        // The AI was asked for checks and none has worked for two minutes.
        {
            let det = self.detector();
            let on = det.is_some() || !self.config.lock().unwrap().ai.url.clone().unwrap_or_default().is_empty();
            if crate::telemetry::ai_snapshot(on, String::new()).state == "stalled" {
                let w = "AI object detection is stalled: no check has worked for the last two minutes";
                problem("ai".into(), w);
                shorts.push("AI detection stalled".into());
                warnings.push(w.into());
            }
        }
        // Disks: a folder at its cap with pruning keeping up is how it is
        // meant to run (no warning); free space below what the drive should
        // keep is a warning, and a fault when it cannot be freed.
        let caps = self.storage_caps_recent();
        let levels = self.storage_levels(&caps);
        let mut disks = Vec::new();
        for (c, (l, _)) in caps.iter().zip(&levels) {
            let name = if c.name.is_empty() { "data" } else { c.name.as_str() };
            if let (Some(_), Some(_)) = (c.free, c.total) {
                if l.level == "warn" || l.level == "crit" {
                    let w = format!("{name} folder: {}", l.detail);
                    // Notifications for faults, and for a drive other files
                    // fill (as before); not while the oldest are being deleted.
                    if l.level == "crit" || l.label == "Low space" {
                        problem(format!("disk:{name}"), &w);
                    }
                    shorts.push(format!("{name} folder: {}", l.label.to_lowercase()));
                    warnings.push(w);
                }
            } else {
                // The alert (which may go to outside services) names the
                // folder; the path is for the admin's health page only.
                problem(format!("folder:{name}"), &format!("cannot read the {name} folder"));
                shorts.push(format!("Cannot read the {name} folder"));
                warnings.push(format!("cannot read the {name} folder ({})", c.path.display()));
            }
            disks.push(serde_json::json!({
                "folder": name, "path": c.path, "free_bytes": c.free, "total_bytes": c.total,
                "auto": c.auto, "cap_bytes": c.cap, "used_bytes": c.used, "target_free": c.target_free,
                "level": l.level, "label": l.label,
            }));
        }
        // Devices the hourly check found that are not cameras here yet (not a problem).
        let (new_cameras, _) = super::newcams::current(self);
        serde_json::json!({ "ok": warnings.is_empty(), "warnings": warnings, "short": shorts, "fixes": fixes, "problems": problems, "cameras": cams, "disks": disks, "new_cameras": new_cameras })
    }
}

fn human_secs(s: f64) -> String {
    if s >= 120.0 { format!("{:.0} min", s / 60.0) } else { format!("{s:.0} s") }
}

fn ago(secs: i64) -> String {
    if secs == i64::MAX {
        "a long time".into()
    } else if secs >= 3600 {
        format!("{} h", secs / 3600)
    } else {
        format!("{} min", (secs / 60).max(1))
    }
}

/// A camera's last error in words for the owner: no stream address, no
/// technical detail. Empty when there is nothing useful to say.
pub fn plain_error(e: &str) -> String {
    let l = e.to_lowercase();
    if l.contains("not reachable") || l.contains("timed out") || l.contains("timeout") || l.contains("no route") || l.contains("unreachable") {
        "not reachable".into()
    } else if l.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == "401") || l.contains("unauthorized") || l.contains("authentication") {
        "the camera refused the sign-in".into()
    } else if l.contains("refused") {
        "the camera refused the connection".into()
    } else if l.contains("stream ended") {
        "the video stopped".into()
    } else {
        // Anything else: the words, without addresses (also a bare
        // user:password@host).
        let kept: Vec<&str> = e.split_whitespace().filter(|w| !w.contains("://") && !w.contains('@')).collect();
        let t = kept.join(" ");
        let t = t.replace("()", "").replace("( )", "");
        t.trim().trim_end_matches(':').trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::plain_error;

    #[test]
    fn errors_are_in_plain_words_without_addresses() {
        assert_eq!(plain_error("camera not reachable within 15 s (rtsp://192.0.2.10:554/stream1)"), "not reachable");
        assert_eq!(plain_error("RTSP 401 Unauthorized"), "the camera refused the sign-in");
        assert_eq!(plain_error("something odd at rtsp://192.0.2.10/x happened"), "something odd at happened");
        // 401 is a status, not part of a port or a number.
        assert_eq!(plain_error("bad answer from 192.0.2.10:5401"), "bad answer from 192.0.2.10:5401");
        assert_eq!(plain_error("server said 401"), "the camera refused the sign-in");
        assert_eq!(plain_error("could not open admin:secret@192.0.2.10:554/live (odd)"), "could not open (odd)");
    }
}
