//! Web interface files, embedded in the binary (release builds) or read from
//! `static/` on disk (debug builds, handy while editing the UI).
//!
//! Layout: `static/index.html` (served at `/`, `/ambs.htm`, `/index.html`),
//! `static/css` and `static/js` (served under `/static/`).

use axum::{
    body::Body,
    extract::Path as AxPath,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "static/"]
struct Assets;

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Text worth compressing (pictures and fonts are compressed already).
fn compressible(path: &str) -> bool {
    matches!(path.rsplit('.').next().unwrap_or(""), "html" | "htm" | "js" | "mjs" | "css" | "json" | "svg" | "webmanifest" | "txt")
}

/// This build's web interface: a short hash over every embedded file. Every
/// answer carries it (`x-ambersight-ui`), and the page holds the one it was
/// loaded with, so a page left open across an update can tell that it is
/// old and offer a reload (until then it would miss new menus and buttons).
pub fn ui_build() -> &'static str {
    use std::sync::OnceLock;
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let mut paths: Vec<String> = Assets::iter().map(|p| p.to_string()).collect();
        paths.sort();
        let mut h = Sha256::new();
        for p in &paths {
            h.update(p.as_bytes());
            if let Some(f) = Assets::get(p) {
                h.update(f.metadata.sha256_hash());
            }
        }
        hex(&h.finalize()[..6])
    })
}

/// The page with a `modulepreload` link for every script module, so the
/// browser asks for all of them at once instead of discovering them import
/// by import (one round trip per level, slow on a tablet over Wi-Fi).
fn with_preloads(html: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(html);
    let mut mods: Vec<String> = Assets::iter().filter(|p| p.starts_with("js/") && p.ends_with(".js") && *p != "js/prepaint.js").map(|p| p.to_string()).collect();
    mods.sort();
    let links: String = mods.iter().map(|m| format!("<link rel=\"modulepreload\" href=\"/static/{m}\">\n")).collect();
    let meta = format!("<meta name=\"ambersight-ui\" content=\"{}\">\n", ui_build());
    match text.find("</head>") {
        Some(i) => format!("{}{}{}{}", &text[..i], meta, links, &text[i..]).into_bytes(),
        None => html.to_vec(),
    }
}

/// Compressed copies, made once per file version (key: path and ETag).
fn gzipped(key: String, data: &[u8]) -> std::sync::Arc<Vec<u8>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return v.clone();
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(data.len() / 3), flate2::Compression::new(6));
    let _ = std::io::Write::write_all(&mut enc, data);
    let out = Arc::new(enc.finish().unwrap_or_default());
    let mut c = cache.lock().unwrap();
    if c.len() > 512 {
        c.clear();
    }
    c.insert(key, out.clone());
    out
}

/// Serve one embedded file with its Content-Type, an ETag and `no-cache`
/// (browsers revalidate on every load, so a new binary is picked up at once
/// while unchanged files cost only a 304). Text goes gzip-compressed to
/// browsers that accept it (about a third of the size).
fn serve(path: &str, req_headers: &HeaderMap) -> Response {
    let Some(file) = Assets::get(path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let gz = compressible(path)
        && req_headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| {
            v.split(',').any(|t| {
                let mut parts = t.split(';').map(str::trim);
                // ("gzip;q=0" means: not gzip.)
                parts.next() == Some("gzip") && !parts.any(|p| p.strip_prefix("q=").and_then(|q| q.trim().parse::<f32>().ok()) == Some(0.0))
            })
        });
    // (The page itself also names the build and lists every script, so its
    // tag changes with any file.)
    let mut base = hex(&file.metadata.sha256_hash()[..12]);
    if path == "index.html" {
        base = format!("{base}-{}", ui_build());
    }
    let etag = if gz { format!("\"{base}-gz\"") } else { format!("\"{base}\"") };
    let cache = HeaderValue::from_static("no-cache");
    let unchanged = req_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim().trim_start_matches("W/") == etag));
    let vary = HeaderValue::from_static("accept-encoding");
    if unchanged {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, cache)
            .header(header::VARY, vary)
            .body(Body::empty())
            .unwrap();
    }
    let data: Vec<u8> = if path == "index.html" { with_preloads(&file.data) } else { file.data.into_owned() };
    let r = Response::builder()
        .header(header::CONTENT_TYPE, content_type(path))
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, cache)
        .header(header::VARY, vary);
    if gz {
        let body = gzipped(format!("{path}:{base}"), &data);
        return r.header(header::CONTENT_ENCODING, "gzip").body(Body::from(body.as_ref().clone())).unwrap();
    }
    r.body(Body::from(data)).unwrap()
}

/// `/`, `/ambs.htm`, `/index.html`
pub async fn index(headers: HeaderMap) -> Response {
    serve("index.html", &headers)
}

/// `/favicon.ico` - browsers ask for it at the root.
pub async fn favicon(headers: HeaderMap) -> Response {
    serve("favicon.ico", &headers)
}

/// `/sw.js` - the service worker that shows notifications (served from the
/// top so it covers every page of the app).
pub async fn service_worker(headers: HeaderMap) -> Response {
    let mut r = serve("sw.js", &headers);
    r.headers_mut().insert("service-worker-allowed", HeaderValue::from_static("/"));
    r
}

/// `/static/*path` - CSS and JS modules (and anything else under static/).
pub async fn asset(AxPath(path): AxPath<String>, headers: HeaderMap) -> Response {
    // Never serve parent paths.
    if path.contains("..") || path.starts_with('/') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    serve(&path, &headers)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_names_its_build_and_its_tag_follows_it() {
        let id = ui_build();
        assert_eq!(id.len(), 12);
        let page = String::from_utf8(with_preloads(&Assets::get("index.html").unwrap().data)).unwrap();
        assert!(page.contains(&format!("<meta name=\"ambersight-ui\" content=\"{id}\">")));
        let r = serve("index.html", &HeaderMap::new());
        let etag = r.headers().get(header::ETAG).unwrap().to_str().unwrap().to_string();
        assert!(etag.ends_with(&format!("-{id}\"")), "{etag}");
    }
}
