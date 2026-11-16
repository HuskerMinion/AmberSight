//! Test helper: a local TCP port that refuses connections for as long as the
//! test holds it.
//!
//! Binding port 0, reading the port and freeing it leaves a window in which
//! another test running in parallel can take the port, and the "closed" port
//! then answers (seen on Windows, where a refused connect is retried for about
//! 2 s). A socket that is bound but never listens keeps the port taken and
//! still refuses every connection.

pub struct ClosedPort {
    _sock: socket2::Socket,
    pub port: u16,
}

/// A port on 127.0.0.1 that nothing listens on, kept so while the result lives.
pub fn closed_port() -> ClosedPort {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).expect("socket");
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    sock.bind(&addr.into()).expect("bind");
    let port = sock.local_addr().expect("local addr").as_socket().expect("inet").port();
    ClosedPort { _sock: sock, port }
}

/// Reads one whole HTTP request (head and a Content-Length body) for a fake
/// server: (head, body). A server that closes the socket with part of the
/// request unread makes Windows send a reset, and the client then reads
/// "connection forcibly closed" instead of the answer. Empty head when the
/// client sent nothing.
pub fn read_http_request(s: &mut impl std::io::Read) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return (String::from_utf8_lossy(&buf).into_owned(), Vec::new()),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let len = head
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length")).map(|(_, v)| v.trim().to_string()))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < head_end + len {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let body = buf[head_end..].to_vec();
    (head, body)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_whole_request_is_read() {
        let req = b"POST /api.cgi?cmd=GetTime HTTP/1.1\r\nHost: x\r\ncontent-length: 11\r\n\r\n[{\"a\": 1}]x";
        let (head, body) = super::read_http_request(&mut &req[..]);
        assert!(head.starts_with("POST /api.cgi?cmd=GetTime"));
        assert_eq!(body, b"[{\"a\": 1}]x");
        let (head, body) = super::read_http_request(&mut &b"GET / HTTP/1.1\r\n\r\n"[..]);
        assert_eq!((head.as_str(), body.len()), ("GET / HTTP/1.1\r\n\r\n", 0));
    }

    #[test]
    fn the_port_refuses_and_stays_taken() {
        let c = super::closed_port();
        let to = std::time::Duration::from_secs(3);
        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", c.port).parse().unwrap();
        assert!(std::net::TcpStream::connect_timeout(&addr, to).is_err(), "refused");
        assert!(std::net::TcpListener::bind(addr).is_err(), "nobody else can take it");
    }
}
