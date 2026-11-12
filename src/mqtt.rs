//! MQTT integration: publishes camera motion/event/snapshot topics, optional
//! Home Assistant discovery, and accepts shield and manual-trigger commands.

use crate::config::{slug, AlertConfig};
use crate::engine::{Engine, Event};
use anyhow::{anyhow, Context, Result};
use rumqttc::{Client, Event as MqEvent, LastWill, MqttOptions, Packet, QoS};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

const DEFAULT_PREFIX: &str = "ambersight";
const DEFAULT_TRIGGER_SECONDS: i64 = 20;
const DISCOVERY_PREFIX: &str = "homeassistant";

/// Broker address and credentials parsed from `mqtt://user:pass@host:1883`.
#[derive(Debug, PartialEq, Eq)]
pub struct Broker {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
}

pub fn parse_broker(raw: &str) -> Result<Broker> {
    let u = url::Url::parse(raw).context("invalid mqtt_url")?;
    if u.scheme() != "mqtt" && u.scheme() != "tcp" {
        return Err(anyhow!("mqtt_url must start with mqtt:// (TLS brokers are not supported)"));
    }
    let host = u.host_str().filter(|h| !h.is_empty()).ok_or_else(|| anyhow!("mqtt_url has no host"))?;
    let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
    Ok(Broker {
        host: host.to_string(),
        port: u.port().unwrap_or(1883),
        user: Some(u.username()).filter(|s| !s.is_empty()).map(decode),
        password: u.password().map(decode),
    })
}

pub fn clean_prefix(p: Option<&str>) -> String {
    let p = p.unwrap_or("").trim().trim_matches('/');
    if p.is_empty() { DEFAULT_PREFIX.into() } else { p.into() }
}

pub fn status_topic(prefix: &str) -> String { format!("{prefix}/status") }
pub fn shield_set_topic(prefix: &str) -> String { format!("{prefix}/shield/set") }
pub fn shield_state_topic(prefix: &str) -> String { format!("{prefix}/shield/state") }
pub fn camera_topic(prefix: &str, cam: &str, leaf: &str) -> String { format!("{prefix}/{cam}/{leaf}") }
pub fn trigger_filter(prefix: &str) -> String { format!("{prefix}/+/trigger") }
pub fn wake_filter(prefix: &str) -> String { format!("{prefix}/+/wake") }
/// `<prefix>/signal/<name>/set` with ON or OFF sets a timeline signal.
pub fn signal_filter(prefix: &str) -> String { format!("{prefix}/signal/+/set") }
pub fn signal_name<'a>(prefix: &str, topic: &'a str) -> Option<&'a str> {
    topic.strip_prefix(prefix)?.strip_prefix("/signal/")?.strip_suffix("/set").filter(|n| !n.is_empty() && !n.contains('/'))
}
/// `<prefix>/overlay/<name>/set` with a value sets the named overlay variable
/// the live view shows as `{v:<name>}`; an empty payload clears it.
pub fn overlay_set_filter(prefix: &str) -> String { format!("{prefix}/overlay/+/set") }
pub fn overlay_var_name<'a>(prefix: &str, topic: &'a str) -> Option<&'a str> {
    topic.strip_prefix(prefix)?.strip_prefix("/overlay/")?.strip_suffix("/set").filter(|n| !n.is_empty() && !n.contains('/'))
}
pub fn profile_set_topic(prefix: &str) -> String { format!("{prefix}/profile/set") }
pub fn profile_state_topic(prefix: &str) -> String { format!("{prefix}/profile/state") }
pub fn active_profile_topic(prefix: &str) -> String { format!("{prefix}/profile/active") }
pub fn problems_topic(prefix: &str) -> String { format!("{prefix}/problems") }
/// Whether anyone is home by the phones' presence: `home`, `away` or `unknown` (retained).
pub fn presence_topic(prefix: &str) -> String { format!("{prefix}/presence/state") }
pub fn problems_list_topic(prefix: &str) -> String { format!("{prefix}/problems/list") }
/// The last faces recognized on any camera (retained JSON, see [`face_payload`]).
pub fn faces_topic(prefix: &str) -> String { format!("{prefix}/faces") }
/// One message per recognition, for Home Assistant's event entity (not retained).
pub fn face_event_topic(prefix: &str) -> String { format!("{prefix}/faces/event") }

/// Whose faces an event showed: `state` is the names ("Unknown" when only
/// unrecognized faces were seen), with the details beside it.
pub fn face_payload(camera: &str, event_id: &str, names: &[String], unknown: usize, at: chrono::DateTime<chrono::Utc>) -> serde_json::Value {
    let state = if names.is_empty() { "Unknown".to_string() } else { names.join(", ") };
    json!({
        "state": state,
        "names": names,
        "unknown": unknown,
        "camera": camera,
        "event_id": event_id,
        "time": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// The last plates read on any camera (retained JSON, see [`plate_payload`]).
pub fn plates_topic(prefix: &str) -> String { format!("{prefix}/plates") }
/// One message per event with plates, for Home Assistant's event entity (not retained).
pub fn plate_event_topic(prefix: &str) -> String { format!("{prefix}/plates/event") }

/// The plates an event showed: `state` is the first plate ("ABC123", or
/// "ABC123 (Mom's car)" when known), with every reading beside it. `known`
/// and `unknown` say whether any plate was or was not in the library.
pub fn plate_payload(camera: &str, event_id: &str, plates: &[(String, f32, Option<String>)], at: chrono::DateTime<chrono::Utc>) -> serde_json::Value {
    let shown = |p: &(String, f32, Option<String>)| match &p.2 {
        Some(n) if !n.is_empty() => format!("{} ({n})", p.0),
        _ => p.0.clone(),
    };
    json!({
        "state": plates.first().map(shown).unwrap_or_else(|| "None".into()),
        "plates": plates.iter().map(|p| json!({ "text": p.0, "confidence": ((p.1 as f64) * 100.0).round() / 100.0, "name": p.2, "known": p.2.is_some() })).collect::<Vec<_>>(),
        "known": plates.iter().any(|p| p.2.is_some()),
        "unknown": plates.iter().any(|p| p.2.is_none()),
        "camera": camera,
        "event_id": event_id,
        "time": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// The select option meaning "no manual profile; follow the schedule".
pub const PROFILE_AUTO: &str = "Auto";

pub fn on_off(v: bool) -> &'static str { if v { "ON" } else { "OFF" } }

/// Payload of `<prefix>/shield/set`.
pub fn parse_shield(payload: &[u8]) -> Option<bool> {
    match String::from_utf8_lossy(payload).trim().to_ascii_uppercase().as_str() {
        "ON" | "TRUE" | "1" | "ARM" => Some(true),
        "OFF" | "FALSE" | "0" | "DISARM" => Some(false),
        _ => None,
    }
}

/// Payload of `<prefix>/<camera>/trigger`: seconds, default 20, clamped to 1..=3600.
pub fn parse_trigger_seconds(payload: &[u8]) -> i64 {
    String::from_utf8_lossy(payload)
        .trim()
        .parse::<i64>()
        .unwrap_or(DEFAULT_TRIGGER_SECONDS)
        .clamp(1, 3600)
}

/// Extract the camera slug from `<prefix>/<slug>/<leaf>`.
pub fn camera_command<'a>(prefix: &str, topic: &'a str, leaf: &str) -> Option<&'a str> {
    topic
        .strip_prefix(prefix)?
        .strip_prefix('/')?
        .strip_suffix(leaf)?
        .strip_suffix('/')
        .filter(|s| !s.is_empty() && !s.contains('/'))
}

/// Extract the camera slug from `<prefix>/<slug>/trigger`.
pub fn trigger_camera<'a>(prefix: &str, topic: &'a str) -> Option<&'a str> {
    camera_command(prefix, topic, "trigger")
}

/// Extract the camera slug from `<prefix>/<slug>/wake`.
pub fn wake_camera_slug<'a>(prefix: &str, topic: &'a str) -> Option<&'a str> {
    camera_command(prefix, topic, "wake")
}

/// Payload of `<prefix>/profile/set`: Some(None) = Auto (follow the schedule),
/// Some(Some(name)) = force that profile, None = empty payload.
pub fn parse_profile(payload: &[u8]) -> Option<Option<String>> {
    let t = String::from_utf8_lossy(payload).trim().to_string();
    if t.is_empty() {
        None
    } else if t.eq_ignore_ascii_case(PROFILE_AUTO) {
        Some(None)
    } else {
        Some(Some(t))
    }
}

/// Select state: the forced profile's name, or "Auto".
pub fn profile_select_state(manual: Option<&str>) -> String {
    match manual {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => PROFILE_AUTO.to_string(),
    }
}

/// A camera as far as discovery is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoCam {
    pub slug: String,
    pub name: String,
    pub on_demand: bool,
    /// Face recognition looks at this camera's person events.
    pub faces: bool,
    /// Package detection is on for this camera.
    pub packages: bool,
    /// Named zones (loitering areas, crossing lines).
    pub zones: Vec<DiscoZone>,
    /// License plates are read on this camera.
    pub plates: bool,
    /// Tamper detection is on for this camera.
    pub tamper: bool,
}

/// A named zone as far as discovery is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoZone {
    pub slug: String,
    pub name: String,
    /// A line (crossings are events); otherwise an area (occupied or not).
    pub line: bool,
}

/// `<prefix>/<camera>/zone/<zone>`: zone events (JSON, not retained).
pub fn zone_topic(prefix: &str, cam: &str, zone: &str) -> String { format!("{prefix}/{cam}/zone/{zone}") }
/// `<prefix>/<camera>/zone/<zone>/state`: ON while someone loiters in an area.
pub fn zone_state_topic(prefix: &str, cam: &str, zone: &str) -> String { format!("{prefix}/{cam}/zone/{zone}/state") }

/// What a zone event says (the payload of the zone topics).
pub fn zone_payload(camera: &str, zone: &str, event_type: &str, label: &str, event_id: &str, at: i64) -> serde_json::Value {
    json!({ "event_type": event_type, "camera": camera, "zone": zone, "label": label, "event_id": event_id, "at": at })
}

/// Object kinds the camera itself can report: (leaf, label, name, device_class).
const OBJECT_SENSORS: [(&str, &str, &str, Option<&str>); 3] = [
    ("person", "person", "Person", Some("occupancy")),
    ("vehicle", "vehicle", "Vehicle", None),
    ("animal", "animal", "Animal", None),
];

/// The labels as a comma-separated list; a comma inside a label (a rule or
/// zone name) becomes a space so the list stays unambiguous.
pub fn objects_payload(ev: &Event) -> String {
    ev.objects.iter().map(|o| o.replace(',', " ")).collect::<Vec<_>>().join(",")
}

pub fn event_payload(ev: &Event) -> String {
    serde_json::to_string(&ev.clone().public()).unwrap_or_else(|_| "{}".into())
}

/// Home Assistant discovery messages as (topic, JSON payload).
/// `profiles` are the configured profile names (the select adds "Auto").
pub fn discovery_messages(prefix: &str, cameras: &[DiscoCam], profiles: &[String], with_shield: bool) -> Vec<(String, String)> {
    let avail = status_topic(prefix);
    let mut out = Vec::new();
    let mut add = |kind: &str, id: &str, body: serde_json::Value| {
        out.push((format!("{DISCOVERY_PREFIX}/{kind}/{prefix}_{id}/config"), body.to_string()));
    };
    if with_shield {
        let dev = json!({
            "identifiers": [format!("{prefix}_server")],
            "name": "AmberSight",
            "manufacturer": "AmberSight",
        });
        add("switch", "shield", json!({
            "name": "Shield",
            "unique_id": format!("{prefix}_shield"),
            "command_topic": shield_set_topic(prefix),
            "state_topic": shield_state_topic(prefix),
            "payload_on": "ON",
            "payload_off": "OFF",
            "availability_topic": avail,
            "device": dev,
        }));
        let mut options = vec![PROFILE_AUTO.to_string()];
        options.extend(profiles.iter().filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case(PROFILE_AUTO)).cloned());
        add("select", "profile", json!({
            "name": "Profile",
            "unique_id": format!("{prefix}_profile"),
            "command_topic": profile_set_topic(prefix),
            "state_topic": profile_state_topic(prefix),
            "options": options,
            "availability_topic": avail,
            "device": dev,
        }));
        add("sensor", "active_profile", json!({
            "name": "Active profile",
            "unique_id": format!("{prefix}_active_profile"),
            "state_topic": active_profile_topic(prefix),
            "entity_category": "diagnostic",
            "availability_topic": avail,
            "device": dev,
        }));
    }
    // System problems (camera offline, no recordings, disk nearly full).
    add("binary_sensor", "problem", json!({
        "name": "Problem",
        "unique_id": format!("{prefix}_problem"),
        "state_topic": problems_topic(prefix),
        "payload_on": "ON",
        "payload_off": "OFF",
        "device_class": "problem",
        "json_attributes_topic": problems_list_topic(prefix),
        "availability_topic": avail,
        "device": {
            "identifiers": [format!("{prefix}_server")],
            "name": "AmberSight",
            "manufacturer": "AmberSight",
        },
    }));
    // Faces: who was recognized last (anywhere), and an event entity that
    // fires on every recognition (automations: "when Alex is seen").
    if cameras.iter().any(|c| c.faces) {
        let dev = json!({
            "identifiers": [format!("{prefix}_server")],
            "name": "AmberSight",
            "manufacturer": "AmberSight",
        });
        add("sensor", "last_face", json!({
            "name": "Last face",
            "unique_id": format!("{prefix}_last_face"),
            "state_topic": faces_topic(prefix),
            "value_template": "{{ value_json.state }}",
            "json_attributes_topic": faces_topic(prefix),
            "icon": "mdi:face-recognition",
            "availability_topic": avail,
            "device": dev,
        }));
        add("event", "face", json!({
            "name": "Face",
            "unique_id": format!("{prefix}_face"),
            "state_topic": face_event_topic(prefix),
            "event_types": ["recognized", "unknown"],
            "icon": "mdi:face-recognition",
            "availability_topic": avail,
            "device": dev,
        }));
    }
    // Plates: the last plate read (anywhere) and an event entity per event.
    if cameras.iter().any(|c| c.plates) {
        let dev = json!({
            "identifiers": [format!("{prefix}_server")],
            "name": "AmberSight",
            "manufacturer": "AmberSight",
        });
        add("sensor", "last_plate", json!({
            "name": "Last plate",
            "unique_id": format!("{prefix}_last_plate"),
            "state_topic": plates_topic(prefix),
            "value_template": "{{ value_json.state }}",
            "json_attributes_topic": plates_topic(prefix),
            "icon": "mdi:car-license-plate",
            "availability_topic": avail,
            "device": dev,
        }));
        add("event", "plate", json!({
            "name": "Plate",
            "unique_id": format!("{prefix}_plate"),
            "state_topic": plate_event_topic(prefix),
            "event_types": ["known", "unknown"],
            "icon": "mdi:car-license-plate",
            "availability_topic": avail,
            "device": dev,
        }));
    }
    for DiscoCam { slug: cam, name, on_demand, faces, packages, zones, plates, tamper } in cameras {
        let dev = json!({
            "identifiers": [format!("{prefix}_{cam}")],
            "name": name,
            "manufacturer": "AmberSight",
            "via_device": format!("{prefix}_server"),
        });
        let binary = |label: &str, leaf: &str, class: Option<&str>, diagnostic: bool| {
            let mut v = json!({
                "name": label,
                "unique_id": format!("{prefix}_{cam}_{leaf}"),
                "state_topic": camera_topic(prefix, cam, leaf),
                "payload_on": "ON",
                "payload_off": "OFF",
                "availability_topic": avail,
                "device": dev,
            });
            if let Some(c) = class {
                v["device_class"] = json!(c);
            }
            if diagnostic {
                v["entity_category"] = json!("diagnostic");
            }
            v
        };
        add("binary_sensor", &format!("{cam}_motion"), binary("Motion", "motion", Some("motion"), false));
        for (leaf, _, label, class) in OBJECT_SENSORS {
            add("binary_sensor", &format!("{cam}_{leaf}"), binary(label, leaf, class, false));
        }
        if *packages {
            let mut v = binary("Package", "package", None, false);
            v["icon"] = json!("mdi:package-variant-closed");
            add("binary_sensor", &format!("{cam}_package"), v);
        }
        // Named zones: an area is on while someone loiters in it; a line fires
        // an event for each crossing ("in" along the arrow, "out" against it).
        for z in zones {
            let zs = &z.slug;
            if z.line {
                add("event", &format!("{cam}_zone_{zs}"), json!({
                    "name": z.name,
                    "unique_id": format!("{prefix}_{cam}_zone_{zs}"),
                    "state_topic": zone_topic(prefix, cam, zs),
                    "event_types": ["in", "out"],
                    "icon": "mdi:vector-line",
                    "availability_topic": avail,
                    "device": dev,
                }));
            } else {
                add("binary_sensor", &format!("{cam}_zone_{zs}"), json!({
                    "name": z.name,
                    "unique_id": format!("{prefix}_{cam}_zone_{zs}"),
                    "state_topic": zone_state_topic(prefix, cam, zs),
                    "payload_on": "ON",
                    "payload_off": "OFF",
                    "device_class": "occupancy",
                    "json_attributes_topic": zone_topic(prefix, cam, zs),
                    "availability_topic": avail,
                    "device": dev,
                }));
            }
        }
        if *tamper {
            let mut v = binary("Tamper", "tamper", Some("tamper"), false);
            v["icon"] = json!("mdi:camera-off");
            add("binary_sensor", &format!("{cam}_tamper"), v);
        }
        add("binary_sensor", &format!("{cam}_online"), binary("Online", "online", Some("connectivity"), true));
        if *on_demand {
            add("binary_sensor", &format!("{cam}_asleep"), binary("Asleep", "asleep", None, true));
            add("button", &format!("{cam}_wake"), json!({
                "name": "Wake",
                "unique_id": format!("{prefix}_{cam}_wake"),
                "command_topic": camera_topic(prefix, cam, "wake"),
                "payload_press": "PRESS",
                "availability_topic": avail,
                "device": dev,
            }));
        }
        if *faces {
            add("event", &format!("{cam}_face"), json!({
                "name": "Face",
                "unique_id": format!("{prefix}_{cam}_face"),
                "state_topic": camera_topic(prefix, cam, "face"),
                "event_types": ["recognized", "unknown"],
                "icon": "mdi:face-recognition",
                "availability_topic": avail,
                "device": dev,
            }));
            add("sensor", &format!("{cam}_last_face"), json!({
                "name": "Last face",
                "unique_id": format!("{prefix}_{cam}_last_face"),
                "state_topic": camera_topic(prefix, cam, "faces"),
                "value_template": "{{ value_json.state }}",
                "json_attributes_topic": camera_topic(prefix, cam, "faces"),
                "icon": "mdi:face-recognition",
                "availability_topic": avail,
                "device": dev,
            }));
        }
        if *plates {
            add("event", &format!("{cam}_plate"), json!({
                "name": "Plate",
                "unique_id": format!("{prefix}_{cam}_plate"),
                "state_topic": camera_topic(prefix, cam, "plate"),
                "event_types": ["known", "unknown"],
                "icon": "mdi:car-license-plate",
                "availability_topic": avail,
                "device": dev,
            }));
            add("sensor", &format!("{cam}_last_plate"), json!({
                "name": "Last plate",
                "unique_id": format!("{prefix}_{cam}_last_plate"),
                "state_topic": camera_topic(prefix, cam, "plates"),
                "value_template": "{{ value_json.state }}",
                "json_attributes_topic": camera_topic(prefix, cam, "plates"),
                "icon": "mdi:car-license-plate",
                "availability_topic": avail,
                "device": dev,
            }));
        }
        add("button", &format!("{cam}_record_clip"), json!({
            "name": "Record clip",
            "unique_id": format!("{prefix}_{cam}_record_clip"),
            "command_topic": camera_topic(prefix, cam, "trigger"),
            "payload_press": DEFAULT_TRIGGER_SECONDS.to_string(),
            "availability_topic": avail,
            "device": dev,
        }));
        add("image", &format!("{cam}_snapshot"), json!({
            "name": "Snapshot",
            "unique_id": format!("{prefix}_{cam}_snapshot"),
            "image_topic": camera_topic(prefix, cam, "snapshot"),
            "content_type": "image/jpeg",
            "availability_topic": avail,
            "device": dev,
        }));
    }
    out
}

/// Topic of the "Someone home" binary sensor's discovery message.
pub fn presence_discovery_topic(prefix: &str) -> String {
    format!("{DISCOVERY_PREFIX}/binary_sensor/{prefix}_someone_home/config")
}

/// Home Assistant discovery for "Someone home" (phones' presence): on for
/// `home`, off for `away`, unavailable while `unknown` (or the server is offline).
pub fn presence_discovery(prefix: &str) -> String {
    json!({
        "name": "Someone home",
        "unique_id": format!("{prefix}_someone_home"),
        "state_topic": presence_topic(prefix),
        "payload_on": "home",
        "payload_off": "away",
        "device_class": "presence",
        "availability_mode": "all",
        "availability": [
            { "topic": status_topic(prefix) },
            { "topic": presence_topic(prefix), "value_template": "{{ 'offline' if value == 'unknown' else 'online' }}" },
        ],
        "device": {
            "identifiers": [format!("{prefix}_server")],
            "name": "AmberSight",
            "manufacturer": "AmberSight",
        },
    })
    .to_string()
}

/// What to publish on the "Someone home" discovery topic, if anything:
/// the sensor while presence is on, and the empty (removing) message when
/// it is off, also once per run when nothing was announced yet (a sensor
/// left from before a restart goes). `announced`: what was last published
/// (None: nothing yet).
pub fn presence_discovery_update(prefix: &str, announced: Option<bool>, on: bool) -> Option<String> {
    if announced == Some(on) {
        return None;
    }
    Some(if on { presence_discovery(prefix) } else { String::new() })
}

/// Publisher used by the alert path.
pub struct MqttHandle {
    client: Client,
    prefix: String,
}

impl MqttHandle {
    fn put(&self, topic: String, payload: impl Into<Vec<u8>>, retain: bool) {
        if let Err(e) = self.client.try_publish(topic, QoS::AtLeastOnce, retain, payload) {
            warn!("mqtt: publish dropped: {e}");
        }
    }

    /// The system problems now (retained; sent every minute, so a broker
    /// restart or a reconnect is caught up quickly).
    pub fn publish_problems(&self, active: &[String]) {
        self.put(problems_list_topic(&self.prefix), json!({ "problems": active }).to_string(), true);
        self.put(problems_topic(&self.prefix), on_off(!active.is_empty()), true);
    }

    /// Publish a finished event: JSON, objects list and thumbnail.
    pub fn publish_event(&self, ev: &Event, snapshot: Option<Vec<u8>>) {
        let cam = slug(&ev.camera);
        // Snapshot first so subscribers holding the event can already fetch it.
        if let Some(jpeg) = snapshot {
            self.put(camera_topic(&self.prefix, &cam, "snapshot"), jpeg, true);
        }
        self.put(camera_topic(&self.prefix, &cam, "objects"), objects_payload(ev), true);
        self.put(camera_topic(&self.prefix, &cam, "event"), event_payload(ev), false);
    }

    /// A zone event: loitering in an area (`event_type` "loitering") or a
    /// crossing of a line ("in" / "out"). To the zone's topic and the camera's
    /// `zone` topic.
    pub fn publish_zone(&self, camera: &str, zone: &str, event_type: &str, label: &str, event_id: &str, at: i64) {
        let cam = slug(camera);
        let p = zone_payload(camera, zone, event_type, label, event_id, at).to_string();
        self.put(zone_topic(&self.prefix, &cam, &slug(zone)), p.clone(), false);
        self.put(camera_topic(&self.prefix, &cam, "zone"), p, false);
    }

    /// Faces recognized in an event: the camera's and the server's "last
    /// face" (retained) and one face event (recognized / unknown).
    pub fn publish_faces(&self, camera: &str, event_id: &str, names: &[String], unknown: usize) {
        let p = face_payload(camera, event_id, names, unknown, chrono::Utc::now());
        self.put(camera_topic(&self.prefix, &slug(camera), "faces"), p.to_string(), true);
        self.put(faces_topic(&self.prefix), p.to_string(), true);
        // Event entities: every recognition fires, the same name twice too.
        let mut e = p;
        e["event_type"] = json!(if names.is_empty() { "unknown" } else { "recognized" });
        e["name"] = json!(names.first().cloned().unwrap_or_default());
        self.put(camera_topic(&self.prefix, &slug(camera), "face"), e.to_string(), false);
        self.put(face_event_topic(&self.prefix), e.to_string(), false);
    }

    /// Plates read in an event: the camera's and the server's "last plate"
    /// (retained) and one plate event (known / unknown).
    pub fn publish_plates(&self, camera: &str, event_id: &str, plates: &[crate::engine::lpr::PlateHit]) {
        let list: Vec<(String, f32, Option<String>)> = plates.iter().map(|p| (p.text.clone(), p.confidence, p.known.as_ref().map(|k| k.1.clone()))).collect();
        let p = plate_payload(camera, event_id, &list, chrono::Utc::now());
        self.put(camera_topic(&self.prefix, &slug(camera), "plates"), p.to_string(), true);
        self.put(plates_topic(&self.prefix), p.to_string(), true);
        let mut e = p;
        e["event_type"] = json!(if list.iter().any(|p| p.2.is_none()) { "unknown" } else { "known" });
        self.put(camera_topic(&self.prefix, &slug(camera), "plate"), e.to_string(), false);
        self.put(plate_event_topic(&self.prefix), e.to_string(), false);
    }
}

/// Start the client when `mqtt_url` is configured. Returns None otherwise.
pub fn start(engine: &Arc<Engine>) -> Option<MqttHandle> {
    let alerts: AlertConfig = engine.config.lock().unwrap().alerts.clone();
    let raw = alerts.mqtt_url.filter(|u| !u.trim().is_empty())?;
    let broker = match parse_broker(&raw) {
        Ok(b) => b,
        Err(e) => {
            warn!("mqtt disabled: {e:#}");
            return None;
        }
    };
    let prefix = clean_prefix(alerts.mqtt_prefix.as_deref());
    let discovery = alerts.mqtt_discovery;
    let mut opts = MqttOptions::new(format!("ambersight-{}", std::process::id()), &broker.host, broker.port);
    opts.set_keep_alive(Duration::from_secs(30));
    opts.set_max_packet_size(8 * 1024 * 1024, 8 * 1024 * 1024);
    if let Some(u) = &broker.user {
        opts.set_credentials(u.clone(), broker.password.clone().unwrap_or_default());
    }
    opts.set_last_will(LastWill::new(status_topic(&prefix), "offline", QoS::AtLeastOnce, true));
    let (client, mut connection) = Client::new(opts, 256);
    info!("mqtt: connecting to {}:{} with prefix '{}'", broker.host, broker.port, prefix);

    let announce = Arc::new(AtomicBool::new(true));

    // Connection thread: drives the socket, reconnects, handles commands.
    {
        let engine = engine.clone();
        let client = client.clone();
        let prefix = prefix.clone();
        let announce = announce.clone();
        std::thread::spawn(move || {
            let mut was_down = false;
            for note in connection.iter() {
                match note {
                    Ok(MqEvent::Incoming(Packet::ConnAck(_))) => {
                        info!("mqtt: connected");
                        was_down = false;
                        let _ = client.try_publish(status_topic(&prefix), QoS::AtLeastOnce, true, "online");
                        let _ = client.try_subscribe(shield_set_topic(&prefix), QoS::AtLeastOnce);
                        let _ = client.try_subscribe(trigger_filter(&prefix), QoS::AtLeastOnce);
                        let _ = client.try_subscribe(wake_filter(&prefix), QoS::AtLeastOnce);
                        let _ = client.try_subscribe(signal_filter(&prefix), QoS::AtLeastOnce);
                        let _ = client.try_subscribe(profile_set_topic(&prefix), QoS::AtLeastOnce);
                        let _ = client.try_subscribe(overlay_set_filter(&prefix), QoS::AtLeastOnce);
                        announce.store(true, Ordering::Relaxed);
                    }
                    Ok(MqEvent::Incoming(Packet::Publish(p))) => handle_command(&engine, &prefix, &p.topic, &p.payload),
                    Ok(_) => {}
                    Err(e) => {
                        if !was_down {
                            warn!("mqtt: connection lost: {e}; retrying");
                            was_down = true;
                        }
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        });
    }

    // State thread: polls motion/shield and publishes changes.
    {
        let engine = engine.clone();
        let client = client.clone();
        let prefix = prefix.clone();
        std::thread::spawn(move || poll_state(engine, client, prefix, discovery, announce));
    }
    Some(MqttHandle { client, prefix })
}

fn camera_id_by_slug(engine: &Engine, cam: &str) -> Option<usize> {
    engine
        .cameras
        .read()
        .unwrap()
        .iter()
        .flatten()
        .find(|h| slug(&h.state.read().unwrap().name) == cam)
        .map(|h| h.id)
}

fn handle_command(engine: &Arc<Engine>, prefix: &str, topic: &str, payload: &[u8]) {
    if topic == shield_set_topic(prefix) {
        match parse_shield(payload) {
            Some(armed) => {
                engine.set_shield(armed, "(mqtt)", "");
            }
            None => warn!("mqtt: ignoring shield payload {:?}", String::from_utf8_lossy(payload)),
        }
    } else if topic == profile_set_topic(prefix) {
        match parse_profile(payload) {
            Some(choice) => {
                if let Err(e) = engine.set_manual_profile(choice, None, "(mqtt)", "") {
                    warn!("mqtt: {e}");
                }
            }
            None => warn!("mqtt: ignoring empty profile payload"),
        }
    } else if let Some(name) = signal_name(prefix, topic) {
        match parse_shield(payload) {
            Some(on) => {
                if !engine.set_signal(name, on, "mqtt") {
                    warn!("mqtt: bad signal name {name:?}");
                }
            }
            None => warn!("mqtt: signal {name}: payload should be ON or OFF"),
        }
    } else if let Some(name) = overlay_var_name(prefix, topic) {
        engine.set_overlay_var(name, &String::from_utf8_lossy(payload));
    } else if let Some(cam) = trigger_camera(prefix, topic) {
        match camera_id_by_slug(engine, cam) {
            Some(id) => {
                let secs = parse_trigger_seconds(payload);
                info!("mqtt: manual clip on '{cam}' for {secs}s");
                engine.manual_clip(id, secs);
            }
            None => warn!("mqtt: trigger for unknown camera '{cam}'"),
        }
    } else if let Some(cam) = wake_camera_slug(prefix, topic) {
        match camera_id_by_slug(engine, cam) {
            Some(id) => {
                info!("mqtt: waking '{cam}'");
                engine.wake_camera(id);
            }
            None => warn!("mqtt: wake for unknown camera '{cam}'"),
        }
    }
}

fn poll_state(engine: Arc<Engine>, client: Client, prefix: String, discovery: bool, announce: Arc<AtomicBool>) {
    // Last published payload per state topic; cleared on reconnect.
    let mut last: HashMap<String, String> = HashMap::new();
    let mut announced: Option<(Vec<DiscoCam>, Vec<String>)> = None;
    // Whether the "Someone home" sensor was announced (None: not yet decided).
    let mut presence_announced: Option<bool> = None;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let mut cams: Vec<DiscoCam> = Vec::new();
        let (faces_cfg, lpr_cfg) = {
            let c = engine.config.lock().unwrap();
            (c.faces.clone(), c.lpr.clone())
        };
        let mut states: Vec<(String, String)> = Vec::new();
        for h in engine.cameras.read().unwrap().iter().flatten() {
            let on_demand = h.running.read().unwrap().on_demand;
            let st = h.state.read().unwrap();
            let s = slug(&st.name);
            let labels = st.camera_detections.labels();
            states.push((camera_topic(&prefix, &s, "motion"), on_off(st.motion).into()));
            states.push((camera_topic(&prefix, &s, "online"), on_off(st.online).into()));
            for (leaf, label, _, _) in OBJECT_SENSORS {
                states.push((camera_topic(&prefix, &s, leaf), on_off(labels.iter().any(|l| l == label)).into()));
            }
            if on_demand {
                states.push((camera_topic(&prefix, &s, "asleep"), on_off(st.sleeping).into()));
            }
            let faces = faces_cfg.wants(&st.name);
            let plates = lpr_cfg.for_camera(&st.name).is_some();
            let packages = h.cfg.read().unwrap().packages == Some(true);
            if packages {
                states.push((camera_topic(&prefix, &s, "package"), on_off(crate::engine::packages::present(&engine, &st.name) > 0).into()));
            }
            let tamper = h.running.read().unwrap().tamper.as_ref().is_some_and(|t| t.enabled);
            if tamper {
                states.push((camera_topic(&prefix, &s, "tamper"), on_off(engine.tamper_active(h.id)).into()));
            }
            let zones: Vec<DiscoZone> = h
                .cfg
                .read()
                .unwrap()
                .named_zones
                .iter()
                .filter(|z| z.enabled)
                .map(|z| DiscoZone { slug: slug(&z.name), name: z.name.clone(), line: z.is_line() })
                .collect();
            for z in zones.iter().filter(|z| !z.line) {
                let on = st.zones_occupied.iter().any(|n| slug(n) == z.slug);
                states.push((zone_state_topic(&prefix, &s, &z.slug), on_off(on).into()));
            }
            cams.push(DiscoCam { slug: s, name: st.name.clone(), on_demand, faces, packages, zones, plates, tamper });
        }
        let profiles: Vec<String> =
            engine.config.lock().unwrap().automation.profiles.iter().map(|p| p.name.clone()).collect();
        let manual = engine.manual_profile.lock().unwrap().as_ref().map(|(n, _)| n.clone());
        states.push((profile_state_topic(&prefix), profile_select_state(manual.as_deref())));
        let active = engine.active_profile().name;
        states.push((active_profile_topic(&prefix), if active.is_empty() { "None".into() } else { active }));
        states.push((shield_state_topic(&prefix), on_off(engine.armed_now()).into()));
        let presence_on = engine.config.lock().unwrap().presence.enabled;
        states.push((presence_topic(&prefix), crate::engine::presence::state_word(engine.presence_home()).into()));

        if announce.swap(false, Ordering::Relaxed) {
            last.clear();
            announced = None;
            presence_announced = None;
        }
        // The sensor exists while presence is on; turning it off removes it.
        if discovery {
            if let Some(body) = presence_discovery_update(&prefix, presence_announced, presence_on) {
                if client.try_publish(presence_discovery_topic(&prefix), QoS::AtLeastOnce, true, body).is_ok() {
                    presence_announced = Some(presence_on);
                }
            }
        }
        if discovery && announced.as_ref() != Some(&(cams.clone(), profiles.clone())) {
            for (t, p) in discovery_messages(&prefix, &cams, &profiles, true) {
                let _ = client.try_publish(t, QoS::AtLeastOnce, true, p);
            }
            announced = Some((cams, profiles));
        }
        for (topic, payload) in states {
            if last.get(&topic) != Some(&payload)
                && client.try_publish(topic.clone(), QoS::AtLeastOnce, true, payload.clone()).is_ok()
            {
                last.insert(topic, payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> Event {
        Event {
            id: "e1".into(),
            camera_id: 0,
            camera: "Front Door".into(),
            start: 100,
            end: 120,
            score: 0.5,
            clip: None,
            thumb: None,
            objects: vec!["person".into(), "car".into()],
            protect: false,
        }
    }

    #[test]
    fn broker_parsing() {
        let b = parse_broker("mqtt://us%40er:p%3Aw@10.0.0.5:1884").unwrap();
        assert_eq!(b, Broker { host: "10.0.0.5".into(), port: 1884, user: Some("us@er".into()), password: Some("p:w".into()) });
        let b = parse_broker("mqtt://broker.lan").unwrap();
        assert_eq!((b.port, b.user), (1883, None));
        assert!(parse_broker("http://x").is_err());
        assert!(parse_broker("garbage").is_err());
    }

    #[test]
    fn topics_and_payloads() {
        assert_eq!(clean_prefix(None), "ambersight");
        assert_eq!(clean_prefix(Some("/home/nvr/")), "home/nvr");
        assert_eq!(camera_topic("a", "front_door", "motion"), "a/front_door/motion");
        assert_eq!(trigger_camera("a", "a/front_door/trigger"), Some("front_door"));
        assert_eq!(trigger_camera("a", "a/x/y/trigger"), None);
        assert_eq!(trigger_camera("a", "b/x/trigger"), None);
        assert_eq!(parse_shield(b" off\n"), Some(false));
        assert_eq!(parse_shield(b"ON"), Some(true));
        assert_eq!(parse_shield(b"maybe"), None);
        assert_eq!(parse_trigger_seconds(b""), 20);
        assert_eq!(parse_trigger_seconds(b"45"), 45);
        assert_eq!(parse_trigger_seconds(b"999999"), 3600);
        assert_eq!(objects_payload(&ev()), "person,car");
        let mut e = ev();
        e.objects = vec!["rule: Truck, front".into(), "person".into()];
        assert_eq!(objects_payload(&e), "rule: Truck  front,person");
        let j: serde_json::Value = serde_json::from_str(&event_payload(&ev())).unwrap();
        assert_eq!(j["camera"], "Front Door");
    }

    fn cam(slug: &str, name: &str, on_demand: bool) -> DiscoCam {
        DiscoCam { slug: slug.into(), name: name.into(), on_demand, faces: false, packages: false, zones: Vec::new(), plates: false, tamper: false }
    }

    #[test]
    fn tamper_sensor_only_where_it_is_on() {
        let on = DiscoCam { tamper: true, ..cam("door", "Door", false) };
        let msgs = discovery_messages("p", &[on, cam("yard", "Yard", false)], &[], false);
        let v = find(&msgs, "homeassistant/binary_sensor/p_door_tamper/config");
        assert_eq!(v["device_class"], "tamper");
        assert_eq!(v["state_topic"], "p/door/tamper");
        assert!(!msgs.iter().any(|(t, _)| t.contains("yard_tamper")));
    }

    #[test]
    fn faces_discovery_and_payload() {
        let msgs = discovery_messages("p", &[DiscoCam { faces: true, ..cam("door", "Door", false) }, cam("yard", "Yard", false)], &[], false);
        let v = find(&msgs, "homeassistant/sensor/p_door_last_face/config");
        assert_eq!(v["state_topic"], "p/door/faces");
        assert_eq!(v["json_attributes_topic"], "p/door/faces");
        assert_eq!(v["value_template"], "{{ value_json.state }}");
        assert!(msgs.iter().all(|(t, _)| !t.contains("p_yard_last_face")), "only cameras with faces");
        let v = find(&msgs, "homeassistant/event/p_door_face/config");
        assert_eq!(v["state_topic"], "p/door/face");
        assert_eq!(v["event_types"], json!(["recognized", "unknown"]));
        let v = find(&msgs, "homeassistant/sensor/p_last_face/config");
        assert_eq!(v["state_topic"], "p/faces");
        assert_eq!(v["device"]["identifiers"][0], "p_server");
        let v = find(&msgs, "homeassistant/event/p_face/config");
        assert_eq!(v["state_topic"], "p/faces/event");
        assert_eq!(v["event_types"], json!(["recognized", "unknown"]));
        // Package sensor only where package detection is on.
        let msgs = discovery_messages("p", &[DiscoCam { packages: true, ..cam("door", "Door", false) }], &[], false);
        let v = find(&msgs, "homeassistant/binary_sensor/p_door_package/config");
        assert_eq!(v["state_topic"], "p/door/package");
        assert_eq!(v["payload_on"], "ON");
        // No camera with faces: no face entities at all.
        let msgs = discovery_messages("p", &[cam("yard", "Yard", false)], &[], false);
        assert!(msgs.iter().all(|(t, _)| !t.contains("face")));
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-03T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let p = face_payload("Door", "e1", &["Ann".into(), "Bo".into()], 1, at);
        assert_eq!(p["state"], "Ann, Bo");
        assert_eq!(p["unknown"], 1);
        assert_eq!(p["time"], "2026-11-03T12:00:00Z");
        assert_eq!(face_payload("Door", "e1", &[], 2, at)["state"], "Unknown");
    }

    #[test]
    fn zone_discovery_and_payload() {
        let zones = vec![
            DiscoZone { slug: "driveway".into(), name: "Driveway".into(), line: false },
            DiscoZone { slug: "gate".into(), name: "Gate".into(), line: true },
        ];
        let msgs = discovery_messages("p", &[DiscoCam { zones, ..cam("yard", "Yard", false) }], &[], false);
        let v = find(&msgs, "homeassistant/binary_sensor/p_yard_zone_driveway/config");
        assert_eq!(v["state_topic"], "p/yard/zone/driveway/state");
        assert_eq!(v["name"], "Driveway");
        assert_eq!(v["device_class"], "occupancy");
        let v = find(&msgs, "homeassistant/event/p_yard_zone_gate/config");
        assert_eq!(v["state_topic"], "p/yard/zone/gate");
        assert_eq!(v["event_types"], json!(["in", "out"]));
        // No zones: no zone entities.
        let msgs = discovery_messages("p", &[cam("yard", "Yard", false)], &[], false);
        assert!(msgs.iter().all(|(t, _)| !t.contains("_zone_")));
        let j = zone_payload("Yard", "Gate", "in", "person", "yard-1-zone1", 99);
        assert_eq!(j["event_type"], "in");
        assert_eq!(j["zone"], "Gate");
        assert_eq!(zone_topic("p", "yard", "gate"), "p/yard/zone/gate");
    }

    #[test]
    fn plates_discovery_and_payload() {
        let msgs = discovery_messages("p", &[DiscoCam { plates: true, ..cam("drive", "Drive", false) }, cam("yard", "Yard", false)], &[], false);
        let v = find(&msgs, "homeassistant/sensor/p_drive_last_plate/config");
        assert_eq!(v["state_topic"], "p/drive/plates");
        assert_eq!(v["value_template"], "{{ value_json.state }}");
        assert!(msgs.iter().all(|(t, _)| !t.contains("p_yard_last_plate")), "only cameras with plates");
        let v = find(&msgs, "homeassistant/event/p_drive_plate/config");
        assert_eq!(v["state_topic"], "p/drive/plate");
        assert_eq!(v["event_types"], json!(["known", "unknown"]));
        assert_eq!(find(&msgs, "homeassistant/sensor/p_last_plate/config")["state_topic"], "p/plates");
        assert_eq!(find(&msgs, "homeassistant/event/p_plate/config")["state_topic"], "p/plates/event");
        let msgs = discovery_messages("p", &[cam("yard", "Yard", false)], &[], false);
        assert!(msgs.iter().all(|(t, _)| !t.contains("plate")));
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-03T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let p = plate_payload("Drive", "e1", &[("ABC123".into(), 0.912, Some("Mom's car".into())), ("XYZ789".into(), 0.8, None)], at);
        assert_eq!(p["state"], "ABC123 (Mom's car)");
        assert_eq!((p["known"].as_bool(), p["unknown"].as_bool()), (Some(true), Some(true)));
        assert_eq!(p["plates"][0]["confidence"], 0.91);
        assert_eq!(p["plates"][1]["known"], false);
        assert_eq!(p["time"], "2026-11-03T12:00:00Z");
        assert_eq!(plate_payload("Drive", "e1", &[], at)["state"], "None");
    }

    fn find(msgs: &[(String, String)], topic: &str) -> serde_json::Value {
        let (_, p) = msgs.iter().find(|(t, _)| t == topic).unwrap_or_else(|| panic!("no {topic}"));
        serde_json::from_str(p).unwrap()
    }

    #[test]
    fn command_helpers() {
        assert_eq!(wake_camera_slug("a", "a/front_door/wake"), Some("front_door"));
        assert_eq!(wake_camera_slug("a", "a/front_door/trigger"), None);
        assert_eq!(wake_camera_slug("a", "a/x/y/wake"), None);
        assert_eq!(wake_camera_slug("a", "a//wake"), None);
        assert_eq!(parse_profile(b"Auto"), Some(None));
        assert_eq!(parse_profile(b" auto\n"), Some(None));
        assert_eq!(parse_profile(b"Night"), Some(Some("Night".into())));
        assert_eq!(parse_profile(b"  "), None);
        assert_eq!(profile_select_state(None), "Auto");
        assert_eq!(profile_select_state(Some("")), "Auto");
        assert_eq!(profile_select_state(Some("Away")), "Away");
        assert_eq!(wake_filter("a"), "a/+/wake");
        assert_eq!(signal_filter("a"), "a/signal/+/set");
        assert_eq!(signal_name("a", "a/signal/Front door/set"), Some("Front door"));
        assert_eq!(signal_name("a", "a/signal//set"), None);
        assert_eq!(signal_name("a", "b/signal/x/set"), None);
        assert_eq!(profile_set_topic("a"), "a/profile/set");
        assert_eq!(overlay_set_filter("a"), "a/overlay/+/set");
        assert_eq!(overlay_var_name("a", "a/overlay/outside_temp/set"), Some("outside_temp"));
        assert_eq!(overlay_var_name("a", "a/overlay//set"), None);
        assert_eq!(overlay_var_name("a", "a/overlay/a/b/set"), None);
        assert_eq!(overlay_var_name("a", "b/overlay/x/set"), None);
    }

    #[test]
    fn discovery_json() {
        let profiles = vec!["Home".to_string(), "Away".to_string()];
        let msgs = discovery_messages("ambersight", &[cam("front_door", "Front Door", false)], &profiles, true);
        // shield, profile, active profile, problem + motion, 3 objects, online, record clip, snapshot
        assert_eq!(msgs.len(), 4 + 7);
        let v = find(&msgs, "homeassistant/binary_sensor/ambersight_problem/config");
        assert_eq!(v["device_class"], "problem");
        assert_eq!(v["state_topic"], "ambersight/problems");
        assert_eq!(v["json_attributes_topic"], "ambersight/problems/list");
        let v = find(&msgs, "homeassistant/binary_sensor/ambersight_front_door_motion/config");
        assert_eq!(v["device_class"], "motion");
        assert_eq!(v["state_topic"], "ambersight/front_door/motion");
        assert_eq!(v["availability_topic"], "ambersight/status");
        assert_eq!(v["device"]["name"], "Front Door");
        for (leaf, name) in [("person", "Person"), ("vehicle", "Vehicle"), ("animal", "Animal")] {
            let v = find(&msgs, &format!("homeassistant/binary_sensor/ambersight_front_door_{leaf}/config"));
            assert_eq!(v["name"], name);
            assert_eq!(v["unique_id"], format!("ambersight_front_door_{leaf}"));
            assert_eq!(v["state_topic"], format!("ambersight/front_door/{leaf}"));
            assert_eq!(v["payload_on"], "ON");
        }
        let v = find(&msgs, "homeassistant/binary_sensor/ambersight_front_door_online/config");
        assert_eq!(v["device_class"], "connectivity");
        assert_eq!(v["entity_category"], "diagnostic");
        assert_eq!(v["state_topic"], "ambersight/front_door/online");
        assert!(msgs.iter().all(|(t, _)| !t.contains("asleep") && !t.contains("_wake/")));
        let v = find(&msgs, "homeassistant/button/ambersight_front_door_record_clip/config");
        assert_eq!(v["command_topic"], "ambersight/front_door/trigger");
        assert_eq!(v["payload_press"], "20");
        let v = find(&msgs, "homeassistant/image/ambersight_front_door_snapshot/config");
        assert_eq!(v["image_topic"], "ambersight/front_door/snapshot");
        let v = find(&msgs, "homeassistant/switch/ambersight_shield/config");
        assert_eq!(v["command_topic"], "ambersight/shield/set");
        let v = find(&msgs, "homeassistant/select/ambersight_profile/config");
        assert_eq!(v["options"], json!(["Auto", "Home", "Away"]));
        assert_eq!(v["command_topic"], "ambersight/profile/set");
        assert_eq!(v["state_topic"], "ambersight/profile/state");
        assert_eq!(v["device"]["identifiers"][0], "ambersight_server");
        let v = find(&msgs, "homeassistant/sensor/ambersight_active_profile/config");
        assert_eq!(v["state_topic"], "ambersight/profile/active");
        assert_eq!(v["entity_category"], "diagnostic");
        assert_eq!(discovery_messages("p", &[], &[], false).len(), 1, "the problem sensor only");
    }

    #[test]
    fn presence_discovery_json() {
        let v: serde_json::Value = serde_json::from_str(&presence_discovery("ambersight")).unwrap();
        assert_eq!(presence_discovery_topic("ambersight"), "homeassistant/binary_sensor/ambersight_someone_home/config");
        assert_eq!(v["name"], "Someone home");
        assert_eq!(v["unique_id"], "ambersight_someone_home");
        assert_eq!(v["device_class"], "presence");
        assert_eq!(v["state_topic"], "ambersight/presence/state");
        assert_eq!((v["payload_on"].as_str(), v["payload_off"].as_str()), (Some("home"), Some("away")));
        assert_eq!(v["availability_mode"], "all");
        assert_eq!(v["availability"][0]["topic"], "ambersight/status");
        assert_eq!(v["availability"][1]["topic"], "ambersight/presence/state");
        assert!(v["availability"][1]["value_template"].as_str().unwrap().contains("unknown"));
        assert_eq!(v["device"]["identifiers"][0], "ambersight_server");
        // Announced while on; removed (empty, retained) when off, also when
        // nothing was announced yet this run; nothing again once done.
        assert_eq!(presence_discovery_update("ambersight", None, false).as_deref(), Some(""));
        assert_eq!(presence_discovery_update("ambersight", Some(true), false).as_deref(), Some(""));
        assert_eq!(presence_discovery_update("ambersight", Some(false), false), None);
        assert!(presence_discovery_update("ambersight", None, true).unwrap().contains("someone_home"));
        assert!(presence_discovery_update("ambersight", Some(false), true).is_some());
        assert_eq!(presence_discovery_update("ambersight", Some(true), true), None);
        // Not part of the regular set (only announced while presence is on).
        assert!(discovery_messages("ambersight", &[], &[], true).iter().all(|(t, _)| !t.contains("someone_home")));
    }

    #[test]
    fn discovery_on_demand_camera() {
        let msgs = discovery_messages("p", &[cam("yard", "Yard", true)], &[], false);
        let v = find(&msgs, "homeassistant/binary_sensor/p_yard_asleep/config");
        assert_eq!(v["state_topic"], "p/yard/asleep");
        assert_eq!(v["entity_category"], "diagnostic");
        assert_eq!(v["unique_id"], "p_yard_asleep");
        let v = find(&msgs, "homeassistant/button/p_yard_wake/config");
        assert_eq!(v["command_topic"], "p/yard/wake");
        assert_eq!(v["payload_press"], "PRESS");
        assert_eq!(v["availability_topic"], "p/status");
        assert_eq!(msgs.len(), 10);
    }
}
