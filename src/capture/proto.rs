//! Control-channel messages (docs/helper-protocol.md).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Something that can be captured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    Desktop,
    Monitor {
        index: u32,
    },
    Window {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exe: Option<String>,
    },
    Region {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
}

fn d_fps() -> u32 {
    10
}
fn d_kbps() -> u32 {
    2500
}
fn d_width() -> u32 {
    1920
}
fn d_thumb() -> u32 {
    320
}

/// A `start` message: what to capture and where to send it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct StartSpec {
    pub camera: String,
    pub key: String,
    pub source: Source,
    #[serde(default = "d_fps")]
    pub fps: u32,
    #[serde(default = "d_kbps")]
    pub max_kbps: u32,
    #[serde(default = "d_width")]
    pub max_width: u32,
}

impl StartSpec {
    /// Keep values ffmpeg can use.
    pub fn clamped(mut self) -> Self {
        self.fps = self.fps.clamp(1, 60);
        self.max_kbps = self.max_kbps.clamp(100, 50_000);
        self.max_width = self.max_width.clamp(160, 7680);
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerMsg {
    List {
        #[serde(default)]
        req: Value,
    },
    Thumb {
        #[serde(default)]
        req: Value,
        source: Source,
        #[serde(default = "d_thumb")]
        width: u32,
    },
    Start(StartSpec),
    Stop {
        camera: String,
    },
    Desired {
        cameras: Vec<String>,
    },
    Ping,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub primary: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WindowInfo {
    pub title: String,
    pub exe: String,
    pub w: u32,
    pub h: u32,
    /// The window's own id: X11 window id on Linux, HWND on Windows (not sent).
    #[serde(skip)]
    pub xid: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Sources {
    pub monitors: Vec<MonitorInfo>,
    pub windows: Vec<WindowInfo>,
}

/// Capture state reported in `status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Running,
    Stopped,
    Error,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Running => "running",
            State::Stopped => "stopped",
            State::Error => "error",
        }
    }
}

pub fn os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn hello(id: &str, machine: &str) -> Value {
    serde_json::json!({"type": "hello", "id": id, "version": version(), "machine": machine, "os": os_name()})
}

pub fn status(camera: &str, state: State, error: Option<&str>) -> Value {
    let mut v = serde_json::json!({"type": "status", "camera": camera, "state": state.as_str()});
    if let Some(e) = error {
        v["error"] = Value::String(e.to_string());
    }
    v
}

pub fn sources_reply(req: &Value, s: &Sources) -> Value {
    serde_json::json!({"type": "sources", "req": req, "monitors": s.monitors, "windows": s.windows})
}

pub fn thumb_reply(req: &Value, jpeg: Result<Vec<u8>, String>) -> Value {
    use base64::Engine as _;
    match jpeg {
        Ok(b) => serde_json::json!({"type": "thumb", "req": req, "jpeg": base64::engine::general_purpose::STANDARD.encode(b)}),
        Err(e) => serde_json::json!({"type": "thumb", "req": req, "error": e}),
    }
}

/// This machine's name.
pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| sysinfo::System::host_name().ok())
        .unwrap_or_else(|| "computer".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_server_messages() {
        let m: ServerMsg = serde_json::from_str(
            r#"{"type":"start","camera":"Desk","key":"abcdefghijklmnop","source":{"kind":"window","title":"Notes","exe":"notes.exe"},"fps":5,"max_kbps":900,"max_width":1280}"#,
        )
        .unwrap();
        match m {
            ServerMsg::Start(s) => {
                assert_eq!(s.source, Source::Window { title: "Notes".into(), exe: Some("notes.exe".into()) });
                assert_eq!((s.fps, s.max_kbps, s.max_width), (5, 900, 1280));
            }
            _ => panic!(),
        }
        let m: ServerMsg = serde_json::from_str(r#"{"type":"thumb","req":8,"source":{"kind":"region","x":-10,"y":0,"w":640,"h":480}}"#).unwrap();
        assert!(matches!(m, ServerMsg::Thumb { width: 320, source: Source::Region { x: -10, .. }, .. }));
        assert!(matches!(serde_json::from_str(r#"{"type":"ping"}"#).unwrap(), ServerMsg::Ping));
        assert!(serde_json::from_str::<ServerMsg>(r#"{"type":"whatever"}"#).is_err());
        let s = serde_json::to_value(Source::Monitor { index: 1 }).unwrap();
        assert_eq!(s, serde_json::json!({"kind": "monitor", "index": 1}));
    }

    #[test]
    fn replies() {
        assert_eq!(status("A", State::Error, Some("x")), serde_json::json!({"type":"status","camera":"A","state":"error","error":"x"}));
        assert_eq!(thumb_reply(&8.into(), Ok(vec![1, 2, 3]))["jpeg"], "AQID");
        let s = Sources { monitors: vec![MonitorInfo { name: "DP-1".into(), w: 10, h: 20, primary: true, ..Default::default() }], windows: vec![WindowInfo { title: "t".into(), exe: "e".into(), w: 1, h: 2, xid: Some(5) }] };
        let v = sources_reply(&7.into(), &s);
        assert_eq!(v["req"], 7);
        assert_eq!(v["windows"][0], serde_json::json!({"title":"t","exe":"e","w":1,"h":2}));
        assert_eq!(v["monitors"][0]["primary"], true);
    }
}
