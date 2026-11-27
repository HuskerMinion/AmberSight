//! Short command/answer exchanges on a device's shared Baichuan TCP session
//! (`wired.rs`), for the camera controls: what the camera has, the state of
//! its lights, and the few writes the controls may send.
//!
//! One exchange at a time per device: the next request goes out only when the
//! one before has its answer (or gave up waiting). Answers are matched to the
//! request by message id and message number. A session that is only wanted
//! for controls stays signed in for CONTROL_IDLE after the last exchange
//! (pages ask a few times in a row), then signs out like an idle stream.
//!
//! Battery cameras and channels of hubs and recorders are refused here: the
//! controls never wake a battery camera, and a hub's channel may be one. The
//! one exception is the siren's sound (`hub_ok`, for its messages only),
//! asked for only after the controls saw the hub report that camera awake.
//!
//! An exchange can be a job of several messages (a sound sent in pieces):
//! each goes out when the one before was answered with 200, and nothing
//! else is sent in between.

use super::bc;
use super::client::Session;
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::SyncSender;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one exchange may wait for its answer.
#[cfg(not(test))]
pub const ANSWER_WAIT: Duration = Duration::from_secs(6);
#[cfg(test)]
pub const ANSWER_WAIT: Duration = Duration::from_secs(2);
/// A session wanted only for controls signs out this long after the last
/// exchange that keeps it (a write, or the ask of what the camera has; a
/// page's state read does not keep it).
pub const CONTROL_IDLE: Duration = Duration::from_secs(60);

/// The words of a refusal for a battery camera or a hub's channel.
pub const NOT_FOR_BATTERY: &str = "Controls over Reolink port 9000 are not for battery cameras: asking would wake them.";
pub const NOT_FOR_HUB: &str = "Controls over Reolink port 9000 work for a camera of its own so far, not for a camera behind a hub or recorder.";
/// The siren's sound messages: the only ones `hub_ok` lets go to a hub's channel.
pub const SOUND_MSGS: &[u32] = &[bc::MSG_AUDIO_FILE_INFO, bc::MSG_AUDIO_FILE_SEND, bc::MSG_AUDIO_FILE_DONE, bc::MSG_AUDIO_CFG_GET, bc::MSG_AUDIO_CFG_SET, bc::MSG_AUDIO_ENCODE_ABILITY];
/// The start of the words for a request that was never sent (the camera has
/// not had it: nothing can be playing because of it).
pub const NOT_SENT: &str = "not sent to the camera";

/// One exchange to send.
pub struct Request {
    pub msg_id: u32,
    pub channel: u8,
    pub ext: String,
    pub body: String,
    /// One of `bc::CONTROL_WRITES`.
    pub write: bool,
    /// Sent ahead of everything waiting (the siren's stop).
    pub urgent: bool,
    /// Keeps a controls-only session signed in for CONTROL_IDLE.
    pub keep: bool,
    /// Never sent after this (whoever asked has stopped waiting: a write
    /// built from an old read must not reach the camera late), and its answer
    /// is not waited for past it.
    pub deadline: Instant,
    pub reply: SyncSender<Result<Answer, String>>,
    /// More messages of the same job, in order (empty: one exchange).
    pub then: Vec<Step>,
    /// May go to a channel of a hub (the caller checked it is awake): only
    /// when every message of it is one of `SOUND_MSGS`.
    pub hub_ok: bool,
}

/// One more message of a job: sent once the one before it was answered
/// with 200 (any other answer ends the job and is the job's answer).
#[derive(Clone, Debug)]
pub struct Step {
    pub msg_id: u32,
    pub ext: String,
    pub payload: Vec<u8>,
    /// The payload goes as is (binary data), not encrypted.
    pub raw: bool,
    /// The message number of the message before (the pieces of a sound share one).
    pub same_num: bool,
}

impl Request {
    pub fn fail(self, why: String) {
        let _ = self.reply.send(Err(why));
    }
}

/// The camera's answer: its status (200: done) and the XML in the clear.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub xml: String,
    /// Which message of a job answered (0: the first; for one exchange, always 0).
    pub step: usize,
}

struct Flight {
    msg_id: u32,
    msg_num: u16,
    until: Instant,
    /// `until` is the request's (or job's) deadline, not ANSWER_WAIT.
    by_deadline: bool,
    keep: bool,
    reply: SyncSender<Result<Answer, String>>,
    job: Option<Job>,
}

/// The rest of a running job.
struct Job {
    channel: u8,
    write: bool,
    deadline: Instant,
    rest: VecDeque<Step>,
    /// The message (0: the first) now on its way.
    step: usize,
}

/// A job whose last message was answered with 200: its message number, whether it keeps the session, and who waits.
type NextStep = (Job, u16, bool, SyncSender<Result<Answer, String>>);

/// The controls' exchanges on one session (owned by the session thread).
#[derive(Default)]
pub(super) struct Queue {
    waiting: VecDeque<Request>,
    flight: Option<Flight>,
    /// A job's next message: it goes before anything else.
    next: Option<NextStep>,
    /// The last exchange that keeps the session.
    last: Option<Instant>,
    key: String,
}

/// When each device's session last carried a control exchange that keeps it
/// (by host:port): `wired::release` leaves such a session alone (it carries
/// no video).
static RECENT: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// A control exchange that keeps the session ran on `key`'s session within CONTROL_IDLE.
pub fn recent(key: &str) -> bool {
    RECENT.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(key)).is_some_and(|t| t.elapsed() < CONTROL_IDLE)
}

/// May this request go to a hub's channel (`hub_ok`, and only the siren's sound)?
fn hub_ok(r: &Request) -> bool {
    r.hub_ok && SOUND_MSGS.contains(&r.msg_id) && r.then.iter().all(|s| SOUND_MSGS.contains(&s.msg_id))
}

/// May this request be sent at all (the read-only list, or the control writes)?
fn on_the_list(r: &Request) -> bool {
    let list = if r.write { bc::CONTROL_WRITES } else { bc::ALLOWED };
    list.contains(&r.msg_id) && r.then.iter().all(|s| list.contains(&s.msg_id))
}

impl Queue {
    pub(super) fn new(key: &str) -> Self {
        Queue { key: key.to_string(), ..Default::default() }
    }

    fn touch(&mut self) {
        let now = Instant::now();
        self.last = Some(now);
        RECENT.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(self.key.clone(), now);
    }

    /// Take a request (refused at once for a battery camera, or a device that
    /// is or may be a hub or recorder: see [`is_multi`]).
    pub(super) fn accept(&mut self, r: Request, battery: bool, multi: bool) {
        if battery {
            return r.fail(format!("{NOT_FOR_BATTERY} ({NOT_SENT})"));
        }
        if (multi || r.channel != 0) && !hub_ok(&r) {
            return r.fail(format!("{NOT_FOR_HUB} ({NOT_SENT})"));
        }
        if r.keep {
            self.touch();
        }
        if r.urgent {
            // Ahead of everything waiting, but never ahead of a siren play that
            // waits: a stop sent before the play it is for would not stop it.
            let at = self.waiting.iter().rposition(|w| w.msg_id == bc::MSG_AUDIO_PLAY).map_or(0, |i| i + 1);
            self.waiting.insert(at, r);
        } else {
            self.waiting.push_back(r);
        }
    }

    /// Something is waiting or running, or kept the session a moment ago.
    pub(super) fn busy(&self) -> bool {
        !self.waiting.is_empty() || self.flight.is_some() || self.next.is_some() || self.last.is_some_and(|t| t.elapsed() < CONTROL_IDLE)
    }

    /// The next request that may go out now: requests past their deadline,
    /// and any not on the lists, are failed (each on its own; the session
    /// goes on).
    fn next_sendable(&mut self, now: Instant) -> Option<Request> {
        while let Some(r) = self.waiting.pop_front() {
            if now >= r.deadline {
                r.fail(format!("waited too long for the camera; {NOT_SENT}"));
                continue;
            }
            if !on_the_list(&r) {
                let id = r.msg_id;
                r.fail(format!("message {id} is not one AmberSight sends; {NOT_SENT}"));
                continue;
            }
            return Some(r);
        }
        None
    }

    /// Send the next request when none is running; give up on an answer that
    /// is late. Err: the session could not send (it is broken).
    pub(super) fn pump(&mut self, s: &mut Session) -> Result<(), String> {
        let now = Instant::now();
        if self.flight.as_ref().is_some_and(|f| now >= f.until) {
            let f = self.flight.take().unwrap();
            let why = match (&f.job, f.by_deadline) {
                (Some(j), true) => format!("the job took too long: message {} of {} was not answered in time", j.step + 1, j.step + 1 + j.rest.len()),
                (Some(j), false) => format!("the camera did not answer within {} s (message {} of {} of the job)", ANSWER_WAIT.as_secs(), j.step + 1, j.step + 1 + j.rest.len()),
                (None, true) => "the camera did not answer in time".to_string(),
                (None, false) => format!("the camera did not answer within {} s", ANSWER_WAIT.as_secs()),
            };
            let _ = f.reply.send(Err(why));
        }
        if self.flight.is_some() {
            return Ok(());
        }
        // The next message of a job: nothing goes in between.
        if let Some((mut job, num, keep, reply)) = self.next.take() {
            let step = job.rest.pop_front().expect("a job waits only with a message left");
            job.step += 1;
            if now >= job.deadline {
                // (Not NOT_SENT: the messages before it reached the camera.)
                let _ = reply.send(Err(format!("the job took too long: stopped after message {} of {}", job.step, job.step + 1 + job.rest.len())));
                return Ok(());
            }
            if keep {
                self.touch();
            }
            let n = if step.same_num { Some(num) } else { None };
            return match s.send_ctl(step.msg_id, job.channel, n, &step.ext, &step.payload, step.raw, job.write) {
                Ok(msg_num) => {
                    let by_deadline = job.deadline < now + ANSWER_WAIT;
                    let until = (now + ANSWER_WAIT).min(job.deadline);
                    self.flight = Some(Flight { msg_id: step.msg_id, msg_num, until, by_deadline, keep, reply, job: Some(job) });
                    Ok(())
                }
                Err(e) => {
                    let why = format!("sending to the camera (message {} of the job): {e:#}", job.step + 1);
                    let _ = reply.send(Err(why.clone()));
                    Err(why)
                }
            };
        }
        let Some(r) = self.next_sendable(now) else { return Ok(()) };
        if r.keep {
            self.touch();
        }
        match s.send_parts(r.msg_id, r.channel, &r.ext, &r.body, r.write) {
            Ok(msg_num) => {
                let job = (!r.then.is_empty()).then(|| Job { channel: r.channel, write: r.write, deadline: r.deadline, rest: r.then.into(), step: 0 });
                let by_deadline = r.deadline < now + ANSWER_WAIT;
                self.flight = Some(Flight { msg_id: r.msg_id, msg_num, until: (now + ANSWER_WAIT).min(r.deadline), by_deadline, keep: r.keep, reply: r.reply, job });
                Ok(())
            }
            Err(e) => {
                let why = format!("sending to the camera: {e:#}");
                let _ = r.reply.send(Err(why.clone()));
                Err(why)
            }
        }
    }

    /// The answer to the running request, if `m` is it (then it is used up).
    pub(super) fn take(&mut self, s: &Session, m: &bc::Msg) -> bool {
        let Some(f) = &self.flight else { return false };
        if m.header.msg_id != f.msg_id || m.header.msg_num != f.msg_num {
            return false;
        }
        let f = self.flight.take().unwrap();
        if f.keep {
            self.touch();
        }
        let step = f.job.as_ref().map_or(0, |j| j.step);
        match f.job {
            // Done: the job's next message goes out on the next pump.
            Some(j) if m.header.status == 200 && !j.rest.is_empty() => self.next = Some((j, f.msg_num, f.keep, f.reply)),
            _ => {
                let xml = s.reply_xml(&m.header, &m.body);
                let _ = f.reply.send(Ok(Answer { status: m.header.status, xml, step }));
            }
        }
        true
    }

    /// The session ended: everything waiting is told why.
    pub(super) fn fail_all(&mut self, why: &str) {
        if let Some(f) = self.flight.take() {
            let _ = f.reply.send(Err(why.to_string()));
        }
        if let Some((j, _, _, reply)) = self.next.take() {
            // (Not NOT_SENT: the job's first messages reached the camera.)
            let _ = reply.send(Err(format!("{why}; the job stopped after message {} of {}", j.step + 1, j.step + 1 + j.rest.len())));
        }
        for r in self.waiting.drain(..) {
            r.fail(format!("{why}; {NOT_SENT}"));
        }
    }
}

/// A device that is, or may be, a hub or recorder (cameras on channels, some
/// of them maybe battery cameras): not exactly one channel (or no channel
/// count in its sign-in reply), or a type that says hub or recorder. (Seen
/// on the lab's cameras: E1 Pro `wifi_solo_ipc`, E1 Outdoor SE and Duo
/// Floodlight `ipc`, all with channelNum 1.)
pub fn is_multi(device_type: Option<&str>, channels: Option<u32>) -> bool {
    let t = device_type.unwrap_or("").to_ascii_lowercase();
    channels.is_none_or(|n| n != 1) || t.contains("hub") || t.contains("nvr")
}

// ------------------------------------------------------------- the XML

/// Escape text for an XML element.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The extension that names the channel a command is for.
pub fn channel_ext(channel: u8) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<channelId>{channel}</channelId>\n</Extension>\n")
}

/// The extension of the ability question (151): the account, and which parts.
pub fn ability_ext(user: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<userName>{}</userName>\n<token>image</token>\n</Extension>\n", esc(user))
}

/// `onOff` with a timed siren (`playMode` 0, `playTimes` rounds). UNPROVEN:
/// public descriptions give 1 here; whether the camera then stops by itself
/// after the rounds (and whether 0 would do the same) is for the live test.
pub const TIMED_ON_OFF: u8 = 1;

/// The siren: `rounds` rounds (the camera stops by itself after them).
pub fn siren_rounds_xml(channel: u8, rounds: u32) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<audioPlayInfo version=\"1.1\">\n<channelId>{channel}</channelId>\n<playMode>0</playMode>\n<playDuration>10</playDuration>\n<playTimes>{rounds}</playTimes>\n<onOff>{TIMED_ON_OFF}</onOff>\n</audioPlayInfo>\n</body>\n"
    )
}

/// The siren off (the manual switch, off).
pub fn siren_off_xml(channel: u8) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<audioPlayInfo version=\"1.1\">\n<channelId>{channel}</channelId>\n<playMode>2</playMode>\n<playDuration>10</playDuration>\n<playTimes>1</playTimes>\n<onOff>0</onOff>\n</audioPlayInfo>\n</body>\n"
    )
}

// The siren's sound (as the official Reolink app sends it, 2026-11-27).

/// The extension of a read of the siren's sound (260, 264, 267).
pub fn read_ext(channel: u8) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<channelId>{channel}</channelId>\n<chnType>0</chnType>\n</Extension>\n")
}

/// The extension of a piece of a sound sent to the camera (its payload is binary).
pub fn binary_ext(channel: u8) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<binaryData>1</binaryData>\n<channelId>{channel}</channelId>\n</Extension>\n")
}

/// The header of a sound sent to the camera (261): its size in bytes.
pub fn sound_header_xml(channel: u8, bytes: usize) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<audioFileInfo version=\"1.1\">\n<channelId>{channel}</channelId>\n<fileSize>{bytes}</fileSize>\n<fileFormat>adpcm</fileFormat>\n</audioFileInfo>\n</body>\n"
    )
}

/// The siren's settings (the `<audioCfg>` element as the camera gave it with
/// 264) with only `audioSelect` changed (1: the custom sound, 0: the built-in
/// one), and `volume` when given; `audioListId` and `visitorLoudspeaker` are
/// added when missing (the app does). Everything else goes back unchanged.
/// None: the camera gave no `audioSelect` to change.
pub fn audio_cfg_xml(cfg: &str, custom: bool, volume: Option<u32>) -> Option<String> {
    let el = element(cfg, "audioCfg")?;
    let mut x = with_text(el, "audioSelect", if custom { "1" } else { "0" })?;
    if let Some(v) = volume {
        x = with_text(&x, "volume", &v.to_string())?;
    }
    let close = x.rfind("</audioCfg>")?;
    let mut add = String::new();
    for name in ["audioListId", "visitorLoudspeaker"] {
        if element(&x, name).is_none() {
            add.push_str(&format!("<{name}>0</{name}>\n"));
        }
    }
    if !add.is_empty() {
        if !x[..close].ends_with('\n') {
            add.insert(0, '\n');
        }
        x.insert_str(close, &add);
    }
    Some(format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n{x}\n</body>\n"))
}

/// The whole `<name ...>...</name>` element in `xml` (the first one); a
/// self-closing `<name/>` is the element alone (it has no text).
pub fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut from = 0;
    loop {
        let i = from + xml[from..].find(&open)?;
        // (`<state` must not match `<stateX`.)
        let after = xml[i + open.len()..].chars().next()?;
        if after == '>' || after == ' ' || after == '/' || after == '\n' || after == '\t' || after == '\r' {
            let tag_end = i + xml[i..].find('>')?;
            if xml[..tag_end].ends_with('/') {
                return Some(&xml[i..=tag_end]);
            }
            let j = tag_end + xml[tag_end..].find(&close)? + close.len();
            return Some(&xml[i..j]);
        }
        from = i + open.len();
    }
}

/// Every whole `<name>` element in `xml`, in order (not nested in each other).
pub fn elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(e) = element(rest, name) {
        out.push(e);
        let at = rest.find(e).unwrap() + e.len();
        rest = &rest[at..];
    }
    out
}

/// The text of the first `<name>` element (trimmed), if any (a self-closing one has none).
pub fn text<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let e = element(xml, name)?;
    if e.ends_with("/>") {
        return None;
    }
    let s = e.find('>')? + 1;
    let t = e.rfind("</")?;
    (s <= t).then(|| e[s..t].trim())
}

/// `xml` with the text of its first `<name>` element replaced (None: no such
/// element, or a self-closing one).
pub fn with_text(xml: &str, name: &str, value: &str) -> Option<String> {
    let e = element(xml, name)?;
    if e.ends_with("/>") {
        return None;
    }
    let at = xml.find(e)?;
    let s = at + e.find('>')? + 1;
    let t = at + e.rfind("</")?;
    Some(format!("{}{}{}", &xml[..s], esc(value), &xml[t..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_are_found_by_their_whole_name() {
        let x = "<body><LedState version=\"1.1\"><channelId>0</channelId><ledVersion>2</ledVersion><state>auto</state><lightState>open</lightState></LedState></body>";
        assert_eq!(text(x, "state"), Some("auto"));
        assert_eq!(text(x, "lightState"), Some("open"));
        assert_eq!(text(x, "missing"), None);
        assert!(element(x, "LedState").unwrap().starts_with("<LedState version"));
        let y = with_text(x, "state", "close").unwrap();
        assert_eq!(text(&y, "state"), Some("close"));
        assert_eq!(text(&y, "lightState"), Some("open"), "the other field is untouched");
        assert_eq!(with_text(x, "lightState", "close").map(|z| text(&z, "state").unwrap().to_string()), Some("auto".into()));
        let items = "<Support><item><chnID>0</chnID><ledCtrl>1</ledCtrl></item><item><chnID>1</chnID></item></Support>";
        let l = elements(items, "item");
        assert_eq!(l.len(), 2);
        assert_eq!(text(l[1], "chnID"), Some("1"));
        assert_eq!(esc("a<b&\"c'"), "a&lt;b&amp;&quot;c&apos;");
    }

    #[test]
    fn self_closing_elements() {
        let x = "<body><LedState version=\"1.1\"><state/><lightState>open</lightState><item chnID=\"1\"/></LedState></body>";
        assert_eq!(element(x, "state"), Some("<state/>"));
        assert_eq!(text(x, "state"), None);
        assert_eq!(with_text(x, "state", "close"), None);
        assert_eq!(text(x, "lightState"), Some("open"), "a self-closing element does not swallow the next one");
        assert_eq!(elements(x, "item"), vec!["<item chnID=\"1\"/>"]);
        assert!(element(x, "LedState").unwrap().ends_with("</LedState>"));
    }

    fn req(msg_id: u32, write: bool, deadline: Instant) -> (Request, std::sync::mpsc::Receiver<Result<Answer, String>>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        (Request { msg_id, channel: 0, ext: String::new(), body: String::new(), write, urgent: false, keep: false, deadline, reply: tx, then: Vec::new(), hub_ok: false }, rx)
    }

    #[test]
    fn late_and_unlisted_requests_are_never_sent_and_fail_alone() {
        let now = Instant::now();
        let mut q = Queue::new("test:1");
        let (late, late_rx) = req(bc::MSG_LED_SET, true, now - Duration::from_millis(1));
        let (bad, bad_rx) = req(23, true, now + Duration::from_secs(5));
        let (sneaky, sneaky_rx) = req(bc::MSG_AUDIO_PLAY, false, now + Duration::from_secs(5));
        let (ok, _ok_rx) = req(bc::MSG_LED_GET, false, now + Duration::from_secs(5));
        for r in [late, bad, sneaky, ok] {
            q.accept(r, false, false);
        }
        let next = q.next_sendable(now).unwrap();
        assert_eq!(next.msg_id, bc::MSG_LED_GET, "the one that may go");
        for (rx, what) in [(late_rx, "waited too long"), (bad_rx, "not one AmberSight sends"), (sneaky_rx, "not one AmberSight sends")] {
            let e = rx.try_recv().unwrap().unwrap_err();
            assert!(e.contains(what) && e.contains(NOT_SENT), "{e}");
        }
        assert!(q.next_sendable(now).is_none());
        // The stop goes first; reads do not keep the session.
        let (a, _) = req(bc::MSG_LED_GET, false, now + Duration::from_secs(5));
        let (mut stop, _) = req(bc::MSG_AUDIO_PLAY, true, now + Duration::from_secs(5));
        stop.urgent = true;
        q.accept(a, false, false);
        q.accept(stop, false, false);
        assert_eq!(q.next_sendable(now).unwrap().msg_id, bc::MSG_AUDIO_PLAY);
        assert!(q.last.is_none(), "no read or unkept request kept the session");
        let _ = q.next_sendable(now);
        // ...but never ahead of a siren play that waits (the stop is for it).
        let (play, _) = req(bc::MSG_AUDIO_PLAY, true, now + Duration::from_secs(5));
        let (b, _) = req(bc::MSG_LED_GET, false, now + Duration::from_secs(5));
        let (mut stop, _) = req(bc::MSG_AUDIO_PLAY, true, now + Duration::from_secs(5));
        stop.urgent = true;
        stop.body = "stop".into();
        q.accept(play, false, false);
        q.accept(b, false, false);
        q.accept(stop, false, false);
        let order: Vec<(u32, String)> = std::iter::from_fn(|| q.next_sendable(now)).map(|r| (r.msg_id, r.body)).collect();
        assert_eq!(order, vec![(bc::MSG_AUDIO_PLAY, String::new()), (bc::MSG_AUDIO_PLAY, "stop".to_string()), (bc::MSG_LED_GET, String::new())]);
        let _ = q.next_sendable(now);
        // ...but never ahead of a siren play that waits (the stop is for it).
        let (play, _) = req(bc::MSG_AUDIO_PLAY, true, now + Duration::from_secs(5));
        let (b, _) = req(bc::MSG_LED_GET, false, now + Duration::from_secs(5));
        let (mut stop, _) = req(bc::MSG_AUDIO_PLAY, true, now + Duration::from_secs(5));
        stop.urgent = true;
        stop.body = "stop".into();
        q.accept(play, false, false);
        q.accept(b, false, false);
        q.accept(stop, false, false);
        let order: Vec<(u32, String)> = std::iter::from_fn(|| q.next_sendable(now)).map(|r| (r.msg_id, r.body)).collect();
        assert_eq!(order, vec![(bc::MSG_AUDIO_PLAY, String::new()), (bc::MSG_AUDIO_PLAY, "stop".to_string()), (bc::MSG_LED_GET, String::new())]);
    }

    #[test]
    fn a_hubs_channel_only_for_the_sound_and_a_job_only_with_listed_messages() {
        let now = Instant::now();
        let mut q = Queue::new("test:2");
        let (mut a, a_rx) = req(bc::MSG_AUDIO_CFG_GET, false, now + Duration::from_secs(5));
        a.channel = 1;
        q.accept(a, false, true);
        assert!(a_rx.try_recv().unwrap().unwrap_err().contains(NOT_FOR_HUB));
        let (mut b, _b_rx) = req(bc::MSG_AUDIO_CFG_GET, false, now + Duration::from_secs(5));
        b.channel = 1;
        b.hub_ok = true;
        q.accept(b, false, true);
        assert_eq!(q.next_sendable(now).unwrap().msg_id, bc::MSG_AUDIO_CFG_GET);
        // hub_ok lets only the siren's sound messages through.
        for (first, more) in [(bc::MSG_LED_SET, None), (bc::MSG_AUDIO_PLAY, None), (bc::MSG_LED_GET, None), (bc::MSG_AUDIO_FILE_SEND, Some(bc::MSG_AUDIO_PLAY))] {
            let (mut x, x_rx) = req(first, first != bc::MSG_LED_GET, now + Duration::from_secs(5));
            x.channel = 1;
            x.hub_ok = true;
            if let Some(m) = more {
                x.then = vec![Step { msg_id: m, ext: String::new(), payload: Vec::new(), raw: false, same_num: false }];
            }
            q.accept(x, false, true);
            assert!(x_rx.try_recv().unwrap().unwrap_err().contains(NOT_FOR_HUB), "{first} {more:?}");
        }
        assert!(q.waiting.is_empty());
        // Never a battery camera's own session, hub_ok or not.
        let (mut c, c_rx) = req(bc::MSG_AUDIO_CFG_GET, false, now + Duration::from_secs(5));
        c.hub_ok = true;
        q.accept(c, true, false);
        assert!(c_rx.try_recv().unwrap().unwrap_err().contains(NOT_FOR_BATTERY));
        // Every message of a job must be on the list.
        let step = |id| Step { msg_id: id, ext: String::new(), payload: Vec::new(), raw: true, same_num: true };
        let (mut j, j_rx) = req(bc::MSG_AUDIO_FILE_SEND, true, now + Duration::from_secs(5));
        j.then = vec![step(bc::MSG_AUDIO_FILE_SEND), step(213)];
        q.accept(j, false, false);
        assert!(q.next_sendable(now).is_none());
        assert!(j_rx.try_recv().unwrap().unwrap_err().contains("not one AmberSight sends"));
    }

    #[test]
    fn a_job_ended_mid_way_is_not_said_to_be_not_sent() {
        let now = Instant::now();
        let mut q = Queue::new("test:3");
        let step = Step { msg_id: bc::MSG_AUDIO_FILE_SEND, ext: String::new(), payload: Vec::new(), raw: true, same_num: true };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let job = Job { channel: 0, write: true, deadline: now, rest: vec![step.clone(), step].into(), step: 3 };
        q.next = Some((job, 1, true, tx));
        q.fail_all("the Baichuan session ended (gone)");
        let e = rx.try_recv().unwrap().unwrap_err();
        assert!(!e.contains(NOT_SENT) && e.contains("stopped after message 4 of 6"), "{e}");
    }

    #[test]
    fn the_sirens_settings_go_back_with_one_field_changed() {
        let doorbell = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<audioCfg version=\"1.1\">\n<channelId>0</channelId>\n<timeout>0</timeout>\n<audioSelect>0</audioSelect>\n<volume>25</volume>\n<preAlarm>0</preAlarm>\n<talkAndReplyVolume>80</talkAndReplyVolume>\n<visitorVolume>60</visitorVolume>\n<audioListId>3</audioListId>\n<visitorLoudspeaker>1</visitorLoudspeaker>\n</audioCfg>\n</body>\n";
        let x = audio_cfg_xml(doorbell, true, None).unwrap();
        assert_eq!(text(&x, "audioSelect"), Some("1"));
        for (k, v) in [("volume", "25"), ("talkAndReplyVolume", "80"), ("visitorVolume", "60"), ("audioListId", "3"), ("visitorLoudspeaker", "1"), ("timeout", "0"), ("preAlarm", "0")] {
            assert_eq!(text(&x, k), Some(v), "{k} kept");
        }
        assert!(x.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<audioCfg version=\"1.1\">\n"), "{x}");
        assert_eq!(x.matches("<audioListId>").count(), 1);
        let cam = "<body><audioCfg><channelId>0</channelId><audioSelect>1</audioSelect><volume>25</volume><unknownThing>7</unknownThing></audioCfg></body>";
        let x = audio_cfg_xml(cam, false, Some(80)).unwrap();
        assert_eq!(text(&x, "audioSelect"), Some("0"));
        assert_eq!(text(&x, "volume"), Some("80"));
        assert_eq!(text(&x, "unknownThing"), Some("7"), "an unknown field goes back");
        assert_eq!((text(&x, "audioListId"), text(&x, "visitorLoudspeaker")), (Some("0"), Some("0")), "added as the app does");
        assert!(audio_cfg_xml("<body><audioCfg><volume>1</volume></audioCfg></body>", true, None).is_none(), "nothing to switch");
        assert!(sound_header_xml(0, 39936).contains("<fileSize>39936</fileSize>\n<fileFormat>adpcm</fileFormat>"));
        assert!(binary_ext(2).contains("<binaryData>1</binaryData>\n<channelId>2</channelId>"));
        assert!(read_ext(0).contains("<chnType>0</chnType>"));
    }

    #[test]
    fn only_a_camera_of_its_own() {
        assert!(!is_multi(Some("wifi_solo_ipc"), Some(1)));
        assert!(!is_multi(Some("ipc"), Some(1)));
        assert!(is_multi(Some("ipc"), None), "no channel count: may be a hub");
        assert!(is_multi(Some("homehub"), Some(1)));
        assert!(is_multi(Some("nvr"), Some(8)));
        assert!(is_multi(None, Some(4)));
    }

    #[test]
    fn the_siren_messages() {
        let on = siren_rounds_xml(0, 3);
        assert!(on.contains("<audioPlayInfo version=\"1.1\">") && on.contains("<playMode>0</playMode>") && on.contains("<playTimes>3</playTimes>") && on.contains("<onOff>1</onOff>"));
        let off = siren_off_xml(0);
        assert!(off.contains("<playMode>2</playMode>") && off.contains("<onOff>0</onOff>"));
        assert!(ability_ext("a<b").contains("<userName>a&lt;b</userName>"));
    }
}
