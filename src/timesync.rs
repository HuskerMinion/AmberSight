//! Turn on a camera's own time sync (NTP) over ONVIF, so the health page's
//! "clock is fast/slow" warning has a one-click fix.
//!
//! Steps: read the camera's date/time settings (keeping its time zone and
//! daylight-saving choice), make sure it has an NTP server (keeping one it
//! already has), set its clock to this computer's time right away, then switch
//! it to NTP. The camera's clock offset is measured before and after.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::{Datelike, Timelike, Utc};

use crate::config::CameraConfig;
use crate::onvif::{find_desc, find_text, xml_escape, Client, Credentials};

const NS_DEVICE: &str = "http://www.onvif.org/ver10/device/wsdl";
const NS_TT: &str = "http://www.onvif.org/ver10/schema";
const TIMEOUT: Duration = Duration::from_secs(8);
/// Used when the camera has no NTP server of its own.
pub const DEFAULT_NTP: &str = "pool.ntp.org";
/// Still this far off (s) after turning on time sync: the camera's own time
/// server is not working.
const OFF_AFTER_SYNC: i64 = 10;

#[derive(Debug, Default, PartialEq)]
pub struct DateTimeSettings {
    pub ntp: bool,
    pub daylight_savings: bool,
    pub tz: Option<String>,
}

/// DateTimeType / DaylightSavings / TimeZone from a GetSystemDateAndTimeResponse.
pub fn parse_settings(xml: &str) -> Option<DateTimeSettings> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let sdt = find_desc(&doc, "SystemDateAndTime")?;
    let tz = sdt
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "TimeZone")
        .and_then(|n| find_text(n, "TZ"))
        .filter(|s| !s.is_empty());
    Some(DateTimeSettings {
        ntp: find_text(sdt, "DateTimeType").is_some_and(|t| t.eq_ignore_ascii_case("NTP")),
        daylight_savings: find_text(sdt, "DaylightSavings").is_some_and(|t| t == "true"),
        tz,
    })
}

/// Whether a GetNTPResponse lists any server (manual or from DHCP).
pub fn ntp_has_server(xml: &str) -> bool {
    let Ok(doc) = roxmltree::Document::parse(xml) else { return false };
    doc.descendants().filter(|n| n.is_element() && matches!(n.tag_name().name(), "NTPManual" | "NTPFromDHCP")).any(|n| {
        ["DNSname", "IPv4Address", "IPv6Address"].iter().any(|f| find_text(n, f).is_some_and(|v| !v.is_empty()))
    })
}

fn body_set_ntp(server: &str) -> String {
    let kind = if server.parse::<std::net::Ipv4Addr>().is_ok() { ("IPv4", "IPv4Address") } else { ("DNS", "DNSname") };
    format!(
        "<tds:SetNTP><tds:FromDHCP>false</tds:FromDHCP><tds:NTPManual>\
<tt:Type xmlns:tt=\"{NS_TT}\">{}</tt:Type><tt:{} xmlns:tt=\"{NS_TT}\">{}</tt:{}>\
</tds:NTPManual></tds:SetNTP>",
        kind.0,
        kind.1,
        xml_escape(server),
        kind.1
    )
}

fn body_set_time(s: &DateTimeSettings, manual_now: Option<chrono::DateTime<Utc>>) -> String {
    let tz = s
        .tz
        .as_ref()
        .map(|t| format!("<tds:TimeZone><tt:TZ xmlns:tt=\"{NS_TT}\">{}</tt:TZ></tds:TimeZone>", xml_escape(t)))
        .unwrap_or_default();
    let (kind, utc) = match manual_now {
        Some(n) => (
            "Manual",
            format!(
                "<tds:UTCDateTime><tt:Time xmlns:tt=\"{NS_TT}\"><tt:Hour>{}</tt:Hour><tt:Minute>{}</tt:Minute><tt:Second>{}</tt:Second></tt:Time>\
<tt:Date xmlns:tt=\"{NS_TT}\"><tt:Year>{}</tt:Year><tt:Month>{}</tt:Month><tt:Day>{}</tt:Day></tt:Date></tds:UTCDateTime>",
                n.hour(),
                n.minute(),
                n.second(),
                n.year(),
                n.month(),
                n.day()
            ),
        ),
        None => ("NTP", String::new()),
    };
    format!(
        "<tds:SetSystemDateAndTime><tds:DateTimeType>{kind}</tds:DateTimeType>\
<tds:DaylightSavings>{}</tds:DaylightSavings>{tz}{utc}</tds:SetSystemDateAndTime>",
        s.daylight_savings
    )
}

fn creds_for(cfg: &CameraConfig, port: u16) -> Result<Credentials> {
    let host = crate::onvif::host_from_rtsp_url(&cfg.url).ok_or_else(|| anyhow!("camera url has no host"))?;
    let (mut username, mut password) = (cfg.username.clone(), cfg.password.clone());
    if username.is_empty() {
        if let Some((u, p)) = crate::onvif::credentials_from_url(&cfg.url) {
            username = u;
            password = p;
        }
    }
    Ok(Credentials { host, port, username, password })
}

fn device_call(c: &mut Client, url: &str, op: &str, body: &str) -> Result<String> {
    c.call(url, &format!("{NS_DEVICE}/{op}"), body, true, TIMEOUT).map_err(|e| not_supported(e, op))
}

/// What each step does, for the "doesn't let AmberSight ..." message.
fn step_what(op: &str) -> &'static str {
    match op {
        "SetNTP" => "set its time server",
        "SetSystemDateAndTime" => "set its clock",
        "GetSystemDateAndTime" => "read its clock",
        "GetNTP" => "read its time server",
        _ => "change its time settings",
    }
}

/// A step the camera answered with an ONVIF NotImplemented /
/// ActionNotSupported fault (an optional method it leaves out). Its text is
/// shown as it is: no SOAP, no context added (see `plain`).
#[derive(Debug)]
struct Unsupported(String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

/// A failed step: a plain `Unsupported` when the camera does not have that
/// method, else the error named after the step.
fn not_supported(e: anyhow::Error, op: &str) -> anyhow::Error {
    let text = format!("{e:#}").to_ascii_lowercase();
    if text.contains("soap fault") && ["notimplemented", "actionnotsupported", "not implemented"].iter().any(|k| text.contains(k)) {
        return anyhow!(Unsupported(format!(
            "This camera doesn't let AmberSight {} (it doesn't support {op} over ONVIF): turn on time sync in the camera's own app or web page.",
            step_what(op)
        )));
    }
    e.context(op.to_string())
}

/// `e` with `what` added, unless a step was one the camera does not
/// support: then only that plain message.
fn plain(e: anyhow::Error, what: &'static str) -> anyhow::Error {
    match e.chain().find_map(|c| c.downcast_ref::<Unsupported>()) {
        Some(u) => anyhow!(u.0.clone()),
        None => e.context(what),
    }
}

/// Read the camera's clock: the GetSystemDateAndTime answer and how far its
/// UTC time is from this computer's (seconds; None: it gave no UTC time).
/// An answer that is not SOAP (a web page) is an error: not ONVIF there.
fn read_clock(c: &mut Client, url: &str) -> Result<(String, Option<i64>)> {
    let xml = device_call(c, url, "GetSystemDateAndTime", "<tds:GetSystemDateAndTime/>")?;
    if !roxmltree::Document::parse(&xml).is_ok_and(|d| d.root_element().tag_name().name() == "Envelope") {
        return Err(anyhow!("the answer on port {} is not ONVIF", c_port(url)));
    }
    let off = crate::onvif::parse_system_time(&xml).map(|d| (d - Utc::now()).num_seconds());
    Ok((xml, off))
}

/// The ONVIF ports to try, in order: the configured one, else the usual ones
/// (80, then 8000).
fn onvif_ports(cfg: &CameraConfig) -> Vec<u16> {
    match cfg.onvif_port {
        Some(p) => vec![p],
        None => vec![80, 8000],
    }
}

/// How far the camera's UTC clock is from this computer's (seconds), read
/// over ONVIF (its configured port, else 80 then 8000, as `enable` tries).
/// Err: ONVIF answered on no port, the one where it did refused, or it gave
/// no UTC time.
pub fn utc_offset(cfg: &CameraConfig) -> Result<i64> {
    utc_offset_on(cfg, &onvif_ports(cfg))
}

/// `utc_offset` over these ports, the next one tried as `enable` does (see
/// `onvif_answered`).
fn utc_offset_on(cfg: &CameraConfig, ports: &[u16]) -> Result<i64> {
    let stop = AtomicBool::new(false);
    let mut last_err = anyhow!("no ONVIF port to try");
    for &port in ports {
        let creds = creds_for(cfg, port)?;
        let url = format!("http://{}:{}/onvif/device_service", creds.host, port);
        let mut c = Client::new(&creds, &stop);
        match read_clock(&mut c, &url) {
            Ok((_, off)) => return off.ok_or_else(|| anyhow!("the camera gave no UTC time")),
            Err(e) if onvif_answered(&e) => return Err(e),
            Err(e) => keep_err(&mut last_err, e),
        }
    }
    Err(last_err)
}

/// The port of an ONVIF address (for the words).
fn c_port(url: &str) -> u16 {
    url::Url::parse(url).ok().and_then(|u| u.port_or_known_default()).unwrap_or(80)
}

/// Whether ONVIF answered on that port (a SOAP fault, a sign-in refused with
/// a SOAP answer, or a method the camera lacks): then it is the camera's
/// answer and no other port is tried. Anything else (nothing listening, no
/// answer, an HTTP error, a web page, or a bare 401 from a web interface with
/// a password) moves on to the next port.
pub(crate) fn onvif_answered(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let t = c.to_string();
        c.is::<Unsupported>() || t.starts_with("SOAP fault") || (t.starts_with("HTTP 401") && t.contains(crate::onvif::SOAP_401))
    })
}

/// Keep a port's error for the words, unless an earlier port's said the
/// sign-in was refused (a bare 401 moves on to the next port, but "check the
/// username and password" is the more useful thing to say if none answers).
fn keep_err(last: &mut anyhow::Error, e: anyhow::Error) {
    let refused = |x: &anyhow::Error| x.chain().any(|c| c.to_string().starts_with("HTTP 401"));
    if !refused(last) || refused(&e) {
        *last = e;
    }
}

/// Within this (s) of a time-zone step, a stream's skew counts as that step;
/// and a camera's UTC clock this close counts as right.
pub const ZONE_TOLERANCE: f64 = 30.0;

/// The time-zone step (seconds: a non-zero multiple of 15 minutes, up to 14
/// hours) a stream's clock skew is, if it is one. Some firmware (an Imou, for
/// one, issue #15) puts its local time in the RTCP sender reports instead of
/// UTC, so the skew is the camera's time zone although its clock is right.
pub fn zone_step(skew: f64) -> Option<f64> {
    let q = (skew / 900.0).round();
    (q != 0.0 && q.abs() <= 56.0 && (skew - q * 900.0).abs() <= ZONE_TOLERANCE).then_some(q * 900.0)
}

/// Whether the camera's clock is to be read over ONVIF now, for a stream skew
/// of `skew`: it is a zone step, and it was not read in the last few hours or
/// the skew moved notably since a reading that worked. `last`: when it was
/// last tried (epoch s), the skew then, and whether the clock was read (a
/// camera without ONVIF, or a reading still going, waits the hours out).
pub fn zone_check_due(skew: f64, last: Option<(i64, f64, bool)>, now: i64) -> bool {
    const AGAIN_AFTER: i64 = 4 * 3600;
    zone_step(skew).is_some()
        && last.is_none_or(|(at, was, read)| now - at >= AGAIN_AFTER || (read && (skew - was).abs() > 2.0 * ZONE_TOLERANCE))
}

/// What an ONVIF reading of the camera's UTC clock (`onvif_offset`, None: not
/// read) says about a stream skew `skew`: the zone step to accept, only when
/// the skew is one and the camera's UTC clock is right.
pub fn confirmed_zone(skew: f64, onvif_offset: Option<i64>) -> Option<f64> {
    if onvif_offset.is_some_and(|o| (o as f64).abs() <= ZONE_TOLERANCE) {
        zone_step(skew)
    } else {
        None
    }
}

/// The confirmed zone step, while the stream's skew still is it (for Status
/// and the camera report).
pub fn zone_now(skew: Option<f64>, zone: Option<f64>) -> Option<f64> {
    skew.zip(zone).filter(|&(s, z)| (s - z).abs() <= ZONE_TOLERANCE).map(|(_, z)| z)
}

/// Whether a stream skew is a clock to warn about, given the zone step an
/// ONVIF reading confirmed (if any): over 30 s off, and not that zone step.
pub fn clock_warns(skew: f64, zone: Option<f64>) -> bool {
    skew.abs() > ZONE_TOLERANCE && !zone.is_some_and(|z| (skew - z).abs() <= ZONE_TOLERANCE)
}

/// Turn on NTP for one camera. `server` is used only when the camera has none.
pub fn enable(cfg: &CameraConfig, server: &str) -> Result<serde_json::Value> {
    let ports = onvif_ports(cfg);
    let stop = AtomicBool::new(false);
    let mut last_err = anyhow!("no ONVIF port to try");
    for port in ports {
        let creds = creds_for(cfg, port)?;
        let url = format!("http://{}:{}/onvif/device_service", creds.host, port);
        let mut c = Client::new(&creds, &stop);
        let (xml, before) = match read_clock(&mut c, &url) {
            Ok(x) => x,
            Err(e) if e.downcast_ref::<Unsupported>().is_some() => return Err(e),
            Err(e) if onvif_answered(&e) => return Err(e.context("the camera's ONVIF service did not let AmberSight read its clock")),
            Err(e) => {
                keep_err(&mut last_err, e);
                continue;
            }
        };
        let settings = parse_settings(&xml).unwrap_or_default();
        c.sync_clock();

        let had_server = device_call(&mut c, &url, "GetNTP", "<tds:GetNTP/>").map(|x| ntp_has_server(&x)).unwrap_or(false);
        let mut server_set = None;
        if !had_server {
            device_call(&mut c, &url, "SetNTP", &body_set_ntp(server)).map_err(|e| plain(e, "could not give the camera an NTP server"))?;
            server_set = Some(server.to_string());
        }
        // Right the clock now (NTP may take a while), then hand it to NTP.
        let set_now = device_call(&mut c, &url, "SetSystemDateAndTime", &body_set_time(&settings, Some(Utc::now()))).is_ok();
        device_call(&mut c, &url, "SetSystemDateAndTime", &body_set_time(&settings, None))
            .map_err(|e| plain(e, "the camera refused to switch to NTP"))?;

        std::thread::sleep(Duration::from_secs(1));
        let offset = |c: &mut Client| read_clock(c, &url).ok().and_then(|x| x.1);
        let mut after = offset(&mut c);
        let mut kept_manual = false;
        // Still well off: the camera's own time server is not working (it
        // put the clock straight back). Give it ours, set the clock again;
        // if even that does not hold, leave the clock set by hand (right now,
        // and the next sync rights it again).
        if after.is_some_and(|a| a.abs() > OFF_AFTER_SYNC) && server_set.is_none() {
            if device_call(&mut c, &url, "SetNTP", &body_set_ntp(server)).is_ok() {
                server_set = Some(server.to_string());
            }
            let _ = device_call(&mut c, &url, "SetSystemDateAndTime", &body_set_time(&settings, Some(Utc::now())));
            let _ = device_call(&mut c, &url, "SetSystemDateAndTime", &body_set_time(&settings, None));
            std::thread::sleep(Duration::from_secs(1));
            after = offset(&mut c);
            if after.is_some_and(|a| a.abs() > OFF_AFTER_SYNC)
                && device_call(&mut c, &url, "SetSystemDateAndTime", &body_set_time(&settings, Some(Utc::now()))).is_ok()
            {
                kept_manual = true;
                std::thread::sleep(Duration::from_secs(1));
                after = offset(&mut c);
            }
        }
        return Ok(serde_json::json!({
            "ok": true,
            "was_ntp": settings.ntp,
            "ntp_server_set": server_set,
            "clock_set_now": set_now,
            "kept_manual": kept_manual,
            "offset_before_s": before,
            "offset_after_s": after,
        }));
    }
    Err(last_err.context("could not reach the camera over ONVIF; turn on time sync (NTP) in the camera's own app or web page"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_settings_and_ntp() {
        let x = r#"<e:Envelope xmlns:e="x"><e:Body><tds:GetSystemDateAndTimeResponse xmlns:tds="a" xmlns:tt="b"><tds:SystemDateAndTime>
<tt:DateTimeType>Manual</tt:DateTimeType><tt:DaylightSavings>true</tt:DaylightSavings><tt:TimeZone><tt:TZ>CST6CDT,M3.2.0,M11.1.0</tt:TZ></tt:TimeZone>
</tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse></e:Body></e:Envelope>"#;
        assert_eq!(
            parse_settings(x),
            Some(DateTimeSettings { ntp: false, daylight_savings: true, tz: Some("CST6CDT,M3.2.0,M11.1.0".into()) })
        );
        let none = r#"<r xmlns:tt="b"><tt:NTPInformation><tt:FromDHCP>false</tt:FromDHCP></tt:NTPInformation></r>"#;
        assert!(!ntp_has_server(none));
        let some = r#"<r xmlns:tt="b"><tt:NTPInformation><tt:FromDHCP>false</tt:FromDHCP><tt:NTPManual><tt:Type>DNS</tt:Type><tt:DNSname>time.example</tt:DNSname></tt:NTPManual></tt:NTPInformation></r>"#;
        assert!(ntp_has_server(some));
    }

    #[test]
    fn zone_steps() {
        for s in [3600.0, -3600.0, 1800.0, 19800.0, 20700.0, 3612.5, -3590.0, 50400.0] {
            assert!(zone_step(s).is_some(), "{s}");
        }
        assert_eq!(zone_step(3612.5), Some(3600.0));
        assert_eq!(zone_step(20700.0), Some(20700.0));
        for s in [0.0, 12.0, 45.0, -45.0, 3000.0, 50000.0, 54000.0, 3650.0] {
            assert_eq!(zone_step(s), None, "{s}");
        }
    }

    #[test]
    fn zone_decision() {
        // Stream an hour fast (local time in the sender reports), the UTC clock right: no warning.
        let z = confirmed_zone(3601.0, Some(2));
        assert_eq!(z, Some(3600.0));
        assert!(!clock_warns(3601.0, z));
        assert!(!clock_warns(3580.0, z));
        // The UTC clock off too, or not read: the warning stays.
        assert_eq!(confirmed_zone(3601.0, Some(3600)), None);
        assert_eq!(confirmed_zone(3601.0, Some(-45)), None);
        assert_eq!(confirmed_zone(3601.0, None), None);
        assert!(clock_warns(3601.0, None));
        // Not a zone step: as before.
        assert_eq!(confirmed_zone(3000.0, Some(0)), None);
        assert!(clock_warns(45.0, None));
        assert!(!clock_warns(20.0, None));
        // The skew moved away from the confirmed step: a warning again.
        assert!(clock_warns(7200.0, Some(3600.0)));
        assert!(clock_warns(3700.0, Some(3600.0)));
        // When to read the clock: zone steps only; again after hours or a move.
        let now = 1_000_000;
        assert!(zone_check_due(3600.0, None, now));
        assert!(!zone_check_due(45.0, None, now));
        assert!(!zone_check_due(3605.0, Some((now - 60, 3600.0, true)), now));
        assert!(zone_check_due(3605.0, Some((now - 5 * 3600, 3600.0, true)), now));
        assert!(zone_check_due(7200.0, Some((now - 60, 3600.0, true)), now));
    }

    /// A fake ONVIF camera: its clock right (UTC now), no NTP server, and the
    /// operations in `missing` answered with the NotImplemented fault an Imou
    /// camera gives (HTTP 200, issue #15).
    fn fake_camera(missing: &'static [&'static str]) -> CameraConfig {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut s) = conn else { break };
                let (_, body) = crate::test_ports::read_http_request(&mut s);
                let req = String::from_utf8_lossy(&body).into_owned();
                let op = ["GetSystemDateAndTime", "SetSystemDateAndTime", "GetNTP", "SetNTP"].into_iter().find(|o| req.contains(&format!("tds:{o}"))).unwrap_or("");
                let inner = if missing.contains(&op) {
                    "<s:Fault><s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>ter:ActionNotSupported</s:Value><s:Subcode><s:Value>ter:NotImplemented</s:Value></s:Subcode></s:Subcode></s:Code><s:Reason><s:Text xml:lang=\"en\">This optional method is not implemented</s:Text></s:Reason></s:Fault>".to_string()
                } else if op == "GetSystemDateAndTime" {
                    let n = Utc::now();
                    format!(
                        "<tds:GetSystemDateAndTimeResponse xmlns:tds=\"{NS_DEVICE}\" xmlns:tt=\"{NS_TT}\"><tds:SystemDateAndTime><tt:DateTimeType>Manual</tt:DateTimeType><tt:DaylightSavings>false</tt:DaylightSavings><tt:TimeZone><tt:TZ>GMT+01:00</tt:TZ></tt:TimeZone>\
<tt:UTCDateTime><tt:Time><tt:Hour>{}</tt:Hour><tt:Minute>{}</tt:Minute><tt:Second>{}</tt:Second></tt:Time><tt:Date><tt:Year>{}</tt:Year><tt:Month>{}</tt:Month><tt:Day>{}</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>",
                        n.hour(), n.minute(), n.second(), n.year(), n.month(), n.day()
                    )
                } else {
                    format!("<tds:{op}Response xmlns:tds=\"{NS_DEVICE}\"/>")
                };
                let reply = format!("<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:ter=\"http://www.onvif.org/ver10/error\"><s:Body>{inner}</s:Body></s:Envelope>");
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        });
        let mut cfg: CameraConfig = toml::from_str("name = \"Driveway\"\nurl = \"rtsp://127.0.0.1:554/cam\"\nusername = \"u\"\npassword = \"p\"").unwrap();
        cfg.onvif_port = Some(port);
        cfg
    }

    #[test]
    fn reads_the_cameras_utc_clock() {
        let off = utc_offset(&fake_camera(&[])).unwrap();
        assert!(off.abs() <= 2, "{off}");
        assert_eq!(confirmed_zone(3600.0, Some(off)), Some(3600.0));
    }

    /// A web server that answers every request with `status` and `page`.
    fn web_server(status: &'static str, page: &'static str) -> u16 {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut s) = conn else { break };
                let _ = crate::test_ports::read_http_request(&mut s);
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
            }
        });
        port
    }

    /// A port nothing listens on.
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[test]
    fn with_no_onvif_port_the_usual_ports_are_tried_in_turn() {
        let mut cfg = fake_camera(&[]);
        assert_eq!(onvif_ports(&cfg), vec![cfg.onvif_port.unwrap()]);
        let good = cfg.onvif_port.take().unwrap();
        assert_eq!(onvif_ports(&cfg), vec![80, 8000]);
        // Nothing on the first port: the next one is read.
        let off = utc_offset_on(&cfg, &[closed_port(), good]).unwrap();
        assert!(off.abs() <= 2, "{off}");
        // Nothing on any port: an error (the warning stays).
        assert!(utc_offset_on(&cfg, &[closed_port(), closed_port()]).is_err());
        // The first port answered ONVIF (with a refusal): no other port is tried.
        let refusing = fake_camera(&["GetSystemDateAndTime"]).onvif_port.unwrap();
        assert!(utc_offset_on(&cfg, &[refusing, good]).is_err());
        // A web server on the first port (404 with a page, a page with 200, or
        // a bare 401 from a web interface with a password): not ONVIF, so the
        // next port is read; enable() moves on the same way.
        for (status, page) in [("404 Not Found", "<html><body>Not Found</body></html>"), ("200 OK", "<!DOCTYPE html><html><body>Login</body></html>"), ("405 Method Not Allowed", ""), ("401 Unauthorized", "<html><body>Login required</body></html>")] {
            let web = web_server(status, page);
            let off = utc_offset_on(&cfg, &[web, good]).unwrap_or_else(|e| panic!("{status}: {e:#}"));
            assert!(off.abs() <= 2, "{off}");
            assert!(utc_offset_on(&cfg, &[web]).is_err(), "only a web page: not read");
        }
        // A camera not read waits the hours out, even when its skew moves.
        let now = 1_000_000;
        assert!(!zone_check_due(7200.0, Some((now - 60, 3600.0, false)), now));
        assert!(zone_check_due(7200.0, Some((now - 4 * 3600, 3600.0, false)), now));
    }

    #[test]
    fn a_refused_sign_in_is_what_is_said_when_no_port_answers() {
        let mut last = anyhow::anyhow!("no ONVIF port to try");
        keep_err(&mut last, anyhow::anyhow!("HTTP 401 unauthorized (check username/password)"));
        keep_err(&mut last, anyhow::anyhow!("connection refused"));
        assert!(last.to_string().starts_with("HTTP 401"), "{last}");
        let mut last = anyhow::anyhow!("no ONVIF port to try");
        keep_err(&mut last, anyhow::anyhow!("connection refused"));
        assert_eq!(last.to_string(), "connection refused");
    }

    #[test]
    fn only_an_onvif_answer_stops_the_port_search() {
        let soap401 = anyhow::anyhow!("HTTP 401 unauthorized (check username/password){}", crate::onvif::SOAP_401);
        assert!(onvif_answered(&soap401));
        assert!(!onvif_answered(&anyhow::anyhow!("HTTP 401 unauthorized (check username/password)")));
        assert!(onvif_answered(&anyhow::anyhow!("SOAP fault: something")));
        assert!(!onvif_answered(&anyhow::anyhow!("HTTP 404")));
    }

    #[test]
    fn a_step_the_camera_lacks_says_so_plainly() {
        let e = format!("{:#}", enable(&fake_camera(&["SetNTP"]), DEFAULT_NTP).unwrap_err());
        assert_eq!(
            e,
            "This camera doesn't let AmberSight set its time server (it doesn't support SetNTP over ONVIF): turn on time sync in the camera's own app or web page."
        );
        let e = format!("{:#}", enable(&fake_camera(&["SetSystemDateAndTime"]), DEFAULT_NTP).unwrap_err());
        assert!(e.starts_with("This camera doesn't let AmberSight set its clock (it doesn't support SetSystemDateAndTime"), "{e}");
        let e = format!("{:#}", enable(&fake_camera(&["GetSystemDateAndTime"]), DEFAULT_NTP).unwrap_err());
        assert!(e.starts_with("This camera doesn't let AmberSight read its clock"), "{e}");
        assert!(!e.contains("SOAP") && !e.contains("HTTP"), "{e}");
        // A camera with every step: turned on.
        assert!(enable(&fake_camera(&[]), DEFAULT_NTP).is_ok());
    }

    #[test]
    fn bodies_are_well_formed() {
        let s = DateTimeSettings { ntp: false, daylight_savings: true, tz: Some("UTC0<x>".into()) };
        for b in [body_set_ntp("pool.ntp.org"), body_set_ntp("192.168.1.1"), body_set_time(&s, Some(Utc::now())), body_set_time(&s, None)] {
            let doc = format!("<r xmlns:tds=\"{NS_DEVICE}\">{b}</r>");
            roxmltree::Document::parse(&doc).unwrap_or_else(|e| panic!("{e}: {b}"));
        }
        assert!(body_set_ntp("192.168.1.1").contains("IPv4Address"));
        assert!(body_set_time(&s, None).contains(">NTP<"));
    }
}
