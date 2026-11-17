// follow.js - "Where did they go?": the same person or vehicle on other cameras around an event,
// as a time-ordered path. It compares CLIP fingerprints of the objects AI found (smart search),
// raised by a matching recognized face or plate. Approximate: the match slider hides weak ones.

import { state, api, esc, openDlg, toast, enc } from './core.js';
import { showTab } from './nav.js';

const KEY = 'ambs.followMin';
const WINDOWS = [[120, '2 minutes'], [300, '5 minutes'], [600, '10 minutes'], [1800, '30 minutes'], [3600, '1 hour'], [10800, '3 hours']];

const errText = (e) => (e && e.body) ? (() => { try { return JSON.parse(e.body).error || e.body; } catch (x) { return e.body; } })() : (e && e.message) || String(e);
const clock = (ms) => new Date(ms).toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit', second: '2-digit' });
const day = (ms) => new Date(ms).toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' });
const gap = (ms) => {
  const s = Math.round(Math.abs(ms) / 1000), m = Math.floor(s / 60);
  const t = m >= 60 ? Math.floor(m / 60) + ' h ' + (m % 60) + ' min' : m ? m + ' min ' + (s % 60) + ' s' : s + ' s';
  return (ms < 0 ? '' : '+') + (ms < 0 ? '-' : '') + t;
};
const pctOf = (s) => Math.round(s * 100);
const band = (s) => s >= 0.9 ? 'hi' : s >= 0.8 ? 'mid' : 'lo';

function savedMin(def) {
  try { const v = parseFloat(localStorage.getItem(KEY)); if (v >= 0.6 && v <= 1) return v; } catch (e) { /* ignore */ }
  return def;
}

// Play the recording of a camera at a moment (as a smart search result does).
function playAt(camName, ms, dlg) {
  const cam = state.cams.find(c => c.name === camName);
  if (!cam) { toast(camName + ' is no longer a camera here'); return; }
  if (cam.remote) { toast(camName + ': its recordings are on ' + ((cam.remote && cam.remote.site) || 'the other server')); return; }
  if (dlg) dlg.__close();
  showTab('live');
  import('./livetl.js').then(m => m.openAt(cam.id, ms));
}

function picHtml(s) {
  const b = s.box;
  return '<span class="fw-pic"><img loading="lazy" src="/api/smart/pic/' + s.pic + '" alt="">' +
    (b ? '<span class="smq-box" style="left:' + (b[0] * 100) + '%;top:' + (b[1] * 100) + '%;width:' + (b[2] * 100) + '%;height:' + (b[3] * 100) + '%"></span>' : '') + '</span>';
}

// Open the view for an event; `pic` picks the person or vehicle in it (default: the largest).
export function openFollow(eventId, pic) {
  const d = openDlg('Where did they go?', '<div class="fw"><p class="empty">Looking...</p></div>');
  d.style.width = 'min(720px, 96vw)';
  const root = d.querySelector('.fw');
  const st = { event: eventId, pic: pic || null, win: 600, same: false, min: savedMin(0.8), data: null, back: [] };
  let gen = 0;

  async function load() {
    const g = ++gen;
    const old = root.querySelector('.fw-list');
    if (old) old.style.opacity = '.5';
    let r;
    try {
      r = await api('/api/smart/follow?event=' + enc(st.event) + (st.pic ? '&pic=' + st.pic : '') + '&before=' + st.win + '&after=' + st.win + (st.same ? '&same_camera=1' : ''));
    } catch (e) {
      if (g !== gen) return;
      root.innerHTML = '<p class="empty">' + esc(errText(e)) + '</p>' + (st.back.length ? '<div class="row"><button class="btn" data-back>Back</button></div>' : '');
      wireBack();
      return;
    }
    if (g !== gen) return;
    st.data = r;
    if (st.min < r.floor) st.min = r.floor;
    paint(true);
  }

  function wireBack() {
    const b = root.querySelector('[data-back]');
    if (b) b.onclick = () => { const p = st.back.pop(); if (p) { st.event = p.event; st.pic = p.pic; load(); } };
  }

  function paint(full) {
    const r = st.data;
    if (full) {
      const src = r.source;
      const kind = /^(person|people|pedestrian)$/i.test(src.label) ? 'person' : 'vehicle';
      root.innerHTML =
        '<div class="fw-src">' + picHtml(src) + '<div><b>Following the ' + esc(src.label) + '</b> seen on ' + esc(src.camera) + '<br><small>' + esc(day(src.at_ms)) + ' ' + esc(clock(src.at_ms)) + '</small>' +
        (r.others.length ? '<div class="fw-others"><small>Someone or something else in this event:</small> ' + r.others.map((o, i) => '<button class="fw-other" data-o="' + i + '" title="Follow this ' + esc(o.label) + ' instead">' + picHtml(o) + '</button>').join('') + '</div>' : '') +
        '</div></div>' +
        '<div class="fw-ctl">' +
        '<label>Look <select data-win>' + WINDOWS.map(w => '<option value="' + w[0] + '"' + (w[0] === st.win ? ' selected' : '') + '>' + w[1] + '</option>').join('') + '</select> before and after</label>' +
        '<label class="fw-same"><input type="checkbox" data-same' + (st.same ? ' checked' : '') + '> this camera too</label>' +
        '<label class="fw-min">Show matches of at least <b data-minv></b> <input type="range" data-min min="' + pctOf(r.floor) + '" max="100" step="1" value="' + pctOf(st.min) + '"></label>' +
        '</div>' +
        '<div class="fw-note" data-note></div>' +
        '<div class="fw-list" data-list></div>' +
        '<div class="fw-foot"><small>Matching is approximate. It compares how the ' + kind + ' looks (shape, colors, clothes or paint) and is raised when the same recognized face or plate was read. Another ' + kind + ' dressed alike can match, and the same one seen from another angle, in other light or far away can score low. Look before you trust it.</small>' +
        (st.back.length ? ' <button class="btn" data-back>Back</button>' : '') + '</div>';
      wireBack();
      root.querySelector('[data-win]').onchange = (e) => { st.win = +e.target.value; load(); };
      root.querySelector('[data-same]').onchange = (e) => { st.same = e.target.checked; load(); };
      root.querySelectorAll('[data-o]').forEach(b => { b.onclick = () => { st.back.push({ event: st.event, pic: st.pic }); st.pic = r.others[+b.dataset.o].pic; load(); }; });
      const rng = root.querySelector('[data-min]');
      rng.oninput = () => { st.min = +rng.value / 100; try { localStorage.setItem(KEY, String(st.min)); } catch (e) { /* ignore */ } paint(false); };
    }
    root.querySelector('[data-minv]').textContent = pctOf(st.min) + '%';
    const path = r.path.filter(s => s.source || s.score >= st.min);
    const hidden = r.path.length - path.length;
    const found = path.length - 1;
    root.querySelector('[data-note]').innerHTML = (found > 0
      ? '<b>' + found + '</b> sighting' + (found === 1 ? '' : 's') + ' on ' + new Set(path.filter(s => !s.source).map(s => s.camera)).size + ' camera' + (new Set(path.filter(s => !s.source).map(s => s.camera)).size === 1 ? '' : 's')
      : 'No other sighting at ' + pctOf(st.min) + '% or more') +
      (hidden > 0 ? ' · ' + hidden + ' weaker hidden' : '') + (found <= 0 ? '. Lower the match, or look further before and after.' : '.');
    const t0 = r.source.at_ms;
    root.querySelector('[data-list]').innerHTML = path.map((s, i) =>
      '<div class="fw-node' + (s.source ? ' src' : '') + '"><div class="fw-when"><b>' + esc(clock(s.at_ms)) + '</b><small>' + (s.source ? 'start' : esc(gap(s.at_ms - t0))) + '</small></div>' +
      '<div class="fw-card"><button type="button" class="fw-play" data-i="' + i + '" title="Play ' + esc(s.camera) + ' at this moment">' + picHtml(s) +
      '<span class="fw-meta"><b>' + esc(s.camera) + '</b><small>' + esc(s.label) + (s.source ? ' · where it started' : '') + '</small>' +
      (s.source ? '' : '<span class="fw-badges"><span class="fw-score ' + band(s.score) + '" title="Look-alike ' + pctOf(s.sim) + '%' + (s.boost ? ', raised ' + pctOf(s.boost) + ' points' : '') + '">' + pctOf(s.score) + '%</span>' +
        (s.face ? '<span class="fw-tag">same face</span>' : '') + (s.plate ? '<span class="fw-tag">same plate</span>' : '') + '</span>') + '</span></button>' +
      (s.source ? '' : '<button type="button" class="btn fw-cont" data-c="' + i + '" title="Look for where this one went next">Follow from here</button>') + '</div></div>').join('');
    root.querySelectorAll('[data-i]').forEach(b => { b.onclick = () => { const s = path[+b.dataset.i]; playAt(s.camera, s.at_ms, d); }; });
    root.querySelectorAll('[data-c]').forEach(b => { b.onclick = () => { const s = path[+b.dataset.c]; st.back.push({ event: st.event, pic: st.pic }); st.event = s.event_id; st.pic = s.pic; load(); }; });
  }

  load();
  return d;
}
