//! The saved layout of each recording (its header and where each fragment
//! is), in its own small database next to the main one
//! (`<data_dir>/layouts.db`). Made when a recording is closed, while the file
//! is still in memory, so starting playback later does not have to read the
//! fragment headers spread through each file on a busy hard disk (a seek per
//! fragment). It is only a cache: a missing or damaged one is made again, an
//! entry whose file changed (size or time) is ignored, and it is not backed up.

use super::{FragmentInfo, ParsedSegment, TrackMeta, TrafInfo};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::collections::HashSet;
use std::sync::{mpsc, Mutex, OnceLock};

/// Bumped when the stored layout changes: older entries are dropped.
const VERSION: i64 = 2;

/// How each entry's layout was read (`layouts.fmt`), so a reading fix
/// reaches what was saved before it without dropping the whole index.
/// 1: before fragment and track default sample durations and flags
/// (tfhd/trex) were read: ffmpeg recordings of a steady frame rate were
/// saved as lasting no time, some keyframes as not keyframes.
///
/// The fix changes nothing for the built-in recorder's files (every sample
/// gives its own duration, at least 1, and flags): their headers are marked
/// once, in the small `inits` table (`inits.ok1`, see [`by_our_recorder`]),
/// and an entry of format 1 with such a header counts as current, without
/// rewriting the entry. An entry of format 1 with another header (ffmpeg's,
/// or one that cannot be told) counts as missing everywhere: it is read from
/// its file again when played, and the background pass reads the rest again
/// once ([`upgrade_plan`], [`upgrade_chunk`]).
pub const FORMAT: i64 = 2;

/// Whether a file header (`ftyp` + `moov`) is the built-in recorder's
/// (ingest/fmp4.rs `init_segment`): its exact `ftyp`, a `moov` of only
/// `mvhd`, `trak`s and `mvex`, and every track's defaults (`trex`) zero.
/// ffmpeg's carries a `udta` (its name and version) and other boxes; a
/// header that cannot be read is not taken as ours.
fn by_our_recorder(init: &[u8]) -> bool {
    let Ok(top) = super::children(init, 0, init.len()) else { return false };
    // ftyp: major "iso6", minor 0, brands iso6 mp41 isom dash.
    const FTYP: &[u8] = b"\0\0\0\x20ftypiso6\0\0\0\0iso6mp41isomdash";
    if !matches!(top.first(), Some(&(k, s, e)) if &k == b"ftyp" && init[s..e] == *FTYP) {
        return false;
    }
    let Some(&(_, ms, me)) = top.iter().find(|(k, _, _)| k == b"moov") else { return false };
    if top.len() != 2 {
        return false;
    }
    let moov = &init[ms..me];
    let Ok(kids) = super::children(moov, 8, moov.len()) else { return false };
    if !kids.iter().all(|(k, _, _)| matches!(k, b"mvhd" | b"trak" | b"mvex")) || !kids.iter().any(|(k, _, _)| k == b"trak") {
        return false;
    }
    let Ok(Some((xs, xe))) = super::find(moov, 8, moov.len(), b"mvex") else { return false };
    let Ok(boxes) = super::children(moov, xs + 8, xe) else { return false };
    let traks = kids.iter().filter(|(k, _, _)| k == b"trak").count();
    boxes.len() == traks
        && boxes.iter().all(|&(k, s, e)| {
            // trex: header(8) version/flags(4) track id(4) description(4) duration(4) size(4) flags(4).
            &k == b"trex" && e - s == 32 && moov[s + 20..e].iter().all(|b| *b == 0)
        })
}

static DB: OnceLock<Mutex<Connection>> = OnceLock::new();
/// A second, read-only connection to the same file (WAL: it reads while the
/// first writes) for the long scan of the format upgrade, so playback and
/// indexing never wait for it behind [`DB`]'s lock.
static RO: OnceLock<Mutex<Connection>> = OnceLock::new();

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v != VERSION {
        c.execute_batch("DROP TABLE IF EXISTS layouts; DROP TABLE IF EXISTS inits; DROP TABLE IF EXISTS meta;")?;
    }
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS inits (id INTEGER PRIMARY KEY, bytes BLOB NOT NULL UNIQUE, ok1 INTEGER);
         CREATE TABLE IF NOT EXISTS layouts (
             path TEXT PRIMARY KEY, len INTEGER NOT NULL, mtime INTEGER NOT NULL,
             init INTEGER NOT NULL, data BLOB NOT NULL, fmt INTEGER NOT NULL DEFAULT 1) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;",
    )?;
    // Entries saved before formats were kept are of the first one. (Adding
    // a column rewrites nothing: the rows that lack it read the default.)
    if c.prepare("SELECT fmt FROM layouts LIMIT 0").is_err() {
        c.execute_batch("ALTER TABLE layouts ADD COLUMN fmt INTEGER NOT NULL DEFAULT 1;")?;
    }
    // (Neither failing is a reason to make the index anew, hours of
    // reading: a header not marked counts as another's, its entries of the
    // first format are read again as they are played.)
    if c.prepare("SELECT ok1 FROM inits LIMIT 0").is_err() {
        if let Err(e) = c.execute_batch("ALTER TABLE inits ADD COLUMN ok1 INTEGER;") {
            tracing::warn!("recording index: could not add the header marks: {e}");
        }
    }
    if let Err(e) = mark_inits(&c) {
        tracing::warn!("recording index: could not mark the recording headers: {e}");
    }
    c.pragma_update(None, "user_version", VERSION)?;
    Ok(c)
}

/// Note for each header not looked at yet whether it is the built-in
/// recorder's (`inits.ok1`): a handful of small rows, not the layouts. A
/// header that is not, new since the last start (an older version ran
/// meanwhile and saved entries of the first format with it), means the
/// upgrade pass looks again ([`FMT_DONE`] is cleared).
fn mark_inits(c: &Connection) -> rusqlite::Result<()> {
    let todo: Vec<(i64, Vec<u8>)> = {
        let mut st = c.prepare("SELECT id, bytes FROM inits WHERE ok1 IS NULL")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut foreign = false;
    for (id, bytes) in todo {
        let ours = by_our_recorder(&bytes);
        foreign |= !ours;
        c.execute("UPDATE inits SET ok1 = ?2 WHERE id = ?1", params![id, ours as i64])?;
    }
    if foreign {
        c.execute("DELETE FROM meta WHERE key = ?1", params![FMT_DONE])?;
    }
    Ok(())
}

/// Use (and make if needed) the layout index at `path`. Without this call
/// (tests, command-line tools) layouts are only kept in memory.
pub fn open_index(path: &Path) {
    let conn = match open(path) {
        Ok(c) => c,
        Err(e) => {
            // A damaged cache: start a new one.
            tracing::warn!("recording index {}: {e}; making a new one", path.display());
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
            }
            match open(path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("recording index unavailable: {e}");
                    return;
                }
            }
        }
    };
    if DB.set(Mutex::new(conn)).is_ok() {
        if let Ok(ro) = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX) {
            let _ = ro.busy_timeout(std::time::Duration::from_secs(5));
            let _ = RO.set(Mutex::new(ro));
        }
        // Entries of files removed some way we did not see (by hand, a
        // drive cleaned up): dropped once a week. Each sweep looks at every
        // file, which on a big spinning drive with a cold cache takes hours of
        // random reads, so it is not repeated at every start.
        std::thread::Builder::new()
            .name("layout-sweep".into())
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(600));
                loop {
                    let now = chrono::Utc::now().timestamp();
                    if sweep_due(meta_get(SWEPT_AT), now) {
                        sweep();
                        meta_set(SWEPT_AT, chrono::Utc::now().timestamp());
                    }
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            })
            .ok();
    }
}

/// One key per file however its path was spelled (Windows paths joined
/// from a folder and a relative path mix both slashes).
fn key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn mtime_ns(t: Option<std::time::SystemTime>) -> Option<i64> {
    let d = t?.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(d.as_nanos()).ok()
}

pub(super) fn load(path: &Path, stamp: (u64, Option<std::time::SystemTime>)) -> Option<ParsedSegment> {
    let db = DB.get()?;
    let mt = mtime_ns(stamp.1)?;
    let row: Option<(i64, i64, Vec<u8>, Vec<u8>, i64, Option<i64>)> = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT l.len, l.mtime, l.data, i.bytes, l.fmt, i.ok1 FROM layouts l JOIN inits i ON i.id = l.init WHERE l.path = ?1",
            params![key(path)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()
        .ok()
        .flatten();
    let (len, mtime, data, init, fmt, ok1) = row?;
    if len as u64 != stamp.0 || mtime != mt || !current(fmt, ok1) {
        return None;
    }
    decode(&data, init)
}

/// Whether an entry of format `fmt` whose header is marked `ok1` reads as
/// the current reader would read its file (see [`FORMAT`]).
fn current(fmt: i64, ok1: Option<i64>) -> bool {
    fmt >= FORMAT || (fmt == 1 && ok1 == Some(1))
}

/// Whether the index holds `path` as it is now (no decoding). For the
/// built-in recorder's entries (nearly all) only the columns before the
/// layout are read; another's format, kept after it, is read only for those.
pub(super) fn has(path: &Path, stamp: (u64, Option<std::time::SystemTime>)) -> bool {
    let (Some(db), Some(mt)) = (DB.get(), mtime_ns(stamp.1)) else { return false };
    let c = db.lock().unwrap();
    let row = c
        .query_row("SELECT l.len, l.mtime, i.ok1 FROM layouts l JOIN inits i ON i.id = l.init WHERE l.path = ?1", params![key(path)], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?))
        })
        .optional()
        .ok()
        .flatten();
    match row {
        Some((len, m, _)) if len as u64 != stamp.0 || m != mt => false,
        Some((_, _, Some(1))) => true,
        Some((_, _, ok1)) => c
            .query_row("SELECT fmt FROM layouts WHERE path = ?1", params![key(path)], |r| r.get::<_, i64>(0))
            .is_ok_and(|fmt| current(fmt, ok1)),
        None => false,
    }
}

/// When the last sweep for removed files finished (epoch seconds).
const SWEPT_AT: &str = "swept_at";
/// Every recording up to this segment id (ambersight.db) has been checked.
const CHECKED_THROUGH: &str = "checked_through";
/// The [`FORMAT`] every entry was brought up to by a finished upgrade pass
/// (nothing left to read again): later starts do not look again.
const FMT_DONE: &str = "fmt_done";
const SWEEP_EVERY_SECS: i64 = 7 * 24 * 3600;

fn sweep_due(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|t| now - t >= SWEEP_EVERY_SECS || t > now)
}

fn meta_get(key: &str) -> Option<i64> {
    meta_get_in(&DB.get()?.lock().unwrap(), key)
}

fn meta_get_in(c: &Connection, key: &str) -> Option<i64> {
    c.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional().ok().flatten()
}

fn meta_set(key: &str, value: i64) {
    if let Some(db) = DB.get() {
        let _ = db.lock().unwrap().execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![key, value]);
    }
}

/// The recordings (segment ids) a finished pass of the start-up layout
/// check covered; `None` before the first finished pass (or after the
/// index was made anew).
pub fn checked_through() -> Option<i64> {
    meta_get(CHECKED_THROUGH)
}

/// How many recordings the index holds.
pub fn layout_count() -> i64 {
    DB.get().and_then(|db| db.lock().unwrap().query_row("SELECT COUNT(*) FROM layouts", [], |r| r.get(0)).ok()).unwrap_or(0)
}

pub fn set_checked_through(id: i64) {
    meta_set(CHECKED_THROUGH, id);
}

/// Whether there is an index to fill (not in tests or command-line tools).
pub fn available() -> bool {
    DB.get().is_some()
}

pub(super) fn store(path: &Path, stamp: (u64, Option<std::time::SystemTime>), p: &ParsedSegment) {
    let (Some(db), Some(mt)) = (DB.get(), mtime_ns(stamp.1)) else { return };
    let data = encode(p);
    let c = db.lock().unwrap();
    let r = (|| -> rusqlite::Result<()> {
        c.execute("INSERT OR IGNORE INTO inits (bytes, ok1) VALUES (?1, ?2)", params![p.init, by_our_recorder(&p.init) as i64])?;
        let init: i64 = c.query_row("SELECT id FROM inits WHERE bytes = ?1", params![p.init], |r| r.get(0))?;
        c.execute(
            "INSERT OR REPLACE INTO layouts (path, len, mtime, init, data, fmt) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![key(path), stamp.0 as i64, mt, init, data, FORMAT],
        )?;
        Ok(())
    })();
    if let Err(e) = r {
        tracing::debug!("recording index: could not save {}: {e}", path.display());
    }
}

/// What the format upgrade has to do (see [`FORMAT`]).
#[derive(Debug, PartialEq, Eq)]
pub enum UpgradePlan {
    /// Nothing: a pass finished before, or no entry can be of an older
    /// format another writer made (no header but the built-in recorder's).
    Done,
    /// Look through the entries for those with one of these headers (not
    /// the built-in recorder's) and an older format: [`upgrade_chunk`].
    Scan(HashSet<i64>),
}

/// What the format upgrade has to do. Reads only the small `meta` and
/// `inits` tables, never the layouts: a start with nothing to do costs
/// nothing.
pub fn upgrade_plan() -> UpgradePlan {
    let Some(db) = DB.get() else { return UpgradePlan::Done };
    upgrade_plan_in(&db.lock().unwrap())
}

fn upgrade_plan_in(c: &Connection) -> UpgradePlan {
    if meta_get_in(c, FMT_DONE).is_some_and(|v| v >= FORMAT) {
        return UpgradePlan::Done;
    }
    let others: rusqlite::Result<HashSet<i64>> = c
        .prepare("SELECT id FROM inits WHERE ok1 IS NOT 1")
        .and_then(|mut st| st.query_map([], |r| r.get(0))?.collect());
    let others = match others {
        Ok(o) => o,
        Err(e) => {
            // (Not noted as done: looked at again at the next start.)
            tracing::warn!("recording index: could not list the recording headers: {e}");
            return UpgradePlan::Done;
        }
    };
    if others.is_empty() {
        upgrade_done_in(c);
        return UpgradePlan::Done;
    }
    UpgradePlan::Scan(others)
}

/// A finished upgrade pass left nothing to read again: later starts skip it.
pub fn upgrade_done() {
    if let Some(db) = DB.get() {
        upgrade_done_in(&db.lock().unwrap());
    }
}

fn upgrade_done_in(c: &Connection) {
    let _ = c.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![FMT_DONE, FORMAT]);
}

/// One step of the upgrade scan: up to `n` entries after `after` (by
/// path), read on the read-only connection (playback and indexing do not
/// wait for it). Gives the entries among them with one of the `others`
/// headers and an older format (their files are to be read again), and
/// the last path looked at (None: the end).
/// Err: the entries could not be read (the pass stops, not noted as done).
pub fn upgrade_chunk(after: &str, n: usize, others: &HashSet<i64>) -> rusqlite::Result<(Vec<String>, Option<String>)> {
    let Some(db) = RO.get().or(DB.get()) else { return Ok((Vec::new(), None)) };
    upgrade_chunk_in(&db.lock().unwrap(), after, n, others)
}

fn upgrade_chunk_in(c: &Connection, after: &str, n: usize, others: &HashSet<i64>) -> rusqlite::Result<(Vec<String>, Option<String>)> {
    // (path and init come before the layout in each row: its overflow
    // pages are not read.)
    let rows: Vec<(String, i64)> = {
        let mut st = c.prepare("SELECT path, init FROM layouts WHERE path > ?1 ORDER BY path LIMIT ?2")?;
        let rows = st.query_map(params![after, n as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let last = rows.last().map(|r| r.0.clone());
    let again = rows
        .into_iter()
        .filter(|(_, i)| others.contains(i))
        // (A format that cannot be read: read the file again.)
        .filter(|(p, _)| c.query_row("SELECT fmt FROM layouts WHERE path = ?1", params![p], |r| r.get::<_, i64>(0)).map_or(true, |f| f < FORMAT))
        .map(|(p, _)| p)
        .collect();
    Ok((again, last))
}

/// Whether the file of an entry is gone for good: not there while its
/// folder is (a drive or share out of reach, or a folder that cannot be
/// read, is not taken for files removed).
fn file_gone(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.parent().is_some_and(|d| d.is_dir()),
        Err(_) => false,
    }
}

/// The recording at `src` was moved to `dest` (to the archive drive, as a
/// rename or a copy): keep its layout under the new name, so the archive
/// copy does not have to be read again to make it. Kept only if the size
/// still matches; otherwise forgotten.
pub fn moved(src: &Path, dest: &Path) {
    let Some(db) = DB.get() else { return };
    let Ok(m) = std::fs::metadata(dest) else {
        forget(src);
        return;
    };
    let Some(mt) = mtime_ns(m.modified().ok()) else {
        forget(src);
        return;
    };
    let c = db.lock().unwrap();
    let kept = c
        .execute(
            "UPDATE OR REPLACE layouts SET path = ?2, mtime = ?3 WHERE path = ?1 AND len = ?4",
            params![key(src), key(dest), mt, m.len() as i64],
        )
        .unwrap_or(0);
    if kept == 0 {
        let _ = c.execute("DELETE FROM layouts WHERE path = ?1", params![key(src)]);
    }
}

/// The file at `path` was deleted or moved.
pub fn forget(path: &Path) {
    if let Some(db) = DB.get() {
        let _ = db.lock().unwrap().execute("DELETE FROM layouts WHERE path = ?1", params![key(path)]);
    }
}

/// A recording was just closed: save its layout now (in the background,
/// never holding up recording), while the file is still in memory.
pub fn remember(path: &Path) {
    if DB.get().is_none() {
        return;
    }
    static TX: OnceLock<Mutex<mpsc::Sender<std::path::PathBuf>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<std::path::PathBuf>();
        std::thread::Builder::new()
            .name("layout-index".into())
            .spawn(move || {
                for p in rx {
                    if let Err(e) = super::parse_segment(&p) {
                        tracing::debug!("recording index: {}: {e:#}", p.display());
                    }
                }
            })
            .ok();
        Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send(path.to_path_buf());
}

/// Drop entries whose file is gone, and headers no entry uses.
fn sweep() {
    let Some(db) = DB.get() else { return };
    let mut after = String::new();
    let mut dropped = 0usize;
    loop {
        // In batches, so playback is never kept waiting long.
        let paths: Vec<String> = {
            let c = db.lock().unwrap();
            let Ok(mut st) = c.prepare("SELECT path FROM layouts WHERE path > ?1 ORDER BY path LIMIT 500") else { return };
            let Ok(rows) = st.query_map(params![after], |r| r.get::<_, String>(0)) else { return };
            rows.flatten().collect()
        };
        let Some(last) = paths.last() else { break };
        after = last.clone();
        let gone: Vec<&String> = paths.iter().filter(|p| file_gone(Path::new(p.as_str()))).collect();
        if !gone.is_empty() {
            let c = db.lock().unwrap();
            for p in &gone {
                let _ = c.execute("DELETE FROM layouts WHERE path = ?1", params![p]);
            }
            dropped += gone.len();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let c = db.lock().unwrap();
    let _ = c.execute("DELETE FROM inits WHERE id NOT IN (SELECT DISTINCT init FROM layouts)", []);
    if dropped > 0 {
        tracing::info!("recording index: dropped {dropped} entries of removed files");
    }
}

// Compact encoding: unsigned LEB128 numbers.

fn put(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn get(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*at)?;
        *at += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn encode(p: &ParsedSegment) -> Vec<u8> {
    let mut o = Vec::with_capacity(16 + p.fragments.len() * 24);
    put(&mut o, p.tracks.len() as u64);
    for t in &p.tracks {
        put(&mut o, t.id as u64);
        put(&mut o, t.timescale as u64);
        put(&mut o, t.video as u64);
    }
    put(&mut o, p.fragments.len() as u64);
    let mut prev_end = 0u64;
    for f in &p.fragments {
        // Fragments follow each other: the gap from the previous one is
        // normally zero.
        put(&mut o, f.offset.wrapping_sub(prev_end));
        put(&mut o, f.len);
        prev_end = f.offset + f.len;
        put(&mut o, f.seq_pos as u64);
        put(&mut o, f.trafs.len() as u64);
        for t in &f.trafs {
            put(&mut o, t.track_id as u64);
            put(&mut o, t.tfdt_pos as u64);
            put(&mut o, t.tfdt);
            put(&mut o, t.duration);
            put(&mut o, t.first_is_sync as u64 | (t.first_sample.is_some() as u64) << 1 | (t.even as u64) << 2);
            // (Zigzag: the offset can be negative.)
            put(&mut o, ((t.first_cto << 1) ^ (t.first_cto >> 31)) as u32 as u64);
            if let Some((off, size)) = t.first_sample {
                put(&mut o, off);
                put(&mut o, size as u64);
            }
        }
    }
    o
}

fn decode(b: &[u8], init: Vec<u8>) -> Option<ParsedSegment> {
    let at = &mut 0usize;
    let small = |v: u64| u32::try_from(v).ok();
    let n = get(b, at)? as usize;
    let mut tracks = Vec::with_capacity(n.min(16));
    for _ in 0..n {
        tracks.push(TrackMeta { id: small(get(b, at)?)?, timescale: small(get(b, at)?)?, video: get(b, at)? != 0 });
    }
    let n = get(b, at)? as usize;
    let mut fragments = Vec::with_capacity(n.min(100_000));
    let mut prev_end = 0u64;
    for _ in 0..n {
        let offset = prev_end.wrapping_add(get(b, at)?);
        let len = get(b, at)?;
        prev_end = offset.checked_add(len)?;
        let seq_pos = get(b, at)? as usize;
        let k = get(b, at)? as usize;
        let mut trafs = Vec::with_capacity(k.min(16));
        for _ in 0..k {
            let track_id = small(get(b, at)?)?;
            let tfdt_pos = get(b, at)? as usize;
            let tfdt = get(b, at)?;
            let duration = get(b, at)?;
            let flags = get(b, at)?;
            let z = small(get(b, at)?)?;
            let first_cto = ((z >> 1) as i32) ^ -((z & 1) as i32);
            let first_sample = if flags & 2 != 0 { Some((get(b, at)?, small(get(b, at)?)?)) } else { None };
            // (Layouts saved before `even` was kept read as not even: checked.)
            trafs.push(TrafInfo { track_id, tfdt_pos, tfdt, duration, first_is_sync: flags & 1 != 0, first_sample, first_cto, even: flags & 4 != 0 });
        }
        fragments.push(FragmentInfo { offset, len, seq_pos, trafs }.checked());
    }
    (*at == b.len() && !tracks.is_empty()).then_some(ParsedSegment { init, tracks, fragments })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweeps_are_weekly() {
        let now = 1_800_000_000;
        assert!(sweep_due(None, now), "never swept: sweep");
        assert!(!sweep_due(Some(now - 3600), now));
        assert!(!sweep_due(Some(now - SWEEP_EVERY_SECS + 1), now));
        assert!(sweep_due(Some(now - SWEEP_EVERY_SECS), now));
        assert!(sweep_due(Some(now + 86_400), now), "a clock set back does not stop sweeps for good");
    }

    #[test]
    fn layouts_survive_encoding() {
        let p = ParsedSegment {
            init: vec![1, 2, 3],
            tracks: vec![TrackMeta { id: 1, timescale: 90000, video: true }, TrackMeta { id: 2, timescale: 8000, video: false }],
            fragments: vec![
                FragmentInfo {
                    offset: 700,
                    len: 123_456,
                    seq_pos: 20,
                    trafs: vec![
                        TrafInfo { track_id: 1, tfdt_pos: 60, tfdt: 1 << 40, duration: 90000, first_is_sync: true, first_sample: Some((300, 40_000)), first_cto: -3000, even: true },
                        TrafInfo { track_id: 2, tfdt_pos: 160, tfdt: 7, duration: 8000, first_is_sync: false, first_sample: None, first_cto: 6000, even: false },
                    ],
                },
                FragmentInfo { offset: 124_156, len: 99, seq_pos: 20, trafs: vec![] },
            ],
        };
        let q = decode(&encode(&p), p.init.clone()).unwrap();
        assert_eq!(q.tracks.len(), 2);
        assert_eq!(q.tracks[1].timescale, 8000);
        assert_eq!(q.fragments.len(), 2);
        assert_eq!(q.fragments[1].offset, 124_156);
        let t = &q.fragments[0].trafs[0];
        assert_eq!((t.tfdt, t.duration, t.first_is_sync, t.first_sample), (1 << 40, 90000, true, Some((300, 40_000))));
        assert!(!q.fragments[0].trafs[1].first_is_sync);
        assert_eq!((q.fragments[0].trafs[0].first_cto, q.fragments[0].trafs[1].first_cto), (-3000, 6000));
        assert_eq!((q.fragments[0].trafs[0].even, q.fragments[0].trafs[1].even), (true, false));
        // Damaged data is refused, not misread.
        let mut bad = encode(&p);
        bad.truncate(bad.len() - 1);
        assert!(decode(&bad, vec![]).is_none());
    }

    #[test]
    fn saved_layouts_match_the_files_and_go_stale_when_a_file_changes() {
        let dir = std::env::temp_dir().join(format!("amber-layouts-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let segs = super::super::tests::record(&dir);
        open_index(&dir.join("layouts.db"));
        let stamp = |p: &Path| {
            let m = std::fs::metadata(p).unwrap();
            (m.len(), m.modified().ok())
        };
        for s in &segs {
            let direct = super::super::parse_segment_file(&s.path).unwrap();
            store(&s.path, stamp(&s.path), &direct);
            let saved = load(&s.path, stamp(&s.path)).expect("saved layout");
            assert_eq!(saved.init, direct.init);
            assert_eq!(saved.fragments.len(), direct.fragments.len());
            for (a, b) in saved.fragments.iter().zip(&direct.fragments) {
                assert_eq!((a.offset, a.len, a.seq_pos, a.trafs.len()), (b.offset, b.len, b.seq_pos, b.trafs.len()));
                assert_eq!((a.trafs[0].tfdt, a.trafs[0].first_sample), (b.trafs[0].tfdt, b.trafs[0].first_sample));
            }
        }
        // A changed file is not answered from the index; a forgotten one neither.
        let p = &segs[0].path;
        let (len, mt) = stamp(p);
        assert!(load(p, (len + 1, mt)).is_none());
        // A recording moved to another folder (the archive) keeps its layout
        // under the new name; the old name is gone.
        let q = &segs[1].path;
        let dest = dir.join("archive").join(q.file_name().unwrap());
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(q, &dest).unwrap();
        let old_stamp = stamp(q);
        std::fs::remove_file(q).unwrap();
        moved(q, &dest);
        assert!(load(q, old_stamp).is_none());
        let at_dest = load(&dest, stamp(&dest)).expect("layout kept after a move");
        assert_eq!(at_dest.fragments.len(), super::super::parse_segment_file(&dest).unwrap().fragments.len());
        forget(p);
        assert!(load(p, (len, mt)).is_none());

        // Entries saved in format 1. The built-in recorder's count as current
        // as they are (nothing rewritten). One with another header (here: a
        // header with a box ffmpeg's has) counts as missing everywhere, and
        // the background pass reads its file again once; one whose file is
        // gone is dropped; one whose folder cannot be reached is kept.
        assert!(segs.len() >= 4);
        let fmt_of = |p: &Path| -> Option<i64> { DB.get().unwrap().lock().unwrap().query_row("SELECT fmt FROM layouts WHERE path = ?1", params![key(p)], |r| r.get(0)).optional().unwrap() };
        let old_format = |p: &Path| {
            DB.get().unwrap().lock().unwrap().execute("UPDATE layouts SET fmt = 1 WHERE path = ?1", params![key(p)]).unwrap();
        };
        let foreign = |p: &Path, stamp_of: &Path| {
            let mut f = super::super::parse_segment_file(stamp_of).unwrap();
            f.init.extend_from_slice(b"\0\0\0\x08free");
            store(p, stamp(stamp_of), &f);
            old_format(p);
        };
        let (r, ours) = (&segs[2].path, &segs[3].path);
        old_format(ours);
        assert!(has(ours, stamp(ours)) && load(ours, stamp(ours)).is_some(), "the built-in recorder's: current as it is");
        assert_eq!(fmt_of(ours), Some(1), "not rewritten");
        foreign(r, r);
        foreign(&dest, &dest);
        let offline = dir.join("offline-drive").join("x.mp4");
        foreign(&offline, r);
        assert!(!has(r, stamp(r)), "another header in an older format is not current");
        assert!(load(r, stamp(r)).is_none());
        std::fs::remove_file(&dest).unwrap();
        let n = crate::engine::reindex::upgrade_pass(|| {});
        assert_eq!((n.redone, n.dropped, n.kept), (1, 1, 1), "{n:?}");
        assert!(has(r, stamp(r)), "read again in the current format");
        assert_eq!(fmt_of(r), Some(FORMAT));
        assert_eq!(fmt_of(&dest), None, "the entry of a file that is gone is dropped");
        assert_eq!(fmt_of(&offline), Some(1), "one out of reach is kept");
        assert_eq!(fmt_of(ours), Some(1), "the built-in recorder's are not touched");
        assert!(!has(&offline, stamp(r)), "still not current: read again when played");
        assert_eq!(upgrade_plan(), UpgradePlan::Done, "done even with one out of reach: not looked through at every start");
        let n = crate::engine::reindex::upgrade_pass(|| {});
        assert_eq!(n, Default::default());
        forget(&offline);
        // Checked or played before the pass: the same choice.
        foreign(r, r);
        assert_eq!(super::super::index_recording(r).unwrap(), true, "read again when checked");
        assert_eq!(fmt_of(r), Some(FORMAT));
        assert_eq!(super::super::index_recording(r).unwrap(), false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn which_entries_are_current() {
        assert!(current(FORMAT, None) && current(FORMAT, Some(0)) && current(FORMAT, Some(1)));
        assert!(current(1, Some(1)), "format 1 of the built-in recorder's");
        assert!(!current(1, Some(0)) && !current(1, None), "format 1 of another's, or not known");
        assert!(!current(0, Some(1)));
    }

    /// Only the built-in recorder's file headers are taken as its own (its
    /// layouts read the same with the current reader); ffmpeg's are not.
    #[test]
    fn the_built_in_recorders_headers_are_told_from_ffmpegs() {
        let dir = std::env::temp_dir().join(format!("amber-layouts-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let segs = super::super::tests::record(&dir);
        let ours = super::super::parse_segment_file(&segs[0].path).unwrap();
        assert!(by_our_recorder(&ours.init));
        assert!(!by_our_recorder(&[]) && !by_our_recorder(&ours.init[..ours.init.len() - 3]), "unreadable: not ours");
        // A track default set (as another writer may): not ours.
        let mut other = ours.init.clone();
        let at = other.windows(4).position(|w| w == b"trex").unwrap();
        other[at + 4 + 12 + 3] = 1; // default duration
        assert!(!by_our_recorder(&other));
        // Another ftyp (brand), or a box more: not ours.
        let mut other = ours.init.clone();
        other[8..12].copy_from_slice(b"isom");
        assert!(!by_our_recorder(&other));
        let mut other = ours.init.clone();
        other.extend_from_slice(b"\0\0\0\x08free");
        assert!(!by_our_recorder(&other));
        if crate::child::ffmpeg().arg("-version").output().is_ok_and(|o| o.status.success()) {
            let out = crate::child::ffmpeg()
                .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=10", "-t", "2"])
                .args(["-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p", "-movflags", "+frag_keyframe+empty_moov+default_base_moof"])
                .arg(dir.join("ff.mp4"))
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let ff = super::super::parse_segment_file(&dir.join("ff.mp4")).unwrap();
            assert!(!by_our_recorder(&ff.init), "ffmpeg's header");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An index made before entries kept their format: kept (not made anew,
    /// nothing rewritten), its entries of the first format, its headers
    /// marked once. A start with nothing to upgrade reads none of the
    /// layouts; one that has to look reads only the entries it needs.
    #[test]
    fn an_older_index_is_upgraded_without_reading_or_rewriting_its_layouts() {
        let dir = std::env::temp_dir().join(format!("amber-layouts-fmt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let segs = super::super::tests::record(&dir);
        let ours = super::super::parse_segment_file(&segs[0].path).unwrap().init;
        let path = dir.join("layouts.db");
        const ROWS: usize = 20_000;
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "journal_mode", "WAL").unwrap();
            c.execute_batch(
                "CREATE TABLE inits (id INTEGER PRIMARY KEY, bytes BLOB NOT NULL UNIQUE);
                 CREATE TABLE layouts (path TEXT PRIMARY KEY, len INTEGER NOT NULL, mtime INTEGER NOT NULL, init INTEGER NOT NULL, data BLOB NOT NULL) WITHOUT ROWID;
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
                 INSERT INTO meta VALUES ('checked_through', 7);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
            c.execute("INSERT INTO inits (id, bytes) VALUES (1, ?1)", params![ours]).unwrap();
            let tx = c.unchecked_transaction().unwrap();
            // Layouts of a few kB, as on a site (their pages overflow).
            let blob = vec![7u8; 3000];
            for i in 0..ROWS {
                tx.execute("INSERT INTO layouts VALUES (?1, 10, 20, 1, ?2)", params![format!("/r/cam/cam-{i:08}-seg000000.mp4"), blob]).unwrap();
            }
            tx.commit().unwrap();
            c.pragma_update(None, "wal_checkpoint", "TRUNCATE").unwrap();
        }
        let wal = || std::fs::metadata(dir.join("layouts.db-wal")).map(|m| m.len()).unwrap_or(0);
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 50_000_000, "{size}");
        let c = open(&path).unwrap();
        assert!(wal() < 1_000_000, "nothing rewritten at the upgrade: {} bytes written", wal());
        let fmt: i64 = c.query_row("SELECT fmt FROM layouts WHERE path = '/r/cam/cam-00000000-seg000000.mp4'", [], |r| r.get(0)).unwrap();
        assert_eq!(fmt, 1);
        let ok1: i64 = c.query_row("SELECT ok1 FROM inits WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(ok1, 1, "the built-in recorder's header, marked once");
        let kept: i64 = c.query_row("SELECT value FROM meta WHERE key = 'checked_through'", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 7, "the index is kept, not made anew");
        // Only the recorder's headers: nothing to look through; done.
        let t = std::time::Instant::now();
        assert_eq!(upgrade_plan_in(&c), UpgradePlan::Done);
        assert_eq!(upgrade_plan_in(&c), UpgradePlan::Done);
        assert!(t.elapsed() < std::time::Duration::from_millis(200), "the layouts are not read: {:?}", t.elapsed());
        assert!(wal() < 1_000_000);
        drop(c);
        // Opened again (the next start): nothing changes, nothing is written.
        let c = open(&path).unwrap();
        assert_eq!(upgrade_plan_in(&c), UpgradePlan::Done);
        assert!(wal() < 1_000_000);
        // An older version ran meanwhile and saved entries of the first
        // format with another header (ffmpeg's), and one in the current
        // format: at the next start the pass looks again (the new header
        // clears the mark), and the scan finds only the old ones.
        let mut ff = ours.clone();
        ff.extend_from_slice(b"\0\0\0\x08free");
        c.execute("INSERT INTO inits (id, bytes) VALUES (2, ?1)", params![ff]).unwrap();
        for (p, fmt) in [("/r/ff/a.mp4", 1), ("/r/ff/b.mp4", 1), ("/r/ff/c.mp4", FORMAT)] {
            c.execute("INSERT INTO layouts VALUES (?1, 10, 20, 2, x'00', ?2)", params![p, fmt]).unwrap();
        }
        drop(c);
        let c = open(&path).unwrap();
        let UpgradePlan::Scan(others) = upgrade_plan_in(&c) else { panic!("an upgrade to do") };
        assert_eq!(others, HashSet::from([2]));
        let (mut found, mut after, mut steps) = (Vec::new(), String::new(), 0);
        loop {
            let (again, last) = upgrade_chunk_in(&c, &after, 2000, &others).unwrap();
            found.extend(again);
            steps += 1;
            let Some(l) = last else { break };
            after = l;
        }
        assert_eq!(found, vec!["/r/ff/a.mp4".to_string(), "/r/ff/b.mp4".into()]);
        assert_eq!(steps, ROWS / 2000 + 2, "in steps");
        upgrade_done_in(&c);
        assert_eq!(upgrade_plan_in(&c), UpgradePlan::Done);
        // A scan that cannot read the entries says so (the pass stops and
        // is not noted as done), not "the end".
        let broken = Connection::open_in_memory().unwrap();
        assert!(upgrade_chunk_in(&broken, "", 10, &others).is_err());
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Marking the headers failing at a start is no reason to make the
    /// index anew: it opens, the header counts as another's.
    #[test]
    fn an_index_whose_headers_cannot_be_marked_still_opens() {
        let dir = std::env::temp_dir().join(format!("amber-layouts-nomark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layouts.db");
        drop(open(&path).unwrap());
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "INSERT INTO inits (id, bytes) VALUES (1, x'00');
                 INSERT INTO layouts VALUES ('/r/a.mp4', 10, 20, 1, x'00', 1);
                 INSERT INTO meta VALUES ('checked_through', 7);
                 CREATE TRIGGER no_marks BEFORE UPDATE ON inits BEGIN SELECT RAISE(ABORT, 'no'); END;",
            )
            .unwrap();
        }
        let c = open(&path).expect("opens");
        let ok1: Option<i64> = c.query_row("SELECT ok1 FROM inits WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(ok1, None);
        assert!(!current(1, ok1), "not marked: another's (read again when played)");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM layouts", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "kept");
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The weekly sweep drops an entry only when its file is gone from a
    /// folder that is there (not those of a drive that is unplugged).
    #[test]
    fn only_files_gone_from_a_folder_that_is_there_are_gone() {
        let dir = std::env::temp_dir().join(format!("amber-layouts-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("here.mp4"), b"x").unwrap();
        assert!(!file_gone(&dir.join("here.mp4")));
        assert!(file_gone(&dir.join("gone.mp4")));
        assert!(!file_gone(&dir.join("unplugged").join("x.mp4")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
