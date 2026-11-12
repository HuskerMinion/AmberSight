//! Pushed streams: an encoder (OBS, ffmpeg, the capture helper) sends video to
//! AmberSight instead of AmberSight pulling it from a camera.
//!
//! A camera with the URL `push://<key>` waits here for a sender with that key.
//! Senders arrive over RTMP (OBS and most tools) or as FLV over HTTPS
//! (`ffmpeg ... -f flv https://server:port/push/<key>`); both become one FLV byte
//! stream that the camera reads with the same parser as HTTP-FLV cameras.
//! A new sender for a key replaces the old one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::Notify;

pub const SCHEME: &str = "push://";
/// Buffer between a sender and its camera; a slow reader slows the sender.
const PIPE_BYTES: usize = 4 << 20;

pub fn is_push_url(url: &str) -> bool {
    url.trim().to_ascii_lowercase().starts_with(SCHEME)
}

/// The stream key of a `push://<key>` URL.
pub fn push_key(url: &str) -> Option<&str> {
    let u = url.trim();
    if !is_push_url(u) {
        return None;
    }
    let k = u[SCHEME.len()..].trim_matches('/');
    (!k.is_empty()).then_some(k)
}

/// A key long and plain enough to be a password in an RTMP/HTTP address.
pub fn valid_key(k: &str) -> bool {
    k.len() >= 12 && k.len() <= 128 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Default)]
struct Slot {
    /// Reader end handed to the camera when it next asks.
    pending: Option<DuplexStream>,
    notify: Arc<Notify>,
    /// While a sender is connected: since when, and from where.
    live: Option<(Instant, String)>,
    /// Increases with each sender, so an old sender's end does not clear a newer one.
    gen: u64,
}

fn hub() -> &'static Mutex<HashMap<String, Slot>> {
    static H: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

/// A connected sender. Feed it the FLV byte stream (any chunking); it passes
/// whole tags on to the camera. If the camera reconnects, the sender stays
/// connected: a new pipe is opened and the stream's setup (FLV header,
/// metadata, codec configuration) is sent again first.
pub struct Publisher {
    key: String,
    gen: u64,
    w: DuplexStream,
    /// Bytes not yet forming a whole tag.
    buf: Vec<u8>,
    header_done: bool,
    /// FLV header + latest metadata + video/audio codec configuration.
    header: Vec<u8>,
    meta: Vec<u8>,
    vconf: Vec<u8>,
    aconf: Vec<u8>,
}

impl Drop for Publisher {
    fn drop(&mut self) {
        let mut h = hub().lock().unwrap();
        if let Some(s) = h.get_mut(&self.key) {
            if s.gen == self.gen {
                s.live = None;
            }
        }
    }
}

impl Publisher {
    /// Pass FLV bytes on. Err when another sender took over this key.
    pub async fn write(&mut self, b: &[u8]) -> std::io::Result<()> {
        self.buf.extend_from_slice(b);
        let mut pos = 0;
        loop {
            let avail = &self.buf[pos..];
            if !self.header_done {
                if avail.len() < 9 {
                    break;
                }
                if &avail[..3] != b"FLV" {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not an FLV stream"));
                }
                let n = u32::from_be_bytes([avail[5], avail[6], avail[7], avail[8]]) as usize + 4;
                if avail.len() < n {
                    break;
                }
                self.header = avail[..n].to_vec();
                self.header_done = true;
                let unit = self.header.clone();
                self.send(&unit).await?;
                pos += n;
                continue;
            }
            if avail.len() < 11 {
                break;
            }
            let size = u32::from_be_bytes([0, avail[1], avail[2], avail[3]]) as usize;
            if size > 16 << 20 {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "FLV tag too large"));
            }
            let n = 11 + size + 4;
            if avail.len() < n {
                break;
            }
            let unit = avail[..n].to_vec();
            pos += n;
            // Remember the setup a reconnecting camera needs first.
            let body = &unit[11..11 + size];
            match unit[0] & 0x1f {
                18 => self.meta = unit.clone(),
                9 if is_video_config(body) => self.vconf = unit.clone(),
                8 if body.len() > 1 && body[0] >> 4 == 10 && body[1] == 0 => self.aconf = unit.clone(),
                _ => {}
            }
            self.send(&unit).await?;
        }
        self.buf.drain(..pos);
        Ok(())
    }

    /// Write one whole unit; if the camera's pipe is gone, open a new one
    /// (with the setup replayed) unless a newer sender owns the key.
    async fn send(&mut self, unit: &[u8]) -> std::io::Result<()> {
        if self.w.write_all(unit).await.is_ok() {
            return Ok(());
        }
        let (w, r) = tokio::io::duplex(PIPE_BYTES);
        {
            let mut h = hub().lock().unwrap();
            let s = h.entry(self.key.clone()).or_default();
            if s.gen != self.gen {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "another sender took over"));
            }
            s.pending = Some(r);
            s.notify.notify_waiters();
        }
        self.w = w;
        let setup: Vec<u8> = [&self.header[..], &self.meta, &self.vconf, &self.aconf].concat();
        self.w.write_all(&setup).await?;
        if unit.first() != Some(&b'F') {
            self.w.write_all(unit).await?;
        }
        Ok(())
    }
}

/// An FLV video tag body that carries codec configuration (AVC/HEVC sequence
/// header, or the enhanced-RTMP sequence start).
fn is_video_config(body: &[u8]) -> bool {
    match body.first() {
        Some(&b0) if b0 & 0x80 != 0 => b0 & 0x0f == 0,
        Some(_) => body.get(1) == Some(&0),
        None => false,
    }
}

/// Start sending to `key` (replaces any earlier sender for it).
pub fn publish(key: &str, from: &str) -> Publisher {
    let (w, r) = tokio::io::duplex(PIPE_BYTES);
    let mut h = hub().lock().unwrap();
    let s = h.entry(key.to_string()).or_default();
    s.gen += 1;
    s.pending = Some(r);
    s.live = Some((Instant::now(), from.to_string()));
    s.notify.notify_waiters();
    Publisher {
        key: key.to_string(),
        gen: s.gen,
        w,
        buf: Vec::new(),
        header_done: false,
        header: Vec::new(),
        meta: Vec::new(),
        vconf: Vec::new(),
        aconf: Vec::new(),
    }
}

/// The camera side: wait up to `wait` for a sender's stream.
pub async fn take_stream(key: &str, wait: Duration) -> Option<DuplexStream> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let notify = {
            let mut h = hub().lock().unwrap();
            let s = h.entry(key.to_string()).or_default();
            if let Some(r) = s.pending.take() {
                return Some(r);
            }
            s.notify.clone()
        };
        if tokio::time::timeout_at(deadline, notify.notified()).await.is_err() {
            return None;
        }
    }
}

/// Whether a sender is connected to `key`, and from where.
pub fn sender(key: &str) -> Option<(Duration, String)> {
    let h = hub().lock().unwrap();
    h.get(key).and_then(|s| s.live.as_ref()).map(|(t, f)| (t.elapsed(), f.clone()))
}

/// FLV file header (audio + video) and PreviousTagSize0.
pub fn flv_header() -> [u8; 13] {
    [b'F', b'L', b'V', 1, 0x05, 0, 0, 0, 9, 0, 0, 0, 0]
}

/// An FLV `onMetaData` script tag body (what RTMP senders send separately).
pub fn meta_body(width: Option<u32>, height: Option<u32>, fps: Option<f32>, audio: bool) -> Vec<u8> {
    let mut b = vec![0x02];
    b.extend_from_slice(&(10u16).to_be_bytes());
    b.extend_from_slice(b"onMetaData");
    let mut props: Vec<(&str, f64)> = Vec::new();
    if let Some(w) = width { props.push(("width", w as f64)); }
    if let Some(h) = height { props.push(("height", h as f64)); }
    if let Some(f) = fps { props.push(("framerate", f as f64)); }
    if audio { props.push(("audiocodecid", 10.0)); }
    b.push(0x08);
    b.extend_from_slice(&(props.len() as u32).to_be_bytes());
    for (k, v) in props {
        b.extend_from_slice(&(k.len() as u16).to_be_bytes());
        b.extend_from_slice(k.as_bytes());
        b.push(0x00);
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&[0, 0, 9]);
    b
}

/// One FLV tag (8 = audio, 9 = video, 18 = script data) with its PreviousTagSize.
pub fn flv_tag(kind: u8, ms: u32, body: &[u8]) -> Vec<u8> {
    let n = body.len() as u32;
    let mut t = Vec::with_capacity(15 + body.len());
    t.push(kind);
    t.extend_from_slice(&n.to_be_bytes()[1..]);
    t.extend_from_slice(&ms.to_be_bytes()[1..]);
    t.push((ms >> 24) as u8);
    t.extend_from_slice(&[0, 0, 0]);
    t.extend_from_slice(body);
    t.extend_from_slice(&(11 + n).to_be_bytes());
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn urls_and_keys() {
        assert_eq!(push_key("push://abcDEF123456"), Some("abcDEF123456"));
        assert_eq!(push_key(" PUSH://k/ "), Some("k"));
        assert!(push_key("rtsp://x").is_none());
        assert!(valid_key("abcDEF123456") && !valid_key("short") && !valid_key("has space here"));
    }

    #[test]
    fn flv_tags_parse_back() {
        let mut bytes = flv_header().to_vec();
        bytes.extend(flv_tag(9, 0x0102_0304, &[0x17, 1, 0, 0, 0, 9, 9]));
        let mut r = crate::ingest::flv::TagReader::default();
        r.push(&bytes);
        let t = r.next_tag().unwrap().expect("a tag");
        assert!(matches!(t, crate::ingest::flv::Tag::Video { .. }), "{t:?}");
    }

    #[test]
    fn metadata_tag_parses() {
        let mut bytes = flv_header().to_vec();
        bytes.extend(flv_tag(18, 0, &meta_body(Some(1280), Some(720), Some(15.0), true)));
        let mut r = crate::ingest::flv::TagReader::default();
        r.push(&bytes);
        let t = r.next_tag().unwrap().expect("a tag");
        assert!(matches!(t, crate::ingest::flv::Tag::Meta { width: Some(1280), height: Some(720), audio: true }), "{t:?}");
    }

    #[tokio::test]
    async fn sender_survives_camera_reconnect_and_replacement_ends_the_old_one() {
        let key = "test-key-0123456789";
        let reader = tokio::spawn(take_stream(key, Duration::from_secs(5)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut p1 = publish(key, "a");
        let meta = flv_tag(18, 0, &meta_body(Some(64), Some(48), None, false));
        let conf = flv_tag(9, 0, &[0x17, 0, 0, 0, 0, 1, 2]);
        let frame = flv_tag(9, 40, &[0x27, 1, 0, 0, 0, 7, 7]);
        // Split mid-tag on purpose: the publisher must pass whole tags.
        let mut all = flv_header().to_vec();
        all.extend(&meta);
        all.extend(&conf);
        p1.write(&all[..20]).await.unwrap();
        p1.write(&all[20..]).await.unwrap();
        let mut r = reader.await.unwrap().expect("stream");
        let mut got = vec![0u8; all.len()];
        r.read_exact(&mut got).await.unwrap();
        assert_eq!(got, all);
        assert!(sender(key).is_some());
        // The camera reconnects: the sender keeps going; the new pipe starts with the setup.
        drop(r);
        p1.write(&frame).await.unwrap();
        let mut r2 = take_stream(key, Duration::from_millis(200)).await.expect("a new pipe");
        let mut want = flv_header().to_vec();
        want.extend(&meta);
        want.extend(&conf);
        want.extend(&frame);
        let mut got2 = vec![0u8; want.len()];
        r2.read_exact(&mut got2).await.unwrap();
        assert_eq!(got2, want);
        // A second sender takes over: the first is told to stop once it writes again.
        let _p2 = publish(key, "b");
        let _r3 = take_stream(key, Duration::from_millis(100)).await.expect("p2 pipe");
        drop(r2);
        assert!(p1.write(&frame).await.is_err(), "replaced sender stops");
        drop(p1);
        assert_eq!(sender(key).map(|s| s.1), Some("b".into()));
    }
}
