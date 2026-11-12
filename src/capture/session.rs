//! The helper's state: which cameras run, and how server messages change that.
//! It outlives control connections, so captures keep running while the
//! helper reconnects.

use super::proto::{self, ServerMsg, Source, StartSpec, State};
use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// A state change reported by a capture task.
#[derive(Clone, Debug)]
pub struct StatusEvent {
    pub camera: String,
    /// Which start of this camera reported it (older ones are ignored).
    pub gen: u64,
    pub state: State,
    pub error: Option<String>,
}

/// What actually captures; replaced by a fake in tests.
pub trait Backend: Send + Sync + 'static {
    fn sources(&self) -> BoxFuture<'static, proto::Sources>;
    fn thumb(&self, source: Source, width: u32) -> BoxFuture<'static, Result<Vec<u8>, String>>;
    /// Run a capture until aborted, reporting state changes on `events`.
    fn start(&self, spec: StartSpec, gen: u64, events: UnboundedSender<StatusEvent>) -> JoinHandle<()>;
}

struct Cam {
    spec: StartSpec,
    gen: u64,
    task: Option<JoinHandle<()>>,
    state: State,
    error: Option<String>,
}

pub struct Session<B: Backend> {
    backend: Arc<B>,
    cams: BTreeMap<String, Cam>,
    paused: bool,
    next_gen: u64,
    events: UnboundedSender<StatusEvent>,
    /// Messages for the server (dropped by the caller while disconnected).
    out: UnboundedSender<Value>,
}

impl<B: Backend> Session<B> {
    pub fn new(backend: Arc<B>, events: UnboundedSender<StatusEvent>, out: UnboundedSender<Value>) -> Self {
        Session { backend, cams: BTreeMap::new(), paused: false, next_gen: 1, events, out }
    }

    fn send(&self, v: Value) {
        let _ = self.out.send(v);
    }

    fn report(&self, camera: &str) {
        if let Some(c) = self.cams.get(camera) {
            self.send(proto::status(camera, c.state, c.error.as_deref()));
        }
    }

    /// One text frame from the server.
    pub fn handle_text(&mut self, text: &str) {
        let msg: ServerMsg = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("capture: ignoring message ({e}): {}", text.chars().take(200).collect::<String>());
                return;
            }
        };
        self.handle(msg);
    }

    pub fn handle(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Ping => self.send(serde_json::json!({"type": "pong"})),
            ServerMsg::List { req } => {
                let (fut, out) = (self.backend.sources(), self.out.clone());
                tokio::spawn(async move {
                    let s = fut.await;
                    let _ = out.send(proto::sources_reply(&req, &s));
                });
            }
            ServerMsg::Thumb { req, source, width } => {
                let (fut, out) = (self.backend.thumb(source, width), self.out.clone());
                tokio::spawn(async move {
                    let r = fut.await;
                    let _ = out.send(proto::thumb_reply(&req, r));
                });
            }
            ServerMsg::Start(spec) => self.start(spec.clamped()),
            ServerMsg::Stop { camera } => self.stop(&camera),
            ServerMsg::Desired { cameras } => {
                let gone: Vec<String> = self.cams.keys().filter(|k| !cameras.contains(k)).cloned().collect();
                for c in gone {
                    self.stop(&c);
                }
            }
        }
    }

    fn start(&mut self, spec: StartSpec) {
        let name = spec.camera.clone();
        if let Some(c) = self.cams.get(&name) {
            if c.spec == spec && (c.task.is_some() || self.paused) {
                // Same settings: keep it running, just say how it is.
                self.report(&name);
                return;
            }
        }
        if let Some(mut old) = self.cams.remove(&name) {
            if let Some(t) = old.task.take() {
                t.abort();
            }
        }
        let gen = self.next_gen;
        self.next_gen += 1;
        let mut cam = Cam { spec, gen, task: None, state: State::Stopped, error: None };
        if !self.paused {
            cam.state = State::Starting;
            cam.task = Some(self.backend.start(cam.spec.clone(), gen, self.events.clone()));
        }
        self.cams.insert(name.clone(), cam);
        self.report(&name);
    }

    fn stop(&mut self, camera: &str) {
        if let Some(mut c) = self.cams.remove(camera) {
            if let Some(t) = c.task.take() {
                t.abort();
            }
            tracing::info!("capture: stopped '{camera}'");
        }
        self.send(proto::status(camera, State::Stopped, None));
    }

    /// A state change from a capture task.
    pub fn on_event(&mut self, ev: StatusEvent) {
        let Some(c) = self.cams.get_mut(&ev.camera) else { return };
        if c.gen != ev.gen || c.task.is_none() {
            return;
        }
        if c.state == ev.state && c.error == ev.error {
            return;
        }
        c.state = ev.state;
        c.error = ev.error;
        self.report(&ev.camera);
    }

    /// Stop every capture (privacy pause); they stay wanted.
    #[cfg_attr(not(windows), allow(dead_code))] // tray menu (Windows only for now)
    pub fn pause(&mut self) {
        if self.paused {
            return;
        }
        self.paused = true;
        let names: Vec<String> = self.cams.keys().cloned().collect();
        for n in names {
            let c = self.cams.get_mut(&n).unwrap();
            if let Some(t) = c.task.take() {
                t.abort();
            }
            c.state = State::Stopped;
            c.error = None;
            self.report(&n);
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn resume(&mut self) {
        if !self.paused {
            return;
        }
        self.paused = false;
        let specs: Vec<StartSpec> = self.cams.values().map(|c| c.spec.clone()).collect();
        for s in specs {
            self.cams.remove(&s.camera);
            self.start(s);
        }
    }

    /// Stop everything for good (quit, or the helper was removed).
    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.cams.keys().cloned().collect();
        for n in names {
            self.stop(&n);
        }
    }

    /// Re-send every camera's state (after reconnecting).
    pub fn report_all(&self) {
        for n in self.cams.keys() {
            self.report(n);
        }
    }

    /// (any camera wanted, paused): drives the tray icon.
    pub fn tray_state(&self) -> (bool, bool) {
        (!self.cams.is_empty(), self.paused)
    }
}

impl<B: Backend> Drop for Session<B> {
    fn drop(&mut self) {
        for c in self.cams.values_mut() {
            if let Some(t) = c.task.take() {
                t.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    #[derive(Default)]
    struct Fake {
        started: Mutex<Vec<(String, u64)>>,
        live: Arc<Mutex<Vec<String>>>,
    }

    struct Live(Arc<Mutex<Vec<String>>>, String);
    impl Drop for Live {
        fn drop(&mut self) {
            let mut l = self.0.lock().unwrap();
            if let Some(i) = l.iter().position(|c| c == &self.1) {
                l.remove(i);
            }
        }
    }

    impl Backend for Fake {
        fn sources(&self) -> BoxFuture<'static, proto::Sources> {
            Box::pin(async {
                proto::Sources { monitors: vec![proto::MonitorInfo { name: "M".into(), w: 100, h: 50, primary: true, ..Default::default() }], windows: vec![] }
            })
        }
        fn thumb(&self, source: Source, width: u32) -> BoxFuture<'static, Result<Vec<u8>, String>> {
            Box::pin(async move {
                match source {
                    Source::Desktop => Ok(vec![0xff, 0xd8, width as u8]),
                    _ => Err("window not open".into()),
                }
            })
        }
        fn start(&self, spec: StartSpec, gen: u64, events: UnboundedSender<StatusEvent>) -> JoinHandle<()> {
            self.started.lock().unwrap().push((spec.camera.clone(), gen));
            self.live.lock().unwrap().push(spec.camera.clone());
            let live = Live(self.live.clone(), spec.camera.clone());
            tokio::spawn(async move {
                let _live = live;
                let _ = events.send(StatusEvent { camera: spec.camera, gen, state: State::Running, error: None });
                std::future::pending::<()>().await;
            })
        }
    }

    struct T {
        s: Session<Fake>,
        fake: Arc<Fake>,
        ev: UnboundedReceiver<StatusEvent>,
        out: UnboundedReceiver<Value>,
    }

    impl T {
        fn new() -> T {
            let fake = Arc::new(Fake::default());
            let (etx, ev) = unbounded_channel();
            let (otx, out) = unbounded_channel();
            T { s: Session::new(fake.clone(), etx, otx), fake, ev, out }
        }
        fn send(&mut self, v: Value) {
            self.s.handle_text(&v.to_string());
        }
        /// Let tasks run, deliver their events, and collect what went out.
        async fn drain(&mut self) -> Vec<Value> {
            for _ in 0..5 {
                tokio::task::yield_now().await;
                while let Ok(e) = self.ev.try_recv() {
                    self.s.on_event(e);
                }
            }
            let mut v = Vec::new();
            while let Ok(m) = self.out.try_recv() {
                v.push(m);
            }
            v
        }
        fn live(&self) -> Vec<String> {
            let mut l = self.fake.live.lock().unwrap().clone();
            l.sort();
            l
        }
        fn starts(&self) -> usize {
            self.fake.started.lock().unwrap().len()
        }
    }

    fn start(cam: &str, fps: u32) -> Value {
        json!({"type": "start", "camera": cam, "key": format!("key-{cam}-0123456789"), "source": {"kind": "desktop"}, "fps": fps, "max_kbps": 2500, "max_width": 1920})
    }

    fn st(cam: &str, state: &str) -> Value {
        json!({"type": "status", "camera": cam, "state": state})
    }

    #[tokio::test]
    async fn ping_list_thumb() {
        let mut t = T::new();
        t.send(json!({"type": "ping"}));
        t.send(json!({"type": "list", "req": 7}));
        t.send(json!({"type": "thumb", "req": 8, "source": {"kind": "desktop"}, "width": 99}));
        t.send(json!({"type": "thumb", "req": 9, "source": {"kind": "window", "title": "x"}}));
        t.send(json!({"type": "unknown-thing"}));
        t.s.handle_text("not json");
        let out = t.drain().await;
        assert_eq!(out[0], json!({"type": "pong"}));
        assert!(out.contains(&json!({"type": "sources", "req": 7, "monitors": [{"index": 0, "name": "M", "x": 0, "y": 0, "w": 100, "h": 50, "primary": true}], "windows": []})));
        assert!(out.contains(&json!({"type": "thumb", "req": 8, "jpeg": "/9hj"})));
        assert!(out.contains(&json!({"type": "thumb", "req": 9, "error": "window not open"})));
        assert_eq!(out.len(), 4);
    }

    #[tokio::test]
    async fn start_stop_and_identical_start_is_noop() {
        let mut t = T::new();
        t.send(start("A", 10));
        assert_eq!(t.drain().await, vec![st("A", "starting"), st("A", "running")]);
        assert_eq!(t.live(), ["A"]);
        // Same settings: no restart, current state reported.
        t.send(start("A", 10));
        assert_eq!(t.drain().await, vec![st("A", "running")]);
        assert_eq!(t.starts(), 1);
        // New settings: restart.
        t.send(start("A", 5));
        assert_eq!(t.drain().await, vec![st("A", "starting"), st("A", "running")]);
        assert_eq!(t.starts(), 2);
        assert_eq!(t.live(), ["A"]);
        t.send(json!({"type": "stop", "camera": "A"}));
        assert_eq!(t.drain().await, vec![st("A", "stopped")]);
        assert!(t.live().is_empty());
    }

    #[tokio::test]
    async fn desired_stops_others() {
        let mut t = T::new();
        for c in ["A", "B", "C"] {
            t.send(start(c, 10));
        }
        t.drain().await;
        assert_eq!(t.live(), ["A", "B", "C"]);
        // Reconnect: the server lists what it wants, then starts each.
        t.send(json!({"type": "desired", "cameras": ["B", "D"]}));
        t.send(start("B", 10));
        t.send(start("D", 10));
        let out = t.drain().await;
        assert_eq!(t.live(), ["B", "D"]);
        assert!(out.contains(&st("A", "stopped")) && out.contains(&st("C", "stopped")));
        assert!(out.contains(&st("B", "running")) && out.contains(&st("D", "running")));
        assert_eq!(t.starts(), 4); // B was not restarted
        t.send(json!({"type": "desired", "cameras": []}));
        t.drain().await;
        assert!(t.live().is_empty());
        assert_eq!(t.s.tray_state(), (false, false));
    }

    #[tokio::test]
    async fn pause_and_resume() {
        let mut t = T::new();
        t.send(start("A", 10));
        t.drain().await;
        t.s.pause();
        assert_eq!(t.drain().await, vec![st("A", "stopped")]);
        assert!(t.live().is_empty());
        assert_eq!(t.s.tray_state(), (true, true));
        // A start while paused is remembered but does not capture.
        t.send(start("B", 10));
        assert_eq!(t.drain().await, vec![st("B", "stopped")]);
        assert!(t.live().is_empty());
        t.s.resume();
        let out = t.drain().await;
        assert_eq!(t.live(), ["A", "B"]);
        assert!(out.contains(&st("A", "running")) && out.contains(&st("B", "running")));
        assert_eq!(t.s.tray_state(), (true, false));
    }

    #[tokio::test]
    async fn stale_events_are_ignored() {
        let mut t = T::new();
        t.send(start("A", 10));
        t.drain().await;
        t.s.on_event(StatusEvent { camera: "A".into(), gen: 0, state: State::Error, error: Some("old".into()) });
        t.s.on_event(StatusEvent { camera: "Z".into(), gen: 1, state: State::Error, error: None });
        assert!(t.drain().await.is_empty());
        t.s.on_event(StatusEvent { camera: "A".into(), gen: 1, state: State::Error, error: Some("window not open".into()) });
        assert_eq!(t.drain().await, vec![json!({"type": "status", "camera": "A", "state": "error", "error": "window not open"})]);
    }
}
