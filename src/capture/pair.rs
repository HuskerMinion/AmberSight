//! Pairing with a server (one-time code) and start at sign-in.

use super::config::HelperConfig;
use super::net::{self, Target};
use super::proto;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct Registered {
    id: String,
    token: String,
    #[serde(default)]
    name: String,
}

/// The CA's SHA-256 fingerprint, formatted like `/api/tls` `ca_fingerprint_sha256`.
pub fn ca_fingerprint(pem: &str) -> Result<String> {
    Ok(crate::tls::fingerprint(net::ca_der(pem)?.as_ref()))
}

/// Fetch the server's CA, let `confirm` check its fingerprint, then register
/// over TLS pinned to that CA. Returns the config to save and the helper name.
pub async fn pair(server: &str, code: &str, confirm: impl FnOnce(&str) -> bool) -> Result<(HelperConfig, String)> {
    let t = Target::parse(server)?;
    let code = code.trim();
    anyhow::ensure!(!code.is_empty(), "the pairing code is empty");

    let (status, body) = net::request(&t, net::first_contact(), "GET", "/tls/ca.pem", None, b"")
        .await
        .with_context(|| format!("could not reach {}", t.base_url()))?;
    if status == 404 {
        bail!("the server has no certificate authority to download (it needs HTTPS with its own certificate authority)");
    }
    anyhow::ensure!(status == 200, "downloading the server certificate failed (HTTP {status})");
    let pem = String::from_utf8(body).map_err(|_| anyhow!("the server certificate is not text"))?;
    let fp = ca_fingerprint(&pem)?;
    if !confirm(&fp) {
        bail!("pairing canceled: the fingerprint was not confirmed");
    }

    let pinned = net::pinned(&pem)?;
    let machine = proto::machine_name();
    let req = serde_json::json!({"code": code, "machine": machine, "os": proto::os_name(), "version": proto::version()});
    let (status, body) = net::request(&t, pinned, "POST", "/helper/register", Some("application/json"), req.to_string().as_bytes())
        .await
        .context("registering with the server failed")?;
    match status {
        200 | 201 => {}
        401 => bail!("the server did not accept the code (wrong, already used, or expired): ask for a new one"),
        429 => bail!("too many wrong codes from this computer; wait a few minutes and try again"),
        404 => bail!("this server does not support capture helpers yet (update AmberSight on the server)"),
        s => bail!("the server refused the pairing (HTTP {s}): {}", String::from_utf8_lossy(&body).chars().take(200).collect::<String>()),
    }
    let r: Registered = serde_json::from_slice(&body).context("unexpected answer from the server")?;
    anyhow::ensure!(!r.id.is_empty() && !r.token.is_empty(), "the server sent an empty helper id or token");
    let name = if r.name.is_empty() { machine } else { r.name };
    Ok((HelperConfig { server: t.base_url(), id: r.id, token: r.token, ca_pem: pem }, name))
}

// ------------------------------------------------------------ autostart

#[cfg_attr(not(windows), allow(dead_code))] // Windows Run key + tests
pub const RUN_VALUE: &str = "AmberSight Capture";

/// The start-at-sign-in command line.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn autostart_command(exe: &std::path::Path) -> String {
    format!("\"{}\" capture", exe.display())
}

/// XDG autostart entry for `exe`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn desktop_entry(exe: &std::path::Path) -> String {
    // Desktop-entry Exec quoting: backslash-escape " ` $ \ inside double quotes.
    let mut q = String::new();
    for c in exe.display().to_string().chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            q.push('\\');
        }
        q.push(c);
    }
    format!(
        "[Desktop Entry]\nType=Application\nName=AmberSight Capture\nComment=Sends this screen to AmberSight when asked\nExec=\"{q}\" capture\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    )
}

#[cfg(not(windows))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn desktop_file() -> Result<std::path::PathBuf> {
    Ok(super::config::xdg_config_home()?.join("autostart").join("ambersight-capture.desktop"))
}

/// ~/Library/LaunchAgents/dev.ambersight.capture.plist
#[cfg(target_os = "macos")]
fn launch_agent_file() -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(std::path::PathBuf::from(home).join("Library/LaunchAgents/dev.ambersight.capture.plist"))
}

/// The launch agent that starts `exe capture` at sign-in.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launch_agent(exe: &std::path::Path) -> String {
    let x = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>dev.ambersight.capture</string>\n  <key>ProgramArguments</key><array><string>{}</string><string>capture</string></array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n</dict>\n</plist>\n",
        x(&exe.display().to_string())
    )
}

/// Turn on start at sign-in for this exe. Returns a description of what was set.
pub fn enable_autostart() -> Result<String> {
    let exe = std::env::current_exe().context("could not find this program's path")?;
    #[cfg(windows)]
    {
        let cmd = autostart_command(&exe);
        super::win::run_key_set(RUN_VALUE, &cmd)?;
        anyhow::ensure!(super::win::run_key_get(RUN_VALUE).as_deref() == Some(cmd.as_str()), "the start-at-sign-in entry did not stick");
        Ok(format!(r"HKCU\{}\{RUN_VALUE}", super::win::RUN_KEY))
    }
    #[cfg(target_os = "macos")]
    {
        // A per-user launch agent: starts the helper at sign-in, in the
        // signed-in session (screen capture needs that session).
        let f = launch_agent_file()?;
        if let Some(d) = f.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&f, launch_agent(&exe))?;
        Ok(f.display().to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let f = desktop_file()?;
        if let Some(d) = f.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&f, desktop_entry(&exe))?;
        Ok(f.display().to_string())
    }
}

pub fn disable_autostart() -> Result<()> {
    #[cfg(windows)]
    {
        super::win::run_key_delete(RUN_VALUE)
    }
    #[cfg(target_os = "macos")]
    {
        match std::fs::remove_file(launch_agent_file()?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match std::fs::remove_file(desktop_file()?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn autostart_lines() {
        let p = std::path::Path::new(r"C:\Program Files\AmberSight\ambersight.exe");
        assert_eq!(autostart_command(p), r#""C:\Program Files\AmberSight\ambersight.exe" capture"#);
        let d = desktop_entry(std::path::Path::new("/opt/amber sight/ambersight"));
        assert!(d.contains("\nExec=\"/opt/amber sight/ambersight\" capture\n"), "{d}");
        assert!(desktop_entry(std::path::Path::new("/a$b/x")).contains(r#"Exec="/a\$b/x" capture"#));
    }

    /// A TLS server with a fresh CA, answering like the AmberSight server.
    /// Returns (address, CA PEM, CA fingerprint, recorded requests).
    async fn fake_server(register_status: u16) -> (String, String, String, Arc<std::sync::Mutex<Vec<String>>>) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("amber-pair-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::tls::ensure_ca(&dir).unwrap();
        let ca = crate::tls::ensure_ca(&dir).unwrap(); // reloaded issuer, as the server does
        crate::tls::ensure_server_cert(&dir, &ca, &["127.0.0.1".to_string()], chrono::Utc::now().timestamp()).unwrap();
        let sc = crate::tls::server_config(&std::fs::read(dir.join("cert.pem")).unwrap(), &std::fs::read(dir.join("key.pem")).unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (pem, seen2) = (ca.pem.clone(), seen.clone());
        tokio::spawn(async move {
            let acc = tokio_rustls::TlsAcceptor::from(sc);
            loop {
                let Ok((tcp, _)) = l.accept().await else { break };
                let Ok(mut s) = acc.accept(tcp).await else { continue };
                let mut raw = Vec::new();
                let mut b = [0u8; 4096];
                loop {
                    let n = s.read(&mut b).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&b[..n]);
                    let text = String::from_utf8_lossy(&raw);
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head.lines().find_map(|l| l.strip_prefix("Content-Length: ")).and_then(|v| v.parse().ok()).unwrap_or(0usize);
                        if body.len() >= len {
                            break;
                        }
                    }
                }
                let req = String::from_utf8_lossy(&raw).into_owned();
                seen2.lock().unwrap().push(req.clone());
                let (status, body) = if req.starts_with("GET /tls/ca.pem ") {
                    (200, pem.clone())
                } else if req.starts_with("POST /helper/register ") {
                    if register_status == 200 {
                        (200, r#"{"id":"h-1","token":"tok-secret","name":"desk-pc"}"#.to_string())
                    } else {
                        (register_status, "no".to_string())
                    }
                } else {
                    (404, "not found".into())
                };
                let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });
        (addr, ca.pem, ca.fingerprint, seen)
    }

    #[tokio::test]
    async fn pairs_with_pinned_ca() {
        let (addr, pem, fp, seen) = fake_server(200).await;
        let mut shown = String::new();
        let (cfg, name) = pair(&format!("https://{addr}/"), " CODE123 ", |f| {
            shown = f.to_string();
            true
        })
        .await
        .unwrap();
        assert_eq!(shown, fp);
        assert_eq!(ca_fingerprint(&pem).unwrap(), fp);
        assert_eq!(cfg, HelperConfig { server: format!("https://{addr}"), id: "h-1".into(), token: "tok-secret".into(), ca_pem: pem.clone() });
        assert_eq!(name, "desk-pc");
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2);
        let body: serde_json::Value = serde_json::from_str(reqs[1].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["code"], "CODE123");
        assert_eq!(body["os"], proto::os_name());
        assert!(body["machine"].as_str().is_some_and(|m| !m.is_empty()));
        // The saved CA verifies this server.
        let t = Target::parse(&cfg.server).unwrap();
        assert!(net::request(&t, net::pinned(&cfg.ca_pem).unwrap(), "GET", "/x", None, b"").await.is_ok());
    }

    #[tokio::test]
    async fn refuses_unconfirmed_and_bad_codes() {
        let (addr, ..) = fake_server(401).await;
        let url = format!("https://{addr}");
        let e = pair(&url, "C", |_| false).await.unwrap_err().to_string();
        assert!(e.contains("canceled"), "{e}");
        let e = pair(&url, "C", |_| true).await.unwrap_err().to_string();
        assert!(e.contains("did not accept the code"), "{e}");
    }

    #[tokio::test]
    async fn pinned_rejects_other_ca() {
        let (addr, ..) = fake_server(200).await;
        let (_, other_pem, ..) = fake_server(200).await;
        let t = Target::parse(&format!("https://{addr}")).unwrap();
        let e = net::request(&t, net::pinned(&other_pem).unwrap(), "GET", "/tls/ca.pem", None, b"").await.unwrap_err();
        assert!(format!("{e:#}").contains("TLS handshake failed"), "{e:#}");
    }
}
