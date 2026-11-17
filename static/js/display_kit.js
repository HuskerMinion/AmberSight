// display_kit.js - the pure parts of display mode (display.js): its settings, which cameras it
// shows, how the grid fits the screen, the night-dimming window, what makes a camera go big, and
// which cameras are big now. No DOM, no imports: scripts/test-display.mjs tests them in node.

export const MAX_TILES = 16;

export const DEFAULTS = {
  source: 'all',        // 'all' | 'layout:<id>' | 'group:<name>' | 'pick'
  cams: [],             // camera names, for 'pick'
  trigger: 'alerts',    // 'alerts' (AI-confirmed) | 'motion' (any motion) | 'off'
  hold: 20,             // seconds a camera stays big after its activity ends
  multi: 'two',         // several at once: 'two' side by side | 'newest' only
  grid: 'light',        // grid pictures: 'light' (JPEG frames) | 'sub' (sub stream video)
  big: 'sub',           // big view: 'sub' | 'main'
  clock: true, armed: true, names: true,
  dim: false, dimFrom: '22:00', dimTo: '07:00', dimLevel: 70, dimWake: true,
  nightly: true,        // reload the page once a night (03:30)
};

const pick1 = (v, opts, d) => (opts.includes(v) ? v : d);
const hhmm = (v, d) => (/^([01]\d|2[0-3]):[0-5]\d$/.test(String(v)) ? String(v) : d);
const clampInt = (v, lo, hi, d) => { const n = parseInt(v, 10); return Number.isFinite(n) ? Math.min(hi, Math.max(lo, n)) : d; };
const bool = (v, d) => (typeof v === 'boolean' ? v : d);

// Settings as stored (anything missing or wrong falls back to the default).
export function cleanSettings(raw) {
  const r = raw && typeof raw === 'object' ? raw : {};
  const D = DEFAULTS;
  const source = typeof r.source === 'string' && (r.source === 'all' || r.source === 'pick' || /^(layout|group):./.test(r.source)) ? r.source : D.source;
  return {
    source,
    cams: Array.isArray(r.cams) ? r.cams.filter(x => typeof x === 'string').slice(0, 64) : [],
    trigger: pick1(r.trigger, ['alerts', 'motion', 'off'], D.trigger),
    hold: clampInt(r.hold, 5, 600, D.hold),
    multi: pick1(r.multi, ['two', 'newest'], D.multi),
    grid: pick1(r.grid, ['light', 'sub'], D.grid),
    big: pick1(r.big, ['sub', 'main'], D.big),
    clock: bool(r.clock, D.clock), armed: bool(r.armed, D.armed), names: bool(r.names, D.names),
    dim: bool(r.dim, D.dim), dimFrom: hhmm(r.dimFrom, D.dimFrom), dimTo: hhmm(r.dimTo, D.dimTo),
    dimLevel: clampInt(r.dimLevel, 20, 95, D.dimLevel), dimWake: bool(r.dimWake, D.dimWake),
    nightly: bool(r.nightly, D.nightly),
  };
}

const camGroups = (c) => ((c.groups && c.groups.length) ? c.groups : (c.group ? [c.group] : []));

// Put `list` (cameras) in the order of `names` (camera names); the others after, as they were.
function ordered(list, names) {
  if (!Array.isArray(names) || !names.length) return list.slice();
  const at = new Map(names.map((n, i) => [n, i]));
  return list.map((c, i) => [c, at.has(c.name) ? at.get(c.name) : names.length + i]).sort((a, b) => a[1] - b[1]).map(x => x[0]);
}

// The cameras the display shows, in order, at most MAX_TILES. layouts: the account's saved
// layouts ({id, name, cams, group?}); orders: the shared orders ({all: [names], 'group:<g>': [names]}).
export function chooseCams(s, cams, layouts, orders) {
  const all = (cams || []).filter(c => c && typeof c.id === 'number');
  const o = orders || {};
  let out;
  if (s.source.startsWith('layout:')) {
    const l = (layouts || []).find(x => x && x.id === s.source.slice(7));
    if (!l) out = ordered(all, o.all);
    else if (l.group) out = ordered(all.filter(c => camGroups(c).includes(l.group)), l.cams.length ? l.cams : o['group:' + l.group]);
    else if (!l.cams.length) out = ordered(all, o.all);
    else { const by = new Map(all.map(c => [c.name, c])); out = l.cams.map(n => by.get(n)).filter(Boolean); }
  } else if (s.source.startsWith('group:')) {
    const g = s.source.slice(6);
    out = ordered(all.filter(c => camGroups(c).includes(g)), o['group:' + g]);
  } else if (s.source === 'pick') {
    const by = new Map(all.map(c => [c.name, c]));
    out = s.cams.map(n => by.get(n)).filter(Boolean);
  } else out = ordered(all, o.all);
  return out.slice(0, MAX_TILES);
}

// Columns and rows for n tiles of aspect `ar` (width / height) in a w x h area: the split that
// gives the biggest tiles.
export function fitGrid(n, w, h, ar) {
  if (n <= 0 || w <= 0 || h <= 0) return { cols: 1, rows: 1 };
  const a = ar || 16 / 9;
  let best = { cols: 1, rows: n, size: 0 };
  for (let cols = 1; cols <= n; cols++) {
    const rows = Math.ceil(n / cols);
    const tw = Math.min(w / cols, (h / rows) * a);
    // (Ties: fewer empty cells, then side by side on a wide screen.)
    const tie = Math.abs(tw - best.size) <= 0.5;
    if (tw > best.size + 0.5 || (tie && cols * rows < best.cols * best.rows) || (tie && cols * rows === best.cols * best.rows && w >= h)) best = { cols, rows, size: tw };
  }
  return { cols: best.cols, rows: best.rows };
}

// Is the local time `mins` (minutes since midnight) in the window from-to ("HH:MM")? A window
// that passes midnight (22:00-07:00) works; from == to is never.
export function inWindow(mins, from, to) {
  const m = (t) => { const [h, mm] = t.split(':').map(Number); return h * 60 + mm; };
  const a = m(from), b = m(to);
  if (a === b) return false;
  return a < b ? mins >= a && mins < b : mins >= a || mins < b;
}

// What a camera is doing that sends it big, or null. now: ms; rxAge: ms since the camera list came.
// Alerts: an AI detection the server counted (ai_last: shown for 5 s, and for up to 2 minutes
// while the motion it was found in goes on); a camera the AI does not look at (ai_active false:
// AI off, or off for that camera) falls back to its motion. Motion: any motion. Either way a clip started by hand
// or through the API (Record now) counts: that is how an automation shows a camera on purpose.
const AI_HOLD = 5000, AI_EVENT_HOLD = 120000;
const AI_CLASS = { person: 'person', people: 'person', face: 'face', car: 'vehicle', truck: 'vehicle', bus: 'vehicle', motorcycle: 'vehicle', bicycle: 'vehicle', vehicle: 'vehicle', dog: 'animal', cat: 'animal', bird: 'animal', animal: 'animal', package: 'package' };
export function triggerOf(c, trigger, now, rxAge) {
  if (trigger === 'off' || !c || c.enabled === false) return null;
  const a = c.ai_last;
  if (a && a.label && typeof a.age_ms === 'number') {
    const age = a.age_ms + (rxAge || 0);
    if (age < AI_HOLD || (c.motion && age < AI_EVENT_HOLD)) {
      const label = String(a.label).toLowerCase();
      return { cls: AI_CLASS[label] || 'package', text: label.charAt(0).toUpperCase() + label.slice(1) };
    }
  }
  if (typeof c.manual_until === 'number' && c.manual_until * 1000 > now) return { cls: 'clip', text: 'Recording' };
  if ((trigger === 'motion' || c.ai_active === false) && (c.motion || c.push_motion)) return { cls: 'motion', text: 'Motion' };
  return null;
}

// Update the activity map (camera id -> {since, until, live, reason, dismissed}) from the
// cameras shown now. A camera goes big when its activity starts and stays until `hold` seconds
// after it ends. Dismissed by hand: not again until its activity has stopped.
export function updateActivity(act, shown, s, now, rxAge) {
  const seen = new Set();
  for (const c of shown) {
    seen.add(c.id);
    const r = triggerOf(c, s.trigger, now, rxAge);
    let a = act.get(c.id);
    if (r) {
      if (!a || (!a.live && a.until <= now)) { a = { since: now, until: 0, live: true, reason: r, dismissed: false }; act.set(c.id, a); }
      a.live = true; a.reason = r;
      if (!a.dismissed) a.until = now + s.hold * 1000;
    } else if (a) {
      a.live = false;
      if (a.dismissed || a.until <= now) act.delete(c.id);
    }
  }
  for (const id of [...act.keys()]) if (!seen.has(id)) act.delete(id);
}

// The cameras big now, newest first: one, or two side by side ('two'). manual: {id, until} or null.
export function bigNow(act, s, now, manual) {
  let ids = [...act].filter(([, a]) => a.until > now).sort((x, y) => y[1].since - x[1].since).map(x => x[0]);
  if (manual && manual.until > now) ids = [manual.id].concat(ids.filter(i => i !== manual.id));
  return ids.slice(0, s.multi === 'two' ? 2 : 1);
}

// Burn-in: where the overlays sit now (a small step every few minutes, round a 3 x 3 square).
export function shiftAt(step) {
  const P = [[0, 0], [6, 0], [6, 6], [0, 6], [-6, 6], [-6, 0], [-6, -6], [0, -6], [6, -6]];
  return P[((step % P.length) + P.length) % P.length];
}

// A camera's privacy masks as a short key (revision, level and the boxes): when
// it changes, the display starts that camera's players again.
export function maskSig(c) {
  if (!c) return '';
  return (c.mask_rev || '') + '|' + (c.mask_mode || '') + '|' + JSON.stringify(c.privacy_masks || []);
}
