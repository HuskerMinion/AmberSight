//! Interface state kept on the server: per-user preferences (layouts, dock
//! side, quality...) and the camera order shared by all users.

use axum::{
    extract::{Path as AxPath, Request, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

use crate::engine::Engine;
use crate::web::{Access, Caller};

const MAX_PREFS: usize = 64 * 1024;
const MAX_DIAG: usize = 16 * 1024;
const MAX_ORDER: usize = 64 * 1024;
const ORDERS: &str = "orders";

fn scope_of(caller: &Caller) -> String {
    // Open-LAN mode has no users: one shared bucket.
    if caller.0 == "(open)" { String::new() } else { caller.0.clone() }
}

/// Parse and check a prefs body: a JSON object of at most 64 KB.
pub fn parse_prefs(bytes: &[u8]) -> Result<String, &'static str> {
    if bytes.len() > MAX_PREFS {
        return Err("too large (max 64 KB)");
    }
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(v) if v.is_object() => Ok(v.to_string()),
        _ => Err("expected a JSON object"),
    }
}

/// An order key is `all` or `group:<name>`.
pub fn valid_order_key(key: &str) -> bool {
    key == "all" || key.strip_prefix("group:").is_some_and(|g| !g.trim().is_empty() && g.len() <= 200)
}

/// Parse a camera-name array, dropping duplicates and empty names.
pub fn parse_order(bytes: &[u8]) -> Result<Vec<String>, &'static str> {
    if bytes.len() > MAX_ORDER {
        return Err("too large");
    }
    let names: Vec<String> = serde_json::from_slice(bytes).map_err(|_| "expected a JSON array of camera names")?;
    let mut seen = std::collections::HashSet::new();
    Ok(names.into_iter().filter(|n| !n.is_empty() && seen.insert(n.clone())).collect())
}

/// Keep only the names `allow` accepts.
pub fn visible_order(names: Vec<String>, allow: impl Fn(&str) -> bool) -> Vec<String> {
    names.into_iter().filter(|n| allow(n)).collect()
}

/// `GET /api/ui/prefs`
pub async fn get_prefs(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>) -> Response {
    let v = engine.db.ui_get(&scope_of(&caller), "prefs").ok().flatten();
    let obj = v
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    Json(obj).into_response()
}

/// `PUT /api/ui/prefs` (any signed-in user, own prefs only)
pub async fn put_prefs(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, req: Request) -> Response {
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_PREFS + 1).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "too large (max 64 KB)").into_response(),
    };
    let text = match parse_prefs(&bytes) {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    match engine.db.ui_put(&scope_of(&caller), "prefs", &text) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// Merge `patch`'s top-level keys into `base` (a JSON object); a `null`
/// value removes the key.
pub fn merge_prefs(base: &str, patch: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let mut obj = serde_json::from_str::<serde_json::Value>(base)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (k, v) in patch {
        if v.is_null() {
            obj.remove(k);
        } else {
            obj.insert(k.clone(), v.clone());
        }
    }
    serde_json::Value::Object(obj)
}

/// `PATCH /api/ui/prefs` (any signed-in user, own prefs only): merge only the
/// keys sent, so a page that changes one setting cannot undo another page's
/// changes to other settings.
pub async fn patch_prefs(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, req: Request) -> Response {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_PREFS + 1).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "too large (max 64 KB)").into_response(),
    };
    let patch = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return (StatusCode::BAD_REQUEST, "expected a JSON object").into_response(),
    };
    let scope = scope_of(&caller);
    let _guard = LOCK.lock().unwrap();
    let cur = engine.db.ui_get(&scope, "prefs").ok().flatten().unwrap_or_else(|| "{}".into());
    let merged = merge_prefs(&cur, &patch).to_string();
    if merged.len() > MAX_PREFS {
        return (StatusCode::PAYLOAD_TOO_LARGE, "too large (max 64 KB)").into_response();
    }
    match engine.db.ui_put(&scope, "prefs", &merged) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// `GET /api/ui/orders` -> `{ key: [camera names] }`
pub async fn get_orders(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>) -> Response {
    let mut out = serde_json::Map::new();
    for (k, v) in engine.db.ui_scope(ORDERS).unwrap_or_default() {
        if let Ok(names) = serde_json::from_str::<Vec<String>>(&v) {
            out.insert(k, visible_order(names, |n| access.can_name(n)).into());
        }
    }
    Json(serde_json::Value::Object(out)).into_response()
}

/// `PUT /api/ui/orders/:key` (admin; the middleware enforces it for writes)
pub async fn put_order(State(engine): State<Arc<Engine>>, AxPath(key): AxPath<String>, req: Request) -> Response {
    if !valid_order_key(&key) {
        return (StatusCode::BAD_REQUEST, "key must be 'all' or 'group:<name>'").into_response();
    }
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_ORDER + 1).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response(),
    };
    let names = match parse_order(&bytes) {
        Ok(n) => n,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    match engine.db.ui_put(ORDERS, &key, &serde_json::to_string(&names).unwrap_or_default()) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// `POST /api/ui/diag`: a page's start-up timings or a sound report
/// (diag.js), written to the log as one line for finding out why the
/// interface is slow (or makes sound) on a device. Any signed-in user, for
/// their own devices; at most one a second per user and 16 KB (logged
/// re-serialized, so a page cannot break the log's lines).
pub async fn diag(axum::Extension(caller): axum::Extension<Caller>, body: axum::body::Bytes) -> Response {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    if body.len() > MAX_DIAG {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let v = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) if v.is_object() => v,
        _ => return (StatusCode::BAD_REQUEST, "expected a JSON object").into_response(),
    };
    let now = chrono::Utc::now().timestamp_millis();
    {
        let mut last = LAST.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
        if last.get(&caller.0).is_some_and(|t| now - t < 1000) {
            return StatusCode::NO_CONTENT.into_response();
        }
        if last.len() > 1000 {
            last.clear();
        }
        last.insert(caller.0.clone(), now);
    }
    tracing::info!("page timings ({}): {}", caller.0, v);
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_must_be_a_small_object() {
        assert!(parse_prefs(br#"{"dock":"left"}"#).is_ok());
        assert!(parse_prefs(b"[1]").is_err());
        assert!(parse_prefs(b"\"x\"").is_err());
        assert!(parse_prefs(b"nope").is_err());
        let big = format!("{{\"a\":\"{}\"}}", "x".repeat(MAX_PREFS));
        assert!(parse_prefs(big.as_bytes()).is_err());
    }

    #[test]
    fn order_keys_and_bodies() {
        assert!(valid_order_key("all") && valid_order_key("group:Front yard"));
        assert!(!valid_order_key("group:") && !valid_order_key("x") && !valid_order_key("group:  "));
        assert_eq!(parse_order(br#"["A","B","A",""]"#).unwrap(), vec!["A", "B"]);
        assert!(parse_order(br#"{"a":1}"#).is_err());
        assert!(parse_order(br#"[1,2]"#).is_err());
    }

    #[test]
    fn limited_users_only_see_their_cameras() {
        let acc = Access(Some(["door".to_string()].into_iter().collect()));
        let v = visible_order(vec!["Door".into(), "Garage".into()], |n| acc.can_name(n));
        assert_eq!(v, vec!["Door"]);
        let all = Access::default();
        assert_eq!(visible_order(vec!["Door".into(), "Garage".into()], |n| all.can_name(n)).len(), 2);
    }

    #[test]
    fn scopes() {
        assert_eq!(scope_of(&Caller("(open)".into(), 0)), "");
        assert_eq!(scope_of(&Caller("bob".into(), 0)), "bob");
    }

    #[test]
    fn prefs_merge_keeps_other_keys() {
        let base = r#"{"layouts":[{"id":"a"}],"dock":{"side":"left"},"quality":"light"}"#;
        let patch: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"quality":"smooth","dock":null,"boxes":"motion"}"#).unwrap();
        let m = merge_prefs(base, &patch);
        assert_eq!(m["quality"], "smooth");
        assert_eq!(m["boxes"], "motion");
        assert!(m.get("dock").is_none());
        assert_eq!(m["layouts"][0]["id"], "a");
        assert_eq!(merge_prefs("not json", &patch)["quality"], "smooth");
    }
}
