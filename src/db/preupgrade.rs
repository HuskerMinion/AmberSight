//! A copy of the database saved before a start-up upgrade changes its layout
//! (`PRAGMA user_version` lower than this program's). Kept in
//! `<data_dir>/backups/pre-upgrade-v<old>-<date>/ambersight.db`, the two newest
//! only. Never stops a start: a failed copy is a warning.

use std::path::{Path, PathBuf};
use std::time::Instant;

const PREFIX: &str = "pre-upgrade-v";
const KEEP: usize = 2;
/// Free space wanted beyond the copy's own size.
const SPARE_BYTES: u64 = 512 << 20;
/// A copy slower than this gets a log line saying why.
const SLOW_SECS: f32 = 10.0;
const STAMP: &str = "%Y%m%d-%H%M%S";

/// Save a copy of the database file at `path` when it is at an older version
/// than `target` (and not new: version 0). Returns the copy's file.
pub fn copy_before_upgrade(path: &Path, target: i64) -> Option<PathBuf> {
    match try_copy(path, target) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("could not save a copy of the database before upgrading it ({e:#}); continuing without");
            None
        }
    }
}

fn open_rw(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    Ok(conn)
}

fn try_copy(path: &Path, target: i64) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context;
    if !path.is_file() {
        return Ok(None);
    }
    let version: i64 = {
        let conn = open_rw(path)?;
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))?
    };
    if version < 1 || version >= target {
        return Ok(None);
    }
    let data_dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if let Ok(free) = fs4::available_space(data_dir) {
        if free < size.saturating_add(SPARE_BYTES) {
            tracing::warn!(
                "no copy of the database saved before upgrading it from version {version}: {} MB free, a copy needs {} MB",
                free >> 20,
                (size + SPARE_BYTES) >> 20
            );
            return Ok(None);
        }
    }
    let root = crate::backup::backups_dir(data_dir);
    crate::backup::private_dir(&root)?;
    let dir = root.join(format!("{PREFIX}{version}-{}", chrono::Local::now().format(STAMP)));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let file = dir.join("ambersight.db");
    let t0 = Instant::now();
    let r = copy_file(path, &file, size).or_else(|e| {
        // The plain copy did not work (or something else has the database open):
        // fall back to the slower rebuild, which works on a live database.
        tracing::warn!("copying the database file did not work ({e:#}); rebuilding it into the copy instead (slower)");
        let _ = std::fs::remove_file(&file);
        open_rw(path)?.execute("VACUUM INTO ?1", [file.to_string_lossy()]).map(|_| ()).with_context(|| format!("copy to {}", file.display()))
    });
    if let Err(e) = r {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    let secs = t0.elapsed().as_secs_f32();
    tracing::info!(
        "saved a copy of the database (version {version}, {} MB) to {} in {secs:.1} s before upgrading it to version {target}",
        size >> 20,
        file.display(),
    );
    if secs > SLOW_SECS {
        tracing::warn!("the database copy took {secs:.0} s: on a spinning drive this can take a minute or more, and recording starts after it");
    }
    prune(&root);
    Ok(Some(file))
}

/// Copy the database file front to back (what a spinning drive is good at; `VACUUM INTO` and
/// the backup API read it page by page in index order). Nothing else has the database open
/// this early in `Db::open`. A connection takes it exclusively and folds the write-ahead log
/// into the main file, then closes (Windows will not let the file be read while SQLite holds
/// it exclusively). Anything unexpected is an error, and the caller then uses `VACUUM INTO`.
fn copy_file(path: &Path, to: &Path, size_before: u64) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    {
        let c = open_rw(path)?;
        c.busy_timeout(std::time::Duration::from_secs(2))?; // someone else has it: no waiting, use the rebuild
        c.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        let (busy, log, done): (i64, i64, i64) = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        // (-1 / -1 means it is not in write-ahead mode: nothing to fold in.)
        if busy != 0 || (log > 0 && done < log) {
            bail!("the write-ahead log could not be folded in (busy {busy}, {done} of {log} pages)");
        }
    }
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    if std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) > 0 {
        bail!("the write-ahead log is not empty");
    }
    let n = std::fs::copy(path, to).with_context(|| format!("copy {} to {}", path.display(), to.display()))?;
    let now = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if n != now || now < size_before {
        bail!("the copy is {n} bytes but the database is {now}");
    }
    // Make it durable before the upgrade starts changing the original.
    std::fs::OpenOptions::new().write(true).open(to).and_then(|f| f.sync_all()).with_context(|| format!("flush {}", to.display()))?;
    Ok(())
}

/// Keep only the newest `KEEP` pre-upgrade folders (their name ends in the stamp).
fn prune(root: &Path) {
    let mut all: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(PREFIX))).collect())
        .unwrap_or_default();
    all.sort_by_key(|p| p.file_name().and_then(|n| n.to_str()).map(|n| {
        // the name ends in "YYYYMMDD-HHMMSS"; sort by that (date first, then time)
        let mut parts = n.rsplitn(3, '-');
        let time = parts.next().unwrap_or("");
        let date = parts.next().unwrap_or("");
        format!("{date}{time}")
    }).unwrap_or_default());
    let n = all.len().saturating_sub(KEEP);
    for old in &all[..n] {
        if let Err(e) = std::fs::remove_dir_all(old) {
            tracing::warn!("could not remove the old database copy {}: {e}", old.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-preup-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make(path: &Path, version: i64) {
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (7);").unwrap();
        c.pragma_update(None, "user_version", version).unwrap();
    }

    #[test]
    fn an_older_database_gets_a_copy_and_a_current_or_new_one_does_not() {
        let d = temp("older");
        let p = d.join("ambersight.db");
        make(&p, 28);
        let copy = copy_before_upgrade(&p, 29).expect("a copy");
        assert!(copy.to_string_lossy().contains("pre-upgrade-v28-"));
        let c = rusqlite::Connection::open(&copy).unwrap();
        assert_eq!(c.query_row::<i64, _, _>("SELECT x FROM t", [], |r| r.get(0)).unwrap(), 7);
        assert_eq!(c.query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0)).unwrap(), 28);
        assert!(copy_before_upgrade(&p, 28).is_none(), "already current");
        let q = d.join("new.db");
        make(&q, 0);
        assert!(copy_before_upgrade(&q, 29).is_none(), "a new database has nothing to save");
        assert!(copy_before_upgrade(&d.join("missing.db"), 29).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn integrity(p: &Path) -> String {
        rusqlite::Connection::open(p).unwrap().query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap()
    }

    fn count(p: &Path) -> i64 {
        rusqlite::Connection::open(p).unwrap().query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_big_database_in_wal_mode_is_copied_whole_and_valid() {
        let d = temp("wal");
        let p = d.join("ambersight.db");
        {
            let c = rusqlite::Connection::open(&p).unwrap();
            c.pragma_update(None, "journal_mode", "WAL").unwrap();
            c.execute_batch("CREATE TABLE t (x INTEGER, s TEXT UNIQUE);").unwrap();
            c.execute_batch("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 20000) INSERT INTO t SELECT i, hex(randomblob(40)) FROM n;").unwrap();
            c.pragma_update(None, "user_version", 28).unwrap();
        }
        let copy = copy_before_upgrade(&p, 29).expect("a copy");
        assert_eq!(integrity(&copy), "ok");
        assert_eq!(count(&copy), 20000);
        assert_eq!(std::fs::metadata(&copy).unwrap().len(), std::fs::metadata(&p).unwrap().len(), "the whole file, front to back");
        assert!(!copy.with_file_name("ambersight.db-wal").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rows_still_in_the_wal_are_in_the_copy() {
        let d = temp("wal-left");
        let live = d.join("live");
        std::fs::create_dir_all(&live).unwrap();
        let lp = live.join("ambersight.db");
        let c = rusqlite::Connection::open(&lp).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        c.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);").unwrap();
        c.pragma_update(None, "user_version", 28).unwrap();
        c.execute_batch("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 500) INSERT INTO t SELECT i FROM n;").unwrap();
        // What a crash leaves: the main file and a write-ahead log holding the newest rows.
        let crashed = d.join("crashed");
        std::fs::create_dir_all(&crashed).unwrap();
        let p = crashed.join("ambersight.db");
        std::fs::copy(&lp, &p).unwrap();
        std::fs::copy(live.join("ambersight.db-wal"), crashed.join("ambersight.db-wal")).unwrap();
        drop(c);
        assert!(std::fs::metadata(crashed.join("ambersight.db-wal")).unwrap().len() > 0, "the test needs a non-empty log");
        let copy = copy_before_upgrade(&p, 29).expect("a copy");
        assert_eq!(integrity(&copy), "ok");
        assert_eq!(count(&copy), 501);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_database_someone_else_has_open_is_still_copied_by_rebuilding() {
        let d = temp("busy");
        let p = d.join("ambersight.db");
        let other = rusqlite::Connection::open(&p).unwrap();
        other.pragma_update(None, "journal_mode", "WAL").unwrap();
        other.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (7);").unwrap();
        other.pragma_update(None, "user_version", 28).unwrap();
        let reader = other.prepare("SELECT x FROM t").unwrap();
        drop(reader);
        let copy = copy_before_upgrade(&p, 29).expect("a copy");
        assert_eq!(integrity(&copy), "ok");
        assert_eq!(count(&copy), 1);
        drop(other);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_the_two_newest_copies_stay() {
        let d = temp("prune");
        let root = d.join("backups");
        for n in ["pre-upgrade-v20-20260101-000000", "pre-upgrade-v27-20260301-000000", "pre-upgrade-v28-20260201-000000", "20260101-000000"] {
            std::fs::create_dir_all(root.join(n)).unwrap();
        }
        prune(&root);
        assert!(!root.join("pre-upgrade-v20-20260101-000000").exists());
        assert!(root.join("pre-upgrade-v27-20260301-000000").exists());
        assert!(root.join("pre-upgrade-v28-20260201-000000").exists());
        assert!(root.join("20260101-000000").exists(), "daily backups are not touched");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pruning_goes_by_date_then_time_not_time_first() {
        let d = temp("prune-time");
        let root = d.join("backups");
        for n in ["pre-upgrade-v27-20261101-230000", "pre-upgrade-v28-20261120-080000", "pre-upgrade-v29-20261205-070000"] {
            std::fs::create_dir_all(root.join(n)).unwrap();
        }
        prune(&root);
        assert!(!root.join("pre-upgrade-v27-20261101-230000").exists());
        assert!(root.join("pre-upgrade-v28-20261120-080000").exists());
        assert!(root.join("pre-upgrade-v29-20261205-070000").exists(), "the newest copy must stay");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_in_memory_database_is_never_copied() {
        // (Db::open_in_memory does not call it; a path that is not a file is skipped.)
        assert!(copy_before_upgrade(Path::new(":memory:"), 29).is_none());
    }
}
