//! A fake Reolink camera on Baichuan TCP (port 9000) for tests: sign-in
//! (nonce, hashed user name and password, AES), what it supports (199), the
//! account's abilities (151), the infrared and status lights (208 read, 209
//! write) and the siren (263). Answers are shaped as the lab's E1 Pro and E1
//! Outdoor SE answered the read-only questions (2026-11-19); the writes are
//! as the public descriptions give them, and are only proven here.

use crate::ingest::baichuan::{bc, ctl};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct BcState {
    pub user: String,
    pub pass: String,
    /// 199's flags for channel 0.
    pub led_ctrl: u64,
    pub audio_version: u64,
    pub battery: bool,
    pub channels: u32,
    /// The device type in the sign-in reply.
    pub dev_type: String,
    /// 151 lists `ledState_rw` for the account.
    pub ir_rw: bool,
    /// 208's words.
    pub ir: String,
    pub light: String,
    /// "Done", and nothing changes.
    pub noop_writes: bool,
    /// The siren's off (manual switch off) is refused.
    pub fail_siren_stop: bool,
    /// Writes are refused as for an account that may not (401).
    pub not_admin: bool,
    pub refuse_login: bool,
    pub logins: usize,
    pub connections: usize,
    /// Sessions open at once now, and the most seen.
    pub open: usize,
    pub most_open: usize,
    /// Every message (id, extension + body in the clear).
    pub log: Vec<(u32, String)>,
    /// The siren as two things, as a camera is believed to have them: a timed
    /// play that ends by itself after its rounds (`playMode` 0), and the
    /// manual switch (`playMode` 2) that stays on until switched off.
    pub siren_timed_until: Option<std::time::Instant>,
    pub siren_manual: bool,
    /// One round of a timed play.
    pub round_ms: u64,
    /// A timed play is done, and its answer never sent (lost).
    pub mute_play: bool,
    /// The sign-in's answer waits this long (a slow camera).
    pub login_delay_ms: u64,
}

impl BcState {
    pub fn playing(&self) -> bool {
        self.siren_manual || self.siren_timed_until.is_some_and(|t| std::time::Instant::now() < t)
    }
}

impl Default for BcState {
    fn default() -> Self {
        // As the lab's E1 Pro answered.
        BcState {
            user: "u".into(),
            pass: "p".into(),
            led_ctrl: 65,
            audio_version: 31,
            battery: false,
            channels: 1,
            dev_type: "wifi_solo_ipc".into(),
            ir_rw: true,
            ir: "auto".into(),
            light: "close".into(),
            noop_writes: false,
            fail_siren_stop: false,
            not_admin: false,
            refuse_login: false,
            logins: 0,
            connections: 0,
            open: 0,
            most_open: 0,
            log: Vec::new(),
            siren_timed_until: None,
            siren_manual: false,
            round_ms: 300,
            mute_play: false,
            login_delay_ms: 0,
        }
    }
}

pub struct FakeBc {
    pub port: u16,
    st: Arc<Mutex<BcState>>,
}

impl FakeBc {
    pub fn start(s: BcState) -> FakeBc {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let st = Arc::new(Mutex::new(s));
        let st2 = st.clone();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let st = st2.clone();
                std::thread::spawn(move || serve(c, &st));
            }
        });
        FakeBc { port, st }
    }
    pub fn state(&self) -> BcState {
        self.st.lock().unwrap().clone()
    }
    /// Sign-ins from now on answer after `ms`.
    pub fn slow_login(&self, ms: u64) {
        self.st.lock().unwrap().login_delay_ms = ms;
    }
    /// The manual switch flipped on the camera (as from the Reolink app).
    pub fn set_manual(&self, on: bool) {
        self.st.lock().unwrap().siren_manual = on;
    }
    /// The messages that change something (209, 263), in order.
    pub fn writes(&self) -> Vec<(u32, String)> {
        self.state().log.into_iter().filter(|(id, _)| bc::CONTROL_WRITES.contains(id)).collect()
    }
}

fn reply(c: &mut TcpStream, id: u32, num: u16, status: u16, class: u16, body: &[u8]) {
    let h = bc::Header { msg_id: id, body_len: body.len() as u32, channel: 0, stream: 0, msg_num: num, status, class, payload_offset: (class == 0).then_some(0) };
    let _ = c.write_all(&[h.encode(), body.to_vec()].concat());
}

fn serve(mut c: TcpStream, st: &Mutex<BcState>) {
    {
        let mut s = st.lock().unwrap();
        s.connections += 1;
        s.open += 1;
        s.most_open = s.most_open.max(s.open);
    }
    let _ = c.set_read_timeout(Some(Duration::from_millis(100)));
    let nonce = "F4KEN0NCE";
    let mut aes: Option<bc::Aes> = None;
    let (mut buf, mut tmp) = (Vec::new(), [0u8; 16384]);
    'conn: loop {
        match c.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(_) => break,
        }
        while let Ok(Some((used, m))) = bc::parse_msg(&buf) {
            buf.drain(..used);
            let h = &m.header;
            if h.msg_id == bc::MSG_LOGIN && h.class == bc::CLASS_LEGACY {
                let x = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Encryption version=\"1.1\">\n<type>md5</type>\n<nonce>{nonce}</nonce>\n</Encryption>\n</body>\n");
                reply(&mut c, bc::MSG_LOGIN, h.msg_num, 0xdd12, bc::CLASS_LEGACY, &bc::xor_crypt(0, x.as_bytes()));
                continue;
            }
            if h.msg_id == bc::MSG_LOGIN {
                let x = String::from_utf8_lossy(&bc::xor_crypt(0, &m.body)).into_owned();
                let mut s = st.lock().unwrap();
                s.logins += 1;
                let ok = !s.refuse_login
                    && bc::xml_tag(&x, "userName") == Some(bc::md5_31_upper(&format!("{}{nonce}", s.user)).as_str())
                    && bc::xml_tag(&x, "password") == Some(bc::md5_31_upper(&format!("{}{nonce}", s.pass)).as_str());
                if !ok {
                    drop(s);
                    reply(&mut c, bc::MSG_LOGIN, h.msg_num, 401, 0, &[]);
                    continue;
                }
                let dev = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<DeviceInfo version=\"1.1\">\n<type>{}</type>\n<channelNum>{}</channelNum>\n</DeviceInfo>\n</body>\n",
                    s.dev_type, s.channels
                );
                aes = Some(bc::Aes::new(nonce, &s.pass));
                let delay = s.login_delay_ms;
                drop(s);
                std::thread::sleep(Duration::from_millis(delay));
                reply(&mut c, bc::MSG_LOGIN, h.msg_num, 200, 0, &bc::xor_crypt(0, dev.as_bytes()));
                continue;
            }
            let Some(a) = &aes else { break 'conn };
            let off = (h.payload_offset.unwrap_or(0) as usize).min(m.body.len());
            let dec = |d: &[u8]| if d.is_empty() { String::new() } else { String::from_utf8_lossy(&a.decrypt(d)).into_owned() };
            let text = format!("{}{}", dec(&m.body[..off]), dec(&m.body[off..]));
            let mut s = st.lock().unwrap();
            s.log.push((h.msg_id, text.clone()));
            let answer: Option<(u16, String)> = match h.msg_id {
                bc::MSG_KEEPALIVE if h.status == 0 => Some((200, String::new())),
                bc::MSG_KEEPALIVE | bc::MSG_LOGOUT => None,
                bc::MSG_SUPPORT => Some((
                    200,
                    format!(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<Support version=\"1.1\">\n<channelNum>{}</channelNum>\n<audioAlarm>1</audioAlarm>\n<smartHome>\n<item>\n<name>googleHome</name>\n<ver>1</ver>\n</item>\n</smartHome>\n<item>\n<chnID>0</chnID>\n<battery>{}</battery>\n<audioVersion>{}</audioVersion>\n<ledCtrl>{}</ledCtrl>\n</item>\n</Support>\n</body>\n",
                        s.channels, u8::from(s.battery), s.audio_version, s.led_ctrl
                    ),
                )),
                bc::MSG_ABILITY => Some((
                    200,
                    format!(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<AbilityInfo version=\"1.1\">\n<userName>{}</userName>\n<image>\n<subModule>\n<channelId>0</channelId>\n<abilityValue>ispBasic_rw, {}</abilityValue>\n</subModule>\n</image>\n</AbilityInfo>\n</body>\n",
                        s.user, if s.ir_rw { "ledState_rw" } else { "ledState_ro" }
                    ),
                )),
                bc::MSG_LED_GET => Some((200, led_xml(&s))),
                bc::MSG_LED_SET if s.not_admin => Some((401, String::new())),
                bc::MSG_LED_SET => {
                    if !s.noop_writes {
                        if let Some(v) = ctl::text(&text, "state") {
                            s.ir = v.to_string();
                        }
                        if let Some(v) = ctl::text(&text, "lightState") {
                            s.light = v.to_string();
                        }
                    }
                    Some((200, String::new()))
                }
                bc::MSG_AUDIO_PLAY if s.not_admin => Some((401, String::new())),
                bc::MSG_AUDIO_PLAY => {
                    let mode = ctl::text(&text, "playMode").unwrap_or("");
                    let on = ctl::text(&text, "onOff") == Some("1");
                    if mode == "0" {
                        let rounds = ctl::text(&text, "playTimes").and_then(|v| v.parse::<u64>().ok()).unwrap_or(1);
                        s.siren_timed_until = Some(std::time::Instant::now() + Duration::from_millis(rounds * s.round_ms));
                        if s.mute_play { None } else { Some((200, String::new())) }
                    } else if !on && s.fail_siren_stop {
                        Some((400, String::new()))
                    } else {
                        // The manual switch; off also ends a timed play.
                        s.siren_manual = on;
                        if !on {
                            s.siren_timed_until = None;
                        }
                        Some((200, String::new()))
                    }
                }
                _ => Some((400, String::new())),
            };
            drop(s);
            if let Some((status, x)) = answer {
                let body = if x.is_empty() { Vec::new() } else { a.encrypt(x.as_bytes()) };
                let _ = c.write_all(&[bc::Header { msg_id: h.msg_id, body_len: body.len() as u32, channel: h.channel, stream: 0, msg_num: h.msg_num, status, class: 0, payload_offset: Some(0) }.encode(), body].concat());
            }
        }
    }
    st.lock().unwrap().open -= 1;
}

fn led_xml(s: &BcState) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<LedState version=\"1.1\">\n<channelId>0</channelId>\n<ledVersion>2</ledVersion>\n<state>{}</state>\n<lightState>{}</lightState>\n</LedState>\n</body>\n",
        s.ir, s.light
    )
}
