//! The accessory database: a bridge (aid 1) and one accessory per camera
//! shown in Apple Home, each with its information, a motion sensor and a
//! camera stream service. Instance ids are fixed per accessory kind (below),
//! so they never move; accessory ids come from the stored camera map.
//!
//! The camera stream service is declared so Home shows a camera tile with
//! snapshots; live video is not offered yet: stream setup answers busy.

use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::tlv::Builder;

/// HAP's short type ids (of 000000XX-0000-1000-8000-0026BB765291).
pub mod ty {
    pub const ACCESSORY_INFO: &str = "3E";
    pub const PROTOCOL_INFO: &str = "A2";
    pub const MOTION_SENSOR: &str = "85";
    pub const CAMERA_RTP: &str = "110";
    pub const MICROPHONE: &str = "112";

    pub const IDENTIFY: &str = "14";
    pub const MANUFACTURER: &str = "20";
    pub const MODEL: &str = "21";
    pub const NAME: &str = "23";
    pub const SERIAL: &str = "30";
    pub const FIRMWARE: &str = "52";
    pub const VERSION: &str = "37";
    pub const MOTION_DETECTED: &str = "22";
    pub const STATUS_ACTIVE: &str = "75";
    pub const SUPPORTED_VIDEO: &str = "114";
    pub const SUPPORTED_AUDIO: &str = "115";
    pub const SUPPORTED_RTP: &str = "116";
    pub const SELECTED_RTP: &str = "117";
    pub const SETUP_ENDPOINTS: &str = "118";
    pub const STREAMING_STATUS: &str = "120";
    pub const ACTIVE: &str = "B0";
    pub const MUTE: &str = "11A";
}

/// Instance ids inside a camera accessory (and the bridge's information).
pub mod iid {
    pub const INFO: u64 = 1;
    pub const IDENTIFY: u64 = 2;
    pub const MANUFACTURER: u64 = 3;
    pub const MODEL: u64 = 4;
    pub const NAME: u64 = 5;
    pub const SERIAL: u64 = 6;
    pub const FIRMWARE: u64 = 7;
    // Bridge only.
    pub const PROTOCOL: u64 = 8;
    pub const VERSION: u64 = 9;
    // Camera accessories.
    pub const MOTION: u64 = 8;
    pub const MOTION_DETECTED: u64 = 9;
    pub const STATUS_ACTIVE: u64 = 10;
    pub const RTP: u64 = 11;
    pub const SUPPORTED_VIDEO: u64 = 12;
    pub const SUPPORTED_AUDIO: u64 = 13;
    pub const SUPPORTED_RTP: u64 = 14;
    pub const SELECTED_RTP: u64 = 15;
    pub const STREAMING_STATUS: u64 = 16;
    pub const SETUP_ENDPOINTS: u64 = 17;
    pub const ACTIVE: u64 = 18;
    pub const MICROPHONE: u64 = 19;
    pub const MUTE: u64 = 20;
    // A camera published on its own: its protocol information.
    pub const SA_PROTOCOL: u64 = 21;
    pub const SA_VERSION: u64 = 22;
}

pub const BRIDGE_AID: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Bool,
    Uint8,
    String,
    Tlv8,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Bool => "bool",
            Format::Uint8 => "uint8",
            Format::String => "string",
            Format::Tlv8 => "tlv8",
        }
    }
}

/// Read, write, events.
#[derive(Clone, Copy, Debug)]
pub struct Perms {
    pub read: bool,
    pub write: bool,
    pub events: bool,
}

const PR: Perms = Perms { read: true, write: false, events: false };
const PW: Perms = Perms { read: false, write: true, events: false };
const PR_EV: Perms = Perms { read: true, write: false, events: true };
const PR_PW: Perms = Perms { read: true, write: true, events: false };
const PR_PW_EV: Perms = Perms { read: true, write: true, events: true };

impl Perms {
    fn json(self) -> Value {
        let mut v = Vec::new();
        if self.read {
            v.push("pr");
        }
        if self.write {
            v.push("pw");
        }
        if self.events {
            v.push("ev");
        }
        json!(v)
    }
}

#[derive(Clone, Debug)]
pub struct Characteristic {
    pub iid: u64,
    pub ty: &'static str,
    pub perms: Perms,
    pub format: Format,
    /// Its value (None for write-only characteristics).
    pub value: Value,
    /// Range for uint8 values.
    pub range: Option<(u8, u8)>,
}

impl Characteristic {
    fn new(iid: u64, ty: &'static str, perms: Perms, format: Format, value: Value) -> Characteristic {
        Characteristic { iid, ty, perms, format, value, range: None }
    }

    /// The JSON for /accessories (and /characteristics with meta etc.).
    pub fn json(&self, aid: Option<u64>, meta: bool, perms: bool, with_type: bool, ev: Option<bool>) -> Value {
        let mut o = serde_json::Map::new();
        if let Some(a) = aid {
            o.insert("aid".into(), a.into());
        }
        o.insert("iid".into(), self.iid.into());
        if with_type {
            o.insert("type".into(), self.ty.into());
        }
        if perms {
            o.insert("perms".into(), self.perms.json());
        }
        if meta {
            o.insert("format".into(), self.format.name().into());
            if let Some((min, max)) = self.range {
                o.insert("minValue".into(), min.into());
                o.insert("maxValue".into(), max.into());
                o.insert("minStep".into(), 1.into());
            }
            if self.format == Format::String {
                o.insert("maxLen".into(), 64.into());
            }
        }
        if self.perms.read {
            o.insert("value".into(), self.value.clone());
        }
        if let Some(e) = ev {
            o.insert("ev".into(), e.into());
        }
        Value::Object(o)
    }

    /// Whether `v` is a value this characteristic takes (written by a controller).
    pub fn accepts(&self, v: &Value) -> bool {
        match self.format {
            Format::Bool => v.is_boolean() || matches!(v.as_u64(), Some(0 | 1)),
            Format::Uint8 => v.as_u64().is_some_and(|n| n <= 255 && self.range.is_none_or(|(a, b)| n >= a as u64 && n <= b as u64)),
            Format::String => v.as_str().is_some_and(|s| s.len() <= 64),
            Format::Tlv8 => v.as_str().is_some_and(|s| base64::engine::general_purpose::STANDARD.decode(s).is_ok()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Service {
    pub iid: u64,
    pub ty: &'static str,
    pub primary: bool,
    pub chars: Vec<Characteristic>,
}

/// What an accessory stands for.
#[derive(Clone, Debug, PartialEq)]
pub enum Role {
    Bridge,
    /// A camera: its engine id (changes across restarts) and name.
    Camera { id: usize, name: String },
}

#[derive(Clone, Debug)]
pub struct Accessory {
    pub aid: u64,
    pub role: Role,
    pub services: Vec<Service>,
}

impl Accessory {
    pub fn find(&self, iid: u64) -> Option<&Characteristic> {
        self.services.iter().flat_map(|s| s.chars.iter()).find(|c| c.iid == iid)
    }
    pub fn find_mut(&mut self, iid: u64) -> Option<&mut Characteristic> {
        self.services.iter_mut().flat_map(|s| s.chars.iter_mut()).find(|c| c.iid == iid)
    }
}

/// A camera to show in Apple Home.
#[derive(Clone, Debug, PartialEq)]
pub struct Cam {
    pub aid: u64,
    pub id: usize,
    pub name: String,
}

fn b64(v: &[u8]) -> Value {
    base64::engine::general_purpose::STANDARD.encode(v).into()
}

/// A name as HomeKit takes it: letters, digits, spaces and apostrophes
/// (anything else becomes a space), spaces collapsed, starting and ending
/// with a letter or digit, at most 63 bytes. None when nothing is left.
pub fn clean_name(name: &str) -> Option<String> {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '\'' { c } else { ' ' }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for c in s.trim_matches(|c: char| !c.is_alphanumeric()).chars() {
        if out.len() + c.len_utf8() > 63 {
            break;
        }
        out.push(c);
    }
    let out = out.trim_end_matches(|c: char| !c.is_alphanumeric()).to_string();
    (!out.is_empty()).then_some(out)
}

/// HAP firmware revisions are numbers: "0.10.3".
fn firmware() -> String {
    let v = env!("CARGO_PKG_VERSION");
    v.split(['-', '+']).next().unwrap_or("0.0.0").to_string()
}

fn info(name: &str, model: &str, serial: &str) -> Service {
    // (Callers pass a name that is already clean, or a fallback.)
    use Format::*;
    Service {
        iid: iid::INFO,
        ty: ty::ACCESSORY_INFO,
        primary: false,
        chars: vec![
            Characteristic::new(iid::IDENTIFY, ty::IDENTIFY, PW, Bool, Value::Null),
            Characteristic::new(iid::MANUFACTURER, ty::MANUFACTURER, PR, String, "AmberSight".into()),
            Characteristic::new(iid::MODEL, ty::MODEL, PR, String, model.into()),
            Characteristic::new(iid::NAME, ty::NAME, PR, String, name.into()),
            Characteristic::new(iid::SERIAL, ty::SERIAL, PR, String, serial.into()),
            Characteristic::new(iid::FIRMWARE, ty::FIRMWARE, PR, String, firmware().into()),
        ],
    }
}

/// The video sizes and frame rates offered to Home (and what a start is
/// held to).
pub const OFFERED_SIZES: [(u16, u16, u8); 9] = [(1920, 1080, 30), (1280, 720, 30), (640, 360, 30), (480, 270, 30), (320, 180, 30), (1280, 960, 30), (1024, 768, 30), (640, 480, 30), (320, 240, 15)];

/// SupportedVideoStreamConfiguration: H.264 (baseline, main, high; levels
/// 3.1, 3.2, 4), the sizes Home asks for.
pub fn supported_video() -> Vec<u8> {
    let params = Builder::new().u8(1, 0).sep().u8(1, 1).sep().u8(1, 2).u8(2, 0).sep().u8(2, 1).sep().u8(2, 2).u8(3, 0).done();
    let mut b = Builder::new().u8(1, 0).bytes(2, &params);
    for (i, (w, h, fps)) in OFFERED_SIZES.iter().enumerate() {
        if i > 0 {
            b = b.sep();
        }
        let attr = Builder::new().u16(1, *w).u16(2, *h).u8(3, *fps).done();
        b = b.bytes(3, &attr);
    }
    Builder::new().bytes(1, &b.done()).done()
}

/// SupportedAudioStreamConfiguration: Opus (what standard ffmpeg builds can
/// make; AAC-ELD needs libfdk-aac), mono, 24 and 16 kHz, variable bit rate,
/// 20 ms packets only; no comfort noise.
pub fn supported_audio() -> Vec<u8> {
    let codec = |ty: u8, rate: u8| {
        let p = Builder::new().u8(1, 1).u8(2, 0).u8(3, rate).u8(4, 20).done();
        Builder::new().u8(1, ty).bytes(2, &p).done()
    };
    Builder::new().bytes(1, &codec(3, 2)).sep().bytes(1, &codec(3, 1)).u8(2, 0).done()
}

/// SupportedRTPConfiguration: SRTP AES_CM_128_HMAC_SHA1_80.
pub fn supported_rtp() -> Vec<u8> {
    Builder::new().u8(2, 0).done()
}

/// StreamingStatus: 0 available, 1 in use, 2 unavailable.
pub fn streaming_status(status: u8) -> Vec<u8> {
    Builder::new().u8(1, status).done()
}

fn camera_services(name: &str, serial: &str) -> Vec<Service> {
    use Format::*;
    let mut active = Characteristic::new(iid::ACTIVE, ty::ACTIVE, PR_PW_EV, Uint8, 1.into());
    active.range = Some((0, 1));
    vec![
        info(name, "AmberSight camera", serial),
        Service {
            iid: iid::MOTION,
            ty: ty::MOTION_SENSOR,
            primary: false,
            chars: vec![
                Characteristic::new(iid::MOTION_DETECTED, ty::MOTION_DETECTED, PR_EV, Bool, false.into()),
                Characteristic::new(iid::STATUS_ACTIVE, ty::STATUS_ACTIVE, PR_EV, Bool, true.into()),
            ],
        },
        Service {
            iid: iid::RTP,
            ty: ty::CAMERA_RTP,
            primary: true,
            chars: vec![
                Characteristic::new(iid::SUPPORTED_VIDEO, ty::SUPPORTED_VIDEO, PR, Tlv8, b64(&supported_video())),
                Characteristic::new(iid::SUPPORTED_AUDIO, ty::SUPPORTED_AUDIO, PR, Tlv8, b64(&supported_audio())),
                Characteristic::new(iid::SUPPORTED_RTP, ty::SUPPORTED_RTP, PR, Tlv8, b64(&supported_rtp())),
                Characteristic::new(iid::SELECTED_RTP, ty::SELECTED_RTP, PR_PW, Tlv8, b64(&[])),
                Characteristic::new(iid::STREAMING_STATUS, ty::STREAMING_STATUS, PR_EV, Tlv8, b64(&streaming_status(0))),
                Characteristic::new(iid::SETUP_ENDPOINTS, ty::SETUP_ENDPOINTS, PR_PW, Tlv8, b64(&[])),
                active,
            ],
        },
        Service { iid: iid::MICROPHONE, ty: ty::MICROPHONE, primary: false, chars: vec![Characteristic::new(iid::MUTE, ty::MUTE, PR_PW_EV, Bool, false.into())] },
    ]
}

fn protocol_info(service: u64, version: u64) -> Service {
    Service { iid: service, ty: ty::PROTOCOL_INFO, primary: false, chars: vec![Characteristic::new(version, ty::VERSION, PR, Format::String, "1.1.0".into())] }
}

#[derive(Clone, Debug)]
pub struct Database {
    pub accessories: Vec<Accessory>,
}

impl Database {
    /// The bridge and its cameras (in accessory-id order).
    pub fn bridge(name: &str, device_id: &str, cams: &[Cam]) -> Database {
        let bridge = clean_name(name).unwrap_or_else(|| "AmberSight".into());
        let mut bridge_services = vec![info(&bridge, "AmberSight", device_id)];
        bridge_services.push(protocol_info(iid::PROTOCOL, iid::VERSION));
        let mut accessories = vec![Accessory { aid: BRIDGE_AID, role: Role::Bridge, services: bridge_services }];
        let mut cams = cams.to_vec();
        cams.sort_by_key(|c| c.aid);
        for c in cams {
            accessories.push(Accessory { aid: c.aid, role: Role::Camera { id: c.id, name: c.name.clone() }, services: camera_services(&clean_name(&c.name).unwrap_or_else(|| format!("Camera {}", c.aid)), &format!("AS-{}", c.aid)) });
        }
        Database { accessories }
    }

    /// One camera published on its own (aid 1).
    pub fn standalone(cam: &Cam, serial: &str) -> Database {
        let name = clean_name(&cam.name).unwrap_or_else(|| format!("Camera {}", cam.aid));
        let mut services = camera_services(&name, serial);
        services.push(protocol_info(iid::SA_PROTOCOL, iid::SA_VERSION));
        Database { accessories: vec![Accessory { aid: 1, role: Role::Camera { id: cam.id, name: cam.name.clone() }, services }] }
    }

    pub fn accessory(&self, aid: u64) -> Option<&Accessory> {
        self.accessories.iter().find(|a| a.aid == aid)
    }

    pub fn accessory_mut(&mut self, aid: u64) -> Option<&mut Accessory> {
        self.accessories.iter_mut().find(|a| a.aid == aid)
    }

    pub fn get(&self, aid: u64, iid: u64) -> Option<&Characteristic> {
        self.accessory(aid)?.find(iid)
    }

    pub fn get_mut(&mut self, aid: u64, iid: u64) -> Option<&mut Characteristic> {
        self.accessory_mut(aid)?.find_mut(iid)
    }

    /// Carry live values (motion, status, Active) over into a rebuilt database.
    pub fn keep_values_from(&mut self, old: &Database) {
        for a in self.accessories.iter_mut() {
            let Some(o) = old.accessory(a.aid) else { continue };
            if o.role != a.role {
                continue;
            }
            for iid in [iid::MOTION_DETECTED, iid::STATUS_ACTIVE, iid::ACTIVE, iid::MUTE, iid::STREAMING_STATUS] {
                if let (Some(n), Some(v)) = (a.find_mut(iid), o.find(iid)) {
                    n.value = v.value.clone();
                }
            }
        }
    }

    /// `GET /accessories`.
    pub fn json(&self) -> Value {
        let accs: Vec<Value> = self
            .accessories
            .iter()
            .map(|a| {
                let services: Vec<Value> = a
                    .services
                    .iter()
                    .map(|s| {
                        let mut o = json!({
                            "iid": s.iid,
                            "type": s.ty,
                            "characteristics": s.chars.iter().map(|c| c.json(None, true, true, true, None)).collect::<Vec<_>>(),
                        });
                        if s.primary {
                            o["primary"] = true.into();
                        }
                        o
                    })
                    .collect();
                json!({ "aid": a.aid, "services": services })
            })
            .collect();
        json!({ "accessories": accs })
    }

    /// The fingerprint of what controllers cache: accessories, services,
    /// types and names (not the values). A change bumps c#.
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        for a in &self.accessories {
            h.update(a.aid.to_le_bytes());
            for s in &a.services {
                h.update(s.iid.to_le_bytes());
                h.update(s.ty.as_bytes());
                for c in &s.chars {
                    h.update(c.iid.to_le_bytes());
                    h.update(c.ty.as_bytes());
                    if c.ty == ty::NAME {
                        h.update(c.value.as_str().unwrap_or("").as_bytes());
                    }
                }
            }
        }
        super::state::hex(&h.finalize()[..16])
    }

    /// The cameras in the database: (aid, engine id, name).
    pub fn cameras(&self) -> Vec<Cam> {
        self.accessories
            .iter()
            .filter_map(|a| match &a.role {
                Role::Camera { id, name } => Some(Cam { aid: a.aid, id: *id, name: name.clone() }),
                Role::Bridge => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::homekit::tlv::Tlv;

    fn db() -> Database {
        Database::bridge("Home NVR", "AA:BB:CC:DD:EE:FF", &[Cam { aid: 3, id: 1, name: "Back".into() }, Cam { aid: 2, id: 0, name: "Front Door".into() }])
    }

    #[test]
    fn layout_and_ids_are_fixed() {
        let d = db();
        assert_eq!(d.accessories.iter().map(|a| a.aid).collect::<Vec<_>>(), vec![1, 2, 3]);
        let j = d.json();
        let front = &j["accessories"][1];
        assert_eq!(front["aid"], 2);
        let svc: Vec<&str> = front["services"].as_array().unwrap().iter().map(|s| s["type"].as_str().unwrap()).collect();
        assert_eq!(svc, vec!["3E", "85", "110", "112"]);
        // Every iid is unique inside an accessory.
        for a in &d.accessories {
            let mut ids: Vec<u64> = a.services.iter().map(|s| s.iid).chain(a.services.iter().flat_map(|s| s.chars.iter().map(|c| c.iid))).collect();
            let n = ids.len();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), n, "aid {}", a.aid);
        }
        assert_eq!(d.get(2, iid::MOTION_DETECTED).unwrap().ty, ty::MOTION_DETECTED);
        assert_eq!(d.get(2, iid::NAME).unwrap().value, "Front Door");
        // Write-only Identify has no value in the listing.
        assert!(front["services"][0]["characteristics"][0].get("value").is_none());
    }

    #[test]
    fn fingerprint_follows_cameras_and_names_not_values() {
        let a = db();
        let mut b = db();
        b.get_mut(2, iid::MOTION_DETECTED).unwrap().value = true.into();
        assert_eq!(a.fingerprint(), b.fingerprint());
        let c = Database::bridge("Home NVR", "AA:BB:CC:DD:EE:FF", &[Cam { aid: 2, id: 0, name: "Front Door".into() }]);
        assert_ne!(a.fingerprint(), c.fingerprint());
        let r = Database::bridge("Home NVR", "AA:BB:CC:DD:EE:FF", &[Cam { aid: 3, id: 1, name: "Back".into() }, Cam { aid: 2, id: 0, name: "Porch".into() }]);
        assert_ne!(a.fingerprint(), r.fingerprint());
        // A camera's engine id (which may change at restart) does not count.
        let moved = Database::bridge("Home NVR", "AA:BB:CC:DD:EE:FF", &[Cam { aid: 3, id: 7, name: "Back".into() }, Cam { aid: 2, id: 5, name: "Front Door".into() }]);
        assert_eq!(a.fingerprint(), moved.fingerprint());
    }

    #[test]
    fn values_survive_a_rebuild() {
        let mut old = db();
        old.get_mut(3, iid::MOTION_DETECTED).unwrap().value = true.into();
        let mut new = db();
        new.keep_values_from(&old);
        assert_eq!(new.get(3, iid::MOTION_DETECTED).unwrap().value, true);
    }

    #[test]
    fn stream_tlvs_decode() {
        let v = Tlv::decode(&supported_video()).unwrap();
        let cfg = Tlv::decode(v.get(1).unwrap()).unwrap();
        assert_eq!(cfg.byte(1), Some(0), "H.264");
        // 9 sizes, 8 separators between them.
        assert_eq!(cfg.items.iter().filter(|(t, _)| *t == 3).count(), 9);
        let first = Tlv::decode(cfg.get(3).unwrap()).unwrap();
        assert_eq!(first.get(1).unwrap(), &1920u16.to_le_bytes());
        let a = Tlv::decode(&supported_audio()).unwrap();
        // Opus only (two sample rates), no comfort noise.
        let codecs: Vec<u8> = a.items.iter().filter(|(t, _)| *t == 1).map(|(_, v)| Tlv::decode(v).unwrap().byte(1).unwrap()).collect();
        assert_eq!(codecs, vec![3, 3]);
        assert_eq!(a.byte(2), Some(0));
        assert_eq!(supported_rtp(), vec![2, 1, 0]);
    }

    #[test]
    fn standalone_camera_is_one_accessory() {
        let d = Database::standalone(&Cam { aid: 3, id: 5, name: "Front Door".into() }, "AS-3");
        assert_eq!(d.accessories.len(), 1);
        assert_eq!(d.accessories[0].aid, 1);
        assert_eq!(d.get(1, iid::NAME).unwrap().value, "Front Door");
        assert!(d.get(1, iid::SA_VERSION).is_some() && d.get(1, iid::MUTE).is_some() && d.get(1, iid::SETUP_ENDPOINTS).is_some());
        assert_eq!(d.cameras(), vec![Cam { aid: 1, id: 5, name: "Front Door".into() }]);
        let mut ids: Vec<u64> = d.accessories[0].services.iter().map(|s| s.iid).chain(d.accessories[0].services.iter().flat_map(|s| s.chars.iter().map(|c| c.iid))).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn names_are_made_acceptable() {
        let c = |s: &str| clean_name(s);
        assert_eq!(c("Front Door").as_deref(), Some("Front Door"));
        assert_eq!(c("  _Barn/North!  ").as_deref(), Some("Barn North"));
        assert_eq!(c("Bob's Porch").as_deref(), Some("Bob's Porch"));
        assert_eq!(c("'Shed'").as_deref(), Some("Shed"));
        assert_eq!(c("Cam #2 (back) & more.").as_deref(), Some("Cam 2 back more"));
        assert_eq!(c("Café Süd").as_deref(), Some("Café Süd"));
        assert_eq!(c("***"), None);
        assert_eq!(c(&"x".repeat(100)).unwrap().len(), 63);
        // 63 bytes, cut on a character boundary.
        let long = c(&"é".repeat(40)).unwrap();
        assert!(long.len() <= 63 && long.chars().all(|ch| ch == 'é'));
        // A name with nothing usable falls back.
        let d = Database::bridge("***", "AA:BB:CC:DD:EE:FF", &[Cam { aid: 7, id: 0, name: "!!!".into() }]);
        assert_eq!(d.get(1, iid::NAME).unwrap().value, "AmberSight");
        assert_eq!(d.get(7, iid::NAME).unwrap().value, "Camera 7");
    }

    #[test]
    fn values_are_checked() {
        let d = db();
        let active = d.get(2, iid::ACTIVE).unwrap();
        assert!(active.accepts(&json!(1)) && active.accepts(&json!(0)));
        assert!(!active.accepts(&json!(2)) && !active.accepts(&json!("1")));
        let sel = d.get(2, iid::SELECTED_RTP).unwrap();
        assert!(sel.accepts(&json!("AQI=")) && !sel.accepts(&json!("%%%")) && !sel.accepts(&json!(5)));
        let ident = d.get(2, iid::IDENTIFY).unwrap();
        assert!(ident.accepts(&json!(true)) && ident.accepts(&json!(1)) && !ident.accepts(&json!("x")));
    }
}
