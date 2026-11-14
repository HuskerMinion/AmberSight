//! Account management over the API. Admin only, except changing your own
//! password. Passwords are stored hashed; they are never returned.

use crate::config::UserConfig;
use crate::engine::Engine;
use axum::{
    extract::{Path as AxPath, Request, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

const MIN_PASSWORD: usize = 8;
/// The longest name of a new account.
const MAX_NAME: usize = 64;

/// Fold a legacy single `[server.auth]` account into `[[server.users]]`
/// before editing, so there is one list to manage.
pub(crate) fn migrate_legacy(cfg: &mut crate::config::Config) {
    if let Some(a) = cfg.server.auth.take() {
        if !cfg.server.users.iter().any(|u| u.username == a.username) {
            cfg.server.users.insert(0, UserConfig { disabled: false, username: a.username, password: a.password, admin: true, cameras: Vec::new(), ..Default::default() });
        }
    }
}

/// A new account's name: names starting with "(" are kept for the server's
/// own entries in the audit log ("(presence)", "(mqtt)", "(watchdog)", ...).
/// It goes into alerts and lists: not long, nothing that does not show, no new
/// lines, nothing that reorders the text around it (the rules of a device name).
/// Only for new accounts: one that exists keeps working, whatever it is called.
fn check_new_name(name: &str) -> Result<(), &'static str> {
    if name.starts_with('(') {
        return Err("a username cannot start with \"(\"; that is kept for the server's own entries in the audit log");
    }
    if name.chars().count() > MAX_NAME {
        return Err("a username can be at most 64 characters long");
    }
    if name.chars().any(|c| c.is_control() || crate::push_http::invisible(c)) {
        return Err("a username must not hold control or invisible characters");
    }
    Ok(())
}

/// Admins that can sign in: the server must always keep at least one.
fn admins(cfg: &crate::config::Config) -> usize {
    cfg.server.users.iter().filter(|u| u.admin && !u.disabled).count() + cfg.server.auth.iter().count()
}

/// Whether `who` (the caller of this request) can still change accounts: an
/// admin who can sign in, or anyone on a server with no accounts.
pub fn caller_may_change_accounts(cfg: &crate::config::Config, who: &str) -> bool {
    let acc = crate::auth::accounts(cfg);
    if who == "(open)" {
        return acc.is_empty();
    }
    acc.iter().any(|(u, _, admin)| u == who && *admin)
}

/// `GET /api/users` → accounts without passwords.
pub async fn list_users(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let on = engine.db.two_step_users().unwrap_or_default();
    let cfg = engine.config.lock().unwrap();
    let extra = |name: &str| (on.iter().any(|u| u == name.trim()), crate::auth::token_count(name.trim()));
    let mut users: Vec<serde_json::Value> = Vec::new();
    if let Some(a) = &cfg.server.auth {
        let (two_step, tokens) = extra(&a.username);
        users.push(serde_json::json!({ "username": a.username, "admin": true, "role": "admin", "perms": serde_json::Value::Null, "cameras": [], "disabled": false, "two_step": two_step, "tokens": tokens }));
    }
    for u in &cfg.server.users {
        let (two_step, tokens) = extra(&u.username);
        let role = if u.admin { "admin" } else if u.is_operator() { "operator" } else { "viewer" };
        // (An operator's own rights; null for others.)
        let perms = if u.is_operator() { serde_json::to_value(u.perms).unwrap_or_default() } else { serde_json::Value::Null };
        users.push(serde_json::json!({ "username": u.username, "admin": u.admin, "role": role, "perms": perms, "cameras": u.cameras, "disabled": u.disabled, "two_step": two_step, "tokens": tokens }));
    }
    Json(serde_json::json!({ "users": users, "auth_required": !users.is_empty() }))
}

#[derive(serde::Deserialize)]
pub struct UserReq {
    username: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    admin: Option<bool>,
    /// "viewer", "operator" or "admin" (instead of `admin`, or agreeing with it).
    #[serde(default)]
    role: Option<String>,
    /// An operator's rights; one left out is on.
    #[serde(default)]
    perms: Option<PermsReq>,
    #[serde(default)]
    cameras: Option<Vec<String>>,
    /// true: this account can no longer sign in or do anything; false: it can again.
    #[serde(default)]
    disabled: Option<bool>,
    /// The caller's own password and code, for what `needs_my_proof` says.
    #[serde(flatten)]
    proof: crate::account_http::MyProof,
}

/// An operator's rights as asked for: a name that is not one is refused (400),
/// so a typo does not quietly leave a right on.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermsReq {
    ptz: Option<bool>,
    talk_clip: Option<bool>,
    arm: Option<bool>,
    events: Option<bool>,
    camera_detection: Option<bool>,
}

impl From<PermsReq> for crate::config::OperatorPerms {
    fn from(p: PermsReq) -> Self {
        let on = |x: Option<bool>| x.unwrap_or(true);
        crate::config::OperatorPerms { ptz: on(p.ptz), talk_clip: on(p.talk_clip), arm: on(p.arm), events: on(p.events), camera_detection: on(p.camera_detection) }
    }
}

/// A change that will be refused whoever asks for it.
fn plain_refusal(cfg: &crate::config::Config, name: &str, admin: Option<bool>, disabled: Option<bool>, password: bool) -> Option<&'static str> {
    let target = cfg.server.users.iter().find(|u| u.username.trim() == name);
    match target {
        Some(u) => {
            if u.admin && !u.disabled && admins(cfg) <= 1 {
                if admin == Some(false) {
                    return Some("cannot remove admin rights from the last admin");
                }
                if disabled == Some(true) {
                    return Some("cannot disable the last admin who can sign in");
                }
            }
            None
        }
        None if cfg.server.auth.as_ref().is_some_and(|a| a.username.trim() == name) => None,
        None => check_new_name(name).err().or(if password { None } else { Some("a password is required for a new user") }),
    }
}

/// A change could not be finished after it was saved: take it back, and say
/// truthfully how that went.
fn take_back(cfg: &mut crate::config::Config, before: Vec<UserConfig>, path: &str, what: &str) -> Response {
    cfg.server.users = before;
    match cfg.save(path) {
        Ok(()) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{what}, so nothing was changed. Try again, or see the server's log")).into_response(),
        Err(e) => {
            tracing::error!("could not take a change back in the config file: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{what}. The change was undone here, but the settings file could not be saved and may still hold it until the next save: see the server's log")).into_response()
        }
    }
}

/// Whether this change needs the caller's own password and code: making an
/// admin (a new one, or promoting), or setting someone else's password. A
/// stolen session must not be able to make itself a way back in. Creating or
/// editing viewers and disabling (an emergency must be quick) need neither,
/// nor does the first account of a server with none.
fn needs_my_proof(cfg: &crate::config::Config, who: &str, name: &str, admin: Option<bool>, password: bool, disabled: Option<bool>) -> bool {
    if who == "(open)" {
        return false;
    }
    let legacy = cfg.server.auth.as_ref().is_some_and(|a| a.username.trim() == name);
    let listed: Vec<bool> = cfg.server.users.iter().filter(|u| u.username.trim() == name).map(|u| u.admin).collect();
    if !legacy && listed.is_empty() {
        return admin == Some(true);
    }
    let is_admin = legacy || listed.iter().any(|a| *a);
    // Enabling a disabled admin gives admin rights back: the same as making one.
    let enabling_admin = is_admin && disabled == Some(false) && admin != Some(false) && cfg.server.users.iter().any(|u| u.username.trim() == name && u.disabled);
    (admin == Some(true) && !is_admin) || enabling_admin || (password && who != name)
}

/// `role` and `admin` of a request as (admin, operator): None leaves it as it
/// is. `admin: true` alone makes an admin (no longer an operator); `admin:
/// false` alone takes admin rights away and leaves the rest (an older page
/// knows nothing of operators).
fn wanted_role(role: Option<&str>, admin: Option<bool>) -> Result<(Option<bool>, Option<bool>), &'static str> {
    let (a, o) = match role.map(str::trim) {
        None => return Ok((admin, admin.filter(|a| *a).map(|_| false))),
        Some("admin") => (true, false),
        Some("operator") => (false, true),
        Some("viewer") => (false, false),
        Some(_) => return Err("role must be viewer, operator or admin"),
    };
    if admin.is_some_and(|x| x != a) {
        return Err("role and admin disagree: send one of them");
    }
    Ok((Some(a), Some(o)))
}

/// How a change gives an account operator rights it does not have now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperatorGain {
    None,
    /// A new operator, a viewer made one, a right turned on, or more cameras
    /// (fewer limits, or none).
    More,
    /// A disabled operator enabled.
    Enabled,
}

/// See `OperatorGain`. Like making an admin, any gain takes the caller's own
/// password (`needs_my_proof`): a stolen session must not be able to give
/// itself a way back in that can disarm the system.
fn operator_gain(cfg: &crate::config::Config, name: &str, w: &Wanted, disabled: Option<bool>) -> OperatorGain {
    let legacy = cfg.server.auth.as_ref().is_some_and(|a| a.username.trim() == name);
    let target = cfg.server.users.iter().find(|u| u.username.trim() == name);
    let was_admin = legacy || target.is_some_and(|u| u.admin);
    let was_op = !legacy && target.is_some_and(|u| u.is_operator());
    // Making an admin is needs_my_proof's; an admin made an operator has less.
    if w.admin.unwrap_or(was_admin) || was_admin {
        return OperatorGain::None;
    }
    if !w.operator.unwrap_or(was_op) {
        return OperatorGain::None;
    }
    if !was_op {
        return OperatorGain::More;
    }
    let old = target.map(|u| u.perms).unwrap_or_default();
    let new = w.perms.unwrap_or(old);
    let gained = (new.ptz && !old.ptz) || (new.talk_clip && !old.talk_clip) || (new.arm && !old.arm) || (new.events && !old.events) || (new.camera_detection && !old.camera_detection);
    // More cameras: limits taken away, or a camera added to them.
    let old_cams = target.map(|u| u.cameras.clone()).unwrap_or_default();
    let wider = w.cameras.as_ref().is_some_and(|c| (c.is_empty() && !old_cams.is_empty()) || (!old_cams.is_empty() && c.iter().any(|x| !old_cams.contains(x))));
    if gained || wider {
        OperatorGain::More
    } else if disabled == Some(false) && target.is_some_and(|u| u.disabled) {
        OperatorGain::Enabled
    } else {
        OperatorGain::None
    }
}

/// What a request asks for, read once.
struct Wanted {
    admin: Option<bool>,
    operator: Option<bool>,
    perms: Option<crate::config::OperatorPerms>,
    cameras: Option<Vec<String>>,
}

/// `needs_my_proof`, and giving operator rights.
fn needs_proof(cfg: &crate::config::Config, who: &str, name: &str, w: &Wanted, password: bool, disabled: Option<bool>) -> bool {
    needs_my_proof(cfg, who, name, w.admin, password, disabled) || (who != "(open)" && operator_gain(cfg, name, w, disabled) != OperatorGain::None)
}

/// `POST /api/users {"username", "password"?, "admin"?, "cameras"?, "disabled"?}`:
/// create (password required) or update an account. Admins only (see
/// admin_only_path and viewer_may_write in web.rs).
pub async fn save_user(
    State(engine): State<Arc<Engine>>,
    caller: Option<axum::Extension<crate::web::Caller>>,
    headers: axum::http::HeaderMap,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> Response {
    // (Read here, so a mistake in it, such as a right's name misspelled, is a 400 that says what.)
    let mut req: UserReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad request: {e}")).into_response(),
    };
    let name = req.username.trim().to_string();
    let who = caller.as_ref().map(|c| c.0 .0.clone()).unwrap_or_default();
    let generation = caller.as_ref().map(|c| c.0 .1).unwrap_or(0);
    // Editing your own account keeps the session you are using (sessions
    // still pick up the new role at once; other sessions end).
    let keep = if who == name {
        let acc = crate::auth::accounts(&engine.config.lock().unwrap());
        crate::auth::session_from_headers(&headers, &acc).map(|(t, _)| t.to_string())
    } else {
        None
    };
    if name.is_empty() || name.contains(':') {
        return (StatusCode::BAD_REQUEST, "username must not be empty or contain ':'").into_response();
    }
    if let Some(p) = &req.password {
        if p.chars().count() < MIN_PASSWORD {
            return (StatusCode::BAD_REQUEST, format!("password must be at least {MIN_PASSWORD} characters")).into_response();
        }
    }
    if req.disabled == Some(true) && who == name {
        return (StatusCode::BAD_REQUEST, "you cannot disable the account you are signed in with").into_response();
    }
    // Your own password: only with your current one (and code), at /api/me/password.
    if req.password.is_some() && who == name {
        return (StatusCode::BAD_REQUEST, "use /api/me/password for your own password (Your account, Change password)").into_response();
    }
    // The slow check after all that costs nothing to check, so that no code is
    // used up on a change that would be refused anyway. The account is checked
    // again under the lock below.
    let w = match wanted_role(req.role.as_deref(), req.admin) {
        Ok((admin, operator)) => Wanted { admin, operator, perms: req.perms.take().map(Into::into), cameras: req.cameras.clone() },
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    let (proved, gives_admin, op_gain) = {
        let cfg = engine.config.lock().unwrap();
        if let Some(why) = plain_refusal(&cfg, &name, w.admin, req.disabled, req.password.is_some()) {
            return (StatusCode::BAD_REQUEST, why).into_response();
        }
        (
            needs_proof(&cfg, &who, &name, &w, req.password.is_some(), req.disabled),
            needs_my_proof(&cfg, &who, &name, w.admin, false, req.disabled),
            operator_gain(&cfg, &name, &w, req.disabled),
        )
    };
    if proved {
        if let Err(r) = crate::account_http::prove_admin(&engine, &who, peer.ip(), &headers, &req.proof, generation).await {
            return r;
        }
    }
    // Argon2 is slow on purpose: hash off the async runtime.
    let hash = match req.password.clone() {
        Some(p) => match crate::auth::password_check(peer.ip(), move || crate::auth::hash_password(&p)).await {
            Ok(h) => Some(h),
            Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, crate::auth::PASSWORD_BUSY).into_response(),
        },
        None => None,
    };
    // A hash that failed must never be saved: it would lock the account out.
    if hash.as_deref().is_some_and(|h| !crate::auth::is_hashed(h)) {
        tracing::error!("could not hash a password");
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not set the password; nothing was changed. See the server's log").into_response();
    }
    let pw_changed = req.password.is_some();
    let (created, turned, changed, lost_admin, target_was_admin) = {
        let mut cfg = engine.config.lock().unwrap();
        migrate_legacy(&mut cfg);
        // (Checked again here: it may have been disabled while this request waited for the lock.)
        if !caller_may_change_accounts(&cfg, &who) {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        }
        // (The account asked about may have changed during the slow steps: a
        // change that needs the proof now must have brought it, from this
        // caller as it is now.)
        if proved && crate::auth::generation(&who) != generation {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        }
        if !proved && needs_proof(&cfg, &who, &name, &w, req.password.is_some(), req.disabled) {
            return (StatusCode::CONFLICT, "this account changed meanwhile: try again").into_response();
        }
        let before = cfg.server.users.clone();
        let target_was_admin = cfg.server.users.iter().any(|u| u.username.trim() == name && u.admin);
        let was_admins = admins(&cfg);
        let created;
        let mut turned = None;
        let mut changed = true;
        let mut lost_admin = false;
        let mut lost_operator = false;
        match cfg.server.users.iter_mut().find(|u| u.username.trim() == name) {
            Some(u) => {
                // The server always keeps an admin who can sign in.
                if u.admin && !u.disabled && was_admins <= 1 {
                    if w.admin == Some(false) {
                        return (StatusCode::BAD_REQUEST, "cannot remove admin rights from the last admin").into_response();
                    }
                    if req.disabled == Some(true) {
                        return (StatusCode::BAD_REQUEST, "cannot disable the last admin who can sign in").into_response();
                    }
                }
                let was = u.clone();
                lost_admin = u.admin && w.admin == Some(false);
                if let Some(h) = hash {
                    u.password = h;
                }
                if let Some(a) = w.admin {
                    u.admin = a;
                }
                // (An admin is never also marked an operator; one that stops
                // being an admin is a viewer unless made an operator.)
                if u.admin || (was.admin && w.operator.is_none()) {
                    u.role.clear();
                } else if let Some(o) = w.operator {
                    u.role = if o { "operator".into() } else { String::new() };
                }
                // (Rights are kept for operators only.)
                if let Some(p) = w.perms.filter(|_| u.is_operator()) {
                    u.perms = p;
                }
                lost_operator = was.is_operator() && !u.is_operator() && !u.admin;
                // (Only a real change ends what the account has open.)
                changed = was.password != u.password || was.admin != u.admin || was.role != u.role || was.perms != u.perms || req.cameras.as_ref().is_some_and(|c| *c != u.cameras) || req.disabled.is_some_and(|d| d != u.disabled);
                if let Some(c) = req.cameras {
                    u.cameras = c;
                }
                if let Some(d) = req.disabled {
                    if d != u.disabled {
                        turned = Some(d);
                    }
                    u.disabled = d;
                }
                created = false;
            }
            None => {
                if let Err(e) = check_new_name(&name) {
                    return (StatusCode::BAD_REQUEST, e).into_response();
                }
                let Some(h) = hash else {
                    return (StatusCode::BAD_REQUEST, "a password is required for a new user").into_response();
                };
                // Rows left under this name (an account gone from the config file)
                // give a new person nothing: cleared first, or it is not created.
                if !crate::auth::clear_account_data(&name) {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "could not prepare the new account; nothing was changed. Try again, or see the server's log").into_response();
                }
                // The first account is always an admin (it turns sign-in on), and can sign in.
                let first = cfg.server.users.is_empty();
                let admin = w.admin.unwrap_or(false) || first;
                let role = if !admin && w.operator == Some(true) { "operator".to_string() } else { String::new() };
                let perms = if role.is_empty() { Default::default() } else { w.perms.unwrap_or_default() };
                cfg.server.users.push(UserConfig { username: name.clone(), password: h, admin, cameras: req.cameras.unwrap_or_default(), disabled: req.disabled.unwrap_or(false) && !first, role, perms });
                created = true;
            }
        }
        if let Err(e) = cfg.save(&engine.config_path) {
            // Not half done: a change that cannot be kept is not made at all.
            tracing::error!("could not save config: {e:#}");
            cfg.server.users = before;
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not save the change; nothing was changed. See the server's log").into_response();
        }
        // What the command line asked for before this and still waits must not undo it.
        if pw_changed {
            crate::account_http::supersede_requests(&engine, &name, "password");
        }
        if turned == Some(true) {
            crate::account_http::supersede_requests(&engine, &name, "enable");
        }
        // In the same step: a demoted admin's or operator's control tokens go
        // for good, and a disabled account's tokens all go (two-step codes stay
        // for when it is enabled again). If they cannot, the change is taken back.
        let tokens_gone = if turned == Some(true) {
            crate::auth::delete_user_tokens(&name)
        } else if lost_admin || lost_operator {
            crate::auth::delete_admin_tokens(&name)
        } else {
            true
        };
        if !tokens_gone {
            return take_back(&mut cfg, before, &engine.config_path, "could not remove this account's device tokens");
        }
        (created, turned, changed, lost_admin, target_was_admin)
    };
    // (A disabled account keeps nothing: every session and sign-in code ends,
    // so turning it back on does not bring an old device back.)
    // (Outside the config lock, which the sending takes too. Before any lock is cleared below.)
    if pw_changed && !created {
        crate::account_http::tell_of_password_change(&engine, &name, &format!("by {who}"));
    }
    let gives_operator = op_gain == OperatorGain::More;
    if proved {
        let what = match (gives_admin, gives_operator, pw_changed && !created) {
            (true, _, true) => "admin rights given and password set for another account, confirmed with own password",
            (true, _, false) => "admin rights given, confirmed with own password",
            (false, true, true) => "operator rights given and password set for another account, confirmed with own password",
            (false, true, false) => "operator rights given, confirmed with own password",
            _ => "password set for another account, confirmed with own password",
        };
        let _ = engine.db.insert_audit(&who, &peer.ip().to_string(), what, &name, 200);
    }
    let keep_now = if turned == Some(true) { None } else { keep.as_deref() };
    // (Read before the change; told after it, on a task of their own: nothing here waits for a push service.)
    let losing = if pw_changed && changed { crate::account_http::rows_losing_alerts(&engine, &name, &crate::auth::devices_ending(&name, keep_now)) } else { Vec::new() };
    let mut browsers_left = None;
    let sessions_gone = !changed || if pw_changed {
        match crate::auth::password_changed(&name, keep_now) {
            Ok(()) => true,
            Err(f) => {
                let ended = f != crate::auth::PwFail::Sessions;
                browsers_left = Some(f);
                ended
            }
        }
    } else {
        crate::auth::forget_user(&name, keep_now)
    };
    crate::account_http::tell_devices_detached(&engine, losing, format!("password changed for {name}"), format!("The password of {name} was changed (by {who}), so this device was signed out. Sign in here again to keep getting alerts."));
    // A new password: whoever kept guessing codes with the old one is out of it.
    if pw_changed {
        crate::auth::code_clear(&name);
    }
    // Link codes not used yet: one could add a server by which to come back.
    if turned == Some(true) || lost_admin {
        crate::links::cancel_codes();
    }
    // An admin losing that hears it first, on their own devices: after this they
    // get no alerts, and if a stolen session is doing this nobody else may be left.
    if who != "(open)" && target_was_admin && (turned == Some(true) || lost_admin) {
        let subs = engine.db.push_list(Some(&name)).unwrap_or_default();
        let what = if lost_admin { "took the admin rights of this account away" } else { "disabled your account" };
        crate::account_http::tell_devices_first(&engine, subs, format!("admin {name}: {what}"), format!("{who} {what} (from {}). If that was not meant to happen, act now: this may be the last alert on this device.", peer.ip())).await;
    }
    if let Some(d) = turned {
        let _ = engine.db.insert_audit(&who, "", if d { "account disabled" } else { "account enabled" }, &name, 200);
        // Its devices stop getting notifications and stop counting as someone
        // at home, for good: they have to be set up again after it is enabled.
        let (e, n) = (engine.clone(), name.clone());
        let _ = tokio::task::spawn_blocking(move || {
            if d {
                let _ = e.db.push_delete_user(&n);
                let _ = e.db.presence_delete_user(&n);
            }
            e.presence_evaluate(None);
        })
        .await;
    }
    // Giving up your own admin rights takes nothing either, and is no less news.
    if lost_admin && who == name && who != "(open)" {
        crate::account_http::tell_admins(&engine, format!("admin {name} demoted"), format!("{who} gave up the admin rights of their own account (from {}). If that was not meant to happen, another admin can give them back.", peer.ip()));
    }
    // Disabling and creating viewers stay quick, with no password asked: admins hear of them instead.
    if who != "(open)" && who != name {
        let tell = crate::account_http::tell_admins_once;
        let every = 60;
        // A burst of new viewers is one message; a new admin is always its own,
        // so a viewer created just before cannot hide it.
        if created && w.admin == Some(true) {
            crate::account_http::tell_admins(&engine, format!("new account {name}"), format!("{who} created the account {name}, an admin."));
        } else if created && gives_operator {
            crate::account_http::tell_admins(&engine, format!("new account {name}"), format!("{who} created the account {name}, an operator (can arm and disarm, and control cameras)."));
        } else if created {
            crate::account_http::tell_new_account(&engine, &who, &name);
        }
        // (A proof was given, but a stolen password and code can give that: never held back.)
        if !created && !target_was_admin && w.admin == Some(true) {
            crate::account_http::tell_admins(&engine, format!("admin rights given to {name}"), format!("{who} made {name} an admin (from {}). If that was not meant to happen, take the rights away in Users.", peer.ip()));
        }
        if !created && gives_operator {
            crate::account_http::tell_admins(&engine, format!("operator rights given to {name}"), format!("{who} gave {name} more operator rights or cameras (from {}). If that was not meant to happen, change them in Users.", peer.ip()));
        }
        if op_gain == OperatorGain::Enabled {
            crate::account_http::tell_admins(&engine, format!("operator {name} enabled"), format!("{who} enabled the operator account {name} (from {}). If that was not meant to happen, disable it again in Users.", peer.ip()));
        }
        if turned == Some(false) && target_was_admin {
            crate::account_http::tell_admins(&engine, format!("admin {name} enabled"), format!("{who} enabled the admin account {name}. If that was not meant to happen, disable it again in Users."));
        }
        if turned == Some(true) && target_was_admin {
            tell(&engine, &format!("admin-disabled:{who}:{name}"), every, format!("admin {name} disabled"), format!("{who} disabled the admin account {name}. If that was not meant to happen, enable it again from the computer: ambersight user enable {name} --config <file>"));
        }
        if lost_admin {
            tell(&engine, &format!("admin-demoted:{who}:{name}"), every, format!("admin {name} demoted"), format!("{who} took the admin rights of {name} away."));
        }
    }
    if created {
        let _ = engine.db.insert_audit(&who, &peer.ip().to_string(), "account created", &name, 200);
    }
    if !sessions_gone {
        return crate::account_http::sessions_still_stored();
    }
    if let Some(f) = browsers_left {
        return crate::account_http::password_cleanup_failed(f);
    }
    Json(serde_json::json!({ "username": name, "created": created })).into_response()
}

/// `DELETE /api/users/:name`
pub async fn delete_user(
    State(engine): State<Arc<Engine>>,
    caller: Option<axum::Extension<crate::web::Caller>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    AxPath(name): AxPath<String>,
) -> Response {
    let who = caller.as_ref().map(|c| c.0 .0.clone()).unwrap_or_default();
    let name = name.trim().to_string();
    if who == name {
        return (StatusCode::BAD_REQUEST, "you cannot delete the account you are signed in with").into_response();
    }
    let (was_admin, sessions_gone) = {
        let mut cfg = engine.config.lock().unwrap();
        migrate_legacy(&mut cfg);
        if !caller_may_change_accounts(&cfg, &who) {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        }
        let name = name.as_str();
        let Some(i) = cfg.server.users.iter().position(|u| u.username.trim() == name) else {
            return (StatusCode::NOT_FOUND, "no such user").into_response();
        };
        let enabled_here = cfg.server.users.iter().filter(|u| u.username.trim() == name && u.admin && !u.disabled).count();
        if cfg.server.users[i].admin && enabled_here > 0 && admins(&cfg) <= enabled_here {
            return (StatusCode::BAD_REQUEST, "cannot delete the last admin").into_response();
        }
        let before = cfg.server.users.clone();
        let was_admin = cfg.server.users.iter().any(|u| u.username.trim() == name && u.admin);
        cfg.server.users.retain(|u| u.username.trim() != name);
        if let Err(e) = cfg.save(&engine.config_path) {
            tracing::error!("could not save config: {e:#}");
            cfg.server.users = before;
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not save the change; nothing was changed. See the server's log").into_response();
        }
        // Its two-step codes and tokens go with it, or it is not deleted.
        if !crate::auth::clear_account_data(name) {
            return take_back(&mut cfg, before, &engine.config_path, "could not remove this account's two-step codes and tokens");
        }
        let gone = crate::auth::forget_user(name, None);
        crate::auth::code_clear(name);
        crate::links::cancel_codes();
        (was_admin, gone)
    };
    if was_admin {
        // Their own devices first: this is the last they would hear.
        let subs = engine.db.push_list(Some(&name)).unwrap_or_default();
        crate::account_http::tell_devices_first(&engine, subs, format!("admin {name}: your account was deleted"), format!("{who} deleted your account (from {}). If that was not meant to happen, act now: this is the last alert on this device.", peer.ip())).await;
        crate::account_http::tell_admins_once(&engine, &format!("admin-deleted:{who}:{name}"), 60, format!("admin {name} deleted"), format!("{who} deleted the admin account {name}."));
    }
    // Their devices stop getting notifications, and their phones no longer
    // count as someone at home. (Off the async workers, with no lock held:
    // see Engine::presence_evaluate.)
    let (e, n) = (engine.clone(), name.clone());
    let _ = tokio::task::spawn_blocking(move || {
        let _ = e.db.push_delete_user(&n);
        if e.db.presence_delete_user(&n).unwrap_or(0) > 0 {
            e.presence_evaluate(None);
        }
    })
    .await;
    if !sessions_gone {
        return crate::account_http::sessions_still_stored();
    }
    Json(serde_json::json!({ "deleted": name })).into_response()
}

#[derive(serde::Deserialize)]
struct PwReq {
    current: String,
    new: String,
    /// With two-step on: a code, or a recovery code.
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
}

/// `POST /api/me/password {"current", "new", "code"?, "recovery_code"?}`: any
/// signed-in user; the code is needed when the account has two-step codes on.
pub async fn change_own_password(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let (user, generation) = req.extensions().get::<crate::web::Caller>().map(|c| (c.0.clone(), c.1)).unwrap_or_default();
    let ip = crate::web::client_ip(&req);
    // Keep the session this change is made from; end the others.
    let a = crate::account_http::actor(&engine, &user, ip, req.headers());
    let bytes = match axum::body::to_bytes(req.into_body(), 16 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad request").into_response(),
    };
    let Ok(body) = serde_json::from_slice::<PwReq>(&bytes) else {
        return (StatusCode::BAD_REQUEST, "expected {\"current\", \"new\"}").into_response();
    };
    if body.new.chars().count() < MIN_PASSWORD {
        return (StatusCode::BAD_REQUEST, format!("password must be at least {MIN_PASSWORD} characters")).into_response();
    }
    // Always the current password, and the code when two-step is on. There is
    // no way round that: someone with a session and the password cannot get
    // past it by causing a lock. A recovery code does instead of a code.
    if let Err(r) = crate::account_http::prove(&engine, &a, &body.current, body.code.as_deref(), body.recovery_code.as_deref(), true).await {
        return r;
    }
    let new = body.new.clone();
    let hash = match crate::auth::password_check(ip, move || crate::auth::hash_password(&new)).await {
        Ok(h) => h,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, crate::auth::PASSWORD_BUSY).into_response(),
    };
    // A hash that failed must never be saved: it would lock the account out.
    if !crate::auth::is_hashed(&hash) {
        tracing::error!("could not hash a password");
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not set the password; nothing was changed. See the server's log").into_response();
    }
    {
    let mut cfg = engine.config.lock().unwrap();
    // (All that checking is slow: the account may have changed since.)
    if crate::auth::generation(&user) != generation {
        return (StatusCode::CONFLICT, "this account changed meanwhile: try again").into_response();
    }
    migrate_legacy(&mut cfg);
    let before = cfg.server.users.clone();
    match cfg.server.users.iter_mut().find(|x| x.username.trim() == user) {
        Some(x) => x.password = hash,
        None => return (StatusCode::NOT_FOUND, "no such user").into_response(),
    }
    if let Err(e) = cfg.save(&engine.config_path) {
        tracing::error!("could not save config: {e:#}");
        cfg.server.users = before;
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not save the new password; nothing was changed. See the server's log").into_response();
    }
    // What the command line asked for before this and still waits must not undo it.
    crate::account_http::supersede_requests(&engine, &user, "password");
    }
    crate::account_http::tell_of_password_change(&engine, &user, "by the person, from a signed-in session");
    // Devices that sign out with this (and lose their alerts) are told from the list read
    // now, after the change, on a task of their own: no wait here for a push service, and a
    // client that leaves cannot stop the sessions from ending.
    let losing = crate::account_http::rows_losing_alerts(&engine, &user, &crate::auth::devices_ending(&user, a.session.as_deref()));
    let gone = crate::auth::password_changed(&user, a.session.as_deref());
    crate::account_http::tell_devices_detached(&engine, losing, format!("password changed for {user}"), format!("The password of {user} was changed (from {}), so this device was signed out. Sign in here again to keep getting alerts.", a.ip));
    // Whoever kept guessing codes with the old password is out of it now.
    crate::auth::code_clear(&user);
    if let Err(f) = gone {
        return crate::account_http::password_cleanup_failed(f);
    }
    Json(serde_json::json!({ "changed": true })).into_response()
}

#[derive(serde::Deserialize)]
pub struct PairCodeReq {
    /// The address the phone should open, e.g. `https://192.168.1.5:8090`.
    origin: String,
    /// The caller's own password and code, when this is a code for an admin account.
    #[serde(flatten)]
    proof: crate::account_http::MyProof,
}

/// `POST /api/users/:name/pair {"origin"}` (admin) → a one-time sign-in link
/// for the user and its QR code (SVG). Scanning it on a phone opens the app
/// already signed in; the code works once and expires after 10 minutes.
pub async fn pair_code(State(engine): State<Arc<Engine>>, caller: Option<axum::Extension<crate::web::Caller>>, headers: axum::http::HeaderMap, axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>, AxPath(name): AxPath<String>, Json(req): Json<PairCodeReq>) -> Response {
    let who = caller.as_ref().map(|c| c.0 .0.clone()).unwrap_or_default();
    let generation = caller.as_ref().map(|c| c.0 .1).unwrap_or(0);
    if engine.config.lock().unwrap().server.users.iter().any(|u| u.username == name && u.disabled) {
        return (StatusCode::BAD_REQUEST, "this account is disabled: enable it first").into_response();
    }
    if !crate::auth::accounts(&engine.config.lock().unwrap()).iter().any(|(u, _, _)| *u == name) {
        return (StatusCode::NOT_FOUND, "no such user").into_response();
    }
    let origin = req.origin.trim().trim_end_matches('/');
    let ok_origin = (origin.starts_with("https://") || origin.starts_with("http://"))
        && origin.len() < 200
        && !origin[origin.find("//").unwrap_or(0) + 2..].contains(['/', '?', '#', '@', ' ']);
    if !ok_origin {
        return (StatusCode::BAD_REQUEST, "origin must look like https://host:port").into_response();
    }
    // A code for an admin (or operator) account is those rights, and for an account
    // with two-step codes it skips those, with no password or code: either takes
    // the caller's own. The slow check first, then again below.
    let is_admin = |cfg: &crate::config::Config| crate::auth::accounts(cfg).iter().any(|(u, _, a)| *u == name && *a);
    let sensitive = |cfg: &crate::config::Config| is_admin(cfg) || crate::auth::is_operator(cfg, &name) || crate::auth::two_step_on(&name) != Ok(false);
    // (Read before looking: if it changes after this, no code is made below.)
    let target_generation = crate::auth::generation(&name);
    let proved = sensitive(&engine.config.lock().unwrap());
    if proved {
        if let Err(r) = crate::account_http::prove_admin(&engine, &who, peer.ip(), &headers, &req.proof, generation).await {
            return r;
        }
    }
    let admin_target = {
        let cfg = engine.config.lock().unwrap();
        if !caller_may_change_accounts(&cfg, &who) || (proved && crate::auth::generation(&who) != generation) {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        }
        if !proved && sensitive(&cfg) {
            return (StatusCode::CONFLICT, "this account changed meanwhile: try again").into_response();
        }
        is_admin(&cfg)
    };
    // (An admin vouches for this person: the code is what lets a phone in
    // without the password, and without a two-step code. Kept on record.)
    let two_step = crate::auth::two_step_on(&name) != Ok(false);
    let _ = engine.db.insert_audit(&who, "", if two_step { "phone sign-in code made (skips this account's two-step code)" } else { "phone sign-in code made" }, &name, 200);
    // (A viewer's session never becomes an admin's if the account is promoted later.)
    let Some(code) = crate::auth::new_pair_code_if_current(&name, admin_target, target_generation) else {
        return (StatusCode::CONFLICT, "this account changed meanwhile: try again").into_response();
    };
    let url = format!("{origin}/#pair={code}");
    let svg = match qrcode::QrCode::with_error_correction_level(url.as_bytes(), qrcode::EcLevel::M) {
        Ok(q) => q.render::<qrcode::render::svg::Color>().min_dimensions(240, 240).quiet_zone(true).build(),
        Err(e) => {
            tracing::error!("QR code: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not draw the QR code").into_response();
        }
    };
    Json(serde_json::json!({ "url": url, "svg": svg, "expires_in": crate::auth::PAIR_SECS })).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn what_takes_the_callers_own_password() {
        use super::needs_my_proof as needs;
        let cfg: crate::config::Config = toml::from_str("[server.auth]\nusername = \"old\"\npassword = \"x\"\n[[server.users]]\nusername = \"boss\"\npassword = \"x\"\n[[server.users]]\nusername = \"view\"\npassword = \"x\"\nadmin = false\n").unwrap();
        // Making an admin.
        assert!(needs(&cfg, "boss", "new", Some(true), true, None), "a new admin");
        assert!(needs(&cfg, "boss", "view", Some(true), false, None), "promoting");
        // Someone else's password, also an admin's or an old-style account's.
        assert!(needs(&cfg, "boss", "view", None, true, None));
        assert!(needs(&cfg, "view", "boss", None, true, None));
        assert!(needs(&cfg, "boss", "old", None, true, None));
        // Not these.
        assert!(!needs(&cfg, "boss", "new", Some(false), true, None), "a new viewer");
        assert!(!needs(&cfg, "boss", "new", None, true, None), "a new viewer by default");
        assert!(!needs(&cfg, "boss", "view", Some(false), false, None), "editing a viewer");
        assert!(!needs(&cfg, "boss", "view", None, false, None), "disabling and such");
        assert!(!needs(&cfg, "boss", "boss", Some(true), false, None), "an admin staying one");
        assert!(!needs(&cfg, "boss", "boss", None, true, None), "your own password has its own rule");
        assert!(!needs(&cfg, "(open)", "first", Some(true), true, None), "the first account of an open server");
        // Enabling a disabled admin is making an admin; a disabled viewer is not.
        let off: crate::config::Config = toml::from_str("[[server.users]]\nusername = \"boss\"\npassword = \"x\"\n[[server.users]]\nusername = \"adm\"\npassword = \"x\"\ndisabled = true\n[[server.users]]\nusername = \"vw\"\npassword = \"x\"\nadmin = false\ndisabled = true\n").unwrap();
        assert!(needs(&off, "boss", "adm", None, false, Some(false)));
        assert!(needs(&off, "boss", "adm", Some(true), false, Some(false)), "sending admin true with it changes nothing");
        assert!(!needs(&off, "boss", "adm", None, false, Some(true)), "disabling stays quick");
        assert!(!needs(&off, "boss", "adm", Some(false), false, Some(false)), "enabled as a viewer");
        assert!(!needs(&off, "boss", "vw", None, false, Some(false)), "a viewer stays quick");
        assert!(!needs(&off, "boss", "adm", None, false, None), "editing it, still disabled");
    }

    #[test]
    fn the_proof_comes_along_in_the_same_request() {
        let r: super::UserReq = serde_json::from_str(r#"{"username":"a","password":"new-one-123","admin":true,"my_password":"mine","my_code":"123456"}"#).unwrap();
        assert_eq!(r.proof.my_password.as_deref(), Some("mine"));
        assert_eq!(r.proof.my_code.as_deref(), Some("123456"));
        assert_eq!(r.password.as_deref(), Some("new-one-123"), "the other account's password is another field");
        let p: super::PairCodeReq = serde_json::from_str(r#"{"origin":"https://x:1"}"#).unwrap();
        assert!(p.proof.my_password.is_none());
        // Role and rights come along too; a right left out is on.
        let r: super::UserReq = serde_json::from_str(r#"{"username":"a","role":"operator","perms":{"arm":false},"my_password":"mine"}"#).unwrap();
        assert_eq!(r.role.as_deref(), Some("operator"));
        assert_eq!(r.perms.map(crate::config::OperatorPerms::from), Some(crate::config::OperatorPerms { arm: false, ..Default::default() }));
        // A right's name misspelled: refused, not quietly on.
        assert!(serde_json::from_str::<super::UserReq>(r#"{"username":"a","perms":{"arms":false}}"#).is_err());
        assert_eq!(r.proof.my_password.as_deref(), Some("mine"));
    }

    #[test]
    fn role_and_admin_are_read_together() {
        use super::wanted_role as w;
        assert_eq!(w(None, None), Ok((None, None)), "left as it is");
        assert_eq!(w(None, Some(true)), Ok((Some(true), Some(false))), "an admin is no operator");
        assert_eq!(w(None, Some(false)), Ok((Some(false), None)), "an older page: an operator stays one");
        assert_eq!(w(Some("operator"), None), Ok((Some(false), Some(true))));
        assert_eq!(w(Some("viewer"), Some(false)), Ok((Some(false), Some(false))));
        assert_eq!(w(Some("admin"), Some(true)), Ok((Some(true), Some(false))));
        assert!(w(Some("operator"), Some(true)).is_err(), "they disagree");
        assert!(w(Some("admin"), Some(false)).is_err());
        assert!(w(Some("root"), None).is_err());
        assert!(w(Some(""), None).is_err());
    }

    #[test]
    fn giving_operator_rights_takes_the_callers_own_password() {
        use super::{needs_proof, Wanted};
        use crate::config::OperatorPerms;
        let cfg: crate::config::Config = toml::from_str(
            "[server.auth]\nusername = \"old\"\npassword = \"x\"\n\
             [[server.users]]\nusername = \"boss\"\npassword = \"x\"\n\
             [[server.users]]\nusername = \"view\"\npassword = \"x\"\nadmin = false\n\
             [[server.users]]\nusername = \"op\"\npassword = \"x\"\nadmin = false\nrole = \"operator\"\nperms = { arm = false }\n\
             [[server.users]]\nusername = \"opoff\"\npassword = \"x\"\nadmin = false\nrole = \"operator\"\ndisabled = true\n\
             [[server.users]]\nusername = \"lim\"\npassword = \"x\"\nadmin = false\nrole = \"operator\"\ncameras = [\"Porch\"]\n",
        )
        .unwrap();
        let w = |role: Option<&str>, admin: Option<bool>, perms: Option<OperatorPerms>| {
            let (admin, operator) = super::wanted_role(role, admin).unwrap();
            Wanted { admin, operator, perms, cameras: None }
        };
        let needs = |name: &str, x: Wanted, disabled: Option<bool>| needs_proof(&cfg, "boss", name, &x, false, disabled);
        let no_arm = Some(OperatorPerms { arm: false, ..Default::default() });
        let none = Some(OperatorPerms { ptz: false, talk_clip: false, arm: false, events: false, camera_detection: false });
        assert!(needs("new", w(Some("operator"), None, None), None), "a new operator");
        assert!(needs("new", w(Some("operator"), None, none), None), "even with no rights: it is made an operator");
        assert!(needs("view", w(Some("operator"), None, None), None), "a viewer made an operator");
        assert!(needs("op", w(None, None, Some(OperatorPerms::default())), None), "a right turned on");
        assert!(needs("opoff", w(None, None, None), Some(false)), "a disabled operator enabled");
        assert!(!needs("op", w(None, None, no_arm), None), "the same rights");
        assert!(!needs("op", w(None, None, none), None), "rights taken away");
        assert!(!needs("op", w(Some("viewer"), None, None), None), "made a viewer");
        assert!(!needs("op", w(None, None, None), Some(true)), "disabled");
        assert!(!needs("new", w(Some("viewer"), None, None), None), "a new viewer");
        assert!(!needs("view", w(None, None, Some(OperatorPerms::default())), None), "rights of a viewer do nothing");
        assert!(!needs("boss", w(Some("operator"), None, None), None), "an admin made an operator has less");
        assert!(!needs("old", w(Some("operator"), None, None), None), "the old-style admin too");
        assert!(needs("op", w(Some("admin"), None, None), None), "an operator made an admin");
        // Cameras: more of them is more rights; fewer is not.
        let cams = |c: &[&str]| Wanted { admin: None, operator: None, perms: None, cameras: Some(c.iter().map(|x| x.to_string()).collect()) };
        assert!(!needs("lim", cams(&["Porch"]), None), "the same");
        assert!(needs("lim", cams(&["Porch", "Gate"]), None), "one more");
        assert!(needs("lim", cams(&[]), None), "all of them");
        assert_eq!(super::operator_gain(&cfg, "opoff", &w(None, None, None), Some(false)), super::OperatorGain::Enabled);
        assert_eq!(super::operator_gain(&cfg, "op", &w(None, None, Some(OperatorPerms::default())), None), super::OperatorGain::More);
        // A server with no accounts: nothing to prove.
        assert!(!needs_proof(&cfg, "(open)", "new", &w(Some("operator"), None, None), false, None));
    }

    #[test]
    fn names_starting_with_a_parenthesis_are_kept_for_the_server() {
        assert!(super::check_new_name("(presence)").is_err());
        assert!(super::check_new_name("(x").is_err());
        assert!(super::check_new_name("ann").is_ok());
        assert!(super::check_new_name("ann (kitchen)").is_ok());
    }

    #[test]
    fn the_name_of_a_new_account_shows_what_it_is() {
        for bad in ["a\nb", "a\tb", "a\u{7}b", "a\u{200b}b", "a\u{200d}b", "a\u{202e}b", "a\u{2066}b", "a\u{ad}b", "a\u{feff}b"] {
            assert!(super::check_new_name(bad).is_err(), "{bad:?}");
        }
        assert!(super::check_new_name(&"x".repeat(super::MAX_NAME + 1)).is_err());
        assert!(super::check_new_name(&"é".repeat(super::MAX_NAME)).is_ok(), "characters, not bytes");
        for good in ["Zoë Müller", "ann.b-c_d", "手机"] {
            assert!(super::check_new_name(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn an_account_that_exists_keeps_working_with_any_name() {
        let cfg: crate::config::Config = toml::from_str("[[server.users]]\nusername = \"boss\"\npassword = \"x\"\n[[server.users]]\nusername = \"odd\\u202Ename\"\npassword = \"x\"\nadmin = false\n").unwrap();
        assert_eq!(super::plain_refusal(&cfg, "odd\u{202e}name", None, None, false), None, "an edit of it is no new name");
        assert!(super::plain_refusal(&cfg, "new\u{202e}name", None, None, true).is_some());
    }
}
