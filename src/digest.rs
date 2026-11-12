//! The morning summary: what happened at the cameras since the last one,
//! worked out from the local database (events and their labels, tamper
//! alerts, camera offline stretches). This file only builds and words the
//! summary; engine/digest.rs runs it on the clock and sends it.
//!
//! Counts are events: "2 unknown people" is two events with a face nobody
//! has named.

use crate::db::DigestEvent;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// An offline stretch shorter than this is not worth a line.
pub const MIN_OUTAGE_MS: i64 = 2 * 60 * 1000;
/// Highlights kept per camera (the counts still cover everything).
const MAX_HIGHLIGHTS: usize = 8;
/// Of each kind of person / vehicle highlight.
const MAX_PER_KIND: usize = 3;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Named {
    pub name: String,
    pub count: u32,
}

/// One thing worth a look, linked to its event (or the camera).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Highlight {
    /// "person", "vehicle", "package", "zone", "unusual", "tamper", "offline".
    pub kind: String,
    pub text: String,
    pub at_ms: i64,
    pub event_id: Option<String>,
    pub thumb: Option<String>,
    /// A path on this server that opens it.
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CamSummary {
    pub camera: String,
    pub events: u32,
    pub people_known: Vec<Named>,
    pub people_unknown: u32,
    /// People AI saw but no face was found for.
    pub people_unidentified: u32,
    pub vehicles_known: Vec<Named>,
    /// Unknown plates read (each once).
    pub plates_unknown: Vec<String>,
    /// Vehicles with no plate read.
    pub vehicles_other: u32,
    pub packages_delivered: u32,
    pub packages_picked_up: u32,
    pub loitering: u32,
    pub crossings: u32,
    pub unusual: u32,
    pub tamper: u32,
    pub offline_count: u32,
    pub offline_minutes: u32,
    /// The counts in words ("People: Anna, 1 unknown; Packages: 1 delivered").
    pub line: String,
    pub highlights: Vec<Highlight>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Digest {
    pub from_ms: i64,
    pub to_ms: i64,
    /// "Overnight", "Last 24 hours", "Since the last summary".
    pub period: String,
    /// The whole thing in one line ("2 unknown people, 1 package delivered").
    pub overview: String,
    pub cameras: Vec<CamSummary>,
    /// Things to look at (a camera that sent too many notifications), in plain words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Digest {
    /// Nothing happened at any camera.
    pub fn is_quiet(&self) -> bool {
        self.cameras.iter().all(|c| c.events == 0 && c.tamper == 0 && c.offline_count == 0)
    }
}

/// What the summary is built from.
pub struct Input<'a> {
    pub from_ms: i64,
    pub to_ms: i64,
    pub period: &'a str,
    /// The cameras covered: (name, id), in the order shown.
    pub cameras: &'a [(String, i64)],
    pub events: &'a [DigestEvent],
    /// (camera, at seconds, kind, cleared_at seconds)
    pub tamper: &'a [(String, i64, String, Option<i64>)],
    /// (camera, start ms, end ms)
    pub outages: &'a [(String, i64, i64)],
    /// Names given to people in Faces (they appear as labels).
    pub people: &'a HashSet<String>,
}

use crate::tracker::is_vehicle;

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// A place in the web app that opens an event.
pub fn event_url(camera_id: i64, camera: &str, start_s: i64) -> String {
    format!("/?alert={camera_id}&cam={}&at={}", enc(camera), start_s * 1000)
}

/// A place that opens a camera (for tamper and offline lines).
pub fn camera_url(camera: &str) -> String {
    format!("/?alert=-1&cam={}", enc(camera))
}

/// Local clock time of a moment, "3:12 AM".
pub fn clock(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).format("%-I:%M %p").to_string()).unwrap_or_default()
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn bump(list: &mut Vec<Named>, name: &str) {
    match list.iter_mut().find(|n| n.name == name) {
        Some(n) => n.count += 1,
        None => list.push(Named { name: name.to_string(), count: 1 }),
    }
}

fn named_list(list: &[Named]) -> Vec<String> {
    list.iter().map(|n| if n.count > 1 { format!("{} x{}", n.name, n.count) } else { n.name.clone() }).collect()
}

/// A known plate's label ("plate ABC123 (Mom's car)") as the name to show.
fn known_plate_name(rest: &str) -> String {
    match (rest.find(" ("), rest.ends_with(')')) {
        (Some(i), true) => rest[i + 2..rest.len() - 1].to_string(),
        _ => rest.to_string(),
    }
}

/// The counts of a camera in words.
fn describe(c: &CamSummary) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !c.people_known.is_empty() || c.people_unknown > 0 || c.people_unidentified > 0 {
        let mut p = named_list(&c.people_known);
        if c.people_unknown > 0 {
            p.push(format!("{} unknown", c.people_unknown));
        }
        if c.people_unidentified > 0 {
            p.push(format!("{} with no face seen", c.people_unidentified));
        }
        parts.push(format!("People: {}", p.join(", ")));
    }
    if !c.vehicles_known.is_empty() || !c.plates_unknown.is_empty() || c.vehicles_other > 0 {
        let mut v = named_list(&c.vehicles_known);
        if !c.plates_unknown.is_empty() {
            v.push(format!("{} unknown ({})", c.plates_unknown.len(), c.plates_unknown.join(", ")));
        }
        if c.vehicles_other > 0 {
            v.push(format!("{} with no plate read", c.vehicles_other));
        }
        parts.push(format!("Vehicles: {}", v.join(", ")));
    }
    let mut pk = Vec::new();
    if c.packages_delivered > 0 {
        pk.push(format!("{} delivered", c.packages_delivered));
    }
    if c.packages_picked_up > 0 {
        pk.push(format!("{} picked up", c.packages_picked_up));
    }
    if !pk.is_empty() {
        parts.push(format!("Packages: {}", pk.join(", ")));
    }
    if c.loitering > 0 {
        parts.push(format!("Loitering: {}", c.loitering));
    }
    if c.crossings > 0 {
        parts.push(format!("Line crossings: {}", c.crossings));
    }
    if c.unusual > 0 {
        parts.push(format!("Unusual: {}", c.unusual));
    }
    if c.tamper > 0 {
        parts.push(format!("Tamper alerts: {}", c.tamper));
    }
    if c.offline_count > 0 {
        parts.push(format!("Offline: {} min ({})", c.offline_minutes, plural(c.offline_count, "time", "times")));
    }
    if parts.is_empty() {
        return if c.events > 0 { format!("{} (nothing identified)", plural(c.events, "event", "events")) } else { "Nothing happened.".to_string() };
    }
    parts.join("; ")
}

/// The whole summary in one line.
pub fn overview_of(cams: &[CamSummary]) -> String {
    let sum = |f: &dyn Fn(&CamSummary) -> u32| cams.iter().map(f).sum::<u32>();
    let unknown_people = sum(&|c| c.people_unknown + c.people_unidentified);
    let known_people = cams.iter().map(|c| c.people_known.iter().map(|n| n.count).sum::<u32>()).sum::<u32>();
    let plates: u32 = cams.iter().map(|c| c.plates_unknown.len() as u32).sum();
    let other_vehicles = sum(&|c| c.vehicles_other);
    let known_vehicles = cams.iter().map(|c| c.vehicles_known.iter().map(|n| n.count).sum::<u32>()).sum::<u32>();
    let mut parts: Vec<String> = Vec::new();
    if unknown_people > 0 {
        parts.push(plural(unknown_people, "unknown person", "unknown people"));
    }
    if known_people > 0 {
        parts.push(format!("{known_people} known"));
    }
    if plates + other_vehicles > 0 {
        parts.push(plural(plates + other_vehicles, "unknown vehicle", "unknown vehicles"));
    }
    if known_vehicles > 0 {
        parts.push(plural(known_vehicles, "known vehicle", "known vehicles"));
    }
    let delivered = sum(&|c| c.packages_delivered);
    if delivered > 0 {
        parts.push(plural(delivered, "package delivered", "packages delivered"));
    }
    let picked = sum(&|c| c.packages_picked_up);
    if picked > 0 {
        parts.push(plural(picked, "package picked up", "packages picked up"));
    }
    let zone = sum(&|c| c.loitering + c.crossings);
    if zone > 0 {
        parts.push(plural(zone, "loitering or line-crossing alert", "loitering or line-crossing alerts"));
    }
    let unusual = sum(&|c| c.unusual);
    if unusual > 0 {
        parts.push(format!("{unusual} unusual"));
    }
    let tamper = sum(&|c| c.tamper);
    if tamper > 0 {
        parts.push(plural(tamper, "tamper alert", "tamper alerts"));
    }
    let offline = sum(&|c| c.offline_count);
    if offline > 0 {
        parts.push(plural(offline, "camera outage", "camera outages"));
    }
    if parts.is_empty() {
        let events = sum(&|c| c.events);
        return if events > 0 { format!("{} (nothing identified)", plural(events, "event", "events")) } else { "Nothing happened.".to_string() };
    }
    parts.join(", ")
}

/// Build the summary.
pub fn build(inp: &Input) -> Digest {
    let mut cams: Vec<CamSummary> = inp.cameras.iter().map(|(n, _)| CamSummary { camera: n.clone(), ..Default::default() }).collect();
    let index: BTreeMap<&str, usize> = inp.cameras.iter().enumerate().map(|(i, (n, _))| (n.as_str(), i)).collect();
    // Highlights per kind, to keep a busy night short.
    let mut per_kind: Vec<BTreeMap<&'static str, usize>> = vec![BTreeMap::new(); cams.len()];
    let mut add = |cams: &mut Vec<CamSummary>, i: usize, kind: &'static str, limit: usize, h: Highlight| {
        let n = per_kind[i].entry(kind).or_insert(0);
        if *n < limit {
            *n += 1;
            cams[i].highlights.push(h);
        }
    };
    for ev in inp.events {
        let Some(&i) = index.get(ev.camera.as_str()) else { continue };
        let url = event_url(ev.camera_id, &ev.camera, ev.start);
        let at_ms = ev.start * 1000;
        let hl = |kind: &str, text: String| Highlight { kind: kind.to_string(), text, at_ms, event_id: Some(ev.id.clone()), thumb: ev.thumb.clone(), url: url.clone() };
        cams[i].events += 1;
        let first = ev.objects.first().map(String::as_str).unwrap_or("");
        let at = clock(at_ms);
        if first == "package" {
            cams[i].packages_delivered += 1;
            add(&mut cams, i, "package", MAX_PER_KIND, hl("package", format!("Package delivered at {at}")));
            continue;
        }
        if first == "package picked up" {
            cams[i].packages_picked_up += 1;
            add(&mut cams, i, "package", MAX_PER_KIND, hl("package", format!("Package picked up at {at}")));
            continue;
        }
        if first.starts_with("loitering: ") || first.starts_with("crossed: ") {
            if first.starts_with("loitering: ") {
                cams[i].loitering += 1;
            } else {
                cams[i].crossings += 1;
            }
            add(&mut cams, i, "zone", MAX_PER_KIND, hl("zone", format!("{} at {at}", capitalize(first))));
            continue;
        }
        let person = ev.objects.iter().any(|o| o == "person");
        let names: Vec<&String> = ev.objects.iter().filter(|o| inp.people.contains(*o)).collect();
        let unknown_face = ev.objects.iter().any(|o| o == "unknown face");
        if !names.is_empty() {
            for n in &names {
                bump(&mut cams[i].people_known, n);
            }
        }
        if unknown_face {
            cams[i].people_unknown += 1;
            add(&mut cams, i, "person", MAX_PER_KIND, hl("person", format!("Unknown person at {at}")));
        } else if person && names.is_empty() {
            cams[i].people_unidentified += 1;
            add(&mut cams, i, "person", MAX_PER_KIND, hl("person", format!("Person, face not seen, at {at}")));
        }
        let mut plate_seen = false;
        for o in &ev.objects {
            if let Some(t) = o.strip_prefix("unknown plate ") {
                plate_seen = true;
                if !cams[i].plates_unknown.iter().any(|p| p == t) {
                    cams[i].plates_unknown.push(t.to_string());
                    add(&mut cams, i, "vehicle", MAX_PER_KIND, hl("vehicle", format!("Unknown plate {t} at {at}")));
                }
            } else if let Some(rest) = o.strip_prefix("plate ") {
                plate_seen = true;
                bump(&mut cams[i].vehicles_known, &known_plate_name(rest));
            }
        }
        if !plate_seen && ev.objects.iter().any(|o| is_vehicle(o)) {
            cams[i].vehicles_other += 1;
        }
        if ev.objects.iter().any(|o| o == "unusual") {
            cams[i].unusual += 1;
            let what = ev.objects.iter().find(|o| o.as_str() != "unusual").map(|o| o.to_lowercase()).unwrap_or_else(|| "motion".into());
            add(&mut cams, i, "unusual", MAX_PER_KIND, hl("unusual", format!("Unusual: {what} at {at}")));
        }
    }
    for (camera, at, kind, cleared) in inp.tamper {
        let Some(&i) = index.get(camera.as_str()) else { continue };
        cams[i].tamper += 1;
        let what = match kind.as_str() {
            "covered" => "covered or blacked out",
            "blurred" => "blurred",
            "moved" => "moved",
            other => other,
        };
        let at_ms = at * 1000;
        let text = format!("Camera {what} at {}{}", clock(at_ms), if cleared.is_some() { " (view restored)" } else { " (not restored)" });
        add(&mut cams, i, "tamper", MAX_PER_KIND, Highlight { kind: "tamper".into(), text, at_ms, event_id: None, thumb: None, url: camera_url(camera) });
    }
    for (camera, start, end) in inp.outages {
        let Some(&i) = index.get(camera.as_str()) else { continue };
        // Only the part inside the period counts.
        let (s, e) = ((*start).max(inp.from_ms), (*end).min(inp.to_ms));
        if e - s < MIN_OUTAGE_MS {
            continue;
        }
        let mins = ((e - s) / 60_000) as u32;
        cams[i].offline_count += 1;
        cams[i].offline_minutes += mins;
        let text = format!("Offline from {} to {} ({mins} min)", clock(s), clock(e));
        add(&mut cams, i, "offline", MAX_PER_KIND, Highlight { kind: "offline".into(), text, at_ms: s, event_id: None, thumb: None, url: camera_url(camera) });
    }
    for c in cams.iter_mut() {
        c.highlights.sort_by_key(|h| h.at_ms);
        c.highlights.truncate(MAX_HIGHLIGHTS);
        c.line = describe(c);
    }
    let overview = overview_of(&cams);
    Digest { from_ms: inp.from_ms, to_ms: inp.to_ms, period: inp.period.to_string(), overview, cameras: cams, notes: Vec::new() }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The period's name for titles ("Overnight").
pub fn period_name(period: &str) -> &'static str {
    match period {
        "24h" => "Last 24 hours",
        "since_last" => "Since the last summary",
        _ => "Overnight",
    }
}

/// The period as a range of clock times ("10:00 PM to 7:00 AM").
pub fn range_text(from_ms: i64, to_ms: i64) -> String {
    let day = |ms: i64| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).format("%a %b %-d").to_string()).unwrap_or_default();
    let (a, b) = (day(from_ms), day(to_ms));
    if a == b {
        format!("{a}, {} to {}", clock(from_ms), clock(to_ms))
    } else {
        format!("{a} {} to {b} {}", clock(from_ms), clock(to_ms))
    }
}

pub fn title(d: &Digest) -> String {
    if d.is_quiet() {
        "Morning summary: all quiet".to_string()
    } else {
        format!("Morning summary: {}", d.overview)
    }
}

/// The short version for push messages: one line per camera that had
/// something, within `max` characters.
pub fn short_body(d: &Digest, max: usize) -> String {
    let mut s = format!("{} ({})", period_name(&d.period), range_text(d.from_ms, d.to_ms));
    for c in d.cameras.iter().filter(|c| c.events > 0 || c.tamper > 0 || c.offline_count > 0) {
        let line = format!("\n{}: {}", c.camera, c.line);
        if s.chars().count() + line.chars().count() > max.saturating_sub(24) {
            s.push_str("\n...and more in the summary.");
            break;
        }
        s.push_str(&line);
    }
    if d.is_quiet() {
        s.push_str("\nNothing happened.");
    }
    for n in &d.notes {
        s.push_str(&format!("\n{n}"));
    }
    s
}

/// The full text version (ntfy, Telegram, the plain half of the email).
/// `base` = this server's address for links; none = no links.
pub fn full_text(d: &Digest, base: Option<&str>) -> String {
    let mut s = format!("{} ({})\n{}\n", period_name(&d.period), range_text(d.from_ms, d.to_ms), d.overview);
    for c in d.cameras.iter().filter(|c| c.events > 0 || c.tamper > 0 || c.offline_count > 0) {
        s.push_str(&format!("\n{}\n  {}\n", c.camera, c.line));
        for h in &c.highlights {
            match base {
                Some(b) => s.push_str(&format!("  - {}: {}{}\n", h.text, b.trim_end_matches('/'), h.url)),
                None => s.push_str(&format!("  - {}\n", h.text)),
            }
        }
    }
    for n in &d.notes {
        match base {
            Some(b) => s.push_str(&format!("\n{n} {}/#settings/system/status\n", b.trim_end_matches('/'))),
            None => s.push_str(&format!("\n{n}\n")),
        }
    }
    s
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The HTML email. `images`: the highlight thumbnails attached, by event id
/// (shown with `cid:<event id>`... as `cid_for`).
pub fn cid_for(i: usize) -> String {
    format!("thumb{i}@ambersight")
}

pub fn html(d: &Digest, base: Option<&str>, images: &[(String, String)]) -> String {
    let link = |url: &str| base.map(|b| format!("{}{}", b.trim_end_matches('/'), url));
    let mut s = String::from("<!doctype html><html><body style=\"margin:0;background:#f3f4f6;font-family:-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif;color:#1f2937\">");
    s.push_str("<div style=\"max-width:620px;margin:0 auto;padding:16px\">");
    s.push_str(&format!(
        "<div style=\"background:#111827;color:#fff;border-radius:12px 12px 0 0;padding:18px 20px\"><div style=\"font-size:13px;opacity:.7\">AmberSight &middot; {}</div><div style=\"font-size:20px;font-weight:600;margin-top:4px\">Morning summary</div><div style=\"font-size:14px;margin-top:6px;opacity:.9\">{}</div></div>",
        esc(&range_text(d.from_ms, d.to_ms)),
        esc(&d.overview)
    ));
    s.push_str("<div style=\"background:#fff;border-radius:0 0 12px 12px;padding:6px 20px 16px\">");
    let busy: Vec<&CamSummary> = d.cameras.iter().filter(|c| c.events > 0 || c.tamper > 0 || c.offline_count > 0).collect();
    if busy.is_empty() {
        s.push_str("<p style=\"font-size:15px\">Nothing happened.</p>");
    }
    for c in busy {
        s.push_str(&format!("<h3 style=\"margin:18px 0 4px;font-size:16px\">{}</h3><div style=\"font-size:14px;color:#374151\">{}</div>", esc(&c.camera), esc(&c.line)));
        s.push_str("<table role=\"presentation\" cellspacing=\"0\" cellpadding=\"0\" style=\"margin-top:8px;width:100%\">");
        for h in &c.highlights {
            let img = h.event_id.as_ref().and_then(|id| images.iter().find(|(e, _)| e == id)).map(|(_, cid)| format!("<img src=\"cid:{}\" width=\"96\" alt=\"\" style=\"border-radius:6px;display:block\">", esc(cid)));
            let text = match link(&h.url) {
                Some(u) => format!("<a href=\"{}\" style=\"color:#b45309;text-decoration:none\">{}</a>", esc(&u), esc(&h.text)),
                None => esc(&h.text),
            };
            s.push_str(&format!(
                "<tr><td style=\"width:104px;padding:4px 8px 4px 0;vertical-align:top\">{}</td><td style=\"padding:4px 0;font-size:14px;vertical-align:middle\">{}</td></tr>",
                img.unwrap_or_default(),
                text
            ));
        }
        s.push_str("</table>");
    }
    for n in &d.notes {
        let text = match link("/#settings/system/status") {
            Some(u) => format!("{} <a href=\"{}\" style=\"color:#b45309\">Open Status</a>", esc(n), esc(&u)),
            None => esc(n),
        };
        s.push_str(&format!("<p style=\"margin-top:16px;font-size:14px;color:#374151\">{text}</p>"));
    }
    if let Some(u) = base {
        s.push_str(&format!("<p style=\"margin-top:20px;font-size:13px\"><a href=\"{}\" style=\"color:#b45309\">Open AmberSight</a></p>", esc(u.trim_end_matches('/'))));
    }
    s.push_str("</div></div></body></html>");
    s
}

// ---------------------------------------------------------------- the clock

/// The period a summary covers, ending `now`. `last_to` is the end of the
/// last summary (ms), for "since_last".
pub fn period_for<Tz: chrono::TimeZone>(cfg: &crate::config::DigestConfig, now: &chrono::DateTime<Tz>, last_to_ms: Option<i64>) -> (i64, i64) {
    use chrono::{Duration, Timelike};
    let to = now.timestamp_millis();
    let day = 24 * 3600 * 1000;
    let from = match cfg.period() {
        "24h" => to - day,
        "since_last" => last_to_ms.map_or(to - day, |t| t.clamp(to - 2 * day, to)),
        _ => {
            // The most recent time the night started.
            let m = cfg.night_start_min();
            let mut date = now.date_naive();
            if now.hour() * 60 + now.minute() < m {
                date -= Duration::days(1);
            }
            let naive = date.and_hms_opt(m / 60, m % 60, 0).unwrap_or_default();
            match now.timezone().from_local_datetime(&naive).earliest() {
                Some(t) => t.timestamp_millis(),
                None => to - 9 * 3600 * 1000,
            }
        }
    };
    (from.min(to), to)
}

/// Whether the scheduled summary is due: it is past the sending time (by at
/// most 3 hours, so a restart at noon does not send a morning summary) and
/// none was sent today.
pub fn due<Tz: chrono::TimeZone>(cfg: &crate::config::DigestConfig, now: &chrono::DateTime<Tz>, last_auto_ms: Option<i64>) -> bool {
    use chrono::Timelike;
    if !cfg.enabled {
        return false;
    }
    let minute = now.hour() * 60 + now.minute();
    let at = cfg.time_min();
    // Minutes since the sending time, past midnight too (22:30 + 3 h = 01:30).
    if (minute + 1440 - at) % 1440 >= 180 {
        return false;
    }
    // The day the sending time was on: yesterday when it is after midnight now.
    let today = now.date_naive();
    let window_day = if minute >= at { today } else { today.pred_opt().unwrap_or(today) };
    let sent = last_auto_ms
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .is_some_and(|t| t.with_timezone(&now.timezone()).date_naive() >= window_day);
    !sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DigestConfig;
    use chrono::TimeZone;

    fn ev(id: &str, cam: &str, cid: i64, start: i64, objects: &[&str]) -> DigestEvent {
        DigestEvent { id: id.into(), camera_id: cid, camera: cam.into(), start, thumb: Some(id.into()), objects: objects.iter().map(|s| s.to_string()).collect() }
    }

    fn people() -> HashSet<String> {
        ["Anna".to_string(), "Bob".to_string()].into_iter().collect()
    }

    fn input<'a>(events: &'a [DigestEvent], tamper: &'a [(String, i64, String, Option<i64>)], outages: &'a [(String, i64, i64)], people: &'a HashSet<String>, cams: &'a [(String, i64)]) -> Input<'a> {
        Input { from_ms: 0, to_ms: 10_000_000, period: "overnight", cameras: cams, events, tamper, outages, people }
    }

    #[test]
    fn counts_people_vehicles_packages_and_the_rest() {
        let cams = vec![("Front Door".to_string(), 1), ("Driveway".to_string(), 2)];
        let events = vec![
            ev("a", "Front Door", 1, 100, &["person", "Anna"]),
            ev("b", "Front Door", 1, 200, &["person", "Anna"]),
            ev("c", "Front Door", 1, 300, &["person", "unknown face"]),
            ev("d", "Front Door", 1, 400, &["person"]),
            ev("e", "Front Door", 1, 500, &["package"]),
            ev("f", "Front Door", 1, 600, &["package picked up"]),
            ev("g", "Front Door", 1, 700, &["loitering: Porch"]),
            ev("h", "Front Door", 1, 800, &["crossed: Gate line"]),
            ev("i", "Driveway", 2, 900, &["car", "plate ABC123 (Mom's car)"]),
            ev("j", "Driveway", 2, 1000, &["car", "unknown plate ZZZ999"]),
            ev("k", "Driveway", 2, 1100, &["car", "unknown plate ZZZ999"]),
            ev("l", "Driveway", 2, 1200, &["truck"]),
            ev("m", "Driveway", 2, 1300, &["person", "unusual"]),
            ev("n", "Elsewhere", 9, 1400, &["person"]),
        ];
        let tamper = vec![("Driveway".to_string(), 1500, "covered".to_string(), None)];
        let outages = vec![("Driveway".to_string(), 2_000_000, 2_600_000), ("Driveway".to_string(), 3_000_000, 3_060_000)];
        let p = people();
        let d = build(&input(&events, &tamper, &outages, &p, &cams));
        let f = &d.cameras[0];
        assert_eq!(f.events, 8);
        assert_eq!(f.people_known, vec![Named { name: "Anna".into(), count: 2 }]);
        assert_eq!((f.people_unknown, f.people_unidentified), (1, 1));
        assert_eq!((f.packages_delivered, f.packages_picked_up, f.loitering, f.crossings), (1, 1, 1, 1));
        assert_eq!(f.line, "People: Anna x2, 1 unknown, 1 with no face seen; Packages: 1 delivered, 1 picked up; Loitering: 1; Line crossings: 1");
        let r = &d.cameras[1];
        assert_eq!(r.vehicles_known, vec![Named { name: "Mom's car".into(), count: 1 }]);
        assert_eq!(r.plates_unknown, vec!["ZZZ999".to_string()]);
        assert_eq!(r.vehicles_other, 1);
        assert_eq!(r.unusual, 1);
        assert_eq!(r.people_unidentified, 1);
        assert_eq!(r.tamper, 1);
        // The one-minute outage is too short to count.
        assert_eq!((r.offline_count, r.offline_minutes), (1, 10));
        assert!(r.line.contains("Vehicles: Mom's car, 1 unknown (ZZZ999), 1 with no plate read"), "{}", r.line);
        assert!(r.line.contains("Offline: 10 min (1 time)"), "{}", r.line);
        assert!(!d.is_quiet());
        assert!(d.overview.contains("3 unknown people") && d.overview.contains("1 package delivered") && d.overview.contains("1 tamper alert"), "{}", d.overview);
        // An event of a camera that is not covered is ignored.
        assert_eq!(d.cameras.len(), 2);
    }

    #[test]
    fn highlights_link_to_events_and_are_capped() {
        let cams = vec![("Door".to_string(), 4)];
        let events: Vec<DigestEvent> = (0..6).map(|i| ev(&format!("e{i}"), "Door", 4, 100 + i, &["person", "unknown face"])).collect();
        let p = people();
        let d = build(&input(&events, &[], &[], &p, &cams));
        let c = &d.cameras[0];
        assert_eq!(c.people_unknown, 6);
        assert_eq!(c.highlights.len(), MAX_PER_KIND);
        assert_eq!(c.highlights[0].event_id.as_deref(), Some("e0"));
        assert_eq!(c.highlights[0].url, "/?alert=4&cam=Door&at=100000");
        assert!(c.highlights[0].text.starts_with("Unknown person at "));
    }

    #[test]
    fn quiet_summary_and_wording() {
        let cams = vec![("Door".to_string(), 4)];
        let p = people();
        let d = build(&input(&[], &[], &[], &p, &cams));
        assert!(d.is_quiet());
        assert_eq!(title(&d), "Morning summary: all quiet");
        assert!(short_body(&d, 500).contains("Nothing happened."));
        assert_eq!(d.cameras[0].line, "Nothing happened.");
    }

    #[test]
    fn push_text_stays_short_even_for_many_cameras() {
        let cams: Vec<(String, i64)> = (0..40).map(|i| (format!("Camera number {i}"), i)).collect();
        let events: Vec<DigestEvent> = (0..40).map(|i| ev(&format!("e{i}"), &format!("Camera number {i}"), i, 100, &["person", "unknown face"])).collect();
        let p = people();
        let d = build(&input(&events, &[], &[], &p, &cams));
        let b = short_body(&d, 700);
        assert!(b.chars().count() <= 700, "{}", b.chars().count());
        assert!(b.contains("and more"));
        let m = crate::push::Message { title: title(&d), body: b, url: "/#digest?id=1".into(), ..Default::default() };
        assert!(crate::push::payload(&m).len() <= crate::push::MAX_PAYLOAD);
    }

    #[test]
    fn full_text_has_links_only_with_an_address() {
        let cams = vec![("Door".to_string(), 4)];
        let events = vec![ev("e0", "Door", 4, 100, &["person", "unknown face"])];
        let p = people();
        let d = build(&input(&events, &[], &[], &p, &cams));
        assert!(!full_text(&d, None).contains("http"));
        assert!(full_text(&d, Some("https://nvr.example/")).contains("https://nvr.example/?alert=4&cam=Door&at=100000"));
    }

    #[test]
    fn html_has_images_links_and_escapes() {
        let cams = vec![("Door <1>".to_string(), 4)];
        let events = vec![ev("e0", "Door <1>", 4, 100, &["person", "unknown face"])];
        let p = people();
        let d = build(&input(&events, &[], &[], &p, &cams));
        let h = html(&d, Some("https://nvr.example"), &[("e0".to_string(), cid_for(0))]);
        assert!(h.contains("cid:thumb0@ambersight"));
        assert!(h.contains("https://nvr.example/?alert=4&amp;cam=Door%20%3C1%3E&amp;at=100000") || h.contains("cam=Door+%3C1%3E"));
        assert!(h.contains("Door &lt;1&gt;"));
        assert!(!h.contains("Door <1>"));
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<chrono::FixedOffset> {
        chrono::FixedOffset::east_opt(-6 * 3600).unwrap().with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn periods() {
        let mut c = DigestConfig::default();
        let now = at(2026, 11, 7, 7, 0);
        // Overnight: from 22:00 the evening before.
        assert_eq!(period_for(&c, &now, None), (at(2026, 11, 6, 22, 0).timestamp_millis(), now.timestamp_millis()));
        // Sent in the evening: from 22:00 of the same day if it is past it.
        assert_eq!(period_for(&c, &at(2026, 11, 7, 23, 0), None).0, at(2026, 11, 7, 22, 0).timestamp_millis());
        c.period = "24h".into();
        assert_eq!(period_for(&c, &now, None).0, now.timestamp_millis() - 86_400_000);
        c.period = "since_last".into();
        let last = at(2026, 11, 6, 7, 5).timestamp_millis();
        assert_eq!(period_for(&c, &now, Some(last)).0, last);
        // Never further back than 48 hours.
        assert_eq!(period_for(&c, &now, Some(0)).0, now.timestamp_millis() - 2 * 86_400_000);
    }

    #[test]
    fn due_once_a_day_shortly_after_the_time() {
        let mut c = DigestConfig::default();
        assert!(!due(&c, &at(2026, 11, 7, 7, 1), None), "off by default");
        c.enabled = true;
        assert!(!due(&c, &at(2026, 11, 7, 6, 59), None));
        assert!(due(&c, &at(2026, 11, 7, 7, 0), None));
        assert!(due(&c, &at(2026, 11, 7, 9, 30), None));
        assert!(!due(&c, &at(2026, 11, 7, 10, 0), None), "too late: wait for tomorrow");
        // Already sent today (at 7:00 local = 13:00 UTC).
        let sent = at(2026, 11, 7, 7, 0).timestamp_millis();
        assert!(!due(&c, &at(2026, 11, 7, 7, 30), Some(sent)));
        assert!(due(&c, &at(2026, 11, 8, 7, 0), Some(sent)));
        c.time = "22:30".into();
        assert!(!due(&c, &at(2026, 11, 7, 22, 45), Some(sent)), "one a day");
        assert!(due(&c, &at(2026, 11, 8, 22, 45), Some(sent)));

    }

    #[test]
    fn the_catch_up_window_wraps_past_midnight() {
        let mut c = DigestConfig::default();
        c.enabled = true;
        c.time = "22:30".into();
        // Restarted at 00:15: still within 3 hours of 22:30 yesterday.
        assert!(due(&c, &at(2026, 11, 8, 0, 15), None));
        assert!(due(&c, &at(2026, 11, 8, 1, 29), None));
        assert!(!due(&c, &at(2026, 11, 8, 1, 30), None));
        assert!(!due(&c, &at(2026, 11, 8, 22, 29), None));
        // Sent at 22:30 yesterday: not sent again after midnight.
        let sent = at(2026, 11, 7, 22, 30).timestamp_millis();
        assert!(!due(&c, &at(2026, 11, 8, 0, 15), Some(sent)));
        // Sent the evening before that: due.
        assert!(due(&c, &at(2026, 11, 8, 0, 15), Some(sent - 86_400_000)));
    }
}
