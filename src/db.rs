//! SQLite store for events and recording segments (`<data_dir>/ambersight.db`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

use crate::engine::Event;

mod account;
mod devices;
mod digest;
mod plates;
pub use account::{AccountRequest, TokenRow};
pub use devices::StoredSession;
pub use digest::DigestEvent;
pub use plates::{PlateFilter, PlateRow};

const SCHEMA_VERSION: i64 = 26;
/// Different signal names kept at most.
const MAX_SIGNALS: i64 = 200;

/// One recording segment file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRow {
    pub path: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub bytes: i64,
    pub video_frames: i64,
    /// "continuous" | "triggered" | "sub" (sub stream) | "legacy"
    pub kind: String,
    /// Recording folder name ("" = the data folder); `path` is relative to it.
    pub folder: String,
}

/// Filters for [`Db::query_events`]; `None` / empty means "no filter".
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    pub camera: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub object: Option<String>,
    pub limit: usize,
    pub offset: usize,
    /// Cameras whose unconfirmed events (AI found nothing that counts) are left out.
    pub hide_unconfirmed: Vec<String>,
    /// Only these cameras (empty: any).
    pub cameras: Vec<String>,
    /// Review verdicts wanted: "false_alarm", "ok", "none" (not reviewed);
    /// empty: any.
    pub review: Vec<String>,
    /// Only protected events.
    pub protected_only: bool,
    /// Events with any of these labels (substring, like `object`).
    pub objects_any: Vec<String>,
    /// Only these event ids (empty: any).
    pub ids: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub events_imported: usize,
    pub events_skipped: usize,
    pub segments_imported: usize,
}

/// One browser that gets notifications (Web Push subscription).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushSub {
    pub id: i64,
    /// The push service address this browser gave (FCM, Mozilla, Apple...).
    pub endpoint: String,
    pub user: String,
    /// The browser's public key and secret (base64url), for encryption.
    pub p256dh: String,
    pub auth: String,
    /// "Chrome on Android" and the like, or the name a phone app gave.
    pub device: String,
    pub created_ms: i64,
    pub last_ok_ms: Option<i64>,
    /// "browser" (Web Push) or "app" (a phone app through UnifiedPush).
    pub kind: String,
}

/// A phone reporting whether it is at home (presence).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PresenceDevice {
    /// The account it reports for ("" without accounts).
    pub user: String,
    /// Chosen by the app (a random id kept on the phone).
    pub device_id: String,
    pub name: String,
    pub home: bool,
    pub updated_ms: i64,
    pub created_ms: i64,
}

/// The presence rule's memory as stored (see engine/presence.rs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PresenceRuleRow {
    /// Last known state: Some(true) someone home, Some(false) everyone away.
    pub known_home: Option<bool>,
    /// When everyone left, while the away action is pending (epoch ms).
    pub left_at_ms: Option<i64>,
    /// Presence did the away action (and nobody has come back since).
    pub acted_away: bool,
}

/// The database connection, held. Holding it for long holds up everything
/// else that needs the database (recording index, web pages, AI), so a long
/// hold is logged with where in this file it was taken.
pub(crate) struct TimedLock<'a> {
    guard: MutexGuard<'a, Connection>,
    at: &'static std::panic::Location<'static>,
    asked: std::time::Instant,
    got: std::time::Instant,
}

/// Holds at least this long are logged.
const SLOW_HOLD: std::time::Duration = std::time::Duration::from_millis(1000);

impl std::ops::Deref for TimedLock<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.guard
    }
}

impl std::ops::DerefMut for TimedLock<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.guard
    }
}

impl Drop for TimedLock<'_> {
    fn drop(&mut self) {
        let held = self.got.elapsed();
        if held >= SLOW_HOLD {
            tracing::info!(
                "database held {} ms by db.rs:{} (waited {} ms for it)",
                held.as_millis(),
                self.at.line(),
                self.got.duration_since(self.asked).as_millis()
            );
        }
    }
}

/// Keep the one-line verdict of an AI review (see `ai_review_sums`).
fn put_review_sum(conn: &Connection, event_id: &str, camera: &str, start: i64, alerted: bool, detail: &str) {
    if let Ok(d) = serde_json::from_str::<serde_json::Value>(detail) {
        let _ = conn.execute(
            "INSERT OR REPLACE INTO ai_review_sums (event_id, camera, start, alerted, summary) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![event_id, camera, start, alerted, crate::ai_review::summarize(&d).to_string()],
        );
    }
}

pub struct Db {
    conn: Mutex<Connection>,
    /// A few read-only connections to the same file for the heavy reads
    /// (totals and counts over the whole recording index, the Status page's
    /// figures, the AI review page). The database lets readers run beside the
    /// writer, so a slow read never holds up recording or events, and with
    /// several of them one slow read does not hold up every page either.
    /// Empty for an in-memory database (tests), which then uses `conn`.
    ro: Vec<Mutex<Connection>>,
    /// Which read connection to wait for when all are busy (taken in turn).
    ro_next: std::sync::atomic::AtomicUsize,
    /// The longest recording segment stored (ms): bounds lookups of the
    /// segments overlapping a time by their start, so they read only that
    /// stretch of the index instead of a camera's whole history.
    longest_segment: std::sync::atomic::AtomicI64,
    /// True while the start-up indexing of existing recordings runs.
    importing: std::sync::atomic::AtomicBool,
    import_started: Mutex<Option<std::time::Instant>>,
    import_warned: std::sync::atomic::AtomicBool,
    /// The one-line verdicts were checked against the reviews in this run
    /// (see `backfill_review_sums`): an older version run in between keeps
    /// reviews without them.
    sums_checked: std::sync::atomic::AtomicBool,
    /// The running recording totals (`seg_totals`, `seg_days`) are complete.
    totals_ready: std::sync::atomic::AtomicBool,
    /// Smart search: every event that started before this (s) was looked at
    /// when the whole list was last checked (at that instant). See `smart_pending`.
    smart_hint: Mutex<Option<(i64, std::time::Instant)>>,
    /// The last `smart_counts` and when.
    smart_counts_cache: Mutex<Option<(std::time::Instant, (i64, i64, i64))>>,
}

/// Read connections (see `Db::ro`).
const READ_CONNECTIONS: usize = 4;

/// An index with at most this many recordings has its totals counted at open;
/// a bigger one in the background.
const SEG_TOTALS_FILL_NOW: i64 = 100_000;

/// Recordings counted into the totals per step of the background fill.
const SEG_TOTALS_STEP: i64 = 5_000;

/// `seg_totals_fill.upto` once every recording is counted.
const SEG_TOTALS_ALL: i64 = i64::MAX;

/// Keep `seg_totals` and `seg_days` up to date with `segments`. A row counts
/// once the fill has passed its id (`seg_totals_fill.upto`); until then the
/// fill counts it as it is then.
const SEG_TOTALS_TRIGGERS: &str = "
DROP TRIGGER IF EXISTS seg_totals_ins;
DROP TRIGGER IF EXISTS seg_totals_del;
DROP TRIGGER IF EXISTS seg_totals_upd;
CREATE TRIGGER seg_totals_ins AFTER INSERT ON segments
WHEN NEW.id <= (SELECT upto FROM seg_totals_fill WHERE id = 1) BEGIN
  INSERT INTO seg_totals (folder, camera, bytes, n) VALUES (NEW.folder, NEW.camera, NEW.bytes, 1)
    ON CONFLICT (folder, camera) DO UPDATE SET bytes = bytes + excluded.bytes, n = n + 1;
  INSERT INTO seg_days (camera, day, bytes, ms, n) VALUES (NEW.camera, NEW.start_ms / 86400000, NEW.bytes, MAX(NEW.end_ms - NEW.start_ms, 0), 1)
    ON CONFLICT (camera, day) DO UPDATE SET bytes = bytes + excluded.bytes, ms = ms + excluded.ms, n = n + 1;
END;
CREATE TRIGGER seg_totals_del AFTER DELETE ON segments
WHEN OLD.id <= (SELECT upto FROM seg_totals_fill WHERE id = 1) BEGIN
  UPDATE seg_totals SET bytes = bytes - OLD.bytes, n = n - 1 WHERE folder = OLD.folder AND camera = OLD.camera;
  DELETE FROM seg_totals WHERE folder = OLD.folder AND camera = OLD.camera AND n <= 0;
  UPDATE seg_days SET bytes = bytes - OLD.bytes, ms = ms - MAX(OLD.end_ms - OLD.start_ms, 0), n = n - 1
    WHERE camera = OLD.camera AND day = OLD.start_ms / 86400000;
  DELETE FROM seg_days WHERE camera = OLD.camera AND day = OLD.start_ms / 86400000 AND n <= 0;
END;
CREATE TRIGGER seg_totals_upd AFTER UPDATE OF camera, folder, bytes, start_ms, end_ms ON segments
WHEN OLD.id <= (SELECT upto FROM seg_totals_fill WHERE id = 1) BEGIN
  INSERT INTO seg_totals (folder, camera, bytes, n) VALUES (NEW.folder, NEW.camera, NEW.bytes, 1)
    ON CONFLICT (folder, camera) DO UPDATE SET bytes = bytes + excluded.bytes, n = n + 1;
  INSERT INTO seg_days (camera, day, bytes, ms, n) VALUES (NEW.camera, NEW.start_ms / 86400000, NEW.bytes, MAX(NEW.end_ms - NEW.start_ms, 0), 1)
    ON CONFLICT (camera, day) DO UPDATE SET bytes = bytes + excluded.bytes, ms = ms + excluded.ms, n = n + 1;
  UPDATE seg_totals SET bytes = bytes - OLD.bytes, n = n - 1 WHERE folder = OLD.folder AND camera = OLD.camera;
  DELETE FROM seg_totals WHERE folder = OLD.folder AND camera = OLD.camera AND n <= 0;
  UPDATE seg_days SET bytes = bytes - OLD.bytes, ms = ms - MAX(OLD.end_ms - OLD.start_ms, 0), n = n - 1
    WHERE camera = OLD.camera AND day = OLD.start_ms / 86400000;
  DELETE FROM seg_days WHERE camera = OLD.camera AND day = OLD.start_ms / 86400000 AND n <= 0;
END;
";

fn segment_from_row(r: &Row) -> rusqlite::Result<SegmentRow> {
    Ok(SegmentRow {
        path: r.get(0)?,
        start_ms: r.get(1)?,
        end_ms: r.get(2)?,
        bytes: r.get(3)?,
        video_frames: r.get(4)?,
        kind: r.get(5)?,
        folder: r.get(6)?,
    })
}

const SEG_COLS: &str = "path, start_ms, end_ms, bytes, video_frames, kind, folder";
const EV_COLS: &str = "id, camera_id, camera, start, end, score, clip, thumb, objects, protect";

fn event_from_row(r: &Row) -> rusqlite::Result<Event> {
    let objects: String = r.get(8)?;
    Ok(Event {
        id: r.get(0)?,
        camera_id: r.get::<_, i64>(1)? as usize,
        camera: r.get(2)?,
        start: r.get(3)?,
        end: r.get(4)?,
        score: r.get(5)?,
        clip: r.get(6)?,
        thumb: r.get(7)?,
        objects: serde_json::from_str(&objects).unwrap_or_default(),
        protect: r.get::<_, i64>(9)? != 0,
    })
}

/// Whether a search word asks for a system tag ("loitering: Gate", "rule: x",
/// "plate ABC") rather than for something seen.
fn word_is_tag(w: &str) -> bool {
    let w = w.to_ascii_lowercase();
    w.contains(": ") || ["rule", "quiet", "loitering", "crossed", "plate", "unknown plate"].iter().any(|p| w.starts_with(p))
}

/// SQL for one label (`value`) matching a search word: a substring, but a
/// rule, zone or plate tag only when the word itself asks for such a tag
/// (so "person" does not find "rule: person on the porch").
fn label_cond(word: &str) -> String {
    if word_is_tag(word) {
        "instr(lower(value), ?) > 0".into()
    } else {
        let tags = crate::tracker::TAG_PREFIXES.iter().map(|p| format!("lower(value) LIKE '{p}%'")).collect::<Vec<_>>().join(" OR ");
        format!("(instr(lower(value), ?) > 0 AND NOT ({tags}))")
    }
}

/// The WHERE part of an event query: (SELECT ... WHERE ..., its arguments).
fn event_filter_sql(q: &EventQuery, cols: &str) -> (String, Vec<rusqlite::types::Value>) {
    let mut sql = format!("SELECT {cols} FROM events WHERE end >= ? AND start <= ?");
    let mut args: Vec<rusqlite::types::Value> = vec![
        q.since.unwrap_or(0).into(),
        q.until.unwrap_or(i64::MAX).into(),
    ];
    if let Some(cam) = q.camera.as_deref().filter(|c| !c.is_empty()) {
        sql.push_str(" AND camera = ? COLLATE NOCASE");
        args.push(cam.to_string().into());
    }
    if let Some(obj) = q.object.as_deref().filter(|o| !o.is_empty()) {
        sql.push_str(&format!(" AND EXISTS (SELECT 1 FROM json_each(events.objects) WHERE {})", label_cond(obj)));
        args.push(obj.to_ascii_lowercase().into());
    }
    if !q.cameras.is_empty() {
        let marks = vec!["?"; q.cameras.len()].join(", ");
        sql.push_str(&format!(" AND camera COLLATE NOCASE IN ({marks})"));
        args.extend(q.cameras.iter().map(|c| c.clone().into()));
    }
    if !q.objects_any.is_empty() {
        let conds = q.objects_any.iter().map(|o| format!("({})", label_cond(o))).collect::<Vec<_>>().join(" OR ");
        sql.push_str(&format!(" AND EXISTS (SELECT 1 FROM json_each(events.objects) WHERE {conds})"));
        args.extend(q.objects_any.iter().map(|o| o.to_ascii_lowercase().into()));
    }
    if q.protected_only {
        sql.push_str(" AND protect = 1");
    }
    if !q.review.is_empty() {
        let verdicts: Vec<&String> = q.review.iter().filter(|r| r.as_str() != "none").collect();
        let mut conds: Vec<String> = Vec::new();
        if verdicts.len() < q.review.len() {
            conds.push("review IS NULL".into());
        }
        if !verdicts.is_empty() {
            conds.push(format!("review IN ({})", vec!["?"; verdicts.len()].join(", ")));
            args.extend(verdicts.iter().map(|v| (*v).clone().into()));
        }
        sql.push_str(&format!(" AND ({})", conds.join(" OR ")));
    }
    if !q.ids.is_empty() {
        let marks = vec!["?"; q.ids.len()].join(", ");
        sql.push_str(&format!(" AND id IN ({marks})"));
        args.extend(q.ids.iter().map(|c| c.clone().into()));
    }
    if !q.hide_unconfirmed.is_empty() {
        let marks = vec!["?"; q.hide_unconfirmed.len()].join(", ");
        sql.push_str(&format!(" AND NOT (unconfirmed = 1 AND camera COLLATE NOCASE IN ({marks}))"));
        args.extend(q.hide_unconfirmed.iter().map(|c| c.clone().into()));
    }
    (sql, args)
}

fn upsert_event(conn: &Connection, ev: &Event) -> Result<()> {
    conn.execute(
        "INSERT INTO events (id, camera_id, camera, start, end, score, clip, thumb, objects, protect)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET camera_id=excluded.camera_id, camera=excluded.camera,
           start=excluded.start, end=excluded.end, score=excluded.score, clip=excluded.clip,
           thumb=excluded.thumb, objects=excluded.objects, protect=excluded.protect",
        params![
            ev.id,
            ev.camera_id as i64,
            ev.camera,
            ev.start,
            ev.end,
            ev.score,
            ev.clip,
            ev.thumb,
            serde_json::to_string(&ev.objects)?,
            ev.protect as i64
        ],
    )?;
    Ok(())
}

/// Longest an event's footage span can be (events are split long before
/// this, pre/post roll included): bounds the event-footage lookups.
const MAX_EVENT_SPAN_MS: i64 = 12 * 3600 * 1000;
/// Longest a recording segment can be: bounds segment lookups by start.
const MAX_SEGMENT_MS: i64 = 2 * 3600 * 1000;

fn insert_segment_on(conn: &Connection, camera: &str, s: &SegmentRow) -> Result<()> {
    conn.execute(
        "INSERT INTO segments (camera, path, start_ms, end_ms, bytes, video_frames, kind, folder)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(path) DO UPDATE SET camera=excluded.camera, start_ms=excluded.start_ms,
           end_ms=excluded.end_ms, bytes=excluded.bytes, video_frames=excluded.video_frames,
           kind=excluded.kind, folder=excluded.folder",
        params![camera, s.path, s.start_ms, s.end_ms, s.bytes, s.video_frames, s.kind, s.folder],
    )?;
    // Footage of an event already saved (its post-roll closes after it).
    // (An event's span is bounded, so only recent ones need looking at.)
    conn.execute(
        "UPDATE segments SET event = 1 WHERE path = ?1 AND event = 0 AND EXISTS
           (SELECT 1 FROM events WHERE span_cam = ?2 AND span_from < ?4 AND span_from > ?3 - ?5 AND span_to > ?3)",
        params![s.path, camera, s.start_ms, s.end_ms, MAX_EVENT_SPAN_MS],
    )?;
    Ok(())
}

/// One stored fingerprint of an event picture or of an object in it.
#[derive(Debug, Clone)]
pub struct SmartRow {
    pub id: i64,
    pub event_id: String,
    pub camera: String,
    pub at_ms: i64,
    /// Where in the picture (JSON [x, y, w, h], 0..1), for objects.
    pub r#box: Option<String>,
    pub label: Option<String>,
    pub emb: Vec<u8>,
}

fn smart_row_from(r: &rusqlite::Row) -> rusqlite::Result<SmartRow> {
    Ok(SmartRow { id: r.get(0)?, event_id: r.get(1)?, camera: r.get(2)?, at_ms: r.get(3)?, r#box: r.get(4)?, label: r.get(5)?, emb: r.get(6)? })
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).map_err(|e| {
            let hint = crate::lockdown::access_hint(path).map(|h| format!(" ({h})")).unwrap_or_default();
            anyhow::Error::new(e).context(format!("open {}{hint}", path.display()))
        })?;
        // It holds two-step secrets: for this account only. (SQLite gives its
        // -wal and -shm files the same rights.)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!("could not make {} private: {e}", path.display());
            }
        }
        let mut db = Self::init(conn)?;
        // The -wal and -shm files exist now; ones left by a crash or an older version may be open.
        crate::lockdown::harden_db(path);
        for _ in 0..READ_CONNECTIONS {
            match Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX) {
                Ok(r) => {
                    r.busy_timeout(Duration::from_secs(30))?;
                    db.ro.push(Mutex::new(r));
                }
                Err(e) => {
                    if db.ro.is_empty() {
                        tracing::warn!("no separate read connection to the database ({e}); big reads will share the main one");
                    }
                    break;
                }
            }
        }
        Ok(db)
    }

    /// Write a consistent copy of the database to `path` (must not exist).
    /// Made on a second connection of its own (the database allows readers
    /// beside the writer), so a large copy does not hold up recording.
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        let file = self.conn.lock().unwrap().path().filter(|p| !p.is_empty()).map(PathBuf::from);
        let run = |conn: &Connection| -> Result<()> {
            conn.execute("VACUUM INTO ?1", params![path.to_string_lossy()])
                .with_context(|| format!("database backup to {}", path.display()))?;
            Ok(())
        };
        match file {
            Some(f) => {
                let reader = Connection::open_with_flags(&f, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)
                    .with_context(|| format!("open {}", f.display()))?;
                reader.busy_timeout(Duration::from_secs(30))?;
                run(&reader)
            }
            None => run(&self.conn.lock().unwrap()),
        }
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Db> {
        let db = Self::init(Connection::open_in_memory()?)?;
        // (No older version has used it.)
        db.sums_checked.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(db)
    }

    fn init(mut conn: Connection) -> Result<Db> {
        conn.busy_timeout(Duration::from_secs(5))?;
        // journal_mode returns a row; in-memory DBs answer "memory".
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            tracing::warn!("the database was last used by a newer AmberSight (version {version}, this one knows {SCHEMA_VERSION}); some features may not work");
        }
        if version < 1 {
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS segments (
                    id INTEGER PRIMARY KEY,
                    camera TEXT NOT NULL,
                    path TEXT NOT NULL UNIQUE,
                    start_ms INTEGER NOT NULL,
                    end_ms INTEGER NOT NULL,
                    bytes INTEGER NOT NULL DEFAULT 0,
                    video_frames INTEGER NOT NULL DEFAULT 0,
                    kind TEXT NOT NULL DEFAULT 'continuous'
                 );
                 CREATE INDEX IF NOT EXISTS idx_segments_camera_start ON segments (camera, start_ms);
                 CREATE INDEX IF NOT EXISTS idx_segments_start ON segments (start_ms);
                 CREATE TABLE IF NOT EXISTS events (
                    id TEXT PRIMARY KEY,
                    camera_id INTEGER NOT NULL,
                    camera TEXT NOT NULL,
                    start INTEGER NOT NULL,
                    end INTEGER NOT NULL,
                    score REAL NOT NULL,
                    clip TEXT,
                    thumb TEXT,
                    objects TEXT NOT NULL DEFAULT '[]',
                    protect INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE INDEX IF NOT EXISTS idx_events_start ON events (start);
                 CREATE INDEX IF NOT EXISTS idx_events_camera_start ON events (camera, start);",
            )?;
            tx.pragma_update(None, "user_version", 1)?;
            tx.commit()?;
        }
        if version < 2 {
            // v2: recordings can live in several folders.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "ALTER TABLE segments ADD COLUMN folder TEXT NOT NULL DEFAULT '';
                 CREATE INDEX IF NOT EXISTS idx_segments_folder_start ON segments (folder, start_ms);",
            )?;
            tx.pragma_update(None, "user_version", 2)?;
            tx.commit()?;
        }
        if version < 3 {
            // v3: audit log of sign-ins and changes.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS audit (
                    id INTEGER PRIMARY KEY,
                    ts INTEGER NOT NULL,
                    user TEXT NOT NULL,
                    ip TEXT NOT NULL,
                    action TEXT NOT NULL,
                    detail TEXT NOT NULL,
                    status INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit (ts);",
            )?;
            tx.pragma_update(None, "user_version", 3)?;
            tx.commit()?;
        }
        if version < 4 {
            // v4: per-user interface state (scope = user name) and shared
            // camera order (scope = "orders").
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS ui_kv (
                    scope TEXT NOT NULL,
                    key TEXT NOT NULL,
                    value TEXT NOT NULL,
                    PRIMARY KEY (scope, key)
                 );",
            )?;
            tx.pragma_update(None, "user_version", 4)?;
            tx.commit()?;
        }
        if version < 5 {
            // v5: sign-in sessions survive a restart (token hashes only).
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                    hash TEXT PRIMARY KEY,
                    user TEXT NOT NULL,
                    admin INTEGER NOT NULL,
                    expires INTEGER NOT NULL
                 );",
            )?;
            tx.pragma_update(None, "user_version", 5)?;
            tx.commit()?;
        }
        if version < 6 {
            // v6: AI review: what each AI check of an event saw.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS ai_reviews (
                    event_id TEXT PRIMARY KEY,
                    camera TEXT NOT NULL,
                    start INTEGER NOT NULL,
                    alerted INTEGER NOT NULL,
                    detail TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_ai_reviews_start ON ai_reviews (start);",
            )?;
            tx.pragma_update(None, "user_version", 6)?;
            tx.commit()?;
        }
        if version < 7 {
            // v7: footage around events is marked, so a full drive loses
            // quiet footage first. Each event keeps the span it covers
            // (camera slug, ms, pre/post roll included); events without one
            // are filled in at start (Engine::mark_event_footage_backlog).
            let tx = conn.transaction()?;
            tx.execute_batch(
                "ALTER TABLE segments ADD COLUMN event INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE events ADD COLUMN span_cam TEXT;
                 ALTER TABLE events ADD COLUMN span_from INTEGER;
                 ALTER TABLE events ADD COLUMN span_to INTEGER;
                 CREATE INDEX IF NOT EXISTS idx_segments_purge ON segments (folder, event, start_ms);
                 CREATE INDEX IF NOT EXISTS idx_events_span ON events (span_cam, span_from);",
            )?;
            tx.pragma_update(None, "user_version", 7)?;
            tx.commit()?;
        }
        if version < 8 {
            // v8: face recognition. A face's fingerprint (128 f32) and small
            // picture; `labeled` faces were named by a person and are what new
            // faces are matched against.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS people (
                     id INTEGER PRIMARY KEY,
                     name TEXT NOT NULL UNIQUE,
                     quiet INTEGER NOT NULL DEFAULT 0,
                     created_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS faces (
                     id INTEGER PRIMARY KEY,
                     event_id TEXT NOT NULL,
                     camera TEXT NOT NULL,
                     at_ms INTEGER NOT NULL,
                     score REAL NOT NULL,
                     size INTEGER NOT NULL,
                     emb BLOB NOT NULL,
                     img BLOB NOT NULL,
                     person_id INTEGER REFERENCES people(id) ON DELETE SET NULL,
                     labeled INTEGER NOT NULL DEFAULT 0,
                     sim REAL
                 );
                 CREATE INDEX IF NOT EXISTS idx_faces_event ON faces (event_id);
                 CREATE INDEX IF NOT EXISTS idx_faces_person ON faces (person_id, labeled);
                 CREATE INDEX IF NOT EXISTS idx_faces_at ON faces (at_ms);",
            )?;
            tx.pragma_update(None, "user_version", 8)?;
            tx.commit()?;
        }
        if version < 9 {
            // v9: how sharp each face picture is (see faces::is_clear): only
            // clear named faces are used to recognize people.
            let tx = conn.transaction()?;
            let has: bool = tx.prepare("SELECT 1 FROM pragma_table_info('faces') WHERE name = 'detail'")?.exists([])?;
            if !has {
                tx.execute_batch("ALTER TABLE faces ADD COLUMN detail REAL;")?;
            }
            let pics: Vec<(i64, Vec<u8>)> = {
                let mut st = tx.prepare("SELECT id, img FROM faces WHERE detail IS NULL")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for (id, img) in pics {
                if let Some((d, _)) = crate::faces::sharpness_parts(&img) {
                    tx.execute("UPDATE faces SET detail = ?2 WHERE id = ?1", params![id, d as f64])?;
                }
            }
            tx.pragma_update(None, "user_version", 9)?;
            tx.commit()?;
        }
        if version < 10 {
            // v10: signals (a door, an alarm, a camera's input): each change
            // of a named on/off signal, shown as lanes on the timeline.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS signal_changes (name TEXT NOT NULL, at_ms INTEGER NOT NULL, state INTEGER NOT NULL);
                 CREATE INDEX IF NOT EXISTS idx_signal_name_at ON signal_changes (name, at_ms);",
            )?;
            tx.pragma_update(None, "user_version", 10)?;
            tx.commit()?;
        }
        if version < 11 {
            // v11: smart search. A fingerprint (CLIP) per event picture and
            // per object AI found in it; `smart_done` marks events looked at.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS smart_vecs (
                     id INTEGER PRIMARY KEY,
                     event_id TEXT NOT NULL,
                     camera TEXT NOT NULL,
                     at_ms INTEGER NOT NULL,
                     src TEXT NOT NULL,
                     box TEXT,
                     label TEXT,
                     emb BLOB NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_smart_at ON smart_vecs (at_ms);
                 CREATE INDEX IF NOT EXISTS idx_smart_event ON smart_vecs (event_id);
                 CREATE TABLE IF NOT EXISTS smart_done (event_id TEXT PRIMARY KEY);",
            )?;
            tx.pragma_update(None, "user_version", 11)?;
            tx.commit()?;
        }
        if version < 12 {
            // v12: browser notifications (Web Push). Each browser that asked
            // for notifications, with the signed-in user it belongs to, and
            // this server's own signing key (made once).
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS push_subs (
                     id INTEGER PRIMARY KEY,
                     endpoint TEXT NOT NULL UNIQUE,
                     user TEXT NOT NULL,
                     p256dh TEXT NOT NULL,
                     auth TEXT NOT NULL,
                     device TEXT NOT NULL,
                     created_ms INTEGER NOT NULL,
                     last_ok_ms INTEGER
                 );
                 CREATE INDEX IF NOT EXISTS idx_push_user ON push_subs (user);
                 CREATE TABLE IF NOT EXISTS push_key (
                     id INTEGER PRIMARY KEY CHECK (id = 1),
                     private_key TEXT NOT NULL
                 );",
            )?;
            tx.pragma_update(None, "user_version", 12)?;
            tx.commit()?;
        }
        if version < 13 {
            // v13: events AI checked without finding anything that counts
            // (a flag in the wind), so a camera can leave them out of the
            // event list and timeline. Earlier events are marked from their
            // AI review: AI ran (not failed), confirmed nothing, no sound and
            // no label from the camera itself.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "ALTER TABLE events ADD COLUMN unconfirmed INTEGER NOT NULL DEFAULT 0;
                 UPDATE events SET unconfirmed = 1 WHERE id IN (
                   SELECT event_id FROM ai_reviews WHERE json_valid(detail)
                     AND json_extract(detail, '$.confirmed') = 0
                     AND COALESCE(json_extract(detail, '$.sound'), 0) = 0
                     AND COALESCE(json_array_length(detail, '$.camera_labels'), 0) = 0
                     AND COALESCE(json_extract(detail, '$.alert.why'), '') <> 'ai_failed'
                     AND COALESCE(json_extract(detail, '$.source'), '') NOT IN ('sound', 'camera'));
                 CREATE INDEX IF NOT EXISTS idx_events_unconfirmed ON events (unconfirmed) WHERE unconfirmed = 1;",
            )?;
            tx.pragma_update(None, "user_version", 13)?;
            tx.commit()?;
        }
        if version < 14 {
            // v14: a reviewer's verdict on an event ('false_alarm' or 'ok',
            // NULL = not reviewed) and when it was given; indexes for the
            // protected-only and false-alarm filters.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "ALTER TABLE events ADD COLUMN review TEXT;
                 ALTER TABLE events ADD COLUMN reviewed_at INTEGER;
                 CREATE INDEX IF NOT EXISTS idx_events_review ON events (review) WHERE review IS NOT NULL;
                 CREATE INDEX IF NOT EXISTS idx_events_protect ON events (protect) WHERE protect = 1;",
            )?;
            tx.pragma_update(None, "user_version", 14)?;
            tx.commit()?;
        }
        if version < 15 {
            // v15: phones reporting whether they are at home (presence /
            // geofencing). One row per (account, device); the app picks the
            // device id.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS presence_devices (
                     user TEXT NOT NULL,
                     device_id TEXT NOT NULL,
                     name TEXT NOT NULL,
                     home INTEGER NOT NULL,
                     updated_ms INTEGER NOT NULL,
                     created_ms INTEGER NOT NULL,
                     PRIMARY KEY (user, device_id)
                 );",
            )?;
            tx.pragma_update(None, "user_version", 15)?;
            tx.commit()?;
        }
        if version < 16 {
            // v16: the presence rule's memory (one row), so a restart keeps
            // the last known state and a running away countdown.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS presence_rule (
                     id INTEGER PRIMARY KEY CHECK (id = 1),
                     known_home INTEGER,
                     left_at_ms INTEGER,
                     acted_away INTEGER NOT NULL
                 );",
            )?;
            tx.pragma_update(None, "user_version", 16)?;
            tx.commit()?;
        }
        if version < 17 {
            // v17: notifications to phone apps (UnifiedPush) next to
            // browsers: which kind each subscription is.
            let tx = conn.transaction()?;
            let has: bool = tx.query_row("SELECT COUNT(*) FROM pragma_table_info('push_subs') WHERE name = 'kind'", [], |r| r.get::<_, i64>(0))? > 0;
            if !has {
                tx.execute_batch("ALTER TABLE push_subs ADD COLUMN kind TEXT NOT NULL DEFAULT 'browser';")?;
            }
            tx.pragma_update(None, "user_version", 17)?;
            tx.commit()?;
        }
        if version < 18 {
            // v18: license plates. Each reading of a plate in an event (text,
            // sureness, a small picture, the known plate it matched) and the
            // library of plates people have named.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS known_plates (
                     id INTEGER PRIMARY KEY,
                     text TEXT NOT NULL UNIQUE,
                     name TEXT NOT NULL,
                     notes TEXT NOT NULL DEFAULT '',
                     created_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS plates (
                     id INTEGER PRIMARY KEY,
                     event_id TEXT NOT NULL,
                     camera TEXT NOT NULL,
                     at_ms INTEGER NOT NULL,
                     text TEXT NOT NULL,
                     confidence REAL NOT NULL,
                     width INTEGER NOT NULL,
                     img BLOB NOT NULL,
                     known_id INTEGER REFERENCES known_plates(id) ON DELETE SET NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_plates_event ON plates (event_id);
                 CREATE INDEX IF NOT EXISTS idx_plates_text ON plates (text);
                 CREATE INDEX IF NOT EXISTS idx_plates_at ON plates (at_ms);",
            )?;
            tx.pragma_update(None, "user_version", 18)?;
            tx.commit()?;
        }
        if version < 19 {
            // v19: camera tamper alerts (a log with their pictures' files)
            // and the activity history behind unusual-activity flagging:
            // per camera, label and local day, the hours with an event.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS tamper_log (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    camera TEXT NOT NULL,
                    at INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    light TEXT NOT NULL DEFAULT 'day',
                    cleared_at INTEGER
                 );
                 CREATE INDEX IF NOT EXISTS idx_tamper_log_camera ON tamper_log (camera, at);
                 CREATE TABLE IF NOT EXISTS activity_days (
                    camera TEXT NOT NULL,
                    day INTEGER NOT NULL,
                    label TEXT NOT NULL,
                    hours INTEGER NOT NULL,
                    PRIMARY KEY (camera, day, label)
                 ) WITHOUT ROWID;
                 CREATE TABLE IF NOT EXISTS activity_meta (
                    camera TEXT PRIMARY KEY,
                    ingested_until INTEGER NOT NULL,
                    first_day INTEGER NOT NULL
                 );",
            )?;
            tx.pragma_update(None, "user_version", 19)?;
            tx.commit()?;
        }
        if version < 20 {
            // v20: the morning summary: the summaries sent (kept for their
            // links) and the stretches a camera was offline.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS digests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_ms INTEGER NOT NULL,
                    from_ms INTEGER NOT NULL,
                    to_ms INTEGER NOT NULL,
                    auto INTEGER NOT NULL DEFAULT 0,
                    body TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS camera_outages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    camera TEXT NOT NULL,
                    start_ms INTEGER NOT NULL,
                    end_ms INTEGER NOT NULL,
                    open INTEGER NOT NULL DEFAULT 1
                 );
                 CREATE INDEX IF NOT EXISTS idx_outages_camera ON camera_outages (camera, start_ms);",
            )?;
            tx.pragma_update(None, "user_version", 20)?;
            tx.commit()?;
        }
        if version < 21 {
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS two_step (
                    user TEXT PRIMARY KEY,
                    secret TEXT NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 0,
                    last_step INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS recovery_codes (
                    id INTEGER PRIMARY KEY,
                    user TEXT NOT NULL,
                    hash TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_recovery_user ON recovery_codes (user);
                 CREATE TABLE IF NOT EXISTS device_tokens (
                    id TEXT PRIMARY KEY,
                    user TEXT NOT NULL,
                    name TEXT NOT NULL,
                    admin INTEGER NOT NULL,
                    hash TEXT NOT NULL UNIQUE,
                    created_at INTEGER NOT NULL,
                    last_used INTEGER
                 );
                 CREATE TABLE IF NOT EXISTS account_requests (
                    id INTEGER PRIMARY KEY,
                    user TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    hash TEXT NOT NULL,
                    revoke_tokens INTEGER NOT NULL DEFAULT 0
                 );",
            )?;
            tx.pragma_update(None, "user_version", 21)?;
            tx.commit()?;
        }
        if version < 22 {
            // A change from the command line that is in the settings already (see
            // account_http.rs): it is never applied again, also after a crash.
            let tx = conn.transaction()?;
            let has: i64 = tx.query_row("SELECT COUNT(*) FROM pragma_table_info('account_requests') WHERE name = 'claimed'", [], |r| r.get(0))?;
            if has == 0 {
                tx.execute_batch("ALTER TABLE account_requests ADD COLUMN claimed INTEGER NOT NULL DEFAULT 0;")?;
            }
            tx.pragma_update(None, "user_version", 22)?;
            tx.commit()?;
        }
        if version < 23 {
            // v23: the list of signed-in devices (what each session was made
            // from and when it was last used; sessions from before have empty
            // columns) and the browsers each account has signed in from. An
            // older build keeps working on this database: it names only the
            // first four columns of sessions, and ignores the new table.
            let tx = conn.transaction()?;
            for (col, def) in [("created", "INTEGER NOT NULL DEFAULT 0"), ("ip", "TEXT NOT NULL DEFAULT ''"), ("label", "TEXT NOT NULL DEFAULT ''"), ("device", "TEXT NOT NULL DEFAULT ''"), ("last_seen", "INTEGER NOT NULL DEFAULT 0")] {
                let has: i64 = tx.query_row("SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = ?1", params![col], |r| r.get(0))?;
                if has == 0 {
                    tx.execute_batch(&format!("ALTER TABLE sessions ADD COLUMN {col} {def};"))?;
                }
            }
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS known_devices (
                    user TEXT NOT NULL,
                    hash TEXT NOT NULL,
                    label TEXT NOT NULL,
                    first_seen INTEGER NOT NULL,
                    last_seen INTEGER NOT NULL,
                    PRIMARY KEY (user, hash)
                 );",
            )?;
            tx.pragma_update(None, "user_version", 23)?;
            tx.commit()?;
        }
        if version < 24 {
            // v24: which sign-in a notification address was turned on from, so that signing
            // that device out can take it off the list too (empty for older ones).
            let tx = conn.transaction()?;
            let has: i64 = tx.query_row("SELECT COUNT(*) FROM pragma_table_info('push_subs') WHERE name = 'session_device'", [], |r| r.get(0))?;
            if has == 0 {
                tx.execute_batch("ALTER TABLE push_subs ADD COLUMN session_device TEXT;")?;
            }
            tx.pragma_update(None, "user_version", 24)?;
            tx.commit()?;
        }
        if version < 25 {
            // v25: the one-line verdict of each AI review, kept apart from the big review text so
            // the AI review page reads a few hundred bytes a row instead of every review in full.
            // An older site fills it in the background (see backfill_review_sums); a new
            // database has nothing to fill.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS ai_review_sums (
                    event_id TEXT PRIMARY KEY,
                    camera TEXT NOT NULL,
                    start INTEGER NOT NULL,
                    alerted INTEGER NOT NULL,
                    summary TEXT NOT NULL
                 ) WITHOUT ROWID;
                 CREATE INDEX IF NOT EXISTS idx_ai_review_sums_cam ON ai_review_sums (camera, start, alerted);
                 INSERT OR IGNORE INTO ui_kv (scope, key, value) SELECT 'system', 'review_sums_done', '1' WHERE NOT EXISTS (SELECT 1 FROM ai_reviews);",
            )?;
            tx.pragma_update(None, "user_version", 25)?;
            tx.commit()?;
        }
        if version < 26 {
            // v26: running totals of the recordings (bytes and files per folder and camera,
            // bytes and recorded time per camera and day), kept by triggers on `segments`, so
            // the Status and Storage pages and retention read a few rows instead of the whole
            // index. A site with recordings fills them in the background (see
            // fill_segment_totals); until then the old queries are used.
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS seg_totals (
                    folder TEXT NOT NULL,
                    camera TEXT NOT NULL,
                    bytes INTEGER NOT NULL,
                    n INTEGER NOT NULL,
                    PRIMARY KEY (folder, camera)
                 ) WITHOUT ROWID;
                 CREATE TABLE IF NOT EXISTS seg_days (
                    camera TEXT NOT NULL,
                    day INTEGER NOT NULL,
                    bytes INTEGER NOT NULL,
                    ms INTEGER NOT NULL,
                    n INTEGER NOT NULL,
                    PRIMARY KEY (camera, day)
                 ) WITHOUT ROWID;
                 CREATE TABLE IF NOT EXISTS seg_totals_fill (id INTEGER PRIMARY KEY CHECK (id = 1), upto INTEGER NOT NULL);",
            )?;
            tx.pragma_update(None, "user_version", 26)?;
            tx.commit()?;
        }
        // The triggers are part of the database, so an older version run in between keeps the
        // totals too. Should they be missing (made by hand, a tool), they are made again and the
        // totals counted afresh.
        let triggers: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name IN ('seg_totals_ins', 'seg_totals_del', 'seg_totals_upd')
               AND sql LIKE '%seg_totals_fill%'",
            [],
            |r| r.get(0),
        )?;
        let filled: Option<i64> = conn.query_row("SELECT upto FROM seg_totals_fill WHERE id = 1", [], |r| r.get(0)).optional()?;
        if triggers != 3 || filled.is_none() {
            let tx = conn.transaction()?;
            tx.execute_batch(SEG_TOTALS_TRIGGERS)?;
            tx.execute_batch("DELETE FROM seg_totals; DELETE FROM seg_days; INSERT OR REPLACE INTO seg_totals_fill (id, upto) VALUES (1, 0);")?;
            tx.commit()?;
        }
        let longest: i64 = conn.query_row("SELECT COALESCE(MAX(end_ms - start_ms), 0) FROM segments", [], |r| r.get(0))?;
        let db = Db { conn: Mutex::new(conn), ro: Vec::new(), ro_next: std::sync::atomic::AtomicUsize::new(0), longest_segment: std::sync::atomic::AtomicI64::new(longest), importing: std::sync::atomic::AtomicBool::new(false), import_started: Mutex::new(None), import_warned: std::sync::atomic::AtomicBool::new(false), sums_checked: std::sync::atomic::AtomicBool::new(false), totals_ready: std::sync::atomic::AtomicBool::new(false), smart_hint: Mutex::new(None), smart_counts_cache: Mutex::new(None) };
        // A small index is counted now; a big one in the background (see fill_segment_totals).
        let max_id: i64 = db.lock().query_row("SELECT COALESCE(MAX(id), 0) FROM segments", [], |r| r.get(0))?;
        if max_id <= SEG_TOTALS_FILL_NOW {
            db.fill_segment_totals_paced(Duration::ZERO)?;
        } else {
            db.segment_totals_ready();
        }
        Ok(db)
    }

    // ---- presence (phones at home or away) ----

    /// Record a phone's report. A new device is refused (Ok(Err)) when the
    /// account already has `max_user` devices or the server `max_total`.
    pub fn presence_report(&self, d: &PresenceDevice, max_user: usize, max_total: usize) -> Result<std::result::Result<(), String>> {
        let conn = self.lock();
        let known: bool = conn
            .query_row("SELECT 1 FROM presence_devices WHERE user = ?1 AND device_id = ?2", params![d.user, d.device_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !known {
            let mine: i64 = conn.query_row("SELECT COUNT(*) FROM presence_devices WHERE user = ?1", params![d.user], |r| r.get(0))?;
            if mine as usize >= max_user {
                return Ok(Err(format!("this account already has {max_user} phones reporting presence; remove one first")));
            }
            let all: i64 = conn.query_row("SELECT COUNT(*) FROM presence_devices", [], |r| r.get(0))?;
            if all as usize >= max_total {
                return Ok(Err(format!("this server already has {max_total} phones reporting presence; remove one first")));
            }
        }
        conn.execute(
            "INSERT INTO presence_devices (user, device_id, name, home, updated_ms, created_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(user, device_id) DO UPDATE SET name = excluded.name, home = excluded.home, updated_ms = excluded.updated_ms",
            params![d.user, d.device_id, d.name, d.home, d.updated_ms],
        )?;
        Ok(Ok(()))
    }

    /// Devices, all or one account's, by account then name.
    pub fn presence_list(&self, user: Option<&str>) -> Result<Vec<PresenceDevice>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT user, device_id, name, home, updated_ms, created_ms FROM presence_devices
             WHERE ?1 IS NULL OR user = ?1 ORDER BY user, name COLLATE NOCASE, device_id",
        )?;
        let rows = st.query_map(params![user], |r| {
            Ok(PresenceDevice { user: r.get(0)?, device_id: r.get(1)?, name: r.get(2)?, home: r.get(3)?, updated_ms: r.get(4)?, created_ms: r.get(5)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Remove one device; false when there was none.
    pub fn presence_delete(&self, user: &str, device_id: &str) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM presence_devices WHERE user = ?1 AND device_id = ?2", params![user, device_id])? > 0)
    }

    /// The presence rule's stored memory, if any.
    pub fn presence_rule_get(&self) -> Result<Option<PresenceRuleRow>> {
        Ok(self
            .lock()
            .query_row("SELECT known_home, left_at_ms, acted_away FROM presence_rule WHERE id = 1", [], |r| {
                Ok(PresenceRuleRow { known_home: r.get(0)?, left_at_ms: r.get(1)?, acted_away: r.get(2)? })
            })
            .optional()?)
    }

    /// Store the presence rule's memory (None: forget it).
    pub fn presence_rule_put(&self, row: Option<&PresenceRuleRow>) -> Result<()> {
        let conn = self.lock();
        match row {
            Some(r) => conn.execute(
                "INSERT INTO presence_rule (id, known_home, left_at_ms, acted_away) VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET known_home = excluded.known_home, left_at_ms = excluded.left_at_ms, acted_away = excluded.acted_away",
                params![r.known_home, r.left_at_ms, r.acted_away],
            )?,
            None => conn.execute("DELETE FROM presence_rule", [])?,
        };
        Ok(())
    }

    /// Remove devices that last reported before `before_ms`; returns how many.
    pub fn presence_purge(&self, before_ms: i64) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM presence_devices WHERE updated_ms < ?1", params![before_ms])?)
    }

    /// Remove every device of an account (the account was deleted).
    pub fn presence_delete_user(&self, user: &str) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM presence_devices WHERE user = ?1", params![user])?)
    }

    // ---- browser notifications (Web Push) ----

    /// Store the push signing key unless one is already there; returns the
    /// one that is stored (so two first callers agree).
    pub fn push_key_init(&self, key: &str) -> Result<String> {
        let conn = self.lock();
        conn.execute("INSERT OR IGNORE INTO push_key (id, private_key) VALUES (1, ?1)", params![key])?;
        Ok(conn.query_row("SELECT private_key FROM push_key WHERE id = 1", [], |r| r.get(0))?)
    }

    /// Add or update a browser's subscription (a browser belongs to the
    /// user who last turned notifications on in it).
    pub fn push_save(&self, s: &PushSub, now_ms: i64) -> Result<()> {
        self.lock().execute(
            "INSERT INTO push_subs (endpoint, user, p256dh, auth, device, created_ms, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(endpoint) DO UPDATE SET user = excluded.user, p256dh = excluded.p256dh,
               auth = excluded.auth, device = excluded.device, kind = excluded.kind",
            params![s.endpoint, s.user, s.p256dh, s.auth, s.device, now_ms, if s.kind.is_empty() { "browser" } else { s.kind.as_str() }],
        )?;
        Ok(())
    }

    /// The account a push address is registered to, if any.
    pub fn push_owner(&self, endpoint: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT user FROM push_subs WHERE endpoint = ?1")?;
        let mut rows = st.query(params![endpoint])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    /// Subscriptions, all or one user's, oldest first.
    pub fn push_list(&self, user: Option<&str>) -> Result<Vec<PushSub>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, endpoint, user, p256dh, auth, device, created_ms, last_ok_ms, kind FROM push_subs
             WHERE ?1 IS NULL OR user = ?1 ORDER BY id",
        )?;
        let rows = st.query_map(params![user], |r| {
            Ok(PushSub {
                id: r.get(0)?,
                endpoint: r.get(1)?,
                user: r.get(2)?,
                p256dh: r.get(3)?,
                auth: r.get(4)?,
                device: r.get(5)?,
                created_ms: r.get(6)?,
                last_ok_ms: r.get(7)?,
                kind: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The account's notification addresses that were turned on from these sign-ins.
    pub fn push_by_session_devices(&self, user: &str, devices: &[String]) -> Result<Vec<PushSub>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, endpoint, user, p256dh, auth, device, created_ms, last_ok_ms, kind FROM push_subs
             WHERE user = ?1 AND session_device = ?2 ORDER BY id",
        )?;
        let mut all = Vec::new();
        for d in devices.iter().filter(|d| !d.is_empty()) {
            let rows = st.query_map(params![user, d], |r| {
                Ok(PushSub { id: r.get(0)?, endpoint: r.get(1)?, user: r.get(2)?, p256dh: r.get(3)?, auth: r.get(4)?, device: r.get(5)?, created_ms: r.get(6)?, last_ok_ms: r.get(7)?, kind: r.get(8)? })
            })?;
            for r in rows {
                all.push(r?);
            }
        }
        Ok(all)
    }

    /// For tests: which sign-in an address was turned on from.
    #[cfg(test)]
    pub fn push_session_device(&self, endpoint: &str) -> Option<String> {
        self.lock().query_row("SELECT session_device FROM push_subs WHERE endpoint = ?1", params![endpoint], |r| r.get(0)).ok().flatten()
    }

    /// Forget a subscription by its endpoint or id; `user` limits it to
    /// that user's own. Returns whether one was removed.
    pub fn push_delete(&self, endpoint: Option<&str>, id: Option<i64>, user: Option<&str>) -> Result<bool> {
        let n = self.lock().execute(
            "DELETE FROM push_subs WHERE (endpoint = ?1 OR id = ?2) AND (?3 IS NULL OR user = ?3)",
            params![endpoint, id, user],
        )?;
        Ok(n > 0)
    }

    /// Forget every subscription of a user (the account was deleted).
    pub fn push_delete_user(&self, user: &str) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM push_subs WHERE user = ?1", params![user])?)
    }

    /// Note that a message reached this subscription's push service.
    pub fn push_delivered(&self, id: i64, now_ms: i64) -> Result<()> {
        self.lock().execute("UPDATE push_subs SET last_ok_ms = ?2 WHERE id = ?1", params![id, now_ms])?;
        Ok(())
    }

    // ---- smart search ----

    #[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
    /// Events not yet looked at by smart search, newest first:
    /// (id, camera, start s).
    ///
    /// Once a check of every event finds none waiting, the next ones look
    /// only at events that started since then (with an hour's slack), which
    /// takes a few rows instead of all of them; every event is checked again
    /// an hour later (an event can be added with an older start, e.g. by an
    /// import).
    pub fn smart_pending(&self, limit: usize) -> Result<Vec<(String, String, i64)>> {
        const FULL_EVERY: Duration = Duration::from_secs(3600);
        let hint = *self.smart_hint.lock().unwrap_or_else(|e| e.into_inner());
        let from = match hint {
            Some((from, at)) if at.elapsed() < FULL_EVERY => from,
            _ => i64::MIN,
        };
        let conn = self.lock_ro();
        let mut st = conn.prepare_cached(
            "SELECT e.id, e.camera, e.start FROM events e WHERE e.start >= ?2 AND NOT EXISTS (SELECT 1 FROM smart_done d WHERE d.event_id = e.id)
             ORDER BY e.start DESC LIMIT ?1",
        )?;
        let rows: Vec<(String, String, i64)> = st.query_map(params![limit as i64, from], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() && from == i64::MIN {
            let newest: Option<i64> = conn.query_row("SELECT MAX(start) FROM events", [], |r| r.get(0))?;
            // (Never above now: one event dated ahead of the clock must not hide the next ones.)
            let now = chrono::Utc::now().timestamp();
            *self.smart_hint.lock().unwrap_or_else(|e| e.into_inner()) = Some((newest.unwrap_or(0).min(now).saturating_sub(3600), std::time::Instant::now()));
        }
        Ok(rows)
    }

    /// Forget what `smart_pending` knows (every event is checked next time).
    fn smart_forget_hint(&self) {
        *self.smart_hint.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.smart_counts_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// (events looked at, events waiting, fingerprints kept). Counted at most
    /// once a minute (it reads every event).
    pub fn smart_counts(&self) -> Result<(i64, i64, i64)> {
        if let Some((at, c)) = *self.smart_counts_cache.lock().unwrap_or_else(|e| e.into_inner()) {
            if at.elapsed() < Duration::from_secs(60) {
                return Ok(c);
            }
        }
        let conn = self.lock_ro();
        let done: i64 = conn.query_row("SELECT COUNT(*) FROM smart_done d WHERE EXISTS (SELECT 1 FROM events e WHERE e.id = d.event_id)", [], |r| r.get(0))?;
        let all: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        let vecs: i64 = conn.query_row("SELECT COUNT(*) FROM smart_vecs", [], |r| r.get(0))?;
        let c = (done, (all - done).max(0), vecs);
        *self.smart_counts_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), c));
        Ok(c)
    }

    #[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
    /// Store one event's fingerprints and mark it looked at:
    /// (camera, at_ms, src, box json, label, fingerprint).
    #[allow(clippy::type_complexity)]
    pub fn smart_store(&self, event_id: &str, rows: &[(String, i64, String, Option<String>, Option<String>, Vec<u8>)]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            tx.execute("DELETE FROM smart_vecs WHERE event_id = ?1", params![event_id])?;
            let mut st = tx.prepare("INSERT INTO smart_vecs (event_id, camera, at_ms, src, box, label, emb) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
            for (cam, at, src, bx, label, emb) in rows {
                st.execute(params![event_id, cam, at, src, bx, label, emb])?;
            }
            tx.execute("INSERT OR IGNORE INTO smart_done (event_id) VALUES (?1)", params![event_id])?;
        }
        tx.commit()?;
        Ok(())
    }

    #[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
    /// Fingerprints recorded in [from_ms, to_ms]: (id, event, camera, at_ms, box, label, fingerprint).
    #[allow(clippy::type_complexity)]
    /// Read in pieces, the database free for recording between them.
    pub fn smart_scan(&self, from_ms: i64, to_ms: i64, f: &mut dyn FnMut(i64, &str, &str, i64, Option<&str>, Option<&str>, &[u8])) -> Result<()> {
        type Row = (i64, String, String, i64, Option<String>, Option<String>, Vec<u8>);
        let mut after = i64::MIN;
        loop {
            let rows: Vec<Row> = {
                let conn = self.lock();
                let mut st = conn.prepare_cached(
                    "SELECT id, event_id, camera, at_ms, box, label, emb FROM smart_vecs WHERE id > ?1 AND at_ms >= ?2 AND at_ms <= ?3 ORDER BY id LIMIT 2000",
                )?;
                let it = st.query_map(params![after, from_ms, to_ms], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
                it.collect::<rusqlite::Result<_>>()?
            };
            let Some(last) = rows.last().map(|r| r.0) else { return Ok(()) };
            for (id, ev, cam, at, bx, label, emb) in &rows {
                f(*id, ev, cam, *at, bx.as_deref(), label.as_deref(), emb);
            }
            after = last;
        }
    }

    /// An event's fingerprints (pictures and objects), oldest first.
    pub fn smart_event_rows(&self, event_id: &str) -> Result<Vec<SmartRow>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, event_id, camera, at_ms, box, label, emb FROM smart_vecs WHERE event_id = ?1 ORDER BY at_ms, id")?;
        let rows = st.query_map(params![event_id], smart_row_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Named or matched people seen in events between two times: (event, person).
    pub fn people_in_events_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<(String, i64)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT DISTINCT event_id, person_id FROM faces WHERE person_id IS NOT NULL AND at_ms >= ?1 AND at_ms <= ?2")?;
        let rows = st.query_map(params![from_ms, to_ms], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Plate texts read in events between two times: (event, text).
    pub fn plates_in_events_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT DISTINCT event_id, text FROM plates WHERE at_ms >= ?1 AND at_ms <= ?2")?;
        let rows = st.query_map(params![from_ms, to_ms], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Where a fingerprint's picture is (relative to the data folder).
    /// (picture path, camera).
    pub fn smart_src(&self, id: i64) -> Result<Option<(String, String)>> {
        Ok(self.lock().query_row("SELECT src, camera FROM smart_vecs WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    #[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
    /// Forget every fingerprint (they are made again).
    pub fn smart_reset(&self) -> Result<()> {
        self.lock().execute_batch("DELETE FROM smart_vecs; DELETE FROM smart_done;")?;
        self.smart_forget_hint();
        Ok(())
    }

    #[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
    /// Drop fingerprints of events that no longer exist.
    pub fn smart_prune(&self) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute("DELETE FROM smart_vecs WHERE NOT EXISTS (SELECT 1 FROM events e WHERE e.id = smart_vecs.event_id)", [])?;
        conn.execute("DELETE FROM smart_done WHERE NOT EXISTS (SELECT 1 FROM events e WHERE e.id = smart_done.event_id)", [])?;
        Ok(n)
    }

    // ---- signals ----

    /// Record that signal `name` is `on` (or off) from `at_ms`; false when it
    /// already was (nothing changed).
    pub fn set_signal(&self, name: &str, at_ms: i64, on: bool) -> Result<bool> {
        let conn = self.lock();
        let last: Option<bool> = conn
            .query_row("SELECT state FROM signal_changes WHERE name = ?1 ORDER BY at_ms DESC, rowid DESC LIMIT 1", params![name], |r| r.get(0))
            .optional()?;
        if last == Some(on) {
            return Ok(false);
        }
        // A new name: at most MAX_SIGNALS different signals (a home has a
        // handful; this keeps a misbehaving script from filling the disk).
        if last.is_none() {
            let n: i64 = conn.query_row("SELECT COUNT(DISTINCT name) FROM signal_changes", [], |r| r.get(0))?;
            if n >= MAX_SIGNALS {
                anyhow::bail!("too many signals ({MAX_SIGNALS}); remove some in the timeline's signal lanes first");
            }
        }
        conn.execute("INSERT INTO signal_changes (name, at_ms, state) VALUES (?1, ?2, ?3)", params![name, at_ms, on])?;
        Ok(true)
    }

    /// Every signal with when it was on within [from_ms, to_ms] (a span still
    /// on ends at `now_ms`), and whether it is on now: (name, on now, spans).
    pub fn signal_spans(&self, from_ms: i64, to_ms: i64, now_ms: i64) -> Result<Vec<(String, bool, Vec<(i64, i64)>)>> {
        let conn = self.lock();
        let names: Vec<String> = {
            let mut st = conn.prepare("SELECT DISTINCT name FROM signal_changes ORDER BY name")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for name in names {
            let before: Option<bool> = conn
                .query_row("SELECT state FROM signal_changes WHERE name = ?1 AND at_ms <= ?2 ORDER BY at_ms DESC, rowid DESC LIMIT 1", params![name, from_ms], |r| r.get(0))
                .optional()?;
            let changes: Vec<(i64, bool)> = {
                let mut st = conn.prepare("SELECT at_ms, state FROM signal_changes WHERE name = ?1 AND at_ms > ?2 AND at_ms <= ?3 ORDER BY at_ms, rowid")?;
                let rows = st.query_map(params![name, from_ms, to_ms], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let now_on: bool = conn
                .query_row("SELECT state FROM signal_changes WHERE name = ?1 ORDER BY at_ms DESC, rowid DESC LIMIT 1", params![name], |r| r.get(0))
                .optional()?
                .unwrap_or(false);
            out.push((name, now_on, signal_on_spans(before.unwrap_or(false), &changes, from_ms, to_ms.min(now_ms))));
        }
        Ok(out)
    }

    /// Changes older than `before_ms`, keeping each signal's latest (its state now).
    pub fn prune_signals(&self, before_ms: i64) -> Result<usize> {
        Ok(self.lock().execute(
            "DELETE FROM signal_changes AS s WHERE at_ms < ?1 AND at_ms < (SELECT MAX(at_ms) FROM signal_changes WHERE name = s.name)",
            params![before_ms],
        )?)
    }

    /// Remove a signal and its history.
    pub fn forget_signal(&self, name: &str) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM signal_changes WHERE name = ?1", params![name])? > 0)
    }

    // ---- faces ----

    /// Save a face; returns its id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_face(&self, event_id: &str, camera: &str, at_ms: i64, score: f32, size: u32, emb: &[u8], img: &[u8], person: Option<(i64, f32)>) -> Result<i64> {
        let detail = crate::faces::sharpness_parts(img).map(|d| d.0 as f64);
        let conn = self.lock();
        conn.execute(
            "INSERT INTO faces (event_id, camera, at_ms, score, size, emb, img, person_id, sim, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![event_id, camera, at_ms, score as f64, size, emb, img, person.map(|p| p.0), person.map(|p| p.1 as f64), detail],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Fingerprints to recognize people by: (person id, fingerprint bytes).
    /// Each person's clear named faces; someone with none uses their best.
    pub fn labeled_faces(&self) -> Result<Vec<(i64, Vec<u8>)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT person_id, emb, size, score, detail FROM faces WHERE labeled = 1 AND person_id IS NOT NULL")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, i64>(2)?, r.get::<_, f64>(3)?, r.get::<_, Option<f64>>(4)?)))?;
        let rows: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        let mut by_person: std::collections::BTreeMap<i64, Vec<(Vec<u8>, bool, f64)>> = Default::default();
        for (pid, emb, size, score, detail) in rows {
            let clear = crate::faces::is_clear(size as u32, score as f32, detail.map(|d| d as f32));
            let quality = size as f64 * score * detail.unwrap_or(10.0);
            by_person.entry(pid).or_default().push((emb, clear, quality));
        }
        let mut out = Vec::new();
        for (pid, mut faces) in by_person {
            if faces.iter().any(|f| f.1) {
                out.extend(faces.into_iter().filter(|f| f.1).map(|f| (pid, f.0)));
            } else {
                faces.sort_by(|a, b| b.2.total_cmp(&a.2));
                out.extend(faces.into_iter().take(1).map(|f| (pid, f.0)));
            }
        }
        Ok(out)
    }

    /// Faces, newest first: all, one event's, one person's, or only those
    /// nobody has named (`unnamed`). Rows as JSON for the API.
    pub fn list_faces(&self, event_id: Option<&str>, person: Option<i64>, unnamed: bool, limit: usize) -> Result<Vec<serde_json::Value>> {
        let conn = self.lock();
        let mut sql = String::from(
            "SELECT f.id, f.event_id, f.camera, f.at_ms, f.score, f.size, f.person_id, p.name, f.labeled, f.sim, f.detail
             FROM faces f LEFT JOIN people p ON p.id = f.person_id WHERE 1 = 1",
        );
        if event_id.is_some() {
            sql.push_str(" AND f.event_id = ?1");
        }
        if person.is_some() {
            sql.push_str(" AND f.person_id = ?2");
        }
        if unnamed {
            sql.push_str(" AND f.labeled = 0");
        }
        sql.push_str(" ORDER BY f.at_ms DESC, f.id DESC LIMIT ?3");
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map(params![event_id.unwrap_or(""), person.unwrap_or(0), limit as i64], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "event_id": r.get::<_, String>(1)?,
                "camera": r.get::<_, String>(2)?,
                "at_ms": r.get::<_, i64>(3)?,
                "score": r.get::<_, f64>(4)?,
                "size": r.get::<_, i64>(5)?,
                "person_id": r.get::<_, Option<i64>>(6)?,
                "person": r.get::<_, Option<String>>(7)?,
                "labeled": r.get::<_, i64>(8)? == 1,
                "sim": r.get::<_, Option<f64>>(9)?,
                "clear": crate::faces::is_clear(r.get::<_, i64>(5)? as u32, r.get::<_, f64>(4)? as f32, r.get::<_, Option<f64>>(10)?.map(|d| d as f32)),
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn face_img(&self, id: i64) -> Result<Option<(String, Vec<u8>)>> {
        let conn = self.lock();
        Ok(conn.query_row("SELECT camera, img FROM faces WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    pub fn face_emb(&self, id: i64) -> Result<Option<Vec<u8>>> {
        let conn = self.lock();
        Ok(conn.query_row("SELECT emb FROM faces WHERE id = ?1", params![id], |r| r.get(0)).optional()?)
    }

    /// Unnamed faces with their fingerprints (to name look-alikes too).
    pub fn unnamed_face_embs(&self, limit: usize) -> Result<Vec<(i64, Vec<u8>)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, emb FROM faces WHERE labeled = 0 ORDER BY at_ms DESC LIMIT ?1")?;
        let rows = st.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Name faces as `person` (None = not this person after all: unnamed).
    pub fn label_faces(&self, ids: &[i64], person: Option<i64>) -> Result<usize> {
        let conn = self.lock();
        let mut n = 0;
        for id in ids {
            n += conn.execute(
                "UPDATE faces SET person_id = ?2, labeled = ?3, sim = NULL WHERE id = ?1",
                params![id, person, person.is_some() as i64],
            )?;
        }
        Ok(n)
    }

    pub fn delete_face(&self, id: i64) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM faces WHERE id = ?1", params![id])? > 0)
    }

    /// Unnamed faces older than `before_ms` (their events are long gone).
    pub fn prune_faces(&self, before_ms: i64) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM faces WHERE labeled = 0 AND at_ms < ?1", params![before_ms])?)
    }

    /// The person with this name, made if new.
    pub fn person_id(&self, name: &str, now_ms: i64) -> Result<i64> {
        let conn = self.lock();
        conn.execute("INSERT OR IGNORE INTO people (name, created_ms) VALUES (?1, ?2)", params![name, now_ms])?;
        Ok(conn.query_row("SELECT id FROM people WHERE name = ?1", params![name], |r| r.get(0))?)
    }

    /// People with how many named faces each and one to show.
    pub fn people(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT p.id, p.name, p.quiet,
                    (SELECT COUNT(*) FROM faces f WHERE f.person_id = p.id AND f.labeled = 1),
                    (SELECT COUNT(*) FROM faces f WHERE f.person_id = p.id),
                    (SELECT f.id FROM faces f WHERE f.person_id = p.id ORDER BY f.labeled DESC, f.size DESC LIMIT 1),
                    (SELECT MAX(f.at_ms) FROM faces f WHERE f.person_id = p.id)
             FROM people p ORDER BY p.name COLLATE NOCASE",
        )?;
        let rows = st.query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "quiet": r.get::<_, i64>(2)? == 1,
                "named": r.get::<_, i64>(3)?,
                "seen": r.get::<_, i64>(4)?,
                "face": r.get::<_, Option<i64>>(5)?,
                "last_ms": r.get::<_, Option<i64>>(6)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Ids of people who should not cause alerts on their own.
    pub fn quiet_people(&self) -> Result<Vec<i64>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id FROM people WHERE quiet = 1")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn person_name(&self, id: i64) -> Result<Option<String>> {
        Ok(self.lock().query_row("SELECT name FROM people WHERE id = ?1", params![id], |r| r.get(0)).optional()?)
    }

    pub fn update_person(&self, id: i64, name: Option<&str>, quiet: Option<bool>) -> Result<bool> {
        let conn = self.lock();
        let mut n = 0;
        if let Some(nm) = name {
            n += conn.execute("UPDATE people SET name = ?2 WHERE id = ?1", params![id, nm])?;
        }
        if let Some(q) = quiet {
            n += conn.execute("UPDATE people SET quiet = ?2 WHERE id = ?1", params![id, q as i64])?;
        }
        Ok(n > 0)
    }

    /// Forget a person: their name and every picture and fingerprint of
    /// their face (named or matched) are deleted.
    pub fn delete_person(&self, id: i64) -> Result<bool> {
        let conn = self.lock();
        conn.execute("DELETE FROM faces WHERE person_id = ?1", params![id])?;
        Ok(conn.execute("DELETE FROM people WHERE id = ?1", params![id])? > 0)
    }

    #[track_caller]
    fn lock(&self) -> TimedLock<'_> {
        let at = std::panic::Location::caller();
        let asked = std::time::Instant::now();
        let guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        TimedLock { guard, at, asked, got: std::time::Instant::now() }
    }

    /// The connection for heavy read-only queries (see `ro`).
    #[track_caller]
    fn lock_ro(&self) -> TimedLock<'_> {
        if self.ro.is_empty() {
            return self.lock();
        }
        let at = std::panic::Location::caller();
        let asked = std::time::Instant::now();
        // A free one if there is one; all busy: look again for whichever frees up first for
        // a couple of seconds (one may be busy with a slow read), then wait for one in turn.
        for round in 0..200 {
            for c in &self.ro {
                match c.try_lock() {
                    Ok(guard) => return TimedLock { guard, at, asked, got: std::time::Instant::now() },
                    Err(std::sync::TryLockError::Poisoned(e)) => return TimedLock { guard: e.into_inner(), at, asked, got: std::time::Instant::now() },
                    Err(std::sync::TryLockError::WouldBlock) => {}
                }
            }
            if round < 199 {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let i = self.ro_next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.ro.len();
        let guard = self.ro[i].lock().unwrap_or_else(|e| e.into_inner());
        TimedLock { guard, at, asked, got: std::time::Instant::now() }
    }

    // ---- segments ----

    pub fn insert_segment(&self, camera: &str, seg: &SegmentRow) -> Result<()> {
        self.note_segment_length(seg);
        insert_segment_on(&self.lock(), camera, seg)
    }

    fn note_segment_length(&self, seg: &SegmentRow) {
        self.longest_segment.fetch_max(seg.end_ms - seg.start_ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Earliest start a segment overlapping `from_ms` can have.
    fn earliest_start(&self, from_ms: i64) -> i64 {
        // (A minute of slack over the longest one stored.)
        from_ms.saturating_sub(self.longest_segment.load(std::sync::atomic::Ordering::Relaxed).max(0) + 60_000)
    }

    /// Segments with `start_ms < to_ms && end_ms > from_ms`, ordered by start.
    pub fn segments_overlapping(&self, camera: &str, from_ms: i64, to_ms: i64) -> Result<Vec<SegmentRow>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {SEG_COLS} FROM segments
             WHERE camera = ?1 AND start_ms < ?3 AND start_ms >= ?4 AND end_ms > ?2
             ORDER BY start_ms, id"
        ))?;
        let rows = st.query_map(params![camera, from_ms, to_ms, self.earliest_start(from_ms)], segment_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Up to `limit` segments (all cameras) starting before (`before_ms`,
    /// `before_id`), newest first, each with its id (for the next page).
    pub fn segments_before(&self, before_ms: i64, before_id: i64, limit: usize) -> Result<Vec<(i64, SegmentRow)>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT id, {SEG_COLS} FROM segments
             WHERE start_ms < ?1 OR (start_ms = ?1 AND id < ?2)
             ORDER BY start_ms DESC, id DESC LIMIT ?3"
        ))?;
        let rows = st.query_map(params![before_ms, before_id, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                SegmentRow { path: r.get(1)?, start_ms: r.get(2)?, end_ms: r.get(3)?, bytes: r.get(4)?, video_frames: r.get(5)?, kind: r.get(6)?, folder: r.get(7)? },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Segment containing `at_ms` (start inclusive, end exclusive).
    pub fn segment_at(&self, camera: &str, at_ms: i64) -> Result<Option<SegmentRow>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {SEG_COLS} FROM segments
             WHERE camera = ?1 AND start_ms <= ?2 AND start_ms >= ?3 AND end_ms > ?2
             ORDER BY start_ms DESC, id DESC LIMIT 1"
        ))?;
        let mut rows = st.query_map(params![camera, at_ms, self.earliest_start(at_ms)], segment_from_row)?;
        Ok(rows.next().transpose()?)
    }

    /// Oldest segments across all cameras as `(camera, row)`.
    pub fn oldest_segments(&self, limit: usize) -> Result<Vec<(String, SegmentRow)>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {SEG_COLS}, camera FROM segments ORDER BY start_ms, id LIMIT ?1"
        ))?;
        let rows = st.query_map(params![limit as i64], |r| Ok((r.get::<_, String>(7)?, segment_from_row(r)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Mark the footage an event covers (`cam` = camera slug, ms bounds
    /// with pre/post roll), now and for recordings still to come.
    pub fn mark_event_footage(&self, id: &str, cam: &str, from_ms: i64, to_ms: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("UPDATE events SET span_cam = ?2, span_from = ?3, span_to = ?4 WHERE id = ?1", params![id, cam, from_ms, to_ms])?;
        conn.execute(
            "UPDATE segments SET event = 1 WHERE camera = ?1 AND start_ms < ?3 AND start_ms > ?2 - ?4 AND end_ms > ?2 AND event = 0",
            params![cam, from_ms, to_ms, MAX_SEGMENT_MS],
        )?;
        Ok(())
    }

    /// Events whose footage has not been marked yet: (id, camera, start, end).
    pub fn events_without_span(&self, limit: usize) -> Result<Vec<(String, String, i64, i64)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, camera, start, end FROM events WHERE span_cam IS NULL LIMIT ?1")?;
        let rows = st.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Recordings of one folder in the order a full folder gives them up:
    /// quiet footage and event footage that started before `events_kept_from_ms`
    /// oldest first, then the newer event footage oldest first.
    /// Each row comes with whether it is kept event footage (the second part).
    pub fn segments_to_purge(&self, folder: &str, events_kept_from_ms: i64, limit: usize) -> Result<Vec<(SegmentRow, bool)>> {
        let conn = self.lock();
        let mut out: Vec<(SegmentRow, bool)> = {
            let mut st = conn.prepare(&format!(
                "SELECT {SEG_COLS} FROM segments WHERE folder = ?1 AND (event = 0 OR start_ms < ?2) ORDER BY start_ms, id LIMIT ?3"
            ))?;
            let rows = st.query_map(params![folder, events_kept_from_ms, limit as i64], |r| Ok((segment_from_row(r)?, false)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if out.len() < limit {
            let mut st = conn.prepare(&format!(
                "SELECT {SEG_COLS} FROM segments WHERE folder = ?1 AND event = 1 AND start_ms >= ?2 ORDER BY start_ms, id LIMIT ?3"
            ))?;
            let rows = st.query_map(params![folder, events_kept_from_ms, (limit - out.len()) as i64], segment_from_row)?;
            for r in rows {
                out.push((r?, true));
            }
        }
        Ok(out)
    }

    /// Oldest recordings in one folder.
    pub fn oldest_segments_in(&self, folder: &str, limit: usize) -> Result<Vec<SegmentRow>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {SEG_COLS} FROM segments WHERE folder = ?1 ORDER BY start_ms, id LIMIT ?2"
        ))?;
        let rows = st.query_map(params![folder, limit as i64], segment_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether the running recording totals are complete (checked again
    /// until they are).
    pub fn segment_totals_ready(&self) -> bool {
        if self.totals_ready.load(std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
        let upto: i64 = self.lock_ro().query_row("SELECT upto FROM seg_totals_fill WHERE id = 1", [], |r| r.get(0)).optional().ok().flatten().unwrap_or(0);
        let ready = upto == SEG_TOTALS_ALL;
        if ready {
            self.totals_ready.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        ready
    }

    /// Count the recordings not yet in the running totals into them, a few
    /// thousand at a time with pauses (it runs on its own thread at a start
    /// after the upgrade; recording is not held up). Returns how many.
    pub fn fill_segment_totals(&self) -> Result<i64> {
        self.fill_segment_totals_paced(Duration::from_millis(20))
    }

    fn fill_segment_totals_paced(&self, pause: Duration) -> Result<i64> {
        let mut counted = 0i64;
        // (All at once when there is no pause: a small index at open.)
        let step = if pause.is_zero() { i64::MAX } else { SEG_TOTALS_STEP };
        while let Some(n) = self.fill_segment_totals_step(step)? {
            counted += n;
            std::thread::sleep(pause);
        }
        Ok(counted)
    }

    /// Count the next `step` ids into the totals: Some(recordings counted),
    /// None when the totals are complete.
    fn fill_segment_totals_step(&self, step: i64) -> Result<Option<i64>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let upto: i64 = tx.query_row("SELECT upto FROM seg_totals_fill WHERE id = 1", [], |r| r.get(0))?;
        if upto == SEG_TOTALS_ALL {
            self.totals_ready.store(true, std::sync::atomic::Ordering::Relaxed);
            return Ok(None);
        }
        let max_id: i64 = tx.query_row("SELECT COALESCE(MAX(id), 0) FROM segments", [], |r| r.get(0))?;
        let hi = upto.saturating_add(step).min(max_id);
        tx.execute(
            "INSERT INTO seg_totals (folder, camera, bytes, n)
             SELECT folder, camera, SUM(bytes), COUNT(*) FROM segments WHERE id > ?1 AND id <= ?2 GROUP BY folder, camera
             ON CONFLICT (folder, camera) DO UPDATE SET bytes = bytes + excluded.bytes, n = n + excluded.n",
            params![upto, hi],
        )?;
        tx.execute(
            "INSERT INTO seg_days (camera, day, bytes, ms, n)
             SELECT camera, start_ms / 86400000, SUM(bytes), SUM(MAX(end_ms - start_ms, 0)), COUNT(*) FROM segments
             WHERE id > ?1 AND id <= ?2 GROUP BY camera, start_ms / 86400000
             ON CONFLICT (camera, day) DO UPDATE SET bytes = bytes + excluded.bytes, ms = ms + excluded.ms, n = n + excluded.n",
            params![upto, hi],
        )?;
        let counted: i64 = tx.query_row("SELECT COUNT(*) FROM segments WHERE id > ?1 AND id <= ?2", params![upto, hi], |r| r.get(0))?;
        let done = hi >= max_id;
        tx.execute("UPDATE seg_totals_fill SET upto = ?1 WHERE id = 1", params![if done { SEG_TOTALS_ALL } else { hi }])?;
        tx.commit()?;
        if done {
            self.totals_ready.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(Some(counted))
    }

    /// Bytes of recordings indexed in one folder.
    pub fn folder_bytes(&self, folder: &str) -> Result<i64> {
        let sql = if self.segment_totals_ready() {
            "SELECT COALESCE(SUM(bytes), 0) FROM seg_totals WHERE folder = ?1"
        } else {
            "SELECT COALESCE(SUM(bytes), 0) FROM segments WHERE folder = ?1"
        };
        Ok(self.lock_ro().query_row(sql, params![folder], |r| r.get(0))?)
    }

    /// Record that a recording was moved to another folder (same relative path).
    pub fn set_segment_folder(&self, path: &str, folder: &str) -> Result<bool> {
        Ok(self.lock().execute("UPDATE segments SET folder = ?2 WHERE path = ?1", params![path, folder])? > 0)
    }

    pub fn delete_segment(&self, path: &str) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM segments WHERE path = ?1", params![path])? > 0)
    }

    pub fn total_segment_bytes(&self) -> Result<i64> {
        let table = if self.segment_totals_ready() { "seg_totals" } else { "segments" };
        Ok(self.lock_ro().query_row(&format!("SELECT COALESCE(SUM(bytes), 0) FROM {table}"), [], |r| r.get(0))?)
    }

    /// Per camera, of the recordings that started on or after the first
    /// midnight (UTC) at or after `since_ms`: (camera, bytes, first start,
    /// recorded ms). From the daily totals: a few hundred rows.
    pub fn camera_rates(&self, since_ms: i64) -> Result<Vec<(String, i64, i64, i64)>> {
        const DAY: i64 = 86_400_000;
        let day0 = since_ms.max(0).div_euclid(DAY) + i64::from(since_ms.max(0).rem_euclid(DAY) != 0);
        if !self.segment_totals_ready() {
            return self.camera_rates_scan(day0 * DAY);
        }
        let conn = self.lock_ro();
        let mut st = conn.prepare("SELECT camera, SUM(bytes), SUM(ms) FROM seg_days WHERE day >= ?1 GROUP BY camera HAVING SUM(n) > 0")?;
        let rows: Vec<(String, i64, i64)> = st.query_map(params![day0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut first = conn.prepare_cached("SELECT MIN(start_ms) FROM segments WHERE camera = ?1 AND start_ms >= ?2")?;
        let mut out = Vec::with_capacity(rows.len());
        for (cam, bytes, ms) in rows {
            let f: Option<i64> = first.query_row(params![cam, day0 * DAY], |r| r.get(0))?;
            out.push((cam, bytes, f.unwrap_or(day0 * DAY), ms));
        }
        Ok(out)
    }

    /// `camera_rates` read from the recordings themselves (until the totals are filled).
    fn camera_rates_scan(&self, since_ms: i64) -> Result<Vec<(String, i64, i64, i64)>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare(
            "SELECT camera, COALESCE(SUM(bytes), 0), MIN(start_ms), COALESCE(SUM(MAX(end_ms - start_ms, 0)), 0)
             FROM segments WHERE start_ms >= ?1 GROUP BY camera",
        )?;
        let rows = st.query_map(params![since_ms], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The last hour per camera and stream. The time index is named: without
    /// table statistics SQLite picks the camera index for the GROUP BY and
    /// reads every recording (12 s on a big site), not only the last hour's.
    const CAMERA_KBPS_SQL: &str = "SELECT camera, kind = 'sub', COALESCE(SUM(bytes), 0), COALESCE(SUM(MAX(end_ms - start_ms, 0)), 0)
         FROM segments INDEXED BY idx_segments_start WHERE start_ms >= ?1 GROUP BY camera, kind = 'sub'";

    /// Per camera since `since_ms`: its recording bit rate (kbit/s), each
    /// stream's bytes over that stream's own recorded time, added (a camera
    /// recording its sub stream too writes both at once).
    pub fn camera_kbps(&self, since_ms: i64) -> Result<Vec<(String, f64)>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare(Self::CAMERA_KBPS_SQL)?;
        let rows = st.query_map(params![since_ms], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))?;
        let mut out: Vec<(String, f64)> = Vec::new();
        for r in rows {
            let (cam, bytes, ms) = r?;
            if ms <= 0 {
                continue;
            }
            let k = bytes as f64 * 8.0 / ms as f64;
            match out.iter_mut().find(|(c, _)| *c == cam) {
                Some(e) => e.1 += k,
                None => out.push((cam, k)),
            }
        }
        Ok(out)
    }

    /// Per camera: (camera, folder, bytes stored, the camera's oldest start
    /// in any folder). One row per camera and folder.
    pub fn camera_holdings(&self) -> Result<Vec<(String, String, i64, i64)>> {
        if !self.segment_totals_ready() {
            let conn = self.lock_ro();
            let mut st = conn.prepare(
                "SELECT camera, folder, COALESCE(SUM(bytes), 0), (SELECT MIN(start_ms) FROM segments o WHERE o.camera = s.camera)
                 FROM segments s GROUP BY camera, folder",
            )?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            return Ok(rows.collect::<rusqlite::Result<_>>()?);
        }
        let conn = self.lock_ro();
        let mut st = conn.prepare(
            "SELECT camera, folder, bytes, (SELECT MIN(start_ms) FROM segments o WHERE o.camera = t.camera) FROM seg_totals t WHERE n > 0",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, Option<i64>>(3)?.unwrap_or(0))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// (recording files, events) in the index.
    pub fn counts(&self) -> Result<(i64, i64)> {
        let ready = self.segment_totals_ready();
        let conn = self.lock_ro();
        let segs = conn.query_row(if ready { "SELECT COALESCE(SUM(n), 0) FROM seg_totals" } else { "SELECT COUNT(*) FROM segments" }, [], |r| r.get(0))?;
        let evs = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        Ok((segs, evs))
    }

    pub fn segment_bytes_by_camera(&self) -> Result<Vec<(String, i64)>> {
        let table = if self.segment_totals_ready() { "seg_totals" } else { "segments" };
        let conn = self.lock_ro();
        let mut st = conn.prepare(&format!("SELECT camera, COALESCE(SUM(bytes), 0) FROM {table} GROUP BY camera ORDER BY camera"))?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- events ----

    pub fn insert_event(&self, ev: &Event) -> Result<()> {
        upsert_event(&self.lock(), ev)
    }

    /// Mark an event as checked by AI without anything that counts found.
    pub fn set_event_unconfirmed(&self, id: &str) -> Result<()> {
        self.lock().execute("UPDATE events SET unconfirmed = 1 WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Record a reviewer's verdict ("false_alarm" | "ok"; None clears it).
    /// Returns true when the event exists.
    pub fn set_event_review(&self, id: &str, verdict: Option<&str>, at: i64) -> Result<bool> {
        let n = self.lock().execute(
            "UPDATE events SET review = ?2, reviewed_at = ?3 WHERE id = ?1",
            params![id, verdict, verdict.map(|_| at)],
        )?;
        Ok(n > 0)
    }

    /// Verdicts of these events: id -> (verdict, when); unreviewed ones are absent.
    pub fn event_reviews(&self, ids: &[String]) -> Result<std::collections::HashMap<String, (String, i64)>> {
        let mut out = std::collections::HashMap::new();
        let conn = self.lock();
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut st = conn.prepare(&format!("SELECT id, review, COALESCE(reviewed_at, 0) FROM events WHERE review IS NOT NULL AND id IN ({marks})"))?;
            let rows = st.query_map(params_from_iter(chunk.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
            for row in rows {
                let (id, v, at) = row?;
                out.insert(id, (v, at));
            }
        }
        Ok(out)
    }

    /// Add labels to an event's own (those it lacks), whatever else was added
    /// meanwhile; the labels it has afterwards.
    pub fn add_event_objects(&self, id: &str, add: &[String]) -> Result<Vec<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let cur: Option<Option<String>> = tx.query_row("SELECT objects FROM events WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
        let Some(cur) = cur else { return Ok(Vec::new()) };
        let mut objs: Vec<String> = cur.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let before = objs.len();
        for a in add {
            if !objs.contains(a) {
                objs.push(a.clone());
            }
        }
        if objs.len() != before {
            tx.execute("UPDATE events SET objects = ?2 WHERE id = ?1", params![id, serde_json::to_string(&objs)?])?;
        }
        tx.commit()?;
        Ok(objs)
    }

    /// Every matching event's (id, camera, start, end, objects JSON, has
    /// picture), newest first, handed to `f` one by one (no limit, nothing
    /// kept: for summaries of long stretches).
    pub fn each_event_mark(&self, q: &EventQuery, mut f: impl FnMut(String, String, i64, i64, String, bool)) -> Result<()> {
        let (sql, args) = event_filter_sql(q, "id, camera, start, end, objects, thumb IS NOT NULL");
        let conn = self.lock();
        let mut st = conn.prepare(&format!("{sql} ORDER BY start DESC, id DESC"))?;
        let mut rows = st.query(params_from_iter(args))?;
        while let Some(r) = rows.next()? {
            f(r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<String>>(4)?.unwrap_or_default(), r.get(5)?);
        }
        Ok(())
    }

    pub fn query_events(&self, q: &EventQuery) -> Result<Vec<Event>> {
        let (mut sql, mut args) = event_filter_sql(q, EV_COLS);
        sql.push_str(" ORDER BY start DESC, id DESC LIMIT ? OFFSET ?");
        args.push((q.limit.min(i64::MAX as usize) as i64).into());
        args.push((q.offset.min(i64::MAX as usize) as i64).into());
        let conn = self.lock();
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map(params_from_iter(args), event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- interface state ----

    pub fn ui_get(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT value FROM ui_kv WHERE scope = ?1 AND key = ?2")?;
        let mut rows = st.query(params![scope, key])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    pub fn ui_put(&self, scope: &str, key: &str, value: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO ui_kv (scope, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
            params![scope, key, value],
        )?;
        Ok(())
    }

    /// All (key, value) pairs of one scope.
    pub fn ui_scope(&self, scope: &str) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT key, value FROM ui_kv WHERE scope = ?1 ORDER BY key")?;
        let rows = st.query_map(params![scope], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- audit log ----

    pub fn insert_audit(&self, user: &str, ip: &str, action: &str, detail: &str, status: u16) -> Result<()> {
        let ts = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let conn = self.lock();
        conn.execute(
            "INSERT INTO audit (ts, user, ip, action, detail, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![ts, user, ip, action, detail, status],
        )?;
        // Keep the log bounded.
        conn.execute("DELETE FROM audit WHERE id <= (SELECT MAX(id) FROM audit) - 20000", [])?;
        Ok(())
    }

    /// (user, action, detail) of every audit entry, oldest first.
    #[cfg(test)]
    pub fn audit_rows(&self) -> Vec<(String, String, String)> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT user, action, detail FROM audit ORDER BY id").unwrap();
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }


    pub fn delete_session(&self, hash: &str) -> Result<()> {
        self.lock().execute("DELETE FROM sessions WHERE hash = ?1", params![hash])?;
        Ok(())
    }

    /// End a user's sessions, except `keep` (a token hash). The browsers `forget` (device
    /// hashes of the sessions ended) are forgotten with them, and their notifications.
    pub fn delete_user_sessions(&self, user: &str, keep: Option<&str>, forget: &[String]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM sessions WHERE user = ?1 AND hash IS NOT ?2", params![user, keep])?;
        for d in forget {
            devices::forget_device(&tx, user, d)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_expired_sessions(&self, now: i64) -> Result<()> {
        self.lock().execute("DELETE FROM sessions WHERE expires <= ?1", params![now])?;
        Ok(())
    }


    /// A camera's restarts, power cycles and watchdog notes, newest first:
    /// (ts, user, action, detail, status). Details start "<camera>: ".
    pub fn camera_actions(&self, camera: &str, limit: usize) -> Result<Vec<(i64, String, String, String, u16)>> {
        let conn = self.lock();
        let prefix = format!("{camera}: ");
        let mut st = conn.prepare(
            "SELECT ts, user, action, detail, status FROM audit
             WHERE action IN ('camera restart', 'camera power cycle', 'watchdog gave up') AND substr(detail, 1, ?1) = ?2
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![prefix.chars().count() as i64, prefix, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Newest first: (ts, user, ip, action, detail, status).
    pub fn recent_audit(&self, limit: usize) -> Result<Vec<(i64, String, String, String, String, u16)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT ts, user, ip, action, detail, status FROM audit ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_event(&self, id: &str) -> Result<Option<Event>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!("SELECT {EV_COLS} FROM events WHERE id = ?1"))?;
        let mut rows = st.query_map(params![id], event_from_row)?;
        Ok(rows.next().transpose()?)
    }

    /// End of the newest indexed recording for a camera (ms).
    pub fn last_segment_end(&self, camera: &str) -> Result<Option<i64>> {
        Ok(self
            .lock_ro()
            // The newest few by start (an index walk): the one that ends last
            // is among them, and the whole history is not read.
            .query_row("SELECT MAX(end_ms) FROM (SELECT end_ms FROM segments WHERE camera = ?1 ORDER BY start_ms DESC LIMIT 20)", params![camera], |r| r.get(0))?)
    }

    /// As [`Db::last_segment_end`], main-stream recordings only.
    pub fn last_main_segment_end(&self, camera: &str) -> Result<Option<i64>> {
        Ok(self
            .lock_ro()
            .query_row(
                "SELECT MAX(end_ms) FROM (SELECT end_ms FROM segments WHERE camera = ?1 AND kind != 'sub' ORDER BY start_ms DESC LIMIT 20)",
                params![camera],
                |r| r.get(0),
            )?)
    }

    /// Which local days hold recordings and how many alerts: `starts` are the days' first
    /// milliseconds (the caller's days, daylight saving included), `end` the end of the last.
    /// One indexed look per camera and day (a recording that began the day before and runs
    /// into it counts), alerts counted up to 100 per camera and day. `cameras`: (slug the
    /// recordings are filed under, name the alerts are filed under). Returns each day's
    /// (recorded, alerts) and the oldest recording start among the cameras.
    pub fn recording_days(&self, starts: &[i64], end: i64, cameras: &[(String, String)], quiet_unconfirmed: &[String]) -> Result<(Vec<(bool, i64)>, Option<i64>)> {
        let conn = self.lock_ro();
        let mut began = conn.prepare_cached("SELECT 1 FROM segments WHERE camera = ?1 AND start_ms >= ?2 AND start_ms < ?3 LIMIT 1")?;
        let mut before = conn.prepare_cached("SELECT end_ms FROM segments WHERE camera = ?1 AND start_ms < ?2 ORDER BY start_ms DESC LIMIT 1")?;
        let mut oldest = conn.prepare_cached("SELECT MIN(start_ms) FROM segments WHERE camera = ?1")?;
        let mut alerts = conn.prepare_cached("SELECT COUNT(*) FROM (SELECT 1 FROM events WHERE camera = ?1 COLLATE NOCASE AND start >= ?2 AND start < ?3 LIMIT 100)")?;
        let mut alerts_confirmed =
            conn.prepare_cached("SELECT COUNT(*) FROM (SELECT 1 FROM events WHERE camera = ?1 COLLATE NOCASE AND start >= ?2 AND start < ?3 AND unconfirmed = 0 LIMIT 100)")?;
        let mut out = Vec::with_capacity(starts.len());
        for (i, &s) in starts.iter().enumerate() {
            let e = starts.get(i + 1).copied().unwrap_or(end);
            let (mut rec, mut n) = (false, 0i64);
            for (slug, cam) in cameras {
                if !rec {
                    rec = began.exists(params![slug, s, e])?
                        || before.query_row(params![slug, s], |r| r.get::<_, i64>(0)).optional()?.is_some_and(|x| x > s);
                }
                let q = if quiet_unconfirmed.iter().any(|c| c.eq_ignore_ascii_case(cam)) { &mut alerts_confirmed } else { &mut alerts };
                n += q.query_row(params![cam, s.div_euclid(1000), e.div_euclid(1000)], |r| r.get::<_, i64>(0))?;
            }
            out.push((rec, n));
        }
        let mut first: Option<i64> = None;
        for (slug, _) in cameras {
            if let Some(m) = oldest.query_row(params![slug], |r| r.get::<_, Option<i64>>(0))? {
                first = Some(first.map_or(m, |f| f.min(m)));
            }
        }
        Ok((out, first))
    }

    /// Start of the oldest recording still indexed for a camera.
    pub fn oldest_segment_start(&self, camera: &str) -> Result<Option<i64>> {
        Ok(self
            .lock()
            .query_row("SELECT MIN(start_ms) FROM segments WHERE camera = ?1", params![camera], |r| r.get(0))?)
    }

    pub fn total_events(&self) -> Result<usize> {
        let n: i64 = self.lock().query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Returns false when `id` does not exist.
    pub fn set_protect(&self, id: &str, protect: bool) -> Result<bool> {
        let n = self.lock().execute("UPDATE events SET protect = ?2 WHERE id = ?1", params![id, protect as i64])?;
        Ok(n > 0)
    }

    /// A camera was renamed: its recordings, events and AI reviews follow.
    /// (Files keep their paths; event ids keep the old name.)
    pub fn rename_camera(&self, old: &str, new: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("UPDATE segments SET camera = ?2 WHERE camera = ?1", params![crate::config::slug(old), crate::config::slug(new)])?;
        tx.execute("UPDATE events SET camera = ?2 WHERE camera = ?1", params![old, new])?;
        tx.execute("UPDATE ai_reviews SET camera = ?2 WHERE camera = ?1", params![old, new])?;
        tx.execute("UPDATE ai_review_sums SET camera = ?2 WHERE camera = ?1", params![old, new])?;
        tx.execute("UPDATE faces SET camera = ?2 WHERE camera = ?1", params![crate::config::slug(old), crate::config::slug(new)])?;
        tx.execute("UPDATE smart_vecs SET camera = ?2 WHERE camera = ?1", params![old, new])?;
        tx.execute("UPDATE plates SET camera = ?2 WHERE camera = ?1", params![crate::config::slug(old), crate::config::slug(new)])?;
        tx.execute("UPDATE tamper_log SET camera = ?2 WHERE camera = ?1", params![old, new])?;
        tx.execute("UPDATE camera_outages SET camera = ?2 WHERE camera = ?1", params![old, new])?;
        tx.execute("UPDATE activity_days SET camera = ?2 WHERE camera = ?1", params![old, new])?;
        tx.execute("UPDATE activity_meta SET camera = ?2 WHERE camera = ?1", params![old, new])?;
        tx.commit()?;
        Ok(())
    }

    // ---- camera tamper alerts ----

    /// Log a tamper alert; returns its id.
    pub fn tamper_add(&self, camera: &str, at: i64, kind: &str, light: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute("INSERT INTO tamper_log (camera, at, kind, light) VALUES (?1, ?2, ?3, ?4)", params![camera, at, kind, light])?;
        Ok(conn.last_insert_rowid())
    }

    /// The view is back.
    pub fn tamper_cleared(&self, id: i64, at: i64) -> Result<()> {
        self.lock().execute("UPDATE tamper_log SET cleared_at = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    /// A camera's tamper alerts, newest first: (id, at, kind, light, cleared_at).
    pub fn tamper_list(&self, camera: &str, limit: usize) -> Result<Vec<(i64, i64, String, String, Option<i64>)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, at, kind, light, cleared_at FROM tamper_log WHERE camera = ?1 ORDER BY at DESC, id DESC LIMIT ?2")?;
        let rows = st.query_map(params![camera, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Which camera this alert belongs to.
    pub fn tamper_owner(&self, id: i64) -> Result<Option<String>> {
        Ok(self.lock().query_row("SELECT camera FROM tamper_log WHERE id = ?1", params![id], |r| r.get(0)).optional()?)
    }

    /// Keep each camera's newest `keep` alerts; returns the ids removed
    /// (their picture files go too).
    pub fn tamper_prune(&self, keep: usize) -> Result<Vec<i64>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id FROM tamper_log t WHERE (SELECT COUNT(*) FROM tamper_log u WHERE u.camera = t.camera AND (u.at > t.at OR (u.at = t.at AND u.id > t.id))) >= ?1",
        )?;
        let ids: Vec<i64> = st.query_map(params![keep as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        drop(st);
        for id in &ids {
            conn.execute("DELETE FROM tamper_log WHERE id = ?1", params![id])?;
        }
        Ok(ids)
    }

    // ---- activity history (unusual-activity flagging) ----

    /// A camera's events starting in [from, to), as (start, labels).
    pub fn events_between(&self, camera: &str, from: i64, to: i64) -> Result<Vec<(i64, Vec<String>)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT start, objects FROM events WHERE camera = ?1 AND start >= ?2 AND start < ?3 ORDER BY start")?;
        let rows = st.query_map(params![camera, from, to], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (t, o) = r?;
            out.push((t, serde_json::from_str(&o).unwrap_or_default()));
        }
        Ok(out)
    }

    /// The earliest event of a camera.
    pub fn first_event_start(&self, camera: &str) -> Result<Option<i64>> {
        Ok(self.lock().query_row("SELECT MIN(start) FROM events WHERE camera = ?1", params![camera], |r| r.get(0))?)
    }

    /// Add active hours to the history (merged with what is there) and move
    /// the point up to which events have been read.
    pub fn activity_add(&self, camera: &str, rows: &[crate::unusual::DayRow], ingested_until: i64, first_day: i64) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for r in rows {
            tx.execute(
                "INSERT INTO activity_days (camera, day, label, hours) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(camera, day, label) DO UPDATE SET hours = hours | excluded.hours",
                params![camera, r.day, r.label, r.hours],
            )?;
        }
        tx.execute(
            "INSERT INTO activity_meta (camera, ingested_until, first_day) VALUES (?1, ?2, ?3)
             ON CONFLICT(camera) DO UPDATE SET ingested_until = excluded.ingested_until, first_day = MIN(first_day, excluded.first_day)",
            params![camera, ingested_until, first_day],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// (events read up to, first day observed) for a camera.
    pub fn activity_meta(&self, camera: &str) -> Result<Option<(i64, i64)>> {
        Ok(self.lock().query_row("SELECT ingested_until, first_day FROM activity_meta WHERE camera = ?1", params![camera], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    /// The history from `since_day` on.
    pub fn activity_rows(&self, camera: &str, since_day: i64) -> Result<Vec<crate::unusual::DayRow>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT day, label, hours FROM activity_days WHERE camera = ?1 AND day >= ?2")?;
        let rows = st.query_map(params![camera, since_day], |r| Ok(crate::unusual::DayRow { day: r.get(0)?, label: r.get(1)?, hours: r.get(2)? }))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Forget history older than `before_day`, and the history of every camera not in `keep`.
    pub fn activity_prune(&self, before_day: i64, keep: &[String]) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM activity_days WHERE day < ?1", params![before_day])?;
        let known: Vec<String> = {
            let mut st = conn.prepare("SELECT camera FROM activity_meta")?;
            let v = st.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
            v
        };
        for c in known.into_iter().filter(|c| !keep.contains(c)) {
            conn.execute("DELETE FROM activity_days WHERE camera = ?1", params![c])?;
            conn.execute("DELETE FROM activity_meta WHERE camera = ?1", params![c])?;
        }
        Ok(())
    }

    pub fn save_ai_review(&self, event_id: &str, camera: &str, start: i64, alerted: bool, detail: &str) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO ai_reviews (event_id, camera, start, alerted, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![event_id, camera, start, alerted, detail],
        )?;
        put_review_sum(&self.lock(), event_id, camera, start, alerted, detail);
        Ok(())
    }

    /// Change what an AI review says about the alert (e.g. a quiet rule held
    /// it back after the review was saved). False when there is no review.
    pub fn set_ai_review_alert(&self, event_id: &str, sent: bool, why: &str) -> Result<bool> {
        let conn = self.lock();
        let cur: Option<String> = conn.query_row("SELECT detail FROM ai_reviews WHERE event_id = ?1", params![event_id], |r| r.get(0)).optional()?;
        let Some(mut d) = cur.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) else { return Ok(false) };
        d["alert"] = serde_json::json!({ "sent": sent, "why": why });
        let text = d.to_string();
        conn.execute("UPDATE ai_reviews SET alerted = ?2, detail = ?3 WHERE event_id = ?1", params![event_id, sent, text])?;
        let camera_start: Option<(String, i64)> = conn.query_row("SELECT camera, start FROM ai_reviews WHERE event_id = ?1", params![event_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((camera, start)) = camera_start {
            put_review_sum(&conn, event_id, &camera, start, sent, &text);
        }
        Ok(true)
    }

    pub fn ai_review(&self, event_id: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT detail FROM ai_reviews WHERE event_id = ?1", params![event_id], |r| r.get(0))
            .optional()?)
    }

    /// Reviews newest first: (event id, camera, start, alerted, detail).
    pub fn ai_reviews(&self, since: i64, camera: Option<&str>, limit: usize) -> Result<Vec<(String, String, i64, bool, String)>> {
        let conn = self.lock_ro();
        // The newest `limit` of each camera (so a busy camera cannot crowd out a
        // quiet one). Ranked without `detail`: only the rows kept read that large text.
        let mut st = conn.prepare(
            "SELECT r.event_id, r.camera, r.start, r.alerted, r.detail FROM (
               SELECT event_id, ROW_NUMBER() OVER (PARTITION BY camera ORDER BY start DESC, event_id DESC) AS n
               FROM ai_reviews WHERE start >= ?1 AND (?2 IS NULL OR camera = ?2)) t
             JOIN ai_reviews r ON r.event_id = t.event_id
             WHERE t.n <= ?3 ORDER BY r.start DESC, r.event_id DESC",
        )?;
        let rows = st.query_map(params![since, camera, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Per camera since `since`: (camera, events checked, alerts sent): true counts, no limit.
    pub fn ai_review_counts(&self, since: i64, camera: Option<&str>) -> Result<Vec<(String, i64, i64)>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare(
            "SELECT camera, COUNT(*), COALESCE(SUM(alerted), 0) FROM ai_reviews
             WHERE start >= ?1 AND (?2 IS NULL OR camera = ?2) GROUP BY camera",
        )?;
        let rows = st.query_map(params![since, camera], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether every review has its one-line verdict in `ai_review_sums`, and
    /// the verdicts were checked against the reviews since this start (see
    /// `backfill_review_sums`).
    pub fn review_sums_ready(&self) -> bool {
        self.sums_checked.load(std::sync::atomic::Ordering::Relaxed) && self.ui_get("system", "review_sums_done").ok().flatten().is_some()
    }

    /// As at a start: the verdicts are not checked yet.
    #[cfg(test)]
    pub fn as_if_restarted(&self) {
        self.sums_checked.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Events since `since` that did not alert because a remembered spot held
    /// back what was seen (verdict "fixed_spot"). Only when `review_sums_ready`.
    pub fn fixed_spot_reviews(&self, since: i64, camera: &str) -> Result<usize> {
        let conn = self.lock_ro();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ai_review_sums WHERE camera = ?2 AND start >= ?1 AND alerted = 0 AND json_extract(summary, '$.kind') = 'fixed_spot'",
            params![since, camera],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// The same rows as `ai_reviews`, with the one-line verdict (JSON text) in place of the whole
    /// review. Reads only the small table. Only when `review_sums_ready`.
    pub fn ai_review_sums(&self, since: i64, camera: Option<&str>, limit: usize) -> Result<Vec<(String, String, i64, bool, String)>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare(
            "SELECT r.event_id, r.camera, r.start, r.alerted, r.summary FROM (
               SELECT event_id, ROW_NUMBER() OVER (PARTITION BY camera ORDER BY start DESC, event_id DESC) AS n
               FROM ai_review_sums WHERE start >= ?1 AND (?2 IS NULL OR camera = ?2)) t
             JOIN ai_review_sums r ON r.event_id = t.event_id
             WHERE t.n <= ?3 ORDER BY r.start DESC, r.event_id DESC",
        )?;
        let rows = st.query_map(params![since, camera, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The newest `limit` rows of `ai_review_sums` with one verdict kind
    /// (`noalert`: every one that sent no alert), in the whole period.
    /// Only when `review_sums_ready`.
    pub fn ai_review_sums_of_kind(&self, since: i64, camera: Option<&str>, kind: &str, limit: usize) -> Result<Vec<(String, String, i64, bool, String)>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare(
            "SELECT event_id, camera, start, alerted, summary FROM ai_review_sums
             WHERE start >= ?1 AND (?2 IS NULL OR camera = ?2)
               AND (CASE WHEN ?3 = 'noalert' THEN alerted = 0 ELSE json_extract(summary, '$.kind') = ?3 END)
             ORDER BY start DESC, event_id DESC LIMIT ?4",
        )?;
        let rows = st.query_map(params![since, camera, kind, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Like `ai_review_counts`, from the small table. Only when `review_sums_ready`.
    pub fn ai_review_sum_counts(&self, since: i64, camera: Option<&str>) -> Result<Vec<(String, i64, i64)>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare(
            "SELECT camera, COUNT(*), COALESCE(SUM(alerted), 0) FROM ai_review_sums
             WHERE start >= ?1 AND (?2 IS NULL OR camera = ?2) GROUP BY camera",
        )?;
        let rows = st.query_map(params![since, camera], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Give every review without a one-line verdict its verdict, a few hundred at a time with
    /// pauses (it runs on its own thread at each start; recording is not held up): those made
    /// before the verdicts existed, and those an older version (run in between, before an upgrade
    /// back) saved. Verdicts of reviews it removed, moved to a renamed camera or changed go first.
    /// The AI review page uses the slow way until it has finished. Returns how many it filled in.
    pub fn backfill_review_sums(&self) -> Result<usize> {
        let stale: Vec<String> = {
            let conn = self.lock_ro();
            let mut st = conn.prepare(
                "SELECT s.event_id FROM ai_review_sums s LEFT JOIN ai_reviews r ON r.event_id = s.event_id
                 WHERE r.event_id IS NULL OR r.camera != s.camera OR r.start != s.start OR r.alerted != s.alerted",
            )?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for ids in stale.chunks(300) {
            let conn = self.lock();
            let mut del = conn.prepare("DELETE FROM ai_review_sums WHERE event_id = ?1")?;
            for id in ids {
                del.execute(params![id])?;
            }
        }
        let mut last = 0i64;
        let mut done = 0usize;
        loop {
            let batch: Vec<(i64, String, String, i64, bool, String)> = {
                let conn = self.lock_ro();
                let mut st = conn.prepare(
                    "SELECT r.rowid, r.event_id, r.camera, r.start, r.alerted, r.detail FROM ai_reviews r
                     WHERE r.rowid > ?1 AND NOT EXISTS (SELECT 1 FROM ai_review_sums s WHERE s.event_id = r.event_id)
                     ORDER BY r.rowid LIMIT 300",
                )?;
                let rows = st.query_map(params![last], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let Some(newest) = batch.last().map(|b| b.0) else { break };
            last = newest;
            let conn = self.lock();
            for (_, id, camera, start, alerted, detail) in &batch {
                // (A review saved or changed meanwhile already has its own verdict: kept.)
                if let Ok(d) = serde_json::from_str::<serde_json::Value>(detail) {
                    let _ = conn.execute(
                        "INSERT OR IGNORE INTO ai_review_sums (event_id, camera, start, alerted, summary) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![id, camera, start, alerted, crate::ai_review::summarize(&d).to_string()],
                    );
                }
            }
            done += batch.len();
            drop(conn);
            std::thread::sleep(Duration::from_millis(40));
        }
        self.ui_put("system", "review_sums_done", "1")?;
        self.sums_checked.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(done)
    }

    /// Reviews of events that sent a notification from `since` on, with the
    /// owner's verdict on the event (`false_alarm` = "Not a real alert"):
    /// (event id, camera, start, detail, review). An alert held back by a pause,
    /// the alert window or the alert interval is not one that was sent
    /// (`notify_event` clears `alerted`).
    pub fn sent_reviews(&self, since: i64) -> Result<Vec<(String, String, i64, String, Option<String>)>> {
        let conn = self.lock_ro();
        let mut st = conn.prepare(
            "SELECT r.event_id, r.camera, r.start, r.detail, e.review FROM ai_reviews r
             LEFT JOIN events e ON e.id = r.event_id
             WHERE r.start >= ?1 AND r.alerted = 1 ORDER BY r.start DESC LIMIT 20000",
        )?;
        let rows = st.query_map(params![since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Event ids that still exist, of those given.
    pub fn existing_events(&self, ids: &[String]) -> Result<std::collections::HashSet<String>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT 1 FROM events WHERE id = ?1")?;
        let mut out = std::collections::HashSet::new();
        for id in ids {
            if st.exists(params![id])? {
                out.insert(id.clone());
            }
        }
        Ok(out)
    }

    pub fn delete_events(&self, ids: &[String]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut n = 0;
        {
            let mut st = tx.prepare("DELETE FROM events WHERE id = ?1")?;
            let mut ai = tx.prepare("DELETE FROM ai_reviews WHERE event_id = ?1")?;
            let mut ai_sum = tx.prepare("DELETE FROM ai_review_sums WHERE event_id = ?1")?;
            // Faces seen in the event go with it, unless someone named them
            // (named faces are what recognition matches against).
            let mut faces = tx.prepare("DELETE FROM faces WHERE event_id = ?1 AND labeled = 0")?;
            // Plate readings too, unless a known plate was read (its history stays).
            let mut plates = tx.prepare("DELETE FROM plates WHERE event_id = ?1 AND known_id IS NULL")?;
            let mut smart = tx.prepare("DELETE FROM smart_vecs WHERE event_id = ?1")?;
            let mut smart_done = tx.prepare("DELETE FROM smart_done WHERE event_id = ?1")?;
            for id in ids {
                n += st.execute(params![id])?;
                ai.execute(params![id])?;
                ai_sum.execute(params![id])?;
                faces.execute(params![id])?;
                plates.execute(params![id])?;
                smart.execute(params![id])?;
                smart_done.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Oldest unprotected events first.
    pub fn oldest_unprotected_events(&self, limit: usize) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {EV_COLS} FROM events WHERE protect = 0 ORDER BY start, id LIMIT ?1"
        ))?;
        let rows = st.query_map(params![limit as i64], event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Unprotected events that ended before `cutoff_secs` (for age retention).
    pub fn events_before(&self, cutoff_secs: i64) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {EV_COLS} FROM events WHERE protect = 0 AND end < ?1 ORDER BY start, id"
        ))?;
        let rows = st.query_map(params![cutoff_secs], event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn protected_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id FROM events WHERE protect = 1 ORDER BY start, id")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- legacy import ----

    /// Import `events.jsonl` and pre-existing `cameras/<slug>/segments/*.mp4`.
    /// Idempotent. `fallback_secs` is the assumed duration of a legacy segment
    /// that cannot be chained to its predecessor.
    /// `folders` are the extra recording folders (name, path); recordings in
    /// `data_dir` belong to folder "".
    pub fn import_legacy(&self, data_dir: &Path, folders: &[(String, PathBuf)], fallback_secs: u64) -> Result<ImportReport> {
        let mut report = self.import_events_log(data_dir)?;
        report.segments_imported = self.import_existing_recordings(data_dir, folders, fallback_secs)?;
        Ok(report)
    }

    /// True while [`Self::begin_startup_import`]'s guard is alive: the index
    /// of recordings may be missing old ones, so nothing may be judged or
    /// deleted from it (retention waits).
    pub fn importing(&self) -> bool {
        self.importing.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How long the start-up indexing has been running (None: not running).
    pub fn importing_for(&self) -> Option<Duration> {
        if !self.importing() {
            return None;
        }
        self.import_started.lock().unwrap_or_else(|e| e.into_inner()).map(|t| t.elapsed())
    }

    /// True the first time it is asked during one start-up indexing run
    /// (so a warning about a long run is logged once).
    pub fn first_overdue_note(&self) -> bool {
        !self.import_warned.swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// Marks the start-up indexing as running until the guard is dropped.
    pub fn begin_startup_import(self: &Arc<Self>) -> StartupImport {
        *self.import_started.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
        self.import_warned.store(false, std::sync::atomic::Ordering::SeqCst);
        self.importing.store(true, std::sync::atomic::Ordering::SeqCst);
        StartupImport(self.clone())
    }

    /// The one-time move of `events.jsonl` into the database.
    pub fn import_events_log(&self, data_dir: &Path) -> Result<ImportReport> {
        let mut report = ImportReport::default();
        let mut conn = self.lock();

        let log = data_dir.join("events.jsonl");
        if log.is_file() {
            let text = std::fs::read(&log).with_context(|| format!("read {}", log.display()))?;
            let text = String::from_utf8_lossy(&text);
            let tx = conn.transaction()?;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                match serde_json::from_str::<Event>(line) {
                    Ok(ev) => {
                        upsert_event(&tx, &ev)?;
                        report.events_imported += 1;
                    }
                    Err(_) => report.events_skipped += 1,
                }
            }
            tx.commit()?;
            let mut dest = log.clone().into_os_string();
            dest.push(".imported");
            let dest = std::path::PathBuf::from(dest);
            // rename() will not replace an existing file on Windows.
            let _ = std::fs::remove_file(&dest);
            std::fs::rename(&log, &dest).with_context(|| format!("rename {}", log.display()))?;
        }

        Ok(report)
    }

    /// Index the recordings already on disk in every folder. The walk holds
    /// no lock; what is found is added a camera at a time in small batches
    /// (see `insert_found_segments`), so a stop half way loses nothing: the
    /// next run adds what is left. Returns how many were added.
    pub fn import_existing_recordings(&self, data_dir: &Path, folders: &[(String, PathBuf)], fallback_secs: u64) -> Result<usize> {
        let started = std::time::Instant::now();
        let mut added = 0;
        let mut seen = 0;
        let mut roots = vec![(String::new(), data_dir.to_path_buf())];
        roots.extend(folders.iter().cloned());
        for (folder, root) in &roots {
            let cams = root.join("cameras");
            let mut slugs: Vec<_> = match std::fs::read_dir(&cams) {
                Ok(rd) => rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).collect(),
                Err(_) => Vec::new(),
            };
            slugs.sort_by_key(|e| e.file_name());
            for cam in slugs {
                let slug = cam.file_name().to_string_lossy().into_owned();
                let found = scan_segment_dir(folder, root, &slug, fallback_secs);
                seen += found.len();
                added += self.insert_found_segments(&found)?;
            }
        }
        let took = started.elapsed();
        if took.as_secs() >= 5 {
            tracing::info!(
                "indexing existing recordings looked at {seen} files and added {added} in {} s",
                took.as_secs()
            );
        }
        Ok(added)
    }

    /// Index the finished recordings in these cameras' folders
    /// (slug, folder name, folder root): the periodic scan for cameras on
    /// the ffmpeg pipeline, whose recordings nothing else reports.
    #[cfg(test)]
    pub fn import_camera_dirs(&self, dirs: &[(String, String, PathBuf)], fallback_secs: u64) -> Result<usize> {
        let mut found = Vec::new();
        for (slug, folder, root) in dirs {
            found.extend(scan_segment_dir(folder, root, slug, fallback_secs));
        }
        if found.is_empty() {
            return Ok(0);
        }
        self.insert_found_segments(&found)
    }

    /// Like [`Self::import_camera_dirs`], adding only recordings that ended
    /// at or after `since_ms` (the walk still reads every file, to chain
    /// start times; the database is held only for the recent ones).
    pub fn import_recent_camera_dirs(
        &self,
        dirs: &[(String, String, PathBuf)],
        fallback_secs: u64,
        since_ms: i64,
    ) -> Result<usize> {
        let mut found = Vec::new();
        for (slug, folder, root) in dirs {
            found.extend(scan_segment_dir(folder, root, slug, fallback_secs).into_iter().filter(|f| f.end_ms >= since_ms));
        }
        if found.is_empty() {
            return Ok(0);
        }
        self.insert_found_segments(&found)
    }

    fn insert_found_segments(&self, found: &[FoundSegment]) -> Result<usize> {
        self.insert_found_in_batches(found, SEGMENT_BATCH_ROWS, SEGMENT_BATCH_TIME, &mut || true)
    }

    /// Adds the found recordings in batches of at most `max_rows` rows and
    /// `max_time`, each its own transaction, releasing the database between
    /// them (`between` runs then; returning false stops, as a crash would).
    /// Rows already known are ignored, so a repeat or a later run adds only
    /// what is missing.
    fn insert_found_in_batches(
        &self,
        found: &[FoundSegment],
        max_rows: usize,
        max_time: std::time::Duration,
        between: &mut dyn FnMut() -> bool,
    ) -> Result<usize> {
        let started = std::time::Instant::now();
        let mut n = 0;
        let mut done = 0;
        let mut longest_hold = std::time::Duration::ZERO;
        while done < found.len() {
            let held = std::time::Instant::now();
            {
                let mut conn = self.lock();
                let tx = conn.transaction()?;
                {
                    let mut st = tx.prepare_cached(
                        "INSERT OR IGNORE INTO segments (camera, path, start_ms, end_ms, bytes, video_frames, kind, folder)
                         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?7, ?6)",
                    )?;
                    let first = done;
                    while done < found.len() && (done == first || (done - first < max_rows && held.elapsed() < max_time)) {
                        let f = &found[done];
                        n += st.execute(params![f.camera, f.path, f.start_ms, f.end_ms, f.bytes, f.folder, f.kind])?;
                        // (As insert_segment does: a longer segment found by the walk is looked for.)
                        self.longest_segment.fetch_max(f.end_ms - f.start_ms, std::sync::atomic::Ordering::Relaxed);
                        done += 1;
                    }
                }
                tx.commit()?;
            }
            longest_hold = longest_hold.max(held.elapsed());
            if done < found.len() {
                if !between() {
                    break;
                }
                // Let whoever is waiting for the database take it.
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        if started.elapsed().as_secs() >= 5 {
            tracing::info!(
                "adding {} recordings to the index took {} s (database held at most {} ms at a time)",
                found.len(),
                started.elapsed().as_secs(),
                longest_hold.as_millis()
            );
        }
        Ok(n)
    }
}

/// Rows per transaction when adding found recordings, and the longest one
/// transaction may run (a cold disk makes each row slow, so time counts too).
const SEGMENT_BATCH_ROWS: usize = 500;
const SEGMENT_BATCH_TIME: std::time::Duration = std::time::Duration::from_millis(150);

/// Held while the start-up indexing of existing recordings runs.
pub struct StartupImport(Arc<Db>);

impl Drop for StartupImport {
    fn drop(&mut self) {
        self.0.importing.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A finished recording file found on disk.
struct FoundSegment {
    camera: String,
    path: String,
    start_ms: i64,
    end_ms: i64,
    bytes: i64,
    folder: String,
    kind: &'static str,
}

/// The finished recordings in `<root>/cameras/<slug>/segments` (no database
/// needed). Start times come from the previous file of the same run, else
/// `fallback_secs` before the file's time.
fn scan_segment_dir(folder: &str, root: &Path, slug: &str, fallback_secs: u64) -> Vec<FoundSegment> {
    let seg_dir = root.join("cameras").join(slug).join("segments");
    let Ok(rd) = std::fs::read_dir(&seg_dir) else { return Vec::new() };
    // (prefix, number, path, mtime ms, bytes)
    let mut files: Vec<(String, u32, std::path::PathBuf, i64, i64)> = Vec::new();
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        let Some((prefix, num)) = split_segment_name(name) else { continue };
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
        let Some(mtime) = mtime else { continue };
        files.push((prefix.to_string(), num, p.clone(), mtime.as_millis() as i64, meta.len() as i64));
    }
    files.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    let fallback_ms = fallback_secs as i64 * 1000;
    // A file touched within the last segment length (+3 s) may still
    // be being written; it is picked up by a later scan once closed.
    let now_ms = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(i64::MAX);
    let still_open = |end_ms: i64| end_ms > now_ms - fallback_ms - 3000;
    let mut out = Vec::new();
    let mut prev: Option<(&str, u32, i64)> = None;
    for (prefix, num, path, end_ms, bytes) in &files {
        let chained = match prev {
            Some((pp, pn, pend)) if pp == prefix && *num == pn + 1 => {
                let gap = end_ms - pend;
                (gap > 0 && gap <= fallback_ms * 2).then_some(pend)
            }
            _ => None,
        };
        prev = Some((prefix, *num, *end_ms));
        if still_open(*end_ms) {
            continue;
        }
        // Sub-stream files (the built-in reader's, or fragmented MP4 from
        // ffmpeg's sub recorder) play straight from the file; ffmpeg's
        // main-stream files are joined into a clip file ("legacy").
        let kind = if prefix.starts_with(&format!("{slug}-sub-")) { "sub" } else { "legacy" };
        out.push(FoundSegment {
            camera: slug.to_string(),
            path: relative_path(root, path),
            start_ms: chained.unwrap_or(end_ms - fallback_ms),
            end_ms: *end_ms,
            bytes: *bytes,
            folder: folder.to_string(),
            kind,
        });
    }
    out
}

/// The spans a signal was on in [from, to], given its state at `from` and
/// its changes after it (in order).
pub fn signal_on_spans(on_at_start: bool, changes: &[(i64, bool)], from: i64, to: i64) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut since = on_at_start.then_some(from);
    for &(at, on) in changes {
        match (since, on) {
            (None, true) => since = Some(at),
            (Some(a), false) => {
                out.push((a, at));
                since = None;
            }
            _ => {}
        }
    }
    if let Some(a) = since {
        if to > a {
            out.push((a, to));
        }
    }
    out
}

/// Path of a recording relative to the data folder, with '/' separators, so
/// the index survives moving the data folder (or reading it on another OS).
/// Paths outside the data folder are stored as given.
pub fn relative_path(data_dir: &Path, path: &Path) -> String {
    match path.strip_prefix(data_dir) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// `{...}-seg{NNNNNN}.mp4` -> (name up to and including `seg`, number).
fn split_segment_name(name: &str) -> Option<(&str, u32)> {
    let stem = name.strip_suffix(".mp4")?;
    let (head, num) = stem.split_at(stem.len().checked_sub(6)?);
    if !num.bytes().all(|b| b.is_ascii_digit()) || !head.ends_with("seg") {
        return None;
    }
    Some((head, num.parse().ok()?))
}

#[cfg(test)]
mod tests {

    #[test]
    fn last_hour_rates_read_only_the_last_hour() {
        let db = super::Db::open_in_memory().unwrap();
        let conn = db.lock();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", super::Db::CAMERA_KBPS_SQL))
            .unwrap()
            .query_map(rusqlite::params![0i64], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(plan.iter().any(|p| p.contains("SEARCH segments USING INDEX idx_segments_start")), "{plan:?}");
    }
    use super::*;
    use std::fs::File;

    fn ev(id: &str, cam: &str, start: i64, end: i64, objs: &[&str]) -> Event {
        Event {
            id: id.into(),
            camera_id: 1,
            camera: cam.into(),
            start,
            end,
            score: 0.5,
            clip: Some(format!("{id}.mp4")),
            thumb: None,
            objects: objs.iter().map(|s| s.to_string()).collect(),
            protect: false,
        }
    }

    fn seg(path: &str, s: i64, e: i64) -> SegmentRow {
        SegmentRow { path: path.into(), start_ms: s, end_ms: e, bytes: 100, video_frames: 10, kind: "continuous".into(), folder: String::new() }
    }

    fn ids(v: &[Event]) -> Vec<&str> {
        v.iter().map(|e| e.id.as_str()).collect()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-dbtest-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn camera_actions_only_for_that_camera() {
        let d = tmp("camacts");
        let db = Db::open(&d.join("a.db")).unwrap();
        db.insert_audit("(watchdog)", "", "camera restart", "Shed: ONVIF, ok", 200).unwrap();
        db.insert_audit("(watchdog)", "", "camera restart", "Back Gate: ONVIF, ok", 200).unwrap();
        db.insert_audit("admin", "1.2.3.4", "POST", "/api/cameras/1/reboot", 200).unwrap();
        db.insert_audit("admin", "", "camera power cycle", "Shed: failed: no answer", 502).unwrap();
        db.insert_audit("(watchdog)", "", "watchdog gave up", "50%_Cam: no way", 200).unwrap();
        let a = db.camera_actions("Shed", 10).unwrap();
        assert_eq!(a.iter().map(|r| r.2.as_str()).collect::<Vec<_>>(), vec!["camera power cycle", "camera restart"]);
        assert_eq!(a[0].4, 502);
        // Names with LIKE wildcards are matched literally.
        assert_eq!(db.camera_actions("50%_Cam", 10).unwrap().len(), 1);
        assert!(db.camera_actions("50", 10).unwrap().is_empty());
    }

    #[test]
    fn tamper_log_and_activity_history() {
        let db = Db::open_in_memory().unwrap();
        let a = db.tamper_add("Yard", 100, "covered", "day").unwrap();
        let b = db.tamper_add("Yard", 200, "moved", "night").unwrap();
        db.tamper_add("Gate", 150, "blurred", "day").unwrap();
        db.tamper_cleared(a, 160).unwrap();
        let l = db.tamper_list("Yard", 10).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].0, l[0].4), (b, None));
        assert_eq!(l[1].4, Some(160));
        assert_eq!(db.tamper_owner(a).unwrap().as_deref(), Some("Yard"));
        assert_eq!(db.tamper_prune(1).unwrap(), vec![a]);
        assert_eq!(db.tamper_list("Yard", 10).unwrap().len(), 1);
        assert_eq!(db.tamper_list("Gate", 10).unwrap().len(), 1);

        use crate::unusual::DayRow;
        let row = |day, label: &str, hours| DayRow { day, label: label.into(), hours };
        db.activity_add("Yard", &[row(10, "person", 0b0110), row(11, "person", 1)], 5000, 10).unwrap();
        // The same day again merges its hours.
        db.activity_add("Yard", &[row(10, "person", 0b1000)], 6000, 12).unwrap();
        assert_eq!(db.activity_meta("Yard").unwrap(), Some((6000, 10)));
        let mut rows = db.activity_rows("Yard", 0).unwrap();
        rows.sort_by_key(|r| r.day);
        assert_eq!(rows, vec![row(10, "person", 0b1110), row(11, "person", 1)]);
        db.activity_prune(11, &["Yard".to_string()]).unwrap();
        assert_eq!(db.activity_rows("Yard", 0).unwrap().len(), 1);
        db.activity_prune(0, &[]).unwrap();
        assert!(db.activity_rows("Yard", 0).unwrap().is_empty());
        assert_eq!(db.activity_meta("Yard").unwrap(), None);
        db.insert_event(&ev("a", "Front", 10, 20, &["person".into()])).unwrap();
        db.insert_event(&ev("b", "Front", 30, 40, &[])).unwrap();
        let e = db.events_between("Front", 0, 30).unwrap();
        assert_eq!(e, vec![(10, vec!["person".to_string()])]);
        assert_eq!(db.first_event_start("Front").unwrap(), Some(10));
        assert_eq!(db.first_event_start("None").unwrap(), None);
    }

    #[test]
    fn reopen_keeps_data_and_version() {
        let dir = tmp("reopen");
        let p = dir.join("ambersight.db");
        {
            let db = Db::open(&p).unwrap();
            db.insert_event(&ev("a", "Front", 10, 20, &[])).unwrap();
            db.insert_segment("front", &seg("s1.mp4", 0, 1000)).unwrap();
        }
        let db = Db::open(&p).unwrap();
        assert_eq!(db.total_events().unwrap(), 1);
        assert_eq!(db.total_segment_bytes().unwrap(), 100);
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let mode: String = db.lock().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn renaming_a_camera_moves_its_history() {
        let db = Db::open_in_memory().unwrap();
        db.insert_event(&ev("front-1", "Front", 10, 20, &[])).unwrap();
        db.insert_segment("front", &seg("s1.mp4", 0, 1000)).unwrap();
        db.save_ai_review("front-1", "Front", 10, true, "{}").unwrap();
        db.rename_camera("Front", "Porch").unwrap();
        let q = EventQuery { limit: 10, camera: Some("Porch".into()), ..Default::default() };
        assert_eq!(db.query_events(&q).unwrap().len(), 1);
        assert_eq!(db.last_segment_end("porch").unwrap(), Some(1000));
        assert_eq!(db.ai_reviews(0, Some("Porch"), 10).unwrap().len(), 1);
    }

    #[test]
    fn review_verdicts_are_kept_apart_and_old_reviews_are_filled_in() {
        let db = Db::open_in_memory().unwrap();
        // A new database has nothing to fill in.
        assert!(db.review_sums_ready());
        let d = |sent: bool| serde_json::json!({ "wanted": ["person"], "min_score": 0.5, "alert": { "sent": sent }, "checks": [{ "seen": [{ "label": "person", "score": 0.9 }] }] }).to_string();
        db.save_ai_review("a", "Front", 10, true, &d(true)).unwrap();
        let r = db.ai_review_sums(0, None, 10).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].4.contains("\"alerted\""), "{}", r[0].4);
        // A held-back alert changes the verdict.
        db.set_ai_review_alert("a", false, "paused").unwrap();
        assert!(db.ai_review_sums(0, None, 10).unwrap()[0].4.contains("alerts_off"));
        // A review from before the table existed: not ready until it is filled in.
        db.lock().execute("INSERT INTO ai_reviews (event_id, camera, start, alerted, detail) VALUES ('old', 'Front', 5, 0, ?1)", params![d(false)]).unwrap();
        db.lock().execute("DELETE FROM ui_kv WHERE key = 'review_sums_done'", []).unwrap();
        assert!(!db.review_sums_ready());
        assert_eq!(db.backfill_review_sums().unwrap(), 1);
        assert!(db.review_sums_ready());
        let c = db.ai_review_sum_counts(0, None).unwrap();
        assert_eq!(c, vec![("Front".to_string(), 2, 0)]);
        assert_eq!(c, db.ai_review_counts(0, None).unwrap());
        // Rename and delete follow.
        db.rename_camera("Front", "Porch").unwrap();
        assert_eq!(db.ai_review_sums(0, Some("Porch"), 10).unwrap().len(), 2);
        db.delete_events(&["a".to_string()]).unwrap();
        assert_eq!(db.ai_review_sums(0, None, 10).unwrap().len(), 1);
    }

    #[test]
    fn review_verdicts_are_checked_at_each_start() {
        let db = Db::open_in_memory().unwrap();
        let d = |sent: bool| serde_json::json!({ "wanted": ["person"], "min_score": 0.5, "alert": { "sent": sent }, "checks": [{ "seen": [{ "label": "person", "score": 0.9 }] }] }).to_string();
        db.save_ai_review("a", "Front", 10, true, &d(true)).unwrap();
        db.save_ai_review("b", "Front", 20, true, &d(true)).unwrap();
        db.save_ai_review("c", "Front", 30, true, &d(true)).unwrap();
        // An older version ran (it knows reviews, not their verdicts): it
        // saved one, removed one, renamed the camera and held an alert back.
        let raw = |sql: &str| db.lock().execute(sql, []).unwrap();
        raw(&format!("INSERT INTO ai_reviews (event_id, camera, start, alerted, detail) VALUES ('new', 'Gate', 40, 1, '{}')", d(true)));
        raw("DELETE FROM ai_reviews WHERE event_id = 'a'");
        raw("UPDATE ai_reviews SET camera = 'Gate'");
        raw(&format!("UPDATE ai_reviews SET alerted = 0, detail = '{}' WHERE event_id = 'c'", d(false)));
        // Started again: the slow way until they are checked.
        db.as_if_restarted();
        assert!(!db.review_sums_ready());
        assert_eq!(db.backfill_review_sums().unwrap(), 3);
        assert!(db.review_sums_ready());
        let r = db.ai_review_sums(0, None, 10).unwrap();
        let ids: Vec<(&str, &str, bool)> = r.iter().map(|x| (x.0.as_str(), x.1.as_str(), x.3)).collect();
        assert_eq!(ids, vec![("new", "Gate", true), ("c", "Gate", false), ("b", "Gate", true)]);
        assert_eq!(db.ai_review_sum_counts(0, None).unwrap(), db.ai_review_counts(0, None).unwrap());
        // Nothing to do the next time.
        assert_eq!(db.backfill_review_sums().unwrap(), 0);
    }

    #[test]
    fn ai_review_counts_are_true_and_the_sample_is_per_camera() {
        let db = Db::open_in_memory().unwrap();
        for i in 0..5 {
            db.save_ai_review(&format!("a{i}"), "Busy", 100 + i, i % 2 == 0, "{}").unwrap();
        }
        db.save_ai_review("q1", "Quiet", 50, true, "{}").unwrap();
        let mut c = db.ai_review_counts(0, None).unwrap();
        c.sort();
        assert_eq!(c, vec![("Busy".to_string(), 5, 3), ("Quiet".to_string(), 1, 1)]);
        // Two newest of each camera, so the busy one does not crowd the quiet one out.
        let r = db.ai_reviews(0, None, 2).unwrap();
        assert_eq!(r.iter().filter(|x| x.1 == "Busy").count(), 2);
        assert_eq!(r.iter().filter(|x| x.1 == "Quiet").count(), 1);
        assert_eq!(r.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["a4", "a3", "q1"], "newest first, with what each one holds");
        assert_eq!(r[0].4, "{}");
        assert_eq!(db.ai_review_counts(0, Some("Quiet")).unwrap().len(), 1);
    }

    #[test]
    fn ai_review_list_of_one_kind_looks_through_the_whole_period() {
        let db = Db::open_in_memory().unwrap();
        {
            let c = db.lock();
            for i in 0..50 {
                let kind = if i % 10 == 0 { "camera_only" } else { "nothing" };
                c.execute(
                    "INSERT INTO ai_review_sums (event_id, camera, start, alerted, summary) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![format!("e{i}"), if i < 25 { "A" } else { "B" }, 1000 + i, i % 7 == 0, format!("{{\"kind\":\"{kind}\"}}")],
                )
                .unwrap();
            }
        }
        let ids = |v: Vec<(String, String, i64, bool, String)>| v.into_iter().map(|r| r.0).collect::<Vec<_>>();
        assert_eq!(ids(db.ai_review_sums_of_kind(0, None, "camera_only", 300).unwrap()), ["e40", "e30", "e20", "e10", "e0"]);
        assert_eq!(ids(db.ai_review_sums_of_kind(0, Some("A"), "camera_only", 2).unwrap()), ["e20", "e10"]);
        assert_eq!(ids(db.ai_review_sums_of_kind(1035, None, "camera_only", 300).unwrap()), ["e40"]);
        assert_eq!(db.ai_review_sums_of_kind(0, None, "noalert", 300).unwrap().len(), 50 - 8);
        assert!(db.ai_review_sums_of_kind(0, None, "nope", 300).unwrap().is_empty());
    }

    #[test]
    fn event_upsert_and_filters() {
        let db = Db::open_in_memory().unwrap();
        db.insert_event(&ev("e1", "Front", 100, 110, &["Person"])).unwrap();
        db.insert_event(&ev("e2", "Front", 200, 210, &["car", "Dog"])).unwrap();
        db.insert_event(&ev("e3", "Back", 300, 310, &[])).unwrap();
        db.insert_event(&ev("e4", "Back", 400, 410, &["person"])).unwrap();
        // upsert replaces
        let mut e1 = ev("e1", "Front", 100, 110, &["Person"]);
        e1.score = 0.9;
        db.insert_event(&e1).unwrap();
        assert_eq!(db.total_events().unwrap(), 4);

        let q = |f: &dyn Fn(&mut EventQuery)| {
            let mut q = EventQuery { limit: 100, ..Default::default() };
            f(&mut q);
            db.query_events(&q).unwrap()
        };
        let all = q(&|_| {});
        assert_eq!(ids(&all), ["e4", "e3", "e2", "e1"]);
        assert_eq!(all[3].score, 0.9);
        assert_eq!(all[2].objects, ["car", "Dog"]);
        assert_eq!(ids(&q(&|q| q.camera = Some("fRoNt".into()))), ["e2", "e1"]);
        assert!(q(&|q| q.camera = Some("Fro".into())).is_empty());
        assert_eq!(q(&|q| q.camera = Some(String::new())).len(), 4);
        assert_eq!(ids(&q(&|q| q.object = Some("PERS".into()))), ["e4", "e1"]);
        assert_eq!(ids(&q(&|q| q.object = Some("oG".into()))), ["e2"]);
        // overlap: end >= since && start <= until
        assert_eq!(ids(&q(&|q| q.since = Some(110))), ["e4", "e3", "e2", "e1"]);
        assert_eq!(ids(&q(&|q| q.since = Some(111))), ["e4", "e3", "e2"]);
        assert_eq!(ids(&q(&|q| q.until = Some(200))), ["e2", "e1"]);
        assert_eq!(ids(&q(&|q| q.until = Some(199))), ["e1"]);
        // Unconfirmed events: left out only for the cameras asked, kept otherwise;
        // an upsert keeps the mark.
        db.set_event_unconfirmed("e3").unwrap();
        db.set_event_unconfirmed("e2").unwrap();
        db.insert_event(&ev("e2", "Front", 200, 212, &["car", "Dog"])).unwrap();
        assert_eq!(ids(&q(&|q| q.hide_unconfirmed = vec!["back".into()])), ["e4", "e2", "e1"]);
        assert_eq!(ids(&q(&|q| q.hide_unconfirmed = vec!["Back".into(), "Front".into()])), ["e4", "e1"]);
        assert_eq!(q(&|_| {}).len(), 4);
        assert_eq!(ids(&q(&|q| q.cameras = vec!["back".into()])), ["e4", "e3"]);
        assert_eq!(ids(&q(&|q| { q.cameras = vec!["Back".into(), "Front".into()]; q.hide_unconfirmed = vec!["Back".into()]; })), ["e4", "e2", "e1"]);
        assert_eq!(
            ids(&q(&|q| {
                q.since = Some(205);
                q.until = Some(305);
            })),
            ["e3", "e2"]
        );
        // paging
        assert_eq!(ids(&q(&|q| q.limit = 2)), ["e4", "e3"]);
        assert_eq!(
            ids(&q(&|q| {
                q.limit = 2;
                q.offset = 2;
            })),
            ["e2", "e1"]
        );
        assert_eq!(
            ids(&q(&|q| {
                q.limit = 2;
                q.offset = 3;
            })),
            ["e1"]
        );
        assert!(q(&|q| q.offset = 10).is_empty());
        assert!(q(&|q| q.limit = 0).is_empty());
    }

    #[test]
    fn protect_and_retention_queries() {
        let db = Db::open_in_memory().unwrap();
        db.insert_event(&ev("a", "c", 10, 20, &[])).unwrap();
        db.insert_event(&ev("b", "c", 30, 40, &[])).unwrap();
        db.insert_event(&ev("c", "c", 50, 60, &[])).unwrap();
        assert!(db.set_protect("a", true).unwrap());
        assert!(!db.set_protect("nope", true).unwrap());
        assert_eq!(db.protected_ids().unwrap(), ["a"]);
        let all = db.query_events(&EventQuery { limit: 10, ..Default::default() }).unwrap();
        assert!(all.iter().any(|e| e.id == "a" && e.protect));
        assert_eq!(ids(&db.oldest_unprotected_events(10).unwrap()), ["b", "c"]);
        assert_eq!(ids(&db.oldest_unprotected_events(1).unwrap()), ["b"]);
        assert_eq!(ids(&db.events_before(100).unwrap()), ["b", "c"]);
        assert!(db.events_before(40).unwrap().is_empty());
        assert_eq!(ids(&db.events_before(41).unwrap()), ["b"]);
        assert_eq!(db.delete_events(&["b".into(), "zzz".into(), "c".into()]).unwrap(), 2);
        assert_eq!(db.total_events().unwrap(), 1);
        assert!(db.set_protect("a", false).unwrap());
        assert!(db.protected_ids().unwrap().is_empty());
    }

    #[test]
    fn v13_marks_earlier_unconfirmed_events_from_their_ai_review() {
        let dir = tmp("v13-migrate");
        let path = dir.join("ambersight.db");
        {
            let db = Db::open(&path).unwrap();
            for id in ["flag", "person", "failed", "sound", "doorbell", "none"] {
                db.insert_event(&ev(id, "Front", 10, 20, &[])).unwrap();
            }
            let rev = |id: &str, d: &str| db.save_ai_review(id, "Front", 10, false, d).unwrap();
            rev("flag", r#"{"source":"built-in (Small model)","confirmed":false,"sound":false,"camera_labels":[],"alert":{"why":"ai_no_wanted"}}"#);
            rev("person", r#"{"source":"built-in (Small model)","confirmed":true,"sound":false,"camera_labels":[],"alert":{"why":"sent"}}"#);
            rev("failed", r#"{"source":"camera","confirmed":false,"sound":false,"camera_labels":[],"alert":{"why":"ai_failed"}}"#);
            rev("sound", r#"{"source":"built-in (Small model)","confirmed":false,"sound":true,"camera_labels":[],"alert":{"why":"sent"}}"#);
            rev("doorbell", r#"{"source":"built-in (Small model)","confirmed":false,"sound":false,"camera_labels":["visitor"],"alert":{"why":"ai_no_wanted"}}"#);
            // Back to a version 12 database.
            db.lock().execute_batch("DROP INDEX idx_events_unconfirmed; DROP INDEX idx_events_review; DROP INDEX idx_events_protect; ALTER TABLE events DROP COLUMN review; ALTER TABLE events DROP COLUMN reviewed_at; ALTER TABLE events DROP COLUMN unconfirmed; PRAGMA user_version = 12;").unwrap();
        }
        let db = Db::open(&path).unwrap();
        let q = EventQuery { limit: 10, hide_unconfirmed: vec!["Front".into()], ..Default::default() };
        let mut left: Vec<String> = db.query_events(&q).unwrap().into_iter().map(|e| e.id).collect();
        left.sort();
        assert_eq!(left, ["doorbell", "failed", "none", "person", "sound"]);
    }

    #[test]
    fn v17_marks_existing_push_devices_as_browsers() {
        let dir = tmp("v17-migrate");
        let path = dir.join("ambersight.db");
        {
            let db = Db::open(&path).unwrap();
            db.lock().execute_batch("ALTER TABLE push_subs DROP COLUMN kind; PRAGMA user_version = 16;").unwrap();
            db.lock()
                .execute("INSERT INTO push_subs (endpoint, user, p256dh, auth, device, created_ms) VALUES ('https://fcm.googleapis.com/fcm/send/a', 'ann', 'p', 'a', 'Chrome on Android', 1)", [])
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let l = db.push_list(None).unwrap();
        assert_eq!((l.len(), l[0].kind.as_str()), (1, "browser"));
        let app = PushSub { endpoint: "https://ntfy.sh/upabc?up=1".into(), user: "ann".into(), p256dh: "p".into(), auth: "a".into(), device: "Pixel 8".into(), kind: "app".into(), ..Default::default() };
        db.push_save(&app, 2).unwrap();
        drop(db);
        // Opened again: nothing changes (the step is not run twice).
        let db = Db::open(&path).unwrap();
        let l = db.push_list(None).unwrap();
        assert_eq!(l.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>(), ["browser", "app"]);
        assert_eq!(l[1].device, "Pixel 8");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn v16_keeps_the_presence_rule_and_old_phones_go() {
        let dir = tmp("v16-migrate");
        let path = dir.join("ambersight.db");
        {
            let db = Db::open(&path).unwrap();
            db.lock().execute_batch("DROP TABLE presence_rule; PRAGMA user_version = 15;").unwrap();
        }
        let db = Db::open(&path).unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(db.presence_rule_get().unwrap(), None);
        let r = PresenceRuleRow { known_home: Some(false), left_at_ms: Some(5), acted_away: false };
        db.presence_rule_put(Some(&r)).unwrap();
        let r2 = PresenceRuleRow { known_home: Some(false), left_at_ms: None, acted_away: true };
        db.presence_rule_put(Some(&r2)).unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.presence_rule_get().unwrap(), Some(r2));
        db.presence_rule_put(None).unwrap();
        assert_eq!(db.presence_rule_get().unwrap(), None);
        // Phones silent for long are removed.
        let dev = |id: &str, t: i64| PresenceDevice { user: "a".into(), device_id: id.into(), name: id.into(), home: true, updated_ms: t, created_ms: 0 };
        db.presence_report(&dev("old", 10), 5, 5).unwrap().unwrap();
        db.presence_report(&dev("new", 100), 5, 5).unwrap().unwrap();
        assert_eq!(db.presence_purge(50).unwrap(), 1);
        assert_eq!(db.presence_list(None).unwrap().iter().map(|d| d.device_id.as_str()).collect::<Vec<_>>(), ["new"]);
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn v15_adds_presence_devices_with_limits() {
        let dir = tmp("v15-migrate");
        let path = dir.join("ambersight.db");
        {
            let db = Db::open(&path).unwrap();
            db.lock().execute_batch("DROP TABLE presence_devices; PRAGMA user_version = 14;").unwrap();
        }
        let db = Db::open(&path).unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let dev = |u: &str, id: &str, home: bool, t: i64| PresenceDevice { user: u.into(), device_id: id.into(), name: format!("{id} phone"), home, updated_ms: t, created_ms: 0 };
        db.presence_report(&dev("ann", "a1", true, 10), 2, 3).unwrap().unwrap();
        db.presence_report(&dev("ann", "a2", false, 20), 2, 3).unwrap().unwrap();
        // A third for ann is refused; the same one again is an update.
        assert!(db.presence_report(&dev("ann", "a3", true, 30), 2, 3).unwrap().is_err());
        db.presence_report(&dev("ann", "a1", false, 40), 2, 3).unwrap().unwrap();
        db.presence_report(&dev("bob", "b1", true, 50), 2, 3).unwrap().unwrap();
        // The server total.
        assert!(db.presence_report(&dev("cat", "c1", true, 60), 2, 3).unwrap().is_err());
        let ann = db.presence_list(Some("ann")).unwrap();
        assert_eq!(ann.len(), 2);
        let a1 = ann.iter().find(|d| d.device_id == "a1").unwrap();
        assert_eq!((a1.home, a1.updated_ms, a1.created_ms), (false, 40, 10));
        assert_eq!(db.presence_list(None).unwrap().len(), 3);
        // The same id under another account is another device.
        assert!(!db.presence_delete("bob", "a1").unwrap());
        assert!(db.presence_delete("ann", "a1").unwrap());
        assert_eq!(db.presence_delete_user("ann").unwrap(), 1);
        assert_eq!(db.presence_list(None).unwrap().len(), 1);
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn v14_adds_review_columns_and_filters_work() {
        let dir = tmp("v14-migrate");
        let path = dir.join("ambersight.db");
        {
            let db = Db::open(&path).unwrap();
            db.insert_event(&ev("a", "Front", 10, 20, &["person"])).unwrap();
            // Back to a version 13 database.
            db.lock().execute_batch("DROP INDEX idx_events_review; DROP INDEX idx_events_protect; ALTER TABLE events DROP COLUMN review; ALTER TABLE events DROP COLUMN reviewed_at; PRAGMA user_version = 13;").unwrap();
        }
        let db = Db::open(&path).unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        for (id, cam, start, objs) in [("b", "Back", 30, vec!["car"]), ("c", "Front", 40, vec!["Dog", "person"]), ("d", "Side", 50, vec![])] {
            db.insert_event(&ev(id, cam, start, start + 5, &objs)).unwrap();
        }
        let q = |f: &dyn Fn(&mut EventQuery)| {
            let mut q = EventQuery { limit: 100, ..Default::default() };
            f(&mut q);
            db.query_events(&q).unwrap().into_iter().map(|e| e.id).collect::<Vec<_>>()
        };
        assert!(db.set_event_review("a", Some("false_alarm"), 111).unwrap());
        assert!(db.set_event_review("b", Some("ok"), 222).unwrap());
        assert!(!db.set_event_review("zz", Some("ok"), 222).unwrap());
        // An upsert (the event is saved again) keeps the verdict.
        db.insert_event(&ev("a", "Front", 10, 20, &["person"])).unwrap();
        let r = db.event_reviews(&["a".into(), "b".into(), "c".into()]).unwrap();
        assert_eq!(r.get("a"), Some(&("false_alarm".to_string(), 111)));
        assert_eq!(r.get("b"), Some(&("ok".to_string(), 222)));
        assert!(!r.contains_key("c"));
        assert_eq!(q(&|_| {}), ["d", "c", "b", "a"]);
        assert_eq!(q(&|q| q.review = vec!["false_alarm".into()]), ["a"]);
        assert_eq!(q(&|q| q.review = vec!["ok".into()]), ["b"]);
        assert_eq!(q(&|q| q.review = vec!["none".into()]), ["d", "c"]);
        assert_eq!(q(&|q| q.review = vec!["ok".into(), "none".into()]), ["d", "c", "b"]);
        // Clearing the verdict.
        db.set_event_review("a", None, 333).unwrap();
        assert_eq!(q(&|q| q.review = vec!["false_alarm".into()]), Vec::<String>::new());
        assert!(db.event_reviews(&["a".into()]).unwrap().is_empty());
        // Protected only, several labels, several cameras, ids.
        db.set_protect("c", true).unwrap();
        assert_eq!(q(&|q| q.protected_only = true), ["c"]);
        assert_eq!(q(&|q| q.objects_any = vec!["CAR".into(), "dog".into()]), ["c", "b"]);
        assert_eq!(q(&|q| q.objects_any = vec!["person".into()]), ["c", "a"]);
        assert_eq!(q(&|q| { q.cameras = vec!["front".into(), "side".into()]; q.objects_any = vec!["person".into()]; }), ["c", "a"]);
        assert_eq!(q(&|q| q.ids = vec!["b".into(), "d".into(), "nope".into()]), ["d", "b"]);
        assert_eq!(q(&|q| { q.ids = vec!["a".into(), "b".into()]; q.cameras = vec!["Back".into()]; }), ["b"]);
        assert_eq!(q(&|q| { q.protected_only = true; q.review = vec!["none".into()]; }), ["c"]);
    }

    #[test]
    fn ai_review_alert_can_be_changed_afterwards() {
        let dir = tmp("ai-alert");
        let db = Db::open(&dir.join("ambersight.db")).unwrap();
        db.save_ai_review("e", "Front", 10, true, r#"{"alert":{"sent":true,"why":"sent"},"x":1}"#).unwrap();
        assert!(db.set_ai_review_alert("e", false, "quiet_rule").unwrap());
        let d: serde_json::Value = serde_json::from_str(&db.ai_review("e").unwrap().unwrap()).unwrap();
        assert_eq!(d["alert"]["why"], "quiet_rule");
        assert_eq!(d["alert"]["sent"], false);
        assert_eq!(d["x"], 1);
        assert!(!db.ai_reviews(0, None, 10).unwrap()[0].3);
        assert!(!db.set_ai_review_alert("nope", false, "quiet_rule").unwrap());
    }

    #[test]
    fn object_filter_ignores_names_inside_tags_and_add_objects_merges() {
        let dir = tmp("obj-tags");
        let db = Db::open(&dir.join("ambersight.db")).unwrap();
        db.insert_event(&ev("t", "Front", 10, 20, &["car", "rule: person on the porch", "plate ABC (Person's car)", "loitering: Gate"])).unwrap();
        db.insert_event(&ev("p", "Front", 30, 40, &["person"])).unwrap();
        let q = |o: &str| {
            let q = EventQuery { limit: 100, object: Some(o.into()), ..Default::default() };
            db.query_events(&q).unwrap().into_iter().map(|e| e.id).collect::<Vec<_>>()
        };
        assert_eq!(q("person"), ["p"]);
        assert_eq!(q("car"), ["t"]);
        assert_eq!(q("loitering: gate"), ["t"]);
        assert_eq!(q("rule: person"), ["t"]);
        assert_eq!(q("plate abc"), ["t"]);
        let now = db.add_event_objects("p", &["Mom".into(), "person".into()]).unwrap();
        assert_eq!(now, ["person", "Mom"]);
        let now = db.add_event_objects("p", &["rule: x".into()]).unwrap();
        assert_eq!(now, ["person", "Mom", "rule: x"]);
        assert!(db.add_event_objects("nope", &["a".into()]).unwrap().is_empty());
    }

    #[test]
    fn big_reads_use_their_own_connection_and_see_new_recordings() {
        let dir = tmp("ro-conn");
        let db = Db::open(&dir.join("ambersight.db")).unwrap();
        assert!(!db.ro.is_empty());
        let mut a = seg("a1", 1000, 2000);
        a.kind = "sub".into();
        db.insert_segment("cam", &seg("m1", 0, 1000)).unwrap();
        db.insert_segment("cam", &a).unwrap();
        // Another connection's read, while the main one is held (a long write).
        let held = db.lock();
        assert_eq!(db.folder_bytes("").unwrap(), 200);
        assert_eq!(db.counts().unwrap().0, 2);
        assert_eq!(db.last_segment_end("cam").unwrap(), Some(2000));
        assert_eq!(db.last_main_segment_end("cam").unwrap(), Some(1000));
        drop(held);
        db.insert_segment("cam", &seg("m2", 3000, 4000)).unwrap();
        assert_eq!(db.last_main_segment_end("cam").unwrap(), Some(4000));
        assert_eq!(db.last_segment_end("other").unwrap(), None);
    }

    #[test]
    fn migrates_v1_database_and_tracks_folders() {
        let dir = tmp("v1-migrate");
        let path = dir.join("ambersight.db");
        {
            // A database as schema version 1 left it (no folder column).
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE segments (id INTEGER PRIMARY KEY, camera TEXT NOT NULL, path TEXT NOT NULL UNIQUE,
                   start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL, bytes INTEGER NOT NULL,
                   video_frames INTEGER NOT NULL, kind TEXT NOT NULL);
                 CREATE TABLE events (id TEXT PRIMARY KEY, camera_id INTEGER, camera TEXT, start INTEGER, end INTEGER,
                   score REAL, clip TEXT, thumb TEXT, objects TEXT, protect INTEGER);
                 INSERT INTO segments (camera, path, start_ms, end_ms, bytes, video_frames, kind)
                   VALUES ('cam', 'cameras/cam/segments/old.mp4', 0, 1000, 500, 10, 'continuous');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // Old rows belong to the data folder.
        let old = db.segments_overlapping("cam", 0, 1000).unwrap();
        assert_eq!(old[0].folder, "");
        // New rows can name a folder; per-folder queries see only their own.
        let mut s = seg("cameras/cam/segments/new.mp4", 1000, 2000);
        s.folder = "nas".into();
        db.insert_segment("cam", &s).unwrap();
        assert_eq!(db.folder_bytes("").unwrap(), 500);
        assert_eq!(db.folder_bytes("nas").unwrap(), 100);
        assert_eq!(db.oldest_segments_in("nas", 10).unwrap().len(), 1);
        // Moving to the archive keeps the relative path.
        assert!(db.set_segment_folder("cameras/cam/segments/old.mp4", "archive").unwrap());
        assert_eq!(db.folder_bytes("archive").unwrap(), 500);
        assert_eq!(db.folder_bytes("").unwrap(), 0);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_recordings_are_found_from_inside() {
        let db = Db::open_in_memory().unwrap();
        let paths = |v: Vec<SegmentRow>| v.into_iter().map(|s| s.path).collect::<Vec<_>>();
        // A very long recording (an import) is still found from deep inside
        // it, also after the database is opened again.
        let h = 3_600_000;
        db.insert_segment("long", &seg("l.mp4", 0, 5 * h)).unwrap();
        db.insert_segment("long", &seg("m.mp4", 5 * h, 5 * h + 5000)).unwrap();
        assert_eq!(paths(db.segments_overlapping("long", 4 * h, 4 * h + 1).unwrap()), ["l.mp4"]);
        assert_eq!(db.segment_at("long", 4 * h).unwrap().unwrap().path, "l.mp4");
        let dir = tmp("longest-seg");
        {
            let d = Db::open(&dir.join("a.db")).unwrap();
            d.insert_segment("long", &seg("l.mp4", 0, 5 * h)).unwrap();
        }
        let d = Db::open(&dir.join("a.db")).unwrap();
        assert_eq!(paths(d.segments_overlapping("long", 4 * h, 4 * h + 1).unwrap()), ["l.mp4"]);
    }

    #[test]
    fn segment_queries_and_boundaries() {
        let db = Db::open_in_memory().unwrap();
        db.insert_segment("cam", &seg("a.mp4", 0, 1000)).unwrap();
        db.insert_segment("cam", &seg("b.mp4", 1000, 2000)).unwrap();
        db.insert_segment("cam", &seg("c.mp4", 5000, 6000)).unwrap();
        db.insert_segment("other", &seg("o.mp4", 0, 9000)).unwrap();

        let paths = |v: Vec<SegmentRow>| v.into_iter().map(|s| s.path).collect::<Vec<_>>();
        assert_eq!(paths(db.segments_overlapping("cam", 500, 1500).unwrap()), ["a.mp4", "b.mp4"]);
        // touching edges do not overlap
        assert_eq!(paths(db.segments_overlapping("cam", 1000, 2000).unwrap()), ["b.mp4"]);
        assert!(db.segments_overlapping("cam", 2000, 5000).unwrap().is_empty());
        assert_eq!(paths(db.segments_overlapping("cam", 0, 10_000).unwrap()), ["a.mp4", "b.mp4", "c.mp4"]);

        assert_eq!(db.segment_at("cam", 0).unwrap().unwrap().path, "a.mp4");
        assert_eq!(db.segment_at("cam", 999).unwrap().unwrap().path, "a.mp4");
        assert_eq!(db.segment_at("cam", 1000).unwrap().unwrap().path, "b.mp4");
        assert!(db.segment_at("cam", 2000).unwrap().is_none());
        assert!(db.segment_at("cam", -1).unwrap().is_none());
        assert!(db.segment_at("nobody", 10).unwrap().is_none());

        let old = db.oldest_segments(2).unwrap();
        assert_eq!(old.len(), 2);
        assert_eq!(old[0].1.start_ms, 0);
        assert_eq!(db.total_segment_bytes().unwrap(), 400);
        assert_eq!(db.segment_bytes_by_camera().unwrap(), [("cam".to_string(), 300), ("other".to_string(), 100)]);
        assert!(db.delete_segment("a.mp4").unwrap());
        assert!(!db.delete_segment("a.mp4").unwrap());
        assert_eq!(db.total_segment_bytes().unwrap(), 300);
    }

    fn touch(path: &Path, mtime_ms: u64) {
        let f = File::create(path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_millis(mtime_ms)).unwrap();
    }

    #[test]
    fn camera_dir_scan_reads_only_the_cameras_given() {
        let dir = tmp("camscan");
        let base = 1_700_000_000_000u64;
        for cam in ["front", "back"] {
            let segs = dir.join("cameras").join(cam).join("segments");
            std::fs::create_dir_all(&segs).unwrap();
            touch(&segs.join(format!("{cam}-20240101120000-seg000001.mp4")), base);
            touch(&segs.join(format!("{cam}-20240101120000-seg000002.mp4")), base + 60_000);
        }
        let db = Db::open_in_memory().unwrap();
        let dirs = vec![("front".to_string(), String::new(), dir.clone())];
        assert_eq!(db.import_camera_dirs(&dirs, 60).unwrap(), 2);
        assert_eq!(db.segments_overlapping("front", 0, i64::MAX).unwrap().len(), 2);
        assert!(db.segments_overlapping("back", 0, i64::MAX).unwrap().is_empty());
        // Again: nothing new; no cameras: nothing to do.
        assert_eq!(db.import_camera_dirs(&dirs, 60).unwrap(), 0);
        assert_eq!(db.import_camera_dirs(&[], 60).unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recent_camera_dir_scan_adds_only_recent_recordings() {
        let dir = tmp("camscan-recent");
        let base = 1_700_000_000_000u64;
        let segs = dir.join("cameras").join("front").join("segments");
        std::fs::create_dir_all(&segs).unwrap();
        touch(&segs.join("front-20240101120000-seg000001.mp4"), base);
        touch(&segs.join("front-20240101120000-seg000002.mp4"), base + 60_000);
        touch(&segs.join("front-20240101120000-seg000003.mp4"), base + 120_000);
        let db = Db::open_in_memory().unwrap();
        let dirs = vec![("front".to_string(), String::new(), dir.clone())];
        // Only the last one ended at or after `since`; it keeps the start
        // time chained from the file before it.
        assert_eq!(db.import_recent_camera_dirs(&dirs, 60, base as i64 + 120_000).unwrap(), 1);
        let rows = db.segments_overlapping("front", 0, i64::MAX).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].start_ms, base as i64 + 60_000);
        // Already indexed ones are left alone.
        assert_eq!(db.import_recent_camera_dirs(&dirs, 60, 0).unwrap(), 2);
        assert_eq!(db.import_recent_camera_dirs(&dirs, 60, 0).unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn fake_found(n: usize) -> Vec<FoundSegment> {
        (0..n)
            .map(|i| FoundSegment {
                camera: "front".into(),
                path: format!("cameras/front/segments/front-20240101120000-seg{i:06}.mp4"),
                start_ms: i as i64 * 60_000,
                end_ms: i as i64 * 60_000 + 60_000,
                bytes: 1000,
                folder: String::new(),
                kind: "legacy",
            })
            .collect()
    }

    #[test]
    fn found_recordings_go_in_small_batches_and_release_the_database_between() {
        let db = Db::open_in_memory().unwrap();
        let found = fake_found(2500);
        let mut batches = 0;
        let n = db
            .insert_found_in_batches(&found, 500, Duration::from_secs(60), &mut || {
                batches += 1;
                // Would hang if the database were still held.
                let have = db.segments_overlapping("front", 0, i64::MAX).unwrap().len();
                assert_eq!(have, batches * 500);
                true
            })
            .unwrap();
        assert_eq!(n, 2500);
        assert_eq!(batches, 4); // between the 5 batches
        // Re-running changes nothing.
        assert_eq!(db.insert_found_segments(&found).unwrap(), 0);
        assert_eq!(db.segments_overlapping("front", 0, i64::MAX).unwrap().len(), 2500);
    }

    #[test]
    fn found_recordings_stopped_half_way_are_finished_by_the_next_run() {
        let db = Db::open_in_memory().unwrap();
        let found = fake_found(1200);
        // "Crash" after two batches: those are committed, nothing else.
        let mut k = 0;
        let n = db
            .insert_found_in_batches(&found, 500, Duration::from_secs(60), &mut || {
                k += 1;
                k < 2
            })
            .unwrap();
        assert_eq!(n, 1000);
        assert_eq!(db.segments_overlapping("front", 0, i64::MAX).unwrap().len(), 1000);
        // The next run adds exactly the rest, once.
        assert_eq!(db.insert_found_segments(&found).unwrap(), 200);
        assert_eq!(db.insert_found_segments(&found).unwrap(), 0);
        assert_eq!(db.segments_overlapping("front", 0, i64::MAX).unwrap().len(), 1200);
    }

    #[test]
    fn a_slow_batch_is_cut_short_by_time() {
        let db = Db::open_in_memory().unwrap();
        let found = fake_found(300);
        let mut batches = 0;
        // A zero time limit still adds a row per batch, so it always moves on.
        let n = db.insert_found_in_batches(&found, 500, Duration::ZERO, &mut || { batches += 1; true }).unwrap();
        assert_eq!(n, 300);
        assert!(batches > 1);
    }

    #[test]
    fn startup_import_flag_follows_the_guard() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        assert!(!db.importing());
        let g = db.begin_startup_import();
        assert!(db.importing());
        drop(g);
        assert!(!db.importing());
    }

    /// Scale check, run by hand: SCALE_ROWS=600000 cargo test --release scale_insert -- --ignored --nocapture
    #[test]
    #[ignore]
    fn scale_insert() {
        let rows: usize = std::env::var("SCALE_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(600_000);
        let dir = tmp("scale");
        let db = Arc::new(Db::open(&dir.join("t.db")).unwrap());
        let found = fake_found(rows);
        let worst = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (d2, w2, s2) = (db.clone(), worst.clone(), stop.clone());
        let probe = std::thread::spawn(move || {
            while !s2.load(std::sync::atomic::Ordering::Relaxed) {
                let t = std::time::Instant::now();
                let _ = d2.oldest_segment_start("front");
                w2.fetch_max(t.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let single = std::env::var("SCALE_SINGLE").is_ok(); // the old one-transaction way
        let add = |db: &Db| {
            if single {
                db.insert_found_in_batches(&found, usize::MAX, Duration::from_secs(86400), &mut || true).unwrap()
            } else {
                db.insert_found_segments(&found).unwrap()
            }
        };
        let t = std::time::Instant::now();
        let n = add(&db);
        let took = t.elapsed();
        std::thread::sleep(Duration::from_millis(1500));
        println!("added {n} in {took:?}; worst wait for the database {} ms", worst.swap(0, std::sync::atomic::Ordering::Relaxed) / 1000);
        let t = std::time::Instant::now();
        let n = add(&db);
        let took = t.elapsed();
        std::thread::sleep(Duration::from_millis(1500));
        println!("again: added {n} in {took:?}; worst wait {} ms", worst.load(std::sync::atomic::Ordering::Relaxed) / 1000);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        probe.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_import() {
        let dir = tmp("legacy");
        let good = serde_json::to_string(&ev("x1", "Front", 5, 9, &["person"])).unwrap();
        let good2 = serde_json::to_string(&ev("x2", "Front", 15, 19, &[])).unwrap();
        std::fs::write(dir.join("events.jsonl"), format!("{good}\nnot json at all\n\n{good2}\n")).unwrap();

        let segs = dir.join("cameras").join("front").join("segments");
        std::fs::create_dir_all(&segs).unwrap();
        let base = 1_700_000_000_000u64;
        // run A: 1 and 2 contiguous (60 s apart), 3 after a long gap
        touch(&segs.join("front-20240101120000-seg000001.mp4"), base);
        touch(&segs.join("front-20240101120000-seg000002.mp4"), base + 60_000);
        touch(&segs.join("front-20240101120000-seg000003.mp4"), base + 1_000_000);
        // run B (-rec), single file
        touch(&segs.join("front-20240101130000-rec-seg000001.mp4"), base + 2_000_000);
        touch(&segs.join("notes.txt"), base);

        let db = Db::open_in_memory().unwrap();
        let r = db.import_legacy(&dir, &[], 60).unwrap();
        assert_eq!(r, ImportReport { events_imported: 2, events_skipped: 1, segments_imported: 4 });
        assert!(!dir.join("events.jsonl").exists());
        assert!(dir.join("events.jsonl.imported").exists());
        assert_eq!(db.total_events().unwrap(), 2);

        let all = db.segments_overlapping("front", 0, i64::MAX).unwrap();
        assert_eq!(all.len(), 4);
        assert!(all.iter().all(|s| s.kind == "legacy"));
        // Stored relative to the data folder with '/' separators.
        assert!(all.iter().all(|s| s.path.starts_with("cameras/front/segments/")), "{:?}", all[0].path);
        let b = base as i64;
        let by_end = |e: i64| all.iter().find(|s| s.end_ms == e).unwrap();
        assert_eq!(by_end(b).start_ms, b - 60_000); // no predecessor -> fallback
        assert_eq!(by_end(b + 60_000).start_ms, b); // chained to previous end
        assert_eq!(by_end(b + 1_000_000).start_ms, b + 1_000_000 - 60_000); // gap too big -> fallback
        assert_eq!(by_end(b + 2_000_000).start_ms, b + 2_000_000 - 60_000); // separate run

        // idempotent
        let again = db.import_legacy(&dir, &[], 60).unwrap();
        assert_eq!(again, ImportReport::default());
        assert_eq!(db.segments_overlapping("front", 0, i64::MAX).unwrap().len(), 4);
        assert_eq!(db.total_events().unwrap(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn segments_page_newest_first_across_cameras() {
        let db = Db::open_in_memory().unwrap();
        let row = |p: &str, a: i64| SegmentRow { path: p.into(), start_ms: a, end_ms: a + 10, bytes: 1, video_frames: 1, kind: "continuous".into(), folder: String::new() };
        db.insert_segment("a", &row("a1", 100)).unwrap();
        db.insert_segment("b", &row("b1", 100)).unwrap();
        db.insert_segment("a", &row("a2", 200)).unwrap();
        db.insert_segment("b", &row("b0", 50)).unwrap();
        let mut seen = Vec::new();
        let (mut ms, mut id) = (i64::MAX, i64::MAX);
        loop {
            let page = db.segments_before(ms, id, 2).unwrap();
            if page.is_empty() {
                break;
            }
            for (i, r) in page {
                (ms, id) = (r.start_ms, i);
                seen.push(r.path);
            }
        }
        // Equal start times both come, once each.
        assert_eq!(seen, vec!["a2", "b1", "a1", "b0"]);
    }

    #[test]
    fn ui_kv_roundtrip_and_scopes() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.ui_get("bob", "prefs").unwrap(), None);
        db.ui_put("bob", "prefs", "{\"a\":1}").unwrap();
        db.ui_put("bob", "prefs", "{\"a\":2}").unwrap();
        db.ui_put("alice", "prefs", "{}").unwrap();
        assert_eq!(db.ui_get("bob", "prefs").unwrap().as_deref(), Some("{\"a\":2}"));
        db.ui_put("orders", "all", "[\"x\"]").unwrap();
        db.ui_put("orders", "group:G", "[\"y\"]").unwrap();
        assert_eq!(db.ui_scope("orders").unwrap().len(), 2);
        assert_eq!(db.ui_scope("nobody").unwrap().len(), 0);
    }

    #[test]
    fn faces_are_named_listed_and_forgotten() {
        let dir = tmp("faces");
        let db = Db::open(&dir.join("t.db")).unwrap();
        let e = crate::faces::emb_to_bytes(&[0.5, 0.5]);
        let f1 = db.insert_face("porch-1", "porch", 1000, 0.9, 80, &e, b"jpg", None).unwrap();
        let f2 = db.insert_face("porch-2", "porch", 2000, 0.9, 60, &e, b"jpg", None).unwrap();
        assert_eq!(db.list_faces(None, None, true, 10).unwrap().len(), 2);
        let alex = db.person_id("Alex", 5).unwrap();
        assert_eq!(db.person_id("Alex", 6).unwrap(), alex, "same name, same person");
        db.label_faces(&[f1], Some(alex)).unwrap();
        assert_eq!(db.labeled_faces().unwrap().len(), 1);
        assert_eq!(db.list_faces(None, None, true, 10).unwrap().len(), 1, "one left unnamed");
        let p = db.people().unwrap();
        assert_eq!((p[0]["name"].as_str(), p[0]["named"].as_i64(), p[0]["face"].as_i64()), (Some("Alex"), Some(1), Some(f1)));
        assert_eq!(db.list_faces(Some("porch-1"), None, false, 10).unwrap()[0]["person"], "Alex");
        db.update_person(alex, None, Some(true)).unwrap();
        assert_eq!(db.quiet_people().unwrap(), vec![alex]);
        assert_eq!(db.face_img(f2).unwrap().unwrap().1, b"jpg");
        // Unnamed faces age out; named ones stay.
        assert_eq!(db.prune_faces(5000).unwrap(), 1);
        assert!(db.delete_person(alex).unwrap());
        assert!(db.labeled_faces().unwrap().is_empty());
        assert!(db.list_faces(None, None, false, 10).unwrap().is_empty(), "forgetting deletes their faces");
        // An event's unnamed faces go with the event.
        let f3 = db.insert_face("porch-3", "porch", 3000, 0.9, 60, &e, b"jpg", None).unwrap();
        db.delete_events(&["porch-3".to_string()]).unwrap();
        assert!(db.face_img(f3).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signals_record_changes_and_give_on_spans() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.set_signal("Front door", 1000, true).unwrap());
        assert!(!db.set_signal("Front door", 1500, true).unwrap(), "already on");
        assert!(db.set_signal("Front door", 3000, false).unwrap());
        assert!(db.set_signal("Front door", 8000, true).unwrap());
        db.set_signal("Alarm", 500, false).unwrap();
        let s = db.signal_spans(2000, 10_000, 9000).unwrap();
        assert_eq!(s.len(), 2);
        let door = s.iter().find(|x| x.0 == "Front door").unwrap();
        assert!(door.1, "on now");
        assert_eq!(door.2, vec![(2000, 3000), (8000, 9000)], "on at the start of the window; still on runs to now");
        assert!(s.iter().find(|x| x.0 == "Alarm").unwrap().2.is_empty());
        // Old changes go, the latest of each stays.
        assert_eq!(db.prune_signals(5000).unwrap(), 2);
        assert_eq!(db.signal_spans(0, 10_000, 9000).unwrap().iter().find(|x| x.0 == "Front door").unwrap().2, vec![(8000, 9000)]);
        assert!(db.forget_signal("Alarm").unwrap());
    }

    #[test]
    fn bit_rate_adds_main_and_sub_streams() {
        let db = Db::open_in_memory().unwrap();
        let mut main = seg("m.mp4", 0, 10_000);
        main.bytes = 2_500_000; // 2000 kbit/s over 10 s
        let mut sub = seg("cam-sub-0-seg000000.mp4", 0, 10_000);
        sub.bytes = 250_000; // 200 kbit/s
        sub.kind = "sub".into();
        db.insert_segment("cam", &main).unwrap();
        db.insert_segment("cam", &sub).unwrap();
        let k = db.camera_kbps(0).unwrap();
        assert_eq!(k.len(), 1);
        assert!((k[0].1 - 2200.0).abs() < 1.0, "{}", k[0].1);
    }

    #[test]
    fn only_clear_named_faces_recognize_unless_someone_has_none() {
        let db = Db::open_in_memory().unwrap();
        let e = |v: f32| crate::faces::emb_to_bytes(&[v, 1.0]);
        let alex = db.person_id("Alex", 1).unwrap();
        let sam = db.person_id("Sam", 1).unwrap();
        // Alex: one clear face, one small and one the detector doubted.
        let clear = db.insert_face("a", "c", 1, 0.92, 120, &e(1.0), b"x", None).unwrap();
        let small = db.insert_face("a", "c", 2, 0.92, 40, &e(2.0), b"x", None).unwrap();
        let doubt = db.insert_face("a", "c", 3, 0.70, 120, &e(3.0), b"x", None).unwrap();
        db.label_faces(&[clear, small, doubt], Some(alex)).unwrap();
        // Sam: only unclear faces; the better one is used.
        let s1 = db.insert_face("b", "c", 4, 0.75, 50, &e(4.0), b"x", None).unwrap();
        let s2 = db.insert_face("b", "c", 5, 0.75, 30, &e(5.0), b"x", None).unwrap();
        db.label_faces(&[s1, s2], Some(sam)).unwrap();
        let refs = db.labeled_faces().unwrap();
        assert_eq!(refs, vec![(alex, e(1.0)), (sam, e(4.0))]);
        let listed = db.list_faces(None, Some(alex), false, 10).unwrap();
        assert_eq!(listed.iter().filter(|f| f["clear"] == true).count(), 1);
    }

    #[test]
    fn event_footage_is_marked_either_way_round_and_purged_last() {
        let db = Db::open_in_memory().unwrap();
        // Recorded before the event is saved...
        for (p, a, b) in [("q1", 0, 10_000), ("e1", 10_000, 20_000), ("q2", 20_000, 30_000)] {
            db.insert_segment("front", &seg(p, a, b)).unwrap();
        }
        db.insert_event(&ev("x", "Front", 12, 14, &[])).unwrap();
        db.mark_event_footage("x", "front", 11_000, 15_000).unwrap();
        // ...and post-roll closing after it.
        db.insert_segment("front", &seg("e2", 14_000, 16_000)).unwrap();
        // Another camera at the same time is not event footage.
        db.insert_segment("back", &seg("b1", 12_000, 13_000)).unwrap();
        let order = |kept_from: i64| db.segments_to_purge("", kept_from, 10).unwrap().into_iter().map(|(s, k)| format!("{}{}", s.path, if k { "*" } else { "" })).collect::<Vec<_>>();
        assert_eq!(order(0), ["q1", "b1", "q2", "e1*", "e2*"]);
        // Event footage older than the keep window is ordinary again.
        assert_eq!(order(i64::MAX), ["q1", "e1", "b1", "e2", "q2"]);
        assert_eq!(db.segments_to_purge("", 0, 2).unwrap().len(), 2);
        assert!(db.events_without_span(10).unwrap().is_empty());
        db.insert_event(&ev("y", "Front", 50, 60, &[])).unwrap();
        assert_eq!(db.events_without_span(10).unwrap().len(), 1);
    }
    #[test]
    fn smart_search_pictures_follow_a_camera_rename() {
        let db = Db::open_in_memory().unwrap();
        db.smart_store("e1", &[("Old Name".into(), 5, "x.jpg".into(), None, None, vec![0; 8])]).unwrap();
        db.rename_camera("Old Name", "New Name").unwrap();
        let mut cams = Vec::new();
        db.smart_scan(0, 10, &mut |_, _, c, _, _, _, _| cams.push(c.to_string())).unwrap();
        assert_eq!(cams, vec!["New Name"]);
    }

    #[test]
    fn signal_names_are_capped() {
        let db = Db::open_in_memory().unwrap();
        for n in 0..MAX_SIGNALS {
            assert!(db.set_signal(&format!("s{n}"), 1, true).unwrap());
        }
        assert!(db.set_signal("one too many", 1, true).is_err());
        // Known names still change.
        assert!(db.set_signal("s0", 2, false).unwrap());
    }

    fn stored(hash: &str, user: &str) -> StoredSession {
        StoredSession { hash: hash.into(), user: user.into(), admin: false, expires: 4_000_000_000, created: 100, ip: "192.0.2.1".into(), label: "Edge on Windows".into(), device: "dev".into(), last_seen: 200 }
    }

    #[test]
    fn migrates_the_previous_schema_keeping_sessions_and_old_builds_still_write() {
        let dir = tmp("v22-migrate");
        let path = dir.join("ambersight.db");
        drop(Db::open(&path).unwrap());
        {
            // A database as schema version 22 left it.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "DROP TABLE known_devices; DROP TABLE sessions;
                 CREATE TABLE sessions (hash TEXT PRIMARY KEY, user TEXT NOT NULL, admin INTEGER NOT NULL, expires INTEGER NOT NULL);
                 INSERT INTO sessions (hash, user, admin, expires) VALUES ('old', 'ann', 1, 4000000000);
                 PRAGMA user_version = 22;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let v: i64 = db.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // Nobody is signed out; the old session has no details.
        let rows = db.load_sessions().unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!((r.hash.as_str(), r.user.as_str(), r.admin, r.expires, r.created, r.ip.as_str(), r.label.as_str(), r.device.as_str(), r.last_seen), ("old", "ann", true, 4_000_000_000, 0, "", "", "", 0));
        assert_eq!(db.device_seen("ann", "h", "x", 5, 50).unwrap().had_any, false);
        // An older build writes sessions naming only the first four columns: that still works.
        db.lock().execute("INSERT OR REPLACE INTO sessions (hash, user, admin, expires) VALUES ('older-build', 'bob', 0, 4000000000)", []).unwrap();
        db.lock().execute("INSERT OR REPLACE INTO sessions (hash, user, admin, expires) VALUES ('old', 'ann', 1, 4000000001)", []).unwrap();
        assert_eq!(db.load_sessions().unwrap().len(), 2);
        drop(db);
        // And opening it again changes nothing.
        let db = Db::open(&path).unwrap();
        assert_eq!(db.load_sessions().unwrap().len(), 2);
    }

    #[test]
    fn sessions_keep_their_details_across_a_restart_and_use_is_written_without_making_sessions() {
        let dir = tmp("sessions-restart");
        let path = dir.join("ambersight.db");
        let db = Db::open(&path).unwrap();
        db.save_session(&stored("h1", "ann")).unwrap();
        db.save_session(&stored("h2", "ann")).unwrap();
        db.touch_session("h1", "192.0.2.77", 999).unwrap();
        assert!(!db.delete_session_of("bob", "h2").unwrap(), "only the account's own");
        assert!(db.delete_session_of("ann", "h2").unwrap());
        assert!(!db.delete_session_of("ann", "h2").unwrap(), "once");
        // (Use noted after the session ended brings nothing back.)
        db.touch_session("h2", "192.0.2.78", 1000).unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        let rows = db.load_sessions().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].hash.as_str(), rows[0].ip.as_str(), rows[0].last_seen, rows[0].label.as_str(), rows[0].created), ("h1", "192.0.2.77", 999, "Edge on Windows", 100));
    }

    #[test]
    fn known_devices_follow_the_account() {
        let db = Db::open_in_memory().unwrap();
        let first = db.device_seen("ann", "a", "Edge on Windows", 1, 3).unwrap();
        assert!(!first.known && !first.had_any);
        let again = db.device_seen("ann", "a", "Edge on Windows", 2, 3).unwrap();
        assert!(again.known && again.had_any);
        assert!(!db.device_seen("bob", "a", "x", 3, 3).unwrap().known, "per account");
        for (i, h) in ["b", "c", "d"].iter().enumerate() {
            db.device_seen("ann", h, "x", 10 + i as i64, 3).unwrap();
        }
        assert_eq!(db.known_device_count("ann"), 3, "the cap");
        assert!(!db.device_seen("ann", "a", "x", 20, 3).unwrap().known, "the least recently used went");
        // A deleted account starts fresh (a recreated name too).
        db.account_data_delete("ann").unwrap();
        assert_eq!(db.known_device_count("ann"), 0);
        assert_eq!(db.known_device_count("bob"), 1);
        assert!(!db.device_seen("ann", "b", "x", 30, 3).unwrap().had_any);
    }

    /// The running totals, and the same worked out from the recordings.
    #[allow(clippy::type_complexity)]
    fn totals_and_truth(db: &Db) -> (Vec<(String, String, i64, i64)>, Vec<(String, String, i64, i64)>, Vec<(String, i64, i64, i64, i64)>, Vec<(String, i64, i64, i64, i64)>) {
        let c = db.lock();
        let q4 = |sql: &str| -> Vec<(String, String, i64, i64)> {
            let mut st = c.prepare(sql).unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(|r| r.unwrap()).collect()
        };
        let q5 = |sql: &str| -> Vec<(String, i64, i64, i64, i64)> {
            let mut st = c.prepare(sql).unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap().map(|r| r.unwrap()).collect()
        };
        (
            q4("SELECT folder, camera, bytes, n FROM seg_totals ORDER BY folder, camera"),
            q4("SELECT folder, camera, SUM(bytes), COUNT(*) FROM segments GROUP BY folder, camera ORDER BY folder, camera"),
            q5("SELECT camera, day, bytes, ms, n FROM seg_days ORDER BY camera, day"),
            q5("SELECT camera, start_ms / 86400000, SUM(bytes), SUM(MAX(end_ms - start_ms, 0)), COUNT(*) FROM segments GROUP BY camera, start_ms / 86400000 ORDER BY 1, 2"),
        )
    }

    fn assert_totals_true(db: &Db) {
        let (t, want, d, want_d) = totals_and_truth(db);
        assert_eq!(t, want);
        assert_eq!(d, want_d);
    }

    #[test]
    fn storage_totals_follow_every_change_to_the_recordings() {
        let dir = tmp("seg-totals");
        let path = dir.join("ambersight.db");
        let day = 86_400_000;
        let db = Db::open(&path).unwrap();
        assert!(db.segment_totals_ready());
        for i in 0..20 {
            let mut s = seg(&format!("a{i}"), i * day / 4, i * day / 4 + 60_000);
            s.bytes = 10 + i;
            db.insert_segment(if i % 3 == 0 { "cam" } else { "other" }, &s).unwrap();
        }
        assert_totals_true(&db);
        assert!(db.set_segment_folder("a3", "stored").unwrap());
        assert!(db.set_segment_folder("a4", "stored").unwrap());
        assert!(db.delete_segment("a5").unwrap());
        assert_totals_true(&db);
        assert_eq!(db.folder_bytes("stored").unwrap(), 13 + 14);
        let all: i64 = (10..30).sum::<i64>() - 15;
        assert_eq!(db.total_segment_bytes().unwrap(), all);
        assert_eq!(db.counts().unwrap().0, 19);
        db.rename_camera("other", "renamed").unwrap();
        assert_totals_true(&db);
        // The import path (INSERT OR IGNORE, repeated) counts each file once.
        let found: Vec<FoundSegment> = (0..3)
            .map(|i| FoundSegment { camera: "cam".into(), path: format!("f{i}"), start_ms: 50 * day + i, end_ms: 50 * day + i + 10, bytes: 7, folder: "".into(), kind: "continuous" })
            .collect();
        db.insert_found_segments(&found).unwrap();
        db.insert_found_segments(&found).unwrap();
        assert_totals_true(&db);
        // Rates come from the days since the first midnight in the window.
        let r = db.camera_rates(50 * day - 5).unwrap();
        assert_eq!(r, [("cam".to_string(), 21, 50 * day, 30)]);
        assert_eq!(db.camera_rates_scan(50 * day).unwrap(), r);
        let h = db.camera_holdings().unwrap();
        assert!(h.contains(&("cam".to_string(), "stored".to_string(), 13, 0)), "{h:?}");
        drop(db);
        // A program that knows nothing of the totals (an older version) changes the
        // recordings: the triggers in the database keep them right.
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "DELETE FROM segments WHERE path = 'a1'; UPDATE segments SET folder = 'x', bytes = 99 WHERE path = 'a2';
                 INSERT INTO segments (camera, path, start_ms, end_ms, bytes, video_frames, kind, folder) VALUES ('cam', 'z', 5, 6, 1, 0, 'sub', 'x');",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_totals_true(&db);
        // Triggers removed by hand: made again and counted afresh.
        db.lock().execute_batch("DROP TRIGGER seg_totals_upd; UPDATE segments SET bytes = 1;").unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_totals_true(&db);
        assert!(db.segment_totals_ready());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storage_totals_are_filled_in_steps_while_recordings_change() {
        let dir = tmp("seg-totals-fill");
        let path = dir.join("ambersight.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.lock();
            c.execute_batch(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 12000)
                 INSERT INTO segments (camera, path, start_ms, end_ms, bytes, video_frames, kind, folder)
                 SELECT 'c' || (i % 4), 'p' || i, i * 60000, i * 60000 + 59000, i, 0, 'continuous', CASE WHEN i % 2 = 0 THEN '' ELSE 'stored' END FROM n;",
            )
            .unwrap();
            // As an upgrade from v25 leaves it.
            c.execute_batch(
                "DROP TRIGGER seg_totals_ins; DROP TRIGGER seg_totals_del; DROP TRIGGER seg_totals_upd;
                 DROP TABLE seg_totals; DROP TABLE seg_days; DROP TABLE seg_totals_fill; PRAGMA user_version = 25;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        // (A small index: counted at open.)
        assert!(db.segment_totals_ready());
        assert_totals_true(&db);
        // Now as a big index: counted in steps, the old queries meanwhile.
        db.lock().execute_batch("DELETE FROM seg_totals; DELETE FROM seg_days; UPDATE seg_totals_fill SET upto = 0;").unwrap();
        db.totals_ready.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(!db.segment_totals_ready());
        let truth_stored: i64 = (1..=12000).filter(|i| i % 2 == 1).sum();
        assert_eq!(db.folder_bytes("stored").unwrap(), truth_stored);
        assert_eq!(db.fill_segment_totals_step(5000).unwrap(), Some(5000));
        assert!(!db.segment_totals_ready());
        // Changes on both sides of the fill.
        db.lock()
            .execute_batch(
                "DELETE FROM segments WHERE id IN (100, 9000);
                 UPDATE segments SET folder = 'x' WHERE id IN (200, 8000);
                 UPDATE segments SET bytes = bytes + 5, start_ms = start_ms + 86400000 WHERE id IN (300, 11000);
                 UPDATE segments SET camera = 'moved' WHERE id IN (400, 7000);
                 DELETE FROM segments WHERE id = 12000;
                 INSERT INTO segments (camera, path, start_ms, end_ms, bytes, video_frames, kind, folder) VALUES ('c1', 'new1', 1, 2, 3, 0, 'sub', '');",
            )
            .unwrap();
        // (7000 ids left past the first step, two of them deleted, and the new one takes the freed last id.)
        assert_eq!(db.fill_segment_totals().unwrap(), 7000 - 2 + 1);
        assert!(db.segment_totals_ready());
        assert_totals_true(&db);
        assert_eq!(db.folder_bytes("stored").unwrap(), truth_stored);
        db.insert_segment("c1", &seg("after", 0, 1)).unwrap();
        assert!(db.delete_segment("p2").unwrap());
        assert_totals_true(&db);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn smart_search_finds_new_events_without_reading_them_all() {
        let db = Db::open_in_memory().unwrap();
        for i in 0..5 {
            db.insert_event(&ev(&format!("e{i}"), "cam", 1000 + i * 100, 1050 + i * 100, &["person"])).unwrap();
        }
        let ids = |v: Vec<(String, String, i64)>| v.into_iter().map(|r| r.0).collect::<Vec<_>>();
        assert_eq!(ids(db.smart_pending(2).unwrap()), ["e4", "e3"]);
        for i in 0..5 {
            db.smart_store(&format!("e{i}"), &[]).unwrap();
        }
        assert!(db.smart_pending(20).unwrap().is_empty());
        // Every one checked: the next look starts an hour before the newest.
        assert_eq!(db.smart_hint.lock().unwrap().unwrap().0, 1400 - 3600);
        db.insert_event(&ev("new", "cam", 5000, 5010, &[])).unwrap();
        assert_eq!(ids(db.smart_pending(20).unwrap()), ["new"]);
        // An old event added now is found by the hourly full check.
        db.smart_store("new", &[]).unwrap();
        db.insert_event(&ev("old", "cam", -9000, -8000, &[])).unwrap();
        assert!(db.smart_pending(20).unwrap().is_empty());
        {
            let mut h = db.smart_hint.lock().unwrap();
            let (from, _) = h.unwrap();
            *h = Some((from, std::time::Instant::now() - Duration::from_secs(3601)));
        }
        assert_eq!(ids(db.smart_pending(20).unwrap()), ["old"]);
        assert_eq!(db.smart_counts().unwrap(), (6, 1, 0));
        // Starting over forgets it all.
        db.smart_reset().unwrap();
        assert_eq!(db.smart_pending(20).unwrap().len(), 7);
        assert_eq!(db.smart_counts().unwrap(), (0, 7, 0));
    }

    #[test]
    fn an_event_ahead_of_the_clock_does_not_hide_new_ones_from_smart_search() {
        let db = Db::open_in_memory().unwrap();
        let now = chrono::Utc::now().timestamp();
        db.insert_event(&ev("ahead", "cam", now + 86_400, now + 86_410, &[])).unwrap();
        db.smart_store("ahead", &[]).unwrap();
        assert!(db.smart_pending(20).unwrap().is_empty());
        db.insert_event(&ev("now", "cam", now, now + 10, &[])).unwrap();
        let got: Vec<String> = db.smart_pending(20).unwrap().into_iter().map(|r| r.0).collect();
        assert_eq!(got, ["now"]);
    }

    #[test]
    fn several_slow_reads_at_once_leave_a_connection_free() {
        let dir = tmp("ro-pool");
        let db = Db::open(&dir.join("ambersight.db")).unwrap();
        assert_eq!(db.ro.len(), READ_CONNECTIONS);
        db.insert_segment("cam", &seg("a", 0, 1000)).unwrap();
        // Three long reads hold their connections; the next read still gets one at once.
        let held: Vec<_> = (0..READ_CONNECTIONS - 1).map(|_| db.lock_ro()).collect();
        let t = std::time::Instant::now();
        assert_eq!(db.folder_bytes("").unwrap(), 100);
        assert!(t.elapsed() < Duration::from_secs(1));
        drop(held);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_days_marks_recorded_days_and_alerts() {
        let dir = tmp("rec-days");
        let db = Db::open(&dir.join("ambersight.db")).unwrap();
        let day = 86_400_000i64;
        let d0 = 100 * day;
        {
            let c = db.lock();
            // A: recorded on day 1; B: a recording from day 2 into day 3; day 4 nothing; day 5 an alert only.
            c.execute_batch(&format!(
                "INSERT INTO segments (camera, path, start_ms, end_ms) VALUES ('a', 'a1', {a}, {a} + 60000);
                 INSERT INTO segments (camera, path, start_ms, end_ms) VALUES ('b', 'b1', {b}, {b} + 3600000);
                 INSERT INTO events (id, camera_id, camera, start, end, score) VALUES ('e1', 0, 'A', {ea}, {ea} + 5, 0.9);
                 INSERT INTO events (id, camera_id, camera, start, end, score, unconfirmed) VALUES ('e2', 0, 'A', {ea} + 9, {ea} + 12, 0.9, 1);
                 INSERT INTO events (id, camera_id, camera, start, end, score) VALUES ('e3', 0, 'B', {eb}, {eb} + 5, 0.9);",
                a = d0 + day + 1000,
                b = d0 + 2 * day + day - 1_800_000,
                ea = (d0 + day + 5000) / 1000,
                eb = (d0 + 4 * day + 5000) / 1000,
            ))
            .unwrap();
        }
        let starts: Vec<i64> = (0..6).map(|i| d0 + i * day).collect();
        let cam = |n: &str| (n.to_lowercase(), n.to_string());
        let both = vec![cam("A"), cam("B")];
        let (days, oldest) = db.recording_days(&starts, d0 + 6 * day, &both, &[]).unwrap();
        let rec: Vec<bool> = days.iter().map(|d| d.0).collect();
        assert_eq!(rec, [false, true, true, true, false, false]);
        let alerts: Vec<i64> = days.iter().map(|d| d.1).collect();
        assert_eq!(alerts, [0, 2, 0, 0, 1, 0]);
        assert_eq!(oldest, Some(d0 + day + 1000));
        // Only B; an unconfirmed alert left out for A.
        let (days, _) = db.recording_days(&starts, d0 + 6 * day, &[cam("B")], &[]).unwrap();
        assert_eq!(days.iter().map(|d| d.0).collect::<Vec<_>>(), [false, false, true, true, false, false]);
        let (days, _) = db.recording_days(&starts, d0 + 6 * day, &[cam("A")], &["A".to_string()]).unwrap();
        assert_eq!(days[1], (true, 1));
        assert_eq!(db.recording_days(&starts, d0 + 6 * day, &[], &[]).unwrap().1, None);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sixty-two days of a calendar on a site's worth of recordings (1 M, 12 cameras, 93 k alerts) is a
    /// few milliseconds. Run with
    /// `cargo test --release --bin ambersight -- --ignored big_recording_days --nocapture`.
    #[test]
    #[ignore]
    fn big_recording_days_are_quick() {
        let dir = tmp("big-days");
        let path = dir.join("ambersight.db");
        let day = 86_400_000i64;
        let now = 400 * day;
        let s0 = now - 40 * day;
        {
            let db = Db::open(&path).unwrap();
            db.lock()
                .execute_batch(&format!(
                    "BEGIN;
                     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000000)
                     INSERT INTO segments (camera, path, start_ms, end_ms, bytes, video_frames, kind)
                     SELECT 'cam-' || (i % 12), 'p' || i, {s0} + i * 3456, {s0} + i * 3456 + 60000, 1, 0, 'continuous' FROM n;
                     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 93000)
                     INSERT INTO events (id, camera_id, camera, start, end, score, objects)
                     SELECT 'ev' || i, 0, 'Cam ' || (i % 12), ({s0} + i * 37000) / 1000, ({s0} + i * 37000) / 1000 + 30, 0.9, '[]' FROM n;
                     COMMIT; ANALYZE;"
                ))
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let cams: Vec<(String, String)> = (0..12).map(|i| (format!("cam-{i}"), format!("Cam {i}"))).collect();
        let starts: Vec<i64> = (0..62).map(|i| now - 61 * day + i * day).collect();
        let run = || db.recording_days(&starts, now + day, &cams, &[]).unwrap();
        run();
        let t = std::time::Instant::now();
        let (days, oldest) = run();
        let us = t.elapsed().as_micros();
        eprintln!("62 days, 12 cameras: {:.1} ms", us as f64 / 1000.0);
        assert!(days.iter().filter(|d| d.0).count() >= 39);
        assert!(oldest.is_some());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(us < 50_000, "took {us} us");
    }

    /// A site's worth of recordings (1 M) in a file: the Status and Storage
    /// figures and an idle smart search pass take milliseconds. Run with
    /// `cargo test --release --bin ambersight -- --ignored big_index --nocapture`.
    #[test]
    #[ignore]
    fn big_index_figures_are_quick() {
        let dir = tmp("big-index");
        let path = dir.join("ambersight.db");
        let day = 86_400_000i64;
        let now = 400 * day;
        {
            let db = Db::open(&path).unwrap();
            let c = db.lock();
            let t = std::time::Instant::now();
            // 1 M recordings over 40 days of 12 cameras, 40% in the main folder and 60% in the
            // archive; 93 k events, all looked at by smart search.
            c.execute_batch(&format!(
                "BEGIN;
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000000)
                 INSERT INTO segments (camera, path, start_ms, end_ms, bytes, video_frames, kind, folder)
                 SELECT 'cam' || (i % 12), 'cameras/cam' || (i % 12) || '/segments/' || i || '.mp4', {s0} + i * 3456, {s0} + i * 3456 + 60000,
                        1500000 + i % 1000, 0, CASE WHEN i % 5 = 0 THEN 'sub' ELSE 'continuous' END, CASE WHEN i > 600000 THEN 'new' ELSE 'stored' END FROM n;
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 93000)
                 INSERT INTO events (id, camera_id, camera, start, end, score, objects)
                 SELECT 'ev' || i, 0, 'Cam ' || (i % 12), ({s0} + i * 37000) / 1000, ({s0} + i * 37000) / 1000 + 30, 0.9, '[\"person\"]' FROM n;
                 INSERT INTO smart_done (event_id) SELECT id FROM events;
                 COMMIT;",
                s0 = now - 40 * day
            ))
            .unwrap();
            eprintln!("built 1 M recordings and 93 k events in {} ms", t.elapsed().as_millis());
            c.execute_batch("ANALYZE;").unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(db.segment_totals_ready());
        let time = |what: &str, f: &mut dyn FnMut()| -> u128 {
            f(); // (the first pass reads the pages in)
            let t = std::time::Instant::now();
            f();
            let us = t.elapsed().as_micros();
            eprintln!("{what}: {:.1} ms", us as f64 / 1000.0);
            us
        };
        let ro = |sql: &str| {
            let c = db.lock_ro();
            let mut st = c.prepare(sql).unwrap();
            let mut rows = st.query([]).unwrap();
            while rows.next().unwrap().is_some() {}
        };
        eprintln!("-- before (the old queries)");
        time("folder_bytes", &mut || ro("SELECT COALESCE(SUM(bytes), 0) FROM segments WHERE folder = 'new'"));
        time("counts", &mut || ro("SELECT COUNT(*) FROM segments"));
        time("camera_rates", &mut || db.camera_rates_scan(now - 7 * day).map(|_| ()).unwrap());
        time("camera_holdings", &mut || ro("SELECT camera, folder, COALESCE(SUM(bytes), 0), MIN(start_ms) FROM segments GROUP BY camera, folder"));
        time("smart_pending (none waiting)", &mut || {
            ro("SELECT e.id FROM events e WHERE NOT EXISTS (SELECT 1 FROM smart_done d WHERE d.event_id = e.id) ORDER BY e.start DESC LIMIT 20")
        });
        eprintln!("-- after");
        let mut worst = 0;
        worst = worst.max(time("folder_bytes", &mut || assert!(db.folder_bytes("new").unwrap() > 0)));
        worst = worst.max(time("counts", &mut || assert_eq!(db.counts().unwrap(), (1_000_000, 93_000))));
        worst = worst.max(time("camera_rates", &mut || assert_eq!(db.camera_rates(now - 7 * day).unwrap().len(), 12)));
        worst = worst.max(time("camera_holdings", &mut || assert_eq!(db.camera_holdings().unwrap().len(), 24)));
        worst = worst.max(time("smart_pending (none waiting)", &mut || assert!(db.smart_pending(20).unwrap().is_empty())));
        *db.smart_counts_cache.lock().unwrap() = None;
        eprintln!("smart_counts is counted at most once a minute");
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(worst < 50_000, "slowest took {worst} us");
    }
}
