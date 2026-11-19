//! Fixes the SHA-256 of every language pack in `lang/<code>.json` into the program (src/lang.rs
//! reads the table). A pack is accepted at run time only if its hash is in that table, so a pack
//! works only with the release it was built for.
//!
//! No `lang/` folder (or no packs) gives an empty table: nothing is accepted, English only.

use sha2::{Digest, Sha256};
use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=lang");
    let mut pins: Vec<(String, String, u64)> = Vec::new();
    if let Ok(dir) = fs::read_dir("lang") {
        for e in dir.flatten() {
            let path = e.path();
            let (Some(stem), Some("json")) = (path.file_stem().and_then(|s| s.to_str()), path.extension().and_then(|s| s.to_str())) else { continue };
            // en.json is the English source the other packs are made from, not a pack.
            if stem == "en" || !stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                continue;
            }
            let Ok(body) = fs::read(&path) else { continue };
            // The pins are of exact bytes: a checkout that turned line ends into CRLF would make every pack fail its pin.
            if body.contains(&b'\r') {
                panic!("{} contains a carriage return: packs must have LF line ends (.gitattributes: lang/*.json -text)", path.display());
            }
            let hex: String = Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect();
            println!("cargo:rerun-if-changed={}", path.display());
            pins.push((stem.to_string(), hex, body.len() as u64));
        }
    }
    pins.sort();
    let mut out = String::from("/// (code, SHA-256 of lang/<code>.json when this program was built, its size). Made by build.rs.\npub(crate) const PINS: &[(&str, &str, u64)] = &[\n");
    for (code, hex, size) in &pins {
        out.push_str(&format!("    ({code:?}, {hex:?}, {size}),\n"));
    }
    out.push_str("];\n");
    let dest = Path::new(&env::var("OUT_DIR").expect("OUT_DIR")).join("lang_pins.rs");
    fs::write(dest, out).expect("write lang_pins.rs");
}
