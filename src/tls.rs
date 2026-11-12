//! HTTPS with a private certificate authority.
//!
//! With `tls = true` and no configured certificate, AmberSight creates its own
//! CA once (`<data_dir>/tls/ca.pem` + `ca-key.pem`) and signs the server
//! certificate (`cert.pem` + `key.pem`) with it. Devices install `ca.pem` once
//! and then trust every certificate the server issues - nothing is ever
//! published to a public certificate-transparency log.
//!
//! The server certificate is reissued (same CA) when it nears expiry or when
//! the set of names/addresses it must cover changes; the TLS acceptor is
//! swapped in place without a restart.
//!
//! The CA is limited (X.509 name constraints) to home-network names and
//! addresses: private IPv4/IPv6 ranges, `localhost`, `.local`, `.lan`,
//! `.home.arpa`, `.internal`, this computer's name, its Tailscale network
//! and any name set in `tls_names` when the CA was made. Were its key ever
//! stolen, it could not be used to impersonate other web sites to the
//! devices that trust it. (`ca.json` holds the limits.)
//!
//! A CA made before limits existed is replaced once, on start: the new
//! limited CA is also signed by the old one (`cross.pem`, sent along with
//! the server certificate), so devices and linked servers that trust the old
//! CA keep working without a warning, and the old CA's key is then erased,
//! so it can never sign anything again.

use anyhow::{Context, Result};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, KeyPair,
    KeyUsagePurpose, NameConstraints,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

const CA_NAME_PREFIX: &str = "AmberSight Local CA";
const SERVER_VALID_DAYS: i64 = 397;
const RENEW_BEFORE_DAYS: i64 = 30;
const DAY: i64 = 86_400;

/// What `/api/tls` reports.
#[derive(Clone, Debug, Default)]
pub struct Info {
    /// "private-ca" | "files" | "self-signed"
    pub mode: &'static str,
    pub names: Vec<String>,
    pub expires: i64,
    pub ca_fingerprint: Option<String>,
    /// Something the admin should know about the mode, e.g. the own
    /// certificate could not be loaded at start, so the private CA is used.
    pub note: Option<String>,
}

/// The live TLS state: current rustls config (hot-swappable) and CA download data.
pub struct Runtime {
    config: RwLock<Arc<rustls::ServerConfig>>,
    info: RwLock<Info>,
    ca_der: Option<Vec<u8>>,
    ca_pem: Option<String>,
    /// Own certificate files were set but could not be loaded at start:
    /// the private CA is used instead (until a restart with usable files).
    fell_back: bool,
}

static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();

/// The running TLS state, if `tls = true`.
pub fn current() -> Option<Arc<Runtime>> {
    RUNTIME.get().cloned()
}

/// What the web listener was started with (`[server] tls`, `tls_cert`,
/// `tls_key`, `tls_names`); a change to the HTTPS mode or the certificate
/// files only applies after a restart.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Started {
    pub tls: bool,
    pub cert: Option<std::path::PathBuf>,
    pub key: Option<std::path::PathBuf>,
    pub names: Vec<String>,
}

impl Started {
    pub fn of(server: &crate::config::ServerConfig) -> Started {
        Started { tls: server.tls, cert: server.tls_cert.clone(), key: server.tls_key.clone(), names: server.tls_names.clone() }
    }
}

static STARTED: OnceLock<Started> = OnceLock::new();

/// Remember what the listener starts with (called once, at start).
pub fn record_started(server: &crate::config::ServerConfig) {
    let _ = STARTED.set(Started::of(server));
}

/// What the listener started with (None: not started by `run`, e.g. tests).
pub fn started() -> Option<&'static Started> {
    STARTED.get()
}

/// Whether the saved settings `now` need a restart to apply, against what
/// the listener `started` with: HTTPS turned on or off, or (with HTTPS on)
/// other certificate files. Extra names apply without one (the certificate
/// is reissued in place).
pub fn restart_needed(started: &Started, now: &crate::config::ServerConfig) -> bool {
    if started.tls != now.tls {
        return true;
    }
    now.tls && (started.cert != now.tls_cert || started.key != now.tls_key)
}

/// Whether `name` is a host name a certificate (and the strict checks in
/// rustls/webpki) can carry: ASCII letters, digits, hyphens and underscores
/// in labels of 1-63 characters that neither start nor end with a hyphen,
/// at most 253 characters in all, and a last label that is not only digits
/// (webpki refuses those, they look like addresses). Underscores are
/// accepted by webpki, so Windows computer names that have them work.
pub fn valid_dns_name(name: &str) -> bool {
    // (A trailing dot is not part of a name a certificate or its limits can carry.)
    let n = name;
    if n.is_empty() || n.len() > 253 || !n.is_ascii() {
        return false;
    }
    let label_ok = |l: &str| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    n.split('.').all(label_ok) && !n.rsplit('.').next().unwrap_or("").chars().all(|c| c.is_ascii_digit())
}

/// The most extra certificate names `tls_names` may hold.
pub const MAX_NAMES: usize = 50;

/// Check the extra certificate names: host names or IP addresses only.
/// Returns them trimmed, lower-cased and without duplicates.
pub fn check_names(names: &[String]) -> std::result::Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for raw in names {
        let n = raw.trim().trim_end_matches('.').to_ascii_lowercase();
        if n.is_empty() {
            continue;
        }
        let shown: String = raw.trim().chars().take(60).collect();
        if n.len() > 253 {
            return Err(format!("the name '{shown}...' is too long (at most 253 characters)"));
        }
        let ip = n.trim_matches(|c| c == '[' || c == ']');
        let n = if let Ok(a) = ip.parse::<IpAddr>() {
            a.to_string()
        } else {
            if n.contains("://") || n.contains('/') || n.contains(char::is_whitespace) || !valid_dns_name(&n) {
                return Err(format!("'{shown}' is not a host name or IP address (write just the name, like nvr.lan, without http:// or a path)"));
            }
            n
        };
        if !out.contains(&n) {
            out.push(n);
        }
    }
    if out.len() > MAX_NAMES {
        return Err(format!("too many extra names: at most {MAX_NAMES}"));
    }
    Ok(out)
}

/// Check own certificate files: both given or neither; both readable,
/// a PEM certificate chain and the private key that belongs to it.
pub fn check_files(cert: Option<&Path>, key: Option<&Path>) -> std::result::Result<(), String> {
    let (c, k) = match (cert, key) {
        (None, None) => return Ok(()),
        (Some(c), Some(k)) => (c, k),
        _ => return Err("give both the certificate file and its key file, or neither".into()),
    };
    let cb = read_pem_file(c, "certificate file")?;
    let kb = read_pem_file(k, "key file")?;
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cb)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| format!("{} is not a readable PEM certificate file", c.display()))?;
    if chain.is_empty() {
        return Err(format!("{} holds no certificate (expected PEM text starting with -----BEGIN CERTIFICATE-----)", c.display()));
    }
    if PrivateKeyDer::from_pem_slice(&kb).is_err() {
        return Err(format!("{} holds no private key (expected PEM text starting with -----BEGIN PRIVATE KEY-----)", k.display()));
    }
    server_config(&cb, &kb).map_err(|_| format!("the key in {} does not belong to the certificate in {}", k.display(), c.display()))?;
    Ok(())
}

/// The most a certificate or key file may hold (a full chain is a few KB).
pub const MAX_PEM_FILE: u64 = 1024 * 1024;

/// Read a PEM file given in the settings: a regular file (not a folder, a
/// device such as /dev/zero, or a pipe) of at most `MAX_PEM_FILE` bytes.
/// `what` names it in the message ("certificate file").
pub fn read_pem_file(path: &Path, what: &str) -> std::result::Result<Vec<u8>, String> {
    use std::io::Read;
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|e| format!("the {what} {shown} could not be read: {e}"))?;
    if !meta.is_file() {
        return Err(format!("the {what} {shown} is not a regular file"));
    }
    let too_big = || format!("the {what} {shown} is too big to be a certificate or key (at most 1 MB)");
    if meta.len() > MAX_PEM_FILE {
        return Err(too_big());
    }
    let f = std::fs::File::open(path).map_err(|e| format!("the {what} {shown} could not be read: {e}"))?;
    let mut buf = Vec::new();
    f.take(MAX_PEM_FILE + 1).read_to_end(&mut buf).map_err(|e| format!("the {what} {shown} could not be read: {e}"))?;
    if buf.len() as u64 > MAX_PEM_FILE {
        return Err(too_big());
    }
    Ok(buf)
}

/// Dry run of what turning HTTPS on with AmberSight's own certificate
/// authority needs at the next start: `<data_dir>/tls` can be made and
/// written to. (Checked when the setting is saved, so a restart does not
/// stop on it.)
pub fn check_private_ca(data_dir: &Path) -> std::result::Result<(), String> {
    let dir = data_dir.join("tls");
    std::fs::create_dir_all(&dir).map_err(|e| format!("the folder {} for AmberSight's certificate could not be made: {e}", dir.display()))?;
    let probe = dir.join("write-test.new");
    write_private(&probe, b"x").map_err(|e| format!("AmberSight cannot write its certificate into {}: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    for f in ["ca.pem", "ca-key.pem"] {
        let p = dir.join(f);
        if p.exists() {
            std::fs::read(&p).map_err(|e| format!("AmberSight cannot read its certificate authority file {}: {e}", p.display()))?;
        }
    }
    Ok(())
}

/// Marker added to requests that arrived over TLS.
#[derive(Clone, Copy)]
pub struct Secure;

impl Runtime {
    pub fn acceptor(&self) -> tokio_rustls::TlsAcceptor {
        tokio_rustls::TlsAcceptor::from(self.config.read().unwrap().clone())
    }
    pub fn info(&self) -> Info {
        self.info.read().unwrap().clone()
    }
    /// This server's own certificate authority (private-CA mode only).
    pub fn ca_pem(&self) -> Option<String> {
        self.ca_pem.clone()
    }

    /// Re-check the certificate and swap it in if it was reissued.
    /// The certificate files (and so the mode) stay as the server started
    /// with: changing them needs a restart.
    pub fn refresh(&self, server: &crate::config::ServerConfig, data_dir: &Path) -> Result<()> {
        let mut server = server.clone();
        if let Some(st) = started() {
            server.tls_cert = st.cert.clone();
            server.tls_key = st.key.clone();
        }
        if self.fell_back {
            server.tls_cert = None;
            server.tls_key = None;
        }
        let (cfg, mut info, _, _) = build(&server, data_dir)?;
        info.note = self.info.read().unwrap().note.clone();
        let changed = {
            let old = self.info.read().unwrap();
            old.names != info.names || old.expires != info.expires
        };
        if changed {
            *self.config.write().unwrap() = cfg;
            *self.info.write().unwrap() = info;
            tracing::info!("HTTPS certificate reloaded");
        }
        Ok(())
    }
}

/// Build the runtime once at startup and register it globally.
pub fn init(server: &crate::config::ServerConfig, data_dir: &Path) -> Result<Arc<Runtime>> {
    let rt = Arc::new(start_runtime(server, data_dir)?);
    let _ = RUNTIME.set(rt.clone());
    Ok(rt)
}

/// The runtime for the start. Own certificate files that cannot be loaded
/// do not stop the start (a service would start again and again with no web
/// page to fix them in): AmberSight's own certificate authority is used
/// instead, with a warning in the log and in `/api/tls` (`note`).
fn start_runtime(server: &crate::config::ServerConfig, data_dir: &Path) -> Result<Runtime> {
    let own = server.tls_cert.is_some() && server.tls_key.is_some();
    let (built, fell_back) = match build(server, data_dir) {
        Ok(b) => (b, false),
        Err(e) if own => {
            tracing::error!(
                "HTTPS: your own certificate could not be loaded ({e:#}); using AmberSight's own certificate instead. Fix the files in Settings > Users & security > HTTPS, then restart"
            );
            let mut s = server.clone();
            s.tls_cert = None;
            s.tls_key = None;
            let mut b = build(&s, data_dir)?;
            b.1.note = Some(format!(
                "Your own certificate could not be loaded at start ({e:#}), so AmberSight's own certificate is used. Fix the certificate files, then restart AmberSight."
            ));
            (b, true)
        }
        Err(e) => return Err(e),
    };
    let (cfg, info, ca_der, ca_pem) = built;
    Ok(Runtime { config: RwLock::new(cfg), info: RwLock::new(info), ca_der, ca_pem, fell_back })
}

/// Whether the running listener uses the private CA in place of own
/// certificate files that could not be loaded at start.
pub fn fell_back() -> bool {
    current().is_some_and(|r| r.fell_back)
}

type Built = (Arc<rustls::ServerConfig>, Info, Option<Vec<u8>>, Option<String>);

fn build(server: &crate::config::ServerConfig, data_dir: &Path) -> Result<Built> {
    if let (Some(c), Some(k)) = (&server.tls_cert, &server.tls_key) {
        let cert = read_pem_file(c, "HTTPS certificate file").map_err(anyhow::Error::msg)?;
        let key = read_pem_file(k, "HTTPS key file").map_err(anyhow::Error::msg)?;
        let cfg = server_config(&cert, &key)?;
        let info = Info { mode: "files", names: Vec::new(), expires: 0, ca_fingerprint: None, note: None };
        return Ok((cfg, info, None, None));
    }
    let dir = data_dir.join("tls");
    std::fs::create_dir_all(&dir)?;
    let names = wanted_names(server);
    let ca = ensure_ca_for(&dir, &names)?;
    let issued = ensure_server_cert(&dir, &ca, &names, chrono::Utc::now().timestamp())?;
    let cert = std::fs::read(dir.join("cert.pem"))?;
    let key = std::fs::read(dir.join("key.pem"))?;
    let cfg = server_config(&cert, &key)?;
    let info = Info {
        mode: "private-ca",
        names: issued.names,
        expires: issued.expires,
        ca_fingerprint: Some(ca.fingerprint.clone()),
        note: None,
    };
    Ok((cfg, info, Some(ca.der), Some(ca.pem)))
}

/// A rustls server config (ring provider, h2 + http/1.1) from PEM bytes.
pub fn server_config(cert_pem: &[u8], key_pem: &[u8]) -> Result<Arc<rustls::ServerConfig>> {
    let chain: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(cert_pem).collect::<std::result::Result<_, _>>().context("bad certificate PEM")?;
    anyhow::ensure!(!chain.is_empty(), "no certificate found in the PEM data");
    let key = PrivateKeyDer::from_pem_slice(key_pem).context("bad private key PEM")?;
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("certificate and key do not match")?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

// ---------------------------------------------------------------- the CA

pub struct Ca {
    pub pem: String,
    pub der: Vec<u8>,
    pub fingerprint: String,
    /// Rebuilt issuer (same key + subject as the stored CA) used for signing;
    /// it is never served - clients get the stored `pem`.
    issuer: rcgen::Certificate,
    key: KeyPair,
    /// What the CA may vouch for (None: an old CA without limits).
    limits: Option<Limits>,
    /// This CA signed by the CA it replaced (sent with the server
    /// certificate, for devices that still trust the old one).
    cross_pem: Option<String>,
}

/// Names and addresses a limited CA may vouch for.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Limits {
    /// DNS names; each also covers the names under it ("local" covers
    /// "nvr.local").
    pub dns: Vec<String>,
    /// Address ranges ("192.168.0.0/16").
    pub ip: Vec<String>,
}

/// Home-network address ranges: private IPv4, Tailscale (100.64/10),
/// loopback, link-local, and IPv6 unique-local, link-local and loopback.
const HOME_RANGES: [&str; 9] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "fc00::/7", "fe80::/10", "::1/128"];
/// Name endings only a home network uses.
const HOME_NAMES: [&str; 5] = ["localhost", "local", "lan", "home.arpa", "internal"];

fn parse_range(r: &str) -> Option<(IpAddr, u8)> {
    let (a, b) = r.split_once('/')?;
    let ip: IpAddr = a.parse().ok()?;
    let bits: u8 = b.parse().ok()?;
    (bits <= if ip.is_ipv4() { 32 } else { 128 }).then_some((ip, bits))
}

fn in_range(ip: IpAddr, (net, bits): (IpAddr, u8)) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => bits == 0 || (u32::from(a) ^ u32::from(n)) >> (32 - bits as u32) == 0,
        (IpAddr::V6(a), IpAddr::V6(n)) => bits == 0 || (u128::from(a) ^ u128::from(n)) >> (128 - bits as u32) == 0,
        _ => false,
    }
}

impl Limits {
    /// The limits for a new CA covering `names` (this server's names now).
    pub fn for_names(names: &[String]) -> Limits {
        let mut l = Limits { dns: HOME_NAMES.iter().map(|s| s.to_string()).collect(), ip: HOME_RANGES.iter().map(|s| s.to_string()).collect() };
        for n in names {
            let n = n.trim().trim_end_matches('.').to_ascii_lowercase();
            if n.is_empty() || n.parse::<IpAddr>().is_ok() || !valid_dns_name(&n) || l.permits(&n) {
                continue;
            }
            // A Tailscale name: its whole tailnet ("tail-example.ts.net").
            let labels: Vec<&str> = n.split('.').collect();
            let entry = if n.ends_with(".ts.net") && labels.len() > 3 { labels[labels.len() - 3..].join(".") } else { n.clone() };
            if !l.dns.contains(&entry) {
                l.dns.push(entry);
            }
        }
        l
    }

    /// Whether a certificate signed by this CA may carry `name`.
    pub fn permits(&self, name: &str) -> bool {
        let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
        match name.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>() {
            Ok(ip) => self.ip.iter().filter_map(|r| parse_range(r)).any(|r| in_range(ip, r)),
            Err(_) => self.dns.iter().any(|d| name == *d || name.ends_with(&format!(".{d}"))),
        }
    }

    fn constraints(&self) -> NameConstraints {
        let mut permitted: Vec<GeneralSubtree> = self.dns.iter().map(|d| GeneralSubtree::DnsName(d.clone())).collect();
        permitted.extend(self.ip.iter().filter_map(|r| parse_range(r)).map(|(ip, bits)| GeneralSubtree::IpAddress(CidrSubnet::from_addr_prefix(ip, bits))));
        NameConstraints { permitted_subtrees: permitted, excluded_subtrees: Vec::new() }
    }
}

fn ymd(epoch: i64) -> (i32, u8, u8) {
    use chrono::Datelike;
    let d = chrono::DateTime::from_timestamp(epoch, 0).unwrap_or_default();
    (d.year(), d.month() as u8, d.day() as u8)
}

fn ca_params(common_name: &str, now: i64, limits: Option<&Limits>) -> CertificateParams {
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    p.distinguished_name = dn;
    match limits {
        // (Signs server certificates only: no CA below it.)
        Some(l) => {
            p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
            p.name_constraints = Some(l.constraints());
        }
        None => p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained),
    }
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let (a, b) = (ymd(now - DAY), ymd(now + 3650 * DAY));
    p.not_before = rcgen::date_time_ymd(a.0, a.1, a.2);
    p.not_after = rcgen::date_time_ymd(b.0, b.1, b.2);
    p
}

/// `AA:BB:..` SHA-256 of a DER certificate.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// The CA's common name, read back from its stored DER (so the rebuilt
/// issuer has the identical subject even if the hostname changed since).
fn ca_common_name(der: &[u8]) -> Option<String> {
    let needle = CA_NAME_PREFIX.as_bytes();
    let at = der.windows(needle.len()).position(|w| w == needle)?;
    if at < 2 || !matches!(der[at - 2], 0x0c | 0x13) {
        return None;
    }
    let len = der[at - 1] as usize;
    String::from_utf8(der.get(at..at + len)?.to_vec()).ok()
}

fn load_ca(pem: &str, key_pem: &str, limits: Option<Limits>, cross_pem: Option<String>) -> Result<Ca> {
    let der = CertificateDer::from_pem_slice(pem.as_bytes()).context("bad CA certificate")?.to_vec();
    let key = KeyPair::from_pem(key_pem).context("bad CA key")?;
    // The key must be this certificate's (not left over from a replaced CA).
    anyhow::ensure!(der.windows(key.public_key_raw().len()).any(|w| w == key.public_key_raw()), "the CA key does not belong to the CA certificate");
    let cn = ca_common_name(&der).context("unrecognized CA subject")?;
    let issuer = ca_params(&cn, chrono::Utc::now().timestamp(), limits.as_ref()).self_signed(&key)?;
    Ok(Ca { pem: pem.to_string(), fingerprint: fingerprint(&der), der, issuer, key, limits, cross_pem })
}

/// The dNSName name constraints (permitted and excluded) of a CA certificate.
fn ca_dns_constraints(der: &[u8]) -> Vec<String> {
    // One DER element: (tag, content, rest).
    fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&tag, b) = b.split_first()?;
        let (&l0, b) = b.split_first()?;
        let (len, b) = if l0 < 0x80 {
            (l0 as usize, b)
        } else {
            let n = (l0 & 0x7f) as usize;
            if n == 0 || n > 3 || b.len() < n {
                return None;
            }
            (b[..n].iter().fold(0usize, |a, &x| (a << 8) | x as usize), &b[n..])
        };
        (b.len() >= len).then(|| (tag, &b[..len], &b[len..]))
    }
    let oid = [0x06, 0x03, 0x55, 0x1d, 0x1e];
    let Some(at) = der.windows(oid.len()).position(|w| w == oid) else { return Vec::new() };
    let mut rest = &der[at + oid.len()..];
    // Optional "critical" flag, then the extension value (an OCTET STRING).
    if let Some((0x01, _, r)) = tlv(rest) {
        rest = r;
    }
    let mut out = Vec::new();
    let Some((0x04, value, _)) = tlv(rest) else { return out };
    let Some((0x30, seq, _)) = tlv(value) else { return out };
    let mut groups = seq;
    while let Some((tag, subtrees, r)) = tlv(groups) {
        groups = r;
        if tag != 0xa0 && tag != 0xa1 {
            continue;
        }
        let mut st = subtrees;
        while let Some((0x30, subtree, r)) = tlv(st) {
            st = r;
            if let Some((0x82, name, _)) = tlv(subtree) {
                out.push(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    out
}

/// Log, plainly and once, a stored CA whose name constraints strict
/// clients (rustls) refuse. The CA is not replaced: devices trust it and
/// linked servers pin it.
fn warn_bad_ca_constraints(der: &[u8]) {
    let bad: Vec<String> = ca_dns_constraints(der).into_iter().filter(|n| !valid_dns_name(n)).collect();
    if !bad.is_empty() {
        tracing::warn!(
            "HTTPS: the private CA lists '{}' as a permitted name, which is not a valid network name. Browsers may accept it, but linked servers and capture helpers refuse the certificate (MalformedNameConstraint). It is not replaced automatically, because devices trust it. To fix it, stop AmberSight, delete ca.pem, ca-key.pem, ca.json, cross.pem, cert.pem, key.pem and server.json in the tls folder of the data folder and start it again (see Troubleshooting in docs/install.md); every device then has to trust the server again and linked servers must be linked again",
            bad.join("', '")
        );
    }
}

/// Whether a CA certificate carries name constraints (OID 2.5.29.30).
fn has_name_constraints(der: &[u8]) -> bool {
    der.windows(5).any(|w| w == [0x06, 0x03, 0x55, 0x1d, 0x1e])
}

/// Write `bytes` to `path` by way of a temporary file (never half-written).
fn replace_file(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    let tmp = path.with_extension("new");
    if private {
        write_private(&tmp, bytes)?;
    } else {
        std::fs::write(&tmp, bytes)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Load the CA from `dir`, creating it on first use (limited).
#[cfg(test)]
pub fn ensure_ca(dir: &Path) -> Result<Ca> {
    ensure_ca_for(dir, &[])
}

/// As `ensure_ca`; `names` (the server's names now) go into the limits of
/// a CA made now. An old CA without limits is replaced (see the module
/// comment).
pub fn ensure_ca_for(dir: &Path, names: &[String]) -> Result<Ca> {
    let (pf, kf, jf, xf) = (dir.join("ca.pem"), dir.join("ca-key.pem"), dir.join("ca.json"), dir.join("cross.pem"));
    // A replacement stopped after the new key went into place: its
    // certificate follows.
    if pf.with_extension("new").exists() && !kf.with_extension("new").exists() {
        std::fs::rename(pf.with_extension("new"), &pf)?;
    }
    let limits: Option<Limits> = std::fs::read(&jf).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let cross = std::fs::read_to_string(&xf).ok();
    let mut old: Option<Ca> = None;
    if let (Ok(pem), Ok(key)) = (std::fs::read_to_string(&pf), std::fs::read_to_string(&kf)) {
        match load_ca(&pem, &key, limits.clone(), cross) {
            Ok(ca) if ca.limits.is_some() => {
                warn_bad_ca_constraints(&ca.der);
                return Ok(ca);
            }
            // Limited, but its ca.json was lost: the limits are made again
            // (only used to pick the names a server certificate carries).
            Ok(mut ca) if has_name_constraints(&ca.der) => {
                let mut all = names.to_vec();
                all.extend(hostname().ok());
                let l = Limits::for_names(&all);
                let _ = replace_file(&jf, &serde_json::to_vec_pretty(&l)?, false);
                ca.limits = Some(l);
                warn_bad_ca_constraints(&ca.der);
                return Ok(ca);
            }
            Ok(ca) => old = Some(ca),
            Err(e) => tracing::warn!("stored CA unusable ({e:#}); creating a new one - devices must trust it again"),
        }
    }
    let host = hostname().unwrap_or_else(|_| "server".into());
    let mut all = names.to_vec();
    all.push(host.clone());
    let limits = Limits::for_names(&all);
    let cn = if old.is_some() { format!("{CA_NAME_PREFIX} ({host}, limited)") } else { format!("{CA_NAME_PREFIX} ({host})") };
    let key = KeyPair::generate()?;
    let params = ca_params(&cn, chrono::Utc::now().timestamp(), Some(&limits));
    // The old CA vouches for the new one, for whoever still trusts the old.
    let cross_pem = match &old {
        Some(o) => Some(params.clone().signed_by(&key, &o.issuer, &o.key)?.pem()),
        None => None,
    };
    let cert = params.self_signed(&key)?;
    let (pem, der) = (cert.pem(), cert.der().to_vec());
    if let Some(o) = &old {
        // Kept for reference only (no key): which CA devices used to trust.
        std::fs::write(dir.join("ca-old.pem"), &o.pem)?;
    }
    match &cross_pem {
        Some(x) => std::fs::write(&xf, x)?,
        None => {
            let _ = std::fs::remove_file(&xf);
        }
    }
    // Both new files first, then into place (key, certificate, limits): a
    // stop part way is finished on the next start (see the top).
    write_private(&kf.with_extension("new"), key.serialize_pem().as_bytes())?;
    std::fs::write(pf.with_extension("new"), pem.as_bytes())?;
    // The new key replaces the old one: the old key is first overwritten
    // (not just unlinked), so it does not linger on the disk.
    if old.is_some() {
        if let Ok(len) = std::fs::metadata(&kf).map(|m| m.len()) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&kf) {
                let _ = f.write_all(&vec![0u8; len as usize]).and_then(|_| f.sync_all());
            }
        }
    }
    std::fs::rename(kf.with_extension("new"), &kf)?;
    std::fs::rename(pf.with_extension("new"), &pf)?;
    replace_file(&jf, &serde_json::to_vec_pretty(&limits)?, false)?;
    if old.is_some() {
        tracing::info!("replaced the AmberSight CA with a limited one (home-network names only); devices that trust the old CA keep working, and the old CA's key is erased");
    } else {
        tracing::info!("created the AmberSight private CA in {}", dir.display());
    }
    Ok(Ca { fingerprint: fingerprint(&der), pem, der, issuer: cert, key, limits: Some(limits), cross_pem })
}

// ------------------------------------------------------ the server cert

#[derive(Serialize, Deserialize)]
struct Meta {
    names: Vec<String>,
    expires: i64,
    ca_fingerprint: String,
}

pub struct Issued {
    pub names: Vec<String>,
    pub expires: i64,
    #[allow(dead_code)] // read by tests
    pub reissued: bool,
}

/// Drop names a certificate cannot carry; sort and de-duplicate.
fn sanitize(names: &[String]) -> Vec<String> {
    let mut v: Vec<String> =
        names.iter().map(|n| n.trim().trim_end_matches('.').to_string()).filter(|n| !n.is_empty() && (n.parse::<IpAddr>().is_ok() || valid_dns_name(n)) && CertificateParams::new(vec![n.clone()]).is_ok()).collect();
    v.sort();
    v.dedup();
    v
}

/// Make sure `dir` holds a server certificate signed by `ca` that covers
/// `names`, is valid for at least 30 more days and was issued by this CA.
pub fn ensure_server_cert(dir: &Path, ca: &Ca, names: &[String], now: i64) -> Result<Issued> {
    let mut names = sanitize(names);
    // Only what the CA may vouch for (a browser refuses the whole
    // certificate otherwise): e.g. a public IPv6 address is left out.
    if let Some(l) = &ca.limits {
        let (keep, drop): (Vec<String>, Vec<String>) = names.into_iter().partition(|n| l.permits(n));
        if !drop.is_empty() {
            tracing::warn!("HTTPS certificate: left out {} (outside the home network the CA is limited to)", drop.join(", "));
        }
        names = keep;
    }
    let (cf, kf, mf) = (dir.join("cert.pem"), dir.join("key.pem"), dir.join("server.json"));
    if cf.exists() && kf.exists() {
        if let Some(m) = std::fs::read(&mf).ok().and_then(|b| serde_json::from_slice::<Meta>(&b).ok()) {
            if m.names == names && m.ca_fingerprint == ca.fingerprint && m.expires - now > RENEW_BEFORE_DAYS * DAY {
                return Ok(Issued { names, expires: m.expires, reissued: false });
            }
        }
    }
    let mut p = CertificateParams::new(names.clone()).context("certificate names")?;
    let mut dn = DistinguishedName::new();
    let cn = names.iter().find(|n| n.parse::<IpAddr>().is_err()).cloned().unwrap_or_else(|| "AmberSight".into());
    dn.push(DnType::CommonName, cn);
    p.distinguished_name = dn;
    p.is_ca = IsCa::NoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let start = now - DAY;
    let end = start + SERVER_VALID_DAYS * DAY;
    let (a, b) = (ymd(start), ymd(end));
    p.not_before = rcgen::date_time_ymd(a.0, a.1, a.2);
    p.not_after = rcgen::date_time_ymd(b.0, b.1, b.2);
    let key = KeyPair::generate()?;
    let cert = p.signed_by(&key, &ca.issuer, &ca.key)?;
    let expires = end.div_euclid(DAY) * DAY;
    write_private(&kf, key.serialize_pem().as_bytes())?;
    // Followed by the CA as signed by the one it replaced, if any (see the
    // module comment).
    std::fs::write(&cf, format!("{}{}", cert.pem(), ca.cross_pem.as_deref().unwrap_or("")))?;
    std::fs::write(&mf, serde_json::to_vec(&Meta { names: names.clone(), expires, ca_fingerprint: ca.fingerprint.clone() })?)?;
    tracing::info!("issued the HTTPS certificate for {}", names.join(", "));
    Ok(Issued { names, expires, reissued: true })
}

/// Every name and address the server certificate should cover.
pub fn wanted_names(server: &crate::config::ServerConfig) -> Vec<String> {
    let mut names: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Ok(h) = hostname() {
        if valid_dns_name(&h) {
            names.push(format!("{h}.local"));
            names.push(h);
        } else {
            warn_bad_hostname(&h);
        }
    }
    if !server.bind.is_empty() && server.bind != "0.0.0.0" && server.bind != "::" {
        names.push(server.bind.trim_matches(|c| c == '[' || c == ']').to_string());
    }
    names.extend(interface_addresses().into_iter().map(|a| a.to_string()));
    names.extend(server.tls_names.iter().cloned());
    sanitize(&names)
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // Created owner-only, so the content is never readable by others.
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes)?;
    #[cfg(windows)]
    {
        // Best effort: only SYSTEM and Administrators (the service account
        // normally is one of them) may read the key.
        let _ = std::process::Command::new(crate::lockdown::icacls_exe())
            .arg(path)
            .args(["/inheritance:r", "/grant:r", "*S-1-5-18:F", "*S-1-5-32-544:F"])
            .output();
        // And the account running AmberSight, when it is not one of those
        // (e.g. `ambersight run` as an ordinary user), so it can still save.
        if let (Ok(domain), Ok(user)) = (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            if !user.ends_with('$') && !user.eq_ignore_ascii_case("SYSTEM") {
                let _ = std::process::Command::new(crate::lockdown::icacls_exe()).arg(path).arg("/grant").arg(format!("{domain}\\{user}:F")).output();
            }
        }
    }
    Ok(())
}

/// Log (once per run) that this computer's name cannot go into certificates.
fn warn_bad_hostname(h: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            "HTTPS: this computer's name ('{h}') is not a valid network name, so it is left out of the certificates; use the computer's address, or add a valid name in Settings > Users & security > HTTPS"
        )
    });
}

fn hostname() -> Result<String> {
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        return Ok(h.to_lowercase());
    }
    let h = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .context("no hostname")?;
    Ok(h)
}

/// Non-loopback unicast addresses of this machine's up interfaces
/// (link-local ones are skipped: browsers cannot use a scoped address).
/// The address this computer reaches other hosts with (no packet is sent).
fn primary_address() -> Option<IpAddr> {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip())
        .ok()
}

fn interface_addresses() -> Vec<IpAddr> {
    let mut out = os_interface_addresses();
    // The address used to reach other hosts (also covers odd setups).
    out.extend(primary_address());
    out.retain(|a| match a {
        IpAddr::V4(v) => !v.is_loopback() && !v.is_unspecified() && !v.is_link_local() && !v.is_multicast(),
        IpAddr::V6(v) => !v.is_loopback() && !v.is_unspecified() && (v.segments()[0] & 0xffc0) != 0xfe80 && !v.is_multicast(),
    });
    out.sort();
    out.dedup();
    out
}

#[cfg(unix)]
fn os_interface_addresses() -> Vec<IpAddr> {
    use std::net::{Ipv4Addr, Ipv6Addr};
    let mut out = Vec::new();
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut p = head;
        while !p.is_null() {
            let ifa = &*p;
            p = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_flags & (libc::IFF_UP as u32) == 0 {
                continue;
            }
            match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))));
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    out.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                }
                _ => {}
            }
        }
        libc::freeifaddrs(head);
    }
    out
}

/// Windows: the machine's own host name resolves to its interface addresses.
#[cfg(not(unix))]
fn os_interface_addresses() -> Vec<IpAddr> {
    use std::net::ToSocketAddrs;
    match hostname() {
        Ok(h) => (h.as_str(), 0).to_socket_addrs().map(|i| i.map(|a| a.ip()).collect()).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------- download + status

/// Serve `/tls/ca.crt` (DER) or `/tls/ca.pem`; None if not in private-CA mode.
pub fn ca_download(path: &str) -> Option<Response> {
    let rt = current()?;
    let (ctype, name, body): (&str, &str, Vec<u8>) = match path {
        "/tls/ca.crt" => ("application/x-x509-ca-cert", "AmberSight-CA.crt", rt.ca_der.clone()?),
        "/tls/ca.pem" => ("application/x-pem-file", "AmberSight-CA.pem", rt.ca_pem.clone()?.into_bytes()),
        _ => return None,
    };
    Some(
        (
            [
                (header::CONTENT_TYPE, ctype.to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
            body,
        )
            .into_response(),
    )
}

pub async fn ca_crt() -> Response {
    ca_download("/tls/ca.crt").unwrap_or_else(|| (StatusCode::NOT_FOUND, "not found").into_response())
}

pub async fn ca_pem() -> Response {
    ca_download("/tls/ca.pem").unwrap_or_else(|| (StatusCode::NOT_FOUND, "not found").into_response())
}

/// `GET /api/tls`
pub async fn api_tls(req: axum::extract::Request) -> Response {
    let secure = req.extensions().get::<Secure>().is_some();
    let info = current().map(|r| r.info());
    let j = match info {
        Some(i) => serde_json::json!({
            "mode": i.mode, "names": i.names, "expires": i.expires,
            "ca_fingerprint_sha256": i.ca_fingerprint, "secure": secure, "note": i.note,
        }),
        None => serde_json::json!({
            "mode": "off", "names": [], "expires": 0, "ca_fingerprint_sha256": null, "secure": secure, "note": null,
        }),
    };
    axum::Json(j).into_response()
}

/// `GET /api/addresses` (admins): this computer's network addresses, for
/// links a phone can open (the phone sign-in code) when the page itself was
/// opened as localhost.
pub async fn api_addresses(
    axum::extract::State(engine): axum::extract::State<std::sync::Arc<crate::engine::Engine>>,
) -> Response {
    let bind = engine.config.lock().unwrap().server.bind.clone();
    // Listing addresses can ask the system resolver (Windows): keep it off
    // the async workers.
    let list = tokio::task::spawn_blocking(move || phone_addresses(&bind)).await.unwrap_or_default();
    axum::Json(serde_json::json!({ "addresses": list })).into_response()
}

/// The addresses a phone could reach this server on: only the bound address
/// when the server listens on one; otherwise the address this computer
/// reaches the network with first (not a virtual adapter's), then the others.
fn phone_addresses(bind: &str) -> Vec<String> {
    if let Ok(ip) = bind.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>() {
        if !ip.is_unspecified() {
            return if ip.is_loopback() { Vec::new() } else { vec![ip.to_string()] };
        }
    }
    let mut all = interface_addresses();
    let primary = primary_address();
    all.sort_by_key(|a| Some(*a) != primary);
    all.into_iter().map(|a| a.to_string()).collect()
}

// -------------------------------------------------------------- redirect

/// `https://<host>:<port><path>` for a plain-HTTP request. `host` is the
/// request's Host header (kept as typed, IP or name); without a usable one,
/// `fallback_host` (the address the client connected to) is used.
pub fn redirect_target(host_header: Option<&str>, fallback_host: &str, port: u16, path_and_query: &str) -> String {
    let host = host_header.and_then(host_only).unwrap_or_else(|| bracket(fallback_host));
    let pq = if path_and_query.starts_with('/') { path_and_query } else { "/" };
    format!("https://{host}:{port}{pq}")
}

fn bracket(h: &str) -> String {
    if h.contains(':') && !h.starts_with('[') {
        format!("[{h}]")
    } else {
        h.to_string()
    }
}

/// The host part of a Host header value (port removed, IPv6 kept bracketed).
fn host_only(h: &str) -> Option<String> {
    let h = h.trim();
    let host = if let Some(rest) = h.strip_prefix('[') {
        let end = rest.find(']')?;
        format!("[{}]", &rest[..end])
    } else if h.matches(':').count() == 1 {
        h.split(':').next()?.to_string()
    } else {
        bracket(h)
    };
    let ok = !host.is_empty()
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']' | '%'));
    ok.then_some(host)
}

#[cfg(test)]
mod tests {
    #[test]
    fn phone_addresses_follow_the_bind_address() {
        assert_eq!(super::phone_addresses("192.168.1.20"), vec!["192.168.1.20".to_string()]);
        assert_eq!(super::phone_addresses("[fd00::5]"), vec!["fd00::5".to_string()]);
        assert!(super::phone_addresses("127.0.0.1").is_empty());
        let any = super::phone_addresses("0.0.0.0");
        assert!(any.iter().all(|a| !a.starts_with("127.")));
    }
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("amber-tls-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ca_is_created_once() {
        let dir = tmp("ca");
        let a = ensure_ca(&dir).unwrap();
        let b = ensure_ca(&dir).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.pem, b.pem);
        assert!(a.pem.contains("BEGIN CERTIFICATE"));
        assert!(std::fs::read_to_string(dir.join("ca-key.pem")).unwrap().contains("PRIVATE KEY"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(dir.join("ca-key.pem")).unwrap().permissions().mode();
            assert_eq!(m & 0o077, 0);
        }
        assert!(ca_common_name(&a.der).unwrap().starts_with(CA_NAME_PREFIX));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_cert_contents_and_reissue() {
        let dir = tmp("srv");
        let ca = ensure_ca(&dir).unwrap();
        let now = chrono::Utc::now().timestamp();
        let n1 = names(&["localhost", "127.0.0.1", "::1", "192.168.1.5", "100.64.0.9", "cam.example.ts.net"]);
        let i1 = ensure_server_cert(&dir, &ca, &n1, now).unwrap();
        assert!(i1.reissued);
        let days = (i1.expires - now) / DAY;
        assert!((395..=397).contains(&days), "{days}");
        let der = CertificateDer::from_pem_slice(&std::fs::read(dir.join("cert.pem")).unwrap()).unwrap().to_vec();
        // EKU serverAuth (1.3.6.1.5.5.7.3.1)
        assert!(der.windows(8).any(|w| w == [0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01]));
        // Same inputs: reused.
        assert!(!ensure_server_cert(&dir, &ca, &n1, now + DAY).unwrap().reissued);
        // A new name: reissued under the same CA.
        let mut n2 = n1.clone();
        n2.push("10.1.2.7".into());
        let i2 = ensure_server_cert(&dir, &ca, &n2, now).unwrap();
        assert!(i2.reissued && i2.names.contains(&"10.1.2.7".to_string()));
        assert_eq!(ensure_ca(&dir).unwrap().fingerprint, ca.fingerprint);
        // Near expiry: reissued.
        assert!(ensure_server_cert(&dir, &ca, &n2, now + 380 * DAY).unwrap().reissued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn chain_verifies_against_only_the_ca() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tmp("chain");
        let ca = ensure_ca(&dir).unwrap();
        let now = chrono::Utc::now().timestamp();
        // Reload the CA from disk so signing uses the rebuilt issuer.
        let ca2 = ensure_ca(&dir).unwrap();
        ensure_server_cert(&dir, &ca2, &names(&["localhost", "127.0.0.1", "192.168.77.5", "cam.lan"]), now).unwrap();
        let sc = server_config(&std::fs::read(dir.join("cert.pem")).unwrap(), &std::fs::read(dir.join("key.pem")).unwrap()).unwrap();
        assert_eq!(sc.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(ca.der.clone())).unwrap();
        let cc = Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        for (name, ok) in [("192.168.77.5", true), ("127.0.0.1", true), ("cam.lan", true), ("localhost", true), ("10.9.8.7", false)] {
            let (c, s) = tokio::io::duplex(16 * 1024);
            let acc = tokio_rustls::TlsAcceptor::from(sc.clone());
            let srv = tokio::spawn(async move {
                if let Ok(mut t) = acc.accept(s).await {
                    let _ = t.write_all(b"hi").await;
                    let _ = t.shutdown().await;
                }
            });
            let sn = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
            let r = tokio_rustls::TlsConnector::from(cc.clone()).connect(sn, c).await;
            assert_eq!(r.is_ok(), ok, "{name}");
            if let Ok(mut t) = r {
                let mut b = Vec::new();
                let _ = t.read_to_end(&mut b).await;
                assert_eq!(b, b"hi");
            }
            srv.abort();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether a TLS client trusting only `root` accepts the server
    /// (`cert_pem` chain + `key_pem`) as `name`.
    async fn accepted(cert_pem: &[u8], key_pem: &[u8], root: &[u8], name: &str) -> bool {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let sc = server_config(cert_pem, key_pem).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(root.to_vec())).unwrap();
        let cc = Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let (c, s) = tokio::io::duplex(32 * 1024);
        let acc = tokio_rustls::TlsAcceptor::from(sc);
        let srv = tokio::spawn(async move {
            if let Ok(mut t) = acc.accept(s).await {
                let _ = t.write_all(b"hi").await;
                let _ = t.shutdown().await;
            }
        });
        let sn = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
        let ok = match tokio_rustls::TlsConnector::from(cc).connect(sn, c).await {
            Ok(mut t) => {
                let mut b = Vec::new();
                let _ = t.read_to_end(&mut b).await;
                b == b"hi"
            }
            Err(_) => false,
        };
        srv.abort();
        ok
    }

    #[test]
    fn limits_cover_home_networks_only() {
        let l = Limits::for_names(&names(&["nvr", "nvr.local", "nvr.tail-example.ts.net", "192.168.1.71", "nvr.example.com"]));
        for yes in ["192.168.1.71", "10.1.2.3", "100.100.1.2", "127.0.0.1", "::1", "fd7a:115c::1", "nvr", "nvr.local", "cam.lan", "other.tail-example.ts.net", "nvr.example.com", "localhost"] {
            assert!(l.permits(yes), "{yes}");
        }
        for no in ["8.8.8.8", "2606:8e80::1", "bank.example.com", "example.com", "evil.ts.net", "notlocal", "nvr.evil.com"] {
            assert!(!l.permits(no), "{no}");
        }
    }

    #[tokio::test]
    async fn a_new_ca_is_limited_and_server_certificates_follow() {
        let dir = tmp("limited");
        let ca = ensure_ca_for(&dir, &names(&["localhost", "127.0.0.1"])).unwrap();
        assert!(has_name_constraints(&ca.der));
        assert!(dir.join("ca.json").exists() && !dir.join("cross.pem").exists());
        let now = chrono::Utc::now().timestamp();
        let i = ensure_server_cert(&dir, &ca, &names(&["localhost", "192.168.9.9", "2606:8e80::5", "8.8.4.4"]), now).unwrap();
        assert_eq!(i.names, names(&["192.168.9.9", "localhost"]), "public addresses are left out");
        let (cert, key) = (std::fs::read(dir.join("cert.pem")).unwrap(), std::fs::read(dir.join("key.pem")).unwrap());
        assert!(accepted(&cert, &key, &ca.der, "192.168.9.9").await);
        // A certificate for a public site, signed by the limited CA, is refused.
        let mut p = CertificateParams::new(vec!["bank.example.com".to_string()]).unwrap();
        p.is_ca = IsCa::NoCa;
        let k = KeyPair::generate().unwrap();
        let bad = p.signed_by(&k, &ca.issuer, &ca.key).unwrap();
        assert!(!accepted(bad.pem().as_bytes(), k.serialize_pem().as_bytes(), &ca.der, "bank.example.com").await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_old_ca_is_replaced_and_still_trusted_through_the_new_one() {
        let dir = tmp("replace");
        // An old, unlimited CA as made before limits existed.
        let old_key = KeyPair::generate().unwrap();
        let old = ca_params(&format!("{CA_NAME_PREFIX} (test)"), chrono::Utc::now().timestamp(), None).self_signed(&old_key).unwrap();
        std::fs::write(dir.join("ca.pem"), old.pem()).unwrap();
        write_private(&dir.join("ca-key.pem"), old_key.serialize_pem().as_bytes()).unwrap();
        let old_der = old.der().to_vec();
        let n = names(&["localhost", "192.168.9.9"]);
        let ca = ensure_ca_for(&dir, &n).unwrap();
        assert_ne!(ca.der, old_der);
        assert!(has_name_constraints(&ca.der));
        // The old key is gone; the old certificate is kept for reference.
        let key_now = std::fs::read_to_string(dir.join("ca-key.pem")).unwrap();
        assert_ne!(key_now, old_key.serialize_pem());
        assert!(!dir.join("ca-key.new").exists() && !dir.join("ca.new").exists());
        assert!(dir.join("ca-old.pem").exists() && dir.join("cross.pem").exists());
        // Loaded again: the same limited CA, not replaced a second time.
        let again = ensure_ca_for(&dir, &n).unwrap();
        assert_eq!(again.fingerprint, ca.fingerprint);
        assert!(again.cross_pem.is_some());
        ensure_server_cert(&dir, &again, &n, chrono::Utc::now().timestamp()).unwrap();
        let (cert, key) = (std::fs::read(dir.join("cert.pem")).unwrap(), std::fs::read(dir.join("key.pem")).unwrap());
        // Trusted by devices with the new CA, and by those with only the old.
        assert!(accepted(&cert, &key, &ca.der, "192.168.9.9").await);
        assert!(accepted(&cert, &key, &old_der, "192.168.9.9").await);
        // ca.json lost: the CA is still known to be limited (no second replacement).
        std::fs::remove_file(dir.join("ca.json")).unwrap();
        assert_eq!(ensure_ca_for(&dir, &n).unwrap().fingerprint, ca.fingerprint);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wanted_names_include_configured_and_loopback() {
        let mut s = crate::config::ServerConfig::default();
        s.tls_names = names(&["nvr.tail1234.ts.net", "10.1.2.3"]);
        s.bind = "192.168.5.5".into();
        let n = wanted_names(&s);
        for want in ["localhost", "127.0.0.1", "::1", "nvr.tail1234.ts.net", "10.1.2.3", "192.168.5.5"] {
            assert!(n.contains(&want.to_string()), "{want} in {n:?}");
        }
    }

    #[test]
    fn extra_names_are_checked() {
        assert_eq!(check_names(&names(&[" NVR.tail1234.ts.net. ", "10.1.2.3", "", "[fd00::1]", "nvr.tail1234.ts.net", "my_cam.lan"])).unwrap(), names(&["nvr.tail1234.ts.net", "10.1.2.3", "fd00::1", "my_cam.lan"]));
        for bad in ["https://nvr.lan", "nvr.lan/live", "two words", "nvr..lan", "-nvr.lan", "nvr.lan:8090", "*.lan", "caf\u{e9}.lan"] {
            assert!(check_names(&names(&[bad])).is_err(), "{bad}");
        }
        assert!(check_names(&[format!("{}.lan", "a".repeat(250))]).is_err());
        let many: Vec<String> = (0..51).map(|i| format!("cam{i}.lan")).collect();
        assert!(check_names(&many).is_err());
        assert!(check_names(&many[..50]).is_ok());
    }

    #[test]
    fn own_certificate_files_are_checked() {
        let dir = tmp("files");
        let k = KeyPair::generate().unwrap();
        let c = CertificateParams::new(vec!["nvr.lan".to_string()]).unwrap().self_signed(&k).unwrap();
        let other = KeyPair::generate().unwrap();
        let (cf, kf, of, junk) = (dir.join("c.pem"), dir.join("k.pem"), dir.join("o.pem"), dir.join("junk.pem"));
        std::fs::write(&cf, c.pem()).unwrap();
        std::fs::write(&kf, k.serialize_pem()).unwrap();
        std::fs::write(&of, other.serialize_pem()).unwrap();
        std::fs::write(&junk, "not a certificate").unwrap();
        assert!(check_files(None, None).is_ok());
        assert!(check_files(Some(&cf), Some(&kf)).is_ok());
        assert!(check_files(Some(&cf), None).unwrap_err().contains("both"));
        assert!(check_files(None, Some(&kf)).unwrap_err().contains("both"));
        assert!(check_files(Some(&dir.join("missing.pem")), Some(&kf)).unwrap_err().contains("could not be read"));
        assert!(check_files(Some(&junk), Some(&kf)).unwrap_err().contains("no certificate"));
        assert!(check_files(Some(&cf), Some(&junk)).unwrap_err().contains("no private key"));
        // The key of another certificate.
        assert!(check_files(Some(&cf), Some(&of)).unwrap_err().contains("does not belong"));
        // Certificate and key swapped.
        assert!(check_files(Some(&kf), Some(&cf)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn own_certificate_files_must_be_small_regular_files() {
        let dir = tmp("pemsize");
        let k = KeyPair::generate().unwrap();
        let c = CertificateParams::new(vec!["nvr.lan".to_string()]).unwrap().self_signed(&k).unwrap();
        let (cf, kf, big) = (dir.join("c.pem"), dir.join("k.pem"), dir.join("big.pem"));
        std::fs::write(&cf, c.pem()).unwrap();
        std::fs::write(&kf, k.serialize_pem()).unwrap();
        // A real chain with a large file (just over 1 MB) is refused unread.
        let mut b = c.pem().into_bytes();
        b.resize(MAX_PEM_FILE as usize + 1, b'\n');
        std::fs::write(&big, b).unwrap();
        assert!(check_files(Some(&big), Some(&kf)).unwrap_err().contains("too big"));
        // A folder.
        assert!(check_files(Some(&dir), Some(&kf)).unwrap_err().contains("not a regular file"));
        // A device that never ends.
        #[cfg(unix)]
        {
            let e = check_files(Some(Path::new("/dev/zero")), Some(&kf)).unwrap_err();
            assert!(e.contains("not a regular file"), "{e}");
            let e = check_files(Some(&cf), Some(Path::new("/dev/zero"))).unwrap_err();
            assert!(e.contains("not a regular file"), "{e}");
        }
        assert!(check_files(Some(&cf), Some(&kf)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_own_certificate_falls_back_to_the_private_ca() {
        let dir = tmp("fallback");
        let s = crate::config::ServerConfig {
            tls: true,
            tls_cert: Some(dir.join("gone.pem")),
            tls_key: Some(dir.join("gone-key.pem")),
            ..Default::default()
        };
        let rt = start_runtime(&s, &dir).expect("starts with the private CA");
        let i = rt.info();
        assert_eq!(i.mode, "private-ca");
        assert!(rt.fell_back && rt.ca_pem.is_some());
        let note = i.note.expect("a note");
        assert!(note.contains("could not be loaded"), "{note}");
        // Kept when the certificate is checked again.
        rt.refresh(&s, &dir).unwrap();
        assert!(rt.info().note.is_some() && rt.info().mode == "private-ca");
        // Usable files: no fallback.
        let k = KeyPair::generate().unwrap();
        let c = CertificateParams::new(vec!["nvr.lan".to_string()]).unwrap().self_signed(&k).unwrap();
        std::fs::write(dir.join("gone.pem"), c.pem()).unwrap();
        std::fs::write(dir.join("gone-key.pem"), k.serialize_pem()).unwrap();
        let rt = start_runtime(&s, &dir).unwrap();
        assert!(!rt.fell_back && rt.info().mode == "files" && rt.info().note.is_none());
        // Without own files, a failure still stops the start (nothing to fall back to).
        let file = dir.join("not-a-folder");
        std::fs::write(&file, "x").unwrap();
        assert!(start_runtime(&crate::config::ServerConfig { tls: true, ..Default::default() }, &file).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_ca_setup_is_checked_before_saving() {
        let dir = tmp("cacheck");
        assert!(check_private_ca(&dir).is_ok());
        assert!(dir.join("tls").is_dir() && !dir.join("tls").join("write-test.new").exists());
        // A data folder that is a file: the certificate folder cannot be made.
        let file = dir.join("file");
        std::fs::write(&file, "x").unwrap();
        let e = check_private_ca(&file).unwrap_err();
        assert!(e.contains("could not be made"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_is_needed_for_the_mode_and_the_files_only() {
        let mut s = crate::config::ServerConfig::default();
        let off = Started::of(&s);
        assert!(!restart_needed(&off, &s));
        // Names and files saved while HTTPS stays off: nothing to apply.
        s.tls_names = names(&["nvr.lan"]);
        s.tls_cert = Some("/x/c.pem".into());
        s.tls_key = Some("/x/k.pem".into());
        assert!(!restart_needed(&off, &s));
        s.tls = true;
        assert!(restart_needed(&off, &s));
        // Started with HTTPS (private CA).
        let mut on = crate::config::ServerConfig { tls: true, ..Default::default() };
        let st = Started::of(&on);
        on.tls_names = names(&["nvr.tail1.ts.net"]);
        assert!(!restart_needed(&st, &on), "names are applied in place");
        on.tls_cert = Some("/x/c.pem".into());
        on.tls_key = Some("/x/k.pem".into());
        assert!(restart_needed(&st, &on), "own certificate files");
        on.tls = false;
        assert!(restart_needed(&st, &on), "turned off");
    }

    #[test]
    fn redirect_targets() {
        let t = |h: Option<&str>, pq: &str| redirect_target(h, "192.168.1.9", 8090, pq);
        assert_eq!(t(Some("100.64.0.10:8090"), "/live?a=1"), "https://100.64.0.10:8090/live?a=1");
        assert_eq!(t(Some("nvr.local"), "/"), "https://nvr.local:8090/");
        assert_eq!(t(Some("[::1]:8090"), "/x"), "https://[::1]:8090/x");
        assert_eq!(t(Some("[fd7a::1]"), "/"), "https://[fd7a::1]:8090/");
        assert_eq!(t(Some("fd7a::1"), "/"), "https://[fd7a::1]:8090/");
        assert_eq!(t(None, "/a/b"), "https://192.168.1.9:8090/a/b");
        assert_eq!(t(Some("bad host\r\n"), "/"), "https://192.168.1.9:8090/");
        assert_eq!(redirect_target(None, "fe80::1", 1, "*"), "https://[fe80::1]:1/");
    }

    #[test]
    fn valid_and_invalid_dns_names() {
        for ok in ["pc", "my-pc", "my_pc", "nvr.lan", "123abc", "a.b.c", "Nvr", "x".repeat(63).as_str()] {
            assert!(valid_dns_name(ok), "{ok}");
        }
        let long_total = vec!["a".repeat(60); 5].join(".");
        for bad in ["", "nvr.example.com.", "pc-", "-w", "a.-b", "a-.b", "my pc", "a..b", "12345", "1.2.3", "caf\u{e9}", "a.\u{4e2d}", ".", &"x".repeat(64), &long_total] {
            assert!(!valid_dns_name(bad), "{bad}");
        }
        assert!(check_names(&names(&["pc-"])).is_err());
        assert!(check_names(&names(&["12345"])).is_err());
        assert_eq!(sanitize(&names(&["pc-", "ok.lan", "10.0.0.1", "caf\u{e9}", "my pc"])), names(&["10.0.0.1", "ok.lan"]));
        // A name given with a trailing dot is kept without it.
        assert_eq!(sanitize(&names(&["nvr.lan."])), names(&["nvr.lan"]));
        assert_eq!(check_names(&names(&["nvr.lan."])).unwrap(), names(&["nvr.lan"]));
    }

    #[tokio::test]
    async fn a_ca_made_with_an_invalid_computer_name_still_verifies() {
        let dir = tmp("badhost");
        let ca = ensure_ca_for(&dir, &names(&["pc-", "localhost", "my_pc"])).unwrap();
        assert!(!ca.limits.as_ref().unwrap().dns.iter().any(|d| d == "pc-"));
        assert!(ca_dns_constraints(&ca.der).iter().all(|n| valid_dns_name(n)));
        assert!(ca_dns_constraints(&ca.der).iter().any(|n| n == "my_pc"));
        let now = chrono::Utc::now().timestamp();
        ensure_server_cert(&dir, &ca, &names(&["pc-", "localhost", "192.168.9.9"]), now).unwrap();
        let (cert, key) = (std::fs::read(dir.join("cert.pem")).unwrap(), std::fs::read(dir.join("key.pem")).unwrap());
        assert!(accepted(&cert, &key, &ca.der, "localhost").await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The failure that was seen: a CA with `pc-` as a permitted name is
    /// refused by rustls whole, and the stored-CA check finds the name.
    #[tokio::test]
    async fn an_old_ca_with_a_bad_constraint_is_refused_by_strict_clients_and_found() {
        let mut l = Limits::for_names(&[]);
        l.dns.push("pc-".to_string());
        let key = KeyPair::generate().unwrap();
        let ca = ca_params("bad", chrono::Utc::now().timestamp(), Some(&l)).self_signed(&key).unwrap();
        assert!(ca_dns_constraints(ca.der()).iter().any(|n| n == "pc-"));
        let mut p = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        p.is_ca = IsCa::NoCa;
        let k = KeyPair::generate().unwrap();
        let c = p.signed_by(&k, &ca, &key).unwrap();
        assert!(!accepted(c.pem().as_bytes(), k.serialize_pem().as_bytes(), ca.der(), "localhost").await);
    }
}
