//! Language packs for the web app (static/js/i18n.js). A pack is `<data>/lang/<code>.json`:
//! `{"lang": "de", "messages": {"key": ["translation", "fingerprint of the English"]}}`.
//! Served to anyone, signed in or not (the sign-in page is translated too); packs hold no secrets.
//!
//! Trust: the web app puts some translated text into the page as HTML, so a tampered pack could
//! inject code. Nothing is served or installed unless its SHA-256 is in `PINS`, the table build.rs
//! fixes into the program from `lang/<code>.json`. A pack therefore works only with the release it
//! was built for; a fix to one language ships with the next release, never on its own (a pack that
//! could be replaced between releases would need a second trust root, so there is none).
//! A download is also checked against the release's signed SHA256SUMS, as the add-ons are.
//! The URL can only name a code from `CODES`, never a path.

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::engine::Engine;
use crate::web::{Caller, IsAdmin};

include!(concat!(env!("OUT_DIR"), "/lang_pins.rs"));

/// Codes the server will ever look up (en-XA is the pseudo-language for testing the layout).
pub const CODES: &[&str] = &["de", "es", "fr", "nl", "it", "zh-CN", "pl", "pt-BR", "sv", "ru", "en-XA"];

/// (code, English name, the language's own name): the languages people can pick.
pub const LANGS: &[(&str, &str, &str)] = &[
    ("de", "German", "Deutsch"),
    ("es", "Spanish", "Español"),
    ("fr", "French", "Français"),
    ("nl", "Dutch", "Nederlands"),
    ("it", "Italian", "Italiano"),
    ("zh-CN", "Chinese (Simplified)", "简体中文"),
    ("pl", "Polish", "Polski"),
    ("pt-BR", "Portuguese (Brazil)", "Português (Brasil)"),
    ("sv", "Swedish", "Svenska"),
    ("ru", "Russian", "Русский"),
];

const MAX_PACK: u64 = 4 << 20;
/// The folder under the data folder, and the settings scope/key of the server default.
const DIR: &str = "lang";
const SCOPE: &str = "lang";
const DEFAULT_KEY: &str = "default";

type Pins<'a> = &'a [(&'a str, &'a str, u64)];

pub fn sha256_hex(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect()
}

fn pin<'a>(pins: Pins<'a>, code: &str) -> Option<(&'a str, u64)> {
    if !CODES.contains(&code) {
        return None;
    }
    pins.iter().find(|p| p.0 == code).map(|p| (p.1, p.2))
}

/// Is `body` the pack pinned for `code`?
fn check_pack(pins: Pins, code: &str, body: &[u8]) -> Result<(), &'static str> {
    let (want, _) = pin(pins, code).ok_or("this version has no pack for that language")?;
    if body.len() as u64 > MAX_PACK {
        return Err("the pack is bigger than 4 MB");
    }
    if sha256_hex(body) != want {
        return Err("the pack is not the one this version of AmberSight was built with");
    }
    Ok(())
}

/// The code whose pinned pack `body` is (for an uploaded file: the file says which language it is
/// by being one of the known files).
fn identify(pins: Pins, body: &[u8]) -> Option<&'static str> {
    if body.len() as u64 > MAX_PACK {
        return None;
    }
    let h = sha256_hex(body);
    CODES.iter().copied().find(|c| pin(pins, c).is_some_and(|(want, _)| want == h))
}

fn user_facing(code: &str) -> bool {
    LANGS.iter().any(|l| l.0 == code)
}

fn pack_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR)
}

// ---------------------------------------------------------------------------------- reading

/// What identifies one state of a file. It only decides whether the bytes already checked are
/// still wanted: only bytes whose hash was checked are ever served, so a stamp that fails to
/// notice a rewrite (a coarse clock, Windows) can make a stale pack show, never a bad one.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime: Option<std::time::SystemTime>,
    #[cfg(unix)]
    unix: (u64, u64, i64, i64),
}

fn stamp(m: &std::fs::Metadata) -> Stamp {
    Stamp {
        size: m.len(),
        mtime: m.modified().ok(),
        // Device, inode and change time (which cannot be set back).
        #[cfg(unix)]
        unix: {
            use std::os::unix::fs::MetadataExt;
            (m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
        },
    }
}

/// The checked bytes of each code's file in each data folder, with the file's state when they were
/// read (None: the file was looked at and is not the pinned one). One entry per code and folder.
type Verdict = ((String, PathBuf), Stamp, Option<Arc<Vec<u8>>>);
static VERDICTS: Mutex<Vec<Verdict>> = Mutex::new(Vec::new());

/// Open a pack file for reading: never through a link, and without waiting on a pipe.
fn open_pack(path: &Path) -> Option<std::fs::File> {
    // Not a link (a Windows junction or symbolic link, like a Unix one, is refused here; the
    // check after the open means a swap in between is seen as a change of file).
    let m = std::fs::symlink_metadata(path).ok()?;
    if !m.is_file() {
        return None;
    }
    #[cfg(unix)]
    let f = {
        use std::os::unix::fs::OpenOptionsExt;
        // (O_NONBLOCK: a pipe put in its place must not make this wait.)
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).ok()?
    };
    #[cfg(windows)]
    let f = {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: a link is opened as itself, not followed (and is no regular file below).
        std::fs::OpenOptions::new().read(true).custom_flags(0x0020_0000).open(path).ok()?
    };
    #[cfg(not(any(unix, windows)))]
    let f = std::fs::File::open(path).ok()?;
    Some(f)
}

/// The pack of `code` in `data_dir` when it is a regular file (never through a link) holding the
/// pinned bytes. The bytes returned are ones that were checked.
fn read_pack(pins: Pins, data_dir: &Path, code: &str) -> Option<Arc<Vec<u8>>> {
    use std::io::Read;
    pin(pins, code)?;
    let path = pack_dir(data_dir).join(format!("{code}.json"));
    let mut f = open_pack(&path)?;
    let before = f.metadata().ok()?;
    if !before.is_file() || before.len() > MAX_PACK {
        return None;
    }
    let key = (code.to_string(), data_dir.to_path_buf());
    let st = stamp(&before);
    {
        let cache = VERDICTS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = cache.iter().find(|v| v.0 == key && v.1 == st) {
            return v.2.clone();
        }
    }
    let mut body = Vec::with_capacity(before.len() as usize);
    (&mut f).take(MAX_PACK + 1).read_to_end(&mut body).ok()?;
    let after = f.metadata().ok()?;
    // Changed while it was read: not served this time.
    if stamp(&after) != st || body.len() as u64 != after.len() {
        return None;
    }
    let ok = check_pack(pins, code, &body).is_ok().then(|| Arc::new(body));
    let mut cache = VERDICTS.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|v| v.0 != key);
    cache.push((key, st, ok.clone()));
    ok
}

fn installed(pins: Pins, data_dir: &Path) -> Vec<&'static str> {
    CODES.iter().copied().filter(|c| read_pack(pins, data_dir, c).is_some()).collect()
}

/// Does the request's If-None-Match name this ETag (or `*`)?
fn etag_matches(headers: &axum::http::HeaderMap, etag: &str) -> bool {
    headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == "*" || t.trim().trim_start_matches("W/") == etag))
}

/// `GET /api/lang/<code>` for one code (routes are made per code, see `public_routes`). The
/// ETag is the pinned hash, so a browser asks again cheaply and a changed pack is always new.
async fn serve(engine: Arc<Engine>, code: &'static str, headers: axum::http::HeaderMap) -> Response {
    let data_dir = engine.data_dir.clone();
    let read = tokio::task::spawn_blocking(move || read_pack(PINS, &data_dir, code)).await;
    let Ok(Some(body)) = read else { return StatusCode::NOT_FOUND.into_response() };
    // (A pack that was read is the pinned one, so its hash is the pin.)
    let etag = format!("\"{}\"", pin(PINS, code).map(|p| p.0).unwrap_or_default());
    if etag_matches(&headers, &etag) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag), (header::CACHE_CONTROL, "no-cache".to_string())]).into_response();
    }
    ([(header::CONTENT_TYPE, "application/json".to_string()), (header::ETAG, etag), (header::CACHE_CONTROL, "no-cache".to_string())], body.to_vec()).into_response()
}

// ------------------------------------------------------------------------------- installing

/// The folder for packs: made when missing (mode 755 where there are modes); an existing one must be
/// a real folder (not a link), and on Unix of this user and not changeable by others.
fn ensure_dir(data_dir: &Path) -> Result<PathBuf> {
    let dir = pack_dir(data_dir);
    match std::fs::symlink_metadata(&dir) {
        Ok(m) => {
            if m.file_type().is_symlink() || !m.is_dir() {
                bail!("{} is not a folder", dir.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // SAFETY: geteuid cannot fail.
                let me = unsafe { libc::geteuid() };
                if m.uid() != me {
                    bail!("{} belongs to another user", dir.display());
                }
                if m.mode() & 0o022 != 0 {
                    bail!("{} can be changed by other users (chmod go-w)", dir.display());
                }
            }
        }
        Err(_) => {
            let mut b = std::fs::DirBuilder::new();
            b.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o755);
            b.create(&dir).with_context(|| format!("could not make {}", dir.display()))?;
        }
    }
    Ok(dir)
}

/// Put a checked pack in place: a new file (never through a link), written whole, then renamed
/// over the old one. The bytes are checked first, here, whoever calls.
fn install_bytes(pins: Pins, data_dir: &Path, code: &str, body: &[u8]) -> Result<PathBuf> {
    use std::io::Write;
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    check_pack(pins, code, body).map_err(|e| anyhow!(e))?;
    let dir = ensure_dir(data_dir)?;
    let dest = dir.join(format!("{code}.json"));
    let tmp = dir.join(format!(".{code}.json.{}-{}.part", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let result = (|| -> Result<()> {
        // A new file only (create_new), never through a link: on Unix also O_NOFOLLOW and mode 644.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o644).custom_flags(libc::O_NOFOLLOW);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("could not write {}", tmp.display()))?;
        f.write_all(body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &dest).with_context(|| format!("could not make {}", dest.display()))?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(dest)
}

/// Where the release's files are, as for the add-ons: `[update] repo`'s release of this version, or
/// AMBERSIGHT_UPDATE_BASE_URL (tests). (Same rule as gpu_addon::release_base.)
fn release_base(repo: &str) -> Result<String> {
    match std::env::var("AMBERSIGHT_UPDATE_BASE_URL") {
        Ok(base) if !base.trim().is_empty() => Ok(base.trim().trim_end_matches('/').to_string()),
        _ => {
            if !crate::update::valid_repo(repo) {
                bail!("[update] repo must look like owner/name, not \"{repo}\"");
            }
            Ok(format!("https://github.com/{repo}/releases/download/v{}", crate::update::CURRENT))
        }
    }
}

fn asset_name(code: &str) -> String {
    format!("ambersight-lang-{code}-{}.json", crate::update::CURRENT)
}

/// Download the pack of `code`: the release's signed SHA256SUMS must list it with the pinned hash,
/// and the file must have that hash. `get(file, limit)` reads a file of the release;
/// `listed(name)` is the hash the signed list gives the asset.
fn fetch_with(pins: Pins, data_dir: &Path, code: &str, get: &dyn Fn(&str, u64) -> Result<Vec<u8>>, listed: &dyn Fn(&str) -> Result<String>) -> Result<PathBuf> {
    let (want, _) = pin(pins, code).ok_or_else(|| anyhow!("this version has no pack for that language"))?;
    let name = asset_name(code);
    let signed = listed(&name)?;
    if !signed.eq_ignore_ascii_case(want) {
        bail!("{name} in the signed SHA256SUMS is not the pack this version was built with");
    }
    let body = get(&name, MAX_PACK + 1).with_context(|| format!("could not download {name}"))?;
    install_bytes(pins, data_dir, code, &body)
}

fn fetch(code: &str, data_dir: &Path, repo: &str) -> Result<PathBuf> {
    let base = release_base(repo)?;
    let get = |file: &str, limit: u64| crate::update::get_small(&format!("{base}/{file}"), limit);
    let listed = |name: &str| -> Result<String> {
        let sums = get("SHA256SUMS", 1 << 20).context("SHA256SUMS of this version's release")?;
        let sig = get("SHA256SUMS.sig", 4096).context("SHA256SUMS.sig of this version's release")?;
        crate::update::verify_signature(&crate::update::release_pubkey(), &sums, &sig)?;
        crate::update::parse_sums(&String::from_utf8_lossy(&sums)).remove(name).ok_or_else(|| anyhow!("{name} is not listed in the signed SHA256SUMS"))
    };
    fetch_with(PINS, data_dir, code, &get, &listed)
}

/// Downloads are allowed (`[update] check`); else why not.
fn downloads_allowed(check: bool) -> Result<(), String> {
    if check {
        Ok(())
    } else {
        Err("downloads are turned off ([update] check = false): install the language from a file instead".to_string())
    }
}

/// A download in progress or the last one's failure, for the page.
#[derive(Clone, Default)]
struct Job {
    busy: bool,
    /// What went wrong, for admins and the log.
    error: Option<String>,
    failed_at: Option<std::time::Instant>,
}
/// After a failed download the same language is not tried again for this long.
const RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
static JOBS: Mutex<Vec<(String, Job)>> = Mutex::new(Vec::new());

fn job(code: &str) -> Job {
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|j| j.0 == code).map(|j| j.1.clone()).unwrap_or_default()
}
fn set_job(code: &str, j: Job) {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    jobs.retain(|x| x.0 != code);
    jobs.push((code.to_string(), j));
}

/// Claim the download of `code`: Ok(true) when this caller may start it, Ok(false) when one is
/// already running, Err (to tell the person) when the last try failed less than a minute ago.
fn begin(code: &str, now: std::time::Instant) -> Result<bool, String> {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(j) = jobs.iter().find(|j| j.0 == code).map(|j| j.1.clone()) {
        if j.busy {
            return Ok(false);
        }
        if let Some(left) = j.failed_at.and_then(|t| RETRY_WAIT.checked_sub(now.saturating_duration_since(t))).filter(|l| !l.is_zero()) {
            return Err(format!("the last download of this language failed; it can be tried again in {} seconds", left.as_secs() + 1));
        }
    }
    jobs.retain(|j| j.0 != code);
    jobs.push((code.to_string(), Job { busy: true, error: None, failed_at: None }));
    Ok(true)
}

/// Start downloading `code` in the background (nothing when it is installed or already coming).
/// The refusal is for the person who asked.
pub fn start_install(code: &str, data_dir: &Path, repo: &str, check: bool) -> Result<(), String> {
    if !user_facing(code) || pin(PINS, code).is_none() {
        return Err("this version has no pack for that language".into());
    }
    if read_pack(PINS, data_dir, code).is_some() {
        return Ok(());
    }
    downloads_allowed(check)?;
    if !begin(code, std::time::Instant::now())? {
        return Ok(());
    }
    let (code, dir, repo) = (code.to_string(), data_dir.to_path_buf(), repo.to_string());
    std::thread::spawn(move || run_job(&code, || fetch(&code, &dir, &repo)));
    Ok(())
}

/// Run the download claimed with `begin` and record how it went. Whatever happens, even a panic
/// in `work`, the job stops being busy (else that language could not be fetched until a restart).
fn run_job(code: &str, work: impl FnOnce() -> Result<PathBuf>) {
    struct Finish<'a>(&'a str, bool);
    impl Drop for Finish<'_> {
        fn drop(&mut self) {
            if !self.1 {
                tracing::warn!("language pack {} download stopped unexpectedly", self.0);
                set_job(self.0, Job { busy: false, error: Some("the download stopped unexpectedly".into()), failed_at: Some(std::time::Instant::now()) });
            }
        }
    }
    let mut guard = Finish(code, false);
    let error = work().err().map(|e| format!("{e:#}"));
    if let Some(e) = &error {
        tracing::warn!("language pack {code} could not be downloaded: {e}");
    }
    set_job(code, Job { busy: false, failed_at: error.as_ref().map(|_| std::time::Instant::now()), error });
    guard.1 = true;
}

/// At start-up: fetch again, in the background, every pack that is installed but is not the one
/// this version pins (after an update), and the server default's if it is missing.
pub fn refresh_after_start(engine: &Engine) {
    let (check, repo) = {
        let cfg = engine.config.lock().unwrap();
        (cfg.update.check, cfg.update.repo.clone())
    };
    refresh_in_background(&engine.data_dir, &repo, check, server_default(engine).as_deref());
}

fn refresh_in_background(data_dir: &Path, repo: &str, check: bool, default: Option<&str>) {
    if !check {
        return;
    }
    for (code, _, _) in LANGS {
        let dest = pack_dir(data_dir).join(format!("{code}.json"));
        let stale = dest.exists() && read_pack(PINS, data_dir, code).is_none();
        if (stale || default == Some(*code)) && pin(PINS, code).is_some() {
            let _ = start_install(code, data_dir, repo, check);
        }
    }
}

// ------------------------------------------------------------------------------- the API

fn json_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn valid_code(v: &serde_json::Value) -> Option<&str> {
    v.as_str().filter(|c| user_facing(c))
}

fn server_default(engine: &Engine) -> Option<String> {
    engine.db.ui_get(SCOPE, DEFAULT_KEY).ok().flatten().filter(|c| user_facing(c))
}

/// The user's own choice, kept with their other preferences (`PATCH /api/ui/prefs {"lang": ...}`).
fn user_choice(engine: &Engine, caller: &Caller) -> Option<String> {
    // (As ui_http::scope_of: open-LAN mode has one shared bucket.)
    let scope = if caller.0 == "(open)" { String::new() } else { caller.0.clone() };
    let prefs: serde_json::Value = serde_json::from_str(&engine.db.ui_get(&scope, "prefs").ok().flatten()?).ok()?;
    prefs.get("lang")?.as_str().filter(|c| user_facing(c)).map(str::to_string)
}

fn status_json(pins: Pins, data_dir: Option<&Path>, default: Option<String>, user: Option<String>, check: bool, admin: bool) -> serde_json::Value {
    let have = data_dir.map(|d| installed(pins, d)).unwrap_or_default();
    let available: Vec<_> = LANGS
        .iter()
        .filter_map(|(code, name, native)| {
            let (_, size) = pin(pins, code)?;
            Some(serde_json::json!({ "code": code, "name": name, "native": native, "machine": true, "size": size }))
        })
        .collect();
    let downloads: serde_json::Map<String, serde_json::Value> = LANGS
        .iter()
        .filter_map(|(code, _, _)| {
            let j = job(code);
            // The detail (paths, addresses) is for admins; everyone else is told it failed.
            let error = j.error.map(|e| if admin { e } else { "Download failed".to_string() });
            (j.busy || error.is_some()).then(|| (code.to_string(), serde_json::json!({ "busy": j.busy, "error": error })))
        })
        .collect();
    serde_json::json!({ "default": default, "user": user, "installed": have, "available": available, "downloads": downloads, "can_download": check })
}

pub mod http {
    use super::*;

    /// `GET /api/lang` (any signed-in user)
    pub async fn get(State(engine): State<Arc<Engine>>, axum::Extension(IsAdmin(admin)): axum::Extension<IsAdmin>, axum::Extension(caller): axum::Extension<Caller>) -> Response {
        let check = engine.config.lock().unwrap().update.check;
        let data_dir = engine.data_dir.clone();
        let (default, user) = (server_default(&engine), user_choice(&engine, &caller));
        let v = tokio::task::spawn_blocking(move || status_json(PINS, Some(&data_dir), default, user, check, admin)).await;
        match v {
            Ok(v) => Json(v).into_response(),
            Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "could not read the language packs"),
        }
    }

    /// `POST /api/lang/default {"code": "de" | null}` (admin)
    pub async fn set_default(
        State(engine): State<Arc<Engine>>,
        axum::Extension(IsAdmin(admin)): axum::Extension<IsAdmin>,
        axum::Extension(caller): axum::Extension<Caller>,
        axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
        Json(body): Json<serde_json::Value>,
    ) -> Response {
        if !admin {
            return json_error(StatusCode::FORBIDDEN, "admin access required");
        }
        let code = match body.get("code") {
            Some(serde_json::Value::Null) | None => None,
            Some(v) => match valid_code(v) {
                Some(c) => Some(c.to_string()),
                None => return json_error(StatusCode::BAD_REQUEST, "unknown language"),
            },
        };
        let stored = match &code {
            Some(c) => engine.db.ui_put(SCOPE, DEFAULT_KEY, c),
            None => engine.db.ui_delete(SCOPE, DEFAULT_KEY),
        };
        if let Err(e) = stored {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"));
        }
        let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "language default changed", code.as_deref().unwrap_or("English"), 200);
        // The choice stands even when the pack cannot be fetched now: the page shows English until it is there.
        let note = match &code {
            Some(c) => {
                let (check, repo) = {
                    let cfg = engine.config.lock().unwrap();
                    (cfg.update.check, cfg.update.repo.clone())
                };
                start_install(c, &engine.data_dir, &repo, check).err()
            }
            None => None,
        };
        Json(serde_json::json!({ "ok": true, "default": code, "note": note })).into_response()
    }

    /// `POST /api/lang/install {"code": "de"}` (admin): fetch now; the answer says how it is going
    /// (`GET /api/lang` carries it afterwards: "downloads").
    pub async fn install(
        State(engine): State<Arc<Engine>>,
        axum::Extension(IsAdmin(admin)): axum::Extension<IsAdmin>,
        axum::Extension(caller): axum::Extension<Caller>,
        axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
        Json(body): Json<serde_json::Value>,
    ) -> Response {
        if !admin {
            return json_error(StatusCode::FORBIDDEN, "admin access required");
        }
        let Some(code) = body.get("code").and_then(valid_code).map(str::to_string) else { return json_error(StatusCode::BAD_REQUEST, "unknown language") };
        let (check, repo) = {
            let cfg = engine.config.lock().unwrap();
            (cfg.update.check, cfg.update.repo.clone())
        };
        let dir = engine.data_dir.clone();
        if let Err(why) = start_install(&code, &dir, &repo, check) {
            return json_error(StatusCode::CONFLICT, &why);
        }
        let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "language pack install", &code, 200);
        let (have, j) = (read_pack(PINS, &dir, &code).is_some(), job(&code));
        Json(serde_json::json!({ "code": code, "installed": have, "busy": j.busy, "error": j.error })).into_response()
    }

    /// `POST /api/lang/upload` with the pack file as the body (admin): offline install. Only a file
    /// that is exactly a pack pinned in this version is accepted; the file says nothing else.
    pub async fn upload(
        State(engine): State<Arc<Engine>>,
        axum::Extension(IsAdmin(admin)): axum::Extension<IsAdmin>,
        axum::Extension(caller): axum::Extension<Caller>,
        axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
        req: Request,
    ) -> Response {
        if !admin {
            return json_error(StatusCode::FORBIDDEN, "admin access required");
        }
        let body = match axum::body::to_bytes(req.into_body(), MAX_PACK as usize + 1).await {
            Ok(b) => b,
            Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "the pack is bigger than 4 MB"),
        };
        let dir = engine.data_dir.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<&'static str, String> {
            let code = identify(PINS, &body).ok_or("this file is not a language pack of this version of AmberSight (a pack works only with the version it was made for)")?;
            install_bytes(PINS, &dir, code, &body).map_err(|e| format!("{e:#}"))?;
            Ok(code)
        })
        .await;
        match result {
            Ok(Ok(code)) => {
                let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "language pack uploaded", code, 200);
                Json(serde_json::json!({ "ok": true, "code": code })).into_response()
            }
            Ok(Err(e)) => {
                let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "language pack upload refused", &e, 400);
                json_error(StatusCode::BAD_REQUEST, &e)
            }
            Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "could not install the pack"),
        }
    }

    /// `GET /api/lang/server-default` (public: the sign-in page): the code only.
    pub async fn public_default(State(engine): State<Arc<Engine>>) -> Response {
        Json(serde_json::json!({ "code": server_default(&engine) })).into_response()
    }
}

/// The routes anyone may call, without signing in (the sign-in page is translated too): one
/// route per known code, so `/api/lang/default`, `/install` and `/upload` below the same prefix
/// reach the signed-in API and no code is ever read from the URL.
pub fn public_routes() -> axum::Router<Arc<Engine>> {
    let mut r = axum::Router::new().route("/api/lang/server-default", axum::routing::get(http::public_default));
    for code in CODES {
        let code: &'static str = code;
        r = r.route(&format!("/api/lang/{code}"), axum::routing::get(move |State(engine): State<Arc<Engine>>, headers: axum::http::HeaderMap| serve(engine, code, headers)));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const DE: &[u8] = br#"{"lang":"de","messages":{"a":["Hallo","00000000"]}}"#;
    const ES: &[u8] = br#"{"lang":"es","messages":{"a":["Hola","00000000"]}}"#;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-lang-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn with_pins<R>(f: impl FnOnce(Pins) -> R) -> R {
        let p = [("de", sha256_hex(DE), DE.len() as u64), ("es", sha256_hex(ES), ES.len() as u64)];
        let view: Vec<(&str, &str, u64)> = p.iter().map(|(c, h, s)| (*c, h.as_str(), *s)).collect();
        f(&view)
    }

    #[test]
    fn only_listed_codes() {
        for bad in ["../config", "de/../../x", "DE", "en", "", "de.json", "zh-cn", "de\0", "de/", "/de"] {
            assert!(CODES.iter().all(|c| *c != bad), "{bad}");
            with_pins(|p| assert!(pin(p, bad).is_none(), "{bad}"));
        }
        // What build.rs pinned is a known code, with a well-formed hash.
        for (code, hex, _) in PINS {
            assert!(CODES.contains(code), "{code}");
            assert!(hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()), "{code}");
        }
        // Every language people can pick is a known code.
        assert!(LANGS.iter().all(|l| CODES.contains(&l.0)));
    }

    #[test]
    fn pinned_hash_good_tampered_and_wrong_code() {
        with_pins(|p| {
            assert!(check_pack(p, "de", DE).is_ok());
            // One byte different.
            let mut bad = DE.to_vec();
            bad[20] ^= 1;
            assert!(check_pack(p, "de", &bad).is_err());
            // Another language's genuine pack under this code.
            assert!(check_pack(p, "de", ES).is_err());
            assert!(check_pack(p, "es", DE).is_err());
            // A code with no pin, and an unknown code.
            assert!(check_pack(p, "fr", DE).is_err());
            assert!(check_pack(p, "../de", DE).is_err());
            // Empty and padded.
            assert!(check_pack(p, "de", b"").is_err());
            let mut padded = DE.to_vec();
            padded.push(b' ');
            assert!(check_pack(p, "de", &padded).is_err());
        });
    }

    #[test]
    fn identify_names_the_language_by_the_file() {
        with_pins(|p| {
            assert_eq!(identify(p, DE), Some("de"));
            assert_eq!(identify(p, ES), Some("es"));
            assert_eq!(identify(p, b"{}"), None);
            // A pack that says it is German but is not the pinned one.
            assert_eq!(identify(p, br#"{"lang":"de","messages":{"a":["<img onerror=x>","00000000"]}}"#), None);
        });
    }

    #[cfg(unix)]
    #[test]
    fn serving_only_pinned_files_and_never_through_links() {
        let d = temp("serve");
        with_pins(|p| {
            // Nothing there.
            assert!(read_pack(p, &d, "de").is_none());
            install_bytes(p, &d, "de", DE).unwrap();
            assert_eq!(read_pack(p, &d, "de").unwrap().as_slice(), DE);
            assert_eq!(installed(p, &d), vec!["de"]);
            // Tampered on disk: refused, also after it was served once (the verdict is cached by file state).
            let path = d.join("lang/de.json");
            std::fs::write(&path, br#"{"lang":"de","messages":{"a":["<img onerror=x>","00000000"]}}"#).unwrap();
            assert!(read_pack(p, &d, "de").is_none());
            // Put back: served again.
            std::fs::write(&path, DE).unwrap();
            assert_eq!(read_pack(p, &d, "de").unwrap().as_slice(), DE);
            // Same size, same modification time, different bytes: still refused (ctime moves).
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            let mut evil = DE.to_vec();
            evil[16] = b'X';
            std::fs::write(&path, &evil).unwrap();
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(mtime).unwrap();
            assert!(read_pack(p, &d, "de").is_none());
            // A link to a genuine pack elsewhere is not followed.
            let real = d.join("elsewhere.json");
            std::fs::write(&real, DE).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&real, &path).unwrap();
            assert!(read_pack(p, &d, "de").is_none());
            // A folder in its place.
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert!(read_pack(p, &d, "de").is_none());
            // A code that has a file but no pin.
            std::fs::write(d.join("lang/fr.json"), DE).unwrap();
            assert!(read_pack(p, &d, "fr").is_none());
        });
    }

    #[test]
    fn oversize_is_refused_everywhere() {
        let d = temp("big");
        // A pin for a file over the limit cannot be met: size is checked before the hash.
        let big = vec![b'x'; MAX_PACK as usize + 1];
        let hex = sha256_hex(&big);
        let pins: Vec<(&str, &str, u64)> = vec![("de", hex.as_str(), big.len() as u64)];
        assert!(check_pack(&pins, "de", &big).is_err());
        assert!(identify(&pins, &big).is_none());
        assert!(install_bytes(&pins, &d, "de", &big).is_err());
        assert!(!d.join("lang/de.json").exists());
        // On disk too.
        std::fs::create_dir_all(d.join("lang")).unwrap();
        std::fs::write(d.join("lang/de.json"), &big).unwrap();
        assert!(read_pack(&pins, &d, "de").is_none());
        // A download over the limit.
        let r = fetch_with(&pins, &d, "de", &|_, limit| Ok(vec![b'x'; limit as usize]), &|_| Ok(hex.clone()));
        assert!(r.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn install_makes_a_private_folder_and_leaves_no_temp_files() {
        let d = temp("install");
        with_pins(|p| {
            let dest = install_bytes(p, &d, "de", DE).unwrap();
            assert_eq!(dest, d.join("lang/de.json"));
            assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o644);
            assert_eq!(std::fs::metadata(d.join("lang")).unwrap().permissions().mode() & 0o022, 0);
            // A refused pack leaves nothing.
            assert!(install_bytes(p, &d, "es", DE).is_err());
            assert!(!d.join("lang/es.json").exists());
            let names: Vec<_> = std::fs::read_dir(d.join("lang")).unwrap().map(|e| e.unwrap().file_name()).collect();
            assert_eq!(names.len(), 1, "{names:?}");
            // Installing over an old file replaces it.
            std::fs::write(d.join("lang/es.json"), b"old").unwrap();
            install_bytes(p, &d, "es", ES).unwrap();
            assert_eq!(std::fs::read(d.join("lang/es.json")).unwrap(), ES);
        });
    }

    #[cfg(unix)]
    #[test]
    fn install_will_not_write_through_a_link_or_into_an_open_folder() {
        with_pins(|p| {
            // The destination is a link to a file elsewhere: the rename replaces the link, the target is untouched.
            let d = temp("link");
            std::fs::create_dir(d.join("lang")).unwrap();
            std::fs::set_permissions(d.join("lang"), std::fs::Permissions::from_mode(0o755)).unwrap();
            let victim = d.join("victim");
            std::fs::write(&victim, b"keep").unwrap();
            std::os::unix::fs::symlink(&victim, d.join("lang/de.json")).unwrap();
            install_bytes(p, &d, "de", DE).unwrap();
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
            assert_eq!(std::fs::read(d.join("lang/de.json")).unwrap(), DE);
            // The folder is a link: nothing is written.
            let d = temp("dirlink");
            let other = temp("dirlink-other");
            std::os::unix::fs::symlink(&other, d.join("lang")).unwrap();
            assert!(install_bytes(p, &d, "de", DE).is_err());
            assert!(std::fs::read_dir(&other).unwrap().next().is_none());
            // The folder can be changed by others.
            let d = temp("open");
            std::fs::create_dir(d.join("lang")).unwrap();
            std::fs::set_permissions(d.join("lang"), std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(install_bytes(p, &d, "de", DE).is_err());
        });
    }

    #[test]
    fn download_checks_the_signed_list_and_the_pin() {
        with_pins(|p| {
            let name = asset_name("de");
            let d = temp("fetch");
            let pinned = sha256_hex(DE);
            let ok = fetch_with(
                p,
                &d,
                "de",
                &|file, _| {
                    assert_eq!(file, name);
                    Ok(DE.to_vec())
                },
                &|n| {
                    assert_eq!(n, name);
                    Ok(pinned.clone())
                },
            );
            assert_eq!(ok.unwrap(), d.join("lang/de.json"));
            // The signed list says another hash: nothing is even downloaded.
            let d = temp("fetch-list");
            let r = fetch_with(p, &d, "de", &|_, _| panic!("downloaded"), &|_| Ok(sha256_hex(ES)));
            assert!(r.is_err());
            // The signed list does not have it.
            let r = fetch_with(p, &d, "de", &|_, _| panic!("downloaded"), &|_| Err(anyhow!("not listed")));
            assert!(r.is_err());
            // The file served is not the pinned one (the list and the pin agree).
            let r = fetch_with(p, &d, "de", &|_, _| Ok(ES.to_vec()), &|_| Ok(pinned.clone()));
            assert!(r.is_err());
            assert!(!d.join("lang/de.json").exists());
            // A language without a pin.
            assert!(fetch_with(p, &d, "fr", &|_, _| panic!("downloaded"), &|_| Ok(pinned.clone())).is_err());
            // The download itself failing.
            assert!(fetch_with(p, &d, "de", &|_, _| Err(anyhow!("offline")), &|_| Ok(pinned.clone())).is_err());
        });
    }

    #[test]
    fn nothing_is_downloaded_when_downloads_are_off() {
        let d = temp("off");
        // (This build may pin no pack at all; either way nothing is made or fetched.)
        assert!(start_install("de", &d, "o/r", false).is_err());
        assert!(downloads_allowed(false).unwrap_err().contains("from a file"));
        assert!(downloads_allowed(true).is_ok());
        assert!(!d.join("lang").exists());
        // Unknown languages are refused before anything else.
        assert!(start_install("../x", &d, "o/r", true).is_err());
        assert!(start_install("en-XA", &d, "o/r", true).is_err());
    }

    #[test]
    fn the_list_shows_what_this_version_can_install() {
        let d = temp("list");
        with_pins(|p| {
            install_bytes(p, &d, "de", DE).unwrap();
            let v = status_json(p, Some(&d), Some("de".into()), None, true, true);
            assert_eq!(v["default"], "de");
            assert!(v["user"].is_null());
            assert_eq!(v["installed"], serde_json::json!(["de"]));
            let av = v["available"].as_array().unwrap();
            assert_eq!(av.len(), 2);
            assert_eq!(av[0]["code"], "de");
            assert_eq!(av[0]["name"], "German");
            assert_eq!(av[0]["native"], "Deutsch");
            assert_eq!(av[0]["machine"], true);
            assert_eq!(av[0]["size"], DE.len());
            assert_eq!(v["can_download"], true);
        });
        // No packs in the build: nothing offered.
        let none: Pins = &[];
        assert_eq!(status_json(none, Some(&d), None, None, true, true)["available"], serde_json::json!([]));
    }

    // ---- adversarial review tests ----
    #[cfg(unix)]
    #[test]
    fn adv_same_tick_inplace_rewrite_is_refused() {
        // Good pack read (verdict cached), then rewritten in place with same-size evil bytes and mtime restored,
        // as fast as possible many times: the cache must never serve the evil bytes.
        let d = temp("advtick");
        with_pins(|p| {
            install_bytes(p, &d, "de", DE).unwrap();
            let path = d.join("lang/de.json");
            let mut evil = DE.to_vec();
            evil[16] = b'X';
            let mut served_evil = 0;
            for _ in 0..20000 {
                std::fs::write(&path, DE).unwrap();
                let mt = std::fs::metadata(&path).unwrap().modified().unwrap();
                assert!(read_pack(p, &d, "de").is_some());
                let f = std::fs::File::options().write(true).open(&path).unwrap();
                use std::os::unix::fs::FileExt;
                f.write_all_at(&evil, 0).unwrap();
                f.set_modified(mt).unwrap();
                if let Some(b) = read_pack(p, &d, "de") {
                    if *b == evil { served_evil += 1; }
                }
            }
            assert_eq!(served_evil, 0, "evil bytes served from the verdict cache");
        });
    }

    #[cfg(unix)]
    #[test]
    fn adv_fifo_and_dirlink_reads() {
        use std::ffi::CString;
        let d = temp("advfifo");
        with_pins(|p| {
            std::fs::create_dir_all(d.join("lang")).unwrap();
            let c = CString::new(d.join("lang/de.json").to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
            assert!(read_pack(p, &d, "de").is_none()); // must return, not hang
            // the lang folder itself a link to a folder holding the genuine pack: bytes are genuine, so serving is harmless
            let other = temp("advfifo-other");
            std::fs::write(other.join("de.json"), DE).unwrap();
            let d2 = temp("advfifo2");
            std::os::unix::fs::symlink(&other, d2.join("lang")).unwrap();
            println!("read through linked lang dir: {}", read_pack(p, &d2, "de").is_some());
        });
    }

    #[test]
    fn download_errors_show_detail_only_to_admins() {
        set_job("sv", Job { busy: false, error: Some("could not write /srv/ambersight/lang/.sv.json.1-0.part: denied".into()), failed_at: Some(std::time::Instant::now()) });
        with_pins(|p| {
            let d = temp("leak");
            let admin = status_json(p, Some(&d), None, None, true, true);
            let viewer = status_json(p, Some(&d), None, None, true, false);
            assert!(admin["downloads"]["sv"]["error"].as_str().unwrap().contains("/srv/ambersight"));
            assert_eq!(viewer["downloads"]["sv"]["error"], "Download failed");
            assert!(!viewer.to_string().contains("/srv"));
        });
        set_job("sv", Job::default());
    }

    #[test]
    fn a_failed_download_waits_a_minute() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        set_job("nl", Job::default());
        assert_eq!(begin("nl", t0), Ok(true));
        // Running: nobody starts a second one.
        assert_eq!(begin("nl", t0), Ok(false));
        set_job("nl", Job { busy: false, error: Some("x".into()), failed_at: Some(t0) });
        let e = begin("nl", t0 + Duration::from_secs(10)).unwrap_err();
        assert!(e.contains("again in 51 seconds"), "{e}");
        assert!(begin("nl", t0 + Duration::from_secs(59)).is_err());
        assert_eq!(begin("nl", t0 + Duration::from_secs(60)), Ok(true));
        set_job("nl", Job::default());
    }

    #[test]
    fn a_download_that_panics_does_not_stay_busy() {
        set_job("pl", Job::default());
        assert_eq!(begin("pl", std::time::Instant::now()), Ok(true));
        assert!(job("pl").busy);
        let r = std::thread::spawn(|| run_job("pl", || -> Result<PathBuf> { panic!("boom") })).join();
        assert!(r.is_err(), "the panic is still a panic");
        let j = job("pl");
        assert!(!j.busy, "still busy after a panic");
        assert_eq!(j.error.as_deref(), Some("the download stopped unexpectedly"));
        // A viewer is told only "Download failed"; the next try waits like after any failure.
        assert!(begin("pl", std::time::Instant::now()).is_err());
        // A normal failure and a success also end the job.
        set_job("pl", Job::default());
        assert_eq!(begin("pl", std::time::Instant::now()), Ok(true));
        run_job("pl", || bail!("offline"));
        assert!(!job("pl").busy && job("pl").error.as_deref() == Some("offline"));
        set_job("pl", Job::default());
        assert_eq!(begin("pl", std::time::Instant::now()), Ok(true));
        run_job("pl", || Ok(PathBuf::from("x")));
        assert!(!job("pl").busy && job("pl").error.is_none());
        set_job("pl", Job::default());
    }

    #[test]
    fn etag_is_the_pinned_hash_and_star_matches() {
        use axum::http::{HeaderMap, HeaderValue};
        let tag = format!("\"{}\"", sha256_hex(DE));
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert(header::IF_NONE_MATCH, HeaderValue::from_str(v).unwrap());
            m
        };
        assert!(etag_matches(&h(&tag), &tag));
        assert!(etag_matches(&h(&format!("W/{tag}")), &tag));
        assert!(etag_matches(&h(&format!("\"other\", {tag}")), &tag));
        assert!(etag_matches(&h("*"), &tag));
        assert!(!etag_matches(&h("\"other\""), &tag));
        assert!(!etag_matches(&HeaderMap::new(), &tag));
    }

    #[test]
    fn a_checked_pack_is_kept_and_served_until_the_file_changes() {
        let d = temp("keep");
        with_pins(|p| {
            install_bytes(p, &d, "de", DE).unwrap();
            let a = read_pack(p, &d, "de").unwrap();
            let b = read_pack(p, &d, "de").unwrap();
            // The second answer is the same copy in memory (the file was not read again).
            assert!(Arc::ptr_eq(&a, &b));
            // The file's name is gone: not served, whatever is in memory.
            std::fs::remove_file(d.join("lang/de.json")).unwrap();
            assert!(read_pack(p, &d, "de").is_none());
        });
    }

    // ---- Windows: reading is attacked the way the Unix tests attack it ----

    /// A directory junction `link` -> `target` (no privilege needed), or false.
    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().map(|o| o.status.success()).unwrap_or(false)
    }

    #[cfg(windows)]
    #[test]
    fn win_symlink_at_the_pack_is_not_followed() {
        let d = temp("wsym");
        with_pins(|p| {
            std::fs::create_dir_all(d.join("lang")).unwrap();
            let real = d.join("elsewhere.json");
            std::fs::write(&real, DE).unwrap();
            if let Err(e) = std::os::windows::fs::symlink_file(&real, d.join("lang/de.json")) {
                println!("SKIPPED win_symlink: cannot create a symbolic link here ({e})");
                return;
            }
            assert!(read_pack(p, &d, "de").is_none(), "a symbolic link to a genuine pack was served");
            // Installing replaces the link, never writes through it.
            let victim = d.join("victim.txt");
            std::fs::remove_file(d.join("lang/de.json")).unwrap();
            std::fs::write(&victim, b"keep").unwrap();
            std::os::windows::fs::symlink_file(&victim, d.join("lang/de.json")).unwrap();
            install_bytes(p, &d, "de", DE).unwrap();
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
            println!("win_symlink: link refused for reading, replaced (not written through) by install");
        });
    }

    #[cfg(windows)]
    #[test]
    fn win_junction_for_the_folder_is_refused_and_never_gives_bad_bytes() {
        let d = temp("wjunc");
        let other = temp("wjunc-other");
        with_pins(|p| {
            std::fs::write(other.join("de.json"), br#"{"lang":"de","messages":{"a":["<img onerror=x>","00000000"]}}"#).unwrap();
            if !junction(&d.join("lang"), &other) {
                println!("SKIPPED win_junction: mklink /J failed here");
                return;
            }
            // Evil bytes behind the junction: never served.
            assert!(read_pack(p, &d, "de").is_none(), "unverified bytes served through a junction");
            // Installing refuses to use the junction as the folder, and writes nothing behind it.
            assert!(install_bytes(p, &d, "es", ES).is_err(), "install went through a junction");
            assert!(!other.join("es.json").exists());
            println!("win_junction: evil bytes behind the junction not served; install refused");
        });
    }

    #[cfg(windows)]
    #[test]
    fn win_hard_link_to_a_non_pack_file_is_not_served() {
        let d = temp("whard");
        with_pins(|p| {
            std::fs::create_dir_all(d.join("lang")).unwrap();
            let other = d.join("settings.toml");
            std::fs::write(&other, b"secret = 1\n").unwrap();
            std::fs::hard_link(&other, d.join("lang/de.json")).unwrap();
            assert!(read_pack(p, &d, "de").is_none(), "a hard link to another file was served");
            // A hard link to a genuine pack that is then rewritten (same size, same time) through the other name.
            std::fs::remove_file(d.join("lang/de.json")).unwrap();
            let real = d.join("real.json");
            std::fs::write(&real, DE).unwrap();
            std::fs::hard_link(&real, d.join("lang/de.json")).unwrap();
            assert_eq!(read_pack(p, &d, "de").unwrap().as_slice(), DE);
            let mt = std::fs::metadata(&real).unwrap().modified().unwrap();
            let mut evil = DE.to_vec();
            evil[16] = b'X';
            std::fs::write(&real, &evil).unwrap();
            std::fs::File::options().write(true).open(&real).unwrap().set_modified(mt).unwrap();
            if let Some(b) = read_pack(p, &d, "de") {
                assert_eq!(b.as_slice(), DE, "evil bytes served after a rewrite through a hard link");
            }
            println!("win_hard_link: non-pack never served; rewritten pack gave none or the checked bytes");
        });
    }

    #[cfg(windows)]
    #[test]
    fn win_same_size_rewrite_never_serves_unverified_bytes() {
        let d = temp("wtick");
        with_pins(|p| {
            install_bytes(p, &d, "de", DE).unwrap();
            let path = d.join("lang/de.json");
            let mut evil = DE.to_vec();
            evil[16] = b'X';
            let (mut evil_served, mut checked_served, mut none) = (0, 0, 0);
            for _ in 0..3000 {
                std::fs::write(&path, DE).unwrap();
                let mt = std::fs::metadata(&path).unwrap().modified().unwrap();
                assert!(read_pack(p, &d, "de").is_some());
                // Same size, same modification time, different bytes.
                std::fs::write(&path, &evil).unwrap();
                std::fs::File::options().write(true).open(&path).unwrap().set_modified(mt).unwrap();
                match read_pack(p, &d, "de") {
                    Some(b) if *b == evil => evil_served += 1,
                    Some(_) => checked_served += 1,
                    None => none += 1,
                }
            }
            println!("win_same_size_rewrite: evil served {evil_served}, checked bytes served {checked_served}, refused {none}");
            assert_eq!(evil_served, 0, "unverified bytes served");
        });
    }

}
