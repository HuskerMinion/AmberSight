//! A fake Reolink on Baichuan TCP for tests: signs in (BC/XOR, so no AES is
//! needed here), streams a small H.264 preview, answers TalkAbility and
//! TalkConfig (or refuses it), and records every message it gets, the talk
//! audio included. Then the whole path is checked through the real session
//! thread: one sign-in shared by video and talk, audio that decodes back to
//! what went in, and a refused talk that leaves the video alone.

use super::bc;
use super::talk::{self, TalkEncoder};
use super::wired::{self, StreamKind};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default, Debug)]
pub struct Log {
    pub logins: usize,
    /// Every message id received, in order.
    pub ids: Vec<u32>,
    pub abilities: usize,
    pub configs: Vec<String>,
    /// Talk messages: (message number, extension XML, payload).
    pub talk: Vec<(u16, String, Vec<u8>)>,
    pub previews: usize,
}

#[derive(Clone)]
pub struct Opts {
    pub refuse_talk: bool,
    pub battery: bool,
    pub ability: String,
}

pub struct FakeCam {
    pub port: u16,
    pub log: Arc<Mutex<Log>>,
    stop: Arc<AtomicBool>,
}

impl Drop for FakeCam {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn reply(id: u32, ch: u8, st: u8, num: u16, status: u16, body: &[u8]) -> Vec<u8> {
    let h = bc::Header { msg_id: id, body_len: body.len() as u32, channel: ch, stream: st, msg_num: num, status, class: 0x0000, payload_offset: Some(0) };
    [h.encode(), body.to_vec()].concat()
}

fn nal(b: &[u8]) -> Vec<u8> {
    [&[0, 0, 0, 1][..], b].concat()
}

fn vframe(key: bool, micros: u32) -> Vec<u8> {
    let annexb = if key {
        [nal(&[0x67, 0x64, 0, 0x1f, 0xac, 0xd9]), nal(&[0x68, 0xeb, 0xe3]), nal(&[0x65, 0x88, 0x84, 0x10])].concat()
    } else {
        nal(&[0x41, 0x9a, 1, 0x20])
    };
    let mut v = Vec::new();
    v.extend_from_slice(if key { b"00dc" } else { b"01dc" });
    v.extend_from_slice(b"H264");
    v.extend_from_slice(&(annexb.len() as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&micros.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&annexb);
    while v.len() % 8 != 0 {
        v.push(0);
    }
    v
}

impl FakeCam {
    pub fn start(opts: Opts) -> FakeCam {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        l.set_nonblocking(true).unwrap();
        let log: Arc<Mutex<Log>> = Default::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (log2, stop2) = (log.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match l.accept() {
                    Ok((s, _)) => {
                        let (log, stop, opts) = (log2.clone(), stop2.clone(), opts.clone());
                        std::thread::spawn(move || serve(s, log, stop, opts));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        FakeCam { port, log, stop }
    }
}

fn serve(s: TcpStream, log: Arc<Mutex<Log>>, stop: Arc<AtomicBool>, opts: Opts) {
    s.set_nonblocking(false).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    let out = Arc::new(Mutex::new(s.try_clone().unwrap()));
    let mut s = s;
    let mut buf = Vec::new();
    let mut tmp = vec![0u8; 65536];
    // The running preview: (num, channel, stream), next frame time, frame count.
    let mut preview: Option<(u16, u8, u8)> = None;
    let mut next_frame = Instant::now();
    let mut frame = 0u32;
    let send = |b: Vec<u8>| {
        let _ = out.lock().unwrap().write_all(&b);
    };
    while !stop.load(Ordering::Relaxed) {
        match s.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => {}
        }
        while let Ok(Some((used, m))) = bc::parse_msg(&buf) {
            buf.drain(..used);
            let h = m.header.clone();
            log.lock().unwrap().ids.push(h.msg_id);
            let off = (h.payload_offset.unwrap_or(0) as usize).min(m.body.len());
            match h.msg_id {
                bc::MSG_LOGIN if h.class == bc::CLASS_LEGACY => {
                    // Pick BC/XOR (status dd00): the nonce, XOR-encrypted.
                    let x = bc::xor_crypt(0, b"<?xml version=\"1.0\"?><body><Encryption version=\"1.1\"><type>md5</type><nonce>FAKE1</nonce></Encryption></body>");
                    let hh = bc::Header { msg_id: 1, body_len: x.len() as u32, channel: 0, stream: 0, msg_num: h.msg_num, status: 0xdd00, class: bc::CLASS_LEGACY, payload_offset: None };
                    send([hh.encode(), x].concat());
                }
                bc::MSG_LOGIN => {
                    log.lock().unwrap().logins += 1;
                    let bat = if opts.battery { "<BatteryList><BatteryInfo><batteryPercent>80</batteryPercent></BatteryInfo></BatteryList>" } else { "" };
                    let x = format!("<?xml version=\"1.0\"?><body><DeviceInfo version=\"1.1\"><type>ipc</type><channelNum>1</channelNum><firmVersion>fake</firmVersion></DeviceInfo>{bat}</body>");
                    send(reply(1, 0, 0, h.msg_num, 200, &bc::xor_crypt(0, x.as_bytes())));
                }
                bc::MSG_LOGOUT => return,
                bc::MSG_PREVIEW => {
                    log.lock().unwrap().previews += 1;
                    preview = Some((h.msg_num, h.channel, h.stream));
                    next_frame = Instant::now();
                    frame = 0;
                }
                bc::MSG_PREVIEW_STOP => preview = None,
                bc::MSG_KEEPALIVE if h.status == 0 => send(reply(bc::MSG_KEEPALIVE, h.channel, 0, h.msg_num, 200, b"")),
                bc::MSG_TALK_ABILITY => {
                    log.lock().unwrap().abilities += 1;
                    send(reply(bc::MSG_TALK_ABILITY, h.channel, 0, h.msg_num, 200, &bc::xor_crypt(h.channel as u32, opts.ability.as_bytes())));
                }
                bc::MSG_TALK_CONFIG => {
                    let x = String::from_utf8_lossy(&bc::xor_crypt(h.channel as u32, &m.body[off..])).into_owned();
                    log.lock().unwrap().configs.push(x);
                    // Talk set-up takes a moment on a real camera.
                    std::thread::sleep(Duration::from_millis(300));
                    send(reply(bc::MSG_TALK_CONFIG, h.channel, 0, h.msg_num, if opts.refuse_talk { 400 } else { 200 }, b""));
                }
                bc::MSG_TALK => {
                    let ext = String::from_utf8_lossy(&bc::xor_crypt(h.channel as u32, &m.body[..off])).into_owned();
                    log.lock().unwrap().talk.push((h.msg_num, ext, m.body[off..].to_vec()));
                }
                _ => {}
            }
        }
        if let Some((num, ch, st)) = preview {
            while Instant::now() >= next_frame {
                let key = frame % 30 == 0;
                send(reply(bc::MSG_PREVIEW, ch, st, num, 200, &vframe(key, frame * 66_666)));
                frame += 1;
                next_frame += Duration::from_micros(66_666);
            }
        }
    }
}

fn ability() -> String {
    talk::tests::ability_xml(16000, 1024)
}

/// Feed `secs` of 48 kHz PCM (silence, then a 1 kHz tone) in real time.
fn feed(h: &talk::TalkHandle, silence_s: f64, tone_s: f64) {
    let mut enc = TalkEncoder::new(48000, &h.format);
    let total = ((silence_s + tone_s) * 48000.0) as usize;
    let start = Instant::now();
    let mut i = 0;
    while i < total {
        let n = 960.min(total - i);
        let chunk: Vec<i16> = (i..i + n)
            .map(|k| if (k as f64) < silence_s * 48000.0 { 0 } else { (8000.0 * (2.0 * std::f64::consts::PI * 1000.0 * k as f64 / 48000.0).sin()) as i16 })
            .collect();
        i += n;
        for b in enc.push_samples(&chunk) {
            assert!(h.send(b), "the session went away");
        }
        let due = start + Duration::from_micros((i as u64) * 1_000_000 / 48000);
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
}

fn video_items(p: &mut wired::WiredPreview) -> usize {
    let mut n = 0;
    while let Ok(Ok(crate::ingest::recorder::MediaItem::Video { .. })) = p.rx.try_recv() {
        n += 1;
    }
    n
}

#[test]
fn talk_goes_over_the_shared_session_and_decodes_back_to_the_input() {
    let cam = FakeCam::start(Opts { refuse_talk: false, battery: false, ability: ability() });
    let c = wired::client_for("127.0.0.1", cam.port, "admin", "pw", "Fake").unwrap();
    let mut pv = c.start(0, StreamKind::Sub, "Fake").expect("preview");
    let h = c.talk_open(0).expect("talk");
    assert_eq!((h.format.rate, h.format.block_samples), (16000, 1024), "read from the camera");
    // 1 s of digital silence (opens nothing), then 3 s of a 1 kHz tone.
    feed(&h, 1.0, 3.0);
    std::thread::sleep(Duration::from_millis(3000));
    let v = video_items(&mut pv);
    assert!(v > 20, "video went on during talk: {v} frames");
    let stats = h.stats.clone();
    drop(h);
    let log = cam.log.lock().unwrap();
    assert_eq!(log.logins, 1, "one session for video and talk");
    assert_eq!(log.abilities, 1);
    assert_eq!(log.configs.len(), 1, "one talk opened (silence opened none)");
    let cfg = &log.configs[0];
    assert!(cfg.contains("<TalkConfig version=\"1.1\">") && cfg.contains("<sampleRate>16000</sampleRate>") && cfg.contains("<lengthPerEncoder>1024</lengthPerEncoder>"), "{cfg}");
    // Talk audio came after TalkConfig only.
    let first_cfg = log.ids.iter().position(|&i| i == bc::MSG_TALK_CONFIG).unwrap();
    assert!(log.ids[..first_cfg].iter().all(|&i| i != bc::MSG_TALK));
    assert!(!log.talk.is_empty());
    let mut pcm = Vec::new();
    for (_, ext, pay) in &log.talk {
        assert!(ext.contains("<binaryData>1</binaryData>") && ext.contains("<channelId>0</channelId>"), "{ext}");
        let mut at = 0;
        while at < pay.len() {
            let (s, used) = talk::parse_block(&pay[at..]).expect("a whole ADPCM block");
            assert_eq!(u16::from_le_bytes([pay[at + 10], pay[at + 11]]), 512);
            pcm.extend(s);
            at += used;
        }
    }
    // Up to ~3 s of tone minus the 1-2 s set-up (dropped) and the release.
    let secs = pcm.len() as f64 / 16000.0;
    assert!((1.0..=3.1).contains(&secs), "{secs:.2} s of talk audio");
    let snr = talk::tests::sine_snr(&pcm[256..], 1000.0, 16000.0);
    eprintln!("fake camera: {secs:.2} s of talk audio, {} talk messages, decoded SNR {snr:.1} dB", log.talk.len());
    assert!(snr > 20.0, "decoded talk SNR {snr:.1} dB");
    assert_eq!(stats.releases.load(Ordering::Relaxed), 1, "let go after the sound stopped");
    assert_eq!(stats.refusals.load(Ordering::Relaxed), 0);
    drop(log);
    drop(pv);
    c.shutdown();
}

#[test]
fn a_refused_talk_leaves_the_video_and_session_alone() {
    let cam = FakeCam::start(Opts { refuse_talk: true, battery: false, ability: ability() });
    let c = wired::client_for("127.0.0.1", cam.port, "admin", "pw", "Fake").unwrap();
    let mut pv = c.start(0, StreamKind::Main, "Fake").expect("preview");
    let h = c.talk_open(0).expect("talk");
    video_items(&mut pv);
    feed(&h, 0.0, 6.5);
    let v = video_items(&mut pv);
    assert!(v >= 80, "video kept coming through the refusals: {v} frames in 6.5 s");
    assert!(h.ended().is_none(), "the talk handle is still usable");
    let st = &h.stats;
    let (refused, warned) = (st.refusals.load(Ordering::Relaxed), st.warnings.load(Ordering::Relaxed));
    assert_eq!(refused, 2, "refused, 5 s back-off, refused again");
    assert_eq!(warned, 2, "warned at most once per 5 s");
    let log = cam.log.lock().unwrap();
    assert_eq!(log.logins, 1, "never signed in again");
    assert_eq!(log.previews, 1, "the preview was never restarted");
    assert_eq!(log.configs.len(), 2);
    assert!(log.talk.is_empty(), "no audio after a refusal");
    let status = wired::status("127.0.0.1", cam.port).unwrap();
    assert_eq!((status.sessions, status.signed_in), (1, true));
    drop(log);
    drop(h);
    drop(pv);
    c.shutdown();
}

#[test]
fn a_camera_reporting_a_battery_gets_no_talk() {
    let cam = FakeCam::start(Opts { refuse_talk: false, battery: true, ability: ability() });
    let c = wired::client_for("127.0.0.1", cam.port, "admin", "pw", "Fake").unwrap();
    let e = c.talk_open(0).err().expect("refused");
    assert!(e.contains("battery"), "{e}");
    std::thread::sleep(Duration::from_millis(200));
    let log = cam.log.lock().unwrap();
    assert_eq!(log.abilities, 0, "not even the talk question");
    assert!(log.ids.iter().all(|&i| i != bc::MSG_TALK_CONFIG && i != bc::MSG_TALK));
    drop(log);
    c.shutdown();
}
