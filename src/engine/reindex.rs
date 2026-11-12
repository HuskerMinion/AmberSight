//! Fills the layout index (`layouts.db`) for recordings made before it
//! existed, newest first, so playing older footage (fast especially) does
//! not have to read each file's headers from the disk while you watch.
//! Recordings closed since are indexed as they close. Works one file at a
//! time and waits while anyone is playing recordings; once a start finds
//! nothing left to do it costs one quick check per file.

use super::*;
use std::time::Duration;

/// Wait this long after the last playback request before reading files.
const QUIET_SECS: i64 = 30;

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
    let t0 = std::time::Instant::now();
    let (mut before_ms, mut before_id) = (i64::MAX, i64::MAX);
    let (mut added, mut had, mut failed) = (0u64, 0u64, 0u64);
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
            if !playback::is_native_kind(&row.kind) {
                continue;
            }
            wait_for_quiet();
            match crate::ingest::splice::index_recording(&engine.segment_path(&row)) {
                Ok(true) => {
                    added += 1;
                    if added % 10_000 == 0 {
                        info!("layout index: {added} older recordings added so far (now at {})", chrono::DateTime::from_timestamp_millis(row.start_ms).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default());
                    }
                    // Leave the disk to recording between files.
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(false) => had += 1,
                Err(_) => failed += 1, // removed meanwhile, or damaged
            }
        }
    }
    if added > 0 || failed > 0 {
        info!("layout index: added {added} older recordings ({had} were there, {failed} unreadable) in {} min", t0.elapsed().as_secs() / 60);
    }
}
