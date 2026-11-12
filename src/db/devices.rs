//! Signed-in devices: what each session was made from, and the browsers each
//! account has signed in from before (hashes only: the cookie that names a
//! browser is never kept, and grants no access).

use super::*;

/// A session as stored (the token itself is only a hash here). The columns
/// after `expires` are empty (0, "") for a session made before the list of
/// devices existed.
pub struct StoredSession {
    pub hash: String,
    pub user: String,
    pub admin: bool,
    pub expires: i64,
    pub created: i64,
    pub ip: String,
    pub label: String,
    pub device: String,
    pub last_seen: i64,
}

/// What `device_seen` found.
pub struct DeviceSeen {
    /// This browser had signed in to the account before.
    pub known: bool,
    /// The account had at least one known browser before this one.
    pub had_any: bool,
}

/// A browser is no longer known to the account, and the notification addresses turned
/// on from it are gone.
pub(super) fn forget_device(tx: &rusqlite::Transaction, user: &str, device: &str) -> Result<()> {
    if device.is_empty() {
        return Ok(());
    }
    tx.execute("DELETE FROM known_devices WHERE user = ?1 AND hash = ?2", params![user, device])?;
    tx.execute("DELETE FROM push_subs WHERE user = ?1 AND session_device = ?2", params![user, device])?;
    Ok(())
}

impl Db {
    /// One session ends, with (when `forget_device_hash` is not empty) its browser's
    /// row and notification addresses: all or nothing.
    pub fn end_session_row(&self, user: &str, hash: &str, forget_device_hash: &str) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let n = tx.execute("DELETE FROM sessions WHERE hash = ?1 AND user = ?2", params![hash, user])?;
        forget_device(&tx, user, forget_device_hash)?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Forget the account's known browsers, except `keep` (empty: none kept).
    pub fn devices_clear(&self, user: &str, keep: &str) -> Result<()> {
        self.lock().execute("DELETE FROM known_devices WHERE user = ?1 AND hash <> ?2", params![user, keep])?;
        Ok(())
    }

    /// Which sign-in (device hash) a notification address was turned on from.
    pub fn push_set_session_device(&self, endpoint: &str, device: Option<&str>) -> Result<()> {
        self.lock().execute("UPDATE push_subs SET session_device = ?2 WHERE endpoint = ?1", params![endpoint, device.filter(|d| !d.is_empty())])?;
        Ok(())
    }

    pub fn save_session(&self, s: &StoredSession) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO sessions (hash, user, admin, expires, created, ip, label, device, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![s.hash, s.user, s.admin, s.expires, s.created, s.ip, s.label, s.device, s.last_seen],
        )?;
        Ok(())
    }

    /// Where and when a session was last used. Never makes a session: one
    /// that was ended meanwhile stays ended.
    pub fn touch_session(&self, hash: &str, ip: &str, last_seen: i64) -> Result<()> {
        self.lock().execute("UPDATE sessions SET ip = ?2, last_seen = ?3 WHERE hash = ?1", params![hash, ip, last_seen])?;
        Ok(())
    }

    pub fn load_sessions(&self) -> Result<Vec<StoredSession>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT hash, user, admin, expires, created, ip, label, device, last_seen FROM sessions")?;
        let rows = st.query_map([], |r| {
            Ok(StoredSession { hash: r.get(0)?, user: r.get(1)?, admin: r.get(2)?, expires: r.get(3)?, created: r.get(4)?, ip: r.get(5)?, label: r.get(6)?, device: r.get(7)?, last_seen: r.get(8)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One session of the account is gone (true), or there was none.
    #[cfg(test)]
    pub fn delete_session_of(&self, user: &str, hash: &str) -> Result<bool> {
        self.end_session_row(user, hash, "")
    }

    /// Note a successful sign-in from the browser (or network) `hash`: known
    /// already, or now remembered. At most `cap` are kept per account, the
    /// least recently used pushed out. All in one step, so two sign-ins at
    /// once cannot both look new.
    pub fn device_seen(&self, user: &str, hash: &str, label: &str, now: i64, cap: usize) -> Result<DeviceSeen> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let before: i64 = tx.query_row("SELECT COUNT(*) FROM known_devices WHERE user = ?1", params![user], |r| r.get(0))?;
        let changed = tx.execute("UPDATE known_devices SET last_seen = ?3, label = ?4 WHERE user = ?1 AND hash = ?2", params![user, hash, now, label])?;
        if changed == 0 {
            tx.execute("INSERT INTO known_devices (user, hash, label, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?4)", params![user, hash, label, now])?;
            tx.execute(
                "DELETE FROM known_devices WHERE user = ?1 AND hash NOT IN (SELECT hash FROM known_devices WHERE user = ?1 ORDER BY last_seen DESC, rowid DESC LIMIT ?2)",
                params![user, cap as i64],
            )?;
        }
        tx.commit()?;
        Ok(DeviceSeen { known: changed > 0, had_any: before > 0 })
    }

    /// Has this account signed in here before, as far as the audit log
    /// remembers? (A new account has not; a new account with the name of an
    /// old one is taken for the old one.)
    pub fn signed_in_before(&self, user: &str) -> Result<bool> {
        let n: i64 = self.lock().query_row(
            "SELECT COUNT(*) FROM audit WHERE user = ?1 AND status = 200 AND action IN ('sign-in', 'sign-in (QR code)', 'sign-in (linked server)', 'sign-in (two-step code)', 'sign-in (recovery code)')",
            params![user],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// For tests: make deleting a browser fail.
    #[cfg(test)]
    pub fn break_known_devices(&self) {
        self.lock().execute_batch("DROP TABLE known_devices;").unwrap();
    }

    #[cfg(test)]
    pub fn known_device_count(&self, user: &str) -> usize {
        self.lock().query_row("SELECT COUNT(*) FROM known_devices WHERE user = ?1", params![user], |r| r.get::<_, i64>(0)).unwrap() as usize
    }
}
