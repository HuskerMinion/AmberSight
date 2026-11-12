//! The first seconds of a battery-camera event, from the camera's own SD card.
//!
//! A battery Argus wakes on its PIR, posts its push (pushx) about 2 s later,
//! and AmberSight then signs in and starts the live stream a few seconds more
//! after that: the live recording begins ~5 s into the event. The camera
//! itself records from the trigger, so after each push AmberSight downloads
//! the camera's clip for the moment (cmd 143) and adds the part from before the
//! live recording began as a recording segment of its own, stamped with the
//! time it was recorded. The event then plays from its true start.

use super::*;
use crate::ingest::baichuan::media::{Demux, Frame};

/// The window asked of the camera around a push: from before the trigger
/// (the push lags it) to well after (so a clip that starts late is caught).
const BEFORE_PUSH_MS: i64 = 10_000;
const AFTER_PUSH_MS: i64 = 20_000;
/// Further pushes within this long are the same event: no second download.
const SAME_EVENT_MS: i64 = 60_000;
/// The camera records about a minute per event and cannot hand over a clip it
/// is still writing: ask this long after the push, and once more later if it
/// came back empty.
const FIRST_TRY_AFTER_MS: i64 = 90_000;
const RETRY_AFTER_MS: i64 = 4 * 60_000;

/// Start the lead-in download for a push at `push_ms` (epoch ms), in the
/// background. A burst of pushes from one event starts it once (per camera
/// uid, so a removed camera's id reused by a new one does not suppress it).
pub(super) fn spawn_lead_in(engine: Arc<Engine>, handle: Arc<CameraHandle>, push_ms: i64) {
    static LAST: std::sync::OnceLock<Mutex<std::collections::HashMap<String, i64>>> = std::sync::OnceLock::new();
    let Some(uid) = handle.cfg.read().unwrap().uid.clone().filter(|u| !u.is_empty()) else { return };
    {
        let mut last = LAST.get_or_init(Default::default).lock().unwrap();
        if last.get(&uid).is_some_and(|&t| push_ms - t < SAME_EVENT_MS) {
            return;
        }
        last.insert(uid.clone(), push_ms);
    }
    if !card_worth_asking(&uid, None) {
        return;
    }
    thread::spawn(move || {
        let name = handle.cfg.read().unwrap().name.clone();
        match lead_in(&engine, &handle, push_ms) {
            Ok(()) => {
                card_worth_asking(&uid, Some(true));
            }
            Err(LeadIn::CardEmpty(e)) => {
                card_worth_asking(&uid, Some(false));
                info!("camera '{name}': no recording from the camera's own card for this event ({e})");
            }
            Err(LeadIn::Skipped(e)) => info!("camera '{name}': no recording from the camera's own card for this event ({e:#})"),
        }
    });
}

/// After this many events in a row with nothing on the card (no card, or
/// card recording off), stop asking: every ask wakes the camera. Asked again
/// every CARD_RECHECK_EVERY events in case a card was put in.
const CARD_EMPTY_LIMIT: u32 = 3;
const CARD_RECHECK_EVERY: u32 = 20;

/// Whether to ask this camera's card for the next event (`outcome: None`),
/// or record how the last ask went (`Some(found)`; the return is then unused).
fn card_worth_asking(uid: &str, outcome: Option<bool>) -> bool {
    static EMPTY: std::sync::OnceLock<Mutex<std::collections::HashMap<String, u32>>> = std::sync::OnceLock::new();
    let mut m = EMPTY.get_or_init(Default::default).lock().unwrap();
    let n = m.entry(uid.to_string()).or_insert(0);
    match outcome {
        Some(true) => *n = 0,
        Some(false) => {
            *n += 1;
            if *n == CARD_EMPTY_LIMIT {
                info!("battery camera {}…: its card had nothing for {CARD_EMPTY_LIMIT} events in a row; not asking it for a while (each ask wakes it)", &uid[..uid.len().min(6)]);
            }
        }
        None => {
            if *n >= CARD_EMPTY_LIMIT {
                *n += 1; // counts skipped events toward the recheck
                return (*n - CARD_EMPTY_LIMIT) % CARD_RECHECK_EVERY == 0;
            }
        }
    }
    true
}

enum LeadIn {
    /// The card answered with nothing around the push.
    CardEmpty(String),
    /// Not asked, or nothing usable (no live recording to lead into, ...).
    Skipped(anyhow::Error),
}

impl From<anyhow::Error> for LeadIn {
    fn from(e: anyhow::Error) -> Self {
        LeadIn::Skipped(e)
    }
}

/// When the live recording of this event began: the first segment that
/// starts after the requested window opens. `Err(true)` when a segment
/// already covers the push (recorded from before it: nothing to add);
/// `Err(false)` when the event was not recorded live, so the camera was not
/// meant to record it (recording off, disarmed, outside its schedule).
fn live_recording(engine: &Engine, slug_name: &str, push_ms: i64) -> Result<i64, bool> {
    let segs = engine.db.segments_overlapping(slug_name, push_ms - 60_000, push_ms + 120_000).unwrap_or_default();
    if segs.iter().any(|s| s.start_ms <= push_ms - 2_000 && s.end_ms >= push_ms) {
        return Err(true);
    }
    segs.iter()
        .filter(|s| s.start_ms > push_ms - BEFORE_PUSH_MS)
        .map(|s| s.start_ms)
        .min()
        .ok_or(false)
}

fn lead_in(engine: &Arc<Engine>, handle: &Arc<CameraHandle>, push_ms: i64) -> Result<(), LeadIn> {
    use anyhow::{anyhow, Context};
    let cfg = handle.cfg.read().unwrap().clone();
    let uid = cfg.uid.clone().filter(|u| !u.is_empty()).ok_or_else(|| anyhow!("no camera uid"))?;
    let slug_name = slug(&cfg.name);
    let (from, to) = (push_ms - BEFORE_PUSH_MS, push_ms + AFTER_PUSH_MS);
    let mut got = None;
    for wait_until in [push_ms + FIRST_TRY_AFTER_MS, push_ms + RETRY_AFTER_MS] {
        let wait = wait_until - Utc::now().timestamp_millis();
        if wait > 0 {
            thread::sleep(std::time::Duration::from_millis(wait as u64));
        }
        // The camera may have been renamed, disabled or removed meanwhile.
        if !handle.active.load(std::sync::atomic::Ordering::Relaxed) || handle.cfg.read().unwrap().name != cfg.name {
            return Err(anyhow!("the camera was changed or turned off meanwhile").into());
        }
        // Only when the event was recorded live, and the start is missing.
        match live_recording(engine, &slug_name, push_ms) {
            Ok(_) => {}
            Err(true) => return Err(anyhow!("already recorded from before the push").into()),
            Err(false) => return Err(anyhow!("the event was not recorded live (recording off or not armed then)").into()),
        }
        // Looked up each time: a settings change replaces the session.
        let client = crate::ingest::baichuan::registry::get(&uid).ok_or_else(|| anyhow!("camera session not running"))?;
        let Some(d) = client.download(from, to) else { continue }; // could not reach it: try later
        if d.bytes.is_empty() && d.aborted {
            continue; // gave way to live video: says nothing about the card
        }
        if !d.bytes.is_empty() {
            got = Some((d, client));
            break;
        }
        if wait_until == push_ms + RETRY_AFTER_MS {
            return Err(LeadIn::CardEmpty("the camera's card had nothing around the push".into()));
        }
    }
    let (d, client) = got.ok_or_else(|| anyhow!("could not reach the camera"))?;
    let mut demux = Demux::new();
    let frames = demux.push(&d.bytes);
    let clip_start = demux.clip_start.ok_or_else(|| anyhow!("the recording carried no start time"))?;
    // Which clock the clip's own header is on varies by firmware (older: UTC;
    // newer: the camera's local time). The clip begins just before its push,
    // so take the reading that puts it nearest the push.
    let candidates = [d.card_offset_s, client.camera_clock_offset_s(), Some(0)];
    let clip_start_ms = clip_start_epoch_ms(clip_start, &candidates, push_ms)
        .ok_or_else(|| anyhow!("start time {clip_start} does not fit any known clock"))?;
    // Asked partway into a clip, the camera sends from the asked time but its
    // header still names the clip's start, so the frames' times are unknown.
    if clip_start_ms < from - 2_000 {
        return Err(anyhow!("the push came {} s into the camera's clip; its start cannot be placed", (push_ms - clip_start_ms) / 1000).into());
    }

    // Keep only what is before the live recording (from any segment that
    // reaches past the clip's start, so nothing is recorded twice).
    let cut = engine
        .db
        .segments_overlapping(&slug_name, clip_start_ms, push_ms + 120_000)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.end_ms > clip_start_ms)
        .map(|s| s.start_ms)
        .min()
        .ok_or_else(|| anyhow!("no live recording to lead into"))?;
    let keep = lead_in_frames(&frames, clip_start_ms, cut);
    if keep.last().map_or(0, |l| l.0 - keep[0].0) < 1_000 {
        return Err(anyhow!("less than a second recorded before the live recording began").into());
    }

    // Write it like a live recording: AVCC frames into a triggered segment.
    let (_, folder_root) = engine.record_folder(&cfg);
    let seg_dir = folder_root.join("cameras").join(&slug_name).join("segments");
    std::fs::create_dir_all(&seg_dir).ok();
    let segment_seconds = engine.config.lock().unwrap().storage.segment_seconds;
    let mut bridge = crate::ingest::baichuan::avcc::AvccBridge::new(demux.h265, demux.width, demux.height);
    let mut writer: Option<crate::ingest::recorder::SegmentWriter> = None;
    let first_micros = frames[keep[0].1].micros;
    let mut written = 0u64;
    for &(_, i) in &keep {
        let f = &frames[i];
        let Some(bf) = bridge.push_frame(&f.annexb) else { continue };
        if writer.is_none() {
            let v = bridge.video_params().ok_or_else(|| anyhow!("no video parameters in the recording"))?;
            let tracks = vec![crate::ingest::fmp4::TrackInfo {
                kind: crate::ingest::fmp4::TrackKind::Video,
                timescale: v.clock_rate,
                sample_entry: v.sample_entry,
                width: v.width,
                height: v.height,
            }];
            // Tick 0 is the first kept frame: dated by when it was recorded.
            let w = crate::ingest::recorder::SegmentWriter::new_recorded_at(
                &seg_dir,
                &format!("{slug_name}-{clip_start_ms}-rec"),
                tracks,
                segment_seconds,
                keep[0].0 as f64 / 1000.0,
            )
            .context("starting the recording file")?;
            writer = Some(w);
        }
        let ticks = f.micros.wrapping_sub(first_micros) as i64 * 9 / 100; // µs -> 90 kHz
        writer.as_mut().unwrap().push_video_cts(ticks, 0, bf.data, bf.key).context("writing the recording")?;
        written += 1;
    }
    let mut w = writer.ok_or_else(|| anyhow!("no frames could be converted"))?;
    w.finish().context("finishing the recording")?;
    let secs = (keep.last().unwrap().0 - keep[0].0) as f64 / 1000.0;
    for seg in w.take_closed() {
        engine.segment_closed(&slug_name, seg);
    }
    info!("camera '{}': added {secs:.1} s ({written} frames) of the camera's own recording from before the live recording began", cfg.name);
    Ok(())
}

/// The epoch ms of a header time `start` read on whichever of the clocks
/// (offsets from UTC, s) puts it nearest `push_ms`; `None` if none is within
/// 10 minutes of it.
pub(super) fn clip_start_epoch_ms(start: chrono::NaiveDateTime, offsets: &[Option<i64>], push_ms: i64) -> Option<i64> {
    let base = start.and_utc().timestamp_millis();
    offsets
        .iter()
        .flatten()
        .map(|o| base - o * 1000)
        .min_by_key(|ms| (ms - push_ms).abs())
        .filter(|ms| (ms - push_ms).abs() <= 10 * 60_000)
}

/// Which frames to keep, as (wall-clock ms, index): those recorded before
/// `cut_ms` (where the live recording begins), starting at the first keyframe
/// (a recording must open with one). Frame times are the clip's start plus
/// each frame's timestamp offset from the first.
pub(super) fn lead_in_frames(frames: &[Frame], clip_start_ms: i64, cut_ms: i64) -> Vec<(i64, usize)> {
    let Some(first) = frames.first().map(|f| f.micros) else { return Vec::new() };
    let mut out = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let wall = clip_start_ms + f.micros.wrapping_sub(first) as i64 / 1000;
        if wall >= cut_ms {
            break;
        }
        if out.is_empty() && !f.key {
            continue; // wait for a keyframe
        }
        out.push((wall, i));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(key: bool, micros: u32) -> Frame {
        Frame { annexb: vec![0, 0, 0, 1, 0x65], key, micros }
    }

    #[test]
    fn keeps_frames_before_the_live_recording_from_a_keyframe() {
        // 0.5 s apart, keyframes at 0 and 4 s; live recording begins at 3.2 s.
        let frames: Vec<Frame> = (0..12).map(|i| f(i % 8 == 0, 1_000_000 + i * 500_000)).collect();
        let keep = lead_in_frames(&frames, 10_000, 13_200);
        assert_eq!(keep.first(), Some(&(10_000, 0)), "starts at the clip's first keyframe, at its start time");
        assert_eq!(keep.last(), Some(&(13_000, 6)), "stops before the live recording");
        assert_eq!(keep.len(), 7);
    }

    #[test]
    fn no_live_recording_keeps_it_all_and_leading_non_keyframes_are_skipped() {
        let frames = vec![f(false, 0), f(true, 200_000), f(false, 400_000)];
        let keep = lead_in_frames(&frames, 5_000, i64::MAX);
        assert_eq!(keep, vec![(5_200, 1), (5_400, 2)]);
        assert!(lead_in_frames(&frames, 5_000, 5_100).is_empty(), "nothing before the cut");
        assert!(lead_in_frames(&[], 0, i64::MAX).is_empty());
    }

    #[test]
    fn the_clip_header_is_read_on_the_clock_that_fits_the_push() {
        let t = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        let push = t("2026-11-11 19:27:02").and_utc().timestamp_millis();
        let utc_minus_7 = Some(-7 * 3600);
        // Older firmware: header in UTC, card on local time (UTC-7).
        assert_eq!(clip_start_epoch_ms(t("2026-11-11 19:27:00"), &[utc_minus_7, None, Some(0)], push), Some(push - 2000));
        // Newer firmware: header in the camera's local time.
        assert_eq!(clip_start_epoch_ms(t("2026-11-11 12:27:00"), &[utc_minus_7, None, Some(0)], push), Some(push - 2000));
        // Fits no clock: refused rather than misfiled.
        assert_eq!(clip_start_epoch_ms(t("2026-11-11 03:00:00"), &[utc_minus_7, Some(0)], push), None);
    }

    #[test]
    fn a_camera_whose_card_never_has_anything_is_left_to_sleep() {
        let uid = "TESTUID-card-empty";
        for _ in 0..CARD_EMPTY_LIMIT {
            assert!(card_worth_asking(uid, None));
            card_worth_asking(uid, Some(false));
        }
        // Now skipped, except one ask every CARD_RECHECK_EVERY events.
        let asked = (0..CARD_RECHECK_EVERY * 2).filter(|_| card_worth_asking(uid, None)).count();
        assert_eq!(asked, 2);
        // A card that answers again resets it.
        card_worth_asking(uid, Some(true));
        assert!(card_worth_asking(uid, None));
        assert!(card_worth_asking(uid, None));
    }

    #[test]
    fn a_wrapping_timestamp_still_counts_forward() {
        let frames = vec![f(true, u32::MAX - 99_999), f(false, 100_000)];
        assert_eq!(lead_in_frames(&frames, 0, i64::MAX), vec![(0, 0), (200, 1)]);
    }
}
