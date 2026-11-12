//! Email alerts over SMTP (rustls, no OpenSSL).

use crate::config::AlertConfig;
use crate::engine::Event;
use anyhow::{anyhow, Context, Result};
use lettre::message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use std::time::Duration;

pub fn subject(ev: &Event, when: &str) -> String {
    let what = if ev.objects.is_empty() { "motion".to_string() } else { ev.objects.join(", ") };
    format!("AmberSight: {} on {} at {}", what, ev.camera, when)
}

pub fn body(ev: &Event, when: &str) -> String {
    let secs = (ev.end - ev.start).max(0);
    let mut s = format!("Camera: {}\nTime: {}\nDuration: {} s\nMotion score: {:.2}\n", ev.camera, when, secs, ev.score);
    if !ev.objects.is_empty() {
        s.push_str(&format!("Objects: {}\n", ev.objects.join(", ")));
    }
    s.push_str(&format!("Event: {}\n", ev.id));
    s
}

pub fn local_time(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "?".into())
}

/// Host, sender and at least one recipient are configured.
pub fn enabled(a: &AlertConfig) -> bool {
    a.smtp_host.as_deref().is_some_and(|h| !h.is_empty())
        && a.smtp_from.as_deref().is_some_and(|h| !h.is_empty())
        && a.email_to.iter().any(|t| !t.trim().is_empty())
}

pub fn build_message(a: &AlertConfig, ev: &Event, jpeg: Option<Vec<u8>>) -> Result<Message> {
    let when = local_time(ev.start);
    let from: Mailbox = a.smtp_from.as_deref().unwrap_or("").parse().context("invalid smtp_from")?;
    let mut b = Message::builder().from(from).subject(subject(ev, &when));
    for t in a.email_to.iter().filter(|t| !t.trim().is_empty()) {
        b = b.to(t.trim().parse().with_context(|| format!("invalid email_to address '{t}'"))?);
    }
    let text = SinglePart::plain(body(ev, &when));
    let msg = match jpeg {
        Some(bytes) => b.multipart(
            MultiPart::mixed().singlepart(text).singlepart(
                Attachment::new_inline("snapshot.jpg".to_string()).body(bytes, ContentType::parse("image/jpeg")?),
            ),
        ),
        None => b.singlepart(text),
    };
    msg.map_err(|e| anyhow!("could not build email: {e}"))
}

pub fn transport(a: &AlertConfig) -> Result<SmtpTransport> {
    let host = a.smtp_host.as_deref().unwrap_or("");
    let port = if a.smtp_port == 0 { 587 } else { a.smtp_port };
    let mode = if a.smtp_tls.is_empty() { "starttls" } else { a.smtp_tls.as_str() };
    let builder = match mode {
        "starttls" => SmtpTransport::starttls_relay(host)?,
        "tls" => SmtpTransport::relay(host)?,
        "none" => SmtpTransport::builder_dangerous(host),
        other => return Err(anyhow!("smtp_tls must be starttls, tls or none (got '{other}')")),
    };
    let mut builder = builder.port(port).timeout(Some(Duration::from_secs(20)));
    if let Some(u) = a.smtp_user.as_deref().filter(|u| !u.is_empty()) {
        builder = builder.credentials(Credentials::new(u.into(), a.smtp_password.clone().unwrap_or_default()));
    }
    Ok(builder.build())
}

/// Blocking send; call from a worker thread.
pub fn send(a: &AlertConfig, ev: &Event, jpeg: Option<Vec<u8>>) -> Result<()> {
    let msg = build_message(a, ev, jpeg)?;
    transport(a)?.send(&msg).map(|_| ()).map_err(|e| anyhow!("smtp send failed: {e}"))
}

/// A plain-text message (system alerts). Blocking; call from a worker thread.
pub fn send_text(a: &AlertConfig, subject: &str, body: &str) -> Result<()> {
    let from: Mailbox = a.smtp_from.as_deref().unwrap_or("").parse().context("invalid smtp_from")?;
    let mut b = Message::builder().from(from).subject(subject);
    for t in a.email_to.iter().filter(|t| !t.trim().is_empty()) {
        b = b.to(t.trim().parse().with_context(|| format!("invalid email_to address '{t}'"))?);
    }
    let msg = b.singlepart(SinglePart::plain(body.to_string())).map_err(|e| anyhow!("could not build email: {e}"))?;
    transport(a)?.send(&msg).map(|_| ()).map_err(|e| anyhow!("smtp send failed: {e}"))
}

/// A message with an HTML part (and a plain one for clients without), the
/// pictures attached inline by content id. Blocking; call from a worker thread.
pub fn send_html(a: &AlertConfig, subject: &str, plain: &str, html: &str, images: Vec<(String, Vec<u8>)>) -> Result<()> {
    let msg = build_html(a, subject, plain, html, images)?;
    transport(a)?.send(&msg).map(|_| ()).map_err(|e| anyhow!("smtp send failed: {e}"))
}

pub fn build_html(a: &AlertConfig, subject: &str, plain: &str, html: &str, images: Vec<(String, Vec<u8>)>) -> Result<Message> {
    let from: Mailbox = a.smtp_from.as_deref().unwrap_or("").parse().context("invalid smtp_from")?;
    let mut b = Message::builder().from(from).subject(subject);
    for t in a.email_to.iter().filter(|t| !t.trim().is_empty()) {
        b = b.to(t.trim().parse().with_context(|| format!("invalid email_to address '{t}'"))?);
    }
    let mut related = MultiPart::related().singlepart(SinglePart::html(html.to_string()));
    for (cid, bytes) in images {
        related = related.singlepart(Attachment::new_inline(cid).body(bytes, ContentType::parse("image/jpeg")?));
    }
    let msg = b
        .multipart(MultiPart::alternative().singlepart(SinglePart::plain(plain.to_string())).multipart(related))
        .map_err(|e| anyhow!("could not build email: {e}"))?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AlertConfig {
        AlertConfig {
            smtp_host: Some("smtp.example.com".into()),
            smtp_from: Some("AmberSight <cam@example.com>".into()),
            email_to: vec!["me@example.com".into(), "you@example.com".into()],
            ..Default::default()
        }
    }

    fn ev(objects: &[&str]) -> Event {
        Event {
            id: "e1".into(),
            camera_id: 0,
            camera: "Porch".into(),
            start: 1_700_000_000,
            end: 1_700_000_012,
            score: 0.42,
            clip: None,
            thumb: None,
            objects: objects.iter().map(|s| s.to_string()).collect(),
            protect: false,
        }
    }

    #[test]
    fn message_has_subject_and_inline_jpeg() {
        let m = build_message(&cfg(), &ev(&["person", "dog"]), Some(vec![0xff, 0xd8, 0xff, 0xd9])).unwrap();
        let raw = String::from_utf8_lossy(&m.formatted()).into_owned();
        assert!(raw.contains("Subject: AmberSight: person, dog on Porch at "), "{raw}");
        assert!(raw.contains("image/jpeg"));
        assert!(raw.contains("Content-Disposition: inline"));
        assert!(raw.contains("snapshot.jpg"));
        assert!(raw.contains("me@example.com") && raw.contains("you@example.com"));
        assert!(raw.contains("Duration: 12 s"));
    }

    #[test]
    fn motion_subject_without_objects_or_image() {
        let m = build_message(&cfg(), &ev(&[]), None).unwrap();
        let raw = String::from_utf8_lossy(&m.formatted()).into_owned();
        assert!(raw.contains("Subject: AmberSight: motion on Porch"));
        assert!(!raw.contains("image/jpeg"));
    }

    #[test]
    fn html_summary_has_both_parts_and_inline_pictures() {
        let m = build_html(&cfg(), "AmberSight: Morning summary", "plain text", "<p>hello <img src=\"cid:t0@x\"></p>", vec![("t0@x".into(), vec![0xff, 0xd8, 0xff, 0xd9])]).unwrap();
        let raw = String::from_utf8_lossy(&m.formatted()).into_owned();
        assert!(raw.contains("multipart/alternative"), "{raw}");
        assert!(raw.contains("text/plain") && raw.contains("text/html"));
        assert!(raw.contains("multipart/related"));
        assert!(raw.contains("Content-ID: <t0@x>"), "{raw}");
        assert!(raw.contains("image/jpeg"));
    }

    #[test]
    fn enabled_and_bad_inputs() {
        assert!(enabled(&cfg()));
        assert!(!enabled(&AlertConfig::default()));
        let mut c = cfg();
        c.email_to = vec!["not an address".into()];
        assert!(build_message(&c, &ev(&[]), None).is_err());
        let mut c = cfg();
        c.smtp_tls = "bogus".into();
        assert!(transport(&c).is_err());
    }
}
