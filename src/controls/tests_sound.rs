//! The siren's sound against the fake Reolink cameras: the whole upload as
//! the official app sends it, switching the sound (the camera's settings
//! sent back with one field changed), refusals, cameras that cannot, a
//! camera behind a hub, and who may.

use super::fake::{Fake, FakeState};
use super::fake_bc::{BcState, FakeBc};
use super::siren_sound::{self, MAX_BYTES};
use super::*;
use crate::ingest::baichuan::bc;
use std::sync::Arc;

/// A camera of its own, all controls over port 9000.
fn camera(fake: &FakeBc) -> (Arc<Engine>, usize) {
    let e = Engine::for_test(toml::from_str("").unwrap());
    let h = e.add_test_camera("Den");
    let text = format!("name = \"Den\"\nurl = \"rtsp://127.0.0.1:554/Preview_01_sub\"\nusername = \"u\"\npassword = \"p\"\nbaichuan_port = {}\n[controls]\nscheme = \"baichuan\"\n", fake.port);
    let cfg: CameraConfig = toml::from_str(&text).unwrap();
    *h.cfg.write().unwrap() = cfg.clone();
    e.config.lock().unwrap().cameras[0] = cfg;
    {
        let mut st = h.state.write().unwrap();
        st.name = "Den".into();
        st.online = true;
    }
    (e, h.id)
}

/// Channel 1 of a hub: the other controls over its web API, the sound over port 9000.
fn hub_camera(web: &Fake, fake: &FakeBc) -> (Arc<Engine>, usize) {
    let e = Engine::for_test(toml::from_str("").unwrap());
    let h = e.add_test_camera("Porch");
    let text = format!(
        "name = \"Porch\"\nurl = \"rtsp://127.0.0.1:554/h264Preview_02_main\"\nusername = \"u\"\npassword = \"p\"\nbaichuan_port = {}\n[controls]\nscheme = \"http\"\nhttp_port = {}\n",
        fake.port, web.port
    );
    let cfg: CameraConfig = toml::from_str(&text).unwrap();
    *h.cfg.write().unwrap() = cfg.clone();
    e.config.lock().unwrap().cameras[0] = cfg;
    {
        let mut st = h.state.write().unwrap();
        st.name = "Porch".into();
        st.online = true;
    }
    (e, h.id)
}

fn me() -> Caller {
    Caller { user: "ann".into(), ip: "192.0.2.1".into(), source: "web" }
}

fn audit(e: &Engine) -> Vec<String> {
    e.db.recent_audit(20).unwrap().into_iter().map(|r| r.4).collect()
}

/// A sound as the camera takes it (not through ffmpeg).
fn some_adpcm(bytes: usize) -> Vec<u8> {
    (0..bytes).map(|i| (i * 7 % 251) as u8).collect()
}

fn driver_for(fake: &FakeBc) -> baichuan_drv::BaichuanDriver {
    baichuan_drv::BaichuanDriver::for_sound(&baichuan_drv::base_for("127.0.0.1", fake.port), "u", "p", 0, false).unwrap()
}

#[test]
fn a_sound_goes_as_the_app_sends_it() {
    let fake = FakeBc::start(BcState::default());
    let d = driver_for(&fake);
    let s = d.sound_state().unwrap();
    assert_eq!(s, baichuan_drv::SoundState { takes: true, custom: Some(false), stored_bytes: Some(0) });
    let data = some_adpcm(MAX_BYTES);
    d.sound_send(&data).unwrap();
    let st = fake.state();
    assert_eq!(st.stored, data, "stored whole");
    assert_eq!(st.audio_select, 1, "switched to it");
    let m = &st.sound_msgs;
    let pieces = MAX_BYTES.div_ceil(baichuan_drv::SOUND_PIECE);
    assert_eq!(pieces, 20);
    assert_eq!(m.len(), 1 + pieces + 1, "header, pieces, end");
    // The header: its size and format, encrypted like other control payloads.
    assert_eq!(m[0].0, bc::MSG_AUDIO_FILE_SEND);
    let head = String::from_utf8_lossy(&m[0].4).into_owned();
    assert!(head.contains("<audioFileInfo version=\"1.1\">\n<channelId>0</channelId>\n<fileSize>39936</fileSize>\n<fileFormat>adpcm</fileFormat>\n</audioFileInfo>"), "{head}");
    assert!(!m[0].3.contains("binaryData"));
    // The pieces: 2048 bytes each (the last shorter), as is, one message number.
    for (i, p) in m[1..=pieces].iter().enumerate() {
        assert_eq!(p.0, bc::MSG_AUDIO_FILE_SEND);
        assert!(p.3.contains("<binaryData>1</binaryData>\n<channelId>0</channelId>"), "{}", p.3);
        let want = &data[i * 2048..((i + 1) * 2048).min(data.len())];
        assert_eq!(p.4, want, "piece {i} sent as is");
        assert_eq!(p.1, m[0].1, "the header's message number");
        assert_eq!(p.2, m[0].2, "the header's channel byte");
    }
    assert_eq!(m[pieces].4.len(), MAX_BYTES - 19 * 2048);
    assert_eq!(m[pieces + 1].0, bc::MSG_AUDIO_FILE_DONE);
    assert!(m[pieces + 1].4.is_empty(), "the end has no payload");
    // Nothing else in between: the log from the header to the end is only 261s and the 262.
    let at = st.log.iter().position(|(id, _)| *id == bc::MSG_AUDIO_FILE_SEND).unwrap();
    let ids: Vec<u32> = st.log[at..at + pieces + 2].iter().map(|(id, _)| *id).collect();
    assert!(ids[..=pieces].iter().all(|id| *id == bc::MSG_AUDIO_FILE_SEND) && ids[pieces + 1] == bc::MSG_AUDIO_FILE_DONE, "{ids:?}");
    // Then the settings, read and sent back with the custom sound picked.
    let w: Vec<u32> = fake.writes().iter().map(|(id, _)| *id).filter(|id| *id == bc::MSG_AUDIO_CFG_SET).collect();
    assert_eq!(w.len(), 1);
    let s = d.sound_state().unwrap();
    assert_eq!((s.custom, s.stored_bytes), (Some(true), Some(MAX_BYTES as u64)));
}

#[test]
fn switching_sends_the_cameras_own_settings_back_with_one_field_changed() {
    let more = "<talkAndReplyVolume>80</talkAndReplyVolume>\n<visitorVolume>60</visitorVolume>\n<oddField>9</oddField>\n";
    let fake = FakeBc::start(BcState { audio_select: 1, audio_cfg_more: more.into(), ..Default::default() });
    let (e, id) = camera(&fake);
    let v = siren_sound::pick(&e, id, &me(), false).unwrap();
    assert_eq!(v["supported"], true, "{v}");
    assert_eq!(v["using"], "built_in", "{v}");
    assert_eq!(fake.state().audio_select, 0);
    let (_, x) = fake.writes().into_iter().find(|(id, _)| *id == bc::MSG_AUDIO_CFG_SET).unwrap();
    for f in ["<audioSelect>0</audioSelect>", "<volume>25</volume>", "<talkAndReplyVolume>80</talkAndReplyVolume>", "<visitorVolume>60</visitorVolume>", "<oddField>9</oddField>", "<audioListId>0</audioListId>", "<visitorLoudspeaker>0</visitorLoudspeaker>", "<timeout>0</timeout>", "<preAlarm>0</preAlarm>"] {
        assert!(x.contains(f), "{f} in {x}");
    }
    assert!(audit(&e).iter().any(|l| l == "Den: siren sound: switched to the built-in sound, ok"), "{:?}", audit(&e));
    std::thread::sleep(siren_sound::PICK_GAP);
    let v = siren_sound::pick(&e, id, &me(), true).unwrap();
    assert_eq!(v["using"], "custom", "{v}");
    // Too quick again: refused, nothing sent.
    let n = fake.writes().len();
    assert_eq!(siren_sound::pick(&e, id, &me(), false).unwrap_err().status, 429);
    assert_eq!(fake.writes().len(), n);
}

#[test]
fn a_switch_that_does_not_take_is_said() {
    let fake = FakeBc::start(BcState { noop_audio_cfg: true, ..Default::default() });
    let (e, id) = camera(&fake);
    let r = siren_sound::pick(&e, id, &me(), true).unwrap_err();
    assert!(r.msg.contains("did not change"), "{}", r.msg);
    assert!(audit(&e).iter().any(|l| l.starts_with("Den: siren sound: switched to the custom sound, failed")), "{:?}", audit(&e));
}

#[test]
fn a_piece_refused_stops_the_upload_and_says_which() {
    let fake = FakeBc::start(BcState { refuse_piece: Some(3), ..Default::default() });
    let d = driver_for(&fake);
    let e = d.sound_send(&some_adpcm(MAX_BYTES)).unwrap_err().to_string();
    assert!(e.contains("part 3 of 20") && e.contains("status 400"), "{e}");
    let st = fake.state();
    assert_eq!(st.sound_msgs.len(), 4, "the header and three pieces, nothing after");
    assert!(st.sound_msgs.iter().all(|m| m.0 == bc::MSG_AUDIO_FILE_SEND), "no end sent");
    assert!(!fake.writes().iter().any(|(id, _)| *id == bc::MSG_AUDIO_CFG_SET), "not switched");
    assert!(st.stored.is_empty() && st.audio_select == 0);
    // The session goes on: the next ask is answered.
    assert!(d.sound_state().is_ok());
}

#[test]
fn the_header_refused_is_said_too() {
    let fake = FakeBc::start(BcState { not_admin: true, ..Default::default() });
    let d = driver_for(&fake);
    let e = d.sound_send(&some_adpcm(4096)).unwrap_err().to_string();
    assert!(e.contains("may not take the sound's header") || e.contains("sign in to the camera with an admin account"), "{e}");
    assert_eq!(fake.state().sound_msgs.len(), 1);
}

#[test]
fn cameras_that_cannot_are_not_offered_and_get_nothing() {
    // No ADPCM in what it takes.
    let fake = FakeBc::start(BcState { sound_ok: false, ..Default::default() });
    let (e, id) = camera(&fake);
    let v = siren_sound::view(&e, id).unwrap();
    assert_eq!(v["supported"], false, "{v}");
    assert_eq!(siren_sound::pick(&e, id, &me(), true).unwrap_err().status, 409);
    assert!(fake.writes().is_empty());
    // No siren (a doorbell): not even asked.
    let fake = FakeBc::start(BcState { audio_version: 3, ..Default::default() });
    let (e, id) = camera(&fake);
    let v = siren_sound::view(&e, id).unwrap();
    assert_eq!((v["supported"].as_bool(), v["why"].as_str()), (Some(false), Some("This camera has no siren.")), "{v}");
    assert!(!fake.state().log.iter().any(|(id, _)| *id == bc::MSG_AUDIO_ENCODE_ABILITY));
    assert!(fake.writes().is_empty());
    // Not a Reolink: the words why, and the same shape.
    let e = Engine::for_test(toml::from_str("").unwrap());
    let h = e.add_test_camera("X");
    *h.cfg.write().unwrap() = toml::from_str("name = \"X\"\nurl = \"rtsp://192.0.2.9/stream1\"\n").unwrap();
    let v = siren_sound::view(&e, h.id).unwrap();
    assert_eq!(v["supported"], false);
    assert!(v["why"].is_string());
}

#[test]
fn the_state_is_shown_in_one_shape() {
    let fake = FakeBc::start(BcState { audio_select: 1, stored: vec![1; 39936], ..Default::default() });
    let (e, id) = camera(&fake);
    let v = siren_sound::view(&e, id).unwrap();
    assert_eq!(v["supported"], true, "{v}");
    assert_eq!(v["using"], "custom");
    assert_eq!(v["custom_bytes"], 39936);
    assert_eq!(v["hub"], false);
    assert_eq!(v["max_upload_bytes"], siren_sound::MAX_UPLOAD);
    assert!((v["max_seconds"].as_f64().unwrap() - 4.992).abs() < 1e-9);
    assert!(v["error"].is_null());
    assert!(fake.writes().is_empty(), "reading changes nothing");
}

#[test]
fn an_upload_is_read_checked_sent_and_audited() {
    if std::process::Command::new(crate::ffmpeg_bin::program()).arg("-version").output().is_err() {
        return;
    }
    let fake = FakeBc::start(BcState::default());
    let (e, id) = camera(&fake);
    // Not sound at all: refused before the camera is asked anything.
    let r = siren_sound::upload(&e, id, &me(), b"hello, not a sound").unwrap_err();
    assert_eq!(r.status, 400, "{}", r.msg);
    assert!(fake.state().sound_msgs.is_empty());
    // 2 s of 44.1 kHz mono: 16 kHz, brought up, a whole number of blocks.
    let v = siren_sound::upload(&e, id, &me(), &wav(44_100, 88_200, 3000.0)).unwrap();
    assert_eq!(v["using"], "custom", "{v}");
    let st = fake.state();
    assert_eq!(st.stored.len(), 16 * 1024, "2 s: 16000 bytes, up to a whole 1024");
    let y = siren_sound::decode(&st.stored);
    let peak = y.iter().map(|s| i32::from(*s).abs()).max().unwrap();
    assert!(peak > 25_000, "brought near full scale: {peak}");
    assert!(audit(&e).iter().any(|l| l.starts_with("Den: siren sound: custom sound sent (2.0 s, 16384 bytes) and switched to, ok")), "{:?}", audit(&e));
    // Again at once: too soon.
    assert_eq!(siren_sound::upload(&e, id, &me(), &wav(16_000, 16_000, 3000.0)).unwrap_err().status, 429);
    // No temp folder left behind.
    let left = std::fs::read_dir(std::env::temp_dir()).unwrap().flatten().filter(|d| d.file_name().to_string_lossy().starts_with(&format!("ambersight-siren-{}-", std::process::id()))).count();
    assert_eq!(left, 0);
}

/// A mono 16-bit WAV of a 440 Hz tone.
fn wav(rate: u32, n: u32, amp: f64) -> Vec<u8> {
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + n * 2).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(n * 2).to_le_bytes());
    for i in 0..n {
        let v = (amp * (f64::from(i) * 440.0 * std::f64::consts::TAU / f64::from(rate)).sin()) as i16;
        w.extend_from_slice(&v.to_le_bytes());
    }
    w
}

#[test]
fn behind_a_hub_only_while_awake_and_a_refusal_says_what_may_help() {
    let web = Fake::start(FakeState::default());
    let fake = FakeBc::start(BcState { dev_type: "homehub".into(), channels: 4, file_info_zero: true, ..Default::default() });
    let (e, id) = hub_camera(&web, &fake);
    let set = |s: Option<bool>| e.camera(id).unwrap().state.write().unwrap().device_sleeping = s;
    // Asleep: nothing asked on port 9000.
    set(Some(true));
    let v = siren_sound::view(&e, id).unwrap();
    assert_eq!((v["supported"].as_bool(), v["asleep"].as_bool()), (Some(false), Some(true)), "{v}");
    assert!(fake.state().log.is_empty(), "{:?}", fake.state().log);
    assert_eq!(siren_sound::pick(&e, id, &me(), true).unwrap_err().status, 409);
    // Awake: asked on the hub's channel 1; the stored size is not given (not reliable there).
    set(Some(false));
    let v = siren_sound::view(&e, id).unwrap();
    assert_eq!(v["supported"], true, "{v}");
    assert_eq!(v["hub"], true);
    assert!(v["custom_bytes"].is_null(), "{v}");
    assert!(!fake.state().log.iter().any(|(id, _)| *id == bc::MSG_AUDIO_FILE_INFO), "260 is not asked behind a hub");
    assert!(fake.state().log.iter().any(|(_, x)| x.contains("<channelId>1</channelId>")));
    // A refused piece: the words say the siren might be off.
    fake.set(|s| s.refuse_piece = Some(1));
    let d = baichuan_drv::BaichuanDriver::for_sound(&baichuan_drv::base_for("127.0.0.1", fake.port), "u", "p", 1, true).unwrap();
    let err = d.sound_send(&some_adpcm(3000)).unwrap_err();
    assert!(not_done(&err), "{err:#}");
    if std::process::Command::new(crate::ffmpeg_bin::program()).arg("-version").output().is_ok() {
        let r = siren_sound::upload(&e, id, &me(), &wav(16_000, 16_000, 3000.0)).unwrap_err();
        assert!(r.msg.contains(siren_sound::HUB_SIREN_HINT), "{}", r.msg);
        assert!(r.msg.contains("part 1 of"), "{}", r.msg);
    }
    // The other controls never went over port 9000 to the hub.
    assert!(!fake.state().log.iter().any(|(id, _)| *id == bc::MSG_LED_GET || *id == bc::MSG_AUDIO_PLAY));
}

#[test]
fn a_hub_channel_still_gets_no_other_control_over_port_9000() {
    // (Only the siren's sound may go to a hub's channel: the queue refuses the rest.)
    let fake = FakeBc::start(BcState { dev_type: "homehub".into(), channels: 4, ..Default::default() });
    let d = baichuan_drv::BaichuanDriver::at(&baichuan_drv::base_for("127.0.0.1", fake.port), "u", "p", 1).unwrap();
    let e = d.siren_play(5).unwrap_err().to_string();
    assert!(e.contains(crate::ingest::baichuan::ctl::NOT_FOR_HUB), "{e}");
    assert!(fake.writes().is_empty());
}

#[test]
fn who_may() {
    use super::http::{sound_refusal, sound_type_ok};
    use crate::auth::Role;
    assert_eq!(sound_refusal(&Role::Admin, false), None);
    assert!(sound_refusal(&Role::Admin, true).is_some(), "not with a device token");
    assert!(sound_refusal(&Role::Viewer, false).is_some());
    let all = crate::config::OperatorPerms { ptz: true, talk_clip: true, arm: true, events: true, camera_detection: true, lights: true, siren: true, reboot: true };
    assert!(sound_refusal(&Role::Operator(all), false).is_some(), "an operator with every right still may not");
    for ok in ["audio/wav", "audio/webm;codecs=opus", "audio/mp4", "audio/aac", "audio/mpeg", "audio/ogg", "AUDIO/X-WAV", "video/webm", "application/octet-stream"] {
        assert!(sound_type_ok(ok), "{ok}");
    }
    for no in ["", "text/plain", "application/json", "multipart/form-data; boundary=x", "image/png", "application/x-mpegurl"] {
        assert!(!sound_type_ok(no), "{no}");
    }
}

#[test]
fn nothing_is_sent_or_switched_while_the_siren_sounds() {
    let fake = FakeBc::start(BcState::default());
    let (e, id) = camera(&fake);
    // Sounding, then its time up but its stop not done yet: both refused.
    for until in [now_ms() + 20_000, now_ms() - 1_000] {
        e.controls.with("Den", |c| c.siren_until_ms = Some(until));
        let r = siren_sound::pick(&e, id, &me(), true).unwrap_err();
        assert_eq!(r.status, 409, "{}", r.msg);
        assert!(r.msg.contains("The siren is sounding"), "{}", r.msg);
        // (Before the file is read: also without ffmpeg, and for a file that is no sound at all.)
        let r = siren_sound::upload(&e, id, &me(), b"not a sound").unwrap_err();
        assert_eq!(r.status, 409, "{}", r.msg);
    }
    // A play on its way counts too.
    e.controls.with("Den", |c| {
        c.siren_until_ms = None;
        c.siren_flying = Some(1);
    });
    assert_eq!(siren_sound::pick(&e, id, &me(), true).unwrap_err().status, 409);
    assert!(fake.writes().is_empty(), "nothing reached the camera");
    // Stopped: switched.
    e.controls.with("Den", |c| c.siren_flying = None);
    assert_eq!(siren_sound::pick(&e, id, &me(), true).unwrap()["using"], "custom");
}

#[test]
fn the_hub_hint_only_for_a_battery_camera_behind_a_home_hub() {
    let web = Fake::start(FakeState::default());
    let fake = FakeBc::start(BcState { dev_type: "homehub".into(), channels: 4, ..Default::default() });
    let (e, id) = hub_camera(&web, &fake);
    let h = e.camera(id).unwrap();
    let t = target(&h.cfg.read().unwrap()).unwrap();
    // A wired camera on a channel (the hub says nothing of sleeping for it): no hint.
    assert!(!battery_behind_hub(&h, &t));
    h.state.write().unwrap().device_sleeping = Some(false);
    assert!(battery_behind_hub(&h, &t), "the hub reports its sleep: a battery camera");
    // A camera of its own: never.
    let fake2 = FakeBc::start(BcState::default());
    let (e2, id2) = camera(&fake2);
    let h2 = e2.camera(id2).unwrap();
    h2.state.write().unwrap().device_sleeping = Some(false);
    assert!(!battery_behind_hub(&h2, &target(&h2.cfg.read().unwrap()).unwrap()));
}

/// A body over the route's limit reaches the handler as a 413 rejection
/// (so `sound_post` answers it in JSON).
#[tokio::test]
async fn a_body_over_the_limit_is_a_413_rejection() {
    use axum::extract::rejection::BytesRejection;
    async fn h(b: Result<axum::body::Bytes, BytesRejection>) -> String {
        match b {
            Ok(b) => format!("ok {}", b.len()),
            Err(r) => format!("rejected {}", r.status().as_u16()),
        }
    }
    let app = axum::Router::new().route("/x", axum::routing::post(h)).layer(axum::extract::DefaultBodyLimit::max(10));
    for (n, want) in [(10, "ok 10"), (11, "rejected 413"), (5000, "rejected 413")] {
        let req = axum::http::Request::post("/x").body(axum::body::Body::from(vec![0u8; n])).unwrap();
        let r = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
        let b = axum::body::to_bytes(r.into_body(), 1000).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&b), want);
    }
}

#[test]
fn a_read_does_not_keep_the_session_a_change_does() {
    let fake = FakeBc::start(BcState::default());
    let d = driver_for(&fake);
    let key = format!("127.0.0.1:{}", fake.port);
    d.sound_state().unwrap();
    assert!(!crate::ingest::baichuan::ctl::recent(&key), "a page's read signs out like an idle stream");
    d.sound_pick(true).unwrap();
    assert!(crate::ingest::baichuan::ctl::recent(&key));
}
