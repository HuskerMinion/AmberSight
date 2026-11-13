use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_fps() -> u32 { 10 }
fn default_scale() -> u32 { 640 }
fn default_segment() -> u64 { 5 }
fn default_retention() -> u64 { 14 }
fn default_max_mb() -> u64 { 0 }
fn default_min_free_percent() -> u64 { 10 }
fn default_sensitivity() -> f64 { 0.12 }
fn default_min_frames() -> u32 { 3 }
fn default_algorithm() -> String { "objects".into() }
fn default_min_object() -> f64 { 0.002 }
fn default_max_object() -> f64 { 0.5 }
fn default_contrast() -> u8 { 25 }
fn default_min_travel() -> f64 { 0.03 }
fn default_break_time() -> u64 { 5 }
fn default_pre_roll() -> u64 { 5 }
fn default_post_roll() -> u64 { 10 }
fn default_bind() -> String { "0.0.0.0".into() }
fn default_port() -> u16 { 8090 }
fn default_rtmp_port() -> u16 { 1935 }
fn yes() -> bool { true }
fn no() -> bool { false }
fn default_ingest() -> String { "auto".into() }
fn default_trigger() -> String { "video".into() }
fn default_idle_timeout() -> u64 { 300 }
/// A battery camera's idle timeout as used: at least 30 s, at most a week
/// (so the wake-until arithmetic cannot overflow).
pub fn idle_secs(idle_timeout: u64) -> i64 { idle_timeout.clamp(30, 7 * 86400) as i64 }
fn is_default_idle(v: &u64) -> bool { *v == 300 }
fn is_default_trigger(s: &String) -> bool { s == "video" }
fn is_default_ingest(s: &String) -> bool { s == "auto" }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub username: String,
    pub password: String,
}

/// Multi-user access (classic NVR "users" equivalent). `admin = false` users can view
/// everything but cannot change settings, add/remove cameras, protect events,
/// or alter alerts. The legacy single [server.auth] block is treated as one
/// admin user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    pub username: String,
    pub password: String,
    #[serde(default = "yes")]
    pub admin: bool,
    /// Cameras this (non-admin) user may see, by name; empty = all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cameras: Vec<String>,
    /// Turned off by an admin: this account cannot sign in or do anything,
    /// and only another admin can turn it back on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub users: Vec<UserConfig>,
    /// Port for RTMP senders (OBS and similar) to push streams to cameras with
    /// a push:// URL. Only opened while such a camera exists. 0 = off.
    #[serde(default = "default_rtmp_port")]
    pub rtmp_port: u16,
    /// Serve HTTPS (plain HTTP on the same port redirects to it). With no
    /// cert/key given, a private certificate authority is created in
    /// <data_dir>/tls and signs the server certificate.
    #[serde(default)]
    pub tls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key: Option<PathBuf>,
    /// Extra DNS names or IPs the private-CA certificate must cover (e.g. a
    /// Tailscale MagicDNS name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tls_names: Vec<String>,
    /// This server's name for linked servers and the server switcher
    /// (default: the computer name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: default_bind(),
            port: default_port(),
            auth: None,
            users: Vec::new(),
            rtmp_port: default_rtmp_port(),
            tls: false,
            tls_cert: None,
            tls_key: None,
            tls_names: Vec::new(),
            name: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_segment")]
    pub segment_seconds: u64,
    #[serde(default = "default_retention")]
    pub retention_days: u64,
    /// Size cap in MB for the data folder (recordings kept there plus clips
    /// and thumbnails). 0 (also when left out) = automatic: use what the
    /// drive can spare and keep `min_free_percent` of it free. When exceeded,
    /// the oldest recordings move to the archive folder if one is
    /// configured, otherwise the oldest recordings and unprotected events
    /// are deleted.
    #[serde(default = "default_max_mb")]
    pub max_mb: u64,
    /// Automatic folders (max_mb = 0) keep this share of their drive free
    /// (percent, 0..=90; never less than the 2% / 1 GB every drive keeps).
    #[serde(default = "default_min_free_percent")]
    pub min_free_percent: u64,
    /// Extra recording folders (other drives, NAS mounts...). Cameras choose
    /// one with `folder = "name"`; without it they record into data_dir.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<FolderConfig>,
    /// When a folder is full, footage around events from the last this many
    /// days is deleted only after all quiet footage (None = 21, 0 = off:
    /// strictly oldest first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_keep_days: Option<u32>,
    /// Also record the sub stream of continuously recording cameras (about a
    /// tenth more space), so phones on data saver play recordings small.
    #[serde(default)]
    pub sub_copy: bool,
    /// Back up config.toml and the database into `<data_dir>/backups` every
    /// this many hours; 0 = off.
    #[serde(default = "default_backup_hours")]
    pub backup_hours: u32,
    /// How many backups to keep (oldest are removed).
    #[serde(default = "default_backup_keep")]
    pub backup_keep: u32,
}

fn default_backup_hours() -> u32 { 24 }
fn default_backup_keep() -> u32 { 7 }

impl StorageConfig {
    /// The free-space share automatic folders keep, as a percentage 0..=90.
    pub fn min_free_pct(&self) -> u64 {
        self.min_free_percent.min(90)
    }

    /// Extra recording folders as (name, path).
    pub fn folder_roots(&self) -> Vec<(String, PathBuf)> {
        self.folders.iter().map(|f| (f.name.clone(), f.path.clone())).collect()
    }
}

/// A recording folder with its own size cap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FolderConfig {
    pub name: String,
    pub path: PathBuf,
    /// Size cap in MB; 0 (or left out) = automatic, like the data folder:
    /// what its drive can spare, keeping `min_free_percent` free.
    #[serde(default)]
    pub max_mb: u64,
    /// Recordings pushed out of other folders by their caps move here
    /// instead of being deleted. At most one folder should be the archive.
    #[serde(default)]
    pub archive: bool,
    /// "Keep this folder private": only administrators and AmberSight can
    /// open it (lockdown.rs). Left out = never touched; a folder added after
    /// this was introduced gets a value when it is first saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            data_dir: default_data_dir(),
            segment_seconds: default_segment(),
            retention_days: default_retention(),
            max_mb: default_max_mb(),
            min_free_percent: default_min_free_percent(),
            folders: Vec::new(),
            event_keep_days: None,
            sub_copy: false,
            backup_hours: default_backup_hours(),
            backup_keep: default_backup_keep(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionConfig {
    #[serde(default = "default_sensitivity")]
    pub sensitivity: f64,
    #[serde(default = "default_min_frames")]
    pub min_frames: u32,
    #[serde(default = "default_pre_roll")]
    pub pre_roll: u64,
    #[serde(default = "default_post_roll")]
    pub post_roll: u64,
    /// Global arm window; used by cameras whose own schedule is blank.
    #[serde(default)]
    pub schedule: Option<String>,
    /// "objects" (default): finds moving objects and ignores noise, lighting
    /// changes and things that move in place. "frame-diff": the older
    /// whole-frame brightness change compared against `sensitivity`.
    #[serde(default = "default_algorithm")]
    pub algorithm: String,
    /// Smallest moving object that counts, as a fraction of the frame area
    /// (0.002 = 0.2%, roughly a person 30 m away on a wide camera).
    #[serde(default = "default_min_object")]
    pub min_object: f64,
    /// Changes larger than this fraction of the frame are lighting changes.
    #[serde(default = "default_max_object")]
    pub max_object: f64,
    /// Brightness difference (0-255) a pixel needs to count as changed.
    #[serde(default = "default_contrast")]
    pub contrast: u8,
    /// An event ends only after this many seconds without motion, so brief
    /// pauses (a person stopping, a flickering TV) do not split it.
    #[serde(default = "default_break_time")]
    pub break_time: u64,
    /// How far an object must move (fraction of the frame diagonal) before
    /// it triggers; filters swaying branches and flicker. 0 = off.
    #[serde(default = "default_min_travel")]
    pub min_travel: f64,
}

impl Default for MotionConfig {
    fn default() -> Self {
        MotionConfig {
            sensitivity: default_sensitivity(),
            min_frames: default_min_frames(),
            pre_roll: default_pre_roll(),
            post_roll: default_post_roll(),
            schedule: None,
            algorithm: default_algorithm(),
            min_object: default_min_object(),
            max_object: default_max_object(),
            contrast: default_contrast(),
            min_travel: default_min_travel(),
            break_time: default_break_time(),
        }
    }
}

/// A rectangle in normalized camera coordinates (0.0..1.0). Used for motion
/// A polygon zone: corner points in normalized frame coordinates (0..1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZonePoly {
    pub points: Vec<[f64; 2]>,
}

impl ZonePoly {
    /// Point-in-polygon (even-odd rule).
    pub fn contains(&self, x: f64, y: f64) -> bool {
        let p = &self.points;
        if p.len() < 3 {
            return false;
        }
        let mut inside = false;
        let mut j = p.len() - 1;
        for i in 0..p.len() {
            let (xi, yi, xj, yj) = (p[i][0], p[i][1], p[j][0], p[j][1]);
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            j = i;
        }
        inside
    }
}

/// A named zone for alerts: an "area" (polygon: someone staying inside it
/// too long is loitering) or a "line" (two or more points: someone's feet
/// crossing it in the chosen direction). Needs AI checks to be on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedZone {
    pub name: String,
    /// "area" or "line".
    pub kind: String,
    /// Area: 3 or more corners. Line: 2 or more points, first (A) to last (B).
    /// Normalized frame coordinates (0..1).
    pub points: Vec<[f64; 2]>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Objects that count (AI labels such as "person", "car").
    #[serde(default = "default_zone_labels")]
    pub labels: Vec<String>,
    /// Area: seconds inside before it counts as loitering.
    #[serde(default = "default_zone_seconds")]
    pub seconds: u32,
    /// Line: "ab" (along the arrow, called "in"), "ba" ("out") or "both".
    #[serde(default = "default_zone_direction")]
    pub direction: String,
    /// Send notifications for this zone's events.
    #[serde(default = "yes")]
    pub alert: bool,
    /// Save its events (with clip) in the event list.
    #[serde(default = "yes")]
    pub record: bool,
    /// Vehicles that have sat in the same place for minutes (parked) do not
    /// count (uses the AI's parked memory; also needs "ignore objects that
    /// have not moved" to be on for the camera).
    #[serde(default = "yes")]
    pub ignore_parked: bool,
    /// Only watch inside this window, "HH:MM-HH:MM" (local time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
}

fn default_zone_labels() -> Vec<String> { vec!["person".to_string()] }
fn default_zone_seconds() -> u32 { 30 }
fn default_zone_direction() -> String { "both".to_string() }

impl NamedZone {
    pub fn is_line(&self) -> bool { self.kind == "line" }

    /// Checks done when settings are saved.
    pub fn check(&self) -> Result<(), String> {
        let n = self.name.trim();
        if n.is_empty() || n.chars().count() > 40 || n.chars().any(|c| c.is_control()) {
            return Err("Zone names need 1 to 40 characters".into());
        }
        let (min, what) = match self.kind.as_str() {
            "area" => (3, "An area"),
            "line" => (2, "A line"),
            _ => return Err(format!("Zone '{n}': the kind must be area or line")),
        };
        if self.points.len() < min || self.points.len() > 64 {
            return Err(format!("Zone '{n}': {what} needs at least {min} points (and no more than 64)"));
        }
        if self.points.iter().any(|p| !(0.0..=1.0).contains(&p[0]) || !(0.0..=1.0).contains(&p[1])) {
            return Err(format!("Zone '{n}' reaches outside the picture"));
        }
        if !matches!(self.direction.as_str(), "ab" | "ba" | "both") {
            return Err(format!("Zone '{n}': the direction must be ab, ba or both"));
        }
        if !(1..=3600).contains(&self.seconds) {
            return Err(format!("Zone '{n}': the time must be 1 to 3600 seconds"));
        }
        if self.labels.is_empty() || self.labels.len() > 30 || self.labels.iter().any(|l| l.trim().is_empty() || l.len() > 40) {
            return Err(format!("Zone '{n}': pick at least one object"));
        }
        if self.schedule.as_deref().is_some_and(|t| !t.trim().is_empty() && parse_schedule(t).is_none()) {
            return Err(format!("Zone '{n}': write the window like 07:00-21:00"));
        }
        Ok(())
    }
}

/// Checks for a camera's whole list of named zones.
pub fn check_named_zones(zones: &[NamedZone]) -> Result<(), String> {
    if zones.len() > 24 {
        return Err("No more than 24 named zones per camera".into());
    }
    let mut seen: Vec<String> = Vec::new();
    for z in zones {
        z.check()?;
        let s = slug(&z.name);
        if s.is_empty() || seen.contains(&s) {
            return Err(format!("Zone names must differ ('{}')", z.name.trim()));
        }
        seen.push(s);
    }
    Ok(())
}

/// zones (restrict detection) and ignore zones (suppress detection).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    pub name: String,
    pub url: String,
    /// Optional lower-resolution sub stream (same device credentials). When
    /// set, motion analysis and the live grid use it; recording uses `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_url: Option<String>,
    /// How the stream is received: "auto" (native RTSP for rtsp:// URLs,
    /// falling back to ffmpeg if the camera cannot be read natively),
    /// "native", or "ffmpeg" (the older ffmpeg-process pipeline).
    #[serde(default = "default_ingest", skip_serializing_if = "is_default_ingest")]
    pub ingest: String,
    /// Hardware video decode for motion analysis, e.g. "vaapi",
    /// "vaapi:/dev/dri/renderD130" or "qsv". Unset = software.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hwaccel: Option<String>,
    /// Recording folder name from [[storage.folders]]; unset = data_dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Device credentials for the stream itself (separate from app users).
    /// Injected into the URL as scheme://user:pass@host when username is set
    /// and the URL has no userinfo yet. On PUT, an empty password keeps the
    /// existing password (blank = unchanged).
    /// Text shown on the picture in the live view (and burned into recordings
    /// when `overlay` is on). Fill-ins: {name} {short} {site} {date} {time}.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_text: Option<String>,
    /// Corner for that text: top-left, top-right, bottom-left (default), bottom-right.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_pos: Option<String>,
    /// Burn a name+timestamp overlay into re-encoded segments (classic-style
    /// text overlay). Skipped for direct stream-copy — no re-encode there.
    #[serde(default)]
    pub overlay: bool,
    /// Copy the source's audio track into segments when present (classic NVR audio
    /// channels). Direct mode already carries audio via -c copy.
    #[serde(default)]
    pub audio: bool,
    /// Share this camera's streams with other apps over RTSP (see
    /// restream/); only while [restream] is enabled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub share: bool,
    /// Share even though a privacy mask or text overlay is on: the admin
    /// confirmed that the shared video shows the picture without them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub share_unmasked: bool,
    /// Record triad: record=false -> off; record_motion_only=true -> only
    /// while motion is active; otherwise continuous.
    #[serde(default)]
    pub record_motion_only: bool,
    /// Continuous recording with a sub stream: record the sub stream all the
    /// time and the main stream only around events (its pre-roll is kept in
    /// memory). Playback shows the main stream wherever it was recorded.
    /// Built-in RTSP reader only; ignored without `sub_url`.
    #[serde(default)]
    pub record_sub: bool,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Short name shown in dense UI spots (classic NVR long/short name pair).
    #[serde(default)]
    pub short_name: String,
    /// Older single-group setting; read and folded into `groups`, never written.
    #[serde(default, skip_serializing)]
    pub group: String,
    /// Groups this camera belongs to ("Front", "Outside"...). Empty = Ungrouped.
    /// Used for the live view's group filter and other cameras' also_trigger.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_scale")]
    pub scale: u32,
    #[serde(default = "yes")]
    pub record: bool,
    #[serde(default = "yes")]
    pub motion: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// "Direct" recording (classic-style): segments are stream-copied by a second
    /// ffmpeg (`-c copy`, no re-encode); motion analysis runs on its own
    /// decoded substream. Only meaningful for encoded sources (rtsp/http).
    #[serde(default = "no")]
    pub direct: bool,
    /// Battery cameras: stay disconnected (so the camera can sleep) until
    /// someone views it, wakes it, or the camera/hub reports motion; then
    /// disconnect again after `idle_timeout` seconds with nothing happening.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_demand: bool,
    #[serde(default = "default_idle_timeout", skip_serializing_if = "is_default_idle")]
    pub idle_timeout: u64,
    /// The camera's ONVIF port (often 80, 8000 or 8080) to receive its own
    /// motion / person / vehicle / doorbell events. Unset = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onvif_port: Option<u16>,
    /// Reolink hubs/recorders: the device channel (0-based) of this camera.
    /// Per-channel motion, person/vehicle/animal detection and battery sleep
    /// state are then read from the device's HTTPS API (its ONVIF events do
    /// not say which channel they are for).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reolink_channel: Option<u32>,
    /// A battery Reolink Argus reached directly over the Baichuan protocol (no
    /// hub): the camera's UID, from the Reolink client. Set with a
    /// `baichuan://<host>` URL; the persistent session streams video on demand
    /// and reports motion/battery. Unset = not a Baichuan camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// Only use camera events from this source (video source / channel
    /// token), for devices that report several channels on one connection
    /// (hubs, recorders). Unset = all events from the device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onvif_source: Option<String>,
    /// What starts an event: "video" (AmberSight's motion detection, the
    /// default), "camera" (the camera's own detection, via onvif_port) or
    /// "both". Camera detections always add labels to events.
    #[serde(default = "default_trigger", skip_serializing_if = "is_default_trigger")]
    pub trigger: String,
    /// AI detection for this camera (default: on when [ai] is enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<bool>,
    /// Objects that confirm events on this camera (default: [ai].labels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_labels: Option<Vec<String>>,
    /// Per-camera overrides of [motion].min_object and [motion].contrast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_object: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contrast: Option<u8>,
    /// Optional per-camera motion sensitivity; falls back to [motion].sensitivity.
    #[serde(default)]
    pub sensitivity: Option<f64>,
    /// Privacy masks: rectangles blacked out in recordings, live view and
    /// snapshots. A camera with masks (or the overlay) is re-encoded, which
    /// costs CPU.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub privacy_masks: Vec<ZoneRect>,
    /// Polygon zones, combined with the rectangles: motion counts inside any
    /// zone (rectangle or polygon) and never inside an ignore zone of either kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zone_polys: Vec<ZonePoly>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_polys: Vec<ZonePoly>,
    /// Named zones for loitering and line-crossing alerts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub named_zones: Vec<NamedZone>,
    /// If non-empty, only motion inside these rectangles counts.
    #[serde(default)]
    pub zones: Vec<ZoneRect>,
    /// Rectangles where motion is always ignored (applied after zones).
    #[serde(default)]
    pub ignore_zones: Vec<ZoneRect>,
    /// classic-style schedule: "HH:MM-HH:MM" (local time). Motion events only fire
    /// inside the window; the range wraps past midnight (e.g. "20:00-07:00").
    /// Unset = armed all day.
    #[serde(default)]
    pub schedule: Option<String>,
    // Per-camera overrides of [motion]; unset = the shared default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion_algorithm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_object: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_travel: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_frames: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub break_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_roll: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_roll: Option<u64>,
    /// Notifications for this camera's events (default on). MQTT always reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alerts: Option<bool>,
    /// Which notification services this camera uses (see ALERT_CHANNELS);
    /// unset = every configured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_channels: Option<Vec<String>>,
    /// Overrides [alerts].min_interval for this camera (seconds, 0 = no limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_interval: Option<u32>,
    /// Notify only inside this "HH:MM-HH:MM" window (recording is unaffected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_schedule: Option<String>,
    /// Notify only when the event holds one of these objects ("person", "car",
    /// "vehicle", "animal"...). Unset = every object that confirms events.
    /// Detection, events, search and MQTT are not affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_labels: Option<Vec<String>>,
    /// Areas that never send a notification: an ordinary alert is skipped when
    /// every object that confirmed the event stood inside one of them (passing
    /// traffic on a street-facing camera). Detection, events, search and MQTT
    /// are not affected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alert_ignore_polys: Vec<ZonePoly>,
    /// No-motion alert: notify when nothing has moved for this many minutes
    /// (a pet or a person check). Unset or 0 = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_motion_minutes: Option<u32>,
    /// Watch for no motion only inside this "HH:MM-HH:MM" window; unset = always.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_motion_schedule: Option<String>,
    /// Loud-sound trigger: a sudden loud sound from the camera's microphone
    /// starts an event (like motion, inside the same arm window). Off by
    /// default; the camera must send audio (the Audio setting may stay off).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub audio_trigger: bool,
    /// How much louder than the usual background a sound must be (dB;
    /// unset = 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "ser_opt_f32")]
    pub audio_margin_db: Option<f32>,
    /// And how loud it must be in itself (dBFS, 0 = the loudest the
    /// microphone reports; unset = -45).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "ser_opt_f32")]
    pub audio_min_db: Option<f32>,
    /// Only these sounds start an event ("glass break", "dog bark"... see
    /// crate::sound::labels); empty = any loud sound. Needs [sound] labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_sounds: Vec<String>,
    /// Notification priority for this camera ("low", "normal", "high",
    /// "urgent"); unset = [alerts].priority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_priority: Option<String>,
    /// Overrides [ai].min_score for this camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "ser_opt_f32")]
    pub ai_min_score: Option<f32>,
    /// Overrides [ai].ignore_parked for this camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_ignore_parked: Option<bool>,
    /// Overrides [ai].strict for this camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_strict: Option<bool>,
    /// Overrides [ai].hide_unconfirmed for this camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_hide_unconfirmed: Option<bool>,
    /// The camera's own detection of a person, vehicle or animal confirms an
    /// event only when AI also sees one (or AI could not look). Off unless set
    /// to true: the AI misses people and animals the camera itself sees well
    /// (a camera looking straight down, someone lying on a sofa).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_confirm_camera: Option<bool>,
    /// Package detection after someone has been at the door (doorbells).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<bool>,
    /// Screen capture from a paired helper; the camera's url is push://<key>.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<HelperCapture>,
    /// Other cameras this camera's own motion also triggers: camera names, or
    /// "group:<name>" for every camera in a group. Empty (the default) = none.
    /// Cameras that were only triggered never pass it on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_trigger: Vec<String>,
    /// Camera watchdog settings for this camera (how to restart it, an
    /// optional smart plug / PoE port to cut its power). Unset = defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog: Option<CameraWatchdog>,
    /// Tamper detection (covered, blurred or moved camera). Unset = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tamper: Option<TamperConfig>,
    /// Unusual-activity flagging (out-of-pattern events). Unset = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unusual: Option<UnusualConfig>,
    /// Alert only for strangers (unknown faces / plates). Unset = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strangers: Option<StrangerConfig>,
}

/// "Only alert for unknown people and vehicles" (see engine/strangers.rs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrangerConfig {
    /// Known faces and known plates stay quiet (the event is still recorded
    /// and labelled); unknown ones alert.
    #[serde(default)]
    pub enabled: bool,
    /// A person whose face could not be identified (turned away, too far),
    /// or a vehicle whose plate could not be read, counts as unknown
    /// (default) instead of staying quiet.
    #[serde(default = "default_true")]
    pub unidentified_is_unknown: bool,
    /// Only inside this "HH:MM-HH:MM" window (e.g. night). Unset = always.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// What happens outside the window: "normal" (alerts as without this
    /// filter) or "quiet" (no alerts for people and vehicles).
    #[serde(default = "default_stranger_outside")]
    pub outside: String,
    /// Alert for an unknown person or vehicle only the first time it is seen
    /// in `first_seen_days` days (implies the unknown-only filter).
    #[serde(default)]
    pub first_seen: bool,
    #[serde(default = "default_first_seen_days")]
    pub first_seen_days: u32,
}

fn default_true() -> bool {
    true
}
fn default_stranger_outside() -> String {
    "normal".to_string()
}
fn default_first_seen_days() -> u32 {
    30
}

impl Default for StrangerConfig {
    fn default() -> Self {
        StrangerConfig { enabled: false, unidentified_is_unknown: true, schedule: None, outside: default_stranger_outside(), first_seen: false, first_seen_days: default_first_seen_days() }
    }
}

impl StrangerConfig {
    /// Whether the camera filters its alerts at all.
    pub fn active(&self) -> bool {
        self.enabled || self.first_seen
    }
    /// Unknown faces are kept 60 days, so a longer memory is not possible.
    pub fn first_seen_days(&self) -> u32 {
        self.first_seen_days.clamp(1, 60)
    }
    pub fn quiet_outside(&self) -> bool {
        self.outside == "quiet"
    }
}

/// Morning summary: one message a day about what happened (see digest.rs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DigestConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Local time of day it is sent, "HH:MM".
    #[serde(default = "default_digest_time")]
    pub time: String,
    /// "overnight" (from `night_start` to the sending time), "24h" (the last
    /// 24 hours) or "since_last" (since the last summary, at most 48 hours).
    #[serde(default = "default_digest_period")]
    pub period: String,
    #[serde(default = "default_digest_night")]
    pub night_start: String,
    /// Services it is sent through (ALERT_CHANNELS ids); unset = every one
    /// that is set up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
    /// Cameras it covers (names); empty = all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cameras: Vec<String>,
    /// Send nothing on a day when nothing happened.
    #[serde(default = "default_true")]
    pub skip_if_empty: bool,
}

fn default_digest_time() -> String {
    "07:00".to_string()
}
fn default_digest_period() -> String {
    "overnight".to_string()
}
fn default_digest_night() -> String {
    "22:00".to_string()
}

impl Default for DigestConfig {
    fn default() -> Self {
        DigestConfig { enabled: false, time: default_digest_time(), period: default_digest_period(), night_start: default_digest_night(), channels: None, cameras: Vec::new(), skip_if_empty: true }
    }
}

impl DigestConfig {
    pub fn is_default(&self) -> bool {
        *self == DigestConfig::default()
    }
    /// Minute of the day it is sent.
    pub fn time_min(&self) -> u32 {
        parse_hhmm(&self.time).unwrap_or(7 * 60)
    }
    pub fn night_start_min(&self) -> u32 {
        parse_hhmm(&self.night_start).unwrap_or(22 * 60)
    }
    pub fn period(&self) -> &str {
        match self.period.as_str() {
            "24h" | "since_last" => &self.period,
            _ => "overnight",
        }
    }
    pub fn uses_channel(&self, channel: &str) -> bool {
        self.channels.as_ref().is_none_or(|l| l.iter().any(|c| c == channel))
    }
    /// Whether the summary covers this camera.
    pub fn covers(&self, camera: &str) -> bool {
        self.cameras.is_empty() || self.cameras.iter().any(|c| c == camera)
    }
    pub fn check(&self) -> Result<(), String> {
        if parse_hhmm(&self.time).is_none() {
            return Err("Morning summary: write the time like 07:00".into());
        }
        if parse_hhmm(&self.night_start).is_none() {
            return Err("Morning summary: write the start of the night like 22:00".into());
        }
        if !matches!(self.period.as_str(), "overnight" | "24h" | "since_last") {
            return Err("Morning summary: the period is overnight, 24h or since_last".into());
        }
        Ok(())
    }
}

/// Camera tamper detection (see tamper.rs and engine/tamper.rs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TamperConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 1 (calm) to 5 (touchy); 3 = the default.
    #[serde(default = "default_tamper_sensitivity")]
    pub sensitivity: u8,
    /// How long the view must stay wrong before anyone is told (seconds).
    #[serde(default = "default_tamper_seconds")]
    pub seconds: u32,
}

fn default_tamper_sensitivity() -> u8 {
    3
}
fn default_tamper_seconds() -> u32 {
    60
}

impl Default for TamperConfig {
    fn default() -> Self {
        TamperConfig { enabled: false, sensitivity: 3, seconds: 60 }
    }
}

impl TamperConfig {
    pub fn sensitivity(&self) -> u8 {
        self.sensitivity.clamp(1, 5)
    }
    pub fn seconds(&self) -> i64 {
        i64::from(self.seconds.clamp(10, 3600))
    }
}

/// Unusual-activity flagging (see unusual.rs and engine/unusual.rs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnusualConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 1 (only what never happens then) to 5 (touchy); 3 = the default.
    #[serde(default = "default_tamper_sensitivity")]
    pub sensitivity: u8,
    /// Labels judged: AI labels such as "person", "car", and "motion" for an
    /// event with no label. Default: person.
    #[serde(default = "default_unusual_labels")]
    pub labels: Vec<String>,
    /// "tag" (only mark the event), "notice" (its notification says
    /// "Unusual:" and is low priority) or "alert" (high priority).
    #[serde(default = "default_unusual_mode")]
    pub mode: String,
    /// Days of history before anything is flagged.
    #[serde(default = "default_unusual_days")]
    pub min_days: u32,
}

fn default_unusual_labels() -> Vec<String> {
    vec!["person".to_string()]
}
fn default_unusual_mode() -> String {
    "notice".to_string()
}
fn default_unusual_days() -> u32 {
    crate::unusual::DEFAULT_MIN_DAYS
}

impl Default for UnusualConfig {
    fn default() -> Self {
        UnusualConfig { enabled: false, sensitivity: 3, labels: default_unusual_labels(), mode: default_unusual_mode(), min_days: default_unusual_days() }
    }
}

impl UnusualConfig {
    pub fn sensitivity(&self) -> u8 {
        self.sensitivity.clamp(1, 5)
    }
    pub fn min_days(&self) -> u32 {
        self.min_days.clamp(1, 90)
    }
    pub fn mode(&self) -> &str {
        match self.mode.as_str() {
            "tag" | "alert" => &self.mode,
            _ => "notice",
        }
    }
}

/// Camera watchdog: when a camera stops sending video, restart it, then
/// cut its power if that is set up, then tell the owner (see
/// engine/watchdog.rs). Off until turned on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchdogConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Minutes without video before the watchdog does anything.
    #[serde(default = "default_watchdog_minutes")]
    pub minutes: u32,
    /// At most this many restarts / power cycles per camera in 24 hours.
    #[serde(default = "default_watchdog_max")]
    pub max_per_day: u32,
    /// "HH:MM-HH:MM": never restart anything during these hours (it waits
    /// until they are over; the offline alert still goes out). Unset = none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<String>,
}

fn default_watchdog_minutes() -> u32 { 10 }
fn default_watchdog_max() -> u32 { 3 }

impl Default for WatchdogConfig {
    fn default() -> Self {
        WatchdogConfig { enabled: false, minutes: default_watchdog_minutes(), max_per_day: default_watchdog_max(), quiet_hours: None }
    }
}

/// Battery Reolink cameras (Argus) post their PIR motion as plaintext HTTP to
/// `pushx.reolink.com`. Redirect that host to this server (LAN DNS) and this
/// listener catches every motion with no cloud and ~no extra battery: the push
/// wakes the on-demand camera and records, keyed camera `uid` -> camera.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushxConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Address to listen on for the camera's push (the cameras post to port 80).
    #[serde(default = "default_pushx_listen")]
    pub listen: String,
    /// How long one push keeps the camera awake and recording (seconds); a
    /// further push extends it.
    #[serde(default = "default_pushx_motion_seconds")]
    pub motion_seconds: u32,
}

fn default_pushx_listen() -> String { "0.0.0.0:80".into() }
fn default_pushx_motion_seconds() -> u32 { 20 }

impl Default for PushxConfig {
    fn default() -> Self {
        PushxConfig { enabled: false, listen: default_pushx_listen(), motion_seconds: default_pushx_motion_seconds() }
    }
}

impl WatchdogConfig {
    /// Keep the numbers sensible.
    pub fn sanitize(&mut self) {
        self.minutes = self.minutes.clamp(2, 24 * 60);
        self.max_per_day = self.max_per_day.clamp(1, 24);
        if self.quiet_hours.as_deref().is_some_and(|q| q.trim().is_empty()) {
            self.quiet_hours = None;
        }
    }
}

/// Push notifications to phone apps (UnifiedPush, see push.rs): the push
/// servers ("distributors", usually ntfy) an app's push address may use.
/// Browsers' own push services (Google, Mozilla, Apple, Microsoft) are
/// always allowed and need nothing here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PushConfig {
    /// Host names, optionally host:port ("ntfy.sh", "ntfy.home.lan:8080",
    /// "192.168.1.20:2586"). Exact names only, no wildcards. A host other
    /// than a well-known public one may be on the home network (see
    /// push::app_endpoint_rules).
    #[serde(default = "default_app_hosts")]
    pub app_hosts: Vec<String>,
}

fn default_app_hosts() -> Vec<String> {
    vec!["ntfy.sh".into()]
}

impl Default for PushConfig {
    fn default() -> Self {
        PushConfig { app_hosts: default_app_hosts() }
    }
}

/// Most push servers [push] app_hosts may list.
pub const MAX_APP_HOSTS: usize = 50;

impl PushConfig {
    pub fn is_default(&self) -> bool {
        *self == PushConfig::default()
    }

    /// Normalize the host list: lower case, a pasted address cut down to
    /// host[:port], duplicates dropped. An entry that is not a host name is
    /// an error (wildcards included).
    pub fn sanitize(&mut self) -> Result<(), String> {
        let mut out: Vec<String> = Vec::new();
        for raw in &self.app_hosts {
            let t = raw.trim();
            if t.is_empty() {
                continue;
            }
            let h = crate::push::normalize_host(t).ok_or_else(|| format!("\"{}\" is not a host name (write it like ntfy.example.com or 192.168.1.20:8080)", t.chars().take(80).collect::<String>()))?;
            if !out.contains(&h) {
                out.push(h);
            }
        }
        if out.len() > MAX_APP_HOSTS {
            return Err(format!("at most {MAX_APP_HOSTS} push servers"));
        }
        self.app_hosts = out;
        Ok(())
    }

    /// The list as loaded from the file: entries that are not host names
    /// are left out and only the first MAX_APP_HOSTS kept, so it is always
    /// normalized (and the settings can be saved). Returns what was left
    /// out, to be logged.
    pub fn sanitize_at_load(&mut self) -> Vec<String> {
        let mut said = Vec::new();
        let mut out: Vec<String> = Vec::new();
        for raw in &self.app_hosts {
            let t = raw.trim();
            if t.is_empty() {
                continue;
            }
            match crate::push::normalize_host(t) {
                Some(h) if !out.contains(&h) => out.push(h),
                Some(_) => {}
                None => said.push(format!("\"{}\" is not a host name; that entry is ignored", t.chars().take(80).collect::<String>())),
            }
        }
        if out.len() > MAX_APP_HOSTS {
            said.push(format!("{} push servers are listed but at most {MAX_APP_HOSTS} are allowed; the first {MAX_APP_HOSTS} are kept and the rest ignored", out.len()));
            out.truncate(MAX_APP_HOSTS);
        }
        self.app_hosts = out;
        said
    }
}

/// Presence ("geofencing"): phone apps report whether the phone is at home;
/// when everyone has left the server arms (or switches profile), and when
/// someone comes back it switches back (see engine/presence.rs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresenceConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Profile to switch to when everyone has left ("" = none).
    #[serde(default)]
    pub away_profile: String,
    /// Profile to switch to when someone comes home ("" = back to the
    /// schedule, if presence switched to the away profile).
    #[serde(default)]
    pub home_profile: String,
    /// Arm the shield when everyone has left. Unset: arm when there is no
    /// away profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm_when_away: Option<bool>,
    /// Disarm the shield when someone comes home.
    #[serde(default)]
    pub disarm_when_home: bool,
    /// Minutes everyone must be away before the away action (0-120).
    #[serde(default = "default_away_delay")]
    pub away_delay_minutes: u32,
    /// A phone that has not reported for this many hours is ignored (1-168).
    #[serde(default = "default_stale_hours")]
    pub stale_hours: u32,
    /// Accounts whose phones count ([] = every account). Ignored on a
    /// server without accounts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<String>,
}

fn default_away_delay() -> u32 { 10 }
fn default_stale_hours() -> u32 { 12 }

impl Default for PresenceConfig {
    fn default() -> Self {
        PresenceConfig {
            enabled: false,
            away_profile: String::new(),
            home_profile: String::new(),
            arm_when_away: None,
            disarm_when_home: false,
            away_delay_minutes: default_away_delay(),
            stale_hours: default_stale_hours(),
            accounts: Vec::new(),
        }
    }
}

impl PresenceConfig {
    pub fn is_default(&self) -> bool {
        *self == PresenceConfig::default()
    }

    /// Whether leaving arms the shield.
    pub fn arms_when_away(&self) -> bool {
        self.arm_when_away.unwrap_or(self.away_profile.trim().is_empty())
    }

    /// Keep the numbers in range and the names trimmed.
    pub fn sanitize(&mut self) {
        self.away_delay_minutes = self.away_delay_minutes.min(120);
        self.stale_hours = self.stale_hours.clamp(1, 168);
        self.away_profile = self.away_profile.trim().to_string();
        self.home_profile = self.home_profile.trim().to_string();
        let mut seen = std::collections::HashSet::new();
        self.accounts = std::mem::take(&mut self.accounts)
            .into_iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty() && seen.insert(a.clone()))
            .collect();
    }

    /// Whether phones of account `user` count, given the server's accounts
    /// (none: an open server, where every phone counts).
    pub fn counts(&self, user: &str, server_accounts: &[String]) -> bool {
        server_accounts.is_empty()
            || (server_accounts.iter().any(|a| a == user) && (self.accounts.is_empty() || self.accounts.iter().any(|a| a == user)))
    }

    /// The settings as the rule uses them: a profile that does not exist
    /// is left out (leaving then arms only if it would have with it).
    pub fn for_rule(&self, auto: &AutomationConfig) -> PresenceConfig {
        let mut c = self.clone();
        let exists = |p: &str| p.is_empty() || auto.profiles.iter().any(|x| x.name == p);
        if !exists(&c.away_profile) {
            c.arm_when_away = Some(c.arms_when_away());
            c.away_profile.clear();
        }
        if !exists(&c.home_profile) {
            c.home_profile.clear();
        }
        c
    }

    /// Profiles named here must exist.
    pub fn validate(&self, auto: &AutomationConfig) -> Result<(), String> {
        for p in [&self.away_profile, &self.home_profile] {
            if !p.is_empty() && !auto.profiles.iter().any(|x| &x.name == p) {
                return Err(format!("profile '{p}' (used by presence) does not exist; choose another in Presence first"));
            }
        }
        Ok(())
    }
}

/// One camera's watchdog settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CameraWatchdog {
    /// Overrides [watchdog] enabled for this camera (unset = follow it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// How to restart the camera: "auto" (default: by what the camera is),
    /// "onvif", "reolink", "dahua" (also Amcrest), "hikvision" or "none".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot: Option<String>,
    /// The camera's web port for the restart (unset = the usual one:
    /// 443/80 for Reolink, 80 for Dahua and Hikvision, the ONVIF port).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_port: Option<u16>,
    /// A smart plug or PoE switch port that powers the camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power: Option<PowerSwitch>,
}

/// A switch that can cut a camera's power over the network.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PowerSwitch {
    /// "shelly" (Gen1), "shelly2" (Gen2/Plus/Pro, RPC), "tasmota", or
    /// "urls" (any device: an off address and an on address).
    pub kind: String,
    /// The plug's address (IP or name, optionally :port) for the presets.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// Which relay / outlet (0 = the first).
    #[serde(default)]
    pub channel: u32,
    /// For "urls": the addresses that turn the power off and on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub off_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub on_url: String,
    /// For "urls": "GET" (default) or "POST".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Sign-in for the plug, if it has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    /// Seconds the power stays off (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_seconds: Option<u32>,
}

impl PowerSwitch {
    pub fn off_secs(&self) -> u64 {
        self.off_seconds.unwrap_or(10).clamp(3, 120) as u64
    }
}

/// Notification services a camera can choose from.
pub const ALERT_CHANNELS: &[&str] = &["push", "email", "telegram", "ntfy", "pushover", "webhook", "home_assistant", "image_post", "action"];

impl CameraConfig {
    /// Fold the older single `group` into `groups`; trim, drop blanks and repeats.
    pub fn normalize_groups(&mut self) {
        let legacy = std::mem::take(&mut self.group);
        let mut out: Vec<String> = Vec::new();
        for g in std::iter::once(legacy).chain(self.groups.drain(..)) {
            let g = g.trim().to_string();
            if !g.is_empty() && !out.iter().any(|x| x.eq_ignore_ascii_case(&g)) {
                out.push(g);
            }
        }
        self.groups = out;
    }

    /// Whether this camera's motion also triggers `target` (by name or group).
    pub fn triggers(&self, target: &CameraConfig) -> bool {
        target.name != self.name
            && self.also_trigger.iter().any(|e| match e.strip_prefix("group:") {
                Some(g) => target.groups.iter().any(|x| x.eq_ignore_ascii_case(g.trim())),
                None => e.trim() == target.name,
            })
    }

    /// Whether an ordinary alert for an event with these objects may go out
    /// ("Notify only for" objects). "vehicle" and "animal" stand for the
    /// usual kinds. Sounds, doorbell presses and packages always may.
    pub fn notifies_for(&self, objects: &[String]) -> bool {
        let Some(want) = self.alert_labels.as_ref().filter(|l| !l.is_empty()) else { return true };
        if objects.iter().any(|o| always_notifies(o)) {
            return true;
        }
        let want: Vec<String> = want.iter().map(|w| w.trim().to_lowercase()).collect();
        objects.iter().any(|o| {
            let o = o.trim().to_lowercase();
            want.iter().any(|w| {
                *w == o
                    || (w == "vehicle" && ["car", "truck", "bus", "motorcycle", "bicycle", "van"].contains(&o.as_str()))
                    || (w == "animal" && ["dog", "cat", "bird", "horse", "cow", "sheep", "bear", "deer", "fox", "raccoon", "squirrel", "rabbit"].contains(&o.as_str()))
            })
        })
    }

    /// `notifies_for` for a whole event. With AI off, nothing could have named
    /// an object, so an event without one is not silenced; with AI on, one
    /// without an object is what AI found nothing in.
    pub fn notifies_for_event(&self, objects: &[String], ai_on: bool) -> bool {
        self.notifies_for(objects) || (!ai_on && objects.iter().all(|o| crate::tracker::is_tag_label(o)))
    }

    /// Whether `channel` may notify for this camera.
    pub fn uses_channel(&self, channel: &str) -> bool {
        self.alert_channels.as_ref().is_none_or(|l| l.iter().any(|c| c == channel))
    }
}

/// Labels that are not left out by "Notify only for" objects or by areas that
/// do not notify: a sound (named ones come with "sound"), a doorbell press, a
/// package delivered or picked up.
pub fn always_notifies(label: &str) -> bool {
    matches!(label.trim().to_lowercase().as_str(), "sound" | "visitor" | "package" | "package picked up")
}

impl MotionConfig {
    /// The motion settings for one camera: its overrides over these defaults.
    pub fn for_camera(&self, c: &CameraConfig) -> MotionConfig {
        let mut m = self.clone();
        if let Some(v) = c.sensitivity { m.sensitivity = v; }
        if let Some(v) = c.min_object { m.min_object = v; }
        if let Some(v) = c.contrast { m.contrast = v; }
        if let Some(v) = c.motion_algorithm.as_ref().filter(|a| matches!(a.as_str(), "objects" | "frame-diff")) { m.algorithm = v.clone(); }
        if let Some(v) = c.max_object { m.max_object = v; }
        if let Some(v) = c.min_travel { m.min_travel = v; }
        if let Some(v) = c.min_frames { m.min_frames = v.max(1); }
        if let Some(v) = c.break_time { m.break_time = v; }
        if let Some(v) = c.pre_roll { m.pre_roll = v; }
        if let Some(v) = c.post_roll { m.post_roll = v; }
        if c.schedule.as_ref().is_some_and(|s| !s.trim().is_empty()) { m.schedule = c.schedule.clone(); }
        m
    }
}

/// Parse a "HH:MM-HH:MM" schedule into (start, end) minutes-of-day.
pub fn parse_schedule(text: &str) -> Option<(u32, u32)> {
    let side: Vec<&str> = text.trim().split('-').collect();
    let start = if side.is_empty() { "" } else { side[0] };
    let end = if side.len() < 2 { "" } else { side[1] };
    let start_min = parse_hhmm(start);
    let end_min = parse_hhmm(end);
    match (start_min, end_min) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    }
}

/// A schedule time: a clock time, or sunrise / sunset with an offset in minutes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeSpec {
    Clock(u32),
    Sun { rise: bool, offset: i32 },
}

/// "HH:MM", "sunrise", "sunset", "sunset-30", "sunrise+45" (offset at most 12 h).
pub fn parse_time_spec(text: &str) -> Option<TimeSpec> {
    let t = text.trim().to_ascii_lowercase();
    for (word, rise) in [("sunrise", true), ("sunset", false)] {
        if let Some(rest) = t.strip_prefix(word) {
            let rest: String = rest.chars().filter(|c| !c.is_whitespace()).collect();
            let offset = if rest.is_empty() { 0 } else { rest.parse::<i32>().ok()? };
            return (offset.abs() <= 720).then_some(TimeSpec::Sun { rise, offset });
        }
    }
    parse_hhmm(&t).map(TimeSpec::Clock)
}

/// Minute of the day for a time spec, given today's (sunrise, sunset) minutes.
pub fn resolve_time(spec: TimeSpec, sun: Option<(u32, u32)>) -> Option<u32> {
    match spec {
        TimeSpec::Clock(m) => Some(m),
        TimeSpec::Sun { rise, offset } => {
            let (sr, ss) = sun?;
            let base = if rise { sr } else { ss } as i32;
            Some((base + offset).rem_euclid(1440) as u32)
        }
    }
}

/// Sunrise and sunset for a day at a place, as minutes after local midnight
/// (`utc_offset_min` = the local zone's offset that day). NOAA's approximate
/// solar equations, good to a minute or two. None: the sun does not rise or
/// set that day (polar summer / winter).
pub fn sun_times(lat: f64, lon: f64, day_of_year: u32, utc_offset_min: i32) -> Option<(u32, u32)> {
    use std::f64::consts::PI;
    let g = 2.0 * PI / 365.0 * (f64::from(day_of_year) - 1.0);
    let eqtime = 229.18
        * (0.000075 + 0.001868 * g.cos() - 0.032077 * g.sin() - 0.014615 * (2.0 * g).cos() - 0.040849 * (2.0 * g).sin());
    let decl = 0.006918 - 0.399912 * g.cos() + 0.070257 * g.sin() - 0.006758 * (2.0 * g).cos() + 0.000907 * (2.0 * g).sin()
        - 0.002697 * (3.0 * g).cos()
        + 0.00148 * (3.0 * g).sin();
    let la = lat.to_radians();
    let x = 90.833f64.to_radians().cos() / (la.cos() * decl.cos()) - la.tan() * decl.tan();
    if !(-1.0..=1.0).contains(&x) {
        return None;
    }
    let ha = x.acos().to_degrees();
    let local = |m: f64| ((m + f64::from(utc_offset_min)).round() as i64).rem_euclid(1440) as u32;
    Some((local(720.0 - 4.0 * (lon + ha) - eqtime), local(720.0 - 4.0 * (lon - ha) - eqtime)))
}

pub fn parse_hhmm(text: &str) -> Option<u32> {
    let hm: Vec<&str> = text.trim().split(':').collect();
    let h = hm.get(0).and_then(|s| s.trim().parse::<u32>().ok());
    let m = hm.get(1).and_then(|s| s.trim().parse::<u32>().ok());
    match (h, m) {
        (Some(h), Some(m)) if h < 24 && m < 60 => Some(h * 60 + m),
        _ => None,
    }
}

/// True when motion is currently armed for this camera.
pub fn schedule_active(window: Option<(u32, u32)>, minute_of_day: u32) -> bool {
    match window {
        None => true,
        Some((a, b)) => if a <= b { a <= minute_of_day && minute_of_day <= b } else { minute_of_day >= a || minute_of_day <= b },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AlertConfig {
    /// POSTed a JSON event payload when a motion clip is finalized.
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Telegram bot alert (classic NVR "Alerts" equivalent). Both required to enable.
    #[serde(default)]
    pub telegram_bot_token: Option<String>,
    #[serde(default)]
    pub telegram_chat_id: Option<String>,
    /// Override the Telegram API base for testing/proxies.
    #[serde(default)]
    pub telegram_api_url: Option<String>,
    /// Action set: whitespace-split argv run on every event; {image} and
    /// {clip} macros expand to the thumb/clip paths. Shell is never used.
    #[serde(default)]
    pub action_command: Option<String>,
    /// POST the event thumbnail (binary JPEG) to this URL.
    #[serde(default)]
    pub image_post_url: Option<String>,
    /// Home Assistant service call on event: POST body
    /// {"entity_id": <ha_entity>} to <ha_url> with Bearer <ha_token>.
    #[serde(default)]
    pub ha_url: Option<String>,
    #[serde(default)]
    pub ha_token: Option<String>,
    #[serde(default)]
    pub ha_entity: Option<String>,
    /// ntfy topic URL (ntfy.sh or your own server), e.g. "https://ntfy.home/cameras".
    /// Alerts carry the event picture when the server accepts attachments.
    #[serde(default)]
    pub ntfy_url: Option<String>,
    /// ntfy access token for protected topics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntfy_token: Option<String>,
    /// Pushover application token and user (or group) key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushover_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushover_user: Option<String>,
    /// Only this Pushover device (blank = all of the user's devices).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushover_device: Option<String>,
    /// Pushover sound name (blank = the user's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushover_sound: Option<String>,
    /// Address of this server as phones reach it, for "open" links in
    /// notifications, e.g. "https://192.168.1.5:8090". Unset = no link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_url: Option<String>,
    /// Default notification priority: "low", "normal", "high" or "urgent".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// MQTT broker, e.g. "mqtt://user:pass@host:1883".
    #[serde(default)]
    pub mqtt_url: Option<String>,
    /// Topic prefix (default "ambersight").
    #[serde(default)]
    pub mqtt_prefix: Option<String>,
    /// Publish Home Assistant MQTT discovery messages.
    #[serde(default)]
    pub mqtt_discovery: bool,
    /// Email alerts over SMTP; needs smtp_host, smtp_from and email_to.
    #[serde(default)]
    pub smtp_host: Option<String>,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_user: Option<String>,
    #[serde(default)]
    pub smtp_password: Option<String>,
    #[serde(default)]
    pub smtp_from: Option<String>,
    #[serde(default)]
    pub email_to: Vec<String>,
    /// "starttls" (default), "tls" or "none".
    #[serde(default = "default_smtp_tls")]
    pub smtp_tls: String,
    /// Notifications (everything except MQTT, which always reports every
    /// event) at most once per this many seconds per camera; 0 = no limit.
    #[serde(default)]
    pub min_interval: u32,
    /// And at most once per this many seconds across all cameras, so one
    /// person walking past several cameras sends one alert; 0 = no limit.
    #[serde(default)]
    pub group_interval: u32,
    /// Arm delay: seconds between pressing the shield (to arm) and motion
    /// detection starting, so you can leave first. 0 = at once. Profiles,
    /// schedules and MQTT always arm at once.
    #[serde(default)]
    pub arm_delay: u32,
    /// Tell the services above (and MQTT) about problems with the system
    /// itself: a camera offline, recordings not being written, a drive
    /// nearly full. None = on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_alerts: Option<bool>,
    /// How long a problem must last before it is reported (minutes; None = 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_alert_minutes: Option<u32>,
}

impl StorageConfig {
    /// Event footage newer than this (ms) is kept ahead of quiet footage;
    /// i64::MIN when off.
    pub fn events_kept_from_ms(&self, now_s: i64) -> i64 {
        match self.event_keep_days.unwrap_or(21) {
            // Off: nothing counts as kept (footage from before "the end of
            // time" is ordinary), so the oldest goes first, events or not.
            0 => i64::MAX,
            d => (now_s - d.min(3650) as i64 * 86400) * 1000,
        }
    }
}

#[cfg(test)]
mod event_keep_tests {
    use super::*;

    #[test]
    fn a_link_in_a_notification_is_http_or_https_only() {
        for good in ["https://nvr.example", "http://nvr.example:8090/", "  https://nvr.example/sub/  ", "http://192.0.2.1:8090"] {
            assert!(link_url_ok(good), "{good}");
        }
        for bad in ["javascript:alert(1)", "data:text/html,x", "ftp://nvr.example", "https://good@evil.example", "https://a:b@evil.example", "nvr.example", "not a url", "", "file:///etc/passwd", "http://"] {
            assert!(!link_url_ok(bad), "{bad}");
        }
        let a = |u: &str| AlertConfig { link_url: Some(u.to_string()), ..Default::default() };
        assert_eq!(a(" https://nvr.example/ ").link_base().as_deref(), Some("https://nvr.example"));
        assert_eq!(a("javascript:alert(1)").link_base(), None, "not even from a file edited by hand");
        assert_eq!(a("").link_base(), None);
    }

    #[test]
    fn a_name_listed_twice_becomes_one_account() {
        let mut c: Config = toml::from_str("[[server.users]]\nusername = \"a\"\npassword = \"view\"\nadmin = false\n[[server.users]]\nusername = \"b\"\npassword = \"keep\"\n[[server.users]]\nusername = \" a \"\npassword = \"adm\"\nadmin = true\ndisabled = true\n[[server.users]]\nusername = \"d\"\npassword = \"off\"\nadmin = true\ndisabled = true\n[[server.users]]\nusername = \"d\"\npassword = \"on\"\nadmin = false\n[[server.users]]\nusername = \"e\"\npassword = \"one\"\ndisabled = true\n[[server.users]]\nusername = \"e\"\npassword = \"two\"\ndisabled = true\n").unwrap();
        assert_eq!(c.dedupe_users(), vec!["a", "d", "e"]);
        let u = |n: &str| c.server.users.iter().filter(|u| u.username.trim() == n).collect::<Vec<_>>();
        assert_eq!(c.server.users.len(), 4);
        // The entry sign-in used stays, so nobody is locked out or gets more.
        assert!(u("a").len() == 1 && u("a")[0].password == "view" && !u("a")[0].admin && !u("a")[0].disabled);
        assert!(u("d").len() == 1 && u("d")[0].password == "on" && !u("d")[0].admin && !u("d")[0].disabled);
        assert!(u("e").len() == 1 && u("e")[0].password == "one" && u("e")[0].disabled);
        assert_eq!(u("b")[0].password, "keep");
        assert!(c.dedupe_users().is_empty());
    }

    #[test]
    fn presence_settings_are_cleaned_at_load() {
        let dir = std::env::temp_dir().join(format!("ambersight-cfgtest-{}-presence", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[presence]\nenabled = true\naway_delay_minutes = 900\nstale_hours = 0\naway_profile = \" Gone \"\nhome_profile = \"Home\"\naccounts = [\" ann \", \"\", \"ann\", \"bob\"]\n[automation]\n[[automation.profiles]]\nname = \"Home\"\n",
        )
        .unwrap();
        // A missing profile does not stop the server.
        let c = Config::load(path.to_str().unwrap()).unwrap();
        let p = &c.presence;
        assert_eq!((p.away_delay_minutes, p.stale_hours, p.away_profile.as_str()), (120, 1, "Gone"));
        assert_eq!(p.accounts, ["ann", "bob"]);
        // The rule leaves the missing one out, and does not start arming
        // because of it.
        let r = p.for_rule(&c.automation);
        assert_eq!((r.away_profile.as_str(), r.home_profile.as_str(), r.arms_when_away()), ("", "Home", false));
        assert!(p.validate(&c.automation).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plate_settings_are_kept_sensible_and_off_by_default() {
        let c = LprConfig::default();
        assert!(!c.enabled && c.cameras.is_empty() && c.for_camera("Drive").is_none());
        let mut c = LprConfig {
            enabled: true,
            min_confidence: 0.0,
            min_plate_px: 1,
            max_diff: 9,
            cameras: vec![
                LprCamera { name: "Drive".into(), min_confidence: Some(f32::NAN), min_plate_px: Some(5000), full_frame: true },
                LprCamera { name: "Drive".into(), ..Default::default() },
                LprCamera { name: String::new(), ..Default::default() },
            ],
            ..Default::default()
        };
        c.sanitize();
        assert_eq!((c.min_confidence, c.min_plate_px, c.max_diff, c.cameras.len()), (0.30, 20, 2, 1));
        let cam = c.for_camera("Drive").unwrap();
        assert_eq!((c.confidence_for(cam), c.plate_px_for(cam), cam.full_frame), (0.70, 1000, true));
        assert!(c.for_camera("Garage").is_none());
        c.enabled = false;
        assert!(c.for_camera("Drive").is_none());
        // Older config files without the section still load.
        let t: LprConfig = toml::from_str("").unwrap();
        assert!(!t.enabled && t.confusions);
    }

    #[test]
    fn face_settings_are_kept_sensible() {
        let mut f = FacesConfig { match_score: 0.0, min_face: 1, ..Default::default() };
        f.sanitize();
        assert_eq!((f.match_score, f.min_face), (0.30, 20));
        f.match_score = f32::NAN;
        f.sanitize();
        assert_eq!(f.match_score, 0.40);
    }

    #[test]
    fn keeping_event_footage_off_means_oldest_first() {
        let mut s = StorageConfig::default();
        s.event_keep_days = Some(0);
        let kept_from = s.events_kept_from_ms(1_800_000_000);
        // No footage is kept ahead of the rest, however recent.
        assert!(1_800_000_000_000i64 < kept_from);
        s.event_keep_days = None;
        assert_eq!(s.events_kept_from_ms(1_800_000_000), (1_800_000_000 - 21 * 86400) * 1000);
    }
}

impl AlertConfig {
    /// This server's address for links in notifications: none if it is not
    /// usable (see `link_url_ok`), so a notification never opens anything else.
    pub fn link_base(&self) -> Option<String> {
        self.link_url.as_deref().filter(|u| link_url_ok(u)).map(|u| u.trim().trim_end_matches('/').to_string())
    }

    pub fn system_alerts_on(&self) -> bool {
        self.system_alerts != Some(false)
    }
    pub fn system_alert_secs(&self) -> i64 {
        self.system_alert_minutes.unwrap_or(5).clamp(1, 24 * 60) as i64 * 60
    }
}

/// A link in a notification: an http or https address, with no user name or
/// password in it (`https://your-server@somewhere.else` looks like yours and is not).
pub fn link_url_ok(u: &str) -> bool {
    url::Url::parse(u.trim()).is_ok_and(|x| matches!(x.scheme(), "http" | "https") && x.host_str().is_some_and(|h| !h.is_empty()) && x.username().is_empty() && x.password().is_none())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OdtConfig {
    #[serde(default)]
    pub enabled: bool,
    /// External detector command. `{image}` is replaced with the event
    /// thumbnail path. Stdout must be JSON: {"objects":[{"label":"person","score":0.9}]}
    /// Objects annotate the event (classic NVR ODT equivalent).
    #[serde(default)]
    pub command: Option<String>,
    /// When true (and ODT enabled), alerts fire ONLY for events where the
    /// detector found objects — the #1 community ask: no more tree/wind alerts.
    #[serde(default)]
    pub alert_only_known: bool,
}

/// Smart search: find event pictures by describing them (Settings > Smart features > Smart
/// search). Off until turned on; turning it on downloads the model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SmartSearchConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// Naming loud sounds (Settings > Smart features > Sounds): turning it on downloads the sound
/// model (YAMNet, ~16 MB) once.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SoundConfig {
    #[serde(default)]
    pub labels: bool,
}

/// Off-site backup (Settings > System > Backup; see src/offsite/mod.rs).
/// Every `every_hours` the settings and the database (and, if chosen, the
/// clips and pictures of protected events) are copied to a folder or to
/// S3-compatible storage. With a passphrase everything is encrypted first;
/// S3 always needs one because the settings hold camera passwords.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OffsiteConfig {
    #[serde(default)]
    pub enabled: bool,
    /// "folder" or "s3".
    #[serde(default = "default_offsite_kind")]
    pub kind: String,
    /// Where copies go for kind = "folder": a network share
    /// (\\nas\backup), another drive, any path.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub folder: String,
    /// S3 address, e.g. https://s3.us-west-004.backblazeb2.com (empty = AWS).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_endpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_region: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_bucket: String,
    /// Folder inside the bucket (e.g. "ambersight/home").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_prefix: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_access_key: String,
    /// Never sent to the browser.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_secret_key: String,
    /// Bucket in the path (https://host/bucket/key) instead of the host
    /// name (https://bucket.host/key); MinIO usually needs this.
    #[serde(default)]
    pub s3_path_style: bool,
    /// Hours between copies.
    #[serde(default = "default_offsite_hours")]
    pub every_hours: u32,
    /// How many copies of the settings and database to keep.
    #[serde(default = "default_offsite_keep")]
    pub keep: u32,
    /// Also copy the video clips of protected events.
    #[serde(default = "yes")]
    pub clips: bool,
    /// Also copy the pictures (thumbnail and AI frames) of protected events.
    #[serde(default = "yes")]
    pub pictures: bool,
    /// Encryption passphrase (never sent to the browser). Without it a
    /// restore is impossible, so the owner must keep it somewhere safe.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub passphrase: String,
}

fn default_offsite_kind() -> String { "folder".into() }
fn default_offsite_hours() -> u32 { 24 }
fn default_offsite_keep() -> u32 { 14 }

impl Default for OffsiteConfig {
    fn default() -> Self {
        OffsiteConfig {
            enabled: false,
            kind: default_offsite_kind(),
            folder: String::new(),
            s3_endpoint: String::new(),
            s3_region: String::new(),
            s3_bucket: String::new(),
            s3_prefix: String::new(),
            s3_access_key: String::new(),
            s3_secret_key: String::new(),
            s3_path_style: false,
            every_hours: default_offsite_hours(),
            keep: default_offsite_keep(),
            clips: true,
            pictures: true,
            passphrase: String::new(),
        }
    }
}

/// Face recognition on person events (Settings > Smart features > Faces).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FacesConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Cameras to look for faces on; empty = every camera with AI.
    #[serde(default)]
    pub cameras: Vec<String>,
    /// Faces narrower than this (pixels in the full picture) are too small
    /// to recognize and are skipped.
    #[serde(default = "default_min_face")]
    pub min_face: u32,
    /// How alike (0-1) a face must be to a named one to count as that person.
    #[serde(default = "default_face_match")]
    #[serde(serialize_with = "ser_f32")]
    pub match_score: f32,
}

fn default_min_face() -> u32 { 40 }
fn default_face_match() -> f32 { 0.40 }

impl Default for FacesConfig {
    fn default() -> Self {
        FacesConfig { enabled: false, cameras: Vec::new(), min_face: default_min_face(), match_score: default_face_match() }
    }
}

impl FacesConfig {
    /// Keep the numbers sensible whatever was sent: a match score of 0 would
    /// make every face anyone at all (and a quiet person silence every alert).
    pub fn sanitize(&mut self) {
        self.match_score = if self.match_score.is_finite() { self.match_score.clamp(0.30, 0.90) } else { default_face_match() };
        self.min_face = self.min_face.clamp(20, 1000);
    }

    pub fn wants(&self, cam: &str) -> bool {
        self.enabled && (self.cameras.is_empty() || self.cameras.iter().any(|c| c == cam))
    }
}

/// License plate recognition on vehicle events (Settings > License plates).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LprConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The cameras plates are read on; none = off everywhere.
    #[serde(default)]
    pub cameras: Vec<LprCamera>,
    /// A plate read this surely (0-1) or more counts.
    #[serde(default = "default_lpr_confidence")]
    #[serde(serialize_with = "ser_f32")]
    pub min_confidence: f32,
    /// Plates narrower than this (pixels in the full picture) are too small
    /// to read well and are skipped.
    #[serde(default = "default_lpr_plate_px")]
    pub min_plate_px: u32,
    /// A reading counts as a known plate when at most this many characters
    /// differ (0 = exactly).
    #[serde(default = "default_lpr_max_diff")]
    pub max_diff: u32,
    /// Letters and numbers that look alike (O/0, I/1, B/8 ...) count as the same.
    #[serde(default = "default_lpr_confusions")]
    pub confusions: bool,
    /// Plate readings (known plates' too) are kept this many days.
    #[serde(default = "default_lpr_keep_days")]
    pub keep_days: u32,
}

fn default_lpr_keep_days() -> u32 {
    180
}

/// One camera plates are read on, with its own settings where they differ.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LprCamera {
    pub name: String,
    #[serde(default)]
    #[serde(serialize_with = "ser_opt_f32")]
    pub min_confidence: Option<f32>,
    #[serde(default)]
    pub min_plate_px: Option<u32>,
    /// Also look for plates in the whole picture when AI saw no vehicle (a
    /// camera aimed at a gate or a narrow drive).
    #[serde(default)]
    pub full_frame: bool,
}

fn default_lpr_confidence() -> f32 { 0.70 }
fn default_lpr_plate_px() -> u32 { 50 }
fn default_lpr_max_diff() -> u32 { 1 }
fn default_lpr_confusions() -> bool { true }

impl Default for LprConfig {
    fn default() -> Self {
        LprConfig { enabled: false, cameras: Vec::new(), min_confidence: default_lpr_confidence(), min_plate_px: default_lpr_plate_px(), max_diff: default_lpr_max_diff(), confusions: true, keep_days: default_lpr_keep_days() }
    }
}

impl LprConfig {
    /// Keep the numbers sensible whatever was sent: a confidence of 0 would
    /// read noise as plates, and a loose match would call every car known.
    pub fn sanitize(&mut self) {
        let conf = |c: f32, d: f32| if c.is_finite() { c.clamp(0.30, 0.99) } else { d };
        self.min_confidence = conf(self.min_confidence, default_lpr_confidence());
        self.min_plate_px = self.min_plate_px.clamp(20, 1000);
        self.keep_days = self.keep_days.clamp(1, 3650);
        self.max_diff = self.max_diff.min(2);
        let mut seen: Vec<String> = Vec::new();
        self.cameras.retain(|c| !c.name.is_empty() && !seen.contains(&c.name) && { seen.push(c.name.clone()); true });
        for c in self.cameras.iter_mut() {
            c.min_confidence = c.min_confidence.map(|v| conf(v, default_lpr_confidence()));
            c.min_plate_px = c.min_plate_px.map(|v| v.clamp(20, 1000));
        }
    }

    /// This camera's plate settings, if plates are read on it.
    pub fn for_camera(&self, cam: &str) -> Option<&LprCamera> {
        if !self.enabled {
            return None;
        }
        self.cameras.iter().find(|c| c.name == cam)
    }

    pub fn confidence_for(&self, c: &LprCamera) -> f32 {
        c.min_confidence.unwrap_or(self.min_confidence)
    }

    pub fn plate_px_for(&self, c: &LprCamera) -> u32 {
        c.min_plate_px.unwrap_or(self.min_plate_px)
    }
}

/// Nightly time-lapse: each day of a camera's recordings squeezed into a
/// short video.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelapseConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Cameras to make them for; empty = every camera that records.
    #[serde(default)]
    pub cameras: Vec<String>,
    /// Length of one day's time-lapse, seconds.
    #[serde(default = "default_timelapse_seconds")]
    pub seconds: u32,
    /// Hour of the night (local time) to make yesterday's.
    #[serde(default = "default_timelapse_hour")]
    pub hour: u32,
    /// Time-lapses are deleted after this many days (0 = keep).
    #[serde(default = "default_timelapse_keep")]
    pub keep_days: u32,
}

fn default_timelapse_seconds() -> u32 { 60 }
fn default_timelapse_hour() -> u32 { 2 }
fn default_timelapse_keep() -> u32 { 30 }

impl Default for TimelapseConfig {
    fn default() -> Self {
        TimelapseConfig {
            enabled: false,
            cameras: Vec::new(),
            seconds: default_timelapse_seconds(),
            hour: default_timelapse_hour(),
            keep_days: default_timelapse_keep(),
        }
    }
}

impl TimelapseConfig {
    /// Whether this camera gets one.
    pub fn wants(&self, cam: &CameraConfig) -> bool {
        cam.record && (self.cameras.is_empty() || self.cameras.iter().any(|c| c == &cam.name))
    }
}

/// Profiles and the weekly schedule that switches between them.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AutomationConfig {
    /// Profile used when no schedule rule matches (and no manual override).
    /// Empty = no profile: every camera uses its own settings.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_profile: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProfileConfig>,
    /// Weekly rules; the first one that matches the current day and time wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule: Vec<ScheduleRule>,
    /// Where the cameras are, for "sunrise" / "sunset" schedule times
    /// (decimal degrees; north and east positive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
}

impl AutomationConfig {
    /// The location, when both coordinates are set and sensible.
    pub fn location(&self) -> Option<(f64, f64)> {
        match (self.latitude, self.longitude) {
            (Some(la), Some(lo)) if la.abs() <= 90.0 && lo.abs() <= 180.0 => Some((la, lo)),
            _ => None,
        }
    }

    /// Check the profiles and schedule for mistakes a person can make in the
    /// editor: blank or duplicate names, rules naming a profile that does not
    /// exist, bad times or day names, overrides for an unnamed camera.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen: Vec<String> = Vec::new();
        for p in &self.profiles {
            let n = p.name.trim();
            if n.is_empty() {
                return Err("a profile has no name".into());
            }
            if seen.iter().any(|x| x.eq_ignore_ascii_case(n)) {
                return Err(format!("profile '{n}' appears twice"));
            }
            if p.cameras.keys().any(|k| k.trim().is_empty()) {
                return Err(format!("profile '{n}' has an override for an unnamed camera"));
            }
            seen.push(n.to_string());
        }
        let known = |name: &str| self.profiles.iter().any(|p| p.name == name);
        if !self.default_profile.is_empty() && !known(&self.default_profile) {
            return Err(format!("default profile '{}' does not exist", self.default_profile));
        }
        const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
        for (i, r) in self.schedule.iter().enumerate() {
            let n = i + 1;
            if !known(&r.profile) {
                return Err(format!("schedule rule {n} uses unknown profile '{}'", r.profile));
            }
            for (what, v) in [("start", &r.from), ("end", &r.to)] {
                match parse_time_spec(v) {
                    None => return Err(format!("schedule rule {n}: bad {what} time '{v}' (use HH:MM, sunrise or sunset, e.g. sunset-30)")),
                    Some(TimeSpec::Sun { .. }) if self.location().is_none() => {
                        return Err(format!("schedule rule {n} uses {v}: set the latitude and longitude first"))
                    }
                    _ => {}
                }
            }
            for d in &r.days {
                let d = d.trim().to_ascii_lowercase();
                if !DAYS.iter().any(|x| d.starts_with(x)) {
                    return Err(format!("schedule rule {n}: unknown day '{d}'"));
                }
            }
        }
        Ok(())
    }
}

/// A named set of overrides, e.g. "Home", "Away", "Night".
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProfileConfig {
    pub name: String,
    /// Arm (true) or disarm (false) all motion triggers when this profile
    /// becomes active. Unset = leave the shield as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed: Option<bool>,
    /// Send alerts at all while this profile is active (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alerts: Option<bool>,
    /// Per-camera overrides, keyed by camera name.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub cameras: std::collections::BTreeMap<String, CameraOverride>,
}

/// Settings a profile can change for one camera; unset fields keep the
/// camera's own setting.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct CameraOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alerts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_motion_only: Option<bool>,
}

/// "On these days, between these times, use this profile." A window that
/// ends before it starts runs past midnight (days are the start days).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScheduleRule {
    /// "mon".."sun"; empty = every day.
    #[serde(default)]
    pub days: Vec<String>,
    pub from: String,
    pub to: String,
    pub profile: String,
}

/// AI object detection: confirms motion events by checking what moved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Built-in detector: path to a YOLOX .onnx model (e.g. yolox_s.onnx or
    /// yolox_tiny.onnx from the YOLOX releases, Apache-2.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<PathBuf>,
    /// Or an AI server with the common `/v1/vision/detection` HTTP API,
    /// e.g. "http://192.168.1.20:32168". Used when no model is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Minimum confidence (0-1) for a detection to count.
    #[serde(default = "default_ai_min_score")]
    #[serde(serialize_with = "ser_f32")]
    pub min_score: f32,
    /// Objects that confirm an event. Events where none of these are found
    /// are still recorded, but send no alert.
    #[serde(default = "default_ai_labels")]
    pub labels: Vec<String>,
    /// Frames checked per event at most (about two per second).
    #[serde(default = "default_ai_max_checks")]
    pub max_checks: u32,
    /// Objects that have not moved for a few minutes (a parked car, someone
    /// sitting at a desk) do not confirm events.
    #[serde(default = "yes")]
    pub ignore_parked: bool,
    /// Stricter confirmation: a wanted object must be seen in 2 of 3
    /// consecutive checks, in about the same place, and have moved between
    /// them (a one-frame mistake, or a statue taken for a person, does not
    /// confirm).
    #[serde(default)]
    pub strict: bool,
    /// Leave motion events that AI checked without finding any of the
    /// objects that count out of the event list and the timeline (a flag or
    /// a tree in the wind). They are still saved; recording is unaffected.
    #[serde(default)]
    pub hide_unconfirmed: bool,
    /// Named zones: milliseconds between AI looks while objects are tracked
    /// (a person walking needs a look every second or so to be followed).
    #[serde(default = "default_zone_interval")]
    pub zone_interval_ms: u32,
    /// Named zones: how long to keep watching after the motion has stopped
    /// while someone is still inside an area (a person standing still).
    #[serde(default = "default_zone_hold")]
    pub zone_hold_seconds: u32,
    /// CPU threads for the built-in detector.
    #[serde(default = "default_ai_threads")]
    pub threads: usize,
    /// Run the built-in detector on the graphics card where possible
    /// (Windows); false: always the CPU.
    #[serde(default = "yes")]
    pub gpu: bool,
    /// Which graphics card (its name as Windows lists it); none: the one
    /// with the most video memory.
    #[serde(default)]
    pub gpu_name: Option<String>,
}

fn default_ai_min_score() -> f32 { 0.5 }
fn default_ai_labels() -> Vec<String> {
    ["person", "bicycle", "car", "motorcycle", "bus", "truck", "dog", "cat", "vehicle", "animal", "visitor"].iter().map(|s| s.to_string()).collect()
}
fn default_ai_max_checks() -> u32 { 10 }
fn default_ai_threads() -> usize { 2 }
fn default_zone_interval() -> u32 { 700 }
fn default_zone_hold() -> u32 { 600 }

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            enabled: false,
            model: None,
            url: None,
            min_score: default_ai_min_score(),
            labels: default_ai_labels(),
            max_checks: default_ai_max_checks(),
            ignore_parked: true,
            strict: false,
            hide_unconfirmed: false,
            zone_interval_ms: default_zone_interval(),
            zone_hold_seconds: default_zone_hold(),
            threads: default_ai_threads(),
            gpu: true,
            gpu_name: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub motion: MotionConfig,
    #[serde(default)]
    pub alerts: AlertConfig,
    #[serde(default)]
    pub odt: OdtConfig,
    #[serde(default)]
    pub ai: AiConfig,
    #[serde(default)]
    pub automation: AutomationConfig,
    #[serde(default)]
    pub timelapse: TimelapseConfig,
    #[serde(default)]
    pub faces: FacesConfig,
    #[serde(default)]
    pub lpr: LprConfig,
    /// The daily morning summary.
    #[serde(default, skip_serializing_if = "DigestConfig::is_default")]
    pub digest: DigestConfig,
    #[serde(default)]
    pub smart_search: SmartSearchConfig,
    /// Alert rules in plain words (Settings > Alerts > Alert rules); they
    /// work while smart search is on. Edited through /api/alert-rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alert_rules: Vec<crate::alert_rules::AlertRule>,
    #[serde(default)]
    pub sound: SoundConfig,
    /// Copies of the settings, database and protected clips somewhere else
    /// (a network share / USB drive, or S3-style cloud storage).
    #[serde(default)]
    pub offsite: OffsiteConfig,
    #[serde(default)]
    pub watchdog: WatchdogConfig,
    /// Catch battery-camera PIR motion over the cameras' push (see PushxConfig).
    #[serde(default)]
    pub pushx: PushxConfig,
    /// Arming / profile switching by phones' presence (geofencing).
    #[serde(default, skip_serializing_if = "PresenceConfig::is_default")]
    pub presence: PresenceConfig,
    /// Push servers phone apps may use (UnifiedPush).
    #[serde(default, skip_serializing_if = "PushConfig::is_default")]
    pub push: PushConfig,
    /// Sharing camera streams with other apps over RTSP (see restream/).
    #[serde(default, skip_serializing_if = "crate::restream::RestreamConfig::is_default")]
    pub restream: crate::restream::RestreamConfig,
    /// Looking for new versions on GitHub (see update.rs).
    #[serde(default, skip_serializing_if = "crate::update::UpdateConfig::is_default")]
    pub update: crate::update::UpdateConfig,
    /// Find cameras: other subnets to scan, and the hourly new-camera check.
    #[serde(default, skip_serializing_if = "DiscoveryConfig::is_default")]
    pub discovery: DiscoveryConfig,
    /// Logins Discovery tries on cameras it finds (kept like camera logins).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovery_logins: Vec<LoginPair>,
    /// Paired capture helpers (other computers that send their screen).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub helpers: Vec<HelperConfig>,
    /// Other AmberSight servers linked with this one (server switcher).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkConfig>,
    #[serde(default)]
    pub cameras: Vec<CameraConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LinkConfig {
    pub id: String,
    /// The other server's name (as it calls itself; editable here).
    pub name: String,
    /// Its address, https://host:port.
    pub url: String,
    /// Its certificate authority (connections are pinned to it).
    pub ca_pem: String,
    /// The key this server presents to it.
    pub token: String,
    /// SHA-256 (hex) of the key it presents to this server.
    pub peer_token_sha256: String,
    /// A small number that stays with this link (remote camera ids are built
    /// from it, so they do not shift when another link is removed).
    #[serde(default)]
    pub num: u32,
    /// Let that server show this server's cameras (live view only; off by default).
    #[serde(default)]
    pub share: bool,
    /// Show that server's cameras here, if it shares them (off by default).
    #[serde(default)]
    pub show: bool,
    /// Who may be signed in here from that server ("open" from its menu):
    /// "viewers" (the same account, never as admin; the default), "all" (admins
    /// too: an admin there can then be any admin here) or "off". A linked
    /// server is trusted to say who its user is.
    #[serde(default = "default_link_signin", skip_serializing_if = "is_link_signin_default")]
    pub signin: String,
    /// Local groups for its cameras, by camera name.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub remote_groups: std::collections::BTreeMap<String, Vec<String>>,
    /// Unix seconds.
    #[serde(default)]
    pub added: i64,
}

fn default_link_signin() -> String {
    "viewers".into()
}
fn is_link_signin_default(s: &String) -> bool {
    s == "viewers" || s.is_empty()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelperConfig {
    pub id: String,
    /// Computer name, editable.
    pub name: String,
    /// SHA-256 (hex) of the helper's secret token.
    pub token_sha256: String,
    #[serde(default)]
    pub os: String,
    /// Unix seconds.
    #[serde(default)]
    pub added: i64,
}

/// What a capture helper sends for one camera (see docs/helper-protocol.md).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelperCapture {
    /// HelperConfig id.
    pub helper: String,
    /// {"kind": "desktop" | "monitor" | "window" | "region", ...}
    pub source: serde_json::Value,
    #[serde(default = "default_capture_fps")]
    pub fps: u32,
    #[serde(default = "default_capture_kbps")]
    pub max_kbps: u32,
    #[serde(default = "default_capture_width")]
    pub max_width: u32,
}
fn default_capture_fps() -> u32 { 10 }
fn default_capture_kbps() -> u32 { 2500 }
fn default_capture_width() -> u32 { 1920 }

/// `[discovery]`: where Find cameras looks besides this server's own subnet,
/// and whether it looks by itself every hour.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DiscoveryConfig {
    /// Other subnets in CIDR form ("10.0.20.0/24"), each /22 or smaller.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<String>,
    /// Check this subnet and `ranges` every hour; report cameras not yet added.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub watch: bool,
    /// Addresses the owner said are not cameras to add (never reported).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<String>,
}

impl DiscoveryConfig {
    pub fn is_default(&self) -> bool { *self == Self::default() }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LoginPair {
    pub username: String,
    #[serde(default)]
    pub password: String,
}

fn default_data_dir() -> PathBuf { PathBuf::from("amberdata") }

impl Config {
    pub fn load(path: &str) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| match crate::lockdown::access_hint(std::path::Path::new(path)) {
                Some(h) => format!("could not read config file: {path} ({h})"),
                None => format!("could not read config file: {path}"),
            })?;
        let mut cfg: Config = match toml::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                // Cut short by a crash while saving? The copy written just
                // before it is complete.
                let last = format!("{path}.last");
                let last_text = std::fs::read_to_string(&last).unwrap_or_default();
                let looks_cut = text.trim().is_empty() || last_text.starts_with(text.as_str());
                match toml::from_str::<Config>(&last_text).ok() {
                    Some(c) if looks_cut => {
                        eprintln!("config file {path} was damaged (cut short); using the copy saved with it ({last})");
                        let _ = std::fs::copy(&last, path);
                        c
                    }
                    _ => return Err(anyhow::Error::new(e).context(format!("invalid config file: {path}"))),
                }
            }
        };
        for c in cfg.cameras.iter_mut() {
            c.normalize_groups();
        }
        // A name listed twice by hand becomes one account: the one that could sign
        // in. Nothing can then bring the other back, with whatever rights it has.
        for name in cfg.dedupe_users() {
            tracing::warn!("server.users: {name} is listed more than once; only the first entry that can sign in is used");
        }
        cfg.presence.sanitize();
        // A push server entry that is not a host name, or one past the
        // most allowed, is left out (and said).
        for w in cfg.push.sanitize_at_load() {
            tracing::warn!("push: app_hosts: {w}");
        }
        // A missing presence profile does not stop the server: the rule
        // leaves it out until it exists (or another is chosen).
        if let Err(e) = cfg.presence.validate(&cfg.automation) {
            tracing::warn!("presence: {e}; it is ignored until then");
        }
        Ok(cfg)
    }

    /// Keep one entry per account name: the first that can sign in (the one sign-in
    /// has always used), else the first. Returns the names that had more.
    pub fn dedupe_users(&mut self) -> Vec<String> {
        let mut kept: Vec<UserConfig> = Vec::with_capacity(self.server.users.len());
        let mut dropped = Vec::new();
        for u in std::mem::take(&mut self.server.users) {
            match kept.iter_mut().find(|k| k.username.trim() == u.username.trim()) {
                None => kept.push(u),
                Some(k) => {
                    dropped.push(k.username.trim().to_string());
                    if k.disabled && !u.disabled {
                        *k = u;
                    }
                }
            }
        }
        self.server.users = kept;
        dropped.dedup();
        dropped
    }

    /// Motion settings for the camera named `camera` (its overrides over [motion]).
    pub fn motion_for(&self, camera: &str) -> MotionConfig {
        match self.cameras.iter().find(|c| c.name == camera || slug(&c.name) == slug(camera)) {
            Some(c) => self.motion.for_camera(c),
            None => self.motion.clone(),
        }
    }

    /// The settings as `save` writes them (also what the off-site backup
    /// copies, so it never reads a file half-rewritten).
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("could not serialize config")
    }

    /// Save, so that a crash or power cut mid-write never loses the
    /// settings: the new text goes to `<path>.last` first (private, synced),
    /// then over `path` in place (keeping its permissions). `load` falls
    /// back to `.last` when `path` is damaged.
    pub fn save(&self, path: &str) -> Result<()> {
        use std::io::Write;
        let text = self.to_toml()?;
        let last = format!("{path}.last");
        let write_synced = |p: &str| -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(p)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()
        };
        // Both files hold camera passwords and keys: private to the owner.
        // A new file is created private; an existing one keeps its permissions
        // (on Unix they are tightened every time).
        for p in [last.as_str(), path] {
            if std::path::Path::new(p).exists() {
                write_synced(p)?;
            } else {
                crate::tls::write_private(std::path::Path::new(p), text.as_bytes())?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
            }
        }
        Ok(())
    }
}

pub fn slug(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Config, parse_schedule, schedule_active};

    #[test]
    fn storage_cap_zero_or_left_out_is_automatic() {
        let c: Config = toml::from_str("[server]\n[storage]\n").unwrap();
        assert_eq!((c.storage.max_mb, c.storage.min_free_percent), (0, 10), "left out = automatic, 10% free");
        let c: Config = toml::from_str("[server]\n").unwrap();
        assert_eq!(c.storage.max_mb, 0, "no [storage] at all = automatic");
        let c: Config = toml::from_str("[server]\n[storage]\nmax_mb = 0\nmin_free_percent = 95\n").unwrap();
        assert_eq!((c.storage.max_mb, c.storage.min_free_pct()), (0, 90), "never more than 90% kept free");
        // An explicit cap stays as it was (existing installs).
        let c: Config = toml::from_str("[server]\n[storage]\nmax_mb = 2048\n[[storage.folders]]\nname = \"nas\"\npath = \"/mnt/nas\"\n[[storage.folders]]\nname = \"big\"\npath = \"/mnt/big\"\nmax_mb = 500000\n").unwrap();
        assert_eq!(c.storage.max_mb, 2048);
        assert_eq!((c.storage.folders[0].max_mb, c.storage.folders[1].max_mb), (0, 500000));
        assert_eq!(super::StorageConfig::default().max_mb, 0);
        // The starter config for new installs is automatic.
        let starter: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert_eq!((starter.storage.max_mb, starter.storage.min_free_percent), (0, 10));
    }

    #[test]
    fn a_config_cut_short_recovers_from_its_copy() {
        let dir = std::env::temp_dir().join(format!("amber-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        let p = p.to_str().unwrap();
        let mut c: Config = toml::from_str("[server]\nport = 8123\n").unwrap();
        c.server.name = Some("Cabin".into());
        c.save(p).unwrap();
        assert_eq!(Config::load(p).unwrap().server.port, 8123);
        // Power cut mid-write: the file holds only part of the text.
        let full = std::fs::read_to_string(p).unwrap();
        // (Cut inside a key, so what is left is never valid by chance.)
        let cut = full.find("segment_seconds").unwrap() + 4;
        std::fs::write(p, &full[..cut]).unwrap();
        let back = Config::load(p).unwrap();
        assert_eq!(back.server.name.as_deref(), Some("Cabin"));
        assert_eq!(std::fs::read_to_string(p).unwrap(), full, "restored");
        // A real mistake typed by hand is still reported, not silently replaced.
        std::fs::write(p, "[server\nport = 1\n").unwrap();
        assert!(Config::load(p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn groups_fold_legacy_and_motion_overrides_merge() {
        let toml_text = r#"
[server]
[storage]
[[cameras]]
name = "A"
url = "rtsp://x"
group = "Front"
groups = ["Outside", "front", " "]
pre_roll = 9
min_frames = 0
motion_algorithm = "frame-diff"
[[cameras]]
name = "B"
url = "rtsp://y"
"#;
        let path = std::env::temp_dir().join(format!("amber-groups-{}.toml", std::process::id()));
        std::fs::write(&path, toml_text).unwrap();
        let cfg = Config::load(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.cameras[0].groups, vec!["Front".to_string(), "Outside".to_string()]);
        assert!(cfg.cameras[0].group.is_empty());
        let m = cfg.motion_for("A");
        assert_eq!(m.pre_roll, 9);
        assert_eq!(m.min_frames, 1, "min_frames never below 1");
        assert_eq!(m.algorithm, "frame-diff");
        assert_eq!(cfg.motion_for("B").pre_roll, cfg.motion.pre_roll);
        // Written back: groups only, no legacy key.
        let out = toml::to_string_pretty(&cfg).unwrap();
        assert!(out.contains("groups = ["));
        assert!(!out.contains("\ngroup = "));
        assert!(cfg.cameras[0].uses_channel("telegram"));
        // also_trigger: off by default; by name or by group; never itself.
        let (mut a, b) = (cfg.cameras[0].clone(), cfg.cameras[1].clone());
        assert!(!a.triggers(&b), "nothing is triggered by default");
        a.also_trigger = vec!["B".into()];
        assert!(a.triggers(&b));
        a.also_trigger = vec!["group:outside".into()];
        assert!(!a.triggers(&b));
        let mut b2 = b.clone();
        b2.groups = vec!["Outside".into()];
        assert!(a.triggers(&b2));
        assert!(!a.triggers(&a.clone()), "a camera never triggers itself");
    }

    #[test]
    fn sun_times_known_places() {
        use super::sun_times;
        // New York, 21 June 2024 (day 173), EDT: sunrise 05:25, sunset 20:31.
        let (r, s) = sun_times(40.7128, -74.0060, 173, -240).unwrap();
        assert!((r as i32 - 325).abs() <= 3, "sunrise {r}");
        assert!((s as i32 - 1231).abs() <= 3, "sunset {s}");
        // Denver, 21 December 2024 (day 356), MST: sunrise 07:18, sunset 16:39.
        let (r, s) = sun_times(39.7392, -104.9903, 356, -420).unwrap();
        assert!((r as i32 - 438).abs() <= 3, "sunrise {r}");
        assert!((s as i32 - 999).abs() <= 3, "sunset {s}");
        // Tromso midsummer: the sun never sets.
        assert!(sun_times(69.65, 18.96, 173, 120).is_none());
    }

    #[test]
    fn time_specs() {
        use super::{parse_time_spec, resolve_time, TimeSpec};
        assert_eq!(parse_time_spec("07:30"), Some(TimeSpec::Clock(450)));
        assert_eq!(parse_time_spec("Sunset"), Some(TimeSpec::Sun { rise: false, offset: 0 }));
        assert_eq!(parse_time_spec("sunset - 30"), Some(TimeSpec::Sun { rise: false, offset: -30 }));
        assert_eq!(parse_time_spec("sunrise+45"), Some(TimeSpec::Sun { rise: true, offset: 45 }));
        assert_eq!(parse_time_spec("sunrise+999"), None);
        assert_eq!(parse_time_spec("dusk"), None);
        let sun = Some((400, 1100));
        assert_eq!(resolve_time(TimeSpec::Sun { rise: false, offset: -30 }, sun), Some(1070));
        assert_eq!(resolve_time(TimeSpec::Sun { rise: true, offset: 0 }, None), None);
        assert_eq!(resolve_time(TimeSpec::Clock(5), None), Some(5));
    }
    #[test]
    fn automation_validation() {
        let ok = |t: &str| toml::from_str::<Config>(t).unwrap().automation.validate();
        let base = "[automation]\ndefault_profile = \"Home\"\n[[automation.profiles]]\nname = \"Home\"\n[[automation.profiles]]\nname = \"Away\"\n";
        assert!(ok(base).is_ok());
        let rule = |extra: &str| format!("{base}[[automation.schedule]]\n{extra}\n");
        assert!(ok(&rule("days=[\"mon\",\"fri\"]\nfrom=\"22:00\"\nto=\"06:00\"\nprofile=\"Away\"")).is_ok());
        assert!(ok(&rule("from=\"22:00\"\nto=\"06:00\"\nprofile=\"Nope\"")).unwrap_err().contains("unknown profile 'Nope'"));
        assert!(ok(&rule("from=\"25:00\"\nto=\"06:00\"\nprofile=\"Away\"")).unwrap_err().contains("start time"));
        assert!(ok(&rule("days=[\"xyz\"]\nfrom=\"01:00\"\nto=\"06:00\"\nprofile=\"Away\"")).unwrap_err().contains("unknown day"));
        assert!(ok("[automation]\ndefault_profile = \"Gone\"\n").unwrap_err().contains("default profile"));
        assert!(ok("[[automation.profiles]]\nname = \"A\"\n[[automation.profiles]]\nname = \"a\"\n").unwrap_err().contains("twice"));
    }

    #[test]
    fn schedule_parse_ok() {
        assert_eq!(parse_schedule("20:00-07:00"), Some((1200, 420)));
        assert_eq!(parse_schedule("0:00-23:59"), Some((0, 1439)));
        assert_eq!(parse_schedule(" 10:30-11:00 "), Some((630, 660)));
    }

    #[test]
    fn toml_round_trip() {
        let lines = [
            "[alerts]",
            "webhook_url = \"http://example.com/hook\"",
            "telegram_bot_token = \"123:abc\"",
            "telegram_chat_id = \"-456\"",
            "ntfy_url = \"https://ntfy.sh/mytopic\"",
            "action_command = \"/bin/true {image} {clip}\"",
            "image_post_url = \"http://127.0.0.1:9/img\"",
            "ha_url = \"http://127.0.0.1:8123\"",
            "ha_token = \"tok\"",
            "ha_entity = \"switch.porch_light\"",
            "",
            "[motion]",
            "schedule = \"08:00-18:00\"",
            "",
            "[odt]",
            "enabled = true",
            "command = \"detect --in {image}\"",
            "[[server.users]]",
            "username = \"viewer\"",
            "password = \"v1\"",
            "admin = false",
            "[[cameras]]",
            "name = \"Front\"",
            "short_name = \"F\"",
            "group = \"Yard\"",
            "url = \"rtsp://x\"",
            "schedule = \"20:00-07:00\"",
            "record_motion_only = true",
        ];
        let text = lines.join("\n");
        let cfg: Config = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("toml: {}", e));
        assert_eq!(cfg.alerts.telegram_bot_token, Some("123:abc".into()));
        assert_eq!(cfg.alerts.telegram_chat_id, Some("-456".into()));
        assert_eq!(cfg.alerts.ntfy_url, Some("https://ntfy.sh/mytopic".into()));
        assert_eq!(cfg.alerts.action_command, Some("/bin/true {image} {clip}".into()));
        assert_eq!(cfg.alerts.image_post_url, Some("http://127.0.0.1:9/img".into()));
        assert_eq!(cfg.alerts.ha_entity, Some("switch.porch_light".into()));
        assert_eq!(cfg.motion.schedule, Some("08:00-18:00".into()));
        assert!(cfg.odt.enabled);
        assert_eq!(cfg.odt.command, Some("detect --in {image}".into()));
        assert_eq!(cfg.cameras.len(), 1);
        assert_eq!(cfg.cameras[0].schedule, Some("20:00-07:00".into()));
        assert!(cfg.cameras[0].record_motion_only);
        assert_eq!(cfg.cameras[0].short_name, "F");
        assert_eq!(cfg.cameras[0].group, "Yard");
        assert_eq!(cfg.server.users.len(), 1);
        assert!(!cfg.server.users[0].admin);
        assert_eq!(parse_schedule(&cfg.cameras[0].schedule.clone().unwrap_or("".into())), Some((1200, 420)));
    }

    #[test]
    fn schedule_parse_rejects_garbage() {
        assert_eq!(parse_schedule(""), None);
        assert_eq!(parse_schedule("24:00-07:00"), None);
        assert_eq!(parse_schedule("07:60"), None);
        assert_eq!(parse_schedule("07:00"), None);
        assert_eq!(parse_schedule("abc-def"), None);
    }

    #[test]
    fn schedule_windows() {
        assert!(schedule_active(None, 0));
        // plain window
        assert!(schedule_active(parse_schedule("09:00-17:00"), 540));
        assert!(!schedule_active(parse_schedule("09:00-17:00"), 539));
        assert!(schedule_active(parse_schedule("09:00-17:00"), 1020));
        assert!(!schedule_active(parse_schedule("09:00-17:00"), 1021));
        // wrap past midnight
        assert!(schedule_active(parse_schedule("20:00-07:00"), 1439));
        assert!(schedule_active(parse_schedule("20:00-07:00"), 0));
        assert!(schedule_active(parse_schedule("20:00-07:00"), 420));
        assert!(!schedule_active(parse_schedule("20:00-07:00"), 421));
        assert!(!schedule_active(parse_schedule("20:00-07:00"), 1199));
    }
}

fn default_smtp_port() -> u16 { 587 }
fn default_smtp_tls() -> String { "starttls".into() }

#[cfg(test)]
mod alert_channel_tests {
    use super::Config;

    #[test]
    fn mqtt_and_smtp_round_trip() {
        let text = r#"
[alerts]
mqtt_url = "mqtt://u:p@10.0.0.2:1883"
mqtt_prefix = "cams"
mqtt_discovery = true
smtp_host = "smtp.example.com"
smtp_port = 465
smtp_user = "me"
smtp_password = "pw"
smtp_from = "cam@example.com"
email_to = ["a@example.com", "b@example.com"]
smtp_tls = "tls"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        for c in [&cfg, &back] {
            assert_eq!(c.alerts.mqtt_url.as_deref(), Some("mqtt://u:p@10.0.0.2:1883"));
            assert_eq!(c.alerts.mqtt_prefix.as_deref(), Some("cams"));
            assert!(c.alerts.mqtt_discovery);
            assert_eq!((c.alerts.smtp_port, c.alerts.smtp_tls.as_str()), (465, "tls"));
            assert_eq!(c.alerts.email_to.len(), 2);
            assert_eq!(c.alerts.smtp_password.as_deref(), Some("pw"));
        }
        let d: Config = toml::from_str("[alerts]\n").unwrap();
        assert_eq!((d.alerts.smtp_port, d.alerts.smtp_tls.as_str()), (587, "starttls"));
        assert!(!d.alerts.mqtt_discovery && d.alerts.email_to.is_empty());
    }

    #[test]
    fn notify_only_for_objects_filters_alerts() {
        let cfg: Config = toml::from_str(
            "[[cameras]]\nname = \"A\"\nurl = \"rtsp://x/a\"\n\n[[cameras]]\nname = \"B\"\nurl = \"rtsp://x/b\"\nalert_labels = [\"Person\", \"vehicle\"]\n\n[[cameras]]\nname = \"C\"\nurl = \"rtsp://x/c\"\nalert_labels = []\n",
        )
        .unwrap();
        let o = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Unset or empty = everything, as before.
        assert!(cfg.cameras[0].notifies_for(&o(&["dog"])) && cfg.cameras[0].notifies_for(&[]));
        assert!(cfg.cameras[2].notifies_for(&o(&["dog"])));
        let b = &cfg.cameras[1];
        assert!(b.notifies_for(&o(&["person"])) && b.notifies_for(&o(&["dog", "person"])));
        assert!(b.notifies_for(&o(&["truck"])), "vehicle covers cars and trucks");
        assert!(!b.notifies_for(&o(&["dog"])) && !b.notifies_for(&o(&["cat", "bird"])));
        assert!(!b.notifies_for(&[]), "a plain motion event has no object");
        // Sounds, doorbell presses and packages always pass, also among other labels.
        for l in [&["visitor"][..], &["glass", "sound"], &["package"], &["package picked up"], &["dog", "visitor"], &["Sound", "cat"]] {
            assert!(b.notifies_for(&o(l)), "{l:?}");
        }
        // AI off: no object can be named, so the filter has nothing to go on.
        assert!(b.notifies_for_event(&[], false) && b.notifies_for_event(&o(&["unusual"]), false));
        assert!(!b.notifies_for_event(&[], true), "AI on and nothing found stays silent");
        assert!(!b.notifies_for_event(&o(&["dog"]), false) && !b.notifies_for_event(&o(&["dog"]), true), "an object that was named still counts");
        // Not written back when unset.
        assert!(!toml::to_string(&cfg.cameras[0]).unwrap().contains("alert_labels"));
    }

    #[test]
    fn browser_notifications_are_a_channel_cameras_can_pick() {
        assert!(super::ALERT_CHANNELS.contains(&"push"));
        let cfg: Config = toml::from_str(
            "[[cameras]]\nname = \"A\"\nurl = \"rtsp://x/a\"\n\n[[cameras]]\nname = \"B\"\nurl = \"rtsp://x/b\"\nalert_channels = [\"ntfy\"]\n\n[[cameras]]\nname = \"C\"\nurl = \"rtsp://x/c\"\nalert_channels = [\"push\", \"email\"]\n",
        )
        .unwrap();
        // Unset = every service, browser notifications included.
        assert!(cfg.cameras[0].uses_channel("push"));
        assert!(!cfg.cameras[1].uses_channel("push") && cfg.cameras[1].uses_channel("ntfy"));
        assert!(cfg.cameras[2].uses_channel("push") && !cfg.cameras[2].uses_channel("ntfy"));
    }
}

/// A 32-bit number written the way a person typed it (0.7, not 0.699999988079071):
/// settings sent to the web page and saved in the config file stay readable.
fn ser_f32<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
    if v.is_finite() { s.serialize_f64(f32_json(*v)) } else { s.serialize_f32(*v) }
}

/// A 32-bit number as the digits a person would write (0.7, not 0.699999988079071), for JSON.
pub fn f32_json(v: f32) -> f64 {
    format!("{v}").parse::<f64>().unwrap_or(f64::from(v))
}

fn ser_opt_f32<S: serde::Serializer>(v: &Option<f32>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(x) => ser_f32(x, s),
        None => s.serialize_none(),
    }
}

#[cfg(test)]
mod f32_tests {
    #[test]
    fn small_numbers_keep_their_typed_digits() {
        #[derive(serde::Serialize)]
        struct T {
            #[serde(serialize_with = "super::ser_f32")]
            a: f32,
            #[serde(serialize_with = "super::ser_opt_f32")]
            b: Option<f32>,
        }
        let j = serde_json::to_string(&T { a: 0.7, b: Some(0.35) }).unwrap();
        assert_eq!(j, r#"{"a":0.7,"b":0.35}"#);
    }
}
