//! The byte pipe under a Baichuan session. The BC message stream (framing,
//! sign-in, crypto, media) is the same over both transports; only how the
//! bytes travel differs:
//!   * UDP ("BcUdp", `super::udp`): battery cameras. Discovery/wake first,
//!     then our own reliable delivery on top of datagrams.
//!   * TCP: wired and Wi-Fi cameras, hubs and recorders, on port 9000 by
//!     default. No discovery and no wake: connect and sign in.
//!
//! A read that finds nothing within the read timeout returns `WouldBlock` or
//! `TimedOut` (see `client::is_wouldblock`); `Ok(0)` means the camera closed
//! the connection.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub trait Transport: Read + Write + Send {
    fn set_read_timeout(&mut self, d: Duration);
    /// "UDP" or "TCP", for the log.
    fn kind(&self) -> &'static str;
}

impl Transport for super::udp::UdpTransport {
    fn set_read_timeout(&mut self, d: Duration) {
        super::udp::UdpTransport::set_read_timeout(self, d)
    }
    fn kind(&self) -> &'static str {
        "UDP"
    }
}

/// The default Baichuan TCP port.
pub const TCP_PORT: u16 = 9000;

/// A Baichuan session over one TCP connection.
pub struct TcpTransport {
    s: TcpStream,
}

impl TcpTransport {
    /// Connect to `host:port`, giving up after `timeout` per address.
    pub fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpTransport> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
        for addr in (host, port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(s) => {
                    s.set_nodelay(true).ok();
                    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
                    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
                    // A large receive buffer: a 4K keyframe arrives in one burst.
                    let _ = socket2::SockRef::from(&s).set_recv_buffer_size(4 << 20);
                    return Ok(TcpTransport { s });
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

impl Read for TcpTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.s.read(buf)
    }
}

impl Write for TcpTransport {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.s.write(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.s.flush()
    }
}

impl Transport for TcpTransport {
    fn set_read_timeout(&mut self, d: Duration) {
        // A zero timeout means "block for ever" to the OS: never ask for it.
        let _ = self.s.set_read_timeout(Some(d.max(Duration::from_millis(1))));
    }
    fn kind(&self) -> &'static str {
        "TCP"
    }
}

impl Drop for TcpTransport {
    fn drop(&mut self) {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
    }
}

/// An in-memory transport for tests: reads come from a script of chunks
/// (each read returns at most one chunk, so a message can be split anywhere;
/// an empty chunk is a pause), writes are kept.
#[cfg(test)]
pub struct ScriptTransport {
    pub chunks: std::collections::VecDeque<Vec<u8>>,
    pub written: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

#[cfg(test)]
impl ScriptTransport {
    pub fn new(chunks: Vec<Vec<u8>>) -> Self {
        ScriptTransport { chunks: chunks.into(), written: Default::default() }
    }
}

#[cfg(test)]
impl Read for ScriptTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.chunks.front_mut() {
            None => Err(io::Error::new(io::ErrorKind::WouldBlock, "nothing more")),
            Some(c) if c.is_empty() => {
                self.chunks.pop_front();
                Err(io::Error::new(io::ErrorKind::WouldBlock, "a pause"))
            }
            Some(c) => {
                let n = buf.len().min(c.len());
                buf[..n].copy_from_slice(&c[..n]);
                c.drain(..n);
                if c.is_empty() {
                    self.chunks.pop_front();
                }
                Ok(n)
            }
        }
    }
}

#[cfg(test)]
impl Write for ScriptTransport {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.written.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl Transport for ScriptTransport {
    fn set_read_timeout(&mut self, _d: Duration) {}
    fn kind(&self) -> &'static str {
        "test"
    }
}
