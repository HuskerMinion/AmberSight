//! Web API for presence (geofencing, see engine/presence.rs). Every
//! signed-in user, viewers included, reports and removes their own phones;
//! admins see and remove everyone's.

use axum::{
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

use crate::db::PresenceDevice;
use crate::engine::presence::{self, MAX_PER_USER, MAX_TOTAL};
use crate::engine::Engine;
use crate::web::{Caller, IsAdmin};

/// A report from an account whose phones do not count.
const NOT_COUNTED: &str = "this account does not report presence on this server";

/// The account a device is stored under: the caller ("" without accounts).
fn owner(caller: &Caller) -> String {
    if caller.0 == "(open)" { String::new() } else { caller.0.clone() }
}

#[derive(serde::Deserialize)]
pub struct ReportReq {
    device_id: String,
    #[serde(default)]
    name: String,
    home: bool,
}

/// The device row a report makes (checked), stored under the caller.
fn device_from(user: &str, r: &ReportReq, now_ms: i64) -> Result<PresenceDevice, String> {
    Ok(PresenceDevice {
        user: user.to_string(),
        device_id: presence::clean_device_id(&r.device_id)?,
        name: presence::clean_name(&r.name)?,
        home: r.home,
        updated_ms: now_ms,
        created_ms: now_ms,
    })
}

/// `POST /api/presence {"device_id", "name", "home"}`: this phone is at
/// home or away.
pub async fn report(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, Json(r): Json<ReportReq>) -> Response {
    let d = match device_from(&owner(&caller), &r, chrono::Utc::now().timestamp_millis()) {
        Ok(d) => d,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    if !engine.presence_counts(&d.user) {
        return (StatusCode::FORBIDDEN, NOT_COUNTED).into_response();
    }
    let e = engine.clone();
    let res = tokio::task::spawn_blocking(move || {
        let r = e.db.presence_report(&d, MAX_PER_USER, MAX_TOTAL);
        if matches!(r, Ok(Ok(()))) {
            e.presence_evaluate(Some(&presence::device_label(&d.name, &d.user)));
        }
        r
    })
    .await;
    match res {
        Ok(Ok(Ok(()))) => {
            let cfg = {
                let c = engine.config.lock().unwrap();
                c.presence.for_rule(&c.automation)
            };
            let home = engine.presence_home();
            Json(serde_json::json!({
                "ok": true,
                "enabled": cfg.enabled,
                "home_now": home,
                "state": presence::state_word(home),
                "rule": presence::away_rule(&cfg),
            }))
            .into_response()
        }
        Ok(Ok(Err(limit))) => (StatusCode::CONFLICT, limit).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response(),
    }
}

/// One device as the API shows it. `counted`: the rule counts its account.
fn device_json(d: &PresenceDevice, now_ms: i64, stale_hours: u32, counted: bool) -> serde_json::Value {
    serde_json::json!({
        "user": d.user,
        "device_id": d.device_id,
        "name": d.name,
        "home": d.home,
        "updated_ms": d.updated_ms,
        "created_ms": d.created_ms,
        "stale": !presence::fresh(d, now_ms, stale_hours),
        "counted": counted,
    })
}

/// Whose devices a caller may list: None = everyone's (admins).
fn list_scope(caller: &Caller, admin: bool) -> Option<String> {
    (!admin).then(|| owner(caller))
}

/// `GET /api/presence`: admins get every device and the rule's state;
/// others their own devices, and whether anyone is home if their phones
/// count.
pub async fn list(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, axum::Extension(admin): axum::Extension<IsAdmin>) -> Response {
    let scope = list_scope(&caller, admin.0);
    let e = engine.clone();
    let devs = match tokio::task::spawn_blocking(move || e.db.presence_list(scope.as_deref())).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response(),
    };
    let (cfg, accounts) = {
        let c = engine.config.lock().unwrap();
        (c.presence.for_rule(&c.automation), presence::account_names(&c))
    };
    let now = chrono::Utc::now().timestamp_millis();
    let (home, pending, last) = engine.presence_status();
    Json(list_json(&cfg, &accounts, &owner(&caller), admin.0, &devs, now, (home, pending, last))).into_response()
}

/// The body of `GET /api/presence`.
fn list_json(
    cfg: &crate::config::PresenceConfig,
    accounts: &[String],
    me: &str,
    admin: bool,
    devs: &[PresenceDevice],
    now: i64,
    (home, pending, last): (Option<bool>, Option<i64>, Option<presence::LastAction>),
) -> serde_json::Value {
    let mut j = serde_json::json!({
        "enabled": cfg.enabled,
        "stale_hours": cfg.stale_hours,
        "devices": devs.iter().map(|d| device_json(d, now, cfg.stale_hours, cfg.counts(&d.user, accounts))).collect::<Vec<_>>(),
    });
    // (Whether anyone is home only to the accounts whose phones count.)
    if admin || cfg.counts(me, accounts) {
        j["anyone_home"] = serde_json::json!(home);
        j["state"] = serde_json::json!(presence::state_word(home));
    }
    if admin {
        j["pending_away_at"] = serde_json::json!(pending);
        j["last_action"] = serde_json::json!(last);
        j["rule"] = serde_json::json!(presence::away_rule(cfg));
        j["accounts"] = serde_json::json!(cfg.accounts);
    }
    j
}

#[derive(serde::Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    user: Option<String>,
}

/// Whose device a delete removes: the caller's own, or (admins only) the
/// account named in `?user=`.
fn delete_owner(caller: &Caller, admin: bool, user: Option<&str>) -> Result<String, (StatusCode, &'static str)> {
    let me = owner(caller);
    match user {
        Some(u) if u != me => {
            if admin {
                Ok(u.to_string())
            } else {
                Err((StatusCode::FORBIDDEN, "admin access required to remove another account's phone"))
            }
        }
        _ => Ok(me),
    }
}

/// `DELETE /api/presence/:device_id[?user=<account>]`.
pub async fn remove(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    axum::Extension(admin): axum::Extension<IsAdmin>,
    AxPath(device_id): AxPath<String>,
    Query(q): Query<DeleteQuery>,
) -> Response {
    let user = match delete_owner(&caller, admin.0, q.user.as_deref()) {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    let e = engine.clone();
    let id = device_id.clone();
    let res = tokio::task::spawn_blocking(move || {
        let r = e.db.presence_delete(&user, &id);
        if matches!(r, Ok(true)) {
            e.presence_evaluate(None);
        }
        r
    })
    .await;
    match res {
        Ok(Ok(true)) => Json(serde_json::json!({ "ok": true, "deleted": device_id })).into_response(),
        Ok(Ok(false)) => (StatusCode::NOT_FOUND, "no such device").into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(u: &str) -> Caller {
        Caller(u.into(), 0)
    }

    #[test]
    fn everyone_reports_only_their_own_phones() {
        let db = crate::db::Db::open_in_memory().unwrap();
        // A viewer's report is stored under the viewer, whatever it says.
        let r: ReportReq = serde_json::from_value(serde_json::json!({ "device_id": "v-1", "name": "Pixel", "home": true, "user": "admin" })).unwrap();
        let d = device_from(&owner(&c("viewer")), &r, 5).unwrap();
        assert_eq!(d.user, "viewer");
        db.presence_report(&d, MAX_PER_USER, MAX_TOTAL).unwrap().unwrap();
        db.presence_report(&device_from("admin", &ReportReq { device_id: "a-1".into(), name: "".into(), home: false }, 6).unwrap(), MAX_PER_USER, MAX_TOTAL).unwrap().unwrap();
        // Without accounts: stored under "".
        assert_eq!(owner(&c("(open)")), "");
        // Bad input.
        assert!(device_from("x", &ReportReq { device_id: "has space".into(), name: "n".into(), home: true }, 0).is_err());
        assert!(device_from("x", &ReportReq { device_id: "ok".into(), name: "n".repeat(65), home: true }, 0).is_err());
        assert!(serde_json::from_value::<ReportReq>(serde_json::json!({ "device_id": "x" })).is_err(), "home is required");

        // Listing: a viewer sees their own, an admin everyone's.
        let scope = list_scope(&c("viewer"), false);
        let mine = db.presence_list(scope.as_deref()).unwrap();
        assert_eq!(mine.iter().map(|d| d.device_id.as_str()).collect::<Vec<_>>(), ["v-1"]);
        assert_eq!(db.presence_list(list_scope(&c("admin"), true).as_deref()).unwrap().len(), 2);

        // Removing: a viewer cannot name another account; their own id
        // space never reaches another account's phone.
        assert_eq!(delete_owner(&c("viewer"), false, Some("admin")).unwrap_err().0, StatusCode::FORBIDDEN);
        assert_eq!(delete_owner(&c("viewer"), false, Some("viewer")).unwrap(), "viewer");
        let u = delete_owner(&c("viewer"), false, None).unwrap();
        assert!(!db.presence_delete(&u, "a-1").unwrap());
        // An admin can.
        let u = delete_owner(&c("admin"), true, Some("viewer")).unwrap();
        assert!(db.presence_delete(&u, "v-1").unwrap());
        assert_eq!(delete_owner(&c("admin"), true, None).unwrap(), "admin");
    }

    #[tokio::test]
    async fn an_account_not_listed_is_refused_and_nothing_is_stored() {
        let mut cfg: crate::config::Config = toml::from_str("").unwrap();
        for u in ["ann", "kiosk"] {
            cfg.server.users.push(crate::config::UserConfig { disabled: false, username: u.into(), password: "x".into(), admin: false, cameras: Vec::new() });
        }
        cfg.presence.accounts = vec!["ann".into()];
        let e = Engine::for_test(cfg);
        let req = |id: &str| Json(ReportReq { device_id: id.into(), name: "P".into(), home: true });
        let r = report(State(e.clone()), axum::Extension(c("kiosk")), req("k1")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(r.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], NOT_COUNTED.as_bytes());
        assert!(e.db.presence_list(None).unwrap().is_empty());
        let r = report(State(e.clone()), axum::Extension(c("ann")), req("a1")).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(e.db.presence_list(None).unwrap().len(), 1);
        // No list: every account reports.
        e.config.lock().unwrap().presence.accounts.clear();
        let r = report(State(e.clone()), axum::Extension(c("kiosk")), req("k1")).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn stale_flag_in_the_list() {
        let d = PresenceDevice { user: "a".into(), device_id: "x".into(), name: "X".into(), home: true, updated_ms: 0, created_ms: 0 };
        assert_eq!(device_json(&d, 3_600_000, 1, true)["stale"], false);
        assert_eq!(device_json(&d, 3_600_001, 1, true)["stale"], true);
    }

    #[test]
    fn only_listed_accounts_count_and_see_the_state() {
        let accounts: Vec<String> = ["ann", "bob", "kiosk"].map(String::from).to_vec();
        let mut cfg = crate::config::PresenceConfig { enabled: true, accounts: vec!["ann".into(), "bob".into()], ..Default::default() };
        let d = |u: &str| PresenceDevice { user: u.into(), device_id: "x".into(), name: "X".into(), home: true, updated_ms: 0, created_ms: 0 };
        let devs = vec![d("ann"), d("kiosk"), d("gone")];
        let st = (Some(true), None, None);
        // An admin sees every phone, with whether it counts.
        let j = list_json(&cfg, &accounts, "admin", true, &devs, 0, st.clone());
        let counted: Vec<bool> = j["devices"].as_array().unwrap().iter().map(|x| x["counted"].as_bool().unwrap()).collect();
        assert_eq!(counted, [true, false, false], "kiosk is not listed; gone is no account any more");
        assert_eq!(j["state"], "home");
        // A listed viewer sees the state; another one only their phones.
        assert_eq!(list_json(&cfg, &accounts, "ann", false, &devs[..1], 0, st.clone())["state"], "home");
        let k = list_json(&cfg, &accounts, "kiosk", false, &devs[1..2], 0, st.clone());
        assert!(k.get("state").is_none() && k.get("anyone_home").is_none());
        assert_eq!(k["devices"].as_array().unwrap().len(), 1);
        // No list: every account counts, but not a phone of a deleted one.
        cfg.accounts.clear();
        assert!(cfg.counts("kiosk", &accounts) && !cfg.counts("gone", &accounts));
        // An open server (no accounts): every phone counts, list or not.
        cfg.accounts = vec!["ann".into()];
        assert!(cfg.counts("", &[]));
        assert_eq!(list_json(&cfg, &[], "", false, &[d("")], 0, st)["state"], "home");
    }
}
