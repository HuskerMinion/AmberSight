//! Conversions that burn something into a recording that leaves the server:
//! the camera's text (`burn_text=1` on a download), and the time subtitle
//! track (mov_text / 3GPP timed text) every converted download carries.
//! (Until 0.11.2 this also burned AmberSight's own privacy masks in; those
//! are gone: cameras have their own.)

use super::file::{Input, Job};
use super::{mp4, OutCodec, Target, Why};
use crate::config::CameraConfig;
use crate::engine::Engine;
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// The largest picture a conversion that only burns something in makes
/// (long side, short side): 1080p.
const BURN_MAX: (u32, u32) = (1920, 1080);

/// What a conversion that only burns something in makes (no `caps` from the
/// player): H.264 (every player plays it, and every encoder makes it fast)
/// at the source's size, made smaller to fit 1080p when it is larger (a 4K
/// picture in software would not keep up on a small server).
pub fn forced_target(desc: &mp4::VideoDesc) -> Target {
    let codec = OutCodec::H264;
    let (w, h) = (desc.width.max(2), desc.height.max(2));
    let (long, short) = (w.max(h) as f64, w.min(h) as f64);
    let scale = (BURN_MAX.0 as f64 / long).min(BURN_MAX.1 as f64 / short).min(1.0);
    let even = |v: f64| ((v.round() as u32) / 2 * 2).max(2);
    let (width, height) = if scale < 1.0 { (even(w as f64 * scale), even(h as f64 * scale)) } else { (w / 2 * 2, h / 2 * 2) };
    let (width, height) = (width.max(2), height.max(2));
    let rate = desc.fps.map(f64::round).unwrap_or(25.0);
    Target { codec, width, height, fps: None, bitrate_kbps: super::bitrate_kbps(codec, width, height, rate) }
}

/// The wall-clock time of `ms` (epoch) as local date and time.
pub fn clock(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default()
}

/// The local time zone's offset from UTC (seconds) at `ms`.
pub fn offset_s(ms: i64) -> i64 {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).offset().local_minus_utc() as i64).unwrap_or(0)
}

/// SRT subtitles for a video whose first frame is at wall time `start_ms`
/// and that runs `dur_ms`: one cue per second of the clock (`label` and the
/// time), times from the video's start.
pub fn srt(start_ms: i64, dur_ms: i64, label: &str, clock: &dyn Fn(i64) -> String) -> String {
    let label: String = label.chars().filter(|c| !c.is_control()).collect();
    let t = |ms: i64| format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let mut out = String::new();
    let end = start_ms + dur_ms.max(0);
    let mut at = start_ms;
    let mut n = 0;
    while at < end {
        let next = ((at.div_euclid(1000) + 1) * 1000).min(end);
        n += 1;
        out.push_str(&format!("{n}\n{} --> {}\n{}  {}\n\n", t(at - start_ms), t(next - start_ms), label.trim(), clock(at)));
        at = next;
    }
    out
}

/// A subtitle file for one conversion, removed when dropped.
pub struct SubsFile(std::path::PathBuf);

impl SubsFile {
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for SubsFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write the time subtitles of a download (see `srt`) to the data folder's
/// tmp folder. None: they could not be written (the download goes without).
pub fn subs_file(engine: &Engine, start_ms: i64, dur_ms: i64, label: &str) -> Option<SubsFile> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = engine.data_dir.join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(format!("subs-{}-{}.srt", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    match std::fs::write(&p, srt(start_ms, dur_ms, label, &clock)) {
        Ok(()) => Some(SubsFile(p)),
        Err(e) => {
            tracing::warn!("download subtitles: {e}");
            None
        }
    }
}

/// 503 + Retry-After: the text cannot be burned in right now, but can soon.
fn busy(why: &str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "10".to_string()), (header::CACHE_CONTROL, "no-store".to_string())], format!("{NEEDS_BURN}, and {why}. Try again shortly, or download it without the text.")).into_response()
}

const NEEDS_BURN: &str = "The camera's text was asked for in this video, so the server has to burn it in";

/// 409: the text can never be burned in on this server as it is set up
/// (video conversion turned off). Not "try again": asking again changes nothing.
fn conversion_off() -> Response {
    (StatusCode::CONFLICT, [(header::CACHE_CONTROL, "no-store")], format!("{NEEDS_BURN}, and video conversion is turned off on this server (Settings > Video conversion). Download it without the text, or ask the admin to turn conversion on.")).into_response()
}

/// 403: this account's videos are never converted (the demo account), so a
/// video with the text burned in is not made for it.
fn never_converted() -> Response {
    (StatusCode::FORBIDDEN, [(header::CACHE_CONTROL, "no-store")], format!("{NEEDS_BURN}, and this account's videos are never converted. Download it without the text.")).into_response()
}

/// The answer when the text cannot be burned in for `user` (None: no
/// account limit) because of `w`: 403 or 409 when that will not change by
/// itself, else 503 (busy, try again).
pub fn refusal(w: Why, user: Option<&str>) -> Response {
    if user.is_some_and(super::is_unconverted) {
        return never_converted();
    }
    match w {
        Why::Off => conversion_off(),
        _ => busy("its video encoders are busy"),
    }
}

/// One conversion that burns a caption (the camera's text, the time) and
/// subtitles into a recording, streamed as fragmented MP4.
pub struct Burn {
    pub input: Input,
    pub desc: mp4::VideoDesc,
    pub camera: String,
    pub caption: Option<String>,
    pub subs: Option<SubsFile>,
    pub seek_ms: i64,
    pub limit_ms: Option<i64>,
    /// How long the output lasts (ms).
    pub media_ms: Option<i64>,
    pub keep_times: bool,
    pub what: &'static str,
    /// A download: holds one of the server's download places (and its time limit).
    pub download: bool,
}

/// The job for `b` on one of this server's encoders, with the account's
/// place (`user`; None: no account limit, an export). Err: why not now.
pub async fn job(b: Burn, user: Option<&str>) -> Result<Job, Response> {
    let target = forced_target(&b.desc);
    let codec = target.codec;
    let encoders = match tokio::task::spawn_blocking(move || super::encoders_for(codec)).await.ok().flatten() {
        None => return Err(busy("the video encoders are still being checked")),
        Some(v) if v.is_empty() => return Err(refusal(Why::Off, user)),
        Some(v) => v,
    };
    job_with(b, user, target, encoders)
}

fn job_with(b: Burn, user: Option<&str>, target: Target, encoders: Vec<super::Encoder>) -> Result<Job, Response> {
    if user.is_some_and(super::is_unconverted) {
        return Err(never_converted());
    }
    let download = match b.download {
        true => Some(super::reserve_download().ok_or_else(|| busy("two downloads are being converted already"))?),
        false => None,
    };
    let lease = match user {
        Some(u) => Some(super::user_lease(u).ok_or_else(|| busy("this account is converting as many videos as it may"))?),
        None => None,
    };
    let all = encoders.clone();
    let (encoders, slot) = super::reserve(&encoders).ok_or_else(|| busy("its video encoders are busy"))?;
    if user.is_some_and(|u| !super::may_start(u)) {
        return Err(busy("too many conversions were started in the last minute"));
    }
    Ok(make_job(b, target, &all, encoders, slot, lease, download))
}

fn make_job(b: Burn, target: Target, all: &[super::Encoder], encoders: Vec<super::Encoder>, slot: super::Slot, lease: Option<super::UserLease>, download: Option<super::DownloadSlot>) -> Job {
    Job {
        input: b.input,
        target,
        software_after: slot.hardware && all.iter().any(|e| e.hw.is_none()),
        encoders,
        slot,
        lease,
        fps: b.desc.fps.unwrap_or(25.0),
        depth: b.desc.bit_depth,
        src_codec: format!("{:?}", b.desc.codec),
        seek_ms: b.seek_ms,
        media_ms: b.media_ms,
        limit_ms: b.limit_ms,
        what: b.what,
        camera: b.camera,
        caption: b.caption,
        download,
        subs: b.subs,
        keep_times: b.keep_times,
    }
}

/// Stream `b` (converted as it is sent: no byte ranges) with these headers
/// besides the usual ones.
pub async fn stream(b: Burn, user: &str, headers: Vec<(String, String)>) -> Response {
    let target = forced_target(&b.desc);
    let job = match job(b, Some(user)).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let mut r = Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCEPT_RANGES, "none")
        .header("x-as-transcoded", "1")
        .header("x-as-codec", target.codec.name())
        .header("x-as-width", target.width.to_string())
        .header("x-as-height", target.height.to_string());
    for (k, v) in headers {
        r = r.header(k, v);
    }
    r.body(super::file::stream(job)).unwrap()
}

/// An event clip for a download with the camera's text (`text`) burned in
/// and the time subtitles. None: no recording. Blocking.
pub fn event_burn(engine: &Arc<Engine>, ev: &crate::engine::Event, text: Option<&CameraConfig>) -> anyhow::Result<Option<Burn>> {
    let path = engine.clip_file_path(&ev.id);
    let (input, desc, start_ms) = if path.is_file() {
        let Some(desc) = mp4::probe_file(&path)? else { return Ok(None) };
        // Where the file really starts: the clock burned into it matches.
        (Input::File(path), desc, engine.clip_span_ms(ev).0)
    } else {
        let Some(clip) = engine.virtual_clip(ev)? else { return Ok(None) };
        let Some(desc) = super::http::probe_clip(&clip) else { return Ok(None) };
        let s = clip.start_ms;
        (Input::Clip(clip), desc, s)
    };
    let media_ms = match &input {
        Input::Clip(c) => Some(c.end_ms - c.start_ms),
        Input::File(_) => desc.duration_ms,
    };
    let dur_ms = media_ms.unwrap_or((ev.end - ev.start + 60) * 1000);
    let caption = text.map(|c| crate::engine::clock_text(start_ms, dur_ms, &offset_s, &|b| crate::engine::overlay_download_drawtext(c, b)));
    let subs = subs_file(engine, start_ms, dur_ms, &ev.camera);
    Ok(Some(Burn { input, desc, camera: ev.camera.clone(), caption, subs, seek_ms: 0, limit_ms: None, media_ms, keep_times: false, what: "download", download: false }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_that_never_clear_are_not_try_again() {
        assert_eq!(refusal(Why::Off, None).status(), StatusCode::CONFLICT);
        assert_eq!(refusal(Why::BusyFallback, None).status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(refusal(Why::BusyFallback, None).headers().contains_key(header::RETRY_AFTER));
        assert!(!refusal(Why::Off, None).headers().contains_key(header::RETRY_AFTER));
        assert_eq!(never_converted().status(), StatusCode::FORBIDDEN);
        assert!(!never_converted().headers().contains_key(header::RETRY_AFTER));
    }

    #[test]
    fn subtitles_say_each_second_of_the_clock() {
        let clock = |ms: i64| format!("t{}", ms / 1000);
        let s = srt(10_500, 2_000, "Porch\n", &clock);
        assert_eq!(s, "1\n00:00:00,000 --> 00:00:00,500\nPorch  t10\n\n2\n00:00:00,500 --> 00:00:01,500\nPorch  t11\n\n3\n00:00:01,500 --> 00:00:02,000\nPorch  t12\n\n");
        assert_eq!(srt(0, 0, "x", &clock), "");
        let long = srt(0, 3_725_000, "x", &clock);
        assert!(long.contains("01:02:04,000 --> 01:02:05,000"), "hours, minutes, seconds");
    }

    #[test]
    fn a_burn_makes_h264_no_larger_than_1080p() {
        use mp4::{VideoCodec, VideoDesc};
        let d = |codec, w, h| VideoDesc { codec, bit_depth: 8, width: w, height: h, fps: Some(20.0), codec_string: None, track_id: 1, timescale: 90000, trex_flags: 0, trex_duration: 0, duration_ms: None };
        let t = forced_target(&d(VideoCodec::Hevc, 4608, 1728));
        assert_eq!((t.codec, t.width, t.height, t.fps), (OutCodec::H264, 1920, 720, None), "a wide 4K picture fits 1920 across");
        let t = forced_target(&d(VideoCodec::Hevc, 3840, 2160));
        assert_eq!((t.codec, t.width, t.height), (OutCodec::H264, 1920, 1080));
        let t = forced_target(&d(VideoCodec::H264, 2560, 1920));
        assert_eq!((t.width, t.height), (1440, 1080), "4:3: the height is the limit");
        let t = forced_target(&d(VideoCodec::H264, 1080, 1920));
        assert_eq!((t.width, t.height), (1080, 1920), "upright 1080p stays");
        let t = forced_target(&d(VideoCodec::H264, 641, 361));
        assert_eq!((t.codec, t.width, t.height), (OutCodec::H264, 640, 360), "smaller: its own size");
        assert_eq!(forced_target(&d(VideoCodec::Other, 320, 240)).codec, OutCodec::H264);
    }

    #[test]
    fn a_caption_is_drawn_in_software_after_the_scale_with_the_subtitles() {
        use crate::transcode::{Encoder, Hw};
        let t = Target { codec: OutCodec::H264, width: 640, height: 360, fps: None, bitrate_kbps: 1000 };
        let subs = std::path::Path::new("/tmp/x.srt");
        let qsv = Encoder { hw: Some(Hw::Qsv), device: None };
        let x = crate::transcode::file::Extra { subs: Some(subs), keep_times: true };
        let a = crate::transcode::file::args_with(&qsv, true, &t, 25.0, 8, None, 1500, Some(10_000), Some("drawtext=text='a'"), x);
        let at = |k: &str| a.iter().position(|v| v == k).map(|i| a[i + 1].clone());
        assert!(!a.iter().any(|v| v == "-hwaccel"), "decoded in software: {a:?}");
        let vf = at("-vf").unwrap();
        assert_eq!(vf, "trim=start=1.500,setpts=PTS-STARTPTS,scale=640:360:flags=bilinear,drawtext=text='a',format=nv12,hwupload=extra_hw_frames=16");
        // From a pipe with subtitles: no -ss (it would drop the first ones), the sound trimmed too.
        assert_eq!(at("-ss"), None);
        assert_eq!(at("-af").as_deref(), Some("atrim=start=1.500,asetpts=PTS-STARTPTS"));
        assert_eq!(at("-c:s").as_deref(), Some("mov_text"));
        let pass = crate::ffmpeg_bin::passthrough_args();
        assert_eq!(at(pass[0]).as_deref(), Some(pass[1]));
        assert!(a.windows(2).any(|w| w[0] == "-i" && w[1] == "/tmp/x.srt"));
        // A file seeks before its input; the subtitles then start at the moment asked for.
        let a = crate::transcode::file::args_with(&Encoder::software(), false, &t, 25.0, 8, Some(std::path::Path::new("/c.mp4")), 1500, None, Some("drawtext=text='a'"), crate::transcode::file::Extra { subs: Some(subs), keep_times: false });
        let at = |k: &str| a.iter().position(|v| v == k).map(|i| a[i + 1].clone());
        assert_eq!(at("-ss").as_deref(), Some("1.500"));
        assert!(a.iter().position(|v| v == "-ss") < a.iter().position(|v| v == "/c.mp4"));
        assert_eq!(at("-vf").unwrap(), "scale=640:360:flags=bilinear,drawtext=text='a',format=yuv420p");
        assert_eq!(at("-c:a").as_deref(), Some("copy"));
        assert!(a.iter().any(|v| v == "-threads"), "threads capped: {a:?}");
    }

    fn have_ffmpeg() -> bool {
        crate::child::ffmpeg().arg("-version").output().is_ok_and(|o| o.status.success())
    }

    /// The camera's text burned into a download across a change of the
    /// time zone's offset: each stretch has its own clock, and ffmpeg
    /// takes the filters (the text is drawn before and after the change).
    #[test]
    fn burned_in_clock_text_follows_a_daylight_saving_change() {
        let off = |ms: i64| if ms < 2_000 { 3600 } else { 7200 };
        let make = |b: f64| format!("drawtext=text='x{b:.0}'");
        let one = crate::engine::clock_text(0, 10_000, &|_| 3600, &make);
        assert_eq!(one, "drawtext=text='x3600'", "no change: one filter");
        let two = crate::engine::clock_text(0, 4_000, &off, &make);
        assert_eq!(two, "drawtext=text='x3600':enable='gte(t,0.000)*lt(t,2.000)',drawtext=text='x7200':enable='gte(t,2.000)*lt(t,1e9)'");
        // To the second inside a long clip.
        let long = crate::engine::clock_text(1_000_000, 3_600_000, &|ms| if ms < 2_234_567 { 0 } else { 3600 }, &make);
        let cut: f64 = long.split("lt(t,").nth(1).unwrap().split(')').next().unwrap().parse().unwrap();
        assert!((1234.5..=1235.6).contains(&cut), "{long}");
        if !have_ffmpeg() || !crate::picture_text::drawtext_available() {
            return;
        }
        let vf = crate::engine::clock_text(0, 4_000, &off, &|b| crate::engine::caption_drawtext("Cam", b));
        let o = crate::child::ffmpeg().args(["-v", "error", "-f", "lavfi", "-i", "color=c=white:s=320x240:r=10", "-t", "4", "-vf", &format!("{vf},scale=320:240"), "-pix_fmt", "gray", "-f", "rawvideo", "-"]).output().unwrap();
        assert!(o.status.success(), "{vf}: {}", String::from_utf8_lossy(&o.stderr));
        let frame = 320 * 240;
        assert_eq!(o.stdout.len(), frame * 40);
        // The text's box (black, half see-through) is in every frame, before and after the change.
        for n in [5, 15, 25, 35] {
            let f = &o.stdout[n * frame..(n + 1) * frame];
            assert!(f[frame - 320 * 30..].iter().any(|&p| p < 200), "frame {n} has the text");
        }
    }
}

/// Recordings leaving the server through the web API, against real files
/// (made with ffmpeg's lavfi and libx264; skipped without ffmpeg).
#[cfg(test)]
mod http_tests {
    use crate::engine::Engine;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use base64::Engine as _;
    use std::sync::Arc;
    use tower::ServiceExt;

    const USERS: [&str; 6] = ["bt-a", "bt-b", "bt-c", "bt-d", "bt-e", "bt-f"];

    /// One conversion test at a time: the server converts two downloads at
    /// most, and these tests download.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Camera "Open" (nothing burned in); admin accounts.
    fn engine(dir: &std::path::Path) -> Arc<Engine> {
        let mut cfg: crate::config::Config = toml::from_str("").unwrap();
        for u in USERS {
            cfg.server.users.push(crate::config::UserConfig { username: u.into(), password: crate::auth::hash_password(&format!("pw-{u}-1")), admin: true, cameras: vec![], disabled: false, role: String::new(), perms: Default::default() });
        }
        let mut e = Engine::for_test(cfg);
        Arc::get_mut(&mut e).unwrap().data_dir = dir.to_path_buf();
        e.add_test_camera("Open");
        e
    }

    fn auth(u: &str) -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:pw-{u}-1")))
    }

    async fn get(app: &axum::Router, path: &str, who: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let req = Request::builder().uri(path).header(header::HOST, "localhost:8090").header(header::AUTHORIZATION, auth(who)).body(Body::empty()).unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let (s, h) = (r.status(), r.headers().clone());
        let b = axum::body::to_bytes(r.into_body(), 1 << 30).await.map(|b| b.to_vec()).unwrap_or_default();
        (s, h, b)
    }

    fn have_ffmpeg() -> bool {
        crate::child::ffmpeg().arg("-version").output().is_ok_and(|o| o.status.success())
    }

    fn ffmpeg(args: &[&str]) -> std::process::Output {
        crate::child::ffmpeg().args(args).output().unwrap()
    }

    /// Whether an MP4 plays (its first picture decodes) and has a subtitle track.
    fn subtitled(dir: &std::path::Path, name: &str, mp4: &[u8]) -> bool {
        let p = dir.join(name);
        std::fs::write(&p, mp4).unwrap();
        let o = ffmpeg(&["-v", "error", "-i", p.to_str().unwrap(), "-frames:v", "1", "-vf", "scale=320:240", "-pix_fmt", "gray", "-f", "rawvideo", "-"]);
        assert_eq!(o.stdout.len(), 320 * 240, "{name}: {}", String::from_utf8_lossy(&o.stderr));
        let info = ffmpeg(&["-hide_banner", "-i", p.to_str().unwrap()]);
        String::from_utf8_lossy(&info.stderr).contains("Subtitle:")
    }

    async fn encoders_ready() {
        let ready = tokio::task::spawn_blocking(|| (0..40).find_map(|_| crate::transcode::encoders_for(crate::transcode::OutCodec::H264))).await.unwrap();
        assert!(ready.is_some_and(|v| !v.is_empty()), "no H.264 encoder");
    }

    /// Downloads of a camera with nothing burned in are not converted and
    /// carry the time subtitles; `burn_text=1` converts for the text; an
    /// event clip says where it starts (a one-byte read too); the export's
    /// clips have the subtitles; fast playback gets the stored keyframe.
    #[tokio::test(flavor = "multi_thread")]
    async fn downloads_carry_subtitles_and_are_converted_only_to_burn_the_text() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-burn-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        let e = engine(&dir);
        // Six seconds of a white picture, as the ffmpeg recorder writes it (fragmented MP4).
        let seg = dir.join("rec").join("open.mp4");
        let o = ffmpeg(&["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=white:s=320x240:r=10", "-t", "6", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "10", "-bf", "0", "-movflags", "frag_keyframe+empty_moov+default_base_moof", seg.to_str().unwrap()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let t0 = (chrono::Utc::now().timestamp() - 3600) * 1000;
        let bytes = std::fs::metadata(&seg).unwrap().len() as i64;
        let row = crate::db::SegmentRow { path: "rec/open.mp4".into(), start_ms: t0, end_ms: t0 + 6000, bytes, video_frames: 60, kind: "continuous".into(), folder: String::new() };
        e.db.insert_segment("open", &row).unwrap();
        let ev = crate::engine::Event { id: "bt-ev-open".into(), camera_id: 0, camera: "Open".into(), start: t0 / 1000 + 2, end: t0 / 1000 + 3, score: 1.0, clip: None, thumb: None, objects: vec![], protect: false };
        e.db.insert_event(&ev).unwrap();
        encoders_ready().await;
        let app = crate::web::build_router(e.clone());
        let win = format!("from={t0}&to={}", t0 + 6000);

        // A plain download: not converted, the subtitles added without it.
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Open&{win}&download=1"), "bt-a").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(!h.contains_key("x-as-transcoded") && h.get(header::CONTENT_DISPOSITION).is_some(), "{h:?}");
        assert!(subtitled(&dir, "d1.mp4", &b));
        // burn_text=1: converted for the text alone, the subtitles kept.
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Open&{win}&download=1&burn_text=1"), "bt-b").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(h.get("x-as-why").is_some_and(|v| v == "text"), "{h:?}");
        assert!(subtitled(&dir, "d2.mp4", &b));

        // The event clip: its first picture's time in the headers, the subtitles without converting.
        let (s, h, b) = get(&app, "/api/clips/bt-ev-open?download=1", "bt-c").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(h.get("x-clip-start-ms").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok()).is_some_and(|ms| (t0..t0 + 6000).contains(&ms)), "{h:?}");
        assert!(subtitled(&dir, "c1.mp4", &b), "subtitles without converting");
        // A one-byte read still says where the clip starts.
        let req = Request::builder().uri("/api/clips/bt-ev-open").header(header::HOST, "localhost:8090").header(header::AUTHORIZATION, auth("bt-c")).header(header::RANGE, "bytes=0-0").body(Body::empty()).unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert!(r.headers().contains_key("x-clip-start-ms"), "{:?}", r.headers());

        // The export: the clip with its subtitles.
        let (s, _, zip) = get(&app, "/api/export?ids=bt-ev-open", "bt-d").await;
        assert_eq!(s, StatusCode::OK);
        let entry = |name: &str| -> Vec<u8> {
            let mut p = 0;
            while p + 30 <= zip.len() && zip[p..p + 4] == [0x50, 0x4B, 0x03, 0x04] {
                let u = |o: usize| u32::from_le_bytes(zip[p + o..p + o + 4].try_into().unwrap()) as usize;
                let n = u16::from_le_bytes(zip[p + 26..p + 28].try_into().unwrap()) as usize;
                let (size, start) = (u(18), p + 30 + n);
                if &zip[p + 30..start] == name.as_bytes() {
                    return zip[start..start + size].to_vec();
                }
                p = start + size;
            }
            panic!("{name} not in the export");
        };
        assert!(subtitled(&dir, "x1.mp4", &entry("bt-ev-open.mp4")));

        // Fast playback pictures: the stored keyframe; the same one again is 204.
        let at = t0 + 2500;
        let (s, h, b) = get(&app, &format!("/api/playback/picture?camera=Open&at={at}"), "bt-e").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(h.contains_key("x-codec") && !b.is_empty(), "{h:?}");
        let have = h.get("x-pic-ms").and_then(|v| v.to_str().ok()).expect("x-pic-ms").to_string();
        let (s, _, _) = get(&app, &format!("/api/playback/picture?camera=Open&at={at}&have={have}"), "bt-e").await;
        assert_eq!(s, StatusCode::NO_CONTENT, "the same picture again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn seg_files(dir: &std::path::Path, slug: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir.join("cameras").join(slug).join("segments"))
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".mp4")).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// A camera renamed while it records on the ffmpeg pipeline (its text
    /// burned in): its recorder stops first, the files it had not finished
    /// (or the 15 s scan had not indexed yet) are indexed and go with it to
    /// the new name, nothing is left under the old one, and the old
    /// recorder writes nothing after.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_renamed_cameras_last_recordings_go_with_it() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() || !crate::picture_text::drawtext_available() {
            eprintln!("no ffmpeg that draws text here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-burn-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let e = engine(&dir);
        e.config.lock().unwrap().storage.segment_seconds = 2;
        let src = "url = \"lavfi://color=c=white:s=320x240:r=10\"\noverlay = true\n";
        let cam: crate::config::CameraConfig = toml::from_str(&format!("name = \"Shed\"\n{src}")).unwrap();
        let id = e.add_camera_arc(cam.clone());
        // Recording (on the ffmpeg pipeline), a few files written.
        let t0 = std::time::Instant::now();
        while seg_files(&dir, "shed").len() < 6 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(40), "no recordings: {:?}", seg_files(&dir, "shed"));
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        assert!(e.camera(id).unwrap().state.read().unwrap().ingest.starts_with("ffmpeg"));
        // The 15 s scan ran just now: the newest files are not indexed yet.
        e.db.import_recent_camera_dirs(&[("shed".into(), String::new(), dir.clone())], 2, 0).unwrap();
        let indexed = e.db.segments_overlapping("shed", 0, i64::MAX / 2).unwrap().len();
        assert!(indexed > 0 && indexed < seg_files(&dir, "shed").len(), "a file still being written is left out ({indexed})");
        // Renamed while it records (and switched off, so nothing new records).
        let mut renamed = cam.clone();
        renamed.name = "Side Yard".into();
        renamed.enabled = false;
        let e2 = e.clone();
        let t = std::time::Instant::now();
        assert!(tokio::task::spawn_blocking(move || e2.rename_and_update_camera(id, "Shed", renamed)).await.unwrap().unwrap());
        assert!(t.elapsed() < crate::engine::RENAME_STOP_WAIT, "its recorder stopped in time: {:?}", t.elapsed());
        // Everything it left in the old folder is the renamed camera's.
        let files = seg_files(&dir, "shed");
        let moved = e.db.segments_overlapping("side_yard", 0, i64::MAX / 2).unwrap();
        let names: std::collections::BTreeSet<String> = moved.iter().map(|r| r.path.rsplit('/').next().unwrap().to_string()).collect();
        assert!(!files.is_empty());
        for f in &files {
            assert!(names.contains(f), "{f} indexed and moved with the camera: {names:?}");
        }
        assert!(e.db.segments_overlapping("shed", 0, i64::MAX / 2).unwrap().is_empty());
        // A camera added under the old name; its folder scanned (as the 15 s
        // scan or the next start would): nothing there is its own.
        let plain: crate::config::CameraConfig = toml::from_str("name = \"Shed\"\nurl = \"lavfi://color=c=white:s=320x240:r=10\"\nenabled = false\n").unwrap();
        e.add_camera_arc(plain);
        assert_eq!(e.db.import_final_camera_dirs(&[("shed".into(), String::new(), dir.clone())], 2, 0).unwrap(), 0);
        assert!(e.db.segments_overlapping("shed", 0, i64::MAX / 2).unwrap().is_empty());
        // The old recorder wrote nothing after it stopped.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(seg_files(&dir, "shed"), files);
        for h in e.cameras.read().unwrap().iter().flatten() {
            h.active.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
