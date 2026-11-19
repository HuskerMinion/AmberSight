//! Lab probe for a real camera over Baichuan TCP (read-only: sign in,
//! preview, sign out). Ignored unless run by hand:
//!   BC_HOST=... BC_USER=... BC_PASS_FILE=... cargo test lab_probe -- --ignored --nocapture
//! The password is read from a file (never printed).

use super::bc;
use super::client::Session;
use super::transport::TcpTransport;
use std::time::{Duration, Instant};

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

#[test]
#[ignore]
fn lab_probe() {
    let host = env("BC_HOST", "");
    if host.is_empty() {
        return;
    }
    let port: u16 = env("BC_PORT", "9000").parse().unwrap();
    let user = env("BC_USER", "admin");
    let pass = std::fs::read_to_string(env("BC_PASS_FILE", "")).unwrap().trim().to_string();
    let ch: u8 = env("BC_CH", "0").parse().unwrap();
    let hdr_ch: u8 = env("BC_HDRCH", &ch.to_string()).parse().unwrap();
    let stream = env("BC_STREAM", "subStream");
    let handle = env("BC_HANDLE", if stream == "mainStream" { "0" } else { "256" });
    let code: u8 = env("BC_CODE", if stream == "mainStream" { "0" } else { "1" }).parse().unwrap();
    let secs: u64 = env("BC_SECS", "6").parse().unwrap();

    let t = TcpTransport::connect(&host, port, Duration::from_secs(5)).unwrap();
    let mut s = match Session::sign_in(Box::new(t), &host, &user, &pass) {
        Ok(s) => s,
        Err(e) => {
            println!("SIGN-IN FAILED: {e:#} (refused={})", e.downcast_ref::<super::client::SignInRefused>().is_some());
            return;
        }
    };
    println!("signed in: aes={} login_xml={} bytes", s.uses_aes, s.login_xml.len());
    for tag in ["type", "name", "firmVersion", "channelNum", "audioNum", "sdCard", "exactType", "boardInfo", "IOInputNum", "pakSuffix"] {
        if let Some(v) = bc::xml_tag(&s.login_xml, tag) {
            println!("  {tag} = {v}");
        }
    }
    if env("BC_XML", "") == "1" {
        println!("{}", s.login_xml);
    }
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Preview version=\"1.1\">\n<channelId>{ch}</channelId>\n<handle>{handle}</handle>\n<streamType>{stream}</streamType>\n</Preview>\n</body>\n"
    );
    let num = s.send_xml_on(bc::MSG_PREVIEW, hdr_ch, code, &xml).unwrap();
    println!("preview sent: msg_num={num} hdr_ch={hdr_ch} code={code} handle={handle} stream={stream} ch={ch}");
    s.set_read_timeout(Duration::from_millis(200));
    let mut buf = Vec::new();
    let mut tmp = vec![0u8; 65536];
    let until = Instant::now() + Duration::from_secs(secs);
    let mut shown = 0;
    let mut counts: std::collections::BTreeMap<(u32, u8, u8, u16, u16), (usize, usize)> = Default::default();
    let mut demux = super::media::Demux::new();
    demux.keep_audio = true;
    let (mut frames, mut keys, mut audio, mut codec) = (0, 0, 0, String::new());
    let mut biggest = 0;
    while Instant::now() < until {
        match s.read(&mut tmp) {
            Ok(0) => {
                println!("closed by camera");
                break;
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if super::client::is_wouldblock(&e) => {}
            Err(e) => {
                println!("read error {e}");
                break;
            }
        }
        while let Ok(Some((used, m))) = bc::parse_msg(&buf) {
            buf.drain(..used);
            let h = &m.header;
            biggest = biggest.max(m.body.len());
            let e = counts.entry((h.msg_id, h.channel, h.stream, h.msg_num, h.status)).or_default();
            e.0 += 1;
            e.1 += m.body.len();
            if h.msg_id == bc::MSG_KEEPALIVE {
                println!("keepalive from camera: ch={} num={} status={} class={:04x}", h.channel, h.msg_num, h.status, h.class);
                let _ = s.ack_keepalive(h.channel, h.msg_num);
            }
            let (ext, pay) = s.decode_msg(h, &m.body);
            if shown < 12 {
                shown += 1;
                println!(
                    "msg id={} ch={} st={} num={} status={} class={:04x} off={:?} body={} ext={:?} pay[..8]={:?}",
                    h.msg_id, h.channel, h.stream, h.msg_num, h.status, h.class, h.payload_offset, m.body.len(),
                    String::from_utf8_lossy(&ext).chars().take(200).collect::<String>(),
                    String::from_utf8_lossy(&pay[..pay.len().min(8)])
                );
            }
            if h.msg_id == bc::MSG_PREVIEW {
                for f in demux.push(&pay) {
                    frames += 1;
                    if f.key {
                        keys += 1;
                    }
                }
                audio += demux.take_audio().len();
                codec = if demux.h265 { "H.265".into() } else { "H.264".into() };
            }
        }
    }
    println!("frames={frames} keys={keys} aac={audio} codec={codec} size={}x{} damaged={} resized={} biggest_body={biggest}", demux.width, demux.height, demux.damaged, demux.resized);
    for (k, v) in counts {
        println!("  id={} ch={} st={} num={} status={}: {} msgs {} bytes", k.0, k.1, k.2, k.3, k.4, v.0, v.1);
    }
    let stop = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Preview version=\"1.1\">\n<channelId>{ch}</channelId>\n<handle>{handle}</handle>\n</Preview>\n</body>\n");
    let _ = s.send_xml_on(bc::MSG_PREVIEW_STOP, hdr_ch, code, &stop);
    std::thread::sleep(Duration::from_millis(300));
    let _ = s.logout();
    std::thread::sleep(Duration::from_millis(300));
}
