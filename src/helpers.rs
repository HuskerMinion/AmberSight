//! Server side of the capture helper (see docs/helper-protocol.md): pairing
//! codes, registration, the control WebSocket, and telling each helper which
//! cameras to capture. The video itself arrives on /push/<key>.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use axum::extract::ws::{Message, WebSocket};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

use crate::config::HelperConfig;
use crate::engine::Engine;

/// How long a pairing code works.
pub const PAIR_SECS: u64 = 900;

struct Conn {
    /// Session number: a newer connection for the same helper replaces this one.
    gen: u64,
    tx: mpsc::UnboundedSender<String>,
    machine: String,
    version: String,
    since: Instant,
}

#[derive(Default)]
struct State {
    codes: HashMap<String, Instant>,
    conns: HashMap<String, Conn>,
    waiting: HashMap<u64, oneshot::Sender<serde_json::Value>>,
    /// (helper id, camera) -> last status from the helper.
    status: HashMap<(String, String), serde_json::Value>,
    next: u64,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn random_hex(n: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|x| format!("{x:02x}")).collect()
}

/// A short code an admin reads to the other computer (letters and digits that
/// are hard to mix up), valid for PAIR_SECS.
pub fn new_pair_code() -> String {
    const ABC: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    use rand::RngCore;
    let mut b = [0u8; 10];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let code: String = b.iter().map(|x| ABC[*x as usize % ABC.len()] as char).collect();
    let mut s = state().lock().unwrap();
    s.codes.retain(|_, t| t.elapsed() < Duration::from_secs(PAIR_SECS));
    s.codes.insert(code.clone(), Instant::now());
    code
}

/// Whether a pairing code is still valid (downloads); does not use it up.
pub fn code_valid(code: &str) -> bool {
    let s = state().lock().unwrap();
    s.codes.get(&code.trim().to_ascii_uppercase()).is_some_and(|t| t.elapsed() < Duration::from_secs(PAIR_SECS))
}

/// Use a pairing code: a new helper id and its token.
pub fn register(engine: &Engine, code: &str, machine: &str, os: &str) -> Result<(String, String, String)> {
    let code = code.trim().to_ascii_uppercase();
    {
        let mut s = state().lock().unwrap();
        match s.codes.remove(&code) {
            Some(t) if t.elapsed() < Duration::from_secs(PAIR_SECS) => {}
            _ => return Err(anyhow!("that pairing code has expired or was already used")),
        }
    }
    let id = random_hex(8);
    let token = random_hex(32);
    let name: String = machine.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    let name = if name.is_empty() { format!("Computer {}", &id[..4]) } else { name };
    let mut cfg = engine.config.lock().unwrap();
    cfg.helpers.push(HelperConfig {
        id: id.clone(),
        name: name.clone(),
        token_sha256: token_hash(&token),
        os: os.chars().take(16).collect(),
        added: chrono::Utc::now().timestamp(),
    });
    cfg.save(&engine.config_path)?;
    Ok((id, token, name))
}

/// The helper a token belongs to.
pub fn authenticate(engine: &Engine, id: &str, token: &str) -> Option<HelperConfig> {
    use subtle::ConstantTimeEq;
    let want = token_hash(token);
    let cfg = engine.config.lock().unwrap();
    cfg.helpers.iter().find(|h| h.id == id && bool::from(h.token_sha256.as_bytes().ct_eq(want.as_bytes()))).cloned()
}

/// Remove a helper: its token stops working and its connection closes.
pub fn remove(engine: &Engine, id: &str) -> Result<bool> {
    let mut cfg = engine.config.lock().unwrap();
    let before = cfg.helpers.len();
    cfg.helpers.retain(|h| h.id != id);
    if cfg.helpers.len() == before {
        return Ok(false);
    }
    cfg.save(&engine.config_path)?;
    drop(cfg);
    let mut s = state().lock().unwrap();
    s.conns.remove(id); // dropping the sender ends the socket task
    s.status.retain(|(h, _), _| h != id);
    Ok(true)
}

fn send(id: &str, msg: serde_json::Value) -> bool {
    let s = state().lock().unwrap();
    s.conns.get(id).is_some_and(|c| c.tx.send(msg.to_string()).is_ok())
}

/// Ask a helper something and wait for its answer (`req` is filled in).
pub async fn request(id: &str, mut msg: serde_json::Value, timeout: Duration) -> Result<serde_json::Value> {
    let (tx, rx) = oneshot::channel();
    let req = {
        let mut s = state().lock().unwrap();
        s.next += 1;
        let r = s.next;
        s.waiting.insert(r, tx);
        r
    };
    msg["req"] = req.into();
    if !send(id, msg) {
        state().lock().unwrap().waiting.remove(&req);
        return Err(anyhow!("that computer is not connected"));
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(v)) => Ok(v),
        _ => {
            state().lock().unwrap().waiting.remove(&req);
            Err(anyhow!("that computer did not answer in time"))
        }
    }
}

/// Tell a helper exactly which cameras it should capture now.
pub fn sync_helper(engine: &Engine, id: &str) {
    let cams: Vec<(String, String, crate::config::HelperCapture)> = {
        let cfg = engine.config.lock().unwrap();
        cfg.cameras
            .iter()
            .filter(|c| c.enabled)
            .filter_map(|c| {
                let cap = c.capture.as_ref().filter(|x| x.helper == id)?;
                let key = crate::ingest::push::push_key(&c.url)?;
                Some((c.name.clone(), key.to_string(), cap.clone()))
            })
            .collect()
    };
    let names: Vec<String> = cams.iter().map(|c| c.0.clone()).collect();
    // Forget what it last said about cameras it no longer sends.
    state().lock().unwrap().status.retain(|(h, cam), _| h != id || names.contains(cam));
    let _ = send(id, serde_json::json!({ "type": "desired", "cameras": names }));
    for (name, key, cap) in cams {
        let _ = send(id, serde_json::json!({
            "type": "start", "camera": name, "key": key, "source": cap.source,
            "fps": cap.fps, "max_kbps": cap.max_kbps, "max_width": cap.max_width,
        }));
    }
}

/// After cameras change: resend every connected helper its list.
pub fn sync_all(engine: &Engine) {
    let ids: Vec<String> = state().lock().unwrap().conns.keys().cloned().collect();
    for id in ids {
        sync_helper(engine, &id);
    }
}

/// Connected helpers and their camera states, for the admin page.
pub fn list(engine: &Engine) -> serde_json::Value {
    let cfg = engine.config.lock().unwrap();
    let s = state().lock().unwrap();
    let helpers: Vec<serde_json::Value> = cfg
        .helpers
        .iter()
        .map(|h| {
            let c = s.conns.get(&h.id);
            let cams: Vec<serde_json::Value> = s
                .status
                .iter()
                .filter(|((hid, _), _)| hid == &h.id)
                .map(|((_, cam), st)| serde_json::json!({ "camera": cam, "status": st }))
                .collect();
            serde_json::json!({
                "id": h.id, "name": h.name, "os": h.os, "added": h.added,
                "online": c.is_some(),
                "machine": c.map(|c| c.machine.clone()),
                "version": c.map(|c| c.version.clone()),
                "connected_s": c.map(|c| c.since.elapsed().as_secs()),
                "cameras": cams,
            })
        })
        .collect();
    serde_json::json!({ "helpers": helpers })
}

/// Last status a helper reported for a camera.
pub fn camera_status(helper: &str, camera: &str) -> Option<serde_json::Value> {
    state().lock().unwrap().status.get(&(helper.to_string(), camera.to_string())).cloned()
}

/// Run one helper's control connection until it closes.
pub async fn run_socket(engine: Arc<Engine>, helper: HelperConfig, mut ws: WebSocket) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let gen = {
        let mut s = state().lock().unwrap();
        s.next += 1;
        let g = s.next;
        // A newer connection replaces an older one for the same helper.
        s.conns.insert(helper.id.clone(), Conn { gen: g, tx, machine: String::new(), version: String::new(), since: Instant::now() });
        g
    };
    tracing::info!("capture helper '{}' connected", helper.name);
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    let mut last_seen = Instant::now();
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(text) => { if ws.send(Message::Text(text)).await.is_err() { break; } }
                None => break, // removed, or replaced by a newer connection
            },
            inc = ws.recv() => match inc {
                Some(Ok(Message::Text(t))) => {
                    last_seen = Instant::now();
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    handle(&engine, &helper, v);
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => { last_seen = Instant::now(); }
            },
            _ = ping.tick() => {
                if last_seen.elapsed() > Duration::from_secs(95) {
                    break; // no answer to three pings
                }
                if ws.send(Message::Text(serde_json::json!({ "type": "ping" }).to_string())).await.is_err() { break; }
            }
        }
    }
    let mut s = state().lock().unwrap();
    if s.conns.get(&helper.id).is_some_and(|c| c.gen == gen) {
        s.conns.remove(&helper.id);
        // Its cameras are not streaming any more: drop their last status.
        s.status.retain(|(h, _), _| h != &helper.id);
        tracing::info!("capture helper '{}' disconnected", helper.name);
    }
}

fn handle(engine: &Arc<Engine>, helper: &HelperConfig, v: serde_json::Value) {
    match v["type"].as_str().unwrap_or("") {
        "hello" => {
            {
                let mut s = state().lock().unwrap();
                if let Some(c) = s.conns.get_mut(&helper.id) {
                    c.machine = v["machine"].as_str().unwrap_or("").chars().take(64).collect();
                    c.version = v["version"].as_str().unwrap_or("").chars().take(32).collect();
                }
            }
            sync_helper(engine, &helper.id);
        }
        "status" => {
            let cam = v["camera"].as_str().unwrap_or("").to_string();
            if !cam.is_empty() {
                let st = serde_json::json!({ "state": v["state"], "error": v["error"] });
                if v["state"] == "error" {
                    tracing::warn!("capture helper '{}': {}: {}", helper.name, cam, v["error"].as_str().unwrap_or("error"));
                }
                state().lock().unwrap().status.insert((helper.id.clone(), cam), st);
            }
        }
        "sources" | "thumb" => {
            if let Some(req) = v["req"].as_u64() {
                if let Some(tx) = state().lock().unwrap().waiting.remove(&req) {
                    let _ = tx.send(v);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_one_time_and_tokens_hash() {
        let c = new_pair_code();
        assert_eq!(c.len(), 10);
        assert!(code_valid(&c) && code_valid(&c.to_lowercase()));
        state().lock().unwrap().codes.remove(&c);
        assert!(!code_valid(&c));
        assert_eq!(token_hash("abc").len(), 64);
        assert_ne!(token_hash("abc"), token_hash("abd"));
    }
}
