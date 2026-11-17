// timelapse.js - Time-lapse tab: a day of a camera in a minute or so. Made
// each night for yesterday (when turned on), or now by an admin. Admins also
// get the settings here.

import { state, api, esc, enc, fmtSize, toast, swHtml, fld, liveSwitch, formBar } from './core.js';

let el = null, data = null, cam = '', day = '', timer = 0;

const today = () => { const d = new Date(); return d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0') + '-' + String(d.getDate()).padStart(2, '0'); };
const prettyDay = (s) => new Date(s + 'T12:00:00').toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric', year: 'numeric' });

async function load() {
  try { data = await api('/api/timelapse'); } catch (e) { el.innerHTML = '<p class="empty">' + esc(e.message) + '</p>'; return; }
  const cams = [...new Set(data.timelapses.map(t => t.camera))];
  const own = state.cams.filter(c => !c.remote).map(c => c.name);
  if (!cam || !(cams.includes(cam) || own.includes(cam))) cam = cams[0] || own[0] || '';
  paintList();
  clearTimeout(timer);
  if (data.timelapses.some(t => t.making)) timer = setTimeout(() => { if (el && el.isConnected && !el.closest('[hidden]')) load(); }, 5000);
}

// Only the list part is redrawn (on the 5 s refresh too), so edits in the settings card survive.
function paintList() {
  const host = el.querySelector('[data-tllist]');
  if (!host) return;
  const all = data.timelapses;
  const camNames = [...new Set(all.map(t => t.camera).concat(state.cams.filter(c => !c.remote).map(c => c.name)))];
  const mine = all.filter(t => t.camera === cam);
  const ready = mine.filter(t => !t.making);
  if (!ready.some(t => t.date === day)) day = ready.length ? ready[0].date : '';
  const cur = ready.find(t => t.date === day);
  let h = '<div class="tl-lapse">';
  if (camNames.length) h += '<div class="searchbar"><label>Camera <select data-cam>' + camNames.map(n => '<option' + (n === cam ? ' selected' : '') + '>' + esc(n) + '</option>').join('') + '</select></label>' +
    (state.isAdmin ? '<label>Day <input type="date" data-day value="' + today() + '" max="' + today() + '"></label><button class="btn" data-make>Make now</button>' : '') + '</div>';
  if (!all.length && !data.enabled) {
    h += '<p class="empty">No time-lapses yet.' + (state.isAdmin ? ' Turn them on below, or make one now.' : ' An admin can turn them on.') + '</p>';
  }
  if (mine.length) {
    h += '<div class="chips tl-days">' + mine.map(t => '<button class="chip' + (t.date === day ? ' on' : '') + '" data-d="' + t.date + '"' + (t.making ? ' disabled' : '') + '>' + esc(prettyDay(t.date)) + (t.making ? ' (making…)' : '') + '</button>').join('') + '</div>';
  } else if (cam && (all.length || data.enabled)) {
    h += '<p class="hint">No time-lapse of ' + esc(cam) + ' yet.</p>';
  }
  if (cur) {
    const url = '/api/timelapse/' + enc(cur.slug) + '/' + cur.date;
    h += '<div class="tl-player"><video controls playsinline preload="metadata" src="' + url + '"></video>' +
      '<div class="row"><span class="hint">' + esc(cam) + ' · ' + esc(prettyDay(cur.date)) + ' · ' + fmtSize(cur.bytes) + '</span><a class="btn" href="' + url + '?download=1" download>Download</a></div></div>';
  }
  h += '</div>';
  host.innerHTML = h;
  const q = (s) => host.querySelector(s);
  const sel = q('[data-cam]');
  if (sel) sel.onchange = (e) => { cam = e.target.value; day = ''; paintList(); };
  host.querySelectorAll('[data-d]').forEach(b => { b.onclick = () => { day = b.dataset.d; paintList(); }; });
  const mk = q('[data-make]');
  if (mk) mk.onclick = async () => {
    mk.disabled = true;
    try {
      const r = await api('/api/timelapse', { json: { camera: cam, date: q('[data-day]').value || today() } });
      toast(r.started ? 'Making the time-lapse; it takes a minute or two' : 'Already being made');
      load();
    } catch (e) { toast(e.message); mk.disabled = false; }
  };
}

// First draw of the tab: the list host and (for admins) the settings card, drawn once.
function start() {
  el.innerHTML = '<div data-tllist></div>' + (state.isAdmin ? '<div data-tlset></div>' : '');
  if (state.isAdmin) settingsCard(el.querySelector('[data-tlset]'));
  load();
}

async function settingsCard(host) {
  let s;
  try { s = (await api('/api/settings')).timelapse; } catch (e) { return; }
  const cams = state.cams.filter(c => !c.remote && c.record !== false).map(c => c.name);
  const all = !s.cameras || !s.cameras.length;
  host.innerHTML = '<div class="ac-card tl-set"><div class="ac-card-t">Time-lapse settings</div>' +
    swHtml('data-en', s.enabled, 'Make one each night for yesterday') +
    '<div class="ac-grid3">' + fld('Length', 'data-sec min="10" max="600" value="' + s.seconds + '"', 's') +
    fld('Made at', 'data-hour min="0" max="23" value="' + s.hour + '"', 'o\'clock (0-23)') +
    fld('Keep for', 'data-keep min="0" max="3650" value="' + s.keep_days + '"', 'days', '0 keeps them always') + '</div>' +
    '<div class="hint">A day becomes one picture every ' + Math.round(86400 / (s.seconds * 30)) + ' s, played at 30 per second. It is made from keyframes, so it takes a minute or two per camera at night.</div>' +
    '<div class="sec">Cameras</div>' + swHtml('data-all', all, 'All cameras that record') +
    '<div class="cs-copygrid" data-camset' + (all ? ' hidden' : '') + '>' + cams.map(n => '<label class="asys-chk"><input type="checkbox" data-c="' + esc(n) + '"' + (all || s.cameras.includes(n) ? ' checked' : '') + '> ' + esc(n) + '</label>').join('') + '</div></div>';
  const q = (x) => host.querySelector(x);
  const body = () => ({
    enabled: q('[data-en]').checked,
    seconds: Math.max(10, Math.min(600, +q('[data-sec]').value || 60)),
    hour: Math.max(0, Math.min(23, +q('[data-hour]').value || 0)),
    keep_days: Math.max(0, +q('[data-keep]').value || 0),
    cameras: q('[data-all]').checked ? [] : [...host.querySelectorAll('[data-c]:checked')].map(x => x.dataset.c),
  });
  const put = async (b) => { const r = await api('/api/settings', { json: { timelapse: b } }); if (r && r.error) throw new Error(r.error); s = Object.assign(s, b); };
  // Switches save when flipped (with the other values as last saved); the numbers and camera ticks use the save bar.
  const saved = () => ({ enabled: s.enabled, seconds: s.seconds, hour: s.hour, keep_days: s.keep_days, cameras: s.cameras || [] });
  liveSwitch(q('[data-en]'), (on) => put(Object.assign(saved(), { enabled: on })));
  // On = every camera (an empty list). Off saves the explicit list of all cameras, so the switch
  // always says what the server does; untick cameras afterwards and use the save bar.
  liveSwitch(q('[data-all]'), async (on) => {
    await put(Object.assign(saved(), { cameras: on ? [] : cams.slice() }));
    host.querySelectorAll('[data-c]').forEach(x => { if (!on) x.checked = true; });
    q('[data-camset]').hidden = on;
  });
  formBar(host, { save: async () => { await put(body()); load(); } });
}

export const timelapseTab = {
  id: 'timelapse', label: 'Time-lapse', order: 65,
  init(e) { el = e; },
  show() { start(); },
  leaveHash: () => '#timelapse',
  hide() { clearTimeout(timer); },
};
