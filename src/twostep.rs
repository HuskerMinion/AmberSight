//! Two-step codes: the six-digit codes of an authenticator app (RFC 6238:
//! SHA-1, 30 s) and the recovery codes kept for a lost phone.
//!
//! The secrets live in the database, never in config.toml (which gets copied
//! around in plain text). The rest of sign-in is in auth.rs and account_http.rs.

use crate::db::Db;
use subtle::ConstantTimeEq;

const PERIOD: i64 = 30;
const RECOVERY_CODES: usize = 10;
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
/// No letters that are easily misread as each other (i, l, o; 0, 1).
const RECOVERY_LETTERS: &[u8; 31] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// A new secret (160 random bits), in the base32 people and apps use.
pub fn new_secret() -> String {
    use rand::RngCore;
    let mut b = [0u8; 20];
    rand::rngs::OsRng.fill_bytes(&mut b);
    base32(&b)
}

fn base32(bytes: &[u8]) -> String {
    let (mut out, mut acc, mut bits) = (String::new(), 0u32, 0u32);
    for &x in bytes {
        acc = (acc << 8) | x as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(BASE32[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0u32);
    for c in s.bytes() {
        let v = BASE32.iter().position(|x| *x == c)? as u32;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
        acc &= (1 << bits) - 1;
    }
    Some(out)
}

/// The RFC 4226 truncated value for a counter, before cutting to digits.
fn hotp(secret: &[u8], counter: u64) -> u32 {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let tag = ring::hmac::sign(&key, &counter.to_be_bytes());
    let h = tag.as_ref();
    let at = (h[h.len() - 1] & 0x0f) as usize;
    u32::from_be_bytes([h[at] & 0x7f, h[at + 1], h[at + 2], h[at + 3]])
}

fn code_at(secret: &[u8], step: i64) -> String {
    format!("{:06}", hotp(secret, step as u64) % 1_000_000)
}

/// Spaces and dashes are fine ("123 456"), anything else that is not six
/// digits is not a code.
fn clean_code(typed: &str) -> Option<String> {
    let s: String = typed.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    (s.len() == 6 && s.bytes().all(|c| c.is_ascii_digit())).then_some(s)
}

/// The time step `code` belongs to: this one or one either side (clocks
/// differ), and only a step after `last_step`, so a code is never good twice
/// and an older one is not good after a newer one was used.
pub fn matching_step(secret_b32: &str, typed: &str, now: i64, last_step: i64) -> Option<i64> {
    let secret = base32_decode(secret_b32)?;
    let code = clean_code(typed)?;
    let current = now.div_euclid(PERIOD);
    let mut found = None;
    // (All three are always compared, in constant time.)
    for step in (current - 1)..=(current + 1) {
        let same: bool = code_at(&secret, step).as_bytes().ct_eq(code.as_bytes()).into();
        if same && step > last_step && found.is_none() {
            found = Some(step);
        }
    }
    found
}

/// What an authenticator app reads from the QR code.
pub fn otpauth_uri(server: &str, user: &str, secret_b32: &str) -> String {
    let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
    // (Label prefix and issuer the same: some apps show an account twice, or refuse it, otherwise.)
    let issuer = enc(&format!("AmberSight ({server})"));
    format!("otpauth://totp/{issuer}:{}?secret={secret_b32}&issuer={issuer}&algorithm=SHA1&digits=6&period={PERIOD}", enc(user))
}

/// New recovery codes, to show once: (for the person, Argon2 hashes to keep).
/// Argon2 is slow on purpose: not on the async workers.
pub fn new_recovery_codes() -> (Vec<String>, Vec<String>) {
    use rand::Rng;
    let mut r = rand::rngs::OsRng;
    let codes: Vec<String> = (0..RECOVERY_CODES)
        .map(|_| {
            let c: Vec<char> = (0..12).map(|_| RECOVERY_LETTERS[r.gen_range(0..RECOVERY_LETTERS.len())] as char).collect();
            format!("{}-{}-{}", c[..4].iter().collect::<String>(), c[4..8].iter().collect::<String>(), c[8..].iter().collect::<String>())
        })
        .collect();
    let hashes = codes.iter().map(|c| crate::auth::hash_password(&clean_recovery(c))).collect();
    (codes, hashes)
}

fn clean_recovery(typed: &str) -> String {
    typed.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

/// Whether checking this as a recovery code takes the slow hashing: something
/// that cannot be one is wrong at once.
pub fn recovery_is_checked(typed: &str) -> bool {
    clean_recovery(typed).len() == 12
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Right. `recovery_left`: a recovery code was used up, this many remain.
    Right { recovery_left: Option<i64> },
    Wrong,
    /// The account has no two-step codes (any more).
    NotOn,
}

/// Check a code (or, without one, a recovery code) and use it up. Slow when
/// given a recovery code (Argon2): not on the async workers.
pub fn verify(db: &Db, user: &str, code: Option<&str>, recovery: Option<&str>, now: i64) -> anyhow::Result<Verdict> {
    let Some(st) = db.two_step_get(user)?.filter(|s| s.enabled) else { return Ok(Verdict::NotOn) };
    if let Some(code) = code.filter(|c| !c.trim().is_empty()) {
        return Ok(match matching_step(&st.secret, code, now, st.last_step) {
            // (Two tries at the same moment: the database lets one through.)
            Some(step) if db.two_step_use_step(user, step)? => Verdict::Right { recovery_left: None },
            _ => Verdict::Wrong,
        });
    }
    let typed = clean_recovery(recovery.unwrap_or(""));
    if !recovery_is_checked(&typed) {
        return Ok(Verdict::Wrong);
    }
    for (id, hash) in db.recovery_hashes(user)? {
        if crate::auth::verify_password(&typed, &hash) {
            if db.recovery_take(id)? {
                return Ok(Verdict::Right { recovery_left: Some(db.recovery_count(user)?) });
            }
            return Ok(Verdict::Wrong);
        }
    }
    Ok(Verdict::Wrong)
}

#[cfg(test)]
pub fn code_for_test(secret_b32: &str, now: i64) -> String {
    code_at(&base32_decode(secret_b32).unwrap(), now.div_euclid(PERIOD))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_6238_test_vectors() {
        // The SHA-1 rows of RFC 6238 appendix B (eight digits there, the last six here).
        let secret = b"12345678901234567890";
        for (t, want) in [(59i64, 94287082u32), (1111111109, 7081804), (1111111111, 14050471), (1234567890, 89005924), (2000000000, 69279037), (20000000000, 65353130)] {
            assert_eq!(hotp(secret, (t / 30) as u64) % 100_000_000, want, "t={t}");
            assert_eq!(code_at(secret, t / 30), format!("{:06}", want % 1_000_000));
        }
    }

    #[test]
    fn base32_round_trip_and_rfc_4648_vectors() {
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(b"fooba"), "MZXW6YTB");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        let s = new_secret();
        assert_eq!(s.len(), 32, "160 bits");
        assert_eq!(base32_decode(&s).unwrap().len(), 20);
        assert_ne!(s, new_secret());
        assert!(base32_decode("not base32!").is_none());
    }

    #[test]
    fn a_step_either_side_is_accepted_and_no_further() {
        let s = new_secret();
        let now = 1_800_000_015;
        for (t, ok) in [(now - 60, false), (now - 30, true), (now, true), (now + 30, true), (now + 60, false)] {
            assert_eq!(matching_step(&s, &code_for_test(&s, t), now, 0).is_some(), ok, "{}", t - now);
        }
        assert_eq!(matching_step(&s, &code_for_test(&s, now), now, 0), Some(now / 30));
    }

    #[test]
    fn typing_is_forgiving_but_only_six_digits_are_a_code() {
        let s = new_secret();
        let now = 1_800_000_015;
        let c = code_for_test(&s, now);
        assert!(matching_step(&s, &format!("{} {}", &c[..3], &c[3..]), now, 0).is_some());
        for bad in ["", "12345", "1234567", "abcdef", "١٢٣٤٥٦"] {
            assert!(matching_step(&s, bad, now, 0).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_code_is_never_good_twice() {
        let db = Db::open_in_memory().unwrap();
        let s = new_secret();
        let now = 1_800_000_015;
        assert!(db.two_step_begin("ann", &s, now).unwrap());
        assert_eq!(verify(&db, "ann", Some(&code_for_test(&s, now)), None, now).unwrap(), Verdict::NotOn, "not on until confirmed");
        assert!(db.two_step_enable("ann", now / 30 - 2, &s, &[], now).unwrap());
        let code = code_for_test(&s, now);
        assert_eq!(verify(&db, "ann", Some(&code), None, now).unwrap(), Verdict::Right { recovery_left: None });
        assert_eq!(verify(&db, "ann", Some(&code), None, now).unwrap(), Verdict::Wrong, "replay");
        assert_eq!(verify(&db, "ann", Some(&code), None, now + 25).unwrap(), Verdict::Wrong, "replay in the next step's window");
        assert_eq!(verify(&db, "ann", Some(&code_for_test(&s, now - 30)), None, now).unwrap(), Verdict::Wrong, "an older code after a newer one");
        assert_eq!(verify(&db, "ann", Some(&code_for_test(&s, now + 30)), None, now + 30).unwrap(), Verdict::Right { recovery_left: None }, "the next code works");
    }

    #[test]
    fn the_code_that_confirmed_enrolment_cannot_sign_in() {
        let db = Db::open_in_memory().unwrap();
        let s = new_secret();
        let now = 1_800_000_015;
        db.two_step_begin("ann", &s, now).unwrap();
        let code = code_for_test(&s, now);
        let step = matching_step(&s, &code, now, 0).unwrap();
        db.two_step_enable("ann", step, &s, &[], now).unwrap();
        assert_eq!(verify(&db, "ann", Some(&code), None, now).unwrap(), Verdict::Wrong);
    }

    #[test]
    fn recovery_codes_work_once_and_are_stored_hashed() {
        let db = Db::open_in_memory().unwrap();
        let s = new_secret();
        db.two_step_begin("ann", &s, 1000).unwrap();
        let (codes, hashes) = new_recovery_codes();
        assert_eq!(codes.len(), 10);
        assert!(hashes.iter().all(|h| crate::auth::is_hashed(h)));
        assert!(!hashes.iter().any(|h| codes.iter().any(|c| h.contains(&clean_recovery(c)))));
        db.two_step_enable("ann", 1, &s, &hashes[..2], 1000).unwrap();
        assert_eq!(verify(&db, "ann", None, Some("aaaa-aaaa-aaaa"), 1000).unwrap(), Verdict::Wrong);
        assert_eq!(verify(&db, "ann", None, None, 1000).unwrap(), Verdict::Wrong, "nothing given");
        assert_eq!(verify(&db, "ann", None, Some(&codes[0].to_uppercase().replace('-', " ")), 1000).unwrap(), Verdict::Right { recovery_left: Some(1) });
        assert_eq!(verify(&db, "ann", None, Some(&codes[0]), 1000).unwrap(), Verdict::Wrong, "used up");
        assert_eq!(verify(&db, "ann", None, Some(&codes[1]), 1000).unwrap(), Verdict::Right { recovery_left: Some(0) });
        // Turning it off forgets them.
        assert_eq!(db.two_step_remove("ann").unwrap(), 1);
        assert_eq!(db.recovery_count("ann").unwrap(), 0);
        assert_eq!(verify(&db, "ann", None, Some(&codes[0]), 1000).unwrap(), Verdict::NotOn);
    }

    #[test]
    fn a_second_enrolment_does_not_replace_a_running_secret() {
        let db = Db::open_in_memory().unwrap();
        db.two_step_begin("ann", "AAAA", 1).unwrap();
        assert!(!db.two_step_enable("ann", 1, "ZZZZ", &[], 1).unwrap(), "not with a secret other than the one waiting");
        assert!(!db.two_step_enabled("ann").unwrap());
        db.two_step_enable("ann", 1, "AAAA", &[], 1).unwrap();
        assert!(!db.two_step_begin("ann", "BBBB", 2).unwrap());
        assert_eq!(db.two_step_get("ann").unwrap().unwrap().secret, "AAAA");
        assert!(!db.two_step_enable("ann", 9, "AAAA", &[], 2).unwrap());
        db.two_step_cancel("ann", "AAAA").unwrap();
        assert!(db.two_step_enabled("ann").unwrap(), "cancel never touches codes that are on");
        db.two_step_begin("eve", "EEEE", 1).unwrap();
        db.two_step_cancel("eve", "OTHER").unwrap();
        assert!(db.two_step_get("eve").unwrap().is_some(), "a newer start stays");
        db.two_step_cancel("eve", "EEEE").unwrap();
        assert!(db.two_step_get("eve").unwrap().is_none(), "a waiting secret is gone");
        assert!(db.two_step_enabled("ann").unwrap());
        assert!(!db.two_step_enabled("bob").unwrap());
        // Only a secret still waiting for its first code is dropped as stale.
        db.two_step_begin("bob", "CCCC", 1).unwrap();
        db.two_step_drop_waiting(100).unwrap();
        assert!(db.two_step_get("bob").unwrap().is_none());
        assert!(db.two_step_enabled("ann").unwrap());
    }

    #[test]
    fn everything_of_an_account_goes_together() {
        let db = Db::open_in_memory().unwrap();
        db.two_step_begin("ann", "AAAA", 1).unwrap();
        db.two_step_enable("ann", 1, "AAAA", &["h".to_string()], 1).unwrap();
        for (id, admin) in [("t1", true), ("t2", false)] {
            db.token_insert(&crate::db::TokenRow { id: id.into(), user: "ann".into(), name: "x".into(), admin, hash: id.into(), created_at: 1, last_used: None }).unwrap();
        }
        db.token_delete_admin("ann").unwrap();
        assert_eq!(db.token_all().unwrap().len(), 1, "a demotion takes the control token only");
        db.account_data_delete("ann").unwrap();
        assert!(db.two_step_get("ann").unwrap().is_none());
        assert_eq!(db.recovery_count("ann").unwrap(), 0);
        assert!(db.token_all().unwrap().is_empty());
    }

    #[test]
    fn changes_from_the_command_line_wait_by_id() {
        let db = Db::open_in_memory().unwrap();
        db.account_request_add("ann", "password", "h1", false).unwrap();
        db.account_request_add("ann", "password", "h2", true).unwrap();
        let rows = db.account_requests().unwrap();
        assert_eq!((rows[0].hash.as_str(), rows[1].hash.as_str(), rows[1].revoke_tokens), ("h1", "h2", true));
        db.account_request_done(rows[0].id).unwrap();
        assert_eq!(db.account_requests().unwrap()[0].hash, "h2", "a second run is not lost");
        // Marked as done is remembered, under the same number.
        let id = db.account_requests().unwrap()[0].id;
        assert!(!db.account_requests().unwrap()[0].claimed);
        db.account_request_claim(id).unwrap();
        let row = &db.account_requests().unwrap()[0];
        assert!(row.claimed);
        assert_eq!(row.id, id);
    }

    #[test]
    fn the_uri_is_what_authenticator_apps_expect() {
        let u = otpauth_uri("Front House", "ann b", "ABC");
        assert_eq!(u, "otpauth://totp/AmberSight%20%28Front%20House%29:ann%20b?secret=ABC&issuer=AmberSight%20%28Front%20House%29&algorithm=SHA1&digits=6&period=30");
    }
}
