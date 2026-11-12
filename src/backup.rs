//! Backups of the settings and the database: `<data_dir>/backups/<stamp>/`
//! holds `config.toml` and `ambersight.db`, made every `[storage]
//! backup_hours` (default daily), the newest `backup_keep` kept. The config
//! holds camera passwords, so the folder is readable by the service account
//! and administrators only. Restore: stop the service, copy both files back
//! (config.toml next to the program, ambersight.db into the data folder),
//! start it again.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::engine::Engine;

const STAMP: &str = "%Y%m%d-%H%M%S";

pub fn backups_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}

/// The backups folder, created private (owner / SYSTEM + Administrators).
pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        // Files created inside inherit this: SYSTEM and Administrators only.
        let _ = std::process::Command::new(crate::lockdown::icacls_exe())
            .arg(dir)
            .args(["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"])
            .output();
        // And the account running AmberSight, when it is not SYSTEM (`ambersight run`
        // as an ordinary user, or an admin without elevation), so it can still save.
        if let (Ok(domain), Ok(user)) = (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
            if !user.ends_with('$') && !user.eq_ignore_ascii_case("SYSTEM") {
                let _ = std::process::Command::new(crate::lockdown::icacls_exe()).arg(dir).arg("/grant").arg(format!("{domain}\\{user}:(OI)(CI)F")).output();
            }
        }
    }
    Ok(())
}

/// Existing backups (folder names are their timestamps), oldest first.
pub fn list(data_dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(backups_dir(data_dir))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(is_stamp))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn is_stamp(name: &str) -> bool {
    chrono::NaiveDateTime::parse_from_str(name, STAMP).is_ok()
}

/// Which backups to delete so that `keep` remain (oldest first in `all`).
pub fn to_prune(all: &[PathBuf], keep: usize) -> &[PathBuf] {
    &all[..all.len().saturating_sub(keep.max(1))]
}

/// Make one backup now; returns its folder.
pub fn backup_now(config_path: &Path, db: &crate::db::Db, data_dir: &Path, keep: u32) -> Result<PathBuf> {
    let root = backups_dir(data_dir);
    private_dir(&root)?;
    let mut dir = root.join(chrono::Local::now().format(STAMP).to_string());
    if dir.exists() {
        // Two backups in the same second (manual + scheduled): wait it out.
        std::thread::sleep(Duration::from_millis(1100));
        dir = root.join(chrono::Local::now().format(STAMP).to_string());
    }
    std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let result = (|| -> Result<()> {
        let cfg = std::fs::read(config_path).with_context(|| format!("read {}", config_path.display()))?;
        crate::tls::write_private(&dir.join("config.toml"), &cfg)?;
        db.backup_to(&dir.join("ambersight.db"))?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    for old in to_prune(&list(data_dir), keep as usize) {
        if let Err(e) = std::fs::remove_dir_all(old) {
            tracing::warn!("backup: could not remove old backup {}: {e}", old.display());
        }
    }
    Ok(dir)
}

/// Age of the newest backup in seconds (None: no backup yet).
fn newest_age(data_dir: &Path) -> Option<i64> {
    let newest = list(data_dir).pop()?;
    let name = newest.file_name()?.to_str()?;
    let t = chrono::NaiveDateTime::parse_from_str(name, STAMP).ok()?;
    let local = t.and_local_timezone(chrono::Local).earliest()?;
    Some(chrono::Local::now().signed_duration_since(local).num_seconds())
}

/// Background thread: a backup whenever the newest one is older than
/// `backup_hours` (checked every 10 minutes, first check 5 minutes after start).
pub fn start(engine: Arc<Engine>) {
    std::thread::Builder::new()
        .name("backup".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(300));
            loop {
                let (hours, keep) = {
                    let c = engine.config.lock().unwrap();
                    (c.storage.backup_hours, c.storage.backup_keep)
                };
                if hours > 0 && newest_age(&engine.data_dir).is_none_or(|a| a >= i64::from(hours) * 3600) {
                    match backup_now(Path::new(&engine.config_path), &engine.db, &engine.data_dir, keep) {
                        Ok(d) => tracing::info!("backup: settings and database saved to {}", d.display()),
                        Err(e) => tracing::warn!("backup failed: {e:#}"),
                    }
                }
                std::thread::sleep(Duration::from_secs(600));
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_newest() {
        let all: Vec<PathBuf> = ["20261029-010000", "20261030-010000", "20261031-010000"].iter().map(PathBuf::from).collect();
        assert_eq!(to_prune(&all, 2), &all[..1]);
        assert!(to_prune(&all, 7).is_empty());
        assert_eq!(to_prune(&all, 0).len(), 2); // at least one is always kept
    }

    #[test]
    fn backup_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("ambs-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg = tmp.join("config.toml");
        std::fs::write(&cfg, "[server]\nport = 1\n").unwrap();
        let db = crate::db::Db::open(&tmp.join("ambersight.db")).unwrap();
        let d1 = backup_now(&cfg, &db, &tmp, 1).unwrap();
        assert_eq!(std::fs::read_to_string(d1.join("config.toml")).unwrap(), "[server]\nport = 1\n");
        assert!(crate::db::Db::open(&d1.join("ambersight.db")).is_ok());
        let d2 = backup_now(&cfg, &db, &tmp, 1).unwrap();
        assert_ne!(d1, d2);
        assert_eq!(list(&tmp), vec![d2]); // keep = 1
        assert!(newest_age(&tmp).is_some_and(|a| a < 60));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
