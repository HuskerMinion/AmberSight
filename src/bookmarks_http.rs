//! Web API for timeline bookmarks (db/bookmarks.rs): a moment marked with a
//! note, on one camera or on all of them, optionally kept (the recordings
//! around it are held back from automatic cleanup, see
//! crate::db::KEEP_WINDOW_MS).
//!
//! Who may do what (the sign-in layer lets writes through for operators with
//! the "events" right and admins, see web::operator_route; these handlers
//! check the rest):
//! - everyone signed in reads the bookmarks of the cameras they may see, and
//!   the all-cameras ones; device tokens read too, never write;
//! - operators (with the "events" right) add bookmarks, and change or delete
//!   their own; admins change or delete any;
//! - an account limited to some cameras bookmarks only those cameras, one at
//!   a time (an all-cameras bookmark, kept, would hold other cameras' video).

use axum::{
    body::Bytes,
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

use crate::auth::{Perm, Role};
use crate::db::{Bookmark, BookmarkCap};
use crate::engine::Engine;
use crate::web::{Access, Caller, TokenUsed};

/// Longest note, in characters.
pub const NOTE_MAX: usize = 500;
/// Largest request body (a note of 500 characters, escaped, fits easily).
pub const BODY_MAX: usize = 8 * 1024;
/// Bookmarks a server keeps at most (a script gone wrong cannot fill the database).
const MAX_BOOKMARKS: i64 = 20_000;
/// Kept bookmarks a server holds at most: each holds 2 minutes either side of its moment past
/// retention and the size caps (an all-cameras one holds every camera's).
pub const MAX_KEPT: i64 = 200;
/// Most bookmarks one list returns.
const LIST_MAX: usize = 1000;
/// A moment this far after now is refused (clocks a little apart are fine).
const FUTURE_SLACK_MS: i64 = 60_000;
/// No earlier moment than 2000-01-01.
const EARLIEST_MS: i64 = 946_684_800_000;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn role_of(role: &Option<axum::Extension<Role>>) -> Role {
    role.as_ref().map(|r| r.0).unwrap_or(Role::Viewer)
}

/// The note as saved: trimmed, line ends as `\n`, other control characters
/// dropped; 1 to NOTE_MAX characters.
pub fn clean_note(raw: &str) -> Result<String, &'static str> {
    let s: String = raw.replace("\r\n", "\n").chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
    let s = s.trim().to_string();
    match s.chars().count() {
        0 => Err("write a note"),
        n if n > NOTE_MAX => Err("the note is too long (500 characters at most)"),
        _ => Ok(s),
    }
}

/// A camera named in a request: its slug, if it is one of this server's
/// cameras the caller may see (else None: answered as not found).
fn camera_slug(engine: &Engine, access: &Access, name: &str) -> Option<String> {
    let slug = crate::config::slug(name.trim());
    let known = engine.config.lock().unwrap().cameras.iter().any(|c| crate::config::slug(&c.name) == slug);
    (known && access.can_slug(&slug)).then_some(slug)
}

/// Whether this caller may change or delete `b`.
fn may_edit(role: &Role, access: &Access, user: &str, b: &Bookmark) -> bool {
    let cam_ok = match &b.camera {
        Some(c) => access.can_slug(c),
        None => !access.limited(),
    };
    cam_ok && (role.is_admin() || (role.may(Perm::Events) && b.owner.as_deref() == Some(user)))
}

/// Why this caller may not make `b` a kept bookmark, when it newly is one (was not kept on
/// all cameras before): an all-cameras one holds every camera's video, so it is for admins.
fn keep_refused(role: &Role, before: Option<&Bookmark>, b: &Bookmark) -> Option<&'static str> {
    let was = before.is_some_and(|x| x.keep && x.camera.is_none());
    (b.keep && b.camera.is_none() && !was && !role.is_admin()).then_some("only an admin can keep video for all cameras: pick one camera, or ask an admin")
}

const KEPT_FULL: &str = "this server already keeps video for 200 bookmarks: let some go first";

fn may_see(access: &Access, b: &Bookmark) -> bool {
    b.camera.as_deref().is_none_or(|c| access.can_slug(c))
}

fn to_json(engine: &Engine, b: &Bookmark, can_edit: bool) -> serde_json::Value {
    let name = b.camera.as_ref().map(|s| {
        engine.config.lock().unwrap().cameras.iter().find(|c| crate::config::slug(&c.name) == *s).map(|c| c.name.clone()).unwrap_or_else(|| s.clone())
    });
    serde_json::json!({
        "id": b.id,
        "camera": name,
        "camera_slug": b.camera,
        "at": b.at_ms,
        "note": b.note,
        "user": b.user,
        "created": b.created_ms,
        "updated": b.updated_ms,
        "keep": b.keep,
        "can_edit": can_edit,
    })
}

fn where_text(b: &Bookmark) -> String {
    let when = chrono::DateTime::from_timestamp_millis(b.at_ms).map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string()).unwrap_or_default();
    format!("#{} {} at {}{}", b.id, b.camera.as_deref().unwrap_or("all cameras"), when, if b.keep { " (kept)" } else { "" })
}

/// What an edit changed, in words for the audit log (a kept bookmark moved
/// to another camera moves what it holds: said too).
fn changes(a: &Bookmark, b: &Bookmark) -> Vec<String> {
    let mut out = Vec::new();
    if a.note != b.note {
        out.push("note".to_string());
    }
    if a.camera != b.camera {
        let name = |c: &Option<String>| c.clone().unwrap_or_else(|| "all cameras".into());
        out.push(format!("camera {} to {}{}", name(&a.camera), name(&b.camera), if a.keep && b.keep { " (the kept video moves with it)" } else { "" }));
    }
    if a.keep != b.keep {
        out.push(if b.keep { "kept" } else { "no longer kept" }.to_string());
    }
    out
}

/// " (made by otto)" when the caller changes another account's bookmark.
fn others(b: &Bookmark, user: &str) -> String {
    if b.user == user { String::new() } else { format!(" (made by {})", b.user) }
}

fn ip_of(parts: &axum::http::request::Parts) -> String {
    parts.extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string()).unwrap_or_default()
}

#[derive(serde::Deserialize)]
pub struct ListQ {
    from: Option<i64>,
    to: Option<i64>,
    /// A camera's name: its bookmarks and the all-cameras ones.
    camera: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
}

/// `GET /api/bookmarks?from=&to=&camera=&q=&limit=`: newest moment first.
pub async fn list(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    axum::Extension(access): axum::Extension<Access>,
    role: Option<axum::Extension<Role>>,
    token: Option<axum::Extension<TokenUsed>>,
    Query(q): Query<ListQ>,
) -> Response {
    let role = role_of(&role);
    // (A device token never writes bookmarks: none is its to change.)
    let by_token = token.is_some();
    if q.q.as_deref().is_some_and(|s| s.chars().count() > 200) {
        return err(StatusCode::BAD_REQUEST, "search text too long");
    }
    let camera = q.camera.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(crate::config::slug);
    let filter = crate::db::BookmarkFilter {
        from_ms: q.from,
        to_ms: q.to,
        camera,
        visible: access.0.clone(),
        q: q.q,
        limit: q.limit.unwrap_or(200).clamp(1, LIST_MAX),
    };
    let e = engine.clone();
    let rows = match tokio::task::spawn_blocking(move || e.db.bookmarks_list(&filter)).await {
        Ok(Ok(r)) => r,
        Ok(Err(x)) => {
            tracing::warn!("bookmarks: {x:#}");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "could not read the bookmarks");
        }
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "bookmarks error"),
    };
    let list: Vec<_> = rows.iter().map(|b| to_json(&engine, b, !by_token && may_edit(&role, &access, &caller.0, b))).collect();
    Json(serde_json::json!({
        "bookmarks": list,
        "can_add": !by_token && role.may(Perm::Events),
        "keep_window_ms": crate::db::KEEP_WINDOW_MS,
    }))
    .into_response()
}

/// The camera field of a body: absent (None), all cameras (Some(None)) or one (Some(Some(slug))).
fn camera_field(engine: &Engine, access: &Access, v: &serde_json::Value) -> Result<Option<Option<String>>, Response> {
    match v.get("camera") {
        None => Ok(None),
        Some(serde_json::Value::Null) => {
            if access.limited() {
                Err(err(StatusCode::FORBIDDEN, "an account limited to some cameras bookmarks one camera at a time"))
            } else {
                Ok(Some(None))
            }
        }
        Some(serde_json::Value::String(s)) if s.trim().is_empty() || s.trim() == "*" => {
            if access.limited() {
                Err(err(StatusCode::FORBIDDEN, "an account limited to some cameras bookmarks one camera at a time"))
            } else {
                Ok(Some(None))
            }
        }
        Some(serde_json::Value::String(s)) => match camera_slug(engine, access, s) {
            Some(slug) => Ok(Some(Some(slug))),
            None => Err(err(StatusCode::NOT_FOUND, "no such camera")),
        },
        Some(_) => Err(err(StatusCode::BAD_REQUEST, "camera must be a camera's name, or null for all cameras")),
    }
}

fn parse_body(body: &Bytes) -> Result<serde_json::Map<String, serde_json::Value>, Response> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(m)) => Ok(m),
        _ => Err(err(StatusCode::BAD_REQUEST, "send a JSON object")),
    }
}

/// `POST /api/bookmarks {"at": ms, "note": "...", "camera": "Porch" | null, "keep": false}`.
pub async fn create(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    axum::Extension(access): axum::Extension<Access>,
    role: Option<axum::Extension<Role>>,
    parts: axum::http::request::Parts,
    body: Bytes,
) -> Response {
    let role = role_of(&role);
    // (The sign-in layer has checked this already.)
    if !role.may(Perm::Events) {
        return err(StatusCode::FORBIDDEN, "this account may not add bookmarks");
    }
    let m = match parse_body(&body) {
        Ok(m) => serde_json::Value::Object(m),
        Err(r) => return r,
    };
    let note = match m.get("note").and_then(|v| v.as_str()).map(clean_note) {
        Some(Ok(n)) => n,
        Some(Err(why)) => return err(StatusCode::BAD_REQUEST, why),
        None => return err(StatusCode::BAD_REQUEST, "write a note"),
    };
    let now = chrono::Utc::now().timestamp_millis();
    let at = match m.get("at").and_then(|v| v.as_i64()) {
        Some(t) if (EARLIEST_MS..=now + FUTURE_SLACK_MS).contains(&t) => t,
        Some(_) => return err(StatusCode::BAD_REQUEST, "that moment is not in the past"),
        None => return err(StatusCode::BAD_REQUEST, "at (the moment, in ms) is needed"),
    };
    let keep = match m.get("keep") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => return err(StatusCode::BAD_REQUEST, "keep must be true or false"),
    };
    // (Left out: all cameras.)
    let camera = match camera_field(&engine, &access, &m) {
        Ok(Some(c)) => c,
        Ok(None) if access.limited() => return err(StatusCode::FORBIDDEN, "an account limited to some cameras bookmarks one camera at a time"),
        Ok(None) => None,
        Err(r) => return r,
    };
    let mut b = Bookmark { id: 0, camera, at_ms: at, note, user: caller.0.clone(), owner: Some(caller.0.clone()), created_ms: now, updated_ms: now, keep };
    if let Some(why) = keep_refused(&role, None, &b) {
        return err(StatusCode::FORBIDDEN, why);
    }
    let e = engine.clone();
    let ip = ip_of(&parts);
    let saved = tokio::task::spawn_blocking(move || -> anyhow::Result<Result<Bookmark, BookmarkCap>> {
        match e.db.bookmark_add_capped(&b, MAX_BOOKMARKS, MAX_KEPT)? {
            Ok(id) => b.id = id,
            Err(cap) => return Ok(Err(cap)),
        }
        let _ = e.db.insert_audit(&b.user, &ip, "bookmark added", &where_text(&b), 200);
        Ok(Ok(b))
    })
    .await;
    match saved {
        Ok(Ok(Ok(b))) => (StatusCode::CREATED, Json(to_json(&engine, &b, true))).into_response(),
        Ok(Ok(Err(BookmarkCap::TooMany))) => err(StatusCode::INSUFFICIENT_STORAGE, "this server has 20000 bookmarks: delete some first"),
        Ok(Ok(Err(BookmarkCap::KeptFull))) => err(StatusCode::INSUFFICIENT_STORAGE, KEPT_FULL),
        Ok(Err(x)) => {
            tracing::warn!("bookmark add: {x:#}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "could not save the bookmark")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "bookmarks error"),
    }
}

/// The bookmark `id` if this caller may see it (else an answer: not found)
/// and change it (else: refused).
fn editable(engine: &Engine, access: &Access, role: &Role, user: &str, id: i64) -> Result<Bookmark, Response> {
    let b = match engine.db.bookmark_get(id) {
        Ok(Some(b)) if may_see(access, &b) => b,
        Ok(_) => return Err(err(StatusCode::NOT_FOUND, "no such bookmark")),
        Err(x) => {
            tracing::warn!("bookmark {id}: {x:#}");
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "could not read the bookmark"));
        }
    };
    if !may_edit(role, access, user, &b) {
        return Err(err(StatusCode::FORBIDDEN, if role.is_admin() || b.owner.as_deref() == Some(user) { "an account limited to some cameras cannot change an all-cameras bookmark" } else { "only the one who made it, or an admin, can change it" }));
    }
    Ok(b)
}

/// `PUT /api/bookmarks/:id {"note"?: "...", "keep"?: bool, "camera"?: "Porch" | null}`:
/// what is left out stays.
pub async fn update(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    axum::Extension(access): axum::Extension<Access>,
    role: Option<axum::Extension<Role>>,
    parts: axum::http::request::Parts,
    AxPath(id): AxPath<i64>,
    body: Bytes,
) -> Response {
    let role = role_of(&role);
    let m = match parse_body(&body) {
        Ok(m) => serde_json::Value::Object(m),
        Err(r) => return r,
    };
    let mut b = match editable(&engine, &access, &role, &caller.0, id) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let before = b.clone();
    if let Some(n) = m.get("note") {
        match n.as_str().map(clean_note) {
            Some(Ok(n)) => b.note = n,
            Some(Err(why)) => return err(StatusCode::BAD_REQUEST, why),
            None => return err(StatusCode::BAD_REQUEST, "note must be text"),
        }
    }
    match m.get("keep") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Bool(k)) => b.keep = *k,
        Some(_) => return err(StatusCode::BAD_REQUEST, "keep must be true or false"),
    }
    // (The camera it has already, also one since removed: no change.)
    let same_camera = matches!((m.get("camera"), &b.camera), (Some(serde_json::Value::String(s)), Some(c)) if crate::config::slug(s.trim()) == *c);
    if !same_camera {
        match camera_field(&engine, &access, &m) {
            Ok(Some(c)) => b.camera = c,
            Ok(None) => {}
            Err(r) => return r,
        }
    }
    if let Some(why) = keep_refused(&role, Some(&before), &b) {
        return err(StatusCode::FORBIDDEN, why);
    }
    b.updated_ms = chrono::Utc::now().timestamp_millis();
    let e = engine.clone();
    let ip = ip_of(&parts);
    let user = caller.0.clone();
    let b2 = b.clone();
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<Result<bool, BookmarkCap>> {
        let ok = match e.db.bookmark_update_capped(&b2, MAX_KEPT)? {
            Ok(ok) => ok,
            Err(cap) => return Ok(Err(cap)),
        };
        let what = changes(&before, &b2);
        if ok && !what.is_empty() {
            let _ = e.db.insert_audit(&user, &ip, &format!("bookmark changed: {}{}", what.join(", "), others(&before, &user)), &where_text(&b2), 200);
        }
        Ok(Ok(ok))
    })
    .await;
    match r {
        Ok(Ok(Ok(true))) => Json(to_json(&engine, &b, true)).into_response(),
        Ok(Ok(Ok(false))) => err(StatusCode::NOT_FOUND, "no such bookmark"),
        Ok(Ok(Err(_))) => err(StatusCode::INSUFFICIENT_STORAGE, KEPT_FULL),
        Ok(Err(x)) => {
            tracing::warn!("bookmark {id}: {x:#}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "could not save the bookmark")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "bookmarks error"),
    }
}

/// `DELETE /api/bookmarks/:id` (its hold on the recordings goes with it).
pub async fn remove(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    axum::Extension(access): axum::Extension<Access>,
    role: Option<axum::Extension<Role>>,
    parts: axum::http::request::Parts,
    AxPath(id): AxPath<i64>,
) -> Response {
    let role = role_of(&role);
    let b = match editable(&engine, &access, &role, &caller.0, id) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let e = engine.clone();
    let ip = ip_of(&parts);
    let user = caller.0.clone();
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let ok = e.db.bookmark_delete(b.id)?;
        if ok {
            let _ = e.db.insert_audit(&user, &ip, &format!("bookmark deleted{}", others(&b, &user)), &where_text(&b), 200);
        }
        Ok(ok)
    })
    .await;
    match r {
        Ok(Ok(true)) => Json(serde_json::json!({ "id": id, "deleted": true })).into_response(),
        Ok(Ok(false)) => err(StatusCode::NOT_FOUND, "no such bookmark"),
        Ok(Err(x)) => {
            tracing::warn!("bookmark {id}: {x:#}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "could not delete the bookmark")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "bookmarks error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(camera: Option<&str>, keep: bool) -> Bookmark {
        Bookmark { id: 1, camera: camera.map(String::from), at_ms: 1, note: "n".into(), user: "otto".into(), owner: Some("otto".into()), created_ms: 1, updated_ms: 1, keep }
    }

    #[test]
    fn keeping_all_cameras_is_for_admins() {
        let (admin, op) = (Role::Admin, Role::Operator(Default::default()));
        assert!(keep_refused(&op, None, &bm(None, true)).is_some(), "an operator cannot keep all cameras");
        assert!(keep_refused(&op, None, &bm(Some("front"), true)).is_none(), "one camera is fine");
        assert!(keep_refused(&op, None, &bm(None, false)).is_none(), "a plain all-cameras note is fine");
        assert!(keep_refused(&admin, None, &bm(None, true)).is_none());
        // Moving a kept single-camera bookmark to all cameras, or switching keep on, is refused too.
        assert!(keep_refused(&op, Some(&bm(Some("front"), true)), &bm(None, true)).is_some());
        assert!(keep_refused(&op, Some(&bm(None, false)), &bm(None, true)).is_some());
        // An all-cameras one an admin kept stays editable (note change) by its owner.
        assert!(keep_refused(&op, Some(&bm(None, true)), &bm(None, true)).is_none());
    }

    #[test]
    fn notes_are_cleaned_and_limited() {
        assert_eq!(clean_note("  hi \r\nthere\u{7}  ").unwrap(), "hi \nthere");
        assert!(clean_note("   ").is_err());
        assert!(clean_note("\u{1}\u{2}").is_err());
        assert_eq!(clean_note(&"é".repeat(500)).unwrap().chars().count(), 500, "characters, not bytes");
        assert!(clean_note(&"a".repeat(501)).is_err());
        assert_eq!(clean_note("<b>x</b>").unwrap(), "<b>x</b>", "kept as typed: escaped where shown");
    }
}
