//! Walking the recording folders cheaply.
//!
//! A big site keeps hundreds of thousands of recordings in a camera's folder,
//! and listing one on a spinning drive with a cold cache takes minutes. The
//! start-up indexing therefore skips a camera's folder when nothing in it
//! changed since the last time it was known to be fully indexed:
//!
//! * A folder's own modified time changes when a file is added to it, removed
//!   or renamed (NTFS, ext4 and xfs all do this). It is read with one `stat`
//!   of the folder (never of the files).
//! * `walk_marks` keeps, per camera folder, that time as of the end of the last
//!   complete walk, and as of a clean shutdown (when every recording had been
//!   indexed, see [`Db::refresh_walk_marks`]). An unclean stop leaves the older
//!   time, which no longer matches, so that folder is walked.
//! * A time too close to the walk itself is not kept (the folder could change
//!   within the clock's step after it was read), and every folder is walked in
//!   full at least every [`FULL_WALK_EVERY`].
//! * A camera that records all the time changes its folder every few seconds
//!   and always has a file open, so the walk never sees its folder settled. Such
//!   a folder gets no mark at the walk; it is noted as read in full except the
//!   files left out as possibly open ([`WalkOk::Busy`]). Every later change is
//!   this run's own (its recorders index what they write, the archive move and
//!   retention keep the index up as they go), so at a clean stop the folder is
//!   marked, once each file the walk left out is in the index or gone. One that
//!   is neither (an earlier run's last file, left by a crash) keeps it unmarked.

use super::*;

/// Every folder is read in full at least this often, whatever its time says.
pub const FULL_WALK_EVERY: Duration = Duration::from_secs(7 * 86400);
/// A folder that changed this recently when it was read is not marked (its
/// time may not show a change that follows within the clock's step).
const MARK_MARGIN: Duration = Duration::from_secs(10);

/// How a walk behaves (the defaults are what the server uses; tests shorten them).
#[derive(Debug, Clone, Copy)]
pub struct WalkOpts {
    pub margin: Duration,
    pub full_every: Duration,
}

impl Default for WalkOpts {
    fn default() -> Self {
        WalkOpts { margin: MARK_MARGIN, full_every: FULL_WALK_EVERY }
    }
}

/// What a start-up walk did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WalkStats {
    /// Recordings added to the index.
    pub added: usize,
    /// Files looked at in the folders that were read.
    pub seen: usize,
    /// Camera folders read.
    pub walked: usize,
    /// Camera folders left alone (unchanged since they were last known indexed).
    pub skipped: usize,
}

/// A folder's modified time in ns since the epoch (from one stat of the folder).
pub fn dir_mtime_ns(dir: &Path) -> Option<i64> {
    let t = std::fs::metadata(dir).ok()?.modified().ok()?;
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(d.as_nanos()).ok()
}

/// A camera folder the folder sweep reads in full at least this often (what it looks for, stray
/// files the index never had or could not delete, is rare; recordings the index misses are
/// found by the start-up walk and the recorders themselves).
pub const SWEEP_FULL_EVERY: Duration = Duration::from_secs(7 * 86400);
/// A folder whose time has not moved since a clean full read is left alone this long.
pub const SWEEP_QUIET_FULL_EVERY: Duration = Duration::from_secs(28 * 86400);

/// What the folder sweep does with one camera folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepPlan {
    /// Not read this time.
    Skip,
    /// Read in full (the index adds what is missing, strays are deleted).
    Full,
}

/// Decide what the sweep does with a camera folder. `mark` is (folder time ns, last full read ms,
/// files) of the last clean full read, `mtime` the folder's time now, `wanted` that something
/// asks for a read now (a recording that could not be deleted).
///
/// * No mark (never read, or the last read found something to fix): read.
/// * A read less than `full_every` ago: not read, whatever the folder's time says (a camera that
///   records all the time changes it every few seconds).
/// * Older: read, unless the folder's time is still the one noted at that read (nothing added,
///   removed or renamed in it since, which NTFS, ext4 and xfs all show), and that read was
///   less than `quiet_every` ago.
pub fn sweep_plan(mark: Option<(i64, i64, i64)>, mtime: Option<i64>, now_ms: i64, wanted: bool, full_every: Duration, quiet_every: Duration) -> SweepPlan {
    let Some((marked, full_ms, _)) = mark else { return SweepPlan::Full };
    let age = now_ms - full_ms;
    if wanted || age < 0 {
        return SweepPlan::Full;
    }
    if age < full_every.as_millis() as i64 {
        return SweepPlan::Skip;
    }
    if mtime == Some(marked) && marked >= 0 && age < quiet_every.as_millis() as i64 {
        return SweepPlan::Skip;
    }
    SweepPlan::Full
}

/// How this run's start-up walk left a camera folder (see [`Db::refresh_walk_marks`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkOk {
    /// Marked at the walk (it had settled and nothing in it may have been open), or skipped on
    /// a matching mark.
    Marked,
    /// Read in full and every recording found added, but not marked: it changed just before
    /// the walk, or files were left out as possibly still being written (`open`, paths relative
    /// to the folder root as the index keeps them). Marked at a clean stop once each of those
    /// is indexed or gone.
    Busy { files: i64, full_ms: i64, open: Vec<String> },
}

/// One entry of a recording folder, from the folder listing itself.
#[derive(Debug, Clone)]
pub struct ListedFile {
    pub path: PathBuf,
    pub name: String,
    /// Modified time, ms since the epoch.
    pub mtime_ms: Option<i64>,
    pub bytes: i64,
    pub is_file: bool,
}

/// Every entry of `dir`. Size and time come with the listing (`DirEntry::metadata`: on Windows
/// that is the directory data already read, where `std::fs::metadata(path)` would open each
/// file, a second disk access). `each` runs per entry (to pace the walk). None when the folder
/// cannot be listed.
pub fn list_dir_files(dir: &Path, each: &mut dyn FnMut()) -> Option<Vec<ListedFile>> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut out = Vec::new();
    for e in rd.filter_map(|e| e.ok()) {
        each();
        let name = e.file_name().to_string_lossy().into_owned();
        let meta = e.metadata().ok();
        let mtime_ms = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        out.push(ListedFile {
            path: e.path(),
            name,
            mtime_ms,
            bytes: meta.as_ref().map_or(0, |m| m.len() as i64),
            is_file: meta.as_ref().is_some_and(|m| m.is_file()),
        });
    }
    Some(out)
}

impl Db {
    /// [`Self::import_existing_recordings_with`] with the usual settings.
    pub fn import_existing_recordings(&self, data_dir: &Path, folders: &[(String, PathBuf)], fallback_secs: u64) -> Result<usize> {
        let st = self.import_existing_recordings_with(data_dir, folders, fallback_secs, WalkOpts::default())?;
        Ok(st.added)
    }

    /// Index the recordings already on disk in every folder. The walk holds no lock; what is
    /// found is added a camera at a time in small batches (see `insert_found_segments`), so a
    /// stop half way loses nothing: the next run adds what is left. A camera folder whose time
    /// matches its mark (see the module notes) is not opened at all.
    pub fn import_existing_recordings_with(&self, data_dir: &Path, folders: &[(String, PathBuf)], fallback_secs: u64, opts: WalkOpts) -> Result<WalkStats> {
        let started = std::time::Instant::now();
        let mut st = WalkStats::default();
        self.walk_ok.lock().unwrap().clear();
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
                let seg_dir = cams.join(&slug).join("segments");
                // Read before the folder is listed: a change during the walk then shows next time.
                let mtime = dir_mtime_ns(&seg_dir);
                let now_ms = chrono::Utc::now().timestamp_millis();
                let mark = self.walk_mark(folder, &slug)?;
                if let (Some(m), Some((marked, full_ms, _))) = (mtime, mark) {
                    if m == marked && now_ms - full_ms < opts.full_every.as_millis() as i64 && full_ms <= now_ms {
                        st.skipped += 1;
                        self.walk_ok.lock().unwrap().insert((folder.clone(), slug.clone()), WalkOk::Marked);
                        continue;
                    }
                }
                let (found, open, listed) = scan_segment_dir_ex(folder, root, &slug, fallback_secs, false);
                // (The walk counts what it listed, not just what looked like a recording.)
                let files = found.len();
                st.seen += files;
                st.walked += 1;
                st.added += self.insert_found_segments(&found)?;
                // Marked only when the folder had settled before it was read.
                let settled = mtime.is_some_and(|m| {
                    now_ms - m / 1_000_000 >= opts.margin.as_millis() as i64
                });
                // Nor when a file that may still be being written was left out (a crash's last
                // file, or one a camera is writing): the folder is not fully indexed yet.
                let key = (folder.clone(), slug.clone());
                if let (true, true, true, Some(m)) = (settled, listed, open.is_empty(), mtime) {
                    self.set_walk_mark(folder, &slug, m, files as i64, now_ms)?;
                    self.walk_ok.lock().unwrap().insert(key, WalkOk::Marked);
                } else {
                    self.clear_walk_mark(folder, &slug)?;
                    // A camera recording into it now: marked at a clean stop if what was left
                    // out is indexed by then (see the module notes).
                    if listed && mtime.is_some() {
                        self.walk_ok.lock().unwrap().insert(key, WalkOk::Busy { files: files as i64, full_ms: now_ms, open });
                    }
                }
            }
        }
        let _ = self.drop_dead_walk_marks(data_dir, folders);
        let took = started.elapsed();
        if took.as_secs() >= 5 || st.skipped > 0 {
            tracing::info!(
                "indexing existing recordings: looked at {} files in {} folder(s), {} unchanged folder(s) not read, added {} ({} s)",
                st.seen,
                st.walked,
                st.skipped,
                st.added,
                took.as_secs()
            );
        }
        Ok(st)
    }

    /// (folder time ns, when the folder was last read in full ms, files then) of a camera folder.
    fn walk_mark(&self, folder: &str, camera: &str) -> Result<Option<(i64, i64, i64)>> {
        Ok(self
            .lock_ro()
            .query_row(
                "SELECT dir_mtime_ns, full_ms, files FROM walk_marks WHERE folder = ?1 AND camera = ?2",
                params![folder, camera],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    fn set_walk_mark(&self, folder: &str, camera: &str, mtime_ns: i64, files: i64, full_ms: i64) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO walk_marks (folder, camera, dir_mtime_ns, files, full_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![folder, camera, mtime_ns, files, full_ms],
        )?;
        Ok(())
    }

    fn clear_walk_mark(&self, folder: &str, camera: &str) -> Result<()> {
        self.lock().execute("DELETE FROM walk_marks WHERE folder = ?1 AND camera = ?2", params![folder, camera])?;
        Ok(())
    }

    /// Recordings the last full walks looked at, all folders together (0 when never walked).
    #[cfg(test)]
    pub fn walked_files(&self) -> i64 {
        self.lock_ro().query_row("SELECT COALESCE(SUM(files), 0) FROM walk_marks", [], |r| r.get(0)).unwrap_or(0)
    }

    /// (folder time ns, last full read ms, files) the folder sweep noted for a camera folder.
    pub fn sweep_mark(&self, folder: &str, camera: &str) -> Result<Option<(i64, i64, i64)>> {
        Ok(self
            .lock_ro()
            .query_row(
                "SELECT dir_mtime_ns, full_ms, files FROM sweep_marks WHERE folder = ?1 AND camera = ?2",
                params![folder, camera],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    /// Note a clean full read. `mtime_ns` -1: the folder was not settled or not every file was
    /// in the index, so only the time of the read counts, not the folder's time.
    pub fn set_sweep_mark(&self, folder: &str, camera: &str, mtime_ns: i64, files: i64, full_ms: i64) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO sweep_marks (folder, camera, dir_mtime_ns, files, full_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![folder, camera, mtime_ns, files, full_ms],
        )?;
        Ok(())
    }

    pub fn clear_sweep_mark(&self, folder: &str, camera: &str) -> Result<()> {
        self.lock().execute("DELETE FROM sweep_marks WHERE folder = ?1 AND camera = ?2", params![folder, camera])?;
        Ok(())
    }

    /// Recordings in the index for a camera's segments folder (of one storage folder).
    pub fn indexed_in_folder(&self, folder: &str, slug: &str) -> Result<i64> {
        let prefix = format!("cameras/{slug}/segments/");
        Ok(self.lock_ro().query_row(
            "SELECT COUNT(*) FROM segments WHERE camera = ?1 AND folder = ?2 AND substr(path, 1, length(?3)) = ?3",
            params![slug, folder, prefix],
            |r| r.get(0),
        )?)
    }

    /// Forget marks of camera folders that are gone (a camera removed or renamed). Only for
    /// storage folders that are known: one that is not listed (a drive that is off) keeps its marks.
    pub fn drop_dead_walk_marks(&self, data_dir: &Path, folders: &[(String, PathBuf)]) -> Result<usize> {
        let rows: Vec<(String, String)> = {
            let conn = self.lock_ro();
            let mut st = conn.prepare("SELECT folder, camera FROM walk_marks")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut n = 0;
        for (folder, camera) in rows {
            let root = if folder.is_empty() { Some(data_dir.to_path_buf()) } else { folders.iter().find(|(f, _)| *f == folder).map(|(_, p)| p.clone()) };
            let Some(root) = root else { continue };
            // A root that cannot be read at all (offline) is left alone.
            if std::fs::read_dir(&root).is_err() {
                continue;
            }
            if !root.join("cameras").join(&camera).join("segments").is_dir() {
                self.clear_walk_mark(&folder, &camera)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// At a clean stop, once every recording is indexed: note each folder's time as it stands,
    /// so the next start can leave it alone. Only folders this run's start-up walk read in full
    /// (or skipped on a matching mark) are noted: a folder it did not reach may hold files the
    /// index has not seen. A folder it read while a camera was recording into it
    /// ([`WalkOk::Busy`]) is noted only when each file the walk left out as possibly open is now
    /// in the index or gone: one that is neither is an earlier run's (a crash's last file),
    /// which only a full read adds. Returns how many were noted.
    pub fn refresh_walk_marks(&self, data_dir: &Path, folders: &[(String, PathBuf)]) -> Result<usize> {
        let _ = self.drop_dead_walk_marks(data_dir, folders);
        let ok = self.walk_ok.lock().unwrap().clone();
        let marks: std::collections::HashMap<(String, String), i64> = {
            let conn = self.lock_ro();
            let mut st = conn.prepare("SELECT folder, camera, dir_mtime_ns FROM walk_marks")?;
            let rows = st.query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut n = 0;
        for ((folder, camera), how) in ok {
            let root = if folder.is_empty() { Some(data_dir.to_path_buf()) } else { folders.iter().find(|(f, _)| *f == folder).map(|(_, p)| p.clone()) };
            let Some(root) = root else { continue };
            let Some(now) = dir_mtime_ns(&root.join("cameras").join(&camera).join("segments")) else { continue };
            match how {
                WalkOk::Marked => {
                    // (A mark gone since, by `drop_dead_walk_marks`: its folder is gone.)
                    let Some(&old) = marks.get(&(folder.clone(), camera.clone())) else { continue };
                    if now != old {
                        self.lock().execute("UPDATE walk_marks SET dir_mtime_ns = ?3 WHERE folder = ?1 AND camera = ?2", params![folder, camera, now])?;
                        n += 1;
                    }
                }
                WalkOk::Busy { files, full_ms, open } => {
                    if !self.all_indexed_or_gone(&folder, &root, &open)? {
                        continue;
                    }
                    self.set_walk_mark(&folder, &camera, now, files, full_ms)?;
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// Whether each of these recordings (paths relative to `root`) is in the index under this
    /// folder, or no longer on disk. A file that cannot be looked at counts as there.
    fn all_indexed_or_gone(&self, folder: &str, root: &Path, paths: &[String]) -> Result<bool> {
        for p in paths {
            let indexed: bool = self.lock_ro().query_row("SELECT EXISTS(SELECT 1 FROM segments WHERE path = ?1 AND folder = ?2)", params![p, folder], |r| r.get(0))?;
            if indexed {
                continue;
            }
            match std::fs::symlink_metadata(root.join(p)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Like `import_recent_camera_dirs` for one camera, from a listing already made (the folder
    /// sweep lists each folder once for this and for its cleanup).
    pub fn import_recent_listed(&self, folder: &str, root: &Path, slug: &str, fallback_secs: u64, since_ms: i64, files: &[ListedFile]) -> Result<usize> {
        let found: Vec<FoundSegment> = found_from_listing(folder, root, slug, fallback_secs, files).into_iter().filter(|f| f.end_ms >= since_ms).collect();
        if found.is_empty() {
            return Ok(0);
        }
        self.insert_found_segments(&found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    /// A folder of its own, removed afterwards.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!("amber-walk-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn quick() -> WalkOpts {
        WalkOpts { margin: Duration::ZERO, full_every: FULL_WALK_EVERY }
    }

    /// A camera folder with `n` closed recordings, old enough to be indexed.
    fn make(dir: &Path, cam: &str, n: u32, prefix: &str) -> PathBuf {
        let seg = dir.join("cameras").join(cam).join("segments");
        std::fs::create_dir_all(&seg).unwrap();
        for i in 0..n {
            add(&seg, prefix, i);
        }
        seg
    }

    fn add(seg: &Path, prefix: &str, i: u32) {
        // One base time for every file: with `now()` read per file, the time
        // spent creating the files (a millisecond or more on Windows) lands
        // in the gaps between them, and so in the lengths chained from them.
        static BASE: std::sync::OnceLock<std::time::SystemTime> = std::sync::OnceLock::new();
        let base = *BASE.get_or_init(std::time::SystemTime::now);
        let f = File::create(seg.join(format!("{prefix}-seg{i:06}.mp4"))).unwrap();
        // Old enough to be a finished recording, later numbers later.
        f.set_modified(base - Duration::from_secs(7200 - i as u64 * 60)).unwrap();
    }

    /// The folder's time moves only after a pause (clocks step coarsely).
    fn settle() {
        std::thread::sleep(Duration::from_millis(30));
    }

    #[test]
    fn unchanged_folder_is_not_read_on_the_next_start() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        make(d.path(), "front", 5, "front");
        make(d.path(), "back", 3, "back");
        let a = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((a.added, a.walked, a.skipped, a.seen), (8, 2, 0, 8));
        assert_eq!(db.walked_files(), 8);
        // Same folders, a new start: nothing is opened.
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.added, b.walked, b.skipped, b.seen), (0, 0, 2, 0));
    }

    #[test]
    fn a_new_file_in_an_old_folder_is_found() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 4, "front");
        db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        settle();
        add(&seg, "front", 40);
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.added, b.walked, b.skipped), (1, 1, 0));
        // And it is then left alone again.
        let c = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((c.added, c.walked, c.skipped), (0, 0, 1));
    }

    #[test]
    fn a_removed_or_renamed_file_makes_the_folder_be_read() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 4, "front");
        db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        settle();
        std::fs::remove_file(seg.join("front-seg000001.mp4")).unwrap();
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.walked, b.skipped), (1, 0), "a removed file shows in the folder's time");
        settle();
        std::fs::rename(seg.join("front-seg000002.mp4"), seg.join("front-seg000052.mp4")).unwrap();
        let c = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((c.added, c.walked, c.skipped), (1, 1, 0), "a renamed one is found under its new name");
    }

    #[test]
    fn a_folder_that_just_changed_is_not_marked() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        make(d.path(), "front", 3, "front");
        // The default margin: the folder changed a moment ago, so it is read again next time.
        db.import_existing_recordings_with(d.path(), &[], 60, WalkOpts::default()).unwrap();
        let b = db.import_existing_recordings_with(d.path(), &[], 60, WalkOpts::default()).unwrap();
        assert_eq!((b.walked, b.skipped), (1, 0));
    }

    #[test]
    fn a_full_walk_comes_round_even_when_nothing_changed() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        make(d.path(), "front", 3, "front");
        db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        // The mark says the last full walk was eight days ago.
        db.lock().execute("UPDATE walk_marks SET full_ms = full_ms - 8 * 86400000", []).unwrap();
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.walked, b.skipped), (1, 0));
        let c = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((c.walked, c.skipped), (0, 1));
    }

    #[test]
    fn a_clean_stop_notes_the_folder_so_the_next_start_skips_it() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 3, "front");
        db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        settle();
        // Recording went on after the walk (and the indexer kept up).
        add(&seg, "front", 20);
        let rows = [FoundSegment { camera: "front".into(), path: "cameras/front/segments/front-seg000020.mp4".into(), start_ms: 1, end_ms: 2, bytes: 0, folder: String::new(), kind: "legacy" }];
        db.insert_found_segments(&rows).unwrap();
        // Without the note the folder would be read; with it, left alone.
        assert_eq!(db.refresh_walk_marks(d.path(), &[]).unwrap(), 1);
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.walked, b.skipped), (0, 1));
        // A crash (no note) leaves the older time, so the folder is read.
        settle();
        add(&seg, "front", 21);
        let c = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((c.added, c.walked), (1, 1));
    }

    #[test]
    fn a_folder_never_walked_gets_no_note_at_a_clean_stop() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        make(d.path(), "front", 3, "front");
        assert_eq!(db.refresh_walk_marks(d.path(), &[]).unwrap(), 0);
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.walked, b.skipped), (1, 0));
    }

    #[test]
    fn a_folder_with_a_file_that_may_be_open_is_not_marked() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 3, "front");
        // A file written just now (a crash's last file, or an ffmpeg camera's open one).
        File::create(seg.join("front-seg000030.mp4")).unwrap();
        let a = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!(a.added, 3);
        assert_eq!(db.refresh_walk_marks(d.path(), &[]).unwrap(), 0);
        // Next start: the folder is read again and the file, closed by now, is added.
        let f = File::options().write(true).open(seg.join("front-seg000030.mp4")).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(600)).unwrap();
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.added, b.walked, b.skipped), (1, 1, 0));
    }

    #[test]
    fn a_clean_stop_does_not_note_a_folder_this_run_did_not_walk() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 3, "front");
        db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        settle();
        // A later start whose walk never got to this folder (an error before it).
        db.walk_ok.lock().unwrap().clear();
        add(&seg, "front", 40);
        assert_eq!(db.refresh_walk_marks(d.path(), &[]).unwrap(), 0, "the old mark must stay");
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.added, b.walked), (1, 1));
    }

    #[test]
    fn the_final_scan_at_a_stop_adds_the_file_the_15_s_scan_leaves_out() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 2, "front");
        File::create(seg.join("front-seg000030.mp4")).unwrap();
        let dirs = vec![("front".to_string(), String::new(), d.path().to_path_buf())];
        let since = chrono::Utc::now().timestamp_millis() - 86_400_000;
        assert_eq!(db.import_recent_camera_dirs(&dirs, 60, since).unwrap(), 2, "the open one waits");
        assert_eq!(db.import_final_camera_dirs(&dirs, 60, since).unwrap(), 1, "the stop's scan takes it");
    }

    #[test]
    fn the_longest_recording_shrinks_again_once_a_bogus_row_is_gone() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        make(d.path(), "front", 3, "front");
        db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        let row = |path: &str, len: i64| FoundSegment { camera: "front".into(), path: path.into(), start_ms: 1_000_000, end_ms: 1_000_000 + len, bytes: 0, folder: String::new(), kind: "legacy" };
        db.insert_found_segments(&[row("cameras/front/segments/bogus.mp4", 9 * 3_600_000)]).unwrap();
        assert_eq!(db.longest_segment.load(std::sync::atomic::Ordering::Relaxed), 9 * 3_600_000);
        // Still there: stays.
        assert_eq!(db.recompute_longest_segment().unwrap().1, 9 * 3_600_000);
        db.lock().execute("DELETE FROM segments WHERE path LIKE '%bogus.mp4'", []).unwrap();
        let (before, after) = db.recompute_longest_segment().unwrap();
        assert_eq!(before, 9 * 3_600_000);
        assert_eq!(after, 60_000, "the longest left is the 60 s fallback length");
        assert_eq!(db.longest_segment.load(std::sync::atomic::Ordering::Relaxed), 60_000);
        let stored: i64 = db.lock_ro().query_row("SELECT ms FROM seg_longest WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(stored, 60_000);
    }

    /// A recording this run's camera writes now: named like the others, dated now.
    fn record(seg: &Path, prefix: &str, i: u32) -> String {
        File::create(seg.join(format!("{prefix}-seg{i:06}.mp4"))).unwrap();
        let cam = seg.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned();
        format!("cameras/{cam}/segments/{prefix}-seg{i:06}.mp4")
    }

    /// What the recorder (or the 15 s scan) adds once it closes a file.
    fn index(db: &Db, cam: &str, folder: &str, path: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        db.insert_found_segments(&[FoundSegment { camera: cam.into(), path: path.into(), start_ms: now - 10_000, end_ms: now, bytes: 0, folder: folder.into(), kind: "legacy" }]).unwrap();
    }

    fn indexed(db: &Db, path: &str) -> Option<String> {
        db.lock_ro().query_row("SELECT folder FROM segments WHERE path = ?1", params![path], |r| r.get(0)).optional().unwrap()
    }

    #[test]
    fn a_folder_a_camera_records_into_during_the_walk_is_marked_at_a_clean_stop() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 5, "front");
        // The camera is recording: a file it is writing as the walk runs (just changed, so not
        // settled either, with the usual margin).
        let open = record(&seg, "front", 10);
        let a = db.import_existing_recordings_with(d.path(), &[], 60, WalkOpts::default()).unwrap();
        assert_eq!((a.added, a.walked), (5, 1), "the open one is left to the recorder");
        assert_eq!(db.walk_mark("", "front").unwrap(), None, "no mark while it is being written to");
        // Recording goes on: the open file is closed and indexed, more come and are indexed.
        index(&db, "front", "", &open);
        for i in 11..14 {
            let p = record(&seg, "front", i);
            index(&db, "front", "", &p);
        }
        // A clean stop marks it; the next start leaves it alone.
        assert_eq!(db.refresh_walk_marks(d.path(), &[]).unwrap(), 1);
        let b = db.import_existing_recordings_with(d.path(), &[], 60, WalkOpts::default()).unwrap();
        assert_eq!((b.walked, b.skipped), (0, 1));
        // And the mark holds the files the walk counted, and when it read them.
        let (_, full_ms, files) = db.walk_mark("", "front").unwrap().unwrap();
        assert_eq!(files, 5, "the five it added (the open one was left to the recorder)");
        assert!(chrono::Utc::now().timestamp_millis() - full_ms < 60_000);
    }

    #[test]
    fn a_busy_folder_is_not_marked_when_the_stop_was_not_clean() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 3, "front");
        let open = record(&seg, "front", 10);
        db.import_existing_recordings_with(d.path(), &[], 60, WalkOpts::default()).unwrap();
        index(&db, "front", "", &open);
        // A crash: no refresh. The next start reads it in full.
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.walked, b.skipped), (1, 0));
    }

    #[test]
    fn a_crash_leftover_still_keeps_a_busy_folder_unmarked() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 3, "front");
        // The last file of a run that crashed a moment before this start: recent, not indexed,
        // and no recorder of this run will index it.
        let leftover = seg.join("front-seg000003.mp4");
        File::create(&leftover).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(5)).unwrap();
        let a = db.import_existing_recordings_with(d.path(), &[], 60, WalkOpts::default()).unwrap();
        assert_eq!(a.added, 3);
        // This run's camera records and indexes its own files.
        for i in 10..13 {
            let p = record(&seg, "front", i);
            index(&db, "front", "", &p);
        }
        assert_eq!(db.refresh_walk_marks(d.path(), &[]).unwrap(), 0, "the leftover is not in the index");
        // The next start, a while later.
        File::options().write(true).open(&leftover).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(600)).unwrap();
        let b =db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.walked, b.skipped), (1, 0));
        assert_eq!(indexed(&db, "cameras/front/segments/front-seg000003.mp4").as_deref(), Some(""), "a full read adds it");
    }

    #[test]
    fn a_crash_leftover_indexed_during_the_run_or_deleted_does_not_keep_it_unmarked() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 3, "front");
        let seg2 = make(d.path(), "back", 3, "back");
        let old = std::time::SystemTime::now() - Duration::from_secs(5);
        File::create(seg.join("front-seg000003.mp4")).unwrap().set_modified(old).unwrap();
        File::create(seg2.join("back-seg000003.mp4")).unwrap().set_modified(old).unwrap();
        db.import_existing_recordings_with(d.path(), &[], 60, WalkOpts::default()).unwrap();
        // The folder sweep (or the 15 s scan) added one; the other was deleted.
        index(&db, "front", "", "cameras/front/segments/front-seg000003.mp4");
        std::fs::remove_file(seg2.join("back-seg000003.mp4")).unwrap();
        assert_eq!(db.refresh_walk_marks(d.path(), &[]).unwrap(), 2);
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.walked, b.skipped), (0, 2));
    }

    #[test]
    fn a_left_out_file_indexed_under_another_folder_does_not_count() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 3, "front");
        let open = record(&seg, "front", 10);
        db.import_existing_recordings_with(d.path(), &[], 60, WalkOpts::default()).unwrap();
        index(&db, "front", "elsewhere", &open);
        assert_eq!(db.refresh_walk_marks(d.path(), &[]).unwrap(), 0);
    }

    #[test]
    fn retention_and_archive_moves_during_the_run_leave_both_folders_right() {
        let d = Tmp::new();
        let e = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 6, "front");
        make(e.path(), "front", 2, "stored");
        let folders = vec![("stored".to_string(), e.path().to_path_buf())];
        let open = record(&seg, "front", 20);
        let a = db.import_existing_recordings_with(d.path(), &folders, 60, WalkOpts::default()).unwrap();
        assert_eq!((a.added, a.walked), (8, 2));
        index(&db, "front", "", &open);
        // Retention deletes the oldest (the file, then its row, as retention_pass does).
        std::fs::remove_file(seg.join("front-seg000000.mp4")).unwrap();
        assert!(db.delete_segment("cameras/front/segments/front-seg000000.mp4").unwrap());
        // The cap moves the next two to the archive (as retire_segment does: rename, then the row).
        for i in 1..3 {
            let rel = format!("cameras/front/segments/front-seg{i:06}.mp4");
            std::fs::rename(d.path().join(&rel), e.path().join(&rel)).unwrap();
            assert!(db.set_segment_folder(&rel, "stored").unwrap());
        }
        // More recording.
        let p = record(&seg, "front", 21);
        index(&db, "front", "", &p);
        assert_eq!(db.refresh_walk_marks(d.path(), &folders).unwrap(), 2);
        let b = db.import_existing_recordings_with(d.path(), &folders, 60, WalkOpts::default()).unwrap();
        assert_eq!((b.walked, b.skipped, b.added), (0, 2, 0));
        // The index matches the disk: what moved is under the archive, what went is gone.
        assert_eq!(indexed(&db, "cameras/front/segments/front-seg000000.mp4"), None);
        assert_eq!(indexed(&db, "cameras/front/segments/front-seg000001.mp4").as_deref(), Some("stored"));
        assert_eq!(indexed(&db, "cameras/front/segments/front-seg000002.mp4").as_deref(), Some("stored"));
        // A full read of both finds nothing the index did not have.
        db.lock().execute("UPDATE walk_marks SET full_ms = full_ms - 8 * 86400000", []).unwrap();
        let c = db.import_existing_recordings_with(d.path(), &folders, 60, quick()).unwrap();
        assert_eq!((c.walked, c.added), (2, 0));
    }

    #[test]
    fn a_busy_folder_gone_by_the_stop_gets_no_mark() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        let seg = make(d.path(), "front", 3, "front");
        record(&seg, "front", 10);
        db.import_existing_recordings_with(d.path(), &[], 60, WalkOpts::default()).unwrap();
        std::fs::remove_dir_all(d.path().join("cameras").join("front")).unwrap();
        assert_eq!(db.refresh_walk_marks(d.path(), &[]).unwrap(), 0);
        assert_eq!(db.walked_files(), 0);
    }

    #[test]
    fn marks_of_folders_that_are_gone_are_dropped() {
        let d = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        make(d.path(), "front", 3, "front");
        make(d.path(), "old", 2, "old");
        db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!(db.walked_files(), 5);
        std::fs::remove_dir_all(d.path().join("cameras").join("old")).unwrap();
        db.refresh_walk_marks(d.path(), &[]).unwrap();
        assert_eq!(db.walked_files(), 3);
        // A storage folder that is not listed (a drive that is off) keeps its marks.
        db.set_walk_mark("away", "cam", 1, 9, 1).unwrap();
        db.refresh_walk_marks(d.path(), &[]).unwrap();
        assert_eq!(db.walked_files(), 12);
    }

    #[test]
    fn extra_recording_folders_are_marked_too() {
        let d = Tmp::new();
        let e = Tmp::new();
        let db = Db::open_in_memory().unwrap();
        make(d.path(), "front", 2, "front");
        make(e.path(), "back", 2, "back");
        let folders = vec![("big".to_string(), e.path().to_path_buf())];
        let a = db.import_existing_recordings_with(d.path(), &folders, 60, quick()).unwrap();
        assert_eq!((a.walked, a.added), (2, 4));
        let b = db.import_existing_recordings_with(d.path(), &folders, 60, quick()).unwrap();
        assert_eq!((b.walked, b.skipped), (0, 2));
    }

    #[test]
    fn the_listing_gives_size_and_time_without_opening_files() {
        let d = Tmp::new();
        let seg = make(d.path(), "front", 3, "front");
        std::fs::create_dir(seg.join("sub")).unwrap();
        let mut n = 0;
        let l = list_dir_files(&seg, &mut || n += 1).unwrap();
        assert_eq!((l.len(), n), (4, 4));
        assert_eq!(l.iter().filter(|f| f.is_file && f.mtime_ms.is_some()).count(), 3);
        assert!(list_dir_files(&seg.join("nope"), &mut || {}).is_none());
    }

    const DAY_MS: i64 = 86_400_000;
    const NOW: i64 = 100 * DAY_MS;

    fn sweep_of(mark: Option<(i64, i64, i64)>, mtime: Option<i64>, wanted: bool) -> SweepPlan {
        sweep_plan(mark, mtime, NOW, wanted, SWEEP_FULL_EVERY, SWEEP_QUIET_FULL_EVERY)
    }

    #[test]
    fn sweep_reads_a_folder_with_no_mark() {
        assert_eq!(sweep_of(None, Some(5), false), SweepPlan::Full);
    }

    #[test]
    fn sweep_leaves_a_folder_read_lately_alone_even_when_its_time_moved() {
        // Read 6 h ago; the folder's time is not the noted one (a camera records into it).
        assert_eq!(sweep_of(Some((-1, NOW - 6 * 3_600_000, 9)), Some(777), false), SweepPlan::Skip);
        assert_eq!(sweep_of(Some((1, NOW - 6 * DAY_MS, 9)), Some(2), false), SweepPlan::Skip);
    }

    #[test]
    fn sweep_reads_a_folder_a_week_after_the_last_read() {
        assert_eq!(sweep_of(Some((-1, NOW - 7 * DAY_MS, 9)), Some(777), false), SweepPlan::Full);
        // Its time moved since the noted one.
        assert_eq!(sweep_of(Some((1, NOW - 8 * DAY_MS, 9)), Some(2), false), SweepPlan::Full);
    }

    #[test]
    fn sweep_leaves_an_untouched_folder_alone_for_four_weeks() {
        assert_eq!(sweep_of(Some((5, NOW - 8 * DAY_MS, 9)), Some(5), false), SweepPlan::Skip);
        assert_eq!(sweep_of(Some((5, NOW - 27 * DAY_MS, 9)), Some(5), false), SweepPlan::Skip);
        assert_eq!(sweep_of(Some((5, NOW - 28 * DAY_MS, 9)), Some(5), false), SweepPlan::Full);
        // A note of -1 (not settled, or not all indexed) never matches.
        assert_eq!(sweep_of(Some((-1, NOW - 8 * DAY_MS, 9)), Some(-1), false), SweepPlan::Full);
        // Folder time unreadable.
        assert_eq!(sweep_of(Some((5, NOW - 8 * DAY_MS, 9)), None, false), SweepPlan::Full);
    }

    #[test]
    fn sweep_reads_everything_when_asked_or_when_the_clock_went_back() {
        assert_eq!(sweep_of(Some((5, NOW - 1000, 9)), Some(5), true), SweepPlan::Full);
        assert_eq!(sweep_of(Some((5, NOW + 1000, 9)), Some(5), false), SweepPlan::Full);
    }

    #[test]
    fn sweep_marks_are_kept_and_cleared() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.sweep_mark("", "cam").unwrap(), None);
        db.set_sweep_mark("", "cam", 5, 7, 99).unwrap();
        assert_eq!(db.sweep_mark("", "cam").unwrap(), Some((5, 99, 7)));
        db.clear_sweep_mark("", "cam").unwrap();
        assert_eq!(db.sweep_mark("", "cam").unwrap(), None);
    }

    /// The mark lives in the database, so a new database opened on the same file still has it.
    #[test]
    fn marks_survive_reopening_the_database() {
        let d = Tmp::new();
        make(d.path(), "front", 3, "front");
        let path = d.path().join("a.db");
        {
            let db = Db::open(&path).unwrap();
            db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        }
        let db = Db::open(&path).unwrap();
        let b = db.import_existing_recordings_with(d.path(), &[], 60, quick()).unwrap();
        assert_eq!((b.added, b.walked, b.skipped), (0, 0, 1));
    }

    fn plan(db: &Db, sql: &str) -> Vec<String> {
        let conn = db.lock();
        let mut st = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = st.query_map([], |r| r.get::<_, String>(3)).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    #[test]
    fn calendar_queries_use_an_index() {
        let db = Db::open_in_memory().unwrap();
        for sql in [
            "SELECT COUNT(*) FROM (SELECT 1 FROM events WHERE camera = 'a' COLLATE NOCASE AND start >= 1 AND start < 2 LIMIT 100)",
            "SELECT 1 FROM segments WHERE camera = 'a' AND start_ms >= 1 AND start_ms < 2 LIMIT 1",
            "SELECT end_ms FROM segments WHERE camera = 'a' AND start_ms < 2 ORDER BY start_ms DESC LIMIT 1",
            "SELECT MIN(start_ms) FROM segments WHERE camera = 'a'",
        ] {
            let p = plan(&db, sql);
            // (Never a scan of the table itself; the `SCAN (subquery..)` of a COUNT is the few rows it gives.)
            assert!(p.iter().all(|l| !(l.starts_with("SCAN events") || l.starts_with("SCAN segments")) || l.contains("USING")), "{sql}: {p:?}");
        }
    }

    #[test]
    fn longest_recording_is_kept_by_the_database_not_found_at_every_open() {
        let d = Tmp::new();
        let path = d.path().join("a.db");
        {
            let db = Db::open(&path).unwrap();
            let rows = [
                FoundSegment { camera: "c".into(), path: "a".into(), start_ms: 0, end_ms: 60_000, bytes: 1, folder: String::new(), kind: "legacy" },
                FoundSegment { camera: "c".into(), path: "b".into(), start_ms: 100_000, end_ms: 400_000, bytes: 1, folder: String::new(), kind: "legacy" },
            ];
            db.insert_found_segments(&rows).unwrap();
            db.lock().execute("UPDATE segments SET end_ms = 100_000 + 500_000 WHERE path = 'b'", []).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.longest_segment.load(std::sync::atomic::Ordering::Relaxed), 500_000);
        // A database that never had it (made before) gets it counted once.
        db.lock().execute_batch("DROP TRIGGER seg_longest_ins; DROP TRIGGER seg_longest_upd; DROP TABLE seg_longest;").unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.longest_segment.load(std::sync::atomic::Ordering::Relaxed), 500_000);
    }

    #[test]
    fn smart_prune_drops_only_pictures_of_deleted_events_in_batches() {
        let db = Db::open_in_memory().unwrap();
        {
            let conn = db.lock();
            for i in 0..(SMART_PRUNE_BATCH * 2 + 30) {
                let alive = i % 3 == 0;
                if alive {
                    conn.execute("INSERT INTO events (id, camera_id, camera, start, end, score) VALUES (?1, 0, 'a', 1, 2, 0.5)", params![format!("e{i}")]).unwrap();
                }
                conn.execute("INSERT INTO smart_vecs (event_id, camera, at_ms, src, emb) VALUES (?1, 'a', 1, 'thumb', x'00')", params![format!("e{i}")]).unwrap();
                conn.execute("INSERT INTO smart_done (event_id) VALUES (?1)", params![format!("e{i}")]).unwrap();
            }
        }
        let total = SMART_PRUNE_BATCH * 2 + 30;
        let alive = (0..total).filter(|i| i % 3 == 0).count();
        assert_eq!(db.smart_prune().unwrap(), total - alive);
        let count = |t: &str| -> usize { db.lock().query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap() as usize };
        assert_eq!((count("smart_vecs"), count("smart_done")), (alive, alive));
        assert_eq!(db.smart_prune().unwrap(), 0);
    }
}
