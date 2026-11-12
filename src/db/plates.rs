//! License plate readings and the library of known plates.

use super::*;

/// One reading of a plate in an event, as saved.
pub struct PlateRow<'a> {
    pub event_id: &'a str,
    /// The camera's slug.
    pub camera: &'a str,
    pub at_ms: i64,
    /// Normalized text.
    pub text: &'a str,
    pub confidence: f32,
    /// Plate width in the full picture (pixels).
    pub width: u32,
    pub img: &'a [u8],
    pub known_id: Option<i64>,
}

/// A plate someone named.
#[derive(Debug, Clone, serde::Serialize)]
pub struct KnownPlate {
    pub id: i64,
    pub text: String,
    pub name: String,
    pub notes: String,
    pub created_ms: i64,
    /// Readings of exactly this plate, and the latest.
    pub seen: i64,
    pub last_ms: Option<i64>,
}

/// Filters for readings; `None` / empty mean "no filter".
#[derive(Default, Clone)]
pub struct PlateFilter {
    pub event: Option<String>,
    /// Camera slugs a limited caller may see (None: all).
    pub cameras: Option<Vec<String>>,
    pub camera: Option<String>,
    /// Part of the plate text, or of a known plate's name.
    pub q: Option<String>,
    /// Some(true): only known plates; Some(false): only unknown ones.
    pub known: Option<bool>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub limit: usize,
    pub offset: usize,
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// WHERE for the filter (on `plates p LEFT JOIN known_plates k`) and its arguments.
fn where_sql(f: &PlateFilter) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    let mut sql = String::from(" WHERE 1 = 1");
    let mut args: Vec<Value> = Vec::new();
    if let Some(e) = &f.event {
        sql.push_str(" AND p.event_id = ?");
        args.push(e.clone().into());
    }
    if let Some(c) = &f.camera {
        sql.push_str(" AND p.camera = ?");
        args.push(c.clone().into());
    }
    if let Some(list) = &f.cameras {
        if list.is_empty() {
            sql.push_str(" AND 0");
        } else {
            sql.push_str(&format!(" AND p.camera IN ({})", vec!["?"; list.len()].join(",")));
            args.extend(list.iter().map(|c| Value::from(c.clone())));
        }
    }
    if let Some(q) = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let norm = crate::lpr::normalize(q);
        if norm.is_empty() {
            // Nothing that can be in a plate: only names can match.
            sql.push_str(" AND k.name LIKE ? ESCAPE '\\'");
        } else {
            sql.push_str(" AND (p.text LIKE ? ESCAPE '\\' OR k.name LIKE ? ESCAPE '\\')");
            args.push(format!("%{}%", escape_like(&norm)).into());
        }
        args.push(format!("%{}%", escape_like(q)).into());
    }
    match f.known {
        Some(true) => sql.push_str(" AND p.known_id IS NOT NULL"),
        Some(false) => sql.push_str(" AND p.known_id IS NULL"),
        None => {}
    }
    if let Some(t) = f.since_ms {
        sql.push_str(" AND p.at_ms >= ?");
        args.push(t.into());
    }
    if let Some(t) = f.until_ms {
        sql.push_str(" AND p.at_ms < ?");
        args.push(t.into());
    }
    (sql, args)
}

impl Db {
    /// Save a reading; returns its id.
    pub fn insert_plate(&self, p: &PlateRow) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO plates (event_id, camera, at_ms, text, confidence, width, img, known_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![p.event_id, p.camera, p.at_ms, p.text, p.confidence as f64, p.width as i64, p.img, p.known_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Readings, newest first, each with its known plate's name.
    pub fn list_plates(&self, f: &PlateFilter) -> Result<Vec<serde_json::Value>> {
        let (w, mut args) = where_sql(f);
        args.push((f.limit.clamp(1, 1000) as i64).into());
        args.push((f.offset as i64).into());
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT p.id, p.event_id, p.camera, p.at_ms, p.text, p.confidence, p.width, p.known_id, k.name, k.notes
             FROM plates p LEFT JOIN known_plates k ON k.id = p.known_id{w} ORDER BY p.at_ms DESC, p.id DESC LIMIT ? OFFSET ?"
        ))?;
        let rows = st.query_map(params_from_iter(args), |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "event": r.get::<_, String>(1)?,
                "camera": r.get::<_, String>(2)?,
                "at_ms": r.get::<_, i64>(3)?,
                "text": r.get::<_, String>(4)?,
                "confidence": r.get::<_, f64>(5)?,
                "width": r.get::<_, i64>(6)?,
                "known": r.get::<_, Option<i64>>(7)?.is_some(),
                "known_id": r.get::<_, Option<i64>>(7)?,
                "name": r.get::<_, Option<String>>(8)?,
                "notes": r.get::<_, Option<String>>(9)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The plates seen, one per text: how many readings, first and last
    /// seen, one reading's picture and the known plate's name. Most
    /// recently seen first.
    pub fn recent_plates(&self, f: &PlateFilter) -> Result<Vec<serde_json::Value>> {
        let (w, where_args) = where_sql(f);
        // The picture is the best reading among the cameras the caller may
        // see (an id from another camera would not show, and tells too much).
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        let mut pick = String::new();
        if let Some(c) = &f.camera {
            pick.push_str(" AND x.camera = ?");
            args.push(c.clone().into());
        }
        if let Some(list) = &f.cameras {
            if list.is_empty() {
                pick.push_str(" AND 0");
            } else {
                pick.push_str(&format!(" AND x.camera IN ({})", vec!["?"; list.len()].join(",")));
                args.extend(list.iter().map(|c| rusqlite::types::Value::from(c.clone())));
            }
        }
        args.extend(where_args);
        args.push((f.limit.clamp(1, 1000) as i64).into());
        args.push((f.offset as i64).into());
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT p.text, COUNT(*), MIN(p.at_ms), MAX(p.at_ms),
                    (SELECT x.id FROM plates x WHERE x.text = p.text{pick} ORDER BY x.confidence DESC, x.id DESC LIMIT 1),
                    MAX(p.confidence), MAX(p.known_id), MAX(k.name), MAX(k.notes), GROUP_CONCAT(DISTINCT p.camera)
             FROM plates p LEFT JOIN known_plates k ON k.id = p.known_id{w}
             GROUP BY p.text ORDER BY MAX(p.at_ms) DESC LIMIT ? OFFSET ?"
        ))?;
        let rows = st.query_map(params_from_iter(args), |r| {
            let cams: String = r.get::<_, Option<String>>(9)?.unwrap_or_default();
            Ok(serde_json::json!({
                "text": r.get::<_, String>(0)?,
                "count": r.get::<_, i64>(1)?,
                "first_ms": r.get::<_, i64>(2)?,
                "last_ms": r.get::<_, i64>(3)?,
                "id": r.get::<_, i64>(4)?,
                "confidence": r.get::<_, f64>(5)?,
                "known": r.get::<_, Option<i64>>(6)?.is_some(),
                "known_id": r.get::<_, Option<i64>>(6)?,
                "name": r.get::<_, Option<String>>(7)?,
                "notes": r.get::<_, Option<String>>(8)?,
                "cameras": cams.split(',').filter(|c| !c.is_empty()).collect::<Vec<_>>(),
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A reading's camera slug and picture.
    pub fn plate_img(&self, id: i64) -> Result<Option<(String, Vec<u8>)>> {
        Ok(self.lock().query_row("SELECT camera, img FROM plates WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    pub fn delete_plate(&self, id: i64) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM plates WHERE id = ?1", params![id])? > 0)
    }

    /// (known, unknown): how many plates were read in an event, by whether
    /// they matched a known plate. For rules that act on strangers only.
    #[allow(dead_code)]
    pub fn plate_flags(&self, event_id: &str) -> Result<(usize, usize)> {
        let (k, u): (i64, i64) = self.lock().query_row(
            "SELECT COALESCE(SUM(known_id IS NOT NULL), 0), COALESCE(SUM(known_id IS NULL), 0) FROM plates WHERE event_id = ?1",
            params![event_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((k as usize, u as usize))
    }

    /// Readings of unknown plates older than `before_ms` (their events are
    /// long gone).
    pub fn prune_plates(&self, before_ms: i64) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM plates WHERE at_ms < ?1", params![before_ms])?)
    }

    // ---- the library of known plates ----

    /// Known plates by name, each with how often and when last seen.
    pub fn known_plates(&self) -> Result<Vec<KnownPlate>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT k.id, k.text, k.name, k.notes, k.created_ms,
                    (SELECT COUNT(*) FROM plates p WHERE p.known_id = k.id),
                    (SELECT MAX(p.at_ms) FROM plates p WHERE p.known_id = k.id)
             FROM known_plates k ORDER BY k.name COLLATE NOCASE, k.text",
        )?;
        let rows = st.query_map([], |r| {
            Ok(KnownPlate { id: r.get(0)?, text: r.get(1)?, name: r.get(2)?, notes: r.get(3)?, created_ms: r.get(4)?, seen: r.get(5)?, last_ms: r.get(6)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// (id, text) of every known plate, for matching readings.
    pub fn known_plate_texts(&self) -> Result<Vec<(i64, String)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, text FROM known_plates ORDER BY id")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn known_plate_name(&self, id: i64) -> Result<Option<String>> {
        Ok(self.lock().query_row("SELECT name FROM known_plates WHERE id = ?1", params![id], |r| r.get(0)).optional()?)
    }

    /// Name a plate. `text` must be normalized. Ok(None): that plate is
    /// already in the library. Earlier readings of exactly this plate
    /// become known.
    pub fn add_known_plate(&self, text: &str, name: &str, notes: &str, now_ms: i64) -> Result<Option<i64>> {
        let conn = self.lock();
        let n = conn.execute("INSERT OR IGNORE INTO known_plates (text, name, notes, created_ms) VALUES (?1, ?2, ?3, ?4)", params![text, name, notes, now_ms])?;
        if n == 0 {
            return Ok(None);
        }
        let id = conn.last_insert_rowid();
        conn.execute("UPDATE plates SET known_id = ?1 WHERE text = ?2 AND known_id IS NULL", params![id, text])?;
        Ok(Some(id))
    }

    /// Change a known plate (what is None stays). Ok(false): no such plate.
    /// Err: another known plate has that text.
    pub fn update_known_plate(&self, id: i64, text: Option<&str>, name: Option<&str>, notes: Option<&str>) -> Result<bool> {
        let conn = self.lock();
        if conn.query_row("SELECT 1 FROM known_plates WHERE id = ?1", params![id], |_| Ok(())).optional()?.is_none() {
            return Ok(false);
        }
        if let Some(t) = text {
            conn.execute("UPDATE known_plates SET text = ?2 WHERE id = ?1", params![id, t])?;
            // Readings of the new text join it; those only the old text matched leave.
            conn.execute("UPDATE plates SET known_id = NULL WHERE known_id = ?1 AND text <> ?2", params![id, t])?;
            conn.execute("UPDATE plates SET known_id = ?1 WHERE text = ?2 AND known_id IS NULL", params![id, t])?;
        }
        if let Some(n) = name {
            conn.execute("UPDATE known_plates SET name = ?2 WHERE id = ?1", params![id, n])?;
        }
        if let Some(n) = notes {
            conn.execute("UPDATE known_plates SET notes = ?2 WHERE id = ?1", params![id, n])?;
        }
        Ok(true)
    }

    /// Forget a known plate; its readings stay, as unknown.
    pub fn delete_known_plate(&self, id: i64) -> Result<bool> {
        let conn = self.lock();
        conn.execute("UPDATE plates SET known_id = NULL WHERE known_id = ?1", params![id])?;
        Ok(conn.execute("DELETE FROM known_plates WHERE id = ?1", params![id])? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-plates-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn row<'a>(event: &'a str, cam: &'a str, at: i64, text: &'a str, known: Option<i64>) -> PlateRow<'a> {
        PlateRow { event_id: event, camera: cam, at_ms: at, text, confidence: 0.9, width: 90, img: b"jpg", known_id: known }
    }

    fn filter() -> PlateFilter {
        PlateFilter { limit: 10, ..Default::default() }
    }

    #[test]
    fn plates_round_trip_with_known_names_and_flags() {
        let db = Db::open(&tmp("rt").join("t.db")).unwrap();
        let a = db.insert_plate(&row("drive-1", "drive", 1000, "ABC123", None)).unwrap();
        db.insert_plate(&row("drive-2", "drive", 2000, "ABC123", None)).unwrap();
        db.insert_plate(&row("drive-2", "drive", 2000, "ZZZ999", None)).unwrap();
        db.insert_plate(&row("gate-3", "gate", 3000, "QQQ555", None)).unwrap();
        assert_eq!(db.plate_flags("drive-2").unwrap(), (0, 2));
        // Naming a plate makes its earlier readings known.
        let k = db.add_known_plate("ABC123", "Mom's car", "blue sedan", 5000).unwrap().unwrap();
        assert!(db.add_known_plate("ABC123", "again", "", 5000).unwrap().is_none(), "one entry per plate");
        assert_eq!(db.plate_flags("drive-2").unwrap(), (1, 1));
        assert_eq!(db.plate_flags("nothing").unwrap(), (0, 0));
        let all = db.list_plates(&filter()).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0]["text"], "QQQ555", "newest first");
        let mom = all.iter().find(|r| r["id"] == a).unwrap();
        assert_eq!((mom["known"].as_bool(), mom["name"].as_str(), mom["notes"].as_str()), (Some(true), Some("Mom's car"), Some("blue sedan")));
        // Search: part of the text (any spacing or case) or the name; known/unknown; camera; time.
        let n = |f: PlateFilter| db.list_plates(&f).unwrap().len();
        assert_eq!(n(PlateFilter { q: Some("bc-12".into()), ..filter() }), 2);
        assert_eq!(n(PlateFilter { q: Some("mom".into()), ..filter() }), 2);
        assert_eq!(n(PlateFilter { q: Some("%".into()), ..filter() }), 0, "no wildcards");
        assert_eq!(n(PlateFilter { known: Some(false), ..filter() }), 2);
        assert_eq!(n(PlateFilter { known: Some(true), ..filter() }), 2);
        assert_eq!(n(PlateFilter { camera: Some("gate".into()), ..filter() }), 1);
        assert_eq!(n(PlateFilter { since_ms: Some(2000), until_ms: Some(3000), ..filter() }), 2);
        assert_eq!(n(PlateFilter { event: Some("drive-1".into()), ..filter() }), 1);
        // A limited caller sees only their cameras.
        assert_eq!(n(PlateFilter { cameras: Some(vec!["gate".into()]), ..filter() }), 1);
        assert_eq!(n(PlateFilter { cameras: Some(vec![]), ..filter() }), 0);
        // One line per plate.
        let recent = db.recent_plates(&filter()).unwrap();
        assert_eq!(recent.len(), 3);
        let r = recent.iter().find(|r| r["text"] == "ABC123").unwrap();
        assert_eq!((r["count"].as_i64(), r["first_ms"].as_i64(), r["last_ms"].as_i64(), r["name"].as_str()), (Some(2), Some(1000), Some(2000), Some("Mom's car")));
        assert_eq!(db.plate_img(a).unwrap().unwrap(), ("drive".to_string(), b"jpg".to_vec()));
        let kp = db.known_plates().unwrap();
        assert_eq!((kp[0].id, kp[0].seen, kp[0].last_ms), (k, 2, Some(2000)));
        assert_eq!(db.known_plate_texts().unwrap(), vec![(k, "ABC123".to_string())]);
        // Forgetting the plate keeps the readings, as unknown.
        assert!(db.delete_known_plate(k).unwrap());
        assert_eq!(db.plate_flags("drive-2").unwrap(), (0, 2));
        assert!(db.known_plates().unwrap().is_empty());
        // Readings age out, known ones too (only the newest is left).
        let k2 = db.add_known_plate("QQQ555", "Gate guest", "", 6000).unwrap().unwrap();
        assert_eq!(db.prune_plates(2_500).unwrap(), 3);
        assert_eq!(db.list_plates(&filter()).unwrap().len(), 1);
        assert!(db.update_known_plate(k2, Some("QQQ556"), Some("Guest"), None).unwrap());
        assert_eq!(db.plate_flags("gate-3").unwrap(), (0, 1), "the old text no longer matches");
        assert!(!db.update_known_plate(999, None, Some("x"), None).unwrap());
    }

    #[test]
    fn readings_follow_events_and_cameras() {
        let db = Db::open(&tmp("ev").join("t.db")).unwrap();
        db.insert_plate(&row("old-1", "old", 1000, "AAA111", None)).unwrap();
        let k = db.add_known_plate("BBB222", "Friend", "", 1).unwrap().unwrap();
        db.insert_plate(&row("old-1", "old", 1000, "BBB222", Some(k))).unwrap();
        db.rename_camera("Old", "New").unwrap();
        assert_eq!(db.list_plates(&PlateFilter { camera: Some("new".into()), ..filter() }).unwrap().len(), 2);
        // Deleting the event drops unknown readings; known ones stay.
        db.delete_events(&["old-1".to_string()]).unwrap();
        let left = db.list_plates(&filter()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0]["text"], "BBB222");
    }

    #[test]
    fn a_version_17_database_gains_the_plate_tables() {
        let dir = tmp("mig");
        let p = dir.join("t.db");
        {
            let db = Db::open(&p).unwrap();
            db.lock().execute_batch("DROP TABLE plates; DROP TABLE known_plates; PRAGMA user_version = 17;").unwrap();
        }
        let db = Db::open(&p).unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        db.insert_plate(&row("e", "c", 1, "ABC123", None)).unwrap();
    }
}
