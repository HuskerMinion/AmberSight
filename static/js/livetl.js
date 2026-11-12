// livetl.js - timeline mode on the Live page.
//
// The grid keeps its cameras/layout/page; every tile plays its recording at one shared
// playhead and a strip docked below the grid shows recorded spans and alert markers.
// Clicking a tile makes that camera fill the grid area (strip shows only its data).
//
// Tiles play /api/playback windows (plain MP4 URLs, the session cookie authenticates).
// Each tile owns two <video> elements: the next window is loaded into the hidden one
// before the current one ends, so playback continues without a gap.

import { playVideo, $, state, api, bus, enc, esc, ic, camFromId, toast, fmtWall, pad2, SPEEDS, REVERSE_SPEEDS, MAX_RATE, speedLabel, speedOptions, safeRate, lowQ, dataSaver, bigQ, overlayOpen, backLayer, overlaysChanged } from './core.js';
import { isActive } from './nav.js';
import * as Live from './live.js';
import { tvOn, tvOne, enterTv, exitTv, setOneQuiet, setEscFirst } from './fullscreen.js';
import { showPreview, showPreviewFrame, hidePreview } from './preview.js';
import { exportDialog } from './export.js';
import { createZoom } from './zoom.js';
import { openCalendar, closeCalendar } from './calendar.js';

const WIN = 5 * 60000;           // playback window per request
const FIRST_WIN = 60000;         // the first window after a click: fewer recordings to open, so it starts sooner
const MIN_VIEW = 10000;          // closest zoom (10 s)
const MAX_VIEW = 7 * 86400000;   // furthest zoom
const LEAD = 2000;               // land this long before an alert
const CLUSTER_PX = 34;

const S = {
  on: false, inited: false,
  shown: [], solo: null,
  view: { from: 0, to: 0 },
  playhead: 0, playing: true, speed: 1, anchorHead: 0, anchorWall: 0, scrubbing: false,
  // At the live edge: the cameras' live streams show, the playhead stays at now.
  live: false,
  // Show what the AI saw (boxes on the video); remembered per browser.
  boxes: (() => { try { return localStorage.getItem('ambs.tlBoxes') === '1'; } catch (e) { return false; } })(),
  fromFull: null, seq: 0, fetchTimer: 0, stepBusy: false, groups: [], noteTimer: 0, lastSec: -1, headVisible: true,
};
const D = { range: null, spans: {}, ev: [], sig: [] };
const SIG_MAX = 8;                // signal lanes shown at most
const SIG_LANE = 14;              // px per lane
const LIVE_EDGE_MS = 1500;       // this close to now is live
const LIVE_POLL_MS = 5000;       // new recordings while live
const LIVE_GROW_MS = 120000;     // a span ending this recently runs on to now while live
const players = new Set();

// ---------- alert kinds ----------
const VEH = new Set(['car', 'truck', 'bus', 'motorcycle', 'bicycle', 'vehicle', 'van', 'bike', 'motorbike', 'suv']);
const ANI = new Set(['dog', 'cat', 'bird', 'animal', 'pet', 'horse', 'cow', 'sheep', 'bear', 'fox', 'deer', 'squirrel', 'rabbit', 'raccoon', 'coyote']);
// (face and package have no sprite icon in the page: drawn inline, see kIcon.)
const KINDS = {
  face: { ic: 'face', label: 'Face', rank: 6 },
  package: { ic: 'package', label: 'Package', rank: 5 },
  person: { ic: 'person', label: 'Person', rank: 4 },
  vehicle: { ic: 'car', label: 'Vehicle', rank: 3 },
  animal: { ic: 'paw', label: 'Animal', rank: 2 },
  other: { ic: 'alert', label: 'Alert', rank: 1 },
};
const KIND_ORDER = ['person', 'vehicle', 'animal', 'face', 'package', 'other'];
const SVG_ICONS = {
  face: '<circle cx="12" cy="12" r="9"/><path d="M8.5 14.2c.9 1.1 2 1.6 3.5 1.6s2.6-.5 3.5-1.6"/><path d="M9 9.5v.5M15 9.5v.5"/>',
  package: '<path d="M21 8 12 3 3 8v8l9 5 9-5z"/><path d="M3 8l9 5 9-5M12 13v8"/><path d="M7.5 5.5l9 5"/>',
};
function kIcon(k) {
  const n = KINDS[k].ic;
  return SVG_ICONS[n] ? '<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' + SVG_ICONS[n] + '</svg>' : ic(n);
}
// Alert type filter (empty = show every type); affects markers and prev/next.
const kindsOn = new Set();
const passes = (e) => !kindsOn.size || kindsOf(e).some(k => kindsOn.has(k));

// Every kind an alert is (a recognized face is also a person), best first.
function kindsOf(e) {
  const o = (e.objects || []).map(x => String(x).toLowerCase());
  const out = [];
  if (o.some(x => x.startsWith('package'))) out.push('package');
  if (o.some(x => x === 'face' || x === 'faces' || x === 'unknown face' || x.endsWith(' face'))) out.push('face');
  if (o.some(x => x === 'person' || x === 'people' || x === 'human' || x === 'pedestrian')) out.push('person');
  if (o.some(x => VEH.has(x))) out.push('vehicle');
  if (o.some(x => ANI.has(x))) out.push('animal');
  if (!out.length) out.push('other');
  return out;
}
function kindOf(e) { return kindsOf(e).reduce((a, b) => (KINDS[b].rank > KINDS[a].rank ? b : a)); }

// ---------- data ----------
// (The page's camera list can be empty for a moment while it is refreshed;
// the names from before stand in, so marks and recordings do not vanish.)
let lastNames = [];
const shownNames = () => {
  const n = S.shown.map(id => { const c = camFromId(id); return c ? c.name : null; }).filter(Boolean);
  if (n.length || !S.shown.length) lastNames = n;
  return n.length ? n : lastNames;
};
const landing = (e) => e.start * 1000 - LEAD;

function mergeSpans(list) {
  list.sort((a, b) => a[0] - b[0]);
  const out = [];
  for (const [a, b] of list) {
    const l = out[out.length - 1];
    if (l && a - l[1] <= 1500) { if (b > l[1]) l[1] = b; } else out.push([a, b]);
  }
  return out;
}
// Whether camera `name` is recording now, its last saved recording ending at
// `end`: online and recording all the time (then the recording being written
// is the gap since), or recording on motion and saved moments ago.
function recordingNow(name, end, now) {
  const c = (state.cams || []).find(x => x.name === name);
  if (!c || !c.online || !c.record || c.enabled === false) return false;
  const always = !c.record_motion_only || c.record_sub;
  return now - end < (always ? LIVE_GROW_MS : 20000);
}
// A camera's recorded spans; while live, one that ended moments ago runs on
// to now (the recording being written is not listed until it closes).
function spansOf(name) {
  const sp = D.spans[name] || [];
  if (!S.live || !sp.length) return sp;
  const now = Date.now(), last = sp[sp.length - 1];
  return recordingNow(name, last[1], now) ? sp.slice(0, -1).concat([[last[0], now]]) : sp;
}
function spanAt(name, ms) { return spansOf(name).find(s => ms >= s[0] - 1500 && ms <= s[1] + 1500) || null; }
// The next stretch of recording starting after `ms`, if any.
function spanAfter(name, ms) { return spansOf(name).filter(s => s[0] > ms).sort((a, b) => a[0] - b[0])[0] || null; }
function recorded(name, ms) {
  const r = D.range;
  if (!r || ms < r[0] || ms > r[1]) return true;       // unknown: try
  return !!spanAt(name, ms);
}

// Seconds per slot the server groups alerts into for this view (about 8 px of
// strip; marks closer than CLUSTER_PX merge anyway); 0 = every alert, for
// views up to a couple of hours.
function wantBucket() {
  const W = ($('tlArea') && $('tlArea').clientWidth) || 800;
  const s = Math.floor((S.view.to - S.view.from) / W * 8 / 1000);
  return s < 60 ? 0 : s;
}

async function loadData() {
  const seq = ++S.seq;
  const bucket = wantBucket();
  const w = S.view.to - S.view.from;
  const a = Math.max(0, Math.floor(S.view.from - w)), b = Math.ceil(S.view.to + w);
  const asked = shownNames();
  // What this load covers (so moving the view inside it asks for nothing
  // more while it is under way); D.range: what the recordings loaded cover.
  D.asked = [a, b];
  D.bucket = bucket;
  S.loadingSince = Date.now();
  setLoading(true);
  // Each answer is used as soon as it comes (recordings can take longer than
  // the alerts); a newer load makes this one's late answers stale.
  const fresh = () => seq === S.seq && S.on;
  const rec = api('/api/recordings?from=' + a + '&to=' + b).then(rec => {
    if (!fresh()) return;
    D.range = [a, b];
    D.spans = {};
    for (const r of (rec.cameras || [])) D.spans[r.camera] = r.spans || [];
    draw();
  });
  // The shown cameras' alert marks (compact: a zoomed-out view of busy
  // cameras holds thousands).
  const ev = api('/api/timeline/events?since=' + Math.floor(a / 1000) + '&until=' + Math.ceil(b / 1000) + '&cameras=' + encodeURIComponent(asked.join(',')) + '&bucket=' + bucket).then(ev => {
    if (!fresh()) return;
    const evCams = ev.cameras || [];
    // (n: how many alerts a zoomed-out slot stands for.)
    D.ev = (ev.rows || []).map(r => ({ id: r[0], camera: evCams[r[1]], start: r[2], end: r[3], objects: r[4] || [], thumb: r[5] ? r[0] : null, n: r[6] || 1 }));
    D.evCams = new Set(asked);
    // (Cameras shown since the request went out: load theirs too.)
    if (shownNames().some(n => !D.evCams.has(n))) scheduleFetch(true);
    draw();
  });
  // (Lanes for doors, alarms and camera inputs; a window too long for them
  // just shows none.)
  const sig = api('/api/signals?from=' + a + '&to=' + b).then(sig => {
    if (!fresh()) return;
    D.sig = (sig.signals || []).filter(x => x.on || x.spans.length).slice(0, SIG_MAX);
    draw();
  });
  await Promise.allSettled([rec, ev, sig]);
  if (seq === S.seq) { S.loadingSince = 0; setLoading(false); }
}
function setLoading(on) { const a = $('tlArea'); if (a) a.classList.toggle('loading', !!on); }
// A refresh on a timer waits while a load is still under way (it would
// make that load's answers stale before they come), unless it is stuck.
function pollData() {
  if (S.loadingSince && Date.now() - S.loadingSince < 20000) return;
  loadData();
}
// Debounced: only fetch when the view has left the loaded range (or it is far too wide).
function scheduleFetch(force) {
  clearTimeout(S.fetchTimer);
  S.fetchTimer = setTimeout(() => {
    const r = D.asked || D.range, v = S.view;
    // (Also when zoomed in far enough that the slots alerts are grouped into
    // are too coarse, or out so far that every alert is too many.)
    const b = wantBucket(), have = D.bucket || 0;
    const regroup = b < have / 2 || (have === 0 ? b > 0 : b > have * 4);
    if (force || !r || regroup || v.from < r[0] || v.to > r[1] || (r[1] - r[0]) > 8 * (v.to - v.from)) loadData();
  }, 300);
}

// ---------- filmstrips ----------
// Small pictures of each shown camera across the strip (one grid image per
// camera from /api/filmstrip), so dragging along the timeline shows the
// moment at once: in the bubble and on the tiles while the video catches up.
const STRIP_STEPS = [2e3, 5e3, 10e3, 15e3, 30e3, 60e3, 120e3, 300e3, 600e3, 900e3, 1800e3, 3600e3, 7200e3];
const STRIP_MAX = 160;
const strips = new Map();   // camera name -> { key, step, times, img, cols, w, h }
const stripBusy = new Set();
const stripWant = new Map();   // camera name -> the key the view wants now
const stripFailed = new Map(); // camera name + key -> retry after (ms)
let stripTimer = 0;
function wantStrips() { clearTimeout(stripTimer); stripTimer = setTimeout(loadStrips, 300); }
function loadStrips() {
  if (!S.on) return;
  const W = ($('tlArea') && $('tlArea').clientWidth) || 800;
  const span = S.view.to - S.view.from;
  // About one picture every 8 px of strip.
  const step = STRIP_STEPS.find(s => s >= span / Math.max(20, W / 8)) || STRIP_STEPS[STRIP_STEPS.length - 1];
  // Whole blocks of pictures (so small moves of the strip reuse what is
  // there), covering the view with a margin each side.
  const block = step * 40;
  const from = Math.floor((S.view.from - span * 0.25) / block) * block;
  const n = Math.min(STRIP_MAX, Math.ceil((S.view.to + span * 0.25 - from) / step));
  for (const name of shownNames()) {
    const c = state.cams.find(x => x.name === name);
    if (!c || c.remote) continue;
    const key = from + '/' + step + '/' + n;
    stripWant.set(name, key);
    const cur = strips.get(name);
    if ((cur && cur.key === key) || stripBusy.has(name + key) || (stripFailed.get(name + key) || 0) > Date.now()) continue;
    fetchStrip(name, key, from, step, n);
  }
}
async function fetchStrip(name, key, from, step, n) {
  stripBusy.add(name + key);
  let objUrl = null;
  try {
    const url = '/api/filmstrip?camera=' + enc(name) + '&from=' + from + '&step=' + step + '&n=' + n + '&w=' + (dataSaver() ? 112 : 160) + '&quiet=1';
    const r = await fetch(url, { credentials: 'same-origin' });
    // None (nothing recorded) or busy: do not ask again for a while.
    if (!r.ok || r.status === 204) { stripFailed.set(name + key, Date.now() + (r.status === 503 ? 5000 : 30000)); return; }
    const times = (r.headers.get('x-sprite-times') || '').split(',').map(Number).filter(x => x > 0);
    const cols = +r.headers.get('x-sprite-cols') || 1, w = +r.headers.get('x-sprite-w') || 160, h = +r.headers.get('x-sprite-h') || 90;
    const img = new Image();
    objUrl = URL.createObjectURL(await r.blob());
    img.src = objUrl;
    await img.decode();
    // Left the timeline, or the view moved on while this was made.
    if (!S.on || stripWant.get(name) !== key) return;
    const old = strips.get(name);
    if (old && old.img) URL.revokeObjectURL(old.img.src);
    strips.set(name, { key, step, times, img, cols, w, h });
    objUrl = null;
    players.forEach(p => { if (p.cam.name === name) tickPlayer(p); });
  } catch (e) {
    stripFailed.set(name + key, Date.now() + 30000);   // no pictures: the video still loads
  } finally {
    if (objUrl) URL.revokeObjectURL(objUrl);
    stripBusy.delete(name + key);
  }
}
// The filmstrip picture for camera `name` at `ms`, or null.
function stripFrame(name, ms) {
  const s = strips.get(name);
  if (!s || !s.times.length) return null;
  let lo = 0, hi = s.times.length - 1, i = -1;
  while (lo <= hi) { const m = (lo + hi) >> 1; if (s.times[m] <= ms) { i = m; lo = m + 1; } else hi = m - 1; }
  if (i < 0 || ms - s.times[i] > s.step * 3) return null;   // nothing recorded near then
  return { img: s.img, sx: (i % s.cols) * s.w, sy: Math.floor(i / s.cols) * s.h, w: s.w, h: s.h, t: s.times[i], i };
}
// Show the filmstrip picture on a tile (true if there was one).
function drawStrip(p, t) {
  const f = stripFrame(p.cam.name, t);
  if (!f) { p.cv.hidden = true; return false; }
  const key = p.cam.name + '|' + strips.get(p.cam.name).key + '|' + f.i;
  if (p.cvKey !== key) {
    if (p.cv.width !== f.w || p.cv.height !== f.h) { p.cv.width = f.w; p.cv.height = f.h; }
    p.cv.getContext('2d').drawImage(f.img, f.sx, f.sy, f.w, f.h, 0, 0, f.w, f.h);
    p.cvKey = key;
  }
  p.cv.hidden = false;
  return true;
}

// ---------- tile players ----------
// Above 16x the timeline plays a keyframe-only clip made for that speed (the
// server spaces and retimes the keyframes so it plays at normal rate): k = speed.
// A tile the device cannot keep up with plays such a clip at lower speeds too
// (its tier is 'key', see planTiers).
// (Kept while paused or dragging along the strip: the tile stays as it is
// until playback goes on; planTiers decides again then.)
const keyOk = () => !S.live && S.speed >= 2 && S.speed < MAX_RATE && Number.isInteger(S.speed);
// Keyframes only as single pictures (WebCodecs): at 1x too, for a camera
// whose one recording the device or network cannot keep up with.
const keyTierOk = () => keyOk() || (PIC_OK && S.speed === 1 && !S.live);
const tileK = (p) => (S.speed > MAX_RATE ? S.speed : p.tier === 'key' && keyOk() ? S.speed : 1);

// ---------- what each tile plays: quality within the device's budget ----------
// Each tile plays one of: 'main' (the best recording), 'sub' (the sub stream,
// where one was recorded) or 'key' (keyframes only, retimed by the server:
// a few pictures a second, little to download or decode). A tile asks for
// the main picture only when it is big enough on screen that the sub stream
// would look soft, and not at speeds where detail goes by too fast to see.
// All tiles together stay within what the device can decode and download
// (pixels a second, bits a second): over it, tiles step down, the main
// picture to the sub stream first (it keeps moving), then to keyframes.
// A tile that still falls behind (stalls, lags, drops frames) steps down
// for a while by itself, and back up carefully.
const SOFT = 1.5;               // the sub stream enlarged more than this looks soft
const UP_MAX_SPEED = 4;         // the main picture up to this speed in the grid...
const UP_MAX_SPEED_SOLO = 8;    // ...and on the one-camera view
const KEY_MIN_MS = 333;         // keyframes only below 16x in the grid: at most 3 pictures a second
// What each camera's streams are like, learned from what played (until then
// a common camera: 2560x1440 main, 640x360 sub). hasSub: whether a sub
// stream was recorded (null: not known yet).
const CAMQ = new Map();
function camQ(name) {
  let c = CAMQ.get(name);
  if (!c) {
    // (gop: ms of recording between keyframes; keyBits: one keyframe's size.)
    c = { main: { w: 2560, h: 1440, fps: 15, bps: 4e6, gop: 2000, keyBits: 2.2e6 }, sub: { w: 640, h: 360, fps: 10, bps: 6e5, gop: 2000, keyBits: 1.4e5 }, hasSub: null };
    CAMQ.set(name, c);
  }
  return c;
}
// The device: a phone or tablet decodes and downloads far less than a desktop.
const DEV = (() => {
  const mm = (q) => { try { return matchMedia(q).matches; } catch (e) { return false; } };
  const touch = mm('(pointer: coarse)') && !mm('(any-pointer: fine)');
  const uaMobile = !!(navigator.userAgentData && navigator.userAgentData.mobile) || /Android|iPhone|iPad|iPod|Mobile/i.test(navigator.userAgent || '');
  const mobile = touch || uaMobile;
  const cores = Math.max(1, navigator.hardwareConcurrency || 4);
  const mem = navigator.deviceMemory || (mobile ? 4 : 8);
  // Decoded pixels a second (a 1440p camera at 15 fps is 55 M).
  let px = mobile ? Math.min(240, 30 * cores) : Math.min(1500, 100 * cores);
  if (mem <= 2) px *= 0.5; else if (mem <= 4 && mobile) px *= 0.75;
  return { mobile, cores, mem, px: px * 1e6, bw: (mobile ? 40 : 400) * 1e6 };
})();
// The whole grid's state: scale < 1 after several tiles fell behind at once
// (the network or the device as a whole is the limit, not one tile).
// Above 1 after a long while of every tile keeping up while some had to
// step down for the budget (the device does more than its class suggests).
const G = { scale: 1, scaleAt: 0, upAt: 0, probe: 20000, limited: false, planAt: 0, sig: '', budget: null };
const MAX_SCALE = DEV.mobile ? 2 : 1.5;
// (Not navigator.connection.downlink: a rough guess, often far too low
// at first; what the tiles manage to play says more, see watchHealth.)
const bwBudget = () => DEV.bw * G.scale;
// Does this tile show the picture so big that the sub stream looks soft?
function needsMain(p, c) {
  if (p.el.classList.contains('zoomed')) return true;   // zoomed in: detail wanted
  const w = p.el.clientWidth, h = p.el.clientHeight;
  if (!w || !h) return false;
  const ar = c.main.w / c.main.h || 16 / 9;
  const shownH = Math.min(h, w / ar) * Math.min(3, window.devicePixelRatio || 1);
  return shownH > SOFT * c.sub.h;
}
// Pixels and bits a second this tile costs at a tier, at speed sp.
function tierCost(p, tier, sp) {
  const c = camQ(p.cam.name);
  const st = tier === 'main' || c.hasSub === false ? c.main : c.sub;
  if (tier === 'key') {
    const fps = Math.min(1000 / (PIC_OK ? keyGap(p) : keyFrameMs()), sp * 1000 / st.gop);
    return { px: st.w * st.h * fps, bw: st.keyBits * fps };
  }
  return { px: st.w * st.h * st.fps * sp, bw: st.bps * sp };
}
// Decide every tile's tier. Cheap; runs with the 250 ms tick (at most every
// 400 ms unless something the user did changed it).
function planTiers() {
  const now = performance.now();
  const list = [...players].filter(p => !p.cam.remote);
  // (Dragging along the strip: every tile stays as it is meanwhile.)
  if (S.scrubbing) return;
  const sig = [S.speed, S.playing, S.solo, S.live, list.length, window.innerWidth, window.innerHeight].join('|');
  const changed = sig !== G.sig;
  if (!changed && now - G.planAt < 400) return;
  if (changed) G.sigAt = now;
  G.sig = sig; G.planAt = now;
  const sp = S.playing ? Math.max(1, Math.abs(S.speed)) : 1;
  // Several tiles fell behind at about the same time: the whole grid steps
  // down (the network or the device is the limit), not each tile.
  const recent = list.filter(p => p.hl && p.hl.struggleAt && now - p.hl.struggleAt < 3000);
  const fresh = recent.filter(p => !p.hl.used && now - p.hl.struggleAt > 800);
  if (fresh.length && recent.length >= Math.max(2, Math.ceil(0.4 * list.length)) && now - G.scaleAt > 4000) {
    // (Soon after a step up: the next one waits longer.)
    if (now - G.upAt < 30000) G.probe = Math.min(160000, G.probe * 2);
    G.scale = Math.max(0.15, G.scale * 0.7); G.scaleAt = now;
    for (const p of recent) p.hl.used = true;
  } else {
    for (const p of fresh) { const h = p.hl; h.used = true; h.pen = Math.min(2, h.pen + 1); h.penUntil = now + h.hold; }
  }
  // Nothing fell behind for a while: the whole grid steps back up, a little
  // (past 1 only while the budget holds tiles down).
  if (now - G.scaleAt > G.probe && !list.some(p => p.hl && p.hl.struggleAt && now - p.hl.struggleAt < G.probe) && (G.scale < 1 || (G.limited && G.scale < MAX_SCALE))) {
    G.scale = Math.min(G.scale < 1 ? 1 : MAX_SCALE, G.scale * 1.25); G.scaleAt = G.upAt = now;
  }
  const kOk = keyTierOk();
  for (const p of list) {
    const c = camQ(p.cam.name);
    const solo = S.solo !== null;
    // (The sound plays only from video, up to 4x: the camera heard stays video.)
    const heard = SND.on && solo && String(p.cam.id) === String(SND.id) && soundSpeedOk();
    const ladder = (c.hasSub === false ? ['main'] : ['main', 'sub']).concat(kOk && !heard ? ['key'] : []);
    let lv = 0;
    if (c.hasSub !== false && (dataSaver() || sp > (solo ? UP_MAX_SPEED_SOLO : UP_MAX_SPEED) || !needsMain(p, c))) lv = 1;
    const h = p.hl;
    if (h && h.pen && now > h.penUntil) {
      // Kept up for a while: one step back up (a step that fails again soon
      // is held longer next time).
      h.pen--; h.upAt = now; h.penUntil = now + h.hold;
    }
    if (h && h.hold > 15000 && now - (h.struggleAt || 0) > 300000) h.hold = 15000;
    p.ladder = ladder;
    p.lv = Math.min(ladder.length - 1, lv + (h ? h.pen : 0));
  }
  const bpx = DEV.px * G.scale, bbw = bwBudget();
  let px = 0, bw = 0;
  const sum = () => { px = 0; bw = 0; for (const p of list) { const k = tierCost(p, p.ladder[p.lv], sp); px += k.px; bw += k.bw; } };
  sum();
  G.limited = false;
  for (let guard = 0; guard < 64 && (px > bpx || bw > bbw); guard++) {
    // The step that saves the most; stepping down to keyframes only when no
    // tile can go to its sub stream instead.
    let best = null, bestGain = 0, bestRank = 9;
    for (const p of list) {
      if (p.lv >= p.ladder.length - 1) continue;
      const a = tierCost(p, p.ladder[p.lv], sp), b = tierCost(p, p.ladder[p.lv + 1], sp);
      const gain = (a.px - b.px) / bpx + (a.bw - b.bw) / bbw;
      const rank = p.ladder[p.lv + 1] === 'key' ? 1 : 0;
      if (gain > 0 && (rank < bestRank || (rank === bestRank && gain > bestGain))) { best = p; bestGain = gain; bestRank = rank; }
    }
    if (!best) break;
    best.lv++; G.limited = true;
    sum();
  }
  G.budget = { px: Math.round(px / 1e6), bw: Math.round(bw / 1e6), maxPx: Math.round(bpx / 1e6), maxBw: Math.round(bbw / 1e6) };
  for (const p of list) {
    const want = p.ladder[p.lv];
    const order = ['main', 'sub', 'key'];
    // Down at once; up only once it has been wanted for 2 s (or the user
    // changed something), so a tile does not flip back and forth.
    if (want === p.tier) { p.upSince = 0; continue; }
    const up = p.tier && order.indexOf(want) < order.indexOf(p.tier);
    if (up && !changed) {
      if (!p.upSince) p.upSince = now;
      if (now - p.upSince < 2000) continue;
    }
    p.upSince = 0;
    p.tier = want;
  }
}
// How long each picture of a keyframes-only clip shows below 16x.
const keyFrameMs = () => (S.solo !== null ? trickFrameMs() : Math.max(KEY_MIN_MS, trickFrameMs()));
// The pause between pictures asked for: below 16x (keyframes only, the
// device or network short) no more often than a fast clip would show them,
// less often still while the whole grid is held down.
const keyGap = (p) => (Math.abs(S.speed) >= MAX_RATE ? picGap() : Math.max(picGap(), keyFrameMs()) / Math.min(1, G.scale));
// The quality a tile's video asks for.
const tierQ = (p) => (p.tier === 'main' && !dataSaver() ? 'full' : 'low');

// How a tile keeps up, checked over 2 s stretches of playing: falling behind
// the playhead (the video slips more than its slack, waits for data, or is
// still seeking) for over a quarter of the time. Reported to planTiers.
const hlOf = (p) => p.hl || (p.hl = { gen: -1, t0: 0, last: 0, bad: 0, w0: 0, q0: null, ct0: 0, pen: 0, penUntil: 0, hold: 15000, upAt: 0, struggleAt: 0, used: true, rate: 0 });
function struggled(h, now, why) { h.why = why + ' ' + (h.why || '').slice(0, 40); h.struggleAt = now; h.used = false; if (now - h.upAt < 30000) h.hold = Math.min(120000, h.hold * 2); }
// A tile with nothing to show while playing on (its next stretch did not
// come in time): after 1.5 s, that counts as falling behind too. (Not soon
// after a jump, a speed change or the start, when every tile loads at once.)
function watchWait(p, waiting) {
  const now = performance.now();
  if (!waiting || !S.playing || S.scrubbing || document.hidden || now - (S.jumpAt || 0) < 4000 || now - (G.sigAt || 0) < 4000 || !recorded(p.cam.name, S.playhead)) { p.waitFrom = 0; return; }
  if (!p.waitFrom) p.waitFrom = now;
  if (now - p.waitFrom > 1500) { struggled(hlOf(p), now, 'wait'); p.waitFrom = now; }
}
function watchHealth(p, s, lag, slack) {
  const h = hlOf(p);
  const now = performance.now(), v = s.v;
  const pq = v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality() : null;
  const start = () => { h.t0 = h.w0 = h.last = now; h.bad = 0; h.q0 = pq; h.ct0 = v.currentTime; };
  if (!S.playing || S.scrubbing || document.hidden || h.gen !== s.gen || h.k !== s.k || h.speed !== S.speed) {
    h.gen = s.gen; h.k = s.k; h.speed = S.speed; start(); h.t0 = now;
    return;
  }
  const dt = Math.min(1000, now - h.last);
  h.last = now;
  // (The first 4 s after a start, a seek or a speed change: settling.)
  if (now - h.t0 < 4000) { h.w0 = now; h.q0 = pq; h.ct0 = v.currentTime; h.bad = 0; return; }
  if (v.readyState < 3 || v.seeking || lag > slack) h.bad += dt;
  const win = now - h.w0;
  if (win < 2000) return;
  // (Not dropped frames: browsers drop them by design at high rates and
  // while the page is busy, with the picture still keeping up. Only how
  // many frames a second the recording has is learned here.)
  if (pq && h.q0 && s.k === 1 && h.bad === 0) {
    const tot = pq.totalVideoFrames - h.q0.totalVideoFrames, media = v.currentTime - h.ct0;
    const st = camQ(p.cam.name)[s.sub ? 'sub' : 'main'];
    if (media > 1 && tot > 5) st.fps = Math.max(1, Math.min(60, 0.7 * st.fps + 0.3 * tot / media));
    h.rate = Math.round(1000 * tot / win);
  }
  if (h.bad > 0.25 * win) struggled(h, now, 'lag');
  h.w0 = now; h.q0 = pq; h.ct0 = v.currentTime; h.bad = 0;
}
// Backwards: pieces of REV_WIN made reversed by the server, played forwards.
const rev = () => S.speed < 0;
const REV_WIN = 8000;
const TRICK_WALL_MS = 30000;   // each fast clip covers 30 s of watching
const TRICK_FIRST_MS = 6000;   // the first one less, so it starts sooner
// How long each fast picture shows. One camera: 10 a second. Many: fewer,
// so the recordings' disk keeps up (each picture comes from its own
// recording file: 16 cameras at 10 a second is more than a hard disk reads).
// Above 128x pictures are spaced further still (each is a file to read).
function trickFrameMs() {
  const over = Math.max(1, Math.abs(S.speed) / 128);
  if (S.solo !== null) return Math.round(Math.min(1000, 100 * over));
  let n = 0;
  for (const p of players) if (!p.cam.remote) n++;
  return Math.round(Math.min(1000, Math.max(100, Math.min(500, 30 * n)) * over));
}

// Fast playback (16x and up, either way): the keyframe at the playhead,
// one picture at a time, each decoded in the browser as it arrives; the
// next is asked for at once. A slow disk means fewer pictures, never a
// blank tile: the last one stays until the next is there (as Blue Iris).
const PIC_OK = typeof window.VideoDecoder === 'function' && typeof window.EncodedVideoChunk === 'function';
// Fast backwards: only as single pictures (made by the browser).
const FAST_REVERSE = [-128, -64, -16];
// (Also a tile on keyframes only below 16x, see planTiers: the first picture
// is there as soon as it has come, where a fast clip shows nothing until
// the browser has read about 20 of them, and asking for the next one only
// when the last has come keeps to what the network carries.)
const picMode = (p) => PIC_OK && !p.picFail && (Math.abs(S.speed) >= MAX_RATE || (p.tier === 'key' && keyTierOk()));

function picState(p) {
  if (p.pic) return p.pic;
  const cv = document.createElement('canvas');
  cv.className = 'tlpic'; cv.hidden = true;
  p.el.insertBefore(cv, p.msg);
  // have/wall: the picture on screen (when it was recorded); shownFor: the
  // moment it was asked for. seq/pending: the picture being decoded.
  // decFails: decoding failures in a row; netFails/retryAt: failed asks in
  // a row (server restarting, network) and when to try again.
  p.pic = {
    cv, dec: null, cfg: '', busy: false, ac: null, have: null, wall: null, shownFor: null, drawn: false, lastAsk: 0, askedAt: null, lat: 250,
    decFails: 0, netFails: 0, retryAt: 0, none: false, seq: 0, pending: null, lastT: null, lastTick: 0, lastPlaying: false, lastSpeed: 1,
  };
  return p.pic;
}

function stopPic(p) {
  const pc = p.pic;
  if (!pc) return;
  if (pc.ac) { try { pc.ac.abort(); } catch (e) { /* ignore */ } }
  if (pc.dec) { try { pc.dec.close(); } catch (e) { /* ignore */ } }
  pc.cv.remove();
  p.pic = null;
}

// The playhead jumped (a click on the timeline, a jump button): the picture
// still on its way is for the old moment, so it is dropped, and so is the
// one on screen (it would show the wrong time until the new one comes).
function picJumped(pc) {
  if (pc.ac) { try { pc.ac.abort(); } catch (e) { /* ignore */ } }
  pc.ac = null; pc.busy = false; pc.pending = null;
  pc.have = null; pc.shownFor = null; pc.askedAt = null; pc.lastAsk = 0;
}

// Pause between asks per tile: one camera as fast as pictures come; in a
// grid less often each, so the recordings' disk keeps up with them all.
function picGap() {
  if (S.solo !== null) return 50;
  let n = 0;
  for (const p of players) if (!p.cam.remote) n++;
  return Math.min(400, Math.max(66, 25 * n));
}

async function askPic(p, at) {
  const pc = p.pic;
  pc.busy = true; pc.lastAsk = Date.now(); pc.askedAt = at;
  const ac = pc.ac = new AbortController();
  const t0 = performance.now();
  const low = S.solo === null || dataSaver();
  const url = '/api/playback/picture?camera=' + enc(p.cam.name) + '&at=' + Math.floor(at) + (low ? '&quality=low' : '') + (pc.have !== null ? '&have=' + pc.have : '') + '&quiet=1';
  // A failed ask (the server restarting after an update, the network gone):
  // try again after 1 s, then 2, 4, up to 8 s. Fast pictures stay on.
  const later = () => { pc.netFails++; pc.retryAt = Date.now() + Math.min(8000, 1000 * 2 ** (pc.netFails - 1)); };
  let decoding = false;
  try {
    const r = await fetch(url, { credentials: 'same-origin', signal: ac.signal });
    if (p.pic !== pc || pc.ac !== ac) return;
    if (r.status === 401) { bus.emit('unauth'); later(); return; }
    // 204: the picture on screen is still the right one.
    if (r.status === 204 && r.headers.get('x-none') === '1') { pc.none = true; pc.netFails = 0; return; }
    if (r.status === 204) { pc.none = false; pc.netFails = 0; if (pc.drawn && pc.have !== null) pc.shownFor = at; return; }
    if (r.status === 404) { pc.none = true; pc.netFails = 0; return; }
    if (!r.ok) { later(); return; }
    const wall = parseInt(r.headers.get('x-pic-ms'), 10);
    const codec = r.headers.get('x-codec') || '';
    const desc = r.headers.get('x-desc') || '';
    const sz = /^(\d+)x(\d+)$/.exec(r.headers.get('x-pic-size') || '');
    const size = sz ? [+sz[1], +sz[2]] : null;
    const data = await r.arrayBuffer();
    if (p.pic !== pc || pc.ac !== ac) return;
    pc.netFails = 0;
    const cq = camQ(p.cam.name), psub = r.headers.get('x-pic-sub') === '1';
    if (low || psub) cq.hasSub = psub;
    const kq = cq[psub ? 'sub' : 'main'];
    kq.keyBits = 0.7 * kq.keyBits + 0.3 * data.byteLength * 8;
    decoding = true;
    const cfg = codec + '|' + desc;
    if (cfg !== pc.cfg || !pc.dec || pc.dec.state === 'closed') {
      const config = { codec, description: Uint8Array.from(atob(desc), c => c.charCodeAt(0)), optimizeForLatency: true };
      const sup = await VideoDecoder.isConfigSupported(config).catch(() => ({ supported: false }));
      if (p.pic !== pc) return;
      // The browser cannot decode this camera's pictures by itself (e.g.
      // H.265 without the codec): fast clips it is, for this tile.
      if (!sup.supported) { p.picFail = true; stopPic(p); tickPlayer(p); return; }
      if (pc.dec) { try { pc.dec.close(); } catch (e) { /* ignore */ } }
      pc.dec = new VideoDecoder({
        output: (frame) => {
          // Only the picture still wanted (not one asked for before a jump).
          const want = pc.pending;
          if (p.pic === pc && want && frame.timestamp === want.seq) {
            const cv = pc.cv;
            // Some H.265 decoders (Edge) report a wrong picture size (1280x720
            // of a 4608x1728 picture), which would show only its top-left
            // corner, enlarged: the size as recorded is used instead.
            let pic = frame, fixed = null;
            const sz = want.size;
            if (sz && (frame.visibleRect.width !== sz[0] || frame.visibleRect.height !== sz[1]) && sz[0] <= frame.codedWidth && sz[1] <= frame.codedHeight) {
              try { fixed = pic = new VideoFrame(frame, { visibleRect: { x: 0, y: 0, width: sz[0], height: sz[1] }, displayWidth: sz[0], displayHeight: sz[1] }); } catch (e) { pic = frame; }
            }
            if (cv.width !== pic.displayWidth || cv.height !== pic.displayHeight) { cv.width = pic.displayWidth; cv.height = pic.displayHeight; }
            cv.getContext('2d').drawImage(pic, 0, 0, cv.width, cv.height);
            if (fixed) fixed.close();
            pc.drawn = true; pc.pending = null;
            pc.have = want.wall; pc.wall = want.wall; pc.shownFor = want.at;
          }
          frame.close();
        },
        error: () => { pc.cfg = ''; },
      });
      pc.dec.configure(config);
      pc.cfg = cfg;
    }
    if (pc.ac !== ac) return;
    const seq = ++pc.seq;
    pc.pending = { seq, wall, at, size };
    pc.dec.decode(new EncodedVideoChunk({ type: 'key', timestamp: seq, data }));
    await pc.dec.flush();
    if (p.pic !== pc) return;
    if (pc.pending && pc.pending.seq === seq) {
      // Decoded, but no picture came out: counts as a decoding failure.
      pc.pending = null;
      throw new Error('no picture');
    }
    pc.none = false; pc.decFails = 0;
  } catch (e) {
    if (p.pic !== pc || ac.signal.aborted) return;
    if (!decoding) { later(); return; }
    pc.cfg = '';
    // The browser keeps failing to decode this camera's pictures: fast
    // clips it is, for this tile.
    if (++pc.decFails >= 5) { p.picFail = true; stopPic(p); }
  } finally {
    if (p.pic === pc && pc.ac === ac) {
      pc.ac = null;
      pc.busy = false;
      // How far behind the playhead a picture arrives (asked that far ahead).
      pc.lat = Math.min(3000, 0.7 * pc.lat + 0.3 * (performance.now() - t0));
    }
    if (p.pic === pc) tickPlayer(p);
  }
}

function tickPic(p) {
  for (const s of p.slots) { if (s.ready || s.loading) resetSlot(s); s.v.hidden = true; }
  p.shownIdx = -1;
  const pc = picState(p);
  const t = S.playhead, now = Date.now(), tick = performance.now();
  const playing = S.playing && !S.scrubbing;
  // Did the playhead jump (not just move on with playback)? While playing
  // it moves about speed x time; a second of watching off from that, or
  // any move while paused (other than dragging along the timeline), is a jump.
  if (pc.lastT !== null && !S.scrubbing) {
    const dt = tick - pc.lastTick;
    const expect = playing && pc.lastPlaying ? dt * S.speed : 0;
    const fastest = Math.max(Math.abs(S.speed), Math.abs(pc.lastSpeed));
    const allow = playing || pc.lastPlaying ? Math.max(2000, fastest * (1000 + dt)) : 500;
    if (Math.abs(t - pc.lastT - expect) > allow) picJumped(pc);
  }
  pc.lastT = t; pc.lastTick = tick; pc.lastPlaying = playing; pc.lastSpeed = S.speed;
  // Ask ahead by the time a picture takes to arrive.
  const at = playing ? t + S.speed * pc.lat : t;
  const rec = recorded(p.cam.name, at);
  if (!pc.busy && rec && now >= pc.retryAt && now - pc.lastAsk >= (playing ? keyGap(p) : 0) && (playing || pc.askedAt !== at)) askPic(p, at);
  // The last picture stays while the next comes, and through short breaks
  // in the recording: up to about 5 s of watching from the moment it was
  // asked for (at 128x pictures come a couple of times a second).
  const hold = 5000 * Math.max(1, Math.abs(S.speed));
  const show = pc.drawn && pc.shownFor !== null && !pc.none && Math.abs(pc.shownFor - t) < hold;
  pc.cv.hidden = !show;
  if (show) { p.cv.hidden = true; p.msg.hidden = true; p.waitSince = 0; return; }
  let pic = false;
  if (rec) pic = drawStrip(p, t); else p.cv.hidden = true;
  const text = rec && !pc.none ? waitText(p) : (p.waitSince = 0, 'No recording');
  if (p.msg.textContent !== text) p.msg.textContent = text;
  p.msg.classList.toggle('wait', text !== 'No recording');
  p.msg.hidden = pic;
}

// A tile waiting for its picture: "Loading…", and after about a second the
// moment it waits for ("Buffering 14:03:22").
function waitText(p) {
  const now = Date.now();
  if (!p.waitSince) p.waitSince = now;
  if (now - p.waitSince < 1000) return 'Loading…';
  const d = new Date(S.playhead);
  return 'Buffering ' + pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
}

function resetSlot(s) {
  // A request still on its way is dropped (the server then stops making it).
  if (s.abort) { try { s.abort.abort(); } catch (e) { /* ignore */ } s.abort = null; }
  s.gen++; s.ready = false; s.loading = false; s.start = s.end = 0; s.up = false; s.sub = false;
  s.v.pause(); s.v.removeAttribute('src'); try { s.v.load(); } catch (e) { /* ignore */ }
}

function mountPlayer(frame, cam) {
  const el = document.createElement('div');
  el.className = 'tlp';
  el.innerHTML = '<video muted playsinline preload="auto" hidden></video><video muted playsinline preload="auto" hidden></video><canvas class="tlstrip" hidden></canvas><div class="tlnone"></div>';
  // (Silent by the property too: the attribute alone is not honored by every browser.)
  for (const v of el.querySelectorAll('video')) v.muted = true;
  const vids = el.querySelectorAll('video');
  // Corner pills as on live tiles: the name top left, the moment played bottom right.
  const pills = document.createElement('div');
  pills.className = 'tlpills';
  pills.innerHTML = '<div class="ptop"><div class="pill ptl"><span class="nm"></span></div></div><div class="pbot"><div class="pill pbr"></div></div>';
  pills.querySelector('.nm').textContent = Live.camLabel(cam);
  frame.appendChild(pills);
  const p = {
    cam, el, tm: pills.querySelector('.pbr'), msg: el.querySelector('.tlnone'), cv: el.querySelector('.tlstrip'), cvKey: '', cur: 0, failUntil: 0, shownIdx: -1,
    slots: [...vids].map(v => ({ v, start: 0, end: 0, ready: false, loading: false, since: 0, gen: 0, k: 1 })),
  };
  // Scroll to zoom, drag to pan, double-click: as in the full view, when
  // one camera is shown (in the grid the wheel scrolls the page).
  createZoom(el, { box: true, on: () => S.solo !== null });
  players.add(p);
  tickPlayer(p);
  return { el, stop() { pills.remove(); players.delete(p); stopLive(p); stopPic(p); if (p.bx) p.bx.remove(); p.slots.forEach(resetSlot); } };
}

// q: 'low' = the sub stream where it was recorded (starts fast), 'full' =
// the best recording. up: a full-quality copy loading behind the one playing.
// twall (fast playback): ms of watching the clip covers.
async function load(p, si, fromMs, q, up, win, twall) {
  fromMs = Math.floor(fromMs);
  const s = p.slots[si];
  const gen = ++s.gen;
  if (s.abort) { try { s.abort.abort(); } catch (e) { /* ignore */ } s.abort = null; }
  s.ready = false; s.loading = true; s.since = Date.now(); s.start = s.end = 0;
  s.q = q || 'full'; s.up = !!up; s.sub = false;
  const k = s.k = tileK(p);
  s.rev = rev();
  const fm = s.fm = k > 1 ? (S.speed > MAX_RATE ? trickFrameMs() : keyFrameMs()) : 0;
  let to = fromMs + (k > 1 ? k * (twall || (win ? TRICK_FIRST_MS : TRICK_WALL_MS)) : (win || WIN));
  if (s.rev) {
    // Backwards: the piece that ends at `fromMs`.
    to = fromMs;
    const sp = spanAt(p.cam.name, to - 1);
    fromMs = Math.max(to - REV_WIN, sp ? Math.floor(sp[0]) : to - REV_WIN);
  } else if (k === 1) {
    // (A fast clip runs on across gaps: the server holds the last picture
    // through one, so short breaks in the recording cost no new request.)
    const sp = spanAt(p.cam.name, fromMs);
    if (sp) to = Math.min(to, Math.floor(sp[1]));
    // Almost at the end of a stretch (a short break follows, e.g. a
    // restart): the next stretch instead, ready when the playhead gets
    // there (the last picture stays meanwhile), not a failure.
    if (to - fromMs < 1500 && !win) {
      const nx = spanAfter(p.cam.name, fromMs);
      if (nx && nx[0] - fromMs < 60000 * Math.max(1, Math.abs(S.speed))) {
        fromMs = Math.floor(nx[0]);
        to = Math.min(fromMs + WIN, Math.floor(nx[1]));
      }
    }
  }
  s.reqFrom = fromMs; s.reqTo = to;
  const fail = () => {
    if (gen !== s.gen) return;
    s.loading = false;
    // A copy in another quality that could not load: keep playing what plays.
    if (s.up) { if (s.q === 'low') p.noDownTo = fromMs + 60000; else p.noUpTo = fromMs + 60000; resetSlot(s); } else p.failUntil = Date.now() + 3000;
  };
  if (to - fromMs < 1500) return fail();
  const url = '/api/playback?camera=' + enc(p.cam.name) + '&from=' + fromMs + '&to=' + to + (s.rev ? '&reverse=1' : (s.q === 'low' || dataSaver() ? '&quality=low' : '') + (k > 1 ? '&speed=' + k + '&frame_ms=' + fm : ''));
  let r;
  // A ranged 1-byte probe returns where the clip really starts (x-clip-start-ms).
  const ac = s.abort = new AbortController();
  try { r = await fetch(url + '&quiet=1', { headers: { Range: 'bytes=0-0' }, credentials: 'same-origin', signal: ac.signal }); } catch (e) { return fail(); }
  if (s.abort === ac) s.abort = null;
  if (gen !== s.gen) return;
  if (r.status === 401) { bus.emit('unauth'); return fail(); }
  if (!r.ok || r.status === 204) return fail();
  s.start = parseInt(r.headers.get('x-clip-start-ms'), 10) || fromMs;
  s.end = parseInt(r.headers.get('x-clip-end-ms'), 10) || to;
  s.sub = r.headers.get('x-clip-sub') === '1';
  try { if (r.body) r.body.cancel(); } catch (e) { /* ignore */ }
  // What this camera's streams are like: whether a sub stream was recorded,
  // and how many bits a second each takes (the clip's size over its length).
  const cq = camQ(p.cam.name);
  if (s.q === 'low' || dataSaver() || s.sub) cq.hasSub = s.sub;
  const size = parseInt((/\/(\d+)$/.exec(r.headers.get('content-range') || '') || [])[1], 10);
  const st = cq[s.sub ? 'sub' : 'main'];
  if (k === 1 && !s.rev && size > 0 && s.end - s.start > 5000) st.bps = Math.max(1e4, size * 8000 / (s.end - s.start));
  // A fast clip: how big its keyframes are, and how far apart (when they
  // came further apart than asked for, that is the camera's spacing).
  const nf = parseInt(r.headers.get('x-clip-frames'), 10);
  if (k > 1 && size > 0 && nf > 1) {
    st.keyBits = size * 8 / nf;
    const per = (s.end - s.start) / nf;
    st.gop = per > 1.3 * k * fm ? per : Math.min(st.gop, k * fm);
  }
  // Nothing better than the sub stream here (it is all that was recorded):
  // stay on what is playing until this window is over. (And a lower one
  // asked for where only the main stream was recorded: the same.)
  if (s.up && s.q === 'full' && s.sub) { p.noUpTo = s.end; s.loading = false; resetSlot(s); return; }
  if (s.up && s.q === 'low' && !s.sub) { p.noDownTo = s.end; s.loading = false; resetSlot(s); return; }
  const v = s.v;
  await new Promise((res) => {
    v.addEventListener('loadedmetadata', res, { once: true });
    v.addEventListener('error', res, { once: true });
    v.src = url;
  });
  if (gen !== s.gen) return;
  if (v.error) return fail();
  if (v.videoWidth && v.videoHeight) { st.w = v.videoWidth; st.h = v.videoHeight; }
  s.ready = true; s.loading = false;
  tickPlayer(p);
}

function tickPlayer(p) { tickVideo(p); applySound(p); paintTime(p); }
// The moment this tile plays (the date too when it is not today).
function paintTime(p) {
  const d = new Date(S.playhead), n = new Date();
  const day = d.toDateString() === n.toDateString() ? '' : d.toLocaleDateString([], { month: 'short', day: 'numeric' }) + ' ';
  const text = day + pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
  if (p.tm.textContent !== text) p.tm.textContent = text;
}
function tickVideo(p) {
  // Another server's camera: its recordings are there, not here.
  if (p.cam.remote) {
    const text = 'Recordings are on ' + ((p.cam.remote && p.cam.remote.site) || 'the other server');
    if (p.msg.textContent !== text) p.msg.textContent = text;
    p.msg.classList.remove('wait'); p.msg.hidden = false; p.cv.hidden = true;
    return;
  }
  if (S.live && !S.scrubbing) { stopPic(p); showLive(p); return; }
  if (p.live) stopLive(p);
  if (picMode(p)) { tickPic(p); return; }
  const t = S.playhead, now = Date.now();
  let cs = p.slots[p.cur], os = p.slots[1 - p.cur];
  for (const s of p.slots) if (s.loading && now - s.since > 20000) resetSlot(s);   // stuck request
  if (!p.tier) p.tier = 'sub';
  const k = tileK(p);
  const back = rev();
  for (const s of p.slots) if ((s.ready || s.loading) && (s.k !== k || !!s.rev !== back)) resetSlot(s);   // speed crossed 16x, to/from keyframes only, or direction changed
  // (Backwards a piece covers (start, end]; its time 0 is `end`.)
  const covers = (s) => s.ready && (back ? t > s.start + 30 && t <= s.end : t >= s.start && t < s.end - 30);
  const wantOf = (s) => (back ? (s.end - t) / 1000 : (t - s.start) / 1000 / s.k);
  // A load for a moment the user has since clicked away from: drop it now
  // (waiting for it would delay the new one by its whole load time).
  // (The next window loads ahead of the playhead: up to `lead` before it
  // is still wanted.)
  const lead = Math.max(2000, 30000 * Math.abs(S.speed));
  for (const s of p.slots) if (s.loading && !(back ? t > s.reqFrom - 2000 && t <= s.reqTo + lead : t >= s.reqFrom - lead && t < s.reqTo)) resetSlot(s);
  // The full-quality copy has caught up (showing the playhead's moment):
  // swap it in, no gap.
  const caughtUp = (s) => covers(s) && !s.v.seeking && s.v.readyState >= 3 && Math.abs(s.v.currentTime - wantOf(s)) < 0.5;
  const swap = () => { resetSlot(cs); p.cur = 1 - p.cur; [cs, os] = [os, cs]; cs.up = false; p.shownIdx = -1; };
  if (os.up && !S.scrubbing && caughtUp(os)) swap();
  let use = -1;
  if (covers(cs)) use = p.cur;
  else if (covers(os)) { swap(); use = p.cur; }
  // (A preload may start after a short break in the recording.)
  const gapOk = Math.max(1000, cs.k === 1 ? 60000 * Math.max(1, Math.abs(S.speed)) : 1000);
  if (use >= 0 && os.ready && !os.up && !(back ? os.end >= cs.start - 1000 && os.start < cs.start : os.start >= cs.end - 1000 - (cs.end - cs.start) && os.start <= cs.end + gapOk && os.end > cs.end)) resetSlot(os);   // stale preload
  if (use >= 0 && os.up && (os.ready || os.loading) && !(t >= os.start - 2000)) resetSlot(os);   // upgrade for a moment left behind
  watchWait(p, use < 0);
  if (use < 0) {
    // The video on screen stays (frozen) while the next one loads when it
    // shows about this moment, e.g. at the end of a window: no black, no
    // flicker back to the small stand-in picture.
    const shown = p.shownIdx >= 0 ? p.slots[p.shownIdx] : null;
    const shownAt = shown && (shown.rev ? shown.end - shown.v.currentTime * 1000 : shown.start + shown.v.currentTime * 1000 * shown.k);
    // (Fast: the last picture holds for up to ~5 s of watching.)
    const hold = shown && shown.k > 1 ? 5000 * Math.abs(S.speed) : 3000 * Math.max(1, Math.abs(S.speed));
    // A window loaded for the stretch after a break, starting a little
    // ahead: kept, it plays when the playhead gets there.
    const ahead = (s) => !back && !S.scrubbing && s.ready && s.start > t && s.start - t < 60000 * Math.max(1, Math.abs(S.speed));
    // In a break in the recording with the next stretch already waiting,
    // the last picture stays up until it starts (it showed "Loading…" after
    // 3 s; some cameras' recordings have breaks of a few seconds).
    const bridging = shown && !back && p.slots.some(s => s !== shown && ahead(s)) && t > shownAt;
    const near = shown && shown.ready && !S.scrubbing && (Math.abs(shownAt - t) < hold || bridging);
    for (const s of p.slots) if (s.ready && !(near && s === shown) && !ahead(s)) resetSlot(s);
    const waiting = p.slots.some(ahead);
    const into = near ? p.slots.indexOf(shown) === 0 ? 1 : 0 : p.cur;
    const target = p.slots[into];
    // Start on the sub stream (small, starts at once); the full picture follows.
    if (!waiting && !target.loading && !S.scrubbing && now >= p.failUntil && recorded(p.cam.name, t)) { p.noUpTo = 0; p.cur = into; load(p, into, back ? t + 500 : t, 'low', false, FIRST_WIN); }
    const busy = cs.loading || os.loading;
    const rec = recorded(p.cam.name, t);
    // Just back from fast pictures: the last one stays until the video is there.
    if (!near && p.pic && p.pic.drawn && p.pic.shownFor !== null && Math.abs(p.pic.shownFor - t) < 5000 * Math.max(1, Math.abs(S.speed))) {
      p.pic.cv.hidden = false; p.cv.hidden = true; p.msg.hidden = true;
      return;
    }
    if (near) {
      if (!shown.v.paused) shown.v.pause();
      shown.v.hidden = false; shown.v.classList.remove('tlup');
      p.cv.hidden = !S.scrubbing || !drawStrip(p, t);
      p.msg.hidden = true; p.waitSince = 0;
      return;
    }
    // While the video loads (or while scrubbing, or if it could not play),
    // the filmstrip picture of a recorded moment stands in for it.
    let pic = false;
    if (rec) pic = drawStrip(p, t); else p.cv.hidden = true;
    const text = (!busy && (now < p.failUntil || !rec)) ? (p.waitSince = 0, 'No recording') : waitText(p);
    if (p.msg.textContent !== text) p.msg.textContent = text;
    p.msg.classList.toggle('wait', text !== 'No recording');
    p.msg.hidden = pic;
    for (const s of p.slots) s.v.hidden = true;
    p.shownIdx = -1;
    return;
  }
  p.msg.hidden = true;
  p.waitSince = 0;
  stopPic(p);
  // The playing slot shows; a full-quality copy catching up behind it plays
  // invisibly (a hidden video would not decode).
  cs.v.hidden = false; cs.v.classList.remove('tlup');
  os.v.hidden = !(os.up && os.ready); os.v.classList.toggle('tlup', true);
  p.shownIdx = use;
  const keep = (s, master) => {
    const v = s.v, want = wantOf(s);
    if (S.playing && !S.scrubbing) {
      const rate = s.k > 1 ? 1 : safeRate(Math.abs(S.speed));
      if (v.playbackRate !== rate) v.playbackRate = rate;
      const slack = s.k > 1 ? 0.6 : 0.6 + 0.25 * Math.abs(S.speed);
      if (Math.abs(v.currentTime - want) > slack && !v.seeking) v.currentTime = want;
      if (v.paused) playVideo(v);
    } else {
      if (!v.paused) v.pause();
      if (Math.abs(v.currentTime - want) > (master ? 0.04 : 0.2) && !v.seeking) v.currentTime = want;
    }
  };
  keep(cs, true);
  if (os.up && os.ready && covers(os)) keep(os, false);
  watchHealth(p, cs, cs.v.seeking ? 0 : Math.abs(cs.v.currentTime - wantOf(cs)), cs.k > 1 ? 0.6 : 0.6 + 0.25 * Math.abs(S.speed));
  // Scrubbing: the filmstrip shows the moment at once; the video takes over
  // when the pointer lets go and it has caught up.
  if (!(S.scrubbing && drawStrip(p, t)) && !(cs.v.seeking && !p.cv.hidden)) p.cv.hidden = true;
  // Backwards: the piece before this one, early.
  if (back) {
    if (S.playing && !os.ready && !os.loading && now >= p.failUntil && t - cs.start < 4000 * Math.abs(S.speed) && recorded(p.cam.name, cs.start - 200)) load(p, 1 - p.cur, cs.start, 'low');
    return;
  }
  // Playing the sub stream where the main picture is wanted: fetch it
  // behind the one playing; playing the main picture where the sub stream
  // will do (a small tile, fast, or the device cannot keep up): the same,
  // the other way. Swapped in when it has caught up, no gap.
  const wq = tierQ(p);
  const idle = cs.k === 1 && !S.scrubbing && !os.loading && !os.ready;
  if (idle && wq === 'full' && cs.sub && !(p.noUpTo && t < p.noUpTo)) load(p, 1 - p.cur, t, 'full', true, FIRST_WIN);
  else if (idle && wq === 'low' && !cs.sub && camQ(p.cam.name).hasSub !== false && !(p.noDownTo && t < p.noDownTo)) load(p, 1 - p.cur, t, 'low', true, FIRST_WIN);
  // Load the next window early so the hand-over has no gap.
  else if (S.playing && !os.up && !os.ready && !os.loading && now >= p.failUntil && cs.end - t < 25000 * S.speed) {
    // The next window, or (a break in the recording follows) the next
    // stretch, when it is not far.
    let next = recorded(p.cam.name, cs.end + 200) ? cs.end : null;
    if (next === null && cs.k === 1) {
      const nx = spanAfter(p.cam.name, cs.end);
      if (nx && nx[0] - cs.end < 60000 * Math.max(1, Math.abs(S.speed))) next = Math.floor(nx[0]);
    }
    // Fast: each window up to twice the last, so the first ones are ready in time.
    // (A camera thought to have no sub stream: asked for it anyway, which
    // brings the main picture where there is none and finds it again where
    // there is, e.g. after a break in its recording.)
    if (next !== null) load(p, 1 - p.cur, next, cs.k > 1 ? cs.q : camQ(p.cam.name).hasSub === false ? 'low' : wq, false, undefined, cs.k > 1 ? Math.min(TRICK_WALL_MS, Math.max(TRICK_FIRST_MS, 2 * (cs.end - cs.start) / cs.k)) : undefined);
  }
}
const syncAll = () => { planTiers(); players.forEach(p => { tickPlayer(p); drawBoxes(p); }); };

// ---------- sound ----------
// The speaker on the one-camera view (speaker.js) hands its state here: at
// most one video sounds, the slot on screen (never the full-quality copy
// catching up behind it, which would double the sound), playing forwards at
// up to SOUND_MAX_RATE (browsers keep the pitch). Faster, backwards, paused
// or while scrubbing it is silent and comes back on its own.
const SOUND_MAX_RATE = 4;
const SND = { id: null, on: false, vol: 0.8 };
const soundSpeedOk = () => S.speed > 0 && S.speed <= SOUND_MAX_RATE;
function setSound(id, on, vol) {
  const was = SND.on && soundSpeedOk();
  SND.id = id; SND.on = !!on; SND.vol = vol;
  if (SND.on && !was && !soundSpeedOk()) note('Sound plays at up to ' + SOUND_MAX_RATE + 'x');
  players.forEach(applySound);
}
function applySound(p) {
  const mine = SND.on && S.on && S.solo !== null && String(p.cam.id) === String(SND.id) && String(S.solo) === String(SND.id);
  const may = mine && S.playing && !S.scrubbing;
  const set = (v, on) => {
    if (on) { if (v.volume !== SND.vol) v.volume = SND.vol; }
    if (v.muted === !on) return;
    v.muted = !on;
    // Unmuting can be refused without a recent click (autoplay rules): the
    // browser then pauses it; play on muted rather than stop.
    if (on && v.paused && S.playing) { const pr = v.play(); if (pr && pr.catch) pr.catch(() => { v.muted = true; v.play().catch(() => {}); }); }
  };
  if (p.live) set(p.live.v, may && !p.live.failed);
  const shown = p.shownIdx >= 0 ? p.slots[p.shownIdx] : null;
  for (const s of p.slots) set(s.v, may && !p.live && s === shown && !s.v.hidden && !s.v.classList.contains('tlup') && s.k === 1 && !s.rev && soundSpeedOk());
}


// At the live edge: the camera's live stream in place of the recording.
function showLive(p) {
  const q = S.solo !== null ? bigQ() : 'sub';
  if (p.live && p.live.q !== q) stopLive(p);
  if (!p.live) {
    const v = document.createElement('video');
    v.className = 'tllive'; v.muted = true; v.autoplay = true; v.playsInline = true;
    p.el.insertBefore(v, p.msg);
    const lv = { v, q, failed: false, player: null };
    lv.player = Live.startLive(v, p.cam.id, q, () => { lv.failed = true; });
    p.live = lv;
  }
  for (const s of p.slots) { if (s.ready || s.loading) resetSlot(s); s.v.hidden = true; }
  p.shownIdx = -1;
  p.cv.hidden = true;
  if (p.live.failed) {
    if (p.msg.textContent !== 'No live picture') p.msg.textContent = 'No live picture';
    p.msg.classList.remove('wait');
  }
  p.msg.hidden = !p.live.failed;
  p.live.v.hidden = p.live.failed;
}
function stopLive(p) {
  if (!p.live) return;
  try { p.live.player.stop(); } catch (e) { /* already stopped */ }
  p.live.v.remove();
  p.live = null;
}
// Playing into the live edge (or a click at it): watch live, the strip
// running on with the recordings. Anything that moves the playhead back,
// pausing or another speed returns to the recordings.
function goLive() {
  if (S.live) return;
  S.live = true;
  S.playing = true;
  if (S.speed !== 1) S.speed = 1;
  S.playhead = Date.now();
  S.anchorHead = S.playhead; S.anchorWall = performance.now();
  $('tlDock').classList.add('atlive');
  $('tlLive').classList.add('on');
  S.lastSec = -1;
  updateControls();
  syncAll();
  // The recordings bar grows: ask for new recordings more often.
  clearInterval(S.liveTimer);
  S.liveTimer = setInterval(() => { if (S.live && isActive('live') && !document.hidden) pollData(); }, LIVE_POLL_MS);
}
function leaveLive() {
  if (!S.live) return;
  S.live = false;
  clearInterval(S.liveTimer);
  const d = $('tlDock');
  if (d) d.classList.remove('atlive');
  if ($('tlLive')) $('tlLive').classList.remove('on');
  S.lastSec = -1;
}

// ---------- what the AI saw ----------
// Each alert's AI review holds the frames it looked at (a few per event) with
// the objects found: boxes as fractions of the picture. Fetched per event
// when needed; between two looks a box glides from one place to the next.
// Per event: the AI's own checks (its first moments, at once) and the AI
// run over the recording, made in parts (TRACK_PART_MS) so the moment being
// watched is ready soon: the part at the playhead, then the next one.
const aiSeen = new Map();   // event id -> { review: [...] | null, parts: Map(part -> frames), asked: Set }
const TRACK_PART_MS = 5000;
function aiChecks(e, t) {
  let a = aiSeen.get(e.id);
  if (!a) {
    a = { review: null, parts: new Map(), asked: new Set() };
    aiSeen.set(e.id, a);
    if (aiSeen.size > 3000) aiSeen.delete(aiSeen.keys().next().value);
    api('/api/events/' + enc(e.id) + '/ai').then(r => {
      const start = (r.event && r.event.start ? r.event.start : e.start) * 1000;
      a.review = ((r.review && r.review.checks) || []).filter(c => c.seen && c.seen.length)
        .map(c => ({ t: start + c.offset_ms, seen: c.seen, parked: c.parked || [] }));
    }).catch(() => { a.review = []; });
  }
  const from = e.start * 1000 - 2000;
  const k = Math.max(0, Math.floor((t - from) / TRACK_PART_MS));
  const ask = (n) => {
    if (n < 0 || a.asked.has(n) || (a.total !== undefined && n >= a.total)) return;
    a.asked.add(n);
    api('/api/events/' + enc(e.id) + '/track?part=' + n).then(r => {
      a.total = r.parts;
      a.parts.set(n, (r.frames || []).map(f => ({ t: f.t, seen: f.seen || [], parked: [] })));
    }).catch((err) => {
      // Busy with another part: ask again shortly (while boxes are wanted).
      a.asked.delete(n);
      if (!(err && err.status === 503)) a.asked.add(n);
    });
  };
  // Playing fast, the moment is gone before a part is ready (and each
  // part costs the graphics card seconds): the AI's own checks only.
  if (!(S.playing && Math.abs(S.speed) > 4)) {
    ask(k);
    ask(rev() ? k - 1 : k + 1);
  }
  if (!a.parts.size) return a.review;
  let all = [];
  for (const f of a.parts.values()) all = all.concat(f);
  // Where the run is not done yet, the AI's own checks fill in.
  return all.concat((a.review || []).filter(c => !a.parts.has(Math.floor((c.t - from) / TRACK_PART_MS))));
}
function boxesAt(name, t) {
  let checks = [];
  for (const e of D.ev) {
    if (e.camera !== name || t < e.start * 1000 - 3000 || t > e.end * 1000 + 3000) continue;
    const c = aiChecks(e, t);
    if (c) checks = checks.concat(c);
  }
  if (!checks.length) return [];
  checks.sort((a, b) => a.t - b.t);
  let i = checks.findIndex(c => c.t > t);
  if (i < 0) i = checks.length;
  const prev = checks[i - 1], next = checks[i];
  const list = (c) => c.seen.map((d, k) => ({ ...d, parked: !!c.parked[k] }));
  if (prev && next && next.t - prev.t <= 4000) {
    const f = (t - prev.t) / (next.t - prev.t);
    const a = list(prev), b = list(next), used = new Set(), out = [];
    const mid = (d) => [d.x + d.w / 2, d.y + d.h / 2];
    for (const d of a) {
      let best = -1, bd = 0.25;
      b.forEach((o, k) => {
        if (used.has(k) || o.label !== d.label) return;
        const [x0, y0] = mid(d), [x1, y1] = mid(o), dist = Math.hypot(x1 - x0, y1 - y0);
        if (dist < bd) { bd = dist; best = k; }
      });
      if (best >= 0) {
        used.add(best);
        const o = b[best], l = (u, v) => u + (v - u) * f;
        out.push({ ...d, x: l(d.x, o.x), y: l(d.y, o.y), w: l(d.w, o.w), h: l(d.h, o.h), score: Math.max(d.score, o.score) });
      } else if (f < 0.5) out.push(d);
    }
    b.forEach((o, k) => { if (!used.has(k) && f >= 0.5) out.push(o); });
    return out;
  }
  const near = [prev, next].filter(c => c && Math.abs(c.t - t) <= 1500).sort((x, y) => Math.abs(x.t - t) - Math.abs(y.t - t))[0];
  return near ? list(near) : [];
}
function kindOfLabel(l) {
  l = String(l).toLowerCase();
  if (l === 'person' || l === 'people' || l === 'human' || l === 'pedestrian') return 'person';
  if (VEH.has(l)) return 'vehicle';
  if (ANI.has(l)) return 'animal';
  return 'other';
}
// Over video, whatever the page's theme: bright colours (the same as the markers
// in dark mode) with near-black text, readable on both day and night pictures.
const BOX_COLORS = { person: '#ff7a45', vehicle: '#6ea8ff', animal: '#4cc38a', other: '#e5e7eb' };
const boxColor = (kind) => BOX_COLORS[kind] || BOX_COLORS.other;
// The boxes over player `p`'s picture at the playhead.
function drawBoxes(p) {
  if (!S.boxes || (p.cam && p.cam.remote)) {
    if (p.bx) { p.bx.remove(); p.bx = null; p.bxKey = ''; }
    return;
  }
  if (!p.bx) {
    p.bx = document.createElement('canvas');
    p.bx.className = 'tlboxes';
    p.el.insertBefore(p.bx, p.msg);
  }
  // Only what alerts are about (people, vehicles, animals): not the chairs,
  // benches and bottles the AI also names.
  const boxes = S.live ? [] : boxesAt(p.cam.name, S.playhead).filter(b => kindOfLabel(b.label) !== 'other');
  const W = p.el.clientWidth, H = p.el.clientHeight;
  const key = W + 'x' + H + ':' + boxes.map(b => b.label + [b.x, b.y, b.w, b.h].map(n => n.toFixed(3)).join(',')).join(';');
  if (key === p.bxKey) return;
  p.bxKey = key;
  const dpr = window.devicePixelRatio || 1;
  const cv = p.bx;
  if (cv.width !== Math.round(W * dpr) || cv.height !== Math.round(H * dpr)) { cv.width = Math.round(W * dpr); cv.height = Math.round(H * dpr); }
  const g = cv.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, W, H);
  if (!boxes.length) return;
  // Where the picture is inside the box (letterboxed to its own shape).
  const v = (p.live && p.live.v) || (p.slots.find(s => !s.v.hidden) || {}).v;
  const vw = (v && v.videoWidth) || 16, vh = (v && v.videoHeight) || 9;
  const sc = Math.min(W / vw, H / vh), pw = vw * sc, ph = vh * sc, ox = (W - pw) / 2, oy = (H - ph) / 2;
  g.font = '700 13px system-ui, sans-serif';
  g.textBaseline = 'middle';
  for (const b of boxes) {
    const col = boxColor(kindOfLabel(b.label));
    const x = ox + b.x * pw, y = oy + b.y * ph, w = b.w * pw, h = b.h * ph;
    // Parked objects dashed; unsure ones (under 50%) faint. The label stays solid so it can be read.
    g.globalAlpha = b.parked || (b.score || 0) < 0.5 ? 0.6 : 1;
    g.setLineDash(b.parked ? [5, 4] : []);
    g.lineWidth = 2;
    g.strokeStyle = col;
    g.strokeRect(x, y, w, h);
    g.globalAlpha = 1;
    g.setLineDash([]);
    const text = b.label + ' ' + Math.round((b.score || 0) * 100) + '%' + (b.parked ? ' (parked)' : '');
    const tw = g.measureText(text).width + 10, th = 20;
    // Above the box when there is room, and always inside the picture.
    const ty = y >= th ? y - th : Math.max(oy, y);
    const tx = Math.max(ox, Math.min(x - 1, ox + pw - tw));
    g.fillStyle = col;
    g.fillRect(tx, ty, tw, th);
    g.fillStyle = '#0b0d10';
    g.fillText(text, tx + 5, ty + th / 2 + 0.5);
  }
  g.globalAlpha = 1;
}
function setBoxes(on) {
  S.boxes = on;
  try { localStorage.setItem('ambs.tlBoxes', on ? '1' : '0'); } catch (e) { /* not remembered */ }
  const b = $('tlBoxes');
  if (b) { b.classList.toggle('on', on); b.setAttribute('aria-pressed', String(on)); }
  players.forEach(drawBoxes);
}

// ---------- playhead / clock ----------
function setPlayhead(ms) {
  leaveLive();
  S.jumpAt = performance.now();
  S.playhead = Math.max(0, Math.min(Math.round(ms), Date.now()));
  S.anchorHead = S.playhead; S.anchorWall = performance.now();
}
function setPlaying(on) {
  // Pausing live: still on the last recorded moment.
  if (!on && S.live) { leaveLive(); S.playhead = Date.now() - LIVE_EDGE_MS; }
  S.playing = on;
  S.anchorHead = S.playhead; S.anchorWall = performance.now();
  updateControls();
  wake();
  syncAll();
}
function setSpeed(x) {
  if (SND.on && soundSpeedOk() && !(x > 0 && x <= SOUND_MAX_RATE)) note('No sound above ' + SOUND_MAX_RATE + 'x or backwards');
  if (S.live && x !== 1) { leaveLive(); S.playhead = Date.now() - LIVE_EDGE_MS; } S.speed = x; S.anchorHead = S.playhead; S.anchorWall = performance.now(); updateControls(); syncAll(); }

function note(text) {
  const n = $('tlNote');
  if (!n) return;
  n.textContent = text;
  clearTimeout(S.noteTimer);
  S.noteTimer = setTimeout(() => { n.textContent = ''; }, 2200);
}

// Jump the playhead; recenter the strip when the target is not comfortably visible.
function seek(ms, ensure) {
  // At (or past) the live line: live.
  if (ms >= Date.now() - LIVE_EDGE_MS && S.playing) { goLive(); draw(); return; }
  setPlayhead(ms);
  const w = S.view.to - S.view.from;
  if (ensure && (S.playhead < S.view.from + w * 0.06 || S.playhead > S.view.to - w * 0.06)) {
    // (Through setView, like every other view change: the same limits, so the
    // strip never jumps later when it is first dragged or zoomed.)
    setView(S.playhead - w * 0.4, S.playhead + w * 0.6);
  }
  draw();
  syncAll();
}

// ---------- strip ----------
function setView(from, to) {
  let w = to - from;
  if (w < MIN_VIEW) { const c = (from + to) / 2; from = c - MIN_VIEW / 2; w = MIN_VIEW; }
  if (w > MAX_VIEW) { const c = (from + to) / 2; from = c - MAX_VIEW / 2; w = MAX_VIEW; }
  const limit = Date.now() + w * 0.1;
  if (from + w > limit) from = limit - w;
  if (from < 0) from = 0;
  S.view = { from, to: from + w };
  draw();
  scheduleFetch();
}
// A view change the user made (drag, wheel, pinch): the playhead is only
// followed again once it is back on screen.
function userView(from, to) {
  setView(from, to);
  S.headVisible = S.playhead >= S.view.from && S.playhead <= S.view.to;
}

function zoomAt(ms, factor) {
  const { from, to } = S.view;
  const w = Math.max(MIN_VIEW, Math.min(MAX_VIEW, (to - from) * factor));
  const frac = (ms - from) / (to - from);
  userView(ms - frac * w, ms - frac * w + w);
}

const STEPS = [1e3, 2e3, 5e3, 10e3, 15e3, 30e3, 60e3, 120e3, 300e3, 600e3, 900e3, 1800e3, 3600e3, 7200e3, 10800e3, 21600e3, 43200e3, 86400e3, 172800e3];
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
function ticksHtml(W) {
  const { from, to } = S.view;
  const ppm = W / (to - from);
  let step = STEPS[STEPS.length - 1];
  for (const s of STEPS) if (s * ppm >= 74) { step = s; break; }
  const off = new Date(from).getTimezoneOffset() * 60000;
  let html = '';
  for (let t = Math.ceil((from - off) / step) * step + off; t <= to; t += step) {
    const d = new Date(t);
    const mid = d.getHours() === 0 && d.getMinutes() === 0;
    const lab = mid ? MONTHS[d.getMonth()] + ' ' + d.getDate()
      : pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + (step < 60e3 ? ':' + pad2(d.getSeconds()) : '');
    html += '<span class="tlt' + (mid ? ' day' : '') + '" style="left:' + ((t - from) * ppm).toFixed(1) + 'px">' + lab + '</span>';
  }
  return html;
}

// The shown cameras that have recordings here (another server's stay there).
function coverCams() {
  return shownNames().filter(n => { const c = (state.cams || []).find(x => x.name === n); return !(c && c.remote); });
}
// One shared recording bar. Solid where every shown camera recorded, hatched
// where only some did (hover names the others), empty where none did.
function spansHtml(W) {
  const { from, to } = S.view;
  const ppm = W / (to - from);
  const tails = [], ev = [];
  const now = Date.now();
  const cams = coverCams(), M = cams.length;
  cams.forEach((n, ci) => {
    const sp = D.spans[n] || [];
    const list = [];
    for (const s of sp) if (s[1] >= from && s[0] <= to) list.push([s[0], s[1]]);
    // Still recording: the part being written (listed once it is saved)
    // runs on to now, fading to yellow.
    const last = sp[sp.length - 1];
    if (last && recordingNow(n, last[1], now) && last[1] < now && now >= from && last[1] <= to) { tails.push([last[1], now]); list.push([last[1], now]); }
    for (const [a, b] of mergeSpans(list)) { ev.push([a, 1, ci]); ev.push([b, -1, ci]); }
  });
  ev.sort((x, y) => x[0] - y[0] || y[1] - x[1]);
  const on = new Set(), segs = [];
  let prev = null;
  for (const [t, d, ci] of ev) {
    if (prev !== null && t > prev && on.size) {
      const full = on.size >= M;
      const miss = full ? [] : cams.filter((n, i) => !on.has(i));
      const last = segs[segs.length - 1];
      const key = miss.join('\n');
      if (last && last.key === key && (prev - last.b) * ppm < 1.5) last.b = t; else segs.push({ a: prev, b: t, full, miss, key });
    }
    if (d > 0) on.add(ci); else on.delete(ci);
    prev = t;
  }
  S.covSegs = [];
  let html = '';
  for (const g of segs) {
    const x0 = (g.a - from) * ppm, x1 = (g.b - from) * ppm;
    const w = Math.max(g.full ? 1.5 : 3, x1 - x0);
    if (g.full) html += '<i class="full" style="left:' + x0.toFixed(1) + 'px;width:' + w.toFixed(1) + 'px"></i>';
    else { S.covSegs.push(g); html += '<i class="part" data-p="' + (S.covSegs.length - 1) + '" style="left:' + x0.toFixed(1) + 'px;width:' + w.toFixed(1) + 'px"></i>'; }
  }
  S.covM = M;
  // (The recording-now end stays visible when zoomed out: at least 8 px,
  // ending at now.)
  let cur = null;
  const flush = () => { if (cur) { const w = Math.max(8, cur[1] - cur[0]); html += '<i class="tail" title="Recording now (still being saved)" style="left:' + (cur[1] - w).toFixed(1) + 'px;width:' + w.toFixed(1) + 'px"></i>'; } };
  for (const [a, b] of mergeSpans(tails)) {
    const x0 = (a - from) * ppm, x1 = (b - from) * ppm;
    if (cur && x0 - cur[1] < 1.5) cur[1] = Math.max(cur[1], x1); else { flush(); cur = [x0, x1]; }
  }
  flush();
  S.hasTail = tails.length > 0;
  return html;
}

function groupEvents() {
  const names = new Set(shownNames());
  const { from, to } = S.view;
  const W = $('tlArea').clientWidth || 1;
  const ppm = W / (to - from);
  const evs = D.ev.filter(e => names.has(e.camera) && passes(e) && e.end * 1000 >= from - 60000 && e.start * 1000 <= to + 60000)
    .sort((a, b) => a.start - b.start);
  const groups = [];
  for (const e of evs) {
    const x = (e.start * 1000 - from) * ppm;
    const g = groups[groups.length - 1];
    if (g && x - g.x < CLUSTER_PX) g.evs.push(e); else groups.push({ x, evs: [e] });
  }
  for (const g of groups) {
    let best = 'other';
    for (const e of g.evs) { const k = kindOf(e); if (KINDS[k].rank > KINDS[best].rank) best = k; }
    g.kind = best;
    g.n = g.evs.reduce((s, e) => s + (e.n || 1), 0);
    g.t0 = g.evs[0].start * 1000;
    g.t1 = Math.max(...g.evs.map(e => e.end * 1000));
  }
  return groups;
}

// What a mark says: the object itself for a single event ("Car", "Truck"), else its kind.
function markWord(g) {
  if (g.n === 1 && g.evs.length === 1) {
    const o = (g.evs[0].objects || []).find(x => x && x !== 'unusual' && !x.includes(':') && x.length <= 12 && kindOf({ objects: [x] }) === g.kind);
    if (o) return o.charAt(0).toUpperCase() + o.slice(1);
  }
  return KINDS[g.kind].label;
}

let drawQueued = false;
function draw() {
  if (!S.on || drawQueued) return;
  drawQueued = true;
  requestAnimationFrame(() => { drawQueued = false; drawNow(); });
}
function drawNow() {
  const area = $('tlArea');
  if (!S.on || !area) return;
  const W = area.clientWidth;
  if (!W) return;
  const { from, to } = S.view;
  const ppm = W / (to - from);
  $('tlTicks').innerHTML = ticksHtml(W);
  wantStrips();
  $('tlSpans').innerHTML = spansHtml(W);
  // Signal lanes under the alerts: a door, an alarm, a camera's input.
  const lanes = D.sig || [];
  // (The strip's own height comes from the page style: shorter on a phone on its side.)
  const base = parseFloat(getComputedStyle(area).getPropertyValue('--tl-h')) || 96;
  const sh = base + lanes.length * SIG_LANE + (lanes.length ? 4 : 0);
  if (area.style.height !== sh + 'px') { area.style.height = sh + 'px'; trackDockHeight(); }
  $('tlSigs').innerHTML = lanes.map((g, i) => {
    const top = base + i * SIG_LANE;
    const bars = g.spans.filter(sp => sp[1] >= from && sp[0] <= to).map(sp => {
      const x0 = Math.max(0, (sp[0] - from) * ppm), x1 = Math.min(W, (sp[1] - from) * ppm);
      return '<i style="left:' + x0.toFixed(1) + 'px;width:' + Math.max(2, x1 - x0).toFixed(1) + 'px" title="' + esc(g.name) + ': on ' + new Date(sp[0]).toLocaleTimeString() + ' to ' + new Date(sp[1]).toLocaleTimeString() + '"></i>';
    }).join('');
    return '<div class="tlsig' + (g.on ? ' on' : '') + '" style="top:' + top + 'px">' + bars + '<span>' + esc(g.name) + '</span></div>';
  }).join('');
  S.groups = groupEvents();
  $('tlMarks').innerHTML = S.groups.map((g, i) => {
    const k = KINDS[g.kind];
    const dur = Math.max(2, Math.min(60, (g.t1 - g.t0) * ppm));
    const word = markWord(g);
    const label = (g.n > 1 ? g.n + ' alerts, ' : '') + word + ' at ' + new Date(g.t0).toLocaleTimeString();
    // The word shows when it fits before the next mark (or the end of the strip).
    // (The "now" line is an end too: a word never runs into it.)
    const nowPx = (Date.now() - from) * ppm;
    const room = Math.min(i + 1 < S.groups.length ? S.groups[i + 1].x : W + 12, nowPx > g.x ? nowPx - 6 : Infinity) - g.x;
    const fits = room >= 34 + word.length * 7.5 + (g.n > 1 ? 12 : 0);
    return '<button class="tlm k-' + g.kind + (fits ? ' wd' : '') + '" data-g="' + i + '" style="left:' + g.x.toFixed(1) + 'px" aria-label="' + esc(label) + '">' +
      kIcon(g.kind) + (fits ? '<span>' + esc(word) + '</span>' : '') + (g.n > 1 ? '<b>' + (g.n > 99 ? '99+' : g.n) + '</b>' : '') +
      '<u style="width:' + dur.toFixed(1) + 'px"></u></button>';
  }).join('');
  const nowX = (Date.now() - from) * ppm;
  const nl = $('tlNow');
  nl.hidden = nowX < 0 || nowX > W;
  nl.style.left = nowX + 'px';
  // Near the right end of the strip the word goes to the left of the line, so it is never cut off.
  nl.classList.toggle('flip', nowX > W - 56);
  updateRange();
  markChips();
  updateHead();
}

function updateHead() {
  const area = $('tlArea');
  if (!S.on || !area) return;
  const W = area.clientWidth;
  const { from, to } = S.view;
  const x = (S.playhead - from) / (to - from) * W;
  const ph = $('tlHead');
  ph.hidden = x < -2 || x > W + 2;
  ph.style.left = x + 'px';
  const sec = Math.floor(S.playhead / 1000);
  if (sec !== S.lastSec) {
    S.lastSec = sec;
    $('tlClock').textContent = (S.live ? 'Live · ' : '') + fmtWall(S.playhead);
    const d = new Date(S.playhead);
    $('tlHeadT').textContent = pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
    const gt = $('tlGoto');
    if (gt && document.activeElement !== gt) gt.value = toLocalSec(S.playhead);
  }
  // The time bubble stays inside the strip.
  const hb = $('tlHeadT');
  hb.dataset.edge = x < 44 ? 'l' : x > W - 44 ? 'r' : '';
}
// YYYY-MM-DDTHH:MM:SS in local time (what datetime-local wants).
function toLocalSec(ms) {
  const d = new Date(ms);
  return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate()) + 'T' + pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
}

// Runs every frame while the mode is on (cheap when nothing moves).
function loop() {
  if (!S.on) return;
  requestAnimationFrame(loop);
  if (!isActive('live') || document.hidden) return;
  if (S.playing && !S.scrubbing) {
    if (S.live) S.playhead = Date.now();
    else {
      S.playhead = S.anchorHead + (performance.now() - S.anchorWall) * S.speed;
      if (S.playhead >= Date.now() - LIVE_EDGE_MS) goLive();
    }
    const { from, to } = S.view, w = to - from;
    const vis = S.playhead >= from && S.playhead <= to;
    // Follow only when playback carries the playhead off the right edge (it
    // was on screen a moment ago); never after the user moved the strip away
    // from it (userView), or the strip would leap back under their pointer.
    if (S.headVisible && !vis && S.playhead > to) setView(S.playhead - w * 0.25, S.playhead + w * 0.75);   // follow
    else if (S.headVisible && !vis && S.playhead < from) setView(S.playhead - w * 0.75, S.playhead + w * 0.25);   // follow backwards
    else S.headVisible = vis;
    updateHead();
  }
  const t = performance.now();
  // Boxes follow the picture smoothly while playing.
  if (S.boxes && S.playing && t - (S.boxesDrawn || 0) > 66) { S.boxesDrawn = t; players.forEach(drawBoxes); }
  // The recording-now end of the bar grows with the clock.
  if (S.hasTail && t - (S.tailDrawn || 0) > 1000) {
    S.tailDrawn = t;
    const W = $('tlArea').clientWidth || 1;
    $('tlSpans').innerHTML = spansHtml(W);
  }
}

// ---------- pointer interaction ----------
function msAt(clientX) {
  const r = $('tlArea').getBoundingClientRect();
  return S.view.from + (clientX - r.left) / r.width * (S.view.to - S.view.from);
}

// Camera the strip previews: the one shown alone, else the selected one, else
// the first shown (own cameras only: another server's recordings stay there).
function previewCam() {
  const ids = [S.solo, Live.getSelected()].concat(S.shown || []);
  for (const id of ids) {
    const c = id != null ? camFromId(id) : null;
    if (c && !c.remote) return c.name;
  }
  return null;
}

function previewAt(clientX) {
  const cam = previewCam();
  const ms = msAt(clientX);
  if (!cam || ms > Date.now() || !recorded(cam, ms)) { hidePreview(); return; }
  const r = $('tlArea').getBoundingClientRect();
  const f = stripFrame(cam, ms);
  if (f) showPreviewFrame(cam, ms, clientX, r.top, f);
  else showPreview(cam, ms, clientX, r.top);
}

// Strip gestures (what NVR timelines have taught people to expect):
// - click anywhere = play from exactly there (a little hand movement is fine)
// - press and drag on the strip = scrub: the playhead and video follow, with a
//   picture of the moment above the pointer; playback resumes on release
// - drag the time ruler along the top (or scroll sideways, or two fingers on
//   a phone) = move the strip; the wheel or a pinch zooms
// - alert markers have their own row and play their alert when clicked
function initPointer() {
  const area = $('tlArea');
  area.addEventListener('pointerleave', () => { if (!S.scrubbing) hidePreview(); hideCovTip(); });
  initRange();
  const ptrs = new Map();
  let g = null;
  let lastSync = 0;
  const RULER = 22;   // px: the time ruler at the top of the strip
  const slack = (e) => (e.pointerType === 'touch' ? 10 : 6);
  const startPinch = () => {
    const [a, b] = [...ptrs.values()];
    g = { mode: 'pinch', d0: Math.max(20, Math.abs(a.x - b.x)), view0: { ...S.view }, moved: true };
  };
  // Scrubbing: the playhead follows at once, the videos a few times a second.
  const scrubTo = (x, force) => {
    setPlayhead(msAt(x));
    updateHead();
    previewAt(x);
    players.forEach(p => { if (drawStrip(p, S.playhead)) p.msg.hidden = true; });
    const now = performance.now();
    if (force || now - lastSync > 150) { lastSync = now; syncAll(); }
  };
  area.addEventListener('pointerdown', (e) => {
    if (e.target.closest('.tlm, .tlrh, .tlexp') || e.button > 0) return;
    try { area.setPointerCapture(e.pointerId); } catch (err) { /* synthetic pointer */ }
    ptrs.set(e.pointerId, { x: e.clientX });
    if (ptrs.size === 2) { if (S.scrubbing) { S.scrubbing = false; } startPinch(); return; }
    const onRuler = e.clientY - area.getBoundingClientRect().top < RULER;
    hideCovTip(); closePop();
    // Shift + drag: select a range to export.
    if (e.shiftKey && !onRuler) { g = { mode: 'range', startX: e.clientX, t0: Math.min(Date.now(), msAt(e.clientX)), moved: false }; hidePreview(); return; }
    g = { mode: onRuler ? 'ruler' : 'press', startX: e.clientX, view0: { ...S.view }, moved: false, resume: S.playing };
    hideTip();
  });
  area.addEventListener('pointermove', (e) => {
    // Hovering (mouse, nothing pressed): picture of that moment.
    if (!ptrs.size && e.pointerType === 'mouse') {
      const onRuler = e.clientY - area.getBoundingClientRect().top < RULER;
      area.classList.toggle('ruler', onRuler);
      if (e.target.closest('.tlm, .tlrh, .tlexp')) hidePreview(); else previewAt(e.clientX);
      return;
    }
    if (!ptrs.has(e.pointerId)) return;
    ptrs.get(e.pointerId).x = e.clientX;
    if (!g) return;
    const r = area.getBoundingClientRect();
    if (g.mode === 'pinch' && ptrs.size === 2) {
      const [a, b] = [...ptrs.values()];
      const w0 = g.view0.to - g.view0.from;
      const w = Math.max(MIN_VIEW, Math.min(MAX_VIEW, w0 * g.d0 / Math.max(20, Math.abs(a.x - b.x))));
      const mid = (a.x + b.x) / 2;
      const frac = (mid - r.left) / r.width;
      // keep the time that was under the fingers' midpoint at the start under them now
      if (g.anchor === undefined) g.anchor = g.view0.from + ((g.mid0 = mid) - r.left) / r.width * w0;
      userView(g.anchor - frac * w, g.anchor - frac * w + w);
      return;
    }
    const far = Math.abs(e.clientX - g.startX) > slack(e);
    if (g.mode === 'range') {
      if (far || g.moved) {
        g.moved = true;
        const t1 = Math.min(Date.now(), msAt(e.clientX));
        setRange(Math.min(g.t0, t1), Math.max(g.t0, t1));
      }
      return;
    }
    if (g.mode === 'press' && far) { g.mode = 'scrub'; g.moved = true; S.scrubbing = true; area.classList.add('scrub'); }
    if (g.mode === 'ruler' && far) { g.mode = 'pan'; g.moved = true; area.classList.add('pan'); hidePreview(); }
    if (g.mode === 'scrub') { scrubTo(e.clientX, false); return; }
    if (g.mode === 'pan') {
      const w0 = g.view0.to - g.view0.from;
      const dt = (e.clientX - g.startX) / r.width * w0;
      userView(g.view0.from - dt, g.view0.to - dt);
    }
  });
  const end = (e) => {
    if (!ptrs.has(e.pointerId)) return;
    ptrs.delete(e.pointerId);
    if (ptrs.size === 1 && g && g.mode === 'pinch') {   // continue as a pan with the remaining finger
      const only = [...ptrs.values()][0];
      g = { mode: 'pan', startX: only.x, view0: { ...S.view }, moved: true };
      return;
    }
    if (ptrs.size) return;
    area.classList.remove('pan', 'scrub');
    // A click (press and release without really moving): exactly there.
    if (g && (g.mode === 'press' || g.mode === 'ruler') && !g.moved && e.type === 'pointerup') { seek(msAt(e.clientX), false); }
    if (g && g.mode === 'scrub') { scrubTo(e.clientX, true); S.scrubbing = false; if (S.playhead >= Date.now() - LIVE_EDGE_MS && S.playing) goLive(); else { setPlayhead(S.playhead); syncAll(); } }
    if (e.pointerType !== 'mouse') hidePreview();
    g = null;
  };
  area.addEventListener('pointerup', end);
  area.addEventListener('pointercancel', end);
  area.addEventListener('wheel', (e) => {
    e.preventDefault();
    if (Math.abs(e.deltaX) > Math.abs(e.deltaY)) {
      const w = S.view.to - S.view.from;
      userView(S.view.from + e.deltaX / area.clientWidth * w, S.view.to + e.deltaX / area.clientWidth * w);
    } else {
      zoomAt(msAt(e.clientX), Math.exp(e.deltaY * (e.deltaMode ? 0.05 : 0.0016)));
    }
  }, { passive: false });
}

// ---------- marker tooltip / clicks ----------
let tipEl = null, tipTimer = 0;
function hideTip() { clearTimeout(tipTimer); if (tipEl) tipEl.hidden = true; }
function showTip(btn) {
  const g = S.groups[+btn.dataset.g];
  if (!g) return;
  const e = g.evs[0];
  const day = (ms) => { const d = new Date(ms); return d.toDateString() === new Date().toDateString() ? d.toLocaleTimeString() : d.toLocaleString(); };
  let h = '';
  if (e.thumb) h += '<img src="/api/events/' + enc(e.id) + '/thumb" alt="" loading="lazy">';
  if (g.n === 1) {
    const objs = (e.objects || []).length ? e.objects.join(', ') : 'unrecognized';
    h += '<div class="tt1">' + ic(KINDS[g.kind].ic) + '<b>' + KINDS[g.kind].label + '</b></div><div>' + esc(day(e.start * 1000)) + '</div><div class="tm">' + esc(e.camera) + ' · ' + esc(objs) + '</div>';
  } else {
    h += '<div class="tt1"><b>' + g.n + ' alerts</b></div><div>' + esc(day(g.t0)) + '</div>';
    h += g.evs.slice(0, 4).map(x => '<div class="tm">' + esc(new Date(x.start * 1000).toLocaleTimeString()) + ' · ' + esc(x.camera) + ' · ' + esc((x.objects || []).join(', ') || 'alert') + (x.n > 1 ? ' · ' + x.n + ' alerts' : '') + '</div>').join('');
    const rest = g.n - g.evs.slice(0, 4).reduce((s, x) => s + (x.n || 1), 0);
    if (rest > 0) h += '<div class="tm">and ' + rest + ' more (zoom in)</div>';
  }
  if (!tipEl) { tipEl = document.createElement('div'); tipEl.className = 'tltip'; tipEl.setAttribute('role', 'tooltip'); document.body.appendChild(tipEl); }
  tipEl.innerHTML = h;
  tipEl.hidden = false;
  // Always above the strip, growing upwards (never over the timeline, so
  // the pointer can keep moving along it): anchored by its bottom edge, and
  // the picture shrinks when there is little room above.
  const area = $('tlArea');
  const top = Math.min(btn.getBoundingClientRect().top, area ? area.getBoundingClientRect().top : Infinity);
  const room = Math.max(40, top - 14);
  const pic = tipEl.querySelector('img');
  if (pic) { pic.style.maxHeight = ''; const extra = tipEl.offsetHeight - room; if (extra > 0) pic.style.maxHeight = Math.max(0, pic.offsetHeight - extra) + 'px'; if (pic.style.maxHeight === '0px') pic.hidden = true; }
  const r = btn.getBoundingClientRect(), tw = tipEl.offsetWidth;
  tipEl.style.left = Math.max(6, Math.min(window.innerWidth - tw - 6, r.left + r.width / 2 - tw / 2)) + 'px';
  tipEl.style.top = 'auto';
  tipEl.style.bottom = (window.innerHeight - top + 8) + 'px';
  tipEl.style.maxHeight = room + 'px';
}

function initMarkers() {
  const marks = $('tlMarks');
  marks.addEventListener('pointerover', (e) => { const b = e.target.closest('.tlm'); if (b && e.pointerType !== 'touch') showTip(b); });
  marks.addEventListener('pointerout', (e) => { if (e.target.closest('.tlm') && e.pointerType !== 'touch') hideTip(); });
  marks.addEventListener('focusin', (e) => { const b = e.target.closest('.tlm'); if (b) showTip(b); });
  marks.addEventListener('focusout', hideTip);
  marks.addEventListener('click', (e) => {
    const b = e.target.closest('.tlm');
    if (!b) return;
    const g = S.groups[+b.dataset.g];
    if (!g) return;
    // Several alerts in one mark: zoom to fit them; already as close as it
    // goes (or zooming would not separate them): a list to pick from.
    if (g.n > 1) {
      const span = g.t1 - g.t0, w = S.view.to - S.view.from;
      const target = Math.max(MIN_VIEW, span * 1.8 + 4000);
      if (w > MIN_VIEW * 1.05 && target < w * 0.85) {
        hideTip();
        const c = (g.t0 + g.t1) / 2;
        userView(c - target / 2, c + target / 2);
        return;
      }
      hideTip();
      openPop(b, g);
      return;
    }
    showTip(b);
    clearTimeout(tipTimer); tipTimer = setTimeout(hideTip, 3500);
    // Play the (first) alert; never move or zoom the strip under the pointer.
    seek(landing(g.evs[0]), false);
  });
  // Hatched stretches of the recording bar: which cameras did not record.
  const sp = $('tlSpans');
  sp.addEventListener('pointerover', (e) => { const i = e.target.closest && e.target.closest('i.part'); if (i && e.pointerType !== 'touch') showCovTip(i, e.clientX); });
  sp.addEventListener('pointermove', (e) => { const i = e.target.closest && e.target.closest('i.part'); if (i && e.pointerType !== 'touch' && covTip && !covTip.hidden) placeCovTip(e.clientX); });
  sp.addEventListener('pointerout', (e) => { if (e.target.closest && e.target.closest('i.part')) hideCovTip(); });
}

let covTip = null;
function showCovTip(i, x) {
  const g = (S.covSegs || [])[+i.dataset.p];
  if (!g) return;
  if (!covTip) { covTip = document.createElement('div'); covTip.className = 'tltip tlcov'; covTip.setAttribute('role', 'tooltip'); document.body.appendChild(covTip); }
  const M = S.covM || 0;
  covTip.innerHTML = '<div class="tt1"><b>' + (M - g.miss.length) + ' of ' + M + ' cameras recorded</b></div><div class="tm">No recording: ' + g.miss.map(esc).join(', ') + '</div>';
  covTip.hidden = false;
  placeCovTip(x);
}
function placeCovTip(x) {
  const top = $('tlArea').getBoundingClientRect().top;
  const tw = covTip.offsetWidth;
  covTip.style.left = Math.max(6, Math.min(window.innerWidth - tw - 6, x - tw / 2)) + 'px';
  covTip.style.top = 'auto';
  covTip.style.bottom = (window.innerHeight - top + 8) + 'px';
}
function hideCovTip() { if (covTip) covTip.hidden = true; }

// ---------- alert list (a mark that cannot be zoomed apart) ----------
let popEl = null, popOff = null;
function closePop() {
  if (!popEl) return false;
  const was = !popEl.hidden;
  popEl.hidden = true;
  if (popOff) { document.removeEventListener('pointerdown', popOff, true); popOff = null; }
  return was;
}
function openPop(btn, g) {
  const rows = g.evs.slice(0, 60);
  if (!popEl) { popEl = document.createElement('div'); popEl.className = 'tlpop'; popEl.setAttribute('role', 'dialog'); popEl.setAttribute('aria-label', 'Alerts here'); document.body.appendChild(popEl); }
  const more = g.n - rows.reduce((a, x) => a + (x.n || 1), 0);
  popEl.innerHTML = '<div class="tlpoph">' + g.n + ' alerts</div>' + rows.map((e, i) => {
    const k = kindOf(e);
    return '<button type="button" class="tlpr k-' + k + '" data-i="' + i + '"><span class="tlpi">' + kIcon(k) + '</span><b>' + esc(new Date(e.start * 1000).toLocaleTimeString()) + '</b><span>' + esc(e.camera) + '</span><em>' + esc(KINDS[k].label + (e.n > 1 ? ' x' + e.n : '')) + '</em></button>';
  }).join('') + (more > 0 ? '<div class="tlpoph">and ' + more + ' more</div>' : '');
  popEl.hidden = false;
  const top = Math.min(btn.getBoundingClientRect().top, $('tlArea').getBoundingClientRect().top);
  const r = btn.getBoundingClientRect(), pw = popEl.offsetWidth;
  popEl.style.left = Math.max(6, Math.min(window.innerWidth - pw - 6, r.left + r.width / 2 - pw / 2)) + 'px';
  popEl.style.bottom = (window.innerHeight - top + 8) + 'px';
  popEl.style.maxHeight = Math.max(120, top - 16) + 'px';
  popEl.onclick = (ev) => {
    const b = ev.target.closest('.tlpr');
    if (!b) return;
    const e = rows[+b.dataset.i];
    closePop();
    if (e) seek(landing(e), true);
  };
  if (popOff) document.removeEventListener('pointerdown', popOff, true);
  popOff = (ev) => { if (!popEl.contains(ev.target) && !ev.target.closest('.tlm')) closePop(); };
  setTimeout(() => { if (popOff) document.addEventListener('pointerdown', popOff, true); }, 0);
  const first = popEl.querySelector('.tlpr');
  if (first && !('ontouchstart' in window)) first.focus({ preventScroll: true });
}

// ---------- range (mark in / out) and export ----------
const R = { a: null, b: null };
function rangeCams() { return coverCams(); }
function fmtDur(ms) {
  const t = Math.round(ms / 1000), h = Math.floor(t / 3600), m = Math.floor(t % 3600 / 60), s = t % 60;
  return h ? h + ':' + pad2(m) + ':' + pad2(s) : m + ':' + pad2(s);
}
function setRange(a, b) { R.a = a; R.b = b; updateRange(); }
function clearRange() {
  if (R.a === null && R.b === null) return false;
  R.a = R.b = null; updateRange();
  return true;
}
function markIn() {
  R.a = Math.round(S.playhead);
  if (R.b !== null && R.b <= R.a) R.b = null;
  updateRange(); note('Range start ' + fmtWall(R.a).slice(-8));
}
function markOut() {
  R.b = Math.round(S.playhead);
  if (R.a !== null && R.a >= R.b) R.a = null;
  updateRange(); note('Range end ' + fmtWall(R.b).slice(-8));
}
function updateRange() {
  const box = $('tlRange');
  if (!box) return;
  const W = $('tlArea').clientWidth || 1;
  const { from, to } = S.view, ppm = W / (to - from);
  const has = R.a !== null || R.b !== null;
  box.hidden = !has;
  const full = R.a !== null && R.b !== null;
  box.classList.toggle('half', has && !full);
  const exp = $('tlExport');
  exp.hidden = !full;
  $('tlClr').hidden = !has;
  if (!has) return;
  const a = R.a !== null ? R.a : R.b, b = R.b !== null ? R.b : R.a;
  const x0 = (a - from) * ppm, x1 = (b - from) * ppm;
  box.style.left = x0.toFixed(1) + 'px';
  box.style.width = Math.max(full ? 2 : 0, x1 - x0).toFixed(1) + 'px';
  if (full) {
    const n = rangeCams().length;
    exp.textContent = 'Export ' + fmtDur(b - a) + ' \u00b7 ' + n + ' camera' + (n === 1 ? '' : 's');
    // Centered over the visible part of the range, kept inside the strip.
    const vis0 = Math.max(0, x0), vis1 = Math.min(W, x1);
    const bw = exp.offsetWidth || 150;
    const c = vis1 > vis0 ? (vis0 + vis1) / 2 : x0;
    exp.style.left = Math.max(4, Math.min(W - bw - 4, c - bw / 2)).toFixed(0) + 'px';
  }
}
function initRange() {
  const area = $('tlArea');
  // Handles: drag either end.
  for (const h of area.querySelectorAll('.tlrh')) {
    h.addEventListener('pointerdown', (e) => {
      if (e.button > 0) return;
      e.preventDefault(); e.stopPropagation();
      try { h.setPointerCapture(e.pointerId); } catch (err) { /* synthetic */ }
      let which = h.dataset.w;
      hidePreview();
      const move = (ev) => {
        const t = Math.min(Date.now(), Math.max(0, Math.round(msAt(ev.clientX))));
        R[which] = t;
        if (R.a > R.b) { [R.a, R.b] = [R.b, R.a]; which = which === 'a' ? 'b' : 'a'; }
        updateRange();
      };
      const up = () => { h.removeEventListener('pointermove', move); h.removeEventListener('pointerup', up); h.removeEventListener('pointercancel', up); };
      h.addEventListener('pointermove', move); h.addEventListener('pointerup', up); h.addEventListener('pointercancel', up);
    });
  }
  $('tlExport').addEventListener('click', (e) => { e.stopPropagation(); exportRange(); });
}
// The saved MP4 is one camera: the dialog offers each shown camera and, with
// several shown, all of them (one file each).
function exportRange() {
  let a = R.a, b = R.b;
  if (a !== null && b === null && S.playhead > a + 500) b = Math.round(S.playhead);
  if (a === null || b === null || b <= a) { note('Mark a range first: I and O, or Shift + drag'); return; }
  const cams = rangeCams();
  if (!cams.length) { note('No cameras with recordings here'); return; }
  closePop();
  if (S.playing) setPlaying(false);
  exportDialog({ cams, camera: S.solo !== null ? (camFromId(S.solo) || {}).name : undefined, from: a, to: b });
}

// ---------- zoom presets / go to ----------
const CHIPS = [['1h', 3600e3], ['6h', 6 * 3600e3], ['24h', 86400e3], ['7d', 7 * 86400e3]];
function viewWidth(w) {
  const now = Date.now();
  if (S.live || S.playhead > now - w * 0.1) userView(now - w * 0.85, now + w * 0.15);
  else userView(S.playhead - w * 0.4, S.playhead + w * 0.6);
}
function markChips() {
  const c = $('tlChips');
  if (!c) return;
  const w = S.view.to - S.view.from;
  for (const b of c.children) b.classList.toggle('on', Math.abs(w / (+b.dataset.ms) - 1) < 0.03);
}

// ---------- alert stepping ----------
function inCache(head) { return D.range && head >= D.range[0] && head <= D.range[1]; }

async function findOutside(dir, head, names) {
  const single = names.length === 1 ? '&camera=' + enc(names[0]) : '';
  const set = new Set(names);
  if (dir < 0) {
    const until = Math.floor((head + 1000) / 1000);
    for (let off = 0; off < 1000; off += 200) {
      const r = await api('/api/events?limit=200&offset=' + off + '&until=' + until + single);
      const evs = r.events || [];
      const hit = evs.find(e => set.has(e.camera) && passes(e) && landing(e) < head - 1000);   // newest first
      if (hit) return hit;
      if (evs.length < 200) return null;
    }
    return null;
  }
  const since = Math.floor((head + 1000) / 1000);
  for (const span of [3600, 43200, 259200, 2592000, 34560000]) {
    let until = since + span;
    for (let k = 0; k < 6; k++) {
      const r = await api('/api/events?limit=500&since=' + since + '&until=' + until + single);
      const evs = r.events || [];
      const ok = evs.filter(e => set.has(e.camera) && passes(e) && landing(e) > head + 1000);
      if (evs.length < 500) {
        if (ok.length) return ok.reduce((a, b) => (landing(a) <= landing(b) ? a : b));
        break;
      }
      // truncated (newest first): the nearest ones are cut; narrow to what we saw
      until = Math.min(...evs.map(e => e.start)) ;
    }
  }
  return null;
}

async function stepAlert(dir) {
  if (S.stepBusy || !S.on) return;
  S.stepBusy = true;
  try {
    const names = shownNames();
    const set = new Set(names), head = S.playhead;
    let hit = null;
    if (inCache(head)) {
      const c = D.ev.filter(e => set.has(e.camera) && passes(e) && (dir < 0 ? landing(e) < head - 1000 : landing(e) > head + 1000));
      if (c.length) hit = c.reduce((a, b) => ((dir < 0) === (landing(a) > landing(b)) ? a : b));
    }
    if (!hit) { try { hit = await findOutside(dir, head, names); } catch (e) { hit = null; } }
    if (!hit) { const w = kindsOn.size ? [...kindsOn].map(k => KINDS[k].label.toLowerCase()).join('/') + ' ' : ''; note(dir < 0 ? 'No earlier ' + w + 'alert' : 'No later ' + w + 'alert'); return; }
    seek(landing(hit), true);
    // zoom so the alert is not lost in a very wide view
    if (S.view.to - S.view.from > 6 * 3600000) { const c = S.playhead; setView(c - 1.5 * 3600000, c + 1.5 * 3600000); }
  } finally { S.stepBusy = false; }
}

// ---------- controls ----------
function buildDock() {
  const dock = $('tlDock');
  dock.innerHTML =
    '<div class="tlbar">' +
      '<div class="tlgrp">' +
        '<button class="btn tlstep" id="tlPrev" title="Previous alert  ," aria-label="Previous alert">' + ic('prevalert') + '<span>Previous alert</span></button>' +
        '<button class="btn tlplay" id="tlPlay" title="Play / pause  Space"></button>' +
        '<button class="btn tlstep" id="tlNext" title="Next alert  ." aria-label="Next alert">' + '<span>Next alert</span>' + ic('nextalert') + '</button>' +
      '</div>' +
      '<label class="tlspdsel" title="Playback speed  [ slower  ] faster  J reverse / slower  L faster"><span class="sr">Speed</span><select id="tlSpeed" aria-label="Playback speed">' + speedOptions(S.speed || 1, 0, PIC_OK ? FAST_REVERSE.concat(REVERSE_SPEEDS) : true) + '</select></label>' +
      '<div class="tlclk"><span id="tlClock"></span><span id="tlNote" class="tlnote" aria-live="polite"></span></div>' +
      '<div class="tlgrp tlright">' +
        '<button class="btn icon' + (S.boxes ? ' on' : '') + '" id="tlBoxes" aria-pressed="' + S.boxes + '" title="Show what the AI detected: a box around each object, colored like the alert icons" aria-label="Show AI detections">' + ic('zone') + '</button>' +
        '<button class="btn" id="tlBack" hidden title="Back to all cameras  Esc" aria-label="Back to all cameras">' + ic('left') + '<span>All cameras</span></button>' +
        '<button class="btn" id="tlLive" title="Jump to now and keep playing live on the timeline  End" aria-label="Jump to live">' + ic('live') + '<span>Live</span></button>' +
        '<button class="btn icon" id="tlFs" aria-pressed="' + tvOn() + '" title="Full screen, with these controls  F" aria-label="Full screen">' + ic('expand') + '</button>' +
        '<button class="btn icon tlmore" id="tlMore" aria-expanded="false" title="More: zoom, alert types, go to a date, mark a range" aria-label="More timeline controls">' + ic('more') + '</button>' +
        '<button class="btn icon" id="tlClose" title="Close the timeline (Esc, Back, T)" aria-label="Close the timeline">' + ic('x') + '</button>' +
      '</div>' +
    '</div>' +
    '<div class="tlbar tlbar2">' +
      '<div class="tlgrp">' +
        '<button class="btn icon" id="tlZoomOut" title="Zoom out  -" aria-label="Zoom out">' + ic('minus') + '</button>' +
        '<button class="btn icon" id="tlZoomIn" title="Zoom in  +  (closest: 10 seconds)" aria-label="Zoom in">' + ic('plus') + '</button>' +
        '<div class="tlchips" id="tlChips" role="group" aria-label="Show this much time">' +
          CHIPS.map(([l, ms]) => '<button class="btn tlchip" data-ms="' + ms + '" title="Show the last ' + l + ' around the playhead" aria-label="Show ' + l + '">' + l + '</button>').join('') +
        '</div>' +
      '</div>' +
      '<label class="tlgoto" title="Jump to a date and time"><span>Go to</span><input type="datetime-local" step="1" id="tlGoto" aria-label="Go to date and time"><button type="button" class="btn icon" id="tlCal" aria-haspopup="dialog" aria-expanded="false" title="Pick a day: days with recordings are tinted, days with alerts have a dot" aria-label="Pick a day from a calendar">' + ic('cal') + '</button></label>' +
      '<div class="tlgrp tlright">' +
        '<div class="tlkinds" id="tlKinds" role="group" aria-label="Show only these alert types">' +
          KIND_ORDER.map(k => '<button class="btn icon tlkind k-' + k + '" data-k="' + k + '" aria-pressed="false" title="Show only ' +
            (k === 'other' ? 'unrecognized' : KINDS[k].label.toLowerCase()) + ' alerts (click again to show all)" aria-label="' + KINDS[k].label + ' alerts">' + kIcon(k) + '</button>').join('') +
        '</div>' +
        '<button class="btn tlmk" id="tlIn" title="Mark range start at the playhead  I" aria-label="Mark range start">In</button>' +
        '<button class="btn tlmk" id="tlOut" title="Mark range end at the playhead  O  (or Shift + drag on the strip)" aria-label="Mark range end">Out</button>' +
        '<button class="btn icon tlmk" id="tlClr" title="Clear the range  Esc" aria-label="Clear the range" hidden>' + ic('x') + '</button>' +
      '</div>' +
    '</div>' +
    '<div class="tlarea" id="tlArea" tabindex="0" aria-label="Timeline: click to play from there, drag to scrub, drag the time ruler to move, scroll or pinch to zoom, Shift and drag to select a range">' +
      '<div class="tlticks" id="tlTicks"></div><div class="tlspans" id="tlSpans"></div><div class="tlmarks" id="tlMarks"></div><div id="tlSigs"></div>' +
      '<div class="tlrange" id="tlRange" hidden><span class="tlrh" data-w="a" title="Drag to move the start"></span><span class="tlrh" data-w="b" title="Drag to move the end"></span></div>' +
      '<button class="tlexp" id="tlExport" hidden title="Save this range as MP4  E">Export</button>' +
      '<div class="tlnow" id="tlNow" hidden><span>Live</span></div>' +
      '<div class="tlhead" id="tlHead"><span id="tlHeadT"></span></div>' +
      '<div class="tlload" aria-hidden="true"></div>' +
    '</div>';
  $('tlPrev').onclick = () => stepAlert(-1);
  $('tlNext').onclick = () => stepAlert(1);
  $('tlPlay').onclick = () => setPlaying(!S.playing);
  $('tlSpeed').onchange = (e) => { setSpeed(+e.target.value); e.target.blur(); };
  for (const b of $('tlKinds').children) {
    b.onclick = () => {
      const k = b.dataset.k;
      if (kindsOn.has(k)) kindsOn.delete(k); else kindsOn.add(k);
      for (const x of $('tlKinds').children) { const on = kindsOn.has(x.dataset.k); x.classList.toggle('on', on); x.setAttribute('aria-pressed', String(on)); }
      draw();
    };
  }
  $('tlBoxes').onclick = () => setBoxes(!S.boxes);
  $('tlZoomIn').onclick = () => zoomAt(S.playhead, 0.5);
  for (const b of $('tlChips').children) b.onclick = () => viewWidth(+b.dataset.ms);
  $('tlIn').onclick = markIn;
  $('tlOut').onclick = markOut;
  $('tlClr').onclick = () => clearRange();
  // Typing a date: the browser reports a change at every digit, so go there only on Enter or
  // when leaving the box (and keep the keys in the box meanwhile, away from the shortcuts).
  let gotoWas = '';
  const goTo = (box) => {
    if (box.value === gotoWas) return;
    gotoWas = box.value;
    const t = new Date(box.value).getTime();
    if (!(t > 0)) return;
    seek(t, true);
    note('Went to ' + fmtWall(t));
  };
  $('tlGoto').onfocus = (e) => { gotoWas = e.target.value; };
  $('tlGoto').onkeydown = (e) => {
    e.stopPropagation();
    if (e.key === 'Enter') { e.preventDefault(); goTo(e.target); e.target.blur(); }
    else if (e.key === 'Escape') { e.preventDefault(); e.target.value = gotoWas; e.target.blur(); }
  };
  $('tlGoto').onblur = (e) => goTo(e.target);
  // A date picked in the browser's own picker (where it still shows one) goes at once.
  $('tlGoto').oninput = (e) => { if (document.activeElement !== e.target) goTo(e.target); };
  $('tlCal').onclick = (e) => {
    e.preventDefault();
    openCalendar({
      anchor: $('tlCal'),
      selected: S.playhead,
      cameras: coverCams,   // this server's (another server's recordings are not here)
      // The same time of day on the picked day.
      onPick: (y, m, d) => {
        const p = new Date(S.playhead);
        const t = new Date(y, m, d, p.getHours(), p.getMinutes(), p.getSeconds()).getTime();
        seek(Math.min(t, Date.now()), true);
        note('Went to ' + fmtWall(Math.min(t, Date.now())));
      },
    });
  };
  $('tlZoomOut').onclick = () => zoomAt(S.playhead, 2);
  $('tlBack').onclick = () => setSolo(null);
  $('tlLive').onclick = () => jumpLive();
  $('tlClose').onclick = () => leave();
  $('tlFs').onclick = () => { if (tvOn()) exitTv(); else enterTv(); };
  $('tlMore').onclick = () => setMore(!$('tlDock').classList.contains('more'));
  initPointer();
  initMarkers();
  new ResizeObserver(() => draw()).observe($('tlArea'));
  updateControls();
}

// Tablets: the second row (zoom, alert types, go to, In / Out) opens above the
// one-row dock from its More button.
function setMore(on) {
  const d = $('tlDock');
  d.classList.toggle('more', on);
  $('tlMore').setAttribute('aria-expanded', String(on));
  wake();
}

// "Timeline controls: Auto-hide" (account menu, per device): the dock floats over
// the cameras and fades out after a few seconds of playing untouched; a tap
// brings it back (and does nothing else).
const HIDE_MS = 4000;
const autoHide = () => document.documentElement.dataset.tlctl === 'autohide';
let idleTimer = 0;
function wake() {
  document.body.classList.remove('tlidle');
  clearTimeout(idleTimer);
  if (!S.on || !autoHide()) return;
  idleTimer = setTimeout(() => {
    const d = $('tlDock');
    if (!S.on || !autoHide() || !S.playing) return;
    const busy = d.classList.contains('more') || S.scrubbing || d.querySelector(':focus-visible, select:focus') ||
      (document.documentElement.dataset.touch !== '1' && d.matches(':hover')) || (popEl && !popEl.hidden);
    if (busy) { wake(); return; }
    document.body.classList.add('tlidle');
  }, HIDE_MS);
}
function initAutoHide() {
  let swallow = false, swallowT = 0;
  const swallowClick = () => { swallow = true; clearTimeout(swallowT); swallowT = setTimeout(() => { swallow = false; }, 700); };
  const poke = (ev) => {
    if (!S.on) return;
    if (ev.type === 'pointermove' && ev.pointerType !== 'mouse') return;
    // The tap that brings the controls back is not also a tap on a camera.
    if (ev.type === 'pointerdown' && document.body.classList.contains('tlidle')) swallowClick();
    wake();
  };
  for (const t of ['pointerdown', 'pointermove', 'keydown', 'wheel']) document.addEventListener(t, poke, { capture: true, passive: true });
  document.addEventListener('click', (ev) => { if (swallow) { swallow = false; ev.stopPropagation(); ev.preventDefault(); } }, true);
  document.addEventListener('pointerdown', (ev) => {
    const d = $('tlDock');
    // (A tap beside the open More row only closes it.)
    const inside = ev.composedPath().some(el => el === d || (el instanceof Element && el.matches('.tlpop, .tltip, .cal')));
    if (d && d.classList.contains('more') && !inside) { setMore(false); swallowClick(); }
  }, true);
  window.addEventListener('ambs-tlctl', wake);
}

function updateControls() {
  const pl = $('tlPlay');
  if (!pl) return;
  pl.innerHTML = ic(S.playing ? 'pause' : 'play');
  pl.setAttribute('aria-label', S.playing ? 'Pause' : 'Play');
  $('tlSpeed').value = String(S.speed);
  $('tlSpeed').classList.toggle('fast', S.speed !== 1);
  const solo = S.solo !== null;
  $('tlBack').hidden = !solo;
  $('tlDock').classList.toggle('solo', solo);
}

export function setSolo(id) {
  S.solo = id;
  document.body.classList.remove('tlpeek');
  if (id === null) { S.fromFull = null; S.fromLive = false; }
  Live.setSolo(id);
  updateControls();
  draw();
}

// Out of the one-camera view: to the camera's full view when it was opened
// from there, else to all cameras on the timeline.
function soloBack() {
  if (S.fromFull !== null) leave();
  else if (S.fromLive) exit();
  else setSolo(null);
}

// ---------- enter / exit ----------
const hook = {
  mount: mountPlayer,
  tileClick(id, ev) {
    if (S.solo === null) { setSolo(id); return; }
    // Solo: a click on the black around the video (or beside the tile) goes
    // back to where the camera was opened from.
    if (ev && ev.target && ev.target.id === 'camGrid') { soloBack(); return; }
    const v = ev && document.querySelector('#camGrid .tlp video:not([hidden])');
    if (!v || !v.videoWidth) return;
    const r = v.getBoundingClientRect();
    const sc = Math.min(r.width / v.videoWidth, r.height / v.videoHeight);
    const w = v.videoWidth * sc, h = v.videoHeight * sc;
    const x = ev.clientX - (r.left + (r.width - w) / 2), y = ev.clientY - (r.top + (r.height - h) / 2);
    if (x < 0 || y < 0 || x > w || y > h) soloBack();
  },
  sound: setSound,
  solo: setSolo,
  shown(ids) {
    if (ids.join() === S.shown.join()) return;
    S.shown = ids;
    // A camera whose alerts are not loaded yet: load them.
    if (S.on && D.evCams && shownNames().some(n => !D.evCams.has(n))) scheduleFetch(true);
    draw();
  },
};

let tickTimer = 0, pollTimer = 0;

// Publish the dock's height as --tl-dock-h so floating panels (the server status
// pod) sit above it instead of covering its controls. 0 while the dock is hidden.
let dockRO = null;
function trackDockHeight() {
  const dock = $('tlDock');
  const root = document.documentElement.style;
  const set = () => {
    root.setProperty('--tl-dock-h', (dock.hidden ? 0 : dock.getBoundingClientRect().height) + 'px');
    // (Auto-hide on a desktop: the floating dock spans the cameras' column, not the panel.)
    const r = dock.parentElement.getBoundingClientRect();
    root.setProperty('--tl-l', Math.round(r.left) + 'px');
    root.setProperty('--tl-w', Math.round(r.width) + 'px');
  };
  if (!dockRO && window.ResizeObserver) { dockRO = new ResizeObserver(set); dockRO.observe(dock); dockRO.observe(dock.parentElement); window.addEventListener('resize', set); }
  set();
}
export async function enter(opts) {
  opts = opts || {};
  if (S.on) return;
  if (!S.inited) { S.inited = true; buildDock(); }
  S.on = true;
  $('tlDock').hidden = false;
  trackDockHeight();
  wake();
  $('tlToggle').classList.add('on');
  $('tlToggle').setAttribute('aria-pressed', 'true');
  const now = Date.now();
  S.speed = S.speed || 1;
  S.playing = true;
  const at = opts.at ? Math.min(opts.at, now - 5000) : now - 60000;
  // At most a tenth of the view past now, as setView allows (else the first
  // drag or zoom would snap the strip back).
  S.view = opts.at && at < now - 45 * 60000 ? { from: at - 30 * 60000, to: at + 30 * 60000 } : { from: now - 54 * 60000, to: now + 6 * 60000 };
  setPlayhead(at);
  S.lastSec = -1;
  S.fromFull = opts.solo != null && opts.fromFull ? opts.solo : null;
  S.fromLive = opts.solo != null && !!opts.fromLive;
  setOneQuiet(null);   // (In full screen the timeline says what shows from now on.)
  Live.setTimelineHook(hook);
  if (opts.solo != null) { S.solo = opts.solo; Live.setSolo(opts.solo); }
  updateControls();
  if (opts.live) jumpLive();
  overlaysChanged();
  tickTimer = setInterval(() => { if (isActive('live') && !document.hidden) syncAll(); }, 250);
  // New recordings: every 5 s while now is in view (the bar keeps up), else 15 s.
  let polls = 0;
  pollTimer = setInterval(() => {
    if (!isActive('live') || document.hidden || S.live) return;
    if (S.view.to >= Date.now() - 60000 || ++polls % 3 === 0) pollData();
  }, LIVE_POLL_MS);
  requestAnimationFrame(loop);
  await loadData();
  // Start a little before the latest recording of what is on screen.
  if (S.on && opts.solo == null && S.playhead > now - 61000) {
    let last = 0;
    for (const n of shownNames()) for (const s of (D.spans[n] || [])) last = Math.max(last, s[1]);
    if (last) { setPlayhead(Math.min(last - 20000, now - 20000)); setView(S.playhead - 40 * 60000, S.playhead + 20 * 60000); }
    S.anchorWall = performance.now();
    draw();
    syncAll();
  }
}

export function exit() {
  closeCalendar(false);
  document.body.classList.remove('tlpeek');
  document.body.classList.remove('tlidle'); clearTimeout(idleTimer);
  if (S.inited) setMore(false);
  hidePreview();
  for (const st of strips.values()) if (st.img) URL.revokeObjectURL(st.img.src);
  strips.clear();
  stripWant.clear();
  stripFailed.clear();
  if (!S.on) return;
  S.on = false;
  SND.on = false;
  leaveLive();
  players.forEach(stopLive);
  clearInterval(tickTimer); clearInterval(pollTimer); clearTimeout(S.fetchTimer);
  hideTip(); hideCovTip(); closePop(); clearRange(); setLoading(false);
  $('tlDock').hidden = true;
  trackDockHeight();
  $('tlToggle').classList.remove('on');
  $('tlToggle').setAttribute('aria-pressed', 'false');
  // Closed in full screen: stays there, live, on the camera it was showing.
  setOneQuiet(S.solo);
  S.solo = null; S.shown = [];
  Live.setTimelineHook(null);
  overlaysChanged();
}
// The strip's Live button: back to the full view of the camera we came from, else the grid.
// The strip's Live button: now, on the timeline, with now in view.
function jumpLive() {
  const w = S.view.to - S.view.from, now = Date.now();
  if (S.view.to < now || S.view.from > now - w * 0.5) setView(now - w * 0.85, now + w * 0.15);
  S.headVisible = true;
  goLive();
  draw();
}
function leave() {
  const f = S.fromFull;
  exit();
  if (f !== null && !tvOn()) Live.openFullCam(f);
}
// From a camera's full view or the dock: this camera alone on the timeline, starting a minute
// before now (or 2 s before the alert the full view was opened from).
export async function enterFromFull() {
  const id = Live.dockCam();
  if (id === null) return;
  // Another server's camera: its recordings are on that server.
  const rc = camFromId(id);
  if (rc && rc.remote) { toast('Recordings of ' + rc.remote.camera + ' are on ' + rc.remote.site + ': right-click the picture > Open on ' + rc.remote.site); return; }
  const wasFull = Live.fullIsOpen();
  const alertAt = Live.fullAlertAt();
  const at = alertAt ? alertAt - LEAD : 0;
  Live.closeFull();
  if (S.on) {
    S.fromFull = wasFull ? id : null;
    setSolo(id);
    S.fromFull = wasFull ? id : null;
    if (at) seek(at, true);
    return;
  }
  await enter({ solo: id, at, fromFull: wasFull });
}
// Play one camera's recording from just before `ms` (a search result).
export async function openAt(id, ms) {
  if (S.speed !== 1 && S.on) setSpeed(1);
  else S.speed = 1;
  if (S.on) { setSolo(id); seek(ms - LEAD, true); return; }
  await enter({ solo: id, at: ms - LEAD });
}
// (From full screen of one camera: that camera's timeline.)
export const toggle = () => (S.on ? leave() : enter(tvOn() && tvOne() !== null ? { solo: tvOne() } : undefined));
export const isOn = () => S.on;

// Escape closes the alert list, then clears the range; only then leaves (the
// listener in initLiveTimeline). One keypress does one of those.
const escDone = new WeakSet();
function clearTransient(ev) {
  const did = closePop() || clearRange();
  if (did && ev) escDone.add(ev);
  return did;
}

// J / L: through reverse and forward speeds, as video editors do. Paused: starts
// playing at 1x either way.
function ladder() { return (PIC_OK ? FAST_REVERSE.slice().sort((a, b) => a - b) : []).concat([-4, -2, -1], [1, 2, 4, 8, 16, 32, 64, 128, 256]); }
function shuttle(dir) {
  if (S.live && dir > 0) { note('Already live'); return; }
  const lad = ladder();
  let n;
  if (!S.playing) n = dir > 0 ? 1 : -1;
  else if (dir > 0) n = lad.find(x => x > S.speed) || lad[lad.length - 1];
  else n = lad.slice().reverse().find(x => x < S.speed) || lad[0];
  setSpeed(n);
  if (!S.playing) setPlaying(true);
  note(speedLabel(n));
}

// Keys while timeline mode is on (Live page, no full screen). Returns true when handled.
export function timelineKey(ev) {
  if (overlayOpen(ev)) return false;
  const k = ev.key;
  const tg = ev.target;
  const typing = tg && (tg.tagName === 'INPUT' || tg.tagName === 'TEXTAREA' || tg.tagName === 'SELECT' || tg.isContentEditable);
  if (ev.ctrlKey || ev.metaKey || ev.altKey) return false;
  if (!S.on) {
    if ((k === 'l' || k === 'L') && !typing) { toggle(); return true; }
    return false;
  }
  if (k === 'Escape') return clearTransient(ev);
  if (typing) return false;
  const lk = k.length === 1 ? k.toLowerCase() : k;
  if (k === ',' || k === '<') { stepAlert(-1); return true; }
  if (k === '.' || k === '>') { stepAlert(1); return true; }
  if (lk === 'k' || (k === ' ' && !(tg && tg.tagName === 'BUTTON'))) { setPlaying(!S.playing); return true; }
  if (lk === 'j') { shuttle(-1); return true; }
  if (lk === 'l') { shuttle(1); return true; }
  if (lk === 'i') { markIn(); return true; }
  if (lk === 'o') { markOut(); return true; }
  if (lk === 'e') { exportRange(); return true; }
  if (lk === 't') { leave(); return true; }
  if (k === 'End') { jumpLive(); return true; }
  if (k === 'ArrowLeft' || k === 'ArrowRight') {
    const dir = k === 'ArrowLeft' ? -1 : 1;
    // Paused: Shift steps one frame (about 1/15 s: the video is seeked by time,
    // there is no frame counter); playing, Shift is a minute.
    if (ev.shiftKey && !S.playing) { seek(S.playhead + dir * 67, true); return true; }
    seek(S.playhead + dir * (ev.shiftKey ? 60000 : 10000), true);
    return true;
  }
  if (k === '[' || k === ']') {
    const i = SPEEDS.indexOf(S.speed);
    const n = SPEEDS[Math.max(0, Math.min(SPEEDS.length - 1, (i < 0 ? SPEEDS.indexOf(1) : i) + (k === ']' ? 1 : -1)))];
    setSpeed(n); note(speedLabel(n)); return true;
  }
  if (k === '+' || k === '=') { zoomAt(S.playhead, 0.5); return true; }
  if (k === '-' || k === '_') { zoomAt(S.playhead, 2); return true; }
  return false;
}

export function initLiveTimeline() {
  $('tlToggle').onclick = toggle;
  $('fullTl').onclick = () => enterFromFull();
  // Ways out besides the Timeline button: Esc (a solo camera first goes back to
  // all cameras), picking All / a layout / a group, or clicking the empty space
  // around the tiles.
  window.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Escape' || !S.on || !isActive('live')) return;
    if (escDone.has(ev)) return;
    if ($('lightbox').style.display === 'flex' || document.querySelector('.dlg')) return;
    if (clearTransient(ev)) return;
    if (S.solo !== null) soloBack(); else exit();
  });
  // In full screen Esc goes to fullscreen.js first: a popup or marked range still closes before it leaves.
  setEscFirst((ev) => S.on && clearTransient(ev));
  bus.on('view-pick', () => { if (S.on) exit(); });
  bus.on('timeline-for', (id) => { Live.openFullCam(id); enterFromFull(); });
  // Back: one camera returns where it came from; all cameras close the timeline.
  backLayer(10, () => S.on, () => { if (S.solo !== null) soloBack(); else exit(); });
  // Tapping Live in the menu while on Live: out of the timeline and the full view.
  let wasLive = false;
  $('nav').addEventListener('click', (ev) => { wasLive = isActive('live'); }, true);
  $('nav').addEventListener('click', (ev) => {
    const b = ev.target.closest && ev.target.closest('button[data-tab="live"]');
    if (!b || !wasLive) return;
    if (Live.fullIsOpen()) Live.closeFull();
    if (S.on) exit();
  });
  // A double-click set to open the timeline: this camera alone, at live;
  // clicking beside it returns to the cameras.
  bus.on('timeline-live', (id) => { if (!S.on) enter({ solo: id, live: true, fromLive: true }); });
  // (Read the solo state before other click handlers run: a click beside a solo
  // camera returns to all cameras and must not also leave the timeline.)
  let soloAtClick = false;
  $('tab-live').addEventListener('click', () => { soloAtClick = S.solo !== null; }, true);
  // A phone on its side, one camera: a tap on the picture hides the timeline
  // (the picture gets the whole screen) and another brings it back.
  const sideways = window.matchMedia('(max-height: 500px) and (pointer: coarse)');
  $('tab-live').addEventListener('click', (ev) => {
    // (Auto-hide does this by itself.)
    if (!S.on || S.solo === null || !sideways.matches || autoHide()) return;
    const path = ev.composedPath();
    if (!path.some(el => el instanceof Element && el.matches('.grid[data-per="solo"] .tile')) || path.some(el => el instanceof Element && el.matches('button, a, input, select, label'))) return;
    document.body.classList.toggle('tlpeek');
  });
  sideways.addEventListener('change', () => document.body.classList.remove('tlpeek'));
  $('tab-live').addEventListener('click', (ev) => {
    if (!S.on) return;
    // Judge by where the click happened (its path when it was dispatched):
    // a button that redraws itself when clicked (Play/Pause) is no longer in
    // the page by now, and must not count as a click outside the timeline.
    const inside = ev.composedPath().some(el => el instanceof Element && el.matches('.tile, #tlDock, .livebar, button, a, input, select, label, .dlg, .tltip, .tlpop'));
    if (inside || !ev.target.isConnected) return;
    // Beside one camera: back where it came from; beside the grid: out of
    // the timeline.
    if (soloAtClick) { if (S.solo !== null && ev.target.id !== 'camGrid') soloBack(); return; }
    exit();
  });
  initAutoHide();
  bus.on('signedout', exit);
}

// For diagnosing playback from the browser console: the timeline's state
// and each tile's two video slots (read only).
export function debugState() {
  return {
    playhead: S.playhead, playing: S.playing, scrubbing: S.scrubbing,
    device: DEV, budget: G.budget, scale: G.scale,
    tiles: [...players].map(p => ({ cam: p.cam.name, tier: p.tier, pen: p.hl ? p.hl.pen : 0, why: p.hl ? p.hl.why : '', rate: p.hl ? p.hl.rate : 0, streams: CAMQ.get(p.cam.name), picFail: !!p.picFail, pic: p.pic && { wall: p.pic.wall, shown: !p.pic.cv.hidden, lat: Math.round(p.pic.lat), none: p.pic.none }, cur: p.cur, shown: p.shownIdx, failUntil: p.failUntil - Date.now(), noUpTo: p.noUpTo,
      slots: p.slots.map(s => ({ loading: s.loading, ready: s.ready, q: s.q, up: s.up, sub: s.sub, start: s.start - S.playhead, end: s.end - S.playhead, reqFrom: s.reqFrom - S.playhead, rs: s.v.readyState, seeking: s.v.seeking, err: s.v.error && s.v.error.code })) })),
  };
}
