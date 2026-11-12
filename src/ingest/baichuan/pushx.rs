//! The battery-camera motion push listener ("pushx").
//!
//! Battery Reolink cameras (Argus) send their PIR motion as a **plaintext HTTP
//! POST** to `pushx.reolink.com` (port 80) — no cloud account needed, the post
//! is on the LAN. Point that hostname at this server with a local DNS override
//! and this listener catches every motion: the request path carries the
//! camera's UID (`POST /devices/<uid>/notifications`), which we map to the
//! configured camera and mark as motion. That wakes the on-demand (battery)
//! camera and, with trigger "camera"/"both", records the woken stream — the
//! hub-less, ~zero-extra-battery equivalent of a hub reporting motion.
//!
//! It is a tiny standalone HTTP/1.1 server on its own port (the cameras always
//! use port 80), deliberately separate from the main web server and its auth:
//! the camera is not an authenticated user, and this must not sit behind the
//! login gate. It only ever acknowledges; it exposes no data.

use crate::engine::Engine;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// The exact JSON body the camera expects back (with HTTP 200), else it treats
/// the push as failed. Confirmed against a real Argus 2.
const OK_BODY: &str = r#"{"status":{"code":0,"detail":"OK"},"data":{"sent":0}}"#;

/// Cap on the request body we read (the push is a small JSON alarm).
const MAX_BODY: usize = 64 * 1024;
/// Limits on what one request may cost: a line's length, the header count,
/// the whole request's time, and connections served at once (others are
/// closed at once). A camera's push is a few hundred bytes.
const MAX_LINE: u64 = 8 * 1024;
const MAX_HEADERS: usize = 32;
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
const MAX_IN_FLIGHT: usize = 16;
/// Connections at once from any one address that is not a camera's.
const MAX_PER_PEER: usize = 4;
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static PER_PEER: std::sync::Mutex<Vec<(std::net::IpAddr, usize)>> = std::sync::Mutex::new(Vec::new());

/// The configured battery cameras' addresses: always served, so other hosts
/// holding connections open cannot crowd out a real push.
fn camera_ips(engine: &Engine) -> Vec<std::net::IpAddr> {
    engine
        .config
        .lock()
        .unwrap()
        .cameras
        .iter()
        .filter_map(|c| c.url.strip_prefix("baichuan://")?.split(['/', '?', ':']).next()?.parse().ok())
        .collect()
}

/// Admit a connection from `peer`: a camera always; anyone else within the
/// overall and per-address limits. `Some(counted)` when admitted.
fn admit(engine: &Engine, peer: Option<std::net::IpAddr>) -> Option<bool> {
    let peer = peer.map(|p| p.to_canonical());
    if peer.is_some_and(|p| camera_ips(engine).contains(&p)) {
        return Some(false);
    }
    let p = peer?;
    let mut per = PER_PEER.lock().unwrap();
    let n = per.iter().find(|(ip, _)| *ip == p).map_or(0, |e| e.1);
    if n >= MAX_PER_PEER || IN_FLIGHT.load(Ordering::SeqCst) >= MAX_IN_FLIGHT {
        return None;
    }
    IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    match per.iter_mut().find(|(ip, _)| *ip == p) {
        Some(e) => e.1 += 1,
        None => per.push((p, 1)),
    }
    Some(true)
}

fn release(peer: Option<std::net::IpAddr>) {
    let Some(p) = peer.map(|p| p.to_canonical()) else { return };
    IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    let mut per = PER_PEER.lock().unwrap();
    if let Some(i) = per.iter().position(|(ip, _)| *ip == p) {
        per[i].1 -= 1;
        if per[i].1 == 0 {
            per.swap_remove(i);
        }
    }
}

/// A connection whose every read is bounded by one deadline for the whole
/// request, so a sender trickling a byte at a time cannot hold it open.
struct DeadlineStream {
    s: TcpStream,
    deadline: Instant,
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "request too slow"));
        }
        self.s.set_read_timeout(Some(left))?;
        self.s.read(buf)
    }
}

/// Start the listener in a background thread. Does nothing unless enabled.
pub fn start(engine: Arc<Engine>) {
    let cfg = engine.config.lock().unwrap().pushx.clone();
    if !cfg.enabled {
        return;
    }
    let listener = match TcpListener::bind(&cfg.listen) {
        Ok(l) => l,
        Err(e) => {
            // Port 80 needs privilege/capability; say so plainly.
            warn!("pushx: cannot listen on {} ({e}); battery-camera motion push is OFF (port 80 needs CAP_NET_BIND_SERVICE for a non-root service)", cfg.listen);
            return;
        }
    };
    info!("pushx: listening on {} for battery-camera motion push", cfg.listen);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let peer = stream.peer_addr().ok().map(|a| a.ip());
                    let Some(counted) = admit(&engine, peer) else {
                        drop(stream); // too many at once from there: refuse
                        continue;
                    };
                    let eng = engine.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = handle(&eng, stream) {
                            tracing::debug!("pushx: connection ended: {e}");
                        }
                        if counted {
                            release(peer);
                        }
                    });
                }
                Err(e) => tracing::debug!("pushx: accept failed: {e}"),
            }
        }
    });
}

/// The UID in `POST /devices/<uid>/notifications`. Tolerant of a query string
/// and of a missing/extra trailing segment.
pub fn parse_uid(path: &str) -> Option<&str> {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let rest = path.strip_prefix("/devices/")?;
    let uid = rest.split('/').next()?;
    (!uid.is_empty()).then_some(uid)
}

/// One line of at most MAX_LINE bytes, within the request's deadline.
fn read_line_capped(reader: &mut BufReader<DeadlineStream>) -> std::io::Result<String> {
    let mut buf = Vec::new();
    (&mut *reader).take(MAX_LINE).read_until(b'\n', &mut buf)?;
    if buf.len() as u64 >= MAX_LINE && !buf.ends_with(b"\n") {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line too long"));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Request text for a log line: printable ASCII only, at most 100 chars.
fn loggable(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_graphic()).take(100).collect()
}

fn handle(engine: &Arc<Engine>, stream: TcpStream) -> std::io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let peer_ip = stream.peer_addr().ok().map(|a| a.ip());
    let peer = peer_ip.map(|a| a.to_string()).unwrap_or_default();
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(DeadlineStream { s: stream, deadline: Instant::now() + REQUEST_DEADLINE });

    // Request line: "METHOD PATH HTTP/1.1".
    let line = read_line_capped(&mut reader)?;
    if line.is_empty() {
        return Ok(()); // peer closed without sending anything
    }
    let mut parts = line.split_whitespace();
    let method = loggable(parts.next().unwrap_or(""));
    let path = loggable(parts.next().unwrap_or(""));

    // Headers — we only need Content-Length to consume the body.
    let mut content_length = 0usize;
    for n in 0.. {
        if n == MAX_HEADERS {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "too many headers"));
        }
        let h = read_line_capped(&mut reader)?;
        if h.is_empty() {
            break;
        }
        let t = h.trim_end_matches(['\r', '\n']);
        if t.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = t.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    // Drain the body so the connection is clean (uid comes from the path).
    if content_length > 0 {
        let take = content_length.min(MAX_BODY);
        let mut body = vec![0u8; take];
        reader.read_exact(&mut body)?;
    }

    // Act: POST /devices/<uid>/notifications -> mark motion for that camera.
    if method.eq_ignore_ascii_case("POST") {
        match parse_uid(&path) {
            Some(uid) => match engine.pushx_motion(uid, peer_ip) {
                crate::engine::PushOutcome::Motion(name) => info!("pushx: motion from {peer} -> camera '{name}'"),
                crate::engine::PushOutcome::UnknownUid => warn_now_and_then(&format!("pushx: a push from {peer} for a camera uid that is not configured")),
                crate::engine::PushOutcome::WrongSender(name) => warn_now_and_then(&format!(
                    "pushx: ignored a push for camera '{name}' from {peer}, which is not that camera's address"
                )),
            },
            None => warn_now_and_then(&format!("pushx: {method} {path} from {peer}: no device uid in the path")),
        }
    } else {
        tracing::debug!("pushx: {method} {path} from {peer}");
    }

    // Always acknowledge with the exact 200 body the camera wants (an error
    // status would only make the camera retry; a missing camera is not the
    // camera's fault).
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        OK_BODY.len(),
        OK_BODY
    );
    writer.write_all(resp.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// A warning at most once a minute, so a stream of bad requests cannot fill the log.
fn warn_now_and_then(msg: &str) {
    static LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
        *last = Some(Instant::now());
        warn!("{msg}");
    } else {
        tracing::debug!("{msg}");
    }
}

/// Whether a push from `peer` may be for a camera at `host` (from its
/// `baichuan://host` URL): yes from that address (IPv4-mapped too) or from
/// this machine itself.
pub fn sender_matches(host: &str, peer: Option<std::net::IpAddr>) -> bool {
    // A camera without an IP address cannot be checked, and cannot stream
    // either (discovery needs one): refused.
    let Ok(cam) = host.parse::<std::net::IpAddr>() else { return false };
    match peer.map(|p| p.to_canonical()) {
        Some(p) => p == cam.to_canonical() || p.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_body_is_the_exact_53_bytes_the_camera_expects() {
        // The Argus firmware checks the body is exactly this; a mismatch makes
        // it treat the push as failed.
        assert_eq!(OK_BODY.len(), 53);
        assert_eq!(OK_BODY, "{\"status\":{\"code\":0,\"detail\":\"OK\"},\"data\":{\"sent\":0}}");
    }

    #[test]
    fn parses_the_uid_from_the_push_path() {
        assert_eq!(parse_uid("/devices/95270000ABCD1234/notifications"), Some("95270000ABCD1234"));
        assert_eq!(parse_uid("/devices/ABC123/notifications?t=1"), Some("ABC123"));
        assert_eq!(parse_uid("/devices/ABC123"), Some("ABC123"));
        assert_eq!(parse_uid("/devices//notifications"), None);
        assert_eq!(parse_uid("/health"), None);
        assert_eq!(parse_uid("/"), None);
    }

    #[test]
    fn only_the_camera_itself_may_push_for_it() {
        let ip = |s: &str| Some(s.parse().unwrap());
        assert!(sender_matches("10.0.0.110", ip("10.0.0.110")));
        assert!(!sender_matches("10.0.0.110", ip("10.0.0.99")), "another LAN host");
        assert!(sender_matches("10.0.0.110", ip("127.0.0.1")), "this machine");
        assert!(!sender_matches("10.0.0.110", None));
        assert!(!sender_matches("camera.lan", ip("10.0.0.99")), "a name cannot be checked: refused");
        assert!(sender_matches("10.0.0.110", ip("::ffff:10.0.0.110")), "dual-stack listener");
        assert!(sender_matches("10.0.0.110", ip("::ffff:127.0.0.1")));
    }

    #[test]
    fn log_text_is_printable_and_short() {
        assert_eq!(loggable("/devices/\x1b[31mX\n"), "/devices/[31mX");
        assert_eq!(loggable(&"a".repeat(500)).len(), 100);
    }
}
