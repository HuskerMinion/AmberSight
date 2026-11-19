//! `GET` / `POST /api/cameras/:id/controls`.
//!
//! Who may do what:
//! - Seeing what a camera has and its state: anyone signed in who sees the camera.
//! - The spotlight on and off: admins, and operators with the "lights" right.
//! - The siren: admins, and operators given the "siren" right (off unless an
//!   admin turns it on); never with a device token (a password sign-in). Stopping
//!   it: anyone who may use the lights or the siren.
//! - Settings kept on the camera (light mode, infrared lights, status and doorbell lights,
//!   asking again what it has): admins, signed in with a password.
//! - Linked servers: never (their cameras have other ids here, and the link
//!   only ever carries pictures).

use std::sync::Arc;

use axum::{
    extract::{Path as AxPath, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

use super::{Action, Caller, May};
use crate::auth::{Perm, Role};
use crate::engine::Engine;
use crate::web::{Caller as WebCaller, SignIn, TokenUsed};

fn err(status: u16, msg: &str) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), Json(serde_json::json!({ "error": msg }))).into_response()
}

/// What this caller may do with controls.
pub fn may(role: &Role, by_token: bool) -> May {
    May { light: role.may(Perm::Lights), siren: role.may(Perm::Siren) && !by_token, settings: role.is_admin() && !by_token }
}

/// One signed-in session or device token reads controls at most this often
/// (each read may ask a camera). Per session, not per account: a token that
/// leaked and is used to flood cannot lock the owner's own pages out.
pub const GET_PER_ACCOUNT: usize = 30;
/// And one account, all its sign-ins together, at most this often.
pub const GET_PER_USER: usize = 120;
pub const GET_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether `who` (a sign-in, or an account) may read controls once more
/// now, within `most` reads a window (and count it).
fn get_allowed(who: &str, most: usize) -> bool {
    use std::collections::{HashMap, VecDeque};
    use std::time::Instant;
    static SEEN: std::sync::Mutex<Option<HashMap<String, VecDeque<Instant>>>> = std::sync::Mutex::new(None);
    let mut g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    m.retain(|_, q| q.back().is_some_and(|t| t.elapsed() < GET_WINDOW));
    let q = m.entry(who.to_string()).or_default();
    while q.front().is_some_and(|t| t.elapsed() >= GET_WINDOW) {
        q.pop_front();
    }
    if q.len() >= most {
        return false;
    }
    q.push_back(Instant::now());
    true
}

pub async fn get(
    State(engine): State<Arc<Engine>>,
    token: Option<axum::Extension<TokenUsed>>,
    axum::Extension(role): axum::Extension<Role>,
    axum::Extension(caller): axum::Extension<WebCaller>,
    sign_in: Option<axum::Extension<SignIn>>,
    AxPath(id): AxPath<usize>,
) -> Response {
    // Per sign-in (the one that let the request in: a junk cookie next to a
    // token is not a new sign-in), and a looser cap per account.
    let key = sign_in.map(|s| s.0 .0).unwrap_or_else(|| format!("account:{}", caller.0));
    if !get_allowed(&format!("in|{key}"), GET_PER_ACCOUNT) || !get_allowed(&format!("user|{}", caller.0), GET_PER_USER) {
        return err(429, "too many reads of camera controls from this sign-in: wait a few seconds");
    }
    let m = may(&role, token.is_some());
    match tokio::task::spawn_blocking(move || super::view(&engine, id, m)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(r)) => err(r.status, &r.msg),
        Err(_) => err(500, "internal error"),
    }
}

/// Why this caller may not do `a`, if not.
pub fn refusal(a: &Action, role: &Role, by_token: bool) -> Option<&'static str> {
    let m = may(role, by_token);
    match a {
        _ if a.is_setting() && by_token => Some("sign in with a password for this"),
        _ if a.is_setting() && !m.settings => Some("only an admin can change the camera's own settings"),
        Action::Siren { .. } if by_token => Some("the siren needs a password sign-in (not a device token)"),
        Action::Siren { .. } if !m.siren => Some("this account may not sound the siren: an admin can allow it in Settings, Users"),
        Action::SirenStop if !(m.light || m.siren) => Some("this account may not use camera controls"),
        Action::LightOn { .. } | Action::LightOff if !m.light => Some("this account may not use the camera's lights: an admin can allow it in Settings, Users"),
        _ => None,
    }
}

pub async fn post(
    State(engine): State<Arc<Engine>>,
    token: Option<axum::Extension<TokenUsed>>,
    axum::Extension(role): axum::Extension<Role>,
    axum::Extension(caller): axum::Extension<WebCaller>,
    ci: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    AxPath(id): AxPath<usize>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let action: Action = match serde_json::from_value(body) {
        Ok(a) => a,
        Err(_) => return err(400, "unknown action: light_on, light_off, light_mode, siren, siren_stop, ir, status_led, doorbell_led or redetect"),
    };
    let by_token = token.is_some();
    if let Some(why) = refusal(&action, &role, by_token) {
        return err(403, why);
    }
    let who = Caller {
        user: match &token {
            Some(t) => format!("{} (device token \"{}\")", caller.0, t.0 .0),
            None => caller.0.clone(),
        },
        ip: ci.map(|c| c.0.ip().to_string()).unwrap_or_default(),
        source: "web",
    };
    let m = may(&role, by_token);
    match tokio::task::spawn_blocking(move || super::act(&engine, id, &who, action, false)).await {
        Ok(Ok(mut v)) => {
            v["may"] = serde_json::json!({ "light": m.light, "siren": m.siren, "settings": m.settings });
            Json(v).into_response()
        }
        Ok(Err(r)) => err(r.status, &r.msg),
        Err(_) => err(500, "internal error"),
    }
}
