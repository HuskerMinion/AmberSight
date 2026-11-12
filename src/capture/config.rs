//! The helper's config file: server address, helper id, token and the pinned CA.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperConfig {
    /// `https://host:port`
    pub server: String,
    pub id: String,
    pub token: String,
    pub ca_pem: String,
}

/// Windows: `%APPDATA%\AmberSight`; others: `$XDG_CONFIG_HOME/ambersight`.
pub fn config_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        let appdata = std::env::var_os("APPDATA").context("APPDATA is not set")?;
        return Ok(PathBuf::from(appdata).join("AmberSight"));
    }
    Ok(xdg_config_home()?.join("ambersight"))
}

pub fn xdg_config_home() -> Result<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        return Ok(PathBuf::from(x));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config"))
}

pub fn default_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("capture.toml"))
}

impl HelperConfig {
    pub fn load(path: &Path) -> Result<HelperConfig> {
        let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{} is not a valid helper config", path.display()))
    }

    /// Write the file readable by the current user only.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(self)?;
        // Write to a fresh temp file that is restricted before the secret goes in.
        let tmp = path.with_extension("toml.tmp");
        let _ = std::fs::remove_file(&tmp);
        {
            use std::io::Write;
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            let mut f = o.open(&tmp).with_context(|| format!("could not create {}", tmp.display()))?;
            restrict_to_user(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("could not write {}", path.display()))?;
        restrict_to_user(path)?;
        Ok(())
    }
}

/// Owner-only access: mode 0600 on Unix; on Windows an ACL that grants only
/// the current user (inheritance removed).
pub fn restrict_to_user(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    {
        let sid = current_user_sid().context("could not find the current user's SID")?;
        let out = std::process::Command::new(crate::lockdown::icacls_exe())
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("*{sid}:F")])
            .output()
            .context("could not run icacls")?;
        anyhow::ensure!(out.status.success(), "icacls failed: {}", String::from_utf8_lossy(&out.stdout).trim());
    }
    Ok(())
}

/// `S-1-5-21-...` of the signed-in user (from `whoami /user`).
#[cfg(windows)]
fn current_user_sid() -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()?;
    parse_whoami_sid(&String::from_utf8_lossy(&out.stdout))
}

/// The SID from `"host\user","S-1-5-21-..."`.
#[allow(dead_code)] // used on Windows and in tests
pub fn parse_whoami_sid(csv: &str) -> Option<String> {
    let line = csv.lines().find(|l| !l.trim().is_empty())?;
    let sid = line.rsplit(',').next()?.trim().trim_matches('"');
    (sid.starts_with("S-1-") && sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| sid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip_private() {
        let dir = std::env::temp_dir().join(format!("amber-capcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("capture.toml");
        let c = HelperConfig {
            server: "https://192.168.1.5:8090".into(),
            id: "h1".into(),
            token: "secret \"quoted\"".into(),
            ca_pem: "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".into(),
        };
        c.save(&path).unwrap();
        assert_eq!(HelperConfig::load(&path).unwrap(), c);
        // Saving again replaces it.
        let mut c2 = c.clone();
        c2.token = "other".into();
        c2.save(&path).unwrap();
        assert_eq!(HelperConfig::load(&path).unwrap(), c2);
        assert!(!path.with_extension("toml.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        #[cfg(windows)]
        {
            // The user with full control, nothing inherited, and no one else but
            // SYSTEM and Administrators (who can read every file anyway; an
            // elevated process's new files list them too).
            let out = std::process::Command::new(crate::lockdown::icacls_exe()).arg(&path).output().unwrap();
            let acl = String::from_utf8_lossy(&out.stdout).to_string();
            let entries: Vec<&str> = acl.lines().filter(|l| l.contains(":(")).collect();
            assert!(!acl.contains("(I)"), "nothing inherited: {acl}");
            let user = std::env::var("USERNAME").unwrap_or_default().to_lowercase();
            let mut mine = 0;
            for e in &entries {
                let who = e.rsplit_once(":(").map(|(w, _)| w.trim()).unwrap_or("").to_lowercase();
                let who = who.rsplit(".toml").next().unwrap_or(&who).trim().to_string();
                if who == "nt authority\\system" || who == "builtin\\administrators" {
                    continue;
                }
                assert!(who.ends_with(&format!("\\{user}")) && e.trim_end().ends_with(":(F)"), "only this user: {acl}");
                mine += 1;
            }
            assert_eq!(mine, 1, "{acl}");
        }
        assert!(HelperConfig::load(&dir.join("missing.toml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whoami_sid() {
        assert_eq!(parse_whoami_sid("\"pc\\alex\",\"S-1-5-21-1-2-3-1001\"\r\n").as_deref(), Some("S-1-5-21-1-2-3-1001"));
        assert_eq!(parse_whoami_sid("garbage"), None);
    }
}
