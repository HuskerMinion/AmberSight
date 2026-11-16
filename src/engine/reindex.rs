//! Fills the layout index (`layouts.db`) for recordings made before it
//! existed, newest first, so playing older footage (fast especially) does
//! not have to read each file's headers from the disk while you watch.
//! Recordings closed since are indexed as they close. Works one file at a
//! time and waits while anyone is playing recordings. Once a pass has
//! finished, later starts check only the recordings indexed since.

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
                    check(&engine, &row, &mut n);
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
                    check(&engine, &row, &mut n);
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
