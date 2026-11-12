//! Keeping other accounts on the same computer out of AmberSight's folders.
//!
//! Windows: the service runs as SYSTEM, so whoever can write to the program
//! folder controls the computer, and whoever can write to the data folder
//! can edit the two-step and token data in the database. A folder made by
//! hand, such as C:\AmberSight, inherits "Authenticated Users: Modify". Here
//! the program folder and the data folder are limited to SYSTEM,
//! Administrators and the non-broad entries that were already there. A
//! recording folder gets the same only when its "Keep this folder private"
//! switch is on.
//!
//! Linux and macOS: the service runs as a normal user, so this only keeps the
//! settings file, the database and the data folder from being read by others.
//!
//! Rules that matter: only the top folder is changed,
//! never `/T` (with `/inheritance:r` it once left every file with an empty
//! ACL); nothing is done when the folder is already closed; a drive root, a
//! profile or system folder, or a program folder that holds other programs is
//! refused; the work runs on one background thread, one folder at a time; a
//! failure is logged and recording carries on.
//!
//! The decisions are plain functions without OS gates so they are tested on
//! every system; the code that changes the computer is at the bottom.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What a person is told for a drive root and the like.
pub const CHOOSE_FOLDER_MSG: &str = "choose a folder on that drive, not the whole drive";
pub const UNSUPPORTED_MSG: &str = "cannot be made private on this kind of drive";

// ---------------------------------------------------------------------
// Reading an ACL (Windows SDDL, as written by `icacls /save`)
// ---------------------------------------------------------------------

/// icacls writes its save file as UTF-16LE, with or without a byte-order
/// mark (without, on current Windows). Reading it as anything else garbles
/// names with non-Latin letters, and the folder is then never closed.
pub fn decode_icacls_save(bytes: &[u8]) -> String {
    let body = bytes.strip_prefix(&[0xFF, 0xFE][..]).unwrap_or(bytes);
    let units: Vec<u16> = body.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let wide = String::from_utf16_lossy(&units);
    if parse_aces(&wide).is_empty() {
        let narrow = String::from_utf8_lossy(bytes).into_owned();
        if !parse_aces(&narrow).is_empty() {
            return narrow;
        }
    }
    wide
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ace {
    /// "A" allow, "D" deny, others as SDDL has them.
    pub kind: String,
    pub flags: String,
    pub rights: String,
    pub sid: String,
}

/// The access control entries of the DACL in an icacls save file (the last
/// line is the SDDL; the line before it names the folder).
pub fn parse_aces(save_text: &str) -> Vec<Ace> {
    let Some(line) = save_text.lines().map(str::trim).filter(|l| !l.is_empty()).last() else { return Vec::new() };
    aces_of_line(line)
}

/// A save of several objects: (name, entries) for each, two lines apiece.
pub fn parse_save_pairs(save_text: &str) -> Vec<(String, Vec<Ace>)> {
    let lines: Vec<&str> = save_text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].to_string(), aces_of_line(c[1]))).collect()
}

fn aces_of_line(line: &str) -> Vec<Ace> {
    // "D:" that starts the DACL: followed by flags, then the first entry.
    let b = line.as_bytes();
    let mut start = None;
    for i in 0..b.len().saturating_sub(1) {
        if &b[i..i + 2] == b"D:" {
            let rest = &line[i + 2..];
            let flags_len = rest.bytes().take_while(|c| b"PAIRNOUC".contains(c)).count();
            if rest[flags_len..].starts_with('(') {
                start = Some(i + 2);
                break;
            }
        }
    }
    let Some(start) = start else { return Vec::new() };
    let mut out = Vec::new();
    let (mut depth, mut from) = (0usize, 0usize);
    let tail = &line[start..];
    for (i, c) in tail.char_indices() {
        match c {
            '(' => {
                if depth == 0 {
                    from = i + 1;
                }
                depth += 1;
            }
            ')' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let parts: Vec<&str> = tail[from..i].split(';').collect();
                    if parts.len() >= 6 {
                        out.push(Ace { kind: parts[0].into(), flags: parts[1].into(), rights: parts[2].into(), sid: parts[5].into() });
                    }
                }
            }
            // The SACL (auditing) is not access.
            'S' if depth == 0 && tail[i..].starts_with("S:") => break,
            _ => {}
        }
    }
    out
}

/// The SID, as icacls takes it, of an entry that stands for "more than the
/// administrators and the service": Everyone, Authenticated Users, Users,
/// Guests, Anonymous, Interactive, Network, Local, and the domain's Users and
/// Guests (their full SID). The open test and the remove list both use this.
pub fn broad_sid(sid: &str) -> Option<String> {
    let u = sid.to_ascii_uppercase();
    let s = match u.as_str() {
        "WD" => "S-1-1-0",
        "AU" => "S-1-5-11",
        "BU" => "S-1-5-32-545",
        "BG" => "S-1-5-32-546",
        "AN" => "S-1-5-7",
        "IU" => "S-1-5-4",
        "NU" => "S-1-5-2",
        "S-1-1-0" | "S-1-5-11" | "S-1-5-32-545" | "S-1-5-32-546" | "S-1-5-7" | "S-1-5-4" | "S-1-5-2" | "S-1-2-0" => return Some(u),
        _ if u.starts_with("S-1-5-21-") && (u.ends_with("-513") || u.ends_with("-514")) => return Some(u),
        _ => return None,
    };
    Some(s.to_string())
}

pub fn is_broad_sid(sid: &str) -> bool {
    broad_sid(sid).is_some()
}

fn is_system_sid(sid: &str) -> bool {
    sid.eq_ignore_ascii_case("SY") || sid == "S-1-5-18"
}
fn is_admins_sid(sid: &str) -> bool {
    sid.eq_ignore_ascii_case("BA") || sid == "S-1-5-32-544"
}

/// Whether any entry lets a broad group do something (a deny does not).
pub fn acl_is_open(aces: &[Ace]) -> bool {
    aces.iter().any(|a| a.kind.starts_with('A') && is_broad_sid(&a.sid) && !a.rights.is_empty() && a.rights != "0")
}

/// Whether SYSTEM and the Administrators can still do everything they need.
pub fn acl_keeps_service(aces: &[Ace]) -> bool {
    let has = |f: fn(&str) -> bool| aces.iter().any(|a| a.kind == "A" && f(&a.sid) && !a.flags.contains("IO"));
    has(is_system_sid) && has(is_admins_sid)
}

// ---------------------------------------------------------------------
// The icacls command lines
// ---------------------------------------------------------------------

const BROAD_REMOVE: [&str; 8] = ["S-1-1-0", "S-1-5-11", "S-1-5-32-545", "S-1-5-32-546", "S-1-5-7", "S-1-5-4", "S-1-5-2", "S-1-2-0"];

/// Everything the remove step takes out: the fixed groups, and any domain
/// group the folder actually names.
pub fn remove_sids(aces: &[Ace]) -> Vec<String> {
    let mut v: Vec<String> = BROAD_REMOVE.iter().map(|s| s.to_string()).collect();
    for a in aces {
        if let Some(b) = broad_sid(&a.sid) {
            if !v.contains(&b) {
                v.push(b);
            }
        }
    }
    v.into_iter().map(|s| format!("*{s}")).collect()
}

/// The icacls commands, run one after the other, that close a folder: take
/// the broad groups out, and make sure SYSTEM, the Administrators and `keep`
/// have full control. The top folder only: no /T.
///
/// When a broad entry is inherited, inheritance is cut first, as a command of
/// its own, which copies the inherited entries as explicit ones (so a custom
/// administrator account keeps its access). It must be separate: icacls does
/// the removing before the inheritance change within one command, which
/// leaves the copied broad entry in place (seen on Windows 11).
pub fn tighten_steps(dir: &Path, keep: Option<&str>, aces: &[Ace]) -> Vec<Vec<String>> {
    let d = dir.to_string_lossy().into_owned();
    let mut steps = Vec::new();
    if aces.iter().any(|a| is_broad_sid(&a.sid) && a.flags.contains("ID")) {
        steps.push(vec![d.clone(), "/inheritance:d".into()]);
    }
    let mut v = vec![d, "/remove:g".into()];
    v.extend(remove_sids(aces));
    for who in ["*S-1-5-18", "*S-1-5-32-544"] {
        v.push("/grant".into());
        v.push(format!("{who}:(OI)(CI)F"));
    }
    if let Some(k) = keep {
        v.push("/grant".into());
        v.push(format!("{k}:(OI)(CI)F"));
    }
    steps.push(v);
    steps
}

/// Turning "private" off: inheritance from the parent again, top folder only.
/// The entries added for SYSTEM, the Administrators and the running account
/// stay: they are what recording needs when the parent is itself restricted.
pub fn release_args(dir: &Path) -> Vec<String> {
    vec![dir.to_string_lossy().into_owned(), "/inheritance:e".into()]
}

/// The signed-in account, when it is neither SYSTEM nor a computer account.
pub fn account_to_keep(domain: Option<&str>, user: Option<&str>) -> Option<String> {
    let user = user.filter(|u| !u.is_empty() && !u.ends_with('$') && !u.eq_ignore_ascii_case("SYSTEM"))?;
    // No separators or switch characters in a name we pass to icacls.
    if user.contains(['/', '\\', ':', '(', ')', ',', '*']) {
        return None;
    }
    match domain.filter(|d| !d.is_empty() && !d.contains(['/', '\\', ':', '(', ')', ',', '*'])) {
        Some(d) => Some(format!("{d}\\{user}")),
        None => Some(user.to_string()),
    }
}

/// icacls by full path (not found through a PATH a planted file could shadow).
pub fn icacls_exe() -> PathBuf {
    let root = std::env::var_os("SystemRoot").or_else(|| std::env::var_os("windir")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join("icacls.exe")
}

pub fn current_account() -> Option<String> {
    account_to_keep(std::env::var("USERDOMAIN").ok().as_deref(), std::env::var("USERNAME").ok().as_deref())
}

// ---------------------------------------------------------------------
// Which folders are never touched
// ---------------------------------------------------------------------

#[derive(PartialEq)]
struct Parsed {
    unc: bool,
    drive: bool,
    letter: Option<char>,
    parts: Vec<String>,
}

fn split_path(s: &str) -> Parsed {
    let mut s = s.trim().to_string();
    let mut unc = false;
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        unc = true;
        s = r.to_string();
    } else if let Some(r) = s.strip_prefix(r"\\?\") {
        s = r.to_string();
    } else if s.starts_with(r"\\") || s.starts_with("//") {
        unc = true;
    }
    let drive = s.as_bytes().get(1) == Some(&b':') && s.as_bytes()[0].is_ascii_alphabetic();
    let body = if drive { &s[2..] } else { &s[..] };
    let parts = body.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").map(|p| p.to_lowercase()).collect();
    let letter = if drive { Some(s.as_bytes()[0].to_ascii_lowercase() as char) } else { None };
    Parsed { unc, drive, letter, parts }
}

const PERSONAL: [&str; 10] = ["downloads", "desktop", "documents", "pictures", "videos", "music", "movies", "library", "appdata", "onedrive"];
const SYSTEM_TOP: [&str; 9] = ["windows", "program files", "program files (x86)", "programdata", "users", "system volume information", "$recycle.bin", "recovery", "documents and settings"];

/// Why this folder must not be changed, or None. `protected` are folders from
/// the environment (the profile, Program Files...): the folder itself, or one
/// that contains them, is refused. A wrong lockdown of a shared folder is
/// worse than none, so this errs towards refusing.
pub fn refuse_reason(path: &str, protected: &[String]) -> Option<String> {
    if path.trim().is_empty() {
        return Some("no folder".into());
    }
    let p = split_path(path);
    if p.unc {
        return Some("a network path".into());
    }
    if p.parts.is_empty() {
        return Some(CHOOSE_FOLDER_MSG.into());
    }
    let first = p.parts[0].as_str();
    let sys = "a system or personal folder, not one of AmberSight's";
    if p.drive {
        if p.parts.len() == 1 && SYSTEM_TOP.contains(&first) {
            return Some(sys.into());
        }
        if matches!(first, "windows" | "system volume information" | "$recycle.bin") {
            return Some(sys.into());
        }
    } else if p.parts.len() == 1 {
        // /mnt, /var, /home...: a top-level folder of the system.
        return Some(CHOOSE_FOLDER_MSG.into());
    }
    // C:\Users\name, C:\Users\name\Downloads, /home/name, /Users/name/Movies, /root.
    if matches!(first, "users" | "home") {
        if p.parts.len() <= 2 || (p.parts.len() == 3 && PERSONAL.contains(&p.parts[2].as_str())) {
            return Some(sys.into());
        }
    }
    if first == "root" && p.parts.len() == 1 {
        return Some(sys.into());
    }
    // Mac volumes: /Volumes/Name is a drive root.
    if !p.drive && first == "volumes" && p.parts.len() <= 2 {
        return Some(CHOOSE_FOLDER_MSG.into());
    }
    for e in protected {
        let q = split_path(e);
        if q.parts.is_empty() {
            continue;
        }
        if q.parts.starts_with(&p.parts) && q.letter == p.letter {
            return Some(sys.into());
        }
    }
    None
}

/// The folders of the running environment that must never be changed.
pub fn protected_from_env() -> Vec<String> {
    let mut v = Vec::new();
    for k in ["USERPROFILE", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData", "SystemRoot", "windir", "PUBLIC", "ALLUSERSPROFILE", "TEMP", "TMP"] {
        if let Some(x) = std::env::var_os(k).filter(|x| !x.is_empty()) {
            v.push(x.to_string_lossy().into_owned());
        }
    }
    // HOME only when it is a person's home: a service account's home is often
    // its data folder (/var/lib/ambersight), which must stay changeable.
    if let Some(h) = std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned()) {
        if home_is_personal(&h) {
            v.push(h);
        }
    }
    v.into_iter().map(|p| resolve(Path::new(&p)).map(|r| r.to_string_lossy().into_owned()).unwrap_or(p)).collect()
}

/// /home/name, /Users/name, /root and the like.
pub fn home_is_personal(home: &str) -> bool {
    let p = split_path(home);
    matches!(p.parts.first().map(String::as_str), Some("home" | "users" | "root")) || p.drive
}

/// Top-level names in a program folder that are programs of someone else:
/// the folder is shared (a tools folder), so it is not ours to close. Our own
/// names are exact: what the zip and the installer put there.
pub fn foreign_programs(names: &[String]) -> Vec<String> {
    const RUNNABLE: [&str; 10] = ["exe", "bat", "cmd", "ps1", "msi", "com", "vbs", "jar", "scr", "lnk"];
    const OURS: [&str; 5] = ["ambersight.exe", "install.cmd", "uninstall.exe", "ffmpeg.exe", "ffprobe.exe"];
    names
        .iter()
        .filter(|n| {
            let l = n.to_lowercase();
            let ext = l.rsplit_once('.').map(|x| x.1).unwrap_or("");
            RUNNABLE.contains(&ext) && !OURS.contains(&l.as_str())
        })
        .cloned()
        .collect()
}

/// The files the capture helper (run from this folder in a person's own
/// session) and the shortcuts need to read and run.
pub fn is_helper_file(name: &str) -> bool {
    let l = name.to_lowercase();
    matches!(l.as_str(), "ambersight.exe" | "ffmpeg.exe" | "ffprobe.exe" | "ffmpeg-path.txt") || l.ends_with(".dll") || l.ends_with(".ico")
}

/// The server's own ffmpeg files that the capture helper (an ordinary
/// account on this computer) may be given read-and-run on: `ffmpeg.exe`, and
/// `ffprobe.exe` when it is there, as they really are inside the tools
/// folder. Only when `chosen` (the ffmpeg the server runs) is the
/// tools folder's own `ffmpeg.exe`; a copy on the PATH or beside the program
/// needs nothing. Each must be a plain file (not a link or a folder) that
/// stays in the tools folder once resolved.
pub fn shareable_tool_files(tools: &Path, chosen: &Path) -> Vec<PathBuf> {
    let Ok(tools_r) = resolve(tools) else { return Vec::new() };
    let in_tools = |p: &Path, name: &str| -> Option<PathBuf> {
        let md = std::fs::symlink_metadata(p).ok()?;
        if !md.is_file() {
            return None;
        }
        let r = resolve(p).ok()?;
        let parent = r.parent()?;
        let same_dir = is_inside(parent, &tools_r) && is_inside(&tools_r, parent);
        (same_dir && r.file_name()?.to_string_lossy().eq_ignore_ascii_case(name)).then_some(r)
    };
    let Some(ff) = in_tools(&tools.join("ffmpeg.exe"), "ffmpeg.exe") else { return Vec::new() };
    let chosen_ok = resolve(chosen).map(|c| split_path(&c.to_string_lossy()) == split_path(&ff.to_string_lossy())).unwrap_or(false);
    if !chosen_ok {
        return Vec::new();
    }
    let mut v = vec![ff];
    v.extend(in_tools(&tools.join("ffprobe.exe"), "ffprobe.exe"));
    v
}

/// Of a save of the program folder's files, the helper files that Users
/// cannot read yet.
pub fn helper_files_needing_grant(files: &[(String, Vec<Ace>)]) -> Vec<String> {
    files
        .iter()
        .filter(|(n, aces)| is_helper_file(n) && !aces.iter().any(|a| a.kind == "A" && !a.flags.contains("IO") && matches!(broad_sid(&a.sid).as_deref(), Some("S-1-5-32-545" | "S-1-5-11" | "S-1-1-0"))))
        .map(|(n, _)| n.clone())
        .collect()
}

/// Whether `child` is `parent` or below it: by resolved names, case-insensitive.
pub fn is_inside(child: &Path, parent: &Path) -> bool {
    let (c, p) = (split_path(&child.to_string_lossy()), split_path(&parent.to_string_lossy()));
    c.letter == p.letter && !c.unc && c.parts.starts_with(&p.parts)
}

/// A recording folder added after this update: on when it is new or empty,
/// off when it already holds files (it may be in use for other things).
pub fn default_private_for_new(exists: bool, has_entries: bool) -> bool {
    !exists || !has_entries
}

// ---------------------------------------------------------------------
// Unix modes
// ---------------------------------------------------------------------

/// The mode a settings file or database should have, when it is more open.
pub fn private_file_mode(mode: u32) -> Option<u32> {
    if mode & 0o077 != 0 { Some(0o600) } else { None }
}

/// The mode of a folder with "others" shut out (and, when `strict`, group
/// write too), when it is more open.
pub fn private_dir_mode(mode: u32, strict: bool) -> Option<u32> {
    let mut new = mode & 0o7777 & !0o007;
    if strict {
        new &= !0o020;
    }
    if new != mode & 0o7777 { Some(new) } else { None }
}

/// "Off" on Unix: readable and enterable by everyone again.
pub fn released_dir_mode(mode: u32) -> u32 {
    (mode & 0o7777) | 0o055
}

// ---------------------------------------------------------------------
// The state of a folder, as shown in Settings
// ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Busy {
    Applying,
    Failed(String),
}

fn key(dir: &Path) -> String {
    dir.to_string_lossy().to_lowercase()
}

static BUSY: Mutex<Option<HashMap<String, Busy>>> = Mutex::new(None);

fn set_busy(dir: &Path, b: Option<Busy>) {
    let mut g = BUSY.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    match b {
        Some(b) => {
            m.insert(key(dir), b);
        }
        None => {
            m.remove(&key(dir));
        }
    }
}

fn busy_of(dir: &Path) -> Option<Busy> {
    BUSY.lock().unwrap().as_ref().and_then(|m| m.get(&key(dir)).cloned())
}

/// The folder as it really is: 8.3 short names, `..`, junctions and links
/// resolved, and no `\\?\` prefix. None when it cannot be resolved.
pub fn resolve(dir: &Path) -> Result<PathBuf, String> {
    let c = std::fs::canonicalize(dir).map_err(|e| format!("cannot find out where this folder really is ({e})"))?;
    let s = c.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(r) if !r.starts_with("UNC\\") => PathBuf::from(r),
        _ => c,
    })
}

/// A folder AmberSight may not close because a shared one would be hit: both
/// as given and as resolved.
fn refusal_for(dir: &Path, protected: &[String]) -> Result<PathBuf, String> {
    if let Some(r) = refuse_reason(&dir.to_string_lossy(), protected) {
        return Err(r);
    }
    let real = resolve(dir)?;
    match refuse_reason(&real.to_string_lossy(), protected) {
        Some(r) => Err(r),
        None => Ok(real),
    }
}

fn program_foreign(dir: &Path) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    let names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let foreign = foreign_programs(&names);
    if foreign.is_empty() {
        None
    } else {
        Some(format!("it also holds other programs ({}): it looks like a shared folder", foreign.iter().take(3).cloned().collect::<Vec<_>>().join(", ")))
    }
}

// A folder that stayed open after a pass is not tried again on every start.
fn marker(dir: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key(dir).hash(&mut h);
    std::env::temp_dir().join(format!("ambersight-lockdown-{:016x}.tried", h.finish()))
}

fn recent_failure(dir: &Path) -> Option<String> {
    let age = std::fs::metadata(marker(dir)).ok()?.modified().ok()?.elapsed().ok()?;
    (age < std::time::Duration::from_secs(24 * 3600)).then(|| "an attempt in the last day did not close it (see the log); it is tried again once a day".to_string())
}

fn mark_failed(dir: &Path) {
    // The name is predictable and this may run as SYSTEM: never write through
    // something planted there (a symlink). Remove, then create only if new.
    let m = marker(dir);
    let _ = std::fs::remove_file(&m);
    let _ = std::fs::OpenOptions::new().write(true).create_new(true).open(&m);
}

/// What Settings shows for a folder: (state, detail) with state one of
/// private, open, applying, failed, refused, skipped, unsupported, missing, na.
pub fn kind_state(dir: &Path, kind: Kind) -> (&'static str, String) {
    if cfg!(unix) && kind == Kind::Program {
        return ("na", String::new());
    }
    if let Some(b) = busy_of(dir) {
        return match b {
            Busy::Applying => ("applying", String::new()),
            Busy::Failed(e) => ("failed", e),
        };
    }
    // As an absolute path (a relative data_dir is relative to the settings), and the
    // name-only checks first: a network path answers at once, with no disk access.
    let abs = clean_path(dir);
    let dir = abs.as_path();
    let prot = protected_from_env();
    if let Some(r) = refuse_reason(&dir.to_string_lossy(), &prot) {
        return ("refused", r);
    }
    if !dir.is_dir() {
        return ("missing", "the folder is not there yet".into());
    }
    let real = match refusal_for(dir, &prot) {
        Ok(r) => r,
        Err(r) => return ("refused", r),
    };
    match sys::is_open(&real) {
        Ok(false) => ("private", String::new()),
        Err(e) => ("unsupported", e),
        Ok(true) => {
            if let Some(f) = recent_failure(&real) {
                return ("failed", f);
            }
            if kind == Kind::Program {
                if let Some(f) = program_foreign(&real) {
                    return ("skipped", f);
                }
            }
            ("open", String::new())
        }
    }
}

pub fn folder_state(dir: &Path) -> (&'static str, String) {
    kind_state(dir, Kind::Recording)
}

// ---------------------------------------------------------------------
// Doing it
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The folder the program runs from.
    Program,
    /// The folder with the database.
    Data,
    /// A recording folder with its switch on.
    Recording,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Not touched, and why.
    Skipped(String),
    /// Already closed: nothing was done.
    AlreadyPrivate,
    Tightened,
    Released,
}

/// Close one folder (top folder only). Cheap when it is already closed.
pub fn protect(dir: &Path, kind: Kind, keep: Option<&str>) -> anyhow::Result<Outcome> {
    protect_job(dir, kind, keep, false)
}

/// `honour_marker`: a pass that began on its own (service start) does not try
/// again within a day of one that failed; one a person asked for always runs.
pub fn protect_job(dir: &Path, kind: Kind, keep: Option<&str>, honour_marker: bool) -> anyhow::Result<Outcome> {
    let dir = clean_path(dir);
    let prot = protected_from_env();
    if let Some(r) = refuse_reason(&dir.to_string_lossy(), &prot) {
        return Ok(Outcome::Skipped(r));
    }
    if !dir.is_dir() {
        if kind == Kind::Data {
            std::fs::create_dir_all(&dir)?;
        } else {
            return Ok(Outcome::Skipped("the folder is not there".into()));
        }
    }
    if cfg!(unix) && kind == Kind::Program {
        return Ok(Outcome::Skipped("not needed here".into()));
    }
    // Where it really is (short names, .., junctions): the resolved folder is
    // the one checked and changed.
    let dir = match refusal_for(&dir, &prot) {
        Ok(d) => d,
        Err(r) => return Ok(Outcome::Skipped(r)),
    };
    if !sys::is_open(&dir).map_err(anyhow::Error::msg)? {
        // The program folder's files are replaced by updates: they keep their
        // own read-and-run entry for the capture helper.
        if kind == Kind::Program {
            if let Err(e) = sys::helper_files(&dir) {
                tracing::warn!("could not let ordinary accounts run the capture helper from {}: {e:#}", dir.display());
            }
        }
        return Ok(Outcome::AlreadyPrivate);
    }
    if honour_marker {
        if let Some(f) = recent_failure(&dir) {
            return Ok(Outcome::Skipped(f));
        }
    }
    if kind == Kind::Program {
        if let Some(f) = program_foreign(&dir) {
            return Ok(Outcome::Skipped(f));
        }
    }
    if let Err(e) = sys::tighten(&dir, kind == Kind::Recording, keep) {
        mark_failed(&dir);
        return Err(e);
    }
    let _ = std::fs::remove_file(marker(&dir));
    if kind == Kind::Program {
        if let Err(e) = sys::helper_files(&dir) {
            tracing::warn!("could not let ordinary accounts run the capture helper from {}: {e:#}", dir.display());
        }
    }
    Ok(Outcome::Tightened)
}

/// Put a recording folder back: inheritance again (Windows), readable (Unix).
pub fn release(dir: &Path) -> anyhow::Result<Outcome> {
    let dir = clean_path(dir);
    let dir = match refusal_for(&dir, &protected_from_env()) {
        Ok(d) => d,
        Err(r) => return Ok(Outcome::Skipped(r)),
    };
    if !dir.is_dir() {
        return Ok(Outcome::Skipped("the folder is not there".into()));
    }
    sys::release(&dir)?;
    Ok(Outcome::Released)
}

/// A path made absolute, without the `\\?\` prefix.
fn clean_path(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let s = abs.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(r) if !r.starts_with("UNC\\") => PathBuf::from(r),
        _ => abs,
    }
}

enum Job {
    Protect { dir: PathBuf, kind: Kind, keep: Option<String>, on_its_own: bool },
    Release { dir: PathBuf, only_if_private: bool },
}

struct Queue {
    jobs: VecDeque<Job>,
    running: bool,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue { jobs: VecDeque::new(), running: false });

/// Queue work for the one background thread (started when needed): it never
/// runs twice at once, and never holds up the caller.
fn submit(job: Job) {
    let dir = match &job {
        Job::Protect { dir, .. } | Job::Release { dir, .. } => dir.clone(),
    };
    set_busy(&dir, Some(Busy::Applying));
    let start = {
        let mut q = QUEUE.lock().unwrap();
        q.jobs.push_back(job);
        !std::mem::replace(&mut q.running, true)
    };
    if start {
        let spawned = std::thread::Builder::new().name("lockdown".into()).spawn(worker);
        if let Err(e) = spawned {
            tracing::warn!("could not start the folder protection thread: {e}");
            let mut q = QUEUE.lock().unwrap();
            q.running = false;
            for j in q.jobs.drain(..) {
                let (Job::Protect { dir, .. } | Job::Release { dir, .. }) = j;
                set_busy(&dir, Some(Busy::Failed(e.to_string())));
            }
        }
    }
}

fn worker() {
    loop {
        let job = {
            let mut q = QUEUE.lock().unwrap();
            match q.jobs.pop_front() {
                Some(j) => j,
                None => {
                    q.running = false;
                    return;
                }
            }
        };
        let t = std::time::Instant::now();
        let (dir, what, result) = match job {
            Job::Protect { dir, kind, keep, on_its_own } => {
                let r = protect_job(&dir, kind, keep.as_deref(), on_its_own);
                (dir, "protecting", r)
            }
            Job::Release { dir, only_if_private } => {
                // A folder that never had a value is only opened when it really is private.
                let r = if only_if_private && !closed_on_disk(&dir) { Ok(Outcome::Skipped("it is not private, nothing to open".into())) } else { release(&dir) };
                (dir, "releasing", r)
            }
        };
        match result {
            Ok(Outcome::Skipped(why)) => {
                tracing::warn!("{what} {}: left as it is, {why}", dir.display());
                set_busy(&dir, None);
            }
            Ok(Outcome::AlreadyPrivate) => {
                tracing::debug!("{} is already private", dir.display());
                set_busy(&dir, None);
            }
            Ok(o) => {
                tracing::info!("{} is now {} ({:.1} s)", dir.display(), if o == Outcome::Released { "open again" } else { "private to administrators and AmberSight" }, t.elapsed().as_secs_f32());
                set_busy(&dir, None);
            }
            Err(e) => {
                tracing::warn!("could not finish {what} {}: {e:#}. Recording is not affected.", dir.display());
                set_busy(&dir, Some(Busy::Failed(format!("{e:#}"))));
            }
        }
    }
}

/// At service start on Windows (and `run` on Unix): the program folder, the
/// data folder and recording folders with the switch on, in the background.
pub fn start_in_background(data_dir: &Path, recording: &[PathBuf]) {
    #[cfg(windows)]
    {
        let keep = current_account();
        if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            submit(Job::Protect { dir: d, kind: Kind::Program, keep: keep.clone(), on_its_own: true });
        }
        submit(Job::Protect { dir: data_dir.to_path_buf(), kind: Kind::Data, keep: keep.clone(), on_its_own: true });
        for r in recording {
            submit(Job::Protect { dir: r.clone(), kind: Kind::Recording, keep: keep.clone(), on_its_own: true });
        }
    }
    #[cfg(unix)]
    {
        let _ = data_dir;
        for r in recording {
            submit(Job::Protect { dir: r.clone(), kind: Kind::Recording, keep: None, on_its_own: true });
        }
    }
}

/// Windows service: let the capture helper, run by an ordinary account on
/// this computer, use the ffmpeg the server downloaded. The two files get a
/// read-and-run entry for Users (file level only; the tools folder and the
/// data folder stay closed), and `ffmpeg-path.txt` in the program folder says
/// where it is. When the server's ffmpeg is not one of those, the note is
/// removed. Runs on its own thread; a failure is only logged.
pub fn share_ffmpeg_in_background(data_dir: &Path, chosen: PathBuf) {
    #[cfg(windows)]
    {
        let tools = data_dir.join("tools");
        let _ = std::thread::Builder::new().name("ffmpeg-share".into()).spawn(move || {
            if let Err(e) = sys::share_ffmpeg(&tools, &chosen) {
                tracing::warn!("could not let ordinary accounts use the downloaded ffmpeg: {e:#}");
            }
        });
    }
    #[cfg(not(windows))]
    let _ = (data_dir, chosen);
}

static SEEN: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

/// A recording folder with its switch on that a camera has just made (it did
/// not exist when the switch was saved): closed once per run, in the background.
pub fn protect_new_folder(dir: &Path) {
    let first = SEEN.lock().unwrap().get_or_insert_with(Default::default).insert(key(dir));
    if first {
        submit(Job::Protect { dir: dir.to_path_buf(), kind: Kind::Recording, keep: current_account(), on_its_own: true });
    }
}

/// The switch of one recording folder changed in Settings.
pub fn apply_switch(dir: &Path, on: bool) {
    if on {
        submit(Job::Protect { dir: dir.to_path_buf(), kind: Kind::Recording, keep: current_account(), on_its_own: false });
    } else {
        submit(Job::Release { dir: dir.to_path_buf(), only_if_private: false });
    }
}

/// Whether the folder really is closed to other accounts now (read from the disk; the
/// "being applied" note of a queued job is not consulted).
fn closed_on_disk(dir: &Path) -> bool {
    let abs = clean_path(dir);
    match refusal_for(&abs, &protected_from_env()) {
        Ok(real) => matches!(sys::is_open(&real), Ok(false)),
        Err(_) => false,
    }
}

/// The switch of a folder that had no value was set to off: it is opened
/// again only if it really is private now (a folder AmberSight never touched
/// and that is open is left exactly as it is).
pub fn apply_switch_off_if_private(dir: &Path) {
    submit(Job::Release { dir: dir.to_path_buf(), only_if_private: true });
}

/// Setup and `service install` (Windows, elevated): the program folder at
/// once (small), the data folder at once when it is new or empty. A data
/// folder with recordings can take minutes: the service closes it in the
/// background when it starts.
pub fn at_install(program_dir: &Path, data_dir: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    // Nothing changes without elevation: a non-administrator who locked the
    // folder would lock themselves out and then fail to register the service.
    if cfg!(windows) && !elevated() {
        notes.push("not run as Administrator: the program and data folders were left as they are".to_string());
        return notes;
    }
    let mut run = |dir: &Path, kind: Kind| match protect(dir, kind, None) {
        Ok(Outcome::Tightened) => notes.push(format!("made {} private to administrators and AmberSight", dir.display())),
        Ok(Outcome::Skipped(why)) => notes.push(format!("left {} as it is: {why}", dir.display())),
        Ok(_) => {}
        Err(e) => notes.push(format!("warning: could not make {} private: {e:#}", dir.display())),
    };
    if cfg!(windows) {
        let empty = std::fs::read_dir(data_dir).map(|mut r| r.next().is_none()).unwrap_or(true);
        // `setup --here`: the data folder inside the program folder and not
        // empty would make this step take minutes; the service does it at start.
        let real = |p: &Path| resolve(p).unwrap_or_else(|_| clean_path(p));
        let inside = !empty && is_inside(&real(data_dir), &real(program_dir));
        if !inside {
            run(program_dir, Kind::Program);
        }
        if empty {
            run(data_dir, Kind::Data);
        }
    }
    notes
}

pub fn elevated() -> bool {
    #[cfg(windows)]
    {
        crate::setup::is_elevated()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Unix, at start and in setup: the settings file, the database and its
/// -wal and -shm files private to this account; the data folder shut to
/// others. Ownership is not changed; failures are warned about and ignored.
pub fn harden_unix(config: &Path, data_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let fix = |p: &Path, want: &dyn Fn(u32) -> Option<u32>| {
            let Ok(m) = std::fs::metadata(p) else { return };
            if let Some(new) = want(m.permissions().mode() & 0o7777) {
                if let Err(e) = std::fs::set_permissions(p, std::fs::Permissions::from_mode(new)) {
                    tracing::warn!("could not make {} private: {e}", p.display());
                }
            }
        };
        fix(config, &private_file_mode);
        let data = clean_path(data_dir);
        if refuse_reason(&data.to_string_lossy(), &protected_from_env()).is_none() {
            fix(&data, &|m| private_dir_mode(m, false));
        }
        harden_db(&data.join("ambersight.db"));
    }
    #[cfg(not(unix))]
    let _ = (config, data_dir);
}

/// Unix: shut a folder to others (750 from 755).
pub fn shut_dir_to_others(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(dir) {
            if let Some(new) = private_dir_mode(m.permissions().mode() & 0o7777, false) {
                if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(new)) {
                    tracing::warn!("could not make {} private: {e}", dir.display());
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// The database and its -wal and -shm files: this account only (SQLite gives
/// the side files the database's mode when it makes them; ones left by a crash
/// or an older version may be more open).
pub fn harden_db(db: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let mut name = db.as_os_str().to_owned();
            name.push(suffix);
            let p = PathBuf::from(name);
            let Ok(m) = std::fs::metadata(&p) else { continue };
            if let Some(new) = private_file_mode(m.permissions().mode() & 0o7777) {
                if let Err(e) = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(new)) {
                    tracing::warn!("could not make {} private: {e}", p.display());
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = db;
}

/// Said after "access denied" on a file only administrators (or the service
/// account) may open now.
pub fn access_hint(path: &Path) -> Option<String> {
    match std::fs::OpenOptions::new().read(true).open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Some(if cfg!(windows) {
            "this folder is private to administrators: run the command from an Administrator terminal (right-click Terminal or Command Prompt, choose \"Run as administrator\")".to_string()
        } else {
            "this file is private to the account AmberSight runs as: run the command as that account (for the package install: sudo -u ambersight ambersight ...) or as root".to_string()
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// The operating system's part
// ---------------------------------------------------------------------

#[cfg(windows)]
mod sys {
    use super::*;
    use std::os::windows::process::CommandExt;

    // CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS: never in the way of recording.
    const FLAGS: u32 = 0x0800_0000 | 0x0000_4000;

    fn icacls(args: &[String]) -> std::io::Result<std::process::Output> {
        std::process::Command::new(icacls_exe()).args(args).creation_flags(FLAGS).output()
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Whether the drive keeps ACLs: not network drives (the share's own
    /// rules apply) and not FAT/exFAT.
    pub fn acl_support(dir: &Path) -> Result<(), String> {
        use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW};
        let mut root = [0u16; 261];
        let d = wide(&dir.to_string_lossy());
        // SAFETY: valid NUL-terminated input and buffers of the sizes passed.
        unsafe {
            if GetVolumePathNameW(d.as_ptr(), root.as_mut_ptr(), root.len() as u32) == 0 {
                return Ok(());
            }
            if GetDriveTypeW(root.as_ptr()) == 4 {
                return Err(UNSUPPORTED_MSG.into());
            }
            let mut flags = 0u32;
            let ok = GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut flags, std::ptr::null_mut(), 0);
            // FILE_PERSISTENT_ACLS
            if ok != 0 && flags & 0x8 == 0 {
                return Err(UNSUPPORTED_MSG.into());
            }
        }
        Ok(())
    }

    pub fn read_aces(dir: &Path) -> Result<Vec<Ace>, String> {
        let tmp = std::env::temp_dir().join(format!("ambersight-acl-{}-{}.txt", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
        let out = icacls(&[dir.to_string_lossy().into_owned(), "/save".into(), tmp.to_string_lossy().into_owned(), "/c".into()]).map_err(|e| format!("could not run icacls: {e}"));
        let bytes = std::fs::read(&tmp);
        let _ = std::fs::remove_file(&tmp);
        let out = out?;
        let bytes = bytes.map_err(|_| format!("could not read the folder's permissions: {}", String::from_utf8_lossy(&out.stdout).trim()))?;
        let aces = parse_aces(&decode_icacls_save(&bytes));
        if aces.is_empty() {
            return Err("could not read the folder's permissions".into());
        }
        Ok(aces)
    }

    pub fn is_open(dir: &Path) -> Result<bool, String> {
        acl_support(dir)?;
        Ok(acl_is_open(&read_aces(dir)?))
    }

    pub fn tighten(dir: &Path, recording: bool, keep: Option<&str>) -> anyhow::Result<()> {
        let _ = recording;
        let before = read_aces(dir).map_err(anyhow::Error::msg)?;
        for step in tighten_steps(dir, keep, &before) {
            run(&step)?;
        }
        // Whoever owns a folder can give themselves access again: Administrators own it. (Needs
        // an elevated process; failing is only logged.)
        match icacls(&[dir.to_string_lossy().into_owned(), "/setowner".into(), "*S-1-5-32-544".into()]) {
            Ok(o) if o.status.success() => {}
            Ok(o) => tracing::info!("could not make Administrators the owner of {}: {}", dir.display(), String::from_utf8_lossy(&o.stdout).trim()),
            Err(e) => tracing::info!("could not make Administrators the owner of {}: {e}", dir.display()),
        }
        let aces = read_aces(dir).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(!acl_is_open(&aces), "the folder is still open to other accounts after the change");
        anyhow::ensure!(acl_keeps_service(&aces), "SYSTEM or Administrators lost access after the change; check the folder's permissions");
        Ok(())
    }

    /// Ordinary accounts may read and run the program, its libraries and ffmpeg
    /// (the capture helper runs from here in a person's own session). These
    /// files only; one save of the folder's files says what is missing.
    pub fn helper_files(dir: &Path) -> anyhow::Result<()> {
        grant_users(dir, &|_| true)
    }

    /// Of the helper files in `dir` that `only` allows, give Users read (and
    /// run, except for text files) on those that lack it. Files only.
    fn grant_users(dir: &Path, only: &dyn Fn(&str) -> bool) -> anyhow::Result<()> {
        // (A name nobody can guess, made by us first: the temporary folder is writable by every account.)
        let tmp = std::env::temp_dir().join(format!("ambersight-files-{}-{}.txt", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
        std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        let pattern = dir.join("*").to_string_lossy().into_owned();
        icacls(&[pattern, "/save".into(), tmp.to_string_lossy().into_owned(), "/c".into()])?;
        let bytes = std::fs::read(&tmp);
        let _ = std::fs::remove_file(&tmp);
        let pairs = parse_save_pairs(&decode_icacls_save(&bytes?));
        // The program first; one failure does not stop the rest. Only real files (a folder
        // that happens to be called x.dll is not one).
        let mut names: Vec<String> = helper_files_needing_grant(&pairs)
            .into_iter()
            .filter(|n| only(n) && std::fs::symlink_metadata(dir.join(n)).map(|m| m.is_file()).unwrap_or(false))
            .collect();
        names.sort_by_key(|n| !n.eq_ignore_ascii_case("ambersight.exe"));
        let mut first_error = None;
        for name in names {
            let right = if name.to_lowercase().ends_with(".txt") { "*S-1-5-32-545:(R)" } else { "*S-1-5-32-545:(RX)" };
            if let Err(e) = run(&[dir.join(&name).to_string_lossy().into_owned(), "/grant".into(), right.into()]) {
                tracing::warn!("could not let ordinary accounts read {name}: {e:#}");
                first_error.get_or_insert(e);
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// See `share_ffmpeg_in_background`.
    pub fn share_ffmpeg(tools: &Path, chosen: &Path) -> anyhow::Result<()> {
        let note = match std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            Some(d) => d.join(crate::ffmpeg_bin::HINT_FILE),
            None => return Ok(()),
        };
        let files = shareable_tool_files(tools, chosen);
        let Some(ffmpeg) = files.first() else {
            // Not ours to share any more: the note must not point at it.
            if std::fs::symlink_metadata(&note).is_ok() {
                std::fs::remove_file(&note)?;
            }
            return Ok(());
        };
        let names: Vec<String> = files.iter().filter_map(|f| f.file_name().map(|n| n.to_string_lossy().to_lowercase())).collect();
        let tools_r = ffmpeg.parent().unwrap_or(tools);
        grant_users(tools_r, &|n| names.contains(&n.to_lowercase()))?;
        let text = format!("{}\r\n", ffmpeg.display());
        if std::fs::read_to_string(&note).ok().as_deref() != Some(text.as_str()) {
            // (Written by the service, in a folder only administrators can change.)
            std::fs::write(&note, text)?;
        }
        if let Some(d) = note.parent() {
            grant_users(d, &|n| n.eq_ignore_ascii_case(crate::ffmpeg_bin::HINT_FILE))?;
        }
        Ok(())
    }

    pub fn release(dir: &Path) -> anyhow::Result<()> {
        acl_support(dir).map_err(anyhow::Error::msg)?;
        run(&release_args(dir))
    }

    fn run(args: &[String]) -> anyhow::Result<()> {
        let out = icacls(args)?;
        anyhow::ensure!(out.status.success(), "icacls failed: {}", String::from_utf8_lossy(&out.stdout).trim());
        Ok(())
    }
}

#[cfg(unix)]
mod sys {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(dir: &Path) -> std::io::Result<u32> {
        Ok(std::fs::metadata(dir)?.permissions().mode() & 0o7777)
    }

    pub fn is_open(dir: &Path) -> Result<bool, String> {
        Ok(mode(dir).map_err(|e| e.to_string())? & 0o007 != 0)
    }

    /// Change the mode, then look: FAT, exFAT and some network mounts accept
    /// the call and change nothing.
    fn set(dir: &Path, new: u32) -> anyhow::Result<()> {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(new)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(1) {
                anyhow::anyhow!("AmberSight does not own this folder, so it cannot change who may open it ({UNSUPPORTED_MSG} unless it is owned by the account running AmberSight)")
            } else {
                anyhow::anyhow!("{e}")
            }
        })?;
        anyhow::ensure!(mode(dir)? == new, "{UNSUPPORTED_MSG}");
        Ok(())
    }

    pub fn tighten(dir: &Path, recording: bool, _keep: Option<&str>) -> anyhow::Result<()> {
        match private_dir_mode(mode(dir)?, recording) {
            Some(new) => set(dir, new),
            None => Ok(()),
        }
    }

    pub fn helper_files(_: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn release(dir: &Path) -> anyhow::Result<()> {
        let m = mode(dir)?;
        let new = released_dir_mode(m);
        if new == m { Ok(()) } else { set(dir, new) }
    }
}

#[cfg(not(any(windows, unix)))]
mod sys {
    use super::*;
    pub fn is_open(_: &Path) -> Result<bool, String> {
        Err(UNSUPPORTED_MSG.into())
    }
    pub fn tighten(_: &Path, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        anyhow::bail!(UNSUPPORTED_MSG)
    }
    pub fn helper_files(_: &Path) -> anyhow::Result<()> {
        Ok(())
    }
    pub fn release(_: &Path) -> anyhow::Result<()> {
        anyhow::bail!(UNSUPPORTED_MSG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: &str = "AmberSight\r\nD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICIIO;0x1301bf;;;AU)(A;OICI;0x1200a9;;;BU)";
    const TIGHT: &str = "AmberSight\nD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;S-1-5-21-1-2-3-1001)";

    #[cfg(unix)]
    #[test]
    fn a_folder_closed_by_hand_reads_as_closed_even_while_a_job_is_queued() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("ambersight-closed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        set_busy(&d, Some(Busy::Applying));
        assert!(closed_on_disk(&d));
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!closed_on_disk(&d));
        set_busy(&d, None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn open_means_a_broad_group_has_an_entry() {
        assert!(acl_is_open(&parse_aces(OPEN)));
        assert!(!acl_is_open(&parse_aces(TIGHT)));
        // Read-only is still more than nothing.
        assert!(acl_is_open(&parse_aces("x\nD:PAI(A;OICI;FA;;;SY)(A;;0x1200a9;;;BU)")));
        assert!(acl_is_open(&parse_aces("x\nO:BAG:SYD:AI(A;;FA;;;S-1-1-0)")));
        // A deny is not a grant.
        assert!(!acl_is_open(&parse_aces("x\nD:PAI(A;OICI;FA;;;SY)(D;;FA;;;WD)")));
        // Nothing readable: not claimed open (the caller treats it as an error).
        assert!(parse_aces("").is_empty());
        assert!(parse_aces("garbage").is_empty());
    }

    #[test]
    fn sacl_and_owner_are_not_entries() {
        let a = parse_aces("x\nO:BAG:SYD:PAI(A;;FA;;;SY)S:AI(AU;SA;FA;;;WD)");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].sid, "SY");
    }

    #[test]
    fn only_the_servers_own_ffmpeg_files_in_the_tools_folder_are_shared() {
        let root = std::env::temp_dir().join(format!("ambersight-share-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (tools, other) = (root.join("tools"), root.join("other"));
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let (ff, fp) = (tools.join("ffmpeg.exe"), tools.join("ffprobe.exe"));
        std::fs::write(&ff, b"x").unwrap();
        std::fs::write(tools.join("ffplay.exe"), b"x").unwrap();
        std::fs::write(other.join("ffmpeg.exe"), b"x").unwrap();
        let real = |p: &Path| resolve(p).unwrap();
        // ffmpeg alone; ffprobe joins it when it is a file; never another program.
        assert_eq!(shareable_tool_files(&tools, &ff), vec![real(&ff)]);
        std::fs::write(&fp, b"x").unwrap();
        assert_eq!(shareable_tool_files(&tools, &ff), vec![real(&ff), real(&fp)]);
        // The server runs some other ffmpeg: nothing shared.
        assert!(shareable_tool_files(&tools, &other.join("ffmpeg.exe")).is_empty());
        assert!(shareable_tool_files(&tools, &root.join("missing.exe")).is_empty());
        // A folder called ffprobe.exe is not a file to share.
        std::fs::remove_file(&fp).unwrap();
        std::fs::create_dir(&fp).unwrap();
        assert_eq!(shareable_tool_files(&tools, &ff), vec![real(&ff)]);
        // No ffmpeg there, or the tools folder is gone.
        std::fs::remove_file(&ff).unwrap();
        assert!(shareable_tool_files(&tools, &ff).is_empty());
        assert!(shareable_tool_files(&root.join("nope"), &ff).is_empty());
        #[cfg(unix)]
        {
            // A link to another program is not shared.
            std::os::unix::fs::symlink(other.join("ffmpeg.exe"), &ff).unwrap();
            assert!(shareable_tool_files(&tools, &ff).is_empty());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_ffmpeg_note_is_a_helper_file() {
        assert!(is_helper_file("ffmpeg-path.txt") && is_helper_file("FFmpeg-Path.TXT"));
        assert!(!is_helper_file("notes.txt"));
        assert_eq!(foreign_programs(&["ffmpeg-path.txt".to_string()]), Vec::<String>::new());
    }

    #[test]
    fn service_kept_check() {
        assert!(acl_keeps_service(&parse_aces(TIGHT)));
        assert!(!acl_keeps_service(&parse_aces("x\nD:P(A;;FA;;;SY)")));
        assert!(!acl_keeps_service(&parse_aces("x\nD:P(A;OICIIO;FA;;;SY)(A;;FA;;;BA)")));
    }

    fn utf16(text: &str, bom: bool) -> Vec<u8> {
        let mut b = if bom { vec![0xFF, 0xFE] } else { vec![] };
        for u in text.encode_utf16() {
            b.extend(u.to_le_bytes());
        }
        b
    }

    #[test]
    fn decodes_utf16_saves_with_and_without_a_bom() {
        let text = "AmberSight\r\nD:PAI(A;;FA;;;WD)";
        assert!(acl_is_open(&parse_aces(&decode_icacls_save(&utf16(text, true)))));
        // Current Windows: no BOM, and names that are not Latin (an odd first byte pattern).
        let cyr = "\u{41a}\u{430}\u{43c}\u{435}\u{440}\u{44b}\r\nD:PAI(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)";
        assert!(acl_is_open(&parse_aces(&decode_icacls_save(&utf16(cyr, false)))));
        let pairs = parse_save_pairs(&decode_icacls_save(&utf16("\u{424}\u{430}\u{439}\u{43b}.dll\r\nD:P(A;;FA;;;SY)\r\nambersight.exe\r\nD:P(A;;0x1200a9;;;BU)", false)));
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "\u{424}\u{430}\u{439}\u{43b}.dll");
        // A plain narrow file still reads.
        assert!(acl_is_open(&parse_aces(&decode_icacls_save(text.as_bytes()))));
    }

    #[test]
    fn helper_files_are_found_and_only_missing_ones_listed() {
        let pairs = parse_save_pairs("ambersight.exe\nD:P(A;;FA;;;SY)\nDirectML.dll\nD:P(A;;FA;;;SY)(A;;0x1200a9;;;BU)\nconfig.toml\nD:P(A;;FA;;;SY)\nffmpeg.exe\nD:P(A;;FA;;;SY)");
        assert_eq!(helper_files_needing_grant(&pairs), vec!["ambersight.exe".to_string(), "ffmpeg.exe".to_string()]);
        assert!(is_helper_file("VCRUNTIME140.DLL") && is_helper_file("ambersight.ico") && !is_helper_file("config.toml") && !is_helper_file("install.cmd"));
    }

    #[test]
    fn open_test_and_remove_list_agree() {
        for sid in ["WD", "AU", "BU", "BG", "AN", "IU", "NU", "S-1-2-0", "S-1-5-21-1-2-3-513", "S-1-5-21-1-2-3-514"] {
            let a = parse_aces(&format!("x\nD:P(A;;FA;;;{sid})"));
            assert!(acl_is_open(&a), "{sid}");
            let rm = remove_sids(&a);
            let want = format!("*{}", broad_sid(sid).unwrap());
            assert!(rm.contains(&want), "{sid} must be removed: {rm:?}");
        }
        assert!(!is_broad_sid("S-1-5-21-1-2-3-1001") && !is_broad_sid("SY") && !is_broad_sid("BA"));
    }

    #[test]
    fn home_of_a_service_account_is_not_protected() {
        assert!(home_is_personal("/home/bob") && home_is_personal("/Users/bob") && home_is_personal("/root"));
        assert!(!home_is_personal("/var/lib/ambersight"));
    }

    #[test]
    fn tighten_steps_never_recurse_or_drop_existing_entries() {
        let dir = Path::new(r"C:\AmberSight");
        // A broad entry inherited from the parent: cut inheritance first, as its own command.
        let inherited = parse_aces("x\nD:PAI(A;OICIID;FA;;;SY)(A;OICIID;0x1301bf;;;AU)");
        let steps = tighten_steps(dir, Some(r"PC\bob"), &inherited);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], vec![r"C:\AmberSight".to_string(), "/inheritance:d".to_string()]);
        let a = &steps[1];
        assert_eq!(a[0], r"C:\AmberSight");
        for st in &steps {
            assert!(!st.iter().any(|x| x.eq_ignore_ascii_case("/t") || x == "/inheritance:r"));
        }
        for sid in ["*S-1-1-0", "*S-1-5-11", "*S-1-5-32-545"] {
            assert!(a.contains(&sid.to_string()));
        }
        assert!(a.contains(&"*S-1-5-18:(OI)(CI)F".to_string()));
        assert!(a.contains(&"*S-1-5-32-544:(OI)(CI)F".to_string()));
        assert!(a.contains(&r"PC\bob:(OI)(CI)F".to_string()));
        // Only explicit broad entries: no inheritance change (one pass over the folder).
        let explicit = parse_aces("x\nD:PAI(A;OICI;FA;;;SY)(A;OICI;0x1301bf;;;AU)");
        let steps = tighten_steps(dir, None, &explicit);
        assert_eq!(steps.len(), 1);
        assert!(!steps[0].iter().any(|x| x.contains("bob") || x.contains("inheritance")));
        let r = release_args(Path::new(r"D:\Cams"));
        assert_eq!(r, vec![r"D:\Cams".to_string(), "/inheritance:e".to_string()]);
    }

    #[test]
    fn account_to_keep_skips_system() {
        assert_eq!(account_to_keep(Some("PC"), Some("bob")), Some(r"PC\bob".into()));
        assert_eq!(account_to_keep(None, Some("bob")), Some("bob".into()));
        assert_eq!(account_to_keep(Some("WORKGROUP"), Some("PC$")), None);
        assert_eq!(account_to_keep(Some("NT AUTHORITY"), Some("SYSTEM")), None);
        assert_eq!(account_to_keep(Some("PC"), None), None);
        assert_eq!(account_to_keep(Some("PC"), Some("a:b")), None);
    }

    fn no_env() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn refuses_roots_profiles_and_system_folders() {
        for p in [
            r"C:\", "D:", r"C:\Users", r"C:\Users\bob", r"C:\Users\bob\Downloads", r"c:\users\BOB\desktop\", r"C:\Users\Public",
            r"C:\Program Files", r"C:\Program Files (x86)", r"C:\Windows", r"C:\Windows\System32", r"C:\ProgramData",
            r"\\server\share\cams", r"\\?\UNC\server\share", "", "/", "/mnt", "/var", "/home", "/home/bob", "/Users/bob/Movies", "/root", "/Volumes/Cams",
        ] {
            assert!(refuse_reason(p, &no_env()).is_some(), "{p} should be refused");
        }
        assert_eq!(refuse_reason(r"E:\", &no_env()).as_deref(), Some(CHOOSE_FOLDER_MSG));
    }

    #[test]
    fn allows_ordinary_folders() {
        for p in [
            r"C:\AmberSight", r"D:\AmberSight\data", r"C:\Program Files\AmberSight NVR", r"C:\ProgramData\AmberSight\data",
            r"E:\Cameras", r"\\?\D:\Cams", r"C:\Users\bob\Cameras", "/mnt/nas/cameras", "/var/lib/ambersight", "/opt/ambersight", "/home/bob/amber", "/srv/cams",
        ] {
            assert_eq!(refuse_reason(p, &no_env()), None, "{p} should be allowed");
        }
    }

    #[test]
    fn refuses_what_contains_an_environment_folder() {
        let env = vec![r"C:\Users\bob".to_string(), r"C:\Users\bob\AppData\Local\Temp".to_string()];
        assert!(refuse_reason(r"C:\Users\bob\AppData", &env).is_some());
        assert!(refuse_reason(r"C:\Users\bob\AppData\Local", &env).is_some());
        assert!(refuse_reason(r"C:\Users\bob\Cameras", &env).is_none());
        // A folder with the same name on another drive is not it.
        assert!(refuse_reason(r"D:\Users\bob\Cameras", &["C:\\Users\\bob\\Cameras\\x".to_string()]).is_none());
    }

    #[test]
    fn shared_program_folders_are_noticed() {
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // What the zip, the installer and an update leave: install.cmd is ours.
        assert!(foreign_programs(&n(&["ambersight.exe", "install.cmd", "ambersight.ico", "onnxruntime.dll", "uninstall.exe", "ffmpeg.exe", "config.toml", "README.txt", "ambersight.exe.old"])).is_empty());
        assert_eq!(foreign_programs(&n(&["ambersight-other.exe"])), n(&["ambersight-other.exe"]));
        assert_eq!(foreign_programs(&n(&["ambersight.exe", "putty.exe", "setup.msi"])), n(&["putty.exe", "setup.msi"]));
        assert_eq!(foreign_programs(&n(&["Run.BAT"])), n(&["Run.BAT"]));
    }

    #[test]
    fn inside_is_by_name_and_ignores_case() {
        assert!(is_inside(Path::new(r"C:\AmberSight\Data"), Path::new(r"c:\ambersight")));
        assert!(is_inside(Path::new(r"\\?\C:\AmberSight"), Path::new(r"C:\AmberSight\")));
        assert!(!is_inside(Path::new(r"C:\AmberSight2"), Path::new(r"C:\AmberSight")));
        assert!(!is_inside(Path::new(r"D:\AmberSight\x"), Path::new(r"C:\AmberSight")));
    }

    #[test]
    fn new_folder_defaults() {
        assert!(default_private_for_new(false, false));
        assert!(default_private_for_new(true, false));
        assert!(!default_private_for_new(true, true));
    }

    #[test]
    fn unix_modes() {
        assert_eq!(private_file_mode(0o644), Some(0o600));
        assert_eq!(private_file_mode(0o640), Some(0o600));
        assert_eq!(private_file_mode(0o600), None);
        assert_eq!(private_dir_mode(0o755, false), Some(0o750));
        assert_eq!(private_dir_mode(0o750, false), None);
        assert_eq!(private_dir_mode(0o775, false), Some(0o770));
        assert_eq!(private_dir_mode(0o775, true), Some(0o750));
        assert_eq!(private_dir_mode(0o700, true), None);
        assert_eq!(released_dir_mode(0o750), 0o755);
        assert_eq!(released_dir_mode(0o700), 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn the_daily_marker_binds_service_passes_only_and_is_not_followed_through_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("amb-marker-test-{}", std::process::id()));
        let dir = base.join("cams");
        std::fs::create_dir_all(&dir).unwrap();
        // The marker goes by the resolved path (macOS: the temp folder is behind a link).
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        // A planted link at the marker's name: not written through.
        let victim = base.join("victim.txt");
        std::fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, marker(&dir)).unwrap();
        mark_failed(&dir);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert!(recent_failure(&dir).is_some());
        // A pass that began on its own waits; one a person asked for runs.
        assert!(matches!(protect_job(&dir, Kind::Recording, None, true).unwrap(), Outcome::Skipped(_)));
        assert_eq!(protect_job(&dir, Kind::Recording, None, false).unwrap(), Outcome::Tightened);
        let _ = std::fs::remove_file(marker(&dir));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert_eq!(kind_state(&dir, Kind::Data).0, "private");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_round_trip_in_a_throwaway_folder() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("amb-lock-test-{}", std::process::id()));
        let dir = base.join("cams");
        std::fs::create_dir_all(&dir).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(folder_state(&dir).0, "open");
        assert_eq!(protect(&dir, Kind::Recording, None).unwrap(), Outcome::Tightened);
        assert_eq!(mode(&dir), 0o750);
        assert_eq!(folder_state(&dir).0, "private");
        assert_eq!(protect(&dir, Kind::Recording, None).unwrap(), Outcome::AlreadyPrivate);
        assert_eq!(release(&dir).unwrap(), Outcome::Released);
        assert_eq!(mode(&dir), 0o755);
        // The settings file and database, including -wal and -shm.
        let cfg = base.join("config.toml");
        std::fs::write(&cfg, "x").unwrap();
        for n in ["ambersight.db", "ambersight.db-wal", "ambersight.db-shm"] {
            std::fs::write(dir.join(n), "x").unwrap();
            std::fs::set_permissions(dir.join(n), std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        harden_unix(&cfg, &dir);
        assert_eq!(mode(&cfg), 0o600);
        for n in ["ambersight.db", "ambersight.db-wal", "ambersight.db-shm"] {
            assert_eq!(mode(&dir.join(n)), 0o600);
        }
        assert_eq!(mode(&dir), 0o750);
        // A folder under a top-level system folder name is refused, not changed.
        assert!(matches!(protect(Path::new("/tmp"), Kind::Data, None).unwrap(), Outcome::Skipped(_)));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Windows, in a throwaway folder: broad entry in, closed, round trip.
    /// Needs only a folder the account owns (no elevation).
    #[cfg(windows)]
    #[test]
    fn windows_round_trip_in_a_throwaway_folder() {
        if current_account().is_none() || std::env::var_os("AMBERSIGHT_SKIP_ACL_TESTS").is_some() {
            eprintln!("skipping: no account name to keep (USERNAME/USERDOMAIN), or AMBERSIGHT_SKIP_ACL_TESTS is set");
            return;
        }
        let base = std::env::temp_dir().join(format!("amb-lock-test-{}", std::process::id()));
        let dir = base.join("cams");
        let ic = |a: &[&str]| {
            let o = std::process::Command::new(icacls_exe()).args(a).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        };
        // The broad entry is inherited from the parent, as under a drive root.
        std::fs::create_dir_all(&base).unwrap();
        ic(&[&base.to_string_lossy(), "/grant", "*S-1-5-11:(OI)(CI)M"]);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("f.txt"), "x").unwrap();
        assert_eq!(folder_state(&dir).0, "open");
        let keep = current_account();
        assert!(keep.is_some(), "the test account is an ordinary user");
        assert_eq!(protect(&dir, Kind::Recording, keep.as_deref()).unwrap(), Outcome::Tightened);
        assert_eq!(folder_state(&dir).0, "private");
        // A file below got the change through inheritance.
        assert!(!acl_is_open(&sys::read_aces(&dir.join("sub").join("f.txt")).unwrap()));
        // The inherited entry for this account was copied: it can still use the folder, and files below.
        assert!(std::fs::read_to_string(dir.join("sub").join("f.txt")).is_ok());
        std::fs::write(dir.join("new.txt"), "y").unwrap();
        assert_eq!(protect(&dir, Kind::Recording, keep.as_deref()).unwrap(), Outcome::AlreadyPrivate);
        // Off: inheriting from the parent again, so the parent's broad entry is back.
        assert_eq!(release(&dir).unwrap(), Outcome::Released);
        assert_eq!(folder_state(&dir).0, "open");
        assert!(std::fs::read_to_string(dir.join("sub").join("f.txt")).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Windows, throwaway folders, no elevation needed: the program folder with
    /// the capture helper's files, a Cyrillic name, a junction, a short name,
    /// and the refusal to change anything without elevation.
    #[cfg(windows)]
    #[test]
    fn windows_program_folder_helper_files_names_and_links() {
        if current_account().is_none() || std::env::var_os("AMBERSIGHT_SKIP_ACL_TESTS").is_some() {
            eprintln!("skipping: no account name to keep (USERNAME/USERDOMAIN), or AMBERSIGHT_SKIP_ACL_TESTS is set");
            return;
        }
        let ic = |a: &[&str]| {
            let o = std::process::Command::new(icacls_exe()).args(a).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        };
        let base = std::env::temp_dir().join(format!("amb-lock2-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        ic(&[&base.to_string_lossy(), "/grant", "*S-1-5-11:(OI)(CI)M"]);
        let keep = current_account();

        // The official zip's files, in a folder with a Cyrillic name: counts as ours, closes, and
        // the helper's files stay readable and runnable by Users, nothing else.
        let prog = base.join("\u{41a}\u{430}\u{43c}\u{435}\u{440}\u{44b}");
        std::fs::create_dir_all(&prog).unwrap();
        for f in ["ambersight.exe", "install.cmd", "DirectML.dll", "ffmpeg.exe", "config.example.toml", "README.txt"] {
            std::fs::write(prog.join(f), "x").unwrap();
        }
        assert_eq!(kind_state(&prog, Kind::Program).0, "open");
        assert_eq!(protect(&prog, Kind::Program, keep.as_deref()).unwrap(), Outcome::Tightened);
        assert_eq!(kind_state(&prog, Kind::Program).0, "private");
        let has_users = |f: &str| sys::read_aces(&prog.join(f)).unwrap().iter().any(|a| matches!(a.sid.as_str(), "BU" | "S-1-5-32-545"));
        for f in ["ambersight.exe", "DirectML.dll", "ffmpeg.exe"] {
            assert!(has_users(f), "{f} must be readable by Users");
        }
        for f in ["install.cmd", "config.example.toml"] {
            assert!(!has_users(f), "{f} must not be");
        }
        // A file replaced by an update loses its entry; the next pass on the closed folder gives it back and nothing else.
        std::fs::remove_file(prog.join("ambersight.exe")).unwrap();
        std::fs::write(prog.join("ambersight.exe"), "new").unwrap();
        assert!(!has_users("ambersight.exe"));
        assert_eq!(protect(&prog, Kind::Program, keep.as_deref()).unwrap(), Outcome::AlreadyPrivate);
        assert!(has_users("ambersight.exe"));

        // A foreign program makes it a shared folder, and it is said (not silently skipped).
        let shared = base.join("tools");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("ambersight.exe"), "x").unwrap();
        std::fs::write(shared.join("putty.exe"), "x").unwrap();
        assert!(matches!(protect(&shared, Kind::Program, keep.as_deref()).unwrap(), Outcome::Skipped(_)));
        assert_eq!(kind_state(&shared, Kind::Program).0, "skipped");

        // A junction: the folder it points to is the one changed and reported.
        let target = base.join("realcams");
        std::fs::create_dir_all(&target).unwrap();
        let link = base.join("linkcams");
        let st = std::process::Command::new("cmd").args(["/c", "mklink", "/J", &link.to_string_lossy(), &target.to_string_lossy()]).output().unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stdout));
        assert_eq!(protect(&link, Kind::Recording, keep.as_deref()).unwrap(), Outcome::Tightened);
        assert_eq!(folder_state(&target).0, "private");
        assert_eq!(folder_state(&link).0, "private");

        // A short (8.3) name resolves to the real one; `..` is followed; a link to a drive root is refused.
        let long = base.join("a long folder name");
        std::fs::create_dir_all(&long).unwrap();
        let out = std::process::Command::new("cmd").args(["/c", "for", "%I", "in", &format!("\"{}\"", long.display()), "do", "@echo", "%~sI"]).output().unwrap();
        let short = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !short.is_empty() && !short.eq_ignore_ascii_case(&long.to_string_lossy()) {
            assert_eq!(resolve(Path::new(&short)).unwrap().to_string_lossy().to_lowercase(), resolve(&long).unwrap().to_string_lossy().to_lowercase());
        }
        let mut dotdot = long.clone();
        for _ in 0..12 {
            dotdot.push("..");
        }
        assert!(matches!(protect(&dotdot, Kind::Data, None).unwrap(), Outcome::Skipped(_)), "a path that resolves to a drive root is refused");
        let rootlink = base.join("rootlink");
        let _ = std::process::Command::new("cmd").args(["/c", "mklink", "/J", &rootlink.to_string_lossy(), r"C:\"]).output();
        if rootlink.exists() {
            assert!(matches!(protect(&rootlink, Kind::Recording, None).unwrap(), Outcome::Skipped(_)));
            let _ = std::process::Command::new("cmd").args(["/c", "rmdir", &rootlink.to_string_lossy()]).output();
        }

        // Not elevated: nothing at all changes.
        if !elevated() {
            let open = base.join("open");
            std::fs::create_dir_all(&open).unwrap();
            let notes = at_install(&open, &open.join("data"));
            assert!(notes[0].contains("not run as Administrator"), "{notes:?}");
            assert_eq!(folder_state(&open).0, "open");
        }
        let _ = std::process::Command::new("cmd").args(["/c", "rmdir", &link.to_string_lossy()]).output();
        let _ = std::fs::remove_dir_all(&base);
    }
}
