//! The real backend: ffmpeg captures and encodes, the helper uploads.

use super::ffmpeg;
use super::net::{self, Target};
use super::proto::{self, Source, StartSpec, State};
use super::session::{Backend, StatusEvent};
use futures::future::BoxFuture;
use std::sync::atomic::{AtomicU64, Ordering};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

const BACKOFF_START: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const WINDOW_RETRY: Duration = Duration::from_secs(10);
/// No new picture for this long: the grabber has gone quiet (see run).
const STALL: Duration = Duration::from_secs(8);

pub struct Real {
    pub target: Target,
    pub tls: Arc<rustls::ClientConfig>,
    /// Replaces the screen grabber (tests use a lavfi pattern).
    pub input_override: Option<Vec<String>>,
}

pub const NO_DESKTOP: &str = "AmberSight Capture is running without a desktop (as a Windows background task). Start it from the signed-in desktop instead: sign out and back in, or run \"ambersight.exe capture\" there.";

fn display() -> String {
    std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| ":0".into())
}

/// Grabber arguments for `source` (best first), or why it cannot be captured now.
fn variants(input_override: &Option<Vec<String>>, source: &Source, fps: u32) -> Result<Vec<Vec<String>>, String> {
    if let Some(i) = input_override {
        return Ok(vec![i.clone()]);
    }
    // Started as a background task (Windows session 0) there is no desktop to see.
    if crate::local_source::in_session_zero() {
        return Err(NO_DESKTOP.into());
    }
    let r = ffmpeg::resolve(source).map_err(|_| "window not open".to_string())?;
    let v = ffmpeg::input_variants(&r, fps, crate::local_source::host_os(), &display()).map_err(|e| e.to_string())?;
    if v.is_empty() {
        return Err("nothing to capture".into());
    }
    Ok(v)
}

async fn variants_async(input_override: Option<Vec<String>>, source: Source, fps: u32) -> Result<Vec<Vec<String>>, String> {
    tokio::task::spawn_blocking(move || variants(&input_override, &source, fps)).await.map_err(|e| e.to_string())?
}

impl Backend for Real {
    fn sources(&self) -> BoxFuture<'static, proto::Sources> {
        Box::pin(async { tokio::task::spawn_blocking(super::sources::enumerate).await.unwrap_or_default() })
    }

    fn thumb(&self, source: Source, width: u32) -> BoxFuture<'static, Result<Vec<u8>, String>> {
        let input_override = self.input_override.clone();
        Box::pin(async move {
            let v = variants_async(input_override, source, 5).await?;
            ffmpeg::grab_jpeg(&v, width).await.map_err(|e| e.to_string())
        })
    }

    fn start(&self, spec: StartSpec, gen: u64, events: UnboundedSender<StatusEvent>) -> JoinHandle<()> {
        let job = Job { target: self.target.clone(), tls: self.tls.clone(), input_override: self.input_override.clone(), spec, gen, events };
        tokio::spawn(job.run())
    }
}

struct Job {
    target: Target,
    tls: Arc<rustls::ClientConfig>,
    input_override: Option<Vec<String>>,
    spec: StartSpec,
    gen: u64,
    events: UnboundedSender<StatusEvent>,
}

impl Job {
    fn report(&self, state: State, error: Option<String>) {
        let _ = self.events.send(StatusEvent { camera: self.spec.camera.clone(), gen: self.gen, state, error });
    }

    async fn run(self) {
        let mut backoff = BACKOFF_START;
        let mut variant = 0usize;
        let name = self.spec.camera.clone();
        loop {
            self.report(State::Starting, None);
            let variants = match variants_async(self.input_override.clone(), self.spec.source.clone(), self.spec.fps).await {
                Ok(v) => v,
                Err(e) => {
                    let wait = if e == "window not open" { WINDOW_RETRY } else { backoff };
                    self.report(State::Error, Some(e));
                    tokio::time::sleep(wait).await;
                    continue;
                }
            };
            let input = &variants[variant % variants.len()];
            let began = Instant::now();
            // With a fallback to go to, a grabber that goes quiet is given up
            // on (Windows Graphics Capture only sends a picture when the
            // window changes, so a still window sends nothing at all).
            let stall = (variants.len() > 1).then_some(STALL);
            let (sent, stalled, err) = self.attempt(input, stall).await;
            if began.elapsed() > Duration::from_secs(60) {
                backoff = BACKOFF_START;
            }
            if (sent == 0 || stalled) && variants.len() > 1 {
                // The grabber produced nothing: try the fallback next time.
                variant += 1;
            }
            if stalled {
                // Not the camera's fault: switch straight away.
                backoff = BACKOFF_START;
                tracing::info!("capture '{name}': {err}; trying the other capture method");
                self.report(State::Starting, None);
                continue;
            }
            tracing::warn!("capture '{name}': {err}; retrying in {}s", backoff.as_secs());
            self.report(State::Error, Some(err));
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    /// One ffmpeg + upload run. Returns bytes sent and why it ended.
    async fn attempt(&self, input: &[String], stall: Option<Duration>) -> (u64, bool, String) {
        let encoder = ffmpeg::encoder().await;
        let args = ffmpeg::capture_args(input, &self.spec, encoder);
        tracing::debug!("capture '{}': ffmpeg {}", self.spec.camera, args.join(" "));
        let mut cmd = ffmpeg::command(&args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return (0, false, format!("could not start ffmpeg ({}): {e}", ffmpeg::ffmpeg_path().display())),
        };
        ffmpeg::adopt(&child);
        let stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let errlog = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 64 * 1024 {
                    buf.drain(..buf.len() - 16 * 1024);
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        });
        // Bytes sent, and when the last ones went (ms after `t0`).
        let sent = AtomicU64::new(0);
        let t0 = Instant::now();
        let last = AtomicU64::new(0);
        let mut stalled = false;
        let result = match net::connect(&self.target, self.tls.clone()).await {
            Err(e) => Err(e),
            Ok(io) => {
                let mut running = false;
                let up = super::upload::push(stdout, io, &self.target.authority, &self.spec.key, |n| {
                    sent.store(n, Ordering::Relaxed);
                    last.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
                    if !running {
                        running = true;
                        self.report(State::Running, None);
                        tracing::info!("capture '{}': streaming", self.spec.camera);
                    }
                });
                let watch = async {
                    let Some(limit) = stall else { return std::future::pending::<()>().await };
                    loop {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let quiet = t0.elapsed().saturating_sub(Duration::from_millis(last.load(Ordering::Relaxed)));
                        if quiet > limit {
                            return;
                        }
                    }
                };
                tokio::select! {
                    r = up => r,
                    _ = watch => {
                        stalled = true;
                        Err(anyhow::anyhow!("no picture for {}s", stall.unwrap_or_default().as_secs()))
                    }
                }
            }
        };
        let sent = sent.load(Ordering::Relaxed);
        let _ = child.start_kill();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        let log = tokio::time::timeout(Duration::from_secs(2), errlog).await.ok().and_then(|r| r.ok()).unwrap_or_default();
        let why = match result {
            Err(e) => format!("{e:#}"),
            Ok(_) => match ffmpeg::last_line(&log) {
                Some(l) => format!("ffmpeg stopped: {l}"),
                None => match status {
                    Ok(Ok(s)) => format!("ffmpeg stopped ({s})"),
                    _ => "ffmpeg stopped".into(),
                },
            },
        };
        (sent, stalled, why)
    }
}
