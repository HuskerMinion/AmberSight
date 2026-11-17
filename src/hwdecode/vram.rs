//! Video memory on the NVIDIA card, read with `nvidia-smi`, and the
//! headroom guard's decision: Auto puts a camera on NVDEC only while the
//! card keeps room for the other programs on it (the AI, a local model).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Memory of the card NVDEC decodes on. ffmpeg's `-hwaccel cuda` uses its
/// default device, the first one, and `nvidia-smi` lists it first.
pub const NVDEC_GPU: usize = 0;
/// What one more NVDEC decoder takes (150 to 280 MiB seen with 1080p to 4K).
pub const DECODER_MB: u64 = 300;
/// A camera that was turned away comes back only with this much more room,
/// so cameras do not move on and off with every program that grows a little.
pub const BACK_MARGIN_MB: u64 = 1024;
/// How long a reading is used before `nvidia-smi` is asked again.
const FRESH: Duration = Duration::from_secs(5);
/// How often to look again when `nvidia-smi` was not found.
const MISSING_RETRY: Duration = Duration::from_secs(60);
/// A hung `nvidia-smi` is stopped after this.
const SMI_LIMIT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gpu {
    pub free_mb: u64,
    pub total_mb: u64,
}

impl Gpu {
    pub fn used_mb(&self) -> u64 {
        self.total_mb.saturating_sub(self.free_mb)
    }
}

/// What is known about the card's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    /// Not read yet (the first read is still running).
    Unknown,
    /// `nvidia-smi` is not installed or not in PATH, or gave nothing usable.
    Missing,
    Ok(Gpu),
}

/// The lines of `nvidia-smi --query-gpu=memory.free,memory.total
/// --format=csv,noheader,nounits`, one per card in index order ("15000, 16311").
/// A card whose numbers are not given ("[N/A]") is None, keeping the indexes.
pub fn parse(out: &str) -> Vec<Option<Gpu>> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut it = l.split(',').map(|x| x.trim().parse::<u64>().ok());
            match (it.next().flatten(), it.next().flatten()) {
                (Some(free_mb), Some(total_mb)) if total_mb > 0 => Some(Gpu { free_mb: free_mb.min(total_mb), total_mb }),
                _ => None,
            }
        })
        .collect()
}

/// Whether a camera may start on (or move onto) NVDEC. `held`: it is on NVDEC
/// already (its memory is taken; it stays). `denied`: it was turned away
/// before, so it needs the extra margin. `min_mb` 0 turns the guard off.
pub fn has_room(free_mb: u64, min_mb: u64, held: bool, denied: bool) -> bool {
    if min_mb == 0 || held {
        return true;
    }
    free_mb >= min_mb + DECODER_MB + if denied { BACK_MARGIN_MB } else { 0 }
}

/// "14.9 of 16.3 GB used".
pub fn used_text(g: &Gpu) -> String {
    format!("{:.1} of {:.1} GB used", g.used_mb() as f64 / 1000.0, g.total_mb as f64 / 1000.0)
}

struct Cache {
    at: Option<Instant>,
    reading: Reading,
    refreshing: bool,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { at: None, reading: Reading::Unknown, refreshing: false });

fn run_smi() -> Reading {
    let mut paths = vec!["nvidia-smi".to_string()];
    if cfg!(windows) {
        paths.push(r"C:\Windows\System32\nvidia-smi.exe".into());
        paths.push(r"C:\Program Files\NVIDIA Corporation\NVSMI\nvidia-smi.exe".into());
    }
    for p in paths {
        let mut c = std::process::Command::new(&p);
        c.args(["--query-gpu=memory.free,memory.total", "--format=csv,noheader,nounits"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let Ok(mut child) = c.spawn() else { continue };
        let mut stdout = child.stdout.take();
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(o) = stdout.as_mut() {
                let _ = std::io::Read::read_to_string(o, &mut s);
            }
            s
        });
        let t0 = Instant::now();
        let ok = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st.success(),
                Ok(None) if t0.elapsed() < SMI_LIMIT => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
            }
        };
        let text = reader.join().unwrap_or_default();
        if !ok {
            return Reading::Missing;
        }
        return match parse(&text).get(NVDEC_GPU).copied().flatten() {
            Some(g) => Reading::Ok(g),
            None => Reading::Missing,
        };
    }
    Reading::Missing
}

/// Ask `nvidia-smi` now (blocks) and keep the answer.
pub fn refresh_now() -> Reading {
    let r = run_smi();
    let mut c = CACHE.lock().unwrap();
    c.at = Some(Instant::now());
    c.reading = r;
    r
}

/// Read the card again on a thread of its own, unless a read is already
/// running (at most one `nvidia-smi` at a time, however often asked).
pub fn refresh_in_background() {
    if !claim_refresh() {
        return;
    }
    std::thread::spawn(|| {
        refresh_now();
        CACHE.lock().unwrap().refreshing = false;
    });
}

/// Mark a read as running; false when one already is.
fn claim_refresh() -> bool {
    claim(&mut CACHE.lock().unwrap().refreshing)
}

fn claim(running: &mut bool) -> bool {
    !std::mem::replace(running, true)
}

fn stale(c: &Cache) -> bool {
    let age = match c.at {
        None => return true,
        Some(t) => t.elapsed(),
    };
    age >= if c.reading == Reading::Missing { MISSING_RETRY } else { FRESH }
}

/// The last reading; never waits for `nvidia-smi` (an old one is renewed on a
/// thread of its own).
pub fn reading() -> Reading {
    let (r, due) = {
        let c = CACHE.lock().unwrap();
        (c.reading, stale(&c))
    };
    if due {
        refresh_in_background();
    }
    r
}

/// Like `reading`, but waits for the first read or an old one (status page).
pub fn reading_blocking() -> Reading {
    {
        let c = CACHE.lock().unwrap();
        if !stale(&c) {
            return c.reading;
        }
    }
    refresh_now()
}

/// A decoder was started on the card: count its memory in the reading until
/// the next real one, so cameras starting in a row do not all see the same room.
pub fn reserve(mb: u64) {
    let mut c = CACHE.lock().unwrap();
    if let Reading::Ok(g) = &mut c.reading {
        g.free_mb = g.free_mb.saturating_sub(mb);
    }
}

/// Tests: use this reading (kept, not renewed by nvidia-smi; Unknown: read
/// again at the next look, as at start).
#[cfg(test)]
pub fn set_for_test(r: Reading) {
    let mut c = CACHE.lock().unwrap();
    c.at = (r != Reading::Unknown).then(|| Instant::now() + Duration::from_secs(3600));
    c.reading = r;
    c.refreshing = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smi_output_is_read() {
        let g = parse("15000, 16311\n 2048 , 16311\n");
        assert_eq!(g, vec![Some(Gpu { free_mb: 15000, total_mb: 16311 }), Some(Gpu { free_mb: 2048, total_mb: 16311 })]);
        assert_eq!(parse("[N/A], [N/A]\n100, 200"), vec![None, Some(Gpu { free_mb: 100, total_mb: 200 })]);
        assert_eq!(parse(""), Vec::<Option<Gpu>>::new());
        assert_eq!(parse("NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver."), vec![None]);
        assert_eq!(parse("300, 200")[0], Some(Gpu { free_mb: 200, total_mb: 200 }));
        assert_eq!(used_text(&Gpu { free_mb: 1400, total_mb: 16311 }), "14.9 of 16.3 GB used");
    }

    #[test]
    fn only_one_card_read_runs_at_a_time() {
        // (The flag only, on its own copy: a test never runs nvidia-smi.)
        let mut running = false;
        assert!(claim(&mut running));
        for _ in 0..5 {
            assert!(!claim(&mut running), "a second read is not started while one runs");
        }
        running = false;
        assert!(claim(&mut running), "the next one once it is done");
    }

    #[test]
    fn room_for_a_decoder_and_the_margin_to_come_back() {
        let min = 1536;
        let need = min + DECODER_MB;
        assert!(has_room(need, min, false, false));
        assert!(!has_room(need - 1, min, false, false));
        // Turned away before: needs 1 GB more.
        assert!(!has_room(need + BACK_MARGIN_MB - 1, min, false, true));
        assert!(has_room(need + BACK_MARGIN_MB, min, false, true));
        // On the card already: stays, whatever is free.
        assert!(has_room(0, min, true, false));
        // Guard off.
        assert!(has_room(0, 0, false, true));
    }
}
