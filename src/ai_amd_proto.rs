//! How AmberSight talks to its AMD graphics helper (`ambersight-ai-amd`,
//! tools/ai-amd), a separate program because the AI runtime that drives AMD
//! graphics (ONNX Runtime with WebGPU over Vulkan) cannot also run the
//! NVIDIA (CUDA) or Intel (OpenVINO) add-ons, so it cannot be AmberSight's
//! own runtime. The helper is started by AmberSight with its stdin and
//! stdout as pipes; stderr carries its log lines.
//!
//! Standard library only: the helper includes this file as it is.
//!
//! Every message is a frame: one byte saying what it is, its length in
//! bytes (u32, little-endian, at most `MAX_FRAME`), then that many bytes.
//!
//! 1. The helper starts and sends `HELLO`: text fields `protocol` (must be
//!    `PROTOCOL`) and `version` (AmberSight's version it was built with; the
//!    server only uses a helper of its own version).
//! 2. The server sends `LOAD`: field `model` (the .onnx file's full path).
//!    The helper answers `READY` (fields `input`: the model's square input
//!    size, `adapter`: the graphics chip as the driver names it, `load_ms`)
//!    or `ERROR` (text), after which it exits.
//! 3. Then any number of `CHECK` frames (one input tensor), each answered by
//!    `OUTPUT` (the model's first output tensor) or `ERROR` (text; the
//!    helper keeps going and the server decides what to do).
//! 4. The server ends it with `QUIT`, or by closing stdin, or by killing it
//!    (a check that takes too long).

use std::io::{self, Read, Write};

/// The protocol's version: both sides must have the same one.
pub const PROTOCOL: u32 = 1;
/// The largest frame either side sends or accepts (an input tensor of
/// 1x3x1280x1280 f32 is under 20 MB).
pub const MAX_FRAME: usize = 64 << 20;
/// Tensors have at most this many dimensions.
pub const MAX_DIMS: usize = 8;

pub const HELLO: u8 = b'H';
pub const LOAD: u8 = b'L';
pub const READY: u8 = b'R';
pub const CHECK: u8 = b'C';
pub const OUTPUT: u8 = b'O';
pub const ERROR: u8 = b'E';
pub const QUIT: u8 = b'Q';

/// Send one frame.
pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one frame: (kind, payload). A frame over `MAX_FRAME` is an error
/// (nothing of it is read past its header).
pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes is too large")));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

/// Text fields: one `key=value` per line (values have no newlines; a
/// newline in one is replaced by a space).
pub fn fields(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut s = String::new();
    for (k, v) in pairs {
        s.push_str(k);
        s.push('=');
        s.push_str(&v.replace(['\n', '\r'], " "));
        s.push('\n');
    }
    s.into_bytes()
}

/// A field's value from `fields`' text (None: not there, or not UTF-8).
pub fn field<'a>(payload: &'a [u8], key: &str) -> Option<&'a str> {
    std::str::from_utf8(payload).ok()?.lines().find_map(|l| l.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

/// A tensor: u32 number of dimensions, each as u64, then the f32 values
/// (all little-endian).
pub fn encode_tensor(shape: &[usize], data: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + shape.len() * 8 + data.len() * 4);
    out.extend_from_slice(&(shape.len() as u32).to_le_bytes());
    for d in shape {
        out.extend_from_slice(&(*d as u64).to_le_bytes());
    }
    for v in data {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// The size of `encode_tensor(shape, ..)` for f32 values (None: it overflows).
pub fn tensor_len(shape: &[usize]) -> Option<usize> {
    let count = shape.iter().try_fold(1usize, |a, d| a.checked_mul(*d))?;
    count.checked_mul(4)?.checked_add(4 + shape.len() * 8)
}

/// `encode_tensor` back: (shape, values). Refuses more than `MAX_DIMS`
/// dimensions, a dimension of 0 (an empty tensor is no answer, and a zero
/// beside a huge dimension hides the product from the callers' checks) and a
/// value count that does not match the shape.
pub fn decode_tensor(b: &[u8]) -> Result<(Vec<usize>, Vec<f32>), String> {
    let n = b.get(..4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as usize).ok_or("tensor: no header")?;
    if n == 0 || n > MAX_DIMS {
        return Err(format!("tensor: {n} dimensions"));
    }
    let dims_end = 4 + n * 8;
    let dims = b.get(4..dims_end).ok_or("tensor: dimensions cut short")?;
    let mut shape = Vec::with_capacity(n);
    let mut count: usize = 1;
    for c in dims.chunks_exact(8) {
        let d = u64::from_le_bytes(c.try_into().unwrap());
        let d = usize::try_from(d).map_err(|_| "tensor: dimension too large")?;
        if d == 0 {
            return Err("tensor: a dimension of 0".into());
        }
        count = count.checked_mul(d).ok_or("tensor: too many values")?;
        shape.push(d);
    }
    let rest = &b[dims_end..];
    if count.checked_mul(4) != Some(rest.len()) {
        return Err(format!("tensor: {} bytes of values for {count} values", rest.len()));
    }
    let data = rest.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
    Ok((shape, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_go_both_ways_and_oversized_ones_are_refused() {
        let mut buf = Vec::new();
        write_frame(&mut buf, HELLO, &fields(&[("protocol", "1"), ("version", "0.13.5")])).unwrap();
        write_frame(&mut buf, QUIT, &[]).unwrap();
        let mut r = &buf[..];
        let (k, p) = read_frame(&mut r).unwrap();
        assert_eq!((k, field(&p, "protocol"), field(&p, "version"), field(&p, "nope")), (HELLO, Some("1"), Some("0.13.5"), None));
        assert_eq!(read_frame(&mut r).unwrap(), (QUIT, vec![]));
        assert!(read_frame(&mut r).is_err());
        // A header claiming more than MAX_FRAME is refused before any allocation.
        let mut big = vec![CHECK];
        big.extend_from_slice(&((MAX_FRAME as u32) + 1).to_le_bytes());
        assert!(read_frame(&mut &big[..]).is_err());
        assert!(write_frame(&mut Vec::new(), CHECK, &vec![0; MAX_FRAME + 1]).is_err());
    }

    #[test]
    fn a_newline_cannot_add_a_field() {
        let p = fields(&[("adapter", "AMD\nprotocol=9")]);
        assert_eq!(field(&p, "adapter"), Some("AMD protocol=9"));
        assert_eq!(field(&p, "protocol"), None);
    }

    #[test]
    fn tensors_round_trip_and_bad_ones_are_refused() {
        let data: Vec<f32> = (0..24).map(|i| i as f32 * 0.5).collect();
        let b = encode_tensor(&[1, 2, 3, 4], &data);
        assert_eq!(decode_tensor(&b).unwrap(), (vec![1, 2, 3, 4], data));
        assert!(decode_tensor(&b[..b.len() - 1]).is_err());
        assert!(decode_tensor(&encode_tensor(&[2, 2], &[1.0])).is_err());
        assert!(decode_tensor(&encode_tensor(&[], &[])).is_err());
        assert!(decode_tensor(&encode_tensor(&[1; 9], &[1.0])).is_err());
        // Dimensions whose product overflows are refused, not wrapped.
        let mut huge = 2u32.to_le_bytes().to_vec();
        huge.extend_from_slice(&u64::MAX.to_le_bytes());
        huge.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_tensor(&huge).is_err());
        assert!(decode_tensor(&[]).is_err());
        // A zero dimension is refused, also beside a huge one with no data.
        assert!(decode_tensor(&encode_tensor(&[0, 3], &[])).is_err());
        let mut zero = 3u32.to_le_bytes().to_vec();
        for d in [0u64, 1 << 61, 8] {
            zero.extend_from_slice(&d.to_le_bytes());
        }
        assert!(decode_tensor(&zero).is_err());
        // tensor_len is the encoded size.
        assert_eq!(tensor_len(&[1, 2, 3, 4]), Some(encode_tensor(&[1, 2, 3, 4], &[0.0; 24]).len()));
        assert_eq!(tensor_len(&[usize::MAX, 2]), None);
    }
}
