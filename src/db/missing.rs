//! Database side of "recordings whose files are gone" (see crate::missing):
//! reading the index in pieces on a reading connection, and removing rows a
//! few thousand at a time so recording is never held up for long.

use super::*;

/// One indexed recording as the missing-files check reads it.
#[derive(Debug, Clone)]
pub struct SegRef {
    pub id: i64,
    pub folder: String,
    pub path: String,
    pub bytes: i64,
    pub camera: String,
    pub start_ms: i64,
}

/// An event as the missing-files check needs it.
#[derive(Debug, Clone)]
pub struct EventRef {
    pub id: String,
    pub camera: String,
    pub start: i64,
    pub end: i64,
    pub protect: bool,
}

/// A short fingerprint of a recording's path (kept instead of the path for
/// millions of rows; see `delete_segments_in`).
pub fn path_hash(p: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

impl Db {
    /// Recordings with an id above `after`, in id order, at most `limit`.
    pub fn segment_refs_after(&self, after: i64, limit: usize) -> Result<Vec<SegRef>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare_cached("SELECT id, folder, path, bytes, camera, start_ms FROM segments WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let rows = st.query_map(params![after, limit as i64], |r| {
            Ok(SegRef { id: r.get(0)?, folder: r.get(1)?, path: r.get(2)?, bytes: r.get(3)?, camera: r.get(4)?, start_ms: r.get(5)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The recordings with these ids that are still there (ids gone are left out).
    pub fn segment_refs_by_id(&self, ids: &[i64]) -> Result<Vec<SegRef>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare_cached("SELECT id, folder, path, bytes, camera, start_ms FROM segments WHERE id = ?1")?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let r = st
                .query_row(params![id], |r| {
                    Ok(SegRef { id: r.get(0)?, folder: r.get(1)?, path: r.get(2)?, bytes: r.get(3)?, camera: r.get(4)?, start_ms: r.get(5)? })
                })
                .optional()?;
            out.extend(r);
        }
        Ok(out)
    }

    /// Number of indexed recordings (an estimate is fine: for progress).
    pub fn segment_count_estimate(&self) -> Result<i64> {
        if self.segment_totals_ready() {
            return Ok(self.lock_ro().query_row("SELECT COALESCE(SUM(n), 0) FROM seg_totals", [], |r| r.get(0))?);
        }
        Ok(self.lock_ro().query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))?)
    }

    /// Folder names recordings are indexed under ("" = the data folder), when
    /// that can be told cheaply (from the running totals); None otherwise.
    pub fn segment_folders_cheap(&self) -> Result<Option<Vec<String>>> {
        if !self.segment_totals_ready() {
            return Ok(None);
        }
        let conn = self.lock_ro();
        let mut st = conn.prepare("SELECT DISTINCT folder FROM seg_totals WHERE n > 0 ORDER BY folder")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(Some(rows.collect::<rusqlite::Result<_>>()?))
    }

    /// Up to `n` recordings of one folder spread evenly over the time it
    /// covers (each found through the folder/time index: cheap on any size).
    pub fn segment_sample(&self, folder: &str, n: usize) -> Result<Vec<String>> {
        let conn = self.lock_ro();
        let span: Option<(i64, i64)> = conn
            .query_row("SELECT MIN(start_ms), MAX(start_ms) FROM segments WHERE folder = ?1", params![folder], |r| {
                Ok(match (r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?) {
                    (Some(a), Some(b)) => Some((a, b)),
                    _ => None,
                })
            })?;
        let Some((lo, hi)) = span else { return Ok(Vec::new()) };
        let mut st = conn.prepare_cached("SELECT path FROM segments WHERE folder = ?1 AND start_ms >= ?2 ORDER BY start_ms LIMIT 1")?;
        let mut out: Vec<String> = Vec::new();
        let n = n.max(1);
        for i in 0..n {
            let at = if n == 1 { lo } else { lo + ((hi - lo) as i128 * i as i128 / (n - 1) as i128) as i64 };
            if let Some(p) = st.query_row(params![folder, at], |r| r.get::<_, String>(0)).optional()? {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        Ok(out)
    }

    /// Events that may play from the recordings (they have a clip address),
    /// with an id above `after` in id order, at most `limit`.
    pub fn events_with_clip_after(&self, after: &str, limit: usize) -> Result<Vec<EventRef>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare_cached("SELECT id, camera, start, end, protect FROM events WHERE clip IS NOT NULL AND id > ?1 ORDER BY id LIMIT ?2")?;
        let rows = st.query_map(params![after, limit as i64], |r| {
            Ok(EventRef { id: r.get(0)?, camera: r.get(1)?, start: r.get(2)?, end: r.get(3)?, protect: r.get::<_, i64>(4)? != 0 })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Ids of a camera's recordings (`camera` = slug) overlapping `from_ms..to_ms`.
    pub fn segment_ids_overlapping(&self, camera: &str, from_ms: i64, to_ms: i64) -> Result<Vec<i64>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare_cached("SELECT id FROM segments WHERE camera = ?1 AND start_ms < ?3 AND start_ms >= ?4 AND end_ms > ?2")?;
        let rows = st.query_map(params![camera, from_ms, to_ms, self.earliest_start(from_ms)], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Remove these recordings (id, folder, start, hash of the path) from
    /// the index, each only if it is still in that folder (one moved to the
    /// archive meanwhile is left alone) with that start and path (not an id
    /// used again since). One transaction; returns (rows, bytes). The running
    /// totals follow through their triggers.
    pub fn delete_segments_in(&self, rows: &[(i64, &str, i64, u64)]) -> Result<(u64, i64)> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let (mut n, mut bytes) = (0u64, 0i64);
        {
            let mut find = tx.prepare_cached("SELECT path FROM segments WHERE id = ?1 AND folder = ?2 AND start_ms = ?3")?;
            let mut del = tx.prepare_cached("DELETE FROM segments WHERE id = ?1 AND path = ?2 RETURNING bytes")?;
            for (id, folder, start, hash) in rows {
                let Some(path) = find.query_row(params![id, folder, start], |r| r.get::<_, String>(0)).optional()? else { continue };
                if path_hash(&path) != *hash {
                    continue;
                }
                if let Some(b) = del.query_row(params![id, path], |r| r.get::<_, i64>(0)).optional()? {
                    n += 1;
                    bytes += b;
                }
            }
        }
        tx.commit()?;
        Ok((n, bytes))
    }

    /// The highest recording id now (0: none).
    pub fn segment_max_id(&self) -> Result<i64> {
        Ok(self.lock_ro().query_row("SELECT COALESCE(MAX(id), 0) FROM segments", [], |r| r.get(0))?)
    }

    /// Cameras (slugs) with recordings.
    pub fn segment_cameras(&self) -> Result<Vec<String>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare("SELECT DISTINCT camera FROM segments")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Start of a camera's oldest recording whose id is not in `gone` (sorted):
    /// its oldest once those are removed.
    pub fn first_segment_start_not_in(&self, camera: &str, gone: &[i64]) -> Result<Option<i64>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare_cached("SELECT id, start_ms FROM segments WHERE camera = ?1 ORDER BY start_ms, id")?;
        let mut rows = st.query(params![camera])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            if gone.binary_search(&id).is_err() {
                return Ok(Some(r.get(1)?));
            }
        }
        Ok(None)
    }

    /// The event as the missing-files check needs it (None: gone).
    pub fn event_ref(&self, id: &str) -> Result<Option<(EventRef, bool)>> {
        Ok(self
            .lock_ro()
            .query_row("SELECT id, camera, start, end, protect, clip IS NOT NULL FROM events WHERE id = ?1", params![id], |r| {
                Ok((EventRef { id: r.get(0)?, camera: r.get(1)?, start: r.get(2)?, end: r.get(3)?, protect: r.get::<_, i64>(4)? != 0 }, r.get::<_, i64>(5)? != 0))
            })
            .optional()?)
    }

    /// Mark an unprotected event as having no video any more (the event list
    /// shows it with "No clip"; it is kept until retention removes it by age).
    pub fn clear_event_clip(&self, id: &str) -> Result<bool> {
        Ok(self.lock().execute("UPDATE events SET clip = NULL WHERE id = ?1 AND protect = 0 AND clip IS NOT NULL", params![id])? > 0)
    }

    /// Undo `clear_event_clip` (its video turned out to be there).
    pub fn restore_event_clip(&self, id: &str) -> Result<bool> {
        Ok(self.lock().execute("UPDATE events SET clip = '/api/clips/' || id WHERE id = ?1 AND clip IS NULL", params![id])? > 0)
    }
}
