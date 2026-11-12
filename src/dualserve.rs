//! One port, two protocols: the first byte of every connection decides.
//! 0x16 is a TLS handshake (served the app over HTTPS); anything else is
//! plain HTTP, which only gets a redirect to HTTPS (and the CA download).

use axum::body::Body;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

/// Accept connections forever (the caller races this against shutdown).
pub async fn serve(listener: TcpListener, app: axum::Router, rt: std::sync::Arc<crate::tls::Runtime>) -> std::io::Result<()> {
    let port = listener.local_addr()?.port();
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // e.g. out of file descriptors: back off instead of spinning.
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let (app, rt) = (app.clone(), rt.clone());
        tokio::spawn(async move {
            let _ = tcp.set_nodelay(true);
            let _ = handle(tcp, peer, port, app, rt).await;
        });
    }
}

async fn handle(
    tcp: TcpStream,
    peer: SocketAddr,
    port: u16,
    app: axum::Router,
    rt: std::sync::Arc<crate::tls::Runtime>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let local = tcp.local_addr().ok();
    // Wait (briefly) for the first byte without consuming it.
    let mut first = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(15), tcp.peek(&mut first)).await??;
    if n == 0 {
        return Ok(());
    }
    if first[0] == 0x16 {
        let tls = tokio::time::timeout(Duration::from_secs(15), rt.acceptor().accept(tcp)).await??;
        let svc = service_fn(move |mut req: Request<Incoming>| {
            req.extensions_mut().insert(ConnectInfo(peer));
            req.extensions_mut().insert(crate::tls::Secure);
            let app = app.clone();
            async move { Ok::<_, Infallible>(app.oneshot(req).await.unwrap_or_else(|e| match e {})) }
        });
        Builder::new(TokioExecutor::new())
            .serve_connection_with_upgrades(TokioIo::new(tls), svc)
            .await?;
    } else {
        let fallback = local.map(|a| a.ip().to_string()).unwrap_or_else(|| "localhost".into());
        let svc = service_fn(move |req: Request<Incoming>| {
            let fallback = fallback.clone();
            async move { Ok::<_, Infallible>(plain(&req, &fallback, port)) }
        });
        hyper::server::conn::http1::Builder::new()
            .keep_alive(false)
            .header_read_timeout(Duration::from_secs(15))
            .timer(hyper_util::rt::TokioTimer::new())
            .serve_connection(TokioIo::new(tcp), svc)
            .await?;
    }
    Ok(())
}

/// The answer to a plain-HTTP request: the CA certificate, or a redirect.
/// (A temporary one, 307: browsers remember a permanent redirect, and the
/// http:// address must work again if HTTPS is turned off in Settings.)
fn plain(req: &Request<Incoming>, fallback: &str, port: u16) -> Response<Body> {
    if req.method() == hyper::Method::GET {
        if let Some(r) = crate::tls::ca_download(req.uri().path()) {
            return r;
        }
    }
    let host = req.headers().get(hyper::header::HOST).and_then(|h| h.to_str().ok());
    let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let to = crate::tls::redirect_target(host, fallback, port, pq);
    match Response::builder().status(StatusCode::TEMPORARY_REDIRECT).header(hyper::header::LOCATION, to).body(Body::empty()) {
        Ok(r) => r,
        Err(_) => Response::builder().status(StatusCode::BAD_REQUEST).body(Body::empty()).unwrap(),
    }
}
