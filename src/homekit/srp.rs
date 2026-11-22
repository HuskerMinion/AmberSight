//! SRP-6a, the accessory (server) side, as HomeKit uses it for pair-setup:
//! the 3072-bit group of RFC 5054 with g = 5, SHA-512, user name
//! "Pair-Setup", the setup code ("123-45-678") as the password.
//!
//!   k  = H(N | PAD(g))            x = H(s | H(I ":" P))      v = g^x
//!   B  = k*v + g^b                u = H(PAD(A) | PAD(B))
//!   S  = (A * v^u)^b              K = H(PAD(S))
//!   M1 = H(H(N) xor H(g) | H(I) | s | PAD(A) | PAD(B) | K)
//!   M2 = H(PAD(A) | M1 | K)
//!
//! (PAD: left-padded with zeros to the 384 bytes of N.) The modular
//! arithmetic is crypto-bigint's constant-time Montgomery code (0.7: 0.5's
//! constant-time code leaked timing, RUSTSEC-2026-0387). Best effort: our
//! own copies of the secrets (b, v, the shared S and the key) are wiped
//! when they are no longer needed; crypto-bigint's and sha2's temporaries
//! and stack copies are not.
//! Checked against the HAP specification's test vector, as reproduced by
//! the aiohomekit controller (see the tests).

use crypto_bigint::modular::{FixedMontyForm, FixedMontyParams};
use crypto_bigint::{Odd, U256, U3072, U512};
use sha2::{Digest, Sha512};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// Bytes of N.
pub const LEN: usize = 384;

const N_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";
const G: u8 = 5;
pub const USER: &str = "Pair-Setup";

/// A number mod N, in Montgomery form.
pub(super) type Mod = FixedMontyForm<{ U3072::LIMBS }>;

pub(super) fn n() -> U3072 {
    U3072::from_be_hex(N_HEX)
}

pub(super) fn params() -> FixedMontyParams<{ U3072::LIMBS }> {
    // (N is public: computing its parameters need not be constant-time.)
    FixedMontyParams::new_vartime(Odd::new(n()).expect("N is odd"))
}

/// `x` mod N.
pub(super) fn m(x: &U3072, p: &FixedMontyParams<{ U3072::LIMBS }>) -> Mod {
    Mod::new(x, p)
}

/// The 384 big-endian bytes of `x`.
pub(super) fn bytes(x: &U3072) -> [u8; LEN] {
    let mut e = x.to_be_bytes();
    let mut out = [0u8; LEN];
    out.copy_from_slice(e.as_ref());
    e.zeroize();
    out
}

pub(super) fn h(parts: &[&[u8]]) -> [u8; 64] {
    let mut d = Sha512::new();
    for p in parts {
        d.update(p);
    }
    d.finalize().into()
}

/// Left-pad to `len` bytes (None when longer).
fn pad(v: &[u8], len: usize) -> Option<Vec<u8>> {
    if v.len() > len {
        return None;
    }
    let mut out = vec![0u8; len - v.len()];
    out.extend_from_slice(v);
    Some(out)
}

fn big(v: &[u8]) -> Option<U3072> {
    pad(v, LEN).map(|p| U3072::from_be_slice(&p))
}

pub(super) fn hash_to_u512(d: &[u8; 64]) -> U512 {
    U512::from_be_slice(d)
}

pub(super) fn k() -> U3072 {
    let mut g = [0u8; LEN];
    g[LEN - 1] = G;
    big(&h(&[&bytes(&n()), &g])).unwrap()
}

/// What the accessory keeps between M2 and M4 of pair-setup (b and the
/// verifier v are wiped when it is dropped).
pub struct Server {
    salt: [u8; 16],
    v: U3072,
    b: U256,
    b_pub: [u8; LEN],
}

impl Drop for Server {
    fn drop(&mut self) {
        self.v.zeroize();
        self.b.zeroize();
    }
}

/// The result of a good proof: the session key K and the accessory's proof.
/// (The key is wiped when this is dropped; the holder wipes its copy.)
pub struct Verified {
    pub key: [u8; 64],
    pub proof: [u8; 64],
}

impl Drop for Verified {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl Server {
    /// Start a pair-setup with this setup code, salt and private value b
    /// (random in use; fixed in the tests).
    pub fn new(code: &str, salt: [u8; 16], b: [u8; 32]) -> Server {
        Server::new_as(USER, code, salt, b)
    }

    /// The same for another user name (HomeKit's is always "Pair-Setup"; the
    /// specification's test vector uses "alice").
    fn new_as(user: &str, code: &str, salt: [u8; 16], b: [u8; 32]) -> Server {
        let p = params();
        let mut inner = h(&[user.as_bytes(), b":", code.as_bytes()]);
        let mut xh = h(&[&salt, &inner]);
        let mut x = hash_to_u512(&xh);
        inner.zeroize();
        xh.zeroize();
        let g = m(&U3072::from_u8(G), &p);
        let mut v = g.pow(&x);
        x.zeroize();
        let mut b = b;
        let bb = U256::from_be_slice(&b);
        b.zeroize();
        let mut gb = g.pow(&bb);
        let b_pub = bytes(&(m(&k(), &p) * &v + &gb).retrieve());
        gb.zeroize();
        let s = Server { salt, v: v.retrieve(), b: bb, b_pub };
        v.zeroize();
        s
    }

    /// A new pair-setup with a random salt and b.
    pub fn random(code: &str) -> Server {
        use rand::RngCore;
        let mut salt = [0u8; 16];
        let mut b = [0u8; 32];
        let mut r = rand::rngs::OsRng;
        r.fill_bytes(&mut salt);
        r.fill_bytes(&mut b);
        Server::new(code, salt, b)
    }

    pub fn salt(&self) -> &[u8; 16] {
        &self.salt
    }

    /// B, 384 bytes.
    pub fn public(&self) -> &[u8; LEN] {
        &self.b_pub
    }

    /// Check the controller's A and proof M1; Some(K, M2) when they are
    /// right. A of 0 (mod N), a u of 0, or anything malformed: None.
    pub fn verify(&self, a_pub: &[u8], m1: &[u8]) -> Option<Verified> {
        self.verify_as(USER, a_pub, m1)
    }

    fn verify_as(&self, user: &str, a_pub: &[u8], m1: &[u8]) -> Option<Verified> {
        let a_pad = pad(a_pub, LEN)?;
        let a = U3072::from_be_slice(&a_pad);
        let n = n();
        if a >= n || a == U3072::ZERO {
            return None;
        }
        let u_hash = h(&[&a_pad, &self.b_pub]);
        if u_hash.iter().all(|b| *b == 0) {
            return None;
        }
        let u = hash_to_u512(&u_hash);
        let p = params();
        let mut v = m(&self.v, &p);
        let mut vu = v.pow(&u);
        let mut sm = (m(&a, &p) * &vu).pow(&self.b);
        let mut s = sm.retrieve();
        let mut s_bytes = bytes(&s);
        let mut key = h(&[&s_bytes]);
        s_bytes.zeroize();
        s.zeroize();
        sm.zeroize();
        vu.zeroize();
        v.zeroize();
        let hn = h(&[&bytes(&n)]);
        let hg = h(&[&[G]]);
        let mut x = [0u8; 64];
        for i in 0..64 {
            x[i] = hn[i] ^ hg[i];
        }
        let hu = h(&[user.as_bytes()]);
        let want = h(&[&x, &hu, &self.salt, &a_pad, &self.b_pub, &key]);
        if m1.len() != 64 || !bool::from(want.ct_eq(m1)) {
            key.zeroize();
            return None;
        }
        let proof = h(&[&a_pad, &want, &key]);
        let out = Verified { key, proof };
        key.zeroize();
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // The HAP specification's SRP test vector (user "alice", password
    // "password123", the RFC 5054 salt and private values), as computed by
    // aiohomekit's SrpClient/SrpServer (an independent controller
    // implementation); B, K, M1 and M2 must match it exactly.
    const A: &str = "fab6f5d2615d1e323512e7991cc37443f487da604ca8c9230fcb04e541dce6280b27ca4680b0374f179dc3bdc7553fe62459798c701ad864a91390a28c93b644adbf9c00745b942b79f9012a21b9b78782319d83a1f8362866fbd6f46bfc0ddb2e1ab6e4b45a9906b82e37f05d6f97f6a3eb6e182079759c4f6847837b62321ac1b4fa68641fcb4bb98dd697a0c73641385f4bab25b793584cc39fc8d48d4bd867a9a3c10f8ea12170268e34fe3bbe6ff89998d60da2f3e4283cbec1393d52af724a57230c604e9fbce583d7613e6bffd67596ad121a8707eec46944957033686a155f644d5c5863b48f61bdbf19a53eab6dad0a186b8c152e5f5d8cad4b0ef8aa4ea5008834c3cd342e5e0f167ad04592cd8bd279639398ef9e114dfaaab919e14e850989224ddd98576d79385d2210902e9f9b1f2d86cfa47ee244635465f71058421a0184be51dd10cc9d079e6f1604e7aa9b7cf7883c7d4ce12b06ebe16081e23f27a231d18432d7d1bb55c28ae21ffcf005f57528d15a88881bb3bbb7fe";
    const B: &str = "40f57088a482d4c7733384fe0d301fddca9080ad7d4f6fdf09a01006c3cb6d562e41639ae8fa21de3b5dba7585b275589bdb279863c562807b2b99083cd1429cdbe89e25bfbd7e3cad3173b2e3c5a0b174da6d5391e6a06e465f037a4006254839a56bf76da84b1c94e0ae208576156fe5c140a4ba4ffc9e38c3b07b88845fc6f7ddda93381fe0ca6084c4cd2d336e5451c464ccb6ec65e7d16e548a273e826284af2559b6264274215960fff47bdd63d3aff064d6137af769661c9d4fee47382603c88eaa0980581d07758461b777e4356dda5835198b51feea308d70f75450b71675c08c7d8302fd7539dd1ff2a11cb4258aa70d234436aa42b6a0615f3f915d55cc3b966b2716b36e4d1a06ce5e5d2ea3bee5a1270e8751da45b60b997b0ffdb0f9962fee4f03bee780ba0a845b1d9271421783ae6601a61ea2e342e4f2e8bc935a409ead19f221bd1b74e2964dd19fc845f60efc09338b60b6b256d8cac889cca306cc370a0b18c8b886e95da0af5235fef4393020d2b7f3056904759042";
    const V: &str = "9b5e061701ea7aeb39cf6e3519655a853cf94c75caf2555ef1faf759bb79cb477014e04a88d68ffc05323891d4c205b8de81c2f203d8fad1b24d2c109737f1bebbd71f912447c4a03c26b9fad8edb3e780778e302529ed1ee138ccfc36d4ba313cc48b14ea8c22a0186b222e655f2df5603fd75df76b3b08ff8950069add03a754ee4ae88587cce1bfde36794dbae4592b7b904f442b041cb17aebad1e3aebe3cbe99de65f4bb1fa00b0e7af06863db53b02254ec66e781e3b62a8212c86beb0d50b5ba6d0b478d8c4e9bbcec21765326fbd14058d2bbde2c33045f03873e53948d78b794f0790e48c36aed6e880f557427b2fc06db5e1e2e1d7e661ac482d18e528d7295ef7437295ff1a72d402771713f16876dd050ae5b7ad53ccb90855c93956648358adfd966422f52498732d68d1d7fbef10d78034ab8dcb6f0fcf885cc2b2ea2c3e6ac86609ea058a9da8cc63531dc915414df568b09482ddac1954dec7eb714f6ff7d44cd5b86f6bd115810930637c01d0f6013bc9740fa2c633ba89";
    const K: &str = "5cbc219db052138ee1148c71cd4498963d682549ce91ca24f098468f06015beb6af245c2093f98c3651bca83ab8cab2b580bbf02184fefdf26142f73df95ac50";
    const M1: &str = "5f7c14ab57ed0e94fd1d78c6b4dd09ed7e340b7e05d419a9fd760f6b35e523d1310777a1ae1d2826f596f3a85116cc457c7c964d4f44ded5559da818c88b617f";
    const M2: &str = "2fa0e81f5cb73b88fa0964270f321dd641f2227a5d805c40f1bfe96aaf6a19ffce8e23287965a39eab9d5a02215f89e128177ed2c4f103e655a045531bcbf7ad";

    fn vector_salt() -> [u8; 16] {
        hex("BEB25379D1A8581EB5A727673A2441EE").try_into().unwrap()
    }
    fn vector_b() -> [u8; 32] {
        hex("E487CB59D31AC550471E81F00F6928E01DDA08E974A004F49E61F5D105284D20").try_into().unwrap()
    }

    #[test]
    fn k_is_the_known_constant() {
        // aiohomekit's CLIENT_K_VALUE.
        let want = hex("a9c2e2559bf0ebb53f0cbbf62282906bede7f2182f00678211fbd5bde5b285033a4993503b87397f9be5ec02080fedbc0835587ad039060879b8621e8c3659e0");
        assert_eq!(&bytes(&k())[LEN - 64..], want.as_slice());
    }

    #[test]
    fn hap_test_vector() {
        // Through the same code as a pair-setup (`new_as`, `verify_as`), with
        // the vector's user name.
        let s = Server::new_as("alice", "password123", vector_salt(), vector_b());
        assert_eq!(bytes(&s.v).to_vec(), hex(V));
        assert_eq!(s.public().to_vec(), hex(B));
        let ok = s.verify_as("alice", &hex(A), &hex(M1)).expect("the vector's M1 is right");
        assert_eq!(ok.key.to_vec(), hex(K));
        assert_eq!(ok.proof.to_vec(), hex(M2));
        // One bit off in M1, or another user name: refused.
        let mut m1 = hex(M1);
        m1[0] ^= 1;
        assert!(s.verify_as("alice", &hex(A), &m1).is_none());
        assert!(s.verify(&hex(A), &hex(M1)).is_none());
    }

    #[test]
    fn pair_setup_user_round_trip() {
        // A controller computing the same with the right code is accepted; a
        // wrong code or a bad A is not.
        let code = "123-45-678";
        let s = Server::new(code, vector_salt(), vector_b());
        let (a_priv, a_pub) = client_pub();
        let m1 = client_proof(code, &s, &a_priv, &a_pub);
        let ok = s.verify(&a_pub, &m1).expect("right code");
        assert_eq!(ok.proof.len(), 64);
        let bad = client_proof("123-45-679", &s, &a_priv, &a_pub);
        assert!(s.verify(&a_pub, &bad).is_none());
        assert!(s.verify(&[0u8; LEN], &m1).is_none());
        assert!(s.verify(&bytes(&n()), &m1).is_none());
        assert!(s.verify(&[1u8; LEN + 1], &m1).is_none());
        assert!(s.verify(&a_pub, &m1[..63]).is_none());
    }

    // The controller's side, for the round trip.
    fn client_pub() -> (U256, Vec<u8>) {
        let a = U256::from_be_slice(&hex("60975527035CF2AD1989806F0407210BC81EDC04E2762A56AFD529DDDA2D4393"));
        let g = m(&U3072::from_u8(G), &params());
        (a, bytes(&g.pow(&a).retrieve()).to_vec())
    }

    fn client_proof(code: &str, s: &Server, a: &U256, a_pub: &[u8]) -> Vec<u8> {
        let p = params();
        let x = hash_to_u512(&h(&[&s.salt, &h(&[USER.as_bytes(), b":", code.as_bytes()])]));
        let u = hash_to_u512(&h(&[a_pub, s.public()]));
        let g = m(&U3072::from_u8(G), &p);
        // S = (B - k*g^x)^(a + u*x)
        let base = m(&U3072::from_be_slice(s.public()), &p) - m(&k(), &p) * g.pow(&x);
        let sv = (base.pow(a) * base.pow(&u).pow(&x)).retrieve();
        let key = h(&[&bytes(&sv)]);
        let hn = h(&[&bytes(&n())]);
        let hg = h(&[&[G]]);
        let xg: Vec<u8> = hn.iter().zip(hg.iter()).map(|(a, b)| a ^ b).collect();
        h(&[&xg, &h(&[USER.as_bytes()]), &s.salt, a_pub, s.public(), &key]).to_vec()
    }
}
