//! How many processes and threads this server may start (the cgroup's
//! pids limit, the account's process limit), and plain words for the day
//! the system says "Resource temporarily unavailable" when ffmpeg or the
//! recorders ask for one more. A container started with a low
//! `--pids-limit` hits it with a few 4K cameras on the ffmpeg pipeline.

use serde_json::{json, Value};

/// What could be read; a field is None when it is not readable here (not
/// Linux, no cgroup).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcLimits {
    /// The cgroup's pids.max: a number, or "max" for no limit.
    pub pids_max: Option<String>,
    /// pids.current: processes and threads in use in that cgroup.
    pub pids_current: Option<u64>,
    /// RLIMIT_NPROC (soft): "Max processes" in /proc/self/limits.
    pub nproc: Option<String>,
}

impl ProcLimits {
    pub fn is_empty(&self) -> bool {
        self.pids_max.is_none() && self.pids_current.is_none() && self.nproc.is_none()
    }

    /// "pids.max 100, in use 97" (what is known).
    pub fn describe(&self) -> Option<String> {
        let mut p = Vec::new();
        if let Some(m) = &self.pids_max {
            p.push(format!("pids.max {m}"));
        }
        if let Some(c) = self.pids_current {
            p.push(format!("in use {c}"));
        }
        (!p.is_empty()).then(|| p.join(", "))
    }

    /// One line for the report ("cgroup pids.max 100, in use 97; ulimit -u 63000").
    pub fn report_line(&self) -> Option<String> {
        let mut p = Vec::new();
        if let Some(d) = self.describe() {
            p.push(format!("cgroup {d}"));
        }
        if let Some(n) = &self.nproc {
            p.push(format!("ulimit -u {n}"));
        }
        (!p.is_empty()).then(|| p.join("; "))
    }

    pub fn to_json(&self) -> Value {
        json!({ "pids_max": self.pids_max, "pids_current": self.pids_current, "nproc_limit": self.nproc })
    }
}

/// A cgroup file's value: a number or "max".
#[cfg(any(target_os = "linux", test))]
fn parse_pids_max(s: &str) -> Option<String> {
    let t = s.trim();
    (t == "max" || (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))).then(|| t.to_string())
}

#[cfg(any(target_os = "linux", test))]
fn parse_count(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

/// The soft "Max processes" limit from the text of /proc/self/limits.
#[cfg(any(target_os = "linux", test))]
pub fn parse_nproc(limits: &str) -> Option<String> {
    let l = limits.lines().find(|l| l.starts_with("Max processes"))?;
    let v = l["Max processes".len()..].split_whitespace().next()?;
    (v == "unlimited" || v.bytes().all(|b| b.is_ascii_digit())).then(|| v.to_string())
}

/// The cgroup v2 path of this process from the text of /proc/self/cgroup
/// ("0::/system.slice/x.service"); None on v1 or at the root.
#[cfg(any(target_os = "linux", test))]
fn v2_path(cgroup: &str) -> Option<String> {
    let p = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    (p.len() > 1 && !p.contains("..")).then(|| p.to_string())
}

#[cfg(target_os = "linux")]
pub fn read() -> ProcLimits {
    use std::fs::read_to_string as rd;
    let mut out = ProcLimits::default();
    // v2: in a container the cgroup is the root of its view; otherwise ours
    // by path. v1: the pids controller.
    let mut dirs = vec!["/sys/fs/cgroup".to_string()];
    if let Some(p) = rd("/proc/self/cgroup").ok().and_then(|c| v2_path(&c)) {
        dirs.push(format!("/sys/fs/cgroup{p}"));
    }
    dirs.push("/sys/fs/cgroup/pids".to_string());
    for d in dirs {
        let max = rd(format!("{d}/pids.max")).ok().and_then(|s| parse_pids_max(&s));
        if max.is_some() {
            out.pids_max = max;
            out.pids_current = rd(format!("{d}/pids.current")).ok().and_then(|s| parse_count(&s));
            break;
        }
    }
    out.nproc = rd("/proc/self/limits").ok().and_then(|s| parse_nproc(&s));
    out
}

#[cfg(not(target_os = "linux"))]
pub fn read() -> ProcLimits {
    ProcLimits::default()
}

/// Whether one line of ffmpeg's output (or an error from starting it) says
/// the system would not start another process or thread. Not every
/// "Resource temporarily unavailable" counts: a socket read says it too.
pub fn is_spawn_limit_line(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("pthread_create")
        // ("os error 11" is EAGAIN on Linux only: on Windows 11 is a bad
        // program file, on macOS EAGAIN is 35.)
        || (cfg!(target_os = "linux") && l.match_indices("os error 11").any(|(i, m)| !l[i + m.len()..].starts_with(|c: char| c.is_ascii_digit())))
        || (l.contains("resource temporarily unavailable") && (l.contains("opening decoder") || l.contains("opening encoder") || l.contains("initializing") || l.contains("could not start") || l.contains("spawn")))
}

/// Whether an error from starting a process says the system would not
/// start another one (EAGAIN from fork/clone). Windows has no such limit
/// to tell apart: never.
pub fn is_spawn_error(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EAGAIN)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

/// Whether any line of the text is one.
pub fn is_spawn_limit(text: &str) -> bool {
    text.lines().any(is_spawn_limit_line)
}

/// The cause in words, for the camera's note. `container`: running in a
/// Docker or Podman container.
pub fn limit_words(limits: &ProcLimits, container: bool) -> String {
    let has = limits.describe().map(|d| format!(" ({d})")).unwrap_or_default();
    if container {
        format!("the system would not start another process or thread (Resource temporarily unavailable): in a container this is usually its process limit{has}; raise or remove it (Docker: --pids-limit, Compose: pids_limit)")
    } else {
        format!("the system would not start another process or thread (Resource temporarily unavailable): a process or thread limit was reached{has}; check ulimit -u and, for a service, TasksMax")
    }
}

/// `limit_words` for this server now.
pub fn limit_words_now() -> String {
    limit_words(&read(), crate::update::in_container())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_lines_from_a_limited_container_are_recognized() {
        for l in [
            "[dec:hevc @ 0x55d1] Error while opening decoder: Resource temporarily unavailable",
            "pthread_create() failed: Resource temporarily unavailable",
            "[vist#0:0/h264 @ 0x1] Error while opening decoder: Resource temporarily unavailable",
        ] {
            assert!(is_spawn_limit_line(l), "{l}");
        }
        // (Linux's EAGAIN in a Rust error: the words say it on every system.)
        assert!(is_spawn_limit_line("camera 'side1': could not start the main stream recorder: Resource temporarily unavailable (os error 11)"));
        assert!(!is_spawn_limit_line("Task finished with error code: -22 (Invalid argument)"));
        assert!(!is_spawn_limit_line("[tcp @ 0x1] Resource temporarily unavailable"));
        // "os error 11" is EAGAIN on Linux only (Windows: a bad program file).
        let w11 = "could not start the main stream recorder: %1 is not a valid Win32 application. (os error 11)";
        assert_eq!(is_spawn_limit_line(w11), cfg!(target_os = "linux"));
        assert!(!is_spawn_limit_line("rtsp://h/s: 401 Unauthorized"));
        assert!(!is_spawn_limit_line("Connection refused (os error 111)"));
        assert!(is_spawn_limit("x\npthread_create() failed: Resource temporarily unavailable\nTask finished with error code: -22 (Invalid argument)\n"));
    }

    #[test]
    fn spawn_errors_are_told_by_their_error_number() {
        #[cfg(unix)]
        {
            assert!(is_spawn_error(&std::io::Error::from_raw_os_error(libc::EAGAIN)));
            assert!(!is_spawn_error(&std::io::Error::from_raw_os_error(libc::ENOENT)));
        }
        #[cfg(windows)]
        {
            // ERROR_BAD_FORMAT (11): not a process limit.
            assert!(!is_spawn_error(&std::io::Error::from_raw_os_error(11)));
        }
        assert!(!is_spawn_error(&std::io::Error::new(std::io::ErrorKind::Other, "Resource temporarily unavailable (os error 11)")));
    }

    #[test]
    fn words_say_what_to_do_and_do_not_overclaim() {
        let l = ProcLimits { pids_max: Some("100".into()), pids_current: Some(98), nproc: Some("63000".into()) };
        let c = limit_words(&l, true);
        assert!(c.contains("in a container this is usually its process limit (pids.max 100, in use 98)") && c.contains("--pids-limit"), "{c}");
        let h = limit_words(&l, false);
        assert!(!h.contains("container") && h.contains("ulimit -u") && h.contains("pids.max 100"), "{h}");
        let none = limit_words(&ProcLimits::default(), true);
        assert!(none.contains("usually its process limit;"), "{none}");
        assert_eq!(l.report_line().unwrap(), "cgroup pids.max 100, in use 98; ulimit -u 63000");
        assert!(ProcLimits::default().report_line().is_none() && ProcLimits::default().is_empty());
        assert_eq!(l.to_json()["pids_max"], "100");
    }

    #[test]
    fn files_are_parsed() {
        assert_eq!(parse_pids_max("100\n").as_deref(), Some("100"));
        assert_eq!(parse_pids_max("max\n").as_deref(), Some("max"));
        assert_eq!(parse_pids_max("nonsense"), None);
        assert_eq!(parse_pids_max(""), None);
        let t = "Limit                     Soft Limit           Hard Limit           Units\nMax cpu time              unlimited            unlimited            seconds\nMax processes             63000                63000                processes\n";
        assert_eq!(parse_nproc(t).as_deref(), Some("63000"));
        assert_eq!(parse_nproc("Max processes             unlimited   unlimited processes").as_deref(), Some("unlimited"));
        assert_eq!(v2_path("0::/system.slice/ambersight.service\n").as_deref(), Some("/system.slice/ambersight.service"));
        assert_eq!(v2_path("0::/\n"), None);
        assert_eq!(v2_path("12:pids:/docker/abc\n"), None);
        assert_eq!(parse_count(" 7\n"), Some(7));
    }

    #[test]
    fn reading_this_machine_does_not_panic() {
        let _ = read();
    }
}
