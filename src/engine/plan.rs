//! Storage planning (Status > Storage): measured MB/day per camera, how many
//! days fit in each recording folder, and the drives a new folder could use.

use super::*;

const DAY_MS: i64 = 86_400_000;
/// Measure over this many days (shorter when there is less history).
const WINDOW_DAYS: i64 = 7;
/// Below this much history the daily figure is extrapolated and marked rough.
const MIN_SPAN_MS: i64 = 3_600_000;

/// Daily rate from `bytes` recorded since `first_ms` (until `now_ms`).
fn per_day(bytes: i64, first_ms: i64, now_ms: i64) -> f64 {
    let span = (now_ms - first_ms).clamp(MIN_SPAN_MS, WINDOW_DAYS * DAY_MS);
    bytes as f64 * DAY_MS as f64 / span as f64
}

/// One recording folder in the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderPlan {
    pub name: String,
    pub archive: bool,
    /// Bytes this folder may hold (its cap, or what fits on the drive).
    pub capacity: f64,
    /// Bytes per day written by the cameras recording here.
    pub rate: f64,
    /// Bytes per day pushed in from other folders' caps (archive only).
    pub inflow: f64,
}

impl FolderPlan {
    /// Days of recordings this folder holds at the current rate.
    pub fn days(&self) -> Option<f64> {
        let r = self.rate + self.inflow;
        (r > 0.0).then(|| self.capacity / r)
    }
}

/// Days a camera's recordings are kept: its folder, plus the archive when its
/// folder is capped and overflows there, never more than `retention_days`.
pub fn days_held(own: &FolderPlan, archive: Option<&FolderPlan>, overflows: bool, retention_days: f64) -> Option<f64> {
    let mut d = own.days()?;
    if overflows {
        if let Some(a) = archive.and_then(|a| a.days()) {
            d += a;
        }
    }
    Some(d.min(retention_days))
}

/// The plan reads the whole recording index, which takes seconds on a big
/// site. The last answer is served for this long; after that the next ask
/// still gets it at once and a background refresh replaces it.
const PLAN_FRESH: std::time::Duration = std::time::Duration::from_secs(60);

impl Engine {
    /// The storage plan, from the last answer when it is under a minute old.
    /// `force` (the Refresh button) works it out again now. The first ask
    /// after start works it out and waits.
    pub fn storage_plan(self: &Arc<Engine>, force: bool) -> serde_json::Value {
        static CACHE: Mutex<Option<(std::time::Instant, serde_json::Value)>> = Mutex::new(None);
        static REFRESHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        static COMPUTE: Mutex<()> = Mutex::new(());
        let age_of = |t: std::time::Instant| t.elapsed().as_secs();
        let stamp = |mut v: serde_json::Value, age: u64| {
            v["age_seconds"] = serde_json::json!(age);
            v
        };
        let cached = CACHE.lock().unwrap().clone();
        if let (Some((t, v)), false) = (&cached, force) {
            if t.elapsed() >= PLAN_FRESH && !REFRESHING.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let engine = self.clone();
                let spawned = std::thread::Builder::new().name("storage-plan".into()).spawn(move || {
                    let _running = super::storage::ClearOnDrop(&REFRESHING);
                    let v = engine.storage_plan_now();
                    *CACHE.lock().unwrap() = Some((std::time::Instant::now(), v));
                });
                if spawned.is_err() {
                    REFRESHING.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
            return stamp(v.clone(), age_of(*t));
        }
        // First ask, or forced: one calculation at a time; asks that arrive
        // meanwhile take its answer.
        let asked = std::time::Instant::now();
        let _one = COMPUTE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, v)) = CACHE.lock().unwrap().clone() {
            if t >= asked {
                return stamp(v, age_of(t));
            }
        }
        let v = self.storage_plan_now();
        *CACHE.lock().unwrap() = Some((std::time::Instant::now(), v.clone()));
        stamp(v, 0)
    }

    fn storage_plan_now(self: &Arc<Engine>) -> serde_json::Value {
        let now = Utc::now().timestamp_millis();
        let cfg = self.config.lock().unwrap().clone();
        // (Each step's time, logged when the whole takes long.)
        let t0 = std::time::Instant::now();
        let mut steps: Vec<(&str, u128)> = Vec::new();
        let mut lap = |name: &'static str, from: &mut std::time::Instant| {
            steps.push((name, from.elapsed().as_millis()));
            *from = std::time::Instant::now();
        };
        let mut t = t0;
        let rates = self.db.camera_rates(now - WINDOW_DAYS * DAY_MS).unwrap_or_default();
        lap("rates", &mut t);
        let hour = self.db.camera_kbps(now - 3_600_000).unwrap_or_default();
        lap("last hour", &mut t);
        let holdings = self.db.camera_holdings().unwrap_or_default();
        lap("holdings", &mut t);
        let caps = self.storage_caps_recent();
        lap("folders", &mut t);
        let levels = self.storage_levels(&caps);
        lap("levels", &mut t);
        if t0.elapsed().as_secs() >= 3 {
            let list: Vec<String> = steps.iter().map(|(n, ms)| format!("{n} {ms} ms")).collect();
            tracing::info!("storage plan took {} s: {}", t0.elapsed().as_secs(), list.join(", "));
        }

        // Folders: "" is the data folder.
        let known: Vec<String> = cfg.storage.folders.iter().map(|f| f.name.clone()).collect();
        let folder_of = |c: &crate::config::CameraConfig| c.folder.clone().filter(|f| known.contains(f)).unwrap_or_default();
        let archive_name = cfg.storage.folders.iter().find(|f| f.archive).map(|f| f.name.clone());

        // Cameras.
        let mut cams = Vec::new();
        let mut rate_in: std::collections::HashMap<String, f64> = Default::default();
        for c in &cfg.cameras {
            let s = slug(&c.name);
            let (bytes, first, _) = rates.iter().find(|r| r.0 == s).map(|r| (r.1, r.2, r.3)).unwrap_or((0, now, 0));
            let day = if bytes > 0 { per_day(bytes, first, now) } else { 0.0 };
            let kbps = hour.iter().find(|r| r.0 == s).map(|r| r.1);
            let held: Vec<&(String, String, i64, i64)> = holdings.iter().filter(|h| h.0 == s).collect();
            let stored: i64 = held.iter().map(|h| h.2).sum();
            let oldest = held.iter().map(|h| h.3).min();
            let folder = folder_of(c);
            if c.enabled && c.record {
                *rate_in.entry(folder.clone()).or_default() += day;
            }
            cams.push(serde_json::json!({
                "name": c.name,
                "folder": folder,
                "enabled": c.enabled,
                "record": c.record,
                "motion_only": c.record_motion_only,
                "bytes_per_day": day.round() as i64,
                "rough": bytes > 0 && now - first < DAY_MS,
                "kbps": kbps.map(|k| k.round() as i64),
                "stored_bytes": stored,
                "oldest_ms": oldest,
            }));
        }

        // Folders: capacity, rate, days.
        let mut plans: Vec<(FolderPlan, serde_json::Value)> = Vec::new();
        for (c, (l, _)) in caps.iter().zip(&levels) {
            let (name, path, archive) = (&c.name, &c.path, &c.archive);
            let used = c.used;
            let (free, total) = (c.free, c.total);
            let room = free.map(|f| used + f);
            // Automatic caps are always worked out from the drive (None when
            // it cannot be read).
            let cap = (c.cap > 0 || (c.auto && c.total.is_some())).then_some(c.cap);
            let capacity = match (cap, room) {
                (Some(c), Some(r)) => c.min(r),
                (Some(c), None) => c,
                (None, Some(r)) => r,
                (None, None) => 0,
            };
            let p = FolderPlan { name: name.clone(), archive: *archive, capacity: capacity as f64, rate: rate_in.get(name).copied().unwrap_or(0.0), inflow: 0.0 };
            let info = serde_json::json!({
                "name": name,
                "label": if name.is_empty() { "Data folder".to_string() } else { name.clone() },
                "path": crate::ai_setup::clean_path(path),
                "archive": archive,
                "auto": c.auto,
                "cap_bytes": cap,
                "used_bytes": used,
                "drive_free": free,
                "drive_total": total,
                "target_free": c.target_free,
                "capacity_bytes": capacity,
                "level": l.level,
                "label_status": l.label,
            });
            plans.push((p, info));
        }
        // What capped folders push out goes to the archive.
        if let Some(an) = &archive_name {
            let inflow: f64 = plans.iter().filter(|(p, i)| !p.archive && i["cap_bytes"].is_u64()).map(|(p, _)| p.rate).sum();
            if let Some((a, _)) = plans.iter_mut().find(|(p, _)| &p.name == an) {
                a.inflow = inflow;
            }
        }
        let archive_plan = archive_name.as_ref().and_then(|an| plans.iter().find(|(p, _)| &p.name == an).map(|(p, _)| p.clone()));
        let retention = cfg.storage.retention_days as f64;
        for cam in cams.iter_mut() {
            let folder = cam["folder"].as_str().unwrap_or("").to_string();
            if let Some((p, i)) = plans.iter().find(|(p, _)| p.name == folder) {
                let overflows = !p.archive && i["cap_bytes"].is_u64();
                let d = if cam["bytes_per_day"].as_i64().unwrap_or(0) > 0 { days_held(p, archive_plan.as_ref(), overflows, retention) } else { None };
                cam["days_kept"] = serde_json::json!(d.map(|d| (d * 10.0).round() / 10.0));
            }
        }
        // Days kept per folder counting the archive it hands its oldest to
        // (`days_total`, with no retention limit), and the shortest of those
        // over every folder that records, limited by the retention setting
        // (`days_kept`): the one figure the Status card and the plan agree on.
        let mut shortest: Option<f64> = None;
        let folders: Vec<serde_json::Value> = plans
            .into_iter()
            .map(|(p, mut i)| {
                i["bytes_per_day"] = serde_json::json!((p.rate + p.inflow).round() as i64);
                i["inflow_per_day"] = serde_json::json!(p.inflow.round() as i64);
                i["days_fit"] = serde_json::json!(p.days().map(|d| (d * 10.0).round() / 10.0));
                let overflows = !p.archive && i["cap_bytes"].is_u64();
                let total = days_held(&p, archive_plan.as_ref(), overflows, f64::INFINITY);
                i["days_total"] = serde_json::json!(total.map(|d| (d * 10.0).round() / 10.0));
                if p.rate > 0.0 {
                    if let Some(d) = total {
                        shortest = Some(shortest.map_or(d, |s: f64| s.min(d)));
                    }
                }
                i
            })
            .collect();

        let db_path = self.data_dir.join("ambersight.db");
        let db_bytes: u64 = ["", "-wal", "-shm"]
            .iter()
            .map(|s| storage::file_size(&std::path::PathBuf::from(format!("{}{s}", db_path.display()))))
            .sum();
        let (segments, events) = self.db.counts().unwrap_or((0, 0));
        serde_json::json!({
            "retention_days": cfg.storage.retention_days,
            "days_kept": shortest.map(|d| (d.min(retention) * 10.0).round() / 10.0),
            "window_days": WINDOW_DAYS,
            "cameras": cams,
            "folders": folders,
            "database": { "bytes": db_bytes, "segments": segments, "events": events },
        })
    }
}

/// Drives / mounted filesystems with total and free space.
pub fn list_drives() -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for p in drive_roots() {
        let (Ok(total), Ok(free)) = (fs4::total_space(&p), fs4::available_space(&p)) else { continue };
        if total == 0 {
            continue;
        }
        out.push(serde_json::json!({ "path": crate::ai_setup::clean_path(&p), "total": total, "free": free }));
    }
    out
}

#[cfg(windows)]
fn drive_roots() -> Vec<std::path::PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    // 2 removable, 3 fixed, 4 network.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .filter(|d| {
            let w: Vec<u16> = d.encode_utf16().chain(std::iter::once(0)).collect();
            matches!(unsafe { GetDriveTypeW(w.as_ptr()) }, 2..=4)
        })
        .map(std::path::PathBuf::from)
        .collect()
}

#[cfg(not(windows))]
fn drive_roots() -> Vec<std::path::PathBuf> {
    const REAL: &[&str] = &["ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "ntfs", "ntfs3", "fuseblk", "vfat", "exfat", "nfs", "nfs4", "cifs", "smb3", "apfs", "hfs"];
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(m) = std::fs::read_to_string("/proc/mounts") {
        for line in m.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 3 || !REAL.contains(&f[2]) {
                continue;
            }
            let mp = f[1].replace("\\040", " ");
            if mp.starts_with("/boot") || mp.starts_with("/snap") || mp.starts_with("/var/lib/docker") {
                continue;
            }
            out.push(mp.into());
        }
    } else {
        // macOS: the root and anything under /Volumes.
        out.push("/".into());
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            out.extend(rd.flatten().map(|e| e.path()));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(name: &str, cap_gb: f64, rate_gb: f64, archive: bool) -> FolderPlan {
        FolderPlan { name: name.into(), archive, capacity: cap_gb * 1e9, rate: rate_gb * 1e9, inflow: 0.0 }
    }

    #[test]
    fn rates_use_the_history_there_is() {
        let now = 100 * DAY_MS;
        // 10 GB over the full week -> about 1.43 GB/day.
        assert!((per_day(10_000_000_000, now - 7 * DAY_MS, now) - 1.428e9).abs() < 1e7);
        // 1 GB in the last 12 hours -> 2 GB/day.
        assert!((per_day(1_000_000_000, now - DAY_MS / 2, now) - 2e9).abs() < 1e6);
        // Older history than the window does not dilute it.
        assert!((per_day(7_000_000_000, now - 30 * DAY_MS, now) - 1e9).abs() < 1e6);
    }

    #[test]
    fn days_with_and_without_archive() {
        let own = fp("data", 100.0, 10.0, false);
        let mut arch = fp("arch", 400.0, 0.0, true);
        arch.inflow = 10e9;
        assert_eq!(own.days(), Some(10.0));
        assert_eq!(days_held(&own, Some(&arch), true, 365.0), Some(50.0));
        assert_eq!(days_held(&own, Some(&arch), false, 365.0), Some(10.0));
        // Never more than the retention setting.
        assert_eq!(days_held(&own, Some(&arch), true, 14.0), Some(14.0));
        // Nothing recorded there: no estimate.
        assert_eq!(fp("x", 1.0, 0.0, false).days(), None);
    }
}
