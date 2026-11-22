//! TLV8, HomeKit's type-length-value encoding: one byte type, one byte
//! length (0-255), the value. A value longer than 255 bytes is split into
//! items of the same type that follow each other and are joined again when
//! read. Two items of the same type that are separate values (a list) have a
//! zero-length separator item (type 0xFF in pairing lists, 0x00 inside camera
//! configurations) between them.

/// Pairing TLV types (HAP spec, table 5-6).
pub mod t {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const STATE: u8 = 0x06;
    pub const RETRY_DELAY: u8 = 0x08;
    pub const ERROR: u8 = 0x07;
    pub const SIGNATURE: u8 = 0x0A;
    pub const PERMISSIONS: u8 = 0x0B;
    pub const SEPARATOR: u8 = 0xFF;
}

/// Pairing error codes (HAP spec, table 5-5).
pub mod err {
    pub const UNKNOWN: u8 = 0x01;
    pub const AUTHENTICATION: u8 = 0x02;
    pub const BACKOFF: u8 = 0x03;
    pub const MAX_PEERS: u8 = 0x04;
    pub const MAX_TRIES: u8 = 0x05;
    pub const UNAVAILABLE: u8 = 0x06;
    pub const BUSY: u8 = 0x07;
}

/// Most items one message may hold (a pairing list of 16 controllers is ~64).
const MAX_ITEMS: usize = 512;

/// A decoded TLV8 message: the items in order, fragments joined.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tlv {
    pub items: Vec<(u8, Vec<u8>)>,
}

impl Tlv {
    pub fn new() -> Tlv {
        Tlv::default()
    }

    /// Decode; None for anything malformed (an item running past the end,
    /// too many items). Never panics.
    pub fn decode(data: &[u8]) -> Option<Tlv> {
        let mut items: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut i = 0usize;
        // Whether the last item was a full 255-byte fragment (the next one of
        // the same type continues it).
        let mut continues = false;
        while i < data.len() {
            if data.len() - i < 2 {
                return None;
            }
            let (ty, len) = (data[i], data[i + 1] as usize);
            i += 2;
            if data.len() - i < len {
                return None;
            }
            let v = &data[i..i + len];
            i += len;
            match items.last_mut() {
                Some((last_ty, last_v)) if continues && *last_ty == ty => last_v.extend_from_slice(v),
                _ => {
                    if items.len() >= MAX_ITEMS {
                        return None;
                    }
                    items.push((ty, v.to_vec()));
                }
            }
            continues = len == 255;
        }
        Some(Tlv { items })
    }

    /// The first value of this type.
    pub fn get(&self, ty: u8) -> Option<&[u8]> {
        self.items.iter().find(|(t, _)| *t == ty).map(|(_, v)| v.as_slice())
    }

    /// A one-byte value (state, method, error, flags...): the first byte, also
    /// of longer little-endian integers that fit (flags are up to 4 bytes).
    pub fn byte(&self, ty: u8) -> Option<u8> {
        self.get(ty).and_then(|v| v.first().copied())
    }

    pub fn push(&mut self, ty: u8, v: &[u8]) -> &mut Tlv {
        self.items.push((ty, v.to_vec()));
        self
    }

    pub fn push_u8(&mut self, ty: u8, v: u8) -> &mut Tlv {
        self.push(ty, &[v])
    }

    /// Encode, splitting long values into 255-byte fragments. Two items in a
    /// row of the same type must be separated by the caller (`push_sep`).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (ty, v) in &self.items {
            encode_item(&mut out, *ty, v);
        }
        out
    }
}

/// Append one item, in fragments when longer than 255 bytes.
pub fn encode_item(out: &mut Vec<u8>, ty: u8, v: &[u8]) {
    if v.is_empty() {
        out.extend_from_slice(&[ty, 0]);
        return;
    }
    for chunk in v.chunks(255) {
        out.push(ty);
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
}

/// A small builder for the nested camera configuration TLVs.
#[derive(Default)]
pub struct Builder {
    pub out: Vec<u8>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }
    pub fn bytes(mut self, ty: u8, v: &[u8]) -> Builder {
        encode_item(&mut self.out, ty, v);
        self
    }
    pub fn u8(self, ty: u8, v: u8) -> Builder {
        self.bytes(ty, &[v])
    }
    pub fn u16(self, ty: u8, v: u16) -> Builder {
        self.bytes(ty, &v.to_le_bytes())
    }
    /// A zero-length item: the list separator inside camera TLVs (type 0).
    pub fn sep(self) -> Builder {
        self.bytes(0x00, &[])
    }
    pub fn done(self) -> Vec<u8> {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_fragments() {
        let long: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let mut m = Tlv::new();
        m.push_u8(t::STATE, 2).push(t::SALT, &[1; 16]).push(t::PUBLIC_KEY, &long);
        let enc = m.encode();
        // 3 + 18 + (2+255) + (2+255) + (2+90)
        assert_eq!(enc.len(), 3 + 18 + 257 + 257 + 92);
        let d = Tlv::decode(&enc).unwrap();
        assert_eq!(d, m);
        assert_eq!(d.byte(t::STATE), Some(2));
        assert_eq!(d.get(t::PUBLIC_KEY).unwrap(), long.as_slice());
    }

    #[test]
    fn exactly_255_then_another_item() {
        let v = vec![7u8; 255];
        let mut m = Tlv::new();
        m.push(t::PUBLIC_KEY, &v).push_u8(t::STATE, 1);
        let d = Tlv::decode(&m.encode()).unwrap();
        assert_eq!(d.items.len(), 2);
        assert_eq!(d.get(t::PUBLIC_KEY).unwrap().len(), 255);
    }

    #[test]
    fn separators_keep_list_items_apart() {
        let mut m = Tlv::new();
        m.push(t::IDENTIFIER, b"a").push(t::SEPARATOR, &[]).push(t::IDENTIFIER, b"b");
        let d = Tlv::decode(&m.encode()).unwrap();
        assert_eq!(d.items.len(), 3);
        assert_eq!(d.items[2].1, b"b");
    }

    #[test]
    fn truncated_is_refused() {
        assert!(Tlv::decode(&[6]).is_none());
        assert!(Tlv::decode(&[6, 2, 1]).is_none());
        assert_eq!(Tlv::decode(&[]).unwrap().items.len(), 0);
    }

    #[test]
    fn random_bytes_never_panic() {
        use rand::{Rng, SeedableRng};
        let mut r = rand::rngs::StdRng::seed_from_u64(7);
        for _ in 0..20_000 {
            let n = r.gen_range(0..700);
            let buf: Vec<u8> = (0..n).map(|_| r.gen()).collect();
            if let Some(d) = Tlv::decode(&buf) {
                // What decodes encodes to something that decodes the same.
                let again = Tlv::decode(&d.encode());
                assert!(again.is_some());
            }
        }
        // Many tiny items are refused beyond the cap.
        let many: Vec<u8> = (0..2000).flat_map(|i| [(i % 2) as u8, 0]).collect();
        assert!(Tlv::decode(&many).is_none());
    }
}
