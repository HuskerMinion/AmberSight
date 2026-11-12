//! Alerts about the system itself: a camera offline, recordings not being
//! written, a drive nearly full or unreadable. Checked once a minute from the
//! health report; a problem is reported once it has lasted
//! [alerts] system_alert_minutes, and again (as resolved) once it has been
//! gone for two minutes. Problems found in the same check go out together.
//! Sent to every notification service that is set up, plus MQTT
//! (`<prefix>/problems`: ON/OFF and a JSON list, with a Home Assistant
//! "Problem" sensor).

use super::*;
use std::collections::HashMap;
use std::time::Duration;

/// A problem must be gone this long before it counts as resolved (a camera
/// that drops and comes back every few minutes is not reported each time).
const CLEAR_SECS: i64 = 120;

#[derive(Debug, Clone)]
struct Tracked {
    text: String,
    first: i64,
    last: i64,
    reported: bool,
}

/// What one check found to say.
#[derive(Debug, Default, PartialEq)]
pub struct Change {
    pub new: Vec<String>,
    pub resolved: Vec<String>,
}

/// The problems seen so far, keyed as in the health report.
#[derive(Default)]
pub struct Tracker {
    seen: HashMap<String, Tracked>,
}

impl Tracker {
    /// Feed one check's problems (key, text); returns what to report.
    pub fn update(&mut self, now: i64, current: &[(String, String)], after_secs: i64) -> Change {
        let mut ch = Change::default();
        for (k, text) in current {
            let t = self.seen.entry(k.clone()).or_insert_with(|| Tracked { text: text.clone(), first: now, last: now, reported: false });
            t.last = now;
            t.text = text.clone();
        }
        let mut keys: Vec<&String> = self.seen.keys().collect();
        keys.sort();
        let keys: Vec<String> = keys.into_iter().cloned().collect();
        for k in keys {
            let t = self.seen.get_mut(&k).unwrap();
            if t.last == now {
                if !t.reported && now - t.first >= after_secs {
                    t.reported = true;
                    ch.new.push(t.text.clone());
                }
            } else if now - t.last >= CLEAR_SECS {
                if t.reported {
                    ch.resolved.push(t.text.clone());
                }
                self.seen.remove(&k);
            }
        }
        ch
    }

    /// Reported problems that have not cleared yet.
    pub fn active(&self) -> Vec<String> {
        let mut v: Vec<(&String, &Tracked)> = self.seen.iter().filter(|(_, t)| t.reported).collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v.into_iter().map(|(_, t)| t.text.clone()).collect()
    }
}

/// Title and body for a change on server `site`.
pub fn message(site: &str, ch: &Change) -> (String, String) {
    let title = match (ch.new.len(), ch.resolved.len()) {
        (1, 0) => format!("{site}: {}", ch.new[0]),
        (n, 0) => format!("{site}: {n} problems"),
        (0, 1) => format!("{site}: resolved: {}", ch.resolved[0]),
        (0, _) => format!("{site}: problems resolved"),
        (n, _) => format!("{site}: {n} new problem{}", if n == 1 { "" } else { "s" }),
    };
    let mut body = String::new();
    for p in &ch.new {
        body.push_str(&format!("Problem: {p}\n"));
    }
    for p in &ch.resolved {
        body.push_str(&format!("Resolved: {p}\n"));
    }
    (title, body.trim_end().to_string())
}

/// The health report's problems as (key, text).
fn current(health: &serde_json::Value) -> Vec<(String, String)> {
    health["problems"]
        .as_array()
        .map(|a| a.iter().filter_map(|p| Some((p["key"].as_str()?.to_string(), p["text"].as_str()?.to_string()))).collect())
        .unwrap_or_default()
}

pub fn system_alert_loop(engine: Arc<Engine>) {
    let mut tracker = Tracker::default();
    // Let cameras connect after a start before judging anything.
    thread::sleep(Duration::from_secs(60));
    loop {
        let alerts = engine.config.lock().unwrap().alerts.clone();
        let now = Utc::now().timestamp();
        let ch = tracker.update(now, &current(&engine.health()), alerts.system_alert_secs());
        if let Some(m) = engine.mqtt.get() {
            m.publish_problems(&tracker.active());
        }
        if alerts.system_alerts_on() && (!ch.new.is_empty() || !ch.resolved.is_empty()) {
            let site = crate::links::own_name(&engine);
            let (title, body) = message(&site, &ch);
            info!("system alert: {}", body.replace('\n', "; "));
            let urgent = !ch.new.is_empty();
            let alerts = alerts.clone();
            thread::spawn(move || {
                send(&alerts, &title, &body, urgent);
            });
        }
        thread::sleep(Duration::from_secs(60));
    }
}

/// Send a system message to every service that is set up. Returns the
/// services tried, each with whether it went out. Blocking.
pub fn send(alerts: &crate::config::AlertConfig, title: &str, body: &str, urgent: bool) -> Vec<(&'static str, bool)> {
    send_via(alerts, title, body, urgent, &|_| true)
}

/// As `send`, through only the services `uses` picks (ALERT_CHANNELS ids).
pub fn send_via(alerts: &crate::config::AlertConfig, title: &str, body: &str, urgent: bool, uses: &dyn Fn(&str) -> bool) -> Vec<(&'static str, bool)> {
    send_for(alerts, title, body, urgent, uses, None)
}

/// As `send_via`, about one camera (`camera`): browser notifications then
/// go to everyone who may see that camera, not only to admins.
pub fn send_for(alerts: &crate::config::AlertConfig, title: &str, body: &str, urgent: bool, uses: &dyn Fn(&str) -> bool, camera: Option<&str>) -> Vec<(&'static str, bool)> {
    send_for_with(alerts, title, body, urgent, uses, camera, None)
}

/// As `send_for`, with a picture: its file (for the services that attach
/// one) and its address on this server (for browser notifications).
pub fn send_for_with(alerts: &crate::config::AlertConfig, title: &str, body: &str, urgent: bool, uses: &dyn Fn(&str) -> bool, camera: Option<&str>, picture: Option<(&std::path::Path, &str)>) -> Vec<(&'static str, bool)> {
    let mut out = Vec::new();
    let tags = vec![if urgent { "warning".to_string() } else { "white_check_mark".to_string() }, "ambersight".to_string()];
    let link = alerts.link_base().map(|u| format!("{u}/#settings/system/status"));
    let note = super::notify::Note {
        title,
        body,
        tags: &tags,
        priority: if urgent { super::notify::Priority::High } else { super::notify::Priority::Low },
        click: link.as_deref(),
        image: picture.map(|p| p.0),
    };
    if let Some(url) = alerts.ntfy_url.as_deref().filter(|u| !u.is_empty() && uses("ntfy")) {
        out.push(("ntfy", super::notify::send_ntfy(url, alerts.ntfy_token.as_deref(), &note)));
    }
    if let (Some(token), Some(user)) = (alerts.pushover_token.as_deref().filter(|t| !t.is_empty()), alerts.pushover_user.as_deref().filter(|u| !u.is_empty() && uses("pushover"))) {
        let api = std::env::var("AMBERSIGHT_PUSHOVER_API").unwrap_or_else(|_| "https://api.pushover.net/1/messages.json".into());
        let p = super::notify::Pushover { token, user, device: alerts.pushover_device.as_deref(), sound: alerts.pushover_sound.as_deref(), api_url: &api };
        out.push(("Pushover", super::notify::send_pushover(&p, &note)));
    }
    if crate::email::enabled(alerts) && uses("email") {
        let r = crate::email::send_text(alerts, &format!("AmberSight: {title}"), body);
        if let Err(e) = &r {
            warn!("email: system alert: {e:#}");
        }
        out.push(("email", r.is_ok()));
    }
    if let (Some(tok), Some(chat)) = (alerts.telegram_bot_token.as_deref().filter(|t| !t.is_empty()), alerts.telegram_chat_id.as_deref().filter(|c| !c.is_empty() && uses("telegram"))) {
        let base = alerts.telegram_api_url.clone().unwrap_or("https://api.telegram.org".into());
        let url = format!("{}/bot{}/sendMessage", base.trim_end_matches('/'), tok);
        let payload = serde_json::json!({ "chat_id": chat, "text": format!("AmberSight: {title}\n{body}") }).to_string();
        out.push(("Telegram", post_json(&url, &payload)));
    }
    if let Some(url) = alerts.webhook_url.as_deref().filter(|u| !u.is_empty() && uses("webhook")) {
        let payload = serde_json::json!({ "type": "system", "title": title, "message": body, "problem": urgent }).to_string();
        out.push(("webhook", post_json(url, &payload)));
    }
    // Last: the other channels never wait for the push services.
    if uses("push") {
        let (aud, url, tag) = match camera {
            Some(c) => {
                let enc: String = url::form_urlencoded::byte_serialize(c.as_bytes()).collect();
                (crate::push::Audience::Camera(c.to_string()), format!("/?alert=-1&cam={enc}"), format!("cam-note-{}", crate::config::slug(c)))
            }
            None => (crate::push::Audience::Admins, "/#settings/system/status".to_string(), "system".to_string()),
        };
        let kind = match (camera, urgent) {
            (Some(_), _) => crate::push::MsgType::Camera,
            (None, true) => crate::push::MsgType::Problem,
            (None, false) => crate::push::MsgType::Notice,
        };
        let m = crate::push::Message { title: title.to_string(), body: body.to_string(), url, tag, image: picture.map(|p| p.1.to_string()), urgent, kind, camera: camera.map(String::from), ..Default::default() };
        if let Some(r) = crate::push::send(&aud, &m) {
            out.push(("browser notifications", r.sent > 0));
        }
    }
    out
}

pub(super) fn post_json(url: &str, body: &str) -> bool {
    // (On curl's stdin: a Telegram URL holds the bot token.)
    let args: Vec<String> = ["-s", "-f", "-m", "10", "-X", "POST", "-H", "Content-Type: application/json", "--data-binary", body, url].iter().map(|s| s.to_string()).collect();
    let ok = crate::child::curl(&args).is_ok_and(|o| o.status.success());
    if !ok {
        warn!("system alert: POST failed");
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(k: &str) -> (String, String) {
        (k.to_string(), format!("{k} text"))
    }

    #[test]
    fn reported_after_the_delay_once_then_resolved_after_clearing() {
        let mut t = Tracker::default();
        assert_eq!(t.update(0, &[p("offline:1")], 300), Change::default());
        assert_eq!(t.update(240, &[p("offline:1")], 300), Change::default());
        let c = t.update(300, &[p("offline:1")], 300);
        assert_eq!(c.new, vec!["offline:1 text"]);
        assert_eq!(t.active(), vec!["offline:1 text"]);
        // Still there: nothing new.
        assert_eq!(t.update(360, &[p("offline:1")], 300), Change::default());
        // Gone for one minute: not yet resolved; two minutes: resolved.
        assert_eq!(t.update(420, &[], 300), Change::default());
        let c = t.update(480, &[], 300);
        assert_eq!(c.resolved, vec!["offline:1 text"]);
        assert!(t.active().is_empty());
    }

    #[test]
    fn short_problems_are_never_reported() {
        let mut t = Tracker::default();
        t.update(0, &[p("norec:2")], 300);
        t.update(60, &[], 300);
        assert_eq!(t.update(180, &[], 300), Change::default());
        // It comes back later: the delay starts again.
        assert_eq!(t.update(240, &[p("norec:2")], 300), Change::default());
        assert_eq!(t.update(480, &[p("norec:2")], 300), Change::default());
        assert_eq!(t.update(540, &[p("norec:2")], 300).new.len(), 1);
    }

    #[test]
    fn a_blip_does_not_resolve_and_re_report() {
        let mut t = Tracker::default();
        t.update(0, &[p("offline:1")], 60);
        assert_eq!(t.update(60, &[p("offline:1")], 60).new.len(), 1);
        // Online for one check, offline again: neither resolved nor new.
        assert_eq!(t.update(120, &[], 60), Change::default());
        assert_eq!(t.update(180, &[p("offline:1")], 60), Change::default());
    }

    #[test]
    fn messages_group_problems() {
        let one = Change { new: vec!["Porch is offline".into()], resolved: vec![] };
        assert_eq!(message("Cabin", &one), ("Cabin: Porch is offline".into(), "Problem: Porch is offline".into()));
        let two = Change { new: vec!["A is offline".into(), "B is offline".into()], resolved: vec![] };
        assert_eq!(message("Cabin", &two).0, "Cabin: 2 problems");
        let back = Change { new: vec![], resolved: vec!["A is offline".into()] };
        assert_eq!(message("Cabin", &back), ("Cabin: resolved: A is offline".into(), "Resolved: A is offline".into()));
        let mixed = Change { new: vec!["B is offline".into()], resolved: vec!["A is offline".into()] };
        assert_eq!(message("Cabin", &mixed), ("Cabin: 1 new problem".into(), "Problem: B is offline\nResolved: A is offline".into()));
    }
}
