//! Video that leaves the server for a camera whose privacy masks are hidden
//! "on screen" (`mask_mode = "screen"`): its recordings and its live stream
//! are the camera's own picture, masks not burned in. So:
//!
//! * a player that draws the masks itself says so with `draws=masks` (the
//!   web page always does) and gets the video as it is;
//! * any other player gets the masks burned in by a conversion (playback,
//!   event clips, fast and backwards playback), or no live video stream at
//!   all (it falls back to the live JPEG pictures, which are masked);
//! * a download or an export always has the masks burned in, `draws=masks`
//!   or not: the file leaves AmberSight.
//!
//! Downloads also carry a subtitle track with the wall-clock time of each
//! second (mov_text / 3GPP timed text).

use super::file::{Input, Job};
use super::{mp4, OutCodec, Target, Why};
use crate::config::{CameraConfig, ZoneRect};
use crate::engine::Engine;
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// Whether the video this camera sends (its live stream, and what it
/// records from now on) shows what its privacy masks cover: it has masks,
/// and they are not burned into its video: hidden on screen only, or set to
/// "recording" on a camera that has no ffmpeg reader (pushed, battery: its
/// stream is never re-encoded, whatever the setting says). The ONE place
/// this is decided, for live video, conversions and sharing.
pub fn sends_unmasked(cfg: &CameraConfig) -> bool {
    // (A mask of no size hides nothing: the same rule as the pictures, privacy.rs.)
    crate::privacy::has_masks(cfg) && (!cfg.mask_in_recording() || cfg.no_ffmpeg_reader())
}

/// ui_kv scope of the records below.
pub const SINCE_SCOPE: &str = "system";

/// How long after a change of masks or mask level the camera's recordings
/// are still taken as made before it (the old reader may finish a segment
/// it began just then).
pub const SETTLE_MS: i64 = 15_000;

/// Key prefix of the "masked from" records ([`note_level`]), then the slug
/// of the camera's name (where its recordings are kept).
pub const SINCE_PREFIX: &str = "mask_rec_since:";

/// Key prefix of the masks of removed cameras ([`remember_removed`]):
/// `mask_gone:<slug>|<until ms>|<random>`.
pub const GONE_PREFIX: &str = "mask_gone:";

/// Key prefix of the masks each camera set up has (written whenever its
/// settings are applied, see [`note_level`]), then the slug of its name: a
/// camera taken out of the settings file by hand while the server was
/// stopped leaves this behind, and the next start keeps its masks for its
/// recordings as a removal would (see [`prune_records`]).
pub const KNOWN_PREFIX: &str = "mask_known:";

fn known_key(slug: &str) -> String {
    format!("{KNOWN_PREFIX}{slug}")
}

fn since_key(camera: &str) -> String {
    format!("{SINCE_PREFIX}{}", crate::config::slug(camera))
}

/// Slugs whose "masked from" record could not be removed or rewritten when
/// it had to be: it is not trusted (until a new one is written).
static DOUBTFUL: std::sync::Mutex<std::collections::BTreeSet<String>> = std::sync::Mutex::new(std::collections::BTreeSet::new());

fn doubt(slug: &str, why: &dyn std::fmt::Display) {
    tracing::warn!("camera '{slug}': could not note its mask level: {why}; its recordings count as unmasked");
    DOUBTFUL.lock().unwrap().insert(slug.to_string());
}

fn doubtful(slug: &str) -> bool {
    DOUBTFUL.lock().unwrap().contains(slug)
}

/// Whether the masks of this camera are burned into what it records from
/// now on (it has masks, at the recording level, with an ffmpeg reader).
pub fn burns_in(cfg: &CameraConfig) -> bool {
    crate::privacy::has_masks(cfg) && !sends_unmasked(cfg)
}

/// Since when (epoch ms) this camera's recordings really carry the masks it
/// has now (burned in by its re-encoding), as noted by [`note_level`]. None:
/// they may not (not at the recording level, no ffmpeg reader, masks changed
/// since, a record of another level or one that could not be removed when
/// it had to be, or nothing noted).
pub fn masked_since(db: &crate::db::Db, cfg: &CameraConfig) -> Option<i64> {
    if !burns_in(cfg) || doubtful(&crate::config::slug(&cfg.name)) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&db.ui_get(SINCE_SCOPE, &since_key(&cfg.name)).ok()??).ok()?;
    if v["rev"].as_str()? != crate::privacy::masks_revision(cfg) || v["level"].as_str()? != "recording" {
        return None;
    }
    v["since"].as_i64()
}

/// Note from when this camera's recordings carry its masks. At the
/// recording level (with an ffmpeg reader): when there is no valid record
/// for these masks (or `fresh`: always), one with the moment `now_ms` (plus
/// [`SETTLE_MS`]). At any other level the record is dropped, so going back
/// to "recording" later starts again from then. Called at start (nothing is
/// known about the recordings before: from then) and whenever a camera's
/// settings are applied (`fresh` when the camera was renamed, or did not
/// burn its masks in before: a record kept from earlier says nothing about
/// the recordings in between).
pub fn note_level(db: &crate::db::Db, cfg: &CameraConfig, now_ms: i64, fresh: bool) {
    note_known(db, cfg);
    let slug = crate::config::slug(&cfg.name);
    // This run's recorder: which masks it burns in, from when (the rows of
    // what it records carry them; see current_needs).
    if burns_in(cfg) {
        let rev = crate::privacy::masks_revision(cfg);
        let keep = !fresh && db.mask_run(&slug).is_some_and(|(r, _)| r == rev);
        if !keep {
            db.set_mask_run(&slug, Some((rev, now_ms + SETTLE_MS)));
        }
    } else {
        db.set_mask_run(&slug, None);
    }
    if !burns_in(cfg) {
        forget_record(db, &cfg.name);
        return;
    }
    if !fresh && masked_since(db, cfg).is_some() {
        return;
    }
    let v = serde_json::json!({ "rev": crate::privacy::masks_revision(cfg), "level": "recording", "since": now_ms + SETTLE_MS });
    // (Not noted: no recording counts as masked, every one is burned in.)
    match db.ui_put(SINCE_SCOPE, &since_key(&cfg.name), &v.to_string()) {
        Ok(()) => {
            DOUBTFUL.lock().unwrap().remove(&slug);
        }
        Err(e) => doubt(&slug, &format!("{e:#}")),
    }
}

/// Keep the masks camera `cfg` has now under the slug of its name (see
/// [`KNOWN_PREFIX`]); none: no record. Written only when it changed.
fn note_known(db: &crate::db::Db, cfg: &CameraConfig) {
    let key = known_key(&crate::config::slug(&cfg.name));
    if !crate::privacy::has_masks(cfg) {
        if !matches!(db.ui_get(SINCE_SCOPE, &key), Ok(None)) {
            if let Err(e) = db.ui_delete(SINCE_SCOPE, &key) {
                tracing::warn!("camera '{}': could not drop the record of its privacy masks: {e:#}", cfg.name);
            }
        }
        return;
    }
    let v = serde_json::json!({ "name": cfg.name, "masks": cfg.privacy_masks }).to_string();
    if db.ui_get(SINCE_SCOPE, &key).ok().flatten().as_deref() == Some(v.as_str()) {
        return;
    }
    if let Err(e) = db.ui_put(SINCE_SCOPE, &key, &v) {
        tracing::warn!("camera '{}': could not note its privacy masks: {e:#}", cfg.name);
    }
}

/// Drop the "masked from" record of the camera called `camera` (removed, or
/// no longer burning its masks in). One that cannot be dropped is not
/// trusted from now on.
pub fn forget_record(db: &crate::db::Db, camera: &str) {
    let key = since_key(camera);
    let slug = crate::config::slug(camera);
    match db.ui_get(SINCE_SCOPE, &key) {
        Ok(None) => {}
        Ok(Some(_)) => {
            if let Err(e) = db.ui_delete(SINCE_SCOPE, &key) {
                doubt(&slug, &format!("{e:#}"));
            }
        }
        Err(e) => doubt(&slug, &format!("{e:#}")),
    }
}

/// At start (`now_ms`): drop the "masked from" records of cameras not set
/// up (a record left from a removed or renamed camera must not be taken up
/// by a camera given its name later); keep the masks of cameras taken out
/// of the settings file by hand (renamed or removed there while the server
/// was stopped) for their recordings, as a removal does (see
/// [`KNOWN_PREFIX`]); and drop the masks of removed cameras whose
/// recordings and events from before their removal are all gone, from the
/// index and from the camera's folder under the recording `roots` (a file
/// not indexed yet, the start-up indexing adds it under that name).
pub fn prune_records(db: &crate::db::Db, cameras: &[CameraConfig], roots: &[std::path::PathBuf], now_ms: i64) {
    let slugs: std::collections::BTreeSet<String> = cameras.iter().map(|c| crate::config::slug(&c.name)).collect();
    match db.ui_scope_prefix(SINCE_SCOPE, KNOWN_PREFIX) {
        Ok(rows) => {
            for (k, v) in rows {
                let slug = &k[KNOWN_PREFIX.len()..];
                if slugs.contains(slug) {
                    continue;
                }
                // (A record that cannot be read: all of the picture.)
                let masks: Vec<ZoneRect> = serde_json::from_str::<serde_json::Value>(&v)
                    .ok()
                    .and_then(|v| serde_json::from_value(v["masks"].clone()).ok())
                    .unwrap_or_else(|| vec![ZoneRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }]);
                match put_gone(db, slug, &masks, now_ms) {
                    Ok(()) => {
                        tracing::info!("privacy masks: camera folder '{slug}' is no longer in the settings; its recordings keep the masks it had");
                        let _ = db.ui_delete(SINCE_SCOPE, &k);
                    }
                    Err(e) => tracing::warn!("privacy masks: could not keep the masks of '{slug}' (no longer set up) for its recordings: {e:#}"),
                }
            }
        }
        Err(e) => tracing::warn!("privacy masks: could not read the mask records: {e:#}"),
    }
    match db.ui_scope_prefix(SINCE_SCOPE, SINCE_PREFIX) {
        Ok(rows) => {
            for (k, _) in rows {
                let slug = &k[SINCE_PREFIX.len()..];
                if !slugs.contains(slug) {
                    if let Err(e) = db.ui_delete(SINCE_SCOPE, &k) {
                        doubt(slug, &format!("{e:#}"));
                    }
                }
            }
        }
        Err(e) => tracing::warn!("privacy masks: could not read the mask level records: {e:#}"),
    }
    let Ok(rows) = db.ui_scope_prefix(SINCE_SCOPE, GONE_PREFIX) else { return };
    for (k, _) in rows {
        let Some((slug, until)) = parse_gone_key(&k) else { continue };
        // (Kept when that cannot be told.)
        if db.footage_before(slug, until).is_ok_and(|any| !any) && !files_before(roots, slug, until) {
            let _ = db.ui_delete(SINCE_SCOPE, &k);
        }
    }
}

/// (slug, until) of a [`GONE_PREFIX`] key.
fn parse_gone_key(k: &str) -> Option<(&str, i64)> {
    let mut p = k.strip_prefix(GONE_PREFIX)?.split('|');
    let slug = p.next()?;
    let until = p.next()?.parse().ok()?;
    Some((slug, until))
}

/// A camera with privacy masks is being removed: its recordings stay (until
/// they age out), so its masks are kept with them (by the slug of its name,
/// for what was recorded before now), for its footage and pictures. Err:
/// they could not be kept (the camera is then not removed).
pub fn remember_removed(db: &crate::db::Db, cfg: &CameraConfig, now_ms: i64) -> anyhow::Result<()> {
    let slug = crate::config::slug(&cfg.name);
    if crate::privacy::has_masks(cfg) {
        put_gone(db, &slug, &cfg.privacy_masks, now_ms)?;
    }
    // (Its masks are kept as a removed camera's now; should this fail, the
    // same masks are only kept again at the next start.)
    let _ = db.ui_delete(SINCE_SCOPE, &known_key(&slug));
    db.set_mask_run(&slug, None);
    Ok(())
}

/// Keep `masks` for what was recorded under `slug` before `now_ms` (and a little after).
fn put_gone(db: &crate::db::Db, slug: &str, masks: &[ZoneRect], now_ms: i64) -> anyhow::Result<()> {
    let (key, v) = gone_entry(slug, masks, now_ms + SETTLE_MS);
    db.ui_put(SINCE_SCOPE, &key, &v)
}

/// The record (ui_kv key and value, scope [`SINCE_SCOPE`]) that keeps
/// `masks` for what was recorded under `slug` before `until_ms`.
fn gone_entry(slug: &str, masks: &[ZoneRect], until_ms: i64) -> (String, String) {
    let key = format!("{GONE_PREFIX}{slug}|{until_ms:013}|{:08x}", rand::random::<u32>());
    (key, serde_json::json!({ "masks": masks }).to_string())
}

/// How long after a rename what is found in the camera's old folder still
/// counts as recorded by it, beyond one segment length: the old recorder
/// may finish its file a little after, and the 15 s scan (or the next
/// start) indexes it later still.
pub const RENAME_TAIL_MS: i64 = 60_000;

/// Camera `before` (with masks) is being renamed at `now_ms`: the record
/// that keeps its masks under its old name's slug for what it recorded
/// there until it stopped (`segment_seconds` long files; see
/// [`RENAME_TAIL_MS`]). Its recordings move to the new name with it, but a
/// file its recorder had not finished, or that was not indexed yet, is
/// found later in the old folder and indexed under the old name, which a
/// camera added under that name would then show as its own. None: no masks.
/// (Written in the rename's transaction, after the masks of cameras removed
/// under the old name have moved: see Db::rename_camera_keeping.)
pub fn rename_tail_entry(before: &CameraConfig, segment_seconds: u64, now_ms: i64) -> Option<(String, String)> {
    if !crate::privacy::has_masks(before) {
        return None;
    }
    let until = now_ms + segment_seconds as i64 * 1000 + RENAME_TAIL_MS;
    Some(gone_entry(&crate::config::slug(&before.name), &before.privacy_masks, until))
}

/// A recording that another thread was still writing into the old folder of
/// a renamed camera (a card download, a recorder finishing) is done at
/// `now_ms`: the camera's `masks` are kept there again from now, as
/// [`rename_tail_entry`] keeps them from the rename, so the record covers it
/// for as long as the one made at the rename covers the rest.
pub fn keep_rename_tail(db: &crate::db::Db, slug: &str, masks: &[ZoneRect], now_ms: i64) -> anyhow::Result<()> {
    let (key, v) = gone_entry(slug, masks, now_ms + RENAME_TAIL_MS);
    db.ui_put(SINCE_SCOPE, &key, &v)
}

/// Whether a recording file from before `until_ms` is still on disk in the
/// folder of camera `slug` under any of these recording roots (indexed or
/// not: one not indexed yet is indexed later under that slug). A folder
/// that cannot be read counts as having one.
fn files_before(roots: &[std::path::PathBuf], slug: &str, until_ms: i64) -> bool {
    for root in roots {
        let dir = root.join("cameras").join(slug).join("segments");
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return true,
        };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { return true };
            if !m.is_file() {
                continue;
            }
            let ms = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64);
            // (A file's time is when it was last written: one begun before
            // then may have been written a while after; an hour covers the
            // longest recording file. Kept a little long only keeps a record.)
            if ms.is_none_or(|ms| ms < until_ms + 3_600_000) {
                return true;
            }
        }
    }
    false
}

/// The masks of removed cameras kept under this slug: (until, masks), what
/// was recorded there before `until` may show what they cover.
pub fn gone_masks(db: &crate::db::Db, slug: &str) -> anyhow::Result<Vec<(i64, Vec<ZoneRect>)>> {
    let mut out = Vec::new();
    for (k, v) in db.ui_scope_prefix(SINCE_SCOPE, &format!("{GONE_PREFIX}{slug}|"))? {
        let Some((_, until)) = parse_gone_key(&k) else { anyhow::bail!("a damaged record: {k}") };
        let v: serde_json::Value = serde_json::from_str(&v)?;
        let masks: Vec<ZoneRect> = serde_json::from_value(v["masks"].clone())?;
        out.push((until, masks));
    }
    Ok(out)
}

/// The URL of the stand-in settings [`gone_cfg`] gives when the masks of a
/// camera that is not set up cannot be read: the whole picture is masked,
/// and a picture masked so is not written back over the stored one.
pub const UNKNOWN_URL: &str = "masks-unknown:";

/// Stand-in settings for the pictures of `camera`, a camera no longer set
/// up: the masks of the removed cameras kept under its name's slug (None:
/// none kept, nothing to mask). When they cannot be read: the whole picture
/// masked ([`UNKNOWN_URL`]).
pub fn gone_cfg(db: &crate::db::Db, camera: &str) -> Option<CameraConfig> {
    let mut c: CameraConfig = toml::from_str("name = \"x\"\nurl = \"gone:\"\nmask_mode = \"screen\"").ok()?;
    c.name = camera.to_string();
    match gone_masks(db, &crate::config::slug(camera)) {
        Ok(v) if v.is_empty() => return None,
        Ok(v) => c.privacy_masks = v.into_iter().flat_map(|(_, m)| m).collect(),
        Err(e) => {
            tracing::warn!("camera '{camera}': the privacy masks it had could not be read ({e:#}); its pictures are blacked out");
            c.url = UNKNOWN_URL.into();
            c.privacy_masks = vec![ZoneRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }];
        }
    }
    Some(c)
}

/// The settings for pictures made from the recordings of `camera` from
/// `from_ms` on (a still, a preview, a filmstrip, a time-lapse, an event's
/// pictures): `set_up`, the camera set up under the name now (None: none,
/// see [`gone_cfg`]), with the masks of the cameras removed under its
/// name's slug that apply to footage from then (recorded before their
/// removal) added, so a camera added again under a removed one's name
/// never shows the old footage unmasked. Masks of removed cameras that
/// cannot be read: the whole picture masked ([`UNKNOWN_URL`]).
pub fn pictures_cfg(db: &crate::db::Db, set_up: Option<CameraConfig>, camera: &str, from_ms: i64) -> Option<CameraConfig> {
    let Some(mut c) = set_up else { return gone_cfg(db, camera) };
    match gone_masks(db, &crate::config::slug(camera)) {
        Ok(v) => {
            let old: Vec<ZoneRect> = v.into_iter().filter(|(until, _)| from_ms < *until).flat_map(|(_, m)| m).collect();
            if !old.is_empty() {
                if !crate::privacy::has_masks(&c) {
                    // (Hidden in its pictures; what it records is untouched.)
                    c.mask_mode = Some("screen".into());
                }
                c.privacy_masks.extend(old);
            }
        }
        Err(e) => {
            tracing::warn!("camera '{camera}': the privacy masks of a camera removed under its name could not be read ({e:#}); its pictures are blacked out");
            c.url = UNKNOWN_URL.into();
            c.privacy_masks = vec![ZoneRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }];
        }
    }
    Some(c)
}

/// The masks that apply to a camera's recordings cannot be told (the masks
/// of cameras removed under its name could not be read): nothing that
/// needs them is sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasksUnknown;

/// 503: the masks to burn in could not be read (never the video as it is).
pub fn unknown_response() -> Response {
    busy("the privacy masks it needs could not be read")
}

/// The masks to burn into this camera's recordings from `from_ms` to
/// `to_ms` (epoch ms) when they leave the server without a player drawing
/// them. None only when the camera has no masks, or every recording there
/// already has its current masks in the pictures (at the recording level
/// since before the first one began, see [`masked_since`]). Everything else
/// counts as unmasked: recordings made at the screen level, before a switch
/// to the recording level or a mask change, and whatever cannot be told.
/// (The recordings looked at are the segments there; a saved event clip
/// file is not one: see `file_start_ms` in [`clip_needs_mask`].) The masks
/// of cameras removed under its name are not looked at: see [`footage_mask`].
#[cfg(test)]
pub fn file_needs_mask(db: &crate::db::Db, cfg: &CameraConfig, from_ms: i64, to_ms: i64) -> Option<String> {
    clip_needs_mask(db, cfg, None, from_ms, to_ms)
}

/// [`file_needs_mask`], also for a saved clip file that began at
/// `file_start_ms` (an event's clip): it counts as masked only when it
/// began after the camera's recordings carried these masks.
#[cfg(test)]
pub fn clip_needs_mask(db: &crate::db::Db, cfg: &CameraConfig, file_start_ms: Option<i64>, from_ms: i64, to_ms: i64) -> Option<String> {
    current_needs(db, cfg, file_start_ms, from_ms, to_ms).then(|| mask_filter(&cfg.privacy_masks))
}

/// A recording counts as having the camera's masks in its pictures only
/// when its row says the recorder that made it burned these masks in
/// (`segments.mask_rev`, written only for what this run's masking recorder
/// made: see Db::set_mask_run). A row added any other way (found by the
/// start-up indexing or a folder sweep, from before an upgrade, from a
/// restored older database, recorded natively) has none: unmasked. A saved
/// clip file also has to have begun after the "masked from" moment.
fn current_needs(db: &crate::db::Db, cfg: &CameraConfig, file_start_ms: Option<i64>, from_ms: i64, to_ms: i64) -> bool {
    if !crate::privacy::has_masks(cfg) {
        return false;
    }
    let Some(since) = masked_since(db, cfg) else { return true };
    if file_start_ms.is_some_and(|s| s < since) {
        return true;
    }
    let rev = crate::privacy::masks_revision(cfg);
    match db.segment_mask_revs(&crate::config::slug(&cfg.name), from_ms, to_ms.max(from_ms + 1)) {
        Ok(rows) => !rows.iter().all(|r| r.as_deref() == Some(rev.as_str())),
        Err(_) => true,
    }
}

/// The masks to burn into the recordings kept under `slug` from `from_ms`
/// to `to_ms` (and a saved clip file that began at `file_start_ms`):
/// those of the camera set up under it now, `cfg`, unless the player draws
/// them (`draws`; see [`clip_needs_mask`]), and always those of cameras
/// removed under it that apply to recordings from before their removal (no
/// player knows them). Blocking (the database).
pub fn footage_mask(db: &crate::db::Db, cfg: Option<&CameraConfig>, slug: &str, file_start_ms: Option<i64>, from_ms: i64, to_ms: i64, draws: bool) -> Result<Option<String>, MasksUnknown> {
    let gone = gone_masks(db, slug).map_err(|e| {
        tracing::warn!("recordings of '{slug}': the privacy masks of the cameras removed under that name could not be read: {e:#}");
        MasksUnknown
    })?;
    let mut rects: Vec<ZoneRect> = Vec::new();
    if let Some(c) = cfg.filter(|_| !draws) {
        if current_needs(db, c, file_start_ms, from_ms, to_ms) {
            rects.extend(c.privacy_masks.iter().cloned());
        }
    }
    if !gone.is_empty() {
        // The earliest recording there (when that cannot be told, every one applies).
        let first = match db.segments_overlapping(slug, from_ms, to_ms.max(from_ms + 1)) {
            Ok(rows) => rows.iter().map(|r| r.start_ms).chain(file_start_ms).min(),
            Err(_) => None,
        };
        for (until, m) in gone {
            if first.is_none_or(|f| f < until) {
                rects.extend(m);
            }
        }
    }
    let f = mask_filter(&rects);
    Ok((!f.is_empty()).then_some(f))
}

/// The `draws` query parameter (a comma separated list of what the client
/// draws over the video itself): true when it lists `masks`.
pub fn draws_masks(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.split(',').any(|x| x.trim().eq_ignore_ascii_case("masks")))
}

/// The settings of the camera called `camera` (its name, or its folder name):
/// the running ones, else the saved ones. None: no such camera any more.
pub fn camera_cfg(engine: &Engine, camera: &str) -> Option<CameraConfig> {
    let slug = crate::config::slug(camera);
    for h in engine.cameras.read().unwrap().iter().flatten() {
        let c = h.cfg.read().unwrap();
        if c.name == camera || crate::config::slug(&c.name) == slug {
            return Some(c.clone());
        }
    }
    engine.config.lock().unwrap().cameras.iter().find(|c| c.name == camera || crate::config::slug(&c.name) == slug).cloned()
}

/// The masks to burn into this camera's recordings from `from_ms` to
/// `to_ms` watched by a client: Some when they may show what the masks
/// cover (see [`footage_mask`]) and the client does not draw them (`draws`).
/// Blocking (the database).
pub fn view_mask(engine: &Engine, camera: &str, draws: bool, from_ms: i64, to_ms: i64) -> Result<Option<String>, MasksUnknown> {
    let cfg = camera_cfg(engine, camera);
    footage_mask(&engine.db, cfg.as_ref(), &crate::config::slug(camera), None, from_ms, to_ms, draws)
}

/// The stretch an event's clip covers (epoch ms): where its saved file
/// really starts and ends (see [`Engine::clip_span_ms`]), else about its
/// start less the pre-roll to its end plus the post-roll.
pub fn event_span(engine: &Engine, ev: &crate::engine::Event) -> (i64, i64) {
    engine.clip_span_ms(ev)
}

/// The masks to burn into a download of a saved clip file of `camera` that began at
/// `file_start_ms` and covers `from_ms` to `to_ms` (see [`clip_needs_mask`]).
pub fn clip_download_mask(engine: &Engine, camera: &str, file_start_ms: i64, from_ms: i64, to_ms: i64) -> Result<Option<String>, MasksUnknown> {
    clip_view_mask(engine, camera, false, file_start_ms, from_ms, to_ms)
}

/// [`view_mask`] for a saved clip file (see [`clip_download_mask`]).
pub fn clip_view_mask(engine: &Engine, camera: &str, draws: bool, file_start_ms: i64, from_ms: i64, to_ms: i64) -> Result<Option<String>, MasksUnknown> {
    let cfg = camera_cfg(engine, camera);
    footage_mask(&engine.db, cfg.as_ref(), &crate::config::slug(camera), Some(file_start_ms), from_ms, to_ms, draws)
}

/// The masks to burn into a download or an export of an event's clip (its saved file, or the recordings).
pub fn event_download_mask(engine: &Engine, ev: &crate::engine::Event) -> Result<Option<String>, MasksUnknown> {
    let (a, b) = event_span(engine, ev);
    clip_download_mask(engine, &ev.camera, a, a, b)
}

/// The masks as ffmpeg drawbox filters: solid black, each edge moved
/// outward to an even pixel (so chroma subsampling cannot leave a lighter
/// line of the hidden area at its border). Relative rectangles, so they
/// fit any picture size.
pub fn mask_filter(masks: &[ZoneRect]) -> String {
    // (One geometry for every masked picture and video: crate::privacy.)
    crate::privacy::mask_filter(masks)
}

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

/// 503 + Retry-After: the masks cannot be burned in right now, but can soon
/// (never the video without them).
pub fn busy(why: &str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "10".to_string()), (header::CACHE_CONTROL, "no-store".to_string())], format!("{NEEDS_BURN}, and {why}. Try again shortly, or use a player that draws the masks itself (the web page does).")).into_response()
}

const NEEDS_BURN: &str = "This recording may show what the camera's privacy masks cover, so the server has to burn them into this video for this player";

/// 409: the masks can never be burned in on this server as it is set up
/// (video conversion turned off). Not "try again": asking again changes nothing.
pub fn conversion_off() -> Response {
    (StatusCode::CONFLICT, [(header::CACHE_CONTROL, "no-store")], format!("{NEEDS_BURN}, and video conversion is turned off on this server (Settings > Video conversion). Use a player that draws the masks itself (the web page does), or ask the admin to turn conversion on.")).into_response()
}

/// 403: this account's videos are never converted (the demo account), so a
/// video that needs its masks burned in is not sent to it.
pub fn never_converted() -> Response {
    (StatusCode::FORBIDDEN, [(header::CACHE_CONTROL, "no-store")], format!("{NEEDS_BURN}, and this account's videos are never converted. Use a player that draws the masks itself (the web page does).")).into_response()
}

/// The answer when the masks cannot be burned in for `user` (None: no
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

/// One conversion that burns the masks (and maybe a caption and subtitles)
/// into a recording, streamed as fragmented MP4.
pub struct Burn {
    pub input: Input,
    pub desc: mp4::VideoDesc,
    pub camera: String,
    pub mask: Option<String>,
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
        mask: b.mask,
        subs: b.subs,
        keep_times: b.keep_times,
    }
}

/// Stream `b` (converted as it is sent: no byte ranges) with these headers
/// besides the usual ones.
pub async fn stream(b: Burn, user: &str, headers: Vec<(String, String)>) -> Response {
    let target = forced_target(&b.desc);
    let masked = b.mask.is_some();
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
    if masked {
        r = r.header("x-as-masked", "1");
    }
    for (k, v) in headers {
        r = r.header(k, v);
    }
    r.body(super::file::stream(job)).unwrap()
}

/// Who waits for an encoder in [`burn_to_file`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Priority {
    /// An export someone is waiting for: the first free place.
    Export,
    /// An event copy to a folder (in the background): one at a time, and a
    /// place only while another stays free for people watching (when the
    /// encoders have more than one; or, after waiting a minute, any place).
    Background,
}

/// Event copies converting now (see `Priority::Background`).
static BACKGROUND: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct BackgroundPlace;

impl Drop for BackgroundPlace {
    fn drop(&mut self) {
        BACKGROUND.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn background_place() -> Option<BackgroundPlace> {
    use std::sync::atomic::Ordering;
    BACKGROUND.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).ok().map(|_| BackgroundPlace)
}

/// The most a burned file may grow to (it is written to the data folder).
pub const BURN_MAX_BYTES: u64 = 4 << 30;

/// A converted file in the data folder's tmp folder, removed when dropped.
pub struct Burned {
    path: std::path::PathBuf,
    len: u64,
}

impl Burned {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Its size in bytes.
    pub fn size(&self) -> u64 {
        self.len
    }

    /// The whole file.
    #[cfg(test)]
    pub fn read(&self) -> std::io::Result<Vec<u8>> {
        std::fs::read(&self.path)
    }
}

impl Drop for Burned {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Why [`burn_to_file`] made nothing when the only reason was waiting:
/// every encoder (or the one place for event copies) stayed busy. Not a
/// failure of the masks: worth trying again later.
#[derive(Debug)]
pub struct EncodersBusy(pub &'static str);

impl std::fmt::Display for EncodersBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for EncodersBusy {}

/// [`burn_to_file`] was stopped by its `cancelled` (the export's download
/// was stopped).
#[derive(Debug)]
pub struct BurnCancelled;

impl std::fmt::Display for BurnCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for BurnCancelled {}

/// [`burn_to_file`]'s conversion grew larger than it was allowed to.
#[derive(Debug)]
pub struct BurnTooLarge;

impl std::fmt::Display for BurnTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the conversion grew too large")
    }
}

impl std::error::Error for BurnTooLarge {}

/// How long [`burn_to_file`] waits for an encoder's place (once it may
/// take one: an event copy first waits for the one place for event copies).
const PLACE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
/// How long an event copy waits for the place for event copies (another
/// copy is converting) before it is tried again later.
const BACKGROUND_WAIT: std::time::Duration = std::time::Duration::from_secs(900);
/// How long an event copy leaves an encoder's place to people watching
/// (while too few are free) before it takes any free one.
const YIELD_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// How many places an event copy leaves free for people watching before it
/// takes one: one, when the encoders have two places or more (none when
/// they have only one: else every copy would wait).
fn background_needs_free(capacity: usize) -> usize {
    capacity.clamp(1, 2)
}

/// Whether an event copy that holds the place for event copies may take an
/// encoder's place now: enough stay free for people watching, or it has
/// waited `waited` (since it got the place for event copies) a minute.
fn background_takes(free: usize, capacity: usize, waited: std::time::Duration) -> bool {
    free >= background_needs_free(capacity) || waited >= YIELD_WAIT
}

/// Why [`burn_to_file`] stops waiting ([`EncodersBusy`]), if it does: `all`
/// is how long it has waited in all, `for_place` how long since it may take
/// an encoder's place (None: an event copy still waiting for the place for
/// event copies; that wait does not count against the encoder's).
fn wait_over(all: std::time::Duration, for_place: Option<std::time::Duration>) -> Option<&'static str> {
    match for_place {
        None if all > BACKGROUND_WAIT => Some("another event copy was converting"),
        None => None,
        Some(w) if w > PLACE_WAIT => Some("the video encoders were busy"),
        Some(_) => None,
    }
}

/// Run `b` to its end into a file in the data folder's tmp folder (an
/// export, an event copy; called on a blocking thread, inside the runtime or
/// not; never the whole video in memory), at most `max_bytes` long. Waits up
/// to two minutes for an encoder's place (an event copy: once the place for
/// event copies is its own), then at most five minutes (or three times the
/// clip's length) for the conversion. `cancelled` is asked as it waits and
/// converts: true stops the conversion ([`BurnCancelled`]). An error that
/// is only waiting is [`EncodersBusy`].
pub fn burn_to_file(engine: &Engine, b: Burn, prio: Priority, cancelled: &dyn Fn() -> bool, max_bytes: u64) -> anyhow::Result<Burned> {
    use std::time::{Duration, Instant};
    let limit = Duration::from_secs(300).max(Duration::from_millis(b.media_ms.unwrap_or(0).max(0) as u64 * 3));
    let max_bytes = max_bytes.min(BURN_MAX_BYTES);
    let target = forced_target(&b.desc);
    let codec = target.codec;
    let t0 = Instant::now();
    let nap = |ms: u64| -> anyhow::Result<()> {
        if cancelled() {
            return Err(BurnCancelled.into());
        }
        std::thread::sleep(Duration::from_millis(ms));
        Ok(())
    };
    let encoders = loop {
        match super::encoders_for(codec) {
            Some(v) if v.is_empty() => anyhow::bail!("video conversion is off, so the privacy masks cannot be burned in"),
            Some(v) => break v,
            None if t0.elapsed() > Duration::from_secs(60) => return Err(EncodersBusy("the video encoders were not ready").into()),
            None => nap(250)?,
        }
    };
    let capacity = super::capacity(&encoders);
    let mut bg = None;
    // (From when it may take a place.)
    let mut since = (prio == Priority::Export).then_some(t0);
    let job = loop {
        if prio == Priority::Background && bg.is_none() {
            bg = background_place();
            if bg.is_some() {
                since = Some(Instant::now());
            }
        }
        if let Some(why) = wait_over(t0.elapsed(), since.map(|s| s.elapsed())) {
            return Err(EncodersBusy(why).into());
        }
        let Some(waited) = since.map(|s| s.elapsed()) else {
            nap(500)?;
            continue;
        };
        // (An event copy yields to people watching: a place only while
        // enough stay free, until it has waited a minute.)
        if prio == Priority::Background && !background_takes(super::free_places(&encoders), capacity, waited) {
            nap(500)?;
            continue;
        }
        // (No account limit or download place: two exports at a time at most.)
        match super::reserve(&encoders) {
            Some((enc, slot)) => break make_job(Burn { download: false, ..b }, target, &encoders, enc, slot, None, None),
            None => nap(500)?,
        }
    };
    let dir = engine.data_dir.join("tmp");
    std::fs::create_dir_all(&dir)?;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut out = Burned { path: dir.join(format!("burn-{}-{}.mp4", std::process::id(), N.fetch_add(1, Ordering::Relaxed))), len: 0 };
    let mut file = std::fs::File::create(&out.path)?;
    let body = super::file::stream(job);
    // (Dropping the body when the time is up, or when cancelled, stops the conversion.)
    let write = async move {
        use http_body_util::BodyExt;
        use std::io::Write;
        let mut body = body;
        let mut len = 0u64;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| anyhow::anyhow!("the conversion stopped: {e}"))?;
            if let Ok(data) = frame.into_data() {
                len += data.len() as u64;
                if len > max_bytes {
                    return Err(BurnTooLarge.into());
                }
                file.write_all(&data)?;
            }
        }
        file.flush()?;
        Ok::<u64, anyhow::Error>(len)
    };
    let watch = async {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if cancelled() {
                return;
            }
        }
    };
    let timed = async move {
        tokio::select! {
            r = tokio::time::timeout(limit, write) => r.map_err(|_| anyhow::anyhow!("the conversion took too long"))?,
            _ = watch => Err(BurnCancelled.into()),
        }
    };
    let got = match tokio::runtime::Handle::try_current() {
        Ok(h) => h.block_on(timed),
        // (An event copy's own thread: a small runtime for the wait.)
        Err(_) => tokio::runtime::Builder::new_current_thread().enable_time().build()?.block_on(timed),
    };
    let len = got?;
    if len == 0 {
        anyhow::bail!("the conversion made nothing");
    }
    out.len = len;
    Ok(out)
}

/// The description of an assembled clip (its first fragments).
pub fn probe_clip(c: &crate::ingest::splice::VirtualClip) -> Option<mp4::VideoDesc> {
    mp4::probe_with(c.len(), &|a, b| c.read_range(a, b).ok())
}

/// An event clip for a download or an export: the masks burned in, the time
/// subtitles and (`text`) the camera's text. None: no recording. Blocking.
pub fn event_burn(engine: &Arc<Engine>, ev: &crate::engine::Event, mask: Option<String>, text: Option<&CameraConfig>) -> anyhow::Result<Option<Burn>> {
    let path = engine.clip_file_path(&ev.id);
    let (input, desc, start_ms) = if path.is_file() {
        let Some(desc) = mp4::probe_file(&path)? else { return Ok(None) };
        // Where the file really starts: the clock burned into it matches.
        (Input::File(path), desc, engine.clip_span_ms(ev).0)
    } else {
        let Some(clip) = engine.virtual_clip(ev)? else { return Ok(None) };
        let Some(desc) = probe_clip(&clip) else { return Ok(None) };
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
    Ok(Some(Burn { input, desc, camera: ev.camera.clone(), mask, caption, subs, seek_ms: 0, limit_ms: None, media_ms, keep_times: false, what: "download", download: false }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(extra: &str) -> CameraConfig {
        toml::from_str(&format!("name = \"Porch\"\nurl = \"rtsp://192.0.2.10/s\"\n{extra}")).unwrap()
    }

    #[test]
    fn the_video_is_unmasked_unless_the_masks_are_burned_into_it() {
        let m = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\n";
        assert!(!sends_unmasked(&cam("")));
        assert!(!sends_unmasked(&cam("mask_mode = \"screen\"")), "no masks: nothing to hide");
        assert!(sends_unmasked(&cam(&format!("{m}mask_mode = \"screen\""))));
        assert!(!sends_unmasked(&cam(&format!("{m}mask_mode = \"recording\""))), "burned in by the ffmpeg reader");
        assert!(!sends_unmasked(&cam(m)), "no mode set with masks: in the recordings (masks from before the setting)");
        assert!(!sends_unmasked(&cam("privacy_masks = [{ x = 0.1, y = 0.1, w = 0.0, h = 0.2 }]\nmask_mode = \"screen\"")), "a mask of no size hides nothing");
        // A pushed or battery camera: nothing burns them in, whatever the level says.
        for url in ["push://porch", "baichuan://192.0.2.9?uid=TESTUID000000001"] {
            let c: CameraConfig = toml::from_str(&format!("name = \"P\"\nurl = {url:?}\n{m}mask_mode = \"recording\"")).unwrap();
            assert!(sends_unmasked(&c), "{url}");
            assert!(crate::restream::http::masked(&c), "{url}: sharing waits for the admin's word");
        }
    }

    fn seg(start_ms: i64, end_ms: i64, name: &str) -> crate::db::SegmentRow {
        crate::db::SegmentRow { path: format!("rec/{name}.mp4"), start_ms, end_ms, bytes: 1, video_frames: 1, kind: "continuous".into(), folder: String::new() }
    }

    #[test]
    fn recordings_count_as_masked_only_from_when_the_level_had_these_masks() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let m = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\n";
        let screen = cam(&format!("{m}mask_mode = \"screen\""));
        let rec = cam(&format!("{m}mask_mode = \"recording\""));
        db.insert_segment("porch", &seg(1_000, 7_000, "a")).unwrap();
        // At the screen level everything is unmasked; nothing is noted.
        note_level(&db, &screen, 10_000, false);
        assert_eq!(masked_since(&db, &screen), None);
        assert!(file_needs_mask(&db, &screen, 1_000, 7_000).is_some());
        // At the recording level, but nothing noted yet (an older server): unmasked.
        assert_eq!(masked_since(&db, &rec), None);
        assert!(file_needs_mask(&db, &rec, 1_000, 7_000).is_some());
        // Switched to the recording level at 100 s: masked from then (and a little after).
        note_level(&db, &rec, 100_000, false);
        let since = 100_000 + SETTLE_MS;
        assert_eq!(masked_since(&db, &rec), Some(since));
        assert!(file_needs_mask(&db, &rec, 1_000, 7_000).is_some(), "made at the screen level");
        assert!(clip_needs_mask(&db, &rec, Some(since - 1), since - 1, since + 6_000).is_some(), "a saved clip that began before");
        // (Found by the folder scan, as the masking recorder's files are: its row says which masks it has.)
        db.insert_scanned_segment("porch", &seg(since + 1_000, since + 7_000, "b")).unwrap();
        assert_eq!(file_needs_mask(&db, &rec, since + 1_000, since + 7_000), None, "made masked: not converted again");
        assert_eq!(file_needs_mask(&db, &rec, since - 20_000, since + 7_000), None, "only what is recorded there counts");
        assert_eq!(clip_needs_mask(&db, &rec, Some(since + 1_000), since + 1_000, since + 7_000), None);
        // A segment the old reader began before the switch still counts.
        db.insert_segment("porch", &seg(since - 3_000, since + 500, "c")).unwrap();
        assert!(file_needs_mask(&db, &rec, since, since + 400).is_some());
        // Noted again with the same masks (a restart, another setting): unchanged.
        note_level(&db, &rec, 500_000, false);
        assert_eq!(masked_since(&db, &rec), Some(since));
        // Other masks: what is in the files is not what is hidden now.
        let moved = cam("privacy_masks = [{ x = 0.5, y = 0.5, w = 0.2, h = 0.2 }]\nmask_mode = \"recording\"");
        assert_eq!(masked_since(&db, &moved), None);
        assert!(file_needs_mask(&db, &moved, since + 1_000, since + 7_000).is_some());
        note_level(&db, &moved, 600_000, false);
        assert_eq!(masked_since(&db, &moved), Some(600_000 + SETTLE_MS));
        // The level back to screen and then recording again: from the second switch.
        note_level(&db, &screen, 700_000, false);
        assert_eq!(masked_since(&db, &rec), None, "the record is gone");
        note_level(&db, &rec, 800_000, false);
        assert_eq!(masked_since(&db, &rec), Some(800_000 + SETTLE_MS));
        assert_eq!(file_needs_mask(&db, &rec, since + 1_000, since + 7_000), None, "made with these masks burned in (its row says so), before the second switch");
        // A camera added again under the name: from now.
        note_level(&db, &rec, 900_000, true);
        assert_eq!(masked_since(&db, &rec), Some(900_000 + SETTLE_MS));
        // Without masks nothing is burned in.
        assert_eq!(file_needs_mask(&db, &cam(""), 0, 10), None);
        // A pushed camera set to "recording": never masked in its files.
        let pushed: CameraConfig = toml::from_str(&format!("name = \"Porch\"\nurl = \"push://porch\"\n{m}mask_mode = \"recording\"")).unwrap();
        note_level(&db, &pushed, 1_000_000, false);
        assert_eq!(masked_since(&db, &pushed), None);
        assert!(file_needs_mask(&db, &pushed, 2_000_000, 2_000_100).is_some());
    }

    #[test]
    fn applying_a_cameras_settings_notes_its_level_and_asks_again_to_share_it() {
        let e = Engine::for_test(toml::from_str("").unwrap());
        let h = e.add_test_camera("Porch");
        let m = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\n";
        let with = |extra: &str| -> CameraConfig { toml::from_str(&format!("name = \"Porch\"\nurl = \"rtsp://127.0.0.1:1/x\"\nenabled = false\nshare = true\nshare_unmasked = true\n{extra}")).unwrap() };
        // Sharing confirmed without masks; masks added: asked again.
        assert!(e.update_camera(h.id, with("")));
        assert!(e.camera(h.id).unwrap().cfg.read().unwrap().share_unmasked);
        let t0 = chrono::Utc::now().timestamp_millis();
        assert!(e.update_camera(h.id, with(&format!("{m}mask_mode = \"screen\""))));
        let c = e.camera(h.id).unwrap().cfg.read().unwrap().clone();
        assert!(!c.share_unmasked, "new masks: the confirmation is gone");
        assert_eq!(masked_since(&e.db, &c), None);
        // Confirmed again for these masks; the level changed: kept.
        let mut rec = with(&format!("{m}mask_mode = \"recording\""));
        assert!(e.update_camera(h.id, rec.clone()));
        let c = e.camera(h.id).unwrap().cfg.read().unwrap().clone();
        assert!(c.share_unmasked, "same masks");
        let since = masked_since(&e.db, &c).expect("noted at the recording level");
        assert!(since >= t0 + SETTLE_MS, "from the switch on");
        // Other settings changed: the same moment.
        rec.audio = true;
        assert!(e.update_camera(h.id, rec.clone()));
        assert_eq!(masked_since(&e.db, &e.camera(h.id).unwrap().cfg.read().unwrap()), Some(since));
        // Moved masks: asked again, and from now.
        let moved = with("privacy_masks = [{ x = 0.5, y = 0.5, w = 0.2, h = 0.2 }]\nmask_mode = \"recording\"");
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(e.update_camera(h.id, moved));
        let c = e.camera(h.id).unwrap().cfg.read().unwrap().clone();
        assert!(!c.share_unmasked);
        assert!(masked_since(&e.db, &c).is_some_and(|s| s > since));
        // Text burned in turned on: asked again.
        let before = with("");
        let mut after = with("");
        after.overlay = true;
        assert!(crate::restream::http::consent_lapses(&before, &after));
        assert!(!crate::restream::http::consent_lapses(&before, &with("")));
        let mut plain = with(&format!("{m}mask_mode = \"screen\""));
        plain.share_unmasked = false;
        assert!(!crate::restream::http::consent_lapses(&before, &plain), "nothing to clear");
        // A camera removed and added again under the name: from then.
        let added = e.add_camera_arc(with(&format!("{m}mask_mode = \"recording\"")));
        let c = e.camera(added).unwrap().cfg.read().unwrap().clone();
        assert!(masked_since(&e.db, &c).is_some());
    }

    fn named(name: &str, extra: &str) -> CameraConfig {
        toml::from_str(&format!("name = {name:?}\nurl = \"rtsp://192.0.2.10/s\"\n{extra}")).unwrap()
    }

    #[test]
    fn a_masked_from_record_holds_only_with_its_level_and_while_trusted() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let m = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\nmask_mode = \"recording\"\n";
        let rec = named("Trust Cam", m);
        // A record without its level (an earlier build): not taken; noted again.
        let key = format!("{SINCE_PREFIX}trust_cam");
        db.ui_put(SINCE_SCOPE, &key, &serde_json::json!({ "rev": crate::privacy::masks_revision(&rec), "since": 1 }).to_string()).unwrap();
        assert_eq!(masked_since(&db, &rec), None);
        note_level(&db, &rec, 100_000, false);
        assert_eq!(masked_since(&db, &rec), Some(100_000 + SETTLE_MS));
        let v: serde_json::Value = serde_json::from_str(&db.ui_get(SINCE_SCOPE, &key).unwrap().unwrap()).unwrap();
        assert_eq!(v["level"], "recording");
        // One that could not be removed when it had to be: not trusted, until written again.
        doubt("trust_cam", &"test");
        assert_eq!(masked_since(&db, &rec), None);
        note_level(&db, &rec, 200_000, false);
        assert_eq!(masked_since(&db, &rec), Some(200_000 + SETTLE_MS), "written fresh, trusted again");
        // At start: records of cameras not set up are dropped.
        db.ui_put(SINCE_SCOPE, &format!("{SINCE_PREFIX}old_name"), "{}").unwrap();
        prune_records(&db, &[rec.clone()], &[], 0);
        assert!(db.ui_get(SINCE_SCOPE, &format!("{SINCE_PREFIX}old_name")).unwrap().is_none());
        assert_eq!(masked_since(&db, &rec), Some(200_000 + SETTLE_MS));
        // Removed: gone.
        forget_record(&db, "Trust Cam");
        assert!(db.ui_get(SINCE_SCOPE, &key).unwrap().is_none());
    }

    #[test]
    fn a_rename_drops_the_records_of_both_names_and_moves_removed_cameras_masks() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let m = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\nmask_mode = \"recording\"\n";
        let (a, b) = (named("Ren A", m), named("Ren B", m));
        note_level(&db, &a, 1_000, false);
        note_level(&db, &b, 1_000, false);
        let gone = named("Ren A", "privacy_masks = [{ x = 0.5, y = 0.5, w = 0.1, h = 0.1 }]\n");
        remember_removed(&db, &gone, 500).unwrap();
        db.rename_camera("Ren A", "Ren B").unwrap();
        assert_eq!(masked_since(&db, &a), None);
        assert_eq!(masked_since(&db, &b), None, "the new name's record is gone too");
        assert!(gone_masks(&db, "ren_a").unwrap().is_empty());
        assert_eq!(gone_masks(&db, "ren_b").unwrap(), vec![(500 + SETTLE_MS, gone.privacy_masks.clone())], "with the recordings");
    }

    #[test]
    fn a_removed_cameras_masks_stay_with_its_recordings() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let c = named("Gone Cam", "privacy_masks = [{ x = 0.0, y = 0.0, w = 0.5, h = 0.5 }]\nmask_mode = \"screen\"\n");
        // Without masks nothing is kept.
        remember_removed(&db, &named("Plain Cam", ""), 1_000).unwrap();
        assert!(db.ui_scope_prefix(SINCE_SCOPE, GONE_PREFIX).unwrap().is_empty());
        remember_removed(&db, &c, 1_000_000).unwrap();
        db.insert_segment("gone_cam", &seg(500_000, 506_000, "g1")).unwrap();
        db.insert_segment("gone_cam", &seg(2_000_000, 2_006_000, "g2")).unwrap();
        let f = footage_mask(&db, None, "gone_cam", None, 500_000, 506_000, true).unwrap();
        assert_eq!(f, Some(mask_filter(&c.privacy_masks)), "a player cannot draw them: always burned in");
        assert_eq!(footage_mask(&db, None, "gone_cam", None, 2_000_000, 2_006_000, false).unwrap(), None, "recorded after it was removed");
        // A camera set up under the name later (no masks): what was recorded before still has them.
        let new = named("Gone Cam", "");
        assert!(footage_mask(&db, Some(&new), "gone_cam", None, 500_000, 506_000, false).unwrap().is_some());
        // A saved clip that began before.
        assert!(footage_mask(&db, Some(&new), "gone_cam", Some(900_000), 2_000_000, 2_006_000, false).unwrap().is_some());
        // ... with masks of its own: both.
        let both = named("Gone Cam", "privacy_masks = [{ x = 0.6, y = 0.6, w = 0.2, h = 0.2 }]\nmask_mode = \"screen\"\n");
        let f = footage_mask(&db, Some(&both), "gone_cam", None, 500_000, 506_000, false).unwrap().unwrap();
        assert_eq!(f.matches("drawbox").count(), 2);
        assert_eq!(footage_mask(&db, Some(&both), "gone_cam", None, 2_000_000, 2_006_000, true).unwrap(), None, "drawn by the player");
        // Its pictures.
        assert_eq!(gone_cfg(&db, "Gone Cam").unwrap().privacy_masks, c.privacy_masks);
        assert!(gone_cfg(&db, "Other Cam").is_none());
        // Kept while anything from before is; dropped at start after.
        prune_records(&db, &[], &[], 0);
        assert_eq!(gone_masks(&db, "gone_cam").unwrap().len(), 1);
        remember_removed(&db, &named("Empty Cam", "privacy_masks = [{ x = 0.0, y = 0.0, w = 0.5, h = 0.5 }]\n"), 3_000_000).unwrap();
        let ev = crate::engine::Event { id: "empty-1".into(), camera_id: 0, camera: "Empty Cam".into(), start: 1_000, end: 1_001, score: 1.0, clip: None, thumb: None, objects: vec![], protect: false };
        db.insert_event(&ev).unwrap();
        prune_records(&db, &[], &[], 0);
        assert_eq!(gone_masks(&db, "empty_cam").unwrap().len(), 1, "an event from before");
        db.delete_events(&["empty-1".to_string()]).unwrap();
        prune_records(&db, &[], &[], 0);
        assert!(gone_masks(&db, "empty_cam").unwrap().is_empty());
        // A damaged record: unknown, never "no masks".
        db.ui_put(SINCE_SCOPE, &format!("{GONE_PREFIX}bad_cam|0000000001000|00000000"), "not json").unwrap();
        assert_eq!(footage_mask(&db, None, "bad_cam", None, 0, 10, true), Err(MasksUnknown));
        let u = gone_cfg(&db, "Bad Cam").unwrap();
        assert_eq!((u.url.as_str(), u.privacy_masks.len()), (UNKNOWN_URL, 1));
    }

    #[test]
    fn event_copies_wait_for_a_free_place_only_when_there_is_another() {
        use std::time::Duration;
        // Software only (one place): take it when it is free, at once.
        assert!(background_takes(1, 1, Duration::ZERO));
        assert!(!background_takes(0, 1, Duration::ZERO));
        // More places: leave one free for people watching, for a minute.
        assert!(!background_takes(1, 2, Duration::from_secs(10)));
        assert!(background_takes(2, 3, Duration::ZERO));
        assert!(background_takes(1, 4, YIELD_WAIT));
        // Waiting for the place for event copies does not count against the encoders' two minutes.
        assert_eq!(wait_over(Duration::from_secs(600), None), None);
        assert!(wait_over(BACKGROUND_WAIT + Duration::from_secs(1), None).is_some());
        assert_eq!(wait_over(Duration::from_secs(600), Some(Duration::from_secs(30))), None);
        assert!(wait_over(Duration::from_secs(130), Some(PLACE_WAIT + Duration::from_secs(1))).is_some());
        // Only waiting: told apart from a failure (an event copy tries again later).
        let e: anyhow::Error = EncodersBusy("busy").into();
        assert!(e.downcast_ref::<EncodersBusy>().is_some());
    }

    #[test]
    fn a_sharing_confirmation_is_renewed_only_when_it_held() {
        let m = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\nmask_mode = \"screen\"\n";
        let mut given = named("Share Cam", &format!("{m}share_unmasked = true\nshare_unmasked_rev = \"forged\"\n"));
        crate::restream::http::settle_consent(None, &mut given);
        assert!(crate::restream::http::consent_holds(&given), "given now: for these settings (not what a client sent)");
        let mut stale = given.clone();
        stale.share_unmasked_rev = Some("old".into());
        let mut next = stale.clone();
        next.audio = true;
        crate::restream::http::settle_consent(Some(&stale), &mut next);
        assert!(!crate::restream::http::consent_holds(&next), "an unrelated change does not renew it");
        let mut off = given.clone();
        off.share_unmasked = false;
        crate::restream::http::settle_consent(Some(&given), &mut off);
        assert_eq!(off.share_unmasked_rev, None);
    }

    #[test]
    fn switching_to_the_recording_level_never_keeps_a_record_from_before() {
        let e = Engine::for_test(toml::from_str("").unwrap());
        let h = e.add_test_camera("Stale Cam");
        let m = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\n";
        let with = |extra: &str| -> CameraConfig { toml::from_str(&format!("name = \"Stale Cam\"\nurl = \"rtsp://127.0.0.1:1/x\"\nenabled = false\n{extra}")).unwrap() };
        assert!(e.update_camera(h.id, with(&format!("{m}mask_mode = \"screen\""))));
        // A record left from earlier at the recording level (its removal failed, say).
        let rec = with(&format!("{m}mask_mode = \"recording\""));
        let v = serde_json::json!({ "rev": crate::privacy::masks_revision(&rec), "level": "recording", "since": 1 });
        e.db.ui_put(SINCE_SCOPE, &format!("{SINCE_PREFIX}stale_cam"), &v.to_string()).unwrap();
        let t = chrono::Utc::now().timestamp_millis();
        assert!(e.update_camera(h.id, rec.clone()));
        assert!(masked_since(&e.db, &rec).is_some_and(|s| s >= t + SETTLE_MS), "from the switch, not the old record");
        // Then a change that keeps the level: the same moment.
        let since = masked_since(&e.db, &rec).unwrap();
        let mut again = rec.clone();
        again.audio = true;
        assert!(e.update_camera(h.id, again.clone()));
        assert_eq!(masked_since(&e.db, &again), Some(since));
        // Removed: its record goes with it.
        assert!(e.remove_camera(h.id));
        assert!(e.db.ui_get(SINCE_SCOPE, &format!("{SINCE_PREFIX}stale_cam")).unwrap().is_none());
        assert_eq!(gone_masks(&e.db, "stale_cam").unwrap().len(), 1, "its masks stay with its recordings");
    }

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
    fn draws_masks_reads_the_list() {
        assert!(draws_masks(Some("masks")));
        assert!(draws_masks(Some("text,masks")));
        assert!(draws_masks(Some(" MASKS ")));
        assert!(!draws_masks(Some("text")));
        assert!(!draws_masks(Some("")));
        assert!(!draws_masks(None));
        assert!(!draws_masks(Some("masksx")));
    }

    #[test]
    fn mask_boxes_use_the_shared_geometry() {
        let masks = [ZoneRect { x: 0.1, y: 0.25, w: 0.3, h: 0.5 }, ZoneRect { x: 0.5, y: 0.5, w: 0.0, h: 0.1 }];
        assert_eq!(mask_filter(&masks), crate::privacy::mask_filter(&masks));
        assert_eq!(mask_filter(&masks).matches("drawbox").count(), 1, "an empty rectangle is left out");
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
    fn the_masks_survive_a_caption_and_go_before_it() {
        use crate::transcode::{Encoder, Hw};
        let t = Target { codec: OutCodec::H264, width: 640, height: 360, fps: None, bitrate_kbps: 1000 };
        let mask = mask_filter(&[ZoneRect { x: 0.0, y: 0.0, w: 0.5, h: 0.5 }]);
        let subs = std::path::Path::new("/tmp/x.srt");
        let qsv = Encoder { hw: Some(Hw::Qsv), device: None };
        let x = crate::transcode::file::Extra { mask: Some(&mask), subs: Some(subs), keep_times: true };
        let a = crate::transcode::file::args_with(&qsv, true, &t, 25.0, 8, None, 1500, Some(10_000), Some("drawtext=text='a'"), x);
        let at = |k: &str| a.iter().position(|v| v == k).map(|i| a[i + 1].clone());
        assert!(!a.iter().any(|v| v == "-hwaccel"), "decoded in software: {a:?}");
        let vf = at("-vf").unwrap();
        assert_eq!(vf, format!("trim=start=1.500,setpts=PTS-STARTPTS,scale=640:360:flags=bilinear,{mask},drawtext=text='a',format=nv12,hwupload=extra_hw_frames=16"));
        // From a pipe with subtitles: no -ss (it would drop the first ones), the sound trimmed too.
        assert_eq!(at("-ss"), None);
        assert_eq!(at("-af").as_deref(), Some("atrim=start=1.500,asetpts=PTS-STARTPTS"));
        assert_eq!(at("-c:s").as_deref(), Some("mov_text"));
        let pass = crate::ffmpeg_bin::passthrough_args();
        assert_eq!(at(pass[0]).as_deref(), Some(pass[1]));
        assert!(a.windows(2).any(|w| w[0] == "-i" && w[1] == "/tmp/x.srt"));
        // A file seeks before its input; the subtitles then start at the moment asked for.
        let a = crate::transcode::file::args_with(&Encoder::software(), false, &t, 25.0, 8, Some(std::path::Path::new("/c.mp4")), 1500, None, None, crate::transcode::file::Extra { mask: Some(&mask), subs: Some(subs), keep_times: false });
        let at = |k: &str| a.iter().position(|v| v == k).map(|i| a[i + 1].clone());
        assert_eq!(at("-ss").as_deref(), Some("1.500"));
        assert!(a.iter().position(|v| v == "-ss") < a.iter().position(|v| v == "/c.mp4"));
        assert_eq!(at("-vf").unwrap(), format!("scale=640:360:flags=bilinear,{mask},format=yuv420p"));
        assert_eq!(at("-c:a").as_deref(), Some("copy"));
        assert!(a.iter().any(|v| v == "-threads"), "threads capped: {a:?}");
    }
}

/// Through the whole stack: which video a camera masked on screen only
/// sends with and without `draws=masks`, next to one masked in its
/// recordings and one without masks.
#[cfg(test)]
mod http_tests {
    use crate::engine::Engine;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use base64::Engine as _;
    use std::sync::Arc;
    use tower::ServiceExt;

    const USERS: [&str; 12] = ["mk-a", "mk-b", "mk-c", "mk-d", "mk-e", "mk-f", "mk-g", "mk-h", "mk-i", "mk-j", "mk-k", "mk-l"];

    /// One conversion test at a time: the server converts two downloads at
    /// most, and these tests download.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Cameras "Screen" (top-left quarter masked on screen only), "Rec"
    /// (masked in the recordings) and "Open" (no masks); admin accounts.
    fn engine(dir: &std::path::Path) -> Arc<Engine> {
        let mut cfg: crate::config::Config = toml::from_str("").unwrap();
        for u in USERS {
            cfg.server.users.push(crate::config::UserConfig { username: u.into(), password: crate::auth::hash_password(&format!("pw-{u}-1")), admin: true, cameras: vec![], disabled: false, role: String::new(), perms: Default::default() });
        }
        let mut e = Engine::for_test(cfg);
        Arc::get_mut(&mut e).unwrap().data_dir = dir.to_path_buf();
        for (name, extra) in [
            ("Screen", "mask_mode = \"screen\"\nprivacy_masks = [{ x = 0.0, y = 0.0, w = 0.5, h = 0.5 }]\n"),
            ("Rec", "mask_mode = \"recording\"\nprivacy_masks = [{ x = 0.0, y = 0.0, w = 0.5, h = 0.5 }]\n"),
            ("Open", ""),
        ] {
            let h = e.add_test_camera(name);
            let c: crate::config::CameraConfig = toml::from_str(&format!("name = {name:?}\nurl = \"rtsp://127.0.0.1/x\"\n{extra}")).unwrap();
            *h.cfg.write().unwrap() = c.clone();
            let mut cfg = e.config.lock().unwrap();
            let i = cfg.cameras.iter().position(|x| x.name == name).unwrap();
            cfg.cameras[i] = c;
        }
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

    fn video_track() -> crate::ingest::fmp4::TrackInfo {
        let mut entry = vec![0, 0, 0, 16];
        entry.extend_from_slice(b"avc1");
        entry.extend_from_slice(&[0; 8]);
        crate::ingest::fmp4::TrackInfo { kind: crate::ingest::fmp4::TrackKind::Video, timescale: 90000, sample_entry: entry, width: 640, height: 480 }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_video_goes_only_to_players_that_draw_the_masks() {
        let dir = std::env::temp_dir().join(format!("amber-masked-live-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let e = engine(&dir);
        // Every camera has a live stream (as on the built-in reader).
        let _muxers: Vec<_> = (0..3).map(|id| crate::ingest::live::LiveMuxer::start(e.camera(id).unwrap().live_main.clone(), &[video_track()], &["avc1.640033".into()])).collect();
        let app = crate::web::build_router(e.clone());
        let info = |id: usize, q: &'static str| {
            let app = app.clone();
            async move {
                let (s, _, b) = get(&app, &format!("/api/live/{id}/info{q}"), "mk-a").await;
                assert_eq!(s, StatusCode::OK);
                serde_json::from_slice::<serde_json::Value>(&b).unwrap()
            }
        };
        // Screen: no live video stream reported without draws=masks (the player uses the JPEG pictures).
        let v = info(0, "").await;
        assert!(v["main"].is_null() && v["sub"].is_null(), "{v}");
        assert_eq!(v["masked_needs_draw"], true);
        let v = info(0, "?caps=h264:640x480").await;
        assert!(v["main"].is_null() && v.get("transcoded").is_none(), "nothing planned either: {v}");
        let v = info(0, "?draws=masks").await;
        assert!(v["main"].is_object(), "{v}");
        assert_eq!(v["masked_needs_draw"], true);
        // Rec and Open: as always.
        for id in [1, 2] {
            let v = info(id, "").await;
            assert!(v["main"].is_object(), "{v}");
            assert_eq!(v["masked_needs_draw"], false);
        }
        // The HTTP stream: refused without draws=masks (also HEAD), sent with it.
        assert_eq!(get(&app, "/api/live/0/stream", "mk-a").await.0, StatusCode::CONFLICT);
        let head = Request::builder().method("HEAD").uri("/api/live/0/stream?caps=h264:640x480").header(header::HOST, "localhost:8090").header(header::AUTHORIZATION, auth("mk-a")).body(Body::empty()).unwrap();
        assert_eq!(app.clone().oneshot(head).await.unwrap().status(), StatusCode::CONFLICT);
        for (path, want) in [("/api/live/0/stream?draws=masks", StatusCode::OK), ("/api/live/1/stream", StatusCode::OK), ("/api/live/2/stream", StatusCode::OK), ("/api/live/0/stream?draws=text", StatusCode::CONFLICT)] {
            let req = Request::builder().uri(path).header(header::HOST, "localhost:8090").header(header::AUTHORIZATION, auth("mk-a")).body(Body::empty()).unwrap();
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), want, "{path}");
        }
        // The WebSocket, on a real server.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = app.clone().into_make_service_with_connect_info::<std::net::SocketAddr>();
        let server = tokio::spawn(async move { axum::serve(listener, svc).await });
        let ws = |path: &str| {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
            req.headers_mut().insert("Authorization", auth("mk-a").parse().unwrap());
            req.headers_mut().insert("Host", "localhost:8090".parse().unwrap());
            async move {
                match tokio_tungstenite::connect_async(req).await {
                    Ok(_) => 101,
                    Err(tokio_tungstenite::tungstenite::Error::Http(r)) => r.status().as_u16(),
                    Err(e) => panic!("{e}"),
                }
            }
        };
        assert_eq!(ws("/api/live/0/ws").await, 409);
        assert_eq!(ws("/api/live/0/ws?q=sub&audio=0").await, 409);
        assert_eq!(ws("/api/live/0/ws?draws=masks").await, 101);
        assert_eq!(ws("/api/live/1/ws").await, 101);
        assert_eq!(ws("/api/live/2/ws").await, 101);
        // The masked JPEG pictures stay open to every player.
        assert_eq!(ws("/api/live/0/jpeg").await, 101);
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_linked_server_is_told_only_whether_its_player_draws_the_masks() {
        let q: crate::live_http::StreamQ = serde_json::from_value(serde_json::json!({"q": "sub", "caps": "h264:1x1", "draws": "text,masks"})).unwrap();
        assert_eq!(q.drawing_only().draws_value(), Some("masks"));
        let q: crate::live_http::StreamQ = serde_json::from_value(serde_json::json!({"draws": "text"})).unwrap();
        assert_eq!(q.drawing_only().draws_value(), None);
        // The camera list a linked server sends: masks checked and kept in the picture.
        let (m, mode) = crate::remote::remote_masks(&serde_json::json!({"privacy_masks": [{"x": 0.9, "y": 0.1, "w": 0.5, "h": 0.2}, {"x": "a"}], "mask_mode": "screen"}));
        assert_eq!(serde_json::json!(m), serde_json::json!([{"x": 0.9, "y": 0.1, "w": 0.09999999999999998, "h": 0.2}]));
        assert_eq!(mode, Some("screen"));
        assert_eq!(crate::remote::remote_masks(&serde_json::json!({"mask_mode": "evil"})).1, None);
    }

    fn have_ffmpeg() -> bool {
        crate::child::ffmpeg().arg("-version").output().is_ok_and(|o| o.status.success())
    }

    fn ffmpeg(args: &[&str]) -> std::process::Output {
        crate::child::ffmpeg().args(args).output().unwrap()
    }

    /// The first picture of an MP4 as 320x240 grey pixels, and whether the
    /// file has a subtitle track.
    fn look(dir: &std::path::Path, name: &str, mp4: &[u8]) -> (Vec<u8>, bool) {
        let p = dir.join(name);
        std::fs::write(&p, mp4).unwrap();
        let o = ffmpeg(&["-v", "error", "-i", p.to_str().unwrap(), "-frames:v", "1", "-vf", "scale=320:240", "-pix_fmt", "gray", "-f", "rawvideo", "-"]);
        assert_eq!(o.stdout.len(), 320 * 240, "{name}: {}", String::from_utf8_lossy(&o.stderr));
        let info = ffmpeg(&["-hide_banner", "-i", p.to_str().unwrap()]);
        (o.stdout, String::from_utf8_lossy(&info.stderr).contains("Subtitle:"))
    }

    /// (black in the masked quarter (top left), white outside it).
    fn masked(px: &[u8]) -> (bool, bool) {
        let at = |x: usize, y: usize| px[y * 320 + x];
        (at(40, 30) < 40 && at(150, 110) < 40, at(240, 180) > 200 && at(100, 200) > 200)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recordings_leave_masked_unless_the_player_draws_them_and_downloads_always_do() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        let e = engine(&dir);
        // Six seconds of a white picture, as the ffmpeg recorder writes it (fragmented MP4).
        let seg = dir.join("rec").join("white.mp4");
        let o = ffmpeg(&["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=white:s=320x240:r=10", "-t", "6", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "10", "-bf", "0", "-movflags", "frag_keyframe+empty_moov+default_base_moof", seg.to_str().unwrap()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let t0 = (chrono::Utc::now().timestamp() - 3600) * 1000;
        let bytes = std::fs::metadata(&seg).unwrap().len() as i64;
        for cam in ["screen", "open"] {
            // (One file per camera: the index knows a file by its path.)
            std::fs::copy(&seg, dir.join("rec").join(format!("{cam}.mp4"))).unwrap();
            let row = crate::db::SegmentRow { path: format!("rec/{cam}.mp4"), start_ms: t0, end_ms: t0 + 6000, bytes, video_frames: 60, kind: "continuous".into(), folder: String::new() };
            e.db.insert_segment(cam, &row).unwrap();
        }
        for (id, cam, cid) in [("mk-ev-screen", "Screen", 0), ("mk-ev-open", "Open", 2)] {
            let ev = crate::engine::Event { id: id.into(), camera_id: cid, camera: cam.into(), start: t0 / 1000 + 2, end: t0 / 1000 + 3, score: 1.0, clip: None, thumb: None, objects: vec![], protect: false };
            e.db.insert_event(&ev).unwrap();
        }
        // (The encoders are tried once, before the first conversion.)
        let ready = tokio::task::spawn_blocking(|| (0..40).find_map(|_| crate::transcode::encoders_for(crate::transcode::OutCodec::H264))).await.unwrap();
        assert!(ready.is_some_and(|v| !v.is_empty()), "no H.264 encoder");
        let app = crate::web::build_router(e.clone());
        let win = format!("from={t0}&to={}", t0 + 6000);
        let is_masked = |h: &axum::http::HeaderMap| h.get("x-as-masked").is_some_and(|v| v == "1");

        // Playback: masked (converted) without draws=masks, as recorded with it.
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Screen&{win}"), "mk-a").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h), "{h:?}");
        assert_eq!(masked(&look(&dir, "p1.mp4", &b).0), (true, true), "the quarter is black, the rest white");
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Screen&{win}&draws=masks"), "mk-a").await;
        assert_eq!(s, StatusCode::OK);
        assert!(!is_masked(&h));
        assert_eq!(masked(&look(&dir, "p2.mp4", &b).0), (false, true), "as recorded: the page draws the masks");
        let (_, h, b) = get(&app, &format!("/api/playback?camera=Open&{win}"), "mk-a").await;
        assert!(!is_masked(&h));
        assert_eq!(masked(&look(&dir, "p3.mp4", &b).0), (false, true));
        // Fast and backwards: masked too, keeping the timeline's headers.
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Screen&{win}&speed=4"), "mk-b").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h) && h.contains_key("x-clip-frames"), "{h:?}");
        assert!(masked(&look(&dir, "p4.mp4", &b).0).0);
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Screen&from={}&to={}&reverse=1", t0 + 1000, t0 + 4000), "mk-b").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h) && h.get("x-clip-reverse").is_some_and(|v| v == "1"), "{h:?}");
        assert!(masked(&look(&dir, "p5.mp4", &b).0).0);

        // Downloads: masked even with draws=masks, with the time subtitles.
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Screen&{win}&download=1&draws=masks"), "mk-c").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h) && h.get(header::CONTENT_DISPOSITION).is_some());
        let (px, subs) = look(&dir, "d1.mp4", &b);
        assert_eq!((masked(&px), subs), ((true, true), true));
        // A camera without masks: not converted, the subtitles added without it.
        let (_, h, b) = get(&app, &format!("/api/playback?camera=Open&{win}&download=1"), "mk-c").await;
        assert!(!is_masked(&h) && !h.contains_key("x-as-transcoded"), "{h:?}");
        let (px, subs) = look(&dir, "d2.mp4", &b);
        assert_eq!((masked(&px).0, subs), (false, true));
        // burn_text=1: converted for the text alone.
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Open&{win}&download=1&burn_text=1"), "mk-c").await;
        assert_eq!(s, StatusCode::OK);
        assert!(!is_masked(&h) && h.get("x-as-why").is_some_and(|v| v == "text"), "{h:?}");
        assert!(look(&dir, "d3.mp4", &b).1);

        // Event clips: the same rules; the clip's first picture is in the headers.
        let (s, h, b) = get(&app, "/api/clips/mk-ev-screen", "mk-d").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h) && h.contains_key("x-clip-start-ms"), "{h:?}");
        assert_eq!(masked(&look(&dir, "c1.mp4", &b).0), (true, true));
        let (_, h, b) = get(&app, "/api/clips/mk-ev-screen?draws=masks", "mk-d").await;
        assert!(!is_masked(&h));
        assert!(!masked(&look(&dir, "c2.mp4", &b).0).0);
        let (s, h, b) = get(&app, "/api/clips/mk-ev-screen?download=1&draws=masks", "mk-d").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h));
        let (px, subs) = look(&dir, "c3.mp4", &b);
        assert_eq!((masked(&px), subs), ((true, true), true));
        let (_, h, b) = get(&app, "/api/clips/mk-ev-open?download=1", "mk-d").await;
        assert!(!is_masked(&h));
        assert!(h.get("x-clip-start-ms").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok()).is_some_and(|ms| (t0..t0 + 6000).contains(&ms)), "{h:?}");
        assert!(look(&dir, "c4.mp4", &b).1, "subtitles without converting");
        // A one-byte read still says where the clip starts.
        let req = Request::builder().uri("/api/clips/mk-ev-open?draws=masks").header(header::HOST, "localhost:8090").header(header::AUTHORIZATION, auth("mk-d")).header(header::RANGE, "bytes=0-0").body(Body::empty()).unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert!(r.headers().contains_key("x-clip-start-ms"));

        // The export: the masked camera's clip has its masks burned in.
        let (s, _, zip) = get(&app, "/api/export?ids=mk-ev-screen,mk-ev-open", "mk-e").await;
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
        let (px, subs) = look(&dir, "x1.mp4", &entry("mk-ev-screen.mp4"));
        assert_eq!((masked(&px), subs), ((true, true), true));
        let (px, subs) = look(&dir, "x2.mp4", &entry("mk-ev-open.mp4"));
        assert_eq!((masked(&px).0, subs), (false, true));

        // Fast playback pictures: without draws=masks a JPEG with the masks
        // blacked out (never the stored keyframe); with it, the keyframe.
        let at = t0 + 2500;
        let (s, h, b) = get(&app, &format!("/api/playback/picture?camera=Screen&at={at}"), "mk-f").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert_eq!(h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()), Some("image/jpeg"));
        assert!(is_masked(&h) && !h.contains_key("x-codec") && h.contains_key("x-pic-ms"), "{h:?}");
        let img = image::load_from_memory(&b).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (320, 240));
        assert!(img.get_pixel(40, 30).0[0] < 40 && img.get_pixel(150, 110).0[0] < 40, "the quarter is black");
        assert!(img.get_pixel(240, 180).0[0] > 200, "the rest is white");
        let have = h.get("x-pic-ms").and_then(|v| v.to_str().ok()).unwrap().to_string();
        let (s, _, _) = get(&app, &format!("/api/playback/picture?camera=Screen&at={at}&have={have}"), "mk-f").await;
        assert_eq!(s, StatusCode::NO_CONTENT, "the same picture again");
        for path in [format!("/api/playback/picture?camera=Screen&at={at}&draws=masks"), format!("/api/playback/picture?camera=Open&at={at}")] {
            let (s, h, _) = get(&app, &path, "mk-f").await;
            assert_eq!(s, StatusCode::OK, "{path}");
            assert!(!is_masked(&h) && h.contains_key("x-codec"), "{path}: {h:?}");
        }

        // An event copy to a folder (its own thread, no runtime there): the masks burned in.
        let ev = e.db.get_event("mk-ev-screen").unwrap().unwrap();
        let mask = super::event_download_mask(&e, &ev).unwrap().expect("masked on screen only");
        let e2 = e.clone();
        let copy = std::thread::spawn(move || crate::engine::event_copy::masked_copy(&e2, &ev, mask)).join().unwrap().unwrap().expect("a clip");
        let (px, subs) = look(&dir, "ec.mp4", &copy.read().unwrap());
        assert_eq!((masked(&px), subs), ((true, true), true));
        // (Written to a file in the tmp folder, not held in memory; removed after.)
        assert!(copy.path().starts_with(dir.join("tmp")) && copy.size() > 0);
        let p = copy.path().to_path_buf();
        drop(copy);
        assert!(!p.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MUST: a camera switched from "screen" to "recording" has unmasked
    /// recordings from before the switch; they still leave masked, and the
    /// ones made after it are not converted again.
    #[tokio::test(flavor = "multi_thread")]
    async fn recordings_from_before_a_switch_to_the_recording_level_stay_masked() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        let e = engine(&dir);
        let seg = dir.join("rec").join("white.mp4");
        let o = ffmpeg(&["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=white:s=320x240:r=10", "-t", "6", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "10", "-bf", "0", "-movflags", "frag_keyframe+empty_moov+default_base_moof", seg.to_str().unwrap()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let bytes = std::fs::metadata(&seg).unwrap().len() as i64;
        // "Screen" recorded an hour ago at the screen level (unmasked on disk).
        let old = (chrono::Utc::now().timestamp() - 3600) * 1000;
        std::fs::copy(&seg, dir.join("rec").join("old.mp4")).unwrap();
        e.db.insert_segment("screen", &crate::db::SegmentRow { path: "rec/old.mp4".into(), start_ms: old, end_ms: old + 6000, bytes, video_frames: 60, kind: "continuous".into(), folder: String::new() }).unwrap();
        let ev = crate::engine::Event { id: "mk-sw-old".into(), camera_id: 0, camera: "Screen".into(), start: old / 1000 + 2, end: old / 1000 + 3, score: 1.0, clip: None, thumb: None, objects: vec![], protect: false };
        e.db.insert_event(&ev).unwrap();
        // Now switched to "recording" (as Engine::update_camera notes it).
        let now = chrono::Utc::now().timestamp_millis();
        {
            let h = e.camera(0).unwrap();
            let mut c = h.cfg.read().unwrap().clone();
            c.mask_mode = Some("recording".into());
            *h.cfg.write().unwrap() = c.clone();
            e.config.lock().unwrap().cameras[0] = c.clone();
            super::note_level(&e.db, &c, now, false);
            assert!(!super::sends_unmasked(&c));
        }
        // A recording made after the switch (masked by the camera's re-encoding; here the same white file).
        let new = now + super::SETTLE_MS + 5_000;
        std::fs::copy(&seg, dir.join("rec").join("new.mp4")).unwrap();
        e.db.insert_scanned_segment("screen", &crate::db::SegmentRow { path: "rec/new.mp4".into(), start_ms: new, end_ms: new + 6000, bytes, video_frames: 60, kind: "continuous".into(), folder: String::new() }).unwrap();
        let ready = tokio::task::spawn_blocking(|| (0..40).find_map(|_| crate::transcode::encoders_for(crate::transcode::OutCodec::H264))).await.unwrap();
        assert!(ready.is_some_and(|v| !v.is_empty()), "no H.264 encoder");
        let app = crate::web::build_router(e.clone());
        let is_masked = |h: &axum::http::HeaderMap| h.get("x-as-masked").is_some_and(|v| v == "1");
        let old_win = format!("from={old}&to={}", old + 6000);
        // Watching, without draws=masks: the old one converted, masked.
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Screen&{old_win}"), "mk-a").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h), "{h:?}");
        assert_eq!(masked(&look(&dir, "s1.mp4", &b).0), (true, true));
        // A download (even from a page that draws them): masked.
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Screen&{old_win}&download=1&draws=masks"), "mk-b").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h));
        assert_eq!(masked(&look(&dir, "s2.mp4", &b).0), (true, true));
        // The event's clip, watched and downloaded; the export; a fast-playback picture.
        let (s, h, b) = get(&app, "/api/clips/mk-sw-old", "mk-c").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h));
        assert_eq!(masked(&look(&dir, "s3.mp4", &b).0), (true, true));
        let (s, h, b) = get(&app, "/api/clips/mk-sw-old?download=1", "mk-c").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h));
        assert_eq!(masked(&look(&dir, "s4.mp4", &b).0), (true, true));
        let (s, h, b) = get(&app, &format!("/api/playback/picture?camera=Screen&at={}", old + 2500), "mk-d").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h) && !h.contains_key("x-codec"), "{h:?}");
        let ev2 = e.db.get_event("mk-sw-old").unwrap().unwrap();
        assert!(super::event_download_mask(&e, &ev2).unwrap().is_some(), "an export or event copy burns them in");
        // The recording made after the switch: as recorded, not converted again.
        let new_win = format!("from={new}&to={}", new + 6000);
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Screen&{new_win}"), "mk-e").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(!is_masked(&h) && !h.contains_key("x-as-transcoded"), "{h:?}");
        let (s, h, _) = get(&app, &format!("/api/playback?camera=Screen&{new_win}&download=1"), "mk-e").await;
        assert_eq!(s, StatusCode::OK);
        assert!(!is_masked(&h) && !h.contains_key("x-as-transcoded"), "{h:?}");
        let (s, h, _) = get(&app, &format!("/api/playback/picture?camera=Screen&at={}", new + 2500), "mk-f").await;
        assert_eq!(s, StatusCode::OK);
        assert!(!is_masked(&h) && h.contains_key("x-codec"), "{h:?}");
        // A window reaching back before the switch: masked.
        let (_, h, _) = get(&app, &format!("/api/playback?camera=Screen&from={}&to={}", old, new + 6000), "mk-f").await;
        assert!(is_masked(&h), "{h:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MUST: a pushed camera set to "recording" (or with a level not
    /// possible for it) sends its stream unmasked: no live video without
    /// draws=masks.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pushed_camera_at_the_recording_level_sends_no_unmasked_live_video() {
        let dir = std::env::temp_dir().join(format!("amber-masked-push-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let e = engine(&dir);
        let h = e.add_test_camera("Pushed");
        let c: crate::config::CameraConfig = toml::from_str("name = \"Pushed\"\nurl = \"push://pushed\"\nmask_mode = \"recording\"\nprivacy_masks = [{ x = 0.0, y = 0.0, w = 0.5, h = 0.5 }]\nshare = true\nshare_unmasked = false\n").unwrap();
        *h.cfg.write().unwrap() = c.clone();
        *h.running.write().unwrap() = c.clone();
        let _m = crate::ingest::live::LiveMuxer::start(h.live_main.clone(), &[video_track()], &["avc1.640033".into()]);
        let app = crate::web::build_router(e.clone());
        let (s, _, b) = get(&app, &format!("/api/live/{}/info", h.id), "mk-a").await;
        assert_eq!(s, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert!(v["main"].is_null() && v["sub"].is_null(), "{v}");
        assert_eq!(get(&app, &format!("/api/live/{}/stream", h.id), "mk-a").await.0, StatusCode::CONFLICT);
        // (Status only: the stream does not end.)
        let req = Request::builder().uri(format!("/api/live/{}/stream?draws=masks", h.id)).header(header::HOST, "localhost:8090").header(header::AUTHORIZATION, auth("mk-a")).body(Body::empty()).unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
        // Sharing it waits for the admin's word.
        assert!(crate::restream::http::blocker(&c).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn put(app: &axum::Router, id: usize, body: serde_json::Value) -> StatusCode {
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/api/cameras/{id}"))
            .header(header::HOST, "localhost:8090")
            .header(header::AUTHORIZATION, auth("mk-a"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let s = r.status();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap_or_default();
        assert_eq!(s, StatusCode::OK, "PUT {body}: {}", String::from_utf8_lossy(&b));
        s
    }

    async fn delete(app: &axum::Router, id: usize) -> serde_json::Value {
        let req = Request::builder().method("DELETE").uri(format!("/api/cameras/{id}")).header(header::HOST, "localhost:8090").header(header::AUTHORIZATION, auth("mk-a")).body(Body::empty()).unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap()
    }

    /// Six seconds of a white 320x240 picture as the ffmpeg recorder writes
    /// it, at `dir/rec/<name>.mp4`; its size.
    fn white(dir: &std::path::Path, name: &str) -> i64 {
        let p = dir.join("rec").join(format!("{name}.mp4"));
        let o = ffmpeg(&["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=white:s=320x240:r=10", "-t", "6", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "10", "-bf", "0", "-movflags", "frag_keyframe+empty_moov+default_base_moof", p.to_str().unwrap()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        std::fs::metadata(&p).unwrap().len() as i64
    }

    fn add_seg(e: &Engine, slug: &str, name: &str, start_ms: i64, bytes: i64) {
        e.db.insert_segment(slug, &crate::db::SegmentRow { path: format!("rec/{name}.mp4"), start_ms, end_ms: start_ms + 6000, bytes, video_frames: 60, kind: "continuous".into(), folder: String::new() }).unwrap();
    }

    /// Playback (watched without drawing the masks, and downloaded by a page
    /// that draws them) and the fast-playback picture of `camera` at
    /// `start`: all with the masked quarter black.
    async fn all_masked(app: &axum::Router, dir: &std::path::Path, camera: &str, start: i64, tag: &str, who: &str) {
        let is_masked = |h: &axum::http::HeaderMap| h.get("x-as-masked").is_some_and(|v| v == "1");
        let win = format!("from={start}&to={}", start + 6000);
        for (i, q) in ["", "&download=1&draws=masks"].iter().enumerate() {
            let (s, h, b) = get(app, &format!("/api/playback?camera={camera}&{win}{q}"), who).await;
            assert_eq!(s, StatusCode::OK, "{tag}{q}: {}", String::from_utf8_lossy(&b));
            assert!(is_masked(&h), "{tag}{q}: {h:?}");
            assert_eq!(masked(&look(dir, &format!("{tag}{i}.mp4"), &b).0), (true, true), "{tag}{q}");
        }
        let (s, h, b) = get(app, &format!("/api/playback/picture?camera={camera}&at={}", start + 2500), who).await;
        assert_eq!(s, StatusCode::OK, "{tag}: {}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h) && !h.contains_key("x-codec"), "{tag}: {h:?}");
        let img = image::load_from_memory(&b).unwrap().to_luma8();
        assert!(img.get_pixel(40, 30).0[0] < 40 && img.get_pixel(240, 180).0[0] > 200, "{tag}: the picture's quarter is black");
    }

    async fn encoders_ready() {
        let ready = tokio::task::spawn_blocking(|| (0..40).find_map(|_| crate::transcode::encoders_for(crate::transcode::OutCodec::H264))).await.unwrap();
        assert!(ready.is_some_and(|v| !v.is_empty()), "no H.264 encoder");
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn pause() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    /// MUST (re-review): renaming a camera (and back), and setting its level
    /// in the same request as a rename, never takes up a "masked from"
    /// record made before: what was recorded at the screen level in between
    /// leaves masked.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rename_never_takes_up_an_earlier_masked_from_record() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        let e = engine(&dir);
        let bytes = white(&dir, "w");
        encoders_ready().await;
        let app = crate::web::build_router(e.clone());
        let since = |id: usize| super::masked_since(&e.db, &e.camera(id).unwrap().cfg.read().unwrap());

        // The reviewer's steps, on "Screen" (masked on screen only).
        put(&app, 0, serde_json::json!({"enabled": false, "mask_mode": "recording"})).await;
        let first = since(0).expect("noted at the recording level");
        pause();
        put(&app, 0, serde_json::json!({"name": "Porch X"})).await;
        pause();
        put(&app, 0, serde_json::json!({"mask_mode": "screen"})).await;
        assert_eq!(since(0), None);
        pause();
        put(&app, 0, serde_json::json!({"mask_mode": "recording"})).await;
        pause();
        let t = now_ms();
        put(&app, 0, serde_json::json!({"name": "Screen"})).await;
        let after = since(0).expect("noted again");
        assert!(after >= t + super::SETTLE_MS && after > first, "from the last change, not the first record: {after} vs {first}");
        // Something recorded after the first record's moment but before the
        // last change (as at the screen level, unmasked on disk) leaves masked.
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/s1.mp4")).unwrap();
        let s = t + super::SETTLE_MS - 1;
        assert!(s > first);
        add_seg(&e, "screen", "s1", s, bytes);
        all_masked(&app, &dir, "Screen", s, "rn", "mk-g").await;

        // In one request: renamed back to its old name and set to "recording".
        put(&app, 1, serde_json::json!({"enabled": false})).await;
        let first = since(1).expect("Rec is at the recording level");
        pause();
        put(&app, 1, serde_json::json!({"name": "Porch Y", "mask_mode": "screen"})).await;
        pause();
        let t = now_ms();
        put(&app, 1, serde_json::json!({"name": "Rec", "mask_mode": "recording"})).await;
        let after = since(1).unwrap();
        assert!(after >= t + super::SETTLE_MS && after > first, "{after} vs {first}");
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/r1.mp4")).unwrap();
        let s = t + super::SETTLE_MS - 1;
        add_seg(&e, "rec", "r1", s, bytes);
        all_masked(&app, &dir, "Rec", s, "rq", "mk-h").await;
        // The old names keep no record.
        for slug in ["porch_x", "porch_y"] {
            assert_eq!(e.db.ui_get(super::SINCE_SCOPE, &format!("{}{slug}", super::SINCE_PREFIX)).unwrap(), None, "{slug}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SHOULD (re-review): a removed camera's recordings and pictures keep
    /// the masks it had (also for a camera given its name later, and for a
    /// player that draws masks: it does not know them); when they cannot be
    /// read, nothing is sent that needs them.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_removed_cameras_recordings_and_pictures_keep_its_masks() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        std::fs::create_dir_all(dir.join("thumbs")).unwrap();
        let e = engine(&dir);
        let bytes = white(&dir, "w");
        encoders_ready().await;
        let app = crate::web::build_router(e.clone());
        // "Rec" recorded an hour ago (before its masks were noted: unmasked), with an event and its picture.
        let old = now_ms() - 3_600_000;
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/g1.mp4")).unwrap();
        add_seg(&e, "rec", "g1", old, bytes);
        let white_jpeg = {
            let img = image::RgbImage::from_pixel(320, 240, image::Rgb([255, 255, 255]));
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
            out.into_inner()
        };
        let thumb = dir.join("thumbs").join("mk-gone-1.jpg");
        std::fs::write(&thumb, &white_jpeg).unwrap();
        let ev = crate::engine::Event { id: "mk-gone-1".into(), camera_id: 1, camera: "Rec".into(), start: old / 1000 + 2, end: old / 1000 + 3, score: 1.0, clip: None, thumb: Some("mk-gone-1".into()), objects: vec![], protect: false };
        e.db.insert_event(&ev).unwrap();
        put(&app, 1, serde_json::json!({"enabled": false})).await;
        assert!(e.db.ui_get(super::SINCE_SCOPE, &format!("{}rec", super::SINCE_PREFIX)).unwrap().is_some());
        // Removed: its record goes, its masks stay with its recordings.
        assert_eq!(delete(&app, 1).await["result"], "success");
        assert!(e.db.ui_get(super::SINCE_SCOPE, &format!("{}rec", super::SINCE_PREFIX)).unwrap().is_none(), "no record left for a camera given the name later");
        assert!(super::camera_cfg(&e, "Rec").is_none());
        all_masked(&app, &dir, "Rec", old, "gn", "mk-i").await;
        // Even for a page that draws masks (it has none to draw for a removed camera).
        let (_, h, b) = get(&app, &format!("/api/playback?camera=Rec&from={old}&to={}&draws=masks", old + 6000), "mk-k").await;
        assert!(h.get("x-as-masked").is_some_and(|v| v == "1"), "{h:?}");
        assert!(masked(&look(&dir, "gd.mp4", &b).0).0);
        // Its event's picture: the masks blacked out.
        let pic = crate::privacy::read_event_picture(&e.config, &e.db, "mk-gone-1", &thumb).unwrap();
        let img = image::load_from_memory(&pic).unwrap().to_luma8();
        assert!(img.get_pixel(40, 30).0[0] < 40 && img.get_pixel(240, 180).0[0] > 200);
        // Another camera renamed to its name (no masks of its own): the old recordings still leave masked.
        put(&app, 2, serde_json::json!({"name": "Rec", "enabled": false})).await;
        assert!(super::camera_cfg(&e, "Rec").is_some_and(|c| c.privacy_masks.is_empty()));
        all_masked(&app, &dir, "Rec", old, "gr", "mk-j").await;
        // What it records under the name from now on is not masked (it has no masks).
        let later = now_ms() + 60_000;
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/g2.mp4")).unwrap();
        add_seg(&e, "rec", "g2", later, bytes);
        let (_, h, _) = get(&app, &format!("/api/playback?camera=Rec&from={later}&to={}", later + 6000), "mk-k").await;
        assert!(!h.contains_key("x-as-masked"), "{h:?}");

        // Masks that cannot be read (a damaged record): nothing that needs them is sent.
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/l1.mp4")).unwrap();
        add_seg(&e, "lost", "l1", old, bytes);
        e.db.ui_put(super::SINCE_SCOPE, &format!("{}lost|0000000000001|00000000", super::GONE_PREFIX), "not json").unwrap();
        let win = format!("from={old}&to={}", old + 6000);
        for path in [format!("/api/playback?camera=Lost&{win}"), format!("/api/playback?camera=Lost&{win}&draws=masks"), format!("/api/playback?camera=Lost&{win}&download=1"), format!("/api/playback/picture?camera=Lost&at={}", old + 2500)] {
            let (s, _, b) = get(&app, &path, "mk-l").await;
            assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{path}: {}", String::from_utf8_lossy(&b));
        }
        // Its pictures: wholly black, and the stored one is left as it was.
        let lost = dir.join("thumbs").join("mk-lost-1.jpg");
        std::fs::write(&lost, &white_jpeg).unwrap();
        let ev = crate::engine::Event { id: "mk-lost-1".into(), camera_id: 9, camera: "Lost".into(), start: old / 1000, end: old / 1000 + 1, score: 1.0, clip: None, thumb: Some("mk-lost-1".into()), objects: vec![], protect: false };
        e.db.insert_event(&ev).unwrap();
        let pic = crate::privacy::read_event_picture(&e.config, &e.db, "mk-lost-1", &lost).unwrap();
        let img = image::load_from_memory(&pic).unwrap().to_luma8();
        assert!(img.get_pixel(240, 180).0[0] < 40, "all black");
        assert_eq!(std::fs::read(&lost).unwrap(), white_jpeg, "not written back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A JPEG answer as grey pixels.
    fn grey(b: &[u8]) -> image::GrayImage {
        image::load_from_memory(b).unwrap().to_luma8()
    }

    /// A white 320x240 JPEG.
    fn white_jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(320, 240, image::Rgb([255, 255, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    /// The still, the preview and the filmstrip of `camera` at `at` (6 s of
    /// white recorded there): (top-left quarter black, bottom-right corner
    /// black, the middle right white) for each.
    async fn pictures(app: &axum::Router, camera: &str, at: i64, who: &str) -> Vec<(bool, bool, bool)> {
        let mut out = Vec::new();
        for path in [format!("/api/still?camera={camera}&at={at}"), format!("/api/preview?camera={camera}&at={at}&w=320"), format!("/api/filmstrip?camera={camera}&from={}&step=2000&n=2&w=320", at - 1000)] {
            let (s, h, b) = get(app, &path, who).await;
            assert_eq!(s, StatusCode::OK, "{path}: {}", String::from_utf8_lossy(&b));
            let img = grey(&b);
            // (The filmstrip's first picture is its top left one.)
            let (w, ht) = if path.contains("filmstrip") {
                (h.get("x-sprite-w").unwrap().to_str().unwrap().parse::<u32>().unwrap(), h.get("x-sprite-h").unwrap().to_str().unwrap().parse::<u32>().unwrap())
            } else {
                img.dimensions()
            };
            // (A 4:3 picture fitted into its cell, black bars beside or above it.)
            let (cw, ch) = ((w as f64).min(ht as f64 * 4.0 / 3.0), (ht as f64).min(w as f64 * 3.0 / 4.0));
            let (x0, y0) = ((w as f64 - cw) / 2.0, (ht as f64 - ch) / 2.0);
            let px = |fx: f64, fy: f64| img.get_pixel((x0 + cw * fx) as u32, (y0 + ch * fy) as u32).0[0];
            out.push((px(0.12, 0.12) < 40 && px(0.4, 0.4) < 40, px(0.95, 0.95) < 40, px(0.9, 0.3) > 200));
        }
        out
    }

    /// SHOULD 1 (round 3): a camera removed and then added again under its
    /// name: the pictures and time-lapses of what the removed one recorded
    /// keep its masks (and the new one's), the new one's own footage does not
    /// get them; pictures and time-lapses made before go stale.
    #[tokio::test(flavor = "multi_thread")]
    async fn pictures_and_time_lapses_of_a_removed_cameras_footage_keep_its_masks_after_it_is_added_again() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-readd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        std::fs::create_dir_all(dir.join("thumbs")).unwrap();
        let e = engine(&dir);
        e.config.lock().unwrap().timelapse.seconds = 10;
        let bytes = white(&dir, "w");
        let app = crate::web::build_router(e.clone());
        // "Screen" (top-left quarter masked on screen only) recorded an hour ago, with an event picture.
        let old = now_ms() - 3_600_000;
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/o1.mp4")).unwrap();
        add_seg(&e, "screen", "o1", old, bytes);
        let thumb = dir.join("thumbs").join("mk-readd-1.jpg");
        std::fs::write(&thumb, white_jpeg()).unwrap();
        let ev = crate::engine::Event { id: "mk-readd-1".into(), camera_id: 0, camera: "Screen".into(), start: old / 1000 + 2, end: old / 1000 + 3, score: 1.0, clip: None, thumb: Some("mk-readd-1".into()), objects: vec![], protect: false };
        e.db.insert_event(&ev).unwrap();
        let at = old + 2500;
        assert_eq!(pictures(&app, "Screen", at, "mk-a").await, vec![(true, false, true); 3], "masked while it is set up");
        // Removed, then a camera without masks added under the name.
        assert_eq!(delete(&app, 0).await["result"], "success");
        // (As if removed two minutes ago, so what came after can be recorded by now.)
        let removed = now_ms() - 120_000;
        for (k, v) in e.db.ui_scope_prefix(super::SINCE_SCOPE, &format!("{}screen|", super::GONE_PREFIX)).unwrap() {
            e.db.ui_delete(super::SINCE_SCOPE, &k).unwrap();
            e.db.ui_put(super::SINCE_SCOPE, &format!("{}screen|{removed:013}|00000000", super::GONE_PREFIX), &v).unwrap();
        }
        let plain: crate::config::CameraConfig = toml::from_str("name = \"Screen\"\nurl = \"rtsp://127.0.0.1/x\"\nenabled = false\n").unwrap();
        let id = e.add_camera_arc(plain);
        assert!(super::camera_cfg(&e, "Screen").is_some_and(|c| c.privacy_masks.is_empty()));
        // The old footage's still, preview and filmstrip: still masked.
        assert_eq!(pictures(&app, "Screen", at, "mk-b").await, vec![(true, false, true); 3], "the removed camera's masks");
        // Its event's picture too.
        let pic = crate::privacy::read_event_picture(&e.config, &e.db, "mk-readd-1", &thumb).unwrap();
        let img = grey(&pic);
        assert!(img.get_pixel(40, 30).0[0] < 40 && img.get_pixel(240, 180).0[0] > 200, "the event picture");
        // What the new camera records itself: as it is.
        let later = now_ms() - 60_000;
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/n1.mp4")).unwrap();
        add_seg(&e, "screen", "n1", later, bytes);
        assert_eq!(pictures(&app, "Screen", later + 2500, "mk-c").await, vec![(false, false, true); 3], "recorded after the removal");
        // A time-lapse of the old day made unmasked (as before this fix) is not served...
        let date = chrono::DateTime::from_timestamp_millis(old).unwrap().with_timezone(&chrono::Local).date_naive();
        let tl = e.timelapse_path("screen", date);
        std::fs::create_dir_all(tl.parent().unwrap()).unwrap();
        std::fs::copy(dir.join("rec/w.mp4"), &tl).unwrap();
        std::fs::write(tl.with_extension("masks"), "none").unwrap();
        assert!(e.timelapse_masks_old("screen", date), "made without the removed camera's masks");
        let (s, _, _) = get(&app, &format!("/api/timelapse/screen/{date}"), "mk-d").await;
        assert_eq!(s, StatusCode::CONFLICT);
        // ... and made again, it has them.
        let e2 = e.clone();
        let made = tokio::task::spawn_blocking(move || e2.make_timelapse("Screen", date)).await.unwrap().unwrap();
        assert!(!e.timelapse_masks_old("screen", date));
        let (s, _, b) = get(&app, &format!("/api/timelapse/screen/{date}"), "mk-d").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, std::fs::read(&made).unwrap());
        let px = look(&dir, "tl.mp4", &b).0;
        assert_eq!(masked(&px), (true, true), "the time-lapse's quarter is black");
        // Added masks of its own (bottom-right corner): both, and the pictures kept before go stale.
        let corner = serde_json::json!({ "privacy_masks": [{ "x": 0.75, "y": 0.75, "w": 0.25, "h": 0.25 }], "mask_mode": "screen" });
        put(&app, id, corner).await;
        assert_eq!(pictures(&app, "Screen", at, "mk-e").await, vec![(true, true, true); 3], "the old masks and the new ones");
        assert_eq!(pictures(&app, "Screen", later + 2500, "mk-f").await, vec![(false, true, true); 3], "only its own on its own footage");
        assert!(e.timelapse_masks_old("screen", date), "made without the new camera's masks");
        let e2 = e.clone();
        tokio::task::spawn_blocking(move || e2.make_timelapse("Screen", date)).await.unwrap().unwrap();
        let (s, _, b) = get(&app, &format!("/api/timelapse/screen/{date}"), "mk-g").await;
        assert_eq!(s, StatusCode::OK);
        let px = look(&dir, "tl2.mp4", &b).0;
        assert!(masked(&px).0 && px[235 * 320 + 315] < 40, "both in the time-lapse");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SHOULD 2 (round 3): an older database brought back (its "masked from"
    /// record from before) never makes what was recorded since count as
    /// masked: only recordings this run's masking recorder made say so.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restored_database_never_counts_later_recordings_as_masked() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        let e = engine(&dir);
        let bytes = white(&dir, "w");
        encoders_ready().await;
        let app = crate::web::build_router(e.clone());
        let cfg = e.camera(1).unwrap().cfg.read().unwrap().clone();
        assert!(super::burns_in(&cfg));
        // The restored database: "Rec" masked in its recordings from two hours ago.
        let backup = now_ms() - 7_200_000;
        let v = serde_json::json!({ "rev": crate::privacy::masks_revision(&cfg), "level": "recording", "since": backup });
        e.db.ui_put(super::SINCE_SCOPE, &format!("{}rec", super::SINCE_PREFIX), &v.to_string()).unwrap();
        // The start after it was brought back (as Engine::new notes it).
        let start = now_ms();
        super::prune_records(&e.db, &e.config.lock().unwrap().cameras, &[], start);
        super::note_level(&e.db, &cfg, start, false);
        assert_eq!(super::masked_since(&e.db, &cfg), Some(backup), "the record from the backup is kept for saved clips");
        // Recorded an hour ago at the screen level (unmasked on disk), found again by the start-up indexing.
        let t = start - 3_600_000;
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/r1.mp4")).unwrap();
        e.db.insert_scanned_segment("rec", &crate::db::SegmentRow { path: "rec/r1.mp4".into(), start_ms: t, end_ms: t + 6000, bytes, video_frames: 0, kind: "legacy".into(), folder: String::new() }).unwrap();
        assert_eq!(e.db.segment_mask_revs("rec", t, t + 6000).unwrap(), vec![None]);
        all_masked(&app, &dir, "Rec", t, "rs", "mk-h").await;
        // What this run's recorder makes (masked): as it is, not converted again.
        let n = start + super::SETTLE_MS + 5_000;
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/r2.mp4")).unwrap();
        e.db.insert_scanned_segment("rec", &crate::db::SegmentRow { path: "rec/r2.mp4".into(), start_ms: n, end_ms: n + 6000, bytes, video_frames: 0, kind: "legacy".into(), folder: String::new() }).unwrap();
        assert_eq!(e.db.segment_mask_revs("rec", n, n + 6000).unwrap(), vec![Some(crate::privacy::masks_revision(&cfg))]);
        let (s, h, _) = get(&app, &format!("/api/playback?camera=Rec&from={n}&to={}", n + 6000), "mk-i").await;
        assert_eq!(s, StatusCode::OK);
        assert!(!h.contains_key("x-as-masked") && !h.contains_key("x-as-transcoded"), "{h:?}");
        // A window over both: masked.
        let (_, h, _) = get(&app, &format!("/api/playback?camera=Rec&from={t}&to={}", n + 6000), "mk-i").await;
        assert!(h.get("x-as-masked").is_some_and(|v| v == "1"), "{h:?}");
        // The same file written again by the native recorder (a level change): no longer counted.
        e.db.insert_segment("rec", &crate::db::SegmentRow { path: "rec/r2.mp4".into(), start_ms: n, end_ms: n + 6000, bytes, video_frames: 60, kind: "continuous".into(), folder: String::new() }).unwrap();
        assert_eq!(e.db.segment_mask_revs("rec", n, n + 6000).unwrap(), vec![None]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MINOR 1 (round 3): a camera taken out of the settings file by hand
    /// while the server was stopped (removed or renamed there): at the next
    /// start its recordings keep the masks it had.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_camera_taken_out_of_the_settings_by_hand_keeps_its_masks_for_its_recordings() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-hand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        let e = engine(&dir);
        let bytes = white(&dir, "w");
        encoders_ready().await;
        let app = crate::web::build_router(e.clone());
        // Its settings applied (as at every start and change): its masks are noted.
        put(&app, 0, serde_json::json!({"enabled": false})).await;
        let known = format!("{}screen", super::KNOWN_PREFIX);
        assert!(e.db.ui_get(super::SINCE_SCOPE, &known).unwrap().is_some());
        let old = now_ms() - 3_600_000;
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/h1.mp4")).unwrap();
        add_seg(&e, "screen", "h1", old, bytes);
        // Stopped; "Screen" taken out of config.toml (here: renamed to "Porch H" there); started again.
        {
            let h = e.cameras.write().unwrap()[0].take().unwrap();
            h.active.store(false, std::sync::atomic::Ordering::Relaxed);
            let mut c = e.config.lock().unwrap();
            c.cameras.retain(|x| x.name != "Screen");
        }
        assert_eq!(footage_mask_of(&e, "screen"), None, "before the start nothing is kept for it");
        let start = now_ms();
        let cams = e.config.lock().unwrap().cameras.clone();
        super::prune_records(&e.db, &cams, &[], start);
        assert!(e.db.ui_get(super::SINCE_SCOPE, &known).unwrap().is_none(), "turned into a removed camera's masks");
        assert_eq!(super::gone_masks(&e.db, "screen").unwrap().len(), 1);
        assert!(footage_mask_of(&e, "screen").is_some());
        all_masked(&app, &dir, "Screen", old, "hd", "mk-j").await;
        let (s, _, b) = get(&app, &format!("/api/still?camera=Screen&at={}", old + 2500), "mk-j").await;
        assert_eq!(s, StatusCode::OK);
        let img = grey(&b);
        assert!(img.get_pixel(40, 30).0[0] < 40 && img.get_pixel(240, 180).0[0] > 200, "its still");
        // The next start: kept once, not again.
        super::prune_records(&e.db, &cams, &[], start + 1000);
        assert_eq!(super::gone_masks(&e.db, "screen").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn footage_mask_of(e: &Engine, slug: &str) -> Option<String> {
        super::footage_mask(&e.db, None, slug, None, 0, i64::MAX / 2, true).unwrap()
    }

    /// MINOR 2 (round 3): a camera removed right after a rename keeps its
    /// masks under the name its recordings are kept under now; a rename of a
    /// camera whose name changed meanwhile is not done.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_removal_after_a_rename_keeps_the_masks_with_the_recordings() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-renrm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        let e = engine(&dir);
        let bytes = white(&dir, "w");
        encoders_ready().await;
        let app = crate::web::build_router(e.clone());
        let old = now_ms() - 3_600_000;
        std::fs::copy(dir.join("rec/w.mp4"), dir.join("rec/m1.mp4")).unwrap();
        add_seg(&e, "screen", "m1", old, bytes);
        let mut cfg = e.camera(0).unwrap().cfg.read().unwrap().clone();
        cfg.name = "Porch Z".into();
        cfg.enabled = false;
        assert!(!e.rename_and_update_camera(0, "Not Its Name", cfg.clone()).unwrap(), "its name is not that");
        assert!(e.rename_and_update_camera(0, "Screen", cfg).unwrap());
        assert_eq!(e.db.segments_overlapping("porch_z", old, old + 6000).unwrap().len(), 1);
        // A removal from another request waits while a rename is under way...
        let waiting = {
            let held = crate::engine::CAMERA_NAMING.lock().unwrap();
            let e2 = e.clone();
            let t = std::thread::spawn(move || e2.try_remove_camera(0));
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(!t.is_finished() && e.camera(0).is_some(), "not removed in the middle of a rename");
            drop(held);
            t
        };
        // ... and then sees the new name.
        assert!(waiting.join().unwrap().unwrap());
        assert_eq!(super::gone_masks(&e.db, "porch_z").unwrap().len(), 1, "with the recordings");
        // Under the old name: only what was recorded there until just after the rename.
        let tail = super::gone_masks(&e.db, "screen").unwrap();
        assert_eq!(tail.len(), 1);
        assert!(tail[0].0 > old && tail[0].0 <= now_ms() + 120_000, "{tail:?}");
        all_masked(&app, &dir, "Porch%20Z", old, "rr", "mk-k").await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The recordings in `<dir>/cameras/<slug>/segments` (names).
    fn seg_files(dir: &std::path::Path, slug: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir.join("cameras").join(slug).join("segments"))
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".mp4")).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// The finding (adversarial check, 2026-11-17): a camera masked on
    /// screen, renamed while it records on the ffmpeg pipeline, then a
    /// camera without masks added under its old name. The files its
    /// recorder had not finished (or the 15 s scan had not indexed yet) at
    /// the rename were found later in the old folder and shown unmasked as
    /// the new camera's. Now its recorder stops first and they are indexed
    /// and moved with it; nothing in the old folder is the new camera's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_renamed_cameras_last_recordings_go_with_it_and_stay_masked() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let e = engine(&dir);
        e.config.lock().unwrap().storage.segment_seconds = 2;
        encoders_ready().await;
        let app = crate::web::build_router(e.clone());
        let src = "url = \"lavfi://color=c=white:s=320x240:r=10\"\n";
        let cam: crate::config::CameraConfig =
            toml::from_str(&format!("name = \"Rec Two\"\n{src}mask_mode = \"screen\"\nprivacy_masks = [{{ x = 0.0, y = 0.0, w = 0.5, h = 0.5 }}]\n")).unwrap();
        let id = e.add_camera_arc(cam.clone());
        // Recording (on the ffmpeg pipeline), a few files written.
        let t0 = std::time::Instant::now();
        while seg_files(&dir, "rec_two").len() < 6 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(40), "no recordings: {:?}", seg_files(&dir, "rec_two"));
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        assert!(e.camera(id).unwrap().state.read().unwrap().ingest.starts_with("ffmpeg"));
        // The 15 s scan ran just now: the newest files are not indexed yet.
        e.db.import_recent_camera_dirs(&[("rec_two".into(), String::new(), dir.clone())], 2, 0).unwrap();
        let indexed = e.db.segments_overlapping("rec_two", 0, i64::MAX / 2).unwrap().len();
        assert!(indexed > 0 && indexed < seg_files(&dir, "rec_two").len(), "a file still being written is left out ({indexed})");
        // Renamed while it records (and switched off, so nothing new records).
        let mut renamed = cam.clone();
        renamed.name = "Rec Three".into();
        renamed.enabled = false;
        let e2 = e.clone();
        let t = std::time::Instant::now();
        assert!(tokio::task::spawn_blocking(move || e2.rename_and_update_camera(id, "Rec Two", renamed)).await.unwrap().unwrap());
        assert!(t.elapsed() < crate::engine::RENAME_STOP_WAIT, "its recorder stopped in time: {:?}", t.elapsed());
        // Everything it left in the old folder is the renamed camera's.
        let files = seg_files(&dir, "rec_two");
        let moved: Vec<crate::db::SegmentRow> = e.db.segments_overlapping("rec_three", 0, i64::MAX / 2).unwrap();
        let names: std::collections::BTreeSet<String> = moved.iter().map(|r| r.path.rsplit('/').next().unwrap().to_string()).collect();
        for f in &files {
            assert!(names.contains(f), "{f} indexed and moved with the camera: {names:?}");
        }
        assert!(e.db.segments_overlapping("rec_two", 0, i64::MAX / 2).unwrap().is_empty());
        // A camera without masks added under the old name; its folder scanned
        // (as the 15 s scan or the next start would): nothing there is its own.
        let plain: crate::config::CameraConfig = toml::from_str(&format!("name = \"Rec Two\"\n{src}enabled = false\n")).unwrap();
        e.add_camera_arc(plain);
        assert_eq!(e.db.import_final_camera_dirs(&[("rec_two".into(), String::new(), dir.clone())], 2, 0).unwrap(), 0);
        assert!(e.db.segments_overlapping("rec_two", 0, i64::MAX / 2).unwrap().is_empty());
        // The last one it recorded: the renamed camera's, masked when it leaves;
        // under the old name, nothing to show (or masked).
        let last = moved.iter().filter(|r| r.end_ms - r.start_ms >= 1000).max_by_key(|r| r.start_ms).unwrap().clone();
        let (from, to) = (last.start_ms, last.end_ms);
        let is_masked = |h: &axum::http::HeaderMap| h.get("x-as-masked").is_some_and(|v| v == "1");
        let (s, h, b) = get(&app, &format!("/api/playback?camera=Rec%20Three&from={from}&to={to}&download=1"), "mk-l").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert!(is_masked(&h), "{h:?}");
        assert_eq!(masked(&look(&dir, "tail0.mp4", &b).0), (true, true));
        let (s, _, b) = get(&app, &format!("/api/still?camera=Rec%20Three&at={}", (from + to) / 2), "mk-l").await;
        assert_eq!(s, StatusCode::OK);
        let img = grey(&b);
        assert!(img.get_pixel(40, 30).0[0] < 40 && img.get_pixel(240, 180).0[0] > 200, "its still");
        for q in ["", "&download=1", "&draws=masks"] {
            let (s, h, b) = get(&app, &format!("/api/playback?camera=Rec%20Two&from={from}&to={to}{q}"), "mk-l").await;
            if s == StatusCode::OK {
                assert!(is_masked(&h), "{q}: {h:?}");
                assert_eq!(masked(&look(&dir, "tail1.mp4", &b).0), (true, true), "{q}");
            }
        }
        let (s, _, b) = get(&app, &format!("/api/still?camera=Rec%20Two&at={}", (from + to) / 2), "mk-l").await;
        if s == StatusCode::OK {
            assert!(grey(&b).get_pixel(40, 30).0[0] < 40, "the old name's still");
        }
        // The old recorder wrote nothing after it stopped.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(seg_files(&dir, "rec_two"), files);
        for h in e.cameras.read().unwrap().iter().flatten() {
            h.active.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The safety net: a renamed camera whose recorder does not stop in time
    /// (or a file indexed late, e.g. after a restart) keeps its masks under
    /// its old name for what was recorded there until just after the
    /// rename: a camera added under that name never shows it unmasked, and
    /// the record is kept at the next start while such a file is on disk.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_file_of_a_renamed_camera_stays_masked_under_its_old_name() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-late-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        let e = engine(&dir);
        e.config.lock().unwrap().storage.segment_seconds = 6;
        white(&dir, "w");
        encoders_ready().await;
        let app = crate::web::build_router(e.clone());
        // "Screen" (masked on screen only) is writing a file; its worker does not stop.
        let h = e.camera(0).unwrap();
        h.parked.store(false, std::sync::atomic::Ordering::SeqCst);
        h.state.write().unwrap().ingest = "ffmpeg";
        let seg_dir = dir.join("cameras/screen/segments");
        std::fs::create_dir_all(&seg_dir).unwrap();
        let mut cfg = h.cfg.read().unwrap().clone();
        cfg.name = "Porch L".into();
        cfg.enabled = false;
        let e2 = e.clone();
        let t = std::time::Instant::now();
        assert!(tokio::task::spawn_blocking(move || e2.rename_and_update_camera(0, "Screen", cfg)).await.unwrap().unwrap());
        let took = t.elapsed();
        assert!(took >= crate::engine::RENAME_STOP_WAIT && took < crate::engine::RENAME_STOP_WAIT + std::time::Duration::from_secs(5), "waited, not for ever: {took:?}");
        // The file it went on writing, finished after the rename.
        let late = seg_dir.join(format!("screen-{}-seg000000.mp4", now_ms()));
        std::fs::copy(dir.join("rec/w.mp4"), &late).unwrap();
        // The next start, before anything indexed it: the record stays.
        let roots = vec![dir.clone()];
        let cams = e.config.lock().unwrap().cameras.clone();
        super::prune_records(&e.db, &cams, &roots, now_ms());
        assert_eq!(super::gone_masks(&e.db, "screen").unwrap().len(), 1, "a file on disk from before");
        // A camera without masks added under the old name; the file is found under it.
        let plain: crate::config::CameraConfig = toml::from_str("name = \"Screen\"\nurl = \"rtsp://127.0.0.1/x\"\nenabled = false\n").unwrap();
        e.add_camera_arc(plain);
        assert_eq!(e.db.import_final_camera_dirs(&[("screen".into(), String::new(), dir.clone())], 6, 0).unwrap(), 1);
        let row = e.db.segments_overlapping("screen", 0, i64::MAX / 2).unwrap().remove(0);
        all_masked(&app, &dir, "Screen", row.start_ms, "lt", "mk-l").await;
        let (s, _, b) = get(&app, &format!("/api/still?camera=Screen&at={}", row.start_ms + 2500), "mk-l").await;
        assert_eq!(s, StatusCode::OK);
        let img = grey(&b);
        assert!(img.get_pixel(40, 30).0[0] < 40 && img.get_pixel(240, 180).0[0] > 200, "its still");
        // What the new camera records later is its own (no masks).
        let later = now_ms() + 600_000;
        add_seg(&e, "screen", "w", later, 1);
        assert_eq!(super::footage_mask(&e.db, None, "screen", None, later, later + 6000, true).unwrap(), None);
        // Gone from the disk and the index: dropped at the next start.
        e.db.delete_segment(&row.path).unwrap();
        std::fs::remove_file(&late).unwrap();
        let cams = e.config.lock().unwrap().cameras.clone();
        super::prune_records(&e.db, &cams, &roots, now_ms());
        assert!(super::gone_masks(&e.db, "screen").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_found_on_disk_carry_masks_only_from_this_runs_masking_recorder() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let rec: crate::config::CameraConfig = toml::from_str("name = \"Porch\"\nurl = \"rtsp://192.0.2.10/s\"\nmask_mode = \"recording\"\nprivacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\n").unwrap();
        let rev = crate::privacy::masks_revision(&rec);
        let row = |p: &str, s: i64| crate::db::SegmentRow { path: p.into(), start_ms: s, end_ms: s + 1000, bytes: 1, video_frames: 0, kind: "legacy".into(), folder: String::new() };
        // Nothing noted (the start-up indexing before any camera starts): none.
        db.insert_scanned_segment("porch", &row("a", 1_000)).unwrap();
        super::note_level(&db, &rec, 10_000, false);
        let since = 10_000 + super::SETTLE_MS;
        db.insert_scanned_segment("porch", &row("b", since - 1500)).unwrap();
        db.insert_scanned_segment("porch", &row("c", since)).unwrap();
        db.insert_scanned_segment("other", &row("d", since)).unwrap();
        let revs = |c: &str, s: i64| db.segment_mask_revs(c, s, s + 1).unwrap();
        assert_eq!(revs("porch", 1_000), vec![None]);
        assert_eq!(revs("porch", since - 1500), vec![None], "begun before its recorder started");
        assert_eq!(revs("porch", since), vec![Some(rev.clone())]);
        assert_eq!(revs("other", since), vec![None], "another camera's");
        // Noted again with the same masks: from the same moment; at the screen level: none.
        super::note_level(&db, &rec, 50_000, false);
        db.insert_scanned_segment("porch", &row("e", since + 2_000)).unwrap();
        assert_eq!(revs("porch", since + 2_000), vec![Some(rev)]);
        let mut screen = rec.clone();
        screen.mask_mode = Some("screen".into());
        super::note_level(&db, &screen, 60_000, false);
        db.insert_scanned_segment("porch", &row("f", 100_000)).unwrap();
        assert_eq!(revs("porch", 100_000), vec![None]);
        // The masks it had are noted, and a camera without masks has no record.
        assert!(db.ui_get(super::SINCE_SCOPE, &format!("{}porch", super::KNOWN_PREFIX)).unwrap().is_some());
        super::note_level(&db, &named_cam("Porch"), 70_000, false);
        assert!(db.ui_get(super::SINCE_SCOPE, &format!("{}porch", super::KNOWN_PREFIX)).unwrap().is_none());
    }

    fn named_cam(name: &str) -> crate::config::CameraConfig {
        toml::from_str(&format!("name = {name:?}\nurl = \"rtsp://192.0.2.10/s\"\n")).unwrap()
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
        if !have_ffmpeg() {
            return;
        }
        let vf = crate::engine::clock_text(0, 4_000, &off, &|b| crate::engine::caption_drawtext("Cam", b));
        let o = ffmpeg(&["-v", "error", "-f", "lavfi", "-i", "color=c=white:s=320x240:r=10", "-t", "4", "-vf", &format!("{vf},scale=320:240"), "-pix_fmt", "gray", "-f", "rawvideo", "-"]);
        assert!(o.status.success(), "{vf}: {}", String::from_utf8_lossy(&o.stderr));
        let frame = 320 * 240;
        assert_eq!(o.stdout.len(), frame * 40);
        // The text's box (black, half see-through) is in every frame, before and after the change.
        for n in [5, 15, 25, 35] {
            let f = &o.stdout[n * frame..(n + 1) * frame];
            assert!(f[frame - 320 * 30..].iter().any(|&p| p < 200), "frame {n} has the text");
        }
    }

    /// An export stopped while a clip's masks are burned in stops that
    /// conversion; a conversion is never larger than it was allowed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_export_stops_its_conversion_and_a_burn_keeps_to_its_size() {
        let _one = SERIAL.lock().await;
        if !have_ffmpeg() {
            eprintln!("no ffmpeg here: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("amber-masked-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rec")).unwrap();
        let e = engine(&dir);
        let bytes = white(&dir, "w");
        let t0 = now_ms() - 3_600_000;
        add_seg(&e, "screen", "w", t0, bytes);
        let ev = crate::engine::Event { id: "mk-stop-1".into(), camera_id: 0, camera: "Screen".into(), start: t0 / 1000 + 1, end: t0 / 1000 + 4, score: 1.0, clip: None, thumb: None, objects: vec![], protect: false };
        e.db.insert_event(&ev).unwrap();
        encoders_ready().await;
        let (e2, ev2, zip) = (e.clone(), ev.clone(), dir.join("x.zip"));
        let (res, took) = tokio::task::spawn_blocking(move || {
            let n = std::sync::atomic::AtomicUsize::new(0);
            let t = std::time::Instant::now();
            // (Asked once before the clip: not yet; then stopped.)
            let r = e2.export_events_zip_to(&zip, &[ev2], &|| n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1);
            (r, t.elapsed())
        })
        .await
        .unwrap();
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
        assert!(took < std::time::Duration::from_secs(10), "{took:?}");
        let e3 = e.clone();
        let err = tokio::task::spawn_blocking(move || {
            let mask = super::event_download_mask(&e3, &ev).unwrap();
            let b = super::event_burn(&e3, &ev, mask, None).unwrap().unwrap();
            super::burn_to_file(&e3, b, super::Priority::Export, &|| false, 2_000).err().unwrap()
        })
        .await
        .unwrap();
        assert!(err.downcast_ref::<super::BurnTooLarge>().is_some(), "{err:#}");
        let left: Vec<_> = std::fs::read_dir(dir.join("tmp")).map(|d| d.flatten().map(|x| x.file_name()).collect()).unwrap_or_default();
        assert!(left.iter().all(|n| !n.to_string_lossy().starts_with("burn-")), "no converted file left: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
