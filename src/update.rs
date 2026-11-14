//! Updating AmberSight from its GitHub releases, and signing those releases.
//!
//! Signing (done once per release on the owner's computer, see RELEASING.md):
//! `ambersight release-keygen` makes an Ed25519 key pair (kept offline), and
//! `ambersight release-sign` signs a release's SHA256SUMS file. The public
//! half of the key is built into the program below (`RELEASE_PUBKEY`).
//!
//! Updating (an admin's click in Settings > System > Status & updates, or `ambersight update`):
//! 1. ask GitHub for the latest release (no account or token, a plain
//!    "AmberSight/<version>" User-Agent and nothing else about this server),
//! 2. download SHA256SUMS and SHA256SUMS.sig and check the signature with
//!    the built-in public key (nothing is installed without it),
//! 3. download this computer's package into `<data folder>/update`, check
//!    its SHA-256 against SHA256SUMS, unpack it, and check the new program
//!    runs and reports the expected version,
//! 4. copy the program now in use (and its libraries) to `previous/` next
//!    to it, so going back by hand is possible, then put the new files in
//!    place: on Mac/Linux each is written next to the old one and renamed
//!    over it (atomic; the running program keeps its old copy open); on
//!    Windows the running files are renamed to `*.old` first (allowed while
//!    they are in use) and deleted on the next start,
//! 5. when running as a service, stop cleanly with an error code so the
//!    service manager starts the new version (Windows recovery actions,
//!    systemd Restart=on-failure, launchd KeepAlive); otherwise say that a
//!    restart is needed.
//!
//! Nothing is ever installed by itself: checks run every 12 hours
//! (unless `[update] check = false`), installing always needs an admin. Programs
//! installed by a package manager (under /usr) are refused: apt/dnf update
//! those.
//!
//! Testing without GitHub (test only): `AMBERSIGHT_UPDATE_BASE_URL=http://..`
//! makes the check read `<base>/latest.json` (the same JSON GitHub returns)
//! instead. That alone cannot install anything the owner did not sign: the
//! signature is still checked with the built-in key. Only debug builds (cargo
//! build without --release, never shipped) also read a test public key from
//! `AMBERSIGHT_UPDATE_TEST_PUBKEY`; release builds do not contain that code.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The release signing public key (Ed25519, base64). Its private half lives
/// offline with the owner (RELEASING.md).
pub const RELEASE_PUBKEY: &str = "b2XBLa0tLhFadZkJf0jXca9ouQ0GdN6dx5DoKRzfrYU=";

/// The version of this program.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Biggest package accepted, and the longest a download may take.
const MAX_PACKAGE_BYTES: u64 = 300 << 20;
const DOWNLOAD_SECS: u64 = 600;
/// Everything unpacked from a package together (it holds one program and a
/// few libraries).
const MAX_UNPACKED_BYTES: u64 = 1 << 30;
/// How often the automatic check runs (when turned on).
const CHECK_EVERY_SECS: i64 = 12 * 3600;

// ---------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------

/// `[update]` in the config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateConfig {
    /// The GitHub repository releases come from, "owner/name".
    #[serde(default = "default_repo")]
    pub repo: String,
    /// Look for a new version every 12 hours (on unless turned off).
    #[serde(default = "default_check")]
    pub check: bool,
}

fn default_check() -> bool {
    true
}

fn default_repo() -> String {
    "HuskerMinion/AmberSight".to_string()
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig { repo: default_repo(), check: true }
    }
}

impl UpdateConfig {
    pub fn is_default(&self) -> bool {
        *self == UpdateConfig::default()
    }
}

// ---------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------

/// Make a new release key pair: the private key goes to `out` (PKCS#8,
/// readable only by its owner on Mac/Linux; an existing file is never
/// overwritten). Returns the public key (base64).
pub fn keygen(out: &Path) -> Result<String> {
    use ring::signature::KeyPair;
    let rng = ring::rand::SystemRandom::new();
    let doc = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow!("could not make a key"))?;
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(|_| anyhow!("could not read the new key"))?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(out).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow!("{} already exists; it is not overwritten", out.display())
        } else {
            anyhow!("could not create {}: {e}", out.display())
        }
    })?;
    std::io::Write::write_all(&mut f, doc.as_ref())?;
    f.sync_all()?;
    Ok(b64(pair.public_key().as_ref()))
}

/// Sign the exact bytes of `file` with the private key in `key`, writing
/// `<file>.sig` (base64). Returns the .sig path and the key's public half.
pub fn sign_file(key: &Path, file: &Path) -> Result<(PathBuf, String)> {
    use ring::signature::KeyPair;
    let der = std::fs::read(key).with_context(|| format!("could not read the key {}", key.display()))?;
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der).map_err(|_| anyhow!("{} is not an Ed25519 PKCS#8 key", key.display()))?;
    let data = std::fs::read(file).with_context(|| format!("could not read {}", file.display()))?;
    let sig = b64(pair.sign(&data).as_ref());
    let mut out = file.as_os_str().to_owned();
    out.push(".sig");
    let out = PathBuf::from(out);
    std::fs::write(&out, format!("{sig}\n")).with_context(|| format!("could not write {}", out.display()))?;
    Ok((out, b64(pair.public_key().as_ref())))
}

/// Check that `sig` (base64, as release-sign writes it) is a signature of
/// `data` by the key `pubkey` (base64).
pub fn verify_signature(pubkey: &str, data: &[u8], sig: &[u8]) -> Result<()> {
    use base64::Engine;
    let pk = base64::engine::general_purpose::STANDARD.decode(pubkey.trim()).map_err(|_| anyhow!("the release key is not valid base64"))?;
    let sig_text = std::str::from_utf8(sig).map_err(|_| anyhow!("the signature file is not text"))?;
    let sig = base64::engine::general_purpose::STANDARD.decode(sig_text.trim()).map_err(|_| anyhow!("the signature file is not valid base64"))?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &pk)
        .verify(data, &sig)
        .map_err(|_| anyhow!("the release signature is not valid: refusing to install"))
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The key releases must be signed with. Debug builds (tests only) may use
/// another from AMBERSIGHT_UPDATE_TEST_PUBKEY; release builds cannot.
pub(crate) fn release_pubkey() -> String {
    #[cfg(debug_assertions)]
    {
        if let Ok(k) = std::env::var("AMBERSIGHT_UPDATE_TEST_PUBKEY") {
            if !k.trim().is_empty() {
                return k;
            }
        }
    }
    RELEASE_PUBKEY.to_string()
}

// ---------------------------------------------------------------------
// Versions, packages, checksums
// ---------------------------------------------------------------------

/// "v1.2.3" or "1.2" as numbers. Pre-releases ("1.2.3-beta") are None:
/// they are never offered.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let s = s.split('+').next()?; // build metadata does not count
    if s.contains('-') || s.is_empty() {
        return None;
    }
    let parts: Vec<u64> = s.split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<_>>()?;
    match parts.as_slice() {
        [a] => Some((*a, 0, 0)),
        [a, b] => Some((*a, *b, 0)),
        [a, b, c] => Some((*a, *b, *c)),
        _ => None,
    }
}

/// Is version `latest` newer than `current`?
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// This computer as the package names spell it: (os, arch), e.g.
/// ("linux", "aarch64"), ("darwin", "arm64"), ("windows", "x86_64").
pub fn machine() -> (&'static str, &'static str) {
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    };
    // `uname -m` on a Mac says arm64 where Rust says aarch64.
    let arch = match std::env::consts::ARCH {
        "aarch64" if os == "darwin" => "arm64",
        a => a,
    };
    (os, arch)
}

/// The package of `version` for a computer, as scripts/package*.sh name it.
pub fn asset_name(version: &str, os: &str, arch: &str) -> Option<String> {
    match (os, arch) {
        ("windows", "x86_64") => Some(format!("ambersight-{version}-windows-x86_64.zip")),
        ("linux", "x86_64" | "aarch64") | ("darwin", "x86_64" | "arm64") => Some(format!("ambersight-{version}-{os}-{arch}.tar.gz")),
        _ => None,
    }
}

/// Read a SHA256SUMS file ("<hex>  <name>" per line, as sha256sum writes
/// it; a "*" before the name means binary mode and changes nothing).
pub fn parse_sums(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some((hash, rest)) = line.split_once(char::is_whitespace) else { continue };
        let name = rest.trim_start().trim_start_matches('*');
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) && !name.is_empty() {
            out.insert(name.to_string(), hash.to_ascii_lowercase());
        }
    }
    out
}

// ---------------------------------------------------------------------
// Where the program lives
// ---------------------------------------------------------------------

const PACKAGED: &str = "AmberSight was installed by the package manager: update with apt/dnf";

/// The program file in use, found once (on Linux the path reads
/// "... (deleted)" after the file is replaced).
pub fn program_path() -> Result<PathBuf> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok().map(|p| std::fs::canonicalize(&p).unwrap_or(p)))
        .clone()
        .ok_or_else(|| anyhow!("could not find the AmberSight program file"))
}

/// Programs under /usr come from a deb/rpm package (/usr/local is where
/// people put programs by hand).
pub fn is_packaged(exe: &Path) -> bool {
    exe.starts_with("/usr") && !exe.starts_with("/usr/local")
}

const CONTAINER: &str = "AmberSight is running in a container: update by building or pulling a new image";

/// Docker and Podman mark their containers with these files. An update
/// installed inside one would be lost when the container is recreated.
fn in_container() -> bool {
    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
}

/// Where the AmberSight package repository (apt and dnf) is described.
const REPO_PAGE: &str = "https://huskerminion.github.io/ambersight-packages/";

/// True when the apt source or dnf repository file made by the repository's
/// setup script exists, so `apt upgrade` / `dnf upgrade` bring new releases.
fn repo_configured(debian: bool) -> bool {
    let file = if debian { "/etc/apt/sources.list.d/ambersight.sources" } else { "/etc/yum.repos.d/ambersight.repo" };
    Path::new(file).exists()
}

/// How to update a deb/rpm install to release `r`: the repository command
/// when the repository is set up, else the package's own link and the
/// command (and a hint about the repository), so nobody has to hunt for them.
fn package_steps(r: &Release, debian: bool, repo: bool) -> String {
    if repo {
        let cmd = if debian { "sudo apt update && sudo apt upgrade" } else { "sudo dnf upgrade --refresh" };
        return format!(
            "AmberSight was installed as a package from the AmberSight repository, so the package manager updates it: run: {cmd} (version {} appears there shortly after its release)",
            r.version
        );
    }
    let arm = cfg!(target_arch = "aarch64");
    let name = if debian {
        format!("ambersight_{}_{}.deb", r.version, if arm { "arm64" } else { "amd64" })
    } else {
        format!("ambersight-{}-1.{}.rpm", r.version, if arm { "aarch64" } else { "x86_64" })
    };
    let url = r.asset(&name).map(|a| a.url.clone()).unwrap_or_else(|| r.notes_url.clone());
    let cmd = if debian { format!("sudo apt install ./{name}") } else { format!("sudo dnf install ./{name}") };
    format!(
        "AmberSight was installed as a package, so the package manager updates it: download {name} ({url}) and run: {cmd}. To get updates with apt/dnf from now on, set up the AmberSight repository once: {REPO_PAGE}"
    )
}

/// Why this program cannot replace itself, if it cannot.
fn path_blocker(exe: &Path) -> Option<String> {
    if in_container() {
        return Some(CONTAINER.to_string());
    }
    if is_packaged(exe) {
        return Some(PACKAGED.to_string());
    }
    let dir = exe.parent()?;
    if !dir_writable(dir) {
        return Some(format!("AmberSight cannot write to its program folder ({}); install the update by hand", dir.display()));
    }
    None
}

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".ambersight-write-test-{}", std::process::id()));
    let ok = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// A shared library that ships next to the program (Windows DLLs, the AI
/// runtime on Mac/Linux).
fn is_library(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    !n.starts_with("._") && (n.ends_with(".dll") || n.ends_with(".dylib") || n.ends_with(".so") || n.contains(".so."))
}

fn program_name() -> &'static str {
    if cfg!(windows) {
        "ambersight.exe"
    } else {
        "ambersight"
    }
}

/// After an update: delete the `*.old` files the Windows update left next
/// to the program, half-written new files, and the download folder.
pub fn cleanup_after_update(data_dir: &Path) {
    let _ = std::fs::remove_dir_all(data_dir.join("update"));
    let Ok(exe) = program_path() else { return };
    let Some(dir) = exe.parent() else { return };
    let exe_name = exe.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let stale = match name.strip_suffix(".old") {
            Some(base) => base == exe_name || is_library(base),
            None => name.starts_with(".ambersight-new-"),
        };
        if stale && std::fs::remove_file(e.path()).is_ok() {
            tracing::info!("removed {} left over from an update", e.path().display());
        }
    }
}

// ---------------------------------------------------------------------
// What is known (shown in Settings > System > Status & updates)
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    /// The tag without its "v", as package names spell it.
    pub version: String,
    pub notes_url: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// An install in progress (or its end).
#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    /// idle, downloading, verifying, installing, restarting, installed
    /// (restart by hand) or failed.
    pub state: &'static str,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
    pub message: Option<String>,
}

const IDLE: Progress = Progress { state: "idle", done: 0, total: 0, error: None, message: None };

struct State {
    latest: Option<Release>,
    checked_at: Option<i64>,
    /// The last check's attempt (successful or not), for the 12 h timer.
    tried_at: i64,
    error: Option<String>,
    progress: Progress,
    /// Failure kinds already written to the log, and when (one line per
    /// kind per day).
    logged: Vec<(&'static str, i64)>,
    /// The newest version already announced in the log.
    announced: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State { latest: None, checked_at: None, tried_at: 0, error: None, progress: IDLE, logged: Vec::new(), announced: None });

/// Set by `service-run` (Windows) so a finished update can restart.
static UNDER_SERVICE: AtomicBool = AtomicBool::new(false);
static RESTART: AtomicBool = AtomicBool::new(false);
static RESTART_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub fn set_under_service() {
    UNDER_SERVICE.store(true, Ordering::Relaxed);
}

/// Started by a service manager that starts it again when it exits with an
/// error: the Windows service, systemd (INVOCATION_ID), our launchd job, or a
/// container (its restart policy; docs/docker.md asks for one).
pub fn under_service() -> bool {
    UNDER_SERVICE.load(Ordering::Relaxed)
        || in_container()
        || (cfg!(target_os = "linux") && std::env::var_os("INVOCATION_ID").is_some())
        || (cfg!(target_os = "macos") && std::env::var("XPC_SERVICE_NAME").is_ok_and(|n| n.starts_with("com.ambersight.")))
}

/// Resolves when a finished update asks the server to stop and restart.
pub async fn restart_requested() {
    loop {
        if RESTART.load(Ordering::SeqCst) {
            return;
        }
        RESTART_NOTIFY.notified().await;
    }
}

/// True once an update asked for a restart (the server then exits with an
/// error code so the service manager starts the new version).
pub fn restart_pending() -> bool {
    RESTART.load(Ordering::SeqCst)
}

/// Whether an update is being downloaded, checked or installed right now
/// (a restart then would cut it off half-way).
pub fn installing() -> bool {
    is_installing(STATE.lock().unwrap().progress.state)
}

fn is_installing(state: &str) -> bool {
    matches!(state, "downloading" | "verifying" | "installing")
}

/// Ask the server to stop so the service manager starts it again (see
/// `restart_pending`): after an update, or from Settings.
pub(crate) fn request_restart() {
    RESTART.store(true, Ordering::SeqCst);
    RESTART_NOTIFY.notify_one();
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Write a check failure to the log at most once a day per kind.
fn log_failure(kind: &'static str, msg: &str) {
    let t = now();
    {
        let mut st = STATE.lock().unwrap();
        match st.logged.iter_mut().find(|(k, _)| *k == kind) {
            Some(e) if t - e.1 < 86400 => return,
            Some(e) => e.1 = t,
            None => st.logged.push((kind, t)),
        }
    }
    tracing::warn!("update check: {msg}");
}

/// The update section of Settings > System > Status & updates (GET /api/update).
pub fn status_json(cfg: &UpdateConfig) -> serde_json::Value {
    let st = STATE.lock().unwrap();
    let latest = st.latest.as_ref();
    let available = latest.is_some_and(|r| is_newer(&r.version, CURRENT));
    let (os, arch) = machine();
    let asset = latest.and_then(|r| asset_name(&r.version, os, arch));
    let busy = !matches!(st.progress.state, "idle" | "failed");
    let why_not = if !available {
        None
    } else if busy {
        Some(match st.progress.state {
            "installed" => "the update is installed: restart AmberSight to use it".to_string(),
            "restarting" => "AmberSight is restarting".to_string(),
            _ => "an update is being installed".to_string(),
        })
    } else {
        latest.and_then(release_blocker)
    };
    serde_json::json!({
        "current": CURRENT,
        "latest": latest.map(|r| r.version.clone()),
        "tag": latest.map(|r| r.tag.clone()),
        "available": available,
        "checked_at": st.checked_at,
        "error": st.error,
        "asset": asset,
        "can_install": available && why_not.is_none(),
        "why_not": why_not,
        "check_enabled": cfg.check,
        "restarts_itself": under_service(),
        "repo": cfg.repo,
        "notes_url": latest.map(|r| r.notes_url.clone()),
        "progress": st.progress,
    })
}

/// Why this release cannot be installed here, if it cannot.
fn release_blocker(r: &Release) -> Option<String> {
    let exe = match program_path() {
        Ok(p) => p,
        Err(e) => return Some(e.to_string()),
    };
    if let Some(why) = path_blocker(&exe) {
        if why == PACKAGED {
            let debian = std::path::Path::new("/etc/debian_version").exists();
            return Some(package_steps(r, debian, repo_configured(debian)));
        }
        return Some(why);
    }
    let (os, arch) = machine();
    let Some(name) = asset_name(&r.version, os, arch) else {
        return Some(format!("no update packages are made for this kind of computer ({os}-{arch})"));
    };
    if r.asset(&name).is_none() {
        return Some(format!("version {} has no package for this computer ({name})", r.version));
    }
    if r.asset("SHA256SUMS").is_none() || r.asset("SHA256SUMS.sig").is_none() {
        return Some(format!("version {} is not signed (no SHA256SUMS.sig)", r.version));
    }
    None
}

// ---------------------------------------------------------------------
// Checking
// ---------------------------------------------------------------------

/// How long a check (one small reply) may take in all.
const CHECK_SECS: u64 = 30;

fn agent(total_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_global(Some(Duration::from_secs(total_secs)))
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// A GET with only our name and version in the User-Agent, allowed
/// `total_secs` in all.
fn get(url: &str, total_secs: u64) -> Result<ureq::http::Response<ureq::Body>, (&'static str, String)> {
    let resp = agent(total_secs)
        .get(url)
        .header("User-Agent", &format!("AmberSight/{CURRENT}"))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| ("network", format!("could not reach {}: {e}", host_of(url))))?;
    let code = resp.status().as_u16();
    if code == 404 {
        return Err(("not_found", "not found".to_string()));
    }
    let out_of_calls = resp.headers().get("x-ratelimit-remaining").is_some_and(|v| v.as_bytes() == b"0");
    if code == 429 || (code == 403 && out_of_calls) {
        return Err(("http", format!("{} limits how often it may be asked; AmberSight tries again later", host_of(url))));
    }
    if !(200..300).contains(&code) {
        return Err(("http", format!("{} answered HTTP {code}", host_of(url))));
    }
    Ok(resp)
}

fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| "the server".into())
}

pub(crate) fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)));
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// Where the latest release is described.
fn latest_url(repo: &str) -> String {
    match std::env::var("AMBERSIGHT_UPDATE_BASE_URL") {
        Ok(base) if !base.trim().is_empty() => format!("{}/latest.json", base.trim().trim_end_matches('/')),
        _ => format!("https://api.github.com/repos/{repo}/releases/latest"),
    }
}

/// Ask for the latest published release.
fn fetch_latest(repo: &str) -> Result<Release, (&'static str, String)> {
    if !valid_repo(repo) {
        return Err(("config", format!("[update] repo must look like owner/name, not \"{repo}\"")));
    }
    let mut resp = get(&latest_url(repo), CHECK_SECS).map_err(|(k, m)| match k {
        "not_found" => (k, format!("no published release found for {repo}")),
        _ => (k, m),
    })?;
    let body = resp.body_mut().with_config().limit(8 << 20).read_to_vec().map_err(|e| ("network", format!("reading the release failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|_| ("reply", "the release description is not valid JSON".to_string()))?;
    parse_release(&v).ok_or(("reply", "the latest release has no usable version tag".to_string()))
}

/// The parts of a GitHub release we use. Drafts, pre-releases and tags
/// that are not plain versions are not offered.
fn parse_release(v: &serde_json::Value) -> Option<Release> {
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let tag = v["tag_name"].as_str()?.to_string();
    parse_version(&tag)?;
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let assets = v["assets"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some(Asset { name: x["name"].as_str()?.to_string(), url: x["browser_download_url"].as_str()?.to_string(), size: x["size"].as_u64().unwrap_or(0) })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Release { tag, version, notes_url: v["html_url"].as_str().unwrap_or("").to_string(), assets })
}

/// Look for a new version now (blocking). The result is remembered for the
/// status page; failures are logged at most once a day per kind.
pub fn check(repo: &str) -> Result<Release> {
    STATE.lock().unwrap().tried_at = now();
    match fetch_latest(repo) {
        Ok(r) => {
            let announce = is_newer(&r.version, CURRENT);
            {
                let mut st = STATE.lock().unwrap();
                st.latest = Some(r.clone());
                st.checked_at = Some(now());
                st.error = None;
                if announce && st.announced.as_deref() != Some(&r.version) {
                    st.announced = Some(r.version.clone());
                } else {
                    drop(st);
                    return Ok(r);
                }
            }
            if in_container() {
                tracing::info!("AmberSight {} is available (this is {CURRENT}); this copy runs in a container: build or pull a new image", r.version);
            } else if program_path().is_ok_and(|p| is_packaged(&p)) {
                tracing::info!("AmberSight {} is available (this is {CURRENT}); this copy came from a package: update it with apt or dnf", r.version);
            } else {
                tracing::info!("AmberSight {} is available (this is {CURRENT}); install it from Settings > System > Status & updates", r.version);
            }
            Ok(r)
        }
        Err((kind, msg)) => {
            {
                let mut st = STATE.lock().unwrap();
                st.checked_at = Some(now());
                st.error = Some(msg.clone());
            }
            log_failure(kind, &msg);
            Err(anyhow!(msg))
        }
    }
}

/// Check every 12 hours while `[update] check` is on (read each time, so
/// turning it on or off needs no restart).
pub fn spawn_checker(settings: impl Fn() -> UpdateConfig + Send + Sync + 'static) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            let cfg = settings();
            let due = now() - STATE.lock().unwrap().tried_at >= CHECK_EVERY_SECS;
            if cfg.check && due {
                let _ = tokio::task::spawn_blocking(move || check(&cfg.repo)).await;
            }
            tokio::time::sleep(Duration::from_secs(1800)).await;
        }
    });
}

// ---------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------

/// Progress callback: (state, bytes done, bytes in all).
type Report<'a> = &'a dyn Fn(&'static str, u64, u64);

/// Download, check and put in place release `r`. Returns the new version.
/// Whatever happens, the download folder is removed afterwards.
pub fn install(r: &Release, data_dir: &Path, report: Report) -> Result<String> {
    let result = install_steps(r, data_dir, report);
    let _ = std::fs::remove_dir_all(data_dir.join("update"));
    result
}

fn install_steps(r: &Release, data_dir: &Path, report: Report) -> Result<String> {
    let exe = program_path()?;
    if let Some(why) = release_blocker(r) {
        bail!("{why}");
    }
    let (os, arch) = machine();
    let name = asset_name(&r.version, os, arch).ok_or_else(|| anyhow!("no package for this computer"))?;
    let asset = r.asset(&name).ok_or_else(|| anyhow!("{name} is missing from the release"))?;
    let total = asset.size.min(MAX_PACKAGE_BYTES);

    // The signed checksums first: nothing big is fetched for an unsigned release.
    report("downloading", 0, total);
    let sums = get_small(&r.asset("SHA256SUMS").unwrap().url, 1 << 20)?;
    let sig = get_small(&r.asset("SHA256SUMS.sig").unwrap().url, 4096)?;
    report("verifying", 0, total);
    verify_signature(&release_pubkey(), &sums, &sig)?;
    let table = parse_sums(&String::from_utf8_lossy(&sums));
    let want = table.get(&name).ok_or_else(|| anyhow!("{name} is not listed in the signed SHA256SUMS: refusing to install"))?;

    let work = data_dir.join("update");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("could not make {}", work.display()))?;
    let pkg = work.join(&name);
    download_checked(&asset.url, &pkg, want, total, &|d, t| report("downloading", d, t))?;

    report("installing", total, total);
    let staged = unpack(&pkg, &work.join("unpacked"))?;
    check_runs(&staged.program, &r.version)?;
    let dir = exe.parent().ok_or_else(|| anyhow!("the program has no folder"))?;
    let mut moves: Vec<(PathBuf, PathBuf)> = staged.libs.iter().map(|l| (l.clone(), dir.join(l.file_name().unwrap()))).collect();
    moves.push((staged.program.clone(), exe.clone()));
    keep_previous(&moves, &dir.join("previous"))?;
    put_in_place(&moves, &dir.join("previous"), cfg!(windows))?;
    let _ = std::fs::remove_dir_all(&work);
    tracing::info!("AmberSight {} installed over {CURRENT} (the old program is kept in {})", r.version, dir.join("previous").display());
    #[cfg(windows)]
    note_version_for_windows(dir, &r.version);
    Ok(r.version.clone())
}

/// Installed with the Windows installer: show the new version in Add/Remove
/// Programs too (only when its entry is for this folder; failures ignored).
#[cfg(windows)]
fn note_version_for_windows(dir: &Path, version: &str) {
    use std::os::windows::process::CommandExt;
    const NO_WINDOW: u32 = 0x0800_0000;
    const KEY: &str = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\AmberSight NVR";
    let Ok(o) = std::process::Command::new("reg").args(["query", KEY, "/v", "InstallLocation"]).creation_flags(NO_WINDOW).output() else { return };
    let text = String::from_utf8_lossy(&o.stdout);
    // (The program's path may carry the \\?\ prefix canonicalize adds.)
    let here = dir.to_string_lossy().trim_start_matches(r"\\?\").trim_end_matches('\\').to_lowercase();
    let ours = text.lines().any(|l| l.trim_start().starts_with("InstallLocation") && l.trim_end().trim_end_matches('\\').to_lowercase().ends_with(&here));
    if o.status.success() && ours {
        let _ = std::process::Command::new("reg").args(["add", KEY, "/v", "DisplayVersion", "/t", "REG_SZ", "/d", version, "/f"]).creation_flags(NO_WINDOW).output();
    }
}

/// A small download (checksums, signature) into memory.
pub(crate) fn get_small(url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut resp = get(url, 120).map_err(|(_, m)| anyhow!("download failed: {m}"))?;
    resp.body_mut().with_config().limit(limit).read_to_vec().map_err(|e| anyhow!("download failed: {e}"))
}

/// Download `url` to `dest` (through a .part file), keeping it only when
/// its SHA-256 is `sha256` and it is no bigger than the limit.
pub(crate) fn download_checked(url: &str, dest: &Path, sha256: &str, total: u64, progress: &dyn Fn(u64, u64)) -> Result<()> {
    let mut resp = get(url, DOWNLOAD_SECS).map_err(|(_, m)| anyhow!("download failed: {m}"))?;
    let rd = resp.body_mut().with_config().limit(MAX_PACKAGE_BYTES + 1).reader();
    save_checked(rd, dest, sha256, total, progress)
}

fn save_checked(mut rd: impl std::io::Read, dest: &Path, sha256: &str, total: u64, progress: &dyn Fn(u64, u64)) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let tmp = dest.with_extension("part");
    let result = (|| -> Result<()> {
        let mut out = std::fs::File::create(&tmp).with_context(|| format!("could not write {}", tmp.display()))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            let n = rd.read(&mut buf).context("download interrupted")?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > MAX_PACKAGE_BYTES {
                bail!("the package is bigger than {} MB", MAX_PACKAGE_BYTES >> 20);
            }
            out.write_all(&buf[..n])?;
            h.update(&buf[..n]);
            progress(done, total.max(done));
        }
        out.sync_all()?;
        let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if !got.eq_ignore_ascii_case(sha256) {
            bail!("the package does not match its signed checksum: refusing to install");
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// The files taken from a package.
struct Staged {
    program: PathBuf,
    libs: Vec<PathBuf>,
}

/// A path inside an archive as safe parts: no absolute paths, no "..",
/// no drive letters. Windows zips may use backslashes.
pub(crate) fn safe_parts(name: &str) -> Option<Vec<String>> {
    let name = name.replace('\\', "/");
    if name.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for p in name.split('/') {
        match p {
            "" | "." => continue,
            ".." => return None,
            p if p.contains(':') => return None,
            p => parts.push(p.to_string()),
        }
    }
    (!parts.is_empty()).then_some(parts)
}

/// Unpack only the program and the libraries beside it into `out`.
fn unpack(pkg: &Path, out: &Path) -> Result<Staged> {
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(out)?;
    let lower = pkg.to_string_lossy().to_ascii_lowercase();
    // (path inside the package, where it was written)
    let mut found: Vec<(Vec<String>, PathBuf)> = Vec::new();
    let mut budget = MAX_UNPACKED_BYTES;
    let mut take = |parts: Vec<String>, rd: &mut dyn std::io::Read| -> Result<()> {
        let base = parts.last().unwrap();
        if base != program_name() && !is_library(base) {
            return Ok(());
        }
        let dest = out.join(format!("{}-{base}", found.len()));
        let mut f = std::fs::File::create(&dest)?;
        let n = std::io::copy(&mut std::io::Read::take(rd, budget + 1), &mut f)?;
        if n > budget {
            bail!("the package unpacks to more than {} MB", MAX_UNPACKED_BYTES >> 20);
        }
        budget -= n;
        f.sync_all()?;
        found.push((parts, dest));
        Ok(())
    };
    if lower.ends_with(".zip") {
        let mut z = zip::ZipArchive::new(std::fs::File::open(pkg)?).context("the package is not a readable zip")?;
        for i in 0..z.len() {
            let mut e = z.by_index(i)?;
            if !e.is_file() {
                continue;
            }
            let name = e.name()?.to_string();
            let parts = safe_parts(&name).ok_or_else(|| anyhow!("the package holds an unsafe path: {name}"))?;
            take(parts, &mut e)?;
        }
    } else if lower.ends_with(".tar.gz") {
        let mut t = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(pkg)?));
        for e in t.entries().context("the package is not a readable .tar.gz")? {
            let mut e = e.context("the package is not a readable .tar.gz")?;
            if e.header().entry_type() != tar::EntryType::Regular {
                continue;
            }
            let raw = e.path_bytes().into_owned();
            let name = String::from_utf8_lossy(&raw).to_string();
            let parts = safe_parts(&name).ok_or_else(|| anyhow!("the package holds an unsafe path: {name}"))?;
            take(parts, &mut e)?;
        }
    } else {
        bail!("unknown package type: {}", pkg.display());
    }
    choose_files(found)
}

/// The one program in the package, and the libraries in the same folder,
/// moved together into `<staging>/ready` (so the program finds its
/// libraries when it is tried).
fn choose_files(found: Vec<(Vec<String>, PathBuf)>) -> Result<Staged> {
    let progs: Vec<_> = found.iter().filter(|(p, _)| p.last().unwrap() == program_name()).collect();
    let [(prog_parts, prog)] = progs.as_slice() else {
        bail!("the package does not hold exactly one {} program", program_name());
    };
    let folder = &prog_parts[..prog_parts.len() - 1];
    let ready = prog.parent().unwrap().join("ready");
    std::fs::create_dir_all(&ready)?;
    let mut libs = Vec::new();
    for (parts, path) in &found {
        let base = parts.last().unwrap();
        let dest = ready.join(base);
        if parts[..parts.len() - 1] == *folder && is_library(base) && !dest.exists() {
            std::fs::rename(path, &dest)?;
            libs.push(dest);
        }
    }
    let program = ready.join(program_name());
    std::fs::rename(prog, &program)?;
    Ok(Staged { program, libs })
}

/// Run the new program with --version: it must start on this computer and
/// say it is the version the release claims.
fn check_runs(prog: &Path, version: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(prog, std::fs::Permissions::from_mode(0o755))?;
    }
    let mut c = std::process::Command::new(prog);
    c.arg("--version").stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = c.output().context("the new program does not start on this computer")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let says = text.split_whitespace().nth(1).unwrap_or("").to_string();
    if !out.status.success() || says != version {
        bail!("the new program does not report version {version} (it says \"{}\"): refusing to install", text.trim());
    }
    Ok(())
}

/// Copy the files about to be replaced into `previous/` (emptied first),
/// so the version in use can be put back by hand.
fn keep_previous(moves: &[(PathBuf, PathBuf)], prev: &Path) -> Result<()> {
    let _ = std::fs::remove_dir_all(prev);
    std::fs::create_dir_all(prev).with_context(|| format!("could not make {}", prev.display()))?;
    for (_, dest) in moves {
        if dest.is_file() {
            let to = prev.join(dest.file_name().unwrap());
            std::fs::copy(dest, &to).with_context(|| format!("could not keep a copy of {}", dest.display()))?;
        }
    }
    Ok(())
}

/// Put each new file (`from`) at its place (`to`). `rename_away` (Windows):
/// a file in use is renamed to `<name>.old` first; otherwise the new file
/// is written beside it and renamed over it. When one fails, the files
/// already replaced are put back from `prev`.
fn put_in_place(moves: &[(PathBuf, PathBuf)], prev: &Path, rename_away: bool) -> Result<()> {
    let mut done: Vec<&Path> = Vec::new();
    for (from, to) in moves {
        if let Err(e) = put_one(from, to, rename_away) {
            for d in done.iter().rev() {
                let back = prev.join(d.file_name().unwrap());
                let r = if back.is_file() { put_one(&back, d, rename_away) } else { std::fs::remove_file(d).map_err(Into::into) };
                if let Err(e2) = r {
                    tracing::error!("update: could not put {} back: {e2:#}", d.display());
                }
            }
            return Err(e.context(format!("could not replace {}", to.display())));
        }
        done.push(to);
    }
    Ok(())
}

fn put_one(from: &Path, to: &Path, rename_away: bool) -> Result<()> {
    let dir = to.parent().ok_or_else(|| anyhow!("no folder"))?;
    let name = to.file_name().unwrap().to_string_lossy().to_string();
    let tmp = dir.join(format!(".ambersight-new-{name}"));
    std::fs::copy(from, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    // Flush to disk before the rename. Windows needs write access for that
    // (FlushFileBuffers on a read-only handle is "Access is denied").
    std::fs::OpenOptions::new().write(true).open(&tmp).and_then(|f| f.sync_all())?;
    if rename_away && to.exists() {
        let old = dir.join(format!("{name}.old"));
        let _ = std::fs::remove_file(&old);
        if let Err(e) = std::fs::rename(to, &old) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, to) {
            let _ = std::fs::rename(&old, to);
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    } else if let Err(e) = std::fs::rename(&tmp, to) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Installing from the web page (in the background)
// ---------------------------------------------------------------------

fn set_progress(state: &'static str, done: u64, total: u64) {
    let mut st = STATE.lock().unwrap();
    st.progress = Progress { state, done, total, error: None, message: None };
}

/// Start installing the latest known release in the background. Refused
/// (with the reason) when there is nothing to install or it cannot be.
pub fn start_install(data_dir: PathBuf) -> Result<(), String> {
    let r = {
        let mut st = STATE.lock().unwrap();
        let Some(r) = st.latest.clone() else { return Err("check for updates first".into()) };
        if !is_newer(&r.version, CURRENT) {
            return Err(format!("AmberSight {CURRENT} is the latest version"));
        }
        if !matches!(st.progress.state, "idle" | "failed") {
            return Err("an update is already being installed".into());
        }
        st.progress = Progress { state: "downloading", ..IDLE };
        r
    };
    if let Some(why) = release_blocker(&r) {
        STATE.lock().unwrap().progress = IDLE;
        return Err(why);
    }
    tracing::info!("installing AmberSight {} (requested from the web page)", r.version);
    std::thread::spawn(move || {
        let result = install(&r, &data_dir, &set_progress);
        let mut st = STATE.lock().unwrap();
        match result {
            Ok(v) if under_service() => {
                st.progress = Progress { state: "restarting", done: 0, total: 0, error: None, message: Some(format!("AmberSight {v} is installed; restarting")) };
                drop(st);
                // Give the page a moment to see it, then stop cleanly; the
                // service manager starts the new program.
                std::thread::sleep(Duration::from_secs(2));
                tracing::info!("restarting to finish the update to {v}");
                request_restart();
            }
            Ok(v) => {
                st.progress = Progress { state: "installed", done: 0, total: 0, error: None, message: Some(format!("AmberSight {v} is installed. Restart AmberSight to start using it.")) };
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!("update to {} failed: {msg}", r.version);
                st.progress = Progress { state: "failed", done: 0, total: 0, error: Some(msg), message: None };
            }
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------
// The command line: `ambersight update`
// ---------------------------------------------------------------------

/// Check (and unless `check_only`, install) from a terminal, waiting for it.
pub fn cli(cfg: &UpdateConfig, data_dir: &Path, check_only: bool) -> Result<()> {
    cleanup_after_update(data_dir);
    let exe = program_path()?;
    println!("AmberSight {CURRENT} ({})", exe.display());
    let r = check(&cfg.repo).context("could not check for updates")?;
    if !is_newer(&r.version, CURRENT) {
        println!("this is the latest version (latest release: {})", r.version);
        return Ok(());
    }
    println!("AmberSight {} is available: {}", r.version, r.notes_url);
    if let Some(why) = release_blocker(&r) {
        bail!("{why}");
    }
    if check_only {
        println!("install it with: ambersight update --config <file>");
        return Ok(());
    }
    let last = std::sync::atomic::AtomicU64::new(u64::MAX);
    let v = install(&r, data_dir, &|state, done, total| {
        let pct = if total > 0 { done * 100 / total } else { 0 };
        let key = (state.len() as u64) << 32 | pct;
        if last.swap(key, Ordering::Relaxed) != key {
            if state == "downloading" && total > 0 {
                print!("\r{state} {pct}% of {} MB   ", total >> 20);
            } else {
                print!("\r{state}...                    ");
            }
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    });
    // End the progress line before the result (or the error).
    println!();
    let v = v?;
    let prev = exe.parent().map(|d| d.join("previous")).unwrap_or_default();
    println!("AmberSight {v} is installed in {}", exe.display());
    println!("the previous version is kept in {}", prev.display());
    println!("restart AmberSight to use it (as a service: ambersight service stop, then ambersight service start)");
    Ok(())
}

/// Web API for the Updates row of Settings > System > Status & updates (admins only, for reading
/// too: the middleware refuses /api/update* to others, and so does each
/// handler):
///
/// - `GET  /api/update`          what is known, and an install's progress
/// - `POST /api/update/check`    look for a new version now
/// - `POST /api/update/install`  install it (in the background)
pub mod http {
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Json, Response};
    use std::sync::Arc;

    use crate::engine::Engine;
    use crate::web::IsAdmin;

    fn refuse(code: StatusCode, msg: impl Into<String>) -> Response {
        (code, Json(serde_json::json!({ "error": msg.into() }))).into_response()
    }

    fn settings(engine: &Engine) -> super::UpdateConfig {
        engine.config.lock().unwrap().update.clone()
    }

    pub async fn get(State(engine): State<Arc<Engine>>, axum::Extension(admin): axum::Extension<IsAdmin>) -> Response {
        if !admin.0 {
            return refuse(StatusCode::FORBIDDEN, "admin access required");
        }
        Json(super::status_json(&settings(&engine))).into_response()
    }

    pub async fn check(State(engine): State<Arc<Engine>>, axum::Extension(admin): axum::Extension<IsAdmin>) -> Response {
        if !admin.0 {
            return refuse(StatusCode::FORBIDDEN, "admin access required");
        }
        let cfg = settings(&engine);
        let repo = cfg.repo.clone();
        // A failure is in the status's "error"; the page shows it there.
        let _ = tokio::task::spawn_blocking(move || super::check(&repo)).await;
        Json(super::status_json(&cfg)).into_response()
    }

    pub async fn install(State(engine): State<Arc<Engine>>, axum::Extension(admin): axum::Extension<IsAdmin>) -> Response {
        if !admin.0 {
            return refuse(StatusCode::FORBIDDEN, "admin access required");
        }
        match super::start_install(engine.data_dir.clone()) {
            Ok(()) => Json(super::status_json(&settings(&engine))).into_response(),
            Err(why) => refuse(StatusCode::CONFLICT, why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_states_block_a_restart() {
        for st in ["downloading", "verifying", "installing"] {
            assert!(is_installing(st), "{st}");
        }
        for st in ["idle", "failed", "installed", "restarting"] {
            assert!(!is_installing(st), "{st}");
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10"), Some((0, 10, 0)));
        assert_eq!(parse_version("2"), Some((2, 0, 0)));
        assert_eq!(parse_version("1.2.3+build5"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3-beta.1"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version(""), None);
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("v9.0.0-rc1", "0.1.0"));
        assert!(!is_newer("nonsense", "0.1.0"));
        assert!(parse_version(CURRENT).is_some());
    }

    #[test]
    fn package_per_computer() {
        assert_eq!(asset_name("1.2.0", "windows", "x86_64").unwrap(), "ambersight-1.2.0-windows-x86_64.zip");
        assert_eq!(asset_name("1.2.0", "linux", "x86_64").unwrap(), "ambersight-1.2.0-linux-x86_64.tar.gz");
        assert_eq!(asset_name("1.2.0", "linux", "aarch64").unwrap(), "ambersight-1.2.0-linux-aarch64.tar.gz");
        assert_eq!(asset_name("1.2.0", "darwin", "arm64").unwrap(), "ambersight-1.2.0-darwin-arm64.tar.gz");
        assert_eq!(asset_name("1.2.0", "darwin", "x86_64").unwrap(), "ambersight-1.2.0-darwin-x86_64.tar.gz");
        assert!(asset_name("1.2.0", "windows", "aarch64").is_none());
        assert!(asset_name("1.2.0", "darwin", "aarch64").is_none());
        assert!(asset_name("1.2.0", "freebsd", "x86_64").is_none());
        let (os, arch) = machine();
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!((os, arch), ("linux", "x86_64"));
        }
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!((os, arch), ("darwin", "arm64"));
        }
        if cfg!(windows) {
            assert_eq!(os, "windows");
        }
    }

    #[test]
    fn sums_file_is_read_like_sha256sum_writes_it() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let text = format!("{a}  ambersight-1.0.0-linux-x86_64.tar.gz\n{b} *ambersight-1.0.0-windows-x86_64.zip\r\nnot a line\n{}  short\n\n", "c".repeat(10));
        let t = parse_sums(&text);
        assert_eq!(t.len(), 2);
        assert_eq!(t["ambersight-1.0.0-linux-x86_64.tar.gz"], a);
        assert_eq!(t["ambersight-1.0.0-windows-x86_64.zip"], "b".repeat(64));
    }

    #[test]
    fn signature_checks_with_a_fresh_key_pair() {
        let dir = temp("sign");
        let key = dir.join("k.pk8");
        let public = keygen(&key).unwrap();
        // Never overwritten.
        assert!(keygen(&key).unwrap_err().to_string().contains("not overwritten"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let sums = dir.join("SHA256SUMS");
        std::fs::write(&sums, format!("{}  ambersight-1.0.0-linux-x86_64.tar.gz\n", "a".repeat(64))).unwrap();
        let (sig_path, pk2) = sign_file(&key, &sums).unwrap();
        assert_eq!(pk2, public);
        assert_eq!(sig_path, dir.join("SHA256SUMS.sig"));
        let data = std::fs::read(&sums).unwrap();
        let sig = std::fs::read(&sig_path).unwrap();
        verify_signature(&public, &data, &sig).unwrap();
        // One changed byte in the file: refused.
        let mut bad = data.clone();
        bad[3] ^= 1;
        assert!(verify_signature(&public, &bad, &sig).is_err());
        // Another key: refused.
        let other = keygen(&dir.join("other.pk8")).unwrap();
        assert!(verify_signature(&other, &data, &sig).is_err());
        // Garbage signature: refused.
        assert!(verify_signature(&public, &data, b"bm90IGEgc2ln").is_err());
        assert!(verify_signature(&public, &data, b"%%%").is_err());
        // The built-in key is a real Ed25519 public key.
        use base64::Engine;
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(RELEASE_PUBKEY).unwrap().len(), 32);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_manager_installs_are_refused() {
        assert!(is_packaged(Path::new("/usr/bin/ambersight")));
        assert!(is_packaged(Path::new("/usr/lib/ambersight/ambersight")));
        assert!(!is_packaged(Path::new("/usr/local/bin/ambersight")));
        assert!(!is_packaged(Path::new("/opt/ambersight/ambersight")));
        assert!(!is_packaged(Path::new("/home/me/ambersight/ambersight")));
        assert!(!is_packaged(Path::new("/usrx/ambersight")));
        assert!(!is_packaged(Path::new("C:\\AmberSight\\ambersight.exe")));
        if in_container() {
            // Inside a container nothing replaces itself (tests built in Docker).
            assert_eq!(path_blocker(Path::new("/opt/ambersight/ambersight")).unwrap(), CONTAINER);
            return;
        }
        assert_eq!(path_blocker(Path::new("/usr/bin/ambersight")).unwrap(), PACKAGED);
        assert!(PACKAGED.contains("apt/dnf"));
        let r = Release { tag: "v9.9.9".into(), version: "9.9.9".into(), notes_url: "https://example.org/notes".into(), assets: vec![
            Asset { name: "ambersight_9.9.9_amd64.deb".into(), url: "https://example.org/a.deb".into(), size: 1 },
            Asset { name: "ambersight_9.9.9_arm64.deb".into(), url: "https://example.org/b.deb".into(), size: 1 },
            Asset { name: "ambersight-9.9.9-1.x86_64.rpm".into(), url: "https://example.org/a.rpm".into(), size: 1 },
            Asset { name: "ambersight-9.9.9-1.aarch64.rpm".into(), url: "https://example.org/b.rpm".into(), size: 1 },
        ] };
        let deb = package_steps(&r, true, false);
        assert!(deb.contains("sudo apt install ./ambersight_9.9.9_") && deb.contains("https://example.org/") && deb.contains(REPO_PAGE), "{deb}");
        let rpm = package_steps(&r, false, false);
        assert!(rpm.contains("sudo dnf install ./ambersight-9.9.9-1.") && rpm.contains(".rpm") && rpm.contains(REPO_PAGE), "{rpm}");
        // With the repository set up, the upgrade command is all that is needed.
        let deb = package_steps(&r, true, true);
        assert!(deb.contains("sudo apt update && sudo apt upgrade") && !deb.contains(".deb"), "{deb}");
        let rpm = package_steps(&r, false, true);
        assert!(rpm.contains("sudo dnf upgrade") && !rpm.contains(".rpm"), "{rpm}");
        // A folder we can write to is fine.
        let dir = temp("writable");
        assert!(path_blocker(&dir.join(program_name())).is_none());
        let _ = std::fs::remove_dir_all(&dir);
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            assert!(path_blocker(Path::new("/proc/ambersight")).unwrap().contains("cannot write"));
        }
    }

    #[test]
    fn release_json_is_read_and_unsuitable_ones_skipped() {
        let v = serde_json::json!({
            "tag_name": "v1.4.0", "draft": false, "prerelease": false,
            "html_url": "https://github.com/o/r/releases/tag/v1.4.0",
            "assets": [{"name": "SHA256SUMS", "browser_download_url": "https://x/SHA256SUMS", "size": 300}, {"name": "broken"}],
        });
        let r = parse_release(&v).unwrap();
        assert_eq!((r.tag.as_str(), r.version.as_str()), ("v1.4.0", "1.4.0"));
        assert_eq!(r.assets.len(), 1);
        assert_eq!(r.asset("SHA256SUMS").unwrap().size, 300);
        let mut d = v.clone();
        d["draft"] = true.into();
        assert!(parse_release(&d).is_none());
        let mut p = v.clone();
        p["prerelease"] = true.into();
        assert!(parse_release(&p).is_none());
        let mut t = v.clone();
        t["tag_name"] = "nightly".into();
        assert!(parse_release(&t).is_none());
        assert!(valid_repo("HuskerMinion/AmberSight") && valid_repo("a-b/c.d_e"));
        assert!(!valid_repo("a") && !valid_repo("a/b/c") && !valid_repo("../x") && !valid_repo("a/..") && !valid_repo("a b/c") && !valid_repo("a/?x"));
    }

    #[test]
    fn download_kept_only_with_the_right_checksum() {
        let dir = temp("dl");
        let data = vec![9u8; 100_000];
        let dest = dir.join("pkg.tar.gz");
        save_checked(&data[..], &dest, &sha(&data), data.len() as u64, &|_, _| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        std::fs::remove_file(&dest).unwrap();
        let e = save_checked(&data[..], &dest, &sha(b"x"), 0, &|_, _| {}).unwrap_err();
        assert!(e.to_string().contains("checksum"), "{e}");
        assert!(!dest.exists() && !dir.join("pkg.tar.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_paths_must_stay_inside() {
        assert_eq!(safe_parts("ambersight-1.0-linux-x86_64/ambersight").unwrap(), vec!["ambersight-1.0-linux-x86_64", "ambersight"]);
        assert_eq!(safe_parts("dir\\bin\\a.dll").unwrap(), vec!["dir", "bin", "a.dll"]);
        assert_eq!(safe_parts("./a/./b").unwrap(), vec!["a", "b"]);
        assert!(safe_parts("/etc/passwd").is_none());
        assert!(safe_parts("a/../../b").is_none());
        assert!(safe_parts("C:/Windows/x").is_none());
        assert!(safe_parts("").is_none());
        assert!(is_library("DirectML.dll") && is_library("libonnxruntime.so.1.20") && is_library("libx.dylib") && is_library("liby.so"));
        assert!(!is_library("README.md") && !is_library("._DirectML.dll") && !is_library("config.example.toml"));
    }

    fn make_tar_gz(path: &Path, files: &[(&str, &[u8])]) {
        let gz = flate2::write::GzEncoder::new(std::fs::File::create(path).unwrap(), flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (name, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, name, *body).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn unpack_takes_the_program_and_its_libraries() {
        let dir = temp("unpack");
        let pkg = dir.join("ambersight-1.0.0-linux-x86_64.tar.gz");
        let prog = program_name();
        make_tar_gz(&pkg, &[
            ("ambersight-1.0.0/README.md", b"readme"),
            (&format!("ambersight-1.0.0/{prog}"), b"new program"),
            ("ambersight-1.0.0/libonnxruntime.so.1", b"lib"),
            ("ambersight-1.0.0/other/libelse.so", b"not beside the program"),
        ]);
        let s = unpack(&pkg, &dir.join("out")).unwrap();
        assert_eq!(std::fs::read(&s.program).unwrap(), b"new program");
        assert_eq!(s.libs.len(), 1);
        assert_eq!(s.libs[0].file_name().unwrap(), "libonnxruntime.so.1");
        assert_eq!(std::fs::read(&s.libs[0]).unwrap(), b"lib");
        // Two programs, or none: refused.
        make_tar_gz(&pkg, &[(&format!("a/{prog}"), b"1"), (&format!("b/{prog}"), b"2")]);
        assert!(unpack(&pkg, &dir.join("out")).is_err());
        make_tar_gz(&pkg, &[("a/README.md", b"1")]);
        assert!(unpack(&pkg, &dir.join("out")).is_err());
        // A zip with Windows-style paths.
        let zpath = dir.join("ambersight-1.0.0-windows-x86_64.zip");
        {
            use std::io::Write;
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zpath).unwrap());
            let o = zip::write::SimpleFileOptions::default();
            w.start_file(format!("ambersight-1.0.0\\{prog}"), o).unwrap();
            w.write_all(b"zip program").unwrap();
            w.start_file("ambersight-1.0.0\\DirectML.dll", o).unwrap();
            w.write_all(b"dml").unwrap();
            w.finish().unwrap();
        }
        let s = unpack(&zpath, &dir.join("out")).unwrap();
        assert_eq!(std::fs::read(&s.program).unwrap(), b"zip program");
        assert_eq!(s.libs[0].file_name().unwrap(), "DirectML.dll");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacing_keeps_the_previous_files_both_ways() {
        for rename_away in [false, true] {
            let dir = temp(&format!("replace-{rename_away}"));
            let app = dir.join("app");
            let new = dir.join("new");
            std::fs::create_dir_all(&app).unwrap();
            std::fs::create_dir_all(&new).unwrap();
            std::fs::write(app.join("ambersight"), b"old program").unwrap();
            std::fs::write(app.join("DirectML.dll"), b"old dll").unwrap();
            std::fs::write(new.join("ambersight"), b"new program").unwrap();
            std::fs::write(new.join("DirectML.dll"), b"new dll").unwrap();
            std::fs::write(new.join("extra.dll"), b"new extra").unwrap();
            let moves: Vec<(PathBuf, PathBuf)> = ["DirectML.dll", "extra.dll", "ambersight"].iter().map(|n| (new.join(n), app.join(n))).collect();
            let prev = app.join("previous");
            keep_previous(&moves, &prev).unwrap();
            put_in_place(&moves, &prev, rename_away).unwrap();
            assert_eq!(std::fs::read(app.join("ambersight")).unwrap(), b"new program");
            assert_eq!(std::fs::read(app.join("DirectML.dll")).unwrap(), b"new dll");
            assert_eq!(std::fs::read(app.join("extra.dll")).unwrap(), b"new extra");
            assert_eq!(std::fs::read(prev.join("ambersight")).unwrap(), b"old program");
            assert_eq!(std::fs::read(prev.join("DirectML.dll")).unwrap(), b"old dll");
            assert!(!prev.join("extra.dll").exists());
            assert_eq!(app.join("ambersight.old").exists(), rename_away);
            assert!(!app.join(".ambersight-new-ambersight").exists());

            // A file that cannot be put in place: the ones done are put back.
            std::fs::write(app.join("ambersight"), b"old program").unwrap();
            std::fs::write(app.join("DirectML.dll"), b"old dll").unwrap();
            let _ = std::fs::remove_file(app.join("extra.dll"));
            let moves = vec![(new.join("DirectML.dll"), app.join("DirectML.dll")), (new.join("extra.dll"), app.join("extra.dll")), (new.join("missing"), app.join("ambersight"))];
            keep_previous(&moves, &prev).unwrap();
            assert!(put_in_place(&moves, &prev, rename_away).is_err());
            assert_eq!(std::fs::read(app.join("DirectML.dll")).unwrap(), b"old dll");
            assert_eq!(std::fs::read(app.join("ambersight")).unwrap(), b"old program");
            assert!(!app.join("extra.dll").exists());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn update_settings_default_to_checking() {
        let c: UpdateConfig = toml::from_str("").unwrap();
        assert_eq!(c, UpdateConfig::default());
        assert!(c.check && c.repo == "HuskerMinion/AmberSight" && c.is_default());
        let c: UpdateConfig = toml::from_str("check = false").unwrap();
        assert!(!c.check && !c.is_default());
    }
}
