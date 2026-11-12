//! A small client for S3-compatible storage (AWS S3, Backblaze B2, Wasabi,
//! MinIO, Cloudflare R2): put, get, list and delete objects, with large
//! files sent in parts. Requests are signed with AWS Signature Version 4
//! (the scheme all of these understand), written out here rather than
//! pulling in a large SDK.

use anyhow::{anyhow, bail, Context, Result};
use sha2::Digest as _;
use std::io::Read;
use std::time::Duration;

/// Files larger than this are sent in parts.
pub const MULTIPART_OVER: u64 = 64 << 20;
const PART: u64 = 16 << 20;

/// Percent-encode as SigV4 wants: everything but A-Z a-z 0-9 - _ . ~
/// (and '/' too when `keep_slash`, for object paths).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    super::crypt::hex(&sha2::Sha256::digest(data))
}

fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), msg.as_bytes()).as_ref().to_vec()
}

/// One request to sign.
pub struct SignInput<'a> {
    pub method: &'a str,
    /// Host header value (with :port when not the default).
    pub host: &'a str,
    /// The path exactly as sent (already encoded), e.g. "/bucket/a%20b.txt".
    pub path: &'a str,
    /// Query parameters, not encoded.
    pub query: &'a [(String, String)],
    /// Headers to sign besides host, x-amz-date and x-amz-content-sha256.
    pub headers: &'a [(String, String)],
    /// SHA-256 of the body in hex (or "UNSIGNED-PAYLOAD").
    pub payload_hash: &'a str,
    /// e.g. "20130524T000000Z".
    pub amz_date: &'a str,
    pub region: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
}

/// The query string as it is sent and signed: sorted, encoded.
pub fn canonical_query(q: &[(String, String)]) -> String {
    let mut v: Vec<(String, String)> = q.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    v.sort();
    v.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// The Authorization header value for a request (AWS Signature Version 4).
pub fn authorization(i: &SignInput) -> String {
    let mut headers: Vec<(String, String)> = i.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())).collect();
    headers.push(("host".into(), i.host.to_string()));
    headers.push(("x-amz-content-sha256".into(), i.payload_hash.to_string()));
    headers.push(("x-amz-date".into(), i.amz_date.to_string()));
    headers.sort();
    let canon_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: String = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{}\n{}\n{}\n{}\n{}\n{}", i.method, i.path, canonical_query(i.query), canon_headers, signed, i.payload_hash);
    let day = &i.amz_date[..8];
    let scope = format!("{day}/{}/s3/aws4_request", i.region);
    let to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", i.amz_date, sha256_hex(canonical.as_bytes()));
    let k = hmac(format!("AWS4{}", i.secret_key).as_bytes(), day);
    let k = hmac(&k, i.region);
    let k = hmac(&k, "s3");
    let k = hmac(&k, "aws4_request");
    let sig = super::crypt::hex(&hmac(&k, &to_sign));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}", i.access_key)
}

pub struct S3 {
    scheme: String,
    /// The endpoint's host[:port].
    host: String,
    region: String,
    bucket: String,
    prefix: String,
    access: String,
    secret: String,
    path_style: bool,
    agent: ureq::Agent,
    /// Files larger than this go in parts of `part` bytes (smaller in tests).
    multipart_over: u64,
    part: u64,
}

pub struct Resp {
    pub status: u16,
    pub etag: Option<String>,
    pub body: Vec<u8>,
}

/// "S3 said AccessDenied: Access Denied" from an error answer.
fn error_text(status: u16, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let pick = |tag: &str| {
        let (a, b) = (format!("<{tag}>"), format!("</{tag}>"));
        let s = text.find(&a)? + a.len();
        let e = text[s..].find(&b)? + s;
        Some(text[s..e].to_string())
    };
    match (pick("Code"), pick("Message")) {
        (Some(c), Some(m)) => format!("the storage said {c}: {m} (HTTP {status})"),
        (Some(c), None) => format!("the storage said {c} (HTTP {status})"),
        _ => format!("the storage answered HTTP {status}"),
    }
}

impl S3 {
    pub fn new(cfg: &crate::config::OffsiteConfig) -> Result<S3> {
        let region = if cfg.s3_region.trim().is_empty() { "us-east-1".to_string() } else { cfg.s3_region.trim().to_string() };
        let endpoint = cfg.s3_endpoint.trim();
        let endpoint = if endpoint.is_empty() {
            format!("https://s3.{region}.amazonaws.com")
        } else if endpoint.contains("://") {
            endpoint.to_string()
        } else {
            format!("https://{endpoint}")
        };
        let u = url::Url::parse(&endpoint).context("the S3 address is not valid")?;
        let host = u.host_str().ok_or_else(|| anyhow!("the S3 address has no host name"))?;
        // Plain http only to this same computer (a local MinIO, the tests):
        // anywhere else the keys and data would cross the network readable.
        let local = matches!(host, "127.0.0.1" | "localhost" | "[::1]");
        if !(u.scheme() == "https" || (u.scheme() == "http" && local)) {
            bail!("the S3 address must start with https://");
        }
        let host = match u.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        };
        let bucket = cfg.s3_bucket.trim().to_string();
        if bucket.is_empty() || bucket.contains('/') {
            bail!("enter the bucket name");
        }
        if cfg.s3_access_key.trim().is_empty() || cfg.s3_secret_key.is_empty() {
            bail!("enter the access key and secret key");
        }
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_per_read(Some(Duration::from_secs(120)))
            .timeout_per_write(Some(Duration::from_secs(120)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        Ok(S3 {
            scheme: u.scheme().to_string(),
            host,
            region,
            bucket,
            prefix: cfg.s3_prefix.trim().trim_matches('/').to_string(),
            access: cfg.s3_access_key.trim().to_string(),
            secret: cfg.s3_secret_key.clone(),
            path_style: cfg.s3_path_style,
            agent,
            multipart_over: MULTIPART_OVER,
            part: PART,
        })
    }

    /// Tests: send files in parts above `over` bytes.
    #[cfg(test)]
    pub fn with_parts(mut self, over: u64, part: u64) -> S3 {
        self.multipart_over = over;
        self.part = part;
        self
    }

    pub fn describe(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }

    /// The object name in the bucket for one of our keys.
    pub fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }

    /// Our key back from an object name (None: outside our prefix).
    fn our_key<'a>(&self, full: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            Some(full)
        } else {
            full.strip_prefix(&self.prefix)?.strip_prefix('/')
        }
    }

    /// (host, path) for an object (or the bucket when `key` is None).
    pub fn target(&self, full_key: Option<&str>) -> (String, String) {
        let k = full_key.map(|k| uri_encode(k, true)).unwrap_or_default();
        if self.path_style {
            let path = if full_key.is_some() { format!("/{}/{k}", self.bucket) } else { format!("/{}", self.bucket) };
            (self.host.clone(), path)
        } else {
            (format!("{}.{}", self.bucket, self.host), format!("/{k}"))
        }
    }

    /// Send one signed request. Network trouble and server errors (5xx) are
    /// tried again twice.
    /// `extra`: headers sent along unsigned (Range).
    fn send_raw(&self, method: &str, full_key: Option<&str>, query: &[(String, String)], extra: &[(&str, String)], body: &[u8]) -> Result<ureq::http::Response<ureq::Body>> {
        let (host, path) = self.target(full_key);
        let qs = canonical_query(query);
        let url = if qs.is_empty() { format!("{}://{host}{path}", self.scheme) } else { format!("{}://{host}{path}?{qs}", self.scheme) };
        let hash = sha256_hex(body);
        let mut last_err = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(3 * attempt));
            }
            let date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let auth = authorization(&SignInput {
                method,
                host: &host,
                path: &path,
                query,
                headers: &[],
                payload_hash: &hash,
                amz_date: &date,
                region: &self.region,
                access_key: &self.access,
                secret_key: &self.secret,
            });
            let mut req = ureq::http::Request::builder()
                .method(method)
                .uri(&url)
                .header("x-amz-date", &date)
                .header("x-amz-content-sha256", &hash)
                .header("authorization", auth);
            for (k, v) in extra {
                req = req.header(*k, v);
            }
            let req = req
                .body(body)
                .map_err(|e| anyhow!("bad request: {e}"))?;
            match self.agent.run(req) {
                Ok(r) if r.status().as_u16() >= 500 => {
                    let st = r.status().as_u16();
                    let mut r = r;
                    let b = r.body_mut().with_config().limit(1 << 20).read_to_vec().unwrap_or_default();
                    last_err = Some(anyhow!("{}", error_text(st, &b)));
                }
                Ok(r) => return Ok(r),
                Err(e) => last_err = Some(anyhow!("could not reach {}: {e}", self.host)),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("request failed")))
    }

    /// Send and read the whole answer (answers are small: lists, errors).
    fn send(&self, method: &str, full_key: Option<&str>, query: &[(String, String)], body: &[u8]) -> Result<Resp> {
        let mut r = self.send_raw(method, full_key, query, &[], body)?;
        let status = r.status().as_u16();
        let etag = r.headers().get("etag").and_then(|v| v.to_str().ok()).map(String::from);
        let body = r.body_mut().with_config().limit(64 << 20).read_to_vec().context("reading the answer")?;
        Ok(Resp { status, etag, body })
    }

    fn ok(r: Resp) -> Result<Resp> {
        if (200..300).contains(&r.status) {
            Ok(r)
        } else {
            Err(anyhow!("{}", error_text(r.status, &r.body)))
        }
    }

    pub fn put_bytes(&self, key: &str, data: &[u8]) -> Result<()> {
        Self::ok(self.send("PUT", Some(&self.full_key(key)), &[], data)?).map(|_| ())
    }

    /// Upload a file: in one request, or in parts when it is large.
    pub fn put_file(&self, key: &str, src: &std::path::Path) -> Result<()> {
        let size = std::fs::metadata(src)?.len();
        if size <= self.multipart_over {
            let data = std::fs::read(src)?;
            return self.put_bytes(key, &data);
        }
        let full = self.full_key(key);
        let r = Self::ok(self.send("POST", Some(&full), &[("uploads".into(), String::new())], &[])?)?;
        let id = xml_text(&r.body, "UploadId").ok_or_else(|| anyhow!("the storage did not start the upload"))?;
        let res = (|| -> Result<()> {
            // At most 10,000 parts are allowed.
            let part = self.part.max(size.div_ceil(9000));
            let mut f = std::fs::File::open(src)?;
            let mut etags = Vec::new();
            let mut n = 1u32;
            loop {
                let mut buf = Vec::with_capacity(part as usize);
                (&mut f).take(part).read_to_end(&mut buf)?;
                if buf.is_empty() {
                    break;
                }
                let q = [("partNumber".to_string(), n.to_string()), ("uploadId".to_string(), id.clone())];
                let r = Self::ok(self.send("PUT", Some(&full), &q, &buf)?)?;
                etags.push(r.etag.ok_or_else(|| anyhow!("the storage did not confirm part {n}"))?);
                n += 1;
            }
            let mut xml = String::from("<CompleteMultipartUpload>");
            for (i, e) in etags.iter().enumerate() {
                xml.push_str(&format!("<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>", i + 1, xml_escape(e)));
            }
            xml.push_str("</CompleteMultipartUpload>");
            let r = Self::ok(self.send("POST", Some(&full), &[("uploadId".into(), id.clone())], xml.as_bytes())?)?;
            // An error can come back with status 200 here.
            if String::from_utf8_lossy(&r.body).contains("<Error>") {
                bail!("{}", error_text(r.status, &r.body));
            }
            Ok(())
        })();
        if res.is_err() {
            let _ = self.send("DELETE", Some(&full), &[("uploadId".into(), id)], &[]);
        }
        res
    }

    pub fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let r = self.send("GET", Some(&self.full_key(key)), &[], &[])?;
        if r.status == 404 {
            return Ok(None);
        }
        Ok(Some(Self::ok(r)?.body))
    }

    /// Download into `dst`; more than `max` bytes is an error.
    pub fn get_to_file(&self, key: &str, dst: &std::path::Path, max: u64) -> Result<()> {
        let mut r = self.send_raw("GET", Some(&self.full_key(key)), &[], &[], &[])?;
        let status = r.status().as_u16();
        if !(200..300).contains(&status) {
            let b = r.body_mut().with_config().limit(1 << 20).read_to_vec().unwrap_or_default();
            bail!("{key}: {}", error_text(status, &b));
        }
        let mut f = super::create_private(dst)?;
        std::io::copy(&mut r.body_mut().with_config().limit(max).reader(), &mut f).with_context(|| format!("{key}: download failed or bigger than expected"))?;
        f.sync_all()?;
        Ok(())
    }

    /// The first `n` bytes of an object (a ranged GET).
    pub fn get_head(&self, key: &str, n: usize) -> Result<Option<Vec<u8>>> {
        let range = format!("bytes=0-{}", n.max(1) - 1);
        let mut r = self.send_raw("GET", Some(&self.full_key(key)), &[], &[("range", range)], &[])?;
        let status = r.status().as_u16();
        let body = r.body_mut().with_config().limit((n as u64).max(1 << 16)).read_to_vec().context("reading the answer")?;
        match status {
            404 => Ok(None),
            // An empty object has no bytes to give.
            416 => Ok(Some(Vec::new())),
            200..=299 => Ok(Some(body.into_iter().take(n).collect())),
            _ => bail!("{key}: {}", error_text(status, &body)),
        }
    }

    /// Every object under our prefix + `prefix`, as (our key, size).
    pub fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("list-type".to_string(), "2".to_string()), ("prefix".to_string(), self.full_key(prefix))];
            if let Some(t) = &token {
                q.push(("continuation-token".into(), t.clone()));
            }
            let r = Self::ok(self.send("GET", None, &q, &[])?)?;
            let (items, next) = parse_list_page(&r.body)?;
            out.extend(items.into_iter().filter_map(|(k, s)| self.our_key(&k).map(|o| (o.to_string(), s))));
            match next {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        Ok(out)
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        let r = self.send("DELETE", Some(&self.full_key(key)), &[], &[])?;
        if r.status == 404 {
            return Ok(());
        }
        Self::ok(r).map(|_| ())
    }
}

/// One page of a bucket list: (object name, size) and the token for the
/// next page (None: this was the last). A list that says there is more but
/// gives no way to get it is an error: a short list would make the backup
/// think files are missing (and re-send them) or let old ones be kept.
fn parse_list_page(body: &[u8]) -> Result<(Vec<(String, u64)>, Option<String>)> {
    let text = std::str::from_utf8(body).context("the list is not text")?;
    let doc = roxmltree::Document::parse(text).context("the list could not be read")?;
    let mut out = Vec::new();
    for c in doc.descendants().filter(|n| n.has_tag_name("Contents")) {
        let child = |name: &str| c.children().find(|n| n.has_tag_name(name)).and_then(|n| n.text()).map(String::from);
        if let (Some(k), Some(s)) = (child("Key"), child("Size")) {
            out.push((k, s.parse().with_context(|| format!("bad size in the list for {}", child("Key").unwrap_or_default()))?));
        }
    }
    let truncated = doc.descendants().find(|n| n.has_tag_name("IsTruncated")).and_then(|n| n.text()) == Some("true");
    let token = doc.descendants().find(|n| n.has_tag_name("NextContinuationToken")).and_then(|n| n.text()).filter(|t| !t.is_empty()).map(String::from);
    match (truncated, token) {
        (false, _) => Ok((out, None)),
        (true, Some(t)) => Ok((out, Some(t))),
        (true, None) => bail!("the storage sent an incomplete list without a way to get the rest"),
    }
}

fn xml_text(body: &[u8], tag: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let doc = roxmltree::Document::parse(text).ok()?;
    doc.descendants().find(|n| n.has_tag_name(tag)).and_then(|n| n.text()).map(String::from)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // The examples from AWS's "Signature Calculations for the Authorization
    // Header: Transferring Payload in a Single Chunk" (Amazon S3 API
    // reference), with their published signatures.
    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn sig(auth: &str) -> &str {
        auth.rsplit("Signature=").next().unwrap()
    }

    #[test]
    fn sigv4_get_object_example() {
        let a = authorization(&SignInput {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            path: "/test.txt",
            query: &[],
            headers: &[("Range".into(), "bytes=0-9".into())],
            payload_hash: EMPTY,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
            access_key: AK,
            secret_key: SK,
        });
        assert_eq!(sig(&a), "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
        assert!(a.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, "));
    }

    #[test]
    fn sigv4_put_object_example() {
        let body = b"Welcome to Amazon S3.";
        let hash = sha256_hex(body);
        assert_eq!(hash, "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
        let a = authorization(&SignInput {
            method: "PUT",
            host: "examplebucket.s3.amazonaws.com",
            path: &format!("/{}", uri_encode("test$file.text", true)),
            query: &[],
            headers: &[("Date".into(), "Fri, 24 May 2013 00:00:00 GMT".into()), ("x-amz-storage-class".into(), "REDUCED_REDUNDANCY".into())],
            payload_hash: &hash,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
            access_key: AK,
            secret_key: SK,
        });
        assert_eq!(sig(&a), "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd");
    }

    #[test]
    fn sigv4_list_objects_example() {
        let a = authorization(&SignInput {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            path: "/",
            query: &[("max-keys".into(), "2".into()), ("prefix".into(), "J".into())],
            headers: &[],
            payload_hash: EMPTY,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
            access_key: AK,
            secret_key: SK,
        });
        assert_eq!(sig(&a), "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
    }

    pub(crate) fn mock_cfg(port: u16) -> crate::config::OffsiteConfig {
        crate::config::OffsiteConfig {
            kind: "s3".into(),
            s3_endpoint: format!("http://127.0.0.1:{port}"),
            s3_bucket: super::super::mock_s3::BUCKET.into(),
            s3_prefix: "home/nvr".into(),
            s3_access_key: super::super::mock_s3::AK.into(),
            s3_secret_key: super::super::mock_s3::SK.into(),
            s3_path_style: true,
            ..Default::default()
        }
    }

    #[test]
    fn mock_server_put_list_get_delete_multipart() {
        let (port, st) = super::super::mock_s3::start();
        let s = S3::new(&mock_cfg(port)).unwrap().with_parts(100_000, 40_000);
        s.put_bytes("a/one.txt", b"hello").unwrap();
        s.put_bytes("a/two words.txt", b"x").unwrap();
        s.put_bytes("b/three", b"").unwrap();
        // A file sent in parts (3 parts of 40 kB and one smaller).
        let tmp = std::env::temp_dir().join(format!("ambs-s3-mp-{}", std::process::id()));
        let big: Vec<u8> = (0..130_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&tmp, &big).unwrap();
        s.put_file("a/big.bin", &tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(st.lock().unwrap().multipart_done, 1);
        assert_eq!(s.get_bytes("a/big.bin").unwrap().unwrap(), big);
        assert_eq!(st.lock().unwrap().objects.keys().next().unwrap(), "home/nvr/a/big.bin");
        // Listing pages through (2 per page) and strips the prefix.
        let mut l = s.list("a/").unwrap();
        l.sort();
        assert_eq!(l, vec![("a/big.bin".to_string(), 130_000), ("a/one.txt".to_string(), 5), ("a/two words.txt".to_string(), 1)]);
        assert_eq!(s.list("").unwrap().len(), 4);
        s.delete("a/one.txt").unwrap();
        assert!(s.get_bytes("a/one.txt").unwrap().is_none());
        assert_eq!(st.lock().unwrap().bad_signatures, 0);
        // A wrong secret is refused with the server's reason.
        let mut c = mock_cfg(port);
        c.s3_secret_key = "wrong".into();
        let err = S3::new(&c).unwrap().put_bytes("x", b"1").unwrap_err().to_string();
        assert!(err.contains("SignatureDoesNotMatch"), "{err}");
        assert!(super::super::test_connection(&mock_cfg(port)).unwrap().starts_with("Connected to s3://bkt/home/nvr"));
    }

    #[test]
    fn list_pages() {
        let page = |more: &str, tok: &str| format!("<ListBucketResult><Contents><Key>a/1</Key><Size>3</Size></Contents><IsTruncated>{more}</IsTruncated>{tok}</ListBucketResult>");
        let (items, next) = parse_list_page(page("false", "").as_bytes()).unwrap();
        assert_eq!((items, next), (vec![("a/1".to_string(), 3)], None));
        let (_, next) = parse_list_page(page("true", "<NextContinuationToken>t2</NextContinuationToken>").as_bytes()).unwrap();
        assert_eq!(next.as_deref(), Some("t2"));
        // More to come but no token: an error, not a short list.
        assert!(parse_list_page(page("true", "").as_bytes()).is_err());
        assert!(parse_list_page(page("true", "<NextContinuationToken></NextContinuationToken>").as_bytes()).is_err());
    }

    #[test]
    fn plain_http_only_locally() {
        let mut c = crate::config::OffsiteConfig { s3_bucket: "b".into(), s3_access_key: "a".into(), s3_secret_key: "s".into(), ..Default::default() };
        c.s3_endpoint = "http://s3.example.com".into();
        assert!(S3::new(&c).is_err());
        c.s3_endpoint = "ftp://s3.example.com".into();
        assert!(S3::new(&c).is_err());
        for ok in ["https://s3.example.com", "s3.example.com", "http://127.0.0.1:9000", "http://localhost:9000"] {
            c.s3_endpoint = ok.into();
            assert!(S3::new(&c).is_ok(), "{ok}");
        }
    }

    #[test]
    fn targets_and_encoding() {
        let mut c = crate::config::OffsiteConfig { s3_bucket: "b".into(), s3_access_key: "a".into(), s3_secret_key: "s".into(), s3_prefix: "/home/x/".into(), ..Default::default() };
        let s = S3::new(&c).unwrap();
        assert_eq!(s.target(Some(&s.full_key("snapshots/1 2.enc"))), ("b.s3.us-east-1.amazonaws.com".to_string(), "/home/x/snapshots/1%202.enc".to_string()));
        assert_eq!(s.our_key("home/x/clips/a.mp4"), Some("clips/a.mp4"));
        assert_eq!(s.our_key("home/xy/clips/a.mp4"), None);
        c.s3_endpoint = "http://127.0.0.1:9000/".into();
        c.s3_path_style = true;
        let s = S3::new(&c).unwrap();
        assert_eq!(s.target(None), ("127.0.0.1:9000".to_string(), "/b".to_string()));
        c.s3_endpoint = "https://host:443".into();
        assert_eq!(S3::new(&c).unwrap().host, "host");
        assert_eq!(canonical_query(&[("prefix".into(), "a/b c".into()), ("list-type".into(), "2".into())]), "list-type=2&prefix=a%2Fb%20c");
        assert_eq!(error_text(403, b"<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"), "the storage said AccessDenied: Access Denied (HTTP 403)");
    }
}
