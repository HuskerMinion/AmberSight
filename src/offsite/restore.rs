//! Getting an off-site copy back (`ambersight offsite-restore`, see the
//! notes at the top of mod.rs). Works from the command line alone, so a
//! new computer needs only the program, the passphrase and the address.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use super::crypt;
use super::store::Store;
use super::{Manifest, ENC};

/// Finished copies at the destination (UTC times), oldest first.
pub fn list_snapshots(store: &dyn Store) -> Result<Vec<String>> {
    let mut v: Vec<String> = store
        .list("snapshots/")?
        .into_iter()
        .filter_map(|(k, _)| k.strip_prefix("snapshots/")?.strip_suffix("/manifest.json").map(String::from))
        .filter(|s| !s.contains('/'))
        .collect();
    v.sort();
    Ok(v)
}

/// A path from a manifest, checked so it stays inside `to`.
fn inside(to: &Path, rel: &str) -> Result<std::path::PathBuf> {
    if rel.is_empty() || rel.split('/').any(|c| c.is_empty() || c == "." || c == ".." || c.contains('\\') || c.contains(':')) {
        bail!("bad file name in the copy: {rel}");
    }
    let mut p = to.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    Ok(p)
}

/// One file to restore.
struct Item {
    key: String,
    rel: String,
    size: u64,
    sha: String,
    /// The name the encrypted file must carry (None: older files).
    expect: Option<String>,
}

/// Restore copy `snapshot` (None: the newest) into the folder `to`.
/// `passphrase` is asked only when something is encrypted. With
/// `must_encrypt` (S3, or a passphrase given up front) a copy that is not
/// encrypted is refused. A file that fails is reported and the rest still
/// restored; returns how many were restored, or an error naming what failed.
pub fn restore(store: &dyn Store, to: &Path, snapshot: Option<&str>, media: bool, must_encrypt: bool, passphrase: &mut dyn FnMut() -> Result<String>, say: &dyn Fn(&str)) -> Result<usize> {
    let snaps = list_snapshots(store)?;
    let stamp = match snapshot {
        Some(s) => snaps.iter().find(|x| x.as_str() == s).cloned().with_context(|| format!("there is no copy {s} (see --list)"))?,
        None => snaps.last().cloned().context("no finished copies found there")?,
    };
    let man: Manifest = serde_json::from_slice(&store.get_bytes(&format!("snapshots/{stamp}/manifest.json"))?.context("the copy's list is missing")?)
        .context("the copy's list could not be read")?;
    for name in ["config.toml", "ambersight.db"] {
        if to.join(name).exists() {
            bail!("{} already has {name}: restore into an empty folder", to.display());
        }
    }

    // Encrypted or not is never taken from the file names alone: anyone who
    // can write there could add a plain config.toml.
    let has_key_file = store.get_bytes("key.json")?.is_some();
    let any_enc = man.files.iter().any(|f| f.key.ends_with(ENC)) || man.media.iter().any(|m| m.key.ends_with(ENC));
    let encrypted = must_encrypt || man.encrypted || man.key.is_some() || has_key_file || any_enc;
    let mut keys = crypt::KeyCache::default();
    let mut pass: Option<String> = None;
    let checked = if encrypted && man.version >= 2 {
        let p = passphrase()?;
        let ki = man.key.as_ref().context("the copy's list has no key information (damaged?)")?;
        let k = ki.key(&p, &mut keys)?.context("wrong passphrase for this copy")?;
        if !man.verify(&k) || man.stamp != stamp {
            bail!("the copy's list does not check out: it was changed after it was made (or is damaged). Nothing restored.");
        }
        pass = Some(p);
        true
    } else {
        if encrypted {
            say("Note: this copy was made by an older AmberSight; its list cannot be checked.");
        } else {
            say("Note: this copy is not encrypted.");
        }
        false
    };
    let mut items: Vec<Item> = man
        .files
        .iter()
        .map(|f| Item { key: f.key.clone(), rel: f.name.clone(), size: f.size, sha: f.sha256.clone(), expect: checked.then(|| f.key.clone()) })
        .collect();
    if media {
        items.extend(man.media.iter().map(|m| Item { key: m.key.clone(), rel: m.path.clone(), size: m.size, sha: String::new(), expect: (checked && m.fmt != 1).then(|| m.key.clone()) }));
    }
    if encrypted {
        if let Some(i) = items.iter().find(|i| !i.key.ends_with(ENC)) {
            bail!("refusing to restore: {} is not encrypted, but this backup is (changed by someone?)", i.key);
        }
    }

    std::fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    let tmp = to.join(".offsite-download");
    std::fs::create_dir_all(&tmp)?;
    say(&format!("Restoring the copy made {stamp} UTC on {} ({} files)", man.server, items.len()));
    let total = items.len();
    let mut done = 0;
    let mut failed: Vec<String> = Vec::new();
    for it in &items {
        let mut get_pass = || -> Result<String> {
            if pass.is_none() {
                pass = Some(passphrase()?);
            }
            Ok(pass.clone().unwrap_or_default())
        };
        match restore_one(store, to, &tmp, it, &mut get_pass, &mut keys) {
            Ok(()) => {
                done += 1;
                say(&format!("  {}", it.rel));
            }
            Err(e) => {
                say(&format!("  FAILED {}: {e:#}", it.rel));
                failed.push(format!("{}: {e:#}", it.rel));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    if !failed.is_empty() {
        bail!("{} of {total} files could not be restored ({done} restored). First: {}", failed.len(), failed[0]);
    }
    Ok(done)
}

/// Download, decrypt and check one file; it appears under its real name
/// only when all is well.
fn restore_one(store: &dyn Store, to: &Path, tmp: &Path, it: &Item, passphrase: &mut dyn FnMut() -> Result<String>, keys: &mut crypt::KeyCache) -> Result<()> {
    let dst = inside(to, &it.rel)?;
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d)?;
    }
    let enc = it.key.ends_with(ENC);
    // Never download more than the file can be.
    let max = if enc { crypt::encrypted_len_v1(it.size) + 2 + crypt::MAX_NAME as u64 } else { it.size };
    let dl = tmp.join("file");
    let part = PathBuf::from(format!("{}.part", dst.display()));
    let res = (|| -> Result<()> {
        store.get_to_file(&it.key, &dl, max).with_context(|| format!("downloading {}", it.key))?;
        let r = std::io::BufReader::new(std::fs::File::open(&dl)?);
        let w = std::io::BufWriter::new(super::create_private(&part)?);
        let digest = if enc {
            let p = passphrase()?;
            crypt::decrypt(&p, keys, it.expect.as_deref(), r, w).with_context(|| it.key.clone())?
        } else {
            crypt::copy_hashed(r, w)?
        };
        if digest.size != it.size || (!it.sha.is_empty() && digest.sha256 != it.sha) {
            bail!("{} does not match what was copied (damaged?)", it.key);
        }
        if dst.exists() {
            std::fs::remove_file(&dst)?;
        }
        std::fs::rename(&part, &dst)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&dl);
    if res.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    res
}

/// Decrypt one file (`ambersight offsite-decrypt`).
pub fn decrypt_file(src: &Path, dst: &Path, passphrase: &str) -> Result<()> {
    if dst.exists() {
        bail!("{} already exists", dst.display());
    }
    let r = std::io::BufReader::new(std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?);
    let w = std::io::BufWriter::new(super::create_private(dst)?);
    if let Err(e) = crypt::decrypt(passphrase, &mut crypt::KeyCache::default(), None, r, w) {
        let _ = std::fs::remove_file(dst);
        return Err(e);
    }
    Ok(())
}

/// The passphrase from AMBERSIGHT_OFFSITE_PASSPHRASE, or asked for.
pub fn ask_passphrase() -> Result<String> {
    if let Ok(p) = std::env::var("AMBERSIGHT_OFFSITE_PASSPHRASE") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    use std::io::Write;
    eprint!("Backup passphrase: ");
    std::io::stderr().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let s = s.trim_end_matches(['\r', '\n']).to_string();
    if s.is_empty() {
        bail!("no passphrase given");
    }
    Ok(s)
}
