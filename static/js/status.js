// status.js - header status text, lamp, clock and the Armed / Disarmed button.

import { $, state, api, fmtSize, msgDlg, toast, viewOnlyNote, confirmDialog } from './core.js';

let armedCache;
let armingUntil = 0;   // ms; an arm delay is running until then
let armDelay = 0;
let healthTick = 0;   // /api/health is read on every 6th status poll (about every 30 s)
let health = null;    // last /api/health reply
let unreachable = false;

export async function refreshStatus() {
  try {
    const s = await api('/api/status');
    if (s.name) state.serverName = s.name;
    const text = s.online + '/' + s.cameras + ' online · ' + s.events + ' events · ' +
      fmtSize(s.storage.bytes) + ' (' + s.storage.clips + ' clips) · up ' + Math.floor(s.uptime_seconds / 60) + ' min';
    $('statusText').textContent = text;
    armingUntil = !s.armed && s.arming_until ? s.arming_until * 1000 : 0;
    armDelay = s.arm_delay || 0;
    $('lamp').className = 'lamp ' + (s.armed ? 'armed' : armingUntil ? 'arming' : 'shielded');
    $('lamp').title = text;
    armedCache = s.armed;
    unreachable = false;
    syncShieldBtn();
    // (The health check takes a while on a big system: it fills the chip when
    // it comes, without holding up the status, or the start page waiting for it.)
    if (state.isAdmin && healthTick++ % 6 === 0) {
      api('/api/health').then(h => { health = h; syncHealth(s); }).catch(() => { /* keep the last reading */ });
    }
    syncHealth(s);
  } catch (e) {
    if (e.status === 401) return;
    $('statusText').textContent = 'server unreachable';
    $('lamp').className = 'lamp';
    unreachable = true;
    syncHealth();
  }
}

// The health chip: nothing while all is well; at every width, an icon and a few
// words when the server is unreachable, a camera is offline or a check warns.
// Whether warning text `w` has camera `name` as whole words ("Front" is in
// "Front Door is offline" too: namedCam picks the longest).
function mentions(w, name) {
  if (!name) return false;
  const word = /[\p{L}\p{N}]/u;
  for (let i = w.indexOf(name); i >= 0; i = w.indexOf(name, i + 1)) {
    const before = i > 0 ? w[i - 1] : '';
    const after = w[i + name.length] || '';
    if (!word.test(before) && !word.test(after)) return true;
  }
  return false;
}

// The camera a warning is about: the longest name it mentions.
function namedCam(w, cams) {
  let best = null;
  for (const c of cams || []) if (mentions(w, c.name) && (!best || c.name.length > best.name.length)) best = c;
  return best;
}

function syncHealth(s) {
  const b = $('healthChip');
  if (!b) return;
  let lvl = '', txt = '', tip = '', compact = '';
  if (unreachable) { lvl = 'fault'; txt = 'Server offline'; tip = 'The server is not answering. Click to open Status.'; }
  else {
    const cams = (health && health.cameras) || [];
    const off = cams.filter(c => c.enabled && !c.online && !c.sleeping);
    const warns = ((health && health.warnings) || []).filter(w => typeof w === 'string');
    // An offline camera the warnings already name counts once.
    const offOnly = off.filter(c => !warns.some(w => (namedCam(w, cams) || {}).name === c.name));
    const n = offOnly.length + warns.length;
    if (n) {
      lvl = off.length ? 'fault' : 'warn';
      // The first problem in words, "+N more" after it; the tooltip lists them all.
      const cap = (t) => t.charAt(0).toUpperCase() + t.slice(1);
      const first = offOnly.length ? offOnly[0].name + ' is offline' : warns.length ? (((health && health.short) || [])[0] || cap(warns[0])) : off[0].name + ' is offline';
      txt = first + (n > 1 ? ' +' + (n - 1) + ' more' : '');
      // (When the chip is too narrow for the words: the count alone, never a cut-off name.)
      compact = n === 1 ? (off.length ? '1 offline' : '1 problem') : n + ' problems';
      tip = [].concat(offOnly.map(c => c.name + ' is offline'), warns.map(cap)).join('\n') + '\nClick to see what to do.';
    }
  }
  // Nothing wrong: a camera the hourly check found that is not added yet.
  const fresh = (!lvl && health && health.new_cameras) || [];
  if (fresh.length) {
    lvl = 'info'; txt = fresh.length === 1 ? 'New camera' : fresh.length + ' new cameras';
    tip = 'Found on the network, not added yet: ' + fresh.map(f => f.ip + (f.name ? ' (' + f.name + ')' : '')).join(', ') + '\nClick to add or ignore.';
  }
  b.dataset.go = lvl === 'info' ? 'find' : '';
  b.hidden = !lvl;
  if (!lvl) return;
  b.className = 'healthchip ' + (lvl === 'fault' ? 'fault' : lvl === 'info' ? 'info' : '');
  const hct = b.querySelector('.hctext');
  hct.textContent = txt;
  if (compact && hct.scrollWidth > hct.clientWidth + 1) hct.textContent = compact;
  b.title = state.isAdmin ? tip : tip.replace(/\n?Click to [^\n]*$/, '');
  b.classList.toggle('noclick', !state.isAdmin);
  b.setAttribute('aria-label', txt);
}

function syncShieldBtn() {
  const b = $('hdrShield');
  const l = $('shieldLbl');
  if (armedCache === undefined) { l.textContent = '\u2026'; return; }
  const left = armingUntil ? Math.max(0, Math.ceil((armingUntil - Date.now()) / 1000)) : 0;
  if (armingUntil && !left) { armingUntil = 0; refreshStatus(); }
  l.textContent = armedCache ? 'Armed' : left ? 'Arming in ' + (left < 100 ? left + ' s' : Math.floor(left / 60) + ':' + String(left % 60).padStart(2, '0')) : 'Disarmed';
  b.className = 'btn shieldbtn ' + (armedCache ? 'armedbtn' : left ? 'armingbtn' : 'shieldedbtn');
  b.title = armedCache ? 'Armed: click to disarm (no motion, no new events)' : left ? 'Arming soon: click to cancel' :
    'Disarmed: click to arm' + (armDelay ? ' (after ' + armDelay + ' s, so you can leave)' : '');
}

// Arm or disarm from anywhere. Disarming never happens silently: a toast offers Undo for 6 s,
// which arms again at once (no arm delay).
export async function toggleShield(quiet) {
  if (!state.isAdmin) { viewOnlyNote(); return; }
  try {
    const s = await api('/api/status');
    // Armed or arming: disarm (cancels the arm delay). Disarmed: arm, after the delay.
    const arm = !s.armed && !s.arming_until;
    const r = await api('/api/shield', { json: { armed: arm, delay: arm } });
    if (r.arming_until) toast('Arming in ' + Math.round(r.arming_until - Date.now() / 1000) + ' s. Tap the button again to cancel.');
    else if (!r.armed && s.armed) {
      toast('Disarmed', { ms: 6000, action: { label: 'Undo', run: async () => {
        try { await api('/api/shield', { json: { armed: true, delay: false } }); } catch (e) { msgDlg(e.message); }
        refreshStatus();
      } } });
    }
    else if (!quiet) toast(r.armed ? 'Armed: motion detection on' : 'Disarmed: no motion, no new events');
    refreshStatus();
  } catch (e) { msgDlg(e.message); }
}

// The S key: a stray key press must not turn detection off for a whole site, so
// disarming asks first. (Arming does not, and the header button is one deliberate click.)
export async function shieldKey() {
  if (!state.isAdmin) { viewOnlyNote(); return; }
  try {
    const s = await api('/api/status');
    if (s.armed || s.arming_until) {
      const yes = await confirmDialog({ title: 'Disarm all cameras?', body: 'Motion detection, new events and motion alerts stop on every camera until you arm again.', ok: 'Disarm', danger: true });
      if (!yes) return;
    }
    await toggleShield(true);
  } catch (e) { msgDlg(e.message); }
}

// Before the first status reply (sign-in check failed): show the chip too.
export function markUnreachable() { unreachable = true; syncHealth(); }

export function initStatus() {
  $('hdrShield').onclick = () => toggleShield(true);
  // The status line and the chip open System > Status & updates (admins), where the problems are listed.
  const openStatus = () => {
    if (!state.isAdmin) return;
    if (location.hash !== '#settings/system/status') location.hash = '#settings/system/status';
  };
  $('statusText').onclick = openStatus;
  // The chip: new cameras -> Find cameras; anything else -> the problems on the status page.
  // (Both are admin pages: for a view-only account the chip only informs.)
  $('healthChip').onclick = () => {
    if (!state.isAdmin) return;
    if ($('healthChip').dataset.go === 'find') location.hash = '#settings/cameras/find';
    else openStatus();
  };
  $('statusText').title = 'Open system status';
  setInterval(() => {
    const clock = $('clock');
    clock.textContent = new Date().toLocaleTimeString();
    // A crowded header drops the clock rather than showing a piece of it behind the next button.
    clock.hidden = false;
    const box = $('status');
    if (box.scrollWidth > box.clientWidth + 1) clock.hidden = true;
    if (armingUntil) syncShieldBtn();
  }, 1000);
}
