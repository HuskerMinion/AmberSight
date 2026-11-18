//! Child processes (ffmpeg) must never outlive AmberSight, even when it is
//! killed abruptly: on Linux the kernel kills them when the parent dies; on
//! Windows they are placed in a job object that closes with the process.

use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once AmberSight was told to stop. A service stop (systemd) signals
/// every process of the service, ffmpeg included, so a child that ends now
/// was stopped from outside: not a failure of what it was doing.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// AmberSight is stopping (see `STOPPING`).
pub fn set_stopping() {
    STOPPING.store(true, Ordering::SeqCst);
}

/// Whether AmberSight was told to stop.
pub fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

/// Whether an ffmpeg ended because it was sent a signal: it catches SIGTERM
/// and SIGINT and then exits with 255 (it uses that code for nothing else);
/// a signal it does not catch shows as such. SIGKILL is left out: that is
/// AmberSight's own `kill` of a stuck decoder, or the system out of memory.
pub fn ended_by_signal(st: &std::process::ExitStatus) -> bool {
    if st.code() == Some(255) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            return sig == libc::SIGTERM || sig == libc::SIGINT || sig == libc::SIGHUP;
        }
    }
    false
}

/// An `ffmpeg` command that will not outlive this process.
pub fn ffmpeg() -> Command {
    let mut c = Command::new(crate::ffmpeg_bin::program());
    die_with_parent(&mut c);
    c
}

#[cfg(target_os = "linux")]
fn die_with_parent(c: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: prctl is async-signal-safe and touches no shared state.
    unsafe {
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_c: &mut Command) {}

/// Call right after spawning a child from `ffmpeg()`.
#[cfg(windows)]
pub fn adopt(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::*;
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        // windows-sys 0.52: HANDLE is an isize, 0 = failure.
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job == 0 {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        job as usize
    });
    if job != 0 {
        unsafe {
            AssignProcessToJobObject(job as isize, child.as_raw_handle() as isize);
        }
    }
}

#[cfg(not(windows))]
pub fn adopt(_child: &Child) {}

/// Options of ours that take no value (everything else starting with '-'
/// takes the next argument as its value).
const CURL_FLAGS: &[&str] = &["-s", "-S", "-f", "-L", "-k", "-sS", "--silent", "--fail", "--location", "--insecure"];

/// The arguments for curl as a config file it reads from stdin (`-K -`):
/// URLs with tokens, auth headers and form fields stay out of the process
/// list, where any local user can read a command line.
pub fn curl_config(args: &[String]) -> String {
    let quote = |v: &str| {
        let mut q = String::with_capacity(v.len() + 2);
        q.push('"');
        for c in v.chars() {
            match c {
                '\\' => q.push_str("\\\\"),
                '"' => q.push_str("\\\""),
                '\n' => q.push_str("\\n"),
                '\r' => q.push_str("\\r"),
                '\t' => q.push_str("\\t"),
                c => q.push(c),
            }
        }
        q.push('"');
        q
    };
    let mut out = String::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with('-') && a.len() > 1 {
            if CURL_FLAGS.contains(&a.as_str()) || i + 1 >= args.len() {
                out.push_str(a);
            } else {
                out.push_str(&format!("{a} {}", quote(&args[i + 1])));
                i += 1;
            }
        } else {
            out.push_str(&format!("url = {}", quote(a)));
        }
        out.push('\n');
        i += 1;
    }
    out
}

/// Whether curl can be run here. Alerts, the AI server option and a few more
/// send with it. Once found it is not looked for again; while missing, it is
/// looked for again at most once a minute (so installing it clears the warning).
pub fn curl_available() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static FOUND: AtomicBool = AtomicBool::new(false);
    static LAST_MISSING: Mutex<Option<Instant>> = Mutex::new(None);
    if FOUND.load(Ordering::Relaxed) {
        return true;
    }
    if LAST_MISSING.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
        return false;
    }
    match Command::new("curl").arg("--version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status() {
        Ok(s) if s.success() => {
            FOUND.store(true, Ordering::Relaxed);
            true
        }
        // Only "no such program" (or one that fails) counts as missing; a
        // start that failed for another reason (out of processes or files for
        // a moment) says nothing either way, and it is looked for again.
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => true,
        _ => {
            *LAST_MISSING.lock().unwrap() = Some(Instant::now());
            false
        }
    }
}

/// Run curl with `args` passed on stdin (see [`curl_config`]); stdout is
/// captured, stderr dropped.
pub fn curl(args: &[String]) -> std::io::Result<std::process::Output> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("curl").args(["-K", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(curl_config(args).as_bytes())?;
    }
    child.wait_with_output()
}

#[cfg(test)]
mod curl_tests {
    use super::*;

    #[test]
    fn curl_config_quotes_and_pairs_options() {
        let args: Vec<String> = ["-s", "-m", "10", "-H", "Authorization: Bearer t\"x", "--form-string", "message=a\nb", "https://api.example/bot123:ABC/send"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            curl_config(&args),
            "-s\n-m \"10\"\n-H \"Authorization: Bearer t\\\"x\"\n--form-string \"message=a\\nb\"\nurl = \"https://api.example/bot123:ABC/send\"\n"
        );
    }
}

#[cfg(all(test, unix))]
mod signal_tests {
    use super::ended_by_signal;
    use std::process::Command;

    fn exit_of(script: &str) -> std::process::ExitStatus {
        Command::new("sh").args(["-c", script]).status().unwrap()
    }

    #[test]
    fn a_stop_signal_is_told_from_a_failure() {
        // ffmpeg's own code after catching SIGTERM/SIGINT.
        assert!(ended_by_signal(&exit_of("exit 255")));
        assert!(!ended_by_signal(&exit_of("exit 1")));
        assert!(!ended_by_signal(&exit_of("exit 0")));
        // A program that does not catch it.
        assert!(ended_by_signal(&exit_of("kill -TERM $$")));
        // AmberSight's own kill of a stuck decoder (or the system out of memory) is not.
        assert!(!ended_by_signal(&exit_of("kill -KILL $$")));
    }

    #[test]
    fn ffmpeg_sent_sigterm_counts_as_stopped_from_outside() {
        let Ok(mut c) = Command::new(crate::ffmpeg_bin::program())
            .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc=size=64x64:rate=10", "-f", "null", "-"])
            .stdin(std::process::Stdio::null())
            .spawn()
        else {
            return; // (no ffmpeg here)
        };
        std::thread::sleep(std::time::Duration::from_millis(700));
        unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
        let st = c.wait().unwrap();
        assert!(ended_by_signal(&st), "{st}");
    }
}
