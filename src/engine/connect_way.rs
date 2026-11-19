//! Which way the built-in reader reaches a camera: RTSP, or Reolink's own
//! Baichuan protocol on TCP port 9000 (`ingest::baichuan::wired`), and when
//! "auto" moves from one to the other.
//!
//! The rules (camera setting `connect = "auto"`):
//!   * Baichuan is the primary; RTSP is the backup (the owner's call after
//!     the lab comparison: Baichuan's own timestamps are smooth, Reolink's
//!     RTSP ones jitter).
//!   * Switch only after the way in use keeps failing: FAILS_TO_SWITCH
//!     sessions in a row that brought no video, or no video for NO_VIDEO_SWITCH.
//!   * A refused user name or password never switches and is never retried:
//!     Reolink locks an account out after a few failed sign-ins, so both ways
//!     stop and the error is shown until the settings change.
//!   * One way at a time (the worker runs one session at a time, and signs
//!     the Baichuan session out before RTSP is tried again).
//!   * On the backup, the primary is tried again every PRIMARY_RETRY, at a
//!     clean point (a recording segment boundary); if that try brings no
//!     video, back to the backup at once.
//!   * A hub channel the hub does not show as wired takes Baichuan out for
//!     good: Auto then stays on RTSP and never tries Baichuan again.
//!   * Battery cameras never use Baichuan here or Auto: they keep their
//!     on-demand path, and nothing here may wake them (see `mode_of`). A hub
//!     channel the hub does not show as wired is refused by the client, and
//!     that refusal is final too.
//!
//! Pure state, no I/O: the worker (native.rs) feeds it session outcomes.

use crate::config::CameraConfig;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    Rtsp,
    Baichuan,
}

impl Way {
    pub fn label(self) -> &'static str {
        match self {
            Way::Rtsp => "RTSP",
            Way::Baichuan => "Baichuan",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Rtsp,
    Baichuan,
    Auto,
}

/// The camera's `connect` setting. Anything set up as a battery camera
/// (on demand, a battery UID, a `baichuan://` battery address) or not on an
/// rtsp:// address stays on RTSP whatever the setting says: Baichuan
/// streaming here is continuous and would keep a battery camera awake.
pub fn mode_of(cfg: &CameraConfig) -> Mode {
    if battery_reason(cfg).is_some() {
        return Mode::Rtsp;
    }
    match cfg.connect.trim().to_ascii_lowercase().as_str() {
        "baichuan" => Mode::Baichuan,
        "auto" => Mode::Auto,
        _ => Mode::Rtsp,
    }
}

/// Why a camera's settings say it is a battery camera (or otherwise not one
/// Baichuan streaming is for), if they do.
pub fn battery_reason(cfg: &CameraConfig) -> Option<&'static str> {
    if crate::ingest::baichuan::is_baichuan_url(&cfg.url) || cfg.battery_uid().is_some() {
        return Some("it is a battery camera (it has a UID)");
    }
    if cfg.on_demand {
        return Some("it is set up as a battery camera (connect only when needed)");
    }
    let l = cfg.url.trim().to_ascii_lowercase();
    if !l.starts_with("rtsp://") && !l.starts_with("rtsps://") {
        return Some("its address is not an rtsp:// address");
    }
    None
}

/// Failed sessions in a row on one way before Auto switches.
pub const FAILS_TO_SWITCH: u32 = 3;
/// No video this long on one way (with at least one failed session) switches.
pub const NO_VIDEO_SWITCH: Duration = Duration::from_secs(120);
/// On the backup, try the primary again this often.
pub const PRIMARY_RETRY: Duration = Duration::from_secs(30 * 60);

/// PRIMARY_RETRY, or (for lab tests of the switch back only) the seconds in
/// AMBERSIGHT_TEST_PRIMARY_RETRY_SECS.
pub fn primary_retry() -> Duration {
    static V: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("AMBERSIGHT_TEST_PRIMARY_RETRY_SECS").ok().and_then(|v| v.parse().ok()).map(Duration::from_secs).unwrap_or(PRIMARY_RETRY)
    })
}

/// How one session ended, as the picker needs it.
#[derive(Debug, Clone, Default)]
pub struct Ended {
    /// Video frames arrived.
    pub got_video: bool,
    /// Why it ended (None: stopped on purpose).
    pub error: Option<String>,
    /// The camera refused the user name or password (on RTSP alone: only a
    /// clear refusal, see `native::is_refused_password`).
    pub auth: bool,
    /// The Baichuan client refused the channel as maybe a battery camera.
    pub battery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Go on with the same way (the usual retry waits apply).
    Same,
    /// Use the other way now.
    Switch { to: Way, why: String },
    /// Stop both ways and show `why` until the settings change.
    Stop { why: String },
}

pub struct WayPicker {
    pub mode: Mode,
    pub primary: Way,
    pub current: Way,
    /// Sessions in a row on `current` that brought no video.
    fails: u32,
    /// When video last came on `current` (or the way was taken up).
    last_video: Instant,
    /// When the backup was taken up (or the primary last tried from it).
    backup_since: Option<Instant>,
    /// This session is a try of the primary from the backup.
    trying_primary: bool,
    /// Baichuan cannot be used (a hub channel that may be battery-powered).
    baichuan_blocked: Option<String>,
    /// What to show next to the way in use: why it is the one in use.
    pub note: Option<String>,
}

impl WayPicker {
    pub fn new(mode: Mode, now: Instant) -> Self {
        let primary = match mode {
            Mode::Baichuan | Mode::Auto => Way::Baichuan,
            Mode::Rtsp => Way::Rtsp,
        };
        WayPicker {
            mode,
            primary,
            current: primary,
            fails: 0,
            last_video: now,
            backup_since: None,
            trying_primary: false,
            baichuan_blocked: None,
            note: None,
        }
    }

    fn other(w: Way) -> Way {
        match w {
            Way::Rtsp => Way::Baichuan,
            Way::Baichuan => Way::Rtsp,
        }
    }

    /// Video is flowing on the current way (called as frames come). True
    /// when that changed the note (a try of the primary worked).
    pub fn video(&mut self, now: Instant) -> bool {
        self.last_video = now;
        self.fails = 0;
        if self.trying_primary {
            self.trying_primary = false;
            self.backup_since = None;
            self.note = Some(format!("back on {}: it works again", self.primary.label()));
            return true;
        }
        false
    }

    /// On the backup long enough: the primary is due another try (the
    /// worker ends the session at a clean point, then calls `try_primary`).
    pub fn primary_due(&self, now: Instant) -> bool {
        self.mode == Mode::Auto
            && self.current != self.primary
            && !self.trying_primary
            && !(self.primary == Way::Baichuan && self.baichuan_blocked.is_some())
            && self.backup_since.is_some_and(|t| now.duration_since(t) >= primary_retry())
    }

    /// The session on the backup ended on purpose: try the primary now.
    pub fn try_primary(&mut self, now: Instant) {
        self.current = self.primary;
        self.trying_primary = true;
        self.fails = 0;
        self.last_video = now;
        self.backup_since = Some(now);
    }

    /// A session ended: what next.
    pub fn session_ended(&mut self, e: &Ended, now: Instant) -> Next {
        // Every mode (RTSP alone too): the worker sets `auth` only for a
        // clear refusal there.
        if e.auth {
            let why = format!(
                "{} sign-in refused: the camera did not accept the user name or password. Not asking again (the camera locks out after a few failed sign-ins); fix them in the camera's settings",
                self.current.label()
            );
            self.note = Some(why.clone());
            return Next::Stop { why };
        }
        if e.battery {
            let why = e.error.clone().unwrap_or_else(|| "maybe a battery camera".into());
            match self.mode {
                Mode::Rtsp => {}
                Mode::Baichuan => {
                    self.note = Some(why.clone());
                    return Next::Stop { why };
                }
                Mode::Auto => {
                    self.baichuan_blocked = Some(why.clone());
                    self.note = Some(format!("Baichuan not used: {why}"));
                    if self.current == Way::Baichuan {
                        self.current = Way::Rtsp;
                        self.trying_primary = false;
                        self.backup_since = None;
                        self.fails = 0;
                        self.last_video = now;
                        return Next::Switch { to: Way::Rtsp, why: format!("Baichuan refused: {why}; using RTSP") };
                    }
                    return Next::Same;
                }
            }
        }
        if self.mode != Mode::Auto {
            return Next::Same;
        }
        if e.got_video {
            self.last_video = now;
            self.fails = 0;
            if self.trying_primary {
                self.trying_primary = false;
                self.backup_since = None;
            }
            return Next::Same;
        }
        if e.error.is_none() {
            return Next::Same; // stopped on purpose
        }
        let reason = e.error.clone().unwrap_or_default();
        let reason = reason.lines().next().unwrap_or_default().to_string();
        if self.trying_primary {
            // The try of the primary failed: back to the backup at once.
            self.trying_primary = false;
            let backup = Self::other(self.primary);
            self.current = backup;
            self.backup_since = Some(now);
            self.fails = 0;
            self.last_video = now;
            let why = format!("{} still failing: {reason}; back on {}", self.primary.label(), backup.label());
            self.note = Some(why.clone());
            return Next::Switch { to: backup, why };
        }
        self.fails += 1;
        let long = now.duration_since(self.last_video) >= NO_VIDEO_SWITCH;
        if self.fails < FAILS_TO_SWITCH && !long {
            return Next::Same;
        }
        let to = Self::other(self.current);
        if to == Way::Baichuan && self.baichuan_blocked.is_some() {
            return Next::Same;
        }
        let what = if self.fails >= FAILS_TO_SWITCH {
            format!("{} failed {} times", self.current.label(), self.fails)
        } else {
            format!("no video by {} for {} min", self.current.label(), now.duration_since(self.last_video).as_secs() / 60)
        };
        let why = format!("{what}: {reason}; using {}", to.label());
        self.current = to;
        self.fails = 0;
        self.last_video = now;
        self.backup_since = (to != self.primary).then_some(now);
        self.note = Some(why.clone());
        Next::Switch { to, why }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(msg: &str) -> Ended {
        Ended { got_video: false, error: Some(msg.into()), auth: false, battery: false }
    }

    #[test]
    fn rtsp_mode_never_switches() {
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Rtsp, t0);
        for i in 0..10 {
            assert_eq!(p.session_ended(&fail("boom"), t0 + Duration::from_secs(i * 60)), Next::Same);
        }
        assert_eq!(p.current, Way::Rtsp);
    }

    #[test]
    fn auto_switches_after_three_failures_and_says_why() {
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Auto, t0);
        assert_eq!(p.current, Way::Baichuan, "Auto starts on Baichuan");
        let e = "Baichuan TCP connect to 192.0.2.5:9000 failed: Connection refused";
        assert_eq!(p.session_ended(&fail(e), t0), Next::Same);
        assert_eq!(p.session_ended(&fail(e), t0 + Duration::from_secs(5)), Next::Same);
        let n = p.session_ended(&fail(e), t0 + Duration::from_secs(10));
        assert_eq!(n, Next::Switch { to: Way::Rtsp, why: format!("Baichuan failed 3 times: {e}; using RTSP") });
        assert_eq!(p.current, Way::Rtsp);
        assert!(p.note.as_deref().unwrap().contains("Baichuan failed 3 times"));
    }

    #[test]
    fn auto_switches_after_two_minutes_without_video() {
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Auto, t0);
        assert_eq!(p.session_ended(&fail("x"), t0 + Duration::from_secs(30)), Next::Same);
        match p.session_ended(&fail("no video from camera for 10 s"), t0 + Duration::from_secs(125)) {
            Next::Switch { to: Way::Rtsp, why } => assert!(why.starts_with("no video by Baichuan for 2 min"), "{why}"),
            n => panic!("{n:?}"),
        }
    }

    #[test]
    fn video_resets_the_count() {
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Auto, t0);
        p.session_ended(&fail("x"), t0);
        p.session_ended(&fail("x"), t0);
        let ok = Ended { got_video: true, error: Some("stream ended".into()), ..Default::default() };
        assert_eq!(p.session_ended(&ok, t0 + Duration::from_secs(1)), Next::Same);
        assert_eq!(p.session_ended(&fail("x"), t0 + Duration::from_secs(2)), Next::Same);
        assert_eq!(p.session_ended(&fail("x"), t0 + Duration::from_secs(3)), Next::Same);
        assert_eq!(p.current, Way::Baichuan);
    }

    #[test]
    fn a_bad_password_never_triggers_fallback_and_stops_both_ways() {
        for mode in [Mode::Auto, Mode::Baichuan] {
            let t0 = Instant::now();
            let mut p = WayPicker::new(mode, t0);
            let bad = Ended { got_video: false, error: Some("401 Unauthorized".into()), auth: true, battery: false };
            match p.session_ended(&bad, t0) {
                Next::Stop { why } => assert!(why.contains("sign-in refused") && why.contains("Not asking again"), "{why}"),
                n => panic!("{mode:?}: {n:?}"),
            }
            assert_eq!(p.current, p.primary, "no switch");
            // Even after earlier failures that would have counted.
            let mut p = WayPicker::new(mode, t0);
            p.session_ended(&fail("x"), t0);
            p.session_ended(&fail("x"), t0);
            assert!(matches!(p.session_ended(&bad, t0), Next::Stop { .. }));
            assert_eq!(p.current, p.primary);
        }
    }

    #[test]
    fn rtsp_alone_stops_on_a_refused_password_too() {
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Rtsp, t0);
        let bad = Ended { got_video: false, error: Some("401 response to DESCRIBE: Received Unauthorized after trying digest auth".into()), auth: true, battery: false };
        match p.session_ended(&bad, t0) {
            Next::Stop { why } => assert!(why.starts_with("RTSP sign-in refused"), "{why}"),
            n => panic!("{n:?}"),
        }
    }

    #[test]
    fn an_rtsp_camera_with_good_credentials_behaves_as_before() {
        // Whatever else goes wrong (and however often), RTSP alone just goes
        // on with its usual waits: nothing stops it, nothing switches.
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Rtsp, t0);
        let ends = [
            "camera did not answer within 15 s (rtsp://192.0.2.5/x)",
            "DESCRIBE failed: Unable to connect to RTSP server: Connection refused (os error 111)",
            "DESCRIBE failed: 503 response to DESCRIBE CSeq=2: Service Unavailable",
            "DESCRIBE failed: 404 response to DESCRIBE CSeq=2: Not Found",
            "no video from camera for 10 s",
            "stream ended",
        ];
        for (i, e) in ends.iter().enumerate() {
            for got_video in [false, true] {
                let ended = Ended { got_video, error: Some(e.to_string()), auth: false, battery: false };
                assert_eq!(p.session_ended(&ended, t0 + Duration::from_secs(i as u64 * 600)), Next::Same, "{e}");
            }
        }
        assert_eq!(p.current, Way::Rtsp);
        assert!(p.note.is_none());
        assert!(!p.primary_due(t0 + Duration::from_secs(99_999)));
    }

    #[test]
    fn primary_is_tried_again_every_30_minutes_and_a_failed_try_goes_straight_back() {
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Auto, t0);
        for _ in 0..3 {
            p.session_ended(&fail("refused"), t0);
        }
        assert_eq!(p.current, Way::Rtsp);
        assert!(!p.primary_due(t0 + Duration::from_secs(29 * 60)));
        let t1 = t0 + Duration::from_secs(30 * 60);
        assert!(p.primary_due(t1));
        // The backup session is ended at a clean point; the primary is tried.
        assert_eq!(p.session_ended(&Ended { got_video: true, error: None, ..Default::default() }, t1), Next::Same);
        p.try_primary(t1);
        assert_eq!(p.current, Way::Baichuan);
        assert!(!p.primary_due(t1));
        // One failure of the try: back at once (not after three).
        match p.session_ended(&fail("still refused"), t1 + Duration::from_secs(5)) {
            Next::Switch { to: Way::Rtsp, why } => assert_eq!(why, "Baichuan still failing: still refused; back on RTSP"),
            n => panic!("{n:?}"),
        }
        // And the next try is 30 minutes after that.
        assert!(!p.primary_due(t1 + Duration::from_secs(20 * 60)));
        assert!(p.primary_due(t1 + Duration::from_secs(31 * 60)));
    }

    #[test]
    fn a_successful_try_of_the_primary_stays_there() {
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Auto, t0);
        for _ in 0..3 {
            p.session_ended(&fail("refused"), t0);
        }
        let t1 = t0 + PRIMARY_RETRY;
        p.try_primary(t1);
        assert!(p.video(t1 + Duration::from_secs(2)));
        assert_eq!(p.note.as_deref(), Some("back on Baichuan: it works again"));
        assert!(!p.video(t1 + Duration::from_secs(3)));
        assert_eq!(p.current, Way::Baichuan);
        assert!(!p.primary_due(t1 + PRIMARY_RETRY * 2), "on the primary: nothing to try");
        // A later single failure does not switch.
        assert_eq!(p.session_ended(&fail("blip"), t1 + Duration::from_secs(60)), Next::Same);
    }

    #[test]
    fn the_backup_failing_too_goes_back_to_the_primary() {
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Auto, t0);
        for _ in 0..3 {
            p.session_ended(&fail("bc down"), t0);
        }
        for _ in 0..2 {
            assert_eq!(p.session_ended(&fail("rtsp down"), t0), Next::Same);
        }
        assert!(matches!(p.session_ended(&fail("rtsp down"), t0), Next::Switch { to: Way::Baichuan, .. }));
    }

    #[test]
    fn a_hub_channel_refused_as_maybe_battery_blocks_the_backup_for_good() {
        let t0 = Instant::now();
        let mut p = WayPicker::new(Mode::Auto, t0);
        assert_eq!(p.current, Way::Baichuan);
        let refused = Ended { got_video: false, error: Some("hub channel 0 may be a battery camera".into()), auth: false, battery: true };
        assert!(matches!(p.session_ended(&refused, t0), Next::Switch { to: Way::Rtsp, .. }));
        for i in 0..10 {
            assert_eq!(p.session_ended(&fail("rtsp down"), t0 + Duration::from_secs(i * 300)), Next::Same, "never back to Baichuan");
        }
        assert!(!p.primary_due(t0 + PRIMARY_RETRY * 4), "Baichuan is never tried again");
        // Baichuan-only mode: it stops.
        let mut p = WayPicker::new(Mode::Baichuan, t0);
        assert!(matches!(p.session_ended(&refused, t0), Next::Stop { .. }));
    }

    #[test]
    fn a_camera_without_the_setting_stays_on_rtsp() {
        let cam: CameraConfig = toml::from_str("name = \"c\"\nurl = \"rtsp://192.0.2.1/Preview_01_main\"\n").unwrap();
        let p = WayPicker::new(mode_of(&cam), Instant::now());
        assert_eq!((p.mode, p.primary, p.current), (Mode::Rtsp, Way::Rtsp, Way::Rtsp));
    }

    #[test]
    fn battery_cameras_never_get_baichuan_or_auto() {
        let cam = |extra: &str, url: &str| -> CameraConfig { toml::from_str(&format!("name = \"c\"\nurl = \"{url}\"\n{extra}")).unwrap() };
        assert_eq!(mode_of(&cam("connect = \"auto\"", "rtsp://192.0.2.1/Preview_01_main")), Mode::Auto);
        assert_eq!(mode_of(&cam("connect = \"baichuan\"", "rtsp://192.0.2.1/Preview_01_main")), Mode::Baichuan);
        assert_eq!(mode_of(&cam("", "rtsp://192.0.2.1/Preview_01_main")), Mode::Rtsp);
        assert_eq!(mode_of(&cam("connect = \"auto\"\non_demand = true", "rtsp://192.0.2.1/Preview_01_main")), Mode::Rtsp);
        assert_eq!(mode_of(&cam("connect = \"baichuan\"\nuid = \"ABC\"", "rtsp://192.0.2.1/Preview_01_main")), Mode::Rtsp);
        assert_eq!(mode_of(&cam("connect = \"baichuan\"", "baichuan://192.0.2.1?uid=ABC")), Mode::Rtsp);
        assert_eq!(mode_of(&cam("connect = \"auto\"", "https://192.0.2.1/flv?port=1935&app=bcs&stream=channel0_sub.bcs")), Mode::Rtsp);
        assert!(battery_reason(&cam("on_demand = true", "rtsp://192.0.2.1/x")).is_some());
    }
}
