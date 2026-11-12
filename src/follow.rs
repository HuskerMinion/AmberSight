//! "Where did they go?": from a person or vehicle seen in one event, find
//! the same one on other cameras around that moment.
//!
//! It uses the fingerprints smart search already keeps for every object AI
//! found (a small picture of the person or vehicle, turned into 512
//! numbers by CLIP). Two pictures of the same person or vehicle get
//! fingerprints that are more alike than two different ones, but only
//! somewhat: CLIP was taught to tell what something is, not who it is, so
//! similar clothes or similar cars look alike, and a different angle,
//! light or distance pulls the same one apart. The score is a guide to
//! look at, not proof. It is raised when the two events show the same
//! recognized face (people) or the same plate (vehicles).

use crate::db::{Db, SmartRow};
use std::collections::{HashMap, HashSet};

/// Windows in seconds before and after the moment.
pub const DEFAULT_WINDOW_S: i64 = 600;
pub const MAX_WINDOW_S: i64 = 6 * 3600;
/// Matches scoring below this are not even returned.
pub const FLOOR: f32 = 0.60;
/// Where the results view starts hiding weaker matches (the slider can move it).
pub const DEFAULT_THRESHOLD: f32 = 0.80;
/// What the same recognized face / the same plate add to the likeness.
const FACE_BOOST: f32 = 0.12;
const PLATE_BOOST: f32 = 0.25;
/// Matches returned at most.
pub const MAX_RESULTS: usize = 80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Person,
    Vehicle,
}

/// Person or vehicle, from AI's label (None: not followed).
pub fn kind_of(label: &str) -> Option<Kind> {
    match label.to_ascii_lowercase().as_str() {
        "person" | "people" | "pedestrian" => Some(Kind::Person),
        "car" | "truck" | "van" | "suv" | "bus" | "vehicle" | "motorcycle" | "motorbike" | "bicycle" | "bike" => Some(Kind::Vehicle),
        _ => None,
    }
}

/// One person or vehicle in one picture of an event.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Sighting {
    pub event_id: String,
    pub camera: String,
    pub at_ms: i64,
    /// The picture (GET /api/smart/pic/:pic).
    pub pic: i64,
    pub r#box: Option<Vec<f32>>,
    pub label: String,
    /// How alike the pictures look (cosine, 0..1 in practice).
    pub sim: f32,
    /// Added for the same recognized face or the same plate.
    pub boost: f32,
    /// sim + boost, at most 1: what results are ranked and cut by.
    pub score: f32,
    pub face: bool,
    pub plate: bool,
    /// The one the search started from.
    pub source: bool,
}

/// A fingerprint in the window, before it is scored.
#[derive(Clone, Debug)]
pub struct Cand {
    pub event_id: String,
    pub camera: String,
    pub at_ms: i64,
    pub pic: i64,
    pub bx: Option<Vec<f32>>,
    pub label: String,
    pub sim: f32,
}

/// The likeness plus what a matching face / plate adds: (boost, score).
pub fn boosted(sim: f32, face: bool, plate: bool) -> (f32, f32) {
    let boost = if plate { PLATE_BOOST } else { 0.0 } + if face { FACE_BOOST } else { 0.0 };
    (boost, (sim + boost).min(1.0))
}

/// The best picture per event, those scoring at least `floor`, best first
/// (at most `limit`). `faces` / `plates` are the events showing the same
/// recognized face / plate as the start.
pub fn rank(cands: Vec<Cand>, faces: &HashSet<String>, plates: &HashSet<String>, floor: f32, limit: usize) -> Vec<Sighting> {
    let mut best: HashMap<String, Sighting> = HashMap::new();
    for c in cands {
        let (face, plate) = (faces.contains(&c.event_id), plates.contains(&c.event_id));
        let (boost, score) = boosted(c.sim, face, plate);
        if score < floor || best.get(&c.event_id).is_some_and(|b| b.score >= score) {
            continue;
        }
        best.insert(
            c.event_id.clone(),
            Sighting { event_id: c.event_id, camera: c.camera, at_ms: c.at_ms, pic: c.pic, r#box: c.bx, label: c.label, sim: c.sim, boost, score, face, plate, source: false },
        );
    }
    let mut out: Vec<Sighting> = best.into_values().collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.at_ms.cmp(&b.at_ms)).then_with(|| a.event_id.cmp(&b.event_id)));
    out.truncate(limit);
    out
}

/// The path: the start and the matches in time order (the start first when
/// at the same time).
pub fn order_path(source: Sighting, found: Vec<Sighting>) -> Vec<Sighting> {
    let mut all = found;
    all.push(source);
    all.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then(b.source.cmp(&a.source)).then_with(|| a.event_id.cmp(&b.event_id)));
    all
}

pub struct Query {
    pub event_id: String,
    /// The object to follow (a fingerprint id); none: the largest person or vehicle.
    pub pic: Option<i64>,
    pub before_ms: i64,
    pub after_ms: i64,
    pub same_camera: bool,
    pub floor: f32,
    pub limit: usize,
}

#[derive(Debug, PartialEq)]
pub enum Error {
    /// No such event or object (or the caller may not see its camera).
    NotFound,
    /// The event has no fingerprinted person or vehicle.
    NoObject,
    Db(String),
}

pub struct Outcome {
    pub source: Sighting,
    /// The other people / vehicles in the start event that could be followed.
    pub others: Vec<Sighting>,
    /// Matches, time ordered, the start included.
    pub path: Vec<Sighting>,
    pub scanned: usize,
}

fn row_box(r: &SmartRow) -> Option<Vec<f32>> {
    r.r#box.as_deref().and_then(|b| serde_json::from_str::<Vec<f32>>(b).ok()).filter(|b| b.len() == 4)
}

fn area(b: &Option<Vec<f32>>) -> f32 {
    b.as_ref().map_or(0.0, |b| b[2] * b[3])
}

fn as_sighting(r: &SmartRow, source: bool) -> Sighting {
    Sighting {
        event_id: r.event_id.clone(),
        camera: r.camera.clone(),
        at_ms: r.at_ms,
        pic: r.id,
        r#box: row_box(r),
        label: r.label.clone().unwrap_or_default(),
        sim: 1.0,
        boost: 0.0,
        score: 1.0,
        face: false,
        plate: false,
        source,
    }
}

/// Plate texts that are the same plate, allowing one misread character.
pub fn same_plate(a: &str, b: &str) -> bool {
    a.chars().count() >= 4 && crate::lpr::matches(a, b, if a.chars().count() >= 5 { 1 } else { 0 }, true).is_some()
}

fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

/// Follow a person or vehicle from an event. `allowed` says which cameras
/// the caller may see: nothing from other cameras is ever returned.
pub fn follow(db: &Db, q: &Query, allowed: &dyn Fn(&str) -> bool) -> Result<Outcome, Error> {
    let dberr = |e: anyhow::Error| Error::Db(format!("{e:#}"));
    let rows = db.smart_event_rows(&q.event_id).map_err(dberr)?;
    if rows.is_empty() || rows.iter().any(|r| !allowed(&r.camera)) {
        return Err(Error::NotFound);
    }
    // The followable objects of the event, the largest first.
    let mut objs: Vec<&SmartRow> = rows.iter().filter(|r| r.label.as_deref().and_then(kind_of).is_some() && row_box(r).is_some()).collect();
    objs.sort_by(|a, b| area(&row_box(b)).total_cmp(&area(&row_box(a))).then(a.at_ms.cmp(&b.at_ms)));
    let src = match q.pic {
        Some(p) => {
            if !rows.iter().any(|r| r.id == p) {
                return Err(Error::NotFound);
            }
            *objs.iter().find(|r| r.id == p).ok_or(Error::NoObject)?
        }
        None => *objs.first().ok_or(Error::NoObject)?,
    };
    let kind = src.label.as_deref().and_then(kind_of).ok_or(Error::NoObject)?;
    let sv = unit(crate::smart::unpack(&src.emb));
    let (from, to) = (src.at_ms - q.before_ms, src.at_ms + q.after_ms);

    let mut cands = Vec::new();
    let mut scanned = 0usize;
    db.smart_scan(from, to, &mut |id, ev, cam, at, bx, label, emb| {
        let Some(label) = label else { return };
        if ev == q.event_id || kind_of(label) != Some(kind) || (!q.same_camera && cam == src.camera) || !allowed(cam) {
            return;
        }
        scanned += 1;
        let sim = crate::smart::score(emb, &sv);
        // Even the largest boost could not lift it to the floor.
        if sim + FACE_BOOST + PLATE_BOOST < q.floor {
            return;
        }
        cands.push(Cand {
            event_id: ev.to_string(),
            camera: cam.to_string(),
            at_ms: at,
            pic: id,
            bx: bx.and_then(|b| serde_json::from_str::<Vec<f32>>(b).ok()).filter(|b| b.len() == 4),
            label: label.to_string(),
            sim,
        });
    })
    .map_err(dberr)?;

    // Events showing the same recognized face / plate.
    let (mut faces, mut plates) = (HashSet::new(), HashSet::new());
    if !cands.is_empty() {
        match kind {
            Kind::Person => {
                let people = db.people_in_events_between(from, to).map_err(dberr)?;
                let mine: HashSet<i64> = people.iter().filter(|(e, _)| *e == q.event_id).map(|(_, p)| *p).collect();
                faces = people.into_iter().filter(|(e, p)| *e != q.event_id && mine.contains(p)).map(|(e, _)| e).collect();
            }
            Kind::Vehicle => {
                let read = db.plates_in_events_between(from, to).map_err(dberr)?;
                let mine: Vec<&String> = read.iter().filter(|(e, _)| *e == q.event_id).map(|(_, t)| t).collect();
                plates = read.iter().filter(|(e, t)| *e != q.event_id && mine.iter().any(|m| same_plate(m, t))).map(|(e, _)| e.clone()).collect();
            }
        }
    }
    let found = rank(cands, &faces, &plates, q.floor, q.limit);
    let source = as_sighting(src, true);
    let others = objs.iter().filter(|r| r.id != src.id).take(8).map(|r| as_sighting(r, false)).collect();
    Ok(Outcome { path: order_path(source.clone(), found), source, others, scanned })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_vec(seed: f32) -> Vec<f32> {
        unit((0..crate::smart::DIM).map(|i| (i as f32 * seed).sin()).collect())
    }
    /// A fingerprint like `base` with some of `other` mixed in.
    fn blend(base: &[f32], other: &[f32], w: f32) -> Vec<f32> {
        unit(base.iter().zip(other).map(|(a, b)| a * (1.0 - w) + b * w).collect())
    }
    fn cand(ev: &str, cam: &str, at: i64, sim: f32) -> Cand {
        Cand { event_id: ev.into(), camera: cam.into(), at_ms: at, pic: at, bx: None, label: "person".into(), sim }
    }

    #[test]
    fn similarity_is_one_for_the_same_and_lower_for_others() {
        let (a, b) = (unit_vec(0.37), unit_vec(0.11));
        let (sa, sb) = (crate::smart::to_bytes(&a), crate::smart::to_bytes(&b));
        assert!((crate::smart::similarity(&sa, &sa) - 1.0).abs() < 0.02);
        let exact: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((crate::smart::similarity(&sa, &sb) - exact).abs() < 0.03);
        assert_eq!(crate::smart::similarity(&sa, &[]), 0.0);
    }

    #[test]
    fn labels_group_into_people_and_vehicles() {
        assert_eq!(kind_of("Person"), Some(Kind::Person));
        assert_eq!(kind_of("truck"), Some(Kind::Vehicle));
        assert_eq!(kind_of("dog"), None);
        assert_eq!(kind_of("backpack"), None);
    }

    #[test]
    fn a_matching_face_or_plate_lifts_the_score_but_not_past_one() {
        assert_eq!(boosted(0.7, false, false), (0.0, 0.7));
        let (b, s) = boosted(0.7, true, false);
        assert!((b - FACE_BOOST).abs() < 1e-6 && (s - (0.7 + FACE_BOOST)).abs() < 1e-6);
        assert!(boosted(0.7, false, true).1 > boosted(0.7, true, false).1);
        assert_eq!(boosted(0.95, true, true).1, 1.0);
    }

    #[test]
    fn ranking_keeps_the_best_per_event_and_drops_weak_ones() {
        let faces: HashSet<String> = ["e3".to_string()].into();
        let r = rank(
            vec![cand("e1", "A", 10, 0.7), cand("e1", "A", 20, 0.9), cand("e2", "B", 30, 0.5), cand("e3", "C", 40, 0.62), cand("e4", "D", 50, 0.85)],
            &faces,
            &HashSet::new(),
            0.6,
            10,
        );
        let ids: Vec<&str> = r.iter().map(|s| s.event_id.as_str()).collect();
        // e2 is below the floor; e3 is lifted by its face; e1 counts once with its best.
        assert_eq!(ids, vec!["e1", "e4", "e3"]);
        assert_eq!(r.iter().find(|s| s.event_id == "e1").unwrap().at_ms, 20);
        assert!(r.iter().find(|s| s.event_id == "e3").unwrap().face);
        assert!(r.iter().all(|s| s.event_id != "e2"));
        assert_eq!(rank(vec![cand("e1", "A", 1, 0.9), cand("e2", "A", 2, 0.8)], &HashSet::new(), &HashSet::new(), 0.6, 1).len(), 1);
    }

    #[test]
    fn the_path_is_in_time_order_with_the_start_marked() {
        let s = |ev: &str, at: i64, source: bool| Sighting {
            event_id: ev.into(),
            camera: "A".into(),
            at_ms: at,
            pic: at,
            r#box: None,
            label: "person".into(),
            sim: 0.9,
            boost: 0.0,
            score: 0.9,
            face: false,
            plate: false,
            source,
        };
        let p = order_path(s("src", 100, true), vec![s("late", 300, false), s("early", 50, false), s("same", 100, false)]);
        let ids: Vec<&str> = p.iter().map(|x| x.event_id.as_str()).collect();
        assert_eq!(ids, vec!["early", "src", "same", "late"]);
        assert!(p[1].source);
    }

    #[test]
    fn plate_misreads_of_one_character_still_match() {
        assert!(same_plate("ABC123", "ABC123"));
        assert!(same_plate("ABC123", "ABC1Z3"));
        assert!(!same_plate("ABC123", "XYZ789"));
        assert!(!same_plate("AB", "AB"));
    }

    fn put(db: &Db, ev: &str, cam: &str, at: i64, label: &str, emb: &[f32], bx: &str) {
        db.smart_store(ev, &[(cam.to_string(), at, format!("{ev}.jpg"), Some(bx.to_string()), Some(label.to_string()), crate::smart::to_bytes(emb))]).unwrap();
    }

    #[test]
    fn follow_finds_the_look_alike_on_other_cameras_in_the_window_and_respects_access() {
        let db = Db::open_in_memory().unwrap();
        let (me, other) = (unit_vec(0.37), unit_vec(0.9));
        put(&db, "start", "Front", 1_000_000, "person", &me, "[0.1,0.1,0.3,0.6]");
        put(&db, "same-side", "Side", 1_100_000, "person", &blend(&me, &other, 0.1), "[0.1,0.1,0.2,0.5]");
        put(&db, "same-back", "Back", 1_300_000, "person", &blend(&me, &other, 0.15), "[0.1,0.1,0.2,0.5]");
        put(&db, "stranger", "Side", 1_150_000, "person", &other, "[0.1,0.1,0.2,0.5]");
        put(&db, "late", "Back", 1_000_000 + 3_000_000, "person", &me, "[0.1,0.1,0.2,0.5]");
        put(&db, "a-car", "Side", 1_120_000, "car", &me, "[0.1,0.1,0.2,0.5]");
        put(&db, "same-cam", "Front", 1_050_000, "person", &me, "[0.1,0.1,0.2,0.5]");
        let q = Query { event_id: "start".into(), pic: None, before_ms: 600_000, after_ms: 600_000, same_camera: false, floor: 0.8, limit: 20 };
        let all = |_: &str| true;
        let r = follow(&db, &q, &all).unwrap();
        let ids: Vec<&str> = r.path.iter().map(|s| s.event_id.as_str()).collect();
        // Time ordered; the start is in it; the stranger, the car, the same camera and the one outside the window are not.
        assert_eq!(ids, vec!["start", "same-side", "same-back"]);
        assert!(r.path[0].source && !r.path[1].source);
        assert!(r.path[1].score > r.path[2].score);
        // A viewer who may not see "Side" never gets it, and cannot start from it.
        let no_side = |c: &str| c != "Side";
        let r = follow(&db, &q, &no_side).unwrap();
        assert_eq!(r.path.iter().map(|s| s.event_id.as_str()).collect::<Vec<_>>(), vec!["start", "same-back"]);
        assert_eq!(follow(&db, &Query { event_id: "same-side".into(), ..q_like(&q) }, &no_side).err(), Some(Error::NotFound));
        // Nobody to follow in an event with only a dog.
        put(&db, "dogday", "Front", 2_000_000, "dog", &me, "[0.1,0.1,0.2,0.5]");
        assert_eq!(follow(&db, &Query { event_id: "dogday".into(), ..q_like(&q) }, &all).err(), Some(Error::NoObject));
        assert_eq!(follow(&db, &Query { event_id: "nothing".into(), ..q_like(&q) }, &all).err(), Some(Error::NotFound));
        // The same camera too, when asked.
        let r = follow(&db, &Query { same_camera: true, ..q_like(&q) }, &all).unwrap();
        assert!(r.path.iter().any(|s| s.event_id == "same-cam"));
    }

    fn q_like(q: &Query) -> Query {
        Query { event_id: q.event_id.clone(), pic: q.pic, before_ms: q.before_ms, after_ms: q.after_ms, same_camera: q.same_camera, floor: q.floor, limit: q.limit }
    }

    #[test]
    fn a_recognized_face_pulls_in_a_weaker_look() {
        let db = Db::open_in_memory().unwrap();
        let (me, other) = (unit_vec(0.37), unit_vec(0.9));
        put(&db, "start", "Front", 1_000_000, "person", &me, "[0.1,0.1,0.3,0.6]");
        put(&db, "hat", "Side", 1_100_000, "person", &blend(&me, &other, 0.5), "[0.1,0.1,0.2,0.5]");
        let q = Query { event_id: "start".into(), pic: None, before_ms: 600_000, after_ms: 600_000, same_camera: false, floor: 0.8, limit: 20 };
        let none = follow(&db, &q, &|_: &str| true).unwrap();
        assert_eq!(none.path.len(), 1);
        let pid = db.person_id("Sam", 1).unwrap();
        for ev in ["start", "hat"] {
            db.insert_face(ev, "x", 1_000_000, 0.9, 80, &[0u8; 8], &[], Some((pid, 0.9))).unwrap();
        }
        let lifted = follow(&db, &q, &|_: &str| true).unwrap();
        assert_eq!(lifted.path.len(), 2);
        assert!(lifted.path[1].face && lifted.path[1].boost > 0.0);
    }
}
