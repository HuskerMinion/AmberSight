//! Live server statistics for the status panel (`GET /api/stats`).
//!
//! A background thread samples the machine every [`INTERVAL_S`] seconds
//! (CPU, memory, the AmberSight process, per-camera received frame rate and,
//! more rarely, storage usage) and keeps the last [`HISTORY_LEN`] samples, so
//! a request only reads the cache and the page can draw full sparklines at once.

use crate::engine::Engine;
use crate::telemetry::{self, AiSnap, GpuMeter, GpuStat, NetMeter, WindowDelta};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

pub const INTERVAL_S: u64 = 2;
pub const HISTORY_LEN: usize = 150;
/// Frame-rate window: long enough to smooth a 1 fps camera, short enough to
/// notice a stalled one quickly.
const FPS_WINDOW_S: f64 = 8.0;
/// Storage figures change slowly and cost a database query: refresh every Nth tick.
const STORAGE_EVERY: u64 = 5;
/// The long history (Performance page) keeps one averaged point per this many ticks.
const LONG_EVERY: u64 = 30;
pub const LONG_LEN: usize = 60;
/// Frames dropped per minute are counted over this window.
const DROP_WINDOW_S: f64 = 60.0;

/// Received frames per second over a short sliding window of a monotonically
/// increasing counter.
#[derive(Default)]
pub struct FpsTracker {
    pts: VecDeque<(f64, u64)>,
}

impl FpsTracker {
    /// Record the counter value at time `t` (seconds) and return the rate.
    pub fn update(&mut self, t: f64, count: u64) -> f64 {
        // A counter that went backwards means the camera was restarted.
        if self.pts.back().is_some_and(|&(_, c)| count < c) {
            self.pts.clear();
        }
        self.pts.push_back((t, count));
        while self.pts.len() > 2 && t - self.pts[1].0 >= FPS_WINDOW_S {
            self.pts.pop_front();
        }
        let (t0, c0) = self.pts[0];
        if t - t0 < 0.5 {
            return 0.0;
        }
        (count - c0) as f64 / (t - t0)
    }
}

/// Fixed-length ring of samples, each tagged with an ever-increasing sequence
/// number so clients can ask only for what is new.
pub struct Ring<T> {
    items: VecDeque<(u64, T)>,
    cap: usize,
    next: u64,
}

impl<T: Clone> Ring<T> {
    pub fn new(cap: usize) -> Self {
        Ring { items: VecDeque::with_capacity(cap), cap, next: 1 }
    }
    pub fn push(&mut self, v: T) {
        if self.items.len() == self.cap {
            self.items.pop_front();
        }
        self.items.push_back((self.next, v));
        self.next += 1;
    }
    /// Sequence number of the newest sample (0 when empty).
    pub fn last_seq(&self) -> u64 {
        self.items.back().map_or(0, |i| i.0)
    }
    /// Samples newer than `since`, oldest first.
    pub fn since(&self, since: u64) -> Vec<T> {
        self.items.iter().filter(|i| i.0 > since).map(|i| i.1.clone()).collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub cpu: f32,
    pub proc_cpu: f32,
    pub mem: f32,
    pub fps: f32,
    pub disk: f32,
    /// AI inference median (ms; 0 when no checks) and checks in the last minute.
    pub ai_p50: f32,
    pub ai_checks: f32,
    /// Busiest graphics card, percent (0 when unknown).
    pub gpu: f32,
    /// Network bytes per second.
    pub rx: f32,
    pub tx: f32,
    /// Bytes per second written, per recording folder (name, rate).
    pub writes: Vec<(String, f32)>,
}

impl Sample {
    /// Average of samples (folders matched by name).
    pub fn mean(v: &[Sample]) -> Sample {
        let n = v.len().max(1) as f32;
        let avg = |f: fn(&Sample) -> f32| v.iter().map(f).sum::<f32>() / n;
        let mut w: Vec<(String, f32)> = Vec::new();
        for s in v {
            for (name, b) in &s.writes {
                match w.iter_mut().find(|(k, _)| k == name) {
                    Some(e) => e.1 += b / n,
                    None => w.push((name.clone(), b / n)),
                }
            }
        }
        Sample {
            cpu: avg(|s| s.cpu),
            proc_cpu: avg(|s| s.proc_cpu),
            mem: avg(|s| s.mem),
            fps: avg(|s| s.fps),
            disk: avg(|s| s.disk),
            ai_p50: avg(|s| s.ai_p50),
            ai_checks: avg(|s| s.ai_checks),
            gpu: avg(|s| s.gpu),
            rx: avg(|s| s.rx),
            tx: avg(|s| s.tx),
            writes: w,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CamStat {
    pub id: usize,
    pub name: String,
    pub slug: String,
    pub online: bool,
    pub sleeping: bool,
    pub enabled: bool,
    pub fps: f64,
    /// Fps the camera is set to, measured video kbit/s, frames dropped in the
    /// last minute, and stream drops since start.
    pub cfg_fps: u32,
    pub kbps: f64,
    pub dropped_1m: u64,
    pub reconnects: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DiskStat {
    pub name: String,
    pub path: String,
    pub used: u64,
    pub cap: u64,
    pub drive_total: Option<u64>,
    pub drive_free: Option<u64>,
    /// Capped, and its oldest recordings move to an archive folder at the
    /// cap: being full is how it is meant to run, not a warning.
    pub rolls: bool,
    /// "new" (where recordings land) or "stored" (the archive).
    pub tier: &'static str,
    /// Bytes per second written over the last sample interval.
    pub write_bps: f64,
    /// Automatic cap (max_mb = 0): `cap` follows the drive's free space.
    pub auto: bool,
    /// Free space the drive is meant to keep.
    pub target_free: u64,
    /// Status: "" / "info" / "warn" / "crit" and its short label
    /// (engine::storage_level).
    pub level: &'static str,
    pub label: String,
}

/// The disk figure shown first (matches sysmon.js worstDisk): all
/// recording space together when every folder is capped (a folder handing
/// its oldest to the archive sits at its cap by design), else the fullest.
pub fn headline_pct(disks: &[DiskStat]) -> f64 {
    if !disks.is_empty() && disks.iter().all(|d| d.cap > 0) {
        let used: u64 = disks.iter().map(|d| d.used).sum();
        let cap: u64 = disks.iter().map(|d| d.cap).sum();
        let p = 100.0 * used as f64 / cap as f64;
        return if disks.iter().any(|d| d.auto) { p.min(100.0) } else { p };
    }
    disks.iter().map(|d| d.pct()).fold(0.0, f64::max)
}

impl DiskStat {
    /// Fill 0..100: used against the cap, or drive fullness when uncapped.
    pub fn pct(&self) -> f64 {
        if self.cap > 0 {
            let p = 100.0 * self.used as f64 / self.cap as f64;
            // An automatic cap moves with the free space; between two
            // pruning passes it can be a little under what is stored.
            return if self.auto { p.min(100.0) } else { p };
        }
        match (self.drive_total, self.drive_free) {
            (Some(t), Some(f)) if t > 0 => 100.0 * (t - f.min(t)) as f64 / t as f64,
            _ => 0.0,
        }
    }
}

struct Inner {
    cpu: f32,
    proc_cpu: f32,
    cpus: usize,
    mem_total: u64,
    mem_used: u64,
    proc_rss: u64,
    cams: Vec<CamStat>,
    disks: Vec<DiskStat>,
    history: Ring<Sample>,
    long: VecDeque<Sample>,
    ai: AiSnap,
    /// Network (rx, tx) bytes per second; None where not readable.
    net: Option<(f64, f64)>,
    gpu: Option<Vec<GpuStat>>,
}

pub struct Stats {
    inner: Mutex<Inner>,
}

fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

impl Stats {
    pub fn new() -> Self {
        Stats {
            inner: Mutex::new(Inner {
                cpu: 0.0,
                proc_cpu: 0.0,
                cpus: 0,
                mem_total: 0,
                mem_used: 0,
                proc_rss: 0,
                cams: Vec::new(),
                disks: Vec::new(),
                history: Ring::new(HISTORY_LEN),
                long: VecDeque::new(),
                ai: AiSnap::default(),
                net: None,
                gpu: None,
            }),
        }
    }

    /// The `/api/stats` document. `allow` filters cameras for limited accounts;
    /// `since` is the last history sequence the client already holds.
    pub fn json(&self, uptime_s: i64, since: u64, allow: &dyn Fn(&CamStat) -> bool) -> serde_json::Value {
        let g = self.inner.lock().unwrap();
        let cams: Vec<&CamStat> = g.cams.iter().filter(|c| allow(c)).collect();
        let active: Vec<_> = cams.iter().filter(|c| c.enabled).collect();
        let fps_total: f64 = active.iter().map(|c| c.fps).sum();
        let streaming = active.iter().filter(|c| c.online && !c.sleeping).count();
        let sleeping = active.iter().filter(|c| c.sleeping).count();
        let samples = g.history.since(since);
        let col = |f: fn(&Sample) -> f32| samples.iter().map(|s| r1(f(s) as f64)).collect::<Vec<_>>();
        let long: Vec<Sample> = g.long.iter().cloned().collect();
        let lcol = |f: fn(&Sample) -> f32| long.iter().map(|s| r1(f(s) as f64)).collect::<Vec<_>>();
        let mut folder_names: Vec<&str> = Vec::new();
        for s in &long {
            for (n, _) in &s.writes {
                if !folder_names.contains(&n.as_str()) {
                    folder_names.push(n);
                }
            }
        }
        let writes: serde_json::Map<String, serde_json::Value> = folder_names
            .iter()
            .map(|n| {
                let v: Vec<f64> = long.iter().map(|s| s.writes.iter().find(|(k, _)| k == n).map_or(0.0, |w| w.1.round() as f64)).collect();
                (n.to_string(), serde_json::json!(v))
            })
            .collect();
        let gpu = g.gpu.as_ref().map(|v| {
            serde_json::json!({
                "util": r1(v.iter().map(|a| a.util).fold(0.0, f64::max)),
                "adapters": v.iter().map(|a| serde_json::json!({
                    "name": a.name, "util": r1(a.util), "vram_used": a.vram_used, "vram_total": a.vram_total,
                })).collect::<Vec<_>>(),
            })
        });
        // The fps history sums every camera; a caller who may see only some
        // must not learn the others' activity from it (zeros keep the series
        // aligned with the other columns).
        let restricted = cams.len() < g.cams.len();
        let hist_fps: Vec<f64> = if restricted { vec![0.0; samples.len()] } else { col(|s| s.fps) };
        let long_fps: Vec<f64> = if restricted { vec![0.0; long.len()] } else { lcol(|s| s.fps) };
        let ai = &g.ai;
        let rd = |v: Option<f64>| v.map(|x| r1(x));
        serde_json::json!({
            "interval_s": INTERVAL_S,
            "uptime_s": uptime_s,
            "cpu": { "machine": r1(g.cpu as f64), "process": r1(g.proc_cpu as f64), "cores": g.cpus },
            "mem": { "total": g.mem_total, "used": g.mem_used, "process": g.proc_rss },
            "fps": { "total": r1(fps_total), "streaming": streaming, "cameras": active.len(), "sleeping": sleeping },
            "storage": g.disks.iter().map(|d| serde_json::json!({
                "name": d.name, "path": d.path, "used": d.used, "cap": d.cap,
                "drive_total": d.drive_total, "drive_free": d.drive_free,
                "pct": r1(d.pct()), "basis": if d.cap > 0 { "cap" } else { "drive" }, "rolls": d.rolls,
                "free": d.drive_free, "total": d.drive_total, "tier": d.tier, "write_bps": d.write_bps.round(),
                "auto": d.auto, "target_free": d.target_free, "level": d.level, "label": d.label,
            })).collect::<Vec<_>>(),
            "cameras": cams.iter().map(|c| serde_json::json!({
                "id": c.id, "name": c.name, "enabled": c.enabled, "online": c.online,
                "sleeping": c.sleeping, "fps": r1(c.fps),
                "fps_in": r1(c.fps), "cfg_fps": c.cfg_fps, "kbps": c.kbps.round(),
                "dropped_1m": c.dropped_1m, "reconnects": c.reconnects,
            })).collect::<Vec<_>>(),
            "ai": {
                "state": ai.state, "device": ai.device, "checks_1m": ai.checks_1m,
                "p50_ms": rd(ai.p50_ms), "p95_ms": rd(ai.p95_ms), "last_ok_s": ai.last_ok_s.map(|x| x.round()),
                "errors_1m": ai.errors_1m,
            },
            "net": g.net.map(|(rx, tx)| serde_json::json!({ "rx_bps": rx.round(), "tx_bps": tx.round() })),
            "gpu": gpu,
            "history": {
                "seq": g.history.last_seq(),
                "cpu": col(|s| s.cpu), "process_cpu": col(|s| s.proc_cpu),
                "mem": col(|s| s.mem), "fps": hist_fps, "disk": col(|s| s.disk),
                "ai_p50": col(|s| s.ai_p50), "ai_checks": col(|s| s.ai_checks), "gpu": col(|s| s.gpu),
                "rx": col(|s| s.rx), "tx": col(|s| s.tx),
            },
            // One averaged point a minute, up to an hour (the Performance page).
            "history_1h": {
                "step_s": INTERVAL_S * LONG_EVERY,
                "cpu": lcol(|s| s.cpu), "mem": lcol(|s| s.mem), "fps": long_fps,
                "ai_p50": lcol(|s| s.ai_p50), "ai_checks": lcol(|s| s.ai_checks), "gpu": lcol(|s| s.gpu),
                "rx": lcol(|s| s.rx), "tx": lcol(|s| s.tx), "writes": writes,
            },
        })
    }
}

fn storage_now(engine: &Arc<Engine>) -> Vec<DiskStat> {
    let caps = engine.storage_caps_recent();
    let levels = engine.storage_levels(&caps);
    caps.into_iter()
        .zip(levels)
        .map(|(c, (l, rolls))| DiskStat {
            name: if c.name.is_empty() { "Data".into() } else { c.name.clone() },
            path: c.path.to_string_lossy().into_owned(),
            used: c.used,
            cap: c.cap,
            drive_total: c.total,
            drive_free: c.free,
            rolls,
            tier: if c.archive { "stored" } else { "new" },
            write_bps: 0.0,
            auto: c.auto,
            target_free: c.target_free,
            level: l.level,
            label: l.label,
        })
        .collect()
}

/// Start the sampler thread.
pub fn start(engine: Arc<Engine>) {
    std::thread::Builder::new().name("stats".into()).spawn(move || sampler(engine)).ok();
}

fn sampler(engine: Arc<Engine>) {
    let Ok(mut sys) = System::new() else { return };
    let pid = sysinfo::get_current_pid().ok();
    let begun = Instant::now();
    let mut trackers: HashMap<usize, FpsTracker> = HashMap::new();
    let mut kb_trackers: HashMap<usize, FpsTracker> = HashMap::new();
    let mut drop_trackers: HashMap<usize, WindowDelta> = HashMap::new();
    let mut net = NetMeter::new();
    let mut gpu_meter = GpuMeter::new();
    let mut last_net: Option<(f64, u64, u64)> = None;
    let mut last_written: Option<(f64, Vec<u64>)> = None;
    let mut acc: Vec<Sample> = Vec::new();
    let mut tick: u64 = 0;
    sys.refresh_cpu_usage();
    loop {
        // The first pass runs right away so the panel has storage at once.
        if tick > 0 {
            std::thread::sleep(Duration::from_secs(INTERVAL_S));
        } else {
            std::thread::sleep(Duration::from_millis(500));
        }
        let now = begun.elapsed().as_secs_f64();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let cores = sys.cpus().len().max(1);
        let (mut proc_cpu, mut rss) = (0.0f32, 0u64);
        if let Some(pid) = pid {
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
            if let Some(p) = sys.process(pid) {
                // sysinfo reports 100% per busy core; show a share of the whole machine.
                proc_cpu = (p.cpu_usage() / cores as f32).min(100.0);
                rss = p.memory();
            }
        }

        let mut cams = Vec::new();
        {
            let list = engine.cameras.read().unwrap();
            trackers.retain(|id, _| list.get(*id).is_some_and(|h| h.is_some()));
            kb_trackers.retain(|id, _| trackers.contains_key(id));
            drop_trackers.retain(|id, _| trackers.contains_key(id));
            for h in list.iter().flatten() {
                let (name, online, sleeping) = {
                    let st = h.state.read().unwrap();
                    (st.name.clone(), st.online, st.sleeping)
                };
                let fps = trackers.entry(h.id).or_default().update(now, h.rx_frames.load(Ordering::Relaxed));
                let online_now = online && !sleeping;
                let kbps = kb_trackers.entry(h.id).or_default().update(now, h.rx_bytes.load(Ordering::Relaxed)) * 8.0 / 1000.0;
                let dropped_1m = drop_trackers.entry(h.id).or_default().update(now, h.dropped.load(Ordering::Relaxed), DROP_WINDOW_S);
                let cfg_fps = h.running.read().unwrap().fps;
                let reconnects = h.state.read().unwrap().reconnects;
                cams.push(CamStat {
                    cfg_fps,
                    kbps: if online_now { kbps } else { 0.0 },
                    dropped_1m,
                    reconnects,
                    id: h.id,
                    slug: crate::config::slug(&name),
                    name,
                    online,
                    sleeping,
                    enabled: h.active.load(Ordering::Relaxed),
                    fps: if online && !sleeping { fps } else { 0.0 },
                });
            }
        }
        let fps_total: f64 = cams.iter().filter(|c| c.enabled).map(|c| c.fps).sum();
        let disks = (tick % STORAGE_EVERY == 0).then(|| storage_now(&engine));
        tick += 1;

        // Network rate between ticks.
        let net_rate = net.totals().and_then(|(rx, tx)| {
            let r = last_net.map(|(t, prx, ptx)| (telemetry::rate(prx, rx, now - t), telemetry::rate(ptx, tx, now - t)));
            last_net = Some((now, rx, tx));
            r
        });
        let gpu = gpu_meter.sample();
        let (ai_on, ai_device) = {
            let det = engine.detector();
            let url = engine.config.lock().unwrap().ai.url.clone().unwrap_or_default();
            let on = det.is_some() || !url.is_empty();
            let dev = match &det {
                Some(d) => d.gpu_name().map_or_else(|| "CPU".to_string(), String::from),
                None if on => "AI server".to_string(),
                None => String::new(),
            };
            (on, dev)
        };
        let ai = telemetry::ai_snapshot(ai_on, ai_device);

        let mem_total = sys.total_memory();
        let mem_used = sys.used_memory();
        let mut g = engine.stats.inner.lock().unwrap();
        g.cpu = sys.global_cpu_usage();
        g.proc_cpu = proc_cpu;
        g.cpus = cores;
        g.mem_total = mem_total;
        g.mem_used = mem_used;
        g.proc_rss = rss;
        g.cams = cams;
        if let Some(d) = disks {
            // A refreshed list keeps no rate of its own: carry the last ones over.
            let old: HashMap<String, f64> = g.disks.iter().map(|d| (d.name.clone(), d.write_bps)).collect();
            g.disks = d;
            for d in g.disks.iter_mut() {
                d.write_bps = old.get(&d.name).copied().unwrap_or(0.0);
            }
        }
        // Bytes written per folder since the last tick, from the recorder's counters.
        let roots: Vec<std::path::PathBuf> = g.disks.iter().map(|d| std::path::PathBuf::from(&d.path)).collect();
        let written = telemetry::written_by_root(&roots);
        if let Some((t0, prev)) = last_written.as_ref().filter(|(_, p)| p.len() == written.len()) {
            for (i, d) in g.disks.iter_mut().enumerate() {
                d.write_bps = telemetry::rate(prev[i], written[i], now - t0);
            }
        }
        last_written = Some((now, written));
        g.ai = ai;
        g.net = net_rate;
        g.gpu = gpu;
        let disk = headline_pct(&g.disks) as f32;
        let sample = Sample {
            cpu: g.cpu,
            proc_cpu,
            mem: if mem_total > 0 { 100.0 * mem_used as f32 / mem_total as f32 } else { 0.0 },
            fps: fps_total as f32,
            disk,
            ai_p50: g.ai.p50_ms.unwrap_or(0.0) as f32,
            ai_checks: g.ai.checks_1m as f32,
            gpu: g.gpu.as_ref().map_or(0.0, |v| v.iter().map(|a| a.util).fold(0.0, f64::max) as f32),
            rx: g.net.map_or(0.0, |n| n.0 as f32),
            tx: g.net.map_or(0.0, |n| n.1 as f32),
            writes: g.disks.iter().map(|d| (d.name.clone(), d.write_bps as f32)).collect(),
        };
        acc.push(sample.clone());
        if acc.len() as u64 >= LONG_EVERY {
            let m = Sample::mean(&acc);
            acc.clear();
            if g.long.len() >= LONG_LEN {
                g.long.pop_front();
            }
            g.long.push_back(m);
        }
        g.history.push(sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_from_counter_deltas() {
        let mut t = FpsTracker::default();
        assert_eq!(t.update(0.0, 0), 0.0);
        assert!((t.update(2.0, 20) - 10.0).abs() < 1e-9);
        assert!((t.update(4.0, 40) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn fps_window_slides_and_drops_to_zero_when_stalled() {
        let mut t = FpsTracker::default();
        let mut c = 0;
        for i in 0..10 {
            c += 20;
            t.update(i as f64 * 2.0, c);
        }
        // Counter frozen: after a window's worth of ticks the rate is 0.
        let mut last = 1.0;
        for i in 10..16 {
            last = t.update(i as f64 * 2.0, c);
        }
        assert_eq!(last, 0.0);
    }

    #[test]
    fn fps_counter_reset_restarts_window() {
        let mut t = FpsTracker::default();
        t.update(0.0, 1000);
        t.update(2.0, 1020);
        assert_eq!(t.update(4.0, 5), 0.0);
        assert!((t.update(6.0, 25) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn ring_keeps_last_n_and_returns_only_new() {
        let mut r = Ring::new(3);
        assert_eq!(r.last_seq(), 0);
        for v in 1..=5 {
            r.push(v);
        }
        assert_eq!(r.since(0), vec![3, 4, 5]);
        assert_eq!(r.since(4), vec![5]);
        assert_eq!(r.since(5), Vec::<i32>::new());
        assert_eq!(r.last_seq(), 5);
    }

    #[test]
    fn disk_pct_uses_cap_or_drive() {
        let full = DiskStat { used: 500, cap: 500, rolls: true, ..Default::default() };
        let archive = DiskStat { used: 0, cap: 4500, ..Default::default() };
        assert_eq!(headline_pct(&[full.clone(), archive]), 10.0);
        assert_eq!(headline_pct(&[full, DiskStat::default()]), 100.0);
        let mut d = DiskStat { used: 50, cap: 200, ..Default::default() };
        assert_eq!(d.pct(), 25.0);
        d.used = 210;
        assert_eq!(d.pct(), 105.0, "over a fixed cap shows as over");
        d.auto = true;
        assert_eq!(d.pct(), 100.0, "an automatic cap reads full, not over");
        d.cap = 0;
        d.drive_total = Some(1000);
        d.drive_free = Some(100);
        assert_eq!(d.pct(), 90.0);
    }

    #[test]
    fn sample_mean_averages_and_merges_folders() {
        let a = Sample { cpu: 10.0, writes: vec![("Data".into(), 100.0)], ..Default::default() };
        let b = Sample { cpu: 30.0, writes: vec![("Data".into(), 300.0), ("NAS".into(), 50.0)], ..Default::default() };
        let m = Sample::mean(&[a, b]);
        assert_eq!(m.cpu, 20.0);
        assert_eq!(m.writes, vec![("Data".to_string(), 200.0), ("NAS".to_string(), 25.0)]);
    }

    #[test]
    fn json_shape() {
        let s = Stats::new();
        {
            let mut g = s.inner.lock().unwrap();
            g.cams = vec![
                CamStat { id: 0, name: "Front".into(), slug: "front".into(), online: true, enabled: true, fps: 10.04, ..Default::default() },
                CamStat { id: 1, name: "Back".into(), slug: "back".into(), enabled: true, ..Default::default() },
            ];
            g.history.push(Sample { cpu: 12.34, ..Default::default() });
        }
        let j = s.json(60, 0, &|_| true);
        assert_eq!(j["fps"]["cameras"], 2);
        assert_eq!(j["fps"]["streaming"], 1);
        assert_eq!(j["fps"]["total"], 10.0);
        assert_eq!(j["cameras"][1]["online"], false);
        assert_eq!(j["history"]["cpu"][0], 12.3);
        assert_eq!(j["history"]["seq"], 1);
        for k in ["cpu", "mem", "fps", "storage", "cameras", "history", "uptime_s", "ai", "net", "gpu", "history_1h"] {
            assert!(j.get(k).is_some(), "missing {k}");
        }
        let none = s.json(60, 1, &|_| true);
        assert_eq!(none["history"]["cpu"].as_array().unwrap().len(), 0);
        let limited = s.json(60, 0, &|c| c.slug == "front");
        assert_eq!(limited["cameras"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn limited_viewer_gets_no_fps_history_of_other_cameras() {
        let s = Stats::new();
        {
            let mut g = s.inner.lock().unwrap();
            g.cams = vec![
                CamStat { id: 0, slug: "front".into(), enabled: true, fps: 10.0, ..Default::default() },
                CamStat { id: 1, slug: "back".into(), enabled: true, fps: 20.0, ..Default::default() },
            ];
            g.history.push(Sample { fps: 30.0, ..Default::default() });
            g.long.push_back(Sample { fps: 30.0, ..Default::default() });
        }
        let all = s.json(60, 0, &|_| true);
        assert_eq!(all["history"]["fps"][0], 30.0);
        assert_eq!(all["history_1h"]["fps"][0], 30.0);
        let limited = s.json(60, 0, &|c| c.slug == "front");
        assert_eq!(limited["history"]["fps"][0], 0.0);
        assert_eq!(limited["history_1h"]["fps"][0], 0.0);
        assert_eq!(limited["fps"]["total"], 10.0);
    }
}
