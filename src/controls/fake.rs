//! A fake Reolink camera's web API for tests: sign-in, abilities, the
//! spotlight, the siren, the infrared lights and the status light, answered
//! as real cameras answer (shapes from the vendor guide and from reading real
//! cameras: `floodLight` 0 on a floodlight, `GetWhiteLed` answering on a
//! camera with no light, no mode list in the range).

use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct FakeState {
    /// The spotlight's (state, mode, bright); None = no light (GetWhiteLed answers anyway).
    pub light: Option<(i64, i64, u32)>,
    pub brightness_flag: bool,
    pub siren: bool,
    pub ir: Option<String>,
    pub power_led: Option<String>,
    /// "Done", and nothing changes (as some cameras do).
    pub noop_writes: bool,
    pub refuse_login: bool,
    /// Every command the camera got, with its param for changes.
    pub log: Vec<String>,
    pub logins: usize,
    pub siren_playing: bool,
    /// The light as a camera without one reports it.
    pub ghost_light: (i64, i64, u32),
    /// After the first SetWhiteLed: GetWhiteLed fails, and further SetWhiteLed fail.
    pub fail_reads_after_set: bool,
    pub fail_sets_after_first: bool,
    /// The siren's stop (`manu`, switch 0) fails.
    pub fail_siren_stop: bool,
    pub sets: usize,
    /// Every answer waits this long (ms).
    pub slow_ms: u64,
    /// Requests to the API (any command).
    pub reqs: usize,
    /// Error detail sent with failures (hostile text).
    pub detail: Option<String>,
    /// The doorbell light (`eDoorbellLightState`) and what its range lists.
    pub doorbell: Option<(String, Vec<String>)>,
    /// What `GetDevInfo` says the device is: (type, model).
    pub dev: (String, String),
}

impl Default for FakeState {
    fn default() -> Self {
        FakeState {
            light: Some((0, 1, 85)),
            brightness_flag: true,
            siren: true,
            ir: Some("Auto".into()),
            power_led: Some("On".into()),
            noop_writes: false,
            refuse_login: false,
            log: Vec::new(),
            logins: 0,
            siren_playing: false,
            ghost_light: (0, 1, 100),
            fail_reads_after_set: false,
            fail_sets_after_first: false,
            fail_siren_stop: false,
            sets: 0,
            slow_ms: 0,
            reqs: 0,
            detail: None,
            doorbell: None,
            dev: ("IPC".into(), "Camera".into()),
        }
    }
}

pub struct Fake {
    pub port: u16,
    pub st: Arc<Mutex<FakeState>>,
}

impl Fake {
    pub fn start(st: FakeState) -> Fake {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let st = Arc::new(Mutex::new(st));
        let s2 = st.clone();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(mut c) = c else { break };
                let st = s2.clone();
                std::thread::spawn(move || {
                    // A TLS hello at a plain HTTP port: answered as a web server does (400), not read.
                    let mut first = [0u8; 1];
                    if c.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                        let _ = c.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    let (head, body) = crate::test_ports::read_http_request(&mut c);
                    let line = head.lines().next().unwrap_or("").to_string();
                    let out = answer(&st, &line, &body).to_string();
                    let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len()).as_bytes());
                });
            }
        });
        Fake { port, st }
    }

    pub fn state(&self) -> FakeState {
        self.st.lock().unwrap().clone()
    }

    pub fn set(&self, f: impl FnOnce(&mut FakeState)) {
        f(&mut self.st.lock().unwrap())
    }

    /// Commands that change something, as logged.
    pub fn writes(&self) -> Vec<String> {
        self.state().log.into_iter().filter(|l| l.starts_with("Set") || l.starts_with("AudioAlarmPlay")).collect()
    }
}

fn ab(on: bool) -> Value {
    json!({ "permit": if on { 6 } else { 0 }, "ver": i32::from(on) })
}

fn ok(cmd: &str, value: Value) -> Value {
    json!({ "cmd": cmd, "code": 0, "value": value })
}

fn fail(cmd: &str, code: i64, detail: &str) -> Value {
    json!({ "cmd": cmd, "code": 1, "error": { "rspCode": code, "detail": detail } })
}

fn answer(st: &Mutex<FakeState>, line: &str, body: &[u8]) -> Value {
    let cmds: Vec<Value> = serde_json::from_slice(body).unwrap_or_default();
    let has_token = line.contains("token=tok");
    let mut out = Vec::new();
    let slow = {
        let mut s = st.lock().unwrap();
        s.reqs += 1;
        s.slow_ms
    };
    if slow > 0 {
        std::thread::sleep(std::time::Duration::from_millis(slow));
    }
    let mut s = st.lock().unwrap();
    let detail = s.detail.clone();
    let fail = |cmd: &str, code: i64, d: &str| fail(cmd, code, detail.as_deref().unwrap_or(d));
    for c in cmds {
        let cmd = c["cmd"].as_str().unwrap_or("").to_string();
        let p = &c["param"];
        s.log.push(if cmd.starts_with("Set") || cmd == "AudioAlarmPlay" { format!("{cmd} {p}") } else { cmd.clone() });
        if cmd == "Login" {
            s.logins += 1;
            out.push(if s.refuse_login { fail(&cmd, -7, "login failed") } else { ok(&cmd, json!({ "Token": { "name": "tok", "leaseTime": 3600 } })) });
            continue;
        }
        if !has_token {
            out.push(fail(&cmd, -6, "please login first"));
            continue;
        }
        let noop = s.noop_writes;
        let r = match cmd.as_str() {
            "Logout" => ok(&cmd, json!({ "rspCode": 200 })),
            "GetDevInfo" if s.dev.0.is_empty() => fail(&cmd, -9, "not support"),
            "GetDevInfo" => ok(&cmd, json!({ "DevInfo": { "type": s.dev.0, "model": s.dev.1 } })),
            "GetAbility" => {
                let l = s.light.is_some();
                ok(&cmd, json!({ "Ability": {
                    "alarmAudio": ab(s.siren),
                    "abilityChn": [{
                        "floodLight": ab(false),
                        "supportFLswitch": ab(l),
                        "supportFLBrightness": ab(l && s.brightness_flag),
                        "supportFLIntelligent": ab(l),
                        "alarmAudio": ab(s.siren),
                        "ledControl": ab(s.ir.is_some()),
                        "powerLed": ab(false),
                    }]
                } }))
            }
            "GetWhiteLed" if s.fail_reads_after_set && s.sets > 0 => fail(&cmd, -1, "read failed"),
            "SetWhiteLed" if s.fail_sets_after_first && s.sets > 0 => {
                s.sets += 1;
                fail(&cmd, -1, "set failed")
            }
            "GetWhiteLed" => {
                let (state, mode, bright) = s.light.unwrap_or(s.ghost_light);
                let mut v = ok(&cmd, json!({ "WhiteLed": { "channel": 0, "state": state, "mode": mode, "bright": bright,
                    "LightingSchedule": { "StartHour": 18, "StartMin": 0, "EndHour": 6, "EndMin": 0 }, "wlAiDetectType": { "people": 1, "vehicle": 0, "dog_cat": 0 } } }));
                if c["action"].as_i64() == Some(1) {
                    v["range"] = json!({ "WhiteLed": { "bright": { "min": 0, "max": 100 }, "wlAiDetectType": { "people": "boolean" } } });
                }
                v
            }
            "SetWhiteLed" => match s.light.as_mut() {
                None => fail(&cmd, -9, "not support"),
                Some(l) => {
                    let l: &mut (i64, i64, u32) = l;
                    let w = &p["WhiteLed"];
                    if !noop {
                        if let Some(x) = w["state"].as_i64() {
                            l.0 = x;
                        }
                        if let Some(x) = w["mode"].as_i64() {
                            l.1 = x;
                        }
                        if let Some(x) = w["bright"].as_u64() {
                            l.2 = x as u32;
                        }
                    }
                    s.sets += 1;
                    ok(&cmd, json!({ "rspCode": 200 }))
                }
            },
            "AudioAlarmPlay" if s.fail_siren_stop && p["alarm_mode"] == "manu" => fail(&cmd, -1, "busy"),
            "AudioAlarmPlay" => {
                if !s.siren {
                    fail(&cmd, -9, "not support")
                } else {
                    if !noop {
                        s.siren_playing = p["alarm_mode"] == "times" || p["manual_switch"].as_i64() == Some(1);
                    }
                    ok(&cmd, json!({ "rspCode": 200 }))
                }
            }
            "GetIrLights" => match &s.ir {
                None => fail(&cmd, -9, "not support"),
                Some(i) => {
                    let mut v = ok(&cmd, json!({ "IrLights": { "channel": 0, "state": i } }));
                    v["range"] = json!({ "IrLights": { "state": ["Auto", "Off"] } });
                    v
                }
            },
            "SetIrLights" => {
                if s.ir.is_none() {
                    fail(&cmd, -9, "not support")
                } else {
                    if !noop {
                        s.ir = p["IrLights"]["state"].as_str().map(str::to_string);
                    }
                    ok(&cmd, json!({ "rspCode": 200 }))
                }
            }
            "GetPowerLed" => match &s.power_led {
                None => fail(&cmd, -9, "not support"),
                Some(x) => {
                    let mut v = ok(&cmd, json!({ "PowerLed": { "channel": 0, "state": x } }));
                    v["range"] = json!({ "PowerLed": { "state": ["On", "Off"] } });
                    if let Some((d, list)) = &s.doorbell {
                        v["value"]["PowerLed"]["eDoorbellLightState"] = json!(d);
                        v["range"]["PowerLed"]["eDoorbellLightState"] = json!(list);
                    }
                    v
                }
            },
            "SetPowerLed" => {
                if s.power_led.is_none() {
                    fail(&cmd, -9, "not support")
                } else {
                    if !noop {
                        if let Some(x) = p["PowerLed"]["state"].as_str() {
                            s.power_led = Some(x.to_string());
                        }
                        if let (Some(x), Some(d)) = (p["PowerLed"]["eDoorbellLightState"].as_str(), s.doorbell.as_mut()) {
                            d.0 = x.to_string();
                        }
                    }
                    ok(&cmd, json!({ "rspCode": 200 }))
                }
            }
            _ => fail(&cmd, -9, "not support"),
        };
        out.push(r);
    }
    Value::Array(out)
}
