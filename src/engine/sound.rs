//! The loud-sound trigger (see crate::sound): listens to the audio of the
//! stream a camera analyses (the sub stream if it has one), and when a
//! sudden loud sound is heard keeps `CameraState::sound` "on" for a few
//! seconds. The camera's worker treats that like motion: the event is
//! recorded, labelled "sound" (plus what it was, when sound names are on)
//! and alerts like any other. Sound names (Settings > Smart features > Sounds) need the YAMNet
//! model, downloaded when they are turned on.

// Intel Macs have no built-in AI runtime, so much of this goes unused there.
#![cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code, unused_imports))]

use super::*;
use crate::sound::{labels, AudioCodec, Decoder, Heard, Loudness, Recent, TriggerSettings};
use std::time::{Duration, Instant};

/// A loud sound keeps the camera "triggered" this long after it was last loud.
const HOLD_MS: i64 = 4_000;
/// Listen this long after a sound starts before naming it, and name this
/// much of it (YAMNet looks at ~1 s).
const NAME_AFTER_MS: u32 = 600;
const NAME_SPAN_MS: u32 = 1_000;
/// The meter keeps the microphone decoded this long after it was last looked at.
const WATCH_S: i64 = 10;

/// What a camera's microphone is doing.
#[derive(Default, Debug, Clone)]
pub struct SoundState {
    /// Latest 100 ms loudness, the usual background, and the level that
    /// counts as loud (dBFS); when measured (epoch ms).
    pub level: Option<f32>,
    pub background: Option<f32>,
    pub threshold: Option<f32>,
    pub measured_at: i64,
    /// A loud sound keeps events going until this time (epoch ms).
    pub until: i64,
    /// What the current loud sound is: "sound", and its names when known.
    pub labels: Vec<String>,
    /// When the last loud sound started (epoch s), and what it was.
    pub heard_at: i64,
    pub heard: Vec<String>,
    /// Someone is looking at the meter until then (epoch s).
    pub watch_until: i64,
    /// Why the sound cannot be listened to.
    pub error: Option<String>,
}

impl SoundState {
    pub fn active(&self, now_ms: i64) -> bool {
        now_ms < self.until
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
static MODEL: RwLock<Option<Arc<labels::SoundModel>>> = RwLock::new(None);

#[derive(Clone, Default, serde::Serialize)]
pub struct SoundDownload {
    pub running: bool,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

static DOWNLOAD: Mutex<SoundDownload> = Mutex::new(SoundDownload { running: false, done: 0, total: 0, error: None });

/// Sound names are on and the model is ready.
fn names_ready() -> bool {
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    return MODEL.read().unwrap().is_some();
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    false
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl Engine {
    fn sound_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("models")
    }

    /// Sound names are on and the model is ready.
    pub fn sound_names_ready(&self) -> bool {
        names_ready()
    }

    /// Match the settings: load the sound model (downloading it first if
    /// needed), or drop it when sound names are off.
    pub fn reload_sound(self: &Arc<Self>) {
        let enabled = self.config.lock().unwrap().sound.labels;
        #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
        {
            if !enabled {
                *MODEL.write().unwrap() = None;
                return;
            }
            if MODEL.read().unwrap().is_some() {
                return;
            }
            if !labels::model_present(&self.sound_dir()) {
                self.start_sound_download();
                return;
            }
            match labels::SoundModel::load(&self.sound_dir()) {
                Ok(m) => {
                    info!("sound names: model loaded");
                    DOWNLOAD.lock().unwrap().error = None;
                    *MODEL.write().unwrap() = Some(Arc::new(m));
                }
                Err(e) => {
                    warn!("sound names: {e:#}");
                    DOWNLOAD.lock().unwrap().error = Some(format!("{e:#}"));
                }
            }
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        let _ = enabled;
    }

    fn start_sound_download(self: &Arc<Self>) {
        {
            let mut d = DOWNLOAD.lock().unwrap();
            if d.running {
                return;
            }
            *d = SoundDownload { running: true, done: 0, total: labels::MODEL_BYTES, error: None };
        }
        info!("sound names: downloading the model ({} MB)", labels::MODEL_BYTES >> 20);
        let engine = self.clone();
        thread::spawn(move || {
            let r = labels::download(&engine.sound_dir(), &|done, total| {
                let mut d = DOWNLOAD.lock().unwrap();
                d.done = done;
                d.total = total;
            });
            {
                let mut d = DOWNLOAD.lock().unwrap();
                d.running = false;
                if let Err(e) = &r {
                    warn!("sound names: {e:#}");
                    d.error = Some(super::download_failure("the sound model", e));
                }
            }
            if r.is_ok() {
                engine.reload_sound();
            }
        });
    }

    /// For Settings > Smart features > Sounds.
    pub fn sound_status(&self) -> serde_json::Value {
        let c = self.config.lock().unwrap().clone();
        let supported = cfg!(not(all(target_os = "macos", target_arch = "x86_64")));
        let cameras: Vec<String> = c.cameras.iter().filter(|x| x.audio_trigger).map(|x| x.name.clone()).collect();
        serde_json::json!({
            "labels": c.sound.labels,
            "supported": supported,
            "loaded": self.sound_names_ready(),
            "model_mb": labels::MODEL_BYTES >> 20,
            "download": DOWNLOAD.lock().unwrap().clone(),
            "names": labels::names(),
            "cameras": cameras,
        })
    }

    /// The live sound meter for a camera (camera settings, Alerts tab).
    /// Looking keeps the camera's sound measured for a few seconds even
    /// when its sound trigger is off, to help choose the settings.
    pub fn sound_meter(&self, id: usize) -> Option<serde_json::Value> {
        let h = self.camera(id)?;
        let cfg = h.running.read().unwrap().clone();
        let mut st = h.state.write().unwrap();
        let now = Utc::now().timestamp();
        st.sound.watch_until = now + WATCH_S;
        let s = &st.sound;
        let fresh = now_ms() - s.measured_at < 2_000;
        Some(serde_json::json!({
            "trigger": cfg.audio_trigger,
            "native": st.ingest == "native",
            "level": s.level.filter(|_| fresh),
            "background": s.background.filter(|_| fresh),
            "threshold": s.threshold.filter(|_| fresh),
            "loud": s.active(now_ms()),
            "heard_at": (s.heard_at > 0).then_some(s.heard_at),
            "heard": s.heard,
            "error": s.error,
            "names": self.sound_names_ready(),
        }))
    }
}

/// Listens to one session's audio. Lives in the stream task that decodes
/// the camera's picture for motion (so one stream per camera is decoded).
pub(super) struct SoundWatch {
    handle: Arc<CameraHandle>,
    name: String,
    enabled: bool,
    settings: TriggerSettings,
    /// Only these sounds start events (empty: any loud sound).
    wanted: Vec<String>,
    codec: Option<AudioCodec>,
    dec: Option<(Decoder, Loudness, Recent)>,
    pcm: Vec<f32>,
    /// Samples still to hear before naming the sound that just started.
    name_in: Option<usize>,
    last_publish: Instant,
    last_check: Instant,
    listening: bool,
    errors: u32,
}

impl SoundWatch {
    pub fn new(handle: &Arc<CameraHandle>, cfg: &CameraConfig) -> SoundWatch {
        let d = TriggerSettings::default();
        SoundWatch {
            handle: handle.clone(),
            name: cfg.name.clone(),
            enabled: cfg.audio_trigger,
            settings: TriggerSettings {
                margin_db: cfg.audio_margin_db.filter(|x| x.is_finite()).map_or(d.margin_db, |x| x.clamp(3.0, 60.0)),
                min_db: cfg.audio_min_db.filter(|x| x.is_finite()).map_or(d.min_db, |x| x.clamp(-90.0, 0.0)),
                ..d
            },
            wanted: cfg.audio_sounds.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect(),
            codec: None,
            dec: None,
            pcm: Vec::new(),
            name_in: None,
            last_publish: Instant::now(),
            last_check: Instant::now() - Duration::from_secs(10),
            listening: false,
            errors: 0,
        }
    }

    /// Whether to decode at all: the trigger is on, or someone looks at the meter.
    fn wanted_now(&mut self) -> bool {
        if self.enabled {
            return true;
        }
        if self.last_check.elapsed() >= Duration::from_millis(500) {
            self.last_check = Instant::now();
            self.listening = self.handle.state.read().unwrap().sound.watch_until > Utc::now().timestamp();
        }
        self.listening
    }

    /// One audio frame from the camera.
    pub fn push(&mut self, s: &crate::ingest::recorder::OpenStream, data: &[u8]) {
        if !self.wanted_now() {
            self.dec = None;
            self.codec = None;
            return;
        }
        let Some(codec) = s.audio_codec() else { return };
        if self.codec.as_ref() != Some(&codec) {
            self.codec = Some(codec.clone());
            self.dec = None;
            match Decoder::new(&codec) {
                Ok(d) if d.rate() == 0 => {
                    self.handle.state.write().unwrap().sound.error = Some("the camera's audio reports no sample rate".into());
                }
                Ok(d) => {
                    let rate = d.rate();
                    self.dec = Some((d, Loudness::new(rate, self.settings), Recent::new(rate as usize * 2)));
                    self.handle.state.write().unwrap().sound.error = None;
                }
                Err(e) => {
                    warn!("camera '{}': cannot listen for loud sounds: {e:#}", self.name);
                    self.handle.state.write().unwrap().sound.error = Some(format!("{e:#}"));
                }
            }
        }
        let Some((dec, loud, recent)) = self.dec.as_mut() else { return };
        self.pcm.clear();
        if let Err(e) = dec.decode(data, &mut self.pcm) {
            self.errors += 1;
            if self.errors == 20 {
                warn!("camera '{}': sound frames do not decode ({e:#})", self.name);
            }
            return;
        }
        let rate = dec.rate();
        recent.push(&self.pcm);
        let heard = loud.push(&self.pcm);
        let now = now_ms();
        if self.last_publish.elapsed() >= Duration::from_millis(100) || heard != Heard::Nothing {
            self.last_publish = Instant::now();
            let mut st = self.handle.state.write().unwrap();
            st.sound.level = Some(loud.level);
            st.sound.background = loud.background;
            st.sound.threshold = loud.threshold();
            st.sound.measured_at = now;
        }
        if !self.enabled {
            return;
        }
        let names = names_ready();
        match heard {
            Heard::Start => {
                info!(
                    "camera '{}': loud sound ({:.0} dB, usually {:.0} dB)",
                    self.name,
                    loud.level,
                    loud.background.unwrap_or(crate::sound::SILENT_DB)
                );
                let mut st = self.handle.state.write().unwrap();
                st.sound.heard_at = now / 1000;
                st.sound.heard = vec!["sound".into()];
                // With a list of sounds to watch for, nothing starts until
                // the sound has been named (and is on the list).
                if self.wanted.is_empty() {
                    st.sound.until = now + HOLD_MS;
                    st.sound.labels = vec!["sound".into()];
                } else if !names {
                    // Watching for some sounds only, but they cannot be named
                    // (sound names off, or the model not loaded): nothing
                    // starts, rather than every loud sound.
                    st.sound.error = Some("Only some sounds are picked, but sound names are off (Settings > Smart features > Sounds): nothing is recorded for sounds".into());
                }
                if names {
                    if st.sound.error.as_deref().is_some_and(|e| e.starts_with("Only some sounds")) {
                        st.sound.error = None;
                    }
                    self.name_in = Some((rate * NAME_AFTER_MS / 1000) as usize);
                }
            }
            Heard::Still => {
                let mut st = self.handle.state.write().unwrap();
                if st.sound.until > now {
                    st.sound.until = now + HOLD_MS;
                }
            }
            Heard::Nothing => {}
        }
        if let Some(n) = self.name_in {
            if n > self.pcm.len() {
                self.name_in = Some(n - self.pcm.len());
            } else {
                self.name_in = None;
                let clip = crate::sound::resample(&recent.last((rate * NAME_SPAN_MS / 1000) as usize), rate, labels::RATE);
                self.name_sound(clip);
            }
        }
    }

    /// Name the sound off this thread (a few ms of CPU), then add the names
    /// to the sound (or, with a list of sounds, start the event if it is one).
    fn name_sound(&self, clip: Vec<f32>) {
        #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
        {
            let Some(model) = MODEL.read().unwrap().clone() else { return };
            let handle = self.handle.clone();
            let name = self.name.clone();
            let wanted = self.wanted.clone();
            thread::spawn(move || {
                let t0 = Instant::now();
                let found = match model.name(&clip) {
                    Ok(f) => f,
                    Err(e) => {
                        warn!("camera '{name}': {e:#}");
                        return;
                    }
                };
                let found: Vec<String> = found.into_iter().map(|(n, _)| n.to_string()).collect();
                info!(
                    "camera '{name}': the sound was {} ({} ms)",
                    if found.is_empty() { "nothing we have a name for".to_string() } else { found.join(", ") },
                    t0.elapsed().as_millis()
                );
                let now = now_ms();
                let mut st = handle.state.write().unwrap();
                st.sound.heard = std::iter::once("sound".to_string()).chain(found.iter().cloned()).collect();
                if wanted.is_empty() {
                    for f in &found {
                        if !st.sound.labels.contains(f) {
                            st.sound.labels.push(f.clone());
                        }
                    }
                } else if found.iter().any(|f| wanted.contains(f)) {
                    st.sound.until = now + HOLD_MS;
                    st.sound.labels = st.sound.heard.clone();
                }
            });
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        let _ = clip;
    }
}
