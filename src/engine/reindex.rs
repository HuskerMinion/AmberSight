//! Fills the layout index (`layouts.db`) for recordings made before it
//! existed, newest first, so playing older footage (fast especially) does
//! not have to read each file's headers from the disk while you watch.
//! Recordings closed since are indexed as they close. Works one file at a
//! time and waits while anyone is playing recordings. Once a pass has
//! finished, later starts check only the recordings indexed since.
//! Entries saved in an older layout format (see splice::index::FORMAT) are
//! read again afterwards, the same gentle way.

use super::*;
use std::time::Duration;

/// Wait this long after the last playback request before reading files.
const QUIET_SECS: i64 = 30;

/// True while a layout-index pass is running (shown as "still checking its
/// recordings" in the status).
static PASS_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn pass_running() -> bool {
    PASS_RUNNING.load(Ordering::Relaxed)
}

struct PassGuard;
impl Drop for PassGuard {
    fn drop(&mut self) {
        PASS_RUNNING.store(false, Ordering::Relaxed);
    }
}

fn wait_for_quiet() {
    loop {
        let last = playback::LAST_PLAYBACK.load(Ordering::Relaxed);
        let idle = Utc::now().timestamp() - last;
        if idle >= QUIET_SECS {
            return;
        }
        thread::sleep(Duration::from_secs((QUIET_SECS - idle).clamp(1, QUIET_SECS) as u64));
    }
}

pub fn reindex_loop(engine: Arc<Engine>) {
    // Cameras first connect and fill their buffers.
    thread::sleep(Duration::from_secs(180));
    PASS_RUNNING.store(true, Ordering::Relaxed);
    let _running = PassGuard;
    fill_pass(&engine);
    upgrade_pass(wait_for_quiet);
}

/// Bring the entries saved in an older layout format up to date (see
/// splice::index::FORMAT). The built-in recorder's need nothing (their
/// headers are marked once, in the small header table); only entries with
/// another header (ffmpeg's: they count as not indexed, playback reads such
/// a file itself and stills fall back to its first keyframe) are read
/// again. A start after a finished pass, or with no such header at all,
/// reads nothing of the layouts. Otherwise the entries are looked through
/// once on a read-only connection (playback and indexing do not wait for
/// it), in steps, waiting while anyone plays recordings (`quiet`), and the
/// files found are read again one at a time the same way. An entry whose
/// file is gone (its folder is there, the file not) or damaged is dropped;
/// one that cannot be reached now (a drive offline, a share, a lock) is
/// kept and looked at again at the next start. Gives what was done.
pub(crate) fn upgrade_pass(quiet: impl Fn()) -> Upgraded {
    let mut n = Upgraded::default();
    let crate::ingest::splice::UpgradePlan::Scan(others) = crate::ingest::splice::upgrade_plan() else { return n };
    info!("layout index: looking for recordings saved in an older format");
    let t0 = std::time::Instant::now();
    let mut after = String::new();
    loop {
        quiet();
        let (again, last) = match crate::ingest::splice::upgrade_chunk(&after, UPGRADE_CHUNK, &others) {
            Ok(r) => r,
            Err(e) => {
                // (Not noted as done: looked at again at the next start.)
                warn!("layout index: could not look through the saved layouts: {e}");
                return n;
            }
        };
        for p in &again {
            quiet();
            let path = std::path::Path::new(p);
            match crate::ingest::splice::index_recording(path) {
                Ok(true) => n.redone += 1,
                Ok(false) => n.had += 1,
                Err(e) if unreachable(&e, path) => n.kept += 1,
                Err(_) => {
                    // Removed (its folder is there, the file is not), or damaged.
                    crate::ingest::splice::forget(path);
                    n.dropped += 1;
                }
            }
            thread::sleep(Duration::from_millis(UPGRADE_PAUSE_MS));
        }
        let Some(last) = last else { break };
        after = last;
        thread::sleep(Duration::from_millis(UPGRADE_PAUSE_MS));
    }
    // Done even with entries out of reach kept: they count as not indexed
    // (another's header, the first format), so each is read again when it
    // is played; the whole index is not looked through at every start.
    crate::ingest::splice::upgrade_done();
    info!(
        "layout index: {} older-format recordings read again, {} were already, {} gone or damaged, {} out of reach for now, in {} min",
        n.redone,
        n.had,
        n.dropped,
        n.kept,
        t0.elapsed().as_secs() / 60
    );
    n
}

/// What the format upgrade did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Upgraded {
    /// Files read again.
    pub redone: u64,
    /// Already current by the time they came up (played meanwhile).
    pub had: u64,
    /// Entries dropped: the file is gone, or damaged.
    pub dropped: u64,
    /// Entries kept as they are: the file could not be reached now.
    pub kept: u64,
}

/// Whether reading `path` failed because it cannot be reached now (rather
/// than being gone or damaged): an input/output error other than "not
/// found", or "not found" while its folder is missing too (an archive drive
/// or share that is offline).
fn unreachable(e: &anyhow::Error, path: &std::path::Path) -> bool {
    let Some(io) = e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()) else { return false };
    match io.kind() {
        // (Shorter than its boxes say: damaged.)
        std::io::ErrorKind::UnexpectedEof => return false,
        std::io::ErrorKind::NotFound => {}
        _ => return true,
    }
    !path.parent().is_some_and(|d| d.is_dir())
}

/// Entries looked through per step of the upgrade scan.
const UPGRADE_CHUNK: usize = 2000;

/// The pause between two files the format upgrade reads again, and between
/// two steps of its scan.
const UPGRADE_PAUSE_MS: u64 = if cfg!(test) { 0 } else { 25 };

/// Index the recordings made before the index existed (see the module).
fn fill_pass(engine: &Arc<Engine>) {
    let t0 = std::time::Instant::now();
    // Recordings closed from now on are indexed as they close; this pass
    // covers everything up to here.
    let through = match engine.db.max_segment_id() {
        Ok(id) => id,
        Err(e) => {
            warn!("layout index: could not list recordings: {e:#}");
            return;
        }
    };
    let mut n = Counts::default();
    let mut done = crate::ingest::splice::checked_through();
    if done.is_none() {
        // No pass has been noted yet (an upgrade from a version that checked
        // every file at each start). An index that already holds nearly every
        // recording is taken as checked: one missing from it still plays (its
        // layout is read and saved when first played), and checking them all
        // once more can take hours on a big spinning drive.
        let (have, want) = (crate::ingest::splice::layout_count(), engine.db.native_segment_count().unwrap_or(i64::MAX));
        if index_nearly_full(have, want) {
            info!("layout index: holds {have} of {want} recordings; not checking them all again");
            crate::ingest::splice::set_checked_through(through);
            return;
        }
    }
    match done.take() {
        // A finished pass before: only the recordings indexed since. Checking
        // every file again would read the whole drive's file table at each
        // start (hours on a big spinning drive whose cache is cold).
        Some(done) if done <= through => {
            let mut after = done;
            loop {
                let rows = match engine.db.segments_after_id(after, through, 500) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("layout index: could not list recordings: {e:#}");
                        return;
                    }
                };
                let Some(&(last, _)) = rows.last() else { break };
                after = last;
                for (_, row) in rows {
                    check(engine, &row, &mut n);
                }
            }
        }
        // The first pass (or the recordings were made anew): every recording,
        // newest first.
        _ => {
            let (mut before_ms, mut before_id) = (i64::MAX, i64::MAX);
            loop {
                let rows = match engine.db.segments_before(before_ms, before_id, 500) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("layout index: could not list recordings: {e:#}");
                        return;
                    }
                };
                if rows.is_empty() {
                    break;
                }
                for (id, row) in rows {
                    before_ms = row.start_ms;
                    before_id = id;
                    check(engine, &row, &mut n);
                }
            }
        }
    }
    crate::ingest::splice::set_checked_through(through);
    if n.added > 0 || n.failed > 0 {
        info!("layout index: added {} older recordings ({} were there, {} unreadable) in {} min", n.added, n.had, n.failed, t0.elapsed().as_secs() / 60);
    }
}

/// At least 95% of the recordings (none at all on a new install counts).
fn index_nearly_full(have: i64, want: i64) -> bool {
    have.saturating_mul(100) >= want.saturating_mul(95)
}

#[derive(Default)]
struct Counts {
    added: u64,
    had: u64,
    failed: u64,
}

fn check(engine: &Arc<Engine>, row: &crate::db::SegmentRow, n: &mut Counts) {
    if !playback::is_native_kind(&row.kind) {
        return;
    }
    wait_for_quiet();
    match crate::ingest::splice::index_recording(&engine.segment_path(row)) {
        Ok(true) => {
            n.added += 1;
            if n.added % 10_000 == 0 {
                info!("layout index: {} older recordings added so far (now at {})", n.added, chrono::DateTime::from_timestamp_millis(row.start_ms).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default());
            }
            // Leave the disk to recording between files.
            thread::sleep(Duration::from_millis(10));
        }
        Ok(false) => n.had += 1,
        Err(_) => n.failed += 1, // removed meanwhile, or damaged
    }
}

#[cfg(test)]
mod tests {
    use super::index_nearly_full;

    /// An entry is dropped only when its file is gone from a folder that is
    /// there, or the file is damaged; a drive or share out of reach keeps it.
    #[test]
    fn only_gone_or_damaged_files_lose_their_entry() {
        let dir = std::env::temp_dir().join(format!("amber-reindex-reach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let io = |k: std::io::ErrorKind| anyhow::Error::from(std::io::Error::from(k)).context("open the file");
        let gone = dir.join("gone.mp4");
        assert!(!super::unreachable(&io(std::io::ErrorKind::NotFound), &gone), "its folder is there: gone");
        let offline = dir.join("no-such-drive").join("x.mp4");
        assert!(super::unreachable(&io(std::io::ErrorKind::NotFound), &offline), "its folder is missing too: out of reach");
        assert!(super::unreachable(&io(std::io::ErrorKind::PermissionDenied), &gone), "locked");
        assert!(super::unreachable(&io(std::io::ErrorKind::TimedOut), &gone), "a share");
        assert!(!super::unreachable(&anyhow::anyhow!("moof without mfhd"), &gone), "damaged");
        assert!(!super::unreachable(&io(std::io::ErrorKind::UnexpectedEof), &gone), "cut short: damaged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_nearly_full_index_is_not_checked_again() {
        assert!(index_nearly_full(0, 0), "a new install");
        assert!(index_nearly_full(846_217, 887_000));
        assert!(index_nearly_full(950, 1000));
        assert!(!index_nearly_full(949, 1000));
        assert!(!index_nearly_full(0, 120_000), "an index made anew: fill it");
        assert!(!index_nearly_full(10, i64::MAX), "count failed: check");
    }
}
