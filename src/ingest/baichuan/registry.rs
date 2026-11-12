//! A process-wide map from camera UID to its running [`BaichuanClient`], so the
//! ingest reader (`recorder::open_stream`) can find the persistent session the
//! event poller (`engine::camera_events`) created, without threading a handle
//! through `StreamSource`. The same pattern the push-stream reader uses.

use super::client::BaichuanClient;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

fn table() -> &'static Mutex<HashMap<String, Arc<BaichuanClient>>> {
    static T: OnceLock<Mutex<HashMap<String, Arc<BaichuanClient>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register (or replace) the client for `uid`. A replaced client is returned so
/// the caller can shut it down.
pub fn register(uid: &str, client: Arc<BaichuanClient>) -> Option<Arc<BaichuanClient>> {
    table().lock().unwrap().insert(uid.to_string(), client)
}

/// Remove the client for `uid` if it is the same handle (idempotent on restart).
pub fn unregister(uid: &str, client: &Arc<BaichuanClient>) {
    let mut t = table().lock().unwrap();
    if t.get(uid).is_some_and(|c| Arc::ptr_eq(c, client)) {
        t.remove(uid);
    }
}

/// The client currently serving `uid`, if any.
pub fn get(uid: &str) -> Option<Arc<BaichuanClient>> {
    table().lock().unwrap().get(uid).cloned()
}
