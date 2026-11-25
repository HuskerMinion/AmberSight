//! A stand-in for `ambersight-ai-amd` for the tests of src/ai_amd.rs: it
//! speaks the same protocol (src/ai_amd_proto.rs) and needs no graphics
//! chip. The tests of ai_amd.rs compile it with rustc (it is not a cargo
//! target: `cargo test` does not build examples for this program) and start
//! it with these arguments:
//!
//!   --version=X         say this version in HELLO (default: this build's)
//!   --protocol=N        say this protocol in HELLO
//!   --delay-ms=N        every check takes this long
//!   --pidfile=PATH      write its process id there at start
//!   --quitfile=PATH     write that file when it is told QUIT
//!   --stderr-lines=N    write this many lines to stderr at start
//!   --input=N           the model's input size in READY (default 32)
//!   --refuse-if=PATH    exit before HELLO when that file is there
//!   --slow-load-if=PATH:MS   take MS ms to load the model when that file is there
//!   --mode=stderr-junk  at start write text that is not UTF-8 and a long line
//!                       without a newline to stderr, then work normally
//!   --mode=stall-after-ready   answer LOAD with READY, then never read again
//!   --mode=load-error   answer LOAD with ERROR
//!   --mode=crash-at-start   exit before HELLO
//!   --mode=silent       say nothing, ever
//!   --mode=hang-on:K    never answer check number K (1 is the first)
//!   --mode=crash-on:K   exit during check number K
//!   --mode=error-on:K   answer check number K with ERROR
#![allow(dead_code)]

#[path = "../ai_amd_proto.rs"]
mod proto;

use std::io::Write;
use std::time::Duration;

fn main() {
    let mut version = env!("CARGO_PKG_VERSION").to_string();
    let mut protocol = proto::PROTOCOL.to_string();
    let (mut delay, mut mode, mut pidfile, mut quitfile, mut lines) = (0u64, String::new(), None, None, 0u32);
    let mut input_size = "32".to_string();
    let mut refuse_if: Option<String> = None;
    let mut slow_load: Option<(String, u64)> = None;
    for a in std::env::args().skip(1) {
        match a.split_once('=') {
            Some(("--version", v)) => version = v.into(),
            Some(("--protocol", v)) => protocol = v.into(),
            Some(("--delay-ms", v)) => delay = v.parse().unwrap(),
            Some(("--mode", v)) => mode = v.into(),
            Some(("--pidfile", v)) => pidfile = Some(v.to_string()),
            Some(("--quitfile", v)) => quitfile = Some(v.to_string()),
            Some(("--stderr-lines", v)) => lines = v.parse().unwrap(),
            Some(("--input", v)) => input_size = v.into(),
            Some(("--refuse-if", v)) => refuse_if = Some(v.to_string()),
            Some(("--slow-load-if", v)) => {
                let (p, ms) = v.rsplit_once(':').unwrap();
                slow_load = Some((p.to_string(), ms.parse().unwrap()));
            }
            _ => {}
        }
    }
    if let Some(p) = &pidfile {
        std::fs::write(p, std::process::id().to_string()).unwrap();
    }
    if refuse_if.as_ref().is_some_and(|p| std::path::Path::new(p).exists()) {
        std::process::exit(5);
    }
    for i in 0..lines {
        eprintln!("fake helper log line {i}");
    }
    if mode == "stderr-junk" {
        let mut e = std::io::stderr().lock();
        e.write_all(b"bad bytes \xff\xfe\xc3\n").unwrap();
        e.write_all(&vec![b'z'; 2 << 20]).unwrap();
        e.write_all(b"\nafter the long line\n").unwrap();
        e.write_all(&vec![b'y'; 1 << 20]).unwrap();
    }
    let at = |name: &str| mode.strip_prefix(name).and_then(|k| k.strip_prefix(':')).and_then(|k| k.parse::<u32>().ok());
    match mode.as_str() {
        "crash-at-start" => std::process::exit(3),
        "silent" => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        _ => {}
    }
    let mut out = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    proto::write_frame(&mut out, proto::HELLO, &proto::fields(&[("protocol", &protocol), ("version", &version)])).unwrap();
    let mut checks = 0u32;
    loop {
        let Ok((kind, payload)) = proto::read_frame(&mut input) else { return };
        match kind {
            proto::LOAD => {
                if let Some((p, ms)) = &slow_load {
                    if std::path::Path::new(p).exists() {
                        std::thread::sleep(Duration::from_millis(*ms));
                    }
                }
                if mode == "load-error" {
                    proto::write_frame(&mut out, proto::ERROR, b"the model needs a newer WebGPU").unwrap();
                    return;
                }
                // Like the real helper, which runs in "/": only a whole path finds the model.
                if !proto::field(&payload, "model").is_some_and(|m| m.starts_with('/')) {
                    proto::write_frame(&mut out, proto::ERROR, b"File at the model's path does not exist").unwrap();
                    return;
                }
                proto::write_frame(&mut out, proto::READY, &proto::fields(&[("input", &input_size), ("adapter", "Fake AMD (test)"), ("load_ms", "1")])).unwrap();
                if mode == "stall-after-ready" {
                    out.flush().unwrap();
                    loop {
                        std::thread::sleep(Duration::from_secs(3600));
                    }
                }
            }
            proto::CHECK => {
                checks += 1;
                if at("hang-on") == Some(checks) {
                    loop {
                        std::thread::sleep(Duration::from_secs(3600));
                    }
                }
                if at("crash-on") == Some(checks) {
                    std::process::exit(4);
                }
                std::thread::sleep(Duration::from_millis(delay));
                let (shape, data) = proto::decode_tensor(&payload).unwrap();
                if at("error-on") == Some(checks) {
                    proto::write_frame(&mut out, proto::ERROR, b"device lost (test)").unwrap();
                } else {
                    let sum: f32 = data.iter().sum();
                    proto::write_frame(&mut out, proto::OUTPUT, &proto::encode_tensor(&[1, 2], &[shape.len() as f32, sum])).unwrap();
                }
            }
            proto::QUIT => {
                if let Some(q) = &quitfile {
                    std::fs::write(q, "quit").unwrap();
                }
                out.flush().unwrap();
                return;
            }
            _ => return,
        }
    }
}
