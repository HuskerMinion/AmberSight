//! The small HTTP/1.1 HomeKit speaks: requests with a Content-Length body
//! (no chunked bodies), responses, and EVENT/1.0 notifications. Its own
//! parser because the same connection becomes encrypted after pair-verify.

/// Largest request head (request line and headers).
pub const MAX_HEAD: usize = 8 * 1024;
/// Most header lines.
pub const MAX_HEADERS: usize = 32;
/// Largest request body.
pub const MAX_BODY: usize = 64 * 1024;

#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    /// The path without the query.
    pub path: String,
    pub query: String,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    #[cfg(test)]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq)]
pub enum Parse {
    /// More bytes are needed.
    Incomplete,
    /// A request and how many bytes of the buffer it took.
    Done(Request, usize),
    /// A request this server will not take: answer with this status and close.
    Bad(u16),
}

fn find_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse one request from the start of `buf`.
pub fn parse(buf: &[u8]) -> Parse {
    let Some(end) = find_end(&buf[..buf.len().min(MAX_HEAD + 4)]) else {
        return if buf.len() > MAX_HEAD { Parse::Bad(431) } else { Parse::Incomplete };
    };
    let Ok(head) = std::str::from_utf8(&buf[..end]) else { return Parse::Bad(400) };
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or("");
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Parse::Bad(400);
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Parse::Bad(505);
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) || !target.starts_with('/') {
        return Parse::Bad(400);
    }
    let mut headers = Vec::new();
    for l in lines {
        if headers.len() >= MAX_HEADERS {
            return Parse::Bad(431);
        }
        let Some((k, v)) = l.split_once(':') else { return Parse::Bad(400) };
        if k.is_empty() || k.contains(' ') {
            return Parse::Bad(400);
        }
        headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
    }
    if headers.iter().any(|(k, _)| k == "transfer-encoding") {
        return Parse::Bad(501);
    }
    let lengths: Vec<&str> = headers.iter().filter(|(k, _)| k == "content-length").map(|(_, v)| v.as_str()).collect();
    let len = match lengths.as_slice() {
        [] => 0,
        [one] => match one.parse::<usize>() {
            Ok(n) if one.bytes().all(|b| b.is_ascii_digit()) => n,
            _ => return Parse::Bad(400),
        },
        _ => return Parse::Bad(400),
    };
    if len > MAX_BODY {
        return Parse::Bad(413);
    }
    let start = end + 4;
    if buf.len() - start < len {
        return Parse::Incomplete;
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    Parse::Done(Request { method: method.to_string(), path, query, headers, body: buf[start..start + len].to_vec() }, start + len)
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        207 => "Multi-Status",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        470 => "Connection Authorization Required",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

/// A response with an optional body of this type.
pub fn response(status: u16, content_type: Option<&str>, body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    if let Some(t) = content_type {
        out.push_str(&format!("Content-Type: {t}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

/// An EVENT/1.0 notification with a HAP JSON body.
pub fn event(body: &[u8]) -> Vec<u8> {
    let mut out = format!("EVENT/1.0 200 OK\r\nContent-Type: application/hap+json\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Query string parameters (no percent-decoding: HAP's are plain).
pub fn query_pairs(q: &str) -> Vec<(&str, &str)> {
    q.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_request_with_body_and_leaves_the_rest() {
        let raw = b"POST /pair-setup HTTP/1.1\r\nHost: x\r\nContent-Type: application/pairing+tlv8\r\nContent-Length: 3\r\n\r\n\x06\x01\x01GET /accessories HTTP/1.1\r\n\r\n";
        let Parse::Done(r, used) = parse(raw) else { panic!() };
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/pair-setup");
        assert_eq!(r.body, vec![6, 1, 1]);
        assert_eq!(r.header("content-type"), Some("application/pairing+tlv8"));
        let Parse::Done(r2, used2) = parse(&raw[used..]) else { panic!() };
        assert_eq!(r2.path, "/accessories");
        assert_eq!(used + used2, raw.len());
    }

    #[test]
    fn query_is_split_off() {
        let Parse::Done(r, _) = parse(b"GET /characteristics?id=1.2,2.9&ev=1 HTTP/1.1\r\n\r\n") else { panic!() };
        assert_eq!(r.path, "/characteristics");
        assert_eq!(query_pairs(&r.query), vec![("id", "1.2,2.9"), ("ev", "1")]);
    }

    #[test]
    fn incomplete_until_the_body_is_there() {
        assert_eq!(parse(b"GET / HTTP/1.1\r\n"), Parse::Incomplete);
        assert_eq!(parse(b"PUT /x HTTP/1.1\r\nContent-Length: 5\r\n\r\nab"), Parse::Incomplete);
    }

    #[test]
    fn limits_and_bad_requests() {
        let big_head = format!("GET / HTTP/1.1\r\nX: {}\r\n", "a".repeat(MAX_HEAD));
        assert_eq!(parse(big_head.as_bytes()), Parse::Bad(431));
        let many: String = (0..MAX_HEADERS + 1).map(|i| format!("H{i}: v\r\n")).collect();
        assert_eq!(parse(format!("GET / HTTP/1.1\r\n{many}\r\n").as_bytes()), Parse::Bad(431));
        assert_eq!(parse(format!("PUT / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1).as_bytes()), Parse::Bad(413));
        assert_eq!(parse(b"PUT / HTTP/1.1\r\nContent-Length: -1\r\n\r\n"), Parse::Bad(400));
        assert_eq!(parse(b"PUT / HTTP/1.1\r\nContent-Length: +1\r\n\r\nx"), Parse::Bad(400));
        assert_eq!(parse(b"PUT / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nxx"), Parse::Bad(400));
        assert_eq!(parse(b"PUT / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"), Parse::Bad(501));
        assert_eq!(parse(b"GET / HTTP/2.0\r\n\r\n"), Parse::Bad(505));
        assert_eq!(parse(b"GET  / HTTP/1.1\r\n\r\n"), Parse::Bad(400));
        assert_eq!(parse(b"get / HTTP/1.1\r\n\r\n"), Parse::Bad(400));
        assert_eq!(parse(b"GET x HTTP/1.1\r\n\r\n"), Parse::Bad(400));
        assert_eq!(parse(b"GET / HTTP/1.1\r\nNoColon\r\n\r\n"), Parse::Bad(400));
        assert_eq!(parse(b"GET / HTTP/1.1\r\n\xff: x\r\n\r\n"), Parse::Bad(400));
    }

    #[test]
    fn random_bytes_never_panic() {
        use rand::{Rng, SeedableRng};
        let mut r = rand::rngs::StdRng::seed_from_u64(11);
        for _ in 0..20_000 {
            let n = r.gen_range(0..300);
            let mut buf: Vec<u8> = (0..n).map(|_| r.gen()).collect();
            if r.gen_bool(0.5) {
                let mut head = b"PUT /characteristics HTTP/1.1\r\nContent-Length: 4\r\n\r\n".to_vec();
                head.append(&mut buf);
                buf = head;
            }
            let _ = parse(&buf);
        }
    }

    #[test]
    fn responses_and_events() {
        let r = response(204, None, b"");
        assert_eq!(r, b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        let e = event(b"{}");
        assert!(e.starts_with(b"EVENT/1.0 200 OK\r\n"));
        assert!(e.ends_with(b"\r\n\r\n{}"));
    }
}
