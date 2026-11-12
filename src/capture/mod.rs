//! AmberSight Capture: `ambersight capture` sends this computer's screen to an
//! AmberSight server when the server asks (docs/helper-protocol.md).
//!
//! The helper only connects out: a WebSocket control channel to
//! `/helper/ws`, and one `PUT /push/<key>` upload per running camera, all over
//! TLS pinned to the server's own CA. It only ever grabs the screen (never a
//! camera or microphone).

pub mod config;
mod ffmpeg;
pub mod net;
pub mod pair;
pub mod proto;
mod runner;
mod session;
mod sources;
#[cfg(windows)]
mod tray;
mod upload;
#[cfg(windows)]
mod win;

use anyhow::{anyhow, Context, Result};
use config::HelperConfig;
use futures::{SinkExt, StreamExt};
use session::{Backend, Session};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

const RECONNECT_START: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// The server pings every 30 s; silence this long means the link is dead.
const SILENCE_LIMIT: Duration = Duration::from_secs(90);
const CONFIG_CHECK: Duration = Duration::from_secs(10);

pub struct Opts {
    pub pair: Option<(String, String)>,
    pub unpair: bool,
    pub yes: bool,
    pub list: bool,
}

/// `ambersight capture ...`
pub async fn main(o: Opts) -> Result<()> {
    if o.list {
        #[cfg(windows)]
        win::dpi_aware();
        let s = tokio::task::spawn_blocking(sources::enumerate).await?;
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }
    if o.unpair {
        return unpair();
    }
    if let Some((server, code)) = o.pair {
        return pair_cmd(&server, &code, o.yes).await;
    }
    // Started hidden there is nobody to read an error: put it in capture.log.
    let r = run().await;
    if let Err(e) = &r {
        tracing::error!("capture: stopped: {e:#}");
    }
    r
}

async fn pair_cmd(server: &str, code: &str, yes: bool) -> Result<()> {
    let path = config::default_path()?;
    println!("Connecting to {server} ...");
    let (cfg, name) = pair::pair(server, code, |fp| {
        println!("\nServer certificate fingerprint (SHA-256):\n  {fp}\n");
        if yes {
            println!("Accepted (--yes).");
            return true;
        }
        println!("Check that it matches the fingerprint shown on the server's page.");
        print!("Does it match? [y/N] ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    })
    .await?;
    cfg.save(&path)?;
    println!("Paired as \"{name}\". Settings saved to {}", path.display());
    match pair::enable_autostart() {
        Ok(where_) => println!("Starts at sign-in: {where_}"),
        Err(e) => println!("Could not turn on start at sign-in: {e:#}"),
    }
    match start_in_background() {
        Ok(()) => println!("AmberSight Capture is now running in the background."),
        Err(e) => println!("Start it with: ambersight capture ({e:#})"),
    }
    Ok(())
}

fn unpair() -> Result<()> {
    let path = config::default_path()?;
    let had = path.exists();
    if had {
        std::fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
    }
    pair::disable_autostart()?;
    if had {
        println!("Removed {} and the start-at-sign-in entry.", path.display());
        println!("A running helper stops within a few seconds. Remove this computer on the server too (Settings > Cameras, under Computers sending their screen).");
    } else {
        println!("This computer was not paired; start at sign-in is off.");
    }
    Ok(())
}

/// Run `<exe> capture` detached from this terminal.
fn start_in_background() -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut c = std::process::Command::new(exe);
    c.arg("capture").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.env(win::HIDDEN_ENV, "1");
        // CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB (so closing the terminal does not end it).
        c.creation_flags(0x0800_0000 | 0x0100_0000);
        if c.spawn().is_ok() {
            return Ok(());
        }
        c.creation_flags(0x0800_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    c.spawn().map(|_| ()).map_err(Into::into)
}

/// Hold a lock so only one helper runs per user; waits a while for an
/// older one to exit (after re-pairing, the old one exits on its own).
fn single_instance(dir: &Path) -> Result<std::fs::File> {
    use fs4::fs_std::FileExt;
    std::fs::create_dir_all(dir)?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("capture.lock"))?;
    let until = Instant::now() + Duration::from_secs(45);
    loop {
        if f.try_lock_exclusive().unwrap_or(false) {
            return Ok(f);
        }
        if Instant::now() > until {
            anyhow::bail!("AmberSight Capture is already running for this user");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

async fn run() -> Result<()> {
    // Started at sign-in: continue without a console window.
    #[cfg(windows)]
    win::hide_console();
    let path = config::default_path()?;
    if !path.exists() {
        anyhow::bail!("this computer is not paired yet: run `ambersight capture --pair https://<server>:<port> <code>`");
    }
    let cfg = HelperConfig::load(&path)?;
    tracing::info!("capture: starting (pid {}, hidden {}, no desktop {})", std::process::id(), cfg!(windows) && std::env::var_os("AMBERSIGHT_CAPTURE_HIDDEN").is_some(), crate::local_source::in_session_zero());
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let _lock = tokio::task::spawn_blocking(move || single_instance(&dir)).await??;
    #[cfg(windows)]
    win::dpi_aware();
    let target = net::Target::parse(&cfg.server)?;
    let tls = net::pinned(&cfg.ca_pem)?;
    println!("AmberSight Capture: connecting to {} (Ctrl+C to stop)", cfg.server);
    // Testing aid: a synthetic lavfi pattern instead of the screen
    // (AMBERSIGHT_CAPTURE_TEST_INPUT=lavfi:testsrc2=size=640x360:rate=5).
    let input_override = std::env::var("AMBERSIGHT_CAPTURE_TEST_INPUT")
        .ok()
        .and_then(|v| v.strip_prefix("lavfi:").map(|spec| vec!["-re".to_string(), "-f".into(), "lavfi".into(), "-i".into(), spec.to_string()]));
    if input_override.is_some() {
        tracing::warn!("capture: using a test pattern instead of the screen (AMBERSIGHT_CAPTURE_TEST_INPUT)");
    }
    let backend = Arc::new(runner::Real { target, tls, input_override });
    control_loop(backend, cfg, LoopOpts { config_path: Some(path), tray: true }).await
}

pub struct LoopOpts {
    /// Exit when this file disappears or changes (unpaired / paired again).
    pub config_path: Option<PathBuf>,
    pub tray: bool,
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

enum ConnectError {
    /// The server refused this helper's credentials.
    Rejected(u16),
    Other(anyhow::Error),
}

async fn connect_ws(cfg: &HelperConfig, target: &net::Target, tls: Arc<rustls::ClientConfig>) -> Result<Ws, ConnectError> {
    use tokio_tungstenite::tungstenite::Error as WsError;
    let io = net::connect(target, tls).await.map_err(ConnectError::Other)?;
    let mut req = format!("wss://{}/helper/ws", target.authority).into_client_request().map_err(|e| ConnectError::Other(e.into()))?;
    let h = req.headers_mut();
    h.insert("Authorization", format!("Bearer {}", cfg.token).parse().map_err(|_| ConnectError::Other(anyhow!("bad token")))?);
    h.insert("X-Helper-Id", cfg.id.parse().map_err(|_| ConnectError::Other(anyhow!("bad helper id")))?);
    h.insert("User-Agent", format!("AmberSight-Capture/{}", proto::version()).parse().expect("ascii"));
    match tokio::time::timeout(net::CONNECT_TIMEOUT, tokio_tungstenite::client_async(req, io)).await {
        Err(_) => Err(ConnectError::Other(anyhow!("the server did not answer the control connection"))),
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(WsError::Http(r))) if matches!(r.status().as_u16(), 401 | 403) => Err(ConnectError::Rejected(r.status().as_u16())),
        Ok(Err(WsError::Http(r))) => Err(ConnectError::Other(anyhow!("the server answered HTTP {}", r.status()))),
        Ok(Err(e)) => Err(ConnectError::Other(e.into())),
    }
}

/// Stay connected to the server and do what it asks, until quit.
pub async fn control_loop<B: Backend>(backend: Arc<B>, cfg: HelperConfig, opts: LoopOpts) -> Result<()> {
    let target = net::Target::parse(&cfg.server)?;
    let tls = net::pinned(&cfg.ca_pem)?;
    let machine = proto::machine_name();
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let mut session = Session::new(backend, ev_tx, out_tx);

    #[cfg(windows)]
    let (tray_tx, mut tray_rx) = tokio::sync::mpsc::unbounded_channel::<tray::TrayCmd>();
    #[cfg(windows)]
    let tray = if opts.tray { tray::Tray::spawn(tray_tx) } else { None };
    #[cfg(not(windows))]
    let _ = opts.tray;
    #[cfg(not(windows))]
    let mut tray_rx = tokio::sync::mpsc::unbounded_channel::<TrayCmdStub>().1;
    let mut tray_shown = (false, false);

    let mut ws: Option<Ws> = None;
    let mut connected_at = Instant::now();
    let mut last_heard = Instant::now();
    let mut backoff = RECONNECT_START;
    let mut reconnect_at = tokio::time::Instant::now();
    let mut config_check = tokio::time::interval(CONFIG_CHECK);
    config_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let quit_reason: &str;

    loop {
        let silence = tokio::time::Instant::from_std(last_heard + SILENCE_LIMIT);
        tokio::select! {
            _ = tokio::time::sleep_until(reconnect_at), if ws.is_none() => {
                match connect_ws(&cfg, &target, tls.clone()).await {
                    Ok(mut s) => {
                        tracing::info!("capture: connected to {}", cfg.server);
                        // Anything queued while offline is stale.
                        while out_rx.try_recv().is_ok() {}
                        let hello = proto::hello(&cfg.id, &machine).to_string();
                        if s.send(Message::Text(hello)).await.is_ok() {
                            session.report_all();
                            connected_at = Instant::now();
                            last_heard = Instant::now();
                            ws = Some(s);
                            continue;
                        }
                        reconnect_at = tokio::time::Instant::now() + backoff;
                    }
                    Err(ConnectError::Rejected(code)) => {
                        tracing::warn!("capture: the server refused this helper (HTTP {code}); it was probably removed. Pair again with `ambersight capture --pair`.");
                        session.stop_all();
                        backoff = RECONNECT_MAX;
                        reconnect_at = tokio::time::Instant::now() + backoff;
                    }
                    Err(ConnectError::Other(e)) => {
                        tracing::warn!("capture: cannot connect ({e:#}); retrying in {}s", backoff.as_secs());
                        reconnect_at = tokio::time::Instant::now() + backoff;
                        backoff = (backoff * 2).min(RECONNECT_MAX);
                    }
                }
            }
            m = async { ws.as_mut().expect("guarded").next().await }, if ws.is_some() => {
                let alive = match m {
                    Some(Ok(Message::Text(t))) => { last_heard = Instant::now(); session.handle_text(&t); true }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => false,
                    Some(Ok(_)) => { last_heard = Instant::now(); true }
                };
                if !alive {
                    drop_connection(&mut ws, &mut backoff, &mut reconnect_at, connected_at, "the server closed the connection");
                }
            }
            _ = tokio::time::sleep_until(silence), if ws.is_some() => {
                drop_connection(&mut ws, &mut backoff, &mut reconnect_at, connected_at, "no word from the server");
            }
            Some(v) = out_rx.recv() => {
                if let Some(s) = ws.as_mut() {
                    if s.send(Message::Text(v.to_string())).await.is_err() {
                        drop_connection(&mut ws, &mut backoff, &mut reconnect_at, connected_at, "sending failed");
                    }
                }
            }
            Some(ev) = ev_rx.recv() => session.on_event(ev),
            Some(cmd) = tray_rx.recv() => {
                #[cfg(windows)]
                match cmd {
                    tray::TrayCmd::Pause => session.pause(),
                    tray::TrayCmd::Resume => session.resume(),
                    tray::TrayCmd::Quit => { quit_reason = "quit from the tray"; break; }
                }
                #[cfg(not(windows))]
                let _ = cmd;
            }
            _ = config_check.tick(), if opts.config_path.is_some() => {
                let p = opts.config_path.as_ref().expect("guarded");
                match HelperConfig::load(p) {
                    Err(_) if !p.exists() => { quit_reason = "unpaired"; break; }
                    Ok(c) if c != cfg => { quit_reason = "paired again"; break; }
                    _ => {}
                }
            }
        }
        let now = session.tray_state();
        if now != tray_shown {
            tray_shown = now;
            #[cfg(windows)]
            if let Some(t) = &tray {
                t.update(now.0, now.1);
            }
        }
    }

    tracing::info!("capture: exiting ({quit_reason})");
    session.stop_all();
    if let Some(mut s) = ws.take() {
        while let Ok(v) = out_rx.try_recv() {
            let _ = s.send(Message::Text(v.to_string())).await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), s.close(None)).await;
    }
    #[cfg(windows)]
    drop(tray);
    Ok(())
}

#[cfg(not(windows))]
enum TrayCmdStub {}

fn drop_connection(ws: &mut Option<Ws>, backoff: &mut Duration, reconnect_at: &mut tokio::time::Instant, connected_at: Instant, why: &str) {
    if ws.take().is_none() {
        return;
    }
    // A connection that held for a while resets the backoff.
    if connected_at.elapsed() > Duration::from_secs(60) {
        *backoff = RECONNECT_START;
    }
    tracing::warn!("capture: disconnected ({why}); reconnecting in {}s", backoff.as_secs());
    *reconnect_at = tokio::time::Instant::now() + *backoff;
    *backoff = (*backoff * 2).min(RECONNECT_MAX);
}

#[cfg(test)]
mod tests;
