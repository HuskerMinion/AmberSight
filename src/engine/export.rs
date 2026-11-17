use super::*;

impl Engine {
    /// Evidence export: store-only ZIP of matching clips plus events.csv,
    /// written to `dest` one clip at a time (only one clip is in memory).
    /// No external deps; opens in unzip, 7-Zip, Explorer, Python zipfile.
    /// `cancelled`: asked between clips; true (the download was stopped)
    /// ends the export with an `Interrupted` error.
    #[allow(clippy::too_many_arguments)]
    pub fn export_zip_to(self: &Arc<Engine>, dest: &std::path::Path, camera: Option<String>, since: Option<i64>,
                         until: Option<i64>, object: Option<String>, cancelled: &dyn Fn() -> bool) -> std::io::Result<()> {
        let evs = self.filtered_events(EXPORT_MAX_EVENTS, camera, since, until, 0, object, true);
        self.export_events_zip_to(dest, &evs, cancelled)
    }

    /// As [`Engine::export_zip_to`], for exactly these events.
    pub fn export_events_zip_to(self: &Arc<Engine>, dest: &std::path::Path, evs: &[Event], cancelled: &dyn Fn() -> bool) -> std::io::Result<()> {
        let mut zip = ZipOut::new(std::io::BufWriter::new(std::fs::File::create(dest)?));
        let mut rows: Vec<String> = Vec::new();
        rows.push("id,camera,start,end,score,objects,clip".to_string());
        let mut total: u64 = 0;
        let mut burned = 0usize;
        let mut left_out = 0usize;
        for ev in evs.iter() {
            if cancelled() {
                return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "export cancelled"));
            }
            let objs = ev.objects.join(";");
            let row = |clip: &str| {
                [csv_field(&ev.id), csv_field(&ev.camera), ev.start.to_string(), ev.end.to_string(), format!("{:.4}", ev.score), csv_field(&objs), csv_field(clip)].join(",")
            };
            if total >= EXPORT_MAX_BYTES {
                rows.push(row("not included: export too large, choose a shorter time range"));
                continue;
            }
            let mask = match crate::transcode::masked::event_download_mask(self, ev) {
                Ok(m) => m,
                Err(_) => {
                    rows.push(row(EXPORT_MASKS_UNKNOWN));
                    continue;
                }
            };
            if mask.is_some() && burned >= EXPORT_MAX_BURNED {
                // Each conversion takes a while: an export makes only so many.
                left_out += 1;
                rows.push(row(EXPORT_BURNED_LEFT_OUT));
                continue;
            }
            burned += usize::from(mask.is_some());
            let name = format!("{}.mp4", ev.id);
            match mask {
                // A clip that may show what the masks cover: them burned in
                // (never the clip as recorded), at most what is left of the
                // export's size, and copied into the zip from its file.
                Some(mask) => {
                    let left = EXPORT_MAX_BYTES.saturating_sub(total).max(1);
                    let got = crate::transcode::masked::event_burn(self, ev, Some(mask), None)
                        .and_then(|b| b.map(|b| crate::transcode::masked::burn_to_file(self, b, crate::transcode::masked::Priority::Export, cancelled, left)).transpose());
                    match got {
                        Ok(Some(f)) => {
                            total += f.size();
                            rows.push(row(&name));
                            zip.add_file(&name, f.path())?;
                        }
                        Ok(None) => rows.push(row("no recording")),
                        Err(e) if e.downcast_ref::<crate::transcode::masked::BurnCancelled>().is_some() => {
                            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "export cancelled"));
                        }
                        Err(e) if e.downcast_ref::<crate::transcode::masked::BurnTooLarge>().is_some() => {
                            total = EXPORT_MAX_BYTES;
                            rows.push(row("not included: export too large, choose a shorter time range"));
                        }
                        Err(e) => {
                            tracing::warn!("export: {}: the privacy masks could not be burned in: {e:#}", ev.id);
                            rows.push(row("not included: the privacy masks could not be burned in (details are in the server log)"));
                        }
                    }
                }
                None => match self.export_clip_bytes(ev) {
                    // (A zip's offsets are 32-bit: one clip never takes it past them.)
                    Some(b) if total + b.len() as u64 > ZIP_HARD_MAX => rows.push(row("not included: export too large, choose a shorter time range")),
                    Some(b) => {
                        total += b.len() as u64;
                        rows.push(row(&name));
                        zip.add(&name, &b)?;
                    }
                    None => rows.push(row("no recording")),
                },
            }
        }
        if left_out > 0 {
            tracing::info!("export: {left_out} clips left out: at most {EXPORT_MAX_BURNED} get their privacy masks burned in per export");
        }
        rows.push("".into());
        zip.add("events.csv", rows.join("\n").as_bytes())?;
        let mut w = zip.finish()?;
        std::io::Write::flush(&mut w)
    }

    /// An event's clip as it is: its saved file, else assembled from the
    /// recordings with the time as a subtitle track (no conversion).
    fn export_clip_bytes(&self, ev: &Event) -> Option<Vec<u8>> {
        if let Ok(b) = std::fs::read(self.clip_file_path(&ev.id)) {
            return Some(b);
        }
        let c = self.virtual_clip(ev).ok().flatten()?;
        let c = match c.clone().for_download(None, &ev.camera, &crate::transcode::masked::clock) {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!("export: {}: no time subtitles: {e:#}", ev.id);
                c
            }
        };
        c.read_range(0, c.len()).ok()
    }
}

/// At most this many events, and about this many bytes of video, per export
/// (store-only ZIP offsets are 32-bit).
pub(crate) const EXPORT_MAX_EVENTS: usize = 500;
const EXPORT_MAX_BYTES: u64 = 1 << 30;
/// At most this many clips per export have their privacy masks burned in
/// (each is a conversion); the rest are listed in events.csv as left out.
pub(crate) const EXPORT_MAX_BURNED: usize = 50;
/// The most a whole export may hold (its zip's offsets are 32-bit).
const ZIP_HARD_MAX: u64 = (u32::MAX as u64) - (64 << 20);
const EXPORT_MASKS_UNKNOWN: &str = "not included: the privacy masks it needs could not be read (details are in the server log)";
const EXPORT_BURNED_LEFT_OUT: &str = "not included: an export burns the privacy masks into at most 50 clips; export fewer events at a time";

/// One CSV field: quoted, quotes doubled, and a leading = + - @ neutralized
/// so a spreadsheet does not run it as a formula.
pub(super) fn csv_field(v: &str) -> String {
    let v = if v.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{v}") } else { v.to_string() };
    format!("\"{}\"", v.replace('"', "\"\""))
}

/// Store-only ZIP writer (no compression, no deps), writing each file as
/// it is added. Enough for every mainstream unzip tool; clips are
/// already-compressed mp4 anyway.
pub(super) struct ZipOut<W: std::io::Write> {
    w: W,
    pos: u64,
    entries: Vec<(String, u32, u32, u32)>,
}

impl<W: std::io::Write> ZipOut<W> {
    pub(super) fn new(w: W) -> Self {
        ZipOut { w, pos: 0, entries: Vec::new() }
    }
    fn put(&mut self, b: &[u8]) -> std::io::Result<()> {
        self.w.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }
    pub(super) fn add(&mut self, name: &str, data: &[u8]) -> std::io::Result<()> {
        self.add_from(name, zip_crc32(data), data.len() as u64, &mut std::io::Cursor::new(data))
    }

    /// Add the file at `path`, read twice (its checksum, then its bytes)
    /// rather than held in memory.
    pub(super) fn add_file(&mut self, name: &str, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut crc = !0u32;
        let mut buf = vec![0u8; 1 << 20];
        let mut len = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crc = zip_crc32_update(crc, &buf[..n]);
            len += n as u64;
        }
        let f = std::fs::File::open(path)?;
        self.add_from(name, !crc, len, &mut f.take(len))
    }

    fn add_from(&mut self, name: &str, crc: u32, len: u64, data: &mut dyn std::io::Read) -> std::io::Result<()> {
        if len > u32::MAX as u64 || self.pos + len > u32::MAX as u64 {
            return Err(std::io::Error::other("the export is too large for a zip"));
        }
        let offset = self.pos as u32;
        let size = len as u32;
        // Local file header (30 bytes + name).
        let mut h: Vec<u8> = vec![0x50, 0x4B, 0x03, 0x04];
        zip_u16(&mut h, 20); zip_u16(&mut h, 0); zip_u16(&mut h, 0);
        zip_u16(&mut h, 0); zip_u16(&mut h, 33); // dos time 00:00, date 1980-01-01
        zip_u32(&mut h, crc);
        zip_u32(&mut h, size); // compressed size (stored)
        zip_u32(&mut h, size); // uncompressed size
        zip_u16(&mut h, name.len() as u16); // filename length
        zip_u16(&mut h, 0); // extra field length
        h.extend(name.as_bytes());
        self.put(&h)?;
        let copied = std::io::copy(data, &mut self.w)?;
        self.pos += copied;
        if copied != len {
            return Err(std::io::Error::other("the file changed while it was added"));
        }
        self.entries.push((name.to_string(), crc, size, offset));
        Ok(())
    }
    /// Write the central directory; returns the writer.
    pub(super) fn finish(mut self) -> std::io::Result<W> {
        let cd_offset = self.pos as u32;
        let mut out: Vec<u8> = Vec::new();
        for (name, crc, size, offset) in self.entries.iter() {
            out.extend([0x50, 0x4B, 0x01, 0x02]);
            zip_u16(&mut out, 20); zip_u16(&mut out, 20); zip_u16(&mut out, 0); zip_u16(&mut out, 0);
            zip_u16(&mut out, 0); zip_u16(&mut out, 33);
            zip_u32(&mut out, *crc);
            zip_u32(&mut out, *size); zip_u32(&mut out, *size);
            zip_u16(&mut out, name.len() as u16); // filename length
            zip_u16(&mut out, 0); // extra field length
            zip_u16(&mut out, 0); // file comment length
            zip_u16(&mut out, 0); // disk number start
            zip_u16(&mut out, 0); // internal attrs
            zip_u32(&mut out, 0); // external attrs
            zip_u32(&mut out, *offset); // local header offset
            out.extend(name.as_bytes());
        }
        let cd_size = out.len() as u32;
        out.extend([0x50, 0x4B, 0x05, 0x06]);
        zip_u16(&mut out, 0); // this disk
        zip_u16(&mut out, 0); // disk with central directory
        zip_u16(&mut out, self.entries.len() as u16); // total entries
        zip_u16(&mut out, self.entries.len() as u16); // total directory entries
        zip_u32(&mut out, cd_size); // central directory size
        zip_u32(&mut out, cd_offset); // central directory offset
        zip_u16(&mut out, 0); // comment length
        self.put(&out)?;
        Ok(self.w)
    }
}

/// A whole store-only ZIP in memory (small archives).
#[allow(dead_code)]
pub(super) fn zip_store(files: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let mut z = ZipOut::new(Vec::new());
    for (name, data) in &files {
        let _ = z.add(name, data);
    }
    z.finish().unwrap_or_default()
}

pub(super) fn zip_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend([(v & 0xFF) as u8, ((v >> 8) & 0xFF) as u8]);
}
pub(super) fn zip_u32(buf: &mut Vec<u8>, v: u32) {
    zip_u16(buf, (v & 0xFFFF) as u16);
    zip_u16(buf, ((v >> 16) & 0xFFFF) as u16);
}

pub(super) fn zip_crc32(data: &[u8]) -> u32 {
    !zip_crc32_update(!0, data)
}

/// The CRC-32 register after `data`, from `crc` (start with !0, end with !).
pub(super) fn zip_crc32_update(crc: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, v) in t.iter_mut().enumerate() {
            *v = (0..8).fold(n as u32, |acc, _| if acc & 1 == 1 { (acc >> 1) ^ 0xEDB88320 } else { acc >> 1 });
        }
        t
    });
    data.iter().fold(crc, |acc, b| (acc >> 8) ^ table[((acc ^ (*b as u32)) & 0xFF) as usize])
}

#[cfg(test)]
mod csv_tests {
    use super::csv_field;

    #[test]
    fn zip_written_piece_by_piece_is_a_valid_archive() {
        let z = super::zip_store(vec![("a.txt".into(), b"hello".to_vec()), ("b.csv".into(), b"x,y\n".to_vec())]);
        assert_eq!(&z[..4], &[0x50, 0x4B, 0x03, 0x04]);
        // End record: 2 entries, central directory right after the data.
        let end = &z[z.len() - 22..];
        assert_eq!(&end[..4], &[0x50, 0x4B, 0x05, 0x06]);
        assert_eq!(u16::from_le_bytes([end[8], end[9]]), 2);
        let cd_off = u32::from_le_bytes([end[16], end[17], end[18], end[19]]) as usize;
        assert_eq!(cd_off, 30 + 5 + 5 + 30 + 5 + 4);
        assert_eq!(&z[cd_off..cd_off + 4], &[0x50, 0x4B, 0x01, 0x02]);
        assert_eq!(super::zip_crc32(b"hello"), 0x3610a686);
    }

    #[test]
    fn a_file_added_from_disk_is_the_same_entry_as_one_from_memory() {
        let dir = std::env::temp_dir().join(format!("amber-zipfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("clip.mp4");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let mut a = super::ZipOut::new(Vec::new());
        a.add("c.mp4", &data).unwrap();
        let mut b = super::ZipOut::new(Vec::new());
        b.add_file("c.mp4", &p).unwrap();
        assert_eq!(a.finish().unwrap(), b.finish().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn masked_engine(n: usize) -> (std::sync::Arc<crate::engine::Engine>, Vec<crate::engine::Event>) {
        let cfg: crate::config::Config = toml::from_str(&format!(
            "[[cameras]]\nname = \"Export Mask {}\"\nurl = \"rtsp://192.0.2.10/s\"\nmask_mode = \"screen\"\nprivacy_masks = [{{ x = 0.1, y = 0.1, w = 0.2, h = 0.2 }}]\n",
            std::process::id()
        ))
        .unwrap();
        let cam = cfg.cameras[0].name.clone();
        let evs = (0..n)
            .map(|i| crate::engine::Event { id: format!("exportmask-nofile-{}-{i}", std::process::id()), camera_id: 0, camera: cam.clone(), start: 1_000 + i as i64 * 100, end: 1_010 + i as i64 * 100, score: 0.5, clip: None, thumb: None, objects: vec![], protect: false })
            .collect();
        (crate::engine::Engine::for_test(cfg), evs)
    }

    #[test]
    fn a_stopped_export_ends_at_the_next_clip() {
        let (e, evs) = masked_engine(3);
        let dest = std::env::temp_dir().join(format!("amber-export-stop-{}.zip", std::process::id()));
        let asked = std::cell::Cell::new(0);
        let r = e.export_events_zip_to(&dest, &evs, &|| {
            asked.set(asked.get() + 1);
            asked.get() > 1
        });
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(asked.get(), 2, "asked before each clip, stopped at the second");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn an_export_burns_masks_into_so_many_clips_and_lists_the_rest() {
        let n = super::EXPORT_MAX_BURNED + 3;
        let (e, evs) = masked_engine(n);
        let dest = std::env::temp_dir().join(format!("amber-export-cap-{}.zip", std::process::id()));
        e.export_events_zip_to(&dest, &evs, &|| false).unwrap();
        // (Store-only: the CSV is there as text.)
        let z = String::from_utf8_lossy(&std::fs::read(&dest).unwrap()).into_owned();
        assert_eq!(z.matches(super::EXPORT_BURNED_LEFT_OUT).count(), 3, "{z}");
        assert_eq!(z.matches("\"no recording\"").count(), super::EXPORT_MAX_BURNED);
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn csv_fields_are_quoted_and_safe() {
        assert_eq!(csv_field("Front Door"), "\"Front Door\"");
        assert_eq!(csv_field("say \"hi\", ok"), "\"say \"\"hi\"\", ok\"");
        assert_eq!(csv_field("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
    }
}
