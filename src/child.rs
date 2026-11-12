//! Child processes (ffmpeg) must never outlive AmberSight, even when it is
//! killed abruptly: on Linux the kernel kills them when the parent dies; on
//! Windows they are placed in a job object that closes with the process.

use std::process::{Child, Command};

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
