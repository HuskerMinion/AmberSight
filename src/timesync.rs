//! Turn on a camera's own time sync (NTP) over ONVIF, so the health page's
//! "clock is fast/slow" warning has a one-click fix.
//!
//! Steps: read the camera's date/time settings (keeping its time zone and
//! daylight-saving choice), make sure it has an NTP server (keeping one it
//! already has), set its clock to this computer's time right away, then switch
//! it to NTP. The camera's clock offset is measured before and after.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
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
    c.call(url, &format!("{NS_DEVICE}/{op}"), body, true, TIMEOUT).with_context(|| op.to_string())
}

/// Turn on NTP for one camera. `server` is used only when the camera has none.
pub fn enable(cfg: &CameraConfig, server: &str) -> Result<serde_json::Value> {
    // The configured ONVIF port, else the usual ones.
    let ports: Vec<u16> = match cfg.onvif_port {
        Some(p) => vec![p],
        None => vec![80, 8000],
    };
    let stop = AtomicBool::new(false);
    let mut last_err = anyhow!("no ONVIF port to try");
    for port in ports {
        let creds = creds_for(cfg, port)?;
        let url = format!("http://{}:{}/onvif/device_service", creds.host, port);
        let mut c = Client::new(&creds, &stop);
        let xml = match device_call(&mut c, &url, "GetSystemDateAndTime", "<tds:GetSystemDateAndTime/>") {
            Ok(x) => x,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        let before = crate::onvif::parse_system_time(&xml).map(|d| (d - Utc::now()).num_seconds());
        let settings = parse_settings(&xml).unwrap_or_default();
        c.sync_clock();

        let had_server = device_call(&mut c, &url, "GetNTP", "<tds:GetNTP/>").map(|x| ntp_has_server(&x)).unwrap_or(false);
        let mut server_set = None;
        if !had_server {
            device_call(&mut c, &url, "SetNTP", &body_set_ntp(server)).context("could not give the camera an NTP server")?;
            server_set = Some(server.to_string());
        }
        // Right the clock now (NTP may take a while), then hand it to NTP.
        let set_now = device_call(&mut c, &url, "SetSystemDateAndTime", &body_set_time(&settings, Some(Utc::now()))).is_ok();
        device_call(&mut c, &url, "SetSystemDateAndTime", &body_set_time(&settings, None))
            .context("the camera refused to switch to NTP")?;

        std::thread::sleep(Duration::from_secs(1));
        let offset = |c: &mut Client| {
            device_call(c, &url, "GetSystemDateAndTime", "<tds:GetSystemDateAndTime/>")
                .ok()
                .and_then(|x| crate::onvif::parse_system_time(&x))
                .map(|d| (d - Utc::now()).num_seconds())
        };
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
