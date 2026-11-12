use super::*;

/// When notifications were last sent, for [alerts] min_interval / group_interval.
#[derive(Default)]
pub struct Throttle {
    per_camera: std::collections::HashMap<String, i64>,
    last_any: Option<i64>,
    /// Alerts held back since the last one that went out (reported with it).
    pub held: u32,
}

impl Throttle {
    /// Whether an alert for `camera` at `now` (seconds) may go out; records it if so.
    pub fn allow(&mut self, camera: &str, now: i64, per_camera: u32, group: u32) -> bool {
        let recent = |last: Option<i64>, gap: u32| gap > 0 && last.is_some_and(|t| now - t < i64::from(gap));
        if recent(self.per_camera.get(camera).copied(), per_camera) || recent(self.last_any, group) {
            self.held += 1;
            return false;
        }
        self.per_camera.insert(camera.to_string(), now);
        self.last_any = Some(now);
        true
    }
}

/// How an alert rule's alert differs from an ordinary one.
pub struct AlertOver {
    pub title: String,
    pub priority: super::notify::Priority,
    pub rule_id: String,
    /// The rule's name (the title without the camera).
    pub name: String,
}

impl Engine {
    /// classic NVR-style alerts: webhook JSON POST + optional Telegram message.
    /// Uses system curl (handles https); warns if curl is unavailable.
    pub(super) fn fire_alert(&self, ev: &Event) {
        self.fire_alert_as(ev, None);
    }

    /// As `fire_alert`; `headline` is the title to use instead of "Person at
    /// Front Door" (the stranger alerts say who it was).
    pub(super) fn fire_alert_as(&self, ev: &Event, headline: Option<&str>) {
        // MQTT always reports the event (Home Assistant sensors stay accurate);
        // notifications obey the throttle.
        if let Some(m) = self.mqtt.get() {
            let jpeg = ev
                .thumb
                .as_deref()
                .filter(|t| !t.is_empty())
                .and_then(|t| std::fs::read(self.data_dir.join("thumbs").join(format!("{t}.jpg"))).ok());
            m.publish_event(ev, jpeg);
        }
        // A camera with a quiet rule waits a moment for its pictures to be checked.
        if self.hold_for_quiet(ev, headline) {
            return;
        }
        self.notify_event(ev, None, headline);
    }

    /// The notifications for an event. `over`: an alert rule's own title
    /// and priority (it skips the alert interval: it is about something
    /// specific, and the ordinary alert of the same event may have used it).
    pub(super) fn notify_event(&self, ev: &Event, over: Option<&AlertOver>, headline: Option<&str>) {
        let (alerts, cam) = {
            let c = self.config.lock().unwrap();
            (c.alerts.clone(), c.cameras.iter().find(|x| x.name == ev.camera).cloned())
        };
        // The AI review says why nothing went out. (A rule's alert is a second
        // notification: the ordinary one may have gone out.)
        let not_sent = |why: &str| {
            if over.is_none() {
                if let Err(e) = self.db.set_ai_review_alert(&ev.id, false, why) {
                    tracing::warn!("could not update the AI review of {}: {e:#}", ev.id);
                }
            }
        };
        // Paused for a while from the camera's menu.
        if self.alerts_paused_until(&ev.camera).is_some() {
            tracing::debug!("alerts for {} are paused", ev.camera);
            not_sent("paused");
            return;
        }
        // Per-camera: notifications off, or outside the camera's alert window.
        if let Some(c) = &cam {
            if c.alerts == Some(false) {
                tracing::debug!("alerts are off for {}", ev.camera);
                not_sent("alerts_off");
                return;
            }
            if let Some(w) = c.alert_schedule.as_deref().and_then(crate::config::parse_schedule) {
                let now = chrono::Local::now();
                if !crate::config::schedule_active(Some(w), chrono::Timelike::hour(&now) * 60 + chrono::Timelike::minute(&now)) {
                    tracing::debug!("alert for {} outside its alert window", ev.camera);
                    not_sent("alert_window");
                    return;
                }
            }
        }
        // "Notify only for" objects: other objects are still detected, recorded and
        // listed. Sounds, doorbell presses and packages always go out. (Rules and
        // zone events say their own thing and skip it.)
        if over.is_none() && !ev.objects.first().is_some_and(|o| crate::tracker::is_zone_tag(o)) {
            if let Some(c) = &cam {
                if !c.notifies_for_event(&ev.objects, super::ai_check::ai_active(self, c)) {
                    tracing::debug!("alert for event {} on {} skipped: not one of its notify-only objects", ev.id, ev.camera);
                    not_sent("notify_objects");
                    return;
                }
                // Areas that never notify: everything that confirmed the event stood in one.
                if !c.alert_ignore_polys.is_empty() {
                    let silenced = self.db.ai_review(&ev.id).ok().flatten().is_some_and(|d| super::noisy::silenced_by_area(c, &ev.objects, &d));
                    if silenced {
                        tracing::debug!("alert for event {} on {} skipped: inside an area that does not notify", ev.id, ev.camera);
                        not_sent("notify_area");
                        return;
                    }
                }
            }
        }
        let per_camera = cam.as_ref().and_then(|c| c.alert_interval).unwrap_or(alerts.min_interval);
        let uses = |ch: &str| cam.as_ref().is_none_or(|c| c.uses_channel(ch));
        let held = {
            let mut th = self.alert_throttle.lock().unwrap();
            // (Deliveries and zone events skip it: a loitering alert right
            // after a motion alert must still be told.)
            let zone_event = ev.objects.first().is_some_and(|o| crate::tracker::is_zone_tag(o));
            if over.is_none() && !super::packages::is_package_event(ev) && !zone_event && !th.allow(&ev.camera, chrono::Utc::now().timestamp(), per_camera, alerts.group_interval) {
                tracing::debug!("alert for event {} on {} held back by the alert interval", ev.id, ev.camera);
                drop(th);
                not_sent("alert_interval");
                return;
            }
            std::mem::take(&mut th.held)
        };
        if uses("email") {
            self.send_email(ev, &alerts);
        }
        let endpoint = alerts.webhook_url.clone().filter(|u| !u.is_empty() && uses("webhook"));
        let bot = alerts.telegram_bot_token.clone().filter(|t| !t.is_empty() && uses("telegram"));
        let chat = alerts.telegram_chat_id.clone().filter(|c| !c.is_empty());
        let base = alerts.telegram_api_url.clone().unwrap_or("https://api.telegram.org".into());
        let ntfy = alerts.ntfy_url.clone().filter(|u| !u.is_empty() && uses("ntfy"));
        let pushover = match (&alerts.pushover_token, &alerts.pushover_user) {
            (Some(t), Some(u)) if !t.is_empty() && !u.is_empty() && uses("pushover") => Some((t.clone(), u.clone())),
            _ => None,
        };
        // Title/body for the picture services: "Person at Front Door", local time.
        let mut priority = super::notify::Priority::parse(cam.as_ref().and_then(|c| c.alert_priority.as_deref()).or(alerts.priority.as_deref()));
        let unusual_mode = cam.as_ref().and_then(|c| c.unusual.as_ref()).map(|u| u.mode().to_string()).filter(|m| m != "tag" && ev.objects.iter().any(|o| o == "unusual"));
        let first = ev.objects.iter().find(|o| *o != "unusual");
        let what = first.map(|o| { let mut c = o.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default() });
        let note_title = if let Some(o) = over {
            priority = o.priority;
            o.title.clone()
        } else if let Some(h) = headline {
            // (A stranger headline names who it was; an unusual event keeps its priority.)
            if let Some(mode) = &unusual_mode {
                priority = if mode == "alert" { super::notify::Priority::High } else { super::notify::Priority::Low };
            }
            h.to_string()
        } else if let Some(mode) = &unusual_mode {
            // "Unusual: person at Back Gate at 3:12 AM"
            priority = if mode == "alert" { super::notify::Priority::High } else { super::notify::Priority::Low };
            let at = chrono::DateTime::<chrono::Utc>::from_timestamp(ev.start, 0).map(|t| t.with_timezone(&chrono::Local).format("%-I:%M %p").to_string()).unwrap_or_default();
            format!("Unusual: {} at {} at {}", first.map_or("motion".to_string(), |o| o.to_lowercase()), ev.camera, at)
        } else {
            format!("{} at {}", what.clone().unwrap_or_else(|| "Motion".into()), ev.camera)
        };
        let note_body = format!(
            "{}{}{}",
            chrono::DateTime::<chrono::Utc>::from_timestamp(ev.start, 0)
                .map(|t| t.with_timezone(&chrono::Local).format("%a %-I:%M:%S %p").to_string())
                .unwrap_or_default(),
            if ev.objects.is_empty() { String::new() } else { format!(" · {}", ev.objects.join(", ")) },
            if held > 0 { format!(" (+{held} more since the last alert)") } else { String::new() }
        );
        // (Tags made by rules, zones and plates hold names with spaces and commas: not push tags.)
        let mut note_tags: Vec<String> = ev.objects.iter().filter(|o| !crate::tracker::is_tag_label(o)).take(3).cloned().collect();
        note_tags.push("camera".into());
        let link = alerts.link_base().map(|u| {
            let cam: String = url::form_urlencoded::byte_serialize(ev.camera.as_bytes()).collect();
            format!("{u}/?alert={}&cam={cam}&at={}", ev.camera_id, ev.start * 1000)
        });
        let ntfy_token = alerts.ntfy_token.clone();
        let po_extra = (alerts.pushover_device.clone(), alerts.pushover_sound.clone());
        let payload = serde_json::to_string(ev).unwrap_or("{}".into());
        let ev_id = ev.id.clone();
        let tg_what = match over {
            Some(o) => o.name.clone(),
            None => ev.objects.first().filter(|o| crate::tracker::is_zone_tag(o)).cloned().unwrap_or_else(|| "motion".into()),
        };
        let tg_when = chrono::DateTime::<chrono::Utc>::from_timestamp(ev.start, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or("?".into());
        let tg_more = if held > 0 { format!(" (+{held} more since the last alert)") } else { String::new() };
        let tg_text = match headline.filter(|_| over.is_none()) {
            Some(h) => format!("AmberSight: {h} at {tg_when}{tg_more}"),
            None => format!("AmberSight: {tg_what} on {} at {tg_when}{tg_more}", ev.camera),
        };
        let action = alerts.action_command.clone().filter(|c| !c.is_empty() && uses("action"));
        let imgpost = alerts.image_post_url.clone().filter(|u| !u.is_empty() && uses("image_post"));
        let ha_url = alerts.ha_url.clone().filter(|u| !u.is_empty() && uses("home_assistant"));
        let ha_token = alerts.ha_token.filter(|t| !t.is_empty());
        let ha_entity = alerts.ha_entity.filter(|t| !t.is_empty());
        let push = uses("push") && crate::push::any_subscribed();
        if endpoint.is_none() && ntfy.is_none() && pushover.is_none() && (bot.is_none() || chat.is_none())
            && action.is_none() && imgpost.is_none() && ha_url.is_none() && !push {
            return;
        }
        let thumb_key = ev.thumb.clone().unwrap_or("".into());
        let thumb_path = self.data_dir.join("thumbs").join(format!("{}.jpg", thumb_key));
        let have_thumb = !thumb_key.is_empty() && std::fs::exists(&thumb_path).unwrap_or(false);
        // Browser notifications: tapping one opens this camera at the alert;
        // the picture is fetched by the browser itself (signed in).
        let push_msg = push.then(|| {
            let cam: String = url::form_urlencoded::byte_serialize(ev.camera.as_bytes()).collect();
            crate::push::Message {
                title: note_title.clone(),
                body: note_body.clone(),
                url: format!("/?alert={}&cam={cam}&at={}", ev.camera_id, ev.start * 1000),
                tag: match over {
                    Some(o) => format!("rule-{}-{}", crate::config::slug(&o.rule_id), crate::config::slug(&ev.camera)),
                    None => format!("cam-{}", crate::config::slug(&ev.camera)),
                },
                image: have_thumb.then(|| format!("/api/events/{thumb_key}/thumb")),
                urgent: matches!(priority, super::notify::Priority::High | super::notify::Priority::Urgent),
                kind: crate::push::MsgType::Event,
                camera: Some(ev.camera.clone()),
                camera_id: Some(ev.camera_id),
                event_id: Some(ev.id.clone()),
                labels: ev.objects.clone(),
                at_ms: Some(ev.start * 1000),
            }
        });
        let push_cam = ev.camera.clone();
        thread::spawn({
            let endpoint = endpoint.clone();
            let payload = payload.clone();
            let ev_id = ev_id.clone();
            let bot = bot.clone();
            let chat = chat.clone();
            let base = base.clone();
            let tg_text = tg_text.clone();
            let ntfy = ntfy.clone();
            let action = action.clone();
            let imgpost = imgpost.clone();
            let ha_url = ha_url.clone();
            let ha_token = ha_token.clone();
            let ha_entity = ha_entity.clone();
            let have_thumb = have_thumb;
            let clip_path = self.data_dir.join("clips").join(format!("{}.mp4", ev_id));
            let thumb_path = thumb_path;
            let have_thumb = have_thumb;
            move || {
                // On its own, so a slow push service never delays the other channels.
                if let Some(m) = push_msg {
                    thread::spawn(move || {
                        crate::push::send(&crate::push::Audience::Camera(push_cam), &m);
                    });
                }
                let post = |url: String, body: String, label: String| {
                    let args: Vec<String> = ["-s", "-m", "10", "-X", "POST", "-H", "Content-Type: application/json", "--data", &body, &url].iter().map(|s| s.to_string()).collect();
                    let status = crate::child::curl(&args).map(|o| o.status);
                    match status {
                        Ok(s) if s.success() => {}
                        Ok(_) => warn!("{}: POST failed for event {}", label, ev_id),
                        Err(_) => warn!("{}: curl not available; event {} not delivered", label, ev_id),
                    }
                };
                if let Some(url) = endpoint {
                    post(url, payload, "webhook".into());
                }
                if let (Some(tok), Some(cid)) = (bot, chat) {
                    if have_thumb {
                        // classic-style alert with picture: sendPhoto + caption.
                        let f_chat = format!("chat_id={}", cid);
                        let f_cap = format!("caption={}", tg_text);
                        let f_photo = format!("photo=@{}", thumb_path.to_string_lossy());
                        let f_url = format!("{}/bot{}/sendPhoto", base.trim_end_matches('/'), tok);
                        // (On curl's stdin: the URL holds the bot token.)
                        let args: Vec<String> = ["-s", "-m", "20", "-X", "POST", "-F", &f_chat, "-F", &f_cap, "-F", &f_photo, &f_url].iter().map(|s| s.to_string()).collect();
                        let r = crate::child::curl(&args).map(|o| o.status);
                        match r {
                            Ok(s) if s.success() => {}
                            Ok(_) => warn!("telegram: sendPhoto failed for event {}", ev_id),
                            Err(_) => warn!("telegram: curl not available; event {} not delivered", ev_id),
                        }
                    } else {
                        let url = format!("{}/bot{}/sendMessage", base.trim_end_matches('/'), tok);
                        let body = format!(
                            "{{\"chat_id\":\"{}\",\"text\":\"{}\"}}",
                            cid,
                            tg_text.replace("\"", "\\\""),
                        );
                        post(url, body, "telegram".into());
                    }
                }
                let note = super::notify::Note {
                    title: &note_title,
                    body: &note_body,
                    tags: &note_tags,
                    priority,
                    click: link.as_deref(),
                    image: if have_thumb { Some(thumb_path.as_path()) } else { None },
                };
                if let Some(url) = ntfy {
                    super::notify::send_ntfy(&url, ntfy_token.as_deref(), &note);
                }
                if let Some((token, user)) = pushover {
                    let api = std::env::var("AMBERSIGHT_PUSHOVER_API").unwrap_or_else(|_| "https://api.pushover.net/1/messages.json".into());
                    let p = super::notify::Pushover { token: &token, user: &user, device: po_extra.0.as_deref(), sound: po_extra.1.as_deref(), api_url: &api };
                    super::notify::send_pushover(&p, &note);
                }
                if let Some(cmd) = action {
                    // Action set: whitespace-split argv, no shell; {image}/{clip}
                    // macros expand to the event thumbnail and clip paths.
                    let img_s = thumb_path.to_string_lossy().into_owned();
                    let clip_s = clip_path.to_string_lossy().into_owned();
                    let mut argv: Vec<String> = Vec::new();
                    for a in cmd.split(" ") {
                        let v = a.replace("{image}", &img_s).replace("{clip}", &clip_s);
                        argv.push(v);
                    }
                    if argv.len() > 1 {
                        let head = argv[0].clone();
                        let rest: Vec<String> = argv.iter().skip(1).map(|s| s.clone()).collect();
                        let r = std::process::Command::new(head.clone()).args(&rest)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                        match r {
                            Ok(s) if s.success() => {}
                            Ok(_) => warn!("action: program failed for event {}", ev_id),
                            Err(_) => warn!("action: could not run {} for event {}", head, ev_id),
                        }
                    }
                }
                if let Some(url) = imgpost {
                    // Image posting: raw JPEG body.
                    if have_thumb {
                        let mut cargs: Vec<String> = vec![
                            "-s".into(), "-m".into(), "15".into(), "-X".into(), "POST".into(),
                            "-H".into(), "Content-Type: image/jpeg".into(),
                            "--data-binary".into(), format!("@{}", thumb_path.to_string_lossy()),
                        ];
                        cargs.push(url);
                        let r = crate::child::curl(&cargs).map(|o| o.status);
                        match r {
                            Ok(s) if s.success() => {}
                            Ok(_) => warn!("image post: failed for event {}", ev_id),
                            Err(_) => warn!("image post: curl not available"),
                        }
                    }
                }
                if let (Some(u), Some(tok), Some(ent)) = (ha_url, ha_token, ha_entity) {
                    // Home Assistant service call: switch.turn_on the bound entity.
                    let body = format!("{{\"entity_id\":\"{}\"}}", ent);
                    let cargs: Vec<String> = vec![
                        "-s".into(), "-m".into(), "10".into(), "-X".into(), "POST".into(),
                        "-H".into(), format!("Authorization: Bearer {}", tok),
                        "-H".into(), "Content-Type: application/json".into(),
                        "--data".into(), body,
                        format!("{}/api/services/switch/turn_on", u.trim_end_matches('/')),
                    ];
                    let r = crate::child::curl(&cargs).map(|o| o.status);
                    match r {
                        Ok(s) if s.success() => {}
                        Ok(_) => warn!("home assistant: service call failed for event {}", ev_id),
                        Err(_) => warn!("home assistant: curl not available"),
                    }
                }
            }
        });
    }

    /// Email for a finished event (non-blocking).
    fn send_email(&self, ev: &Event, alerts: &crate::config::AlertConfig) {
        let want_mail = crate::email::enabled(alerts);
        if !want_mail {
            return;
        }
        let jpeg = ev
            .thumb
            .as_deref()
            .filter(|t| !t.is_empty())
            .and_then(|t| std::fs::read(self.data_dir.join("thumbs").join(format!("{t}.jpg"))).ok());
        {
            let (alerts, ev) = (alerts.clone(), ev.clone());
            thread::spawn(move || {
                if let Err(e) = crate::email::send(&alerts, &ev, jpeg) {
                    warn!("email: event {}: {e:#}", ev.id);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Throttle;

    #[test]
    fn no_limits_lets_everything_through() {
        let mut t = Throttle::default();
        assert!((0..5).all(|i| t.allow("A", 100 + i, 0, 0)));
        assert_eq!(t.held, 0);
    }

    #[test]
    fn per_camera_gap() {
        let mut t = Throttle::default();
        assert!(t.allow("A", 0, 60, 0));
        assert!(!t.allow("A", 30, 60, 0));
        assert!(t.allow("B", 30, 60, 0)); // other camera unaffected
        assert!(t.allow("A", 60, 60, 0));
        assert_eq!(t.held, 1);
    }

    #[test]
    fn group_gap_spans_cameras() {
        let mut t = Throttle::default();
        assert!(t.allow("A", 0, 0, 20));
        assert!(!t.allow("B", 5, 0, 20));
        assert!(!t.allow("C", 19, 0, 20));
        assert!(t.allow("C", 20, 0, 20));
        assert_eq!(t.held, 2);
    }

    #[test]
    fn held_back_alerts_do_not_restart_the_gap() {
        let mut t = Throttle::default();
        assert!(t.allow("A", 0, 60, 0));
        assert!(!t.allow("A", 50, 60, 0));
        assert!(t.allow("A", 61, 60, 0));
    }
}
