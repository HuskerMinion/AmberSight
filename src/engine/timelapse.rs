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

impl Engine {
    pub fn timelapse_dir(&self) -> PathBuf {
        self.data_dir.join("timelapse")
    }

    pub fn timelapse_path(&self, camera_slug: &str, date: NaiveDate) -> PathBuf {
        self.timelapse_dir().join(camera_slug).join(format!("{date}.mp4"))
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
        // Normally one pass; another only where the recording's format changed.
        while cur < to {
            let Some(clip) = crate::ingest::splice::VirtualClip::plan_trick(&segs, cur, to, speed, FRAME_MS)? else { break };
            let mut dec = crate::child::ffmpeg()
                .args(["-hide_banner", "-loglevel", "error", "-i", "-", "-vf", &fit, "-vsync", "0", "-f", "rawvideo", "-"])
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
                    let _ = std::fs::remove_file(engine.timelapse_path(&s, d));
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
    fn a_day_is_about_24_hours() {
        let (a, b) = day_bounds(NaiveDate::from_ymd_opt(2026, 6, 15).unwrap());
        assert_eq!(b - a, 24 * 3600 * 1000);
    }
}
