//! Browser notifications (Web Push): alerts arrive as the phone's or
//! computer's own notifications straight from AmberSight's page, with no
//! other app. Each browser that turns them on gives us a "subscription" (an
//! address at its push service: Google, Mozilla, Apple or Microsoft, plus
//! keys). We encrypt each message for that browser (RFC 8291) and sign the
//! request with this server's own key (RFC 8292, "VAPID"), so the push
//! service only delivers what we sent and cannot read it.
//!
//! Phone apps get the same through UnifiedPush (unifiedpush.org): a
//! "distributor" app on the phone (usually ntfy) gives the app a push
//! address on its server, and the app's connector library makes the keys
//! and decrypts. Those subscriptions are kind "app": they get a JSON
//! payload of their own (app_payload) and their addresses must be on a
//! push server an admin allowed ([push] app_hosts, see app_target).

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use p256::ecdsa::signature::Signer;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rand::RngCore;
use sha2::Sha256;
use tracing::{info, warn};

use crate::db::{Db, PushSub};
use crate::engine::Engine;

/// Each encrypted message is one record of at most this size.
const RECORD_SIZE: u32 = 4096;
/// Messages are kept well under the 4 KB the push services accept.
pub const MAX_PAYLOAD: usize = 3000;
/// How long a push service keeps trying a phone that is off (seconds).
pub const TTL_SECS: u32 = 24 * 3600;
/// A subscription made by a phone app (UnifiedPush); others are "browser".
pub const KIND_APP: &str = "app";
pub const KIND_BROWSER: &str = "browser";
/// Most of a push server's answer that is read (only to log why it refused).
const MAX_RESPONSE: u64 = 8 * 1024;

/// This server's signing key (P-256), made once and kept in the database.
pub struct Keys {
    secret: p256::SecretKey,
    /// The public key as browsers want it: 65 bytes, uncompressed.
    public: Vec<u8>,
}

impl Keys {
    pub fn from_secret(secret: p256::SecretKey) -> Keys {
        let public = secret.public_key().to_encoded_point(false).as_bytes().to_vec();
        Keys { secret, public }
    }
    /// The public key in base64url (what the page passes to the browser).
    pub fn public_b64(&self) -> String {
        B64.encode(&self.public)
    }
}

static ENGINE: OnceLock<Weak<Engine>> = OnceLock::new();
static KEYS: Mutex<Option<Arc<Keys>>> = Mutex::new(None);

/// Called once at start, so alerts can reach the subscriptions.
pub fn init(engine: &Arc<Engine>) {
    let _ = ENGINE.set(Arc::downgrade(engine));
}

fn engine() -> Option<Arc<Engine>> {
    ENGINE.get().and_then(|w| w.upgrade())
}

/// The server's key, made and stored on first use.
pub fn keys(db: &Db) -> Result<Arc<Keys>> {
    let mut k = KEYS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(k) = k.as_ref() {
        return Ok(k.clone());
    }
    let fresh = B64.encode(p256::SecretKey::random(&mut rand::rngs::OsRng).to_bytes());
    let stored = db.push_key_init(&fresh)?;
    let bytes = B64.decode(stored.trim()).context("stored push key is damaged")?;
    let secret = p256::SecretKey::from_slice(&bytes).map_err(|_| anyhow!("stored push key is damaged"))?;
    let keys = Arc::new(Keys::from_secret(secret));
    *k = Some(keys.clone());
    Ok(keys)
}

/// Base64 the way browsers write it (url-safe, padding optional; the
/// standard alphabet is accepted too).
pub fn b64d(s: &str) -> Result<Vec<u8>> {
    let t: String = s.trim().trim_end_matches('=').chars().map(|c| match c {
        '+' => '-',
        '/' => '_',
        c => c,
    }).collect();
    B64.decode(t).map_err(|_| anyhow!("not valid base64"))
}

/// Encrypt a message for one browser (RFC 8291, "aes128gcm"). `as_secret`
/// is a one-off key made for this message and `salt` 16 random bytes.
pub fn encrypt(plain: &[u8], ua_public: &[u8], auth_secret: &[u8], as_secret: &p256::SecretKey, salt: [u8; 16]) -> Result<Vec<u8>> {
    if plain.len() + 1 + 16 > RECORD_SIZE as usize {
        bail!("message too long");
    }
    let ua = p256::PublicKey::from_sec1_bytes(ua_public).map_err(|_| anyhow!("the browser's key is not valid"))?;
    let as_public = as_secret.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua.as_affine());
    // Mix the shared secret with the browser's secret and both public keys.
    let mut info = Vec::with_capacity(14 + 65 + 65);
    info.extend_from_slice(b"WebPush: info\0");
    info.extend_from_slice(ua_public);
    info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes())
        .expand(&info, &mut ikm)
        .map_err(|_| anyhow!("key derivation failed"))?;
    let prk = hkdf::Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|_| anyhow!("key derivation failed"))?;
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|_| anyhow!("key derivation failed"))?;
    // One record: the message, then 2 = "last record" (no padding).
    let mut record = plain.to_vec();
    record.push(2);
    let cipher = aes_gcm::Aes128Gcm::new_from_slice(&cek).map_err(|_| anyhow!("bad key"))?;
    let sealed = cipher.encrypt(aes_gcm::Nonce::from_slice(&nonce), record.as_slice()).map_err(|_| anyhow!("encryption failed"))?;
    // Header: salt, record size, our one-off public key.
    let mut out = Vec::with_capacity(16 + 4 + 1 + 65 + sealed.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.as_bytes().len() as u8);
    out.extend_from_slice(as_public.as_bytes());
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// The Authorization header that proves the message is from this server
/// (RFC 8292): a signed token naming the push service and a contact.
pub fn vapid_auth(keys: &Keys, endpoint: &str, subject: &str, now: i64) -> Result<String> {
    let url = url::Url::parse(endpoint).context("bad push address")?;
    let aud = url.origin().ascii_serialization();
    let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    // Valid for 12 hours (the services allow at most 24).
    let claims = B64.encode(serde_json::json!({ "aud": aud, "exp": now + 12 * 3600, "sub": subject }).to_string());
    let signing = format!("{header}.{claims}");
    let key = p256::ecdsa::SigningKey::from(&keys.secret);
    let sig: p256::ecdsa::Signature = key.sign(signing.as_bytes());
    Ok(format!("vapid t={signing}.{}, k={}", B64.encode(sig.to_bytes()), keys.public_b64()))
}

/// The browsers' push services (Chrome and most others: Google; Firefox:
/// Mozilla; Edge: Microsoft; Safari: Apple). Nothing else is ever sent to,
/// so a signed-in page cannot make this server post to other addresses
/// (another computer on the home network, say).
const PUSH_HOSTS: [&str; 5] = ["fcm.googleapis.com", "push.services.mozilla.com", "notify.windows.com", "push.apple.com", "android.googleapis.com"];

/// Only real push services: https on the normal port, one of PUSH_HOSTS
/// (or a name under it).
pub fn endpoint_ok(endpoint: &str) -> bool {
    let Ok(u) = url::Url::parse(endpoint) else { return false };
    endpoint.len() <= 2048
        && u.scheme() == "https"
        && u.port().is_none()
        && u.username().is_empty()
        && matches!(u.host(), Some(url::Host::Domain(d)) if {
            let d = d.to_ascii_lowercase();
            PUSH_HOSTS.iter().any(|h| d == *h || d.ends_with(&format!(".{h}")))
        })
}

// ---- push servers for phone apps (UnifiedPush) ----
//
// Why the care: the push address comes from the signed-in user's app, and
// this server POSTs to it. Without limits any account (a viewer included)
// could make AmberSight send requests to other machines on the home
// network: the router, a NAS, a camera's admin page, or the cloud metadata
// address (server-side request forgery). So:
// - only push servers an admin listed in [push] app_hosts (exact names, no
//   wildcards), or the browsers' own push services above;
// - https, except plain http for a listed server on the home network,
//   loopback or Tailscale (self-hosted ntfy without TLS on a LAN is
//   common, and an admin chose that host): only those addresses of the
//   name are used (its public ones, a global IPv6 say, are left out);
// - a home-network address only for a host an admin listed: a well-known
//   public server (the default ntfy.sh, the browsers' services) that
//   suddenly resolves to a private address is refused, because that is
//   what a DNS rebinding or hijack looks like;
// - link-local (169.254.x: cloud metadata), multicast and unspecified
//   addresses never;
// - the name is resolved at send time, checked, and the connection goes to
//   exactly the checked addresses (no second lookup that could answer
//   differently); no redirects; at most 8 KB of the answer is read.

/// Public push servers that are never on a home network, whatever is
/// listed: one resolving to a private address is refused.
const KNOWN_PUBLIC: [&str; 1] = ["ntfy.sh"];

/// An app_hosts entry as stored: "name", "name:port", "1.2.3.4:port",
/// "[v6]:port". A pasted address (https://ntfy.example.com/up) is cut to
/// its host and port. None: not a host name (wildcards included).
pub fn normalize_host(raw: &str) -> Option<String> {
    let mut t = raw.trim();
    for p in ["https://", "http://"] {
        if t.len() >= p.len() && t[..p.len()].eq_ignore_ascii_case(p) {
            t = &t[p.len()..];
        }
    }
    let t = t.split(['/', '?', '#']).next().unwrap_or("");
    if t.is_empty() || t.len() > 260 || t.contains(['@', '*', ' ', '\t', '%']) {
        return None;
    }
    let (host, port) = split_host_port(t)?;
    let h = url::Host::parse(host).ok()?;
    let name = match h {
        url::Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            let ok = !d.is_empty()
                && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
            if !ok {
                return None;
            }
            d
        }
        url::Host::Ipv4(a) => a.to_string(),
        url::Host::Ipv6(a) => format!("[{a}]"),
    };
    Some(match port {
        Some(p) => format!("{name}:{p}"),
        None => name,
    })
}

/// "host:port" / "[v6]:port" / "host" into its parts (port 1-65535).
fn split_host_port(t: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if t.starts_with('[') {
        let end = t.find(']')?;
        let rest = &t[end + 1..];
        (&t[..=end], if rest.is_empty() { None } else { Some(rest.strip_prefix(':')?) })
    } else {
        match t.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (t, None),
        }
    };
    let port = match port {
        Some(p) => Some(p.parse::<u16>().ok().filter(|p| *p > 0)?),
        None => None,
    };
    Some((host, port))
}

/// The host of a parsed address as app_hosts writes it (lower case, no
/// trailing dot, IPv6 in brackets).
fn url_host(u: &url::Url) -> Option<String> {
    Some(match u.host()? {
        url::Host::Domain(d) => d.trim_end_matches('.').to_ascii_lowercase(),
        url::Host::Ipv4(a) => a.to_string(),
        url::Host::Ipv6(a) => format!("[{a}]"),
    })
}

/// Whether the address's host and port are listed. An entry without a port
/// allows the scheme's normal port only (443 for https, 80 for http).
fn listed(u: &url::Url, hosts: &[String]) -> bool {
    let Some(h) = url_host(u) else { return false };
    let port = u.port_or_known_default();
    hosts.iter().filter_map(|e| normalize_host(e)).any(|e| {
        let Some((eh, ep)) = split_host_port(&e) else { return false };
        eh == h && match ep {
            None => u.port().is_none(),
            Some(p) => Some(p) == port,
        }
    })
}

/// What kind of network an address is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Net {
    Public,
    /// Home network, loopback or Tailscale / carrier-grade NAT (100.64/10).
    Private,
    /// Link-local (cloud metadata), multicast, unspecified, reserved: never.
    Never,
}

pub fn net_of(ip: IpAddr) -> Net {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            // 192.0.0.0/24 (protocol assignments), 198.18.0.0/15
            // (benchmarking), 192.88.99.0/24 (old 6to4 relays): never.
            let special = (o[0] == 192 && o[1] == 0 && o[2] == 0) || (o[0] == 198 && (o[1] & 0xfe) == 18) || (o[0] == 192 && o[1] == 88 && o[2] == 99);
            if a.is_unspecified() || a.is_link_local() || a.is_multicast() || a.is_broadcast() || o[0] == 0 || o[0] >= 240 || special {
                Net::Never
            } else if a.is_loopback() || a.is_private() || (o[0] == 100 && (o[1] & 0xc0) == 64) {
                Net::Private
            } else {
                Net::Public
            }
        }
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return net_of(IpAddr::V4(v4));
            }
            let s = a.segments();
            // NAT64 (64:ff9b::/96) carries an IPv4 address.
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|x| *x == 0) {
                return net_of(IpAddr::V4(std::net::Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8)));
            }
            let v4 = |hi: u16, lo: u16| IpAddr::V4(std::net::Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8));
            // IPv4-compatible (::a.b.c.d, the old form; :: and ::1 aside).
            if s[..6].iter().all(|x| *x == 0) && !a.is_unspecified() && !a.is_loopback() {
                return net_of(v4(s[6], s[7]));
            }
            // 6to4 (2002::/16) carries an IPv4 address in bits 16..48.
            if s[0] == 0x2002 {
                return net_of(v4(s[1], s[2]));
            }
            // Teredo (2001::/32) and local-use NAT64 (64:ff9b:1::/48): their
            // real destination cannot be told from the address.
            if (s[0] == 0x2001 && s[1] == 0) || (s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1) {
                return Net::Never;
            }
            if a.is_unspecified() || a.is_multicast() || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xffc0) == 0xfec0 {
                Net::Never
            } else if a.is_loopback() || (s[0] & 0xfe00) == 0xfc00 {
                Net::Private
            } else {
                Net::Public
            }
        }
    }
}

fn not_allowed(host: &str) -> String {
    format!("this push server ({host}) is not allowed on this AmberSight; an admin can add it under Settings, Notifications, Browser and phone push")
}

/// What an app's push address must be, before any lookup. Ok(true): one of
/// the browsers' (or a well-known) public push services.
pub fn app_endpoint_rules(endpoint: &str, hosts: &[String]) -> Result<bool, String> {
    if endpoint.len() > 2048 {
        return Err("the push address is too long".into());
    }
    let u = url::Url::parse(endpoint).map_err(|_| "the push address is not a valid URL".to_string())?;
    if !matches!(u.scheme(), "https" | "http") {
        return Err("the push address must start with https://".into());
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("the push address must not hold a user name or password".into());
    }
    let host = url_host(&u).ok_or_else(|| "the push address has no host".to_string())?;
    if endpoint_ok(endpoint) {
        return Ok(true);
    }
    if !listed(&u, hosts) {
        return Err(not_allowed(&host_port(&u)));
    }
    let public = KNOWN_PUBLIC.contains(&host.as_str());
    if u.scheme() == "http" {
        if public {
            return Err(format!("this push server ({host}) needs an https:// address"));
        }
        if let Some(ip) = ip_of(&u) {
            if net_of(ip) == Net::Public {
                return Err(http_only_home(&host));
            }
        }
    }
    if let Some(ip) = ip_of(&u) {
        if net_of(ip) == Net::Never {
            return Err(format!("{host} is not an address a push server can have"));
        }
    }
    Ok(public)
}

fn http_only_home(host: &str) -> String {
    format!("plain http:// is only allowed for a push server on the home network or Tailscale, and {host} is on the internet; use its https:// address")
}

fn host_port(u: &url::Url) -> String {
    let h = url_host(u).unwrap_or_default();
    match u.port() {
        Some(p) => format!("{h}:{p}"),
        None => h,
    }
}

fn ip_of(u: &url::Url) -> Option<IpAddr> {
    match u.host()? {
        url::Host::Ipv4(a) => Some(IpAddr::V4(a)),
        url::Host::Ipv6(a) => Some(IpAddr::V6(a)),
        url::Host::Domain(_) => None,
    }
}

/// Which of a push server's looked-up addresses may be used (see the rules
/// above). `public`: a well-known public server. Plain http keeps only the
/// home-network, loopback and Tailscale addresses (a home DNS name often
/// also has a global IPv6 address), and is refused when none are left.
pub fn vet_addresses(host: &str, http: bool, public: bool, addrs: &[IpAddr]) -> Result<Vec<IpAddr>, String> {
    if addrs.is_empty() {
        return Err(format!("could not look up {host}"));
    }
    let nets: Vec<Net> = addrs.iter().map(|a| net_of(*a)).collect();
    if nets.contains(&Net::Never) {
        return Err(format!("{host} points to an address a push server cannot have (link-local, multicast or reserved); refused"));
    }
    if public && nets.contains(&Net::Private) {
        return Err(format!("{host} is a public push server but points to a home-network address; refused (the name may have been tampered with)"));
    }
    if http {
        let home: Vec<IpAddr> = addrs.iter().copied().filter(|a| net_of(*a) == Net::Private).collect();
        if home.is_empty() {
            return Err(http_only_home(host));
        }
        return Ok(home);
    }
    Ok(addrs.to_vec())
}

/// Where to deliver an app's message: the rules, a lookup, the addresses
/// checked. The connection must go to these addresses only. Blocking.
pub fn app_target(endpoint: &str, hosts: &[String]) -> Result<Vec<SocketAddr>, String> {
    let public = app_endpoint_rules(endpoint, hosts)?;
    let u = url::Url::parse(endpoint).map_err(|_| "the push address is not a valid URL".to_string())?;
    let host = url_host(&u).unwrap_or_default();
    let port = u.port_or_known_default().unwrap_or(443);
    let found: Vec<SocketAddr> = match ip_of(&u) {
        Some(ip) => vec![SocketAddr::new(ip, port)],
        None => (host.as_str(), port).to_socket_addrs().map(|a| a.take(16).collect()).unwrap_or_default(),
    };
    let ips: Vec<IpAddr> = found.iter().map(|a| a.ip()).collect();
    let ok = vet_addresses(&host, u.scheme() == "http", public, &ips)?;
    Ok(found.into_iter().filter(|a| ok.contains(&a.ip())).collect())
}

/// A resolver that answers with the addresses app_target checked (so the
/// connection cannot go anywhere else).
#[derive(Debug)]
struct Pinned(Vec<SocketAddr>);

impl ureq::unversioned::resolver::Resolver for Pinned {
    fn resolve(
        &self,
        _uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let mut out = self.empty();
        for a in self.0.iter().take(16) {
            out.push(*a);
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}

/// An agent that connects only to `addrs` and never through a proxy.
fn pinned_agent(addrs: Vec<SocketAddr>) -> ureq::Agent {
    let cfg = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .build();
    ureq::Agent::with_parts(cfg, ureq::unversioned::transport::DefaultConnector::default(), Pinned(addrs))
}

/// A short name for the device from its browser's User-Agent:
/// "Chrome on Android", "Safari on iPhone", "Edge on Windows".
pub fn device_name(ua: &str) -> String {
    let os = if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("CrOS") {
        "Chromebook"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
        "Mac"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        ""
    };
    let browser = if ua.contains("Edg/") || ua.contains("EdgA/") || ua.contains("EdgiOS/") {
        "Edge"
    } else if ua.contains("OPR/") {
        "Opera"
    } else if ua.contains("SamsungBrowser/") {
        "Samsung Internet"
    } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
        "Firefox"
    } else if ua.contains("CriOS/") || ua.contains("Chrome/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else if matches!(os, "iPhone" | "iPad") {
        // Added to the Home Screen: no browser name in the User-Agent.
        "Home Screen app"
    } else {
        "Browser"
    };
    if os.is_empty() { browser.to_string() } else { format!("{browser} on {os}") }
}

/// Who a message is for.
pub enum Audience {
    /// System problems: admin accounts only.
    Admins,
    /// An alert from a camera: everyone allowed to see that camera.
    Camera(String),
}

/// Whether `user`'s devices get a message for `aud`. Without accounts
/// (open on the home network) every device does; a deleted account's
/// devices never do.
pub fn wants(cfg: &crate::config::Config, user: &str, aud: &Audience) -> bool {
    let acc = crate::auth::accounts(cfg);
    if acc.is_empty() {
        return true;
    }
    let Some((_, _, admin)) = acc.iter().find(|(u, _, _)| u == user) else { return false };
    match aud {
        Audience::Admins => *admin,
        Audience::Camera(name) => *admin || crate::auth::camera_limits(cfg, user).is_none_or(|s| s.contains(&crate::config::slug(name))),
    }
}

/// The message as an admin's device gets it.
#[cfg(test)]
pub fn payload(m: &Message) -> Vec<u8> {
    payload_for(m, true)
}

/// The app's message as an admin's device gets it.
#[cfg(test)]
pub fn app_payload(m: &Message) -> Vec<u8> {
    app_payload_for(m, true)
}

/// Whether `user`'s devices are offered "Pause 1 h": the route is admin only.
/// Without accounts everyone is.
fn may_pause(cfg: &crate::config::Config, user: &str) -> bool {
    let acc = crate::auth::accounts(cfg);
    acc.is_empty() || acc.iter().any(|(u, _, admin)| u == user && *admin)
}

/// What a message is about (apps get it as "type").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MsgType {
    /// A camera alert (an event).
    Event,
    /// A note about one camera (it stopped sending, came back...).
    Camera,
    /// A system problem (camera offline, disk nearly full, the system test).
    Problem,
    /// Other system news (a problem cleared, new cameras found).
    #[default]
    Notice,
    /// The test button.
    Test,
}

impl MsgType {
    pub fn as_str(self) -> &'static str {
        match self {
            MsgType::Event => "event",
            MsgType::Camera => "camera",
            MsgType::Problem => "problem",
            MsgType::Notice => "notice",
            MsgType::Test => "test",
        }
    }
}

/// What a notification shows.
#[derive(Clone, Debug, Default)]
pub struct Message {
    pub title: String,
    pub body: String,
    /// Opened when it is tapped (a path on this server).
    pub url: String,
    /// A newer notification with the same tag replaces the older one.
    pub tag: String,
    /// The event picture (a path on this server; the browser fetches it).
    pub image: Option<String>,
    pub urgent: bool,
    /// For apps (browsers get title, body and the rest above).
    pub kind: MsgType,
    pub camera: Option<String>,
    /// The camera's number (event notifications; offers "Pause 1 h").
    pub camera_id: Option<usize>,
    pub event_id: Option<String>,
    /// What was seen ("person", "car").
    pub labels: Vec<String>,
    /// When it happened (ms since 1970); None: now.
    pub at_ms: Option<i64>,
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

/// The buttons an event notification offers (the app or browser calls
/// `POST /api/cameras/<camera_id>/pause {minutes}`, admin only: `may_pause` says
/// whether this device's account may).
fn pause_actions(m: &Message, may_pause: bool) -> serde_json::Value {
    if may_pause && m.kind == MsgType::Event && m.camera_id.is_some() {
        serde_json::json!([{ "action": "pause", "title": "Pause 1 h", "minutes": 60 }])
    } else {
        serde_json::json!([])
    }
}

/// The message as the page's service worker reads it, always under
/// MAX_PAYLOAD bytes.
fn payload_for(m: &Message, may_pause: bool) -> Vec<u8> {
    let make = |body_len: usize, image: bool| {
        serde_json::json!({
            "title": cut(&m.title, 120),
            "body": cut(&m.body, body_len),
            "url": cut(&m.url, 600),
            "tag": cut(&m.tag, 120),
            "image": if image { m.image.as_deref().map(|i| cut(i, 400)) } else { None },
            "urgent": m.urgent,
            "ts": chrono::Utc::now().timestamp_millis(),
            "camera": m.camera.as_deref().map(|c| cut(c, 120)),
            "camera_id": m.camera_id,
            "actions": pause_actions(m, may_pause),
        })
        .to_string()
        .into_bytes()
    };
    for (len, image) in [(600, true), (300, false), (100, false)] {
        let p = make(len, image);
        if p.len() <= MAX_PAYLOAD {
            return p;
        }
    }
    serde_json::json!({ "title": cut(&m.title, 60), "body": "", "url": "/", "tag": "ambersight" }).to_string().into_bytes()
}

/// The message as a phone app gets it (UnifiedPush; documented in
/// docs/api-guide.md, "Push notifications for apps"), always under
/// MAX_PAYLOAD bytes: the body, labels and picture are cut first.
fn app_payload_for(m: &Message, may_pause: bool) -> Vec<u8> {
    let now = chrono::Utc::now().timestamp_millis();
    let make = |body_len: usize, labels: usize, image: bool| {
        serde_json::json!({
            "v": 1,
            "type": m.kind.as_str(),
            "title": cut(&m.title, 120),
            "body": cut(&m.body, body_len),
            "camera": m.camera.as_deref().map(|c| cut(c, 120)),
            "event_id": m.event_id.as_deref().map(|c| cut(c, 120)),
            "labels": m.labels.iter().take(labels).map(|l| cut(l, 40)).collect::<Vec<_>>(),
            "at_ms": m.at_ms.unwrap_or(now),
            "sent_ms": now,
            "thumb": if image { m.image.as_deref().map(|i| cut(i, 400)) } else { None },
            "url": cut(&m.url, 600),
            "tag": cut(&m.tag, 120),
            "urgent": m.urgent,
            "camera_id": m.camera_id,
            "actions": pause_actions(m, may_pause),
        })
        .to_string()
        .into_bytes()
    };
    for (len, labels, image) in [(600, 10, true), (300, 5, true), (100, 0, false)] {
        let p = make(len, labels, image);
        if p.len() <= MAX_PAYLOAD {
            return p;
        }
    }
    serde_json::json!({
        "v": 1, "type": m.kind.as_str(), "title": cut(&m.title, 60), "body": "", "camera": null, "event_id": null,
        "labels": [], "at_ms": m.at_ms.unwrap_or(now), "sent_ms": now, "thumb": null, "url": "/", "tag": "ambersight", "urgent": m.urgent,
    })
    .to_string()
    .into_bytes()
}

/// The Urgency header (RFC 8030): alerts and problems wake the phone,
/// other news can wait for its next wake-up.
fn urgency(kind: &str, m: &Message) -> &'static str {
    if kind == KIND_APP {
        if m.urgent || matches!(m.kind, MsgType::Event | MsgType::Problem | MsgType::Camera | MsgType::Test) { "high" } else { "normal" }
    } else if m.urgent {
        "high"
    } else {
        "normal"
    }
}

/// How a round of sending went.
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub sent: usize,
    pub failed: usize,
    /// Browsers that turned notifications off (the push service said gone).
    pub removed: usize,
}

/// The contact the push services see in the signed token: this server's
/// https address when one is set, else a placeholder address.
fn subject(alerts: &crate::config::AlertConfig) -> String {
    alerts
        .link_base()
        .filter(|u| u.starts_with("https://"))
        .unwrap_or_else(|| "mailto:nvr@example.com".into())
}

/// Whether any browser has notifications on (the "push" channel is set up).
pub fn any_subscribed() -> bool {
    engine().is_some_and(|e| e.db.push_list(None).is_ok_and(|l| !l.is_empty()))
}

/// Send to every device that should get it. Blocking (run it on a
/// background thread). None when no device is signed up for it.
pub fn send(aud: &Audience, m: &Message) -> Option<Report> {
    let engine = engine()?;
    let subs = engine.db.push_list(None).ok()?;
    let subs: Vec<PushSub> = {
        let cfg = engine.config.lock().unwrap();
        subs.into_iter().filter(|s| wants(&cfg, &s.user, aud)).collect()
    };
    if subs.is_empty() {
        return None;
    }
    Some(send_to(&engine, &subs, m))
}

/// Send to these devices. Blocking.
pub fn send_to(engine: &Engine, subs: &[PushSub], m: &Message) -> Report {
    let mut rep = Report::default();
    let keys = match keys(&engine.db) {
        Ok(k) => k,
        Err(e) => {
            warn!("push: {e:#}");
            rep.failed = subs.len();
            return rep;
        }
    };
    let (subject, app_hosts, may) = {
        let c = engine.config.lock().unwrap();
        let may: Vec<bool> = subs.iter().map(|s| may_pause(&c, &s.user)).collect();
        (subject(&c.alerts), c.push.app_hosts.clone(), may)
    };
    let admin_bodies = Bodies { browser: payload_for(m, true), app: app_payload_for(m, true) };
    let viewer_bodies = Bodies { browser: payload_for(m, false), app: app_payload_for(m, false) };
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .new_agent();
    // All devices at once: one slow or unreachable push service must not
    // hold up the others (each try may take up to 15 s).
    let results: Vec<(&PushSub, Result<u16>)> = std::thread::scope(|sc| {
        let jobs: Vec<_> = subs
            .iter()
            .zip(may)
            .map(|(s, may)| {
                let bodies = if may { &admin_bodies } else { &viewer_bodies };
                let (agent, keys, subject, app_hosts) = (&agent, &keys, &subject, &app_hosts);
                (s, sc.spawn(move || deliver(agent, keys, s, bodies, m, subject, app_hosts)))
            })
            .collect();
        jobs.into_iter().map(|(s, j)| (s, j.join().unwrap_or_else(|_| Err(anyhow::anyhow!("send failed"))))).collect()
    });
    for (s, r) in results {
        match r {
            Ok(st) if (200..300).contains(&st) => {
                rep.sent += 1;
                let _ = engine.db.push_delivered(s.id, chrono::Utc::now().timestamp_millis());
            }
            // A push server an admin listed (self-hosted) answering 404:
            // more likely a proxy or address change than a phone that is
            // gone, so the device is kept (only 410 removes it there).
            Ok(404) if !gone(s, 404, &app_hosts) => {
                warn!("push: {} ({}): its push server answered 404 (not found); kept (only 410 removes a device on a self-hosted push server; check the server's address and proxy)", s.device, s.user);
                rep.failed += 1;
            }
            // Gone: the browser turned notifications off or was reset (or
            // the app unregistered at its push server).
            Ok(404) | Ok(410) => {
                info!("push: {} ({}) no longer takes notifications; removed", s.device, s.user);
                let _ = engine.db.push_delete(None, Some(s.id), None);
                rep.removed += 1;
            }
            Ok(st) => {
                warn!("push: {} ({}) refused the message (HTTP {st})", s.device, s.user);
                rep.failed += 1;
            }
            Err(e) => {
                warn!("push: {} ({}): {e:#}", s.device, s.user);
                rep.failed += 1;
            }
        }
    }
    rep
}

/// Whether a push server's answer means the device is gone (and is
/// removed): 404 or 410, except that a push server an admin listed
/// (self-hosted, not a browser's or a well-known public one) is believed
/// only on 410; a 404 from it is more likely a misconfigured proxy or a
/// changed base URL, which must not wipe every device.
fn gone(s: &PushSub, status: u16, app_hosts: &[String]) -> bool {
    match status {
        410 => true,
        404 => !(s.kind == KIND_APP && matches!(app_endpoint_rules(&s.endpoint, app_hosts), Ok(false))),
        _ => false,
    }
}

/// The message encoded once for each kind of device.
struct Bodies {
    browser: Vec<u8>,
    app: Vec<u8>,
}

fn deliver(agent: &ureq::Agent, keys: &Keys, s: &PushSub, bodies: &Bodies, m: &Message, subject: &str, app_hosts: &[String]) -> Result<u16> {
    let app = s.kind == KIND_APP;
    // Apps: checked and looked up now, and connected to the checked
    // addresses only (see app_target).
    let pinned = if app {
        Some(pinned_agent(app_target(&s.endpoint, app_hosts).map_err(|e| anyhow!(e))?))
    } else {
        if !endpoint_ok(&s.endpoint) {
            bail!("not a push service address");
        }
        None
    };
    let agent = pinned.as_ref().unwrap_or(agent);
    let body = if app { &bodies.app } else { &bodies.browser };
    let ua_public = b64d(&s.p256dh)?;
    let auth = b64d(&s.auth)?;
    let one_off = p256::SecretKey::random(&mut rand::rngs::OsRng);
    let mut salt = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let data = encrypt(body, &ua_public, &auth, &one_off, salt)?;
    let auth_header = vapid_auth(keys, &s.endpoint, subject, chrono::Utc::now().timestamp())?;
    let resp = agent
        .post(&s.endpoint)
        .header("TTL", TTL_SECS.to_string())
        .header("Urgency", urgency(&s.kind, m))
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("Authorization", auth_header)
        .send(&data[..])
        .map_err(|e| anyhow!("could not reach the push service: {e}"))?;
    let status = resp.status().as_u16();
    if app && !(200..300).contains(&status) {
        // Why it refused, from the first few KB of the answer.
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::Read::take(resp.into_body().into_reader(), MAX_RESPONSE), &mut buf);
        let why: String = String::from_utf8_lossy(&buf).chars().filter(|c| !c.is_control()).take(200).collect();
        if !why.trim().is_empty() {
            warn!("push: {} ({}): the push server said: {}", s.device, s.user, why.trim());
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b64: &str) -> p256::SecretKey {
        p256::SecretKey::from_slice(&b64d(b64).unwrap()).unwrap()
    }

    /// RFC 8291 Appendix A.
    #[test]
    fn a_disabled_accounts_devices_get_nothing() {
        let cfg: crate::config::Config = toml::from_str("[[server.users]]\nusername = \"boss\"\npassword = \"x\"\n[[server.users]]\nusername = \"off\"\npassword = \"x\"\ndisabled = true\n").unwrap();
        assert!(wants(&cfg, "boss", &Audience::Admins));
        assert!(!wants(&cfg, "off", &Audience::Admins));
        assert!(!wants(&cfg, "off", &Audience::Camera("Door".into())));
    }

    #[test]
    fn encryption_matches_the_rfc_example() {
        let as_secret = key("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw");
        assert_eq!(
            B64.encode(as_secret.public_key().to_encoded_point(false).as_bytes()),
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
        let ua_public = b64d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4").unwrap();
        let auth = b64d("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
        let salt: [u8; 16] = b64d("DGv6ra1nlYgDCS1FRnbzlw").unwrap().try_into().unwrap();
        let out = encrypt(b"When I grow up, I want to be a watermelon", &ua_public, &auth, &as_secret, salt).unwrap();
        assert_eq!(
            B64.encode(out),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    /// What a browser does with a message: decrypt with its own key.
    fn decrypt(msg: &[u8], ua_secret: &p256::SecretKey, auth: &[u8]) -> Vec<u8> {
        let salt = &msg[..16];
        let idlen = msg[20] as usize;
        let as_public = &msg[21..21 + idlen];
        let ua_public = ua_secret.public_key().to_encoded_point(false);
        let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), p256::PublicKey::from_sec1_bytes(as_public).unwrap().as_affine());
        let mut info = b"WebPush: info\0".to_vec();
        info.extend_from_slice(ua_public.as_bytes());
        info.extend_from_slice(as_public);
        let mut ikm = [0u8; 32];
        hkdf::Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes()).expand(&info, &mut ikm).unwrap();
        let prk = hkdf::Hkdf::<Sha256>::new(Some(salt), &ikm);
        let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
        prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).unwrap();
        prk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();
        let mut p = aes_gcm::Aes128Gcm::new_from_slice(&cek).unwrap().decrypt(aes_gcm::Nonce::from_slice(&nonce), &msg[21 + idlen..]).unwrap();
        assert_eq!(p.pop(), Some(2));
        p
    }

    #[test]
    fn a_browser_can_read_what_we_send() {
        let ua = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let ua_pub = ua.public_key().to_encoded_point(false).as_bytes().to_vec();
        let auth = [7u8; 16];
        let m = Message { title: "Person at Front Door".into(), body: "Mon 9:01 PM · person".into(), url: "/?alert=1".into(), tag: "cam-front-door".into(), image: Some("/api/events/x/thumb".into()), urgent: true, ..Default::default() };
        let p = payload(&m);
        let enc = encrypt(&p, &ua_pub, &auth, &p256::SecretKey::random(&mut rand::rngs::OsRng), [1; 16]).unwrap();
        let back: serde_json::Value = serde_json::from_slice(&decrypt(&enc, &ua, &auth)).unwrap();
        assert_eq!(back["title"], "Person at Front Door");
        assert_eq!(back["image"], "/api/events/x/thumb");
        assert_eq!(back["urgent"], true);
    }

    #[test]
    fn vapid_token_is_signed_by_our_key() {
        use p256::ecdsa::signature::Verifier;
        let keys = Keys::from_secret(key("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"));
        let h = vapid_auth(&keys, "https://fcm.googleapis.com/fcm/send/abc:def", "mailto:nvr@example.com", 1_000_000).unwrap();
        let rest = h.strip_prefix("vapid t=").unwrap();
        let (jwt, k) = rest.split_once(", k=").unwrap();
        assert_eq!(k, keys.public_b64());
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let head: serde_json::Value = serde_json::from_slice(&B64.decode(parts[0]).unwrap()).unwrap();
        assert_eq!((head["alg"].as_str(), head["typ"].as_str()), (Some("ES256"), Some("JWT")));
        let claims: serde_json::Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 1_000_000 + 12 * 3600);
        assert_eq!(claims["sub"], "mailto:nvr@example.com");
        let sig = p256::ecdsa::Signature::from_slice(&B64.decode(parts[2]).unwrap()).unwrap();
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&B64.decode(k).unwrap()).unwrap();
        assert!(vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).is_ok());
        assert!(vk.verify(b"something else", &sig).is_err());
    }

    #[test]
    fn keys_are_made_once_and_kept() {
        let db = Db::open_in_memory().unwrap();
        let a = db.push_key_init("first").unwrap();
        let b = db.push_key_init("second").unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("first", "first"));
    }

    #[test]
    fn subscriptions_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let s = PushSub { endpoint: "https://fcm.googleapis.com/fcm/send/a".into(), user: "ann".into(), p256dh: "pk".into(), auth: "au".into(), device: "Chrome on Android".into(), ..Default::default() };
        db.push_save(&s, 5).unwrap();
        db.push_save(&PushSub { endpoint: "https://updates.push.services.mozilla.com/wpush/v2/b".into(), user: "bob".into(), ..s.clone() }, 6).unwrap();
        // The same browser again (now signed in as bob): updated, not added.
        db.push_save(&PushSub { user: "bob".into(), device: "Chrome on Windows".into(), ..s.clone() }, 7).unwrap();
        let all = db.push_list(None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!((all[0].user.as_str(), all[0].device.as_str(), all[0].created_ms), ("bob", "Chrome on Windows", 5));
        assert!(db.push_list(Some("ann")).unwrap().is_empty());
        db.push_delivered(all[0].id, 99).unwrap();
        assert_eq!(db.push_list(Some("bob")).unwrap()[0].last_ok_ms, Some(99));
        // Another user cannot remove bob's device.
        assert!(!db.push_delete(None, Some(all[0].id), Some("ann")).unwrap());
        assert!(db.push_delete(Some(&s.endpoint), None, Some("bob")).unwrap());
        assert_eq!(db.push_delete_user("bob").unwrap(), 1);
        assert!(db.push_list(None).unwrap().is_empty());
    }

    #[test]
    fn who_gets_what() {
        let cfg: crate::config::Config = toml::from_str(
            r#"
[[server.users]]
username = "boss"
password = "x"
[[server.users]]
username = "kid"
password = "x"
admin = false
cameras = ["Front Door"]
[[server.users]]
username = "guest"
password = "x"
admin = false
"#,
        )
        .unwrap();
        let cam = |n: &str| Audience::Camera(n.into());
        assert!(wants(&cfg, "boss", &Audience::Admins));
        assert!(!wants(&cfg, "kid", &Audience::Admins));
        assert!(wants(&cfg, "kid", &cam("Front Door")));
        assert!(!wants(&cfg, "kid", &cam("Side Yard")));
        assert!(wants(&cfg, "guest", &cam("Side Yard")));
        assert!(!wants(&cfg, "gone", &cam("Front Door")));
        // No accounts: open on the home network, every device.
        let open: crate::config::Config = toml::from_str("").unwrap();
        assert!(wants(&open, "(open)", &Audience::Admins));
    }

    #[test]
    fn only_push_service_addresses() {
        assert!(endpoint_ok("https://fcm.googleapis.com/fcm/send/abc"));
        assert!(endpoint_ok("https://web.push.apple.com/QGx"));
        assert!(!endpoint_ok("http://fcm.googleapis.com/fcm/send/abc"));
        assert!(!endpoint_ok("https://192.168.1.10/x"));
        assert!(!endpoint_ok("https://[::1]/x"));
        assert!(!endpoint_ok("https://router.local/x"));
        assert!(!endpoint_ok("https://localhost/x"));
        assert!(!endpoint_ok("https://fcm.googleapis.com:8443/x"));
        assert!(endpoint_ok("https://updates.push.services.mozilla.com/wpush/v2/abc"));
        assert!(endpoint_ok("https://wns2-bl2p.notify.windows.com/w/?token=abc"));
        assert!(!endpoint_ok("https://evil.example/x"));
        assert!(!endpoint_ok("https://192-168-1-10.nip.io/x"));
        assert!(!endpoint_ok("https://fcm.googleapis.com.evil.example/x"));
        assert!(!endpoint_ok("https://notfcm.googleapis.com.example/x"));
    }

    #[test]
    fn device_names() {
        assert_eq!(device_name("Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Mobile Safari/537.36"), "Chrome on Android");
        assert_eq!(device_name("Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1"), "Safari on iPhone");
        assert_eq!(device_name("Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148"), "Home Screen app on iPhone");
        assert_eq!(device_name("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36 Edg/130.0"), "Edge on Windows");
        assert_eq!(device_name("Mozilla/5.0 (X11; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0"), "Firefox on Linux");
        assert_eq!(device_name(""), "Browser");
    }

    #[test]
    fn payload_stays_small() {
        let m = Message { title: "t".repeat(500), body: "é".repeat(5000), url: "/".into(), tag: "x".into(), image: Some("/api/events/1/thumb".into()), urgent: false, ..Default::default() };
        let p = payload(&m);
        assert!(p.len() <= MAX_PAYLOAD, "{}", p.len());
        let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
        assert!(v["title"].as_str().unwrap().chars().count() <= 120);
    }

    #[test]
    fn app_hosts_are_normalized() {
        assert_eq!(normalize_host("NTFY.sh").as_deref(), Some("ntfy.sh"));
        assert_eq!(normalize_host(" https://ntfy.example.com/up?x=1 ").as_deref(), Some("ntfy.example.com"));
        assert_eq!(normalize_host("http://192.168.1.20:8080/").as_deref(), Some("192.168.1.20:8080"));
        assert_eq!(normalize_host("ntfy.home.lan.:80").as_deref(), Some("ntfy.home.lan:80"));
        assert_eq!(normalize_host("[FD7A:115C::1]:2586").as_deref(), Some("[fd7a:115c::1]:2586"));
        for bad in ["", "*.ntfy.sh", "user@ntfy.sh", "ntfy.sh:0", "ntfy.sh:99999", "ntfy .sh", "-x.example", "a..b", "[::1", "ntfy.sh:x"] {
            assert_eq!(normalize_host(bad), None, "{bad}");
        }
        let mut c = crate::config::PushConfig { app_hosts: vec!["ntfy.sh".into(), "NTFY.SH".into(), " ".into(), "https://n.example.com:8443/x".into()] };
        c.sanitize().unwrap();
        assert_eq!(c.app_hosts, ["ntfy.sh", "n.example.com:8443"]);
        assert!(crate::config::PushConfig { app_hosts: vec!["*.example.com".into()] }.sanitize().is_err());
        // At load: bad entries left out, more than 50 cut to the first 50
        // (so the list is normalized and saving works).
        let mut c = crate::config::PushConfig { app_hosts: (0..60).map(|i| format!("H{i}.example.com")).chain(["*.x".to_string(), "h0.example.com".into()]).collect() };
        let said = c.sanitize_at_load();
        assert_eq!(c.app_hosts.len(), 50);
        assert_eq!((c.app_hosts[0].as_str(), c.app_hosts[49].as_str()), ("h0.example.com", "h49.example.com"));
        assert!(c.clone().sanitize().is_ok());
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said[0].contains("\"*.x\" is not a host name"), "{said:?}");
        assert!(said[1].contains("60 push servers") && said[1].contains("first 50 are kept"), "{said:?}");
        let dir = std::env::temp_dir().join(format!("ambersight-push-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let list: Vec<String> = (0..55).map(|i| format!("\"n{i}.example.com\"")).collect();
        std::fs::write(&path, format!("[server]\n[push]\napp_hosts = [{}]\n", list.join(", "))).unwrap();
        let loaded = crate::config::Config::load(path.to_str().unwrap()).unwrap();
        assert_eq!(loaded.push.app_hosts.len(), 50);
        let _ = std::fs::remove_dir_all(&dir);
        // Default: the public ntfy server only.
        let d: crate::config::Config = toml::from_str("").unwrap();
        assert_eq!(d.push.app_hosts, ["ntfy.sh"]);
    }

    #[test]
    fn app_endpoint_rules_follow_the_list() {
        let hosts: Vec<String> = ["ntfy.sh", "ntfy.home.lan:8080", "192.168.1.20:2586", "ntfy.example.com", "8.8.8.8:80"].iter().map(|s| s.to_string()).collect();
        let ok = |e: &str| app_endpoint_rules(e, &hosts);
        assert_eq!(ok("https://ntfy.sh/upAbC?up=1"), Ok(true));
        assert_eq!(ok("https://NTFY.SH./upAbC?up=1"), Ok(true));
        // The browsers' push services always work (Sunup uses Mozilla's).
        assert_eq!(ok("https://updates.push.services.mozilla.com/wpush/v2/abc"), Ok(true));
        assert_eq!(ok("https://ntfy.example.com/upX"), Ok(false));
        assert_eq!(ok("http://ntfy.home.lan:8080/upX?up=1"), Ok(false));
        assert_eq!(ok("http://192.168.1.20:2586/upX"), Ok(false));
        // Not listed (or another port).
        let e = ok("https://ntfy.evil.example/upX").unwrap_err();
        assert!(e.contains("(ntfy.evil.example) is not allowed") && e.contains("an admin can add it"), "{e}");
        assert!(ok("https://ntfy.home.lan/upX").unwrap_err().contains("(ntfy.home.lan)"));
        assert!(ok("http://ntfy.home.lan:8081/upX").unwrap_err().contains("(ntfy.home.lan:8081)"));
        assert!(ok("https://ntfy.sh:8443/x").is_err());
        assert!(ok("https://sub.ntfy.sh/x").is_err());
        assert!(ok("http://192.168.1.21:2586/x").is_err());
        assert!(ok("https://10.0.0.1/x").is_err());
        assert!(ok("http://169.254.169.254/latest/meta-data").is_err());
        // https required for the public server, and for public addresses.
        assert!(ok("http://ntfy.sh/upX").unwrap_err().contains("needs an https://"));
        assert!(ok("http://8.8.8.8/x").unwrap_err().contains("home network"));
        assert!(ok("ftp://ntfy.sh/x").is_err());
        assert!(ok("https://u:p@ntfy.sh/x").is_err());
        assert!(ok(&format!("https://ntfy.sh/{}", "a".repeat(2100))).is_err());
        assert!(ok("not a url").is_err());
        // An empty list: only the browsers' services.
        assert!(app_endpoint_rules("https://ntfy.sh/x", &[]).is_err());
    }

    #[test]
    fn address_kinds() {
        let n = |s: &str| net_of(s.parse().unwrap());
        assert_eq!(n("1.1.1.1"), Net::Public);
        assert_eq!(n("192.168.1.5"), Net::Private);
        assert_eq!(n("10.1.2.3"), Net::Private);
        assert_eq!(n("172.20.0.1"), Net::Private);
        assert_eq!(n("172.32.0.1"), Net::Public);
        assert_eq!(n("127.0.0.1"), Net::Private);
        assert_eq!(n("100.101.102.103"), Net::Private);
        assert_eq!(n("100.128.0.1"), Net::Public);
        assert_eq!(n("169.254.169.254"), Net::Never);
        assert_eq!(n("0.0.0.0"), Net::Never);
        assert_eq!(n("224.0.0.1"), Net::Never);
        assert_eq!(n("255.255.255.255"), Net::Never);
        assert_eq!(n("::1"), Net::Private);
        assert_eq!(n("fd7a:115c:a1e0::1"), Net::Private);
        assert_eq!(n("fe80::1"), Net::Never);
        assert_eq!(n("::"), Net::Never);
        assert_eq!(n("::ffff:192.168.1.1"), Net::Private);
        assert_eq!(n("::ffff:169.254.169.254"), Net::Never);
        assert_eq!(n("64:ff9b::a9fe:a9fe"), Net::Never);
        assert_eq!(n("2606:4700::1111"), Net::Public);
        // IPv4-compatible (::a.b.c.d): the IPv4 address inside.
        assert_eq!(n("::a9fe:a9fe"), Net::Never);
        assert_eq!(n("::c0a8:101"), Net::Private);
        assert_eq!(n("::808:808"), Net::Public);
        assert_eq!(n("::7f00:1"), Net::Private);
        // 6to4: the IPv4 address in bits 16..48.
        assert_eq!(n("2002:a9fe:a9fe::1"), Net::Never);
        assert_eq!(n("2002:c0a8:101::1"), Net::Private);
        assert_eq!(n("2002:808:808::1"), Net::Public);
        // Teredo and local-use NAT64: never.
        assert_eq!(n("2001:0:4136:e378:8000:63bf:3fff:fdd2"), Net::Never);
        assert_eq!(n("64:ff9b:1::a9fe:a9fe"), Net::Never);
        assert_eq!(n("64:ff9b:1:2::808:808"), Net::Never);
        // (2001:db8 and other 2001: prefixes are not Teredo.)
        assert_eq!(n("2001:4860:4860::8888"), Net::Public);
        // IPv4 special-purpose ranges.
        assert_eq!(n("192.0.0.1"), Net::Never);
        assert_eq!(n("192.0.0.170"), Net::Never);
        assert_eq!(n("192.0.1.1"), Net::Public);
        assert_eq!(n("198.18.0.1"), Net::Never);
        assert_eq!(n("198.19.255.254"), Net::Never);
        assert_eq!(n("198.20.0.1"), Net::Public);
        assert_eq!(n("192.88.99.1"), Net::Never);
        assert_eq!(n("192.88.98.1"), Net::Public);
    }

    #[test]
    fn looked_up_addresses_are_checked() {
        let ip = |v: &[&str]| v.iter().map(|s| s.parse().unwrap()).collect::<Vec<IpAddr>>();
        // A public server: public addresses only (a private one = rebinding).
        assert_eq!(vet_addresses("ntfy.sh", false, true, &ip(&["159.203.148.75"])), Ok(ip(&["159.203.148.75"])));
        let e = vet_addresses("ntfy.sh", false, true, &ip(&["159.203.148.75", "192.168.1.1"])).unwrap_err();
        assert!(e.contains("public push server"), "{e}");
        assert!(vet_addresses("ntfy.sh", false, true, &ip(&["127.0.0.1"])).is_err());
        // A host an admin listed may be at home, over https or http.
        assert!(vet_addresses("ntfy.home.lan", true, false, &ip(&["192.168.1.20"])).is_ok());
        assert!(vet_addresses("ntfy.tail1.ts.net", true, false, &ip(&["100.64.1.2", "fd7a:115c:a1e0::2"])).is_ok());
        assert!(vet_addresses("ntfy.example.com", false, false, &ip(&["192.168.1.20", "203.0.113.9"])).is_ok());
        // Plain http: only the home addresses are used; none, refused.
        assert_eq!(vet_addresses("ntfy.example.com", true, false, &ip(&["192.168.1.20", "203.0.113.9"])), Ok(ip(&["192.168.1.20"])));
        assert_eq!(vet_addresses("ntfy.home.lan", true, false, &ip(&["2606:4700::1111", "192.168.1.20", "fd00::5"])), Ok(ip(&["192.168.1.20", "fd00::5"])));
        let e = vet_addresses("ntfy.example.com", true, false, &ip(&["203.0.113.9", "2606:4700::1111"])).unwrap_err();
        assert_eq!(e, http_only_home("ntfy.example.com"));
        // https keeps them all.
        assert_eq!(vet_addresses("ntfy.example.com", false, false, &ip(&["192.168.1.20", "203.0.113.9"])).unwrap().len(), 2);
        // Never link-local / metadata, never nothing.
        assert!(vet_addresses("x", false, false, &ip(&["169.254.169.254"])).is_err());
        assert!(vet_addresses("x", false, false, &ip(&["fe80::1"])).is_err());
        assert!(vet_addresses("x", false, false, &[]).unwrap_err().contains("could not look up"));
        // A listed name that resolves (localhost: loopback) is pinned to it.
        let hosts = vec!["localhost:2586".to_string()];
        let a = app_target("http://localhost:2586/upX", &hosts).unwrap();
        assert!(!a.is_empty() && a.iter().all(|x| x.ip().is_loopback() && x.port() == 2586), "{a:?}");
        assert!(app_target("http://localhost:2587/upX", &hosts).is_err());
    }

    #[test]
    fn which_answers_remove_a_device() {
        let hosts = vec!["ntfy.sh".to_string(), "ntfy.home.lan".to_string()];
        let sub = |kind: &str, ep: &str| PushSub { kind: kind.into(), endpoint: ep.into(), ..Default::default() };
        let browser = sub(KIND_BROWSER, "https://fcm.googleapis.com/fcm/send/a");
        let public = sub(KIND_APP, "https://ntfy.sh/upX");
        let fcm_app = sub(KIND_APP, "https://fcm.googleapis.com/fcm/send/b");
        let own = sub(KIND_APP, "https://ntfy.home.lan/upX");
        for s in [&browser, &public, &fcm_app] {
            assert!(gone(s, 404, &hosts) && gone(s, 410, &hosts), "{}", s.endpoint);
        }
        // A self-hosted (admin-listed) server: only 410.
        assert!(!gone(&own, 404, &hosts));
        assert!(gone(&own, 410, &hosts));
        for st in [200, 201, 400, 413, 429, 500] {
            assert!(!gone(&own, st, &hosts) && !gone(&browser, st, &hosts));
        }
    }

    fn app_msg() -> Message {
        Message {
            title: "Person at Front Door".into(),
            body: "Mon 9:01 PM · person".into(),
            url: "/?alert=3&cam=Front%20Door&at=1700000000000".into(),
            tag: "cam-front-door".into(),
            image: Some("/api/events/ev1/thumb".into()),
            urgent: false,
            kind: MsgType::Event,
            camera: Some("Front Door".into()),
            camera_id: Some(3),
            event_id: Some("ev1".into()),
            labels: vec!["person".into(), "dog".into()],
            at_ms: Some(1_700_000_000_000),
        }
    }

    #[test]
    fn app_payload_has_every_field() {
        let v: serde_json::Value = serde_json::from_slice(&app_payload(&app_msg())).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "event");
        assert_eq!(v["title"], "Person at Front Door");
        assert_eq!(v["camera"], "Front Door");
        assert_eq!(v["event_id"], "ev1");
        assert_eq!(v["labels"], serde_json::json!(["person", "dog"]));
        assert_eq!(v["at_ms"], 1_700_000_000_000i64);
        assert!(v["sent_ms"].as_i64().unwrap() > 1_700_000_000_000);
        assert_eq!(v["thumb"], "/api/events/ev1/thumb");
        assert_eq!(v["url"], "/?alert=3&cam=Front%20Door&at=1700000000000");
        assert_eq!(v["tag"], "cam-front-door");
        assert_eq!(v["urgent"], false);
        // A system problem: no camera or event.
        let p = Message { title: "Disk nearly full".into(), kind: MsgType::Problem, urgent: true, url: "/#settings/system/status".into(), ..Default::default() };
        let v: serde_json::Value = serde_json::from_slice(&app_payload(&p)).unwrap();
        assert_eq!((v["type"].as_str(), v["camera"].is_null(), v["thumb"].is_null()), (Some("problem"), true, true));
        assert_eq!(v["labels"], serde_json::json!([]));
        // The browser payload is unchanged.
        let b: serde_json::Value = serde_json::from_slice(&payload(&app_msg())).unwrap();
        assert!(b.get("type").is_none() && b.get("v").is_none() && b["image"] == "/api/events/ev1/thumb");
    }

    #[test]
    fn event_notifications_offer_a_pause() {
        for bytes in [app_payload(&app_msg()), payload(&app_msg())] {
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v["camera_id"], 3);
            assert_eq!(v["actions"], serde_json::json!([{ "action": "pause", "title": "Pause 1 h", "minutes": 60 }]));
        }
        let n = Message { kind: MsgType::Notice, ..Default::default() };
        let v: serde_json::Value = serde_json::from_slice(&payload(&n)).unwrap();
        assert_eq!(v["actions"], serde_json::json!([]));
    }

    #[test]
    fn only_admins_are_offered_the_pause_button() {
        for bytes in [app_payload_for(&app_msg(), false), payload_for(&app_msg(), false)] {
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v["actions"], serde_json::json!([]));
            assert_eq!(v["camera_id"], 3);
        }
        let open: crate::config::Config = toml::from_str("").unwrap();
        assert!(may_pause(&open, "anyone"), "no accounts: everyone is an admin");
        let cfg: crate::config::Config = toml::from_str("[[server.users]]\nusername = \"boss\"\npassword = \"x\"\nadmin = true\n[[server.users]]\nusername = \"ann\"\npassword = \"x\"\nadmin = false\n").unwrap();
        assert!(may_pause(&cfg, "boss"));
        assert!(!may_pause(&cfg, "ann"));
        assert!(!may_pause(&cfg, "gone"));
    }

    #[test]
    fn app_payload_stays_small() {
        let mut m = app_msg();
        m.title = "t".repeat(500);
        m.body = "é".repeat(5000);
        m.labels = (0..50).map(|i| format!("{i}{}", "x".repeat(100))).collect();
        m.url = format!("/{}", "u".repeat(5000));
        let p = app_payload(&m);
        assert!(p.len() <= MAX_PAYLOAD, "{}", p.len());
        let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
        assert_eq!(v["type"], "event");
        assert!(v["title"].as_str().unwrap().chars().count() <= 120);
        assert!(v["labels"].as_array().unwrap().len() <= 10);
        // Everything long at once still fits (the last resort).
        m.camera = Some("c".repeat(5000));
        m.event_id = Some("e".repeat(5000));
        m.tag = "g".repeat(5000);
        let p = app_payload(&m);
        assert!(p.len() <= MAX_PAYLOAD, "{}", p.len());
        assert!(serde_json::from_slice::<serde_json::Value>(&p).is_ok());
    }

    #[test]
    fn apps_get_high_urgency_for_alerts() {
        let m = app_msg();
        assert_eq!(urgency(KIND_APP, &m), "high");
        assert_eq!(urgency(KIND_BROWSER, &m), "normal");
        let n = Message { kind: MsgType::Notice, ..Default::default() };
        assert_eq!(urgency(KIND_APP, &n), "normal");
        assert_eq!(urgency(KIND_APP, &Message { kind: MsgType::Test, ..Default::default() }), "high");
    }

    /// A push server on this computer: answers `status` to one request and
    /// hands back its headers (lower case) and body.
    fn mock_push_server(status: u16) -> (u16, std::thread::JoinHandle<(Vec<(String, String)>, Vec<u8>, String)>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut r = BufReader::new(c.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let mut headers = Vec::new();
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                let (k, v) = h.split_once(':').unwrap();
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
            let len: usize = headers.iter().find(|(k, _)| k == "content-length").map(|(_, v)| v.parse().unwrap()).unwrap_or(0);
            let mut body = vec![0u8; len];
            r.read_exact(&mut body).unwrap();
            let answer = "x".repeat(20_000);
            let _ = write!(c, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
            (headers, body, line)
        });
        (port, h)
    }

    #[test]
    fn an_app_gets_an_encrypted_signed_message() {
        let (port, server) = mock_push_server(201);
        let ua = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let ua_pub = ua.public_key().to_encoded_point(false).as_bytes().to_vec();
        let auth = [9u8; 16];
        let sub = PushSub {
            id: 1,
            endpoint: format!("http://127.0.0.1:{port}/upAbC123?up=1"),
            user: "ann".into(),
            p256dh: B64.encode(&ua_pub),
            auth: B64.encode(auth),
            device: "Pixel 8".into(),
            kind: KIND_APP.into(),
            ..Default::default()
        };
        let keys = Keys::from_secret(key("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"));
        let m = app_msg();
        let bodies = Bodies { browser: payload(&m), app: app_payload(&m) };
        let agent = ureq::Agent::new_with_defaults();
        let hosts = vec![format!("127.0.0.1:{port}")];
        // Not listed: nothing is sent.
        assert!(deliver(&agent, &keys, &sub, &bodies, &m, "mailto:nvr@example.com", &[]).unwrap_err().to_string().contains("not allowed"));
        let st = deliver(&agent, &keys, &sub, &bodies, &m, "mailto:nvr@example.com", &hosts).unwrap();
        assert_eq!(st, 201);
        let (headers, body, line) = server.join().unwrap();
        assert_eq!(line.trim_end(), "POST /upAbC123?up=1 HTTP/1.1");
        let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
        assert_eq!(h("content-encoding"), "aes128gcm");
        assert_eq!(h("urgency"), "high");
        assert_eq!(h("ttl"), TTL_SECS.to_string());
        let vapid = h("authorization");
        assert!(vapid.starts_with("vapid t=") && vapid.ends_with(&format!(", k={}", keys.public_b64())), "{vapid}");
        let jwt = vapid.strip_prefix("vapid t=").unwrap().split(", k=").next().unwrap().to_string();
        let claims: serde_json::Value = serde_json::from_slice(&B64.decode(jwt.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], format!("http://127.0.0.1:{port}"));
        // The app's library decrypts it with its keys.
        let back: serde_json::Value = serde_json::from_slice(&decrypt(&body, &ua, &auth)).unwrap();
        assert_eq!((back["v"].as_i64(), back["type"].as_str(), back["event_id"].as_str()), (Some(1), Some("event"), Some("ev1")));
        assert_eq!(back["thumb"], "/api/events/ev1/thumb");
    }

    #[test]
    fn an_app_that_is_gone_is_told_by_its_status() {
        // (The caller removes it on 404 / 410; a long answer is cut short.)
        let (port, server) = mock_push_server(410);
        let ua = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let sub = PushSub {
            endpoint: format!("http://localhost:{port}/upGone"),
            p256dh: B64.encode(ua.public_key().to_encoded_point(false).as_bytes()),
            auth: B64.encode([1u8; 16]),
            kind: KIND_APP.into(),
            ..Default::default()
        };
        let keys = Keys::from_secret(key("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"));
        let m = Message { title: "x".into(), kind: MsgType::Notice, ..Default::default() };
        let bodies = Bodies { browser: payload(&m), app: app_payload(&m) };
        let st = deliver(&ureq::Agent::new_with_defaults(), &keys, &sub, &bodies, &m, "mailto:a@b.c", &[format!("localhost:{port}")]).unwrap();
        assert_eq!(st, 410);
        let (headers, _, _) = server.join().unwrap();
        assert!(headers.iter().any(|(k, v)| k == "urgency" && v == "normal"));
    }
}
