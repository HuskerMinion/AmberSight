use super::*;
use std::io::Write;

/// Filters burned into the recordings: the camera's text when "Also burn
/// it into recordings" is on. Empty: the camera's own stream can be
/// recorded as it is.
pub fn recording_filters(cfg: &CameraConfig) -> String {
    if cfg.overlay {
        overlay_text_filter(cfg, crate::picture_text::Clock::Live)
    } else {
        String::new()
    }
}

/// Which of the camera's streams a recorder keeps: the first video stream,
/// and the first audio stream only when the camera records sound (ffmpeg on
/// its own takes an audio stream too). `copy`: the sound is copied as it
/// is, as before (made AAC, ffmpeg cannot write a recording until sound
/// arrives: a camera that lists sound but sends none recorded nothing);
/// else it is made AAC, as the re-encoding recorders do.
fn recorder_maps(audio: bool, copy: bool) -> Vec<String> {
    let mut a: Vec<String> = ["-map", "0:v:0"].map(String::from).to_vec();
    if audio {
        a.extend(["-map", "0:a:0?", "-c:a", if copy { "copy" } else { "aac" }].map(String::from));
    }
    a
}

/// The ffmpeg arguments of the direct recorder (`input`: its input
/// arguments; `out`: the segment file pattern): the camera's stream copied
/// as it is, its sound only when the camera records sound (ffmpeg on its
/// own takes a video and an audio stream, so sound was recorded with
/// "Record audio" off).
fn direct_recorder_args(input: Vec<String>, audio: bool, segment_seconds: u64, out: String) -> Vec<String> {
    let mut rargs: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    rargs.extend(input);
    rargs.extend(recorder_maps(audio, true));
    rargs.extend(["-f", "segment", "-segment_time"].map(String::from));
    rargs.push(segment_seconds.to_string());
    rargs.extend(["-reset_timestamps", "1"].map(String::from));
    rargs.extend(FRAGMENTED.map(String::from));
    rargs.extend(["-c:v", "copy"].map(String::from));
    rargs.push(out);
    rargs
}

/// The ffmpeg arguments of the sub stream recorder (`input`: its input
/// arguments; `out`: the segment file pattern): the sub stream copied as
/// it is.
fn sub_recorder_args(input: Vec<String>, segment_seconds: u64, out: String) -> Vec<String> {
    let mut rargs: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    rargs.extend(input);
    rargs.extend(["-map", "0:v:0"].map(String::from));
    rargs.extend(["-f", "segment", "-segment_time"].map(String::from));
    rargs.push(segment_seconds.to_string());
    rargs.extend(["-reset_timestamps", "1"].map(String::from));
    rargs.extend(["-c", "copy"].map(String::from));
    rargs.extend(FRAGMENTED.map(String::from));
    rargs.push(out);
    rargs
}

/// The camera's text (`overlay_text`, in its corner) as a drawtext filter,
/// its date and time from `clock`.
pub fn overlay_text_filter(cfg: &CameraConfig, clock: crate::picture_text::Clock) -> String {
    let (x, y) = match cfg.overlay_pos.as_deref().unwrap_or("bottom-left") {
        "top-left" => ("10", "10"),
        "top-right" => ("w-tw-10", "10"),
        "bottom-right" => ("w-tw-10", "h-th-10"),
        _ => ("10", "h-th-10"),
    };
    format!(
        "drawtext={}text='{}':fontcolor=white:fontsize=h/25+12:box=1:boxcolor=black@0.5:x={x}:y={y}",
        font_option(),
        overlay_drawtext_at(cfg, clock)
    )
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

/// The camera's text (`overlay_text`, at `overlay_pos`) burned into a
/// downloaded clip: its {date} and {time} are the recording's own wall time
/// (`base_s`: local wall time, seconds since the epoch with the time zone's
/// offset added, of the input's first frame), not the time of the encode.
pub fn overlay_download_drawtext(cfg: &CameraConfig, base_s: f64) -> String {
    let (x, y) = match cfg.overlay_pos.as_deref().unwrap_or("bottom-left") {
        "top-left" => ("10", "10"),
        "top-right" => ("w-tw-10", "10"),
        "bottom-right" => ("w-tw-10", "h-th-10"),
        _ => ("10", "h-th-10"),
    };
    let text = overlay_drawtext_at(cfg, crate::picture_text::Clock::Live)
        .replace("%{localtime\\:%F}", &format!("%{{pts\\:gmtime\\:{base_s:.3}\\:%F}}"))
        .replace("%{localtime\\:%T}", &format!("%{{pts\\:gmtime\\:{base_s:.3}\\:%T}}"));
    format!("drawtext={}text='{text}':fontcolor=white:fontsize=h/25+12:box=1:boxcolor=black@0.5:x={x}:y={y}", font_option())
}

/// Burned-in clock text for a clip whose first frame is at `start_ms`
/// (epoch) and that lasts `dur_ms`: `make` gives the drawtext filter for a
/// base (`start` in local wall time, as in [`overlay_download_drawtext`]).
/// When the local time zone's offset from UTC (`offset_s` at an epoch ms)
/// changes inside the clip (a daylight saving change), one filter per
/// stretch with its own offset, each shown only in its stretch, so the
/// clock stays right across the change.
pub fn clock_text(start_ms: i64, dur_ms: i64, offset_s: &dyn Fn(i64) -> i64, make: &dyn Fn(f64) -> String) -> String {
    let base = |off: i64| start_ms as f64 / 1000.0 + off as f64;
    let end = start_ms + dur_ms.max(0);
    // Where the offset changes (to the second), stepping a quarter hour.
    let mut cuts: Vec<(i64, i64)> = vec![(start_ms, offset_s(start_ms))];
    let mut at = start_ms;
    while at < end {
        let next = (at + 900_000).min(end);
        let (cur, then) = (cuts.last().unwrap().1, offset_s(next));
        if then != cur {
            let (mut lo, mut hi) = (at, next);
            while hi - lo > 1000 {
                let mid = lo + (hi - lo) / 2;
                if offset_s(mid) == cur {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            cuts.push((hi, then));
        }
        at = next;
    }
    if cuts.len() == 1 {
        return make(base(cuts[0].1));
    }
    let rel = |ms: i64| (ms - start_ms) as f64 / 1000.0;
    cuts.iter()
        .enumerate()
        .map(|(i, &(from, off))| {
            let to = cuts.get(i + 1).map(|c| format!("{:.3}", rel(c.0))).unwrap_or_else(|| "1e9".into());
            format!("{}:enable='gte(t,{:.3})*lt(t,{to})'", make(base(off)), rel(from))
        })
        .collect::<Vec<_>>()
        .join(",")
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
#[cfg(test)]
pub fn overlay_drawtext(cfg: &CameraConfig) -> String {
    overlay_drawtext_at(cfg, crate::picture_text::Clock::Live)
}

/// As [`overlay_drawtext`], the date and time from `clock`.
fn overlay_drawtext_at(cfg: &CameraConfig, clock: crate::picture_text::Clock) -> String {
    use crate::picture_text::Clock;
    // A fixed moment is written out (colons escaped for the filter); a
    // frame's own time is ffmpeg's pts clock from `base_s`.
    let at = |ms: i64, fmt: &str| {
        use chrono::TimeZone;
        chrono::Local.timestamp_millis_opt(ms).single().map(|t| t.format(fmt).to_string().replace(':', "\\:")).unwrap_or_default()
    };
    let date = match clock {
        Clock::Live => "%{localtime\\:%F}".to_string(),
        Clock::At(ms) => at(ms, "%Y-%m-%d"),
        Clock::Pts { base_s } => format!("%{{pts\\:localtime\\:{base_s:.3}\\:%F}}"),
    };
    let time = match clock {
        Clock::Live => "%{localtime\\:%T}".to_string(),
        Clock::At(ms) => at(ms, "%H:%M:%S"),
        Clock::Pts { base_s } => format!("%{{pts\\:localtime\\:{base_s:.3}\\:%T}}"),
    };
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
            (date.clone(), after.len() - r.len())
        } else if let Some(r) = after.strip_prefix("{time}") {
            (time.clone(), after.len() - r.len())
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

/// Full chain for single-output use (analysis pipe, CLI snapshot): the
/// burned-in text (when on), then `analysis_filter`.
pub fn video_filter(cfg: &CameraConfig) -> String {
    crate::picture_text::chain(&recording_filters(cfg), &analysis_filter(cfg))
}

/// Re-encoding arguments for recorded segments: H.264 with a keyframe every 1-2 s,
/// including at every segment boundary (time-based, so it works at any frame rate).
/// ffmpeg's segment muxer writes plain MP4 (the index at the end), which the
/// clip and playback splicer cannot use: fragmented MP4 like the built-in
/// reader writes.
const FRAGMENTED: [&str; 2] = ["-segment_format_options", "movflags=+frag_keyframe+empty_moov+default_base_moof"];

pub fn ffmpeg_input_args(url: &str) -> Vec<String> {
    ffmpeg_input_args_over(url, false)
}

/// ffmpeg's input arguments for one of this camera's streams, over UDP
/// when its `transport` setting (or a server remembered as UDP-only) says
/// so, or (`try_udp`) when this camera's last try over TCP got a 461.
fn input_args(cfg: &CameraConfig, try_udp: bool, url: &str) -> Vec<String> {
    let udp = crate::ingest::wants_udp(&cfg.transport, url) || (try_udp && crate::ingest::transport_setting(&cfg.transport) == "auto");
    ffmpeg_input_args_over(url, udp)
}

/// An RTSP input for ffmpeg of this major version (the socket timeout's
/// name changed in ffmpeg 5).
fn rtsp_input_args(url: &str, udp: bool, major: Option<u32>) -> Vec<String> {
    vec![
        "-rtsp_transport".into(),
        if udp { "udp" } else { "tcp" }.into(),
        crate::ffmpeg_bin::rtsp_timeout_flag(major).into(),
        "10000000".into(),
        "-i".into(),
        url.into(),
    ]
}

/// `udp`: RTSP over UDP instead of TCP.
pub fn ffmpeg_input_args_over(url: &str, udp: bool) -> Vec<String> {
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
        rtsp_input_args(url, udp, crate::ffmpeg_bin::major())
    } else {
        vec!["-i".into(), url.into()]
    }
}

/// Wait until `ready()` or for `max`, looking at the camera's `active`
/// meanwhile: false when it was stopped (its worker goes back to park).
fn wait_while_active(handle: &CameraHandle, max: std::time::Duration, ready: impl Fn() -> bool) -> bool {
    let until = std::time::Instant::now() + max;
    loop {
        if !handle.active.load(Ordering::SeqCst) {
            return false;
        }
        let now = std::time::Instant::now();
        if ready() || now >= until {
            return true;
        }
        thread::sleep((until - now).min(std::time::Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The waits before a session give way to a stop at once (a rename
    /// waits for the camera to park), and otherwise end when ready or after
    /// their time.
    #[test]
    fn waits_before_a_session_give_way_to_a_stop() {
        let h = super::super::make_handle(0, toml::from_str("name = \"Cam\"\nurl = \"rtsp://127.0.0.1/x\"").unwrap());
        let t = Instant::now();
        assert!(wait_while_active(&h, Duration::from_secs(5), || true), "ready");
        assert!(wait_while_active(&h, Duration::from_millis(150), || false), "its time is up");
        assert!(t.elapsed() >= Duration::from_millis(150) && t.elapsed() < Duration::from_secs(2));
        let h2 = h.clone();
        let stop = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            h2.active.store(false, Ordering::SeqCst);
        });
        let t = Instant::now();
        assert!(!wait_while_active(&h, Duration::from_secs(20), || false), "stopped");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        stop.join().unwrap();
        assert!(!wait_while_active(&h, Duration::from_secs(20), || true), "a stopped camera does not start");
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
    // (Hardware decoding that failed for this camera is remembered in
    // hwdecode: the way is tried again after a wait that grows each time.)
    // The same for the hardware encoder of a re-encoded recording (why it is
    // off), and for decoding on that card.
    let mut enc_off: Option<String> = None;
    let mut rec_dec_off = false;
    // The camera answered 461 over TCP: UDP next. Kept for the server
    // (other cameras, the built-in reader) only once UDP brought pictures.
    let mut try_udp = false;
    // A UDP try that brought nothing: a later 461 waits as usual.
    let mut udp_failed = false;
    // A sub stream that keeps failing: motion moves to the main stream.
    let mut sub_watch = super::ffpath::SubWatch::default();
    // The camera's place on the NVIDIA card while its decoder runs there.
    // Kept from one session to the next (a reconnect, a stream change) and
    // replaced only once the next session's decoder way is known, so a
    // camera on a nearly full card stays on it.
    // Only through short gaps, though: after `hwdecode::HOLD_THROUGH` with no
    // working session the place is let go (its memory shows as free and other
    // cameras may take it; back, the camera is judged as new).
    let mut _nvdec_hold: Option<crate::hwdecode::NvdecHold> = None;
    let mut outage = crate::hwdecode::Outage::default();

    // The worker lives for the lifetime of the camera slot: while the camera
    // is disabled it parks; when re-enabled it restarts ffmpeg.
    loop {
        // (Before `active` is looked at: see Engine::stop_for_rename.)
        handle.parked.store(false, Ordering::SeqCst);
        if !handle.active.load(Ordering::SeqCst) {
            handle.parked.store(true, Ordering::SeqCst);
            // Disabled: no decoder, no place on the card.
            _nvdec_hold = None;
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
        // Milliseconds, as the built-in reader names its files (files from
        // before 0.11 have seconds here; every reader of the names takes both).
        let run_ts = Utc::now().timestamp_millis();
        let seg_prefix = format!("{slug_name}-{run_ts}-seg");
        if outage.lapsed(std::time::Instant::now()) {
            _nvdec_hold = None;
        }

        // Direct mode stream-copies the source; lavfi sources have nothing
        // encoded to copy, so they fall back to inline re-encode segments.
        // Burned-in text changes the picture, so it rules out stream copy.
        let mut picture = recording_filters(&cfg);
        if let Some(cap) = crate::local_source::record_filter(&cfg.url) {
            picture = if picture.is_empty() { cap.to_string() } else { format!("{cap},{picture}") };
        }
        let direct = cfg.direct && cfg.record && !cfg.url.starts_with("lavfi://") && !local && picture.is_empty();
        // "Sub stream all the time, main around events": the sub stream is
        // copied into its own segments all along, and the main stream is
        // recorded as for motion-only.
        // (An address that cannot be opened is left out: see `sub_url_problem`.)
        let sub_url = if sub_watch.use_sub(std::time::Instant::now()) { super::usable_sub_url(&cfg) } else { None };
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
        // Hardware decoding: network cameras only (nothing to decode in a
        // synthetic source; capture devices give raw pictures).
        let netcam = !local && !cfg.url.starts_with("lavfi://");
        // Motion analysis decodes the sub stream when the camera has one (as
        // the built-in reader does): the large main stream is then only
        // recorded, by its own ffmpeg.
        let ana_sub = netcam && sub_url.is_some();
        let ana_cfg = match (&sub_url, ana_sub) {
            (Some(u), true) => CameraConfig { url: u.clone(), ..cfg.clone() },
            _ => cfg.clone(),
        };
        // (The waits before the session look at `active`: a camera stopped
        // meanwhile goes back to park at once. See Engine::stop_for_rename.)
        if netcam && !wait_while_active(&handle, std::time::Duration::from_secs(5), crate::hwdecode::probe_settled) {
            continue;
        }
        let hw = if netcam { crate::hwdecode::resolve(&cfg.name, cfg.hwaccel.as_deref(), None, std::time::Duration::ZERO) } else { Default::default() };
        if hw.spec.is_some() {
            // Hardware decoders start a little apart (not 16 at once).
            let now = std::time::Instant::now();
            let slot = crate::hwdecode::reserve_start(now);
            if !wait_while_active(&handle, slot.saturating_duration_since(now), || false) {
                continue;
            }
        }
        let mut hw_generation = crate::hwdecode::generation();
        let mut hw_recheck = std::time::Instant::now();
        let hw_args = hw.spec.as_deref().and_then(|s| crate::ingest::decode::hw_input_args(s, None).ok());
        // This session's decoder starts on that way: on NVDEC the headroom
        // guard counts the camera as on the card until the next session's
        // decoder has started (the old place is let go only after).
        let h = if netcam && (hw.spec.is_none() || hw_args.is_some()) { crate::hwdecode::started(&cfg.name, &hw) } else { None };
        _nvdec_hold = h;
        let mut hw_ok_noted = false;
        let download = if hw_args.is_some() { format!("{},", crate::ingest::decode::HW_DOWNLOAD) } else { String::new() };
        let segment_seconds = engine.config.lock().unwrap().storage.segment_seconds;
        // The full-size recording is re-encoded all along (text burned in,
        // or Direct copy off): on a hardware encoder when one
        // passed the test and has a free place, else libx264.
        let reencode_main = cfg.record && !direct && !motion_only;
        // One ffmpeg decodes the main stream for both the recording and motion.
        let single = reencode_main && !ana_sub;
        let mut rec_enc: Option<crate::transcode::Encoder> = None;
        let mut rec_slot: Option<crate::transcode::RecordingSlot> = None;
        let mut enc_note: Option<String> = None;
        if reencode_main && netcam {
            match &enc_off {
                Some(why) => enc_note = Some(why.clone()),
                None => match super::ffpath::pick_hw_encoder({
                    if !wait_while_active(&handle, std::time::Duration::from_secs(20), crate::transcode::encoders_ready) {
                        continue;
                    }
                    // Ready: it answers at once. Still not after the wait:
                    // it says the test has not finished.
                    if crate::transcode::encoders_ready() { std::time::Duration::from_secs(20) } else { std::time::Duration::ZERO }
                }) {
                    Ok((e, s)) => {
                        rec_enc = Some(e);
                        rec_slot = Some(s);
                    }
                    Err(why) => enc_note = Some(why),
                },
            }
        }
        let rec_hw_decode = rec_enc.is_some() && !rec_dec_off && cfg.hwaccel.as_deref() != Some("off");
        // Whether the camera's hardware decoding (hwaccel) is used: by the
        // analysis ffmpeg, and by a separate main-stream recorder on libx264.
        let ana_hw = hw_args.is_some() && !(single && rec_enc.is_some());
        let rec_label = rec_enc.as_ref().map(super::ffpath::encoder_label);
        let up = super::ffpath::rec_upload(rec_enc.as_ref()).map(|u| format!(",{u}")).unwrap_or_default();
        let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
        // Bounded threads (see ffpath::thread_budget: 6, fewer on a small
        // machine): a process limit must not stop a camera that is only
        // re-encoding.
        let budget = super::ffpath::thread_budget_now();
        args.extend(super::ffpath::graph_thread_args());
        if single && rec_enc.is_some() {
            args.extend(super::ffpath::rec_input_args(rec_enc.as_ref(), rec_hw_decode));
        } else if let (true, Some(h)) = (ana_hw, &hw_args) {
            args.extend(h.iter().cloned());
        }
        if netcam {
            args.extend(super::ffpath::decode_thread_args(budget));
        }
        args.extend(input_args(&cfg, try_udp, &camera_url(&ana_cfg)));
        if single {
            // One decode, two outputs: the full-resolution recording (with
            // the text burned in) and the small analysis stream.
            let pre = if picture.is_empty() { "null".to_string() } else { picture.clone() };
            let dl = if ana_hw { download.as_str() } else { "" };
            args.extend([
                "-filter_complex".to_string(),
                format!("[0:v]{dl}{pre},split=2[r0][a0];[r0]null{up}[rec];[a0]{}[ana]", analysis_filter(&cfg)),
                "-map".into(),
                "[rec]".into(),
            ]);
            if cfg.audio {
                args.extend(["-map", "0:a:0?", "-c:a", "aac"].map(String::from));
            }
            args.extend(super::ffpath::rec_encode_args(rec_enc.as_ref(), segment_seconds));
            args.extend(super::ffpath::encode_thread_args(rec_enc.as_ref(), budget));
            args.extend(["-f", "segment", "-segment_time"].map(String::from));
            args.push(segment_seconds.to_string());
            args.extend(["-reset_timestamps", "1"].map(String::from));
            args.extend(FRAGMENTED.map(String::from));
            args.push(seg_dir.join(format!("{seg_prefix}%06d.mp4")).to_string_lossy().into_owned());
            args.extend(["-map", "[ana]"].map(String::from));
        } else {
            args.push("-vf".into());
            // (On the card, frames are dropped there first, so only the
            // analysed ones are copied back.)
            args.push(if ana_hw { format!("fps={},{download}{}", cfg.fps.max(1), video_filter(&cfg)) } else { video_filter(&cfg) });
        }
        args.extend(super::ffpath::analysis_thread_args());
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
                let why = spawn_error_text(&e);
                warn!("camera '{}': could not start ffmpeg: {}", cfg.name, why);
                {
                    let mut st = handle.state.write().unwrap();
                    st.online = false;
                    if crate::proclimit::is_spawn_error(&e) {
                        st.last_error = Some(why);
                        st.last_error_at = Some(Utc::now().timestamp());
                    }
                }
                outage.ended(false, std::time::Instant::now());
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
        // An ffmpeg of this camera the system would not start (process or thread limit).
        let mut spawn_err: Option<String> = None;
        let frames_at_start = handle.state.read().unwrap().frames;
        // Over UDP this session because of an earlier 461 (or a server remembered so).
        let auto_transport = crate::ingest::transport_setting(&cfg.transport) == "auto";
        let udp_session = auto_transport && netcam && (try_udp || crate::ingest::wants_udp(&cfg.transport, &cfg.url));
        // Started while the graphics card test ran: once it is done, start
        // over (once) so the card can be used.
        let mut hw_restart = false;
        // A side try of a given-up sub stream, and whether it worked.
        let mut probe: Option<std::sync::mpsc::Receiver<Result<(), String>>> = None;
        let mut back_to_sub = false;
        let mut motion_meter = super::ffpath::SegMeter::default();
        let mut motion_prefix: Option<String> = None;
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
        // The main-stream recorder (analysis on the sub stream) stopped by
        // itself: its last words, and the session restarts.
        let mut rec_died: Option<String> = None;
        let mut meter = super::ffpath::SegMeter::default();

        // The main stream recorded by its own ffmpeg while motion decodes
        // the sub stream: re-encoded (text burned in), on the
        // hardware encoder when there is one.
        if reencode_main && ana_sub {
            let mut rargs: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
            rargs.extend(super::ffpath::graph_thread_args());
            let mut chain: Vec<String> = Vec::new();
            if rec_enc.is_some() {
                rargs.extend(super::ffpath::rec_input_args(rec_enc.as_ref(), rec_hw_decode));
            } else if let Some(h) = &hw_args {
                rargs.extend(h.iter().cloned());
                chain.push(crate::ingest::decode::HW_DOWNLOAD.to_string());
            }
            rargs.extend(super::ffpath::decode_thread_args(budget));
            rargs.extend(input_args(&cfg, try_udp, &camera_url(&cfg)));
            rargs.extend(["-map", "0:v:0"].map(String::from));
            if cfg.audio {
                rargs.extend(["-map", "0:a:0?", "-c:a", "aac"].map(String::from));
            }
            if !picture.is_empty() {
                chain.push(picture.clone());
            }
            if let Some(u) = super::ffpath::rec_upload(rec_enc.as_ref()) {
                chain.push(u.to_string());
            }
            if !chain.is_empty() {
                rargs.push("-vf".into());
                rargs.push(chain.join(","));
            }
            rargs.extend(super::ffpath::rec_encode_args(rec_enc.as_ref(), segment_seconds));
            rargs.extend(super::ffpath::encode_thread_args(rec_enc.as_ref(), budget));
            rargs.extend(["-f", "segment", "-segment_time"].map(String::from));
            rargs.push(segment_seconds.to_string());
            rargs.extend(["-reset_timestamps", "1"].map(String::from));
            rargs.extend(FRAGMENTED.map(String::from));
            rargs.push(seg_dir.join(format!("{seg_prefix}%06d.mp4")).to_string_lossy().into_owned());
            match crate::child::ffmpeg().args(&rargs).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn() {
                Ok(mut c) => {
                    crate::child::adopt(&c);
                    if let Some(e) = c.stderr.take() {
                        extra_tails.push(("recorder (main stream)", drain_stderr(e)));
                    }
                    let cb = Arc::new(Mutex::new(c));
                    children.push(cb.clone());
                    if let Some((_, t)) = extra_tails.last() {
                        let mut w = RecWatch::new(cb, t.clone());
                        w.main = true;
                        rec_watch.push(w);
                    }
                }
                Err(err) => {
                    warn!("camera '{}': could not start the main stream recorder: {}", cfg.name, spawn_error_text(&err));
                    spawn_err = spawn_err.or_else(|| crate::proclimit::is_spawn_error(&err).then(|| spawn_error_text(&err)));
                }
            }
        }

        // Direct mode: a second ffmpeg stream-copies the source into segments
        // while the analysis process decodes its own stream.
        if direct && !motion_only {
            let segment_seconds = engine.config.lock().unwrap().storage.segment_seconds;
            let rargs = direct_recorder_args(input_args(&cfg, try_udp, &camera_url(&cfg)), cfg.audio, segment_seconds, seg_dir.join(format!("{seg_prefix}%06d.mp4")).to_string_lossy().into_owned());
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
                    warn!("camera '{}': could not start direct recorder: {}", cfg.name, spawn_error_text(&err));
                    spawn_err = spawn_err.or_else(|| crate::proclimit::is_spawn_error(&err).then(|| spawn_error_text(&err)));
                    None
                }
            };
            if let Some(c) = rec_child {
                let cb = Arc::new(Mutex::new(c));
                children.push(cb.clone());
                if let Some((_, t)) = extra_tails.last() {
                    let mut w = RecWatch::new(cb, t.clone());
                    // With motion on the sub stream nothing else would notice it stop.
                    w.main = ana_sub;
                    rec_watch.push(w);
                }
            }
        }
        // The sub stream, copied all along (named so it is indexed as the
        // sub stream: "<camera>-sub-<time>-seg").
        if let (true, Some(u)) = (sub_mode, sub_url.as_ref()) {
            let mut sc = cfg.clone();
            sc.url = u.clone();
            let out = seg_dir.join(format!("{slug_name}-sub-{run_ts}-seg%06d.mp4")).to_string_lossy().into_owned();
            let rargs = sub_recorder_args(input_args(&cfg, try_udp, &camera_url(&sc)), segment_seconds, out);
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
                Err(err) => {
                    warn!("camera '{}': could not start the sub stream recorder: {}", cfg.name, spawn_error_text(&err));
                    spawn_err = spawn_err.or_else(|| crate::proclimit::is_spawn_error(&err).then(|| spawn_error_text(&err)));
                }
            }
        }
        start_watchdog(children.clone(), handle.clone());
        // Events of a motion-only camera are re-encoded too when the
        // picture changes or Direct copy is off.
        let motion_reencode = motion_only && netcam && !(cfg.direct && picture.is_empty());
        {
            // What runs for this camera (Status): the capture, and the recorders.
            let mut dec = handle.decode.lock().unwrap();
            dec.stream = Some(if ana_sub { "sub" } else { "main" });
            dec.codec = None;
            dec.gpu = if ana_hw {
                hw.spec.as_deref().map(crate::hwdecode::describe)
            } else if single && rec_hw_decode {
                rec_label.clone()
            } else {
                None
            };
            let sub_note = if sub_watch.trying() { Some("trying the sub stream again".to_string()) } else { sub_watch.off.clone() };
            dec.hw_setting = Some(if hw.camera_setting { "camera" } else { "server" });
            dec.note = super::ffpath::join_notes(hw.why_not.clone(), sub_note.as_ref());
            let mut procs = vec![if ana_sub { "motion decoder (sub stream)" } else { "capture" }];
            procs.extend(extra_tails.iter().map(|(n, _)| *n));
            if motion_only {
                procs.push("motion recorder (while there is motion)");
            }
            dec.procs = procs;
            let cause = if reencode_main || motion_reencode { super::ffpath::reencode_cause(&cfg) } else { None };
            let hw_for_events = if motion_reencode && enc_off.is_none() { super::ffpath::hw_encoder_available().map(|e| super::ffpath::encoder_label(&e)) } else { None };
            let label = rec_label.clone().or(hw_for_events);
            dec.reencode = cause.map(|c| super::ffpath::reencode_text(&cfg.name, c, label.as_deref()));
            dec.reencode_hw = cause.is_some() && label.is_some();
            dec.encoder = cause.map(|_| label.clone().unwrap_or_else(|| "libx264".into()));
            dec.encoder_note = if cause.is_some() && label.is_none() { enc_note.clone().or_else(|| enc_off.clone()) } else { None };
        }
        // (Held while the recording runs: freed when this session ends.)
        let _rec_slot = rec_slot;
        // The bitrate shown for the camera: how fast its continuous
        // recording grows (ffmpeg does not say what it receives).
        let meter_on = cfg.record && !motion_only;

        let mut recorder: Option<std::process::Child> = None;
        // The motion recorder's hardware encoder place, and whether it uses one.
        let mut recorder_slot: Option<crate::transcode::RecordingSlot> = None;
        let mut motion_hw: Option<String> = None;
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
                    if hw.pending && last_rec_check.elapsed() >= std::time::Duration::from_secs(5) && crate::hwdecode::probe_done() {
                        hw_restart = true;
                        break;
                    }
                    // The Hardware decoding setting changed, or (once a
                    // minute) a way that failed may be tried again: start
                    // over when the decoder would be another.
                    let changed = crate::hwdecode::generation() != hw_generation;
                    hw_generation = crate::hwdecode::generation();
                    // The hardware decoder has run well a while: earlier
                    // failures of that way no longer count toward giving it up.
                    if !hw_ok_noted && started.elapsed() >= crate::hwdecode::HEALTHY_RUN {
                        hw_ok_noted = true;
                        if let (Some(spec), Some(_)) = (hw.spec.as_deref(), &hw_args) {
                            crate::hwdecode::note_camera_ok(&cfg.name, spec, None);
                        }
                    }
                    if netcam && (changed || hw_recheck.elapsed() >= std::time::Duration::from_secs(60)) {
                        hw_recheck = std::time::Instant::now();
                        // (Only a look: the move is the next session's start.)
                        let now = crate::hwdecode::resolve(&cfg.name, cfg.hwaccel.as_deref(), None, std::time::Duration::ZERO);
                        // In this pipeline one ffmpeg (or its recorder beside
                        // it) decodes with the chosen way, so moving the camera
                        // to another way starts the session again: its recording
                        // restarts too, a few seconds' gap. That includes Auto's
                        // moves for the better and the headroom guard's move back
                        // onto NVDEC once there is 1 GB more room.
                        if crate::hwdecode::worth_moving(hw.spec.as_deref(), now.spec.as_deref(), changed, None) {
                            hw_restart = true;
                            break;
                        }
                    }
                    // A given-up sub stream is tried on the side when due; when
                    // it works the session starts over on it.
                    if probe.is_none() && sub_watch.probe_due(std::time::Instant::now()) {
                        if let Some(u) = super::usable_sub_url(&cfg) {
                            let (tx, rx) = std::sync::mpsc::channel();
                            let c = cfg.clone();
                            thread::spawn(move || {
                                let _ = tx.send(super::ffpath::probe_sub(&c, &u));
                            });
                            probe = Some(rx);
                        }
                    }
                    if let Some(rx) = &probe {
                        match rx.try_recv() {
                            Ok(outcome) => {
                                probe = None;
                                let (back, line) = sub_watch.probe_done(std::time::Instant::now(), outcome);
                                warn!("camera '{}': {line}", cfg.name);
                                if back {
                                    back_to_sub = true;
                                    break;
                                }
                                let mut d = handle.decode.lock().unwrap();
                                d.note = super::ffpath::replace_sub_note(d.note.take(), sub_watch.off.as_ref());
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => probe = None,
                            Err(std::sync::mpsc::TryRecvError::Empty) => {}
                        }
                    }
                    // A recorder that stopped by itself while the stream goes
                    // on (a folder that refuses writes) would otherwise go
                    // unnoticed: the stream never ends.
                    if cfg.record && last_rec_check.elapsed() >= std::time::Duration::from_secs(5) {
                        last_rec_check = std::time::Instant::now();
                        if meter_on {
                            handle.rx_bytes.fetch_add(meter.poll(&seg_dir, &seg_prefix), Ordering::Relaxed);
                        }
                        // A camera that records its main stream around events
                        // only (motion-only, sub stream all along): the
                        // bitrate of that main recording while it runs.
                        if let Some(p) = &motion_prefix {
                            handle.rx_bytes.fetch_add(motion_meter.poll(&seg_dir, p), Ordering::Relaxed);
                        }
                        // (Whether the folder is there is asked on the probe's own
                        // thread: a hung share must not stall frames.)
                        for w in rec_watch.iter_mut() {
                            if !w.done {
                                if w.child.lock().unwrap().try_wait().ok().flatten().is_some() {
                                    w.done = true;
                                    let text = w.tail.lock().unwrap().clone();
                                    if w.main && !write_hint(&[text.as_str()]) {
                                        // Motion goes on from the sub stream:
                                        // start over so recording resumes.
                                        rec_died = Some(text);
                                    } else if exit_needs_probe(&text, started.elapsed(), &mut || super::storage::real_probe_due(&folder_name, 600)) {
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
                        if rec_died.is_some() {
                            break;
                        }
                        if !motion_checked && recorder.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_some()) {
                            motion_checked = true;
                            motion_renew = std::time::Instant::now() + std::time::Duration::from_secs(300);
                            let text = extra_tails.iter().rev().find(|(n, _)| *n == "motion recorder").map(|(_, t)| t.lock().unwrap().clone()).unwrap_or_default();
                            // The graphics card would not encode it: the
                            // processor for the next events.
                            if let (Some(label), true) = (&motion_hw, super::ffpath::hw_encode_blamed(&text)) {
                                let why = hw_line(&text).unwrap_or_else(|| "it failed".into());
                                warn!("camera '{}': recording an event on {label} did not work ({why}); using the processor (libx264)", cfg.name);
                                enc_off = Some(format!("{label} did not work ({why}); using the processor"));
                                recorder_slot = None;
                            }
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
                                    &slug_name, &seg_dir, &clips_dir, &thumbs_dir, false,
                                );
                                if motion_only {
                                    let m = handle.state.read().unwrap().motion;
                                    if m && recorder.is_none() {
                                        // Re-encoded (text burned in, or Direct copy off): on
                                        // a hardware encoder with a free place, when one
                                        // passed the test (never waiting for it here).
                                        let copy = cfg.direct && picture.is_empty();
                                        let (enc, slot) = match (motion_reencode, &enc_off) {
                                            (true, None) => match super::ffpath::pick_hw_encoder(std::time::Duration::ZERO) {
                                                Ok((e, s)) => (Some(e), Some(s)),
                                                Err(_) => (None, None),
                                            },
                                            _ => (None, None),
                                        };
                                        let enc_hw_decode = enc.is_some() && cfg.hwaccel.as_deref() != Some("off") && !rec_dec_off;
                                        let mut rargs: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
                                        if !copy {
                                            rargs.extend(super::ffpath::graph_thread_args());
                                            rargs.extend(super::ffpath::rec_input_args(enc.as_ref(), enc_hw_decode));
                                            rargs.extend(super::ffpath::decode_thread_args(budget));
                                        }
                                        rargs.extend(input_args(&cfg, try_udp, &camera_url(&cfg)));
                                        rargs.extend(recorder_maps(cfg.audio, copy));
                                        rargs.push("-f".into());
                                        rargs.push("segment".into());
                                        rargs.push("-segment_time".into());
                                        rargs.push(segment_seconds.to_string());
                                        rargs.push("-reset_timestamps".into());
                                        rargs.push("1".into());
                                        rargs.extend(FRAGMENTED.map(String::from));
                                        if copy {
                                            rargs.push("-c:v".into());
                                            rargs.push("copy".into());
                                        } else {
                                            let mut chain: Vec<&str> = Vec::new();
                                            if !picture.is_empty() {
                                                chain.push(&picture);
                                            }
                                            if let Some(u) = super::ffpath::rec_upload(enc.as_ref()) {
                                                chain.push(u);
                                            }
                                            if !chain.is_empty() {
                                                rargs.push("-vf".into());
                                                rargs.push(chain.join(","));
                                            }
                                            rargs.extend(super::ffpath::rec_encode_args(enc.as_ref(), segment_seconds));
                                            rargs.extend(super::ffpath::encode_thread_args(enc.as_ref(), budget));
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
                                                motion_prefix = Some(rec_prefix.clone());
                                                motion_meter = super::ffpath::SegMeter::default();
                                                motion_hw = enc.as_ref().map(super::ffpath::encoder_label);
                                                recorder_slot = slot;
                                                motion_started = std::time::Instant::now();
                                                motion_checked = false;
                                                motion_bad.store(false, Ordering::Relaxed);
                                            }
                                            Err(err) => warn!("camera '{}': motion-only recorder failed: {}", cfg.name, err),
                                        }
                                    } else if !m {
                                        if let Some(c) = recorder.take() {
                                            // Don't stall the frame loop while ffmpeg finalizes the MP4.
                                            // (A rename waits for the file: Engine::writing_into.)
                                            let slot = recorder_slot.take();
                                            let writing = engine.writing_into(&cfg.name);
                                            thread::spawn(move || {
                                                stop_recorder(c);
                                                drop(slot);
                                                drop(writing);
                                            });
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

        // The main-stream recorder was still going when motion stopped (so
        // the sub stream, not the camera, ended the session).
        // Motion (the sub stream) stopped by itself: the sub stream's fault
        // alone only when the main-stream recorder wrote segments this
        // session and is still going a while later (an outage takes both down).
        let sub_alone = ana_sub
            && !hw_restart
            && !back_to_sub
            && rec_died.is_none()
            && handle.active.load(Ordering::Relaxed)
            && rec_watch.iter().find(|w| w.main).is_some_and(|w| {
                let alive = || w.child.lock().unwrap().try_wait().ok().flatten().is_none();
                let until = std::time::Instant::now() + SUB_GRACE;
                while alive() && std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
                    thread::sleep(std::time::Duration::from_millis(250));
                }
                // (A sub stream refused at once: the recorder has had the
                // grace time to write its first segment.)
                alive() && handle.active.load(Ordering::Relaxed) && seg_dir.join(format!("{seg_prefix}000000.mp4")).exists()
            });
        for cb in children.iter() {
            let mut c = cb.lock().unwrap();
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(c) = recorder.take() {
            stop_recorder(c);
        }
        drop(recorder_slot.take());
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
        {
            let mut dec = handle.decode.lock().unwrap();
            let note = dec.note.take();
            dec.stopped();
            dec.note = note;
        }
        let active = handle.active.load(Ordering::Relaxed);
        let got_frames = handle.state.read().unwrap().frames > frames_at_start;
        outage.ended(got_frames, std::time::Instant::now());
        if hw_restart && active {
            info!("camera '{}': starting again to decode on {}", cfg.name, crate::hwdecode::resolve(&cfg.name, cfg.hwaccel.as_deref(), None, std::time::Duration::ZERO).spec.as_deref().map(crate::hwdecode::describe).unwrap_or_else(|| "the processor".into()));
            continue;
        }
        if back_to_sub && active {
            continue;
        }
        // Nothing records the main stream all along to compare with: a
        // refusal of the sub stream (401/403/404) is its own fault (if the
        // main stream fails too, the camera says so, and the later try of
        // the sub stream sorts it out).
        let sub_alone = sub_alone || (ana_sub && !rec_watch.iter().any(|w| w.main) && super::ffpath::is_refusal(&main_text));
        // The sub stream (motion) stopped while the main stream recorded on:
        // after a refusal or a few sessions, motion moves to the main stream
        // for a while; a later try takes it back.
        if active {
            let why = tail_summary(&main_text).unwrap_or_else(|| "no pictures came".into());
            let failed = sub_alone.then_some((main_text.as_str(), why.as_str()));
            if let Some(line) = sub_watch.ended(std::time::Instant::now(), started.elapsed(), failed) {
                warn!("camera '{}': {line}", cfg.name);
                if sub_alone {
                    continue;
                }
            }
        }
        // The graphics card would not decode or encode the re-encoded
        // recording: at once again, first without its decoding, then on
        // the processor (libx264).
        let rec_text = if single { Some(main_text.clone()) } else { rec_died.clone() };
        if let (Some(label), Some(text), true) = (&rec_label, &rec_text, active) {
            if super::ffpath::hw_encode_blamed(text) {
                let why = hw_line(text).unwrap_or_else(|| "it failed".into());
                if rec_hw_decode {
                    rec_dec_off = true;
                    warn!("camera '{}': decoding for the recording on {label} did not work ({why}); decoding on the processor, encoding on {label}", cfg.name);
                } else {
                    warn!("camera '{}': recording on {label} did not work ({why}); using the processor (libx264)", cfg.name);
                    enc_off = Some(format!("{label} did not work ({why}); using the processor"));
                }
                continue;
            }
        }
        // A camera that refuses TCP (461 Unsupported Transport) under
        // transport = "auto": again over UDP. The server is remembered as
        // UDP-only only once UDP brought pictures, and forgotten when UDP
        // brought nothing.
        if auto_transport && netcam && active {
            let urls: Vec<&String> = std::iter::once(&cfg.url).chain(sub_url.as_ref()).collect();
            match udp_transport_step(udp_session, got_frames, || {
                std::iter::once(main_text.clone()).chain(extra_tails.iter().map(|(_, t)| t.lock().unwrap().clone())).any(|t| crate::ingest::is_unsupported_transport(&t))
            }) {
                UdpStep::Keep => urls.iter().for_each(|u| crate::ingest::remember_udp(u)),
                UdpStep::Drop => {
                    urls.iter().for_each(|u| crate::ingest::forget_udp(u));
                    try_udp = false;
                    udp_failed = true;
                }
                UdpStep::Try => {
                    try_udp = true;
                    info!("camera '{}': the camera refused RTSP over TCP; asking over UDP", cfg.name);
                    if !udp_failed {
                        continue;
                    }
                }
                UdpStep::Nothing => {}
            }
        }
        // The card would not decode this camera: at once again, on the processor.
        let hw_text = match (&rec_died, single) {
            // (A separate main-stream recorder on libx264 decodes with hwaccel too.)
            (Some(t), false) if rec_enc.is_none() && hw_args.is_some() => format!("{main_text}\n{t}"),
            _ => main_text.clone(),
        };
        if (ana_hw || (hw_args.is_some() && rec_died.is_some() && rec_enc.is_none())) && hw_blamed(&hw_text) && active {
            let main_text = hw_text;
            let why = hw_line(&main_text).unwrap_or_else(|| "it failed".into());
            let what = hw.spec.as_deref().map(crate::hwdecode::describe).unwrap_or_default();
            warn!("camera '{}': hardware decoding ({what}) did not work ({why})", cfg.name);
            // This camera leaves that way alone for a while (the next
            // session uses the next way, or the processor).
            if let Some(spec) = hw.spec.as_deref() {
                handle.decode.lock().unwrap().note = Some(crate::hwdecode::note_camera_failure(&cfg.name, spec, None, &why));
            }
            continue;
        }
        if !local {
            let why = tail_summary(rec_died.as_deref().unwrap_or(&main_text));
            let mut reason = ended_text(why.as_deref());
            // macOS's Local Network privacy (ffmpeg: "No route to host").
            if let Some(m) = crate::macnet::blocked(&main_text, &cfg.url) {
                reason = m;
            }
            // Out of processes or threads (a container's pids limit): any
            // ffmpeg of the camera saying so is the cause, not the -22
            // that ffmpeg ends with.
            let limit_hit = spawn_err.is_some() || crate::proclimit::is_spawn_limit(&main_text) || extra_tails.iter().any(|(_, t)| crate::proclimit::is_spawn_limit(&t.lock().unwrap()));
            if limit_hit {
                reason = spawn_err.clone().unwrap_or_else(crate::proclimit::limit_words_now);
            }
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
            // (A recorder that stopped by itself after running a good while
            // starts the waits over too; one that keeps stopping at once does not.)
            let ran_long = started.elapsed() >= std::time::Duration::from_secs(60);
            if got_frames && (rec_died.is_none() || ran_long) {
                backoff = 3;
            }
            let why = tail_summary(&main_text);
            if !got_frames && ffmpeg_refused_password(&main_text) {
                // A clear refusal of the sign-in: not asked again until the
                // camera's settings are saved (that replaces this worker),
                // since cameras lock an account out after a few.
                let msg = "the camera refused the user name or password (RTSP 401/403). Not asking again until the camera's settings are saved";
                warn!("camera '{}': {msg}", cfg.name);
                {
                    let mut st = handle.state.write().unwrap();
                    st.last_error = Some(msg.to_string());
                    st.last_error_at = Some(Utc::now().timestamp());
                }
                while handle.active.load(Ordering::Relaxed) {
                    thread::sleep(std::time::Duration::from_millis(500));
                }
                continue;
            }
            let reason = handle.state.read().unwrap().last_error.clone().unwrap_or_else(|| ended_text(why.as_deref()));
            crate::camreport::note_end(&cfg.name, &reason, handle.state.read().unwrap().frames > frames_at_start);
            warn!("camera '{}': {}, retrying in {}s", cfg.name, reason, backoff);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(backoff);
            while std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
                thread::sleep(std::time::Duration::from_millis(300));
                if outage.lapsed(std::time::Instant::now()) {
                    _nvdec_hold = None;
                }
            }
            if !got_frames || (rec_died.is_some() && !ran_long) {
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

/// ffmpeg's own words for an RTSP server refusing the sign-in it sent
/// (ffmpeg answers a digest/basic challenge by itself before saying this).
/// Timeouts, refused connections and 5xx are not it.
pub(super) fn ffmpeg_refused_password(text: &str) -> bool {
    let l = text.to_ascii_lowercase();
    ["describe failed: 401", "describe failed: 403", "server returned 401 unauthorized", "server returned 403 forbidden"].iter().any(|p| l.contains(p))
}

/// After motion (the sub stream) stops, how long the main-stream recorder
/// must go on for it to be the sub stream's failure alone.
const SUB_GRACE: std::time::Duration = std::time::Duration::from_secs(12);

/// What a session's end means for RTSP over UDP under `transport = "auto"`.
#[derive(Debug, PartialEq, Eq)]
enum UdpStep {
    /// UDP brought pictures: remember the server as UDP-only.
    Keep,
    /// UDP brought nothing: forget it, TCP first next time.
    Drop,
    /// TCP got a 461: try UDP.
    Try,
    Nothing,
}

/// `udp`: the session was over UDP; `refused_tcp`: whether a 461 is in
/// ffmpeg's words (asked only for a TCP session).
fn udp_transport_step(udp: bool, got_frames: bool, refused_tcp: impl FnOnce() -> bool) -> UdpStep {
    match (udp, got_frames) {
        (true, true) => UdpStep::Keep,
        (true, false) => UdpStep::Drop,
        (false, _) if refused_tcp() => UdpStep::Try,
        _ => UdpStep::Nothing,
    }
}

/// Whether ffmpeg's stderr says the hardware decoder (not the camera) failed.
/// Not when the system would not start another thread: "Error while
/// opening decoder" after a refused pthread_create is the process limit,
/// and the card must not be given up for it.
fn hw_blamed(text: &str) -> bool {
    hw_line(text).is_some() && !crate::proclimit::is_spawn_limit(text)
}

/// The first line of ffmpeg's stderr about the hardware decoder.
fn hw_line(text: &str) -> Option<String> {
    const WORDS: [&str; 13] = ["vaapi", "qsv", "hwaccel", "hwdownload", "hardware", "device creation", "hw_frames", "hwframe", "d3d11", "dxva", "videotoolbox", "cuda", "impossible to convert"];
    text.lines().find(|l| {
        let low = l.to_ascii_lowercase();
        WORDS.iter().any(|w| low.contains(w))
    }).and_then(crate::hwdecode::reason_from_stderr)
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
    /// JPEG of the highest-motion frame seen during the event, and when (epoch ms).
    thumb: Vec<u8>,
    thumb_ms: Option<i64>,
    ai: super::ai_check::AiOutcome,
}

/// When the event's thumbnail (`best`) was seen: None when there is none or
/// the time kept is from before the event (`start`, s; with a margin for
/// the frames that opened it).
fn best_frame_ms(handle: &CameraHandle, start: i64, best: &[u8]) -> Option<i64> {
    let at = handle.state.read().unwrap().best_frame_ms;
    (!best.is_empty() && at >= (start - 60) * 1000).then_some(at)
}

/// Builds the clips of finished events, off the frame loop (clip building
/// shells out to ffmpeg).
#[allow(clippy::too_many_arguments)]
fn build_clips(
    engine: &Arc<Engine>,
    handle: &Arc<CameraHandle>,
    cfg: &CameraConfig,
    motion_cfg: &crate::config::MotionConfig,
    segment_seconds: u64,
    due: Vec<PendingEvent>,
    slug_name: &str,
    seg_dir: &std::path::Path,
    clips_dir: &std::path::Path,
    thumbs_dir: &std::path::Path,
) {
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
                p.start, p.end, p.score, p.thumb, p.thumb_ms, p.ai,
                &motion_cfg, &slug_name, &seg_dir, &clips_dir, &thumbs_dir, None,
            );
        });
    }
}

/// A battery camera going back to sleep: no more footage is coming, so an
/// open event ends now and every waiting clip is built at once (otherwise
/// they would wait for the next wake, maybe hours later).
#[allow(clippy::too_many_arguments)]
pub(super) fn close_events_for_sleep(
    handle: &Arc<CameraHandle>,
    engine: &Arc<Engine>,
    cfg: &CameraConfig,
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
    let now = Utc::now().timestamp();
    *frames_over = 0;
    if let Some(start) = event_start.take() {
        pending.push(PendingEvent {
            start,
            end: now,
            score: *event_score,
            deadline: now,
            thumb_ms: best_frame_ms(handle, start, best_frame),
            thumb: std::mem::take(best_frame),
            ai: super::ai_check::AiOutcome::take(ai, &super::ai_check::wanted_labels(engine, cfg)),
        });
        handle.state.write().unwrap().motion = false;
    }
    best_frame.clear();
    *event_score = 0.0;
    let due = std::mem::take(pending);
    build_clips(engine, handle, cfg, &motion_cfg, segment_seconds, due, slug_name, seg_dir, clips_dir, thumbs_dir);
}

/// Whether an on-demand camera going to sleep must first close an event
/// still open or build clips still waiting (a session that ended with an
/// error, not by idle_stop, which closes them itself).
pub(super) fn closes_at_sleep(event_open: bool, pending: usize) -> bool {
    event_open || pending > 0
}

/// Whether enough frames over the threshold open a new event. Not once the
/// session is `ending` (an on-demand camera going back to sleep): the event
/// would be closed at once and the push that started it wakes the camera
/// again, one event becoming two; the next session's frames start it.
pub(super) fn opens_event(frames_over: u32, min_frames: u32, ending: bool) -> bool {
    frames_over >= min_frames && !ending
}

/// A battery camera counts its picture's own motion only this long after its
/// last motion push ended: rain, insects in the night light or a swaying bush
/// would otherwise keep an event (and so the camera) going for ever.
pub(super) const BATTERY_PICTURE_WINDOW_MS: i64 = 90_000;
/// An event keeps a battery camera awake at most this long after its last
/// push ended; after that only viewers (or a new push) keep it awake.
pub(super) const BATTERY_EVENT_AWAKE_MS: i64 = 180_000;

/// When a battery camera's picture motion counts (see starts_event).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PictureWindow {
    /// When the last push's pulse ends (epoch ms).
    pub pulse_until_ms: i64,
    /// When the camera last woke (epoch ms): woken by a viewer, the Wake
    /// button or MQTT, no push came, but someone walking past counts too.
    pub woke_at_ms: i64,
    /// Someone watches it live (who keeps it awake anyway): its picture
    /// motion always counts.
    pub viewers: bool,
    /// AI confirmed a wanted object (a person, a vehicle ...) in the open
    /// event: someone walking about for minutes sends no new push, so the
    /// picture counts up to the BATTERY_EVENT_AWAKE_MS cap instead of only
    /// BATTERY_PICTURE_WINDOW_MS. (Rain and insects confirm nothing. Once
    /// confirmed, AI stops looking, so the confirmation's own time says
    /// nothing about whether the object is still there: the cap bounds it.)
    pub ai_confirmed: bool,
}

impl PictureWindow {
    pub(super) fn counts(&self, now_ms: i64) -> bool {
        let window = if self.ai_confirmed { BATTERY_EVENT_AWAKE_MS } else { BATTERY_PICTURE_WINDOW_MS };
        self.viewers || now_ms < self.pulse_until_ms.max(self.woke_at_ms) + window
    }
}

/// Whether the open event holds an object AI confirmed (with a box: the
/// camera's own reports and sounds have none) of a label the camera wants.
pub(super) fn ai_confirmed_object(found: &[crate::ai::Detection], wanted: &[String]) -> bool {
    found.iter().any(|d| d.w > 0.0 && wanted.contains(&d.label))
}

/// Whether this frame counts as motion that starts (or keeps) an event:
/// the picture's own motion (`video`), the camera's report (`camera`), or
/// both, as the camera's `trigger` says. A battery Argus (baichuan://)
/// ignores `trigger`: its PIR push wakes it for a moment only, so the push
/// must start the event itself, and while it is awake and streaming it
/// posts no pushes, so only the picture can show further motion, but only
/// within `window` (BATTERY_PICTURE_WINDOW_MS after the push or the wake,
/// longer once AI confirmed a wanted object, always while watched live).
fn starts_event(cfg: &CameraConfig, video: bool, camera: bool, now_ms: i64, window: PictureWindow) -> bool {
    if crate::ingest::baichuan::is_baichuan_url(&cfg.url) {
        return camera || (video && window.counts(now_ms));
    }
    match cfg.trigger.as_str() {
        "camera" => camera,
        "both" => video || camera,
        _ => video,
    }
}

/// Whether an on-demand camera is still needed: a viewer always keeps it
/// awake; an open event does too, except that a battery camera's event
/// stops counting BATTERY_EVENT_AWAKE_MS after its last push or wake
/// (`pulse_until_ms`: the later of the two). `held`:
/// something outside the picture still wants it recording: another
/// camera's trigger (`triggered_until`, refreshed a few seconds at a time
/// only while that camera has motion).
pub(super) fn on_demand_busy(viewers: bool, event_open: bool, held: bool, battery: bool, now_ms: i64, pulse_until_ms: i64) -> bool {
    viewers || held || (event_open && (!battery || now_ms < pulse_until_ms + BATTERY_EVENT_AWAKE_MS))
}

/// `held` for on_demand_busy (epoch s): another camera's trigger, or a
/// manual "record N minutes" (API or MQTT) still running.
pub(super) fn on_demand_held(triggered_until: i64, manual_until: i64, now: i64) -> bool {
    now < triggered_until || now < manual_until
}

#[cfg(test)]
mod trigger_tests {
    use super::*;

    #[test]
    fn a_battery_push_starts_an_event_whatever_the_trigger_says() {
        for trigger in ["video", "camera", "both"] {
            let mut cfg: CameraConfig = toml::from_str(&format!(
                "name = \"Garden\"\nurl = \"baichuan://192.0.2.10?uid=95270000ABCDEFGH\"\non_demand = true\ntrigger = \"{trigger}\""
            ))
            .unwrap();
            // A push whose pulse ends 10 s from now.
            let (now, pulse) = (1_000_000, 1_010_000);
            let w = PictureWindow { pulse_until_ms: pulse, ..Default::default() };
            assert!(starts_event(&cfg, false, true, now, w), "{trigger}: the push alone starts the event");
            assert!(starts_event(&cfg, true, false, now, w), "{trigger}: picture motion while awake counts too");
            assert!(!starts_event(&cfg, false, false, now, w));
            assert!(!starts_event(&cfg, true, false, pulse + BATTERY_PICTURE_WINDOW_MS, w), "{trigger}: not long after the push");
            // A network camera follows its setting.
            cfg.url = "rtsp://192.0.2.11/stream1".into();
            let w = PictureWindow::default();
            let (v, c) = (starts_event(&cfg, true, false, now, w), starts_event(&cfg, false, true, now, w));
            assert_eq!((v, c), match trigger { "video" => (true, false), "camera" => (false, true), _ => (true, true) }, "{trigger}");
        }
    }

    /// One simulated second at a time, the event and wake rules of the frame
    /// loop (process_frame) and the on-demand check (native_worker): steady
    /// picture motion from second 0 on, one push at second 0 whose pulse
    /// lasts 10 s. `force_motion` stands for something else that keeps the
    /// event open (another camera's trigger, a sound). Returns the second
    /// the camera may sleep and whether an event was still open then.
    fn simulate(viewers: bool, force_motion: bool, until: i64) -> Option<(i64, bool)> {
        simulate_with(viewers, force_motion, None, until)
    }

    /// `confirmed_at`: the second AI confirms a wanted object in the event.
    fn simulate_with(viewers: bool, force_motion: bool, confirmed_at: Option<i64>, until: i64) -> Option<(i64, bool)> {
        let cfg: CameraConfig = toml::from_str(
            "name = \"Garden\"\nurl = \"baichuan://192.0.2.10?uid=95270000ABCDEFGH\"\non_demand = true",
        )
        .unwrap();
        let (break_time, idle) = (20, crate::config::idle_secs(cfg.idle_timeout));
        let pulse_until_ms = 10_000;
        let (mut event_start, mut last_motion, mut wake_until) = (None::<i64>, 0i64, 0i64);
        for t in 0..until {
            let now_ms = t * 1000;
            let camera = now_ms < pulse_until_ms;
            // native_worker's check comes first in each pass.
            let busy = on_demand_busy(viewers, event_start.is_some(), false, true, now_ms, pulse_until_ms);
            if busy || camera {
                wake_until = wake_until.max(t + idle);
            }
            if t >= wake_until {
                return Some((t, event_start.is_some()));
            }
            let w = PictureWindow { pulse_until_ms, ai_confirmed: event_start.is_some() && confirmed_at.is_some_and(|c| t >= c), viewers, woke_at_ms: 0 };
            if starts_event(&cfg, true, camera, now_ms, w) || force_motion {
                last_motion = t;
                let start = *event_start.get_or_insert(t);
                if t - start >= MAX_EVENT_SECONDS {
                    event_start = Some(t); // rolls over, never None
                }
            } else if event_start.is_some() && t - last_motion < break_time {
            } else {
                event_start = None;
            }
        }
        None
    }

    #[test]
    fn steady_picture_motion_does_not_keep_a_battery_camera_awake() {
        let idle = crate::config::idle_secs(300);
        // Rain all night: the picture counts for 90 s after the push, the
        // event ends a break time later, and the camera sleeps once idle.
        let (slept, open) = simulate(false, false, 3600).expect("the camera must be allowed to sleep");
        assert!(!open, "the event ended before sleep");
        assert!(slept <= 10 + BATTERY_PICTURE_WINDOW_MS / 1000 + 20 + idle + 1, "slept at {slept}");
        // Something else keeps the event open: the hard cap still lets it sleep
        // (the worker then closes the event and builds its clip).
        let (slept, open) = simulate(false, true, 3600).expect("the cap must let it sleep");
        assert!(open);
        assert!(slept <= 10 + BATTERY_EVENT_AWAKE_MS / 1000 + idle + 1, "slept at {slept}");
        // A live viewer keeps it awake.
        assert_eq!(simulate(true, false, 3600), None);
        assert_eq!(simulate(true, true, 3600), None);
    }

    #[test]
    fn a_confirmed_person_keeps_picture_motion_counting_up_to_the_cap() {
        let idle = crate::config::idle_secs(300);
        let w = PictureWindow { pulse_until_ms: 10_000, ai_confirmed: true, ..Default::default() };
        assert!(w.counts(10_000 + BATTERY_PICTURE_WINDOW_MS + 30_000), "past the plain window");
        assert!(!w.counts(10_000 + BATTERY_EVENT_AWAKE_MS), "never past the cap");
        // Someone walking about for many minutes, confirmed at second 5: the
        // event runs to the cap, then ends a break time later, and the camera sleeps.
        let (slept, open) = simulate_with(false, false, Some(5), 3600).expect("the cap must let it sleep");
        assert!(!open);
        let plain = simulate(false, false, 3600).unwrap().0;
        assert!(slept > plain, "confirmed {slept} vs plain {plain}");
        assert!(slept <= 10 + BATTERY_EVENT_AWAKE_MS / 1000 + 20 + idle + 1, "slept at {slept}");
        // Only an object AI saw (with a box) and the camera wants counts.
        let d = |label: &str, w: f32| crate::ai::Detection { label: label.into(), score: 0.9, x: 0.1, y: 0.1, w, h: w };
        let wanted = vec!["person".to_string(), "vehicle".to_string()];
        assert!(ai_confirmed_object(&[d("person", 0.2)], &wanted));
        assert!(!ai_confirmed_object(&[d("person", 0.0)], &wanted), "the camera's own report");
        assert!(!ai_confirmed_object(&[d("bird", 0.2)], &wanted), "not wanted");
        assert!(!ai_confirmed_object(&[], &wanted));
    }

    #[test]
    fn picture_motion_counts_after_a_wake_without_a_push_and_while_watched() {
        let cfg: CameraConfig = toml::from_str(
            "name = \"Garden\"\nurl = \"baichuan://192.0.2.10?uid=95270000ABCDEFGH\"\non_demand = true",
        )
        .unwrap();
        // Woken by the Wake button (or MQTT) at 1_000_000 ms; the last push was long before.
        let woke = PictureWindow { pulse_until_ms: 5_000, woke_at_ms: 1_000_000, ..Default::default() };
        assert!(starts_event(&cfg, true, false, 1_000_000 + 30_000, woke));
        assert!(!starts_event(&cfg, true, false, 1_000_000 + BATTERY_PICTURE_WINDOW_MS, woke), "only for a while");
        // A live viewer: any time.
        let watched = PictureWindow { pulse_until_ms: 5_000, viewers: true, ..Default::default() };
        assert!(starts_event(&cfg, true, false, i64::MAX / 2, watched));
        assert!(!starts_event(&cfg, false, false, i64::MAX / 2, watched), "still needs motion");
        // The awake cap counts from the wake too, and still ends.
        assert!(on_demand_busy(false, true, false, true, 1_000_000 + 60_000, 1_000_000));
        assert!(!on_demand_busy(false, true, false, true, 1_000_000 + BATTERY_EVENT_AWAKE_MS, 1_000_000));
    }

    #[test]
    fn no_new_event_starts_while_the_stream_ends_for_sleep() {
        assert!(opens_event(3, 3, false));
        assert!(!opens_event(2, 3, false));
        assert!(!opens_event(3, 3, true), "going back to sleep");
    }

    #[test]
    fn a_network_cameras_event_still_keeps_it_awake() {
        assert!(on_demand_busy(false, true, false, false, i64::MAX / 2, 0));
        assert!(!on_demand_busy(false, false, false, false, 0, 0));
        assert!(on_demand_busy(true, false, false, true, i64::MAX / 2, 0));
    }

    #[test]
    fn another_cameras_trigger_keeps_a_battery_camera_awake() {
        // Long after its last push, with the event open: only the trigger's hold counts.
        let late = 10_000 + BATTERY_EVENT_AWAKE_MS + 1;
        assert!(!on_demand_busy(false, true, false, true, late, 10_000));
        assert!(on_demand_busy(false, true, true, true, late, 10_000));
        assert!(on_demand_busy(false, false, true, true, late, 10_000));
        // In the native loop: held while now < triggered_until (seconds).
        assert!(on_demand_held(100, 0, 99) && !on_demand_held(100, 0, 100));
    }

    #[test]
    fn falling_asleep_after_a_failed_session_closes_the_open_event() {
        assert!(closes_at_sleep(true, 0), "an open event");
        assert!(closes_at_sleep(false, 2), "clips still waiting");
        assert!(!closes_at_sleep(false, 0), "nothing open");
    }

    #[test]
    fn a_manual_recording_keeps_a_battery_camera_awake_until_it_ends() {
        // "Record 5 minutes" at second 0: held to second 300, not past it.
        let manual_until = 300;
        for now in [0, 120, 299] {
            let held = on_demand_held(0, manual_until, now);
            assert!(on_demand_busy(false, true, held, true, now * 1000, 0), "second {now}");
        }
        let held = on_demand_held(0, manual_until, 300);
        assert!(!held && !on_demand_busy(false, true, held, true, 300_000, 0));
    }
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
    ending: bool,
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
    let (cam_active, cam_labels, pulse_until_ms, woke_at_ms) = {
        let st = handle.state.read().unwrap();
        (st.camera_detections.active(), st.camera_detections.labels(), st.camera_detections.pulse_until(), st.woke_at_ms)
    };
    let window = PictureWindow {
        pulse_until_ms,
        woke_at_ms,
        viewers: handle.live_main.viewers() + handle.live_sub.viewers() > 0,
        ai_confirmed: event_start.is_some()
            && crate::ingest::baichuan::is_baichuan_url(&cfg.url)
            && ai_confirmed_object(&ai.found, &super::ai_check::wanted_labels(engine, cfg)),
    };
    let over = starts_event(cfg, over, cam_active, Utc::now().timestamp_millis(), window);
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
    build_clips(engine, handle, cfg, &motion_cfg, segment_seconds, due, slug_name, seg_dir, clips_dir, thumbs_dir);

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
            handle.state.write().unwrap().best_frame_ms = Utc::now().timestamp_millis();
        }
        *event_score = event_score.max(score);
        if event_start.is_none() && opens_event(*frames_over, motion_cfg.min_frames, ending) {
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
                    thumb_ms: best_frame_ms(handle, start, best_frame),
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
                thumb_ms: best_frame_ms(handle, start, best_frame),
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
                // Camera actions next to alerts (spotlight, siren): only on what the AI itself saw now.
                crate::controls::automation::on_confirmed(engine, handle.id, cfg, &d.label, armed);
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
mod transport_arg_tests {
    use super::*;

    #[test]
    fn rtsp_transport_follows_the_setting() {
        let a = ffmpeg_input_args_over("rtsp://h/x", true);
        assert_eq!(a[..2], ["-rtsp_transport", "udp"]);
        assert_eq!(ffmpeg_input_args("rtsp://h/x")[..2], ["-rtsp_transport", "tcp"]);
        let mut c: CameraConfig = toml::from_str("name = \"a\"\nurl = \"rtsp://transport-test.invalid/x\"\ntransport = \"udp\"").unwrap();
        assert_eq!(input_args(&c, false, &c.url)[1], "udp");
        c.transport = "tcp".into();
        assert_eq!(input_args(&c, false, &c.url)[1], "tcp");
        c.transport = "auto".into();
        assert_eq!(input_args(&c, false, &c.url)[1], "tcp");
    }

    #[test]
    fn udp_is_kept_only_after_it_brought_pictures() {
        assert_eq!(udp_transport_step(false, false, || true), UdpStep::Try);
        assert_eq!(udp_transport_step(false, true, || false), UdpStep::Nothing);
        assert_eq!(udp_transport_step(true, true, || unreachable!()), UdpStep::Keep);
        assert_eq!(udp_transport_step(true, false, || unreachable!()), UdpStep::Drop);
        // A 461-looking number in ffmpeg's output is not a refusal.
        let said = "[h264 @ 0x1] cabac decode of qscale diff failed at 1461 > 1200";
        assert_eq!(udp_transport_step(false, true, || crate::ingest::is_unsupported_transport(said)), UdpStep::Nothing);
        let c: CameraConfig = toml::from_str("name = \"a\"\nurl = \"rtsp://try-udp.invalid/x\"").unwrap();
        assert_eq!(input_args(&c, true, &c.url)[1], "udp");
        let mut t = c.clone();
        t.transport = "tcp".into();
        assert_eq!(input_args(&t, true, &t.url)[1], "tcp", "tcp is tcp");
    }

    /// ffmpeg 4's `-timeout` on RTSP means "listen"; its socket timeout is `-stimeout`.
    #[test]
    fn rtsp_timeout_follows_the_ffmpeg_version() {
        assert_eq!(rtsp_input_args("rtsp://h/x", false, Some(4))[2..4], ["-stimeout", "10000000"]);
        assert_eq!(rtsp_input_args("rtsp://h/x", false, Some(5))[2..4], ["-timeout", "10000000"]);
        assert_eq!(rtsp_input_args("rtsp://h/x", true, None)[2], "-timeout");
        assert_eq!(crate::ffmpeg_bin::major_of("4.3.1"), Some(4));
        assert_eq!(crate::ffmpeg_bin::major_of("n4.4.2"), Some(4));
        assert_eq!(crate::ffmpeg_bin::major_of("8.0.1-3ubuntu2"), Some(8));
        assert_eq!(crate::ffmpeg_bin::major_of("N-112345-gabc"), None);
        assert_eq!(crate::ffmpeg_bin::rtsp_timeout_flag(Some(4)), "-stimeout");
        assert_eq!(crate::ffmpeg_bin::rtsp_timeout_flag(Some(7)), "-timeout");
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
        let mut c = cam(Some("{short}"));
        c.overlay = true;
        let f = recording_filters(&c);
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

    /// A camera recorded all along by the ffmpeg pipeline: its main-stream files, found by the
    /// folder scan (kind "legacy"), are what playback and the time-lapse read, named with the
    /// start in seconds (files from before 0.11) or in milliseconds. They were shown on the
    /// timeline but nothing played them, so playback said "No recording" outside events and
    /// the time-lapse found "no recordings".
    #[test]
    fn continuous_ffmpeg_recordings_play_and_make_time_lapses() {
        use crate::db::{Db, SegmentRow};
        use crate::engine::playback;
        let base = std::env::temp_dir().join(format!("amber-ffcont-{}", std::process::id()));
        let root = base.join("new");
        let seg_dir = root.join("cameras").join("office_wyze").join("segments");
        let Some(segs) = record(&seg_dir, true) else {
            eprintln!("skipped: no ffmpeg with libx264");
            return;
        };
        assert!(segs.len() >= 3, "{segs:?}");
        // Named as the pipeline names them, half with a 10-digit stamp (an older run) and half
        // with a 13-digit one; each file closed 2 s after the one before, a while ago.
        let t_end = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        let n = segs.len();
        for (i, s) in segs.iter().enumerate() {
            let name = if i < n / 2 { format!("office_wyze-1794754343-seg{i:06}.mp4") } else { format!("office_wyze-1794849248123-seg{:06}.mp4", i - n / 2) };
            let to = seg_dir.join(name);
            std::fs::rename(&s.path, &to).unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(&to).unwrap();
            f.set_modified(t_end - std::time::Duration::from_secs(2 * (n - 1 - i) as u64)).unwrap();
        }
        let db = Db::open(&base.join("t.db")).unwrap();
        let added = db.import_camera_dirs(&[("office_wyze".into(), "new".into(), root.clone())], 2).unwrap();
        assert_eq!(added, n);
        let resolve = |r: &SegmentRow| root.join(&r.path);
        let rows = db.segments_overlapping("office_wyze", 0, i64::MAX).unwrap();
        assert!(rows.iter().all(|r| r.kind == "legacy" && r.folder == "new"), "{rows:?}");
        let (from, to) = (rows[0].start_ms, rows[n - 1].end_ms);
        // The timeline shows them,
        assert_eq!(playback::coverage(&db, "Office Wyze", from, to).unwrap().segments, n);
        // playback plays them through, across the two runs (a player asks again from where
        // each clip ends),
        let mut at = from + 500;
        for _ in 0..n {
            if at >= to - 2500 {
                break;
            }
            let clip = playback::window_clip(&db, "Office Wyze", at, to - 500, resolve).unwrap().expect("playback from ffmpeg recordings");
            assert!(clip.len() > 0 && clip.start_ms < at + 2000 && clip.end_ms > at, "{} {} / {at} {to}", clip.start_ms, clip.end_ms);
            at = clip.end_ms;
        }
        assert!(at >= to - 2500, "{at} {to}");
        // and the time-lapse (fast pictures over the day, sub stream preferred) has pictures.
        let day = playback::sources_q(&db, "office_wyze", from - 3_600_000, to, true, resolve).unwrap();
        assert_eq!(day.len(), n);
        let fast = VirtualClip::plan_trick(&day, from, to, 4, 100).unwrap().expect("time-lapse pictures");
        assert!(fast.len() > 0);
        let _ = std::fs::remove_dir_all(&base);
    }
}

type StderrTail = Arc<Mutex<String>>;

/// Read a child's stderr on its own thread so a full pipe never blocks ffmpeg;
/// only the last few KB are kept.
pub(super) fn drain_stderr(e: std::process::ChildStderr) -> StderrTail {
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
/// loses its userinfo (found as in the settings, `creds::userinfo_at`: also
/// a password typed raw with a '/' in it), and secret query values are hidden.
/// Punctuation after the URL (as in "...: 401") is kept.
pub(crate) fn redact_text(line: &str) -> String {
    redact_text_with(line, false)
}

/// [`redact_text`]; `strict` for the camera report (see
/// `creds::shown_userinfo_at`: an '@' path after a plain host is masked too).
pub(crate) fn redact_text_with(line: &str, strict: bool) -> String {
    line.split(' ')
        .map(|tok| {
            let Some(i) = tok.find("://") else { return tok.to_string() };
            let body_end = tok.trim_end_matches(|c: char| matches!(c, ':' | ',' | ';' | ')' | ']' | '"' | '\'' | '.')).len();
            let (body, tail) = tok.split_at(body_end.max(i + 3));
            let (head, rest) = body.split_at(i + 3);
            // (Wider than the connection rule: sign-ins it leaves in place
            // on purpose are hidden too.)
            let rest = match super::creds::shown_userinfo_at(rest, strict) {
                Some(at) => &rest[at + 1..],
                None => rest,
            };
            format!("{}{}", super::creds::redact_url_with(format!("{head}{rest}"), strict), tail)
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
    /// The camera's continuous main-stream recording while motion decodes
    /// the sub stream: when it stops (not from its folder) the camera starts over.
    main: bool,
}

impl RecWatch {
    fn new(child: Arc<Mutex<std::process::Child>>, tail: StderrTail) -> Self {
        RecWatch { child, tail, done: false, bad: Arc::new(std::sync::atomic::AtomicBool::new(false)), next_renewal: std::time::Instant::now() + std::time::Duration::from_secs(300), main: false }
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
    // The system would not start another process or thread: that is the
    // cause, whatever ffmpeg printed after it (it ends with -22).
    if crate::proclimit::is_spawn_limit(text) {
        return Some((crate::proclimit::limit_words_now(), true));
    }
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

/// An error from starting ffmpeg, in words when the system's process or
/// thread limit is what stopped it.
fn spawn_error_text(e: &std::io::Error) -> String {
    if crate::proclimit::is_spawn_error(e) { crate::proclimit::limit_words_now() } else { e.to_string() }
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
    fn hardware_decoder_failures_are_told_from_camera_failures() {
        assert!(hw_blamed("[AVHWDeviceContext @ 0x1] Failed to initialise VAAPI connection: -1\nDevice creation failed: -5."));
        assert_eq!(hw_line("rtsp://x: Connection refused\n[h264 @ 0x2] Impossible to convert between the formats").as_deref(), Some("Impossible to convert between the formats"));
        assert!(!hw_blamed("rtsp://h/s: 401 Unauthorized\nError opening input"));
        assert!(!hw_blamed(""));
        // Out of threads with -hwaccel: the limit, not the card.
        let limit = "[h264 @ 0x1] hwaccel initialisation returned error.\npthread_create() failed: Resource temporarily unavailable\n[vist#0:0/h264 @ 0x1] Error while opening decoder: Resource temporarily unavailable";
        assert!(hw_line(limit).is_some());
        assert!(!hw_blamed(limit));
        // A full NVIDIA card: the decoder is blamed (this camera waits a
        // while), and the reason reads as memory trouble (never given up for).
        let oom = "[AVHWDeviceContext @ 0x55] cu->cuCtxCreate(&hwctx->cuda_ctx, desired_flags, hwdev) failed -> CUDA_ERROR_OUT_OF_MEMORY: out of memory\nDevice creation failed: -542398533.";
        assert!(hw_blamed(oom));
        assert!(crate::hwdecode::memory_trouble("cuda", &hw_line(oom).unwrap()), "{:?}", hw_line(oom));
    }

    #[test]
    fn redacts_special_password() {
        let l = "[rtsp @ 0x1] rtsp://admin:p%40ss%2Fw%3Ard%3F%23x@192.168.1.5:554/stream1: 401 Unauthorized";
        let r = redact_text(l);
        assert!(!r.contains("p%40ss") && !r.contains("w%3Ard") && !r.contains("admin"), "{r}");
        assert!(r.contains("rtsp://192.168.1.5:554/stream1:"), "{r}");
        assert!(r.ends_with("401 Unauthorized"), "{r}");
        assert_eq!(redact_text("no url here"), "no url here");
        // '@' outside the authority is not userinfo.
        assert_eq!(redact_text("http://h/api?cmd=Snap&user=admin&password=ab@cd: 401"), "http://h/api?cmd=Snap&user=***&password=***: 401");
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
    fn a_process_limit_is_the_cause_not_the_error_after_it() {
        // What ffmpeg printed in a container with a low --pids-limit.
        let t = "[libx264 @ 0x5581] pthread_create() failed: Resource temporarily unavailable\n\
                 [vost#0:0/libx264 @ 0x5582] Error while opening encoder - maybe incorrect parameters such as bit_rate, rate, width or height.\n\
                 [vf#0:0 @ 0x5583] Task finished with error code: -22 (Invalid argument)\n\
                 [vf#0:0 @ 0x5583] Terminating thread with return code -22 (Invalid argument)\n";
        let s = tail_summary(t).unwrap();
        assert!(s.starts_with("the system would not start another process or thread (Resource temporarily unavailable)"), "{s}");
        assert!(!s.contains("-22"), "{s}");
        assert!(ended_text(Some(&s)).starts_with("stream ended (ffmpeg: the system would not start"));
        let d = "[dec:hevc @ 0x55d1] Error while opening decoder: Resource temporarily unavailable\nError opening input file\n";
        assert!(tail_summary(d).unwrap().contains("process or thread"));
        // A refused sign-in is still a refused sign-in.
        assert!(tail_summary("[rtsp @ 0x1] 401 Unauthorized\n").unwrap().contains("401"));
        // Starting the recorder itself failed.
        #[cfg(unix)]
        {
            let e = std::io::Error::from_raw_os_error(libc::EAGAIN);
            let w = spawn_error_text(&e);
            assert!(w.starts_with("the system would not start another process or thread"), "{w}");
        }
        // (Error 11 on Windows is a bad program file: told as it is.)
        #[cfg(windows)]
        assert!(!spawn_error_text(&std::io::Error::from_raw_os_error(11)).starts_with("the system would not"));
        assert_eq!(spawn_error_text(&std::io::Error::new(std::io::ErrorKind::NotFound, "no ffmpeg")), "no ffmpeg");
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

#[cfg(test)]
mod recording_filter_tests {
    use super::*;

    fn cam() -> CameraConfig {
        toml::from_str("name = \"a\"\nurl = \"rtsp://192.0.2.1/x\"").unwrap()
    }

    #[test]
    fn only_burned_in_text_goes_into_the_recordings() {
        let mut c = cam();
        assert_eq!(recording_filters(&c), "", "the stream can be copied");
        assert!(video_filter(&c).starts_with("fps="));
        c.overlay = true;
        let r = recording_filters(&c);
        assert!(r.starts_with("drawtext"), "{r}");
        assert!(video_filter(&c).starts_with("drawtext"));
    }

    #[test]
    fn the_sub_stream_recorder_copies() {
        let input = vec!["-i".to_string(), "rtsp://192.0.2.1/sub".to_string()];
        let a = sub_recorder_args(input, 10, "out%06d.mp4".into());
        let j = a.join(" ");
        assert!(j.contains("-c copy") && j.contains("-map 0:v:0") && !j.contains("-vf"), "{j}");
        assert_eq!(a.last().map(String::as_str), Some("out%06d.mp4"));
    }

    #[test]
    fn the_direct_recorder_records_sound_only_when_asked() {
        let input = || vec!["-i".to_string(), "rtsp://192.0.2.1/main".to_string()];
        let off = direct_recorder_args(input(), false, 10, "out%06d.mp4".into()).join(" ");
        assert!(off.contains("-map 0:v:0") && !off.contains("0:a") && off.contains("-c:v copy"), "{off}");
        let on = direct_recorder_args(input(), true, 10, "out%06d.mp4".into()).join(" ");
        assert!(on.contains("-map 0:v:0") && on.contains("-map 0:a:0? -c:a copy") && on.contains("-c:v copy"), "{on}");
    }

    #[test]
    fn the_motion_recorder_records_sound_only_when_asked() {
        for copy in [true, false] {
            assert_eq!(recorder_maps(false, copy).join(" "), "-map 0:v:0");
        }
        assert_eq!(recorder_maps(true, true).join(" "), "-map 0:v:0 -map 0:a:0? -c:a copy");
        assert_eq!(recorder_maps(true, false).join(" "), "-map 0:v:0 -map 0:a:0? -c:a aac");
    }

    /// With ffmpeg: a source with sound, recorded with sound off, has no
    /// audio track; with sound on, it has one.
    #[test]
    fn a_direct_recording_has_no_sound_with_record_audio_off() {
        let ff = crate::ffmpeg_bin::program();
        let dir = std::env::temp_dir().join(format!("amber-direct-audio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        let made = std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=10:duration=2", "-f", "lavfi", "-i", "sine=duration=2"])
            .args(["-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&src)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("skipped: ffmpeg with libx264/aac not available");
            return;
        }
        // G.711 sound, as many cameras send (MP4 cannot hold it as it is).
        let g711 = dir.join("src.mkv");
        let made = std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-i"])
            .arg(&src)
            .args(["-c:v", "copy", "-c:a", "pcm_mulaw", "-ar", "8000", "-ac", "1"])
            .arg(&g711)
            .status();
        assert!(made.is_ok_and(|s| s.success()));
        let tracks = |src: &std::path::Path, tag: &str, audio: bool| {
            let out = dir.join(format!("{tag}{}-%06d.mp4", audio as u8));
            let input = vec!["-i".to_string(), src.to_string_lossy().into_owned()];
            let st = std::process::Command::new(&ff).args(direct_recorder_args(input, audio, 10, out.to_string_lossy().into_owned())).status().unwrap();
            assert!(st.success(), "{tag} audio {audio}: the recorder failed");
            let first = dir.join(format!("{tag}{}-000000.mp4", audio as u8));
            let probe = std::process::Command::new(&ff).args(["-hide_banner", "-i"]).arg(&first).output().unwrap();
            let info = String::from_utf8_lossy(&probe.stderr).into_owned();
            assert!(info.contains("Video:"), "{tag}: {info}");
            info.contains("Audio:")
        };
        assert!(!tracks(&src, "aac", false), "sound recorded with Record audio off");
        assert!(tracks(&src, "aac", true), "no sound with Record audio on");
        // (G.711 with sound on cannot be copied into MP4: as before, not yet handled here.)
        assert!(!tracks(&g711, "g711", false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod refused_password_tests {
    use super::ffmpeg_refused_password;

    #[test]
    fn only_a_clear_refusal_counts() {
        assert!(ffmpeg_refused_password("[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized\nError opening input files: Server returned 401 Unauthorized (authorization failed)\n"));
        assert!(ffmpeg_refused_password("[rtsp @ 0x1] method DESCRIBE failed: 403 Forbidden\n"));
        for t in [
            "[rtsp @ 0x1] Connection to tcp://192.0.2.5:554?timeout=0 failed: Connection refused\n",
            "[rtsp @ 0x1] method DESCRIBE failed: 503 Service Unavailable\n",
            "Error opening input files: Connection timed out\n",
            "[rtsp @ 0x1] method DESCRIBE failed: 404 Not Found\n",
            "",
        ] {
            assert!(!ffmpeg_refused_password(t), "{t}");
        }
    }
}
