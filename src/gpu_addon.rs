//! The graphics-card add-ons for the built-in AI on 64-bit Linux PCs: the
//! NVIDIA one (CUDA) and the Intel one (OpenVINO).
//!
//! ONNX Runtime is built into the program, but its CUDA part is two separate
//! libraries (libonnxruntime_providers_shared.so, a few KB, and
//! libonnxruntime_providers_cuda.so, about 116 MB), and its OpenVINO part is
//! libonnxruntime_providers_openvino.so with Intel's OpenVINO runtime and its
//! GPU plugin (see `Addon::libs`). Packages leave them out: most computers
//! have neither card. When one has a card (`gpu::list`), the card's own
//! libraries are installed (NVIDIA's CUDA libraries, or Intel's compute
//! driver) and the AI should use the card, AmberSight downloads the add-on
//! once, as the release's `ambersight-cuda-<version>-linux-x86_64.tar.gz`
//! or `ambersight-openvino-<version>-linux-x86_64.tar.gz`, checked like an
//! update (the release's SHA256SUMS must carry the release signature and
//! list the archive with its SHA-256), into `<data folder>/tools/cuda-<version>/`
//! or `<data folder>/tools/openvino-<version>/`. Not with `[update] check =
//! false`: then the owner puts the files there by hand.
//!
//! Which copy is used: one beside the program that belongs to root and only
//! root may change (a container image), else this version's copy in the
//! data folder, else one beside the program that belongs to this user (a
//! build folder). A copy (its folder and every file) that belongs to another
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
//! uninitialized). So while a CUDA or OpenVINO session is made, argv[0]
//! reads `<add-on folder>/ambersight`, and afterwards the real one again.
//! When that cannot be done, no such session is made (ONNX Runtime would
//! otherwise look beside argv[0], even in the current folder). The OpenVINO
//! libraries its provider needs are loaded from the add-on folder first, by
//! their full paths (`openvino::preload`); OpenVINO finds its GPU plugin
//! beside its own library.
//!
//! The CUDA libraries the NVIDIA add-on needs (CUDA 13 runtime, cuBLAS,
//! cuRAND and cuDNN 9) come from NVIDIA and are installed by the owner
//! (docs/install.md); `missing_system_libs` names the first one not found.
//! The card is found in CUDA by its PCI address, so CUDA_DEVICE_ORDER and
//! CUDA_VISIBLE_DEVICES do not mix the cards up. The Intel add-on needs
//! Intel's compute driver (intel-opencl-icd); `openvino::missing_driver`
//! says what is missing.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Which add-on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Addon {
    /// NVIDIA cards (CUDA).
    Cuda,
    /// Intel graphics (OpenVINO's GPU plugin, through Intel's compute driver).
    OpenVino,
}

impl Addon {
    /// Its libraries, in the order they load.
    pub fn libs(self) -> &'static [&'static str] {
        match self {
            Addon::Cuda => &["libonnxruntime_providers_shared.so", "libonnxruntime_providers_cuda.so"],
            Addon::OpenVino => &crate::openvino::LIBS,
        }
    }

    /// Its name in file and folder names.
    pub fn key(self) -> &'static str {
        match self {
            Addon::Cuda => "cuda",
            Addon::OpenVino => "openvino",
        }
    }

    /// For people.
    pub fn what(self) -> &'static str {
        match self {
            Addon::Cuda => "the NVIDIA add-on",
            Addon::OpenVino => "the Intel graphics add-on",
        }
    }

    /// The files to put in place by hand, for notes.
    fn files_phrase(self) -> &'static str {
        match self {
            Addon::Cuda => "the two .so files",
            Addon::OpenVino => "its library files",
        }
    }

    /// The release asset holding it, as scripts/package-cuda-addon.sh and
    /// scripts/package-openvino-addon.sh name it.
    pub fn archive_name(self, version: &str) -> String {
        format!("ambersight-{}-{version}-linux-x86_64.tar.gz", self.key())
    }

    /// Its folder for this version under the data folder.
    pub fn folder(self, data_dir: &Path) -> PathBuf {
        crate::ffmpeg_bin::tools_dir(data_dir).join(format!("{}-{}", self.key(), crate::update::CURRENT))
    }

    fn has_libs(self, dir: &Path) -> bool {
        self.libs().iter().all(|l| dir.join(l).is_file())
    }

    fn slot(self) -> usize {
        self as usize
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid cannot fail.
    unsafe { libc::geteuid() }
}

/// May this process use `p`? It must belong to this user or root (only root
/// when this is root), must not be a link and must not be writable by group
/// or others. Err says why not.
fn check_owned(p: &Path, euid: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
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
}

/// May this process load the copy in `dir`? The folder and every file must
/// pass `check_owned`.
fn trusted(addon: Addon, dir: &Path, euid: u32) -> Result<(), String> {
    check_owned(dir, euid)?;
    addon.libs().iter().try_for_each(|l| check_owned(&dir.join(l), euid))
}

/// A folder this process may keep its own files in (OpenVINO's cache of
/// compiled models, which it loads back): made when missing (mode 700),
/// and only used when it is a real folder of this very user (not root's, for
/// a service of its own) that others cannot change.
pub fn private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    if !dir.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    }
    owned_by(dir, euid())
}

/// `dir` is a real folder of the user `uid` (exactly: not root's for
/// another user) that others cannot change.
fn owned_by(dir: &Path, uid: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if m.file_type().is_symlink() || !m.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    if m.uid() != uid {
        return Err(format!("{} belongs to another user", dir.display()));
    }
    if m.mode() & 0o022 != 0 {
        return Err(format!("{} can be changed by other users (chmod go-w)", dir.display()));
    }
    Ok(())
}

/// What root is told: it loads only root's own copy beside the program.
fn root_note(addon: Addon) -> String {
    format!(
        "AmberSight is running as root, which loads {} only from beside the program (root's own copy): run it as its own user (e.g. sudo -u ambersight), or put {}, owned by root, beside the program",
        addon.what(),
        addon.files_phrase()
    )
}

/// Root's own copy, which this process cannot change (unless it is root).
fn root_owned(addon: Addon, dir: &Path) -> bool {
    // (For root, only root's own files pass.)
    trusted(addon, dir, 0).is_ok()
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

/// Run `f` (making a CUDA or OpenVINO session) with argv[0] in `dir`, so
/// ONNX Runtime loads the add-on from there. Err (and `f` not run) when that
/// cannot be done: `dir` not absolute, or argv not known.
pub fn with_runtime_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> Result<T, String> {
    use std::os::unix::ffi::OsStrExt;
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    if !dir.is_absolute() {
        return Err(format!("the add-on folder {} is not a full path", dir.display()));
    }
    let fake = std::ffi::CString::new(dir.join("ambersight").as_os_str().as_bytes()).map_err(|_| format!("the add-on folder {} cannot be used", dir.display()))?;
    let argv = ARGV.load(Ordering::Relaxed);
    if argv.is_null() {
        return Err("the program's arguments are not known, so the graphics card add-on cannot be loaded safely".into());
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

struct Shared {
    data_dir: Option<PathBuf>,
    repo: String,
    /// `[update] check`: off means no downloads.
    downloads: bool,
    /// Called once an add-on is in place (reloads the AI).
    on_ready: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

/// One add-on's download state.
struct State {
    /// A download (with its retries) is running.
    busy: bool,
    /// The AI still wants this card (cleared when it is loaded without one:
    /// retries then stop).
    wanted: bool,
    /// Shown in Settings while it downloads or after it failed.
    note: Option<String>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { data_dir: None, repo: String::new(), downloads: true, on_ready: None });
static STATE: [Mutex<State>; 2] = [Mutex::new(State { busy: false, wanted: false, note: None }), Mutex::new(State { busy: false, wanted: false, note: None })];

fn state(addon: Addon) -> std::sync::MutexGuard<'static, State> {
    STATE[addon.slot()].lock().unwrap_or_else(|e| e.into_inner())
}

/// Where to keep them, which GitHub repository releases come from, and
/// whether downloading is allowed (`[update] check`).
pub fn configure(data_dir: &Path, repo: &str, downloads: bool) {
    let mut st = SHARED.lock().unwrap();
    st.data_dir = Some(data_dir.to_path_buf());
    st.repo = repo.to_string();
    st.downloads = downloads;
}

/// The data folder `configure` was given.
pub fn data_dir() -> Option<PathBuf> {
    SHARED.lock().unwrap().data_dir.clone()
}

/// The AI was loaded without wanting this card (graphics card off, AI off,
/// no card, another card in use): a download waiting to retry stops.
pub fn not_wanted(addon: Addon) {
    state(addon).wanted = false;
}

/// Neither card is wanted.
pub fn none_wanted() {
    not_wanted(Addon::Cuda);
    not_wanted(Addon::OpenVino);
}

/// What to do once a download has put an add-on in place.
pub fn on_ready(f: impl Fn() + Send + Sync + 'static) {
    SHARED.lock().unwrap().on_ready = Some(std::sync::Arc::new(f));
}

/// The folder holding a copy this process may load (see the top of this
/// file): Ok(None) when there is none, Err when the only one there may not
/// be loaded (the reason).
pub fn find(addon: Addon) -> Result<Option<PathBuf>, String> {
    pick_copy(addon, crate::update::program_path().ok().and_then(|p| p.parent().map(Path::to_path_buf)).as_deref(), data_dir().map(|d| addon.folder(&d)).as_deref(), euid())
}

fn pick_copy(addon: Addon, exe_dir: Option<&Path>, data_copy: Option<&Path>, euid: u32) -> Result<Option<PathBuf>, String> {
    let exe_dir = exe_dir.filter(|d| addon.has_libs(d));
    if let Some(d) = exe_dir.filter(|d| root_owned(addon, d)) {
        return Ok(Some(d.to_path_buf()));
    }
    // Root never loads a copy in a folder another user can change.
    if euid == 0 {
        return match data_copy.filter(|d| addon.has_libs(d)) {
            Some(_) => Err(root_note(addon)),
            None => Ok(None),
        };
    }
    let mut refused = None;
    if let Some(d) = data_copy.filter(|d| addon.has_libs(d)) {
        match trusted(addon, d, euid) {
            Ok(()) => return Ok(Some(d.to_path_buf())),
            Err(e) => refused = Some(e),
        }
    }
    if let Some(d) = exe_dir.filter(|d| trusted(addon, d, euid).is_ok()) {
        return Ok(Some(d.to_path_buf()));
    }
    refused.map_or(Ok(None), |e| Err(format!("{} is not loaded: {e}", addon.what())))
}

/// A download in progress or the last one's failure, for Settings (None
/// once a usable copy is there).
pub fn note(addon: Addon) -> Option<String> {
    let st = state(addon);
    let n = st.note.clone()?;
    let busy = st.busy;
    drop(st);
    if !busy && matches!(find(addon), Ok(Some(_))) {
        return None;
    }
    Some(n)
}

/// Why nothing is downloaded with `[update] check = false`, and what to do.
fn downloads_off_note(addon: Addon, data_dir: &Path) -> String {
    format!(
        "{} is not downloaded because update checks are off ([update] check = false): put {} from {} (this version's release) in {}",
        addon.what(),
        addon.files_phrase(),
        addon.archive_name(crate::update::CURRENT),
        addon.folder(data_dir).display()
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
pub fn start_download(addon: Addon) {
    let (data_dir, repo, downloads) = {
        let sh = SHARED.lock().unwrap();
        let Some(d) = sh.data_dir.clone() else { return };
        (d, sh.repo.clone(), sh.downloads)
    };
    {
        let mut st = state(addon);
        st.wanted = true;
        if euid() == 0 {
            st.note = Some(root_note(addon));
            return;
        }
        if !downloads {
            st.note = Some(downloads_off_note(addon, &data_dir));
            return;
        }
        if st.busy {
            return;
        }
        st.busy = true;
        st.note = Some(format!("downloading {} for the AI", addon.what()));
    }
    let spawned = std::thread::Builder::new().name(format!("{}-addon", addon.key())).spawn(move || {
        let mut tries = 0u32;
        let ok = loop {
            tries += 1;
            if download(addon, &data_dir, &repo, &|_, _| {}).is_ok() {
                break true;
            }
            let Some(wait) = retry_wait(tries) else { break false };
            if let Some(n) = state(addon).note.as_mut() {
                n.push_str(&format!("; trying again in {} minutes", wait.as_secs() / 60));
            }
            std::thread::sleep(wait);
            // Put in place by hand meanwhile, or downloads turned off.
            if matches!(find(addon), Ok(Some(_))) {
                break true;
            }
            let downloads = SHARED.lock().unwrap().downloads;
            if !downloads || !state(addon).wanted {
                break false;
            }
        };
        {
            let mut st = state(addon);
            st.busy = false;
            if ok {
                st.note = None;
            }
        }
        let ready = SHARED.lock().unwrap().on_ready.clone();
        if let (true, Some(f)) = (ok, ready) {
            f();
        }
    });
    if spawned.is_err() {
        let mut st = state(addon);
        st.busy = false;
        st.note = None;
    }
}

/// `fetch`, with its progress and result kept for Settings and logged.
pub fn download(addon: Addon, data_dir: &Path, repo: &str, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    let r = fetch(addon, data_dir, repo, &|done, total| {
        state(addon).note = Some(format!("downloading {} for the AI ({} MB so far)", addon.what(), done >> 20));
        progress(done, total);
    });
    let mut st = state(addon);
    match &r {
        Ok(dir) => {
            st.note = None;
            tracing::info!("AI: {} downloaded to {}", addon.what(), dir.display());
        }
        Err(e) => {
            st.note = Some(format!("{} could not be downloaded: {e:#}", addon.what()));
            tracing::warn!("AI: {} could not be downloaded ({e:#}); the AI uses the CPU", addon.what());
        }
    }
    r
}

/// Downloading is allowed (`[update] check`); else why not, for commands.
pub fn downloads_allowed(addon: Addon, data_dir: &Path, check: bool) -> Result<(), String> {
    if check {
        Ok(())
    } else {
        Err(downloads_off_note(addon, data_dir))
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
/// `<data folder>/tools/<key>-<version>` (blocking); older versions' copies
/// are removed. One process at a time (a lock file in the tools folder);
/// never as root (see `pick_copy`).
pub fn fetch(addon: Addon, data_dir: &Path, repo: &str, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let me = euid();
    // Root would leave files the service cannot use, and never loads them
    // anyway (see `pick_copy`): nothing is made or fetched.
    if me == 0 {
        bail!("{}", root_note(addon));
    }
    let base = release_base(repo)?;
    let name = addon.archive_name(crate::update::CURRENT);
    let sums = crate::update::get_small(&format!("{base}/SHA256SUMS"), 1 << 20).context("SHA256SUMS of this version's release")?;
    let sig = crate::update::get_small(&format!("{base}/SHA256SUMS.sig"), 4096).context("SHA256SUMS.sig of this version's release")?;
    crate::update::verify_signature(&crate::update::release_pubkey(), &sums, &sig)?;
    let table = crate::update::parse_sums(&String::from_utf8_lossy(&sums));
    let want = table.get(&name).ok_or_else(|| anyhow!("{name} is not listed in the signed SHA256SUMS"))?;

    let tools = crate::ffmpeg_bin::tools_dir(data_dir);
    tools_folder(&tools)?;
    let lock_path = tools.join(format!("{}.lock", addon.key()));
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).context("could not open the add-on lock file")?;
    // SAFETY: flock on a file this function keeps open.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
        bail!("could not lock {}", lock_path.display());
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
    let dest = addon.folder(data_dir);
    // Another process may have put it in place while this one waited.
    if addon.has_libs(&dest) && trusted(addon, &dest, me).is_ok() {
        return Ok(dest);
    }
    let work = tools.join(format!("{}-{}.download-{}", addon.key(), crate::update::CURRENT, std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("could not make {}", work.display()))?;
    let result = (|| -> Result<()> {
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o755))?;
        let pkg = work.join(&name);
        crate::update::download_checked(&format!("{base}/{name}"), &pkg, want, 0, progress)?;
        unpack(addon, &pkg, &work)?;
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
    remove_other_versions(&tools, &format!("{}-", addon.key()), &dest);
    drop(lock);
    Ok(dest)
}

/// Make the tools folder (mode 755); an existing one of this user that others
/// may change loses that (a stricter one, such as 700, stays as it is).
fn tools_folder(tools: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o755).create(tools).with_context(|| format!("could not make {}", tools.display()))?;
    let m = std::fs::symlink_metadata(tools).with_context(|| format!("could not read {}", tools.display()))?;
    if m.uid() == euid() && m.mode() & 0o022 != 0 {
        std::fs::set_permissions(tools, std::fs::Permissions::from_mode(m.mode() & 0o7777 & !0o022)).with_context(|| format!("could not set the mode of {}", tools.display()))?;
    }
    Ok(())
}

/// Remove the folders in `parent` named `<prefix><something>` other than
/// `keep` (other versions' copies), leaving a download of another live
/// process alone.
pub fn remove_other_versions(parent: &Path, prefix: &str, keep: &Path) {
    let Ok(rd) = std::fs::read_dir(parent) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if !n.starts_with(prefix) || e.path() == keep || !e.path().is_dir() {
            continue;
        }
        // (Only a version number follows: "cuda-" never takes another add-on's folder.)
        if !n[prefix.len()..].starts_with(|c: char| c.is_ascii_digit()) {
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

/// Take only the add-on's libraries (by name, from anywhere in the archive)
/// into `out` (mode 644); the archive must hold every one.
fn unpack(addon: Addon, pkg: &Path, out: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    const MAX_BYTES: u64 = 1 << 30;
    let libs = addon.libs();
    let mut t = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(pkg)?));
    let mut got = vec![false; libs.len()];
    for e in t.entries().context("the add-on is not a readable .tar.gz")? {
        let mut e = e.context("the add-on is not a readable .tar.gz")?;
        if e.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let raw = e.path_bytes().into_owned();
        let path = String::from_utf8_lossy(&raw).to_string();
        let parts = crate::update::safe_parts(&path).ok_or_else(|| anyhow!("the add-on holds an unsafe path: {path}"))?;
        let Some(i) = libs.iter().position(|l| parts.last().is_some_and(|p| p == l)) else { continue };
        if got[i] {
            bail!("the add-on holds {} twice", libs[i]);
        }
        let dest = out.join(libs[i]);
        // (A new file only, never through a link: the folder is new and made by this process.)
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o644).custom_flags(libc::O_NOFOLLOW).open(&dest)
        }
        .with_context(|| format!("could not write {}", dest.display()))?;
        let n = std::io::copy(&mut std::io::Read::take(&mut e, MAX_BYTES + 1), &mut f)?;
        if n > MAX_BYTES {
            bail!("{} is bigger than {} MB", libs[i], MAX_BYTES >> 20);
        }
        f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        f.sync_all()?;
        got[i] = true;
    }
    if let Some(i) = got.iter().position(|g| !g) {
        bail!("the add-on does not hold {}", libs[i]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-addon-{name}-{}", std::process::id()));
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

    /// A copy of the add-on (small files) with these modes.
    fn copy(addon: Addon, d: &Path, dir_mode: u32, file_mode: u32) -> PathBuf {
        std::fs::create_dir_all(d).unwrap();
        for l in addon.libs() {
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
        unpack(Addon::Cuda, &p, &out).unwrap();
        assert!(Addon::Cuda.has_libs(&out));
        let libs = Addon::Cuda.libs();
        assert_eq!(std::fs::read(out.join(libs[1])).unwrap(), b"cuda");
        assert_eq!(std::fs::metadata(out.join(libs[1])).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(!out.join("README.txt").exists() && !out.join("libonnxruntime_providers_tensorrt.so").exists());
    }

    #[test]
    fn unpacks_the_intel_add_on_and_needs_every_library() {
        let d = temp("openvino");
        let libs = Addon::OpenVino.libs();
        let mut files: Vec<(String, Vec<u8>)> = libs.iter().map(|l| (format!("ambersight-openvino/{l}"), l.as_bytes().to_vec())).collect();
        files.push(("ambersight-openvino/licenses/LICENSE".into(), b"Apache".to_vec()));
        // A plugin the add-on does not use is left out.
        files.push(("ambersight-openvino/libopenvino_intel_cpu_plugin.so".into(), b"no".to_vec()));
        let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
        let p = tar_gz(&d, &refs);
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        unpack(Addon::OpenVino, &p, &out).unwrap();
        assert!(Addon::OpenVino.has_libs(&out));
        assert!(!out.join("libopenvino_intel_cpu_plugin.so").exists() && !out.join("LICENSE").exists());
        // One missing: refused, naming it.
        let short: Vec<(&str, &[u8])> = refs.iter().filter(|(n, _)| !n.ends_with("libopenvino_intel_gpu_plugin.so")).cloned().collect();
        let p = tar_gz(&d, &short);
        std::fs::create_dir_all(d.join("out2")).unwrap();
        let e = unpack(Addon::OpenVino, &p, &d.join("out2")).unwrap_err().to_string();
        assert!(e.contains("does not hold libopenvino_intel_gpu_plugin.so"), "{e}");
    }

    #[test]
    fn never_writes_through_a_link_or_over_a_file() {
        let d = temp("nofollow");
        let p = tar_gz(&d, &[("libonnxruntime_providers_shared.so", b"shared"), ("libonnxruntime_providers_cuda.so", b"cuda")]);
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let victim = d.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, out.join("libonnxruntime_providers_shared.so")).unwrap();
        assert!(unpack(Addon::Cuda, &p, &out).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        let out2 = d.join("out2");
        std::fs::create_dir_all(&out2).unwrap();
        std::fs::write(out2.join("libonnxruntime_providers_cuda.so"), b"old").unwrap();
        assert!(unpack(Addon::Cuda, &p, &out2).is_err());
    }

    #[test]
    fn the_tools_folder_is_not_writable_by_others() {
        let d = temp("tools");
        let t = d.join("tools");
        tools_folder(&t).unwrap();
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o777)).unwrap();
        tools_folder(&t).unwrap();
        assert_eq!(std::fs::metadata(&t).unwrap().permissions().mode() & 0o777, 0o755);
        // A stricter folder is not loosened.
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o700)).unwrap();
        tools_folder(&t).unwrap();
        assert_eq!(std::fs::metadata(&t).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn refuses_an_incomplete_archive() {
        let d = temp("incomplete");
        let p = tar_gz(&d, &[("libonnxruntime_providers_shared.so", b"shared")]);
        let e = unpack(Addon::Cuda, &p, &d).unwrap_err().to_string();
        assert!(e.contains("libonnxruntime_providers_cuda.so"), "{e}");
    }

    #[test]
    fn names() {
        assert_eq!(archive_name_cuda("0.7.3"), "ambersight-cuda-0.7.3-linux-x86_64.tar.gz");
        assert_eq!(Addon::OpenVino.archive_name("0.12.0"), "ambersight-openvino-0.12.0-linux-x86_64.tar.gz");
        assert!(Addon::Cuda.folder(Path::new("/var/lib/ambersight")).ends_with(format!("tools/cuda-{}", crate::update::CURRENT)));
        assert!(Addon::OpenVino.folder(Path::new("/var/lib/ambersight")).ends_with(format!("tools/openvino-{}", crate::update::CURRENT)));
    }

    fn archive_name_cuda(v: &str) -> String {
        Addon::Cuda.archive_name(v)
    }

    #[test]
    fn other_versions_are_removed_but_not_other_add_ons() {
        let d = temp("versions");
        for n in ["openvino-0.11.0", "openvino-0.12.0", "cuda-0.11.0", "openvino-cache-x"] {
            std::fs::create_dir_all(d.join(n)).unwrap();
        }
        remove_other_versions(&d, "openvino-", &d.join("openvino-0.12.0"));
        assert!(!d.join("openvino-0.11.0").exists());
        assert!(d.join("openvino-0.12.0").exists() && d.join("cuda-0.11.0").exists() && d.join("openvino-cache-x").exists());
    }

    #[test]
    fn only_safe_copies_are_loaded() {
        let me = euid();
        if me == 0 {
            return; // (the checks below are about another user's files)
        }
        for addon in [Addon::Cuda, Addon::OpenVino] {
            let d = temp(&format!("trust-{}", addon.key()));
            let libs = addon.libs();
            let last = libs[libs.len() - 1];
            let good = copy(addon, &d.join("good"), 0o755, 0o644);
            assert!(trusted(addon, &good, me).is_ok());
            // Root may not load this user's files, nor any data folder copy.
            assert!(trusted(addon, &good, 0).unwrap_err().contains("root"));
            assert!(pick_copy(addon, None, Some(&good), 0).unwrap_err().contains("running as root"));
            assert_eq!(pick_copy(addon, None, None, 0).unwrap(), None);
            // Links are refused, for the folder and for the files.
            let linked = d.join("linked");
            std::os::unix::fs::symlink(&good, &linked).unwrap();
            assert!(trusted(addon, &linked, me).unwrap_err().contains("symbolic link"));
            let file_link = copy(addon, &d.join("flink"), 0o755, 0o644);
            std::fs::remove_file(file_link.join(last)).unwrap();
            std::os::unix::fs::symlink(good.join(last), file_link.join(last)).unwrap();
            assert!(trusted(addon, &file_link, me).unwrap_err().contains("symbolic link"));
            // Another uid (the files are not theirs, nor root's).
            assert!(trusted(addon, &good, me + 1).unwrap_err().contains("another user"));
            let group_file = copy(addon, &d.join("gfile"), 0o755, 0o664);
            assert!(trusted(addon, &group_file, me).unwrap_err().contains("other users"));
            let group_dir = copy(addon, &d.join("gdir"), 0o775, 0o644);
            assert!(trusted(addon, &group_dir, me).is_err());

            // Data folder copy first (the program folder's is not root's).
            let exe = copy(addon, &d.join("exe"), 0o755, 0o644);
            assert_eq!(pick_copy(addon, Some(&exe), Some(&good), me).unwrap(), Some(good.clone()));
            // An unsafe data copy is skipped for a safe program-folder copy...
            assert_eq!(pick_copy(addon, Some(&exe), Some(&group_file), me).unwrap(), Some(exe.clone()));
            // ...and refused, with the reason, when it is the only one.
            assert!(pick_copy(addon, None, Some(&group_file), me).unwrap_err().contains("other users"));
            assert_eq!(pick_copy(addon, None, Some(&d.join("none")), me).unwrap(), None);
        }
        // A CUDA copy is not an OpenVINO one (it lacks OpenVINO's libraries).
        let d = temp("mixed");
        let cuda = copy(Addon::Cuda, &d.join("c"), 0o755, 0o644);
        assert_eq!(pick_copy(Addon::OpenVino, None, Some(&cuda), me).unwrap(), None);
    }

    #[test]
    fn the_cache_folder_must_be_private() {
        if euid() == 0 {
            return;
        }
        let d = temp("cache");
        let c = d.join("cache/openvino-x");
        private_dir(&c).unwrap();
        assert_eq!(std::fs::metadata(&c).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(private_dir(&c).unwrap_err().contains("other users"));
        let l = d.join("link");
        std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&c, &l).unwrap();
        assert!(private_dir(&l).is_err());
        // Only this user's own: not root's (nor anyone else's) for a non-root service.
        assert!(owned_by(&c, euid()).is_ok());
        assert!(owned_by(&c, 0).unwrap_err().contains("another user"));
        assert!(owned_by(Path::new("/"), euid()).unwrap_err().contains("another user"));
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
        assert!(downloads_allowed(Addon::Cuda, Path::new("/d"), true).is_ok());
        let e = downloads_allowed(Addon::Cuda, Path::new("/d"), false).unwrap_err();
        assert!(e.contains("check = false") && e.contains("ambersight-cuda-") && e.contains("/d/tools/cuda-"), "{e}");
        let e = downloads_allowed(Addon::OpenVino, Path::new("/d"), false).unwrap_err();
        assert!(e.contains("Intel") && e.contains("ambersight-openvino-") && e.contains("/d/tools/openvino-"), "{e}");
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
