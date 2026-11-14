//! Linked AmberSight servers (server switcher).
//!
//! Linking: an admin on server B makes a one-time link code. An admin on A
//! enters B's address and the code; A downloads B's certificate authority,
//! the admin checks its fingerprint, and A calls B's `/link/accept` over TLS
//! pinned to it, sending its own name, address, CA and a fresh key for B.
//! B answers with its name and a key for A. Each side keeps the other's
//! address and CA, the key it presents, and the hash of the key it accepts.
//!
//! Switching: a signed-in user on A picks B; A asks B (`/link/signin`) for a
//! one-time sign-in code for the same user name, never an admin one unless
//! the user is an admin on A too. The browser opens `https://B/#pair=<code>`.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Path as AxPath, Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use sha2::{Digest, Sha256};

use crate::capture::net::{self, Target};
use crate::config::LinkConfig;
use crate::engine::Engine;

const CODE_SECS: u64 = 15 * 60;

fn sha_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(n: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Constant-time string comparison.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn codes() -> &'static Mutex<Vec<(String, Instant)>> {
    static C: OnceLock<Mutex<Vec<(String, Instant)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// A one-time link code (letters and digits that are hard to mix up).
pub fn new_code() -> String {
    const ABC: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    use rand::RngCore;
    let mut b = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let code: String = b.iter().map(|x| ABC[*x as usize % ABC.len()] as char).collect();
    let mut c = codes().lock().unwrap();
    c.retain(|(_, t)| t.elapsed() < Duration::from_secs(CODE_SECS));
    c.push((code.clone(), Instant::now()));
    code
}

fn take_code(code: &str) -> bool {
    let code = code.trim().to_ascii_uppercase().replace(['-', ' '], "");
    let mut c = codes().lock().unwrap();
    c.retain(|(_, t)| t.elapsed() < Duration::from_secs(CODE_SECS));
    match c.iter().position(|(x, _)| same(x, &code)) {
        Some(i) => {
            c.remove(i);
            true
        }
        None => false,
    }
}

/// This server's name for other servers.
pub fn own_name(engine: &Engine) -> String {
    engine.config.lock().unwrap().server.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(crate::capture::proto::machine_name)
}

/// This server's name, for places without the engine at hand (overlays).
pub fn own_name_cached() -> String {
    NAME.get().cloned().unwrap_or_else(crate::capture::proto::machine_name)
}
static NAME: OnceLock<String> = OnceLock::new();

/// Remember the server's name at startup, before cameras start (renames show
/// in burned-in text after a restart).
pub fn remember_name(cfg: &crate::config::Config) {
    let n = cfg.server.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(crate::capture::proto::machine_name);
    let _ = NAME.set(n);
}

fn own_ca() -> Result<String> {
    crate::tls::current()
        .and_then(|r| r.ca_pem())
        .ok_or_else(|| anyhow!("this server needs HTTPS with its own certificate authority to link (turn on HTTPS in its settings)"))
}

fn clean_name(s: &str) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(64).collect()
}

fn api_error(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn bearer(h: &axum::http::HeaderMap) -> String {
    h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").trim().to_string()
}

/// The link whose key this request presents.
pub(crate) fn authenticate(engine: &Engine, h: &axum::http::HeaderMap) -> Option<LinkConfig> {
    let token = bearer(h);
    if token.len() < 32 {
        return None;
    }
    let hash = sha_hex(&token);
    engine.config.lock().unwrap().links.iter().find(|l| same(&l.peer_token_sha256, &hash)).cloned()
}

/// Call a linked server.
pub(crate) async fn call(l: &LinkConfig, method: &str, path: &str, body: &serde_json::Value) -> Result<(u16, serde_json::Value)> {
    let t = Target::parse(&l.url)?;
    let tls = net::pinned(&l.ca_pem)?;
    let payload = body.to_string();
    let (status, bytes) = net::request_with(&t, tls, method, path, &[("Authorization", format!("Bearer {}", l.token))], Some("application/json"), payload.as_bytes()).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)))
}

// ------------------------------------------------------------- admin API

/// `POST /api/links/code`: a code another server uses to link to this one.
pub async fn api_code(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<crate::web::Caller>, headers: axum::http::HeaderMap, axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>, bytes: axum::body::Bytes) -> Response {
    if let Err(e) = own_ca() {
        return api_error(StatusCode::CONFLICT, format!("{e:#}"));
    }
    // A code lets another server link to this one: it takes the caller's own password.
    if caller.0 != "(open)" {
        let proof: crate::account_http::MyProof = serde_json::from_slice(&bytes).unwrap_or_default();
        if let Err(r) = crate::account_http::prove_admin(&engine, &caller.0, peer.ip(), &headers, &proof, caller.1).await {
            return r;
        }
        let cfg = engine.config.lock().unwrap();
        if !crate::users_http::caller_may_change_accounts(&cfg, &caller.0) || crate::auth::generation(&caller.0) != caller.1 {
            return api_error(StatusCode::UNAUTHORIZED, "authentication required");
        }
    }
    let fp = crate::tls::current().and_then(|r| r.info().ca_fingerprint);
    Json(serde_json::json!({ "code": new_code(), "expires_in": CODE_SECS, "name": own_name(&engine), "fingerprint": fp })).into_response()
}

#[derive(serde::Deserialize)]
pub struct ProbeReq {
    url: String,
}

fn normalize_url(u: &str) -> Result<String> {
    let mut u = u.trim().trim_end_matches('/').to_string();
    if !u.contains("://") {
        u = format!("https://{u}");
    }
    let t = Target::parse(&u)?;
    Ok(t.base_url())
}

async fn fetch_ca(url: &str) -> Result<String> {
    let t = Target::parse(url)?;
    let (status, body) = net::request(&t, net::first_contact(), "GET", "/tls/ca.pem", None, b"").await.with_context(|| format!("could not reach {}", t.base_url()))?;
    if status == 404 {
        bail!("that server has no certificate authority to download (it needs HTTPS with its own certificate authority)");
    }
    anyhow::ensure!(status == 200, "downloading its certificate failed (HTTP {status})");
    String::from_utf8(body).map_err(|_| anyhow!("its certificate is not text"))
}

/// `POST /api/links/probe {url}`: the other server's certificate
/// fingerprint, for the admin to compare before linking.
pub async fn api_probe(Json(r): Json<ProbeReq>) -> Response {
    let url = match normalize_url(&r.url) {
        Ok(u) => u,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    match fetch_ca(&url).await.and_then(|pem| crate::capture::pair::ca_fingerprint(&pem)) {
        Ok(fp) => Json(serde_json::json!({ "url": url, "fingerprint": fp })).into_response(),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
pub struct LinkReq {
    url: String,
    code: String,
    /// The fingerprint the admin confirmed.
    fingerprint: String,
    /// This server's address as the other one should reach it.
    own_url: String,
    /// The caller's own password and code.
    #[serde(flatten)]
    proof: crate::account_http::MyProof,
}

/// `POST /api/links`: link with another server using its code.
pub async fn api_link(State(engine): State<Arc<Engine>>, caller: Option<axum::Extension<crate::web::Caller>>, headers: axum::http::HeaderMap, axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>, Json(r): Json<LinkReq>) -> Response {
    let (who, generation) = caller.map(|c| (c.0 .0, c.0 .1)).unwrap_or_default();
    // (What costs nothing to check goes first: a code is not used up on a bad address.)
    if let Err(e) = normalize_url(&r.url).and_then(|_| normalize_url(&r.own_url)) {
        return api_error(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    if who != "(open)" {
        if let Err(resp) = crate::account_http::prove_admin(&engine, &who, peer.ip(), &headers, &r.proof, generation).await {
            return resp;
        }
    }
    match link(&engine, r, &who, generation).await {
        Ok(l) => Json(serde_json::json!({ "id": l.id, "name": l.name })).into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn link(engine: &Arc<Engine>, r: LinkReq, who: &str, generation: u64) -> Result<LinkConfig> {
    let url = normalize_url(&r.url)?;
    let own_url = normalize_url(&r.own_url).context("this server's address")?;
    let own_ca = own_ca()?;
    let pem = fetch_ca(&url).await?;
    let fp = crate::capture::pair::ca_fingerprint(&pem)?;
    if !same(&fp.to_ascii_uppercase(), &r.fingerprint.trim().to_ascii_uppercase()) {
        bail!("the other server's certificate changed since you checked it; check the fingerprint again");
    }
    if engine.config.lock().unwrap().links.iter().any(|l| l.url == url) {
        bail!("this server is already linked with {url}");
    }
    let key_for_them = random_hex(32);
    let mut l = LinkConfig {
        id: random_hex(8),
        name: String::new(),
        url: url.clone(),
        ca_pem: pem,
        token: String::new(),
        peer_token_sha256: sha_hex(&key_for_them),
        added: chrono::Utc::now().timestamp(),
        ..Default::default()
    };
    let t = Target::parse(&url)?;
    let body = serde_json::json!({ "code": r.code, "name": own_name(engine), "url": own_url, "ca_pem": own_ca, "token": key_for_them });
    let (status, bytes) = net::request(&t, net::pinned(&l.ca_pem)?, "POST", "/link/accept", Some("application/json"), body.to_string().as_bytes())
        .await
        .context("linking failed")?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    match status {
        200 => {}
        401 => bail!("the other server did not accept the code (wrong, used, or expired): make a new one there"),
        429 => bail!("too many wrong codes; wait a few minutes"),
        404 => bail!("that server does not support linking yet (update AmberSight there)"),
        s => bail!("the other server refused (HTTP {s}): {}", v["error"].as_str().unwrap_or("")),
    }
    l.name = clean_name(v["name"].as_str().unwrap_or(""));
    l.token = v["token"].as_str().unwrap_or("").to_string();
    anyhow::ensure!(l.token.len() >= 32, "the other server sent no key");
    if l.name.is_empty() {
        l.name = t.host.clone();
    }
    let saved = {
        let mut c = engine.config.lock().unwrap();
        // (Linking takes a while: the admin may have been disabled meanwhile.)
        if !crate::users_http::caller_may_change_accounts(&c, who) || crate::auth::generation(who) != generation {
            bail!("this account can no longer change this");
        }
        c.links.push(l.clone());
        let r = c.save(&engine.config_path);
        if r.is_err() {
            c.links.retain(|x| x.id != l.id);
        }
        r
    };
    if let Err(e) = saved {
        // Do not leave the other server linked to us when we could not keep it.
        let _ = tokio::time::timeout(Duration::from_secs(5), call(&l, "POST", "/link/unlink", &serde_json::json!({}))).await;
        return Err(e.context("could not save the link here"));
    }
    tracing::info!("linked with server '{}' at {}", l.name, l.url);
    Ok(l)
}

/// `GET /api/links`: linked servers (no secrets), each checked for being reachable.
#[derive(serde::Deserialize)]
pub struct ListQ {
    #[serde(default)]
    check: Option<u8>,
}

pub async fn api_list(
    State(engine): State<Arc<Engine>>,
    axum::Extension(admin): axum::Extension<crate::web::IsAdmin>,
    axum::extract::Query(q): axum::extract::Query<ListQ>,
) -> Response {
    let links = engine.config.lock().unwrap().links.clone();
    let check = q.check == Some(1);
    let checks = links.iter().map(|l| async move {
        if !check {
            return (None, None, None);
        }
        let r = tokio::time::timeout(Duration::from_secs(4), call(l, "GET", "/link/ping", &serde_json::Value::Null)).await;
        match r {
            Ok(Ok((200, v))) => (Some(true), v["version"].as_str().map(String::from), None),
            Ok(Ok((401, _))) => (Some(false), None, Some("it no longer knows this server (unlinked there?)".to_string())),
            Ok(Ok((s, _))) => (Some(false), None, Some(format!("HTTP {s}"))),
            Ok(Err(e)) => (Some(false), None, Some(format!("{e:#}"))),
            Err(_) => (Some(false), None, Some("did not answer".to_string())),
        }
    });
    let states = futures::future::join_all(checks).await;
    let list: Vec<serde_json::Value> = links
        .iter()
        .zip(states)
        .map(|(l, (online, version, error))| {
            let mut v = serde_json::json!({ "id": l.id, "name": l.name, "online": online });
            if admin.0 {
                v["url"] = l.url.clone().into();
                v["version"] = version.into();
                v["error"] = error.into();
                v["added"] = l.added.into();
                v["fingerprint"] = crate::capture::pair::ca_fingerprint(&l.ca_pem).ok().into();
                v["share"] = l.share.into();
                v["show"] = l.show.into();
                v["signin"] = (if l.signin.is_empty() { "viewers" } else { l.signin.as_str() }).into();
                v["show_error"] = if l.show { crate::remote::link_error(&l.id).into() } else { serde_json::Value::Null };
            }
            v
        })
        .collect();
    Json(serde_json::json!({ "name": own_name(&engine), "links": list })).into_response()
}

#[derive(serde::Deserialize)]
pub struct RenameReq {
    #[serde(default)]
    name: Option<String>,
    /// Let that server show this server's cameras.
    #[serde(default)]
    share: Option<bool>,
    /// Show that server's cameras here.
    #[serde(default)]
    show: Option<bool>,
    /// Who may be signed in here from that server: all, viewers, off.
    #[serde(default)]
    signin: Option<String>,
    /// Reach it at another address (checked first: same server, same key).
    #[serde(default)]
    url: Option<String>,
    /// The caller's own password and code, for setting `signin` to "all".
    #[serde(flatten)]
    proof: crate::account_http::MyProof,
}

/// `PUT /api/links/:id {name}`
/// (`id` "self" renames this server.)
pub async fn api_rename(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<crate::web::Caller>, headers: axum::http::HeaderMap, axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>, AxPath(id): AxPath<String>, Json(r): Json<RenameReq>) -> Response {
    let name = r.name.as_deref().map(clean_name);
    if name.as_deref() == Some("") {
        return api_error(StatusCode::BAD_REQUEST, "enter a name");
    }
    if r.signin.as_deref().is_some_and(|v| !matches!(v, "all" | "viewers" | "off")) {
        return api_error(StatusCode::BAD_REQUEST, "signin must be all, viewers or off");
    }
    // Letting that server sign people in here as admins takes the caller's own password.
    let gives_admins = |c: &crate::config::Config| caller.0 != "(open)" && r.signin.as_deref() == Some("all") && c.links.iter().any(|l| l.id == id && l.signin != "all");
    let proved = gives_admins(&engine.config.lock().unwrap());
    if proved {
        if let Err(resp) = crate::account_http::prove_admin(&engine, &caller.0, peer.ip(), &headers, &r.proof, caller.1).await {
            return resp;
        }
    }
    // New address: it must answer as the same server (its certificate is
    // pinned and it must accept this link's key) before it is saved.
    let new_url = match r.url.as_deref() {
        None => None,
        Some(u) => {
            let url = match normalize_url(u) {
                Ok(u) => u,
                Err(e) => return api_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
            };
            let Some(mut l) = engine.config.lock().unwrap().links.iter().find(|l| l.id == id).cloned() else {
                return api_error(StatusCode::NOT_FOUND, "no such link");
            };
            l.url = url.clone();
            match tokio::time::timeout(Duration::from_secs(8), call(&l, "GET", "/link/ping", &serde_json::Value::Null)).await {
                Ok(Ok((200, _))) => Some(url),
                Ok(Ok((s, _))) => return api_error(StatusCode::BAD_REQUEST, format!("{url} answered, but not as {} (HTTP {s})", l.name)),
                Ok(Err(e)) => return api_error(StatusCode::BAD_REQUEST, format!("could not reach {} at {url}: {e:#}", l.name)),
                Err(_) => return api_error(StatusCode::BAD_REQUEST, format!("{url} did not answer")),
            }
        }
    };
    let refresh_show = {
        let mut c = engine.config.lock().unwrap();
        // (Checked again now the slow steps are over.)
        if proved && (!crate::users_http::caller_may_change_accounts(&c, &caller.0) || crate::auth::generation(&caller.0) != caller.1) {
            return api_error(StatusCode::UNAUTHORIZED, "authentication required");
        }
        if !proved && gives_admins(&c) {
            return api_error(StatusCode::CONFLICT, "this link changed meanwhile: try again");
        }
        let before = (c.server.name.clone(), c.links.clone());
        if id == "self" {
            if let Some(n) = name {
                c.server.name = Some(n);
            }
        } else {
            let Some(l) = c.links.iter_mut().find(|l| l.id == id) else { return api_error(StatusCode::NOT_FOUND, "no such link") };
            if let Some(n) = name {
                l.name = n;
            }
            if let Some(v) = r.share {
                l.share = v;
            }
            if let Some(v) = r.show {
                l.show = v;
            }
            if let Some(v) = r.signin.as_deref() {
                l.signin = v.to_string();
            }
            if let Some(u) = new_url {
                l.url = u;
            }
        }
        if let Err(e) = c.save(&engine.config_path) {
            // Not half done: what cannot be kept is not in effect either.
            tracing::error!("could not save a link change: {e:#}");
            c.server.name = before.0;
            c.links = before.1;
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "could not save the change; nothing was changed. See the server's log");
        }
        r.show.is_some()
    };
    // Turning "show" on: fetch its camera list now rather than in 20 s.
    crate::remote::number_links(&engine);
    if refresh_show {
        crate::remote::refresh(&engine, &id).await;
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// `DELETE /api/links/:id`: unlink here, and tell the other side.
pub async fn api_remove(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<String>) -> Response {
    let l = {
        let mut c = engine.config.lock().unwrap();
        let Some(i) = c.links.iter().position(|l| l.id == id) else { return api_error(StatusCode::NOT_FOUND, "no such link") };
        let l = c.links.remove(i);
        if let Err(e) = c.save(&engine.config_path) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        }
        l
    };
    let told = matches!(tokio::time::timeout(Duration::from_secs(5), call(&l, "POST", "/link/unlink", &serde_json::json!({}))).await, Ok(Ok((200, _))));
    Json(serde_json::json!({ "removed": id, "other_side_told": told })).into_response()
}

/// `POST /api/links/:id/open`: where the browser goes to open the other
/// server signed in as the same user.
pub async fn api_open(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<String>,
    axum::Extension(caller): axum::Extension<crate::web::Caller>,
    axum::Extension(admin): axum::Extension<crate::web::IsAdmin>,
) -> Response {
    let Some(l) = engine.config.lock().unwrap().links.iter().find(|l| l.id == id).cloned() else {
        return api_error(StatusCode::NOT_FOUND, "no such link");
    };
    let open_mode = caller.0 == "(open)";
    let req = serde_json::json!({ "user": if open_mode { serde_json::Value::Null } else { caller.0.clone().into() }, "admin": admin.0 });
    match tokio::time::timeout(Duration::from_secs(8), call(&l, "POST", "/link/signin", &req)).await {
        Ok(Ok((200, v))) => {
            let url = match v["code"].as_str() {
                Some(code) => format!("{}/#pair={code}", l.url),
                None => format!("{}/", l.url),
            };
            Json(serde_json::json!({ "url": url, "name": l.name })).into_response()
        }
        Ok(Ok((403, v))) => api_error(StatusCode::FORBIDDEN, v["error"].as_str().unwrap_or("the other server refused").to_string()),
        Ok(Ok((401, _))) => api_error(StatusCode::BAD_GATEWAY, format!("{} no longer accepts this link; link the servers again", l.name)),
        Ok(Ok((s, _))) => api_error(StatusCode::BAD_GATEWAY, format!("{} answered HTTP {s}", l.name)),
        Ok(Err(e)) => api_error(StatusCode::BAD_GATEWAY, format!("could not reach {}: {e:#}", l.name)),
        Err(_) => api_error(StatusCode::GATEWAY_TIMEOUT, format!("{} did not answer", l.name)),
    }
}

// ------------------------------------------------------- server-to-server

#[derive(serde::Deserialize)]
struct AcceptReq {
    code: String,
    name: String,
    url: String,
    ca_pem: String,
    token: String,
}

/// Link codes not used yet stop working (an admin was just disabled).
pub fn cancel_codes() {
    codes().lock().unwrap().clear();
}

/// `POST /link/accept`: another server links using one of our codes.
pub async fn accept(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let ip = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip()).unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    if crate::auth::is_locked(ip) {
        return api_error(StatusCode::TOO_MANY_REQUESTS, "too many wrong codes; try again in a few minutes");
    }
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 64 * 1024).await else { return api_error(StatusCode::BAD_REQUEST, "bad request") };
    let Ok(r) = serde_json::from_slice::<AcceptReq>(&bytes) else { return api_error(StatusCode::BAD_REQUEST, "bad request") };
    if !take_code(&r.code) {
        crate::auth::record_failure(ip);
        let _ = engine.db.insert_audit("(linked server)", &ip.to_string(), "server link refused (bad code)", &clean_name(&r.name), 401);
        return api_error(StatusCode::UNAUTHORIZED, "wrong or expired link code");
    }
    let url = match normalize_url(&r.url) {
        Ok(u) => u,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, format!("its address: {e:#}")),
    };
    if r.token.len() < 32 || net::pinned(&r.ca_pem).is_err() {
        return api_error(StatusCode::BAD_REQUEST, "missing key or certificate");
    }
    crate::auth::record_success(ip);
    let key_for_them = random_hex(32);
    let name = clean_name(&r.name);
    let l = LinkConfig {
        id: random_hex(8),
        name: if name.is_empty() { url.clone() } else { name },
        url,
        ca_pem: r.ca_pem,
        token: r.token,
        peer_token_sha256: sha_hex(&key_for_them),
        added: chrono::Utc::now().timestamp(),
        ..Default::default()
    };
    {
        let mut c = engine.config.lock().unwrap();
        c.links.retain(|x| x.url != l.url);
        c.links.push(l.clone());
        if let Err(e) = c.save(&engine.config_path) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        }
    }
    let _ = engine.db.insert_audit("(linked server)", &ip.to_string(), "server linked", &l.name, 200);
    tracing::info!("linked with server '{}' at {} (from {ip})", l.name, l.url);
    crate::account_http::tell_admins(&engine, format!("server linked: {}", l.name), format!("A server called {} was linked to this one, from {ip}, with a code an admin made. If that was not meant to happen, remove it in Settings, Linked servers.", l.name));
    Json(serde_json::json!({ "name": own_name(&engine), "token": key_for_them })).into_response()
}

fn link_from(engine: &Engine, req: &Request) -> Result<LinkConfig, Response> {
    let ip = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip()).unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    if crate::auth::is_locked(ip) {
        return Err(api_error(StatusCode::TOO_MANY_REQUESTS, "locked out"));
    }
    // A wrong key is not counted toward the sign-in lockout: keys are 256-bit
    // (not guessable), and a server unlinked on one side only would otherwise
    // lock its own address out by checking the link.
    authenticate(engine, req.headers()).ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "unknown server"))
}

/// `GET /link/ping`
pub async fn ping(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    if let Err(r) = link_from(&engine, &req) {
        return r;
    }
    Json(serde_json::json!({ "name": own_name(&engine), "version": env!("CARGO_PKG_VERSION") })).into_response()
}

/// `POST /link/unlink`: the other server removed the link.
pub async fn unlink(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let l = match link_from(&engine, &req) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let mut c = engine.config.lock().unwrap();
    c.links.retain(|x| x.id != l.id);
    let _ = c.save(&engine.config_path);
    tracing::info!("server '{}' removed its link with this server", l.name);
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// `POST /link/signin {user, admin}`: a one-time sign-in code for a user
/// arriving from the linked server.
pub async fn signin(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let l = match link_from(&engine, &req) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 4096).await else { return api_error(StatusCode::BAD_REQUEST, "bad request") };
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let accounts = crate::auth::accounts(&engine.config.lock().unwrap());
    // Sign-in off here: nothing to carry over.
    if accounts.is_empty() {
        return Json(serde_json::json!({ "code": null })).into_response();
    }
    // This server's choice for that link: nobody, or never as an admin.
    if l.signin == "off" {
        return api_error(StatusCode::FORBIDDEN, format!("sign-in from {} is turned off at {}: sign in there", l.name, own_name(&engine)));
    }
    let Some(user) = v["user"].as_str() else {
        return api_error(StatusCode::FORBIDDEN, format!("sign-in is on at {}: sign in there", own_name(&engine)));
    };
    // (Names in brackets are the server's own, never someone's account.)
    let Some((name, _, is_admin)) = accounts.iter().find(|(u, _, _)| u == user && !u.starts_with('(')) else {
        return api_error(StatusCode::FORBIDDEN, format!("{} has no account named \"{user}\": sign in there, or add that account there", own_name(&engine)));
    };
    // Admin rights only where an admin chose that for this link: the other
    // server decides who has a name, so an admin there could otherwise become
    // any admin here (and turn a disabled account back on).
    let admin_ok = v["admin"].as_bool().unwrap_or(false) && l.signin == "all";
    // Nor is anyone let into an admin's account without those rights: it sees
    // every camera and gets that admin's alerts. They go to this server's own
    // sign-in instead (a browser already signed in here stays as it is).
    // An operator's account likewise (a session cannot be held to a viewer's
    // rights in an operator's account: it would have the operator's).
    let is_operator = crate::auth::is_operator(&engine.config.lock().unwrap(), name);
    if (*is_admin || is_operator) && !admin_ok {
        let _ = engine.db.insert_audit("(linked server)", "", "sign-in to an admin account not carried over", &l.name, 200);
        return Json(serde_json::json!({ "code": null })).into_response();
    }
    // An account with two-step codes signs in with its code, here: another
    // server cannot vouch for that, whatever this link allows. (Read before the
    // check: if it comes on just after, no code is made.)
    let generation = crate::auth::generation(name);
    if crate::auth::two_step_on(name) != Ok(false) {
        let _ = engine.db.insert_audit("(linked server)", "", "sign-in to a two-step account not carried over", &l.name, 200);
        return Json(serde_json::json!({ "code": null })).into_response();
    }
    let Some(code) = crate::auth::new_link_signin_code(name, admin_ok, generation) else {
        return Json(serde_json::json!({ "code": null })).into_response();
    };
    let _ = engine.db.insert_audit(name, "", "sign-in code for a user from a linked server", &l.name, 200);
    Json(serde_json::json!({ "code": code })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_single_use() {
        let c = new_code();
        assert_eq!(c.len(), 12);
        assert!(take_code(&c.to_lowercase()));
        assert!(!take_code(&c));
        assert!(!take_code("NOPE"));
    }

    #[test]
    fn urls_are_normalized() {
        assert_eq!(normalize_url("nvr:8090").unwrap(), "https://nvr:8090");
        assert_eq!(normalize_url("https://10.0.0.5:8090/").unwrap(), "https://10.0.0.5:8090");
        assert!(normalize_url("http://x:1").is_err());
    }

    #[test]
    fn compare() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
    }
}
