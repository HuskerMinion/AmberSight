//! What the morning summary reads and keeps: the summaries sent, the
//! stretches a camera was offline, and the lookups behind "first time seen".

use super::*;

/// An event as the summary sees it.
#[derive(Debug, Clone)]
pub struct DigestEvent {
    pub id: String,
    pub camera_id: i64,
    pub camera: String,
    pub start: i64,
    pub thumb: Option<String>,
    pub objects: Vec<String>,
}

/// How many summaries are kept.
const KEEP_DIGESTS: i64 = 60;

impl Db {
    // ---- summaries ----

    pub fn digest_add(&self, created_ms: i64, from_ms: i64, to_ms: i64, auto: bool, body: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute("INSERT INTO digests (created_ms, from_ms, to_ms, auto, body) VALUES (?1, ?2, ?3, ?4, ?5)", params![created_ms, from_ms, to_ms, auto as i64, body])?;
        let id = conn.last_insert_rowid();
        conn.execute("DELETE FROM digests WHERE id <= ?1 - ?2", params![id, KEEP_DIGESTS])?;
        Ok(id)
    }

    /// A summary's JSON.
    pub fn digest_get(&self, id: i64) -> Result<Option<String>> {
        Ok(self.lock().query_row("SELECT body FROM digests WHERE id = ?1", params![id], |r| r.get(0)).optional()?)
    }

    /// The newest summary's id.
    pub fn digest_latest_id(&self) -> Result<Option<i64>> {
        Ok(self.lock().query_row("SELECT MAX(id) FROM digests", [], |r| r.get(0))?)
    }

    /// Summaries, newest first: (id, created_ms, from_ms, to_ms, sent by the clock).
    pub fn digest_list(&self, limit: usize) -> Result<Vec<(i64, i64, i64, i64, bool)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, created_ms, from_ms, to_ms, auto FROM digests ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, i64>(4)? != 0)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// When the last summary sent by the clock was made, and the end of its
    /// period.
    pub fn digest_last_auto(&self) -> Result<Option<(i64, i64)>> {
        Ok(self
            .lock()
            .query_row("SELECT created_ms, to_ms FROM digests WHERE auto = 1 ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    /// The end of the newest summary's period (sent by the clock or by hand).
    pub fn digest_last_to(&self) -> Result<Option<i64>> {
        Ok(self.lock().query_row("SELECT MAX(to_ms) FROM digests", [], |r| r.get(0))?)
    }

    // ---- offline stretches ----

    /// A camera went offline; returns the stretch's id.
    pub fn outage_open(&self, camera: &str, start_ms: i64) -> Result<i64> {
        let conn = self.lock();
        conn.execute("INSERT INTO camera_outages (camera, start_ms, end_ms, open) VALUES (?1, ?2, ?2, 1)", params![camera, start_ms])?;
        Ok(conn.last_insert_rowid())
    }

    /// Still offline at `now_ms`.
    pub fn outage_touch(&self, id: i64, now_ms: i64) -> Result<()> {
        self.lock().execute("UPDATE camera_outages SET end_ms = ?2 WHERE id = ?1 AND open = 1", params![id, now_ms])?;
        Ok(())
    }

    /// Back online at `end_ms`.
    pub fn outage_close(&self, id: i64, end_ms: i64) -> Result<()> {
        self.lock().execute("UPDATE camera_outages SET end_ms = ?2, open = 0 WHERE id = ?1", params![id, end_ms])?;
        Ok(())
    }

    /// At a start: stretches still open belong to the last run; they end
    /// where it last saw them.
    pub fn outages_close_stale(&self) -> Result<()> {
        self.lock().execute("UPDATE camera_outages SET open = 0 WHERE open = 1", [])?;
        Ok(())
    }

    /// Stretches that overlap [from, to): (camera, start, end), oldest first.
    pub fn outages_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<(String, i64, i64)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT camera, start_ms, end_ms FROM camera_outages WHERE end_ms > ?1 AND start_ms < ?2 ORDER BY start_ms")?;
        let rows = st.query_map(params![from_ms, to_ms], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn outages_prune(&self, before_ms: i64) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM camera_outages WHERE open = 0 AND end_ms < ?1", params![before_ms])?)
    }

    // ---- reading ----

    /// Events starting in [from, to) (seconds), oldest first. Events reviewed
    /// as false alarms are left out, and so are unconfirmed events of the
    /// cameras in `hide_unconfirmed` (as the event list leaves them out).
    pub fn digest_events(&self, from: i64, to: i64, hide_unconfirmed: &[String]) -> Result<Vec<DigestEvent>> {
        let conn = self.lock();
        let marks = vec!["?"; hide_unconfirmed.len()].join(", ");
        let hide = if hide_unconfirmed.is_empty() { String::new() } else { format!(" AND NOT (unconfirmed = 1 AND camera COLLATE NOCASE IN ({marks}))") };
        let mut st = conn.prepare(&format!(
            "SELECT id, camera_id, camera, start, thumb, objects FROM events WHERE start >= ? AND start < ? AND review IS NOT 'false_alarm'{hide} ORDER BY start"
        ))?;
        let args = [from.into(), to.into()].into_iter().chain(hide_unconfirmed.iter().map(|c| c.clone().into())).collect::<Vec<rusqlite::types::Value>>();
        let rows = st.query_map(rusqlite::params_from_iter(args), |r| {
            Ok(DigestEvent {
                id: r.get(0)?,
                camera_id: r.get(1)?,
                camera: r.get(2)?,
                start: r.get(3)?,
                thumb: r.get(4)?,
                objects: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Tamper alerts raised in [from, to) (seconds): (camera, at, kind, cleared_at).
    pub fn tamper_between(&self, from: i64, to: i64) -> Result<Vec<(String, i64, String, Option<i64>)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT camera, at, kind, cleared_at FROM tamper_log WHERE at >= ?1 AND at < ?2 ORDER BY at")?;
        let rows = st.query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Names of the people given a name in Faces.
    pub fn person_names(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT name FROM people")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- "first time seen" ----

    /// Fingerprints of faces nobody named, seen since `since_ms` in other
    /// events than `exclude_event`.
    pub fn unnamed_face_embs_since(&self, since_ms: i64, exclude_event: &str) -> Result<Vec<Vec<u8>>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT emb FROM faces WHERE person_id IS NULL AND at_ms >= ?1 AND event_id != ?2")?;
        let rows = st.query_map(params![since_ms, exclude_event], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The plates read since `since_ms` in other events than `exclude_event`
    /// (each text once).
    pub fn plate_texts_since(&self, since_ms: i64, exclude_event: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT DISTINCT text FROM plates WHERE at_ms >= ?1 AND event_id != ?2")?;
        let rows = st.query_map(params![since_ms, exclude_event], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-digest-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn digest_leaves_out_false_alarms_and_hidden_unconfirmed_events() {
        let db = Db::open(&tmp("b").join("t.db")).unwrap();
        for (id, cam) in [("a", "Front"), ("b", "Front"), ("c", "Front"), ("d", "Back")] {
            let ev = crate::db::Event { id: id.into(), camera_id: 1, camera: cam.into(), start: 10, end: 20, score: 0.5, clip: None, thumb: None, objects: vec!["person".into()], protect: false };
            db.insert_event(&ev).unwrap();
        }
        db.set_event_review("b", Some("false_alarm"), 100).unwrap();
        db.set_event_review("c", Some("ok"), 100).unwrap();
        db.set_event_unconfirmed("c").unwrap();
        db.set_event_unconfirmed("d").unwrap();
        let ids = |hide: &[String]| db.digest_events(0, 100, hide).unwrap().into_iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(ids(&[]), ["a", "c", "d"]);
        assert_eq!(ids(&["front".to_string()]), ["a", "d"]);
    }

    #[test]
    fn digests_outages_and_lookups() {
        let db = Db::open(&tmp("a").join("t.db")).unwrap();
        let a = db.digest_add(1000, 0, 900, true, "{\"x\":1}").unwrap();
        let b = db.digest_add(2000, 900, 1900, false, "{}").unwrap();
        assert_eq!(db.digest_get(a).unwrap().as_deref(), Some("{\"x\":1}"));
        assert_eq!(db.digest_latest_id().unwrap(), Some(b));
        assert_eq!(db.digest_last_auto().unwrap(), Some((1000, 900)));
        assert_eq!(db.digest_last_to().unwrap(), Some(1900));
        assert_eq!(db.digest_list(10).unwrap().len(), 2);

        let o = db.outage_open("Yard", 10_000).unwrap();
        db.outage_touch(o, 70_000).unwrap();
        assert_eq!(db.outages_between(0, 20_000).unwrap(), vec![("Yard".to_string(), 10_000, 70_000)]);
        assert!(db.outages_between(80_000, 90_000).unwrap().is_empty());
        db.outage_close(o, 100_000).unwrap();
        db.outages_close_stale().unwrap();
        assert_eq!(db.outages_between(0, 20_000).unwrap()[0].2, 100_000);

        db.insert_plate(&crate::db::PlateRow { event_id: "e1", camera: "yard", at_ms: 5000, text: "ABC123", confidence: 0.9, width: 90, img: b"x", known_id: None }).unwrap();
        assert_eq!(db.plate_texts_since(1000, "e2").unwrap(), vec!["ABC123".to_string()]);
        assert!(db.plate_texts_since(1000, "e1").unwrap().is_empty());
        assert!(db.plate_texts_since(6000, "e2").unwrap().is_empty());

        db.insert_face("e1", "yard", 5000, 0.9, 80, &[1, 2, 3, 4], b"img", None).unwrap();
        assert_eq!(db.unnamed_face_embs_since(1000, "e2").unwrap().len(), 1);
        assert!(db.unnamed_face_embs_since(1000, "e1").unwrap().is_empty());
    }
}
