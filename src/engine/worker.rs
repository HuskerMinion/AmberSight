use super::*;
use std::io::Write;

/// Filters burned into the picture itself (recordings, live view, analysis):
/// privacy masks (solid black boxes) and the name + time overlay. Empty when
/// the camera has neither, i.e. no re-encoding is needed for them.
pub fn picture_filters(cfg: &CameraConfig) -> String {
    let mut f: Vec<String> = cfg
        .privacy_masks
        .iter()
        .map(|m| {
            let c = |v: f64| v.clamp(0.0, 1.0);
            format!(
                "drawbox=x=iw*{:.4}:y=ih*{:.4}:w=iw*{:.4}:h=ih*{:.4}:color=black:t=fill",
                c(m.x), c(m.y), c(m.w), c(m.h)
            )
        })
        .collect();
    if cfg.overlay {
        let (x, y) = match cfg.overlay_pos.as_deref().unwrap_or("bottom-left") {
            "top-left" => ("10", "10"),
            "top-right" => ("w-tw-10", "10"),
            "bottom-right" => ("w-tw-10", "h-th-10"),
            _ => ("10", "h-th-10"),
        };
        f.push(format!(
            "drawtext={}text='{}':fontcolor=white:fontsize=h/25+12:box=1:boxcolor=black@0.5:x={x}:y={y}",
            font_option(),
            overlay_drawtext(cfg)
        ));
    }
    f.join(",")
}

/// Captions burned into a download: `label` and the date and time of each
/// moment, centered at the bottom. `base_s`: the local wall time (seconds
/// since the epoch, the time zone's offset added) of the input's first frame;
/// ffmpeg adds each frame's time to it.
pub fn caption_drawtext(label: &str, base_s: f64) -> String {
    format!(
        "drawtext={}text='{}  %{{pts\\:gmtime\\:{base_s:.3}}}':fontcolor=white:fontsize=h/25+12:box=1:boxcolor=black@0.5:boxborderw=6:x=(w-tw)/2:y=h-th-12",
        font_option(),
        drawtext_safe(label)
    )
}

/// The font for burned-in text, named as a file: ffmpeg on Windows has no
/// font configuration, and static or container builds often have none either
/// (drawtext then fails). Empty when none of the usual fonts is there:
/// ffmpeg's own font lookup is tried then.
fn font_option() -> String {
    static FONT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FONT.get_or_init(|| font_option_from(&font_candidates())).clone()
}

/// Where the usual fonts are, best first.
fn font_candidates() -> Vec<std::path::PathBuf> {
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        return ["arial.ttf", "segoeui.ttf", "tahoma.ttf"].iter().map(|f| std::path::Path::new(&root).join("Fonts").join(f)).collect();
    }
    [
        // Debian and Ubuntu (fonts-dejavu-core, in the Docker images), Fedora, Arch, Alpine.
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
        // macOS.
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/Library/Fonts/Arial.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
    ]
    .iter()
    .map(std::path::PathBuf::from)
    .collect()
}

/// The drawtext `fontfile` option for the first of `candidates` that is there.
fn font_option_from(candidates: &[std::path::PathBuf]) -> String {
    match candidates.iter().find(|p| p.is_file()) {
        // Filter syntax: forward slashes, a drive colon escaped.
        Some(p) => format!("fontfile='{}':", p.to_string_lossy().replace('\\', "/").replace(':', "\\:")),
        None => String::new(),
    }
}

#[cfg(test)]
mod font_tests {
    #[test]
    fn the_first_font_that_is_there_is_named() {
        let dir = std::env::temp_dir().join(format!("amber-font-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let have = dir.join("Have.ttf");
        std::fs::write(&have, b"x").unwrap();
        let gone = dir.join("Gone.ttf");
        let got = super::font_option_from(&[gone.clone(), have.clone()]);
        assert_eq!(got, format!("fontfile='{}':", have.to_string_lossy().replace('\\', "/").replace(':', "\\:")));
        assert_eq!(super::font_option_from(&[gone]), "");
        // (The Debian path the Docker images install is looked for.)
        if !cfg!(windows) {
            assert!(super::font_candidates().iter().any(|p| p.ends_with("truetype/dejavu/DejaVuSans.ttf")));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Characters that could break out of a drawtext value, removed from user text.
fn drawtext_safe(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\\' | '\'' | ':' | '%' | '{' | '}' | ';' | ',' | '[' | ']') && !c.is_control()).collect()
}

/// The overlay template as drawtext text: fill-ins become the camera's names
/// and ffmpeg's clock; everything else is sanitized.
pub fn overlay_drawtext(cfg: &CameraConfig) -> String {
    // At most 120 characters of template, cut before the fill-ins expand (so
    // an expansion is never cut in half).
    let tpl_owned: String = cfg.overlay_text.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or("{short} {date} {time}").chars().take(120).collect();
    let tpl = tpl_owned.as_str();
    let short = if cfg.short_name.is_empty() { cfg.name.as_str() } else { cfg.short_name.as_str() };
    let site = crate::links::own_name_cached();
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(i) = rest.find('{') {
        out.push_str(&drawtext_safe(&rest[..i]));
        let after = &rest[i..];
        let (piece, used) = if let Some(r) = after.strip_prefix("{name}") {
            (drawtext_safe(&cfg.name), after.len() - r.len())
        } else if let Some(r) = after.strip_prefix("{short}") {
            (drawtext_safe(short), after.len() - r.len())
        } else if let Some(r) = after.strip_prefix("{site}") {
            (drawtext_safe(&site), after.len() - r.len())
        } else if let Some(r) = after.strip_prefix("{date}") {
            ("%{localtime\\:%F}".to_string(), after.len() - r.len())
        } else if let Some(r) = after.strip_prefix("{time}") {
            ("%{localtime\\:%T}".to_string(), after.len() - r.len())
        } else if after.starts_with("{v:") {
            // Named overlay variables are live-view only (their value changes
            // after the encode starts); drop the whole token from burned-in text.
            (String::new(), after.find('}').map_or(1, |j| j + 1))
        } else {
            (String::new(), 1)
        };
        out.push_str(&piece);
        rest = &rest[i + used..];
    }
    out.push_str(&drawtext_safe(rest));
    out
}

/// Frame rate and size for motion analysis / the MJPEG live view.
fn analysis_filter(cfg: &CameraConfig) -> String {
    format!("fps={},scale={}:-2", cfg.fps.max(1), cfg.scale)
}

/// Full chain for single-output use (analysis pipe, CLI snapshot).
pub fn video_filter(cfg: &CameraConfig) -> String {
    let p = picture_filters(cfg);
    if p.is_empty() { analysis_filter(cfg) } else { format!("{p},{}", analysis_filter(cfg)) }
}

/// Re-encoding arguments for recorded segments: H.264 with a keyframe at
/// every segment boundary (time-based, so it works at any frame rate).
/// ffmpeg's segment muxer writes plain MP4 (the index at the end), which the
/// clip and playback splicer cannot use: fragmented MP4 like the built-in
/// reader writes.
const FRAGMENTED: [&str; 2] = ["-segment_format_options", "movflags=+frag_keyframe+empty_moov+default_base_moof"];

fn reencode_args(segment_seconds: u64) -> Vec<String> {
    [
        "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p",
        "-force_key_frames",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(std::iter::once(format!("expr:gte(t,n_forced*{})", segment_seconds.max(1))))
    .collect()
}

pub fn ffmpeg_input_args(url: &str) -> Vec<String> {
    if crate::local_source::is_local(url) {
        // Invalid local URLs are caught before spawning (camera_worker);
        // otherwise fall through so ffmpeg reports the problem.
        if let Ok(a) = crate::local_source::input_args(url, crate::local_source::host_os()) {
            return a;
        }
    }
    if let Some(rest) = url.strip_prefix("lavfi://") {
        // -re paces synthetic sources at real time so segments/clips are real-time.
        vec!["-re".into(), "-f".into(), "lavfi".into(), "-i".into(), rest.into()]
    } else if url.starts_with("rtsp://") {
        // -timeout (microseconds): without it ffmpeg waits forever on a
        // stream path the camera does not answer.
        vec!["-rtsp_transport".into(), "tcp".into(), "-timeout".into(), "10000000".into(), "-i".into(), url.into()]
    } else {
        vec!["-i".into(), url.into()]
    }
}

pub(super) fn camera_worker(handle: Arc<CameraHandle>, cfg: CameraConfig, engine: Arc<Engine>) {
    let slug_name = slug(&cfg.name);
    let (folder_name, folder_root) = engine.record_folder(&cfg);
    let seg_dir = folder_root.join("cameras").join(&slug_name).join("segments");
    let clips_dir = engine.data_dir.join("clips");
    let thumbs_dir = engine.data_dir.join("thumbs");
    std::fs::create_dir_all(&seg_dir).ok();
    // A folder whose "keep private" switch is on may only exist now: close it
    // (in the background, once) rather than leave what was just made open.
    if !folder_name.is_empty() && engine.config.lock().unwrap().storage.folders.iter().any(|f| f.name == folder_name && f.private == Some(true)) {
        crate::lockdown::protect_new_folder(&folder_root);
    }
    std::fs::create_dir_all(&clips_dir).ok();
    std::fs::create_dir_all(&thumbs_dir).ok();

    let mut detector = MotionDetector::new();
    // Capture devices and the screen produce raw frames: always encoded, never
    // stream-copied. A device opens only once, so motion-only recording (a
    // second ffmpeg on the same input) records continuously instead.
    let local = crate::local_source::is_local(&cfg.url);
    let exclusive = crate::local_source::is_device(&cfg.url);
    if exclusive && cfg.record && cfg.record_motion_only {
        warn!("camera '{}': motion-only recording needs a second open of the device; recording continuously instead", cfg.name);
    }
    // Retry delay for local sources that fail; grows while nothing works.
    let mut backoff: u64 = 3;

    // The worker lives for the lifetime of the camera slot: while the camera
    // is disabled it parks; when re-enabled it restarts ffmpeg.
    loop {
        if !handle.active.load(Ordering::Relaxed) {
            // If this handle is no longer the occupant of its slot (camera
            // removed or replaced via update), stop the thread for good.
            let in_slot = match engine.camera(handle.id) {
                Some(h) => Arc::ptr_eq(&h, &handle),
                None => false,
            };
            if !in_slot {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(300));
            continue;
        }
        let run_ts = Utc::now().timestamp();
        let seg_prefix = format!("{slug_name}-{run_ts}-seg");

        // Direct mode stream-copies the source; lavfi sources have nothing
        // encoded to copy, so they fall back to inline re-encode segments.
        // Masks/overlay change the picture, so they rule out stream copy.
        let mut picture = picture_filters(&cfg);
        if let Some(cap) = crate::local_source::record_filter(&cfg.url) {
            picture = if picture.is_empty() { cap.to_string() } else { format!("{cap},{picture}") };
        }
        let direct = cfg.direct && cfg.record && !cfg.url.starts_with("lavfi://") && !local && picture.is_empty();
        // "Sub stream all the time, main around events": the sub stream is
        // copied into its own segments all along, and the main stream is
        // recorded as for motion-only.
        let sub_url = cfg.sub_url.clone().filter(|u| !u.is_empty());
        let sub_mode = cfg.record && !cfg.record_motion_only && cfg.record_sub && sub_url.is_some() && !local;
        // Motion-only recording: segments are written by a recorder process
        // that lives only while the camera currently reports motion.
        let motion_only = cfg.record && (cfg.record_motion_only || sub_mode) && !exclusive;
        if local {
            let why = crate::local_source::precheck(&cfg.url)
                .or_else(|| crate::local_source::input_args(&cfg.url, crate::local_source::host_os()).err());
            if let Some(why) = why {
                fail_local(&handle, &cfg, why, &mut backoff);
                continue;
            }
        }
        let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
        args.extend(ffmpeg_input_args(&camera_url(&cfg)));
        let segment_seconds = engine.config.lock().unwrap().storage.segment_seconds;
        if cfg.record && !direct && !motion_only {
            // One decode, two outputs: the full-resolution recording (with
            // masks/overlay burned in) and the small analysis stream.
            let pre = if picture.is_empty() { "null".to_string() } else { picture.clone() };
            args.extend([
                "-filter_complex".to_string(),
                format!("[0:v]{pre},split=2[rec][a0];[a0]{}[ana]", analysis_filter(&cfg)),
                "-map".into(),
                "[rec]".into(),
            ]);
            if cfg.audio {
                args.extend(["-map", "0:a?", "-c:a", "aac"].map(String::from));
            }
            args.extend(reencode_args(segment_seconds));
            args.extend(["-f", "segment", "-segment_time"].map(String::from));
            args.push(segment_seconds.to_string());
            args.extend(["-reset_timestamps", "1"].map(String::from));
            args.extend(FRAGMENTED.map(String::from));
            args.push(seg_dir.join(format!("{seg_prefix}%06d.mp4")).to_string_lossy().into_owned());
            args.extend(["-map", "[ana]"].map(String::from));
        } else {
            args.push("-vf".into());
            args.push(video_filter(&cfg));
        }
        args.extend(["-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "8", "-"].map(String::from));

        let mut child = match crate::child::ffmpeg()
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => {
                crate::child::adopt(&c);
                c
            }
            Err(e) => {
                warn!("camera '{}': could not start ffmpeg: {}", cfg.name, e);
                handle.state.write().unwrap().online = false;
                thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };

        // Failing local sources retry quietly; their error is logged once per change.
        if !local || backoff == 3 {
            info!("camera '{}': capture started ({})", cfg.name, redact_url(cfg.url.clone()));
        }
        // `online` is set when the first frame arrives (process_frame), not
        // here: a started ffmpeg is not yet a working camera.

        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        // Keep ffmpeg's last error lines (drained continuously) for a readable camera error.
        let stderr_tail = child.stderr.take().map(drain_stderr);
        let mut extra_tails: Vec<(&'static str, StderrTail)> = Vec::new();
        let frames_at_start = handle.state.read().unwrap().frames;
        // Share processes with a watchdog so disabling the camera unblocks
        // the read loop immediately (kill ffmpeg -> EOF -> park).
        let child_box = Arc::new(Mutex::new(child));
        let mut children: Vec<Arc<Mutex<std::process::Child>>> = Vec::new();
        children.push(child_box.clone());
        // Recorders (direct, sub stream) with their stderr, and whether their
        // early exit has been looked at.
        let mut rec_watch: Vec<RecWatch> = Vec::new();
        let started = std::time::Instant::now();
        let mut last_rec_check = std::time::Instant::now();
        let mut motion_checked = false;
        let motion_bad = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut motion_renew = std::time::Instant::now();
        let mut motion_started = std::time::Instant::now();

        // Direct mode: a second ffmpeg stream-copies the source into segments
        // while the analysis process decodes its own substream.
        if direct && !motion_only {
            let mut rargs: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
            rargs.extend(ffmpeg_input_args(&camera_url(&cfg)));
            rargs.push("-f".into());
            rargs.push("segment".into());
            rargs.push("-segment_time".into());
            rargs.push(engine.config.lock().unwrap().storage.segment_seconds.to_string());
            rargs.push("-reset_timestamps".into());
            rargs.push("1".into());
            rargs.extend(FRAGMENTED.map(String::from));
            rargs.push("-c".into());
            rargs.push("copy".into());
            rargs.push(seg_dir.join(format!("{seg_prefix}%06d.mp4")).to_string_lossy().into_owned());
            let rec_child: Option<std::process::Child> = match crate::child::ffmpeg()
                .args(&rargs)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(mut c) => {
                    crate::child::adopt(&c);
                    if let Some(e) = c.stderr.take() {
                        extra_tails.push(("direct recorder", drain_stderr(e)));
                    }
                    Some(c)
                }
                Err(err) => {
                    warn!("camera '{}': could not start direct recorder: {}", cfg.name, err);
                    None
                }
            };
            if let Some(c) = rec_child {
                let cb = Arc::new(Mutex::new(c));
                children.push(cb.clone());
                if let Some((_, t)) = extra_tails.last() {
                    rec_watch.push(RecWatch::new(cb, t.clone()));
                }
            }
        }
        // The sub stream, copied all along (named so it is indexed as the
        // sub stream: "<camera>-sub-<time>-seg").
        if let (true, Some(u)) = (sub_mode, sub_url.as_ref()) {
            let mut sc = cfg.clone();
            sc.url = u.clone();
            let mut rargs: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
            rargs.extend(ffmpeg_input_args(&camera_url(&sc)));
            rargs.extend(["-map", "0:v"].map(String::from));
            rargs.extend(["-f", "segment", "-segment_time"].map(String::from));
            rargs.push(segment_seconds.to_string());
            rargs.extend(["-reset_timestamps", "1", "-c", "copy"].map(String::from));
            rargs.extend(FRAGMENTED.map(String::from));
            rargs.push(seg_dir.join(format!("{slug_name}-sub-{run_ts}-seg%06d.mp4")).to_string_lossy().into_owned());
            match crate::child::ffmpeg().args(&rargs).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn() {
                Ok(mut c) => {
                    crate::child::adopt(&c);
                    if let Some(e) = c.stderr.take() {
                        extra_tails.push(("sub stream recorder", drain_stderr(e)));
                    }
                    let cb = Arc::new(Mutex::new(c));
                    children.push(cb.clone());
                    if let Some((_, t)) = extra_tails.last() {
                        rec_watch.push(RecWatch::new(cb, t.clone()));
                    }
                }
                Err(err) => warn!("camera '{}': could not start the sub stream recorder: {}", cfg.name, err),
            }
        }
        start_watchdog(children.clone(), handle.clone());

        let mut recorder: Option<std::process::Child> = None;
        let mut frames_over: u32 = 0;
        let mut event_start: Option<i64> = None;
        let mut event_score: f64 = 0.0;
        let mut best_frame: Vec<u8> = Vec::new();
        // Finished events waiting for post-roll; several can overlap when
        // short motion bursts end within one post-roll window.
        let mut pending: Vec<PendingEvent> = Vec::new();
        let mut ai_state = super::ai_check::AiState::default();
        ai_state.parked = super::ai_check::restore_parked(&engine, &cfg.name);

        let mut chunk = vec![0u8; 65536];
        let mut buf: Vec<u8> = Vec::new();

        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    // A recorder that stopped by itself while the stream goes
                    // on (a folder that refuses writes) would otherwise go
                    // unnoticed: the stream never ends.
                    if cfg.record && last_rec_check.elapsed() >= std::time::Duration::from_secs(5) {
                        last_rec_check = std::time::Instant::now();
                        // (Whether the folder is there is asked on the probe's own
                        // thread: a hung share must not stall frames.)
                        for w in rec_watch.iter_mut() {
                            if !w.done {
                                if w.child.lock().unwrap().try_wait().ok().flatten().is_some() {
                                    w.done = true;
                                    let text = w.tail.lock().unwrap().clone();
                                    if exit_needs_probe(&text, started.elapsed(), &mut || super::storage::real_probe_due(&folder_name, 600)) {
                                        spawn_folder_check(&folder_name, &folder_root, &seg_dir, 90, Some(w.bad.clone()));
                                    }
                                }
                            } else if w.due_renewal() {
                                // Dead from a folder problem and the stream
                                // goes on: keep the finding alive (no restart
                                // of a camera that is otherwise working).
                                spawn_folder_check(&folder_name, &folder_root, &seg_dir, 330, Some(w.bad.clone()));
                            }
                        }
                        if !motion_checked && recorder.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_some()) {
                            motion_checked = true;
                            motion_renew = std::time::Instant::now() + std::time::Duration::from_secs(300);
                            let text = extra_tails.iter().rev().find(|(n, _)| *n == "motion recorder").map(|(_, t)| t.lock().unwrap().clone()).unwrap_or_default();
                            if exit_needs_probe(&text, motion_started.elapsed(), &mut || super::storage::real_probe_due(&folder_name, 600)) {
                                spawn_folder_check(&folder_name, &folder_root, &seg_dir, 90, Some(motion_bad.clone()));
                            }
                        } else if motion_checked && motion_bad.load(Ordering::Relaxed) && std::time::Instant::now() >= motion_renew {
                            motion_renew = std::time::Instant::now() + std::time::Duration::from_secs(300);
                            spawn_folder_check(&folder_name, &folder_root, &seg_dir, 330, Some(motion_bad.clone()));
                        }
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    loop {
                        if buf.len() >= 3 && !(buf[0] == 0xFF && buf[1] == 0xD8 && buf[2] == 0xFF) {
                            if let Some(pos) = find_marker(&buf, 1) {
                                buf.drain(..pos);
                                continue;
                            }
                            break;
                        }
                        match find_marker(&buf, 3) {
                            Some(next) => {
                                let frame = buf[..next].to_vec();
                                buf.drain(..next);
                                process_frame(
                                    &handle, &engine, &cfg, &mut detector, frame,
                                    segment_seconds,
                                    &mut frames_over, &mut event_start, &mut event_score,
                                    &mut best_frame, &mut pending, &mut ai_state,
                                    &slug_name, &seg_dir, &clips_dir, &thumbs_dir,
                                );
                                if motion_only {
                                    let m = handle.state.read().unwrap().motion;
                                    if m && recorder.is_none() {
                                        let mut rargs: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
                                        rargs.extend(ffmpeg_input_args(&camera_url(&cfg)));
                                        rargs.push("-f".into());
                                        rargs.push("segment".into());
                                        rargs.push("-segment_time".into());
                                        rargs.push(segment_seconds.to_string());
                                        rargs.push("-reset_timestamps".into());
                                        rargs.push("1".into());
                                        rargs.extend(FRAGMENTED.map(String::from));
                                        if cfg.direct && picture.is_empty() {
                                            rargs.push("-c".into());
                                            rargs.push("copy".into());
                                        } else {
                                            if !picture.is_empty() {
                                                rargs.push("-vf".into());
                                                rargs.push(picture.clone());
                                            }
                                            rargs.extend(reencode_args(segment_seconds));
                                        }
                                        // Unique prefix per launch so restarts never overwrite earlier files.
                                        let rec_prefix = format!("{slug_name}-{}-rec-seg", Utc::now().timestamp_millis());
                                        rargs.push(seg_dir.join(format!("{rec_prefix}%06d.mp4")).to_string_lossy().into_owned());
                                        match crate::child::ffmpeg()
                                            .args(&rargs)
                                            .stdin(std::process::Stdio::piped())
                                            .stdout(std::process::Stdio::null())
                                            .stderr(std::process::Stdio::piped())
                                            .spawn()
                                        {
                                            Ok(mut c) => {
                                                crate::child::adopt(&c);
                                                if let Some(e) = c.stderr.take() {
                                                    // Kept per launch; a restart replaces the older one.
                                                    extra_tails.retain(|(n, _)| *n != "motion recorder");
                                                    extra_tails.push(("motion recorder", drain_stderr(e)));
                                                }
                                                recorder = Some(c);
                                                motion_started = std::time::Instant::now();
                                                motion_checked = false;
                                                motion_bad.store(false, Ordering::Relaxed);
                                            }
                                            Err(err) => warn!("camera '{}': motion-only recorder failed: {}", cfg.name, err),
                                        }
                                    } else if !m {
                                        if let Some(c) = recorder.take() {
                                            // Don't stall the frame loop while ffmpeg finalizes the MP4.
                                            thread::spawn(move || stop_recorder(c));
                                        }
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                }
                Err(e) => {
                    warn!("camera '{}': read error: {}", cfg.name, e);
                    break;
                }
            }
        }

        for cb in children.iter() {
            let mut c = cb.lock().unwrap();
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(c) = recorder.take() {
            stop_recorder(c);
        }
        {
            let mut st = handle.state.write().unwrap();
            st.online = false;
            st.motion = false;
        }
        {
            let mut st = handle.state.write().unwrap();
            st.reconnects += 1;
            // Keep the time it first went offline across retries.
            st.offline_since.get_or_insert(Utc::now().timestamp());
            st.online_since = None;
        }
        // Let the stderr readers collect ffmpeg's last lines (killed or exited,
        // the pipes close at once).
        thread::sleep(std::time::Duration::from_millis(if local { 200 } else { 100 }));
        let main_text = stderr_tail.map(|t| t.lock().unwrap().clone()).unwrap_or_default();
        if !local {
            let why = tail_summary(&main_text);
            let mut reason = ended_text(why.as_deref());
            // A recording folder this account cannot write explains it best
            // (only for cameras that record, and never for a folder that is
            // missing: that has its own "cannot read" problem).
            if cfg.record && folder_root.is_dir() {
                let extra_texts: Vec<String> = extra_tails.iter().map(|(_, t)| t.lock().unwrap().clone()).collect();
                let mut texts: Vec<&str> = vec![main_text.as_str()];
                texts.extend(extra_texts.iter().map(|t| t.as_str()));
                // The disk is awake now, so a real write is fair when any
                // ffmpeg of this camera hints at it; or when nothing came
                // through, but then at most every 10 minutes per folder.
                let no_frames = handle.state.read().unwrap().frames <= frames_at_start;
                let real = write_hint(&texts) || (no_frames && super::storage::real_probe_due(&folder_name, 600));
                let keep = (backoff * 2).min(60) as i64 + 30;
                if let Some(path) = super::storage::check_folder(&super::storage::SYSTEM_PROBES, &folder_name, &seg_dir, real, keep) {
                    reason = super::storage::unwritable_text(&folder_name, &path, true);
                    // Keep a different cause ffmpeg named (a refused sign-in).
                    if let Some(c) = tail_cause(&main_text) {
                        reason = format!("{reason} (ffmpeg: {c})");
                    }
                }
            }
            handle.state.write().unwrap().last_error = Some(reason);
            handle.state.write().unwrap().last_error_at = Some(Utc::now().timestamp());
            for (name, t) in &extra_tails {
                let text = t.lock().unwrap().clone();
                if let Some(w) = tail_summary(&text) {
                    warn!("camera '{}': {} ffmpeg: {}", cfg.name, name, w);
                }
            }
        }
        if local {
            if handle.state.read().unwrap().frames > frames_at_start {
                backoff = 3;
            }
            let text = main_text;
            let why = crate::local_source::explain_error(&cfg.url, &text, crate::local_source::host_os());
            if handle.active.load(Ordering::Relaxed) {
                fail_local(&handle, &cfg, why, &mut backoff);
            }
            continue;
        }
        if handle.active.load(Ordering::Relaxed) {
            // Nothing came through: wait longer each time (up to a minute),
            // so a camera refusing the sign-in is not asked every few seconds.
            if handle.state.read().unwrap().frames > frames_at_start {
                backoff = 3;
            }
            let why = tail_summary(&main_text);
            let reason = handle.state.read().unwrap().last_error.clone().unwrap_or_else(|| ended_text(why.as_deref()));
            warn!("camera '{}': {}, retrying in {}s", cfg.name, reason, backoff);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(backoff);
            while std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
                thread::sleep(std::time::Duration::from_millis(300));
            }
            if handle.state.read().unwrap().frames <= frames_at_start {
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

/// Record why a local source is not running and wait before retrying; the
/// wait doubles up to a minute so a missing device does not spin. Returns
/// early when the camera is disabled.
fn fail_local(handle: &Arc<CameraHandle>, cfg: &CameraConfig, why: String, backoff: &mut u64) {
    {
        let mut st = handle.state.write().unwrap();
        st.online = false;
        st.offline_since.get_or_insert(Utc::now().timestamp());
        st.online_since = None;
        if st.last_error.as_deref() != Some(why.as_str()) {
            warn!("camera '{}': {}, retrying in {}s", cfg.name, why, *backoff);
        }
        st.last_error = Some(why);
        st.last_error_at = Some(Utc::now().timestamp());
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(*backoff);
    *backoff = (*backoff * 2).min(60);
    while std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
        thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// Stop a recorder gracefully so the MP4 gets finalized: ask ffmpeg to quit
/// via stdin, wait up to ~3 s, then kill as a fallback.
pub(super) fn stop_recorder(mut c: std::process::Child) {
    if let Some(mut stdin) = c.stdin.take() {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
    }
    for _ in 0..30 {
        if let Ok(Some(_)) = c.try_wait() {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = c.kill();
    let _ = c.wait();
}

/// Kill ffmpeg when the camera is disabled (or its slot removed) so the
/// blocking read in camera_worker returns EOF and the worker can park.
pub(super) fn start_watchdog(
    children: Vec<Arc<Mutex<std::process::Child>>>,
    handle: Arc<CameraHandle>,
) {
    thread::spawn({
        let children = children.clone();
        let handle = handle.clone();
        move || {
            loop {
                thread::sleep(std::time::Duration::from_millis(300));
                if !handle.active.load(Ordering::Relaxed) {
                    for cb in children.iter() {
                        let mut c = cb.lock().unwrap();
                        let _ = c.kill();
                    }
                    break;
                }
                // If every ffmpeg exited on its own, stop watching; the worker
                // loop respawns them if the camera is still enabled.
                let mut alive = false;
                for cb in children.iter() {
                    let mut c = cb.lock().unwrap();
                    match c.try_wait() {
                        Ok(Some(_)) => {},
                        _ => { alive = true; },
                    }
                }
                if !alive {
                    break;
                }
            }
        }
    });
}

pub(super) fn find_marker(buf: &[u8], from: usize) -> Option<usize> {
    if buf.len() < 3 || from >= buf.len() {
        return None;
    }
    for i in from..buf.len() - 2 {
        if buf[i] == 0xFF && buf[i + 1] == 0xD8 && buf[i + 2] == 0xFF {
            return Some(i);
        }
    }
    None
}

/// A finished event waiting for its post-roll footage to be recorded.
pub(super) struct PendingEvent {
    start: i64,
    end: i64,
    score: f64,
    deadline: i64,
    /// JPEG of the highest-motion frame seen during the event.
    thumb: Vec<u8>,
    ai: super::ai_check::AiOutcome,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn process_frame(
    handle: &Arc<CameraHandle>,
    engine: &Arc<Engine>,
    cfg: &CameraConfig,
    detector: &mut MotionDetector,
    frame: Vec<u8>,
    segment_seconds: u64,
    frames_over: &mut u32,
    event_start: &mut Option<i64>,
    event_score: &mut f64,
    best_frame: &mut Vec<u8>,
    pending: &mut Vec<PendingEvent>,
    ai: &mut super::ai_check::AiState,
    slug_name: &str,
    seg_dir: &std::path::Path,
    clips_dir: &std::path::Path,
    thumbs_dir: &std::path::Path,
) {
    let motion_cfg = engine.config.lock().unwrap().motion.for_camera(cfg);
    let sensitivity = motion_cfg.sensitivity;
    let use_ai = super::ai_check::ai_active(engine, cfg);
    let ai_frame = if use_ai { Some(frame.clone()) } else { None };
    let (score, over, blobs) = if motion_cfg.algorithm == "frame-diff" {
        let s = detector.score(&frame, &cfg.zones, &cfg.ignore_zones).unwrap_or(0.0);
        (s, s >= sensitivity, Vec::new())
    } else {
        let p = crate::motion::ObjectParams {
            min_object: motion_cfg.min_object,
            max_object: motion_cfg.max_object,
            contrast: motion_cfg.contrast,
            min_travel: motion_cfg.min_travel,
        };
        detector.objects.set_polygons(&cfg.zone_polys, &cfg.ignore_polys);
        let r = detector.objects.analyze(&frame, &cfg.zones, &cfg.ignore_zones, &p).unwrap_or_default();
        if r.lighting_change {
            tracing::debug!("camera '{}': lighting change ignored (background reset)", cfg.name);
        }
        (r.score, r.triggered, r.blobs)
    };
    // Only the event's peak-motion frame is kept as its thumbnail.
    let candidate = if score > *event_score || best_frame.is_empty() { Some(frame.clone()) } else { None };
    {
        let mut st = handle.state.write().unwrap();
        st.latest_frame = frame;
        if !st.online {
            st.online_since = Some(Utc::now().timestamp());
            st.offline_since = None;
        }
        st.online = true;
        st.frame_seq += 1;
        st.frames += 1;
        // The native path counts frames as they arrive; ffmpeg only delivers decoded ones.
        if st.ingest != "native" {
            handle.rx_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        st.score = score;
        st.blobs = blobs;
    }

    let now = Utc::now().timestamp();
    // classic-style schedule: motion only fires inside the camera's arm window.
    let local = chrono::Local::now();
    let minute_of_day: u32 = local.hour() * 60 + local.minute();
    let cam_window = cfg.schedule.as_ref().map(|s| parse_schedule(s)).flatten();
    let global_window = motion_cfg.schedule.as_ref().map(|s| parse_schedule(s)).flatten();
    let win = if cam_window.is_some() { cam_window } else { global_window };
    // The camera's own detection: starts events in "camera"/"both" mode,
    // and always labels them.
    let (cam_active, cam_labels) = {
        let st = handle.state.read().unwrap();
        (st.camera_detections.active(), st.camera_detections.labels())
    };
    let over = match cfg.trigger.as_str() {
        "camera" => cam_active,
        "both" => over || cam_active,
        _ => over,
    };
    // Anything moving at all, armed or not (the no-motion alert).
    if over {
        handle.state.write().unwrap().last_activity_at = now;
    }
    if event_start.is_some() || over {
        for l in cam_labels {
            if !ai.camera_labels.contains(&l) {
                ai.camera_labels.push(l.clone());
            }
            // A person, vehicle or animal the camera reports waits for AI to
            // see it too (and AI keeps looking meanwhile); anything else
            // (a doorbell press ...) confirms at once.
            let own = crate::ai::Detection { label: l.clone(), score: 1.0, x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
            if super::ai_check::camera_label_needs_ai(engine, cfg, &l) {
                ai.add_camera_label(own);
            } else if !ai.found.iter().any(|d| d.label == l) {
                ai.found.push(own);
            }
        }
    }
    let shielded = engine.armed_now();
    let armed = engine.motion_enabled(cfg) && shielded && schedule_active(win, minute_of_day);
    // A loud sound (the camera's sound trigger) counts like motion, inside
    // the same arm window, even with motion detection off for this camera.
    let sound_armed = cfg.audio_trigger && shielded && schedule_active(win, minute_of_day);
    let (heard, sound_labels) = if cfg.audio_trigger {
        let st = handle.state.read().unwrap();
        (st.sound.active(Utc::now().timestamp_millis()), st.sound.labels.clone())
    } else {
        (false, Vec::new())
    };
    let heard = heard && sound_armed;
    if heard || (event_start.is_some() && !sound_labels.is_empty() && ai.sound) {
        // Named sounds first (they make the alert's title), then "sound".
        for (i, l) in sound_labels.iter().enumerate() {
            if !ai.camera_labels.contains(l) {
                ai.camera_labels.push(l.clone());
            }
            if !ai.found.iter().any(|d| &d.label == l) {
                let score = if i == 0 { 0.99 } else { 1.0 };
                ai.found.push(crate::ai::Detection { label: l.clone(), score, x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
            }
        }
        ai.sound |= heard;
    }
    let st_now = handle.state.read().unwrap();
    let manual = now < st_now.manual_until;
    // Triggered by another camera: records even if this camera's own motion
    // detection is off or outside its arm window, but never while disarmed.
    let remote = now < st_now.triggered_until && shielded;
    let fanout_due = now > st_now.fanout_at;
    drop(st_now);

    // Flush a finished event once its post-roll footage has been recorded.
    let (due, waiting): (Vec<PendingEvent>, Vec<PendingEvent>) =
        std::mem::take(pending).into_iter().partition(|p| now >= p.deadline);
    *pending = waiting;
    for p in due {
        // Clip building shells out to ffmpeg; keep it off the frame loop.
        let engine = engine.clone();
        let handle = handle.clone();
        let cfg = cfg.clone();
        let motion_cfg = motion_cfg.clone();
        let slug_name = slug_name.to_string();
        let seg_dir = seg_dir.to_path_buf();
        let clips_dir = clips_dir.to_path_buf();
        let thumbs_dir = thumbs_dir.to_path_buf();
        thread::spawn(move || {
            finalize_event(
                &engine, &handle, &cfg, segment_seconds,
                p.start, p.end, p.score, p.thumb, p.ai,
                &motion_cfg, &slug_name, &seg_dir, &clips_dir, &thumbs_dir,
            );
        });
    }

    // Only this camera's own motion passes the trigger on (no loops).
    if armed && over && event_start.is_some() && fanout_due && !cfg.also_trigger.is_empty() {
        handle.state.write().unwrap().fanout_at = now;
        engine.trigger_others(cfg, now);
    }
    if (armed && (over || manual)) || remote || heard {
        handle.state.write().unwrap().last_motion_at = now;
        *frames_over += 1;
        if let Some(f) = candidate {
            *best_frame = f;
        }
        *event_score = event_score.max(score);
        if event_start.is_none() && *frames_over >= motion_cfg.min_frames {
            *event_start = Some(now);
            handle.state.write().unwrap().motion = true;
        }
        // Cap rolling events so continuous motion still produces clips.
        if let Some(start) = *event_start {
            if now - start >= MAX_EVENT_SECONDS {
                pending.push(PendingEvent {
                    start,
                    end: now,
                    score: *event_score,
                    deadline: now + motion_cfg.post_roll as i64 + segment_seconds as i64,
                    thumb: std::mem::take(best_frame),
                    ai: super::ai_check::AiOutcome::take(ai, &super::ai_check::wanted_labels(engine, cfg)),
                });
                *event_start = Some(now);
                *event_score = 0.0;
            }
        }
    } else if event_start.is_some() && (armed || remote || sound_armed) && now - handle.state.read().unwrap().last_motion_at < motion_cfg.break_time as i64 {
        // Brief pause inside an event: keep it open until break_time passes.
        *frames_over = 0;
    } else {
        *frames_over = 0;
        if let Some(start) = *event_start {
            pending.push(PendingEvent {
                start,
                end: now,
                score: *event_score,
                deadline: now + motion_cfg.post_roll as i64 + segment_seconds as i64,
                thumb: std::mem::take(best_frame),
                ai: super::ai_check::AiOutcome::take(ai, &super::ai_check::wanted_labels(engine, cfg)),
            });
            handle.state.write().unwrap().motion = false;
            *event_start = None;
            *event_score = 0.0;
        } else {
            // A blip that never reached min_frames must not leak its score or
            // frame into the next event.
            best_frame.clear();
            *event_score = 0.0;
        }
    }
    // AI: check frames while the event is active.
    let mut person_pic: Option<(Vec<u8>, [f32; 4])> = None;
    // What this frame's AI check saw, for the named zones' tracker.
    let mut fed: Option<Vec<crate::ai::Detection>> = None;
    if let (Some(f), true) = (ai_frame.as_ref(), event_start.is_some()) {
        let before = ai.log.len();
        super::ai_check::maybe_check(engine, cfg, ai, f);
        if ai.log.len() > before {
            if let Some(c) = ai.log.last().filter(|c| c.error.is_none()) {
                fed = Some(c.seen.clone());
            }
        }
        // Live tiles: the best wanted object this look saw, once the event is confirmed.
        if ai.log.len() > before {
            let wanted = super::ai_check::wanted_labels(engine, cfg);
            let confirmed: Vec<&str> = ai.found.iter().filter(|d| wanted.contains(&d.label)).map(|d| d.label.as_str()).collect();
            let best = ai.log.last().and_then(|c| {
                c.seen.iter().filter(|d| d.score >= 0.4 && confirmed.contains(&d.label.as_str())).max_by(|a, b| a.score.total_cmp(&b.score))
            });
            if let Some(d) = best {
                handle.state.write().unwrap().ai_last = Some((d.label.clone(), d.score, Utc::now().timestamp_millis()));
            }
        }
        // A person: look for named faces in the live pictures right away.
        let biggest = if ai.log.len() > before {
            ai.log.last().and_then(|c| c.seen.iter().filter(|d| d.label == "person" && d.score >= 0.5).max_by(|a, b| (a.w * a.h).total_cmp(&(b.w * b.h))).map(|d| [d.x, d.y, d.w, d.h]))
        } else {
            None
        };
        let person_now = biggest.is_some();
        if let Some(b) = biggest {
            person_pic = Some((f.clone(), b));
        }
        if person_now && engine.config.lock().unwrap().faces.wants(&cfg.name) {
            super::faces::start_watch(engine, handle, &cfg.name);
        }
    }
    // Named zones: loitering in areas, crossing of lines.
    if let Some(f) = ai_frame.as_ref().filter(|_| super::zone_watch::has_zones(cfg)) {
        super::zone_watch::on_frame(engine, handle, cfg, ai, event_start.is_some(), fed, f);
    }
    // Camera tamper detection looks at one picture every few seconds.
    engine.tamper_feed(handle);
    // Packages left at the door (or taken) after someone was there.
    if cfg.packages == Some(true) {
        super::packages::on_frame(engine, handle, &cfg.name, event_start.is_some(), person_pic.as_ref().map(|(p, b)| (p.as_slice(), *b)));
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    fn cam(text: Option<&str>) -> CameraConfig {
        let mut c: CameraConfig = toml::from_str("name = \"Front: Door\"\nurl = \"rtsp://x/y\"\n").unwrap();
        c.short_name = "Door".into();
        c.overlay = true;
        c.overlay_text = text.map(String::from);
        c
    }

    #[test]
    fn overlay_fill_ins_and_safety() {
        assert_eq!(overlay_drawtext(&cam(None)), "Door %{localtime\\:%F} %{localtime\\:%T}");
        assert_eq!(overlay_drawtext(&cam(Some("{name} at {time}"))), "Front Door at %{localtime\\:%T}");
        // Text that could break the filter is dropped; unknown fill-ins vanish.
        assert_eq!(overlay_drawtext(&cam(Some("a'b:c%d\\e,f;g[h] {nope}x"))), "abcdefgh nopex");
        // Named overlay variables are live-view only: the whole {v:NAME} token
        // is dropped from burned-in text (not left as garbage).
        assert_eq!(overlay_drawtext(&cam(Some("{short} {v:outside_temp}C"))), "Door C");
        let f = picture_filters(&cam(Some("{short}")));
        // (A fontfile= option comes first when a usual font is found: see font_option.)
        assert!(f.starts_with("drawtext=") && f.contains("text='Door':") && f.contains("x=10:y=h-th-10"), "{f}");
        assert_eq!(f.contains("fontfile="), !font_option().is_empty(), "{f}");
        // A long text never ends inside a time expansion.
        let long = format!("{}{{time}}", "x".repeat(118));
        let t = overlay_drawtext(&cam(Some(&long)));
        assert!(!t.contains("%{localtime") || t.ends_with('}'), "{t}");
    }
}

#[cfg(test)]
mod fragmented_tests {
    use super::FRAGMENTED;
    use crate::ingest::splice::{SourceSegment, VirtualClip};

    /// Records 6 s with ffmpeg's segment muxer into 2 s files, with or without
    /// the recorders' fragmented-MP4 options, and returns them as the index
    /// would list them. None when ffmpeg (with libx264) isn't available.
    fn record(dir: &std::path::Path, fragmented: bool) -> Option<Vec<SourceSegment>> {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut cmd = std::process::Command::new(crate::ffmpeg_bin::program());
        cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=10:duration=6"]);
        cmd.args(["-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p"]);
        cmd.args(["-f", "segment", "-segment_time", "2", "-reset_timestamps", "1"]);
        if fragmented {
            cmd.args(FRAGMENTED);
        }
        cmd.arg(dir.join("cam-rec-seg%06d.mp4"));
        if !cmd.status().ok()?.success() {
            return None;
        }
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        files.sort();
        let t0 = 1_800_000_000_000i64;
        Some(files.into_iter().enumerate().map(|(i, path)| {
            let start_ms = t0 + i as i64 * 2000;
            SourceSegment { path, start_ms, end_ms: start_ms + 2000, low: false, sub: false }
        }).collect())
    }

    /// ffmpeg's recorders must write files the clip splicer can use: plain MP4
    /// (index at the end) gave no clip, so events recorded after a fallback to
    /// ffmpeg had nothing to play.
    #[test]
    fn ffmpeg_recordings_can_be_spliced_into_clips() {
        let base = std::env::temp_dir().join(format!("amber-ffseg-{}", std::process::id()));
        let Some(segs) = record(&base.join("frag"), true) else {
            eprintln!("skipped: no ffmpeg with libx264");
            return;
        };
        assert!(segs.len() >= 3, "{segs:?}");
        let t0 = segs[0].start_ms;
        let clip = VirtualClip::plan(&segs, t0 + 500, t0 + 5000).unwrap().expect("a clip from fragmented ffmpeg files");
        assert!(clip.len() > 0);
        // Without the options, ffmpeg's files can't be spliced (what the fix is for).
        let plain = record(&base.join("plain"), false).unwrap();
        assert!(VirtualClip::plan(&plain, t0 + 500, t0 + 5000).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}

type StderrTail = Arc<Mutex<String>>;

/// Read a child's stderr on its own thread so a full pipe never blocks ffmpeg;
/// only the last few KB are kept.
fn drain_stderr(e: std::process::ChildStderr) -> StderrTail {
    let tail: StderrTail = Arc::new(Mutex::new(String::new()));
    let t = tail.clone();
    thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(e).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&line);
            let mut s = t.lock().unwrap();
            s.push_str(&line);
            s.push('\n');
            if s.len() > 4096 {
                let from = s.len() - 2048;
                let cut = (from..s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(s.len());
                s.drain(..cut);
            }
        }
    });
    tail
}

/// Remove sign-in details from free text: any `scheme://user:pw@host` token
/// loses its userinfo (looked for only in the authority, up to the first '/',
/// '?' or '#', and up to its last '@'), and secret query values are hidden.
/// Punctuation after the URL (as in "...: 401") is kept.
pub(crate) fn redact_text(line: &str) -> String {
    line.split(' ')
        .map(|tok| {
            let Some(i) = tok.find("://") else { return tok.to_string() };
            let body_end = tok.trim_end_matches(|c: char| matches!(c, ':' | ',' | ';' | ')' | ']' | '"' | '\'' | '.')).len();
            let (body, tail) = tok.split_at(body_end.max(i + 3));
            let (head, rest) = body.split_at(i + 3);
            let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            let rest = match rest[..auth_end].rfind('@') {
                Some(at) => &rest[at + 1..],
                None => rest,
            };
            format!("{}{}", super::creds::redact_url(format!("{head}{rest}")), tail)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words that name what went wrong (the last such line is the cause).
const CAUSES: [&str; 17] = [
    "permission denied", "no such file or directory", "401", "403", "404", "unauthorized", "forbidden", "not found",
    "connection refused", "connection timed out", "timeout", "timed out", "invalid data found", "no route to host",
    "name or service not known", "could not resolve", "server returned",
];
/// Lines ffmpeg prints about the outcome, not the reason.
const GENERIC: [&str; 8] = [
    "nothing was written into output file", "error opening output file", "error opening input file", "conversion failed",
    "exiting normally", "error while opening encoder", "error initializing output stream", "error muxing",
];

/// "[out#0/segment @ 0x55d] text" -> "text".
fn strip_prefix_tag(l: &str) -> &str {
    if l.starts_with('[') {
        if let Some(end) = l.find("] ") {
            if l[..end].contains(" @ 0x") {
                return l[end + 2..].trim_start();
            }
        }
    }
    l
}

/// Cut `s` to at most `max` characters at a word boundary, with "…".
fn cut_words(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    let next_is_space = s.chars().nth(max).is_some_and(|c| c.is_whitespace());
    let cut = if next_is_space { head.as_str() } else { head.rsplit_once(char::is_whitespace).map(|(h, _)| h).filter(|h| !h.is_empty()).unwrap_or(head.as_str()) };
    format!("{}…", cut.trim_end())
}

/// Why ffmpeg stopped, from its stderr, credentials removed: the last line
/// that names a known cause, else the last meaningful lines (at most two).
/// None when nothing useful was printed.
pub(crate) fn tail_summary(text: &str) -> Option<String> {
    tail_parts(text).map(|(t, _)| t)
}

/// Whether any of these stderr texts says the recording folder refused writes.
pub(crate) fn write_hint(texts: &[&str]) -> bool {
    texts.iter().any(|t| {
        let l = t.to_ascii_lowercase();
        l.contains("permission denied") || l.contains("read-only")
    })
}

/// A recorder that exited by itself is worth a real write probe when its
/// stderr hints at the folder, or it did not last long.
/// `early_due` says whether an early exit without a hint may cost a probe
/// (rate limit); it is only asked when needed.
pub(crate) fn exit_needs_probe(tail: &str, ran_for: std::time::Duration, early_due: &mut dyn FnMut() -> bool) -> bool {
    write_hint(&[tail]) || (ran_for < std::time::Duration::from_secs(30) && early_due())
}

/// A recorder (direct or sub stream) being watched while the stream goes on.
struct RecWatch {
    child: Arc<Mutex<std::process::Child>>,
    tail: StderrTail,
    done: bool,
    /// Its folder refused writes the last time that was looked at.
    bad: Arc<std::sync::atomic::AtomicBool>,
    next_renewal: std::time::Instant,
}

impl RecWatch {
    fn new(child: Arc<Mutex<std::process::Child>>, tail: StderrTail) -> Self {
        RecWatch { child, tail, done: false, bad: Arc::new(std::sync::atomic::AtomicBool::new(false)), next_renewal: std::time::Instant::now() + std::time::Duration::from_secs(300) }
    }
    /// A recorder that died from a folder problem renews its finding every
    /// 5 minutes while it stays that way.
    fn due_renewal(&mut self) -> bool {
        if self.bad.load(Ordering::Relaxed) && std::time::Instant::now() >= self.next_renewal {
            self.next_renewal = std::time::Instant::now() + std::time::Duration::from_secs(300);
            return true;
        }
        false
    }
}

/// Folders with a probe outstanding: a second is not started meanwhile (a
/// hung share would otherwise collect a thread every few seconds).
static PROBING: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

/// Held while a folder's probe runs.
struct ProbeSlot(String);

impl ProbeSlot {
    fn take(folder: &str) -> Option<ProbeSlot> {
        PROBING.lock().unwrap().get_or_insert_with(Default::default).insert(folder.to_string()).then(|| ProbeSlot(folder.to_string()))
    }
}

impl Drop for ProbeSlot {
    fn drop(&mut self) {
        if let Some(s) = PROBING.lock().unwrap().as_mut() {
            s.remove(&self.0);
        }
    }
}

/// A real write probe of the folder, on a thread of its own (a slow disk or a
/// hung share must not stall the frame loop; the folder's existence is asked
/// there too). `bad` gets the answer. Skipped while one is already running.
fn spawn_folder_check(folder: &str, folder_root: &std::path::Path, seg_dir: &std::path::Path, keep_secs: i64, bad: Option<Arc<std::sync::atomic::AtomicBool>>) {
    let Some(slot) = ProbeSlot::take(folder) else { return };
    let (folder, root, seg_dir) = (folder.to_string(), folder_root.to_path_buf(), seg_dir.to_path_buf());
    thread::spawn(move || {
        let _slot = slot;
        if !root.is_dir() {
            return;
        }
        let found = super::storage::check_folder(&super::storage::SYSTEM_PROBES, &folder, &seg_dir, true, keep_secs).is_some();
        if let Some(b) = bad {
            b.store(found, Ordering::Relaxed);
        }
    });
}

/// The last line naming a known cause that is not about the folder itself
/// (permission denied, no such file), when there is one.
pub(crate) fn tail_cause(text: &str) -> Option<String> {
    clean_lines(text).iter().rev().find(|l| is_cause(l) && !is_folder_cause(l)).map(|l| cut_words(l, 240))
}

fn is_folder_cause(l: &str) -> bool {
    let low = l.to_ascii_lowercase();
    low.contains("permission denied") || low.contains("no such file or directory")
}

fn is_cause(l: &str) -> bool {
    let low = l.to_ascii_lowercase();
    CAUSES.iter().any(|c| if c.chars().all(|ch| ch.is_ascii_digit()) { low.split(|ch: char| !ch.is_ascii_alphanumeric()).any(|w| w == *c) } else { low.contains(c) })
}

/// ffmpeg's stderr without banner lines and tags, credentials removed.
fn clean_lines(text: &str) -> Vec<String> {
    const SKIP: [&str; 4] = ["ffmpeg version", "built with", "configuration:", "press [q]"];
    text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .filter(|l| {
            let low = l.to_ascii_lowercase();
            // Banner library versions look like "libavutil      58. 29.100".
            let lib_banner = low.starts_with("lib") && low.contains(". ") && low.chars().any(|c| c.is_ascii_digit()) && low.split_whitespace().count() <= 4;
            !lib_banner && !SKIP.iter().any(|p| low.starts_with(p))
        })
        .map(|l| redact_text(strip_prefix_tag(l)))
        .collect()
}

fn tail_parts(text: &str) -> Option<(String, bool)> {
    let lines = clean_lines(text);
    let is_generic = |l: &str| {
        let low = l.to_ascii_lowercase();
        GENERIC.iter().any(|g| low.contains(g))
    };
    let mut found = false;
    let keep: Vec<&str> = match lines.iter().rposition(|l| is_cause(l)) {
        Some(i) => {
            found = true;
            vec![lines[i].as_str()]
        }
        None => {
            let specific: Vec<&String> = lines.iter().filter(|l| !is_generic(l)).collect();
            let pool: Vec<&str> = if specific.is_empty() { lines.iter().map(|l| l.as_str()).collect() } else { specific.into_iter().map(|l| l.as_str()).collect() };
            pool[pool.len().saturating_sub(2)..].to_vec()
        }
    };
    if keep.is_empty() {
        return None;
    }
    Some((cut_words(&keep.join("; "), 240), found))
}

/// Text for the log and the camera's last_error when a network stream ends.
pub(crate) fn ended_text(why: Option<&str>) -> String {
    match why {
        Some(w) => format!("stream ended (ffmpeg: {w})"),
        None => "stream ended".to_string(),
    }
}

#[cfg(test)]
mod tail_tests {
    use super::*;

    #[test]
    fn redacts_special_password() {
        let l = "[rtsp @ 0x1] rtsp://admin:p%40ss%2Fw%3Ard%3F%23x@192.168.1.5:554/stream1: 401 Unauthorized";
        let r = redact_text(l);
        assert!(!r.contains("p%40ss") && !r.contains("w%3Ard") && !r.contains("admin"), "{r}");
        assert!(r.contains("rtsp://192.168.1.5:554/stream1:"), "{r}");
        assert!(r.ends_with("401 Unauthorized"), "{r}");
        assert_eq!(redact_text("no url here"), "no url here");
        // '@' outside the authority is not userinfo.
        assert_eq!(redact_text("http://h/api?cmd=Snap&user=admin&password=ab@cd: 401"), "http://h/api?cmd=Snap&user=admin&password=***: 401");
        assert_eq!(redact_text("rtsp://h/stream@1: 404"), "rtsp://h/stream@1: 404");
        assert_eq!(redact_text("http://h/x?token=abc 404"), "http://h/x?token=*** 404");
        assert_eq!(redact_text("http://h/x?token=abc: 404"), "http://h/x?token=***: 404");
        assert_eq!(redact_text("rtsp://u:p%40ss@h:554/s, retry"), "rtsp://h:554/s, retry");
        assert_eq!(redact_text("rtsp://u:p@ss@h/s?x=1#a@b"), "rtsp://h/s?x=1#a@b");
        assert!(redact_text("http://h/x?password=secret&a=1").contains("password=***"));
    }

    #[test]
    fn tail_picks_last_meaningful_lines() {
        let t = "ffmpeg version 6.1 Copyright\n  built with gcc\n  configuration: --x\n\nfirst problem\nsecond\n\nrtsp://u:pw@h/s: 401 Unauthorized\n\n";
        assert_eq!(tail_summary(t).unwrap(), "rtsp://h/s: 401 Unauthorized");
        assert_eq!(tail_summary("first problem\nsecond\n").unwrap(), "first problem; second");
        assert!(tail_summary("\n  \nffmpeg version 7\n").is_none());
        assert!(tail_summary("").is_none());
        let long = "x".repeat(1000);
        assert!(tail_summary(&long).unwrap().len() < 260);
    }

    #[test]
    fn cause_beats_generic_tail_lines() {
        let t = "[segment @ 0x55d1] Failed to open segment '/recordings/cameras/a/segments/a-seg000000.mp4'\n\
                 [segment @ 0x55d1] Error opening '/recordings/cameras/a/segments/a-seg000000.mp4': Permission denied\n\
                 [out#0/segment @ 0x55d2] Nothing was written into output file, because at least one of its streams received no packets.\n\
                 [out#1/image2pipe @ 0x55d3] Nothing was written into output file, because at least one of its streams received no packets.\n";
        let s = tail_summary(t).unwrap();
        assert_eq!(s, "Error opening '/recordings/cameras/a/segments/a-seg000000.mp4': Permission denied");
        let t = "[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized\nError opening input file rtsp://u:pw@h/s.\nError opening input files: Server returned 401 Unauthorized (authorization failed)\n[out#0/segment @ 0x2] Nothing was written into output file, because at least one of its streams received no packets.\nConversion failed!\n";
        let s = tail_summary(t).unwrap();
        assert_eq!(s, "Error opening input files: Server returned 401 Unauthorized (authorization failed)");
        // Only generic lines: they are still shown.
        assert!(tail_summary("Conversion failed!\n").unwrap().contains("Conversion failed"));
    }

    #[test]
    fn cause_only_when_one_is_named() {
        assert_eq!(tail_cause("x\n[rtsp @ 0x1] 401 Unauthorized\nConversion failed!\n").unwrap(), "401 Unauthorized");
        assert!(tail_cause("Nothing was written into output file\nsomething odd\n").is_none());
    }

    #[test]
    fn cause_search_goes_backwards_past_folder_lines() {
        let t = "[rtsp @ 0x1] 401 Unauthorized\nError opening 'x.mp4': Permission denied\n[out#0 @ 0x2] Nothing was written into output file\n";
        assert_eq!(tail_cause(t).unwrap(), "401 Unauthorized");
        assert!(tail_cause("Error opening 'x.mp4': Permission denied\n").is_none());
    }

    #[test]
    fn the_folder_hint_reads_every_ffmpeg_of_the_camera() {
        let main = "Conversion failed!";
        let rec = "[segment @ 0x1] Error opening 'x.mp4': Permission denied";
        assert!(!write_hint(&[main]));
        assert!(write_hint(&[main, rec]));
        assert!(write_hint(&["Read-only file system"]));
    }

    #[test]
    fn only_one_probe_per_folder_runs_at_a_time() {
        let a = ProbeSlot::take("slot-test").expect("free");
        assert!(ProbeSlot::take("slot-test").is_none());
        assert!(ProbeSlot::take("slot-other").is_some());
        drop(a);
        assert!(ProbeSlot::take("slot-test").is_some());
    }

    #[test]
    fn an_early_or_telling_recorder_exit_is_probed() {
        use std::time::Duration;
        assert!(exit_needs_probe("anything", Duration::from_secs(3), &mut || true));
        // An early exit without a hint is probed only when the rate limit allows.
        assert!(!exit_needs_probe("anything", Duration::from_secs(3), &mut || false));
        let mut asked = false;
        assert!(exit_needs_probe("x: Permission denied", Duration::from_secs(900), &mut || { asked = true; false }));
        assert!(!asked, "a hint needs no rate limit");
        assert!(!exit_needs_probe("Connection reset", Duration::from_secs(900), &mut || true));
    }

    #[test]
    fn prefixes_are_stripped_and_cuts_are_at_words() {
        assert_eq!(tail_summary("[h264 @ 0x7f12ab] something odd happened\nthen [x @ 0x1] this").unwrap(), "something odd happened; then [x @ 0x1] this");
        let words = "word ".repeat(100);
        let s = tail_summary(&words).unwrap();
        assert!(s.ends_with("word…"), "{s}");
        assert!(s.chars().count() <= 241);
        assert_eq!(cut_words("short", 10), "short");
        assert_eq!(cut_words("alpha beta gamma", 8), "alpha…");
        assert_eq!(cut_words("alpha beta gamma", 10), "alpha beta…");
    }

    #[test]
    fn ended_text_forms() {
        assert_eq!(ended_text(Some("401 Unauthorized")), "stream ended (ffmpeg: 401 Unauthorized)");
        assert_eq!(ended_text(None), "stream ended");
    }
}
