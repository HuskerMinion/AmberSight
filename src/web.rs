use axum::{
    extract::{Path as AxPath, Query, Request, State},
    body::Body,
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
    routing::{get, post, put},
    Router,
};
use std::sync::{Arc, Mutex};
use tower_http::services::ServeFile;

use crate::auth::{Perm, Role};
use crate::config::{AlertConfig, CameraConfig, MotionConfig, OdtConfig, StorageConfig};
use crate::engine::Engine;

// Web interface files (index, css, js, help images) live in static_files.rs.
#[path = "static_files.rs"]
mod static_files;

pub fn build_router(engine: Arc<Engine>) -> Router {
    spawn_ptz_checker(engine.clone());
    let api = Router::new()
        .route("/cameras", get(list_cameras).post(add_camera))
        .route("/cameras/:id", put(update_camera).delete(remove_camera))
        .route("/camera-defaults", get(camera_defaults))
        .route("/cameras/:id/enable", post(set_enabled))
        .route("/event-copy/test", post(event_copy_test))
        .route("/cameras/:id/ptz", post(ptz_action))
        .route("/cameras/:id/timesync", post(camera_timesync))
        .route("/timesync/all", post(timesync_all))
        .route("/cameras/:id/reboot", post(camera_reboot))
        .route("/cameras/:id/powercycle", post(camera_powercycle))
        .route("/cameras/:id/watchdog", get(camera_watchdog))
        .route("/cameras/:id/netinfo", get(camera_netinfo))
        .route("/cameras/:id/tamper", get(tamper_status))
        .route("/cameras/:id/tamper/reference", post(tamper_set_reference))
        .route("/cameras/:id/tamper/ref/:light", get(tamper_ref_picture))
        .route("/cameras/:id/tamper/incidents/:incident/:which", get(tamper_incident_picture))
        .route("/cameras/:id/unusual", get(unusual_status))
        .route("/cameras/:id/unusual/refresh", post(unusual_refresh))
        .route("/cameras/:id/spots", get(camera_spots))
        .route("/cameras/:id/spots/forget", post(forget_camera_spots))
        .route("/cameras/:id/ptz/presets", get(ptz_presets))
        .route("/cameras/:id/ptz/caps", get(ptz_caps))
        .route("/cameras/:id/onvif", get(onvif_get))
        .route("/cameras/:id/onvif/find", post(onvif_find))
        .route("/cameras/:id/ptz/aim", post(ptz_aim))
        .route("/ui/prefs", get(crate::ui_http::get_prefs).put(crate::ui_http::put_prefs).patch(crate::ui_http::patch_prefs))
        .route("/ui/orders", get(crate::ui_http::get_orders))
        .route("/ui/diag", post(crate::ui_http::diag))
        .route("/ui/orders/:key", put(crate::ui_http::put_order))
        .route("/discover", get(discover_devices))
        .route("/devices", get(local_devices))
        .route("/discover/inspect", post(discover_inspect))
        .route("/discover/autologin", post(discover_autologin))
        .route("/discover/preview", post(discover_preview))
        .route("/ai/model", get(ai_model_status).post(ai_model_get))
        .route("/alerts/system_test", post(system_alert_test))
        .route("/helpers", get(helpers_list))
        .route("/helpers/pair", post(helpers_pair))
        .route("/helpers/:id", put(helpers_rename).delete(helpers_remove))
        .route("/helpers/:id/sources", post(helpers_sources))
        .route("/helpers/:id/thumb", post(helpers_thumb))
        .route("/discover/logins", get(get_discovery_logins).put(put_discovery_logins))
        .route("/discover/watch", get(get_discovery_watch).put(put_discovery_watch))
        .route("/discover/ignore", post(discovery_ignore))
        .route("/cameras/:id/talk", get(camera_talk))
        .route("/shield", post(shield))
        .route("/cameras/:id/pause", post(pause_alerts))
        .route("/noisy", get(noisy_list))
        .route("/noisy/:id/dismiss", post(noisy_dismiss))
        .route("/profiles", get(list_profiles))
        .route("/profile", post(set_profile))
        .route("/clips/record", post(record_clip))
        .route("/status", get(status))
        .route("/stats", get(stats))
        .route("/whoami", get(whoami))
        .route("/login", post(login))
        .route("/login/totp", post(crate::account_http::login_totp))
        .route("/pair", post(pair))
        .route("/logout", post(logout))
        .route("/audit", get(audit_log))
        .route("/users", get(crate::users_http::list_users).post(crate::users_http::save_user))
        .route("/users/:name", axum::routing::delete(crate::users_http::delete_user))
        .route("/users/:name/pair", post(crate::users_http::pair_code))
        .route("/users/:name/tokens", get(crate::account_http::user_tokens))
        .route("/users/:name/tokens/:id", axum::routing::delete(crate::account_http::user_token_revoke))
        .route("/users/:name/two-step/off", post(crate::account_http::user_two_step_off))
        .route("/me/password", post(crate::users_http::change_own_password))
        .route("/me/account", get(crate::account_http::me_account))
        .route("/me/sessions/sign-out-others", post(crate::account_http::sessions_sign_out_others))
        .route("/me/sessions/:id/sign-out", post(crate::account_http::session_sign_out))
        .route("/me/two-step/start", post(crate::account_http::two_step_start))
        .route("/me/two-step/confirm", post(crate::account_http::two_step_confirm))
        .route("/me/two-step/cancel", post(crate::account_http::two_step_cancel))
        .route("/me/two-step/off", post(crate::account_http::two_step_off))
        .route("/me/two-step/recovery", post(crate::account_http::two_step_recovery))
        .route("/me/tokens", post(crate::account_http::token_create))
        .route("/me/tokens/:id", axum::routing::delete(crate::account_http::token_revoke))
        .route("/push/key", get(crate::push_http::key))
        .route("/push/subscribe", post(crate::push_http::subscribe))
        .route("/push/unsubscribe", post(crate::push_http::unsubscribe))
        .route("/push/devices", get(crate::push_http::devices))
        .route("/push/test", post(crate::push_http::test))
        .route("/restream", get(crate::restream::http::get).put(crate::restream::http::put))
        .route("/presence", get(crate::presence_http::list).post(crate::presence_http::report))
        .route("/presence/:device_id", axum::routing::delete(crate::presence_http::remove))
        .route("/health", get(health))
        .route("/connections", get(connections))
        .route("/links", get(crate::links::api_list).post(crate::links::api_link))
        .route("/links/code", post(crate::links::api_code))
        .route("/links/probe", post(crate::links::api_probe))
        .route("/links/:id", put(crate::links::api_rename).delete(crate::links::api_remove))
        .route("/links/:id/open", post(crate::links::api_open))
        .route("/links/:id/stats", get(crate::remote::api_link_stats))
        .route("/storage/plan", get(storage_plan))
        .route("/drives", get(drives))
        .route("/storage/privacy", get(storage_privacy))
        .route("/storage/missing", get(missing_status))
        .route("/storage/missing/scan", post(missing_scan))
        .route("/storage/missing/apply", post(missing_apply))
        .route("/tls", get(crate::tls::api_tls))
        .route("/addresses", get(crate::tls::api_addresses))
        .route("/logs", get(logs))
        .route("/events", get(events))
        .route("/timeline/events", get(timeline_events))
        .route("/events/:id/thumb", get(event_thumb))
        .route("/events/:id/protect", post(protect_event))
        .route("/events/:id/review", post(review_event))
        .route("/events/:id/ai", get(crate::ai_review::event_ai))
        .route("/events/:id/track", get(crate::ai_review::event_track))
        .route("/signals", get(list_signals).post(set_signal))
        .route("/signals/:name", axum::routing::delete(forget_signal))
        .route("/events/:id/ai/recheck", post(crate::ai_review::event_ai_recheck))
        .route("/events/:id/ai/:n", get(crate::ai_review::event_ai_frame))
        .route("/ai/reviews", get(crate::ai_review::reviews))
        .route("/seek", get(seek_at))
        .route("/clips", get(list_clips))
        .route("/clips/:id", get(clip_file))
        .route("/export", get(export))
        .route("/recordings", get(recordings))
        .route("/recordings/days", get(recording_days))
        .route("/playback", get(playback))
        .route("/playback/picture", get(playback_picture))
        .route("/still", get(still))
        .route("/preview", get(preview))
        .route("/filmstrip", get(filmstrip))
        .route("/timelapse", get(timelapse_list).post(timelapse_make))
        .route("/timelapse/:slug/:date", get(timelapse_file))
        .route("/faces", get(crate::faces_http::list))
        .route("/faces/status", get(crate::faces_http::status))
        .route("/smart/status", get(smart_status))
        .route("/sound/status", get(sound_status))
        .route("/cameras/:id/sound", get(sound_meter))
        .route("/smart/search", get(smart_search))
        .route("/smart/pic/:id", get(smart_pic))
        .route("/smart/follow", get(crate::rules_http::smart_follow))
        .route("/alert-rules", get(crate::rules_http::list).post(crate::rules_http::save))
        .route("/alert-rules/preview", post(crate::rules_http::preview))
        .route("/alert-rules/:id", axum::routing::delete(crate::rules_http::remove))
        .route("/faces/models", post(crate::faces_http::download_models))
        .route("/faces/import", post(crate::faces_http::import))
        .route("/faces/:id/img", get(crate::faces_http::img))
        .route("/faces/:id/name", post(crate::faces_http::name))
        .route("/faces/:id", axum::routing::delete(crate::faces_http::delete))
        .route("/digest/preview", get(crate::digest_http::preview))
        .route("/digest/send", post(crate::digest_http::send_now))
        .route("/digests", get(crate::digest_http::list))
        .route("/digests/:id", get(crate::digest_http::get))
        .route("/plates", get(crate::lpr_http::list))
        .route("/plates/recent", get(crate::lpr_http::recent))
        .route("/plates/status", get(crate::lpr_http::status))
        .route("/plates/models", post(crate::lpr_http::download_models))
        .route("/plates/known", get(crate::lpr_http::known).post(crate::lpr_http::add_known))
        .route("/plates/known/:id", axum::routing::put(crate::lpr_http::update_known).delete(crate::lpr_http::delete_known))
        .route("/plates/:id/img", get(crate::lpr_http::img))
        .route("/plates/:id", axum::routing::delete(crate::lpr_http::delete))
        .route("/people", get(crate::faces_http::people))
        .route("/people/:id", axum::routing::put(crate::faces_http::update_person).delete(crate::faces_http::delete_person))
        .route("/snapshot/:id", get(snapshot))
        .route("/cameras/:id/wake", post(wake_camera))
        .route("/live/:id", get(live))
        .route("/live/:id/info", get(crate::live_http::live_info))
        .route("/live/:id/stream", get(crate::live_http::live_stream))
        .route("/live/:id/ws", get(crate::live_http::live_ws))
        .route("/live/:id/jpeg", get(crate::live_http::live_jpeg_ws))
        .route("/settings", get(get_settings).post(update_settings))
        .route("/offsite", get(crate::offsite::http::get).put(crate::offsite::http::put))
        .route("/offsite/test", post(crate::offsite::http::test))
        .route("/offsite/run", post(crate::offsite::http::run))
        .route("/update", get(crate::update::http::get))
        .route("/update/check", post(crate::update::http::check))
        .route("/update/install", post(crate::update::http::install))
        .route("/system/restart", post(system_restart))
        .route("/overlay", get(list_overlay_vars))
        .route("/overlay/:name", post(set_overlay_var_http))
        .layer(axum::middleware::from_fn_with_state(
            engine.clone(),
            auth_mw,
        ));

    Router::new()
        .route("/", get(static_files::index))
        .route("/ambs.htm", get(static_files::index))
        .route("/index.html", get(static_files::index))
        .route("/favicon.ico", get(static_files::favicon))
        // The notification helper (service worker) must sit at the top to cover every page.
        .route("/sw.js", get(static_files::service_worker))
        .route("/static/*path", get(static_files::asset))
        .route("/tls/ca.crt", get(crate::tls::ca_crt))
        .route("/tls/ca.pem", get(crate::tls::ca_pem))
        // Pushed streams (ffmpeg -f flv https://.../push/<key>): the key is the password.
        .route("/push/:key", axum::routing::put(push_stream).post(push_stream))
        // Capture helpers: pairing (one-time code), control socket (token), downloads (code).
        .route("/helper/register", post(helper_register))
        .route("/helper/ws", get(helper_ws))
        .route("/helper/download/:file", get(helper_download))
        // Linked servers (server switcher): code to link, then per-link keys.
        .route("/link/accept", post(crate::links::accept))
        .route("/link/ping", get(crate::links::ping))
        .route("/link/unlink", post(crate::links::unlink))
        .route("/link/signin", post(crate::links::signin))
        // Cameras shared with a linked server (live view only).
        .route("/link/cameras", get(crate::remote::share_cameras))
        .route("/link/live/:id/info", get(crate::remote::share_live_info))
        .route("/link/live/:id/ws", get(crate::remote::share_live_ws))
        .route("/link/live/:id/jpeg", get(crate::remote::share_live_jpeg))
        .route("/link/snapshot/:id", get(crate::remote::share_snapshot))
        .route("/link/stats", get(crate::remote::share_stats))
        .nest("/api", api)
        .layer(axum::middleware::map_response(security_headers))
        .layer(axum::middleware::from_fn_with_state(engine.clone(), real_client))
        .with_state(engine)
}

/// Behind a trusted reverse proxy ([server] trusted_proxies), every request
/// arrives from the proxy: put the visitor's own address (the last entry of
/// X-Forwarded-For, the one the proxy added) in its place, so everything that
/// reads the caller's address sees the visitor.
async fn real_client(State(engine): State<Arc<Engine>>, mut req: Request, next: Next) -> Response {
    if let Some(peer) = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0) {
        let trusted = { engine.config.lock().unwrap().server.trusted_proxies.clone() };
        if let Some(ip) = forwarded_client(peer.ip(), &trusted, req.headers()) {
            req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(ip, peer.port())));
            // The proxy took the visitor's HTTPS: cookies then get the Secure flag.
            if forwarded_https(req.headers()) {
                req.extensions_mut().insert(crate::tls::Secure);
            }
        }
    }
    next.run(req).await
}

/// The visitor's address when `peer` is a trusted proxy that named one.
fn forwarded_client(peer: std::net::IpAddr, trusted: &[String], headers: &axum::http::HeaderMap) -> Option<std::net::IpAddr> {
    if trusted.is_empty() || !trusted.iter().any(|t| ip_in(peer, t)) {
        return None;
    }
    let last = headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).last()?;
    Some(parse_forwarded_ip(last.trim())?.to_canonical())
}

/// An address as proxies write it: plain, or with a port ("1.2.3.4:5678",
/// "[2001:db8::1]:443") or brackets ("[2001:db8::1]").
fn parse_forwarded_ip(s: &str) -> Option<std::net::IpAddr> {
    if let Ok(ip) = s.parse() {
        return Some(ip);
    }
    if let Some(rest) = s.strip_prefix('[') {
        return rest.split(']').next()?.parse().ok();
    }
    // a.b.c.d:port (an IPv6 address without brackets has several colons and parsed above)
    let (host, port) = s.rsplit_once(':')?;
    port.parse::<u16>().ok()?;
    host.parse::<std::net::Ipv4Addr>().ok().map(std::net::IpAddr::V4)
}

/// The trusted proxy says the visitor came over HTTPS (last X-Forwarded-Proto).
fn forwarded_https(headers: &axum::http::HeaderMap) -> bool {
    headers.get_all("x-forwarded-proto").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).last().is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
}

/// Problems with [server] trusted_proxies worth saying at start: an entry
/// that is not an address or range (it never matches), or a range so wide it
/// would let ordinary visitors claim any address.
pub fn trusted_proxy_warnings(list: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for spec in list {
        let s = spec.trim();
        let (addr, bits) = match s.split_once('/') {
            Some((a, b)) => (a, Some(b)),
            None => (s, None),
        };
        let Ok(ip) = addr.parse::<std::net::IpAddr>() else {
            out.push(format!("trusted_proxies: \"{s}\" is not an address or range; it is ignored"));
            continue;
        };
        let wide = match (ip.to_canonical(), bits.map(|b| b.parse::<u32>())) {
            (_, Some(Err(_))) => {
                out.push(format!("trusted_proxies: \"{s}\" has a bad range size; it is read as the single address {addr}"));
                continue;
            }
            (std::net::IpAddr::V4(_), Some(Ok(n))) => n < 16,
            (std::net::IpAddr::V6(_), Some(Ok(n))) => n < 48,
            _ => false,
        };
        if wide {
            out.push(format!("trusted_proxies: \"{s}\" is a very wide range: anyone in it can claim any address (sign-in limits, logs, alerts); list only your proxy's own address"));
        }
    }
    out
}

/// `ip` is the address `spec` names, or inside its range ("a.b.c.d/n").
fn ip_in(ip: std::net::IpAddr, spec: &str) -> bool {
    let (addr, bits) = match spec.trim().split_once('/') {
        Some((a, b)) => (a, b.parse::<u32>().ok()),
        None => (spec.trim(), None),
    };
    let Ok(net) = addr.parse::<std::net::IpAddr>() else { return false };
    match (ip.to_canonical(), net.to_canonical()) {
        (std::net::IpAddr::V4(a), std::net::IpAddr::V4(n)) => {
            let b = bits.unwrap_or(32).min(32);
            let mask = if b == 0 { 0 } else { u32::MAX << (32 - b) };
            u32::from(a) & mask == u32::from(n) & mask
        }
        (std::net::IpAddr::V6(a), std::net::IpAddr::V6(n)) => {
            let b = bits.unwrap_or(128).min(128);
            let mask = if b == 0 { 0 } else { u128::MAX << (128 - b) };
            u128::from(a) & mask == u128::from(n) & mask
        }
        _ => false,
    }
}

/// `PUT|POST /push/<key>`: an FLV stream sent to a push:// camera, held open
/// for as long as the sender keeps sending.
async fn push_stream(State(engine): State<Arc<Engine>>, AxPath(key): AxPath<String>, req: Request) -> Response {
    use http_body_util::BodyExt;
    let ip = client_ip(&req);
    if crate::auth::is_locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many wrong stream keys; try again in a few minutes").into_response();
    }
    let Some(name) = engine.push_camera(&key) else {
        crate::auth::record_failure(ip);
        return (StatusCode::NOT_FOUND, "unknown stream key").into_response();
    };
    tracing::info!("camera '{name}': receiving a pushed stream from {ip} (HTTPS)");
    let mut p = crate::ingest::push::publish(&key, &format!("{ip} (HTTPS)"));
    let mut body = req.into_body();
    let mut bytes = 0u64;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { break };
        if let Ok(data) = frame.into_data() {
            bytes += data.len() as u64;
            if p.write(&data).await.is_err() {
                break; // the camera stopped reading, or another sender took over
            }
        }
    }
    tracing::info!("camera '{name}': pushed stream from {ip} ended after {} MB", bytes / 1_000_000);
    (StatusCode::OK, "stream ended").into_response()
}

// ---------------------------------------------------------------- built-in AI model

/// `GET /api/ai/model`: the available models, the loaded one, and any download in progress.
async fn ai_model_status(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let model = engine.config.lock().unwrap().ai.model.clone();
    let loaded = engine.detector().is_some();
    let current = model.as_deref().and_then(crate::ai_setup::identify).map(|m| m.id);
    let job = crate::ai_setup::job().lock().unwrap().clone();
    let dir = engine.data_dir.join("models");
    Json(serde_json::json!({
        "models": crate::ai_setup::MODELS
            .iter()
            .map(|m| serde_json::json!({ "id": m.id, "name": m.name, "about": m.about, "downloaded": dir.join(m.file).is_file() }))
            .collect::<Vec<_>>(),
        "current": current, "loaded": loaded, "job": job,
    }))
}

#[derive(serde::Deserialize)]
struct AiModelReq {
    model: String,
}

/// `POST /api/ai/model {"model": "tiny"|"s"|"l"}` (admin): download (if needed),
/// load and switch AI on. Progress via GET.
async fn ai_model_get(State(engine): State<Arc<Engine>>, Json(r): Json<AiModelReq>) -> Response {
    let Some(m) = crate::ai_setup::find(&r.model) else { return api_error(StatusCode::BAD_REQUEST, "unknown model") };
    {
        let mut j = crate::ai_setup::job().lock().unwrap();
        if matches!(j.state, "downloading" | "loading") {
            return api_error(StatusCode::CONFLICT, "a model is already being set up");
        }
        *j = crate::ai_setup::Job { state: "downloading", model: m.id.into(), ..Default::default() };
    }
    let dir = engine.data_dir.join("models");
    std::thread::spawn(move || {
        let set = |f: &dyn Fn(&mut crate::ai_setup::Job)| f(&mut crate::ai_setup::job().lock().unwrap());
        let r = crate::ai_setup::ensure(&dir, m, &|b, t| set(&|j| { j.bytes = b; j.total = t; })).and_then(|path| {
            set(&|j| j.state = "loading");
            // Load first; only a model that works goes into the settings.
            let (threads, gpu, gpu_name, gpu_mb) = {
                let c = engine.config.lock().unwrap();
                (c.ai.threads, c.ai.gpu, c.ai.gpu_name.clone(), c.ai.gpu_memory_mb)
            };
            let d = crate::ai::AiDetector::load_on(&path, threads, gpu, gpu_name.as_deref(), gpu_mb)?;
            *engine.ai.write().unwrap() = Some(std::sync::Arc::new(d));
            let mut c = engine.config.lock().unwrap();
            c.ai.enabled = true;
            c.ai.model = Some(std::path::PathBuf::from(crate::ai_setup::clean_path(&path)));
            c.save(&engine.config_path)
        });
        match r {
            Ok(()) => {
                tracing::info!("AI: {} model ready and in use", m.name);
                set(&|j| j.state = "ready");
            }
            Err(e) => {
                tracing::warn!("AI: model setup failed: {e:#}");
                let msg = format!("{e:#}");
                set(&|j| { j.state = "error"; j.error = Some(msg.clone()); });
            }
        }
    });
    Json(serde_json::json!({ "started": true })).into_response()
}

/// `POST /api/alerts/system_test` (admin): send a test system alert to every
/// notification service that is set up; says which went out.
async fn system_alert_test(State(engine): State<Arc<Engine>>) -> Response {
    let alerts = engine.config.lock().unwrap().alerts.clone();
    let site = crate::links::own_name(&engine);
    let res = tokio::task::spawn_blocking(move || {
        crate::engine::system_alerts::send(&alerts, &format!("{site}: test"), "This is a test of AmberSight's system alerts (camera offline, recordings not written, disk nearly full).", true)
    })
    .await
    .unwrap_or_default();
    let mqtt = engine.mqtt.get().is_some();
    Json(serde_json::json!({
        "sent": res.iter().filter(|r| r.1).map(|r| r.0).collect::<Vec<_>>(),
        "failed": res.iter().filter(|r| !r.1).map(|r| r.0).collect::<Vec<_>>(),
        "mqtt": mqtt,
    }))
    .into_response()
}

// ---------------------------------------------------------------- capture helpers

/// `POST /api/helpers/pair` (admin): a one-time code for a new computer.
async fn helpers_pair(req: Request) -> Response {
    let origin = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(|h| format!("https://{h}")).unwrap_or_default();
    let code = crate::helpers::new_pair_code();
    Json(serde_json::json!({ "code": code, "expires_in": crate::helpers::PAIR_SECS, "server": origin })).into_response()
}

async fn helpers_list(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(crate::helpers::list(&engine))
}

#[derive(serde::Deserialize)]
struct RenameReq {
    name: String,
}

async fn helpers_rename(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<String>, Json(r): Json<RenameReq>) -> Response {
    let name: String = r.name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    if name.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "enter a name");
    }
    let mut cfg = engine.config.lock().unwrap();
    let Some(h) = cfg.helpers.iter_mut().find(|h| h.id == id) else { return api_error(StatusCode::NOT_FOUND, "no such computer") };
    h.name = name;
    if let Err(e) = cfg.save(&engine.config_path) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("could not save: {e:#}"));
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn helpers_remove(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<String>) -> Response {
    match crate::helpers::remove(&engine, &id) {
        Ok(true) => Json(serde_json::json!({ "removed": id })).into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "no such computer"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("could not save: {e:#}")),
    }
}

/// `POST /api/helpers/:id/sources`: the screens and windows that computer offers.
async fn helpers_sources(AxPath(id): AxPath<String>) -> Response {
    match crate::helpers::request(&id, serde_json::json!({ "type": "list" }), std::time::Duration::from_secs(10)).await {
        Ok(v) => Json(serde_json::json!({ "monitors": v["monitors"], "windows": v["windows"] })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct ThumbReq {
    source: serde_json::Value,
    #[serde(default)]
    width: Option<u32>,
}

/// `POST /api/helpers/:id/thumb {source, width}` -> a small JPEG of that source.
async fn helpers_thumb(AxPath(id): AxPath<String>, Json(r): Json<ThumbReq>) -> Response {
    let msg = serde_json::json!({ "type": "thumb", "source": r.source, "width": r.width.unwrap_or(320).clamp(80, 640) });
    let v = match crate::helpers::request(&id, msg, std::time::Duration::from_secs(15)).await {
        Ok(v) => v,
        Err(e) => return api_error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    if let Some(err) = v["error"].as_str() {
        return api_error(StatusCode::BAD_GATEWAY, err.to_string());
    }
    let Some(b64) = v["jpeg"].as_str() else { return api_error(StatusCode::BAD_GATEWAY, "no picture") };
    match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
        Ok(jpeg) if jpeg.len() < 4 << 20 => Response::builder()
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(jpeg))
            .unwrap(),
        _ => api_error(StatusCode::BAD_GATEWAY, "bad picture"),
    }
}

#[derive(serde::Deserialize)]
struct RegisterReq {
    code: String,
    #[serde(default)]
    machine: String,
    #[serde(default)]
    os: String,
}

/// `POST /helper/register {code, machine, os}`: a new helper's id and token.
async fn helper_register(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let ip = client_ip(&req);
    if crate::auth::is_locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many wrong codes; try again in a few minutes").into_response();
    }
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 8 * 1024).await else {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    };
    let Ok(r) = serde_json::from_slice::<RegisterReq>(&bytes) else {
        return (StatusCode::BAD_REQUEST, "expected {\"code\", \"machine\", \"os\"}").into_response();
    };
    match crate::helpers::register(&engine, &r.code, &r.machine, &r.os) {
        Ok((id, token, name)) => {
            crate::auth::record_success(ip);
            let _ = engine.db.insert_audit("(capture helper)", &ip.to_string(), "helper paired", &name, 200);
            tracing::info!("capture helper '{name}' paired from {ip}");
            Json(serde_json::json!({ "id": id, "token": token, "name": name })).into_response()
        }
        Err(e) => {
            crate::auth::record_failure(ip);
            (StatusCode::UNAUTHORIZED, format!("{e:#}")).into_response()
        }
    }
}

/// `GET /helper/ws` (Authorization: Bearer <token>, X-Helper-Id: <id>).
async fn helper_ws(State(engine): State<Arc<Engine>>, ws: axum::extract::WebSocketUpgrade, req_headers: axum::http::HeaderMap, axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>) -> Response {
    let ip = peer.ip();
    if crate::auth::is_locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "locked out").into_response();
    }
    let id = req_headers.get("x-helper-id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let token = req_headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").trim().to_string();
    let Some(helper) = crate::helpers::authenticate(&engine, &id, &token) else {
        crate::auth::record_failure(ip);
        return (StatusCode::UNAUTHORIZED, "unknown helper").into_response();
    };
    ws.max_message_size(8 << 20).on_upgrade(move |socket| crate::helpers::run_socket(engine, helper, socket))
}

#[derive(serde::Deserialize)]
struct HelperCodeQ {
    #[serde(default)]
    code: String,
}

/// `GET /helper/download/:file?code=` - the helper program (this server's own
/// executable) or ffmpeg, while a pairing code is valid.
async fn helper_download(AxPath(file): AxPath<String>, Query(q): Query<HelperCodeQ>, req: Request) -> Response {
    let ip = client_ip(&req);
    if crate::auth::is_locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "locked out").into_response();
    }
    if !crate::helpers::code_valid(&q.code) {
        crate::auth::record_failure(ip);
        return (StatusCode::UNAUTHORIZED, "the pairing code has expired; make a new one").into_response();
    }
    let path = match file.as_str() {
        "ambersight.exe" | "ambersight" => std::env::current_exe().ok(),
        "ffmpeg.exe" | "ffmpeg" => crate::ffmpeg_bin::existing(),
        _ => None,
    };
    let Some(path) = path.filter(|p| p.is_file()) else { return (StatusCode::NOT_FOUND, "not available on this server").into_response() };
    let Ok(f) = tokio::fs::File::open(&path).await else { return (StatusCode::NOT_FOUND, "not available").into_response() };
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len)
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file}\""))
        .body(Body::from_stream(tokio_util::io::ReaderStream::new(f)))
        .unwrap()
}

fn accounts(engine: &Engine) -> Vec<(String, String, bool)> {
    crate::auth::accounts(&engine.config.lock().unwrap())
}

pub(crate) fn client_ip(req: &Request) -> std::net::IpAddr {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
}

/// A caller the server knows.
struct Authed {
    id: crate::auth::Identity,
    /// Came with a device token: it cannot do everything (see `token_may_not`).
    by_token: bool,
    /// The account's generation, when it was read along with the check.
    generation: Option<u64>,
    /// It came with a password in HTTP Basic (not a session, not a token).
    basic: bool,
}

/// Why a request names nobody.
#[derive(PartialEq)]
enum NotAuthed {
    /// It offered nothing.
    Nothing,
    /// It offered a password or token that is wrong.
    Wrong,
    /// The right password, for an account that needs a code with it.
    TwoStep,
    /// A password that was not looked at.
    NotChecked(crate::auth::NotChecked),
}

/// Who is calling: session cookie first, then the Authorization header (a
/// device token, or a password in HTTP Basic: checked in line, see auth.rs).
async fn identify(accounts: &[(String, String, bool)], h: &axum::http::HeaderMap, ip: std::net::IpAddr) -> Result<Authed, NotAuthed> {
    if let Some((_, id)) = crate::auth::session_from_headers(h, accounts) {
        return Ok(Authed { id, by_token: false, generation: None, basic: false });
    }
    let Some(header) = h.get(header::AUTHORIZATION) else { return Err(NotAuthed::Nothing) };
    let Ok(header) = header.to_str() else { return Err(NotAuthed::Nothing) };
    match crate::auth::check_authorization_queued(header, accounts, ip).await {
        Ok(crate::auth::Cred::Password(id, generation)) => Ok(Authed { id, by_token: false, generation: Some(generation), basic: true }),
        Ok(crate::auth::Cred::Token(id, generation)) => Ok(Authed { id, by_token: true, generation: Some(generation), basic: false }),
        Ok(crate::auth::Cred::TwoStepNeeded) => Err(NotAuthed::TwoStep),
        Ok(crate::auth::Cred::Wrong) => Err(NotAuthed::Wrong),
        Err(why) => Err(NotAuthed::NotChecked(why)),
    }
}

/// The answer when a password was not checked: it does not count as a wrong one.
fn not_checked(why: crate::auth::NotChecked) -> Response {
    match why {
        crate::auth::NotChecked::Busy => (StatusCode::SERVICE_UNAVAILABLE, crate::auth::PASSWORD_BUSY).into_response(),
        crate::auth::NotChecked::Locked => (StatusCode::TOO_MANY_REQUESTS, "too many failed sign-ins; try again in a few minutes").into_response(),
    }
}

/// A request's `Authorization` header holds a live device token: no lockout
/// for it (it is no guess; 256 random bits cannot be guessed either way).
fn has_valid_token(accounts: &[(String, String, bool)], req: &Request) -> bool {
    req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).is_some_and(|h| crate::auth::token_identity(h, accounts).is_some())
}

/// What device tokens may do: nothing but this. A new route is refused until
/// it is added here on purpose, and a test fails until it is classified.
/// (route, method, needs control rights: a token made with the "admin" scope.)
/// Whoever holds a copy of a token must not be able to run programs through
/// the settings, send stored passwords somewhere else, or make it permanent.
const TOKEN_ROUTES: &[(&str, &str, bool)] = &[
    // Watching.
    ("/cameras", "GET", false),
    ("/status", "GET", false),
    ("/live/:id", "GET", false),
    ("/live/:id/info", "GET", false),
    ("/live/:id/stream", "GET", false),
    ("/live/:id/ws", "GET", false),
    ("/live/:id/jpeg", "GET", false),
    ("/snapshot/:id", "GET", false),
    ("/events", "GET", false),
    ("/events/:id/thumb", "GET", false),
    ("/timeline/events", "GET", false),
    ("/clips", "GET", false),
    ("/clips/:id", "GET", false),
    ("/recordings", "GET", false),
    ("/recordings/days", "GET", false),
    ("/playback", "GET", false),
    ("/playback/picture", "GET", false),
    ("/seek", "GET", false),
    ("/still", "GET", false),
    ("/preview", "GET", false),
    ("/filmstrip", "GET", false),
    ("/cameras/:id/wake", "POST", false),
    ("/signals", "POST", false),
    // Day-to-day control.
    ("/shield", "POST", true),
    ("/profiles", "GET", true),
    ("/profile", "POST", true),
    ("/overlay", "GET", false),
    ("/overlay/:name", "POST", true),
    ("/cameras/:id/ptz", "POST", true),
    ("/cameras/:id/ptz/aim", "POST", true),
    ("/cameras/:id/ptz/presets", "GET", true),
    ("/cameras/:id/ptz/caps", "GET", true),
    ("/cameras/:id/pause", "POST", true),
    ("/clips/record", "POST", true),
    ("/events/:id/review", "POST", true),
    ("/events/:id/protect", "POST", true),
];

fn route_matches(route: &str, path: &str) -> bool {
    let (mut r, mut p) = (route.split('/'), path.split('/'));
    loop {
        match (r.next(), p.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) => {
                if !(a == b || (a.starts_with(':') && !b.is_empty())) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// Whether a device token may make this request. `control`: it has control
/// rights (its scope, and an account that is still an admin). Default: no.
fn token_may(path: &str, method: &axum::http::Method, control: bool) -> bool {
    let m = if method == axum::http::Method::HEAD { "GET" } else { method.as_str() };
    TOKEN_ROUTES.iter().any(|(route, want, needs_control)| *want == m && (control || !needs_control) && route_matches(route, path))
}

/// The signed-in user's name and the account's generation at that moment,
/// attached to every API request.
#[derive(Clone)]
pub struct Caller(pub String, pub u64);

/// Whose video this is, to end it when the account changes: (engine, user,
/// the account's generation when the request was let in). None: a linked
/// server, which has its own check.
pub type Who = Option<(Arc<Engine>, String, u64)>;

/// The request came with a device token, of this name.
#[derive(Clone)]
pub struct TokenUsed(pub String);

/// How a change came in, for the log of arming and profiles.
fn source_of(token: &Option<axum::Extension<TokenUsed>>) -> String {
    match token {
        Some(t) => format!("device token \"{}\"", t.0 .0),
        None => "web page or API".to_string(),
    }
}

/// The longest a device token may hold a profile or pause notifications: never until changed by hand.
const TOKEN_HOLD_MINUTES: u32 = 24 * 60;
/// The longest manual clip.
const MAX_RECORD_SECONDS: i64 = 3600;
/// What a device token does in a row within this long is told in one message at
/// its end: all of it, also after the owner has undone the first.
const TOKEN_NOTE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

fn record_seconds(asked: i64) -> i64 {
    asked.clamp(5, MAX_RECORD_SECONDS)
}

/// Minutes of a pause asked for with a token: never until resumed.
fn token_pause(asked: Option<u32>) -> Option<u32> {
    Some(asked.unwrap_or(TOKEN_HOLD_MINUTES).min(TOKEN_HOLD_MINUTES))
}

/// Whether the caller has admin rights (open-LAN mode: everyone).
#[derive(Clone, Copy)]
pub struct IsAdmin(pub bool);

/// Which cameras the caller may see (None = all), attached to every API
/// request by the auth middleware.
#[derive(Clone, Default)]
pub struct Access(pub Option<std::collections::HashSet<String>>);

impl Access {
    pub fn can_slug(&self, slug: &str) -> bool {
        self.0.as_ref().is_none_or(|s| s.contains(slug))
    }
    pub fn can_name(&self, name: &str) -> bool {
        self.can_slug(&crate::config::slug(name))
    }
    pub fn limited(&self) -> bool {
        self.0.is_some()
    }
}

/// Every value of `key` in the query, decoded exactly as the handlers'
/// `Query<..>` decodes it (keys too: `%63amera` is `camera`).
fn query_values(req: &Request, key: &str) -> Vec<String> {
    let Some(q) = req.uri().query() else { return Vec::new() };
    url::form_urlencoded::parse(q.as_bytes()).filter(|(k, _)| k == key).map(|(_, v)| v.into_owned()).collect()
}

/// Single-camera requests for a camera the caller may not see: answer as if
/// it does not exist.
/// The camera part of an event / clip id: "<camera slug>-<start>", or for
/// package events "<camera slug>-<time>-pkg" / "-pkgoff".
fn event_camera_slug(id: &str) -> Option<&str> {
    let (rest, tail) = id.rsplit_once('-')?;
    let base = if tail.is_empty() || tail.chars().all(|c| c.is_ascii_digit()) { id } else { rest };
    base.rsplit_once('-').map(|(slug, _)| slug)
}

fn camera_allowed(engine: &Engine, access: &Access, req: &Request) -> bool {
    if !access.limited() {
        return true;
    }
    // Decoded as the handlers will see them (else "%2D" would hide a "-").
    let decoded: Vec<String> = req.uri().path().trim_start_matches('/').split('/').map(|p| percent_encoding::percent_decode_str(p).decode_utf8_lossy().into_owned()).collect();
    let parts: Vec<&str> = decoded.iter().map(String::as_str).collect();
    let by_id = |id: &str| -> bool {
        id.parse::<usize>()
            .ok()
            .and_then(|i| engine.camera(i))
            .is_some_and(|h| access.can_name(&h.cfg.read().unwrap().name))
    };
    let by_event = |id: &str| -> bool { event_camera_slug(id).is_some_and(|slug| access.can_slug(slug)) };
    match parts.as_slice() {
        ["snapshot", id] | ["live", id] | ["live", id, ..] | ["cameras", id, ..] => by_id(id),
        ["events", id, ..] => by_event(id),
        ["clips", id] if *id != "record" => by_event(id),
        ["timelapse", slug, ..] => access.can_slug(slug),
        // Every camera named in the query must be one the caller may see.
        // (An empty or missing camera means "all"; the handlers then filter
        // or refuse for limited users.)
        _ => query_values(req, "camera").iter().filter(|c| !c.is_empty()).all(|c| access.can_name(c)),
    }
}

/// Headers on every answer: no type sniffing, no framing by other sites, no
/// page addresses leaking to other sites.
async fn security_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.entry(header::X_CONTENT_TYPE_OPTIONS).or_insert(HeaderValue::from_static("nosniff"));
    h.entry(header::X_FRAME_OPTIONS).or_insert(HeaderValue::from_static("SAMEORIGIN"));
    h.entry(header::REFERRER_POLICY).or_insert(HeaderValue::from_static("same-origin"));
    // Which web interface this server has (see static_files::ui_build).
    if let Ok(v) = HeaderValue::from_str(static_files::ui_build()) {
        h.insert("x-ambersight-ui", v);
    }
    // Only this server's own scripts run in its pages (a name or label that
    // somehow carried script would do nothing). Video comes in as blob:
    // addresses (live view, playback), pictures as blob: or data:.
    h.entry(header::CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; \
         connect-src 'self'; worker-src 'self' blob:; frame-ancestors 'self'; object-src 'none'; base-uri 'self'; form-action 'self'",
    ));
    r
}

/// The host the browser addressed: the Host header (HTTP/1.1) or the
/// request's authority (HTTP/2 sends `:authority` instead of Host).
fn request_host(req: &Request) -> Option<String> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()))
}

/// No Origin header (not a browser, or a same-origin GET), or one whose
/// host:port matches the host the request was sent to.
fn same_origin(h: &axum::http::HeaderMap, host: Option<&str>) -> bool {
    let Some(origin) = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else { return true };
    let Some(host) = host else { return false };
    let o = origin.split_once("://").map(|(_, r)| r).unwrap_or(origin).trim_end_matches('/');
    o.eq_ignore_ascii_case(host)
}

/// The Host header names this server: an IP address, localhost, this
/// computer's name, or one of the configured certificate names.
fn host_is_ours(engine: &Engine, host: Option<&str>) -> bool {
    let Some(host) = host else { return true };
    let name = match host.rsplit_once(':') {
        Some((n, p)) if p.chars().all(|c| c.is_ascii_digit()) && !n.ends_with(':') => n,
        _ => host,
    };
    let name = name.trim_matches(|c| c == '[' || c == ']').trim_end_matches('.').to_ascii_lowercase();
    if name.parse::<std::net::IpAddr>().is_ok() || name == "localhost" {
        return true;
    }
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let me = HOST.get_or_init(|| crate::capture::proto::machine_name().to_ascii_lowercase());
    if name == *me || name == format!("{me}.local") {
        return true;
    }
    engine.config.lock().unwrap().server.tls_names.iter().any(|n| n.eq_ignore_ascii_case(&name))
}

/// Changes a signed-in viewer (not admin) may make: their own password and
/// settings, their own devices' notifications and presence, waking a camera
/// they can see; other changes need admin.
fn viewer_may_write(path: &str, method: &axum::http::Method) -> bool {
    use axum::http::Method;
    path == "/me/password"
        // Their own two-step codes and device tokens.
        || matches!(path, "/me/two-step/start" | "/me/two-step/confirm" | "/me/two-step/cancel" | "/me/two-step/off" | "/me/two-step/recovery" | "/me/tokens")
        || (path.starts_with("/me/tokens/") && method == Method::DELETE)
        // Their own signed-in devices.
        || (method == Method::POST && (path == "/me/sessions/sign-out-others" || (path.starts_with("/me/sessions/") && path.ends_with("/sign-out"))))
        || path == "/ui/prefs"
        // Page timings and the sound tripwire from their own devices.
        || path == "/ui/diag"
        // Notifications on their own devices.
        || matches!(path, "/push/subscribe" | "/push/unsubscribe" | "/push/test")
        // Their own phones at home or away (and removing them).
        || (path == "/presence" && method == Method::POST)
        || (path.starts_with("/presence/") && path.len() > "/presence/".len() && method == Method::DELETE)
        // (So a home-automation account need not be an admin.)
        || (path == "/signals" && method == Method::POST)
        || (path.starts_with("/cameras/") && path.ends_with("/wake"))
        || matches!(path.trim_start_matches('/').split('/').collect::<Vec<_>>().as_slice(), ["links", id, "open"] if !id.is_empty())
}

/// The changes an operator may make, and the right each needs: nothing but
/// these (beyond what any viewer may do). `gate` keeps arming and profiles
/// (all cameras) from operators limited to some; the handlers check more: the
/// camera (limited accounts), what a camera edit touches
/// (`operator_update_camera`), no saving or deleting presets.
fn operator_route(path: &str, method: &axum::http::Method) -> Option<Perm> {
    let parts: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    let cam = |id: &str| id.parse::<usize>().is_ok();
    match (method.as_str(), parts.as_slice()) {
        ("POST", ["cameras", id, "ptz"] | ["cameras", id, "ptz", "aim"]) if cam(id) => Some(Perm::Ptz),
        ("POST", ["clips", "record"]) => Some(Perm::TalkClip),
        ("POST", ["shield"] | ["profile"]) => Some(Perm::Arm),
        ("POST", ["cameras", id, "pause"]) if cam(id) => Some(Perm::Arm),
        ("POST", ["events", id, "protect" | "review"]) if !id.is_empty() => Some(Perm::Events),
        ("PUT", ["cameras", id]) if cam(id) => Some(Perm::CameraDetection),
        _ => None,
    }
}

/// Whether the sign-in layer lets this caller's request through (handlers
/// may refuse more). Default: admins only.
fn gate(path: &str, method: &axum::http::Method, role: &Role, limited: bool) -> Result<(), &'static str> {
    if role.is_admin() {
        return Ok(());
    }
    let is_write = method != axum::http::Method::GET && method != axum::http::Method::HEAD;
    if is_write && !viewer_may_write(path, method) {
        match operator_route(path, method) {
            // Arming and profiles change every camera.
            Some(Perm::Arm) if limited && matches!(path, "/shield" | "/profile") => {
                return Err("an account limited to some cameras cannot arm, disarm or switch profiles: those change every camera");
            }
            Some(p) if role.may(p) => {}
            Some(_) if role.is_operator() => return Err("this account may not do that: an admin can allow it in Settings, Users"),
            _ => return Err("admin access required for changes"),
        }
    }
    if admin_only_path(path) {
        return Err("admin access required");
    }
    Ok(())
}

/// API paths only admins may read too (settings, logs, health, accounts,
/// network scans, restarting the server ...).
fn admin_only_path(path: &str) -> bool {
    matches!(path, "/settings" | "/logs" | "/health" | "/users" | "/addresses" | "/discover" | "/devices" | "/discover/logins" | "/discover/watch" | "/discover/ignore" | "/helpers" | "/connections" | "/storage/plan" | "/storage/privacy" | "/drives" | "/ai/reviews" | "/sound/status" | "/restream" | "/plates/status" | "/plates/known")
        || path.starts_with("/users/")
        || path.starts_with("/storage/missing")
        || path.starts_with("/alert-rules")
        || path.starts_with("/faces")
        || path.starts_with("/people")
        || path.starts_with("/offsite")
        || path.starts_with("/event-copy")
        || path.starts_with("/digest/")
        || path == "/noisy"
        || path.starts_with("/update")
        || path.starts_with("/system")
        // A camera's tamper, unusual-activity and ONVIF settings windows (the
        // pictures are open to anyone who may see the camera).
        || matches!(path.trim_start_matches('/').split('/').collect::<Vec<_>>().as_slice(), ["cameras", _, "tamper" | "unusual" | "onvif" | "spots"])
}

async fn auth_mw(State(engine): State<Arc<Engine>>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    // Sign-in endpoints answer for themselves (but not to other web sites'
    // pages: no signing someone in as another account, no guessing
    // passwords from another site).
    if matches!(path.as_str(), "/login" | "/login/totp" | "/logout" | "/whoami" | "/pair") {
        if req.method() != axum::http::Method::GET && !same_origin(req.headers(), request_host(&req).as_deref()) {
            return (StatusCode::FORBIDDEN, "request from another web site refused").into_response();
        }
        return next.run(req).await;
    }
    let acc = accounts(&engine);
    let ip = client_ip(&req);
    let method = req.method().to_string();
    let is_write = method != "GET" && method != "HEAD";
    // Another web site's page (in a browser on the network) must not be able
    // to change settings or open live video here.
    let upgrade = req.headers().get(header::UPGRADE).is_some() || req.method() == axum::http::Method::CONNECT;
    if (is_write || upgrade) && !same_origin(req.headers(), request_host(&req).as_deref()) {
        return (StatusCode::FORBIDDEN, "request from another web site refused").into_response();
    }
    // Without accounts, a name that is not this server's (DNS rebinding:
    // evil.example resolving to this address) is refused too.
    if acc.is_empty() && !host_is_ours(&engine, request_host(&req).as_deref()) {
        return (StatusCode::FORBIDDEN, "unknown host name; open AmberSight by its address or its own name").into_response();
    }
    // No users configured: open-LAN mode allows everything.
    let mut access = Access::default();
    let mut is_admin = true;
    let mut role = Role::Admin;
    let mut generation_read = None;
    let mut with_token = false;
    let mut token_used: Option<String> = None;
    let user = if acc.is_empty() {
        "(open)".to_string()
    } else {
        // A lockout stops password attempts from this address; it never
        // blocks someone already signed in (a valid session cookie or token).
        let session = crate::auth::session_from_headers(req.headers(), &acc).map(|(t, _)| t.to_string());
        let has_session = session.is_some();
        if !has_session && req.headers().contains_key(header::AUTHORIZATION) && crate::auth::is_locked(ip) && !has_valid_token(&acc, &req) {
            return (StatusCode::TOO_MANY_REQUESTS, "too many failed sign-ins; try again in a few minutes").into_response();
        }
        match identify(&acc, req.headers(), ip).await {
            Ok(Authed { id, by_token, generation, basic }) => {
                // An operator's device token has the operator's rights only
                // when it was made with control rights.
                let control = !by_token || req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).is_some_and(crate::auth::token_control);
                let (r, limits) = {
                    let cfg = engine.config.lock().unwrap();
                    (crate::auth::role_of(&cfg, &id.user, id.admin, control), crate::auth::camera_limits(&cfg, &id.user))
                };
                if by_token && !token_may(&path, req.method(), r != Role::Viewer) {
                    return (StatusCode::FORBIDDEN, "sign in with a password for this").into_response();
                }
                // Where and when a session was last used (in memory; the database
                // gets it every few minutes at most), and a password used directly
                // from a network not seen before (on a thread of its own).
                if let Some(t) = &session {
                    crate::auth::session_seen(t, ip);
                } else if basic {
                    crate::devices::note_direct(&engine, &id.user, ip);
                }
                generation_read = generation;
                with_token = by_token;
                if by_token {
                    token_used = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(crate::auth::token_name);
                }
                // Changes: admins, and what viewers and operators may do.
                // Settings, logs, health, accounts and network scans: admins only.
                if let Err(why) = gate(&path, req.method(), &r, limits.is_some()) {
                    return (StatusCode::FORBIDDEN, why).into_response();
                }
                access = Access(limits);
                is_admin = id.admin;
                role = r;
                id.user
            }
            Err(NotAuthed::NotChecked(why)) => return not_checked(why),
            Err(why) => {
                if why == NotAuthed::TwoStep {
                    return (StatusCode::UNAUTHORIZED, crate::auth::USE_A_TOKEN).into_response();
                }
                if why == NotAuthed::Wrong {
                    crate::auth::record_failure(ip);
                    let _ = engine.db.insert_audit("?", &ip.to_string(), "sign-in failed (basic)", &path, 401);
                }
                return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
            }
        }
    };
    if !camera_allowed(&engine, &access, &req) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let ua = req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    crate::conns::seen(&user, ip, ua, &path);
    req.extensions_mut().insert(access);
    let generation = generation_read.unwrap_or_else(|| crate::auth::generation(&user));
    req.extensions_mut().insert(Caller(user.clone(), generation));
    // (Handlers use this to leave out camera logins, folder paths and such. A
    // token is let through by the list above, and never sees those.)
    req.extensions_mut().insert(IsAdmin(is_admin && !with_token));
    // (What the caller may do; a device token's role is capped by its scope.)
    req.extensions_mut().insert(role);
    if let Some(n) = &token_used {
        req.extensions_mut().insert(TokenUsed(n.clone()));
    }
    // A change sent slowly: signed in when it began, but the account may have
    // been disabled by the time its body has arrived. Handlers read the body
    // to its end before they act, and this makes that fail in that case.
    let req = if is_write { recheck_at_body_end(&engine, &user, generation, req) } else { req };
    let resp = next.run(req).await;
    // (Page timing reports are diagnostics, not changes, and phones'
    // presence reports come every few hours and record their own real
    // changes: neither is audited here.)
    if is_write && path != "/ui/diag" && !(path == "/presence" && method == "POST") {
        let action = match &token_used {
            Some(n) => format!("{method} (device token \"{n}\")"),
            None => method.clone(),
        };
        let _ = engine.db.insert_audit(&user, &ip.to_string(), &action, &path, resp.status().as_u16());
    }
    end_stream_with_account(&engine, &user, generation, resp)
}

/// See auth_mw: the body ends in an error if the account changed meanwhile.
fn recheck_at_body_end(engine: &Arc<Engine>, user: &str, generation: u64, req: Request) -> Request {
    use futures::StreamExt;
    let (parts, body) = req.into_parts();
    let (engine, user) = (engine.clone(), user.to_string());
    let check = futures::stream::once(async move {
        if crate::auth::still_allowed(&engine.config.lock().unwrap(), &user, generation) {
            Ok(axum::body::Bytes::new())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the account changed"))
        }
    });
    let data = body.into_data_stream().map(|r| r.map_err(std::io::Error::other));
    Request::from_parts(parts, Body::from_stream(data.chain(check)))
}

/// How often running video checks that its viewer's account is still allowed.
const STILL_ALLOWED_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// Resolves when `user`'s account has been disabled or deleted.
async fn account_ended(engine: Arc<Engine>, user: String, generation: u64) {
    loop {
        tokio::time::sleep(STILL_ALLOWED_EVERY).await;
        if !crate::auth::still_allowed(&engine.config.lock().unwrap(), &user, generation) {
            return;
        }
    }
}

/// Video going out over plain HTTP (live streams and recordings can run for a
/// long time) stops when its viewer's account is disabled or deleted.
fn end_stream_with_account(engine: &Arc<Engine>, user: &str, generation: u64, resp: Response) -> Response {
    let long = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| {
        t.starts_with("video/") || t.starts_with("audio/") || t.starts_with("multipart/") || t.starts_with("application/zip") || t.starts_with("application/octet-stream")
    });
    if !long {
        return resp;
    }
    use futures::StreamExt;
    let (parts, body) = resp.into_parts();
    let ended = account_ended(engine.clone(), user.to_string(), generation);
    Response::from_parts(parts, Body::from_stream(body.into_data_stream().take_until(ended)))
}

/// Runs a WebSocket session for `who` (the signed-in caller; None: a linked
/// server, which has its own check) until it finishes or the account is
/// disabled, deleted or changed since the request was let in; dropping the
/// session closes the connection.
/// Completes when this account may no longer watch (never for None: a
/// linked server, which has its own check).
pub async fn account_gone(who: Who) {
    match who {
        None => std::future::pending::<()>().await,
        Some((engine, user, generation)) => account_ended(engine, user, generation).await,
    }
}

pub async fn while_account_allowed<F: std::future::Future<Output = ()>>(who: Who, session: F) {
    match who {
        None => session.await,
        Some((engine, user, generation)) => {
            tokio::select! {
                _ = session => {}
                _ = account_ended(engine, user, generation) => {}
            }
        }
    }
}

/// GET /api/whoami: identity + role of the caller, so the dashboard can
/// gate its own UI the same way the middleware gates the API.
async fn whoami(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let acc = accounts(&engine);
    let ip = client_ip(&req);
    // A Basic password tried here counts like any other try: this answer says
    // whether it was right, so it must not allow endless guessing.
    let tries = req.headers().contains_key(header::AUTHORIZATION);
    let has_session = crate::auth::session_from_headers(req.headers(), &acc).is_some();
    if !has_session && tries && crate::auth::is_locked(ip) && !has_valid_token(&acc, &req) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many failed sign-ins; try again in a few minutes").into_response();
    }
    let mut two_step_needed = false;
    let mut by_token = false;
    let id = match identify(&acc, req.headers(), ip).await {
        Ok(a) => {
            by_token = a.by_token;
            if a.basic {
                crate::devices::note_direct(&engine, &a.id.user, ip);
            }
            Some(a.id)
        }
        Err(NotAuthed::NotChecked(why)) => return not_checked(why),
        Err(why) => {
            if why == NotAuthed::Wrong {
                crate::auth::record_failure(ip);
                let _ = engine.db.insert_audit("?", &ip.to_string(), "sign-in failed (basic)", "/whoami", 401);
            }
            two_step_needed = why == NotAuthed::TwoStep;
            None
        }
    };
    // The same role the sign-in layer will give this caller's requests.
    let role = match &id {
        Some(i) => {
            let control = !by_token || req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).is_some_and(crate::auth::token_control);
            let cfg = engine.config.lock().unwrap();
            Some((crate::auth::role_of(&cfg, &i.user, i.admin, control), crate::auth::camera_limits(&cfg, &i.user).is_some()))
        }
        None if acc.is_empty() => Some((Role::Admin, false)),
        None => None,
    };
    Json(serde_json::json!({
        "signed_in": id.is_some() || acc.is_empty(),
        "user": id.as_ref().map(|i| i.user.clone()).unwrap_or_default(),
        // (Operators are not admins: an app that knows only this flag shows them a viewer's buttons.)
        "admin": id.as_ref().map(|i| i.admin).unwrap_or(acc.is_empty()),
        // "viewer", "operator" or "admin"; null when not signed in.
        "role": role.map(|r| r.0.name()),
        // What this session may do now beyond watching (all true for an admin's
        // session, all false for viewers).
        "perms": role.map(|r| r.0.perms_json(r.1, by_token)),
        // Limited to some cameras (an admin never is).
        "limited": role.is_some_and(|r| r.1),
        "auth_required": !acc.is_empty(),
        // What this server can do, for apps to check before asking (also
        // before sign-in): "caps" = transcoding for players (see transcode/).
        "features": FEATURES,
        // The right password in Basic for an account that needs a device token.
        "two_step_required": two_step_needed,
        // Came with a device token, so account changes are refused with 403.
        "by_token": by_token,
        // Sign-in troubleshooting: did a session cookie arrive at all?
        "session_cookie": !crate::auth::cookie_tokens(req.headers()).is_empty(),
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

/// `POST /api/login {"username","password"}` → session cookie.
pub(crate) async fn login(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let ip = client_ip(&req);
    let secure = arrived_secure(&req);
    if crate::auth::is_locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many failed sign-ins; try again in a few minutes").into_response();
    }
    let ctx = crate::devices::SignInCtx::new(&engine, req.headers(), ip);
    let bytes = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad request").into_response(),
    };
    // (The body may have been held back for a long time.)
    if crate::auth::is_locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many failed sign-ins; try again in a few minutes").into_response();
    }
    let Ok(body) = serde_json::from_slice::<LoginReq>(&bytes) else {
        return (StatusCode::BAD_REQUEST, "expected {\"username\", \"password\"}").into_response();
    };
    let user = body.username.trim().to_string();
    let generation = crate::auth::generation(&user);
    let acc = accounts(&engine);
    // Argon2 is slow on purpose, and costs memory: off the async runtime, a few at a time.
    let found = match crate::auth::password_check(ip, move || crate::auth::check_login(&acc, &user, &body.password)).await {
        Ok(found) => found,
        Err(why) => return not_checked(why),
    };
    // The right password for an account with two-step codes makes no session:
    // the code comes next, with this ticket, at /api/login/totp. (Nothing is
    // cleared from the address's count of failures until the code is right.)
    if let Some(id) = &found {
        match crate::auth::two_step_on(&id.user) {
            Ok(false) => {}
            Ok(true) => {
                // While six-digit codes are locked for this address or account the
                // ticket takes only a recovery code. Nobody is locked out of
                // their own account by someone who has their password.
                // (Limited per account and address; a ticket stores nothing, so nothing can fill up.)
                if !crate::auth::ticket_allowed(&id.user, ip) {
                    crate::account_http::tell_admins_once(&engine, &format!("login-burst:{}", id.user), 3600, format!("many sign-ins for {}", id.user), format!("The right password for {} keeps arriving, last from {ip}, faster than a person signs in. Someone has it: change it now (menu at the top, Change password, or an admin in Users).", id.user));
                    return (StatusCode::TOO_MANY_REQUESTS, "too many sign-ins from here; wait a minute").into_response();
                }
                let recovery_only = crate::auth::code_blocked(&id.user, ip);
                if let Some(ticket) = crate::auth::new_ticket(&id.user, generation, recovery_only) {
                    let _ = engine.db.insert_audit(&id.user, &ip.to_string(), if recovery_only { "sign-in: password right, only a recovery code accepted" } else { "sign-in: password right, code asked for" }, "", 200);
                    return Json(serde_json::json!({ "totp_required": true, "ticket": ticket, "expires_in": crate::auth::TICKET_SECS, "recovery_only": recovery_only })).into_response();
                }
                // (The account changed meanwhile: answered as below, no session.)
            }
            Err(()) => return (StatusCode::SERVICE_UNAVAILABLE, "cannot check two-step sign-in right now; try again").into_response(),
        }
    }
    let started = found.as_ref().filter(|id| crate::auth::two_step_on(&id.user) == Ok(false)).and_then(|id| crate::devices::complete_sign_in(&engine, &ctx, id.clone(), generation, crate::devices::How::Password));
    match found.zip(started) {
        Some((id, started)) => {
            crate::auth::record_success(ip);
            let _ = engine.db.insert_audit(&id.user, &ip.to_string(), "sign-in", "", 200);
            signed_in_response(&engine, id, started, secure)
        }
        None => {
            let locked = crate::auth::record_failure(ip);
            // Whatever was typed, kept short and printable (and never
            // mistaken for the server's own entries, which are in brackets).
            let typed: String = body.username.trim().chars().filter(|c| !c.is_control()).take(64).collect();
            let typed = if typed.starts_with('(') { format!("'{typed}'") } else { typed };
            let _ = engine.db.insert_audit(&typed, &ip.to_string(), "sign-in failed", "", 401);
            let msg = if locked { "wrong username or password; too many tries, wait a few minutes" } else { "wrong username or password" };
            (StatusCode::UNAUTHORIZED, msg).into_response()
        }
    }
}

/// Whether this request came in over TLS (the listener marks those). The
/// session cookie's `Secure` flag follows it, not the saved settings: with
/// HTTPS saved on but not yet applied (or saved off while still running),
/// the cookie must suit the connection the browser is actually using.
pub(crate) fn arrived_secure(req: &Request) -> bool {
    req.extensions().get::<crate::tls::Secure>().is_some()
}

/// The session cookie header value: `token` for `max_age` seconds (0 clears
/// it), `Secure` when the request came over HTTPS.
pub(crate) fn session_cookie(token: &str, max_age: u64, secure: bool) -> String {
    format!(
        "{}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}{}",
        crate::auth::SESSION_COOKIE,
        if secure { "; Secure" } else { "" }
    )
}

/// A new session for `id`, as a cookie on a JSON `{user, admin}` answer.
/// `secure`: the request came over HTTPS (see `arrived_secure`).
fn signed_in_response(engine: &Engine, id: crate::auth::Identity, started: crate::devices::Started, secure: bool) -> Response {
    let mut answer = serde_json::json!({ "user": id.user, "admin": id.admin });
    add_role(engine, &mut answer, &id.user, id.admin);
    signed_in_with(answer, started, secure)
}

/// A sign-in answer's `role`, `perms` and `limited`, as /api/whoami gives them
/// for the new session.
pub(crate) fn add_role(engine: &Engine, answer: &mut serde_json::Value, user: &str, admin: bool) {
    let cfg = engine.config.lock().unwrap();
    let role = crate::auth::role_of(&cfg, user, admin, true);
    let limited = crate::auth::camera_limits(&cfg, user).is_some();
    answer["role"] = role.name().into();
    answer["perms"] = role.perms_json(limited, false);
    answer["limited"] = limited.into();
}

/// `answer` (at least `{user, admin}`) with the cookies of a new session and
/// of the browser it was made in.
pub(crate) fn signed_in_with(answer: serde_json::Value, started: crate::devices::Started, secure: bool) -> Response {
    let cookie = session_cookie(&started.token, crate::auth::SESSION_DAYS as u64 * 86400, secure);
    Response::builder()
        .header(header::SET_COOKIE, cookie)
        .header(header::SET_COOKIE, crate::devices::set_cookie(&started.device_cookie, secure))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(answer.to_string()))
        .unwrap()
}

/// `POST /api/pair {"code"}` → session cookie. The phone side of the sign-in
/// QR code: a one-time code an admin made for this user (no password).
pub(crate) async fn pair(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let ip = client_ip(&req);
    let secure = arrived_secure(&req);
    if crate::auth::is_locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many failed sign-ins; try again in a few minutes").into_response();
    }
    #[derive(serde::Deserialize)]
    struct PairReq {
        code: String,
    }
    let ctx = crate::devices::SignInCtx::new(&engine, req.headers(), ip);
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 4096).await else {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    };
    let Ok(body) = serde_json::from_slice::<PairReq>(&bytes) else {
        return (StatusCode::BAD_REQUEST, "expected {\"code\"}").into_response();
    };
    let taken = crate::auth::take_pair_code_from(body.code.trim());
    let generation = taken.as_ref().map_or(0, |(u, _, _)| crate::auth::generation(u));
    let acc = accounts(&engine);
    let from_link = taken.as_ref().is_some_and(|t| t.2);
    let found = taken.and_then(|(u, admin_ok, _)| acc.iter().find(|(n, _, _)| *n == u).map(|(n, _, a)| crate::auth::Identity { user: n.clone(), admin: *a && admin_ok }));
    let how = if from_link { crate::devices::How::LinkedServer } else { crate::devices::How::QrCode };
    let started = found.as_ref().and_then(|id| crate::devices::complete_sign_in(&engine, &ctx, id.clone(), generation, how));
    match found.zip(started) {
        Some((id, started)) => {
            crate::auth::record_success(ip);
            let _ = engine.db.insert_audit(&id.user, &ip.to_string(), if from_link { "sign-in (linked server)" } else { "sign-in (QR code)" }, "", 200);
            signed_in_response(&engine, id, started, secure)
        }
        None => {
            crate::auth::record_failure(ip);
            let _ = engine.db.insert_audit("", &ip.to_string(), "sign-in failed (QR code)", "", 401);
            (StatusCode::UNAUTHORIZED, "this sign-in code has expired or was already used; ask for a new one").into_response()
        }
    }
}

/// `POST /api/logout` → ends the session and clears the cookie.
async fn logout(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let acc = accounts(&engine);
    for tok in crate::auth::cookie_tokens(req.headers()) {
        if let Some(id) = crate::auth::session_identity(tok, &acc) {
            let _ = engine.db.insert_audit(&id.user, &client_ip(&req).to_string(), "sign-out", "", 200);
        }
        crate::auth::end_session(tok);
    }
    Response::builder()
        // (A Secure cookie can only be cleared over HTTPS: browsers ignore
        // an http answer that tries to.)
        .header(header::SET_COOKIE, session_cookie("", 0, arrived_secure(&req)))
        .body(Body::from("{}"))
        .unwrap()
}

/// `GET /api/audit?limit=N` (admin) → recent sign-ins and changes.
async fn audit_log(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let acc = accounts(&engine);
    let ip = client_ip(&req);
    if !acc.is_empty() && !identify(&acc, req.headers(), ip).await.map(|a| a.id.admin).unwrap_or(false) {
        return (StatusCode::FORBIDDEN, "admin access required").into_response();
    }
    let limit = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("limit=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(200usize)
        .min(5000);
    let rows = engine.db.recent_audit(limit).unwrap_or_default();
    Json(serde_json::json!({ "entries": rows.into_iter().map(|(ts, user, ip, action, detail, status)| serde_json::json!({
        "ts": ts, "user": user, "ip": ip, "action": action, "detail": detail, "status": status
    })).collect::<Vec<_>>() }))
    .into_response()
}

/// Whether a camera has PTZ: true/false from the last check, null if not checked yet.
fn ptz_flag(id: usize, cfg: &CameraConfig) -> serde_json::Value {
    if crate::onvif_ptz::effective_cfg(id, cfg).onvif_port.is_none() {
        return false.into();
    }
    match crate::onvif_ptz::cached_any(id) {
        Some(e) if e.error.is_none() => e.caps.ptz.into(),
        _ => serde_json::Value::Null,
    }
}

/// Look at every camera's PTZ capabilities in the background (and again when
/// a camera's connection settings change or an earlier check failed). A
/// camera with no ONVIF port set is searched for one, at most one camera per
/// pass and never again for six hours if nothing answered.
fn spawn_ptz_checker(engine: Arc<Engine>) {
    std::thread::spawn(move || loop {
        let cams: Vec<_> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
        let mut searched = false;
        for h in cams {
            let mut cfg = h.cfg.read().unwrap().clone();
            crate::engine::normalize_creds(&mut cfg);
            if cfg.onvif_port.is_none() {
                if !searched && crate::onvif_ptz::find_begin(h.id, &cfg, false) {
                    searched = true;
                    let (id, c) = (h.id, cfg.clone());
                    std::thread::spawn(move || {
                        if crate::onvif_ptz::find_run(id, &c).is_some() {
                            crate::onvif_ptz::check_caps(id, &crate::onvif_ptz::effective_cfg(id, &c));
                        }
                    });
                }
                cfg = crate::onvif_ptz::effective_cfg(h.id, &cfg);
                if cfg.onvif_port.is_none() {
                    continue;
                }
            }
            let fp = crate::onvif_ptz::fingerprint(&cfg);
            let stale = match crate::onvif_ptz::cached_caps(h.id, &fp) {
                None => true,
                Some(e) => e.error.is_some() && e.at.elapsed() > std::time::Duration::from_secs(60),
            };
            if stale {
                crate::onvif_ptz::check_caps(h.id, &cfg);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(15));
    });
}

/// What the settings page shows about a camera's ONVIF port.
fn onvif_status(id: usize, cfg: &CameraConfig) -> serde_json::Value {
    if let Some(p) = cfg.onvif_port {
        return serde_json::json!({ "state": "configured", "configured": p, "found": null });
    }
    if !crate::onvif_ptz::find_wanted(cfg) {
        return serde_json::json!({ "state": "na", "configured": null, "found": null });
    }
    match crate::onvif_ptz::find_entry(id, cfg) {
        Some(e) if e.scanning => serde_json::json!({ "state": "searching", "configured": null, "found": null }),
        Some(e) if e.port.is_some() => serde_json::json!({ "state": "found", "configured": null, "found": e.port }),
        Some(_) => serde_json::json!({ "state": "none", "configured": null, "found": null }),
        None => serde_json::json!({ "state": "searching", "configured": null, "found": null }),
    }
}

fn onvif_cfg(engine: &Engine, id: usize) -> Result<CameraConfig, Response> {
    let h = engine.camera(id).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "no such camera"))?;
    let mut cfg = h.cfg.read().unwrap().clone();
    crate::engine::normalize_creds(&mut cfg);
    Ok(cfg)
}

/// `GET /api/cameras/:id/onvif` ->
/// `{ state: "configured"|"found"|"searching"|"none"|"na", configured, found }`
async fn onvif_get(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    match onvif_cfg(&engine, id) {
        Ok(cfg) => Json(onvif_status(id, &cfg)).into_response(),
        Err(r) => r,
    }
}

/// `POST /api/cameras/:id/onvif/find`: search the common ONVIF ports now
/// (waits for the answer) and return the same as the GET.
async fn onvif_find(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    let cfg = match onvif_cfg(&engine, id) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if cfg.onvif_port.is_some() {
        // A set port is checked again instead of searched for.
        let c2 = cfg.clone();
        let _ = tokio::task::spawn_blocking(move || crate::onvif_ptz::check_caps(id, &c2)).await;
        return Json(onvif_status(id, &cfg)).into_response();
    }
    if crate::onvif_ptz::find_begin(id, &cfg, true) {
        let c2 = cfg.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if crate::onvif_ptz::find_run(id, &c2).is_some() {
                crate::onvif_ptz::check_caps(id, &crate::onvif_ptz::effective_cfg(id, &c2));
            }
        })
        .await;
    }
    Json(onvif_status(id, &cfg)).into_response()
}

/// A camera address as shown to a viewer: no credentials, no push key.
/// A camera address as a viewer sees it: only the kind of source (where
/// the camera is on the network is not theirs to know).
fn viewer_url(u: &str) -> String {
    match u.split_once("://") {
        Some((scheme, _)) if !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+') => format!("{scheme}://(hidden)"),
        _ => "(hidden)".into(),
    }
}

/// `GET /api/overlay` — the current named overlay variables (name -> value).
async fn list_overlay_vars(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(serde_json::Value::Object(engine.overlay_vars_json()))
}

/// `POST /api/overlay/:name` (admin) — set the named overlay variable the live
/// overlay shows as `{v:<name>}`. The body is the plain value, or JSON
/// `{"value":"...","ttl":<seconds>}` to blank it after ttl; empty clears it.
/// Automations with the broker can also set these over MQTT
/// (`<prefix>/overlay/<name>/set`), like the shield and profile.
async fn set_overlay_var_http(
    State(engine): State<Arc<Engine>>,
    axum::Extension(admin): axum::Extension<IsAdmin>,
    token: Option<axum::Extension<TokenUsed>>,
    AxPath(name): AxPath<String>,
    body: String,
) -> Response {
    // A device token reaches here only with control rights on an admin's
    // account (auth_mw: token_may + no viewer writes to this path), but
    // IsAdmin is never set for tokens.
    if !admin.0 && token.is_none() {
        return api_error(StatusCode::FORBIDDEN, "admins only");
    }
    engine.set_overlay_var(&name, &body);
    StatusCode::NO_CONTENT.into_response()
}

/// What GET /api/cameras leaves out for anyone not an admin (viewers and
/// operators alike): logins, folders, event copies, capture helpers, pushed
/// senders, hardware and the watchdog.
const HIDDEN_FROM_NON_ADMINS: [&str; 12] = ["username", "folder", "copy_events_to", "copy_events_keep_days", "copy_events_only_confirmed", "copy_events_status", "capture", "capture_status", "push_sender", "hwaccel", "has_password", "watchdog"];

/// `GET /api/camera-defaults`: the shared detection and alert settings a
/// camera uses where it sets none of its own (motion, AI, alert priority and
/// interval), for showing "Default (...)" in a camera's settings. Nothing in
/// it is secret; the rest of the settings stay with admins.
async fn camera_defaults(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let c = engine.config.lock().unwrap();
    Json(serde_json::json!({
        "motion": c.motion,
        "ai": {
            "enabled": c.ai.enabled,
            "labels": c.ai.labels,
            "min_score": crate::config::f32_json(c.ai.min_score),
            "strict": c.ai.strict,
            "ignore_parked": c.ai.ignore_parked,
            "hide_unconfirmed": c.ai.hide_unconfirmed,
        },
        "alerts": { "priority": c.alerts.priority, "min_interval": c.alerts.min_interval },
    }))
}

async fn list_cameras(
    State(engine): State<Arc<Engine>>,
    axum::Extension(access): axum::Extension<Access>,
    axum::Extension(admin): axum::Extension<IsAdmin>,
) -> Json<serde_json::Value> {
    let cams = engine.cameras.read().unwrap();
    let list: Vec<serde_json::Value> = cams
        .iter()
        .filter(|slot| slot.as_ref().is_some_and(|h| access.can_name(&h.cfg.read().unwrap().name)))
        .map(|slot| {
            let h = slot.as_ref().unwrap();
            let st = h.state.read().unwrap();
            let cfg = h.cfg.read().unwrap();
            let mut v = serde_json::json!({
                "id": h.id,
                "ptz": ptz_flag(h.id, &cfg),
                "name": st.name,
                "short_name": cfg.short_name,
                "group": cfg.groups.first().cloned().unwrap_or_default(),
                "url": st.url,
                "online": st.online,
                "ingest": st.ingest,
                "ingest_setting": cfg.ingest,
                "on_demand": cfg.on_demand,
                "idle_timeout": cfg.idle_timeout,
                "sleeping": st.sleeping,
                "device_sleeping": st.device_sleeping,
                "reolink_channel": cfg.reolink_channel,
                "sub_url": cfg.sub_url,
                "hwaccel": cfg.hwaccel,
                "folder": cfg.folder,
                "min_object": cfg.min_object,
                "contrast": cfg.contrast,
                "ai": cfg.ai,
                "ai_labels": cfg.ai_labels,
                "blobs": st.blobs,
                "motion": st.motion,
                "enabled": h.active.load(std::sync::atomic::Ordering::Relaxed),
                "frames": st.frames,
                "score": st.score,
                "clip_count": st.clips.len(),
                "fps": cfg.fps,
                "scale": cfg.scale,
                "record": cfg.record,
                "direct": cfg.direct,
                "sensitivity": cfg.sensitivity,
                "zones": cfg.zones,
                "ignore_zones": cfg.ignore_zones,
                "schedule": cfg.schedule,
                "record_motion_only": cfg.record_motion_only,
                "manual_until": st.manual_until,
                "username": cfg.username,
                "has_password": !cfg.password.is_empty(),
                "overlay": cfg.overlay,
                "audio": cfg.audio,
            });
            v["ai_last"] = st.ai_last.as_ref().map(|(l, s, t)| serde_json::json!({"label": l, "score": s, "age_ms": (chrono::Utc::now().timestamp_millis() - t).max(0)})).into();
            // The camera's motion-detection setting ("motion" above is motion happening now).
            v["motion_enabled"] = cfg.motion.into();
            // A battery camera's pushed PIR motion (pushx) is active now: shown
            // even while the camera has no picture yet.
            v["push_motion"] = st.camera_detections.pulse_active().into();
            v["onvif_port"] = cfg.onvif_port.into();
            // Polygon zones and privacy masks (rectangles above are the simple kind).
            v["zone_polys"] = serde_json::json!(cfg.zone_polys);
            v["ignore_polys"] = serde_json::json!(cfg.ignore_polys);
            v["privacy_masks"] = serde_json::json!(cfg.privacy_masks);
            // Named zones (loitering areas, crossing lines) and which areas have someone loitering now.
            v["named_zones"] = serde_json::json!(cfg.named_zones);
            v["zones_occupied"] = serde_json::json!(st.zones_occupied);
            // Per-camera overrides (null = the shared default) and groups.
            let paused = engine.alerts_paused_until(&cfg.name);
            let extra = serde_json::json!({
                // Paused notifications; no end time = until resumed.
                "alerts_paused": paused.is_some(),
                "alerts_paused_until": paused.filter(|&u| u != i64::MAX),
                "record_sub": cfg.record_sub,
                "groups": cfg.groups,
                "motion_algorithm": cfg.motion_algorithm,
                "max_object": cfg.max_object,
                "min_travel": cfg.min_travel,
                "min_frames": cfg.min_frames,
                "break_time": cfg.break_time,
                "pre_roll": cfg.pre_roll,
                "post_roll": cfg.post_roll,
                "alerts": cfg.alerts,
                "alert_channels": cfg.alert_channels,
                "alert_interval": cfg.alert_interval,
                "alert_schedule": cfg.alert_schedule,
                "alert_labels": cfg.alert_labels,
                "alert_ignore_polys": cfg.alert_ignore_polys,
                "no_motion_minutes": cfg.no_motion_minutes,
                "no_motion_schedule": cfg.no_motion_schedule,
                "tamper": cfg.tamper,
                "unusual": cfg.unusual,
                "strangers": cfg.strangers,
                "audio_trigger": cfg.audio_trigger,
                "audio_margin_db": cfg.audio_margin_db.map(crate::config::f32_json),
                "audio_min_db": cfg.audio_min_db.map(crate::config::f32_json),
                "audio_sounds": cfg.audio_sounds,
                "ai_min_score": cfg.ai_min_score.map(crate::config::f32_json),
                "ai_ignore_parked": cfg.ai_ignore_parked,
                "ai_strict": cfg.ai_strict,
                "ai_hide_unconfirmed": cfg.ai_hide_unconfirmed,
                "ai_confirm_camera": cfg.ai_confirm_camera,
                "packages": cfg.packages.unwrap_or(false),
                "also_trigger": cfg.also_trigger,
                "alert_priority": cfg.alert_priority,
                "overlay_text": cfg.overlay_text,
                "overlay_pos": cfg.overlay_pos,
                // Watchdog settings; the plug's password is never sent.
                "watchdog": masked_watchdog(cfg.watchdog.as_ref()),
                // Pushed streams: who is sending right now.
                "push_sender": crate::ingest::push::push_key(&cfg.url).and_then(crate::ingest::push::sender).map(|(age, from)| serde_json::json!({ "from": from, "seconds": age.as_secs() })),
                // Screen capture from another computer, and what that computer last said.
                "capture": cfg.capture,
                "capture_status": cfg.capture.as_ref().and_then(|c| crate::helpers::camera_status(&c.helper, &cfg.name)),
            });
            if let (Some(a), serde_json::Value::Object(b)) = (v.as_object_mut(), extra) {
                a.extend(b);
            }
            // Event clips also saved to a folder (admins only, below).
            v["copy_events_to"] = cfg.copy_events_to.clone().into();
            v["copy_events_keep_days"] = cfg.copy_events_keep_days.into();
            v["copy_events_only_confirmed"] = cfg.copy_events_only_confirmed.into();
            v["copy_events_status"] = cfg.copy_folder().map(|_| crate::engine::event_copy::status_json(&cfg.name)).into();
            // Viewers see what a camera is, not how to reach or impersonate it
            // (a push key is the only secret guarding its stream input).
            if !admin.0 {
                if let Some(o) = v.as_object_mut() {
                    o.insert("url".into(), viewer_url(&cfg.url).into());
                    o.insert("sub_url".into(), cfg.sub_url.as_deref().map(viewer_url).into());
                    for k in HIDDEN_FROM_NON_ADMINS {
                        o.remove(k);
                    }
                }
            }
            v
        })
        .collect();
    let mut list = list;
    // Shown linked servers' cameras (not for camera-limited accounts).
    if !access.limited() {
        list.extend(crate::remote::ui_cameras(&engine));
    }
    Json(serde_json::json!({ "cameras": list, "overlay_vars": serde_json::Value::Object(engine.overlay_vars_json()) }))
}

/// Another camera already uses this name (compared as recordings are stored:
/// "Front Door" and "front-door" would share a folder) or this push key.
fn camera_clash(engine: &Engine, cam: &CameraConfig, except: Option<usize>) -> Option<String> {
    let new_slug = crate::config::slug(&cam.name);
    let new_push = crate::ingest::push::push_key(&cam.url).map(str::to_string);
    for (i, h) in engine.cameras.read().unwrap().iter().enumerate() {
        let Some(h) = h else { continue };
        if Some(i) == except {
            continue;
        }
        let other = h.cfg.read().unwrap();
        if crate::config::slug(&other.name) == new_slug {
            return Some(format!("another camera is already called \"{}\"; pick a different name", other.name));
        }
        if new_push.is_some() && crate::ingest::push::push_key(&other.url).map(str::to_string) == new_push {
            return Some(format!("\"{}\" already uses this push key", other.name));
        }
    }
    None
}

/// Most zones of one kind (rectangles or polygons) and corners of one polygon.
const MAX_ZONES: usize = 32;
const MAX_POINTS: usize = 64;

/// Two settings values the same, numbers to within float noise (a value
/// read from the config and sent back by a page need not be bit for bit equal).
fn json_close(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value as V;
    match (a, b) {
        (V::Number(x), V::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0),
            _ => x == y,
        },
        (V::Array(x), V::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_close(p, q)),
        (V::Object(x), V::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_close(v, w))),
        _ => a == b,
    }
}

/// Longest text in a camera's detection and alert settings.
const MAX_TEXT: usize = 200;
/// Most sounds a loud-sound trigger may name.
const MAX_SOUNDS: usize = 20;

/// "HH:MM-HH:MM" and nothing more.
fn strict_schedule(t: &str) -> bool {
    t.trim().split('-').count() == 2 && crate::config::parse_schedule(t).is_some()
}

/// Sizes, ranges and lengths of zones, lists and texts, checked when saving a
/// value that changed (a camera saved before these limits keeps saving as
/// long as that value is unchanged).
fn check_camera_sizes(cam: &CameraConfig, old: Option<&CameraConfig>) -> Result<(), String> {
    let changed = |f: &dyn Fn(&CameraConfig) -> serde_json::Value| old.is_none_or(|o| !json_close(&f(o), &f(cam)));
    const EPS: f64 = 1e-6;
    let inside = |q: &[f64; 2]| (-EPS..=1.0 + EPS).contains(&q[0]) && (-EPS..=1.0 + EPS).contains(&q[1]);
    let rect_ok = |r: &crate::config::ZoneRect| r.x >= -EPS && r.y >= -EPS && r.w > 0.0 && r.h > 0.0 && r.x + r.w <= 1.0 + EPS && r.y + r.h <= 1.0 + EPS;
    for (what, list, f) in [
        ("Zones", &cam.zones, (|c: &CameraConfig| serde_json::json!(c.zones)) as fn(&CameraConfig) -> serde_json::Value),
        ("Ignore zones", &cam.ignore_zones, |c: &CameraConfig| serde_json::json!(c.ignore_zones)),
        ("Privacy masks", &cam.privacy_masks, |c: &CameraConfig| serde_json::json!(c.privacy_masks)),
    ] {
        if (list.len() > MAX_ZONES || !list.iter().all(rect_ok)) && changed(&f) {
            return Err(format!("{what}: at most {MAX_ZONES} rectangles, each inside the picture and not empty"));
        }
    }
    for (what, v, f) in [
        ("Arm window", &cam.schedule, (|c: &CameraConfig| serde_json::json!(c.schedule)) as fn(&CameraConfig) -> serde_json::Value),
        ("Alert window", &cam.alert_schedule, |c: &CameraConfig| serde_json::json!(c.alert_schedule)),
        ("No-motion watch window", &cam.no_motion_schedule, |c: &CameraConfig| serde_json::json!(c.no_motion_schedule)),
    ] {
        if v.as_deref().is_some_and(|t| !t.trim().is_empty() && (t.chars().count() > MAX_TEXT || !strict_schedule(t))) && changed(&f) {
            return Err(format!("{what}: write it like 07:00-21:00"));
        }
    }
    if cam.motion_algorithm.as_deref().is_some_and(|a| !matches!(a, "objects" | "frame-diff")) && changed(&|c: &CameraConfig| serde_json::json!(c.motion_algorithm)) {
        return Err("Algorithm: objects or frame-diff".into());
    }
    if cam.alert_priority.as_deref().is_some_and(|p| !matches!(p, "low" | "normal" | "high" | "urgent")) && changed(&|c: &CameraConfig| serde_json::json!(c.alert_priority)) {
        return Err("Priority: low, normal, high or urgent".into());
    }
    let mut seen = std::collections::HashSet::new();
    if (cam.audio_sounds.len() > MAX_SOUNDS || !cam.audio_sounds.iter().all(|x| seen.insert(x.trim().to_lowercase())) || cam.audio_sounds.iter().any(|x| x.chars().count() > 40))
        && changed(&|c: &CameraConfig| serde_json::json!(c.audio_sounds))
    {
        return Err(format!("Only these sounds: at most {MAX_SOUNDS}, each once"));
    }
    for (what, list, f) in [
        ("Zones", &cam.zone_polys, (|c: &CameraConfig| serde_json::json!(c.zone_polys)) as fn(&CameraConfig) -> serde_json::Value),
        ("Ignore zones", &cam.ignore_polys, |c: &CameraConfig| serde_json::json!(c.ignore_polys)),
    ] {
        if (list.len() > MAX_ZONES || list.iter().any(|p| p.points.len() < 3 || p.points.len() > MAX_POINTS || !p.points.iter().all(inside))) && changed(&f) {
            return Err(format!("{what}: at most {MAX_ZONES} shapes, each with 3 to {MAX_POINTS} corners inside the picture"));
        }
    }
    let nz = &cam.named_zones;
    if (nz.len() > MAX_ZONES || nz.iter().any(|z| z.points.len() < 2 || z.points.len() > MAX_POINTS || !z.points.iter().all(inside) || z.labels.len() > 30 || z.name.chars().count() > 60))
        && changed(&|c: &CameraConfig| serde_json::json!(c.named_zones))
    {
        return Err(format!("Alert zones: at most {MAX_ZONES}, each with up to {MAX_POINTS} points inside the picture, 30 objects and a name of up to 60 characters"));
    }
    if cam.ai_labels.as_ref().is_some_and(|l| l.len() > 64 || l.iter().any(|x| x.trim().is_empty() || x.chars().count() > 40)) && changed(&|c: &CameraConfig| serde_json::json!(c.ai_labels)) {
        return Err("Objects that count: at most 64, each up to 40 characters".into());
    }
    Ok(())
}

/// Checks of a camera's settings shared by adding and changing one.
fn check_camera_fields(cam: &crate::config::CameraConfig) -> Result<(), String> {
    for (what, w) in [("Alert window", &cam.alert_schedule), ("No-motion watch window", &cam.no_motion_schedule)] {
        if w.as_deref().is_some_and(|t| !t.trim().is_empty() && crate::config::parse_schedule(t).is_none()) {
            return Err(format!("{what}: write it like 07:00-21:00"));
        }
    }
    if let Some(l) = &cam.alert_labels {
        if l.len() > 30 || l.iter().any(|x| x.trim().is_empty() || x.chars().count() > 40) {
            return Err("Notify only for: pick up to 30 objects".into());
        }
    }
    if cam.alert_ignore_polys.len() > 12
        || cam.alert_ignore_polys.iter().any(|p| p.points.len() < 3 || p.points.len() > 64 || p.points.iter().any(|q| !(0.0..=1.0).contains(&q[0]) || !(0.0..=1.0).contains(&q[1])))
    {
        return Err("Areas that don't notify: up to 12 areas, each with 3 or more corners inside the picture".into());
    }
    let names = crate::sound::labels::names();
    if let Some(bad) = cam.audio_sounds.iter().find(|s| !names.contains(&s.trim().to_lowercase().as_str())) {
        return Err(format!("Unknown sound '{bad}' (known: {})", names.join(", ")));
    }
    crate::config::check_named_zones(&cam.named_zones)?;
    if let Some(u) = &cam.unusual {
        if u.labels.len() > 20 || u.labels.iter().any(|l| l.trim().is_empty() || l.len() > 40) {
            return Err("Unusual activity: up to 20 labels, each a short word such as person or car".into());
        }
        if !matches!(u.mode.as_str(), "tag" | "notice" | "alert") {
            return Err("Unusual activity: the mode is tag, notice or alert".into());
        }
    }
    if let Some(s) = &cam.strangers {
        if s.schedule.as_deref().is_some_and(|t| !t.trim().is_empty() && crate::config::parse_schedule(t).is_none()) {
            return Err("Strangers: write the hours like 22:00-06:00".into());
        }
        if !matches!(s.outside.as_str(), "normal" | "quiet") {
            return Err("Strangers: outside the hours is normal or quiet".into());
        }
    }
    check_watchdog(cam)
}

async fn add_camera(
    State(engine): State<Arc<Engine>>,
    Json(mut cam): Json<CameraConfig>,
) -> (StatusCode, Json<serde_json::Value>) {
    cam.normalize_groups();
    // Added from Discovery with a saved login: the page never had its password.
    if cam.password.is_empty() && !cam.username.is_empty() {
        if let Some(l) = engine.config.lock().unwrap().discovery_logins.iter().find(|l| l.username == cam.username.trim()) {
            cam.password = l.password.clone();
        }
    }
    if cam.name.trim().is_empty() || cam.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name and url are required" })),
        );
    }
    if let Err(e) = check_local_url(&cam.url) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e })));
    }
    if let Err(e) = check_camera_fields(&cam).and_then(|_| check_camera_sizes(&cam, None)) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e })));
    }
    if let Some(e) = camera_clash(&engine, &cam, None) {
        return (StatusCode::CONFLICT, Json(serde_json::json!({ "error": e })));
    }
    if let Err(e) = check_copy_folder(&engine, &mut cam, None, None).await {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e })));
    }
    let id = engine.add_camera_arc(cam);
    crate::helpers::sync_all(&engine);
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "result": "success" })),
    )
}

/// The folder event clips are also saved to: one AmberSight may use (not a
/// system folder, not its own folders), keep days in range. Checked off the
/// async workers (it looks at the disk, which may be a slow share).
async fn check_copy_folder(engine: &Arc<Engine>, cam: &mut CameraConfig, id: Option<usize>, before: Option<&CameraConfig>) -> Result<(), String> {
    // An empty folder is "off": not written as copy_events_to = "".
    cam.copy_events_to = cam.copy_folder().map(str::to_string);
    if cam.copy_events_keep_days > 3650 {
        return Err("Keep copies: at most 3650 days (0 = keep them)".into());
    }
    let Some(path) = cam.copy_folder().map(str::to_string) else { return Ok(()) };
    // Two cameras would share one <Camera> folder in the same place.
    let others: Vec<CameraConfig> = engine.cameras.read().unwrap().iter().enumerate().filter(|(i, _)| Some(*i) != id).filter_map(|(_, h)| h.as_ref().map(|h| h.cfg.read().unwrap().clone())).collect();
    if let Some(o) = crate::engine::event_copy::name_clash(cam, others.iter()) {
        return Err(format!("Also save event clips to: \"{o}\" saves to this folder under the same name; rename one of the cameras or pick another folder"));
    }
    // Looked at on the disk only when it changed (every copy checks it again,
    // so one edited into the config file is caught too), and never for long:
    // a share that does not answer must not hold the page.
    if before.and_then(|b| b.copy_folder()) == Some(path.as_str()) {
        return Ok(());
    }
    // One check of a folder at a time (a hung share must not pile up threads).
    let Some(guard) = crate::engine::event_copy::begin_check(&path) else {
        return Err("Also save event clips to: still checking this folder (it may not be answering); try again in a moment".into());
    };
    let eng = engine.clone();
    let job = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let g = crate::engine::event_copy::guards(&eng);
        let r = crate::engine::event_copy::refusal(&path, &g);
        // Saved by the owner: this is the folder copies go to from now on.
        if r.is_none() {
            crate::engine::event_copy::accept_folder(&path, &g);
        }
        r
    });
    match tokio::time::timeout(std::time::Duration::from_secs(10), job).await {
        Err(_) => Err("Also save event clips to: the folder did not answer within 10 seconds (a network share or drive that is offline?)".into()),
        Ok(r) => match r.unwrap_or_else(|_| Some("could not check the folder".into())) {
            Some(r) => Err(format!("Also save event clips to: {r}")),
            None => Ok(()),
        },
    }
}

#[derive(serde::Deserialize)]
struct CopyTestBody {
    path: String,
}

/// `POST /api/event-copy/test {"path"}` (admin): whether event clips can be
/// saved to this folder now (a test file is written and removed there).
/// `{ok, message}`; the folder is never created.
async fn event_copy_test(State(engine): State<Arc<Engine>>, Json(b): Json<CopyTestBody>) -> Json<serde_json::Value> {
    let Some(guard) = crate::engine::event_copy::begin_check(&b.path) else {
        return Json(serde_json::json!({ "ok": false, "message": "Still checking this folder from before: it may not be answering (an offline share or drive). Try again in a moment." }));
    };
    let job = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let g = crate::engine::event_copy::guards(&engine);
        crate::engine::event_copy::test_folder(&b.path, &g)
    });
    let res = match tokio::time::timeout(std::time::Duration::from_secs(15), job).await {
        Err(_) => Err("The folder did not answer within 15 seconds: a network share or a drive may be offline or very slow.".into()),
        Ok(r) => r.unwrap_or_else(|_| Err("could not check the folder".into())),
    };
    Json(match res {
        Ok(m) => serde_json::json!({ "ok": true, "message": m }),
        Err(m) => serde_json::json!({ "ok": false, "message": m }),
    })
}

/// A device:// or screen:// URL must build valid ffmpeg input arguments.
fn check_local_url(url: &str) -> Result<(), String> {
    if crate::local_source::is_local(url) {
        crate::local_source::input_args(url.trim(), crate::local_source::host_os())?;
    }
    if crate::ingest::push::is_push_url(url) && !crate::ingest::push::push_key(url).is_some_and(crate::ingest::push::valid_key) {
        return Err("a stream key needs 12 to 128 letters, digits, - or _".into());
    }
    Ok(())
}

/// A camera's watchdog settings for the browser: the plug's password is
/// replaced by `has_password`.
fn masked_watchdog(w: Option<&crate::config::CameraWatchdog>) -> serde_json::Value {
    let Some(w) = w else { return serde_json::Value::Null };
    let mut v = serde_json::to_value(w).unwrap_or_default();
    if let Some(p) = v.get_mut("power").and_then(|p| p.as_object_mut()) {
        p.remove("password");
        p.insert("has_password".into(), w.power.as_ref().is_some_and(|p| !p.password.is_empty()).into());
    }
    v
}

/// A blank plug password keeps the stored one (it is never sent to the
/// page), but only for the same switch: pointed somewhere else, the
/// password must be typed again (else it would be sent to the new address).
fn keep_power_password(new: &mut CameraConfig, cur: &CameraConfig) {
    let Some(old) = cur.watchdog.as_ref().and_then(|w| w.power.as_ref()) else { return };
    if let Some(p) = new.watchdog.as_mut().and_then(|w| w.power.as_mut()) {
        let same = p.kind == old.kind && p.host == old.host && p.off_url == old.off_url && p.on_url == old.on_url && p.username == old.username;
        if p.password.is_empty() && same {
            p.password = old.password.clone();
        }
    }
}

/// Watchdog settings a person can easily get wrong.
fn check_watchdog(cam: &CameraConfig) -> Result<(), String> {
    let Some(w) = &cam.watchdog else { return Ok(()) };
    if let Some(m) = w.reboot.as_deref().filter(|m| !m.trim().is_empty() && *m != "auto") {
        if crate::camreboot::Method::parse(m).is_none() {
            return Err(format!("Restart method '{m}': pick auto, onvif, reolink, dahua, hikvision or none"));
        }
    }
    if let Some(p) = &w.power {
        crate::camreboot::power_requests(p).map_err(|e| format!("Power switch: {e:#}"))?;
    }
    Ok(())
}

/// Blank incoming password keeps the stored one (device creds are never
/// echoed back, so the UI cannot resend them).
fn merge_password(engine: Arc<Engine>, id: usize, cfg: CameraConfig) -> CameraConfig {
    let cams = engine.cameras.read().unwrap();
    if id < cams.len() {
        match cams[id].as_ref() {
            Some(h) => {
                let cur = h.cfg.read().unwrap();
                let mut out = cfg;
                if out.password.is_empty() && !cur.password.is_empty() {
                    out.password = cur.password.clone();
                }
                keep_power_password(&mut out, &cur);
                return out;
            }
            _ => {},
        }
    }
    cfg
}

/// Overlay the JSON object `body` onto `cur`. Keys present in `body` replace
/// the current value (null clears an optional setting); absent keys keep it.
fn merge_camera_json(cur: &CameraConfig, body: serde_json::Value) -> Result<CameraConfig, String> {
    let serde_json::Value::Object(patch) = body else {
        return Err("expected a JSON object".into());
    };
    let mut merged = serde_json::to_value(cur).map_err(|e| e.to_string())?;
    let obj = merged.as_object_mut().ok_or("internal error")?;
    for (k, v) in patch {
        obj.insert(k, v);
    }
    let mut c: CameraConfig = serde_json::from_value(merged).map_err(|e| format!("invalid camera settings: {e}"))?;
    c.normalize_groups();
    Ok(c)
}

/// What an operator with the camera_detection right may change in a camera's
/// settings: when and where motion counts, AI, and when and how it alerts.
/// Never how the camera is reached (URLs, logins, ingest), where or whether it
/// records, commands, folders, the watchdog, overlays, privacy masks, its name
/// or groups, or turning it on or off.
const OPERATOR_CAMERA_FIELDS: &[&str] = &[
    // Motion detection.
    "motion",
    "sensitivity",
    "min_object",
    "max_object",
    "contrast",
    "min_travel",
    "min_frames",
    "break_time",
    "motion_algorithm",
    "schedule",
    // Zones and ignore areas.
    "zones",
    "ignore_zones",
    "zone_polys",
    "ignore_polys",
    "named_zones",
    // Alerts.
    "alerts",
    "alert_interval",
    "alert_schedule",
    "alert_labels",
    "alert_ignore_polys",
    "alert_priority",
    "no_motion_minutes",
    "no_motion_schedule",
    // Sound trigger.
    "audio_trigger",
    "audio_margin_db",
    "audio_min_db",
    "audio_sounds",
    // AI.
    "ai",
    "ai_labels",
    "ai_min_score",
    "ai_ignore_parked",
    "ai_strict",
    "ai_hide_unconfirmed",
    "ai_confirm_camera",
];

/// The settings an edit from `cur` to `new` changes outside what an operator
/// may change (empty: none), compared as stored.
fn operator_edit_outside(cur: &CameraConfig, new: &CameraConfig) -> Vec<String> {
    let (Ok(serde_json::Value::Object(a)), Ok(serde_json::Value::Object(b))) = (serde_json::to_value(cur), serde_json::to_value(new)) else {
        return vec!["(the settings could not be compared)".into()];
    };
    let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter().filter(|k| !OPERATOR_CAMERA_FIELDS.contains(&k.as_str()) && a.get(*k) != b.get(*k)).cloned().collect()
}

/// The settings that differ between two versions of a camera (as stored).
fn changed_fields(a: &CameraConfig, b: &CameraConfig) -> Vec<String> {
    let (Ok(serde_json::Value::Object(a)), Ok(serde_json::Value::Object(b))) = (serde_json::to_value(a), serde_json::to_value(b)) else { return Vec::new() };
    let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).cloned().collect()
}

/// A camera setting an operator may change, in plain words (for admins).
fn setting_words(field: &str) -> &'static str {
    match field {
        "motion" => "motion detection on or off",
        "sensitivity" | "min_object" | "max_object" | "contrast" | "min_travel" | "min_frames" | "break_time" | "motion_algorithm" => "motion sensitivity",
        "schedule" => "arm window",
        "zones" | "zone_polys" => "detection zones",
        "ignore_zones" | "ignore_polys" => "ignore zones",
        "named_zones" => "alert zones",
        "alerts" => "notifications on or off",
        "alert_interval" => "alert interval",
        "alert_schedule" => "alert schedule",
        "alert_labels" => "objects that notify",
        "alert_ignore_polys" => "areas that don't notify",
        "alert_priority" => "alert priority",
        "no_motion_minutes" | "no_motion_schedule" => "no-motion alert",
        "audio_trigger" | "audio_margin_db" | "audio_min_db" | "audio_sounds" => "loud-sound trigger",
        "ai" => "AI on or off",
        "ai_labels" => "AI objects",
        "ai_min_score" => "AI confidence",
        _ => "AI confirmation",
    }
}

/// The changed settings in plain words, each once, in order.
fn changes_in_words(fields: &[String]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for f in fields {
        let w = setting_words(f);
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out.join(", ")
}

/// One message per camera for what operators do to it in this long.
const OPERATOR_NOTE_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

/// Tell admins what an operator changed on a camera: every change, gathered
/// into one message per camera every few minutes. `what` reads after the
/// operator's name ("changed Porch: alert schedule").
fn tell_operator_change(engine: &Arc<Engine>, user: &str, camera: &str, what: &str) {
    let c = camera.to_string();
    crate::account_http::tell_gathered(
        engine,
        format!("operator-camera:{camera}"),
        format!("{user} {what}"),
        OPERATOR_NOTE_WINDOW,
        (format!("{camera}: changed by operator {user}"), format!("{user} {what}. Every change an operator makes is reported; if one was not meant to happen, change it back in the camera's settings.")),
        move |items, n| (format!("{c}: more changes by operators"), format!("{n} more change(s) to {c} by operators in the last minutes: {items}")),
    );
}

/// Restarts of one camera that operators' edits may cause in `RESTART_BUDGET_WINDOW`.
const RESTART_BUDGET: usize = 3;
const RESTART_BUDGET_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

/// An operator's edit of `cur` with `body`, checked: the new settings, or the answer.
fn operator_edit(cur: &CameraConfig, body: &serde_json::Value) -> Result<CameraConfig, (StatusCode, serde_json::Value)> {
    let new = merge_camera_json(cur, body.clone()).map_err(|e| (StatusCode::BAD_REQUEST, serde_json::json!({ "error": e })))?;
    let mut base = cur.clone();
    base.normalize_groups();
    // Nothing else (the same values sent back unchanged are fine).
    let outside = operator_edit_outside(&base, &new);
    if !outside.is_empty() {
        return Err((StatusCode::FORBIDDEN, serde_json::json!({ "error": format!("only an admin can change these settings of a camera: {}", outside.join(", ")), "fields": outside })));
    }
    check_camera_fields(&new).and_then(|_| check_camera_sizes(&new, Some(cur))).map_err(|e| (StatusCode::BAD_REQUEST, serde_json::json!({ "error": e })))?;
    // A camera that records only on motion would stop recording.
    if cur.motion && !new.motion && cur.record_motion_only {
        return Err((StatusCode::FORBIDDEN, serde_json::json!({ "error": "this camera records only when there is motion: turning motion detection off would stop its recording, so only an admin can do that", "fields": ["motion"] })));
    }
    Ok(new)
}

/// `PUT /api/cameras/:id` from an operator: only `OPERATOR_CAMERA_FIELDS`, on
/// the settings as they are when saved (under the cameras lock), and only the
/// keys sent.
async fn operator_update_camera(engine: Arc<Engine>, id: usize, body: serde_json::Value, user: String, ip: String) -> (StatusCode, Json<serde_json::Value>) {
    type Refusal = (StatusCode, serde_json::Value);
    if !body.is_object() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "expected a JSON object" })));
    }
    // The work and the checks first, on the settings as they are now; under
    // the cameras lock only a quick look that nothing changed meanwhile.
    let Some(snap) = engine.camera(id).map(|h| h.cfg.read().unwrap().clone()) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "camera not found" })));
    };
    let new = match operator_edit(&snap, &body) {
        Ok(n) => n,
        Err((s, j)) => return (s, Json(j)),
    };
    let snap_v = serde_json::to_value(&snap).ok();
    let edit = move |cur: &CameraConfig| -> Result<CameraConfig, Refusal> {
        if snap_v.is_some() && serde_json::to_value(cur).ok() == snap_v {
            Ok(new)
        } else {
            Err((StatusCode::CONFLICT, serde_json::json!({ "error": "the camera's settings changed meanwhile: load them again and save" })))
        }
    };
    let (e2, name, who) = (engine.clone(), snap.name.clone(), user.clone());
    let may_restart = move |differs: &[String]| -> Result<(), Refusal> {
        if differs.is_empty() {
            return Ok(());
        }
        static RESTARTS: Mutex<Option<std::collections::HashMap<usize, std::collections::VecDeque<std::time::Instant>>>> = Mutex::new(None);
        let mut m = RESTARTS.lock().unwrap_or_else(|p| p.into_inner());
        let m = m.get_or_insert_with(Default::default);
        let now = std::time::Instant::now();
        let q = m.entry(id).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= RESTART_BUDGET_WINDOW) {
            q.pop_front();
        }
        if q.len() >= RESTART_BUDGET {
            let wait = q.front().map_or(RESTART_BUDGET_WINDOW, |t| RESTART_BUDGET_WINDOW.saturating_sub(now.duration_since(*t))).as_secs().max(1);
            crate::account_http::tell_admins_once(&e2, &format!("operator-restarts:{id}"), RESTART_BUDGET_WINDOW.as_secs(), format!("{name}: operators' changes held back"), format!("{who} tried to change {name} again; operators' changes restarted it {RESTART_BUDGET} times in the last 10 minutes, so more that restart it wait about {} minutes.", wait.div_ceil(60)));
            return Err((StatusCode::TOO_MANY_REQUESTS, serde_json::json!({ "error": format!("this camera restarted {RESTART_BUDGET} times for operators' changes in the last 10 minutes: wait {} minutes and save again", wait.div_ceil(60)), "retry_after": wait })));
        }
        q.push_back(now);
        Ok(())
    };
    let e = engine.clone();
    let r = tokio::task::spawn_blocking(move || e.update_camera_checked(id, edit, may_restart)).await;
    match r {
        Ok(Ok((before, after))) => {
            let fields = changed_fields(&before, &after);
            if !fields.is_empty() {
                let _ = engine.db.insert_audit(&user, &ip, &format!("camera settings changed: {}", fields.join(", ")), &after.name, 200);
            }
            if !fields.is_empty() {
                tell_operator_change(&engine, &user, &after.name, &format!("changed {}: {}", after.name, changes_in_words(&fields)));
            }
            crate::helpers::sync_all(&engine);
            (StatusCode::OK, Json(serde_json::json!({ "id": id, "result": "updated" })))
        }
        Ok(Err(Some((s, j)))) => (s, Json(j)),
        Ok(Err(None)) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "camera not found" }))),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "internal error" }))),
    }
}

async fn update_camera(
    State(engine): State<Arc<Engine>>,
    axum::Extension(role): axum::Extension<Role>,
    caller: Option<axum::Extension<Caller>>,
    peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    AxPath(id): AxPath<usize>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    // (The sign-in layer lets only admins and operators with this right here.)
    let operator = !role.is_admin();
    if operator && !role.may(Perm::CameraDetection) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "admin access required for changes" })));
    }
    if operator && crate::remote::is_remote(id) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "only an admin can change the groups of another server's camera" })));
    }
    if operator {
        let user = caller.map(|c| c.0 .0.clone()).unwrap_or_default();
        let ip = peer.map(|p| p.0.ip().to_string()).unwrap_or_default();
        return operator_update_camera(engine, id, body, user, ip).await;
    }
    if crate::remote::is_remote(id) {
        return match crate::remote::set_groups(&engine, id, &body) {
            Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "id": id, "result": "updated" }))),
            Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))),
        };
    }
    // Fields the request leaves out keep their current value, so a page
    // that does not know a setting (sub stream, folder, AI...) cannot erase it.
    let Some(cur) = engine.camera(id).map(|h| h.cfg.read().unwrap().clone()) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "camera not found" })));
    };
    let mut cam = match merge_camera_json(&cur, body) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))),
    };
    if cam.name.trim().is_empty() || cam.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name and url are required" })),
        );
    }
    if let Err(e) = check_local_url(&cam.url) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e })));
    }
    if let Err(e) = check_camera_fields(&cam).and_then(|_| check_camera_sizes(&cam, Some(&cur))) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e })));
    }
    if let Some(e) = camera_clash(&engine, &cam, Some(id)) {
        return (StatusCode::CONFLICT, Json(serde_json::json!({ "error": e })));
    }
    if let Err(e) = check_copy_folder(&engine, &mut cam, Some(id), Some(&cur)).await {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e })));
    }
    // Renamed: everything that refers to the camera by name follows it.
    if cur.name != cam.name {
        engine.rename_camera_refs(&cur.name, &cam.name);
    }
    let merged = merge_password(engine.clone(), id, cam);
    if engine.update_camera(id, merged) {
        crate::helpers::sync_all(&engine);
        (
            StatusCode::OK,
            Json(serde_json::json!({ "id": id, "result": "updated" })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "camera not found" })),
        )
    }
}

async fn remove_camera(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<usize>,
) -> Json<serde_json::Value> {
    if engine.remove_camera(id) {
        crate::helpers::sync_all(&engine);
        Json(serde_json::json!({ "result": "success" }))
    } else {
        Json(serde_json::json!({ "error": "camera not found" }))
    }
}

#[derive(serde::Deserialize)]
struct EnableBody {
    enabled: bool,
}

async fn set_enabled(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<usize>,
    Json(body): Json<EnableBody>,
) -> Json<serde_json::Value> {
    if engine.set_enabled(id, body.enabled) {
        crate::helpers::sync_all(&engine);
        Json(serde_json::json!({ "result": "success" }))
    } else {
        Json(serde_json::json!({ "error": "camera not found" }))
    }
}

#[derive(serde::Deserialize)]
struct ShieldBody {
    #[serde(default = "default_armed")]
    pub armed: bool,
    /// Arming: wait the configured arm delay first.
    #[serde(default)]
    pub delay: bool,
}

fn default_armed() -> bool {
    true
}

#[derive(serde::Deserialize)]
struct RecordBody {
    #[serde(default = "default_cam_id")]
    pub camera: usize,
    #[serde(default = "default_secs")]
    pub seconds: i64,
}

fn default_cam_id() -> usize {
    0
}

fn default_secs() -> i64 {
    15
}

async fn shield(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    token: Option<axum::Extension<TokenUsed>>,
    Json(body): Json<ShieldBody>,
) -> Json<serde_json::Value> {
    let source = source_of(&token);
    let token_name = token.map(|t| t.0 .0.clone());
    // Global arm/disarm (application-level; not per camera). (Off the
    // async workers: a change writes the audit log.)
    let res = tokio::task::spawn_blocking(move || {
        if body.armed && body.delay {
            let secs = engine.config.lock().unwrap().alerts.arm_delay;
            let at = engine.arm_after(secs, &caller.0);
            let armed = engine.armed_now();
            return serde_json::json!({ "armed": armed, "arming_until": (!armed).then_some(at) });
        }
        let changed = engine.set_shield(body.armed, &caller.0, &source);
        if let (Some(t), false, true) = (&token_name, body.armed, changed) {
            let (u, t2) = (caller.0.clone(), t.clone());
            crate::account_http::tell_gathered(
                &engine,
                format!("token-disarm:{}:{t}", caller.0),
                String::new(),
                TOKEN_NOTE_WINDOW,
                (format!("disarmed by a device token of {}", caller.0), format!("The device token \"{t}\" of {} disarmed the system. If that was not meant to happen, revoke it in Users and arm again.", caller.0)),
                move |_, n| (format!("disarmed again by a device token of {u}"), format!("The device token \"{t2}\" of {u} disarmed the system {n} more time(s) in the last minute. If that was not meant to happen, revoke it in Users and arm again.")),
            );
        }
        serde_json::json!({ "armed": body.armed })
    })
    .await;
    Json(res.unwrap_or_else(|_| serde_json::json!({ "error": "internal error" })))
}

#[derive(serde::Deserialize)]
struct PauseBody {
    /// Minutes; 0 resumes; missing/null = until resumed.
    #[serde(default)]
    minutes: Option<u32>,
}

/// `GET /api/noisy` (admin): cameras that sent many notifications in the
/// last 24 hours for one thing in one part of the picture, with what to do.
async fn noisy_list(State(engine): State<Arc<Engine>>) -> Response {
    match tokio::task::spawn_blocking(move || engine.noisy_suggestions()).await {
        Ok(v) => Json(serde_json::json!({ "suggestions": v })).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "could not look at the notifications" }))).into_response(),
    }
}

/// `POST /api/noisy/:id/dismiss` (admin): hide this camera's suggestion for 7 days.
async fn noisy_dismiss(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    let Some(name) = engine.camera(id).map(|h| h.cfg.read().unwrap().name.clone()) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "camera not found" }))).into_response();
    };
    engine.noisy_dismiss(&name);
    Json(serde_json::json!({ "camera": name, "dismissed_days": crate::engine::noisy::DISMISS_DAYS })).into_response()
}

/// `POST /api/cameras/:id/pause {minutes}`: pause this camera's
/// notifications (admins; operators with the arm right and device tokens for
/// a day at most). Motion, recording and events carry on.
async fn pause_alerts(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, token: Option<axum::Extension<TokenUsed>>, role: Option<axum::Extension<Role>>, AxPath(id): AxPath<usize>, Json(body): Json<PauseBody>) -> Response {
    let Some(h) = engine.camera(id) else {
        return (StatusCode::NOT_FOUND, "camera not found").into_response();
    };
    let name = h.cfg.read().unwrap().name.clone();
    // A device token, or an operator: a day at most, never "until resumed".
    let capped = token.is_some() || role.as_ref().is_some_and(|r| r.0.is_operator());
    let minutes = if capped { token_pause(body.minutes) } else { body.minutes };
    if let (Some(t), true) = (&token, minutes != Some(0)) {
        let (u, t2, n2) = (caller.0.clone(), t.0 .0.clone(), name.clone());
        crate::account_http::tell_gathered(
            &engine,
            format!("token-pause:{}:{}:{name}", caller.0, t.0 .0),
            String::new(),
            TOKEN_NOTE_WINDOW,
            (format!("notifications paused by a device token of {}", caller.0), format!("The device token \"{}\" of {} paused the notifications of {name}. If that was not meant to happen, revoke it in Users and resume them.", t.0 .0, caller.0)),
            move |_, n| (format!("notifications paused again by a device token of {u}"), format!("The device token \"{t2}\" of {u} paused the notifications of {n2} {n} more time(s) in the last minute. If that was not meant to happen, revoke it in Users and resume them.")),
        );
    }
    let until = engine.pause_alerts(&name, minutes);
    Json(serde_json::json!({ "camera": name, "alerts_paused": until.is_some(), "alerts_paused_until": until.filter(|&u| u != i64::MAX) })).into_response()
}

async fn record_clip(
    State(engine): State<Arc<Engine>>,
    axum::Extension(access): axum::Extension<Access>,
    Json(body): Json<RecordBody>,
) -> Json<serde_json::Value> {
    // (The camera is named in the body, not the path: checked here for
    // accounts limited to some cameras, answered as if it did not exist.)
    if !engine.camera(body.camera).is_some_and(|h| access.can_name(&h.cfg.read().unwrap().name)) {
        return Json(serde_json::json!({ "error": "camera not found" }));
    }
    // Manual clip: force motion on one camera for N seconds.
    let secs = record_seconds(body.seconds);
    if engine.manual_clip(body.camera, secs) {
        Json(serde_json::json!({ "recording": true, "camera": body.camera, "seconds": secs }))
    } else {
        Json(serde_json::json!({ "error": "camera not found" }))
    }
}

async fn status(State(engine): State<Arc<Engine>>, axum::Extension(admin): axum::Extension<IsAdmin>) -> Json<serde_json::Value> {
    // (Disk and database work: off the async threads.)
    let e = engine.clone();
    let mut v = tokio::task::spawn_blocking(move || status_json(&e)).await.unwrap_or_default();
    // Folder paths are for admins.
    if !admin.0 {
        if let Some(o) = v.as_object_mut() {
            o.remove("data_dir");
            if let Some(f) = o.get_mut("storage").and_then(|s| s.get_mut("folders")).and_then(|f| f.as_array_mut()) {
                for x in f.iter_mut() {
                    if let Some(x) = x.as_object_mut() {
                        x.remove("path");
                    }
                }
            }
        }
    }
    Json(v)
}

fn status_json(engine: &Arc<Engine>) -> serde_json::Value {
    // (Before taking the camera list: disk and database work.)
    let storage = engine.storage_status();
    let cams = engine.cameras.read().unwrap();
    let online = cams.iter().filter(|s| s.is_some()).map(|s| s.as_ref().unwrap()).filter(|h| h.state.read().unwrap().online).count();
    let total = cams.iter().filter(|s| s.is_some()).count();
    drop(cams);
    let events = engine.total_events();
    serde_json::json!({
        "app": "AmberSight",
        "name": crate::links::own_name(engine),
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": chrono::Utc::now().timestamp() - engine.started,
        "cameras": total,
        "online": online,
        "armed": engine.armed_now(),
        "arming_until": engine.arming_until(),
        "arm_delay": engine.config.lock().unwrap().alerts.arm_delay,
        "events": events,
        "webhook": engine.config.lock().unwrap().alerts.webhook_url.is_some(),
        "data_dir": engine.data_dir.to_string_lossy(),
        "storage": storage,
        "profile": engine.active_profile(),
        "ffmpeg": crate::ffmpeg_bin::status(),
    })
}

#[derive(serde::Deserialize)]
struct StatsQ {
    #[serde(default)]
    since: u64,
}

/// Live machine/camera/storage figures for the status panel (cached by the
/// sampler thread, so this never touches the disk). `since` = last history
/// sequence the page already has.
async fn stats(
    State(engine): State<Arc<Engine>>,
    axum::Extension(access): axum::Extension<Access>,
    axum::Extension(admin): axum::Extension<IsAdmin>,
    Query(q): Query<StatsQ>,
) -> Response {
    let up = chrono::Utc::now().timestamp() - engine.started;
    let mut j = engine.stats.json(up, q.since, &|c| access.can_slug(&c.slug));
    // Folder paths and machine details (GPU, network, AI, write rates) are for admins.
    if !admin.0 {
        strip_stats_for_viewer(&mut j);
    }
    ([(header::CACHE_CONTROL, "no-store")], Json(j)).into_response()
}

/// What a viewer (not admin) does not get from /api/stats: folder paths and
/// names, write rates, and the machine's GPU, network and AI details.
fn strip_stats_for_viewer(j: &mut serde_json::Value) {
    for d in j.get_mut("storage").and_then(|s| s.as_array_mut()).into_iter().flatten() {
        if let Some(o) = d.as_object_mut() {
            for k in ["path", "write_bps", "tier"] {
                o.remove(k);
            }
        }
    }
    if let Some(o) = j.as_object_mut() {
        for k in ["gpu", "net", "ai"] {
            o.remove(k);
        }
    }
    for h in ["history", "history_1h"] {
        if let Some(o) = j.get_mut(h).and_then(|v| v.as_object_mut()) {
            for k in ["ai_p50", "ai_checks", "gpu", "rx", "tx", "writes"] {
                o.remove(k);
            }
        }
    }
}

async fn export(
    State(engine): State<Arc<Engine>>,
    axum::Extension(access): axum::Extension<Access>,
    Query(q): Query<EventQuery>,
) -> Response {
    let q = EventQuery { camera: q.camera.filter(|c| !c.is_empty()), ..q };
    // `ids=a,b,c` (exactly these events) or the list filters (`cameras`,
    // `protect`, `objects`, `review`): the events the caller may see.
    let by_ids = q.ids.is_some();
    let by_query = by_ids || q.cameras.is_some() || q.protect.is_some() || q.objects.is_some() || q.review.is_some();
    let id_query = if by_query {
        let names = visible_names(&engine, &access);
        // Same cap as the classic path. Events the cameras' settings hide as
        // unconfirmed are left out of a filter export exactly as /api/events
        // leaves them out (`all=1` includes them); events picked by `ids` are
        // always included, and the classic params (camera/since/until/object
        // alone) keep their old behaviour of exporting everything.
        let hide = if by_ids || q.all { Vec::new() } else { engine.hide_unconfirmed_cameras() };
        if by_ids && !csv_list(&q.ids, EXPORT_IDS_MAX).iter().any(|i| event_id_ok(i)) {
            return (StatusCode::BAD_REQUEST, "no event ids given").into_response();
        }
        match event_db_query(&q, &access, names, hide, if by_ids { EXPORT_IDS_MAX } else { crate::engine::export::EXPORT_MAX_EVENTS }) {
            Some(b) if by_ids && b.ids.is_empty() => return (StatusCode::BAD_REQUEST, "no event ids given").into_response(),
            other => other,
        }
    } else {
        None
    };
    if !by_query && access.limited() && q.camera.as_deref().is_none_or(|c| !access.can_name(c)) {
        return (StatusCode::BAD_REQUEST, "choose a camera to export").into_response();
    }
    // Evidence export: ZIP of matching clips + events.csv manifest, written
    // to a temporary file and sent from there (it can be up to ~1 GB), two
    // exports at a time.
    static EXPORTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let Ok(permit) = EXPORTS.try_acquire() else {
        return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "30")], "other exports are running; try again shortly").into_response();
    };
    let dir = engine.data_dir.join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    // Left over from a crash: gone after an hour.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for f in rd.flatten() {
            let old = f.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a.as_secs() > 3600);
            if old && f.file_name().to_string_lossy().starts_with("export-") {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = dir.join(format!("export-{}-{}.zip", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let (e, p) = (engine.clone(), path.clone());
    let built = tokio::task::spawn_blocking(move || match id_query {
        Some(iq) => {
            let evs = e.db.query_events(&iq).map_err(std::io::Error::other)?;
            e.export_events_zip_to(&p, &evs)
        }
        // Filters asked for but nothing visible: an empty export.
        None if by_query => e.export_events_zip_to(&p, &[]),
        None => e.export_zip_to(&p, q.camera, q.since, q.until, q.object),
    })
    .await;
    // Removes the file once it has been sent (or the download stopped).
    struct Remove(std::path::PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let remove = Remove(path.clone());
    if !matches!(built, Ok(Ok(()))) {
        if let Ok(Err(e)) = built {
            tracing::warn!("export: {e:#}");
        }
        return (StatusCode::INTERNAL_SERVER_ERROR, "export failed (details are in the server log)").into_response();
    }
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "export failed").into_response();
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = futures::stream::unfold((Some(tokio_util::io::ReaderStream::new(file)), remove, permit), |(mut rs, remove, permit)| async move {
        let next = match rs.as_mut() {
            Some(r) => futures::StreamExt::next(r).await,
            None => None,
        };
        match next {
            Some(chunk) => Some((chunk, (rs, remove, permit))),
            None => {
                drop(rs.take()); // close the file before it is removed
                drop(remove);
                drop(permit);
                None
            }
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CONTENT_LENGTH, len)
        .header("content-disposition", "attachment; filename=\"ambersight-events.zip\"")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream)).unwrap()
}

#[derive(serde::Deserialize)]
struct DownloadQ {
    #[serde(default)]
    download: String,
    /// "low": the sub stream where it was recorded (data saver).
    #[serde(default)]
    quality: String,
    /// What the player decodes (see transcode/): transcoded when the
    /// clip does not fit. Not with `download`.
    #[serde(default)]
    caps: Option<String>,
    /// With caps: ms from the clip's start.
    #[serde(default)]
    start: Option<i64>,
    /// With caps, "1": what would be sent, as JSON.
    #[serde(default)]
    info: String,
}

/// `caps` of a request (None: not given or a download); Err: malformed.
fn caps_of(caps: &Option<String>, download: bool) -> Result<Option<crate::transcode::Caps>, Response> {
    match caps.as_deref().filter(|_| !download).map(crate::transcode::Caps::parse) {
        None => Ok(None),
        Some(Ok(c)) => Ok(Some(c)),
        Some(Err(e)) => Err((StatusCode::BAD_REQUEST, e).into_response()),
    }
}

/// `GET /api/whoami` `features`: abilities apps can test for (the version
/// string is for the updater).
const FEATURES: &[&str] = &["caps", "operator", "event_copy"];

/// How a request with caps asks for the bytes: HEAD or `Range: bytes=0-0`
/// (headers only), a range from further in, or from the start.
fn ask_of(req: &Request) -> crate::transcode::http::Ask {
    use crate::transcode::http::Ask;
    if req.method() == axum::http::Method::HEAD {
        return Ask::Headers;
    }
    match req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(|r| r.trim().to_string()) {
        None => Ask::Start,
        Some(r) if r == "bytes=0-0" => Ask::Headers,
        Some(r) if r == "bytes=0-" => Ask::Start,
        Some(_) => Ask::Resume,
    }
}

fn with_headers(mut r: Response, headers: Vec<(&'static str, String)>) -> Response {
    for (k, v) in headers {
        if let Ok(v) = HeaderValue::from_str(&v) {
            r.headers_mut().insert(k, v);
        }
    }
    r
}

#[derive(serde::Deserialize)]
struct EventQuery {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
    // classic NVR-style event search filters.
    #[serde(default)]
    camera: Option<String>,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    until: Option<i64>,
    #[serde(default)]
    object: Option<String>,
    /// Include events left out as unconfirmed (AI found nothing that counts).
    /// `1`, `true`, `yes` or `on` (the Events page sends `all=1`).
    #[serde(default, deserialize_with = "query_flag")]
    all: bool,
    /// Camera names, comma separated (each must be one the caller may see).
    #[serde(default)]
    cameras: Option<String>,
    /// `1`/`true`: protected events only.
    #[serde(default)]
    protect: Option<String>,
    /// Labels, comma separated: events with any of them.
    #[serde(default)]
    objects: Option<String>,
    /// Verdicts, comma separated: false_alarm, ok, none (not reviewed), any.
    #[serde(default)]
    review: Option<String>,
    /// Event ids, comma separated (export: exactly these).
    #[serde(default)]
    ids: Option<String>,
}
fn default_limit() -> usize {
    100
}

/// A yes/no query parameter that also accepts `1`/`0` (serde's bool only
/// takes `true`/`false`, so `?all=1` used to be refused with 400).
fn query_flag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let s = <String as serde::Deserialize>::deserialize(d)?;
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        other => Err(serde::de::Error::custom(format!("expected 1/0 or true/false, got {other:?}"))),
    }
}

/// Names of the cameras the caller may see, when that is not all of them.
fn visible_names(engine: &Engine, access: &Access) -> Option<Vec<String>> {
    access.limited().then(|| engine.cameras.read().unwrap().iter().flatten().map(|h| h.cfg.read().unwrap().name.clone()).filter(|n| access.can_name(n)).collect())
}

/// Most event ids one export by ids takes.
const EXPORT_IDS_MAX: usize = 200;

fn csv_list(v: &Option<String>, max: usize) -> Vec<String> {
    v.as_deref().unwrap_or("").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).take(max).collect()
}

/// The database query for an events request, or None when nothing can match
/// (a camera the caller may not see, no cameras left). `visible` = the
/// cameras a limited caller may see (None: all). `hide_unconfirmed` as the
/// cameras' settings say.
/// What an event id can be made of.
fn event_id_ok(id: &str) -> bool {
    id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn event_db_query(q: &EventQuery, access: &Access, visible: Option<Vec<String>>, hide_unconfirmed: Vec<String>, limit: usize) -> Option<crate::db::EventQuery> {
    if access.limited() && q.camera.as_deref().is_some_and(|c| !access.can_name(c)) {
        return None;
    }
    let asked = csv_list(&q.cameras, 200);
    let mut cameras: Vec<String> = asked.iter().filter(|c| access.can_name(c)).cloned().collect();
    if !asked.is_empty() && cameras.is_empty() {
        return None;
    }
    if cameras.is_empty() {
        // A limited caller with no camera chosen: the cameras they may see.
        if let Some(v) = visible {
            if v.is_empty() {
                return None;
            }
            cameras = v;
        }
    }
    let review: Vec<String> = csv_list(&q.review, 8)
        .into_iter()
        .map(|r| r.to_ascii_lowercase())
        .filter(|r| matches!(r.as_str(), "false_alarm" | "ok" | "none"))
        .collect();
    let asked = csv_list(&q.ids, EXPORT_IDS_MAX);
    let ids: Vec<String> = asked.iter().filter(|i| event_id_ok(i)).cloned().collect();
    if !asked.is_empty() && ids.is_empty() {
        // Asked for events by id, none of them a real id: none (never the newest in their place).
        return None;
    }
    Some(crate::db::EventQuery {
        camera: q.camera.clone().filter(|c| !c.is_empty()),
        since: q.since,
        until: q.until,
        object: q.object.clone(),
        limit,
        offset: q.offset,
        hide_unconfirmed,
        cameras,
        review,
        protected_only: matches!(q.protect.as_deref(), Some("1" | "true" | "yes" | "on")),
        objects_any: csv_list(&q.objects, 20),
        ids,
    })
}

async fn events(
    State(engine): State<Arc<Engine>>,
    axum::Extension(access): axum::Extension<Access>,
    Query(q): Query<EventQuery>,
) -> Json<serde_json::Value> {
    // One page at most this big (the web app asks for up to 500).
    let q = EventQuery { camera: q.camera.filter(|c| !c.is_empty()), limit: q.limit.min(EVENTS_PAGE_MAX), offset: q.offset.min(EVENTS_OFFSET_MAX), ..q };
    let hide = if q.all { Vec::new() } else { engine.hide_unconfirmed_cameras() };
    let Some(query) = event_db_query(&q, &access, visible_names(&engine, &access), hide, q.limit) else {
        return Json(serde_json::json!({ "events": [] }));
    };
    let e = engine.clone();
    let out = tokio::task::spawn_blocking(move || {
        let evs = e.db.query_events(&query).unwrap_or_else(|err| {
            tracing::warn!("event query failed: {err:#}");
            Vec::new()
        });
        let ids: Vec<String> = evs.iter().map(|ev| ev.id.clone()).collect();
        let reviews = e.db.event_reviews(&ids).unwrap_or_default();
        evs.into_iter()
            .map(|ev| {
                let r = reviews.get(&ev.id).cloned();
                let mut j = serde_json::to_value(ev.public()).unwrap_or_default();
                if let Some(o) = j.as_object_mut() {
                    o.insert("review".into(), r.as_ref().map(|r| r.0.clone()).into());
                    o.insert("reviewed_at".into(), r.map(|r| r.1).into());
                }
                j
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Json(serde_json::json!({ "events": out }))
}

const EVENTS_PAGE_MAX: usize = 2000;
const EVENTS_OFFSET_MAX: usize = 1_000_000;

#[derive(serde::Deserialize)]
struct MarksQuery {
    since: i64,
    until: i64,
    /// Camera names, comma separated (the cameras the timeline shows).
    #[serde(default)]
    cameras: String,
    /// Seconds per slot for a zoomed-out view: alerts of one camera and kind
    /// in one slot come as one mark with a count. 0: every alert.
    #[serde(default)]
    bucket: i64,
}

/// Most alert marks one timeline request returns (newest first).
const MARKS_MAX: usize = 20_000;

/// The timeline's alert kinds (as livetl.js draws them): 0 person, 1
/// vehicle, 2 animal, 3 anything else.
fn mark_kind(objects: &[String]) -> u8 {
    const VEH: &[&str] = &["car", "truck", "bus", "motorcycle", "bicycle", "vehicle", "van", "bike", "motorbike", "suv"];
    const ANI: &[&str] = &["dog", "cat", "bird", "animal", "pet", "horse", "cow", "sheep", "bear", "fox", "deer", "squirrel", "rabbit", "raccoon", "coyote"];
    let low: Vec<String> = objects.iter().map(|o| o.to_lowercase()).collect();
    // Package and face alerts group on their own, like the timeline's markers.
    if low.iter().any(|o| o.starts_with("package")) {
        4
    } else if low.iter().any(|o| o == "face" || o.ends_with(" face")) {
        5
    } else if low.iter().any(|o| matches!(o.as_str(), "person" | "people" | "human" | "pedestrian")) {
        0
    } else if low.iter().any(|o| VEH.contains(&o.as_str())) {
        1
    } else if low.iter().any(|o| ANI.contains(&o.as_str())) {
        2
    } else {
        3
    }
}

/// `GET /api/timeline/events?since&until&cameras=A,B[&bucket=S]` → the
/// timeline's alert marks, compact: `{cameras: [names], rows: [[id, camera
/// index, start, end, objects, has picture, count]], more}`. With a bucket,
/// one row per camera, kind and slot (id and picture of its earliest alert),
/// so a view of weeks of busy cameras stays small.
async fn timeline_events(
    State(engine): State<Arc<Engine>>,
    axum::Extension(access): axum::Extension<Access>,
    Query(q): Query<MarksQuery>,
) -> Json<serde_json::Value> {
    let names: Vec<String> = q.cameras.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && access.can_name(s)).take(200).collect();
    if names.is_empty() {
        return Json(serde_json::json!({ "cameras": [], "rows": [], "more": false }));
    }
    let query = crate::db::EventQuery {
        since: Some(q.since),
        until: Some(q.until),
        hide_unconfirmed: engine.hide_unconfirmed_cameras(),
        cameras: names,
        ..Default::default()
    };
    let bucket = q.bucket.clamp(0, 86_400 * 7);
    let e = engine.clone();
    let out = tokio::task::spawn_blocking(move || {
        let mut cams: Vec<String> = Vec::new();
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut more = false;
        // (camera index, slot, kind) -> (id, start, end, objects, picture, count)
        type Slot = (String, i64, i64, Vec<String>, bool, u64);
        let mut slots: std::collections::HashMap<(usize, i64, u8), Slot> = std::collections::HashMap::new();
        let res = e.db.each_event_mark(&query, |id, camera, start, end, objects, thumb| {
            if bucket == 0 && rows.len() >= MARKS_MAX {
                more = true;
                return;
            }
            let ci = match cams.iter().position(|c| *c == camera) {
                Some(i) => i,
                None => {
                    cams.push(camera);
                    cams.len() - 1
                }
            };
            let objs: Vec<String> = serde_json::from_str(&objects).unwrap_or_default();
            if bucket == 0 {
                rows.push(serde_json::json!([id, ci, start, end, objs, thumb, 1]));
                return;
            }
            let key = (ci, start.div_euclid(bucket), mark_kind(&objs));
            let s = slots.entry(key).or_insert_with(|| (id.clone(), start, end, Vec::new(), thumb, 0));
            // Newest first: the last one seen is the earliest.
            if start <= s.1 {
                s.0 = id;
                s.1 = start;
                s.4 = thumb;
            }
            s.2 = s.2.max(end);
            s.5 += 1;
            for o in objs {
                if s.3.len() < 6 && !s.3.contains(&o) {
                    s.3.push(o);
                }
            }
        });
        if let Err(err) = res {
            tracing::warn!("timeline alert marks: {err:#}");
        }
        for ((ci, _, _), (id, start, end, objs, thumb, n)) in slots {
            rows.push(serde_json::json!([id, ci, start, end, objs, thumb, n]));
        }
        serde_json::json!({ "cameras": cams, "rows": rows, "more": more })
    })
    .await
    .unwrap_or_else(|_| serde_json::json!({ "cameras": [], "rows": [], "more": false }));
    Json(out)
}

async fn event_thumb(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<String>,
) -> Response {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return (StatusCode::BAD_REQUEST, "bad event id").into_response();
    }
    let path = engine.data_dir.join("thumbs").join(format!("{id}.jpg"));
    match std::fs::read(path) {
        Ok(bytes) => Response::builder()
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => (StatusCode::NOT_FOUND, "no thumbnail").into_response(),
    }
}

async fn seek_at(
    State(engine): State<Arc<Engine>>,
    Query(q): Query<SeekQ>,
) -> Json<serde_json::Value> {
    let name = q.camera.unwrap_or_default();
    let at = q.at.unwrap_or(0);
    Json(engine.seek(&name, at))
}

#[derive(serde::Deserialize)]
struct SeekQ {
    #[serde(default)]
    pub camera: Option<String>,
    #[serde(default)]
    pub at: Option<i64>,
}

#[derive(serde::Deserialize)]
struct ProtectReq {
    #[serde(default = "yes")]
    pub protect: bool,
}
fn yes() -> bool { true }

async fn protect_event(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<String>,
    Json(req): Json<ProtectReq>,
) -> Response {
    // (Too many clips waiting to be saved: nothing changes; try again later.)
    if req.protect && engine.clip_saves_full() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "id": id, "error": "too many kept clips are waiting to be saved: try again in a few minutes" }))).into_response();
    }
    let ok = engine.set_protect(&id, req.protect);
    Json(serde_json::json!({ "id": id, "protect": req.protect, "found": ok })).into_response()
}

#[derive(serde::Deserialize)]
struct ReviewReq {
    /// "false_alarm" | "ok"; null clears the verdict.
    #[serde(default)]
    verdict: Option<String>,
    /// With "false_alarm": "not a <label>" ("person", "car" ...): the boxes
    /// of that kind that sat still in the event are remembered as not real.
    #[serde(default)]
    label: Option<String>,
}

/// `POST /api/events/:id/review {"verdict": "false_alarm"|"ok"|null, "label"?: "person"}`.
/// Like protecting, a change: admins only (the sign-in layer). The camera
/// check for limited callers is done before this handler runs, by the
/// `camera_allowed` middleware (`events/<id>/...` paths: the id names its camera).
/// "false_alarm" with a label also remembers the event's boxes of that kind
/// that sat still as not real on its camera (engine::spots): the same thing
/// in the same spot does not alert again for 30 days, unless it moves.
/// Without a label nothing is remembered. Any other verdict takes it back.
async fn review_event(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    role: Option<axum::Extension<Role>>,
    req_parts: axum::http::request::Parts,
    AxPath(id): AxPath<String>,
    Json(req): Json<ReviewReq>,
) -> Response {
    let verdict = req.verdict.as_deref().map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty());
    if verdict.as_deref().is_some_and(|v| v != "false_alarm" && v != "ok") {
        return (StatusCode::BAD_REQUEST, "verdict must be false_alarm, ok or null").into_response();
    }
    let label = req.label.as_deref().map(|l| l.trim().to_ascii_lowercase()).filter(|l| !l.is_empty());
    if label.as_deref().is_some_and(|l| l.len() > 40 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '_')) {
        return (StatusCode::BAD_REQUEST, "label must be an object name, like person or car").into_response();
    }
    let ip = req_parts.extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string()).unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    let e = engine.clone();
    let (id2, v2) = (id.clone(), verdict.clone());
    let found = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Option<crate::engine::spots::Marked>>> {
        if !e.db.set_event_review(&id2, v2.as_deref(), now)? {
            return Ok(None);
        }
        // (Not a reason to fail the verdict: it is saved.)
        // Only "not a <label>" remembers anything: a plain "not a real
        // alert" says nothing about which box was wrong.
        let marked = match (v2.as_deref(), label.as_deref()) {
            (Some("false_alarm"), Some(l)) => e.mark_not_real(&id2, l).unwrap_or_else(|x| {
                tracing::warn!("could not remember the spots of {id2}: {x:#}");
                None
            }),
            (Some("false_alarm"), None) => None,
            _ => {
                let _ = e.unmark_not_real(&id2);
                None
            }
        };
        if let Some(m) = marked.as_ref().filter(|m| m.spots > 0) {
            let _ = e.db.insert_audit(&caller.0, &ip, &format!("marked not a {}: {} spot{} remembered for 30 days", m.label, m.spots, if m.spots == 1 { "" } else { "s" }), &format!("{} ({id2})", m.camera), 200);
            // (An operator muting a spot: admins hear of it.)
            if role.as_ref().is_some_and(|r| r.0.is_operator()) {
                tell_operator_change(&e, &caller.0, &m.camera, &format!("marked an event of {} \"not a {}\" ({id2}): a {} in the same spot{} will not alert for 30 days unless it moves", m.camera, m.label, m.label, if m.spots == 1 { "" } else { "s" }));
            }
        }
        Ok(Some(marked))
    })
    .await;
    match found {
        Ok(Ok(Some(marked))) => Json(serde_json::json!({ "id": id, "review": verdict, "reviewed_at": verdict.as_ref().map(|_| now), "found": true, "marked": marked })).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "id": id, "found": false }))).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "could not save the review").into_response(),
    }
}

#[derive(serde::Serialize)]
struct ClipEntry {
    id: String,
    camera: String,
    start: i64,
    size: u64,
}

async fn list_clips(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>) -> Json<serde_json::Value> {
    // Every event with video: a saved file, or recordings it plays from.
    let q = crate::db::EventQuery { limit: 500, ..Default::default() };
    let events = engine.db.query_events(&q).unwrap_or_default();
    let cfg = engine.config.lock().unwrap().clone();
    let mut clips = Vec::new();
    for ev in events.into_iter().filter(|e| e.clip.is_some() && access.can_name(&e.camera)) {
        let m = cfg.motion_for(&ev.camera);
        let (pre, post) = (m.pre_roll as i64, m.post_roll as i64);
        let file = engine.clip_file_path(&ev.id);
        let size = match std::fs::metadata(&file) {
            Ok(m) => m.len(),
            // Estimate from the recordings covering the event.
            Err(_) => engine
                .db
                .segments_overlapping(&crate::config::slug(&ev.camera), (ev.start - pre) * 1000, (ev.end + post) * 1000)
                .map(|v| {
                    // Each overlapping segment contributes the share of its
                    // bytes that falls inside the clip window.
                    let (from, to) = ((ev.start - pre) * 1000, (ev.end + post) * 1000);
                    v.iter()
                        .map(|r| {
                            let span = (r.end_ms - r.start_ms).max(1);
                            let inside = (r.end_ms.min(to) - r.start_ms.max(from)).max(0);
                            (r.bytes.max(0) as u64) * inside as u64 / span as u64
                        })
                        .sum()
                })
                .unwrap_or(0),
        };
        clips.push(ClipEntry { id: ev.id, camera: ev.camera, start: ev.start, size });
    }
    clips.sort_by(|a, b| b.start.cmp(&a.start));
    Json(serde_json::json!({ "clips": clips }))
}

async fn clip_file(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<String>,
    Query(dl): Query<DownloadQ>,
    caller: Option<axum::Extension<Caller>>,
    req: Request,
) -> Response {
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return (StatusCode::BAD_REQUEST, "bad clip id").into_response();
    }
    let path = engine.clip_file_path(&id);
    let download = dl.download == "1" || dl.download == "true";
    let caps = match caps_of(&dl.caps, download) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if caps.is_some() || (dl.info == "1" && !download) {
        // What the player decodes: untouched when the clip fits, else
        // transcoded (from `start` ms into it). info=1: what, as JSON.
        let src = crate::transcode::http::ClipSrc::Event { id: id.clone(), offset: dl.start.unwrap_or(0).max(0) };
        let user = caller.map(|c| c.0 .0.clone()).unwrap_or_default();
        let ask = ask_of(&req);
        return match crate::transcode::http::handle(engine.clone(), src, caps, dl.info == "1", ask, dl.quality == "low", user).await {
            crate::transcode::http::Outcome::Done(r) => r,
            crate::transcode::http::Outcome::Untouched { low, headers } => {
                let r = if low || !path.is_file() {
                    virtual_clip_response(engine, id, false, low, req).await
                } else {
                    match tower::ServiceExt::oneshot(ServeFile::new(path), req).await {
                        Ok(r) => {
                            let mut r = r.into_response();
                            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
                            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                            r
                        }
                        Err(_) => (StatusCode::NOT_FOUND, "clip not found").into_response(),
                    }
                };
                with_headers(r, headers)
            }
        };
    }
    if !path.is_file() {
        // No saved file: assemble the event's video from the recordings.
        return virtual_clip_response(engine, id, download, dl.quality == "low", req).await;
    }
    // ServeFile streams from disk and handles Range / If-Range / HEAD.
    let mut resp = match tower::ServiceExt::oneshot(ServeFile::new(path), req).await {
        Ok(r) => r.into_response(),
        Err(_) => return (StatusCode::NOT_FOUND, "clip not found").into_response(),
    };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if dl.download == "1" || dl.download == "true" {
        // "Save" button in the dashboard: force a file download.
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}.mp4\"", id)) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    resp
}

/// Parse a single `Range: bytes=...` header into an inclusive (start, end).
/// Err(()) = unsatisfiable.
fn parse_range(h: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = h.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multiple ranges: answer with the whole body
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let r = if a.is_empty() {
        // Suffix: the last N bytes.
        let n: u64 = b.parse().ok()?;
        if n == 0 || len == 0 {
            return Some(Err(()));
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        if start >= len {
            return Some(Err(()));
        }
        // Open-ended: everything to the end (streamed in pieces as it is
        // read). Capping it made Android's player (ExoPlayer) stop at the
        // cap: it takes the answer's length as the rest of the file.
        let end = if b.is_empty() {
            len - 1
        } else {
            b.parse::<u64>().ok()?.min(len - 1)
        };
        if end < start {
            return Some(Err(()));
        }
        (start, end)
    };
    Some(Ok(r))
}

async fn virtual_clip_response(engine: Arc<Engine>, id: String, download: bool, small: bool, req: Request) -> Response {
    let built = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<(crate::ingest::splice::VirtualClip, String)>> {
        let Some(ev) = engine.db.get_event(&id)? else { return Ok(None) };
        Ok(engine.virtual_clip_q(&ev, small)?.map(|c| (c, id)))
    })
    .await;
    match built {
        Ok(Ok(Some((clip, id)))) => serve_clip(clip, format!("{id}.mp4"), Vec::new(), download, req).await,
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "clip not found").into_response(),
        Ok(Err(e)) => internal_error("clip", &e),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "clip error").into_response(),
    }
}

/// Serve an assembled clip with Range/206 and HEAD support.
async fn serve_clip(
    clip: crate::ingest::splice::VirtualClip,
    filename: String,
    extra_headers: Vec<(&'static str, String)>,
    download: bool,
    req: Request,
) -> Response {
    let range = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let head = req.method() == axum::http::Method::HEAD;
    let len = clip.len();
    let mut b = Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ACCEPT_RANGES, "bytes");
    if download {
        b = b.header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""));
    }
    for (k, v) in extra_headers {
        b = b.header(k, v);
    }
    let (status, start, end) = match range.as_deref().and_then(|r| parse_range(r, len)) {
        Some(Err(())) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap();
        }
        Some(Ok((s, e))) => {
            b = b.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{len}"));
            (StatusCode::PARTIAL_CONTENT, s, e + 1)
        }
        None => (StatusCode::OK, 0, len),
    };
    b = b.status(status).header(header::CONTENT_LENGTH, end - start);
    if head {
        return b.body(Body::empty()).unwrap();
    }
    // Streamed in small pieces as they are read: the player gets the first
    // bytes at once instead of after the whole range (up to 8 MB of 4K video
    // from a hard disk took seconds) has been read.
    const PIECE: u64 = 256 * 1024;
    let clip = Arc::new(clip);
    let first = {
        let c = clip.clone();
        let to = (start + PIECE).min(end);
        match tokio::task::spawn_blocking(move || c.read_range(start, to)).await {
            Ok(Ok(bytes)) => (bytes, to),
            _ => return (StatusCode::INTERNAL_SERVER_ERROR, "could not read recordings").into_response(),
        }
    };
    let rest = futures::stream::unfold((clip, first.1), move |(clip, pos)| async move {
        if pos >= end {
            return None;
        }
        let to = (pos + PIECE).min(end);
        let c = clip.clone();
        match tokio::task::spawn_blocking(move || c.read_range(pos, to)).await {
            Ok(Ok(bytes)) => Some((Ok::<_, std::io::Error>(bytes), (clip, to))),
            Ok(Err(e)) => Some((Err(e), (clip, end))),
            Err(_) => Some((Err(std::io::Error::other("read failed")), (clip, end))),
        }
    });
    let body = futures::StreamExt::chain(futures::stream::once(async move { Ok::<_, std::io::Error>(first.0) }), rest);
    b.body(Body::from_stream(body)).unwrap()
}

#[derive(serde::Deserialize)]
struct RecordingsQ {
    camera: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
}

async fn recordings(
    State(engine): State<Arc<Engine>>,
    axum::Extension(access): axum::Extension<Access>,
    Query(q): Query<RecordingsQ>,
) -> Response {
    let (from, to) = (q.from.unwrap_or(0), q.to.unwrap_or(i64::MAX));
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        match q.camera {
            Some(name) => Ok(serde_json::to_value(engine.recording_coverage(&name, from, to)?)?),
            None => {
                let cams = engine
                    .camera_names()
                    .iter()
                    .filter(|n| access.can_name(n))
                    .map(|n| engine.recording_coverage(n, from, to))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Ok(serde_json::json!({ "cameras": cams }))
            }
        }
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal_error("recordings", &e),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "recordings error").into_response(),
    }
}

#[derive(serde::Deserialize)]
struct DaysQ {
    /// First millisecond of each day to look at, comma separated (the caller's own days).
    #[serde(default)]
    starts: String,
    /// End of the last day (epoch ms).
    end: i64,
    /// Camera names, comma separated; none: every camera the caller may see.
    #[serde(default)]
    cameras: String,
}

/// `GET /api/recordings/days?starts=ms,ms,..&end=ms[&cameras=A,B]` → which of the
/// caller's days hold recordings and alerts, for a calendar:
/// `{days: [{start, rec, alerts}], oldest}`.
async fn recording_days(
    State(engine): State<Arc<Engine>>,
    axum::Extension(access): axum::Extension<Access>,
    Query(q): Query<DaysQ>,
) -> Response {
    const DAY: i64 = 86_400_000;
    let bad = |m: &'static str| (StatusCode::BAD_REQUEST, m).into_response();
    if q.starts.len() > 62 * 16 || q.cameras.len() > 8192 {
        return bad("too long");
    }
    let parts: Vec<&str> = q.starts.split(',').filter(|s| !s.is_empty()).collect();
    let starts: Vec<i64> = parts.iter().filter_map(|s| s.trim().parse::<i64>().ok()).collect();
    if starts.is_empty() || starts.len() > 62 || starts.len() != parts.len() {
        return bad("starts: 1 to 62 millisecond times");
    }
    let limit = chrono::Utc::now().timestamp_millis() + 3 * DAY;
    if starts[0] < 0 || starts[starts.len() - 1] > limit || starts.windows(2).any(|w| w[1] <= w[0] || w[1] - w[0] > 2 * DAY) {
        return bad("starts: rising times, a day or so apart");
    }
    let last = starts[starts.len() - 1];
    if q.end <= last || q.end - last > 2 * DAY {
        return bad("end: after the last start, within two days");
    }
    let wanted: Vec<String> = q.cameras.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty() && s.len() <= 200).take(200).collect();
    let names: Vec<(String, String)> = engine
        .camera_names()
        .into_iter()
        .filter(|n| access.can_name(n) && (wanted.is_empty() || wanted.contains(&n.to_lowercase())))
        .map(|n| (crate::config::slug(&n), n))
        .collect();
    let quiet = engine.hide_unconfirmed_cameras();
    let end = q.end;
    let res = tokio::task::spawn_blocking(move || engine.db.recording_days(&starts, end, &names, &quiet).map(|(days, oldest)| (starts, days, oldest))).await;
    match res {
        Ok(Ok((starts, days, oldest))) => Json(serde_json::json!({
            "days": starts.iter().zip(days).map(|(s, (rec, alerts))| serde_json::json!({ "start": s, "rec": rec, "alerts": alerts })).collect::<Vec<_>>(),
            "oldest": oldest,
        }))
        .into_response(),
        Ok(Err(e)) => internal_error("recording days", &e),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "recording days error").into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PlaybackQ {
    camera: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    at: Option<i64>,
    /// Fast playback: keyframes only, retimed to play this many times faster.
    speed: Option<u32>,
    /// Fast playback: ms each picture shows (100 = 10 a second, the
    /// default; more = fewer pictures, for many cameras at once).
    frame_ms: Option<u32>,
    /// "low": the sub stream wherever it was recorded (data saver).
    #[serde(default)]
    quality: String,
    #[serde(default)]
    download: String,
    /// What the player decodes (see transcode/): transcoded when the
    /// recording does not fit. Not with download, speed or reverse.
    #[serde(default)]
    caps: Option<String>,
    /// Play from this moment (ms; within from..to; a value below `from` is
    /// taken as ms after it).
    #[serde(default)]
    start: Option<i64>,
    /// With caps, "1": what would be sent, as JSON.
    #[serde(default)]
    info: String,
    /// "1": the window played backwards (at most 10 s; see Engine::reverse_clip).
    #[serde(default)]
    reverse: String,
    /// "1": nothing recorded is answered 204 with x-none: 1 instead of 404.
    #[serde(default)]
    quiet: String,
}

/// `GET /api/sound/status` (Settings > Smart features > Sounds): sound names on/off, the
/// model download, the names there are.
async fn sound_status(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(engine.sound_status())
}

/// `GET /api/cameras/:id/sound` (admin): the camera's live sound level and
/// background (dBFS), for tuning its sound trigger. Asking keeps the sound
/// measured for 10 s even while the trigger is off.
async fn sound_meter(
    State(engine): State<Arc<Engine>>,
    axum::Extension(admin): axum::Extension<IsAdmin>,
    AxPath(id): AxPath<usize>,
) -> Response {
    if !admin.0 {
        return (StatusCode::FORBIDDEN, "admin access required").into_response();
    }
    match engine.sound_meter(id) {
        Some(v) => Json(v).into_response(),
        None => (StatusCode::NOT_FOUND, "camera not found").into_response(),
    }
}

async fn smart_status(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(engine.smart_status())
}

#[derive(serde::Deserialize)]
struct SmartQ {
    q: String,
    #[serde(default)]
    camera: String,
    /// Seconds since the epoch, like the events list.
    since: Option<i64>,
    until: Option<i64>,
    limit: Option<usize>,
    /// "plain": rank by likeness alone (for comparing; see Engine::smart_search).
    #[serde(default)]
    mode: String,
    /// Results must stand out at least this far (for tuning).
    min_z: Option<f32>,
}

/// `GET /api/smart/search?q=red truck&camera=&since=&until=`: events whose
/// pictures look like the description, best first.
async fn smart_search(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>, Query(q): Query<SmartQ>) -> Response {
    let text = q.q.trim().to_string();
    if text.is_empty() || text.chars().count() > 200 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "describe what to look for (up to 200 letters)" }))).into_response();
    }
    let from = q.since.unwrap_or(0).saturating_mul(1000);
    let to = q.until.map_or(i64::MAX, |u| u.saturating_mul(1000) + 999);
    let limit = q.limit.unwrap_or(60).clamp(1, 300);
    let t0 = std::time::Instant::now();
    // A user limited to some cameras finds only those.
    let allowed = move |cam: &str| access.can_name(cam);
    match tokio::task::spawn_blocking(move || engine.smart_search(&text, &q.camera, from, to, limit, q.mode == "plain", q.min_z, &allowed)).await {
        Ok(Ok(hits)) => Json(serde_json::json!({ "results": hits, "ms": t0.elapsed().as_millis() as u64 })).into_response(),
        Ok(Err(e)) => {
            tracing::warn!("smart search: {e:#}");
            (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": "smart search failed (details are in the server log)" }))).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "search error").into_response(),
    }
}

async fn smart_pic(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>, AxPath(id): AxPath<i64>) -> Response {
    match tokio::task::spawn_blocking(move || engine.smart_pic(id, &|cam| access.can_name(cam))).await {
        Ok(Ok(Some(b))) => Response::builder().header(header::CONTENT_TYPE, "image/jpeg").header(header::CACHE_CONTROL, "private, max-age=3600").body(Body::from(b)).unwrap(),
        _ => (StatusCode::NOT_FOUND, "no picture").into_response(),
    }
}

/// Nothing recorded for what was asked. With `quiet=1` the page's own requests get 204 and an
/// `x-none: 1` header (a browser logs a 404 as a red error in its console; the answer is the same);
/// everyone else keeps the plain 404.
fn nothing_recorded(quiet: &str, why: &'static str) -> Response {
    if quiet == "1" {
        Response::builder().status(StatusCode::NO_CONTENT).header("x-none", "1").body(Body::empty()).unwrap()
    } else {
        (StatusCode::NOT_FOUND, why).into_response()
    }
}

#[derive(serde::Deserialize)]
struct PictureQ {
    camera: String,
    at: i64,
    /// "low": the sub stream where it was recorded.
    #[serde(default)]
    quality: String,
    /// The picture the browser shows now (its x-pic-ms): the same again
    /// comes back as 204, no body.
    have: Option<i64>,
    /// "1": nothing recorded is answered 204 with x-none: 1 instead of 404.
    #[serde(default)]
    quiet: String,
}

/// `GET /api/playback/picture?camera=&at=<ms>`: the keyframe on screen at
/// that moment, by itself, for fast playback: the browser decodes and shows
/// each as it arrives and asks for the next moment at once, so a slow disk
/// means fewer pictures, never a blank one. Body: the picture as stored
/// (length-prefixed NAL units); x-pic-ms (when it was recorded), x-codec
/// (RFC 6381), x-desc (the avcC/hvcC record, base64).
async fn playback_picture(State(engine): State<Arc<Engine>>, Query(q): Query<PictureQ>) -> Response {
    // (A moment before 1970 or after the year 3000 is a mistake, not a
    // recording: refused before any arithmetic on it.)
    if !(0..=32_503_680_000_000).contains(&q.at) {
        return (StatusCode::BAD_REQUEST, "bad time").into_response();
    }
    crate::engine::playback::playback_now();
    let low = q.quality == "low";
    let cam = q.camera.clone();
    let quiet = q.quiet.clone();
    let r = tokio::task::spawn_blocking(move || engine.picture_at(&cam, q.at, low)).await;
    match r {
        Ok(Ok(Some(k))) => {
            if q.have == Some(k.wall_ms) {
                return StatusCode::NO_CONTENT.into_response();
            }
            use base64::Engine as _;
            Response::builder()
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CACHE_CONTROL, "no-store")
                .header("x-pic-ms", k.wall_ms.to_string())
                .header("x-codec", k.codec)
                .header("x-desc", base64::engine::general_purpose::STANDARD.encode(&k.desc))
                .header("x-pic-sub", (k.sub as u8).to_string())
                // The size as recorded: some browsers' H.265 decoders report a
                // wrong one (the page then crops to this).
                .header("x-pic-size", k.size.map(|(w, h)| format!("{w}x{h}")).unwrap_or_default())
                .body(Body::from(k.data))
                .unwrap()
        }
        Ok(Ok(None)) => nothing_recorded(&quiet, "no recording"),
        Ok(Err(e)) => {
            tracing::warn!("picture: {}: {e:#}", q.camera);
            (StatusCode::INTERNAL_SERVER_ERROR, "picture error").into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "picture error").into_response(),
    }
}

async fn playback(State(engine): State<Arc<Engine>>, Query(q): Query<PlaybackQ>, caller: Option<axum::Extension<Caller>>, req: Request) -> Response {
    let (Some(camera), Some(from), Some(to)) = (q.camera, q.from, q.to) else {
        return (StatusCode::BAD_REQUEST, "camera, from and to are required").into_response();
    };
    if from < 0 || to <= from {
        return (StatusCode::BAD_REQUEST, "bad time window").into_response();
    }
    let reverse = q.reverse == "1";
    crate::engine::playback::playback_now();
    let trick = q.speed.filter(|s| *s >= 2 && !reverse).map(|s| s.min(1024));
    let to = to.min(from + if trick.is_some() { crate::engine::playback::MAX_TRICK_WINDOW_MS } else { crate::engine::playback::MAX_WINDOW_MS });
    let download = q.download == "1" || q.download == "true";
    let mut low = q.quality == "low";
    let caps = match caps_of(&q.caps, download || trick.is_some() || reverse) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let mut caps_headers = Vec::new();
    let mut from = from;
    if caps.is_some() || (q.info == "1" && !(download || trick.is_some() || reverse)) {
        // `start`: a moment (epoch ms) within the window; a value no
        // larger than the window's length is ms after its start; an
        // earlier moment is the window's start.
        let start = match q.start {
            Some(s) if s >= from => s.min(to - 1),
            Some(s) if s > 0 && s <= to - from => (from + s).min(to - 1),
            _ => from,
        };
        let src = crate::transcode::http::ClipSrc::Window { camera: camera.clone(), from, to, start };
        let user = caller.map(|c| c.0 .0.clone()).unwrap_or_default();
        let ask = ask_of(&req);
        match crate::transcode::http::handle(engine.clone(), src, caps, q.info == "1", ask, low, user).await {
            crate::transcode::http::Outcome::Done(r) => return r,
            crate::transcode::http::Outcome::Untouched { low: l, headers } => {
                low = l;
                caps_headers = headers;
                from = start;
            }
        }
    }
    let low = low;
    let from = from;
    let cam = camera.clone();
    let frame_ms = q.frame_ms.unwrap_or(crate::engine::playback::TRICK_FRAME_MS).clamp(100, 1000);
    let t0 = std::time::Instant::now();
    // Set when this request is dropped (the browser went on to another
    // moment): a fast clip still being planned stops, freeing the disk, and
    // a backwards clip not yet made is not made (ffmpeg is stopped).
    struct Gone(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Gone {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let gone = Gone(Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let flag = gone.0.clone();
    let built = tokio::task::spawn_blocking(move || match trick {
        _ if reverse => engine.reverse_clip(&cam, from, to, &|| flag.load(std::sync::atomic::Ordering::Relaxed)),
        Some(sp) => engine.trick_clip(&cam, from, to, sp, frame_ms, low, &|| flag.load(std::sync::atomic::Ordering::Relaxed)),
        None => engine.playback_clip(&cam, from, to, low),
    })
    .await;
    if t0.elapsed() > std::time::Duration::from_millis(500) {
        tracing::info!("playback: {camera}: window {from} took {} ms to prepare{}", t0.elapsed().as_millis(), if low { " (sub stream)" } else { "" });
    }
    match built {
        Ok(Ok(Some(clip))) => {
            // A download (the timeline's export): exactly the stretch asked
            // for, with the camera and time as captions.
            let clip = if download && trick.is_none() && !reverse {
                let clock = |ms: i64| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default()
                };
                match clip.clone().for_download(Some((from, to)), &camera, &clock) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("export: {camera}: {e:#}; sending the plain clip");
                        clip
                    }
                }
            } else {
                clip
            };
            let mut extra = vec![
                ("x-clip-start-ms", clip.start_ms.to_string()),
                ("x-clip-end-ms", clip.end_ms.to_string()),
                ("x-clip-sub", (clip.sub as u8).to_string()),
                ("x-clip-reverse", (reverse as u8).to_string()),
                // Fast clips: how many pictures (the page works out how big
                // and how far apart its keyframes are).
                ("x-clip-frames", clip.frame_times.len().to_string()),
            ];
            extra.append(&mut caps_headers);
            let name = format!("{}-{from}.mp4", crate::config::slug(&camera));
            serve_clip(clip, name, extra, download, req).await
        }
        Ok(Ok(None)) => nothing_recorded(&q.quiet, "no recording covers that window"),
        Ok(Err(e)) if e.to_string() == "busy" => (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "1")], "busy, try again").into_response(),
        Ok(Err(e)) => internal_error("playback", &e),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "playback error").into_response(),
    }
}

#[derive(serde::Deserialize)]
struct SignalsQ {
    from: Option<i64>,
    to: Option<i64>,
}

/// `GET /api/signals?from=&to=` (ms): every signal with the spans it was on
/// in that window: `{signals: [{name, on, spans: [[from, to], ...]}]}`.
async fn list_signals(State(engine): State<Arc<Engine>>, Query(q): Query<SignalsQ>) -> Response {
    let now = chrono::Utc::now().timestamp_millis();
    let to = q.to.unwrap_or(now);
    let from = q.from.unwrap_or(to - 86_400_000);
    if to < from || to - from > 31 * 86_400_000 {
        return (StatusCode::BAD_REQUEST, "bad time window").into_response();
    }
    match tokio::task::spawn_blocking(move || engine.db.signal_spans(from, to, now)).await {
        Ok(Ok(v)) => Json(serde_json::json!({
            "signals": v.into_iter().map(|(name, on, spans)| serde_json::json!({ "name": name, "on": on, "spans": spans })).collect::<Vec<_>>(),
        }))
        .into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "signals error").into_response(),
    }
}

#[derive(serde::Deserialize)]
struct SignalSet {
    name: String,
    /// true/false, or "on"/"off", "open"/"closed", 1/0.
    state: serde_json::Value,
}

/// `POST /api/signals {name, state}`: a signal (a door, an alarm) is now on
/// or off; the timeline shows it as a lane. Any signed-in account may.
async fn set_signal(State(engine): State<Arc<Engine>>, Json(b): Json<SignalSet>) -> Response {
    let on = match &b.state {
        serde_json::Value::Bool(v) => Some(*v),
        serde_json::Value::Number(n) => n.as_f64().map(|x| x != 0.0),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "1" | "open" | "active" | "yes" => Some(true),
            "off" | "false" | "0" | "closed" | "inactive" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    };
    let Some(on) = on else { return (StatusCode::BAD_REQUEST, "state should be on or off").into_response() };
    if engine.set_signal(&b.name, on, "api") {
        Json(serde_json::json!({ "name": b.name.trim(), "on": on })).into_response()
    } else {
        (StatusCode::BAD_REQUEST, "give a name of 1 to 60 characters").into_response()
    }
}

/// `DELETE /api/signals/:name`: remove a signal and its history (admin).
async fn forget_signal(State(engine): State<Arc<Engine>>, AxPath(name): AxPath<String>) -> Response {
    match engine.db.forget_signal(&name) {
        Ok(true) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such signal").into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "signals error").into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PreviewQ {
    camera: Option<String>,
    at: Option<i64>,
    w: Option<u32>,
}

/// A picture that could not be made: "busy" is a short wait (503); other
/// failures are logged here, not shown to the viewer (paths, ffmpeg output).
/// An internal failure: the details go to the server log (they can hold
/// file paths and camera addresses), the client gets a plain message.
pub(crate) fn internal_error(what: &str, e: &anyhow::Error) -> Response {
    tracing::warn!("{what}: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{what} failed (details are in the server log)")).into_response()
}

fn picture_error(what: &str, camera: &str, e: anyhow::Error) -> Response {
    if e.to_string() == "busy" {
        return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "2")], "busy, try again").into_response();
    }
    tracing::warn!("{what}: {camera}: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, "could not make the picture").into_response()
}

#[derive(serde::Deserialize)]
struct FilmstripQ {
    camera: Option<String>,
    from: Option<i64>,
    step: Option<i64>,
    n: Option<u32>,
    w: Option<u32>,
    /// "1": nothing recorded is answered 204 with x-none: 1 instead of 404.
    #[serde(default)]
    quiet: String,
}

/// `GET /api/filmstrip?camera=&from=<ms>&step=<ms>&n=&w=`: `n` small
/// pictures of the recording from `from`, one every `step`, in one JPEG grid.
/// Headers: x-sprite-cols, x-sprite-w, x-sprite-h (cell size) and
/// x-sprite-times (each picture's time, ms, comma separated).
async fn filmstrip(State(engine): State<Arc<Engine>>, Query(q): Query<FilmstripQ>) -> Response {
    let (Some(camera), Some(from), Some(step)) = (q.camera.filter(|c| !c.is_empty()), q.from, q.step) else {
        return (StatusCode::BAD_REQUEST, "camera, from and step are required").into_response();
    };
    if from < 0 || step <= 0 {
        return (StatusCode::BAD_REQUEST, "bad time").into_response();
    }
    let (n, w) = (q.n.unwrap_or(60), q.w.unwrap_or(160));
    let t0 = std::time::Instant::now();
    let cam = camera.clone();
    let r = tokio::task::spawn_blocking(move || engine.filmstrip(&camera, from, step, n, w)).await;
    if t0.elapsed() > std::time::Duration::from_millis(1000) {
        tracing::info!("filmstrip: {cam}: {n} pictures took {} ms", t0.elapsed().as_millis());
    }
    match r {
        Ok(Ok(Some(f))) => {
            let times = f.times.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(",");
            Response::builder()
                .header(header::CONTENT_TYPE, "image/jpeg")
                .header(header::CACHE_CONTROL, "private, max-age=60")
                .header("x-sprite-cols", f.cols.to_string())
                .header("x-sprite-w", f.w.to_string())
                .header("x-sprite-h", f.h.to_string())
                .header("x-sprite-times", times)
                .body(Body::from(f.jpeg))
                .unwrap()
        }
        Ok(Ok(None)) => nothing_recorded(&q.quiet, "no recording in that window"),
        Ok(Err(e)) => picture_error("filmstrip", &cam, e),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "filmstrip error").into_response(),
    }
}

/// `GET /api/preview?camera=&at=<ms>&w=<px>`: a small picture of the
/// recording at (the keyframe before) that moment, for timeline hovering.
/// `x-preview-ms` is the keyframe's time.
async fn preview(State(engine): State<Arc<Engine>>, Query(q): Query<PreviewQ>) -> Response {
    let (Some(camera), Some(at)) = (q.camera.filter(|c| !c.is_empty()), q.at) else {
        return (StatusCode::BAD_REQUEST, "camera and at are required").into_response();
    };
    if at < 0 {
        return (StatusCode::BAD_REQUEST, "bad time").into_response();
    }
    let w = q.w.unwrap_or(320);
    let cam_name = camera.clone();
    match tokio::task::spawn_blocking(move || engine.preview_at(&camera, at, w)).await {
        Ok(Ok(Some((jpeg, t)))) => Response::builder()
            .header(header::CONTENT_TYPE, "image/jpeg")
            // Recordings do not change: let the browser keep it.
            .header(header::CACHE_CONTROL, "private, max-age=3600")
            .header("x-preview-ms", t.to_string())
            .body(Body::from(jpeg))
            .unwrap(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "no recording at that time").into_response(),
        Ok(Err(e)) => picture_error("preview", &cam_name, e),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "preview error").into_response(),
    }
}

/// `GET /api/timelapse`: the time-lapses there are (of cameras the caller
/// may see), newest first, and the settings that matter for the page.
async fn timelapse_list(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>) -> Json<serde_json::Value> {
    let (names, tl) = {
        let c = engine.config.lock().unwrap();
        (c.cameras.iter().map(|x| (crate::config::slug(&x.name), x.name.clone())).collect::<Vec<_>>(), c.timelapse.clone())
    };
    let list: Vec<serde_json::Value> = engine
        .timelapses()
        .into_iter()
        .filter(|(s, ..)| access.can_slug(s))
        .map(|(s, d, bytes, making)| {
            let name = names.iter().find(|(x, _)| *x == s).map(|(_, n)| n.clone()).unwrap_or_else(|| s.clone());
            serde_json::json!({ "slug": s, "camera": name, "date": d.to_string(), "bytes": bytes, "making": making })
        })
        .collect();
    Json(serde_json::json!({ "timelapses": list, "enabled": tl.enabled, "seconds": tl.seconds, "hour": tl.hour }))
}

#[derive(serde::Deserialize)]
struct TimelapseMake {
    camera: String,
    date: String,
}

/// `POST /api/timelapse {camera, date}` (admin): make that day's now, in the
/// background (today's covers the day so far).
async fn timelapse_make(State(engine): State<Arc<Engine>>, Json(b): Json<TimelapseMake>) -> Response {
    let Ok(date) = chrono::NaiveDate::parse_from_str(&b.date, "%Y-%m-%d") else {
        return (StatusCode::BAD_REQUEST, "date must be YYYY-MM-DD").into_response();
    };
    if !engine.camera_names().contains(&b.camera) {
        return (StatusCode::NOT_FOUND, "camera not found").into_response();
    }
    let started = engine.start_timelapse(b.camera, date);
    Json(serde_json::json!({ "started": started })).into_response()
}

/// `GET /api/timelapse/:slug/:date` -> the MP4 (Range supported).
async fn timelapse_file(
    State(engine): State<Arc<Engine>>,
    AxPath((cam, date)): AxPath<(String, String)>,
    Query(dl): Query<DownloadQ>,
    req: Request,
) -> Response {
    let Ok(d) = chrono::NaiveDate::parse_from_str(date.trim_end_matches(".mp4"), "%Y-%m-%d") else {
        return (StatusCode::BAD_REQUEST, "bad date").into_response();
    };
    if cam.is_empty() || !cam.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return (StatusCode::BAD_REQUEST, "bad camera").into_response();
    }
    let path = engine.timelapse_path(&cam, d);
    if !path.is_file() {
        return (StatusCode::NOT_FOUND, "no time-lapse for that day").into_response();
    }
    let mut resp = match tower::ServiceExt::oneshot(ServeFile::new(path), req).await {
        Ok(r) => r.into_response(),
        Err(_) => return (StatusCode::NOT_FOUND, "no time-lapse for that day").into_response(),
    };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if dl.download == "1" || dl.download == "true" {
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{cam}-{d}-timelapse.mp4\"")) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    resp
}

async fn still(State(engine): State<Arc<Engine>>, Query(q): Query<PlaybackQ>) -> Response {
    let (Some(camera), Some(at)) = (q.camera, q.at) else {
        return (StatusCode::BAD_REQUEST, "camera and at are required").into_response();
    };
    if at < 0 {
        return (StatusCode::BAD_REQUEST, "bad time").into_response();
    }
    let download = q.download == "1" || q.download == "true";
    let cam = camera.clone();
    match tokio::task::spawn_blocking(move || engine.still_at(&cam, at)).await {
        Ok(Ok(Some(jpeg))) => {
            let mut b = Response::builder()
                .header(header::CONTENT_TYPE, "image/jpeg")
                .header(header::CACHE_CONTROL, "no-cache");
            if download {
                b = b.header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}-{at}.jpg\"", crate::config::slug(&camera)),
                );
            }
            b.body(Body::from(jpeg)).unwrap()
        }
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "no recording at that time").into_response(),
        Ok(Err(e)) => internal_error("picture", &e),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "still error").into_response(),
    }
}

/// `POST /api/cameras/:id/wake` — wake an on-demand (battery) camera for its
/// idle timeout (any signed-in user who can see the camera).
async fn wake_camera(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    if engine.wake_camera(id) {
        let st = engine.camera(id).map(|h| (h.state.read().unwrap().sleeping, h.state.read().unwrap().wake_until));
        Json(serde_json::json!({ "woken": true, "sleeping": st.map(|s| s.0), "awake_until": st.map(|s| s.1) })).into_response()
    } else {
        (StatusCode::NOT_FOUND, "camera not found").into_response()
    }
}

pub(crate) async fn snapshot(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    if crate::remote::is_remote(id) {
        return crate::remote::proxy_get(&engine, id, "/link/snapshot/{id}", "image/jpeg").await;
    }
    // Asking for a picture of a battery camera wakes it.
    if engine.camera(id).is_some_and(|h| h.running.read().unwrap().on_demand) {
        engine.wake_camera(id);
    }
    let handle = match engine.camera(id) {
        Some(h) => h,
        None => return (StatusCode::NOT_FOUND, "camera not found").into_response(),
    };
    let frame = handle.state.read().unwrap().latest_frame.clone();
    if frame.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "no frame captured yet").into_response();
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(frame))
        .unwrap()
}

async fn live(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    // Another server's camera: one current picture (its live view uses the sockets).
    if crate::remote::is_remote(id) {
        return crate::remote::proxy_get(&engine, id, "/link/snapshot/{id}", "image/jpeg").await;
    }
    if engine.camera(id).is_some_and(|h| h.running.read().unwrap().on_demand) {
        engine.wake_camera(id);
    }
    let handle = match engine.camera(id) {
        Some(h) => h,
        None => return (StatusCode::NOT_FOUND, "camera not found").into_response(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
    tokio::spawn(async move {
        let mut last: u64 = 0;
        loop {
            let (seq, frame) = {
                let st = handle.state.read().unwrap();
                (st.frame_seq, st.latest_frame.clone())
            };
            if seq != last && !frame.is_empty() {
                last = seq;
                let header = format!(
                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    frame.len()
                );
                let mut msg = header.into_bytes();
                msg.extend(frame);
                msg.extend(b"\r\n");
                // A slow viewer skips pictures instead of queueing them.
                if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) = tx.try_send(msg) {
                    break;
                }
            }
            // The viewer left (also while the camera sends nothing): stop.
            if tx.is_closed() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(msg) => Some((Ok::<_, std::io::Error>(msg), rx)),
            None => None,
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "multipart/x-mixed-replace; boundary=frame")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Secret alert settings: never sent to the browser. "<set>" means a value
/// exists; sending "<set>" or blank back keeps it.
fn mask_alert_secrets(a: &AlertConfig) -> AlertConfig {
    let mut m = a.clone();
    let mask = |v: &mut Option<String>| {
        if v.as_deref().is_some_and(|s| !s.is_empty()) {
            *v = Some("<set>".into());
        }
    };
    mask(&mut m.telegram_bot_token);
    mask(&mut m.ha_token);
    mask(&mut m.ntfy_token);
    mask(&mut m.pushover_token);
    mask(&mut m.smtp_password);
    // Webhook-style addresses often carry their secret in the path (Discord, Slack).
    mask(&mut m.webhook_url);
    mask(&mut m.image_post_url);
    // A broker URL can carry user:password@; hide it only then.
    if m.mqtt_url.as_deref().is_some_and(|u| u.contains('@')) {
        mask(&mut m.mqtt_url);
    }
    m
}

/// Overlay a partial alerts object onto the stored settings, keeping stored
/// secrets when the page sends them back masked or blank.
fn merge_alerts(cur: &AlertConfig, patch: serde_json::Value) -> Result<AlertConfig, String> {
    let serde_json::Value::Object(patch) = patch else { return Err("alerts must be an object".into()) };
    let mut v = serde_json::to_value(cur).map_err(|e| e.to_string())?;
    let obj = v.as_object_mut().ok_or("internal error")?;
    for (k, val) in patch {
        let masked = matches!(k.as_str(), "telegram_bot_token" | "ha_token" | "smtp_password" | "mqtt_url" | "ntfy_token" | "pushover_token" | "webhook_url" | "image_post_url");
        let keep = masked && val.as_str().is_some_and(|s| s.trim().is_empty() || s == "<set>");
        if !keep {
            obj.insert(k, val);
        }
    }
    serde_json::from_value(v).map_err(|e| format!("invalid alert settings: {e}"))
}

async fn get_settings(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    // Listing the cards may run nvidia-smi (up to seconds): not on this thread.
    let (gpus, gpu_note) = tokio::task::spawn_blocking(|| (crate::gpu::list(), crate::ai::gpu_note())).await.unwrap_or_default();
    let cfg = engine.config.lock().unwrap();
    // Secrets are never sent to the browser; "<set>" means a value exists
    // and saving with the field untouched keeps it.
    let al = mask_alert_secrets(&cfg.alerts);
    Json(serde_json::json!({
        "motion": cfg.motion,
        "storage": cfg.storage,
        "alerts": al,
        "odt": cfg.odt,
        "ai": cfg.ai,
        "ai_status": {
            "builtin_loaded": engine.detector().is_some(),
            "model_exists": cfg.ai.model.as_ref().map(|m| m.is_file()),
            // The graphics card it runs on (null: the CPU), and the cards there are.
            "running_on": engine.detector().and_then(|d| d.gpu_name().map(String::from)),
            "gpus": gpus,
            // Why it is not on the card although one was found (null: no reason to give).
            "gpu_note": gpu_note,
        },
        "automation": cfg.automation,
        "timelapse": cfg.timelapse,
        "faces": cfg.faces,
        "lpr": cfg.lpr,
        "digest": cfg.digest,
        "smart_search": cfg.smart_search,
        "sound": cfg.sound,
        "watchdog": cfg.watchdog,
        "presence": cfg.presence,
        "push": cfg.push,
        // Notification services and whether each is set up (cameras pick from these).
        "alert_channels": alert_channel_status(&cfg.alerts),
        "server": server_settings(&cfg),
    }))
}

/// The `server` section of `GET /api/settings`: the saved HTTPS settings,
/// what the listener uses now, and whether a restart is needed to apply them.
fn server_settings(cfg: &crate::config::Config) -> serde_json::Value {
    let s = &cfg.server;
    let info = crate::tls::current().map(|r| r.info());
    let running_tls = info.is_some();
    let mode = info.as_ref().map(|i| i.mode).unwrap_or("off");
    let note = info.and_then(|i| i.note);
    let path = |p: &Option<std::path::PathBuf>| p.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
    serde_json::json!({
        "port": s.port, "rtmp_port": s.rtmp_port, "tls": s.tls,
        "tls_names": s.tls_names, "tls_cert": path(&s.tls_cert), "tls_key": path(&s.tls_key),
        "running": { "tls": running_tls, "mode": mode, "note": note },
        "restart_needed": https_restart_needed(s),
        // What depends on AmberSight's own certificate authority (they pin
        // it): servers linked to this one and paired capture helpers.
        "linked_servers": cfg.links.len(),
        "paired_helpers": cfg.helpers.len(),
        // Whether POST /api/system/restart can restart it (run by a service manager).
        "restarts_itself": crate::update::under_service(),
    })
}

/// Whether the saved HTTPS settings need a restart to apply, against what
/// the listener started with (also when own certificate files could not be
/// loaded at start and the private CA stands in for them).
fn https_restart_needed(s: &crate::config::ServerConfig) -> bool {
    crate::tls::started().is_some_and(|st| crate::tls::restart_needed(st, s)) || (s.tls && crate::tls::fell_back())
}

/// The warning for own certificate files newly saved on a server that has
/// linked servers or paired capture helpers: both trust only AmberSight's
/// own certificate authority (pinned when they were linked or paired).
fn own_cert_warning(cur: &crate::config::ServerConfig, new: &crate::config::ServerConfig, links: usize, helpers: usize) -> Option<String> {
    let own = new.tls && new.tls_cert.is_some() && new.tls_key.is_some();
    let changed = new.tls_cert != cur.tls_cert || new.tls_key != cur.tls_key || new.tls != cur.tls;
    if !own || !changed || links + helpers == 0 {
        return None;
    }
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let what = match (links, helpers) {
        (0, h) => plural(h, "paired capture helper", "paired capture helpers"),
        (l, 0) => plural(l, "linked server", "linked servers"),
        (l, h) => format!("{} and {}", plural(l, "linked server", "linked servers"), plural(h, "paired capture helper", "paired capture helpers")),
    };
    Some(format!(
        "After the restart, {what} stop working: they trust only AmberSight's own certificate authority, and new links and pairings need it too. They work again when you switch back to AmberSight's own certificate (it is kept)."
    ))
}

/// `w` added to an earlier warning, if there was one.
fn join_warning(earlier: Option<String>, w: String) -> Option<String> {
    Some(match earlier {
        Some(e) => format!("{e} {w}"),
        None => w,
    })
}

/// HTTPS settings that can be changed from Settings (`port` and `bind`
/// cannot: they are ignored here). Fields left out keep their value;
/// `""` clears a certificate file.
#[derive(serde::Deserialize, Clone)]
struct ServerUpdate {
    tls: Option<bool>,
    tls_names: Option<Vec<String>>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
}

/// `cur` with the HTTPS settings in `u`, checked: names are host names or
/// addresses, and own certificate files come as a readable, matching pair.
fn apply_server_update(cur: &crate::config::ServerConfig, u: ServerUpdate) -> Result<crate::config::ServerConfig, String> {
    let mut s = cur.clone();
    if let Some(t) = u.tls {
        s.tls = t;
    }
    if let Some(n) = u.tls_names {
        s.tls_names = crate::tls::check_names(&n)?;
    }
    let path = |v: String| { let v = v.trim().to_string(); (!v.is_empty()).then(|| std::path::PathBuf::from(v)) };
    if let Some(c) = u.tls_cert {
        s.tls_cert = path(c);
    }
    if let Some(k) = u.tls_key {
        s.tls_key = path(k);
    }
    crate::tls::check_files(s.tls_cert.as_deref(), s.tls_key.as_deref())?;
    Ok(s)
}

/// `POST /api/system/restart` (admin): stop so the service manager starts
/// AmberSight again (as after an update). 409 when it is not run by one.
async fn system_restart(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>) -> Response {
    if let Some(refused) = restart_refusal(crate::update::under_service(), crate::update::installing()) {
        return refused;
    }
    tracing::info!("restart requested from Settings by {}", caller.0);
    // (It takes no password. What waits to go out is sent before it stops: see cli.rs.)
    crate::account_http::tell_admins(&engine, "restart requested".to_string(), format!("{} asked for a restart of the server, from {}.", caller.0, peer.ip()));
    // Shortly after the answer has gone out (time for the page to sign out
    // over https first when moving to http, see https_settings.js).
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        crate::update::request_restart();
    });
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// Why a restart from Settings is refused (409), if it is: not run by a
/// service manager, or an update is being installed.
fn restart_refusal(under_service: bool, installing: bool) -> Option<Response> {
    let why = if !under_service {
        "AmberSight is not running as a service here, so it cannot restart itself; restart it by hand"
    } else if installing {
        "an update is being installed; try again when it has finished"
    } else {
        return None;
    };
    Some((StatusCode::CONFLICT, Json(serde_json::json!({ "error": why }))).into_response())
}

fn alert_channel_status(a: &crate::config::AlertConfig) -> serde_json::Value {
    let set = |o: &Option<String>| o.as_deref().is_some_and(|v| !v.trim().is_empty());
    let configured = |ch: &str| match ch {
        "email" => set(&a.smtp_host) && !a.email_to.is_empty(),
        "telegram" => set(&a.telegram_bot_token) && set(&a.telegram_chat_id),
        "ntfy" => set(&a.ntfy_url),
        "pushover" => set(&a.pushover_token) && set(&a.pushover_user),
        "webhook" => set(&a.webhook_url),
        "home_assistant" => set(&a.ha_url),
        "image_post" => set(&a.image_post_url),
        "action" => set(&a.action_command),
        "push" => crate::push::any_subscribed(),
        _ => false,
    };
    crate::config::ALERT_CHANNELS.iter().map(|ch| serde_json::json!({ "id": ch, "configured": configured(ch) })).collect()
}

/// Overlay a partial JSON object onto a settings section; keys left out
/// keep their current value.
fn merge_section<T: serde::Serialize + serde::de::DeserializeOwned>(cur: &T, patch: serde_json::Value) -> Result<T, String> {
    let serde_json::Value::Object(patch) = patch else { return Err("expected an object".into()) };
    let mut v = serde_json::to_value(cur).map_err(|e| e.to_string())?;
    let obj = v.as_object_mut().ok_or("internal error")?;
    for (k, val) in patch {
        obj.insert(k, val);
    }
    serde_json::from_value(v).map_err(|e| e.to_string())
}

#[derive(serde::Deserialize)]
struct SettingsUpdate {
    /// Every section is partial: fields left out keep their current value.
    motion: Option<serde_json::Value>,
    storage: Option<StorageUpdate>,
    alerts: Option<serde_json::Value>,
    odt: Option<serde_json::Value>,
    ai: Option<serde_json::Value>,
    automation: Option<serde_json::Value>,
    timelapse: Option<serde_json::Value>,
    faces: Option<serde_json::Value>,
    lpr: Option<serde_json::Value>,
    digest: Option<serde_json::Value>,
    smart_search: Option<serde_json::Value>,
    sound: Option<serde_json::Value>,
    watchdog: Option<serde_json::Value>,
    presence: Option<serde_json::Value>,
    push: Option<serde_json::Value>,
    server: Option<ServerUpdate>,
    /// The caller's own password and code, when `settings_need_proof` says so.
    #[serde(flatten)]
    proof: crate::account_http::MyProof,
}

/// Whether a command in a settings patch would change: to a new text. Left out,
/// masked, the same as now, or being cleared: not.
fn command_changes(cur: Option<&str>, patch: &Option<serde_json::Value>, key: &str) -> bool {
    let Some(val) = patch.as_ref().and_then(|p| p.get(key)) else { return false };
    match val {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => {
            let t = s.trim();
            !t.is_empty() && t != "<set>" && Some(t) != cur.map(str::trim)
        }
        _ => true,
    }
}

/// Where alerts go and whether they are heard. Someone with a stolen session must
/// not be able to quietly silence them or send them, with what they carry, somewhere else.
const ALERT_DESTINATIONS: &[&str] = &[
    "pushover_sound", "webhook_url", "image_post_url", "telegram_bot_token", "telegram_chat_id", "telegram_api_url", "ha_url", "ha_token", "ha_entity", "ntfy_url", "ntfy_token", "pushover_token", "pushover_user", "pushover_device", "mqtt_url", "mqtt_prefix", "smtp_host", "smtp_port", "smtp_user", "smtp_password", "smtp_from", "smtp_tls", "email_to",
];

/// Which alert destinations a settings patch would change, compared after the
/// merge, so that masked values and what is left out count as unchanged. A patch
/// that does not merge is refused later anyway.
fn alert_destinations_changed(cur: &AlertConfig, patch: &Option<serde_json::Value>) -> Vec<&'static str> {
    use serde_json::Value;
    let Some(p) = patch else { return Vec::new() };
    let Ok(new) = merge_alerts(cur, p.clone()) else { return Vec::new() };
    let (Ok(a), Ok(b)) = (serde_json::to_value(cur), serde_json::to_value(&new)) else { return Vec::new() };
    let plain = |v: Option<&Value>| match v {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(s)) if s.trim().is_empty() => Value::Null,
        Some(Value::Array(l)) if l.is_empty() => Value::Null,
        Some(x) => x.clone(),
    };
    let mut out: Vec<&'static str> = ALERT_DESTINATIONS.iter().copied().filter(|k| plain(a.get(*k)) != plain(b.get(*k))).collect();
    // Compared for what they do, so that saving what a form shows is no change:
    // a link with or without its last slash, and the defaults spelled out.
    if link_text(&cur.link_url) != link_text(&new.link_url) {
        out.push("link_url");
    }
    if cur.system_alerts_on() != new.system_alerts_on() {
        out.push("system_alerts");
    }
    if cur.system_alert_secs() != new.system_alert_secs() {
        out.push("system_alert_minutes");
    }
    out
}

fn link_text(u: &Option<String>) -> &str {
    u.as_deref().map_or("", |u| u.trim().trim_end_matches('/'))
}

/// What a settings patch would change about where or whether alerts arrive: the
/// alert settings, and three other places. A push server taken off the list stops
/// the devices on it. The morning summary turned off or narrowed stops it telling.
/// The watchdog holds the plain "camera offline" alert back while it is working
/// on a camera, so turning it on, or letting it wait or try for longer, delays that
/// alert (up to a day); its quiet hours do not (the alert goes through then).
fn delivery_changes(cfg: &crate::config::Config, update: &SettingsUpdate) -> Vec<&'static str> {
    let mut out = alert_destinations_changed(&cfg.alerts, &update.alerts);
    if let Some(Ok(mut new)) = update.push.as_ref().map(|p| merge_section::<crate::config::PushConfig>(&cfg.push, p.clone())) {
        if new.sanitize().is_ok() && cfg.push.app_hosts.iter().any(|h| !new.app_hosts.contains(h)) {
            out.push("push.app_hosts");
        }
    }
    if let Some(Ok(new)) = update.digest.as_ref().map(|p| merge_section::<crate::config::DigestConfig>(&cfg.digest, p.clone())) {
        let set = |l: &[String]| l.iter().collect::<std::collections::BTreeSet<_>>().into_iter().cloned().collect::<Vec<_>>();
        if cfg.digest.enabled && !new.enabled {
            out.push("digest.enabled");
        }
        if cfg.digest.channels.as_deref().map(set) != new.channels.as_deref().map(set) {
            out.push("digest.channels");
        }
        if set(&cfg.digest.cameras) != set(&new.cameras) {
            out.push("digest.cameras");
        }
    }
    if let Some(Ok(mut new)) = update.watchdog.as_ref().map(|p| merge_section::<crate::config::WatchdogConfig>(&cfg.watchdog, p.clone())) {
        new.sanitize();
        if !cfg.watchdog.enabled && new.enabled {
            out.push("watchdog.enabled");
        }
        if new.minutes > cfg.watchdog.minutes {
            out.push("watchdog.minutes");
        }
        if new.max_per_day > cfg.watchdog.max_per_day {
            out.push("watchdog.max_per_day");
        }
    }
    out
}

/// Settings that run programs on this computer, and those that change where or
/// whether alerts arrive, take the caller's own password.
fn settings_need_proof(cfg: &crate::config::Config, update: &SettingsUpdate) -> bool {
    command_changes(cfg.alerts.action_command.as_deref(), &update.alerts, "action_command") || command_changes(cfg.odt.command.as_deref(), &update.odt, "command") || !delivery_changes(cfg, update).is_empty()
}

/// A new link for notifications that is no good, whoever asks: said before any
/// password or code is asked for or used up.
fn bad_link_url(cur: &AlertConfig, patch: &Option<serde_json::Value>) -> bool {
    let Some(Ok(new)) = patch.as_ref().map(|p| merge_alerts(cur, p.clone())) else { return false };
    link_text(&cur.link_url) != link_text(&new.link_url) && !link_text(&new.link_url).is_empty() && !crate::config::link_url_ok(link_text(&new.link_url))
}

/// Storage settings that can change while running. Fields left out keep
/// their current value; `folders` is only replaced when sent. `data_dir`
/// is accepted but ignored (it only changes with a restart).
#[derive(serde::Deserialize, Clone)]
struct StorageUpdate {
    segment_seconds: Option<u64>,
    retention_days: Option<u64>,
    max_mb: Option<u64>,
    min_free_percent: Option<u64>,
    event_keep_days: Option<u32>,
    sub_copy: Option<bool>,
    folders: Option<Vec<crate::config::FolderConfig>>,
    #[allow(dead_code)]
    data_dir: Option<serde_json::Value>,
    /// What a folder that is new or moved gets for "keep private", worked out
    /// before the settings lock is taken (it reads the disk).
    #[serde(skip)]
    defaults: std::collections::HashMap<std::path::PathBuf, bool>,
}

/// Reject folder lists a person can easily get wrong in the editor.
fn validate_folders(folders: &[crate::config::FolderConfig]) -> Result<(), String> {
    for (i, f) in folders.iter().enumerate() {
        if f.name.trim().is_empty() {
            return Err("a storage folder has no name".into());
        }
        if f.path.as_os_str().is_empty() {
            return Err(format!("storage folder '{}' has no path", f.name));
        }
        if folders[..i].iter().any(|o| o.name.eq_ignore_ascii_case(&f.name)) {
            return Err(format!("storage folder '{}' appears twice", f.name));
        }
    }
    if folders.iter().filter(|f| f.archive).count() > 1 {
        return Err("only one storage folder can be the archive".into());
    }
    Ok(())
}

/// A folder that still holds recordings must stay, under the same name and
/// path (otherwise its recordings could no longer be found or cleaned up).
fn folders_keep_recordings(in_use: &[(String, std::path::PathBuf, i64)], new: &[crate::config::FolderConfig]) -> Result<(), String> {
    for (name, path, bytes) in in_use {
        match new.iter().find(|f| &f.name == name) {
            None => return Err(format!("folder '{name}' still holds {} MB of recordings: move cameras off it and wait for them to age out (or set a small cap) before removing it", bytes / 1_048_576)),
            Some(f) if &f.path != path => return Err(format!("folder '{name}' still holds recordings at {}: its path cannot change", path.display())),
            _ => {}
        }
    }
    Ok(())
}

fn apply_storage_update(cur: &mut StorageConfig, u: StorageUpdate) {
    if let Some(v) = u.segment_seconds.filter(|v| *v > 0) {
        cur.segment_seconds = v;
    }
    // 0 days would delete every recording at the next pass: never accepted.
    if let Some(v) = u.retention_days.filter(|v| *v > 0) {
        cur.retention_days = v.min(3650);
    }
    if let Some(v) = u.max_mb {
        cur.max_mb = v;
    }
    if let Some(v) = u.min_free_percent {
        cur.min_free_percent = v.min(90);
    }
    if let Some(v) = u.event_keep_days {
        cur.event_keep_days = if v == 21 { None } else { Some(v.min(3650)) };
    }
    if let Some(v) = u.sub_copy {
        cur.sub_copy = v;
    }
    if let Some(mut f) = u.folders.clone() {
        for n in f.iter_mut() {
            // The same folder: by name, else by path (a rename).
            let old = cur.folders.iter().find(|o| o.name == n.name).or_else(|| cur.folders.iter().find(|o| o.path == n.path));
            let default = || u.defaults.get(&n.path).copied().unwrap_or(false);
            n.private = match old {
                // Same place (a rename or no change): what it was, including no value at all.
                Some(o) if o.path == n.path => n.private.or(o.private),
                // A moved folder is a new folder; the page sends no value for it unless the
                // person chose one. The old path is left as it is.
                _ => n.private.or_else(|| Some(default())),
            };
        }
        cur.folders = f;
    }
}

/// A folder added after this was introduced: on when new or empty, off when it
/// already holds files, and off where it cannot be made private.
fn new_folder_default(path: &std::path::Path) -> bool {
    if matches!(crate::lockdown::folder_state(path).0, "refused" | "unsupported") {
        return false;
    }
    let (exists, empty) = folder_contents(path);
    crate::lockdown::default_private_for_new(exists, !empty)
}

/// The recording folders whose "keep private" switch changed with this save:
/// (path, now on). A new folder with the switch on counts; a folder that
/// never had the switch is left alone.
fn privacy_changes(old: &[crate::config::FolderConfig], new: &[crate::config::FolderConfig]) -> Vec<(std::path::PathBuf, bool)> {
    let mut out = Vec::new();
    for f in new {
        let before = old.iter().find(|o| o.name == f.name).or_else(|| old.iter().find(|o| o.path == f.path));
        match before {
            Some(b) if b.path == f.path => match (b.private, f.private) {
                (p, Some(true)) if p != Some(true) => out.push((f.path.clone(), true)),
                (Some(true), Some(false)) => out.push((f.path.clone(), false)),
                _ => {}
            },
            // A moved or new folder: switched on if it says so; the old path stays as it is.
            _ if f.private == Some(true) => out.push((f.path.clone(), true)),
            _ => {}
        }
    }
    out
}

/// Folders that had no value and now say "off" at the same place: opened
/// again only if they really are private (the page shows a private folder
/// with no value as on, and turning it off must open it).
fn unset_to_off(old: &[crate::config::FolderConfig], new: &[crate::config::FolderConfig]) -> Vec<std::path::PathBuf> {
    new.iter()
        .filter(|f| f.private == Some(false))
        .filter(|f| old.iter().any(|o| o.name == f.name && o.path == f.path && o.private.is_none()))
        .map(|f| f.path.clone())
        .collect()
}

/// What each new or moved recording folder gets for "keep private", worked out
/// here, off the async workers and before the settings lock is taken: it reads
/// the disk (and a dead network path must not hold the lock up).
async fn fill_folder_defaults(engine: &Engine, update: &mut SettingsUpdate) {
    let Some(s) = update.storage.as_mut() else { return };
    let Some(folders) = s.folders.clone() else { return };
    let current = engine.config.lock().unwrap().storage.folders.clone();
    let todo: Vec<std::path::PathBuf> = folders.iter().filter(|n| n.private.is_none() && !current.iter().any(|o| o.path == n.path)).map(|n| n.path.clone()).collect();
    if todo.is_empty() {
        return;
    }
    s.defaults = tokio::task::spawn_blocking(move || todo.into_iter().map(|p| { let d = new_folder_default(&p); (p, d) }).collect::<std::collections::HashMap<_, _>>()).await.unwrap_or_default();
}

async fn update_settings(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    headers: axum::http::HeaderMap,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Json(mut update): Json<SettingsUpdate>,
) -> Response {
    fill_folder_defaults(&engine, &mut update).await;
    if bad_link_url(&engine.config.lock().unwrap().alerts, &update.alerts) {
        return Json(serde_json::json!({ "error": "this server's address for links must start with http:// or https://, with no user name or password in it" })).into_response();
    }
    let sensitive = settings_need_proof(&engine.config.lock().unwrap(), &update);
    // Before any password is asked for and before anyone is told: a change that
    // will be refused anyway does neither.
    if sensitive {
        if let Some(e) = settings_refusal(&engine, &update).await {
            return Json(serde_json::json!({ "error": e })).into_response();
        }
    }
    let proved = caller.0 != "(open)" && sensitive;
    if proved {
        if let Err(r) = crate::account_http::prove_admin(&engine, &caller.0, peer.ip(), &headers, &update.proof, caller.1).await {
            return r;
        }
        // The old destinations hear of it first: whoever is changing them may be
        // silencing alerts. (Not waiting long for a service that does not answer.)
        let (old, changed) = {
            let cfg = engine.config.lock().unwrap();
            (cfg.alerts.clone(), delivery_changes(&cfg, &update))
        };
        if !changed.is_empty() {
            let site = crate::links::own_name(&engine);
            let fields = changed.join(", ");
            let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "alert destinations changed", &fields, 200);
            let (who, ip) = (caller.0.clone(), peer.ip());
            let told = tokio::task::spawn_blocking(move || {
                crate::engine::system_alerts::send(&old, &format!("{site}: alert destinations being changed"), &format!("{who} is changing where or whether alerts go, from {ip} ({fields}). This may be the last message here. If that was not meant to happen, check Settings, Alerts, and change that password."), true);
            });
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), told).await;
        }
    }
    apply_settings(engine, update, caller, proved, peer.ip()).await.into_response()
}

/// What applying a settings change has to do afterwards.
#[derive(Default)]
struct SettingsEffects {
    reload_ai: bool,
    restart_sub_copy: bool,
    reload_faces: bool,
    reload_lpr: bool,
    reload_smart: bool,
    reload_sound: bool,
    profiles_changed: bool,
    presence_changed: bool,
}

/// Merge a settings change into `cfg` (the HTTPS section apart: see
/// `check_server_update`). The refusal is what the caller is told; nothing but
/// `cfg` is touched, so it can also be tried on a copy first.
fn merge_settings(cfg: &mut crate::config::Config, update: &SettingsUpdate, in_use: &[(String, std::path::PathBuf, i64)]) -> Result<SettingsEffects, String> {
    let mut fx = SettingsEffects::default();
    if let Some(m) = update.motion.clone() {
        match merge_section::<MotionConfig>(&cfg.motion, m) {
            Ok(v) => cfg.motion = v,
            Err(e) => return Err(format!("motion: {e}").into()),
        }
    }
    if let Some(s) = update.storage.clone() {
        if let Some(f) = &s.folders {
            if let Err(e) = validate_folders(f) {
                return Err(format!("storage: {e}").into());
            }
            if let Err(e) = folders_keep_recordings(&in_use, f) {
                return Err(format!("storage: {e}").into());
            }
        }
        let had = cfg.storage.sub_copy;
        apply_storage_update(&mut cfg.storage, s);
        // Cameras whose sessions record differently now.
        if cfg.storage.sub_copy != had {
            fx.restart_sub_copy = true;
        }
    }
    if let Some(a) = update.alerts.clone() {
        match merge_alerts(&cfg.alerts, a) {
            Ok(m) => cfg.alerts = m,
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(o) = update.odt.clone() {
        match merge_section::<OdtConfig>(&cfg.odt, o) {
            Ok(v) => cfg.odt = v,
            Err(e) => return Err(format!("odt: {e}").into()),
        }
    }
    if let Some(a) = update.ai.clone() {
        match merge_section::<crate::config::AiConfig>(&cfg.ai, a) {
            Ok(v) => {
                if v.model != cfg.ai.model || v.enabled != cfg.ai.enabled || v.threads != cfg.ai.threads || v.gpu != cfg.ai.gpu || v.gpu_name != cfg.ai.gpu_name || v.gpu_memory_mb != cfg.ai.gpu_memory_mb {
                    fx.reload_ai = true;
                }
                cfg.ai = v;
            }
            Err(e) => return Err(format!("ai: {e}").into()),
        }
    }
    if let Some(f) = update.faces.clone() {
        match merge_section::<crate::config::FacesConfig>(&cfg.faces, f) {
            Ok(mut v) => {
                v.sanitize();
                if v.enabled != cfg.faces.enabled {
                    fx.reload_faces = true;
                }
                cfg.faces = v;
            }
            Err(e) => return Err(format!("faces: {e}").into()),
        }
    }
    if let Some(f) = update.lpr.clone() {
        match merge_section::<crate::config::LprConfig>(&cfg.lpr, f) {
            Ok(mut v) => {
                v.sanitize();
                if v.enabled != cfg.lpr.enabled {
                    fx.reload_lpr = true;
                }
                cfg.lpr = v;
            }
            Err(e) => return Err(format!("license plates: {e}").into()),
        }
    }
    if let Some(f) = update.digest.clone() {
        match merge_section::<crate::config::DigestConfig>(&cfg.digest, f) {
            Ok(v) => match v.check() {
                Ok(()) => cfg.digest = v,
                Err(e) => return Err(e.into()),
            },
            Err(e) => return Err(format!("morning summary: {e}").into()),
        }
    }
    if let Some(f) = update.smart_search.clone() {
        match merge_section::<crate::config::SmartSearchConfig>(&cfg.smart_search, f) {
            Ok(v) => {
                if v.enabled != cfg.smart_search.enabled {
                    fx.reload_smart = true;
                }
                cfg.smart_search = v;
            }
            Err(e) => return Err(format!("smart search: {e}").into()),
        }
    }
    if let Some(f) = update.sound.clone() {
        match merge_section::<crate::config::SoundConfig>(&cfg.sound, f) {
            Ok(v) => {
                if v.labels != cfg.sound.labels {
                    fx.reload_sound = true;
                }
                cfg.sound = v;
            }
            Err(e) => return Err(format!("sound: {e}").into()),
        }
    }
    if let Some(w) = update.watchdog.clone() {
        match merge_section::<crate::config::WatchdogConfig>(&cfg.watchdog, w) {
            Ok(mut v) => {
                if v.quiet_hours.as_deref().is_some_and(|q| !q.trim().is_empty() && crate::config::parse_schedule(q).is_none()) {
                    return Err("watchdog: write quiet hours like 22:00-06:00".into());
                }
                v.sanitize();
                cfg.watchdog = v;
            }
            Err(e) => return Err(format!("watchdog: {e}").into()),
        }
    }
    if let Some(p) = update.presence.clone() {
        match merge_section::<crate::config::PresenceConfig>(&cfg.presence, p) {
            Ok(mut v) => {
                v.sanitize();
                cfg.presence = v;
                fx.presence_changed = true;
            }
            Err(e) => return Err(format!("presence: {e}").into()),
        }
    }
    if let Some(p) = update.push.clone() {
        match merge_section::<crate::config::PushConfig>(&cfg.push, p) {
            Ok(mut v) => {
                if let Err(e) = v.sanitize() {
                    return Err(format!("push servers for apps: {e}").into());
                }
                cfg.push = v;
            }
            Err(e) => return Err(format!("push: {e}").into()),
        }
    }
    if let Some(t) = update.timelapse.clone() {
        match merge_section::<crate::config::TimelapseConfig>(&cfg.timelapse, t) {
            Ok(v) => cfg.timelapse = v,
            Err(e) => return Err(format!("timelapse: {e}").into()),
        }
    }
    if let Some(a) = update.automation.clone() {
        match merge_section::<crate::config::AutomationConfig>(&cfg.automation, a) {
            Ok(v) => {
                if let Err(e) = v.validate() {
                    return Err(format!("automation: {e}").into());
                }
                fx.profiles_changed = true;
                cfg.automation = v;
            }
            Err(e) => return Err(format!("automation: {e}").into()),
        }
    }
    // (Also when only the profiles changed: one may have been removed.)
    if fx.presence_changed || fx.profiles_changed {
        if let Err(e) = cfg.presence.validate(&cfg.automation) {
            return Err(format!("presence: {e}").into());
        }
    }
    Ok(fx)
}

/// Folders that still hold recordings cannot be removed or moved.
fn folders_in_use(engine: &Engine) -> Vec<(String, std::path::PathBuf, i64)> {
    let folders = engine.config.lock().unwrap().storage.folders.clone();
    folders.into_iter().map(|f| { let b = engine.db.folder_bytes(&f.name).unwrap_or(0); (f.name, f.path, b) }).filter(|f| f.2 > 0).collect()
}

/// HTTPS settings, checked first (reads the certificate files: off the async
/// workers). The new settings and a note about the certificate, if any.
async fn check_server_update(engine: &Engine, u: ServerUpdate) -> Result<(crate::config::ServerConfig, Option<String>), String> {
    let (cur, data_dir, links, helpers) = {
        let c = engine.config.lock().unwrap();
        (c.server.clone(), c.storage.data_dir.clone(), c.links.len(), c.helpers.len())
    };
    let cur2 = cur.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let v = apply_server_update(&cur2, u)?;
        // HTTPS with AmberSight's own certificate authority, not
        // running yet: check now that the next start can set it up.
        let running_ca = crate::tls::current().is_some_and(|r| r.info().mode == "private-ca");
        if v.tls && v.tls_cert.is_none() && !running_ca {
            crate::tls::check_private_ca(&data_dir)?;
        }
        Ok::<_, String>(v)
    })
    .await;
    match checked {
        Ok(Ok(v)) => {
            let note = own_cert_warning(&cur, &v, links, helpers);
            Ok((v, note))
        }
        Ok(Err(e)) => Err(format!("HTTPS: {e}")),
        Err(e) => Err(format!("HTTPS: {e}")),
    }
}

/// What `apply_settings` would refuse in this change, tried on a copy: a change
/// that will fail anyway asks for no password and tells no one.
async fn settings_refusal(engine: &Engine, update: &SettingsUpdate) -> Option<String> {
    if let Some(u) = update.server.clone() {
        if let Err(e) = check_server_update(engine, u).await {
            return Some(e);
        }
    }
    let in_use = folders_in_use(engine);
    let mut cfg = engine.config.lock().unwrap().clone();
    merge_settings(&mut cfg, update, &in_use).err()
}

async fn apply_settings(engine: Arc<Engine>, mut update: SettingsUpdate, caller: Caller, proved: bool, ip: std::net::IpAddr) -> Json<serde_json::Value> {
    let mut server_changed = false;
    let fx;
    let in_use = folders_in_use(&engine);
    let mut own_cert_note: Option<String> = None;
    let old_folders;
    let new_server = match update.server.take() {
        Some(u) => match check_server_update(&engine, u).await {
            Ok((v, note)) => {
                own_cert_note = note;
                Some(v)
            }
            Err(e) => return Json(serde_json::json!({ "error": e })),
        },
        None => None,
    };
    {
        // Work on a copy: a section that fails leaves everything as it was.
        let mut live = engine.config.lock().unwrap();
        // (Checked again now the slow check is over: the caller as it is now, and the
        // commands as they are now.)
        if proved && (!crate::users_http::caller_may_change_accounts(&live, &caller.0) || crate::auth::generation(&caller.0) != caller.1) {
            return Json(serde_json::json!({ "error": "this account changed meanwhile: try again" }));
        }
        if !proved && caller.0 != "(open)" && settings_need_proof(&live, &update) {
            return Json(serde_json::json!({ "error": "the settings changed meanwhile: try again" }));
        }
        let mut cfg = live.clone();
        old_folders = live.storage.folders.clone();
        let merged = merge_settings(&mut cfg, &update, &in_use);
        fx = match merged {
            Ok(f) => f,
            Err(e) => return Json(serde_json::json!({ "error": e })),
        };
        if let Some(v) = new_server {
            server_changed = v.tls_names != cfg.server.tls_names;
            cfg.server.tls = v.tls;
            cfg.server.tls_names = v.tls_names;
            cfg.server.tls_cert = v.tls_cert;
            cfg.server.tls_key = v.tls_key;
        }
        *live = cfg;
        if let Err(e) = live.save(&engine.config_path) {
            return Json(serde_json::json!({ "error": format!("the settings apply now, but could not be saved to the config file: {e:#}") }));
        }
    }
    // Recording folders switched to or from private: done on the background
    // thread, never in the way of recording. Said in the audit log.
    let (changes, was_unset) = {
        let now = engine.config.lock().unwrap().storage.folders.clone();
        (privacy_changes(&old_folders, &now), unset_to_off(&old_folders, &now))
    };
    for path in was_unset {
        let _ = engine.db.insert_audit(&caller.0, &ip.to_string(), "recording folder: asked to open it again (it had no setting)", &path.to_string_lossy(), 200);
        crate::lockdown::apply_switch_off_if_private(&path);
    }
    for (path, on) in changes {
        let _ = engine.db.insert_audit(&caller.0, &ip.to_string(), if on { "recording folder: asked to make it private" } else { "recording folder: asked to open it again" }, &path.to_string_lossy(), 200);
        crate::lockdown::apply_switch(&path, on);
    }
    let (server, data_dir) = {
        let c = engine.config.lock().unwrap();
        (c.server.clone(), c.storage.data_dir.clone())
    };
    let restart_needed = https_restart_needed(&server);
    // New extra names go on the certificate now (no restart needed).
    let mut https_warning: Option<String> = own_cert_note;
    if let (true, Some(rt)) = (server_changed, crate::tls::current()) {
        let (s2, rt2) = (server.clone(), rt.clone());
        match tokio::task::spawn_blocking(move || rt2.refresh(&s2, &data_dir)).await {
            Ok(Err(e)) => https_warning = join_warning(https_warning, format!("The certificate could not be renewed for the new names: {e:#}")),
            _ => {
                let info = rt.info();
                if info.mode == "private-ca" {
                    let left: Vec<String> = server.tls_names.iter().filter(|n| !info.names.contains(n)).cloned().collect();
                    if !left.is_empty() {
                        https_warning = join_warning(https_warning, format!("The certificate cannot cover {}: AmberSight's certificate authority only vouches for home-network names (ending in .lan, .local, .home.arpa or .internal, and this computer's name), home-network and Tailscale (100.x) addresses, and the names it was made with. Other names, such as a Tailscale .ts.net name, are only covered by a certificate authority made with them (add them before HTTPS is first turned on) or by your own certificate (Advanced)", left.join(", ")));
                    }
                }
            }
        }
    }
    if fx.profiles_changed {
        engine.reapply_profile();
    }
    // (A profile presence names may have come or gone.)
    if fx.presence_changed || fx.profiles_changed {
        let e = engine.clone();
        let _ = tokio::task::spawn_blocking(move || e.presence_evaluate(None)).await;
    }
    // The built-in model is swapped in (or out) right away.
    // Cameras whose sessions record differently now: continuous ones with a
    // sub stream, by their running settings (profile included) and real ids.
    if fx.restart_sub_copy {
        let ids: Vec<usize> = engine
            .cameras
            .read()
            .unwrap()
            .iter()
            .flatten()
            .filter(|h| {
                let c = h.running.read().unwrap();
                c.record && !c.record_motion_only && !c.record_sub && c.sub_url.as_deref().is_some_and(|u| !u.is_empty())
            })
            .map(|h| h.id)
            .collect();
        for id in ids {
            engine.restart_camera(id);
        }
    }
    if fx.reload_sound {
        let e = engine.clone();
        let _ = tokio::task::spawn_blocking(move || e.reload_sound()).await;
    }
    if fx.reload_smart {
        let e = engine.clone();
        let _ = tokio::task::spawn_blocking(move || e.reload_smart()).await;
    }
    if fx.reload_faces {
        let e = engine.clone();
        if let Ok(Err(err)) = tokio::task::spawn_blocking(move || e.reload_faces()).await {
            return Json(serde_json::json!({ "result": "success", "restart_needed": restart_needed, "warning": format!("face models not loaded: {err:#}") }));
        }
    }
    if fx.reload_lpr {
        let e = engine.clone();
        // Turned on without the models: fetch them now.
        if e.config.lock().unwrap().lpr.enabled && !crate::lpr::models_present(&e.lpr_models_dir()) {
            e.start_lpr_download();
        }
        if let Ok(Err(err)) = tokio::task::spawn_blocking(move || e.reload_lpr()).await {
            return Json(serde_json::json!({ "result": "success", "restart_needed": restart_needed, "warning": format!("plate models not loaded: {err:#}") }));
        }
    }
    if fx.reload_ai {
        let e = engine.clone();
        if let Ok(Err(err)) = tokio::task::spawn_blocking(move || e.reload_ai()).await {
            return Json(serde_json::json!({ "result": "success", "restart_needed": restart_needed, "warning": format!("AI model not loaded: {err:#}") }));
        }
    }
    match https_warning {
        Some(w) => Json(serde_json::json!({ "result": "success", "restart_needed": restart_needed, "warning": w })),
        None => Json(serde_json::json!({ "result": "success", "restart_needed": restart_needed })),
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    #[test]
    fn visitor_address_only_from_a_trusted_proxy() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.9, 198.51.100.7".parse().unwrap());
        let docker = vec!["172.17.0.0/16".to_string()];
        // The proxy's own entry is the last one.
        assert_eq!(forwarded_client(ip("172.17.0.1"), &docker, &h), Some(ip("198.51.100.7")));
        // Not a trusted proxy, or none configured: the header is ignored.
        assert_eq!(forwarded_client(ip("192.0.2.5"), &docker, &h), None);
        assert_eq!(forwarded_client(ip("172.17.0.1"), &[], &h), None);
        // No header, or junk in it.
        assert_eq!(forwarded_client(ip("172.17.0.1"), &docker, &axum::http::HeaderMap::new()), None);
        let mut bad = axum::http::HeaderMap::new();
        bad.insert("x-forwarded-for", "not-an-ip".parse().unwrap());
        assert_eq!(forwarded_client(ip("172.17.0.1"), &docker, &bad), None);
        for (h, want) in [("1.2.3.4:5678", "1.2.3.4"), ("[2001:db8::1]", "2001:db8::1"), ("[2001:db8::1]:443", "2001:db8::1"), ("2001:db8::2", "2001:db8::2")] {
            let mut x = axum::http::HeaderMap::new();
            x.insert("x-forwarded-for", h.parse().unwrap());
            assert_eq!(forwarded_client(ip("172.17.0.1"), &docker, &x), Some(ip(want)), "{h}");
        }
        let mut p = axum::http::HeaderMap::new();
        p.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(forwarded_https(&p));
        p.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!forwarded_https(&p));
        assert!(!forwarded_https(&axum::http::HeaderMap::new()));
        // Single addresses, IPv6 and IPv4-mapped forms.
        assert!(ip_in(ip("127.0.0.1"), "127.0.0.1"));
        assert!(!ip_in(ip("127.0.0.2"), "127.0.0.1"));
        assert!(ip_in(ip("::ffff:172.17.0.1"), "172.17.0.0/16"));
        assert!(ip_in(ip("fd7a:115c:a1e0::5"), "fd7a:115c:a1e0::/48"));
        assert!(!ip_in(ip("10.0.0.1"), "garbage"));
        assert!(ip_in(ip("10.1.2.3"), "0.0.0.0/0"));
        let w = |l: &[&str]| trusted_proxy_warnings(&l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(w(&["127.0.0.1", "172.17.0.1", "172.17.0.0/16", "fd7a:115c:a1e0::/48"]).is_empty());
        assert!(w(&["0.0.0.0/0"])[0].contains("very wide"));
        assert!(w(&["::/0"])[0].contains("very wide"));
        assert!(w(&["10.0.0.0/8"])[0].contains("very wide"));
        assert!(w(&["proxy.lan"])[0].contains("not an address"));
        assert!(w(&["10.0.0.1/abc"])[0].contains("bad range"));
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;

    #[test]
    fn range_parsing() {
        let len = 1000;
        assert_eq!(parse_range("bytes=0-99", len), Some(Ok((0, 99))));
        assert_eq!(parse_range("bytes=900-", len), Some(Ok((900, 999))));
        assert_eq!(parse_range("bytes=-100", len), Some(Ok((900, 999))));
        assert_eq!(parse_range("bytes=990-5000", len), Some(Ok((990, 999))));
        assert_eq!(parse_range("bytes=1000-", len), Some(Err(())));
        assert_eq!(parse_range("bytes=50-10", len), Some(Err(())));
        assert_eq!(parse_range("bytes=0-1,5-9", len), None);
        assert_eq!(parse_range("items=0-1", len), None);
        // Open-ended requests on a large clip get the whole rest (Android's
        // player reads only what one answer says it holds).
        let big = 100 * 1024 * 1024;
        assert_eq!(parse_range("bytes=0-", big), Some(Ok((0, big - 1))));
    }
}

#[cfg(test)]
mod origin_tests {
    use super::*;

    fn req(version: axum::http::Version, uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut b = axum::http::Request::builder().version(version).uri(uri).method("POST");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    fn allowed(r: &Request) -> bool {
        same_origin(r.headers(), request_host(r).as_deref())
    }

    #[test]
    fn other_sites_are_refused() {
        let h1 = axum::http::Version::HTTP_11;
        assert!(allowed(&req(h1, "/api/x", &[("host", "nvr:8090")])));
        assert!(allowed(&req(h1, "/api/x", &[("host", "nvr:8090"), ("origin", "https://nvr:8090")])));
        assert!(allowed(&req(h1, "/api/x", &[("host", "NVR:8090"), ("origin", "https://nvr:8090")])));
        assert!(!allowed(&req(h1, "/api/x", &[("host", "nvr:8090"), ("origin", "https://evil.example")])));
        assert!(!allowed(&req(h1, "/api/x", &[("host", "nvr:8090"), ("origin", "null")])));
        assert!(!allowed(&req(h1, "/api/x", &[("origin", "https://nvr:8090")])));
        // HTTP/2 (browsers over HTTPS): no Host header, the address is in the URI.
        let h2 = axum::http::Version::HTTP_2;
        assert!(allowed(&req(h2, "https://nvr:8090/api/x", &[("origin", "https://nvr:8090")])));
        assert!(!allowed(&req(h2, "https://nvr:8090/api/x", &[("origin", "https://evil.example")])));
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn a_plug_password_is_kept_only_for_the_same_switch() {
        let cam = |host: &str, pw: &str| -> CameraConfig {
            toml::from_str(&format!(
                "name = \"c\"\nurl = \"rtsp://10.0.0.5/s\"\n[watchdog.power]\nkind = \"shelly\"\nhost = \"{host}\"\npassword = \"{pw}\"\n"
            ))
            .unwrap()
        };
        let pw = |c: &CameraConfig| c.watchdog.as_ref().unwrap().power.as_ref().unwrap().password.clone();
        let cur = cam("10.0.0.50", "secret");
        let mut same = cam("10.0.0.50", "");
        keep_power_password(&mut same, &cur);
        assert_eq!(pw(&same), "secret");
        let mut moved = cam("10.1.2.99", "");
        keep_power_password(&mut moved, &cur);
        assert_eq!(pw(&moved), "", "a new address must not get the old password");
    }

    #[test]
    fn private_switch_defaults_and_changes() {
        let dir = std::env::temp_dir().join(format!("amb-priv-{}", std::process::id()));
        let full = dir.join("full");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("a.txt"), "x").unwrap();
        let full3 = dir.join("full3");
        std::fs::create_dir_all(&full3).unwrap();
        std::fs::write(full3.join("c.txt"), "x").unwrap();
        let mk = |n: &str, p: &std::path::Path, v: Option<bool>| crate::config::FolderConfig { name: n.into(), path: p.into(), max_mb: 0, archive: false, private: v };
        let mut cur = StorageConfig::default();
        // An upgrade: the folder had no value and the page sends none back. Untouched.
        cur.folders.push(mk("old", &full, None));
        let send = |cur: &mut StorageConfig, f: Vec<crate::config::FolderConfig>| {
            let mut u: StorageUpdate = serde_json::from_value(serde_json::json!({ "folders": f })).unwrap();
            for n in u.folders.clone().unwrap() {
                u.defaults.insert(n.path.clone(), new_folder_default(&n.path));
            }
            apply_storage_update(cur, u);
        };
        let before = cur.folders.clone();
        send(&mut cur, vec![mk("old", &full, None)]);
        assert_eq!(cur.folders[0].private, None);
        assert!(privacy_changes(&before, &cur.folders).is_empty());
        // New folders: nothing there yet -> on; holds files -> off; empty -> on.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        send(&mut cur, vec![mk("old", &full, None), mk("fresh", &dir.join("nope"), None), mk("full3", &full3, None), mk("empty", &empty, None)]);
        let p = |c: &StorageConfig, n: &str| c.folders.iter().find(|f| f.name == n).unwrap().private;
        assert_eq!((p(&cur, "old"), p(&cur, "fresh"), p(&cur, "full3"), p(&cur, "empty")), (None, Some(true), Some(false), Some(true)));
        // An older page that leaves the value out keeps what was set.
        send(&mut cur, vec![mk("fresh", &dir.join("nope"), None)]);
        assert_eq!(p(&cur, "fresh"), Some(true));
        // Flipping: on is applied, off puts it back, and an untouched value does nothing.
        let on = vec![mk("old", &full, Some(true))];
        assert_eq!(privacy_changes(&before, &on), vec![(full.clone(), true)]);
        assert!(privacy_changes(&on, &on).is_empty());
        assert_eq!(privacy_changes(&on, &[mk("old", &full, Some(false))]), vec![(full.clone(), false)]);
        assert!(privacy_changes(&before, &[mk("old", &full, Some(false))]).is_empty());
        // ...but an unset folder switched off is opened if it is really private.
        assert_eq!(unset_to_off(&before, &[mk("old", &full, Some(false))]), vec![full.clone()]);
        assert!(unset_to_off(&before, &[mk("old", &full, None)]).is_empty());
        assert!(unset_to_off(&on, &[mk("old", &full, Some(false))]).is_empty());
        // A rename keeps the value, including no value.
        cur.folders = vec![mk("old", &full, None)];
        send(&mut cur, vec![mk("renamed", &full, None)]);
        assert_eq!(p(&cur, "renamed"), None);
        // A moved folder is a new folder: the carried value is replaced by the default rule,
        // and nothing is done to the old path.
        let full2 = dir.join("full2");
        std::fs::create_dir_all(&full2).unwrap();
        std::fs::write(full2.join("b.txt"), "x").unwrap();
        cur.folders = vec![mk("a", &full, Some(true))];
        let before = cur.folders.clone();
        send(&mut cur, vec![mk("a", &full2, None)]);
        assert_eq!(p(&cur, "a"), Some(false), "a folder with files does not start private");
        assert!(privacy_changes(&before, &cur.folders).is_empty());
        send(&mut cur, vec![mk("a", &full, Some(true))]);
        assert_eq!(p(&cur, "a"), Some(true), "a different value than the old path had is a choice, and is kept");
        cur.folders = vec![mk("a", &full, Some(true))];
        send(&mut cur, vec![mk("a", &empty, None)]);
        assert_eq!(p(&cur, "a"), Some(true));
        // A value sent with a moved folder is the person's choice, even if it equals the old one.
        cur.folders = vec![mk("a", &full, Some(true))];
        send(&mut cur, vec![mk("a", &full2, Some(true))]);
        assert_eq!(p(&cur, "a"), Some(true));
        // Where it cannot be made private, the default is off.
        let top = std::path::Path::new(if cfg!(windows) { r"C:\" } else { "/" });
        send(&mut cur, vec![mk("root", top, None)]);
        assert_eq!(p(&cur, "root"), Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storage_update_keeps_folders_and_data_dir() {
        let mut cur = StorageConfig::default();
        cur.folders.push(crate::config::FolderConfig { name: "nas".into(), path: "/mnt/nas".into(), max_mb: 10, archive: true, private: None });
        let before_dir = cur.data_dir.clone();
        // What the settings dialog sends: no folders, a data_dir string.
        let u: StorageUpdate = serde_json::from_value(serde_json::json!({
            "data_dir": "/somewhere/else", "segment_seconds": 7, "retention_days": 3, "max_mb": 99
        }))
        .unwrap();
        apply_storage_update(&mut cur, u);
        assert_eq!(cur.folders.len(), 1);
        assert_eq!(cur.data_dir, before_dir);
        assert_eq!((cur.segment_seconds, cur.retention_days, cur.max_mb), (7, 3, 99));
        assert_eq!(cur.min_free_percent, 10, "left out: unchanged");
        // Back to Automatic, keeping 15% free (never more than 90%).
        apply_storage_update(&mut cur, serde_json::from_value(serde_json::json!({ "max_mb": 0, "min_free_percent": 15 })).unwrap());
        assert_eq!((cur.max_mb, cur.min_free_percent), (0, 15));
        apply_storage_update(&mut cur, serde_json::from_value(serde_json::json!({ "min_free_percent": 400 })).unwrap());
        assert_eq!(cur.min_free_percent, 90);
        let f = |n: &str, p: &str, a: bool| crate::config::FolderConfig { name: n.into(), path: p.into(), max_mb: 0, archive: a, private: None };
        assert!(validate_folders(&[f("a", "/x", true), f("b", "/y", false)]).is_ok());
        assert!(validate_folders(&[f("", "/x", false)]).is_err());
        assert!(validate_folders(&[f("a", "", false)]).is_err());
        assert!(validate_folders(&[f("a", "/x", false), f("A", "/y", false)]).unwrap_err().contains("twice"));
        assert!(validate_folders(&[f("a", "/x", true), f("b", "/y", true)]).unwrap_err().contains("archive"));
        let used = vec![("a".to_string(), std::path::PathBuf::from("/x"), 5_000_000i64)];
        assert!(folders_keep_recordings(&used, &[f("a", "/x", false)]).is_ok());
        assert!(folders_keep_recordings(&used, &[]).unwrap_err().contains("still holds"));
        assert!(folders_keep_recordings(&used, &[f("a", "/z", false)]).unwrap_err().contains("path"));
        // Folders change only when sent explicitly.
        let u: StorageUpdate = serde_json::from_value(serde_json::json!({ "folders": [] })).unwrap();
        apply_storage_update(&mut cur, u);
        assert!(cur.folders.is_empty());
    }
}

/// `GET /api/profiles` -> `{ profiles: [names], active, scheduled, manual: {name, until}|null, ... }`.
async fn list_profiles(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(engine.profiles_json())
}

#[derive(serde::Deserialize)]
struct ProfileReq {
    /// Profile to switch to; null (or absent) returns to the schedule.
    #[serde(default)]
    name: Option<String>,
    /// Hold the manual choice this many minutes; null/absent = until changed.
    #[serde(default)]
    minutes: Option<f64>,
}

/// `POST /api/profile {"name": "Away", "minutes": 60}` (admin).
async fn set_profile(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, token: Option<axum::Extension<TokenUsed>>, Json(req): Json<ProfileReq>) -> Response {
    let mut minutes = req.minutes.filter(|m| m.is_finite() && *m > 0.0).map(|m| m.round().clamp(1.0, 525_600.0) as i64);
    // A device token holds a profile for a day at most, never until changed by hand.
    if token.is_some() {
        minutes = Some(minutes.unwrap_or(i64::from(TOKEN_HOLD_MINUTES)).min(i64::from(TOKEN_HOLD_MINUTES)));
    }
    let source = source_of(&token);
    let name = req.name.filter(|n| !n.is_empty());
    match engine.set_manual_profile(
        name.clone(),
        minutes,
        &caller.0,
        &source,
    ) {
        Ok(()) => {
            // (Only now: what set_manual_profile took is a profile that exists, or none.)
            if let Some(t) = &token {
                let wanted = name.unwrap_or_else(|| "(back to the schedule)".to_string());
                let (u, t2, w2) = (caller.0.clone(), t.0 .0.clone(), wanted.clone());
                crate::account_http::tell_gathered(
                    &engine,
                    format!("token-profile:{}:{}:{wanted}", caller.0, t.0 .0),
                    String::new(),
                    TOKEN_NOTE_WINDOW,
                    (format!("profile changed by a device token of {}", caller.0), format!("The device token \"{}\" of {} set the profile: {wanted}. If that was not meant to happen, revoke it in Users and set the profile again.", t.0 .0, caller.0)),
                    move |_, n| (format!("profile changed again by a device token of {u}"), format!("The device token \"{t2}\" of {u} set the profile {w2} {n} more time(s) in the last minute. If that was not meant to happen, revoke it in Users and set the profile again.")),
                );
            }
            Json(engine.profiles_json()).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[cfg(test)]
mod delivery_settings_tests {
    use super::*;

    fn upd(v: serde_json::Value) -> SettingsUpdate {
        serde_json::from_value(v).unwrap()
    }

    fn cfg() -> crate::config::Config {
        toml::from_str("[alerts]\nwebhook_url = \"http://hook.example/x\"\nlink_url = \"https://nvr.example/\"\n[digest]\nenabled = true\n[watchdog]\nminutes = 10\nmax_per_day = 3\n[push]\napp_hosts = [\"ntfy.sh\", \"ntfy.home.example:8080\"]\n").unwrap()
    }

    #[test]
    fn what_stops_or_redirects_delivery_takes_the_proof() {
        let c = cfg();
        for (v, field) in [
            (serde_json::json!({"alerts": {"link_url": "https://other.example"}}), "link_url"),
            (serde_json::json!({"alerts": {"link_url": null}}), "link_url"),
            (serde_json::json!({"alerts": {"system_alerts": false}}), "system_alerts"),
            (serde_json::json!({"alerts": {"system_alert_minutes": 1440}}), "system_alert_minutes"),
            (serde_json::json!({"alerts": {"pushover_sound": "none"}}), "pushover_sound"),
            (serde_json::json!({"push": {"app_hosts": ["ntfy.sh"]}}), "push.app_hosts"),
            (serde_json::json!({"push": {"app_hosts": []}}), "push.app_hosts"),
            (serde_json::json!({"digest": {"enabled": false}}), "digest.enabled"),
            (serde_json::json!({"digest": {"channels": ["email"]}}), "digest.channels"),
            (serde_json::json!({"digest": {"cameras": ["Door"]}}), "digest.cameras"),
            (serde_json::json!({"watchdog": {"enabled": true}}), "watchdog.enabled"),
            (serde_json::json!({"watchdog": {"minutes": 1440}}), "watchdog.minutes"),
            (serde_json::json!({"watchdog": {"max_per_day": 24}}), "watchdog.max_per_day"),
        ] {
            let u = upd(v);
            assert!(delivery_changes(&c, &u).contains(&field), "{field}");
            assert!(settings_need_proof(&c, &u), "{field}");
        }
    }

    #[test]
    fn saving_what_is_there_or_what_cannot_silence_takes_nothing() {
        let c = cfg();
        for v in [
            serde_json::json!({"alerts": {"link_url": "https://nvr.example"}}),
            serde_json::json!({"alerts": {"link_url": " https://nvr.example/ "}}),
            serde_json::json!({"alerts": {"system_alerts": null}}),
            serde_json::json!({"alerts": {"system_alerts": true}}),
            serde_json::json!({"alerts": {"system_alert_minutes": 5}}),
            serde_json::json!({"alerts": {"pushover_sound": ""}}),
            serde_json::json!({"alerts": {"webhook_url": "<set>"}}),
            serde_json::json!({"alerts": {"min_interval": 30}}),
            serde_json::json!({"push": {"app_hosts": ["ntfy.home.example:8080", "NTFY.sh"]}}),
            serde_json::json!({"push": {"app_hosts": ["ntfy.sh", "ntfy.home.example:8080", "ntfy.other.example"]}}),
            serde_json::json!({"digest": {"time": "08:00", "enabled": true}}),
            serde_json::json!({"digest": {"channels": null, "cameras": []}}),
            serde_json::json!({"watchdog": {"enabled": false, "minutes": 10, "max_per_day": 3}}),
            serde_json::json!({"watchdog": {"minutes": 5, "max_per_day": 1, "quiet_hours": "22:00-06:00"}}),
            serde_json::json!({"motion": {}}),
        ] {
            let u = upd(v.clone());
            assert!(delivery_changes(&c, &u).is_empty(), "{v}: {:?}", delivery_changes(&c, &u));
            assert!(!settings_need_proof(&c, &u), "{v}");
        }
        // Turned off in the first place: turning it on is no silencing.
        let mut off = cfg();
        off.digest.enabled = false;
        assert!(delivery_changes(&off, &upd(serde_json::json!({"digest": {"enabled": true}}))).is_empty());
    }

    #[test]
    fn a_bad_link_for_notifications_is_refused_before_anything_is_asked() {
        let cur = cfg().alerts;
        let bad = |u: &str| bad_link_url(&cur, &Some(serde_json::json!({"link_url": u})));
        assert!(bad("javascript:alert(1)") && bad("ftp://x.example") && bad("https://a@b.example"));
        assert!(!bad("https://other.example") && !bad("http://192.0.2.1:8090") && !bad(""));
        assert!(!bad_link_url(&cur, &Some(serde_json::json!({"link_url": null}))));
        // One that is there already (a file edited by hand) does not stop other saves.
        let odd = AlertConfig { link_url: Some("nvr.example".into()), ..Default::default() };
        assert!(!bad_link_url(&odd, &Some(serde_json::json!({"link_url": "nvr.example", "min_interval": 5}))));
    }
}

#[cfg(test)]
mod camera_merge_tests {
    use super::*;

    #[test]
    fn partial_camera_update_keeps_unsent_fields() {
        let cur: CameraConfig = toml::from_str(
            "name = \"Door\"\nurl = \"rtsp://h/main\"\nsub_url = \"rtsp://h/sub\"\nfolder = \"nas\"\nmin_object = 0.01\n",
        )
        .unwrap();
        // What an older page sends: no sub_url/folder; clears min_object.
        let m = merge_camera_json(&cur, serde_json::json!({"name": "Door", "url": "rtsp://h/main", "fps": 7, "min_object": null})).unwrap();
        assert_eq!(m.sub_url.as_deref(), Some("rtsp://h/sub"));
        assert_eq!(m.folder.as_deref(), Some("nas"));
        assert_eq!(m.fps, 7);
        assert_eq!(m.min_object, None);
        assert!(merge_camera_json(&cur, serde_json::json!({"fps": "fast"})).is_err());
    }

    #[test]
    fn named_zones_are_saved_merged_and_checked() {
        let cur: CameraConfig = toml::from_str("name = \"Door\"\nurl = \"rtsp://h/main\"\n").unwrap();
        assert!(cur.named_zones.is_empty());
        let body = serde_json::json!({"named_zones": [
            {"name": "Gate", "kind": "line", "points": [[0.2, 0.5], [0.8, 0.5]], "direction": "ab"},
            {"name": "Driveway", "kind": "area", "points": [[0.1, 0.1], [0.5, 0.1], [0.5, 0.6]], "seconds": 45, "labels": ["person", "car"], "alert": false}
        ]});
        let m = merge_camera_json(&cur, body).unwrap();
        assert_eq!(m.named_zones.len(), 2);
        // Defaults: on, person, 30 s, both, alert and record.
        let g = &m.named_zones[0];
        assert!(g.enabled && g.alert && g.record && g.labels == ["person"] && g.seconds == 30 && g.direction == "ab");
        assert!(!m.named_zones[1].alert);
        assert!(check_camera_fields(&m).is_ok());
        // A page that does not know them keeps them.
        let k = merge_camera_json(&m, serde_json::json!({"fps": 7})).unwrap();
        assert_eq!(k.named_zones.len(), 2);
        // Survives the config file.
        let t = toml::to_string(&k).unwrap();
        let back: CameraConfig = toml::from_str(&t).unwrap();
        assert_eq!(back.named_zones, k.named_zones);
        // Bad ones are refused.
        let bad = merge_camera_json(&m, serde_json::json!({"named_zones": [{"name": "A", "kind": "line", "points": [[0.2, 0.5]]}]})).unwrap();
        assert!(check_camera_fields(&bad).is_err());
        let clear = merge_camera_json(&m, serde_json::json!({"named_zones": []})).unwrap();
        assert!(clear.named_zones.is_empty());
    }
}

#[cfg(test)]
mod alert_settings_tests {
    use super::*;

    #[test]
    fn secrets_masked_and_kept_partial_updates_merge() {
        let cur: AlertConfig = toml::from_str(
            "telegram_bot_token = \"123:abc\"\nsmtp_password = \"pw\"\nmqtt_url = \"mqtt://u:p@broker\"\nsmtp_host = \"mail\"\nwebhook_url = \"http://hook\"\n",
        )
        .unwrap();
        let m = mask_alert_secrets(&cur);
        assert_eq!(m.telegram_bot_token.as_deref(), Some("<set>"));
        assert_eq!(m.smtp_password.as_deref(), Some("<set>"));
        assert_eq!(m.mqtt_url.as_deref(), Some("<set>"));
        // The dialog sends back what it knows: masked secrets, a new webhook,
        // nothing about SMTP host or MQTT.
        let out = merge_alerts(&cur, serde_json::json!({"telegram_bot_token": "<set>", "smtp_password": "", "webhook_url": "http://new"})).unwrap();
        assert_eq!(out.telegram_bot_token.as_deref(), Some("123:abc"));
        assert_eq!(out.smtp_password.as_deref(), Some("pw"));
        assert_eq!(out.mqtt_url.as_deref(), Some("mqtt://u:p@broker"));
        assert_eq!(out.smtp_host.as_deref(), Some("mail"));
        assert_eq!(out.webhook_url.as_deref(), Some("http://new"));
    }

    #[test]
    fn event_query_flag_accepts_1_and_true() {
        for (q, want) in [("all=1", true), ("all=true", true), ("all=0", false), ("all=false", false), ("", false)] {
            let uri: axum::http::Uri = format!("/api/events?{q}").parse().unwrap();
            let e = axum::extract::Query::<EventQuery>::try_from_uri(&uri).unwrap().0;
            assert_eq!(e.all, want, "{q}");
        }
        let bad: axum::http::Uri = "/api/events?all=maybe".parse().unwrap();
        assert!(axum::extract::Query::<EventQuery>::try_from_uri(&bad).is_err());
    }

    #[test]
    fn webhook_urls_masked_and_kept() {
        let cur: AlertConfig = toml::from_str("webhook_url = \"http://hook/secret\"\nimage_post_url = \"http://img/secret\"\n").unwrap();
        let m = mask_alert_secrets(&cur);
        assert_eq!(m.webhook_url.as_deref(), Some("<set>"));
        assert_eq!(m.image_post_url.as_deref(), Some("<set>"));
        for sent in [serde_json::json!("<set>"), serde_json::json!(""), serde_json::json!("  ")] {
            let out = merge_alerts(&cur, serde_json::json!({"webhook_url": sent.clone(), "image_post_url": sent})).unwrap();
            assert_eq!(out.webhook_url.as_deref(), Some("http://hook/secret"));
            assert_eq!(out.image_post_url.as_deref(), Some("http://img/secret"));
        }
    }

    #[test]
    fn viewer_stats_lack_machine_details() {
        let mut j = serde_json::json!({
            "gpu": {"util": 1}, "net": {"rx_bps": 1}, "ai": {"state": "ok"}, "fps": {"total": 1},
            "storage": [{"name": "d", "path": "/x", "write_bps": 5, "tier": "hot", "used": 1}],
            "history": {"cpu": [1], "ai_p50": [1], "ai_checks": [1], "gpu": [1], "rx": [1], "tx": [1], "writes": {}},
            "history_1h": {"cpu": [1], "ai_p50": [1], "writes": {"a": [1]}},
        });
        strip_stats_for_viewer(&mut j);
        for k in ["gpu", "net", "ai"] {
            assert!(j.get(k).is_none(), "{k} leaked");
        }
        assert!(j["fps"].is_object());
        assert!(j["storage"][0].get("path").is_none() && j["storage"][0].get("write_bps").is_none() && j["storage"][0].get("tier").is_none());
        assert_eq!(j["storage"][0]["used"], 1);
        for h in ["history", "history_1h"] {
            for k in ["ai_p50", "ai_checks", "gpu", "rx", "tx", "writes"] {
                assert!(j[h].get(k).is_none(), "{h}.{k} leaked");
            }
            assert!(j[h].get("cpu").is_some());
        }
    }
}

/// `GET /api/health` → cameras, disks and warnings.
async fn health(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let e = engine.clone();
    Json(tokio::task::spawn_blocking(move || e.health()).await.unwrap_or_default())
}

/// `GET /api/connections`: browsers and apps using this server in the last
/// hour, capture helpers and pushed streams.
async fn connections(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let names = |id: usize| engine.camera(id).map(|h| h.cfg.read().unwrap().name.clone());
    let pushes: Vec<serde_json::Value> = engine
        .config
        .lock()
        .unwrap()
        .cameras
        .iter()
        .filter_map(|c| {
            let key = crate::ingest::push::push_key(&c.url)?;
            let sender = crate::ingest::push::sender(key);
            Some(serde_json::json!({
                "camera": c.name,
                "helper": c.capture.as_ref().map(|h| h.helper.clone()),
                "from": sender.as_ref().map(|(_, f)| f.clone()),
                "seconds": sender.map(|(a, _)| a.as_secs()),
            }))
        })
        .collect();
    Json(serde_json::json!({
        "clients": crate::conns::list(&names),
        "transcodes": crate::transcode::status(),
        "helpers": crate::helpers::list(&engine)["helpers"],
        "pushes": pushes,
    }))
}

#[derive(serde::Deserialize)]
struct PlanQ {
    /// Work it out again now instead of using the last answer.
    #[serde(default)]
    force: bool,
}

/// `GET /api/storage/plan[?force=true]`: measured MB/day per camera and how many days fit
/// in each recording folder.
async fn storage_plan(State(engine): State<Arc<Engine>>, Query(q): Query<PlanQ>) -> Json<serde_json::Value> {
    let e = engine.clone();
    Json(tokio::task::spawn_blocking(move || e.storage_plan(q.force)).await.unwrap_or_default())
}

/// `GET /api/drives`: drives and mounted filesystems with their space, for
/// picking where a new recording folder goes.
async fn drives() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "drives": tokio::task::spawn_blocking(crate::engine::list_drives).await.unwrap_or_default() }))
}

/// `GET /api/storage/missing` (admins): what the start-up check noticed
/// (recording folders not there, or whose recordings are mostly missing),
/// the last dry run of "remove missing recordings" and what is running.
async fn missing_status(State(engine): State<Arc<Engine>>) -> Response {
    match tokio::task::spawn_blocking(move || crate::missing::status_json(&engine)).await {
        Ok(v) => Json(v).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "storage check error").into_response(),
    }
}

/// `POST /api/storage/missing/scan` (admins): start a dry run (read only);
/// its result appears in `GET /api/storage/missing`.
async fn missing_scan(State(engine): State<Arc<Engine>>, axum::Extension(IsAdmin(admin)): axum::Extension<IsAdmin>) -> Response {
    if !admin {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "admin access required" }))).into_response();
    }
    match crate::missing::start_scan(engine) {
        Ok(()) => (StatusCode::ACCEPTED, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(why) => (StatusCode::CONFLICT, Json(serde_json::json!({ "error": why }))).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct MissingApply {
    /// The dry run's id (what the admin looked at).
    scan_id: String,
    /// Folders to include although they need confirming (not there, not in
    /// the settings, or none of their recordings found); "" = data folder.
    #[serde(default)]
    include: Vec<String>,
}

/// `POST /api/storage/missing/apply` (admins): back up the database, then
/// remove the rows of the missing recordings found by dry run `scan_id`
/// (never files). Runs in the background; see `GET /api/storage/missing`.
async fn missing_apply(
    State(engine): State<Arc<Engine>>,
    axum::Extension(IsAdmin(admin)): axum::Extension<IsAdmin>,
    caller: Option<axum::Extension<Caller>>,
    peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    Json(b): Json<MissingApply>,
) -> Response {
    if !admin {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "admin access required" }))).into_response();
    }
    let who = format!("{} from {}", caller.map(|c| c.0 .0.clone()).unwrap_or_else(|| "?".into()), peer.map(|p| p.0.ip().to_string()).unwrap_or_else(|| "?".into()));
    match crate::missing::start_apply(engine, &b.scan_id, b.include, who) {
        Ok(()) => (StatusCode::ACCEPTED, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err((code, why)) => (StatusCode::from_u16(code).unwrap_or(StatusCode::CONFLICT), Json(serde_json::json!({ "error": why }))).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PrivacyQ {
    /// A folder being added in Settings, not saved yet.
    #[serde(default)]
    path: Option<String>,
}

/// `GET /api/storage/privacy`: whether each recording folder is private to
/// administrators and AmberSight right now (state: private, open, applying,
/// failed, refused, unsupported, missing, with a detail for the last four).
/// `program` and `data` are read-only lines for the program folder (Windows)
/// and the data folder: `{path, state, detail}`, state also `skipped` and `na`.
/// With `?path=` also what a new folder there would get: `{exists, empty,
/// default_private, state, detail}`.
async fn storage_privacy(State(engine): State<Arc<Engine>>, Query(q): Query<PrivacyQ>) -> Json<serde_json::Value> {
    let folders = engine.config.lock().unwrap().storage.folders.clone();
    let data_dir = engine.data_dir.clone();
    let out = tokio::task::spawn_blocking(move || {
        // Each folder is looked at on its own thread, all together, and the answer waits for
        // none of them longer than FOLDER_WAIT: a mapped drive to a NAS that is off can take
        // minutes to say no.
        let deadline = std::time::Instant::now() + FOLDER_WAIT;
        let state_of = |p: std::path::PathBuf, kind: crate::lockdown::Kind| {
            let q = p.clone();
            BoundedFolder::start(p, move || {
                let (state, detail) = crate::lockdown::kind_state(&q, kind);
                serde_json::json!({ "state": state, "detail": detail })
            })
        };
        let pending: Vec<_> = folders.iter().map(|f| state_of(std::path::PathBuf::from(&f.path), crate::lockdown::Kind::Recording)).collect();
        let program = std::env::current_exe().ok().and_then(|e| e.parent().map(std::path::Path::to_path_buf));
        let program_p = program.map(|p| (p.clone(), state_of(p, crate::lockdown::Kind::Program)));
        let data_p = (data_dir.clone(), state_of(data_dir, crate::lockdown::Kind::Data));
        let probe_p = q.path.filter(|p| !p.trim().is_empty()).map(|p| {
            let p = std::path::PathBuf::from(p.trim());
            let q = p.clone();
            BoundedFolder::start(p, move || {
                let (exists, empty) = folder_contents(&q);
                let (state, detail) = crate::lockdown::folder_state(&q);
                serde_json::json!({ "exists": exists, "empty": empty, "default_private": crate::lockdown::default_private_for_new(exists, !empty), "state": state, "detail": detail })
            })
        });
        let list: Vec<_> = folders
            .iter()
            .zip(pending)
            .map(|(f, p)| {
                let v = p.finish(deadline);
                serde_json::json!({ "name": f.name, "private": f.private, "state": v["state"], "detail": v["detail"] })
            })
            .collect();
        let line = |p: (std::path::PathBuf, BoundedFolder)| {
            let v = p.1.finish(deadline);
            serde_json::json!({ "path": p.0.to_string_lossy(), "state": v["state"], "detail": v["detail"] })
        };
        let program_line = program_p.map(line);
        let data_line = Some(line(data_p));
        let probe = probe_p.map(|p| p.finish(deadline));
        serde_json::json!({ "folders": list, "probe": probe, "program": program_line, "data": data_line })
    })
    .await
    .unwrap_or_default();
    Json(out)
}

/// The longest the folder answer waits for a drive.
const FOLDER_WAIT: std::time::Duration = std::time::Duration::from_secs(4);

/// Paths whose look-up did not come back in time and is still running: not started again
/// (one stuck thread per path, however often the page asks), answered "unknown" at once.
fn stuck_folders() -> &'static Mutex<std::collections::HashSet<std::path::PathBuf>> {
    static S: std::sync::OnceLock<Mutex<std::collections::HashSet<std::path::PathBuf>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

enum BoundedFolder {
    /// An earlier look-up of this path is still stuck.
    Stuck,
    Running { path: std::path::PathBuf, rx: std::sync::mpsc::Receiver<serde_json::Value>, done: Arc<std::sync::atomic::AtomicBool> },
}

impl BoundedFolder {
    fn start(path: std::path::PathBuf, work: impl FnOnce() -> serde_json::Value + Send + 'static) -> BoundedFolder {
        use std::sync::atomic::{AtomicBool, Ordering};
        if stuck_folders().lock().unwrap().contains(&path) {
            return BoundedFolder::Stuck;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        let (d, p) = (done.clone(), path.clone());
        let spawned = std::thread::Builder::new().name("folder-state".into()).spawn(move || {
            let v = work();
            let _ = tx.send(v);
            d.store(true, Ordering::SeqCst);
            stuck_folders().lock().unwrap().remove(&p);
        });
        if spawned.is_err() {
            return BoundedFolder::Stuck;
        }
        BoundedFolder::Running { path, rx, done }
    }

    /// The answer, or `{"state": "unknown"}` if it is not there by `deadline`.
    fn finish(self, deadline: std::time::Instant) -> serde_json::Value {
        let unknown = || serde_json::json!({ "state": "unknown", "detail": "no answer from this folder yet: the drive or network share may be off or slow", "exists": false, "empty": true, "default_private": false });
        match self {
            BoundedFolder::Stuck => unknown(),
            BoundedFolder::Running { path, rx, done } => match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(v) => v,
                Err(_) => {
                    // (Marked under the lock the thread takes to clear it, so a thread that
                    // finishes just now cannot leave the mark behind.)
                    let mut stuck = stuck_folders().lock().unwrap();
                    if !done.load(std::sync::atomic::Ordering::SeqCst) {
                        stuck.insert(path);
                    }
                    unknown()
                }
            },
        }
    }
}

/// (is there such a folder, does it hold nothing).
fn folder_contents(p: &std::path::Path) -> (bool, bool) {
    match std::fs::read_dir(p) {
        Ok(mut r) => (true, r.next().is_none()),
        Err(_) => (p.exists(), true),
    }
}

#[derive(serde::Deserialize)]
struct LogsQ {
    /// error | warn | info (default)
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /api/logs?level=warn&limit=200` → recent log lines, oldest first.
async fn logs(Query(q): Query<LogsQ>) -> Json<serde_json::Value> {
    let level = match q.level.as_deref() {
        Some("error") => tracing::Level::ERROR,
        Some("warn") => tracing::Level::WARN,
        _ => tracing::Level::INFO,
    };
    Json(serde_json::json!({ "lines": crate::logbuf::recent(level, q.limit.unwrap_or(300).min(2000)) }))
}

// ---------------------------------------------------------------------------
// ONVIF discovery and PTZ
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct DiscoverQuery {
    seconds: Option<u64>,
    /// Also scan this range directly, e.g. 192.168.1.0/24.
    subnet: Option<String>,
}

/// `GET /api/discover?seconds=3`: ONVIF devices answering a WS-Discovery
/// probe (admin only; no credentials involved).
async fn discover_devices(Query(q): Query<DiscoverQuery>) -> Response {
    let wait = std::time::Duration::from_secs(q.seconds.unwrap_or(3).clamp(1, 15));
    let hosts = match q.subnet.as_deref().filter(|s| !s.trim().is_empty()).map(crate::onvif_ptz::parse_subnets) {
        Some(Ok(h)) => h,
        Some(Err(e)) => return api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        None => Vec::new(),
    };
    let r = tokio::task::spawn_blocking(move || {
        let mut d = crate::onvif_ptz::discover(wait);
        let (more, rtsp) = if hosts.is_empty() { (Vec::new(), Vec::new()) } else { crate::onvif_ptz::discover_hosts(&hosts, wait) };
        for m in more {
            if !d.iter().any(|x| x.ip == m.ip) {
                d.push(m);
            }
        }
        (d, rtsp)
    })
    .await;
    match r {
        Ok((devices, rtsp_only)) => Json(serde_json::json!({ "devices": devices, "rtsp_only": rtsp_only })).into_response(),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "discovery failed"),
    }
}

/// `GET /api/devices` (admin): local capture devices and screens, as
/// `{video:[{name,url}], audio:[{name,url}], screen:[{name,url}], note?}`.
async fn local_devices() -> Response {
    match tokio::task::spawn_blocking(crate::local_source::list_devices).await {
        Ok(l) => Json(l).into_response(),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "device listing failed"),
    }
}

#[derive(serde::Deserialize)]
struct InspectReq {
    xaddr: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

/// `POST /api/discover/inspect`: identity, profiles, stream URLs and a
/// suggested camera config for one device. The password is never echoed.
#[derive(serde::Deserialize)]
struct PreviewReq {
    url: String,
    #[serde(default)]
    sub_url: Option<String>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

/// The address a found camera's picture is read from: its small stream if it has one.
fn preview_source(req: &PreviewReq) -> &str {
    req.sub_url.as_deref().map(str::trim).filter(|u| !u.is_empty()).unwrap_or(req.url.trim())
}

/// How long a sign-in with a saved login lets the picture use it on that host.
const SAVED_LOGIN_FOR: std::time::Duration = std::time::Duration::from_secs(3600);
const SAVED_LOGIN_HOSTS: usize = 256;
/// (host, username, when): where a saved Discovery login signed in lately.
static SAVED_LOGIN_AT: std::sync::Mutex<Vec<(String, String, std::time::Instant)>> = std::sync::Mutex::new(Vec::new());

fn saved_login_host(url: &str) -> Option<String> {
    crate::onvif::host_from_rtsp_url(url).map(|h| h.to_ascii_lowercase())
}

fn note_saved_login_worked(host: &str, username: &str) {
    let mut l = SAVED_LOGIN_AT.lock().unwrap_or_else(|e| e.into_inner());
    l.retain(|e| e.2.elapsed() < SAVED_LOGIN_FOR && !(e.0 == host && e.1 == username));
    l.push((host.to_string(), username.to_string(), std::time::Instant::now()));
    if l.len() > SAVED_LOGIN_HOSTS {
        l.remove(0);
    }
}

fn saved_login_worked(host: &str, username: &str) -> bool {
    SAVED_LOGIN_AT.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|e| e.2.elapsed() < SAVED_LOGIN_FOR && e.0 == host && e.1 == username)
}

/// The page never has a saved login's password: fill it in here. Only for a
/// host that login has just signed in to while looking for cameras: asked for
/// any other address, it goes nowhere, and that camera is not tried at all (a
/// try with no password would count as a failed sign-in).
fn use_saved_login(req: &mut PreviewReq, saved: &[crate::config::LoginPair], worked: impl Fn(&str, &str) -> bool) -> Result<(), &'static str> {
    if req.username.is_empty() || !req.password.is_empty() {
        return Ok(());
    }
    let Some(s) = saved.iter().find(|s| s.username == req.username && !s.password.is_empty()) else { return Ok(()) };
    let host = saved_login_host(preview_source(req)).ok_or("not a camera address")?;
    if !worked(&host, &req.username) {
        return Err("this saved login has not signed in to that camera");
    }
    req.password = s.password.clone();
    Ok(())
}

/// Where to read a found camera's picture from, with the login in the address,
/// for ffmpeg. RTSP addresses only.
fn preview_input(req: &PreviewReq) -> Result<String, &'static str> {
    let from = preview_source(req);
    let mut u = url::Url::parse(from).map_err(|_| "not a camera address")?;
    if !matches!(u.scheme(), "rtsp" | "rtsps") || u.host_str().is_none_or(str::is_empty) {
        return Err("only RTSP cameras can show a picture here");
    }
    if !req.username.is_empty() {
        u.set_username(&req.username).map_err(|_| "bad user name")?;
        u.set_password(Some(&req.password)).map_err(|_| "bad password")?;
    }
    Ok(u.to_string())
}

/// One frame from a found camera, as a small JPEG, so people can tell their
/// cameras apart before adding them. `POST /api/discover/preview {url, sub_url?,
/// username, password}` (admin). A blank password = the saved one, for a camera
/// that login signed in to. A few at a time; gives up after 12 seconds.
async fn discover_preview(State(engine): State<Arc<Engine>>, Json(mut req): Json<PreviewReq>) -> Response {
    static AT_ONCE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(3);
    let saved = engine.config.lock().unwrap().discovery_logins.clone();
    if let Err(e) = use_saved_login(&mut req, &saved, saved_login_worked) {
        return api_error(StatusCode::FORBIDDEN, e);
    }
    let input = match preview_input(&req) {
        Ok(u) => u,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e),
    };
    let Ok(_slot) = AT_ONCE.acquire().await else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "busy");
    };
    let frame = tokio::task::spawn_blocking(move || grab_preview(&input)).await.ok().flatten();
    match frame {
        Some(jpeg) => Response::builder()
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(jpeg))
            .unwrap(),
        // (No detail: the answer must never echo the address with its login.)
        None => api_error(StatusCode::BAD_GATEWAY, "the camera did not send a picture"),
    }
}

fn grab_preview(input: &str) -> Option<Vec<u8>> {
    use std::process::Stdio;
    let mut child = crate::child::ffmpeg()
        .args(["-hide_banner", "-loglevel", "quiet"])
        .args(if input.starts_with("rtsp") { &["-rtsp_transport", "tcp"][..] } else { &[][..] })
        .args(["-i", input, "-vf", "scale=320:-2", "-frames:v", "1", "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "6", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut b = Vec::new();
        let _ = (&mut out).take(4 << 20).read_to_end(&mut b);
        b
    });
    let until = std::time::Instant::now() + std::time::Duration::from_secs(12);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < until => std::thread::sleep(std::time::Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    reader.join().ok().filter(|b| b.len() > 100 && b.starts_with(&[0xff, 0xd8]))
}

async fn discover_inspect(Json(req): Json<InspectReq>) -> Response {
    let r = tokio::task::spawn_blocking(move || crate::onvif_ptz::inspect(&req.xaddr, &req.username, &req.password)).await;
    match r {
        Ok(Ok(i)) => Json(i).into_response(),
        Ok(Err(e)) => api_error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "inspect failed"),
    }
}

/// `GET /api/discover/watch` (admin): the other subnets Find cameras scans, the
/// hourly new-camera check (on/off), ignored addresses, and what the last
/// check found that is not a camera here yet.
async fn get_discovery_watch(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let d = engine.config.lock().unwrap().discovery.clone();
    let (found, last) = crate::engine::newcams::current(&engine);
    Json(serde_json::json!({ "ranges": d.ranges, "watch": d.watch, "ignored": d.ignored, "found": found, "last_check": if last > 0 { Some(last) } else { None } }))
}

#[derive(serde::Deserialize)]
struct WatchReq {
    /// Ranges as typed ("192.168.1.0/24, 10.0.20.0/24").
    ranges: Option<String>,
    watch: Option<bool>,
}

/// `PUT /api/discover/watch {ranges?, watch?}` (admin).
async fn put_discovery_watch(State(engine): State<Arc<Engine>>, Json(req): Json<WatchReq>) -> Response {
    if let Some(r) = req.ranges.as_deref() {
        if let Err(e) = crate::onvif_ptz::parse_subnets(r) {
            return api_error(StatusCode::BAD_REQUEST, &format!("{e:#}"));
        }
    }
    let mut c = engine.config.lock().unwrap();
    let was = c.discovery.clone();
    if let Some(r) = req.ranges.as_deref() {
        c.discovery.ranges = crate::onvif_ptz::split_ranges(r);
    }
    if let Some(w) = req.watch {
        c.discovery.watch = w;
    }
    if c.discovery == was {
        return Json(serde_json::json!({ "saved": false })).into_response();
    }
    if let Err(e) = c.save(&engine.config_path) {
        c.discovery = was;
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("could not save: {e:#}"));
    }
    if c.discovery.watch && (!was.watch || c.discovery.ranges != was.ranges) {
        crate::engine::newcams::request();
    }
    Json(serde_json::json!({ "saved": true })).into_response()
}

#[derive(serde::Deserialize)]
struct IgnoreReq {
    ip: String,
    #[serde(default = "yes")]
    ignore: bool,
}

/// `POST /api/discover/ignore {ip, ignore}` (admin): stop (or start again)
/// reporting a found device that is not a camera to add.
async fn discovery_ignore(State(engine): State<Arc<Engine>>, Json(req): Json<IgnoreReq>) -> Response {
    let ip = req.ip.trim().to_string();
    if ip.parse::<std::net::IpAddr>().is_err() {
        return api_error(StatusCode::BAD_REQUEST, "not an IP address");
    }
    let mut c = engine.config.lock().unwrap();
    let was = c.discovery.ignored.clone();
    c.discovery.ignored.retain(|x| *x != ip);
    if req.ignore {
        c.discovery.ignored.push(ip);
    }
    if c.discovery.ignored == was {
        return Json(serde_json::json!({ "saved": false })).into_response();
    }
    if let Err(e) = c.save(&engine.config_path) {
        c.discovery.ignored = was;
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("could not save: {e:#}"));
    }
    Json(serde_json::json!({ "saved": true })).into_response()
}

/// Saved Discovery logins: usernames only (passwords never leave the server).
async fn get_discovery_logins(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let c = engine.config.lock().unwrap();
    Json(serde_json::json!({ "logins": c.discovery_logins.iter().map(|l| serde_json::json!({ "username": l.username, "has_password": !l.password.is_empty() })).collect::<Vec<_>>() }))
}

#[derive(serde::Deserialize)]
struct LoginsReq {
    logins: Vec<crate::config::LoginPair>,
}

/// `PUT /api/discover/logins {"logins":[{username,password}]}` (admin). A blank
/// password for a username already saved keeps the saved one.
async fn put_discovery_logins(State(engine): State<Arc<Engine>>, Json(req): Json<LoginsReq>) -> Response {
    let mut c = engine.config.lock().unwrap();
    let old = c.discovery_logins.clone();
    let mut out: Vec<crate::config::LoginPair> = Vec::new();
    for l in req.logins.into_iter().take(MAX_AUTO_LOGINS) {
        let username = l.username.trim().to_string();
        if username.is_empty() || out.iter().any(|x| x.username == username) {
            continue;
        }
        let password = if l.password.is_empty() { old.iter().find(|o| o.username == username).map(|o| o.password.clone()).unwrap_or_default() } else { l.password };
        out.push(crate::config::LoginPair { username, password });
    }
    c.discovery_logins = out;
    if let Err(e) = c.save(&engine.config_path) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("could not save: {e:#}"));
    }
    Json(serde_json::json!({ "saved": c.discovery_logins.len() })).into_response()
}

/// Cameras lock their login after a few wrong passwords: try at most this many.
const MAX_AUTO_LOGINS: usize = 2;

#[derive(serde::Deserialize)]
struct AutoLoginReq {
    xaddr: String,
    /// Logins typed on the page (tried first); blank password = the saved one.
    #[serde(default)]
    logins: Vec<crate::config::LoginPair>,
}

/// Whether an ONVIF error means "wrong username or password".
fn is_auth_error(e: &anyhow::Error) -> bool {
    let t = format!("{e:#}").to_ascii_lowercase();
    t.contains("401") || t.contains("not authorized") || t.contains("notauthorized") || t.contains("unauthorized")
}

/// `POST /api/discover/autologin {xaddr, logins}`: sign in to a found device
/// with each login in turn (at most MAX_AUTO_LOGINS, stopping at the first
/// that works or at any error that is not a wrong login) and read it.
async fn discover_autologin(State(engine): State<Arc<Engine>>, Json(req): Json<AutoLoginReq>) -> Response {
    let saved = engine.config.lock().unwrap().discovery_logins.clone();
    let mut tries: Vec<crate::config::LoginPair> = Vec::new();
    for l in req.logins.iter().chain(saved.iter()) {
        let username = l.username.trim().to_string();
        if username.is_empty() || tries.iter().any(|t| t.username == username) {
            continue;
        }
        let password = if l.password.is_empty() { saved.iter().find(|s| s.username == username).map(|s| s.password.clone()).unwrap_or_default() } else { l.password.clone() };
        tries.push(crate::config::LoginPair { username, password });
    }
    tries.truncate(MAX_AUTO_LOGINS);
    if tries.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "no login to try");
    }
    let xaddr = req.xaddr.clone();
    let r = tokio::task::spawn_blocking(move || {
        let mut last = None;
        for (i, l) in tries.iter().enumerate() {
            match crate::onvif_ptz::inspect(&xaddr, &l.username, &l.password) {
                Ok(ins) => {
                    // The picture of this camera may use the saved password now.
                    if saved.iter().any(|s| s.username == l.username && s.password == l.password && !s.password.is_empty()) {
                        if let Some(h) = saved_login_host(&xaddr) {
                            note_saved_login_worked(&h, &l.username);
                        }
                    }
                    return Ok((i, l.username.clone(), ins));
                }
                Err(e) if is_auth_error(&e) => last = Some(e),
                Err(e) => return Err((false, e)),
            }
        }
        Err((true, last.unwrap_or_else(|| anyhow::anyhow!("no login worked"))))
    })
    .await;
    match r {
        Ok(Ok((i, username, ins))) => Json(serde_json::json!({ "ok": true, "login": i, "username": username, "inspection": ins })).into_response(),
        Ok(Err((auth, e))) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": false, "auth_failed": auth, "error": if auth { "None of the logins worked on this camera".to_string() } else { format!("{e:#}") } })),
        )
            .into_response(),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "sign-in check failed"),
    }
}

fn api_error(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn ptz_target(engine: &Engine, id: usize) -> Result<crate::onvif_ptz::PtzTarget, Response> {
    let h = engine.camera(id).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "no such camera"))?;
    let mut cfg = h.cfg.read().unwrap().clone();
    crate::engine::normalize_creds(&mut cfg);
    let cfg = crate::onvif_ptz::effective_cfg(id, &cfg);
    crate::onvif_ptz::target_from_config(&cfg).map_err(|e| api_error(StatusCode::BAD_REQUEST, format!("{e:#}")))
}

fn ptz_error(e: anyhow::Error) -> Response {
    if let Some(n) = e.downcast_ref::<crate::onvif_ptz::NoPtz>() {
        return api_error(StatusCode::BAD_REQUEST, n.to_string());
    }
    tracing::warn!("PTZ: {e:#}");
    api_error(StatusCode::BAD_GATEWAY, "the camera did not accept the PTZ request (details are in the server log)")
}

#[derive(serde::Deserialize, Default)]
struct TimeSyncReq {
    #[serde(default)]
    server: Option<String>,
}

/// `POST /api/cameras/:id/timesync {"server"?}` (admin): turn on the
/// camera's NTP time sync over ONVIF and set its clock now.
async fn camera_timesync(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>, bytes: axum::body::Bytes) -> Response {
    // (Bytes, not Option<Json>: a body that fails to arrive stops this, see auth_mw. Empty: no options.)
    let body = serde_json::from_slice::<TimeSyncReq>(&bytes).ok().map(Json);
    let Some(h) = engine.camera(id) else { return api_error(StatusCode::NOT_FOUND, "no such camera") };
    let mut cfg = h.cfg.read().unwrap().clone();
    crate::engine::normalize_creds(&mut cfg);
    let server = body
        .and_then(|b| b.0.server)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::timesync::DEFAULT_NTP.to_string());
    let name = cfg.name.clone();
    match tokio::task::spawn_blocking(move || crate::timesync::enable(&cfg, &server)).await {
        Ok(Ok(v)) => {
            tracing::info!("{name}: time sync turned on ({v})");
            Json(v).into_response()
        }
        Ok(Err(e)) => api_error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /api/timesync/all`: turn on time sync (NTP) and set the clock of
/// every enabled camera at once (see `timesync::enable`); one result each:
/// `{results: [{camera, ok, offset_after_s?, error?}]}`.
async fn timesync_all(State(engine): State<Arc<Engine>>) -> Response {
    let cams: Vec<crate::config::CameraConfig> = engine
        .cameras
        .read()
        .unwrap()
        .iter()
        .flatten()
        .filter(|h| h.active.load(std::sync::atomic::Ordering::Relaxed))
        .map(|h| {
            let mut c = h.cfg.read().unwrap().clone();
            crate::engine::normalize_creds(&mut c);
            c
        })
        // Network cameras only (not a screen, a phone or another server's camera).
        .filter(|c| c.url.starts_with("rtsp") || c.url.starts_with("http"))
        .collect();
    let results = tokio::task::spawn_blocking(move || {
        std::thread::scope(|s| {
            let jobs: Vec<_> = cams
                .iter()
                .map(|c| s.spawn(move || (c.name.clone(), crate::timesync::enable(c, crate::timesync::DEFAULT_NTP))))
                .collect();
            jobs.into_iter()
                .filter_map(|j| j.join().ok())
                .map(|(name, r)| match r {
                    Ok(v) => {
                        tracing::info!("{name}: time sync turned on ({v})");
                        serde_json::json!({ "camera": name, "ok": true, "offset_after_s": v.get("offset_after_s") })
                    }
                    Err(e) => serde_json::json!({ "camera": name, "ok": false, "error": format!("{e:#}") }),
                })
                .collect::<Vec<_>>()
        })
    })
    .await
    .unwrap_or_default();
    Json(serde_json::json!({ "results": results })).into_response()
}

/// `POST /api/cameras/:id/reboot` (admin): restart the camera now through
/// its own interface (ONVIF, Reolink, Dahua/Amcrest or Hikvision).
async fn camera_reboot(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>, axum::Extension(caller): axum::Extension<Caller>) -> Response {
    if engine.camera(id).is_none() {
        return api_error(StatusCode::NOT_FOUND, "no such camera");
    }
    match tokio::task::spawn_blocking(move || engine.reboot_camera_now(id, &caller.0)).await {
        Ok(Ok(how)) => Json(serde_json::json!({ "ok": true, "method": how })).into_response(),
        Ok(Err(e)) => api_error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(serde::Deserialize, Default)]
struct PowerReq {
    /// Settings from the camera window (maybe not saved yet); unset = saved ones.
    #[serde(default)]
    power: Option<crate::config::PowerSwitch>,
}

/// `POST /api/cameras/:id/powercycle {"power"?}` (admin): turn the
/// camera's smart plug / PoE port off, wait, and on again.
async fn camera_powercycle(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>, axum::Extension(caller): axum::Extension<Caller>, bytes: axum::body::Bytes) -> Response {
    // (Bytes, not Option<Json>: a body that fails to arrive stops this, see auth_mw. Empty: no options.)
    let body = serde_json::from_slice::<PowerReq>(&bytes).ok().map(Json);
    let Some(h) = engine.camera(id) else { return api_error(StatusCode::NOT_FOUND, "no such camera") };
    let mut power = body.and_then(|b| b.0.power);
    if let Some(p) = power.as_mut() {
        // Blank password: the stored one (the page never has it).
        if p.password.is_empty() {
            let mut tmp = h.cfg.read().unwrap().clone();
            tmp.watchdog = Some(crate::config::CameraWatchdog { power: Some(p.clone()), ..Default::default() });
            keep_power_password(&mut tmp, &h.cfg.read().unwrap());
            *p = tmp.watchdog.and_then(|w| w.power).unwrap_or_default();
        }
        if let Err(e) = crate::camreboot::power_requests(p) {
            return api_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
        }
    }
    match tokio::task::spawn_blocking(move || engine.power_cycle_now(id, &caller.0, power)).await {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => api_error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(serde::Deserialize, Default)]
struct NetinfoQ {
    #[serde(default)]
    refresh: Option<String>,
}

/// `GET /api/cameras/:id/netinfo[?refresh=1]` (admin): the camera's network
/// facts (address, MAC, ports, stream path without sign-in, reachability,
/// maker/model). Never includes a user name or password.
async fn camera_netinfo(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>, Query(q): Query<NetinfoQ>, axum::Extension(admin): axum::Extension<IsAdmin>) -> Response {
    if !admin.0 {
        return api_error(StatusCode::FORBIDDEN, "admin access required");
    }
    let Some(h) = engine.camera(id) else { return api_error(StatusCode::NOT_FOUND, "no such camera") };
    let mut cfg = h.cfg.read().unwrap().clone();
    crate::engine::normalize_creds(&mut cfg);
    // (With the port the search found: it shows the maker and model too.)
    let cfg = crate::onvif_ptz::effective_cfg(id, &cfg);
    let (ingest, online) = {
        let st = h.state.read().unwrap();
        (st.ingest, st.online)
    };
    let refresh = q.refresh.as_deref().is_some_and(|v| !v.is_empty() && v != "0");
    match tokio::task::spawn_blocking(move || crate::netinfo::collect(id, &cfg, refresh, ingest, online)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /api/cameras/:id/watchdog` (admin): what the watchdog is doing for
/// this camera, and its recent restarts and power cycles.
async fn camera_watchdog(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>, axum::Extension(admin): axum::Extension<IsAdmin>) -> Response {
    if !admin.0 {
        return api_error(StatusCode::FORBIDDEN, "admin access required");
    }
    match engine.watchdog_status(id) {
        Some(v) => Json(v).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no such camera"),
    }
}

/// `GET /api/cameras/:id/tamper` (admin): the camera's tamper detection:
/// state ("off", "learning", "ok", "covered", "blurred", "moved"), its
/// reference pictures, the latest scores and recent alerts.
async fn tamper_status(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    match engine.tamper_status(id) {
        Some(v) => Json(v).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no such camera"),
    }
}

/// `POST /api/cameras/:id/tamper/reference` (admin): take the camera's
/// picture of this moment as its reference (day or night, as it shows).
async fn tamper_set_reference(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    match engine.tamper_set_reference(id) {
        Ok(light) => {
            Json(serde_json::json!({ "ok": true, "light": light.word() })).into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e),
    }
}

fn jpeg_reply(bytes: Option<Vec<u8>>) -> Response {
    match bytes {
        Some(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-cache")], b).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no such picture"),
    }
}

/// `GET /api/cameras/:id/tamper/ref/:light` (day | night): a reference picture.
async fn tamper_ref_picture(State(engine): State<Arc<Engine>>, AxPath((id, light)): AxPath<(usize, String)>) -> Response {
    let Some(light) = crate::tamper::Light::from_word(&light) else { return api_error(StatusCode::NOT_FOUND, "day or night") };
    jpeg_reply(engine.tamper_ref_jpeg(id, light))
}

/// `GET /api/cameras/:id/tamper/incidents/:incident/:which` (ref | cur): the
/// reference and the current picture an alert was raised with.
async fn tamper_incident_picture(State(engine): State<Arc<Engine>>, AxPath((id, incident, which)): AxPath<(usize, i64, String)>) -> Response {
    jpeg_reply(engine.tamper_incident_jpeg(id, incident, &which))
}

/// `GET /api/cameras/:id/unusual` (admin): the camera's unusual-activity
/// learning: state ("off", "learning", "ready"), days of history and what is
/// typical for each hour of the week.
async fn unusual_status(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    match tokio::task::spawn_blocking(move || engine.unusual_status(id)).await {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no such camera"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /api/cameras/:id/unusual/refresh` (admin): read the events not yet
/// in the history now (it is done hourly anyway); returns the new status.
async fn unusual_refresh(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    let r = tokio::task::spawn_blocking(move || {
        let name = engine.camera(id)?.running.read().unwrap().name.clone();
        engine.activity_ingest(&name);
        engine.unusual_status(id)
    })
    .await;
    match r {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no such camera"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /api/cameras/:id/spots` (admin): how many spots the camera's AI
/// remembers as fixed objects (`remembered`), the owner's "not a ..."
/// marks (`marked`) and those still being learned (`learning`); `on`: the
/// camera's "ignore objects that have not moved" (the remembered ones count
/// only while it is on; the marks always do).
async fn camera_spots(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    let Some(h) = engine.camera(id) else { return api_error(StatusCode::NOT_FOUND, "no such camera") };
    let cfg = h.cfg.read().unwrap().clone();
    let on = cfg.ai_ignore_parked.unwrap_or_else(|| engine.config.lock().unwrap().ai.ignore_parked);
    match tokio::task::spawn_blocking(move || engine.spot_counts(&cfg.name)).await {
        Ok(c) => Json(serde_json::json!({ "remembered": c.remembered, "marked": c.marked, "learning": c.learning, "on": on })).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /api/cameras/:id/spots/forget` (admin): forget them all, the marks too.
async fn forget_camera_spots(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    req_parts: axum::http::request::Parts,
    AxPath(id): AxPath<usize>,
) -> Response {
    let Some(h) = engine.camera(id) else { return api_error(StatusCode::NOT_FOUND, "no such camera") };
    let name = h.cfg.read().unwrap().name.clone();
    let ip = req_parts.extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string()).unwrap_or_default();
    let r = tokio::task::spawn_blocking(move || {
        let n = engine.forget_spots(&name);
        let _ = engine.db.insert_audit(&caller.0, &ip, &format!("forgot {n} remembered spot{}", if n == 1 { "" } else { "s" }), &name, 200);
        n
    })
    .await;
    match r {
        Ok(n) => Json(serde_json::json!({ "forgot": n })).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /api/cameras/:id/ptz/presets`
async fn ptz_presets(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    let t = match ptz_target(&engine, id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    match tokio::task::spawn_blocking(move || crate::onvif_ptz::ptz_presets(&t)).await {
        Ok(Ok(p)) => Json(serde_json::json!({ "presets": p })).into_response(),
        Ok(Err(e)) => ptz_error(e),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "ptz failed"),
    }
}

#[derive(serde::Deserialize)]
struct CapsQ {
    #[serde(default)]
    refresh: Option<String>,
}

/// The camera's capabilities, from the cache unless `refresh` (or the
/// connection settings changed / the last check failed a while ago).
async fn caps_entry(engine: &Arc<Engine>, id: usize, refresh: bool) -> Result<crate::onvif_ptz::CapsEntry, Response> {
    let h = engine.camera(id).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "no such camera"))?;
    let mut cfg = h.cfg.read().unwrap().clone();
    crate::engine::normalize_creds(&mut cfg);
    let cfg = crate::onvif_ptz::effective_cfg(id, &cfg);
    let fp = crate::onvif_ptz::fingerprint(&cfg);
    if !refresh {
        if let Some(e) = crate::onvif_ptz::cached_caps(id, &fp) {
            if e.error.is_none() || e.at.elapsed() < std::time::Duration::from_secs(60) {
                return Ok(e);
            }
        }
    }
    tokio::task::spawn_blocking(move || crate::onvif_ptz::check_caps(id, &cfg))
        .await
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "ptz failed"))
}

/// `GET /api/cameras/:id/ptz/caps[?refresh=1]` ->
/// `{ ptz, zoom, aim, presets: [{token, name}], error? }`
async fn ptz_caps(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>, Query(q): Query<CapsQ>) -> Response {
    let refresh = matches!(q.refresh.as_deref(), Some("1" | "true"));
    let e = match caps_entry(&engine, id, refresh).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    let mut presets: Vec<crate::onvif_ptz::Preset> = Vec::new();
    let mut error = e.error.clone();
    if e.caps.ptz {
        if let Ok(t) = ptz_target(&engine, id) {
            match tokio::task::spawn_blocking(move || {
                crate::onvif_ptz::run_with_timeout(std::time::Duration::from_secs(5), move || crate::onvif_ptz::ptz_presets(&t))
            })
            .await
            {
                Ok(Some(Ok(p))) => presets = p,
                Ok(Some(Err(err))) => error = Some(format!("{err:#}")),
                _ => error = Some("could not read presets in time".into()),
            }
        }
    }
    let mut v = serde_json::json!({
        "ptz": e.caps.ptz,
        "zoom": e.caps.zoom,
        "aim": e.caps.aim.is_some(),
        "pan_tilt": e.caps.pan_tilt,
        "presets": presets,
    });
    if let Some(err) = error {
        v["error"] = err.into();
    }
    Json(v).into_response()
}

#[derive(serde::Deserialize)]
struct AimReq {
    x: f64,
    y: f64,
    w: Option<f64>,
    h: Option<f64>,
}

/// `POST /api/cameras/:id/ptz/aim {x,y}` or `{x,y,w,h}` (picture fractions;
/// x,y = the point or the box center). Admin only (a write).
async fn ptz_aim(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>, Json(req): Json<AimReq>) -> Response {
    let e = match caps_entry(&engine, id, false).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    let size = match (req.w, req.h) {
        (Some(w), Some(h)) => Some((w, h)),
        (None, None) => None,
        _ => return api_error(StatusCode::BAD_REQUEST, "send both w and h for a box"),
    };
    let m = match crate::onvif_ptz::aim_move(&e.caps, req.x, req.y, size) {
        Ok(m) => m,
        Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
    };
    let t = match ptz_target(&engine, id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    match tokio::task::spawn_blocking(move || crate::onvif_ptz::ptz_relative(&t, &m)).await {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => ptz_error(e),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "ptz failed"),
    }
}

/// Body of `POST /api/cameras/:id/ptz`, selected by `action`.
#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum PtzReq {
    Move {
        #[serde(default)]
        pan: f64,
        #[serde(default)]
        tilt: f64,
        #[serde(default)]
        zoom: f64,
        timeout_ms: Option<u64>,
    },
    Absolute {
        #[serde(default)]
        pan: f64,
        #[serde(default)]
        tilt: f64,
        #[serde(default)]
        zoom: f64,
    },
    Stop,
    Status,
    Goto { preset: String },
    SetPreset { name: String },
    RemovePreset { preset: String },
    Tour { presets: Vec<String>, dwell_s: Option<u64> },
    StopTour,
}

/// `POST /api/cameras/:id/ptz`. A write, so the middleware makes it admin-only.
async fn ptz_action(State(engine): State<Arc<Engine>>, token: Option<axum::Extension<TokenUsed>>, axum::Extension(role): axum::Extension<Role>, AxPath(id): AxPath<usize>, Json(req): Json<PtzReq>) -> Response {
    use crate::onvif_ptz as p;
    // Moving is day-to-day; what stays set up on the camera is not.
    if token.is_some() && matches!(&req, PtzReq::SetPreset { .. } | PtzReq::RemovePreset { .. } | PtzReq::Tour { .. }) {
        return (StatusCode::FORBIDDEN, "sign in with a password for this").into_response();
    }
    if !role.is_admin() && matches!(&req, PtzReq::SetPreset { .. } | PtzReq::RemovePreset { .. }) {
        return (StatusCode::FORBIDDEN, "only an admin can save or delete presets").into_response();
    }
    let t = match ptz_target(&engine, id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if matches!(&req, PtzReq::Tour { presets, .. } if presets.is_empty()) {
        return api_error(StatusCode::BAD_REQUEST, "tour needs at least one preset");
    }
    let key = format!("cam{id}");
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let ok = serde_json::json!({ "ok": true });
        Ok(match req {
            PtzReq::Move { pan, tilt, zoom, timeout_ms } => {
                // Always bounded, so a lost Stop cannot leave the camera moving.
                let ms = timeout_ms.unwrap_or(500).clamp(100, 10_000);
                p::ptz_move(&t, pan, tilt, zoom, Some(std::time::Duration::from_millis(ms)))?;
                ok
            }
            PtzReq::Absolute { pan, tilt, zoom } => {
                p::ptz_absolute(&t, pan, tilt, zoom)?;
                ok
            }
            PtzReq::Stop => {
                p::stop_tour(&key);
                p::ptz_stop(&t)?;
                ok
            }
            PtzReq::Status => {
                let mut v = p::ptz_status(&t)?;
                v["tour"] = p::tour_running(&key).into();
                v
            }
            PtzReq::Goto { preset } => {
                p::ptz_goto(&t, &preset)?;
                ok
            }
            PtzReq::SetPreset { name } => serde_json::json!({ "ok": true, "token": p::ptz_set_preset(&t, &name)? }),
            PtzReq::RemovePreset { preset } => {
                p::ptz_remove_preset(&t, &preset)?;
                ok
            }
            PtzReq::Tour { presets, dwell_s } => {
                // Fail now (no PTZ, bad credentials) rather than in the background.
                p::ptz_presets(&t)?;
                let dwell = std::time::Duration::from_secs(dwell_s.unwrap_or(10).clamp(2, 3600));
                p::start_tour(&key, t, presets, dwell);
                ok
            }
            PtzReq::StopTour => serde_json::json!({ "ok": true, "was_running": p::stop_tour(&key) }),
        })
    })
    .await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => ptz_error(e),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "ptz failed"),
    }
}

#[derive(serde::Deserialize)]
struct TalkQ {
    /// Sample rate of the PCM16LE the browser sends (default 48000).
    #[serde(default)]
    rate: Option<u32>,
}

/// `GET /api/cameras/:id/talk?rate=48000` -> WebSocket carrying PCM16LE mono
/// from the browser, played out through the camera's audio backchannel.
async fn camera_talk(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<usize>,
    axum::Extension(access): axum::Extension<Access>,
    axum::Extension(caller): axum::Extension<Caller>,
    axum::Extension(admin): axum::Extension<IsAdmin>,
    axum::Extension(role): axum::Extension<Role>,
    token: Option<axum::Extension<TokenUsed>>,
    peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    Query(q): Query<TalkQ>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    // Admins, and operators with the talk right (never with a device token).
    let may = admin.0 || (role.is_operator() && role.may(Perm::TalkClip) && token.is_none());
    let ip = peer.map(|p| p.0.ip().to_string()).unwrap_or_default();
    crate::talk::handle(engine, id, access, caller, may, ip, q.rate, ws).await
}

#[cfg(test)]
mod event_id_tests {
    #[test]
    fn viewers_see_only_the_kind_of_source() {
        assert_eq!(super::viewer_url("rtsp://admin:pw@192.0.2.10:554/h264"), "rtsp://(hidden)");
        assert_eq!(super::viewer_url("push://abcdef"), "push://(hidden)");
        assert_eq!(super::viewer_url("no scheme"), "(hidden)");
    }

    #[test]
    fn camera_of_event_ids() {
        assert_eq!(super::event_camera_slug("front_door-1793547441"), Some("front_door"));
        assert_eq!(super::event_camera_slug("front_door-1793547441-pkg"), Some("front_door"));
        assert_eq!(super::event_camera_slug("front_door-1793547441-pkgoff"), Some("front_door"));
        assert_eq!(super::event_camera_slug("nodash"), None);
    }
}

#[cfg(test)]
mod event_filter_tests {
    use super::*;

    fn eq(f: impl Fn(&mut EventQuery)) -> EventQuery {
        let mut q = serde_urlencoded_empty();
        f(&mut q);
        q
    }
    fn serde_urlencoded_empty() -> EventQuery {
        EventQuery { offset: 0, limit: 100, camera: None, since: None, until: None, object: None, all: false, cameras: None, protect: None, objects: None, review: None, ids: None }
    }
    fn limited(names: &[&str]) -> Access {
        Access(Some(names.iter().map(|n| crate::config::slug(n)).collect()))
    }

    #[test]
    fn several_cameras_are_each_checked_and_forbidden_ones_dropped() {
        let acc = limited(&["Front", "Back"]);
        let vis = Some(vec!["Front".to_string(), "Back".to_string()]);
        let q = event_db_query(&eq(|q| q.cameras = Some("Front, Garage ,Back,".into())), &acc, vis.clone(), vec![], 50).unwrap();
        assert_eq!(q.cameras, ["Front", "Back"]);
        // Only forbidden names: nothing, not "everything".
        assert!(event_db_query(&eq(|q| q.cameras = Some("Garage".into())), &acc, vis.clone(), vec![], 50).is_none());
        // A single forbidden `camera=` as before.
        assert!(event_db_query(&eq(|q| q.camera = Some("Garage".into())), &acc, vis.clone(), vec![], 50).is_none());
        // No camera chosen: a limited caller gets their own cameras.
        let q = event_db_query(&eq(|_| {}), &acc, vis, vec![], 50).unwrap();
        assert_eq!(q.cameras, ["Front", "Back"]);
        // An admin / open server: no camera limit at all.
        let q = event_db_query(&eq(|_| {}), &Access::default(), None, vec![], 50).unwrap();
        assert!(q.cameras.is_empty());
        assert!(event_db_query(&eq(|_| {}), &limited(&[]), Some(vec![]), vec![], 50).is_none());
    }

    #[test]
    fn filters_parse() {
        let a = Access::default();
        let q = event_db_query(
            &eq(|q| {
                q.protect = Some("1".into());
                q.objects = Some("Person, vehicle".into());
                q.review = Some("ok,none,bogus".into());
                q.ids = Some("a-1,b_2,../x,c-3".into());
                q.offset = 7;
            }),
            &a, None, vec![], 25,
        )
        .unwrap();
        assert!(q.protected_only);
        assert_eq!(q.objects_any, ["Person", "vehicle"]);
        assert_eq!(q.review, ["ok", "none"]);
        assert_eq!(q.ids, ["a-1", "b_2", "c-3"]);
        assert_eq!((q.limit, q.offset), (25, 7));
        let q = event_db_query(&eq(|q| { q.protect = Some("0".into()); q.review = Some("any".into()); }), &a, None, vec![], 25).unwrap();
        assert!(!q.protected_only && q.review.is_empty());
        // At most EXPORT_IDS_MAX ids.
        let many = (0..500).map(|i| format!("e{i}")).collect::<Vec<_>>().join(",");
        let q = event_db_query(&eq(|q| q.ids = Some(many.clone())), &a, None, vec![], 25).unwrap();
        assert_eq!(q.ids.len(), EXPORT_IDS_MAX);
        // Ids asked for, none of them real (a mangled link): no events, never the newest.
        assert!(event_db_query(&eq(|q| q.ids = Some("abc.def".into())), &a, None, vec![], 1).is_none());
        let q = event_db_query(&eq(|q| q.ids = Some("abc.def,a-1".into())), &a, None, vec![], 1).unwrap();
        assert_eq!(q.ids, ["a-1"]);
    }

    #[test]
    fn review_request_reads_verdicts() {
        let r: ReviewReq = serde_json::from_str(r#"{"verdict":"false_alarm"}"#).unwrap();
        assert_eq!(r.verdict.as_deref(), Some("false_alarm"));
        assert!(serde_json::from_str::<ReviewReq>(r#"{"verdict":null}"#).unwrap().verdict.is_none());
        assert!(serde_json::from_str::<ReviewReq>("{}").unwrap().verdict.is_none());
    }
}

#[cfg(test)]
mod viewer_write_tests {
    use super::viewer_may_write;

    /// Every route of the API that no device token may use, at all. A route
    /// in neither this list nor `TOKEN_ROUTES` fails the test below.
    const TOKEN_REFUSED: &[&str] = &[
        "/cameras/:id",
        "/camera-defaults",
        "/cameras/:id/enable",
        "/cameras/:id/timesync",
        "/timesync/all",
        "/cameras/:id/reboot",
        "/cameras/:id/powercycle",
        "/cameras/:id/watchdog",
        "/cameras/:id/netinfo",
        "/cameras/:id/tamper",
        "/cameras/:id/tamper/reference",
        "/cameras/:id/tamper/ref/:light",
        "/cameras/:id/tamper/incidents/:incident/:which",
        "/cameras/:id/unusual",
        "/cameras/:id/unusual/refresh",
        "/cameras/:id/spots",
        "/cameras/:id/spots/forget",
        "/cameras/:id/onvif",
        "/cameras/:id/onvif/find",
        "/ui/prefs",
        "/ui/orders",
        "/ui/diag",
        "/ui/orders/:key",
        "/discover",
        "/devices",
        "/discover/inspect",
        "/discover/autologin",
        "/discover/preview",
        "/ai/model",
        "/alerts/system_test",
        "/helpers",
        "/helpers/pair",
        "/helpers/:id",
        "/helpers/:id/sources",
        "/helpers/:id/thumb",
        "/discover/logins",
        "/discover/watch",
        "/discover/ignore",
        "/cameras/:id/talk",
        "/noisy",
        "/noisy/:id/dismiss",
        "/stats",
        "/whoami",
        "/login",
        "/login/totp",
        "/pair",
        "/logout",
        "/audit",
        "/users",
        "/users/:name",
        "/users/:name/pair",
        "/users/:name/tokens",
        "/users/:name/tokens/:id",
        "/users/:name/two-step/off",
        "/me/password",
        "/me/account",
        "/me/sessions/sign-out-others",
        "/me/sessions/:id/sign-out",
        "/me/two-step/start",
        "/me/two-step/confirm",
        "/me/two-step/cancel",
        "/me/two-step/off",
        "/me/two-step/recovery",
        "/me/tokens",
        "/me/tokens/:id",
        "/push/key",
        "/push/subscribe",
        "/push/unsubscribe",
        "/push/devices",
        "/push/test",
        "/restream",
        "/presence",
        "/presence/:device_id",
        "/health",
        "/connections",
        "/links",
        "/links/code",
        "/links/probe",
        "/links/:id",
        "/links/:id/open",
        "/links/:id/stats",
        "/storage/plan",
        "/storage/privacy",
        "/storage/missing",
        "/storage/missing/scan",
        "/storage/missing/apply",
        "/drives",
        "/tls",
        "/addresses",
        "/logs",
        "/events/:id/ai",
        "/events/:id/track",
        "/signals/:name",
        "/events/:id/ai/recheck",
        "/events/:id/ai/:n",
        "/ai/reviews",
        "/export",
        "/timelapse",
        "/timelapse/:slug/:date",
        "/faces",
        "/faces/status",
        "/smart/status",
        "/sound/status",
        "/cameras/:id/sound",
        "/smart/search",
        "/smart/pic/:id",
        "/smart/follow",
        "/alert-rules",
        "/alert-rules/preview",
        "/alert-rules/:id",
        "/faces/models",
        "/faces/import",
        "/faces/:id/img",
        "/faces/:id/name",
        "/faces/:id",
        "/digest/preview",
        "/digest/send",
        "/digests",
        "/digests/:id",
        "/plates",
        "/plates/recent",
        "/plates/status",
        "/plates/models",
        "/plates/known",
        "/plates/known/:id",
        "/plates/:id/img",
        "/plates/:id",
        "/people",
        "/people/:id",
        "/settings",
        "/offsite",
        "/offsite/test",
        "/event-copy/test",
        "/offsite/run",
        "/update",
        "/update/check",
        "/update/install",
        "/system/restart",
    ];

    fn api_routes() -> Vec<(String, String)> {
        let src = include_str!("web.rs");
        let start = src.find("let api = Router::new()").unwrap();
        let end = start + src[start..].find(".layer(axum::middleware::from_fn_with_state").unwrap();
        src[start..end]
            .lines()
            .filter_map(|l| {
                let rest = l.trim().strip_prefix(".route(\"")?;
                let (route, how) = rest.split_once('"')?;
                Some((route.to_string(), how.to_string()))
            })
            .collect()
    }

    #[test]
    fn what_in_the_settings_takes_the_callers_own_password() {
        use super::{command_changes as ch, record_seconds, token_pause};
        let v = |x: serde_json::Value| Some(x);
        use serde_json::json as j;
        assert!(ch(None, &v(j!({"command": "/bin/evil"})), "command"), "a new one");
        assert!(ch(Some("/bin/a"), &v(j!({"command": "/bin/b"})), "command"), "a different one");
        assert!(ch(None, &v(j!({"command": 5})), "command"), "not text: no way round");
        assert!(!ch(Some("/bin/a"), &v(j!({"command": "/bin/a"})), "command"), "the same");
        assert!(!ch(Some("/bin/a"), &v(j!({"command": " /bin/a "})), "command"), "the same, with spaces");
        assert!(!ch(Some("/bin/a"), &v(j!({"command": "<set>"})), "command"), "masked");
        assert!(!ch(Some("/bin/a"), &v(j!({"command": ""})), "command"), "cleared");
        assert!(!ch(Some("/bin/a"), &v(j!({"command": null})), "command"), "cleared");
        assert!(!ch(Some("/bin/a"), &v(j!({"other": 1})), "command"), "left out");
        assert!(!ch(Some("/bin/a"), &None, "command"), "no section");
        // Caps.
        assert_eq!(record_seconds(i64::MAX), 3600);
        assert_eq!(record_seconds(i64::MIN), 5);
        assert_eq!(record_seconds(30), 30);
        assert_eq!(token_pause(None), Some(1440), "never until resumed");
        assert_eq!(token_pause(Some(100_000)), Some(1440));
        assert_eq!(token_pause(Some(60)), Some(60));
        assert_eq!(token_pause(Some(0)), Some(0), "resuming");
    }

    #[test]
    fn what_in_the_alert_settings_takes_the_callers_own_password() {
        use serde_json::json as j;
        let cur = crate::config::AlertConfig { webhook_url: Some("https://hook/secret".into()), ntfy_url: Some("https://ntfy/a".into()), smtp_password: Some("pw".into()), email_to: vec!["a@example.com".into()], ..Default::default() };
        let ch = |p: serde_json::Value| super::alert_destinations_changed(&cur, &Some(p));
        // Unchanged, masked, or not a destination.
        for p in [j!({}), j!({"webhook_url": "<set>"}), j!({"webhook_url": ""}), j!({"smtp_password": "<set>"}), j!({"ntfy_url": "https://ntfy/a"}), j!({"email_to": ["a@example.com"]}), j!({"min_interval": 9}), j!({"telegram_chat_id": ""}), j!({"ha_url": null}), j!({"action_command": "/bin/x"})] {
            assert!(ch(p.clone()).is_empty(), "{p}");
        }
        // Changed or cleared.
        for (p, k) in [(j!({"webhook_url": "https://evil/x"}), "webhook_url"), (j!({"webhook_url": null}), "webhook_url"), (j!({"ntfy_url": ""}), "ntfy_url"), (j!({"ntfy_url": "https://evil"}), "ntfy_url"), (j!({"telegram_api_url": "https://evil"}), "telegram_api_url"), (j!({"telegram_chat_id": "9"}), "telegram_chat_id"), (j!({"smtp_host": "evil"}), "smtp_host"), (j!({"smtp_port": 2525}), "smtp_port"), (j!({"smtp_password": "new"}), "smtp_password"), (j!({"email_to": []}), "email_to"), (j!({"email_to": ["evil@example.com"]}), "email_to"), (j!({"ha_url": "http://evil"}), "ha_url"), (j!({"pushover_user": "evil"}), "pushover_user"), (j!({"mqtt_url": "mqtt://evil"}), "mqtt_url"), (j!({"image_post_url": "https://evil"}), "image_post_url")] {
            assert_eq!(ch(p.clone()), vec![k], "{p}");
        }
        assert!(ch(j!(5)).is_empty(), "no merge: refused later");
    }

    /// Who gets past the sign-in layer, for every route and method of the API:
    /// "viewer" (everyone signed in), "operator:<right>" (operators with that
    /// right, and admins) or "admin". A route left out fails the test below,
    /// so none becomes an operator's (or a viewer's) without a decision.
    /// (Handlers refuse more: talk takes the talk right; presets are saved
    /// and deleted by admins only; an operator's camera edit is held to
    /// OPERATOR_CAMERA_FIELDS; netinfo, watchdog, sound meter and audit check
    /// for an admin themselves.)
    const ROLE_TABLE: &[(&str, &str, &str)] = &[
        ("/cameras", "GET", "viewer"),
        ("/cameras", "POST", "admin"),
        ("/cameras/:id", "PUT", "operator:camera_detection"),
        ("/camera-defaults", "GET", "viewer"),
        ("/cameras/:id", "DELETE", "admin"),
        ("/cameras/:id/enable", "POST", "admin"),
        ("/cameras/:id/ptz", "POST", "operator:ptz"),
        ("/cameras/:id/timesync", "POST", "admin"),
        ("/timesync/all", "POST", "admin"),
        ("/cameras/:id/reboot", "POST", "admin"),
        ("/cameras/:id/powercycle", "POST", "admin"),
        ("/cameras/:id/watchdog", "GET", "viewer"),
        ("/cameras/:id/netinfo", "GET", "viewer"),
        ("/cameras/:id/tamper", "GET", "admin"),
        ("/cameras/:id/tamper/reference", "POST", "admin"),
        ("/cameras/:id/tamper/ref/:light", "GET", "viewer"),
        ("/cameras/:id/tamper/incidents/:incident/:which", "GET", "viewer"),
        ("/cameras/:id/unusual", "GET", "admin"),
        ("/cameras/:id/unusual/refresh", "POST", "admin"),
        ("/cameras/:id/spots", "GET", "admin"),
        ("/cameras/:id/spots/forget", "POST", "admin"),
        ("/cameras/:id/ptz/presets", "GET", "viewer"),
        ("/cameras/:id/ptz/caps", "GET", "viewer"),
        ("/cameras/:id/onvif", "GET", "admin"),
        ("/cameras/:id/onvif/find", "POST", "admin"),
        ("/cameras/:id/ptz/aim", "POST", "operator:ptz"),
        ("/ui/prefs", "GET", "viewer"),
        ("/ui/prefs", "PUT", "viewer"),
        ("/ui/prefs", "PATCH", "viewer"),
        ("/ui/orders", "GET", "viewer"),
        ("/ui/diag", "POST", "viewer"),
        ("/ui/orders/:key", "PUT", "admin"),
        ("/discover", "GET", "admin"),
        ("/devices", "GET", "admin"),
        ("/discover/inspect", "POST", "admin"),
        ("/discover/autologin", "POST", "admin"),
        ("/discover/preview", "POST", "admin"),
        ("/ai/model", "GET", "viewer"),
        ("/ai/model", "POST", "admin"),
        ("/alerts/system_test", "POST", "admin"),
        ("/helpers", "GET", "admin"),
        ("/helpers/pair", "POST", "admin"),
        ("/helpers/:id", "PUT", "admin"),
        ("/helpers/:id", "DELETE", "admin"),
        ("/helpers/:id/sources", "POST", "admin"),
        ("/helpers/:id/thumb", "POST", "admin"),
        ("/discover/logins", "GET", "admin"),
        ("/discover/logins", "PUT", "admin"),
        ("/discover/watch", "GET", "admin"),
        ("/discover/watch", "PUT", "admin"),
        ("/discover/ignore", "POST", "admin"),
        ("/cameras/:id/talk", "GET", "viewer"),
        ("/shield", "POST", "operator:arm"),
        ("/cameras/:id/pause", "POST", "operator:arm"),
        ("/noisy", "GET", "admin"),
        ("/noisy/:id/dismiss", "POST", "admin"),
        ("/profiles", "GET", "viewer"),
        ("/profile", "POST", "operator:arm"),
        ("/clips/record", "POST", "operator:talk_clip"),
        ("/status", "GET", "viewer"),
        ("/stats", "GET", "viewer"),
        ("/whoami", "GET", "viewer"),
        ("/login", "POST", "admin"),
        ("/login/totp", "POST", "admin"),
        ("/pair", "POST", "admin"),
        ("/logout", "POST", "admin"),
        ("/audit", "GET", "viewer"),
        ("/users", "GET", "admin"),
        ("/users", "POST", "admin"),
        ("/users/:name", "DELETE", "admin"),
        ("/users/:name/pair", "POST", "admin"),
        ("/users/:name/tokens", "GET", "admin"),
        ("/users/:name/tokens/:id", "DELETE", "admin"),
        ("/users/:name/two-step/off", "POST", "admin"),
        ("/me/password", "POST", "viewer"),
        ("/me/account", "GET", "viewer"),
        ("/me/sessions/sign-out-others", "POST", "viewer"),
        ("/me/sessions/:id/sign-out", "POST", "viewer"),
        ("/me/two-step/start", "POST", "viewer"),
        ("/me/two-step/confirm", "POST", "viewer"),
        ("/me/two-step/cancel", "POST", "viewer"),
        ("/me/two-step/off", "POST", "viewer"),
        ("/me/two-step/recovery", "POST", "viewer"),
        ("/me/tokens", "POST", "viewer"),
        ("/me/tokens/:id", "DELETE", "viewer"),
        ("/push/key", "GET", "viewer"),
        ("/push/subscribe", "POST", "viewer"),
        ("/push/unsubscribe", "POST", "viewer"),
        ("/push/devices", "GET", "viewer"),
        ("/push/test", "POST", "viewer"),
        ("/restream", "GET", "admin"),
        ("/restream", "PUT", "admin"),
        ("/presence", "GET", "viewer"),
        ("/presence", "POST", "viewer"),
        ("/presence/:device_id", "DELETE", "viewer"),
        ("/health", "GET", "admin"),
        ("/connections", "GET", "admin"),
        ("/links", "GET", "viewer"),
        ("/links", "POST", "admin"),
        ("/links/code", "POST", "admin"),
        ("/links/probe", "POST", "admin"),
        ("/links/:id", "PUT", "admin"),
        ("/links/:id", "DELETE", "admin"),
        ("/links/:id/open", "POST", "viewer"),
        ("/links/:id/stats", "GET", "viewer"),
        ("/storage/plan", "GET", "admin"),
        ("/drives", "GET", "admin"),
        ("/event-copy/test", "POST", "admin"),
        ("/storage/privacy", "GET", "admin"),
        ("/storage/missing", "GET", "admin"),
        ("/storage/missing/scan", "POST", "admin"),
        ("/storage/missing/apply", "POST", "admin"),
        ("/tls", "GET", "viewer"),
        ("/addresses", "GET", "admin"),
        ("/logs", "GET", "admin"),
        ("/events", "GET", "viewer"),
        ("/timeline/events", "GET", "viewer"),
        ("/events/:id/thumb", "GET", "viewer"),
        ("/events/:id/protect", "POST", "operator:events"),
        ("/events/:id/review", "POST", "operator:events"),
        ("/events/:id/ai", "GET", "viewer"),
        ("/events/:id/track", "GET", "viewer"),
        ("/signals", "GET", "viewer"),
        ("/signals", "POST", "viewer"),
        ("/signals/:name", "DELETE", "admin"),
        ("/events/:id/ai/recheck", "POST", "admin"),
        ("/events/:id/ai/:n", "GET", "viewer"),
        ("/ai/reviews", "GET", "admin"),
        ("/seek", "GET", "viewer"),
        ("/clips", "GET", "viewer"),
        ("/clips/:id", "GET", "viewer"),
        ("/export", "GET", "viewer"),
        ("/recordings", "GET", "viewer"),
        ("/recordings/days", "GET", "viewer"),
        ("/playback", "GET", "viewer"),
        ("/playback/picture", "GET", "viewer"),
        ("/still", "GET", "viewer"),
        ("/preview", "GET", "viewer"),
        ("/filmstrip", "GET", "viewer"),
        ("/timelapse", "GET", "viewer"),
        ("/timelapse", "POST", "admin"),
        ("/timelapse/:slug/:date", "GET", "viewer"),
        ("/faces", "GET", "admin"),
        ("/faces/status", "GET", "admin"),
        ("/smart/status", "GET", "viewer"),
        ("/sound/status", "GET", "admin"),
        ("/cameras/:id/sound", "GET", "viewer"),
        ("/smart/search", "GET", "viewer"),
        ("/smart/pic/:id", "GET", "viewer"),
        ("/smart/follow", "GET", "viewer"),
        ("/alert-rules", "GET", "admin"),
        ("/alert-rules", "POST", "admin"),
        ("/alert-rules/preview", "POST", "admin"),
        ("/alert-rules/:id", "DELETE", "admin"),
        ("/faces/models", "POST", "admin"),
        ("/faces/import", "POST", "admin"),
        ("/faces/:id/img", "GET", "admin"),
        ("/faces/:id/name", "POST", "admin"),
        ("/faces/:id", "DELETE", "admin"),
        ("/digest/preview", "GET", "admin"),
        ("/digest/send", "POST", "admin"),
        ("/digests", "GET", "viewer"),
        ("/digests/:id", "GET", "viewer"),
        ("/plates", "GET", "viewer"),
        ("/plates/recent", "GET", "viewer"),
        ("/plates/status", "GET", "admin"),
        ("/plates/models", "POST", "admin"),
        ("/plates/known", "GET", "admin"),
        ("/plates/known", "POST", "admin"),
        ("/plates/known/:id", "PUT", "admin"),
        ("/plates/known/:id", "DELETE", "admin"),
        ("/plates/:id/img", "GET", "viewer"),
        ("/plates/:id", "DELETE", "admin"),
        ("/people", "GET", "admin"),
        ("/people/:id", "PUT", "admin"),
        ("/people/:id", "DELETE", "admin"),
        ("/snapshot/:id", "GET", "viewer"),
        ("/cameras/:id/wake", "POST", "viewer"),
        ("/live/:id", "GET", "viewer"),
        ("/live/:id/info", "GET", "viewer"),
        ("/live/:id/stream", "GET", "viewer"),
        ("/live/:id/ws", "GET", "viewer"),
        ("/live/:id/jpeg", "GET", "viewer"),
        ("/settings", "GET", "admin"),
        ("/settings", "POST", "admin"),
        ("/offsite", "GET", "admin"),
        ("/offsite", "PUT", "admin"),
        ("/offsite/test", "POST", "admin"),
        ("/offsite/run", "POST", "admin"),
        ("/update", "GET", "admin"),
        ("/update/check", "POST", "admin"),
        ("/update/install", "POST", "admin"),
        ("/system/restart", "POST", "admin"),
        ("/overlay", "GET", "viewer"),
        ("/overlay/:name", "POST", "admin"),
    ];

    /// The methods a route line has (`get(..).post(..)`).
    fn route_methods(how: &str) -> Vec<&'static str> {
        ["GET", "POST", "PUT", "PATCH", "DELETE"]
            .into_iter()
            .filter(|m| {
                let pat = format!("{}(", m.to_ascii_lowercase());
                how.match_indices(&pat).any(|(i, _)| i == 0 || matches!(how.as_bytes()[i - 1], b' ' | b'.' | b':' | b'('))
            })
            .collect()
    }

    fn perms_without(p: &str) -> crate::config::OperatorPerms {
        let mut x = crate::config::OperatorPerms::default();
        match p {
            "ptz" => x.ptz = false,
            "talk_clip" => x.talk_clip = false,
            "arm" => x.arm = false,
            "events" => x.events = false,
            "camera_detection" => x.camera_detection = false,
            other => panic!("no such right: {other}"),
        }
        x
    }

    #[test]
    fn every_route_is_classified_for_viewers_operators_and_admins() {
        use super::{gate, Role};
        use axum::http::Method;
        let routes = api_routes();
        let mut pairs = Vec::new();
        for (route, how) in &routes {
            let ms = route_methods(how);
            assert!(!ms.is_empty(), "{route}: no method found in {how}");
            for m in ms {
                pairs.push((route.clone(), m));
            }
        }
        assert!(pairs.len() > 180, "the list was read: {}", pairs.len());
        for (route, m) in &pairs {
            assert!(ROLE_TABLE.iter().any(|t| t.0 == route && t.1 == *m), "{route} {m}: add it to ROLE_TABLE (viewer, operator:<right> or admin)");
        }
        for (route, m, _) in ROLE_TABLE {
            assert!(pairs.iter().any(|p| p.0 == *route && p.1 == *m), "{route} {m} is no route");
        }
        let none = crate::config::OperatorPerms { ptz: false, talk_clip: false, arm: false, events: false, camera_detection: false };
        let sample = |route: &str| route.split('/').map(|p| if p.starts_with(':') { "1" } else { p }).collect::<Vec<_>>().join("/");
        for (route, m, who) in ROLE_TABLE {
            let p = sample(route);
            let method = Method::from_bytes(m.as_bytes()).unwrap();
            let ok = |r: Role| gate(&p, &method, &r, false).is_ok();
            assert!(ok(Role::Admin), "{route} {m}: admin");
            let viewer = *who == "viewer";
            assert_eq!(ok(Role::Viewer), viewer, "{route} {m}: viewer");
            assert_eq!(ok(Role::Operator(none)), viewer, "{route} {m}: operator with no rights");
            match who.strip_prefix("operator:") {
                Some(right) => {
                    assert!(ok(Role::Operator(Default::default())), "{route} {m}: operator");
                    assert!(!ok(Role::Operator(perms_without(right))), "{route} {m}: operator without {right}");
                    for other in ["ptz", "talk_clip", "arm", "events", "camera_detection"].into_iter().filter(|o| o != &right) {
                        assert!(ok(Role::Operator(perms_without(other))), "{route} {m}: operator without {other} only");
                    }
                }
                None => {
                    assert!(viewer || *who == "admin", "{route} {m}: {who}?");
                    assert_eq!(ok(Role::Operator(Default::default())), viewer, "{route} {m}: operator");
                }
            }
            // HEAD is GET.
            if *m == "GET" {
                assert_eq!(gate(&p, &Method::HEAD, &Role::Viewer, false).is_ok(), viewer, "{route} HEAD");
            }
        }
    }

    #[test]
    fn operators_get_nothing_by_path_tricks() {
        use super::{gate, operator_route, Role};
        use axum::http::Method;
        let op = Role::Operator(Default::default());
        for (p, m) in [
            ("/cameras/x/ptz", Method::POST),
            ("/cameras//ptz", Method::POST),
            ("/cameras/1/ptz/", Method::POST),
            ("/cameras/1/ptz/presets", Method::POST),
            ("/cameras/1/ptz", Method::PUT),
            ("/cameras/1/ptz", Method::DELETE),
            ("/cameras/1", Method::DELETE),
            ("/cameras/1", Method::POST),
            ("/cameras", Method::POST),
            ("/cameras/1/enable", Method::POST),
            ("/cameras/1/reboot", Method::POST),
            ("/cameras/1/spots/forget", Method::POST),
            ("/cameras/1/tamper/reference", Method::POST),
            ("/events//protect", Method::POST),
            ("/events/1/ai/recheck", Method::POST),
            ("/settings", Method::POST),
            ("/users", Method::POST),
            ("/overlay/x", Method::POST),
            ("/noisy/1/dismiss", Method::POST),
            ("/timesync/all", Method::POST),
            ("/system/restart", Method::POST),
            ("cameras/1/ptz", Method::POST),
            ("/%63ameras/1/ptz", Method::POST),
            ("/cameras/1/../../settings", Method::POST),
            ("/brand-new", Method::POST),
        ] {
            assert!(operator_route(p, &m).is_none(), "{p} {m}");
            assert!(gate(p, &m, &op, false).is_err(), "{p} {m}");
        }
        // Reads an operator has a viewer's answer for: still admin only.
        for p in ["/settings", "/users", "/logs", "/health", "/cameras/1/tamper", "/cameras/1/onvif", "/cameras/1/spots", "/storage/plan", "/offsite", "/update", "/alert-rules"] {
            assert!(gate(p, &Method::GET, &op, false).is_err(), "{p}");
        }
        // Limited to some cameras: no arming or profiles (every camera), the rest as before.
        for p in ["/shield", "/profile"] {
            assert!(gate(p, &Method::POST, &op, false).is_ok(), "{p}");
            assert!(gate(p, &Method::POST, &op, true).is_err(), "{p} limited");
            assert!(gate(p, &Method::POST, &Role::Admin, true).is_ok(), "{p} admin");
        }
        for p in ["/cameras/1/pause", "/cameras/1/ptz", "/clips/record", "/events/x-1/protect"] {
            assert!(gate(p, &Method::POST, &op, true).is_ok(), "{p} limited");
        }
        assert!(gate("/cameras/1", &Method::PUT, &op, true).is_ok());
    }

    #[test]
    fn every_route_is_classified_for_device_tokens() {
        let routes = api_routes();
        assert!(routes.len() > 150, "the list was read: {}", routes.len());
        for (route, _) in &routes {
            let allowed = super::TOKEN_ROUTES.iter().any(|t| t.0 == route);
            let refused = TOKEN_REFUSED.contains(&route.as_str());
            assert!(allowed != refused, "{route}: add it to TOKEN_ROUTES (tokens may) or TOKEN_REFUSED (they may not)");
        }
        // Nothing listed that is not a route, or with a method it does not have.
        for (route, method, _) in super::TOKEN_ROUTES {
            let how = routes.iter().find(|r| r.0 == *route).unwrap_or_else(|| panic!("{route} is no route")).1.to_ascii_lowercase();
            assert!(how.contains(&format!("{}(", method.to_ascii_lowercase())), "{route} has no {method}");
        }
        for route in TOKEN_REFUSED {
            assert!(routes.iter().any(|r| r.0 == *route), "{route} is no route");
        }
    }

    #[test]
    fn device_tokens_are_refused_by_default() {
        use axum::http::Method;
        let may = super::token_may;
        let sample = |route: &str| route.split('/').map(|p| if p.starts_with(':') { "1" } else { p }).collect::<Vec<_>>().join("/");
        for route in TOKEN_REFUSED {
            // (/events/1/ai/recheck and such: no route allowed has that shape.)
            for m in [Method::GET, Method::HEAD, Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
                for control in [false, true] {
                    assert!(!may(&sample(route), &m, control), "{route} {m} {control}");
                }
            }
        }
        // Routes partly open: only the method listed.
        for (p, m) in [("/cameras", Method::POST), ("/cameras/1", Method::PUT), ("/cameras/1", Method::DELETE), ("/signals", Method::GET), ("/signals", Method::DELETE), ("/clips/1", Method::DELETE)] {
            assert!(!may(p, &m, true), "{p} {m}");
        }
        // A route nobody has thought of yet.
        for p in ["/brand-new", "/cameras/1/brand-new", "/", "", "/cameras/", "/live//ws", "/%63ameras", "/cameras/1/../../settings"] {
            assert!(!may(p, &Method::GET, true), "{p}");
            assert!(!may(p, &Method::POST, true), "{p}");
        }
        // What a watching token does, and control only with control rights.
        for (p, m) in [("/cameras", Method::GET), ("/status", Method::GET), ("/live/1/ws", Method::GET), ("/snapshot/1", Method::GET), ("/snapshot/1", Method::HEAD), ("/events", Method::GET), ("/clips/1", Method::GET), ("/recordings", Method::GET), ("/playback", Method::GET), ("/timeline/events", Method::GET), ("/still", Method::GET), ("/cameras/1/wake", Method::POST), ("/signals", Method::POST)] {
            assert!(may(p, &m, false), "{p}");
            assert!(may(p, &m, true), "{p}");
        }
        for (p, m) in [("/shield", Method::POST), ("/profile", Method::POST), ("/cameras/1/ptz", Method::POST), ("/cameras/1/ptz/aim", Method::POST), ("/cameras/1/pause", Method::POST), ("/clips/record", Method::POST), ("/events/x/review", Method::POST), ("/events/x/protect", Method::POST)] {
            assert!(!may(p, &m, false), "{p}");
            assert!(may(p, &m, true), "{p}");
        }
    }

    #[test]
    fn a_viewer_manages_only_their_own_two_step_and_tokens() {
        use axum::http::Method;
        for p in ["/me/two-step/start", "/me/two-step/confirm", "/me/two-step/cancel", "/me/two-step/off", "/me/two-step/recovery", "/me/tokens"] {
            assert!(viewer_may_write(p, &Method::POST), "{p}");
        }
        assert!(viewer_may_write("/me/tokens/abc", &Method::DELETE));
        // Their own signed-in devices; a device token never.
        for p in ["/me/sessions/sign-out-others", "/me/sessions/ses-0123456789abcdef/sign-out"] {
            assert!(viewer_may_write(p, &Method::POST), "{p}");
            assert!(!viewer_may_write(p, &Method::DELETE), "{p}");
            for control in [false, true] {
                assert!(!super::token_may(p, &Method::POST, control), "{p}");
            }
        }
        assert!(!viewer_may_write("/me/sessions/x", &Method::POST));
        assert!(!super::token_may("/me/account", &Method::GET, true));
        assert!(!viewer_may_write("/users/ann/two-step/off", &Method::POST));
        assert!(!viewer_may_write("/users/ann/tokens/abc", &Method::DELETE));
        assert!(super::admin_only_path("/users/ann/tokens"), "others' token lists");
        assert!(!super::admin_only_path("/me/account"));
    }
    use axum::http::Method;

    #[test]
    fn viewers_may_report_and_remove_their_phones_only() {
        assert!(viewer_may_write("/presence", &Method::POST));
        assert!(viewer_may_write("/presence/abc-1", &Method::DELETE));
        assert!(!viewer_may_write("/presence/", &Method::DELETE));
        assert!(!viewer_may_write("/presence", &Method::DELETE));
        assert!(!viewer_may_write("/presence/abc", &Method::POST));
        assert!(!viewer_may_write("/presence/abc", &Method::PUT));
        // The rest as before.
        assert!(viewer_may_write("/push/subscribe", &Method::POST));
        assert!(viewer_may_write("/signals", &Method::POST));
        assert!(!viewer_may_write("/signals", &Method::DELETE));
        assert!(viewer_may_write("/links/x/open", &Method::POST));
        assert!(!viewer_may_write("/shield", &Method::POST));
        assert!(!viewer_may_write("/profile", &Method::POST));
        assert!(!viewer_may_write("/settings", &Method::POST));
        assert!(!viewer_may_write("/system/restart", &Method::POST));
    }

    #[test]
    fn a_saved_login_goes_only_to_a_host_it_signed_in_to() {
        use super::{note_saved_login_worked, preview_input, saved_login_worked, use_saved_login, PreviewReq};
        use crate::config::LoginPair;
        let saved = vec![LoginPair { username: "admin".into(), password: "hunter2".into() }, LoginPair { username: "blank".into(), password: String::new() }];
        let req = |url: &str, sub: Option<&str>, user: &str, pw: &str| PreviewReq { url: url.into(), sub_url: sub.map(String::from), username: user.into(), password: pw.into() };
        note_saved_login_worked("10.9.8.7", "admin");
        // The page sends no password: the one saved, in the address only.
        let mut r = req("rtsp://10.9.8.7/main", Some("rtsp://10.9.8.7/sub"), "admin", "");
        use_saved_login(&mut r, &saved, saved_login_worked).unwrap();
        assert_eq!(preview_input(&r).unwrap(), "rtsp://admin:hunter2@10.9.8.7/sub");
        // Any other host: not sent, and not tried with no password either.
        for (url, sub) in [("rtsp://10.9.8.8/main", None), ("rtsp://10.9.8.7/main", Some("rtsp://10.9.8.8/sub")), ("rtsp://evil.example/main", None)] {
            let mut r = req(url, sub, "admin", "");
            assert!(use_saved_login(&mut r, &saved, saved_login_worked).is_err(), "{url} {sub:?}");
            assert!(r.password.is_empty());
        }
        // Another login that signed in elsewhere does not open it.
        note_saved_login_worked("10.9.8.9", "someone");
        assert!(use_saved_login(&mut req("rtsp://10.9.8.9/main", None, "admin", ""), &saved, saved_login_worked).is_err());
        // A typed password, an unsaved name and no name stay as they were.
        let mut r = req("rtsp://10.9.8.8/main", None, "admin", "typed");
        use_saved_login(&mut r, &saved, saved_login_worked).unwrap();
        assert_eq!(r.password, "typed");
        for user in ["other", "blank", ""] {
            let mut r = req("rtsp://10.9.8.8/main", None, user, "");
            use_saved_login(&mut r, &saved, saved_login_worked).unwrap();
            assert!(r.password.is_empty(), "{user}");
        }
    }

    #[test]
    fn a_settings_change_that_will_fail_is_found_on_a_copy() {
        use super::{merge_settings, settings_need_proof, SettingsUpdate};
        let cfg: crate::config::Config = toml::from_str("").unwrap();
        let update = |v: serde_json::Value| serde_json::from_value::<SettingsUpdate>(v).unwrap();
        // A new destination, and something else in the same save that is refused.
        let bad = update(serde_json::json!({ "alerts": { "webhook_url": "https://example.com/hook" }, "watchdog": { "quiet_hours": "whenever" } }));
        assert!(settings_need_proof(&cfg, &bad), "the destination changes");
        let mut copy = cfg.clone();
        assert!(merge_settings(&mut copy, &bad, &[]).err().unwrap().contains("quiet hours"));
        let good = update(serde_json::json!({ "alerts": { "webhook_url": "https://example.com/hook" } }));
        let mut copy = cfg.clone();
        assert!(merge_settings(&mut copy, &good, &[]).is_ok());
        assert_eq!(copy.alerts.webhook_url.as_deref(), Some("https://example.com/hook"));
        assert!(cfg.alerts.webhook_url.is_none(), "only the copy changes");
    }

    #[test]
    fn admin_only_reads() {
        for p in ["/settings", "/storage/missing", "/storage/missing/scan", "/storage/missing/apply", "/system/restart", "/system", "/update", "/logs", "/users", "/cameras/3/tamper", "/cameras/3/unusual", "/cameras/3/onvif", "/alert-rules", "/alert-rules/preview"] {
            assert!(super::admin_only_path(p), "{p}");
        }
        for p in ["/tls", "/whoami", "/cameras", "/status", "/cameras/3/tamper/ref/day", "/cameras/3/tamper/incidents/7/cur"] {
            assert!(!super::admin_only_path(p), "{p}");
        }
    }
}

#[cfg(test)]
mod https_settings_tests {
    use super::*;

    fn upd(j: serde_json::Value) -> ServerUpdate {
        serde_json::from_value(j).unwrap()
    }

    #[test]
    fn a_found_cameras_picture_comes_from_its_small_stream() {
        let r = |url: &str, sub: Option<&str>, u: &str, p: &str| PreviewReq { url: url.into(), sub_url: sub.map(String::from), username: u.into(), password: p.into() };
        assert_eq!(preview_input(&r("rtsp://10.0.0.5/main", Some("rtsp://10.0.0.5/sub"), "", "")).unwrap(), "rtsp://10.0.0.5/sub");
        assert_eq!(preview_input(&r("rtsp://10.0.0.5/main", Some(" "), "", "")).unwrap(), "rtsp://10.0.0.5/main");
        // The login goes in the address, escaped.
        assert_eq!(preview_input(&r("rtsp://10.0.0.5:554/m", None, "ann", "p@ss/w:d")).unwrap(), "rtsp://ann:p%40ss%2Fw%3Ad@10.0.0.5:554/m");
        // Nothing but RTSP: not a way to make the server read files or web addresses.
        for bad in ["http://10.0.0.5/x", "file:///etc/passwd", "lavfi://testsrc", "concat:a|b", "/etc/passwd", ""] {
            assert!(preview_input(&r(bad, None, "", "")).is_err(), "{bad}");
        }
    }

    #[test]
    fn https_settings_merge_and_are_checked() {
        let cur = crate::config::ServerConfig { port: 8090, ..Default::default() };
        // Partial: only what is sent changes; port and bind cannot change here.
        let s = apply_server_update(&cur, upd(serde_json::json!({ "tls": true, "port": 1, "bind": "1.2.3.4" }))).unwrap();
        assert!(s.tls && s.port == 8090 && s.bind == cur.bind && s.tls_names.is_empty());
        let s = apply_server_update(&s, upd(serde_json::json!({ "tls_names": ["NVR.tail1.ts.net", "nvr.tail1.ts.net", " 10.0.0.9 "] }))).unwrap();
        assert!(s.tls);
        assert_eq!(s.tls_names, vec!["nvr.tail1.ts.net".to_string(), "10.0.0.9".to_string()]);
        let e = apply_server_update(&s, upd(serde_json::json!({ "tls_names": ["https://nvr.lan/"] }))).unwrap_err();
        assert!(e.contains("not a host name"), "{e}");
        // One file without the other.
        let e = apply_server_update(&s, upd(serde_json::json!({ "tls_cert": "/nowhere/c.pem" }))).unwrap_err();
        assert!(e.contains("both"), "{e}");
        let e = apply_server_update(&s, upd(serde_json::json!({ "tls_cert": "/nowhere/c.pem", "tls_key": "/nowhere/k.pem" }))).unwrap_err();
        assert!(e.contains("could not be read"), "{e}");
        // Bad PEM.
        let dir = std::env::temp_dir().join(format!("amber-https-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cf, kf) = (dir.join("c.pem"), dir.join("k.pem"));
        std::fs::write(&cf, "-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n").unwrap();
        std::fs::write(&kf, "junk").unwrap();
        let e = apply_server_update(&s, upd(serde_json::json!({ "tls_cert": cf, "tls_key": kf }))).unwrap_err();
        assert!(e.contains("PEM") || e.contains("no certificate"), "{e}");
        // A real pair, then cleared with "".
        let k = rcgen::KeyPair::generate().unwrap();
        let c = rcgen::CertificateParams::new(vec!["nvr.lan".to_string()]).unwrap().self_signed(&k).unwrap();
        std::fs::write(&cf, c.pem()).unwrap();
        std::fs::write(&kf, k.serialize_pem()).unwrap();
        let s2 = apply_server_update(&s, upd(serde_json::json!({ "tls_cert": cf, "tls_key": kf }))).unwrap();
        assert_eq!(s2.tls_cert.as_deref(), Some(cf.as_path()));
        let s3 = apply_server_update(&s2, upd(serde_json::json!({ "tls_cert": "", "tls_key": " " }))).unwrap();
        assert!(s3.tls_cert.is_none() && s3.tls_key.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The session cookie of an answer.
    fn set_cookie(r: &Response) -> String {
        r.headers().get(header::SET_COOKIE).expect("a cookie").to_str().unwrap().to_string()
    }

    fn post(path: &str, body: &str, secure: bool) -> Request {
        let mut r = Request::builder().method("POST").uri(path).body(Body::from(body.to_string())).unwrap();
        if secure {
            r.extensions_mut().insert(crate::tls::Secure);
        }
        r
    }

    #[tokio::test]
    async fn session_cookie_is_secure_only_over_https() {
        // HTTPS saved on, but this request came over plain http (not
        // restarted yet, or turned off): no Secure flag, or the browser
        // would drop the cookie and sign-in would never stick.
        let mut cfg: crate::config::Config = toml::from_str("").unwrap();
        cfg.server.tls = true;
        cfg.server.users.push(crate::config::UserConfig { disabled: false, username: "ann".into(), password: crate::auth::hash_password("pw-ann-1"), admin: false, cameras: Vec::new(), ..Default::default() });
        let e = Engine::for_test(cfg);
        for secure in [false, true] {
            let code = crate::auth::new_pair_code("ann");
            let r = pair(State(e.clone()), post("/api/pair", &serde_json::json!({ "code": code }).to_string(), secure)).await;
            assert_eq!(r.status(), StatusCode::OK);
            let c = set_cookie(&r);
            assert!(c.starts_with("as_session=") && c.contains("HttpOnly"), "{c}");
            assert_eq!(c.contains("; Secure"), secure, "pair, secure={secure}: {c}");
            let r = login(State(e.clone()), post("/api/login", r#"{"username":"ann","password":"pw-ann-1"}"#, secure)).await;
            assert_eq!(r.status(), StatusCode::OK);
            assert_eq!(set_cookie(&r).contains("; Secure"), secure, "login, secure={secure}");
            // Clearing it: a Secure cookie is only replaced by a Secure answer.
            let r = logout(State(e.clone()), post("/api/logout", "", secure)).await;
            let c = set_cookie(&r);
            assert!(c.contains("Max-Age=0"), "{c}");
            assert_eq!(c.contains("; Secure"), secure, "logout, secure={secure}: {c}");
        }
        // HTTPS saved off, request over TLS (still running with it): Secure.
        e.config.lock().unwrap().server.tls = false;
        let code = crate::auth::new_pair_code("ann");
        let r = pair(State(e.clone()), post("/api/pair", &serde_json::json!({ "code": code }).to_string(), true)).await;
        assert!(set_cookie(&r).contains("; Secure"));
    }

    #[test]
    fn own_certificate_warns_about_links_and_helpers() {
        let cur = crate::config::ServerConfig { tls: true, ..Default::default() };
        let own = crate::config::ServerConfig { tls_cert: Some("/c.pem".into()), tls_key: Some("/k.pem".into()), ..cur.clone() };
        assert!(own_cert_warning(&cur, &own, 0, 0).is_none());
        let w = own_cert_warning(&cur, &own, 2, 1).expect("warned");
        assert!(w.contains("2 linked servers and 1 paired capture helper"), "{w}");
        assert!(own_cert_warning(&cur, &own, 0, 3).unwrap().contains("3 paired capture helpers"));
        // Saved again unchanged, or back to AmberSight's own certificate: no warning.
        assert!(own_cert_warning(&own, &own, 2, 1).is_none());
        assert!(own_cert_warning(&own, &cur, 2, 1).is_none());
    }

    #[test]
    fn restart_refused_when_not_a_service() {
        let r = restart_refusal(false, false).expect("refused");
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert!(restart_refusal(true, false).is_none());
    }

    #[tokio::test]
    async fn restart_refused_while_an_update_installs() {
        let r = restart_refusal(true, true).expect("refused");
        assert_eq!(r.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "an update is being installed; try again when it has finished");
        // Not a service wins (nothing to wait for).
        assert!(restart_refusal(false, true).is_some());
    }
}

#[cfg(test)]
mod folder_wait_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn a_stuck_folder_is_unknown_in_time_the_others_answer_and_it_is_not_looked_at_twice() {
        let runs = Arc::new(AtomicUsize::new(0));
        let slow = std::path::PathBuf::from("/nas/dead-for-test");
        let quick = std::path::PathBuf::from("/nas/fine-for-test");
        let r = runs.clone();
        let a = BoundedFolder::start(slow.clone(), move || {
            r.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(900));
            serde_json::json!({ "state": "private", "detail": "" })
        });
        let b = BoundedFolder::start(quick, || serde_json::json!({ "state": "open", "detail": "" }));
        let deadline = Instant::now() + Duration::from_millis(300);
        let started = Instant::now();
        let (va, vb) = (a.finish(deadline), b.finish(deadline));
        assert_eq!(va["state"], "unknown");
        assert!(va["detail"].as_str().unwrap().contains("network share"));
        assert_eq!(vb["state"], "open", "the others still report");
        assert!(started.elapsed() < Duration::from_millis(800), "{:?}", started.elapsed());
        // Asked again (the page polls) while the first is still stuck: no second look-up.
        let r = runs.clone();
        let again = BoundedFolder::start(slow.clone(), move || {
            r.fetch_add(1, Ordering::SeqCst);
            serde_json::json!({ "state": "private", "detail": "" })
        });
        assert!(matches!(again, BoundedFolder::Stuck));
        assert_eq!(again.finish(Instant::now())["state"], "unknown");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // When the drive answers at last, the path is looked at again.
        std::thread::sleep(Duration::from_millis(1000));
        let ok = BoundedFolder::start(slow, || serde_json::json!({ "state": "private", "detail": "" })).finish(Instant::now() + Duration::from_secs(2));
        assert_eq!(ok["state"], "private");
    }
}

#[cfg(test)]
mod operator_tests {
    use super::{operator_edit_outside, CameraConfig};

    fn cam() -> CameraConfig {
        toml::from_str(
            r#"
            name = "Porch"
            url = "rtsp://admin:secret@192.0.2.10/main"
            sub_url = "rtsp://admin:secret@192.0.2.10/sub"
            username = "admin"
            password = "secret"
            folder = "/mnt/rec"
            ai_min_score = 0.55
            audio_margin_db = 12.5
            alert_labels = ["person"]
            groups = ["Outside"]
            "#,
        )
        .unwrap()
    }

    fn edit(cur: &CameraConfig, patch: serde_json::Value) -> CameraConfig {
        super::merge_camera_json(cur, patch).unwrap()
    }

    #[test]
    fn an_operator_changes_detection_and_alerts_only() {
        use serde_json::json as j;
        let cur = cam();
        // Nothing changed: the whole camera sent back as it is is fine, floats and all.
        let same = serde_json::to_value(&cur).unwrap();
        assert!(operator_edit_outside(&cur, &edit(&cur, same)).is_empty());
        for p in [
            j!({"motion": false}),
            j!({"sensitivity": 0.02, "min_object": 0.01, "max_object": 0.4, "contrast": 30, "min_travel": 0.05, "min_frames": 3, "break_time": 9, "motion_algorithm": "frame-diff", "schedule": "20:00-07:00"}),
            j!({"zones": [{"x": 0.1, "y": 0.1, "w": 0.5, "h": 0.5}], "ignore_zones": []}),
            j!({"alerts": false, "alert_interval": 60, "alert_schedule": "08:00-17:00", "alert_labels": ["car"], "alert_priority": "high"}),
            j!({"no_motion_minutes": 30, "no_motion_schedule": "09:00-17:00"}),
            j!({"audio_trigger": true, "audio_margin_db": 20.0, "audio_min_db": -40.0, "audio_sounds": ["dog"]}),
            j!({"ai": true, "ai_labels": ["person", "dog"], "ai_min_score": 0.7, "ai_ignore_parked": false, "ai_strict": true, "ai_hide_unconfirmed": true, "ai_confirm_camera": true}),
        ] {
            assert!(operator_edit_outside(&cur, &edit(&cur, p.clone())).is_empty(), "{p}");
        }
        // Everything else: refused, by name.
        for (p, k) in [
            (j!({"url": "rtsp://evil/"}), "url"),
            (j!({"sub_url": null}), "sub_url"),
            (j!({"username": "x"}), "username"),
            (j!({"password": ""}), "password"),
            (j!({"name": "Back"}), "name"),
            (j!({"short_name": "B"}), "short_name"),
            (j!({"groups": []}), "groups"),
            (j!({"folder": "/tmp/x"}), "folder"),
            (j!({"enabled": false}), "enabled"),
            (j!({"record": false}), "record"),
            (j!({"record_motion_only": true}), "record_motion_only"),
            (j!({"hwaccel": "vaapi"}), "hwaccel"),
            (j!({"ingest": "ffmpeg"}), "ingest"),
            (j!({"fps": 30}), "fps"),
            (j!({"privacy_masks": [{"x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0}]}), "privacy_masks"),
            (j!({"also_trigger": ["Garage"]}), "also_trigger"),
            (j!({"overlay": true}), "overlay"),
            (j!({"overlay_text": "{v:x}"}), "overlay_text"),
            (j!({"onvif_port": 8000}), "onvif_port"),
            (j!({"watchdog": {"reboot": "onvif"}}), "watchdog"),
            (j!({"packages": true}), "packages"),
            (j!({"share": true}), "share"),
            (j!({"alert_channels": ["push"]}), "alert_channels"),
            (j!({"copy_events_to": "/tmp/evil"}), "copy_events_to"),
            (j!({"copy_events_keep_days": 1}), "copy_events_keep_days"),
            (j!({"copy_events_only_confirmed": true}), "copy_events_only_confirmed"),
            (j!({"copy_events_only_alerts": true}), "copy_events_only_confirmed"),
        ] {
            let new = edit(&cur, p.clone());
            let out = operator_edit_outside(&cur, &new);
            assert!(out.iter().any(|x| x == k), "{p}: {out:?}");
        }
    }

    #[test]
    fn event_copies_are_an_admins() {
        // Never in what an operator may change, and hidden from operators as from viewers.
        assert!(!super::OPERATOR_CAMERA_FIELDS.iter().any(|f| f.starts_with("copy_")));
        for k in ["copy_events_to", "copy_events_keep_days", "copy_events_only_confirmed", "copy_events_status", "folder", "username", "has_password"] {
            assert!(super::HIDDEN_FROM_NON_ADMINS.contains(&k), "{k}");
        }
    }

    #[test]
    fn every_change_is_told_in_plain_words() {
        use super::{changed_fields, changes_in_words as words};
        let cur = cam();
        let f = |p| changed_fields(&cur, &edit(&cur, p));
        assert_eq!(words(&f(serde_json::json!({"alert_schedule": "08:00-17:00", "zone_polys": [{"points": [[0.1, 0.1], [0.5, 0.1], [0.5, 0.5]]}]}))), "alert schedule, detection zones");
        assert_eq!(words(&f(serde_json::json!({"min_object": 0.02, "contrast": 40, "motion": false}))), "motion sensitivity, motion detection on or off");
        assert_eq!(words(&f(serde_json::json!({"ai_strict": true, "ai_hide_unconfirmed": true}))), "AI confirmation");
        assert_eq!(words(&f(serde_json::json!({"alert_ignore_polys": [{"points": [[0.1, 0.1], [0.5, 0.1], [0.5, 0.5]]}]}))), "areas that don't notify");
        // Every field an operator may change has words of its own (not the catch-all) unless it is AI.
        for k in super::OPERATOR_CAMERA_FIELDS {
            assert!(super::setting_words(k) != "AI confirmation" || k.starts_with("ai_"), "{k}");
        }
    }

    #[test]
    fn zone_sizes_are_held_to_limits_when_they_change() {
        use super::check_camera_sizes as sizes;
        let cur = cam();
        let poly = |n: usize| serde_json::json!({"points": (0..n).map(|i| { let a = i as f64 / n as f64 * 6.28; [0.5 + 0.4 * a.cos(), 0.5 + 0.4 * a.sin()] }).collect::<Vec<_>>()});
        let rect = serde_json::json!({"x": 0.1, "y": 0.1, "w": 0.1, "h": 0.1});
        assert!(sizes(&edit(&cur, serde_json::json!({"zone_polys": [poly(64)], "zones": vec![rect.clone(); 32]})), Some(&cur)).is_ok());
        for p in [
            serde_json::json!({"zone_polys": [poly(65)]}),
            serde_json::json!({"ignore_polys": vec![poly(4); 33]}),
            serde_json::json!({"ignore_polys": [{"points": [[0.1, 0.1], [2.0, 0.1], [0.5, 0.5]]}]}),
            serde_json::json!({"zones": vec![rect.clone(); 33]}),
            serde_json::json!({"ai_labels": vec!["x"; 65]}),
            serde_json::json!({"ai_labels": [""]}),
        ] {
            assert!(sizes(&edit(&cur, p.clone()), Some(&cur)).is_err(), "{p}");
            assert!(sizes(&edit(&cur, p.clone()), None).is_err(), "{p} new");
        }
        // Saved before the limits: still saves while unchanged.
        let big = edit(&cur, serde_json::json!({"zones": vec![rect.clone(); 40]}));
        assert!(sizes(&edit(&big, serde_json::json!({"motion": false})), Some(&big)).is_ok());
        // Unchanged also when a page sends the numbers back with float noise.
        let mut noisy = serde_json::to_value(&big).unwrap()["zones"].clone();
        for z in noisy.as_array_mut().unwrap() {
            z["x"] = serde_json::json!(z["x"].as_f64().unwrap() + 1e-12);
        }
        assert!(sizes(&edit(&big, serde_json::json!({"zones": noisy})), Some(&big)).is_ok());
        // Rectangles inside the picture and not empty; schedules strictly; known values; sounds once.
        for p in [
            serde_json::json!({"zones": [{"x": 0.9, "y": 0.1, "w": 0.2, "h": 0.1}]}),
            serde_json::json!({"ignore_zones": [{"x": -0.1, "y": 0.1, "w": 0.2, "h": 0.1}]}),
            serde_json::json!({"privacy_masks": [{"x": 0.1, "y": 0.1, "w": 0.0, "h": 0.1}]}),
            serde_json::json!({"schedule": "08:00-17:00-x"}),
            serde_json::json!({"alert_schedule": "08:00-17:00-09:00"}),
            serde_json::json!({"schedule": "nonsense"}),
            serde_json::json!({"motion_algorithm": "magic"}),
            serde_json::json!({"alert_priority": "loud"}),
            serde_json::json!({"audio_sounds": ["dog bark", "Dog bark"]}),
        ] {
            assert!(sizes(&edit(&cur, p.clone()), Some(&cur)).is_err(), "{p}");
        }
        for p in [
            serde_json::json!({"zones": [{"x": 0.8, "y": 0.0, "w": 0.2, "h": 1.0}]}),
            serde_json::json!({"schedule": "20:00-07:00", "motion_algorithm": "frame-diff", "alert_priority": "urgent"}),
            serde_json::json!({"schedule": null, "alert_schedule": ""}),
        ] {
            assert!(sizes(&edit(&cur, p.clone()), Some(&cur)).is_ok(), "{p}");
        }
        assert!(super::json_close(&serde_json::json!([0.1, {"a": 0.3}]), &serde_json::json!([0.1000000000001, {"a": 0.3}])));
        assert!(!super::json_close(&serde_json::json!([0.1]), &serde_json::json!([0.11])));
    }

    #[tokio::test]
    async fn operators_through_the_whole_stack() {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use base64::Engine as _;
        use tower::ServiceExt;
        let mut cfg: crate::config::Config = toml::from_str("").unwrap();
        let no_arm = crate::config::OperatorPerms { arm: false, ..Default::default() };
        let users: [(&str, bool, &str, crate::config::OperatorPerms, Vec<String>); 6] = [
            ("ann", true, "", Default::default(), vec![]),
            ("otto", false, "operator", Default::default(), vec![]),
            ("olga", false, "operator", no_arm, vec!["Porch".to_string()]),
            ("vic", false, "", Default::default(), vec![]),
            ("odd", false, "superuser", Default::default(), vec![]),
            ("lim", false, "operator", Default::default(), vec!["Porch".to_string()]),
        ];
        for (name, admin, role, perms, cams) in users {
            cfg.server.users.push(crate::config::UserConfig { username: name.into(), password: crate::auth::hash_password(&format!("pw-{name}-1")), admin, cameras: cams, disabled: false, role: role.into(), perms });
        }
        let e = crate::engine::Engine::for_test(cfg);
        let app = super::build_router(e.clone());
        let call = |method: &str, path: &str, who: &str, origin: Option<&str>, body: &str| {
            let mut b = Request::builder().method(method).uri(path).header(header::HOST, "localhost:8090").header(header::CONTENT_TYPE, "application/json");
            b = b.header(header::AUTHORIZATION, format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{who}:pw-{who}-1"))));
            if let Some(o) = origin {
                b = b.header(header::ORIGIN, o);
            }
            let req = b.body(Body::from(body.to_string())).unwrap();
            let app = app.clone();
            async move {
                let r = app.oneshot(req).await.unwrap();
                let s = r.status();
                let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
                (s, String::from_utf8_lossy(&body).into_owned())
            }
        };
        // whoami tells each its role and rights; an operator is no admin.
        let (_, w) = call("GET", "/api/whoami", "olga", None, "").await;
        let w: serde_json::Value = serde_json::from_str(&w).unwrap();
        assert_eq!((w["admin"].as_bool(), w["role"].as_str(), w["perms"]["arm"].as_bool(), w["perms"]["ptz"].as_bool()), (Some(false), Some("operator"), Some(false), Some(true)), "{w}");
        for (who, role) in [("ann", "admin"), ("otto", "operator"), ("vic", "viewer"), ("odd", "viewer")] {
            let (_, w) = call("GET", "/api/whoami", who, None, "").await;
            assert!(w.contains(&format!("\"role\":\"{role}\"")), "{who}: {w}");
        }
        // Arming: admins and operators with the right.
        let disarm = r#"{"armed":false}"#;
        assert_eq!(call("POST", "/api/shield", "ann", None, disarm).await.0, StatusCode::OK);
        assert_eq!(call("POST", "/api/shield", "otto", None, disarm).await.0, StatusCode::OK);
        assert_eq!(call("POST", "/api/shield", "olga", None, disarm).await.0, StatusCode::FORBIDDEN, "right turned off");
        assert_eq!(call("POST", "/api/shield", "vic", None, disarm).await.0, StatusCode::FORBIDDEN);
        assert_eq!(call("POST", "/api/shield", "odd", None, disarm).await.0, StatusCode::FORBIDDEN, "an unknown role is a viewer");
        // Limited to some cameras: no arming or profiles, which change every camera.
        assert_eq!(call("POST", "/api/shield", "lim", None, disarm).await.0, StatusCode::FORBIDDEN);
        assert_eq!(call("POST", "/api/profile", "lim", None, r#"{"name":null}"#).await.0, StatusCode::FORBIDDEN);
        let (_, w) = call("GET", "/api/whoami", "lim", None, "").await;
        assert!(w.contains("\"arm\":false") && w.contains("\"pause\":true") && w.contains("\"limited\":true"), "{w}");
        assert_eq!(call("GET", "/api/camera-defaults", "vic", None, "").await.0, StatusCode::OK);
        // The sign-in answer says the same as whoami for the new session.
        let (s, b) = call("POST", "/api/login", "lim", None, r#"{"username":"lim","password":"pw-lim-1"}"#).await;
        let b: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
        assert_eq!((s, b["role"].as_str(), b["perms"]["arm"].as_bool(), b["perms"]["pause"].as_bool(), b["limited"].as_bool(), b["admin"].as_bool()), (StatusCode::OK, Some("operator"), Some(false), Some(true), Some(true), Some(false)), "{b}");
        let (_, b) = call("POST", "/api/login", "vic", None, r#"{"username":"vic","password":"pw-vic-1"}"#).await;
        assert!(b.contains("\"role\":\"viewer\"") && b.contains("\"camera_detection\":false"), "{b}");
        // A device token's session: no camera settings or talk, whatever the account may do.
        let (tok, _) = crate::auth::add_token("otto", "app", true, crate::auth::generation("otto")).unwrap().unwrap();
        let req = Request::builder().uri("/api/whoami").header(header::HOST, "localhost:8090").header(header::AUTHORIZATION, format!("Bearer {tok}")).body(Body::empty()).unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let w: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!((w["role"].as_str(), w["perms"]["arm"].as_bool(), w["perms"]["ptz"].as_bool(), w["perms"]["camera_detection"].as_bool(), w["perms"]["talk_clip"].as_bool()), (Some("operator"), Some(true), Some(true), Some(false), Some(false)), "{w}");
        assert_eq!(call("POST", "/api/shield", "otto", Some("http://evil.example"), disarm).await.0, StatusCode::FORBIDDEN, "another site");
        assert_eq!(call("POST", "/api/cameras/0/pause", "olga", None, "{}").await.0, StatusCode::FORBIDDEN);
        // Events of their cameras only.
        let keep = r#"{"protect":true}"#;
        assert_eq!(call("POST", "/api/events/porch-1700000000/protect", "olga", None, keep).await.0, StatusCode::OK);
        assert_eq!(call("POST", "/api/events/garage-1700000000/protect", "olga", None, keep).await.0, StatusCode::NOT_FOUND, "another camera");
        assert_eq!(call("POST", "/api/events/garage-1700000000/protect", "otto", None, keep).await.0, StatusCode::OK);
        assert_eq!(call("POST", "/api/events/garage-1700000000/protect", "vic", None, keep).await.0, StatusCode::FORBIDDEN);
        // A clip of a camera they cannot see (or that is not there): not found.
        let (s, b) = call("POST", "/api/clips/record", "olga", None, r#"{"camera":0}"#).await;
        assert!(s == StatusCode::OK && b.contains("camera not found"), "{s} {b}");
        assert_eq!(call("POST", "/api/clips/record", "vic", None, r#"{"camera":0}"#).await.0, StatusCode::FORBIDDEN);
        // Camera settings: through to the handler (no such camera here) for
        // operators; never adding or removing cameras.
        assert_eq!(call("PUT", "/api/cameras/0", "otto", None, r#"{"motion":false}"#).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call("PUT", "/api/cameras/0", "vic", None, r#"{"motion":false}"#).await.0, StatusCode::FORBIDDEN);
        // Admin-only, whatever an operator may do.
        for (m, p, b) in [
            ("GET", "/api/settings", ""),
            ("POST", "/api/settings", "{}"),
            ("GET", "/api/users", ""),
            ("POST", "/api/users", r#"{"username":"otto","role":"admin"}"#),
            ("POST", "/api/users", r#"{"username":"otto","perms":{"ptz":true}}"#),
            ("POST", "/api/users", r#"{"username":"otto","cameras":[]}"#),
            ("GET", "/api/users/ann/tokens", ""),
            ("POST", "/api/cameras", r#"{"name":"x","url":"rtsp://x/"}"#),
            ("DELETE", "/api/cameras/0", ""),
            ("POST", "/api/cameras/0/enable", r#"{"enabled":false}"#),
            ("POST", "/api/overlay/x", "1"),
            ("POST", "/api/system/restart", ""),
            ("GET", "/api/logs", ""),
            ("GET", "/api/health", ""),
            ("GET", "/api/audit", ""),
            ("POST", "/api/alert-rules", "{}"),
            ("GET", "/api/offsite", ""),
            ("POST", "/api/update/install", ""),
            ("POST", "/api/storage/missing/scan", ""),
            ("POST", "/api/links/code", "{}"),
            ("POST", "/api/events/porch-1700000000/ai/recheck", ""),
            ("POST", "/api/event-copy/test", r#"{"path":"/tmp"}"#),
        ] {
            assert_eq!(call(m, p, "otto", None, b).await.0, StatusCode::FORBIDDEN, "{m} {p}");
        }
    }
}
