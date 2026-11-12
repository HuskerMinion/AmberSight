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
use std::sync::{mpsc, Mutex, OnceLock};

/// Bumped when the stored layout changes: older entries are dropped.
const VERSION: i64 = 2;

static DB: OnceLock<Mutex<Connection>> = OnceLock::new();

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v != VERSION {
        c.execute_batch("DROP TABLE IF EXISTS layouts; DROP TABLE IF EXISTS inits;")?;
    }
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS inits (id INTEGER PRIMARY KEY, bytes BLOB NOT NULL UNIQUE);
         CREATE TABLE IF NOT EXISTS layouts (
             path TEXT PRIMARY KEY, len INTEGER NOT NULL, mtime INTEGER NOT NULL,
             init INTEGER NOT NULL, data BLOB NOT NULL) WITHOUT ROWID;",
    )?;
    c.pragma_update(None, "user_version", VERSION)?;
    Ok(c)
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
        // Entries of files removed some way we did not see (by hand, a
        // drive cleaned up): dropped a while after start, then daily.
        std::thread::Builder::new()
            .name("layout-sweep".into())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(600));
                sweep();
                std::thread::sleep(std::time::Duration::from_secs(24 * 3600 - 600));
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
    let row: Option<(i64, i64, Vec<u8>, Vec<u8>)> = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT l.len, l.mtime, l.data, i.bytes FROM layouts l JOIN inits i ON i.id = l.init WHERE l.path = ?1",
            params![key(path)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .ok()
        .flatten();
    let (len, mtime, data, init) = row?;
    if len as u64 != stamp.0 || mtime != mt {
        return None;
    }
    decode(&data, init)
}

/// Whether the index holds `path` as it is now (no decoding).
pub(super) fn has(path: &Path, stamp: (u64, Option<std::time::SystemTime>)) -> bool {
    let (Some(db), Some(mt)) = (DB.get(), mtime_ns(stamp.1)) else { return false };
    db.lock()
        .unwrap()
        .query_row("SELECT len, mtime FROM layouts WHERE path = ?1", params![key(path)], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
        .optional()
        .ok()
        .flatten()
        .is_some_and(|(len, m)| len as u64 == stamp.0 && m == mt)
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
        c.execute("INSERT OR IGNORE INTO inits (bytes) VALUES (?1)", params![p.init])?;
        let init: i64 = c.query_row("SELECT id FROM inits WHERE bytes = ?1", params![p.init], |r| r.get(0))?;
        c.execute(
            "INSERT OR REPLACE INTO layouts (path, len, mtime, init, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key(path), stamp.0 as i64, mt, init, data],
        )?;
        Ok(())
    })();
    if let Err(e) = r {
        tracing::debug!("recording index: could not save {}: {e}", path.display());
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
        let gone: Vec<&String> = paths.iter().filter(|p| !Path::new(p.as_str()).exists()).collect();
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
            put(&mut o, t.first_is_sync as u64 | (t.first_sample.is_some() as u64) << 1);
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
            trafs.push(TrafInfo { track_id, tfdt_pos, tfdt, duration, first_is_sync: flags & 1 != 0, first_sample, first_cto });
        }
        fragments.push(FragmentInfo { offset, len, seq_pos, trafs }.checked());
    }
    (*at == b.len() && !tracks.is_empty()).then_some(ParsedSegment { init, tracks, fragments })
}

#[cfg(test)]
mod tests {
    use super::*;

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
                        TrafInfo { track_id: 1, tfdt_pos: 60, tfdt: 1 << 40, duration: 90000, first_is_sync: true, first_sample: Some((300, 40_000)), first_cto: -3000 },
                        TrafInfo { track_id: 2, tfdt_pos: 160, tfdt: 7, duration: 8000, first_is_sync: false, first_sample: None, first_cto: 6000 },
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
