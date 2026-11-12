//! Baichuan UDP transport ("BcUdp") for battery cameras that don't listen on
//! TCP. Three packet types wrap the SAME BC message stream used over TCP:
//!   Discovery (magic 2a87cf3a): encrypted XML control (handshake, heartbeat)
//!   Ack       (magic 2a87cf20): reliable-delivery acknowledgements
//!   Data      (magic 2a87cf10): raw BC bytes, fragmented + sequenced
//! The discovery XML uses a 32-byte keystream cipher keyed by the packet's tid;
//! Data/Ack headers are plain little-endian.

use anyhow::{anyhow, bail, Result};
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

pub const MAGIC_DISCOVERY: u32 = 0x2a87_cf3a;
pub const MAGIC_ACK: u32 = 0x2a87_cf20;
pub const MAGIC_DATA: u32 = 0x2a87_cf10;
pub const MTU: usize = 1350;

/// Enlarge the socket receive buffer so a burst (a big keyframe fragmented into
/// many UDP packets) isn't dropped before we read it.
pub fn set_rcvbuf(sock: &UdpSocket, bytes: usize) {
    let _ = socket2::SockRef::from(sock).set_recv_buffer_size(bytes);
}
pub const DATA_HEADER: usize = 20;
pub const MAX_CHUNK: usize = MTU - DATA_HEADER; // 1330
/// Out-of-order packets held for reassembly at most (~5 MB of payload).
const MAX_PENDING: u32 = 4096;
/// Received-packet flags in one ack at most (it must fit one datagram).
const MAX_ACK_FLAGS: usize = 1024;

// ------------------------------------------------------------- randomness

/// A random 32-bit value from the OS-seeded generator: the connection ids
/// must not be guessable, or a LAN host could answer discovery in the
/// camera's place.
pub fn rand_u32() -> u32 {
    rand::random::<u32>()
}

// ------------------------------------------------------------- crypto + crc

/// The eight 32-bit key words of the discovery cipher.
const WORDS: [u32; 8] = [
    0x1f2d_3c4b, 0x5a6c_7f8d, 0x3817_2e4b, 0x8271_635a,
    0x863f_1a2b, 0xa5c6_f7d8, 0x8371_e1b4, 0x17f2_d3a5,
];

/// Encrypt or decrypt a discovery XML payload (same operation). The keystream
/// is each key word plus `tid` (wrapping), little-endian, concatenated (32 B),
/// then cycled.
pub fn disc_crypt(tid: u32, data: &[u8]) -> Vec<u8> {
    let mut ks = [0u8; 32];
    for (i, w) in WORDS.iter().enumerate() {
        ks[i * 4..i * 4 + 4].copy_from_slice(&w.wrapping_add(tid).to_le_bytes());
    }
    data.iter().enumerate().map(|(i, b)| b ^ ks[i % 32]).collect()
}

/// BC's non-standard CRC over the encrypted discovery payload: reflected poly
/// 0xEDB88320, initial value 0x00000000, and NO final XOR (not standard CRC-32).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    crc
}

// ------------------------------------------------------------- packets

fn u32le(b: &[u8], i: usize) -> u32 { u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) }
fn i32le(b: &[u8], i: usize) -> i32 { i32::from_le_bytes(b[i..i + 4].try_into().unwrap()) }

#[derive(Debug)]
pub enum Packet {
    Discovery { tid: u32, xml: String },
    Ack { connection_id: i32, group_id: u32, packet_id: u32, truth: Vec<u8> },
    Data { connection_id: i32, packet_id: u32, payload: Vec<u8> },
}

pub fn encode_discovery(tid: u32, xml: &str) -> Vec<u8> {
    let enc = disc_crypt(tid, xml.as_bytes());
    let crc = crc32(&enc);
    let mut p = Vec::with_capacity(20 + enc.len());
    p.extend_from_slice(&MAGIC_DISCOVERY.to_le_bytes());
    p.extend_from_slice(&(enc.len() as u32).to_le_bytes());
    p.extend_from_slice(&1u32.to_le_bytes()); // unknown == 1
    p.extend_from_slice(&tid.to_le_bytes());
    p.extend_from_slice(&crc.to_le_bytes());
    p.extend_from_slice(&enc);
    p
}

pub fn encode_data(connection_id: i32, packet_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(20 + payload.len());
    p.extend_from_slice(&MAGIC_DATA.to_le_bytes());
    p.extend_from_slice(&connection_id.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes()); // unknown == 0
    p.extend_from_slice(&packet_id.to_le_bytes());
    p.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    p.extend_from_slice(payload);
    p
}

pub fn encode_ack(connection_id: i32, group_id: u32, packet_id: u32, truth: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(24 + truth.len());
    p.extend_from_slice(&MAGIC_ACK.to_le_bytes());
    p.extend_from_slice(&connection_id.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes()); // unknown == 0
    p.extend_from_slice(&group_id.to_le_bytes());
    p.extend_from_slice(&packet_id.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes()); // maybe_latency
    p.extend_from_slice(&(truth.len() as u32).to_le_bytes());
    p.extend_from_slice(truth);
    p
}

/// An empty ack (nothing received yet).
pub fn encode_ack_empty(connection_id: i32) -> Vec<u8> {
    encode_ack(connection_id, 0xffff_ffff, 0xffff_ffff, &[])
}

pub fn decode(buf: &[u8]) -> Result<Packet> {
    if buf.len() < 8 { bail!("short udp packet ({} bytes)", buf.len()); }
    match u32le(buf, 0) {
        MAGIC_DISCOVERY => {
            if buf.len() < 20 { bail!("short discovery packet"); }
            let size = u32le(buf, 4) as usize;
            let tid = u32le(buf, 12);
            let enc = buf.get(20..20 + size).ok_or_else(|| anyhow!("discovery payload truncated"))?;
            let xml = String::from_utf8_lossy(&disc_crypt(tid, enc)).to_string();
            Ok(Packet::Discovery { tid, xml })
        }
        MAGIC_ACK => {
            if buf.len() < 24 { bail!("short ack packet"); }
            let connection_id = i32le(buf, 4);
            let group_id = u32le(buf, 12);
            let packet_id = u32le(buf, 16);
            let size = u32le(buf, 24 - 4) as usize; // payload_size at offset 20
            let truth = buf.get(24..24 + size).unwrap_or(&[]).to_vec();
            Ok(Packet::Ack { connection_id, group_id, packet_id, truth })
        }
        MAGIC_DATA => {
            if buf.len() < 20 { bail!("short data packet"); }
            let connection_id = i32le(buf, 4);
            let packet_id = u32le(buf, 12);
            let size = u32le(buf, 16) as usize;
            let payload = buf.get(20..20 + size).ok_or_else(|| anyhow!("data payload truncated"))?.to_vec();
            Ok(Packet::Data { connection_id, packet_id, payload })
        }
        m => bail!("unknown udp magic {m:08x}"),
    }
}

// ------------------------------------------------------------- discovery XML

/// The client->device connect request. `uid` is the camera UID; `port` the
/// client's local UDP port; `cid` the client id.
/// A UdpXml carries NO <?xml?> prolog (unlike the BC-layer XML), and <debug>
/// is the bool text "false". Compact, no extra whitespace.
pub fn c2d_c_xml(uid: &str, port: u16, cid: i32) -> String {
    format!(
        "<P2P><C2D_C><uid>{uid}</uid><cli><port>{port}</port></cli>\
         <cid>{cid}</cid><mtu>{MTU}</mtu><debug>false</debug><p>MAC</p></C2D_C></P2P>"
    )
}

/// The client heartbeat.
pub fn c2d_hb_xml(cid: i32, did: i32) -> String {
    format!("<P2P><C2D_HB><cid>{cid}</cid><did>{did}</did></C2D_HB></P2P>")
}

/// Minimal XML field read.
pub fn tag<'a>(xml: &'a str, t: &str) -> Option<&'a str> {
    let o = format!("<{t}>");
    let c = format!("</{t}>");
    let s = xml.find(&o)? + o.len();
    let e = xml[s..].find(&c)? + s;
    Some(xml[s..e].trim())
}

/// Result of a successful local discovery handshake.
pub struct Discovered {
    pub sock: UdpSocket,
    pub cam: SocketAddr,
    pub client_id: i32, // cid
    pub camera_id: i32, // did
    pub tid: u32,
}

/// Local discovery: send C2D_C to the camera's IP (and the broadcast
/// addresses, which a sleeping camera may need) on 2018 and 2015, and wait for
/// a D2C_C_R whose cid matches ours, sent FROM the camera's address: a reply
/// from anywhere else could be a LAN host posing as the camera to collect the
/// sign-in hashes.
pub fn discover_local(ip: &str, uid: &str, timeout: Duration) -> Result<Discovered> {
    let cam_ip: std::net::Ipv4Addr = ip
        .parse()
        .map_err(|_| anyhow::anyhow!("battery camera address {ip:?} must be an IPv4 address"))?;
    let sock = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    set_rcvbuf(&sock, 8 << 20); // 8 MB, so a burst (a large keyframe) isn't dropped
    sock.set_broadcast(true).ok();
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    let port = sock.local_addr()?.port();
    let cid = (rand_u32() & 0x7fff_ffff) as i32;
    let tid = (rand_u32() & 0xff).max(1); // nonzero byte, like the official client
    let pkt = encode_discovery(tid, &c2d_c_xml(uid, port, cid));
    // Unicast to the camera, plus subnet (/24) + global broadcast, on 2018 and 2015.
    let o = cam_ip.octets();
    let hosts = [cam_ip, std::net::Ipv4Addr::BROADCAST, std::net::Ipv4Addr::new(o[0], o[1], o[2], 255)];
    let dests: Vec<SocketAddr> = hosts
        .iter()
        .flat_map(|h| [2018u16, 2015].map(|p| SocketAddr::from((*h, p))))
        .collect();

    let deadline = std::time::Instant::now() + timeout;
    let debug = std::env::var("ARGUS_DEBUG").is_ok();
    let mut sent = 0;
    while std::time::Instant::now() < deadline {
        for d in &dests { if sock.send_to(&pkt, d).is_ok() { sent += 1; } }
        let mut buf = [0u8; 2048];
        // drain replies for ~500 ms, then resend
        let until = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < until {
            match sock.recv_from(&mut buf) {
                Ok((n, src)) if src.ip() == std::net::IpAddr::V4(cam_ip) => {
                    if debug {
                        let magic = if n >= 4 { u32le(&buf, 0) } else { 0 };
                        eprintln!("  [recv {n}B from {src} magic {magic:08x}]");
                    }
                    if let Ok(Packet::Discovery { xml, .. }) = decode(&buf[..n]) {
                        if debug { eprintln!("  [disc from {src}] {}", xml.chars().take(160).collect::<String>()); }
                        if xml.contains("D2C_C_R") {
                            let rcid = tag(&xml, "cid").and_then(|v| v.parse::<i32>().ok());
                            let did = tag(&xml, "did").and_then(|v| v.parse::<i32>().ok());
                            if rcid == Some(cid) {
                                if let Some(did) = did {
                                    sock.set_read_timeout(Some(Duration::from_millis(50)))?;
                                    return Ok(Discovered { sock, cam: src, client_id: cid, camera_id: did, tid });
                                }
                            }
                        }
                    }
                }
                Ok(_) => {} // not from the camera: ignore
                Err(_) => break,
            }
        }
    }
    bail!("no D2C_C_R reply from {ip} after {sent} packets (camera asleep, wrong UID, or UDP blocked)")
}

// ------------------------------------------------------------- transport

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

struct Shared {
    sock: UdpSocket,
    cam: SocketAddr,
    did: i32, // camera connection id (we put it in Data/Ack we send)
    cid: i32, // our client id (camera puts it in Data it sends us)
    tid: u32,
    sent_next: Mutex<u32>,
    unacked: Mutex<BTreeMap<u32, Vec<u8>>>,
    want: Mutex<u32>,
    pending: Mutex<BTreeMap<u32, Vec<u8>>>,
    inbuf: Mutex<VecDeque<u8>>,
    raw_dump: Mutex<Option<std::fs::File>>,
    alive: AtomicBool,
    dead_reason: Mutex<Option<String>>,
    n_data: std::sync::atomic::AtomicU32,
    n_ooo: std::sync::atomic::AtomicU32,
    n_dup: std::sync::atomic::AtomicU32,
    n_resent: std::sync::atomic::AtomicU32,
    n_bytes: std::sync::atomic::AtomicU64,
}

impl Shared {
    fn build_ack(&self) -> Vec<u8> {
        let want = *self.want.lock().unwrap();
        let pending = self.pending.lock().unwrap();
        if want == 0 && pending.is_empty() {
            return encode_ack_empty(self.did);
        }
        let last_contig = want.wrapping_sub(1);
        let maxid = pending.keys().max().copied().unwrap_or(last_contig);
        // One flag per packet after the last in-order one, counted (not
        // compared) so ids near u32::MAX cannot wrap into an endless loop,
        // and capped so the ack fits in one datagram.
        let span = if maxid >= want { (maxid - want) as usize + 1 } else { 0 };
        let truth: Vec<u8> = (0..span.min(MAX_ACK_FLAGS))
            .map(|k| u8::from(pending.contains_key(&want.wrapping_add(k as u32))))
            .collect();
        encode_ack(self.did, 0, last_contig, &truth)
    }
}

pub struct UdpTransport {
    shared: Arc<Shared>,
    read_timeout: Duration,
}

impl UdpTransport {
    pub fn connect(ip: &str, uid: &str, timeout: Duration) -> Result<UdpTransport> {
        let d = discover_local(ip, uid, timeout)?;
        d.sock.set_read_timeout(Some(Duration::from_millis(15)))?;
        let shared = Arc::new(Shared {
            sock: d.sock,
            cam: d.cam,
            did: d.camera_id,
            cid: d.client_id,
            tid: d.tid,
            sent_next: Mutex::new(0),
            unacked: Mutex::new(BTreeMap::new()),
            want: Mutex::new(0),
            pending: Mutex::new(BTreeMap::new()),
            inbuf: Mutex::new(VecDeque::new()),
            // Raw payload dump for protocol work: debug builds only (it holds the sign-in exchange).
            raw_dump: Mutex::new(if cfg!(debug_assertions) {
                std::env::var("ARGUS_RAW").ok().and_then(|p| std::fs::File::create(p).ok())
            } else {
                None
            }),
            alive: AtomicBool::new(true),
            dead_reason: Mutex::new(None),
            n_data: std::sync::atomic::AtomicU32::new(0),
            n_ooo: std::sync::atomic::AtomicU32::new(0),
            n_dup: std::sync::atomic::AtomicU32::new(0),
            n_resent: std::sync::atomic::AtomicU32::new(0),
            n_bytes: std::sync::atomic::AtomicU64::new(0),
        });
        let bg = shared.clone();
        std::thread::spawn(move || background(bg));
        Ok(UdpTransport { shared, read_timeout: Duration::from_secs(10) })
    }

    pub fn set_read_timeout(&mut self, d: Duration) {
        self.read_timeout = d;
    }
}

fn background(s: Arc<Shared>) {
    let mut buf = [0u8; 2048];
    let mut last_hb = Instant::now();
    let mut last_resend = Instant::now();
    let mut last_rx = Instant::now();
    let mut last_ack = Instant::now();
    let debug = std::env::var("ARGUS_UDP_DEBUG").is_ok();
    while s.alive.load(Ordering::Relaxed) {
        match s.sock.recv_from(&mut buf) {
            Ok((n, src)) if src == s.cam || src.ip() == s.cam.ip() => {
                last_rx = Instant::now();
                match decode(&buf[..n]) {
                    Ok(Packet::Data { connection_id, packet_id, payload }) if connection_id == s.cid => {
                        s.n_data.fetch_add(1, Ordering::Relaxed);
                        {
                            let mut want = s.want.lock().unwrap();
                            let mut pending = s.pending.lock().unwrap();
                            if packet_id < *want { s.n_dup.fetch_add(1, Ordering::Relaxed); }
                            else if packet_id > *want { s.n_ooo.fetch_add(1, Ordering::Relaxed); }
                            // Far ahead of what we wait for: not this stream (or a flood);
                            // dropped so the reorder buffer stays bounded.
                            if packet_id >= *want && packet_id - *want < MAX_PENDING {
                                pending.insert(packet_id, payload);
                                let mut inbuf = s.inbuf.lock().unwrap();
                                let mut w = *want;
                                while let Some(p) = pending.remove(&w) {
                                    s.n_bytes.fetch_add(p.len() as u64, Ordering::Relaxed);
                                    if let Some(f) = s.raw_dump.lock().unwrap().as_mut() {
                                        use std::io::Write as _;
                                        let _ = f.write_all(&p);
                                    }
                                    inbuf.extend(p);
                                    w = w.wrapping_add(1);
                                }
                                *want = w;
                            }
                        }
                        let ack = s.build_ack();
                        let _ = s.sock.send_to(&ack, s.cam);
                    }
                    Ok(Packet::Ack { packet_id, truth, .. }) => {
                        if packet_id != 0xffff_ffff {
                            let mut un = s.unacked.lock().unwrap();
                            un.retain(|&id, _| id > packet_id);
                            for (i, t) in truth.iter().enumerate() {
                                if *t > 0 { un.remove(&packet_id.wrapping_add(1 + i as u32)); }
                            }
                        }
                    }
                    Ok(_) => {} // discovery heartbeats etc.
                    Err(e) => if debug { eprintln!("  [udp decode {e}]"); }
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {}
            Err(_) => {}
        }
        let now = Instant::now();
        // The official client acks every 10 ms; pace the camera's stream.
        if now.duration_since(last_ack) >= Duration::from_millis(10) {
            last_ack = now;
            if *s.want.lock().unwrap() > 0 {
                let ack = s.build_ack();
                let _ = s.sock.send_to(&ack, s.cam);
            }
        }
        if now.duration_since(last_resend) >= Duration::from_millis(500) {
            last_resend = now;
            let un = s.unacked.lock().unwrap();
            s.n_resent.fetch_add(un.len() as u32, Ordering::Relaxed);
            for pkt in un.values() { let _ = s.sock.send_to(pkt, s.cam); }
        }
        if now.duration_since(last_hb) >= Duration::from_secs(1) {
            last_hb = now;
            let hb = encode_discovery(s.tid, &c2d_hb_xml(s.cid, s.did));
            let _ = s.sock.send_to(&hb, s.cam);
        }
        if now.duration_since(last_rx) >= Duration::from_secs(10) {
            *s.dead_reason.lock().unwrap() = Some("no packets from the camera for 10 s".into());
            s.alive.store(false, Ordering::Relaxed);
        }
    }
}

impl Read for UdpTransport {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let deadline = Instant::now() + self.read_timeout;
        loop {
            {
                let mut inbuf = self.shared.inbuf.lock().unwrap();
                if !inbuf.is_empty() {
                    let n = out.len().min(inbuf.len());
                    for b in out.iter_mut().take(n) { *b = inbuf.pop_front().unwrap(); }
                    return Ok(n);
                }
            }
            if !self.shared.alive.load(Ordering::Relaxed) {
                let r = self.shared.dead_reason.lock().unwrap().clone().unwrap_or_else(|| "udp session closed".into());
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, r));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "udp read timed out"));
            }
            std::thread::sleep(Duration::from_millis(3));
        }
    }
}

impl Write for UdpTransport {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        for chunk in data.chunks(MAX_CHUNK) {
            let id = { let mut n = self.shared.sent_next.lock().unwrap(); let id = *n; *n = n.wrapping_add(1); id };
            let pkt = encode_data(self.shared.did, id, chunk);
            self.shared.sock.send_to(&pkt, self.shared.cam)?;
            self.shared.unacked.lock().unwrap().insert(id, pkt);
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl UdpTransport {
    pub fn stats(&self) -> String {
        let s = &self.shared;
        format!("udp: {} data pkts, {} B delivered, {} out-of-order, {} dup, {} resent",
            s.n_data.load(Ordering::Relaxed), s.n_bytes.load(Ordering::Relaxed),
            s.n_ooo.load(Ordering::Relaxed), s.n_dup.load(Ordering::Relaxed),
            s.n_resent.load(Ordering::Relaxed))
    }
}

impl Drop for UdpTransport {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disc_crypt_roundtrip() {
        let m = b"<?xml?><P2P></P2P>";
        for tid in [0u32, 87, 1234567] {
            assert_eq!(disc_crypt(tid, &disc_crypt(tid, m)), m);
        }
    }
    #[test]
    fn crc32_bc() {
        // BC CRC (init 0, no final xor): empty input is 0.
        assert_eq!(crc32(b""), 0);
        // Non-standard: differs from the standard CRC-32 check value.
        assert_ne!(crc32(b"123456789"), 0xcbf4_3926);
    }
    #[test]
    fn data_roundtrip() {
        let p = encode_data(7, 3, b"hello");
        match decode(&p).unwrap() {
            Packet::Data { connection_id, packet_id, payload } => {
                assert_eq!((connection_id, packet_id, payload), (7, 3, b"hello".to_vec()));
            }
            _ => panic!("wrong type"),
        }
    }
    #[test]
    fn ack_empty_roundtrip() {
        match decode(&encode_ack_empty(9)).unwrap() {
            Packet::Ack { connection_id, group_id, packet_id, .. } => {
                assert_eq!(connection_id, 9);
                assert_eq!(group_id, 0xffff_ffff);
                assert_eq!(packet_id, 0xffff_ffff);
            }
            _ => panic!("wrong type"),
        }
    }
}
