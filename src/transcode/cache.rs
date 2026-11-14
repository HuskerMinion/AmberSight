//! A short-lived copy on disk of converted answers. A conversion has no byte
//! positions of its own, but a player (Android's ExoPlayer) reopens the same
//! URL with `Range: bytes=N-`. The conversion writes its output to a file as
//! it is made, every reader (the first request too) reads from that file, and
//! a Range is answered from it. An answer is dropped 10 minutes (by default)
//! after it was last used.
//!
//! The size of one answer and of all of them is capped, but a cap never cuts
//! a conversion that is being watched: an answer that reaches a cap is taken
//! out of the cache (no new request finds it, and it no longer counts toward
//! the total), its conversion goes on for the readers it has, and its file is
//! deleted when the last of them goes.
//!
//! The file name is a hash of the request (account, recording, caps, start,
//! quality) and a counter, never anything the caller sends. The key includes
//! the account, so one account's answer is never found by another.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Writing,
    Done,
    /// The conversion failed or was stopped: what was written stays readable
    /// for a Range inside it, but a new request starts a new conversion.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub written: u64,
    pub status: Status,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub entry_max: u64,
    pub total_max: u64,
    pub expire: Duration,
}

impl Limits {
    pub fn from_config(c: &super::TranscodeConfig) -> Limits {
        Limits { entry_max: c.cache_entry_mb.saturating_mul(1 << 20), total_max: c.cache_total_mb.saturating_mul(1 << 20), expire: Duration::from_secs(c.cache_seconds) }
    }
}

/// A conversion is stopped this long after its last reader went (a player
/// that closes and reopens the URL with a Range is back well within it).
pub const ABANDON: Duration = Duration::from_secs(10);

/// The most a reader waits for more bytes once some have come.
pub const WAIT: Duration = Duration::from_secs(30);

/// How far the conversion may run ahead of the furthest reader.
pub const AHEAD: u64 = 64 << 20;

/// A conversion held back for a reader that takes nothing for this long is
/// stopped (as a player that stops reading a streamed answer is dropped).
pub const STALLED: Duration = Duration::from_secs(45);

/// The most a request waits for another request for the same answer to
/// decide whether it converts.
const PENDING_WAIT: Duration = Duration::from_secs(30);

const CHUNK: u64 = 256 * 1024;

/// The cache key: a hash of everything that changes the output.
pub fn key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

static GLOBAL: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();

/// The server's cache (set up once, at start: `<data_dir>/tmp/transcode-cache`).
pub fn init(data_dir: &std::path::Path, limits: Limits) {
    GLOBAL.get_or_init(|| {
        let c = Cache::new(data_dir.join("tmp").join("transcode-cache"), limits);
        let c2 = c.clone();
        std::thread::Builder::new()
            .name("transcode-cache".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(1));
                c2.sweep(Instant::now());
            })
            .ok();
        c
    });
}

pub fn global() -> Option<&'static Cache> {
    GLOBAL.get()
}

/// The source tag of an event's answers (see `drop_source`).
pub fn event_source(id: &str) -> String {
    format!("event:{id}")
}

/// An event was deleted: its kept answers go.
pub fn forget_event(id: &str) {
    if let Some(c) = global() {
        c.drop_source(&event_source(id));
    }
}

enum Item {
    /// A request is deciding whether it converts (others for the same key
    /// wait for it). The sender is dropped when it decides, which wakes them.
    Pending { id: u64, tx: tokio::sync::watch::Sender<()> },
    Ready(Arc<Entry>),
}

struct State {
    map: HashMap<String, Item>,
    /// Bytes counted by the entries in the map.
    total: u64,
    /// Entries just taken out of the map: dropped (their files deleted) only
    /// after the lock is let go.
    dead: Vec<Arc<Entry>>,
}

/// The state, locked. Entries taken out while it was held are dropped after
/// the lock goes, so a file is never deleted under it.
struct Locked<'a> {
    // (Always Some until dropped.)
    guard: Option<std::sync::MutexGuard<'a, State>>,
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        let dead = self.guard.as_mut().map(|g| std::mem::take(&mut g.dead));
        self.guard = None;
        drop(dead);
    }
}

impl std::ops::Deref for Locked<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        self.guard.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut State {
        self.guard.as_mut().unwrap()
    }
}

impl State {
    /// Take the entry out of the map (if it is still there) and stop counting
    /// it: exactly what it counted, once.
    fn unlist(&mut self, e: &Entry) {
        if matches!(self.map.get(&e.key), Some(Item::Ready(x)) if std::ptr::eq(Arc::as_ptr(x), e)) {
            if let Some(Item::Ready(x)) = self.map.remove(&e.key) {
                self.dead.push(x);
            }
        }
        if e.listed.swap(false, Ordering::SeqCst) {
            self.total = self.total.saturating_sub(e.counted.load(Ordering::SeqCst));
        }
    }

    /// Evict the oldest idle entries (never `keep`) until the total is at
    /// most `max`; false when it cannot be.
    fn make_room(&mut self, max: u64, keep: Option<&Entry>) -> bool {
        let evictable = |m: &HashMap<String, Item>| -> Vec<Arc<Entry>> {
            m.values()
                .filter_map(|i| match i {
                    Item::Ready(e) if e.idle() && !keep.is_some_and(|k| std::ptr::eq(Arc::as_ptr(e), k)) => Some(e.clone()),
                    _ => None,
                })
                .collect()
        };
        if self.total <= max {
            return true;
        }
        let mut idle = evictable(&self.map);
        // Not enough even with all of them gone: none goes for nothing.
        let freeable: u64 = idle.iter().map(|e| e.counted.load(Ordering::SeqCst)).sum();
        if self.total.saturating_sub(freeable) > max {
            return false;
        }
        idle.sort_by_key(|e| *e.last_use.lock().unwrap());
        for e in idle {
            if self.total <= max {
                break;
            }
            self.unlist(&e);
        }
        self.total <= max
    }
}

struct Inner {
    dir: PathBuf,
    state: Mutex<State>,
    seq: AtomicU64,
    limits: Mutex<Limits>,
}

impl Inner {
    fn lock(&self) -> Locked<'_> {
        Locked { guard: Some(self.state.lock().unwrap()) }
    }
}

#[derive(Clone)]
pub struct Cache(Arc<Inner>);

pub struct Entry {
    /// The headers of the converted answer (all of them, kept on every reply).
    pub headers: Vec<(String, String)>,
    key: String,
    /// What it was made from (`event:<id>`, `camera:<name>`).
    source: String,
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
    tx: tokio::sync::watch::Sender<Progress>,
    readers: AtomicUsize,
    last_use: Mutex<Instant>,
    owner: Weak<Inner>,
    entry_max: u64,
    /// Bytes counted toward the total, and whether it is in the map: both
    /// changed only under the state lock.
    counted: AtomicU64,
    listed: AtomicBool,
    /// The furthest position a reader has asked for or read to.
    wanted: AtomicU64,
    ahead: AtomicU64,
    /// How long a reader waits for the first byte (ms): as long as every way
    /// the conversion may try can take to start.
    first_wait_ms: AtomicU64,
    /// How long a reader waits for more once bytes have come (ms).
    stall_ms: AtomicU64,
}

impl Drop for Entry {
    fn drop(&mut self) {
        *self.file.lock().unwrap() = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// What a request should do about the cache.
pub enum Lookup {
    /// Answer from this kept copy.
    Found(Arc<Entry>),
    /// Nothing usable is kept: this request may make it (others for the same
    /// key wait for it). Dropping the claim without `create` lets them go on.
    Claim(Claim),
    /// Nothing usable is kept, and this request does not start one.
    Miss,
}

enum Step {
    Done(Lookup),
    Wait(tokio::sync::watch::Receiver<()>),
}

/// The right to make the entry for a key.
pub struct Claim {
    cache: Cache,
    key: String,
    id: u64,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut st = self.cache.0.lock();
        if matches!(st.map.get(&self.key), Some(Item::Pending { id, .. }) if *id == self.id) {
            st.map.remove(&self.key);
        }
    }
}

impl Claim {
    /// Begin the entry for the conversion about to start. None: caching is
    /// off, or the total is full of answers still in use (the request is then
    /// streamed without a copy).
    pub fn create(self, headers: Vec<(String, String)>, source: &str) -> Option<Arc<Entry>> {
        let inner = &self.cache.0;
        let l = *inner.limits.lock().unwrap();
        if l.total_max == 0 || l.entry_max == 0 {
            return None;
        }
        self.cache.sweep(Instant::now());
        if !inner.lock().make_room(l.total_max.saturating_sub(1), None) {
            return None;
        }
        let name = format!("{}-{}.mp4", &self.key[..self.key.len().min(32)], inner.seq.fetch_add(1, Ordering::SeqCst));
        let path = inner.dir.join(name);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let file = o.open(&path).ok()?;
        let (tx, _) = tokio::sync::watch::channel(Progress { written: 0, status: Status::Writing });
        let e = Arc::new(Entry {
            headers,
            key: self.key.clone(),
            source: source.to_string(),
            path,
            file: Mutex::new(Some(file)),
            tx,
            readers: AtomicUsize::new(0),
            last_use: Mutex::new(Instant::now()),
            owner: Arc::downgrade(inner),
            entry_max: l.entry_max,
            counted: AtomicU64::new(0),
            listed: AtomicBool::new(true),
            wanted: AtomicU64::new(0),
            ahead: AtomicU64::new(AHEAD),
            first_wait_ms: AtomicU64::new(WAIT.as_millis() as u64),
            stall_ms: AtomicU64::new(WAIT.as_millis() as u64),
        });
        let mut st = inner.lock();
        if let Some(Item::Ready(old)) = st.map.get(&self.key) {
            let old = old.clone();
            st.unlist(&old);
        }
        // Replaces this claim's place holder (its sender goes: waiters look again).
        st.map.insert(self.key.clone(), Item::Ready(e.clone()));
        Some(e)
    }
}

/// Whether an entry may answer a request: one being written or done, or a
/// failed one only for a Range wholly inside what it wrote.
fn usable(p: Progress, range: Option<(u64, Option<u64>)>) -> bool {
    match p.status {
        Status::Writing | Status::Done => true,
        Status::Failed => matches!(range, Some((_, Some(to))) if to < p.written),
    }
}

impl Cache {
    /// The folder is emptied (a crash left files) and made private.
    pub fn new(dir: PathBuf, limits: Limits) -> Cache {
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        Cache(Arc::new(Inner { dir, state: Mutex::new(State { map: HashMap::new(), total: 0, dead: Vec::new() }), seq: AtomicU64::new(0), limits: Mutex::new(limits) }))
    }

    pub fn set_limits(&self, l: Limits) {
        *self.0.limits.lock().unwrap() = l;
    }

    #[cfg(test)]
    pub fn total(&self) -> u64 {
        self.0.lock().total
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.lock().map.values().filter(|i| matches!(i, Item::Ready(_))).count()
    }

    fn step(&self, key: &str, range: Option<(u64, Option<u64>)>, may_start: bool) -> Step {
        let mut st = self.0.lock();
        let found = match st.map.get(key) {
            Some(Item::Pending { tx, .. }) => return Step::Wait(tx.subscribe()),
            Some(Item::Ready(e)) => Some(e.clone()),
            None => None,
        };
        if let Some(e) = found {
            if usable(e.progress(), range) {
                e.touch();
                return Step::Done(Lookup::Found(e));
            }
            // Failed, and this request needs more than it wrote: it goes.
            st.unlist(&e);
        }
        if !may_start {
            return Step::Done(Lookup::Miss);
        }
        let id = self.0.seq.fetch_add(1, Ordering::SeqCst);
        let (tx, _) = tokio::sync::watch::channel(());
        st.map.insert(key.to_string(), Item::Pending { id, tx });
        Step::Done(Lookup::Claim(Claim { cache: self.clone(), key: key.to_string(), id }))
    }

    /// Find the kept answer for `key`, or claim the right to make it, as one
    /// step: a second request for the same answer at the same time waits for
    /// the first and then reads its copy (one conversion, one count).
    /// `range`: the Range asked for (None: from the start); `may_start`: this
    /// request would start a conversion when nothing is kept.
    pub async fn lookup(&self, key: &str, range: Option<(u64, Option<u64>)>, may_start: bool) -> Lookup {
        let end = tokio::time::Instant::now() + PENDING_WAIT;
        loop {
            match self.step(key, range, may_start) {
                Step::Done(l) => return l,
                Step::Wait(mut rx) => {
                    // Ends when the other request created its entry or gave up.
                    if tokio::time::timeout_at(end, rx.changed()).await.is_err() {
                        return Lookup::Miss;
                    }
                }
            }
        }
    }

    /// The kept answer that could answer this range (None: start) now; counts
    /// as a use.
    #[cfg(test)]
    pub fn find(&self, key: &str, range: Option<(u64, Option<u64>)>) -> Option<Arc<Entry>> {
        match self.step(key, range, false) {
            Step::Done(Lookup::Found(e)) => Some(e),
            _ => None,
        }
    }

    /// Find or begin (no waiting): the entry being written or done for this
    /// key, else a new one (tests, and callers with nothing to decide).
    #[cfg(test)]
    pub fn create(&self, key: &str, headers: Vec<(String, String)>) -> Option<Arc<Entry>> {
        match self.step(key, None, true) {
            Step::Done(Lookup::Found(e)) => Some(e),
            Step::Done(Lookup::Claim(c)) => c.create(headers, ""),
            _ => None,
        }
    }

    /// Drop every kept answer made from `source` (its readers finish).
    pub fn drop_source(&self, source: &str) {
        let mut st = self.0.lock();
        let gone: Vec<Arc<Entry>> = st
            .map
            .values()
            .filter_map(|i| match i {
                Item::Ready(e) if e.source == source => Some(e.clone()),
                _ => None,
            })
            .collect();
        for e in gone {
            st.unlist(&e);
        }
    }

    /// Drop what has not been used within the time (never one still being
    /// written or read).
    pub fn sweep(&self, now: Instant) {
        let expire = self.0.limits.lock().unwrap().expire;
        let mut st = self.0.lock();
        let gone: Vec<Arc<Entry>> = st
            .map
            .values()
            .filter_map(|i| match i {
                Item::Ready(e) if e.idle() && now.saturating_duration_since(*e.last_use.lock().unwrap()) >= expire => Some(e.clone()),
                _ => None,
            })
            .collect();
        for e in gone {
            st.unlist(&e);
        }
    }
}

impl Inner {
    /// Count `n` more bytes of `e`. At a cap the entry leaves the cache
    /// (after older idle answers were evicted for the total): never fails it.
    fn count(&self, e: &Entry, n: u64) {
        let l = *self.limits.lock().unwrap();
        let mut st = self.lock();
        if !e.listed.load(Ordering::SeqCst) {
            return;
        }
        let c = e.counted.fetch_add(n, Ordering::SeqCst) + n;
        st.total += n;
        if c > e.entry_max || (st.total > l.total_max && !st.make_room(l.total_max, Some(e))) {
            st.unlist(e);
        }
    }
}

impl Entry {
    pub fn progress(&self) -> Progress {
        *self.tx.borrow()
    }

    fn idle(&self) -> bool {
        self.progress().status != Status::Writing && self.readers.load(Ordering::SeqCst) == 0
    }

    /// Still in the cache (not dropped, evicted or over a cap).
    #[cfg(test)]
    pub fn listed(&self) -> bool {
        self.listed.load(Ordering::SeqCst)
    }

    pub fn touch(&self) {
        *self.last_use.lock().unwrap() = Instant::now();
    }

    /// The conversion is nobody's any more: nothing reads it and nothing has
    /// for a while.
    pub fn abandoned(&self, after: Duration) -> bool {
        self.readers.load(Ordering::SeqCst) == 0 && self.last_use.lock().unwrap().elapsed() >= after
    }

    pub fn set_first_wait(&self, d: Duration) {
        self.first_wait_ms.store(d.as_millis() as u64, Ordering::SeqCst);
    }

    /// How long a reader waits for bytes when `written` are there.
    pub fn wait_limit(&self, written: u64) -> Duration {
        let stall = Duration::from_millis(self.stall_ms.load(Ordering::SeqCst));
        if written == 0 {
            Duration::from_millis(self.first_wait_ms.load(Ordering::SeqCst)).max(stall)
        } else {
            stall
        }
    }

    #[cfg(test)]
    pub fn wanted(&self) -> u64 {
        self.wanted.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn set_ahead(&self, n: u64) {
        self.ahead.store(n, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn set_stall(&self, d: Duration) {
        self.stall_ms.store(d.as_millis() as u64, Ordering::SeqCst);
    }

    fn set(&self, written: u64, status: Status) {
        self.tx.send_replace(Progress { written, status });
    }

    /// Hold the conversion back while it is far ahead of every reader. False:
    /// stop it (nobody reads any more, or a reader took nothing for STALLED).
    fn pace(&self) -> bool {
        let ahead = self.ahead.load(Ordering::SeqCst);
        let mut last = self.wanted.load(Ordering::SeqCst);
        let mut since = Instant::now();
        loop {
            let want = self.wanted.load(Ordering::SeqCst);
            if self.progress().written < want.saturating_add(ahead) {
                return true;
            }
            if self.abandoned(ABANDON) {
                return false;
            }
            if want != last {
                (last, since) = (want, Instant::now());
            } else if since.elapsed() >= STALLED {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Append what the conversion made (blocks while it is far ahead of its
    /// readers). False: stop converting (the disk failed, nobody reads it, or
    /// it was marked failed). A size cap never stops it: see the module notes.
    pub fn write(&self, data: &[u8]) -> bool {
        if self.progress().status != Status::Writing {
            return false;
        }
        if !self.pace() {
            return self.fail();
        }
        let n = data.len() as u64;
        if let Some(inner) = self.owner.upgrade() {
            inner.count(self, n);
        }
        let ok = match self.file.lock().unwrap().as_mut() {
            Some(f) => f.write_all(data).is_ok(),
            None => false,
        };
        if !ok {
            return self.fail();
        }
        let p = self.progress();
        if p.status != Status::Writing {
            return false;
        }
        self.set(p.written + n, Status::Writing);
        true
    }

    /// The conversion ended complete.
    pub fn finish(&self) {
        *self.file.lock().unwrap() = None;
        let p = self.progress();
        if p.status == Status::Writing {
            self.set(p.written, Status::Done);
        }
    }

    /// The conversion ended short (always false: for `return e.fail()`).
    pub fn fail(&self) -> bool {
        *self.file.lock().unwrap() = None;
        let p = self.progress();
        if p.status == Status::Writing {
            self.set(p.written, Status::Failed);
        }
        false
    }

    /// Wait until more than `need` bytes are written or the conversion has
    /// ended, for at most `limit`.
    pub async fn wait_past(&self, need: u64, limit: Duration) -> Progress {
        let mut rx = self.tx.subscribe();
        let end = tokio::time::Instant::now() + limit;
        loop {
            let p = *rx.borrow_and_update();
            if p.written > need || p.status != Status::Writing {
                return p;
            }
            if tokio::time::timeout_at(end, rx.changed()).await.is_err() {
                return *rx.borrow();
            }
        }
    }

    /// The bytes from `from` (up to `end`, exclusive, when given), as they are
    /// written: waits for what is not there yet; an error when the conversion
    /// failed first.
    #[cfg(test)]
    pub fn read_from(self: &Arc<Self>, from: u64, end: Option<u64>) -> axum::body::Body {
        self.read_held(self.hold(from), from, end)
    }

    /// Count as a reader wanting the bytes up to `at` from now on (a request
    /// that waits for bytes before its answer starts is one: the conversion is
    /// not given up, and runs on to reach it).
    pub fn hold(self: &Arc<Self>, at: u64) -> Hold {
        self.readers.fetch_add(1, Ordering::SeqCst);
        self.wanted.fetch_max(at, Ordering::SeqCst);
        self.touch();
        Hold(Guard(self.clone()))
    }

    /// As `read_from`, the reader counted by `hold` (made for this entry).
    pub fn read_held(self: &Arc<Self>, hold: Hold, from: u64, end: Option<u64>) -> axum::body::Body {
        debug_assert!(Arc::ptr_eq(self, &hold.0 .0));
        self.wanted.fetch_max(from, Ordering::SeqCst);
        let st = ReadState { file: None, rx: self.tx.subscribe(), pos: from, end, guard: hold.0 };
        let s = futures::stream::unfold(st, |mut st| async move {
            let r = st.next().await?;
            Some((r, st))
        });
        axum::body::Body::from_stream(s)
    }
}

struct Guard(Arc<Entry>);

/// A reader counted before its answer starts (see `Entry::hold`).
pub struct Hold(Guard);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.readers.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

struct ReadState {
    // The file before the entry: closed first (Windows cannot delete an open file).
    file: Option<std::fs::File>,
    rx: tokio::sync::watch::Receiver<Progress>,
    pos: u64,
    end: Option<u64>,
    guard: Guard,
}

impl ReadState {
    async fn next(&mut self) -> Option<Result<Vec<u8>, std::io::Error>> {
        loop {
            if self.end.is_some_and(|e| self.pos >= e) {
                return None;
            }
            let p = *self.rx.borrow_and_update();
            if self.pos < p.written {
                let mut n = CHUNK.min(p.written - self.pos);
                if let Some(e) = self.end {
                    n = n.min(e - self.pos);
                }
                let (pos, path) = (self.pos, self.guard.0.path.clone());
                let file = self.file.take();
                let read = tokio::task::spawn_blocking(move || {
                    let mut f = match file {
                        Some(f) => f,
                        None => {
                            let mut f = std::fs::File::open(&path)?;
                            f.seek(SeekFrom::Start(pos))?;
                            f
                        }
                    };
                    let mut buf = vec![0u8; n as usize];
                    f.read_exact(&mut buf)?;
                    Ok::<_, std::io::Error>((f, buf))
                })
                .await;
                return match read {
                    Ok(Ok((f, buf))) => {
                        self.file = Some(f);
                        self.pos += n;
                        self.guard.0.wanted.fetch_max(self.pos, Ordering::SeqCst);
                        self.guard.0.touch();
                        Some(Ok(buf))
                    }
                    Ok(Err(e)) => {
                        self.end = Some(self.pos);
                        Some(Err(e))
                    }
                    Err(_) => {
                        self.end = Some(self.pos);
                        Some(Err(std::io::Error::other("read failed")))
                    }
                };
            }
            match p.status {
                Status::Done => return None,
                Status::Failed => {
                    self.end = Some(self.pos);
                    return Some(Err(std::io::Error::other("transcode failed")));
                }
                Status::Writing => {
                    // The first byte may wait for every way the conversion tries.
                    let limit = self.guard.0.wait_limit(p.written);
                    if tokio::time::timeout(limit, self.rx.changed()).await.is_err() {
                        self.end = Some(self.pos);
                        return Some(Err(std::io::Error::other("transcode stalled")));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(n: &str) -> PathBuf {
        std::env::temp_dir().join(format!("amber-tcache-{}-{n}", std::process::id()))
    }

    fn lim(entry: u64, total: u64, secs: u64) -> Limits {
        Limits { entry_max: entry, total_max: total, expire: Duration::from_secs(secs) }
    }

    #[test]
    fn the_key_covers_every_part_and_is_a_hash() {
        let a = key(&["bob", "clip1", "h264:1920x1080", "0", "low"]);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, key(&["bob", "clip1", "h264:1920x1080", "0", "low"]));
        assert_ne!(a, key(&["amy", "clip1", "h264:1920x1080", "0", "low"]), "another account");
        assert_ne!(a, key(&["bob", "clip1", "h264:1920x1080", "5000", "low"]), "another start");
        // The parts cannot be run together to make another key.
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
    }

    #[test]
    fn the_folder_is_cleaned_at_start_and_the_file_is_private() {
        let d = dir("clean");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("old.mp4"), b"x").unwrap();
        let c = Cache::new(d.clone(), lim(1000, 1000, 60));
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        let e = c.create(&key(&["u"]), vec![]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&e.path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert!(e.path.starts_with(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn a_reader_waits_for_bytes_the_writer_has_not_reached() {
        use futures::StreamExt;
        let c = Cache::new(dir("wait"), lim(1 << 20, 1 << 20, 60));
        let k = key(&["u", "a"]);
        let e = c.create(&k, vec![]).unwrap();
        assert!(e.write(b"0123456789"));
        let body = e.read_from(4, None);
        let mut s = body.into_data_stream();
        let first = s.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"456789");
        // Nothing more yet: the reader waits (no busy loop), then gets the next piece.
        let e2 = e.clone();
        let w = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            e2.write(b"abc");
            tokio::time::sleep(Duration::from_millis(50)).await;
            e2.finish();
        });
        let t0 = Instant::now();
        let more = s.next().await.unwrap().unwrap();
        assert_eq!(&more[..], b"abc");
        assert!(t0.elapsed() >= Duration::from_millis(80));
        assert!(s.next().await.is_none(), "done ends the stream");
        w.await.unwrap();
        // wait_past: a position beyond the end of a finished entry returns at once.
        let p = e.wait_past(100, Duration::from_secs(5)).await;
        assert_eq!((p.written, p.status), (13, Status::Done));
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[tokio::test]
    async fn a_failed_conversion_ends_readers_with_an_error_not_a_hang() {
        use futures::StreamExt;
        let c = Cache::new(dir("fail"), lim(1 << 20, 1 << 20, 60));
        let e = c.create(&key(&["u", "f"]), vec![]).unwrap();
        e.write(b"abcd");
        let mut s = e.read_from(0, None).into_data_stream();
        assert_eq!(&s.next().await.unwrap().unwrap()[..], b"abcd");
        e.fail();
        assert!(s.next().await.unwrap().is_err());
        let p = e.wait_past(10, Duration::from_secs(5)).await;
        assert_eq!((p.written, p.status), (4, Status::Failed));
        assert!(!e.write(b"more"), "no writes after it failed");
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[tokio::test]
    async fn an_entry_over_its_size_leaves_the_cache_but_keeps_converting() {
        use futures::StreamExt;
        let d = dir("cap");
        let c = Cache::new(d.clone(), lim(10, 1000, 60));
        let k = key(&["u", "c"]);
        let e = c.create(&k, vec![]).unwrap();
        assert!(e.write(b"123456"));
        let mut s = e.read_from(0, None).into_data_stream();
        assert!(e.write(b"7890123"), "past the cap: the conversion goes on for its reader");
        assert_eq!(e.progress(), Progress { written: 13, status: Status::Writing });
        assert!(!e.listed());
        assert_eq!(c.total(), 0, "no longer counted");
        assert!(c.find(&k, None).is_none(), "no new request finds it");
        assert!(e.write(b"45"));
        e.finish();
        let mut got = Vec::new();
        while let Some(b) = s.next().await {
            got.extend_from_slice(&b.unwrap());
        }
        assert_eq!(got, b"123456789012345");
        let path = e.path.clone();
        drop(s);
        drop(e);
        assert!(!path.exists(), "the file goes with its last reader");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn entries_expire_after_their_last_use_and_the_file_goes() {
        let d = dir("expire");
        let c = Cache::new(d.clone(), lim(1000, 1000, 600));
        let k = key(&["u", "x"]);
        let e = c.create(&k, vec![]).unwrap();
        e.write(b"hello");
        let path = e.path.clone();
        // Still being written: never dropped.
        c.sweep(Instant::now() + Duration::from_secs(10_000));
        assert_eq!(c.len(), 1);
        e.finish();
        drop(e);
        c.sweep(Instant::now() + Duration::from_secs(599));
        assert_eq!(c.len(), 1, "not yet");
        assert!(c.find(&k, None).is_some(), "a use");
        c.sweep(Instant::now() + Duration::from_secs(599));
        assert_eq!(c.len(), 1, "the use started the time again");
        c.sweep(Instant::now() + Duration::from_secs(601));
        assert_eq!(c.len(), 0);
        assert_eq!(c.total(), 0);
        assert!(!path.exists(), "the file is deleted");
        assert!(c.find(&k, None).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_oldest_entries_go_first_when_the_total_is_over() {
        let c = Cache::new(dir("evict"), lim(100, 25, 600));
        let mk = |n: &str, bytes: &[u8]| {
            let e = c.create(&key(&[n]), vec![]).unwrap();
            assert!(e.write(bytes));
            e.finish();
            std::thread::sleep(Duration::from_millis(5));
        };
        mk("a", &[1; 10]);
        mk("b", &[2; 10]);
        assert_eq!(c.total(), 20);
        // 20 + 10 would pass 25 while writing: a, the oldest, goes.
        let e = c.create(&key(&["c"]), vec![]).unwrap();
        assert!(e.write(&[3; 10]));
        assert!(c.find(&key(&["a"]), None).is_none(), "oldest evicted");
        assert!(c.find(&key(&["b"]), None).is_some());
        assert_eq!(c.total(), 20);
        // One being written is not evicted for itself: when nothing else can
        // go it leaves the cache, and its conversion goes on.
        let e2 = c.create(&key(&["d"]), vec![]).unwrap();
        assert!(e2.write(&[4; 4]));
        assert!(e2.write(&[4; 20]), "a full cache never stops a conversion");
        assert_eq!(e2.progress(), Progress { written: 24, status: Status::Writing });
        assert!(!e2.listed() && c.find(&key(&["d"]), None).is_none());
        assert_eq!(c.total(), 20, "b and c; d's bytes left the total with it");
        // Full of answers in use: no new entry (the request streams without one).
        let _rb = c.find(&key(&["b"]), None).unwrap().read_from(0, None);
        let _rc = c.find(&key(&["c"]), None).unwrap().read_from(0, None);
        let full = Cache::new(dir("evict-full"), lim(100, 20, 600));
        let a = full.create(&key(&["a"]), vec![]).unwrap();
        assert!(a.write(&[1; 20]));
        assert!(full.create(&key(&["b"]), vec![]).is_none(), "nothing to evict");
        let _ = std::fs::remove_dir_all(&full.0.dir);
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[test]
    fn an_entry_being_read_is_not_evicted_or_expired() {
        let c = Cache::new(dir("busy"), lim(100, 1000, 1));
        let k = key(&["r"]);
        let e = c.create(&k, vec![]).unwrap();
        e.write(b"abc");
        e.finish();
        let _body = e.read_from(0, None);
        c.sweep(Instant::now() + Duration::from_secs(100));
        assert_eq!(c.len(), 1);
        drop(_body);
        c.sweep(Instant::now() + Duration::from_secs(100));
        assert_eq!(c.len(), 0);
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[test]
    fn a_second_create_for_the_same_key_joins_and_nothing_leaks() {
        let c = Cache::new(dir("leak"), lim(1000, 1000, 600));
        let k = key(&["same"]);
        let a = c.create(&k, vec![]).unwrap();
        let b = c.create(&k, vec![]).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "joined, not replaced");
        assert!(a.write(&[0; 100]));
        a.finish();
        assert_eq!(c.total(), 100);
        drop(a);
        drop(b);
        c.sweep(Instant::now() + Duration::from_secs(10_000));
        assert_eq!(c.len(), 0);
        assert_eq!(c.total(), 0, "no bytes left in the total");
        // A failed entry replaced by a new one: its bytes leave the total once.
        let f = c.create(&k, vec![]).unwrap();
        assert!(f.write(&[0; 30]));
        f.fail();
        let g = c.create(&k, vec![]).unwrap();
        assert!(!Arc::ptr_eq(&f, &g));
        assert!(!f.write(&[0; 5]));
        assert!(g.write(&[0; 7]));
        assert_eq!(c.total(), 7);
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[test]
    fn a_failed_copy_answers_only_ranges_inside_what_it_wrote() {
        let c = Cache::new(dir("failfind"), lim(1000, 1000, 600));
        let k = key(&["f"]);
        let e = c.create(&k, vec![]).unwrap();
        e.write(b"abcdef");
        e.fail(); // e.g. stopped when nobody read it
        assert!(c.find(&k, Some((1, Some(4)))).is_some(), "a range inside");
        assert!(c.find(&k, Some((0, Some(5)))).is_some());
        assert!(c.find(&k, Some((2, Some(6)))).is_none(), "past what was written");
        assert!(c.find(&k, None).is_none(), "a fresh request");
        assert_eq!(c.len(), 0, "and it went: the next request converts again");
        drop(e);
        let e = c.create(&k, vec![]).unwrap();
        e.write(b"abc");
        e.fail();
        assert!(c.find(&k, Some((1, None))).is_none(), "an open range reaches past it");
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[test]
    fn a_full_cache_of_busy_entries_does_not_stop_a_live_answer() {
        let c = Cache::new(dir("full"), lim(1000, 1000, 600));
        let a = c.create(&key(&["a"]), vec![]).unwrap();
        assert!(a.write(&[0; 900]));
        let b = c.create(&key(&["b"]), vec![]).unwrap();
        assert!(b.write(&[0; 50]));
        assert!(b.write(&[0; 100]), "b's viewer keeps its stream");
        assert!(a.listed() && !b.listed());
        assert_eq!(c.total(), 900);
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[tokio::test]
    async fn simultaneous_cold_requests_share_one_entry() {
        let c = Cache::new(dir("claim"), lim(1000, 1000, 600));
        let k = key(&["cold"]);
        let Lookup::Claim(first) = c.lookup(&k, None, true).await else { panic!("the first claims") };
        // The second waits for the first to decide.
        let (c2, k2) = (c.clone(), k.clone());
        let second = tokio::spawn(async move { c2.lookup(&k2, None, true).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!second.is_finished(), "waits while the first decides");
        let e = first.create(vec![], "camera:x").unwrap();
        match second.await.unwrap() {
            Lookup::Found(s) => assert!(Arc::ptr_eq(&s, &e), "joined"),
            _ => panic!("the second must join the first"),
        }
        // A claim given up (the first did not convert): the next one claims.
        let k = key(&["cold2"]);
        let Lookup::Claim(first) = c.lookup(&k, None, true).await else { panic!() };
        let (c2, k2) = (c.clone(), k.clone());
        let second = tokio::spawn(async move { c2.lookup(&k2, None, true).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(first);
        assert!(matches!(second.await.unwrap(), Lookup::Claim(_)));
        // A Range that would not start a conversion does not claim.
        assert!(matches!(c.lookup(&key(&["none"]), Some((100, None)), false).await, Lookup::Miss));
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[tokio::test]
    async fn answers_from_a_deleted_event_go() {
        let c = Cache::new(dir("source"), lim(1000, 1000, 600));
        let mk = |k: &str, src: &str| {
            let Step::Done(Lookup::Claim(cl)) = c.step(&key(&[k]), None, true) else { panic!() };
            let e = cl.create(vec![], src).unwrap();
            assert!(e.write(&[1; 10]));
            e.finish();
            e
        };
        let a = mk("a", &event_source("e1"));
        let _b = mk("b", &event_source("e2"));
        let _c = mk("c", &event_source("e1"));
        let r = a.read_from(0, None);
        c.drop_source(&event_source("e1"));
        assert_eq!((c.len(), c.total()), (1, 10));
        assert!(c.find(&key(&["a"]), None).is_none() && c.find(&key(&["b"]), None).is_some());
        // A reader still going finishes.
        assert_eq!(axum::body::to_bytes(r, 100).await.unwrap().len(), 10);
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[test]
    fn the_conversion_waits_for_its_reader_and_stops_soon_after_it_goes() {
        assert!(ABANDON <= Duration::from_secs(10), "an encoder slot is not held long for nobody");
        let c = Cache::new(dir("pace"), lim(1 << 20, 1 << 20, 600));
        let e = c.create(&key(&["p"]), vec![]).unwrap();
        e.set_ahead(100);
        let body = e.read_from(0, None);
        assert!(e.write(&[0; 60]));
        assert!(e.write(&[0; 60]), "up to the limit ahead");
        // Now 120 written, the reader at 0: the next write waits until it reads.
        let (e2, t0) = (e.clone(), Instant::now());
        let w = std::thread::spawn(move || e2.write(&[0; 10]));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!w.is_finished(), "held back");
        e.wanted.fetch_max(50, Ordering::SeqCst);
        assert!(w.join().unwrap());
        assert!(t0.elapsed() >= Duration::from_millis(140));
        // The reader goes: the conversion is nobody's after ABANDON.
        drop(body);
        assert!(!e.abandoned(ABANDON));
        *e.last_use.lock().unwrap() = Instant::now() - ABANDON;
        assert!(e.abandoned(ABANDON));
        assert!(e.write(&[0; 30]), "still within the limit ahead");
        assert!(!e.write(&[0; 1]), "held back for nobody: stops");
        assert_eq!(e.progress().status, Status::Failed);
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[tokio::test]
    async fn the_first_byte_waits_as_long_as_the_tries_can_take() {
        use futures::StreamExt;
        let c = Cache::new(dir("first"), lim(1 << 20, 1 << 20, 600));
        let e = c.create(&key(&["w"]), vec![]).unwrap();
        e.set_first_wait(Duration::from_secs(60));
        assert_eq!(e.wait_limit(0), Duration::from_secs(60));
        assert_eq!(e.wait_limit(1), WAIT, "later stalls: the usual wait");
        e.set_first_wait(Duration::from_secs(1));
        assert_eq!(e.wait_limit(0), WAIT, "never less than the usual wait");
        // Scaled down: later stalls 100 ms, the first byte 600 ms.
        e.set_stall(Duration::from_millis(100));
        e.set_first_wait(Duration::from_millis(600));
        let mut s = e.read_from(0, None).into_data_stream();
        let e2 = e.clone();
        let w = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            e2.write(b"late");
        });
        assert_eq!(&s.next().await.unwrap().unwrap()[..], b"late", "a slow start is waited for");
        w.await.unwrap();
        let t0 = Instant::now();
        assert!(s.next().await.unwrap().is_err(), "a stall after it is not");
        assert!(t0.elapsed() < Duration::from_millis(500));
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[tokio::test]
    async fn a_request_waiting_for_its_bytes_is_a_reader() {
        // AVPlayer's bytes=0-1 while the first try on the graphics card takes its 15 s.
        let c = Cache::new(dir("held"), lim(1 << 20, 1 << 20, 60));
        let e = c.create(&key(&["h"]), vec![]).unwrap();
        e.set_first_wait(Duration::from_secs(60));
        let hold = e.hold(1);
        let e2 = e.clone();
        let w = tokio::spawn(async move { e2.wait_past(1, e2.wait_limit(0)).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        *e.last_use.lock().unwrap() = Instant::now() - ABANDON - Duration::from_secs(1);
        assert!(!e.abandoned(ABANDON), "a waiting request keeps the conversion");
        tokio::task::spawn_blocking({
            let e = e.clone();
            move || assert!(e.write(b"ab"))
        })
        .await
        .unwrap();
        assert_eq!(w.await.unwrap().written, 2);
        // The hold passes to the body: still one reader, then none.
        let body = e.read_held(hold, 0, Some(2));
        assert_eq!(e.readers.load(Ordering::SeqCst), 1);
        assert_eq!(&axum::body::to_bytes(body, 10).await.unwrap()[..], b"ab");
        assert_eq!(e.readers.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }

    #[tokio::test]
    async fn a_far_range_moves_the_conversion_on_to_it() {
        let c = Cache::new(dir("far"), lim(1 << 20, 1 << 20, 60));
        let e = c.create(&key(&["far"]), vec![]).unwrap();
        e.set_ahead(100);
        // A first reader that holds on but reads nothing (a player that seeks).
        let _first = e.read_from(0, None);
        let w = e.clone();
        let writer = std::thread::spawn(move || {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(5) && w.write(&[0u8; 50]) {}
        });
        // A seek: Range 1000- waits for byte 1000, held as serve does.
        let _hold = e.hold(1000);
        let p = e.wait_past(1000, Duration::from_secs(4)).await;
        e.fail();
        writer.join().unwrap();
        assert!(p.written > 1000, "reached: written {}", p.written);
    }

    #[test]
    fn a_files_entry_is_dropped_after_the_lock_goes() {
        // A sweep that drops the last reference deletes the file outside the
        // lock: the lock is free again while the entry is dropped.
        let c = Cache::new(dir("dead"), lim(1000, 1000, 1));
        let e = c.create(&key(&["d"]), vec![]).unwrap();
        e.write(b"x");
        e.finish();
        let path = e.path.clone();
        drop(e);
        c.sweep(Instant::now() + Duration::from_secs(5));
        assert!(!path.exists());
        assert!(c.0.state.lock().unwrap().dead.is_empty());
        assert!(c.0.state.try_lock().is_ok());
        let _ = std::fs::remove_dir_all(&c.0.dir);
    }
}
