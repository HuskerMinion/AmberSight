//! Event copies: a camera can also save each event's clip as an .mp4 in a
//! folder of the owner's choosing (one a OneDrive, Google Drive or Dropbox
//! app syncs, a USB drive, a network share), while it records as before.
//! The same stream, no second connection: Blue Iris's "clone camera" use.
//!
//! - Files: `<folder>/<Camera>/<YYYY-MM-DD>/<HH-MM-SS> <labels>.mp4` (local
//!   time of the event's start), with the event's picture beside it as
//!   `.jpg`. Written under a temporary `.part` name nobody can guess and
//!   renamed when complete, so sync apps normally see only finished files.
//! - Queued when an event is finalized (after its post-roll) and written by
//!   a few threads of their own, one folder at a time each: a slow or dead
//!   drive or share never holds up recording, the frame loop, or the copies
//!   of cameras that save elsewhere. The queue is bounded and kept in the
//!   data folder, so a restart or an offline folder only delays copies.
//!   Copies wait at most [`MAX_AGE_SECS`]; older ones, and events whose
//!   recordings are gone, are skipped (counted in the camera's status).
//! - A folder that keeps failing (or stops answering) raises a system
//!   problem with the reason; it clears when a copy works again.
//! - The folder is the owner's: it is never created (an unplugged drive
//!   must not turn into a folder on the system drive), never counted in
//!   recording caps or clean-up, never locked down (lockdown.rs). Only
//!   `copy_events_keep_days` deletes there, and only files with AmberSight's
//!   own names in its own `<Camera>/<date>` folders. Everything inside it is
//!   done relative to folders opened once (copy_fs.rs): others who may write
//!   there cannot redirect a write or a delete with a link or junction.

use super::copy_fs::{Dir, Kind, NewFile};
use super::*;
use chrono::{Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, MutexGuard};

/// The most copies waiting at once (all cameras); beyond that the oldest
/// is skipped.
const MAX_JOBS: usize = 2000;
/// A copy that could not be made for this long is skipped.
pub const MAX_AGE_SECS: i64 = 3 * 86400;
/// Wait a moment after the event is saved (names from faces and plates may
/// still be added to it).
const SETTLE_SECS: i64 = 10;
/// Seconds before the next try after failures in a row.
const RETRY: [i64; 5] = [15, 30, 60, 120, 120];
/// Longest name part from a camera name or a label.
const NAME_MAX: usize = 60;
/// Threads making copies; each works on one folder at a time.
const WORKERS: usize = 3;
/// A copy running this long: the folder is reported as not responding.
const STUCK_SECS: i64 = 180;
/// Tries for an event whose recordings could not be read.
const SOURCE_TRIES: u32 = 3;
/// A battery camera's lead-in from its card is waited for at most this long
/// after the event.
const LEAD_IN_WAIT_SECS: i64 = 600;
const GB: u64 = 1 << 30;

/// One event to copy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    pub camera: String,
    /// The event counts by what was detected (AI confirmed an object, a
    /// sound, the camera's own detection, or AI off): what "only confirmed
    /// events" copies. Notification settings play no part.
    #[serde(default, alias = "alert")]
    pub confirmed: bool,
    pub queued: i64,
    #[serde(default)]
    pub tries: u32,
    #[serde(default)]
    pub next_try: i64,
}

/// What a camera's copying has done, for its settings and the health page.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CamStatus {
    /// Copies made since the server started.
    pub copied: u64,
    pub last_ok: i64,
    /// The last copy, relative to the copy folder.
    pub last_file: String,
    /// Something to know about the last copy (only part of it was recorded).
    pub last_note: String,
    /// Copies are failing, since then.
    pub failing_since: Option<i64>,
    /// Failed tries in a row.
    pub failures: u32,
    /// Why, in plain words (no paths: it may go out in an alert).
    pub last_error: String,
    /// What to do about it.
    pub hint: String,
    /// Events left out (waited too long, recordings gone, queue full).
    pub skipped: u64,
    pub last_skip: String,
    /// Old copies deleted by keep-days since the server started.
    pub trimmed: u64,
    /// A copy into this camera's folder has been running since then.
    pub busy_since: Option<i64>,
}

struct State {
    jobs: VecDeque<Job>,
    status: BTreeMap<String, CamStatus>,
    dirty: bool,
    /// Folders being written to now (folder key -> since).
    busy: BTreeMap<String, i64>,
}

static STATE: Mutex<State> = Mutex::new(State { jobs: VecDeque::new(), status: BTreeMap::new(), dirty: false, busy: BTreeMap::new() });
static WAKE: Condvar = Condvar::new();
static QUEUE_FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
static SAVING: Mutex<()> = Mutex::new(());

/// The state, also after a thread panicked holding it.
fn st() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The same folder for two cameras (case aside on Windows and macOS).
fn folder_key(p: &str) -> String {
    let t = p.trim().trim_end_matches(['/', '\\']);
    if cfg!(any(windows, target_os = "macos")) { t.to_lowercase() } else { t.to_string() }
}

// ------------------------------------------------------------- queueing

/// Queue the event `id` of `camera` if the camera copies its events (and,
/// with "only confirmed events", if it counts by what was detected), by its
/// settings now: changing them does not restart the camera. Never blocks on
/// the copy folder.
pub fn queue_event(engine: &Engine, camera: &str, id: &str, confirmed: bool) {
    let wanted = engine.config.lock().unwrap().cameras.iter().find(|c| c.name == camera).is_some_and(|c| c.copy_folder().is_some() && (confirmed || !c.copy_events_only_confirmed));
    if !wanted {
        return;
    }
    let now = Utc::now().timestamp();
    push(Job { id: id.to_string(), camera: camera.to_string(), confirmed, queued: now, tries: 0, next_try: now + SETTLE_SECS });
}

fn push(job: Job) {
    {
        let mut st = st();
        if st.jobs.iter().any(|j| j.id == job.id) {
            return;
        }
        while st.jobs.len() >= MAX_JOBS {
            if let Some(old) = st.jobs.pop_front() {
                warn!("event copies: too many waiting; skipped {} of {}", old.id, old.camera);
                let s = st.status.entry(old.camera.clone()).or_default();
                s.skipped += 1;
                s.last_skip = "too many copies were waiting".into();
            }
        }
        st.jobs.push_back(job);
        st.dirty = true;
        WAKE.notify_all();
    }
    // Saved here too: every copy thread may be stuck on a dead share.
    save_queue();
}

/// A renamed camera's copies still waiting follow it (into its new folder).
pub fn rename_camera(old: &str, new: &str) {
    let mut st = st();
    let mut moved = false;
    for j in st.jobs.iter_mut().filter(|j| j.camera == old) {
        j.camera = new.to_string();
        moved = true;
    }
    if let Some(s) = st.status.remove(old) {
        st.status.insert(new.to_string(), s);
    }
    st.dirty |= moved;
}

/// Copies waiting for `camera`.
fn waiting(st: &State, camera: &str) -> usize {
    st.jobs.iter().filter(|j| j.camera == camera).count()
}

fn save_queue() {
    let Some(path) = QUEUE_FILE.get() else { return };
    let _one = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    let jobs: Vec<Job> = {
        let mut st = st();
        if !st.dirty {
            return;
        }
        st.dirty = false;
        st.jobs.iter().cloned().collect()
    };
    let tmp = path.with_extension("json.tmp");
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(&jobs).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        warn!("event copies: could not save the queue: {e}");
        st().dirty = true;
    }
}

fn load_queue(path: &Path, now: i64) -> Vec<Job> {
    let Ok(b) = std::fs::read(path) else { return Vec::new() };
    let jobs: Vec<Job> = match serde_json::from_slice(&b) {
        Ok(j) => j,
        Err(e) => {
            let bad = path.with_extension("json.bad");
            warn!("event copies: the saved queue could not be read ({e}); kept as {} and started empty", bad.display());
            let _ = std::fs::rename(path, bad);
            return Vec::new();
        }
    };
    jobs.into_iter().filter(|j| now - j.queued < MAX_AGE_SECS).take(MAX_JOBS).collect()
}

// ------------------------------------------------------------- status

/// A camera's copy status for its settings (admins).
pub fn status_json(camera: &str) -> serde_json::Value {
    let st = st();
    let mut v = serde_json::to_value(st.status.get(camera).cloned().unwrap_or_default()).unwrap_or_default();
    v["waiting"] = waiting(&st, camera).into();
    v
}

/// A camera whose copies keep failing, for the health report.
pub struct Problem {
    /// Stays the same while the problem lasts (system_alerts).
    pub key: String,
    /// For the alert, which may go to outside services: no path in it.
    pub text: String,
    /// A few words for the header chip.
    pub short: String,
    /// For the admin's health page: with what to do.
    pub warning: String,
}

/// Cameras whose copies keep failing (twice in a row or for a minute), or
/// whose folder has not answered for minutes.
pub fn problems(cameras: &[CameraConfig], now: i64) -> Vec<Problem> {
    let st = st();
    let mut out = Vec::new();
    for c in cameras.iter().filter(|c| c.copy_folder().is_some()) {
        let Some(s) = st.status.get(&c.name) else { continue };
        let n = waiting(&st, &c.name);
        let more = if n > 0 { format!("; {n} waiting") } else { String::new() };
        if let Some(since) = s.busy_since.filter(|t| now - t >= STUCK_SECS) {
            out.push(Problem {
                key: format!("copy:{}", c.name),
                text: format!("{}: its copy folder is not responding", c.name),
                short: format!("{}: copy folder not responding", c.name),
                warning: format!("{}: its copy folder is not responding (a copy has been running for {} min{more}). A network share or a drive may have stopped answering; recording is not affected", c.name, (now - since) / 60),
            });
            continue;
        }
        let Some(since) = s.failing_since else { continue };
        if s.failures < 2 && now - since < 60 {
            continue;
        }
        let hint = if s.hint.is_empty() { String::new() } else { format!(". {}", s.hint) };
        out.push(Problem {
            key: format!("copy:{}", c.name),
            text: format!("{}: event clips are not being saved to its copy folder ({})", c.name, s.last_error),
            short: format!("{}: clips not copied", c.name),
            warning: format!("{}: event clips are not being saved to its copy folder: {}{more}{hint}", c.name, s.last_error),
        });
    }
    out
}

// ------------------------------------------------------------- names

/// Unicode format characters (category Cf): invisible, some reorder text.
fn is_format_char(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB
        | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// `s` as one safe file or folder name: no path separators, no `..`, no
/// control, format (invisible) or odd space characters, nothing Windows
/// refuses (`<>:"|?*`, trailing dots and spaces, CON, NUL, COM1 ...). Never empty.
pub fn safe_name(s: &str, fallback: &str) -> String {
    let bad = |c: char| {
        c.is_control() || is_format_char(c) || (c.is_whitespace() && c != ' ') || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    };
    let mapped: String = s.chars().map(|c| if bad(c) { '_' } else { c }).collect();
    let mut out: String = mapped.trim().trim_start_matches('.').chars().take(NAME_MAX).collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    let out = out.trim().to_string();
    if out.is_empty() || out.chars().all(|c| c == '_' || c == '.') {
        return fallback.to_string();
    }
    if windows_reserved(&out) {
        return format!("_{out}");
    }
    out
}

/// CON, PRN, AUX, NUL, COM0-9, LPT0-9 (also with an extension): names
/// Windows keeps for devices.
fn windows_reserved(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let b = base.as_bytes();
    b.len() == 4 && (base.starts_with("COM") || base.starts_with("LPT")) && b[3].is_ascii_digit()
        || ((base.starts_with("COM") || base.starts_with("LPT")) && matches!(&base[3..], "¹" | "²" | "³"))
}

/// The file name (without extension) for an event at `t`: `HH-MM-SS` and
/// its labels, e.g. `14-03-22 person, car`.
pub fn file_stem(t: &chrono::DateTime<Local>, labels: &[String]) -> String {
    let hms = t.format("%H-%M-%S").to_string();
    let mut words: Vec<String> = Vec::new();
    for l in labels.iter().take(6) {
        let w = safe_name(l, "");
        if !w.is_empty() && !words.contains(&w) {
            words.push(w);
        }
    }
    let mut lab = words.join(", ");
    if lab.chars().count() > NAME_MAX {
        lab = lab.chars().take(NAME_MAX).collect();
        while lab.ends_with(['.', ' ', ',']) {
            lab.pop();
        }
    }
    if lab.is_empty() { hms } else { format!("{hms} {lab}") }
}

/// A file AmberSight writes in a date folder: `HH-MM-SS[ labels][ (n)].mp4`
/// or `.jpg`, or one still being written (`<that>.<8 hex>.part`, or the
/// older `<that>.part`).
pub fn is_our_file(name: &str) -> bool {
    let mut n = name;
    if let Some(p) = n.strip_suffix(".part") {
        n = match p.rsplit_once('.') {
            Some((head, hex)) if hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => head,
            _ => p,
        };
    }
    let Some(stem) = n.strip_suffix(".mp4").or_else(|| n.strip_suffix(".jpg")) else { return false };
    let b = stem.as_bytes();
    if b.len() < 8 || !(b[2] == b'-' && b[5] == b'-') || ![0, 1, 3, 4, 6, 7].iter().all(|&i| b[i].is_ascii_digit()) {
        return false;
    }
    let rest = &stem[8..];
    (rest.is_empty() || rest.starts_with(' ')) && !rest.contains(['/', '\\'])
}

/// A date folder's name: `YYYY-MM-DD`.
fn day_of(name: &str) -> Option<NaiveDate> {
    if name.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(name, "%Y-%m-%d").ok()
}

/// Another camera that would save into the same `<Camera>` folder as `cam`
/// (same copy folder, same folder name once made safe, case aside).
pub fn name_clash<'a>(cam: &CameraConfig, others: impl Iterator<Item = &'a CameraConfig>) -> Option<String> {
    let f = folder_key(cam.copy_folder()?);
    let n = safe_name(&cam.name, "Camera").to_lowercase();
    for o in others {
        if o.name != cam.name && o.copy_folder().map(folder_key).as_deref() == Some(f.as_str()) && safe_name(&o.name, "Camera").to_lowercase() == n {
            return Some(o.name.clone());
        }
    }
    None
}

// ------------------------------------------------------------- the folder

/// The folders a copy folder must keep clear of.
#[derive(Debug, Clone, Default)]
pub struct Guards {
    pub data_dir: PathBuf,
    pub program_dir: Option<PathBuf>,
    /// Recording folders: (name, path).
    pub storage: Vec<(String, PathBuf)>,
}

pub fn guards(engine: &Engine) -> Guards {
    let storage = engine.config.lock().unwrap().storage.folder_roots();
    Guards {
        data_dir: engine.data_dir.clone(),
        program_dir: std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)),
        storage,
    }
}

/// A path cut into lower-case parts (both kinds of slash: stricter on Unix,
/// where a backslash is an ordinary character).
#[derive(Debug, PartialEq)]
struct Parts {
    /// Its drive letter (Windows).
    drive: Option<char>,
    /// A network path (`\\server\share\...`, Windows): parts[0] is the
    /// server and parts[1] the share.
    unc: bool,
    parts: Vec<String>,
}

fn parts_of(p: &str) -> Parts {
    let s = p.trim();
    let unc = cfg!(windows) && (s.starts_with(r"\\") || s.starts_with("//"));
    let b = s.as_bytes();
    let drive = (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()).then(|| (b[0] as char).to_ascii_lowercase());
    let body = if drive.is_some() { &s[2..] } else { s };
    let parts = body.split(['/', '\\']).filter(|x| !x.is_empty() && *x != ".").map(|x| x.to_lowercase()).collect();
    Parts { drive, unc, parts }
}

/// `child` is `parent` or below it (by names, case aside; a network path is
/// below a network path only on the same server and share).
fn inside(child: &str, parent: &str) -> bool {
    let (c, p) = (parts_of(child), parts_of(parent));
    !p.parts.is_empty() && c.drive == p.drive && c.unc == p.unc && c.parts.starts_with(&p.parts)
}

/// A path as the checks read it: without `\\?\` (and `\\?\UNC\` as `\\`),
/// and on macOS without the `/private` that /tmp, /var and /etc lead into.
fn plain(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = if let Some(r) = s.strip_prefix(r"\\?\UNC\") { format!(r"\\{r}") } else { s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or_else(|| s.into_owned()) };
    if cfg!(target_os = "macos") { s.strip_prefix("/private").filter(|x| x.starts_with('/')).map(str::to_string).unwrap_or(s) } else { s }
}

const WIN_SYSTEM: [&str; 10] = ["windows", "program files", "program files (x86)", "programdata", "system volume information", "$recycle.bin", "recovery", "boot", "documents and settings", "perflogs"];
const UNIX_SYSTEM: [&str; 21] = ["bin", "boot", "dev", "etc", "lib", "lib32", "lib64", "libx32", "proc", "run", "sbin", "sys", "usr", "snap", "system", "library", "applications", "private", "cores", "efi", "tmp"];
const VAR_SYSTEM: [&str; 10] = ["lib", "log", "run", "cache", "spool", "db", "empty", "lock", "mail", "tmp"];

/// Why `path` cannot be a copy folder, or None. Shared by saving the
/// setting and by every copy (a config edited by hand is checked too).
pub fn refusal(path: &str, g: &Guards) -> Option<String> {
    let p = path.trim();
    if p.is_empty() {
        return Some("choose a folder".into());
    }
    if p.chars().count() > 300 {
        return Some("the folder path is too long".into());
    }
    if p.chars().any(|c| c.is_control()) {
        return Some("the folder path has control characters in it".into());
    }
    if p.starts_with(r"\\?\") || p.starts_with(r"\\.\") || p.starts_with("//?/") || p.starts_with("//./") {
        return Some(r"write the folder as a plain path, such as D:\Camera copies".into());
    }
    if !Path::new(p).is_absolute() {
        return Some(if cfg!(windows) { r"use the folder's full path, such as D:\Camera copies or \\nas\share\cameras".into() } else { "use the folder's full path, such as /mnt/backup/cameras".into() });
    }
    if let Some(r) = place_refusal(p) {
        return Some(r);
    }
    // Where it really is (a link or junction to a system folder is caught
    // here; checked again at every copy, so swapping the folder for a link
    // later is caught too).
    let resolved = crate::lockdown::resolve(Path::new(p)).ok().map(|r| plain(&r));
    if let Some(r) = &resolved {
        if let Some(why) = place_refusal(r) {
            return Some(format!("{why} (where it really leads)"));
        }
    }
    // Not AmberSight's own folders, not inside them and not holding them
    // (a sync app would upload every recording): as written and as resolved.
    let mut forms = vec![p.to_string()];
    forms.extend(resolved);
    let mut named: Vec<(String, PathBuf)> = vec![("AmberSight's data folder".into(), g.data_dir.clone())];
    if let Some(d) = &g.program_dir {
        named.push(("AmberSight's program folder".into(), d.clone()));
    }
    for (n, d) in &g.storage {
        named.push((format!("the recording folder \"{n}\""), d.clone()));
    }
    for (what, dir) in &named {
        // (A program run from a drive's top folder would hold everything.)
        if dir.as_os_str().is_empty() || parts_of(&dir.to_string_lossy()).parts.is_empty() {
            continue;
        }
        let mut dirs = vec![plain(dir)];
        if let Ok(r) = crate::lockdown::resolve(dir) {
            dirs.push(plain(&r));
        }
        for f in &forms {
            for d in &dirs {
                if inside(f, d) {
                    return Some(format!("it is inside {what}: pick a folder of its own"));
                }
                if inside(d, f) {
                    return Some(format!("it holds {what}: pick a folder of its own"));
                }
            }
        }
    }
    None
}

/// A server name that is this computer.
fn local_server(s: &str) -> bool {
    let me = std::env::var("COMPUTERNAME").unwrap_or_default().to_lowercase();
    matches!(s, "localhost" | "." | "?" | "::1" | "0:0:0:0:0:0:0:1" | "[::1]") || s.starts_with("127.") || (!me.is_empty() && (s == me || s.starts_with(&format!("{me}."))))
}

/// Why this path, as written, is no place for copies: a system or program
/// folder, a whole system drive, a user's own top folder, settings folders,
/// a temporary folder, a share on this computer.
fn place_refusal(p: &str) -> Option<String> {
    let Parts { drive, unc, parts } = parts_of(p);
    if parts.iter().any(|x| x == "..") {
        return Some("the folder path cannot contain ..".into());
    }
    if parts.iter().skip(if unc { 2 } else { 0 }).any(|x| x.contains(':') || (cfg!(windows) && windows_reserved(x))) {
        return Some("the folder path has a name Windows does not allow".into());
    }
    let sys = "that is a system or program folder: pick a folder of your own".to_string();
    if unc {
        if parts.len() < 2 {
            return Some(r"use a folder on the share, such as \\nas\share\cameras".into());
        }
        // \\localhost\c$\... would get round the checks of local folders.
        if local_server(&parts[0]) {
            return Some("that is a share on this computer: use the folder's own path here instead".into());
        }
    } else if parts.is_empty() {
        let sysdrive = std::env::var("SystemDrive").ok().and_then(|d| d.chars().next()).unwrap_or('C').to_ascii_lowercase();
        if drive.is_none() || drive == Some(sysdrive) {
            return Some("choose a folder, not the whole drive".into());
        }
    } else {
        let first = parts[0].as_str();
        if drive.is_some() && WIN_SYSTEM.contains(&first) {
            return Some(sys);
        }
        if drive.is_none() && (UNIX_SYSTEM.contains(&first) || (first == "var" && (parts.len() == 1 || VAR_SYSTEM.contains(&parts[1].as_str())))) {
            return Some(sys);
        }
        // C:\Users, C:\Users\name, /home/name, /root: pick a folder inside.
        if matches!(first, "users" | "home") && parts.get(1).is_some_and(|u| matches!(u.as_str(), "public" | "default" | "default user" | "all users")) && parts.len() <= 2 {
            return Some(sys);
        }
        if matches!(first, "users" | "home") && parts.len() <= 2 || (first == "root" && parts.len() == 1) {
            return Some("choose a folder inside it, such as its OneDrive or Google Drive folder, or one you made for the copies".into());
        }
        // A program's settings (AppData, a Mac's Library), temporary folders.
        if parts.iter().any(|x| x == "appdata") || (matches!(first, "users" | "home") && parts.get(2).is_some_and(|x| x == "library")) {
            return Some(sys);
        }
    }
    None
}

/// Plain words for a failure (no paths: it may go out in an alert), and
/// what to do about it.
fn explain(e: &std::io::Error, folder: &str) -> (String, String) {
    // ENOSPC, EDQUOT; ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL.
    let codes: &[i32] = if cfg!(windows) { &[112, 39] } else { &[28, 122] };
    let full = e.raw_os_error().is_some_and(|c| codes.contains(&c)) || e.kind() == std::io::ErrorKind::StorageFull;
    if full {
        return ("the drive is full or nearly full".into(), "Free some space there, or set Keep copies to a number of days. AmberSight leaves at least 2 GB (or 5%) free there, 10 GB (or 10%) on the system drive or the drive of its own data".into());
    }
    match e.kind() {
        std::io::ErrorKind::NotFound => (
            "the folder is missing or offline".into(),
            "Plug the drive in or bring the share back; AmberSight never creates the folder itself, so copies wait until it is there".into(),
        ),
        std::io::ErrorKind::PermissionDenied => {
            let hint = if cfg!(windows) && parts_of(folder).parts.first().is_some_and(|x| x == "users") {
                "The AmberSight service runs as the Local System account. Make sure that account may write to this folder (folder Properties, Security), and in OneDrive set the folder to \"Always keep on this device\"; or pick a folder outside your user folder (such as D:\\Camera copies) and have your sync app sync that one".to_string()
            } else if cfg!(windows) {
                "The AmberSight service runs as the Local System account: give that account write access to the folder (folder Properties, Security); for a network share, give this computer's account access to the share".to_string()
            } else {
                "Give the account AmberSight runs as (ambersight for the package install) write access to the folder".to_string()
            };
            ("AmberSight is not allowed to write there".into(), hint)
        }
        // Our own words (a link in the way, a folder that is not allowed).
        std::io::ErrorKind::Other if e.raw_os_error().is_none() => (e.to_string(), String::new()),
        _ => ("could not write there".into(), String::new()),
    }
}

/// What the copy folders are (by folder key), as the owner last saved or
/// tested them: a folder replaced since (deleted and made again, swapped
/// for another) is not written to until the owner accepts it with Test.
static ROOTS: Mutex<Roots> = Mutex::new(Roots { ids: BTreeMap::new(), lost: false });
static ROOTS_FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
const REPLACED: &str = "the copy folder was replaced (it is not the same folder as before); open the camera and press Test to accept it";

struct Roots {
    ids: BTreeMap<String, (u64, u64)>,
    /// The saved list could not be read: no folder is taken as it is the first time; each
    /// waits for the owner's Test (or a save of the camera).
    lost: bool,
}

impl Roots {
    /// May `key`, now `id`, be used opened `how`? `Ok(true)`: accepted now (to be saved).
    fn check(&mut self, key: String, id: (u64, u64), how: Open) -> Result<bool, &'static str> {
        match (self.ids.get(&key), how) {
            (Some(k), Open::Copy) if *k != id => Err(REPLACED),
            (Some(k), _) if *k == id => Ok(false),
            (None, Open::Copy) if self.lost => Err(REPLACED),
            _ => {
                self.ids.insert(key, id);
                Ok(true)
            }
        }
    }
}

fn roots() -> MutexGuard<'static, Roots> {
    ROOTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Written while the caller holds the lock, so two threads never share the temporary file or
/// put an older list in place after a newer one.
fn save_roots(r: &Roots) {
    let Some(path) = ROOTS_FILE.get() else { return };
    if let Err(e) = write_roots(path, r) {
        warn!("event copies: could not save the copy folders' identities: {e}");
    }
}

fn write_roots(path: &Path, r: &Roots) -> std::io::Result<()> {
    let list: Vec<(&String, u64, u64)> = r.ids.iter().map(|(k, (a, b))| (k, *a, *b)).collect();
    let b = serde_json::to_vec(&list).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&b)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// The saved list. None when it is there but cannot be read: it is moved aside (`.bad`).
fn read_roots(path: &Path) -> Option<BTreeMap<String, (u64, u64)>> {
    let b = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(BTreeMap::new()),
        Err(e) => {
            warn!("event copies: could not read {}: {e}; copies wait until each folder is accepted with Test", path.display());
            return None;
        }
    };
    match serde_json::from_slice::<Vec<(String, u64, u64)>>(&b) {
        Ok(list) => Some(list.into_iter().map(|(k, a, b)| (k, (a, b))).collect()),
        Err(e) => {
            let bad = path.with_extension("json.bad");
            let _ = std::fs::rename(path, &bad);
            warn!("event copies: {} is damaged ({e}; kept as {}); copies wait until each folder is accepted with Test", path.display(), bad.display());
            None
        }
    }
}

fn load_roots(path: &Path) {
    let read = read_roots(path);
    let mut r = roots();
    match read {
        Some(ids) => r.ids.extend(ids),
        None => r.lost = true,
    }
}

/// How the copy folder is opened: for a copy (its identity must be the one
/// accepted; the first time it is taken as it is) or for the owner's Test
/// or save (it becomes the accepted one).
#[derive(PartialEq, Clone, Copy)]
enum Open {
    Copy,
    Accept,
}

/// The copy folder, opened, if copies may go there. Resolved once: the
/// checks look at where it really is and exactly that is opened, without
/// following any link on the way (copy_fs.rs), so it cannot be swapped
/// between the two. Not open to everyone on this computer (Unix), and the
/// same folder as the one the owner accepted.
fn open_root(folder: &str, g: &Guards, how: Open) -> std::io::Result<Dir> {
    if let Some(r) = refusal(folder, g) {
        return Err(std::io::Error::other(format!("the folder cannot be used: {r}")));
    }
    let real = super::copy_fs::canonical(Path::new(folder.trim()))?;
    if let Some(r) = refusal(&plain(&real), g) {
        return Err(std::io::Error::other(format!("the folder cannot be used: {r}")));
    }
    let root = Dir::open_root(&real)?;
    if root.world_writable() {
        return Err(std::io::Error::other("anyone on this computer may change things in the folder, so AmberSight does not write there; take away other users' write access (chmod o-w) or pick another folder"));
    }
    let id = root.identity()?;
    let key = folder_key(folder);
    let mut r = roots();
    if r.check(key, id, how).map_err(std::io::Error::other)? {
        save_roots(&r);
    }
    drop(r);
    Ok(root)
}

/// The owner saved `folder` for a camera: if it is there and usable, it is
/// the accepted copy folder from now on.
pub fn accept_folder(folder: &str, g: &Guards) {
    let _ = open_root(folder, g, Open::Accept);
}

/// Folders a Test or a save is looking at now: a share that does not answer
/// gets one check at a time, not a pile of stuck threads.
static CHECKING: Mutex<std::collections::BTreeSet<String>> = Mutex::new(std::collections::BTreeSet::new());

/// Held while a folder is being checked.
pub struct CheckGuard(String);

impl Drop for CheckGuard {
    fn drop(&mut self) {
        CHECKING.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// Start checking `folder`, unless a check of it is still running.
pub fn begin_check(folder: &str) -> Option<CheckGuard> {
    let key = folder_key(folder);
    // (The lock is let go here, not at the end of the caller's statement.)
    let fresh = CHECKING.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone());
    fresh.then_some(CheckGuard(key))
}

/// Bytes the copy folder's drive must keep free: 2 GB or 5%, and 10 GB or
/// 10% when it is the system drive or the drive of AmberSight's own data.
fn space_floor(total: u64, shared: bool) -> u64 {
    if shared { (10 * GB).max(total / 10) } else { (2 * GB).max(total / 20) }
}

/// The copy folder is on the system drive or the drive of the data folder.
fn on_busy_drive(folder: &str, data_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
        let f = dev(Path::new(folder.trim()));
        f.is_some() && (f == dev(Path::new("/")) || f == dev(data_dir))
    }
    #[cfg(not(unix))]
    {
        let letter = |p: &Path| std::fs::canonicalize(p).ok().and_then(|r| parts_of(&plain(&r)).drive);
        let sys = std::env::var("SystemDrive").ok().and_then(|d| d.chars().next()).map(|c| c.to_ascii_lowercase());
        let f = letter(Path::new(folder.trim()));
        f.is_some() && (f == sys || f == letter(data_dir))
    }
}

/// Check that copies can be written to `folder` now: the rules above, the
/// folder there, and a test file written and removed. Ok: a line to show.
pub fn test_folder(folder: &str, g: &Guards) -> Result<String, String> {
    if let Some(r) = refusal(folder, g) {
        return Err(format!("This folder cannot be used: {r}."));
    }
    let root = match open_root(folder, g, Open::Accept) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("The folder does not exist (or its drive or share is offline). Create it first: AmberSight never creates it, so an unplugged drive is noticed instead of filling the system drive.".into())
        }
        Err(e) => {
            let (why, hint) = explain(&e, folder);
            return Err(format!("AmberSight cannot use this folder: {why}. {hint}").trim_end_matches(". ").to_string());
        }
    };
    let res = (|| {
        let mut f = root.create_new(&format!(".ambersight-write-test-{:08x}.tmp", rand::random::<u32>()))?;
        let r = f.file.write_all(b"AmberSight write test - safe to delete").and_then(|_| f.file.sync_all());
        root.discard(f);
        r
    })();
    if let Err(e) = res {
        let (why, hint) = explain(&e, folder);
        return Err(format!("AmberSight cannot write to this folder: {why}. {hint}").trim_end_matches(". ").to_string());
    }
    let mut msg = String::from("AmberSight can write to this folder.");
    if let Ok((free, total)) = root.space() {
        let floor = space_floor(total, on_busy_drive(folder, &g.data_dir));
        if free < floor {
            msg = format!("AmberSight can write to this folder, but its drive has only {:.1} GB free; copies wait until at least {:.0} GB is free.", free as f64 / GB as f64, floor as f64 / GB as f64);
        }
    }
    if cfg!(windows) && parts_of(folder).parts.first().is_some_and(|x| x == "users") {
        msg.push_str(" It writes as the Local System account; your sync app uploads the files while you are signed in to Windows.");
    }
    Ok(msg)
}

// ------------------------------------------------------------- copying

enum Outcome {
    /// Copied (or already there): the file, relative to the copy folder,
    /// and a note about it.
    Done(String, String),
    /// Left out, with why.
    Skip(String),
    /// Not ready yet (the recordings are still being written).
    Later(i64),
    /// The copy folder failed: the error, for the folder at this path.
    Fail(std::io::Error, String),
    /// The event's recordings could not be read (not the folder's fault).
    Source(String),
}

/// Write `name` in `dir` through a `.part` file, from `fill` (whose own
/// read errors are told apart from write errors).
fn write_file(dir: &Dir, name: &str, fill: impl FnOnce(&mut NewFile) -> Result<(), WriteErr>) -> Result<(), WriteErr> {
    let mut f = dir.create_new(&format!("{name}.{:08x}.part", rand::random::<u32>())).map_err(WriteErr::Dest)?;
    let res = fill(&mut f).and_then(|_| f.file.flush().and_then(|_| f.file.sync_all()).map_err(WriteErr::Dest)).and_then(|_| dir.rename_into(&f, name).map_err(WriteErr::Dest));
    if res.is_err() {
        dir.discard(f);
    }
    res
}

enum WriteErr {
    Source(String),
    Dest(std::io::Error),
}

/// `stem.ext`, or `stem (2).ext` ... when a different file has that name.
fn free_name(names: &[String], stem: &str, ext: &str) -> Option<String> {
    let taken = |n: &str| names.iter().any(|x| x.eq_ignore_ascii_case(n));
    (1..=20).map(|i| if i == 1 { format!("{stem}.{ext}") } else { format!("{stem} ({i}).{ext}") }).find(|n| !taken(n))
}

fn copy_one(engine: &Arc<Engine>, job: &Job, now: i64) -> Outcome {
    if now - job.queued > MAX_AGE_SECS {
        return Outcome::Skip("it waited more than 3 days".into());
    }
    let (cfg, seg_secs, pre, post) = {
        let c = engine.config.lock().unwrap();
        let Some(cam) = c.cameras.iter().find(|x| x.name == job.camera).cloned() else {
            return Outcome::Skip("the camera was removed".into());
        };
        let m = c.motion_for(&cam.name);
        (cam, c.storage.segment_seconds as i64, m.pre_roll as i64, m.post_roll as i64)
    };
    let Some(folder) = cfg.copy_folder().map(str::to_string) else {
        return Outcome::Skip("copying was turned off".into());
    };
    if cfg.copy_events_only_confirmed && !job.confirmed {
        return Outcome::Skip("the camera copies only confirmed events".into());
    }
    let g = guards(engine);
    if let Some(r) = refusal(&folder, &g) {
        return Outcome::Fail(std::io::Error::other(format!("the folder cannot be used: {r}")), folder);
    }
    let ev = match engine.db.get_event(&job.id) {
        Ok(Some(e)) => e,
        Ok(None) => return Outcome::Skip("the event was deleted".into()),
        Err(e) => return Outcome::Source(format!("could not read the event: {e}")),
    };
    if ev.camera != job.camera {
        return Outcome::Skip("the event belongs to another camera".into());
    }
    // A battery camera's first seconds come from its own card a minute or
    // two later (battery_clips.rs).
    if super::battery_clips::lead_in_pending(&cfg.name) && now - ev.end < LEAD_IN_WAIT_SECS {
        return Outcome::Later(30);
    }
    // Until the recordings reach the end of the post-roll (or clearly never will).
    let to_ms = (ev.end + post) * 1000;
    let s = slug(&cfg.name);
    let last = if cfg.record_sub { engine.db.last_main_segment_end(&s) } else { engine.db.last_segment_end(&s) }.ok().flatten().unwrap_or(0);
    if last < to_ms && now < ev.end + post + 2 * seg_secs.max(10) + 60 {
        return Outcome::Later(15);
    }
    // The source first: an event with nothing recorded is not the folder's problem.
    enum Src {
        File(PathBuf, u64),
        Clip(crate::ingest::splice::VirtualClip),
    }
    let saved = engine.clip_file_path(&ev.id);
    let mut note = String::new();
    let src = match std::fs::metadata(&saved) {
        Ok(m) if m.is_file() => Src::File(saved, m.len()),
        _ => match engine.virtual_clip(&ev) {
            Ok(Some(c)) => {
                let (want_from, want_to) = ((ev.start - pre) * 1000, to_ms);
                if c.start_ms > want_from + 3000 || c.end_ms < want_to - 3000 {
                    note = "only part of it was recorded".into();
                }
                Src::Clip(c)
            }
            Ok(None) => return Outcome::Skip("no recording covers it (recording off, or already deleted)".into()),
            Err(e) => return Outcome::Source(format!("{e:#}")),
        },
    };
    let size = match &src {
        Src::File(_, n) => *n,
        Src::Clip(c) => c.len(),
    };
    let root = match open_root(&folder, &g, Open::Copy) {
        Ok(r) => r,
        Err(e) => return Outcome::Fail(e, folder),
    };
    if let Ok((free, total)) = root.space() {
        let floor = space_floor(total, on_busy_drive(&folder, &engine.data_dir));
        if free < floor.saturating_add(size) {
            return Outcome::Fail(std::io::Error::new(std::io::ErrorKind::StorageFull, "nearly full"), folder);
        }
    }
    let t = Local.timestamp_opt(ev.start, 0).earliest().unwrap_or_else(Local::now);
    let cam_name = safe_name(&cfg.name, "Camera");
    let day = t.format("%Y-%m-%d").to_string();
    let hms = t.format("%H-%M-%S").to_string();
    // (Each level stays open until the copy is done: on Windows that keeps it
    // from being moved or swapped for a junction meanwhile.)
    let (_cam_dir, day_dir) = match root.sub(&cam_name, true).and_then(|c| c.sub(&day, true).map(|d| (c, d))) {
        Ok(d) => d,
        Err(e) => return Outcome::Fail(e, folder),
    };
    let rel = |n: &str| format!("{cam_name}/{day}/{n}");
    let entries = match day_dir.entries() {
        Ok(e) => e,
        Err(e) => return Outcome::Fail(e, folder),
    };
    // Made already (a restart between the copy and saving the queue): one of
    // ours from that second, of the very same size.
    if let Some(e) = entries.iter().find(|e| e.kind == Kind::File && e.len == size && e.name.ends_with(".mp4") && e.name.starts_with(&hms) && is_our_file(&e.name)) {
        return Outcome::Done(rel(&e.name), note);
    }
    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    let stem = file_stem(&t, &ev.objects);
    let Some(mp4) = free_name(&names, &stem, "mp4") else {
        return Outcome::Skip("too many files of that name there".into());
    };
    let res = match &src {
        Src::File(p, _) => write_file(&day_dir, &mp4, |f| {
            let mut s = std::fs::File::open(p).map_err(|e| WriteErr::Source(e.to_string()))?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = s.read(&mut buf).map_err(|e| WriteErr::Source(e.to_string()))?;
                if n == 0 {
                    return Ok(());
                }
                f.file.write_all(&buf[..n]).map_err(WriteErr::Dest)?;
            }
        }),
        Src::Clip(clip) => write_file(&day_dir, &mp4, |f| {
            let mut pos = 0;
            while pos < clip.len() {
                let next = (pos + 4 * 1024 * 1024).min(clip.len());
                let data = clip.read_range(pos, next).map_err(|e| WriteErr::Source(e.to_string()))?;
                f.file.write_all(&data).map_err(WriteErr::Dest)?;
                pos = next;
            }
            Ok(())
        }),
    };
    match res {
        Ok(()) => {}
        Err(WriteErr::Source(e)) => return Outcome::Source(e),
        Err(WriteErr::Dest(e)) => return Outcome::Fail(e, folder),
    }
    // The event's picture beside it, under the clip's name (not worth failing the copy for).
    let pic_name = ev.thumb.as_deref().filter(|t| !t.is_empty() && !t.contains(['/', '\\', ':', '\0']) && !t.starts_with('.'));
    if let Some(th) = pic_name {
        let pic = engine.data_dir.join("thumbs").join(format!("{th}.jpg"));
        if pic.is_file() {
            let jpg = format!("{}.jpg", mp4.trim_end_matches(".mp4"));
            let r = write_file(&day_dir, &jpg, |f| {
                let b = std::fs::read(&pic).map_err(|e| WriteErr::Source(e.to_string()))?;
                f.file.write_all(&b).map_err(WriteErr::Dest)
            });
            if r.is_err() {
                warn!("event copies: could not save the picture of {}", ev.id);
            }
        }
    }
    Outcome::Done(rel(&mp4), note)
}

/// Delete copies older than `keep_days` in one camera's folder: only
/// AmberSight's own names, in `YYYY-MM-DD` folders, never through a link.
/// A date folder left empty is removed. Returns the files deleted.
pub fn trim_camera(cam: &Dir, keep_days: u32, today: NaiveDate, now: std::time::SystemTime) -> usize {
    if keep_days == 0 {
        return 0;
    }
    let Some(oldest_kept) = today.checked_sub_days(chrono::Days::new(keep_days as u64)) else { return 0 };
    let min_age = std::time::Duration::from_secs(keep_days as u64 * 86400);
    let mut n = 0;
    let Ok(days) = cam.entries() else { return 0 };
    for d in days.into_iter().filter(|d| d.kind == Kind::Dir) {
        if !day_of(&d.name).is_some_and(|date| date < oldest_kept) {
            continue;
        }
        {
            let Ok(day) = cam.sub(&d.name, false) else { continue };
            let Ok(files) = day.entries() else { continue };
            for f in files {
                let old = f.modified.and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age >= min_age);
                if f.kind == Kind::File && is_our_file(&f.name) && old && day.remove_file(&f.name).is_ok() {
                    n += 1;
                }
            }
        }
        // Only when empty (anything else someone put there stays).
        let _ = cam.remove_empty_dir(&d.name);
    }
    n
}

/// Folders being trimmed now: a hung one is left alone until it answers.
static TRIMMING: Mutex<std::collections::BTreeSet<String>> = Mutex::new(std::collections::BTreeSet::new());

/// Trim every copy folder, each on a thread of its own (a share that does
/// not answer holds up only its own cameras' trim).
fn trim_all(engine: &Arc<Engine>) {
    let cams: Vec<CameraConfig> = engine.config.lock().unwrap().cameras.iter().filter(|c| c.copy_folder().is_some() && c.copy_events_keep_days > 0).cloned().collect();
    let mut by_folder: BTreeMap<String, Vec<CameraConfig>> = BTreeMap::new();
    for c in cams {
        by_folder.entry(folder_key(c.copy_folder().unwrap_or_default())).or_default().push(c);
    }
    let g = guards(engine);
    for (key, cams) in by_folder {
        if !TRIMMING.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone()) {
            info!("event copies: a copy folder is still being trimmed from before; skipped this time");
            continue;
        }
        let g = g.clone();
        let _ = thread::Builder::new().name("event-copy-trim-folder".into()).spawn(move || {
            let today = Local::now().date_naive();
            for c in cams {
                let folder = c.copy_folder().unwrap_or_default().to_string();
                let Ok(root) = open_root(&folder, &g, Open::Copy) else { continue };
                let Ok(cam) = root.sub(&safe_name(&c.name, "Camera"), false) else { continue };
                let n = trim_camera(&cam, c.copy_events_keep_days, today, std::time::SystemTime::now());
                if n > 0 {
                    info!("event copies: deleted {n} copies of {} older than {} days", c.name, c.copy_events_keep_days);
                    st().status.entry(c.name.clone()).or_default().trimmed += n as u64;
                }
            }
            TRIMMING.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        });
    }
}

/// Start the copy threads (once): reads the queue left from the last run.
pub fn start(engine: Arc<Engine>) {
    let path = engine.data_dir.join("event-copy-queue.json");
    let roots_path = engine.data_dir.join("event-copy-folders.json");
    load_roots(&roots_path);
    let _ = ROOTS_FILE.set(roots_path);
    let now = Utc::now().timestamp();
    {
        let mut st = st();
        for j in load_queue(&path, now) {
            if !st.jobs.iter().any(|x| x.id == j.id) {
                st.jobs.push_back(Job { next_try: now + 60, ..j });
            }
        }
        if !st.jobs.is_empty() {
            info!("event copies: {} waiting from before the restart", st.jobs.len());
        }
    }
    if QUEUE_FILE.set(path).is_err() {
        return;
    }
    for i in 0..WORKERS {
        let eng = engine.clone();
        let _ = thread::Builder::new().name(format!("event-copy-{i}")).spawn(move || worker(eng));
    }
    let _ = thread::Builder::new().name("event-copy-trim".into()).spawn(move || {
        thread::sleep(std::time::Duration::from_secs(120));
        loop {
            trim_all(&engine);
            thread::sleep(std::time::Duration::from_secs(3600));
        }
    });
}

/// Take the next copy that is due and whose folder no other thread is
/// writing to; marks that folder busy.
fn next_job(folders: &HashMap<String, String>, now: i64) -> Option<(Job, String)> {
    let mut st = st();
    let pick = st.jobs.iter().find(|j| j.next_try <= now && folders.get(&j.camera).is_none_or(|f| !st.busy.contains_key(f))).cloned();
    match pick {
        Some(j) => {
            let key = folders.get(&j.camera).cloned().unwrap_or_default();
            if !key.is_empty() {
                st.busy.insert(key.clone(), now);
                for (cam, f) in folders {
                    if *f == key {
                        st.status.entry(cam.clone()).or_default().busy_since = Some(now);
                    }
                }
            }
            Some((j, key))
        }
        None => {
            let soonest = st.jobs.iter().map(|j| j.next_try).min().unwrap_or(now + 60);
            let wait = (soonest - now).clamp(1, 60) as u64;
            let _ = WAKE.wait_timeout(st, std::time::Duration::from_secs(wait));
            None
        }
    }
}

fn worker(engine: Arc<Engine>) {
    loop {
        let now = Utc::now().timestamp();
        save_queue();
        let folders: HashMap<String, String> = engine.config.lock().unwrap().cameras.iter().filter_map(|c| Some((c.name.clone(), folder_key(c.copy_folder()?)))).collect();
        let Some((job, key)) = next_job(&folders, now) else { continue };
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| copy_one(&engine, &job, now))).unwrap_or_else(|_| Outcome::Source("an internal error".into()));
        let mut st = st();
        if !key.is_empty() {
            st.busy.remove(&key);
            for (cam, f) in &folders {
                if *f == key {
                    st.status.entry(cam.clone()).or_default().busy_since = None;
                }
            }
        }
        WAKE.notify_all();
        let now = Utc::now().timestamp();
        let drop_job = |st: &mut State| {
            st.jobs.retain(|j| j.id != job.id);
            st.dirty = true;
        };
        match out {
            Outcome::Done(file, note) => {
                drop_job(&mut st);
                let s = st.status.entry(job.camera.clone()).or_default();
                if s.failing_since.take().is_some() {
                    info!("event copies: {} saves to its copy folder again", job.camera);
                }
                if !note.is_empty() {
                    info!("event copies: {file} of {}: {note}", job.camera);
                }
                s.copied += 1;
                s.last_ok = now;
                s.last_file = file;
                s.last_note = note;
                s.failures = 0;
                s.last_error.clear();
                s.hint.clear();
                // The others waiting for this camera need not wait out a retry.
                for j in st.jobs.iter_mut().filter(|j| j.camera == job.camera) {
                    j.next_try = j.next_try.min(now);
                }
            }
            Outcome::Skip(why) => {
                drop_job(&mut st);
                if why != "the camera was removed" && why != "copying was turned off" {
                    info!("event copies: skipped {} of {}: {why}", job.id, job.camera);
                    let s = st.status.entry(job.camera.clone()).or_default();
                    s.skipped += 1;
                    s.last_skip = why;
                }
            }
            Outcome::Later(secs) => {
                if let Some(j) = st.jobs.iter_mut().find(|j| j.id == job.id) {
                    j.next_try = now + secs;
                }
            }
            Outcome::Source(detail) => {
                // This event only: the folder is fine.
                let tries = job.tries + 1;
                warn!("event copies: could not read the recordings of {} ({detail}); try {tries} of {SOURCE_TRIES}", job.id);
                if tries >= SOURCE_TRIES {
                    drop_job(&mut st);
                    let s = st.status.entry(job.camera.clone()).or_default();
                    s.skipped += 1;
                    s.last_skip = "could not read the recordings".into();
                } else if let Some(j) = st.jobs.iter_mut().find(|j| j.id == job.id) {
                    j.tries = tries;
                    j.next_try = now + 60;
                    st.dirty = true;
                }
            }
            Outcome::Fail(e, folder) => {
                let (why, hint) = explain(&e, &folder);
                let s = st.status.entry(job.camera.clone()).or_default();
                s.failures += 1;
                let wait = RETRY[(s.failures as usize - 1).min(RETRY.len() - 1)];
                if s.failing_since.is_none() || s.last_error != why {
                    warn!("event copies: {} could not save {} to its copy folder: {why} ({e}); trying again in {wait} s", job.camera, job.id);
                }
                s.failing_since.get_or_insert(now);
                s.last_error = why;
                s.hint = hint;
                // The folder is the camera's: its other copies wait too.
                for j in st.jobs.iter_mut().filter(|j| j.camera == job.camera) {
                    j.next_try = j.next_try.max(now + wait);
                }
                st.dirty = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_damaged_list_of_copy_folders_is_moved_aside_and_accepts_nothing() {
        let d = std::env::temp_dir().join(format!("amber-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("event-copy-folders.json");
        // Not there yet: an empty list, folders taken as they are the first time.
        assert_eq!(read_roots(&p), Some(BTreeMap::new()));
        let mut r = Roots { ids: BTreeMap::new(), lost: false };
        assert_eq!(r.check("a".into(), (1, 2), Open::Copy), Ok(true));
        assert_eq!(r.check("a".into(), (1, 2), Open::Copy), Ok(false));
        assert_eq!(r.check("a".into(), (1, 3), Open::Copy), Err(REPLACED));
        write_roots(&p, &r).unwrap();
        assert!(!p.with_extension("json.tmp").exists());
        assert_eq!(read_roots(&p).unwrap().get("a"), Some(&(1, 2)));
        // Half written (two writers once shared the temporary file).
        std::fs::write(&p, b"[[\"a\",1,2]][[\"b\"").unwrap();
        assert_eq!(read_roots(&p), None);
        assert!(!p.exists() && p.with_extension("json.bad").exists());
        let mut r = Roots { ids: BTreeMap::new(), lost: true };
        assert_eq!(r.check("a".into(), (1, 2), Open::Copy), Err(REPLACED), "not taken as it is");
        assert_eq!(r.check("a".into(), (1, 2), Open::Accept), Ok(true), "Test accepts it");
        assert_eq!(r.check("a".into(), (1, 2), Open::Copy), Ok(false));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_folders_are_saved_whole_from_many_threads() {
        let d = std::env::temp_dir().join(format!("amber-roots-mt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("event-copy-folders.json");
        let shared = std::sync::Arc::new(Mutex::new(Roots { ids: BTreeMap::new(), lost: false }));
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let (shared, p) = (shared.clone(), p.clone());
                std::thread::spawn(move || {
                    for i in 0..20u64 {
                        let mut r = shared.lock().unwrap();
                        r.check(format!("folder-{t}-{i}"), (t, i), Open::Accept).unwrap();
                        write_roots(&p, &r).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(read_roots(&p).map(|m| m.len()), Some(160));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn names_are_single_safe_components() {
        assert_eq!(safe_name("Front Door", "x"), "Front Door");
        assert_eq!(safe_name("../../etc", "x"), "_.._etc");
        assert_eq!(safe_name("..", "Camera"), "Camera");
        assert_eq!(safe_name("a/b\\c:d*e?f\"g<h>i|j", "x"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(safe_name("CON", "x"), "_CON");
        assert_eq!(safe_name("com1.txt", "x"), "_com1.txt");
        assert_eq!(safe_name("LPT9", "x"), "_LPT9");
        assert_eq!(safe_name("Console", "x"), "Console");
        assert_eq!(safe_name("trailing. . ", "x"), "trailing");
        assert_eq!(safe_name(".hidden", "x"), "hidden");
        assert_eq!(safe_name("tab\there\u{202E}gpj", "x"), "tab_here_gpj");
        assert_eq!(safe_name("a\u{00A0}b\u{2028}c\u{2060}d\u{E0041}e", "x"), "a_b_c_d_e");
        assert_eq!(safe_name("   ", "Camera"), "Camera");
        assert_eq!(safe_name(&"x".repeat(200), "c").len(), NAME_MAX);
        for n in ["/", "\\", "..", "a/../b", "C:", "nul", "a\0b"] {
            let s = safe_name(n, "Camera");
            assert!(!s.contains(['/', '\\', ':', '\0']) && s != ".." && !windows_reserved(&s), "{n} -> {s}");
        }
    }

    #[test]
    fn stems_and_our_files() {
        let t = Local.with_ymd_and_hms(2026, 11, 13, 14, 3, 22).unwrap();
        assert_eq!(file_stem(&t, &[]), "14-03-22");
        assert_eq!(file_stem(&t, &["person".into(), "car".into(), "person".into()]), "14-03-22 person, car");
        assert_eq!(file_stem(&t, &["a/b".into()]), "14-03-22 a_b");
        let long: Vec<String> = (0..6).map(|i| format!("label{i}-{}", "y".repeat(20))).collect();
        assert!(file_stem(&t, &long).chars().count() <= 8 + 1 + NAME_MAX);
        for n in ["14-03-22.mp4", "14-03-22 person, car.mp4", "14-03-22 person.jpg", "14-03-22 person.mp4.part", "14-03-22 person.mp4.0a1b2c3d.part", "14-03-22 (2).mp4", "00-00-00.jpg.part"] {
            assert!(is_our_file(n), "{n}");
        }
        for n in ["notes.txt", "14-03-22.txt", "14-03-22person.mp4", "1a-03-22.mp4", "14-03.mp4", "family.jpg", "14-03-22 x.mp4.bak", ".mp4", "14-03-22.mp4.zzzzzzzz.part.x"] {
            assert!(!is_our_file(n), "{n}");
        }
        let names = vec!["14-03-22.mp4".to_string(), "14-03-22 (2).MP4".to_string()];
        assert_eq!(free_name(&names, "14-03-22", "mp4").as_deref(), Some("14-03-22 (3).mp4"));
        assert_eq!(free_name(&[], "14-03-22", "mp4").as_deref(), Some("14-03-22.mp4"));
    }

    /// A throwaway folder, removed when dropped. Under target/: the usual
    /// temporary folder is refused (and on Windows it is under AppData).
    struct TestDir(PathBuf);
    impl TestDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn test_dir() -> TestDir {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::current_dir().unwrap().join("target").join(format!("event-copy-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        TestDir(d)
    }

    fn g(dir: &Path) -> Guards {
        Guards { data_dir: dir.join("data"), program_dir: Some(dir.join("prog")), storage: vec![("nas".into(), dir.join("nas"))] }
    }

    #[test]
    fn folder_rules() {
        let tmp = test_dir();
        let gd = g(tmp.path());
        let ok = tmp.path().join("copies");
        assert_eq!(refusal(&ok.to_string_lossy(), &gd), None);
        // AmberSight's own folders: not inside, not holding them.
        for bad in [tmp.path().join("data"), tmp.path().join("data").join("x"), tmp.path().join("prog").join("c"), tmp.path().join("nas").join("c"), tmp.path().to_path_buf()] {
            assert!(refusal(&bad.to_string_lossy(), &gd).is_some(), "{}", bad.display());
        }
        assert!(refusal("", &gd).is_some());
        assert!(refusal("relative/dir", &gd).is_some());
        assert!(refusal(&format!("{}/a/../b", tmp.path().display()), &gd).is_some());
        assert!(refusal(&format!("{}/a\nb", tmp.path().display()), &gd).is_some());
        assert!(inside("/a/b/c", "/a/b") && !inside("/a/bc", "/a/b") && !inside("/a", "/a/b"));
        #[cfg(unix)]
        {
            for bad in ["/", "/etc", "/etc/x", "/usr/local/x", "/proc/self", "/var", "/var/lib/x", "/var/tmp/x", "/tmp", "/tmp/cams", "/home", "/home/pat", "/root", "/Users/pat", "/Users/pat/Library/x", "/System/x", "/dev/shm/x", "//etc/x"] {
                assert!(refusal(bad, &gd).is_some(), "{bad}");
            }
            for good in ["/home/pat/Dropbox/Cameras", "/mnt/usb/cams", "/media/pat/USB/cams", "/srv/copies", "/var/backups/cams", "/Users/pat/Google Drive/Cams", "/Volumes/USB/cams", "/copies"] {
                assert_eq!(refusal(good, &gd), None, "{good}");
            }
            // A link that leads into the data folder is caught by where it really is.
            std::fs::create_dir_all(tmp.path().join("data").join("inner")).unwrap();
            let link = tmp.path().join("innocent");
            std::os::unix::fs::symlink(tmp.path().join("data").join("inner"), &link).unwrap();
            assert!(refusal(&link.to_string_lossy(), &gd).unwrap().contains("data folder"));
            // A link to a system folder is refused by where it leads.
            let to_etc = tmp.path().join("to-etc");
            std::os::unix::fs::symlink("/etc", &to_etc).unwrap();
            assert!(refusal(&to_etc.to_string_lossy(), &gd).unwrap().contains("really leads"));
        }
        #[cfg(windows)]
        {
            for bad in [r"C:\", r"C:\Windows\x", r"c:\program files\x", r"C:\ProgramData\x", r"C:\Users", r"C:\Users\pat", r"C:\Users\pat\AppData\Local\x", r"\\?\C:\x", r"\\.\PhysicalDrive0", r"\\nas", r"C:\x\CON\y", r"C:\x\a:b", r"/x", r"\\localhost\c$\x", r"\\127.0.0.1\share\x"] {
                assert!(refusal(bad, &gd).is_some(), "{bad}");
            }
            for good in [r"C:\Users\pat\OneDrive\Cameras", r"D:\", r"D:\Camera copies", r"\\nas\share\cams", r"\\nas\share"] {
                assert_eq!(refusal(good, &gd), None, "{good}");
            }
            // Network recording folders: the same server and share count.
            let gn = Guards { storage: vec![("nas".into(), PathBuf::from(r"\\NAS\rec\cams"))], ..gd.clone() };
            assert!(refusal(r"\\nas\rec\cams\copies", &gn).is_some());
            assert!(refusal(r"\\nas\rec", &gn).is_some());
            assert_eq!(refusal(r"\\nas\other\copies", &gn), None);
            assert_eq!(refusal(r"\\other\rec\cams\copies", &gn), None);
        }
    }

    #[test]
    fn test_folder_needs_an_existing_writable_folder() {
        let tmp = test_dir();
        let gd = g(tmp.path());
        let missing = tmp.path().join("gone");
        assert!(test_folder(&missing.to_string_lossy(), &gd).unwrap_err().contains("does not exist"));
        assert!(!missing.exists(), "never created");
        let dir = tmp.path().join("copies");
        std::fs::create_dir(&dir).unwrap();
        assert!(test_folder(&dir.to_string_lossy(), &gd).is_ok());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "the test file is removed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if !nix_root() {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
                let e = test_folder(&dir.to_string_lossy(), &gd).unwrap_err();
                assert!(e.contains("not allowed"), "{e}");
            }
            // Open to everyone (sticky or not): refused.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
            assert!(test_folder(&dir.to_string_lossy(), &gd).unwrap_err().contains("anyone"));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[cfg(unix)]
    fn nix_root() -> bool {
        // SAFETY: getuid has no preconditions.
        unsafe { libc::getuid() == 0 }
    }

    #[test]
    fn a_replaced_copy_folder_waits_for_the_owner() {
        let tmp = test_dir();
        let gd = g(tmp.path());
        let dir = tmp.path().join("swap");
        std::fs::create_dir(&dir).unwrap();
        let f = dir.to_string_lossy().to_string();
        assert!(open_root(&f, &gd, Open::Copy).is_ok(), "taken as it is the first time");
        assert!(open_root(&f, &gd, Open::Copy).is_ok());
        std::fs::rename(&dir, tmp.path().join("old")).unwrap();
        std::fs::create_dir(&dir).unwrap();
        let e = open_root(&f, &gd, Open::Copy).err().unwrap();
        assert!(e.to_string().contains("replaced"), "{e}");
        assert!(test_folder(&f, &gd).is_ok(), "Test accepts it");
        assert!(open_root(&f, &gd, Open::Copy).is_ok());
        #[cfg(unix)]
        {
            // Replaced by a link to somewhere allowed: where it leads is a
            // different folder, so it waits too.
            std::fs::remove_dir(&dir).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("old"), &dir).unwrap();
            assert!(open_root(&f, &gd, Open::Copy).err().unwrap().to_string().contains("replaced"));
            // A link into AmberSight's own folders is refused by where it leads.
            std::fs::create_dir_all(tmp.path().join("data").join("x")).unwrap();
            std::fs::remove_file(&dir).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("data").join("x"), &dir).unwrap();
            assert!(open_root(&f, &gd, Open::Accept).err().unwrap().to_string().contains("data folder"));
        }
        assert!(begin_check(&f).is_some_and(|g1| begin_check(&f).is_none() && { drop(g1); begin_check(&f).is_some() }));
    }

    #[test]
    fn writes_through_part_files_with_new_names() {
        let tmp = test_dir();
        let root_path = tmp.path().join("copies");
        std::fs::create_dir(&root_path).unwrap();
        let root = Dir::open_root(&super::super::copy_fs::canonical(&root_path).unwrap()).unwrap();
        let day = root.sub("Cam", true).and_then(|c| c.sub("2026-11-13", true)).unwrap();
        write_file(&day, "10-00-00.mp4", |f| f.file.write_all(b"video").map_err(WriteErr::Dest)).ok().unwrap();
        assert_eq!(std::fs::read(root_path.join("Cam/2026-11-13/10-00-00.mp4")).unwrap(), b"video");
        // A failed write leaves nothing behind, a source error is told apart.
        assert!(matches!(write_file(&day, "11-00-00.mp4", |_| Err(WriteErr::Source("gone".into()))), Err(WriteErr::Source(_))));
        let left: Vec<String> = day.entries().unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(left, vec!["10-00-00.mp4".to_string()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root_path.join("Cam/2026-11-13/10-00-00.mp4")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode & 0o007, 0, "nothing for other users: {mode:o}");
        }
    }

    #[test]
    fn trim_deletes_only_old_files_of_ours() {
        let tmp = test_dir();
        let cam_path = tmp.path().join("Cam");
        let old = cam_path.join("2026-10-01");
        let new = cam_path.join("2026-11-12");
        let other = cam_path.join("holiday");
        for d in [&old, &new, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        for (d, n) in [(&old, "08-00-00 person.mp4"), (&old, "08-00-00 person.jpg"), (&old, "09-00-00.mp4.0badf00d.part"), (&new, "08-00-00.mp4"), (&other, "08-00-00.mp4")] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        std::fs::write(old.join("notes.txt"), b"mine").unwrap();
        let cam = Dir::open_root(&super::super::copy_fs::canonical(&cam_path).unwrap()).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 11, 13).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(40 * 86400);
        // Files written just now are not old enough yet, whatever their folder says.
        assert_eq!(trim_camera(&cam, 30, today, std::time::SystemTime::now()), 0);
        assert_eq!(trim_camera(&cam, 30, today, later), 3);
        assert_eq!(std::fs::read_dir(&old).unwrap().count(), 1, "notes.txt stays, so does its folder");
        assert!(new.join("08-00-00.mp4").exists() && other.join("08-00-00.mp4").exists());
        std::fs::remove_file(old.join("notes.txt")).unwrap();
        std::fs::write(old.join("10-00-00.mp4"), b"x").unwrap();
        assert_eq!(trim_camera(&cam, 30, today, later), 1);
        assert!(!old.exists(), "an emptied date folder goes");
        assert_eq!(trim_camera(&cam, 0, today, later), 0);
        #[cfg(unix)]
        {
            // A date folder that is a link leads outside: never followed.
            let outside = tmp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("08-00-00.mp4"), b"x").unwrap();
            std::os::unix::fs::symlink(&outside, cam_path.join("2020-01-01")).unwrap();
            assert_eq!(trim_camera(&cam, 30, today, later), 0);
            assert!(outside.join("08-00-00.mp4").exists());
        }
    }

    #[test]
    fn queue_file_is_read_back_and_a_bad_one_kept_aside() {
        let mut st = State { jobs: VecDeque::new(), status: BTreeMap::new(), dirty: false, busy: BTreeMap::new() };
        let j = |id: &str| Job { id: id.into(), camera: "C".into(), confirmed: false, queued: 0, tries: 0, next_try: 0 };
        st.jobs.push_back(j("a"));
        assert_eq!(waiting(&st, "C"), 1);
        assert_eq!(waiting(&st, "D"), 0);
        let tmp = test_dir();
        let p = tmp.path().join("q.json");
        let now = 10 * 86400;
        let jobs = vec![Job { queued: now - 10, ..j("new") }, Job { queued: now - MAX_AGE_SECS - 1, ..j("old") }];
        std::fs::write(&p, serde_json::to_vec(&jobs).unwrap()).unwrap();
        let back = load_queue(&p, now);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "new");
        // The 0.8 field name still reads.
        std::fs::write(&p, br#"[{"id":"x","camera":"C","alert":true,"queued":864000}]"#).unwrap();
        assert!(load_queue(&p, now)[0].confirmed);
        std::fs::write(&p, b"{not json").unwrap();
        assert!(load_queue(&p, now).is_empty());
        assert!(tmp.path().join("q.json.bad").exists() && !p.exists());
    }

    #[test]
    fn copy_settings_change_without_a_camera_restart() {
        let old: CameraConfig = toml::from_str("name = \"a\"\nurl = \"rtsp://x/y\"").unwrap();
        let mut new = old.clone();
        new.copy_events_to = Some("/mnt/c".into());
        new.copy_events_keep_days = 9;
        new.copy_events_only_confirmed = true;
        assert!(!super::super::restart_needed(&old, &new));
        let back: CameraConfig = toml::from_str(&toml::to_string(&new).unwrap()).unwrap();
        assert_eq!((back.copy_folder(), back.copy_events_keep_days, back.copy_events_only_confirmed), (Some("/mnt/c"), 9, true));
        assert!(!toml::to_string(&old).unwrap().contains("copy_events"), "nothing written while off");
        let dev: CameraConfig = toml::from_str("name = \"a\"\nurl = \"rtsp://x/y\"\ncopy_events_only_alerts = true").unwrap();
        assert!(dev.copy_events_only_confirmed, "the name from development builds still reads");
    }

    #[test]
    fn two_cameras_cannot_share_a_folder_name() {
        let cam = |n: &str, f: &str| -> CameraConfig {
            let mut c: CameraConfig = toml::from_str(&format!("name = \"{n}\"\nurl = \"rtsp://x/y\"")).unwrap();
            c.copy_events_to = Some(f.into());
            c
        };
        let a = cam("Front: Door", "/mnt/c");
        let others = [cam("Front_ Door", "/mnt/c/"), cam("Front: Door2", "/mnt/c")];
        assert_eq!(name_clash(&a, others.iter()).as_deref(), Some("Front_ Door"));
        assert_eq!(name_clash(&a, others[1..].iter()), None);
        assert_eq!(name_clash(&a, [cam("front: door", "/mnt/d")].iter()), None);
    }

    #[test]
    fn renamed_camera_keeps_its_waiting_copies() {
        st().jobs.push_back(Job { id: "rn-test-1".into(), camera: "Rn Old".into(), confirmed: false, queued: 0, tries: 0, next_try: i64::MAX });
        rename_camera("Rn Old", "Rn New");
        let mut st = st();
        assert_eq!(waiting(&st, "Rn New"), 1);
        assert_eq!(waiting(&st, "Rn Old"), 0);
        st.jobs.retain(|j| j.id != "rn-test-1");
    }

    #[test]
    fn plain_reasons_without_paths() {
        let (w, h) = explain(&std::io::Error::from(std::io::ErrorKind::NotFound), "/mnt/x");
        assert!(w.contains("missing or offline") && h.contains("never creates"));
        let (w, _) = explain(&std::io::Error::from(std::io::ErrorKind::PermissionDenied), "/mnt/x");
        assert!(w.contains("not allowed"));
        let (w, _) = explain(&std::io::Error::from_raw_os_error(if cfg!(windows) { 112 } else { 28 }), "/mnt/x");
        assert!(w.contains("full"));
        let (w, _) = explain(&std::io::Error::from_raw_os_error(if cfg!(windows) { 1117 } else { 5 }), "/mnt/x");
        assert_eq!(w, "could not write there");
        assert_eq!(space_floor(100 * GB, false), 5 * GB);
        assert_eq!(space_floor(10 * GB, false), 2 * GB);
        assert_eq!(space_floor(1000 * GB, true), 100 * GB);
    }

    #[test]
    fn problems_wait_for_a_second_failure_or_a_minute() {
        let mut c: CameraConfig = toml::from_str("name = \"Pb Cam\"\nurl = \"rtsp://x/y\"").unwrap();
        c.copy_events_to = Some("/mnt/c".into());
        let now = 1_000_000;
        st().status.insert("Pb Cam".into(), CamStatus { failing_since: Some(now - 10), failures: 1, last_error: "x".into(), ..Default::default() });
        assert!(problems(std::slice::from_ref(&c), now).is_empty());
        st().status.get_mut("Pb Cam").unwrap().failures = 2;
        assert_eq!(problems(std::slice::from_ref(&c), now).len(), 1);
        st().status.insert("Pb Cam".into(), CamStatus { busy_since: Some(now - STUCK_SECS), ..Default::default() });
        assert!(problems(std::slice::from_ref(&c), now)[0].text.contains("not responding"));
        st().status.remove("Pb Cam");
    }
}
