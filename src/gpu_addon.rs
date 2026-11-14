//! The NVIDIA add-on for the built-in AI on 64-bit Linux PCs.
//!
//! ONNX Runtime is built into the program, but its CUDA part is two separate
//! libraries (libonnxruntime_providers_shared.so, a few KB, and
//! libonnxruntime_providers_cuda.so, about 116 MB). Packages leave them out:
//! most computers have no NVIDIA card. When one has a card (`gpu::list`),
//! NVIDIA's CUDA libraries are installed and the AI should use the card,
//! AmberSight downloads them once, as the release's
//! `ambersight-cuda-<version>-linux-x86_64.tar.gz`, checked like an update
//! (the release's SHA256SUMS must carry the release signature and list the
//! archive with its SHA-256), into `<data folder>/tools/cuda-<version>/`. Not
//! with `[update] check = false`: then the owner puts them there by hand.
//!
//! Which copy is used: one beside the program that belongs to root and only
//! root may change (the CUDA container image), else this version's copy in
//! the data folder, else one beside the program that belongs to this user (a
//! build folder). A copy (its folder and both files) that belongs to another
//! user, that others may change, or that is a symbolic link is never loaded;
//! root loads only a root-owned copy beside the program, never the data
//! folder's, and downloads nothing.
//!
//! Where ONNX Runtime loads them from: it does not search for them. It takes
//! the folder of the module it is built into (dladdr), which for a program
//! with the runtime built in is the folder in the program's argv[0], and
//! loads `<that folder>/libonnxruntime_providers_shared.so` by its full path.
//! Loading the library from elsewhere first does not help (a full path is
//! matched by name, and loading it outside ONNX Runtime leaves it
//! uninitialized). So while a CUDA session is made, argv[0] reads
//! `<add-on folder>/ambersight`, and afterwards the real one again. When that
//! cannot be done, no CUDA session is made (ONNX Runtime would otherwise
//! look beside argv[0], even in the current folder).
//!
//! The CUDA libraries the add-on needs (CUDA 13 runtime, cuBLAS, cuRAND and
//! cuDNN 9) come from NVIDIA and are installed by the owner (docs/install.md);
//! `missing_system_libs` names the first one not found. The card is found in
//! CUDA by its PCI address, so CUDA_DEVICE_ORDER and CUDA_VISIBLE_DEVICES do
//! not mix the cards up.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// The two libraries, in the order they load.
pub const LIBS: [&str; 2] = ["libonnxruntime_providers_shared.so", "libonnxruntime_providers_cuda.so"];

/// The release asset holding them, as scripts/package-cuda-addon.sh names it.
pub fn archive_name(version: &str) -> String {
    format!("ambersight-cuda-{version}-linux-x86_64.tar.gz")
}

/// The add-on's folder for this version under the data folder.
pub fn folder(data_dir: &Path) -> PathBuf {
    crate::ffmpeg_bin::tools_dir(data_dir).join(format!("cuda-{}", crate::update::CURRENT))
}

fn has_libs(dir: &Path) -> bool {
    LIBS.iter().all(|l| dir.join(l).is_file())
}

fn euid() -> u32 {
    // SAFETY: geteuid cannot fail.
    unsafe { libc::geteuid() }
}

/// May this process load the copy in `dir`? The folder and both files must
/// belong to this user or root (only root when this is root) and must not be
/// writable by group or others. Err says why not.
fn trusted(dir: &Path, euid: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let check = |p: &Path| -> Result<(), String> {
        // (Not followed: a link could point anywhere, or be swapped later.)
        let m = std::fs::symlink_metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
        if m.file_type().is_symlink() {
            return Err(format!("{} is a symbolic link (put the real files there)", p.display()));
        }
        if !(m.uid() == 0 || (euid != 0 && m.uid() == euid)) {
            return Err(if euid == 0 {
                format!("{} does not belong to root (when running as root, run as the user AmberSight runs as, e.g. sudo -u ambersight)", p.display())
            } else {
                format!("{} belongs to another user (run this as the user AmberSight runs as)", p.display())
            });
        }
        if m.mode() & 0o022 != 0 {
            return Err(format!("{} can be changed by other users (chmod go-w)", p.display()));
        }
        Ok(())
    };
    check(dir)?;
    LIBS.iter().try_for_each(|l| check(&dir.join(l)))
}

/// What root is told: it loads only root's own copy beside the program.
const ROOT_NOTE: &str = "AmberSight is running as root, which loads the NVIDIA add-on only from beside the program (root's own copy): run it as its own user (e.g. sudo -u ambersight), or put the two .so files, owned by root, beside the program";

/// Root's own copy, which this process cannot change (unless it is root).
fn root_owned(dir: &Path) -> bool {
    // (For root, only root's own files pass.)
    trusted(dir, 0).is_ok()
}

// ---------------------------------------------------------------------
// argv[0]
// ---------------------------------------------------------------------

/// The program's argv, as the C library passes it to start-up functions
/// (Rust's standard library takes it the same way).
static ARGV: AtomicPtr<*const c_char> = AtomicPtr::new(std::ptr::null_mut());

#[used]
#[link_section = ".init_array"]
static AT_START: extern "C" fn(c_int, *mut *const c_char, *const *const c_char) = at_start;

/// Runs before main: keep argv.
extern "C" fn at_start(argc: c_int, argv: *mut *const c_char, _envp: *const *const c_char) {
    if argc > 0 && !argv.is_null() {
        ARGV.store(argv, Ordering::Relaxed);
    }
}

/// Run `f` (making a CUDA session) with argv[0] in `dir`, so ONNX Runtime
/// loads the add-on from there. Err (and `f` not run) when that cannot be
/// done: `dir` not absolute, or argv not known.
pub fn with_runtime_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> Result<T, String> {
    use std::os::unix::ffi::OsStrExt;
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    if !dir.is_absolute() {
        return Err(format!("the add-on folder {} is not a full path", dir.display()));
    }
    let fake = std::ffi::CString::new(dir.join("ambersight").as_os_str().as_bytes()).map_err(|_| format!("the add-on folder {} cannot be used", dir.display()))?;
    let argv = ARGV.load(Ordering::Relaxed);
    if argv.is_null() {
        return Err("the program's arguments are not known, so the NVIDIA add-on cannot be loaded safely".into());
    }
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // Left in memory for good: another thread reading the arguments in
    // between still finds a valid string.
    let fake: &'static std::ffi::CStr = Box::leak(fake.into_boxed_c_str());
    // SAFETY: argv[0] exists (argc > 0) and is a pointer-sized slot; it is
    // swapped atomically and always points at a valid string.
    let slot = unsafe { &*(argv as *const AtomicPtr<c_char>) };
    // Put back on the way out, a panic included.
    struct Restore<'a>(&'a AtomicPtr<c_char>, *mut c_char);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.0.store(self.1, Ordering::SeqCst);
        }
    }
    let _restore = Restore(slot, slot.swap(fake.as_ptr().cast_mut(), Ordering::SeqCst));
    Ok(f())
}

// ---------------------------------------------------------------------
// The card and NVIDIA's libraries
// ---------------------------------------------------------------------

/// The CUDA libraries from NVIDIA the add-on needs: the first one this
/// computer does not have, as a reason to show (None: all there).
pub fn missing_system_libs() -> Option<String> {
    const NEEDED: [(&str, &str); 6] = [
        ("libcuda.so.1", "install NVIDIA's driver (in a container: start it with --gpus all)"),
        ("libcudart.so.13", "install the CUDA 13 runtime (cuda-cudart-13) from NVIDIA (see the install guide)"),
        ("libcublasLt.so.13", "install cuBLAS for CUDA 13 (libcublas-13) from NVIDIA (see the install guide)"),
        ("libcublas.so.13", "install cuBLAS for CUDA 13 (libcublas-13) from NVIDIA (see the install guide)"),
        ("libcurand.so.10", "install cuRAND for CUDA 13 (libcurand-13) from NVIDIA (see the install guide)"),
        ("libcudnn.so.9", "install cuDNN 9 for CUDA 13 (libcudnn9-cuda-13) from NVIDIA (see the install guide)"),
    ];
    for (lib, what) in NEEDED {
        let name = std::ffi::CString::new(lib).unwrap();
        // SAFETY: a plain library load; kept loaded (ONNX Runtime loads it next).
        let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if h.is_null() {
            return Some(format!("{lib} not found: {what}"));
        }
    }
    None
}

/// CUDA's own number for the card at PCI address `bus` (CUDA may count the
/// cards in another order than nvidia-smi, and CUDA_VISIBLE_DEVICES may hide
/// or renumber them). Call after `missing_system_libs` found the runtime.
pub fn cuda_device(bus: &str) -> Result<i32, String> {
    type ByBus = unsafe extern "C" fn(*mut c_int, *const c_char) -> c_int;
    let lib = c"libcudart.so.13";
    // SAFETY: the CUDA runtime is already loaded; its documented function
    // takes an out-pointer and a C string.
    unsafe {
        let h = libc::dlopen(lib.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
        if h.is_null() {
            return Err("libcudart.so.13 not found".into());
        }
        let f = libc::dlsym(h, c"cudaDeviceGetByPCIBusId".as_ptr());
        if f.is_null() {
            return Err("libcudart.so.13 has no cudaDeviceGetByPCIBusId".into());
        }
        let f: ByBus = std::mem::transmute(f);
        let want = std::ffi::CString::new(bus).map_err(|_| format!("bad PCI address {bus:?}"))?;
        let mut dev: c_int = -1;
        match f(&mut dev, want.as_ptr()) {
            0 => Ok(dev),
            100 => Err("CUDA sees no card (CUDA_VISIBLE_DEVICES?)".into()),
            101 => Err(format!("CUDA does not see the card at {bus} (hidden by CUDA_VISIBLE_DEVICES?)")),
            35 => Err("NVIDIA's driver is too old for CUDA 13 (needs 580 or newer)".into()),
            e => {
                // CUDA's own words for it.
                type Text = unsafe extern "C" fn(c_int) -> *const c_char;
                let t = libc::dlsym(h, c"cudaGetErrorString".as_ptr());
                let text = if t.is_null() {
                    String::new()
                } else {
                    let t: Text = std::mem::transmute(t);
                    let p = t(e);
                    if p.is_null() { String::new() } else { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
                };
                Err(format!("CUDA could not open the card at {bus}: {} (error {e})", if text.is_empty() { "unknown error" } else { &text }))
            }
        }
    }
}

// ---------------------------------------------------------------------
// Finding it
// ---------------------------------------------------------------------

struct State {
    data_dir: Option<PathBuf>,
    repo: String,
    /// `[update] check`: off means no downloads.
    downloads: bool,
    /// A download (with its retries) is running.
    busy: bool,
    /// The AI still wants the card (cleared when it is loaded without one:
    /// retries then stop).
    wanted: bool,
    /// Shown in Settings while it downloads or after it failed.
    note: Option<String>,
    /// Called once the add-on is in place (reloads the AI).
    on_ready: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

static STATE: Mutex<State> = Mutex::new(State { data_dir: None, repo: String::new(), downloads: true, busy: false, wanted: false, note: None, on_ready: None });

/// Where to keep it, which GitHub repository releases come from, and
/// whether downloading is allowed (`[update] check`).
pub fn configure(data_dir: &Path, repo: &str, downloads: bool) {
    let mut st = STATE.lock().unwrap();
    st.data_dir = Some(data_dir.to_path_buf());
    st.repo = repo.to_string();
    st.downloads = downloads;
}

/// The AI was loaded without wanting a card (graphics card off, AI off, no
/// card): a download waiting to retry stops.
pub fn not_wanted() {
    STATE.lock().unwrap().wanted = false;
}

/// What to do once a download has put the add-on in place.
pub fn on_ready(f: impl Fn() + Send + Sync + 'static) {
    STATE.lock().unwrap().on_ready = Some(std::sync::Arc::new(f));
}

/// The folder holding a copy this process may load (see the top of this
/// file): Ok(None) when there is none, Err when the only one there may not
/// be loaded (the reason).
pub fn find() -> Result<Option<PathBuf>, String> {
    let data_dir = STATE.lock().unwrap().data_dir.clone();
    pick_copy(crate::update::program_path().ok().and_then(|p| p.parent().map(Path::to_path_buf)).as_deref(), data_dir.map(|d| folder(&d)).as_deref(), euid())
}

fn pick_copy(exe_dir: Option<&Path>, data_copy: Option<&Path>, euid: u32) -> Result<Option<PathBuf>, String> {
    let exe_dir = exe_dir.filter(|d| has_libs(d));
    if let Some(d) = exe_dir.filter(|d| root_owned(d)) {
        return Ok(Some(d.to_path_buf()));
    }
    // Root never loads a copy in a folder another user can change.
    if euid == 0 {
        return match data_copy.filter(|d| has_libs(d)) {
            Some(_) => Err(ROOT_NOTE.to_string()),
            None => Ok(None),
        };
    }
    let mut refused = None;
    if let Some(d) = data_copy.filter(|d| has_libs(d)) {
        match trusted(d, euid) {
            Ok(()) => return Ok(Some(d.to_path_buf())),
            Err(e) => refused = Some(e),
        }
    }
    if let Some(d) = exe_dir.filter(|d| trusted(d, euid).is_ok()) {
        return Ok(Some(d.to_path_buf()));
    }
    refused.map_or(Ok(None), |e| Err(format!("the NVIDIA add-on is not loaded: {e}")))
}

/// A download in progress or the last one's failure, for Settings (None
/// once a usable copy is there).
pub fn note() -> Option<String> {
    let st = STATE.lock().unwrap();
    let n = st.note.clone()?;
    let busy = st.busy;
    drop(st);
    if !busy && matches!(find(), Ok(Some(_))) {
        return None;
    }
    Some(n)
}

/// Why nothing is downloaded with `[update] check = false`, and what to do.
fn downloads_off_note(data_dir: &Path) -> String {
    format!(
        "the NVIDIA add-on is not downloaded because update checks are off ([update] check = false): put the two .so files from {} (this version's release) in {}",
        archive_name(crate::update::CURRENT),
        folder(data_dir).display()
    )
}

/// The wait before try `n` (1, 2, ...) after a failed download: 5 minutes,
/// 15 minutes, then hourly; None after a day of trying.
fn retry_wait(n: u32) -> Option<Duration> {
    match n {
        1 => Some(Duration::from_secs(300)),
        2 => Some(Duration::from_secs(900)),
        3..=24 => Some(Duration::from_secs(3600)),
        _ => None,
    }
}

/// Start downloading the add-on in the background (once at a time; a
/// failure is tried again, see `retry_wait`). Without a data folder
/// (commands that read no config) nothing happens; with downloads off, the
/// note says how to put it in place by hand.
pub fn start_download() {
    let (data_dir, repo) = {
        let mut st = STATE.lock().unwrap();
        let Some(d) = st.data_dir.clone() else { return };
        st.wanted = true;
        if euid() == 0 {
            st.note = Some(ROOT_NOTE.into());
            return;
        }
        if !st.downloads {
            st.note = Some(downloads_off_note(&d));
            return;
        }
        if st.busy {
            return;
        }
        st.busy = true;
        st.note = Some("downloading the NVIDIA add-on for the AI".into());
        (d, st.repo.clone())
    };
    let spawned = std::thread::Builder::new().name("cuda-addon".into()).spawn(move || {
        let mut tries = 0u32;
        let ok = loop {
            tries += 1;
            if download(&data_dir, &repo, &|_, _| {}).is_ok() {
                break true;
            }
            let Some(wait) = retry_wait(tries) else { break false };
            if let Some(n) = STATE.lock().unwrap().note.as_mut() {
                n.push_str(&format!("; trying again in {} minutes", wait.as_secs() / 60));
            }
            std::thread::sleep(wait);
            // Put in place by hand meanwhile, or downloads turned off.
            if matches!(find(), Ok(Some(_))) {
                break true;
            }
            let st = STATE.lock().unwrap();
            if !st.downloads || !st.wanted {
                break false;
            }
        };
        let ready = {
            let mut st = STATE.lock().unwrap();
            st.busy = false;
            if ok {
                st.note = None;
            }
            st.on_ready.clone()
        };
        if let (true, Some(f)) = (ok, ready) {
            f();
        }
    });
    if spawned.is_err() {
        let mut st = STATE.lock().unwrap();
        st.busy = false;
        st.note = None;
    }
}

/// `fetch`, with its progress and result kept for Settings and logged.
pub fn download(data_dir: &Path, repo: &str, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    let r = fetch(data_dir, repo, &|done, total| {
        STATE.lock().unwrap().note = Some(format!("downloading the NVIDIA add-on for the AI ({} MB so far)", done >> 20));
        progress(done, total);
    });
    let mut st = STATE.lock().unwrap();
    match &r {
        Ok(dir) => {
            st.note = None;
            tracing::info!("AI: NVIDIA add-on downloaded to {}", dir.display());
        }
        Err(e) => {
            st.note = Some(format!("the NVIDIA add-on could not be downloaded: {e:#}"));
            tracing::warn!("AI: the NVIDIA add-on could not be downloaded ({e:#}); the AI uses the CPU");
        }
    }
    r
}

/// Downloading is allowed (`[update] check`); else why not, for commands.
pub fn downloads_allowed(data_dir: &Path, check: bool) -> Result<(), String> {
    if check {
        Ok(())
    } else {
        Err(downloads_off_note(data_dir))
    }
}

/// Where the release's files are: `[update] repo`'s release of this version
/// on GitHub, or AMBERSIGHT_UPDATE_BASE_URL (tests), which holds them side
/// by side. The files are checked with the release key either way.
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

/// Download, check and unpack the add-on for this version into
/// `<data folder>/tools/cuda-<version>` (blocking); older versions' copies
/// are removed. One process at a time (a lock file in the tools folder);
/// never as root (see `pick_copy`).
pub fn fetch(data_dir: &Path, repo: &str, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let me = euid();
    // Root would leave files the service cannot use, and never loads them
    // anyway (see `pick_copy`): nothing is made or fetched.
    if me == 0 {
        bail!("{ROOT_NOTE}");
    }
    let base = release_base(repo)?;
    let name = archive_name(crate::update::CURRENT);
    let sums = crate::update::get_small(&format!("{base}/SHA256SUMS"), 1 << 20).context("SHA256SUMS of this version's release")?;
    let sig = crate::update::get_small(&format!("{base}/SHA256SUMS.sig"), 4096).context("SHA256SUMS.sig of this version's release")?;
    crate::update::verify_signature(&crate::update::release_pubkey(), &sums, &sig)?;
    let table = crate::update::parse_sums(&String::from_utf8_lossy(&sums));
    let want = table.get(&name).ok_or_else(|| anyhow!("{name} is not listed in the signed SHA256SUMS"))?;

    let tools = crate::ffmpeg_bin::tools_dir(data_dir);
    std::fs::create_dir_all(&tools).with_context(|| format!("could not make {}", tools.display()))?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(tools.join("cuda.lock")).context("could not open the add-on lock file")?;
    // SAFETY: flock on a file this function keeps open.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
        bail!("could not lock {}", tools.join("cuda.lock").display());
    }
    // Unlocked explicitly when this function returns, not by closing the file: a child process
    // another thread is just starting (forked, not yet running its program) holds a copy of
    // the descriptor, and the lock would last until that copy is closed.
    struct Unlock(std::fs::File);
    impl Drop for Unlock {
        fn drop(&mut self) {
            // SAFETY: the descriptor stays open for as long as self lives.
            unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN) };
        }
    }
    let lock = Unlock(file);
    let dest = folder(data_dir);
    // Another process may have put it in place while this one waited.
    if has_libs(&dest) && trusted(&dest, me).is_ok() {
        return Ok(dest);
    }
    let work = tools.join(format!("cuda-{}.download-{}", crate::update::CURRENT, std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("could not make {}", work.display()))?;
    let result = (|| -> Result<()> {
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o755))?;
        let pkg = work.join(&name);
        crate::update::download_checked(&format!("{base}/{name}"), &pkg, want, 0, progress)?;
        unpack(&pkg, &work)?;
        std::fs::remove_file(&pkg)?;
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&work, &dest).with_context(|| format!("could not make {}", dest.display()))?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&work);
        return Err(e);
    }
    // Copies for other versions are of no use to this one; a download of
    // another live process is left alone.
    if let Ok(rd) = std::fs::read_dir(&tools) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with("cuda-") || e.path() == dest || !e.path().is_dir() {
                continue;
            }
            if let Some(pid) = n.rsplit_once(".download-").and_then(|(_, p)| p.parse::<u32>().ok()) {
                if pid != std::process::id() && Path::new(&format!("/proc/{pid}")).exists() {
                    continue;
                }
            }
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    drop(lock);
    Ok(dest)
}

/// Take only the two libraries (by name, from anywhere in the archive) into
/// `out` (mode 644); the archive must hold both.
fn unpack(pkg: &Path, out: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    const MAX_BYTES: u64 = 1 << 30;
    let mut t = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(pkg)?));
    let mut got = [false; 2];
    for e in t.entries().context("the add-on is not a readable .tar.gz")? {
        let mut e = e.context("the add-on is not a readable .tar.gz")?;
        if e.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let raw = e.path_bytes().into_owned();
        let path = String::from_utf8_lossy(&raw).to_string();
        let parts = crate::update::safe_parts(&path).ok_or_else(|| anyhow!("the add-on holds an unsafe path: {path}"))?;
        let Some(i) = LIBS.iter().position(|l| parts.last().is_some_and(|p| p == l)) else { continue };
        if got[i] {
            bail!("the add-on holds {} twice", LIBS[i]);
        }
        let dest = out.join(LIBS[i]);
        let mut f = std::fs::File::create(&dest).with_context(|| format!("could not write {}", dest.display()))?;
        let n = std::io::copy(&mut std::io::Read::take(&mut e, MAX_BYTES + 1), &mut f)?;
        if n > MAX_BYTES {
            bail!("{} is bigger than {} MB", LIBS[i], MAX_BYTES >> 20);
        }
        f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        f.sync_all()?;
        got[i] = true;
    }
    if let Some(i) = got.iter().position(|g| !g) {
        bail!("the add-on does not hold {}", LIBS[i]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-cuda-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tar_gz(dir: &Path, files: &[(&str, &[u8])]) -> PathBuf {
        let p = dir.join("a.tar.gz");
        let gz = flate2::write::GzEncoder::new(std::fs::File::create(&p).unwrap(), flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        p
    }

    /// A copy of the add-on (two small files) with these modes.
    fn copy(d: &Path, dir_mode: u32, file_mode: u32) -> PathBuf {
        std::fs::create_dir_all(d).unwrap();
        for l in LIBS {
            std::fs::write(d.join(l), b"x").unwrap();
            std::fs::set_permissions(d.join(l), std::fs::Permissions::from_mode(file_mode)).unwrap();
        }
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(dir_mode)).unwrap();
        d.to_path_buf()
    }

    #[test]
    fn unpacks_only_the_two_libraries() {
        let d = temp("only-two");
        let p = tar_gz(
            &d,
            &[
                ("ambersight-cuda/README.txt", b"hi"),
                ("ambersight-cuda/libonnxruntime_providers_shared.so", b"shared"),
                ("ambersight-cuda/libonnxruntime_providers_cuda.so", b"cuda"),
                ("ambersight-cuda/libonnxruntime_providers_tensorrt.so", b"no"),
            ],
        );
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        unpack(&p, &out).unwrap();
        assert!(has_libs(&out));
        assert_eq!(std::fs::read(out.join(LIBS[1])).unwrap(), b"cuda");
        assert_eq!(std::fs::metadata(out.join(LIBS[1])).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(!out.join("README.txt").exists() && !out.join("libonnxruntime_providers_tensorrt.so").exists());
    }

    #[test]
    fn refuses_an_incomplete_archive() {
        let d = temp("incomplete");
        let p = tar_gz(&d, &[("libonnxruntime_providers_shared.so", b"shared")]);
        let e = unpack(&p, &d).unwrap_err().to_string();
        assert!(e.contains("libonnxruntime_providers_cuda.so"), "{e}");
    }

    #[test]
    fn names() {
        assert_eq!(archive_name("0.7.3"), "ambersight-cuda-0.7.3-linux-x86_64.tar.gz");
        assert!(folder(Path::new("/var/lib/ambersight")).ends_with(format!("tools/cuda-{}", crate::update::CURRENT)));
    }

    #[test]
    fn only_safe_copies_are_loaded() {
        let me = euid();
        if me == 0 {
            return; // (the checks below are about another user's files)
        }
        let d = temp("trust");
        let good = copy(&d.join("good"), 0o755, 0o644);
        assert!(trusted(&good, me).is_ok());
        // Root may not load this user's files, nor any data folder copy.
        assert!(trusted(&good, 0).unwrap_err().contains("root"));
        assert!(pick_copy(None, Some(&good), 0).unwrap_err().contains("running as root"));
        assert_eq!(pick_copy(None, None, 0).unwrap(), None);
        // Links are refused, for the folder and for the files.
        let linked = d.join("linked");
        std::os::unix::fs::symlink(&good, &linked).unwrap();
        assert!(trusted(&linked, me).unwrap_err().contains("symbolic link"));
        let file_link = copy(&d.join("flink"), 0o755, 0o644);
        std::fs::remove_file(file_link.join(LIBS[1])).unwrap();
        std::os::unix::fs::symlink(good.join(LIBS[1]), file_link.join(LIBS[1])).unwrap();
        assert!(trusted(&file_link, me).unwrap_err().contains("symbolic link"));
        // Another uid (the files are not theirs, nor root's).
        assert!(trusted(&good, me + 1).unwrap_err().contains("another user"));
        let group_file = copy(&d.join("gfile"), 0o755, 0o664);
        assert!(trusted(&group_file, me).unwrap_err().contains("other users"));
        let group_dir = copy(&d.join("gdir"), 0o775, 0o644);
        assert!(trusted(&group_dir, me).is_err());

        // Data folder copy first (the program folder's is not root's).
        let exe = copy(&d.join("exe"), 0o755, 0o644);
        assert_eq!(pick_copy(Some(&exe), Some(&good), me).unwrap(), Some(good.clone()));
        // An unsafe data copy is skipped for a safe program-folder copy...
        assert_eq!(pick_copy(Some(&exe), Some(&group_file), me).unwrap(), Some(exe.clone()));
        // ...and refused, with the reason, when it is the only one.
        assert!(pick_copy(None, Some(&group_file), me).unwrap_err().contains("other users"));
        assert_eq!(pick_copy(None, Some(&d.join("none")), me).unwrap(), None);
    }

    #[test]
    fn retries_slow_down_then_stop() {
        assert_eq!(retry_wait(1), Some(Duration::from_secs(300)));
        assert_eq!(retry_wait(2), Some(Duration::from_secs(900)));
        assert_eq!(retry_wait(3), Some(Duration::from_secs(3600)));
        assert_eq!(retry_wait(24), Some(Duration::from_secs(3600)));
        assert_eq!(retry_wait(25), None);
    }

    #[test]
    fn downloads_off_says_how_to_install_by_hand() {
        assert!(downloads_allowed(Path::new("/d"), true).is_ok());
        let e = downloads_allowed(Path::new("/d"), false).unwrap_err();
        assert!(e.contains("check = false") && e.contains("ambersight-cuda-") && e.contains("/d/tools/cuda-"), "{e}");
    }

    #[test]
    fn argv0_is_put_back() {
        let before: Vec<String> = std::env::args().take(1).collect();
        let during = with_runtime_dir(Path::new("/opt/addon"), || std::env::args().next()).unwrap();
        assert_eq!(during.as_deref(), Some("/opt/addon/ambersight"));
        assert_eq!(std::env::args().take(1).collect::<Vec<_>>(), before);
        // A relative folder is refused, and nothing runs (ONNX Runtime would read beside the real argv[0]).
        let mut ran = false;
        assert!(with_runtime_dir(Path::new("addon"), || ran = true).is_err());
        assert!(!ran);
    }
}
