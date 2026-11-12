//! Keeps the most recent log lines in memory for the web log viewer
//! (`GET /api/logs`), alongside normal console/file logging.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

const MAX_LINES: usize = 2000;

#[derive(Clone, Debug, serde::Serialize)]
pub struct LogLine {
    pub ts: i64,
    pub level: &'static str,
    pub target: String,
    pub message: String,
}

fn buf() -> &'static Mutex<VecDeque<LogLine>> {
    static B: OnceLock<Mutex<VecDeque<LogLine>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LINES)))
}

/// Recent lines at or above `min` (newest last), at most `limit`.
pub fn recent(min: Level, limit: usize) -> Vec<LogLine> {
    let rank = |l: &str| match l {
        "ERROR" => 0,
        "WARN" => 1,
        "INFO" => 2,
        "DEBUG" => 3,
        _ => 4,
    };
    let want = rank(min.as_str());
    let b = buf().lock().unwrap();
    let all: Vec<LogLine> = b.iter().filter(|l| rank(l.level) <= want).cloned().collect();
    all[all.len().saturating_sub(limit)..].to_vec()
}

struct MsgVisitor(String);

impl Visit for MsgVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        } else {
            self.0.push_str(&format!(" {}={value}", field.name()));
        }
    }
}

/// tracing layer that records INFO and above (independent of RUST_LOG).
pub struct RingLayer;

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > Level::INFO || !meta.target().starts_with("ambersight") {
            return;
        }
        let mut v = MsgVisitor(String::new());
        event.record(&mut v);
        let line = LogLine {
            ts: chrono::Utc::now().timestamp(),
            level: meta.level().as_str(),
            target: meta.target().trim_start_matches("ambersight::").to_string(),
            message: v.0,
        };
        let mut b = buf().lock().unwrap();
        if b.len() == MAX_LINES {
            b.pop_front();
        }
        b.push_back(line);
    }
}
