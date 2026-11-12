// layouts.js - saved live-grid layouts and live-page settings, kept on the server per user
// (through prefs.js).
//
// A layout = { id, name, per: 'auto' | 1 | 4 | 9 | 16, cams: [camera names in display order],
// group?: name }. Empty `cams` means "every camera the server lists". A layout that goes
// with a group (its `group`, or a group with the layout's own name) shows that group's
// cameras as they are now, in the layout's order. The built-in layout "All" is never stored.
// Server prefs keys: layouts, activeLayout, quality ('light' | 'smooth'), cycle { on, sec },
// viewStyles { viewKey: 'fit' | 'focus' | 'focus2' } (missing = 'grid').
// Older versions kept these in localStorage; they are moved to the server once.

import { state, bus, openDlg, esc, msgDlg, raiseDlg, toast } from './core.js';
import { setGroup } from './groups.js';
import * as Prefs from './prefs.js';

const KEY = 'ambs.layouts.v1';
const PREF_KEY = 'ambs.liveprefs.v1';

export const ALL = { id: '', name: 'All', per: 'auto', cams: [] };

let data = { list: [], active: '' };
let prefs = { cycle: false, sec: 10, quality: 'light', boxes: 'off' };

function valid(l) {
  return l && typeof l.id === 'string' && typeof l.name === 'string' && Array.isArray(l.cams) &&
    (l.per === 'auto' || [1, 4, 9, 16].includes(l.per));
}
function readLocal() {
  let layouts = null, lp = null;
  try {
    const d = JSON.parse(localStorage.getItem(KEY) || 'null');
    if (d && Array.isArray(d.list)) layouts = { list: d.list.filter(valid), active: d.active || '' };
  } catch (e) { /* corrupt or storage disabled */ }
  try { lp = JSON.parse(localStorage.getItem(PREF_KEY) || 'null'); } catch (e) { /* ignore */ }
  return { layouts, lp };
}

// Load from the server (call once after sign-in, before the first render).
export async function init() {
  await Prefs.load();
  const l = Prefs.get('layouts', null);
  let migrate = false;
  data = { list: Array.isArray(l) ? l.filter(valid) : [], active: Prefs.get('activeLayout', '') || '' };
  // Older versions kept layouts in this browser's storage, per address (so the
  // LAN address and the Tailscale address could each hold different ones).
  // Merge whatever this browser still has into the server's list, skipping
  // ones the server already has, then clear the local copy once saved.
  const { layouts, lp } = readLocal();
  if (layouts && layouts.list.length && Prefs.serverOk) {
    const have = new Set(data.list.map(x => x.id));
    const sig = (x) => x.name + '|' + x.per + '|' + x.cams.join(',');
    const haveSig = new Set(data.list.map(sig));
    for (const x of layouts.list) {
      if (have.has(x.id) || haveSig.has(sig(x))) continue;
      data.list.push(x);
    }
    if (!data.active && layouts.active && data.list.some(x => x.id === layouts.active)) data.active = layouts.active;
    migrate = true;
  }
  if (lp && Prefs.get('cycle', null) === null) {
    Prefs.set({ cycle: { on: !!lp.cycle, sec: Math.max(3, parseInt(lp.sec, 10) || 10) }, quality: lp.smooth ? 'smooth' : 'light' });
  }
  const c = Prefs.get('cycle', { on: false, sec: 10 });
  prefs = { cycle: !!c.on, sec: Math.max(3, parseInt(c.sec, 10) || 10), quality: Prefs.get('quality', 'light') === 'smooth' ? 'smooth' : 'light', boxes: Prefs.get('boxes', 'off') === 'motion' ? 'motion' : 'off' };
  if (migrate) {
    Prefs.set({ layouts: data.list, activeLayout: data.active });
    if (await Prefs.flush()) {
      try { localStorage.removeItem(KEY); localStorage.removeItem(PREF_KEY); } catch (e) { /* ignore */ }
    }
  }
  bus.emit('layouts');
  bus.emit('layouts-prefs');
}

function persist() { Prefs.set({ layouts: data.list, activeLayout: data.active }); }

export function list() { return [ALL].concat(data.list); }

// The group a layout follows: its own `group`, else a group with the same name
// (a layout saved with "Also make a group" before layouts remembered it). '' = none.
export function followsGroup(l) {
  if (!l || !l.id) return '';
  const groups = [...new Set((state.cams || []).flatMap(c => c.groups || []))];
  const want = (l.group || l.name || '').toLowerCase();
  return groups.find(g => g.toLowerCase() === want) || '';
}
// The camera names a layout shows, in its order ([] for "All").
export function camsOf(l) {
  const g = followsGroup(l);
  if (!g) return (l && l.cams) || [];
  const members = (state.cams || []).filter(c => (c.groups || []).includes(g)).map(c => c.name);
  const saved = (l.cams || []).filter(n => members.includes(n));
  return saved.concat(members.filter(n => !saved.includes(n)));
}
// Whether a layout picks its own cameras (not every camera).
export const isCustom = (l) => !!followsGroup(l) || !!(l && l.cams && l.cams.length);
export function active() { return data.list.find(l => l.id === data.active) || ALL; }
export function setActive(id) {
  data.active = data.list.some(l => l.id === id) ? id : '';
  persist();
  bus.emit('layouts');
}
export function getPrefs() { return prefs; }
export function setPrefs(p) {
  prefs = Object.assign({}, prefs, p);
  prefs.sec = Math.max(3, parseInt(prefs.sec, 10) || 10);
  prefs.quality = prefs.quality === 'smooth' ? 'smooth' : 'light';
  prefs.boxes = prefs.boxes === 'motion' ? 'motion' : 'off';
  Prefs.set({ quality: prefs.quality, boxes: prefs.boxes, cycle: { on: !!prefs.cycle, sec: prefs.sec } });
  bus.emit('layouts-prefs');
}

// View style per view key ('all', 'group:<name>' or 'layout:<id>'), per user.
const STYLE_SET = ['grid', 'fit', 'focus', 'focus2'];
export function styleFor(key) {
  const s = Prefs.get('viewStyles', {}) || {};
  return STYLE_SET.includes(s[key]) ? s[key] : 'grid';
}
export function setStyle(key, style) {
  const s = Object.assign({}, Prefs.get('viewStyles', {}) || {});
  if (style === 'grid') delete s[key]; else s[key] = style;
  Prefs.set({ viewStyles: s });
}

// New camera order for a saved layout (from Arrange). `names` may be a subset (the group
// filter is on): those cameras swap among their own places, the rest stay put.
export function reorderLayout(id, names) {
  const l = data.list.find(x => x.id === id);
  if (!l) return;
  const set = new Set(names);
  let i = 0;
  l.cams = camsOf(l).map(n => (set.has(n) ? names[i++] : n));
  persist();
}

function saveLayout(l) {
  const i = data.list.findIndex(x => x.id === l.id);
  if (i >= 0) data.list[i] = l; else data.list.push(l);
  data.active = l.id;
  persist();
  bus.emit('layouts');
}
function deleteLayout(id) {
  data.list = data.list.filter(l => l.id !== id);
  if (data.active === id) data.active = '';
  persist();
  bus.emit('layouts');
}

// ---------- editor dialog ----------
let editorDlg = null;
export function openEditor() {
  if (raiseDlg(editorDlg)) return;
  const d = editorDlg = openDlg('Live layouts',
    '<div class="hint">Layouts are saved to your account. Pick how many tiles per page and which cameras appear, in which order.</div>' +
    '<label>Layout <select id="lyPick"></select></label>' +
    '<label>Name <input id="lyName" maxlength="40" placeholder="Front of house"></label>' +
    '<label>Tiles per page <select id="lyPer"><option value="auto">Auto (all on one page)</option><option value="1">1</option><option value="4">4 (2 x 2)</option><option value="9">9 (3 x 3)</option><option value="16">16 (4 x 4)</option></select></label>' +
    '<div class="sec">Cameras (tick to show, arrows to reorder; none ticked = all cameras)</div>' +
    '<div id="lyFollows" class="hint" hidden></div>' +
    '<div id="lyCams" class="lycams"></div>' +
    (state.isAdmin ? '<div class="lygroup"><label class="asys-chk"><input type="checkbox" id="lyMkGroup"> Also make a group from these cameras</label>' +
      '<label class="lygname">Group name <input id="lyGroup" maxlength="40" disabled></label>' +
      '<div class="hint">Groups filter the live view and can be used by other settings. Making one never turns on recording or triggering. If a group with this name already exists, its cameras become these.</div></div>' : '') +
    '<div class="row"><button class="primary" id="lySave">Save layout</button> <button id="lyDel">Delete</button></div>');
  const $d = (s) => d.querySelector(s);
  let cur = null;      // layout being edited (copy)
  let order = [];      // [{name, on}]

  function fillPick(selectId) {
    const pick = $d('#lyPick');
    pick.innerHTML = '<option value="__new">+ New layout</option>' +
      data.list.map(l => '<option value="' + esc(l.id) + '">' + esc(l.name) + '</option>').join('');
    pick.value = selectId;
  }
  function load(id) {
    const l = data.list.find(x => x.id === id);
    cur = l ? JSON.parse(JSON.stringify(l)) : { id: 'l' + Date.now().toString(36), name: '', per: 'auto', cams: [], isNew: true };
    $d('#lyName').value = cur.name;
    $d('#lyPer').value = String(cur.per);
    const names = state.cams.map(c => c.name);
    const follows = l ? followsGroup(l) : '';
    const chosen = (l ? camsOf(l) : cur.cams).filter(n => names.includes(n));
    cur.follows = follows;
    const note = $d('#lyFollows');
    note.hidden = !follows;
    note.textContent = follows ? 'This layout shows the cameras of the group ' + follows + ': cameras added to that group appear here by themselves.' + (state.isAdmin ? ' Ticking cameras here changes the group too.' : '') : '';
    order = chosen.map(n => ({ name: n, on: true })).concat(names.filter(n => !chosen.includes(n)).map(n => ({ name: n, on: false })));
    $d('#lyDel').hidden = !l;
    drawCams();
  }
  function drawCams() {
    const box = $d('#lyCams');
    box.innerHTML = '';
    order.forEach((o, i) => {
      const row = document.createElement('div');
      row.className = 'lycam';
      row.innerHTML = '<label><input type="checkbox"' + (o.on ? ' checked' : '') + '> ' + esc(o.name) + '</label>' +
        '<button title="up" aria-label="move up">&#9650;</button><button title="down" aria-label="move down">&#9660;</button>';
      const [up, down] = row.querySelectorAll('button');
      row.querySelector('input').onchange = (ev) => { o.on = ev.target.checked; };
      up.onclick = () => { if (i > 0) { [order[i - 1], order[i]] = [order[i], order[i - 1]]; drawCams(); } };
      down.onclick = () => { if (i < order.length - 1) { [order[i + 1], order[i]] = [order[i], order[i + 1]]; drawCams(); } };
      box.appendChild(row);
    });
    if (!order.length) box.innerHTML = '<div class="empty">No cameras yet.</div>';
  }

  // Group from layout: the name follows the layout name until edited.
  const mk = $d('#lyMkGroup'), gname = $d('#lyGroup');
  let gnameEdited = false;
  if (mk) {
    mk.onchange = () => { gname.disabled = !mk.checked; if (mk.checked && !gnameEdited) gname.value = $d('#lyName').value.trim(); };
    gname.oninput = () => { gnameEdited = true; };
    $d('#lyName').addEventListener('input', () => { if (!gnameEdited && mk.checked) gname.value = $d('#lyName').value.trim(); });
  }
  $d('#lyPick').onchange = (ev) => load(ev.target.value === '__new' ? '' : ev.target.value);
  $d('#lySave').onclick = () => {
    const name = $d('#lyName').value.trim();
    if (!name) { msgDlg('Give the layout a name'); return; }
    const per = $d('#lyPer').value === 'auto' ? 'auto' : parseInt($d('#lyPer').value, 10);
    const cams = order.filter(o => o.on).map(o => o.name);
    // Cameras this page does not list (in a split-view pane: the other
    // server's) stay in the layout, where they were.
    const seen = new Set(state.cams.map(c => c.name));
    const kept = (cur.cams || []).filter(n => !seen.has(n));
    const follows = cur.follows || '';
    const saved = { id: cur.id, name, per, cams: cams.concat(kept) };
    if (follows) saved.group = follows;
    if (mk && mk.checked) saved.group = gname.value.trim() || name;
    saveLayout(saved);
    // A layout that follows a group: its ticked cameras are the group's members.
    // None ticked means "all cameras" for the layout: never empty the group.
    if (follows && state.isAdmin && !(mk && mk.checked) && cams.length) {
      const ids = cams.map(n => (state.cams.find(c => c.name === n) || {}).id).filter(x => x != null);
      setGroup(follows, ids).catch(e => msgDlg('The layout was saved, but the group was not: ' + (e.body || e.message)));
    }
    if (mk && mk.checked) {
      const g = gname.value.trim() || name;
      const ids = (cams.length ? cams : state.cams.map(c => c.name)).map(n => (state.cams.find(c => c.name === n) || {}).id).filter(x => x != null);
      setGroup(g, ids).then(() => toast('Layout saved and group ' + g + ' made')).catch(e => msgDlg('The layout was saved, but the group was not: ' + (e.body || e.message)));
      mk.checked = false; gname.disabled = true;
    }
    cur.isNew = false;
    fillPick(cur.id);
    $d('#lyDel').hidden = false;
  };
  $d('#lyDel').onclick = () => {
    if (!cur || cur.isNew) return;
    deleteLayout(cur.id);
    fillPick('__new');
    load('');
  };
  const start = data.active || '__new';
  fillPick(start);
  load(start === '__new' ? '' : start);
}
