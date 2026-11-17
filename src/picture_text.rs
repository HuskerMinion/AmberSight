//! The camera's text (`overlay_text`) burned into the pictures the server
//! makes (live snapshots, event pictures, stills, previews, time-lapses),
//! and the ffmpeg helpers that do it. (Until 0.11.2 this module also made
//! AmberSight's own privacy masks; cameras have their own.)

use crate::config::CameraConfig;

/// `a` then `b` in one filter chain (either may be empty).
pub fn chain(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a},{b}"),
    }
}

/// Whose clock the date and time in burned-in text show.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Clock {
    /// Now (live pictures).
    Live,
    /// A fixed moment (ms since the epoch): a picture from a recording.
    At(i64),
    /// Each frame's own time: `base_s` (s since the epoch) plus the frame's
    /// time in the stream, which is already in real seconds.
    Pts { base_s: f64 },
}

/// Whether the camera's text is burned into the pictures the server makes:
/// it has text, and the text is not already in the video (`overlay`).
pub fn burns_text(cfg: &CameraConfig) -> bool {
    !cfg.overlay && cfg.overlay_text.as_deref().is_some_and(|t| !t.trim().is_empty())
}

/// The camera's text as a drawtext filter at `clock`, when it is burned
/// into pictures (see [`burns_text`]).
pub fn text_filter(cfg: &CameraConfig, clock: Clock) -> Option<String> {
    (burns_text(cfg) && drawtext_available()).then(|| crate::engine::overlay_text_filter(cfg, clock))
}

/// Whether this ffmpeg can draw text (a build without its font library
/// cannot): pictures are then made without the text rather than not at all.
pub(crate) fn drawtext_available() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        crate::child::ffmpeg()
            .args(["-hide_banner", "-filters"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some("drawtext")))
    })
}

/// The live snapshot of camera `id` with its text burned in (`vf`): made
/// once per picture (`seq`) and kept for the next asker. The picture as it
/// is when that cannot be done.
pub fn snapshot_with_text(id: usize, seq: u64, frame: &[u8], vf: &str) -> Vec<u8> {
    use std::sync::Mutex;
    static LAST: Mutex<Vec<(usize, u64, String, std::sync::Arc<Vec<u8>>)>> = Mutex::new(Vec::new());
    if let Some((.., j)) = LAST.lock().unwrap().iter().find(|(i, s, f, _)| *i == id && *s == seq && f == vf) {
        return j.to_vec();
    }
    match burn(frame, vf) {
        Ok(j) => {
            let mut l = LAST.lock().unwrap();
            l.retain(|(i, ..)| *i != id);
            if l.len() >= 256 {
                l.remove(0);
            }
            l.push((id, seq, vf.to_string(), std::sync::Arc::new(j.clone())));
            j
        }
        Err(e) => {
            tracing::debug!("snapshot text: {e:#}");
            frame.to_vec()
        }
    }
}

/// Run ffmpeg filter `vf` over one JPEG and return the new JPEG. Few at a
/// time; ffmpeg is stopped when it takes longer than a few seconds.
pub fn burn(jpeg: &[u8], vf: &str) -> anyhow::Result<Vec<u8>> {
    one_picture(&["-f", "image2pipe", "-c:v", "mjpeg"], jpeg, vf)
}

/// Run ffmpeg filter `vf` over one picture fed on stdin (`input_args`: its
/// format) and return the first picture made, as a JPEG.
fn one_picture(input_args: &[&str], input: &[u8], vf: &str) -> anyhow::Result<Vec<u8>> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    static JOBS: crate::engine::playback::Gate = crate::engine::playback::Gate::new(3);
    let Some(_slot) = JOBS.enter(std::time::Duration::from_secs(5)) else { anyhow::bail!("busy") };
    let mut child = crate::child::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error"])
        .args(input_args)
        .args(["-i", "-", "-vf", vf])
        .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "4", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = input.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut stdout).take(32 << 20).read_to_end(&mut b);
        b
    });
    let mut stderr = child.stderr.take().expect("piped stderr");
    let errs = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < until => std::thread::sleep(std::time::Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let _ = feeder.join();
    let out = reader.join().unwrap_or_default();
    let err = errs.join().unwrap_or_default();
    if out.len() < 4 || !out.starts_with(&[0xFF, 0xD8]) {
        anyhow::bail!("ffmpeg made no picture: {}", err.lines().next().unwrap_or(""));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> CameraConfig {
        toml::from_str("name = \"Porch\"\nurl = \"rtsp://192.0.2.10/s\"").unwrap()
    }

    #[test]
    fn filters_chain_with_a_comma_only_between_two() {
        assert_eq!(chain("a", ""), "a");
        assert_eq!(chain("", "b"), "b");
        assert_eq!(chain("a", "b"), "a,b");
    }

    fn gray_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 200, 200]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode_image(&img).unwrap();
        out
    }

    #[test]
    fn snapshot_text_is_burned_once_per_picture() {
        let mut c = cam();
        c.overlay_text = Some("{short}".into());
        let Some(vf) = text_filter(&c, Clock::Live) else { return }; // no ffmpeg with text here
        let g = gray_jpeg(320, 180);
        let a = snapshot_with_text(7_000_001, 1, &g, &vf);
        assert_ne!(a, g, "text was drawn");
        assert!(image::load_from_memory(&a).is_ok());
        assert_eq!(snapshot_with_text(7_000_001, 1, &gray_jpeg(8, 8), &vf), a, "kept for the same picture");
        // The camera's own burned-in text is not drawn twice.
        c.overlay = true;
        assert!(text_filter(&c, Clock::Live).is_none());
    }

    /// Against real recordings (grey, 320x240): stills, previews,
    /// filmstrips, AI frames and time-lapses are made, and the camera's text
    /// is burned into the stills and time-lapses (when this ffmpeg can draw
    /// text; else they come without it).
    #[test]
    fn pictures_made_from_recordings_have_the_text_where_it_is_burned() {
        use crate::engine::playback;
        let name = format!("Porch {}", std::process::id());
        let s = crate::config::slug(&name);
        let base = std::env::temp_dir().join(format!("amber-textpics-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("rec");
        let seg_dir = root.join("cameras").join(&s).join("segments");
        std::fs::create_dir_all(&seg_dir).unwrap();
        let t0 = chrono::Utc::now().timestamp_millis() - 15 * 60_000;
        let ok = crate::child::ffmpeg()
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=0xC8C8C8:size=320x240:rate=10:duration=6"])
            .args(["-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p", "-f", "segment", "-segment_time", "2", "-reset_timestamps", "1"])
            .args(["-segment_format_options", "movflags=+frag_keyframe+empty_moov+default_base_moof"])
            .arg(seg_dir.join(format!("{s}-{t0}-seg%06d.mp4")))
            .status()
            .is_ok_and(|st| st.success());
        if !ok {
            eprintln!("skipped: no ffmpeg with libx264");
            return;
        }
        let mut files: Vec<_> = std::fs::read_dir(&seg_dir).unwrap().map(|e| e.unwrap().path()).collect();
        files.sort();
        for (i, f) in files.iter().enumerate() {
            let fh = std::fs::OpenOptions::new().write(true).open(f).unwrap();
            fh.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis((t0 + 2000 * (i as i64 + 1)) as u64)).unwrap();
        }
        let cfg: crate::config::Config = toml::from_str(&format!(
            "[storage]\nfolders = [{{ name = \"rec\", path = '{}' }}]\n[[cameras]]\nname = \"{name}\"\nurl = \"rtsp://192.0.2.10/s\"\noverlay_text = \"{{short}} {{date}} {{time}}\"\n",
            root.display()
        ))
        .unwrap();
        let engine = crate::engine::Engine::for_test(cfg);
        let added = engine.db.import_camera_dirs(&[(s.clone(), "rec".into(), root.clone())], 2).unwrap();
        assert_eq!(added, files.len());
        let rows = engine.db.segments_overlapping(&s, 0, i64::MAX).unwrap();
        let (from, to) = (rows[0].start_ms, rows[rows.len() - 1].end_ms);
        let text = drawtext_available();
        // Grey away from the text (it sits at the bottom left); `text`: the
        // text's dark box or white letters there. (`cell`: where the picture
        // is in the JPEG, x, y, w, h; None: all of it.)
        let check = |what: &str, jpeg: &[u8], cell: Option<(u32, u32, u32, u32)>, text: bool| {
            let img = image::load_from_memory(jpeg).unwrap_or_else(|e| panic!("{what}: {e}")).to_luma8();
            let (x0, y0, w, h) = cell.unwrap_or((0, 0, img.width(), img.height()));
            let at = |fx: f64, fy: f64| img.get_pixel(x0 + (w as f64 * fx) as u32, y0 + (h as f64 * fy) as u32).0[0];
            assert!([(0.5, 0.3), (0.9, 0.2), (0.9, 0.7)].iter().all(|&(x, y)| at(x, y).abs_diff(200) < 25), "{what}: grey picture");
            let marked = (0..50).flat_map(|i| (0..20).map(move |j| (i as f64 / 100.0, 0.8 + j as f64 / 100.0))).filter(|&(x, y)| at(x, y).abs_diff(200) > 60).count();
            assert_eq!(marked > 20, text, "{what}: text at the bottom left ({marked} points)");
        };
        let still = engine.still_at(&name, from + 2500).unwrap().expect("a still");
        check("still", &still, None, text);
        let (pre, _) = engine.preview_at(&name, from + 2500, 160).unwrap().expect("a preview");
        check("preview", &pre, None, false);
        let strip = engine.filmstrip(&name, from, 2000, 2, 160).unwrap().expect("a filmstrip");
        // (The first cell, the 4:3 picture in the middle of a 16:9 cell.)
        let pw = strip.h * 4 / 3;
        check("filmstrip", &strip.jpeg, Some(((strip.w - pw) / 2, 0, pw, strip.h)), false);
        let clip = playback::window_clip(&engine.db, &name, from, to, |r| engine.segment_path(r)).unwrap().unwrap();
        let mut n = 0;
        playback::clip_frames(clip, 2, 320, &mut |_, j| {
            check("AI frame", j, None, false);
            n += 1;
            Ok(())
        })
        .unwrap();
        assert!(n > 0);
        // The time-lapse of that day.
        let day = chrono::DateTime::from_timestamp_millis(from).unwrap().with_timezone(&chrono::Local).date_naive();
        let tl = engine.make_timelapse(&name, day).expect("a time-lapse");
        let o = crate::child::ffmpeg().args(["-v", "error", "-i"]).arg(&tl).args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "2", "-"]).output().unwrap();
        let _ = std::fs::remove_dir_all(tl.parent().unwrap());
        let _ = std::fs::remove_dir_all(&base);
        check("time-lapse", &o.stdout, None, text);
    }
}
