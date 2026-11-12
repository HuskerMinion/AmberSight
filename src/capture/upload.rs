//! Streams ffmpeg's FLV output to `PUT /push/<key>` with chunked transfer encoding.

use anyhow::{bail, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Copy `src` to the server until `src` ends (Ok(total bytes)) or the upload
/// fails. `on_data` is called with the running total after each chunk.
pub async fn push<R, S>(mut src: R, io: S, authority: &str, key: &str, mut on_data: impl FnMut(u64)) -> Result<u64>
where
    R: AsyncRead + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut rd, mut wr) = tokio::io::split(io);
    let head = format!(
        "PUT /push/{key} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: AmberSight-Capture/{}\r\nContent-Type: video/x-flv\r\nTransfer-Encoding: chunked\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    );
    wr.write_all(head.as_bytes()).await?;
    wr.flush().await?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut answer = Vec::new();
    let mut ans_buf = [0u8; 2048];
    let mut total = 0u64;
    loop {
        tokio::select! {
            n = src.read(&mut buf) => {
                let n = n?;
                if n == 0 {
                    break;
                }
                wr.write_all(format!("{n:x}\r\n").as_bytes()).await?;
                wr.write_all(&buf[..n]).await?;
                wr.write_all(b"\r\n").await?;
                wr.flush().await?;
                total += n as u64;
                on_data(total);
            }
            // The server only answers early when it refuses the stream.
            n = rd.read(&mut ans_buf) => {
                let n = n.unwrap_or(0);
                if n == 0 {
                    bail!("the server closed the upload");
                }
                answer.extend_from_slice(&ans_buf[..n]);
                if answer.windows(4).any(|w| w == b"\r\n\r\n") || answer.len() > 8192 {
                    bail!("{}", refusal(&answer));
                }
            }
        }
    }
    wr.write_all(b"0\r\n\r\n").await?;
    wr.flush().await?;
    // Wait briefly for the server's answer; the data is already sent.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while !answer.windows(4).any(|w| w == b"\r\n\r\n") {
            match rd.read(&mut ans_buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => answer.extend_from_slice(&ans_buf[..n]),
            }
        }
    })
    .await;
    Ok(total)
}

/// A readable reason from an early server answer.
fn refusal(answer: &[u8]) -> String {
    let text = String::from_utf8_lossy(answer);
    let status = text.lines().next().unwrap_or("").trim().to_string();
    let code = super::net::parse_status_line(&status).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.trim().to_string()).unwrap_or_default();
    let what = match code {
        404 => "the server does not know this camera's stream key",
        429 => "the server is refusing this address after too many wrong keys",
        _ => "the server refused the stream",
    };
    if body.is_empty() {
        format!("{what} ({status})")
    } else {
        format!("{what} ({status}: {})", body.chars().take(200).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Accept one connection; return its head and de-chunked body.
    async fn recording_server(answer_early: Option<&'static [u8]>) -> (String, tokio::task::JoinHandle<(String, Vec<u8>, Vec<usize>)>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let h = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut b = [0u8; 4096];
            let head_end = loop {
                if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i;
                }
                let n = s.read(&mut b).await.unwrap();
                raw.extend_from_slice(&b[..n]);
            };
            let head = String::from_utf8(raw[..head_end].to_vec()).unwrap();
            raw.drain(..head_end + 4);
            if let Some(a) = answer_early {
                s.write_all(a).await.unwrap();
                s.shutdown().await.unwrap();
                return (head, Vec::new(), Vec::new());
            }
            // Read the chunked body to the terminating chunk.
            let (mut body, mut sizes) = (Vec::new(), Vec::new());
            loop {
                let nl = loop {
                    if let Some(i) = raw.windows(2).position(|w| w == b"\r\n") {
                        break i;
                    }
                    let n = s.read(&mut b).await.unwrap();
                    assert!(n > 0, "connection closed mid-body");
                    raw.extend_from_slice(&b[..n]);
                };
                let size = usize::from_str_radix(std::str::from_utf8(&raw[..nl]).unwrap(), 16).unwrap();
                raw.drain(..nl + 2);
                while raw.len() < size + 2 {
                    let n = s.read(&mut b).await.unwrap();
                    assert!(n > 0);
                    raw.extend_from_slice(&b[..n]);
                }
                assert_eq!(&raw[size..size + 2], b"\r\n");
                if size == 0 {
                    break;
                }
                sizes.push(size);
                body.extend_from_slice(&raw[..size]);
                raw.drain(..size + 2);
            }
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\n\r\nstream ended").await.unwrap();
            (head, body, sizes)
        });
        (addr, h)
    }

    #[tokio::test]
    async fn streams_chunked_until_source_ends() {
        let (addr, srv) = recording_server(None).await;
        let io = tokio::net::TcpStream::connect(&addr).await.unwrap();
        // A source that yields several pieces with pauses.
        let (mut tx, rx) = tokio::io::duplex(1 << 20);
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let d2 = data.clone();
        tokio::spawn(async move {
            for part in d2.chunks(70_000) {
                tx.write_all(part).await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let mut seen = 0;
        let total = push(rx, io, "127.0.0.1:1234", "abcdefghijklmnop", |t| seen = t).await.unwrap();
        assert_eq!(total, data.len() as u64);
        assert_eq!(seen, total);
        let (head, body, sizes) = srv.await.unwrap();
        assert!(head.starts_with("PUT /push/abcdefghijklmnop HTTP/1.1\r\n"), "{head}");
        assert!(head.contains("\r\nHost: 127.0.0.1:1234"));
        assert!(head.contains("\r\nTransfer-Encoding: chunked"));
        assert!(!head.to_ascii_lowercase().contains("content-length"));
        assert_eq!(body, data);
        assert!(sizes.len() >= 2 && sizes.iter().all(|&s| s <= 64 * 1024));
    }

    #[tokio::test]
    async fn early_refusal_is_an_error() {
        let (addr, srv) = recording_server(Some(b"HTTP/1.1 404 Not Found\r\ncontent-length: 18\r\n\r\nunknown stream key")).await;
        let io = tokio::net::TcpStream::connect(&addr).await.unwrap();
        // A source that never ends: only the server's answer can stop the upload.
        let (_tx, rx) = tokio::io::duplex(1024);
        let e = push(rx, io, "h:1", "abcdefghijklmnop", |_| {}).await.unwrap_err().to_string();
        assert!(e.contains("does not know this camera's stream key") && e.contains("unknown stream key"), "{e}");
        srv.await.unwrap();
    }
}
