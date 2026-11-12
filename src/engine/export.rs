use super::*;

impl Engine {
    /// Evidence export: store-only ZIP of matching clips plus events.csv,
    /// written to `dest` one clip at a time (only one clip is in memory).
    /// No external deps; opens in unzip, 7-Zip, Explorer, Python zipfile.
    pub fn export_zip_to(self: &Arc<Engine>, dest: &std::path::Path, camera: Option<String>, since: Option<i64>,
                         until: Option<i64>, object: Option<String>) -> std::io::Result<()> {
        let evs = self.filtered_events(EXPORT_MAX_EVENTS, camera, since, until, 0, object, true);
        self.export_events_zip_to(dest, &evs)
    }

    /// As [`Engine::export_zip_to`], for exactly these events.
    pub fn export_events_zip_to(self: &Arc<Engine>, dest: &std::path::Path, evs: &[Event]) -> std::io::Result<()> {
        let mut zip = ZipOut::new(std::io::BufWriter::new(std::fs::File::create(dest)?));
        let mut rows: Vec<String> = Vec::new();
        rows.push("id,camera,start,end,score,objects,clip".to_string());
        let mut total: u64 = 0;
        for ev in evs.iter() {
            let objs = ev.objects.join(";");
            let row = |clip: &str| {
                [csv_field(&ev.id), csv_field(&ev.camera), ev.start.to_string(), ev.end.to_string(), format!("{:.4}", ev.score), csv_field(&objs), csv_field(clip)].join(",")
            };
            if total >= EXPORT_MAX_BYTES {
                rows.push(row("not included: export too large, choose a shorter time range"));
                continue;
            }
            let p = self.clip_file_path(&ev.id);
            let bytes = match std::fs::read(&p) {
                Ok(b) => Some(b),
                // No saved file: assemble it from the recordings.
                Err(_) => self
                    .virtual_clip(ev)
                    .ok()
                    .flatten()
                    .and_then(|c| c.read_range(0, c.len()).ok()),
            };
            match bytes {
                Some(b) => {
                    total += b.len() as u64;
                    rows.push(row(&format!("{}.mp4", ev.id)));
                    zip.add(&format!("{}.mp4", ev.id), &b)?;
                }
                None => rows.push(row("no recording")),
            }
        }
        rows.push("".into());
        zip.add("events.csv", rows.join("\n").as_bytes())?;
        let mut w = zip.finish()?;
        std::io::Write::flush(&mut w)
    }
}

/// At most this many events, and about this many bytes of video, per export
/// (store-only ZIP offsets are 32-bit).
pub(crate) const EXPORT_MAX_EVENTS: usize = 500;
const EXPORT_MAX_BYTES: u64 = 1 << 30;

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
        let crc = zip_crc32(data);
        let offset = self.pos as u32;
        let size = data.len() as u32;
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
        self.put(data)?;
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
    let table: Vec<u32> = (0..256).map(|n| {
        let c: u32 = n as u32;
        (0..8).fold(c, |acc, _| {
            if acc & 1 == 1 { (acc >> 1) ^ 0xEDB88320 } else { acc >> 1 }
        })
    }).collect();
    let crc: u32 = data.iter().fold(0xFFFFFFFF, |acc, b| {
        (acc >> 8) ^ table[((acc ^ (*b as u32)) & 0xFF) as usize]
    }) ^ 0xFFFFFFFF;
    crc
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
    fn csv_fields_are_quoted_and_safe() {
        assert_eq!(csv_field("Front Door"), "\"Front Door\"");
        assert_eq!(csv_field("say \"hi\", ok"), "\"say \"\"hi\"\", ok\"");
        assert_eq!(csv_field("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
    }
}
