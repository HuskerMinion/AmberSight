// display.js - display mode (/display): a full-screen view for a wall TV, an old tablet or a spare
// monitor, made to run unattended for weeks.
//
//  * A grid of chosen cameras (all, a saved layout, a group, or picked by hand). When something
//    happens on one (AI-confirmed alerts by default, or any motion), it goes big for a while,
//    longer while it goes on, then back to the grid; two at once share the screen.
//  * Overlays: clock, armed or disarmed, camera names; they move a few pixels now and then
//    (burn-in). Optional dimming at night.
//  * Long runs: the page is built once, every poll updates in place, players free what they made
//    (player.js), it reconnects by itself after a network drop or a server restart, reloads itself
//    when the server's web interface changes (x-ambersight-ui), keeps the screen awake (Wake
//    Lock, HTTPS only) and uses the light picture stream or sub streams by default.
//  * Signing in without a keyboard: not signed in, it shows a display code; an admin approves it
//    in Settings > Users & security > Displays (display_admin.js, src/display.rs). A display
//    session keeps itself (POST /api/display/renew).
// Settings are per device (localStorage), in the panel the gear button opens.

import { t } from './i18n.js';
import { api, bus, esc, sleep, setTheme, themeMode } from './core.js';
import { startLive, startJpeg } from './player.js';
import { initWake, wakeSupported, keepAwake } from './wake.js';
import { cleanSettings, chooseCams, fitGrid, inWindow, updateActivity, bigNow, shiftAt } from './display_kit.js';

const KEY = 'ambs.display.v1';
const PAIR_BY_KEY = 'ambs.display.pairBy';   // when this display must be paired again (unix s)
const POLL_MS = 2000, STATUS_MS = 15000, RENEW_MS = 6 * 3600 * 1000, LISTS_MS = 10 * 60 * 1000, SHIFT_MS = 3 * 60 * 1000;
const OFFLINE_AFTER = 15000;
const $ = (id) => document.getElementById(id);

function loadSettings() { try { return cleanSettings(JSON.parse(localStorage.getItem(KEY) || 'null')); } catch (e) { return cleanSettings(null); } }
function saveSettings(s) { try { localStorage.setItem(KEY, JSON.stringify(s)); } catch (e) { /* not remembered: this visit only */ } }

let S = loadSettings();
let cams = [], camsRx = 0, layouts = [], orders = {};
let shown = [], gridSig = '';
const tiles = new Map();     // camera id -> { el, frame, media, player, mode, nameEl, offEl, offSince }
const panes = new Map();     // camera id -> { el, media, player, label, bar }
const act = new Map();       // camera id -> activity (display_kit.updateActivity)
let manual = null;           // a tile tapped by hand: { id, until }
let fails = 0, running = false, who = null, displaySession = false;
let lastPoll = 0, lastStatus = 0, lastRenew = 0, lastLists = 0, lastShift = 0, shiftStep = 0, polling = false;
let ctlTimer = 0, reloadWanted = false, pairToken = 0;
const started = Date.now();

// For tests and for anyone curious (window.__dsp): what the display did, the last 200 changes.
const stats = window.__dsp = { opens: 0, closes: 0, log: [], polls: 0, fails: 0, reconnectedAt: 0, downSince: 0 };
const note = (what, id) => { stats.log.push([Date.now(), what, id]); if (stats.log.length > 200) stats.log.shift(); };

// ---------------------------------------------------------------- start, sign-in

async function whoami() {
  const r = await fetch('/api/whoami', { credentials: 'same-origin', cache: 'no-store' });
  if (r.status === 401) return { signed_in: false, auth_required: true };
  if (!r.ok) throw new Error('whoami ' + r.status);
  return r.json();
}

async function boot() {
  for (let wait = 2000; ; wait = Math.min(30000, wait * 2)) {
    try { who = await whoami(); break; } catch (e) { message('<p>' + t('display.cannot_reach', 'Cannot reach AmberSight. Trying again…') + '</p>'); await sleep(wait); }
  }
  if (who.auth_required && !who.signed_in) { pairing(); return; }
  displaySession = !!who.display;
  message('');
  await loadLists();
  start();
}

// Not signed in: show a display code until an admin approves it (or it expires: a new one).
// Why a display asks for a code again (shown on the pairing screen).
const WHY_SIGNED_OUT = t('display.signed_out', 'This display was signed out: by an admin, a password change, or after two weeks unused.');
const WHY_NO_DEVICE = t('display.cannot_keep_signed_in', 'This browser cannot keep the display signed in (its device cookie is gone, for example after clearing site data).');
const WHY_SIX_MONTHS = t('display.pair_again_six_months', 'Displays are paired again every six months, so one that leaves the house stops by itself.');

async function pairing(reason) {
  stop();
  const my = ++pairToken;
  const box = $('dspMsg');
  const draw = (code, left, extra) => {
    box.hidden = false;
    box.innerHTML = '<div class="dsp-pair"><div class="dsp-brand"><svg class="ic mark"><use href="#i-logo"/></svg><span><b>Amber</b>Sight</span></div>' +
      '<h1>' + t('display.pair_this_display', 'Pair this display') + '</h1>' +
      (code ? '<div class="dsp-code" aria-label="' + t('display.display_code', 'Display code') + '">' + esc(code) + '</div>' : '<div class="dsp-code wait">…</div>') +
      '<ol class="dsp-steps"><li>' + t('display.step_signin', 'On a phone or computer, sign in to AmberSight as an admin.') + '</li>' +
      '<li>' + t('display.open_settings_path', 'Open <b>Settings</b> &rsaquo; <b>Users &amp; security</b> &rsaquo; <b>Displays</b>.') + '</li>' +
      '<li>' + t('display.step_enter_code', 'Enter this code and pick the account this display uses.') + '</li></ol>' +
      '<p class="dsp-sub">' + (left > 0 ? t('display.code_works', 'The code works for {min, plural, one {# more min} other {# more min}}; a new one comes by itself.', { min: Math.ceil(left / 60000) }) : '') + (extra ? ' ' + extra : '') + '</p>' +
      '<p class="dsp-alt"><a href="/">' + t('display.signin_password', 'Sign in with a password instead') + '</a></p></div>';
  };
  // A display past its six months: say why it asks for a code again.
  let again = reason || '';
  try { const by = +localStorage.getItem(PAIR_BY_KEY) || 0; if (by && Date.now() / 1000 > by - 60) again = WHY_SIX_MONTHS; } catch (e) { /* no storage */ }
  if (again) again += ' ' + t('display.pair_again_below', 'Pair it again with the code below.');
  window.__dsp.pairReason = again;
  const draw0 = draw;
  const drawWith = (code, left, extra) => draw0(code, left, [again, extra].filter(Boolean).join(' '));
  drawWith('', 0);
  while (my === pairToken) {
    let r;
    try { r = await fetch('/api/display/start', { method: 'POST', credentials: 'same-origin', cache: 'no-store' }); } catch (e) { drawWith('', 0, t('display.cannot_reach', 'Cannot reach AmberSight. Trying again…')); await sleep(5000); continue; }
    if (r.status === 429) { let why = ''; try { why = (await r.text()).trim(); } catch (e) { /* none */ } drawWith('', 0, esc(why || t('display.too_many_waiting', 'Too many displays are waiting.')) + ' ' + t('display.try_again_minute', 'Trying again in a minute…')); await sleep(60000); continue; }
    if (!r.ok) { await sleep(10000); continue; }
    const d = await r.json().catch(() => ({}));
    if (d.open) { location.reload(); return; }
    let code = d.code, left = (d.expires_in || 600) * 1000, until = Date.now() + left;
    drawWith(code, left);
    // Ask every few seconds until it is approved, or the code runs out.
    while (my === pairToken) {
      await sleep(3000);
      if (my !== pairToken) return;
      let w;
      try { w = await fetch('/api/display/wait', { method: 'POST', credentials: 'same-origin', cache: 'no-store' }); } catch (e) { drawWith(code, until - Date.now(), t('display.cannot_reach_again', 'Cannot reach AmberSight. Trying again…')); continue; }
      if (w.status === 410) break;   // expired (or the server restarted): a new code
      if (!w.ok) continue;
      const x = await w.json().catch(() => ({}));
      if (x.signed_in) { location.reload(); return; }
      if (x.pending) { code = x.code || code; until = Date.now() + (x.expires_in || 0) * 1000; drawWith(code, until - Date.now()); }
    }
  }
}

function message(html) {
  const box = $('dspMsg');
  box.hidden = !html;
  box.innerHTML = html ? '<div class="dsp-pair">' + html + '</div>' : '';
}

// The account's saved layouts and the shared camera orders (for the camera choice).
async function loadLists() {
  lastLists = Date.now();
  try { const p = await api('/api/ui/prefs'); layouts = Array.isArray(p.layouts) ? p.layouts.filter(l => l && typeof l.id === 'string' && Array.isArray(l.cams)) : []; } catch (e) { /* keep */ }
  try { const o = await api('/api/ui/orders'); if (o && typeof o === 'object') orders = o; } catch (e) { /* keep */ }
}

// ---------------------------------------------------------------- the running display

function start() {
  if (running) return;
  running = true;
  document.body.classList.add('dsp-on');
  applyOverlays();
  renew();
  poll();
  pollStatus();
}

// Signed out (or the session ended): everything stops and the display code shows.
function stop() {
  running = false;
  for (const id of [...panes.keys()]) closePane(id);
  for (const t of tiles.values()) dropTile(t);
  tiles.clear(); act.clear(); manual = null;
  $('dspGrid').innerHTML = ''; gridSig = '';
  $('dspBig').hidden = true;
  document.body.classList.remove('dsp-on', 'dsp-isbig');
}

// One timer for everything: each job runs when it is due and never twice at once.
setInterval(() => {
  const now = Date.now();
  if (running) {
    if (now - lastPoll >= POLL_MS) poll();
    if (now - lastStatus >= STATUS_MS) pollStatus();
    if (now - lastRenew >= RENEW_MS) renew();
    if (now - lastLists >= LISTS_MS) loadLists();
    if (now - lastShift >= SHIFT_MS) shift();
    refreshBig(now);
    paintTilesOffline(now);
  }
  clock();
  dim();
  if (S.nightly && now - started > 3600 * 1000) {
    const d = new Date();
    if (d.getHours() === 3 && d.getMinutes() === 30) reloadSoon();
  }
  if (reloadWanted && !panes.size) location.reload();
}, 1000);

// A display session keeps itself: its end moves two weeks ahead while it is used.
async function renew() {
  lastRenew = Date.now();
  try {
    const r = await api('/api/display/renew', { method: 'POST' });
    displaySession = !!r.display;
    // A display session this browser cannot keep: end it here and pair again (it would
    // otherwise sign out without warning when its two weeks run out).
    if (r.display && r.renewed === false) {
      try { await fetch('/api/logout', { method: 'POST', credentials: 'same-origin' }); } catch (e) { /* ends by itself */ }
      pairing(WHY_NO_DEVICE);
      return;
    }
    try { if (r.pair_by) localStorage.setItem(PAIR_BY_KEY, String(r.pair_by)); else if (!r.display) localStorage.removeItem(PAIR_BY_KEY); } catch (e) { /* not remembered */ }
  } catch (e) { lastRenew -= RENEW_MS - 60000; /* again in a minute */ }
}

async function poll() {
  if (polling) return;
  polling = true; lastPoll = Date.now();
  const ac = new AbortController();
  const t = setTimeout(() => ac.abort(), 10000);
  try {
    const d = await api('/api/cameras', { signal: ac.signal });
    stats.polls++; stats.lastOk = Date.now();
    if (Array.isArray(d.cameras)) { cams = d.cameras; camsRx = Date.now(); }
    if (fails >= 2) { stats.reconnectedAt = Date.now(); note('reconnected'); }
    fails = 0;
    $('dspConn').hidden = true;
    stats.downSince = 0;
    if (!running) return;
    shown = chooseCams(S, cams, layouts, orders);
    const sig = shown.map(c => c.id + ':' + c.name).join(',') + '|' + S.grid + '|' + S.names;
    if (sig !== gridSig) buildGrid(sig);
    else for (const c of shown) updateTile(c);
    updateActivity(act, shown, S, Date.now(), 0);
    refreshBig(Date.now());
  } catch (e) {
    fails++; stats.fails++;
    if (!stats.downSince) stats.downSince = Date.now();
    if (fails >= 2) $('dspConn').hidden = false;
  } finally { clearTimeout(t); polling = false; }
}

async function pollStatus() {
  lastStatus = Date.now();
  const el = $('dspArmed');
  if (!S.armed) { el.hidden = true; return; }
  try {
    const s = await api('/api/status');
    const armed = !!s.armed;
    el.hidden = false;
    el.className = 'dsp-chip ' + (armed ? 'ok' : 'off');
    el.innerHTML = '<svg class="ic"><use href="#i-shield"/></svg>' + (armed ? t('display.armed','Armed') : t('display.disarmed','Disarmed'));
  } catch (e) { /* keep the last */ }
}

// ---------------------------------------------------------------- the grid

function buildGrid(sig) {
  for (const t of tiles.values()) dropTile(t);
  tiles.clear();
  gridSig = sig;
  const grid = $('dspGrid');
  grid.innerHTML = '';
  if (!shown.length) {
    grid.innerHTML = '<div class="dsp-empty"><p>' + t('display.no_cams','No cameras to show.') + '</p><p class="dsp-sub">' + (cams.length ? t('display.open_settings','Open Settings (move the mouse or tap the screen) and choose cameras.') : t('display.no_account_cams','This account cannot see any cameras.')) + '</p></div>';
    return;
  }
  layoutGrid();
  for (const c of shown) {
    const el = document.createElement('div');
    el.className = 'dsp-tile';
    el.dataset.id = c.id;
    el.innerHTML = '<div class="dsp-frame"></div><span class="dsp-name dsp-shift"' + (S.names ? '' : ' hidden') + '></span><span class="dsp-badge dsp-shift" hidden></span>';
    const t = { el, frame: el.firstChild, media: null, player: null, mode: '', nameEl: el.children[1], offEl: el.children[2], offSince: 0, cam: c };
    t.nameEl.textContent = c.short_name || c.name;
    el.onclick = () => { manual = { id: c.id, until: Date.now() + Math.max(30, S.hold) * 1000 }; refreshBig(Date.now()); };
    grid.appendChild(el);
    tiles.set(c.id, t);
    updateTile(c);
  }
}

function layoutGrid() {
  const grid = $('dspGrid');
  const { cols, rows } = fitGrid(shown.length, innerWidth, innerHeight, 16 / 9);
  grid.style.gridTemplateColumns = 'repeat(' + cols + ', 1fr)';
  grid.style.gridTemplateRows = 'repeat(' + rows + ', 1fr)';
}
addEventListener('resize', () => { if (shown.length) layoutGrid(); });

// What a tile shows: live pictures, 'asleep' (a battery camera: the display never wakes it) or 'off'.
function tileMode(c) {
  if (c.enabled === false) return 'off';
  if (c.on_demand && !c.online) return 'asleep';
  return 'live';
}

function updateTile(c) {
  const tile = tiles.get(c.id);
  if (!tile) return;
  tile.cam = c;
  const mode = tileMode(c);
  if (c.online || mode !== 'live') tile.offSince = 0; else if (!tile.offSince) tile.offSince = Date.now();
  if (mode === tile.mode) return;
  dropMedia(tile);
  tile.mode = mode;
  if (mode === 'live') {
    tile.player = mountPlayer(tile.frame, c, S.grid === 'sub' || c.remote ? 'sub' : 'light', (m) => { tile.media = m; });
    return;
  }
  const ph = document.createElement('div');
  ph.className = 'dsp-ph';
  ph.innerHTML = mode === 'asleep' ? '<span><svg class="ic"><use href="#i-moon"/></svg>' + t('display.asleep', 'Asleep') + '</span>' : '<span>' + t('display.off', 'Off') + '</span>';
  tile.frame.appendChild(ph);
  tile.media = ph;
}

// Live pictures into `frame`: JPEG frames ('light'), or video ('sub' / 'main') falling back to
// JPEG frames when the browser cannot play the stream. setMedia(el) is told each element shown.
function mountPlayer(frame, c, q, setMedia) {
  const holder = { player: null, stop() { if (this.player) this.player.stop(); this.player = null; } };
  const jpeg = () => {
    const img = document.createElement('img');
    img.className = 'dsp-media'; img.alt = c.name; img.decoding = 'async';
    frame.appendChild(img);
    setMedia(img);
    holder.player = startJpeg(img, c.id);
  };
  if (q === 'light') { jpeg(); return holder; }
  const v = document.createElement('video');
  v.className = 'dsp-media'; v.muted = true; v.autoplay = true; v.playsInline = true;
  frame.appendChild(v);
  setMedia(v);
  holder.player = startLive(v, c.id, q, () => { v.remove(); jpeg(); });
  return holder;
}

function dropMedia(t) {
  if (t.player) { t.player.stop(); t.player = null; }
  t.frame.textContent = '';
  t.media = null;
}
function dropTile(t) { dropMedia(t); t.el.onclick = null; }

// "Offline" on a tile whose camera has been down for a while (its player keeps trying).
function paintTilesOffline(now) {
  for (const tile of tiles.values()) {
    const off = tile.mode === 'live' && tile.offSince && now - tile.offSince > OFFLINE_AFTER;
    if (tile.offEl.hidden === !off) continue;
    tile.offEl.hidden = !off;
    tile.offEl.textContent = off ? t('display.offline', 'Offline') : '';
    tile.el.classList.toggle('offline', !!off);
  }
}

// ---------------------------------------------------------------- the big view

function refreshBig(now) {
  if (!running) return;
  if (manual && manual.until <= now) manual = null;
  const on = new Set(shown.map(c => c.id));
  const want = bigNow(act, S, now, manual).filter(id => on.has(id));
  for (const id of [...panes.keys()]) if (!want.includes(id)) closePane(id);
  want.forEach((id, i) => { if (!panes.has(id)) openPane(id); const p = panes.get(id); if (p) p.el.style.order = String(i); });
  const box = $('dspBig');
  box.hidden = !want.length;
  box.dataset.n = String(want.length);
  document.body.classList.toggle('dsp-isbig', want.length > 0);
  for (const [id, p] of panes) paintPane(id, p, now);
}

function openPane(id) {
  const c = shown.find(x => x.id === id);
  if (!c) return;
  const el = document.createElement('div');
  el.className = 'dsp-pane';
  el.innerHTML = '<div class="dsp-frame"></div><div class="dsp-plabel dsp-shift"><i class="dsp-dot"></i><span class="dsp-pname"></span><span class="dsp-why"></span></div><div class="dsp-bar" hidden><i></i></div>';
  const p = { el, frame: el.firstChild, media: null, player: null, label: el.children[1], bar: el.children[2].firstChild, barUntil: 0 };
  p.label.querySelector('.dsp-pname').textContent = c.short_name || c.name;
  p.player = mountPlayer(p.frame, c, S.big === 'main' && !c.remote ? 'main' : 'sub', (m) => { p.media = m; });
  el.onclick = () => {
    // Tapped: back to the grid, and this camera stays there until its activity has stopped.
    if (manual && manual.id === id) manual = null;
    const a = act.get(id);
    if (a) { a.dismissed = true; a.until = 0; }
    refreshBig(Date.now());
  };
  $('dspBig').appendChild(el);
  panes.set(id, p);
  stats.opens++; note('big', id);
}

function closePane(id) {
  const p = panes.get(id);
  if (!p) return;
  p.player.stop();
  p.el.onclick = null;
  p.el.remove();
  panes.delete(id);
  stats.closes++; note('grid', id);
}

function paintPane(id, p, now) {
  const a = act.get(id);
  const why = a ? a.reason.text : '';
  const w = p.label.querySelector('.dsp-why');
  if (w.textContent !== why) w.textContent = why;
  p.label.dataset.cls = a ? a.reason.cls : '';
  // The bar runs down once the activity has stopped (while it goes on, the camera stays).
  const until = a && !a.live ? a.until : (manual && manual.id === id && !a ? manual.until : 0);
  if (until !== p.barUntil) {
    p.barUntil = until;
    const bar = p.bar;
    bar.style.transition = 'none';
    bar.style.transform = 'scaleX(1)';
    if (until) {
      void bar.offsetWidth;   // (start the run-down from full)
      bar.style.transition = 'transform ' + Math.max(0, (until - now) / 1000).toFixed(1) + 's linear';
      bar.style.transform = 'scaleX(0)';
    }
    p.bar.parentNode.hidden = !until;
  }
}

// ---------------------------------------------------------------- overlays, dimming, burn-in

function applyOverlays() {
  $('dspClock').hidden = !S.clock;
  if (!S.armed) $('dspArmed').hidden = true; else pollStatus();
  clock(true);
}

let clockMin = -1;
function clock(force) {
  if (!S.clock) return;
  const d = new Date();
  const m = d.getHours() * 60 + d.getMinutes();
  if (m === clockMin && !force) return;
  clockMin = m;
  const el = $('dspClock');
  el.querySelector('b').textContent = d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  el.querySelector('small').textContent = d.toLocaleDateString([], { weekday: 'short', month: 'short', day: 'numeric' });
}

function dim() {
  const d = new Date();
  const on = S.dim && inWindow(d.getHours() * 60 + d.getMinutes(), S.dimFrom, S.dimTo) && !(S.dimWake && panes.size);
  const el = $('dspDim');
  if (el.hidden === !on) return;
  el.hidden = !on;
  el.style.opacity = String(S.dimLevel / 100);
}

// Move the overlays a few pixels every few minutes, so nothing stands still on a TV for weeks.
function shift() {
  lastShift = Date.now();
  const [x, y] = shiftAt(++shiftStep);
  const r = $('dsp').style;
  r.setProperty('--sx', x + 'px');
  r.setProperty('--sy', y + 'px');
}

// ---------------------------------------------------------------- controls and settings

function showControls() {
  const c = $('dspCtl');
  c.hidden = false;
  document.body.classList.remove('dsp-idle');
  clearTimeout(ctlTimer);
  ctlTimer = setTimeout(() => { if (!c.contains(document.activeElement)) { c.hidden = true; document.body.classList.add('dsp-idle'); } }, 4000);
}
for (const ev of ['pointermove', 'pointerdown', 'keydown']) document.addEventListener(ev, (e) => {
  if (ev === 'keydown' && e.key === 'Escape') {
    if (closeSettings()) return;
    if (panes.size) { for (const id of panes.keys()) { const a = act.get(id); if (a) { a.dismissed = true; a.until = 0; } } manual = null; refreshBig(Date.now()); return; }
  }
  if (!running) return;
  const was = $('dspCtl').hidden;
  showControls();
  // (A TV remote: the first key shows the buttons and puts focus on Settings; OK then opens it.)
  if (ev === 'keydown' && was && !setBox) $('dspSet').focus();
}, { passive: true });
$('dspFull').onclick = () => {
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  else if (document.documentElement.requestFullscreen) document.documentElement.requestFullscreen().catch(() => {});
};
$('dspSet').onclick = () => openSettings();

let setBox = null;
function closeSettings() {
  if (!setBox) return false;
  setBox.remove(); setBox = null;
  return true;
}

function sel(name, label, opts, val) {
  return '<label class="dsp-f"><span>' + label + '</span><select name="' + name + '">' + opts.map(([v, label]) => '<option value="' + esc(v) + '"' + (String(v) === String(val) ? ' selected' : '') + '>' + esc(label) + '</option>').join('') + '</select></label>';
}
const chk = (name, label, on) => '<label class="dsp-c"><input type="checkbox" name="' + name + '"' + (on ? ' checked' : '') + '><span>' + label + '</span></label>';

function openSettings() {
  closeSettings();
  const groups = [...new Set(cams.flatMap(c => (c.groups && c.groups.length) ? c.groups : (c.group ? [c.group] : [])))].sort();
  const src = [['all', t('display.all_cameras', 'All cameras')]].concat(layouts.map(l => ['layout:' + l.id, t('display.layout_name', 'Layout: {name}', { name: l.name })]), groups.map(g => ['group:' + g, t('display.group_name', 'Group: {name}', { name: g })]), [['pick', t('display.cameras_i_pick', 'Cameras I pick')]]);
  const picked = new Set(S.cams);
  const awake = wakeSupported() ? (keepAwake() ? t('display.awake_on', 'This browser keeps the screen on while the display shows.') : t('display.awake_off', 'Keep screen awake is off on this device (AmberSight\'s account menu).'))
    : t('display.awake_http', 'Over plain HTTP the browser cannot keep the screen on: set the device\'s screen timeout to Never (or turn on HTTPS in AmberSight).');
  const acct = who && who.user ? (displaySession ? t('display.signed_in_display', 'Signed in as <b>{user}</b> (a display: it stays signed in while it is used, and signs out after {days} days unused).', { user: esc(who.user), days: 14 }) : t('display.signed_in', 'Signed in as <b>{user}</b>.', { user: esc(who.user) })) : '';
  setBox = document.createElement('div');
  setBox.className = 'dsp-set';
  setBox.setAttribute('role', 'dialog'); setBox.setAttribute('aria-modal', 'true'); setBox.setAttribute('aria-label', t('display.settings_title', 'Display settings'));
  setBox.innerHTML = '<form class="dsp-card" novalidate><div class="dsp-head"><h2>' + t('display.settings_title', 'Display settings') + '</h2><button type="button" class="btn icon" data-x aria-label="' + t('display.close', 'Close') + '"><svg class="ic"><use href="#i-x"/></svg></button></div>' +
    '<div class="dsp-sec"><h3>' + t('display.cameras', 'Cameras') + '</h3>' + sel('source', t('display.show', 'Show'), src, S.source) +
    '<div class="dsp-pick" data-pick' + (S.source === 'pick' ? '' : ' hidden') + '>' + cams.map(c => chk('cam', esc(c.name), picked.has(c.name)).replace('name="cam"', 'name="cam" value="' + esc(c.name) + '"')).join('') + '</div>' +
    '<p class="dsp-hint">' + t('display.up_to_16', 'Up to 16 cameras. Layouts and groups are the ones on Live.') + '</p></div>' +
    '<div class="dsp-sec"><h3>' + t('display.when_happens', 'When something happens') + '</h3>' +
    sel('trigger', t('display.show_big_on', 'Show the camera big on'), [['alerts', t('display.trig_alerts', 'Alerts (AI-confirmed; motion on cameras without AI)')], ['motion', t('display.trig_motion', 'Any motion')], ['off', t('display.trig_off', 'Never: grid only')]], S.trigger) +
    '<label class="dsp-f"><span>' + t('display.for_seconds', 'For (seconds, longer while it goes on)') + '</span><input name="hold" type="number" min="5" max="600" step="5" value="' + S.hold + '"></label>' +
    sel('multi', t('display.two_at_once', 'Two at once'), [['two', t('display.side_by_side', 'Side by side')], ['newest', t('display.newest_only', 'Newest only')]], S.multi) +
    '<p class="dsp-hint">' + t('display.alerts_hint', 'With Alerts, a camera the AI does not check goes big on any motion. A clip started with Record now (or by an automation through the API) also shows its camera big.') + '</p></div>' +
    '<div class="dsp-sec"><h3>' + t('display.picture', 'Picture') + '</h3>' +
    sel('grid', t('display.grid', 'Grid'), [['light', t('display.grid_light', 'Light: pictures, lowest load')], ['sub', t('display.grid_sub', 'Video: sub streams')]], S.grid) +
    sel('big', t('display.big_view', 'Big view'), [['sub', t('display.big_sub', 'Sub stream (light)')], ['main', t('display.big_main', 'Main stream (sharper, more load)')]], S.big) + '</div>' +
    '<div class="dsp-sec"><h3>' + t('display.show', 'Show') + '</h3>' + chk('clock', t('display.clock', 'Clock'), S.clock) + chk('armed', t('display.armed_or_disarmed', 'Armed or disarmed'), S.armed) + chk('names', t('display.camera_names', 'Camera names'), S.names) +
    sel('theme', t('display.theme', 'Theme'), [['auto', t('display.theme_auto', 'Auto')], ['dark', t('display.theme_dark', 'Dark')], ['light', t('display.theme_light', 'Light')]], themeMode()) + '</div>' +
    '<div class="dsp-sec"><h3>' + t('display.night', 'Night') + '</h3>' + chk('dim', t('display.dim_at_night', 'Dim the screen at night'), S.dim) +
    '<div class="dsp-row"><label class="dsp-f"><span>' + t('display.from', 'From') + '</span><input name="dimFrom" type="time" value="' + S.dimFrom + '"></label><label class="dsp-f"><span>' + t('display.to', 'To') + '</span><input name="dimTo" type="time" value="' + S.dimTo + '"></label></div>' +
    '<label class="dsp-f"><span>' + t('display.how_dark', 'How dark (%)') + '</span><input name="dimLevel" type="range" min="20" max="95" step="5" value="' + S.dimLevel + '"></label>' +
    chk('dimWake', t('display.brighten_while_big', 'Brighten while a camera is big'), S.dimWake) + '</div>' +
    '<div class="dsp-sec"><h3>' + t('display.this_device', 'This device') + '</h3>' + chk('nightly', t('display.reload_nightly', 'Reload the page every night at 3:30'), S.nightly) +
    '<p class="dsp-hint">' + awake + '</p><p class="dsp-hint">' + acct + '</p></div>' +
    '<div class="dsp-foot"><button type="submit" class="primary">' + t('display.save', 'Save') + '</button><button type="button" class="btn" data-x>' + t('display.cancel', 'Cancel') + '</button><span class="dsp-grow"></span>' +
    (displaySession ? '<button type="button" class="btn" data-out>' + t('display.sign_out', 'Sign out this display') + '</button>' : '<a class="btn" href="/">' + t('display.exit', 'Exit display mode') + '</a>') + '</div></form>';
  document.body.appendChild(setBox);
  const f = setBox.querySelector('form');
  f.source.onchange = () => { setBox.querySelector('[data-pick]').hidden = f.source.value !== 'pick'; };
  for (const x of setBox.querySelectorAll('[data-x]')) x.onclick = closeSettings;
  const out = setBox.querySelector('[data-out]');
  if (out) out.onclick = async () => {
    if (!confirm(t('display.confirm_sign_out', 'Sign this display out? It shows a new display code, which an admin approves again.'))) return;
    try { await fetch('/api/logout', { method: 'POST', credentials: 'same-origin' }); } catch (e) { /* the page shows what is true */ }
    location.reload();
  };
  f.onsubmit = (e) => {
    e.preventDefault();
    const v = (n) => f.elements[n].value, c = (n) => f.elements[n].checked;
    S = cleanSettings({
      source: v('source'), cams: [...f.querySelectorAll('input[name=cam]:checked')].map(i => i.value),
      trigger: v('trigger'), hold: v('hold'), multi: v('multi'), grid: v('grid'), big: v('big'),
      clock: c('clock'), armed: c('armed'), names: c('names'),
      dim: c('dim'), dimFrom: v('dimFrom'), dimTo: v('dimTo'), dimLevel: v('dimLevel'), dimWake: c('dimWake'), nightly: c('nightly'),
    });
    saveSettings(S);
    setTheme(v('theme'));
    closeSettings();
    // Everything again with the new settings.
    for (const id of [...panes.keys()]) closePane(id);
    act.clear(); manual = null; gridSig = '';
    applyOverlays();
    $('dspDim').hidden = true; dim();
    poll();
  };
  const first = f.querySelector('select');
  if (first) first.focus();
}

// ---------------------------------------------------------------- events from core.js

// Signed out elsewhere (Settings > Displays > Sign out, a password change): the display code again.
bus.on('unauth', () => { if (running) pairing(displaySession ? WHY_SIGNED_OUT : ''); });
// The server has a new web interface: reload (once nothing is big).
bus.on('ui-updated', () => reloadSoon());
function reloadSoon() { reloadWanted = true; }
// (A camera big for a very long time does not hold an update up for more than ten minutes.)
setInterval(() => { if (reloadWanted) location.reload(); }, 10 * 60 * 1000);

initWake();
boot();
