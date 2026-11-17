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
        // Notes about cameras (not problems), shown with the graphics card notes.
        let mut hw_notes: Vec<String> = Vec::new();
        // Camera notes with a button that opens the camera's settings: (text, camera id, fix kind).
        let mut cam_notes: Vec<(String, usize, &'static str)> = Vec::new();
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
            // Motion decoding: on the graphics card or the processor, and the
            // ffmpeg processes the camera runs now.
            let dec = h.decode.lock().unwrap().clone();
            let mut procs: Vec<String> = Vec::new();
            if st.ingest == "native" {
                if let Some(s) = dec.stream {
                    procs.push(format!("motion decoder ({s} stream)"));
                }
            } else if dec.stream.is_some() {
                procs.extend(dec.procs.iter().map(|p| p.to_string()));
            }
            procs.extend(crate::transcode::active_for(&run.name));
            // No sub stream: motion decodes the full-size stream. Worth a note
            // when it is large and on the processor (not a problem: some cameras
            // have one stream, and the owner can say so with no_sub_ok).
            if enabled && st.online && wants_sub_note(&run) {
                if let Some((w, hh, codec)) = dec.heavy_main_on_cpu() {
                    cam_notes.push((
                        format!("{}: motion is decoding the full-size stream ({w}x{hh} {codec}) on the processor; adding the camera's sub stream cuts CPU use a lot", run.name),
                        h.id,
                        "substream",
                    ));
                }
            }
            // A sub stream address that cannot be opened: motion uses the main stream.
            if let Some(why) = run.sub_url.as_deref().filter(|u| !u.trim().is_empty()).and_then(super::sub_url_problem) {
                if enabled {
                    hw_notes.push(format!("{}: {why}; motion uses the main stream. Fix it in the camera's Stream settings", run.name));
                }
            }
            // The ffmpeg pipeline re-encodes the full-size recording. A note
            // when it is the owner's choice (burn-in, Direct copy off);
            // a thing to check on the processor only when the camera fell back
            // to ffmpeg by itself.
            let on_ffmpeg = st.ingest.starts_with("ffmpeg");
            if let (true, true, true, Some(text)) = (enabled, st.online, on_ffmpeg, dec.reencode.as_ref()) {
                let full = match &dec.encoder_note {
                    Some(n) if !dec.reencode_hw => format!("{text} (Not on the graphics card: {n}.)"),
                    _ => text.clone(),
                };
                match reencode_kind(dec.reencode_hw, st.ingest) {
                    Said::Note => hw_notes.push(text.clone()),
                    Said::Warning => {
                        shorts.push(format!("{} re-encodes its recordings", run.name));
                        warnings.push(full);
                    }
                    Said::NoteWithFix => cam_notes.push((full, h.id, "settings")),
                }
            }
            cams.push(serde_json::json!({
                "name": run.name,
                "decode": dec.json(),
                "ffmpeg_why": super::ffpath::why_ffmpeg_now(&run, st.ingest),
                "ffmpeg": procs,
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
                // Holes filled from the camera's own card (null: not turned on).
                "card_fill": if run.fill_from_card { serde_json::json!(super::card_fill::status(&run.name)) } else { serde_json::Value::Null },
                "card_catchup": if run.catch_up_on() { serde_json::json!(super::card_catchup::status(&run.name)) } else { serde_json::Value::Null },
            }));
        }
        // Event clips that cannot be saved to a camera's copy folder.
        {
            let cams = self.config.lock().unwrap().cameras.clone();
            for p in super::event_copy::problems(&cams, now) {
                problem(p.key, &p.text);
                shorts.push(p.short);
                warnings.push(p.warning);
            }
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
        // Folders AmberSight may not write to (recordings would fail).
        for (name, path) in super::storage::unwritable_folders(&caps, now) {
            let label = if name.is_empty() { "data".to_string() } else { name.clone() };
            problem(format!("unwritable:{label}"), &super::storage::unwritable_text(&name, &path, false));
            shorts.push(format!("{label} folder is not writable"));
            warnings.push(super::storage::unwritable_text(&name, &path, true));
        }
        // Devices the hourly check found that are not cameras here yet (not a problem).
        let (new_cameras, _) = super::newcams::current(self);
        // Notes (not problems): graphics cards that cannot decode, and
        // decoders that left the card while running.
        let mut notes = crate::hwdecode::notes();
        notes.extend(hw_notes);
        // Privacy masks made ignore zones (0.11.2) and text now drawn on
        // screen instead of burned in (0.11.1), for a while.
        notes.extend(super::mask_settings::masks_note(&self.db, now * 1000));
        notes.extend(super::mask_settings::overlay_note(&self.db, now * 1000));
        // Camera notes carry a button: note_fixes[] = {note (index into notes), camera, kind}.
        let mut note_fixes: Vec<serde_json::Value> = Vec::new();
        for (text, cam, kind) in cam_notes {
            note_fixes.push(serde_json::json!({ "note": notes.len(), "camera": cam, "kind": kind }));
            notes.push(text);
        }
        notes.extend(crate::ffmpeg_bin::old_note());
        serde_json::json!({ "ok": warnings.is_empty(), "warnings": warnings, "short": shorts, "fixes": fixes, "problems": problems, "notes": notes, "note_fixes": note_fixes, "cameras": cams, "disks": disks, "new_cameras": new_cameras })
    }
}

/// How Status shows a thing about a camera.
#[derive(Debug, PartialEq, Eq)]
enum Said {
    /// A note (health stays ok).
    Note,
    /// A note with a button that opens the camera's settings (health stays ok).
    NoteWithFix,
    /// A warning (health is not ok).
    Warning,
}

/// The recording is re-encoded: on a hardware encoder, a note; on the
/// processor because of the owner's own settings, a note with a button; on
/// the processor after the camera fell back to ffmpeg by itself, a warning.
fn reencode_kind(on_hw: bool, ingest: &str) -> Said {
    if on_hw {
        Said::Note
    } else if ingest == "ffmpeg (fallback)" {
        Said::Warning
    } else {
        Said::NoteWithFix
    }
}

/// A network camera (not a battery one) with no sub stream whose owner did
/// not say it has none: its large main stream decoded on the processor is noted.
fn wants_sub_note(run: &CameraConfig) -> bool {
    let netcam = ["rtsp://", "rtsps://", "http://", "https://"].iter().any(|s| run.url.starts_with(s));
    netcam && !run.on_demand && !run.no_sub_ok && run.sub_url.as_deref().is_none_or(|u| u.trim().is_empty())
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
    // First: a folder named "Refused" must not read as a refused camera.
    if e.starts_with(crate::macnet::BLOCKED_PREFIX) {
        // Already in words, with what to do.
        e.to_string()
    } else if l.contains("is on a read-only file system") {
        "its recording folder is on a read-only file system".into()
    } else if l.contains("is not writable by ambersight") {
        "its recording folder is not writable".into()
    } else if l.contains("not reachable") || l.contains("timed out") || l.contains("timeout") || l.contains("no route") || l.contains("unreachable") {
        "not reachable".into()
    } else if l.contains("stream ended") {
        "the video stopped".into()
    } else {
        // RTSP answers (404, 401, busy, refused) in words; anything else:
        // its first line, without addresses (also a bare user:password@host).
        super::native::readable_error(e)
    }
}

#[cfg(test)]
mod tests {
    use super::{plain_error, reencode_kind, wants_sub_note, Said};

    #[test]
    fn owners_choices_are_notes_not_warnings() {
        assert_eq!(reencode_kind(false, "ffmpeg"), Said::NoteWithFix);
        assert_eq!(reencode_kind(true, "ffmpeg"), Said::Note);
        assert_eq!(reencode_kind(true, "ffmpeg (fallback)"), Said::Note);
        assert_eq!(reencode_kind(false, "ffmpeg (fallback)"), Said::Warning);
        let cam = |url: &str, sub: Option<&str>, on_demand: bool, ok: bool| {
            let mut c: crate::config::CameraConfig = serde_json::from_value(serde_json::json!({ "name": "c", "url": url })).unwrap();
            c.sub_url = sub.map(String::from);
            c.on_demand = on_demand;
            c.no_sub_ok = ok;
            c
        };
        assert!(wants_sub_note(&cam("rtsp://192.0.2.1/main", None, false, false)));
        assert!(wants_sub_note(&cam("rtsp://192.0.2.1/main", Some(" "), false, false)));
        assert!(!wants_sub_note(&cam("rtsp://192.0.2.1/main", Some("rtsp://192.0.2.1/sub"), false, false)));
        assert!(!wants_sub_note(&cam("rtsp://192.0.2.1/main", None, true, false)), "battery camera");
        assert!(!wants_sub_note(&cam("rtsp://192.0.2.1/main", None, false, true)), "owner said it has none");
        assert!(!wants_sub_note(&cam("push://k", None, false, false)));
        assert!(!wants_sub_note(&cam("baichuan://192.0.2.1", None, false, false)));
    }

    #[test]
    fn the_process_limit_note_stays_whole() {
        let w = crate::proclimit::limit_words(&crate::proclimit::ProcLimits { pids_max: Some("100".into()), pids_current: Some(99), nproc: None }, true);
        assert_eq!(plain_error(&w), w);
        assert!(plain_error(&w).contains("--pids-limit"));
    }

    #[test]
    fn errors_are_in_plain_words_without_addresses() {
        assert_eq!(plain_error("camera not reachable within 15 s (rtsp://192.0.2.10:554/stream1)"), "not reachable");
        assert_eq!(plain_error("Folder Refused (/r/cameras/a) is not writable by AmberSight (user 99:100)"), "its recording folder is not writable");
        assert_eq!(plain_error("Folder Timeout (/r) is not writable by AmberSight (user 1:1) (ffmpeg: 401 Unauthorized)"), "its recording folder is not writable");
        assert_eq!(plain_error("Folder Timeout (/r) is on a read-only file system"), "its recording folder is on a read-only file system");
        assert_eq!(plain_error("RTSP 401 Unauthorized"), "the camera refused the user name or password");
        assert_eq!(plain_error("something odd at rtsp://192.0.2.10/x happened"), "something odd at happened");
        // 401 is a status, not part of a port or a number.
        assert_eq!(plain_error("bad answer from 192.0.2.10:5401"), "bad answer from 192.0.2.10:5401");
        assert_eq!(plain_error("server said 401"), "the camera refused the user name or password");
        assert_eq!(
            plain_error("DESCRIBE failed: 404 response to DESCRIBE CSeq=3: Unexpected RTSP response status\n\nconn: 127.0.0.1:1(me)->127.0.0.1:2@x\nmsg: y"),
            "the camera has no stream at that address (404); check the path"
        );
        assert_eq!(plain_error("the camera refused the user name or password (check them in the camera's settings)"), "the camera refused the user name or password");
        assert_eq!(plain_error("could not open admin:secret@192.0.2.10:554/live (odd)"), "could not open (odd)");
    }
}
