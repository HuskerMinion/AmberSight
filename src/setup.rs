//! `ambersight setup`: one command that leaves a working, auto-starting
//! install on Windows, macOS and Linux.
//!
//! It decides where the program, the settings (config.toml) and the
//! recordings go, copies the program there, writes a starter config.toml
//! when there is none, installs the background service (service.rs), opens
//! the firewall port, adds an "AmberSight" shortcut that opens the web page,
//! and opens the page. `setup --uninstall` takes all of that away again but
//! keeps the settings and recordings. Running setup again updates the
//! program copy and the service and keeps the settings.
//!
//! The decisions (folders, file contents, command lines) are plain functions
//! without OS gates so they can be unit-tested everywhere; the code that
//! changes the computer is at the bottom.

// The text generators are compiled on every OS but only used on some.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::service::{self, ServiceSpec, Target};

/// The starter settings, as `ambersight init` writes them.
pub const STARTER_CONFIG: &str = include_str!("../config.example.toml");

/// Icons for the shortcuts (static/icons; the .ico and .icns are made from
/// icon-512.png by the ignored test `make_icon_files`).
const ICON_PNG: &[u8] = include_bytes!("../static/icons/icon-512.png");
const ICON_SVG: &[u8] = include_bytes!("../static/icons/icon.svg");
const ICON_ICO: &[u8] = include_bytes!("../static/icons/ambersight.ico");
const ICON_ICNS: &[u8] = include_bytes!("../static/icons/ambersight.icns");

#[derive(Args, Clone, Debug, Default)]
pub struct SetupArgs {
    /// Linux/macOS: install for the whole computer (runs at boot without
    /// anyone signed in); needs sudo
    #[arg(long)]
    pub system: bool,
    /// Keep the program and config.toml where they are (this folder); only
    /// add the service, shortcut and firewall rule
    #[arg(long, conflicts_with = "installed")]
    pub here: bool,
    /// The program is already in its final folder (the Windows installer
    /// uses this): do not copy or remove it
    #[arg(long, hide = true)]
    pub installed: bool,
    /// Only print what would be done
    #[arg(long)]
    pub dry_run: bool,
    /// Do not open the web page at the end
    #[arg(long)]
    pub no_browser: bool,
    /// Remove the service, firewall rule, shortcuts and program copy; the
    /// settings and recordings are kept
    #[arg(long)]
    pub uninstall: bool,
}

// ---------------------------------------------------------------------
// Where things go
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Windows,
    Mac,
    Linux,
}

impl Os {
    fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }

    fn program_name(self) -> &'static str {
        if self == Os::Windows { "ambersight.exe" } else { "ambersight" }
    }

    /// Libraries that travel with the program (the AI runtime's DLLs on Windows).
    fn library_ext(self) -> &'static str {
        match self {
            Os::Windows => "dll",
            Os::Mac => "dylib",
            Os::Linux => "so",
        }
    }
}

/// What the install layout depends on, read from the environment.
#[derive(Clone, Debug)]
pub struct Env {
    pub os: Os,
    pub home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    /// Windows: %ProgramW6432% (or %ProgramFiles%), %ProgramData%, %PUBLIC%.
    pub program_files: Option<PathBuf>,
    pub program_data: Option<PathBuf>,
    pub public: Option<PathBuf>,
    /// The running program.
    pub exe: PathBuf,
    pub cwd: PathBuf,
}

impl Env {
    fn current() -> Result<Env> {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        Ok(Env {
            os: Os::current(),
            home: var("HOME").or_else(|| var("USERPROFILE")),
            xdg_data_home: var("XDG_DATA_HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            program_files: var("ProgramW6432").or_else(|| var("ProgramFiles")),
            program_data: var("ProgramData"),
            public: var("PUBLIC"),
            exe: std::env::current_exe().context("could not find the running program")?,
            cwd: std::env::current_dir().context("could not read the current folder")?,
        })
    }
}

/// Where setup puts everything.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub os: Os,
    /// Linux/macOS system-wide service (always true on Windows).
    pub system: bool,
    /// Folder the program runs from after setup.
    pub program_dir: PathBuf,
    /// Whether setup copies the program there (and `--uninstall` removes it).
    pub copy_program: bool,
    pub config: PathBuf,
    /// Data folder written into a new config.toml.
    pub data_dir: PathBuf,
    /// Shortcut files: Windows .url files, the macOS app, the Linux .desktop file.
    pub shortcuts: Vec<PathBuf>,
    /// Icon files installed for the shortcuts.
    pub icons: Vec<PathBuf>,
}

impl Layout {
    pub fn program(&self) -> PathBuf {
        join(self.os, &self.program_dir, self.os.program_name())
    }

    fn service_target(&self) -> Target {
        Target { system: self.system && self.os != Os::Windows, name: None }
    }
}

/// Join with the separator of the target system (so the Windows layout can
/// be tested on other systems too).
fn join(os: Os, base: &Path, rest: &str) -> PathBuf {
    if os == Os::Windows {
        let b = base.to_string_lossy();
        PathBuf::from(format!("{}\\{}", b.trim_end_matches(['\\', '/']), rest.replace('/', "\\")))
    } else {
        base.join(rest)
    }
}

/// Folder of a program path, for either kind of separator.
fn parent_dir(os: Os, exe: &Path) -> PathBuf {
    let s = exe.to_string_lossy();
    let cut = if os == Os::Windows { s.rfind(['\\', '/']) } else { s.rfind('/') };
    match cut {
        Some(0) => PathBuf::from("/"),
        Some(i) => PathBuf::from(&s[..i]),
        None => PathBuf::from("."),
    }
}

/// Decide the install layout for this system and these options.
pub fn decide(env: &Env, a: &SetupArgs) -> Result<Layout> {
    let os = env.os;
    let keep_program = a.here || a.installed;
    let exe_dir = parent_dir(os, &env.exe);
    let home = || env.home.clone().context("the HOME environment variable is not set");
    let j = |b: &Path, r: &str| join(os, b, r);
    let layout = match os {
        Os::Windows => {
            let pf = env.program_files.clone().unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
            let pd = env.program_data.clone().unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
            let public = env.public.clone().unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"));
            // "AmberSight NVR": a folder of its own, never one another
            // program called AmberSight might already use.
            let program_dir = if keep_program { exe_dir } else { j(&pf, "AmberSight NVR") };
            let (config, data_dir) = if a.here {
                (j(&env.cwd, "config.toml"), j(&env.cwd, "amberdata"))
            } else {
                (j(&pd, "AmberSight/config.toml"), j(&pd, "AmberSight/data"))
            };
            Layout {
                os,
                system: true,
                icons: vec![j(&program_dir, "ambersight.ico")],
                program_dir,
                copy_program: !keep_program,
                config,
                data_dir,
                shortcuts: vec![
                    j(&pd, "Microsoft/Windows/Start Menu/Programs/AmberSight.url"),
                    j(&public, "Desktop/AmberSight.url"),
                ],
            }
        }
        Os::Mac => {
            let (apps, support) = if a.system {
                (PathBuf::from("/Applications"), PathBuf::from("/Library/Application Support/AmberSight"))
            } else {
                let h = home()?;
                (h.join("Applications"), h.join("Library/Application Support/AmberSight"))
            };
            let program_dir = if keep_program { exe_dir } else { apps.join("AmberSight") };
            let (config, data_dir) = if a.here {
                (env.cwd.join("config.toml"), env.cwd.join("amberdata"))
            } else {
                (support.join("config.toml"), support.join("data"))
            };
            Layout {
                os,
                system: a.system,
                program_dir,
                copy_program: !keep_program,
                config,
                data_dir,
                shortcuts: vec![apps.join("AmberSight.app")],
                icons: vec![],
            }
        }
        Os::Linux => {
            let (program_dir, config_dir, data_dir, share) = if a.system {
                (
                    PathBuf::from("/opt/ambersight"),
                    PathBuf::from("/etc/ambersight"),
                    PathBuf::from("/var/lib/ambersight"),
                    PathBuf::from("/usr/share"),
                )
            } else {
                let h = home()?;
                let data_home = env.xdg_data_home.clone().unwrap_or_else(|| h.join(".local/share"));
                let config_home = env.xdg_config_home.clone().unwrap_or_else(|| h.join(".config"));
                (data_home.join("ambersight"), config_home.join("ambersight"), data_home.join("ambersight/data"), data_home)
            };
            let program_dir = if keep_program { exe_dir } else { program_dir };
            let (config, data_dir) = if a.here {
                (env.cwd.join("config.toml"), env.cwd.join("amberdata"))
            } else {
                (config_dir.join("config.toml"), data_dir)
            };
            Layout {
                os,
                system: a.system,
                program_dir,
                copy_program: !keep_program,
                config,
                data_dir,
                shortcuts: vec![share.join("applications/ambersight.desktop")],
                icons: vec![
                    share.join("icons/hicolor/512x512/apps/ambersight.png"),
                    share.join("icons/hicolor/scalable/apps/ambersight.svg"),
                ],
            }
        }
    };
    Ok(layout)
}

// ---------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------

/// The starter config.toml with `data_dir` set to the given folder.
pub fn starter_config(data_dir: &Path) -> String {
    let quoted = toml::Value::String(data_dir.to_string_lossy().into_owned()).to_string();
    let mut done = false;
    let mut out = String::with_capacity(STARTER_CONFIG.len() + 64);
    for line in STARTER_CONFIG.lines() {
        if !done && line.starts_with("data_dir = ") {
            out.push_str(&format!("data_dir = {quoted}\n"));
            done = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Port and HTTPS setting of a config.toml (defaults when unreadable).
pub fn server_address(config_text: &str) -> (u16, bool) {
    let v: toml::Value = toml::from_str(config_text).unwrap_or(toml::Value::Table(Default::default()));
    let server = v.get("server");
    let port = server
        .and_then(|s| s.get("port"))
        .and_then(|p| p.as_integer())
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(8090);
    let tls = server.and_then(|s| s.get("tls")).and_then(|t| t.as_bool()).unwrap_or(false);
    (port, tls)
}

/// The data folder a config.toml names, relative paths taken from its folder.
fn data_dir_of(config_text: &str, config_dir: &Path) -> PathBuf {
    let v: toml::Value = toml::from_str(config_text).unwrap_or(toml::Value::Table(Default::default()));
    let dir = v
        .get("storage")
        .and_then(|s| s.get("data_dir"))
        .and_then(|d| d.as_str())
        .unwrap_or("amberdata");
    let p = PathBuf::from(dir);
    if p.is_absolute() { p } else { config_dir.join(p) }
}

pub fn local_url(port: u16, tls: bool) -> String {
    format!("{}://localhost:{port}", if tls { "https" } else { "http" })
}

// ---------------------------------------------------------------------
// Firewall
// ---------------------------------------------------------------------

const RULE_NAME: &str = "AmberSight NVR";

/// netsh arguments that remove the AmberSight rule.
pub fn netsh_delete_args() -> Vec<String> {
    ["advfirewall", "firewall", "delete", "rule", &format!("name={RULE_NAME}")].map(String::from).to_vec()
}

/// netsh arguments that allow the web port in.
pub fn netsh_add_args(port: u16) -> Vec<String> {
    [
        "advfirewall".to_string(),
        "firewall".into(),
        "add".into(),
        "rule".into(),
        format!("name={RULE_NAME}"),
        "dir=in".into(),
        "action=allow".into(),
        "protocol=TCP".into(),
        format!("localport={port}"),
    ]
    .to_vec()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinuxFirewall {
    Ufw,
    Firewalld,
}

/// Commands that open the port in a Linux firewall (each run as root).
pub fn linux_firewall_commands(fw: LinuxFirewall, port: u16) -> Vec<Vec<String>> {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match fw {
        LinuxFirewall::Ufw => vec![v(&["ufw", "allow", &format!("{port}/tcp")])],
        LinuxFirewall::Firewalld => vec![
            v(&["firewall-cmd", "--add-port", &format!("{port}/tcp"), "--permanent"]),
            v(&["firewall-cmd", "--reload"]),
        ],
    }
}

/// icacls arguments that make the Windows settings folder private to
/// SYSTEM (the service) and Administrators: config.toml holds the camera
/// passwords and ProgramData is readable by every user by default.
pub fn icacls_private_args(dir: &Path) -> Vec<String> {
    vec![
        dir.to_string_lossy().into_owned(),
        "/inheritance:r".into(),
        "/grant:r".into(),
        "*S-1-5-18:(OI)(CI)F".into(),
        "/grant:r".into(),
        "*S-1-5-32-544:(OI)(CI)F".into(),
    ]
}

// ---------------------------------------------------------------------
// Shortcuts
// ---------------------------------------------------------------------

/// A Windows Internet Shortcut that opens the page with the AmberSight icon.
pub fn windows_url_file(url: &str, icon: &Path) -> String {
    format!("[InternetShortcut]\r\nURL={url}/\r\nIconFile={}\r\nIconIndex=0\r\n", icon.display())
}

/// The Linux desktop entry.
pub fn linux_desktop_file(url: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=AmberSight\nGenericName=Camera recorder\n\
         Comment=Open AmberSight in the web browser\nExec=xdg-open {url}\nIcon=ambersight\n\
         Terminal=false\nCategories=AudioVideo;Video;Network;\nKeywords=camera;nvr;cctv;recorder;\n"
    )
}

/// Info.plist of the macOS AmberSight.app.
pub fn mac_info_plist() -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    s.push_str("<plist version=\"1.0\">\n<dict>\n");
    for (k, v) in [
        ("CFBundleName", "AmberSight"),
        ("CFBundleDisplayName", "AmberSight"),
        ("CFBundleIdentifier", "com.ambersight.open"),
        ("CFBundleExecutable", "AmberSight"),
        ("CFBundleIconFile", "AmberSight"),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleShortVersionString", env!("CARGO_PKG_VERSION")),
    ] {
        s.push_str(&format!("\t<key>{k}</key>\n\t<string>{v}</string>\n"));
    }
    s.push_str("</dict>\n</plist>\n");
    s
}

/// The script the macOS app runs: open the page in the default browser.
pub fn mac_launcher(url: &str) -> String {
    format!("#!/bin/sh\nexec /usr/bin/open \"{url}\"\n")
}

/// One file setup writes for the shortcut.
pub struct ShortcutFile {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

/// Every file of the shortcuts and their icons.
pub fn shortcut_files(l: &Layout, url: &str) -> Vec<ShortcutFile> {
    let f = |path: PathBuf, bytes: Vec<u8>| ShortcutFile { path, bytes, executable: false };
    let mut out = Vec::new();
    match l.os {
        Os::Windows => {
            let ico = l.icons[0].clone();
            for s in &l.shortcuts {
                out.push(f(s.clone(), windows_url_file(url, &ico).into_bytes()));
            }
            out.push(f(ico, ICON_ICO.to_vec()));
        }
        Os::Mac => {
            let app = &l.shortcuts[0];
            out.push(f(app.join("Contents/Info.plist"), mac_info_plist().into_bytes()));
            out.push(ShortcutFile {
                path: app.join("Contents/MacOS/AmberSight"),
                bytes: mac_launcher(url).into_bytes(),
                executable: true,
            });
            out.push(f(app.join("Contents/Resources/AmberSight.icns"), ICON_ICNS.to_vec()));
        }
        Os::Linux => {
            out.push(f(l.shortcuts[0].clone(), linux_desktop_file(url).into_bytes()));
            out.push(f(l.icons[0].clone(), ICON_PNG.to_vec()));
            out.push(f(l.icons[1].clone(), ICON_SVG.to_vec()));
        }
    }
    out
}

// ---------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------

/// What setup will do, printed before it does anything.
fn plan_text(l: &Layout, existing_config: bool, data_dir: &Path, url: &str, port: u16) -> String {
    let mut s = String::new();
    let scope = match (l.os, l.system) {
        (Os::Windows, _) => "for all users",
        (_, true) => "for the whole computer",
        (_, false) => "for this user",
    };
    s.push_str(&format!("AmberSight setup ({scope}):\n"));
    let program = l.program();
    if l.copy_program {
        s.push_str(&format!("  program:   {} (copied there)\n", program.display()));
    } else {
        s.push_str(&format!("  program:   {} (kept where it is)\n", program.display()));
    }
    s.push_str(&format!(
        "  settings:  {} ({})\n",
        l.config.display(),
        if existing_config { "kept as it is" } else { "new, from the starter settings" }
    ));
    s.push_str(&format!("  data:      {}\n", data_dir.display()));
    let service = match (l.os, l.system) {
        (Os::Windows, _) => "Windows service \"AmberSight\", starts at boot".to_string(),
        (Os::Mac, true) => "LaunchDaemon com.ambersight.server, starts at boot".into(),
        (Os::Mac, false) => "LaunchAgent com.ambersight.server, starts when you sign in".into(),
        (Os::Linux, true) => "systemd service \"ambersight\", starts at boot".into(),
        (Os::Linux, false) => "systemd user service \"ambersight\", starts at boot (linger)".into(),
    };
    s.push_str(&format!("  service:   {service}\n"));
    let firewall = match l.os {
        Os::Windows => format!("allow TCP port {port} in (rule \"{RULE_NAME}\")"),
        Os::Mac => "macOS asks the first time AmberSight accepts connections".into(),
        Os::Linux => format!("if ufw or firewalld is on: open TCP port {port}"),
    };
    s.push_str(&format!("  firewall:  {firewall}\n"));
    for sc in &l.shortcuts {
        s.push_str(&format!("  shortcut:  {}\n", sc.display()));
    }
    s.push_str(&format!("  address:   {url}\n"));
    s
}

/// The command line to run setup again, as typed (for "run this as ...").
fn rerun_command(exe: &Path) -> String {
    let mut parts = vec![quote_arg(&exe.to_string_lossy())];
    parts.extend(std::env::args().skip(1).map(|a| quote_arg(&a)));
    parts.join(" ")
}

fn quote_arg(a: &str) -> String {
    if cfg!(windows) {
        service::windows_quote(a)
    } else if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c)) {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

// ---------------------------------------------------------------------
// Doing it
// ---------------------------------------------------------------------

pub fn run(a: SetupArgs) -> Result<()> {
    let env = Env::current()?;
    let layout = decide(&env, &a)?;
    if a.uninstall {
        return uninstall(&env, &layout, &a);
    }
    let existing_text = std::fs::read_to_string(&layout.config).ok();
    let text = existing_text.clone().unwrap_or_else(|| starter_config(&layout.data_dir));
    let config_dir = parent_dir(layout.os, &layout.config);
    let data_dir = data_dir_of(&text, &config_dir);
    let (port, tls) = server_address(&text);
    let url = local_url(port, tls);
    print!("{}", plan_text(&layout, existing_text.is_some(), &data_dir, &url, port));
    if a.dry_run {
        println!("\n(dry run: nothing was changed)");
        return Ok(());
    }
    println!();
    check_privileges(&env, &layout)?;
    if layout.os == Os::Linux && service::find_ffmpeg_dir(&std::env::var("PATH").unwrap_or_default()).is_none() {
        println!("Note: ffmpeg is not installed. AmberSight needs it on Linux and will not start without it:");
        println!("  Debian, Ubuntu, Raspberry Pi OS:  sudo apt install ffmpeg");
        println!("  Fedora (after adding RPM Fusion): sudo dnf install ffmpeg --allowerasing");
        println!("  Arch:                             sudo pacman -S ffmpeg\n");
    }

    // A program folder that holds a program setup did not put there (another
    // program of the same name): stop before changing anything.
    if layout.copy_program {
        if let Some(p) = foreign_program(&env, &layout) {
            anyhow::bail!(
                "{} already holds a program that this setup did not install ({}). Nothing was changed. Move that program elsewhere first, or use `ambersight setup --here` from the folder AmberSight is in.",
                layout.program_dir.display(),
                p.display()
            );
        }
    }
    // An earlier install is stopped and removed first, so its program file
    // can be replaced and the new service starts the new program.
    let target = layout.service_target();
    if service::is_installed(&target) {
        println!("updating the existing install");
        service::uninstall(&target)?;
    }
    if layout.copy_program {
        copy_program(&env, &layout)?;
    }
    if existing_text.is_none() {
        create_config(&layout, &text)?;
    }
    std::fs::create_dir_all(&data_dir).with_context(|| format!("could not create {}", data_dir.display()))?;
    #[cfg(unix)]
    {
        if layout.system {
            give_to_service_user(&[&config_dir, &layout.config, &data_dir])?;
        }
        // As the .deb does: settings private, data folder shut to others, and
        // (a system install, not --here: its settings folder is not the current one)
        // the settings folder too.
        crate::lockdown::harden_unix(&layout.config, &data_dir);
        if layout.system && !a.here {
            crate::lockdown::shut_dir_to_others(&config_dir);
        }
    }
    // Windows: the program folder and the data folder limited to SYSTEM and
    // administrators (a new or empty data folder now; a full one when the
    // service starts, as it can take minutes).
    for note in crate::lockdown::at_install(&layout.program_dir, &data_dir) {
        println!("{note}");
    }

    install_service(&layout)?;
    open_firewall(&layout, port);
    write_shortcuts(&layout, &url)?;
    // The shortcuts' icon lives in the now private program folder: let every
    // account read that one file.
    if layout.os == Os::Windows {
        if let Some(ico) = layout.icons.first() {
            let _ = run_quiet(&crate::lockdown::icacls_exe().to_string_lossy(), &[ico.to_string_lossy().into_owned(), "/grant".into(), "*S-1-5-32-545:R".into()]);
        }
    }
    #[cfg(target_os = "linux")]
    if !layout.system {
        enable_linger();
    }

    // On Linux the program needs the system's ffmpeg: without it the service
    // stops at once, so do not claim it is running (the note above says how).
    let no_ffmpeg = layout.os == Os::Linux && service::find_ffmpeg_dir(&std::env::var("PATH").unwrap_or_default()).is_none();
    if no_ffmpeg {
        println!("\nAmberSight is installed, but it will not start until ffmpeg is installed:");
        println!("  Debian, Ubuntu, Raspberry Pi OS:  sudo apt install ffmpeg");
        println!("  then:                             systemctl --user restart ambersight   (or the system service: sudo systemctl restart ambersight)");
        println!("  open:      {url}");
        println!("  settings:  {}", layout.config.display());
        println!("  data:      {}", data_dir.display());
        return Ok(());
    }
    println!("\nAmberSight is installed and running.");
    println!("  open:      {url}");
    println!("  settings:  {}", layout.config.display());
    println!("  data:      {}", data_dir.display());
    if !a.no_browser {
        wait_for_port(port);
        open_browser(&url);
    }
    Ok(())
}

fn uninstall(env: &Env, l: &Layout, a: &SetupArgs) -> Result<()> {
    let config_dir = parent_dir(l.os, &l.config);
    let data_dir = std::fs::read_to_string(&l.config)
        .map(|t| data_dir_of(&t, &config_dir))
        .unwrap_or_else(|_| l.data_dir.clone());
    println!("Removing AmberSight: service, firewall rule, shortcuts{}.", if l.copy_program { ", program" } else { "" });
    println!("Kept: settings {} and recordings {}", l.config.display(), data_dir.display());
    if a.dry_run {
        println!("\n(dry run: nothing was changed)");
        return Ok(());
    }
    check_privileges(env, l)?;
    let target = l.service_target();
    if service::is_installed(&target) {
        service::uninstall(&target)?;
    }
    if l.os == Os::Windows {
        let _ = run_quiet("netsh", &netsh_delete_args());
    }
    for f in shortcut_files(l, "") {
        remove_file(&f.path);
    }
    for s in &l.shortcuts {
        // The macOS app is a folder.
        if s.is_dir() {
            let _ = std::fs::remove_dir_all(s);
        }
    }
    if l.os == Os::Windows {
        // The service's note about where its ffmpeg is (not part of any file list).
        remove_file(&l.program_dir.join(crate::ffmpeg_bin::HINT_FILE));
    }
    if l.copy_program {
        remove_program(env, l);
    }
    println!("\nAmberSight is removed. Your settings and recordings are still here:");
    println!("  settings:  {}", l.config.display());
    println!("  data:      {}", data_dir.display());
    println!("Delete those folders too if you no longer need them.");
    Ok(())
}

fn remove_file(p: &Path) {
    match std::fs::remove_file(p) {
        Ok(()) => println!("removed {}", p.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("warning: could not remove {}: {e}", p.display()),
    }
}

/// Stop with a clear message when this needs an Administrator terminal or sudo.
fn check_privileges(env: &Env, l: &Layout) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = l;
        if !is_elevated() {
            bail!(
                "setting up AmberSight needs an Administrator terminal.\n\
                 Right-click Terminal (or Command Prompt), choose \"Run as administrator\", then run:\n\n  {}",
                rerun_command(&env.exe)
            );
        }
    }
    #[cfg(unix)]
    {
        let root = service::is_root();
        if l.system && !root {
            bail!("installing for the whole computer needs root. Run:\n\n  sudo {}", rerun_command(&env.exe));
        }
        if !l.system && root {
            bail!(
                "this would install AmberSight for the root user. Run it without sudo to install it for \
                 yourself, or add --system to install it for the whole computer."
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: plain Win32 calls with valid, correctly sized arguments; the
    // token handle is closed before returning.
    unsafe {
        let mut token: HANDLE = 0;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Copy the program (and the libraries beside it) into the program folder.
fn copy_program(env: &Env, l: &Layout) -> Result<()> {
    std::fs::create_dir_all(&l.program_dir).with_context(|| format!("could not create {}", l.program_dir.display()))?;
    let src_dir = parent_dir(l.os, &env.exe);
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same(&src_dir, &l.program_dir) {
        println!("the program is already in {}", l.program_dir.display());
        return Ok(());
    }
    let mut files = vec![(env.exe.clone(), l.program())];
    if let Ok(rd) = std::fs::read_dir(&src_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some(l.os.library_ext()) {
                files.push((p.clone(), l.program_dir.join(e.file_name())));
            }
        }
    }
    let mut names = Vec::new();
    for (from, to) in files {
        replace_file(&from, &to)?;
        println!("copied {}", to.display());
        if let Some(n) = to.file_name() {
            names.push(n.to_string_lossy().into_owned());
        }
    }
    // What setup put here: the only files `setup --uninstall` removes.
    std::fs::write(l.program_dir.join(MANIFEST), names.join("\n") + "\n")
        .with_context(|| format!("could not write {}", l.program_dir.join(MANIFEST).display()))?;
    #[cfg(target_os = "macos")]
    {
        // A program that came through a web browser is marked as downloaded;
        // the copy would be blocked by Gatekeeper.
        let _ = run_quiet("xattr", &["-d".into(), "com.apple.quarantine".into(), l.program().to_string_lossy().into_owned()]);
    }
    Ok(())
}

/// Copy beside the target, then rename over it: works while the old
/// program is still running (Linux/macOS).
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    let tmp = to.with_extension("new");
    std::fs::copy(from, &tmp).with_context(|| format!("could not copy {} to {}", from.display(), tmp.display()))?;
    std::fs::rename(&tmp, to).with_context(|| format!("could not replace {}", to.display()))?;
    Ok(())
}

fn create_config(l: &Layout, text: &str) -> Result<()> {
    let dir = parent_dir(l.os, &l.config);
    let new_dir = !dir.exists();
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    if new_dir && l.os == Os::Windows {
        if let Err(e) = run_quiet(&crate::lockdown::icacls_exe().to_string_lossy(), &icacls_private_args(&dir)) {
            eprintln!("warning: could not make {} private: {e:#}", dir.display());
        }
    }
    // Never overwrite: create_new fails if a config appeared meanwhile.
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&l.config).with_context(|| format!("could not create {}", l.config.display()))?;
    f.write_all(text.as_bytes())?;
    println!("wrote {} (starter settings)", l.config.display());
    Ok(())
}

/// A system-wide service runs as the user who ran sudo (service.rs); that
/// user must be able to write the settings and data.
#[cfg(unix)]
fn give_to_service_user(paths: &[&Path]) -> Result<()> {
    let user = service::login_user().context("could not determine the user the service runs as")?;
    let id = |flag: &str| -> Result<u32> {
        let out = run_quiet("id", &[flag.into(), user.clone()])?;
        out.trim().parse().with_context(|| format!("bad id output for {user}"))
    };
    let (uid, gid) = (id("-u")?, id("-g")?);
    for p in paths {
        std::os::unix::fs::chown(p, Some(uid), Some(gid)).with_context(|| format!("could not hand {} to {user}", p.display()))?;
    }
    Ok(())
}

fn install_service(l: &Layout) -> Result<()> {
    let spec = ServiceSpec::for_program(l.program(), l.config.clone(), l.service_target().system);
    // A Windows service that was just removed can take a moment to go.
    let mut tries = if l.os == Os::Windows { 10 } else { 1 };
    loop {
        match service::install(&spec) {
            Ok(()) => return Ok(()),
            Err(e) if tries > 1 && format!("{e:#}").contains("marked for deletion") => {
                tries -= 1;
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(e) => return Err(e),
        }
    }
}

fn open_firewall(l: &Layout, port: u16) {
    match l.os {
        Os::Windows => {
            let _ = run_quiet("netsh", &netsh_delete_args());
            match run_quiet("netsh", &netsh_add_args(port)) {
                Ok(_) => println!("firewall: allowed TCP port {port} in (rule \"{RULE_NAME}\")"),
                Err(e) => eprintln!(
                    "warning: could not add the firewall rule ({e:#}). Run as Administrator:\n  netsh {}",
                    netsh_add_args(port).join(" ")
                ),
            }
        }
        Os::Mac => {}
        Os::Linux => {
            #[cfg(unix)]
            for fw in linux_firewalls() {
                let cmds = linux_firewall_commands(fw, port);
                if service::is_root() {
                    for c in &cmds {
                        match run_quiet(&c[0], &c[1..]) {
                            Ok(_) => println!("firewall: ran {}", c.join(" ")),
                            Err(e) => eprintln!("warning: {e:#}"),
                        }
                    }
                } else {
                    println!("\nA firewall is on. To reach AmberSight from other devices, run:");
                    for c in &cmds {
                        println!("  sudo {}", c.join(" "));
                    }
                }
            }
        }
    }
}

/// The Linux firewalls that are switched on.
#[cfg(unix)]
fn linux_firewalls() -> Vec<LinuxFirewall> {
    let mut out = Vec::new();
    // `ufw status` needs root; its settings file is readable by everyone.
    let ufw_on = if service::is_root() {
        run_quiet("ufw", &["status".into()]).map(|o| o.contains("Status: active")).unwrap_or(false)
    } else {
        std::fs::read_to_string("/etc/ufw/ufw.conf")
            .map(|t| t.lines().any(|l| l.trim().eq_ignore_ascii_case("ENABLED=yes")))
            .unwrap_or(false)
    };
    if ufw_on {
        out.push(LinuxFirewall::Ufw);
    }
    if run_quiet("firewall-cmd", &["--state".into()]).map(|o| o.trim() == "running").unwrap_or(false) {
        out.push(LinuxFirewall::Firewalld);
    }
    out
}

fn write_shortcuts(l: &Layout, url: &str) -> Result<()> {
    for s in &l.shortcuts {
        // Start afresh so no file of an older app version is left behind.
        if l.os == Os::Mac && s.is_dir() {
            let _ = std::fs::remove_dir_all(s);
        }
    }
    for f in shortcut_files(l, url) {
        let dir = parent_dir(l.os, &f.path);
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&f.path, &f.bytes)) {
            eprintln!("warning: could not write {}: {e}", f.path.display());
            continue;
        }
        #[cfg(unix)]
        if f.executable {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&f.path, std::fs::Permissions::from_mode(0o755));
        }
    }
    for s in &l.shortcuts {
        println!("shortcut: {}", s.display());
    }
    #[cfg(target_os = "linux")]
    {
        // Menus pick the new entry up sooner with fresh caches (optional tools).
        let apps = parent_dir(l.os, &l.shortcuts[0]);
        let _ = run_quiet("update-desktop-database", &[apps.to_string_lossy().into_owned()]);
    }
    Ok(())
}

/// Keep the per-user service running at boot and after signing out. When
/// that needs an administrator, the service install above already printed
/// the command.
#[cfg(target_os = "linux")]
fn enable_linger() {
    let who = service::login_user().unwrap_or_default();
    if Path::new("/var/lib/systemd/linger").join(&who).exists() {
        println!("start at boot: already on (linger)");
    } else if run_quiet("loginctl", &["enable-linger".into()]).is_ok() {
        println!("start at boot: turned on (loginctl enable-linger)");
    }
}

/// Give the server a few seconds to start listening before opening the page.
fn wait_for_port(port: u16) {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    for _ in 0..30 {
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

fn open_browser(url: &str) {
    let result = match Os::current() {
        // Through Explorer, so the browser does not run as Administrator.
        Os::Windows => run_quiet("explorer.exe", &[url.to_string()]).map(|_| ()),
        Os::Mac => run_quiet("open", &[url.to_string()]).map(|_| ()),
        Os::Linux => {
            let desktop = std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
            if desktop {
                std::process::Command::new("xdg-open")
                    .arg(url)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            } else {
                Err(anyhow::anyhow!("no desktop"))
            }
        }
    };
    if result.is_err() && Os::current() != Os::Windows {
        println!("Open {url} in a web browser.");
    }
}

/// The list of files setup copied into the program folder.
const MANIFEST: &str = "ambersight-files.txt";

/// The program already in the target folder, when setup did not put it
/// there (no list of its files beside it) and it is not this program.
fn foreign_program(env: &Env, l: &Layout) -> Option<PathBuf> {
    let p = l.program();
    let same = |a: &Path, b: &Path| matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b);
    // (On Windows and macOS file names ignore case: AmberSight.exe counts.)
    (p.exists() && !l.program_dir.join(MANIFEST).exists() && !same(&p, &env.exe)).then_some(p)
}

/// The file names in a list setup wrote: plain names only (never a path).
fn manifest_names(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|n| !n.is_empty() && !n.contains(['/', '\\', ':']) && *n != "." && *n != "..")
        .map(String::from)
        .collect()
}

/// Remove exactly the files setup copied (as listed beside them), then the
/// folder if it is empty. Anything else in the folder stays.
fn remove_program(env: &Env, l: &Layout) {
    let list = l.program_dir.join(MANIFEST);
    let Ok(text) = std::fs::read_to_string(&list) else {
        println!("{} has no list of the files setup installed: nothing removed there", l.program_dir.display());
        return;
    };
    let mut files: Vec<PathBuf> = manifest_names(&text).into_iter().map(|n| l.program_dir.join(n)).collect();
    files.push(list);
    let running = env.exe.canonicalize().ok();
    for f in files {
        // Windows cannot delete a running program: a short-lived helper
        // removes it once this one has exited.
        if l.os == Os::Windows && f.canonicalize().ok() == running {
            delete_after_exit(&f, &l.program_dir);
            continue;
        }
        remove_file(&f);
    }
    if std::fs::remove_dir(&l.program_dir).is_ok() {
        println!("removed {}", l.program_dir.display());
    }
}

#[cfg(windows)]
fn delete_after_exit(file: &Path, dir: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let line = format!(
        "/C ping -n 3 127.0.0.1 >NUL & del /F /Q \"{}\" & rmdir \"{}\"",
        file.display(),
        dir.display()
    );
    let _ = std::process::Command::new("cmd").raw_arg(line).creation_flags(CREATE_NO_WINDOW).spawn();
    println!("{} is removed when this command ends", file.display());
}

#[cfg(not(windows))]
fn delete_after_exit(file: &Path, _dir: &Path) {
    remove_file(file);
}

/// Run a program, returning its output; an error when it fails.
fn run_quiet(prog: &str, args: &[String]) -> Result<String> {
    let out = std::process::Command::new(prog)
        .args(args)
        .output()
        .with_context(|| format!("could not run {prog}"))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
        bail!("{prog} {} failed: {text}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {

    #[test]
    fn manifest_names_are_plain_file_names() {
        let n = manifest_names("ambersight.exe\nDirectML.dll\n\n../evil.dll\nC:\\x.dll\nsub/y.dll\n..\n");
        assert_eq!(n, vec!["ambersight.exe".to_string(), "DirectML.dll".to_string()]);
    }

    #[test]
    fn a_folder_with_someone_elses_program_is_refused() {
        let dir = std::env::temp_dir().join(format!("ambs-setup-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let os = if cfg!(windows) { Os::Windows } else { Os::Linux };
        let l = Layout { os, system: false, program_dir: dir.clone(), copy_program: true, config: dir.join("config.toml"), data_dir: dir.join("data"), shortcuts: vec![], icons: vec![] };
        let env = Env { exe: std::env::temp_dir().join("elsewhere").join(os.program_name()), ..env(os) };
        assert!(foreign_program(&env, &l).is_none(), "empty folder");
        std::fs::write(l.program(), b"other program").unwrap();
        assert!(foreign_program(&env, &l).is_some(), "someone else's program");
        std::fs::write(dir.join(MANIFEST), "ambersight\n").unwrap();
        assert!(foreign_program(&env, &l).is_none(), "installed by setup earlier");
        // Uninstall removes only listed files.
        std::fs::write(dir.join("theirs.dll"), b"x").unwrap();
        std::fs::write(dir.join(MANIFEST), format!("{}\n", os.program_name())).unwrap();
        remove_program(&env, &l);
        assert!(!l.program().exists() && !dir.join(MANIFEST).exists());
        assert!(dir.join("theirs.dll").exists(), "other files stay");
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn env(os: Os) -> Env {
        match os {
            Os::Windows => Env {
                os,
                home: Some(r"C:\Users\pat".into()),
                xdg_data_home: None,
                xdg_config_home: None,
                program_files: Some(r"C:\Program Files".into()),
                program_data: Some(r"C:\ProgramData".into()),
                public: Some(r"C:\Users\Public".into()),
                exe: r"C:\Users\pat\Downloads\ambersight\ambersight.exe".into(),
                cwd: r"C:\Users\pat\Downloads\ambersight".into(),
            },
            _ => Env {
                os,
                home: Some("/home/pat".into()),
                xdg_data_home: None,
                xdg_config_home: None,
                program_files: None,
                program_data: None,
                public: None,
                exe: "/home/pat/Downloads/as/ambersight".into(),
                cwd: "/home/pat/cams".into(),
            },
        }
    }

    fn args(f: impl FnOnce(&mut SetupArgs)) -> SetupArgs {
        let mut a = SetupArgs::default();
        f(&mut a);
        a
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn windows_layout() {
        let l = decide(&env(Os::Windows), &SetupArgs::default()).unwrap();
        assert_eq!(l.program(), p(r"C:\Program Files\AmberSight NVR\ambersight.exe"));
        assert!(l.copy_program && l.system);
        assert_eq!(l.config, p(r"C:\ProgramData\AmberSight\config.toml"));
        assert_eq!(l.data_dir, p(r"C:\ProgramData\AmberSight\data"));
        assert_eq!(
            l.shortcuts,
            vec![
                p(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\AmberSight.url"),
                p(r"C:\Users\Public\Desktop\AmberSight.url")
            ]
        );
        assert_eq!(l.icons, vec![p(r"C:\Program Files\AmberSight NVR\ambersight.ico")]);
    }

    #[test]
    fn windows_installer_and_here_layouts() {
        let mut e = env(Os::Windows);
        e.exe = r"D:\Apps\AmberSight\ambersight.exe".into();
        let l = decide(&e, &args(|a| a.installed = true)).unwrap();
        assert_eq!(l.program_dir, p(r"D:\Apps\AmberSight"));
        assert!(!l.copy_program);
        assert_eq!(l.config, p(r"C:\ProgramData\AmberSight\config.toml"));
        assert_eq!(l.icons, vec![p(r"D:\Apps\AmberSight\ambersight.ico")]);

        let l = decide(&env(Os::Windows), &args(|a| a.here = true)).unwrap();
        assert_eq!(l.program_dir, p(r"C:\Users\pat\Downloads\ambersight"));
        assert_eq!(l.config, p(r"C:\Users\pat\Downloads\ambersight\config.toml"));
        assert_eq!(l.data_dir, p(r"C:\Users\pat\Downloads\ambersight\amberdata"));
        assert!(!l.copy_program);
    }

    #[test]
    fn mac_layouts() {
        let l = decide(&env(Os::Mac), &SetupArgs::default()).unwrap();
        assert_eq!(l.program(), p("/home/pat/Applications/AmberSight/ambersight"));
        assert_eq!(l.config, p("/home/pat/Library/Application Support/AmberSight/config.toml"));
        assert_eq!(l.data_dir, p("/home/pat/Library/Application Support/AmberSight/data"));
        assert_eq!(l.shortcuts, vec![p("/home/pat/Applications/AmberSight.app")]);
        assert!(!l.system);

        let l = decide(&env(Os::Mac), &args(|a| a.system = true)).unwrap();
        assert_eq!(l.program(), p("/Applications/AmberSight/ambersight"));
        assert_eq!(l.config, p("/Library/Application Support/AmberSight/config.toml"));
        assert_eq!(l.shortcuts, vec![p("/Applications/AmberSight.app")]);
        assert!(l.system);
    }

    #[test]
    fn linux_layouts() {
        let l = decide(&env(Os::Linux), &SetupArgs::default()).unwrap();
        assert_eq!(l.program(), p("/home/pat/.local/share/ambersight/ambersight"));
        assert_eq!(l.config, p("/home/pat/.config/ambersight/config.toml"));
        assert_eq!(l.data_dir, p("/home/pat/.local/share/ambersight/data"));
        assert_eq!(l.shortcuts, vec![p("/home/pat/.local/share/applications/ambersight.desktop")]);
        assert_eq!(l.icons[0], p("/home/pat/.local/share/icons/hicolor/512x512/apps/ambersight.png"));
        assert_eq!(l.icons[1], p("/home/pat/.local/share/icons/hicolor/scalable/apps/ambersight.svg"));

        let mut e = env(Os::Linux);
        e.xdg_data_home = Some("/data/share".into());
        e.xdg_config_home = Some("/data/conf".into());
        let l = decide(&e, &SetupArgs::default()).unwrap();
        assert_eq!(l.program(), p("/data/share/ambersight/ambersight"));
        assert_eq!(l.config, p("/data/conf/ambersight/config.toml"));

        let l = decide(&env(Os::Linux), &args(|a| a.system = true)).unwrap();
        assert_eq!(l.program(), p("/opt/ambersight/ambersight"));
        assert_eq!(l.config, p("/etc/ambersight/config.toml"));
        assert_eq!(l.data_dir, p("/var/lib/ambersight"));
        assert_eq!(l.shortcuts, vec![p("/usr/share/applications/ambersight.desktop")]);
        assert_eq!(l.icons[0], p("/usr/share/icons/hicolor/512x512/apps/ambersight.png"));

        let l = decide(&env(Os::Linux), &args(|a| a.here = true)).unwrap();
        assert_eq!(l.program(), p("/home/pat/Downloads/as/ambersight"));
        assert_eq!(l.config, p("/home/pat/cams/config.toml"));
        assert_eq!(l.data_dir, p("/home/pat/cams/amberdata"));
        assert!(!l.copy_program);
    }

    #[test]
    fn user_layout_needs_home() {
        let mut e = env(Os::Linux);
        e.home = None;
        assert!(decide(&e, &SetupArgs::default()).is_err());
        assert!(decide(&e, &args(|a| a.system = true)).is_ok());
    }

    #[test]
    fn starter_config_points_at_the_data_folder() {
        for dir in ["/var/lib/ambersight", r"C:\ProgramData\AmberSight\data", "/it's \"odd\""] {
            let text = starter_config(Path::new(dir));
            let cfg: crate::config::Config = toml::from_str(&text).expect("starter config parses");
            assert_eq!(cfg.storage.data_dir, PathBuf::from(dir));
            assert_eq!(text.matches("data_dir = ").count(), 1);
        }
        assert_eq!(server_address(STARTER_CONFIG), (8090, false));
    }

    #[test]
    fn address_from_config() {
        assert_eq!(server_address("[server]\nport = 9000\ntls = true\n"), (9000, true));
        assert_eq!(server_address("not toml ["), (8090, false));
        assert_eq!(local_url(9000, true), "https://localhost:9000");
        assert_eq!(local_url(8090, false), "http://localhost:8090");
        assert_eq!(data_dir_of("[storage]\ndata_dir = \"rec\"\n", Path::new("/c")), p("/c/rec"));
        assert_eq!(data_dir_of("[storage]\ndata_dir = \"/abs\"\n", Path::new("/c")), p("/abs"));
        assert_eq!(data_dir_of("", Path::new("/c")), p("/c/amberdata"));
    }

    #[test]
    fn firewall_commands() {
        assert_eq!(netsh_delete_args().join(" "), "advfirewall firewall delete rule name=AmberSight NVR");
        assert_eq!(
            netsh_add_args(8090).join(" "),
            "advfirewall firewall add rule name=AmberSight NVR dir=in action=allow protocol=TCP localport=8090"
        );
        let ufw = linux_firewall_commands(LinuxFirewall::Ufw, 8090);
        assert_eq!(ufw, vec![vec!["ufw", "allow", "8090/tcp"]]);
        let fwd = linux_firewall_commands(LinuxFirewall::Firewalld, 9000);
        assert_eq!(fwd[0].join(" "), "firewall-cmd --add-port 9000/tcp --permanent");
        assert_eq!(fwd[1].join(" "), "firewall-cmd --reload");
        assert_eq!(
            icacls_private_args(Path::new(r"C:\ProgramData\AmberSight")).join(" "),
            r"C:\ProgramData\AmberSight /inheritance:r /grant:r *S-1-5-18:(OI)(CI)F /grant:r *S-1-5-32-544:(OI)(CI)F"
        );
    }

    #[test]
    fn shortcut_contents() {
        let url = windows_url_file("https://localhost:8443", Path::new(r"C:\Program Files\AmberSight NVR\ambersight.ico"));
        assert_eq!(
            url,
            "[InternetShortcut]\r\nURL=https://localhost:8443/\r\nIconFile=C:\\Program Files\\AmberSight NVR\\ambersight.ico\r\nIconIndex=0\r\n"
        );
        let d = linux_desktop_file("http://localhost:8090");
        assert!(d.starts_with("[Desktop Entry]\nType=Application\nName=AmberSight\n"));
        assert!(d.contains("\nExec=xdg-open http://localhost:8090\n"));
        assert!(d.contains("\nIcon=ambersight\n"));
        let plist = mac_info_plist();
        assert!(plist.contains("<key>CFBundleExecutable</key>\n\t<string>AmberSight</string>"));
        assert!(plist.contains("<key>CFBundleIconFile</key>\n\t<string>AmberSight</string>"));
        assert_eq!(mac_launcher("http://localhost:8090"), "#!/bin/sh\nexec /usr/bin/open \"http://localhost:8090\"\n");
    }

    #[test]
    fn shortcut_files_per_os() {
        let l = decide(&env(Os::Mac), &SetupArgs::default()).unwrap();
        let files = shortcut_files(&l, "http://localhost:8090");
        let paths: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
        let app = p("/home/pat/Applications/AmberSight.app");
        assert_eq!(
            paths,
            vec![app.join("Contents/Info.plist"), app.join("Contents/MacOS/AmberSight"), app.join("Contents/Resources/AmberSight.icns")]
        );
        assert!(files[1].executable);

        let l = decide(&env(Os::Windows), &SetupArgs::default()).unwrap();
        let files = shortcut_files(&l, "http://localhost:8090");
        assert_eq!(files.len(), 3);
        assert!(String::from_utf8_lossy(&files[0].bytes).contains(r"IconFile=C:\Program Files\AmberSight NVR\ambersight.ico"));
        assert_eq!(files[2].bytes, ICON_ICO);

        let l = decide(&env(Os::Linux), &SetupArgs::default()).unwrap();
        let files = shortcut_files(&l, "http://localhost:8090");
        assert_eq!(files.len(), 3);
        assert_eq!(files[1].bytes, ICON_PNG);
    }

    #[test]
    fn plan_names_every_place() {
        let l = decide(&env(Os::Linux), &SetupArgs::default()).unwrap();
        // Paths are joined with this computer's separator, so compare with "/" on Windows too.
        let t = plan_text(&l, false, &l.data_dir, "http://localhost:8090", 8090).replace('\\', "/");
        assert!(t.contains("for this user"));
        assert!(t.contains("/home/pat/.local/share/ambersight/ambersight (copied there)"));
        assert!(t.contains("/home/pat/.config/ambersight/config.toml (new, from the starter settings)"));
        assert!(t.contains("systemd user service"));
        assert!(t.contains("http://localhost:8090"));
        let l = decide(&env(Os::Windows), &SetupArgs::default()).unwrap();
        assert!(plan_text(&l, true, &l.data_dir, "u", 8090).contains("(kept as it is)"));
    }

    #[test]
    fn parent_dirs() {
        assert_eq!(parent_dir(Os::Windows, Path::new(r"C:\a b\x.exe")), p(r"C:\a b"));
        assert_eq!(parent_dir(Os::Linux, Path::new("/opt/x")), p("/opt"));
        assert_eq!(parent_dir(Os::Linux, Path::new("/x")), p("/"));
        assert_eq!(join(Os::Windows, Path::new(r"C:\a\"), "b/c.url"), p(r"C:\a\b\c.url"));
    }

    /// Reads a little-endian / big-endian number.
    fn le16(b: &[u8], i: usize) -> usize {
        u16::from_le_bytes([b[i], b[i + 1]]) as usize
    }
    fn le32(b: &[u8], i: usize) -> usize {
        u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize
    }
    fn be32(b: &[u8], i: usize) -> usize {
        u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize
    }

    #[test]
    fn committed_icons_are_well_formed() {
        // .ico: 16, 32, 48 and 256 pixel PNG pictures.
        let ico = ICON_ICO;
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        assert_eq!(le16(ico, 4), 4);
        let mut sizes = Vec::new();
        for n in 0..4 {
            let e = 6 + n * 16;
            let w = if ico[e] == 0 { 256 } else { ico[e] as usize };
            let (len, off) = (le32(ico, e + 8), le32(ico, e + 12));
            assert_eq!(&ico[off..off + 8], b"\x89PNG\r\n\x1a\n");
            assert!(off + len <= ico.len());
            sizes.push(w);
        }
        assert_eq!(sizes, vec![16, 32, 48, 256]);
        // .icns: the declared length and every entry a PNG.
        let icns = ICON_ICNS;
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(be32(icns, 4), icns.len());
        let mut i = 8;
        let mut kinds = Vec::new();
        while i < icns.len() {
            kinds.push(String::from_utf8_lossy(&icns[i..i + 4]).into_owned());
            assert_eq!(&icns[i + 8..i + 16], b"\x89PNG\r\n\x1a\n");
            i += be32(icns, i + 4);
        }
        assert_eq!(i, icns.len());
        assert_eq!(kinds, vec!["icp4", "icp5", "icp6", "ic07", "ic08", "ic09"]);
    }

    fn png_of_size(src: &image::DynamicImage, size: u32) -> Vec<u8> {
        let img = src.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    /// Remakes static/icons/ambersight.ico and ambersight.icns from
    /// icon-512.png: `cargo test make_icon_files -- --ignored`.
    #[test]
    #[ignore = "run by hand to remake the .ico and .icns"]
    fn make_icon_files() {
        let src = image::load_from_memory(ICON_PNG).unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("static/icons");

        let sizes = [16u32, 32, 48, 256];
        let pngs: Vec<Vec<u8>> = sizes.iter().map(|&s| png_of_size(&src, s)).collect();
        let mut ico = vec![0, 0, 1, 0];
        ico.extend((sizes.len() as u16).to_le_bytes());
        let mut offset = 6 + 16 * sizes.len();
        for (s, png) in sizes.iter().zip(&pngs) {
            let b = if *s >= 256 { 0 } else { *s as u8 };
            ico.extend([b, b, 0, 0]);
            ico.extend(1u16.to_le_bytes());
            ico.extend(32u16.to_le_bytes());
            ico.extend((png.len() as u32).to_le_bytes());
            ico.extend((offset as u32).to_le_bytes());
            offset += png.len();
        }
        for png in &pngs {
            ico.extend(png);
        }
        std::fs::write(dir.join("ambersight.ico"), ico).unwrap();

        let mut body = Vec::new();
        for (kind, s) in [("icp4", 16u32), ("icp5", 32), ("icp6", 64), ("ic07", 128), ("ic08", 256), ("ic09", 512)] {
            let png = if s == 512 { ICON_PNG.to_vec() } else { png_of_size(&src, s) };
            body.extend(kind.as_bytes());
            body.extend(((png.len() + 8) as u32).to_be_bytes());
            body.extend(png);
        }
        let mut icns = b"icns".to_vec();
        icns.extend(((body.len() + 8) as u32).to_be_bytes());
        icns.extend(body);
        std::fs::write(dir.join("ambersight.icns"), icns).unwrap();
    }
}
