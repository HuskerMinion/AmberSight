//! Running AmberSight as a background service: systemd (Linux), launchd
//! (macOS) and the Windows Service Control Manager.
//!
//! The unit/plist/command-line generators are plain functions without OS
//! gates so they can be unit-tested everywhere; the code that talks to the
//! service manager lives in one module per operating system.

// The text generators are compiled on every OS but only used on some.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};

/// Selects which installed service a command refers to.
#[derive(Args, Clone, Debug)]
pub struct Target {
    /// Linux/macOS: use the system-wide service instead of the per-user one
    #[arg(long)]
    pub system: bool,
    /// Alternate service name (for testing)
    #[arg(long, hide = true)]
    pub name: Option<String>,
}

#[derive(Subcommand, Clone, Debug)]
pub enum ServiceCmd {
    /// Install and start AmberSight as a background service
    Install {
        /// Config file the service should run with
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[command(flatten)]
        target: Target,
    },
    /// Stop and remove the service
    Uninstall {
        #[command(flatten)]
        target: Target,
    },
    /// Show whether the service is installed and running, and where its logs are
    Status {
        #[command(flatten)]
        target: Target,
    },
    /// Start the installed service
    Start {
        #[command(flatten)]
        target: Target,
    },
    /// Stop the installed service
    Stop {
        #[command(flatten)]
        target: Target,
    },
}

pub fn handle(cmd: ServiceCmd) -> Result<()> {
    match cmd {
        ServiceCmd::Install { config, target } => {
            let spec = ServiceSpec::new(&config, &target)?;
            // Windows: the program and data folders private to administrators
            // before the service starts (the data folder only when it is new
            // or empty; the service closes a full one itself).
            if cfg!(windows) {
                if let Ok(cfg) = crate::config::Config::load(&spec.config.to_string_lossy()) {
                    let data = if cfg.storage.data_dir.is_absolute() { cfg.storage.data_dir.clone() } else { spec.work_dir.join(&cfg.storage.data_dir) };
                    if let Some(dir) = spec.exe.parent() {
                        for note in crate::lockdown::at_install(dir, &data) {
                            println!("{note}");
                        }
                    }
                }
            }
            sys::install(&spec)
        }
        ServiceCmd::Uninstall { target } => sys::uninstall(&target),
        ServiceCmd::Status { target } => sys::status(&target),
        ServiceCmd::Start { target } => sys::start(&target),
        ServiceCmd::Stop { target } => sys::stop(&target),
    }
}

/// Install (and start) the service described by `spec` (used by `setup`).
pub fn install(spec: &ServiceSpec) -> Result<()> {
    sys::install(spec)
}

/// Stop and remove the service (used by `setup`).
pub fn uninstall(target: &Target) -> Result<()> {
    sys::uninstall(target)
}

/// Whether this exact service (user or system-wide as `target` says) is installed.
pub fn is_installed(target: &Target) -> bool {
    sys::is_installed(target)
}

/// Everything an installer needs to know, with all paths absolute.
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub exe: PathBuf,
    pub config: PathBuf,
    /// Directory of the config file; the service runs here so relative
    /// paths in the config (e.g. `data_dir = "amberdata"`) keep working.
    pub work_dir: PathBuf,
    pub system: bool,
    pub name: Option<String>,
    pub path_env: String,
}

impl ServiceSpec {
    fn new(config: &str, target: &Target) -> Result<Self> {
        let exe = std::env::current_exe().context("could not determine the ambersight executable path")?;
        let config = std::path::absolute(config).with_context(|| format!("bad config path {config}"))?;
        if !config.is_file() {
            bail!(
                "config file {} does not exist (create one with `ambersight init {}`)",
                config.display(),
                config.display()
            );
        }
        let mut spec = Self::for_program(exe, config, target.system);
        spec.name = target.name.clone();
        Ok(spec)
    }

    /// The service for a given program and (absolute) config file.
    pub fn for_program(exe: PathBuf, config: PathBuf, system: bool) -> Self {
        let work_dir = config.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        Self { exe, config, work_dir, system, name: None, path_env: std::env::var("PATH").unwrap_or_default() }
    }
}

fn run_cmd(prog: &str, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new(prog)
        .args(args)
        .output()
        .with_context(|| format!("could not run `{prog}`"))
}

/// Run a command, failing with its stderr when it exits non-zero.
fn run_ok(prog: &str, args: &[&str]) -> Result<String> {
    let out = run_cmd(prog, args)?;
    if !out.status.success() {
        bail!(
            "`{prog} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(unix)]
fn unix_id(flag: &str) -> Option<String> {
    run_cmd("id", &[flag])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(unix)]
pub(crate) fn is_root() -> bool {
    unix_id("-u").as_deref() == Some("0")
}

/// The login the service should run as (honours `sudo`).
#[cfg(unix)]
pub(crate) fn login_user() -> Option<String> {
    if is_root() {
        if let Ok(u) = std::env::var("SUDO_USER") {
            if !u.is_empty() && u != "root" {
                return Some(u);
            }
        }
    }
    std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(|| unix_id("-un"))
}

// ---------------------------------------------------------------------
// systemd
// ---------------------------------------------------------------------

/// Quote one word for a systemd unit file value (ExecStart, Environment).
fn systemd_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Text of the systemd unit. `user` is only set for system-wide units.
fn systemd_unit(spec: &ServiceSpec, user: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str("[Unit]\nDescription=AmberSight NVR\n");
    if spec.system {
        s.push_str("After=network-online.target\nWants=network-online.target\n");
    }
    s.push_str("\n[Service]\nType=simple\n");
    s.push_str(&format!(
        "ExecStart={} run --config {}\n",
        systemd_quote(&spec.exe.to_string_lossy()),
        systemd_quote(&spec.config.to_string_lossy())
    ));
    s.push_str(&format!("WorkingDirectory={}\n", spec.work_dir.display()));
    s.push_str("Restart=on-failure\nRestartSec=5\n");
    s.push_str("Environment=RUST_LOG=info\n");
    // ffmpeg must be found on the service's PATH.
    s.push_str(&format!("Environment={}\n", systemd_quote(&format!("PATH={}", spec.path_env))));
    if let Some(u) = user {
        s.push_str(&format!("User={u}\n"));
    }
    s.push_str(&format!(
        "\n[Install]\nWantedBy={}\n",
        if spec.system { "multi-user.target" } else { "default.target" }
    ));
    s
}

// ---------------------------------------------------------------------
// launchd
// ---------------------------------------------------------------------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn plist_label(name: Option<&str>) -> String {
    format!("com.ambersight.{}", name.unwrap_or("server"))
}

/// Text of the launchd property list. `user` is only set for daemons.
fn launchd_plist(spec: &ServiceSpec, label: &str, user: Option<&str>) -> String {
    let e = |p: &Path| xml_escape(&p.to_string_lossy());
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    s.push_str("<plist version=\"1.0\">\n<dict>\n");
    s.push_str(&format!("\t<key>Label</key>\n\t<string>{}</string>\n", xml_escape(label)));
    s.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    s.push_str(&format!("\t\t<string>{}</string>\n", e(&spec.exe)));
    s.push_str("\t\t<string>run</string>\n\t\t<string>--config</string>\n");
    s.push_str(&format!("\t\t<string>{}</string>\n\t</array>\n", e(&spec.config)));
    s.push_str(&format!("\t<key>WorkingDirectory</key>\n\t<string>{}</string>\n", e(&spec.work_dir)));
    s.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n");
    s.push_str(&format!(
        "\t<key>StandardOutPath</key>\n\t<string>{}</string>\n",
        e(&spec.work_dir.join("ambersight.out.log"))
    ));
    s.push_str(&format!(
        "\t<key>StandardErrorPath</key>\n\t<string>{}</string>\n",
        e(&spec.work_dir.join("ambersight.err.log"))
    ));
    s.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    s.push_str(&format!("\t\t<key>PATH</key>\n\t\t<string>{}</string>\n", xml_escape(&spec.path_env)));
    s.push_str("\t\t<key>RUST_LOG</key>\n\t\t<string>info</string>\n\t</dict>\n");
    if let Some(u) = user {
        s.push_str(&format!("\t<key>UserName</key>\n\t<string>{}</string>\n", xml_escape(u)));
    }
    s.push_str("</dict>\n</plist>\n");
    s
}

// ---------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------

/// Quote one argument the way the Windows C runtime parses command lines.
pub(crate) fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Arguments (after the executable) the service control manager launches us with.
///
/// `ffmpeg_dir` is the folder holding ffmpeg as seen by the installing user;
/// the service account usually has a shorter PATH, so it is prepended there.
fn windows_launch_args(config: &Path, name: Option<&str>, ffmpeg_dir: Option<&Path>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["service-run".into(), "--config".into(), config.as_os_str().to_owned()];
    if let Some(n) = name {
        args.push("--name".into());
        args.push(n.into());
    }
    if let Some(d) = ffmpeg_dir {
        args.push("--path-prepend".into());
        args.push(d.as_os_str().to_owned());
    }
    args
}

/// Directory of the first `ffmpeg` executable found on `path_env`.
pub(crate) fn find_ffmpeg_dir(path_env: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    std::env::split_paths(path_env).find(|d| d.join(exe).is_file())
}

/// Full command line as it is registered with the service manager.
fn windows_command_line(exe: &Path, args: &[OsString]) -> String {
    let mut parts = vec![windows_quote(&exe.to_string_lossy())];
    parts.extend(args.iter().map(|a| windows_quote(&a.to_string_lossy())));
    parts.join(" ")
}

// ---------------------------------------------------------------------
// Logging for the service process
// ---------------------------------------------------------------------

/// Set up tracing. Under the Windows service manager there is no console,
/// so `service-run` logs to `ambersight.log` next to the config file.
pub fn init_logging() {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;
    let args: Vec<String> = std::env::args().collect();
    if cfg!(windows) && args.get(1).map(String::as_str) == Some("service-run") {
        let config = args.iter().position(|a| a == "--config").and_then(|i| args.get(i + 1));
        if let Some(config) = config {
            let dir = Path::new(config).parent().map(Path::to_path_buf).unwrap_or_default();
            let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("ambersight.log"));
            if let Ok(file) = file {
                let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
                tracing_subscriber::registry()
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_ansi(false)
                            .with_writer(std::sync::Mutex::new(file))
                            .with_filter(filter),
                    )
                    .with(crate::logbuf::RingLayer)
                    .init();
                std::panic::set_hook(Box::new(|info| tracing::error!("panic: {info}")));
                return;
            }
        }
    }
    // The capture helper started at sign-in has no console: log to a file
    // next to its settings (capture.log, trimmed when over 1 MB).
    if args.get(1).map(String::as_str) == Some("capture") && !args.iter().any(|a| matches!(a.as_str(), "--pair" | "--unpair" | "--list")) {
        if let Some(dir) = crate::capture::config::default_path().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            let path = dir.join("capture.log");
            if std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
                let _ = std::fs::rename(&path, dir.join("capture.old.log"));
            }
            if let Ok(file) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(&path)) {
                let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
                tracing_subscriber::registry()
                    .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(file)).with_filter(filter))
                    .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env()))
                    .init();
                // Started hidden there is nowhere else a panic message would show.
                let default = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    tracing::error!("panic: {info}");
                    default(info);
                }));
                return;
            }
        }
    }
    // Console output follows RUST_LOG; the web log viewer always keeps the
    // recent INFO/WARN/ERROR lines.
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env()))
        .with(crate::logbuf::RingLayer)
        .init();
}

/// Entry point of the hidden `service-run` command.
pub fn service_run(config: String, name: Option<String>, path_prepend: Option<String>) -> Result<()> {
    #[cfg(windows)]
    {
        windows::dispatch(config, name, path_prepend)
    }
    #[cfg(not(windows))]
    {
        let _ = (config, name, path_prepend);
        bail!("`service-run` is the Windows service entry point; on this system use `ambersight service install` (systemd/launchd) or `ambersight run`")
    }
}

// ---------------------------------------------------------------------
// Linux backend
// ---------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod sys {
    use super::*;

    const DEFAULT_NAME: &str = "ambersight";

    fn unit_name(t: Option<&str>) -> String {
        t.unwrap_or(DEFAULT_NAME).to_string()
    }

    fn user_unit_path(name: &str) -> Result<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(x) => PathBuf::from(x),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
        };
        Ok(base.join("systemd/user").join(format!("{name}.service")))
    }

    fn system_unit_path(name: &str) -> PathBuf {
        PathBuf::from(format!("/etc/systemd/system/{name}.service"))
    }

    /// Whether to address the system-wide unit: explicit flag, else whichever is installed.
    fn use_system(t: &Target, name: &str) -> bool {
        if t.system {
            return true;
        }
        let user_exists = user_unit_path(name).map(|p| p.exists()).unwrap_or(false);
        !user_exists && system_unit_path(name).exists()
    }

    fn ctl(system: bool, args: &[&str]) -> Result<String> {
        let mut full: Vec<&str> = Vec::new();
        if !system {
            full.push("--user");
        }
        full.extend_from_slice(args);
        run_ok("systemctl", &full)
    }

    fn sudo_hint(verb: &str, spec_config: Option<&Path>, name: Option<&str>) -> Result<String> {
        let mut cmd = format!("sudo {} service {verb}", std::env::current_exe()?.display());
        if let Some(c) = spec_config {
            cmd.push_str(&format!(" --config {}", c.display()));
        }
        cmd.push_str(" --system");
        if let Some(n) = name {
            cmd.push_str(&format!(" --name {n}"));
        }
        Ok(cmd)
    }

    pub fn install(spec: &ServiceSpec) -> Result<()> {
        let name = unit_name(spec.name.as_deref());
        if spec.system && !is_root() {
            let cmd = sudo_hint("install", Some(&spec.config), spec.name.as_deref())?;
            bail!("installing a system-wide service needs root. Run:\n\n  {cmd}\n\nor omit --system to install a per-user service that needs no root.");
        }
        let (path, user) = if spec.system {
            (system_unit_path(&name), Some(login_user().context("could not determine the current user")?))
        } else {
            (user_unit_path(&name)?, None)
        };
        let dir = path.parent().expect("unit path has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
        std::fs::write(&path, systemd_unit(spec, user.as_deref()))
            .with_context(|| format!("could not write {}", path.display()))?;
        println!("wrote {}", path.display());
        ctl(spec.system, &["daemon-reload"])?;
        ctl(spec.system, &["enable", "--now", &name])?;
        println!("AmberSight service '{name}' installed, enabled and started.");
        println!("  config:  {}", spec.config.display());
        println!("  workdir: {}", spec.work_dir.display());
        if spec.system {
            println!("  logs:    journalctl -u {name} -f");
        } else {
            println!("  logs:    journalctl --user -u {name} -f");
            let who = login_user().unwrap_or_else(|| "$USER".into());
            // Only when it is not on yet (`ambersight setup` turns it on or says so).
            if !std::path::Path::new("/var/lib/systemd/linger").join(&who).exists() {
                println!("\nTo keep it running at boot without logging in, run once:\n  loginctl enable-linger {who}");
            }
        }
        Ok(())
    }

    pub fn uninstall(t: &Target) -> Result<()> {
        let name = unit_name(t.name.as_deref());
        let system = use_system(t, &name);
        let path = if system { system_unit_path(&name) } else { user_unit_path(&name)? };
        if !path.exists() {
            println!("service '{name}' is not installed ({} does not exist)", path.display());
            return Ok(());
        }
        if system && !is_root() {
            bail!("removing a system-wide service needs root. Run:\n\n  {}", sudo_hint("uninstall", None, t.name.as_deref())?);
        }
        // Disabling may fail if it was never enabled; the file removal matters.
        if let Err(e) = ctl(system, &["disable", "--now", &name]) {
            eprintln!("warning: {e:#}");
        }
        std::fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
        ctl(system, &["daemon-reload"])?;
        let _ = ctl(system, &["reset-failed", &name]);
        println!("removed service '{name}' ({})", path.display());
        Ok(())
    }

    pub fn is_installed(t: &Target) -> bool {
        let name = unit_name(t.name.as_deref());
        if t.system {
            system_unit_path(&name).exists()
        } else {
            user_unit_path(&name).map(|p| p.exists()).unwrap_or(false)
        }
    }

    pub fn status(t: &Target) -> Result<()> {
        let name = unit_name(t.name.as_deref());
        let system = use_system(t, &name);
        let path = if system { system_unit_path(&name) } else { user_unit_path(&name)? };
        if !path.exists() {
            println!("AmberSight service '{name}': not installed");
            println!("  install with: ambersight service install --config config.toml");
            return Ok(());
        }
        let state = |verb: &str| {
            let args: Vec<&str> = if system { vec![verb, &name] } else { vec!["--user", verb, &name] };
            run_cmd("systemctl", &args)
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|_| "unknown".into())
        };
        println!("AmberSight service '{name}': installed ({} unit)", if system { "system" } else { "user" });
        println!("  unit file: {}", path.display());
        println!("  running:   {}", state("is-active"));
        println!("  enabled:   {}", state("is-enabled"));
        println!("  logs:      journalctl {}-u {name} -f", if system { "" } else { "--user " });
        Ok(())
    }

    pub fn start(t: &Target) -> Result<()> {
        let name = unit_name(t.name.as_deref());
        ctl(use_system(t, &name), &["start", &name])?;
        println!("started '{name}'");
        Ok(())
    }

    pub fn stop(t: &Target) -> Result<()> {
        let name = unit_name(t.name.as_deref());
        ctl(use_system(t, &name), &["stop", &name])?;
        println!("stopped '{name}'");
        Ok(())
    }
}

// ---------------------------------------------------------------------
// macOS backend
// ---------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod sys {
    use super::*;

    fn plist_path(label: &str, system: bool) -> Result<PathBuf> {
        let file = format!("{label}.plist");
        Ok(if system {
            PathBuf::from("/Library/LaunchDaemons").join(file)
        } else {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join("Library/LaunchAgents")
                .join(file)
        })
    }

    fn domain(system: bool) -> Result<String> {
        if system {
            Ok("system".into())
        } else {
            Ok(format!("gui/{}", unix_id("-u").context("could not determine your user id")?))
        }
    }

    fn use_system(t: &Target, label: &str) -> bool {
        if t.system {
            return true;
        }
        let user_exists = plist_path(label, false).map(|p| p.exists()).unwrap_or(false);
        !user_exists && plist_path(label, true).map(|p| p.exists()).unwrap_or(false)
    }

    fn loaded(domain: &str, label: &str) -> bool {
        run_cmd("launchctl", &["print", &format!("{domain}/{label}")])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn install(spec: &ServiceSpec) -> Result<()> {
        let label = plist_label(spec.name.as_deref());
        if spec.system && !is_root() {
            bail!(
                "installing a system-wide service needs root. Run:\n\n  sudo {} service install --config {} --system",
                spec.exe.display(),
                spec.config.display()
            );
        }
        let path = plist_path(&label, spec.system)?;
        let user = if spec.system { login_user() } else { None };
        std::fs::create_dir_all(path.parent().expect("plist path has a parent"))?;
        std::fs::write(&path, launchd_plist(spec, &label, user.as_deref()))
            .with_context(|| format!("could not write {}", path.display()))?;
        println!("wrote {}", path.display());
        let dom = domain(spec.system)?;
        if loaded(&dom, &label) {
            let _ = run_cmd("launchctl", &["bootout", &format!("{dom}/{label}")]);
        }
        run_ok("launchctl", &["bootstrap", &dom, &path.to_string_lossy()])?;
        println!("AmberSight service '{label}' installed and started.");
        println!("  config:  {}", spec.config.display());
        println!("  workdir: {}", spec.work_dir.display());
        println!("  logs:    {}", spec.work_dir.join("ambersight.out.log").display());
        println!("           {}", spec.work_dir.join("ambersight.err.log").display());
        Ok(())
    }

    pub fn uninstall(t: &Target) -> Result<()> {
        let label = plist_label(t.name.as_deref());
        let system = use_system(t, &label);
        let path = plist_path(&label, system)?;
        let dom = domain(system)?;
        if loaded(&dom, &label) {
            run_ok("launchctl", &["bootout", &format!("{dom}/{label}")])?;
        }
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
            println!("removed service '{label}' ({})", path.display());
        } else {
            println!("service '{label}' is not installed");
        }
        Ok(())
    }

    pub fn is_installed(t: &Target) -> bool {
        plist_path(&plist_label(t.name.as_deref()), t.system).map(|p| p.exists()).unwrap_or(false)
    }

    pub fn status(t: &Target) -> Result<()> {
        let label = plist_label(t.name.as_deref());
        let system = use_system(t, &label);
        let path = plist_path(&label, system)?;
        if !path.exists() {
            println!("AmberSight service '{label}': not installed");
            return Ok(());
        }
        let dom = domain(system)?;
        let out = run_cmd("launchctl", &["print", &format!("{dom}/{label}")])?;
        let text = String::from_utf8_lossy(&out.stdout);
        let state = if !out.status.success() {
            "not loaded".to_string()
        } else {
            text.lines()
                .find_map(|l| l.trim().strip_prefix("state = "))
                .unwrap_or("loaded")
                .to_string()
        };
        println!("AmberSight service '{label}': installed ({} plist)", if system { "system" } else { "user" });
        println!("  plist:   {}", path.display());
        println!("  running: {state}");
        let log = std::fs::read_to_string(&path).ok().and_then(|p| {
            p.lines()
                .skip_while(|l| !l.contains("StandardOutPath"))
                .nth(1)
                .map(|l| l.trim().trim_start_matches("<string>").trim_end_matches("</string>").to_string())
        });
        if let Some(log) = log {
            println!("  logs:    {log}");
        }
        Ok(())
    }

    pub fn start(t: &Target) -> Result<()> {
        let label = plist_label(t.name.as_deref());
        let system = use_system(t, &label);
        let path = plist_path(&label, system)?;
        if !path.exists() {
            bail!("service '{label}' is not installed");
        }
        let dom = domain(system)?;
        if loaded(&dom, &label) {
            run_ok("launchctl", &["kickstart", &format!("{dom}/{label}")])?;
        } else {
            run_ok("launchctl", &["bootstrap", &dom, &path.to_string_lossy()])?;
        }
        println!("started '{label}'");
        Ok(())
    }

    /// With `KeepAlive` launchd would restart a killed job, so stopping
    /// unloads it; `service start` loads it again.
    pub fn stop(t: &Target) -> Result<()> {
        let label = plist_label(t.name.as_deref());
        let system = use_system(t, &label);
        let dom = domain(system)?;
        if !loaded(&dom, &label) {
            println!("'{label}' is not running");
            return Ok(());
        }
        run_ok("launchctl", &["bootout", &format!("{dom}/{label}")])?;
        println!("stopped '{label}' (use `ambersight service start` to run it again)");
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Windows backend
// ---------------------------------------------------------------------

#[cfg(windows)]
mod sys {
    use super::windows::*;
    use super::*;
    use std::time::Duration;
    use windows_service::service::{
        Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
        ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    fn manager(access: ServiceManagerAccess) -> Result<ServiceManager> {
        ServiceManager::local_computer(None::<&str>, access)
            .map_err(|e| win_err("could not connect to the service manager", e))
    }

    /// Open an existing service; `Ok(None)` when it is not installed.
    fn open(name: &str, access: ServiceAccess) -> Result<Option<Service>> {
        let mgr = manager(ServiceManagerAccess::CONNECT)?;
        match mgr.open_service(name, access) {
            Ok(s) => Ok(Some(s)),
            Err(e) if win_code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => Ok(None),
            Err(e) => Err(win_err(&format!("could not open service '{name}'"), e)),
        }
    }

    fn wait_for(svc: &Service, want: ServiceState, secs: u64) -> Result<bool> {
        for _ in 0..secs * 4 {
            let st = svc.query_status().map_err(|e| win_err("could not query service status", e))?;
            if st.current_state == want {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(false)
    }

    pub fn install(spec: &ServiceSpec) -> Result<()> {
        let name = svc_name(spec.name.as_deref());
        let mgr = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
        let info = ServiceInfo {
            name: name.clone().into(),
            display_name: "AmberSight NVR".into(),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: spec.exe.clone(),
            launch_arguments: windows_launch_args(&spec.config, spec.name.as_deref(), find_ffmpeg_dir(&spec.path_env).as_deref()),
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };
        println!("registering: {}", windows_command_line(&info.executable_path, &info.launch_arguments));
        let service = mgr
            .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS)
            .map_err(|e| win_err(&format!("could not create service '{name}'"), e))?;
        let _ = service.set_description("AmberSight self-hosted camera recording server");
        let restart = |secs| ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(secs),
        };
        service
            .update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86400)),
                reboot_msg: None,
                command: None,
                actions: Some(vec![restart(5), restart(5), restart(30)]),
            })
            .map_err(|e| win_err("could not set recovery actions", e))?;
        service
            .set_failure_actions_on_non_crash_failures(true)
            .map_err(|e| win_err("could not set recovery actions", e))?;
        service
            .start::<&str>(&[])
            .map_err(|e| win_err("service installed but could not be started", e))?;
        println!("AmberSight service '{name}' installed (starts automatically at boot) and started.");
        println!("  config: {}", spec.config.display());
        println!("  logs:   {}", spec.work_dir.join("ambersight.log").display());
        Ok(())
    }

    pub fn uninstall(t: &Target) -> Result<()> {
        let name = svc_name(t.name.as_deref());
        let Some(svc) = open(&name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE)? else {
            println!("service '{name}' is not installed");
            return Ok(());
        };
        let st = svc.query_status().map_err(|e| win_err("could not query service status", e))?;
        if st.current_state != ServiceState::Stopped {
            let _ = svc.stop();
            if !wait_for(&svc, ServiceState::Stopped, 20)? {
                eprintln!("warning: service did not stop within 20 s; it will be removed once it exits");
            }
        }
        svc.delete().map_err(|e| win_err(&format!("could not delete service '{name}'"), e))?;
        println!("removed service '{name}'");
        Ok(())
    }

    pub fn is_installed(t: &Target) -> bool {
        matches!(open(&svc_name(t.name.as_deref()), ServiceAccess::QUERY_STATUS), Ok(Some(_)))
    }

    pub fn status(t: &Target) -> Result<()> {
        let name = svc_name(t.name.as_deref());
        let Some(svc) = open(&name, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG)? else {
            println!("AmberSight service '{name}': not installed");
            println!("  install with (Administrator terminal): ambersight service install --config config.toml");
            return Ok(());
        };
        let st = svc.query_status().map_err(|e| win_err("could not query service status", e))?;
        println!("AmberSight service '{name}': installed");
        println!("  running: {:?}", st.current_state);
        if let Ok(cfg) = svc.query_config() {
            println!("  command: {}", cfg.executable_path.display());
            if let Some(dir) = config_dir_from_command(&cfg.executable_path.to_string_lossy()) {
                println!("  logs:    {}", dir.join("ambersight.log").display());
            }
        }
        Ok(())
    }

    pub fn start(t: &Target) -> Result<()> {
        let name = svc_name(t.name.as_deref());
        let svc = open(&name, ServiceAccess::START)?.with_context(|| format!("service '{name}' is not installed"))?;
        svc.start::<&str>(&[]).map_err(|e| win_err("could not start the service", e))?;
        println!("started '{name}'");
        Ok(())
    }

    pub fn stop(t: &Target) -> Result<()> {
        let name = svc_name(t.name.as_deref());
        let svc = open(&name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?
            .with_context(|| format!("service '{name}' is not installed"))?;
        svc.stop().map_err(|e| win_err("could not stop the service", e))?;
        wait_for(&svc, ServiceState::Stopped, 20)?;
        println!("stopped '{name}'");
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    pub const ERROR_ACCESS_DENIED: i32 = 5;
    pub const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;

    pub fn svc_name(name: Option<&str>) -> String {
        name.unwrap_or("AmberSight").to_string()
    }

    pub fn win_code(e: &windows_service::Error) -> Option<i32> {
        match e {
            windows_service::Error::Winapi(io) => io.raw_os_error(),
            _ => None,
        }
    }

    /// Turn a service-manager error into a readable one; "access denied"
    /// gets the Administrator hint.
    pub fn win_err(what: &str, e: windows_service::Error) -> anyhow::Error {
        if win_code(&e) == Some(ERROR_ACCESS_DENIED) {
            return anyhow::anyhow!(
                "{what}: access denied. Managing Windows services needs elevation - run this command from an Administrator terminal (right-click the terminal, \"Run as administrator\")."
            );
        }
        match std::error::Error::source(&e) {
            Some(src) => anyhow::anyhow!("{what}: {e}: {src}"),
            None => anyhow::anyhow!("{what}: {e}"),
        }
    }

    /// Extract the directory of `--config <path>` from a registered command line.
    pub fn config_dir_from_command(cmd: &str) -> Option<PathBuf> {
        let i = cmd.find("--config")? + "--config".len();
        let rest = cmd[i..].trim_start();
        let path = if let Some(q) = rest.strip_prefix('"') { q.split('"').next()? } else { rest.split(' ').next()? };
        Path::new(path).parent().map(Path::to_path_buf)
    }

    /// (config path, service name, directory to prepend to PATH)
    static RUN: OnceLock<(String, String, Option<String>)> = OnceLock::new();

    windows_service::define_windows_service!(ffi_service_main, service_main);

    pub fn dispatch(config: String, name: Option<String>, path_prepend: Option<String>) -> Result<()> {
        let name = svc_name(name.as_deref());
        let _ = RUN.set((config, name.clone(), path_prepend));
        service_dispatcher::start(name, ffi_service_main).map_err(|e| {
            if win_code(&e) == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) {
                anyhow::anyhow!(
                    "`service-run` can only be started by the Windows service manager, not from a terminal.\n\
                     - to run AmberSight in this terminal, use: ambersight run --config <file>\n\
                     - to install the background service, use (Administrator terminal): ambersight service install --config <file>"
                )
            } else {
                win_err("service dispatcher failed", e)
            }
        })
    }

    fn service_main(_args: Vec<std::ffi::OsString>) {
        let (config, name, prepend) = RUN.get().expect("service parameters are set before dispatch").clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let slot: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>> = Arc::new(Mutex::new(Some(tx)));
        let handler = move |ev: ServiceControl| match ev {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(tx) = slot.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let status_handle = match service_control_handler::register(&name, handler) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("could not register the service control handler: {e}");
                return;
            }
        };
        let status = |state, accept, code, hint: u64| {
            let _ = status_handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: accept,
                exit_code: code,
                checkpoint: 0,
                wait_hint: Duration::from_secs(hint),
                process_id: None,
            });
        };
        status(ServiceState::StartPending, ServiceControlAccept::empty(), ServiceExitCode::Win32(0), 10);
        let result = run(config, prepend, rx, || {
            status(
                ServiceState::Running,
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                ServiceExitCode::Win32(0),
                0,
            )
        });
        status(ServiceState::StopPending, ServiceControlAccept::empty(), ServiceExitCode::Win32(0), 15);
        let code = match result {
            Ok(()) => ServiceExitCode::Win32(0),
            Err(e) => {
                tracing::error!("service stopped with an error: {e:#}");
                ServiceExitCode::ServiceSpecific(1)
            }
        };
        tracing::info!("AmberSight service stopped");
        status(ServiceState::Stopped, ServiceControlAccept::empty(), code, 0);
    }

    fn run(
        config: String,
        prepend: Option<String>,
        stop: tokio::sync::oneshot::Receiver<()>,
        running: impl FnOnce(),
    ) -> Result<()> {
        if let Some(dir) = prepend {
            let old = std::env::var_os("PATH").unwrap_or_default();
            let joined = std::env::join_paths(std::iter::once(PathBuf::from(dir)).chain(std::env::split_paths(&old)))?;
            std::env::set_var("PATH", joined);
        }
        // Services start in System32; run from the config's directory so
        // relative paths in the config resolve as they do in a terminal.
        if let Some(dir) = Path::new(&config).parent().filter(|d| !d.as_os_str().is_empty()) {
            std::env::set_current_dir(dir).with_context(|| format!("could not enter {}", dir.display()))?;
        }
        kill_children_on_exit();
        tracing::info!("AmberSight service starting with config {config}");
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        running();
        let result = rt.block_on(async {
            // The stop request shuts the server down cleanly (TEARDOWN to
            // each camera, recordings finished).
            crate::cli::run_server_until(config, async {
                let _ = stop.await;
                tracing::info!("stop requested");
            })
            .await
        });
        rt.shutdown_timeout(Duration::from_secs(5));
        result
    }

    /// Put this process in a job object that kills its children (ffmpeg)
    /// when the service process exits, however it exits.
    fn kill_children_on_exit() {
        use windows_sys::Win32::System::JobObjects::*;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: plain Win32 calls with valid, correctly sized arguments;
        // the job handle is intentionally leaked so it lives as long as the process.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 {
                return;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 || AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                tracing::warn!("could not create a job object; ffmpeg may outlive the service");
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod sys {
    use super::*;
    const MSG: &str = "running as a service is supported on Linux (systemd), macOS (launchd) and Windows";
    pub fn install(_: &ServiceSpec) -> Result<()> {
        bail!(MSG)
    }
    pub fn uninstall(_: &Target) -> Result<()> {
        bail!(MSG)
    }
    pub fn status(_: &Target) -> Result<()> {
        bail!(MSG)
    }
    pub fn is_installed(_: &Target) -> bool {
        false
    }
    pub fn start(_: &Target) -> Result<()> {
        bail!(MSG)
    }
    pub fn stop(_: &Target) -> Result<()> {
        bail!(MSG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(system: bool) -> ServiceSpec {
        ServiceSpec {
            exe: PathBuf::from("/opt/amber sight/ambersight"),
            config: PathBuf::from("/home/u/cams/config.toml"),
            work_dir: PathBuf::from("/home/u/cams"),
            system,
            name: None,
            path_env: "/usr/local/bin:/usr/bin:/opt/my tools".into(),
        }
    }

    #[test]
    fn systemd_user_unit() {
        let u = systemd_unit(&spec(false), None);
        assert!(u.contains("ExecStart=\"/opt/amber sight/ambersight\" run --config \"/home/u/cams/config.toml\"\n"));
        assert!(u.contains("WorkingDirectory=/home/u/cams\n"));
        assert!(u.contains("Restart=on-failure\nRestartSec=5\n"));
        assert!(u.contains("Environment=RUST_LOG=info\n"));
        assert!(u.contains("Environment=\"PATH=/usr/local/bin:/usr/bin:/opt/my tools\"\n"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(!u.contains("User="));
    }

    #[test]
    fn systemd_system_unit() {
        let u = systemd_unit(&spec(true), Some("alice"));
        assert!(u.contains("User=alice\n"));
        assert!(u.contains("WantedBy=multi-user.target"));
        assert!(u.contains("After=network-online.target"));
    }

    #[test]
    fn systemd_quoting_escapes_specials() {
        assert_eq!(systemd_quote("a%b$c\"d\\e"), "\"a%%b$$c\\\"d\\\\e\"");
    }

    #[test]
    fn launchd_plist_text() {
        let p = launchd_plist(&spec(false), "com.ambersight.server", None);
        assert!(p.contains("<key>Label</key>\n\t<string>com.ambersight.server</string>"));
        assert!(p.contains("<string>/opt/amber sight/ambersight</string>"));
        assert!(p.contains("<string>/home/u/cams/config.toml</string>"));
        assert!(p.contains("<key>WorkingDirectory</key>\n\t<string>/home/u/cams</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(p.contains("<key>KeepAlive</key>\n\t<true/>"));
        let log = |n: &str| PathBuf::from("/home/u/cams").join(n).display().to_string();
        assert!(p.contains(&log("ambersight.out.log")));
        assert!(p.contains(&log("ambersight.err.log")));
        assert!(p.contains("<key>PATH</key>\n\t\t<string>/usr/local/bin:/usr/bin:/opt/my tools</string>"));
        assert!(!p.contains("UserName"));
        assert!(launchd_plist(&spec(true), "x", Some("bob")).contains("<key>UserName</key>\n\t<string>bob</string>"));
    }

    #[test]
    fn plist_escapes_xml() {
        let mut s = spec(false);
        s.path_env = "/a&b:/c<d".into();
        assert!(launchd_plist(&s, "l", None).contains("/a&amp;b:/c&lt;d"));
        assert_eq!(plist_label(Some("test")), "com.ambersight.test");
        assert_eq!(plist_label(None), "com.ambersight.server");
    }

    #[test]
    fn windows_quoting() {
        assert_eq!(windows_quote(r"C:\plain\a.exe"), r"C:\plain\a.exe");
        assert_eq!(windows_quote(r"C:\Program Files\AmberSight\a.exe"), r#""C:\Program Files\AmberSight\a.exe""#);
        assert_eq!(windows_quote(r"C:\My Cams\"), r#""C:\My Cams\\""#);
        assert_eq!(windows_quote(r#"a "b" c"#), r#""a \"b\" c""#);
        assert_eq!(windows_quote(""), "\"\"");
    }

    #[test]
    fn windows_command_line_with_spaces() {
        let args = windows_launch_args(Path::new(r"C:\My Cams\config.toml"), Some("ambersight-test"), None);
        assert_eq!(args, vec!["service-run", "--config", r"C:\My Cams\config.toml", "--name", "ambersight-test"]);
        let line = windows_command_line(Path::new(r"C:\Program Files\AmberSight\ambersight.exe"), &args);
        assert_eq!(
            line,
            r#""C:\Program Files\AmberSight\ambersight.exe" service-run --config "C:\My Cams\config.toml" --name ambersight-test"#
        );
        assert_eq!(windows_launch_args(Path::new(r"C:\c.toml"), None, None).len(), 3);
        let with_path = windows_launch_args(Path::new(r"C:\c.toml"), None, Some(Path::new(r"C:\Program Files\ffmpeg\bin")));
        assert_eq!(
            windows_command_line(Path::new(r"C:\a.exe"), &with_path),
            r#"C:\a.exe service-run --config C:\c.toml --path-prepend "C:\Program Files\ffmpeg\bin""#
        );
    }
}
