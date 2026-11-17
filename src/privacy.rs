//! Privacy masks on the pictures the server makes (live JPEGs, snapshots,
//! event thumbnails, stills, previews, filmstrips, time-lapses, AI input).
//!
//! Masks are relative rectangles (0..1). In a picture they become solid
//! black boxes whose edges are moved outward to even pixels (so the halved
//! colour planes of a 4:2:0 picture are covered as well). The main way is a
//! `drawbox` filter in the ffmpeg run that already makes the picture;
//! [`black_out_jpeg`] does the same to a JPEG that is already made.

use crate::config::{CameraConfig, ZoneRect};
use std::borrow::Cow;

/// Smallest mask side (relative) that counts: a box of no size is skipped
/// (drawbox takes a width or height of 0 as "the whole picture").
const MIN_SIDE: f64 = 1e-4;

/// The masks that cover something, as clamped (x0, y0, x1, y1), relative.
fn edges(masks: &[ZoneRect]) -> Vec<(f64, f64, f64, f64)> {
    masks
        .iter()
        .filter(|m| [m.x, m.y, m.w, m.h].iter().all(|v| v.is_finite()))
        .map(|m| {
            let c = |v: f64| v.clamp(0.0, 1.0);
            (c(m.x), c(m.y), c(m.x + m.w), c(m.y + m.h))
        })
        .filter(|(x0, y0, x1, y1)| x1 - x0 >= MIN_SIDE && y1 - y0 >= MIN_SIDE)
        .collect()
}

/// The masks in pixels of a `w` x `h` picture: (x, y, width, height), edges
/// moved outward to even pixels and kept inside the picture.
pub fn pixel_rects(masks: &[ZoneRect], w: u32, h: u32) -> Vec<(u32, u32, u32, u32)> {
    let lo = |v: f64, size: u32| (((v * size as f64) / 2.0).floor() * 2.0).clamp(0.0, size as f64) as u32;
    let hi = |v: f64, size: u32| (((v * size as f64) / 2.0).ceil() * 2.0).clamp(0.0, size as f64) as u32;
    edges(masks)
        .into_iter()
        .filter_map(|(x0, y0, x1, y1)| {
            let (ax, ay, bx, by) = (lo(x0, w), lo(y0, h), hi(x1, w), hi(y1, h));
            (bx > ax && by > ay).then_some((ax, ay, bx - ax, by - ay))
        })
        .collect()
}

/// One `drawbox` (solid black) per mask, joined with commas, for any
/// `-vf`/`-filter_complex` chain; empty when there are none. The sizes are
/// expressions of the picture's own size (iw, ih), so it works before or
/// after scaling; edges are rounded outward to even pixels there. (The
/// relative values are written rounded outward too.)
pub fn mask_filter(masks: &[ZoneRect]) -> String {
    let down = |v: f64| format!("{:.6}", (v * 1e6).floor() / 1e6);
    let up = |v: f64| format!("{:.6}", (v * 1e6).ceil() / 1e6);
    edges(masks)
        .into_iter()
        .map(|(x0, y0, x1, y1)| {
            let (x0, y0, x1, y1) = (down(x0), down(y0), up(x1), up(y1));
            let left = format!("2*floor(iw*{x0}/2)");
            let top = format!("2*floor(ih*{y0}/2)");
            format!("drawbox=x={left}:y={top}:w=2*ceil(iw*{x1}/2)-{left}:h=2*ceil(ih*{y1}/2)-{top}:color=black:t=fill")
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Whether the camera has a privacy mask that covers something.
pub fn has_masks(cfg: &CameraConfig) -> bool {
    !edges(&cfg.privacy_masks).is_empty()
}

/// The camera's mask filter (see [`mask_filter`]).
pub fn camera_mask_filter(cfg: &CameraConfig) -> String {
    mask_filter(&cfg.privacy_masks)
}

/// `a` then `b` in one filter chain (either may be empty).
pub fn chain(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a},{b}"),
    }
}

/// A short tag for the camera's masks and mask level, for picture cache
/// keys: a picture kept under another tag was made with other masks. Empty
/// for a camera without masks.
pub fn revision(cfg: &CameraConfig) -> String {
    rev_of(cfg, true)
}

/// As [`revision`], of the masks alone (not their level): what is hidden,
/// wherever it is hidden.
pub fn masks_revision(cfg: &CameraConfig) -> String {
    rev_of(cfg, false)
}

fn rev_of(cfg: &CameraConfig, level: bool) -> String {
    let e = edges(&cfg.privacy_masks);
    if e.is_empty() {
        return String::new();
    }
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for (x0, y0, x1, y1) in e {
        h.update(format!("{x0:.6},{y0:.6},{x1:.6},{y1:.6};"));
    }
    if level {
        h.update(b"|");
        h.update(cfg.mask_level().as_bytes());
    }
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Motion counts nowhere under a privacy mask: the camera's ignore zones
/// plus its masks.
pub fn motion_ignore(cfg: &CameraConfig) -> Cow<'_, [ZoneRect]> {
    if cfg.privacy_masks.is_empty() {
        Cow::Borrowed(&cfg.ignore_zones)
    } else {
        Cow::Owned(cfg.ignore_zones.iter().chain(cfg.privacy_masks.iter()).cloned().collect())
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

/// Masks then text for a picture from this camera at `clock` (empty: nothing to do).
pub fn picture_filter(cfg: &CameraConfig, clock: Clock) -> String {
    chain(&camera_mask_filter(cfg), &text_filter(cfg, clock).unwrap_or_default())
}

/// The camera named `camera` (its name, or the name's slug).
pub fn camera_cfg(config: &crate::config::Config, camera: &str) -> Option<CameraConfig> {
    let s = crate::config::slug(camera);
    config.cameras.iter().find(|c| c.name == camera).or_else(|| config.cameras.iter().find(|c| crate::config::slug(&c.name) == s)).cloned()
}

/// The camera an event id belongs to (ids start with the camera's slug and
/// a dash; the longest slug that fits wins).
pub fn camera_of_event(config: &crate::config::Config, id: &str) -> Option<CameraConfig> {
    config
        .cameras
        .iter()
        .filter(|c| {
            let s = crate::config::slug(&c.name);
            id.strip_prefix(s.as_str()).and_then(|r| r.strip_prefix('-')).is_some_and(|r| r.starts_with(|ch: char| ch.is_ascii_digit()))
        })
        .max_by_key(|c| crate::config::slug(&c.name).len())
        .cloned()
}

/// The configured camera whose main or sub stream is at `url` (logins left
/// out of the comparison).
pub fn camera_at_url<'a>(config: &'a crate::config::Config, url: &str) -> Option<&'a CameraConfig> {
    let bare = |u: &str| -> Option<String> {
        let mut p = url::Url::parse(u.trim()).ok()?;
        let _ = p.set_username("");
        let _ = p.set_password(None);
        Some(p.to_string().trim_end_matches('/').to_ascii_lowercase())
    };
    let want = bare(url)?;
    config.cameras.iter().find(|c| {
        bare(&c.url).as_deref() == Some(want.as_str()) || c.sub_url.as_deref().and_then(bare).as_deref() == Some(want.as_str())
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

/// One keyframe of a recording (as stored: length-prefixed NAL units, with
/// its avcC / hvcC record) decoded into a JPEG with filter `vf` (the
/// privacy masks): fast playback for a player that does not draw them.
pub fn keyframe_jpeg(k: &crate::ingest::splice::KeyPicture, vf: &str) -> anyhow::Result<Vec<u8>> {
    use crate::ingest::decode::{length_prefixed_to_annexb, parameter_sets_annexb, Codec};
    let codec = if k.codec.starts_with("avc") {
        Codec::H264
    } else if k.codec.starts_with("hvc") || k.codec.starts_with("hev") {
        Codec::H265
    } else {
        anyhow::bail!("not H.264 or H.265: {}", k.codec)
    };
    let mut es = parameter_sets_annexb(codec, &k.desc)?;
    length_prefixed_to_annexb(&k.data, &mut es)?;
    one_picture(&["-f", if codec == Codec::H264 { "h264" } else { "hevc" }], &es, vf)
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

/// Black out `masks` in an already made JPEG (decoded and encoded again
/// here, no ffmpeg). The picture comes back unchanged when there are none.
pub fn black_out_jpeg(jpeg: &[u8], masks: &[ZoneRect]) -> anyhow::Result<Vec<u8>> {
    if edges(masks).is_empty() {
        return Ok(jpeg.to_vec());
    }
    let mut img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?.to_rgb8();
    let (w, h) = img.dimensions();
    for (x, y, rw, rh) in pixel_rects(masks, w, h) {
        for yy in y..y + rh {
            for xx in x..x + rw {
                img.put_pixel(xx, yy, image::Rgb([0, 0, 0]));
            }
        }
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img)?;
    Ok(out)
}

/// The JPEG comment that names the mask revision a picture was made with.
const MARK: &[u8] = b"ambersight-masks:";

/// `jpeg` with a comment saying it was masked with revision `rev`.
pub fn with_mark(jpeg: &[u8], rev: &str) -> Vec<u8> {
    if jpeg.len() < 2 || !jpeg.starts_with(&[0xFF, 0xD8]) {
        return jpeg.to_vec();
    }
    let body: Vec<u8> = MARK.iter().chain(rev.as_bytes()).copied().collect();
    let len = (body.len() + 2) as u16;
    let mut out = Vec::with_capacity(jpeg.len() + body.len() + 4);
    out.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xFE]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// The mask revision a JPEG says it was made with (see [`with_mark`]).
pub fn mark(jpeg: &[u8]) -> Option<String> {
    // Only the segments before the picture data are looked at.
    let mut i = 2;
    while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
        let kind = jpeg[i + 1];
        if kind == 0xDA || kind == 0xD9 {
            return None;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if len < 2 || i + 2 + len > jpeg.len() {
            return None;
        }
        if kind == 0xFE {
            if let Some(r) = jpeg[i + 4..i + 2 + len].strip_prefix(MARK) {
                return Some(String::from_utf8_lossy(r).into_owned());
            }
        }
        i += 2 + len;
    }
    None
}

/// An event thumbnail as it may be shown now: when the camera has masks and
/// the picture was not made with these, they are blacked out (the second
/// value is then the picture to keep in its place).
pub fn thumb_for_masks(jpeg: Vec<u8>, cfg: &CameraConfig) -> (Vec<u8>, bool) {
    let rev = revision(cfg);
    if rev.is_empty() || mark(&jpeg).as_deref() == Some(rev.as_str()) {
        return (jpeg, false);
    }
    match black_out_jpeg(&jpeg, &cfg.privacy_masks) {
        Ok(j) => (with_mark(&j, &rev), true),
        // A picture that cannot be read is not shown unmasked.
        Err(_) => (Vec::new(), false),
    }
}

/// A picture of camera `cfg` kept on disk (an event thumbnail, an AI review
/// frame), read to be shown or sent: when it was made before the camera's
/// privacy masks were set or changed, they are blacked out now and the file
/// is replaced by the masked picture (so this is done once). `cfg` None (the
/// camera is gone): the picture as it is. Err: not there, or it could not be
/// masked (it is then never sent as it is).
pub fn read_masked(path: &std::path::Path, cfg: Option<&CameraConfig>) -> std::io::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    let Some(cfg) = cfg else { return Ok(bytes) };
    let (bytes, changed) = thumb_for_masks(bytes, cfg);
    if bytes.is_empty() {
        return Err(std::io::Error::other("the picture could not be masked"));
    }
    // (Masked whole because its masks could not be read: not kept so.)
    if changed && cfg.url != crate::transcode::masked::UNKNOWN_URL {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let tmp = path.with_extension(format!("{}-{}.part", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        if std::fs::write(&tmp, &bytes).is_err() || std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Ok(bytes)
}

/// As [`read_masked`], for a sender that passes the file itself on (by its
/// path): the file is brought up to date first. False: it must not be sent.
pub fn mask_file(path: &std::path::Path, cfg: Option<&CameraConfig>) -> bool {
    read_masked(path, cfg).is_ok()
}

/// A small picture cut out of camera `cfg`'s masked picture (a face, a
/// plate), noted with the masks it was made with (see [`crop_shown`]).
pub fn mark_crop(jpeg: &[u8], cfg: Option<&CameraConfig>) -> Vec<u8> {
    with_mark(jpeg, &cfg.map(masks_revision).unwrap_or_default())
}

/// Whether a cut-out picture of camera `cfg` (see [`mark_crop`]) may be
/// shown: the camera has no masks now, or it was cut with the ones it has.
/// One cut before a mask change (or before crops were noted) cannot be
/// masked again (where the masks fall in it is not known), so it is not
/// shown. `cfg` None (the camera is gone, or an imported photo): shown.
pub fn crop_shown(jpeg: &[u8], cfg: Option<&CameraConfig>) -> bool {
    let Some(cfg) = cfg else { return true };
    let rev = masks_revision(cfg);
    rev.is_empty() || mark(jpeg).as_deref() == Some(rev.as_str())
}

/// Whose an event's pictures are, for masking them.
#[derive(Debug)]
pub enum EventCamera {
    /// This camera's (its settings now).
    Camera(CameraConfig),
    /// A camera that is no longer set up (the event names it).
    Gone,
    /// Cannot be told: no such event, and no camera's name fits its id.
    Unknown,
}

/// The camera of event `id`: the one its database entry names, else the
/// one its id starts with (see [`camera_of_event`]).
pub fn event_camera(config: &crate::config::Config, ev: Option<&str>, id: &str) -> EventCamera {
    match ev {
        Some(cam) => camera_cfg(config, cam).map_or(EventCamera::Gone, EventCamera::Camera),
        None => camera_of_event(config, id).map_or(EventCamera::Unknown, EventCamera::Camera),
    }
}

/// [`event_camera`], looking the event up in `db`, with the masks of the
/// cameras removed under its camera's name that apply to its time (see
/// `transcode::masked::pictures_cfg`); a camera no longer set up that had
/// masks when it was removed: its masks then.
pub fn event_camera_in(config: &std::sync::Mutex<crate::config::Config>, db: &crate::db::Db, id: &str) -> EventCamera {
    // (The database first, without the settings locked.)
    let ev = db.get_event(id).ok().flatten();
    let found = event_camera(&config.lock().unwrap(), ev.as_ref().map(|e| e.camera.as_str()), id);
    match (found, ev) {
        (EventCamera::Gone, Some(e)) => crate::transcode::masked::gone_cfg(db, &e.camera).map_or(EventCamera::Gone, EventCamera::Camera),
        (EventCamera::Camera(c), ev) => {
            let name = c.name.clone();
            // (Its time not known: all of them.)
            let at = ev.map_or(i64::MIN, |e| e.start.saturating_mul(1000));
            crate::transcode::masked::pictures_cfg(db, Some(c), &name, at).map_or(EventCamera::Gone, EventCamera::Camera)
        }
        (found, _) => found,
    }
}

/// [`read_masked`] for a picture of event `event_id` (its camera found by
/// [`event_camera_in`]). Err when its camera cannot be told at all: the
/// masks it should have are unknown, so it is not sent. A camera no longer
/// set up: the masks it had when removed, else the picture as it is.
pub fn read_event_picture(config: &std::sync::Mutex<crate::config::Config>, db: &crate::db::Db, event_id: &str, path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    match event_camera_in(config, db, event_id) {
        EventCamera::Camera(c) => read_masked(path, Some(&c)),
        EventCamera::Gone => read_masked(path, None),
        EventCamera::Unknown => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "the event's camera is not known, so its masks are not either")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> ZoneRect {
        ZoneRect { x, y, w, h }
    }

    fn cam(masks: Vec<ZoneRect>) -> CameraConfig {
        let mut c: CameraConfig = toml::from_str("name = \"Porch\"\nurl = \"rtsp://192.0.2.10/s\"").unwrap();
        c.privacy_masks = masks;
        c
    }

    #[test]
    fn pixel_edges_move_outward_to_even_pixels() {
        // 0.1 of 645 = 64.5 -> 64; 0.1 + 0.3 = 0.4 of 645 = 258 -> 258.
        assert_eq!(pixel_rects(&[r(0.1, 0.1, 0.3, 0.3)], 645, 363), vec![(64, 36, 194, 110)]);
        // Odd starts go down, odd ends go up.
        assert_eq!(pixel_rects(&[r(0.0031, 0.0, 0.0031, 0.5)], 1000, 100), vec![(2, 0, 6, 50)]);
        // Kept inside the picture (an odd width cannot be overrun).
        assert_eq!(pixel_rects(&[r(0.9, 0.9, 0.5, 0.5)], 101, 101), vec![(90, 90, 11, 11)]);
        assert_eq!(pixel_rects(&[r(-0.5, -0.5, 2.0, 2.0)], 64, 48), vec![(0, 0, 64, 48)]);
    }

    #[test]
    fn empty_and_broken_masks_cover_nothing() {
        assert!(pixel_rects(&[r(0.1, 0.1, 0.0, 0.3), r(f64::NAN, 0.0, 0.1, 0.1), r(1.2, 0.0, 0.1, 0.1)], 640, 360).is_empty());
        assert_eq!(mask_filter(&[r(0.1, 0.1, 0.0, 0.3)]), "");
        assert_eq!(mask_filter(&[]), "");
    }

    #[test]
    fn filter_is_one_black_drawbox_per_mask() {
        let f = mask_filter(&[r(0.125, 0.25, 0.25, 0.5), r(0.5, 0.5, 0.5, 0.5)]);
        assert_eq!(
            f,
            "drawbox=x=2*floor(iw*0.125000/2):y=2*floor(ih*0.250000/2):w=2*ceil(iw*0.375000/2)-2*floor(iw*0.125000/2):h=2*ceil(ih*0.750000/2)-2*floor(ih*0.250000/2):color=black:t=fill,\
drawbox=x=2*floor(iw*0.500000/2):y=2*floor(ih*0.500000/2):w=2*ceil(iw*1.000000/2)-2*floor(iw*0.500000/2):h=2*ceil(ih*1.000000/2)-2*floor(ih*0.500000/2):color=black:t=fill"
        );
        // (No comma inside a box: the chain splits only between boxes.)
        assert_eq!(f.matches(',').count(), 1);
        assert_eq!(chain("a", ""), "a");
        assert_eq!(chain("", "b"), "b");
        assert_eq!(chain("a", "b"), "a,b");
    }

    #[test]
    fn revision_changes_with_the_masks() {
        assert_eq!(revision(&cam(vec![])), "");
        let a = revision(&cam(vec![r(0.1, 0.1, 0.2, 0.2)]));
        let b = revision(&cam(vec![r(0.1, 0.1, 0.2, 0.25)]));
        assert_eq!(a.len(), 12);
        assert_ne!(a, b);
        assert_eq!(a, revision(&cam(vec![r(0.1, 0.1, 0.2, 0.2)])));
    }

    #[test]
    fn masks_are_ignored_for_motion() {
        let mut c = cam(vec![r(0.5, 0.5, 0.1, 0.1)]);
        c.ignore_zones = vec![r(0.0, 0.0, 0.1, 0.1)];
        assert_eq!(motion_ignore(&c).len(), 2);
        c.privacy_masks.clear();
        assert!(matches!(motion_ignore(&c), Cow::Borrowed(_)));
    }

    fn gray_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 200, 200]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode_image(&img).unwrap();
        out
    }

    fn luma_at(jpeg: &[u8], x: u32, y: u32) -> u8 {
        image::load_from_memory(jpeg).unwrap().to_luma8().get_pixel(x, y).0[0]
    }

    #[test]
    fn a_made_jpeg_is_blacked_out_in_the_masks_only() {
        let j = black_out_jpeg(&gray_jpeg(320, 180), &[r(0.25, 0.25, 0.25, 0.25)]).unwrap();
        assert!(luma_at(&j, 100, 60) < 20, "inside");
        assert!(luma_at(&j, 10, 10) > 180, "outside");
        assert!(luma_at(&j, 300, 170) > 180, "outside");
    }

    #[test]
    fn thumbnails_are_masked_once_per_revision() {
        let c = cam(vec![r(0.0, 0.0, 0.5, 0.5)]);
        let rev = revision(&c);
        let (j, changed) = thumb_for_masks(gray_jpeg(160, 90), &c);
        assert!(changed);
        assert_eq!(mark(&j).as_deref(), Some(rev.as_str()));
        assert!(luma_at(&j, 10, 10) < 20);
        let (j2, again) = thumb_for_masks(j.clone(), &c);
        assert!(!again && j2 == j);
        // Other masks: done again.
        let c2 = cam(vec![r(0.5, 0.5, 0.5, 0.5)]);
        let (j3, changed) = thumb_for_masks(j, &c2);
        assert!(changed && luma_at(&j3, 150, 80) < 20);
        // Without masks nothing changes.
        let g = gray_jpeg(32, 32);
        assert_eq!(thumb_for_masks(g.clone(), &cam(vec![])), (g, false));
        // A marked picture still reads as a picture.
        assert!(image::load_from_memory(&with_mark(&gray_jpeg(32, 32), "abc")).is_ok());
    }

    #[test]
    fn saved_pictures_are_masked_when_read_and_kept_masked() {
        let dir = std::env::temp_dir().join(format!("amber-readmasked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ev.jpg");
        std::fs::write(&p, gray_jpeg(160, 90)).unwrap();
        // No camera (removed): as it is.
        assert_eq!(read_masked(&p, None).unwrap(), gray_jpeg(160, 90));
        let c = cam(vec![r(0.0, 0.0, 0.5, 0.5)]);
        let j = read_masked(&p, Some(&c)).unwrap();
        assert!(luma_at(&j, 10, 10) < 20 && luma_at(&j, 150, 80) > 180);
        assert_eq!(std::fs::read(&p).unwrap(), j, "the file is the masked picture now");
        assert_eq!(read_masked(&p, Some(&c)).unwrap(), j, "done once");
        assert!(mask_file(&p, Some(&c)));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no part file left");
        // A picture that cannot be masked is never sent.
        std::fs::write(&p, b"not a picture").unwrap();
        assert!(read_masked(&p, Some(&c)).is_err());
        assert!(!mask_file(&p, Some(&c)));
        assert!(read_masked(&dir.join("gone.jpg"), Some(&c)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real ffmpeg with libx264 (skipped without): recordings of a plain grey
    /// picture, and the pictures the server makes of them for a camera with
    /// a privacy mask (still, preview, filmstrip, time-lapse, AI frames) are
    /// black in the mask and grey outside it; another mask is not served from
    /// the old picture's cache.
    #[test]
    fn pictures_made_from_recordings_are_masked() {
        use crate::engine::playback;
        let name = format!("Mask Test {}", std::process::id());
        let s = crate::config::slug(&name);
        let base = std::env::temp_dir().join(format!("amber-maskpics-{}", std::process::id()));
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
            "[storage]\nfolders = [{{ name = \"rec\", path = '{}' }}]\n[[cameras]]\nname = \"{name}\"\nurl = \"rtsp://192.0.2.10/s\"\noverlay_text = \"{{short}} {{date}} {{time}}\"\nprivacy_masks = [{{ x = 0.5, y = 0.1, w = 0.3, h = 0.4 }}]\n",
            root.display()
        ))
        .unwrap();
        let engine = crate::engine::Engine::for_test(cfg);
        let added = engine.db.import_camera_dirs(&[(s.clone(), "rec".into(), root.clone())], 2).unwrap();
        assert_eq!(added, files.len());
        let rows = engine.db.segments_overlapping(&s, 0, i64::MAX).unwrap();
        let (from, to) = (rows[0].start_ms, rows[rows.len() - 1].end_ms);
        // Black at (0.65, 0.3) of the picture, grey at (0.2, 0.3) and (0.9, 0.7).
        // (`cell`: where the picture is in the JPEG, x, y, w, h; None: all of it.)
        let check = |what: &str, jpeg: &[u8], cell: Option<(u32, u32, u32, u32)>| {
            let img = image::load_from_memory(jpeg).unwrap_or_else(|e| panic!("{what}: {e}")).to_luma8();
            let (x0, y0, w, h) = cell.unwrap_or((0, 0, img.width(), img.height()));
            let at = |fx: f64, fy: f64| img.get_pixel(x0 + (w as f64 * fx) as u32, y0 + (h as f64 * fy) as u32).0[0];
            assert!(at(0.65, 0.3) < 30, "{what}: inside the mask {}", at(0.65, 0.3));
            assert!(at(0.2, 0.3) > 150 && at(0.9, 0.7) > 150, "{what}: outside the mask {} {}", at(0.2, 0.3), at(0.9, 0.7));
        };
        let still = engine.still_at(&name, from + 2500).unwrap().expect("a still");
        check("still", &still, None);
        let (pre, _) = engine.preview_at(&name, from + 2500, 160).unwrap().expect("a preview");
        check("preview", &pre, None);
        let strip = engine.filmstrip(&name, from, 2000, 2, 160).unwrap().expect("a filmstrip");
        // (The first cell, the 4:3 picture in the middle of a 16:9 cell.)
        let pw = strip.h * 4 / 3;
        check("filmstrip", &strip.jpeg, Some(((strip.w - pw) / 2, 0, pw, strip.h)));
        let clip = playback::window_clip(&engine.db, &name, from, to, |r| engine.segment_path(r)).unwrap().unwrap();
        let mut n = 0;
        playback::clip_frames(clip, 2, 320, &camera_mask_filter(&engine.picture_cfg(&name).unwrap()), &mut |_, j| {
            check("AI frame", j, None);
            n += 1;
            Ok(())
        })
        .unwrap();
        assert!(n > 0);
        // Another mask: a new preview, not the cached one.
        engine.config.lock().unwrap().cameras[0].privacy_masks = vec![r(0.0, 0.0, 0.3, 0.3)];
        let (pre2, _) = engine.preview_at(&name, from + 2500, 160).unwrap().unwrap();
        let img = image::load_from_memory(&pre2).unwrap().to_luma8();
        assert!(img.get_pixel(10, 10).0[0] < 30 && img.get_pixel(104, 36).0[0] > 150, "the new mask, not the cached picture");
        engine.config.lock().unwrap().cameras[0].privacy_masks = vec![r(0.5, 0.1, 0.3, 0.4)];
        // The time-lapse of that day.
        let day = chrono::DateTime::from_timestamp_millis(from).unwrap().with_timezone(&chrono::Local).date_naive();
        let tl = engine.make_timelapse(&name, day).expect("a time-lapse");
        // It says which masks are in it: another mask makes it one not served.
        assert!(!engine.timelapse_masks_old(&s, day));
        engine.config.lock().unwrap().cameras[0].privacy_masks = vec![r(0.0, 0.0, 0.3, 0.3)];
        assert!(engine.timelapse_masks_old(&s, day), "made with other masks");
        let _ = std::fs::remove_file(tl.with_extension("masks"));
        // No note: from before notes were kept, its masks unknown: not served at either level.
        assert!(engine.timelapse_masks_old(&s, day), "from before the note, masks in the recordings: not served");
        engine.config.lock().unwrap().cameras[0].mask_mode = Some("screen".into());
        assert!(engine.timelapse_masks_old(&s, day), "from before the note, masks on screen only: not served");
        let o = crate::child::ffmpeg().args(["-v", "error", "-i"]).arg(&tl).args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "2", "-"]).output().unwrap();
        let _ = std::fs::remove_dir_all(tl.parent().unwrap());
        let _ = std::fs::remove_dir_all(&base);
        check("time-lapse", &o.stdout, None);
    }

    #[test]
    fn event_ids_and_addresses_find_their_camera() {
        let c: crate::config::Config = toml::from_str(
            "[[cameras]]\nname = \"Gate\"\nurl = \"rtsp://u:p@192.0.2.10:554/main\"\nsub_url = \"rtsp://192.0.2.10:554/sub\"\n[[cameras]]\nname = \"Gate 2\"\nurl = \"rtsp://192.0.2.11/main\"\n",
        )
        .unwrap();
        assert_eq!(camera_of_event(&c, "gate-1700000000").unwrap().name, "Gate");
        assert_eq!(camera_of_event(&c, "gate_2-1700000000-pkg").unwrap().name, "Gate 2");
        assert!(camera_of_event(&c, "gatehouse-1700000000").is_none());
        assert_eq!(camera_at_url(&c, "rtsp://admin:x@192.0.2.10:554/main").unwrap().name, "Gate");
        assert_eq!(camera_at_url(&c, "rtsp://192.0.2.10:554/sub").unwrap().name, "Gate");
        assert!(camera_at_url(&c, "rtsp://192.0.2.12/main").is_none());
    }

    #[test]
    fn snapshot_text_is_burned_once_per_picture() {
        let mut c = cam(vec![]);
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

    /// Real ffmpeg (skipped without one): the filter blacks out the mask,
    /// also after scaling and on an odd-sized picture, and leaves the rest.
    #[test]
    fn ffmpeg_draws_the_masks() {
        let masks = [r(0.25, 0.25, 0.25, 0.25)];
        let vf = chain("scale=321:-2", &mask_filter(&masks));
        let Ok(o) = crate::child::ffmpeg()
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=0xC8C8C8:size=642x362:rate=1", "-frames:v", "1", "-vf", &vf, "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "2", "-"])
            .output()
        else {
            return;
        };
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let img = image::load_from_memory(&o.stdout).unwrap().to_luma8();
        let (w, h) = img.dimensions();
        for (x, y, rw, rh) in pixel_rects(&masks, w, h) {
            assert!(img.get_pixel(x + 1, y + 1).0[0] < 20 && img.get_pixel(x + rw - 2, y + rh - 2).0[0] < 20);
        }
        assert!(img.get_pixel(5, 5).0[0] > 180 && img.get_pixel(w - 5, h - 5).0[0] > 180);
        // The same through the JPEG burner.
        let again = burn(&o.stdout, &mask_filter(&[r(0.75, 0.75, 0.2, 0.2)])).unwrap();
        assert!(luma_at(&again, w - 20, h - 20) < 20 && luma_at(&again, 5, 5) > 180 && luma_at(&again, 100, 60) < 20);
    }

    #[test]
    fn the_masks_revision_leaves_out_the_level() {
        let mut a = cam(vec![r(0.1, 0.1, 0.2, 0.2)]);
        a.mask_mode = Some("screen".into());
        let mut b = a.clone();
        b.mask_mode = Some("recording".into());
        assert_ne!(revision(&a), revision(&b));
        assert_eq!(masks_revision(&a), masks_revision(&b));
        assert_ne!(masks_revision(&a), masks_revision(&cam(vec![r(0.2, 0.1, 0.2, 0.2)])));
        assert_eq!(masks_revision(&cam(vec![])), "");
    }

    #[test]
    fn crops_cut_before_a_mask_change_are_not_shown() {
        let jpeg = gray_jpeg(16, 16);
        let none = cam(vec![]);
        let one = cam(vec![r(0.1, 0.1, 0.2, 0.2)]);
        let other = cam(vec![r(0.5, 0.5, 0.2, 0.2)]);
        // Cut with these masks: shown while they stay.
        let c = mark_crop(&jpeg, Some(&one));
        assert!(image::load_from_memory(&c).is_ok(), "still a picture");
        assert!(crop_shown(&c, Some(&one)));
        assert!(!crop_shown(&c, Some(&other)), "other masks now");
        assert!(crop_shown(&c, Some(&none)), "no masks now");
        // Cut without masks, then masks were set: not shown.
        let c = mark_crop(&jpeg, Some(&none));
        assert!(crop_shown(&c, Some(&none)));
        assert!(!crop_shown(&c, Some(&one)));
        // From before crops were noted: only for a camera without masks.
        assert!(crop_shown(&jpeg, Some(&none)));
        assert!(!crop_shown(&jpeg, Some(&one)));
        // No camera (gone, or an imported photo): shown.
        assert!(crop_shown(&jpeg, None));
    }

    #[test]
    fn an_events_camera_comes_from_its_entry_else_its_id_else_it_is_not_known() {
        let config: crate::config::Config = toml::from_str("[[cameras]]\nname = \"Front Door\"\nurl = \"rtsp://192.0.2.10/s\"\n\n[[cameras]]\nname = \"Garage\"\nurl = \"rtsp://192.0.2.11/s\"\n").unwrap();
        let name = |e: EventCamera| match e {
            EventCamera::Camera(c) => c.name,
            EventCamera::Gone => "(gone)".into(),
            EventCamera::Unknown => "(unknown)".into(),
        };
        // The entry wins over the id (an id that looks like another camera's).
        assert_eq!(name(event_camera(&config, Some("Garage"), "front_door-1700000000")), "Garage");
        assert_eq!(name(event_camera(&config, None, "front_door-1700000000")), "Front Door");
        assert_eq!(name(event_camera(&config, Some("Old Porch"), "old_porch-1")), "(gone)");
        assert_eq!(name(event_camera(&config, None, "nobody-1")), "(unknown)");
        // Through the database; an unknown camera's picture is not sent.
        let db = crate::db::Db::open_in_memory().unwrap();
        let ev = crate::engine::Event { id: "renamed-1700000000".into(), camera_id: 1, camera: "Garage".into(), start: 1_700_000_000, end: 1_700_000_005, score: 1.0, clip: None, thumb: None, objects: vec![], protect: false };
        db.insert_event(&ev).unwrap();
        let m = std::sync::Mutex::new(config);
        assert_eq!(name(event_camera_in(&m, &db, "renamed-1700000000")), "Garage");
        let dir = std::env::temp_dir().join(format!("amber-evcam-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.jpg");
        std::fs::write(&p, gray_jpeg(8, 8)).unwrap();
        assert!(read_event_picture(&m, &db, "renamed-1700000000", &p).is_ok());
        assert!(read_event_picture(&m, &db, "nobody-1", &p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
