//! When the graphics card fails the AI: checks that failed for that reason
//! are done again later, and a card that keeps failing is left for the
//! processor for a while.
//!
//! - A graphics card error (CUDA, cuDNN, out of video memory, DirectML,
//!   OpenVINO, a card that stopped answering) is told from an ordinary one
//!   (a bad picture, an AI server that is down).
//! - What follows is `ai_card::recovery`. A card that stopped answering, and
//!   any OpenVINO error: the built-in AI moves to the processor at once, the
//!   check that failed is done again there, and the card stays off for the
//!   AI until AmberSight restarts (Settings and the Status page say so).
//!   Other card errors: three in a row within a minute move the built-in AI
//!   to the processor for five minutes, the check that failed is done again
//!   there, and the card is tried again after that.
//! - An event whose looks all failed that way is recorded as before (it
//!   alerts as if AI were off); its saved pictures are looked at again
//!   after 1, 5 and 15 minutes, and the result is added to the event and
//!   its AI review. That late look sends no alert, and does not use the
//!   parked-object memory (it only knows what the pictures show).
//! - Retries are queued per event (a burst is one entry) and done by one
//!   thread of their own. The queue is in memory: a restart forgets it.

use super::*;
use std::collections::VecDeque;

/// This many card errors within [`TRIP_WINDOW_MS`] (no success between) move the AI to the processor.
const TRIP_ERRORS: usize = 3;
const TRIP_WINDOW_MS: i64 = 60_000;
/// How long the processor does the work before the card is tried again.
const CPU_FOR_MS: i64 = 5 * 60_000;
/// Waits before the 1st, 2nd and 3rd late look at an event; then it is let go.
const RETRY_WAITS_S: [u64; 3] = [60, 300, 900];
/// Pictures looked at again per event.
const MAX_FRAMES: usize = 8;

/// Whether an AI error comes from the graphics card or its runtime (so
/// another try, or the processor, may work) and not from the picture.
pub fn is_gpu_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "cuda", "cudnn", "cublas", "out of memory", "failed to allocate", "bfcarena", "directml", "dxgi", "d3d12", "device removed", "device hung", "coreml", "core ml", "openvino",
        "did not answer in time", "stopped answering", "stopped responding", "card thread", "cl_out_of_resources", "clfinish", "cl_invalid",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// An AI error in one plain line for people: what the card did ("the
/// graphics card gave an error (CUDA)"), or the first line of any other
/// error, cut short, with no file paths or host names. A card error stays
/// one by [`is_gpu_error`], so late looks still find it; worded already, it
/// comes back the same.
pub fn plain_error(msg: &str) -> String {
    const MAX: usize = 120;
    if is_gpu_error(msg) {
        let m = msg.to_lowercase();
        let memory = ["out of memory", "failed to allocate", "bfcarena", "alloc_failed"].iter().any(|k| m.contains(k));
        let kind = if ["cuda", "cudnn", "cublas"].iter().any(|k| m.contains(k)) {
            "CUDA"
        } else if ["directml", "dxgi", "d3d12"].iter().any(|k| m.contains(k)) {
            "DirectML"
        } else if m.contains("openvino") {
            "OpenVINO"
        } else if m.contains("coreml") || m.contains("core ml") {
            "CoreML"
        } else if m.contains("device removed") {
            "device removed"
        } else if m.contains("device hung") {
            "device hung"
        } else {
            ""
        };
        let stopped = m.contains("did not answer in time") || m.contains("stopped answering") || m.contains("stopped responding");
        let what = if memory {
            "the graphics card ran out of memory"
        } else if stopped {
            "the graphics card stopped responding"
        } else {
            "the graphics card gave an error"
        };
        return if kind.is_empty() { what.to_string() } else { format!("{what} ({kind})") };
    }
    let clean = clean_error(msg);
    let line = clean.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("unknown error");
    if line.chars().count() <= MAX {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX).collect();
    // At a word when there is one near the end.
    let cut = match cut.rfind(' ') {
        Some(i) if i > MAX / 2 => cut[..i].trim_end_matches([',', ';', ':']).to_string(),
        _ => cut,
    };
    format!("{cut}\u{2026}")
}

/// An AI error with file paths cut to their file name and `hostname=...`
/// parts taken out (onnxruntime puts both in its messages). Web addresses
/// stay, without any sign-in in them.
pub fn clean_error(msg: &str) -> String {
    msg.lines()
        .map(|line| {
            // (Paths first: one with spaces in it is more than one word.)
            let line = cut_paths(line);
            let words: Vec<String> = line.split_whitespace().filter(|w| !w.to_lowercase().starts_with("hostname=")).map(cut_path).collect();
            let mut out = crate::engine::redact_text(&words.join(" "));
            while out.contains("; ;") {
                out = out.replace("; ;", ";");
            }
            out.trim_end_matches([' ', ';']).to_string()
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// File paths in a line cut to their last part, also paths with spaces in
/// them (`C:\Users\Jo Smith\models\a.onnx: bad` is `a.onnx: bad`): a path
/// starts at a drive (`C:\`) or a separator at the start of a word (not a
/// web address's "://"), and runs to its last separator before a ':', ';',
/// ',', quote or bracket.
fn cut_paths(line: &str) -> String {
    let b = line.as_bytes();
    let sep = |c: u8| c == b'/' || c == b'\\';
    let word_start = |i: usize| i == 0 || matches!(b[i - 1], b' ' | b'\t' | b'=' | b'"' | b'\'' | b'(' | b'[' | b'<');
    let mut out = String::with_capacity(line.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        let drive = i + 2 < b.len() && b[i].is_ascii_alphabetic() && b[i + 1] == b':' && sep(b[i + 2]) && word_start(i);
        let rooted = sep(b[i]) && word_start(i);
        if !(drive || rooted) {
            i += 1;
            continue;
        }
        let body = if drive { i + 3 } else { i + 1 };
        let end = line[body..].find([':', ';', ',', '"', '\'', ')', ']', '>']).map_or(b.len(), |e| body + e);
        let last = line[i..end].rfind(['/', '\\']).map(|l| i + l);
        match last {
            // A path: two or more separators (a lone "/" or "a/b" is a word).
            Some(l) if line[i..=l].matches(['/', '\\']).count() >= 2 => {
                out.push_str(&line[copied..i]);
                copied = l + 1;
                i = l + 1;
            }
            _ => i += 1,
        }
    }
    out.push_str(&line[copied..]);
    out
}

/// A word holding a file path (two or more separators, not a web address):
/// the path cut to its last part, keeping what stood before it ("file=").
fn cut_path(w: &str) -> String {
    if w.contains("://") || w.matches(['/', '\\']).count() < 2 {
        return w.to_string();
    }
    let Some(first) = w.find(['/', '\\']) else { return w.to_string() };
    // A Windows drive ("C:\...") belongs to the path.
    let b = w.as_bytes();
    let start = if first >= 2 && b[first - 1] == b':' && b[first - 2].is_ascii_alphabetic() { first - 2 } else { first };
    let last = w.rfind(['/', '\\']).unwrap_or(first);
    format!("{}{}", &w[..start], &w[last + 1..])
}

/// Word the errors in a saved AI review for showing: `error` one plain
/// line, `error_detail` the longer text, cleaned (reviews saved before
/// errors were worded hold the raw text).
pub fn clean_review_errors(review: &mut serde_json::Value) {
    let Some(checks) = review["checks"].as_array_mut() else { return };
    for c in checks.iter_mut() {
        let Some(raw) = c["error"].as_str().map(String::from) else { continue };
        let detail = clean_error(c["error_detail"].as_str().unwrap_or(&raw));
        let plain = plain_error(&raw);
        c["error_detail"] = if detail != plain && !detail.is_empty() { detail.into() } else { serde_json::Value::Null };
        c["error"] = plain.into();
    }
}

/// The wait before late look number `attempt` (0 = the first), or None when
/// the event has had its last one.
pub fn retry_wait(attempt: u32) -> Option<u64> {
    RETRY_WAITS_S.get(attempt as usize).copied()
}

/// The recent card errors and whether the processor is standing in.
#[derive(Default)]
pub struct Health {
    errors: VecDeque<i64>,
    /// When the AI moved to the processor.
    cpu_since: Option<i64>,
}

impl Health {
    /// A card error at `now` (ms); true when it is time to leave the card.
    pub fn error(&mut self, now: i64) -> bool {
        self.errors.push_back(now);
        while self.errors.front().is_some_and(|t| now - t > TRIP_WINDOW_MS) {
            self.errors.pop_front();
        }
        self.errors.len() >= TRIP_ERRORS
    }

    /// A check worked: errors must be in a row to count.
    pub fn ok(&mut self) {
        self.errors.clear();
    }

    pub fn left_card(&mut self, now: i64) {
        self.errors.clear();
        self.cpu_since = Some(now);
    }

    /// Time to try the card again (true once: asking again says no until the next `left_card`).
    pub fn card_due(&mut self, now: i64) -> bool {
        if self.cpu_since.is_some_and(|t| now - t >= CPU_FOR_MS) {
            self.cpu_since = None;
            return true;
        }
        false
    }
}

static HEALTH: Mutex<Option<Health>> = Mutex::new(None);

/// The card's detector set aside after a check went over its limit, until
/// it is judged (`Engine::judge_slow_card`).
static SLOW_CARD: Mutex<Option<Arc<crate::ai::AiDetector>>> = Mutex::new(None);

/// When checks on the card went over their limit lately (ms).
static SLOW_EPISODES: Mutex<VecDeque<i64>> = Mutex::new(VecDeque::new());
/// This many slow checks within `FLAP_WINDOW_MS` keep the card off until a restart.
const FLAP_EPISODES: usize = 3;
const FLAP_WINDOW_MS: i64 = 3_600_000;

/// Note a slow check at `now`; true when it is the third within an hour.
fn too_slow_too_often(episodes: &mut VecDeque<i64>, now: i64) -> bool {
    episodes.push_back(now);
    while episodes.front().is_some_and(|t| now - t > FLAP_WINDOW_MS) {
        episodes.pop_front();
    }
    episodes.len() >= FLAP_EPISODES
}

fn health<R>(f: impl FnOnce(&mut Health) -> R) -> R {
    f(HEALTH.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Health::default))
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl Engine {
    /// `ai_detect_inner` with the card's errors handled (see the top of this file).
    pub(super) fn ai_detect_guarded(&self, jpeg: &[u8], min_score: f32) -> anyhow::Result<Vec<crate::ai::Detection>> {
        self.guarded(&|e: &Engine| e.ai_detect_inner(jpeg, min_score))
    }

    /// The AI track (playback): on the card, the whole picture and its
    /// tiles at full size; on the processor, the whole picture. With the
    /// card's errors and time limit handled as for every other check.
    pub fn ai_detect_tiled(&self, img: &image::RgbImage, min_score: f32) -> anyhow::Result<Vec<crate::ai::Detection>> {
        self.guarded(&|e: &Engine| match e.detector() {
            Some(d) if d.gpu_name().is_some() => d.detect_tiled(img, min_score),
            Some(d) => d.detect_rgb(img, min_score),
            None => anyhow::bail!("the built-in AI is not loaded"),
        })
    }

    /// Run a check (`run`, which uses the detector loaded now) with the
    /// card's errors handled (see the top of this file).
    fn guarded(&self, run: &dyn Fn(&Engine) -> anyhow::Result<Vec<crate::ai::Detection>>) -> anyhow::Result<Vec<crate::ai::Detection>> {
        use crate::ai_card::{recovery, CardFailure, FailureKind, Recovery};
        let used = self.detector().filter(|d| d.gpu_name().is_some());
        let mut r = run(self);
        match (&r, &used) {
            (Err(e), Some(d)) if e.downcast_ref::<CardFailure>().is_some() || is_gpu_error(&format!("{e:#}")) => {
                let msg = format!("{e:#}");
                let kind = e.downcast_ref::<CardFailure>().map_or(FailureKind::Error, |f| f.kind);
                match recovery(d.gpu_api().unwrap_or(""), kind) {
                    Recovery::OffUntilRestart => self.card_off(d, &msg),
                    Recovery::RestThenJudge => self.card_slow(d, &msg),
                    Recovery::CountThenRest => {
                        if health(|h| h.error(now_ms())) {
                            self.leave_card(d, &msg);
                        }
                    }
                }
                // Moved to the processor (by this call or another): do this check there.
                if self.detector().is_some_and(|n| !Arc::ptr_eq(&n, d)) {
                    r = run(self);
                }
            }
            (Ok(_), Some(_)) => health(|h| h.ok()),
            (Ok(_), None) => {
                if health(|h| h.card_due(now_ms())) && self.judge_slow_card() && crate::ai_card::blocked().is_none() {
                    self.try_card_again();
                }
            }
            _ => {}
        }
        r
    }

    /// A check on the card went over its limit (`Recovery::RestThenJudge`):
    /// the AI runs on the processor for five minutes, and the card's
    /// detector is kept aside to be judged then (`judge_slow_card`).
    fn card_slow(&self, used: &Arc<crate::ai::AiDetector>, msg: &str) {
        let _one = super::AI_ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        if !self.detector().is_some_and(|d| Arc::ptr_eq(&d, used)) {
            return; // another thread did it
        }
        let (enabled, model, threads, mb) = {
            let c = self.config.lock().unwrap();
            (c.ai.enabled, c.ai.model.clone(), c.ai.threads, c.ai.gpu_memory_mb)
        };
        let Some(model) = model.filter(|_| enabled) else { return };
        let api = used.gpu_api().unwrap_or("");
        // The third slow check within an hour: the card stays off until a
        // restart (it is still judged as below, and let go or left alone).
        let flapping = too_slow_too_often(&mut SLOW_EPISODES.lock().unwrap_or_else(|e| e.into_inner()), now_ms());
        let why = if flapping { crate::ai_card::keeps_slow_text(api) } else { crate::ai_card::slow_text(api) };
        if flapping {
            crate::ai_card::block(&why);
        }
        let short = msg.lines().next().unwrap_or("").chars().take(160).collect::<String>();
        warn!("AI: {why} ({short})");
        match crate::ai::AiDetector::load_on(&model, threads, false, None, mb) {
            Ok(d) => {
                crate::ai::set_gpu_note(Some(why));
                *SLOW_CARD.lock().unwrap_or_else(|e| e.into_inner()) = Some(used.clone());
                *self.ai.write().unwrap() = Some(Arc::new(d));
                health(|h| h.left_card(now_ms()));
            }
            Err(e) => warn!("AI: the processor could not take over from the graphics card: {e:#}"),
        }
    }

    /// Five minutes after a check on the card went over its limit: a card
    /// thread still stuck in it means the card is wedged (off until restart,
    /// its session left alone for good); one that came back means it was
    /// only slow (it is let go, and the card is tried again). True when the
    /// card may be tried again.
    fn judge_slow_card(&self) -> bool {
        let Some(slow) = SLOW_CARD.lock().unwrap_or_else(|e| e.into_inner()).take() else { return true };
        if slow.card_stuck() {
            let why = crate::ai_card::off_text(slow.gpu_api().unwrap_or(""));
            warn!("AI: a check on the graphics card has not come back in five minutes: {why}");
            crate::ai_card::block(&why);
            {
                let _one = super::AI_ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
                crate::ai::set_gpu_note(Some(why));
            }
            // Never dropped: dropping could wait on the card.
            std::mem::forget(slow);
            crate::ai_card::mark_stuck();
            return false;
        }
        info!("AI: the slow check on the graphics card came back; trying the card again");
        true
    }

    /// The card stopped answering or gave an error that turns it off for
    /// the AI (`ai_card::recovery`): the AI runs on the processor until
    /// AmberSight restarts.
    fn card_off(&self, used: &Arc<crate::ai::AiDetector>, msg: &str) {
        let api = used.gpu_api().unwrap_or("");
        let short = msg.lines().next().unwrap_or("").chars().take(160).collect::<String>();
        let why = crate::ai_card::off_text(api);
        crate::ai_card::block(&why);
        let _one = super::AI_ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        if !self.detector().is_some_and(|d| Arc::ptr_eq(&d, used)) {
            return; // another thread did it
        }
        let (enabled, model, threads, mb) = {
            let c = self.config.lock().unwrap();
            (c.ai.enabled, c.ai.model.clone(), c.ai.threads, c.ai.gpu_memory_mb)
        };
        let Some(model) = model.filter(|_| enabled) else { return };
        warn!("AI: {why} ({short})");
        match crate::ai::AiDetector::load_on(&model, threads, false, None, mb) {
            Ok(d) => {
                crate::ai::set_gpu_note(Some(why));
                *self.ai.write().unwrap() = Some(Arc::new(d));
            }
            Err(e) => warn!("AI: the processor could not take over from the graphics card: {e:#}"),
        }
    }

    /// Replace the AI that runs on the card with one on the processor.
    fn leave_card(&self, used: &Arc<crate::ai::AiDetector>, msg: &str) {
        let _one = super::AI_ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        if !self.detector().is_some_and(|d| Arc::ptr_eq(&d, used)) {
            return; // another thread did it
        }
        let (enabled, model, threads, mb) = {
            let c = self.config.lock().unwrap();
            (c.ai.enabled, c.ai.model.clone(), c.ai.threads, c.ai.gpu_memory_mb)
        };
        // The AI was turned off (or has no model): do not bring it back.
        let Some(model) = model.filter(|_| enabled) else { return };
        match crate::ai::AiDetector::load_on(&model, threads, false, None, mb) {
            Ok(d) => {
                let short = msg.lines().next().unwrap_or("").chars().take(160).collect::<String>();
                warn!("AI: the graphics card keeps giving errors ({short}); the AI checks run on the processor for 5 minutes, then the card is tried again");
                crate::ai::set_gpu_note(Some("the graphics card gave errors, so the AI runs on the processor for a while".into()));
                *self.ai.write().unwrap() = Some(Arc::new(d));
                health(|h| h.left_card(now_ms()));
            }
            Err(e) => warn!("AI: the graphics card keeps giving errors and the processor could not take over: {e:#}"),
        }
    }

    /// Load the AI as the settings say again (on the card if it can).
    /// Move the built-in AI (running on the processor) onto the graphics
    /// card in the background, as the settings say: once the card's session
    /// is made and has passed one check it takes over, unless the AI was
    /// loaded again meanwhile (newer settings). Recording and the AI on the
    /// processor never wait for the card.
    pub fn move_to_card(&self) {
        let (gpu, model, threads, gpu_name, mb) = {
            let c = self.config.lock().unwrap();
            (c.ai.gpu, c.ai.model.clone(), c.ai.threads, c.ai.gpu_name.clone(), c.ai.gpu_memory_mb)
        };
        let Some(cpu) = self.detector() else { return };
        if !gpu || cpu.gpu_name().is_some() {
            if !gpu {
                #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
                crate::gpu_addon::none_wanted();
                self.note_for(&cpu, None);
            }
            return;
        }
        if let Some(b) = crate::ai_card::blocked() {
            self.note_for(&cpu, Some(b));
            return;
        }
        // Resting on the processor after a slow check (a reload meanwhile:
        // settings saved, an add-on arrived): no new session on the card
        // while that check may still be stuck there. When the rest is over
        // the slow card is judged first, then the card is tried again.
        if SLOW_CARD.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            return;
        }
        let (Some(model), Some(me)) = (model, self.me.get().cloned()) else { return };
        let spawned = thread::Builder::new().name("ai-to-card".into()).spawn(move || {
            // The whole move, a session made and let go again included, is
            // card work the program must not run the libraries' exit code under.
            let _loading = crate::ai_card::LoadingGuard::new();
            let tried = crate::ai::AiDetector::try_card(&model, threads, gpu_name.as_deref(), mb);
            let stale = match me.upgrade() {
                Some(engine) => {
                    let _one = super::AI_ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
                    // Not when AmberSight is stopping (stop_ai has taken the
                    // detector, under the same lock), and only in place of
                    // the processor's detector this move was started for;
                    // Settings' note is written only by the move that counts.
                    let current = !crate::ai_card::stopping() && engine.detector().is_some_and(|now| Arc::ptr_eq(&now, &cpu));
                    match tried {
                        crate::ai::CardTry::OnCard(d) if current => {
                            info!("AI: moved onto the graphics card {} ({})", d.gpu_name().unwrap_or(""), d.gpu_api().unwrap_or(""));
                            crate::ai::set_gpu_note(None);
                            *engine.ai.write().unwrap() = Some(Arc::new(d));
                            None
                        }
                        crate::ai::CardTry::OnCard(d) => Some(d),
                        crate::ai::CardTry::NotUsed(note) => {
                            if current {
                                crate::ai::set_gpu_note(note);
                            }
                            None
                        }
                    }
                }
                None => match tried {
                    crate::ai::CardTry::OnCard(d) => Some(d),
                    crate::ai::CardTry::NotUsed(_) => None,
                },
            };
            // A move overtaken by newer settings or shutdown: its session is
            // let go here, before the guard above ends.
            if let Some(d) = stale {
                d.finish(std::time::Duration::from_secs(5));
            }
        });
        if let Err(e) = spawned {
            warn!("AI: could not start loading the model on the graphics card: {e}");
        }
    }

    /// Write Settings' note about the card, under the AI lock, when `cpu` is
    /// still the detector in use (a later reload's note wins).
    fn note_for(&self, cpu: &Arc<crate::ai::AiDetector>, note: Option<String>) {
        let _one = super::AI_ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        if self.detector().is_some_and(|now| Arc::ptr_eq(&now, cpu)) {
            crate::ai::set_gpu_note(note);
        }
    }

    /// At shutdown, after the cameras have stopped: no new checks start on
    /// the card; the one in progress gets up to `wait` to finish, then the
    /// card's session is let go. A stuck or faulted card is left alone (the
    /// program then leaves without the libraries' exit code, see main.rs).
    pub fn stop_ai(&self, wait: std::time::Duration) {
        crate::ai_card::begin_stop();
        // (Under the lock a move onto the card installs with: once taken
        // here, no move puts a detector back.)
        let (slow, current) = {
            let _one = super::AI_ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
            (SLOW_CARD.lock().unwrap_or_else(|e| e.into_inner()).take(), self.ai.write().unwrap().take())
        };
        // A card set aside after a slow check: let go if it came back.
        if let Some(slow) = slow {
            match Arc::try_unwrap(slow) {
                Ok(d) if !d.card_stuck() => {
                    d.finish(wait);
                }
                Ok(d) => std::mem::forget(d),
                Err(a) => std::mem::forget(a),
            }
        }
        let Some(mut d) = current else { return };
        if d.gpu_api().is_none() {
            return;
        }
        let until = std::time::Instant::now() + wait;
        loop {
            match Arc::try_unwrap(d) {
                Ok(det) => {
                    if det.finish(until.saturating_duration_since(std::time::Instant::now())) {
                        info!("shutdown: the AI let go of the graphics card");
                    } else {
                        warn!("shutdown: the graphics card's AI session was left alone (the card stopped responding or a check did not finish)");
                    }
                    return;
                }
                // A check still holds it.
                Err(back) => {
                    if std::time::Instant::now() >= until {
                        std::mem::forget(back);
                        crate::ai_card::mark_stuck();
                        warn!("shutdown: an AI check on the graphics card did not finish in time; its session is left alone");
                        return;
                    }
                    d = back;
                    thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }

    fn try_card_again(&self) {
        info!("AI: trying the graphics card again");
        // (The AI is on the processor already: only the card is loaded.)
        self.move_to_card();
        // Still not on the card (it cannot take the model): look again later.
        if self.config.lock().unwrap().ai.gpu && self.detector().is_some_and(|d| d.gpu_name().is_none()) {
            health(|h| h.left_card(now_ms()));
        }
    }
}

/// An event waiting for a late look at its pictures.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: String,
    pub camera: String,
    pub start: i64,
    /// Late looks done so far.
    pub attempt: u32,
    pub due_ms: i64,
}

/// Add `job` unless the event is queued already (one entry per event).
pub fn add_job(queue: &mut Vec<Job>, job: Job) {
    if !queue.iter().any(|j| j.id == job.id) {
        queue.push(job);
    }
}

/// What to do with a job after its late look failed again: queued again
/// later, or None when it has had its last.
pub fn after_failure(job: &Job, now: i64) -> Option<Job> {
    let wait = retry_wait(job.attempt + 1)?;
    Some(Job { attempt: job.attempt + 1, due_ms: now + wait as i64 * 1000, ..job.clone() })
}

static QUEUE: Mutex<Vec<Job>> = Mutex::new(Vec::new());
static WORKING: AtomicBool = AtomicBool::new(false);

/// An event just recorded whose looks all failed on the card: look at its
/// saved pictures again later.
pub(super) fn queue_failed(engine: &Arc<Engine>, id: &str, camera: &str, start: i64, ai: &super::ai_check::AiOutcome) {
    let failed = !ai.ran && ai.log.iter().any(|c| !c.frame.is_empty() && c.error.as_deref().is_some_and(is_gpu_error));
    if !failed {
        return;
    }
    let first = retry_wait(0).unwrap_or(60) as i64 * 1000;
    add_job(&mut QUEUE.lock().unwrap(), Job { id: id.into(), camera: camera.into(), start, attempt: 0, due_ms: now_ms() + first });
    info!("camera '{camera}': the AI check of event {id} failed on the graphics card; it will be done again in a minute");
    if !WORKING.swap(true, Ordering::SeqCst) {
        let eng = engine.clone();
        let _ = thread::Builder::new().name("ai-late-checks".into()).spawn(move || work(eng));
    }
}

fn work(engine: Arc<Engine>) {
    loop {
        thread::sleep(std::time::Duration::from_secs(5));
        let due = {
            let mut q = QUEUE.lock().unwrap();
            let now = now_ms();
            match q.iter().position(|j| j.due_ms <= now) {
                Some(i) => Some(q.remove(i)),
                None => {
                    if q.is_empty() {
                        WORKING.store(false, Ordering::SeqCst);
                        return;
                    }
                    None
                }
            }
        };
        let Some(job) = due else { continue };
        match late_check(&engine, &job) {
            Ok(true) => {}
            // The card failed again, or the record could not be read or saved.
            Ok(false) | Err(_) => match after_failure(&job, now_ms()) {
                Some(next) => add_job(&mut QUEUE.lock().unwrap(), next),
                None => warn!("camera '{}': the AI check of event {} could not be done; it stays unchecked", job.camera, job.id),
            },
        }
    }
}

/// Look at the failed checks' pictures again and store the result. Ok(false)
/// when the card failed again (try later); Ok(true) when done or nothing to do.
fn late_check(engine: &Arc<Engine>, job: &Job) -> anyhow::Result<bool> {
    let Some(text) = engine.db.ai_review(&job.id)? else { return Ok(true) };
    let review: serde_json::Value = serde_json::from_str(&text)?;
    let todo: Vec<usize> = review["checks"]
        .as_array()
        .map(|a| a.iter().filter(|c| c["frame"] == true && c["error"].as_str().is_some_and(is_gpu_error)).filter_map(|c| c["n"].as_u64()).map(|n| n as usize).take(MAX_FRAMES).collect())
        .unwrap_or_default();
    if todo.is_empty() {
        return Ok(true);
    }
    let dir = engine.ai_review_dir(&job.id);
    let mut results: Vec<(usize, Vec<crate::ai::Detection>)> = Vec::new();
    for n in todo {
        let Ok(jpeg) = std::fs::read(dir.join(format!("{n}.jpg"))) else { continue };
        match engine.ai_detect(&jpeg, 0.25) {
            Ok(d) => results.push((n, d)),
            Err(e) if is_gpu_error(&format!("{e:#}")) => return Ok(false),
            Err(_) => {}
        }
    }
    let (wanted, min_score) = {
        let c = engine.config.lock().unwrap();
        match c.cameras.iter().find(|x| x.name == job.camera) {
            Some(cam) => (cam.ai_labels.clone().unwrap_or_else(|| c.ai.labels.clone()), cam.ai_min_score.unwrap_or(c.ai.min_score)),
            None => (c.ai.labels.clone(), c.ai.min_score),
        }
    };
    // Edit the review as stored now (it may have changed, or the event may be
    // gone, while the pictures were checked): only what the late check found.
    let source = super::ai_check::source_name(engine);
    let mut labels: Vec<String> = Vec::new();
    if !engine.db.update_ai_review(&job.id, |cur| labels = apply_results(cur, &results, &wanted, min_score, &source))? {
        return Ok(true);
    }
    if !labels.is_empty() {
        engine.db.add_event_objects(&job.id, &labels)?;
    } else if !results.is_empty() {
        engine.db.set_event_unconfirmed(&job.id)?;
    }
    info!("camera '{}': event {} was checked late ({})", job.camera, job.id, if labels.is_empty() { "nothing wanted".to_string() } else { labels.join(", ") });
    Ok(true)
}

/// Put late results into an AI review record; the wanted labels they found.
/// Checks with no result keep their error.
pub fn apply_results(review: &mut serde_json::Value, results: &[(usize, Vec<crate::ai::Detection>)], wanted: &[String], min_score: f32, source: &str) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    if let Some(checks) = review["checks"].as_array_mut() {
        for c in checks.iter_mut() {
            let Some((_, seen)) = results.iter().find(|(n, _)| c["n"].as_u64() == Some(*n as u64)) else { continue };
            c["error"] = serde_json::Value::Null;
            c["error_detail"] = serde_json::Value::Null;
            c["seen"] = serde_json::json!(seen);
            c["late"] = true.into();
            for d in seen.iter().filter(|d| d.score >= min_score && wanted.contains(&d.label)) {
                if !labels.contains(&d.label) {
                    labels.push(d.label.clone());
                }
            }
        }
    }
    if !results.is_empty() {
        let confirmed = !labels.is_empty();
        review["source"] = source.into();
        review["confirmed"] = confirmed.into();
        let mut all: Vec<String> = review["labels"].as_array().map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect()).unwrap_or_default();
        for l in &labels {
            if !all.contains(l) {
                all.push(l.clone());
            }
        }
        review["labels"] = serde_json::json!(all);
        if review["alert"]["why"] == "ai_failed" {
            let sent = review["alert"]["sent"].as_bool().unwrap_or(false);
            review["alert"]["why"] = if sent { "sent" } else if confirmed { "other" } else { "ai_no_wanted" }.into();
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::Detection;

    /// The error a real event showed (2026-11-16).
    const REAL: &str = "AI inference: /home/runner/work/ort-artifacts/ort-artifacts/onnxruntime/onnxruntime/core/providers/cuda/cuda_call.cc:154 std::conditional_t<THRW, void, Status> onnxruntime::CudaCall(ERRTYPE, const char *, const char *, SUCCTYPE, const char *, const char *, const int) [ERRTYPE = cudnnStatus_t, THRW = true, SUCCTYPE = cudnnStatus_t] /home/runner/work/ort-artifacts/ort-artifacts/onnxruntime/onnxruntime/core/providers/cuda/cuda_call.cc:146 std::conditional_t<THRW, void, Status> onnxruntime::CudaCall(ERRTYPE, const char *, const char *, SUCCTYPE, const char *, const char *, const int) [ERRTYPE = cudnnStatus_t, THRW = true, SUCCTYPE = cudnnStatus_t] CUDNN failure 4000: CUDNN_STATUS_INTERNAL_ERROR ; GPU=0 ; hostname=box ; file=/home/runner/work/ort-artifacts/ort-artifacts/onnxruntime/onnxruntime/core/providers/cuda/cuda_stream_handle.cc ; line=82 ; expr=cudnnCreate(&cudnn_handle_);";

    #[test]
    fn errors_are_worded_plainly_without_paths_or_host_names() {
        assert_eq!(plain_error(REAL), "the graphics card gave an error (CUDA)");
        let d = clean_error(REAL);
        assert!(!d.contains("hostname") && !d.contains("box") && !d.contains("/home") && !d.contains("runner"), "{d}");
        assert!(d.contains("cuda_call.cc:154") && d.contains("CUDNN_STATUS_INTERNAL_ERROR ; GPU=0 ; file=cuda_stream_handle.cc ; line=82"), "{d}");
        assert_eq!(plain_error("AI inference: CUDA failure 2: out of memory ; GPU=0"), "the graphics card ran out of memory (CUDA)");
        assert_eq!(plain_error("AI inference: DirectML: DXGI_ERROR_DEVICE_REMOVED"), "the graphics card gave an error (DirectML)");
        assert_eq!(plain_error("Failed to allocate memory for requested buffer"), "the graphics card ran out of memory");
        // OpenVINO, and a card that stopped answering (ai_card.rs).
        assert_eq!(plain_error("AI inference (OpenVINO): Exception from src/inference/src/cpp/infer_request.cpp:223: [GPU] clFinish, error code: -36"), "the graphics card gave an error (OpenVINO)");
        assert_eq!(plain_error("the graphics card did not answer in time (OpenVINO, over 10 s)"), "the graphics card stopped responding (OpenVINO)");
        assert_eq!(plain_error("the graphics card did not answer in time (CUDA, over 2 s)"), "the graphics card stopped responding (CUDA)");
        // The OpenCL errors of a card that faulted.
        assert_eq!(plain_error("AI inference: Exception from src/plugins/intel_gpu/src/runtime/ocl/ocl_stream.cpp:419: [GPU] clFinish, error code: -5 CL_OUT_OF_RESOURCES"), "the graphics card gave an error");
        assert!(is_gpu_error("[GPU] CL_INVALID_COMMAND_QUEUE"));
        // Worded once, the same again, and still a card error for late looks.
        for raw in [
            REAL,
            "AI inference: CUDA failure 2: out of memory",
            "DirectML: DXGI_ERROR_DEVICE_HUNG",
            "Failed to allocate memory",
            "device removed",
            "coreml failed",
            "AI inference (OpenVINO): [GPU] error",
            "the graphics card did not answer in time (OpenVINO, over 10 s)",
            "AI inference (Core ML): failed",
        ] {
            let p = plain_error(raw);
            assert_eq!(plain_error(&p), p);
            assert!(is_gpu_error(&p), "{p}");
        }
        // Other errors: their first line, cut short, web addresses kept.
        assert_eq!(plain_error("AI server at http://192.0.2.5/v1/vision/detection did not answer with JSON\ncaused by: x"), "AI server at http://192.0.2.5/v1/vision/detection did not answer with JSON");
        assert_eq!(plain_error("could not read C:\\Users\\someone\\models\\a.onnx: bad file hostname=box"), "could not read a.onnx: bad file");
        // A path with spaces in it: no piece of it is left.
        assert_eq!(plain_error("could not read C:\\Users\\Jo Smith\\models\\a.onnx: bad file"), "could not read a.onnx: bad file");
        assert_eq!(clean_error("load failed; file=/home/jo smith/m/a.onnx ; line=3"), "load failed; file=a.onnx ; line=3");
        // A sign-in in an AI server's address is never shown.
        for raw in ["AI server at http://admin:s3cret@192.0.2.5/v1/vision/detection did not answer", "connection to http://admin:s3cret@192.0.2.5:5000/v1 refused"] {
            for shown in [plain_error(raw), clean_error(raw)] {
                assert!(!shown.contains("s3cret") && !shown.contains("admin"), "{shown}");
                assert!(shown.contains("192.0.2.5"), "{shown}");
            }
        }
        let long = plain_error(&"word ".repeat(60));
        assert!(long.chars().count() <= 121 && long.ends_with('\u{2026}'), "{long}");
    }

    #[test]
    fn saved_reviews_are_cleaned_when_shown() {
        let mut r = serde_json::json!({ "checks": [{ "n": 0, "error": REAL }, { "n": 1, "error": null }, { "n": 2, "error": "AI server down" }] });
        clean_review_errors(&mut r);
        assert_eq!(r["checks"][0]["error"], "the graphics card gave an error (CUDA)");
        assert!(r["checks"][0]["error_detail"].as_str().unwrap().contains("CUDNN_STATUS_INTERNAL_ERROR"));
        assert!(!r.to_string().contains("hostname") && !r.to_string().contains("/home/runner"));
        assert!(r["checks"][1]["error"].is_null());
        assert_eq!(r["checks"][2]["error"], "AI server down");
        assert!(r["checks"][2]["error_detail"].is_null());
        // Twice is the same as once.
        let once = r.clone();
        clean_review_errors(&mut r);
        assert_eq!(r, once);
    }

    #[test]
    fn card_errors_are_told_from_ordinary_ones() {
        assert!(is_gpu_error("AI inference: CUDA failure 2: out of memory ; GPU=0"));
        assert!(is_gpu_error("AI inference: cudnn error: CUDNN_STATUS_ALLOC_FAILED"));
        assert!(is_gpu_error("AI inference: Failed to allocate memory for requested buffer"));
        assert!(is_gpu_error("AI inference: DirectML: DXGI_ERROR_DEVICE_REMOVED"));
        // The real one (2026-11-16, the card full with another program's model).
        assert!(is_gpu_error("AI inference: /home/runner/work/ort-artifacts/ort-artifacts/onnxruntime/onnxruntime/core/providers/cuda/cuda_call.cc:154 std::conditional_t<THRW, void, Status> onnxruntime::CudaCall(ERRTYPE, const char *, const char *, SUCCTYPE, const char *, const char *, const int) [ERR"));
        assert!(!is_gpu_error("could not decode image"));
        assert!(!is_gpu_error("unexpected AI model output shape [1, 2]"));
        assert!(!is_gpu_error("AI server at http://192.0.2.5/v1/vision/detection did not answer with JSON"));
        assert!(!is_gpu_error("no AI model or server configured"));
    }

    #[test]
    fn late_looks_have_a_last_one() {
        assert_eq!(retry_wait(0), Some(60));
        assert_eq!(retry_wait(2), Some(900));
        assert_eq!(retry_wait(3), None);
        let j = Job { id: "a-1".into(), camera: "A".into(), start: 1, attempt: 0, due_ms: 0 };
        let n1 = after_failure(&j, 1000).unwrap();
        assert_eq!((n1.attempt, n1.due_ms), (1, 301_000));
        let n2 = after_failure(&n1, 0).unwrap();
        assert_eq!(n2.attempt, 2);
        assert!(after_failure(&n2, 0).is_none());
    }

    #[test]
    fn a_burst_is_one_entry_per_event() {
        let mut q = Vec::new();
        let j = |id: &str| Job { id: id.into(), camera: "A".into(), start: 1, attempt: 0, due_ms: 0 };
        for _ in 0..30 {
            add_job(&mut q, j("a-1"));
        }
        add_job(&mut q, j("a-2"));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn three_errors_in_a_row_within_a_minute_leave_the_card() {
        let mut h = Health::default();
        assert!(!h.error(0));
        assert!(!h.error(1_000));
        assert!(h.error(2_000));
        // Spread out over more than a minute: not enough.
        let mut h = Health::default();
        assert!(!h.error(0));
        assert!(!h.error(40_000));
        assert!(!h.error(80_000));
        // A check that worked in between starts the count again.
        let mut h = Health::default();
        h.error(0);
        h.error(1);
        h.ok();
        assert!(!h.error(2));
    }

    #[test]
    fn a_card_slow_three_times_in_an_hour_stays_off() {
        let mut e = VecDeque::new();
        assert!(!too_slow_too_often(&mut e, 0));
        assert!(!too_slow_too_often(&mut e, 1_000_000));
        assert!(too_slow_too_often(&mut e, 2_000_000), "the third within an hour");
        let mut e = VecDeque::new();
        assert!(!too_slow_too_often(&mut e, 0));
        assert!(!too_slow_too_often(&mut e, 2_000_000));
        assert!(!too_slow_too_often(&mut e, 4_000_000), "spread over more than an hour");
    }

    #[test]
    fn the_card_is_tried_again_after_five_minutes_once() {
        let mut h = Health::default();
        assert!(!h.card_due(10_000_000)); // never left it
        h.left_card(0);
        assert!(!h.card_due(299_000));
        assert!(h.card_due(300_000));
        assert!(!h.card_due(400_000));
    }

    fn det(label: &str, score: f32) -> Detection {
        Detection { label: label.into(), score, x: 0.1, y: 0.1, w: 0.2, h: 0.4 }
    }

    #[test]
    fn late_results_fill_in_the_review() {
        let mut r = serde_json::json!({
            "source": "camera", "confirmed": false, "labels": [],
            "alert": { "sent": true, "why": "ai_failed" },
            "checks": [
                { "n": 0, "frame": true, "error": "AI inference: CUDA failure 2", "seen": [] },
                { "n": 1, "frame": true, "error": "AI inference: CUDA failure 2", "seen": [] },
            ],
        });
        let wanted = vec!["person".to_string()];
        let got = apply_results(&mut r, &[(0, vec![det("person", 0.8), det("car", 0.9)]), (1, vec![det("person", 0.2)])], &wanted, 0.5, "built-in (Small model)");
        assert_eq!(got, vec!["person".to_string()]);
        assert!(r["checks"][0]["error"].is_null() && r["checks"][1]["error"].is_null());
        assert_eq!((r["confirmed"].clone(), r["alert"]["why"].clone()), (true.into(), "sent".into()));
        assert_eq!(r["source"], "built-in (Small model)");
        // Nothing wanted: not confirmed; a check with no result keeps its error.
        let mut r2 = serde_json::json!({ "labels": [], "alert": { "sent": false, "why": "ai_failed" }, "checks": [
            { "n": 0, "error": "CUDA" }, { "n": 1, "error": "CUDA" } ] });
        assert!(apply_results(&mut r2, &[(0, vec![])], &wanted, 0.5, "x").is_empty());
        assert_eq!(r2["alert"]["why"], "ai_no_wanted");
        assert_eq!(r2["checks"][1]["error"], "CUDA");
        // No results at all: untouched.
        let mut r3 = serde_json::json!({ "alert": { "why": "ai_failed" }, "checks": [{ "n": 0, "error": "CUDA" }] });
        apply_results(&mut r3, &[], &wanted, 0.5, "x");
        assert_eq!(r3["alert"]["why"], "ai_failed");
    }
}

/// The engine's handling of a card that is slow, stuck or overtaken, with
/// fake detectors (ai::fake): no model, no card. One test, as the state is
/// the process's own (one engine's AI at a time).
#[cfg(all(test, not(all(target_os = "macos", target_arch = "x86_64"))))]
mod card_state_tests {
    use super::*;
    use crate::ai::fake;
    use std::sync::atomic::{AtomicU32, AtomicUsize};
    use std::time::{Duration, Instant};

    /// Notes when the runner it is kept in is dropped.
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    const FAST: u32 = 0;
    const SLOW: u32 = 1;
    const HANG: u32 = 2;

    /// A fake card: answers at once (FAST), after 300 ms (SLOW), or not
    /// until `mode` changes (HANG).
    fn card(limit: Duration, mode: Arc<AtomicU32>, dropped: Arc<AtomicBool>) -> crate::ai::AiDetector {
        let keep = Dropped(dropped);
        fake::card(limit, move |_i| {
            let _k = &keep;
            loop {
                match mode.load(Ordering::SeqCst) {
                    FAST => return Ok(fake::nothing()),
                    SLOW => {
                        std::thread::sleep(Duration::from_millis(300));
                        return Ok(fake::nothing());
                    }
                    _ => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        })
    }

    fn engine() -> Arc<Engine> {
        let cfg: crate::config::Config = toml::from_str(&format!("[ai]\nenabled = true\ngpu = true\nmodel = \"{}\"\n", fake::MODEL)).unwrap();
        let mut e = Engine::for_test(cfg);
        Arc::get_mut(&mut e).unwrap().config_path = std::env::temp_dir().join(format!("ambersight-card-state-{}.toml", std::process::id())).to_string_lossy().into_owned();
        // (As Engine::new does; for_test leaves it out: other tests take the
        // engine back with Arc::get_mut.)
        let _ = e.me.set(Arc::downgrade(&e));
        e
    }

    fn reset() {
        crate::ai_card::reset_for_test();
        *HEALTH.lock().unwrap() = None;
        if let Some(s) = SLOW_CARD.lock().unwrap().take() {
            std::mem::forget(s);
        }
        SLOW_EPISODES.lock().unwrap().clear();
        *fake::CARD.lock().unwrap() = None;
        crate::ai::set_gpu_note(None);
    }

    fn jpeg() -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbImage::from_pixel(64, 48, image::Rgb([90, 90, 90])).write_to(&mut std::io::Cursor::new(&mut out), image::ImageOutputFormat::Jpeg(80)).unwrap();
        out
    }

    fn rest_over() {
        health(|h| h.cpu_since = Some(now_ms() - CPU_FOR_MS));
    }

    fn wait_for(f: impl Fn() -> bool) -> bool {
        let t0 = Instant::now();
        while !f() {
            if t0.elapsed() > Duration::from_secs(5) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn on_card(e: &Engine) -> bool {
        e.detector().is_some_and(|d| d.gpu_name().is_some())
    }

    fn slow_card_back() -> bool {
        SLOW_CARD.lock().unwrap().as_ref().is_some_and(|s| !s.card_stuck())
    }

    /// A fake card factory for moves onto the card, counting its calls.
    fn cards(calls: Arc<AtomicUsize>, mode: Arc<AtomicU32>, dropped: Arc<AtomicBool>) {
        *fake::CARD.lock().unwrap() = Some(Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(card(Duration::from_secs(2), mode.clone(), dropped.clone()))
        }));
    }

    #[test]
    fn slow_stuck_reloaded_stopped_and_overtaken() {
        let ms = Duration::from_millis;
        let pic = jpeg();
        let flag = || Arc::new(AtomicBool::new(false));

        // 1. Slow, then back: a rest on the processor, then the card again.
        reset();
        let e = engine();
        let (mode, dropped) = (Arc::new(AtomicU32::new(SLOW)), flag());
        *e.ai.write().unwrap() = Some(Arc::new(card(ms(100), mode.clone(), dropped.clone())));
        let before = fake::CPU_CHECKS.load(Ordering::SeqCst);
        assert!(e.ai_detect(&pic, 0.5).is_ok(), "the slow check is done on the processor");
        assert!(fake::CPU_CHECKS.load(Ordering::SeqCst) > before);
        assert!(!on_card(&e));
        assert_eq!(crate::ai::gpu_note(), Some(crate::ai_card::slow_text("OpenVINO")));
        assert!(wait_for(slow_card_back), "the slow check comes back");
        let (calls, mode2, dropped2) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicU32::new(FAST)), flag());
        cards(calls.clone(), mode2.clone(), dropped2.clone());
        rest_over();
        assert!(e.ai_detect(&pic, 0.5).is_ok());
        assert!(wait_for(|| on_card(&e)), "on the card again");
        assert!(SLOW_CARD.lock().unwrap().is_none() && crate::ai_card::blocked().is_none());
        assert!(wait_for(|| dropped.load(Ordering::SeqCst)), "the slow card's session is let go, not leaked");
        assert_eq!(crate::ai::gpu_note(), None);
        assert!(e.ai_detect(&pic, 0.5).is_ok());

        // 2. Slow, then still stuck: off until restart, left alone.
        reset();
        let e = engine();
        let (mode, dropped) = (Arc::new(AtomicU32::new(HANG)), flag());
        *e.ai.write().unwrap() = Some(Arc::new(card(ms(100), mode.clone(), dropped.clone())));
        assert!(e.ai_detect(&pic, 0.5).is_ok());
        let calls = Arc::new(AtomicUsize::new(0));
        cards(calls.clone(), Arc::new(AtomicU32::new(FAST)), flag());
        rest_over();
        assert!(e.ai_detect(&pic, 0.5).is_ok());
        let off = crate::ai_card::off_text("OpenVINO");
        assert_eq!(crate::ai_card::blocked(), Some(off.clone()));
        assert_eq!(crate::ai::gpu_note(), Some(off));
        std::thread::sleep(ms(200));
        assert!(!on_card(&e) && calls.load(Ordering::SeqCst) == 0, "not tried again");
        assert!(!dropped.load(Ordering::SeqCst), "never dropped");
        mode.store(FAST, Ordering::SeqCst); // (frees the test's thread)

        // 3. A reload (settings saved, an add-on arriving) during the rest:
        // no new card session until the slow one is judged.
        reset();
        let e = engine();
        let (mode, dropped) = (Arc::new(AtomicU32::new(SLOW)), flag());
        *e.ai.write().unwrap() = Some(Arc::new(card(ms(100), mode.clone(), dropped.clone())));
        assert!(e.ai_detect(&pic, 0.5).is_ok());
        let calls = Arc::new(AtomicUsize::new(0));
        cards(calls.clone(), Arc::new(AtomicU32::new(FAST)), flag());
        e.reload_ai().unwrap();
        std::thread::sleep(ms(300));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no move while resting");
        assert!(!on_card(&e) && SLOW_CARD.lock().unwrap().is_some());
        assert!(wait_for(slow_card_back));
        rest_over();
        assert!(e.ai_detect(&pic, 0.5).is_ok());
        assert!(wait_for(|| on_card(&e)), "judged, then moved");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(wait_for(|| dropped.load(Ordering::SeqCst)));

        // 4. Shutdown during the rest, with the slow check still stuck: no wait.
        reset();
        let e = engine();
        let (mode, dropped) = (Arc::new(AtomicU32::new(HANG)), flag());
        *e.ai.write().unwrap() = Some(Arc::new(card(ms(100), mode.clone(), dropped.clone())));
        assert!(e.ai_detect(&pic, 0.5).is_ok());
        let t0 = Instant::now();
        e.stop_ai(Duration::from_secs(2));
        assert!(t0.elapsed() < ms(1500), "{:?}", t0.elapsed());
        assert!(e.detector().is_none() && SLOW_CARD.lock().unwrap().is_none());
        assert!(!dropped.load(Ordering::SeqCst) && crate::ai_card::any_stuck());
        mode.store(FAST, Ordering::SeqCst);

        // 5. Shutdown during a move onto the card: the card is not put in,
        // and its session is let go.
        reset();
        let e = engine();
        e.reload_ai().unwrap();
        assert!(e.detector().is_some() && !on_card(&e));
        let (gate, entered, dropped) = (flag(), flag(), flag());
        {
            let (gate, entered, dropped) = (gate.clone(), entered.clone(), dropped.clone());
            *fake::CARD.lock().unwrap() = Some(Arc::new(move || {
                entered.store(true, Ordering::SeqCst);
                while !gate.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Some(card(Duration::from_secs(2), Arc::new(AtomicU32::new(FAST)), dropped.clone()))
            }));
        }
        e.move_to_card();
        assert!(wait_for(|| entered.load(Ordering::SeqCst)));
        e.stop_ai(Duration::from_secs(2));
        gate.store(true, Ordering::SeqCst);
        assert!(wait_for(|| dropped.load(Ordering::SeqCst)), "the late card's session is let go");
        assert!(e.detector().is_none(), "nothing put back after shutdown");

        // 6. A move overtaken by a reload: discarded, and its note not written.
        reset();
        let e = engine();
        e.reload_ai().unwrap();
        let (gate, entered, dropped) = (flag(), flag(), flag());
        {
            let (gate, entered, dropped) = (gate.clone(), entered.clone(), dropped.clone());
            *fake::CARD.lock().unwrap() = Some(Arc::new(move || {
                entered.store(true, Ordering::SeqCst);
                while !gate.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Some(card(Duration::from_secs(2), Arc::new(AtomicU32::new(FAST)), dropped.clone()))
            }));
        }
        e.move_to_card();
        assert!(wait_for(|| entered.load(Ordering::SeqCst)));
        let newer = Arc::new(fake::cpu());
        *e.ai.write().unwrap() = Some(newer.clone());
        crate::ai::set_gpu_note(Some("newer".into()));
        gate.store(true, Ordering::SeqCst);
        assert!(wait_for(|| dropped.load(Ordering::SeqCst)), "the stale move's card is let go");
        assert!(e.detector().is_some_and(|d| Arc::ptr_eq(&d, &newer)), "the newer detector stays");
        assert_eq!(crate::ai::gpu_note(), Some("newer".into()));

        // 8. Choosing a model (POST /api/ai/model) takes the same locked path
        // as any reload: in use at once, on the card in the background, and
        // nothing put in once AmberSight is stopping.
        reset();
        let e = engine();
        let (calls, dropped) = (Arc::new(AtomicUsize::new(0)), flag());
        cards(calls.clone(), Arc::new(AtomicU32::new(FAST)), dropped.clone());
        e.use_model(std::path::Path::new(fake::MODEL)).unwrap();
        assert!(e.detector().is_some());
        assert!(wait_for(|| on_card(&e)), "moved onto the card");
        e.stop_ai(Duration::from_secs(2));
        assert!(wait_for(|| dropped.load(Ordering::SeqCst)));
        let calls_before = calls.load(Ordering::SeqCst);
        e.use_model(std::path::Path::new(fake::MODEL)).unwrap();
        std::thread::sleep(ms(200));
        assert!(e.detector().is_none(), "never installs after stopping");
        assert_eq!(calls.load(Ordering::SeqCst), calls_before, "no card session either");
        let _ = std::fs::remove_file(&e.config_path);

        // 7. Too slow three times within an hour: off until restart.
        reset();
        let e = engine();
        let t = now_ms();
        SLOW_EPISODES.lock().unwrap().extend([t - 1_000_000, t - 500_000]);
        let (mode, dropped) = (Arc::new(AtomicU32::new(SLOW)), flag());
        *e.ai.write().unwrap() = Some(Arc::new(card(ms(100), mode.clone(), dropped.clone())));
        assert!(e.ai_detect(&pic, 0.5).is_ok());
        assert_eq!(crate::ai_card::blocked(), Some(crate::ai_card::keeps_slow_text("OpenVINO")));
        assert!(wait_for(slow_card_back));
        rest_over();
        assert!(e.ai_detect(&pic, 0.5).is_ok());
        std::thread::sleep(ms(200));
        assert!(!on_card(&e), "not tried again");
        assert!(wait_for(|| dropped.load(Ordering::SeqCst)), "the slow card came back, so it is let go");
        reset();
    }
}
