use super::*;

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
        let s = self.db.oldest_segments_in(name, 1).ok()?.into_iter().next()?;
        Some(((Utc::now().timestamp_millis() - s.start_ms).max(0)) as f64 / 86_400_000.0)
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
        // Older ones a stop or crash left out are added by the folder sweep.
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
    let _ = std::fs::remove_file(dir.join("clips").join(format!("{id}.mp4")));
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
        if passes % 60 == 0 {
            sweep_ai_frames(&engine);
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

/// Clears a "running" flag when dropped, also when the thread panics.
pub(super) struct ClearOnDrop(pub(super) &'static std::sync::atomic::AtomicBool);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

static SWEEPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
const SWEEP_FIRST_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Whether a folder sweep should start now (and none is running); marks it running.
fn sweep_due() -> bool {
    // When the next one is due (the first: 30 minutes after start).
    static NEXT_SWEEP: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut next = NEXT_SWEEP.lock().unwrap();
    let now = std::time::Instant::now();
    let due_at = *next.get_or_insert(now + SWEEP_FIRST_AFTER);
    if now < due_at || SWEEPING.swap(true, std::sync::atomic::Ordering::SeqCst) {
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

/// Deletes files past `cutoff` the index does not cover: stray recordings in
/// every camera folder on every drive, and clips/pictures with no event.
fn folder_sweep(engine: &Arc<Engine>, cutoff: i64, paced: bool) {
    let started = std::time::Instant::now();
    let mut pace = Pace { on: paced, n: 0 };
    let dir = &engine.data_dir;
    // Recordings closed but never indexed (the server stopped before the
    // indexer got to them, or crashed): add the recent ones, so playback,
    // caps and retention see them. Only the last two days: older ones
    // would already have been found by an earlier sweep.
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
    match engine.db.import_recent_camera_dirs(&dirs, seg, since_ms) {
        Ok(n) if n > 0 => tracing::info!("folder sweep: added {n} recording(s) missing from the database"),
        Ok(_) => {}
        Err(e) => tracing::warn!("folder sweep: could not add missing recordings: {e:#}"),
    }
    pace.folder();
    let mut roots = vec![dir.clone()];
    roots.extend(engine.config.lock().unwrap().storage.folders.iter().map(|f| f.path.clone()));
    for root in &roots {
        if let Ok(entries) = std::fs::read_dir(root.join("cameras")) {
            for cam in entries.flatten() {
                prune_dir(&cam.path().join("segments"), cutoff, &mut pace);
                pace.folder();
            }
        }
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
        tracing::info!("folder sweep looked at {} files in {} s", pace.n, started.elapsed().as_secs());
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
/// their clip/thumbnail) are never deleted by either.
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
        let old: Vec<_> = db
            .oldest_segments(500)?
            .into_iter()
            .filter(|(_, s)| s.end_ms < cutoff * 1000)
            .collect();
        if old.is_empty() {
            break;
        }
        for (_, s) in &old {
            let path = engine.segment_path(s);
            let _ = std::fs::remove_file(&path);
            crate::ingest::splice::forget(&path);
            super::playback::recordings_moved();
            db.delete_segment(&s.path)?;
        }
    }
    // Folder sweeps (files the index missed, orphan clips and pictures) list
    // hundreds of thousands of files on the recording disks: every 6 hours
    // (first one 30 minutes after start), on their own thread, paced so the
    // disk the cameras record to is never busy for long. (Recordings in the
    // index go above, every pass.)
    if cfg!(test) {
        folder_sweep(engine, cutoff, false);
    } else if sweep_due() {
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
        for &i in &members {
            let (name, root, _) = &roots[i];
            for (s, kept) in engine.db.segments_to_purge(name, kept_from, 5000)? {
                home.insert(root.join(&s.path), i);
                cands.push(PurgeCandidate { time: s.start_ms / 1000, size: s.bytes.max(0) as u64, kind: PurgeKind::Segment { path: root.join(&s.path), newest: false }, kept });
            }
        }
        let data_here = members.contains(&0);
        if data_here {
            for ev in engine.db.oldest_unprotected_events(5000)? {
                let size = event_files_size(dir, &ev.id);
                let kept = ev.start * 1000 >= kept_from;
                cands.push(PurgeCandidate { time: ev.start, size, kind: PurgeKind::Event { id: ev.id, protect: false }, kept });
            }
        }
        let picked = select_purge(cands, need);
        let freed: u64 = picked.iter().map(|c| c.size).sum();
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
        for c in picked {
            match c.kind {
                PurgeKind::Segment { path, .. } => {
                    let Some(&i) = home.get(&path) else { continue };
                    let (_, root, is_archive) = &roots[i];
                    let rel = crate::db::relative_path(root, &path);
                    let dest = if *is_archive { None } else { to_archive };
                    if let Err(e) = retire_segment(engine, &rel, &path, dest) {
                        retire_failed(&e);
                    }
                }
                PurgeKind::Event { id, .. } => {
                    remove_event_files(dir, &id);
                    purged.push(id);
                }
            }
        }
        engine.db.delete_events(&purged)?;
    }
    Ok(())
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
            let _ = std::fs::remove_file(src);
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

/// Delete files in `dir` last modified before `cutoff` (epoch seconds).
fn prune_dir(dir: &std::path::Path, cutoff: i64, pace: &mut Pace) {
    if let Ok(files) = std::fs::read_dir(dir) {
        for f in files.flatten() {
            pace.file();
            let modified = f
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).timestamp())
                .unwrap_or(0);
            if modified < cutoff {
                let _ = std::fs::remove_file(f.path());
                crate::ingest::splice::forget(&f.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
