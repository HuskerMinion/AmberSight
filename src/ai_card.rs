//! The built-in AI on a graphics card runs on a thread of its own, with a
//! time limit, so a card that stops answering never holds up detection or
//! shutdown.
//!
//! What an Intel Arc card does when it faults (seen on Linux with the xe
//! driver): the session that faulted is dead (every later run fails), a new
//! session on that card fails too, and sometimes the card's scheduler does
//! not reset it, so the next job on it from any process spins with no time
//! limit until that process is killed. A session on the processor works at
//! once, and so does another card. The C++ exit code of the libraries
//! hangs after a fault. So:
//!
//! - The thread makes the session itself (with a limit on how long that and
//!   one first check may take, `LOAD_DEADLINE`: this also covers a card
//!   still wedged by a process before this one) and then does every check on
//!   the card, one at a time. A caller waits for its check at most the
//!   current limit (`CardThread::limit`): 20 times the usual time of a check
//!   on this card, at least 2 s and at most 10 s.
//!   The first check after the card has been idle (it may have powered
//!   down) gets the 10 s, and time the computer spent asleep does not count.
//! - A check over the limit, or the thread found still busy with one past
//!   the limit, marks the card stuck: later checks are refused at once (no
//!   one waits behind it), and the thread is left alone: it can be in the
//!   card's driver for good, so nothing ever waits for it. A check that comes
//!   back after all, without an OpenVINO error, shows the card is slow, not
//!   wedged: the thread is no longer stuck and keeps its session.
//! - An OpenVINO error marks the card faulted (`fault_kills_card`), and the
//!   session is never called or dropped again: it is leaked with its thread
//!   (any further call into the card's driver could hang).
//! - With a stuck or faulted card the program leaves without running the
//!   libraries' exit code (`any_stuck`). A healthy card's session is dropped
//!   at shutdown after the check in progress has finished
//!   (`CardThread::finish`).
//! - What the engine does next is `recovery` (engine/ai_gpu.rs does it; the
//!   check that failed is always done again on the processor): an OpenVINO
//!   error turns the cards off for the AI until AmberSight restarts
//!   (`block`). A check over the limit (any API) moves the AI to the
//!   processor for five minutes; then a card thread that is still stuck
//!   means off until restart, and one that came back gets the card again.
//!   Other card errors (CUDA, DirectML, Core ML) are counted as before:
//!   three in a row within a minute move the AI to the processor for five
//!   minutes.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// How long making the session on the card and its first check may take.
/// OpenVINO compiles the model for the card on its first start (about a
/// second for the Large model on an Arc card, far longer on small built-in
/// graphics) and loads it from its cache later. Nothing waits for it: the
/// AI runs on the processor meanwhile and moves to the card once it works.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
pub const LOAD_DEADLINE: Duration = Duration::from_secs(180);

/// The limit for one check: 20 times the median of the recent ones, within
/// these bounds; `MAX_LIMIT` until a few checks have been timed.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
const MIN_LIMIT: Duration = Duration::from_secs(2);
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
const MAX_LIMIT: Duration = Duration::from_secs(10);
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
const TIMES_KEPT: usize = 32;
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
const TIMES_NEEDED: usize = 5;

/// A card idle this long may have powered down: its next check gets
/// `MAX_LIMIT` (waking it can take a while).
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
const IDLE_GAP: Duration = Duration::from_secs(30);

/// A wait for a check that took this much longer than asked for means the
/// computer was asleep in between (a clock that counts sleep, as Windows'
/// does): that time is not held against the card.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
const SLEPT: Duration = Duration::from_secs(2);
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
const MAX_ASLEEP: Duration = Duration::from_secs(300);

/// The time asleep so far in a wait (`asleep`), after a step of `gap` that
/// asked for `slice`: only a jump of over twice the step and over `SLEPT`
/// counts, and at most `MAX_ASLEEP` in all, so a thread starved of time
/// cannot hide a real hang for ever.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn asleep_after(asleep: Duration, gap: Duration, slice: Duration) -> Duration {
    if gap > slice * 2 && gap > slice + SLEPT {
        (asleep + (gap - slice)).min(MAX_ASLEEP)
    } else {
        asleep
    }
}

/// A fixed limit for one check from AMBERSIGHT_AI_CARD_DEADLINE_MS (tests
/// force a timeout with it); None: the adaptive one.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn fixed_limit() -> Option<Duration> {
    std::env::var("AMBERSIGHT_AI_CARD_DEADLINE_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|ms| *ms > 0).map(Duration::from_millis)
}

/// The limit from the recent check times (see `MIN_LIMIT`).
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn limit_from(times: &[Duration]) -> Duration {
    if times.len() < TIMES_NEEDED {
        return MAX_LIMIT;
    }
    let mut t = times.to_vec();
    t.sort();
    (t[t.len() / 2] * 20).clamp(MIN_LIMIT, MAX_LIMIT)
}

/// A caller that waited this many limits for its check (behind others in
/// the queue) gives up too.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
const QUEUE_LIMITS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The card did not answer within the limit.
    // (Never made on Intel macOS, where the AI has no card path.)
    #[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
    Timeout,
    /// The card's runtime gave an error.
    Error,
}

/// A check the card did not do.
#[derive(Debug)]
pub struct CardFailure {
    /// "CUDA", "OpenVINO", "DirectML", "Core ML".
    pub api: &'static str,
    pub kind: FailureKind,
    pub detail: String,
}

impl fmt::Display for CardFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            // (Worded so engine/ai_gpu.rs tells it for a card error.)
            FailureKind::Timeout => write!(f, "the graphics card did not answer in time ({}, {})", self.api, self.detail),
            FailureKind::Error => write!(f, "AI inference ({}): {}", self.api, self.detail),
        }
    }
}

impl std::error::Error for CardFailure {}

/// What to do when a card fails a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Count it: three in a row within a minute move the AI to the
    /// processor for five minutes, then the card is tried again.
    CountThenRest,
    /// The processor does this check and all later ones; the card stays
    /// off for the AI until AmberSight restarts.
    OffUntilRestart,
    /// The processor does this check and the next five minutes' ones; then
    /// a card thread still stuck means `OffUntilRestart`, and one that came
    /// back (the card was slow, not wedged) gets the card again.
    RestThenJudge,
}

/// The strategy (see the top of this file).
pub fn recovery(api: &str, kind: FailureKind) -> Recovery {
    match (kind, api) {
        (FailureKind::Timeout, _) => Recovery::RestThenJudge,
        (FailureKind::Error, _) if fault_kills_card(api) => Recovery::OffUntilRestart,
        (FailureKind::Error, _) => Recovery::CountThenRest,
    }
}

/// Whether an error from the card's runtime means the session (and the
/// card, in this process) is dead: OpenVINO on Intel graphics, where any
/// further call after a fault may hang.
pub fn fault_kills_card(api: &str) -> bool {
    api == "OpenVINO"
}

/// What Settings and the Status page say when the card did not load the
/// model within `took` (a slow first compile is not "stopped responding").
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
pub fn load_off_text(api: &str, took: Duration) -> String {
    let card = if api == "OpenVINO" { "the Intel graphics card".to_string() } else { format!("the graphics card ({api})") };
    format!("{card} did not finish loading the AI model within {} minutes; using the processor until AmberSight restarts", took.as_secs().div_ceil(60))
}

/// What Settings says while the AI rests on the processor after a check on
/// the card took too long.
pub fn slow_text(api: &str) -> String {
    let card = if api == "OpenVINO" { "the Intel graphics card".to_string() } else { format!("the graphics card ({api})") };
    format!("{card} was too slow to answer an AI check; using the processor for 5 minutes, then the card again")
}

/// What Settings and the Status page say when checks on the card were too
/// slow three times within an hour.
pub fn keeps_slow_text(api: &str) -> String {
    let card = if api == "OpenVINO" { "the Intel graphics card".to_string() } else { format!("the graphics card ({api})") };
    format!("{card} keeps being too slow for the AI; staying on the processor until AmberSight restarts")
}

/// What Settings and the Status page say when the card is off for the AI
/// until a restart.
pub fn off_text(api: &str) -> String {
    match api {
        "OpenVINO" => "the Intel graphics card stopped responding to the AI; using the processor until AmberSight restarts".to_string(),
        "" => "the graphics card stopped responding to the AI; using the processor until AmberSight restarts".to_string(),
        a => format!("the graphics card stopped responding to the AI ({a}); using the processor until AmberSight restarts"),
    }
}

// ---------------------------------------------------------------------
// The card turned off until a restart; stopping
// ---------------------------------------------------------------------

static BLOCKED: Mutex<Option<String>> = Mutex::new(None);
/// Card threads stuck in a check now (one may still come back).
static STUCK_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// A card session was leaked (a fault, or not let go at shutdown).
static LEFT: AtomicBool = AtomicBool::new(false);
/// Card sessions being made now (a program leaving meanwhile must not run
/// the libraries' exit code under them).
static LOADING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Counts in `LOADING` while it lives (also when the work panics): card work
/// the program must not run the libraries' exit code under.
pub struct LoadingGuard(&'static std::sync::atomic::AtomicUsize);

impl LoadingGuard {
    pub fn new() -> LoadingGuard {
        LoadingGuard::on(&LOADING)
    }

    fn on(count: &'static std::sync::atomic::AtomicUsize) -> LoadingGuard {
        count.fetch_add(1, Ordering::SeqCst);
        LoadingGuard(count)
    }
}

impl Drop for LoadingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

static STOPPING: AtomicBool = AtomicBool::new(false);

/// Keep the AI off the graphics cards until AmberSight restarts, saying why
/// (the first reason stays).
pub fn block(reason: &str) {
    let mut b = BLOCKED.lock().unwrap_or_else(|e| e.into_inner());
    if b.is_none() {
        *b = Some(reason.to_string());
    }
}

/// Why the AI may not use a card now (None: it may).
pub fn blocked() -> Option<String> {
    BLOCKED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A card thread is stuck (it may never come back), a card faulted, or a
/// session is being made: leave the program without running the libraries'
/// exit code.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn any_stuck() -> bool {
    STUCK_THREADS.load(Ordering::SeqCst) > 0 || LEFT.load(Ordering::SeqCst) || LOADING.load(Ordering::SeqCst) > 0
}

/// AmberSight is stopping: no new checks start on a card.
pub fn begin_stop() {
    STOPPING.store(true, Ordering::SeqCst);
}

/// A card's session could not be let go at shutdown: leave without the
/// libraries' exit code.
pub fn mark_stuck() {
    LEFT.store(true, Ordering::SeqCst);
}

pub fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

/// Tests: the cards allowed again, and not stopping.
#[cfg(test)]
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
pub fn reset_for_test() {
    *BLOCKED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    STOPPING.store(false, Ordering::SeqCst);
}

// ---------------------------------------------------------------------
// The thread (not on Intel Macs, which have no built-in AI)
// ---------------------------------------------------------------------

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub use thread::*;

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
mod thread {
use super::*;
use std::collections::VecDeque;
use std::sync::{mpsc, Arc};
use std::time::Instant;

/// A model's input, and its first output (shape, values).
pub type Input = ndarray::Array4<f32>;
pub type Output = (Vec<usize>, Vec<f32>);

struct Shared {
    /// When the check being done started (None: idle).
    busy_since: Mutex<Option<Instant>>,
    /// How long the recent checks took (newest last).
    times: Mutex<VecDeque<Duration>>,
    /// When the last check ended (None: none yet).
    last_done: Mutex<Option<Instant>>,
    /// A check went over the limit and has not come back (yet).
    stuck: AtomicBool,
    /// A fault: the session is leaked, never used again.
    faulted: AtomicBool,
    /// The thread has ended and dropped its session.
    done: AtomicBool,
}

impl Shared {
    fn mark_stuck(&self) {
        if !self.stuck.swap(true, Ordering::SeqCst) {
            STUCK_THREADS.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn unstick(&self) {
        if self.stuck.swap(false, Ordering::SeqCst) {
            STUCK_THREADS.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn mark_faulted(&self) {
        self.faulted.store(true, Ordering::SeqCst);
        LEFT.store(true, Ordering::SeqCst);
    }

    fn unusable(&self) -> bool {
        self.stuck.load(Ordering::SeqCst) || self.faulted.load(Ordering::SeqCst)
    }
}

struct Job {
    input: Input,
    reply: mpsc::SyncSender<Result<Output, String>>,
    /// Its caller gave up waiting: not run on the card.
    cancelled: Arc<AtomicBool>,
}

/// Why no session was made on the card.
#[derive(Debug, PartialEq)]
pub enum LoadError {
    /// It cannot be used (the reason).
    Failed(String),
    /// Making the session took longer than the limit (the thread is left).
    Timeout(Duration),
}

pub struct CardThread {
    tx: mpsc::Sender<Job>,
    shared: Arc<Shared>,
    api: &'static str,
    /// A fixed limit for each check (None: `limit_from` the recent times).
    fixed: Option<Duration>,
}

impl CardThread {
    /// Start the thread: it runs `build` (making the session and doing a
    /// first check, which must happen on it) and then serves checks with the
    /// runner `build` gave. Returns once the session is made, with what
    /// `build` said about it.
    pub fn start<R, T>(api: &'static str, load_deadline: Duration, build: impl FnOnce() -> Result<(R, T), String> + Send + 'static) -> Result<(CardThread, T), LoadError>
    where
        R: FnMut(Input) -> Result<Output, String> + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<T, String>>(1);
        let shared = Arc::new(Shared {
            busy_since: Mutex::new(None),
            times: Mutex::new(VecDeque::new()),
            last_done: Mutex::new(None),
            stuck: AtomicBool::new(false),
            faulted: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        let sh = shared.clone();
        let leak_on_error = fault_kills_card(api);
        // (Dropped with the closure if the thread cannot start.)
        let loading = LoadingGuard::new();
        let spawned = std::thread::Builder::new().name(format!("ai-{}", api.to_lowercase().replace(' ', ""))).spawn(move || {
            let built = build();
            drop(loading);
            let mut run = match built {
                Ok((run, info)) => {
                    if ready_tx.send(Ok(info)).is_err() {
                        // Too late: no one waits for it any more. It came
                        // back, so the card is slow, not wedged: the session
                        // is let go as usual.
                        drop(run);
                        return;
                    }
                    run
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // Ends when the detector is dropped (no sender left).
            while let Ok(job) = rx.recv() {
                if job.cancelled.load(Ordering::SeqCst) {
                    let _ = job.reply.send(Err("given up before it ran".into()));
                    continue;
                }
                let t0 = Instant::now();
                *sh.busy_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(t0);
                let r = run(job.input);
                let took = t0.elapsed();
                *sh.busy_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *sh.last_done.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                // A fault that kills the session: leave the session alone
                // for good (leaked, never dropped).
                if r.is_err() && leak_on_error {
                    sh.mark_faulted();
                    let _ = job.reply.send(r);
                    std::mem::forget(run);
                    return;
                }
                // Back after the limit: slow, not wedged (its time is not
                // kept: it would raise the limit for the checks to come).
                if sh.stuck.load(Ordering::SeqCst) {
                    sh.unstick();
                    let _ = job.reply.send(r);
                    continue;
                }
                if r.is_ok() {
                    let mut t = sh.times.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_back(took);
                    if t.len() > TIMES_KEPT {
                        t.pop_front();
                    }
                }
                let _ = job.reply.send(r);
            }
            drop(run);
            sh.done.store(true, Ordering::SeqCst);
        });
        if let Err(e) = spawned {
            return Err(LoadError::Failed(format!("could not start the AI's card thread: {e}")));
        }
        match ready_rx.recv_timeout(load_deadline) {
            Ok(Ok(info)) => Ok((CardThread { tx, shared, api, fixed: fixed_limit() }, info)),
            Ok(Err(e)) => Err(LoadError::Failed(e)),
            // (`LOADING` keeps the program from running the libraries'
            // exit code while it goes on.)
            Err(mpsc::RecvTimeoutError::Timeout) => Err(LoadError::Timeout(load_deadline)),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(LoadError::Failed("the AI's card thread stopped while loading the model".into())),
        }
    }

    pub fn api(&self) -> &'static str {
        self.api
    }

    #[cfg(test)]
    pub fn with_deadline(self, deadline: Duration) -> Self {
        CardThread { fixed: Some(deadline), ..self }
    }

    /// The limit for the next check: `limit_from` the recent times, or
    /// `MAX_LIMIT` after the card has been idle (`IDLE_GAP`).
    pub fn limit(&self) -> Duration {
        if let Some(f) = self.fixed {
            return f;
        }
        let idle = self.shared.last_done.lock().unwrap_or_else(|e| e.into_inner()).is_none_or(|t| t.elapsed() > IDLE_GAP);
        if idle {
            return MAX_LIMIT;
        }
        limit_from(&self.shared.times.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().collect::<Vec<_>>())
    }

    /// A check is over the limit and has not come back, or the card faulted:
    /// no check is given to this thread now.
    pub fn stuck(&self) -> bool {
        self.shared.unusable()
    }

    /// The card faulted (its session is leaked).
    pub fn faulted(&self) -> bool {
        self.shared.faulted.load(Ordering::SeqCst)
    }

    fn timeout(&self, limit: Duration) -> CardFailure {
        let ms = limit.as_millis();
        let detail = if ms % 1000 == 0 { format!("over {} s", ms / 1000) } else { format!("over {ms} ms") };
        CardFailure { api: self.api, kind: FailureKind::Timeout, detail }
    }

    /// Do one check on the card, waiting at most the limit for it.
    pub fn run(&self, input: Input) -> Result<Output, CardFailure> {
        let limit = self.limit();
        if self.faulted() {
            return Err(CardFailure { api: self.api, kind: FailureKind::Error, detail: "the card faulted on an earlier check".into() });
        }
        if self.stuck() {
            return Err(self.timeout(limit));
        }
        let (reply, answer) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        if self.tx.send(Job { input, reply, cancelled: cancelled.clone() }).is_err() {
            return Err(CardFailure { api: self.api, kind: FailureKind::Error, detail: "the AI's card thread has stopped".into() });
        }
        let start = Instant::now();
        let slice = (limit / 4).clamp(Duration::from_millis(1), Duration::from_millis(50));
        // Time the computer spent asleep while waiting (a wait that took far
        // longer than asked for), not held against the card.
        let mut asleep = Duration::ZERO;
        let mut last = Instant::now();
        loop {
            let r = answer.recv_timeout(slice);
            let gap = last.elapsed();
            last = Instant::now();
            asleep = asleep_after(asleep, gap, slice);
            match r {
                Ok(Ok(out)) => return Ok(out),
                Ok(Err(e)) => return Err(CardFailure { api: self.api, kind: FailureKind::Error, detail: e }),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(CardFailure { api: self.api, kind: FailureKind::Error, detail: "the AI's card thread stopped during a check".into() })
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.stuck() {
                        cancelled.store(true, Ordering::SeqCst);
                        return Err(self.timeout(limit));
                    }
                    // The check being done (this one or one before it) is over the limit.
                    let late = self.shared.busy_since.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| t.elapsed().saturating_sub(asleep) > limit);
                    if late || start.elapsed().saturating_sub(asleep) > limit * QUEUE_LIMITS {
                        self.shared.mark_stuck();
                        cancelled.store(true, Ordering::SeqCst);
                        return Err(self.timeout(limit));
                    }
                }
            }
        }
    }

    /// At shutdown (no new checks start, see `begin_stop`): wait up to
    /// `wait` for the check in progress, then let the thread drop its
    /// session. True when it did; false when the card is stuck or faulted
    /// (its session is left alone) or the thread did not finish in time.
    pub fn finish(self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        // (A stuck or faulted card is not waited for.)
        while !self.stuck() && self.shared.busy_since.lock().unwrap_or_else(|e| e.into_inner()).is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        if self.stuck() || self.shared.busy_since.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            if !self.stuck() {
                // Still busy past the wait: treat it as stuck (no exit code).
                LEFT.store(true, Ordering::SeqCst);
            }
            return false;
        }
        let shared = self.shared.clone();
        drop(self); // (the thread ends and drops the session)
        while !shared.done.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        let done = shared.done.load(Ordering::SeqCst);
        if !done {
            LEFT.store(true, Ordering::SeqCst);
        }
        done
    }
}
}

#[cfg(all(test, not(all(target_os = "macos", target_arch = "x86_64"))))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    fn input() -> Input {
        ndarray::Array4::<f32>::zeros((1, 3, 2, 2))
    }

    #[test]
    fn checks_run_on_the_thread_and_a_stuck_card_is_left_at_once() {
        // A card that answers quickly, then hangs on the third check.
        let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let n2 = n.clone();
        let (t, info) = CardThread::start("OpenVINO", Duration::from_secs(5), move || {
            let run = move |i: Input| -> Result<Output, String> {
                if n2.fetch_add(1, Ordering::SeqCst) == 2 {
                    std::thread::sleep(Duration::from_secs(3600));
                }
                Ok((i.shape().to_vec(), vec![1.0]))
            };
            Ok((run, "card"))
        })
        .unwrap();
        assert_eq!(info, "card");
        let t = t.with_deadline(Duration::from_millis(200));
        assert_eq!(t.run(input()).unwrap().0, vec![1, 3, 2, 2]);
        assert!(t.run(input()).is_ok());
        let t0 = Instant::now();
        let e = t.run(input()).unwrap_err();
        assert_eq!(e.kind, FailureKind::Timeout);
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        assert!(t.stuck() && any_stuck());
        assert!(e.to_string().contains("did not answer in time (OpenVINO, over 200 ms)"), "{e}");
        // Later checks do not wait behind it.
        let t1 = Instant::now();
        assert!(t.run(input()).is_err());
        assert!(t1.elapsed() < Duration::from_millis(50));
        // Shutdown does not wait for it either.
        let t2 = Instant::now();
        assert!(!t.finish(Duration::from_secs(5)));
        assert!(t2.elapsed() < Duration::from_millis(50));
    }

    /// A runner that notes when it is dropped.
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn an_openvino_fault_leaks_the_session_and_a_cuda_error_does_not() {
        for (api, err, leaked) in [("OpenVINO", "[GPU] clFinish, error code: -5 CL_OUT_OF_RESOURCES", true), ("CUDA", "CUDA failure 2: out of memory", false)] {
            let dropped = Arc::new(AtomicBool::new(false));
            let d2 = dropped.clone();
            let (t, _) = CardThread::start(api, Duration::from_secs(5), move || {
                let guard = Dropped(d2);
                let run = move |_i: Input| -> Result<Output, String> {
                    let _keep = &guard;
                    Err(err.to_string())
                };
                Ok((run, ()))
            })
            .unwrap();
            let e = t.run(input()).unwrap_err();
            assert_eq!(e.kind, FailureKind::Error);
            assert_eq!((t.stuck(), t.faulted()), (leaked, leaked), "{api}");
            // At shutdown a healthy card's session is dropped; a faulted one is never touched.
            assert_eq!(t.finish(Duration::from_secs(2)), !leaked, "{api}");
            assert_eq!(dropped.load(Ordering::SeqCst), !leaked, "{api}");
        }
    }

    #[test]
    fn shutdown_waits_for_the_check_in_progress() {
        let dropped = Arc::new(AtomicBool::new(false));
        let d2 = dropped.clone();
        let (t, _) = CardThread::start("OpenVINO", Duration::from_secs(5), move || {
            let guard = Dropped(d2);
            let run = move |_i: Input| -> Result<Output, String> {
                let _keep = &guard;
                std::thread::sleep(Duration::from_millis(300));
                Ok((vec![1], vec![0.0]))
            };
            Ok((run, ()))
        })
        .unwrap();
        let t = Arc::new(t);
        let t2 = t.clone();
        let h = std::thread::spawn(move || t2.run(input()).is_ok());
        std::thread::sleep(Duration::from_millis(50));
        let t0 = Instant::now();
        assert!(h.join().unwrap());
        let t = Arc::try_unwrap(t).ok().unwrap();
        assert!(t.finish(Duration::from_secs(5)));
        assert!(dropped.load(Ordering::SeqCst));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_check_that_comes_back_late_shows_a_slow_card_not_a_wedged_one() {
        let dropped = Arc::new(AtomicBool::new(false));
        let d2 = dropped.clone();
        let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let n2 = n.clone();
        let (t, _) = CardThread::start("OpenVINO", Duration::from_secs(5), move || {
            let guard = Dropped(d2);
            let run = move |_i: Input| -> Result<Output, String> {
                let _keep = &guard;
                if n2.fetch_add(1, Ordering::SeqCst) == 1 {
                    std::thread::sleep(Duration::from_millis(400));
                }
                Ok((vec![1], vec![0.0]))
            };
            Ok((run, ()))
        })
        .unwrap();
        // A fresh card (no check timed yet) gets the long limit.
        assert_eq!(t.limit(), MAX_LIMIT);
        let t = t.with_deadline(Duration::from_millis(100));
        assert!(t.run(input()).is_ok());
        assert_eq!(t.run(input()).unwrap_err().kind, FailureKind::Timeout);
        assert!(t.stuck() && !t.faulted());
        let t0 = Instant::now();
        while t.stuck() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!t.stuck(), "came back: no longer stuck");
        assert!(t.run(input()).is_ok(), "and still usable");
        assert!(t.finish(Duration::from_secs(2)), "its session is let go, not leaked");
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn a_slow_load_that_comes_back_lets_its_session_go() {
        let dropped = Arc::new(AtomicBool::new(false));
        let d2 = dropped.clone();
        let r = CardThread::start("OpenVINO", Duration::from_millis(100), move || {
            std::thread::sleep(Duration::from_millis(300));
            let guard = Dropped(d2);
            Ok((
                move |_i: Input| -> Result<Output, String> {
                    let _keep = &guard;
                    Ok((vec![], vec![]))
                },
                (),
            ))
        });
        assert_eq!(r.err(), Some(LoadError::Timeout(Duration::from_millis(100))));
        let t0 = Instant::now();
        while !dropped.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(dropped.load(Ordering::SeqCst), "slow, not wedged: dropped as usual, not leaked");
    }

    #[test]
    fn a_check_its_caller_gave_up_on_is_not_run() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = calls.clone();
        let (t, _) = CardThread::start("OpenVINO", Duration::from_secs(5), move || {
            let run = move |_i: Input| -> Result<Output, String> {
                if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(400));
                }
                Ok((vec![1], vec![0.0]))
            };
            Ok((run, ()))
        })
        .unwrap();
        let t = Arc::new(t.with_deadline(Duration::from_millis(100)));
        let t2 = t.clone();
        let first = std::thread::spawn(move || t2.run(input()).is_ok());
        std::thread::sleep(Duration::from_millis(20));
        // Queued behind the slow one; its caller gives up with it.
        assert!(t.run(input()).is_err());
        assert!(!first.join().unwrap());
        let t0 = Instant::now();
        while t.stuck() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the abandoned check never reached the card");
        assert!(t.run(input()).is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_load_that_panics_is_no_longer_counted() {
        // (A counter of its own: other tests load cards at the same time.)
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let h = std::thread::spawn(|| {
            let _g = LoadingGuard::on(&COUNT);
            assert_eq!(COUNT.load(Ordering::SeqCst), 1);
            panic!("a driver bug");
        });
        assert!(h.join().is_err());
        assert_eq!(COUNT.load(Ordering::SeqCst), 0);
        // And a real load that panics ends as a failed load.
        let r = CardThread::start("OpenVINO", Duration::from_secs(5), || -> Result<(fn(Input) -> Result<Output, String>, ()), String> { panic!("a driver bug") });
        assert!(matches!(r, Err(LoadError::Failed(_))));
    }

    #[test]
    fn time_asleep_is_excused_but_only_so_much() {
        let ms = Duration::from_millis;
        let s = ms(50);
        assert_eq!(asleep_after(Duration::ZERO, ms(60), s), Duration::ZERO, "an ordinary step");
        assert_eq!(asleep_after(Duration::ZERO, ms(1500), s), Duration::ZERO, "a short stall is not sleep");
        assert_eq!(asleep_after(Duration::ZERO, ms(30_050), s), ms(30_000), "30 s asleep");
        assert_eq!(asleep_after(ms(290_000), ms(60_050), s), MAX_ASLEEP, "never over 5 minutes in all");
    }

    #[test]
    fn the_limit_follows_the_card() {
        let ms = Duration::from_millis;
        assert_eq!(limit_from(&[]), MAX_LIMIT, "until timed");
        assert_eq!(limit_from(&[ms(3); 4]), MAX_LIMIT);
        assert_eq!(limit_from(&[ms(3); 10]), MIN_LIMIT, "a fast card: 2 s");
        assert_eq!(limit_from(&[ms(150); 10]), ms(3000), "20 times the median");
        assert_eq!(limit_from(&[ms(150), ms(150), ms(150), ms(9000), ms(150)]), ms(3000), "one slow check does not move it");
        assert_eq!(limit_from(&[ms(900); 10]), MAX_LIMIT, "never over 10 s");
    }

    #[test]
    fn errors_and_slow_loads() {
        let (t, _) = CardThread::start("CUDA", Duration::from_secs(5), || Ok((|_i: Input| -> Result<Output, String> { Err("CUDA failure 2: out of memory".into()) }, ()))).unwrap();
        let e = t.run(input()).unwrap_err();
        assert_eq!((e.kind, e.to_string()), (FailureKind::Error, "AI inference (CUDA): CUDA failure 2: out of memory".to_string()));
        assert!(!t.stuck());
        let r = CardThread::start("OpenVINO", Duration::from_secs(5), || Err::<(fn(Input) -> Result<Output, String>, ()), _>("no card".into()));
        assert_eq!(r.err(), Some(LoadError::Failed("no card".into())));
        let t0 = Instant::now();
        let r = CardThread::start("OpenVINO", Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(3600));
            Ok((|_i: Input| -> Result<Output, String> { Ok((vec![], vec![])) }, ()))
        });
        assert_eq!(r.err(), Some(LoadError::Timeout(Duration::from_millis(100))));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn what_happens_after_a_failure() {
        // A timeout may be a slow card: rest, then judge (on every API).
        assert_eq!(recovery("OpenVINO", FailureKind::Timeout), Recovery::RestThenJudge);
        assert_eq!(recovery("CUDA", FailureKind::Timeout), Recovery::RestThenJudge);
        assert_eq!(recovery("DirectML", FailureKind::Timeout), Recovery::RestThenJudge);
        assert_eq!(recovery("OpenVINO", FailureKind::Error), Recovery::OffUntilRestart);
        assert_eq!(recovery("CUDA", FailureKind::Error), Recovery::CountThenRest);
        assert_eq!(recovery("DirectML", FailureKind::Error), Recovery::CountThenRest);
        assert!(off_text("OpenVINO").starts_with("the Intel graphics card stopped responding to the AI; using the processor until AmberSight restarts"));
        assert!(off_text("CUDA").contains("(CUDA)"));
        assert_eq!(load_off_text("OpenVINO", LOAD_DEADLINE), "the Intel graphics card did not finish loading the AI model within 3 minutes; using the processor until AmberSight restarts");
    }
}
