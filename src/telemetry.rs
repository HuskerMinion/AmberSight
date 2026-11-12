//! Performance telemetry for `/api/stats`: AI check timing, bytes written per
//! recording folder, network throughput and graphics-card load. Everything
//! here is cheap to record (an atomic add or a short lock) and is read by the
//! stats sampler thread, never on a request.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// AI checks
// ---------------------------------------------------------------------------

/// Checks older than this are forgotten.
const AI_KEEP: Duration = Duration::from_secs(600);
/// A check running longer than this, or no success for this long after one was
/// asked for, means the AI is stalled.
pub const STALL_S: f64 = 120.0;

#[derive(Default)]
struct AiLog {
    /// (finished at, inference ms, ok)
    done: VecDeque<(Instant, f64, bool)>,
    /// Checks running now, by ticket.
    inflight: HashMap<u64, Instant>,
    next: u64,
    last_ok: Option<Instant>,
}

static AI: Mutex<Option<AiLog>> = Mutex::new(None);

fn with_ai<R>(f: impl FnOnce(&mut AiLog) -> R) -> R {
    let mut g = AI.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(AiLog::default))
}

/// Note that an AI check starts; hand the ticket to [`ai_end`].
pub fn ai_begin() -> u64 {
    with_ai(|l| {
        l.next += 1;
        let t = l.next;
        l.inflight.insert(t, Instant::now());
        t
    })
}

/// Ends its ticket as failed when dropped unfinished (a panic mid-check must
/// not leave a ticket "running" forever and show the AI as stalled).
pub struct AiTicket {
    id: u64,
    done: bool,
}

impl AiTicket {
    pub fn begin() -> Self {
        AiTicket { id: ai_begin(), done: false }
    }
    pub fn finish(mut self, ok: bool) {
        self.done = true;
        ai_end(self.id, ok);
    }
}

impl Drop for AiTicket {
    fn drop(&mut self) {
        if !self.done {
            ai_end(self.id, false);
        }
    }
}

/// Note that the check finished (`ok` false: it failed).
pub fn ai_end(ticket: u64, ok: bool) {
    with_ai(|l| {
        let now = Instant::now();
        let ms = l.inflight.remove(&ticket).map_or(0.0, |s| now.duration_since(s).as_secs_f64() * 1000.0);
        while l.done.front().is_some_and(|d| now.duration_since(d.0) > AI_KEEP) {
            l.done.pop_front();
        }
        if l.done.len() >= 4000 {
            l.done.pop_front();
        }
        l.done.push_back((now, ms, ok));
        if ok {
            l.last_ok = Some(now);
        }
    })
}

/// Percentile (0..=100, nearest rank) of `v`; None when empty.
pub fn percentile(v: &[f64], p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p / 100.0) * s.len() as f64).ceil().max(1.0) as usize;
    Some(s[rank.min(s.len()) - 1])
}

/// The AI's state from the recent record. `ok_*` / `err_*` count finished
/// checks in the stall window (120 s) and the last minute; `oldest_inflight_s`
/// is how long the longest running check has been going.
pub fn ai_state(on: bool, ok_win: usize, err_win: usize, ok_1m: usize, err_1m: usize, oldest_inflight_s: Option<f64>) -> &'static str {
    if !on {
        return "off";
    }
    if oldest_inflight_s.is_some_and(|s| s > STALL_S) {
        return "stalled";
    }
    // Asked for, but nothing came back right in the whole window. One lone
    // failure is not enough evidence (a blip, a bad frame).
    if err_win >= 2 && ok_win == 0 {
        return "stalled";
    }
    // Errors dominate a busy minute.
    if err_1m >= 3 && err_1m > ok_1m {
        return "stalled";
    }
    if ok_win + err_win == 0 && oldest_inflight_s.is_none() {
        return "idle";
    }
    "ok"
}

#[derive(Clone, Debug, Default)]
pub struct AiSnap {
    pub state: &'static str,
    pub device: String,
    pub checks_1m: usize,
    pub errors_1m: usize,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub last_ok_s: Option<f64>,
}

/// The current AI picture. `on` / `device` come from the engine.
pub fn ai_snapshot(on: bool, device: String) -> AiSnap {
    with_ai(|l| {
        let now = Instant::now();
        let age = |t: Instant| now.duration_since(t).as_secs_f64();
        let (mut ok_w, mut err_w, mut ok_m, mut err_m) = (0, 0, 0, 0);
        let mut times = Vec::new();
        for &(t, ms, ok) in &l.done {
            let a = age(t);
            if a <= STALL_S {
                if ok { ok_w += 1 } else { err_w += 1 }
            }
            if a <= 60.0 {
                if ok {
                    ok_m += 1;
                    times.push(ms);
                } else {
                    err_m += 1;
                }
            }
        }
        let oldest = l.inflight.values().map(|&t| age(t)).fold(None, |m: Option<f64>, a| Some(m.map_or(a, |x| x.max(a))));
        AiSnap {
            state: ai_state(on, ok_w, err_w, ok_m, err_m, oldest),
            device,
            checks_1m: ok_m + err_m,
            errors_1m: err_m,
            p50_ms: percentile(&times, 50.0),
            p95_ms: percentile(&times, 95.0),
            last_ok_s: l.last_ok.map(age),
        }
    })
}

// ---------------------------------------------------------------------------
// Bytes written to recording folders
// ---------------------------------------------------------------------------

static WRITES: Mutex<Vec<(PathBuf, Arc<AtomicU64>)>> = Mutex::new(Vec::new());

/// The byte counter for recordings written under `dir` (one per directory).
pub fn write_counter(dir: &Path) -> Arc<AtomicU64> {
    let mut g = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, c)) = g.iter().find(|(d, _)| d == dir) {
        return c.clone();
    }
    let c = Arc::new(AtomicU64::new(0));
    g.push((dir.to_path_buf(), c.clone()));
    c
}

/// Forget counters whose folder is gone and that no recorder holds any more.
fn prune_writes_now() {
    let mut g = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|(d, c)| Arc::strong_count(c) > 1 || d.exists());
}

/// [`prune_writes_now`], at most once a minute.
fn prune_writes() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut l = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if l.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
        return;
    }
    *l = Some(Instant::now());
    drop(l);
    prune_writes_now();
}

/// Total bytes written so far under each root (the longest matching root
/// wins, so a folder nested in the data folder is not counted twice).
pub fn written_by_root(roots: &[PathBuf]) -> Vec<u64> {
    prune_writes();
    let g = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = vec![0u64; roots.len()];
    for (dir, c) in g.iter() {
        let best = roots.iter().enumerate().filter(|(_, r)| dir.starts_with(r)).max_by_key(|(_, r)| r.components().count());
        if let Some((i, _)) = best {
            out[i] += c.load(Ordering::Relaxed);
        }
    }
    out
}

/// Per-second rate between two readings of a counter; 0 for a counter that
/// went backwards or an interval too short to say.
pub fn rate(prev: u64, now: u64, secs: f64) -> f64 {
    if secs < 0.25 || now < prev {
        return 0.0;
    }
    (now - prev) as f64 / secs
}

/// How much a counter grew over the last `window` seconds (sliding).
#[derive(Default)]
pub struct WindowDelta {
    pts: VecDeque<(f64, u64)>,
}

impl WindowDelta {
    pub fn update(&mut self, t: f64, count: u64, window: f64) -> u64 {
        if self.pts.back().is_some_and(|&(_, c)| count < c) {
            self.pts.clear();
        }
        self.pts.push_back((t, count));
        while self.pts.len() > 2 && t - self.pts[1].0 >= window {
            self.pts.pop_front();
        }
        count - self.pts[0].1
    }
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// Total (received, sent) bytes over all real network interfaces.
pub struct NetMeter {
    nets: Option<sysinfo::Networks>,
}

impl NetMeter {
    pub fn new() -> Self {
        NetMeter { nets: sysinfo::Networks::new_with_refreshed_list().ok() }
    }
    /// Cumulative (rx, tx) bytes since the meter was created, or None.
    pub fn totals(&mut self) -> Option<(u64, u64)> {
        let n = self.nets.as_mut()?;
        n.refresh(true);
        let (mut rx, mut tx) = (0u64, 0u64);
        let mut any = false;
        for (name, d) in n.iter() {
            if is_virtual_iface(&name.to_string_lossy()) {
                continue;
            }
            any = true;
            rx += d.total_received();
            tx += d.total_transmitted();
        }
        any.then_some((rx, tx))
    }
}

/// Loopback and container/VM bridges would double count traffic.
pub fn is_virtual_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "lo" || n.starts_with("loopback") || n.starts_with("docker") || n.starts_with("veth") || n.starts_with("br-") || n.starts_with("virbr")
}

// ---------------------------------------------------------------------------
// Graphics cards
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct GpuStat {
    pub name: String,
    /// Busy share 0..100.
    pub util: f64,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
}

/// Samples graphics-card load: Windows through the performance counters
/// ("GPU Engine" / "GPU Adapter Memory", works for NVIDIA, AMD and Intel);
/// Linux through the amdgpu sysfs files. None when nothing can be read.
pub struct GpuMeter {
    #[cfg(windows)]
    win: Option<win::Pdh>,
}

impl GpuMeter {
    pub fn new() -> Self {
        GpuMeter {
            #[cfg(windows)]
            win: win::Pdh::open(),
        }
    }

    #[cfg(windows)]
    pub fn sample(&mut self) -> Option<Vec<GpuStat>> {
        self.win.as_mut()?.sample()
    }

    #[cfg(not(windows))]
    pub fn sample(&mut self) -> Option<Vec<GpuStat>> {
        sysfs_gpus()
    }
}

/// amdgpu exposes `gpu_busy_percent` and `mem_info_vram_*` under the device.
#[cfg(not(windows))]
fn sysfs_gpus() -> Option<Vec<GpuStat>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !n.starts_with("card") || n.contains('-') {
            continue;
        }
        let dev = e.path().join("device");
        let read = |f: &str| std::fs::read_to_string(dev.join(f)).ok().map(|s| s.trim().to_string());
        let Some(busy) = read("gpu_busy_percent").and_then(|s| s.parse::<f64>().ok()) else { continue };
        let num = |f: &str| read(f).and_then(|s| s.parse::<u64>().ok());
        let name = read("product_name").unwrap_or_else(|| format!("AMD GPU ({n})"));
        out.push(GpuStat { name, util: busy.clamp(0.0, 100.0), vram_used: num("mem_info_vram_used"), vram_total: num("mem_info_vram_total") });
    }
    (!out.is_empty()).then_some(out)
}

/// Windows: the share of an adapter in use is, like Task Manager, its busiest
/// engine type (3D, copy, video decode...) summed over all processes.
/// `inst` is a counter instance name such as
/// `pid_1234_luid_0x00000000_0x0000D1A2_phys_0_eng_0_engtype_3D`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_gpu_instance(inst: &str) -> Option<(String, String)> {
    let i = inst.find("luid_")?;
    let rest = &inst[i + 5..];
    let mut parts = rest.split('_');
    let (a, b) = (parts.next()?, parts.next()?);
    let luid = format!("{}_{}", a.to_ascii_lowercase(), b.to_ascii_lowercase());
    let ty = inst.find("engtype_").map(|j| inst[j + 8..].to_string()).unwrap_or_default();
    Some((luid, ty))
}

/// Combine engine readings (luid, engine type, percent) into one busy share
/// per adapter: sum per engine type, then the largest type.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn adapter_util(readings: &[(String, String, f64)]) -> HashMap<String, f64> {
    let mut per: HashMap<(String, String), f64> = HashMap::new();
    for (l, t, v) in readings {
        *per.entry((l.clone(), t.clone())).or_default() += v;
    }
    let mut out: HashMap<String, f64> = HashMap::new();
    for ((l, _), v) in per {
        let e = out.entry(l).or_default();
        *e = e.max(v.clamp(0.0, 100.0));
    }
    out
}

#[cfg(windows)]
mod win {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
    use windows::Win32::System::Performance::*;

    /// PDH_FMT_NOCAP100: do not cap a busy share at 100 (summed over processes).
    const FMT: PDH_FMT = PDH_FMT(PDH_FMT_DOUBLE.0 | 0x8000);

    pub struct Pdh {
        query: PDH_HQUERY,
        util: PDH_HCOUNTER,
        mem: Option<PDH_HCOUNTER>,
        /// luid -> (name, dedicated memory bytes)
        cards: HashMap<String, (String, u64)>,
        primed: bool,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn adapters() -> HashMap<String, (String, u64)> {
        let mut out = HashMap::new();
        let Ok(f) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return out };
        for i in 0..16u32 {
            let Ok(a) = (unsafe { f.EnumAdapters1(i) }) else { break };
            let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let len = d.Description.iter().position(|c| *c == 0).unwrap_or(d.Description.len());
            let luid = format!("0x{:08x}_0x{:08x}", d.AdapterLuid.HighPart as u32, d.AdapterLuid.LowPart);
            out.insert(luid, (String::from_utf16_lossy(&d.Description[..len]).trim().to_string(), d.DedicatedVideoMemory as u64));
        }
        out
    }

    impl Pdh {
        pub fn open() -> Option<Pdh> {
            unsafe {
                let mut query = PDH_HQUERY::default();
                if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                    return None;
                }
                let mut util = PDH_HCOUNTER::default();
                let p = wide("\\GPU Engine(*)\\Utilization Percentage");
                if PdhAddEnglishCounterW(query, PCWSTR(p.as_ptr()), 0, &mut util) != 0 {
                    let _ = PdhCloseQuery(query);
                    return None;
                }
                let mut m = PDH_HCOUNTER::default();
                let p = wide("\\GPU Adapter Memory(*)\\Dedicated Usage");
                let mem = (PdhAddEnglishCounterW(query, PCWSTR(p.as_ptr()), 0, &mut m) == 0).then_some(m);
                let cards = adapters();
                if cards.is_empty() {
                    let _ = PdhCloseQuery(query);
                    return None;
                }
                Some(Pdh { query, util, mem, cards, primed: false })
            }
        }

        /// (instance name, value) for every instance of a wildcard counter.
        unsafe fn array(counter: PDH_HCOUNTER) -> Vec<(String, f64)> {
            let (mut size, mut count) = (0u32, 0u32);
            // First call reports the buffer size (PDH_MORE_DATA).
            let _ = PdhGetFormattedCounterArrayW(counter, FMT, &mut size, &mut count, None);
            if size == 0 {
                return Vec::new();
            }
            // 8-byte aligned buffer.
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(counter, FMT, &mut size, &mut count, Some(items)) != 0 {
                return Vec::new();
            }
            (0..count as usize)
                .map(|i| {
                    let it = &*items.add(i);
                    (it.szName.to_string().unwrap_or_default(), it.FmtValue.Anonymous.doubleValue)
                })
                .collect()
        }

        pub fn sample(&mut self) -> Option<Vec<GpuStat>> {
            unsafe {
                if PdhCollectQueryData(self.query) != 0 {
                    return None;
                }
                // Rates need two collections: the first only primes them.
                if !self.primed {
                    self.primed = true;
                    return None;
                }
                let readings: Vec<(String, String, f64)> = Self::array(self.util)
                    .into_iter()
                    .filter_map(|(n, v)| parse_gpu_instance(&n).map(|(l, t)| (l, t, v)))
                    .collect();
                let util = adapter_util(&readings);
                let mut vram: HashMap<String, u64> = HashMap::new();
                if let Some(m) = self.mem {
                    for (n, v) in Self::array(m) {
                        if let Some((l, _)) = parse_gpu_instance(&n) {
                            *vram.entry(l).or_default() += v.max(0.0) as u64;
                        }
                    }
                }
                let mut out: Vec<GpuStat> = self
                    .cards
                    .iter()
                    .map(|(l, (name, total))| GpuStat {
                        name: name.clone(),
                        util: util.get(l).copied().unwrap_or(0.0),
                        vram_used: vram.get(l).copied(),
                        vram_total: (*total > 0).then_some(*total),
                    })
                    .collect();
                out.sort_by(|a, b| b.vram_total.cmp(&a.vram_total).then(a.name.cmp(&b.name)));
                Some(out)
            }
        }
    }

    impl Drop for Pdh {
        fn drop(&mut self) {
            unsafe {
                let _ = PdhCloseQuery(self.query);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_nearest_rank() {
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[5.0], 95.0), Some(5.0));
        let v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert_eq!(percentile(&v, 50.0), Some(50.0));
        assert_eq!(percentile(&v, 95.0), Some(95.0));
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 100.0), Some(3.0));
    }

    #[test]
    fn ai_state_rules() {
        assert_eq!(ai_state(false, 5, 0, 5, 0, None), "off");
        assert_eq!(ai_state(true, 0, 0, 0, 0, None), "idle");
        assert_eq!(ai_state(true, 3, 0, 1, 0, None), "ok");
        // Asked for, nothing came back right.
        assert_eq!(ai_state(true, 0, 2, 0, 2, None), "stalled");
        // A hung check.
        assert_eq!(ai_state(true, 0, 0, 0, 0, Some(200.0)), "stalled");
        assert_eq!(ai_state(true, 0, 0, 0, 0, Some(5.0)), "ok");
        // Errors dominate.
        assert_eq!(ai_state(true, 2, 8, 2, 8, None), "stalled");
        assert_eq!(ai_state(true, 9, 2, 9, 2, None), "ok");
        // Two errors with no success in the window is stalled; one lone error is not.
        assert_eq!(ai_state(true, 0, 2, 0, 2, None), "stalled");
        assert_eq!(ai_state(true, 0, 1, 0, 1, None), "ok");
    }

    #[test]
    fn dropped_ticket_is_not_left_inflight() {
        let id = Arc::new(AtomicU64::new(0));
        let id2 = id.clone();
        let r = std::panic::catch_unwind(move || {
            let t = AiTicket::begin();
            id2.store(t.id, Ordering::SeqCst);
            panic!("boom");
        });
        assert!(r.is_err());
        let id = id.load(Ordering::SeqCst);
        assert!(id > 0);
        assert!(!with_ai(|l| l.inflight.contains_key(&id)), "ticket must be ended by Drop");
    }

    #[test]
    fn deleted_folder_counters_are_pruned() {
        let gone = PathBuf::from("/tmp/tt-prune-gone/cameras/x");
        drop(write_counter(&gone));
        prune_writes_now();
        let g = WRITES.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!g.iter().any(|(d, _)| d == &gone));
    }

    #[test]
    fn rate_math() {
        assert_eq!(rate(100, 300, 2.0), 100.0);
        assert_eq!(rate(300, 100, 2.0), 0.0);
        assert_eq!(rate(1, 2, 0.0), 0.0);
        let mut w = WindowDelta::default();
        assert_eq!(w.update(0.0, 0, 60.0), 0);
        assert_eq!(w.update(30.0, 5, 60.0), 5);
        assert_eq!(w.update(61.0, 9, 60.0), 9, "keeps the point just outside the window");
        assert_eq!(w.update(130.0, 9, 60.0), 0);
        assert_eq!(w.update(131.0, 2, 60.0), 0, "counter reset");
    }

    #[test]
    fn write_counters_group_by_root() {
        let a = write_counter(Path::new("/tmp/tt-data/cameras/a"));
        let b = write_counter(Path::new("/tmp/tt-nas/cameras/b"));
        assert!(Arc::ptr_eq(&a, &write_counter(Path::new("/tmp/tt-data/cameras/a"))));
        a.fetch_add(10, Ordering::Relaxed);
        b.fetch_add(7, Ordering::Relaxed);
        let roots = vec![PathBuf::from("/tmp/tt-data"), PathBuf::from("/tmp/tt-nas"), PathBuf::from("/tmp/tt-data/cameras/a")];
        assert_eq!(written_by_root(&roots), vec![0, 7, 10]);
    }

    #[test]
    fn gpu_instances_and_adapter_share() {
        let (l, t) = parse_gpu_instance("pid_12_luid_0x00000000_0x0000D1A2_phys_0_eng_3_engtype_3D").unwrap();
        assert_eq!((l.as_str(), t.as_str()), ("0x00000000_0x0000d1a2", "3D"));
        let (l2, _) = parse_gpu_instance("luid_0x00000000_0x0000D1A2_phys_0").unwrap();
        assert_eq!(l, l2);
        assert!(parse_gpu_instance("nonsense").is_none());
        let r = vec![
            ("a".to_string(), "3D".to_string(), 30.0),
            ("a".to_string(), "3D".to_string(), 20.0),
            ("a".to_string(), "Copy".to_string(), 10.0),
            ("b".to_string(), "VideoDecode".to_string(), 5.0),
        ];
        let u = adapter_util(&r);
        assert_eq!(u["a"], 50.0);
        assert_eq!(u["b"], 5.0);
    }

    #[test]
    fn virtual_interfaces_skipped() {
        assert!(is_virtual_iface("lo"));
        assert!(is_virtual_iface("docker0"));
        assert!(!is_virtual_iface("enp5s0"));
    }
}
