//! Recordings the database knows whose files are gone: a folder emptied by
//! hand, a drive reformatted or replaced, a database brought over from
//! another computer without all of its recordings.
//!
//! Recordings are stored as (folder name, path relative to that folder), so
//! moving a folder only needs its path changed in Settings. What is really
//! gone is found and forgotten here:
//!
//! - `detect` (at start, cheap): a couple of dozen recordings per folder,
//!   spread over the time it covers, are looked for on disk. A folder that is
//!   not there, whose recordings are mostly missing, or that looks like an
//!   unmounted drive's mount point, is noted (log line and a note in
//!   Settings > Recording).
//! - `scan` (the dry run): every indexed recording is looked for (one folder
//!   listing per directory, not one disk lookup per file), with counts and
//!   sizes per folder and camera, and what that means for events.
//! - `apply`: removes the database rows of the missing recordings (never a
//!   file), a few thousand per transaction, each looked for again first, after
//!   a backup of the database. A folder whose missing recordings could just as
//!   well be a wrong path or a drive that is not connected (not there, not in
//!   the settings, none or most of its recordings missing, a camera's
//!   recordings all missing, or on the system disk where a drive is expected)
//!   is left alone unless it is named explicitly.
//!
//! Events: an event whose video was all in the recordings removed (and that
//! has no saved clip file) stays in the event list with its picture, AI
//! labels and faces, marked "No clip" (its clip address is cleared, so
//! nothing offers to play it, and retention's "outlived its video" rule
//! leaves it to age retention). So does an event the removal would leave
//! older than its camera's oldest recording. Protected events are never
//! changed. The events marked are written to a file first, so a stop part
//! way through can be put right at the next start.
//!
//! One check or removal at a time, also between the server and the command
//! line: a lock file in the data folder.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{slug, Config};
use crate::db::Db;

/// Rows read at a time.
const BATCH: usize = 5000;
/// Rows removed per transaction (the server may be running and recording).
const SERVER_BATCH: usize = 1000;
/// Recordings looked for per folder at start.
const SAMPLE: usize = 24;
/// A dry run is applied only this long after it was made.
pub const SCAN_VALID: Duration = Duration::from_secs(3600);
/// Names kept from one directory listing; a bigger directory is looked up file by file.
const DIR_CAP: usize = 1_000_000;
/// Names kept from all listings together.
const ALL_DIRS_CAP: usize = 6_000_000;
/// Events marked "No clip" by a removal not finished yet (see `recover_marked`).
const MARKED_FILE: &str = "missing-marked.json";
/// One check or removal at a time (server and command line).
const LOCK_FILE: &str = "missing-check.lock";

/// A recording folder: "" is the data folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub name: String,
    pub path: PathBuf,
    /// Not in the settings (recordings indexed under a folder that was
    /// removed or renamed): looked for in the data folder, as playback does.
    pub configured: bool,
}

/// The data folder and every recording folder in the settings.
pub fn roots(cfg: &Config, data_dir: &Path) -> Vec<Root> {
    let mut v = vec![Root { name: String::new(), path: data_dir.to_path_buf(), configured: true }];
    v.extend(cfg.storage.folders.iter().map(|f| Root { name: f.name.clone(), path: f.path.clone(), configured: true }));
    v
}

/// The folder a recording's folder name points at (an unknown name: the data folder).
fn root_for(roots: &[Root], name: &str) -> Root {
    match roots.iter().find(|r| r.name == name) {
        Some(r) => r.clone(),
        None => Root { name: name.to_string(), path: roots.first().map(|r| r.path.clone()).unwrap_or_default(), configured: false },
    }
}

/// How a folder is named to people (and on the command line).
pub fn label(name: &str) -> String {
    if name.is_empty() { "data folder".into() } else { name.to_string() }
}

/// A folder named on the command line or by the page: "data" (or "") is the
/// data folder unless a recording folder is really called that.
pub fn folder_name_from(arg: &str, roots: &[Root]) -> String {
    let a = arg.trim();
    if roots.iter().any(|r| r.name == a) {
        return a.to_string();
    }
    if a.is_empty() || a.eq_ignore_ascii_case("data") || a.eq_ignore_ascii_case("data folder") {
        return String::new();
    }
    a.to_string()
}

/// Whether a stored path is an absolute path (of this or another OS): rows
/// written that way are reported, they cannot follow a folder setting.
pub fn looks_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with('\\') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// For log lines: no control characters (a folder name or path is the settings' text).
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// A folder that is there, but on a drive that is not mounted (Linux): the
/// recorder then writes new recordings into the empty mount point on the
/// system disk, and the old ones look deleted. True when the folder or a
/// folder above it is a mount point in /etc/fstab that is not mounted now;
/// for paths fstab does not cover, when it is under /mnt, /media or
/// /run/media with no mount between (an empty mount-point folder). Other
/// systems: never (not told reliably).
#[cfg(target_os = "linux")]
pub fn looks_unmounted(p: &Path) -> bool {
    let Ok(p) = std::fs::canonicalize(p) else { return false };
    let fstab = std::fs::read_to_string("/etc/fstab").map(|t| fstab_mount_points(&t)).unwrap_or_default();
    let mounted = std::fs::read_to_string("/proc/self/mountinfo").map(|t| mounted_points(&t)).unwrap_or_default();
    if let Some(v) = unmounted_by_fstab(&p, &fstab, &mounted) {
        return v;
    }
    use std::os::unix::fs::MetadataExt;
    let dev = |q: &Path| std::fs::metadata(q).map(|m| m.dev()).ok();
    for base in ["/run/media", "/media", "/mnt"] {
        let base = Path::new(base);
        if p.starts_with(base) && p != base {
            return dev(&p).is_some() && dev(&p) == dev(base);
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
pub fn looks_unmounted(_p: &Path) -> bool {
    false
}

/// fstab's escapes in paths (`\040` for a space).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(((b[i + 1] - b'0') << 6) | ((b[i + 2] - b'0') << 3) | (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mount points in /etc/fstab (not swap; `noauto` ones count: they are expected too).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn fstab_mount_points(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (mp, ty) = (f.get(1)?, f.get(2).copied().unwrap_or(""));
            (ty != "swap" && mp.starts_with('/')).then(|| PathBuf::from(unescape_octal(mp)))
        })
        .collect()
}

/// Mount points mounted now (/proc/self/mountinfo, the fifth field).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn mounted_points(text: &str) -> HashSet<PathBuf> {
    text.lines().filter_map(|l| l.split_whitespace().nth(4)).map(|m| PathBuf::from(unescape_octal(m))).collect()
}

/// The deepest fstab mount point at or above `p`: whether it is not mounted
/// (None: no fstab entry covers `p`, "/" left out).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unmounted_by_fstab(p: &Path, fstab: &[PathBuf], mounted: &HashSet<PathBuf>) -> Option<bool> {
    p.ancestors().filter(|a| *a != Path::new("/")).find(|a| fstab.iter().any(|f| f == a)).map(|a| !mounted.contains(a))
}

// ---- looking for files ----

enum Dir {
    /// The directory is not there.
    Gone,
    /// Not there, but one whose name differs only in capitals is.
    GoneCase,
    /// Its file names, and the same in lower case.
    Listed(HashSet<OsString>, HashSet<String>),
    /// Could not be listed (permissions, a share that does not answer, too
    /// big): each file is looked up on its own.
    Unlisted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    Here,
    Gone,
    /// Not there, but a name differing only in capitals is (a folder renamed):
    /// kept and reported.
    Case,
    /// Could not be told (an error other than "not found"): kept.
    Unknown,
}

/// Looks for files one directory listing at a time.
pub struct Files {
    dirs: HashMap<PathBuf, Dir>,
    names: usize,
    /// Rows up to this id were in the database before any listing was made, so
    /// their files (if any) were too: one missing from a listing is gone. Later
    /// ones are looked up on their own (written since the listing).
    known_before: i64,
}

impl Files {
    pub fn new(known_before: i64) -> Files {
        Files { dirs: HashMap::new(), names: 0, known_before }
    }

    fn list(&mut self, dir: &Path) -> Dir {
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                let mut names = HashSet::new();
                for e in rd.filter_map(|e| e.ok()) {
                    if names.len() >= DIR_CAP || self.names + names.len() >= ALL_DIRS_CAP {
                        return Dir::Unlisted;
                    }
                    names.insert(e.file_name());
                }
                self.names += names.len();
                let lower = names.iter().map(|n| n.to_string_lossy().to_lowercase()).collect();
                Dir::Listed(names, lower)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A folder renamed with other capitals (on a disk that tells them apart)?
                let case = match (dir.parent(), dir.file_name()) {
                    (Some(up), Some(me)) => {
                        let me = me.to_string_lossy().to_lowercase();
                        std::fs::read_dir(up).map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().to_lowercase() == me)).unwrap_or(false)
                    }
                    _ => false,
                };
                if case { Dir::GoneCase } else { Dir::Gone }
            }
            Err(_) => Dir::Unlisted,
        }
    }

    /// Whether the file of row `id` at `full` is there.
    pub fn check(&mut self, full: &Path, id: i64) -> Seen {
        let (Some(parent), Some(name)) = (full.parent(), full.file_name()) else { return Seen::Unknown };
        if !self.dirs.contains_key(parent) {
            let d = self.list(parent);
            self.dirs.insert(parent.to_path_buf(), d);
        }
        match &self.dirs[parent] {
            Dir::Gone => Seen::Gone,
            Dir::GoneCase => Seen::Case,
            Dir::Unlisted => stat(full),
            Dir::Listed(names, _) if names.contains(name) => Seen::Here,
            Dir::Listed(_, lower) => {
                // Spelled with other capitals: on Windows that is the same file.
                if lower.contains(&name.to_string_lossy().to_lowercase()) {
                    return if cfg!(windows) { stat(full) } else { Seen::Case };
                }
                if id <= self.known_before { Seen::Gone } else { stat(full) }
            }
        }
    }
}

fn stat(p: &Path) -> Seen {
    match std::fs::metadata(p) {
        Ok(_) => Seen::Here,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Seen::Gone,
        Err(_) => Seen::Unknown,
    }
}

// ---- the dry run ----

#[derive(Debug, Clone, Default, Serialize)]
pub struct CameraMissing {
    pub camera: String,
    pub rows: u64,
    pub missing: u64,
    pub missing_bytes: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FolderReport {
    pub name: String,
    pub label: String,
    pub path: String,
    pub configured: bool,
    /// The folder itself is there.
    pub exists: bool,
    /// ok | some_missing | all_missing (there, but none of its recordings
    /// are) | not_found (the folder is not there) | not_configured (no such
    /// folder in the settings any more).
    pub state: String,
    /// It is on the system disk where a drive of its own is expected (see `looks_unmounted`).
    pub not_mounted: bool,
    pub rows: u64,
    pub bytes: i64,
    pub missing: u64,
    pub missing_bytes: i64,
    /// Recordings that could not be looked for (kept).
    pub unknown: u64,
    /// Not there, but a name differing only in capitals is (kept).
    pub case_differs: u64,
    pub oldest_missing_ms: Option<i64>,
    pub newest_missing_ms: Option<i64>,
    /// Rows stored with a full path instead of one relative to the folder.
    pub absolute_paths: u64,
    /// Why its missing recordings are only removed when the folder is named:
    /// not_found, not_configured, all_missing, most_missing (over half),
    /// cameras_all_missing (see `whole_cameras`), not_mounted.
    pub confirm_why: Vec<String>,
    pub needs_confirm: bool,
    /// Cameras none of whose recordings in this folder are there.
    pub whole_cameras: Vec<String>,
    /// Cameras with recordings missing, most first.
    pub cameras: Vec<CameraMissing>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub id: String,
    pub made_ms: i64,
    pub took_ms: u64,
    pub rows: u64,
    pub missing: u64,
    pub missing_bytes: i64,
    pub folders: Vec<FolderReport>,
    /// Events that would have no video left when the folders that need no naming are done:
    /// unprotected ones are kept and marked "No clip"; protected ones are not changed.
    pub events_no_video: u64,
    pub protected_no_video: u64,
    /// Events that keep part of their video.
    pub events_part_video: u64,
    /// The same as `events_no_video` with every folder included.
    pub events_no_video_all: u64,
}

/// A dry run, kept to be applied.
pub struct Scan {
    pub report: Report,
    /// (row id, index into report.folders), in id order.
    missing: Vec<(i64, u32)>,
    /// The folders as they were set when it was made.
    pub roots: Vec<Root>,
    pub made: Instant,
}

/// Pre/post roll seconds of a camera (by name), for an event's video span.
pub type Rolls<'a> = &'a (dyn Fn(&str) -> (i64, i64) + Sync);

/// Progress: (phase, done, total).
pub type Progress<'a> = &'a (dyn Fn(&str, u64, u64) + Sync);

/// Look for every indexed recording on disk. Reads only (on a reading
/// connection, in pieces): safe while the server runs.
pub fn scan(db: &Db, data_dir: &Path, roots: &[Root], rolls: Rolls, progress: Progress) -> Result<Scan> {
    let started = Instant::now();
    let total = db.segment_count_estimate()?.max(0) as u64;
    let mut folders: Vec<FolderReport> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut cams: Vec<HashMap<String, CameraMissing>> = Vec::new();
    let mut files = Files::new(db.segment_max_id()?);
    let mut missing: Vec<(i64, u32)> = Vec::new();
    let mut after = 0i64;
    let mut done = 0u64;
    progress("recordings", 0, total);
    loop {
        let rows = db.segment_refs_after(after, BATCH)?;
        let Some(last) = rows.last() else { break };
        after = last.id;
        for r in &rows {
            let i = *index.entry(r.folder.clone()).or_insert_with(|| {
                let root = root_for(roots, &r.folder);
                let exists = root.path.is_dir();
                folders.push(FolderReport {
                    name: root.name.clone(),
                    label: label(&root.name),
                    path: root.path.display().to_string(),
                    configured: root.configured,
                    exists,
                    not_mounted: root.configured && exists && looks_unmounted(&root.path),
                    ..Default::default()
                });
                cams.push(HashMap::new());
                folders.len() - 1
            });
            let f = &mut folders[i];
            f.rows += 1;
            f.bytes += r.bytes;
            if looks_absolute(&r.path) {
                f.absolute_paths += 1;
            }
            let c = cams[i].entry(r.camera.clone()).or_insert_with(|| CameraMissing { camera: r.camera.clone(), ..Default::default() });
            c.rows += 1;
            let full = Path::new(&f.path).join(&r.path);
            match files.check(&full, r.id) {
                Seen::Here => {}
                Seen::Unknown => f.unknown += 1,
                Seen::Case => f.case_differs += 1,
                Seen::Gone => {
                    f.missing += 1;
                    f.missing_bytes += r.bytes;
                    f.oldest_missing_ms = Some(f.oldest_missing_ms.map_or(r.start_ms, |m| m.min(r.start_ms)));
                    f.newest_missing_ms = Some(f.newest_missing_ms.map_or(r.start_ms, |m| m.max(r.start_ms)));
                    c.missing += 1;
                    c.missing_bytes += r.bytes;
                    missing.push((r.id, i as u32));
                }
            }
        }
        done += rows.len() as u64;
        progress("recordings", done, total.max(done));
    }
    drop(files);
    for (f, c) in folders.iter_mut().zip(cams) {
        let mut c: Vec<CameraMissing> = c.into_values().collect();
        c.sort_by(|a, b| b.missing.cmp(&a.missing).then(a.camera.cmp(&b.camera)));
        f.whole_cameras = c.iter().filter(|c| c.rows > 0 && c.missing == c.rows).map(|c| c.camera.clone()).collect();
        f.cameras = c.into_iter().filter(|c| c.missing > 0).collect();
        f.state = folder_state(f).into();
        f.confirm_why = confirm_why(f);
        f.needs_confirm = f.missing > 0 && !f.confirm_why.is_empty();
    }
    // Configured folders with no recordings indexed: listed too (so a wrong path shows).
    for r in roots {
        if !index.contains_key(&r.name) {
            let exists = r.path.is_dir();
            folders.push(FolderReport { name: r.name.clone(), label: label(&r.name), path: r.path.display().to_string(), configured: r.configured, exists, state: if exists { "ok" } else { "not_found" }.into(), ..Default::default() });
        }
    }
    folders.sort_by(|a, b| (!a.name.is_empty(), &a.name).cmp(&(!b.name.is_empty(), &b.name)));
    // (index -> position after sorting)
    let pos: HashMap<String, u32> = folders.iter().enumerate().map(|(i, f)| (f.name.clone(), i as u32)).collect();
    let by_old: HashMap<usize, String> = index.into_iter().map(|(k, v)| (v, k)).collect();
    for m in missing.iter_mut() {
        m.1 = pos[&by_old[&(m.1 as usize)]];
    }
    // What it means for events: with the folders that need no naming, and with all.
    let plain: Vec<i64> = missing.iter().filter(|m| !folders[m.1 as usize].needs_confirm).map(|m| m.0).collect();
    let all: Vec<i64> = missing.iter().map(|m| m.0).collect();
    let ev = events_plan(db, data_dir, &plain, rolls, progress)?;
    let ev_all = if plain.len() == all.len() { None } else { Some(events_plan(db, data_dir, &all, rolls, progress)?) };
    let report = Report {
        id: format!("{:x}", rand_id()),
        made_ms: chrono::Utc::now().timestamp_millis(),
        took_ms: started.elapsed().as_millis() as u64,
        rows: folders.iter().map(|f| f.rows).sum(),
        missing: folders.iter().map(|f| f.missing).sum(),
        missing_bytes: folders.iter().map(|f| f.missing_bytes).sum(),
        events_no_video: ev.no_video.len() as u64,
        protected_no_video: ev_all.as_ref().unwrap_or(&ev).protected,
        events_part_video: ev_all.as_ref().unwrap_or(&ev).part,
        events_no_video_all: ev_all.as_ref().unwrap_or(&ev).no_video.len() as u64,
        folders,
    };
    Ok(Scan { report, missing, roots: roots.to_vec(), made: Instant::now() })
}

fn rand_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.finish()
}

/// The state of a folder from its counts (see FolderReport::state).
pub fn folder_state(f: &FolderReport) -> &'static str {
    if !f.configured {
        if f.missing == 0 { "ok" } else { "not_configured" }
    } else if !f.exists {
        "not_found"
    } else if f.rows > 0 && f.missing == f.rows {
        "all_missing"
    } else if f.missing > 0 {
        "some_missing"
    } else {
        "ok"
    }
}

/// Why a folder's missing recordings need the folder named (see FolderReport::confirm_why).
pub fn confirm_why(f: &FolderReport) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    match f.state.as_str() {
        s @ ("not_found" | "not_configured" | "all_missing") => v.push(s.into()),
        "some_missing" => {
            if f.missing * 2 > f.rows {
                v.push("most_missing".into());
            }
            if !f.whole_cameras.is_empty() {
                v.push("cameras_all_missing".into());
            }
        }
        _ => {}
    }
    if f.not_mounted && f.missing > 0 {
        v.push("not_mounted".into());
    }
    v
}

/// Clips saved as files (their events play from those, whatever the recordings).
fn saved_clips(data_dir: &Path) -> HashSet<String> {
    std::fs::read_dir(data_dir.join("clips"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".mp4")).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// What removing the recordings `gone` (sorted ids) means for events.
#[derive(Default)]
struct EventPlan {
    /// Unprotected events with no saved clip that would have no video left: all
    /// of it in `gone`, or none in their own time and older than what would be
    /// their camera's oldest recording (retention would take them as having
    /// outlived their video).
    no_video: Vec<String>,
    protected: u64,
    part: u64,
}

fn events_plan(db: &Db, data_dir: &Path, gone: &[i64], rolls: Rolls, progress: Progress) -> Result<EventPlan> {
    let mut out = EventPlan::default();
    if gone.is_empty() {
        return Ok(out);
    }
    let clips = saved_clips(data_dir);
    // Each camera's oldest recording now and after the removal.
    let mut oldest: HashMap<String, (Option<i64>, Option<i64>)> = HashMap::new();
    for cam in db.segment_cameras()? {
        let now = db.oldest_segment_start(&cam)?;
        let after = db.first_segment_start_not_in(&cam, gone)?;
        oldest.insert(cam, (now, after));
    }
    let mut after = String::new();
    let mut done = 0u64;
    loop {
        let evs = db.events_with_clip_after(&after, BATCH)?;
        let Some(last) = evs.last() else { break };
        after = last.id.clone();
        for ev in &evs {
            if clips.contains(&ev.id) {
                continue;
            }
            let (pre, post) = rolls(&ev.camera);
            let cam = slug(&ev.camera);
            let ids = db.segment_ids_overlapping(&cam, (ev.start - pre) * 1000, (ev.end + post) * 1000)?;
            if ids.is_empty() {
                // No video of its own: safe from retention while it is not older than the
                // camera's oldest recording; the removal could change that.
                let end_ms = (ev.end + post) * 1000;
                if let Some((Some(now), Some(then))) = oldest.get(&cam) {
                    if !ev.protect && end_ms >= *now && end_ms < *then {
                        out.no_video.push(ev.id.clone());
                    }
                }
                continue;
            }
            let g = ids.iter().filter(|id| gone.binary_search(id).is_ok()).count();
            if g == ids.len() {
                if ev.protect {
                    out.protected += 1;
                } else {
                    out.no_video.push(ev.id.clone());
                }
            } else if g > 0 {
                out.part += 1;
            }
        }
        done += evs.len() as u64;
        progress("events", done, done);
    }
    Ok(out)
}

// ---- events marked during a removal ----

fn marked_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MARKED_FILE)
}

/// Written to disk for good (a power cut after this keeps it) before any event is marked.
fn write_marked(data_dir: &Path, ids: &[String]) -> Result<()> {
    use std::io::Write;
    let p = marked_path(data_dir);
    let tmp = p.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(&serde_json::to_vec(&serde_json::json!({ "ids": ids }))?).with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().with_context(|| format!("write {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &p).with_context(|| format!("write {}", p.display()))?;
    #[cfg(unix)]
    std::fs::File::open(data_dir).and_then(|d| d.sync_all()).with_context(|| format!("sync {}", data_dir.display()))?;
    Ok(())
}

/// Events a removal marked "No clip" that have video after all (the removal
/// stopped part way, a recording moved to the archive meanwhile, or the event
/// was protected meanwhile) get their clip address back; the record of them
/// is then dropped. Returns (kept marked, restored).
pub fn recover_marked(db: &Db, data_dir: &Path, rolls: Rolls) -> Result<(u64, u64)> {
    let p = marked_path(data_dir);
    let ids: Vec<String> = match std::fs::read(&p) {
        Ok(b) => match serde_json::from_slice::<serde_json::Value>(&b).ok().and_then(|v| v["ids"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())) {
            Some(ids) => ids,
            // Kept as it is: the events in it are not known any more. Nothing new is removed
            // until someone has looked.
            None => anyhow::bail!(
                "{} (events an earlier removal of missing recordings marked \"No clip\") cannot be read; no removal runs until it is fixed or deleted (the events stay marked; restoring the backup made before that removal puts them back)",
                p.display()
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e).with_context(|| format!("read {}", p.display())),
    };
    let (mut kept, mut restored, mut failed) = (0u64, 0u64, 0u64);
    for id in &ids {
        let r = (|| -> Result<()> {
            let Some((ev, has_clip)) = db.event_ref(id)? else { return Ok(()) };
            if has_clip {
                return Ok(());
            }
            let (pre, post) = rolls(&ev.camera);
            let has_video = !db.segment_ids_overlapping(&slug(&ev.camera), (ev.start - pre) * 1000, (ev.end + post) * 1000)?.is_empty();
            if (ev.protect || has_video) && db.restore_event_clip(id)? {
                restored += 1;
            } else {
                kept += 1;
            }
            Ok(())
        })();
        if let Err(e) = r {
            failed += 1;
            tracing::warn!("storage: could not look at event {}: {e:#}", clean(id));
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} events marked \"No clip\" could not be looked at again; tried again at the next start");
    }
    std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
    Ok((kept, restored))
}

// ---- one at a time ----

/// Held while a check or removal runs (server or command line).
pub struct JobLock(std::fs::File);

impl Drop for JobLock {
    // Unlocked here rather than by closing the file: a child process another thread is just
    // starting (forked, not yet running its program) holds a copy of the descriptor, and the
    // lock would last until that copy is closed.
    fn drop(&mut self) {
        let _ = fs4::fs_std::FileExt::unlock(&self.0);
    }
}

pub fn lock(data_dir: &Path) -> Result<JobLock> {
    use fs4::fs_std::FileExt;
    let p = data_dir.join(LOCK_FILE);
    // Opened for reading (enough to lock it), so the service's account and an administrator
    // using the command line can both; made readable and writable by the group when new.
    let f = match std::fs::File::open(&p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut o = std::fs::OpenOptions::new();
            o.create(true).truncate(false).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o660);
            }
            o.open(&p).with_context(|| format!("create {}", p.display()))?;
            std::fs::File::open(&p).with_context(|| format!("open {}", p.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("open {}", p.display())),
    };
    match f.try_lock_exclusive() {
        Ok(true) => Ok(JobLock(f)),
        Ok(false) => anyhow::bail!("another check or removal of missing recordings is running (in the server or on the command line); try again when it has finished"),
        Err(e) => Err(e).with_context(|| format!("lock {}", p.display())),
    }
}

// ---- applying ----

#[derive(Debug, Clone, Default, Serialize)]
pub struct Applied {
    pub scan_id: String,
    pub removed: u64,
    pub removed_bytes: i64,
    /// Found on disk again when looked for once more (kept).
    pub found_again: u64,
    /// Moved to another folder (the archive) since the check (kept).
    pub moved: u64,
    /// Not there, but a name differing only in capitals is (kept).
    pub case_differs: u64,
    /// Folders left alone (named on the page or command line to include them).
    pub skipped_folders: Vec<String>,
    pub skipped_rows: u64,
    pub events_marked: u64,
    pub backup: Option<String>,
    pub took_ms: u64,
}

/// Why a dry run cannot be applied as it is (None: it can).
pub fn stale(scan: &Scan, roots_now: &[Root]) -> Option<&'static str> {
    if scan.made.elapsed() > SCAN_VALID {
        return Some("the check is more than an hour old; run it again");
    }
    if scan.roots != roots_now {
        return Some("the recording folders changed since the check; run it again");
    }
    None
}

/// Which folders of a dry run take part, as they stand now: those that need
/// no naming (and have not gone missing or come to look unmounted since), and
/// those named in `include`. Also (skipped folder labels, skipped rows).
fn taking_part(scan: &Scan, include: &[String]) -> (Vec<bool>, Vec<String>, u64) {
    let (mut labels, mut rows) = (Vec::new(), 0u64);
    let allowed = scan
        .report
        .folders
        .iter()
        .map(|f| {
            let named = include.iter().any(|n| *n == f.name);
            let p = Path::new(&f.path);
            let changed = (f.exists && !p.is_dir()) || (f.configured && !f.not_mounted && looks_unmounted(p));
            let ok = named || (!f.needs_confirm && !changed);
            if !ok && f.missing > 0 {
                labels.push(f.label.clone());
                rows += f.missing;
            }
            ok
        })
        .collect();
    (allowed, labels, rows)
}

/// How many rows applying `scan` with `include` would look at (0: nothing to do, no backup needed).
pub fn to_remove(scan: &Scan, include: &[String]) -> u64 {
    let (allowed, _, _) = taking_part(scan, include);
    scan.missing.iter().filter(|m| allowed[m.1 as usize]).count() as u64
}

/// Remove the database rows of the missing recordings found by `scan`
/// (never a file). Each is looked for again first; a folder that needs
/// confirming is included only when named in `include` (folder names).
/// `batch` rows per transaction, `pause` between them lets recording and
/// playback in. Make a backup first (the caller) and hold the lock.
#[allow(clippy::too_many_arguments)]
pub fn apply(db: &Db, data_dir: &Path, scan: &Scan, include: &[String], rolls: Rolls, batch: usize, pause: Duration, progress: Progress) -> Result<Applied> {
    let started = Instant::now();
    let mut out = Applied { scan_id: scan.report.id.clone(), ..Default::default() };
    // A removal that stopped part way: its events first.
    recover_marked(db, data_dir, rolls)?;
    let (allowed, labels, rows) = taking_part(scan, include);
    out.skipped_folders = labels;
    out.skipped_rows = rows;
    let todo: Vec<(i64, u32)> = scan.missing.iter().copied().filter(|m| allowed[m.1 as usize]).collect();
    let total = todo.len() as u64;
    // 1. Each one looked for once more, file by file (reading only).
    let mut files = Files::new(i64::MIN);
    // (id, folder index, start, path fingerprint): against an id used again since.
    let mut gone: Vec<(i64, u32, i64, u64)> = Vec::with_capacity(todo.len());
    let mut done = 0u64;
    progress("checking", 0, total);
    for chunk in todo.chunks(BATCH) {
        let ids: Vec<i64> = chunk.iter().map(|m| m.0).collect();
        let folder_of: HashMap<i64, u32> = chunk.iter().copied().collect();
        for r in db.segment_refs_by_id(&ids)? {
            let fi = folder_of[&r.id];
            let f = &scan.report.folders[fi as usize];
            // Moved to another folder since (the archive): not this one's to remove.
            if r.folder != f.name {
                out.moved += 1;
                continue;
            }
            match files.check(&Path::new(&f.path).join(&r.path), r.id) {
                Seen::Gone => gone.push((r.id, fi, r.start_ms, crate::db::path_hash(&r.path))),
                Seen::Case => out.case_differs += 1,
                _ => out.found_again += 1,
            }
        }
        done += chunk.len() as u64;
        progress("checking", done, total);
    }
    drop(files);
    // 2. Events that would have no video left: written down, then marked "No clip", before the
    // rows go, so retention (which removes events that outlived their video) does not take them.
    let ids: Vec<i64> = gone.iter().map(|g| g.0).collect();
    let plan = events_plan(db, data_dir, &ids, rolls, progress)?;
    write_marked(data_dir, &plan.no_video)?;
    for (i, id) in plan.no_video.iter().enumerate() {
        if i % 1000 == 0 {
            progress("events", i as u64, plan.no_video.len() as u64);
        }
        db.clear_event_clip(id)?;
    }
    // 3. The rows, a few at a time.
    let removal = (|| -> Result<()> {
        let mut done = 0u64;
        let total = gone.len() as u64;
        progress("removing", 0, total);
        for chunk in gone.chunks(batch.max(1)) {
            let rows: Vec<(i64, &str, i64, u64)> = chunk.iter().map(|g| (g.0, scan.report.folders[g.1 as usize].name.as_str(), g.2, g.3)).collect();
            let (n, b) = db.delete_segments_in(&rows)?;
            out.removed += n;
            out.removed_bytes += b;
            done += chunk.len() as u64;
            progress("removing", done, total);
            if !pause.is_zero() {
                std::thread::sleep(pause);
            }
        }
        Ok(())
    })();
    // 4. A marked event whose video is still there after all, or protected meanwhile, plays again.
    let (kept, _restored) = recover_marked(db, data_dir, rolls)?;
    out.events_marked = kept;
    removal?;
    out.took_ms = started.elapsed().as_millis() as u64;
    Ok(out)
}

// ---- noticing at start ----

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Note {
    pub name: String,
    pub label: String,
    pub path: String,
    /// not_found | not_mounted | mostly_missing | not_configured
    pub state: String,
    pub sampled: usize,
    pub missing: usize,
}

impl Note {
    pub fn text(&self) -> String {
        let which = if self.name.is_empty() { format!("the data folder ({})", self.path) } else { format!("recording folder {} ({})", self.name, self.path) };
        const TOOL: &str = "Settings > Recording > Drives & folders, Recordings whose files are gone";
        match self.state.as_str() {
            "not_found" => format!("{which} is not there: a drive not connected, or the folder moved (set its path in Settings > Recording > Drives & folders). If its recordings are gone for good, remove them from the database in {TOOL}"),
            "not_mounted" => format!("{which}: {} of {} recordings looked for are missing, and the drive it is on is set to be mounted there but is not. Is the drive mounted? Mount it (and make it mount at start), then restart AmberSight; until then new recordings go to the system disk. If this folder is meant to be on the system disk, check it again in Settings > Recording > Drives & folders", self.missing, self.sampled),
            "not_configured" => format!("recordings are listed under a folder named '{}' that is not in the settings, and {} of {} looked for are not in the data folder: add the folder again, or remove them from the database in {TOOL}", self.name, self.missing, self.sampled),
            _ => format!("{which}: {} of {} recordings looked for are missing. If the folder moved, set its new path in Settings > Recording > Drives & folders; if they are gone for good, remove them from the database in {TOOL}", self.missing, self.sampled),
        }
    }
}

/// Look for a few recordings of each folder (cheap on any database size).
pub fn detect(db: &Db, roots: &[Root]) -> Result<Vec<Note>> {
    let mut names: Vec<String> = roots.iter().map(|r| r.name.clone()).collect();
    if let Some(more) = db.segment_folders_cheap()? {
        for n in more {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    let mut notes = Vec::new();
    for name in names {
        let sample = db.segment_sample(&name, SAMPLE)?;
        if sample.is_empty() {
            continue;
        }
        let root = root_for(roots, &name);
        let mut files = Files::new(i64::MIN);
        let missing = sample.iter().filter(|p| files.check(&root.path.join(p), 0) == Seen::Gone).count();
        let state = if !root.configured {
            if missing * 2 > sample.len() { Some("not_configured") } else { None }
        } else if !root.path.is_dir() {
            Some("not_found")
        } else if missing * 2 > sample.len() && looks_unmounted(&root.path) {
            Some("not_mounted")
        } else if missing * 2 > sample.len() {
            Some("mostly_missing")
        } else {
            None
        };
        if let Some(s) = state {
            notes.push(Note { name: name.clone(), label: label(&name), path: root.path.display().to_string(), state: s.into(), sampled: sample.len(), missing });
        }
    }
    Ok(notes)
}

// ---- in the server ----

#[derive(Default)]
struct Detected {
    roots: Vec<Root>,
    at_ms: i64,
    notes: Vec<Note>,
}

#[derive(Default)]
struct State {
    /// idle | scanning | applying | failed
    job: &'static str,
    phase: String,
    done: u64,
    total: u64,
    error: Option<String>,
    scan: Option<Arc<Scan>>,
    applied: Option<Applied>,
    detected: Option<Detected>,
    /// A start-up check is running (one at a time: a folder that hangs must not pile them up).
    detecting: bool,
    /// When the last one was started (they are at most a minute apart).
    detect_started: Option<Instant>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| State { job: "idle", ..Default::default() });
    f(s)
}

fn engine_roots(engine: &crate::engine::Engine) -> Vec<Root> {
    roots(&engine.config.lock().unwrap_or_else(|e| e.into_inner()), &engine.data_dir)
}

fn engine_rolls(engine: &crate::engine::Engine) -> impl Fn(&str) -> (i64, i64) + Sync {
    let cfg = engine.config.lock().unwrap_or_else(|e| e.into_inner()).clone();
    move |camera: &str| {
        let m = cfg.motion_for(camera);
        (m.pre_roll as i64, m.post_roll as i64)
    }
}

fn set_progress(phase: &str, done: u64, total: u64) {
    with(|s| {
        s.phase = phase.to_string();
        s.done = done;
        s.total = total;
    });
}

/// A job's result, a panic turned into an error (the job state must not stay "running").
fn caught<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unexpected error".into());
            Err(anyhow::anyhow!("stopped: {msg}"))
        }
    }
}

/// Run the start-up check on a thread of its own (unless one is running, or
/// one started in the last minute and `force` is off) and keep what it found.
fn spawn_detect(engine: Arc<crate::engine::Engine>, force: bool) {
    let go = with(|s| {
        if s.detecting || (!force && s.detect_started.is_some_and(|t| t.elapsed() < Duration::from_secs(60))) {
            return false;
        }
        s.detecting = true;
        s.detect_started = Some(Instant::now());
        true
    });
    if !go {
        return;
    }
    let spawned = std::thread::Builder::new().name("missing-check".into()).spawn(move || {
        let roots = engine_roots(&engine);
        let r = caught(|| detect(&engine.db, &roots));
        with(|s| s.detecting = false);
        match r {
            Ok(notes) => {
                for n in &notes {
                    tracing::warn!("storage: {}", clean(&n.text()));
                }
                with(|s| s.detected = Some(Detected { roots, at_ms: chrono::Utc::now().timestamp_millis(), notes }));
            }
            Err(e) => tracing::warn!("storage: could not check for missing recordings: {e:#}"),
        }
    });
    if spawned.is_err() {
        with(|s| s.detecting = false);
    }
}

/// At start, a little later (the cameras first): put right events a removal
/// left marked part way, then the check.
pub fn start(engine: Arc<crate::engine::Engine>) {
    let _ = std::thread::Builder::new().name("missing-start".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(20));
        if marked_path(&engine.data_dir).exists() {
            let rolls = engine_rolls(&engine);
            match lock(&engine.data_dir).and_then(|_l| caught(|| recover_marked(&engine.db, &engine.data_dir, &rolls))) {
                Ok((kept, restored)) => tracing::info!("storage: a removal of missing recordings had stopped part way; {restored} events play again, {kept} stay marked No clip"),
                Err(e) => tracing::error!("storage: events left marked by an earlier removal: {e:#}"),
            }
        }
        spawn_detect(engine, true);
    });
}

/// `GET /api/storage/missing`: the start-up note, the dry run (if any) and
/// what is running. When the folders changed since the note it is made again
/// in the background (the next call has it).
pub fn status_json(engine: &Arc<crate::engine::Engine>) -> serde_json::Value {
    let roots = engine_roots(engine);
    if with(|s| s.detected.as_ref().is_some_and(|d| d.roots != roots)) {
        spawn_detect(engine.clone(), false);
    }
    with(|s| {
        let scan = s.scan.as_ref().map(|sc| {
            let mut v = serde_json::to_value(&sc.report).unwrap_or_default();
            v["stale"] = serde_json::json!(stale(sc, &roots));
            v
        });
        serde_json::json!({
            "detected": s.detected.as_ref().map(|d| serde_json::json!({ "at_ms": d.at_ms, "notes": d.notes.iter().map(|n| { let mut v = serde_json::to_value(n).unwrap_or_default(); v["text"] = n.text().into(); v }).collect::<Vec<_>>() })),
            "job": { "state": s.job, "phase": s.phase, "done": s.done, "total": s.total, "error": s.error },
            "scan": scan,
            "applied": s.applied,
        })
    })
}

/// Mark a job running; Err when one is (here, or in another process: the lock).
fn begin(engine: &crate::engine::Engine, job: &'static str) -> std::result::Result<JobLock, String> {
    with(|s| {
        if matches!(s.job, "scanning" | "applying") {
            return Err("a check or removal is already running".to_string());
        }
        Ok(())
    })?;
    let l = lock(&engine.data_dir).map_err(|e| format!("{e:#}"))?;
    with(|s| {
        s.job = job;
        s.error = None;
        s.phase.clear();
        s.done = 0;
        s.total = 0;
    });
    Ok(l)
}

/// Start a dry run in the background. Err: something is running already.
pub fn start_scan(engine: Arc<crate::engine::Engine>) -> std::result::Result<(), String> {
    let l = begin(&engine, "scanning")?;
    let spawned = std::thread::Builder::new().name("missing-scan".into()).spawn(move || {
        let _l = l;
        let roots = engine_roots(&engine);
        let rolls = engine_rolls(&engine);
        let r = caught(|| {
            recover_marked(&engine.db, &engine.data_dir, &rolls)?;
            scan(&engine.db, &engine.data_dir, &roots, &rolls, &set_progress)
        });
        with(|s| {
            s.job = "idle";
            match r {
                Ok(sc) => {
                    tracing::info!("storage: missing recordings check: {} of {} recordings missing ({:.1} GB) in {} ms", sc.report.missing, sc.report.rows, sc.report.missing_bytes as f64 / 1e9, sc.report.took_ms);
                    s.scan = Some(Arc::new(sc));
                }
                Err(e) => {
                    s.job = "failed";
                    s.error = Some(format!("{e:#}"));
                }
            }
        });
    });
    if spawned.is_err() {
        with(|s| s.job = "idle");
        return Err("could not start the check".into());
    }
    Ok(())
}

/// Apply the dry run `scan_id` in the background, after a backup of the
/// database. `who`: the account and address asking (for the log).
/// Err: (HTTP status, reason).
pub fn start_apply(engine: Arc<crate::engine::Engine>, scan_id: &str, include: Vec<String>, who: String) -> std::result::Result<(), (u16, String)> {
    let roots = engine_roots(&engine);
    let scan = with(|s| {
        let Some(sc) = s.scan.clone() else { return Err((409, "run the check first".to_string())) };
        if sc.report.id != scan_id {
            return Err((409, "that check was replaced by a newer one; look at it first".to_string()));
        }
        if let Some(why) = stale(&sc, &roots) {
            return Err((409, why.to_string()));
        }
        Ok(sc)
    })?;
    let include: Vec<String> = include.iter().map(|n| folder_name_from(n, &roots)).collect();
    let l = begin(&engine, "applying").map_err(|e| (409, e))?;
    let spawned = std::thread::Builder::new().name("missing-apply".into()).spawn(move || {
        let _l = l;
        tracing::info!(
            "storage: {} asked to remove missing recordings (check {}, folders named: {})",
            clean(&who),
            scan.report.id,
            if include.is_empty() { "none".to_string() } else { include.iter().map(|n| clean(&label(n))).collect::<Vec<_>>().join(", ") }
        );
        let r = caught(|| -> Result<Applied> {
            if to_remove(&scan, &include) == 0 {
                return Ok(Applied { scan_id: scan.report.id.clone(), ..Default::default() });
            }
            set_progress("backup", 0, 0);
            let keep = engine.config.lock().unwrap_or_else(|e| e.into_inner()).storage.backup_keep.max(2);
            let dir = crate::backup::backup_now(Path::new(&engine.config_path), &engine.db, &engine.data_dir, keep).context("backup before removing")?;
            tracing::info!("storage: database backed up to {} before removing missing recordings", dir.display());
            let rolls = engine_rolls(&engine);
            let mut a = apply(&engine.db, &engine.data_dir, &scan, &include, &rolls, SERVER_BATCH, Duration::from_millis(10), &set_progress)?;
            a.backup = Some(dir.display().to_string());
            Ok(a)
        });
        crate::engine::playback::recordings_moved();
        let ok = r.is_ok();
        with(|s| {
            s.job = "idle";
            match r {
                Ok(a) => {
                    tracing::info!("storage: removed {} missing recordings ({:.1} GB) from the database for {}; {} events marked as having no video", a.removed, a.removed_bytes as f64 / 1e9, clean(&who), a.events_marked);
                    s.applied = Some(a);
                    s.scan = None;
                }
                Err(e) => {
                    tracing::warn!("storage: removing missing recordings failed: {e:#}");
                    s.job = "failed";
                    s.error = Some(format!("{e:#}"));
                }
            }
        });
        if ok {
            spawn_detect(engine, true);
        }
    });
    if spawned.is_err() {
        with(|s| s.job = "idle");
        return Err((500, "could not start".into()));
    }
    Ok(())
}

// ---- command line ----

fn gb(b: i64) -> String {
    format!("{:.2} GB", b as f64 / 1e9)
}

fn day(ms: Option<i64>) -> String {
    ms.and_then(chrono::DateTime::from_timestamp_millis).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string()).unwrap_or_else(|| "-".into())
}

/// Why a folder needs naming, in words.
pub fn why_text(f: &FolderReport, w: &str) -> String {
    match w {
        "not_found" => "the folder is not there".into(),
        "not_configured" => "no folder of this name is in the settings any more".into(),
        "all_missing" => "the folder is there but none of its recordings are".into(),
        "most_missing" => format!("most of its recordings are missing ({} of {})", f.missing, f.rows),
        "cameras_all_missing" => format!("every recording of {} in it is missing", f.whole_cameras.join(", ")),
        "not_mounted" => "is the drive mounted? It looks like an empty mount point; if this folder is meant to be on the system disk, name it to remove the missing ones".into(),
        o => o.into(),
    }
}

/// The dry run as text.
pub fn report_text(r: &Report) -> String {
    let mut s = format!("{} recordings indexed; {} missing on disk ({}). Checked in {:.1} s.\n", r.rows, r.missing, gb(r.missing_bytes), r.took_ms as f64 / 1000.0);
    for f in &r.folders {
        let what = match f.state.as_str() {
            "ok" => "all there".to_string(),
            "some_missing" => format!("{} of {} missing", f.missing, f.rows),
            "all_missing" => format!("THE FOLDER IS THERE BUT NONE OF ITS {} RECORDINGS ARE", f.rows),
            "not_found" if f.rows == 0 => "FOLDER NOT FOUND".to_string(),
            "not_found" => format!("FOLDER NOT FOUND ({} recordings indexed in it)", f.rows),
            "not_configured" => format!("NOT IN THE SETTINGS (looked for in the data folder): {} of {} missing", f.missing, f.rows),
            o => o.to_string(),
        };
        s += &format!("\n  {} [{}]: {}\n", clean(&f.label), clean(&f.path), what);
        if f.not_mounted && f.missing > 0 {
            s += "    IS THE DRIVE MOUNTED? This folder looks like the mount point of a drive that is not mounted.\n    Mount it and restart AmberSight. If this folder is meant to be on the system disk, name it below to remove the missing ones.\n";
        }
        if f.missing > 0 {
            s += &format!("    missing: {} recordings, {}, from {} to {}\n", f.missing, gb(f.missing_bytes), day(f.oldest_missing_ms), day(f.newest_missing_ms));
            for c in f.cameras.iter().take(12) {
                s += &format!("      {}: {} of {} ({})\n", clean(&c.camera), c.missing, c.rows, gb(c.missing_bytes));
            }
            if f.cameras.len() > 12 {
                s += &format!("      ... and {} more cameras\n", f.cameras.len() - 12);
            }
        }
        if f.unknown > 0 {
            s += &format!("    {} could not be looked for (kept)\n", f.unknown);
        }
        if f.case_differs > 0 {
            s += &format!("    {} are there under a name with other capitals (kept; was a folder renamed?)\n", f.case_differs);
        }
        if f.absolute_paths > 0 {
            s += &format!("    {} stored with a full path rather than one inside the folder\n", f.absolute_paths);
        }
        if f.needs_confirm {
            let why: Vec<String> = f.confirm_why.iter().map(|w| why_text(f, w)).collect();
            s += &format!(
                "    Left alone unless named ({}). If the path is wrong or a drive is not connected, fix that first (Settings > Recording > Drives & folders).\n    To remove them anyway: --include-folder {}\n",
                why.join("; "),
                if f.name.is_empty() { "data".to_string() } else { clean(&f.name) }
            );
        }
    }
    if r.events_no_video_all + r.protected_no_video + r.events_part_video > 0 {
        let all = if r.events_no_video_all != r.events_no_video { format!(" ({} with the folders that need naming too)", r.events_no_video_all) } else { String::new() };
        s += &format!(
            "\nEvents: {}{all} would have no video to play (none left, or older than what would be their camera's oldest recording); they stay in the list, marked \"No clip\". {} protected ones would have none either (left unchanged); {} keep part of theirs.\n",
            r.events_no_video, r.protected_no_video, r.events_part_video
        );
    }
    s
}

/// Run as root on a data folder that belongs to another account (the
/// service's): the files made (backup, database journal) would be root's.
#[cfg(unix)]
fn refuse_root(data_dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        return Ok(());
    }
    if let Ok(m) = std::fs::metadata(data_dir) {
        if m.uid() != 0 {
            anyhow::bail!(
                "run this as the account AmberSight runs as (the owner of {}, user id {}), not as root, or the files it makes would be root's: for the packages, sudo -u ambersight ambersight storage prune-missing --config /etc/ambersight/config.toml; in Docker, docker exec -u <PUID>:<PGID> <container> ambersight storage prune-missing --config /config/config.toml",
                data_dir.display(),
                m.uid()
            );
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_root(_data_dir: &Path) -> Result<()> {
    Ok(())
}

/// `ambersight storage prune-missing`.
pub fn cli(config: &str, do_apply: bool, include: &[String]) -> Result<()> {
    let cfg = Config::load(config)?;
    let data_dir = cfg.storage.data_dir.clone();
    refuse_root(&data_dir)?;
    let _lock = lock(&data_dir)?;
    let db = Db::open(&data_dir.join("ambersight.db"))?;
    let roots = roots(&cfg, &data_dir);
    let include: Vec<String> = include.iter().map(|n| folder_name_from(n, &roots)).collect();
    for n in &include {
        if !n.is_empty() && !roots.iter().any(|r| &r.name == n) && db.segment_sample(n, 1)?.is_empty() {
            anyhow::bail!("no folder named '{}' (and no recordings indexed under that name)", clean(n));
        }
    }
    let backup_keep = cfg.storage.backup_keep.max(2);
    let rolls = move |camera: &str| {
        let m = cfg.motion_for(camera);
        (m.pre_roll as i64, m.post_roll as i64)
    };
    let (kept, restored) = recover_marked(&db, &data_dir, &rolls)?;
    if kept + restored > 0 {
        println!("An earlier removal had stopped part way: {restored} events play again, {kept} stay marked \"No clip\".");
    }
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let progress = move |phase: &str, done: u64, total: u64| {
        if tty && (done % 100_000 < BATCH as u64 || done == total) {
            eprint!("\r  {phase}: {done} of {total}      ");
        }
    };
    let sc = scan(&db, &data_dir, &roots, &rolls, &progress)?;
    if tty {
        eprintln!();
    }
    print!("{}", report_text(&sc.report));
    let n = to_remove(&sc, &include);
    if !do_apply {
        if n > 0 {
            println!("\nNothing was changed (dry run). To remove the {n} missing recordings listed (folders that need naming only with --include-folder) from the database, run this again with --apply.");
        }
        return Ok(());
    }
    if n == 0 {
        println!("\nNothing to remove.");
        return Ok(());
    }
    let dir = crate::backup::backup_now(Path::new(config), &db, &data_dir, backup_keep)?;
    println!("\nDatabase backed up to {}", dir.display());
    // Pauses between pieces: the server may be running and recording.
    let a = apply(&db, &data_dir, &sc, &include, &rolls, SERVER_BATCH, Duration::from_millis(20), &progress)?;
    if tty {
        eprintln!();
    }
    println!("Removed {} recordings ({}) from the database in {:.1} s; no files were deleted.", a.removed, gb(a.removed_bytes), a.took_ms as f64 / 1000.0);
    if a.found_again > 0 {
        println!("{} were found on disk when looked for again and kept.", a.found_again);
    }
    if a.moved > 0 {
        println!("{} had moved to another folder meanwhile and were kept.", a.moved);
    }
    if a.case_differs > 0 {
        println!("{} are there under a name with other capitals and were kept.", a.case_differs);
    }
    if !a.skipped_folders.is_empty() {
        println!("Left alone: {} ({} recordings).", a.skipped_folders.iter().map(|s| clean(s)).collect::<Vec<_>>().join(", "), a.skipped_rows);
    }
    if a.events_marked > 0 {
        println!("{} events have no video to play: kept, marked \"No clip\".", a.events_marked);
    }
    println!(
        "To undo: stop AmberSight and copy {} back over the database in the data folder (events recorded since are lost then). The backup is one of the {} kept: newer backups replace it in time.",
        dir.join("ambersight.db").display(),
        backup_keep
    );
    println!("A running server shows the change at once; playback it had already prepared catches up within a minute.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SegmentRow;
    use crate::engine::Event;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ambs-missing-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seg(db: &Db, cam: &str, folder: &str, path: &str, start_s: i64) {
        let row = SegmentRow { path: path.into(), start_ms: start_s * 1000, end_ms: (start_s + 60) * 1000, bytes: 1000, video_frames: 10, kind: "continuous".into(), folder: folder.into() };
        db.insert_segment(cam, &row).unwrap();
    }

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    fn event(db: &Db, id: &str, start: i64, end: i64, protect: bool) {
        db.insert_event(&Event { id: id.into(), camera_id: 0, camera: "Front".into(), start, end, score: 0.5, clip: Some(format!("/api/clips/{id}")), thumb: Some(id.into()), objects: vec![], protect }).unwrap();
    }

    fn no_roll(_: &str) -> (i64, i64) {
        (0, 0)
    }
    fn quiet(_: &str, _: u64, _: u64) {}
    const NO_ROLL: Rolls<'static> = &no_roll;
    const QUIET: Progress<'static> = &quiet;

    fn run(db: &Db, data: &Path, sc: &Scan, include: &[&str]) -> Applied {
        let inc: Vec<String> = include.iter().map(|s| s.to_string()).collect();
        apply(db, data, sc, &inc, NO_ROLL, 3, Duration::ZERO, QUIET).unwrap()
    }

    #[test]
    fn stored_paths_that_are_absolute() {
        for p in ["F:/AmberSight/x.mp4", "d:\\a\\b.mp4", "/mnt/f/x.mp4", "\\\\nas\\share\\x.mp4"] {
            assert!(looks_absolute(p), "{p}");
        }
        for p in ["cameras/front/segments/a.mp4", "a.mp4", "f/x.mp4", ""] {
            assert!(!looks_absolute(p), "{p}");
        }
        assert_eq!(clean("a\nb\x1b[31mc"), "a?b?[31mc");
    }

    #[test]
    fn folder_names_from_the_command_line() {
        let r = |names: &[&str]| names.iter().map(|n| Root { name: n.to_string(), path: PathBuf::from("/x"), configured: true }).collect::<Vec<_>>();
        assert_eq!(folder_name_from("data", &r(&["", "stored"])), "");
        assert_eq!(folder_name_from(" Data ", &r(&["", "stored"])), "");
        assert_eq!(folder_name_from("", &r(&["", "stored"])), "");
        assert_eq!(folder_name_from("stored", &r(&["", "stored"])), "stored");
        // A folder really called "data" wins.
        assert_eq!(folder_name_from("data", &r(&["", "data"])), "data");
        assert_eq!(label(""), "data folder");
        assert_eq!(label("new"), "new");
    }

    #[test]
    fn files_are_found_by_listing_and_looked_up_when_not_listed() {
        let d = tmp("files");
        touch(&d, "cam/a.mp4");
        let mut f = Files::new(10);
        assert_eq!(f.check(&d.join("cam/a.mp4"), 1), Seen::Here);
        assert_eq!(f.check(&d.join("cam/b.mp4"), 1), Seen::Gone);
        // Written after the listing (a newer row): looked up on its own.
        touch(&d, "cam/c.mp4");
        assert_eq!(f.check(&d.join("cam/c.mp4"), 11), Seen::Here);
        // An older row is not looked up again: its file was there before the listing or never.
        assert_eq!(f.check(&d.join("cam/c.mp4"), 2), Seen::Gone);
        assert_eq!(f.check(&d.join("nodir/a.mp4"), 1), Seen::Gone);
        // Other capitals: kept (on Windows the same file, elsewhere reported).
        touch(&d, "Cam2/x.mp4");
        touch(&d, "cam3/Y.mp4");
        if cfg!(windows) {
            assert_eq!(f.check(&d.join("cam2/x.mp4"), 1), Seen::Here);
            assert_eq!(f.check(&d.join("cam3/y.mp4"), 1), Seen::Here);
        } else if cfg!(target_os = "macos") {
            // Usually a volume that ignores capitals (the file is there), unless formatted case-sensitive.
            for p in ["cam2/x.mp4", "cam3/y.mp4"] {
                let s = f.check(&d.join(p), 1);
                assert!(s == Seen::Here || s == Seen::Case, "{p}: {s:?}");
            }
        } else {
            assert_eq!(f.check(&d.join("cam2/x.mp4"), 1), Seen::Case);
            assert_eq!(f.check(&d.join("cam3/y.mp4"), 1), Seen::Case);
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn folder_states_and_why_they_need_naming() {
        let f = |configured, exists, rows, missing| folder_state(&FolderReport { configured, exists, rows, missing, ..Default::default() });
        assert_eq!(f(true, true, 10, 0), "ok");
        assert_eq!(f(true, true, 10, 3), "some_missing");
        assert_eq!(f(true, true, 10, 10), "all_missing");
        assert_eq!(f(true, false, 10, 10), "not_found");
        assert_eq!(f(true, false, 0, 0), "not_found");
        assert_eq!(f(false, true, 10, 4), "not_configured");
        assert_eq!(f(false, true, 10, 0), "ok", "found in the data folder: fine");
        let why = |rows, missing, whole: &[&str], not_mounted| {
            let mut r = FolderReport { configured: true, exists: true, rows, missing, not_mounted, whole_cameras: whole.iter().map(|s| s.to_string()).collect(), ..Default::default() };
            r.state = folder_state(&r).into();
            confirm_why(&r)
        };
        assert!(why(10, 3, &[], false).is_empty());
        assert_eq!(why(10, 6, &[], false), vec!["most_missing"]);
        assert_eq!(why(10, 5, &[], false), Vec::<String>::new(), "half is not most");
        assert_eq!(why(10, 3, &["porch"], false), vec!["cameras_all_missing"]);
        assert_eq!(why(10, 3, &[], true), vec!["not_mounted"]);
        assert_eq!(why(10, 0, &[], true), Vec::<String>::new(), "nothing missing: nothing to name");
        assert_eq!(why(10, 10, &[], false), vec!["all_missing"]);
    }

    #[test]
    fn a_drive_set_up_in_fstab_but_not_mounted() {
        let fstab = "# /etc/fstab\n\nUUID=1 / ext4 defaults 0 1\nUUID=2 /mnt/f ntfs3 defaults,nofail 0 0\nUUID=3 none swap sw 0 0\n/dev/sdb1 /mnt/my\\040disk exfat noauto 0 0\n  # UUID=4 /mnt/old ext4 defaults 0 0\n";
        let fs = fstab_mount_points(fstab);
        assert_eq!(fs, vec![PathBuf::from("/"), PathBuf::from("/mnt/f"), PathBuf::from("/mnt/my disk")]);
        let info = "22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n90 22 0:50 / /mnt/my\\040disk rw - exfat /dev/sdb1 rw\n";
        let m = mounted_points(info);
        assert!(m.contains(Path::new("/")) && m.contains(Path::new("/mnt/my disk")));
        // /mnt/f is in fstab and not mounted: its folders are on the empty mount point.
        assert_eq!(unmounted_by_fstab(Path::new("/mnt/f/AmberSight/Stored"), &fs, &m), Some(true));
        assert_eq!(unmounted_by_fstab(Path::new("/mnt/f"), &fs, &m), Some(true));
        // Mounted (noauto, mounted by hand): fine.
        assert_eq!(unmounted_by_fstab(Path::new("/mnt/my disk/AmberSight"), &fs, &m), Some(false));
        // Not covered by fstab ("/" is left out): the fallback decides.
        assert_eq!(unmounted_by_fstab(Path::new("/var/lib/ambersight"), &fs, &m), None);
        assert_eq!(unmounted_by_fstab(Path::new("/mnt/fx/AmberSight"), &fs, &m), None);
        assert_eq!(unescape_octal("a\\040b\\134c\\0"), "a b\\c\\0");
    }

    #[test]
    fn an_ordinary_folder_does_not_look_unmounted() {
        let d = tmp("mount");
        assert!(!looks_unmounted(&d));
        assert!(!looks_unmounted(&d.join("not-there")));
        let _ = std::fs::remove_dir_all(d);
    }

    /// A data folder with some recordings gone, a folder "stored" that is not
    /// there, and events around them.
    fn site(name: &str) -> (PathBuf, Db, Vec<Root>) {
        let d = tmp(name);
        let data = d.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let db = Db::open(&data.join("ambersight.db")).unwrap();
        // Data folder: 0..10 minutes, the first 4 gone.
        for i in 0..10 {
            let rel = format!("cameras/front/segments/front-seg{i:06}.mp4");
            seg(&db, "front", "", &rel, i * 60);
            if i >= 4 {
                touch(&data, &rel);
            }
        }
        // "stored" (not there): 4 older recordings.
        for i in 0..4 {
            seg(&db, "front", "stored", &format!("cameras/front/segments/old-seg{i:06}.mp4"), -10_000 + i * 60);
        }
        let roots = vec![
            Root { name: String::new(), path: data.clone(), configured: true },
            Root { name: "stored".into(), path: d.join("gone-drive").join("Stored"), configured: true },
        ];
        (d, db, roots)
    }

    #[test]
    fn dry_run_counts_and_changes_nothing() {
        let (d, db, roots) = site("dry");
        let data = roots[0].path.clone();
        event(&db, "all-gone", 30, 90, false);
        event(&db, "kept-prot", 130, 170, true);
        event(&db, "part", 200, 300, false);
        event(&db, "present", 430, 500, false);
        let sc = scan(&db, &data, &roots, NO_ROLL, QUIET).unwrap();
        let r = &sc.report;
        assert_eq!((r.rows, r.missing, r.missing_bytes), (14, 8, 8_000));
        let fd = &r.folders[0];
        assert_eq!((fd.name.as_str(), fd.state.as_str(), fd.rows, fd.missing, fd.needs_confirm), ("", "some_missing", 10, 4, false));
        assert_eq!((fd.cameras.len(), fd.cameras[0].missing, fd.cameras[0].rows), (1, 4, 10));
        let fs = &r.folders[1];
        assert_eq!((fs.name.as_str(), fs.state.as_str(), fs.exists, fs.missing, fs.needs_confirm), ("stored", "not_found", false, 4, true));
        assert_eq!((r.events_no_video, r.protected_no_video, r.events_part_video, r.events_no_video_all), (1, 1, 1, 1));
        assert!(report_text(r).contains("FOLDER NOT FOUND"), "{}", report_text(r));
        assert_eq!(to_remove(&sc, &[]), 4);
        assert_eq!(to_remove(&sc, &["stored".into()]), 8);
        // Nothing changed.
        assert_eq!(db.segment_count_estimate().unwrap(), 14);
        assert!(db.get_event("all-gone").unwrap().unwrap().clip.is_some());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn apply_removes_rows_keeps_files_and_events_and_totals_follow() {
        let (d, db, roots) = site("apply");
        let data = roots[0].path.clone();
        event(&db, "all-gone", 30, 90, false);
        event(&db, "kept-prot", 130, 170, true);
        event(&db, "part", 200, 300, false);
        event(&db, "saved", 60, 100, false);
        touch(&data, "clips/saved.mp4");
        assert_eq!(db.folder_bytes("").unwrap(), 10_000);
        let sc = scan(&db, &data, &roots, NO_ROLL, QUIET).unwrap();
        // A file that comes back before applying is kept.
        touch(&data, "cameras/front/segments/front-seg000003.mp4");
        // The folder that is not there is left alone unless named.
        let a = run(&db, &data, &sc, &[]);
        assert_eq!((a.removed, a.removed_bytes, a.found_again, a.moved), (3, 3000, 1, 0));
        assert_eq!((a.skipped_folders.clone(), a.skipped_rows), (vec!["stored".to_string()], 4));
        assert_eq!(db.folder_bytes("").unwrap(), 7000, "the running totals follow");
        assert_eq!(db.folder_bytes("stored").unwrap(), 4000);
        // Events: kept; the one with no video left is marked, the others not.
        assert_eq!(a.events_marked, 1);
        assert!(db.get_event("all-gone").unwrap().unwrap().clip.is_none());
        let p = db.get_event("kept-prot").unwrap().unwrap();
        assert!(p.clip.is_some() && p.protect, "protected events are never changed");
        assert!(db.get_event("part").unwrap().unwrap().clip.is_some());
        assert!(db.get_event("saved").unwrap().unwrap().clip.is_some(), "plays from its saved clip");
        assert!(!marked_path(&data).exists(), "nothing left to put right");
        // Files present were not touched.
        for i in 3..10 {
            assert!(data.join(format!("cameras/front/segments/front-seg{i:06}.mp4")).exists());
        }
        // Named: its rows go too.
        let sc = scan(&db, &data, &roots, NO_ROLL, QUIET).unwrap();
        assert_eq!(sc.report.missing, 4);
        let a = run(&db, &data, &sc, &["stored"]);
        assert_eq!(a.removed, 4);
        assert_eq!(db.folder_bytes("stored").unwrap(), 0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn an_event_the_removal_would_leave_before_the_oldest_recording_is_kept_too() {
        let (d, db, roots) = site("older");
        let data = roots[0].path.clone();
        // No recordings in its own time (between "stored" and the data folder), and the
        // removal makes the camera's oldest recording newer than it: retention would take it.
        event(&db, "between", -5000, -4990, false);
        // Already older than everything: retention's as before, not marked.
        event(&db, "ancient", -50_000, -49_990, false);
        let sc = scan(&db, &data, &roots, NO_ROLL, QUIET).unwrap();
        assert_eq!((sc.report.events_no_video, sc.report.events_no_video_all), (0, 1), "only once stored is included");
        let a = run(&db, &data, &sc, &["stored"]);
        assert_eq!(a.events_marked, 1);
        assert!(db.get_event("between").unwrap().unwrap().clip.is_none());
        assert!(db.get_event("ancient").unwrap().unwrap().clip.is_some());
        // Retention's rule no longer sees it.
        let left: Vec<String> = db.oldest_unprotected_events_with_clip(10).unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(left, vec!["ancient".to_string()]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_recording_moved_to_another_folder_meanwhile_is_not_removed() {
        let (d, db, roots) = site("moved");
        let data = roots[0].path.clone();
        event(&db, "in-moved", 10, 20, false);
        let sc = scan(&db, &data, &roots, NO_ROLL, QUIET).unwrap();
        assert_eq!(sc.report.events_no_video, 1);
        // Retention moved it to the archive after the check.
        assert!(db.set_segment_folder("cameras/front/segments/front-seg000000.mp4", "archive").unwrap());
        let a = run(&db, &data, &sc, &[]);
        assert_eq!((a.removed, a.moved, a.found_again, a.events_marked), (3, 1, 0, 0));
        assert_eq!(db.folder_bytes("archive").unwrap(), 1000);
        assert!(db.get_event("in-moved").unwrap().unwrap().clip.is_some(), "its video is in the archive");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn events_marked_by_a_removal_that_stopped_are_put_right() {
        let (d, db, roots) = site("recover");
        let data = roots[0].path.clone();
        event(&db, "gone", 30, 90, false);
        event(&db, "has-video", 300, 320, false);
        event(&db, "protected-later", 30, 40, false);
        // As left by a removal stopped after marking, before removing.
        write_marked(&data, &["gone".into(), "has-video".into(), "protected-later".into(), "no-such".into()]).unwrap();
        for id in ["gone", "has-video", "protected-later"] {
            assert!(db.clear_event_clip(id).unwrap());
        }
        db.set_protect("protected-later", true).unwrap();
        // "gone" still has its (missing) rows: the removal did not get to them, so it plays (tries to) again.
        let (kept, restored) = recover_marked(&db, &data, NO_ROLL).unwrap();
        assert_eq!((kept, restored), (0, 3));
        assert!(!marked_path(&data).exists());
        assert_eq!(recover_marked(&db, &data, NO_ROLL).unwrap(), (0, 0), "nothing to do");
        // Rows gone: stays marked.
        let sc = scan(&db, &data, &roots, NO_ROLL, QUIET).unwrap();
        let _ = run(&db, &data, &sc, &[]);
        assert!(db.get_event("gone").unwrap().unwrap().clip.is_none());
        write_marked(&data, &["gone".into()]).unwrap();
        assert_eq!(recover_marked(&db, &data, NO_ROLL).unwrap(), (1, 0));
        assert!(db.get_event("protected-later").unwrap().unwrap().clip.is_some(), "protected: never left marked");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn an_unreadable_record_of_marked_events_is_kept_and_stops_removals() {
        let (d, db, roots) = site("badmarked");
        let data = roots[0].path.clone();
        let sc = scan(&db, &data, &roots, NO_ROLL, QUIET).unwrap();
        std::fs::write(marked_path(&data), b"{not json").unwrap();
        assert!(recover_marked(&db, &data, NO_ROLL).is_err());
        let inc: Vec<String> = Vec::new();
        assert!(apply(&db, &data, &sc, &inc, NO_ROLL, 3, Duration::ZERO, QUIET).is_err());
        assert!(marked_path(&data).exists(), "kept for someone to look at");
        assert_eq!(db.segment_count_estimate().unwrap(), 14, "nothing removed");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_row_is_removed_only_with_the_same_folder_start_and_path() {
        let (d, db, _roots) = site("rowguard");
        let rows = db.segment_refs_after(0, 2).unwrap();
        let (a, b) = (&rows[0], &rows[1]);
        let h = |p: &str| crate::db::path_hash(p);
        // Another start, another path, another folder: kept.
        assert_eq!(db.delete_segments_in(&[(a.id, "", a.start_ms + 1, h(&a.path))]).unwrap().0, 0);
        assert_eq!(db.delete_segments_in(&[(a.id, "", a.start_ms, h("other.mp4"))]).unwrap().0, 0);
        assert_eq!(db.delete_segments_in(&[(a.id, "stored", a.start_ms, h(&a.path))]).unwrap().0, 0);
        assert_eq!(db.delete_segments_in(&[(a.id, "", a.start_ms, h(&a.path)), (b.id, "", b.start_ms, h(&b.path))]).unwrap(), (2, 2000));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn retention_deletes_only_events_still_unmarked() {
        let db = Db::open_in_memory().unwrap();
        event(&db, "a", 10, 20, false);
        event(&db, "b", 30, 40, false);
        event(&db, "p", 50, 60, true);
        db.clear_event_clip("b").unwrap();
        let ids: Vec<String> = ["a", "b", "p"].iter().map(|s| s.to_string()).collect();
        assert_eq!(db.delete_events_with_clip(&ids).unwrap(), vec!["a".to_string()]);
        assert!(db.get_event("b").unwrap().is_some() && db.get_event("p").unwrap().is_some());
    }

    #[test]
    fn one_check_or_removal_at_a_time() {
        let d = tmp("lock");
        let l = lock(&d).unwrap();
        assert!(lock(&d).is_err());
        drop(l);
        assert!(lock(&d).is_ok());
        let _ = std::fs::remove_dir_all(d);
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_is_released_while_a_starting_child_still_holds_a_copy() {
        // A child forked by another thread holds a copy of the descriptor until it runs its
        // program; a duplicate descriptor stands for it (same open file).
        use std::os::fd::AsRawFd;
        let d = tmp("lockcopy");
        let l = lock(&d).unwrap();
        // SAFETY: dup of a descriptor that is open; closed below.
        let copy = unsafe { libc::dup(l.0.as_raw_fd()) };
        assert!(copy >= 0);
        drop(l);
        let again = lock(&d);
        // SAFETY: the copy made above, closed once.
        unsafe { libc::close(copy) };
        assert!(again.is_ok(), "the lock lingered while a copy of the descriptor was open");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_folder_that_holds_none_of_its_recordings_needs_naming() {
        let d = tmp("allgone");
        let data = d.join("data");
        let stored = d.join("Stored");
        std::fs::create_dir_all(&stored).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let db = Db::open(&data.join("ambersight.db")).unwrap();
        for i in 0..3 {
            seg(&db, "front", "stored", &format!("cameras/front/segments/s{i}.mp4"), i * 60);
        }
        let roots = vec![Root { name: String::new(), path: data.clone(), configured: true }, Root { name: "stored".into(), path: stored, configured: true }];
        let sc = scan(&db, &data, &roots, NO_ROLL, QUIET).unwrap();
        let f = sc.report.folders.iter().find(|f| f.name == "stored").unwrap();
        assert_eq!((f.state.as_str(), f.needs_confirm), ("all_missing", true));
        assert_eq!(to_remove(&sc, &[]), 0, "no backup, nothing to do");
        assert_eq!(run(&db, &data, &sc, &[]).removed, 0);
        assert_eq!(run(&db, &data, &sc, &["stored"]).removed, 3);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn new_recordings_on_an_unmounted_drive_s_folder_do_not_hide_the_old_ones() {
        // The drive is not mounted; the recorder made its folder again and wrote a little there:
        // most of the folder, and one camera entirely, missing.
        let d = tmp("unmounted");
        let data = d.join("data");
        let stored = d.join("Stored");
        std::fs::create_dir_all(&data).unwrap();
        let db = Db::open(&data.join("ambersight.db")).unwrap();
        for i in 0..6 {
            seg(&db, "front", "stored", &format!("cameras/front/segments/f{i}.mp4"), i * 60);
        }
        for i in 0..3 {
            seg(&db, "porch", "stored", &format!("cameras/porch/segments/p{i}.mp4"), i * 60);
        }
        for i in 4..6 {
            touch(&stored, &format!("cameras/front/segments/f{i}.mp4"));
        }
        let roots = vec![Root { name: String::new(), path: data.clone(), configured: true }, Root { name: "stored".into(), path: stored, configured: true }];
        let sc = scan(&db, &data, &roots, NO_ROLL, QUIET).unwrap();
        let f = sc.report.folders.iter().find(|f| f.name == "stored").unwrap();
        assert_eq!(f.state, "some_missing");
        assert_eq!(f.confirm_why, vec!["most_missing", "cameras_all_missing"]);
        assert_eq!(f.whole_cameras, vec!["porch".to_string()]);
        assert!(report_text(&sc.report).contains("every recording of porch in it is missing"));
        assert_eq!(run(&db, &data, &sc, &[]).removed, 0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_folder_no_longer_in_the_settings_needs_naming() {
        let (d, db, roots) = site("unconf");
        let data = roots[0].path.clone();
        // "stored" removed from the settings: looked for in the data folder.
        let sc = scan(&db, &data, &roots[..1], NO_ROLL, QUIET).unwrap();
        let f = sc.report.folders.iter().find(|f| f.name == "stored").unwrap();
        assert_eq!((f.state.as_str(), f.configured, f.needs_confirm), ("not_configured", false, true));
        let a = run(&db, &data, &sc, &[]);
        assert_eq!((a.removed, a.skipped_rows), (4, 4));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_check_goes_stale_when_folders_change() {
        let (d, db, roots) = site("stale");
        let sc = scan(&db, &roots[0].path, &roots, NO_ROLL, QUIET).unwrap();
        assert!(stale(&sc, &roots).is_none());
        let mut moved = roots.clone();
        moved[1].path = PathBuf::from("/mnt/f/AmberSight/Stored");
        assert!(stale(&sc, &moved).is_some());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn start_up_check_notices_missing_and_absent_folders() {
        let (d, db, roots) = site("detect");
        let data = roots[0].path.clone();
        for i in 4..6 {
            std::fs::remove_file(data.join(format!("cameras/front/segments/front-seg{i:06}.mp4"))).unwrap();
        }
        // Data folder: 6 of 10 gone (mostly), "stored" not there.
        let notes = detect(&db, &roots).unwrap();
        let st: Vec<(&str, &str)> = notes.iter().map(|n| (n.name.as_str(), n.state.as_str())).collect();
        assert_eq!(st, vec![("", "mostly_missing"), ("stored", "not_found")]);
        assert!(notes[1].text().contains("not there"));
        // All there: nothing to say about the data folder.
        for i in 0..6 {
            touch(&data, &format!("cameras/front/segments/front-seg{i:06}.mp4"));
        }
        let notes = detect(&db, &roots).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        // "stored" taken out of the settings: its recordings are not in the data folder either.
        let notes = detect(&db, &roots[..1]).unwrap();
        let st: Vec<(&str, &str)> = notes.iter().map(|n| (n.name.as_str(), n.state.as_str())).collect();
        assert_eq!(st, vec![("stored", "not_configured")]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_unmounted_note_says_to_mount_not_to_remove() {
        let n = Note { name: "stored".into(), label: "stored".into(), path: "/mnt/f/AmberSight".into(), state: "not_mounted".into(), sampled: 24, missing: 20 };
        let t = n.text();
        assert!(t.contains("Is the drive mounted?") && t.contains("meant to be on the system disk"), "{t}");
        assert!(!t.contains("Recordings whose files are gone"), "{t}");
    }

    #[test]
    fn a_job_that_panics_is_an_error() {
        let r: Result<()> = caught(|| panic!("boom"));
        assert!(format!("{:#}", r.unwrap_err()).contains("boom"));
    }

    #[tokio::test]
    async fn only_admins_reach_it_and_not_from_other_sites() {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use base64::Engine as _;
        use tower::ServiceExt;
        let mut cfg: Config = toml::from_str("").unwrap();
        for (name, admin) in [("ann", true), ("bob", false)] {
            cfg.server.users.push(crate::config::UserConfig { disabled: false, username: name.into(), password: crate::auth::hash_password(&format!("pw-{name}-1")), admin, cameras: Vec::new(), ..Default::default() });
        }
        let e = crate::engine::Engine::for_test(cfg);
        let app = crate::web::build_router(e.clone());
        let call = |method: &str, path: &str, who: Option<&str>, origin: Option<&str>, body: &str| {
            let mut b = Request::builder().method(method).uri(path).header(header::HOST, "localhost:8090").header(header::CONTENT_TYPE, "application/json");
            if let Some(u) = who {
                b = b.header(header::AUTHORIZATION, format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:pw-{u}-1"))));
            }
            if let Some(o) = origin {
                b = b.header(header::ORIGIN, o);
            }
            let req = b.body(Body::from(body.to_string())).unwrap();
            let app = app.clone();
            async move { app.oneshot(req).await.unwrap().status() }
        };
        let apply = r#"{"scan_id":"nope","include":["stored"]}"#;
        assert_eq!(call("GET", "/api/storage/missing", None, None, "").await, StatusCode::UNAUTHORIZED);
        for (m, p, b) in [("GET", "/api/storage/missing", ""), ("POST", "/api/storage/missing/scan", ""), ("POST", "/api/storage/missing/apply", apply)] {
            assert_eq!(call(m, p, Some("bob"), None, b).await, StatusCode::FORBIDDEN, "viewer: {m} {p}");
        }
        assert_eq!(call("POST", "/api/storage/missing/apply", Some("ann"), Some("http://evil.example"), apply).await, StatusCode::FORBIDDEN, "another site");
        assert_eq!(call("POST", "/api/storage/missing/scan", Some("ann"), Some("http://evil.example"), "").await, StatusCode::FORBIDDEN, "another site");
        assert_eq!(call("GET", "/api/storage/missing", Some("ann"), None, "").await, StatusCode::OK);
        // An admin gets through; a check that is not the current one is refused.
        assert_eq!(call("POST", "/api/storage/missing/apply", Some("ann"), Some("http://localhost:8090"), apply).await, StatusCode::CONFLICT);
    }

    #[test]
    fn retention_leaves_events_marked_without_video_to_age() {
        let db = Db::open_in_memory().unwrap();
        event(&db, "a", 10, 20, false);
        event(&db, "b", 30, 40, false);
        assert!(db.clear_event_clip("a").unwrap());
        let ids: Vec<String> = db.oldest_unprotected_events_with_clip(10).unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["b".to_string()]);
        // Protected ones are never marked.
        event(&db, "p", 50, 60, true);
        assert!(!db.clear_event_clip("p").unwrap());
        // Restored only while it has no clip address.
        assert!(!db.restore_event_clip("b").unwrap());
        assert!(db.restore_event_clip("a").unwrap());
        assert_eq!(db.get_event("a").unwrap().unwrap().clip.as_deref(), Some("/api/clips/a"));
    }
}
