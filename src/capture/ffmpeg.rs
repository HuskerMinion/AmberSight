//! ffmpeg command lines for screen capture, encoding and thumbnails.
//!
//! The helper never opens cameras or microphones: only screen grabbers
//! (ddagrab/gdigrab on Windows, x11grab on Linux) are ever built here.

use super::proto::{Source, StartSpec};
use crate::local_source::Os;
use anyhow::{anyhow, bail, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// A source with what was looked up about it on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    Desktop,
    /// `rect` = (x, y, w, h) when known.
    Monitor { index: u32, rect: Option<(i32, i32, u32, u32)> },
    /// `xid`: X11 window id (Linux), window handle (Windows). `drawn`:
    /// (window DPI, screen DPI) when the window draws itself smaller than
    /// it shows (an older program on a scaled screen, Windows).
    Window { title: String, xid: Option<u64>, drawn: Option<(u32, u32)> },
    Region { x: i32, y: i32, w: u32, h: u32 },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    WindowNotOpen,
}

/// Look the source up (windows must be open; monitors get their position).
pub fn resolve(source: &Source) -> std::result::Result<Resolved, ResolveError> {
    Ok(match source {
        Source::Desktop => Resolved::Desktop,
        Source::Monitor { index } => {
            let rect = super::sources::find_monitor(*index).map(|m| (m.x, m.y, m.w, m.h));
            Resolved::Monitor { index: *index, rect }
        }
        Source::Window { title, exe } => {
            let w = super::sources::find_window(title, exe.as_deref()).ok_or(ResolveError::WindowNotOpen)?;
            #[cfg(windows)]
            let drawn = w.xid.and_then(super::win::drawn_scale);
            #[cfg(not(windows))]
            let drawn = None;
            Resolved::Window { title: w.title, xid: w.xid, drawn }
        }
        Source::Region { x, y, w, h } => Resolved::Region { x: *x, y: *y, w: *w, h: *h },
    })
}

fn v(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// ffmpeg input arguments, best first (later ones are fallbacks).
pub fn input_variants(r: &Resolved, fps: u32, os: Os, display: &str) -> Result<Vec<Vec<String>>> {
    let fps = fps.to_string();
    let gdi = |extra: &[String], input: &str| {
        let mut a = v(&["-f", "gdigrab", "-framerate", &fps, "-draw_mouse", "1"]);
        a.extend_from_slice(extra);
        a.extend(v(&["-i", input]));
        a
    };
    let area = |x: i32, y: i32, w: u32, h: u32| v(&["-offset_x", &x.to_string(), "-offset_y", &y.to_string(), "-video_size", &format!("{w}x{h}")]);
    let x11 = |extra: &[String], input: String| {
        let mut a = v(&["-f", "x11grab", "-framerate", &fps, "-draw_mouse", "1"]);
        a.extend_from_slice(extra);
        a.extend(["-i".to_string(), input]);
        a
    };
    Ok(match os {
        Os::Windows => match r {
            Resolved::Desktop => vec![gdi(&[], "desktop")],
            Resolved::Monitor { index, rect } => {
                // ddagrab (Desktop Duplication): frames are on the GPU, download them.
                let mut out = vec![v(&["-f", "lavfi", "-i", &format!("ddagrab=output_idx={index}:framerate={fps},hwdownload,format=bgra")])];
                if let Some((x, y, w, h)) = rect {
                    out.push(gdi(&area(*x, *y, *w, *h), "desktop"));
                }
                out
            }
            Resolved::Window { title, xid, drawn } => {
                // Windows Graphics Capture (ffmpeg 8+): that window's own
                // picture, even while other windows cover it. Older ffmpeg:
                // the screen area it occupies (covered parts show what is on top).
                let mut out = Vec::new();
                if let Some(h) = xid {
                    out.push(v(&["-f", "lavfi", "-i", &format!("gfxcapture=hwnd={h}:max_framerate={fps},hwdownload,format=bgra")]));
                }
                let mut g = gdi(&[], &format!("title={title}"));
                if let Some((win, screen)) = drawn {
                    // gdigrab gets such a window's own (smaller) drawing in
                    // the top-left corner of a screen-sized frame: keep it.
                    let f = |d: &str| format!("trunc(i{d}*{win}/{screen}/2)*2");
                    g.extend(v(&["-vf", &format!("crop={}:{}:0:0", f("w"), f("h"))]));
                }
                out.push(g);
                out
            }
            Resolved::Region { x, y, w, h } => vec![gdi(&area(*x, *y, *w, *h), "desktop")],
        },
        Os::Linux => match r {
            Resolved::Desktop | Resolved::Monitor { rect: None, .. } => vec![x11(&[], display.to_string())],
            Resolved::Monitor { rect: Some((x, y, w, h)), .. } | Resolved::Region { x, y, w, h } => {
                vec![x11(&v(&["-video_size", &format!("{w}x{h}")]), format!("{display}+{x},{y}"))]
            }
            Resolved::Window { xid: Some(id), .. } => vec![x11(&v(&["-window_id", &format!("0x{id:x}")]), display.to_string())],
            Resolved::Window { xid: None, .. } => bail!("window capture needs wmctrl on Linux"),
        },
        Os::Mac => {
            // AVFoundation screen devices are named "Capture screen N".
            let screen = |n: u32| {
                v(&["-f", "avfoundation", "-capture_cursor", "1", "-framerate", &fps, "-pixel_format", "uyvy422", "-i", &format!("Capture screen {n}:none")])
            };
            match r {
                Resolved::Desktop => vec![screen(0)],
                Resolved::Monitor { index, .. } => vec![screen(*index)],
                Resolved::Window { .. } => bail!("capturing a single window is not available on macOS: capture its screen (or a region of it) instead"),
                Resolved::Region { x, y, w, h } => {
                    // Screen 0, cropped to the region.
                    let mut a = screen(0);
                    a.extend(v(&["-vf", &format!("crop={w}:{h}:{x}:{y}")]));
                    vec![a]
                }
            }
        }
    })
}

/// Hardware encoders tried (in order) before libx264.
pub const HW_ENCODERS: [&str; 3] = ["h264_qsv", "h264_nvenc", "h264_amf"];

fn pix_fmt(encoder: &str) -> &'static str {
    if encoder == "h264_qsv" {
        "nv12"
    } else {
        "yuv420p"
    }
}

/// Even width at most `max_width`, aspect kept.
fn scale_filter(max_width: u32, encoder: &str) -> String {
    format!("scale='trunc(min({max_width},iw)/2)*2':-2,format={}", pix_fmt(encoder))
}

/// Full argument list: `input` captured, encoded with `encoder`, FLV on stdout.
pub fn capture_args(input: &[String], spec: &StartSpec, encoder: &str) -> Vec<String> {
    let k = spec.max_kbps;
    let (input, pre) = split_vf(input);
    let mut a = v(&["-hide_banner", "-loglevel", "error", "-nostdin"]);
    a.extend_from_slice(&input);
    a.extend(v(&["-an", "-vf", &chain(&pre, scale_filter(spec.max_width, encoder)), "-r", &spec.fps.to_string(), "-c:v", encoder]));
    match encoder {
        "libx264" => a.extend(v(&["-preset", "veryfast", "-tune", "zerolatency", "-crf", "23"])),
        "h264_qsv" => a.extend(v(&["-preset", "veryfast", "-b:v", &format!("{k}k")])),
        "h264_nvenc" => a.extend(v(&["-preset", "p4", "-tune", "ll", "-b:v", &format!("{k}k")])),
        "h264_amf" => a.extend(v(&["-usage", "lowlatency", "-b:v", &format!("{k}k")])),
        _ => {}
    }
    a.extend(v(&[
        "-maxrate",
        &format!("{k}k"),
        "-bufsize",
        &format!("{}k", k * 2),
        "-g",
        &(spec.fps * 2).to_string(),
        "-bf",
        "0",
        "-f",
        "flv",
        "pipe:1",
    ]));
    a
}

/// An input's own picture filter (a crop: macOS regions), taken out so it
/// can go first in the one -vf chain (a second -vf would replace it).
fn split_vf(input: &[String]) -> (Vec<String>, Option<String>) {
    match input.iter().position(|a| a == "-vf") {
        Some(i) if i + 1 < input.len() => {
            let mut rest = input.to_vec();
            let f = rest.remove(i + 1);
            rest.remove(i);
            (rest, Some(f))
        }
        _ => (input.to_vec(), None),
    }
}

fn chain(pre: &Option<String>, f: String) -> String {
    match pre {
        Some(p) => format!("{p},{f}"),
        None => f,
    }
}

/// One JPEG frame of `input`, `width` pixels wide at most.
pub fn thumb_args(input: &[String], width: u32) -> Vec<String> {
    let (input, pre) = split_vf(input);
    let mut a = v(&["-hide_banner", "-loglevel", "error", "-nostdin"]);
    a.extend_from_slice(&input);
    a.extend(v(&[
        "-frames:v",
        "1",
        "-an",
        "-vf",
        &chain(&pre, format!("scale='trunc(min({width},iw)/2)*2':-2")),
        "-c:v",
        "mjpeg",
        "-q:v",
        "5",
        "-f",
        "image2",
        "pipe:1",
    ]));
    a
}

/// ffmpeg next to the helper's own exe, the one the service on this computer
/// names in its note, else from PATH (see ffmpeg_bin).
pub fn ffmpeg_path() -> PathBuf {
    crate::ffmpeg_bin::helper_program()
}

/// An ffmpeg command that dies with the helper and never opens a window.
pub fn command(args: &[String]) -> tokio::process::Command {
    let mut std_cmd = std::process::Command::new(ffmpeg_path());
    std_cmd.args(args).stdin(Stdio::null());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe and touches no shared state.
        unsafe {
            std_cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std_cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut c = tokio::process::Command::from(std_cmd);
    c.kill_on_drop(true);
    c
}

pub fn adopt(_child: &tokio::process::Child) {
    #[cfg(windows)]
    super::win::adopt(_child);
}

/// The encoder to use: the first hardware encoder that works, else libx264.
pub async fn encoder() -> &'static str {
    static ENC: tokio::sync::OnceCell<&'static str> = tokio::sync::OnceCell::const_new();
    *ENC.get_or_init(|| async {
        if let Ok(e) = std::env::var("AMBERSIGHT_CAPTURE_ENCODER") {
            if let Some(hit) = HW_ENCODERS.iter().chain(["libx264"].iter()).find(|n| **n == e) {
                return *hit;
            }
        }
        for e in HW_ENCODERS {
            if probe(e).await {
                tracing::info!("capture: using the {e} hardware encoder");
                return e;
            }
        }
        tracing::info!("capture: using the libx264 software encoder");
        "libx264"
    })
    .await
}

/// Encode a few synthetic frames (no capture device) to see if `encoder` works.
async fn probe(encoder: &str) -> bool {
    let args = v(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=320x240:r=10",
        "-frames:v",
        "5",
        "-vf",
        &format!("format={}", pix_fmt(encoder)),
        "-c:v",
        encoder,
        "-f",
        "null",
        "-",
    ]);
    let mut c = command(&args);
    c.stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = c.spawn() else { return false };
    adopt(&child);
    matches!(tokio::time::timeout(Duration::from_secs(8), child.wait()).await, Ok(Ok(s)) if s.success())
}

/// Grab one JPEG using the first input variant that works.
pub async fn grab_jpeg(variants: &[Vec<String>], width: u32) -> Result<Vec<u8>> {
    let width = width.clamp(16, 1920);
    let mut last = anyhow!("nothing to capture");
    for (i, input) in variants.iter().enumerate() {
        // A still window gives Windows Graphics Capture nothing to send:
        // do not wait long before trying the fallback.
        let wait = if i + 1 < variants.len() { 4 } else { 10 };
        let mut c = command(&thumb_args(input, width));
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = c.spawn().map_err(|e| anyhow!("could not start ffmpeg ({}): {e}", ffmpeg_path().display()))?;
        adopt(&child);
        match tokio::time::timeout(Duration::from_secs(wait), child.wait_with_output()).await {
            Err(_) => last = anyhow!("timed out grabbing a picture"),
            Ok(Err(e)) => last = e.into(),
            Ok(Ok(o)) if o.status.success() && o.stdout.starts_with(&[0xff, 0xd8]) => return Ok(o.stdout),
            Ok(Ok(o)) => last = anyhow!("{}", last_line(&String::from_utf8_lossy(&o.stderr)).unwrap_or("ffmpeg failed".into())),
        }
    }
    Err(last)
}

/// The last non-empty line of ffmpeg's error output.
pub fn last_line(s: &str) -> Option<String> {
    s.lines().map(str::trim).rfind(|l| !l.is_empty()).map(|l| l.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(source: Source) -> StartSpec {
        StartSpec { camera: "Desk".into(), key: "abcdefghijklmnop".into(), source, fps: 10, max_kbps: 2500, max_width: 1280 }
    }

    fn s(a: &[String]) -> String {
        a.join(" ")
    }

    #[test]
    fn windows_inputs() {
        let w = |r: Resolved| input_variants(&r, 10, Os::Windows, "").unwrap().iter().map(|a| s(a)).collect::<Vec<_>>();
        assert_eq!(w(Resolved::Desktop), ["-f gdigrab -framerate 10 -draw_mouse 1 -i desktop"]);
        assert_eq!(
            w(Resolved::Monitor { index: 1, rect: Some((-1920, 0, 1920, 1080)) }),
            [
                "-f lavfi -i ddagrab=output_idx=1:framerate=10,hwdownload,format=bgra",
                "-f gdigrab -framerate 10 -draw_mouse 1 -offset_x -1920 -offset_y 0 -video_size 1920x1080 -i desktop",
            ]
        );
        assert_eq!(w(Resolved::Monitor { index: 0, rect: None }).len(), 1);
        assert_eq!(w(Resolved::Window { title: "A b - c".into(), xid: None, drawn: None }), ["-f gdigrab -framerate 10 -draw_mouse 1 -i title=A b - c"]);
        let r = input_variants(&Resolved::Window { title: "A b".into(), xid: None, drawn: None }, 10, Os::Windows, "").unwrap();
        assert_eq!(r[0].last().unwrap(), "title=A b"); // one argument, spaces kept
        assert_eq!(
            w(Resolved::Region { x: 10, y: 20, w: 640, h: 480 }),
            ["-f gdigrab -framerate 10 -draw_mouse 1 -offset_x 10 -offset_y 20 -video_size 640x480 -i desktop"]
        );
    }

    #[test]
    fn linux_inputs() {
        let l = |r: Resolved| input_variants(&r, 5, Os::Linux, ":1").unwrap().iter().map(|a| s(a)).collect::<Vec<_>>();
        assert_eq!(l(Resolved::Desktop), ["-f x11grab -framerate 5 -draw_mouse 1 -i :1"]);
        assert_eq!(l(Resolved::Monitor { index: 1, rect: Some((2560, 0, 1920, 1080)) }), ["-f x11grab -framerate 5 -draw_mouse 1 -video_size 1920x1080 -i :1+2560,0"]);
        assert_eq!(l(Resolved::Region { x: 1, y: 2, w: 30, h: 40 }), ["-f x11grab -framerate 5 -draw_mouse 1 -video_size 30x40 -i :1+1,2"]);
        assert_eq!(l(Resolved::Window { title: "t".into(), xid: Some(0x3000007), drawn: None }), ["-f x11grab -framerate 5 -draw_mouse 1 -window_id 0x3000007 -i :1"]);
        assert!(input_variants(&Resolved::Window { title: "t".into(), xid: None, drawn: None }, 5, Os::Linux, ":1").is_err());
        // macOS: AVFoundation screens; a region is a crop that stays first in the filter chain.
        let m = input_variants(&Resolved::Monitor { index: 1, rect: None }, 5, Os::Mac, "").unwrap();
        assert!(m[0].join(" ").contains("-f avfoundation") && m[0].join(" ").contains("Capture screen 1:none"));
        assert!(input_variants(&Resolved::Window { title: "x".into(), xid: None, drawn: None }, 5, Os::Mac, "").is_err());
        let r = input_variants(&Resolved::Region { x: 10, y: 20, w: 640, h: 360 }, 5, Os::Mac, "").unwrap();
        let spec = StartSpec { camera: "c".into(), key: "k".into(), source: Source::Desktop, fps: 5, max_kbps: 1000, max_width: 1280 };
        let args = capture_args(&r[0], &spec, "libx264");
        assert_eq!(args.iter().filter(|a| *a == "-vf").count(), 1);
        assert!(args.join(" ").contains("crop=640:360:10:20,scale="));
        // Windows: a window with a handle tries Windows Graphics Capture first.
        let w = input_variants(&Resolved::Window { title: "Notes".into(), xid: Some(4242), drawn: None }, 5, Os::Windows, "").unwrap();
        assert!(w[0].join(" ").contains("gfxcapture=hwnd=4242"));
        assert!(w[1].join(" ").contains("title=Notes"));
        // An older program on a 150% screen: the fallback keeps its drawing.
        let w = input_variants(&Resolved::Window { title: "Old".into(), xid: Some(7), drawn: Some((96, 144)) }, 5, Os::Windows, "").unwrap();
        assert!(!w[0].join(" ").contains("crop"));
        let args = capture_args(&w[1], &spec, "libx264").join(" ");
        assert!(args.contains("-i title=Old -an -vf crop=trunc(iw*96/144/2)*2:trunc(ih*96/144/2)*2:0:0,scale="), "{args}");
    }

    #[test]
    fn no_device_grabbers_ever() {
        let all = [
            Resolved::Desktop,
            Resolved::Monitor { index: 0, rect: Some((0, 0, 100, 100)) },
            Resolved::Window { title: "x".into(), xid: Some(1), drawn: None },
            Resolved::Region { x: 0, y: 0, w: 2, h: 2 },
        ];
        for os in [Os::Windows, Os::Linux] {
            for r in &all {
                for a in input_variants(r, 10, os, ":0").unwrap() {
                    let j = s(&a);
                    for bad in ["dshow", "v4l2", "avfoundation", "video=", "audio="] {
                        assert!(!j.contains(bad), "{j}");
                    }
                }
            }
        }
    }

    #[test]
    fn encode_args() {
        let input = v(&["-f", "lavfi", "-i", "testsrc2"]);
        let a = s(&capture_args(&input, &spec(Source::Desktop), "libx264"));
        assert_eq!(
            a,
            "-hide_banner -loglevel error -nostdin -f lavfi -i testsrc2 -an -vf scale='trunc(min(1280,iw)/2)*2':-2,format=yuv420p -r 10 -c:v libx264 \
             -preset veryfast -tune zerolatency -crf 23 -maxrate 2500k -bufsize 5000k -g 20 -bf 0 -f flv pipe:1"
        );
        let q = s(&capture_args(&input, &StartSpec { fps: 15, ..spec(Source::Desktop) }, "h264_qsv"));
        assert!(q.contains("format=nv12") && q.contains("-c:v h264_qsv -preset veryfast -b:v 2500k -maxrate 2500k") && q.contains("-g 30"), "{q}");
        let n = s(&capture_args(&input, &spec(Source::Desktop), "h264_nvenc"));
        assert!(n.contains("format=yuv420p") && n.contains("-c:v h264_nvenc"));
        let t = s(&thumb_args(&input, 320));
        assert!(t.ends_with("-frames:v 1 -an -vf scale='trunc(min(320,iw)/2)*2':-2 -c:v mjpeg -q:v 5 -f image2 pipe:1"), "{t}");
    }

    #[tokio::test]
    async fn thumbnail_from_test_pattern() {
        // Needs ffmpeg on this machine (skipped where it is not installed).
        if std::process::Command::new(super::ffmpeg_path()).arg("-version").output().is_err() {
            eprintln!("ffmpeg not installed: skipping");
            return;
        }
        // A synthetic source only: no screen or device is opened.
        let input = v(&["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=5"]);
        let jpeg = grab_jpeg(&[v(&["-f", "lavfi", "-i", "nosuchfilter"]), input], 200).await.unwrap();
        assert!(jpeg.starts_with(&[0xff, 0xd8]));
        let img = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (200, 112));
        assert!(grab_jpeg(&[v(&["-f", "lavfi", "-i", "nosuchfilter"])], 200).await.is_err());
    }
}
