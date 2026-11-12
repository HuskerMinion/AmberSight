//! Two-step sign-in and device tokens: secrets kept here and not in
//! config.toml, which gets copied around in plain text.

use super::*;

/// An account's two-step state.
pub struct TwoStepRow {
    pub secret: String,
    /// False while it waits for the first correct code.
    pub enabled: bool,
    pub last_step: i64,
    pub created_at: i64,
}

/// A device token as stored (its value is only a hash here).
pub struct TokenRow {
    pub id: String,
    pub user: String,
    pub name: String,
    pub admin: bool,
    pub hash: String,
    pub created_at: i64,
    pub last_used: Option<i64>,
}

/// A change from the command line, waiting for the server.
pub struct AccountRequest {
    pub id: i64,
    pub user: String,
    pub kind: String,
    pub hash: String,
    pub revoke_tokens: bool,
    /// Already in the settings; only what follows from it is left to do.
    pub claimed: bool,
    /// A newer change to the same thing came first (from a web page): it is never
    /// put in the settings. `claimed` is true then too.
    pub superseded: bool,
}

impl Db {
    /// Whether signing in to this account needs a code.
    pub fn two_step_enabled(&self, user: &str) -> Result<bool> {
        Ok(self.lock().query_row("SELECT enabled FROM two_step WHERE user = ?1", params![user], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) == 1)
    }

    pub fn two_step_get(&self, user: &str) -> Result<Option<TwoStepRow>> {
        Ok(self
            .lock()
            .query_row("SELECT secret, enabled, last_step, created_at FROM two_step WHERE user = ?1", params![user], |r| {
                Ok(TwoStepRow { secret: r.get(0)?, enabled: r.get::<_, i64>(1)? == 1, last_step: r.get(2)?, created_at: r.get(3)? })
            })
            .optional()?)
    }

    /// The accounts with two-step on.
    pub fn two_step_users(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT user FROM two_step WHERE enabled = 1")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forget secrets that never got their first correct code.
    pub fn two_step_drop_waiting(&self, older_than: i64) -> Result<()> {
        self.lock().execute("DELETE FROM two_step WHERE enabled = 0 AND created_at < ?1", params![older_than])?;
        Ok(())
    }

    /// Keep a new secret until its first correct code. False (and nothing
    /// changed) when two-step is already on for the account.
    pub fn two_step_begin(&self, user: &str, secret: &str, now: i64) -> Result<bool> {
        let n = self.lock().execute(
            "INSERT INTO two_step (user, secret, enabled, last_step, created_at) VALUES (?1, ?2, 0, 0, ?3)
             ON CONFLICT(user) DO UPDATE SET secret = excluded.secret, last_step = 0, created_at = excluded.created_at WHERE enabled = 0",
            params![user, secret, now],
        )?;
        Ok(n == 1)
    }

    /// This secret, if it is still waiting for its first code, is forgotten;
    /// one that is on stays, and so does a newer start.
    pub fn two_step_cancel(&self, user: &str, secret: &str) -> Result<()> {
        self.lock().execute("DELETE FROM two_step WHERE user = ?1 AND enabled = 0 AND secret = ?2", params![user, secret])?;
        Ok(())
    }

    /// Turn it on, with the secret that was checked, the step of the code that
    /// proved it and the hashes of the recovery codes. False if that is not
    /// what is waiting (a newer start replaced it, or it is on already).
    pub fn two_step_enable(&self, user: &str, step: i64, secret: &str, recovery: &[String], now: i64) -> Result<bool> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let n = tx.execute("UPDATE two_step SET enabled = 1, last_step = ?2, created_at = ?3 WHERE user = ?1 AND enabled = 0 AND secret = ?4", params![user, step, now, secret])?;
        if n == 1 {
            tx.execute("DELETE FROM recovery_codes WHERE user = ?1", params![user])?;
            for h in recovery {
                tx.execute("INSERT INTO recovery_codes (user, hash) VALUES (?1, ?2)", params![user, h])?;
            }
        }
        tx.commit()?;
        Ok(n == 1)
    }

    /// Use up a time step: true for the first caller only (kept across restarts).
    pub fn two_step_use_step(&self, user: &str, step: i64) -> Result<bool> {
        Ok(self.lock().execute("UPDATE two_step SET last_step = ?2 WHERE user = ?1 AND enabled = 1 AND last_step < ?2", params![user, step])? == 1)
    }

    /// Forget an account's two-step secret and recovery codes. How many secrets went (0 or 1).
    pub fn two_step_remove(&self, user: &str) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM recovery_codes WHERE user = ?1", params![user])?;
        let n = tx.execute("DELETE FROM two_step WHERE user = ?1", params![user])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn recovery_hashes(&self, user: &str) -> Result<Vec<(i64, String)>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, hash FROM recovery_codes WHERE user = ?1")?;
        let rows = st.query_map(params![user], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Use up a recovery code: true for the first caller only.
    pub fn recovery_take(&self, id: i64) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM recovery_codes WHERE id = ?1", params![id])? == 1)
    }

    pub fn recovery_count(&self, user: &str) -> Result<i64> {
        Ok(self.lock().query_row("SELECT COUNT(*) FROM recovery_codes WHERE user = ?1", params![user], |r| r.get(0))?)
    }

    /// New recovery codes: the old ones stop working.
    pub fn recovery_replace(&self, user: &str, hashes: &[String]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM recovery_codes WHERE user = ?1", params![user])?;
        for h in hashes {
            tx.execute("INSERT INTO recovery_codes (user, hash) VALUES (?1, ?2)", params![user, h])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn token_insert(&self, t: &TokenRow) -> Result<()> {
        self.lock().execute(
            "INSERT INTO device_tokens (id, user, name, admin, hash, created_at, last_used) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![t.id, t.user, t.name, t.admin, t.hash, t.created_at, t.last_used],
        )?;
        Ok(())
    }

    pub fn token_delete(&self, id: &str) -> Result<()> {
        self.lock().execute("DELETE FROM device_tokens WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn token_delete_user(&self, user: &str) -> Result<()> {
        self.lock().execute("DELETE FROM device_tokens WHERE user = ?1", params![user])?;
        Ok(())
    }

    /// A demoted admin's control tokens.
    pub fn token_delete_admin(&self, user: &str) -> Result<()> {
        self.lock().execute("DELETE FROM device_tokens WHERE user = ?1 AND admin = 1", params![user])?;
        Ok(())
    }

    /// Everything kept for an account, all or nothing: its two-step secret,
    /// recovery codes and device tokens.
    pub fn account_data_delete(&self, user: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM recovery_codes WHERE user = ?1", params![user])?;
        tx.execute("DELETE FROM two_step WHERE user = ?1", params![user])?;
        tx.execute("DELETE FROM device_tokens WHERE user = ?1", params![user])?;
        tx.execute("DELETE FROM known_devices WHERE user = ?1", params![user])?;
        tx.commit()?;
        Ok(())
    }

    /// A change from the command line, for the server to apply ("password" or "enable").
    pub fn account_request_add(&self, user: &str, kind: &str, hash: &str, revoke_tokens: bool) -> Result<()> {
        self.lock().execute("INSERT INTO account_requests (user, kind, hash, revoke_tokens) VALUES (?1, ?2, ?3, ?4)", params![user, kind, hash, revoke_tokens])?;
        Ok(())
    }

    /// Oldest first.
    pub fn account_requests(&self) -> Result<Vec<AccountRequest>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, user, kind, hash, revoke_tokens, claimed FROM account_requests ORDER BY id")?;
        let rows = st.query_map([], |r| Ok(AccountRequest { id: r.get(0)?, user: r.get(1)?, kind: r.get(2)?, hash: r.get(3)?, revoke_tokens: r.get(4)?, claimed: r.get::<_, i64>(5)? != 0, superseded: r.get::<_, i64>(5)? == 2 }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// It is in the settings now: it is never applied again, also after a crash.
    pub fn account_request_claim(&self, id: i64) -> Result<()> {
        self.lock().execute("UPDATE account_requests SET claimed = 1 WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Whether this row is still waiting to go in the settings, as it was when read.
    pub fn account_request_waiting(&self, id: i64) -> Result<bool> {
        let n: i64 = self.lock().query_row("SELECT COUNT(*) FROM account_requests WHERE id = ?1 AND claimed = 0", params![id], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// The account's password or state was just changed another way: what still
    /// waits from before that must not overwrite it. Under the settings lock.
    pub fn account_requests_supersede(&self, user: &str, kind: &str) -> Result<()> {
        self.lock().execute("UPDATE account_requests SET claimed = 2 WHERE user = ?1 AND kind = ?2 AND claimed = 0", params![user, kind])?;
        Ok(())
    }

    /// (By id: a second run of the command made meanwhile stays.)
    pub fn account_request_done(&self, id: i64) -> Result<()> {
        self.lock().execute("DELETE FROM account_requests WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn token_touch(&self, id: &str, at: i64) -> Result<()> {
        self.lock().execute("UPDATE device_tokens SET last_used = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    pub fn token_all(&self) -> Result<Vec<TokenRow>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT id, user, name, admin, hash, created_at, last_used FROM device_tokens")?;
        let rows = st.query_map([], |r| {
            Ok(TokenRow { id: r.get(0)?, user: r.get(1)?, name: r.get(2)?, admin: r.get(3)?, hash: r.get(4)?, created_at: r.get(5)?, last_used: r.get(6)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
