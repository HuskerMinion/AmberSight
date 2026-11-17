//! When the graphics card fails the AI: checks that failed for that reason
//! are done again later, and a card that keeps failing is left for the
//! processor for a while.
//!
//! - A graphics card error (CUDA, cuDNN, out of video memory, DirectML) is
//!   told from an ordinary one (a bad picture, an AI server that is down).
//! - Three of them in a row within a minute: the built-in AI moves to the
//!   processor for five minutes, the check that failed is done again there,
//!   and the card is tried again after that.
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
    ["cuda", "cudnn", "cublas", "out of memory", "failed to allocate", "bfcarena", "directml", "dxgi", "d3d12", "device removed", "device hung", "coreml"]
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
        } else if m.contains("coreml") {
            "CoreML"
        } else if m.contains("device removed") {
            "device removed"
        } else if m.contains("device hung") {
            "device hung"
        } else {
            ""
        };
        let what = if memory { "the graphics card ran out of memory" } else { "the graphics card gave an error" };
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

fn health<R>(f: impl FnOnce(&mut Health) -> R) -> R {
    f(HEALTH.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Health::default))
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl Engine {
    /// `ai_detect_inner` with the card's errors handled (see the top of this file).
    pub(super) fn ai_detect_guarded(&self, jpeg: &[u8], min_score: f32) -> anyhow::Result<Vec<crate::ai::Detection>> {
        let used = self.detector().filter(|d| d.gpu_name().is_some());
        let mut r = self.ai_detect_inner(jpeg, min_score);
        match (&r, &used) {
            (Err(e), Some(d)) if is_gpu_error(&format!("{e:#}")) => {
                let msg = format!("{e:#}");
                if health(|h| h.error(now_ms())) {
                    self.leave_card(d, &msg);
                }
                // Moved to the processor (by this call or another): do this check there.
                if self.detector().is_some_and(|n| !Arc::ptr_eq(&n, d)) {
                    r = self.ai_detect_inner(jpeg, min_score);
                }
            }
            (Ok(_), Some(_)) => health(|h| h.ok()),
            (Ok(_), None) => {
                if health(|h| h.card_due(now_ms())) {
                    self.try_card_again();
                }
            }
            _ => {}
        }
        r
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
    fn try_card_again(&self) {
        info!("AI: trying the graphics card again");
        if let Err(e) = self.reload_ai() {
            warn!("AI: {e:#}");
        }
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
        // Worded once, the same again, and still a card error for late looks.
        for raw in [REAL, "AI inference: CUDA failure 2: out of memory", "DirectML: DXGI_ERROR_DEVICE_HUNG", "Failed to allocate memory", "device removed", "coreml failed"] {
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
