//! Baichuan message framing and body encryption.
//!
//! Every message starts with a little-endian header:
//!   magic u32 (0x0abcdef0) | msg_id u32 | body_len u32 |
//!   channel u8 | stream u8 | msg_num u16 | status u16 | class u16 |
//!   [payload_offset u32, for classes that carry one]
//! Bodies are XML (and, for media, binary payload after payload_offset).

use anyhow::{bail, Result};
use std::io::{Read, Write};

pub const MAGIC: u32 = 0x0abc_def0;
/// Some devices answer with the byte-swapped magic.
pub const MAGIC_REV: u32 = 0x0fed_cba0;

pub const MSG_LOGIN: u32 = 1;
pub const MSG_LOGOUT: u32 = 2;
pub const MSG_PREVIEW: u32 = 3;
pub const MSG_PREVIEW_STOP: u32 = 4;
pub const MSG_ABILITY: u32 = 151;       // query account abilities
pub const MSG_MOTION_REQUEST: u32 = 31; // subscribe to motion events
pub const MSG_MOTION: u32 = 33;         // camera-pushed motion event (received)
pub const MSG_KEEPALIVE: u32 = 234;     // camera-initiated; we ack it
pub const MSG_BATTERY: u32 = 253;       // read battery/solar state
pub const MSG_DOWNLOAD: u32 = 143;      // stream SD-card recordings for a time range (read-only)
pub const MSG_DOWNLOAD_STOP: u32 = 144; // stop a cmd-143 stream early
pub const MSG_GENERAL: u32 = 104;       // read the camera's clock and time zone

/// Message ids this client is allowed to send. All are read-only or housekeeping
/// (sign in/out, preview, subscribe to motion, ack keep-alives, read battery,
/// stream SD recordings for a time range and stop that stream, read the
/// clock). Anything that could change a camera
/// setting (e.g. the PIR-config write, 213) is deliberately absent.
pub const ALLOWED: &[u32] =
    &[MSG_LOGIN, MSG_LOGOUT, MSG_PREVIEW, MSG_PREVIEW_STOP, MSG_MOTION_REQUEST, MSG_KEEPALIVE, MSG_BATTERY, MSG_ABILITY, MSG_DOWNLOAD,
      MSG_DOWNLOAD_STOP, MSG_GENERAL];

pub const CLASS_LEGACY: u16 = 0x6514;
pub const CLASS_MODERN: u16 = 0x6614;
pub const CLASS_MODERN_OFS: u16 = 0x6414;
pub const CLASS_REPLY_OFS: u16 = 0x0000;

pub fn class_has_offset(class: u16) -> bool {
    matches!(class, CLASS_MODERN_OFS | CLASS_REPLY_OFS)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub msg_id: u32,
    pub body_len: u32,
    pub channel: u8,
    pub stream: u8,
    pub msg_num: u16,
    pub status: u16,
    pub class: u16,
    pub payload_offset: Option<u32>,
}

impl Header {
    pub fn len(&self) -> usize {
        if self.payload_offset.is_some() { 24 } else { 20 }
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(24);
        v.extend_from_slice(&MAGIC.to_le_bytes());
        v.extend_from_slice(&self.msg_id.to_le_bytes());
        v.extend_from_slice(&self.body_len.to_le_bytes());
        v.push(self.channel);
        v.push(self.stream);
        v.extend_from_slice(&self.msg_num.to_le_bytes());
        v.extend_from_slice(&self.status.to_le_bytes());
        v.extend_from_slice(&self.class.to_le_bytes());
        if let Some(o) = self.payload_offset {
            v.extend_from_slice(&o.to_le_bytes());
        }
        v
    }
    /// Parse the first 20 bytes; the caller reads 4 more if `needs_offset`.
    pub fn decode20(b: &[u8; 20]) -> Result<Self> {
        let u32at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u16at = |i: usize| u16::from_le_bytes(b[i..i + 2].try_into().unwrap());
        let magic = u32at(0);
        if magic != MAGIC && magic != MAGIC_REV {
            bail!("bad magic {magic:08x} (header bytes {})", hex(b));
        }
        Ok(Header {
            msg_id: u32at(4),
            body_len: u32at(8),
            channel: b[12],
            stream: b[13],
            msg_num: u16at(14),
            status: u16at(16),
            class: u16at(18),
            payload_offset: None,
        })
    }
}

pub struct Msg {
    pub header: Header,
    pub body: Vec<u8>,
}

impl Msg {
    /// The XML extension / body part (before payload_offset).
    pub fn xml_part(&self) -> &[u8] {
        match self.header.payload_offset {
            Some(o) if (o as usize) <= self.body.len() && o > 0 => &self.body[..o as usize],
            Some(_) => &[],
            None => &self.body,
        }
    }
    pub fn payload_part(&self) -> &[u8] {
        match self.header.payload_offset {
            Some(o) if (o as usize) <= self.body.len() => &self.body[o as usize..],
            _ => &[],
        }
    }
}

/// Parse one complete message from the front of `buf` without consuming it on a
/// short read. Returns Ok(None) if `buf` doesn't yet hold a whole message (the
/// caller keeps the bytes and reads more), or the consumed length + message.
/// This avoids read_exact's partial-byte loss over a timing-variable transport.
pub fn parse_msg(buf: &[u8]) -> Result<Option<(usize, Msg)>> {
    if buf.len() < 20 {
        return Ok(None);
    }
    let mut header = Header::decode20(buf[..20].try_into().unwrap())?;
    let mut off = 20;
    if class_has_offset(header.class) {
        if buf.len() < 24 {
            return Ok(None);
        }
        header.payload_offset = Some(u32::from_le_bytes(buf[20..24].try_into().unwrap()));
        off = 24;
    }
    if header.body_len > 16 * 1024 * 1024 {
        bail!("body length {} is not believable", header.body_len);
    }
    let total = off + header.body_len as usize;
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some((total, Msg { header, body: buf[off..total].to_vec() })))
}

pub fn read_msg<R: Read>(r: &mut R) -> Result<Msg> {
    let mut h20 = [0u8; 20];
    r.read_exact(&mut h20)?;
    let mut header = Header::decode20(&h20)?;
    if class_has_offset(header.class) {
        let mut o = [0u8; 4];
        r.read_exact(&mut o)?;
        header.payload_offset = Some(u32::from_le_bytes(o));
    }
    if header.body_len > 16 * 1024 * 1024 {
        bail!("body length {} is not believable", header.body_len);
    }
    let mut body = vec![0u8; header.body_len as usize];
    r.read_exact(&mut body)?;
    Ok(Msg { header, body })
}

pub fn write_msg<W: Write>(w: &mut W, header: &Header, body: &[u8]) -> Result<()> {
    if !ALLOWED.contains(&header.msg_id) {
        bail!("refusing to send message id {} (not on the read-only list)", header.msg_id);
    }
    let mut h = header.clone();
    h.body_len = body.len() as u32;
    let mut out = h.encode();
    out.extend_from_slice(body);
    w.write_all(&out)?;
    w.flush()?;
    Ok(())
}

// ---------------------------------------------------------------- crypto

/// The fixed 8-byte key of the original scheme (the hard-coded key in
/// CVE-2020-25173). Each byte is also XORed with the low byte of `offset`.
const XOR_KEY: [u8; 8] = [0x1F, 0x2D, 0x3C, 0x4B, 0x5A, 0x69, 0x78, 0xFF];

/// BC ("Baichuan") body cipher. For byte i:
///   out[i] = in[i] ^ KEY[(offset + i) % 8] ^ (offset as u8)
/// The additive term is the constant low byte of `offset`, and `offset` is the
/// message header's channel-id byte (0 for login). XOR is its own inverse.
pub fn xor_crypt(offset: u32, data: &[u8]) -> Vec<u8> {
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ XOR_KEY[(offset as usize + i) % 8] ^ (offset as u8))
        .collect()
}

/// MD5 of `s`, uppercase hex, truncated to the first 31 characters — the form
/// the login messages use for the user and password fields.
pub fn md5_31_upper(s: &str) -> String {
    md5_hex_upper(s.as_bytes())[..31].to_string()
}

/// Newer firmware: AES-128-CFB, key = first 16 characters of the uppercase
/// MD5 hex of "<nonce>-<password>", IV "0123456789abcdef".
pub struct Aes {
    key: [u8; 16],
}

const AES_IV: &[u8; 16] = b"0123456789abcdef";

impl Aes {
    pub fn new(nonce: &str, password: &str) -> Self {
        let h = md5_hex_upper(format!("{nonce}-{password}").as_bytes());
        let mut key = [0u8; 16];
        key.copy_from_slice(&h.as_bytes()[..16]);
        Aes { key }
    }
    pub fn encrypt(&self, data: &[u8]) -> Vec<u8> {
        use cfb_mode::cipher::{AsyncStreamCipher, KeyIvInit};
        let mut buf = data.to_vec();
        cfb_mode::Encryptor::<aes::Aes128>::new(&self.key.into(), AES_IV.into()).encrypt(&mut buf);
        buf
    }
    pub fn decrypt(&self, data: &[u8]) -> Vec<u8> {
        use cfb_mode::cipher::{AsyncStreamCipher, KeyIvInit};
        let mut buf = data.to_vec();
        cfb_mode::Decryptor::<aes::Aes128>::new(&self.key.into(), AES_IV.into()).decrypt(&mut buf);
        buf
    }
}

pub fn md5_hex_upper(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    Md5::digest(data).iter().map(|b| format!("{b:02X}")).collect()
}

pub fn md5_hex_lower(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    Md5::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// Credentials as the login messages carry them: MD5 hex, 31 characters.
pub fn md5_31(s: &str) -> String {
    md5_hex_upper(s.as_bytes())[..31].to_string()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

/// Pull `<tag>value</tag>` out of a small XML document (good enough for
/// the probe; the real client would use a parser).
pub fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = xml.find(&open)? + open.len();
    let e = xml[s..].find(&close)? + s;
    Some(xml[s..e].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let h = Header { msg_id: 3, body_len: 9, channel: 0, stream: 1, msg_num: 7, status: 0, class: CLASS_MODERN_OFS, payload_offset: Some(5) };
        let e = h.encode();
        assert_eq!(e.len(), 24);
        assert_eq!(&e[..4], &[0xf0, 0xde, 0xbc, 0x0a]);
        let d = Header::decode20(e[..20].try_into().unwrap()).unwrap();
        assert_eq!(d.msg_id, 3);
        assert!(class_has_offset(d.class));
    }

    #[test]
    fn xor_is_its_own_inverse() {
        let m = b"<?xml version=\"1.0\"?><body/>";
        for off in [0u32, 1, 7, 250] {
            assert_eq!(xor_crypt(off, &xor_crypt(off, m)), m);
        }
        // offset 0: first byte is XORed with 0x1F only
        assert_eq!(xor_crypt(0, &[0x00])[0], 0x1F);
    }

    #[test]
    fn aes_round_trip_and_key() {
        let a = Aes::new("abc", "pw");
        let m = b"hello baichuan body of some length";
        assert_eq!(a.decrypt(&a.encrypt(m)), m);
        assert_eq!(a.key.len(), 16);
        assert!(a.key.iter().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn refuses_unlisted_messages() {
        let h = Header { msg_id: 999, body_len: 0, channel: 0, stream: 0, msg_num: 0, status: 0, class: CLASS_MODERN, payload_offset: None };
        let mut out = Vec::new();
        assert!(write_msg(&mut out, &h, b"").is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn xml_tags() {
        let x = "<Encryption><type>md5</type><nonce> N1 </nonce></Encryption>";
        assert_eq!(xml_tag(x, "nonce"), Some("N1"));
        assert_eq!(xml_tag(x, "missing"), None);
    }
}
