use super::*;

impl Engine {
    /// Folder with the frames AI checked for an event (`thumbs/ai/<id>`).
    pub fn ai_review_dir(&self, id: &str) -> std::path::PathBuf {
        self.data_dir.join("thumbs").join("ai").join(id)
    }

    /// Path of an event's saved clip file (exists for protected events,
    /// older ffmpeg-pipeline clips, and exports).
    pub fn clip_file_path(&self, id: &str) -> std::path::PathBuf {
        self.data_dir.join("clips").join(format!("{id}.mp4"))
    }

    /// The event's video assembled from the recordings: pre-roll before the
    /// event through post-roll after it. None if no recording covers it.
    pub fn virtual_clip(&self, ev: &Event) -> anyhow::Result<Option<crate::ingest::splice::VirtualClip>> {
        self.virtual_clip_q(ev, false)
    }

    /// As [`Engine::virtual_clip`]; `small` prefers the sub stream (data saver).
    pub fn virtual_clip_q(&self, ev: &Event, small: bool) -> anyhow::Result<Option<crate::ingest::splice::VirtualClip>> {
        let (pre, post) = {
            let m = self.config.lock().unwrap().motion_for(&ev.camera);
            (m.pre_roll as i64, m.post_roll as i64)
        };
        let (from_ms, to_ms) = ((ev.start - pre) * 1000, (ev.end + post) * 1000);
        let segs = super::playback::sources_q(&self.db, &slug(&ev.camera), from_ms, to_ms, small, |r| self.segment_path(r))?;
        // One stream for the whole clip: a clip ends where the stream changes,
        // and an event clip (played, exported, saved for a protected event)
        // is one file. The main stream when it recorded the event (the sub
        // stream for `small`), else the other one: a recording the splicer
        // cannot use (plain MP4 from an older ffmpeg recorder) must not leave
        // the event with no clip at all.
        let first: Vec<_> = segs.into_iter().filter(|s| s.sub == small).collect();
        if !first.is_empty() {
            if let Some(c) = crate::ingest::splice::VirtualClip::plan(&first, from_ms, to_ms)? {
                return Ok(Some(c));
            }
        }
        // The other stream's files were left out wherever the preferred
        // stream has files (usable or not): ask for its own full list.
        let other = super::playback::sources_q(&self.db, &slug(&ev.camera), from_ms, to_ms, !small, |r| self.segment_path(r))?;
        let second: Vec<_> = other.into_iter().filter(|s| s.sub != small).collect();
        crate::ingest::splice::VirtualClip::plan(&second, from_ms, to_ms)
    }

    /// Write the event's clip to `clips/<id>.mp4` (if not there already) so
    /// it survives the recordings being deleted. Returns the path.
    pub fn save_clip_file(&self, ev: &Event) -> anyhow::Result<std::path::PathBuf> {
        let dest = self.clip_file_path(&ev.id);
        if dest.exists() {
            return Ok(dest);
        }
        match self.virtual_clip(ev)? {
            Some(clip) => {
                clip.write_file(&dest)?;
                Ok(dest)
            }
            None => anyhow::bail!("no recordings cover event {}", ev.id),
        }
    }
}

pub(super) fn finalize_event(
    engine: &Arc<Engine>,
    handle: &Arc<CameraHandle>,
    cfg: &CameraConfig,
    segment_seconds: u64,
    start: i64,
    end: i64,
    score: f64,
    thumb_jpeg: Vec<u8>,
    ai: super::ai_check::AiOutcome,
    motion_cfg: &crate::config::MotionConfig,
    slug_name: &str,
    seg_dir: &std::path::Path,
    clips_dir: &std::path::Path,
    thumbs_dir: &std::path::Path,
) {
    // Recordings the index knows are finished, with their exact times
    // (recordings that just closed are indexed on their own thread: give
    // it a moment to catch up).
    engine.wait_for_indexing(std::time::Duration::from_secs(5));
    let from_ms = (start - motion_cfg.pre_roll as i64) * 1000;
    let to_ms = (end + motion_cfg.post_roll as i64) * 1000;
    let indexed: Vec<(i64, std::path::PathBuf)> = engine
        .db
        .segments_overlapping(slug_name, from_ms, to_ms)
        .unwrap_or_default()
        .into_iter()
        // The sub stream is never joined into a main-stream clip file.
        .filter(|r| r.kind != "sub")
        .map(|r| (r.start_ms, engine.segment_path(&r)))
        .collect();
    let id = format!("{}-{}", slug_name, start);
    // Native recordings play straight from the segments (no copy). Older
    // ffmpeg-pipeline recordings are joined into a clip file as before.
    // A camera on ffmpeg: its main-stream files are indexed by a folder scan
    // that lags (so the clip would miss its end); join them into a file.
    let on_ffmpeg = handle.state.read().unwrap().ingest != "native";
    let native = !on_ffmpeg
        && !indexed.is_empty()
        && engine
            .db
            .segments_overlapping(slug_name, from_ms, to_ms)
            .unwrap_or_default()
            .iter()
            .all(|r| super::playback::is_native_kind(&r.kind));
    let clip = if native {
        Some(format!("/api/clips/{id}"))
    } else {
        build_clip(
            indexed,
            segment_seconds, start, end,
            motion_cfg.pre_roll, motion_cfg.post_roll,
            slug_name, seg_dir, clips_dir,
        )
    };
    // Event thumbnail: the highest-motion frame seen during the event.
    let thumb = save_thumb(&id, &thumb_jpeg, thumbs_dir);
    let mut ev = Event {
        protect: false,
        id: id.clone(),
        camera_id: handle.id,
        camera: cfg.name.clone(),
        start,
        end,
        score,
        clip: clip.clone(),
        thumb: if thumb { Some(id.clone()) } else { None },
        objects: Vec::new(),
    };
    // ODT hook (classic NVR object-detection equivalent): annotate the event with
    // labels from an external detector run on the event thumbnail.
    // AI: the objects seen become the event's labels; an event AI checked
    // without seeing anything wanted is kept but sends no alert.
    let alerts_on = engine.alerts_enabled(cfg);
    // A loud sound alerts on its own: AI looks at pictures, not sounds.
    let mut alert_ok = (!ai.ran || ai.confirmed || ai.sound) && alerts_on;
    let by_sound = ai.sound;
    let mut odt_quiet = false;
    ev.objects = ai.labels.clone();
    if ai.ran {
        info!(
            "camera '{}': AI {} event ({})",
            cfg.name,
            if ai.confirmed { "confirmed" } else { "found nothing wanted in" },
            if ai.labels.is_empty() { "no objects".to_string() } else { ai.labels.join(", ") }
        );
    }
    {
        let odt = engine.config.lock().unwrap().odt.clone();
        let cmd_tpl = odt.command.unwrap_or("".into());
        if odt.enabled && !cmd_tpl.is_empty() && thumb {
            let thumb_path = thumbs_dir.join(format!("{id}.jpg"));
            let img_arg = thumb_path.to_string_lossy().to_string();
            for o in run_odt(cmd_tpl.replace("{image}", &img_arg)) {
                if !ev.objects.contains(&o) {
                    ev.objects.push(o);
                }
            }
        }
        if odt.enabled && odt.alert_only_known && ev.objects.is_empty() {
            odt_quiet = alert_ok;
            alert_ok = false;
        }
    }
    // Faces: who it was, when AI saw a person (Settings > Smart features > Faces). Looking takes a
    // few seconds, so it happens before the alert only when someone is marked
    // "no alerts" (it decides whether to send one); otherwise the alert goes
    // out first and the names are added to the event afterwards.
    let mut quiet_people = false;
    let faces_wanted = ai.ran && ai.labels.iter().any(|l| l == "person") && engine.config.lock().unwrap().faces.wants(&cfg.name);
    // A camera that alerts only for strangers needs the names too, so for
    // that camera (and only that one) the alert waits for the faces.
    let strangers = cfg.strangers.clone().filter(|s| s.active());
    let faces_first = faces_wanted && alert_ok && (strangers.is_some() || !engine.db.quiet_people().unwrap_or_default().is_empty());
    let mut face_result: Option<super::faces::FaceResult> = None;
    if faces_first {
        let r = super::faces::recognize(engine, &cfg.name, &id, &ai, strangers.as_ref().filter(|s| s.first_seen).map(|s| s.first_seen_days()));
        add_face_labels(&mut ev.objects, &r);
        if super::faces::quiet_only(engine, &r, &ai) {
            alert_ok = false;
            quiet_people = true;
        }
        face_result = Some(r);
    }
    // License plates: read before the alert goes out, so the alert says
    // which plate it was (Settings > License plates). The result also tells
    // later rules whether the plate was a known one.
    let mut plate_result: Option<super::lpr::PlateResult> = None;
    if engine.lpr_wanted(&cfg.name, &ai) {
        let r = super::lpr::recognize(engine, &cfg.name, &id, &ai, (start * 1000, end * 1000));
        for l in r.labels() {
            if !ev.objects.contains(&l) {
                ev.objects.push(l);
            }
        }
        plate_result = Some(r);
    }
    // Strangers only: known faces and plates stay quiet (the event is still
    // recorded and labelled); the alert of an unknown one names it.
    let mut headline: Option<String> = None;
    let mut known_only = false;
    if let (true, Some(sc)) = (alert_ok, &strangers) {
        let facts = engine.stranger_facts(sc, &id, &ai, face_result.as_ref(), plate_result.as_ref());
        let now = chrono::Local::now();
        let minute = chrono::Timelike::hour(&now) * 60 + chrono::Timelike::minute(&now);
        match super::strangers::decide(sc, &cfg.name, &facts, minute) {
            super::strangers::Verdict::Alert { headline: h } => headline = Some(h),
            super::strangers::Verdict::Quiet { why } => {
                info!("camera '{}': event {id} stays quiet ({why}): strangers only", cfg.name);
                alert_ok = false;
                known_only = true;
            }
            super::strangers::Verdict::Pass => {}
        }
    }
    // AI review: what each check saw, and why the event did or did not alert.
    if ai.ran || !ai.camera_labels.is_empty() || !ai.log.is_empty() || odt_quiet {
        let mut ai = ai.clone();
        ai.min_score = {
            let c = engine.config.lock().unwrap();
            cfg.ai_min_score.unwrap_or(c.ai.min_score)
        };
        let why = if !ai.ran && !ai.log.is_empty() {
            "ai_failed"
        } else if alert_ok {
            "sent"
        } else if quiet_people {
            "quiet_people"
        } else if known_only {
            "known_only"
        } else if odt_quiet && alerts_on {
            "odt_only_known"
        } else if !alerts_on {
            "alerts_off"
        } else if ai.ran && !ai.confirmed {
            "ai_no_wanted"
        } else {
            "other"
        };
        let alert = serde_json::json!({ "sent": alert_ok, "why": why });
        let source = if ai.ran {
            super::ai_check::source_name(engine)
        } else if ai.sound {
            "sound".to_string()
        } else {
            "camera".to_string()
        };
        let review = ai.review_json(start, &source, &alert);
        let dir = engine.ai_review_dir(&id);
        if ai.log.iter().any(|c| !c.frame.is_empty()) && std::fs::create_dir_all(&dir).is_ok() {
            for (n, c) in ai.log.iter().enumerate().filter(|(_, c)| !c.frame.is_empty()) {
                let _ = std::fs::write(dir.join(format!("{n}.jpg")), &c.frame);
            }
        }
        if let Err(e) = engine.db.save_ai_review(&id, &cfg.name, start, alert_ok, &review.to_string()) {
            warn!("could not save the AI review for {id}: {e:#}");
        }
    }
    // Alert actions that pass {clip} need a real file.
    let wants_file = engine
        .config
        .lock()
        .unwrap()
        .alerts
        .action_command
        .as_deref()
        .is_some_and(|c| c.contains("{clip}"));
    if native && wants_file {
        if let Err(e) = engine.save_clip_file(&ev) {
            warn!("could not save clip for {id}: {e:#}");
        }
    }
    let mut unusual_override = false;
    // Out of the camera's usual pattern? Tagged "unusual" (filterable; the
    // notification says so).
    if let Some(v) = engine.unusual_check(cfg, start, &ev.objects) {
        info!("camera '{}': unusual {} for this hour of the week (rarity {:.2})", cfg.name, v.label, v.rarity);
        ev.objects.push("unusual".to_string());
        // Unusual in "alert" mode alerts even where a known person kept the
        // ordinary alert quiet (strangers only, a person marked "no alerts").
        if !alert_ok && alerts_on && (known_only || quiet_people) && cfg.unusual.as_ref().is_some_and(|u| u.mode() == "alert") {
            alert_ok = true;
            unusual_override = true;
        }
    }
    let (ev_id, mut labels) = (ev.id.clone(), ev.objects.clone());
    // AI looked and found nothing that counts (no sound, no label from the
    // camera itself): a camera can leave such events out of its lists.
    let unconfirmed = ai.ran && !ai.confirmed && !ai.sound && ai.camera_labels.is_empty() && !labels.iter().any(|l| ai.wanted.contains(l));
    // (Before the alert is tried: a pause or the alert interval has the last word.)
    if unusual_override {
        if let Err(e) = engine.db.set_ai_review_alert(&ev_id, true, "unusual") {
            warn!("could not update the AI review of {ev_id}: {e:#}");
        }
    }
    engine.record_event_as(ev, alert_ok, headline.as_deref());
    if unconfirmed {
        if let Err(e) = engine.db.set_event_unconfirmed(&ev_id) {
            warn!("could not mark event {ev_id}: {e:#}");
        }
    }
    if faces_wanted && !faces_first {
        let r = super::faces::recognize(engine, &cfg.name, &ev_id, &ai, None);
        let before = labels.len();
        if add_face_labels(&mut labels, &r) {
            // Added to what the event has now: a rule may tag it at the same time.
            if let Err(e) = engine.db.add_event_objects(&ev_id, &labels[before..]) {
                warn!("could not add names to event {ev_id}: {e:#}");
            }
        }
    }
    {
        let mut st = handle.state.write().unwrap();
        if let Some(path) = clip {
            st.clips.push(ClipInfo {
                id: id.clone(),
                camera: cfg.name.clone(),
                start,
                end,
                size: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            });
            if st.clips.len() > 200 {
                let overflow = st.clips.len() - 200;
                st.clips.drain(..overflow);
            }
        }
    }
    info!(
        "camera '{}': {} event at {} (score {:.3})",
        cfg.name,
        if by_sound { "sound" } else { "motion" },
        start,
        score
    );
}

/// ODT hook: run the configured external detector on the event thumbnail
/// and return its labels. Command is split on whitespace (no shell needed,
/// so this works the same on Windows). Stdout JSON:
/// Add recognized names (and "unknown face") to an event's labels; true if
/// anything was added.
fn add_face_labels(objects: &mut Vec<String>, r: &super::faces::FaceResult) -> bool {
    let before = objects.len();
    for n in &r.names {
        if !objects.contains(n) {
            objects.push(n.clone());
        }
    }
    if r.unknown > 0 && !objects.iter().any(|o| o == "unknown face") {
        objects.push("unknown face".into());
    }
    objects.len() > before
}

///   {"objects":[{"label":"person","score":0.92}]}
pub(super) fn run_odt(command_line: String) -> Vec<String> {
    let parts: Vec<String> = command_line.split(' ').filter(|p| !p.trim().is_empty()).map(|p| p.to_string()).collect();
    if parts.len() == 0 {
        return Vec::new();
    }
    let prog = &parts[0];
    let args: Vec<String> = parts.iter().skip(1).cloned().collect();
    let out = std::process::Command::new(&prog).args(&args).output();
    let stdout: String = match out {
        Err(_) => { warn!("odt: cannot run '{}'", prog); return Vec::new(); }
        Ok(o) => String::from_utf8(o.stdout).unwrap_or("".into()),
    };
    // Detector must print its JSON on one line; scan stdout lines.
    let mut v: serde_json::Value = serde_json::json!(null);
    for line in stdout.split('\n') {
        let cand = serde_json::from_str::<serde_json::Value>(line.trim()).ok();
        if let Some(c) = cand {
            v = c;
            break;
        }
    }
    if v.is_null() {
        warn!("odt: no JSON line on stdout of '{}'", prog);
        return Vec::new();
    }
    let mut labels: Vec<String> = Vec::new();
    let arr = &v["objects"];
    if let Some(list) = arr.as_array() {
        for o in list {
            let lab = &o["label"];
            if let Some(s) = lab.as_str() {
                labels.push(s.to_string());
            }
        }
    }
    labels
}
pub(super) fn save_thumb(id: &str, frame: &[u8], thumbs_dir: &std::path::Path) -> bool {
    if frame.is_empty() {
        return false;
    }
    let path = thumbs_dir.join(format!("{id}.jpg"));
    std::fs::write(&path, frame).is_ok()
}

/// Split a segment file name of the form `{slug}-{stamp}[-rec]-seg{NNNNNN}.mp4`
/// into (prefix before the number, number). Returns None when the name does
/// not belong to `slug`.
fn parse_segment_name<'a>(name: &'a str, slug_name: &str) -> Option<(&'a str, u32)> {
    let rest = name.strip_prefix(slug_name)?.strip_prefix('-')?;
    let stem = rest.strip_suffix(".mp4")?;
    let (head, num) = stem.split_at(stem.len().checked_sub(6)?);
    if !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let head = head.strip_suffix("-seg")?;
    // Stamp must be digits, optionally followed by "-rec" (guards against a
    // camera whose slug is a prefix of another's, e.g. "cam" vs "cam-2").
    let stamp = head.strip_suffix("-rec").unwrap_or(head);
    if stamp.is_empty() || !stamp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let prefix_len = name.len() - ".mp4".len() - 6;
    Some((&name[..prefix_len], num.parse().ok()?))
}

/// Pure selection of the closed segments overlapping the clip window.
/// `files` is (file name, mtime as unix seconds). A segment ends at its mtime
/// and starts `segment_seconds` earlier. It is closed (safe to concat) if a
/// higher-numbered file with the same prefix exists, or it has not been
/// touched for `segment_seconds + 3` s. Result is sorted by start time.
#[allow(clippy::too_many_arguments)]
pub(super) fn select_segments(
    files: &[(String, i64)],
    slug_name: &str,
    segment_seconds: u64,
    start: i64,
    end: i64,
    pre: u64,
    post: u64,
    now: i64,
) -> Vec<String> {
    let window_start = start - pre as i64;
    let window_end = end + post as i64;
    let seg = segment_seconds as i64;
    let parsed: Vec<(&str, i64, &str, u32)> = files
        .iter()
        .filter_map(|(n, m)| parse_segment_name(n, slug_name).map(|(p, num)| (n.as_str(), *m, p, num)))
        .collect();
    let mut chosen: Vec<(i64, &str)> = Vec::new();
    for &(name, mtime, prefix, num) in &parsed {
        let has_successor = parsed.iter().any(|&(_, _, p, n)| p == prefix && n > num);
        let closed = has_successor || mtime < now - (seg + 3);
        let s = mtime - seg;
        if closed && mtime > window_start && s < window_end {
            chosen.push((s, name));
        }
    }
    chosen.sort();
    chosen.into_iter().map(|(_, n)| n.to_string()).collect()
}

/// Join the recordings covering [start - pre, end + post] into one clip.
/// `indexed` are recordings from the database (known finished, exact start
/// in ms); the segment folder is also scanned for finished files the index
/// has not picked up yet.
pub(super) fn build_clip(
    indexed: Vec<(i64, std::path::PathBuf)>,
    segment_seconds: u64,
    start: i64,
    end: i64,
    pre: u64,
    post: u64,
    slug_name: &str,
    seg_dir: &std::path::Path,
    clips_dir: &std::path::Path,
) -> Option<String> {
    let now = Utc::now().timestamp();
    let mut listing: Vec<(String, i64)> = Vec::new();
    for entry in std::fs::read_dir(seg_dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".mp4") {
            continue;
        }
        // Wall-clock estimates drift (lavfi -re jitter, direct-mode keyframe
        // cuts), so use the file's mtime as its end.
        let mtime = entry.metadata().ok()
            .and_then(|m| m.modified().ok())
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).timestamp())
            .unwrap_or(0);
        listing.push((name, mtime));
    }
    let names = select_segments(&listing, slug_name, segment_seconds, start, end, pre, post, now);
    // (start ms, path): index entries win; folder-scan entries use the
    // mtime-based estimate.
    let mut chosen: Vec<(i64, std::path::PathBuf)> = indexed;
    for n in names {
        let p = seg_dir.join(&n);
        if !chosen.iter().any(|(_, q)| q.file_name() == p.file_name()) {
            let mtime = listing.iter().find(|(l, _)| *l == n).map(|(_, m)| *m).unwrap_or(0);
            chosen.push(((mtime - segment_seconds as i64) * 1000, p));
        }
    }
    chosen.sort_by_key(|(t, _)| *t);
    // ffmpeg's concat demuxer resolves paths relative to the list file,
    // so store absolute paths.
    let files: Vec<std::path::PathBuf> = chosen
        .into_iter()
        .map(|(_, p)| p.canonicalize().unwrap_or(p))
        .collect();
    if files.is_empty() {
        return None;
    }

    let list = clips_dir.join(format!("{slug_name}-{start}.list.txt"));
    let content: String = files
        .iter()
        .map(|f| format!("file '{}'\n", f.to_string_lossy()))
        .collect();
    if std::fs::write(&list, content).is_err() {
        return None;
    }
    let out = clips_dir.join(format!("{slug_name}-{start}.mp4"));
    let status = crate::child::ffmpeg()
        .args([
            "-hide_banner", "-loglevel", "error",
            "-f", "concat", "-safe", "0", "-i", &list.to_string_lossy(),
            "-c", "copy", &out.to_string_lossy(),
        ])
        .status();
    let _ = std::fs::remove_file(&list);
    match status {
        Ok(s) if s.success() => Some(out.to_string_lossy().into_owned()),
        _ => None,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn f(n: &str, m: i64) -> (String, i64) {
        (n.to_string(), m)
    }

    #[test]
    fn excludes_open_newest_segment() {
        let files = vec![
            f("cam-100-seg000000.mp4", 960),
            f("cam-100-seg000001.mp4", 990),
            f("cam-100-seg000002.mp4", 999), // open: no successor, fresh
        ];
        let got = select_segments(&files, "cam", 10, 900, 1000, 0, 0, 1000);
        assert_eq!(got, vec!["cam-100-seg000000.mp4", "cam-100-seg000001.mp4"]);
    }

    #[test]
    fn includes_closed_final_motion_only_segment() {
        let files = vec![f("cam-5-rec-seg000000.mp4", 900)];
        let got = select_segments(&files, "cam", 10, 880, 900, 0, 0, 1000);
        assert_eq!(got, vec!["cam-5-rec-seg000000.mp4"]);
    }

    #[test]
    fn respects_window() {
        let files = vec![
            f("cam-1-seg000000.mp4", 100), // [90,100]
            f("cam-1-seg000001.mp4", 110), // [100,110]
            f("cam-1-seg000002.mp4", 120), // [110,120]
            f("cam-1-seg000003.mp4", 130), // [120,130]
        ];
        let got = select_segments(&files, "cam", 10, 105, 112, 2, 2, 1000);
        assert_eq!(got, vec!["cam-1-seg000001.mp4", "cam-1-seg000002.mp4"]);
    }

    #[test]
    fn works_across_run_prefixes_and_ignores_other_cameras() {
        let files = vec![
            f("cam-2-seg000001.mp4", 500),
            f("cam-1-seg000000.mp4", 490),
            f("cam-1-seg000001.mp4", 480), // higher number, lower mtime
            f("cam-2-3-seg000000.mp4", 495), // other camera "cam-2"? slug "cam" -> stamp "2-3" rejected
            f("cam-7-rec-seg000000.mp4", 505),
            f("notes.mp4", 500),
        ];
        let got = select_segments(&files, "cam", 10, 470, 510, 0, 0, 1000);
        assert_eq!(
            got,
            vec![
                "cam-1-seg000001.mp4",
                "cam-1-seg000000.mp4",
                "cam-2-seg000001.mp4",
                "cam-7-rec-seg000000.mp4",
            ]
        );
    }
}
