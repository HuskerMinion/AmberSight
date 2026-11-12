//! Tests only: a tiny S3 look-alike on 127.0.0.1 that checks every
//! request's signature and keeps objects in memory, to run the S3 client
//! and a whole backup + restore without real cloud storage.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use super::s3::{authorization, sha256_hex, SignInput};

pub const AK: &str = "TESTACCESSKEY";
pub const SK: &str = "test/secret/key";
pub const BUCKET: &str = "bkt";
/// Objects per list page (small, to exercise paging).
const PAGE: usize = 2;

#[derive(Default)]
pub struct State {
    pub objects: BTreeMap<String, Vec<u8>>,
    uploads: HashMap<String, BTreeMap<u32, Vec<u8>>>,
    pub multipart_done: u32,
    pub bad_signatures: u32,
    pub range_gets: u32,
}

pub fn start() -> (u16, Arc<Mutex<State>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let st = Arc::new(Mutex::new(State::default()));
    let st2 = st.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let st = st2.clone();
            std::thread::spawn(move || serve(c, st));
        }
    });
    (port, st)
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().to_string()
}

fn reply(w: &mut TcpStream, code: u16, headers: &[(&str, String)], body: &[u8]) {
    let mut h = format!("HTTP/1.1 {code} X\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    let _ = w.write_all(h.as_bytes());
    let _ = w.write_all(body);
}

fn serve(stream: TcpStream, st: Arc<Mutex<State>>) {
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let hv = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let len: usize = hv("content-length").parse().unwrap_or(0);
        let mut body = vec![0u8; len];
        if r.read_exact(&mut body).is_err() {
            return;
        }
        let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
        let query: Vec<(String, String)> = qs
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (decode(k), decode(v))
            })
            .collect();
        // Check the signature the way S3 does.
        let auth = hv("authorization");
        let signed: Vec<String> = auth.split("SignedHeaders=").nth(1).and_then(|s| s.split(',').next()).unwrap_or("").split(';').map(String::from).collect();
        let extra: Vec<(String, String)> = signed.iter().filter(|h| !matches!(h.as_str(), "host" | "x-amz-date" | "x-amz-content-sha256")).map(|h| (h.clone(), hv(h))).collect();
        let want = authorization(&SignInput {
            method: &method,
            host: &hv("host"),
            path,
            query: &query,
            headers: &extra,
            payload_hash: &hv("x-amz-content-sha256"),
            amz_date: &hv("x-amz-date"),
            region: "us-east-1",
            access_key: AK,
            secret_key: SK,
        });
        if want != auth || hv("x-amz-content-sha256") != sha256_hex(&body) {
            st.lock().unwrap().bad_signatures += 1;
            reply(&mut w, 403, &[], b"<Error><Code>SignatureDoesNotMatch</Code><Message>bad signature</Message></Error>");
            continue;
        }
        let q = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let Some(rest) = path.strip_prefix(&format!("/{BUCKET}")) else {
            reply(&mut w, 404, &[], b"<Error><Code>NoSuchBucket</Code></Error>");
            continue;
        };
        let key = decode(rest.trim_start_matches('/'));
        let mut s = st.lock().unwrap();
        match (method.as_str(), key.is_empty()) {
            ("GET", true) => {
                let prefix = q("prefix").unwrap_or_default();
                let start: usize = q("continuation-token").and_then(|t| t.parse().ok()).unwrap_or(0);
                let all: Vec<(&String, &Vec<u8>)> = s.objects.iter().filter(|(k, _)| k.starts_with(&prefix)).collect();
                let page = &all[start.min(all.len())..(start + PAGE).min(all.len())];
                let more = start + PAGE < all.len();
                let mut x = String::from("<?xml version=\"1.0\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
                for (k, v) in page {
                    x.push_str(&format!("<Contents><Key>{k}</Key><Size>{}</Size></Contents>", v.len()));
                }
                x.push_str(&format!("<IsTruncated>{more}</IsTruncated>"));
                if more {
                    x.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", start + PAGE));
                }
                x.push_str("</ListBucketResult>");
                reply(&mut w, 200, &[], x.as_bytes());
            }
            ("GET", false) => match s.objects.get(&key) {
                Some(v) => {
                    let v = v.clone();
                    // Only the "bytes=a-b" form the backup sends.
                    let range = hv("range");
                    if let Some((a, b)) = range.strip_prefix("bytes=").and_then(|r| r.split_once('-')) {
                        let (a, b): (usize, usize) = (a.parse().unwrap_or(0), b.parse().unwrap_or(0));
                        s.range_gets += 1;
                        if a >= v.len() {
                            reply(&mut w, 416, &[], b"<Error><Code>InvalidRange</Code></Error>");
                        } else {
                            reply(&mut w, 206, &[], &v[a..(b + 1).min(v.len())]);
                        }
                    } else {
                        reply(&mut w, 200, &[], &v);
                    }
                }
                None => reply(&mut w, 404, &[], b"<Error><Code>NoSuchKey</Code></Error>"),
            },
            ("PUT", false) => {
                if let (Some(n), Some(id)) = (q("partNumber"), q("uploadId")) {
                    let n: u32 = n.parse().unwrap();
                    match s.uploads.get_mut(&id) {
                        Some(u) => {
                            u.insert(n, body.clone());
                            reply(&mut w, 200, &[("ETag", format!("\"{}\"", &sha256_hex(&body)[..16]))], b"");
                        }
                        None => reply(&mut w, 404, &[], b"<Error><Code>NoSuchUpload</Code></Error>"),
                    }
                } else {
                    s.objects.insert(key, body);
                    reply(&mut w, 200, &[("ETag", "\"x\"".into())], b"");
                }
            }
            ("POST", false) if q("uploads").is_some() => {
                let id = format!("up{}", s.uploads.len() + 1);
                s.uploads.insert(id.clone(), BTreeMap::new());
                reply(&mut w, 200, &[], format!("<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>").as_bytes());
            }
            ("POST", false) if q("uploadId").is_some() => {
                let id = q("uploadId").unwrap();
                let parts = s.uploads.remove(&id).unwrap_or_default();
                let xml = String::from_utf8_lossy(&body).to_string();
                // Every part listed, in order, with the ETag it was given.
                let ok = parts.iter().all(|(n, v)| xml.contains(&format!("<PartNumber>{n}</PartNumber><ETag>&quot;{}&quot;</ETag>", &sha256_hex(v)[..16])));
                if ok && !parts.is_empty() {
                    let all: Vec<u8> = parts.into_values().flatten().collect();
                    s.objects.insert(key, all);
                    s.multipart_done += 1;
                    reply(&mut w, 200, &[], b"<CompleteMultipartUploadResult/>");
                } else {
                    reply(&mut w, 400, &[], b"<Error><Code>InvalidPart</Code></Error>");
                }
            }
            ("DELETE", false) => {
                if let Some(id) = q("uploadId") {
                    s.uploads.remove(&id);
                } else {
                    s.objects.remove(&key);
                }
                reply(&mut w, 204, &[], b"");
            }
            _ => reply(&mut w, 400, &[], b"<Error><Code>BadRequest</Code></Error>"),
        }
    }
}
