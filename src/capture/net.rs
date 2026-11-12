//! TLS and minimal HTTP/1.1 for the helper. Every connection is verified
//! against the server's own CA only, except the one first-contact request
//! that downloads that CA (the user confirms its fingerprint).

use anyhow::{anyhow, bail, Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the server is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Host name or IP (no brackets).
    pub host: String,
    pub port: u16,
    /// For the `Host:` header (`host:port`, IPv6 bracketed).
    pub authority: String,
}

impl Target {
    /// From `https://host[:port][/...]`. Plain http is refused.
    pub fn parse(server: &str) -> Result<Target> {
        let u = url::Url::parse(server.trim()).with_context(|| format!("'{server}' is not a server address"))?;
        if u.scheme() != "https" {
            bail!("the server address must start with https:// (turn on HTTPS on the server)");
        }
        let host = u.host_str().ok_or_else(|| anyhow!("the server address has no host"))?;
        let port = u.port().unwrap_or(443);
        let bare = host.trim_matches(|c| c == '[' || c == ']').to_string();
        let authority = if bare.contains(':') { format!("[{bare}]:{port}") } else { format!("{bare}:{port}") };
        Ok(Target { host: bare, port, authority })
    }

    pub fn base_url(&self) -> String {
        format!("https://{}", self.authority)
    }

    fn server_name(&self) -> Result<ServerName<'static>> {
        ServerName::try_from(self.host.clone()).map_err(|_| anyhow!("bad host name {}", self.host))
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The DER of the first certificate in `pem`.
pub fn ca_der(pem: &str) -> Result<CertificateDer<'static>> {
    CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| anyhow!("not a PEM certificate: {e}"))
}

/// A client config that trusts exactly one CA (and nothing from the system store).
pub fn pinned(ca_pem: &str) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca_der(ca_pem)?).context("the saved server certificate authority is not usable")?;
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// First contact only: used to download `/tls/ca.pem`, whose fingerprint the
/// user then compares with the one the server's admin page shows. Nothing
/// secret is sent on this connection.
pub fn first_contact() -> Arc<rustls::ClientConfig> {
    let p = provider();
    let mut cfg = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(FirstContact(p)))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

#[derive(Debug)]
struct FirstContact(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for FirstContact {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// TCP + TLS to the server.
pub async fn connect(t: &Target, cfg: Arc<rustls::ClientConfig>) -> Result<TlsStream<TcpStream>> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((t.host.as_str(), t.port)))
        .await
        .map_err(|_| anyhow!("timed out reaching {}", t.authority))?
        .with_context(|| format!("cannot reach {}", t.authority))?;
    tcp.set_nodelay(true).ok();
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, tokio_rustls::TlsConnector::from(cfg).connect(t.server_name()?, tcp))
        .await
        .map_err(|_| anyhow!("timed out in the TLS handshake with {}", t.authority))?
        .context("TLS handshake failed (is this the server you paired with?)")?;
    Ok(tls)
}

/// One request with `Connection: close`; returns (status, body).
pub async fn request(
    t: &Target,
    cfg: Arc<rustls::ClientConfig>,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<(u16, Vec<u8>)> {
    let io = connect(t, cfg).await?;
    request_on(io, &t.authority, method, path, content_type, body).await
}

pub async fn request_on<S: AsyncRead + AsyncWrite + Unpin>(
    io: S,
    authority: &str,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<(u16, Vec<u8>)> {
    request_on_with(io, authority, method, path, &[], content_type, body).await
}

/// One request with extra headers (e.g. Authorization) and `Connection: close`.
pub async fn request_with(
    t: &Target,
    cfg: Arc<rustls::ClientConfig>,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    content_type: Option<&str>,
    body: &[u8],
) -> Result<(u16, Vec<u8>)> {
    let io = connect(t, cfg).await?;
    request_on_with(io, &t.authority, method, path, headers, content_type, body).await
}

pub async fn request_on_with<S: AsyncRead + AsyncWrite + Unpin>(
    mut io: S,
    authority: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    content_type: Option<&str>,
    body: &[u8],
) -> Result<(u16, Vec<u8>)> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: AmberSight-Capture/{}\r\nAccept: */*\r\nConnection: close\r\n",
        env!("CARGO_PKG_VERSION")
    );
    for (k, v) in headers {
        anyhow::ensure!(!k.contains(['\r', '\n', ':']) && !v.contains(['\r', '\n']), "bad header");
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    if !body.is_empty() || method != "GET" {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    io.write_all(head.as_bytes()).await?;
    io.write_all(body).await?;
    io.flush().await?;
    let mut raw = Vec::new();
    let read = tokio::time::timeout(RESPONSE_TIMEOUT, async {
        let mut buf = [0u8; 8192];
        loop {
            match io.read(&mut buf).await {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.len() > 4 << 20 {
                        break Err(anyhow!("the answer is too large"));
                    }
                }
                // A peer that closes without close_notify after a full answer is fine.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => break Ok(()),
                Err(e) => break Err(e.into()),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("the server did not answer"))?;
    read?;
    parse_response(&raw)
}

/// Status and (de-chunked) body of a complete HTTP/1.1 response.
pub fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>)> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow!("incomplete answer from the server"))?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let status: u16 = parse_status_line(head.lines().next().unwrap_or(""))?;
    let rest = &raw[end + 4..];
    let header = |name: &str| {
        head.lines().skip(1).filter_map(|l| l.split_once(':')).find(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_ascii_lowercase())
    };
    let body = if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        dechunk(rest)?
    } else if let Some(n) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        rest[..n.min(rest.len())].to_vec()
    } else {
        rest.to_vec()
    };
    Ok((status, body))
}

pub fn parse_status_line(line: &str) -> Result<u16> {
    let mut it = line.split_whitespace();
    let proto = it.next().unwrap_or("");
    if !proto.starts_with("HTTP/1.") {
        bail!("not an HTTP answer: {}", line.chars().take(60).collect::<String>());
    }
    it.next().and_then(|c| c.parse().ok()).ok_or_else(|| anyhow!("bad status line"))
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let nl = b.windows(2).position(|w| w == b"\r\n").ok_or_else(|| anyhow!("bad chunked answer"))?;
        let size_s = String::from_utf8_lossy(&b[..nl]);
        let size = usize::from_str_radix(size_s.split(';').next().unwrap_or("").trim(), 16).map_err(|_| anyhow!("bad chunk size"))?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        anyhow::ensure!(size.checked_add(2).is_some_and(|n| b.len() >= n), "truncated chunked answer");
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let t = Target::parse("https://192.168.1.5:8090/").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.authority.as_str()), ("192.168.1.5", 8090, "192.168.1.5:8090"));
        let t = Target::parse("https://[fd7a::1]:8090").unwrap();
        assert_eq!((t.host.as_str(), t.authority.as_str()), ("fd7a::1", "[fd7a::1]:8090"));
        assert_eq!(Target::parse("https://nvr.local").unwrap().port, 443);
        assert!(Target::parse("http://nvr.local:8090").is_err());
        assert!(Target::parse("nvr.local").is_err());
    }

    #[test]
    fn responses() {
        let (s, b) = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA").unwrap();
        assert_eq!((s, b.as_slice()), (200, &b"hello"[..]));
        let (s, b) = parse_response(b"HTTP/1.1 401 Unauthorized\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n").unwrap();
        assert_eq!((s, b.as_slice()), (401, &b"abcde"[..]));
        assert!(parse_response(b"SSH-2.0-x\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }
}
