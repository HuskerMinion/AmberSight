//! Time-lapse: a day of one camera's recordings squeezed into a short video
//! (one picture every so many seconds, played at 30 per second). Made each
//! night for yesterday, or on request, into `<data>/timelapse/<camera>/<date>.mp4`.
//!
//! Only keyframes are decoded (the recordings are never decoded in full):
//! the sub stream when it was recorded all day, else the main stream.

use super::*;
use chrono::{Local, NaiveDate, TimeZone, Timelike};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const FPS: u32 = 30;
const MAX_WIDTH: u32 = 1280;
/// The masks note of a time-lapse made without masks.
const NO_MASKS: &str = "none";

/// A child process that is stopped and reaped if still held when dropped
/// (an early return on an error must not leave ffmpeg running).
struct Reap(Option<std::process::Child>);

impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Time-lapses being made now, as (camera slug, date).
fn making() -> &'static Mutex<HashSet<(String, NaiveDate)>> {
    static M: OnceLock<Mutex<HashSet<(String, NaiveDate)>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// Local midnight to midnight, ms since the epoch.
pub fn day_bounds(date: NaiveDate) -> (i64, i64) {
    let at = |d: NaiveDate| {
        Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .map(|t| t.timestamp_millis())
            .unwrap_or_else(|| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
    };
    (at(date), at(date.succ_opt().unwrap_or(date)))
}

/// Width and height of a recording's video, from ffmpeg's description of it.
fn video_size(path: &std::path::Path) -> Option<(u32, u32)> {
    let out = crate::child::ffmpeg().args(["-hide_banner", "-i"]).arg(path).stdout(Stdio::null()).stderr(Stdio::piped()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let line = text.lines().find(|l| l.contains("Video:"))?;
    line.split([',', ' ']).find_map(|w| {
        let (a, b) = w.split_once('x')?;
        let (a, b) = (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?);
        (a >= 16 && b >= 16).then_some((a, b))
    })
}

/// Output size: at most MAX_WIDTH wide, the recording's shape, even numbers.
fn output_size(w: u32, h: u32) -> (u32, u32) {
    let ow = w.min(MAX_WIDTH) & !1;
    let oh = ((ow as f64 * h as f64 / w as f64).round() as u32).max(2) & !1;
    (ow, oh)
}

/// The decoder's filters for one pass: `masks` on the picture as recorded,
/// then `fit`, then the camera's text when it is burned into pictures. The
/// clip shows the recording at `start_ms + t * speed` (t: its own time), so
/// its clock is sped up by `speed` for the text.
fn frame_filter(masks: &str, fit: &str, cfg: Option<&CameraConfig>, start_ms: i64, speed: u32) -> String {
    let mut vf = crate::privacy::chain(masks, fit);
    if let Some(t) = cfg.and_then(|c| crate::privacy::text_filter(c, crate::privacy::Clock::Pts { base_s: start_ms as f64 / 1000.0 })) {
        vf = format!("{vf},setpts=PTS*{speed},{t}");
    }
    vf
}

impl Engine {
    pub fn timelapse_dir(&self) -> PathBuf {
        self.data_dir.join("timelapse")
    }

    pub fn timelapse_path(&self, camera_slug: &str, date: NaiveDate) -> PathBuf {
        self.timelapse_dir().join(camera_slug).join(format!("{date}.mp4"))
    }

    /// Beside a time-lapse: the privacy masks burned into it (see
    /// privacy::masks_revision; NO_MASKS when made without any). Every
    /// time-lapse made since 0.11.1 has one.
    fn timelapse_masks_path(&self, camera_slug: &str, date: NaiveDate) -> PathBuf {
        self.timelapse_dir().join(camera_slug).join(format!("{date}.masks"))
    }

    /// A time-lapse made with other privacy masks than the camera has now: it
    /// may show what they hide, so it is not served. One without a note was
    /// made before notes were kept: which masks it has cannot be told, so for
    /// a camera with masks it counts as old too (made again, it is served).
    pub fn timelapse_masks_old(&self, camera_slug: &str, date: NaiveDate) -> bool {
        // (With the masks of cameras removed under the name that apply to that day.)
        let Some(cfg) = self.picture_cfg_at(camera_slug, day_bounds(date).0) else { return false };
        let rev = crate::privacy::masks_revision(&cfg);
        if rev.is_empty() {
            return false;
        }
        match std::fs::read_to_string(self.timelapse_masks_path(camera_slug, date)) {
            Ok(r) => r.trim() != rev,
            Err(_) => true,
        }
    }

    /// Delete a time-lapse (and its masks note).
    pub fn remove_timelapse(&self, camera_slug: &str, date: NaiveDate) {
        let _ = std::fs::remove_file(self.timelapse_path(camera_slug, date));
        let _ = std::fs::remove_file(self.timelapse_masks_path(camera_slug, date));
    }

    /// Make (or remake) one camera's time-lapse for `date`. Blocking; takes
    /// a minute or two for a full day.
    pub fn make_timelapse(&self, camera: &str, date: NaiveDate) -> anyhow::Result<PathBuf> {
        let cam = slug(camera);
        {
            let mut m = making().lock().unwrap();
            if !m.insert((cam.clone(), date)) {
                anyhow::bail!("already being made");
            }
        }
        // One at a time (each is two ffmpeg runs over a day of recordings).
        static JOBS: super::playback::Gate = super::playback::Gate::new(1);
        let r = match JOBS.enter(Duration::from_secs(3 * 3600)) {
            Some(_slot) => self.make_timelapse_inner(camera, &cam, date),
            None => Err(anyhow::anyhow!("another time-lapse took too long")),
        };
        making().lock().unwrap().remove(&(cam, date));
        r
    }

    fn make_timelapse_inner(&self, camera: &str, cam: &str, date: NaiveDate) -> anyhow::Result<PathBuf> {
        let seconds = self.config.lock().unwrap().timelapse.seconds.clamp(10, 600);
        let (from, day_end) = day_bounds(date);
        let to = day_end.min(Utc::now().timestamp_millis());
        if to <= from {
            anyhow::bail!("{date} has not started yet");
        }
        // The sub stream when it was recorded: far less to decode.
        let segs = super::playback::sources_q(&self.db, cam, from, to, true, |r| self.segment_path(r))?;
        let Some(first) = segs.first() else { anyhow::bail!("no recordings of {camera} on {date}") };
        // A day recorded only in part (or today so far) still fills the length.
        let from = from.max(first.start_ms);
        let (w, h) = video_size(&first.path).ok_or_else(|| anyhow::anyhow!("cannot read the size of {}", first.path.display()))?;
        let (ow, oh) = output_size(w, h);
        // One picture per `step` of the day; plan_trick's frame length is
        // only pacing (the encoder sets the real rate).
        let frames = (seconds * FPS) as f64;
        let step_ms = ((to - from) as f64 / frames).max(1000.0);
        const FRAME_MS: u32 = 100;
        let speed = (step_ms / FRAME_MS as f64).round().max(1.0) as u32;

        let dest = self.timelapse_path(cam, date);
        std::fs::create_dir_all(dest.parent().unwrap())?;
        let tmp = dest.with_extension("part.mp4");
        // Whatever goes wrong, no half-written file is left behind.
        struct Partial(PathBuf, bool);
        impl Drop for Partial {
            fn drop(&mut self) {
                if !self.1 {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
        }
        let mut partial = Partial(tmp.clone(), false);
        let mut enc = crate::child::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
            .args(["-s", &format!("{ow}x{oh}"), "-r", &FPS.to_string(), "-i", "-"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "30", "-threads", "2", "-pix_fmt", "yuv420p", "-movflags", "+faststart", "-f", "mp4"])
            .arg(&tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot run ffmpeg: {e}"))?;
        let mut enc_in = enc.stdin.take().expect("piped stdin");
        let mut enc = Reap(Some(enc));
        let mut cur = from;
        let mut pictures = 0usize;
        let fit = format!("scale={ow}:{oh}:force_original_aspect_ratio=decrease,pad={ow}:{oh}:(ow-iw)/2:(oh-ih)/2,format=yuv420p");
        // Privacy masks on each picture as recorded; the camera's text with
        // each picture's own time.
        // (Also those of cameras removed under its name, for what they recorded that day.)
        let look = self.picture_cfg_at(camera, from);
        let masks = look.as_ref().map(crate::privacy::camera_mask_filter).unwrap_or_default();
        // Normally one pass; another only where the recording's format changed.
        while cur < to {
            let Some(clip) = crate::ingest::splice::VirtualClip::plan_trick(&segs, cur, to, speed, FRAME_MS)? else { break };
            let vf = frame_filter(&masks, &fit, look.as_ref(), clip.start_ms, speed);
            let mut dec = crate::child::ffmpeg()
                .args(["-hide_banner", "-loglevel", "error", "-i", "-", "-vf", &vf])
                .args(crate::ffmpeg_bin::passthrough_args())
                .args(["-f", "rawvideo", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| anyhow::anyhow!("cannot run ffmpeg: {e}"))?;
            let mut dec_in = dec.stdin.take().expect("piped stdin");
            let mut out = dec.stdout.take().expect("piped stdout");
            let mut dec = Reap(Some(dec));
            let feed = clip.clone();
            let feeder = std::thread::spawn(move || {
                let len = feed.len();
                let mut pos = 0;
                while pos < len {
                    let end = (pos + (1 << 20)).min(len);
                    match feed.read_range(pos, end) {
                        Ok(b) if dec_in.write_all(&b).is_ok() => pos = end,
                        _ => break,
                    }
                }
            });
            let mut buf = vec![0u8; 1 << 20];
            let frame_bytes = (ow * oh * 3 / 2) as usize;
            let mut bytes = 0usize;
            loop {
                let n = out.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                bytes += n;
                enc_in.write_all(&buf[..n]).map_err(|e| anyhow::anyhow!("encoder stopped: {e}"))?;
            }
            let _ = feeder.join();
            if let Some(mut d) = dec.0.take() {
                let _ = d.wait();
            }
            pictures += bytes / frame_bytes.max(1);
            if clip.end_ms <= cur {
                break;
            }
            cur = clip.end_ms;
        }
        drop(enc_in);
        let res = enc.0.take().expect("encoder").wait_with_output()?;
        if !res.status.success() || pictures == 0 {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!("ffmpeg could not make it: {}", String::from_utf8_lossy(&res.stderr).lines().next().unwrap_or("no pictures"));
        }
        let _ = std::fs::remove_file(&dest);
        std::fs::rename(&tmp, &dest)?;
        partial.1 = true;
        // Which masks are in it (a later mask change makes it one not served).
        // (Always written, "none" without masks: no note means from before.)
        let rev = look.as_ref().map(crate::privacy::masks_revision).unwrap_or_default();
        let note = self.timelapse_masks_path(cam, date);
        if let Err(e) = std::fs::write(&note, if rev.is_empty() { NO_MASKS } else { rev.as_str() }) {
            warn!("time-lapse: {camera} {date}: {e}");
        }
        info!("time-lapse: {camera} {date}: {pictures} pictures, {:.1} MB", std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) as f64 / 1048576.0);
        Ok(dest)
    }

    /// Make one in the background (admin "make now").
    pub fn start_timelapse(self: &Arc<Self>, camera: String, date: NaiveDate) -> bool {
        if making().lock().unwrap().contains(&(slug(&camera), date)) {
            return false;
        }
        let engine = self.clone();
        thread::spawn(move || {
            if let Err(e) = engine.make_timelapse(&camera, date) {
                warn!("time-lapse: {camera} {date}: {e:#}");
            }
        });
        true
    }

    /// Every time-lapse on disk, plus those being made: (slug, date, bytes, making).
    pub fn timelapses(&self) -> Vec<(String, NaiveDate, u64, bool)> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.timelapse_dir()) {
            for cam in rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
                let s = cam.file_name().to_string_lossy().into_owned();
                for f in std::fs::read_dir(cam.path()).into_iter().flatten().filter_map(|e| e.ok()) {
                    let name = f.file_name().to_string_lossy().into_owned();
                    let Some(d) = name.strip_suffix(".mp4").and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()) else { continue };
                    out.push((s.clone(), d, f.metadata().map(|m| m.len()).unwrap_or(0), false));
                }
            }
        }
        for (s, d) in making().lock().unwrap().iter() {
            match out.iter_mut().find(|x| &x.0 == s && x.1 == *d) {
                Some(x) => x.3 = true,
                None => out.push((s.clone(), *d, 0, true)),
            }
        }
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }
}

/// Each night after [timelapse] hour: yesterday's for every chosen camera,
/// and old ones deleted.
pub fn timelapse_loop(engine: Arc<Engine>) {
    let mut tried: HashSet<(String, NaiveDate)> = HashSet::new();
    thread::sleep(Duration::from_secs(120));
    loop {
        let (tl, cams) = {
            let c = engine.config.lock().unwrap();
            (c.timelapse.clone(), c.cameras.clone())
        };
        let now = Local::now();
        if tl.enabled && now.hour() >= tl.hour {
            let yesterday = now.date_naive().pred_opt().unwrap_or(now.date_naive());
            for cam in cams.iter().filter(|c| tl.wants(c)) {
                let key = (slug(&cam.name), yesterday);
                // A file made before the day was over ("Make now" during it)
                // is only part of the day: made again.
                let done = std::fs::metadata(engine.timelapse_path(&key.0, yesterday))
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| chrono::DateTime::<Utc>::from(t).timestamp_millis() >= day_bounds(yesterday).1);
                if done || !tried.insert(key) {
                    continue;
                }
                match engine.make_timelapse(&cam.name, yesterday) {
                    Ok(_) => {}
                    Err(e) => info!("time-lapse: {} {yesterday}: {e:#}", cam.name),
                }
            }
        }
        if tl.keep_days > 0 {
            let oldest = now.date_naive() - chrono::Duration::days(tl.keep_days as i64);
            for (s, d, _, busy) in engine.timelapses() {
                if d < oldest && !busy {
                    engine.remove_timelapse(&s, d);
                }
            }
        }
        tried.retain(|(_, d)| *d >= now.date_naive() - chrono::Duration::days(3));
        thread::sleep(Duration::from_secs(600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_keeps_the_shape_and_even_sizes() {
        assert_eq!(output_size(2560, 1440), (1280, 720));
        assert_eq!(output_size(4608, 1728), (1280, 480));
        assert_eq!(output_size(640, 360), (640, 360));
        assert_eq!(output_size(1001, 563), (1000, 562));
    }

    #[test]
    fn frames_get_masks_first_and_text_on_the_recordings_clock() {
        let mut c: CameraConfig = toml::from_str("name = \"Porch\"\nurl = \"rtsp://192.0.2.10/s\"").unwrap();
        assert_eq!(frame_filter("drawbox=m", "scale=fit", Some(&c), 1_700_000_000_000, 300), "drawbox=m,scale=fit");
        c.overlay_text = Some("{short} {time}".into());
        let f = frame_filter("drawbox=m", "scale=fit", Some(&c), 1_700_000_000_000, 300);
        if !crate::privacy::drawtext_available() {
            // (No ffmpeg, or one that can't draw text: the frames come without the text.)
            assert_eq!(f, "drawbox=m,scale=fit");
            return;
        }
        assert!(f.starts_with("drawbox=m,scale=fit,setpts=PTS*300,drawtext="), "{f}");
        assert!(f.contains("Porch %{pts\\:localtime\\:1700000000.000\\:%T}"), "{f}");
        assert_eq!(frame_filter("", "scale=fit", None, 0, 1), "scale=fit");
    }

    #[test]
    fn a_time_lapse_is_served_only_with_the_masks_the_camera_has() {
        let dir = std::env::temp_dir().join(format!("amber-tl-masks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = Engine::for_test(toml::from_str("[[cameras]]\nname = \"Porch\"\nurl = \"rtsp://192.0.2.10/s\"\nmask_mode = \"recording\"\nprivacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\n").unwrap());
        Arc::get_mut(&mut e).unwrap().data_dir = dir.clone();
        let d = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        std::fs::create_dir_all(e.timelapse_dir().join("porch")).unwrap();
        let note = e.timelapse_masks_path("porch", d);
        // No note: made before notes were kept; which masks it has is not known.
        assert!(e.timelapse_masks_old("porch", d));
        // Made without masks ("none"), or with other ones: old.
        std::fs::write(&note, NO_MASKS).unwrap();
        assert!(e.timelapse_masks_old("porch", d));
        std::fs::write(&note, "0123456789ab").unwrap();
        assert!(e.timelapse_masks_old("porch", d));
        // With these masks (whatever their level): served.
        let cfg = e.picture_cfg("porch").unwrap();
        std::fs::write(&note, crate::privacy::masks_revision(&cfg)).unwrap();
        assert!(!e.timelapse_masks_old("porch", d));
        e.config.lock().unwrap().cameras[0].mask_mode = Some("screen".into());
        assert!(!e.timelapse_masks_old("porch", d));
        // A camera without masks: any.
        e.config.lock().unwrap().cameras[0].privacy_masks.clear();
        let _ = std::fs::remove_file(&note);
        assert!(!e.timelapse_masks_old("porch", d));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_day_is_about_24_hours() {
        let (a, b) = day_bounds(NaiveDate::from_ymd_opt(2026, 6, 15).unwrap());
        assert_eq!(b - a, 24 * 3600 * 1000);
    }
}
