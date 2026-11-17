//! Settings changes made at start for privacy masks and burned-in text,
//! and what Status says about them.
//!
//! - 0.11.2: AmberSight no longer makes privacy masks (cameras have their
//!   own). Each camera's masks become ignore zones and areas that don't
//!   notify and are cleared, at every start (an older config put back is
//!   changed again); Status names the cameras for a while
//!   (`masks_removed_0_11_2`). The records 0.11.1 kept about masks are
//!   deleted once.
//! - 0.11.1: text burned into the recordings of cameras without masks is
//!   drawn on screen instead (`overlay = false`), once: the database
//!   remembers it (`overlay_migrated_0_11_1`), so an owner who turns it
//!   back on (or a config set back from an older copy) is not changed again.

use crate::config::{Config, MasksMoved};
use crate::db::Db;

/// ui_kv scope and key that remember the overlay change (and for whom).
pub const KV_SCOPE: &str = "system";
pub const OVERLAY_KV: &str = "overlay_migrated_0_11_1";
/// ... and the one that remembers which cameras' masks became ignore zones
/// (0.11.2: AmberSight no longer makes privacy masks), for Status.
pub const MASKS_KV: &str = "masks_removed_0_11_2";
/// How long Status keeps the note about masks that stopped hiding anything.
const MASKS_NOTE_DAYS: i64 = 90;
/// How long Status keeps the note about the overlay change.
const NOTE_DAYS: i64 = 30;
/// Records 0.11.1 kept about masks (in KV_SCOPE), unread since 0.11.2:
/// "masked from" times, masks of removed cameras, the masks a camera was
/// last seen with, and the note about "share without the mask"
/// confirmations. Deleted once, at a start (`OLD_MASKS_DELETED_KV`).
const OLD_MASK_RECORDS: &[&str] = &["mask_rec_since:", "mask_gone:", "mask_known:", "share_unmasked_rev_0_11_1"];
/// Remembers (in KV_SCOPE) that OLD_MASK_RECORDS were deleted.
pub const OLD_MASKS_DELETED_KV: &str = "old_mask_records_deleted_0_11_2";

/// What was changed, for the log.
#[derive(Debug, Default, PartialEq)]
pub struct Changed {
    /// Cameras whose privacy masks became ignore zones (0.11.2), and what
    /// else changed with them.
    pub masks_moved: Vec<MasksMoved>,
    pub overlay_off: Vec<String>,
}

impl Changed {
    /// (Only tests ask: the settings are saved when their text changed.)
    #[cfg(test)]
    pub fn any(&self) -> bool {
        !self.masks_moved.is_empty() || !self.overlay_off.is_empty()
    }
}

/// Run both changes on `cfg` and write it (`save`) when something changed.
/// The overlay one runs only if the database has not recorded it yet; each
/// is recorded only once the config is written (or nothing needed writing),
/// so Status never says something changed that is not so in the file.
/// `now_ms`: when, for the Status notes.
pub fn migrate(db: &Db, cfg: &mut Config, now_ms: i64, save: &dyn Fn(&Config) -> anyhow::Result<()>) -> Changed {
    delete_old_mask_records(db);
    // (Written when anything changed, also what is not reported: masks that
    // hid nothing, a pushed camera's text setting.)
    let before = cfg.to_toml().ok();
    let masks_moved = cfg.move_masks_to_ignore();
    let mut overlay_off = Vec::new();
    let mut record = false;
    match db.ui_get(KV_SCOPE, OVERLAY_KV) {
        // A database made at this start is a new install, not an upgrade:
        // text the owner set to burn in before the first start stays so.
        Ok(None) if db.created_fresh() => record = true,
        Ok(None) => {
            overlay_off = cfg.migrate_overlay();
            record = true;
        }
        Ok(Some(_)) => {}
        Err(e) => tracing::warn!("settings: could not read whether burned-in text was changed ({e:#}); left as it is"),
    }
    let undo = |cfg: &mut Config, overlay_off: &mut Vec<String>| {
        for c in cfg.cameras.iter_mut().filter(|c| overlay_off.contains(&c.name)) {
            c.overlay = true;
        }
        overlay_off.clear();
    };
    // (Settings that could not be put into words count as changed: the save
    // then fails and nothing is noted that the file does not have.)
    let changed_text = match (&before, cfg.to_toml()) {
        (Some(b), Ok(a)) => *b != a,
        _ => true,
    };
    if changed_text {
        if let Err(e) = save(cfg) {
            // Not written: the text stays burned in, and both changes are
            // tried again next start (the masks are ignore zones in memory).
            tracing::warn!("settings: could not save the privacy mask and text settings ({e:#})");
            undo(cfg, &mut overlay_off);
            // The text of a camera whose masks moved stays as the file has it
            // too (pushed and battery cameras included); the masks themselves
            // are not made any more either way, so Status names those cameras
            // now, not only after a later start. (Sharing stays off for this
            // run, and is turned off again next start.)
            let mut masks_moved = masks_moved;
            for m in masks_moved.iter_mut() {
                if let Some(c) = cfg.cameras.iter_mut().find(|c| c.name == m.name) {
                    c.overlay = m.overlay_was;
                }
                m.text_off = false;
            }
            note_masks(db, &masks_moved, now_ms);
            return Changed { masks_moved, overlay_off };
        }
    }
    note_masks(db, &masks_moved, now_ms);
    if record {
        let v = serde_json::json!({ "at": now_ms, "cameras": overlay_off });
        if let Err(e) = db.ui_put(KV_SCOPE, OVERLAY_KV, &v.to_string()) {
            // Not recorded: the change would be made again next time
            // (after the owner may have turned it back on), so not now.
            tracing::warn!("settings: could not record the change of burned-in text ({e:#}); left as it is");
            if !overlay_off.is_empty() {
                undo(cfg, &mut overlay_off);
                if let Err(e) = save(cfg) {
                    tracing::warn!("settings: could not save the burned-in text back ({e:#})");
                }
            }
        }
    }
    Changed { masks_moved, overlay_off }
}

/// Delete the records 0.11.1 kept about masks, once: the database
/// remembers it, and tries again next start if a delete failed.
fn delete_old_mask_records(db: &Db) {
    if !matches!(db.ui_get(KV_SCOPE, OLD_MASKS_DELETED_KV), Ok(None)) {
        return;
    }
    let mut all = true;
    for prefix in OLD_MASK_RECORDS {
        if let Err(e) = db.ui_delete_prefix(KV_SCOPE, prefix) {
            tracing::debug!("settings: old privacy mask records: {e:#}");
            all = false;
        }
    }
    if all {
        if let Err(e) = db.ui_put(KV_SCOPE, OLD_MASKS_DELETED_KV, "1") {
            tracing::debug!("settings: could not record that the old privacy mask records are deleted: {e:#}");
        }
    }
}

/// One camera in the Status note: what changed, and when it was first noted.
struct Noted {
    m: MasksMoved,
    at: i64,
}

/// Note for Status which cameras' masks became ignore zones (added to what an
/// earlier start noted, so a camera changed later by hand does not hide the
/// earlier ones). A camera noted before keeps its time (each goes from the
/// note on its own) and what was said about it, unless its note had gone
/// already or this start has something new to say about it (its time is
/// then now).
fn note_masks(db: &Db, moved: &[MasksMoved], now_ms: i64) {
    if moved.is_empty() {
        return;
    }
    let mut cams = masks_note_cameras(db);
    for m in moved {
        match cams.iter_mut().find(|n| n.m.name == m.name) {
            Some(n) if now_ms - n.at > MASKS_NOTE_DAYS * 86_400_000 => *n = Noted { m: m.clone(), at: now_ms },
            Some(n) => {
                let new = (m.text_off && !n.m.text_off) || (m.share_off && !n.m.share_off) || m.no_zone > n.m.no_zone || m.no_area > n.m.no_area;
                if new {
                    n.at = now_ms;
                }
                n.m.text_off |= m.text_off;
                n.m.share_off |= m.share_off;
                n.m.no_zone = n.m.no_zone.max(m.no_zone);
                n.m.no_area = n.m.no_area.max(m.no_area);
            }
            None => cams.push(Noted { m: m.clone(), at: now_ms }),
        }
    }
    let v = serde_json::json!({
        "cameras": cams.iter().map(|n| serde_json::json!({ "name": n.m.name, "text_off": n.m.text_off, "share_off": n.m.share_off, "no_zone": n.m.no_zone, "no_area": n.m.no_area, "at": n.at })).collect::<Vec<_>>(),
    });
    if let Err(e) = db.ui_put(KV_SCOPE, MASKS_KV, &v.to_string()) {
        tracing::warn!("settings: could not record which cameras' privacy masks became ignore zones ({e:#})");
    }
}

/// The cameras an earlier start noted (see MASKS_KV). (A note of an
/// earlier 0.11.2 build has one time for all.)
fn masks_note_cameras(db: &Db) -> Vec<Noted> {
    let Some(v) = db.ui_get(KV_SCOPE, MASKS_KV).ok().flatten().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else {
        return Vec::new();
    };
    let all_at = v["at"].as_i64().unwrap_or(0);
    let Some(a) = v["cameras"].as_array() else { return Vec::new() };
    a.iter()
        .filter_map(|c| {
            let m = MasksMoved {
                name: c["name"].as_str()?.to_string(),
                text_off: c["text_off"].as_bool().unwrap_or(false),
                share_off: c["share_off"].as_bool().unwrap_or(false),
                no_zone: c["no_zone"].as_u64().unwrap_or(0) as usize,
                no_area: c["no_area"].as_u64().unwrap_or(0) as usize,
                ..Default::default()
            };
            Some(Noted { m, at: c["at"].as_i64().unwrap_or(all_at) })
        })
        .collect()
}

/// "A", "A and B", "A, B and C".
fn and_list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn masks_note_text(cams: &[MasksMoved]) -> String {
    let names: Vec<&str> = cams.iter().map(|m| m.name.as_str()).collect();
    let pick = |f: fn(&MasksMoved) -> bool| -> Vec<&str> { cams.iter().filter(|m| f(m)).map(|m| m.name.as_str()).collect() };
    let one = names.len() == 1;
    let its = if one { "its" } else { "their" };
    let mut s = format!(
        "AmberSight no longer makes privacy masks: {} show{} the areas {its} masks hid, live, in pictures, in downloads and in {its} recordings (older ones too, unless the masks were burned into them); linked servers and Share (RTSP) streams get the full picture too. \
         Set the camera's own privacy mask (Reolink, Dahua/Amcrest and Hikvision cameras have one). \
         Motion in those areas still doesn't count (they are ignore zones now), and objects there are still found by the AI and in search, but ordinary object alerts there are not sent (they are areas that don't notify now)",
        and_list(&names),
        if one { "s" } else { "" },
    );
    let text = pick(|m| m.text_off);
    if !text.is_empty() {
        s.push_str(&format!(". {} now draw{} the camera's text on screen instead of re-encoding; turn \"Also burn it into recordings\" back on if you need it in the files", and_list(&text), if text.len() == 1 { "s" } else { "" }));
    }
    let share = pick(|m| m.share_off);
    if !share.is_empty() {
        s.push_str(&format!(". Sharing (RTSP) was turned off for {}: {} picture now includes the areas the masks hid; turn Share back on if you want it shared", and_list(&share), if share.len() == 1 { "its" } else { "their" }));
    }
    for m in cams.iter().filter(|m| m.no_zone > 0) {
        s.push_str(&format!(". {} of {}'s masks had no room as ignore zones (motion there now counts): draw them again if you need them", m.no_zone, m.name));
    }
    for m in cams.iter().filter(|m| m.no_area > 0) {
        s.push_str(&format!(". {} of {}'s masks had no room as areas that don't notify", m.no_area, m.name));
    }
    s
}

/// The Status note about masks that became ignore zones: each camera for
/// MASKS_NOTE_DAYS after it was first noted.
pub fn masks_note(db: &Db, now_ms: i64) -> Option<String> {
    let cams: Vec<MasksMoved> = masks_note_cameras(db).into_iter().filter(|n| now_ms - n.at <= MASKS_NOTE_DAYS * 86_400_000).map(|n| n.m).collect();
    (!cams.is_empty()).then(|| masks_note_text(&cams))
}

/// Log lines for what `migrate` changed.
pub fn log_lines(c: &Changed) -> Vec<String> {
    let mut out = Vec::new();
    if !c.masks_moved.is_empty() {
        out.push(format!("privacy masks: {}", masks_note_text(&c.masks_moved)));
    }
    if !c.overlay_off.is_empty() {
        out.push(format!("burned-in text: {}", overlay_note_text(&c.overlay_off)));
    }
    out
}

fn overlay_note_text(names: &[String]) -> String {
    let n = names.len();
    format!(
        "{n} camera{} now draw{} {} text on screen instead of re-encoding: {}; turn \"Also burn it into recordings\" back on per camera if you need it in the files",
        if n == 1 { "" } else { "s" },
        if n == 1 { "s" } else { "" },
        if n == 1 { "its" } else { "their" },
        names.join(", ")
    )
}

/// The Status note about the overlay change, for a while after it.
pub fn overlay_note(db: &Db, now_ms: i64) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&db.ui_get(KV_SCOPE, OVERLAY_KV).ok()??).ok()?;
    let at = v["at"].as_i64()?;
    if now_ms - at > NOTE_DAYS * 86_400_000 {
        return None;
    }
    let names: Vec<String> = v["cameras"].as_array()?.iter().filter_map(|n| n.as_str().map(str::to_string)).collect();
    (!names.is_empty()).then(|| overlay_note_text(&names))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cams: &str) -> Config {
        toml::from_str(cams).unwrap()
    }

    /// Cameras as 0.11.1 saved them.
    const CAMS: &str = r#"
[[cameras]]
name = "Porch"
url = "rtsp://192.0.2.1/a"
overlay = true
mask_mode = "recording"
privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }, { x = 0.5, y = 0.5, w = 0.1, h = 0.1 }]
ignore_zones = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]

[[cameras]]
name = "Side Yard"
url = "push://sideyard00001"
mask_mode = "screen"
privacy_masks = [{ x = 0.6, y = 0.6, w = 0.2, h = 0.2 }]

[[cameras]]
name = "Text"
url = "rtsp://192.0.2.2/a"
overlay = true

[[cameras]]
name = "Plain"
url = "rtsp://192.0.2.3/a"
mask_mode = "screen"
"#;

    fn cam<'a>(c: &'a Config, n: &str) -> &'a crate::config::CameraConfig {
        c.cameras.iter().find(|c| c.name == n).unwrap()
    }

    fn r(x: f64, y: f64, w: f64, h: f64) -> crate::config::ZoneRect {
        crate::config::ZoneRect { x, y, w, h }
    }

    /// Each moved camera's name and whether its text was turned off.
    fn moved(ch: &Changed) -> Vec<(&str, bool)> {
        ch.masks_moved.iter().map(|m| (m.name.as_str(), m.text_off)).collect()
    }

    #[test]
    fn masks_become_ignore_zones_once_each_and_their_text_goes_on_screen() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg(CAMS);
        let ch = migrate(&db, &mut c, 1_000, &|_| Ok(()));
        assert_eq!(moved(&ch), vec![("Porch", true), ("Side Yard", false)]);
        // The mask that was an ignore zone already is there once.
        assert_eq!(cam(&c, "Porch").ignore_zones, vec![r(0.1, 0.1, 0.2, 0.2), r(0.5, 0.5, 0.1, 0.1)]);
        assert_eq!(cam(&c, "Side Yard").ignore_zones, vec![r(0.6, 0.6, 0.2, 0.2)]);
        // And areas that don't notify: still detected and searchable, ordinary object alerts not sent.
        let sq = |x: f64, y: f64, w: f64, h: f64| crate::config::ZonePoly { points: vec![[x, y], [x + w, y], [x + w, y + h], [x, y + h]] };
        assert_eq!(cam(&c, "Porch").alert_ignore_polys, vec![sq(0.1, 0.1, 0.2, 0.2), sq(0.5, 0.5, 0.1, 0.1)]);
        assert_eq!(cam(&c, "Side Yard").alert_ignore_polys, vec![sq(0.6, 0.6, 0.2, 0.2)]);
        for c in &c.cameras {
            assert!(c.privacy_masks.is_empty() && c.mask_mode.is_none(), "{}", c.name);
        }
        // The text that was burned in for its masks is drawn on screen.
        assert!(!cam(&c, "Porch").overlay);
        assert_eq!(ch.overlay_off, vec!["Text".to_string()]);
        // Saved and loaded again, nothing of the masks is written.
        let text = c.to_toml().unwrap();
        assert!(!text.contains("privacy_masks") && !text.contains("mask_mode"), "{text}");
        // Status names them, says the text, and goes after a while.
        let note = masks_note(&db, 2_000).unwrap();
        assert!(note.contains("Porch and Side Yard show the areas their masks hid"), "{note}");
        assert!(note.contains("Porch now draws the camera's text on screen"), "{note}");
        assert!(note.contains("camera's own privacy mask"), "{note}");
        assert_eq!(masks_note(&db, 1_000 + 91 * 86_400_000), None);
        assert_eq!(log_lines(&ch).len(), 2);
    }

    #[test]
    fn running_again_changes_nothing() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg(CAMS);
        migrate(&db, &mut c, 1_000, &|_| Ok(()));
        let before = c.to_toml().unwrap();
        let saved = std::cell::Cell::new(false);
        let again = migrate(&db, &mut c, 5_000, &|_| {
            saved.set(true);
            Ok(())
        });
        assert!(!again.any(), "{again:?}");
        assert!(!saved.get(), "nothing to write");
        assert_eq!(c.to_toml().unwrap(), before);
        // The note keeps its time.
        assert_eq!(masks_note(&db, 1_000 + 90 * 86_400_000 + 1), None);
    }

    #[test]
    fn the_note_adds_cameras_from_a_later_start() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg(CAMS);
        migrate(&db, &mut c, 1_000, &|_| Ok(()));
        // A config from before put back with masks on another camera.
        let mut later = cfg("[[cameras]]\nname = \"Shed\"\nurl = \"rtsp://192.0.2.4/a\"\nprivacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\n");
        let ch = migrate(&db, &mut later, 2_000, &|_| Ok(()));
        assert_eq!(moved(&ch), vec![("Shed", false)]);
        let note = masks_note(&db, 3_000).unwrap();
        assert!(note.contains("Porch, Side Yard and Shed show"), "{note}");
        assert!(note.contains("Porch now draws"), "{note}");
    }

    #[test]
    fn a_config_that_cannot_be_saved_is_noted_at_once_and_tried_again() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg(CAMS);
        let ch = migrate(&db, &mut c, 1_000, &|_| anyhow::bail!("read-only"));
        assert_eq!(ch.masks_moved.len(), 2, "moved in memory: nothing is hidden by AmberSight any more");
        assert!(ch.masks_moved.iter().all(|m| !m.text_off), "no text turned off: {:?}", ch.masks_moved);
        assert!(ch.overlay_off.is_empty());
        assert!(cam(&c, "Text").overlay, "still burned in, as the file says");
        assert!(cam(&c, "Porch").overlay, "a masked camera's text too");
        // Status says so now: the masks are not made this run either.
        let note = masks_note(&db, 2_000).unwrap();
        assert!(note.contains("Porch and Side Yard show") && !note.contains("draws"), "{note}");
        assert_eq!(overlay_note(&db, 2_000), None);
        // Next start, the file as it was, now saved: noted then.
        let mut again = cfg(CAMS);
        let ch = migrate(&db, &mut again, 3_000, &|_| Ok(()));
        assert_eq!(ch.masks_moved.len(), 2);
        assert!(masks_note(&db, 4_000).unwrap().contains("Porch and Side Yard"));
    }

    /// Not saved: every moved camera's text setting is put back as the file
    /// has it, a pushed or battery camera's too; sharing stays off.
    #[test]
    fn a_failed_save_puts_back_the_text_of_every_moved_camera() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mask = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\nshare = true\n";
        let mut c = cfg(&format!(
            "[[cameras]]\nname = \"Porch\"\nurl = \"push://porch0000001\"\noverlay = true\n{mask}\n[[cameras]]\nname = \"Shed\"\nurl = \"rtsp://192.0.2.2/a\"\noverlay = true\n{mask}\n[[cameras]]\nname = \"Side Yard\"\nurl = \"rtsp://192.0.2.3/a\"\n{mask}"
        ));
        let ch = migrate(&db, &mut c, 1_000, &|_| anyhow::bail!("read-only"));
        assert!(cam(&c, "Porch").overlay && cam(&c, "Shed").overlay, "put back");
        assert!(!cam(&c, "Side Yard").overlay, "as it was");
        assert!(ch.masks_moved.iter().all(|m| !m.text_off && m.share_off), "{:?}", ch.masks_moved);
        assert!(c.cameras.iter().all(|c| !c.share), "sharing stays off this run");
    }

    #[test]
    fn a_fresh_install_without_masks_has_no_note() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.created_fresh());
        let mut c: Config = toml::from_str("[[cameras]]\nname = \"Text Cam\"\nurl = \"rtsp://192.0.2.1/x\"\noverlay = true\n").unwrap();
        let ch = migrate(&db, &mut c, 0, &|_| Ok(()));
        assert!(!ch.any());
        assert!(c.cameras[0].overlay, "text set to burn in before the first start stays so");
        assert!(db.ui_get(KV_SCOPE, OVERLAY_KV).unwrap().is_some(), "and is never changed later either");
        assert_eq!(masks_note(&db, 1), None);
        assert_eq!(overlay_note(&db, 1), None);
    }

    #[test]
    fn old_mask_records_are_deleted_once() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        for k in ["mask_rec_since:porch", "mask_gone:shed|123", "mask_known:porch", "share_unmasked_rev_0_11_1", OVERLAY_KV] {
            db.ui_put(KV_SCOPE, k, "{}").unwrap();
        }
        migrate(&db, &mut cfg(CAMS), 1_000, &|_| Ok(()));
        let left: Vec<String> = db.ui_scope(KV_SCOPE).unwrap().into_iter().map(|(k, _)| k).collect();
        assert!(left.iter().all(|k| !k.starts_with("mask_rec_since:") && !k.starts_with("mask_gone:") && !k.starts_with("mask_known:") && !k.starts_with("share_unmasked")), "{left:?}");
        assert!(left.contains(&OVERLAY_KV.to_string()));
        assert!(db.ui_get(KV_SCOPE, OLD_MASKS_DELETED_KV).unwrap().is_some(), "remembered");
        // Not looked for again at later starts.
        db.ui_put(KV_SCOPE, "mask_known:later", "{}").unwrap();
        migrate(&db, &mut cfg(CAMS), 2_000, &|_| Ok(()));
        assert!(db.ui_get(KV_SCOPE, "mask_known:later").unwrap().is_some());
    }

    /// Every camera that had masks stops sharing (RTSP), confirmed in 0.11.1
    /// or not: a confirmation may have been for other masks, and its
    /// picture now shows what the masks hid.
    #[test]
    fn sharing_of_a_camera_with_masks_is_turned_off() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mask = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\n";
        let mut c = cfg(&format!(
            "[[cameras]]\nname = \"Porch\"\nurl = \"rtsp://192.0.2.1/a\"\nshare = true\n{mask}\n\
             [[cameras]]\nname = \"Shed\"\nurl = \"rtsp://192.0.2.2/a\"\nshare = true\nshare_unmasked = true\nshare_unmasked_rev = \"0123456789ab\"\n{mask}\n\
             [[cameras]]\nname = \"Side Yard\"\nurl = \"rtsp://192.0.2.3/a\"\nshare = true\n\n\
             [[cameras]]\nname = \"Garden\"\nurl = \"rtsp://192.0.2.4/a\"\n{mask}"
        ));
        let ch = migrate(&db, &mut c, 1_000, &|_| Ok(()));
        assert!(!cam(&c, "Porch").share && !cam(&c, "Shed").share, "off, confirmed or not");
        assert!(cam(&c, "Side Yard").share, "no masks: left on");
        assert_eq!(ch.masks_moved.iter().map(|m| (m.name.as_str(), m.share_off)).collect::<Vec<_>>(), vec![("Porch", true), ("Shed", true), ("Garden", false)]);
        let text = c.to_toml().unwrap();
        assert!(!text.contains("share_unmasked"), "{text}");
        let note = masks_note(&db, 2_000).unwrap();
        assert!(note.contains(". Sharing (RTSP) was turned off for Porch and Shed: their picture now includes the areas the masks hid; turn Share back on if you want it shared"), "{note}");
        assert!(note.contains("linked servers and Share (RTSP) streams get the full picture"), "{note}");
        assert!(note.contains("still found by the AI") && note.contains("ordinary object alerts there are not sent") && note.contains("still doesn't count"), "{note}");
        assert!(!note.contains("never send"), "{note}");
        assert!(note.contains("older ones too, unless the masks were burned into them"), "{note}");
        // The owner turns it back on: left on at the next start.
        c.cameras[0].share = true;
        assert!(!migrate(&db, &mut c, 3_000, &|_| Ok(())).any());
        assert!(cam(&c, "Porch").share);
    }

    /// Masks with no size or outside the picture go; the rest are cut to
    /// it; each list is filled while it has room, on its own.
    #[test]
    fn masks_are_kept_inside_the_picture_and_the_limits() {
        use crate::config::{MAX_ALERT_IGNORE_AREAS, MAX_ZONES};
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg("[[cameras]]\nname = \"Porch\"\nurl = \"rtsp://192.0.2.1/a\"\n[[cameras]]\nname = \"Shed\"\nurl = \"rtsp://192.0.2.2/a\"\n[[cameras]]\nname = \"Side Yard\"\nurl = \"rtsp://192.0.2.3/a\"\n");
        // Porch: 20 ignore zones of its own and 21 masks.
        let p = &mut c.cameras[0];
        p.ignore_zones = (0..20).map(|i| r(0.01 * i as f64, 0.0, 0.01, 0.01)).collect();
        p.privacy_masks = (0..21).map(|i| r(0.04 * i as f64, 0.5, 0.03, 0.03)).collect();
        // Shed: no size, outside the picture, partly outside.
        let s = &mut c.cameras[1];
        s.privacy_masks = vec![r(0.2, 0.2, 0.0, 0.3), r(1.2, 0.1, 0.2, 0.2), r(-0.5, 0.1, 0.3, 0.2), r(0.7, 0.9, 0.5, 0.5), r(0.1, f64::NAN, 0.1, 0.1)];
        // Side Yard: 20 masks, no zones.
        c.cameras[2].privacy_masks = (0..20).map(|i| r(0.04 * i as f64, 0.2, 0.03, 0.03)).collect();
        let ch = migrate(&db, &mut c, 1_000, &|_| Ok(()));
        let porch = cam(&c, "Porch");
        assert_eq!(porch.ignore_zones.len(), MAX_ZONES);
        assert_eq!(porch.alert_ignore_polys.len(), MAX_ALERT_IGNORE_AREAS);
        assert_eq!((ch.masks_moved[0].no_zone, ch.masks_moved[0].no_area), (9, 9), "{:?}", ch.masks_moved);
        let shed = cam(&c, "Shed");
        let near = |a: &[f64], b: &[f64]| a.len() == b.len() && a.iter().zip(b).all(|(p, q)| (p - q).abs() < 1e-9);
        assert_eq!(shed.ignore_zones.len(), 1, "{:?}", shed.ignore_zones);
        let z = &shed.ignore_zones[0];
        assert!(near(&[z.x, z.y, z.w, z.h], &[0.7, 0.9, 0.3, 0.1]), "cut to the picture: {z:?}");
        assert_eq!(shed.alert_ignore_polys.len(), 1);
        let corners: Vec<f64> = shed.alert_ignore_polys[0].points.iter().flatten().copied().collect();
        assert!(near(&corners, &[0.7, 0.9, 1.0, 0.9, 1.0, 1.0, 0.7, 1.0]), "{corners:?}");
        assert!(corners.iter().all(|v| (0.0..=1.0).contains(v)), "inside the picture as saving needs: {corners:?}");
        assert_eq!((ch.masks_moved[1].no_zone, ch.masks_moved[1].no_area), (0, 0), "nothing hidden by the others: not counted");
        let side = cam(&c, "Side Yard");
        assert_eq!((side.ignore_zones.len(), side.alert_ignore_polys.len()), (20, 12), "every one an ignore zone, the first 12 areas too");
        assert_eq!((ch.masks_moved[2].no_zone, ch.masks_moved[2].no_area), (0, 8));
        let note = masks_note(&db, 2_000).unwrap();
        assert!(note.contains(". 9 of Porch's masks had no room as ignore zones (motion there now counts): draw them again if you need them"), "{note}");
        assert!(note.contains(". 9 of Porch's masks had no room as areas that don't notify"), "{note}");
        assert!(note.contains(". 8 of Side Yard's masks had no room as areas that don't notify"), "{note}");
        assert!(!note.contains("of Shed's") && !note.contains("of Side Yard's masks had no room as ignore"), "{note}");
        assert!(log_lines(&ch)[0].contains("9 of Porch's masks"));
    }

    /// A mask that is an ignore zone already needs only an area: with the
    /// areas full, it is not reported as lost from the ignore zones.
    #[test]
    fn a_mask_already_an_ignore_zone_only_needs_an_area() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg("[[cameras]]\nname = \"Porch\"\nurl = \"rtsp://192.0.2.1/a\"\n");
        let p = &mut c.cameras[0];
        p.ignore_zones = vec![r(0.1, 0.1, 0.2, 0.2)];
        p.alert_ignore_polys = (0..12).map(|i| crate::config::ZonePoly { points: vec![[0.05 * i as f64, 0.8], [0.05 * i as f64 + 0.04, 0.8], [0.05 * i as f64, 0.9]] }).collect();
        p.privacy_masks = vec![r(0.1, 0.1, 0.2, 0.2)];
        let ch = migrate(&db, &mut c, 1_000, &|_| Ok(()));
        assert_eq!((ch.masks_moved[0].no_zone, ch.masks_moved[0].no_area), (0, 1));
        assert_eq!(cam(&c, "Porch").ignore_zones.len(), 1);
        let note = masks_note(&db, 2_000).unwrap();
        assert!(note.contains("1 of Porch's masks had no room as areas that don't notify") && !note.contains("as ignore zones"), "{note}");
    }

    /// Masks that hid nothing (no size, outside the picture): the camera is
    /// cleaned, not named, and its sharing and text are left as they are.
    #[test]
    fn a_camera_whose_masks_hid_nothing_is_only_cleaned() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg("[[cameras]]\nname = \"Porch\"\nurl = \"rtsp://192.0.2.1/a\"\nshare = true\noverlay = true\nmask_mode = \"screen\"\nprivacy_masks = [{ x = 0.2, y = 0.2, w = 0.0, h = 0.1 }, { x = 1.5, y = 0.1, w = 0.1, h = 0.1 }]\n");
        db.ui_put(KV_SCOPE, OVERLAY_KV, "{}").unwrap();
        let saved = std::cell::Cell::new(false);
        let ch = migrate(&db, &mut c, 1_000, &|_| {
            saved.set(true);
            Ok(())
        });
        assert!(ch.masks_moved.is_empty(), "{ch:?}");
        assert!(saved.get(), "cleaned in the file too");
        let p = cam(&c, "Porch");
        assert!(p.privacy_masks.is_empty() && p.mask_mode.is_none() && p.ignore_zones.is_empty() && p.alert_ignore_polys.is_empty());
        assert!(p.share && p.overlay);
        assert_eq!(masks_note(&db, 2_000), None);
    }

    /// A camera noted again starts its 90 days again when its note had
    /// gone, or when there is something new to say about it.
    #[test]
    fn a_camera_noted_again_with_something_new_starts_its_time_again() {
        const DAY: i64 = 86_400_000;
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let porch = |extra: &str| cfg(&format!("[[cameras]]\nname = \"Porch\"\nurl = \"rtsp://192.0.2.1/a\"\nprivacy_masks = [{{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }}]\n{extra}"));
        migrate(&db, &mut porch(""), 1_000, &|_| Ok(()));
        // Day 50: the same again: keeps its time.
        migrate(&db, &mut porch(""), 1_000 + 50 * DAY, &|_| Ok(()));
        assert_eq!(masks_note(&db, 1_000 + 91 * DAY), None);
        // Day 60 (a fresh start of the list): now with sharing on: new, so 90 days from now.
        let db = Db::open_in_memory().unwrap().as_upgraded();
        migrate(&db, &mut porch(""), 1_000, &|_| Ok(()));
        migrate(&db, &mut porch("share = true\n"), 1_000 + 60 * DAY, &|_| Ok(()));
        let note = masks_note(&db, 1_000 + 140 * DAY).unwrap();
        assert!(note.contains("Sharing (RTSP) was turned off for Porch"), "{note}");
        // Gone, then noted again: back for 90 days.
        migrate(&db, &mut porch(""), 1_000 + 200 * DAY, &|_| Ok(()));
        assert!(masks_note(&db, 1_000 + 250 * DAY).is_some());
        assert_eq!(masks_note(&db, 1_000 + 291 * DAY), None);
    }

    /// A pushed or battery camera's text was never burned into its
    /// recordings: nothing to say about it.
    #[test]
    fn text_of_a_pushed_or_battery_camera_is_not_reported() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mask = "privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\noverlay = true\n";
        let mut c = cfg(&format!(
            "[[cameras]]\nname = \"Porch\"\nurl = \"push://porch0000001\"\n{mask}\n[[cameras]]\nname = \"Shed\"\nurl = \"baichuan://192.0.2.5?uid=95270000ABCDEFGH\"\n{mask}\n[[cameras]]\nname = \"Side Yard\"\nurl = \"rtsp://192.0.2.6/a\"\n{mask}"
        ));
        let ch = migrate(&db, &mut c, 1_000, &|_| Ok(()));
        assert_eq!(moved(&ch), vec![("Porch", false), ("Shed", false), ("Side Yard", true)]);
        assert!(c.cameras.iter().all(|c| !c.overlay), "turned off all the same");
        let note = masks_note(&db, 2_000).unwrap();
        assert!(note.contains(". Side Yard now draws the camera's text"), "{note}");
        assert!(!note.contains("Porch now draws") && !note.contains("Shed now draws"), "{note}");
    }

    /// Each camera stays in the note for its own 90 days: a later start
    /// that moves another camera's masks neither starts them again nor
    /// ends them early.
    #[test]
    fn each_camera_leaves_the_note_on_its_own() {
        const DAY: i64 = 86_400_000;
        let db = Db::open_in_memory().unwrap().as_upgraded();
        migrate(&db, &mut cfg(CAMS), 1_000, &|_| Ok(()));
        let shed = "[[cameras]]\nname = \"Shed\"\nurl = \"rtsp://192.0.2.4/a\"\nprivacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]\n";
        migrate(&db, &mut cfg(shed), 1_000 + 60 * DAY, &|_| Ok(()));
        // An older config with Porch's masks put back, later still.
        migrate(&db, &mut cfg(CAMS), 1_000 + 80 * DAY, &|_| Ok(()));
        let note = masks_note(&db, 1_000 + 85 * DAY).unwrap();
        assert!(note.contains("Porch, Side Yard and Shed show"), "{note}");
        let note = masks_note(&db, 1_000 + 95 * DAY).unwrap();
        assert!(note.starts_with("AmberSight no longer makes privacy masks: Shed shows the areas its masks hid"), "Porch and Side Yard kept their time: {note}");
        assert_eq!(masks_note(&db, 1_000 + 151 * DAY), None);
    }

    /// A note written by an earlier 0.11.2 build (one time for all) still reads.
    #[test]
    fn a_note_with_one_time_for_all_still_reads() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        db.ui_put(KV_SCOPE, MASKS_KV, r#"{"at":1000,"cameras":[{"name":"Porch","text_off":true}]}"#).unwrap();
        assert!(masks_note(&db, 2_000).unwrap().contains("Porch now draws"));
        assert_eq!(masks_note(&db, 1_000 + 91 * 86_400_000), None);
    }

    #[test]
    fn text_without_masks_is_drawn_once_and_left_when_turned_back_on() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg(CAMS);
        migrate(&db, &mut c, 1_000, &|_| Ok(()));
        let note = overlay_note(&db, 2_000).unwrap();
        assert!(note.starts_with("1 camera now draws its text on screen instead of re-encoding: Text;"), "{note}");
        assert_eq!(overlay_note(&db, 1_000 + 31 * 86_400_000), None, "the note goes after a while");
        // The owner turns the text back on; next start leaves it.
        c.cameras.iter_mut().find(|c| c.name == "Text").unwrap().overlay = true;
        assert!(!migrate(&db, &mut c, 5_000, &|_| Ok(())).any());
        assert!(cam(&c, "Text").overlay);
    }

    /// The 0.11.1 change names only cameras whose text was really burned
    /// in; a pushed or battery camera's setting is turned off all the same.
    #[test]
    fn the_text_change_names_no_pushed_or_battery_camera() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg("[[cameras]]\nname = \"Porch\"\nurl = \"push://porch0000001\"\noverlay = true\n[[cameras]]\nname = \"Shed\"\nurl = \"baichuan://192.0.2.5?uid=95270000ABCDEFGH\"\noverlay = true\n[[cameras]]\nname = \"Side Yard\"\nurl = \"rtsp://192.0.2.6/a\"\noverlay = true\n");
        let ch = migrate(&db, &mut c, 1_000, &|_| Ok(()));
        assert_eq!(ch.overlay_off, vec!["Side Yard".to_string()]);
        assert!(c.cameras.iter().all(|c| !c.overlay));
        assert!(overlay_note(&db, 2_000).unwrap().contains(": Side Yard;"));
    }

    #[test]
    fn text_that_cannot_be_saved_off_stays_burned_in_and_is_tried_again() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg("[[cameras]]\nname = \"Text\"\nurl = \"rtsp://192.0.2.2/a\"\noverlay = true\n");
        let ch = migrate(&db, &mut c, 1_000, &|_| anyhow::bail!("read-only"));
        assert!(ch.overlay_off.is_empty());
        assert!(cam(&c, "Text").overlay);
        assert!(db.ui_get(KV_SCOPE, OVERLAY_KV).unwrap().is_none(), "not recorded");
        let saved = std::cell::RefCell::new(None);
        let ch = migrate(&db, &mut c, 3_000, &|c| {
            *saved.borrow_mut() = Some(cam(c, "Text").overlay);
            Ok(())
        });
        assert_eq!(ch.overlay_off, vec!["Text".to_string()]);
        assert_eq!(*saved.borrow(), Some(false), "saved with the text off");
        assert!(overlay_note(&db, 4_000).unwrap().contains(": Text;"));
    }
}
