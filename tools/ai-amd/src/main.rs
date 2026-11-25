//! ambersight-ai-amd: runs AmberSight's AI model on AMD graphics (ONNX
//! Runtime with WebGPU over Vulkan). Started by AmberSight; speaks the
//! protocol in src/ai_amd_proto.rs on stdin/stdout, logs to stderr.
#[path = "../../../src/ai_amd_proto.rs"]
#[allow(dead_code)]
mod proto;

use proto::*;
use std::io::{Read, Write};
use std::time::Instant;

const VERSION: &str = env!("AMBERSIGHT_VERSION");

type Tensor = (Vec<usize>, Vec<f32>);

/// A loaded model (the real one runs on the graphics card; tests use a fake).
trait Backend {
    fn input_size(&self) -> usize;
    fn adapter(&self) -> String;
    fn run(&mut self, input: &[f32], size: usize) -> Result<Tensor, String>;
}

fn log(s: &str) {
    eprintln!("ambersight-ai-amd: {s}");
}

type Loader<'a> = &'a mut dyn FnMut(&str) -> Result<Box<dyn Backend>, String>;

/// The frame loop. Returns the exit code.
fn serve<R: Read, W: Write>(r: &mut R, w: &mut W, load: Loader) -> i32 {
    let hello = fields(&[("protocol", &PROTOCOL.to_string()), ("version", VERSION)]);
    if write_frame(w, HELLO, &hello).is_err() {
        return 1;
    }
    let mut model: Option<Box<dyn Backend>> = None;
    let mut loaded_once = false;
    loop {
        let (kind, payload) = match read_frame(r) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return 0,
            Err(e) => {
                log(&format!("read: {e}"));
                let _ = write_frame(w, ERROR, format!("read: {e}").as_bytes());
                return 1;
            }
        };
        match kind {
            QUIT => return 0,
            LOAD => {
                if loaded_once {
                    let _ = write_frame(w, ERROR, b"a model is already loaded");
                    return 1;
                }
                loaded_once = true;
                let Some(path) = field(&payload, "model") else {
                    let _ = write_frame(w, ERROR, b"LOAD has no model field");
                    return 1;
                };
                let t = Instant::now();
                match load(path) {
                    Ok(m) => {
                        let ms = t.elapsed().as_millis().to_string();
                        let ready = fields(&[("input", &m.input_size().to_string()), ("adapter", &m.adapter()), ("load_ms", &ms)]);
                        log(&format!("model ready in {ms} ms on {}", m.adapter()));
                        model = Some(m);
                        if write_frame(w, READY, &ready).is_err() {
                            return 1;
                        }
                    }
                    Err(e) => {
                        log(&format!("load failed: {e}"));
                        let _ = write_frame(w, ERROR, e.as_bytes());
                        return 1;
                    }
                }
            }
            CHECK => {
                let Some(m) = model.as_mut() else {
                    if write_frame(w, ERROR, b"no model is loaded").is_err() {
                        return 1;
                    }
                    continue;
                };
                let s = m.input_size();
                let reply = match decode_tensor(&payload) {
                    Err(e) => Err(e),
                    Ok((shape, data)) => {
                        if shape != [1, 3, s, s] {
                            Err(format!("input must be 1x3x{s}x{s}, got {shape:?}"))
                        } else {
                            m.run(&data, s)
                        }
                    }
                };
                let sent = match reply {
                    Ok((shape, data)) => write_frame(w, OUTPUT, &encode_tensor(&shape, &data)),
                    Err(e) => {
                        log(&format!("check failed: {e}"));
                        write_frame(w, ERROR, e.as_bytes())
                    }
                };
                if sent.is_err() {
                    return 1;
                }
            }
            other => {
                log(&format!("unexpected frame {other}"));
                let _ = write_frame(w, ERROR, format!("unexpected frame kind {other}").as_bytes());
                return 1;
            }
        }
    }
}

struct Gpu {
    session: ort::session::Session,
    size: usize,
}

impl Backend for Gpu {
    fn input_size(&self) -> usize {
        self.size
    }
    fn adapter(&self) -> String {
        // ONNX Runtime does not hand out Dawn's adapter name.
        "AMD graphics (WebGPU)".into()
    }
    fn run(&mut self, input: &[f32], size: usize) -> Result<Tensor, String> {
        let arr = ndarray::Array4::from_shape_vec((1, 3, size, size), input.to_vec()).map_err(|e| e.to_string())?;
        let t = ort::value::Tensor::from_array(arr).map_err(|e| e.to_string())?;
        let o = self.session.run(ort::inputs![t]).map_err(|e| e.to_string())?;
        let v = o[0].try_extract_array::<f32>().map_err(|e| e.to_string())?;
        Ok((v.shape().to_vec(), v.iter().copied().collect()))
    }
}

fn load_gpu(path: &str) -> Result<Box<dyn Backend>, String> {
    let env = ort::init().with_name("ambersight-ai-amd").build().map_err(|e| format!("AI runtime: {e}"))?;
    let session = ort::session::Session::builder(&env)
        .map_err(|e| e.to_string())?
        .with_intra_threads(2)
        .map_err(|e| e.to_string())?
        .with_execution_providers([ort::ep::WebGPU::default().build().error_on_failure()])
        .map_err(|e| format!("the graphics card could not be used (WebGPU over Vulkan): {e}"))?
        .commit_from_file(path)
        .map_err(|e| format!("could not load the model on the graphics card: {e}"))?;
    let size = match session.inputs().first().map(|i| i.dtype()) {
        Some(ort::value::ValueType::Tensor { shape, .. }) => shape.get(2).copied().filter(|d| *d > 0).unwrap_or(640) as usize,
        _ => 640,
    };
    Ok(Box::new(Gpu { session, size }))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("{VERSION}");
        return;
    }
    if !args.is_empty() {
        eprintln!("ambersight-ai-amd is started by AmberSight (only --version is accepted)");
        std::process::exit(2);
    }
    unsafe {
        let parent = libc::getppid();
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0);
        // The parent may have gone before the signal was set up.
        if libc::getppid() != parent || parent == 1 {
            std::process::exit(1);
        }
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let code = serve(&mut stdin.lock(), &mut stdout.lock(), &mut load_gpu);
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;
    impl Backend for Fake {
        fn input_size(&self) -> usize {
            4
        }
        fn adapter(&self) -> String {
            "fake".into()
        }
        fn run(&mut self, input: &[f32], _: usize) -> Result<Tensor, String> {
            Ok((vec![1, 2], vec![input[0], input.len() as f32]))
        }
    }

    fn session(frames: &[(u8, Vec<u8>)]) -> (i32, Vec<(u8, Vec<u8>)>) {
        let mut input = Vec::new();
        for (k, p) in frames {
            write_frame(&mut input, *k, p).unwrap();
        }
        let mut out = Vec::new();
        let code = serve(&mut &input[..], &mut out, &mut |p| if p == "bad" { Err("nope".into()) } else { Ok(Box::new(Fake)) });
        let mut r = &out[..];
        let mut got = vec![];
        while let Ok(f) = read_frame(&mut r) {
            got.push(f);
        }
        (code, got)
    }

    fn load() -> (u8, Vec<u8>) {
        (LOAD, fields(&[("model", "m.onnx")]))
    }
    fn check(shape: &[usize]) -> (u8, Vec<u8>) {
        (CHECK, encode_tensor(shape, &vec![0.5; shape.iter().product()]))
    }

    #[test]
    fn hello_first_then_eof_exits_clean() {
        let (code, got) = session(&[]);
        assert_eq!(code, 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, HELLO);
        assert_eq!(field(&got[0].1, "protocol"), Some("1"));
        assert_eq!(field(&got[0].1, "version"), Some(VERSION));
    }

    #[test]
    fn load_check_quit() {
        let (code, got) = session(&[load(), check(&[1, 3, 4, 4]), (QUIT, vec![]), check(&[1, 3, 4, 4])]);
        assert_eq!(code, 0);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].0, READY);
        assert_eq!(field(&got[1].1, "input"), Some("4"));
        assert_eq!(got[2].0, OUTPUT);
        assert_eq!(decode_tensor(&got[2].1).unwrap(), (vec![1, 2], vec![0.5, 48.0]));
    }

    #[test]
    fn check_before_load_is_refused_and_it_keeps_going() {
        let (code, got) = session(&[check(&[1, 3, 4, 4]), load(), check(&[1, 3, 4, 4])]);
        assert_eq!(code, 0);
        assert_eq!(got[1].0, ERROR);
        assert_eq!(got[2].0, READY);
        assert_eq!(got[3].0, OUTPUT);
    }

    #[test]
    fn wrong_shape_is_refused_and_it_keeps_going() {
        let (code, got) = session(&[load(), check(&[1, 3, 5, 5]), check(&[3, 4, 4]), (CHECK, vec![1, 2]), check(&[1, 3, 4, 4])]);
        assert_eq!(code, 0);
        assert_eq!(got.iter().map(|f| f.0).collect::<Vec<_>>(), vec![HELLO, READY, ERROR, ERROR, ERROR, OUTPUT]);
    }

    #[test]
    fn load_failure_second_load_and_unknown_frames_exit_nonzero() {
        let (code, got) = session(&[(LOAD, fields(&[("model", "bad")])), check(&[1, 3, 4, 4])]);
        assert_eq!((code, got.len(), got[1].0), (1, 2, ERROR));
        assert_eq!(&got[1].1[..], b"nope");
        let (code, got) = session(&[load(), load()]);
        assert_eq!((code, got[2].0), (1, ERROR));
        let (code, got) = session(&[(b'Z', vec![])]);
        assert_eq!((code, got[1].0), (1, ERROR));
        let (code, _) = session(&[(LOAD, vec![])]);
        assert_eq!(code, 1);
    }
}
