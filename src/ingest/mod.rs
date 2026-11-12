//! Native RTSP ingest (Phase 1). Uses the `retina` crate to demux camera
//! streams in-process: no ffmpeg needed to receive or record encoded video.
//! ffmpeg stays as the decoder for motion analysis and as a fallback ingest
//! for cameras retina cannot handle.

pub mod aac;
pub mod baichuan;
pub mod decode;
pub mod dts;
pub mod flv;
pub mod push;
pub mod fmp4;
pub mod g711;
pub mod live;
pub mod preroll;
pub mod recorder;
pub mod splice;
pub mod sps;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use retina::client::{Credentials, PlayOptions, Session, SessionOptions, SetupOptions, Transport};
use retina::codec::{CodecItem, ParametersRef};
use std::time::{Duration, Instant};

/// Connection parameters for one RTSP stream.
#[derive(Clone)]
pub struct StreamSource {
    /// URL without userinfo (credentials go in `username`/`password`).
    pub url: String,
    pub username: String,
    pub password: String,
    /// Sessions in this group can be torn down (RTSP TEARDOWN) and awaited
    /// before the session's runtime ends, so cameras free the session at
    /// once instead of waiting for it to time out.
    pub group: Option<std::sync::Arc<retina::client::SessionGroup>>,
}

/// What we measured on one media stream during a probe.
#[derive(Debug, Default)]
pub struct StreamStats {
    pub media: String,
    pub encoding: String,
    pub rfc6381: Option<String>,
    pub dimensions: Option<(u32, u32)>,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    pub sdp_framerate: Option<f32>,
    pub frames: u64,
    pub keyframes: u64,
    pub bytes: u64,
    pub loss: u64,
    pub ts_backwards: u64,
    pub max_gap_ms: f64,
    pub first_ts: Option<f64>,
    pub last_ts: Option<f64>,
    gop_current: u64,
    pub gops: Vec<u64>,
    pub param_changes: u64,
    pub first_frame_after: Option<Duration>,
    pub first_keyframe_after: Option<Duration>,
}

impl StreamStats {
    /// Account one received frame (`ts` in seconds on the stream's clock).
    fn record(&mut self, ts: f64, size: usize, loss: u64, key: bool, newp: bool, play_start: Instant) {
        if self.first_frame_after.is_none() {
            self.first_frame_after = Some(play_start.elapsed());
        }
        if let Some(prev) = self.last_ts {
            if ts < prev {
                self.ts_backwards += 1;
            } else {
                self.max_gap_ms = self.max_gap_ms.max((ts - prev) * 1000.0);
            }
        }
        if self.first_ts.is_none() {
            self.first_ts = Some(ts);
        }
        self.last_ts = Some(ts);
        self.frames += 1;
        self.bytes += size as u64;
        self.loss += loss;
        if newp {
            self.param_changes += 1;
        }
        if key {
            if self.first_keyframe_after.is_none() {
                self.first_keyframe_after = Some(play_start.elapsed());
            }
            if self.keyframes > 0 {
                self.gops.push(self.gop_current);
            }
            self.keyframes += 1;
            self.gop_current = 0;
        }
        self.gop_current += 1;
    }

    /// Frames per second measured from RTP timestamps (not wall clock).
    pub fn measured_fps(&self) -> Option<f64> {
        match (self.first_ts, self.last_ts) {
            (Some(a), Some(b)) if b > a && self.frames > 1 => Some((self.frames - 1) as f64 / (b - a)),
            _ => None,
        }
    }
    pub fn kbps(&self) -> Option<f64> {
        match (self.first_ts, self.last_ts) {
            (Some(a), Some(b)) if b > a => Some(self.bytes as f64 * 8.0 / 1000.0 / (b - a)),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ProbeReport {
    pub tool: Option<String>,
    pub streams: Vec<StreamStats>,
    pub connect_time: Duration,
    pub wall_seconds: f64,
}

pub(crate) fn session_options(src: &StreamSource) -> SessionOptions {
    let creds = if src.username.is_empty() {
        None
    } else {
        Some(Credentials { username: src.username.clone(), password: src.password.clone() })
    };
    let opts = SessionOptions::default()
        .creds(creds)
        .user_agent(format!("AmberSight/{}", env!("CARGO_PKG_VERSION")));
    match &src.group {
        Some(g) => opts.session_group(g.clone()),
        None => opts,
    }
}

/// Connect, set up every video/audio stream over TCP, receive for `seconds`,
/// and report codec parameters plus timing/GOP/loss statistics.
pub async fn probe(src: &StreamSource, seconds: u64) -> Result<ProbeReport> {
    if flv::is_flv_url(&src.url) {
        return probe_flv(src, seconds).await;
    }
    let url = url::Url::parse(&src.url).with_context(|| "invalid stream URL")?;
    let t0 = Instant::now();
    let mut session = Session::describe(url, session_options(src))
        .await
        .map_err(|e| anyhow!("DESCRIBE failed: {e}"))?;

    let mut report = ProbeReport {
        tool: session.tool().map(|t| t.to_string()),
        ..Default::default()
    };
    // stream index in the session -> index in report.streams
    let mut map: Vec<Option<usize>> = vec![None; session.streams().len()];
    for i in 0..session.streams().len() {
        let media = session.streams()[i].media().to_string();
        if media != "video" && media != "audio" {
            continue;
        }
        session
            .setup(i, SetupOptions::default().transport(Transport::Tcp(Default::default())))
            .await
            .map_err(|e| anyhow!("SETUP of {media} stream {i} failed: {e}"))?;
        map[i] = Some(report.streams.len());
        let s = &session.streams()[i];
        report.streams.push(StreamStats {
            media,
            encoding: s.encoding_name().to_string(),
            clock_rate: s.clock_rate_hz(),
            channels: s.channels().map(|c| c.get()),
            sdp_framerate: s.framerate(),
            ..Default::default()
        });
    }
    if report.streams.is_empty() {
        return Err(anyhow!("camera offered no video or audio streams"));
    }

    let playing = session
        .play(PlayOptions::default().initial_timestamp(retina::client::InitialTimestampPolicy::Permissive))
        .await
        .map_err(|e| anyhow!("PLAY failed: {e}"))?;
    let mut demuxed = playing.demuxed().map_err(|e| anyhow!("demux setup failed: {e}"))?;
    report.connect_time = t0.elapsed();
    let play_start = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);

    loop {
        let item = match tokio::time::timeout_at(deadline, demuxed.next()).await {
            Err(_) => break, // probe window over
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(anyhow!("stream error: {e}")),
            Ok(Some(Ok(item))) => item,
        };
        let (stream_id, ts, size, loss, key, newp) = match &item {
            CodecItem::VideoFrame(f) => (
                f.stream_id(),
                f.timestamp().elapsed_secs(),
                f.data().len(),
                f.loss() as u64,
                f.is_random_access_point(),
                f.has_new_parameters(),
            ),
            CodecItem::AudioFrame(f) => (
                f.stream_id(),
                f.timestamp().elapsed_secs(),
                f.data().len(),
                f.loss() as u64,
                false,
                false,
            ),
            _ => continue,
        };
        let Some(ri) = map.get(stream_id).copied().flatten() else { continue };
        report.streams[ri].record(ts, size, loss, key, newp, play_start);
    }
    report.wall_seconds = play_start.elapsed().as_secs_f64();

    // Codec parameters are known once the first frames arrived.
    for (i, slot) in map.iter().enumerate() {
        let Some(ri) = slot else { continue };
        let st = &mut report.streams[*ri];
        match demuxed.streams()[i].parameters() {
            Some(ParametersRef::Video(v)) => {
                st.rfc6381 = Some(v.rfc6381_codec().to_string());
                st.dimensions = Some(v.pixel_dimensions());
            }
            Some(ParametersRef::Audio(a)) => {
                st.rfc6381 = a.rfc6381_codec().map(|c| c.to_string());
            }
            _ => {}
        }
    }
    Ok(report)
}

/// Human-readable report for the CLI.
pub fn format_report(label: &str, r: &ProbeReport) -> String {
    let mut out = vec![format!(
        "== {label}: server {:?}, connect {:.0} ms, received {:.1} s",
        r.tool.clone().unwrap_or_else(|| "?".into()),
        r.connect_time.as_secs_f64() * 1000.0,
        r.wall_seconds
    )];
    for s in &r.streams {
        let mut line = format!("  {} {} ({})", s.media, s.encoding, s.rfc6381.clone().unwrap_or_else(|| "-".into()));
        if let Some((w, h)) = s.dimensions {
            line += &format!(" {w}x{h}");
        }
        if let Some(c) = s.channels {
            line += &format!(" {c}ch");
        }
        line += &format!(" clock {} Hz", s.clock_rate);
        out.push(line);
        let gop = if s.gops.is_empty() {
            "-".to_string()
        } else {
            let min = s.gops.iter().min().unwrap();
            let max = s.gops.iter().max().unwrap();
            let avg = s.gops.iter().sum::<u64>() as f64 / s.gops.len() as f64;
            format!("{min}/{avg:.1}/{max}")
        };
        out.push(format!(
            "    frames {} | fps {} (sdp {}) | {} kbps | keyframes {} | GOP min/avg/max {} frames",
            s.frames,
            s.measured_fps().map(|f| format!("{f:.2}")).unwrap_or_else(|| "-".into()),
            s.sdp_framerate.map(|f| format!("{f}")).unwrap_or_else(|| "-".into()),
            s.kbps().map(|k| format!("{k:.0}")).unwrap_or_else(|| "-".into()),
            s.keyframes,
            gop,
        ));
        out.push(format!(
            "    loss {} pkts | ts backwards {} | max gap {:.0} ms | param changes {} | first frame {} | first keyframe {}",
            s.loss,
            s.ts_backwards,
            s.max_gap_ms,
            s.param_changes,
            s.first_frame_after.map(|d| format!("{} ms", d.as_millis())).unwrap_or_else(|| "-".into()),
            s.first_keyframe_after.map(|d| format!("{} ms", d.as_millis())).unwrap_or_else(|| "-".into()),
        ));
    }
    out.join("\n")
}

/// Probe an HTTP-FLV stream through the same reader the engine uses.
async fn probe_flv(src: &StreamSource, seconds: u64) -> Result<ProbeReport> {
    use recorder::MediaItem;
    let t0 = Instant::now();
    let mut s = recorder::open_stream(src, true).await?;
    let mut report = ProbeReport { tool: Some("HTTP-FLV".into()), connect_time: t0.elapsed(), ..Default::default() };
    let mut video = StreamStats { media: "video".into(), ..Default::default() };
    let mut audio = StreamStats { media: "audio".into(), encoding: "mpeg4-generic".into(), ..Default::default() };
    let play_start = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        match tokio::time::timeout_at(deadline, s.next()).await {
            Err(_) | Ok(None) => break,
            Ok(Some(Err(e))) => return Err(anyhow!("stream error: {e:#}")),
            Ok(Some(Ok(MediaItem::Video { ticks, data, key, new_params, .. }))) => {
                video.record(ticks as f64 / 90_000.0, data.len(), 0, key, new_params, play_start);
            }
            Ok(Some(Ok(MediaItem::Audio { ticks, data }))) => {
                let rate = s.audio_params().map(|a| a.clock_rate).unwrap_or(1).max(1);
                audio.record(ticks as f64 / f64::from(rate), data.len(), 0, false, false, play_start);
            }
            Ok(Some(Ok(MediaItem::SenderReport { .. }))) => {}
        }
    }
    report.wall_seconds = play_start.elapsed().as_secs_f64();
    if let Some(v) = s.video_params()? {
        video.encoding = if v.h265 { "h265".into() } else { "h264".into() };
        video.rfc6381 = Some(v.codec);
        video.dimensions = Some((v.width, v.height));
        video.clock_rate = v.clock_rate;
    }
    report.streams.push(video);
    if let Some(a) = s.audio_params() {
        audio.rfc6381 = Some(a.codec);
        audio.clock_rate = a.clock_rate;
        report.streams.push(audio);
    }
    Ok(report)
}
