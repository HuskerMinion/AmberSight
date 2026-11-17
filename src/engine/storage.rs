use super::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

impl Engine {
    /// Disk usage of the data folder. Walking it is slow (hundreds of
    /// thousands of thumbnails and AI pictures, on the disk the cameras
    /// record to), so the last answer is used and a fresh one is made in the
    /// background at most every 10 minutes. Only the first call waits.
    pub fn storage_usage(self: &Arc<Engine>) -> serde_json::Value {
        static CACHE: Mutex<Option<(std::time::Instant, serde_json::Value)>> = Mutex::new(None);
        static REFRESHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        const FRESH: std::time::Duration = std::time::Duration::from_secs(600);
        let cached = CACHE.lock().unwrap().clone();
        match cached {
            Some((t, v)) => {
                if t.elapsed() >= FRESH && !REFRESHING.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let engine = self.clone();
                    let spawned = std::thread::Builder::new().name("storage-usage".into()).spawn(move || {
                        let _running = super::storage::ClearOnDrop(&REFRESHING);
                        let v = engine.storage_usage_scan();
                        *CACHE.lock().unwrap() = Some((std::time::Instant::now(), v));
                    });
                    if spawned.is_err() {
                        REFRESHING.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                v
            }
            None => {
                // One first walk at a time: callers that arrive meanwhile
                // wait for it instead of walking the folder again.
                static FIRST: Mutex<()> = Mutex::new(());
                let _one = FIRST.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((_, v)) = CACHE.lock().unwrap().clone() {
                    return v;
                }
                let v = self.storage_usage_scan();
                *CACHE.lock().unwrap() = Some((std::time::Instant::now(), v.clone()));
                v
            }
        }
    }

    fn storage_usage_scan(self: &Arc<Engine>) -> serde_json::Value {
        let mut bytes = 0u64;
        // Everything but recordings (cameras/<x>/segments), which the
        // database counts exactly and at once.
        let mut other_bytes = 0u64;
        let mut clips = 0usize;
        let mut segments = 0usize;
        let mut stack = vec![(self.data_dir.clone(), false)];
        let clips_dir = self.data_dir.join("clips");
        while let Some((dir, in_recordings)) = stack.pop() {
            let entries = std::fs::read_dir(&dir).ok();
            for entry in entries.into_iter().flat_map(|it| it.flatten()) {
                // Type and size from the folder listing itself: on Windows
                // asking the file again opens it, a second disk access each.
                let Ok(meta) = entry.metadata() else { continue };
                let path = entry.path();
                if meta.is_dir() {
                    let recordings = in_recordings
                        || (entry.file_name() == "segments"
                            && dir.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "cameras"));
                    stack.push((path, recordings));
                    continue;
                }
                bytes += meta.len();
                if !in_recordings {
                    other_bytes += meta.len();
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".mp4") {
                    if path.starts_with(&clips_dir) {
                        clips += 1;
                    } else if name.contains("-seg") {
                        segments += 1;
                    }
                }
            }
        }
        serde_json::json!({
            "bytes": bytes,
            "other_bytes": other_bytes,
            "clips": clips,
            "segments": segments,
            // Bytes of recordings in the index (finished files).
            "indexed_bytes": self.db.total_segment_bytes().unwrap_or(0),
            // Kept bookmarks and what they hold back from cleanup.
            "kept_bookmarks": self.db.bookmark_kept_summary().map(|(n, b)| serde_json::json!({ "count": n, "bytes": b })).unwrap_or(serde_json::Value::Null),
            // Extra recording folders with their indexed usage.
            "folders": self.config.lock().unwrap().storage.folders.iter().map(|f| serde_json::json!({
                "name": f.name,
                "path": f.path,
                "max_mb": f.max_mb,
                "archive": f.archive,
                "bytes": self.db.folder_bytes(&f.name).unwrap_or(0),
            })).collect::<Vec<_>>(),
        })
    }
}

/// One recording folder ("" = the data folder) with the cap that applies to
/// it now: its fixed max_mb, or the automatic cap worked out from its drive.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FolderCap {
    pub name: String,
    pub path: std::path::PathBuf,
    pub archive: bool,
    /// max_mb = 0: the cap follows the drive's free space.
    pub auto: bool,
    /// Bytes allowed (0 = none could be worked out: drive not readable).
    pub cap: u64,
    /// Bytes the folder's recordings take (what the cap is compared with).
    pub used: u64,
    pub total: Option<u64>,
    pub free: Option<u64>,
    /// Free space the drive is meant to keep (automatic folders aim for it;
    /// for fixed caps it is when "Low space" shows).
    pub target_free: u64,
    /// Automatic folders over their cap prune this far below it, so the next
    /// few minutes of recording do not start another round at once.
    pub margin: u64,
}

/// What the free space of one drive means for one folder on it, shown in the
/// status panel, the health report and the Storage pages.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StorageLevel {
    /// "" (nothing to say), "info", "warn" or "crit".
    pub level: &'static str,
    pub label: String,
    /// A sentence for the health report / tooltips.
    pub detail: String,
}

/// Free space a drive keeps under automatic folders: `pct` percent of it,
/// never less than the hard reserve, never more than the drive.
pub fn target_free(total: u64, pct: u64) -> u64 {
    ((total as u128 * pct.min(90) as u128 / 100) as u64).max(reserve_bytes(total)).min(total)
}

/// How far below an automatic cap pruning goes: 0.5% of the drive, 256 MB
/// to 10 GB (never more than a tenth of the cap, see storage_caps).
pub fn auto_margin(total: u64) -> u64 {
    (total / 200).clamp(256 << 20, 10 << 30)
}

/// One folder as the automatic cap sees it.
#[derive(Debug, Clone, Default)]
pub struct RootSpace {
    /// Which drive it is on (None: unknown, treated as a drive of its own).
    pub drive: Option<String>,
    pub used: u64,
    pub auto: bool,
    pub total: Option<u64>,
    pub free: Option<u64>,
}

/// Automatic caps (None for fixed folders and drives that cannot be read).
///
/// An automatic folder may hold what it holds now plus the drive's free
/// space above the target. Automatic folders on the same drive share that:
/// spare room is split evenly, a shortfall is taken from each in proportion
/// to what it holds, so together they never claim the same free space twice
/// nor prune more than the shortfall. Never below 0. Fixed folders on the
/// drive just count as other files.
pub fn auto_caps(roots: &[RootSpace], pct: u64) -> Vec<Option<u64>> {
    let mut out = vec![None; roots.len()];
    for (i, r) in roots.iter().enumerate() {
        if !r.auto || out[i].is_some() {
            continue;
        }
        let (Some(total), Some(free)) = (r.total, r.free) else { continue };
        let members: Vec<usize> = match &r.drive {
            Some(d) => roots.iter().enumerate().filter(|(_, o)| o.auto && o.drive.as_ref() == Some(d)).map(|(j, _)| j).collect(),
            None => vec![i],
        };
        let spare = free as i128 - target_free(total, pct) as i128;
        let n = members.len() as i128;
        let sum_used: i128 = members.iter().map(|&j| roots[j].used as i128).sum();
        for &j in &members {
            let used = roots[j].used as i128;
            let cap = if spare >= 0 {
                used + spare / n
            } else if sum_used > 0 {
                used + spare * used / sum_used
            } else {
                0
            };
            out[j] = Some(cap.max(0) as u64);
        }
    }
    out
}

/// The status of one folder (see StorageLevel). `short`: the last pass could
/// not free what it needed (nothing left to delete, or files that would not
/// go); `days_kept`: age of its oldest recording.
pub fn storage_level(c: &FolderCap, short: bool, rolls: bool, days_kept: Option<f64>, retention_days: u64) -> StorageLevel {
    let lv = |level, label: &str, detail: String| StorageLevel { level, label: label.into(), detail };
    let gbs = |b: u64| format!("{:.1} GB", b as f64 / 1e9);
    let (Some(total), Some(free)) = (c.total, c.free) else { return StorageLevel::default() };
    if total == 0 {
        return StorageLevel::default();
    }
    if (free as u128) * 50 < total as u128 {
        return lv("crit", "Drive nearly full", format!("the drive is nearly full: {} free of {}", gbs(free), gbs(total)));
    }
    if !c.auto {
        // Fixed caps: pruning to the cap cannot help here; warned as before
        // (under 5% or 5 GB free).
        if (free as u128) * 20 < total as u128 || free < 5 << 30 {
            return lv("warn", "Low space", format!("low disk space: {} free of {} (other files, or a cap larger than the drive)", gbs(free), gbs(total)));
        }
    } else if free.saturating_add(2 * c.margin) < c.target_free {
        // (Recording between passes dips under the target by a little:
        // that is the steady state, not falling behind.)
        if short || free.saturating_add(c.used) < c.target_free {
            return lv("crit", "Can't free space", format!("only {} free and deleting recordings cannot free more: other files fill the drive or the folder cannot be written", gbs(free)));
        }
        return lv("warn", "Freeing space", format!("{} free, below the {} to keep; deleting the oldest recordings", gbs(free), gbs(c.target_free)));
    }
    let at_cap = c.cap > 0 && if c.auto { c.used.saturating_add(2 * c.margin) >= c.cap } else { c.used as u128 * 100 >= c.cap as u128 * 95 };
    if !at_cap {
        return StorageLevel::default();
    }
    if short && !c.auto && c.used > c.cap {
        // No recordings in it at all (cameras record to other folders): the
        // space is event clips and pictures, which go as events expire, not
        // something deleting recordings can fix. Not a fault.
        if days_kept.is_none() {
            return lv("warn", "Over its limit", format!("{} used of its {} limit, but it holds no recordings to delete: the space is event clips and pictures, which are removed as events expire. Raise its limit (Settings > Recording) or keep events fewer days", gbs(c.used), gbs(c.cap)));
        }
        return lv("crit", "Can't free space", "over its limit and the oldest recordings could not be deleted (files in use, or the folder cannot be written)".into());
    }
    let kept = match days_kept {
        Some(d) if d >= 2.0 => format!("keeping {} days", d.floor() as u64),
        Some(d) if d >= 1.0 => "keeping 1 day".into(),
        Some(d) if d * 24.0 >= 1.0 => format!("keeping {} h", (d * 24.0).floor() as u64),
        Some(d) => format!("keeping {} min", (d * 1440.0).floor().max(1.0) as u64),
        None => String::new(),
    };
    let where_ = if rolls { "; older recordings move to the archive" } else { "" };
    if days_kept.is_some_and(|d| d < 1.0) && retention_days > 1 {
        return lv("warn", &format!("Full · {kept}"), format!("the drive is full, so it holds less than a day of recordings (you asked for {retention_days} days){where_}; keep fewer days, choose a bigger drive, or record less"));
    }
    // A folder that hands its oldest recordings on to an archive keeps only part of the
    // days: say so, so it does not read as all the days kept.
    let label = match (kept.is_empty(), rolls) {
        (true, _) => "Full".to_string(),
        (false, false) => format!("Full · {kept}"),
        (false, true) => format!("Full · {kept} + archive"),
    };
    lv("info", &label, format!("full as intended: the oldest recordings are replaced as new ones arrive{where_}"))
}

/// Folders whose last retention pass could not free what it needed.
static SHORT: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn set_short(names: HashSet<String>) {
    *SHORT.lock().unwrap() = Some(names);
}

/// Whether the last pass came up short for folder `name` ("" = data folder).
pub fn was_short(name: &str) -> bool {
    SHORT.lock().unwrap().as_ref().is_some_and(|s| s.contains(name))
}

impl Engine {
    /// Bytes in the data folder that count against its cap: everything in it
    /// except other recording folders mounted inside and time-lapse videos.
    fn data_folder_used(self: &Arc<Engine>) -> u64 {
        // Recordings in the data folder from the database (exact, now); the
        // rest (pictures, clips) from the last walk. Recordings of other
        // folders mounted inside it are not counted: the walk leaves out
        // every cameras/<x>/segments.
        let usage = self.storage_usage();
        let other = usage["other_bytes"].as_u64().unwrap_or_else(|| usage["bytes"].as_u64().unwrap_or(0));
        (other + self.db.folder_bytes("").unwrap_or(0).max(0) as u64)
            // Time-lapse videos keep their own days (Settings > Time-lapse)
            // and are never removed to make room: counting them would only
            // make recordings go sooner.
            .saturating_sub(dir_bytes(&self.timelapse_dir()))
    }

    /// Every recording folder (data folder first) with its cap right now.
    pub fn storage_caps(self: &Arc<Engine>) -> Vec<FolderCap> {
        let (data_max, pct, folders) = {
            let c = self.config.lock().unwrap();
            (c.storage.max_mb, c.storage.min_free_pct(), c.storage.folders.clone())
        };
        let mb = 1024 * 1024;
        let mut caps = vec![FolderCap { name: String::new(), path: self.data_dir.clone(), archive: false, auto: data_max == 0, cap: data_max * mb, used: self.data_folder_used(), ..Default::default() }];
        for f in &folders {
            caps.push(FolderCap { name: f.name.clone(), path: f.path.clone(), archive: f.archive, auto: f.max_mb == 0, cap: f.max_mb * mb, used: self.db.folder_bytes(&f.name).unwrap_or(0).max(0) as u64, ..Default::default() });
        }
        let mut spaces = Vec::new();
        for c in caps.iter_mut() {
            // An unplugged drive's mount point would report the system disk.
            let readable = c.name.is_empty() || c.path.is_dir();
            c.total = readable.then(|| fs4::total_space(&c.path).ok()).flatten();
            c.free = readable.then(|| fs4::available_space(&c.path).ok()).flatten();
            if let Some(t) = c.total {
                c.target_free = target_free(t, pct);
                c.margin = auto_margin(t);
            }
            spaces.push(RootSpace { drive: readable.then(|| volume_key(&c.path)).flatten(), used: c.used, auto: c.auto, total: c.total, free: c.free });
        }
        for (c, a) in caps.iter_mut().zip(auto_caps(&spaces, pct)) {
            if c.auto {
                c.cap = a.unwrap_or(0);
            }
            // A margin is never more than a tenth of the cap.
            c.margin = c.margin.min(c.cap / 10);
        }
        caps
    }

    /// As [`Engine::storage_caps`], for pages that only show it (Status,
    /// health, the plan): the last answer when it is under 10 s old, so a
    /// page polled every few seconds does not add up every recording each
    /// time. Pruning always asks `storage_caps` itself (it needs exact sizes).
    pub fn storage_caps_recent(self: &Arc<Engine>) -> Vec<FolderCap> {
        static CACHE: Mutex<Option<(std::time::Instant, Vec<FolderCap>)>> = Mutex::new(None);
        if let Some((t, v)) = CACHE.lock().unwrap().as_ref() {
            if t.elapsed() < std::time::Duration::from_secs(10) {
                return v.clone();
            }
        }
        let v = self.storage_caps();
        *CACHE.lock().unwrap() = Some((std::time::Instant::now(), v.clone()));
        v
    }

    /// Days since the oldest recording of a folder ("" = data folder).
    pub fn folder_days_kept(&self, name: &str) -> Option<f64> {
        let start_ms = self.db.oldest_segment_start_in(name).ok()??;
        Some(((Utc::now().timestamp_millis() - start_ms).max(0)) as f64 / 86_400_000.0)
    }

    /// The status of each folder in `storage_caps` order, with whether its
    /// oldest recordings move to the archive at the cap.
    pub fn storage_levels(&self, caps: &[FolderCap]) -> Vec<(StorageLevel, bool)> {
        let retention = self.config.lock().unwrap().storage.retention_days;
        let has_archive = caps.iter().any(|c| c.archive);
        caps.iter()
            .map(|c| {
                let rolls = has_archive && !c.archive && c.cap > 0;
                (storage_level(c, was_short(&c.name), rolls, self.folder_days_kept(&c.name), retention), rolls)
            })
            .collect()
    }

    /// `storage_usage` plus each folder's cap and status (GET /api/status).
    /// The storage part of `/api/status`, worked out at most every 10 s
    /// (every open page asks every 5 s; each answer queries the disks and the
    /// database).
    pub fn storage_status(self: &Arc<Engine>) -> serde_json::Value {
        use std::sync::Mutex;
        static CACHE: Mutex<Option<(std::time::Instant, serde_json::Value)>> = Mutex::new(None);
        if let Some((at, v)) = CACHE.lock().unwrap().as_ref() {
            if at.elapsed() < std::time::Duration::from_secs(10) {
                return v.clone();
            }
        }
        let v = self.storage_status_now();
        *CACHE.lock().unwrap() = Some((std::time::Instant::now(), v.clone()));
        v
    }

    fn storage_status_now(self: &Arc<Engine>) -> serde_json::Value {
        let mut v = self.storage_usage();
        // The indexed sizes from the running totals as they are now (a few rows): the walk above
        // is kept for minutes, and recordings removed from the index meanwhile (see
        // crate::missing) must not show until then.
        if self.db.segment_totals_ready() {
            v["indexed_bytes"] = serde_json::json!(self.db.total_segment_bytes().unwrap_or(0));
            if let Some(fs) = v["folders"].as_array_mut() {
                for f in fs.iter_mut() {
                    let name = f["name"].as_str().unwrap_or("").to_string();
                    f["bytes"] = serde_json::json!(self.db.folder_bytes(&name).unwrap_or(0));
                }
            }
        }
        let caps = self.storage_caps();
        let levels = self.storage_levels(&caps);
        let pct = self.config.lock().unwrap().storage.min_free_pct();
        let cap_json = |c: &FolderCap, l: &StorageLevel| {
            serde_json::json!({
                "auto": c.auto, "cap_bytes": c.cap, "used_bytes": c.used, "drive_total": c.total, "drive_free": c.free,
                "target_free": c.target_free, "level": l.level, "label": l.label, "detail": l.detail,
            })
        };
        v["min_free_percent"] = serde_json::json!(pct);
        v["data_cap"] = cap_json(&caps[0], &levels[0].0);
        if let Some(fs) = v["folders"].as_array_mut() {
            for f in fs.iter_mut() {
                let name = f["name"].as_str().unwrap_or("").to_string();
                if let Some(i) = caps.iter().position(|c| !c.name.is_empty() && c.name == name) {
                    f["cap"] = cap_json(&caps[i], &levels[i].0);
                }
            }
        }
        v
    }
}

/// Portable file size.
pub(super) fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum PurgeKind {
    /// A recording segment; `newest` marks the newest file of its camera
    /// (possibly still being written) which must never be deleted.
    Segment { path: std::path::PathBuf, newest: bool },
    /// An event (clip + thumb); protected events are never deleted.
    Event { id: String, protect: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct PurgeCandidate {
    pub time: i64,
    pub size: u64,
    pub kind: PurgeKind,
    /// Footage around a recent event: goes only after everything else.
    pub kept: bool,
}

/// Pure selection: pick the oldest deletable candidates until at least
/// `to_free` bytes would be freed, recent event footage last. Returns them
/// in that order.
pub(super) fn select_purge(cands: Vec<PurgeCandidate>, to_free: u64) -> Vec<PurgeCandidate> {
    let mut eligible: Vec<PurgeCandidate> = cands
        .into_iter()
        .filter(|c| match &c.kind {
            PurgeKind::Segment { newest, .. } => !*newest,
            PurgeKind::Event { protect, .. } => !*protect,
        })
        .collect();
    eligible.sort_by_key(|c| (c.kept, c.time));
    let mut freed = 0u64;
    let mut out = Vec::new();
    for c in eligible {
        if freed >= to_free {
            break;
        }
        freed += c.size;
        out.push(c);
    }
    out
}

/// Every 15 s, index recordings that cameras on the ffmpeg pipeline have
/// finished writing (the native recorder reports its own as it closes them).
/// Only those cameras' own folders are looked at: walking every folder
/// (hundreds of thousands of files, archive included) took seconds each
/// time and kept the database and the disks busy for everyone.
pub(super) fn segment_scan_loop(engine: Arc<Engine>) {
    loop {
        thread::sleep(std::time::Duration::from_secs(15));
        let cams: Vec<_> = engine
            .cameras
            .read()
            .unwrap()
            .iter()
            .flatten()
            .filter(|h| h.state.read().unwrap().ingest.starts_with("ffmpeg"))
            .map(|h| h.running.read().unwrap().clone())
            .collect();
        if cams.is_empty() {
            continue;
        }
        let seg = engine.config.lock().unwrap().storage.segment_seconds;
        let dirs: Vec<(String, String, std::path::PathBuf)> = cams
            .iter()
            .map(|cfg| {
                let (folder, root) = engine.record_folder(cfg);
                (crate::config::slug(&cfg.name), folder, root)
            })
            .collect();
        // Only the last hour's: the files are still walked (to chain start times), but the
        // database is held for those alone, not for every recording the camera ever made.
        // Older ones a stop or crash left out are added by the folder sweep (it adds those
        // since its last full read of the folder, see sweep_add_since).
        let since_ms = (Utc::now().timestamp() - 3600) * 1000;
        if let Err(e) = engine.db.import_recent_camera_dirs(&dirs, seg, since_ms) {
            warn!("recordings scan failed: {e:#}");
        }
    }
}

fn path_stem(p: &std::path::Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Delete an event's clip, thumbnail and AI review frames.
fn remove_event_files(dir: &std::path::Path, id: &str) {
    // Converted answers kept from its clip go too.
    crate::transcode::cache::forget_event(id);
    let _ = std::fs::remove_file(dir.join("clips").join(format!("{id}.mp4")));
    let _ = std::fs::remove_file(dir.join("clips").join(format!("{id}.start")));
    let _ = std::fs::remove_file(dir.join("thumbs").join(format!("{id}.jpg")));
    if !id.is_empty() && !id.contains(['/', '\\', '.']) {
        let _ = std::fs::remove_dir_all(dir.join("thumbs").join("ai").join(id));
    }
}

/// Bytes an event's files take (clip, thumbnail, AI review frames).
fn event_files_size(dir: &std::path::Path, id: &str) -> u64 {
    let frames: u64 = std::fs::read_dir(dir.join("thumbs").join("ai").join(id))
        .map(|rd| rd.flatten().map(|e| file_size(&e.path())).sum())
        .unwrap_or(0);
    file_size(&dir.join("clips").join(format!("{id}.mp4"))) + file_size(&dir.join("thumbs").join(format!("{id}.jpg"))) + frames
}

pub(super) fn retention_loop(engine: Arc<Engine>) {
    let mut passes: u64 = 0;
    loop {
        thread::sleep(std::time::Duration::from_secs(60));
        if let Err(e) = retention_pass(&engine) {
            warn!("retention pass failed: {e:#}");
        }
        // Hourly: AI review frames of events that are gone, and faces nobody
        // named after 60 days.
        // Daily: the longest recording's length, which only grows between these.
        if passes % 1440 == 720 {
            if let Err(e) = engine.db.recompute_longest_segment() {
                warn!("could not recompute the longest recording: {e:#}");
            }
        }
        if passes % 60 == 0 {
            sweep_ai_frames(&engine);
            sweep_temp_files(&engine.data_dir, true);
            let _ = engine.db.prune_faces(Utc::now().timestamp_millis() - super::faces::KEEP_UNNAMED_MS);
            let _ = engine.db.prune_plates(Utc::now().timestamp_millis() - super::lpr::keep_ms(&engine));
            // Signal history as long as recordings are kept.
            let days = engine.config.lock().unwrap().storage.retention_days.max(1) as i64;
            let _ = engine.db.prune_signals(Utc::now().timestamp_millis() - days * 86_400_000);
        }
        passes += 1;
    }
}

/// Remove `thumbs/ai/<id>` folders whose event no longer exists.
pub(super) fn sweep_ai_frames(engine: &Engine) {
    let root = engine.data_dir.join("thumbs").join("ai");
    let Ok(rd) = std::fs::read_dir(&root) else { return };
    let ids: Vec<String> = rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    if ids.is_empty() {
        return;
    }
    let Ok(alive) = engine.db.existing_events(&ids) else { return };
    for id in ids.iter().filter(|i| !alive.contains(*i)) {
        // A just-finished event writes its frames before its row exists.
        let fresh = std::fs::metadata(root.join(id)).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_none_or(|age| age < std::time::Duration::from_secs(600));
        if !fresh {
            let _ = std::fs::remove_dir_all(root.join(id));
        }
    }
}

/// Temporary files in use (an export being written or sent): the sweep
/// leaves them alone however old they look. Removed from the list, and the
/// file deleted, when the last holder drops it.
pub(crate) struct TempInUse(std::path::PathBuf);

fn temp_in_use() -> &'static Mutex<std::collections::HashSet<std::path::PathBuf>> {
    static IN_USE: std::sync::OnceLock<Mutex<std::collections::HashSet<std::path::PathBuf>>> = std::sync::OnceLock::new();
    IN_USE.get_or_init(Default::default)
}

impl TempInUse {
    pub(crate) fn new(path: std::path::PathBuf) -> Self {
        temp_in_use().lock().unwrap().insert(path.clone());
        TempInUse(path)
    }
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempInUse {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        temp_in_use().lock().unwrap().remove(&self.0);
    }
}

/// Temporary files left over from a crash or a stopped job, in `data_dir`:
/// in tmp/, exports (`export-*`, after an hour), time subtitle files
/// (`subs-*.srt`) and burned-in clips (`burn-*.mp4`), after a day; with
/// `thumbs`, the half-written pictures (`*.part`) a masked picture's rewrite
/// leaves (after a day). Files in use (see [`TempInUse`]) stay.
pub(crate) fn sweep_temp_files(data_dir: &std::path::Path, thumbs: bool) {
    let age = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).unwrap_or_default();
    let in_use = |p: &std::path::Path| temp_in_use().lock().unwrap().contains(p);
    const HOUR: std::time::Duration = std::time::Duration::from_secs(3600);
    const DAY: std::time::Duration = std::time::Duration::from_secs(86_400);
    if let Ok(rd) = std::fs::read_dir(data_dir.join("tmp")) {
        for f in rd.flatten() {
            let (p, name) = (f.path(), f.file_name().to_string_lossy().into_owned());
            let old = if name.starts_with("export-") {
                age(&p) > HOUR
            } else if (name.starts_with("subs-") && name.ends_with(".srt")) || (name.starts_with("burn-") && name.ends_with(".mp4")) {
                age(&p) > DAY
            } else {
                false
            };
            if old && p.is_file() && !in_use(&p) {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    if !thumbs {
        return;
    }
    // thumbs/<id>.jpg, thumbs/ai/<id>/<n>.jpg and the like: a few levels.
    let mut dirs = vec![(data_dir.join("thumbs"), 0)];
    while let Some((d, depth)) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for f in rd.flatten() {
            let Ok(t) = f.file_type() else { continue };
            let p = f.path();
            if t.is_dir() && depth < 3 {
                dirs.push((p, depth + 1));
            } else if t.is_file() && p.extension().is_some_and(|e| e == "part") && age(&p) > DAY {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

/// Clears a "running" flag when dropped, also when the thread panics.
pub(super) struct ClearOnDrop(pub(super) &'static std::sync::atomic::AtomicBool);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

static SWEEPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Set when a recording that retention removed from the index could not be deleted from the
/// disk (locked by a scanner or a player): the next sweep reads every folder in full.
pub(super) static SWEEP_WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The sweep comes this often. A pass is cheap: a camera folder is read in full only when it is
/// due (see [`crate::db::sweep_plan`]); the orphan clips and pictures are listed every time.
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
const SWEEP_FIRST_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Whether a folder sweep should start now (and none is running); marks it running.
/// Never while the start-up indexing runs (both would list the same folders).
fn sweep_due(importing: bool) -> bool {
    // When the next one is due (the first: 30 minutes after start).
    static NEXT_SWEEP: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut next = NEXT_SWEEP.lock().unwrap();
    let now = std::time::Instant::now();
    let due_at = *next.get_or_insert(now + SWEEP_FIRST_AFTER);
    if now < due_at || importing || SWEEPING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return false;
    }
    *next = Some(now + SWEEP_EVERY);
    true
}

/// Paces a folder walk: a short rest every few hundred files and between
/// folders, so listing a big folder is spread out instead of one burst.
struct Pace {
    on: bool,
    n: u32,
}

impl Pace {
    fn file(&mut self) {
        self.n += 1;
        if self.on && self.n % 200 == 0 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    fn folder(&self) {
        if self.on {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
}

/// One camera's segments folder as the sweep sees it.
struct SweepFolder<'a> {
    /// The storage folder's name ("" = the data folder).
    name: &'a str,
    root: &'a std::path::Path,
    slug: &'a str,
    /// Whether a configured camera records here (its recordings the index misses are added).
    import: bool,
    segment_seconds: u64,
}

/// What the sweep did with one camera folder.
#[derive(Debug, Default, PartialEq, Eq)]
struct FolderSweep {
    /// Not read (read less than a week ago, or untouched since a clean read).
    skipped: bool,
    added: usize,
    deleted: usize,
    /// Stray files that would not go (locked): the folder is read again at the next sweep (for
    /// the same files alone, only the next [`STUCK_SWEEPS`] - 1 sweeps).
    failed: usize,
}

/// The sweep of one camera folder: nothing when it is not due (see [`crate::db::sweep_plan`]);
/// else it is listed once, the recordings the index misses are added (those since the folder's
/// last full read, at least the last two days: see [`sweep_add_since`]), and files older than `cutoff` that the index does not keep are deleted. A read that found
/// nothing to fix is noted, so the folder is left alone until it is due again; one that fixed
/// something is not, so the next sweep looks again.
fn sweep_camera_folder(
    db: &crate::db::Db,
    f: &SweepFolder,
    since_ms: i64,
    cutoff: i64,
    wanted: bool,
    (full_every, quiet_every): (std::time::Duration, std::time::Duration),
    pace: &mut Pace,
) -> FolderSweep {
    let seg_dir = f.root.join("cameras").join(f.slug).join("segments");
    // The folder's time is read before it is listed: a change during the read shows next time.
    let mtime = crate::db::dir_mtime_ns(&seg_dir);
    let now_ms = Utc::now().timestamp_millis();
    let mark = db.sweep_mark(f.name, f.slug).unwrap_or(None);
    let since_ms = sweep_add_since(since_ms, mark);
    if crate::db::sweep_plan(mark, mtime, now_ms, wanted, full_every, quiet_every) == crate::db::SweepPlan::Skip {
        return FolderSweep { skipped: true, ..Default::default() };
    }
    let Some(listing) = crate::db::list_dir_files(&seg_dir, &mut || pace.file()) else { return FolderSweep::default() };
    let mut out = FolderSweep::default();
    if f.import {
        match db.import_recent_listed(f.name, f.root, f.slug, f.segment_seconds, since_ms, &listing) {
            Ok(n) => out.added = n,
            Err(e) => tracing::warn!("folder sweep: could not add missing recordings: {e:#}"),
        }
    }
    let failed = prune_listed(db, f.root, &listing, cutoff, &mut out.deleted);
    out.failed = failed.len();
    // The same files that would not go (owned by another user, say), sweep after sweep, with
    // nothing else to fix: after a few sweeps the read counts as clean anyway, so the folder is
    // not read in full every six hours for good.
    let only_those = out.added == 0 && out.deleted == 0;
    let stuck = stuck_only(&seg_dir, if only_those { failed } else { Vec::new() });
    if stuck || (out.added == 0 && out.deleted == 0 && out.failed == 0) {
        // The folder's time counts for a later skip only when it had settled before the read
        // and every recording in it is in the index (else a stray not yet old enough to go
        // would be skipped past its age).
        let listed = listing.iter().filter(|l| l.is_file && crate::db::is_segment_name(&l.name)).count() as i64;
        let settled = mtime.is_some_and(|m| now_ms - m / 1_000_000 >= 10_000);
        let all_indexed = db.indexed_in_folder(f.name, f.slug).is_ok_and(|n| n == listed);
        let noted = if settled && all_indexed { mtime.unwrap_or(-1) } else { -1 };
        let _ = db.set_sweep_mark(f.name, f.slug, noted, listed, now_ms);
    } else {
        let _ = db.clear_sweep_mark(f.name, f.slug);
    }
    out
}

/// Sweeps in a row that find only the same files that would not go before the folder's read
/// counts as clean anyway (see [`stuck_only`]).
const STUCK_SWEEPS: u32 = 3;

/// Whether the files that would not go at this read of `seg_dir` (`failed`, the read's only
/// problem; empty: none) are the same as at each of the last [`STUCK_SWEEPS`] - 1 reads.
/// Logged once when that is first so. Kept in memory: a restart starts the count again.
fn stuck_only(seg_dir: &std::path::Path, mut failed: Vec<std::path::PathBuf>) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static STUCK: OnceLock<Mutex<HashMap<std::path::PathBuf, (Vec<std::path::PathBuf>, u32)>>> = OnceLock::new();
    let mut m = STUCK.get_or_init(Default::default).lock().unwrap();
    if failed.is_empty() {
        m.remove(seg_dir);
        return false;
    }
    failed.sort();
    let e = m.entry(seg_dir.to_path_buf()).or_insert_with(|| (Vec::new(), 0));
    if e.0 == failed {
        e.1 = e.1.saturating_add(1);
    } else {
        *e = (failed, 1);
    }
    if e.1 == STUCK_SWEEPS {
        let names: Vec<String> = e.0.iter().take(5).map(|p| p.display().to_string()).collect();
        let more = if e.0.len() > 5 { format!(" and {} more", e.0.len() - 5) } else { String::new() };
        tracing::warn!(
            "folder sweep: could not delete {}{more} ({} sweeps in a row; check who owns it). The folder is read again only when due.",
            names.join(", "),
            e.1
        );
    }
    e.1 >= STUCK_SWEEPS
}

/// Slack before a folder's last full read (ms): a recording that was still being written then
/// (and so left out) is added at the next read.
const SWEEP_ADD_MARGIN_MS: i64 = 86_400_000;

/// From when (ms) the sweep adds the recordings a folder read finds missing from the index:
/// `since_ms` (two days back), or, when the folder was last read in full longer ago than that
/// (a clean read is not repeated for a week or more, see [`crate::db::sweep_plan`]), from that
/// read less [`SWEEP_ADD_MARGIN_MS`]. Every recording closed since the last read is then added
/// before the strays past the retention age are deleted, so a missed one is never taken for a
/// stray. A folder with no mark keeps the two days: it was never read (the start-up walk indexes
/// everything on the disk) or its last read (at most one sweep ago) found something to fix and
/// added what it found.
fn sweep_add_since(since_ms: i64, mark: Option<(i64, i64, i64)>) -> i64 {
    match mark {
        Some((_, full_ms, _)) => since_ms.min(full_ms.saturating_sub(SWEEP_ADD_MARGIN_MS)),
        None => since_ms,
    }
}

/// Deletes files past `cutoff` the index does not cover: stray recordings in
/// every camera folder on every drive, and clips/pictures with no event.
fn folder_sweep(engine: &Arc<Engine>, cutoff: i64, paced: bool) {
    let started = std::time::Instant::now();
    let mut pace = Pace { on: paced, n: 0 };
    let dir = &engine.data_dir;
    // Recordings closed but never indexed (the server stopped before the
    // indexer got to them, or crashed): add the recent ones, so playback,
    // caps and retention see them. Those since each folder's last full read
    // (at least two days back, see sweep_add_since): older ones were found
    // by that read.
    let (cams, seg) = {
        let c = engine.config.lock().unwrap();
        (c.cameras.clone(), c.storage.segment_seconds)
    };
    let dirs: Vec<(String, String, std::path::PathBuf)> = cams
        .iter()
        .map(|cfg| {
            let (folder, root) = engine.record_folder(cfg);
            (crate::config::slug(&cfg.name), folder, root)
        })
        .collect();
    let since_ms = (Utc::now().timestamp() - 2 * 86400) * 1000;
    let mut roots = vec![(String::new(), dir.clone())];
    roots.extend(engine.config.lock().unwrap().storage.folders.iter().map(|f| (f.name.clone(), f.path.clone())));
    // A recording the disk would not give up asks for a full read of every folder.
    let wanted = SWEEP_WANTED.swap(false, std::sync::atomic::Ordering::SeqCst);
    let (full_every, quiet_every) = if cfg!(test) {
        (std::time::Duration::ZERO, std::time::Duration::ZERO)
    } else {
        (crate::db::SWEEP_FULL_EVERY, crate::db::SWEEP_QUIET_FULL_EVERY)
    };
    // Each camera folder is listed once, for both the missing recordings and the cleanup.
    let (mut added, mut read, mut skipped) = (0, 0, 0);
    for (folder_name, root) in &roots {
        if let Ok(entries) = std::fs::read_dir(root.join("cameras")) {
            for cam in entries.flatten() {
                let slug = cam.file_name().to_string_lossy().into_owned();
                let configured = dirs.iter().any(|(s, f, r)| *s == slug && r == root && f == folder_name);
                let r = sweep_camera_folder(
                    &engine.db,
                    &SweepFolder { name: folder_name, root, slug: &slug, import: configured, segment_seconds: seg },
                    since_ms,
                    cutoff,
                    wanted,
                    (full_every, quiet_every),
                    &mut pace,
                );
                if r.skipped {
                    skipped += 1;
                } else {
                    read += 1;
                    pace.folder();
                }
                added += r.added;
            }
        }
    }
    if added > 0 {
        tracing::info!("folder sweep: added {added} recording(s) missing from the database");
    }
    // Orphaned clips/thumbnails (no event row) past the age.
    let protected: HashSet<String> = match engine.db.protected_ids() {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            tracing::warn!("folder sweep: could not read protected events: {e}");
            return;
        }
    };
    for sub in ["clips", "thumbs"] {
        if let Ok(files) = std::fs::read_dir(dir.join(sub)) {
            for f in files.flatten() {
                pace.file();
                if protected.contains(&path_stem(&f.path())) {
                    continue;
                }
                let mt = f.metadata().ok().and_then(|m| m.modified().ok());
                let mt = mt.map(|t| chrono::DateTime::<chrono::Utc>::from(t).timestamp()).unwrap_or(0);
                if mt < cutoff {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        pace.folder();
    }
    if paced {
        tracing::info!(
            "folder sweep looked at {} files ({read} camera folder(s) read, {skipped} not due) in {} s",
            pace.n,
            started.elapsed().as_secs()
        );
    }
}

/// How long retention waits for the start-up indexing before it runs anyway.
const IMPORT_WAIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Whether retention should skip this pass: the start-up indexing has run
/// (`age`: None = not running) for less than [`IMPORT_WAIT`].
fn retention_waits_for_import(age: Option<std::time::Duration>) -> bool {
    age.is_some_and(|a| a < IMPORT_WAIT)
}

/// One sweep: age-based retention, then the size caps (fixed or automatic),
/// then the drive reserve. Protected events (and
/// their clip/thumbnail) are never deleted by either, nor the recordings a
/// kept bookmark holds (crate::db::KEEP_WINDOW_MS around its moment).
pub(super) fn retention_pass(engine: &Arc<Engine>) -> anyhow::Result<()> {
    // The start-up indexing of existing recordings is still adding them.
    // For a while retention waits (caps would see too little). Past
    // IMPORT_WAIT it runs again, so a walk that hangs (an unplugged drive)
    // cannot let the disk fill; see `retention_waits_for_import`.
    let import_age = engine.db.importing_for();
    if retention_waits_for_import(import_age) {
        // The caps would see too little and wait. A drive that is really below its reserve
        // does not: its oldest known recordings go (this reads the rows known so far and
        // removes only those files, so a half-built index is no danger), or recording would
        // stop for lack of room while the indexing is slow.
        let (folders, archive, kept_from) = {
            let c = engine.config.lock().unwrap();
            let archive = c.storage.folders.iter().find(|f| f.archive).map(|f| (f.name.clone(), f.path.clone()));
            (c.storage.folders.clone(), archive, c.storage.events_kept_from_ms(Utc::now().timestamp()))
        };
        disk_guard(engine, &folders, archive.as_ref(), kept_from, &mut HashSet::new())?;
        return Ok(());
    }
    if import_age.is_some() && engine.db.first_overdue_note() {
        warn!(
            "indexing of existing recordings has run for over {} minutes (a folder or drive may be slow or missing); storage limits are running again meanwhile",
            IMPORT_WAIT.as_secs() / 60
        );
    }
    let retention_days = engine.config.lock().unwrap().storage.retention_days;
    let dir = &engine.data_dir;
    let db = &engine.db;
    // A hand-edited 0 must not mean "delete everything now".
    let cutoff = Utc::now().timestamp() - (retention_days.max(1) as i64) * 86400;

    // Recordings past the retention age: indexed ones via the database,
    // anything on disk the index missed via the folder.
    loop {
        // (Never those a kept bookmark holds: crate::db::KEEP_WINDOW_MS.)
        let old: Vec<_> = db
            .oldest_unkept_segments(500)?
            .into_iter()
            .filter(|(_, s)| s.end_ms < cutoff * 1000)
            .collect();
        if old.is_empty() {
            break;
        }
        for (_, s) in &old {
            let path = engine.segment_path(s);
            // (The index forgets it below either way: one that would not go asks for a sweep.)
            remove_retired(&path);
            crate::ingest::splice::forget(&path);
            super::playback::recordings_moved();
            db.delete_segment(&s.path)?;
        }
    }
    // Folder sweeps (files the index missed, orphan clips and pictures) list
    // hundreds of thousands of files on the recording disks: every 6 hours (first one
    // 30 minutes after start; a camera folder is read in full only when due, see sweep_camera_folder), on their own thread, paced so the
    // disk the cameras record to is never busy for long. (Recordings in the
    // index go above, every pass.)
    if cfg!(test) {
        folder_sweep(engine, cutoff, false);
    } else if sweep_due(db.importing()) {
        let engine = engine.clone();
        let spawned = std::thread::Builder::new().name("folder-sweep".into()).spawn(move || {
            let _running = ClearOnDrop(&SWEEPING);
            folder_sweep(&engine, cutoff, true);
        });
        if let Err(e) = spawned {
            SWEEPING.store(false, std::sync::atomic::Ordering::SeqCst);
            tracing::warn!("could not start the folder sweep: {e}");
        }
    }

    // Events past the retention age, with their clip and thumbnail.
    let expired: Vec<String> = db.events_before(cutoff)?.into_iter().map(|e| e.id).collect();
    for id in &expired {
        remove_event_files(dir, id);
    }
    db.delete_events(&expired)?;
    // Never on a half-built index: it would take events for outliving
    // their video.
    if !engine.db.importing() {
        drop_events_without_video(engine)?;
    }

    let (folders, archive, kept_from) = {
        let c = engine.config.lock().unwrap();
        let archive = c.storage.folders.iter().find(|f| f.archive).map(|f| (f.name.clone(), f.path.clone()));
        (c.storage.folders.clone(), archive, c.storage.events_kept_from_ms(Utc::now().timestamp()))
    };

    // Caps as they stand now (automatic ones follow the free space).
    let caps = engine.storage_caps();
    let mut short: HashSet<String> = HashSet::new();
    // Prune down to: a fixed cap exactly (as always); an automatic cap a
    // margin below it, so the drive is not trimmed again every minute.
    let goal = |c: &FolderCap| if c.auto { c.cap.saturating_sub(c.margin) } else { c.cap };

    // Data folder cap: recordings (moved to the archive when there is one)
    // and, without an archive, the oldest unprotected events too. An
    // automatic cap of 0 (the drive is short by more than the folder holds)
    // still prunes; a cap that could not be worked out (0, fixed or not
    // readable) does not.
    let data = &caps[0];
    if (data.cap > 0 || (data.auto && data.total.is_some())) && data.used > data.cap {
        let usage = data.used;
        let to_free = usage - goal(data);
        {
            let mut cands: Vec<PurgeCandidate> = Vec::new();
            // Moving to the archive goes strictly oldest first; deleting
            // keeps recent event footage until the quiet footage is gone.
            let rows = if archive.is_some() {
                db.oldest_segments_in("", 5000)?.into_iter().map(|s| (s, false)).collect()
            } else {
                db.segments_to_purge("", kept_from, 5000)?
            };
            for (s, kept) in rows {
                cands.push(PurgeCandidate {
                    time: s.start_ms / 1000,
                    size: s.bytes.max(0) as u64,
                    // Indexed segments are all closed; the one being written
                    // is not in the index yet.
                    kind: PurgeKind::Segment { path: dir.join(&s.path), newest: false },
                    kept,
                });
            }
            if archive.is_none() {
                for ev in db.oldest_unprotected_events(5000)? {
                    let size = event_files_size(dir, &ev.id);
                    let kept = ev.start * 1000 >= kept_from;
                    cands.push(PurgeCandidate { time: ev.start, size, kind: PurgeKind::Event { id: ev.id, protect: false }, kept });
                }
            }
            let picked = select_purge(cands, to_free);
            if picked.iter().map(|c| c.size).sum::<u64>() < to_free {
                short.insert(String::new());
            }
            let mut purged = Vec::new();
            for c in picked {
                match c.kind {
                    PurgeKind::Segment { path, .. } => {
                        let rel = crate::db::relative_path(dir, &path);
                        if let Err(e) = retire_segment(engine, &rel, &path, archive.as_ref()) {
                            retire_failed(&e);
                            short.insert(String::new());
                        }
                    }
                    PurgeKind::Event { id, .. } => {
                        remove_event_files(dir, &id);
                        purged.push(id);
                    }
                }
            }
            db.delete_events(&purged)?;
        }
    }

    // Each extra folder's cap; the archive's own cap last, so recordings
    // just moved there are counted.
    let mut ordered: Vec<&FolderCap> = caps[1..].iter().filter(|c| c.cap > 0 || (c.auto && c.total.is_some())).collect();
    ordered.sort_by_key(|c| c.archive);
    for c in ordered {
        let Some(f) = folders.iter().find(|f| f.name == c.name) else { continue };
        let quota = c.cap as i64;
        let goal = goal(c) as i64;
        let mut usage = db.folder_bytes(&f.name)?;
        if usage <= quota {
            continue;
        }
        let dest = if f.archive { None } else { archive.as_ref() };
        let rows: Vec<crate::db::SegmentRow> = if dest.is_some() {
            db.oldest_segments_in(&f.name, 5000)?
        } else {
            db.segments_to_purge(&f.name, kept_from, 5000)?.into_iter().map(|r| r.0).collect()
        };
        for s in rows {
            if usage <= goal {
                break;
            }
            if let Err(e) = retire_segment(engine, &s.path, &f.path.join(&s.path), dest) {
                retire_failed(&e);
                short.insert(f.name.clone());
            }
            usage -= s.bytes;
        }
        if usage > goal {
            short.insert(f.name.clone());
        }
    }

    disk_guard(engine, &folders, archive.as_ref(), kept_from, &mut short)?;
    set_short(short);
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Tests: the reserve on this thread only (the environment variable is for the whole process).
    static TEST_RESERVE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Free space every drive holding recordings keeps, whatever the caps say:
/// 2% of the drive, at least 1 GB.
pub(super) fn reserve_bytes(total: u64) -> u64 {
    #[cfg(test)]
    if let Some(n) = TEST_RESERVE.with(|r| r.get()) {
        return n;
    }
    // Tests: pretend the drive needs this much free.
    if let Some(n) = std::env::var("AMBERSIGHT_TEST_DISK_RESERVE").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    (total / 50).max(1 << 30)
}

/// Which drive a folder is on (folders on the same drive share free space).
fn volume_key(p: &std::path::Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p).ok().map(|m| format!("dev{}", m.dev()))
    }
    #[cfg(not(unix))]
    {
        let c = std::fs::canonicalize(p).ok()?;
        match c.components().next()? {
            std::path::Component::Prefix(x) => Some(x.as_os_str().to_string_lossy().to_ascii_uppercase()),
            _ => None,
        }
    }
}

/// Last resort: a drive with recordings that is nearly full (other programs'
/// files, caps set larger than the drive) loses its oldest recordings until
/// it has its reserve free again, so recording never stops for lack of room.
/// Recordings of an ordinary folder go to the archive when that is on
/// another drive with room; everything else is deleted. Protected events are
/// never touched.
fn disk_guard(engine: &Arc<Engine>, folders: &[crate::config::FolderConfig], archive: Option<&(String, std::path::PathBuf)>, kept_from: i64, short: &mut HashSet<String>) -> anyhow::Result<()> {
    let dir = &engine.data_dir;
    // (folder name, "" = data folder; root; is the archive)
    let mut roots: Vec<(String, std::path::PathBuf, bool)> = vec![(String::new(), dir.clone(), false)];
    roots.extend(folders.iter().filter(|f| f.path.is_dir()).map(|f| (f.name.clone(), f.path.clone(), f.archive)));
    let mut drives: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, r) in roots.iter().enumerate() {
        let Some(k) = volume_key(&r.1) else { continue };
        match drives.iter_mut().find(|d| d.0 == k) {
            Some(d) => d.1.push(i),
            None => drives.push((k, vec![i])),
        }
    }
    let archive_drive = archive.and_then(|a| volume_key(&a.1));
    let short_of = |p: &std::path::Path| -> Option<u64> {
        #[cfg(test)]
        if let Some(n) = TEST_NEED.with(|c| c.get()) {
            return Some(n);
        }
        let (free, total) = (fs4::available_space(p).ok()?, fs4::total_space(p).ok()?);
        Some(reserve_bytes(total).saturating_sub(free))
    };
    for (key, members) in drives {
        let need = match short_of(&roots[members[0]].1) {
            Some(n) if n > 0 => n,
            _ => continue,
        };
        // Moving to the archive frees this drive only if the archive is
        // elsewhere, and only while the archive has room itself.
        let to_archive = archive.filter(|a| archive_drive.as_deref().is_some_and(|ad| ad != key) && short_of(&a.1) == Some(0));
        let mut cands: Vec<PurgeCandidate> = Vec::new();
        let mut home: std::collections::HashMap<std::path::PathBuf, usize> = Default::default();
        // Whether every list below came back whole (shorter than its limit):
        // only then is nothing but kept bookmark video left.
        let mut whole = true;
        for &i in &members {
            let (name, root, _) = &roots[i];
            let rows = engine.db.segments_to_purge(name, kept_from, GUARD_ROWS)?;
            whole &= rows.len() < GUARD_ROWS;
            for (s, kept) in rows {
                home.insert(root.join(&s.path), i);
                cands.push(PurgeCandidate { time: s.start_ms / 1000, size: s.bytes.max(0) as u64, kind: PurgeKind::Segment { path: root.join(&s.path), newest: false }, kept });
            }
        }
        let data_here = members.contains(&0);
        if data_here {
            let evs = engine.db.oldest_unprotected_events(GUARD_ROWS)?;
            whole &= evs.len() < GUARD_ROWS;
            for ev in evs {
                let size = event_files_size(dir, &ev.id);
                let kept = ev.start * 1000 >= kept_from;
                cands.push(PurgeCandidate { time: ev.start, size, kind: PurgeKind::Event { id: ev.id, protect: false }, kept });
            }
        }
        let mut picked = select_purge(cands, need);
        let mut freed: u64 = picked.iter().map(|c| c.size).sum();
        // Recording never stops: when nothing else is left, the video kept
        // bookmarks hold goes too, oldest first (only here: age and the size
        // caps never touch it), and the admins are told. Not while ordinary
        // video may be left past what the lists held: the next pass (a minute
        // later) takes that first.
        let mut kept_paths: HashSet<std::path::PathBuf> = HashSet::new();
        if freed < need && whole {
            for s in engine.db.bookmark_kept_segments()? {
                if freed >= need {
                    break;
                }
                let Some(&i) = members.iter().find(|&&i| roots[i].0 == s.folder) else { continue };
                let path = roots[i].1.join(&s.path);
                home.insert(path.clone(), i);
                freed += s.bytes.max(0) as u64;
                kept_paths.insert(path.clone());
                picked.push(PurgeCandidate { time: s.start_ms / 1000, size: s.bytes.max(0) as u64, kind: PurgeKind::Segment { path, newest: false }, kept: true });
            }
        }
        if freed < need {
            short.extend(members.iter().map(|&i| roots[i].0.clone()));
        }
        if picked.is_empty() {
            continue;
        }
        warn!(
            "storage: the drive with {} is nearly full; removing its {} oldest recordings ({:.1} GB) to keep {:.1} GB free",
            roots[members[0]].1.display(),
            picked.len(),
            freed as f64 / 1e9,
            reserve_bytes(fs4::total_space(&roots[members[0]].1).unwrap_or(0)) as f64 / 1e9
        );
        let mut purged = Vec::new();
        // Kept bookmark video: (deleted, moved to the archive).
        let (mut kept_deleted, mut kept_moved) = (0usize, 0usize);
        for c in picked {
            match c.kind {
                PurgeKind::Segment { path, .. } => {
                    let Some(&i) = home.get(&path) else { continue };
                    let (_, root, is_archive) = &roots[i];
                    let rel = crate::db::relative_path(root, &path);
                    let dest = if *is_archive { None } else { to_archive };
                    match retire_segment(engine, &rel, &path, dest) {
                        Err(e) => retire_failed(&e),
                        Ok(()) if kept_paths.contains(&path) => {
                            if dest.is_some() {
                                kept_moved += 1;
                            } else {
                                kept_deleted += 1;
                            }
                        }
                        Ok(()) => {}
                    }
                }
                PurgeKind::Event { id, .. } => {
                    remove_event_files(dir, &id);
                    purged.push(id);
                }
            }
        }
        engine.db.delete_events(&purged)?;
        // (Told once it is done, saying what really happened to them.)
        if kept_deleted + kept_moved > 0 {
            warn!("storage: the drive with {} is full: of the recordings kept by bookmarks, {kept_deleted} were deleted and {kept_moved} moved to the archive to keep recording", roots[members[0]].1.display());
            kept_video_gone(engine, kept_deleted, kept_moved);
        }
    }
    Ok(())
}

/// Rows each list of the drive reserve holds at a time.
const GUARD_ROWS: usize = 5000;

/// Admins hear (at most every 6 hours) that kept bookmark video had to leave
/// the full drive: `deleted` files deleted, `moved` moved to the archive folder.
fn kept_video_gone(engine: &Arc<Engine>, deleted: usize, moved: usize) {
    #[cfg(test)]
    KEPT_GONE_TOLD.with(|c| c.set(c.get() + deleted + moved));
    let title = if moved == 0 {
        "Kept bookmark video was deleted to keep recording: drive full"
    } else if deleted == 0 {
        "Kept bookmark video was moved to the archive to keep recording: drive full"
    } else {
        "Kept bookmark video was removed from the main folder to keep recording: drive full"
    };
    let what = match (deleted, moved) {
        (d, 0) => format!("{d} recording file(s) that kept bookmarks held were deleted"),
        (0, m) => format!("{m} recording file(s) that kept bookmarks held were moved to the archive folder"),
        (d, m) => format!("of the recording files kept bookmarks held, {d} were deleted and {m} moved to the archive folder"),
    };
    crate::account_http::tell_admins_once(
        engine,
        "bookmark-video-deleted",
        6 * 3600,
        title.to_string(),
        format!("The drive with the recordings was full and nothing else was left to remove, so {what} to keep recording. Free space on the drive, or set a smaller size for recordings in Settings, Recording."),
    );
}

#[cfg(test)]
thread_local! {
    static KEPT_GONE_TOLD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Tests: how far every drive is short of its reserve, on this thread.
    static TEST_NEED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Move a recording to the archive folder (same relative path), or delete it
/// when there is no archive. Moving across drives copies then deletes.
/// The folder a recording's relative path `rel` hangs from (`src` minus `rel`).
fn rel_root(src: &std::path::Path, rel: &str) -> Option<std::path::PathBuf> {
    let mut p = src.to_path_buf();
    for _ in std::path::Path::new(rel).components() {
        p = p.parent()?.to_path_buf();
    }
    Some(p)
}

/// Log a recording that could not be moved or deleted (at most once every
/// 10 minutes), and carry on with the rest.
fn retire_failed(e: &anyhow::Error) {
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let mut l = LAST.lock().unwrap();
    if l.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(600)) {
        warn!("storage: could not move or delete an old recording: {e:#}");
        *l = Some(std::time::Instant::now());
    }
}

/// Delete a recording the index is about to forget. One the disk would not give up (locked by
/// a scanner or a player, probably) is found again only by a full read of its folder: that asks
/// the next sweep for one ([`SWEEP_WANTED`]). True when it asked.
fn remove_retired(path: &std::path::Path) -> bool {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            SWEEP_WANTED.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        }
        _ => false,
    }
}

fn retire_segment(
    engine: &Arc<Engine>,
    rel: &str,
    src: &std::path::Path,
    archive: Option<&(String, std::path::PathBuf)>,
) -> anyhow::Result<()> {
    // Gone already (deleted by hand, drive swapped): just forget it, so one
    // missing file cannot stop the cap from working.
    if !src.exists() {
        let root = rel_root(src, rel);
        if !root.as_deref().is_some_and(|r| r.is_dir()) {
            anyhow::bail!("the recordings folder for {} is not there (drive not connected?)", src.display());
        }
        engine.db.delete_segment(rel)?;
        return Ok(());
    }
    match archive {
        Some((name, root)) => {
            // An unplugged archive drive: do not create its folders on the
            // system disk (a bare mount point or drive letter).
            if !root.exists() {
                anyhow::bail!("the archive folder {} is not there (drive not connected?)", root.display());
            }
            let dest = root.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::fs::rename(src, &dest).is_err() {
                std::fs::copy(src, &dest)?;
                // Already gone (a folder sweep got to it meanwhile) is fine:
                // the copy is in the archive.
                if let Err(e) = std::fs::remove_file(src) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return Err(e.into());
                    }
                }
            }
            // Keep its layout: else the background index reads the archive
            // copy back from the archive drive to make it again.
            crate::ingest::splice::moved(src, &dest);
            super::playback::recordings_moved();
            engine.db.set_segment_folder(rel, name)?;
        }
        None => {
            remove_retired(src);
            crate::ingest::splice::forget(src);
            super::playback::recordings_moved();
            engine.db.delete_segment(rel)?;
        }
    }
    Ok(())
}

/// Unprotected events with no saved clip file whose recordings have all been
/// deleted (they ended before the oldest recording still kept for their
/// camera) can no longer be played: remove them. Events with no clip address
/// (never had video, or marked so when missing recordings were removed, see
/// crate::missing) are not: age retention removes those.
fn drop_events_without_video(engine: &Arc<Engine>) -> anyhow::Result<()> {
    let cfg = engine.config.lock().unwrap().clone();
    let mut oldest: std::collections::HashMap<String, Option<i64>> = Default::default();
    let mut gone = Vec::new();
    for ev in engine.db.oldest_unprotected_events_with_clip(5000)? {
        let cam = slug(&ev.camera);
        let first = match oldest.get(&cam) {
            Some(v) => *v,
            None => {
                let v = engine.db.oldest_segment_start(&cam)?;
                oldest.insert(cam.clone(), v);
                v
            }
        };
        // No indexed recordings at all: an ffmpeg-pipeline camera whose
        // clips are files; age retention handles those.
        let Some(first) = first else { continue };
        if (ev.end + cfg.motion_for(&ev.camera).post_roll as i64) * 1000 < first && !engine.clip_file_path(&ev.id).exists() {
            gone.push(ev.id);
        }
    }
    // Only those still unprotected with a clip address (a removal of missing recordings may have
    // marked one meanwhile), and their files only once their row is gone.
    for id in engine.db.delete_events_with_clip(&gone)? {
        remove_event_files(&engine.data_dir, &id);
    }
    Ok(())
}

/// Bytes of the files under `dir` (0 when it is not there).
fn dir_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flat_map(|it| it.flatten()) {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// Delete the listed files last modified before `cutoff` (epoch seconds).
/// A recording a kept bookmark holds (crate::db::KEEP_WINDOW_MS) stays: asked
/// for each old file just before it would go (the walk is slow; a bookmark may
/// be kept meanwhile). `root`: the folder the index's paths hang from.
/// Counts the files deleted in `deleted`; returns the files that would not go (locked, say), so
/// the sweep looks at the folder again instead of taking the read for a clean one.
fn prune_listed(db: &crate::db::Db, root: &std::path::Path, files: &[crate::db::ListedFile], cutoff: i64, deleted: &mut usize) -> Vec<std::path::PathBuf> {
    let mut failed = Vec::new();
    for f in files {
        // (No time known: 0, so it is looked at as an old one, as before.)
        let modified = f.mtime_ms.map(|ms| ms.div_euclid(1000)).unwrap_or(0);
        if modified < cutoff {
            // (Unreadable: kept, to be safe; the next sweep looks again.)
            if db.segment_kept(&crate::db::relative_path(root, &f.path)).unwrap_or(true) {
                continue;
            }
            match std::fs::remove_file(&f.path) {
                Ok(()) => *deleted += 1,
                // (A folder in the listing is not a recording; one gone meanwhile is fine.)
                Err(e) if f.is_file && e.kind() != std::io::ErrorKind::NotFound => failed.push(f.path.clone()),
                Err(_) => {}
            }
            crate::ingest::splice::forget(&f.path);
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_comes_every_six_hours_after_the_first_half_hour() {
        assert_eq!(SWEEP_EVERY, std::time::Duration::from_secs(6 * 3600));
        assert!(SWEEP_FIRST_AFTER >= std::time::Duration::from_secs(30 * 60));
    }

    /// The sweep reads a camera folder once, notes it, and leaves it alone until it is due
    /// (a week) or a recording that could not be deleted asks for a read; a folder it fixed
    /// something in is read again at the next sweep.
    #[test]
    fn sweep_reads_a_camera_folder_only_when_it_is_due() {
        let dir = std::env::temp_dir().join(format!("ambersight-sweepdue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seg = dir.join("cameras/cam/segments");
        std::fs::create_dir_all(&seg).unwrap();
        let now = std::time::SystemTime::now();
        let write = |name: &str, age_s: u64| {
            let p = seg.join(name);
            std::fs::write(&p, b"x").unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(now - std::time::Duration::from_secs(age_s)).unwrap();
            p
        };
        for i in 0..5 {
            write(&format!("cam-1700000000-seg{i:06}.mp4"), 10 * 86400 - i * 60);
        }
        let db = crate::db::Db::open_in_memory().unwrap();
        // The five are indexed (so none is a stray).
        let st = db.import_existing_recordings(&dir, &[], 60).unwrap();
        assert_eq!(st, 5);
        let f = SweepFolder { name: "", root: &dir, slug: "cam", import: true, segment_seconds: 60 };
        let week = (std::time::Duration::from_secs(7 * 86400), std::time::Duration::from_secs(28 * 86400));
        let cutoff = Utc::now().timestamp() - 20 * 86400;
        let since = (Utc::now().timestamp() - 2 * 86400) * 1000;
        let mut pace = Pace { on: false, n: 0 };
        // First sweep: no mark, read in full; nothing to fix, so it is noted.
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace);
        assert_eq!(r, FolderSweep::default());
        assert_eq!(pace.n, 5);
        assert!(db.sweep_mark("", "cam").unwrap().is_some());
        // Second sweep: not read at all.
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace);
        assert!(r.skipped);
        assert_eq!(pace.n, 5, "no file was looked at");
        // A new stray old file in the folder: not seen while the folder is not due.
        let stray = write("cam-1600000000-seg000000.mp4", 30 * 86400);
        assert!(sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace).skipped);
        assert!(stray.exists());
        // A recording that could not be deleted asks for a read: the stray goes, the folder is read.
        let r = sweep_camera_folder(&db, &f, since, cutoff, true, week, &mut pace);
        assert_eq!((r.skipped, r.deleted), (false, 1));
        assert!(!stray.exists());
        assert_eq!(pace.n, 11);
        // It fixed something: no mark, so the next sweep reads again, and that one (clean) notes it.
        assert!(db.sweep_mark("", "cam").unwrap().is_none());
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace);
        assert_eq!((r.skipped, r.deleted), (false, 0));
        assert_eq!(pace.n, 16);
        assert!(sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace).skipped);
        // Due again (a zero-length week): read.
        let none = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        assert!(!sweep_camera_folder(&db, &f, since, cutoff, false, none, &mut pace).skipped);
        assert_eq!(pace.n, 21);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A recording the index misses (another process wrote it, a stop left it out) is added when
    /// the folder is read, whatever its name's time stamp says (seconds or milliseconds).
    #[test]
    fn sweep_adds_recent_recordings_with_second_or_millisecond_names() {
        let dir = std::env::temp_dir().join(format!("ambersight-sweepadd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seg = dir.join("cameras/cam/segments");
        std::fs::create_dir_all(&seg).unwrap();
        let now = std::time::SystemTime::now();
        for (i, name) in ["cam-1794850000-seg000000.mp4", "cam-1794851000123-seg000000.mp4"].iter().enumerate() {
            let p = seg.join(name);
            std::fs::write(&p, b"x").unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(now - std::time::Duration::from_secs(3600 - i as u64 * 600)).unwrap();
        }
        let db = crate::db::Db::open_in_memory().unwrap();
        let f = SweepFolder { name: "", root: &dir, slug: "cam", import: true, segment_seconds: 60 };
        let week = (std::time::Duration::from_secs(7 * 86400), std::time::Duration::from_secs(28 * 86400));
        let mut pace = Pace { on: false, n: 0 };
        let r = sweep_camera_folder(&db, &f, (Utc::now().timestamp() - 2 * 86400) * 1000, Utc::now().timestamp() - 3 * 86400, false, week, &mut pace);
        assert_eq!(r.added, 2);
        // It fixed something, so the next sweep reads again.
        assert!(db.sweep_mark("", "cam").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sweep adds what is missing since the folder's last full read (with a day's slack),
    /// at least the last two days.
    #[test]
    fn sweep_adds_from_the_last_full_read() {
        let two_days = 1_000_000_000_000;
        assert_eq!(sweep_add_since(two_days, None), two_days);
        // Read an hour ago: the two days.
        assert_eq!(sweep_add_since(two_days, Some((0, two_days + 2 * 86_400_000, 3))), two_days);
        // Read 8 days ago: from 9 days back.
        let eight = two_days - 6 * 86_400_000;
        assert_eq!(sweep_add_since(two_days, Some((0, eight, 3))), eight - SWEEP_ADD_MARGIN_MS);
    }

    /// A folder read only once a week: a recording a stop left out 3-7 days before the read is
    /// added, not left to be deleted as a stray later.
    #[test]
    fn a_weekly_read_adds_recordings_older_than_two_days() {
        let dir = std::env::temp_dir().join(format!("ambersight-sweepweek-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seg = dir.join("cameras/cam/segments");
        std::fs::create_dir_all(&seg).unwrap();
        let now = std::time::SystemTime::now();
        let write = |name: String, age_s: u64| {
            let p = seg.join(name);
            std::fs::write(&p, b"x").unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(now - std::time::Duration::from_secs(age_s)).unwrap();
            p
        };
        let t = Utc::now().timestamp();
        for i in 0..3 {
            write(format!("cam-{}-seg{i:06}.mp4", t - 10 * 86400 + i * 60), (10 * 86400 - i * 60 - 60) as u64);
        }
        let db = crate::db::Db::open_in_memory().unwrap();
        assert_eq!(db.import_existing_recordings(&dir, &[], 60).unwrap(), 3);
        let f = SweepFolder { name: "", root: &dir, slug: "cam", import: true, segment_seconds: 60 };
        let week = (std::time::Duration::from_secs(7 * 86400), std::time::Duration::from_secs(28 * 86400));
        let cutoff = t - 20 * 86400;
        let since = (t - 2 * 86400) * 1000;
        let mut pace = Pace { on: false, n: 0 };
        assert_eq!(sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace), FolderSweep::default());
        // That clean read was 8 days ago (and the folder has changed since).
        let (_, _, files) = db.sweep_mark("", "cam").unwrap().unwrap();
        db.set_sweep_mark("", "cam", -1, files, (t - 8 * 86400) * 1000).unwrap();
        // A recording from 5 days ago that the index never got.
        let missed = write(format!("cam-{}-seg000000.mp4", t - 5 * 86400), 5 * 86400 - 60);
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace);
        assert_eq!((r.skipped, r.added, r.deleted), (false, 1, 0));
        assert!(missed.exists());
        assert_eq!(db.indexed_in_folder("", "cam").unwrap(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    /// Age retention: a recording the disk would not give up asks the next sweep for a full
    /// read (the index forgets it, so only a read of the folder finds it again).
    #[cfg(unix)]
    #[test]
    fn a_retired_recording_that_will_not_go_asks_for_a_sweep() {
        use std::os::unix::fs::PermissionsExt;
        if is_root() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ambersight-retire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.mp4");
        std::fs::write(&p, b"x").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(remove_retired(&p), "a delete that failed asks for a sweep");
        assert!(p.exists());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!remove_retired(&p));
        assert!(!p.exists());
        assert!(!remove_retired(&p), "one already gone does not ask");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stray that would not go is not a clean read: the folder is not marked, so the next
    /// sweep (six hours on, not a week) tries again.
    #[cfg(unix)]
    #[test]
    fn a_stray_that_will_not_go_keeps_the_folder_due() {
        use std::os::unix::fs::PermissionsExt;
        if is_root() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ambersight-sweeplock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seg = dir.join("cameras/cam/segments");
        std::fs::create_dir_all(&seg).unwrap();
        let now = std::time::SystemTime::now();
        let p = seg.join("cam-1600000000-seg000000.mp4");
        std::fs::write(&p, b"x").unwrap();
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(now - std::time::Duration::from_secs(30 * 86400)).unwrap();
        let db = crate::db::Db::open_in_memory().unwrap();
        let f = SweepFolder { name: "", root: &dir, slug: "cam", import: true, segment_seconds: 60 };
        let week = (std::time::Duration::from_secs(7 * 86400), std::time::Duration::from_secs(28 * 86400));
        let cutoff = Utc::now().timestamp() - 20 * 86400;
        let since = (Utc::now().timestamp() - 2 * 86400) * 1000;
        let mut pace = Pace { on: false, n: 0 };
        std::fs::set_permissions(&seg, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace);
        std::fs::set_permissions(&seg, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!((r.skipped, r.added, r.deleted, r.failed), (false, 0, 0, 1));
        assert!(db.sweep_mark("", "cam").unwrap().is_none(), "not marked as a clean read");
        // Next sweep: read again, and this time it goes.
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace);
        assert_eq!((r.skipped, r.deleted, r.failed), (false, 1, 0));
        assert!(!p.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stray that can never go (another user's file, say) does not keep the folder read in full
    /// every six hours for good: after a few sweeps that found only that, the read counts as clean
    /// and the folder waits until it is due (a week). Anything else to fix starts the count again.
    #[cfg(unix)]
    #[test]
    fn a_stray_that_never_goes_stops_keeping_the_folder_due() {
        use std::os::unix::fs::PermissionsExt;
        if is_root() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ambersight-sweepstuck-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seg = dir.join("cameras/cam/segments");
        std::fs::create_dir_all(&seg).unwrap();
        let now = std::time::SystemTime::now();
        let write = |name: &str, age_s: u64| {
            let p = seg.join(name);
            std::fs::write(&p, b"x").unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(now - std::time::Duration::from_secs(age_s)).unwrap();
            p
        };
        let p = write("cam-1600000000-seg000000.mp4", 30 * 86400);
        let db = crate::db::Db::open_in_memory().unwrap();
        let f = SweepFolder { name: "", root: &dir, slug: "cam", import: true, segment_seconds: 60 };
        let week = (std::time::Duration::from_secs(7 * 86400), std::time::Duration::from_secs(28 * 86400));
        let cutoff = Utc::now().timestamp() - 20 * 86400;
        let since = (Utc::now().timestamp() - 2 * 86400) * 1000;
        let mut pace = Pace { on: false, n: 0 };
        let lock = |on: bool| std::fs::set_permissions(&seg, std::fs::Permissions::from_mode(if on { 0o555 } else { 0o755 })).unwrap();
        lock(true);
        for _ in 1..STUCK_SWEEPS {
            let r = sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace);
            assert_eq!((r.skipped, r.deleted, r.failed), (false, 0, 1));
            assert!(db.sweep_mark("", "cam").unwrap().is_none(), "still due");
        }
        // The last of those sweeps: marked, so the next ones skip the folder.
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace);
        assert_eq!((r.skipped, r.failed), (false, 1));
        assert!(db.sweep_mark("", "cam").unwrap().is_some(), "counted as clean");
        assert!(sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace).skipped);
        // Due again (a zero-length week), still stuck: marked again straight away.
        let none = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, none, &mut pace);
        assert_eq!((r.skipped, r.failed), (false, 1));
        assert!(db.sweep_mark("", "cam").unwrap().is_some());
        // Another file that would not go: a different problem, so the count starts again.
        lock(false);
        let q = write("cam-1600000100-seg000000.mp4", 30 * 86400);
        lock(true);
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, none, &mut pace);
        assert_eq!(r.failed, 2);
        assert!(db.sweep_mark("", "cam").unwrap().is_none(), "new trouble: due again");
        // Once the files can go, they do.
        lock(false);
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, none, &mut pace);
        assert_eq!((r.deleted, r.failed), (2, 0));
        assert!(!p.exists() && !q.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Timing of the sweep of one big camera folder (run by hand:
    /// `SWEEP_TIMING_FILES=200000 cargo test --release sweep_timing -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn sweep_timing_of_a_big_folder() {
        let n: u32 = std::env::var("SWEEP_TIMING_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(200_000);
        let dir = std::env::temp_dir().join(format!("ambersight-sweeptime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seg = dir.join("cameras/cam/segments");
        std::fs::create_dir_all(&seg).unwrap();
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(5 * 86400);
        for i in 0..n {
            let f = std::fs::File::create(seg.join(format!("cam-1700000000-seg{:06}.mp4", i % 1_000_000))).unwrap();
            f.set_modified(t + std::time::Duration::from_secs(i as u64)).unwrap();
        }
        let db = crate::db::Db::open_in_memory().unwrap();
        let t0 = std::time::Instant::now();
        db.import_existing_recordings(&dir, &[], 60).unwrap();
        eprintln!("start-up index of {n} files: {:?}", t0.elapsed());
        let f = SweepFolder { name: "", root: &dir, slug: "cam", import: true, segment_seconds: 60 };
        let week = (std::time::Duration::from_secs(7 * 86400), std::time::Duration::from_secs(28 * 86400));
        let cutoff = Utc::now().timestamp() - 30 * 86400;
        let since = (Utc::now().timestamp() - 2 * 86400) * 1000;
        let mut pace = Pace { on: false, n: 0 };
        // The old sweep: always a full read (a zero-length week).
        let none = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        let t0 = std::time::Instant::now();
        sweep_camera_folder(&db, &f, since, cutoff, false, none, &mut pace);
        eprintln!("full read (what every sweep did): {:?}, {} files", t0.elapsed(), pace.n);
        let before = pace.n;
        let t0 = std::time::Instant::now();
        let r = sweep_camera_folder(&db, &f, since, cutoff, false, week, &mut pace);
        eprintln!("sweep not due: {:?}, skipped {}, files looked at {}", t0.elapsed(), r.skipped, pace.n - before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reserve_is_two_percent_at_least_one_gb() {
        if std::env::var_os("AMBERSIGHT_TEST_DISK_RESERVE").is_some() {
            return;
        }
        assert_eq!(reserve_bytes(4_000_000_000_000), 80_000_000_000);
        assert_eq!(reserve_bytes(20 << 30), 1 << 30);
        assert_eq!(reserve_bytes(0), 1 << 30);
    }

    const GB: u64 = 1_000_000_000;
    fn rs(drive: &str, used: u64, auto: bool, total: u64, free: u64) -> RootSpace {
        RootSpace { drive: Some(drive.into()), used, auto, total: Some(total), free: Some(free) }
    }
    fn no_reserve_override() -> bool {
        std::env::var_os("AMBERSIGHT_TEST_DISK_RESERVE").is_none()
    }

    #[test]
    fn auto_cap_is_use_plus_free_above_the_target() {
        if !no_reserve_override() {
            return;
        }
        // 2 TB drive, 10% = 200 GB to keep: 300 GB used + 500 GB free -> 600 GB.
        assert_eq!(auto_caps(&[rs("a", 300 * GB, true, 2000 * GB, 500 * GB)], 10), vec![Some(600 * GB)]);
        // Free space below the target: the cap is under the use by the shortfall.
        assert_eq!(auto_caps(&[rs("a", 300 * GB, true, 2000 * GB, 150 * GB)], 10), vec![Some(250 * GB)]);
        // Exactly at the target: cap = what the recordings use now.
        assert_eq!(auto_caps(&[rs("a", 300 * GB, true, 2000 * GB, 200 * GB)], 10), vec![Some(300 * GB)]);
        // Other files fill the drive beyond what the recordings could give back: never negative.
        assert_eq!(auto_caps(&[rs("a", 10 * GB, true, 2000 * GB, 50 * GB)], 10), vec![Some(0)]);
        // Fixed folders and unreadable drives get no automatic cap.
        assert_eq!(auto_caps(&[rs("a", 1, false, 2000 * GB, 500 * GB)], 10), vec![None]);
        let unknown = RootSpace { auto: true, used: 5, ..Default::default() };
        assert_eq!(auto_caps(&[unknown], 10), vec![None]);
        // A different percentage.
        assert_eq!(auto_caps(&[rs("a", 0, true, 1000 * GB, 500 * GB)], 25), vec![Some(250 * GB)]);
    }

    #[test]
    fn two_auto_folders_on_one_drive_share_it() {
        if !no_reserve_override() {
            return;
        }
        // 1 TB drive keeps 100 GB: 300 GB spare split evenly.
        let caps = auto_caps(&[rs("d", 200 * GB, true, 1000 * GB, 400 * GB), rs("d", 0, true, 1000 * GB, 400 * GB), rs("e", 0, true, 1000 * GB, 900 * GB)], 10);
        assert_eq!(caps, vec![Some(350 * GB), Some(150 * GB), Some(800 * GB)]);
        let shared: u64 = caps[..2].iter().map(|c| c.unwrap()).sum();
        assert_eq!(shared, 200 * GB + 400 * GB - 100 * GB, "together they claim the spare space once");
        // Short 60 GB: taken from each by what it holds (300 GB : 100 GB).
        let caps = auto_caps(&[rs("d", 300 * GB, true, 1000 * GB, 40 * GB), rs("d", 100 * GB, true, 1000 * GB, 40 * GB)], 10);
        assert_eq!(caps, vec![Some(255 * GB), Some(85 * GB)]);
        // A fixed folder on the same drive is just other files.
        let caps = auto_caps(&[rs("d", 100 * GB, true, 1000 * GB, 400 * GB), rs("d", 500 * GB, false, 1000 * GB, 400 * GB)], 10);
        assert_eq!(caps, vec![Some(400 * GB), None]);
    }

    #[test]
    fn tiny_drives_keep_the_reserve() {
        if !no_reserve_override() {
            return;
        }
        // 4 GB drive: 10% is 400 MB, but every drive keeps at least 1 GB.
        assert_eq!(target_free(4 << 30, 10), 1 << 30);
        assert_eq!(auto_caps(&[rs("t", 1 << 30, true, 4 << 30, 2 << 30)], 10), vec![Some(2 << 30)]);
        // Smaller than the reserve: nothing fits, the cap is 0, not negative.
        assert_eq!(target_free(512 << 20, 10), 512 << 20);
        assert_eq!(auto_caps(&[rs("t", 100 << 20, true, 512 << 20, 300 << 20)], 10), vec![Some(0)]);
        // Large drives: the percentage; capped at 90%.
        assert_eq!(target_free(1000 * GB, 10), 100 * GB);
        assert_eq!(target_free(1000 * GB, 100), 900 * GB);
        assert_eq!(auto_margin(1000 * GB), 5 * GB);
        assert_eq!(auto_margin(1 << 30), 256 << 20);
    }

    fn fc(auto: bool, used: u64, cap: u64, total: u64, free: u64) -> FolderCap {
        FolderCap { auto, used, cap, total: Some(total), free: Some(free), target_free: total / 10, margin: total / 200, ..Default::default() }
    }

    #[test]
    fn levels_full_and_keeping_up_is_information() {
        let t = 2000 * GB;
        // Automatic, at the cap, free space at the target: the steady state.
        let l = storage_level(&fc(true, 600 * GB, 605 * GB, t, 205 * GB), false, false, Some(9.4), 14);
        assert_eq!((l.level, l.label.as_str()), ("info", "Full · keeping 9 days"));
        // Lots of room: nothing to say.
        assert_eq!(storage_level(&fc(true, 100 * GB, 1500 * GB, t, 1600 * GB), false, false, Some(2.0), 14).level, "");
        // A fixed cap at its limit is the same steady state.
        let l = storage_level(&fc(false, 2000 << 20, 2048 << 20, t, 1000 * GB), false, false, Some(1.5), 14);
        assert_eq!((l.level, l.label.as_str()), ("info", "Full · keeping 1 day"));
        // Full but keeping under a day of a 14-day retention: a warning.
        let l = storage_level(&fc(true, 600 * GB, 605 * GB, t, 205 * GB), false, false, Some(0.5), 14);
        assert_eq!((l.level, l.label.as_str()), ("warn", "Full · keeping 12 h"));
        assert_eq!(storage_level(&fc(true, 600 * GB, 605 * GB, t, 205 * GB), false, false, Some(0.01), 1).label, "Full · keeping 14 min");
        // A folder whose older recordings move to an archive says the days are only its own.
        assert_eq!(storage_level(&fc(true, 600 * GB, 605 * GB, t, 205 * GB), false, true, Some(3.8), 40).label, "Full · keeping 3 days + archive");
        // ...unless retention itself is a day.
        assert_eq!(storage_level(&fc(true, 600 * GB, 605 * GB, t, 205 * GB), false, false, Some(0.5), 1).level, "info");
    }

    #[test]
    fn levels_below_target_warn_then_fault() {
        let t = 2000 * GB;
        // Below the 200 GB target, recordings can still give it back: catching up.
        let l = storage_level(&fc(true, 600 * GB, 450 * GB, t, 50 * GB), false, false, Some(9.0), 14);
        assert_eq!((l.level, l.label.as_str()), ("warn", "Freeing space"));
        // The last pass could not free enough (unwritable, nothing left): fault.
        assert_eq!(storage_level(&fc(true, 600 * GB, 450 * GB, t, 50 * GB), true, false, Some(9.0), 14).level, "crit");
        // Other files fill the drive: even deleting every recording would not reach the target.
        let l = storage_level(&fc(true, 10 * GB, 0, t, 100 * GB), false, false, Some(0.1), 14);
        assert_eq!((l.level, l.label.as_str()), ("crit", "Can't free space"));
        // Nearly full whatever the mode.
        assert_eq!(storage_level(&fc(false, 1, 1 << 40, t, 30 * GB), false, false, None, 14).label, "Drive nearly full");
        assert_eq!(storage_level(&fc(true, 600 * GB, 0, t, 30 * GB), false, false, None, 14).level, "crit");
        // Fixed caps: the old low-space rule (under 5% / 5 GB) is a warning, not a fault.
        let l = storage_level(&fc(false, 10 * GB, 50 * GB, t, 90 * GB), false, false, None, 14);
        assert_eq!((l.level, l.label.as_str()), ("warn", "Low space"));
        assert_eq!(storage_level(&fc(false, 10 * GB, 50 * GB, t, 150 * GB), false, false, None, 14).level, "");
        // Over a fixed cap and the oldest will not go: fault.
        assert_eq!(storage_level(&fc(false, 60 * GB, 50 * GB, t, 1000 * GB), true, false, Some(3.0), 14).level, "crit");
        // Over a fixed cap with no recordings in it (event clips and pictures): a warning that says so.
        let l = storage_level(&fc(false, 26 * GB, 20 * GB, t, 1000 * GB), true, false, None, 14);
        assert_eq!((l.level, l.label.as_str()), ("warn", "Over its limit"));
        assert!(l.detail.contains("event clips and pictures"));
        // Unknown drive: nothing (the health report says it cannot read it).
        assert_eq!(storage_level(&FolderCap::default(), false, false, None, 14), StorageLevel::default());
    }

    #[test]
    fn root_of_a_recording() {
        let r = rel_root(std::path::Path::new("/d/rec/cameras/porch/a.mp4"), "cameras/porch/a.mp4").unwrap();
        assert_eq!(r, std::path::PathBuf::from("/d/rec"));
    }

    fn seg(time: i64, size: u64, newest: bool) -> PurgeCandidate {
        PurgeCandidate { time, size, kept: false, kind: PurgeKind::Segment { path: format!("s{time}.mp4").into(), newest } }
    }
    fn ev(id: &str, time: i64, size: u64, protect: bool) -> PurgeCandidate {
        PurgeCandidate { time, size, kept: false, kind: PurgeKind::Event { id: id.to_string(), protect } }
    }

    #[test]
    fn purge_segments_older_than_events_go_first() {
        let c = vec![ev("e1", 50, 10, false), seg(10, 10, false), seg(20, 10, false)];
        let out = select_purge(c, 20);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|x| matches!(x.kind, PurgeKind::Segment { .. })));
        assert_eq!(out[0].time, 10);
    }

    #[test]
    fn recent_event_footage_goes_last() {
        let mut kept = seg(1, 100, false);
        kept.kept = true;
        let picked = select_purge(vec![kept, seg(2, 100, false), seg(3, 100, false)], 150);
        assert_eq!(picked.iter().map(|c| c.time).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn purge_never_chooses_protected_events() {
        let c = vec![ev("p", 1, 100, true), ev("u", 2, 10, false)];
        let out = select_purge(c, 1000);
        assert_eq!(out, vec![ev("u", 2, 10, false)]);
    }

    #[test]
    fn purge_never_chooses_newest_segment() {
        let c = vec![seg(1, 10, false), seg(99, 10, true)];
        let out = select_purge(c, 1000);
        assert_eq!(out, vec![seg(1, 10, false)]);
    }

    #[test]
    fn purge_stops_once_under_quota() {
        let c = vec![seg(1, 10, false), seg(2, 10, false), seg(3, 10, false), seg(4, 10, false)];
        let out = select_purge(c, 15);
        assert_eq!(out.len(), 2);
        assert!(select_purge(vec![seg(1, 10, false)], 0).is_empty());
    }
}

#[cfg(test)]
mod import_wait_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn retention_waits_for_the_startup_index_only_for_a_while() {
        assert!(!retention_waits_for_import(None));
        assert!(retention_waits_for_import(Some(Duration::ZERO)));
        assert!(retention_waits_for_import(Some(IMPORT_WAIT - Duration::from_secs(1))));
        assert!(!retention_waits_for_import(Some(IMPORT_WAIT)));
        assert!(!retention_waits_for_import(Some(Duration::from_secs(86400))));
    }

    #[test]
    fn the_long_indexing_warning_is_once_per_run() {
        let db = Arc::new(crate::db::Db::open_in_memory().unwrap());
        assert_eq!(db.importing_for(), None);
        let g = db.begin_startup_import();
        assert!(db.importing_for().is_some());
        assert!(db.first_overdue_note());
        assert!(!db.first_overdue_note());
        drop(g);
        assert_eq!(db.importing_for(), None);
        let _g = db.begin_startup_import();
        assert!(db.first_overdue_note());
    }

    #[test]
    fn a_drive_below_its_reserve_is_trimmed_even_while_the_start_up_indexing_runs() {
        let mut e = Engine::for_test(toml::from_str("").unwrap());
        let dir = std::env::temp_dir().join(format!("ambersight-guard-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cameras/cam")).unwrap();
        Arc::get_mut(&mut e).unwrap().data_dir = dir.clone();
        let file = dir.join("cameras/cam/old.mp4");
        std::fs::write(&file, vec![0u8; 1000]).unwrap();
        let row = crate::db::SegmentRow { path: "cameras/cam/old.mp4".into(), start_ms: 1000, end_ms: 6000, bytes: 1000, video_frames: 10, kind: "continuous".into(), folder: String::new() };
        e.db.insert_segment("cam", &row).unwrap();
        let _indexing = e.db.begin_startup_import();
        // Plenty of room: nothing is touched while the indexing runs.
        TEST_RESERVE.with(|r| r.set(Some(0)));
        retention_pass(&e).unwrap();
        assert!(file.exists());
        // Below the reserve: the oldest known recording goes, and only that file.
        let other = dir.join("cameras/cam/unindexed.mp4");
        std::fs::write(&other, vec![0u8; 10]).unwrap();
        TEST_RESERVE.with(|r| r.set(Some(u64::MAX / 4)));
        retention_pass(&e).unwrap();
        TEST_RESERVE.with(|r| r.set(None));
        assert!(!file.exists(), "the known old recording was removed");
        assert!(other.exists(), "a file the index does not know is left alone");
        assert!(e.db.oldest_segments(10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_skips_the_recordings_a_kept_bookmark_holds() {
        let mut e = Engine::for_test(toml::from_str("[storage]\nretention_days = 1").unwrap());
        let dir = std::env::temp_dir().join(format!("ambersight-bm-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cameras/cam/segments")).unwrap();
        Arc::get_mut(&mut e).unwrap().data_dir = dir.clone();
        // Ten one-minute recordings three days old (past the 1-day retention).
        let t0 = (Utc::now().timestamp() - 3 * 86400) * 1000;
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86400);
        let file = |i: i64| dir.join(format!("cameras/cam/segments/{i}.mp4"));
        for i in 0..10 {
            std::fs::write(file(i), vec![0u8; 100]).unwrap();
            std::fs::File::options().write(true).open(file(i)).unwrap().set_modified(old).unwrap();
            let row = crate::db::SegmentRow { path: format!("cameras/cam/segments/{i}.mp4"), start_ms: t0 + i * 60_000, end_ms: t0 + (i + 1) * 60_000, bytes: 100, video_frames: 10, kind: "continuous".into(), folder: String::new() };
            e.db.insert_segment("cam", &row).unwrap();
        }
        // A file the index does not know, inside the window too: the folder sweep keeps it
        // only if the index has it (it is not a kept recording).
        std::fs::write(dir.join("cameras/cam/segments/stray.mp4"), b"x").unwrap();
        std::fs::File::options().write(true).open(dir.join("cameras/cam/segments/stray.mp4")).unwrap().set_modified(old).unwrap();
        // Kept at 5:30: 3:30 to 7:30 stays (files 3..=7); another camera's bookmark and an unkept one hold nothing.
        let bm = |camera: Option<&str>, at: i64, keep: bool| crate::db::Bookmark { id: 0, camera: camera.map(str::to_string), at_ms: at, note: "n".into(), user: "ann".into(), owner: Some("ann".into()), created_ms: 0, updated_ms: 0, keep };
        let id = e.db.bookmark_add(&bm(Some("cam"), t0 + 330_000, true)).unwrap();
        e.db.bookmark_add(&bm(Some("other"), t0 + 30_000, true)).unwrap();
        e.db.bookmark_add(&bm(Some("cam"), t0 + 30_000, false)).unwrap();
        TEST_RESERVE.with(|r| r.set(Some(0)));
        retention_pass(&e).unwrap();
        let left: Vec<i64> = (0..10).filter(|&i| file(i).exists()).collect();
        assert_eq!(left, [3, 4, 5, 6, 7]);
        assert_eq!(e.db.oldest_segments(100).unwrap().len(), 5, "their index rows stay too");
        assert!(!dir.join("cameras/cam/segments/stray.mp4").exists());
        // Unticking keep releases them (the drive reserve, below, is the one exception).
        let mut b = e.db.bookmark_get(id).unwrap().unwrap();
        b.keep = false;
        e.db.bookmark_update(&b).unwrap();
        TEST_RESERVE.with(|r| r.set(Some(0)));
        retention_pass(&e).unwrap();
        TEST_RESERVE.with(|r| r.set(None));
        assert_eq!((0..10).filter(|&i| file(i).exists()).count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A camera on the ffmpeg pipeline recording into an extra folder, its files named with the
    /// start in seconds (before 0.11): indexed ones go by age and by the folder's cap, and old
    /// ones the index never had go by age through the folder sweep, which adds recent ones.
    #[test]
    fn ffmpeg_recordings_in_an_extra_folder_are_pruned_by_age_and_cap() {
        let dir = std::env::temp_dir().join(format!("ambersight-ffprune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let new = dir.join("new");
        let seg = new.join("cameras/office_wyze/segments");
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let cfg = format!(
            "[storage]\nretention_days = 1\n[[storage.folders]]\nname = \"new\"\npath = {:?}\nmax_mb = 1\n\
             [[cameras]]\nname = \"Office Wyze\"\nurl = \"rtsp://192.0.2.10/stream0\"\ningest = \"ffmpeg\"\nfolder = \"new\"\n",
            new.to_string_lossy()
        );
        let mut e = Engine::for_test(toml::from_str(&cfg).unwrap());
        Arc::get_mut(&mut e).unwrap().data_dir = dir.join("data");
        let now = std::time::SystemTime::now();
        let write = |name: &str, age_s: u64| {
            let p = seg.join(name);
            std::fs::write(&p, vec![0u8; 400_000]).unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(now - std::time::Duration::from_secs(age_s)).unwrap();
            p
        };
        // Three days old (past the 1-day retention), 10 s apart.
        let old: Vec<_> = (0..3).map(|i| write(&format!("office_wyze-1794500000-seg{i:06}.mp4"), 3 * 86400 - i * 10)).collect();
        // An hour old: five over the 1 MB cap together.
        let recent: Vec<_> = (0..5).map(|i| write(&format!("office_wyze-1794849248-seg{i:06}.mp4"), 3600 - i * 10)).collect();
        assert_eq!(e.db.import_camera_dirs(&[("office_wyze".into(), "new".into(), new.clone())], 10).unwrap(), 8);
        // Never indexed: one three days old, one from a later run (milliseconds) 30 minutes old.
        let stray_old = write("office_wyze-1794500100-seg000000.mp4", 3 * 86400);
        let stray_new = write("office_wyze-1794851000123-seg000000.mp4", 1800);
        assert!(e.db.segments_overlapping("office_wyze", 0, i64::MAX).unwrap().iter().all(|r| r.kind == "legacy" && r.folder == "new"));
        TEST_RESERVE.with(|r| r.set(Some(0)));
        retention_pass(&e).unwrap();
        TEST_RESERVE.with(|r| r.set(None));
        assert!(old.iter().all(|p| !p.exists()), "past the age: deleted");
        assert!(!stray_old.exists(), "an old file the index never had: deleted by the sweep");
        // The sweep indexed the recent stray; the cap (1 MiB) then took the oldest recent ones.
        let left: Vec<bool> = recent.iter().map(|p| p.exists()).collect();
        assert_eq!(left, [false, false, false, false, true]);
        assert!(stray_new.exists());
        let rows = e.db.segments_overlapping("office_wyze", 0, i64::MAX).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(e.db.folder_bytes("new").unwrap() <= 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_drive_deletes_kept_bookmark_video_last_and_says_so() {
        let mut e = Engine::for_test(toml::from_str("").unwrap());
        let dir = std::env::temp_dir().join(format!("ambersight-bm-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cameras/cam/segments")).unwrap();
        Arc::get_mut(&mut e).unwrap().data_dir = dir.clone();
        let t0 = (Utc::now().timestamp() - 3600) * 1000;
        let file = |i: i64| dir.join(format!("cameras/cam/segments/{i}.mp4"));
        for i in 0..10 {
            std::fs::write(file(i), vec![0u8; 100]).unwrap();
            let row = crate::db::SegmentRow { path: format!("cameras/cam/segments/{i}.mp4"), start_ms: t0 + i * 60_000, end_ms: t0 + (i + 1) * 60_000, bytes: 100, video_frames: 10, kind: "continuous".into(), folder: String::new() };
            e.db.insert_segment("cam", &row).unwrap();
        }
        // Kept at 1:30: files 0..=3.
        e.db.bookmark_add(&crate::db::Bookmark { id: 0, camera: Some("cam".into()), at_ms: t0 + 90_000, note: "n".into(), user: "ann".into(), owner: Some("ann".into()), created_ms: 0, updated_ms: 0, keep: true }).unwrap();
        KEPT_GONE_TOLD.with(|c| c.set(0));
        // Short by a little: unkept video goes first, kept stays, nothing said.
        let short = |bytes: u64| TEST_NEED.with(|r| r.set(Some(bytes)));
        short(250);
        retention_pass(&e).unwrap();
        assert_eq!((0..10).filter(|&i| file(i).exists()).collect::<Vec<_>>(), [0, 1, 2, 3, 7, 8, 9]);
        assert_eq!(KEPT_GONE_TOLD.with(|c| c.get()), 0);
        // Nothing else left: kept video goes too, oldest first, and the admins are told.
        short(100 * 3 + 150);
        retention_pass(&e).unwrap();
        TEST_NEED.with(|r| r.set(None));
        assert_eq!((0..10).filter(|&i| file(i).exists()).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(KEPT_GONE_TOLD.with(|c| c.get()), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ordinary_video_past_one_list_goes_before_kept_video() {
        let mut e = Engine::for_test(toml::from_str("").unwrap());
        let dir = std::env::temp_dir().join(format!("ambersight-bm-many-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cameras/cam/segments")).unwrap();
        Arc::get_mut(&mut e).unwrap().data_dir = dir.clone();
        let t0 = (Utc::now().timestamp() - 86400) * 1000;
        // Kept video first (oldest), then more ordinary recordings than one list holds.
        let n = GUARD_ROWS as i64 + 100;
        let file = |i: i64| dir.join(format!("cameras/cam/segments/{i}.mp4"));
        for i in 0..(4 + n) {
            std::fs::write(file(i), b"x").unwrap();
            let row = crate::db::SegmentRow { path: format!("cameras/cam/segments/{i}.mp4"), start_ms: t0 + i * 5000, end_ms: t0 + (i + 1) * 5000, bytes: 10, video_frames: 1, kind: "continuous".into(), folder: String::new() };
            e.db.insert_segment("cam", &row).unwrap();
        }
        // Kept so that its 2 minutes after reach into the first two recordings only.
        e.db.bookmark_add(&crate::db::Bookmark { id: 0, camera: Some("cam".into()), at_ms: t0 - 120_000 + 10_000, note: "n".into(), user: "ann".into(), owner: None, created_ms: 0, updated_ms: 0, keep: true }).unwrap();
        let kept: Vec<i64> = (0..(4 + n)).filter(|i| e.db.segment_kept(&format!("cameras/cam/segments/{i}.mp4")).unwrap()).collect();
        assert_eq!(kept, [0, 1]);
        KEPT_GONE_TOLD.with(|c| c.set(0));
        // Short by more than everything: the first pass takes one list of ordinary video only.
        TEST_NEED.with(|r| r.set(Some(u64::MAX / 4)));
        retention_pass(&e).unwrap();
        assert!(file(0).exists() && file(1).exists(), "kept video stays while ordinary video is left");
        assert_eq!(KEPT_GONE_TOLD.with(|c| c.get()), 0);
        assert!((2..(4 + n)).filter(|&i| file(i).exists()).count() > 0, "more than one list of ordinary video");
        // The next pass: the rest of the ordinary video, then the kept video, and a word to the admins.
        retention_pass(&e).unwrap();
        TEST_NEED.with(|r| r.set(None));
        assert_eq!((0..(4 + n)).filter(|&i| file(i).exists()).count(), 0);
        assert_eq!(KEPT_GONE_TOLD.with(|c| c.get()), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------- writability

/// What recorders found they cannot write, by (folder name, path): when, and
/// how long it is reported whatever the cheap check says. So the health
/// report names them even when the folder itself still looks writable.
static UNWRITABLE: Mutex<Option<HashMap<(String, PathBuf), (i64, i64)>>> = Mutex::new(None);
/// Real-write probes made for "no frames came" (folder name -> when).
static LAST_REAL: Mutex<Option<HashMap<String, std::time::Instant>>> = Mutex::new(None);
/// How long a recorder's finding counts without a new one.
const UNWRITABLE_KEEP_SECS: i64 = 600;

/// The account AmberSight runs as, for messages.
fn run_as() -> String {
    #[cfg(unix)]
    {
        // SAFETY: plain getters with no arguments.
        let (u, g) = unsafe { (libc::geteuid(), libc::getegid()) };
        format!("user {u}:{g}")
    }
    #[cfg(not(unix))]
    {
        "this service account".to_string()
    }
}

/// "Folder cams (/recordings) is not writable by AmberSight (user 99:100)", or
/// "... is on a read-only file system" when that is the cause. `path` is the
/// folder that refuses (a folder below the root, when only that one does).
/// The form without a path is for alerts, which may leave the machine.
pub(crate) fn unwritable_text(name: &str, path: &Path, with_path: bool) -> String {
    // The read-only answer comes from the last probe: no statvfs here (this
    // runs on the request path, where a hung share must not block).
    let ro = PROBED.lock().unwrap().as_ref().and_then(|m| m.get(path)).is_some_and(|(_, _, ro)| *ro);
    unwritable_text_with(name, path, with_path, ro)
}

pub(crate) fn unwritable_text_with(name: &str, path: &Path, with_path: bool, read_only: bool) -> String {
    let name = if name.is_empty() { "data" } else { name };
    match (with_path, read_only) {
        (true, false) => format!("Folder {name} ({}) is not writable by AmberSight ({})", path.display(), run_as()),
        (true, true) => format!("Folder {name} ({}) is on a read-only file system", path.display()),
        (false, false) => format!("Folder {name} is not writable by AmberSight"),
        (false, true) => format!("Folder {name} is on a read-only file system"),
    }
}

/// Whether `dir` is on a file system mounted read-only.
#[cfg(unix)]
fn dir_read_only(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    // SAFETY: valid NUL-terminated path; `st` is read only after statvfs succeeds.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        libc::statvfs(c.as_ptr(), &mut st) == 0 && (st.f_flag as u64 & libc::ST_RDONLY as u64) != 0
    }
}

#[cfg(not(unix))]
fn dir_read_only(_dir: &Path) -> bool {
    false
}

/// Whether an error means "this account may not write here" (not a full
/// disk or a missing folder, which have their own messages).
pub(crate) fn is_write_denied(e: &std::io::Error) -> bool {
    if matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem) {
        return true;
    }
    #[cfg(unix)]
    {
        return e.raw_os_error() == Some(libc::EROFS);
    }
    #[cfg(not(unix))]
    false
}

/// The cheap question: may this account create files in `dir`? On unix it
/// asks the system (faccessat with the effective uid, and statvfs) and writes
/// nothing, so an idle disk stays asleep. It can be wrong where the file
/// system does not enforce modes (some FUSE and network mounts), which is why
/// [`write_denied`] exists. True only when the folder exists and refuses.
#[cfg(unix)]
pub(crate) fn dir_write_denied(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if !dir.is_dir() {
        return false;
    }
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    const FLAGS: libc::c_int = libc::AT_EACCESS;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    const FLAGS: libc::c_int = 0;
    // SAFETY: `c` is a valid NUL-terminated path; `st` is only read after statvfs succeeds.
    unsafe {
        if libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::W_OK | libc::X_OK, FLAGS) != 0 {
            return is_write_denied(&std::io::Error::last_os_error());
        }
        let mut st: libc::statvfs = std::mem::zeroed();
        libc::statvfs(c.as_ptr(), &mut st) == 0 && (st.f_flag as u64 & libc::ST_RDONLY as u64) != 0
    }
}

/// Elsewhere there is no cheap question to ask: write a probe file. Callers
/// cache the answer for minutes.
#[cfg(not(unix))]
pub(crate) fn dir_write_denied(dir: &Path) -> bool {
    write_denied(dir)
}

/// The real question: create (and remove) a small file of our own in `dir`.
/// Wakes the disk, so only for a folder that is being recorded to anyway or
/// is asked about rarely. The name is unique, the create is exclusive and
/// does not follow a link, and a name taken (or briefly pending delete) is
/// retried under another, never read as "denied".
pub(crate) fn write_denied(dir: &Path) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    if !dir.is_dir() {
        return false;
    }
    for _ in 0..4 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let probe = dir.join(format!(".ambersight-write-test-{}-{}-{nanos:x}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match o.open(&probe) {
            Ok(f) => {
                drop(f);
                let _ = std::fs::remove_file(&probe);
                return false;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return is_write_denied(&e),
        }
    }
    false
}

/// How long an answer about a folder is kept.
#[cfg(unix)]
const PROBE_KEEP: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(not(unix))]
const PROBE_KEEP: std::time::Duration = std::time::Duration::from_secs(600);

static PROBED: Mutex<Option<HashMap<PathBuf, (std::time::Instant, bool, bool)>>> = Mutex::new(None);
/// (when asked, denied, on a read-only file system): the last answer for a folder.

/// `dir_write_denied`, remembered for [`PROBE_KEEP`] (a recorder's finding
/// replaces the entry at once). The lock is not held while asking, so a hung
/// network mount cannot hold up every caller.
fn denied_cached(dir: &Path) -> bool {
    if let Some((t, d, _)) = PROBED.lock().unwrap().as_ref().and_then(|m| m.get(dir)) {
        if t.elapsed() < PROBE_KEEP {
            return *d;
        }
    }
    let d = dir_write_denied(dir);
    PROBED.lock().unwrap().get_or_insert_with(HashMap::new).insert(dir.to_path_buf(), (std::time::Instant::now(), d, d && dir_read_only(dir)));
    d
}

/// The two questions, so tests can stand in for a file system that lies.
pub(crate) struct Probes {
    pub cheap: fn(&Path) -> bool,
    pub real: fn(&Path) -> bool,
}
pub(crate) const SYSTEM_PROBES: Probes = Probes { cheap: dir_write_denied, real: write_denied };

pub(crate) enum Verdict {
    /// This path refuses writes.
    Denied(PathBuf),
    /// A real write there worked.
    Writable(PathBuf),
    /// Nothing was asked of the disk, or the folder is not there.
    Unknown,
}

/// The deepest folder that exists on the way to `p` (itself, when it does).
pub(crate) fn deepest_existing(p: &Path) -> Option<PathBuf> {
    p.ancestors().find(|a| a.is_dir()).map(|a| a.to_path_buf())
}

/// Whether recordings can be written under `seg_dir`: the deepest existing
/// folder on its way is asked cheaply, and (when `real`) by a real write.
/// Folders above it are not asked: a root-owned share whose `cameras` folder
/// belongs to AmberSight is fine.
pub(crate) fn find_denied(pr: &Probes, seg_dir: &Path, real: bool) -> Verdict {
    let Some(target) = deepest_existing(seg_dir) else { return Verdict::Unknown };
    if (pr.cheap)(&target) {
        return Verdict::Denied(target);
    }
    if real {
        return if (pr.real)(&target) { Verdict::Denied(target) } else { Verdict::Writable(target) };
    }
    Verdict::Unknown
}

/// A recorder could not write into `dir` of the folder `name`. Reported for
/// at least `keep_secs` (30 s or more; the recorder's retry interval plus 30 s
/// stops a lying file system from making the warning flicker), until a real
/// write into the same path works.
pub(crate) fn note_unwritable(name: &str, dir: &Path, keep_secs: i64) {
    UNWRITABLE.lock().unwrap().get_or_insert_with(HashMap::new).insert((name.to_string(), dir.to_path_buf()), (Utc::now().timestamp(), keep_secs.max(30)));
    // What was asked of the system before is out of date.
    PROBED.lock().unwrap().get_or_insert_with(HashMap::new).insert(dir.to_path_buf(), (std::time::Instant::now(), true, dir_read_only(dir)));
}

/// A real write into `dir` of folder `name` worked: drop that finding (and
/// only that one: another camera's path is its own business).
pub(crate) fn note_writable(name: &str, dir: &Path) {
    if let Some(m) = UNWRITABLE.lock().unwrap().as_mut() {
        m.remove(&(name.to_string(), dir.to_path_buf()));
    }
    if let Some(m) = PROBED.lock().unwrap().as_mut() {
        m.remove(dir);
    }
}

/// True at most once per `every_secs` for a folder: the "no frames came"
/// reason to write a probe must not keep an idle disk awake through an outage.
pub(crate) fn real_probe_due(name: &str, every_secs: u64) -> bool {
    let mut g = LAST_REAL.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    if m.get(name).is_some_and(|t| t.elapsed().as_secs() < every_secs) {
        return false;
    }
    m.insert(name.to_string(), std::time::Instant::now());
    true
}

/// Ask about `seg_dir` (cheaply, and by a real write when `real`) and keep
/// the books: a refusal is noted, a real write that worked drops the note.
/// Returns the path that refuses.
pub(crate) fn check_folder(pr: &Probes, name: &str, seg_dir: &Path, real: bool, keep_secs: i64) -> Option<PathBuf> {
    match find_denied(pr, seg_dir, real) {
        Verdict::Denied(p) => {
            note_unwritable(name, &p, keep_secs);
            Some(p)
        }
        Verdict::Writable(p) => {
            note_writable(name, &p);
            None
        }
        Verdict::Unknown => None,
    }
}

/// At the end of a note's time: a real write (at most once a minute per path)
/// that works clears it.
fn recheck_clears(recheck: Option<fn(&Path) -> bool>, name: &str, p: &Path) -> bool {
    let Some(real) = recheck else { return false };
    if !real_probe_due(&format!("recheck:{}", p.display()), 60) {
        return false;
    }
    if real(p) {
        return false;
    }
    PROBED.lock().unwrap().get_or_insert_with(HashMap::new).insert(p.to_path_buf(), (std::time::Instant::now(), false, false));
    let _ = name;
    true
}

/// The folders that cannot be written right now, as (name, the path that
/// refuses), one per folder. Asked cheaply about the deepest existing folder
/// on the way to where recordings go (`<folder>/cameras`), and about what
/// recorders reported: reported for the time they asked for, then while the
/// cheap check agrees (up to 10 minutes). A missing or unreadable folder is
/// not listed: it has its own "cannot read" problem.
pub(crate) fn unwritable_folders(caps: &[FolderCap], now: i64) -> Vec<(String, PathBuf)> {
    // Where the cheap check is itself a probe file kept for minutes, the end
    // of a note's time is when a real write may clear it.
    #[cfg(unix)]
    let recheck: Option<fn(&Path) -> bool> = None;
    #[cfg(not(unix))]
    let recheck: Option<fn(&Path) -> bool> = Some(write_denied);
    unwritable_folders_with(caps, now, recheck)
}

fn unwritable_folders_with(caps: &[FolderCap], now: i64, recheck: Option<fn(&Path) -> bool>) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for c in caps {
        if c.free.is_none() {
            continue;
        }
        let mut notes: Vec<(PathBuf, i64, i64)> = UNWRITABLE
            .lock()
            .unwrap()
            .as_ref()
            .map(|m| m.iter().filter(|((n, _), _)| *n == c.name).map(|((_, p), (t, k))| (p.clone(), *t, *k)).collect())
            .unwrap_or_default();
        notes.sort();
        let mut found: Option<PathBuf> = None;
        for (p, t, keep) in notes {
            let age = now - t;
            let still = age < UNWRITABLE_KEEP_SECS && (age < keep || (denied_cached(&p) && !recheck_clears(recheck, &c.name, &p)));
            if still {
                found.get_or_insert(p);
            } else if let Some(m) = UNWRITABLE.lock().unwrap().as_mut() {
                // Only the note that was read: a newer one may have been
                // written meanwhile.
                let key = (c.name.clone(), p);
                if m.get(&key).is_some_and(|(tt, _)| *tt == t) {
                    m.remove(&key);
                }
            }
        }
        if found.is_none() {
            if let Some(target) = deepest_existing(&c.path.join("cameras")) {
                if denied_cached(&target) {
                    found = Some(target);
                }
            }
        }
        if let Some(p) = found {
            out.push((c.name.clone(), p));
        }
    }
    out
}

#[cfg(all(test, unix))]
mod writable_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("as-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }
    fn caps(name: &str, d: &Path) -> [FolderCap; 1] {
        [FolderCap { name: name.into(), path: d.to_path_buf(), free: Some(1), total: Some(2), ..Default::default() }]
    }

    #[test]
    fn a_read_only_file_system_is_said_so() {
        assert_eq!(unwritable_text_with("cams", Path::new("/r"), true, true), "Folder cams (/r) is on a read-only file system");
        assert_eq!(unwritable_text_with("cams", Path::new("/r"), false, true), "Folder cams is on a read-only file system");
        // A normal folder is not read-only.
        assert!(!dir_read_only(&std::env::temp_dir()) || is_root());
    }

    #[test]
    fn text_names_folder_path_and_user() {
        let t = unwritable_text("Cams", Path::new("/recordings/cameras/a"), true);
        assert!(t.starts_with("Folder Cams (/recordings/cameras/a) is not writable by AmberSight (user "), "{t}");
        assert_eq!(unwritable_text("", Path::new("/d"), false), "Folder data is not writable by AmberSight");
    }

    #[test]
    fn probe_sees_a_read_only_folder_and_the_cheap_one_writes_nothing() {
        let d = tmp("wprobe");
        assert!(!dir_write_denied(&d));
        assert!(!dir_write_denied(&d.join("missing")));
        assert!(!write_denied(&d));
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "probe files are removed");
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert_eq!(dir_write_denied(&d), !is_root());
        assert_eq!(write_denied(&d), !is_root());
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn read_only_errors_count_as_denied_and_others_do_not() {
        assert!(is_write_denied(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)));
        assert!(is_write_denied(&std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem)));
        assert!(is_write_denied(&std::io::Error::from_raw_os_error(libc::EROFS)));
        assert!(!is_write_denied(&std::io::Error::from(std::io::ErrorKind::NotFound)));
    }

    #[test]
    fn a_file_system_that_says_yes_and_refuses_is_caught_by_the_real_write() {
        let d = tmp("wfuse");
        let seg = d.join("cameras/a/segments");
        let lying = Probes { cheap: |_| false, real: |_| true };
        // Cheap only: believed (and nothing asked of the disk).
        assert!(matches!(find_denied(&lying, &seg, false), Verdict::Unknown));
        // With the real write: the refusal is found, naming the deepest existing folder.
        assert!(matches!(find_denied(&lying, &seg, true), Verdict::Denied(p) if p == d));
        let honest = Probes { cheap: |_| false, real: |_| false };
        assert!(matches!(find_denied(&honest, &seg, true), Verdict::Writable(p) if p == d));
        let refusing = Probes { cheap: |_| true, real: |_| false };
        assert!(matches!(find_denied(&refusing, &seg, false), Verdict::Denied(_)));
        assert!(matches!(find_denied(&lying, Path::new("/nonexistent-as-root/x"), true), Verdict::Denied(p) if p == Path::new("/")));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_note_beats_a_cached_writable_answer_and_clears_when_a_write_works() {
        let d = tmp("wcache");
        let cs = caps("wcache", &d);
        let now = Utc::now().timestamp();
        // Cached "writable" for the folder (nothing is denied).
        assert!(unwritable_folders(&cs, now).is_empty());
        note_unwritable("wcache", &d, 30);
        assert_eq!(unwritable_folders(&cs, now), vec![("wcache".to_string(), d.clone())]);
        // A real write that worked drops it at once.
        note_writable("wcache", &d);
        assert!(unwritable_folders(&cs, now).is_empty());
        // Old notes are rechecked: the folder is writable, so it clears.
        note_unwritable("wcache", &d, 30);
        PROBED.lock().unwrap().as_mut().unwrap().remove(&d);
        assert!(unwritable_folders(&cs, now + 40).is_empty());
        // A longer keep time holds it past 30 s whatever the cheap check says.
        note_unwritable("wcache", &d, 90);
        PROBED.lock().unwrap().as_mut().unwrap().insert(d.clone(), (std::time::Instant::now(), false, false));
        assert_eq!(unwritable_folders(&cs, now + 60).len(), 1);
        assert!(unwritable_folders(&cs, now + 130).is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_root_owned_root_with_a_writable_cameras_folder_is_fine() {
        if is_root() {
            return;
        }
        let d = tmp("wcams");
        std::fs::create_dir_all(d.join("cameras")).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
        let now = Utc::now().timestamp();
        assert!(unwritable_folders(&caps("wcams", &d), now).is_empty());
        assert!(matches!(find_denied(&SYSTEM_PROBES, &d.join("cameras/a/segments"), true), Verdict::Writable(p) if p == d.join("cameras")));
        // With no cameras folder yet, the root itself is what matters.
        let d2 = tmp("wcams2");
        std::fs::set_permissions(&d2, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert_eq!(unwritable_folders(&caps("wcams2", &d2), now), vec![("wcams2".to_string(), d2.clone())]);
        for x in [&d, &d2] {
            std::fs::set_permissions(x, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::remove_dir_all(x).unwrap();
        }
    }

    #[test]
    fn a_missing_folder_is_not_reported() {
        let d = tmp("wgone");
        let gone = [FolderCap { name: "gone".into(), path: d.join("nope"), free: None, total: None, ..Default::default() }];
        note_unwritable("gone", &d.join("nope"), 30);
        assert!(unwritable_folders(&gone, Utc::now().timestamp()).is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn one_cameras_good_write_does_not_clear_anothers_finding() {
        let d = tmp("wtwo");
        let cs = caps("wtwo", &d);
        let a = d.join("cameras/a");
        let b = d.join("cameras/b");
        let now = Utc::now().timestamp();
        note_unwritable("wtwo", &a, 90);
        note_unwritable("wtwo", &b, 90);
        note_writable("wtwo", &b);
        PROBED.lock().unwrap().as_mut().unwrap().insert(a.clone(), (std::time::Instant::now(), true, false));
        assert_eq!(unwritable_folders(&cs, now), vec![("wtwo".to_string(), a.clone())]);
        note_writable("wtwo", &a);
        assert!(unwritable_folders(&cs, now).is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn check_folder_notes_refusals_and_real_probes_are_rate_limited() {
        let d = tmp("wcheck");
        let seg = d.join("cameras/a/segments");
        let lying = Probes { cheap: |_| false, real: |_| true };
        assert!(check_folder(&lying, "wcheck", &seg, false, 30).is_none());
        assert_eq!(check_folder(&lying, "wcheck", &seg, true, 30), Some(d.clone()));
        let honest = Probes { cheap: |_| false, real: |_| false };
        assert!(check_folder(&honest, "wcheck", &seg, true, 30).is_none());
        assert!(unwritable_folders(&caps("wcheck", &d), Utc::now().timestamp()).is_empty());
        assert!(real_probe_due("wcheck-rate", 600));
        assert!(!real_probe_due("wcheck-rate", 600));
        assert!(real_probe_due("wcheck-rate2", 600));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_note_written_during_the_scan_is_not_removed_and_a_real_write_clears_at_the_end() {
        let d = tmp("wrace");
        let cs = caps("wrace", &d);
        let now = Utc::now().timestamp();
        // An expired note is removed only if it is still the one that was read.
        note_unwritable("wrace", &d, 30);
        UNWRITABLE.lock().unwrap().as_mut().unwrap().insert(("wrace".into(), d.clone()), (now - 100, 30));
        PROBED.lock().unwrap().as_mut().unwrap().insert(d.clone(), (std::time::Instant::now(), false, false));
        // (a fresh note replaces the old one before the scan acts on it)
        note_unwritable("wrace", &d, 90);
        assert_eq!(unwritable_folders(&cs, Utc::now().timestamp()).len(), 1);
        // End of keep time with the cheap answer still "denied" (a cached
        // probe on Windows): a real write that works clears the note.
        let key = ("wrace".to_string(), d.clone());
        UNWRITABLE.lock().unwrap().as_mut().unwrap().insert(key.clone(), (now - 100, 30));
        PROBED.lock().unwrap().as_mut().unwrap().insert(d.clone(), (std::time::Instant::now(), true, false));
        assert!(unwritable_folders_with(&cs, now, Some(|_| false)).is_empty());
        assert!(UNWRITABLE.lock().unwrap().as_ref().unwrap().get(&key).is_none());
        std::fs::remove_dir_all(&d).unwrap();
    }
}

#[cfg(test)]
mod temp_sweep_tests {
    #[test]
    fn old_temporary_files_are_swept_but_not_ones_in_use() {
        let d = std::env::temp_dir().join(format!("amber-tmpsweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("tmp")).unwrap();
        std::fs::create_dir_all(d.join("thumbs/ai/cam-1")).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
        let hours = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        let put = |rel: &str, t: std::time::SystemTime| {
            let p = d.join(rel);
            std::fs::write(&p, b"x").unwrap();
            std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(t).unwrap();
            p
        };
        let gone = [put("tmp/export-1-0.zip", hours), put("tmp/subs-1-0.srt", old), put("tmp/burn-1-0.mp4", old), put("thumbs/cam-5.12-0.part", old), put("thumbs/ai/cam-1/2.12-1.part", old)];
        let kept = [put("tmp/subs-1-1.srt", hours), put("tmp/burn-1-1.mp4", hours), put("tmp/other.bin", old), put("thumbs/cam-5.jpg", old), put("thumbs/cam-6.12-2.part", std::time::SystemTime::now())];
        let busy = super::TempInUse::new(put("tmp/export-1-1.zip", hours));
        super::sweep_temp_files(&d, true);
        for p in &gone {
            assert!(!p.exists(), "{}", p.display());
        }
        for p in kept.iter().chain([&busy.path().to_path_buf()]) {
            assert!(p.exists(), "{}", p.display());
        }
        // Without the pictures: tmp/ only.
        let part = put("thumbs/cam-7.12-3.part", old);
        super::sweep_temp_files(&d, false);
        assert!(part.exists());
        // In use no more: the file goes with it.
        let p = busy.path().to_path_buf();
        drop(busy);
        assert!(!p.exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
