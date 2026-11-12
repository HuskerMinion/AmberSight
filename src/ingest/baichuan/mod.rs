//! Baichuan client for battery Reolink Argus cameras that do not listen on TCP
//! and cannot join a hub. A clean-room implementation of Reolink's proprietary
//! protocol, written from public protocol facts and our own cameras' traffic
//! (no third-party code): the UDP "BcUdp" transport, the BC message framing and
//! AES/XOR crypto, and the BcMedia video framing.
//!
//! The pieces:
//!   * `udp`   — the BcUdp reliable-UDP transport (discovery/wake, data, acks)
//!   * `bc`    — BC message framing, XOR cipher, AES-128-CFB, MD5 login hashes
//!   * `media` — BcMedia framing (delimits H.264/H.265 frames, strips headers)
//!   * `avcc`  — the bridge from the cameras' Annex-B output to the AVCC
//!     (length-prefixed NAL) frames + configuration record the ingest pipeline
//!     expects (see `recorder::MediaItem::Video` / `VideoParams`).
//!
//!   * `client` — the per-camera session thread: on-demand sign-in, live
//!     preview, SD-card downloads; `registry` maps a camera uid to its client
//!   * `pushx` — the listener for the cameras' motion push

#[allow(dead_code)]
pub mod bc;
#[allow(dead_code)]
pub mod udp;
#[allow(dead_code)]
pub mod media;
pub mod avcc;
pub mod client;
pub mod pushx;
pub mod registry;

/// Scheme for a battery Argus camera: `baichuan://<host>?uid=<UID>`.
pub const SCHEME: &str = "baichuan://";

/// Whether this URL names a battery Argus (Baichuan) camera.
pub fn is_baichuan_url(url: &str) -> bool {
    url.starts_with(SCHEME)
}

/// Parse `baichuan://<host>?uid=<UID>` into (host, uid). The uid is optional in
/// the URL (it may instead come from the camera's `uid` config field).
pub fn parse_url(url: &str) -> Option<(String, Option<String>)> {
    let rest = url.strip_prefix(SCHEME)?;
    let (host, query) = match rest.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (rest, None),
    };
    let host = host.trim_end_matches('/');
    if host.is_empty() {
        return None;
    }
    let uid = query.and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("uid="))
            .filter(|u| !u.is_empty())
            .map(|u| u.to_string())
    });
    Some((host.to_string(), uid))
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn parses_host_and_uid() {
        assert!(is_baichuan_url("baichuan://10.0.0.110?uid=ABC123"));
        assert!(!is_baichuan_url("rtsp://10.0.0.110"));
        assert_eq!(parse_url("baichuan://10.0.0.110?uid=ABC123"), Some(("10.0.0.110".into(), Some("ABC123".into()))));
        assert_eq!(parse_url("baichuan://10.0.0.110"), Some(("10.0.0.110".into(), None)));
        assert_eq!(parse_url("baichuan://10.0.0.110/"), Some(("10.0.0.110".into(), None)));
        assert_eq!(parse_url("rtsp://x"), None);
        assert_eq!(parse_url("baichuan://"), None);
    }
}
