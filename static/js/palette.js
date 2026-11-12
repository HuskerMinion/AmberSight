// palette.js - command palette. Ctrl/Cmd+K opens it (or call open()). Self-installing on import.
// Modules are imported on first use of a command so the palette itself stays light.
import { state, $, ic, api, toast, setTheme, backLayer, overlaysChanged, afterOverlayClose } from './core.js';
import { savedFilters } from './events.js';

const RECENT_KEY = 'ambersight.palette.recent';
let root = null, input = null, list = null, items = [], shown = [], sel = 0, prevFocus = null;

const imp = {
  nav: () => import('./nav.js'), live: () => import('./live.js'), tl: () => import('./livetl.js'),
  status: () => import('./status.js'), settings: () => import('./settings.js'), cams: () => import('./cameras.js'),
  zones: () => import('./zones.js'),
};
const goTab = async (id) => (await imp.nav()).goTab(id);

// Every settings page: [route, name, section, plain-word synonyms people might type].
const SETTINGS = [
  ['cameras/list', 'Cameras', 'Cameras', 'add camera remove camera rename camera stream address rtsp ip'],
  ['cameras/find', 'Find cameras', 'Cameras', 'discover scan network onvif add camera automatically new camera'],
  ['cameras/groups', 'Camera groups', 'Cameras', 'group rooms outside inside organize'],
  ['recording/retention', 'Storage & retention', 'Recording', 'how long to keep video delete old recordings disk space full storage limit days keep forever protect'],
  ['recording/drives', 'Drives & folders', 'Recording', 'where recordings are saved external drive nas network folder move recordings usb'],
  ['detection/motion', 'Motion sensitivity', 'Detection & alerts', 'too many alerts false alarms shadows sensitive missed motion low medium high'],
  ['detection/objects', 'Objects (AI)', 'Detection & alerts', 'person vehicle animal package ai object detection what to look for'],
  ['detection/alerts', 'Alerts', 'Detection & alerts', 'phone alerts notifications notify ntfy pushover telegram push email webhook home assistant messages'],
  ['detection/limits', 'Alert limits & priority', 'Detection & alerts', 'too many notifications quiet snooze priority rate limit'],
  ['detection/rules', 'Alert rules', 'Detection & alerts', 'only alert when quiet hours delivery truck conditions'],
  ['detection/summary', 'Morning summary', 'Detection & alerts', 'daily digest recap overnight report'],
  ['smart/review', 'AI review', 'Smart features', 'why no alert missing alerts false alarms'],
  ['smart/faces', 'Faces', 'Smart features', 'face recognition known people names strangers'],
  ['smart/plates', 'License plates', 'Smart features', 'number plate lpr anpr car registration'],
  ['smart/sounds', 'Sounds', 'Smart features', 'audio glass break bark loud noise scream'],
  ['smart/search', 'Smart search', 'Smart features', 'describe find red truck search by description'],
  ['schedules/profiles', 'Profiles & schedule', 'Schedules & presence', 'home away night schedule when to arm sunrise sunset'],
  ['schedules/presence', 'Presence', 'Schedules & presence', 'phone at home away geofence arrive leave'],
  ['users/users', 'Users', 'Users & security', 'password add user accounts sign in viewer admin family pair code invite'],
  ['users/https', 'HTTPS', 'Users & security', 'secure certificate tls ssl remote access tailscale padlock'],
  ['connections/share', 'Share streams', 'Connections', 'rtsp restream home assistant vlc frigate scrypted'],
  ['connections/servers', 'Servers', 'Connections', 'other site link servers second location cabin'],
  ['connections/detector', 'External detector', 'Connections', 'custom detector hook codeproject'],
  ['connections/watchdog', 'Watchdog', 'Connections', 'reboot camera power cycle frozen camera smart plug offline'],
  ['system/status', 'Status & updates', 'System', 'health problems warnings update version upgrade uptime'],
  ['system/logs', 'Logs', 'System', 'errors warnings debug'],
  ['system/audit', 'Audit log', 'System', 'who changed sign in history'],
  ['system/backup', 'Backup', 'System', 'restore export settings save settings'],
];

function link(h) { return () => { if (location.hash !== h) location.hash = h; }; }

function build() {
  const admin = state.isAdmin !== false, out = [];
  const add = (id, group, icon, label, hint, run, keys, adminOnly, kw) => { if (adminOnly && !admin) return; out.push({ id, group, icon, label, hint: hint || '', run, keys: keys || [], kw: (kw || '').toLowerCase() }); };

  add('go:live', 'Go to', 'live', 'Go to Live', 'Camera grid', () => goTab('live'), ['G', 'L']);
  add('go:events', 'Go to', 'alert', 'Go to Events', 'Alerts and detections', () => goTab('events'), ['G', 'E']);
  add('go:grid', 'Go to', 'grid', 'Events grid', 'Event clips as pictures', link('#events?view=grid'), ['G', 'V'], false, 'clips grid thumbnails');
  add('go:timelapse', 'Go to', 'timeline', 'Go to Time-lapse', 'A day in a minute', () => goTab('timelapse'), [], false, 'timelapse day fast summary');
  add('go:settings', 'Go to', 'gear', 'Go to Settings', '', () => goTab('settings'), ['G', 'S'], true);
  for (const [route, name, sec, kw] of SETTINGS) add('set:' + route, 'Settings', 'gear', 'Settings: ' + name, sec, link('#settings/' + route), [], true, kw);
  add('go:help', 'Go to', 'help', 'Go to Help', 'Guides and answers', () => goTab('help'), ['G', 'H'], false, 'how do i guide manual support');

  for (const c of state.cams || []) {
    const id = c.id, n = c.name;
    add('cam:full:' + id, 'Cameras', 'expand', n, 'Open full screen on Live', async () => { await goTab('live'); (await imp.live()).openFullCam(id); }, [], false, 'camera full screen live view');
    add('cam:play:' + id, 'Cameras', 'timeline', 'Play back ' + n, 'Recordings', async () => { await goTab('live'); (await imp.tl()).openAt(id, Date.now() - 60000); }, [], false, 'recordings rewind history');
    add('cam:snap:' + id, 'Cameras', 'camera', 'Snapshot ' + n, n, async () => (await imp.live()).snapshot(id));
    add('cam:set:' + id, 'Cameras', 'gear', n + ' settings', 'Camera settings', async () => (await imp.cams()).openCameraDialog(id), [], true);
    add('cam:zones:' + id, 'Cameras', 'zone', 'Zones for ' + n, 'Motion zones and masks', async () => (await imp.zones()).openZoneEditor(id), [], true);
  }

  add('act:shield', 'Actions', 'shield', 'Arm / disarm', 'Turn motion detection on or off for all cameras', async () => (await imp.status()).toggleShield(true), ['S'], true, 'armed disarmed shield detection on off');
  for (const p of profileNames) add('act:profile:' + p, 'Actions', 'user', 'Switch profile to ' + p, profileActive === p ? 'Active now' : 'Profile', async () => {
    try { await api('/api/profile', { json: { name: p, minutes: null } }); toast('Profile: ' + p); } catch (e) { toast(e.message, { kind: 'err' }); }
  }, [], true);
  add('act:timeline', 'Actions', 'timeline', 'Timeline on / off', 'Live timeline mode', async () => { await goTab('live'); (await imp.tl()).toggle(); }, ['T']);
  add('act:view', 'Actions', 'v-grid', 'Change layout', 'Grid, Fit, Focus, Focus 2', async () => { await goTab('live'); (await imp.live()).cycleViewStyle(); }, ['V'], false, 'view style tiles');
  for (const [m, label] of [['auto', 'Auto'], ['light', 'Light'], ['dark', 'Dark']]) add('act:theme:' + m, 'Actions', m === 'light' ? 'sun' : m === 'dark' ? 'moon' : 'auto', 'Theme: ' + label, m === 'auto' ? 'Follow this device' : '', () => setTheme(m), [], false, 'dark mode light mode appearance');
  add('act:keys', 'Actions', 'help', 'Show keyboard shortcuts', '', () => { const b = $('keysBtn'); if (b) b.click(); }, ['?']);

  for (const [o, label] of [['person', 'People'], ['vehicle', 'Vehicles'], ['animal', 'Animals'], ['package', 'Packages'], ['face', 'Faces']])
    add('ev:' + o, 'Events', o === 'face' ? 'face' : o === 'vehicle' ? 'car' : o === 'animal' ? 'paw' : o === 'package' ? 'pkg' : 'person', 'Events: ' + label, 'Filter to ' + o, link('#events?obj=' + o));
  add('ev:prot', 'Events', 'lock', 'Events: protected only', 'Filter', link('#events?prot=1'));
  // The named filter sets saved on the Events page (this browser).
  for (const f of savedFilters()) add('saved:' + f.name, 'Saved filters', 'search', f.name, 'Saved filter on Events', link(f.hash), [], false, 'saved filter events');
  return out;
}

let profileNames = [], profileActive = '';
async function loadProfiles() {
  try { const p = await api('/api/profiles'); profileNames = p.profiles || []; profileActive = p.active || ''; } catch (e) { profileNames = []; }
}

// ---- fuzzy: subsequence, bonus for word starts and consecutive runs; returns {score, idx[]} or null
function fuzzy(q, s) {
  const a = q.toLowerCase(), b = s.toLowerCase();
  let i = 0, score = 0, last = -2; const idx = [];
  for (let j = 0; j < b.length && i < a.length; j++) {
    if (b[j] !== a[i]) continue;
    const start = j === 0 || /[^a-z0-9]/.test(b[j - 1]);
    score += 1 + (start ? 6 : 0) + (last === j - 1 ? 4 : 0) - j * 0.01;
    idx.push(j); last = j; i++;
  }
  if (i < a.length) return null;
  const sub = b.indexOf(a);
  if (sub >= 0) { score += 10 + (sub === 0 ? 8 : 0) - b.length * 0.05; idx.length = 0; for (let k = 0; k < a.length; k++) idx.push(sub + k); }
  return { score, idx };
}

const recents = () => { try { const v = JSON.parse(localStorage.getItem(RECENT_KEY)); return Array.isArray(v) ? v.filter(x => typeof x === 'string') : []; } catch (e) { return []; } };
function remember(id) { if (id.startsWith('search:')) return; try { localStorage.setItem(RECENT_KEY, JSON.stringify([id, ...recents().filter(x => x !== id)].slice(0, 6))); } catch (e) { /* not remembered */ } }

const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
function mark(label, idx) {
  if (!idx || !idx.length) return esc(label);
  const set = new Set(idx);
  return [...label].map((ch, i) => (set.has(i) ? '<mark>' + esc(ch) + '</mark>' : esc(ch))).join('');
}

// Several words: each must match (anywhere, any order); scores add up.
function fuzzyMulti(q, s) {
  const toks = q.split(/\s+/).filter(Boolean);
  if (toks.length < 2) return fuzzy(q, s);
  let score = 0; const idx = [];
  for (const t of toks) { const f = fuzzy(t, s); if (!f) return null; score += f.score; idx.push(...f.idx); }
  return { score, idx };
}

// Plain-word synonyms: every word typed must start a word of the keywords (or be inside one, from 4 letters).
function kwMatch(q, kw) {
  const words = kw.split(/\s+/);
  return q.toLowerCase().split(/\s+/).filter(Boolean).every(t => words.some(w => w.startsWith(t)) || (t.length >= 4 && kw.includes(t)));
}

function filter(q) {
  q = q.trim();
  if (!q) {
    const r = recents(), byId = new Map(items.map(i => [i.id, i]));
    const rec = r.map(id => byId.get(id)).filter(Boolean).map(i => ({ ...i, group: 'Recent' }));
    const rest = items.filter(i => !r.includes(i.id) && ['go:live', 'go:events', 'act:shield', 'act:view', 'act:timeline', 'act:keys'].includes(i.id));
    return [...rec, ...rest];
  }
  const res = [];
  for (const it of items) {
    const f = fuzzyMulti(q, it.label), h = f ? null : fuzzyMulti(q, it.hint);
    if (f) res.push({ ...it, score: f.score + (it.group === 'Go to' && it.id.startsWith('go:') ? 9 : 0), idx: f.idx });
    else if (h && h.score > q.length * 5) res.push({ ...it, score: h.score - 8, idx: null });
    else if (it.kw && kwMatch(q, it.kw)) res.push({ ...it, score: q.length * 3.5, idx: null });
  }
  res.sort((a, b) => b.score - a.score);
  const top = res.slice(0, 40);
  const weak = !top.length || top[0].score < q.length * 3;
  top[weak ? 'unshift' : 'push']({ id: 'search:' + q, group: 'Events', icon: 'search', label: 'Search events for "' + q + '"', hint: 'Smart search', keys: [], run: () => searchEvents(q), score: 0 });
  return top;
}

async function searchEvents(q) {
  await goTab('events');
  setTimeout(() => {
    const f = $('fSmart');
    if (f && !f.closest('[hidden]')) { f.value = q; f.focus(); f.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true })); }
    else toast('Smart search is not turned on (Settings > Smart features > Smart search)');
  }, 150);
}

const coarse = () => { try { return matchMedia('(pointer: coarse)').matches; } catch (e) { return false; } };
function paint() {
  shown = filter(input.value);
  sel = Math.min(sel, Math.max(0, shown.length - 1));
  if (!shown.length) { list.innerHTML = '<div class="cp-empty">No matching commands</div>'; return; }
  let g = '', h = '';
  shown.forEach((it, i) => {
    if (it.group !== g) { g = it.group; h += '<div class="cp-h">' + esc(g) + '</div>'; }
    h += '<div class="cp-row' + (i === sel ? ' on' : '') + '" role="option" data-i="' + i + '" id="cpo' + i + '">' + ic(it.icon) +
      '<span class="cp-lab">' + mark(it.label, it.idx) + '</span><span class="cp-hint">' + esc(it.hint) + '</span>' +
      (it.keys.length && !coarse() ? '<span class="cp-key">' + it.keys.map(k => '<kbd>' + esc(k) + '</kbd>').join('') + '</span>' : '') + '</div>';
  });
  list.innerHTML = h;
  input.setAttribute('aria-activedescendant', 'cpo' + sel);
}
function move(d) {
  if (!shown.length) return;
  sel = (sel + d + shown.length) % shown.length;
  list.querySelector('.on')?.classList.remove('on');
  const el = list.querySelector('[data-i="' + sel + '"]');
  if (el) { el.classList.add('on'); el.scrollIntoView({ block: 'nearest' }); input.setAttribute('aria-activedescendant', el.id); }
}
function run(i) {
  const it = shown[i];
  if (!it) return;
  remember(it.id);
  // (The palette's history step is taken back first, so a page change made by the command is not undone by it.)
  afterOverlayClose(close).then(() => it.run()).catch((e) => toast(String(e.message || e), { kind: 'err' }));
}

function ensure() {
  if (root) return;
  if (!document.getElementById('paletteCss')) {
    const l = document.createElement('link');
    l.id = 'paletteCss'; l.rel = 'stylesheet'; l.href = '/static/css/palette.css';
    document.head.appendChild(l);
  }
  root = document.createElement('div');
  root.className = 'cp-scrim'; root.hidden = true;
  root.innerHTML = '<div class="cp" role="dialog" aria-modal="true" aria-label="Command palette"><div class="cp-in">' + ic('search') +
    '<input type="text" role="combobox" aria-expanded="true" aria-controls="cpList" autocomplete="off" spellcheck="false" placeholder="Type a command or search..." aria-label="Command"><kbd>Esc</kbd></div><div class="cp-list" id="cpList" role="listbox"></div></div>';
  document.body.appendChild(root);
  input = root.querySelector('input'); list = root.querySelector('.cp-list');
  input.addEventListener('input', () => { sel = 0; paint(); });
  root.addEventListener('mousedown', (e) => { if (e.target === root) close(); });
  list.addEventListener('mousemove', (e) => { const r = e.target.closest('.cp-row'); if (r && +r.dataset.i !== sel) { list.querySelector('.on')?.classList.remove('on'); sel = +r.dataset.i; r.classList.add('on'); } });
  list.addEventListener('click', (e) => { const r = e.target.closest('.cp-row'); if (r) run(+r.dataset.i); });
}

backLayer(50, () => isOpen(), () => close());
export function isOpen() { return !!root && !root.hidden; }
export function open() {
  if (isOpen()) return;
  ensure();
  prevFocus = document.activeElement;
  items = build();
  input.value = ''; sel = 0;
  paint();
  root.hidden = false;
  input.focus();
  overlaysChanged();
  // Profiles load in the background; rebuild the list unless the user already typed.
  loadProfiles().then(() => { if (isOpen() && !input.value) { items = build(); paint(); } else if (isOpen()) { const v = input.value; items = build(); input.value = v; paint(); } });
}
export function close() {
  if (!isOpen()) return;
  root.hidden = true;
  overlaysChanged();
  if (prevFocus && prevFocus.isConnected && prevFocus !== document.body) { try { prevFocus.focus(); } catch (e) { /* gone */ } }
}

window.addEventListener('keydown', (ev) => {
  if ((ev.ctrlKey || ev.metaKey) && !ev.altKey && !ev.shiftKey && ev.key.toLowerCase() === 'k') {
    if (!state.signedIn && document.querySelector('#loginGate.on')) return;
    ev.preventDefault(); ev.stopPropagation();
    if (isOpen()) close(); else open();
    return;
  }
  if (!isOpen()) return;
  const k = ev.key;
  if (k === 'Escape') close();
  else if (k === 'ArrowDown' || (ev.ctrlKey && k === 'n')) move(1);
  else if (k === 'ArrowUp' || (ev.ctrlKey && k === 'p')) move(-1);
  else if (k === 'Enter') run(sel);
  else if (k === 'Tab') { ev.preventDefault(); input.focus(); }
  else return;
  ev.preventDefault(); ev.stopPropagation();
}, true);
