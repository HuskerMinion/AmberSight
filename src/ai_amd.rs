//! The built-in AI on AMD graphics (64-bit Linux): a helper program, not a
//! library.
//!
//! AMD graphics run the model through ONNX Runtime's WebGPU provider (Dawn
//! over Vulkan, Mesa's RADV driver). That runtime cannot also run CUDA or
//! OpenVINO, so it cannot be the program's own: it is `ambersight-ai-amd`,
//! a program of its own from the AMD add-on (gpu_addon.rs, `Addon::Amd`),
//! which AmberSight starts and talks to over its stdin and stdout
//! (ai_amd_proto.rs). It sits behind the same card thread as every other
//! card (ai_card.rs), so the time limits, the rest after a slow check and
//! Settings' notes work as they do there; what is special is here:
//!
//! - The program is run by its full path from the add-on folder that
//!   gpu_addon.rs verified (never from anywhere else, never through a
//!   shell), with a cleared environment: `VK_DRIVER_FILES` (and the older
//!   `VK_ICD_FILENAMES`) naming Mesa's AMD driver only, so Vulkan uses the
//!   AMD chip and not an Intel or NVIDIA card; `HOME`, `XDG_CACHE_HOME` and
//!   `MESA_SHADER_CACHE_DIR` in `<data folder>/cache/amd-<version>/` (the
//!   compiled shaders are kept there, so later starts are quick); and
//!   `LD_LIBRARY_PATH` the add-on folder (its `libwebgpu_dawn.so`).
//! - It must say HELLO with this very version of AmberSight (the same
//!   protocol and the same version), then loads the model (LOAD, READY);
//!   one check is done before the card counts as usable.
//! - A check over the limit kills the program at once (`CardThread::on_stuck`),
//!   which frees the chip and ends the thread's wait. When the thread ends the
//!   program is told to QUIT, and killed when it has not left in a moment, and
//!   always waited for (no zombie).
//! - A helper that stops (a crash, a GPU reset) is started again for the next
//!   check, once: stopping again soon after leaves the card for the usual
//!   rest at once (`Helper`).
//! - Its log lines (stderr) go to the log at debug level, a few per ten
//!   seconds, without control characters.
//! - AMD graphics are not always faster than the processor: a small built-in
//!   chip can be several times slower. When the AI picked them itself (not
//!   chosen in Settings), a few checks are timed on the chip and on the
//!   processor, and the chip is used only when its median is at most 1.5 times
//!   the processor's (`judge`); the answer is kept until AmberSight restarts.

use crate::ai_amd_proto as proto;
use crate::ai_card::{CardThread, Input, LoadError, Output, LOAD_DEADLINE};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// What the card is called to the AI ("WebGPU": the API the helper uses).
pub const API: &str = "WebGPU";

/// The helper must say HELLO this soon after it starts.
const HELLO_WAIT: Duration = Duration::from_secs(30);
/// Checks timed on the chip and on the processor to compare them.
const SPEED_CHECKS: usize = 5;
/// The chip is used when its median check takes at most this much of the processor's.
const SLOWER_BY: f64 = 1.5;

// ---------------------------------------------------------------------
// The Vulkan driver and access to the card
// ---------------------------------------------------------------------

/// Where Vulkan drivers list themselves (the loader's own search path).
const ICD_DIRS: [&str; 3] = ["/etc/vulkan/icd.d", "/usr/local/share/vulkan/icd.d", "/usr/share/vulkan/icd.d"];

/// Whether `p` (a file or folder) belongs to `uid` and no one else can write to it.
fn owned_and_closed(p: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).is_ok_and(|m| m.uid() == uid && m.mode() & 0o022 == 0)
}

/// The driver file is handed to Vulkan, which loads the library it names into
/// the helper, so only what the system's owner (root) put there counts: the
/// folder and the file (after following a link, as packages that link a file
/// from another folder make) are owned by `uid` and not writable by group or
/// others. Distribution packages (Debian, Ubuntu, Fedora, Arch) ship plain
/// root-owned files, which pass.
fn icd_trusted(dir: &Path, file: &Path, uid: u32) -> bool {
    let Ok(real) = std::fs::canonicalize(file) else { return false };
    let ok = owned_and_closed(dir, uid) && owned_and_closed(&real, uid) && real.parent().is_some_and(|d| owned_and_closed(d, uid));
    if !ok {
        tracing::debug!("AMD Vulkan driver file {} is not used: it or its folder is not owned by root or can be written by others", file.display());
    }
    ok
}

/// Mesa's Vulkan driver for AMD (RADV): its `radeon_icd*.json`, the 64-bit
/// one (a 32-bit file, `radeon_icd.i686.json`, is no use here), owned by `uid`
/// (root). The first folder of `dirs` that has one wins.
fn find_icd_owned_by(dirs: &[&Path], uid: u32) -> Option<PathBuf> {
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| is_radeon_icd(n))
            .filter(|n| icd_trusted(dir, &dir.join(n), uid))
            .collect();
        // radeon_icd.x86_64.json, else radeon_icd.json, else the first by name.
        names.sort_by_key(|n| (!n.contains("x86_64"), n.clone()));
        if let Some(n) = names.first() {
            return Some(dir.join(n));
        }
    }
    None
}

/// A 64-bit `radeon_icd*.json` name.
fn is_radeon_icd(n: &str) -> bool {
    n.starts_with("radeon_icd") && n.ends_with(".json") && !n.contains("i686") && !n.contains("i386") && !n.contains("32")
}

/// The first AMD driver file there that `find_icd_owned_by` refused (it is
/// installed, but not as root left it), so the note does not ask for a
/// driver that is already there.
fn refused_icd(dirs: &[&Path]) -> Option<PathBuf> {
    dirs.iter().find_map(|dir| {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().filter(|e| is_radeon_icd(&e.file_name().to_string_lossy())).map(|e| e.path()).collect();
        v.sort();
        v.into_iter().next()
    })
}

/// What this computer lacks for the AI on AMD graphics, as a reason to show
/// (Ok: the path of Mesa's AMD Vulkan driver file, and this user may use the
/// card). `render`: the AMD chips' render nodes ("/dev/dri/renderD128").
pub fn driver(render: &[String]) -> Result<PathBuf, String> {
    let dirs: Vec<&Path> = ICD_DIRS.iter().map(Path::new).collect();
    driver_in(&dirs, render, crate::update::in_container(), 0)
}

fn driver_in(dirs: &[&Path], render: &[String], container: bool, uid: u32) -> Result<PathBuf, String> {
    let Some(icd) = find_icd_owned_by(dirs, uid) else {
        if let Some(f) = refused_icd(dirs) {
            return Err(format!(
                "the AMD Vulkan driver file {} is not used: it or its folder is not owned by root, or others can change it (reinstall Mesa's Vulkan drivers, or fix its owner and permissions)",
                f.display()
            ));
        }
        return Err(if container {
            format!("{DRIVER_MISSING}: the Docker images cannot run the AI on AMD graphics yet; install AmberSight from its package on the computer instead")
        } else {
            format!("{DRIVER_MISSING}: install Mesa's Vulkan drivers (Debian, Ubuntu and Fedora: mesa-vulkan-drivers; Arch: vulkan-radeon), see the install guide")
        });
    };
    // (Same rule as for the Intel card: the render node is read and written.)
    if !render.is_empty() && !render.iter().any(|n| crate::openvino::may_use(n)) {
        let who = std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "the user AmberSight runs as".into());
        return Err(format!(
            "this user may not use the AMD graphics ({} needs the render group): add {who} to it (sudo usermod -aG render {who}) and restart AmberSight",
            render.join(", ")
        ));
    }
    Ok(icd)
}

const DRIVER_MISSING: &str = "no Vulkan driver for AMD graphics is installed";

/// Whether `why` (from `driver`) is the Vulkan driver missing.
pub fn is_driver_missing(why: &str) -> bool {
    why.starts_with(DRIVER_MISSING)
}

/// `<data folder>/cache/amd-<version>`: the helper's home and shader cache.
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cache").join(format!("amd-{}", crate::update::CURRENT))
}

// ---------------------------------------------------------------------
// Starting the helper
// ---------------------------------------------------------------------

/// How to start the helper: its program, arguments and the whole
/// environment (nothing else is passed on).
pub struct Spec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

impl Spec {
    /// The add-on's program in `dir` (a folder gpu_addon.rs verified), with
    /// Mesa's AMD driver file `icd` and the cache folder `cache`.
    pub fn for_addon(dir: &Path, icd: &Path, cache: &Path) -> Spec {
        let e = |k: &str, v: &Path| (OsString::from(k), v.as_os_str().to_os_string());
        Spec {
            program: dir.join(crate::gpu_addon::AMD_HELPER),
            args: Vec::new(),
            env: vec![
                e("VK_DRIVER_FILES", icd),
                e("VK_ICD_FILENAMES", icd),
                e("HOME", cache),
                e("XDG_CACHE_HOME", cache),
                e("MESA_SHADER_CACHE_DIR", cache),
                e("LD_LIBRARY_PATH", dir),
                // Overlay layers (Steam, OBS, MangoHud) are implicit layers that load into every
                // Vulkan program; the helper has no use for them.
                (OsString::from("VK_LOADER_LAYERS_DISABLE"), OsString::from("~implicit~")),
            ],
        }
    }
}

/// The helper's process. Dropping it kills the process and waits for it.
struct Proc {
    child: Mutex<Child>,
}

impl Proc {
    fn kill(&self) {
        // (Child::kill does nothing for a process already waited for, so a
        // number that was reused is never signaled.)
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }

    /// Wait up to `wait` for it to leave (and so be reaped).
    fn reap(&self, wait: Duration) -> Option<ExitStatus> {
        let until = Instant::now() + wait;
        loop {
            match self.child.lock().unwrap_or_else(|e| e.into_inner()).try_wait() {
                Ok(Some(s)) => return Some(s),
                Ok(None) if Instant::now() < until => {}
                _ => return None,
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
        let _ = self.reap(Duration::from_secs(2));
    }
}

/// Kills the helper (what the card thread does to a check that is over the limit).
#[derive(Clone)]
pub struct Killer(Arc<Proc>);

impl Killer {
    pub fn kill(&self) {
        self.0.kill();
    }
}

type Frames = mpsc::Receiver<io::Result<(u8, Vec<u8>)>>;

/// A running helper that has loaded the model.
struct Session {
    proc: Arc<Proc>,
    stdin: Option<ChildStdin>,
    frames: Frames,
    /// The helper stopped, or its output ended: no check can be done with it now.
    broken: std::sync::atomic::AtomicBool,
}

/// What the helper said about the model once loaded.
struct Ready {
    input: usize,
    adapter: String,
}

/// A name from the helper for people: one line, no control characters, cut short.
fn plain(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(100).collect::<String>().trim().to_string()
}

impl Session {
    /// Start the helper, check its HELLO, load `model` and wait for READY,
    /// all before `until`. Any failure kills the helper (it is waited for);
    /// `slot` gets a way to kill it from another thread.
    fn start(spec: &Spec, model: &Path, until: Instant, slot: &Mutex<Option<Killer>>) -> Result<(Session, Ready), String> {
        let model = model.to_str().ok_or("the model's path is not text")?;
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args).env_clear().envs(spec.env.iter().map(|(k, v)| (k, v))).current_dir("/").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("could not start the AMD graphics helper: {e}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("the AMD graphics helper has no output")?;
        let stderr = child.stderr.take();
        let proc = Arc::new(Proc { child: Mutex::new(child) });
        // Given out at once: when the load hangs in a write that has no deadline,
        // the caller kills the helper with it (which ends that write).
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Killer(proc.clone()));
        if let Some(e) = stderr {
            let _ = std::thread::Builder::new().name("ai-amd-log".into()).spawn(move || log_lines(e));
        }
        let (tx, frames) = mpsc::channel();
        let reader = std::thread::Builder::new().name("ai-amd-read".into()).spawn(move || {
            let mut out = io::BufReader::new(stdout);
            loop {
                let f = proto::read_frame(&mut out);
                let bad = f.is_err();
                if tx.send(f).is_err() || bad {
                    return;
                }
            }
        });
        if let Err(e) = reader {
            return Err(format!("could not start the AMD graphics helper's reader: {e}"));
        }
        let mut s = Session { proc, stdin, frames, broken: Default::default() };
        let ready = s.handshake(model, until)?;
        Ok((s, ready))
    }

    fn handshake(&mut self, model: &str, until: Instant) -> Result<Ready, String> {
        let hello_by = until.min(Instant::now() + HELLO_WAIT);
        let (kind, p) = self.next(hello_by).map_err(|e| format!("the AMD graphics helper did not say hello: {e}"))?;
        if kind != proto::HELLO {
            return Err("the AMD graphics helper did not start with hello".into());
        }
        let protocol = proto::field(&p, "protocol").unwrap_or("?");
        let version = proto::field(&p, "version").unwrap_or("?");
        if protocol != proto::PROTOCOL.to_string() || version != crate::update::CURRENT {
            return Err(format!(
                "the AMD graphics helper is from another version (protocol {protocol}, AmberSight {version}; this is protocol {}, AmberSight {}): remove {} so the right one is downloaded",
                proto::PROTOCOL,
                crate::update::CURRENT,
                crate::gpu_addon::Addon::Amd.what()
            ));
        }
        // The helper runs in "/", so a relative model path (the default data
        // folder "amberdata" is one) is made whole from AmberSight's own folder.
        let model = std::path::absolute(model).map_err(|e| format!("the model's path {model}: {e}"))?;
        let model = model.to_str().ok_or("the model's path is not UTF-8")?;
        self.send(proto::LOAD, &proto::fields(&[("model", model)]))?;
        let (kind, p) = self.next(until).map_err(|e| format!("the AMD graphics helper did not load the model: {e}"))?;
        match kind {
            proto::READY => {
                let input = proto::field(&p, "input").and_then(|v| v.parse::<usize>().ok()).filter(|n| (1..=8192).contains(n)).ok_or("the AMD graphics helper did not say the model's input size")?;
                // A size whose check could not be sent is refused before anything is allocated.
                if proto::tensor_len(&[1, 3, input, input]).is_none_or(|n| n > proto::MAX_FRAME) {
                    return Err(format!("the model's input size {input} is too large for the AMD graphics helper (a check of 1x3x{input}x{input} would not fit in one message)"));
                }
                let adapter = plain(proto::field(&p, "adapter").unwrap_or(""));
                Ok(Ready { input, adapter: if adapter.is_empty() { "AMD graphics".into() } else { adapter } })
            }
            proto::ERROR => Err(first_line(&p)),
            _ => Err("the AMD graphics helper answered the load with something else".into()),
        }
    }

    fn send(&mut self, kind: u8, payload: &[u8]) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("the AMD graphics helper was told to stop")?;
        proto::write_frame(stdin, kind, payload).map_err(|e| self.stopped(&format!("could not reach it: {e}")))
    }

    /// The next frame, by `until`.
    fn next(&mut self, until: Instant) -> Result<(u8, Vec<u8>), String> {
        match self.frames.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Ok(f)) => Ok(f),
            Ok(Err(e)) => Err(self.stopped(&e.to_string())),
            Err(mpsc::RecvTimeoutError::Timeout) => Err("no answer in time".into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.stopped("its output ended")),
        }
    }

    /// The next frame, as long as it takes (the card thread's limit applies).
    fn next_blocking(&mut self) -> Result<(u8, Vec<u8>), String> {
        match self.frames.recv() {
            Ok(Ok(f)) => Ok(f),
            Ok(Err(e)) => Err(self.stopped(&e.to_string())),
            Err(_) => Err(self.stopped("its output ended")),
        }
    }

    /// "the AMD graphics helper stopped (why; exit status)".
    fn stopped(&self, why: &str) -> String {
        self.broken.store(true, std::sync::atomic::Ordering::SeqCst);
        let status = self.proc.reap(Duration::from_millis(200)).map(|s| format!("; {s}")).unwrap_or_default();
        format!("the AMD graphics helper stopped ({why}{status})")
    }

    /// One check: the model's first output.
    fn check(&mut self, input: &Input, until: Option<Instant>) -> Result<Output, String> {
        let input = input.as_standard_layout();
        let data = input.as_slice().ok_or("the AI's input is not in one piece")?;
        self.send(proto::CHECK, &proto::encode_tensor(input.shape(), data))?;
        let (kind, p) = match until {
            Some(u) => self.next(u)?,
            None => self.next_blocking()?,
        };
        match kind {
            proto::OUTPUT => proto::decode_tensor(&p).map_err(|e| format!("the AMD graphics helper's answer: {e}")),
            proto::ERROR => Err(first_line(&p)),
            _ => Err("the AMD graphics helper answered a check with something else".into()),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // QUIT is sent from a thread of its own: a helper that is not reading
        // must not hold this one up (the kill below ends its write).
        if let Some(mut si) = self.stdin.take() {
            let _ = std::thread::Builder::new().name("ai-amd-quit".into()).spawn(move || {
                let _ = proto::write_frame(&mut si, proto::QUIT, &[]);
            });
        }
        if self.proc.reap(Duration::from_millis(1500)).is_none() {
            self.proc.kill();
            let _ = self.proc.reap(Duration::from_secs(2));
        }
    }
}

fn first_line(p: &[u8]) -> String {
    let t = String::from_utf8_lossy(p);
    let l = t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("the AMD graphics helper gave an error");
    plain(l)
}

/// A helper's log line as it goes into the log: at most 300 characters,
/// control characters (escape sequences, carriage returns) shown as "?".
fn log_text(line: &str) -> String {
    line.chars().take(300).map(|c| if c == '\t' { ' ' } else if c.is_control() { '?' } else { c }).collect()
}

/// The longest stderr line kept for the log; the rest of a longer line is read and dropped.
const LOG_LINE: usize = 4096;

/// The helper's stderr into the log (debug), at most `BURST` lines per ten seconds.
/// It reads to the end whatever the helper writes (a line that is not UTF-8,
/// or one without a newline): a pipe closed early would make the helper's
/// next `eprintln!` fail with a broken pipe and panic.
fn log_lines(stderr: impl io::Read) {
    const BURST: u32 = 20;
    let (mut window, mut n, mut dropped) = (Instant::now(), 0u32, 0u32);
    let mut input = stderr;
    let mut line: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut log = |line: &mut Vec<u8>| {
        if window.elapsed() > Duration::from_secs(10) {
            if dropped > 0 {
                tracing::debug!("AMD helper: {dropped} more lines not logged");
            }
            (window, n, dropped) = (Instant::now(), 0, 0);
        }
        if n < BURST {
            n += 1;
            let text = String::from_utf8_lossy(line);
            tracing::debug!("AMD helper: {}", log_text(&text));
        } else {
            dropped += 1;
        }
        line.clear();
    };
    loop {
        let got = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(g) => g,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for part in buf[..got].split_inclusive(|b| *b == b'\n') {
            let (text, ends) = match part.split_last() {
                Some((b'\n', t)) => (t, true),
                _ => (part, false),
            };
            let room = LOG_LINE.saturating_sub(line.len());
            line.extend_from_slice(&text[..text.len().min(room)]);
            if ends {
                log(&mut line);
            }
        }
    }
    if !line.is_empty() {
        log(&mut line);
    }
}

// ---------------------------------------------------------------------
// The card thread
// ---------------------------------------------------------------------

/// The AI on the chip: its thread, the model's input size and the chip's
/// name as the graphics driver gives it.
pub struct Loaded {
    pub thread: CardThread,
    pub size: usize,
    pub name: String,
}

/// How the chip's checks compare with the processor's.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub chip_ms: u64,
    pub cpu_ms: u64,
    pub use_chip: bool,
}

impl Verdict {
    /// What Settings says when the processor is used instead.
    pub fn note(&self) -> String {
        format!(
            "the AMD graphics chip is slower than the processor here ({} ms vs {} ms per check), so the AI uses the processor; choose the graphics card in Settings to use it anyway",
            self.chip_ms, self.cpu_ms
        )
    }
}

fn median(t: &[Duration]) -> Duration {
    let mut t = t.to_vec();
    t.sort();
    t.get(t.len() / 2).copied().unwrap_or_default()
}

/// The chip is used when its median check takes at most `SLOWER_BY` times
/// the processor's.
pub fn judge(chip: &[Duration], cpu: &[Duration]) -> Verdict {
    let (c, p) = (median(chip), median(cpu));
    Verdict { chip_ms: c.as_millis() as u64, cpu_ms: p.as_millis() as u64, use_chip: c.as_secs_f64() <= p.as_secs_f64() * SLOWER_BY }
}

/// What the speed check found this run (None: not done yet).
pub static SPEED: Mutex<Option<Verdict>> = Mutex::new(None);

/// A blank picture of the model's size, as the first check uses.
pub fn blank(size: usize) -> Input {
    ndarray::Array4::<f32>::from_elem((1, 3, size, size), 114.0)
}

/// Start the helper described by `spec` on a card thread, load `model` and
/// run one check. `auto`: the AI picked the chip itself (not chosen in
/// Settings), so it is timed against the processor (`cpu_times`: a few
/// timed checks there; `speed` keeps the answer for the run): a chip that is
/// too slow is let go and its note is the Err.
pub fn load(spec: Spec, model: &Path, auto: bool, cpu_times: &dyn Fn() -> Result<Vec<Duration>, String>, speed: &Mutex<Option<Verdict>>) -> Result<Loaded, String> {
    load_within(spec, model, auto, cpu_times, speed, LOAD_DEADLINE)
}

/// `load` with its own time limit (the tests shorten it).
fn load_within(spec: Spec, model: &Path, auto: bool, cpu_times: &dyn Fn() -> Result<Vec<Duration>, String>, speed: &Mutex<Option<Verdict>>, limit: Duration) -> Result<Loaded, String> {
    if auto {
        if let Some(v) = speed.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|v| !v.use_chip) {
            return Err(v.note());
        }
    }
    let model = model.to_path_buf();
    // The helper's own limits end first, so it is killed by the thread that started it.
    let until = Instant::now() + limit - limit / 18;
    let slot: Arc<Mutex<Option<Killer>>> = Arc::new(Mutex::new(None));
    let thread_slot = slot.clone();
    let restarting = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_restarting = restarting.clone();
    // (A third of the load's limit: a minute.)
    let restart_load = limit / 3;
    let started = CardThread::start(API, limit, move || {
        let (mut session, ready) = Session::start(&spec, &model, until, &thread_slot)?;
        // One check on this thread before the card counts as usable.
        let out = session.check(&blank(ready.input), Some(until)).map_err(|e| format!("the first check on the AMD graphics failed: {e}"))?;
        if out.0.is_empty() {
            return Err("the AMD graphics helper's first answer was empty".into());
        }
        let mut helper = Helper { session: Some(session), spec, model, size: ready.input, slot: thread_slot, died: None, gone: false, restarting: thread_restarting, restart_load };
        let run = move |input: Input| helper.check(&input);
        Ok((run, (ready.input, ready.adapter)))
    });
    let (thread, (size, name)) = match started {
        Ok(x) => x,
        Err(LoadError::Failed(why)) => return Err(why),
        Err(LoadError::Timeout(d)) => {
            // The thread may be stuck in a write to a helper that stopped reading.
            if let Some(k) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                k.kill();
            }
            let why = crate::ai_card::load_off_text(API, d);
            crate::ai_card::block(&why);
            return Err(why);
        }
    };
    // (The helper running now: one started again after a crash replaces it in the slot.)
    thread.on_stuck(move || {
        let k = slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(k) = k {
            k.kill();
        }
    });
    // A check that starts the helper again is not judged by the usual limit.
    thread.on_restart(restarting, restart_load);
    if auto {
        let known = speed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let verdict = match known {
            Some(v) => v,
            None => {
                let v = time_both(&thread, size, cpu_times);
                match v {
                    Ok(v) => {
                        *speed.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.clone());
                        v
                    }
                    Err(why) => {
                        let _ = thread.finish(Duration::from_secs(5));
                        return Err(why);
                    }
                }
            }
        };
        if !verdict.use_chip {
            tracing::info!("AI: {}", verdict.note());
            let _ = thread.finish(Duration::from_secs(5));
            return Err(verdict.note());
        }
    }
    Ok(Loaded { thread, size, name })
}

/// A helper that stops again within this time of stopping before leaves
/// the card (`FailureKind::Gone`) instead of being started again.
const RESTART_WINDOW: Duration = Duration::from_secs(600);
/// The helper on the card thread. When it stops (a crash, a GPU reset),
/// that check fails as an ordinary error and the next check starts it again
/// (the shader cache makes that quick); stopping again within
/// `RESTART_WINDOW`, or not starting again, is `FailureKind::Gone`: the AI
/// leaves the card for the usual rest at once (ai_card.rs, `Recovery::RestNow`),
/// and a new helper is started when the card is tried again. The check that
/// starts it again (loading the model takes seconds) may take `restart_load`
/// (`CardThread::on_restart`): one over that is `Gone` too, not a slow check.
struct Helper {
    session: Option<Session>,
    spec: Spec,
    model: PathBuf,
    /// The model's input size (the detector's; a new helper must say the same).
    size: usize,
    /// Kills the helper running now (`CardThread::on_stuck`).
    slot: Arc<Mutex<Option<Killer>>>,
    /// When the helper last stopped.
    died: Option<Instant>,
    /// Given up: every check fails at once.
    gone: bool,
    /// Set while a check starts the helper again (`CardThread::on_restart`).
    restarting: Arc<std::sync::atomic::AtomicBool>,
    /// How long that check may take.
    restart_load: Duration,
}

/// Clears the restart flag when the check that set it ends.
struct Restarting<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Restarting<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Helper {
    fn gone(&mut self, why: String) -> crate::ai_card::RunError {
        self.gone = true;
        crate::ai_card::RunError { kind: crate::ai_card::FailureKind::Gone, detail: why }
    }

    fn check(&mut self, input: &Input) -> Result<Output, crate::ai_card::RunError> {
        if self.gone {
            return Err(self.gone("the AMD graphics helper stopped and is not started again until the card is tried again".into()));
        }
        let restarting = self.restarting.clone();
        let _restarting = if self.session.is_none() {
            restarting.store(true, std::sync::atomic::Ordering::SeqCst);
            Some(Restarting(&restarting))
        } else {
            None
        };
        if self.session.is_none() {
            // (Its own limits end first, so a load that hangs fails here.)
            let until = Instant::now() + self.restart_load - self.restart_load / 18;
            match Session::start(&self.spec, &self.model, until, &self.slot) {
                Ok((s, ready)) if ready.input == self.size => {
                    tracing::info!("AI: the AMD graphics helper was started again");
                    self.session = Some(s);
                }
                Ok(_) => return Err(self.gone("the AMD graphics helper started again with another input size".into())),
                Err(why) => return Err(self.gone(format!("the AMD graphics helper could not be started again: {why}"))),
            }
        }
        let s = self.session.as_mut().expect("started above");
        match s.check(input, None) {
            Ok(out) => Ok(out),
            Err(e) if s.broken.load(std::sync::atomic::Ordering::SeqCst) => {
                // (Killed first, so dropping the session does not wait for it.)
                s.proc.kill();
                self.session = None;
                let again = self.died.is_some_and(|t| t.elapsed() < RESTART_WINDOW);
                self.died = Some(Instant::now());
                if again {
                    return Err(self.gone(format!("{e}, a second time within {} minutes", RESTART_WINDOW.as_secs() / 60)));
                }
                tracing::warn!("AI: {e}; it is started again for the next check");
                Err(e.into())
            }
            Err(e) => Err(e.into()),
        }
    }
}

fn time_both(thread: &CardThread, size: usize, cpu_times: &dyn Fn() -> Result<Vec<Duration>, String>) -> Result<Verdict, String> {
    let mut chip = Vec::new();
    for _ in 0..SPEED_CHECKS {
        let t = Instant::now();
        thread.run(blank(size)).map_err(|e| format!("the AMD graphics chip failed a speed check: {e}"))?;
        chip.push(t.elapsed());
    }
    let cpu = cpu_times()?;
    let v = judge(&chip, &cpu);
    tracing::info!("AI: a check takes {} ms on the AMD graphics chip and {} ms on the processor", v.chip_ms, v.cpu_ms);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-amd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn mesas_amd_driver_file_is_found_and_a_missing_one_says_what_to_install() {
        let d = temp("icd");
        // (Test files belong to whoever runs the tests, so `find_icd`'s root is that user here.)
        let me = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&d).unwrap());
        let find_icd = |dirs: &[&Path]| find_icd_owned_by(dirs, me);
        let (a, b) = (d.join("a"), d.join("b"));
        // (Explicit modes: the test user's umask may leave group write on.)
        let put = |p: &Path| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(p, "{}").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
        };
        for dir in [&d, &a, &b] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(dir).unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Not found: Intel's and NVIDIA's files, and a 32-bit AMD one, are no use.
        for n in ["intel_icd.x86_64.json", "nvidia_icd.json", "radeon_icd.i686.json", "lvp_icd.x86_64.json"] {
            put(&a.join(n));
        }
        assert_eq!(find_icd(&[a.as_path(), b.as_path()]), None);
        let e = driver_in(&[a.as_path()], &[], false, me).unwrap_err();
        assert!(is_driver_missing(&e) && e.contains("mesa-vulkan-drivers"), "{e}");
        let e = driver_in(&[a.as_path()], &[], true, me).unwrap_err();
        assert!(is_driver_missing(&e) && e.contains("Docker"), "{e}");
        // Found, the 64-bit file first, the first folder first.
        put(&b.join("radeon_icd.json"));
        put(&b.join("radeon_icd.x86_64.json"));
        assert_eq!(find_icd(&[a.as_path(), b.as_path()]), Some(b.join("radeon_icd.x86_64.json")));
        std::fs::remove_file(b.join("radeon_icd.x86_64.json")).unwrap();
        assert_eq!(find_icd(&[a.as_path(), b.as_path()]), Some(b.join("radeon_icd.json")));
        put(&a.join("radeon_icd.x86_64.json"));
        assert_eq!(find_icd(&[a.as_path(), b.as_path()]), Some(a.join("radeon_icd.x86_64.json")));
        // Installed but not as root left it (here: owned by another user): the
        // note names the file instead of asking for a driver that is there.
        let e = driver_in(&[a.as_path()], &[], false, me.wrapping_add(1)).unwrap_err();
        assert!(!is_driver_missing(&e) && e.contains("radeon_icd.x86_64.json") && e.contains("not owned by root"), "{e}");
        // The render node: one this user cannot use is told, naming the group.
        assert!(driver_in(&[a.as_path()], &[d.join("a").join("radeon_icd.x86_64.json").display().to_string()], false, me).is_ok());
        let e = driver_in(&[a.as_path()], &["/nonexistent/renderD128".into()], false, me).unwrap_err();
        assert!(e.contains("render group") && e.contains("usermod") && !is_driver_missing(&e), "{e}");
    }

    #[test]
    fn a_driver_file_not_owned_by_root_or_open_to_others_is_not_used() {
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
        let d = temp("icd-owner");
        let me = std::fs::metadata(&d).unwrap().uid();
        let dir = d.join("icd.d");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("radeon_icd.json");
        std::fs::write(&file, "{}").unwrap();
        let set = |p: &Path, mode| std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        set(&dir, 0o755);
        set(&file, 0o644);
        assert_eq!(find_icd_owned_by(&[dir.as_path()], me), Some(file.clone()));
        // Another owner (the rule is root's in the program, this user's here).
        assert_eq!(find_icd_owned_by(&[dir.as_path()], me + 1), None);
        // The file, or its folder, writable by group or others.
        set(&file, 0o664);
        assert_eq!(find_icd_owned_by(&[dir.as_path()], me), None);
        set(&file, 0o644);
        set(&dir, 0o757);
        assert_eq!(find_icd_owned_by(&[dir.as_path()], me), None);
        set(&dir, 0o755);
        // A link is followed and its target judged: into a closed folder it passes, into an open one it does not.
        let (good, open) = (d.join("good"), d.join("open"));
        for f in [&good, &open] {
            std::fs::create_dir_all(f).unwrap();
            std::fs::write(f.join("real.json"), "{}").unwrap();
            set(&f.join("real.json"), 0o644);
        }
        set(&good, 0o755);
        set(&open, 0o777);
        std::fs::remove_file(&file).unwrap();
        symlink(good.join("real.json"), &file).unwrap();
        assert_eq!(find_icd_owned_by(&[dir.as_path()], me), Some(file.clone()));
        std::fs::remove_file(&file).unwrap();
        symlink(open.join("real.json"), &file).unwrap();
        assert_eq!(find_icd_owned_by(&[dir.as_path()], me), None);
        set(&open, 0o755);
    }

    #[test]
    fn the_helper_gets_the_amd_driver_and_nothing_else() {
        let s = Spec::for_addon(Path::new("/data/tools/amd-1"), Path::new("/usr/share/vulkan/icd.d/radeon_icd.x86_64.json"), Path::new("/data/cache/amd-1"));
        assert_eq!(s.program, Path::new("/data/tools/amd-1/ambersight-ai-amd"));
        let env: std::collections::BTreeMap<String, String> = s.env.iter().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect();
        assert_eq!(env["VK_DRIVER_FILES"], "/usr/share/vulkan/icd.d/radeon_icd.x86_64.json");
        assert_eq!(env["VK_ICD_FILENAMES"], env["VK_DRIVER_FILES"]);
        assert_eq!((env["HOME"].as_str(), env["XDG_CACHE_HOME"].as_str()), ("/data/cache/amd-1", "/data/cache/amd-1"));
        assert_eq!(env["LD_LIBRARY_PATH"], "/data/tools/amd-1");
        assert_eq!(env["VK_LOADER_LAYERS_DISABLE"], "~implicit~");
        assert_eq!(env.len(), 7);
    }

    #[test]
    fn the_chip_is_used_when_its_median_is_at_most_one_and_a_half_times_the_processors() {
        let ms = |v: &[u64]| v.iter().map(|m| Duration::from_millis(*m)).collect::<Vec<_>>();
        // A small 2-compute-unit chip: 139 ms against 29 ms.
        let v = judge(&ms(&[139, 140, 138, 500, 139]), &ms(&[29, 30, 28, 29, 29]));
        assert_eq!((v.chip_ms, v.cpu_ms, v.use_chip), (139, 29, false));
        assert!(v.note().contains("slower than the processor here (139 ms vs 29 ms per check)"));
        // A 780M: 35 ms against 112 ms.
        assert!(judge(&ms(&[35, 36, 34, 35, 35]), &ms(&[112, 110, 115, 112, 112])).use_chip);
        // The edge: exactly 1.5 times is still used, a little over is not.
        assert!(judge(&ms(&[150]), &ms(&[100])).use_chip);
        assert!(!judge(&ms(&[151]), &ms(&[100])).use_chip);
        // One slow outlier does not decide it.
        assert!(judge(&ms(&[10, 10, 900, 10, 10]), &ms(&[100, 100, 100, 100, 100])).use_chip);
    }

    // -- the helper itself: src/test_support/fake_amd_helper.rs, compiled
    // once per test run with rustc (std only; `cargo test` builds no examples here)

    fn fake() -> PathBuf {
        static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        BUILT
            .get_or_init(|| {
                let dir = temp("fake-build");
                let out = dir.join("fake_amd_helper");
                let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/test_support/fake_amd_helper.rs");
                let st = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                    .args(["--edition", "2021", "-A", "warnings", "-o"])
                    .arg(&out)
                    .arg(&src)
                    .env("CARGO_PKG_VERSION", crate::update::CURRENT)
                    .status()
                    .expect("rustc");
                assert!(st.success(), "could not compile {}", src.display());
                out
            })
            .clone()
    }

    fn spec(args: &[&str]) -> Spec {
        Spec { program: fake(), args: args.iter().map(OsString::from).collect(), env: Vec::new() }
    }

    /// Whether the process `pid` is gone (or only a zombie waiting to be waited for).
    fn gone(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(s) => s.rsplit(") ").next().is_some_and(|r| r.starts_with('Z')),
        }
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(10), "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pid_of(file: &Path) -> u32 {
        wait_until("the helper wrote its pid", || std::fs::read_to_string(file).is_ok_and(|s| s.trim().parse::<u32>().is_ok()));
        std::fs::read_to_string(file).unwrap().trim().parse().unwrap()
    }

    fn model() -> PathBuf {
        PathBuf::from("/nowhere/model.onnx")
    }

    fn no_cpu() -> Result<Vec<Duration>, String> {
        panic!("not timed")
    }

    fn load_plain(args: &[&str]) -> Result<Loaded, String> {
        load(spec(args), &model(), false, &no_cpu, &Mutex::new(None))
    }

    #[test]
    fn a_relative_model_path_reaches_the_helper_whole() {
        // The default data folder ("amberdata") makes the model's path relative;
        // the helper runs in "/", so it must get the whole path.
        let l = load(spec(&[]), Path::new("amberdata/models/yolox_s.onnx"), false, &no_cpu, &Mutex::new(None)).unwrap();
        assert!(l.thread.finish(Duration::from_secs(5)));
    }

    #[test]
    fn a_helper_loads_checks_and_leaves_when_told() {
        let d = temp("ok");
        let (pidfile, quitfile) = (d.join("pid"), d.join("quit"));
        let l = load_plain(&[&format!("--pidfile={}", pidfile.display()), &format!("--quitfile={}", quitfile.display())]).unwrap();
        assert_eq!((l.size, l.name.as_str(), l.thread.api()), (32, "Fake AMD (test)", "WebGPU"));
        let pid = pid_of(&pidfile);
        let out = l.thread.run(blank(l.size)).unwrap();
        assert_eq!(out.0, vec![1, 2]);
        // Shutdown: QUIT, the helper leaves, and it is waited for (no zombie).
        assert!(l.thread.finish(Duration::from_secs(5)));
        wait_until("the helper got QUIT", || quitfile.exists());
        wait_until("the helper is gone", || !Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn a_helper_of_another_version_or_protocol_is_refused_and_killed() {
        for (arg, word) in [("--version=0.0.1", "another version"), ("--protocol=9", "another version")] {
            let d = temp("version");
            let pidfile = d.join("pid");
            let e = load_plain(&[arg, &format!("--pidfile={}", pidfile.display())]).err().unwrap();
            assert!(e.contains(word) && e.contains("AMD graphics add-on"), "{e}");
            let pid = pid_of(&pidfile);
            wait_until("the refused helper is gone", || !Path::new(&format!("/proc/{pid}")).exists());
        }
    }

    #[test]
    fn a_load_error_and_a_helper_that_never_says_hello_fail_the_load() {
        let e = load_plain(&["--mode=load-error"]).err().unwrap();
        assert_eq!(e, "the model needs a newer WebGPU");
        let e = load_plain(&["--mode=crash-at-start"]).err().unwrap();
        assert!(e.contains("the AMD graphics helper stopped"), "{e}");
        let e = load(Spec { program: PathBuf::from("/nonexistent/ambersight-ai-amd"), args: vec![], env: vec![] }, &model(), false, &no_cpu, &Mutex::new(None)).err().unwrap();
        assert!(e.contains("could not start the AMD graphics helper"), "{e}");
        // (A helper that is silent is waited for HELLO_WAIT, too long to run here:
        // `next` with a short deadline is what it uses.)
        let d = temp("silent");
        let pidfile = d.join("pid");
        let mut cmd_spec = spec(&["--mode=silent", &format!("--pidfile={}", pidfile.display())]);
        cmd_spec.env = Vec::new();
        let r = Session::start(&cmd_spec, &model(), Instant::now() + Duration::from_millis(300), &Mutex::new(None));
        assert!(r.is_err());
        let pid = pid_of(&pidfile);
        wait_until("the silent helper is gone", || !Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn a_check_over_the_limit_kills_the_helper_and_it_is_waited_for() {
        crate::ai_card::reset_for_test();
        let d = temp("hang");
        let pidfile = d.join("pid");
        // Check number 4 (counting the first one, made while loading) never answers.
        let l = load_plain(&["--mode=hang-on:4", &format!("--pidfile={}", pidfile.display())]).unwrap();
        let pid = pid_of(&pidfile);
        let t = l.thread.with_deadline(Duration::from_millis(300));
        assert!(t.run(blank(l.size)).is_ok());
        assert!(t.run(blank(l.size)).is_ok());
        let t0 = Instant::now();
        let e = t.run(blank(l.size)).unwrap_err();
        assert_eq!(e.kind, crate::ai_card::FailureKind::Timeout);
        assert!(t0.elapsed() < Duration::from_secs(3));
        // The hook killed the process (by its pid); its thread's wait ended.
        wait_until("the hung helper is gone", || gone(pid));
        wait_until("the card thread came back", || !t.stuck());
        // Dropping the card thread waits for the process: not even a zombie is left.
        drop(t);
        wait_until("no zombie", || !Path::new(&format!("/proc/{pid}")).exists());
        // Slow, not wedged, so the recovery is the usual rest and a new helper.
        assert_eq!(crate::ai_card::recovery(API, crate::ai_card::FailureKind::Timeout), crate::ai_card::Recovery::RestThenJudge);
    }

    #[test]
    fn a_helper_that_crashes_or_errors_fails_that_check_and_is_a_counted_error() {
        // Crash on check 2.
        let d = temp("crash");
        let pidfile = d.join("pid");
        let l = load_plain(&["--mode=crash-on:2", &format!("--pidfile={}", pidfile.display())]).unwrap();
        let pid = pid_of(&pidfile);
        let e = l.thread.run(blank(l.size)).unwrap_err();
        assert_eq!(e.kind, crate::ai_card::FailureKind::Error);
        assert_eq!(crate::ai_card::recovery(API, e.kind), crate::ai_card::Recovery::CountThenRest);
        assert!(e.to_string().contains("AI inference (WebGPU): the AMD graphics helper stopped"), "{e}");
        assert!(crate::engine::ai_gpu::is_gpu_error(&e.to_string()));
        // The crashed helper is waited for (no zombie), and the next check starts a new one.
        wait_until("no zombie", || !Path::new(&format!("/proc/{pid}")).exists());
        assert!(l.thread.run(blank(l.size)).is_ok(), "started again");
        let pid2 = pid_of(&pidfile);
        assert_ne!(pid, pid2);
        // It stops again soon (its own check 2): the card is left at once, not counted.
        let e = l.thread.run(blank(l.size)).unwrap_err();
        assert_eq!(e.kind, crate::ai_card::FailureKind::Gone);
        assert_eq!(crate::ai_card::recovery(API, e.kind), crate::ai_card::Recovery::RestNow);
        assert!(e.to_string().contains("a second time within 10 minutes"), "{e}");
        // Not started a third time: later checks fail at once, and the thread is not stuck.
        let t0 = Instant::now();
        assert_eq!(l.thread.run(blank(l.size)).unwrap_err().kind, crate::ai_card::FailureKind::Gone);
        assert!(t0.elapsed() < Duration::from_millis(500));
        assert_eq!(pid_of(&pidfile), pid2);
        assert!(!l.thread.stuck() && !l.thread.faulted());
        wait_until("no zombie", || !Path::new(&format!("/proc/{pid2}")).exists());
        drop(l);
        // A helper that cannot be started again leaves the card at once too.
        let d = temp("crash-gone");
        let marker = d.join("broken");
        let l = load_plain(&["--mode=crash-on:2", &format!("--refuse-if={}", marker.display())]).unwrap();
        std::fs::write(&marker, "x").unwrap();
        assert_eq!(l.thread.run(blank(l.size)).unwrap_err().kind, crate::ai_card::FailureKind::Error);
        let e = l.thread.run(blank(l.size)).unwrap_err();
        assert_eq!(e.kind, crate::ai_card::FailureKind::Gone);
        assert!(e.to_string().contains("could not be started again"), "{e}");
        drop(l);
        // An ERROR frame fails one check; the helper goes on.
        let l = load_plain(&["--mode=error-on:2"]).unwrap();
        let e = l.thread.run(blank(l.size)).unwrap_err();
        assert!(e.to_string().contains("device lost (test)"), "{e}");
        assert!(l.thread.run(blank(l.size)).is_ok());
    }

    #[test]
    fn the_speed_check_keeps_a_fast_chip_and_lets_a_slow_one_go() {
        crate::ai_card::reset_for_test();
        let cpu = |ms: u64| move || Ok(vec![Duration::from_millis(ms); 5]);
        // A chip about as fast as the processor (about 30 ms each): used.
        let speed = Mutex::new(None);
        let l = load(spec(&["--delay-ms=30"]), &model(), true, &cpu(40), &speed).unwrap();
        let v = speed.lock().unwrap().clone().unwrap();
        assert!(v.use_chip && v.cpu_ms == 40 && v.chip_ms >= 30, "{v:?}");
        assert!(l.thread.finish(Duration::from_secs(5)));
        // A slow one (about 200 ms against 20 ms): not used, and its helper is let go.
        let d = temp("slow");
        let (pidfile, quitfile) = (d.join("pid"), d.join("quit"));
        let speed = Mutex::new(None);
        let e = load(spec(&["--delay-ms=200", &format!("--pidfile={}", pidfile.display()), &format!("--quitfile={}", quitfile.display())]), &model(), true, &cpu(20), &speed).err().unwrap();
        assert!(e.contains("slower than the processor here (") && e.contains(" ms vs 20 ms per check)"), "{e}");
        let pid = pid_of(&pidfile);
        wait_until("the slow chip's helper left", || !Path::new(&format!("/proc/{pid}")).exists());
        assert!(quitfile.exists());
        // Remembered for the run: the next try does not even start a helper.
        let e2 = load(Spec { program: PathBuf::from("/nonexistent"), args: vec![], env: vec![] }, &model(), true, &no_cpu, &speed).err().unwrap();
        assert_eq!(e, e2);
        // Chosen in Settings (not auto): used however slow, and not timed against the processor.
        let l = load(spec(&["--delay-ms=200"]), &model(), false, &no_cpu, &speed).unwrap();
        assert!(l.thread.finish(Duration::from_secs(5)));
    }

    #[test]
    fn starting_the_helper_again_is_not_judged_by_the_check_limit() {
        // Load limit 3 s, so starting again may take 1 s; a check may take 200 ms.
        let load_slow = |ms: u64, marker: &Path, pidfile: &Path| {
            let args = ["--mode=crash-on:2".to_string(), format!("--slow-load-if={}:{ms}", marker.display()), format!("--pidfile={}", pidfile.display())];
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let l = load_within(spec(&args), &model(), false, &no_cpu, &Mutex::new(None), Duration::from_secs(3)).unwrap();
            let t = l.thread.with_deadline(Duration::from_millis(200));
            (t, l.size)
        };
        // A new helper that takes 600 ms to load (three check limits): used.
        let d = temp("slow-restart");
        let (marker, pidfile) = (d.join("slow"), d.join("pid"));
        let (t, size) = load_slow(600, &marker, &pidfile);
        std::fs::write(&marker, "x").unwrap();
        assert_eq!(t.run(blank(size)).unwrap_err().kind, crate::ai_card::FailureKind::Error);
        let t0 = Instant::now();
        assert!(t.run(blank(size)).is_ok(), "started again, slowly");
        assert!(t0.elapsed() >= Duration::from_millis(600));
        assert!(!t.stuck());
        drop(t);
        // One that does not load within 1 s: the card is left (Gone), not "too slow".
        let d = temp("hung-restart");
        let (marker, pidfile) = (d.join("slow"), d.join("pid"));
        let (t, size) = load_slow(60_000, &marker, &pidfile);
        std::fs::write(&marker, "x").unwrap();
        assert_eq!(t.run(blank(size)).unwrap_err().kind, crate::ai_card::FailureKind::Error);
        let t0 = Instant::now();
        let e = t.run(blank(size)).unwrap_err();
        assert_eq!(e.kind, crate::ai_card::FailureKind::Gone, "{e}");
        assert_eq!(crate::ai_card::recovery(API, e.kind), crate::ai_card::Recovery::RestNow);
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        // The new helper was killed, the thread came back and gives up at once.
        let pid = pid_of(&pidfile);
        wait_until("the slow helper is gone", || gone(pid));
        wait_until("the card thread came back", || !t.stuck());
        assert_eq!(t.run(blank(size)).unwrap_err().kind, crate::ai_card::FailureKind::Gone);
        drop(t);
        wait_until("no zombie", || !Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn a_check_waiting_behind_a_slow_restart_is_not_a_slow_check() {
        // Two cameras: one check starts the helper again (1.5 s, over three
        // 200 ms limits), the other waits behind it; both are done.
        let d = temp("queued-restart");
        let (marker, pidfile) = (d.join("slow"), d.join("pid"));
        let args = ["--mode=crash-on:3".to_string(), "--delay-ms=100".to_string(), format!("--slow-load-if={}:1500", marker.display()), format!("--pidfile={}", pidfile.display())];
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let l = load_within(spec(&args), &model(), false, &no_cpu, &Mutex::new(None), Duration::from_secs(6)).unwrap();
        let (t, size) = (l.thread.with_deadline(Duration::from_millis(200)), l.size);
        assert!(t.run(blank(size)).is_ok());
        std::fs::write(&marker, "x").unwrap();
        assert_eq!(t.run(blank(size)).unwrap_err().kind, crate::ai_card::FailureKind::Error, "the crash");
        let (a, b) = std::thread::scope(|s| {
            let a = s.spawn(|| t.run(blank(size)).map(|_| ()).map_err(|e| e.kind));
            std::thread::sleep(Duration::from_millis(100));
            let b = s.spawn(|| t.run(blank(size)).map(|_| ()).map_err(|e| e.kind));
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_eq!((a, b), (Ok(()), Ok(())));
        assert!(!t.stuck());
        let pid = pid_of(&pidfile);
        assert!(!gone(pid), "the new helper was not killed");
        drop(t);
    }

    #[test]
    fn a_helper_that_logs_bytes_that_are_not_text_and_endless_lines_keeps_working() {
        // The log reader must read to the end: a closed pipe would kill the helper's next eprintln.
        let d = temp("junk");
        let pidfile = d.join("pid");
        let l = load_plain(&["--mode=stderr-junk", &format!("--pidfile={}", pidfile.display())]).unwrap();
        for _ in 0..3 {
            assert!(l.thread.run(blank(l.size)).is_ok());
        }
        assert!(l.thread.finish(Duration::from_secs(5)));
    }

    #[test]
    fn log_lines_cut_long_lines_and_read_to_the_end() {
        let mut data = b"ok\n\xff\xfe not text\n".to_vec();
        data.extend(std::iter::repeat(b'x').take(3 << 20));
        data.extend_from_slice(b"\nlast without newline");
        // Returns only at the end of the input, whatever it held.
        log_lines(&data[..]);
        // What reaches the log: no escape sequences or carriage returns, 300 characters at most.
        assert_eq!(log_text("red \x1b[31mtext\r\tok"), "red ?[31mtext? ok");
        assert_eq!(log_text(&"é".repeat(1000)).chars().count(), 300);
    }

    #[test]
    fn a_load_that_hangs_in_a_write_kills_the_helper() {
        crate::ai_card::reset_for_test();
        let d = temp("stall");
        let pidfile = d.join("pid");
        // READY, then it never reads again: the first check (12 MB, over the pipe's size) cannot be written.
        let t0 = Instant::now();
        let e = load_within(spec(&["--mode=stall-after-ready", "--input=1000", &format!("--pidfile={}", pidfile.display())]), &model(), false, &no_cpu, &Mutex::new(None), Duration::from_millis(1500)).err().unwrap();
        assert!(e.contains("did not finish loading"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(8));
        let pid = pid_of(&pidfile);
        wait_until("the stalled helper is gone", || !Path::new(&format!("/proc/{pid}")).exists());
        crate::ai_card::reset_for_test();
    }

    #[test]
    fn an_input_size_whose_check_cannot_be_sent_is_refused() {
        let e = load_plain(&["--input=3000"]).err().unwrap();
        assert!(e.contains("input size 3000 is too large"), "{e}");
        assert!(proto::tensor_len(&[1, 3, 2300, 2300]).unwrap() <= proto::MAX_FRAME);
    }

    #[test]
    fn what_the_helper_logs_does_not_reach_the_protocol() {
        // (The fake writes to stderr at start; a load still succeeds and the line is only logged.)
        let l = load_plain(&["--stderr-lines=500"]).unwrap();
        assert!(l.thread.run(blank(l.size)).is_ok());
        assert!(l.thread.finish(Duration::from_secs(5)));
    }
}
