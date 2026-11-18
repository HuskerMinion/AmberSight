//! Web API for Settings > Connections > Apple Home (admins only; device
//! tokens are refused by the sign-in layer):
//!   GET  /api/homekit           settings, each accessory's pairing state, the
//!                               pairing window (who is pairing, and whether it
//!                               is this browser's device); the setup code and
//!                               the QR code only while the window is open
//!   PUT  /api/homekit           {enabled?, name?, port?, bind?, mode?, cameras?: {name: bool}}
//!   POST /api/homekit/pairing   {action: "open", camera?} | {action: "stop"} |
//!                               {action: "restart", address}: the pairing window
//!   POST /api/homekit/reset     forget every controller, new setup code
//!   POST /api/homekit/unpair    {camera}: forget one standalone camera's controllers

use std::net::IpAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, State as AxState},
    Extension,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

use super::pairing::{Open, WINDOW};
use super::server::{Instance, Kind};
use super::state::State;
use super::{bridge_name, mdns, running, state_dir, status, HomekitConfig};
use crate::engine::Engine;

fn qr_svg(text: &str) -> Option<String> {
    let q = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M).ok()?;
    Some(q.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).quiet_zone(true).build())
}

/// One accessory's part: pairing state; its QR code while the pairing
/// window is open for it.
fn accessory_json(s: &State, category: u8, open: bool) -> serde_json::Value {
    let mut v = json!({
        "paired": s.paired(),
        "too_many_tries": s.failed_tries >= super::state::MAX_TRIES,
        "controllers": s.pairings.iter().map(|p| json!({ "id": p.id, "admin": p.admin })).collect::<Vec<_>>(),
        "pairing_open": open,
    });
    if s.pairable() && open {
        let uri = mdns::setup_uri(&s.setup_code, &s.setup_id, category as u64);
        v["setup_uri"] = uri.clone().into();
        v["qr_svg"] = qr_svg(&uri).into();
    }
    v
}

/// `viewer`: the address of the admin's browser, to mark the pair-setup
/// from it as this device.
fn status_json(engine: &Engine, viewer: IpAddr) -> serde_json::Value {
    let (hk, name, cams) = {
        let c = engine.config.lock().unwrap();
        let cams: Vec<serde_json::Value> = c.cameras.iter().map(|cam| json!({ "name": cam.name, "homekit": cam.homekit })).collect();
        (c.homekit.clone(), bridge_name(&c), cams)
    };
    let st = status();
    let r = running();
    let mut accessories = Vec::new();
    let mut any_pairable = false;
    let mut sessions = 0;
    let mut code: Option<String> = None;
    let mut window = serde_json::Value::Null;
    if let Some(r) = &r {
        code = Some(r.main.lock().unwrap().setup_code.clone());
        let open = r.window.lock().unwrap().current().cloned();
        let mut served = r.served.clone();
        served.sort_by_key(|s| s.inst.port);
        for s in served {
            let i = &s.inst;
            sessions += i.session_count();
            let open_here = hk.enabled && open.as_ref().is_some_and(|o| o.target == i.kind);
            if open_here {
                window = window_json(open.as_ref().unwrap(), i, viewer);
            }
            let state = i.state.lock().unwrap();
            any_pairable |= state.pairable() && open_here;
            let mut v = accessory_json(&state, i.category(), open_here);
            v["kind"] = match &i.kind {
                Kind::Bridge => "bridge",
                Kind::Camera(_) => "camera",
            }
            .into();
            if let Kind::Camera(c) = &i.kind {
                v["camera"] = c.clone().into();
            }
            v["name"] = i.name.clone().into();
            v["port"] = i.port.into();
            v["streams"] = i.db.lock().unwrap().cameras().iter().map(|c| i.streams.count(c.aid)).sum::<usize>().into();
            accessories.push(v);
        }
    }
    let paired_any = accessories.iter().any(|a| a["paired"] == true);
    let stored_paired = r.is_none() && State::load(&state_dir(engine)).ok().flatten().is_some_and(|s| s.paired());
    let mut out = json!({
        "enabled": hk.enabled,
        "port": hk.port,
        "bind": hk.bind,
        "mode": if hk.standalone() { "standalone" } else { "bridged" },
        "name": hk.name,
        "bridge_name": name,
        "listening": st.listening,
        "error": st.error,
        "sessions": sessions,
        "live_streams": super::stream::active(),
        // (Windows: the ports to open by hand when AmberSight could not add its rule.)
        "firewall_note": if hk.enabled { super::firewall_note() } else { None },
        "paired": paired_any || stored_paired,
        "cameras": cams,
        "accessories": accessories,
        // The pairing window while it is open: for which accessory, how
        // long it has left, and who is pairing now.
        "pairing": window,
    });
    // The one setup code, while the pairing window is open for an
    // accessory that can be added.
    if any_pairable && hk.enabled {
        out["setup_code"] = code.into();
    }
    out
}

/// The open pairing window: its accessory, time left, the address of the
/// pair-setup in progress (and whether it is the admin's own browser's),
/// and the addresses stopped in it.
fn window_json(o: &Open, i: &Instance, viewer: IpAddr) -> serde_json::Value {
    let now = i.pairing_now();
    json!({
        "kind": match &i.kind { Kind::Bridge => "bridge", Kind::Camera(_) => "camera" },
        "camera": match &i.kind { Kind::Camera(c) => Some(c.clone()), Kind::Bridge => None },
        "name": i.name,
        "seconds_left": o.until.saturating_duration_since(std::time::Instant::now()).as_secs(),
        "pairing_from": now.map(|(ip, _)| ip.to_string()),
        "pairing_for_seconds": now.map(|(_, d)| d.as_secs()),
        "pairing_from_this_device": now.is_some_and(|(ip, _)| same_device(ip, viewer)),
        "refused": o.refused.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
    })
}

/// Whether the pair-setup from `pairing` comes from the admin's browser at
/// `viewer` (the accessory servers see IPv4 inside IPv6 as plain IPv4; the
/// web server may not).
fn same_device(pairing: IpAddr, viewer: IpAddr) -> bool {
    !viewer.is_unspecified() && pairing.to_canonical() == viewer.to_canonical()
}

/// `GET /api/homekit`.
/// (`ConnectInfo` is the visitor's address, also behind a trusted proxy:
/// see `web::real_client`.)
pub async fn get(AxState(engine): AxState<Arc<Engine>>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>) -> Json<serde_json::Value> {
    Json(status_json(&engine, peer.ip()))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    enabled: Option<bool>,
    /// Empty: the server's name.
    name: Option<String>,
    port: Option<u16>,
    /// "[::]", "0.0.0.0" or one address of this computer.
    bind: Option<String>,
    /// "standalone" or "bridged".
    mode: Option<String>,
    /// Camera name -> shown in Apple Home or not. Others keep their setting.
    cameras: Option<std::collections::HashMap<String, bool>>,
}

fn bad(m: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": m }))).into_response()
}

/// `PUT /api/homekit {enabled?, name?, port?, bind?, mode?, cameras?: {name: bool}}`.
pub async fn put(AxState(engine): AxState<Arc<Engine>>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, Json(u): Json<Update>) -> Response {
    let restart_listener = u.enabled.is_some() || u.port.is_some() || u.name.is_some() || u.bind.is_some() || u.mode.is_some() || u.cameras.is_some();
    {
        let mut cfg = engine.config.lock().unwrap();
        let mut new = cfg.homekit.clone();
        if let Some(e) = u.enabled {
            new.enabled = e;
        }
        if let Some(p) = u.port {
            new.port = p;
        }
        if let Some(n) = &u.name {
            let n = n.trim();
            if n.len() > 63 {
                return bad("the name may be at most 63 bytes (about 60 letters)".into());
            }
            new.name = (!n.is_empty()).then(|| n.to_string());
        }
        if let Some(b) = &u.bind {
            if let Err(e) = HomekitConfig::check_bind(b) {
                return bad(e);
            }
            new.bind = b.trim().to_string();
        }
        if let Some(m) = &u.mode {
            if let Err(e) = HomekitConfig::check_cameras(m) {
                return bad(e);
            }
            new.cameras = m.clone();
        }
        {
            let mut probe = cfg.clone();
            probe.homekit = new.clone();
            if let Err(e) = HomekitConfig::check_port(new.port, &probe) {
                return bad(e);
            }
        }
        if let Some(map) = &u.cameras {
            for name in map.keys() {
                if !cfg.cameras.iter().any(|c| c.name == *name) {
                    return bad(format!("no camera named '{name}'"));
                }
            }
            for c in cfg.cameras.iter_mut() {
                if let Some(v) = map.get(&c.name) {
                    c.homekit = *v;
                }
            }
            // The running cameras hold their own copy of their settings.
            for h in engine.cameras.read().unwrap().iter().flatten() {
                let name = h.cfg.read().unwrap().name.clone();
                if let Some(v) = map.get(&name) {
                    h.cfg.write().unwrap().homekit = *v;
                    h.running.write().unwrap().homekit = *v;
                }
            }
        }
        cfg.homekit = new;
        if let Err(e) = cfg.save(&engine.config_path) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("applied, but could not be saved to the config file: {e:#}") }))).into_response();
        }
    }
    if restart_listener {
        // Give the listeners a moment to open or close before reporting on them.
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
    }
    Json(status_json(&engine, peer.ip())).into_response()
}

/// `POST /api/homekit/reset`: forget every paired controller of every
/// accessory and their sessions; a new setup code (shared by all) and new
/// identities. Apple Home sees new accessories (rooms, scenes and
/// automations start over).
pub async fn reset(AxState(engine): AxState<Arc<Engine>>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>) -> Response {
    let dir = state_dir(&engine);
    match tokio::task::spawn_blocking(move || super::reset_all(&dir)).await.unwrap_or_else(|e| Err(anyhow::anyhow!("{e}"))) {
        Ok(()) => {
            tracing::info!("HomeKit: pairing reset from Settings; every controller was forgotten");
            Json(status_json(&engine, peer.ip())).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unpair {
    camera: String,
}

/// `POST /api/homekit/unpair {camera}`: a standalone camera forgets its
/// paired Apple devices (same code and identity: it can be added again).
pub async fn unpair(AxState(engine): AxState<Arc<Engine>>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, Json(u): Json<Unpair>) -> Response {
    let Some(s) = running().and_then(|r| r.served.into_iter().find(|s| s.inst.kind == Kind::Camera(u.camera.clone()))) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "no camera of that name is in Apple Home on its own now" }))).into_response();
    };
    match s.inst.unpair() {
        Ok(()) => {
            tracing::info!("HomeKit: a camera's pairings were removed from Settings");
            Json(status_json(&engine, peer.ip())).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    /// "open", "stop" or "restart".
    action: String,
    /// open, standalone mode: the camera to pair.
    camera: Option<String>,
    /// restart: the address of the pair-setup to stop (as shown).
    address: Option<String>,
}

fn conflict(m: &str) -> Response {
    (StatusCode::CONFLICT, Json(json!({ "error": m }))).into_response()
}

/// An address as typed or shown, IPv4 inside IPv6 as plain IPv4 (as the
/// accessory servers see them).
fn parse_address(a: &str) -> Option<IpAddr> {
    let ip: IpAddr = a.trim().trim_start_matches('[').trim_end_matches(']').parse().ok()?;
    Some(match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    })
}

/// `POST /api/homekit/pairing`: the pairing window. Pair-setup is refused
/// unless it is open for that accessory.
///   {action: "open", camera?}: open it for 5 minutes, for the bridge
///     (bridged) or the camera named (standalone); again for the same one
///     while open: 5 minutes from now, the same window (a pair-setup in
///     progress goes on, the refused addresses stay).
///   {action: "stop"}: close it, ending a pair-setup in progress.
///   {action: "restart", address}: end the pair-setup from that address,
///     refuse the address, and give the same window 5 minutes from now (a
///     pair-setup from another address goes on).
pub async fn pairing(AxState(engine): AxState<Arc<Engine>>, Extension(caller): Extension<crate::web::Caller>, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, Json(p): Json<Pairing>) -> Response {
    let Some(r) = running() else { return conflict("Apple Home is off, or still starting") };
    let standalone = engine.config.lock().unwrap().homekit.standalone();
    let audit = |action: String, detail: &str| {
        let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), &action, detail, 200);
    };
    let find = |kind: &Kind| r.served.iter().find(|s| s.inst.kind == *kind).map(|s| s.inst.clone());
    let insts = || r.served.iter().map(|s| &s.inst);
    // A window past its end that the supervisor has not seen yet (it looks
    // every 500 ms): closed first, as the supervisor would.
    super::close_expired_window(&r.window, insts());
    match p.action.as_str() {
        "open" => {
            let kind = match (standalone, &p.camera) {
                (true, Some(c)) => Kind::Camera(c.clone()),
                (true, None) => return bad("choose the camera to pair".into()),
                (false, None) => Kind::Bridge,
                (false, Some(_)) => return bad("the cameras are behind one bridge: pair the bridge (no camera)".into()),
            };
            let Some(inst) = find(&kind) else {
                return (StatusCode::NOT_FOUND, Json(json!({ "error": "no such camera is in Apple Home now" }))).into_response();
            };
            {
                let st = inst.state.lock().unwrap();
                if st.paired() {
                    return conflict("already in Apple Home: remove its pairings first to add it again");
                }
                if !st.pairable() {
                    return conflict("too many wrong setup codes were tried: reset pairing first");
                }
            }
            // One window at a time: one open for another accessory ends.
            let (_, was) = r.window.lock().unwrap().open(kind.clone(), WINDOW);
            super::window_left(was, Some(&kind), insts());
            inst.advertise.notify_one();
            tracing::info!("HomeKit: pairing window opened for {} from Settings (5 minutes)", inst.name);
            audit("HomeKit: pairing window opened (5 minutes)".into(), &inst.name);
        }
        "stop" => {
            let was = r.window.lock().unwrap().close();
            // (One past its end closed by itself: its accessory is still
            // announced again, but the admin did not close it.)
            let open = was.as_ref().is_some_and(|o| o.until > std::time::Instant::now());
            if let Some(inst) = super::window_left(was, None, insts()).filter(|_| open).and_then(|k| find(&k)) {
                tracing::info!("HomeKit: pairing window closed from Settings");
                audit("HomeKit: pairing window closed".into(), &inst.name);
            }
        }
        "restart" => {
            let Some(ip) = p.address.as_deref().and_then(parse_address) else { return bad("give the address to stop".into()) };
            let Some(o) = r.window.lock().unwrap().restart(ip, WINDOW) else { return conflict("the pairing window is closed") };
            let name = match find(&o.target) {
                Some(inst) => {
                    inst.end_setup(Some(ip));
                    inst.advertise.notify_one();
                    inst.name.clone()
                }
                None => String::new(),
            };
            tracing::info!("HomeKit: a pairing from {ip} was stopped from Settings; that address is refused, and the window started again (5 minutes)");
            audit(format!("HomeKit: pairing from {ip} stopped and refused; window started again (5 minutes)"), &name);
        }
        _ => return bad("action must be open, stop or restart".into()),
    }
    Json(status_json(&engine, peer.ip())).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn addresses_are_read_as_the_accessory_sees_them() {
        use super::parse_address;
        assert_eq!(parse_address(" 192.0.2.7 ").unwrap().to_string(), "192.0.2.7");
        assert_eq!(parse_address("::ffff:192.0.2.7").unwrap().to_string(), "192.0.2.7");
        assert_eq!(parse_address("[fe80::1]").unwrap().to_string(), "fe80::1");
        for bad in ["", "x", "192.0.2", "1.2.3.4:5"] {
            assert!(parse_address(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn this_device_is_the_browser_s_own_address() {
        use super::same_device;
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert!(same_device(ip("192.0.2.7"), ip("192.0.2.7")));
        assert!(same_device(ip("192.0.2.7"), ip("::ffff:192.0.2.7")));
        assert!(same_device(ip("2001:db8::7"), ip("2001:db8::7")));
        assert!(!same_device(ip("192.0.2.7"), ip("192.0.2.8")));
        assert!(!same_device(ip("0.0.0.0"), ip("0.0.0.0")), "an unknown browser address is never this device");
    }
}
