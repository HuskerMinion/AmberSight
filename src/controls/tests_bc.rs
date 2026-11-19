//! Camera controls over Reolink port 9000 (Baichuan) against a fake camera
//! (fake_bc.rs): the same rules as over the web API (tests.rs), through the
//! device's one shared session.

use super::fake_bc::{BcState, FakeBc};
use crate::ingest::baichuan::bc;
use super::*;
use std::sync::Arc;

fn engine_with(fake: &FakeBc, extra: &str, controls: &str) -> (Arc<Engine>, usize) {
    let e = Engine::for_test(toml::from_str("").unwrap());
    let h = e.add_test_camera("Den");
    let text = format!(
        "name = \"Den\"\nurl = \"rtsp://127.0.0.1:554/Preview_01_sub\"\nusername = \"u\"\npassword = \"p\"\nbaichuan_port = {}\n{extra}\n[controls]\n{controls}\n",
        fake.port
    );
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

fn picked(fake: &FakeBc) -> (Arc<Engine>, usize) {
    engine_with(fake, "", "scheme = \"baichuan\"")
}

fn me() -> Caller {
    Caller { user: "ann".into(), ip: "192.0.2.1".into(), source: "web" }
}

fn all() -> May {
    May { light: true, siren: true, settings: true }
}

fn tick_until(e: &Arc<Engine>, f: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until {
        tick(e);
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out");
}

fn audit(e: &Engine) -> Vec<String> {
    e.db.recent_audit(20).unwrap().into_iter().map(|r| r.4).collect()
}

#[test]
fn an_e1_pro_has_its_siren_infrared_and_status_light_over_port_9000() {
    let fake = FakeBc::start(BcState::default());
    let (e, id) = picked(&fake);
    let v = view(&e, id, all()).unwrap();
    assert_eq!(v["supported"], true, "{v}");
    assert_eq!(v["via"], "port9000", "{v}");
    assert!(v["error"].is_null(), "{v}");
    assert!(v["light"].is_null() && v["light_unswitched"] == false, "{v}");
    assert!(v["siren"].is_object(), "{v}");
    assert_eq!(v["ir"]["states"], serde_json::json!(["Auto", "Off"]));
    assert_eq!(v["status_led"]["states"], serde_json::json!(["On", "Off"]));
    assert_eq!(v["state"]["ir"], "Auto");
    assert_eq!(v["state"]["status_led"], "Off");
    assert_eq!(v["state"]["siren"], "unknown", "the siren's state is never guessed");
    assert!(fake.writes().is_empty(), "asking changes nothing");
}

#[test]
fn the_siren_is_capped_rate_limited_audited_and_stopped() {
    let fake = FakeBc::start(BcState::default());
    let (e, id) = picked(&fake);
    let v = act(&e, id, &me(), Action::Siren { seconds: Some(999) }, false).unwrap();
    assert_eq!(v["state"]["siren"], "playing", "{v}");
    assert!(v["state"]["siren_until_ms"].as_i64().unwrap() <= now_ms() + i64::from(SIREN_MAX_SECS) * 1000, "capped");
    let w = fake.writes();
    assert_eq!(w.len(), 1);
    let (id263, x) = &w[0];
    assert_eq!(*id263, bc::MSG_AUDIO_PLAY);
    // Rounds, so the camera stops by itself (at most 3 of up to 10 s).
    assert!(x.contains("<playMode>0</playMode>") && x.contains("<playTimes>3</playTimes>") && x.contains("<onOff>1</onOff>") && x.contains("<channelId>0</channelId>"), "{x}");
    assert!(fake.state().playing());
    assert_eq!(act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap_err().status, 429);
    act(&e, id, &me(), Action::SirenStop, false).unwrap();
    assert!(!fake.state().playing());
    let (_, x) = fake.writes().last().cloned().unwrap();
    assert!(x.contains("<playMode>2</playMode>") && x.contains("<onOff>0</onOff>"), "{x}");
    let a = audit(&e);
    assert!(a.iter().any(|l| l == "Den: siren for 30 s, ok"), "{a:?}");
    assert!(a.iter().any(|l| l == "Den: siren stopped, ok"), "{a:?}");
    assert_eq!(fake.state().connections, 1, "one session for everything");
}

#[test]
fn the_siren_stops_when_its_time_is_up() {
    let fake = FakeBc::start(BcState { round_ms: 10_000, ..Default::default() });
    let (e, id) = picked(&fake);
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap();
    assert!(fake.state().playing());
    std::thread::sleep(Duration::from_millis(1100));
    tick_until(&e, || !fake.state().playing());
    tick_until(&e, || e.controls.with("Den", |c| c.siren_until_ms.is_none()));
    assert!(e.db.ui_get(SCOPE, "siren|Den").unwrap().is_none());
}

#[test]
fn a_siren_stop_that_fails_is_tried_again_then_given_up() {
    let fake = FakeBc::start(BcState { fail_siren_stop: true, ..Default::default() });
    let (e, id) = picked(&fake);
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap();
    let err = act(&e, id, &me(), Action::SirenStop, false).unwrap_err();
    assert!(err.msg.contains("did not stop its siren"), "{}", err.msg);
    tick_until(&e, || e.controls.with("Den", |c| c.siren_until_ms.is_none()));
    let stops = fake.writes().iter().filter(|(_, x)| x.contains("<onOff>0</onOff>")).count();
    assert!((2..=6).contains(&stops), "{stops} stops tried");
}

#[test]
fn infrared_and_status_light_change_one_field_and_are_read_back() {
    let fake = FakeBc::start(BcState::default());
    let (e, id) = picked(&fake);
    let v = act(&e, id, &me(), Action::Ir { state: "off".into() }, false).unwrap();
    assert_eq!(v["state"]["ir"], "Off", "{v}");
    let (wid, x) = fake.writes().last().cloned().unwrap();
    assert_eq!(wid, bc::MSG_LED_SET);
    // The camera's own state sent back with only that field changed.
    assert!(x.contains("<LedState version=\"1.1\">") && x.contains("<state>close</state>") && x.contains("<lightState>close</lightState>") && x.contains("<ledVersion>2</ledVersion>"), "{x}");
    let v = act(&e, id, &me(), Action::StatusLed { state: "On".into() }, false).unwrap();
    assert_eq!(v["state"]["status_led"], "On");
    let s = fake.state();
    assert_eq!((s.ir.as_str(), s.light.as_str()), ("close", "open"));
    // Only listed values.
    assert_eq!(act(&e, id, &me(), Action::Ir { state: "On".into() }, false).unwrap_err().status, 400);
    assert_eq!(act(&e, id, &me(), Action::StatusLed { state: "Blink".into() }, false).unwrap_err().status, 400);
    assert_eq!(fake.writes().len(), 2);
    let a = audit(&e);
    assert!(a.iter().any(|l| l == "Den: infrared lights Off, ok"), "{a:?}");
}

#[test]
fn a_change_the_camera_does_not_make_is_reported() {
    let fake = FakeBc::start(BcState { noop_writes: true, ..Default::default() });
    let (e, id) = picked(&fake);
    let err = act(&e, id, &me(), Action::StatusLed { state: "On".into() }, false).unwrap_err();
    assert!(err.msg.contains("nothing changed"), "{}", err.msg);
}

#[test]
fn an_account_that_may_not_is_told_so() {
    let fake = FakeBc::start(BcState { not_admin: true, ..Default::default() });
    let (e, id) = picked(&fake);
    let err = act(&e, id, &me(), Action::Siren { seconds: Some(2) }, false).unwrap_err();
    assert!(err.msg.contains("admin account"), "{}", err.msg);
    assert!(e.controls.with("Den", |c| c.siren_until_ms.is_none()), "a siren that did not start is not waited for");
    // A read-only infrared right: not offered.
    let fake = FakeBc::start(BcState { ir_rw: false, ..Default::default() });
    let (e, id) = picked(&fake);
    assert!(view(&e, id, all()).unwrap()["ir"].is_null());
}

#[test]
fn a_spotlight_is_said_but_not_switched_over_port_9000() {
    // As the lab's E1 Outdoor SE answered: ledCtrl 7.
    let fake = FakeBc::start(BcState { led_ctrl: 7, light: "open".into(), ..Default::default() });
    let (e, id) = picked(&fake);
    let v = view(&e, id, all()).unwrap();
    assert!(v["light"].is_null() && v["light_unswitched"] == true, "{v}");
    assert_eq!(act(&e, id, &me(), Action::LightOn { seconds: Some(5), brightness: None }, false).unwrap_err().status, 409);
    assert!(fake.writes().is_empty());
}

#[test]
fn a_refused_sign_in_is_never_repeated() {
    let fake = FakeBc::start(BcState { refuse_login: true, ..Default::default() });
    let (e, id) = picked(&fake);
    let v = view(&e, id, all()).unwrap();
    assert!(v["error"].as_str().unwrap().contains("sign-in refused"), "{v}");
    let _ = act(&e, id, &me(), Action::Redetect, false);
    let _ = view(&e, id, all());
    assert_eq!(fake.state().logins, 1);
}

#[test]
fn battery_cameras_and_hub_channels_get_nothing_and_are_never_asked() {
    let fake = FakeBc::start(BcState::default());
    let (e, id) = engine_with(&fake, "on_demand = true", "scheme = \"baichuan\"");
    let v = view(&e, id, all()).unwrap();
    assert_eq!(v["supported"], false);
    assert!(v["why"].as_str().unwrap().contains("battery"), "{v}");
    // A battery camera by its own word (199).
    let fake2 = FakeBc::start(BcState { battery: true, ..Default::default() });
    let (e2, id2) = picked(&fake2);
    let v = view(&e2, id2, all()).unwrap();
    assert!(v["error"].as_str().unwrap().contains("battery"), "{v}");
    assert!(fake2.writes().is_empty());
    // A device with more channels (a hub or recorder).
    let fake3 = FakeBc::start(BcState { channels: 4, dev_type: "homehub".into(), ..Default::default() });
    let (e3, id3) = picked(&fake3);
    let v = view(&e3, id3, all()).unwrap();
    assert!(v["error"].as_str().unwrap().contains("hub or recorder"), "{v}");
    assert!(fake3.state().log.iter().all(|(m, _)| *m == bc::MSG_KEEPALIVE || *m == bc::MSG_LOGOUT), "nothing asked of it: {:?}", fake3.state().log);
    assert_eq!(fake.state().connections, 0, "the battery camera was never reached");
}

#[test]
fn auto_reaches_port_9000_when_the_web_interface_is_off() {
    let closed = crate::test_ports::closed_port();
    let fake = FakeBc::start(BcState::default());
    let (e, id) = engine_with(&fake, "connect = \"auto\"", &format!("http_port = {}", closed.port));
    let v = view(&e, id, all()).unwrap();
    assert_eq!(v["via"], "port9000", "{v}");
    assert!(v["siren"].is_object(), "{v}");
}

#[test]
fn which_way_controls_go() {
    let cam = |extra: &str, url: &str| -> CameraConfig { toml::from_str(&format!("name = \"C\"\nurl = \"{url}\"\nusername = \"u\"\npassword = \"p\"\n{extra}")).unwrap() };
    let u = "rtsp://192.0.2.5:554/h264Preview_01_main";
    let t = target(&cam("[controls]\nscheme = \"baichuan\"", u)).unwrap();
    assert_eq!(t.bases, vec!["baichuan-tcp://192.0.2.5:9000".to_string()]);
    assert!(!t.ch0 && t.timers_ok());
    let t = target(&cam("baichuan_port = 9100\nconnect = \"baichuan\"", u)).unwrap();
    assert_eq!(t.bases, vec!["baichuan-tcp://192.0.2.5:9100".to_string()]);
    let t = target(&cam("connect = \"auto\"", u)).unwrap();
    assert_eq!(t.bases, vec!["https://192.0.2.5".to_string(), "baichuan-tcp://192.0.2.5:9000".to_string()]);
    // HTTPS picked: only HTTPS.
    let t = target(&cam("connect = \"auto\"\n[controls]\nscheme = \"https\"", u)).unwrap();
    assert_eq!(t.bases, vec!["https://192.0.2.5".to_string()]);
    let t = target(&cam("", u)).unwrap();
    assert_eq!(t.bases, vec!["https://192.0.2.5".to_string()], "RTSP cameras keep HTTPS");
    // Never for a battery camera or a hub's channel.
    assert!(target(&cam("on_demand = true\n[controls]\nscheme = \"baichuan\"", u)).unwrap_err().contains("battery"));
    assert!(target(&cam("[controls]\nscheme = \"baichuan\"", "rtsp://192.0.2.5/h264Preview_02_main")).unwrap_err().contains("hub"));
    let t = target(&cam("connect = \"auto\"", "rtsp://192.0.2.5/h264Preview_02_main")).unwrap();
    assert_eq!(t.bases, vec!["https://192.0.2.5".to_string()]);
    let t = target(&cam("connect = \"auto\"\non_demand = true", u)).unwrap();
    assert_eq!(t.bases, vec!["https://192.0.2.5".to_string()]);
    assert!(check_settings(&cam("[controls]\nscheme = \"baichuan\"", u)).is_ok());
}

// ---- review fixes (S1-S5, N3, N5)

fn set_cfg(e: &Engine, id: usize, f: impl FnOnce(&mut CameraConfig)) {
    let h = e.camera(id).unwrap();
    let mut c = h.cfg.write().unwrap();
    f(&mut c);
}

#[test]
fn s1_after_an_address_change_the_siren_stop_goes_only_to_the_new_one() {
    // Over port 9000: two fake cameras.
    let old = FakeBc::start(BcState { round_ms: 10_000, ..Default::default() });
    let new = FakeBc::start(BcState::default());
    let (e, id) = picked(&old);
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap();
    let (logins, msgs) = (old.state().logins, old.state().log.len());
    set_cfg(&e, id, |c| c.baichuan_port = Some(new.port));
    std::thread::sleep(Duration::from_millis(1100));
    tick_until(&e, || new.writes().iter().any(|(_, x)| x.contains("<onOff>0</onOff>")));
    let o = old.state();
    assert_eq!((o.logins, o.connections), (logins, 1), "no sign-in at the old address");
    assert!(o.log[msgs..].iter().all(|(m, _)| *m == bc::MSG_KEEPALIVE), "nothing sent to the old address: {:?}", &o.log[msgs..]);
    // The same over the web API.
    use super::fake::{Fake, FakeState};
    let a = Fake::start(FakeState::default());
    let b = Fake::start(FakeState::default());
    // (A camera of its own by its settings: no channel in its address.)
    let (e, id) = engine_with(&old, "", &format!("kind = \"reolink\"\nscheme = \"http\"\nhttp_port = {}", a.port));
    set_cfg(&e, id, |c| c.url = "rtsp://127.0.0.1:554/live".into());
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap();
    let n = a.state().reqs;
    set_cfg(&e, id, |c| c.controls.as_mut().unwrap().http_port = Some(b.port));
    std::thread::sleep(Duration::from_millis(1100));
    tick_until(&e, || b.state().log.iter().any(|l| l.contains("\"manu\"")));
    assert_eq!(a.state().reqs, n, "nothing sent to the old web address: {:?}", a.state().log);
}

#[test]
fn s2_a_siren_whose_answer_is_lost_may_be_playing_and_is_still_stopped() {
    let fake = FakeBc::start(BcState { mute_play: true, round_ms: 10_000, ..Default::default() });
    let (e, id) = picked(&fake);
    let err = act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap_err();
    assert!(err.msg.contains("may be sounding"), "{}", err.msg);
    assert!(fake.state().playing(), "the camera did sound it");
    assert!(e.controls.with("Den", |c| c.siren_until_ms.is_some()), "its stop is still due");
    assert!(e.db.ui_get(SCOPE, "siren|Den").unwrap().is_some(), "and survives a restart");
    let a = audit(&e);
    assert!(a.iter().any(|l| l.contains("siren for 1 s (may be playing") && l.contains("failed")), "{a:?}");
    tick_until(&e, || !fake.state().playing());
    assert!(fake.writes().iter().any(|(_, x)| x.contains("<onOff>0</onOff>")));
    // A clear refusal: not playing, nothing left due.
    let fake = FakeBc::start(BcState { not_admin: true, ..Default::default() });
    let (e, id) = picked(&fake);
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap_err();
    assert!(e.controls.with("Den", |c| c.siren_until_ms.is_none()));
    assert!(e.db.ui_get(SCOPE, "siren|Den").unwrap().is_none());
}

#[test]
fn s3_a_timed_siren_ends_by_itself_even_when_every_stop_fails() {
    let fake = FakeBc::start(BcState { fail_siren_stop: true, round_ms: 200, ..Default::default() });
    let (e, id) = picked(&fake);
    act(&e, id, &me(), Action::Siren { seconds: Some(30) }, false).unwrap();
    let (_, x) = fake.writes().last().cloned().unwrap();
    assert!(x.contains("<playTimes>3</playTimes>"), "30 s is at most 3 rounds: {x}");
    assert!(fake.state().playing());
    assert!(act(&e, id, &me(), Action::SirenStop, false).is_err());
    std::thread::sleep(Duration::from_millis(700));
    assert!(!fake.state().playing(), "3 rounds, then quiet");
    // The manual switch is another thing: on until switched off.
    fake.set_manual(true);
    std::thread::sleep(Duration::from_millis(700));
    assert!(fake.state().playing());
}

#[test]
fn s4_one_device_one_key_however_its_ipv6_address_is_written() {
    let cam = |url: &str| -> CameraConfig { toml::from_str(&format!("name = \"C\"\nurl = \"{url}\"\nusername = \"u\"\npassword = \"p\"\n[controls]\nscheme = \"baichuan\"")).unwrap() };
    let a = target(&cam("rtsp://[FD00::5]:554/Preview_01_sub")).unwrap();
    let b = target(&cam("rtsp://[fd00:0:0::5]/Preview_01_main")).unwrap();
    assert_eq!(a.bases, vec!["baichuan-tcp://[fd00::5]:9000".to_string()]);
    assert_eq!((a.bases.clone(), a.place.clone()), (b.bases.clone(), b.place.clone()));
    // The video's session key is the same host.
    let v = crate::ingest::baichuan::wired::target_for("rtsp://[FD00::5]:554/Preview_01_sub", 9000, None, crate::ingest::baichuan::wired::StreamKind::Main).unwrap();
    assert_eq!(v.host, "fd00::5");
    assert_eq!(baichuan_drv::base_for(&v.host, 9000), a.bases[0]);
}

#[test]
fn s5_a_light_is_the_same_light_whichever_way_the_camera_is_reached() {
    let cam = |extra: &str| -> CameraConfig { toml::from_str(&format!("name = \"C\"\nurl = \"rtsp://192.0.2.5/h264Preview_01_main\"\nusername = \"u\"\npassword = \"p\"\n{extra}")).unwrap() };
    let places: Vec<String> = ["", "connect = \"auto\"", "connect = \"baichuan\"", "baichuan_port = 9000", "[controls]\nscheme = \"http\"", "[controls]\nscheme = \"baichuan\""]
        .iter()
        .map(|x| target(&cam(x)).unwrap().place)
        .collect();
    assert!(places.iter().all(|p| p == "reolink|192.0.2.5|u|0"), "{places:?}");
    let mut other = cam("");
    other.username = "v".into();
    assert_ne!(target(&other).unwrap().place, places[0]);
    // Old saved records find their place.
    assert_eq!(place_from_key("reolink|https://192.0.2.5,baichuan-tcp://192.0.2.5:9000|u|0|abcdef"), "reolink|192.0.2.5|u|0");
    assert_eq!(place_from_key("reolink|192.0.2.5|u|0"), "reolink|192.0.2.5|u|0");
    // On over the web API, then the camera is set to port 9000: given back over the web API.
    use super::fake::{Fake, FakeState};
    let web = Fake::start(FakeState { light: Some((0, 1, 43)), ..Default::default() });
    let bcf = FakeBc::start(BcState { led_ctrl: 7, ..Default::default() });
    let (e, id) = engine_with(&bcf, "", &format!("scheme = \"http\"\nhttp_port = {}", web.port));
    act(&e, id, &me(), Action::LightOn { seconds: Some(600), brightness: None }, false).unwrap();
    assert_eq!(web.state().light.unwrap().0, 1);
    set_cfg(&e, id, |c| c.controls.as_mut().unwrap().scheme = Some("baichuan".into()));
    for l in e.controls.lights.lock().unwrap().values_mut() {
        l.f.until_ms = now_ms() - 1;
    }
    tick_until(&e, || web.state().light == Some((0, 1, 43)));
    tick_until(&e, || e.controls.lights.lock().unwrap().is_empty());
    assert!(bcf.writes().is_empty(), "never over port 9000");
    // After a restart (no way it was turned on is known) and only port 9000: given up, nothing sent.
    set_cfg(&e, id, |c| c.controls.as_mut().unwrap().scheme = Some("http".into()));
    std::thread::sleep(LIGHT_GAP);
    act(&e, id, &me(), Action::LightOn { seconds: Some(600), brightness: None }, false).unwrap();
    set_cfg(&e, id, |c| c.controls.as_mut().unwrap().scheme = Some("baichuan".into()));
    *e.controls.lights.lock().unwrap() = Default::default();
    load_pending(&e);
    for l in e.controls.lights.lock().unwrap().values_mut() {
        l.f.until_ms = now_ms() - 1;
    }
    let n = web.state().reqs;
    tick_until(&e, || e.controls.lights.lock().unwrap().is_empty());
    assert_eq!(web.state().reqs, n);
    assert!(bcf.writes().is_empty());
    assert!(audit(&e).iter().any(|l| l.contains("only over Reolink port 9000")), "{:?}", audit(&e));
}

#[test]
fn n3_the_siren_stop_does_not_wait_behind_a_slow_change() {
    let fake = FakeBc::start(BcState { noop_writes: true, round_ms: 10_000, ..Default::default() });
    let (e, id) = picked(&fake);
    act(&e, id, &me(), Action::Siren { seconds: Some(20) }, false).unwrap();
    let e2 = e.clone();
    // A change the camera never makes: written, then read back for about two seconds.
    let slow = std::thread::spawn(move || act(&e2, id, &me(), Action::StatusLed { state: "On".into() }, false));
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    act(&e, id, &me(), Action::SirenStop, false).unwrap();
    assert!(t.elapsed() < Duration::from_millis(900), "the stop waited {:?}", t.elapsed());
    assert!(!fake.state().playing());
    assert!(slow.join().unwrap().is_err());
}

#[test]
fn n5_page_reads_neither_keep_a_session_nor_sign_in_again_and_again() {
    let fake = FakeBc::start(BcState::default());
    let d = baichuan_drv::BaichuanDriver::at(&baichuan_drv::base_for("127.0.0.1", fake.port), "u", "p", 0).unwrap();
    let ab = Abilities { ir: vec!["Auto".into(), "Off".into()], ..Default::default() };
    for _ in 0..3 {
        assert_eq!(d.read_if_idle(&ab).unwrap().unwrap().ir.as_deref(), Some("Auto"));
    }
    // Reads alone: the session signs out soon (not after a minute, not never).
    let until = Instant::now() + Duration::from_secs(12);
    while crate::ingest::baichuan::wired::status("127.0.0.1", fake.port).is_some_and(|s| s.signed_in) {
        assert!(Instant::now() < until, "page reads kept the session signed in");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Asked again soon after: the last reading stands, no new sign-in.
    assert!(d.read_if_idle(&ab).is_none());
    assert_eq!(fake.state().logins, 1);
}

// ---- second review (M1, R1, R2, nits)

#[test]
fn m1_a_stop_during_a_slow_sign_in_never_goes_out_before_the_play() {
    let fake = FakeBc::start(BcState { round_ms: 3000, ..Default::default() });
    let (e, id) = picked(&fake);
    view(&e, id, all()).unwrap();
    // A cold session: the old one is replaced (other credentials), and the next sign-in is slow.
    let _ = crate::ingest::baichuan::wired::client_for("127.0.0.1", fake.port, "x", "y", "someone else").unwrap();
    fake.slow_login(1500);
    let e2 = e.clone();
    let play = std::thread::spawn(move || act(&e2, id, &me(), Action::Siren { seconds: Some(20) }, false).map(|_| ()).map_err(|r| r.msg));
    std::thread::sleep(Duration::from_millis(400));
    act(&e, id, &me(), Action::SirenStop, false).unwrap();
    play.join().unwrap().unwrap();
    let order: Vec<&str> = fake.writes().iter().map(|(_, x)| if x.contains("<playMode>0</playMode>") { "play" } else { "stop" }).collect::<Vec<_>>().into_iter().collect();
    assert_eq!(order, ["play", "stop"]);
    assert!(!fake.state().playing(), "the stop came after the play");
}

#[test]
fn m1_a_play_that_lands_after_its_stop_keeps_a_timer_and_is_stopped_again() {
    let e = Engine::for_test(toml::from_str("").unwrap());
    let until = now_ms() + 20_000;
    let sid = siren_begin(&e, "Den", "", until);
    // The stop went through while the play was on its way (it may have reached the camera first).
    siren_stopped(&e, "Den", sid);
    assert!(e.controls.with("Den", |c| c.siren_until_ms.is_none()));
    siren_landed(&e, "Den", sid, false);
    let due = e.controls.with("Den", |c| c.siren_until_ms);
    assert!(due.is_some_and(|u| u <= now_ms()), "stopped again at once: {due:?}");
    assert!(e.db.ui_get(SCOPE, "siren|Den").unwrap().is_some(), "and saved");
    // A play the camera surely did not do leaves nothing.
    let sid = siren_begin(&e, "Den", "", until);
    siren_landed(&e, "Den", sid, true);
    assert!(e.controls.with("Den", |c| c.siren_until_ms.is_none()));
    assert!(e.db.ui_get(SCOPE, "siren|Den").unwrap().is_none());
    // A stop pressed before the play started (its number is the older one) leaves the new siren its timer.
    let old = e.controls.with("Den", |c| c.siren_id);
    let sid = siren_begin(&e, "Den", "", until);
    siren_stopped(&e, "Den", old);
    siren_landed(&e, "Den", sid, false);
    assert_eq!(e.controls.with("Den", |c| c.siren_until_ms), Some(until));
}

#[test]
fn r2_the_timer_clears_only_the_siren_it_stopped() {
    let fake = FakeBc::start(BcState { round_ms: 10_000, ..Default::default() });
    let (e, id) = picked(&fake);
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap();
    let first = e.controls.with("Den", |c| c.siren_id);
    // A newer siren started while the first one's stop was on its way.
    let newer = siren_begin(&e, "Den", "", now_ms() + 20_000);
    siren_landed(&e, "Den", newer, false);
    finish_siren(&e, "Den", first).unwrap();
    assert!(e.controls.with("Den", |c| c.siren_until_ms.is_some()), "the newer siren keeps its record");
    assert!(e.db.ui_get(SCOPE, "siren|Den").unwrap().is_some());
    finish_siren(&e, "Den", newer).unwrap();
    assert!(e.controls.with("Den", |c| c.siren_until_ms.is_none()));
    assert!(e.db.ui_get(SCOPE, "siren|Den").unwrap().is_none());
}

#[test]
fn r1_set_ports_tell_cameras_on_one_address_apart() {
    let cam = |extra: &str| -> CameraConfig { toml::from_str(&format!("name = \"C\"\nurl = \"rtsp://192.0.2.5/h264Preview_01_main\"\nusername = \"u\"\npassword = \"p\"\n{extra}")).unwrap() };
    let a = target(&cam("[controls]\nhttp_port = 8081")).unwrap().place;
    let b = target(&cam("[controls]\nhttp_port = 8082")).unwrap().place;
    assert_ne!(a, b);
    assert_eq!(a, "reolink|192.0.2.5#web=8081|u|0");
    assert_eq!(a, target(&cam("[controls]\nhttp_port = 8081\nscheme = \"http\"")).unwrap().place, "the scheme is not part of it");
    let c = target(&cam("baichuan_port = 9001\n[controls]\nscheme = \"baichuan\"")).unwrap().place;
    let d = target(&cam("baichuan_port = 9002\n[controls]\nscheme = \"baichuan\"")).unwrap().place;
    assert_ne!(c, d);
    assert_eq!(c, "reolink|192.0.2.5#bc=9001|u|0");
    // Saved records the same way.
    assert_eq!(place_from_key("reolink|https://192.0.2.5:8081|u|0|abcdef"), a);
    assert_eq!(place_from_key("reolink|baichuan-tcp://192.0.2.5:9001|u|0"), c);
    assert_eq!(place_from_key("reolink|http://[FD00::5]:80|u|2"), "reolink|fd00::5#web=80|u|2");
    assert_eq!(place_from_key("reolink|https://192.0.2.5:8081,baichuan-tcp://192.0.2.5:9000|u|0"), a);
}

#[test]
fn nits_hub_refusal_is_marked_not_sent_and_a_password_fix_reads_at_once() {
    assert!(not_done(&anyhow::anyhow!(baichuan_drv::NotSent(format!("{} ({})", crate::ingest::baichuan::ctl::NOT_FOR_HUB, crate::ingest::baichuan::ctl::NOT_SENT)))));
    let fake3 = FakeBc::start(BcState { channels: 4, dev_type: "homehub".into(), ..Default::default() });
    let (e3, id3) = picked(&fake3);
    let err = act(&e3, id3, &me(), Action::Siren { seconds: Some(1) }, false).unwrap_err();
    assert!(err.msg.contains("hub or recorder"), "{}", err.msg);
    // The page-read gate is per credentials: a wrong password, then the right one, reads at once.
    let fake = FakeBc::start(BcState::default());
    let ab = Abilities { ir: vec!["Auto".into(), "Off".into()], ..Default::default() };
    let wrong = baichuan_drv::BaichuanDriver::at(&baichuan_drv::base_for("127.0.0.1", fake.port), "u", "wrong", 0).unwrap();
    assert!(wrong.read_if_idle(&ab).unwrap().is_err());
    let right = baichuan_drv::BaichuanDriver::at(&baichuan_drv::base_for("127.0.0.1", fake.port), "u", "p", 0).unwrap();
    assert_eq!(right.read_if_idle(&ab).unwrap().unwrap().ir.as_deref(), Some("Auto"));
}

#[test]
fn a_renamed_cameras_siren_is_still_stopped_at_its_address_now() {
    let fake = FakeBc::start(BcState { round_ms: 10_000, ..Default::default() });
    let (e, id) = picked(&fake);
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap();
    set_cfg(&e, id, |c| c.name = "Study".into());
    std::thread::sleep(Duration::from_millis(1100));
    tick_until(&e, || !fake.state().playing());
    assert!(fake.writes().iter().any(|(_, x)| x.contains("<onOff>0</onOff>")), "the stop went to the camera");
    tick_until(&e, || e.controls.with("Den", |c| c.siren_until_ms.is_none()));
    // After a restart too (the saved record says where it is).
    std::thread::sleep(SIREN_GAP);
    set_cfg(&e, id, |c| c.name = "Den".into());
    act(&e, id, &me(), Action::Siren { seconds: Some(20) }, false).unwrap();
    assert!(fake.state().playing());
    assert!(e.db.ui_get(SCOPE, "siren|Den").unwrap().unwrap().contains("|reolink|127.0.0.1"));
    set_cfg(&e, id, |c| c.name = "Study".into());
    *e.controls.cams.lock().unwrap() = Default::default();
    load_pending(&e);
    tick_until(&e, || !fake.state().playing());
    // Moved to an address no camera is set to: nothing is sent there.
    let other = FakeBc::start(BcState { round_ms: 10_000, ..Default::default() });
    let (e, id) = picked(&other);
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap();
    let n = other.state().log.len();
    set_cfg(&e, id, |c| {
        c.name = "Elsewhere".into();
        c.url = "rtsp://192.0.2.99:554/Preview_01_sub".into();
    });
    std::thread::sleep(Duration::from_millis(1100));
    tick_until(&e, || e.controls.with("Den", |c| c.siren_until_ms.is_none()));
    assert!(other.state().log[n..].iter().all(|(m, _)| *m == bc::MSG_KEEPALIVE), "nothing sent to the old address");
}
