//! Lab probe for a real camera's talk ability over Baichuan TCP. READ-ONLY:
//! signs in, asks TalkAbility (msg 10), optionally watches the preview for
//! the camera's own ADPCM microphone audio, signs out. It never sends
//! TalkConfig or talk audio (nothing plays on the camera). Ignored unless
//! run by hand:
//!   BC_HOST=... BC_PASS_FILE=... cargo test talk_probe -- --ignored --nocapture
//! The password is read from a file (never printed). A refused sign-in stops.

use super::bc;
use super::client::{is_wouldblock, Session, SignInRefused};
use super::transport::TcpTransport;
use std::time::{Duration, Instant};

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// IMA decode of one packet's nibbles in either order.
fn decode(pay: &[u8], low_first: bool) -> Vec<i16> {
    let mut st = super::talk::Ima { pred: i16::from_le_bytes([pay[0], pay[1]]) as i32, index: (pay[2] as i32).min(88) };
    let mut out = Vec::new();
    for &b in &pay[4..] {
        let (a, c) = if low_first { (b & 15, b >> 4) } else { (b >> 4, b & 15) };
        out.push(st.decode(a));
        out.push(st.decode(c));
    }
    out
}

/// High-frequency energy relative to the whole (lower = smoother).
fn roughness(x: &[i16]) -> f64 {
    let e: f64 = x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() + 1.0;
    let d: f64 = x.windows(2).map(|w| (w[1] as f64 - w[0] as f64).powi(2)).sum();
    d / e
}

#[test]
#[ignore]
fn talk_probe() {
    let host = env("BC_HOST", "");
    if host.is_empty() {
        return;
    }
    let port: u16 = env("BC_PORT", "9000").parse().unwrap();
    let user = env("BC_USER", "admin");
    let pass = std::fs::read_to_string(env("BC_PASS_FILE", "")).unwrap().trim().to_string();
    let ch: u8 = env("BC_CH", "0").parse().unwrap();
    let t = TcpTransport::connect(&host, port, Duration::from_secs(5)).unwrap();
    let mut s = match Session::sign_in(Box::new(t), &host, &user, &pass) {
        Ok(s) => s,
        Err(e) => {
            println!("SIGN-IN FAILED (not retried): {e:#} (refused={})", e.downcast_ref::<SignInRefused>().is_some());
            return;
        }
    };
    println!("{host}: signed in, aes={}", s.uses_aes);
    for tag in ["type", "name", "firmVersion", "channelNum", "audioNum"] {
        if let Some(v) = bc::xml_tag(&s.login_xml, tag) {
            println!("  {tag} = {v}");
        }
    }
    if s.has_battery() || bc::xml_tag(&s.login_xml, "channelNum").is_some_and(|n| n != "1") {
        println!("  battery or multi-channel device: not asking (signing out)");
        let _ = s.logout();
        return;
    }
    let num = s.send_ext_on(bc::MSG_TALK_ABILITY, ch, None, &super::talk::ext_xml(ch), b"", true).unwrap();
    let preview = env("BC_PREVIEW", "") == "1";
    let mut pnum = None;
    if preview {
        let xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Preview version=\"1.1\">\n<channelId>{ch}</channelId>\n<handle>256</handle>\n<streamType>subStream</streamType>\n</Preview>\n</body>\n");
        pnum = Some(s.send_xml_on(bc::MSG_PREVIEW, ch, 1, &xml).unwrap());
    }
    s.set_read_timeout(Duration::from_millis(200));
    let (mut buf, mut tmp) = (Vec::new(), vec![0u8; 65536]);
    let until = Instant::now() + Duration::from_secs(if preview { 6 } else { 4 });
    let mut media = Vec::new();
    while Instant::now() < until {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if is_wouldblock(&e) => {}
            Err(e) => {
                println!("read error {e}");
                break;
            }
        }
        while let Ok(Some((used, m))) = bc::parse_msg(&buf) {
            buf.drain(..used);
            let h = &m.header;
            if h.msg_id == bc::MSG_KEEPALIVE && h.status == 0 {
                let _ = s.ack_keepalive(h.channel, h.msg_num);
            } else if h.msg_id == bc::MSG_TALK_ABILITY && h.msg_num == num {
                let x = s.reply_xml(h, &m.body);
                println!("  TalkAbility status {}: {}", h.status, x.replace('\n', ""));
                match super::talk::parse_ability(&x) {
                    Ok(f) => println!("  -> {}", f.describe()),
                    Err(e) => println!("  -> {e}"),
                }
            } else if Some(h.msg_num) == pnum && h.msg_id == bc::MSG_PREVIEW {
                media.extend(s.decode_msg(h, &m.body).1);
            }
        }
    }
    if let Some(p) = pnum {
        let stop = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Preview version=\"1.1\">\n<channelId>{ch}</channelId>\n<handle>256</handle>\n</Preview>\n</body>\n");
        let _ = s.send_xml_on(bc::MSG_PREVIEW_STOP, ch, 1, &stop);
        let _ = p;
        // The camera's own microphone audio, if it sends ADPCM: which nibble
        // order decodes smoothly, and the header fields.
        let (mut lo, mut hi, mut n, mut aac) = (Vec::new(), Vec::new(), 0, 0);
        let mut i = 0;
        while i + 16 <= media.len() {
            if &media[i..i + 4] == b"05wb" {
                aac += 1;
            }
            if &media[i..i + 4] == b"01wb" && media[i + 8..i + 10] == [0, 1] {
                let inner = u16::from_le_bytes([media[i + 4], media[i + 5]]) as usize;
                let nib = u16::from_le_bytes([media[i + 10], media[i + 11]]) as usize;
                if n == 0 {
                    println!("  camera ADPCM packet: inner size {inner}, nibble bytes {nib}, state {:?}", &media[i + 12..i + 16]);
                }
                if i + 8 + inner <= media.len() && inner >= 8 {
                    let pay = &media[i + 12..i + 8 + inner];
                    lo.extend(decode(pay, true));
                    hi.extend(decode(pay, false));
                    n += 1;
                }
                i += 8 + inner;
                continue;
            }
            i += 1;
        }
        println!("  preview audio: {aac} AAC packets, {n} ADPCM packets");
        if n > 0 {
            println!("  roughness low-nibble-first {:.4}, high-nibble-first {:.4}", roughness(&lo), roughness(&hi));
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    let _ = s.logout();
    std::thread::sleep(Duration::from_millis(200));
}
