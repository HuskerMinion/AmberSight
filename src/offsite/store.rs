//! Where off-site copies go: a folder (network share, other drive) or
//! S3-compatible storage. Both look the same to the backup: named files
//! ("keys" like `snapshots/20261102-030000/manifest.json`) that can be put,
//! read, listed and deleted.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub trait Store: Send + Sync {
    /// For messages: where the copies go.
    fn describe(&self) -> String;
    fn put_file(&self, key: &str, src: &Path) -> Result<()>;
    fn put_bytes(&self, key: &str, data: &[u8]) -> Result<()>;
    /// None when there is no such file.
    fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Download into `dst`; a file bigger than `max` bytes is refused.
    fn get_to_file(&self, key: &str, dst: &Path, max: u64) -> Result<()>;
    /// The first `n` bytes (fewer if the file is shorter); None when there
    /// is no such file.
    fn get_head(&self, key: &str, n: usize) -> Result<Option<Vec<u8>>>;
    /// Every file whose key starts with `prefix`, as (key, size).
    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>>;
    /// Deleting a file that is not there is not an error.
    fn delete(&self, key: &str) -> Result<()>;
}

/// The store for the settings (does not check that it can be reached).
pub fn open(cfg: &crate::config::OffsiteConfig) -> Result<Box<dyn Store>> {
    match cfg.kind.as_str() {
        "s3" => Ok(Box::new(super::s3::S3::new(cfg)?)),
        "folder" => Ok(Box::new(Folder::new(&cfg.folder)?)),
        other => bail!("unknown destination kind '{other}'"),
    }
}

impl Store for super::s3::S3 {
    fn describe(&self) -> String {
        super::s3::S3::describe(self)
    }
    fn put_file(&self, key: &str, src: &Path) -> Result<()> {
        super::s3::S3::put_file(self, key, src)
    }
    fn put_bytes(&self, key: &str, data: &[u8]) -> Result<()> {
        super::s3::S3::put_bytes(self, key, data)
    }
    fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        super::s3::S3::get_bytes(self, key)
    }
    fn get_to_file(&self, key: &str, dst: &Path, max: u64) -> Result<()> {
        super::s3::S3::get_to_file(self, key, dst, max)
    }
    fn get_head(&self, key: &str, n: usize) -> Result<Option<Vec<u8>>> {
        super::s3::S3::get_head(self, key, n)
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        super::s3::S3::list(self, prefix)
    }
    fn delete(&self, key: &str) -> Result<()> {
        super::s3::S3::delete(self, key)
    }
}

/// Copies into a folder. Files are written under a temporary name, checked
/// for size and then renamed, so a half-written file never looks finished.
pub struct Folder {
    root: PathBuf,
}

const PART_SUFFIX: &str = ".part";

impl Folder {
    pub fn new(path: &str) -> Result<Folder> {
        let p = path.trim();
        if p.is_empty() {
            bail!("choose the folder to copy to");
        }
        Ok(Folder { root: PathBuf::from(p) })
    }

    /// The folder itself; only its last part is ever created. If the drive
    /// or share is missing, copies must not quietly land on this computer
    /// (an unplugged USB drive's mount point, say).
    fn ready(&self) -> Result<&Path> {
        if !self.root.is_dir() {
            let parent = self.root.parent().filter(|p| !p.as_os_str().is_empty());
            match parent {
                Some(p) if p.is_dir() => std::fs::create_dir(&self.root).with_context(|| format!("could not create {}", self.root.display()))?,
                _ => bail!("{} cannot be reached (is the drive connected or the share available?)", self.root.display()),
            }
        }
        Ok(&self.root)
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        // Keys are made by AmberSight; refuse anything that could leave the folder.
        if key.is_empty() || key.split('/').any(|c| c.is_empty() || c == "." || c == ".." || c.contains('\\') || c.contains(':')) {
            bail!("bad file name {key}");
        }
        let mut p = self.ready()?.to_path_buf();
        for c in key.split('/') {
            p.push(c);
        }
        Ok(p)
    }

    fn write_with(&self, key: &str, size: u64, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        let dst = self.path(key)?;
        if let Some(d) = dst.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
        }
        let tmp = PathBuf::from(format!("{}{PART_SUFFIX}", dst.display()));
        let res = (|| -> Result<()> {
            write(&tmp)?;
            let got = std::fs::metadata(&tmp)?.len();
            if got != size {
                bail!("{key}: wrote {got} bytes, expected {size}");
            }
            // Rename over an older copy (Windows needs it gone first).
            if dst.exists() {
                std::fs::remove_file(&dst)?;
            }
            std::fs::rename(&tmp, &dst)?;
            Ok(())
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res.with_context(|| format!("writing {}", dst.display()))
    }
}

/// Files under `dir` whose key starts with `prefix`; folders that cannot
/// hold such a key are not entered.
fn walk(dir: &Path, rel: &str, prefix: &str, out: &mut Vec<(String, u64)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let key = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                let sub = format!("{key}/");
                if sub.starts_with(prefix) || prefix.starts_with(&sub) {
                    walk(&e.path(), &key, prefix, out);
                }
            }
            Ok(t) if t.is_file() && !name.ends_with(PART_SUFFIX) && key.starts_with(prefix) => out.push((key, e.metadata().map(|m| m.len()).unwrap_or(0))),
            _ => {}
        }
    }
}

impl Store for Folder {
    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn put_file(&self, key: &str, src: &Path) -> Result<()> {
        let size = std::fs::metadata(src).with_context(|| format!("read {}", src.display()))?.len();
        self.write_with(key, size, |tmp| {
            let mut r = std::fs::File::open(src)?;
            let mut w = super::create_private(tmp)?;
            std::io::copy(&mut r, &mut w)?;
            w.sync_all()?;
            Ok(())
        })
    }

    fn put_bytes(&self, key: &str, data: &[u8]) -> Result<()> {
        self.write_with(key, data.len() as u64, |tmp| {
            use std::io::Write;
            let mut w = super::create_private(tmp)?;
            w.write_all(data)?;
            w.sync_all()?;
            Ok(())
        })
    }

    fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(key)?) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn get_to_file(&self, key: &str, dst: &Path, max: u64) -> Result<()> {
        use std::io::Read;
        let src = self.path(key)?;
        let r = std::fs::File::open(&src).with_context(|| format!("open {}", src.display()))?;
        let mut w = super::create_private(dst)?;
        let n = std::io::copy(&mut r.take(max.saturating_add(1)), &mut w)?;
        if n > max {
            bail!("{key} is bigger than expected");
        }
        Ok(())
    }

    fn get_head(&self, key: &str, n: usize) -> Result<Option<Vec<u8>>> {
        use std::io::Read;
        let f = match std::fs::File::open(self.path(key)?) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut b = Vec::with_capacity(n);
        f.take(n as u64).read_to_end(&mut b)?;
        Ok(Some(b))
    }

    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let root = self.ready()?;
        let mut out = Vec::new();
        // Walk only the folder the prefix names.
        let dir_part = prefix.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let mut start = root.to_path_buf();
        for c in dir_part.split('/').filter(|c| !c.is_empty()) {
            start.push(c);
        }
        walk(&start, dir_part, prefix, &mut out);
        out.retain(|(k, _)| k.starts_with(prefix));
        out.sort();
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let p = self.path(key)?;
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("delete {}", p.display())),
        }
        // Tidy up folders left empty (never the root itself).
        let mut d = p.parent().map(Path::to_path_buf);
        while let Some(dir) = d {
            if dir == self.root || std::fs::remove_dir(&dir).is_err() {
                break;
            }
            d = dir.parent().map(Path::to_path_buf);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_put_list_delete() {
        let tmp = std::env::temp_dir().join(format!("ambs-offsite-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let f = Folder::new(tmp.join("dest").to_str().unwrap()).unwrap();
        f.put_bytes("snapshots/a/manifest.json", b"{}").unwrap();
        f.put_bytes("clips/x.mp4", b"12345").unwrap();
        assert_eq!(f.list("snapshots/").unwrap(), vec![("snapshots/a/manifest.json".to_string(), 2)]);
        assert_eq!(f.list("").unwrap().len(), 2);
        // A prefix at the top level does not look inside other folders.
        f.put_bytes("t-1.txt", b"t").unwrap();
        let mut seen = Vec::new();
        walk(&tmp.join("dest"), "", "t-1.txt", &mut seen);
        assert_eq!(seen, vec![("t-1.txt".to_string(), 1)]);
        let mut all = Vec::new();
        walk(&tmp.join("dest"), "", "clips/", &mut all);
        assert_eq!(all, vec![("clips/x.mp4".to_string(), 5)]);
        f.delete("t-1.txt").unwrap();
        assert_eq!(f.get_head("clips/x.mp4", 3).unwrap().unwrap(), b"123");
        assert!(f.get_head("clips/none.mp4", 3).unwrap().is_none());
        let dl = tmp.join("dl");
        assert!(f.get_to_file("clips/x.mp4", &dl, 4).is_err());
        f.get_to_file("clips/x.mp4", &dl, 5).unwrap();
        assert_eq!(f.get_bytes("clips/x.mp4").unwrap().unwrap(), b"12345");
        assert!(f.get_bytes("clips/none.mp4").unwrap().is_none());
        f.delete("snapshots/a/manifest.json").unwrap();
        assert!(!tmp.join("dest").join("snapshots").exists(), "empty folders tidied");
        assert!(f.delete("snapshots/a/manifest.json").is_ok());
        assert!(f.path("../evil").is_err());
        // A folder whose parent is missing (unplugged drive) is not created.
        let gone = Folder::new(tmp.join("nodrive").join("backup").to_str().unwrap()).unwrap();
        assert!(gone.put_bytes("k", b"1").is_err());
        assert!(!tmp.join("nodrive").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
