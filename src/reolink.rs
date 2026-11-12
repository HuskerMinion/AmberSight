//! Reolink HTTP API (as documented by Reolink) for per-channel motion / AI
//! state and battery-camera sleep state. Needed for hubs and recorders:
//! their ONVIF events do not say which channel they belong to, while
//! `GetMdState` / `GetAiState` / `GetChannelstatus` are per channel.
//!
//! Devices use self-signed certificates on the LAN, so certificate checks
//! are off for this client (it only ever talks to the configured camera).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// What one channel reports right now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelState {
    pub motion: bool,
    pub people: bool,
    pub vehicle: bool,
    pub animal: bool,
    /// Battery camera asleep (None when the device does not say).
    pub sleeping: Option<bool>,
    pub online: Option<bool>,
    pub name: Option<String>,
}

pub struct ReolinkClient {
    base: String,
    user: String,
    pass: String,
    token: Option<(String, Instant)>,
    agent: ureq::Agent,
}

/// Tokens last an hour; renew a little early.
const TOKEN_LIFE: Duration = Duration::from_secs(50 * 60);

impl ReolinkClient {
    /// `host` is an IP or name; HTTPS on port 443 is used (as Reolink hubs
    /// and cameras serve the API).
    pub fn new(host: &str, user: &str, pass: &str) -> Self {
        let tls = ureq::tls::TlsConfig::builder().disable_verification(true).build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_global(Some(Duration::from_secs(8)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        ReolinkClient { base: format!("https://{host}"), user: user.into(), pass: pass.into(), token: None, agent }
    }

    /// A client for a full base address ("https://host:port"), e.g. when
    /// the API is on another port or on plain HTTP.
    pub fn with_base(base: &str, user: &str, pass: &str) -> Self {
        let mut c = ReolinkClient::new("x", user, pass);
        c.base = base.trim_end_matches('/').to_string();
        c
    }

    /// Restart the camera (`Reboot`); re-signs-in once if the token expired.
    pub fn reboot(&mut self) -> Result<()> {
        for attempt in 0..2 {
            let t = self.login()?;
            let v = self.post("Reboot", Some(&t), &json!([{ "cmd": "Reboot", "action": 0, "param": {} }]))?;
            let r = &v[0];
            if r["code"].as_i64() == Some(0) {
                return Ok(());
            }
            if attempt == 0 && r["error"]["rspCode"].as_i64() == Some(-6) {
                self.token = None;
                continue;
            }
            bail!("the camera refused to restart: {}", error_text(r));
        }
        bail!("the camera keeps rejecting the sign-in token")
    }

    fn post(&self, cmd: &str, token: Option<&str>, body: &Value) -> Result<Value> {
        let mut url = format!("{}/cgi-bin/api.cgi?cmd={cmd}", self.base);
        if let Some(t) = token {
            url.push_str("&token=");
            url.push_str(t);
        }
        let mut resp = self.agent.post(&url).send_json(body).map_err(|e| anyhow!("{cmd}: {e}"))?;
        let v: Value = resp.body_mut().read_json().map_err(|e| anyhow!("{cmd}: unexpected answer: {e}"))?;
        Ok(v)
    }

    fn login(&mut self) -> Result<String> {
        if let Some((t, at)) = &self.token {
            if at.elapsed() < TOKEN_LIFE {
                return Ok(t.clone());
            }
        }
        let body = json!([{ "cmd": "Login", "param": { "User": { "Version": "0", "userName": self.user, "password": self.pass } } }]);
        let v = self.post("Login", None, &body)?;
        let t = v[0]["value"]["Token"]["name"]
            .as_str()
            .ok_or_else(|| anyhow!("sign-in to the camera failed: {}", error_text(&v[0])))?
            .to_string();
        self.token = Some((t.clone(), Instant::now()));
        Ok(t)
    }

    /// Run several commands in one request; re-signs-in once if the token
    /// expired.
    pub fn batch(&mut self, cmds: &Value) -> Result<Vec<Value>> {
        for attempt in 0..2 {
            let t = self.login()?;
            let v = self.post("GetMdState", Some(&t), cmds)?;
            let arr = v.as_array().cloned().context("expected a list of answers")?;
            // -6 = "please login first" (token expired or evicted).
            if attempt == 0 && arr.iter().any(|r| r["error"]["rspCode"].as_i64() == Some(-6)) {
                self.token = None;
                continue;
            }
            return Ok(arr);
        }
        bail!("the camera keeps rejecting the sign-in token")
    }

    /// Motion / AI / sleep state of one channel (0-based).
    pub fn channel_state(&mut self, channel: u32) -> Result<ChannelState> {
        let cmds = json!([
            { "cmd": "GetMdState", "action": 0, "param": { "channel": channel } },
            { "cmd": "GetAiState", "action": 0, "param": { "channel": channel } },
            { "cmd": "GetChannelstatus", "action": 0, "param": {} }
        ]);
        let r = self.batch(&cmds)?;
        Ok(parse_state(&r, channel))
    }

    /// Best-effort sign-out (devices allow only a few sessions).
    pub fn logout(&mut self) {
        if let Some((t, _)) = self.token.take() {
            let _ = self.post("Logout", Some(&t), &json!([{ "cmd": "Logout", "param": {} }]));
        }
    }
}

fn error_text(r: &Value) -> String {
    r["error"]["detail"].as_str().map(|s| s.to_string()).unwrap_or_else(|| r.to_string())
}

/// Pull the channel's state out of a batch answer.
pub fn parse_state(answers: &[Value], channel: u32) -> ChannelState {
    let mut st = ChannelState::default();
    for a in answers {
        let v = &a["value"];
        match a["cmd"].as_str().unwrap_or("") {
            "GetMdState" => st.motion = v["state"].as_i64() == Some(1),
            "GetAiState" => {
                let on = |k: &str| v[k]["alarm_state"].as_i64() == Some(1);
                st.people = on("people") || on("face");
                st.vehicle = on("vehicle");
                st.animal = on("dog_cat");
            }
            "GetChannelstatus" => {
                if let Some(ch) = v["status"].as_array().and_then(|l| l.iter().find(|c| c["channel"].as_u64() == Some(channel as u64))) {
                    st.sleeping = ch["sleep"].as_i64().map(|s| s == 1);
                    st.online = ch["online"].as_i64().map(|s| s == 1);
                    st.name = ch["name"].as_str().filter(|n| !n.is_empty()).map(|n| n.to_string());
                }
            }
            _ => {}
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hub_answers_per_channel() {
        // Shapes as returned by the Reolink Home Hub on the LAN.
        let answers: Vec<Value> = serde_json::from_value(json!([
            { "cmd": "GetMdState", "code": 0, "value": { "state": 1 } },
            { "cmd": "GetAiState", "code": 0, "value": { "channel": 0,
                "dog_cat": { "alarm_state": 0, "support": 1 },
                "face": { "alarm_state": 0, "support": 0 },
                "people": { "alarm_state": 1, "support": 1 },
                "vehicle": { "alarm_state": 0, "support": 1 } } },
            { "cmd": "GetChannelstatus", "code": 0, "value": { "count": 8, "status": [
                { "channel": 0, "name": "Garage Side", "online": 1, "sleep": 1, "uid": "x" },
                { "channel": 1, "name": "Back Porch", "online": 1, "sleep": 0, "uid": "y" } ] } }
        ]))
        .unwrap();
        let s = parse_state(&answers, 0);
        assert!(s.motion && s.people && !s.vehicle && !s.animal);
        assert_eq!(s.sleeping, Some(true));
        assert_eq!(s.name.as_deref(), Some("Garage Side"));
        let s1 = parse_state(&answers, 1);
        assert_eq!(s1.sleeping, Some(false));
        assert_eq!(s1.name.as_deref(), Some("Back Porch"));
    }
}
