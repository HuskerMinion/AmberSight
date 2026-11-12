//! Control-channel tests against a fake server (TLS with its own CA), and
//! an opt-in upload test against a real server.

use super::*;
use futures::future::BoxFuture;
use proto::{Source, StartSpec, State};
use serde_json::{json, Value};
use session::StatusEvent;
use std::sync::Mutex;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// A server TLS config signed by a fresh CA, and that CA's PEM.
fn tls_server(tag: &str) -> (Arc<rustls::ServerConfig>, String) {
    let dir = std::env::temp_dir().join(format!("amber-capctl-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::tls::ensure_ca(&dir).unwrap();
    let ca = crate::tls::ensure_ca(&dir).unwrap();
    crate::tls::ensure_server_cert(&dir, &ca, &["127.0.0.1".to_string()], chrono::Utc::now().timestamp()).unwrap();
    let sc = crate::tls::server_config(&std::fs::read(dir.join("cert.pem")).unwrap(), &std::fs::read(dir.join("key.pem")).unwrap()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (sc, ca.pem)
}

#[derive(Default)]
struct Fake {
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
        Box::pin(async { proto::Sources::default() })
    }
    fn thumb(&self, _s: Source, _w: u32) -> BoxFuture<'static, Result<Vec<u8>, String>> {
        Box::pin(async { Err("no".to_string()) })
    }
    fn start(&self, spec: StartSpec, gen: u64, events: UnboundedSender<StatusEvent>) -> JoinHandle<()> {
        self.live.lock().unwrap().push(spec.camera.clone());
        let live = Live(self.live.clone(), spec.camera.clone());
        tokio::spawn(async move {
            let _live = live;
            let _ = events.send(StatusEvent { camera: spec.camera, gen, state: State::Running, error: None });
            std::future::pending::<()>().await;
        })
    }
}

type ServerWs = tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

/// Accept one helper connection; returns it and its request headers.
async fn accept(l: &TcpListener, sc: &Arc<rustls::ServerConfig>) -> (ServerWs, Vec<(String, String)>) {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    let (tcp, _) = tokio::time::timeout(Duration::from_secs(15), l.accept()).await.expect("helper reconnects").unwrap();
    let tls = tokio_rustls::TlsAcceptor::from(sc.clone()).accept(tcp).await.unwrap();
    let headers = Arc::new(Mutex::new(Vec::new()));
    let h2 = headers.clone();
    let ws = tokio_tungstenite::accept_hdr_async(tls, move |req: &Request, resp: Response| {
        assert_eq!(req.uri().path(), "/helper/ws");
        *h2.lock().unwrap() = req.headers().iter().map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())).collect();
        Ok(resp)
    })
    .await
    .unwrap();
    let h = headers.lock().unwrap().clone();
    (ws, h)
}

async fn next_json(ws: &mut ServerWs) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("a message").expect("open").unwrap();
        if let Message::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

async fn send(ws: &mut ServerWs, v: Value) {
    ws.send(Message::Text(v.to_string())).await.unwrap();
}

#[tokio::test]
async fn control_channel_with_reconnect() {
    let (sc, pem) = tls_server("ctl");
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cfg = HelperConfig { server: format!("https://{}", l.local_addr().unwrap()), id: "h-1".into(), token: "tok-secret".into(), ca_pem: pem };
    let fake = Arc::new(Fake::default());
    let task = tokio::spawn(control_loop(fake.clone(), cfg, LoopOpts { config_path: None, tray: false }));

    // First connection.
    let (mut ws, headers) = accept(&l, &sc).await;
    let get = |k: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
    assert_eq!(get("authorization").as_deref(), Some("Bearer tok-secret"));
    assert_eq!(get("x-helper-id").as_deref(), Some("h-1"));
    let hello = next_json(&mut ws).await;
    assert_eq!((hello["type"].as_str(), hello["id"].as_str(), hello["os"].as_str()), (Some("hello"), Some("h-1"), Some(proto::os_name())));
    assert!(hello["machine"].as_str().is_some_and(|m| !m.is_empty()) && hello["version"].is_string());
    send(&mut ws, json!({"type": "ping"})).await;
    assert_eq!(next_json(&mut ws).await, json!({"type": "pong"}));
    send(&mut ws, json!({"type": "list", "req": 7})).await;
    assert_eq!(next_json(&mut ws).await, json!({"type": "sources", "req": 7, "monitors": [], "windows": []}));
    let start_a = json!({"type": "start", "camera": "A", "key": "abcdefghijklmnop", "source": {"kind": "monitor", "index": 0}, "fps": 10, "max_kbps": 2500, "max_width": 1920});
    send(&mut ws, json!({"type": "desired", "cameras": ["A"]})).await;
    send(&mut ws, start_a.clone()).await;
    assert_eq!(next_json(&mut ws).await, json!({"type": "status", "camera": "A", "state": "starting"}));
    assert_eq!(next_json(&mut ws).await, json!({"type": "status", "camera": "A", "state": "running"}));
    assert_eq!(*fake.live.lock().unwrap(), ["A"]);

    // The server goes away; the capture keeps running and the helper comes back.
    drop(ws);
    let (mut ws, _) = accept(&l, &sc).await;
    assert_eq!(next_json(&mut ws).await["type"], "hello");
    assert_eq!(next_json(&mut ws).await, json!({"type": "status", "camera": "A", "state": "running"}));
    assert_eq!(*fake.live.lock().unwrap(), ["A"]);
    // Server now wants B only: A stops, B starts.
    send(&mut ws, json!({"type": "desired", "cameras": ["B"]})).await;
    let mut start_b = start_a.clone();
    start_b["camera"] = "B".into();
    send(&mut ws, start_b).await;
    assert_eq!(next_json(&mut ws).await, json!({"type": "status", "camera": "A", "state": "stopped"}));
    assert_eq!(next_json(&mut ws).await, json!({"type": "status", "camera": "B", "state": "starting"}));
    assert_eq!(next_json(&mut ws).await, json!({"type": "status", "camera": "B", "state": "running"}));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*fake.live.lock().unwrap(), ["B"]);
    task.abort();
}

#[tokio::test]
async fn rejected_helper_stops_and_waits() {
    // A server that answers the upgrade with 401: the helper must not spin.
    let (sc, pem) = tls_server("rej");
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cfg = HelperConfig { server: format!("https://{}", l.local_addr().unwrap()), id: "h-1".into(), token: "t".into(), ca_pem: pem };
    let task = tokio::spawn(control_loop(Arc::new(Fake::default()), cfg, LoopOpts { config_path: None, tray: false }));
    let (tcp, _) = l.accept().await.unwrap();
    let mut tls = tokio_rustls::TlsAcceptor::from(sc.clone()).accept(tcp).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut b = [0u8; 4096];
    let _ = tls.read(&mut b).await.unwrap();
    tls.write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n").await.unwrap();
    let _ = tls.shutdown().await;
    // No new attempt within a few seconds (the backoff jumped to its maximum).
    assert!(tokio::time::timeout(Duration::from_secs(4), l.accept()).await.is_err());
    task.abort();
}

/// Opt-in: push a synthetic test pattern through the real capture path to a
/// running server. Set AMBER_E2E_SERVER (https://host:port), AMBER_E2E_KEY
/// (a push:// camera's key), AMBER_E2E_CA (path to the server's ca.pem) and
/// optionally AMBER_E2E_SECONDS, then run with `--ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn e2e_push_test_pattern() {
    let server = std::env::var("AMBER_E2E_SERVER").expect("AMBER_E2E_SERVER");
    let key = std::env::var("AMBER_E2E_KEY").expect("AMBER_E2E_KEY");
    let pem = std::fs::read_to_string(std::env::var("AMBER_E2E_CA").expect("AMBER_E2E_CA")).unwrap();
    let secs: u64 = std::env::var("AMBER_E2E_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    // Synthetic frames by default. AMBER_E2E_VIRTUAL_DISPLAY=:N (an Xvfb you
    // started) uses the real x11grab path on that virtual display instead.
    let (input_override, source) = match std::env::var("AMBER_E2E_VIRTUAL_DISPLAY") {
        Ok(d) if cfg!(target_os = "linux") && d.starts_with(':') => {
            std::env::set_var("DISPLAY", d);
            (None, Source::Monitor { index: 0 })
        }
        _ => (Some(["-re", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=10"].iter().map(|s| s.to_string()).collect()), Source::Desktop),
    };
    let real = runner::Real { target: net::Target::parse(&server).unwrap(), tls: net::pinned(&pem).unwrap(), input_override };
    let spec = StartSpec { camera: "e2e".into(), key, source, fps: 10, max_kbps: 1500, max_width: 960 };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let h = real.start(spec, 1, tx);
    let mut states = Vec::new();
    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Some(ev)) = tokio::time::timeout_at(end, rx.recv()).await {
        println!("status: {:?} {:?}", ev.state, ev.error);
        states.push(ev.state);
    }
    h.abort();
    assert!(states.contains(&State::Running), "{states:?}");
    assert!(!states.contains(&State::Error), "{states:?}");
}
