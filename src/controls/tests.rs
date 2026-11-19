//! Camera controls against a fake Reolink camera (fake.rs): what is found,
//! timed lights given back, sirens limited, writes read back, battery
//! cameras left alone, and who may do what over the API.

use super::fake::{Fake, FakeState};
use super::*;
use std::sync::Arc;

fn engine_with(fake: &Fake, extra: &str) -> (Arc<Engine>, usize) {
    let e = Engine::for_test(toml::from_str("").unwrap());
    let h = e.add_test_camera("Yard");
    let text = format!(
        "name = \"Yard\"\nurl = \"rtsp://127.0.0.1:554/h264Preview_01_main\"\nusername = \"u\"\npassword = \"p\"\n{extra}\n[controls]\nscheme = \"http\"\nhttp_port = {}\n",
        fake.port
    );
    let cfg: CameraConfig = toml::from_str(&text).unwrap();
    *h.cfg.write().unwrap() = cfg.clone();
    e.config.lock().unwrap().cameras[0] = cfg;
    {
        let mut st = h.state.write().unwrap();
        st.name = "Yard".into();
        st.online = true;
    }
    (e, h.id)
}

fn me() -> Caller {
    Caller { user: "ann".into(), ip: "192.0.2.1".into(), source: "web" }
}

fn all() -> May {
    May { light: true, siren: true, settings: true }
}

/// The one light turned on from AmberSight (tests with one camera).
fn the_light(e: &Engine) -> Option<Forced> {
    e.controls.lights.lock().unwrap().values().next().map(|l| l.f.clone())
}

/// The one saved light record.
fn saved(e: &Engine) -> Option<String> {
    e.db.ui_scope_prefix(SCOPE, "light|").unwrap().into_iter().next().map(|(_, v)| v)
}

/// Save `f` as the record of its place (as a restart would find it).
fn put_saved(e: &Engine, f: &Forced) {
    e.db.ui_put(SCOPE, &format!("light|{}", f.key), &serde_json::to_string(f).unwrap()).unwrap();
}

/// A restart: the board forgets everything, the saved records are read back.
fn restart(e: &Engine) {
    *e.controls.cams.lock().unwrap() = Default::default();
    *e.controls.lights.lock().unwrap() = Default::default();
    load_pending(e);
}

/// Run the timer until `f` holds (at most 5 s).
fn tick_until(e: &Arc<Engine>, f: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        tick(e);
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out");
}

#[test]
fn what_the_camera_has_comes_from_its_flags() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "");
    let v = view(&e, id, all()).unwrap();
    assert_eq!(v["supported"], true, "{v}");
    assert_eq!(v["light"]["brightness"], json!({ "min": 0, "max": 100 }), "{v}");
    assert_eq!(v["light"]["modes"], json!([]), "modes are never guessed");
    assert!(v["siren"].is_object());
    assert_eq!(v["ir"]["states"], json!(["Auto", "Off"]));
    assert_eq!(v["status_led"]["states"], json!(["On", "Off"]), "GetPowerLed answers with powerLed 0");
    assert_eq!(v["state"]["light"], "off");
    assert_eq!(v["state"]["mode"], 1);
    assert_eq!(v["state"]["siren"], "unknown", "a siren's state cannot be read");
    // Nothing was changed by looking.
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    // The camera list's summary (no camera asked).
    let s = summary(&e, &e.camera(id).unwrap().cfg.read().unwrap().clone());
    assert_eq!((s["light"].as_bool(), s["siren"].as_bool(), s["light_on"].as_bool()), (Some(true), Some(true), Some(false)), "{s}");
}

#[test]
fn a_camera_without_a_light_has_none_though_it_answers() {
    let fake = Fake::start(FakeState { light: None, siren: false, ir: None, ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    let v = view(&e, id, all()).unwrap();
    assert!(v["light"].is_null() && v["siren"].is_null() && v["ir"].is_null(), "{v}");
    let r = act(&e, id, &me(), Action::LightOn { seconds: Some(5), brightness: None }, false).unwrap_err();
    assert_eq!(r.status, 409);
    let r = act(&e, id, &me(), Action::Siren { seconds: None }, false).unwrap_err();
    assert_eq!(r.status, 409);
    assert!(fake.writes().is_empty());
}

#[test]
fn a_timed_light_gets_back_exactly_what_it_had() {
    let fake = Fake::start(FakeState { light: Some((0, 1, 43)), ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    let v = act(&e, id, &me(), Action::LightOn { seconds: Some(1), brightness: Some(90) }, false).unwrap();
    assert_eq!(v["state"]["light"], "on", "{v}");
    assert!(v["state"]["forced_until_ms"].is_i64());
    assert_eq!(fake.state().light, Some((1, 1, 90)), "forced on with state, the mode untouched");
    // Saved, so a restart still gives it back.
    assert!(saved(&e).is_some());
    std::thread::sleep(Duration::from_millis(1100));
    tick_until(&e, || fake.state().light == Some((0, 1, 43)));
    tick_until(&e, || the_light(&e).is_none());
    assert!(saved(&e).is_none());
    let audit: Vec<String> = e.db.recent_audit(10).unwrap().into_iter().map(|r| format!("{} {} {}", r.1, r.3, r.4)).collect();
    assert!(audit.iter().any(|a| a.contains("camera control") && a.contains("Yard: spotlight on for 1 s at brightness 90, ok")), "{audit:?}");
    assert!(audit.iter().any(|a| a.contains("back to the camera's own setting")), "{audit:?}");
}

#[test]
fn a_light_that_could_not_be_given_back_stays_on_the_list_until_it_is() {
    // The light comes on, its state cannot be read back, and the give-back fails too.
    let fake = Fake::start(FakeState { light: Some((0, 1, 43)), fail_reads_after_set: true, fail_sets_after_first: true, ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    let r = act(&e, id, &me(), Action::LightOn { seconds: Some(600), brightness: None }, false).unwrap_err();
    assert_eq!(r.status, 502, "{}", r.msg);
    assert_eq!(fake.state().light, Some((1, 1, 43)), "it is on");
    let f = the_light(&e).expect("kept to give back");
    assert!(f.until_ms <= now_ms(), "its time is up at once");
    assert!(saved(&e).is_some(), "and saved");
    // The camera answers again: the timer gives it back.
    fake.set(|s| {
        s.fail_reads_after_set = false;
        s.fail_sets_after_first = false;
    });
    for l in e.controls.lights.lock().unwrap().values_mut() {
        l.restore_next = None;
    }
    tick_until(&e, || fake.state().light == Some((0, 1, 43)));
    tick_until(&e, || the_light(&e).is_none());
    assert!(saved(&e).is_none());
}

#[test]
fn a_cameras_hostile_error_text_is_cleaned_everywhere() {
    let fake = Fake::start(FakeState { refuse_login: true, detail: Some(format!("x\nFAKE LOG LINE\r\u{1b}[2J{}", "B".repeat(100_000))), ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    let v = view(&e, id, all()).unwrap();
    let err = v["error"].as_str().unwrap();
    assert!(!err.contains('\n') && !err.contains('\r') && !err.contains('\u{1b}') && err.len() < 600, "{err:?}");
    let r = act(&e, id, &me(), Action::Redetect, false);
    let audit = e.db.recent_audit(5).unwrap();
    assert!(audit.iter().all(|a| !a.4.contains('\n') && a.4.len() < 800), "{audit:?}");
    drop(r);
}

#[test]
fn many_page_reads_ask_the_camera_once_and_never_hold_up_a_stop() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "");
    view(&e, id, all()).unwrap();
    act(&e, id, &me(), Action::Siren { seconds: Some(30) }, false).unwrap();
    fake.set(|s| s.slow_ms = 150);
    std::thread::sleep(READ_FRESH);
    let before = fake.state().reqs;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..40)
        .map(|_| {
            let (e, stop) = (e.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = view(&e, id, all());
                }
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    act(&e, id, &me(), Action::SirenStop, false).unwrap();
    let took = t.elapsed();
    std::thread::sleep(Duration::from_millis(500));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
    assert!(took < Duration::from_millis(2500), "the stop waited {took:?}");
    // About 1.2 s of 40 readers: a few reads of 3 commands each (one at a time, 3 s apart), not 40.
    let asked = fake.state().reqs - before;
    assert!(asked <= 12, "{asked} requests to the camera");
}

/// The light's time is up now (as if it had run out).
fn time_up(e: &Arc<Engine>, _name: &str) {
    for l in e.controls.lights.lock().unwrap().values_mut() {
        l.f.until_ms = now_ms() - 1;
    }
}

#[test]
fn a_light_is_given_back_after_a_rename_controls_off_or_new_address() {
    for change in ["rename", "off", "url"] {
        let fake = Fake::start(FakeState { light: Some((0, 1, 43)), ..Default::default() });
        let (e, id) = engine_with(&fake, "");
        act(&e, id, &me(), Action::LightOn { seconds: Some(600), brightness: None }, false).unwrap();
        {
            let h = e.camera(id).unwrap();
            let mut c = h.cfg.write().unwrap();
            match change {
                "rename" => c.name = "Garden".into(),
                "off" => c.controls.as_mut().unwrap().kind = Some("off".into()),
                _ => c.url = "rtsp://192.0.2.99/h264Preview_01_main".into(),
            }
        }
        time_up(&e, "Yard");
        if change == "url" {
            // No camera is configured at the old address any more: nothing is sent there.
            let n = fake.state().reqs;
            tick_until(&e, || the_light(&e).is_none());
            assert_eq!(fake.state().reqs, n, "{change}: a request went to the old address");
            assert_eq!(fake.state().light.unwrap().0, 1);
        } else {
            tick_until(&e, || fake.state().light == Some((0, 1, 43)));
            tick_until(&e, || the_light(&e).is_none());
        }
    }
}

#[test]
fn after_a_restart_a_renamed_camera_is_found_by_its_address_and_no_other_camera_is() {
    let fake = Fake::start(FakeState { light: Some((0, 1, 43)), ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    act(&e, id, &me(), Action::LightOn { seconds: Some(600), brightness: None }, false).unwrap();
    let up_and_restart = |e: &Arc<Engine>| {
        let mut f: Forced = serde_json::from_str(&saved(e).unwrap()).unwrap();
        f.until_ms = now_ms() - 1;
        put_saved(e, &f);
        restart(e);
    };
    // Renamed while stopped: found by what reaches it.
    e.camera(id).unwrap().cfg.write().unwrap().name = "Garden".into();
    up_and_restart(&e);
    tick_until(&e, || fake.state().light == Some((0, 1, 43)));
    // A new address while stopped: whatever listens at the old one now (a stranger)
    // is sent nothing, no sign-in either. Given up, said in the log and audit.
    std::thread::sleep(LIGHT_GAP);
    act(&e, id, &me(), Action::LightOn { seconds: Some(600), brightness: None }, false).unwrap();
    assert_eq!(fake.state().light.unwrap().0, 1);
    let mut f: Forced = serde_json::from_str(&saved(&e).unwrap()).unwrap();
    f.until_ms = now_ms() - 1;
    put_saved(&e, &f);
    e.camera(id).unwrap().cfg.write().unwrap().url = "rtsp://192.0.2.99/h264Preview_01_main".into();
    restart(&e);
    let n = fake.state().reqs;
    tick_until(&e, || saved(&e).is_none());
    assert_eq!(fake.state().reqs, n, "nothing sent to the old address: {:?}", fake.state().log);
    let audit: Vec<String> = e.db.recent_audit(5).unwrap().into_iter().map(|r| r.4).collect();
    assert!(audit.iter().any(|a| a.contains("Garden: spotlight back") && a.contains("failed")), "{audit:?}");
}

#[test]
fn a_new_camera_given_a_renamed_cameras_name_does_not_inherit_its_light() {
    let old = Fake::start(FakeState { light: Some((0, 1, 43)), ..Default::default() });
    let (e, id) = engine_with(&old, "");
    act(&e, id, &me(), Action::LightOn { seconds: Some(120), brightness: None }, false).unwrap();
    e.camera(id).unwrap().cfg.write().unwrap().name = "Yardx".into();
    // A new camera called "Yard", elsewhere.
    let new = Fake::start(FakeState { light: Some((0, 3, 70)), ..Default::default() });
    let h = e.add_test_camera("Yard2");
    // (Another device: a light belongs to a host, an account and a channel; every fake is on 127.0.0.1.)
    let cfg: CameraConfig = toml::from_str(&format!("name = \"Yard\"\nurl = \"rtsp://127.0.0.1:554/h264Preview_01_main\"\nusername = \"u2\"\npassword = \"p\"\n[controls]\nscheme = \"http\"\nhttp_port = {}\n", new.port)).unwrap();
    *h.cfg.write().unwrap() = cfg;
    h.state.write().unwrap().online = true;
    let v = view(&e, h.id, all()).unwrap();
    assert!(v["state"]["forced_until_ms"].is_null(), "not shown: {v}");
    std::thread::sleep(LIGHT_GAP);
    act(&e, h.id, &me(), Action::LightOff, false).unwrap();
    assert_eq!(new.state().light, Some((0, 3, 70)), "the new camera never got the old one's saved state");
    assert_eq!(old.state().light.unwrap().0, 1, "the old light still on, still on the list");
    assert!(the_light(&e).is_some());
    // Its own time up: given back to the old camera.
    time_up(&e, "");
    tick_until(&e, || old.state().light == Some((0, 1, 43)));
    assert_eq!(new.state().light, Some((0, 3, 70)));
}

#[test]
fn a_light_the_camera_has_on_already_is_left_alone() {
    // (Giving back "on" afterwards would leave it on by hand, past what the camera's own setting does.)
    let fake = Fake::start(FakeState { light: Some((1, 1, 70)), ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    let v = act(&e, id, &me(), Action::LightOn { seconds: Some(60), brightness: None }, true).unwrap();
    assert_eq!(v["state"]["light"], "on");
    assert!(v["state"]["forced_until_ms"].is_null(), "{v}");
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    assert!(saved(&e).is_none());
    let audit: Vec<String> = e.db.recent_audit(3).unwrap().into_iter().map(|r| r.4).collect();
    assert_eq!(audit[0], "Yard: spotlight left alone (already on by the camera's own setting), ok");
}

#[test]
fn a_mode_whose_meaning_is_unsettled_is_kept_as_is() {
    // Mode 0 means "always on at night" in one guide and "off" in another: whatever it is, it comes back.
    let fake = Fake::start(FakeState { light: Some((0, 0, 1)), ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    act(&e, id, &me(), Action::LightOn { seconds: Some(60), brightness: None }, false).unwrap();
    assert_eq!(fake.state().light, Some((1, 0, 1)));
    // On again while on: the end moves, nothing is read as "before" again.
    std::thread::sleep(LIGHT_GAP);
    let v = act(&e, id, &me(), Action::LightOn { seconds: Some(300), brightness: None }, false).unwrap();
    let until = v["state"]["forced_until_ms"].as_i64().unwrap();
    assert!(until > now_ms() + 250_000, "{v}");
    std::thread::sleep(LIGHT_GAP);
    let v = act(&e, id, &me(), Action::LightOff, false).unwrap();
    assert_eq!(fake.state().light, Some((0, 0, 1)));
    assert!(v["state"]["forced_until_ms"].is_null(), "{v}");
    assert!(v["state"]["mode_label"].is_null(), "mode 0 is not named");
    // Picking a mode: none is offered (the camera lists none).
    assert_eq!(act(&e, id, &me(), Action::LightMode { mode: 1 }, false).unwrap_err().status, 400);
}

#[test]
fn an_action_light_only_moves_the_end_out() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "");
    let v = act(&e, id, &me(), Action::LightOn { seconds: Some(600), brightness: None }, false).unwrap();
    let first = v["state"]["forced_until_ms"].as_i64().unwrap();
    let v = act(&e, id, &me(), Action::LightOn { seconds: Some(60), brightness: None }, true).unwrap();
    assert_eq!(v["state"]["forced_until_ms"].as_i64(), Some(first), "an action never shortens it");
}

#[test]
fn a_change_the_camera_does_not_make_is_reported_and_nothing_is_left_behind() {
    let fake = Fake::start(FakeState { noop_writes: true, ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    let r = act(&e, id, &me(), Action::LightOn { seconds: Some(30), brightness: None }, false).unwrap_err();
    assert_eq!(r.status, 502);
    assert!(r.msg.contains("nothing changed"), "{}", r.msg);
    assert!(the_light(&e).is_none(), "no light to give back");
    assert!(saved(&e).is_none());
    let r = act(&e, id, &me(), Action::Ir { state: "Off".into() }, false).unwrap_err();
    assert!(r.msg.contains("nothing changed"), "{}", r.msg);
    let audit = e.db.recent_audit(10).unwrap();
    assert!(audit.iter().any(|r| r.4.contains("failed") && r.5 == 502), "{audit:?}");
}

#[test]
fn settings_are_read_back_and_only_listed_values_taken() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "");
    let v = act(&e, id, &me(), Action::Ir { state: "off".into() }, false).unwrap();
    assert_eq!(v["state"]["ir"], "Off");
    assert_eq!(act(&e, id, &me(), Action::Ir { state: "On".into() }, false).unwrap_err().status, 400, "this camera lists no On");
    let v = act(&e, id, &me(), Action::StatusLed { state: "Off".into() }, false).unwrap();
    assert_eq!(v["state"]["status_led"], "Off");
}

#[test]
fn a_doorbells_light_kept_off() {
    let list = vec!["On".to_string(), "Off".into(), "KeepOff".into(), "KeepOn".into(), "NotSupport".into()];
    let fake = Fake::start(FakeState { light: None, doorbell: Some(("On".into(), list)), ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    let v = view(&e, id, all()).unwrap();
    assert_eq!(v["doorbell_led"]["states"], json!(["On", "Off", "KeepOff", "KeepOn"]), "{v}");
    assert_eq!(v["state"]["doorbell_led"], "On");
    let v = act(&e, id, &me(), Action::DoorbellLed { state: "keepoff".into() }, false).unwrap();
    assert_eq!(v["state"]["doorbell_led"], "KeepOff");
    assert_eq!(fake.state().power_led.as_deref(), Some("On"), "the status light untouched");
    assert_eq!(act(&e, id, &me(), Action::DoorbellLed { state: "NotSupport".into() }, false).unwrap_err().status, 400);
}

#[test]
fn the_siren_is_limited_and_always_stops() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "");
    let v = act(&e, id, &me(), Action::Siren { seconds: Some(999) }, false).unwrap();
    assert_eq!(v["state"]["siren"], "playing", "{v}");
    let until = v["state"]["siren_until_ms"].as_i64().unwrap();
    assert!(until <= now_ms() + i64::from(SIREN_MAX_SECS) * 1000, "capped");
    let w = fake.writes();
    assert!(w.last().unwrap().contains("\"alarm_mode\":\"times\""), "the camera stops it by itself: {w:?}");
    // Not again within the gap; stopping always works.
    assert_eq!(act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap_err().status, 429);
    act(&e, id, &me(), Action::SirenStop, false).unwrap();
    assert!(!fake.state().siren_playing);
    assert!(fake.writes().last().unwrap().contains("\"manual_switch\":0"));
    assert!(e.db.ui_get(SCOPE, "siren|Yard").unwrap().is_none());
    let audit: Vec<String> = e.db.recent_audit(10).unwrap().into_iter().map(|r| r.4).collect();
    assert!(audit.iter().any(|a| a == "Yard: siren for 30 s, ok"), "{audit:?}");
    assert!(audit.iter().any(|a| a == "Yard: siren stopped, ok"), "{audit:?}");
}

#[test]
fn the_siren_stops_when_its_time_is_up_and_after_a_restart() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "");
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap();
    assert!(fake.state().siren_playing);
    std::thread::sleep(Duration::from_millis(1100));
    tick_until(&e, || !fake.state().siren_playing);
    tick_until(&e, || e.controls.with("Yard", |c| c.siren_until_ms.is_none()));
    // A siren left by a restart is stopped at start.
    fake.set(|s| s.siren_playing = true);
    e.db.ui_put(SCOPE, "siren|Yard", &(now_ms() + 20_000).to_string()).unwrap();
    load_pending(&e);
    tick_until(&e, || !fake.state().siren_playing);
}

#[test]
fn a_stop_that_keeps_failing_backs_off_and_is_given_up() {
    let fake = Fake::start(FakeState { fail_siren_stop: true, ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    act(&e, id, &me(), Action::Siren { seconds: Some(1) }, false).unwrap();
    let stops = || fake.state().log.iter().filter(|l| l.contains("\"manu\"")).count();
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(3500) {
        tick(&e);
        std::thread::sleep(Duration::from_millis(50));
    }
    // At 1 s, then 1, 2 s later: a handful, not one every quarter second.
    let n = stops();
    assert!((2..=4).contains(&n), "{n} stops tried");
    // Given up (the camera stops it by itself): the record goes.
    tick_until(&e, || e.controls.with("Yard", |c| c.siren_until_ms.is_none()));
    assert!(e.db.ui_get(SCOPE, "siren|Yard").unwrap().is_none());
    let n = stops();
    std::thread::sleep(Duration::from_millis(1500));
    for _ in 0..10 {
        tick(&e);
    }
    assert_eq!(stops(), n, "no more tries");
}

#[test]
fn one_account_sounds_only_so_many_sirens() {
    let e = Engine::for_test(toml::from_str("").unwrap());
    let fakes: Vec<Fake> = (0..SIREN_PER_USER + 1).map(|_| Fake::start(FakeState::default())).collect();
    for (i, f) in fakes.iter().enumerate() {
        let h = e.add_test_camera(&format!("Cam{i}"));
        let cfg: CameraConfig = toml::from_str(&format!("name = \"Cam{i}\"\nurl = \"rtsp://127.0.0.1/h264Preview_01_main\"\nusername = \"u\"\npassword = \"p\"\n[controls]\nscheme = \"http\"\nhttp_port = {}\n", f.port)).unwrap();
        *h.cfg.write().unwrap() = cfg;
        h.state.write().unwrap().online = true;
    }
    let who = Caller { user: "bob-siren-limit".into(), ip: String::new(), source: "web" };
    for i in 0..SIREN_PER_USER {
        act(&e, i, &who, Action::Siren { seconds: Some(1) }, false).unwrap();
    }
    let r = act(&e, SIREN_PER_USER, &who, Action::Siren { seconds: Some(1) }, false).unwrap_err();
    assert_eq!(r.status, 429, "{}", r.msg);
    assert!(fakes[SIREN_PER_USER].writes().is_empty());
}

#[test]
fn a_light_left_on_by_a_restart_is_given_back() {
    let fake = Fake::start(FakeState { light: Some((0, 1, 50)), ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    act(&e, id, &me(), Action::LightOn { seconds: Some(3600), brightness: None }, false).unwrap();
    // "Restart": the board forgets, the saved record is read back with its time up.
    let mut f: Forced = serde_json::from_str(&saved(&e).unwrap()).unwrap();
    f.until_ms = now_ms() - 1;
    put_saved(&e, &f);
    restart(&e);
    tick_until(&e, || fake.state().light == Some((0, 1, 50)));
}

#[test]
fn the_light_is_switched_at_most_once_a_second() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "");
    act(&e, id, &me(), Action::LightOn { seconds: Some(60), brightness: None }, false).unwrap();
    // Within the second: not done now, the latest one waits (on, off, on, off: off).
    for a in [Action::LightOff, Action::LightOn { seconds: Some(60), brightness: Some(20) }, Action::LightOff] {
        let v = act(&e, id, &me(), a, false).unwrap();
        assert_eq!(v["queued"], true);
    }
    assert_eq!(fake.state().light.unwrap().0, 1);
    // Whoever's request was replaced is told (the audit, and the panel).
    let audit: Vec<String> = e.db.recent_audit(5).unwrap().into_iter().map(|r| r.4).collect();
    assert!(audit.iter().any(|a| a.contains("spotlight off not done: replaced by a later on from ann")), "{audit:?}");
    assert!(view(&e, id, all()).unwrap()["state"]["note"].as_str().unwrap().contains("replaced by a later off"));
    tick_until(&e, || fake.state().light.unwrap().0 == 0);
    assert!(the_light(&e).is_none());
    let sets = fake.writes().iter().filter(|w| w.starts_with("SetWhiteLed")).count();
    assert_eq!(sets, 2, "{:?}", fake.writes());
    // The actions next to alerts are not held to it (they have their own gap).
    std::thread::sleep(LIGHT_GAP);
    act(&e, id, &me(), Action::LightOn { seconds: Some(60), brightness: None }, false).unwrap();
    act(&e, id, &me(), Action::LightOn { seconds: Some(120), brightness: None }, true).unwrap();
    assert!(the_light(&e).unwrap().until_ms > now_ms() + 100_000);
}

#[test]
fn saved_times_cannot_be_stretched_and_unreadable_ones_are_dropped() {
    let e = Engine::for_test(toml::from_str("").unwrap());
    let place = "reolink|https://192.0.2.5|u|0";
    let far = Forced { until_ms: now_ms() + 1_000_000_000, prev: LightState::default(), by: "x".into(), since_ms: now_ms(), key: place.into(), name: "A".into() };
    put_saved(&e, &far);
    e.db.ui_put(SCOPE, "light|B", "{not json").unwrap();
    let nowhere = Forced { key: String::new(), ..far.clone() };
    e.db.ui_put(SCOPE, "light|C", &serde_json::to_string(&nowhere).unwrap()).unwrap();
    load_pending(&e);
    let f = the_light(&e).unwrap();
    assert!(f.until_ms <= now_ms() + i64::from(LIGHT_MAX_SECS) * 1000);
    assert!(e.db.ui_get(SCOPE, "light|B").unwrap().is_none());
    assert!(e.db.ui_get(SCOPE, "light|C").unwrap().is_none());
    // While running, too.
    for l in e.controls.lights.lock().unwrap().values_mut() {
        l.f.until_ms = now_ms() + 1_000_000_000;
        l.restoring = true;
    }
    e.controls.with("A", |c| {
        c.siren_until_ms = Some(now_ms() + 1_000_000_000);
        c.stopping = true;
    });
    tick(&e);
    assert!(the_light(&e).unwrap().until_ms <= now_ms() + i64::from(LIGHT_MAX_SECS) * 1000);
    e.controls.with("A", |c| assert!(c.siren_until_ms.unwrap() <= now_ms() + i64::from(SIREN_MAX_SECS) * 1000));
}

#[test]
fn shutdown_gives_lights_back_and_stops_sirens() {
    let fake = Fake::start(FakeState { light: Some((0, 1, 50)), ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    act(&e, id, &me(), Action::LightOn { seconds: Some(3600), brightness: None }, false).unwrap();
    act(&e, id, &me(), Action::Siren { seconds: Some(30) }, false).unwrap();
    shutdown(&e, Duration::from_secs(5));
    let s = fake.state();
    assert_eq!(s.light, Some((0, 1, 50)));
    assert!(!s.siren_playing);
}

#[test]
fn a_sleeping_battery_camera_is_never_asked() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "on_demand = true");
    {
        let h = e.camera(id).unwrap();
        let mut st = h.state.write().unwrap();
        st.online = false;
        st.sleeping = true;
    }
    let v = view(&e, id, all()).unwrap();
    assert_eq!(v["asleep"], true, "{v}");
    assert!(v["why"].as_str().unwrap().contains("asleep"), "{v}");
    let r = act(&e, id, &me(), Action::LightOn { seconds: Some(5), brightness: None }, false).unwrap_err();
    assert_eq!(r.status, 409);
    std::thread::sleep(Duration::from_millis(100));
    assert!(fake.state().log.is_empty(), "nothing sent: {:?}", fake.state().log);
    // Awake (streaming anyway): read then, and the background pass never reads its state.
    {
        let h = e.camera(id).unwrap();
        let mut st = h.state.write().unwrap();
        st.online = true;
        st.sleeping = false;
    }
    view(&e, id, all()).unwrap();
    assert!(fake.state().log.contains(&"GetAbility".to_string()));
    let n = fake.state().log.len();
    background_pass(&e);
    assert_eq!(fake.state().log.len(), n, "no reads on a timer");
}

#[test]
fn a_hub_channel_is_asked_only_while_the_hub_says_it_is_awake_and_never_on_a_timer() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "reolink_channel = 0");
    let set = |s: Option<bool>| e.camera(id).unwrap().state.write().unwrap().device_sleeping = s;
    // Not said yet: the channel is not asked (only the device, once, what it is: not a recorder).
    set(None);
    let v = view(&e, id, all()).unwrap();
    assert_eq!(v["asleep"], true, "{v}");
    assert!(fake.state().log.iter().all(|c| c == "Login" || c == "GetDevInfo"), "{:?}", fake.state().log);
    assert_eq!(act(&e, id, &me(), Action::LightOn { seconds: Some(5), brightness: None }, false).unwrap_err().status, 409);
    // Awake: asked, and its state read for the page.
    set(Some(false));
    let v = view(&e, id, all()).unwrap();
    assert_eq!(v["state"]["light"], "off", "{v}");
    // Old abilities are not asked again by the timer, nor its state for Home Assistant.
    e.controls.with("Yard", |c| {
        c.probed_at = Some(Instant::now() - PROBE_EVERY - Duration::from_secs(1));
        c.reading = None;
    });
    let n = fake.state().log.len();
    background_pass(&e);
    assert_eq!(fake.state().log.len(), n, "{:?}", &fake.state().log[n..]);
}

#[test]
fn the_actions_siren_cooldown_survives_a_restart() {
    let fake = Fake::start(FakeState::default());
    let (e, id) = engine_with(&fake, "");
    {
        let h = e.camera(id).unwrap();
        let mut c = h.cfg.write().unwrap();
        let k = c.controls.as_mut().unwrap();
        k.siren_on_detect = true;
        k.siren_seconds = Some(1);
    }
    let cfg = e.camera(id).unwrap().cfg.read().unwrap().clone();
    let plays = || fake.state().log.iter().filter(|l| l.contains("\"times\"")).count();
    automation::on_confirmed(&e, id, &cfg, "person", true);
    let t = Instant::now();
    while plays() == 0 && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(plays(), 1);
    // "Restart": the memory goes, the saved times stay.
    e.controls.auto.lock().unwrap().clear();
    std::thread::sleep(SIREN_GAP);
    automation::on_confirmed(&e, id, &cfg, "person", true);
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(plays(), 1, "still in its cooldown");
}

#[test]
fn a_channel_with_no_word_about_sleep_is_wired_only_on_a_recorder() {
    for (ty, model, asked) in [("NVR", "RLN8-410", true), ("HOMEHUB", "Reolink Home Hub", false), ("NVR", "Reolink Home Hub", false)] {
        let fake = Fake::start(FakeState { dev: (ty.into(), model.into()), ..Default::default() });
        let (e, id) = engine_with(&fake, "reolink_channel = 1");
        e.camera(id).unwrap().state.write().unwrap().device_sleeping = None;
        let v = view(&e, id, all()).unwrap();
        let got = fake.state().log.iter().any(|c| c == "GetAbility");
        assert_eq!(got, asked, "{ty} {model}: {v}");
        assert!(fake.state().log.iter().any(|c| c == "GetDevInfo"), "asked what it is");
        // Asked once, not at every page.
        view(&e, id, all()).unwrap();
        assert_eq!(fake.state().log.iter().filter(|c| *c == "GetDevInfo").count(), 1);
    }
}

#[test]
fn a_first_channel_known_only_from_the_address_is_a_hubs_when_the_device_says_so() {
    // Behind a Home Hub, added with only Preview_01 in its address (no channel setting).
    let fake = Fake::start(FakeState { dev: ("HOMEHUB".into(), "Reolink Home Hub".into()), ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    e.camera(id).unwrap().state.write().unwrap().device_sleeping = None;
    let v = view(&e, id, all()).unwrap();
    assert_eq!(v["asleep"], true, "{v}");
    assert!(!fake.state().log.iter().any(|c| c == "GetAbility"), "{:?}", fake.state().log);
    background_pass(&e);
    assert!(!fake.state().log.iter().any(|c| c == "GetAbility" || c == "GetWhiteLed"));
    // Awake by the hub's word: asked then, still never on a timer.
    e.camera(id).unwrap().state.write().unwrap().device_sleeping = Some(false);
    view(&e, id, all()).unwrap();
    assert!(fake.state().log.iter().any(|c| c == "GetAbility"));
    // A camera of its own at Preview_01 (the usual): asked as a wired camera.
    let own = Fake::start(FakeState::default());
    let (e2, id2) = engine_with(&own, "");
    let v = view(&e2, id2, all()).unwrap();
    assert_eq!(v["asleep"], false, "{v}");
    assert!(own.state().log.iter().any(|c| c == "GetAbility"));
    // A camera that does not know GetDevInfo: a camera of its own.
    let plain = Fake::start(FakeState { dev: (String::new(), String::new()), ..Default::default() });
    let (e3, id3) = engine_with(&plain, "");
    let v = view(&e3, id3, all()).unwrap();
    assert!(v["light"].is_object() && v["error"].is_null(), "{v}");
    // Preview_02 and up: always a hub's channel.
    let cam = |url: &str| -> CameraConfig { toml::from_str(&format!("name = \"C\"\nurl = \"{url}\"\nusername = \"u\"\npassword = \"p\"")).unwrap() };
    let t = target(&cam("rtsp://192.0.2.5/h264Preview_03_sub")).unwrap();
    assert!(t.hub && !t.ch0 && !t.timers_ok() && t.channel == 2);
}

#[test]
fn a_refused_sign_in_is_not_repeated() {
    let fake = Fake::start(FakeState { refuse_login: true, ..Default::default() });
    let (e, id) = engine_with(&fake, "");
    let v = view(&e, id, all()).unwrap();
    assert!(v["error"].as_str().unwrap().contains("sign-in"), "{v}");
    assert_eq!(fake.state().logins, 1);
    // Asking again right away does not sign in again (the account would be locked).
    let _ = act(&e, id, &me(), Action::Redetect, false);
    assert_eq!(fake.state().logins, 1);
}

#[test]
fn a_camera_with_its_web_interface_off_says_so() {
    let closed = crate::test_ports::closed_port();
    let e = Engine::for_test(toml::from_str("").unwrap());
    let h = e.add_test_camera("Yard");
    let cfg: CameraConfig = toml::from_str(&format!("name = \"Yard\"\nurl = \"rtsp://127.0.0.1/h264Preview_01_main\"\nusername = \"u\"\npassword = \"p\"\n[controls]\nscheme = \"http\"\nhttp_port = {}\n", closed.port)).unwrap();
    *h.cfg.write().unwrap() = cfg;
    let v = view(&e, h.id, all()).unwrap();
    assert!(v["error"].as_str().unwrap().contains("doesn't offer the web interface"), "{v}");
}

#[test]
fn a_camera_whose_https_does_not_answer_says_what_to_do() {
    // Plain HTTP where HTTPS was expected: the TLS handshake fails.
    let fake = Fake::start(FakeState::default());
    let e = Engine::for_test(toml::from_str("").unwrap());
    let h = e.add_test_camera("Yard");
    let cfg: CameraConfig = toml::from_str(&format!("name = \"Yard\"\nurl = \"rtsp://127.0.0.1/h264Preview_01_main\"\nusername = \"u\"\npassword = \"p\"\n[controls]\nhttp_port = {}\n", fake.port)).unwrap();
    *h.cfg.write().unwrap() = cfg;
    let v = view(&e, h.id, all()).unwrap();
    let err = v["error"].as_str().unwrap();
    assert!(err.contains("HTTPS didn't answer") && err.contains("choose HTTP"), "{err}");
    assert!(fake.state().log.is_empty(), "nothing (no password) sent over plain HTTP");
}

#[test]
fn which_cameras_have_controls() {
    let cam = |extra: &str, url: &str| -> CameraConfig { toml::from_str(&format!("name = \"C\"\nurl = \"{url}\"\nusername = \"u\"\npassword = \"p\"\n{extra}")).unwrap() };
    let t = target(&cam("", "rtsp://192.0.2.5:554/h264Preview_01_main")).unwrap();
    assert_eq!((t.bases.clone(), t.channel, t.battery), (vec!["https://192.0.2.5".to_string()], 0, false));
    let t = target(&cam("reolink_channel = 2\non_demand = true", "rtsp://192.0.2.6/h264Preview_03_sub")).unwrap();
    assert_eq!((t.channel, t.battery), (2, true));
    assert!(target(&cam("", "rtsp://192.0.2.7/cam/realmonitor?channel=1")).unwrap_err().contains("Reolink"));
    assert!(target(&cam("[controls]\nkind = \"reolink\"", "rtsp://192.0.2.7/stream1")).is_ok());
    assert!(target(&cam("[controls]\nkind = \"off\"", "rtsp://192.0.2.5/h264Preview_01_main")).unwrap_err().contains("off"));
    assert!(target(&cam("", "baichuan://192.0.2.8?uid=ABC")).is_err());
    let t = target(&cam("[controls]\nhttp_port = 8443\nscheme = \"https\"", "rtsp://192.0.2.5/h264Preview_01_main")).unwrap();
    assert_eq!(t.bases, vec!["https://192.0.2.5:8443".to_string()]);
    // Plain HTTP only when picked; never a fall back.
    let t = target(&cam("[controls]\nscheme = \"http\"", "rtsp://192.0.2.5/h264Preview_01_main")).unwrap();
    assert_eq!(t.bases, vec!["http://192.0.2.5".to_string()]);
    let t = target(&cam("[controls]\nscheme = \"auto\"", "rtsp://192.0.2.5/h264Preview_01_main")).unwrap();
    assert_eq!(t.bases, vec!["https://192.0.2.5".to_string()]);
    // A changed password is another key (asked again).
    let a = target(&cam("", "rtsp://192.0.2.5/h264Preview_01_main")).unwrap();
    let mut c = cam("", "rtsp://192.0.2.5/h264Preview_01_main");
    c.password = "other".into();
    assert_ne!(a.key, target(&c).unwrap().key);
    assert!(!a.key.contains('p') || !a.key.contains("|p|"), "no password in the key");
}

#[test]
fn settings_out_of_range_are_refused() {
    let cam = |c: &str| -> CameraConfig { toml::from_str(&format!("name = \"C\"\nurl = \"rtsp://192.0.2.5/x\"\n[controls]\n{c}")).unwrap() };
    assert!(check_settings(&cam("light_on_detect = true\nlight_minutes = 5\nsiren_seconds = 10")).is_ok());
    for bad in ["kind = \"hikvision\"", "scheme = \"ftp\"", "http_port = 0", "light_minutes = 0", "light_minutes = 31", "siren_seconds = 31", "siren_cooldown_minutes = 0", "labels = [\"\"]"] {
        assert!(check_settings(&cam(bad)).is_err(), "{bad}");
    }
}

// ------------------------------------------------------------- over the API

mod api {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use base64::Engine as _;
    use tower::ServiceExt;

    struct Site {
        e: Arc<Engine>,
        app: axum::Router,
        fake: Fake,
        tag: String,
    }

    /// An admin, a viewer, an operator (lights, no siren), an operator with
    /// the siren, an operator with neither, an operator limited to another camera.
    fn site(tag: &str) -> Site {
        let fake = Fake::start(FakeState::default());
        let mut text = format!(
            "[[cameras]]\nname = \"Yard\"\nurl = \"rtsp://127.0.0.1/h264Preview_01_main\"\nusername = \"u\"\npassword = \"p\"\n[cameras.controls]\nscheme = \"http\"\nhttp_port = {}\n\n[[cameras]]\nname = \"Shed\"\nurl = \"rtsp://127.0.0.2/x\"\n",
            fake.port
        );
        for (name, extra) in [
            ("ann", "admin = true"),
            ("vic", "admin = false"),
            ("otto", "admin = false\nrole = \"operator\""),
            ("sid", "admin = false\nrole = \"operator\"\n[server.users.perms]\nsiren = true"),
            ("nolo", "admin = false\nrole = \"operator\"\n[server.users.perms]\nlights = false"),
            ("lim", "admin = false\nrole = \"operator\"\ncameras = [\"Shed\"]\n[server.users.perms]\nsiren = true"),
        ] {
            text += &format!("\n[[server.users]]\nusername = \"{tag}{name}\"\npassword = \"{}\"\n{extra}\n", crate::auth::hash_password(&format!("pw-{tag}{name}-1")));
        }
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        let e = Engine::for_test(cfg.clone());
        for c in &cfg.cameras {
            let h = e.add_test_camera(&c.name);
            *h.cfg.write().unwrap() = c.clone();
            let mut st = h.state.write().unwrap();
            st.name = c.name.clone();
            st.online = true;
        }
        e.config.lock().unwrap().cameras.truncate(cfg.cameras.len());
        let app = crate::web::build_router(e.clone());
        Site { e, app, fake, tag: tag.into() }
    }

    struct Call<'a> {
        method: &'a str,
        path: &'a str,
        who: Option<&'a str>,
        bearer: Option<String>,
        cookie: Option<String>,
        origin: Option<&'a str>,
        body: Option<&'a str>,
    }

    fn call<'a>(method: &'a str, path: &'a str, who: &'a str) -> Call<'a> {
        Call { method, path, who: Some(who), bearer: None, cookie: None, origin: None, body: None }
    }

    async fn send(s: &Site, c: Call<'_>) -> (StatusCode, serde_json::Value) {
        let mut b = Request::builder().method(c.method).uri(c.path).header(header::HOST, "localhost:8090");
        if let Some(u) = c.who {
            let u = format!("{}{u}", s.tag);
            b = b.header(header::AUTHORIZATION, format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:pw-{u}-1"))));
        }
        if let Some(t) = &c.bearer {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if let Some(k) = &c.cookie {
            b = b.header(header::COOKIE, format!("as_session={k}"));
        }
        if let Some(o) = c.origin {
            b = b.header(header::ORIGIN, o);
        }
        let body = match c.body {
            Some(j) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                Body::from(j.to_string())
            }
            None => Body::empty(),
        };
        let mut req = b.body(body).unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([10, 0, 0, 1], 5000))));
        let r = s.app.clone().oneshot(req).await.unwrap();
        let st = r.status();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (st, serde_json::from_slice(&body).unwrap_or(serde_json::Value::String(String::from_utf8_lossy(&body).into_owned())))
    }

    fn post<'a>(who: &'a str, body: &'a str) -> Call<'a> {
        Call { body: Some(body), ..call("POST", "/api/cameras/0/controls", who) }
    }

    const LIGHT: &str = r#"{"action":"light_on","seconds":60}"#;
    const SIREN: &str = r#"{"action":"siren","seconds":2}"#;

    #[tokio::test]
    async fn who_may_do_what() {
        let s = site("cc1");
        // Everyone who sees the camera sees what it has; buttons follow their rights.
        let (st, v) = send(&s, call("GET", "/api/cameras/0/controls", "vic")).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["may"], json!({ "light": false, "siren": false, "settings": false }));
        let (_, v) = send(&s, call("GET", "/api/cameras/0/controls", "otto")).await;
        assert_eq!(v["may"], json!({ "light": true, "siren": false, "settings": false }));
        // A viewer: nothing.
        assert_eq!(send(&s, post("vic", LIGHT)).await.0, StatusCode::FORBIDDEN);
        assert_eq!(send(&s, post("vic", r#"{"action":"siren_stop"}"#)).await.0, StatusCode::FORBIDDEN);
        // An operator: the light, not the siren (off unless an admin allows it), no settings.
        let (st, v) = send(&s, post("otto", LIGHT)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["state"]["light"], "on");
        let (st, v) = send(&s, post("otto", SIREN)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(v["error"].as_str().unwrap().contains("siren"));
        assert_eq!(send(&s, post("otto", r#"{"action":"ir","state":"Off"}"#)).await.0, StatusCode::FORBIDDEN);
        assert_eq!(send(&s, post("otto", r#"{"action":"redetect"}"#)).await.0, StatusCode::FORBIDDEN);
        assert!(!s.fake.state().siren_playing);
        // An operator allowed the siren; stopping is open to anyone with controls.
        let (st, v) = send(&s, post("sid", SIREN)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(send(&s, post("otto", r#"{"action":"siren_stop"}"#)).await.0, StatusCode::OK);
        // No lights right: refused at the door for the light, the siren-less stop too.
        assert_eq!(send(&s, post("nolo", LIGHT)).await.0, StatusCode::FORBIDDEN);
        // Another camera than the account's: as if it did not exist.
        assert_eq!(send(&s, post("lim", LIGHT)).await.0, StatusCode::NOT_FOUND);
        assert_eq!(send(&s, call("GET", "/api/cameras/0/controls", "lim")).await.0, StatusCode::NOT_FOUND);
        // An admin: settings too.
        let (st, v) = send(&s, post("ann", r#"{"action":"ir","state":"Off"}"#)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        // Unknown actions and remote (linked server) cameras.
        assert_eq!(send(&s, post("ann", r#"{"action":"self_destruct"}"#)).await.0, StatusCode::BAD_REQUEST);
        let p = format!("/api/cameras/{}/controls", crate::remote::REMOTE_BASE + 10_001);
        assert_eq!(send(&s, Call { body: Some(SIREN), ..call("POST", &p, "ann") }).await.0, StatusCode::NOT_FOUND);
        // The audit names who.
        let audit = s.e.db.recent_audit(50).unwrap();
        assert!(audit.iter().any(|r| r.1 == "cc1sid" && r.3 == "camera control" && r.4.starts_with("Yard: siren for 2 s")), "{audit:?}");
    }

    #[tokio::test]
    async fn one_account_cannot_flood_the_reads() {
        let s = site("cc5");
        let mut codes = Vec::new();
        for _ in 0..crate::controls::http::GET_PER_ACCOUNT + 1 {
            codes.push(send(&s, call("GET", "/api/cameras/0/controls", "vic")).await.0);
        }
        assert!(codes[..crate::controls::http::GET_PER_ACCOUNT].iter().all(|c| *c == StatusCode::OK), "{codes:?}");
        assert_eq!(*codes.last().unwrap(), StatusCode::TOO_MANY_REQUESTS);
        // Another account is not held up by it.
        assert_eq!(send(&s, call("GET", "/api/cameras/0/controls", "ann")).await.0, StatusCode::OK);
        // Nor the same account's other sign-ins: a flooding token does not lock out its pages.
        let user = format!("{}ann", s.tag);
        let (tok, _) = crate::auth::add_token(&user, "app", false, crate::auth::generation(&user)).unwrap().unwrap();
        let mut last = StatusCode::OK;
        for _ in 0..crate::controls::http::GET_PER_ACCOUNT + 1 {
            last = send(&s, Call { who: None, bearer: Some(tok.clone()), ..call("GET", "/api/cameras/0/controls", "") }).await.0;
        }
        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(send(&s, call("GET", "/api/cameras/0/controls", "ann")).await.0, StatusCode::OK);
        // A junk session cookie next to the token is not a new sign-in.
        let junk = Call { who: None, bearer: Some(tok.clone()), cookie: Some("junk-1".into()), ..call("GET", "/api/cameras/0/controls", "") };
        assert_eq!(send(&s, junk).await.0, StatusCode::TOO_MANY_REQUESTS);
        // And an account's sign-ins together have a looser cap (many tokens, one account).
        let user = format!("{}otto", s.tag);
        let per = crate::controls::http::GET_PER_ACCOUNT;
        let n = crate::controls::http::GET_PER_USER / per;
        let mut toks = Vec::new();
        for i in 0..=n {
            toks.push(crate::auth::add_token(&user, &format!("t{i}"), false, crate::auth::generation(&user)).unwrap().unwrap().0);
        }
        for t in &toks[..n] {
            for _ in 0..per {
                assert_eq!(send(&s, Call { who: None, bearer: Some(t.clone()), ..call("GET", "/api/cameras/0/controls", "") }).await.0, StatusCode::OK);
            }
        }
        assert_eq!(send(&s, Call { who: None, bearer: Some(toks[n].clone()), ..call("GET", "/api/cameras/0/controls", "") }).await.0, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn another_sites_page_cannot_use_the_controls() {
        let s = site("cc2");
        let (st, _) = send(&s, Call { origin: Some("http://evil.example"), ..post("ann", SIREN) }).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert!(s.fake.writes().is_empty());
        let (st, _) = send(&s, Call { origin: Some("http://localhost:8090"), ..post("ann", LIGHT) }).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn device_tokens_get_the_light_never_the_siren() {
        let s = site("cc3");
        let user = format!("{}ann", s.tag);
        let (tok, _) = crate::auth::add_token(&user, "app", true, crate::auth::generation(&user)).unwrap().unwrap();
        let t = |body: &'static str| Call { who: None, bearer: Some(tok.clone()), body: Some(body), ..call("POST", "/api/cameras/0/controls", "") };
        let (st, v) = send(&s, t(LIGHT)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let (st, v) = send(&s, t(SIREN)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(send(&s, t(r#"{"action":"status_led","state":"Off"}"#)).await.0, StatusCode::FORBIDDEN);
        let (st, v) = send(&s, Call { who: None, bearer: Some(tok.clone()), ..call("GET", "/api/cameras/0/controls", "") }).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["may"]["siren"], false);
        assert!(!s.fake.state().siren_playing);
        // A token without control rights: no light either.
        let (ro, _) = crate::auth::add_token(&user, "wall", false, crate::auth::generation(&user)).unwrap().unwrap();
        assert_eq!(send(&s, Call { who: None, bearer: Some(ro), body: Some(LIGHT), ..call("POST", "/api/cameras/0/controls", "") }).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_camera_list_shows_what_is_known() {
        let s = site("cc4");
        send(&s, call("GET", "/api/cameras/0/controls", "vic")).await;
        let (_, v) = send(&s, call("GET", "/api/cameras", "vic")).await;
        let cams = v["cameras"].as_array().unwrap();
        assert_eq!(cams[0]["controls"]["light"], true, "{}", cams[0]);
        assert!(cams[1]["controls"].is_null());
        assert!(cams[0].get("controls_cfg").is_none(), "settings are for admins");
        let (_, v) = send(&s, call("GET", "/api/cameras", "ann")).await;
        assert!(v["cameras"][0].get("controls_cfg").is_some());
    }
}
