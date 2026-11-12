// Copyright (C) The Retina Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! tokio-based [`Connection`].
//!
//! In theory there could be a similar async-std-based implementation.

use futures::{Sink, Stream};
use std::time::Instant;
use tokio::io::AsyncWrite;
use tokio::net::{TcpStream, UdpSocket};
use url::Host;

use crate::buf::MarkBuf;
use crate::inputs::{Input as _, Split};
use crate::rtsp::msg::OwnedMessage;
use crate::{Error, ErrorInt, RtspMessageContext};

use super::{ConnectionContext, ReceivedMessage, WallTime};

/// AmberSight patch: socket receive buffer for RTSP connections (bytes; 0 =
/// the system's default). Set before connecting so the TCP window can open
/// that far: a camera that must wait while the reader is busy may cut
/// packets short, so a large buffer rides out short pauses.
static RECV_BUFFER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Sets the receive buffer for RTSP connections made from now on.
pub fn set_tcp_recv_buffer(bytes: u32) {
    RECV_BUFFER.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

async fn connect_with_recv_buffer(a: std::net::SocketAddr) -> Result<TcpStream, std::io::Error> {
    let size = RECV_BUFFER.load(std::sync::atomic::Ordering::Relaxed);
    if size == 0 {
        return TcpStream::connect(a).await;
    }
    let sock = if a.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
    if let Err(e) = sock.set_recv_buffer_size(size) {
        log::debug!("could not set the receive buffer to {size}: {e}");
    }
    sock.connect(a).await
}

/// Default initial capacity for the read ring buffer (64 KiB).
const DEFAULT_READ_CAPACITY: usize = 64 * 1024;

/// A RTSP connection which implements `Stream`, `Sink`, and `Unpin`.
pub(crate) struct Connection {
    stream: TcpStream,
    ctx: ConnectionContext,
    parser: crate::rtsp::parse::Parser,
    read_buf: MarkBuf,
    /// Write buffer for outgoing messages (written then flushed).
    write_buf: Vec<u8>,
    /// AmberSight patch: recovery in progress after a framing error.
    resync: Option<Resync>,
    /// AmberSight patch: when recent recoveries happened (rate limit).
    resyncs: std::collections::VecDeque<Instant>,
    recent: RecentHeaders,
    /// AmberSight patch: a recovery happened since `take_resynced` last ran.
    resynced: bool,
}

impl Connection {
    pub(crate) async fn connect(host: Host<&str>, port: u16) -> Result<Self, std::io::Error> {
        let addrs: Vec<std::net::SocketAddr> = match host {
            Host::Domain(h) => tokio::net::lookup_host((h, port)).await?.collect(),
            Host::Ipv4(h) => vec![(h, port).into()],
            Host::Ipv6(h) => vec![(h, port).into()],
        };
        let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses");
        for a in addrs {
            match connect_with_recv_buffer(a).await {
                Ok(stream) => return Self::from_stream(stream),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    pub(crate) fn from_stream(stream: TcpStream) -> Result<Self, std::io::Error> {
        let established_wall = WallTime::now();
        let local_addr = stream.local_addr()?;
        let peer_addr = stream.peer_addr()?;
        Ok(Self {
            stream,
            ctx: ConnectionContext {
                local_addr,
                peer_addr,
                established_wall,
            },
            parser: crate::rtsp::parse::Parser::default(),
            read_buf: MarkBuf::new(DEFAULT_READ_CAPACITY),
            write_buf: Vec::new(),
            resync: None,
            resyncs: std::collections::VecDeque::new(),
            recent: RecentHeaders::default(),
            resynced: false,
        })
    }

    pub(crate) fn ctx(&self) -> &ConnectionContext {
        &self.ctx
    }

    pub(crate) fn read_buf(&self) -> &MarkBuf {
        &self.read_buf
    }

    /// Reads body data from the ring buffer as `Bytes`.
    ///
    /// This copies the data out. Use for cold paths (RTSP responses) or the
    /// public `PacketItem` API. The `Demuxed` path reads directly from the
    /// ring buffer to avoid this copy.
    pub(crate) fn body_bytes(&self, body_pos: u64, body_len: u32) -> bytes::Bytes {
        let (s1, s2) = self.read_buf.split(body_pos, body_len as usize).slices();
        let mut buf = Vec::with_capacity(body_len as usize);
        buf.extend_from_slice(s1);
        buf.extend_from_slice(s2);
        bytes::Bytes::from(buf)
    }

    pub(crate) fn eof_ctx(&self) -> RtspMessageContext {
        RtspMessageContext {
            pos: self.parser.stream_pos() + self.read_buf.unparsed_len() as u64,
            received_wall: WallTime::now(),
            received: Instant::now(),
        }
    }
}

impl Connection {
    /// AmberSight patch: queues an interleaved data message behind the message
    /// just given to `start_send` (sent by the same flush).
    pub(crate) fn append_data(&mut self, channel_id: u8, body: &[u8]) {
        let len = body.len() as u16;
        self.write_buf.extend_from_slice(&[b'$', channel_id, (len >> 8) as u8, len as u8]);
        self.write_buf.extend_from_slice(body);
    }

    /// AmberSight patch: whether a recovery skipped bytes since the last call
    /// (it may have skipped the answer to a keepalive).
    pub(crate) fn take_resynced(&mut self) -> bool {
        std::mem::take(&mut self.resynced)
    }

    /// AmberSight patch: whether another recovery is allowed now (at most
    /// RESYNC_MAX_PER_MIN a minute; more means the stream is just broken).
    fn may_resync(&mut self) -> bool {
        let now = Instant::now();
        while self.resyncs.front().is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(60)) {
            self.resyncs.pop_front();
        }
        if self.resyncs.len() >= RESYNC_MAX_PER_MIN {
            return false;
        }
        self.resyncs.push_back(now);
        true
    }
}

/// Tries to decode a message from the read buffer without I/O.
///
/// Separated from `Connection` methods to avoid self-borrow issues.
///
/// When `eof` is true, the TCP stream has closed. `Incomplete` and empty
/// `Ok(None)` with leftover data are promoted to errors.
fn try_decode(
    parser: &mut crate::rtsp::parse::Parser,
    read_buf: &mut MarkBuf,
    eof: bool,
) -> Result<Option<ReceivedMessage>, CodecError> {
    use crate::rtsp::parse::FeedError;

    let pos = parser.stream_pos();
    let unparsed = read_buf.unparsed();
    let mut input = {
        let split = read_buf.unparsed_split();
        let (first, second) = split.slices();
        Split::new(first, second)
    };
    let initial_len = input.len();

    match parser.feed(&mut input) {
        Ok(None) => Ok(None), // idle, no data; caller distinguishes EOF from "need more"
        Ok(Some((msg, body_slice))) => {
            let consumed = initial_len - input.len();
            let body_len = body_slice.len();
            let body_pos = unparsed + (consumed - body_len) as u64;
            read_buf.advance_unparsed(unparsed + consumed as u64);
            Ok(Some(ReceivedMessage {
                msg,
                body_pos,
                body_len: body_len as u32,
                ctx: RtspMessageContext {
                    pos,
                    received_wall: WallTime::now(),
                    received: Instant::now(),
                },
            }))
        }
        Err(FeedError::Incomplete(_)) if !eof => {
            // On Incomplete, the parser may have stably consumed some bytes
            // (e.g. header lines). Advance past them.
            let consumed = initial_len - input.len();
            if consumed > 0 {
                read_buf.advance_unparsed(unparsed + consumed as u64);
            }
            Ok(None)
        }
        Err(FeedError::Incomplete(inc)) => {
            // EOF with incomplete data: the message is truncated.
            Err(CodecError::ParseError {
                description: format!(
                    "Incomplete RTSP message at EOF ({inc}); buffered:\n{:#?}",
                    crate::hex::LimitedHex::from_split(read_buf.unparsed_split(), 128),
                ),
                pos,
            })
        }
        Err(FeedError::Invalid(inv)) => Err(CodecError::ParseError {
            description: format!(
                "Invalid RTSP message: {inv}; buffered:\n{:#?}",
                crate::hex::LimitedHex::from_split(read_buf.unparsed_split(), 128),
            ),
            pos: inv.pos,
        }),
    }
}

// --- AmberSight patch: resync after a framing error -----------------------
//
// Some cameras (seen with Reolink over TCP) occasionally put the stream out
// of step: where a message should start there is video data. Upstream treats
// that as fatal and the session ends (a 2-8 s gap while it reconnects).
// Instead, skip forward to the next place that is certainly a message start
// (an interleaved header whose packet is followed by another header, or an
// RTSP response) and continue. Lost packets show up as an RTP sequence gap;
// the picture recovers at the next keyframe. Gives up (the original error)
// after RESYNC_MAX_BYTES or RESYNC_MAX_TIME without finding a start, or
// after RESYNC_MAX_PER_MIN recoveries in a minute.

const RESYNC_MAX_BYTES: u64 = 4 << 20;
const RESYNC_MAX_TIME: std::time::Duration = std::time::Duration::from_secs(5);
const RESYNC_MAX_PER_MIN: usize = 30;
/// Interleaved channel ids above this are taken as noise (clients assign
/// 0, 1, 2, ... two per set-up stream).
const RESYNC_MAX_CHANNEL: u8 = 31;

struct Resync {
    started: Instant,
    /// Ring position of the framing error.
    from_pos: u64,
    skipped: u64,
    /// Not yet looked past the failed byte.
    first: bool,
    description: String,
    error_pos: u64,
    recent: String,
}

enum SyncScan {
    /// A certain message start this many bytes into the unparsed data.
    Found(usize),
    /// None yet; this many bytes can be dropped for sure.
    NeedMore(usize),
}

/// Looks for a certain message start in `input` from offset `from` (1 on the
/// first look: the parser just failed at 0).
fn scan_for_sync<'i, I: crate::inputs::Input<'i>>(input: &I, from: usize) -> SyncScan {
    let n = input.len();
    let rtp_v2 = |b: u8| b >> 6 == 2;
    // A plausible interleaved header at `i`: Some(end of its packet).
    let header_at = |i: usize| -> Option<Option<usize>> {
        if i + 5 > n {
            return None; // undecided
        }
        let ch = input.byte_at(i + 1);
        let len = (input.byte_at(i + 2) as usize) << 8 | input.byte_at(i + 3) as usize;
        if ch > RESYNC_MAX_CHANNEL || len < 4 || !rtp_v2(input.byte_at(i + 4)) {
            return Some(None);
        }
        Some(Some(i + 4 + len))
    };
    let mut i = from;
    while i < n {
        match input.byte_at(i) {
            b'$' => match header_at(i) {
                None => return SyncScan::NeedMore(i),
                Some(None) => {}
                Some(Some(next)) => {
                    // Confirm: another message must start right after it.
                    if next + 5 > n {
                        return SyncScan::NeedMore(i);
                    }
                    let follows = match input.byte_at(next) {
                        b'$' => matches!(header_at(next), Some(Some(_))),
                        b'R' => next + 5 <= n && (0..5).all(|k| input.byte_at(next + k) == b"RTSP/"[k]),
                        _ => false,
                    };
                    if follows {
                        return SyncScan::Found(i);
                    }
                }
            },
            b'R' => {
                if i + 9 > n {
                    return SyncScan::NeedMore(i);
                }
                if (0..9).all(|k| input.byte_at(i + k) == b"RTSP/1.0 "[k]) {
                    return SyncScan::Found(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    // Nothing here: drop all but the last bytes (a header may begin there).
    SyncScan::NeedMore(n.saturating_sub(4).max(from.min(n)))
}

/// The last few interleaved headers, for the recovery log line.
#[derive(Default)]
struct RecentHeaders {
    h: [(u64, u8, u16); 4],
    next: usize,
    count: usize,
}

impl RecentHeaders {
    fn push(&mut self, pos: u64, ch: u8, len: u16) {
        self.h[self.next] = (pos, ch, len);
        self.next = (self.next + 1) % self.h.len();
        self.count += 1;
    }
    fn describe(&self, error_pos: u64) -> String {
        let k = self.count.min(self.h.len());
        let mut out = Vec::with_capacity(k);
        for j in 0..k {
            let (pos, ch, len) = self.h[(self.next + self.h.len() - k + j) % self.h.len()];
            out.push(format!("ch{ch} len {len} at -{}", error_pos.saturating_sub(pos)));
        }
        out.join(", ")
    }
}
// --------------------------------------------------------------------------

/// An intermediate error type used internally.
#[derive(Debug)]
#[allow(dead_code)] // IoError reserved for future use
enum CodecError {
    IoError(std::io::Error),
    ParseError { description: String, pos: u64 },
}

impl Stream for Connection {
    type Item = Result<ReceivedMessage, Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            // AmberSight patch: looking for the next message start.
            if let Some(r) = this.resync.as_mut() {
                let unparsed = this.read_buf.unparsed();
                let scan = {
                    let split = this.read_buf.unparsed_split();
                    let (first, second) = split.slices();
                    scan_for_sync(&Split::new(first, second), usize::from(r.first))
                };
                r.first = false;
                match scan {
                    SyncScan::Found(off) => {
                        let at = unparsed + off as u64;
                        this.read_buf.advance_unparsed(at);
                        this.parser.resync_to(at);
                        let r = this.resync.take().unwrap();
                        this.resynced = true;
                        log::warn!(
                            "RTSP stream out of step from {}: skipped {} bytes to the next message and continued \
                             (last headers: {}; error: {})",
                            this.ctx.peer_addr,
                            at - r.from_pos,
                            r.recent,
                            r.description.lines().next().unwrap_or(""),
                        );
                    }
                    SyncScan::NeedMore(skip) => {
                        if skip > 0 {
                            this.read_buf.advance_unparsed(unparsed + skip as u64);
                            r.skipped += skip as u64;
                        }
                        if r.skipped > RESYNC_MAX_BYTES || r.started.elapsed() > RESYNC_MAX_TIME {
                            let r = this.resync.take().unwrap();
                            return std::task::Poll::Ready(Some(Err(codec_err_to_error(
                                &this.ctx,
                                &this.parser,
                                &this.read_buf,
                                CodecError::ParseError {
                                    description: format!(
                                        "{} (no message start found in the next {} bytes)",
                                        r.description, r.skipped
                                    ),
                                    pos: r.error_pos,
                                },
                            ))));
                        }
                    }
                }
            }
            if this.resync.is_none() {
                // Try decoding from existing buffered data.
                match try_decode(&mut this.parser, &mut this.read_buf, false) {
                    Ok(Some(msg)) => {
                        if let crate::rtsp::msg::Message::Data(d) = &msg.msg {
                            this.recent.push(msg.body_pos.saturating_sub(4), d.channel_id, d.body_len);
                        }
                        return std::task::Poll::Ready(Some(Ok(msg)));
                    }
                    Ok(None) => {}
                    Err(CodecError::ParseError { description, pos }) if this.may_resync() => {
                        let from_pos = this.read_buf.unparsed();
                        this.resync = Some(Resync {
                            started: Instant::now(),
                            from_pos,
                            skipped: 0,
                            first: true,
                            recent: this.recent.describe(from_pos),
                            description,
                            error_pos: pos,
                        });
                        continue;
                    }
                    Err(e) => {
                        return std::task::Poll::Ready(Some(Err(codec_err_to_error(
                            &this.ctx,
                            &this.parser,
                            &this.read_buf,
                            e,
                        ))));
                    }
                }
            }

            // Need more data. Read from the stream into the ring buffer
            // using vectored I/O to fill both halves of the ring.
            match this.stream.poll_read_ready(cx) {
                std::task::Poll::Ready(Ok(())) => {}
                std::task::Poll::Ready(Err(error)) => {
                    let pos = this.parser.stream_pos() + this.read_buf.unparsed_len() as u64;
                    return std::task::Poll::Ready(Some(Err(wrap!(ErrorInt::RtspReadError {
                        conn_ctx: this.ctx,
                        msg_ctx: RtspMessageContext {
                            pos,
                            received_wall: WallTime::now(),
                            received: Instant::now(),
                        },
                        source: error,
                    }))));
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
            let (first, second) = this.read_buf.spare_capacity(4096);
            let mut bufs = [
                std::io::IoSliceMut::new(first),
                std::io::IoSliceMut::new(second),
            ];
            match this.stream.try_read_vectored(&mut bufs) {
                Ok(0) => {
                    // EOF. Try decode with eof=true.
                    return match try_decode(&mut this.parser, &mut this.read_buf, true) {
                        Ok(Some(msg)) => std::task::Poll::Ready(Some(Ok(msg))),
                        Ok(None) => std::task::Poll::Ready(None),
                        Err(e) => std::task::Poll::Ready(Some(Err(codec_err_to_error(
                            &this.ctx,
                            &this.parser,
                            &this.read_buf,
                            e,
                        )))),
                    };
                }
                Ok(n) => {
                    this.read_buf.advance_end(n);
                    // Loop to try decoding again.
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    // Spurious readiness; re-register and wait.
                    continue;
                }
                Err(error) => {
                    let pos = this.parser.stream_pos() + this.read_buf.unparsed_len() as u64;
                    return std::task::Poll::Ready(Some(Err(wrap!(ErrorInt::RtspReadError {
                        conn_ctx: this.ctx,
                        msg_ctx: RtspMessageContext {
                            pos,
                            received_wall: WallTime::now(),
                            received: Instant::now(),
                        },
                        source: error,
                    }))));
                }
            }
        }
    }
}

fn codec_err_to_error(
    ctx: &ConnectionContext,
    parser: &crate::rtsp::parse::Parser,
    read_buf: &MarkBuf,
    e: CodecError,
) -> Error {
    wrap!(match e {
        CodecError::IoError(error) => ErrorInt::RtspReadError {
            conn_ctx: *ctx,
            msg_ctx: RtspMessageContext {
                pos: parser.stream_pos() + read_buf.unparsed_len() as u64,
                received_wall: WallTime::now(),
                received: Instant::now(),
            },
            source: error,
        },
        CodecError::ParseError { description, pos } => ErrorInt::RtspFramingError {
            conn_ctx: *ctx,
            msg_ctx: RtspMessageContext {
                pos,
                received_wall: WallTime::now(),
                received: Instant::now(),
            },
            description,
        },
    })
}

impl Sink<OwnedMessage> for Connection {
    type Error = ErrorInt;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        item: OwnedMessage,
    ) -> Result<(), Self::Error> {
        self.write_buf.clear();
        item.write(&mut self.write_buf)
            .expect("Vec Writer is infallible");
        Ok(())
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        let this = &mut *self;
        while !this.write_buf.is_empty() {
            match std::pin::Pin::new(&mut this.stream).poll_write(cx, &this.write_buf) {
                std::task::Poll::Ready(Ok(n)) => {
                    this.write_buf.drain(..n);
                }
                std::task::Poll::Ready(Err(e)) => {
                    return std::task::Poll::Ready(Err(ErrorInt::WriteError {
                        conn_ctx: this.ctx,
                        source: e,
                    }));
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
        match std::pin::Pin::new(&mut this.stream).poll_flush(cx) {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(())),
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(ErrorInt::WriteError {
                conn_ctx: this.ctx,
                source: e,
            })),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.as_mut().poll_flush(cx) {
            std::task::Poll::Ready(Ok(())) => {}
            other => return other,
        }
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.stream).poll_shutdown(cx) {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(())),
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(ErrorInt::WriteError {
                conn_ctx: this.ctx,
                source: e,
            })),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// tokio-specific version of [`crate::UdpPair`].
pub(crate) struct UdpPair {
    pub(crate) rtp_port: u16,
    pub(crate) rtp_socket: UdpSocket,
    pub(crate) rtcp_socket: UdpSocket,
}

impl UdpPair {
    pub(crate) fn for_ip(ip_addr: std::net::IpAddr) -> Result<Self, std::io::Error> {
        let inner = crate::UdpPair::for_ip(ip_addr)?;
        inner.rtp_socket.set_nonblocking(true)?;
        inner.rtcp_socket.set_nonblocking(true)?;
        Ok(Self {
            rtp_port: inner.rtp_port,
            rtp_socket: UdpSocket::from_std(inner.rtp_socket)?,
            rtcp_socket: UdpSocket::from_std(inner.rtcp_socket)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_data() {
        let mut read_buf = MarkBuf::new(64);
        let data = b"\r\n$\x00\x00\x04asdfrest";
        let (first, _) = read_buf.spare_capacity(data.len());
        first[..data.len()].copy_from_slice(data);
        read_buf.advance_end(data.len());

        let mut parser = crate::rtsp::parse::Parser::default();
        let msg = try_decode(&mut parser, &mut read_buf, false)
            .unwrap()
            .unwrap();
        // Read body from ring buffer.
        let (s1, s2) = read_buf.split(msg.body_pos, msg.body_len as usize).slices();
        assert_eq!(s1, b"asdf");
        assert!(s2.is_empty());
        assert_eq!(read_buf.unparsed_len(), 4); // "rest" remains
    }

    fn scan(data: &[u8], from: usize) -> SyncScan {
        scan_for_sync(&Split::new(data, &[]), from)
    }

    fn pkt(ch: u8, body_len: usize) -> Vec<u8> {
        let mut v = vec![b'$', ch, (body_len >> 8) as u8, body_len as u8, 0x80];
        v.resize(4 + body_len, 0x11);
        v
    }

    #[test]
    fn resync_finds_a_confirmed_header() {
        // Garbage (with a fake '$' whose packet is not followed by a header),
        // then two real packets.
        let mut d = vec![0xc8, 0xdc, b'$', 0x00, 0x00, 0x08, 0x80, 1, 2, 3, 4, 5, 6, 7, 9, 9, 9];
        let real = d.len();
        d.extend(pkt(0, 20));
        d.extend(pkt(1, 8));
        assert!(matches!(scan(&d, 1), SyncScan::Found(i) if i == real));
        // An RTSP response also counts.
        let mut d = vec![0x55; 10];
        d.extend(b"RTSP/1.0 200 OK\r\n");
        assert!(matches!(scan(&d, 1), SyncScan::Found(10)));
    }

    #[test]
    fn resync_waits_for_confirmation_and_keeps_the_candidate() {
        // A header whose packet end is not buffered yet: wait, keep it.
        let mut d = vec![0x01, 0x02, 0x03];
        d.extend(&pkt(0, 100)[..30]);
        assert!(matches!(scan(&d, 1), SyncScan::NeedMore(3)));
        // Next look starts at the kept candidate (offset 0) and finds it.
        let mut d = pkt(0, 100);
        d.extend(pkt(1, 12));
        assert!(matches!(scan(&d, 0), SyncScan::Found(0)));
        // Only noise: everything but the last 4 bytes can go.
        assert!(matches!(scan(&[0x11; 50], 1), SyncScan::NeedMore(46)));
        // Wrong channel / version / tiny length are not headers.
        let d = [0u8, b'$', 200, 0, 8, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(scan(&d, 1), SyncScan::NeedMore(12)));
        let d = [0u8, b'$', 0, 0, 8, 0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(scan(&d, 1), SyncScan::NeedMore(12)));
    }

    /// End to end through a real socket: a stream that goes out of step
    /// recovers and delivers the packets after the damage.
    #[tokio::test]
    async fn connection_recovers_from_garbage() {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut d = pkt(0, 40);
            // Declared 40, sends 47: the next "message" starts in data.
            d.extend([0x7c, 0x16, 0x9d, 0xb6, 0x34, 0x8c, 0x82]);
            for _ in 0..5 {
                d.extend(pkt(0, 1000));
            }
            for chunk in d.chunks(333) {
                s.write_all(chunk).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        });
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut c = Connection::from_stream(stream).unwrap();
        let mut lens = Vec::new();
        while let Some(m) = c.next().await {
            let m = m.unwrap();
            lens.push(m.body_len);
        }
        server.await.unwrap();
        assert_eq!(lens, vec![40, 1000, 1000, 1000, 1000, 1000]);
    }

    #[test]
    fn response_decode() {
        let mut read_buf = MarkBuf::new(64);
        let data = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n";
        let (first, _) = read_buf.spare_capacity(data.len());
        first[..data.len()].copy_from_slice(data);
        read_buf.advance_end(data.len());

        let mut parser = crate::rtsp::parse::Parser::default();
        let msg = try_decode(&mut parser, &mut read_buf, false)
            .unwrap()
            .unwrap();
        assert!(matches!(msg.msg, crate::rtsp::msg::Message::Response(_)));
        assert_eq!(msg.body_len, 0);
        assert_eq!(read_buf.unparsed_len(), 0);
    }
}
