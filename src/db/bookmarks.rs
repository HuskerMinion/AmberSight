//! Timeline bookmarks: a moment marked with a note, on one camera or on all
//! of them (see crate::bookmarks_http). A bookmark with `keep` holds the
//! recordings around it back from automatic cleanup, see [`KEEP_WINDOW_MS`].

use super::*;

/// A kept bookmark protects the recordings from this long before its moment
/// to this long after it (on its camera, or on every camera for an
/// all-cameras bookmark): age retention, the size caps and the drive reserve
/// all skip a recording that overlaps that window. Moving a recording to the
/// archive folder is still allowed (it is kept there).
pub const KEEP_WINDOW_MS: i64 = 2 * 60 * 1000;

/// SQL condition, true for a row of `segments` that a kept bookmark covers.
/// (The bookmark's moment within KEEP_WINDOW_MS of the recording: uses the
/// partial index on kept bookmarks' times.)
pub(super) const KEPT_BY_BOOKMARK: &str = "EXISTS (SELECT 1 FROM bookmarks b WHERE b.keep = 1
    AND b.at_ms > segments.start_ms - 120000 AND b.at_ms < segments.end_ms + 120000
    AND (b.camera IS NULL OR b.camera = segments.camera))";

/// One bookmark as stored. `camera` is the camera's slug; None = all cameras.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub id: i64,
    pub camera: Option<String>,
    pub at_ms: i64,
    pub note: String,
    /// Who made it (shown).
    pub user: String,
    /// The account that may change it besides admins: who made it, until that
    /// account is deleted (None then; a new account of the same name gets nothing).
    pub owner: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub keep: bool,
}

/// What [`Db::bookmarks_list`] returns.
#[derive(Debug, Clone, Default)]
pub struct BookmarkFilter {
    /// Moments in `[from_ms, to_ms]` (None: open-ended).
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// This camera's bookmarks (a slug) and the all-cameras ones.
    pub camera: Option<String>,
    /// Camera slugs the caller may see (None: all). All-cameras bookmarks
    /// are always seen.
    pub visible: Option<std::collections::HashSet<String>>,
    /// Part of the note, any case.
    pub q: Option<String>,
    pub limit: usize,
}

const BM_COLS: &str = "id, camera, at_ms, note, user, created_ms, updated_ms, keep, owner";

fn bookmark_from_row(r: &Row) -> rusqlite::Result<Bookmark> {
    Ok(Bookmark {
        id: r.get(0)?,
        camera: r.get(1)?,
        at_ms: r.get(2)?,
        note: r.get(3)?,
        user: r.get(4)?,
        created_ms: r.get(5)?,
        updated_ms: r.get(6)?,
        keep: r.get::<_, i64>(7)? != 0,
        owner: r.get(8)?,
    })
}

/// Why a bookmark was not saved: a server-wide limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookmarkCap {
    /// The server has `max_total` bookmarks.
    TooMany,
    /// `max_kept` bookmarks keep video already.
    KeptFull,
}

impl Db {
    /// Save a new bookmark (its `id` is ignored) unless a limit is reached:
    /// the counts and the insert are one transaction, so two saves at once
    /// cannot both pass the last free place. Returns the id.
    pub fn bookmark_add_capped(&self, b: &Bookmark, max_total: i64, max_kept: i64) -> Result<std::result::Result<i64, BookmarkCap>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let total: i64 = tx.query_row("SELECT COUNT(*) FROM bookmarks", [], |r| r.get(0))?;
        if total >= max_total {
            return Ok(Err(BookmarkCap::TooMany));
        }
        if b.keep {
            let kept: i64 = tx.query_row("SELECT COUNT(*) FROM bookmarks WHERE keep = 1", [], |r| r.get(0))?;
            if kept >= max_kept {
                return Ok(Err(BookmarkCap::KeptFull));
            }
        }
        tx.execute(
            "INSERT INTO bookmarks (camera, at_ms, note, user, owner, created_ms, updated_ms, keep) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![b.camera, b.at_ms, b.note, b.user, b.owner, b.created_ms, b.updated_ms, b.keep as i64],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Ok(id))
    }

    /// Write back a changed bookmark, unless it now keeps video and
    /// `max_kept` others keep video already (count and write in one
    /// transaction). Ok(false): it is gone.
    pub fn bookmark_update_capped(&self, b: &Bookmark, max_kept: i64) -> Result<std::result::Result<bool, BookmarkCap>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        if b.keep {
            let others: i64 = tx.query_row("SELECT COUNT(*) FROM bookmarks WHERE keep = 1 AND id != ?1", params![b.id], |r| r.get(0))?;
            let was: bool = tx.query_row("SELECT keep FROM bookmarks WHERE id = ?1", params![b.id], |r| r.get::<_, i64>(0)).optional()?.is_some_and(|k| k != 0);
            if !was && others >= max_kept {
                return Ok(Err(BookmarkCap::KeptFull));
            }
        }
        let n = tx.execute(
            "UPDATE bookmarks SET camera = ?2, note = ?3, keep = ?4, updated_ms = ?5 WHERE id = ?1",
            params![b.id, b.camera, b.note, b.keep as i64, b.updated_ms],
        )?;
        tx.commit()?;
        Ok(Ok(n > 0))
    }

    /// Save a new bookmark (its `id` is ignored); returns the id.
    #[cfg(test)]
    pub fn bookmark_add(&self, b: &Bookmark) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO bookmarks (camera, at_ms, note, user, owner, created_ms, updated_ms, keep) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![b.camera, b.at_ms, b.note, b.user, b.owner, b.created_ms, b.updated_ms, b.keep as i64],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn bookmark_get(&self, id: i64) -> Result<Option<Bookmark>> {
        let conn = self.lock();
        Ok(conn.query_row(&format!("SELECT {BM_COLS} FROM bookmarks WHERE id = ?1"), params![id], bookmark_from_row).optional()?)
    }

    /// Write back a changed bookmark (camera, note, keep, updated time); false when it is gone.
    #[cfg(test)]
    pub fn bookmark_update(&self, b: &Bookmark) -> Result<bool> {
        let n = self.lock().execute(
            "UPDATE bookmarks SET camera = ?2, note = ?3, keep = ?4, updated_ms = ?5 WHERE id = ?1",
            params![b.id, b.camera, b.note, b.keep as i64, b.updated_ms],
        )?;
        Ok(n > 0)
    }

    /// Delete one; false when there was none. (Its hold on the recordings goes with it.)
    pub fn bookmark_delete(&self, id: i64) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM bookmarks WHERE id = ?1", params![id])? > 0)
    }

    /// Bookmarks that hold recordings back from cleanup.
    pub fn bookmark_kept_count(&self) -> Result<i64> {
        Ok(self.lock_ro().query_row("SELECT COUNT(*) FROM bookmarks WHERE keep = 1", [], |r| r.get(0))?)
    }

    #[cfg(test)]
    pub fn bookmark_count(&self) -> Result<i64> {
        Ok(self.lock().query_row("SELECT COUNT(*) FROM bookmarks", [], |r| r.get(0))?)
    }

    /// Bookmarks matching `f`, newest moment first, at most `f.limit`.
    pub fn bookmarks_list(&self, f: &BookmarkFilter) -> Result<Vec<Bookmark>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {BM_COLS} FROM bookmarks WHERE at_ms >= ?1 AND at_ms <= ?2
               AND (?3 IS NULL OR camera IS NULL OR camera = ?3)
             ORDER BY at_ms DESC, id DESC"
        ))?;
        let q = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty()).map(str::to_lowercase);
        let rows = st.query_map(params![f.from_ms.unwrap_or(i64::MIN), f.to_ms.unwrap_or(i64::MAX), f.camera], bookmark_from_row)?;
        let mut out = Vec::new();
        // (Visibility and the note search in Rust: the search is any case for
        // every language, which SQLite's LIKE is not.)
        for r in rows {
            if out.len() >= f.limit {
                break;
            }
            let b = r?;
            if let (Some(v), Some(c)) = (&f.visible, &b.camera) {
                if !v.contains(c) {
                    continue;
                }
            }
            if q.as_ref().is_some_and(|q| !b.note.to_lowercase().contains(q.as_str())) {
                continue;
            }
            out.push(b);
        }
        Ok(out)
    }

    /// A camera was removed: its bookmarks stay, but no longer hold its video.
    pub fn bookmarks_camera_removed(&self, camera: &str) -> Result<usize> {
        Ok(self.lock().execute("UPDATE bookmarks SET keep = 0 WHERE camera = ?1 AND keep = 1", params![camera])?)
    }

    /// An account was deleted: its bookmarks are no one's (admins still change them).
    pub fn bookmarks_user_removed(&self, user: &str) -> Result<usize> {
        Ok(self.lock().execute("UPDATE bookmarks SET owner = NULL WHERE owner = ?1", params![user])?)
    }

    /// The indexed recordings kept bookmarks hold back from cleanup, oldest
    /// first. Looked up from the bookmarks' side (each a short range of the
    /// segments' camera+start or start index), not by probing every recording.
    pub fn bookmark_kept_segments(&self) -> Result<Vec<SegmentRow>> {
        // (A read: one query per kept bookmark must not hold the write connection.)
        let conn = self.lock_ro();
        let kept: Vec<(Option<String>, i64)> = {
            let mut st = conn.prepare_cached("SELECT camera, at_ms FROM bookmarks WHERE keep = 1")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut out: Vec<SegmentRow> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (camera, at) in kept {
            let (from, to) = (at - KEEP_WINDOW_MS, at + KEEP_WINDOW_MS);
            let rows: Vec<SegmentRow> = match &camera {
                Some(c) => {
                    let mut st = conn.prepare_cached(&format!(
                        "SELECT {SEG_COLS} FROM segments WHERE camera = ?1 AND start_ms < ?3 AND start_ms >= ?4 AND end_ms > ?2"
                    ))?;
                    let rows = st.query_map(params![c, from, to, self.earliest_start(from)], segment_from_row)?;
                    rows.collect::<rusqlite::Result<_>>()?
                }
                None => {
                    let mut st = conn.prepare_cached(&format!(
                        "SELECT {SEG_COLS} FROM segments WHERE start_ms < ?2 AND start_ms >= ?3 AND end_ms > ?1"
                    ))?;
                    let rows = st.query_map(params![from, to, self.earliest_start(from)], segment_from_row)?;
                    rows.collect::<rusqlite::Result<_>>()?
                }
            };
            for r in rows {
                if seen.insert((r.folder.clone(), r.path.clone())) {
                    out.push(r);
                }
            }
        }
        out.sort_by_key(|r| r.start_ms);
        Ok(out)
    }

    /// Kept bookmarks, and the bytes of the recordings they hold (for the storage page).
    pub fn bookmark_kept_summary(&self) -> Result<(i64, i64)> {
        let n = self.bookmark_kept_count()?;
        if n == 0 {
            return Ok((0, 0));
        }
        Ok((n, self.bookmark_kept_segments()?.iter().map(|s| s.bytes.max(0)).sum()))
    }

    /// Whether a kept bookmark holds the indexed recording at `path` (false
    /// when the index does not have it).
    pub fn segment_kept(&self, path: &str) -> Result<bool> {
        let conn = self.lock();
        Ok(conn
            .query_row(&format!("SELECT {KEPT_BY_BOOKMARK} FROM segments WHERE path = ?1"), params![path], |r| r.get::<_, i64>(0))
            .optional()?
            .is_some_and(|v| v != 0))
    }

    /// Oldest recordings across all cameras that no kept bookmark holds, as
    /// `(camera, row)` (age retention).
    pub fn oldest_unkept_segments(&self, limit: usize) -> Result<Vec<(String, SegmentRow)>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {SEG_COLS}, camera FROM segments WHERE NOT {KEPT_BY_BOOKMARK} ORDER BY start_ms, id LIMIT ?1"
        ))?;
        let rows = st.query_map(params![limit as i64], |r| Ok((r.get::<_, String>(7)?, segment_from_row(r)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(camera: Option<&str>, at_ms: i64, note: &str, user: &str, keep: bool) -> Bookmark {
        Bookmark { id: 0, camera: camera.map(str::to_string), at_ms, note: note.into(), user: user.into(), owner: Some(user.into()), created_ms: 1, updated_ms: 1, keep }
    }
    fn seg(path: &str, start_ms: i64, end_ms: i64) -> SegmentRow {
        SegmentRow { path: path.into(), start_ms, end_ms, bytes: 100, video_frames: 10, kind: "continuous".into(), folder: String::new() }
    }

    #[test]
    fn add_list_search_update_delete() {
        let db = Db::open_in_memory().unwrap();
        let a = db.bookmark_add(&bm(Some("porch"), 1000, "Delivery van ÄRGER", "otto", false)).unwrap();
        let b = db.bookmark_add(&bm(None, 2000, "Power cut", "ann", true)).unwrap();
        let c = db.bookmark_add(&bm(Some("garage"), 3000, "Door left open", "ann", false)).unwrap();
        let all = |f: BookmarkFilter| db.bookmarks_list(&BookmarkFilter { limit: 100, ..f }).unwrap().into_iter().map(|b| b.id).collect::<Vec<_>>();
        assert_eq!(all(Default::default()), [c, b, a], "newest first");
        assert_eq!(all(BookmarkFilter { camera: Some("porch".into()), ..Default::default() }), [b, a], "the camera's and the all-cameras ones");
        assert_eq!(all(BookmarkFilter { from_ms: Some(1500), to_ms: Some(2500), ..Default::default() }), [b]);
        assert_eq!(all(BookmarkFilter { q: Some("ärger".into()), ..Default::default() }), [a], "any case, any language");
        assert_eq!(all(BookmarkFilter { q: Some("  OPEN ".into()), ..Default::default() }), [c]);
        let vis: std::collections::HashSet<String> = ["porch".to_string()].into();
        assert_eq!(all(BookmarkFilter { visible: Some(vis), ..Default::default() }), [b, a], "a limited account: its cameras and all-cameras ones");
        assert_eq!(db.bookmarks_list(&BookmarkFilter { limit: 1, ..Default::default() }).unwrap().len(), 1);
        let mut x = db.bookmark_get(a).unwrap().unwrap();
        x.note = "Changed".into();
        x.keep = true;
        x.camera = None;
        x.updated_ms = 9;
        assert!(db.bookmark_update(&x).unwrap());
        assert_eq!(db.bookmark_get(a).unwrap().unwrap(), x);
        assert!(db.bookmark_delete(a).unwrap());
        assert!(!db.bookmark_delete(a).unwrap());
        assert!(db.bookmark_get(a).unwrap().is_none());
        assert_eq!(db.bookmark_count().unwrap(), 2);
        // A renamed camera keeps its bookmarks.
        db.rename_camera("Garage", "Shed").unwrap();
        assert_eq!(db.bookmark_get(c).unwrap().unwrap().camera.as_deref(), Some("shed"));
    }

    #[test]
    fn kept_bookmarks_hold_the_recordings_around_them() {
        let db = Db::open_in_memory().unwrap();
        let m = 60_000;
        // porch: 0-1, 1-2, ... 9-10 minutes; garage the same.
        for cam in ["porch", "garage"] {
            for i in 0..10 {
                db.insert_segment(cam, &seg(&format!("cameras/{cam}/{i}.mp4"), i * m, (i + 1) * m)).unwrap();
            }
        }
        let kept = |db: &Db| {
            let mut v: Vec<String> = db.bookmark_kept_segments().unwrap().into_iter().map(|s| s.path).collect();
            v.sort();
            v
        };
        assert!(kept(&db).is_empty());
        // Not kept: holds nothing.
        db.bookmark_add(&bm(Some("porch"), 5 * m + 30_000, "x", "ann", false)).unwrap();
        assert!(kept(&db).is_empty());
        // Kept at 5:30 on the porch: 3:30 to 7:30, so files 3..=7 of the porch only.
        let id = db.bookmark_add(&bm(Some("porch"), 5 * m + 30_000, "x", "ann", true)).unwrap();
        assert_eq!(kept(&db), (3..=7).map(|i| format!("cameras/porch/{i}.mp4")).collect::<Vec<_>>());
        let old: Vec<String> = db.oldest_unkept_segments(100).unwrap().into_iter().map(|(_, s)| s.path).collect();
        assert_eq!(old.len(), 15);
        assert!(!old.iter().any(|p| p == "cameras/porch/5.mp4"));
        let purge: Vec<String> = db.segments_to_purge("", 0, 100).unwrap().into_iter().map(|(s, _)| s.path).collect();
        assert_eq!(purge.len(), 15, "the caps and the drive reserve skip them too");
        assert!(!purge.iter().any(|p| p.starts_with("cameras/porch/") && ["3", "4", "5", "6", "7"].iter().any(|n| p.ends_with(&format!("/{n}.mp4")))));
        // An all-cameras one at exactly 0:00 holds 0-1 (the window's edges: a
        // recording ending exactly 2 min before is not held).
        db.bookmark_add(&bm(None, 0, "x", "ann", true)).unwrap();
        assert_eq!(kept(&db).len(), 5 + 4, "porch 0, 1 and garage 0, 1 (porch 3..7 already)");
        // Unticking keep and deleting release them.
        let mut b = db.bookmark_get(id).unwrap().unwrap();
        b.keep = false;
        db.bookmark_update(&b).unwrap();
        assert_eq!(kept(&db).len(), 4);
        db.lock().execute("DELETE FROM bookmarks WHERE camera IS NULL", []).unwrap();
        assert!(kept(&db).is_empty());
        assert_eq!(db.oldest_unkept_segments(100).unwrap().len(), 20);
        assert_eq!(KEEP_WINDOW_MS, 120_000, "KEPT_BY_BOOKMARK spells it out");
    }

    #[test]
    fn the_limits_are_checked_with_the_write() {
        let db = Db::open_in_memory().unwrap();
        let a = db.bookmark_add_capped(&bm(None, 1_000, "a", "ann", true), 3, 1).unwrap().unwrap();
        assert_eq!(db.bookmark_add_capped(&bm(None, 2_000, "b", "ann", true), 3, 1).unwrap(), Err(BookmarkCap::KeptFull));
        let b = db.bookmark_add_capped(&bm(None, 2_000, "b", "ann", false), 3, 1).unwrap().unwrap();
        // The one place is taken: another may not start keeping video ...
        let mut nb = db.bookmark_get(b).unwrap().unwrap();
        nb.keep = true;
        assert_eq!(db.bookmark_update_capped(&nb, 1).unwrap(), Err(BookmarkCap::KeptFull));
        // ... but the one that keeps it may still be changed.
        let mut na = db.bookmark_get(a).unwrap().unwrap();
        na.note = "changed".into();
        assert_eq!(db.bookmark_update_capped(&na, 1).unwrap(), Ok(true));
        db.bookmark_add_capped(&bm(None, 3_000, "c", "ann", false), 3, 1).unwrap().unwrap();
        assert_eq!(db.bookmark_add_capped(&bm(None, 4_000, "d", "ann", false), 3, 1).unwrap(), Err(BookmarkCap::TooMany));
        // Gone: false, not an error.
        nb.id = 999;
        nb.keep = false;
        assert_eq!(db.bookmark_update_capped(&nb, 1).unwrap(), Ok(false));
    }

    #[test]
    fn kept_recordings_are_found_from_the_bookmarks_among_many() {
        let db = Db::open_in_memory().unwrap();
        // 3 cameras x 20,000 five-second recordings (about 28 hours each).
        {
            let mut conn = db.lock();
            let tx = conn.transaction().unwrap();
            for cam in ["a", "b", "c"] {
                for i in 0..20_000i64 {
                    tx.execute("INSERT INTO segments (camera, path, start_ms, end_ms, bytes, video_frames, kind, folder) VALUES (?1, ?2, ?3, ?4, 10, 1, 'continuous', '')",
                        params![cam, format!("{cam}/{i}.mp4"), i * 5000, (i + 1) * 5000]).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        db.longest_segment.store(5000, std::sync::atomic::Ordering::Relaxed);
        db.bookmark_add(&bm(Some("b"), 50_000_000, "x", "ann", true)).unwrap();
        db.bookmark_add(&bm(None, 90_000_000, "x", "ann", true)).unwrap();
        db.bookmark_add(&bm(None, 70_000_000, "x", "ann", false)).unwrap();
        let t = std::time::Instant::now();
        let got = db.bookmark_kept_segments().unwrap();
        let took = t.elapsed();
        // The same as asking every recording.
        let mut want: Vec<String> = {
            let conn = db.lock();
            let mut st = conn.prepare(&format!("SELECT path FROM segments WHERE {KEPT_BY_BOOKMARK}")).unwrap();
            let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };
        let mut paths: Vec<String> = got.iter().map(|s| s.path.clone()).collect();
        assert!(got.windows(2).all(|w| w[0].start_ms <= w[1].start_ms), "oldest first");
        paths.sort();
        want.sort();
        // 4 min of 5 s files (48, +1 straddling an edge) on b; 3 x that for all cameras.
        assert_eq!(paths, want);
        assert_eq!(paths.len(), 48 + 3 * 48, "{}", paths.len());
        assert!(took < std::time::Duration::from_millis(200), "{took:?}");
        assert_eq!(db.bookmark_kept_summary().unwrap(), (2, 10 * 4 * 48));
        assert!(db.segment_kept("b/10000.mp4").unwrap());
        assert!(!db.segment_kept("a/10000.mp4").unwrap());
        assert!(!db.segment_kept("nowhere.mp4").unwrap());
    }

    #[test]
    fn removed_cameras_and_accounts_let_go() {
        let db = Db::open_in_memory().unwrap();
        let a = db.bookmark_add(&bm(Some("porch"), 1000, "x", "otto", true)).unwrap();
        let b = db.bookmark_add(&bm(None, 1000, "y", "otto", true)).unwrap();
        assert_eq!(db.bookmarks_camera_removed("porch").unwrap(), 1);
        let x = db.bookmark_get(a).unwrap().unwrap();
        assert!(!x.keep && x.note == "x", "the note stays, the hold goes");
        assert!(db.bookmark_get(b).unwrap().unwrap().keep);
        assert_eq!(db.bookmarks_user_removed("otto").unwrap(), 2);
        let y = db.bookmark_get(b).unwrap().unwrap();
        assert_eq!((y.user.as_str(), y.owner), ("otto", None), "still says who made it; no one owns it");
    }

    #[test]
    fn a_version_28_database_gains_the_bookmarks_table() {
        let dir = std::env::temp_dir().join(format!("ambersight-bm-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.db");
        {
            let db = Db::open(&p).unwrap();
            db.lock().execute_batch("DROP TABLE bookmarks; PRAGMA user_version = 28;").unwrap();
        }
        let db = Db::open(&p).unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let idx: i64 = db.lock().query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name IN ('idx_bookmarks_at', 'idx_bookmarks_keep')", [], |r| r.get(0)).unwrap();
        assert_eq!(idx, 2);
        db.bookmark_add(&bm(None, 1, "after the upgrade", "ann", true)).unwrap();
        // Opening again changes nothing.
        drop(db);
        let db = Db::open(&p).unwrap();
        assert_eq!(db.bookmark_count().unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v29_bookmarks_table_without_owner_gains_it() {
        let dir = std::env::temp_dir().join(format!("ambersight-bm-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.db");
        drop(Db::open(&p).unwrap());
        {
            // As the earlier development build left it.
            let c = rusqlite::Connection::open(&p).unwrap();
            c.execute_batch(
                "DROP TABLE bookmarks;
                 CREATE TABLE bookmarks (id INTEGER PRIMARY KEY, camera TEXT, at_ms INTEGER NOT NULL, note TEXT NOT NULL, user TEXT NOT NULL,
                    created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL, keep INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO bookmarks (camera, at_ms, note, user, created_ms, updated_ms, keep) VALUES ('porch', 5, 'old', 'otto', 1, 1, 1);",
            )
            .unwrap();
        }
        let db = Db::open(&p).unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let b = db.bookmarks_list(&BookmarkFilter { limit: 10, ..Default::default() }).unwrap();
        assert_eq!((b.len(), b[0].owner.as_deref(), b[0].note.as_str()), (1, Some("otto"), "old"));
        db.bookmark_add(&bm(None, 9, "new", "ann", false)).unwrap();
        drop(db);
        // Opening again changes nothing.
        assert_eq!(Db::open(&p).unwrap().bookmark_count().unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_database_has_the_bookmarks_table() {
        let db = Db::open_in_memory().unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(db.bookmark_count().unwrap(), 0);
    }
}
