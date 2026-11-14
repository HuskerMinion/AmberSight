use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::engine;
use crate::engine::Engine;
use crate::web;

#[derive(Parser)]
#[command(name = "ambersight", version, about = "AmberSight - self-hosted camera security and recording server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// A new password: from a prompt when a person is at the keyboard, or one line
/// from stdin (never on the command line, where others can see it).
fn read_new_password() -> Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    const MIN: usize = 8;
    let stdin = std::io::stdin();
    let line = |prompt: &str| -> Result<String> {
        if stdin.is_terminal() {
            print!("{prompt}");
            std::io::stdout().flush()?;
        }
        let mut s = String::new();
        stdin.lock().read_line(&mut s)?;
        Ok(s.trim_end_matches(['\r', '\n']).to_string())
    };
    let hidden = stdin.is_terminal() && set_terminal_echo(false);
    if stdin.is_terminal() && !hidden {
        println!("Note: what you type next will show on the screen (this system cannot hide it here, Windows included). Make sure nobody is looking.");
    }
    // (Ctrl-C while typing: the terminal must not stay silent.)
    let restore = hidden.then(|| {
        tokio::spawn(async {
            if tokio::signal::ctrl_c().await.is_ok() {
                set_terminal_echo(true);
                std::process::exit(130);
            }
        })
    });
    let first = line("New password (at least 8 characters): ");
    if hidden {
        println!();
    }
    let again = if stdin.is_terminal() { line("Repeat it: ") } else { first.as_ref().map(|s| s.clone()).map_err(|e| anyhow::anyhow!("{e}")) };
    if hidden {
        println!();
        set_terminal_echo(true);
    }
    if let Some(t) = restore {
        t.abort();
    }
    let (first, again) = (first?, again?);
    anyhow::ensure!(first == again, "the two passwords differ: nothing changed");
    anyhow::ensure!(first.chars().count() >= MIN, "a password needs at least {MIN} characters: nothing changed");
    Ok(first)
}

/// Turn off showing what is typed, where the system has the means. False: it stays on.
fn set_terminal_echo(on: bool) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("stty").arg(if on { "echo" } else { "-echo" }).stdin(std::process::Stdio::inherit()).status().is_ok_and(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        let _ = on;
        false
    }
}

#[derive(Subcommand)]
enum UserCmd {
    /// Turn off an account's two-step codes (a lost phone with no recovery
    /// codes left). Works with the server running or stopped
    ResetTwoStep {
        /// The account's username
        name: String,
        #[arg(long, default_value = "config.toml")]
        config: String,
    },
    /// Set an account's password: asked for here, or one line read from stdin.
    /// It signs that account out everywhere. Device tokens keep working unless
    /// --revoke-tokens. Works with the server running or stopped (a stopped
    /// server applies it when it starts)
    SetPassword {
        /// The account's username
        name: String,
        #[arg(long, default_value = "config.toml")]
        config: String,
        /// Also delete all the account's device tokens
        #[arg(long)]
        revoke_tokens: bool,
    },
    /// Enable a disabled account (for example the only admin, disabled by
    /// someone who had that session). Works with the server running or stopped
    Enable {
        /// The account's username
        name: String,
        #[arg(long, default_value = "config.toml")]
        config: String,
    },
}

#[derive(Subcommand)]
enum StorageCmd {
    /// Find recordings the database knows whose files are gone (a folder
    /// emptied, a drive reformatted, a database moved to another computer)
    /// and, with --apply, remove them from the database after a backup of
    /// it. Files are never deleted. Without --apply nothing is changed.
    /// Works with the server running or stopped. A folder that is not there
    /// at all, or holds none of its recordings, is left alone unless named
    /// with --include-folder (a wrong path looks the same as deleted files)
    PruneMissing {
        #[arg(long, default_value = "config.toml")]
        config: String,
        /// Remove them (after a backup); without it, only a report
        #[arg(long)]
        apply: bool,
        /// Also remove the missing recordings of this folder though it is not
        /// there or none of its recordings are ("data" = the data folder);
        /// may be given more than once
        #[arg(long = "include-folder", value_name = "FOLDER")]
        include_folder: Vec<String>,
    },
}

#[derive(Subcommand)]
enum Commands {
    /// Recording storage tools (see `storage prune-missing --help`)
    Storage {
        #[command(subcommand)]
        action: StorageCmd,
    },
    /// Create a starter config file
    Init {
        #[arg(default_value = "config.toml")]
        path: String,
    },
    /// Start the server and camera engine
    Run {
        #[arg(long, default_value = "config.toml")]
        config: String,
    },
    /// Install AmberSight in one step: program, settings, background
    /// service, firewall and a shortcut (`--uninstall` takes it away again)
    Setup(crate::setup::SetupArgs),
    /// Install, remove and control the background service
    Service {
        #[command(subcommand)]
        action: crate::service::ServiceCmd,
    },
    /// Windows service entry point (started by the service manager)
    #[command(hide = true)]
    ServiceRun {
        #[arg(long)]
        config: String,
        #[arg(long)]
        name: Option<String>,
        /// Directory to put first on PATH (where the installer found ffmpeg)
        #[arg(long)]
        path_prepend: Option<String>,
    },
    /// AmberSight Capture: send this computer's screen to an AmberSight server
    /// when it asks. Pair once with --pair, then it starts at sign-in.
    Capture {
        /// Pair with a server using the one-time code from its Settings page
        #[arg(long, num_args = 2, value_names = ["SERVER_URL", "CODE"], conflicts_with_all = ["unpair", "list"])]
        pair: Option<Vec<String>>,
        /// Accept the server's certificate fingerprint without asking
        #[arg(long, requires = "pair")]
        yes: bool,
        /// Forget the server and turn off start at sign-in
        #[arg(long, conflicts_with = "list")]
        unpair: bool,
        /// Print the monitors and windows that can be captured, then exit
        #[arg(long)]
        list: bool,
    },
    /// Check for a new version on GitHub and install it (the same as
    /// Settings > System > Status & updates). The previous version is kept in previous/
    /// next to the program.
    Update {
        #[arg(long, default_value = "config.toml")]
        config: String,
        /// Only check; install nothing
        #[arg(long)]
        check: bool,
    },
    /// For making releases: create a release signing key (kept offline)
    /// and print its public key
    ReleaseKeygen {
        /// Private key file to create (never overwritten)
        #[arg(long)]
        out: String,
    },
    /// For making releases: sign a SHA256SUMS file, writing <file>.sig
    ReleaseSign {
        /// Private key file from release-keygen
        #[arg(long)]
        key: String,
        file: String,
    },
    /// List configured cameras
    Cameras {
        #[arg(long, default_value = "config.toml")]
        config: String,
    },
    /// Show recent motion events
    Events {
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Probe a camera host for working RTSP URLs (ONVIF install wizard)
    Probe {
        #[arg(long, default_value = "8000")]
        onvif_port: u16,
        #[arg(long, default_value = "554")]
        rtsp_port: u16,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password: Option<String>,
        host: String,
    },
    /// Find ONVIF cameras on the LAN (WS-Discovery); with credentials, also
    /// read their profiles and print a ready-to-paste [[cameras]] block
    Discover {
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long, default_value_t = 3)]
        seconds: u64,
        /// Also scan an address range directly, e.g. 192.168.1.0/24 (for
        /// cameras on other subnets; may be repeated)
        #[arg(long)]
        subnet: Vec<String>,
    },
    /// Control a configured camera's PTZ over ONVIF: presets, goto TOKEN,
    /// stop, move PAN TILT ZOOM, set-preset NAME, remove-preset TOKEN
    Ptz {
        #[arg(long, default_value = "config.toml")]
        config: String,
        camera: String,
        action: String,
        args: Vec<String>,
    },
    /// Open a camera's audio backchannel and stream silence (or a WAV file)
    TalkTest {
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        camera: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Stream silence (the default)
        #[arg(long, conflicts_with = "wav")]
        silence: bool,
        /// Stream a 16-bit PCM WAV file instead (audible on the camera!)
        #[arg(long)]
        wav: Option<String>,
    },
    /// Index recordings on disk that the database does not know about yet
    Index {
        #[arg(long, default_value = "config.toml")]
        config: String,
    },
    /// Back up config.toml and the database now (into <data_dir>/backups)
    Backup {
        #[arg(long, default_value = "config.toml")]
        config: String,
    },
    /// Restore an off-site backup (settings, database, clips) into a folder.
    /// The passphrase is asked for, or read from AMBERSIGHT_OFFSITE_PASSPHRASE.
    OffsiteRestore {
        /// A backup folder, "s3" (the [offsite] settings in --config) or
        /// s3://bucket/prefix (with --endpoint, --region, --access-key and the
        /// secret key in AMBERSIGHT_S3_SECRET_KEY)
        #[arg(long)]
        from: String,
        /// Empty folder to restore into
        #[arg(long, required_unless_present = "list")]
        to: Option<String>,
        /// Show the copies there and stop
        #[arg(long)]
        list: bool,
        /// Which copy (a time from --list); default the newest
        #[arg(long)]
        snapshot: Option<String>,
        /// Settings and database only (no clips or pictures)
        #[arg(long)]
        no_media: bool,
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        access_key: Option<String>,
        /// Bucket in the path (MinIO)
        #[arg(long)]
        path_style: bool,
    },
    /// Decrypt one file from an off-site backup (.enc)
    OffsiteDecrypt {
        input: String,
        output: String,
    },
    /// Look up the recording covering a wall-clock time
    Seek {
        #[arg(long, default_value = "config.toml")]
        config: String,
        camera: String,
        #[arg(long)]
        at: Option<i64>,
    },
    /// Grab a single snapshot from a camera
    Snapshot {
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        camera: String,
        #[arg(long, default_value = "snapshot.jpg")]
        out: String,
    },
    /// Download an AI detection model (YOLOX, Apache-2.0) into the data folder
    AiModel {
        #[arg(long, default_value = "config.toml")]
        config: String,
        /// "tiny" (fast, 416x416), "s" (more accurate, 640x640) or "l" (most accurate, 640x640, about 5 times the work of s)
        #[arg(long, default_value = "tiny")]
        model: String,
    },
    /// Name faces from a folder of pictures (one sub-folder per person,
    /// named after them); downloads the face models if needed
    FacesImport {
        #[arg(long, default_value = "config.toml")]
        config: String,
        dir: String,
    },
    /// Set one of a user's interface preferences (merged into what the
    /// browser saved), e.g. --key dblclick --value '{"all":"timeline"}'
    UserPref {
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        key: String,
        /// JSON value
        #[arg(long)]
        value: String,
    },
    /// Account repairs for someone with access to this computer
    User {
        #[command(subcommand)]
        action: UserCmd,
    },
    /// Print each saved face's size, detector score and sharpness (numbers
    /// only), to see which are clear enough to recognize people by
    FacesQuality {
        #[arg(long, default_value = "config.toml")]
        config: String,
    },
    /// Try package detection on past visits (recordings): for each event in
    /// which AI saw a person, the picture before it and the one after are
    /// compared as the live check would (numbers only, plus event ids)
    PackagesScan {
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        camera: String,
        #[arg(long, default_value_t = 72)]
        hours: i64,
    },
    /// Run the built-in AI object detector on an image file
    AiTest {
        /// YOLOX .onnx model file
        #[arg(long)]
        model: String,
        #[arg(long, default_value_t = 0.4)]
        min_score: f32,
        /// Only the CPU (to compare with the graphics card)
        #[arg(long)]
        cpu: bool,
        /// How many times to run it
        #[arg(long, default_value_t = 3)]
        runs: u32,
        /// Settings file: its graphics card choices, and (Linux) where the
        /// NVIDIA add-on is kept, downloaded first when it is missing
        #[arg(long)]
        config: Option<String>,
        image: String,
    },
    /// Find and read license plates in image files (fetches the plate
    /// models into --models first if they are not there) and report the
    /// time each step takes on the CPU
    LprTest {
        /// Where the plate models are (kept between runs)
        #[arg(long, default_value = "lpr-models")]
        models: String,
        #[arg(long, default_value_t = 0.4)]
        min_score: f32,
        /// AI threads
        #[arg(long, default_value_t = 2)]
        threads: usize,
        /// Write each found plate's picture next to the models as plate-N.jpg
        #[arg(long)]
        save: bool,
        images: Vec<String>,
    },
    /// Native RTSP test (retina): connect to a camera's main and sub
    /// streams, receive for a while, and report codecs, timing and GOP.
    IngestTest {
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        camera: String,
        #[arg(long, default_value_t = 15)]
        seconds: u64,
        /// Override the sub-stream URL (else the camera's sub_url, if any).
        #[arg(long)]
        sub_url: Option<String>,
    },
    /// Record a camera direct-to-disk with the native RTSP reader
    /// (fragmented MP4 segments, no re-encoding).
    RecordTest {
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        camera: String,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        #[arg(long, default_value_t = 10)]
        segment_seconds: u64,
        /// Record this URL instead of the camera's main stream.
        #[arg(long)]
        url: Option<String>,
        #[arg(long, default_value = "record-test")]
        out: String,
        #[arg(long)]
        no_audio: bool,
        /// Test triggered recording: fire a simulated motion trigger this
        /// many seconds in; only pre-roll + trigger + post-roll is recorded.
        #[arg(long)]
        trigger_at: Option<u64>,
        #[arg(long, default_value_t = 5)]
        pre_roll: u64,
        #[arg(long, default_value_t = 5)]
        post_roll: u64,
    },
    /// Receive a camera stream natively and decode it through ffmpeg (decoder test)
    DecodeTest {
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        camera: String,
        #[arg(long, default_value_t = 15)]
        seconds: u64,
        /// Stream URL to use instead of the camera's sub_url (or main url).
        #[arg(long)]
        sub_url: Option<String>,
        /// Hardware decode: vaapi, vaapi:/dev/dri/renderD129, qsv
        #[arg(long)]
        hwaccel: Option<String>,
        /// Decoded frames per second
        #[arg(long, default_value_t = 5)]
        fps: u32,
        /// Output width in pixels (0 = keep source size)
        #[arg(long, default_value_t = 640)]
        scale: u32,
        /// Write every Nth decoded JPEG to --out
        #[arg(long, default_value_t = 5)]
        every: u64,
        /// Directory for sample JPEGs
        #[arg(long)]
        out: String,
    },
    /// Subscribe to a camera's ONVIF events and print them live
    OnvifEvents {
        #[arg(long, default_value = "config.toml")]
        config: String,
        #[arg(long)]
        camera: String,
        /// ONVIF HTTP port of the camera
        #[arg(long, default_value_t = 8000)]
        port: u16,
        #[arg(long, default_value_t = 60)]
        seconds: u64,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init { path } => {
            std::fs::write(&path, crate::setup::STARTER_CONFIG)?;
            println!("wrote starter config to {path}");
            Ok(())
        }
        Commands::Run { config } => run_server(config).await,
        Commands::Setup(args) => tokio::task::spawn_blocking(move || crate::setup::run(args)).await?,
        Commands::Service { action } => crate::service::handle(action),
        Commands::ServiceRun { config, name, path_prepend } => {
            // An update may restart the service by stopping with an error.
            crate::update::set_under_service();
            // The dispatcher blocks until the service stops; keep it off the async workers.
            tokio::task::spawn_blocking(move || crate::service::service_run(config, name, path_prepend)).await?
        }
        Commands::Capture { pair, yes, unpair, list } => {
            let pair = pair.map(|v| (v[0].clone(), v[1].clone()));
            crate::capture::main(crate::capture::Opts { pair, unpair, yes, list }).await
        }
        Commands::Update { config, check } => {
            let cfg = Config::load(&config)?;
            tokio::task::spawn_blocking(move || crate::update::cli(&cfg.update, &cfg.storage.data_dir, check)).await?
        }
        Commands::ReleaseKeygen { out } => {
            let public = crate::update::keygen(std::path::Path::new(&out))?;
            println!("private key written to {out} (keep it offline and backed up; never commit it)");
            println!("public key (RELEASE_PUBKEY in src/update.rs):\n{public}");
            Ok(())
        }
        Commands::ReleaseSign { key, file } => {
            let (sig, public) = crate::update::sign_file(std::path::Path::new(&key), std::path::Path::new(&file))?;
            println!("wrote {}", sig.display());
            if public != crate::update::RELEASE_PUBKEY {
                anyhow::bail!("this key is not the one built into this program (RELEASE_PUBKEY): its updater would refuse this signature");
            }
            Ok(())
        }
        Commands::Cameras { config } => {
            let cfg = Config::load(&config)?;
            if cfg.cameras.is_empty() {
                println!("no cameras configured");
                return Ok(());
            }
            for (i, c) in cfg.cameras.iter().enumerate() {
                println!(
                    "[{i}] {name}  ({url})  fps={fps} scale={scale} record={record} motion={motion} enabled={enabled}",
                    name = c.name,
                    url = c.url,
                    fps = c.fps,
                    scale = c.scale,
                    record = c.record,
                    motion = c.motion,
                    enabled = c.enabled,
                );
            }
            Ok(())
        }
        Commands::Events { config, limit } => {
            let cfg = Config::load(&config)?;
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            db.import_legacy(&cfg.storage.data_dir, &cfg.storage.folder_roots(), cfg.storage.segment_seconds)?;
            let q = crate::db::EventQuery { limit, ..Default::default() };
            for ev in db.query_events(&q)?.into_iter().rev() {
                println!(
                    "{}  {}  score {:.3}  clip {}{}",
                    chrono::DateTime::<chrono::Utc>::from_timestamp(ev.start, 0)
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default(),
                    ev.camera,
                    ev.score,
                    ev.clip.as_deref().unwrap_or("none"),
                    if ev.protect { "  [protected]" } else { "" },
                );
            }
            Ok(())
        }
        Commands::Probe { host, onvif_port, rtsp_port, user, password } => {
            let uname: String = user.unwrap_or("".into());
            let passwd: String = password.unwrap_or("".into());
            let auth: String = if uname.is_empty() {
                "".into()
            } else {
                format!("{}:{}", uname, passwd)
            };
            // 1) Standard ONVIF media-profile discovery.
            let onvif_url = format!("http://{}:{}/onvif/devices", host, onvif_port);
            println!("probe: GET {}", onvif_url);
            let out = std::process::Command::new("curl")
                .args(["-s", "-m", "5", &onvif_url]).output();
            let found: Vec<String> = match out {
                Err(_) => { println!("probe: curl unavailable"); Vec::new() }
                Ok(o) => {
                    let text = String::from_utf8(o.stdout).unwrap_or("".into());
                    text.split('"').filter(|f| f.starts_with("rtsp://")).map(|f| f.to_string()).collect()
                }
            };
            for u in found.iter() {
                println!("  ONVIF media URI: {}", u);
            }
            // 2) RTSP DESCRIBE against known-good paths (common NVR profiles + Reolink classics).
            let paths = [
                "Preview_02_main", "Preview_02_sub", "h264Preview_01_main",
                "h264Preview_01_sub", "stream1", "stream2", "cam1", "media", "live", "onvif_main",
            ];
            let mut working: Vec<String> = Vec::new();
            for p in paths {
                let url = if auth.is_empty() {
                    format!("rtsp://{}:{}/{}", host, rtsp_port, p)
                } else {
                    format!("rtsp://{}@{}:{}/{}", auth, host, rtsp_port, p)
                };
                // (On curl's stdin: the URL holds the camera's password.)
                let args: Vec<String> = ["-s", "-m", "5", "--url", &url, "-X", "DESCRIBE", "-o", if cfg!(windows) { "NUL" } else { "/dev/null" }, "-w", "%{http_code}"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                let code = crate::child::curl(&args);
                let code_s: String = match code {
                    Err(_) => "err".to_string(),
                    Ok(o) => String::from_utf8(o.stdout).unwrap_or("?".into()),
                };
                let ok = code_s.starts_with("200") || code_s.starts_with("20");
                println!("  {} -> {} {}", p, code_s, if ok { "OK" } else { "" });
                if ok {
                    working.push(url);
                }
            }
            if !working.is_empty() {
                // DESCRIBE 200 can be a permissive server lying; validate by
                // actually pulling one frame from the top candidate.
                let mut best: Option<String> = None;
                for cand in working.iter().take(3) {
                    let st = std::process::Command::new(crate::ffmpeg_bin::program())
                        .args(["-hide_banner", "-loglevel", "error",
                               "-timeout", "5", "-rtsp_transport", "tcp",
                               "-i", &cand.clone(), "-frames:v", "1", "-f", "null", "-"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    let ok = match st { Err(_) => false, Ok(s) => s.success() };
                    println!("  validate {} -> {}", cand.clone(), if ok { "plays" } else { "no frames" });
                    if ok && best.is_none() {
                        best = Some(cand.clone());
                    }
                }
                if let Some(u) = best {
                    println!("\nsuggested [[cameras]] block (direct = true for encoded RTSP):");
                    println!("[[cameras]]");
                    println!("name = \"{}\"", host);
                    println!("url = \"{}\"", u.clone());
                    println!("direct = true");
                } else {
                    println!("paths answered DESCRIBE but none delivered frames; check credentials.");
                }
            } else if !found.is_empty() {
                println!("ONVIF advertised the URIs above; try them with direct = true.");
            } else {
                println!("no RTSP endpoints found; check host/port/credentials.");
            }
            Ok(())
        }
        Commands::Discover { user, password, seconds, subnet } => {
            let wait = std::time::Duration::from_secs(seconds.clamp(1, 30));
            let mut hosts = Vec::new();
            for sn in &subnet {
                hosts.extend(crate::onvif_ptz::parse_subnet(sn)?);
            }
            let (devices, rtsp_only) = tokio::task::spawn_blocking(move || {
                let mut d = crate::onvif_ptz::discover(wait);
                let mut rtsp = Vec::new();
                if !hosts.is_empty() {
                    let (more, r) = crate::onvif_ptz::discover_hosts(&hosts, wait);
                    for m in more {
                        if !d.iter().any(|x| x.ip == m.ip) {
                            d.push(m);
                        }
                    }
                    rtsp = r;
                }
                (d, rtsp)
            })
            .await?;
            for h in &rtsp_only {
                println!("{}  RTSP on port {} (no ONVIF answer)", h.ip, h.port);
            }
            if devices.is_empty() {
                println!("no ONVIF devices answered");
            }
            for d in devices {
                println!("{}  {}  hw={}  loc={}", d.ip, if d.name.is_empty() { "(unnamed)" } else { &d.name }, d.hardware, d.location);
                for x in &d.xaddrs {
                    println!("  {x}");
                }
                let Some(user) = user.clone() else { continue };
                let Some(xaddr) = d.xaddrs.iter().find(|x| x.contains(&d.ip)).or(d.xaddrs.first()).cloned() else { continue };
                let pw = password.clone().unwrap_or_default();
                let r = tokio::task::spawn_blocking(move || crate::onvif_ptz::inspect(&xaddr, &user, &pw)).await?;
                match r {
                    Err(e) => println!("  inspect failed: {e:#}"),
                    Ok(i) => {
                        println!("  {} {} fw {} serial {}", i.info.manufacturer, i.info.model, i.info.firmware, i.info.serial);
                        for p in &i.profiles {
                            println!("  profile {} ({}) {}x{} {} ptz={} {}", p.name, p.token, p.width, p.height, p.encoding, p.has_ptz, p.uri.as_deref().unwrap_or("-"));
                        }
                        println!();
                        println!("{}", crate::onvif_ptz::toml_block(&i.suggested, &d.name));
                    }
                }
            }
            Ok(())
        }
        Commands::Ptz { config, camera, action, args } => {
            let cfg = Config::load(&config)?;
            let mut cam = cfg
                .cameras
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&camera))
                .context("camera not found in config")?
                .clone();
            engine::normalize_creds(&mut cam);
            let t = crate::onvif_ptz::target_from_config(&cam)?;
            let (args_n, args_w) = (args.clone(), args);
            let num = move |i: usize| -> Result<f64> {
                args_n.get(i).map(|v| v.parse::<f64>()).transpose()?.ok_or_else(|| anyhow::anyhow!("missing argument"))
            };
            let word = move |i: usize| -> Result<String> { args_w.get(i).cloned().ok_or_else(|| anyhow::anyhow!("missing argument")) };
            let res: Result<()> = tokio::task::spawn_blocking(move || {
                use crate::onvif_ptz as p;
                match action.as_str() {
                    "presets" => {
                        for x in p::ptz_presets(&t)? {
                            println!("{}\t{}", x.token, x.name);
                        }
                    }
                    "stop" => p::ptz_stop(&t)?,
                    "goto" => p::ptz_goto(&t, &word(0)?)?,
                    "move" => p::ptz_move(&t, num(0)?, num(1)?, num(2).unwrap_or(0.0), Some(std::time::Duration::from_millis(500)))?,
                    "set-preset" => println!("token {}", p::ptz_set_preset(&t, &word(0)?)?),
                    "remove-preset" => p::ptz_remove_preset(&t, &word(0)?)?,
                    other => anyhow::bail!("unknown action {other}"),
                }
                Ok(())
            })
            .await?;
            res
        }
        Commands::OffsiteRestore { from, to, list, snapshot, no_media, config, endpoint, region, access_key, path_style } => {
            use crate::offsite::{restore, store};
            let cfg = if from == "s3" {
                let mut c = Config::load(&config)?.offsite;
                c.kind = "s3".into();
                c
            } else if let Some(rest) = from.strip_prefix("s3://") {
                let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
                crate::config::OffsiteConfig {
                    kind: "s3".into(),
                    s3_bucket: bucket.into(),
                    s3_prefix: prefix.into(),
                    s3_endpoint: endpoint.unwrap_or_default(),
                    s3_region: region.unwrap_or_default(),
                    s3_access_key: access_key.or_else(|| std::env::var("AMBERSIGHT_S3_ACCESS_KEY").ok()).unwrap_or_default(),
                    s3_secret_key: std::env::var("AMBERSIGHT_S3_SECRET_KEY").unwrap_or_default(),
                    s3_path_style: path_style,
                    ..Default::default()
                }
            } else {
                crate::config::OffsiteConfig { kind: "folder".into(), folder: from.clone(), ..Default::default() }
            };
            tokio::task::spawn_blocking(move || -> Result<()> {
                let st = store::open(&cfg)?;
                if list {
                    let snaps = restore::list_snapshots(st.as_ref())?;
                    if snaps.is_empty() {
                        println!("no finished copies at {}", st.describe());
                    }
                    for s in snaps {
                        println!("{s}");
                    }
                    return Ok(());
                }
                let to = std::path::PathBuf::from(to.context("--to is needed")?);
                // From S3, or with the passphrase given up front, a copy that
                // is not encrypted is refused (it could have been put there).
                let must_encrypt = cfg.kind == "s3" || std::env::var("AMBERSIGHT_OFFSITE_PASSPHRASE").is_ok_and(|p| !p.is_empty());
                let n = restore::restore(st.as_ref(), &to, snapshot.as_deref(), !no_media, must_encrypt, &mut restore::ask_passphrase, &|m| println!("{m}"))?;
                println!("{n} files restored into {}.", to.display());
                println!("To use them: stop AmberSight, put config.toml next to the program and the rest into the data folder, then start it again.");
                Ok(())
            })
            .await?
        }
        Commands::OffsiteDecrypt { input, output } => {
            let pass = crate::offsite::restore::ask_passphrase()?;
            crate::offsite::restore::decrypt_file(std::path::Path::new(&input), std::path::Path::new(&output), &pass)?;
            println!("decrypted into {output}");
            Ok(())
        }
        Commands::Backup { config } => {
            let cfg = Config::load(&config)?;
            std::fs::create_dir_all(&cfg.storage.data_dir)?;
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            let dir = crate::backup::backup_now(std::path::Path::new(&config), &db, &cfg.storage.data_dir, cfg.storage.backup_keep)?;
            println!("backup saved to {}", dir.display());
            Ok(())
        }
        Commands::Storage { action: StorageCmd::PruneMissing { config, apply, include_folder } } => {
            tokio::task::spawn_blocking(move || crate::missing::cli(&config, apply, &include_folder)).await?
        }
        Commands::Index { config } => {
            let cfg = Config::load(&config)?;
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            let r = db.import_legacy(&cfg.storage.data_dir, &cfg.storage.folder_roots(), cfg.storage.segment_seconds)?;
            println!(
                "indexed {} new recordings; imported {} events ({} unreadable lines skipped)",
                r.segments_imported, r.events_imported, r.events_skipped
            );
            for (camera, bytes) in db.segment_bytes_by_camera()? {
                println!("  {camera}: {:.1} MB of recordings", bytes as f64 / 1048576.0);
            }
            Ok(())
        }
        Commands::Seek { config, camera, at } => {
            let cfg = Config::load(&config)?;
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            let t = at.unwrap_or(chrono::Utc::now().timestamp());
            match db.segment_at(&crate::config::slug(&camera), t * 1000)? {
                Some(r) => println!(
                    "{}  {}..{} ms  ({} bytes, {:.1} s into the file)",
                    r.path, r.start_ms, r.end_ms, r.bytes, (t * 1000 - r.start_ms) as f64 / 1000.0
                ),
                None => println!("no recording covers {t}"),
            }
            Ok(())
        }
        Commands::OnvifEvents { config, camera, port, seconds } => {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex};
            let cfg = Config::load(&config)?;
            let cam = cfg
                .cameras
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&camera))
                .context("camera not found in config")?;
            let host = crate::onvif::host_from_rtsp_url(&cam.url).context("cannot find a host in the camera url")?;
            let (user, pass) = if cam.username.is_empty() {
                crate::onvif::credentials_from_url(&cam.url).unwrap_or_default()
            } else {
                (cam.username.clone(), cam.password.clone())
            };
            println!("subscribing to ONVIF events on {host}:{port} for {seconds}s");
            let seen: Arc<Mutex<Vec<crate::onvif::OnvifEvent>>> = Arc::default();
            let sink = seen.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let handle = crate::onvif::OnvifEventSource::spawn(host, port, user, pass, move |ev| {
                println!(
                    "{}  {:<8} active={:<5} source={:<12} {} [{}]{}",
                    chrono::Local::now().format("%H:%M:%S"),
                    ev.kind.label(),
                    ev.active,
                    if ev.source.is_empty() { "-" } else { ev.source.as_str() },
                    ev.topic,
                    ev.utc_time.as_deref().unwrap_or("-"),
                    if ev.initial { "  (initial state)" } else { "" }
                );
                sink.lock().unwrap().push(ev);
            }, stop.clone());
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            stop.store(true, Ordering::Relaxed);
            tokio::task::spawn_blocking(move || handle.join()).await?.ok();
            let seen = seen.lock().unwrap();
            println!("-- {} event(s)", seen.len());
            let mut counts: std::collections::BTreeMap<(String, bool), usize> = Default::default();
            for e in seen.iter() {
                *counts.entry((e.topic.clone(), e.active)).or_default() += 1;
            }
            for ((topic, active), n) in counts {
                println!("  {n} x {topic} active={active}");
            }
            Ok(())
        }
        Commands::Snapshot { config, camera, out } => {
            let cfg = Config::load(&config)?;
            let cam = cfg
                .cameras
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&camera))
                .context("camera not found in config")?;
            let mut args = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
            args.extend(engine::ffmpeg_input_args(&engine::camera_url(&cam)));
            args.extend(["-vf".into(), engine::video_filter(&cam)]);
            args.extend(["-frames:v".into(), "1".into(), out.clone()]);
            crate::ffmpeg_bin::set_data_dir(&cfg.storage.data_dir);
            let status = std::process::Command::new(crate::ffmpeg_bin::program()).args(&args).status()?;
            if status.success() {
                println!("saved snapshot to {out}");
            } else {
                anyhow::bail!("snapshot failed (check the camera url)");
            }
            Ok(())
        }
        Commands::RecordTest { config, camera, seconds, segment_seconds, url, out, no_audio, trigger_at, pre_roll, post_roll } => {
            let cfg = Config::load(&config)?;
            let mut cam = cfg
                .cameras
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&camera))
                .context("camera not found in config")?
                .clone();
            engine::normalize_creds(&mut cam);
            let src = crate::ingest::StreamSource {
                group: None,
                url: url.unwrap_or(cam.url.clone()),
                username: cam.username.clone(),
                password: cam.password.clone(),
            };
            println!("recording {} for {} s into {}", engine::redact_url(src.url.clone()), seconds, out);
            let segs = crate::ingest::recorder::record_stream(
                &src, std::path::Path::new(&out), &crate::config::slug(&cam.name),
                segment_seconds, seconds, !no_audio,
                match trigger_at {
                    None => crate::ingest::recorder::RecordMode::Continuous,
                    Some(t) => crate::ingest::recorder::RecordMode::Triggered {
                        trigger_after: std::time::Duration::from_secs(t), pre_roll, post_roll,
                    },
                },
            ).await?;
            for s in &segs {
                println!(
                    "{}  {:.2} s  {} frames  {} fragments  {} KB",
                    s.path.display(),
                    (s.end_ms - s.start_ms) as f64 / 1000.0,
                    s.video_frames, s.fragments, s.bytes / 1024
                );
            }
            Ok(())
        }
        Commands::FacesImport { config, dir } => {
            let cfg = Config::load(&config)?;
            let models_dir = cfg.storage.data_dir.join("models");
            if !crate::faces::models_present(&models_dir) {
                println!("getting the face models ...");
                crate::faces::download_models(&models_dir, &|_, _| {})?;
            }
            let models = crate::faces::FaceModels::load(&models_dir, 2)?;
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            let r = crate::engine::faces::import_named_faces(&db, &models, std::path::Path::new(&dir))?;
            println!("imported {} faces of: {}", r.faces, r.people.join(", "));
            if !r.skipped.is_empty() {
                println!("no clear face in: {}", r.skipped.join(", "));
            }
            Ok(())
        }
        Commands::User { action: UserCmd::ResetTwoStep { name, config } } => {
            let cfg = Config::load(&config)?;
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            let name = name.trim();
            let had = db.two_step_enabled(name)?;
            db.two_step_remove(name)?;
            if had {
                db.insert_audit("(command line)", "", "two-step turned off (reset-two-step)", name, 200)?;
                // (So that admins are told, when the server next looks.)
                db.account_request_add(name, "two-step-reset", "", false)?;
                println!("Two-step codes are off for {name}. They can sign in with just their password, and turn it on again in Your account.");
                println!("(Too many wrong tries? That wait still runs out by itself, within an hour at most.)");
                println!("If someone else may know the password, change it now: ambersight user set-password {name} --config <file>");
            } else {
                println!("{name} does not have two-step codes on: nothing changed.");
            }
            Ok(())
        }
        Commands::User { action: UserCmd::SetPassword { name, config, revoke_tokens } } => {
            let cfg = Config::load(&config)?;
            let name = name.trim().to_string();
            let known = cfg.server.users.iter().any(|u| u.username.trim() == name) || cfg.server.auth.as_ref().is_some_and(|a| a.username.trim() == name);
            anyhow::ensure!(known, "there is no account named {name} in {config}");
            let password = read_new_password()?;
            let hash = crate::auth::hash_password(&password);
            anyhow::ensure!(crate::auth::is_hashed(&hash), "could not hash the password");
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            db.account_request_add(&name, "password", &hash, revoke_tokens)?;
            db.insert_audit("(command line)", "", "password set (set-password), waiting for the server", &name, 200)?;
            println!("The new password of {name} will apply within a few seconds on a running server, or when a stopped one starts. It signs {name} out everywhere.");
            if revoke_tokens {
                println!("The device tokens of {name} will be deleted too.");
            } else {
                println!("The device tokens of {name} are not revoked and keep working. Run this with --revoke-tokens to delete them too.");
            }
            Ok(())
        }
        Commands::User { action: UserCmd::Enable { name, config } } => {
            let cfg = Config::load(&config)?;
            let name = name.trim().to_string();
            let Some(u) = cfg.server.users.iter().find(|u| u.username.trim() == name) else {
                anyhow::bail!("there is no account named {name} in {config}");
            };
            if !u.disabled {
                println!("{name} is not disabled in {config}: asking the server to make sure.");
            }
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            db.account_request_add(&name, "enable", "", false)?;
            db.insert_audit("(command line)", "", "account enable asked for (user enable), waiting for the server", &name, 200)?;
            println!("{name} will be enabled within a few seconds on a running server, or when a stopped one starts. It signs in with its password (and code) again, and devices signed in before must sign in again.");
            Ok(())
        }
        Commands::UserPref { config, user, key, value } => {
            let cfg = Config::load(&config)?;
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            let v: serde_json::Value = serde_json::from_str(&value).map_err(|e| anyhow::anyhow!("--value is not JSON: {e}"))?;
            let cur = db.ui_get(&user, "prefs")?.unwrap_or_else(|| "{}".into());
            let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&cur).unwrap_or_default();
            obj.insert(key.clone(), v);
            let text = serde_json::Value::Object(obj).to_string();
            crate::ui_http::parse_prefs(text.as_bytes()).map_err(|e| anyhow::anyhow!("prefs {e}"))?;
            db.ui_put(&user, "prefs", &text)?;
            println!("set {key} for {user}");
            Ok(())
        }
        Commands::FacesQuality { config } => {
            let cfg = Config::load(&config)?;
            let db = crate::db::Db::open(&cfg.storage.data_dir.join("ambersight.db"))?;
            for f in db.list_faces(None, None, false, 100_000)? {
                let id = f["id"].as_i64().unwrap_or(0);
                let Some((_, img)) = db.face_img(id)? else { continue };
                let (lap, gray) = crate::faces::sharpness_parts(&img).unwrap_or((0.0, 0.0));
                println!(
                    "{id}\t{}\t{}\tsize {}\tscore {:.2}\tdetail {lap:.1}\tcontrast {gray:.1}",
                    if f["labeled"].as_bool() == Some(true) { "named" } else { "-" },
                    f["person"].as_str().unwrap_or("-"),
                    f["size"],
                    f["score"].as_f64().unwrap_or(0.0)
                );
            }
            Ok(())
        }
        Commands::AiModel { config, model } => {
            let m = crate::ai_setup::find(&model).ok_or_else(|| anyhow::anyhow!("unknown model '{model}': use tiny, s or l"))?;
            let cfg = Config::load(&config)?;
            let dir = cfg.storage.data_dir.join("models");
            println!("getting the {} model ...", m.name);
            let last = std::sync::atomic::AtomicU64::new(0);
            let dest = crate::ai_setup::ensure(&dir, m, &|b, t| {
                let mb = b / 1_000_000;
                if mb != last.swap(mb, std::sync::atomic::Ordering::Relaxed) {
                    match t {
                        Some(t) => print!("\r  {mb} / {} MB", t / 1_000_000),
                        None => print!("\r  {mb} MB"),
                    }
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
            })?;
            println!();
            let det = crate::ai::AiDetector::load(&dest, 2)?;
            let path = crate::ai_setup::clean_path(&std::fs::canonicalize(&dest).unwrap_or(dest.clone()));
            println!("model ready: {path} ({}x{} input)", det.input_size(), det.input_size());
            println!("\nadd to {config} (or use Settings > Detection & alerts > Objects (AI), which does this for you):\n\n[ai]\nenabled = true\nmodel = \"{path}\"\n");
            Ok(())
        }
        Commands::PackagesScan { config, camera, hours } => {
            let cfg = Config::load(&config)?;
            crate::engine::packages::scan(&cfg, &camera, hours)
        }
        Commands::AiTest { model, min_score, cpu, runs, config, image } => {
            let ai = match &config {
                Some(c) => Config::load(c)?.ai,
                None => crate::config::AiConfig::default(),
            };
            #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
            if let Some(c) = &config {
                let cfg = Config::load(c)?;
                let data_dir = &cfg.storage.data_dir;
                crate::gpu_addon::configure(data_dir, &cfg.update.repo, cfg.update.check);
                let wanted = !cpu && ai.gpu && !crate::gpu::list().is_empty() && crate::gpu_addon::missing_system_libs().is_none();
                match crate::gpu_addon::find() {
                    // A copy this user may not load (another user's, e.g. the service's while running as root).
                    Err(why) if wanted => anyhow::bail!("{why}"),
                    Ok(None) if wanted => match crate::gpu_addon::downloads_allowed(data_dir, cfg.update.check) {
                        Err(why) => println!("{why}"),
                        Ok(()) => {
                            println!("downloading the NVIDIA add-on into {}", crate::gpu_addon::folder(data_dir).display());
                            let last = std::sync::atomic::AtomicU64::new(u64::MAX);
                            match crate::gpu_addon::download(data_dir, &cfg.update.repo, &|d, _| {
                                // Once per MB, not per chunk (a log or ssh session keeps every line).
                                let mb = d >> 20;
                                if mb != last.swap(mb, std::sync::atomic::Ordering::Relaxed) {
                                    print!("\r  {mb} MB");
                                    let _ = std::io::Write::flush(&mut std::io::stdout());
                                }
                            }) {
                                Ok(_) => println!(),
                                Err(e) => anyhow::bail!("the NVIDIA add-on could not be downloaded: {e:#}"),
                            }
                        }
                    },
                    _ => {}
                }
            }
            let t0 = std::time::Instant::now();
            let det = crate::ai::AiDetector::load_on(std::path::Path::new(&model), 4, !cpu && ai.gpu, ai.gpu_name.as_deref(), ai.gpu_memory_mb)?;
            println!("model loaded in {:?} (input {}x{}, on {})", t0.elapsed(), det.input_size(), det.input_size(), det.gpu_name().unwrap_or("the CPU"));
            if let Some(why) = crate::ai::gpu_note() {
                println!("not on the graphics card: {why}");
            }
            let bytes = std::fs::read(&image).context("could not read image")?;
            let runs = runs.max(1);
            for run in 0..runs {
                let t = std::time::Instant::now();
                let found = det.detect(&bytes, min_score)?;
                println!("run {}: {} objects in {:?}", run + 1, found.len(), t.elapsed());
                if run == runs - 1 {
                    for d in found {
                        println!("  {:<12} {:.2}  box x={:.3} y={:.3} w={:.3} h={:.3}", d.label, d.score, d.x, d.y, d.w, d.h);
                    }
                }
            }
            Ok(())
        }
        Commands::LprTest { models, min_score, threads, save, images } => {
            let dir = std::path::PathBuf::from(&models);
            if !crate::lpr::models_present(&dir) {
                println!("downloading the plate models ...");
                crate::lpr::download_models(&dir, &|_, _| {})?;
            }
            let t0 = std::time::Instant::now();
            let m = crate::lpr::LprModels::load(&dir, threads)?;
            println!("models loaded in {:?}", t0.elapsed());
            let (mut plates, mut detect_total, mut read_total) = (0u32, std::time::Duration::ZERO, std::time::Duration::ZERO);
            for path in &images {
                let img = image::open(path).with_context(|| format!("could not open {path}"))?.to_rgb8();
                // The first run warms the runtime up; the second is timed.
                m.detect(&img, min_score)?;
                let t = std::time::Instant::now();
                let found = m.detect(&img, min_score)?;
                let detect = t.elapsed();
                detect_total += detect;
                println!("{path} ({}x{}): {} plate(s) found in {:?}", img.width(), img.height(), found.len(), detect);
                for (n, b) in found.iter().enumerate() {
                    let t = std::time::Instant::now();
                    let r = m.read(&img, b)?;
                    let took = t.elapsed();
                    read_total += took;
                    plates += 1;
                    match r {
                        Some(r) => println!("  plate {}: {} (confidence {:.2}, detector {:.2}) {}px wide, read in {:?}", n + 1, r.text, r.confidence, b.score, b.w.round(), took),
                        None => println!("  plate {}: unreadable ({}px wide)", n + 1, b.w.round()),
                    }
                    if save {
                        std::fs::write(dir.join(format!("plate-{}.jpg", plates)), crate::lpr::plate_jpeg(&img, b, 240)?)?;
                    }
                }
            }
            if plates > 0 {
                println!("per plate: detection {:?} + reading {:?} (CPU, {threads} threads)", detect_total / plates, read_total / plates);
            }
            Ok(())
        }
        Commands::TalkTest { config, camera, seconds, silence: _, wav } => {
            let cfg = Config::load(&config)?;
            let cam = cfg.cameras.iter().find(|c| c.name.eq_ignore_ascii_case(&camera)).context("camera not found in config")?;
            crate::talk::run_test(&cam.url, &cam.username, &cam.password, seconds, wav.as_deref()).await
        }
        Commands::IngestTest { config, camera, seconds, sub_url } => {
            let cfg = Config::load(&config)?;
            let mut cam = cfg
                .cameras
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&camera))
                .context("camera not found in config")?
                .clone();
            engine::normalize_creds(&mut cam);
            let mut targets = vec![("main", cam.url.clone())];
            if let Some(u) = sub_url.or(cam.sub_url.clone()) {
                targets.push(("sub", u));
            }
            for (label, url) in targets {
                let src = crate::ingest::StreamSource {
                group: None,
                    url: url.clone(),
                    username: cam.username.clone(),
                    password: cam.password.clone(),
                };
                match crate::ingest::probe(&src, seconds).await {
                    Ok(r) => println!("{}", crate::ingest::format_report(
                        &format!("{} {} ({})", cam.name, label, engine::redact_url(url)), &r)),
                    Err(e) => println!("== {} {}: FAILED: {:#}", cam.name, label, e),
                }
            }
            Ok(())
        }
        Commands::DecodeTest { config, camera, seconds, sub_url, hwaccel, fps, scale, every, out } => {
            let cfg = Config::load(&config)?;
            let mut cam = cfg
                .cameras
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(&camera))
                .context("camera not found in config")?
                .clone();
            engine::normalize_creds(&mut cam);
            let url = sub_url.or(cam.sub_url.clone()).unwrap_or_else(|| cam.url.clone());
            let src = crate::ingest::StreamSource {
                group: None,
                url,
                username: cam.username.clone(),
                password: cam.password.clone(),
            };
            decode_test(&src, seconds, hwaccel, fps, scale, every.max(1), &out).await
        }
    }
}

/// Start the engine and web server and serve until it fails.
/// Resolves on Ctrl-C, or SIGTERM on Unix (systemd/launchd stop).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Run the server until `shutdown` resolves, then stop the cameras cleanly
/// (TEARDOWN to each camera, recordings finished) before returning.
/// Keep other accounts out of the data folder, the settings and (Windows
/// service) the program folder, and out of recording folders switched to
/// private. Nothing here waits: the work is on a background thread, and a
/// failure is only logged.
fn protect_folders(cfg: &Config, config: &str) {
    #[cfg(unix)]
    {
        // (A brand-new data folder is made now, so it is shut to others at this first start, not the next.)
        let _ = std::fs::create_dir_all(&cfg.storage.data_dir);
        crate::lockdown::harden_unix(std::path::Path::new(config), &cfg.storage.data_dir);
    }
    #[cfg(windows)]
    let _ = config;
    let recording: Vec<std::path::PathBuf> = cfg.storage.folders.iter().filter(|f| f.private == Some(true)).map(|f| f.path.clone()).collect();
    if cfg!(unix) || crate::update::under_service() {
        crate::lockdown::start_in_background(&cfg.storage.data_dir, &recording);
    }
    if cfg!(windows) && crate::update::under_service() {
        if let Some(p) = crate::ffmpeg_bin::existing() {
            crate::lockdown::share_ffmpeg_in_background(&cfg.storage.data_dir, p);
        }
    }
}

pub async fn run_server(config: String) -> Result<()> {
    run_server_until(config, shutdown_signal()).await
}

pub async fn run_server_until(config: String, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
    let mut cfg = Config::load(&config)?;
    // Files a finished update left behind (Windows: the old program).
    crate::update::cleanup_after_update(&cfg.storage.data_dir);
    // Find ffmpeg (or download it once on Mac/Windows) before anything needs it.
    let data_dir = cfg.storage.data_dir.clone();
    tokio::task::spawn_blocking(move || crate::ffmpeg_bin::ensure(&data_dir)).await??;
    if cfg.cameras.is_empty() {
        println!("no cameras yet - add them from the web page (Settings > Cameras > Find cameras)");
    }
    // Never keep sign-in passwords in plain text.
    if crate::auth::hash_config_passwords(&mut cfg) {
        cfg.save(&config)?;
        println!("stored user passwords as secure hashes in {config}");
    }
    if crate::auth::accounts(&cfg).is_empty() && !matches!(cfg.server.bind.as_str(), "127.0.0.1" | "::1" | "localhost") {
        let msg = "no user accounts: ANYONE who can reach this server can watch every camera and change every setting (including commands it runs). Add an account in Settings > Users & security (or [[server.users]] in the config) before using it on a shared network.";
        println!("WARNING: {msg}");
        tracing::warn!("{msg}");
    }
    for w in crate::web::trusted_proxy_warnings(&cfg.server.trusted_proxies) {
        println!("WARNING: {w}");
        tracing::warn!("{w}");
    }
    protect_folders(&cfg, &config);
    crate::links::remember_name(&cfg);
    let engine = Engine::start(cfg.clone(), config.clone())?;
    let bind = bind_addr(&cfg.server.bind, cfg.server.port);
    let app = web::build_router(engine.clone());
    crate::rtmp::start(engine.clone());
    crate::restream::start(engine.clone());
    crate::transcode::probe_in_background();
    crate::remote::start(engine.clone());
    let e = engine.clone();
    crate::update::spawn_checker(move || e.config.lock().unwrap().update.clone());
    // Stop on the caller's signal, or when an installed update restarts us.
    let shutdown = async {
        tokio::select! {
            _ = shutdown => {}
            _ = crate::update::restart_requested() => {}
        }
    };
    // (Settings compares what is saved with this to tell whether a restart is needed.)
    crate::tls::record_started(&cfg.server);
    if cfg.server.tls {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = crate::tls::init(&cfg.server, &cfg.storage.data_dir)?;
        let listener = web_listener(&bind).await?;
        println!("AmberSight listening on https://{bind} (plain http on the same port redirects to it)");
        // Re-check the certificate regularly: new network address, new name, nearing expiry.
        let (eng, rt2) = (engine.clone(), rt.clone());
        let renew = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                let (server, dir) = {
                    let c = eng.config.lock().unwrap();
                    (c.server.clone(), c.storage.data_dir.clone())
                };
                let rt3 = rt2.clone();
                let r = tokio::task::spawn_blocking(move || rt3.refresh(&server, &dir)).await;
                if let Ok(Err(e)) = r {
                    tracing::warn!("HTTPS certificate check failed: {e:#}");
                }
            }
        });
        tokio::select! {
            r = crate::dualserve::serve(listener, app, rt) => r?,
            _ = shutdown => {}
        }
        renew.abort();
    } else {
        // Client addresses are needed for the sign-in lockout and audit log.
        let svc = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
        println!("AmberSight listening on http://{bind}");
        let listener = web_listener(&bind).await?;
        tokio::select! {
            r = axum::serve(listener, svc) => r?,
            _ = shutdown => {}
        }
    }
    // Demo sign-ins not yet in the audit log.
    crate::web::flush_demo_audit(&engine);
    // What admins were to hear goes out first: a restart must not hide it.
    let e = engine.clone();
    tokio::task::spawn_blocking(move || crate::account_http::finish_alerts(&e, std::time::Duration::from_secs(10))).await.ok();
    println!("AmberSight stopping: closing camera sessions...");
    let e = engine.clone();
    tokio::task::spawn_blocking(move || e.shutdown(std::time::Duration::from_secs(8))).await.ok();
    if crate::update::restart_pending() {
        // An error exit makes the service manager start it again: the new
        // version after an update, or new settings after a restart from
        // Settings (Windows recovery actions, systemd Restart=on-failure, launchd).
        anyhow::bail!("stopped to restart (an update or a restart from Settings); the service manager starts AmberSight again");
    }
    Ok(())
}

async fn decode_test(
    src: &crate::ingest::StreamSource,
    seconds: u64,
    hwaccel: Option<String>,
    fps: u32,
    scale: u32,
    every: u64,
    out: &str,
) -> Result<()> {
    use crate::ingest::decode::{Codec, FrameDecoder};
    use futures::StreamExt;
    use retina::client::{Credentials, PlayOptions, Session, SessionOptions, SetupOptions, Transport};
    use retina::codec::{CodecItem, FrameFormat, ParametersRef};
    use std::time::{Duration, Instant};

    std::fs::create_dir_all(out)?;
    let url = url::Url::parse(&src.url).context("invalid stream URL")?;
    let creds = (!src.username.is_empty())
        .then(|| Credentials { username: src.username.clone(), password: src.password.clone() });
    let mut session = Session::describe(url, SessionOptions::default().creds(creds))
        .await
        .map_err(|e| anyhow::anyhow!("DESCRIBE failed: {e}"))?;
    let idx = session
        .streams()
        .iter()
        .position(|s| s.media() == "video" && matches!(s.encoding_name(), "h264" | "h265"))
        .context("no H.264/H.265 video stream offered")?;
    let codec = if session.streams()[idx].encoding_name() == "h264" { Codec::H264 } else { Codec::H265 };
    session
        .setup(
            idx,
            SetupOptions::default()
                .transport(Transport::Tcp(Default::default()))
                .frame_format(FrameFormat::MP4),
        )
        .await
        .map_err(|e| anyhow::anyhow!("SETUP failed: {e}"))?;
    let playing = session
        .play(PlayOptions::default().initial_timestamp(retina::client::InitialTimestampPolicy::Permissive))
        .await
        .map_err(|e| anyhow::anyhow!("PLAY failed: {e}"))?;
    let mut demuxed = playing.demuxed().map_err(|e| anyhow::anyhow!("demux setup failed: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut decoder: Option<FrameDecoder> = None;
    let (mut frames_in, mut written, mut decoded) = (0u64, 0u64, 0u64);
    let mut first_jpeg: Option<Instant> = None;
    let mut last_jpeg = Instant::now();
    let started = Instant::now();

    let mut drain = |d: &FrameDecoder, decoded: &mut u64, written: &mut u64| -> Result<()> {
        while let Ok(jpeg) = d.frames().try_recv() {
            *decoded += 1;
            first_jpeg.get_or_insert_with(Instant::now);
            last_jpeg = Instant::now();
            if (*decoded - 1) % every == 0 {
                std::fs::write(format!("{out}/frame_{:05}.jpg", *decoded), &jpeg)?;
                *written += 1;
            }
        }
        Ok(())
    };

    loop {
        let item = match tokio::time::timeout_at(deadline, demuxed.next()).await {
            Err(_) | Ok(None) => break,
            Ok(Some(Err(e))) => return Err(anyhow::anyhow!("stream error: {e}")),
            Ok(Some(Ok(item))) => item,
        };
        let CodecItem::VideoFrame(f) = item else { continue };
        let extra = match demuxed.streams()[idx].parameters() {
            Some(ParametersRef::Video(v)) => Some(v.extra_data().to_vec()),
            _ => None,
        };
        if decoder.is_none() {
            let Some(extra) = extra else { continue };
            let d = FrameDecoder::start(codec, &extra, fps, scale, hwaccel.clone())?;
            decoder = Some(d);
        } else if f.has_new_parameters() {
            if let (Some(d), Some(extra)) = (decoder.as_mut(), extra) {
                d.set_extra_data(&extra)?;
            }
        }
        let d = decoder.as_mut().expect("decoder started above");
        frames_in += 1;
        d.push(f.data(), f.is_random_access_point())?;
        drain(d, &mut decoded, &mut written)?;
    }
    let wall = started.elapsed().as_secs_f64();
    let cpu = decoder.as_ref().and_then(|d| d.pid()).and_then(proc_cpu_seconds);
    let hw_state = decoder.as_ref().map(|d| (d.using_hwaccel(), d.hw_fallback_used()));
    if let Some(mut d) = decoder {
        drain(&d, &mut decoded, &mut written)?;
        let st = d.stats();
        let alive = d.is_running();
        d.stop();
        println!(
            "frames in (retina): {frames_in}  accepted: {}  dropped: {}  decoded JPEGs: {}  jpeg dropped: {}  ffmpeg emitted: {}  ffmpeg alive at end: {alive}  written: {written}",
            st.frames_in, st.frames_dropped, decoded, st.jpegs_dropped, st.jpegs_out
        );
        println!("time to first JPEG: {:.2}s", first_jpeg.map(|t| t.duration_since(started).as_secs_f64()).unwrap_or(f64::NAN));
        let active = first_jpeg.map(|t| last_jpeg.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        if decoded > 1 && active > 0.0 {
            println!(
                "decoded fps: {:.2} (target {fps}; {decoded} frames over {active:.1}s)",
                (decoded - 1) as f64 / active
            );
        }
    } else {
        println!("no video frames / parameters received");
    }
    if let Some((hw_active, fell_back)) = hw_state {
        println!("hwaccel requested: {hwaccel:?}  active at end: {hw_active}  fell back to software: {fell_back}");
    }
    if let Some(c) = cpu {
        println!("ffmpeg cpu time: {c:.2}s over {wall:.1}s wall = {:.1}% of one core", c / wall * 100.0);
    }
    println!("samples in {out}");
    Ok(())
}

/// User+system CPU seconds of a process (Linux only).
fn proc_cpu_seconds(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    // Fields after the comm: state is f[0]; utime/stime are overall fields 14/15.
    let ticks = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}



/// `host:port` to listen on; an IPv6 address gets brackets (`[::]:8090`).
fn bind_addr(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

/// The web server's listening socket. On Windows its send buffer is raised
/// to 1 MB (connections accepted from it inherit that): with the default
/// 64 KB, one download to a phone or another site 30 ms away tops out near
/// 90 Mbps (measured: 1 MB gives ~240, the link's own limit). Linux and
/// macOS size it by themselves.
async fn web_listener(bind: &str) -> anyhow::Result<tokio::net::TcpListener> {
    let mut last = None;
    for addr in tokio::net::lookup_host(bind).await? {
        match listen_on(addr) {
            Ok(l) => return Ok(l),
            Err(e) => last = Some(e),
        }
    }
    Err(last.map(anyhow::Error::from).unwrap_or_else(|| anyhow::anyhow!("{bind}: no address to listen on")))
}

fn listen_on(addr: std::net::SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let sock = socket2::Socket::new(socket2::Domain::for_address(addr), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    #[cfg(not(windows))]
    sock.set_reuse_address(true)?;
    #[cfg(windows)]
    {
        let _ = sock.set_send_buffer_size(1 << 20);
    }
    sock.bind(&addr.into())?;
    sock.listen(1024)?;
    sock.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(sock.into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn bind_address_brackets_ipv6() {
        assert_eq!(super::bind_addr("0.0.0.0", 8090), "0.0.0.0:8090");
        assert_eq!(super::bind_addr("::", 8090), "[::]:8090");
        assert_eq!(super::bind_addr("[::1]", 8090), "[::1]:8090");
        assert_eq!(super::bind_addr("nvr.lan", 80), "nvr.lan:80");
    }
}
