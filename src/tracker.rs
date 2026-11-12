//! A light object tracker over AI detections, and the judging of named
//! zones on top of it: loitering (a tracked object stays inside an area
//! longer than the zone's time) and line crossing (a tracked object's feet,
//! the bottom centre of its box, cross a line in the chosen direction).
//!
//! Pure logic with times passed in (milliseconds), so it is easy to test.
//! The engine (engine/zone_watch.rs) feeds it the detections of the looks it
//! takes while a camera is in motion or someone stands in an area.
//!
//! Tracking is by overlap (intersection over union) of boxes of the same
//! kind of object between looks, then by distance between centres for
//! objects that moved further than their box between two looks. Looks are
//! 0.5 to 1.5 seconds apart, so a walking person moves a lot between them.
//! A track that is not seen is kept for MEMORY_MS (a detector missing an
//! object in a look or two does not restart a loitering timer).

use crate::ai::Detection;
use crate::config::NamedZone;
use std::collections::{HashMap, HashSet};

/// How long a track survives without being seen.
pub const MEMORY_MS: i64 = 6000;
/// Boxes overlapping this much (IoU) can be the same object.
const MIN_IOU: f32 = 0.15;
/// Someone seen outside an area for this long has left it (a step over the
/// edge for a moment keeps the timer).
const EXIT_GRACE_MS: i64 = 3000;
/// A line is crossed only when the feet end up this far past it (fraction of
/// the picture): jitter of the box at the line crosses nothing.
pub const LINE_MARGIN: f64 = 0.012;
/// One track crosses a line in one direction at most this often.
const LINE_COOLDOWN_MS: i64 = 5000;
/// A zone reports loitering at most this often.
const LOITER_COOLDOWN_MS: i64 = 30_000;

fn is_road_vehicle(label: &str) -> bool {
    matches!(label, "car" | "truck" | "bus" | "vehicle" | "van" | "suv")
}

/// The same kind of thing: the detector often calls one vehicle "car" in one
/// look and "truck" in the next.
pub fn same_kind(a: &str, b: &str) -> bool {
    a == b || (is_road_vehicle(a) && is_road_vehicle(b))
}

/// The feet of a detected object: bottom centre of its box.
pub fn feet(d: &Detection) -> (f64, f64) {
    (f64::from(d.x + d.w / 2.0), f64::from(d.y + d.h))
}

fn center(b: (f32, f32, f32, f32)) -> (f32, f32) {
    (b.0 + b.2 / 2.0, b.1 + b.3 / 2.0)
}

fn iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let (ax2, ay2, bx2, by2) = (a.0 + a.2, a.1 + a.3, b.0 + b.2, b.1 + b.3);
    let iw = (ax2.min(bx2) - a.0.max(b.0)).max(0.0);
    let ih = (ay2.min(by2) - a.1.max(b.1)).max(0.0);
    let inter = iw * ih;
    let union = a.2 * a.3 + b.2 * b.3 - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub id: u64,
    pub label: String,
    pub bx: (f32, f32, f32, f32),
    pub last_ms: i64,
}

#[derive(Debug, Default)]
pub struct Tracker {
    tracks: Vec<Track>,
    next_id: u64,
    /// Width over height of the picture (0: not known, taken as 1), so
    /// distances count the same sideways and up and down.
    aspect: f32,
}

impl Tracker {
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }
    pub fn clear(&mut self) {
        self.tracks.clear();
    }
    pub fn set_aspect(&mut self, a: f64) {
        self.aspect = if a.is_finite() && a > 0.2 && a < 10.0 { a as f32 } else { 0.0 };
    }
    fn asp(&self) -> f32 {
        if self.aspect > 0.0 { self.aspect } else { 1.0 }
    }

    /// Match the detections of one look to the tracks (new tracks for the
    /// rest) and return the track id of each detection, in order.
    pub fn update(&mut self, dets: &[Detection], now_ms: i64) -> Vec<u64> {
        self.tracks.retain(|t| now_ms - t.last_ms <= MEMORY_MS);
        let asp = self.asp();
        // (score, track, detection): higher is a better match.
        let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            for (di, d) in dets.iter().enumerate() {
                if !same_kind(&t.label, &d.label) {
                    continue;
                }
                let db = (d.x, d.y, d.w, d.h);
                let o = iou(t.bx, db);
                if o >= MIN_IOU {
                    pairs.push((1.0 + o, ti, di));
                    continue;
                }
                let (a, b) = (center(t.bx), center(db));
                let dist = (((a.0 - b.0) * asp).powi(2) + (a.1 - b.1).powi(2)).sqrt();
                let limit = (t.bx.2 * asp).max(t.bx.3).max(d.w * asp).max(d.h) + 0.05;
                if dist <= limit {
                    pairs.push((1.0 - dist / limit, ti, di));
                }
            }
        }
        pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut det_track: Vec<Option<usize>> = vec![None; dets.len()];
        let mut used = vec![false; self.tracks.len()];
        for (_, ti, di) in pairs {
            if !used[ti] && det_track[di].is_none() {
                used[ti] = true;
                det_track[di] = Some(ti);
            }
        }
        let mut out = Vec::with_capacity(dets.len());
        for (di, d) in dets.iter().enumerate() {
            let bx = (d.x, d.y, d.w, d.h);
            match det_track[di] {
                Some(ti) => {
                    let t = &mut self.tracks[ti];
                    t.bx = bx;
                    t.label = d.label.clone();
                    t.last_ms = now_ms;
                    out.push(t.id);
                }
                None => {
                    self.next_id += 1;
                    self.tracks.push(Track { id: self.next_id, label: d.label.clone(), bx, last_ms: now_ms });
                    out.push(self.next_id);
                }
            }
        }
        out
    }
}

// ---------- geometry ----------

type P = (f64, f64);

/// Point in polygon (even-odd rule).
pub fn in_polygon(pts: &[[f64; 2]], p: P) -> bool {
    if pts.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (xi, yi, xj, yj) = (pts[i][0], pts[i][1], pts[j][0], pts[j][1]);
        if (yi > p.1) != (yj > p.1) && p.0 < (xj - xi) * (p.1 - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Which side of the directed segment a->b the point is on: positive is the
/// right-hand side when walking from a to b on a picture (y grows downward),
/// the side the editor's arrow points to.
fn side_value(a: [f64; 2], b: [f64; 2], p: P) -> f64 {
    (b[0] - a[0]) * (p.1 - a[1]) - (b[1] - a[1]) * (p.0 - a[0])
}

/// Distance from the point to the segment, and which side of it (cross
/// product sign) the point is on. None for a segment of no length.
fn seg_dist(a: [f64; 2], b: [f64; 2], p: P) -> Option<(f64, f64)> {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-12 {
        return None;
    }
    let t = (((p.0 - a[0]) * dx + (p.1 - a[1]) * dy) / len2).clamp(0.0, 1.0);
    let (cx, cy) = (a[0] + t * dx, a[1] + t * dy);
    Some((((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt(), side_value(a, b, p)))
}

/// Side (1 right of the line's direction, -1 left) of a point, judged at
/// the nearest part of the line; None within `margin` of the line.
pub fn line_side(pts: &[[f64; 2]], p: P, margin: f64) -> Option<i8> {
    let mut best: Option<(f64, f64)> = None;
    for w in pts.windows(2) {
        if let Some((d, c)) = seg_dist(w[0], w[1], p) {
            if best.is_none_or(|b| d < b.0) {
                best = Some((d, c));
            }
        }
    }
    let (d, c) = best?;
    if d < margin || c == 0.0 {
        None
    } else {
        Some(if c > 0.0 { 1 } else { -1 })
    }
}

fn segs_cross(p1: P, p2: P, q1: [f64; 2], q2: [f64; 2]) -> bool {
    let o = |a: P, b: P, c: P| (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
    let (q1, q2) = ((q1[0], q1[1]), (q2[0], q2[1]));
    let (d1, d2, d3, d4) = (o(q1, q2, p1), o(q1, q2, p2), o(p1, p2, q1), o(p1, p2, q2));
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

/// The way a move from `from` to `to` crosses the line, if it does:
/// `Some(true)` towards the right-hand side of the line's direction (along
/// the arrow: "in"), `Some(false)` the other way.
pub fn crossing(pts: &[[f64; 2]], from: P, to: P) -> Option<bool> {
    for w in pts.windows(2) {
        if segs_cross(from, to, w[0], w[1]) {
            return Some(side_value(w[0], w[1], to) > 0.0);
        }
    }
    None
}

// ---------- zones ----------

#[derive(Debug, Clone, PartialEq)]
pub enum HitKind {
    /// Inside an area this long (seconds).
    Loiter { secs: i64 },
    /// Crossed a line; true: along the arrow ("in").
    Cross { inward: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub zone: String,
    pub kind: HitKind,
    pub label: String,
    /// When the object came into the area (loitering) or crossed.
    pub since_ms: i64,
}

impl Hit {
    /// The tag put on the event: "loitering: Driveway", "crossed: Gate (in)".
    pub fn tag(&self) -> String {
        match self.kind {
            HitKind::Loiter { .. } => format!("loitering: {}", self.zone),
            HitKind::Cross { inward } => format!("crossed: {} ({})", self.zone, if inward { "in" } else { "out" }),
        }
    }
}

/// Whether a label is a vehicle (one list for plates, digests, parked-object
/// and zone checks).
pub fn is_vehicle(label: &str) -> bool {
    matches!(label, "car" | "truck" | "motorcycle" | "bus" | "vehicle" | "van" | "suv")
}

/// Label prefixes of tags made by the system (rules, zones, plates) rather
/// than by what was seen: their text holds the user's own names.
pub const TAG_PREFIXES: [&str; 6] = ["rule: ", "quiet: ", "loitering: ", "crossed: ", "plate ", "unknown plate "];

/// Whether a label is such a tag or the "unusual" mark.
pub fn is_tag_label(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    l == "unusual" || TAG_PREFIXES.iter().any(|p| l.starts_with(p))
}

/// Whether an event tag was made by a zone.
pub fn is_zone_tag(tag: &str) -> bool {
    tag.starts_with("loitering: ") || tag.starts_with("crossed: ")
}

#[derive(Debug)]
struct AreaState {
    entered_ms: i64,
    last_in_ms: i64,
    fired: bool,
}

#[derive(Debug, Default)]
struct LineState {
    /// The last place seen clearly on one side, and which side.
    anchor: Option<(P, i8)>,
    last_hit: [Option<i64>; 2],
}

/// Tracker plus the state of every (object, zone) pair.
#[derive(Debug, Default)]
pub struct ZoneWatch {
    pub tracker: Tracker,
    areas: HashMap<(u64, String), AreaState>,
    lines: HashMap<(u64, String), LineState>,
    last_loiter: HashMap<String, i64>,
}

fn wants(z: &NamedZone, label: &str) -> bool {
    z.labels.iter().any(|l| same_kind(&l.trim().to_lowercase(), label))
}

impl ZoneWatch {
    /// Judge one look. `zones` are the zones watching now (enabled, inside
    /// their window); `dets` the objects seen, already over the score bar.
    #[cfg(test)]
    pub fn step(&mut self, zones: &[NamedZone], dets: &[Detection], now_ms: i64) -> Vec<Hit> {
        self.step_parked(zones, dets, &[], now_ms)
    }

    /// As `step`; `parked[i]` marks detection i as parked (not moved for
    /// minutes): zones that ignore parked objects skip it.
    pub fn step_parked(&mut self, zones: &[NamedZone], dets: &[Detection], parked: &[bool], now_ms: i64) -> Vec<Hit> {
        // (The camera's own reports have no box: nothing to track.)
        let (dets, parked): (Vec<Detection>, Vec<bool>) =
            dets.iter().enumerate().filter(|(_, d)| d.w > 0.0 && d.h > 0.0).map(|(i, d)| (d.clone(), parked.get(i).copied().unwrap_or(false))).unzip();
        let asp = f64::from(self.tracker.asp());
        let ids = self.tracker.update(&dets, now_ms);
        let live: HashSet<u64> = self.tracker.tracks().iter().map(|t| t.id).collect();
        self.areas.retain(|(t, z), _| live.contains(t) && zones.iter().any(|q| !q.is_line() && q.name == *z));
        self.lines.retain(|(t, z), _| live.contains(t) && zones.iter().any(|q| q.is_line() && q.name == *z));
        let mut hits = Vec::new();
        for ((d, &id), &is_parked) in dets.iter().zip(&ids).zip(&parked) {
            let at = feet(d);
            let at_s = (at.0 * asp, at.1);
            for z in zones {
                if !wants(z, &d.label) || (is_parked && z.ignore_parked) {
                    continue;
                }
                let key = (id, z.name.clone());
                if z.is_line() {
                    let pts: Vec<[f64; 2]> = z.points.iter().map(|p| [p[0] * asp, p[1]]).collect();
                    let Some(side) = line_side(&pts, at_s, LINE_MARGIN) else { continue };
                    let st = self.lines.entry(key).or_default();
                    if let Some((from, was)) = st.anchor {
                        if was != side {
                            if let Some(inward) = crossing(&pts, from, at_s) {
                                let i = usize::from(!inward);
                                let ok = match z.direction.as_str() {
                                    "ab" => inward,
                                    "ba" => !inward,
                                    _ => true,
                                };
                                if ok && st.last_hit[i].is_none_or(|t| now_ms - t >= LINE_COOLDOWN_MS) {
                                    st.last_hit[i] = Some(now_ms);
                                    hits.push(Hit { zone: z.name.clone(), kind: HitKind::Cross { inward }, label: d.label.clone(), since_ms: now_ms });
                                }
                            }
                        }
                    }
                    st.anchor = Some((at_s, side));
                } else if in_polygon(&z.points, at) {
                    let st = self.areas.entry(key).or_insert(AreaState { entered_ms: now_ms, last_in_ms: now_ms, fired: false });
                    st.last_in_ms = now_ms;
                    if !st.fired
                        && now_ms - st.entered_ms >= i64::from(z.seconds) * 1000
                        && self.last_loiter.get(&z.name).is_none_or(|t| now_ms - t >= LOITER_COOLDOWN_MS)
                    {
                        st.fired = true;
                        self.last_loiter.insert(z.name.clone(), now_ms);
                        let secs = (now_ms - st.entered_ms) / 1000;
                        hits.push(Hit { zone: z.name.clone(), kind: HitKind::Loiter { secs }, label: d.label.clone(), since_ms: st.entered_ms });
                    }
                } else if self.areas.get(&key).is_some_and(|st| now_ms - st.last_in_ms > EXIT_GRACE_MS) {
                    self.areas.remove(&key);
                }
            }
        }
        // Not seen at all for a while (left the picture): gone from the area.
        self.areas.retain(|_, st| now_ms - st.last_in_ms <= MEMORY_MS);
        hits
    }

    /// Someone is inside (or just left) an area: worth watching on.
    pub fn someone_inside(&self) -> bool {
        !self.areas.is_empty()
    }

    /// Someone is inside an area but has not been there long enough yet.
    pub fn waiting_inside(&self) -> bool {
        self.areas.values().any(|s| !s.fired)
    }

    /// Areas with someone loitering in them now.
    pub fn occupied(&self) -> HashSet<String> {
        self.areas.iter().filter(|(_, s)| s.fired).map(|((_, z), _)| z.clone()).collect()
    }

    pub fn clear(&mut self) {
        self.tracker.clear();
        self.areas.clear();
        self.lines.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(label: &str, x: f32, y: f32, w: f32, h: f32) -> Detection {
        Detection { label: label.into(), score: 0.9, x, y, w, h }
    }
    /// A person whose feet are at (fx, fy).
    fn person_at(fx: f32, fy: f32) -> Detection {
        det("person", fx - 0.04, fy - 0.25, 0.08, 0.25)
    }
    fn area(name: &str, secs: u32, pts: &[[f64; 2]]) -> NamedZone {
        NamedZone { name: name.into(), kind: "area".into(), points: pts.to_vec(), enabled: true, labels: vec!["person".into()], seconds: secs, direction: "both".into(), alert: true, record: true, ignore_parked: true, schedule: None }
    }
    fn line(name: &str, dir: &str, pts: &[[f64; 2]]) -> NamedZone {
        NamedZone { kind: "line".into(), direction: dir.into(), ..area(name, 30, pts) }
    }
    const SQUARE: [[f64; 2]; 4] = [[0.4, 0.4], [0.8, 0.4], [0.8, 0.9], [0.4, 0.9]];

    #[test]
    fn tracker_follows_an_object_and_keeps_ids_apart() {
        let mut t = Tracker::default();
        let a = t.update(&[person_at(0.20, 0.8), person_at(0.70, 0.8)], 0);
        assert_eq!(a.len(), 2);
        assert_ne!(a[0], a[1]);
        // Both move a little (one look later), listed in the other order.
        let b = t.update(&[person_at(0.73, 0.8), person_at(0.23, 0.8)], 700);
        assert_eq!(b, vec![a[1], a[0]]);
        assert_eq!(t.tracks().len(), 2);
    }

    #[test]
    fn tracker_follows_a_fast_mover_by_distance_when_boxes_no_longer_overlap() {
        let mut t = Tracker::default();
        let a = t.update(&[person_at(0.20, 0.8)], 0);
        // Moved 0.12 of the picture in a look: the 0.08 wide boxes do not overlap.
        let b = t.update(&[person_at(0.32, 0.8)], 1000);
        assert_eq!(a, b);
        // Far away is someone else.
        let c = t.update(&[person_at(0.90, 0.5)], 2000);
        assert_ne!(c[0], a[0]);
    }

    #[test]
    fn tracker_is_per_kind_and_remembers_briefly() {
        let mut t = Tracker::default();
        let p = t.update(&[det("person", 0.4, 0.4, 0.1, 0.3)], 0);
        // A car in the same place is not that person.
        let c = t.update(&[det("car", 0.4, 0.4, 0.1, 0.3)], 500);
        assert_ne!(p[0], c[0]);
        // A car then a truck in one place: one vehicle.
        let k = t.update(&[det("truck", 0.41, 0.4, 0.1, 0.3)], 1000);
        assert_eq!(c[0], k[0]);
        // Missed for a few seconds, then back: the same person.
        let back = t.update(&[det("person", 0.4, 0.4, 0.1, 0.3)], 4000);
        assert_eq!(back[0], p[0]);
        // Gone for longer than the memory: new track.
        let late = t.update(&[det("person", 0.4, 0.4, 0.1, 0.3)], 4000 + MEMORY_MS + 2000);
        assert_ne!(late[0], p[0]);
    }

    #[test]
    fn polygon_membership() {
        let sq: Vec<[f64; 2]> = SQUARE.to_vec();
        assert!(in_polygon(&sq, (0.6, 0.6)));
        assert!(!in_polygon(&sq, (0.3, 0.6)));
        assert!(!in_polygon(&sq, (0.6, 0.95)));
        // An L shape: the notch is outside.
        let l = vec![[0.0, 0.0], [0.5, 0.0], [0.5, 0.5], [1.0, 0.5], [1.0, 1.0], [0.0, 1.0]];
        assert!(in_polygon(&l, (0.25, 0.25)));
        assert!(in_polygon(&l, (0.75, 0.75)));
        assert!(!in_polygon(&l, (0.75, 0.25)));
        assert!(!in_polygon(&[[0.0, 0.0], [1.0, 1.0]], (0.5, 0.5)), "two points are no area");
    }

    #[test]
    fn loitering_fires_once_after_the_time_with_feet_in_the_area() {
        let z = vec![area("Driveway", 10, &SQUARE)];
        let mut w = ZoneWatch::default();
        // Head inside but feet below the area: not inside.
        assert!(w.step(&z, &[person_at(0.6, 0.95)], 0).is_empty());
        assert!(!w.someone_inside());
        let mut fired = Vec::new();
        for k in 0..30 {
            let now = 1000 + k * 700;
            for h in w.step(&z, &[person_at(0.6, 0.7)], now) {
                fired.push((now, h));
            }
            if k == 2 {
                assert!(w.waiting_inside(), "inside, the time not up");
            }
        }
        assert_eq!(fired.len(), 1, "once");
        let (at, h) = &fired[0];
        assert!(*at - 1000 >= 10_000 && *at - 1000 < 10_700, "fired at {at}");
        assert_eq!(h.tag(), "loitering: Driveway");
        assert!(matches!(h.kind, HitKind::Loiter { secs: 10 }));
        assert_eq!(w.occupied(), HashSet::from(["Driveway".to_string()]));
        assert!(w.someone_inside());
        assert!(!w.waiting_inside(), "reported: nothing left to wait for");
    }

    #[test]
    fn walking_through_does_not_loiter_and_leaving_clears_the_area() {
        let z = vec![area("Yard", 10, &SQUARE)];
        let mut w = ZoneWatch::default();
        // Crosses the area in about 5 s.
        for k in 0..9 {
            let x = 0.3 + 0.07 * k as f32;
            assert!(w.step(&z, &[person_at(x, 0.7)], k as i64 * 700).is_empty());
        }
        // Seen outside for longer than the grace: the timer is gone.
        for k in 0..6 {
            w.step(&z, &[person_at(0.93, 0.7)], 6300 + k * 700);
        }
        assert!(!w.someone_inside());
        // A short step out of the area keeps the time already spent.
        let mut w = ZoneWatch::default();
        let mut got = 0;
        for k in 0..20i64 {
            let fx = if k == 8 { 0.36 } else { 0.6 };
            got += w.step(&z, &[person_at(fx, 0.7)], k * 700).len();
        }
        assert_eq!(got, 1, "a step out for one look does not restart the timer");
    }

    #[test]
    fn loitering_labels_and_cooldown() {
        let mut z = vec![area("Yard", 2, &SQUARE)];
        let mut w = ZoneWatch::default();
        let car = det("car", 0.5, 0.6, 0.2, 0.2);
        for k in 0..10 {
            assert!(w.step(&z, &[car.clone()], k * 700).is_empty(), "cars are not wanted here");
        }
        z[0].labels = vec!["car".into()];
        let mut n = 0;
        for k in 10..20 {
            n += w.step(&z, &[car.clone()], k * 700).len();
        }
        assert_eq!(n, 1);
        // A person walking in right after: held back by the zone cooldown.
        let mut hits = 0;
        z[0].labels = vec!["person".into()];
        for k in 20..30 {
            hits += w.step(&z, &[person_at(0.6, 0.8)], k * 700).len();
        }
        assert_eq!(hits, 0);
        // Later it can fire again.
        for k in 0..10 {
            hits += w.step(&z, &[person_at(0.6, 0.8)], 40_000 + k * 700).len();
        }
        assert_eq!(hits, 1);
    }

    // The line goes left to right across the middle: A=(0.2,0.5) B=(0.8,0.5).
    // Walking along A->B, the right-hand side is down the picture.
    const HLINE: [[f64; 2]; 2] = [[0.2, 0.5], [0.8, 0.5]];

    #[test]
    fn crossing_direction_follows_the_arrow() {
        // Going down the picture = towards the right of A->B = "in".
        assert_eq!(crossing(&HLINE, (0.5, 0.4), (0.5, 0.6)), Some(true));
        assert_eq!(crossing(&HLINE, (0.5, 0.6), (0.5, 0.4)), Some(false));
        // Past the end of the line is no crossing.
        assert_eq!(crossing(&HLINE, (0.9, 0.4), (0.9, 0.6)), None);
        // Staying on one side.
        assert_eq!(crossing(&HLINE, (0.3, 0.6), (0.7, 0.7)), None);
        // Reversed line: reversed meaning.
        let rev = [HLINE[1], HLINE[0]];
        assert_eq!(crossing(&rev, (0.5, 0.4), (0.5, 0.6)), Some(false));
        // A bent line whose second part runs south: walking south, the right
        // hand is west, so east-ward is "out".
        let bent = [[0.2, 0.5], [0.5, 0.5], [0.5, 0.9]];
        assert_eq!(crossing(&bent, (0.4, 0.7), (0.6, 0.7)), Some(false));
        assert_eq!(crossing(&bent, (0.6, 0.7), (0.4, 0.7)), Some(true));
    }

    #[test]
    fn line_crossing_fires_in_the_chosen_direction_only() {
        let both = vec![line("Gate", "both", &HLINE)];
        let ab = vec![line("Gate", "ab", &HLINE)];
        let ba = vec![line("Gate", "ba", &HLINE)];
        let walk = |zones: &Vec<NamedZone>, ys: &[f32]| {
            let mut w = ZoneWatch::default();
            let mut out = Vec::new();
            for (k, y) in ys.iter().enumerate() {
                out.extend(w.step(zones, &[person_at(0.5, *y)], k as i64 * 700));
            }
            out
        };
        let down = [0.30, 0.40, 0.46, 0.55, 0.65, 0.75];
        let up = [0.75, 0.65, 0.55, 0.46, 0.40, 0.30];
        let h = walk(&both, &down);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].tag(), "crossed: Gate (in)");
        let h = walk(&both, &up);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].tag(), "crossed: Gate (out)");
        assert_eq!(walk(&ab, &down).len(), 1);
        assert!(walk(&ab, &up).is_empty());
        assert!(walk(&ba, &down).is_empty());
        assert_eq!(walk(&ba, &up).len(), 1);
    }

    #[test]
    fn jitter_at_the_line_is_not_a_crossing() {
        let z = vec![line("Gate", "both", &HLINE)];
        let mut w = ZoneWatch::default();
        let mut n = 0;
        // Feet wobble +-0.008 around the line (inside the margin).
        for k in 0..20 {
            let y = 0.5 + if k % 2 == 0 { 0.008 } else { -0.008 };
            n += w.step(&z, &[person_at(0.5, y)], k * 700).len();
        }
        assert_eq!(n, 0);
        // Starts clearly above, wobbles at the line, ends above: nothing.
        let mut w = ZoneWatch::default();
        for (k, y) in [0.40, 0.49, 0.51, 0.495, 0.505, 0.45].iter().enumerate() {
            n += w.step(&z, &[person_at(0.5, *y)], k as i64 * 700).len();
        }
        assert_eq!(n, 0);
        // Wobbles first, then really goes through: one crossing.
        let mut w = ZoneWatch::default();
        for (k, y) in [0.40, 0.49, 0.51, 0.49, 0.56, 0.7].iter().enumerate() {
            n += w.step(&z, &[person_at(0.5, *y)], k as i64 * 700).len();
        }
        assert_eq!(n, 1);
    }

    #[test]
    fn walking_around_the_end_of_a_line_is_not_a_crossing() {
        let z = vec![line("Gate", "both", &HLINE)];
        let mut w = ZoneWatch::default();
        let mut n = 0;
        // From above the line to below it, but past its end (x 0.9).
        for (k, (x, y)) in [(0.9, 0.3), (0.9, 0.45), (0.9, 0.55), (0.9, 0.7)].iter().enumerate() {
            n += w.step(&z, &[person_at(*x, *y)], k as i64 * 700).len();
        }
        assert_eq!(n, 0);
    }

    #[test]
    fn one_person_crossing_back_and_forth_fires_each_way() {
        let z = vec![line("Gate", "both", &HLINE)];
        let mut w = ZoneWatch::default();
        let mut tags = Vec::new();
        for (k, y) in [0.3, 0.4, 0.6, 0.7, 0.4, 0.3].iter().enumerate() {
            for h in w.step(&z, &[person_at(0.5, *y)], k as i64 * 700) {
                tags.push(h.tag());
            }
        }
        assert_eq!(tags, vec!["crossed: Gate (in)".to_string(), "crossed: Gate (out)".to_string()]);
    }

    #[test]
    fn parked_objects_are_skipped_by_zones_that_ignore_them() {
        let mut z = vec![area("Yard", 2, &SQUARE)];
        z[0].labels = vec!["car".into()];
        let car = det("car", 0.5, 0.6, 0.2, 0.2);
        let run = |zones: &Vec<NamedZone>, parked: bool| {
            let mut w = ZoneWatch::default();
            (0..10).map(|k| w.step_parked(zones, &[car.clone()], &[parked], k * 700).len()).sum::<usize>()
        };
        assert_eq!(run(&z, false), 1);
        assert_eq!(run(&z, true), 0, "parked, ignored");
        z[0].ignore_parked = false;
        assert_eq!(run(&z, true), 1, "the zone counts parked ones too");
    }

    #[test]
    fn aspect_ratio_makes_distances_equal_sideways_and_up() {
        // Feet 0.008 of the width from a vertical line: 0.014 of the height
        // on a 16:9 picture, outside the margin; unscaled it is inside.
        let vline: [[f64; 2]; 2] = [[0.5, 0.2], [0.5, 0.8]];
        let z = vec![line("Door", "both", &vline)];
        let go = |aspect: f64| {
            let mut w = ZoneWatch::default();
            w.tracker.set_aspect(aspect);
            let mut n = 0;
            for (k, x) in [0.40, 0.492, 0.508, 0.492, 0.40].iter().enumerate() {
                n += w.step(&z, &[person_at(*x, 0.6)], k as i64 * 700).len();
            }
            n
        };
        assert!(go(16.0 / 9.0) >= 1);
        assert_eq!(go(0.0), 0, "without the aspect the same move stays in the dead band");
    }

    #[test]
    fn zone_tags_are_recognised() {
        assert!(is_zone_tag("loitering: Driveway"));
        assert!(is_zone_tag("crossed: Gate (out)"));
        assert!(!is_zone_tag("person"));
        assert!(!is_zone_tag("package"));
    }

    #[test]
    fn zone_checks() {
        let ok = area("Yard", 10, &SQUARE);
        assert!(ok.check().is_ok());
        assert!(NamedZone { name: " ".into(), ..ok.clone() }.check().is_err());
        assert!(NamedZone { points: vec![[0.1, 0.1], [0.2, 0.2]], ..ok.clone() }.check().is_err(), "area needs 3 points");
        assert!(NamedZone { points: vec![[0.1, 0.1], [1.2, 0.2], [0.5, 0.5]], ..ok.clone() }.check().is_err());
        assert!(NamedZone { seconds: 0, ..ok.clone() }.check().is_err());
        assert!(NamedZone { labels: vec![], ..ok.clone() }.check().is_err());
        assert!(NamedZone { schedule: Some("late".into()), ..ok.clone() }.check().is_err());
        assert!(line("Gate", "both", &HLINE).check().is_ok());
        assert!(line("Gate", "sideways", &HLINE).check().is_err());
        assert!(crate::config::check_named_zones(&[ok.clone(), NamedZone { name: "yard".into(), ..ok.clone() }]).is_err(), "names must differ");
        assert!(crate::config::check_named_zones(&[ok, line("Gate", "ab", &HLINE)]).is_ok());
    }
}
