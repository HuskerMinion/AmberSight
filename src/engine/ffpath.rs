//! What the ffmpeg pipeline does for a camera, in words and in arguments:
//! why the camera is read by ffmpeg, whether its recordings are re-encoded
//! (and on which encoder), and how fast its recording grows (the bitrate
//! shown for it).

use crate::config::CameraConfig;
use crate::transcode::{Encoder, Hw, OutCodec};

/// Why this camera is read by ffmpeg rather than the built-in reader, with
/// the setting that changes it where there is one. None: read natively.
/// `ingest`: what the camera runs on now; `fallback`: why the built-in
/// reader gave up (when it did).
pub fn why_ffmpeg(cfg: &CameraConfig, ingest: &str, fallback: Option<&str>) -> Option<String> {
    if !ingest.starts_with("ffmpeg") {
        return None;
    }
    if ingest.contains("fallback") {
        let why = fallback.map(|w| format!(" ({})", w.trim())).unwrap_or_default();
        return Some(format!(
            "the built-in reader could not play this camera{why}, so it switched to ffmpeg by itself. Saving the camera's settings tries the built-in reader again"
        ));
    }
    if crate::local_source::is_local(&cfg.url) {
        return Some("a local device or the screen is always read by ffmpeg".into());
    }
    let mut picture = Vec::new();
    if cfg.overlay {
        picture.push("\"Also burn it into recordings\" is on (Audio & text): burning text into the video needs ffmpeg. Turn it off to use the built-in reader");
    }
    if !cfg.privacy_masks.is_empty() {
        picture.push("the camera has privacy masks, which ffmpeg burns into the picture. Remove them (Motion > Edit zones) to use the built-in reader");
    }
    if !picture.is_empty() {
        return Some(picture.join("; and "));
    }
    if cfg.ingest == "ffmpeg" {
        return Some("its ingest setting is \"ffmpeg\" (ingest = \"ffmpeg\" in config.toml). Remove that line to use the built-in reader".into());
    }
    let scheme = cfg.url.split_once("://").map(|(s, _)| s).unwrap_or("");
    Some(if scheme.is_empty() { "the built-in reader does not read this kind of address".into() } else { format!("the built-in reader does not read {scheme}:// addresses") })
}

/// `why_ffmpeg` for a running camera, with the built-in reader's last
/// reason for giving up (in plain words).
pub fn why_ffmpeg_now(run: &CameraConfig, ingest: &str) -> Option<String> {
    let fallback = crate::camreport::fallback_reason(&run.name).map(|w| super::health::plain_error(&w)).filter(|w| !w.is_empty());
    why_ffmpeg(run, ingest, fallback.as_deref())
}

/// Why the full-size recording is re-encoded on the ffmpeg pipeline. None:
/// the camera's own stream is recorded as it is (or nothing is recorded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reencode {
    Overlay,
    Masks,
    /// Direct copy is off.
    NoDirect,
}

/// `picture`: the camera has burned-in filters (overlay or masks). Local
/// sources and synthetic ones always encode (there is nothing to copy).
pub fn reencode_cause(cfg: &CameraConfig) -> Option<Reencode> {
    if !cfg.record || crate::local_source::is_local(&cfg.url) || cfg.url.starts_with("lavfi://") {
        return None;
    }
    if cfg.overlay {
        Some(Reencode::Overlay)
    } else if !cfg.privacy_masks.is_empty() {
        Some(Reencode::Masks)
    } else if !cfg.direct {
        Some(Reencode::NoDirect)
    } else {
        None
    }
}

/// The Status text for a re-encoded camera. `encoder`: the hardware
/// encoder that does it ("NVENC" ...), None on the processor.
pub fn reencode_text(name: &str, cause: Reencode, encoder: Option<&str>) -> String {
    let (why, fix) = match cause {
        Reencode::Overlay => ("to burn in the text", "Turn off \"Also burn it into recordings\" (Audio & text) to record the camera's own stream, or keep it."),
        Reencode::Masks => ("to burn in the privacy masks", "Remove the masks (Motion > Edit zones) to record the camera's own stream, or keep them."),
        Reencode::NoDirect => ("because Direct copy is off", "Turn on Direct copy (Recording) to record the camera's own stream."),
    };
    match encoder {
        Some(e) => format!("{name}: recordings are re-encoded {why}, on the graphics card ({e}), so it costs little processor time. {fix}"),
        None => format!("{name}: recordings are re-encoded {why} (about one processor core per 4K camera). {fix}"),
    }
}

// ---------------------------------------------------------------------------
// The encoder of a re-encoded recording
// ---------------------------------------------------------------------------

/// A hardware encoder for one recording process and its place under
/// `[transcode] max_recording_hw` (apart from the live-view and download
/// transcodes' `max_hardware`), or why there is none. Waits up to `wait`
/// for the start-up test encodes. Blocks.
pub fn pick_hw_encoder(wait: std::time::Duration) -> Result<(Encoder, crate::transcode::RecordingSlot), String> {
    if wait.is_zero() && !crate::transcode::probed() {
        return Err("the test of the video encoders has not finished yet".into());
    }
    let t0 = std::time::Instant::now();
    let list = loop {
        if let Some(l) = crate::transcode::encoders_for(OutCodec::H264) {
            break l;
        }
        if t0.elapsed() >= wait {
            return Err("the test of the video encoders has not finished yet".into());
        }
    };
    let hw: Vec<Encoder> = list.into_iter().filter(|e| e.hw.is_some()).collect();
    if hw.is_empty() {
        let setting = crate::transcode::config().encoder;
        if setting == "software" || setting == "off" {
            return Err(format!("[transcode] encoder is set to \"{setting}\""));
        }
        return Err("no hardware video encoder passed the test at start".into());
    }
    match crate::transcode::reserve_recording() {
        Some(slot) => Ok((hw[0].clone(), slot)),
        None => Err("every place for recordings on the graphics card is in use ([transcode] max_recording_hw)".into()),
    }
}

/// The first hardware encoder that passed the test (not reserved), when
/// the test is done. Does not wait.
pub fn hw_encoder_available() -> Option<Encoder> {
    if !crate::transcode::probed() {
        return None;
    }
    crate::transcode::encoders_for(OutCodec::H264)?.into_iter().find(|e| e.hw.is_some())
}

/// "NVENC", "VAAPI on /dev/dri/renderD129", ...
pub fn encoder_label(e: &Encoder) -> String {
    let n = match e.hw {
        Some(Hw::Nvenc) => "NVENC",
        Some(Hw::Qsv) => "Quick Sync",
        Some(Hw::Vaapi) => "VAAPI",
        Some(Hw::Videotoolbox) => "VideoToolbox",
        Some(Hw::Amf) => "AMF",
        None => "libx264",
    };
    match &e.device {
        Some(d) => format!("{n} on {d}"),
        None => n.to_string(),
    }
}

/// Arguments before the input: decoding on the same card when `hw_decode`
/// (the pictures come back to memory for the burned-in filters), and the
/// encoder's device.
pub fn rec_input_args(e: Option<&Encoder>, hw_decode: bool) -> Vec<String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let Some(e) = e else { return Vec::new() };
    let dev = e.device.as_deref().unwrap_or("/dev/dri/renderD128");
    match (e.hw, hw_decode) {
        (Some(Hw::Nvenc), true) => s(&["-hwaccel", "cuda"]),
        (Some(Hw::Vaapi), true) => s(&["-init_hw_device", &format!("vaapi=rec:{dev}"), "-hwaccel", "vaapi", "-hwaccel_device", "rec", "-filter_hw_device", "rec"]),
        (Some(Hw::Qsv), true) if e.device.is_some() => s(&["-init_hw_device", &format!("vaapi=va:{dev}"), "-init_hw_device", "qsv=qs@va", "-hwaccel", "vaapi", "-hwaccel_device", "va", "-filter_hw_device", "qs"]),
        (Some(Hw::Amf), true) => s(&["-hwaccel", "d3d11va"]),
        (Some(Hw::Videotoolbox), true) => s(&["-hwaccel", "videotoolbox"]),
        _ => crate::transcode::device_args(e, false),
    }
}

/// What ends the filter chain before the encoder: the picture sent to the
/// card for VAAPI and Quick Sync, NV12 for the other hardware encoders.
pub fn rec_upload(e: Option<&Encoder>) -> Option<&'static str> {
    match e.and_then(|e| e.hw) {
        Some(Hw::Qsv) => Some("format=nv12,hwupload=extra_hw_frames=16"),
        Some(Hw::Vaapi) => Some("format=nv12,hwupload"),
        Some(_) => Some("format=nv12"),
        None => None,
    }
}

/// Encoder settings for recorded segments: constant quality, no B-frames,
/// and a keyframe at every segment boundary (time-based, so it works at any
/// frame rate). Software: libx264 as before.
pub fn rec_encode_args(e: Option<&Encoder>, segment_seconds: u64) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let s = |v: &mut Vec<String>, a: &[&str]| v.extend(a.iter().map(|x| x.to_string()));
    match e.and_then(|e| e.hw) {
        None => s(&mut v, &["-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p"]),
        Some(h) => {
            s(&mut v, &["-c:v", Encoder { hw: Some(h), device: None }.ffmpeg_name(OutCodec::H264), "-bf", "0"]);
            match h {
                Hw::Nvenc => s(&mut v, &["-preset", "p4", "-rc", "vbr", "-cq", "26", "-b:v", "0", "-forced-idr", "1"]),
                Hw::Qsv => s(&mut v, &["-preset", "veryfast", "-global_quality", "26", "-look_ahead", "0", "-forced_idr", "1"]),
                Hw::Vaapi => s(&mut v, &["-rc_mode", "CQP", "-qp", "26"]),
                Hw::Videotoolbox => s(&mut v, &["-q:v", "55", "-realtime", "1", "-allow_sw", "0"]),
                Hw::Amf => s(&mut v, &["-usage", "transcoding", "-rc", "cqp", "-qp_i", "24", "-qp_p", "26"]),
            }
        }
    }
    v.push("-force_key_frames".into());
    v.push(format!("expr:gte(t,n_forced*{})", segment_seconds.max(1)));
    v
}

/// Whether a recorder's ffmpeg error is about the hardware encoder or
/// decoder (not the camera or the folder).
pub fn hw_encode_blamed(text: &str) -> bool {
    const WORDS: [&str; 16] = ["nvenc", "cuda", "vaapi", "qsv", "hwupload", "hwaccel", "hw_frames", "hwframe", "device creation", "error while opening encoder", "could not open encoder", "d3d11", "amf", "videotoolbox", "no device available", "impossible to convert"];
    text.lines().any(|l| {
        let low = l.to_ascii_lowercase();
        WORDS.iter().any(|w| low.contains(w))
    })
}

// ---------------------------------------------------------------------------
// A sub stream that does not work
// ---------------------------------------------------------------------------

/// Sessions in a row the sub stream ended (the main stream fine) before
/// motion moves to the main stream.
pub const SUB_FAILS_MAX: u32 = 3;
/// A session this long counts as healthy: the failures before it are
/// forgotten, and a sub stream being tried again is taken back.
pub const SUB_HEALTHY: std::time::Duration = std::time::Duration::from_secs(180);
/// The first wait before a given-up sub stream is tried again; it doubles
/// up to `SUB_RETRY_MAX`.
pub const SUB_RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(15 * 60);
pub const SUB_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(2 * 3600);

/// Whether a sub stream that broke is given up on for a while. Only
/// failures of the sub stream alone count (the caller makes sure the main
/// stream was working; an outage that takes both down is not one): refused
/// (401/403) or missing (404) at once, anything else after `SUB_FAILS_MAX`
/// in a row. A healthy session forgets them. Given up, motion uses the main
/// stream, and the sub stream is tried again after 15 min, then 30, ... up to
/// 2 h; when a try works it is used again.
pub struct SubWatch {
    fails: u32,
    /// Given up: the note for Status and the camera settings.
    pub off: Option<String>,
    /// Why it was given up (for the note).
    why: String,
    /// When to try it again, and the wait after that try if it fails.
    retry_at: Option<std::time::Instant>,
    wait: std::time::Duration,
    /// This session tries the sub stream again.
    trying: bool,
    /// A side probe of the given-up sub stream brought pictures: the next
    /// session tries it.
    probe_ok: bool,
}

impl Default for SubWatch {
    fn default() -> Self {
        SubWatch { fails: 0, off: None, why: String::new(), retry_at: None, wait: SUB_RETRY_FIRST, trying: false, probe_ok: false }
    }
}

/// "15 min", "1 h", "1 h 30 min".
fn wait_words(d: std::time::Duration) -> String {
    let m = d.as_secs() / 60;
    match (m / 60, m % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

impl SubWatch {
    /// At the start of a session: whether to use the sub stream (it works,
    /// or it was given up and its next try is due).
    pub fn use_sub(&mut self, now: std::time::Instant) -> bool {
        if self.off.is_none() {
            return true;
        }
        if std::mem::take(&mut self.probe_ok) || self.retry_at.is_some_and(|t| now >= t) {
            self.trying = true;
            return true;
        }
        false
    }

    /// While motion uses the main stream: whether the given-up sub stream
    /// is due for a try on the side (`probe_sub`), the session going on.
    pub fn probe_due(&self, now: std::time::Instant) -> bool {
        self.off.is_some() && !self.probe_ok && self.retry_at.is_some_and(|t| now >= t)
    }

    /// The side try's outcome. Pictures came: true, and the caller starts
    /// the session again on the sub stream (still a try: a healthy session
    /// on it takes it back for good). Nothing: the next try waits twice as
    /// long. Returns the line to log.
    pub fn probe_done(&mut self, now: std::time::Instant, outcome: Result<(), String>) -> (bool, String) {
        match outcome {
            Ok(()) => {
                self.probe_ok = true;
                (true, "the sub stream answers again; motion moves back to it".into())
            }
            Err(why) => {
                self.why = why.trim().trim_end_matches('.').to_string();
                self.wait = (self.wait * 2).min(SUB_RETRY_MAX);
                self.give_up(now);
                (false, self.off.clone().unwrap_or_default())
            }
        }
    }

    /// Whether this session is a try of a given-up sub stream.
    pub fn trying(&self) -> bool {
        self.trying
    }

    fn give_up(&mut self, now: std::time::Instant) {
        self.retry_at = Some(now + self.wait);
        self.off = Some(format!(
            "the sub stream did not work ({}); motion uses the main stream, and the sub stream is tried again in {}",
            self.why, wait_words(self.wait)
        ));
    }

    /// A session ended. `ran`: how long it ran; `sub_failed`: the sub stream
    /// alone ended it (the main stream was working), with its error as given
    /// (`err`) and in words (`why`). Returns a line to log when the sub
    /// stream was given up or taken back.
    pub fn ended(&mut self, now: std::time::Instant, ran: std::time::Duration, sub_failed: Option<(&str, &str)>) -> Option<String> {
        let healthy = ran >= SUB_HEALTHY;
        if healthy {
            self.fails = 0;
        }
        if std::mem::take(&mut self.trying) {
            return match sub_failed {
                Some((_, why)) => {
                    // Still broken: wait twice as long before the next try.
                    self.why = why.trim().trim_end_matches('.').to_string();
                    self.wait = (self.wait * 2).min(SUB_RETRY_MAX);
                    self.give_up(now);
                    self.off.clone()
                }
                None if healthy => {
                    self.off = None;
                    self.retry_at = None;
                    self.wait = SUB_RETRY_FIRST;
                    Some("the sub stream works again; motion uses it again".into())
                }
                // Ended by the main stream before it was clear: try again next session.
                None => {
                    self.trying = true;
                    None
                }
            };
        }
        let (err, why) = sub_failed?;
        if self.off.is_some() {
            return None;
        }
        self.fails += 1;
        if is_refusal(err) || self.fails >= SUB_FAILS_MAX {
            self.why = why.trim().trim_end_matches('.').to_string();
            self.give_up(now);
            return self.off.clone();
        }
        None
    }
}

/// How long a side try of a sub stream may take, and the pictures it must bring.
pub const PROBE_TIME: std::time::Duration = std::time::Duration::from_secs(20);
const PROBE_FRAMES: &str = "5";

/// Try a sub stream on the side (its own ffmpeg, the camera's transport):
/// Ok when it brings a few pictures within `PROBE_TIME`. Blocks; run it on
/// its own thread.
pub fn probe_sub(cfg: &CameraConfig, sub_url: &str) -> Result<(), String> {
    let sc = CameraConfig { url: sub_url.to_string(), ..cfg.clone() };
    let url = crate::engine::camera_url(&sc);
    let udp = crate::ingest::wants_udp(&cfg.transport, &url);
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-nostdin".into()];
    args.extend(super::worker::ffmpeg_input_args_over(&url, udp));
    args.extend(["-map", "0:v:0", "-frames:v", PROBE_FRAMES, "-f", "null", "-"].map(String::from));
    let mut child = crate::child::ffmpeg()
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg did not start: {e}"))?;
    crate::child::adopt(&child);
    let tail = child.stderr.take().map(super::worker::drain_stderr);
    let until = std::time::Instant::now() + PROBE_TIME;
    let status = loop {
        if let Ok(Some(s)) = child.try_wait() {
            break Some(s);
        }
        if std::time::Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    let said = tail.map(|t| t.lock().unwrap().clone()).unwrap_or_default();
    let last = crate::engine::redact_text(said.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or(""));
    match status {
        Some(s) if s.success() => Ok(()),
        Some(_) if !last.is_empty() => Err(last),
        Some(_) => Err("it did not play".into()),
        None => Err(format!("no pictures within {} s", PROBE_TIME.as_secs())),
    }
}

/// A stream refused (401/403) or missing (404), as retina or ffmpeg say it.
pub fn is_refusal(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    let status = err.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric())).any(|t| matches!(t, "401" | "403" | "404"));
    status || low.contains("unauthorized") || low.contains("forbidden") || low.contains("not found")
}

/// A decoding note with its part about the sub stream replaced by `sub`.
pub fn replace_sub_note(note: Option<String>, sub: Option<&String>) -> Option<String> {
    let kept = note.map(|n| n.split("; ").filter(|p| !p.contains("the sub stream")).collect::<Vec<_>>().join("; ")).filter(|n| !n.is_empty());
    join_notes(kept, sub)
}

/// Two notes in one ("a; b"), either may be missing.
pub fn join_notes(a: Option<String>, b: Option<&String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or_else(|| b.cloned()),
    }
}

// ---------------------------------------------------------------------------
// Recording bitrate
// ---------------------------------------------------------------------------

/// How much a camera's continuous recording grew: ffmpeg's segment muxer
/// writes `<prefix>000000.mp4`, `<prefix>000001.mp4`, ... one after the
/// other, so only the current file and the next are looked at.
#[derive(Default)]
pub struct SegMeter {
    idx: u32,
    size: u64,
}

impl SegMeter {
    /// Bytes written since the last call.
    pub fn poll(&mut self, dir: &std::path::Path, prefix: &str) -> u64 {
        let path = |i: u32| dir.join(format!("{prefix}{i:06}.mp4"));
        let mut added = 0;
        for _ in 0..1000 {
            let cur = std::fs::metadata(path(self.idx)).map(|m| m.len()).unwrap_or(0);
            if cur > self.size {
                added += cur - self.size;
                self.size = cur;
            }
            if path(self.idx + 1).exists() {
                self.idx += 1;
                self.size = 0;
            } else {
                break;
            }
        }
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(extra: &str) -> CameraConfig {
        toml::from_str(&format!("name = \"Porch\"\nurl = \"rtsp://h/x\"\n{extra}")).unwrap()
    }

    #[test]
    fn says_why_a_camera_is_on_ffmpeg() {
        assert_eq!(why_ffmpeg(&cam(""), "native", None), None);
        let w = why_ffmpeg(&cam("overlay = true"), "ffmpeg", None).unwrap();
        assert!(w.contains("Also burn it into recordings") && w.contains("Turn it off"), "{w}");
        let w = why_ffmpeg(&cam("privacy_masks = [{ x = 0.0, y = 0.0, w = 0.1, h = 0.1 }]"), "ffmpeg", None).unwrap();
        assert!(w.contains("privacy masks"), "{w}");
        let w = why_ffmpeg(&cam("ingest = \"ffmpeg\""), "ffmpeg", None).unwrap();
        assert!(w.contains("ingest = \"ffmpeg\""), "{w}");
        let w = why_ffmpeg(&cam(""), "ffmpeg (fallback)", Some("SETUP failed: 461")).unwrap();
        assert!(w.contains("could not play") && w.contains("461"), "{w}");
        let mut c = cam("");
        c.url = "lavfi://testsrc".into();
        assert!(why_ffmpeg(&c, "ffmpeg", None).unwrap().contains("lavfi://"));
    }

    #[test]
    fn re_encoding_is_named_with_its_cause() {
        assert_eq!(reencode_cause(&cam("record = true\ndirect = true")), None);
        assert_eq!(reencode_cause(&cam("record = true\ndirect = true\noverlay = true")), Some(Reencode::Overlay));
        assert_eq!(reencode_cause(&cam("record = false\noverlay = true")), None);
        assert_eq!(reencode_cause(&cam("record = true")), Some(Reencode::NoDirect));
        let t = reencode_text("Porch", Reencode::Overlay, None);
        assert!(t.starts_with("Porch: recordings are re-encoded to burn in the text (about one processor core per 4K camera)"), "{t}");
        assert!(t.contains("Turn off \"Also burn it into recordings\""));
        let t = reencode_text("Porch", Reencode::Masks, Some("NVENC"));
        assert!(t.contains("graphics card (NVENC)") && t.contains("little processor time"), "{t}");
    }

    #[test]
    fn hardware_encoder_arguments() {
        let nv = Encoder { hw: Some(Hw::Nvenc), device: None };
        let va = Encoder { hw: Some(Hw::Vaapi), device: Some("/dev/dri/renderD129".into()) };
        assert_eq!(rec_input_args(Some(&nv), true), vec!["-hwaccel", "cuda"]);
        assert!(rec_input_args(Some(&nv), false).is_empty());
        let a = rec_input_args(Some(&va), true).join(" ");
        assert!(a.contains("vaapi=rec:/dev/dri/renderD129") && a.contains("-filter_hw_device rec"), "{a}");
        assert_eq!(rec_input_args(Some(&va), false), vec!["-vaapi_device", "/dev/dri/renderD129"]);
        assert_eq!(rec_upload(Some(&va)), Some("format=nv12,hwupload"));
        assert_eq!(rec_upload(None), None);
        let e = rec_encode_args(Some(&va), 10).join(" ");
        assert!(e.starts_with("-c:v h264_vaapi") && e.ends_with("-force_key_frames expr:gte(t,n_forced*10)"), "{e}");
        let e = rec_encode_args(None, 4).join(" ");
        assert_eq!(e, "-c:v libx264 -preset veryfast -pix_fmt yuv420p -force_key_frames expr:gte(t,n_forced*4)");
        assert!(rec_encode_args(Some(&nv), 4).join(" ").contains("h264_nvenc"));
        assert_eq!(encoder_label(&va), "VAAPI on /dev/dri/renderD129");
        assert!(hw_encode_blamed("[h264_nvenc @ 0x1] OpenEncodeSessionEx failed: out of memory (10)\nError while opening encoder"));
        assert!(!hw_encode_blamed("rtsp://h/x: Connection refused"));
    }

    #[test]
    fn a_broken_sub_stream_is_given_up_and_tried_again() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let short = Duration::from_secs(20);
        // Refused or missing (the main stream working): at once.
        let mut w = SubWatch::default();
        assert!(w.use_sub(t0));
        let said = w.ended(t0, short, Some(("DESCRIBE failed: 404 response to DESCRIBE CSeq=2", "the camera has no stream at that address (404)")));
        assert!(said.unwrap().contains("tried again in 15 min"));
        assert!(w.off.as_deref().unwrap().contains("motion uses the main stream"));
        assert!(!w.use_sub(t0 + Duration::from_secs(60)), "main stream until the retry");
        // The retry after 15 min works and lasts: the sub stream is back.
        let t1 = t0 + Duration::from_secs(15 * 60);
        assert!(w.use_sub(t1) && w.trying());
        assert!(w.ended(t1, short, None).is_none(), "not clear yet: tried again next session");
        assert!(w.use_sub(t1) && w.trying());
        assert!(w.ended(t1, SUB_HEALTHY, None).unwrap().contains("works again"));
        assert!(w.off.is_none() && w.use_sub(t1) && !w.trying());

        // Anything else: three failures of the sub stream alone.
        let mut w = SubWatch::default();
        for _ in 0..2 {
            assert!(w.ended(t0, short, Some(("camera closed the stream", "closed"))).is_none());
        }
        // An outage that took both down is not counted (the caller passes None)...
        assert!(w.ended(t0, short, None).is_none());
        // ...and a healthy session forgets the earlier ones.
        assert!(w.ended(t0, SUB_HEALTHY, None).is_none());
        for _ in 0..2 {
            assert!(w.ended(t0, short, Some(("camera closed the stream", "closed"))).is_none());
        }
        assert!(w.ended(t0, short, Some(("camera closed the stream", "closed"))).is_some(), "the third in a row");
        // A failed retry waits twice as long, up to 2 h.
        let mut t = t0;
        for want in ["30 min", "1 h", "2 h", "2 h"] {
            t += Duration::from_secs(3 * 3600);
            assert!(w.use_sub(t));
            assert!(w.ended(t, short, Some(("closed", "closed"))).unwrap().contains(&format!("tried again in {want}")), "{want}");
        }
        // (A port or a frame count is not a status.)
        let mut w = SubWatch::default();
        assert!(w.ended(t0, short, Some(("no frames from 192.0.2.4:1404 after 4040 ms", "x"))).is_none());

        // On a camera that never reconnects: the try happens on the side.
        let mut w = SubWatch::default();
        w.ended(t0, short, Some(("401 Unauthorized", "refused")));
        assert!(!w.probe_due(t0 + Duration::from_secs(60)));
        let due = t0 + Duration::from_secs(15 * 60);
        assert!(w.probe_due(due));
        // It fails: the next one in 30 min, and the note says so.
        let (back, line) = w.probe_done(due, Err("no pictures within 20 s".into()));
        assert!(!back && line.contains("tried again in 30 min"), "{line}");
        assert!(!w.probe_due(due + Duration::from_secs(60)));
        let due = due + Duration::from_secs(30 * 60);
        assert!(w.probe_due(due));
        // It works: the next session uses the sub stream, as a try.
        let (back, _) = w.probe_done(due, Ok(()));
        assert!(back && !w.probe_due(due));
        assert!(w.use_sub(due) && w.trying());
        assert!(w.ended(due, SUB_HEALTHY, None).unwrap().contains("works again"));
        assert!(w.off.is_none());
        assert_eq!(join_notes(Some("a".into()), Some(&"b".to_string())).as_deref(), Some("a; b"));
        assert_eq!(join_notes(None, Some(&"b".to_string())).as_deref(), Some("b"));
        let old = "VAAPI failed; the sub stream did not work (x); motion uses the main stream, and the sub stream is tried again in 15 min".to_string();
        let new = "the sub stream did not work (y); motion uses the main stream, and the sub stream is tried again in 30 min".to_string();
        assert_eq!(replace_sub_note(Some(old), Some(&new)).unwrap(), format!("VAAPI failed; {new}"));
    }

    #[test]
    fn a_side_try_of_a_dead_sub_stream_fails_quickly() {
        if std::process::Command::new(crate::ffmpeg_bin::program()).arg("-version").output().is_err() {
            return;
        }
        let t0 = std::time::Instant::now();
        let r = probe_sub(&cam(""), "rtsp://u:secret77@127.0.0.1:9/sub");
        let why = r.unwrap_err();
        assert!(!why.contains("secret77"), "{why}");
        assert!(t0.elapsed() < PROBE_TIME, "{why}");
    }

    #[test]
    fn segment_meter_follows_the_files() {
        let dir = std::env::temp_dir().join(format!("amber-segmeter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = "cam-1-seg";
        let mut m = SegMeter::default();
        assert_eq!(m.poll(&dir, p), 0);
        std::fs::write(dir.join("cam-1-seg000000.mp4"), vec![0u8; 1000]).unwrap();
        assert_eq!(m.poll(&dir, p), 1000);
        std::fs::write(dir.join("cam-1-seg000000.mp4"), vec![0u8; 1500]).unwrap();
        std::fs::write(dir.join("cam-1-seg000001.mp4"), vec![0u8; 200]).unwrap();
        assert_eq!(m.poll(&dir, p), 700);
        assert_eq!(m.poll(&dir, p), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
