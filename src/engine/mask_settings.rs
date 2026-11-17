//! The 0.11.1 change of privacy masks and burned-in text (once, at start),
//! and what Status says about it.
//!
//! - Masks from before the mask level existed get `mask_mode = "recording"`
//!   written down (what they always did), pushed and battery cameras
//!   "screen" (nothing ever burned masks into their recordings).
//! - Text burned into the recordings of cameras without masks is drawn on
//!   screen instead (`overlay = false`), once: the database remembers it
//!   (`overlay_migrated_0_11_1`), so an owner who turns it back on (or a
//!   config set back from an older copy) is not changed again.

use crate::config::Config;
use crate::db::Db;

/// ui_kv scope and key that remember the overlay change (and for whom).
pub const KV_SCOPE: &str = "system";
pub const OVERLAY_KV: &str = "overlay_migrated_0_11_1";
/// ... and the one that notes what an earlier "share without the mask"
/// confirmation was given for (`share_unmasked_rev`, once).
pub const CONSENT_KV: &str = "share_unmasked_rev_0_11_1";
/// How long Status keeps the note about it.
const NOTE_DAYS: i64 = 30;

/// What was changed, for the log.
#[derive(Debug, Default, PartialEq)]
pub struct Changed {
    pub masks: crate::config::MaskMigration,
    pub overlay_off: Vec<String>,
    /// Cameras whose "share without the mask" confirmation (from before it
    /// noted what it was given for) now holds for their masks and text.
    pub consent: Vec<String>,
}

impl Changed {
    pub fn any(&self) -> bool {
        self.masks.changed() || !self.overlay_off.is_empty() || !self.consent.is_empty()
    }
}

/// Run both changes on `cfg` and write it (`save`) when something changed.
/// The overlay one runs only if the database has not recorded it yet; it is
/// recorded only once the config is written (or nothing needed writing),
/// so Status never says text was turned off that is still on in the file.
/// `now_ms`: when, for the Status note.
pub fn migrate(db: &Db, cfg: &mut Config, now_ms: i64, save: &dyn Fn(&Config) -> anyhow::Result<()>) -> Changed {
    let masks = cfg.migrate_masks();
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
    // A confirmation given before this version: for what the camera has now
    // (once; later ones are noted when given, and one whose masks were
    // changed by hand since no longer holds).
    let consent = match db.ui_get(KV_SCOPE, CONSENT_KV) {
        Ok(None) => {
            let mut names = Vec::new();
            for c in cfg.cameras.iter_mut().filter(|c| c.share_unmasked && c.share_unmasked_rev.is_none()) {
                c.share_unmasked_rev = Some(crate::restream::http::consent_rev(c));
                names.push(c.name.clone());
            }
            Some(names)
        }
        Ok(Some(_)) => None,
        Err(e) => {
            tracing::warn!("settings: could not read whether sharing confirmations were noted ({e:#})");
            None
        }
    };
    let changed = Changed { masks, overlay_off: overlay_off.clone(), consent: consent.clone().unwrap_or_default() };
    if changed.any() {
        if let Err(e) = save(cfg) {
            // Not written: the text stays burned in (and the change is tried
            // again next start); the mask levels hold in memory.
            tracing::warn!("settings: could not save the privacy mask and text settings ({e:#})");
            undo(cfg, &mut overlay_off);
            return Changed { masks: changed.masks, overlay_off, consent: changed.consent };
        }
    }
    if consent.is_some() {
        if let Err(e) = db.ui_put(KV_SCOPE, CONSENT_KV, "1") {
            tracing::warn!("settings: could not record that sharing confirmations were noted ({e:#})");
        }
    }
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
    Changed { masks: changed.masks, overlay_off, consent: changed.consent }
}

/// Log lines for what `migrate` changed.
pub fn log_lines(c: &Changed) -> Vec<String> {
    let mut out = Vec::new();
    if !c.masks.kept_in_recording.is_empty() {
        out.push(format!(
            "privacy masks: {} keep their masks in the recordings too (mask_mode = \"recording\", as before)",
            c.masks.kept_in_recording.join(", ")
        ));
    }
    if !c.masks.screen_only.is_empty() {
        out.push(format!(
            "privacy masks: {} (pushed or battery cameras) hide their masks on screen and in pictures (mask_mode = \"screen\"); their recordings never had them",
            c.masks.screen_only.join(", ")
        ));
    }
    if !c.overlay_off.is_empty() {
        out.push(format!("burned-in text: {}", overlay_note_text(&c.overlay_off)));
    }
    if !c.consent.is_empty() {
        out.push(format!("stream sharing: the confirmation to share {} without their masks now holds for the masks and text they have (changed ones need it again)", c.consent.join(", ")));
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

/// Why this camera records nothing although recording is on: its masks
/// are to be burned into the recordings, which cannot be done for it
/// (fail closed: no unmasked recording instead).
pub fn record_blocker(cfg: &crate::config::CameraConfig) -> Option<String> {
    (cfg.record && cfg.mask_in_recording() && cfg.no_ffmpeg_reader()).then(|| {
        format!(
            "{} records nothing: its privacy masks are set to \"Hide in recordings too\", which is not possible for a pushed or battery camera. Choose \"Hide on screen and in pictures\" (Motion > Edit zones), or use the camera's own privacy mask",
            cfg.name
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cams: &str) -> Config {
        toml::from_str(cams).unwrap()
    }

    const CAMS: &str = r#"
[[cameras]]
name = "Masked"
url = "rtsp://192.0.2.1/a"
overlay = true
privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]

[[cameras]]
name = "Text"
url = "rtsp://192.0.2.2/a"
overlay = true

[[cameras]]
name = "Plain"
url = "rtsp://192.0.2.3/a"

[[cameras]]
name = "Pushed"
url = "push://pushed"
privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]

[[cameras]]
name = "Chosen"
url = "rtsp://192.0.2.4/a"
mask_mode = "screen"
privacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]
"#;

    fn cam<'a>(c: &'a Config, n: &str) -> &'a crate::config::CameraConfig {
        c.cameras.iter().find(|c| c.name == n).unwrap()
    }

    #[test]
    fn a_new_install_keeps_text_set_to_burn_in() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.created_fresh());
        let mut cfg: Config = toml::from_str("[[cameras]]\nname = \"Text Cam\"\nurl = \"rtsp://192.0.2.1/x\"\noverlay = true\n").unwrap();
        let changed = migrate(&db, &mut cfg, 0, &|_| Ok(()));
        assert!(changed.overlay_off.is_empty(), "nothing to upgrade on a new database");
        assert!(cfg.cameras[0].overlay, "text stays burned in");
        assert!(db.ui_get(KV_SCOPE, OVERLAY_KV).unwrap().is_some(), "and never changed later either");
    }

    #[test]
    fn masks_get_their_level_and_text_without_masks_is_drawn_once() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg(CAMS);
        let ch = migrate(&db, &mut c, 1_000, &|_| Ok(()));
        assert_eq!(ch.masks.kept_in_recording, vec!["Masked".to_string()]);
        assert_eq!(ch.masks.screen_only, vec!["Pushed".to_string()]);
        assert_eq!(ch.overlay_off, vec!["Text".to_string()]);
        assert_eq!(cam(&c, "Masked").mask_mode.as_deref(), Some("recording"));
        assert!(cam(&c, "Masked").overlay, "a camera with masks keeps its burned-in text");
        assert!(!cam(&c, "Text").overlay);
        assert_eq!(cam(&c, "Pushed").mask_mode.as_deref(), Some("screen"));
        assert_eq!(cam(&c, "Chosen").mask_mode.as_deref(), Some("screen"));
        assert_eq!(cam(&c, "Plain").mask_mode, None);
        // Saved and loaded again (what the server writes), the levels are there.
        let back: Config = toml::from_str(&c.to_toml().unwrap()).unwrap();
        assert_eq!(cam(&back, "Masked").mask_mode.as_deref(), Some("recording"));
        assert!(!cam(&back, "Text").overlay);
        // Status says it, with the names.
        let note = overlay_note(&db, 2_000).unwrap();
        assert!(note.starts_with("1 camera now draws its text on screen instead of re-encoding: Text;"), "{note}");
        assert_eq!(overlay_note(&db, 1_000 + 31 * 86_400_000), None, "the note goes after a while");
        assert_eq!(log_lines(&ch).len(), 3);
    }

    #[test]
    fn running_again_or_on_an_older_config_changes_nothing_more() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg(CAMS);
        migrate(&db, &mut c, 1_000, &|_| Ok(()));
        // The owner turns the text back on; next start leaves it.
        c.cameras.iter_mut().find(|c| c.name == "Text").unwrap().overlay = true;
        let again = migrate(&db, &mut c, 5_000, &|_| Ok(()));
        assert!(!again.any(), "{again:?}");
        assert!(cam(&c, "Text").overlay);
        // A config set back from before the change (no levels, text on):
        // masks get "recording" again (never less), the text stays on.
        let mut old = cfg(CAMS);
        let ch = migrate(&db, &mut old, 9_000, &|_| Ok(()));
        assert_eq!(ch.masks.kept_in_recording, vec!["Masked".to_string()]);
        assert!(ch.overlay_off.is_empty());
        assert!(cam(&old, "Text").overlay);
        // The note still names the first change only.
        assert!(overlay_note(&db, 9_000).unwrap().contains(": Text;"));
    }

    #[test]
    fn a_config_that_cannot_be_saved_keeps_its_text_and_is_tried_again() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg(CAMS);
        let ch = migrate(&db, &mut c, 1_000, &|_| anyhow::bail!("read-only"));
        assert!(ch.overlay_off.is_empty());
        assert!(cam(&c, "Text").overlay, "still burned in, as the file says");
        assert!(db.ui_get(KV_SCOPE, OVERLAY_KV).unwrap().is_none(), "not recorded");
        assert_eq!(overlay_note(&db, 2_000), None);
        // Next start, with a config that saves: the change is made then.
        let saved = std::cell::RefCell::new(None);
        let ch = migrate(&db, &mut c, 3_000, &|c| {
            *saved.borrow_mut() = Some(cam(c, "Text").overlay);
            Ok(())
        });
        assert_eq!(ch.overlay_off, vec!["Text".to_string()]);
        assert_eq!(*saved.borrow(), Some(false), "saved with the text off");
        assert!(overlay_note(&db, 4_000).unwrap().contains(": Text;"));
    }

    #[test]
    fn nothing_to_change_is_recorded_too_and_says_nothing() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg("[[cameras]]\nname = \"A\"\nurl = \"rtsp://192.0.2.1/a\"\n");
        assert!(!migrate(&db, &mut c, 1, &|_| Ok(())).any());
        assert!(db.ui_get(KV_SCOPE, OVERLAY_KV).unwrap().is_some());
        assert_eq!(overlay_note(&db, 2), None);
        // A camera given burned-in text later is not changed.
        c.cameras[0].overlay = true;
        assert!(!migrate(&db, &mut c, 3, &|_| Ok(())).any());
        assert!(c.cameras[0].overlay);
    }

    #[test]
    fn an_earlier_sharing_confirmation_is_noted_once_and_a_hand_edit_lapses_it() {
        let db = Db::open_in_memory().unwrap().as_upgraded();
        let mut c = cfg(&CAMS.replace("name = \"Masked\"\n", "name = \"Masked\"\nshare = true\nshare_unmasked = true\n"));
        assert!(crate::restream::http::blocker(cam(&c, "Masked")).is_some(), "not noted yet: waits");
        let ch = migrate(&db, &mut c, 1_000, &|_| Ok(()));
        assert_eq!(ch.consent, vec!["Masked".to_string()]);
        assert!(crate::restream::http::consent_holds(cam(&c, "Masked")));
        // Saved with it.
        let back: Config = toml::from_str(&c.to_toml().unwrap()).unwrap();
        assert!(crate::restream::http::consent_holds(cam(&back, "Masked")));
        // The masks changed in the file while stopped: asked again.
        let mut edited = back.clone();
        edited.cameras.iter_mut().find(|c| c.name == "Masked").unwrap().privacy_masks[0].w = 0.5;
        assert!(migrate(&db, &mut edited, 2_000, &|_| Ok(())).consent.is_empty(), "once only");
        assert!(!crate::restream::http::consent_holds(cam(&edited, "Masked")));
        assert!(crate::restream::http::blocker(cam(&edited, "Masked")).is_some());
        // The text setting changed by hand: asked again too.
        let mut text = back.clone();
        text.cameras.iter_mut().find(|c| c.name == "Masked").unwrap().overlay = false;
        assert!(!crate::restream::http::consent_holds(cam(&text, "Masked")));
        // A confirmation set by hand after this version (no revision): not taken.
        let mut later = cfg(&CAMS.replace("name = \"Masked\"\n", "name = \"Masked\"\nshare_unmasked = true\n"));
        migrate(&db, &mut later, 3_000, &|_| Ok(()));
        assert!(crate::restream::http::blocker(cam(&later, "Masked")).is_some());
    }

    #[test]
    fn a_pushed_or_battery_camera_set_to_recording_records_nothing() {
        let mut c = cfg(CAMS);
        let p = c.cameras.iter_mut().find(|c| c.name == "Pushed").unwrap();
        p.mask_mode = Some("recording".into());
        assert!(record_blocker(p).unwrap().contains("records nothing"));
        p.mask_mode = Some("screen".into());
        assert_eq!(record_blocker(p), None);
        let mut b: crate::config::CameraConfig = toml::from_str("name = \"B\"\nurl = \"baichuan://192.0.2.9?uid=TESTUID000000001\"\nmask_mode = \"recording\"\nprivacy_masks = [{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }]").unwrap();
        assert!(record_blocker(&b).is_some());
        b.record = false;
        assert_eq!(record_blocker(&b), None);
        assert_eq!(record_blocker(cam(&c, "Masked")), None, "an RTSP camera burns them in");
    }
}
