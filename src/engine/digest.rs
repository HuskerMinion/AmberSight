//! The morning summary on the clock (see crate::digest for what it says):
//! builds it from the database at the chosen time, keeps it for its link,
//! and sends it through the chosen services. Also keeps the log of camera
//! offline stretches the summary reads.

use super::*;
use crate::digest as dg;
use std::collections::HashMap;
use std::time::Duration;

/// Longest the short (push) text may be, in characters.
const PUSH_BODY: usize = 900;
/// Thumbnails attached to the email at most.
const EMAIL_IMAGES: usize = 8;
/// Offline stretches and summaries older than this are forgotten.
const KEEP_OUTAGES_MS: i64 = 60 * 24 * 3600 * 1000;

/// Watches which cameras are offline and logs each stretch.
pub fn outage_loop(engine: Arc<Engine>) {
    if let Err(e) = engine.db.outages_close_stale() {
        warn!("digest: {e:#}");
    }
    // Camera name -> the open stretch's id.
    let mut open: HashMap<String, i64> = HashMap::new();
    let mut ticks = 0u32;
    loop {
        thread::sleep(Duration::from_secs(15));
        let now_ms = Utc::now().timestamp_millis();
        let handles: Vec<Arc<CameraHandle>> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
        for h in handles {
            let name = h.running.read().unwrap().name.clone();
            let enabled = h.active.load(Ordering::Relaxed);
            let (online, sleeping, since) = {
                let st = h.state.read().unwrap();
                (st.online, st.sleeping, st.offline_since)
            };
            // A camera switched off, or a sleeping battery camera, is not offline.
            let down = enabled && !online && !sleeping;
            match (down, open.get(&name).copied()) {
                (true, None) => {
                    // The stretch began when frames stopped (but not before this run started watching).
                    let start = since.map_or(now_ms, |s| s * 1000).min(now_ms);
                    match engine.db.outage_open(&name, start) {
                        Ok(id) => {
                            open.insert(name, id);
                        }
                        Err(e) => warn!("digest: {e:#}"),
                    }
                }
                (true, Some(id)) => {
                    let _ = engine.db.outage_touch(id, now_ms);
                }
                (false, Some(id)) => {
                    let _ = engine.db.outage_close(id, now_ms);
                    open.remove(&name);
                }
                (false, None) => {}
            }
        }
        ticks += 1;
        if ticks % 240 == 0 {
            let _ = engine.db.outages_prune(now_ms - KEEP_OUTAGES_MS);
        }
    }
}

/// Sends the summary when it is due.
pub fn digest_loop(engine: Arc<Engine>) {
    // When this loop last sent one: if the record cannot be saved, the loop
    // must not try again every 30 seconds.
    let mut mine: Option<i64> = None;
    loop {
        thread::sleep(Duration::from_secs(30));
        let cfg = engine.config.lock().unwrap().digest.clone();
        if !cfg.enabled {
            continue;
        }
        let last = engine.db.digest_last_auto().ok().flatten().map(|l| l.0).max(mine);
        if !dg::due(&cfg, &chrono::Local::now(), last) {
            continue;
        }
        mine = Some(Utc::now().timestamp_millis());
        let d = engine.build_digest(&cfg, chrono::Local::now());
        if d.is_quiet() && cfg.skip_if_empty {
            info!("morning summary: nothing happened, nothing sent");
            // (Remembered, so it is not looked at again today.)
            let _ = engine.db.digest_add(Utc::now().timestamp_millis(), d.from_ms, d.to_ms, true, &serde_json::to_string(&d).unwrap_or_default());
            continue;
        }
        let r = engine.deliver_digest(&d, true);
        info!("morning summary sent: {}", r.0.iter().map(|(c, ok)| format!("{c} {}", if *ok { "ok" } else { "failed" })).collect::<Vec<_>>().join(", "));
    }
}

impl Engine {
    /// The summary for the period that ends `now`.
    pub fn build_digest(&self, cfg: &crate::config::DigestConfig, now: chrono::DateTime<chrono::Local>) -> dg::Digest {
        let last_to = self.db.digest_last_to().ok().flatten();
        let (from_ms, to_ms) = dg::period_for(cfg, &now, last_to);
        let cams: Vec<(String, i64)> = self
            .cameras
            .read()
            .unwrap()
            .iter()
            .flatten()
            .map(|h| (h.cfg.read().unwrap().name.clone(), h.id as i64))
            .filter(|(n, _)| cfg.covers(n))
            .collect();
        let events = self.db.digest_events(from_ms / 1000, to_ms / 1000 + 1, &self.hide_unconfirmed_cameras()).unwrap_or_default();
        let tamper = self.db.tamper_between(from_ms / 1000, to_ms / 1000 + 1).unwrap_or_default();
        let outages = self.db.outages_between(from_ms, to_ms).unwrap_or_default();
        let people: std::collections::HashSet<String> = self.db.person_names().unwrap_or_default().into_iter().collect();
        let mut d = dg::build(&dg::Input { from_ms, to_ms, period: cfg.period(), cameras: &cams, events: &events, tamper: &tamper, outages: &outages, people: &people });
        // Cameras that sent far too many notifications (what to do is on Status).
        d.notes = self.noisy_notes();
        d
    }

    /// Keep a summary (for its link) and send it. Returns the id it was
    /// kept under and how each service answered.
    pub fn deliver_digest(&self, d: &dg::Digest, auto: bool) -> (Vec<(&'static str, bool)>, Option<i64>) {
        let (alerts, cfg) = {
            let c = self.config.lock().unwrap();
            (c.alerts.clone(), c.digest.clone())
        };
        let id = match self.db.digest_add(Utc::now().timestamp_millis(), d.from_ms, d.to_ms, auto, &serde_json::to_string(d).unwrap_or_default()) {
            Ok(id) => Some(id),
            Err(e) => {
                warn!("digest: could not keep the summary: {e:#}");
                None
            }
        };
        if auto && id.is_none() {
            warn!("digest: the scheduled summary was not sent, it could not be saved");
            return (Vec::new(), None);
        }
        let path = format!("/#digest?id={}", id.unwrap_or(0));
        let base = alerts.link_base();
        let click = base.as_ref().map(|b| format!("{b}/{}", path.trim_start_matches('/')));
        let title = dg::title(d);
        let uses = |ch: &str| cfg.uses_channel(ch);
        let mut out: Vec<(&'static str, bool)> = Vec::new();
        let short = dg::short_body(d, PUSH_BODY);
        let text = dg::full_text(d, base.as_deref());
        let tags = vec!["sunrise".to_string(), "ambersight".to_string()];
        let note = super::notify::Note { title: &title, body: &text, tags: &tags, priority: super::notify::Priority::Low, click: click.as_deref(), image: None };
        if let Some(url) = alerts.ntfy_url.as_deref().filter(|u| !u.is_empty() && uses("ntfy")) {
            out.push(("ntfy", super::notify::send_ntfy(url, alerts.ntfy_token.as_deref(), &note)));
        }
        if let (Some(token), Some(user)) = (alerts.pushover_token.as_deref().filter(|t| !t.is_empty()), alerts.pushover_user.as_deref().filter(|u| !u.is_empty() && uses("pushover"))) {
            let api = std::env::var("AMBERSIGHT_PUSHOVER_API").unwrap_or_else(|_| "https://api.pushover.net/1/messages.json".into());
            let p = super::notify::Pushover { token, user, device: alerts.pushover_device.as_deref(), sound: alerts.pushover_sound.as_deref(), api_url: &api };
            let short_note = super::notify::Note { title: &title, body: &short, tags: &tags, priority: super::notify::Priority::Low, click: click.as_deref(), image: None };
            out.push(("Pushover", super::notify::send_pushover(&p, &short_note)));
        }
        if let (Some(tok), Some(chat)) = (alerts.telegram_bot_token.as_deref().filter(|t| !t.is_empty()), alerts.telegram_chat_id.as_deref().filter(|c| !c.is_empty() && uses("telegram"))) {
            let api = alerts.telegram_api_url.clone().unwrap_or("https://api.telegram.org".into());
            let url = format!("{}/bot{}/sendMessage", api.trim_end_matches('/'), tok);
            let msg: String = format!("AmberSight: {title}\n{text}").chars().take(4000).collect();
            let payload = serde_json::json!({ "chat_id": chat, "text": msg, "disable_web_page_preview": true }).to_string();
            out.push(("Telegram", super::system_alerts::post_json(&url, &payload)));
        }
        if crate::email::enabled(&alerts) && uses("email") {
            let images = self.digest_images(d);
            let cids: Vec<(String, String)> = images.iter().enumerate().map(|(i, (ev, _))| (ev.clone(), dg::cid_for(i))).collect();
            let html = dg::html(d, base.as_deref(), &cids);
            let pics: Vec<(String, Vec<u8>)> = images.into_iter().enumerate().map(|(i, (_, b))| (dg::cid_for(i), b)).collect();
            let subject = format!("AmberSight: {title}");
            let r = crate::email::send_html(&alerts, &subject, &text, &html, pics);
            if let Err(e) = &r {
                warn!("email: morning summary: {e:#}");
            }
            out.push(("email", r.is_ok()));
        }
        if let Some(url) = alerts.webhook_url.as_deref().filter(|u| !u.is_empty() && uses("webhook")) {
            let payload = serde_json::json!({ "type": "digest", "title": title, "message": short, "digest": d }).to_string();
            out.push(("webhook", super::system_alerts::post_json(url, &payload)));
        }
        // Last: the other services never wait for the push servers.
        if uses("push") {
            let m = crate::push::Message {
                title: title.clone(),
                body: short.clone(),
                url: path,
                tag: "digest".into(),
                kind: crate::push::MsgType::Notice,
                ..Default::default()
            };
            if let Some(r) = crate::push::send(&crate::push::Audience::Admins, &m) {
                out.push(("browser notifications", r.sent > 0));
            }
        }
        (out, id)
    }

    /// The thumbnails of the summary's highlights: (event id, JPEG).
    fn digest_images(&self, d: &dg::Digest) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for h in d.cameras.iter().flat_map(|c| c.highlights.iter()) {
            if out.len() >= EMAIL_IMAGES {
                break;
            }
            let (Some(ev), Some(th)) = (&h.event_id, &h.thumb) else { continue };
            if out.iter().any(|(e, _)| e == ev) {
                continue;
            }
            if let Ok(b) = std::fs::read(self.data_dir.join("thumbs").join(format!("{th}.jpg"))) {
                // A thumbnail is small; anything big is left out of the email.
                if b.len() <= 300_000 {
                    out.push((ev.clone(), b));
                }
            }
        }
        out
    }
}
