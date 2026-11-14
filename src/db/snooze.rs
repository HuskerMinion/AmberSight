//! Per-account snoozes: one account's push notifications for one camera stop
//! until a time (see crate::snooze).

use super::*;

/// One snooze as stored. `camera` is the camera's slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snooze {
    pub camera: String,
    pub until_ms: i64,
}

impl Db {
    /// Snooze `user`'s notifications for `camera` (a slug) until `until_ms`;
    /// replaces an earlier snooze of the same camera.
    pub fn snooze_set(&self, user: &str, camera: &str, until_ms: i64) -> Result<()> {
        self.lock().execute(
            "INSERT INTO snoozes (user, camera, until_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(user, camera) DO UPDATE SET until_ms = excluded.until_ms",
            params![user, camera, until_ms],
        )?;
        Ok(())
    }

    /// Clear one snooze; false when there was none.
    pub fn snooze_clear(&self, user: &str, camera: &str) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM snoozes WHERE user = ?1 AND camera = ?2", params![user, camera])? > 0)
    }

    /// The account's snoozes that still run at `now_ms`, soonest end first.
    pub fn snooze_list(&self, user: &str, now_ms: i64) -> Result<Vec<Snooze>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT camera, until_ms FROM snoozes WHERE user = ?1 AND until_ms > ?2 ORDER BY until_ms, camera")?;
        let rows = st.query_map(params![user, now_ms], |r| Ok(Snooze { camera: r.get(0)?, until_ms: r.get(1)? }))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The accounts whose notifications for `camera` (a slug) are snoozed at `now_ms`.
    pub fn snoozed_users(&self, camera: &str, now_ms: i64) -> Result<std::collections::HashSet<String>> {
        let conn = self.lock();
        let mut st = conn.prepare("SELECT user FROM snoozes WHERE camera = ?1 AND until_ms > ?2")?;
        let rows = st.query_map(params![camera, now_ms], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Remove snoozes that ended by `now_ms`; returns how many.
    pub fn snooze_prune(&self, now_ms: i64) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM snoozes WHERE until_ms <= ?1", params![now_ms])?)
    }

    /// The camera `old` (a slug) is now `new`: its snoozes go with it,
    /// replacing any left under the new slug.
    pub fn snooze_rename_camera(&self, old: &str, new: &str) -> Result<()> {
        if old == new {
            return Ok(());
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM snoozes WHERE camera = ?1", params![new])?;
        tx.execute("UPDATE snoozes SET camera = ?2 WHERE camera = ?1", params![old, new])?;
        tx.commit()?;
        Ok(())
    }

    /// Remove every snooze of a camera (it was removed: a new camera of the
    /// same name must not inherit them).
    pub fn snooze_forget_camera(&self, camera: &str) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM snoozes WHERE camera = ?1", params![camera])?)
    }

    /// Remove every snooze of an account (it was deleted or disabled).
    pub fn snooze_delete_user(&self, user: &str) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM snoozes WHERE user = ?1", params![user])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snoozes_are_kept_per_account_and_end() {
        let db = Db::open_in_memory().unwrap();
        db.snooze_set("ann", "porch", 2000).unwrap();
        db.snooze_set("ann", "garage", 5000).unwrap();
        db.snooze_set("bob", "porch", 3000).unwrap();
        // Set again: the new end replaces the old.
        db.snooze_set("ann", "porch", 4000).unwrap();
        assert_eq!(db.snooze_list("ann", 1000).unwrap(), vec![Snooze { camera: "porch".into(), until_ms: 4000 }, Snooze { camera: "garage".into(), until_ms: 5000 }]);
        let porch = |at| db.snoozed_users("porch", at).unwrap();
        assert_eq!(porch(1000).len(), 2);
        assert!(porch(3500).contains("ann") && !porch(3500).contains("bob"), "bob's ended");
        assert!(porch(4000).is_empty(), "an end time is not snoozed any more");
        // Expired ones are left out of the list, and pruned.
        assert_eq!(db.snooze_list("ann", 4500).unwrap().len(), 1);
        assert_eq!(db.snooze_prune(4500).unwrap(), 2);
        assert_eq!(db.snooze_list("ann", 0).unwrap().len(), 1);
        assert!(db.snooze_clear("ann", "garage").unwrap());
        assert!(!db.snooze_clear("ann", "garage").unwrap());
        db.snooze_set("bob", "porch", 9000).unwrap();
        assert_eq!(db.snooze_delete_user("bob").unwrap(), 1);
        assert!(db.snoozed_users("porch", 0).unwrap().is_empty());
    }

    #[test]
    fn snoozes_follow_a_renamed_camera_and_go_with_a_removed_one() {
        let db = Db::open_in_memory().unwrap();
        db.snooze_set("ann", "porch", 9000).unwrap();
        db.snooze_set("bob", "porch", 9000).unwrap();
        db.snooze_set("bob", "front_porch", 5000).unwrap();
        db.snooze_rename_camera("porch", "front_porch").unwrap();
        assert!(db.snoozed_users("porch", 0).unwrap().is_empty(), "nothing left for a new camera of the old name");
        assert_eq!(db.snoozed_users("front_porch", 0).unwrap().len(), 2);
        assert_eq!(db.snooze_list("bob", 0).unwrap(), vec![Snooze { camera: "front_porch".into(), until_ms: 9000 }]);
        db.snooze_rename_camera("front_porch", "front_porch").unwrap();
        assert_eq!(db.snooze_forget_camera("front_porch").unwrap(), 2);
        assert!(db.snooze_list("ann", 0).unwrap().is_empty());
    }

    #[test]
    fn snoozes_survive_reopening() {
        let d = std::env::temp_dir().join(format!("amber-snooze-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("a.db");
        {
            let db = Db::open(&p).unwrap();
            db.snooze_set("ann", "porch", i64::MAX).unwrap();
        }
        let db = Db::open(&p).unwrap();
        assert_eq!(db.snooze_list("ann", 0).unwrap().len(), 1);
        drop(db);
        let _ = std::fs::remove_dir_all(&d);
    }
}
