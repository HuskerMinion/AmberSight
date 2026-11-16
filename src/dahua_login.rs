//! Adding a Dahua-style camera (Dahua, Amcrest and the makers that sell their cameras under
//! another name) when ONVIF will not take the login but the stream will.
//!
//! On these cameras ONVIF may be off, or may want its own user, while the video stream signs in
//! with the web user. Find cameras tries ONVIF first; when that fails it asks for the Dahua
//! stream path (`/cam/realmonitor?channel=1&subtype=0`) with the same login. If the camera
//! answers, the camera can be added by its stream address alone.
//!
//! Tested here: two Amcrest cameras (IP8M-2496E-V2). Other makers that sell Dahua hardware under
//! their own name (Lorex, Empiretech and so on) answer the same path as far as their owners
//! report, but we have not run them ourselves.

use std::time::Duration;

use serde_json::{json, Value};

/// How a stream check with one login ended.
#[derive(Debug, Clone, PartialEq)]
pub enum RtspTry {
    /// The camera sent the stream description.
    Works,
    /// The camera refused the login (HTTP 401 / "Unauthorized").
    LoginRefused,
    /// The path is not there (404 and the like): not a Dahua-style camera, or another port.
    NoSuchPath,
    /// No answer at all (closed port, timeout).
    Unreachable,
}

pub const MAIN_PATH: &str = "/cam/realmonitor?channel=1&subtype=0";
pub const SUB_PATH: &str = "/cam/realmonitor?channel=1&subtype=1";
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(6);

fn host_part(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// The stream addresses on a host (no login in them).
pub fn stream_urls(host: &str) -> (String, String) {
    let h = host_part(host);
    (format!("rtsp://{h}:554{MAIN_PATH}"), format!("rtsp://{h}:554{SUB_PATH}"))
}

/// What an RTSP answer's status means (None: not a status we know).
fn classify_status(status: u16) -> Option<RtspTry> {
    match status {
        401 => Some(RtspTry::LoginRefused),
        400 | 403 | 404 | 454 => Some(RtspTry::NoSuchPath),
        _ => None,
    }
}

/// What a failed RTSP description means, from its text. Only the first
/// line is read: retina adds the connection's addresses, ports and
/// timestamps after it, whose digits can read as "401" or "404".
pub fn classify_error(msg: &str) -> RtspTry {
    let first = msg.split("\n\nconn:").next().unwrap_or(msg);
    let first = first.lines().next().unwrap_or(first);
    let m = first.to_ascii_lowercase();
    if m.contains("401") || m.contains("unauthorized") {
        RtspTry::LoginRefused
    } else if m.contains("404") || m.contains("not found") || m.contains("454") || m.contains("400") || m.contains("403") {
        RtspTry::NoSuchPath
    } else {
        RtspTry::Unreachable
    }
}

async fn describe(url: &str, user: &str, pass: &str) -> RtspTry {
    let Ok(url) = url::Url::parse(url) else { return RtspTry::Unreachable };
    let opts = retina::client::SessionOptions::default()
        .creds(Some(retina::client::Credentials { username: user.to_string(), password: pass.to_string() }))
        .user_agent(format!("AmberSight/{}", env!("CARGO_PKG_VERSION")));
    match tokio::time::timeout(DESCRIBE_TIMEOUT, retina::client::Session::describe(url, opts)).await {
        Err(_) => RtspTry::Unreachable,
        Ok(Err(e)) => e.status_code().and_then(classify_status).unwrap_or_else(|| classify_error(&e.to_string())),
        Ok(Ok(_)) => RtspTry::Works,
    }
}

/// Ask for the camera's main stream with this login (one DESCRIBE).
pub async fn try_rtsp(host: &str, user: &str, pass: &str) -> RtspTry {
    describe(&stream_urls(host).0, user, pass).await
}

/// `key=value` lines as Dahua's CGI answers them.
pub fn parse_kv(body: &str, key: &str) -> Option<String> {
    body.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().chars().filter(|c| !c.is_control()).take(64).collect::<String>()).filter(|v| !v.is_empty())
    })
}

/// The web interface's addresses to try, in order: plain HTTP, then HTTPS (a camera with HTTP
/// turned off, or one that sends HTTP on to HTTPS).
fn web_bases(host: &str) -> [String; 2] {
    let h = host_part(host);
    [format!("http://{h}"), format!("https://{h}")]
}

/// Whether an answer over plain HTTP means "ask over HTTPS instead": port 80 refused, a
/// TLS-only or otherwise garbled server there, or a redirect. Not a timeout: a camera that does
/// not answer at all would only cost another full wait over HTTPS.
fn try_https_after(answer: &anyhow::Result<u16>) -> bool {
    match answer {
        Err(e) => !is_timeout(&e.to_string()),
        Ok(s) => (300..400).contains(s),
    }
}

/// An HTTP request error that says the device did not answer in time (ureq's own timeouts, or
/// the system's: "timed out", Windows' error 10060).
fn is_timeout(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("timeout: ") || e.contains("timed out") || e.contains("os error 10060") || e.contains("did not properly respond")
}

/// True when an unanswered-login probe says this is a Dahua-style device: its web interface
/// answers 401 with a Digest challenge whose realm reads "Login to <serial>" (Dahua, Amcrest and
/// the brands that sell their hardware). No password is sent. Over HTTPS the camera's own
/// (self-signed) certificate is accepted, as for every request to a camera (camreboot); nothing
/// secret goes over it. Blocking; false on any error.
pub fn looks_dahua(host: &str) -> bool {
    for base in web_bases(host) {
        let url = format!("{base}/cgi-bin/magicBox.cgi?action=getDeviceType");
        let r = crate::camreboot::probe(&url);
        if let Ok((401, www)) = &r {
            return www.iter().any(|h| is_dahua_challenge(h));
        }
        if !try_https_after(&r.as_ref().map(|(s, _)| *s).map_err(|e| anyhow::anyhow!("{e}"))) {
            return false;
        }
    }
    false
}

/// A Digest challenge with a "Login to ..." realm.
pub fn is_dahua_challenge(header: &str) -> bool {
    crate::camreboot::parse_challenge(header).is_some_and(|c| c.realm.to_ascii_lowercase().starts_with("login to"))
}

/// (maker, model) from the camera's web interface, with the login that just worked on the
/// stream. Blocking; empty strings when it does not answer. One request each; a refused login
/// counts once.
pub fn identity(host: &str, user: &str, pass: &str) -> (String, String) {
    for base in web_bases(host) {
        let ask = |action: &str| {
            let url = format!("{base}/cgi-bin/magicBox.cgi?action={action}");
            crate::camreboot::request("GET", &url, None, &crate::camreboot::Auth::Digest(user, pass))
        };
        let first = ask("getDeviceType");
        let model = match &first {
            Ok(a) if a.status == 200 => parse_kv(&a.body, "type").unwrap_or_default(),
            _ => String::new(),
        };
        if model.is_empty() {
            // HTTPS only when plain HTTP did not answer or sent it on (a refused login is not
            // tried again over HTTPS).
            if try_https_after(&first.map(|a| a.status)) {
                continue;
            }
            return (String::new(), String::new());
        }
        // Only ask the second question when the first was answered (the login works).
        let vendor = match ask("getVendor") {
            Ok(a) if a.status == 200 => parse_kv(&a.body, "vendor").unwrap_or_default(),
            _ => String::new(),
        };
        return (vendor, model);
    }
    (String::new(), String::new())
}

/// What Find cameras returns for a camera added by its stream address: the same shape as an
/// ONVIF inspection, with no profiles. Never holds a login.
pub fn inspection(host: &str, username: &str, vendor: &str, model: &str) -> Value {
    let (main, sub) = stream_urls(host);
    let name = if !model.is_empty() { model.to_string() } else { host.to_string() };
    let cam = json!({ "name": name, "url": main, "sub_url": sub, "username": username });
    json!({
        "info": { "manufacturer": vendor, "model": model, "firmware": "", "serial": "", "hardware_id": "" },
        "media_xaddr": null,
        "ptz_xaddr": null,
        "profiles": [],
        "main": null,
        "sub": null,
        "suggested": cam.clone(),
        "cameras": [cam],
        "note": "Signed in over RTSP only: the camera did not take this login over ONVIF, so ONVIF events and PTZ are off for it. The stream address is the Dahua-style one (sub stream = subtype=1).",
    })
}

/// What went wrong, in words for the person adding the camera. `onvif_auth`: ONVIF refused the
/// login; `onvif_error`: what ONVIF said otherwise; `rtsp`: the stream check (None = not tried).
pub fn failure_text(onvif_auth: bool, onvif_error: &str, rtsp: Option<&RtspTry>) -> String {
    let hint = "On Dahua-style cameras (Dahua, Amcrest and some other brands), ONVIF might need to be turned on in the camera, or might need its own user.";
    match (onvif_auth, rtsp) {
        (true, Some(RtspTry::LoginRefused)) => format!("The camera refused this login, both over ONVIF and for its video stream. Check the user name and password. {hint}"),
        (true, _) => format!("ONVIF login refused. {hint}"),
        (false, Some(RtspTry::LoginRefused)) => format!("ONVIF failed ({onvif_error}) and the camera refused this login for its video stream. Check the user name and password."),
        (false, _) => format!("ONVIF failed: {onvif_error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_have_no_login_and_bracket_v6() {
        let (m, s) = stream_urls("192.0.2.5");
        assert_eq!(m, "rtsp://192.0.2.5:554/cam/realmonitor?channel=1&subtype=0");
        assert_eq!(s, "rtsp://192.0.2.5:554/cam/realmonitor?channel=1&subtype=1");
        assert!(stream_urls("2001:db8::5").0.starts_with("rtsp://[2001:db8::5]:554/"));
    }

    #[test]
    fn errors_are_told_apart() {
        assert_eq!(classify_error("RTSP error: Received status 401 response to DESCRIBE"), RtspTry::LoginRefused);
        assert_eq!(classify_error("404 response to DESCRIBE CSeq=3: Unexpected RTSP response status"), RtspTry::NoSuchPath);
        assert_eq!(classify_error("Connection refused (os error 111)"), RtspTry::Unreachable);
        // The connection details retina adds (ports, nanosecond times) are not read.
        let msg = "Unable to connect to RTSP server: Connection refused (os error 111)\n\nconn: 127.0.0.1:40401(me)->127.0.0.1:55404@2026-11-16T10:00:00.140401000";
        assert_eq!(classify_error(msg), RtspTry::Unreachable);
        assert_eq!(classify_status(401), Some(RtspTry::LoginRefused));
        assert_eq!(classify_status(454), Some(RtspTry::NoSuchPath));
        assert_eq!(classify_status(500), None);
    }

    #[test]
    fn only_a_login_to_digest_realm_counts_as_dahua() {
        assert!(is_dahua_challenge("Digest realm=\"Login to AMC06554E72FFDC7E6\", qop=\"auth\", nonce=\"1\", opaque=\"x\""));
        assert!(!is_dahua_challenge("Digest realm=\"IP Camera\", nonce=\"1\""));
        assert!(!is_dahua_challenge("Basic realm=\"Login to X\""));
    }

    #[test]
    fn https_is_tried_only_when_http_does_not_answer_or_redirects() {
        assert!(try_https_after(&Err(anyhow::anyhow!("http://192.0.2.5/x: io: Connection refused (os error 111)"))));
        assert!(try_https_after(&Err(anyhow::anyhow!("http://192.0.2.5/x: protocol: invalid HTTP version"))));
        // No answer at all: HTTPS would only wait again.
        assert!(!try_https_after(&Err(anyhow::anyhow!("http://192.0.2.5/x: timeout: global"))));
        assert!(!try_https_after(&Err(anyhow::anyhow!("http://192.0.2.5/x: timeout: connect"))));
        assert!(!try_https_after(&Err(anyhow::anyhow!("http://192.0.2.5/x: io: Connection timed out (os error 110)"))));
        assert!(!try_https_after(&Err(anyhow::anyhow!(
            "http://192.0.2.5/x: io: A connection attempt failed because the connected party did not properly respond after a period of time (os error 10060)"
        ))));
        assert!(try_https_after(&Ok(301)) && try_https_after(&Ok(302)) && try_https_after(&Ok(307)));
        assert!(!try_https_after(&Ok(401)) && !try_https_after(&Ok(404)) && !try_https_after(&Ok(200)));
        assert_eq!(web_bases("192.0.2.5"), ["http://192.0.2.5".to_string(), "https://192.0.2.5".to_string()]);
    }

    #[test]
    fn cgi_answers_are_read_by_key() {
        assert_eq!(parse_kv("type=IPC-T54IR-ZE-S3\r\n", "type").as_deref(), Some("IPC-T54IR-ZE-S3"));
        assert_eq!(parse_kv("vendor=Amcrest\nx=1", "vendor").as_deref(), Some("Amcrest"));
        assert_eq!(parse_kv("type=\n", "type"), None);
        assert_eq!(parse_kv("other=1", "type"), None);
    }

    #[test]
    fn the_inspection_has_the_stream_pair_and_no_password() {
        let v = inspection("192.0.2.5", "viewer", "Amcrest", "IPC-X");
        assert_eq!(v["suggested"]["url"], "rtsp://192.0.2.5:554/cam/realmonitor?channel=1&subtype=0");
        assert_eq!(v["suggested"]["sub_url"], "rtsp://192.0.2.5:554/cam/realmonitor?channel=1&subtype=1");
        assert_eq!(v["suggested"]["name"], "IPC-X");
        assert_eq!(v["cameras"].as_array().unwrap().len(), 1);
        assert_eq!(v["info"]["model"], "IPC-X");
        assert!(v["suggested"].get("onvif_port").is_none());
        assert!(!v.to_string().to_ascii_lowercase().contains("password"));
    }

    #[test]
    fn failure_texts_say_what_failed() {
        let t = failure_text(true, "", None);
        assert!(t.starts_with("ONVIF login refused.") && t.contains("might need to be turned on") && t.contains("its own user"), "{t}");
        let t = failure_text(true, "", Some(&RtspTry::LoginRefused));
        assert!(t.contains("both over ONVIF and for its video stream"), "{t}");
        let t = failure_text(true, "", Some(&RtspTry::NoSuchPath));
        assert!(t.starts_with("ONVIF login refused."), "{t}");
        assert_eq!(failure_text(false, "HTTP 500", None), "ONVIF failed: HTTP 500");
    }

    /// A camera whose RTSP server answers 401 for a wrong login is told apart from one with no
    /// such path and from one that does not answer.
    #[tokio::test]
    async fn rtsp_answers_are_classified_live() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, want) in [("401 Unauthorized", RtspTry::LoginRefused), ("404 Not Found", RtspTry::NoSuchPath)] {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                loop {
                    let Ok((mut s, _)) = l.accept().await else { return };
                    tokio::spawn(async move {
                        let mut b = vec![0u8; 4096];
                        while let Ok(n) = s.read(&mut b).await {
                            if n == 0 {
                                break;
                            }
                            // Answer with the request's own CSeq, as a camera does.
                            let req = String::from_utf8_lossy(&b[..n]).to_string();
                            let cseq = req
                                .lines()
                                .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("cseq")).map(|(_, v)| v.trim().to_string()))
                                .unwrap_or_else(|| "1".into());
                            let r = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\nWWW-Authenticate: Digest realm=\"x\", nonce=\"n\"\r\nContent-Length: 0\r\n\r\n");
                            if s.write_all(r.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            });
            let got = describe(&format!("rtsp://127.0.0.1:{port}{MAIN_PATH}"), "u", "p").await;
            assert_eq!(got, want);
        }
        // Nothing listening.
        let closed = crate::test_ports::closed_port();
        assert_eq!(describe(&format!("rtsp://127.0.0.1:{}{MAIN_PATH}", closed.port), "u", "p").await, RtspTry::Unreachable);
    }
}
