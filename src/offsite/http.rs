//! Web API for Settings > System > Backup (admins only):
//!
//! - `GET  /api/offsite`       settings (secrets shown as "<set>") and status
//! - `PUT  /api/offsite`       save settings (fields left out stay as they are)
//! - `POST /api/offsite/test`  try the destination (with unsaved settings sent along)
//! - `POST /api/offsite/run`   make a copy now

use axum::{
    extract::{ConnectInfo, State},
    http::HeaderMap,
    response::{IntoResponse, Json, Response},
    Extension,
};
use std::sync::Arc;

use crate::config::OffsiteConfig;
use crate::engine::Engine;

const SECRETS: [&str; 2] = ["s3_secret_key", "passphrase"];
const SET: &str = "<set>";

/// The settings as the page may see them: secrets only say whether they exist.
pub fn masked(c: &OffsiteConfig) -> OffsiteConfig {
    let mut m = c.clone();
    for v in [&mut m.s3_secret_key, &mut m.passphrase] {
        if !v.is_empty() {
            *v = SET.into();
        }
    }
    m
}

/// Overlay what the page sent. A secret sent back as "<set>" or blank is
/// kept; `null` removes it.
pub fn merge(cur: &OffsiteConfig, patch: serde_json::Value) -> Result<OffsiteConfig, String> {
    let serde_json::Value::Object(patch) = patch else { return Err("expected an object".into()) };
    let mut v = serde_json::to_value(cur).map_err(|e| e.to_string())?;
    let obj = v.as_object_mut().ok_or("internal error")?;
    for (k, val) in patch {
        if SECRETS.contains(&k.as_str()) {
            match &val {
                serde_json::Value::Null => {
                    obj.insert(k, serde_json::Value::String(String::new()));
                }
                serde_json::Value::String(s) if s.trim().is_empty() || s == SET => {}
                serde_json::Value::String(s) => {
                    if k == "passphrase" && s.chars().count() < 8 {
                        return Err("the passphrase needs at least 8 characters".into());
                    }
                    obj.insert(k, val);
                }
                _ => return Err(format!("{k} must be text")),
            }
        } else {
            obj.insert(k, val);
        }
    }
    let mut out: OffsiteConfig = serde_json::from_value(v).map_err(|e| format!("invalid settings: {e}"))?;
    out.every_hours = out.every_hours.clamp(1, 720);
    out.keep = out.keep.clamp(1, 365);
    out.folder = out.folder.trim().to_string();
    out.s3_prefix = out.s3_prefix.trim().trim_matches('/').to_string();
    if !matches!(out.kind.as_str(), "folder" | "s3") {
        return Err("choose a folder or S3 storage".into());
    }
    Ok(out)
}

fn status_json(engine: &Engine) -> serde_json::Value {
    let cfg = engine.config.lock().unwrap().offsite.clone();
    let st = super::status();
    serde_json::json!({
        "settings": masked(&cfg),
        "status": {
            "running": st.running,
            "phase": st.phase,
            "last_start": st.last_start,
            "last_end": st.last_end,
            "last_ok": st.last_ok,
            "last_error": st.last_error,
            "last_bytes": st.last_bytes,
            "last_sent": st.last_sent,
            "last_skipped": st.last_skipped,
            "last_success": st.last_success,
            "failures": st.failures,
            "next_run": super::next_run(&cfg, &st, super::started_at()),
        },
    })
}

pub async fn get(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(status_json(&engine))
}

/// Whether where backups go changes: where, with what credentials, or with what
/// passphrase. They hold every secret on this server, so that takes the caller's
/// own password. Turning them on or off and their schedule do not.
fn destination_changes(a: &OffsiteConfig, b: &OffsiteConfig) -> bool {
    a.kind != b.kind
        || a.folder != b.folder
        || a.s3_endpoint != b.s3_endpoint
        || a.s3_region != b.s3_region
        || a.s3_bucket != b.s3_bucket
        || a.s3_prefix != b.s3_prefix
        || a.s3_access_key != b.s3_access_key
        || a.s3_secret_key != b.s3_secret_key
        || a.s3_path_style != b.s3_path_style
        || a.passphrase != b.passphrase
}

pub async fn put(State(engine): State<Arc<Engine>>, Extension(caller): Extension<crate::web::Caller>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, Json(mut patch): Json<serde_json::Value>) -> Response {
    // Its own fields stay out of what is saved.
    let mut proof = crate::account_http::MyProof::default();
    if let Some(o) = patch.as_object_mut() {
        let take = |o: &mut serde_json::Map<String, serde_json::Value>, k: &str| o.remove(k).and_then(|v| v.as_str().map(str::to_string));
        proof.my_password = take(o, "my_password");
        proof.my_code = take(o, "my_code");
        proof.my_recovery_code = take(o, "my_recovery_code");
    }
    let open = caller.0 == "(open)";
    let proved = {
        let cur = engine.config.lock().unwrap().offsite.clone();
        !open && merge(&cur, patch.clone()).is_ok_and(|new| destination_changes(&cur, &new))
    };
    if proved {
        if let Err(r) = crate::account_http::prove_admin(&engine, &caller.0, peer.ip(), &headers, &proof, caller.1).await {
            return r;
        }
    }
    put_checked(engine, patch, caller, proved, open).await.into_response()
}

async fn put_checked(engine: Arc<Engine>, patch: serde_json::Value, caller: crate::web::Caller, proved: bool, open: bool) -> Json<serde_json::Value> {
    // Writing the file can be slow (a busy disk): not on the async threads.
    // The lock is held while saving, as every other settings save does, so
    // two saves never reach the file in the wrong order.
    let e = engine.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut live = e.config.lock().unwrap();
        let new = merge(&live.offsite, patch)?;
        // (Checked again now the slow check is over: the caller and the settings as they are now.)
        if proved && (!crate::users_http::caller_may_change_accounts(&live, &caller.0) || crate::auth::generation(&caller.0) != caller.1) {
            return Err("this account changed meanwhile: try again".into());
        }
        if !proved && !open && destination_changes(&live.offsite, &new) {
            return Err("the settings changed meanwhile: try again".into());
        }
        if new.enabled {
            super::check_settings(&new)?;
        }
        let mut cfg = live.clone();
        cfg.offsite = new;
        cfg.save(&e.config_path).map_err(|err| format!("could not save the settings: {err:#}"))?;
        *live = cfg;
        Ok(())
    })
    .await;
    match res {
        Ok(Ok(())) => Json(status_json(&engine)),
        Ok(Err(e)) => Json(serde_json::json!({ "error": e })),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

/// Trying an unsaved destination writes and reads a file there using the saved
/// keys: the access key and signed requests go to that server (never the secret
/// key or the passphrase), and a folder test touches any path the server can
/// write. So it takes the same proof as saving that destination.
pub async fn test(State(engine): State<Arc<Engine>>, Extension(caller): Extension<crate::web::Caller>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, bytes: axum::body::Bytes) -> Response {
    // (Bytes, not Option<Json>: a body that fails to arrive stops this. Empty: the saved settings.)
    let mut body = serde_json::from_slice::<serde_json::Value>(&bytes).ok().map(Json);
    let mut proof = crate::account_http::MyProof::default();
    if let Some(o) = body.as_mut().and_then(|b| b.0.as_object_mut()) {
        let take = |o: &mut serde_json::Map<String, serde_json::Value>, k: &str| o.remove(k).and_then(|v| v.as_str().map(str::to_string));
        proof.my_password = take(o, "my_password");
        proof.my_code = take(o, "my_code");
        proof.my_recovery_code = take(o, "my_recovery_code");
    }
    let cur = engine.config.lock().unwrap().offsite.clone();
    let cfg = match body {
        Some(Json(p)) if p.is_object() => match merge(&cur, p) {
            Ok(v) => v,
            Err(e) => return Json(serde_json::json!({ "ok": false, "message": e })).into_response(),
        },
        _ => cur.clone(),
    };
    if caller.0 != "(open)" && destination_changes(&cur, &cfg) {
        if let Err(r) = crate::account_http::prove_admin(&engine, &caller.0, peer.ip(), &headers, &proof, caller.1).await {
            return r;
        }
    }
    test_run(cfg).await.into_response()
}

async fn test_run(cfg: OffsiteConfig) -> Json<serde_json::Value> {
    let res = tokio::task::spawn_blocking(move || super::test_connection(&cfg)).await;
    match res {
        Ok(Ok(m)) => Json(serde_json::json!({ "ok": true, "message": m })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "message": format!("{e:#}") })),
        Err(e) => Json(serde_json::json!({ "ok": false, "message": e.to_string() })),
    }
}

pub async fn run(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let cfg = engine.config.lock().unwrap().offsite.clone();
    if let Err(e) = super::check_settings(&cfg) {
        return Json(serde_json::json!({ "error": e }));
    }
    if !super::request_run() {
        return Json(serde_json::json!({ "error": "a copy is being made already" }));
    }
    Json(serde_json::json!({ "started": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_masked_and_kept() {
        let cur = OffsiteConfig { kind: "s3".into(), s3_secret_key: "sk".into(), passphrase: "a long passphrase".into(), ..Default::default() };
        let m = masked(&cur);
        assert_eq!((m.s3_secret_key.as_str(), m.passphrase.as_str()), (SET, SET));
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("sk\"") && !json.contains("long passphrase"));
        // Sent back masked or blank: kept.
        let out = merge(&cur, serde_json::json!({ "s3_secret_key": SET, "passphrase": "", "s3_bucket": "b", "keep": 0 })).unwrap();
        assert_eq!((out.s3_secret_key.as_str(), out.passphrase.as_str(), out.s3_bucket.as_str(), out.keep), ("sk", "a long passphrase", "b", 1));
        // New values replace; null removes; short passphrases are refused.
        let out = merge(&cur, serde_json::json!({ "s3_secret_key": "new", "passphrase": null })).unwrap();
        assert_eq!((out.s3_secret_key.as_str(), out.passphrase.as_str()), ("new", ""));
        assert!(merge(&cur, serde_json::json!({ "passphrase": "short" })).is_err());
        assert!(merge(&cur, serde_json::json!({ "kind": "ftp" })).is_err());
    }

    #[test]
    fn only_where_backups_go_takes_the_callers_password() {
        let cur = OffsiteConfig { kind: "s3".into(), s3_bucket: "b".into(), s3_secret_key: "sk".into(), passphrase: "a long passphrase".into(), every_hours: 24, keep: 7, ..Default::default() };
        let after = |p: serde_json::Value| destination_changes(&cur, &merge(&cur, p).unwrap());
        for p in [serde_json::json!({ "enabled": true }), serde_json::json!({ "every_hours": 6 }), serde_json::json!({ "keep": 3 }), serde_json::json!({ "s3_secret_key": SET, "passphrase": SET }), serde_json::json!({ "s3_bucket": "b" })] {
            assert!(!after(p.clone()), "{p}");
        }
        for p in [serde_json::json!({ "s3_bucket": "theirs" }), serde_json::json!({ "s3_endpoint": "https://evil.example" }), serde_json::json!({ "s3_access_key": "k" }), serde_json::json!({ "s3_secret_key": "new" }), serde_json::json!({ "passphrase": "a new long passphrase" }), serde_json::json!({ "passphrase": null }), serde_json::json!({ "kind": "folder" })] {
            assert!(after(p.clone()), "{p}");
        }
    }
}
