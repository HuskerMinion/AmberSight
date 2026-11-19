//! Lab probe of a real camera's controls over Baichuan TCP, READ-ONLY: sign
//! in, ask what it supports (199), the account's abilities (151) and the
//! state of its lights (208), sign out. Nothing that writes can be sent from
//! here (`send_parts` with write = false goes through the read-only list).
//! Ignored unless run by hand:
//!   BC_HOST=... BC_USER=... BC_PASS_FILE=... cargo test lab_ctl_probe -- --ignored --nocapture
//! The password is read from a file (never printed). A refused sign-in stops
//! at once (never tried again).

use super::bc;
use super::client::{is_wouldblock, Session};
use super::transport::TcpTransport;
use std::time::{Duration, Instant};

fn ask(s: &mut Session, id: u32, ext: &str) -> Option<(u16, String)> {
    let num = s.send_parts(id, 0, ext, "", false).ok()?;
    s.set_read_timeout(Duration::from_millis(200));
    let (mut buf, mut tmp) = (Vec::new(), vec![0u8; 65536]);
    let until = Instant::now() + Duration::from_secs(6);
    while Instant::now() < until {
        match s.read(&mut tmp) {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if is_wouldblock(&e) => {}
            Err(_) => return None,
        }
        while let Ok(Some((used, m))) = bc::parse_msg(&buf) {
            buf.drain(..used);
            if m.header.msg_id == bc::MSG_KEEPALIVE {
                let _ = s.ack_keepalive(m.header.channel, m.header.msg_num);
                continue;
            }
            if m.header.msg_id == id && m.header.msg_num == num {
                return Some((m.header.status, s.reply_xml(&m.header, &m.body)));
            }
            println!("  (other message id={} num={} status={})", m.header.msg_id, m.header.msg_num, m.header.status);
        }
    }
    None
}

#[test]
#[ignore]
fn lab_ctl_probe() {
    let host = std::env::var("BC_HOST").unwrap_or_default();
    if host.is_empty() {
        return;
    }
    let port: u16 = std::env::var("BC_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(9000);
    let user = std::env::var("BC_USER").unwrap_or_else(|_| "admin".into());
    let pass = std::fs::read_to_string(std::env::var("BC_PASS_FILE").unwrap()).unwrap().trim().to_string();
    let t = TcpTransport::connect(&host, port, Duration::from_secs(5)).unwrap();
    let mut s = match Session::sign_in(Box::new(t), &host, &user, &pass) {
        Ok(s) => s,
        Err(e) => {
            println!("SIGN-IN FAILED: {e:#} (refused={}); not trying again", e.downcast_ref::<super::client::SignInRefused>().is_some());
            return;
        }
    };
    println!("signed in: aes={}", s.uses_aes);
    for tag in ["type", "name", "firmVersion", "channelNum", "exactType"] {
        if let Some(v) = bc::xml_tag(&s.login_xml, tag) {
            println!("  {tag} = {v}");
        }
    }
    let ch = super::ctl::channel_ext(0);
    let ab = super::ctl::ability_ext(&user);
    for (id, ext, what) in [(bc::MSG_SUPPORT, "", "199 Support"), (bc::MSG_ABILITY, ab.as_str(), "151 AbilityInfo (image)"), (bc::MSG_LED_GET, ch.as_str(), "208 LedState")] {
        match ask(&mut s, id, ext) {
            Some((st, xml)) => println!("== {what}: status {st}\n{xml}"),
            None => println!("== {what}: no answer"),
        }
    }
    let _ = s.logout();
    std::thread::sleep(Duration::from_millis(300));
}
