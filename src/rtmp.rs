//! RTMP input for pushed streams (OBS, most streaming tools). A sender
//! publishes to `rtmp://server:1935/live` with the camera's stream key; the
//! audio/video it sends is rewritten as FLV tags for the push:// camera.
//! The port opens only once a push:// camera exists.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::engine::Engine;
use crate::ingest::push;

/// Watch the config; open the RTMP port when a push:// camera appears.
pub fn start(engine: Arc<Engine>) {
    tokio::spawn(async move {
        let mut warned = false;
        loop {
            let (bind, port, wanted) = {
                let c = engine.config.lock().unwrap();
                (c.server.bind.clone(), c.server.rtmp_port, c.cameras.iter().any(|x| push::is_push_url(&x.url)))
            };
            if wanted && port != 0 {
                match TcpListener::bind((bind.as_str(), port)).await {
                    Ok(l) => {
                        tracing::info!("RTMP input listening on {bind}:{port}");
                        accept_loop(engine.clone(), l).await;
                    }
                    Err(e) if !warned => {
                        tracing::warn!("RTMP input: cannot listen on {bind}:{port}: {e}");
                        warned = true;
                    }
                    Err(_) => {}
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

/// Most RTMP connections open at once.
const MAX_CONNECTIONS: usize = 64;

async fn accept_loop(engine: Arc<Engine>, l: TcpListener) {
    loop {
        match l.accept().await {
            Ok((sock, peer)) => {
                // A handful of senders is normal; many connections at once
                // (or from a locked-out address) are refused straight away.
                static OPEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                if OPEN.load(std::sync::atomic::Ordering::Relaxed) >= MAX_CONNECTIONS || crate::auth::is_locked(peer.ip()) {
                    drop(sock);
                    continue;
                }
                OPEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let e = engine.clone();
                tokio::spawn(async move {
                    if let Err(err) = connection(e, sock, peer).await {
                        tracing::debug!("RTMP {peer}: {err:#}");
                    }
                    OPEN.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                });
            }
            Err(e) => {
                tracing::warn!("RTMP accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn read_some(sock: &mut TcpStream, buf: &mut [u8], secs: u64) -> Result<usize> {
    Ok(tokio::time::timeout(Duration::from_secs(secs), sock.read(buf)).await.map_err(|_| anyhow!("timed out"))??)
}

async fn connection(engine: Arc<Engine>, mut sock: TcpStream, peer: SocketAddr) -> Result<()> {
    if crate::auth::is_locked(peer.ip()) {
        return Ok(());
    }
    sock.set_nodelay(true).ok();
    let mut buf = vec![0u8; 64 * 1024];
    // Handshake.
    let mut hs = Handshake::new(PeerType::Server);
    let leftover = loop {
        let n = read_some(&mut sock, &mut buf, 10).await?;
        if n == 0 {
            return Ok(());
        }
        match hs.process_bytes(&buf[..n]).map_err(|e| anyhow!("handshake: {e:?}"))? {
            HandshakeProcessResult::InProgress { response_bytes } => sock.write_all(&response_bytes).await?,
            HandshakeProcessResult::Completed { response_bytes, remaining_bytes } => {
                sock.write_all(&response_bytes).await?;
                break remaining_bytes;
            }
        }
    };
    let (mut session, mut todo) = ServerSession::new(ServerSessionConfig::new()).map_err(|e| anyhow!("session: {e:?}"))?;
    todo.extend(session.handle_input(&leftover).map_err(|e| anyhow!("{e:?}"))?);
    let mut publisher: Option<(push::Publisher, String)> = None;
    loop {
        while !todo.is_empty() {
            let mut more = Vec::new();
            for r in todo.drain(..) {
                match r {
                    ServerSessionResult::OutboundResponse(p) => sock.write_all(&p.bytes).await?,
                    ServerSessionResult::RaisedEvent(ev) => match ev {
                        ServerSessionEvent::ConnectionRequested { request_id, .. } => {
                            more.extend(session.accept_request(request_id).map_err(|e| anyhow!("{e:?}"))?);
                        }
                        ServerSessionEvent::PublishStreamRequested { request_id, stream_key, .. } => {
                            match engine.push_camera(&stream_key) {
                                Some(name) => {
                                    more.extend(session.accept_request(request_id).map_err(|e| anyhow!("{e:?}"))?);
                                    tracing::info!("camera '{name}': receiving a pushed stream from {} (RTMP)", peer.ip());
                                    let mut p = push::publish(&stream_key, &format!("{} (RTMP)", peer.ip()));
                                    p.write(&push::flv_header()).await?;
                                    publisher = Some((p, name));
                                }
                                None => {
                                    crate::auth::record_failure(peer.ip());
                                    tracing::info!("RTMP {}: unknown stream key", peer.ip());
                                    // Answer, then hang up: one connection may not
                                    // keep guessing keys.
                                    for r in session
                                        .reject_request(request_id, "NetStream.Publish.BadName", "unknown stream key")
                                        .map_err(|e| anyhow!("{e:?}"))?
                                    {
                                        if let ServerSessionResult::OutboundResponse(p) = r {
                                            let _ = sock.write_all(&p.bytes).await;
                                        }
                                    }
                                    return Ok(());
                                }
                            }
                        }
                        ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
                            if let Some((p, _)) = publisher.as_mut() {
                                if p.write(&push::flv_tag(9, timestamp.value, &data)).await.is_err() {
                                    return Ok(()); // camera stopped reading, or another sender took over
                                }
                            }
                        }
                        ServerSessionEvent::AudioDataReceived { data, timestamp, .. } => {
                            if let Some((p, _)) = publisher.as_mut() {
                                if p.write(&push::flv_tag(8, timestamp.value, &data)).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                        ServerSessionEvent::StreamMetadataChanged { metadata, .. } => {
                            // RTMP sends stream details as a message of its own; the
                            // camera reads them from an FLV metadata tag.
                            if let Some((p, _)) = publisher.as_mut() {
                                let m = push::meta_body(metadata.video_width, metadata.video_height, metadata.video_frame_rate, metadata.audio_codec_id.is_some());
                                if p.write(&push::flv_tag(18, 0, &m)).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                        ServerSessionEvent::PublishStreamFinished { .. } => {
                            if let Some((_, name)) = &publisher {
                                tracing::info!("camera '{name}': pushed stream ended (RTMP)");
                            }
                            return Ok(());
                        }
                        _ => {}
                    },
                    ServerSessionResult::UnhandleableMessageReceived(_) => {}
                }
            }
            todo = more;
        }
        let n = read_some(&mut sock, &mut buf, 30).await?;
        if n == 0 {
            return Ok(());
        }
        todo = session.handle_input(&buf[..n]).map_err(|e| anyhow!("{e:?}"))?;
    }
}
