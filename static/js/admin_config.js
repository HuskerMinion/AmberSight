// admin_config.js - Admin pages: Profiles & schedule, AI, Storage, Discovery.
// Every page edits a draft copy of what the server holds, shows an "unsaved changes" bar,
// and saves with POST /api/settings (sections MERGE on the server; unsent fields keep their value).

import { api, esc, toast, msgDlg, fmtSize, state, ic, refreshCameras, pageBar, wireBar, confirmDialog, liveSwitch, swHtml, fld, savedToast, errText, stillWaiting } from './core.js';
import { lead, how, more, notice } from './sp_kit.js';
import { registerAdminPage } from './admin.js';
import { showTab } from './nav.js';
import { addCameraConfig } from './cameras.js';
import { openSettingsDialog } from './settings.js';
import { armStrip, armData } from './armstrip.js';

const clone = (o) => JSON.parse(JSON.stringify(o));
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const DAYS = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];
const DAY_LABEL = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
const HUES = [38, 205, 150, 330, 270, 8, 180, 75];
let timers = [];
const later = (fn, ms) => { const t = setInterval(fn, ms); timers.push(t); };
function stopTimers() { timers.forEach(clearInterval); timers = []; }

// Page lead-in: one line of help. The title is the section and the sub-page tab, so it is not repeated.
function head(title, sub) {
  return sub ? '<div class="ac-head"><div class="hint">' + sub + '</div></div>' : '';
}
async function postSettings(body) {
  const r = await api('/api/settings', { json: body });
  if (r.error) throw new Error(r.error);
  return r;
}
function guardAdmin(el) {
  if (state.isAdmin) return true;
  el.innerHTML = '<p class="empty">Admin sign-in required.</p>';
  return false;
}
const optYesNo = (v, labels) => ['', 'on', 'off'].map((x, i) => '<option value="' + x + '"' + (x === v ? ' selected' : '') + '>' + labels[i] + '</option>').join('');
const triOf = (v) => (v === true ? 'on' : v === false ? 'off' : '');
const triTo = (s) => (s === 'on' ? true : s === 'off' ? false : undefined);

/* ====================================================================== Profiles & schedule */

const hm = (t) => { const m = /^(\d{1,2}):(\d{2})$/.exec((t || '').trim()); return m && +m[1] < 24 && +m[2] < 60 ? +m[1] * 60 + +m[2] : null; };
// A rule time: "HH:MM", or "sunrise" / "sunset" with an optional offset in minutes ("sunset-30").
function parseSpec(t) {
  const s = String(t || '').trim().toLowerCase();
  const m = /^(sunrise|sunset)\s*([+-]\s*\d+)?$/.exec(s);
  if (m) return { kind: m[1], off: m[2] ? parseInt(m[2].replace(/\s/g, ''), 10) : 0 };
  return { kind: 'time', time: s };
}
function specStr(s) { return s.kind === 'time' ? s.time : s.kind + (s.off ? (s.off > 0 ? '+' : '') + s.off : ''); }
// Minute of the day; sun = today's { sunrise, sunset } ("HH:MM") from /api/profiles.
function specMin(t, sun) {
  const s = parseSpec(t);
  if (s.kind === 'time') return hm(s.time);
  const b = sun ? hm(s.kind === 'sunrise' ? sun.sunrise : sun.sunset) : null;
  return b == null ? null : (((b + s.off) % 1440) + 1440) % 1440;
}

function ruleMatches(rule, wd, minute, sun) {
  const from = specMin(rule.from, sun), to = specMin(rule.to, sun);
  if (from == null || to == null) return false;
  const on = (d) => !rule.days || !rule.days.length || rule.days.some(x => String(x).trim().toLowerCase().startsWith(DAYS[(d + 7) % 7]));
  if (from <= to) return on(wd) && minute >= from && minute < to;
  return (on(wd) && minute >= from) || (on(wd - 1) && minute < to);
}

// Strip empty overrides so the draft compares equal to what the server stores.
function cleanAuto(a) {
  const o = { default_profile: a.default_profile || '', profiles: [], schedule: [] };
  if (a.latitude != null && a.longitude != null) { o.latitude = a.latitude; o.longitude = a.longitude; }
  for (const p of a.profiles || []) {
    const q = { name: p.name };
    if (p.armed != null) q.armed = p.armed;
    if (p.alerts != null) q.alerts = p.alerts;
    const cams = {};
    for (const [k, v] of Object.entries(p.cameras || {})) {
      const c = {};
      for (const f of ['motion', 'alerts', 'ai', 'record', 'record_motion_only']) if (v[f] != null) c[f] = v[f];
      if (Object.keys(c).length) cams[k] = c;
    }
    q.cameras = cams;
    o.profiles.push(q);
  }
  for (const r of a.schedule || []) o.schedule.push({ days: (r.days || []).slice(), from: r.from, to: r.to, profile: r.profile });
  return o;
}

const OVR = [['motion', 'Motion'], ['alerts', 'Alerts'], ['ai', 'AI'], ['record', 'Record'], ['record_motion_only', 'Motion only']];

function profilesPage(el) {
  if (!guardAdmin(el)) return;
  let saved = null, draft = null, sel = 0, live = null, gMotion = null, shieldOn = true;
  const shown = new Set(), strips = new Map();   // cameras whose "when it watches" strip is open
  el.innerHTML = '<div class="ac"><p class="empty">Loading...</p></div>';

  async function load(keepSel) {
    try {
      const [s, p, stt] = await Promise.all([api('/api/settings'), api('/api/profiles'), api('/api/status').catch(() => ({})), refreshCameras().catch(() => {})]);
      gMotion = (s.motion && s.motion.schedule) || null;
      shieldOn = stt.armed !== false;
      saved = cleanAuto(s.automation || {});
      draft = clone(saved);
      live = p;
      if (!keepSel) sel = 0;
      sel = Math.min(sel, Math.max(0, draft.profiles.length - 1));
      paint();
    } catch (e) { el.innerHTML = '<p class="empty">' + esc(e.message) + '</p>'; }
  }
  async function loadLive() {
    try { live = await api('/api/profiles'); paintLive(); } catch (e) { /* ignore */ }
  }
  const hue = (name) => { const i = draft.profiles.findIndex(p => p.name === name); return i < 0 ? null : HUES[i % HUES.length]; };

  function profileAt(wd, minute) {
    const r = draft.schedule.find(x => ruleMatches(x, wd, minute, live && live.sun));
    if (r) return { name: r.profile, src: 'rule' };
    return { name: draft.default_profile, src: 'default' };
  }

  function paintLive() {
    const box = el.querySelector('[data-live]');
    if (!box || !live) return;
    const names = live.profiles || [];
    const man = live.manual;
    const until = man && man.until ? ' until ' + new Date(man.until * 1000).toLocaleString([], { weekday: 'short', hour: '2-digit', minute: '2-digit' }) : '';
    const h = hue(live.active);
    box.innerHTML =
      '<div class="ac-live-main"><span class="ac-label">Active now</span>' +
      (live.active ? '<span class="ac-pill" style="--h:' + (h == null ? 38 : h) + '">' + esc(live.active) + '</span>' : '<span class="ac-pill none">No profile</span>') +
      '<span class="hint ac-inl">' + (man ? 'switched by hand' + esc(until) : (live.scheduled ? 'from the schedule' : 'cameras use their own settings')) + '</span></div>' +
      '<div class="ac-live-ctl">' +
      '<select data-sw-name aria-label="Profile">' + names.map(n => '<option' + (n === live.active ? ' selected' : '') + '>' + esc(n) + '</option>').join('') + '</select>' +
      '<select data-sw-min aria-label="For how long"><option value="">until changed</option><option value="60">1 hour</option><option value="240">4 hours</option><option value="480">8 hours</option><option value="1440">24 hours</option></select>' +
      '<button class="btn" data-sw-go' + (names.length ? '' : ' disabled') + '>Switch</button>' +
      (man ? '<button class="btn" data-sw-back>Back to schedule</button>' : '') + '</div>';
    const sw = async (name, minutes) => {
      try { live = await api('/api/profile', { json: { name, minutes } }); paintLive(); toast(name ? 'Profile: ' + name : 'Following the schedule'); } catch (e) { msgDlg(e.message); }
    };
    const go = box.querySelector('[data-sw-go]');
    if (go) go.onclick = () => { const m = box.querySelector('[data-sw-min]').value; sw(box.querySelector('[data-sw-name]').value, m ? +m : null); };
    const back = box.querySelector('[data-sw-back]');
    if (back) back.onclick = () => sw(null, null);
  }

  function drawStrip() {
    const box = el.querySelector('[data-strip]');
    if (!box) return;
    const now = new Date();
    const today = (now.getDay() + 6) % 7;
    const nowSlot = now.getHours() * 2 + (now.getMinutes() >= 30 ? 1 : 0);
    let h = '<div class="ac-wk-hours"><span></span>';
    for (let hr = 0; hr < 24; hr++) h += '<span>' + (hr % 3 === 0 ? String(hr).padStart(2, '0') : '') + '</span>';
    h += '</div>';
    for (let d = 0; d < 7; d++) {
      h += '<div class="ac-wk-row' + (d === today ? ' today' : '') + '"><span class="ac-wk-day">' + DAY_LABEL[d] + '</span><div class="ac-wk-cells">';
      for (let k = 0; k < 48; k++) {
        const pr = profileAt(d, k * 30);
        const hh = pr.name ? hue(pr.name) : null;
        const t = String(Math.floor(k / 2)).padStart(2, '0') + ':' + (k % 2 ? '30' : '00');
        h += '<i class="' + (hh == null ? 'none' : pr.src === 'default' ? 'dflt' : '') + (d === today && k === nowSlot ? ' now' : '') + '"' +
          (hh == null ? '' : ' style="--h:' + hh + '"') + ' title="' + DAY_LABEL[d] + ' ' + t + ' - ' + esc(pr.name || 'no profile') + (pr.src === 'default' && pr.name ? ' (default)' : '') + '"></i>';
      }
      h += '</div></div>';
    }
    box.innerHTML = h;
    const lg = el.querySelector('[data-legend]');
    if (lg) lg.innerHTML = draft.profiles.map(p => '<span class="ac-key"><i style="--h:' + hue(p.name) + '"></i>' + esc(p.name) + '</span>').join('') +
      (draft.default_profile ? '<span class="ac-key"><i class="dflt" style="--h:' + hue(draft.default_profile) + '"></i>default (no rule matches)</span>' : '<span class="ac-key"><i class="none"></i>no profile</span>');
  }

  // No profile yet: only the way in. The schedule and location come once a profile exists.
  function paintEmpty() {
    el.innerHTML = '<div class="ac">' + lead('A profile is a mode such as Home or Away that changes how your cameras behave.', 'Start with Home and Away.') +
      '<div class="sp-empty"><h3>No profiles yet</h3>' +
      '<p>Home and Away: Away turns detection on (Armed); Home leaves every camera as it is now. You can switch by hand or set a weekly schedule afterwards.</p>' +
      '<div class="row"><button class="primary" type="button" data-homeaway>Create Home and Away</button><button class="btn" type="button" data-newprof>Create a profile</button></div></div></div>';
    el.querySelector('[data-homeaway]').onclick = async (e) => {
      e.target.disabled = true;
      try {
        const next = clone(draft);
        next.profiles = [{ name: 'Home', cameras: {} }, { name: 'Away', armed: true, cameras: {} }];
        next.default_profile = 'Home';
        await postSettings({ automation: cleanAuto(next) });
        savedToast('Home and Away created');
        await load();
      } catch (err) { toast(errText(err), { kind: 'err' }); e.target.disabled = false; }
    };
    el.querySelector('[data-newprof]').onclick = () => {
      draft.profiles.push({ name: 'Profile 1', cameras: {} }); sel = 0; paint();
      const i = el.querySelector('[data-pname]'); if (i) { i.focus(); i.select(); }
      upd();
    };
  }

  function paint() {
    const P = draft.profiles, p = P[sel];
    // The first-run page only while the saved list is empty too; after deleting the last
    // profile the normal page (with its save bar) stays so the delete can be saved.
    if (!P.length && !saved.profiles.length) { paintEmpty(); return; }
    const camNames = state.cams.map(c => c.name);
    let h = lead('Switch cameras between modes like Home and Away, by hand or on a weekly schedule.', 'One profile per situation, with a schedule only if the times are regular.') +
      how('A profile is a named set of overrides (arming, alerts, per-camera motion / AI / recording). The schedule picks one by day and time; a manual switch wins until you go back to the schedule.') +
      '<div class="ac-card ac-live" data-live></div>' +
      '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">Profiles</div><span class="ac-sp"></span>' +
      '<label class="ac-inl">Default <select data-default><option value="">none (cameras use their own settings)</option>' +
      P.map(x => '<option' + (x.name === draft.default_profile ? ' selected' : '') + '>' + esc(x.name) + '</option>').join('') + '</select></label></div>' +
      '<div class="chips ac-profs">' + P.map((x, i) => '<button class="chip' + (i === sel ? ' on' : '') + '" data-sel="' + i + '"><i class="ac-dot" style="--h:' + HUES[i % HUES.length] + '"></i>' + esc(x.name) + '</button>').join('') +
      '<button class="chip" data-add>+ New profile</button></div>';
    if (p) {
      h += '<div class="ac-edit"><div class="ac-grid3">' +
        '<label>Name <input data-pname value="' + esc(p.name) + '" maxlength="40"></label>' +
        '<label>Armed or Disarmed <select data-parm>' + optYesNo(triOf(p.armed), ['leave as is', 'Armed', 'Disarmed']) + '</select></label>' +
        '<label>Alerts <select data-palr>' + optYesNo(triOf(p.alerts), ['leave as is', 'send', 'mute']) + '</select></label></div>' +
        '<details class="ac-more sp-more"><summary>Per-camera overrides</summary><div class="hint">A dash means the camera keeps its own setting.</div>';
      const keys = camNames.slice();
      for (const k of Object.keys(p.cameras)) if (!keys.includes(k)) keys.push(k);
      if (!keys.length) h += '<p class="hint">No cameras yet.</p>';
      else {
        h += '<div class="ac-ovr"><div class="ac-ovr-h"><span>Camera</span>' + OVR.map(o => '<span>' + o[1] + '</span>').join('') + '</div>';
        for (const k of keys) {
          const o = p.cameras[k] || {};
          h += '<div class="ac-ovr-r"><span class="ac-cam' + (camNames.includes(k) ? '' : ' gone') + '" title="' + (camNames.includes(k) ? '' : 'No camera with this name') + '">' + esc(k) +
            (camNames.includes(k) ? ' <button type="button" class="as-show" data-armshow="' + esc(k) + '" title="Show when this camera detects and alerts over the next 7 days">' + (shown.has(k) ? 'hide' : 'show') + '</button>' : '') + '</span>' +
            OVR.map(o2 => '<label data-l="' + o2[1] + '"><select data-ovr="' + esc(k) + '" data-f="' + o2[0] + '">' + optYesNo(triOf(o[o2[0]]), ['-', 'on', 'off']) + '</select></label>').join('') + '</div>' +
            '<div class="ac-armrow" data-armrow="' + esc(k) + '"' + (shown.has(k) ? '' : ' hidden') + '></div>';
        }
        h += '</div>';
      }
      h += '</details><div class="ac-row"><button class="btn danger" data-del>Delete profile</button></div></div>';
    }
    h += '</div>';
    // schedule
    h += '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">Weekly schedule</div><span class="ac-sp"></span><button class="btn" data-addrule' + (P.length ? '' : ' disabled') + '>' + ic('plus') + ' Add rule</button></div>' +
      '<div class="hint">Rules are checked top to bottom; the first one that covers the moment wins, otherwise the default profile applies. A window that ends before it starts runs past midnight.</div>' +
      '<details class="ac-more sp-more"' + (draft.latitude != null ? ' open' : '') + '><summary>Location (for sunrise and sunset times)</summary><div class="ac-row ac-loc"><span class="ac-label">Location</span>' +
        '<input data-lat type="number" step="0.0001" min="-90" max="90" placeholder="Latitude" aria-label="Latitude" value="' + (draft.latitude != null ? draft.latitude : '') + '">' +
        '<input data-lon type="number" step="0.0001" min="-180" max="180" placeholder="Longitude" aria-label="Longitude" value="' + (draft.longitude != null ? draft.longitude : '') + '">' +
        '<button class="btn" data-geo title="Fill in from this device (the browser asks first)">Use this device\'s location</button>' +
        '<span class="hint ac-inl">' + (live && live.sun ? 'Today: sunrise ' + esc(live.sun.sunrise) + ', sunset ' + esc(live.sun.sunset) : 'Set it to use sunrise / sunset in rules') + '</span></div></details>' +
      '<div class="ac-wk" data-strip></div><div class="ac-legend" data-legend></div><div class="ac-rules">';
    if (!draft.schedule.length) h += '<p class="hint">No rules. The default profile is always active.</p>';
    draft.schedule.forEach((r, i) => {
      h += '<div class="ac-rule" data-i="' + i + '"><span class="ac-n">' + (i + 1) + '</span><div class="ac-days">' +
        DAYS.map((d, di) => '<button class="chip' + ((r.days || []).includes(d) ? ' on' : '') + '" data-day="' + d + '" title="' + DAY_LABEL[di] + '">' + DAY_LABEL[di][0] + (di === 1 || di === 3 ? '<small>' + DAY_LABEL[di][1] + '</small>' : '') + '</button>').join('') + '</div>' +
        '<div class="ac-times">' + timeCtl('from', r.from) + '<span>to</span>' + timeCtl('to', r.to) + '</div>' +
        '<select data-rp>' + P.map(x => '<option' + (x.name === r.profile ? ' selected' : '') + '>' + esc(x.name) + '</option>').join('') + '</select>' +
        '<div class="ac-mv"><button class="btn icon" data-mv="-1" title="Move up" aria-label="Move rule up"' + (i === 0 ? ' disabled' : '') + '>&uarr;</button><button class="btn icon" data-mv="1" title="Move down" aria-label="Move rule down"' + (i === draft.schedule.length - 1 ? ' disabled' : '') + '>&darr;</button><button class="btn icon" data-rm title="Delete rule" aria-label="Delete rule">&times;</button></div></div>';
    });
    h += '</div></div>' + pageBar();
    el.innerHTML = '<div class="ac">' + h + '</div>';
    bind();
    paintLive();
    drawStrip();
    strips.clear();
    for (const n of shown) drawArm(n);
  }
  // The "when does this camera watch" strip, from the draft (so unsaved profile edits show too).
  function drawArm(name) {
    const host = [...el.querySelectorAll('[data-armrow]')].find(x => x.dataset.armrow === name);
    const cam = state.cams.find(c => c.name === name);
    if (!host || !cam) return;
    host.hidden = false;
    const data = armData({ automation: draft, motion: { schedule: gMotion } }, live, { armed: shieldOn });
    const cur = strips.get(name);
    if (cur && host.contains(cur.root)) cur.strip.update(cam, data);
    else { host.textContent = ''; const strip = armStrip(host, cam, data); strips.set(name, { strip, root: host.firstChild }); }
    const b = [...el.querySelectorAll('[data-armshow]')].find(x => x.dataset.armshow === name);
    if (b) b.textContent = 'hide';
  }

  // One end of a rule: Time / Sunrise / Sunset, then a clock time or an offset in minutes.
  function timeCtl(which, val) {
    const s = parseSpec(val);
    return '<span class="ac-tctl" data-tc="' + which + '"><select data-tk aria-label="' + (which === 'from' ? 'Start' : 'End') + '">' +
      [['time', 'Time'], ['sunrise', 'Sunrise'], ['sunset', 'Sunset']].map(o => '<option value="' + o[0] + '"' + (o[0] === s.kind ? ' selected' : '') + '>' + o[1] + '</option>').join('') + '</select>' +
      (s.kind === 'time'
        ? '<input type="time" data-tv value="' + esc(s.time) + '">'
        : '<input type="number" data-to step="5" min="-720" max="720" value="' + (s.off || 0) + '" title="Minutes before (-) or after (+)" aria-label="Offset in minutes"><span class="hint">min</span>') +
      '</span>';
  }

  let upd = () => {};
  function mark() { upd(); drawStrip(); for (const n of shown) drawArm(n); }
  function bind() {
    const q = (s) => el.querySelector(s);
    const P = draft.profiles, p = P[sel];
    upd = wireBar(el, () => !same(cleanAuto(draft), saved), async () => {
      await postSettings({ automation: cleanAuto(draft) });
      await load(true);
    }, () => { draft = clone(saved); sel = Math.min(sel, Math.max(0, draft.profiles.length - 1)); paint(); });
    el.querySelectorAll('[data-sel]').forEach(b => { b.onclick = () => { sel = +b.dataset.sel; paint(); }; });
    q('[data-add]').onclick = () => {
      let n = 1, name;
      do { name = 'Profile ' + n++; } while (P.some(x => x.name === name));
      P.push({ name, cameras: {} }); sel = P.length - 1; paint(); const i = q('[data-pname]'); if (i) { i.focus(); i.select(); }
    };
    q('[data-default]').onchange = (e) => { draft.default_profile = e.target.value; mark(); };
    if (p) {
      q('[data-pname]').onchange = (e) => {
        const n = e.target.value.trim(), old = p.name;
        if (!n) { toast('A profile needs a name'); e.target.value = old; return; }
        if (P.some((x, i) => i !== sel && x.name.toLowerCase() === n.toLowerCase())) { toast('That name is taken'); e.target.value = old; return; }
        p.name = n;
        draft.schedule.forEach(r => { if (r.profile === old) r.profile = n; });
        if (draft.default_profile === old) draft.default_profile = n;
        paint(); upd();
      };
      q('[data-parm]').onchange = (e) => { const v = triTo(e.target.value); if (v === undefined) delete p.armed; else p.armed = v; mark(); };
      q('[data-palr]').onchange = (e) => { const v = triTo(e.target.value); if (v === undefined) delete p.alerts; else p.alerts = v; mark(); };
      el.querySelectorAll('[data-armshow]').forEach(b => {
        b.onclick = () => {
          const n = b.dataset.armshow;
          const host = [...el.querySelectorAll('[data-armrow]')].find(x => x.dataset.armrow === n);
          if (shown.has(n)) { shown.delete(n); const e = strips.get(n); if (e) e.strip.destroy(); strips.delete(n); host.hidden = true; host.textContent = ''; b.textContent = 'show'; }
          else { shown.add(n); drawArm(n); }
        };
      });
      el.querySelectorAll('[data-ovr]').forEach(s => {
        s.onchange = () => {
          const k = s.dataset.ovr, f = s.dataset.f, v = triTo(s.value);
          p.cameras[k] = p.cameras[k] || {};
          if (v === undefined) delete p.cameras[k][f]; else p.cameras[k][f] = v;
          mark();
        };
      });
      q('[data-del]').onclick = async () => {
        const used = draft.schedule.filter(r => r.profile === p.name).length;
        if (!(await confirmDialog({ title: 'Delete profile ' + p.name + '?', body: used ? 'Its ' + used + ' schedule rule' + (used === 1 ? '' : 's') + ' are deleted too. Takes effect when you save.' : 'Takes effect when you save.', ok: 'Delete profile', danger: true }))) return;
        draft.schedule = draft.schedule.filter(r => r.profile !== p.name);
        if (draft.default_profile === p.name) draft.default_profile = '';
        P.splice(sel, 1); sel = Math.max(0, sel - 1); paint(); upd();
      };
    }
    const setLoc = () => {
      const la = parseFloat(q('[data-lat]').value), lo = parseFloat(q('[data-lon]').value);
      if (Number.isFinite(la) && Number.isFinite(lo)) { draft.latitude = la; draft.longitude = lo; } else { delete draft.latitude; delete draft.longitude; }
      upd();
    };
    q('[data-lat]').oninput = setLoc;
    q('[data-lon]').oninput = setLoc;
    q('[data-geo]').onclick = () => {
      if (!navigator.geolocation) { toast('This browser cannot share its location'); return; }
      navigator.geolocation.getCurrentPosition((pos) => {
        q('[data-lat]').value = pos.coords.latitude.toFixed(4);
        q('[data-lon]').value = pos.coords.longitude.toFixed(4);
        setLoc();
      }, (e) => toast('Location not available: ' + e.message), { timeout: 10000 });
    };
    q('[data-addrule]').onclick = () => {
      draft.schedule.push({ days: [], from: '22:00', to: '06:00', profile: (p || P[0]).name });
      paint(); upd();
    };
    el.querySelectorAll('.ac-rule').forEach(row => {
      const r = draft.schedule[+row.dataset.i];
      row.querySelectorAll('[data-day]').forEach(b => {
        b.onclick = () => {
          const d = b.dataset.day;
          r.days = (r.days || []).includes(d) ? r.days.filter(x => x !== d) : DAYS.filter(x => x === d || (r.days || []).includes(x));
          if (r.days.length === 7) r.days = [];
          b.classList.toggle('on'); mark();
        };
      });
      row.querySelectorAll('[data-tc]').forEach(tc => {
        const k = tc.dataset.tc;
        tc.querySelector('[data-tk]').onchange = (e) => {
          const kind = e.target.value;
          r[k] = kind === 'time' ? (k === 'from' ? '22:00' : '06:00') : kind;
          paint(); upd();
        };
        const tv = tc.querySelector('[data-tv]');
        if (tv) tv.oninput = () => { r[k] = tv.value; mark(); };
        const to = tc.querySelector('[data-to]');
        if (to) to.oninput = () => { const s = parseSpec(r[k]); s.off = Math.max(-720, Math.min(720, parseInt(to.value, 10) || 0)); r[k] = specStr(s); mark(); };
      });
      row.querySelector('[data-rp]').onchange = (e) => { r.profile = e.target.value; mark(); };
      row.querySelector('[data-rm]').onclick = () => { draft.schedule.splice(+row.dataset.i, 1); paint(); upd(); };
      row.querySelectorAll('[data-mv]').forEach(b => {
        b.onclick = () => {
          const i = +row.dataset.i, j = i + +b.dataset.mv, s = draft.schedule;
          [s[i], s[j]] = [s[j], s[i]]; paint(); upd();
        };
      });
    });
  }

  load();
  later(loadLive, 15000);
}

/* ====================================================================== AI */

const AI_LABELS = ['person', 'bicycle', 'car', 'motorcycle', 'bus', 'truck', 'dog', 'cat', 'vehicle', 'animal', 'visitor'];

function aiPage(el) {
  if (!guardAdmin(el)) return;
  let saved, draft, status;
  el.innerHTML = '<div class="ac"><p class="empty">Loading...</p></div>';
  const norm = (a) => ({
    enabled: !!a.enabled, source: a.model ? 'builtin' : (a.url ? 'server' : 'builtin'),
    model: a.model || '', url: a.url || '', min_score: +(+a.min_score).toFixed(2),
    labels: (a.labels || []).slice(), max_checks: a.max_checks, threads: a.threads, ignore_parked: a.ignore_parked !== false, strict: !!a.strict, hide_unconfirmed: !!a.hide_unconfirmed, zone_interval_ms: a.zone_interval_ms || 700, zone_hold_seconds: a.zone_hold_seconds == null ? 600 : a.zone_hold_seconds,
    gpu: a.gpu !== false, gpu_name: a.gpu_name || '',
  });
  async function load() {
    try {
      const [s] = await Promise.all([api('/api/settings'), refreshCameras().catch(() => {})]);
      saved = norm(s.ai); draft = clone(saved); status = s.ai_status || {};
      paint();
      refreshModels();
    } catch (e) { el.innerHTML = '<p class="empty">' + esc(e.message) + '</p>'; }
  }
  // Built-in model choice: download with progress, then it is used at once.
  let mstat = null, mpick = null, mtimer = 0;
  function modelCards() {
    if (!mstat) return '<p class="hint">Loading models...</p>';
    const job = mstat.job || {};
    const busy = job.state === 'downloading' || job.state === 'loading';
    const pick = mpick || mstat.current || 's';
    const pct = job.total ? Math.round(job.bytes / job.total * 100) : null;
    return '<div class="ai-choice">' + mstat.models.map(m => {
      const inUse = mstat.current === m.id && mstat.loaded;
      return '<label class="ai-opt' + (pick === m.id ? ' on' : '') + '"><input type="radio" name="aimodel" value="' + m.id + '"' + (pick === m.id ? ' checked' : '') + (busy ? ' disabled' : '') + '>' +
        '<span><span class="ai-name"><b>' + esc(m.name) + '</b>' + (inUse ? '<span class="badge on dot">in use</span>' : '') + (m.id === 's' ? '<span class="badge info">recommended</span>' : '') + '</span>' +
        '<span class="hint">' + esc(m.about) + (m.downloaded === false ? ' Downloaded when you choose it.' : '') + '</span>' +
        (inUse && mstat.p50 != null ? '<span class="hint">Measured here: ' + (mstat.p50 >= 1000 ? (mstat.p50 / 1000).toFixed(1) + ' s' : Math.round(mstat.p50) + ' ms') + ' per check' + (mstat.device ? ' on ' + esc(mstat.device) : '') + '.</span>' : '') + '</span></label>';
    }).join('') + '</div>' +
      (busy ? '<div class="ai-prog"><div class="ai-bar"><i style="width:' + (pct == null ? 30 : pct) + '%"></i></div><span class="hint">' +
        (job.state === 'loading' ? 'Loading the model...' : 'Downloading' + (job.total ? ' ' + (job.bytes / 1e6).toFixed(1) + ' of ' + (job.total / 1e6).toFixed(1) + ' MB' : '...')) + '</span></div>'
        : '<div class="ac-row">' + ((mstat.current === pick && mstat.loaded) ? '<span class="hint">This model is in use.</span>'
          : '<button class="primary" data-getmodel>' + (mstat.current && mstat.loaded ? 'Switch to ' + esc((mstat.models.find(m => m.id === pick) || {}).name || pick) : 'Download and turn on AI') + '</button>') +
          (job.state === 'error' ? '<span class="ac-err">Could not download the AI model: the download site did not answer, or the file did not arrive intact. Nothing is wrong with your cameras or this server. Try again in a few minutes.</span>' : '') + '</div>' +
          '<div class="hint">The model comes from the official YOLOX release and is checked against its published checksum. It runs on this server; pictures never leave it.</div>');
  }
  let watchingJob = false;
  async function refreshModels() {
    try { mstat = await api('/api/ai/model'); } catch (e) { return; }
    // The measured time per check of the model in use (from the live stats).
    try { const st = await api('/api/stats'); mstat.p50 = st && st.ai && st.ai.checks_1m > 0 ? st.ai.p50_ms : null; mstat.device = st && st.ai && st.ai.device; } catch (e) { mstat.p50 = null; }
    const box = el.querySelector('[data-models]');
    if (box) { box.innerHTML = modelCards(); bindModels(); }
    const job = mstat.job || {};
    clearTimeout(mtimer);
    if ((job.state === 'downloading' || job.state === 'loading') && el.isConnected) { watchingJob = true; mtimer = setTimeout(refreshModels, 600); }
    // A download or switch this page watched has finished: reload the saved
    // settings, so a later save does not send the old model path back.
    else if (job.state === 'ready' && watchingJob) { watchingJob = false; load(); }
  }
  function bindModels() {
    const box = el.querySelector('[data-models]');
    if (!box) return;
    box.querySelectorAll('input[name=aimodel]').forEach(r => { r.onchange = () => { mpick = r.value; box.innerHTML = modelCards(); bindModels(); }; });
    const go = box.querySelector('[data-getmodel]');
    if (go) go.onclick = async () => {
      go.disabled = true;
      watchingJob = true;
      try { await api('/api/ai/model', { json: { model: mpick || (mstat && mstat.current) || 's' } }); }
      catch (e) { toast('Could not start: ' + (e.body || e.message)); }
      refreshModels();
    };
  }

  function paint() {
    const labels = AI_LABELS.concat(draft.labels.filter(l => !AI_LABELS.includes(l)));
    const sBuiltin = draft.source === 'builtin';
    let badge = '';
    if (draft.enabled && sBuiltin && saved.model) {
      badge = status.builtin_loaded ? '<span class="badge on dot">model loaded</span>' :
        (status.model_exists === false ? '<span class="badge off dot">model file not found</span>' : '<span class="badge off dot">model not loaded</span>');
    } else if (saved.model && status.model_exists === false) badge = '<span class="badge off dot">model file not found</span>';
    else if (saved.model && status.model_exists) badge = '<span class="badge info dot">model file found</span>';
    const camRows = state.cams.map(c => '<tr><td>' + esc(c.name) + '</td><td>' + (c.ai === false ? '<span class="badge off">off</span>' : c.ai ? '<span class="badge on">on</span>' : '<span class="badge">default</span>') +
      '</td><td class="hint">' + (c.ai_labels && c.ai_labels.length ? esc(c.ai_labels.join(', ')) : 'global labels') + '</td></tr>').join('');
    el.innerHTML = '<div class="ac">' + lead('Checks what moved, so you are alerted for a person or car and not for shadows, rain or bugs.', 'Turn it on and keep the built-in model.') +
      how('AI confirms motion events by checking what moved. Events with none of the chosen labels are still recorded but send no alert. The model comes from the official YOLOX release and runs on this server; pictures never leave it.') +
      '<div class="ac-card"><div class="ac-row">' + swHtml('data-en', draft.enabled, 'Enable AI checks') + '<span class="ac-sp"></span>' + badge + '</div>' +
      '<div class="ac-sub">Detector</div><div class="seg ac-seg"><button data-src="builtin" class="' + (sBuiltin ? 'on' : '') + '">Built-in model</button><button data-src="server" class="' + (!sBuiltin ? 'on' : '') + '" title="Use another computer for AI (advanced)">Another computer (advanced)</button></div>' +
      (sBuiltin ?
        '<div class="ai-models" data-models>' + modelCards() + '</div>' +
        more('Advanced', '<label class="ac-f">Use a different model file (YOLOX .onnx) <input data-model value="' + esc(draft.model) + '" placeholder="e.g. D:/models/yolox_m.onnx"></label><div class="hint">A path on the server. Takes effect when you save.</div>', !!draft.model) :
        '<label class="ac-f">Address of the other computer <input data-url value="' + esc(draft.url) + '" placeholder="http://192.168.1.20:32168"></label><div class="hint">A computer running an AI server with the common <code>/v1/vision/detection</code> web API. Used when no built-in model is set.</div>') +
      (sBuiltin ? hardwareCard() : '') +
      '</div><div class="ac-card"><div class="ac-card-t">Sensitivity</div>' +
      '<label class="ac-f">How sure AI must be <b data-sv>' + Math.round(draft.min_score * 100) + '%</b><input type="range" data-score min="0.05" max="0.95" step="0.05" value="' + draft.min_score + '"></label>' +
      '<div class="hint">Higher = fewer false alerts but real ones may be missed; lower = more alerts, more false ones.</div>' +
      swHtml('data-parked', draft.ignore_parked, 'Ignore objects that have not moved', 'A car parked in view (or someone sitting still) for a few minutes no longer confirms every motion event; one that arrives or leaves still does. AI also remembers spots where the same kind of thing is seen event after event (a bush it takes for a person, a car parked between events): they stop alerting until they move or go. Cameras can override this (AI tab).') +
      swHtml('data-strict', draft.strict, 'Stricter confirmation', 'An object alerts only when AI sees it in 2 of 3 looks in a row and it moved between them. Stops one-off wrong guesses and things that never move (a statue, a bush taken for a person); someone passing very quickly may be missed. Cameras can override this (AI tab).') +
      swHtml('data-hideun', draft.hide_unconfirmed, 'Show only events AI confirms', 'Motion where AI found none of the alert labels (a flag or branches in the wind) stays out of Events and the timeline. Nothing is deleted; Events can still show them. Cameras can override this (AI tab).') +
      more('More settings', '<div class="ac-grid3">' + fld('Frames checked per event', 'data-mc min="1" max="100" value="' + draft.max_checks + '"', 'frames') + fld('CPU threads', 'data-th min="1" max="32" value="' + draft.threads + '"', 'threads') + '</div>' +
      '<div class="ac-sub">Alert zones (loitering, line crossing)</div>' +
      '<div class="ac-grid3">' + fld('Look every', 'data-zint min="250" max="5000" step="50" value="' + draft.zone_interval_ms + '"', 'milliseconds') + fld('Keep watching a person standing still for', 'data-zhold min="0" max="3600" step="30" value="' + draft.zone_hold_seconds + '"', 'seconds') + '</div>' +
      '<div class="hint">Cameras with alert zones look at the picture this often while there is motion, to follow people and cars. Faster follows quick movers better and uses more CPU. After the motion stops, it keeps watching while someone is still inside an area, for the time above.</div>') + '</div>' +
      '<div class="ac-card"><div class="ac-card-t">Alert labels</div><div class="hint">Objects that confirm an event.</div><div class="chips">' +
      labels.map(l => '<button class="chip' + (draft.labels.includes(l) ? ' on' : '') + '" data-lab="' + esc(l) + '">' + esc(l) + '</button>').join('') +
      '</div><div class="ac-row"><input data-newlab placeholder="Add a label" maxlength="30"><button class="btn" data-addlab>Add</button></div></div>' +
      '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">Per camera</div><span class="ac-sp"></span><button class="btn" data-cams>Camera settings</button></div>' +
      '<div class="hint">Each camera can turn AI on or off and use its own labels; set that in each camera&#39;s settings (Motion &amp; AI).</div>' +
      (camRows ? '<table class="ac-tbl"><thead><tr><th>Camera</th><th>AI</th><th>Labels</th></tr></thead><tbody>' + camRows + '</tbody></table>' : '<p class="hint">No cameras yet.</p>') + '</div>' +
      pageBar() + '</div>';
    bind();
  }
  // Where the built-in model runs: a graphics card (Windows) or the CPU.
  function hardwareCard() {
    const gpus = status.gpus || [];
    const on = status.running_on;
    const now = '<div class="hint">Running on: <b>' + esc(on || (status.builtin_loaded ? 'the CPU' : 'not loaded')) + '</b></div>';
    if (!gpus.length) return '<div class="ac-sub">Hardware</div>' + now + '<div class="hint">No graphics card the AI can use was found (it needs Windows and a DirectX 12 card), so it uses the CPU.</div>';
    const best = gpus.slice().sort((a, b) => b.memory_mb - a.memory_mb || a.index - b.index)[0];
    const gb = (g) => g.memory_mb >= 1024 ? ' (' + Math.round(g.memory_mb / 1024) + ' GB)' : '';
    return '<div class="ac-sub">Hardware</div>' + now +
      swHtml('data-gpu', draft.gpu, 'Use a graphics card', 'Much faster than the CPU; the CPU takes over if the card cannot be used.') +
      '<label class="ac-f">Graphics card <select data-gpuname' + (draft.gpu ? '' : ' disabled') + '>' +
        '<option value="">Automatic: ' + esc(best.name) + gb(best) + '</option>' +
        gpus.map(g => '<option value="' + esc(g.name) + '"' + (g.name === draft.gpu_name ? ' selected' : '') + '>' + esc(g.name) + gb(g) + '</option>').join('') +
      '</select></label>';
  }
  let upd;
  function bind() {
    const q = (s) => el.querySelector(s);
    upd = wireBar(el, () => !same(draft, saved), async () => {
      const body = { enabled: draft.enabled, min_score: draft.min_score, labels: draft.labels, max_checks: draft.max_checks, threads: draft.threads, ignore_parked: draft.ignore_parked, strict: draft.strict, hide_unconfirmed: draft.hide_unconfirmed, zone_interval_ms: draft.zone_interval_ms, zone_hold_seconds: draft.zone_hold_seconds,
        gpu: draft.gpu, gpu_name: draft.gpu_name || null,
        model: draft.source === 'builtin' ? (draft.model.trim() || null) : null, url: draft.url.trim() || null };
      const r = await postSettings({ ai: body });
      await load();
      if (r.warning) { toast('Saved, but: ' + r.warning); return false; }
    }, () => { draft = clone(saved); paint(); });
    // On/off settings save when flipped (only that value is sent).
    const flip = (sel, key) => { const i = q(sel); if (!i) return; liveSwitch(i, async (on) => {
      const r = await postSettings({ ai: { [key]: on } });
      saved[key] = on; draft[key] = on; upd();
      if (r.warning) { toast('Saved, but: ' + r.warning); return false; }
      if (key === 'gpu') q('[data-gpuname]').disabled = !on;
    }); };
    flip('[data-en]', 'enabled'); flip('[data-parked]', 'ignore_parked'); flip('[data-strict]', 'strict'); flip('[data-hideun]', 'hide_unconfirmed'); flip('[data-gpu]', 'gpu');
    el.querySelectorAll('[data-src]').forEach(b => { b.onclick = () => { draft.source = b.dataset.src; paint(); upd(); }; });
    const m = q('[data-model]'); if (m) m.oninput = () => { draft.model = m.value; upd(); };
    const u = q('[data-url]'); if (u) u.oninput = () => { draft.url = u.value; upd(); };
    q('[data-score]').oninput = (e) => { draft.min_score = +e.target.value; q('[data-sv]').textContent = Math.round(draft.min_score * 100) + '%'; upd(); };
    q('[data-mc]').oninput = (e) => { draft.max_checks = Math.max(1, parseInt(e.target.value, 10) || 1); upd(); };
    q('[data-zint]').oninput = (e) => { draft.zone_interval_ms = Math.min(5000, Math.max(250, parseInt(e.target.value, 10) || 700)); upd(); };
    q('[data-zhold]').oninput = (e) => { draft.zone_hold_seconds = Math.min(3600, Math.max(0, parseInt(e.target.value, 10) || 0)); upd(); };
    q('[data-th]').oninput = (e) => { draft.threads = Math.max(1, parseInt(e.target.value, 10) || 1); upd(); };
    const gn = q('[data-gpuname]'); if (gn) gn.onchange = (e) => { draft.gpu_name = e.target.value; upd(); };
    el.querySelectorAll('[data-lab]').forEach(b => {
      b.onclick = () => {
        const l = b.dataset.lab;
        draft.labels = draft.labels.includes(l) ? draft.labels.filter(x => x !== l) : draft.labels.concat(l);
        b.classList.toggle('on'); upd();
      };
    });
    const addLab = () => {
      const v = q('[data-newlab]').value.trim().toLowerCase();
      if (!v) return;
      if (!draft.labels.includes(v)) draft.labels.push(v);
      paint(); upd();
    };
    q('[data-addlab]').onclick = addLab;
    q('[data-newlab]').onkeydown = (e) => { if (e.key === 'Enter') addLab(); };
    q('[data-cams]').onclick = () => openSettingsDialog('cameras');
    bindModels();
  }
  load();
}

/* ====================================================================== Storage */

// Two pages share one loader: Recording > Storage & retention (how long, how much space) and
// Recording > Drives & folders (where). Both save their own fields of [storage] with the bar.
const mbToGb = (mb) => +(mb / 1024).toFixed(2);
const gbToMb = (gb) => Math.max(1, Math.round(gb * 1024));

// Who a folder is for: "3 cameras", or for the archive the cameras whose oldest recordings move in.
const camsWord = (n) => n + ' camera' + (n === 1 ? '' : 's');
function whoUses(f, n) {
  if (!f.archive) return camsWord(n);
  const others = state.cams.filter(c => !c.remote).length - n;
  return 'archive for ' + camsWord(Math.max(0, others)) + (n ? '; ' + camsWord(n) + ' record here' : '');
}

function storagePage(el, mode) {
  if (!guardAdmin(el)) return;
  const retention = mode === 'retention';
  let saved, draft, st, dataDir = '', drives = [], plan = null, priv = {}, probes = {}, ends = {}, privTimer = null;
  el.innerHTML = '<div class="ac"><p class="empty">Loading...</p></div>';
  stillWaiting(el.firstElementChild, () => load());
  const norm = (s) => ({
    segment_seconds: s.segment_seconds, retention_days: s.retention_days, max_mb: s.max_mb,
    event_keep_days: s.event_keep_days == null ? 21 : s.event_keep_days,
    sub_copy: !!s.sub_copy,
    folders: (s.folders || []).map(f => ({ name: f.name, path: String(f.path), max_mb: f.max_mb || 0, archive: !!f.archive, private: f.private === true ? true : f.private === false ? false : null })),
    min_free_percent: s.min_free_percent != null ? s.min_free_percent : 10,
  });
  // "Keep this folder private": the real state of each folder (private, open, being applied, failed, refused).
  let privLoaded = false;
  async function loadPrivacy() {
    try {
      const v = await api('/api/storage/privacy');
      priv = {}; (v.folders || []).forEach(f => { priv[f.name] = f; });
      ends = { program: v.program || null, data: v.data || null };
    } catch (e) { priv = {}; ends = {}; }
    privLoaded = true;
  }
  // The program and data folders: read-only, with the real state and why one was left alone.
  function endLine(e) {
    if (!e || e.state === 'na') return '';
    const t = {
      private: 'Private: only administrators and AmberSight can open it.',
      open: 'Open: other accounts on this computer can open it.',
      applying: 'Being applied in the background; recording carries on.',
      skipped: 'Left as it is: ' + e.detail + '.',
      refused: 'Left as it is: ' + (e.detail || 'not a folder AmberSight may change') + '.',
      failed: 'Could not be made private: ' + e.detail,
      unsupported: 'Cannot be made private on this kind of drive.',
      missing: 'The folder is not there yet.',
      unknown: 'No answer from this folder yet: the drive or network share may be off or slow.',
    }[e.state] || '';
    return t ? '<div class="hint">' + esc(t) + '</div>' : '';
  }
  const PRIV_TEXT = {
    private: 'Private now: only administrators and AmberSight can open it.',
    open: 'Open: other accounts on this computer can open it.',
    applying: 'Being applied in the background. A folder with many recordings can take several minutes; recording carries on.',
    missing: 'The folder is not there yet.',
    unsupported: 'Cannot be made private on this kind of drive.',
    unknown: 'No answer from this folder yet: the drive or network share may be off or slow. You can still change or remove it here.',
  };
  // The switch shows the folder's real state when no value is saved (a folder locked by hand shows on).
  // Nothing is saved for it until the person flips the switch.
  function shownPrivate(f, i) {
    if (f.private === true || f.private === false) return f.private;
    const sv = saved.folders[i], s = priv[f.name];
    return !!(sv && sv.path === f.path && s && s.state === 'private');
  }
  function privLine(f, i) {
    const sv = saved.folders[i];
    if (!sv || sv.path !== f.path) {
      // Not saved at this place yet: what the drive says about it, and the old place is left alone.
      const pr = probes[f.name];
      if (pr && pr.state === 'refused') return 'Refused: ' + (pr.detail || 'choose a folder on that drive, not the whole drive');
      if (pr && pr.state === 'unsupported') return 'Cannot be made private on this kind of drive.';
      return 'Applied when you save.' + (sv ? ' The old location is left as it is.' : '');
    }
    const s = priv[f.name];
    if (!s) return privLoaded ? 'Applied when you save.' : 'Checking...';
    if (s.state === 'failed') return 'Could not apply: ' + s.detail;
    if (s.state === 'refused') return 'Refused: ' + (s.detail || 'choose a folder on that drive, not the whole drive');
    if (s.state === 'unsupported' && s.detail) return s.detail.charAt(0).toUpperCase() + s.detail.slice(1) + '.';
    return PRIV_TEXT[s.state] || '';
  }
  // The default for a folder not saved yet, from what is there now (the same rule the server uses).
  async function probeFolder(f) {
    if (f._touched || !f.path.trim()) return;
    try {
      const pr = (await api('/api/storage/privacy?path=' + encodeURIComponent(f.path))).probe;
      if (!pr) return;
      probes[f.name] = pr;
      f.private = !!pr.default_private && !['refused', 'unsupported'].includes(pr.state);
      paint(); upd();
    } catch (e) { /* decided when saved */ }
  }
  // Turning on for a folder that holds files: asked first.
  async function confirmOn(f) {
    let probe = null;
    try { probe = (await api('/api/storage/privacy?path=' + encodeURIComponent(f.path))).probe; } catch (e) { /* no answer: ask anyway */ }
    if (probe && (!probe.exists || probe.empty)) return true;
    return confirmDialog({ title: 'Keep ' + f.name + ' private?', body: 'This folder already holds files. Only administrators and AmberSight will be able to open it afterwards, so File Explorer, a network share or other programs that use it may stop working. On a big folder this can take several minutes in the background.', ok: 'Make private', danger: true });
  }
  // Draw again when the answer to something slow arrives, unless someone is typing in the page.
  function repaintQuietly() {
    if (el.isConnected && !el.contains(document.activeElement && document.activeElement.matches('input,select,textarea') ? document.activeElement : null)) paint();
  }
  // Re-read while something is being applied, and update the lines in place.
  function watchPrivacy() {
    clearTimeout(privTimer);
    const waiting = (s) => s && (s.state === 'applying' || s.state === 'unknown');
    if (retention || !el.isConnected || !(Object.values(priv).some(waiting) || waiting(ends.program) || waiting(ends.data))) return;
    privTimer = setTimeout(async () => {
      if (!el.isConnected) return;
      const before = JSON.stringify(ends);
      await loadPrivacy();
      el.querySelectorAll('[data-privline]').forEach(n => { const f = draft.folders[+n.dataset.privline]; if (f) n.textContent = privLine(f, +n.dataset.privline); });
      if (JSON.stringify(ends) !== before) repaintQuietly();
      watchPrivacy();
    }, 3000);
  }
  async function load() {
    try {
      const [s, t] = await Promise.all([api('/api/settings'), api('/api/status'), refreshCameras().catch(() => {})]);
      saved = norm(s.storage); draft = clone(saved); st = t.storage || {}; dataDir = t.data_dir || String(s.storage.data_dir);
      try { drives = (await api('/api/drives')).drives || []; } catch (e) { drives = []; }
      // The page is drawn first. What the folders say comes when it comes: a drive that does not
      // answer must not keep the page on Loading (or the folder from being removed).
      paint();
      if (!retention) loadPrivacy().then(() => { if (el.isConnected) { repaintQuietly(); watchPrivacy(); } });
      // The plan ("room for about N days") is slow on a big site: the page is drawn first and gets it when it arrives.
      api('/api/storage/plan').then(v => { plan = v; repaintQuietly(); }).catch(() => {});
    } catch (e) { el.innerHTML = '<p class="empty">' + esc(e.message) + '</p>'; }
  }
  // capMb 0 = automatic: the server's current cap and status (`info`, from
  // /api/status) when the saved setting is automatic too. Full while the
  // oldest make room is a plain note; amber/red only when the server says so.
  // rolls: a folder that hands its oldest to the archive runs full by design.
  const bar = (used, capMb, rolls, info) => {
    const auto = !capMb;
    if (auto && !(info && info.auto && info.drive_total)) return '<div class="ac-meter"><i style="width:0"></i></div><span class="hint">automatic: worked out from the drive once saved</span>';
    const capB = auto ? info.cap_bytes || 0 : capMb * 1048576;
    const pc = capB ? Math.min(100, used / capB * 100) : 100;
    const same = info && !!info.auto === auto && (auto || info.cap_bytes === capB);
    const lvl = same ? ({ crit: ' crit', warn: ' warn' }[info.level] || '') : '';
    const note = same && info.label ? '; ' + info.label.toLowerCase() + (rolls ? ', older ones move to the archive' : '') : (rolls && pc >= 95 ? '; full, the oldest move to the archive' : '');
    return '<div class="ac-meter' + lvl + '"><i style="width:' + pc.toFixed(1) + '%"></i></div><span class="hint">' + pc.toFixed(0) + '% of ' + (auto ? 'about ' : '') + fmtSize(capB) +
      (auto ? ' (what the drive can spare now)' : '') + esc(note) + '</span>';
  };
  const pctFree = () => (draft.min_free_percent != null ? draft.min_free_percent : 10);
  // Space for one folder: Automatic, or a fixed size in GB. Stored as MB.
  const capField = (box, mb) =>
    '<label class="fld"><span class="fld-l">Space for recordings</span><select data-capmode><option value="auto"' + (mb ? '' : ' selected') + '>Automatic (what the drive can spare)</option><option value="fixed"' + (mb ? ' selected' : '') + '>Fixed size</option></select></label>' +
    (mb ? fld('Size', 'data-gb="' + box + '" min="0.1" step="0.5" value="' + mbToGb(mb) + '"', 'GB') : '');
  // "Holds about N days at X/day" for a saved folder ("" = data folder).
  const fits = (name) => {
    const x = plan && name != null && (plan.folders || []).find(p => p.name === name);
    if (!x || !x.bytes_per_day) return '';
    const d = x.days_fit;
    return '<div class="hint">Recording about ' + fmtSize(x.bytes_per_day) + '/day here' + (x.inflow_per_day ? ' (including what other folders move in)' : '') +
      (d != null ? ': room for about <b>' + (d >= 10 ? Math.round(d) : d.toFixed(1)) + ' days</b>' : '') + '.</div>';
  };
  const tiles = () => '<div class="ac-tiles">' +
    '<div class="ac-tile"><b>' + fmtSize(st.bytes || 0) + '</b><span>used in data folder</span></div>' +
    '<div class="ac-tile"><b>' + (st.segments || 0) + '</b><span>recording files</span></div>' +
    '<div class="ac-tile"><b>' + (st.clips || 0) + '</b><span>saved clips</span></div>' +
    '<div class="ac-tile"><b>' + fmtSize(st.indexed_bytes || 0) + '</b><span>indexed recordings</span></div></div>';

  function paintRetention() {
    const usage = {}, capInfo = {}; (st.folders || []).forEach(f => { usage[f.name] = f.bytes; capInfo[f.name] = f.cap; });
    const hasArchive = saved.folders.some(f => f.archive);
    const auto = !draft.max_mb;
    const dc = st.data_cap || {};
    let h = lead('How long recordings are kept, and how much space they may use.', 'The default uses what the drive can spare. Change it only if you want a fixed number of days.') +
      how('Over the limit, the oldest recordings move to the archive folder if one is set, otherwise they are deleted. Events you keep forever are never removed.') + tiles() +
      '<div class="ac-card"><div class="ac-card-t">How long to keep</div>' +
      fld('Keep recordings for', 'data-k="retention_days" min="1" value="' + draft.retention_days + '"', 'days') +
      '</div><div class="ac-card"><div class="ac-card-t">How much space</div>' +
      '<div class="hint">Data folder: <span class="ac-path-i">' + esc(dataDir) + '</span></div>' +
      '<div class="ac-meterrow">' + bar(st.data_cap ? st.data_cap.used_bytes : st.bytes || 0, draft.max_mb, hasArchive, st.data_cap) + '</div>' + fits('') +
      '<div data-capbox="data">' + capField('data', draft.max_mb) + '</div>' +
      (auto ? fld('Always keep this much of the disk free', 'data-k="min_free_percent" min="0" max="90" value="' + pctFree() + '"', '% of the disk',
        dc.auto && dc.drive_total ? 'Now about ' + fmtSize(dc.cap_bytes || 0) + ' for recordings: ' + fmtSize(dc.used_bytes || 0) + ' used, ' + fmtSize(dc.drive_free || 0) + ' free of ' + fmtSize(dc.drive_total) + ' on the drive. Other files on the drive make it smaller.' : 'The size is worked out once saved.') : '') +
      '</div>';
    if (draft.folders.length) {
      h += '<div class="ac-card"><div class="ac-card-t">Space in extra folders</div>';
      draft.folders.forEach((f, i) => {
        const n = state.cams.filter(c => c.folder === (saved.folders[i] ? saved.folders[i].name : f.name)).length;
        h += '<div class="ac-folder" data-i="' + i + '"><div class="ac-card-t">' + esc(f.name) + (f.archive ? ' <span class="badge info">archive</span>' : '') + '</div>' +
          '<div class="hint">' + fmtSize(usage[f.name] || 0) + ' used - ' + whoUses(f, n) + '</div>' +
          '<div class="ac-meterrow">' + bar(usage[f.name] || 0, f.max_mb, hasArchive && !f.archive, saved.folders[i] && saved.folders[i].name === f.name ? capInfo[f.name] : null) + '</div>' + fits(saved.folders[i] ? saved.folders[i].name : null) +
          '<div data-capbox="' + i + '">' + capField(String(i), f.max_mb) + '</div></div>';
      });
      h += '<div class="hint">Add, rename or remove folders under Drives &amp; folders.</div></div>';
    }
    h += '<details class="ac-more"' + (more ? ' open' : '') + '><summary>More settings</summary>' +
      fld('Keep event footage first for', 'data-k="event_keep_days" min="0" max="3650" value="' + draft.event_keep_days + '"', 'days', '0 = off. When a folder is full, footage with no events in it is deleted before footage around events from this many days, so alerts stay viewable longer than the rest. It never keeps anything past "Keep recordings for".') +
      fld('Segment length', 'data-k="segment_seconds" min="1" value="' + draft.segment_seconds + '"', 'seconds', 'Recordings are written in pieces this long.') +
      swHtml('data-subcopy', draft.sub_copy, 'Also record the sub stream of continuous cameras', 'A small second copy (about a tenth more space) so phones with Data saver on play recordings with little data. Their streams reconnect when this changes.') +
      '</details>' + pageBar();
    el.innerHTML = '<div class="ac">' + h + '</div>';
    bindCommon();
    bindRetention();
  }

  function paintDrives() {
    const usage = {}, capInfo = {}; (st.folders || []).forEach(f => { usage[f.name] = f.bytes; capInfo[f.name] = f.cap; });
    const camsIn = (n) => state.cams.filter(c => c.folder === n).length;
    let h = lead('Where recordings are stored.', 'Most homes only need the default folder; add a drive when it fills up.') +
      how('Cameras without a folder of their own record into the data folder; a camera can pick another folder in its own settings. The archive folder receives the oldest recordings pushed out of the others. How much space each may use is set under Storage &amp; retention.') +
      '<div class="ac-card"><div class="ac-card-t">Data folder</div><div class="ac-path">' + esc(dataDir) + '</div><div class="hint">' + fmtSize(st.bytes || 0) + ' used. Changing this folder needs a config edit and a restart.</div>' + endLine(ends.data) + '</div>' +
      (ends.program && ends.program.state !== 'na' ? '<div class="ac-card"><div class="ac-card-t">Program folder</div><div class="ac-path">' + esc(ends.program.path) + '</div>' + endLine(ends.program) + '</div>' : '') +
      '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">Extra folders</div><span class="ac-sp"></span><button class="btn" type="button" data-addf>' + ic('plus') + ' Add folder</button></div>' +
      '<div class="hint">Other drives or network shares.</div>';
    if (!draft.folders.length) h += '<p class="hint">None. Everything records into the data folder.</p>';
    draft.folders.forEach((f, i) => {
      const n = camsIn(saved.folders[i] ? saved.folders[i].name : f.name);
      h += '<div class="ac-folder" data-i="' + i + '"><div class="ac-grid4">' +
        '<label class="fld"><span class="fld-l">Name</span><input data-f="name" value="' + esc(f.name) + '"></label>' +
        '<label class="fld ac-wide"><span class="fld-l">Path</span><input data-f="path" value="' + esc(f.path) + '" placeholder="/mnt/nas/cameras or D:\\Cameras"></label>' +
        '</div>' +
        (drives.length ? '<div class="ac-row"><label class="ac-inl">Pick a drive <select data-drive><option value="">choose...</option>' +
          drives.map(dv => '<option value="' + esc(dv.path) + '">' + esc(dv.path) + ' - ' + fmtSize(dv.free) + ' free of ' + fmtSize(dv.total) + '</option>').join('') +
          '</select></label><span class="hint ac-inl">fills in the path with an AmberSight folder on that drive</span></div>' : '') +
        swHtml('data-f="archive"', f.archive, 'Archive folder', 'Receives recordings pushed out of the other folders. Only one folder can be the archive.') +
        swHtml('data-f="private"', shownPrivate(f, i), 'Keep this folder private', 'Only administrators and AmberSight can open it, so browsing it in File Explorer or over a network share may stop working.',
          priv[f.name] && priv[f.name].state === 'refused' && f.private !== true) +
        '<div class="hint" data-privline="' + i + '">' + esc(privLine(f, i)) + '</div>' +
        '<div class="ac-row"><span class="hint ac-inl">' + fmtSize(usage[f.name] || 0) + ' used - ' + whoUses(f, n) + '</span><span class="ac-sp"></span><button class="btn danger" type="button" data-rm>Remove</button></div></div>';
    });
    h += '</div>' + pageBar();
    el.innerHTML = '<div class="ac">' + h + '</div>';
    bindCommon();
    bindDrives();
  }

  let more = false;
  function paint() { if (retention) paintRetention(); else paintDrives(); }

  let upd;
  // A moved folder is sent with no choice unless the person made one (the server then applies its default).
  function foldersToSend() {
    return draft.folders.map((f, i) => {
      const sv = saved.folders[i];
      return sv && sv.path !== f.path && !f._touched ? Object.assign({}, f, { private: null }) : f;
    });
  }
  function bindCommon() {
    upd = wireBar(el, () => !same(draft, saved), async () => {
      const body = retention
        ? { segment_seconds: draft.segment_seconds, retention_days: draft.retention_days, max_mb: draft.max_mb, event_keep_days: draft.event_keep_days, folders: foldersToSend() }
        : { folders: foldersToSend() };
      if (retention && !draft.max_mb) body.min_free_percent = Math.min(90, Math.max(0, draft.min_free_percent));
      // The switch saved by itself already; the rest is sent as it stands.
      await postSettings({ storage: body });
      await load();
    }, () => { draft = clone(saved); paint(); });
  }

  function bindRetention() {
    const d = el.querySelector('.ac-more');
    if (d) d.addEventListener('toggle', () => { more = d.open; });
    // Days and segment length never go below 1.
    el.querySelectorAll('[data-k]').forEach(i => { i.oninput = () => {
      const n = parseInt(i.value, 10);
      const k = i.dataset.k;
      draft[k] = k === 'event_keep_days' ? Math.min(3650, Math.max(0, isNaN(n) ? 21 : n))
        : k === 'min_free_percent' ? Math.min(90, Math.max(0, isNaN(n) ? 10 : n)) : Math.max(1, n || saved[k] || 1);
      upd();
    }; });
    // Automatic <-> fixed: a fixed size starts from what automatic allows now.
    el.querySelectorAll('[data-capbox]').forEach(box => {
      const sel = box.querySelector('[data-capmode]');
      sel.onchange = () => {
        const i = box.dataset.capbox, obj = i === 'data' ? draft : draft.folders[+i];
        const sf = obj && (st.folders || []).find(x => x.name === obj.name);
        const info = i === 'data' ? st.data_cap : sf && sf.cap;
        const now = info && info.cap_bytes ? Math.max(1, Math.round(info.cap_bytes / 1048576)) : 2048;
        obj.max_mb = sel.value === 'auto' ? 0 : now;
        paint(); upd();
      };
      const gb = box.querySelector('[data-gb]');
      if (gb) gb.oninput = () => {
        const obj = box.dataset.capbox === 'data' ? draft : draft.folders[+box.dataset.capbox];
        const v = parseFloat(gb.value);
        obj.max_mb = isFinite(v) && v > 0 ? gbToMb(v) : obj.max_mb;
        upd();
      };
    });
    // On/off: saved at once, on its own.
    const sc = el.querySelector('[data-subcopy]');
    liveSwitch(sc, async (on) => { await postSettings({ storage: { sub_copy: on } }); saved.sub_copy = on; draft.sub_copy = on; upd(); });
  }

  function bindDrives() {
    el.querySelector('[data-addf]').onclick = () => {
      let n = 1, name; do { name = 'folder' + n++; } while (draft.folders.some(f => f.name === name));
      draft.folders.push({ name, path: '', max_mb: 0, archive: false }); paint(); upd();
      const last = el.querySelector('.ac-folder:last-of-type [data-f="name"]'); if (last) { last.focus(); last.select(); }
    };
    el.querySelectorAll('.ac-folder').forEach(row => {
      const i = +row.dataset.i, f = draft.folders[i];
      row.querySelectorAll('[data-f]').forEach(inp => {
        const k = inp.dataset.f;
        if (k === 'archive') inp.onchange = () => { draft.folders.forEach(x => { x.archive = false; }); f.archive = inp.checked; paint(); upd(); };
        else if (k === 'private') {
          const isSaved = () => saved.folders[i] && saved.folders[i].name === f.name && saved.folders[i].path === f.path;
          if (isSaved()) {
            // A folder that is already saved: flipped and applied at once, like the other on/off settings.
            liveSwitch(inp, async (on) => {
              if (on && !(await confirmOn(f))) throw { cancelled: true };
              // Off gives the folder back what its parent allows, which can be more than it had.
              if (!on && !(await confirmDialog({ title: 'Open ' + f.name + ' again?', body: 'The folder goes back to the permissions of the folder it is in, so other accounts on this computer can open it again (on Windows, whatever its parent allows; on Linux and macOS, mode 755).', ok: 'Open it again', danger: true }))) throw { cancelled: true };
              const folders = saved.folders.map(x => x.name === f.name ? Object.assign({}, x, { private: on }) : x);
              await postSettings({ storage: { folders } });
              saved.folders[i].private = on; f.private = on;
              await loadPrivacy(); paint(); watchPrivacy(); upd();
            });
          } else inp.onchange = async () => {
            if (inp.checked && !(await confirmOn(f))) { inp.checked = false; return; }
            f.private = inp.checked; f._touched = true; upd();
          };
        }
        else inp.oninput = () => { f[k] = inp.value; upd(); };
      });
      const pathInp = row.querySelector('[data-f="path"]');
      if (pathInp) pathInp.onchange = () => {
        const sv = saved.folders[i];
        // Typed back to the saved path: the saved choice, not a fresh default.
        if (sv && sv.path === f.path) { f.private = sv.private; f._touched = false; delete probes[f.name]; setTimeout(() => { paint(); upd(); }, 0); return; }
        f._touched = false; probeFolder(f);
      };
      const dsel = row.querySelector('[data-drive]');
      if (dsel) dsel.onchange = () => {
        if (!dsel.value) return;
        f.path = dsel.value.replace(/[\\/]+$/, '') + '/AmberSight';
        row.querySelector('[data-f="path"]').value = f.path; f._touched = false; upd(); probeFolder(f);
      };
      row.querySelector('[data-rm]').onclick = async () => {
        const n = state.cams.filter(c => c.folder === f.name).length;
        if (!(await confirmDialog({ title: 'Remove folder ' + f.name + '?', body: (n ? n + ' camera' + (n === 1 ? ' uses' : 's use') + ' this folder and will record into the data folder instead. ' : '') + 'Recordings already in it stay on the drive but are no longer managed by AmberSight. Takes effect when you save.', ok: 'Remove folder', danger: true }))) return;
        draft.folders.splice(i, 1); paint(); upd();
      };
    });
  }
  load();
}

/* ====================================================================== Discovery */

function hostOf(u) { try { return new URL(u).hostname; } catch (e) { return ''; } }

function discoveryPage(el) {
  if (!guardAdmin(el)) return;
  let res = null, scanning = false, err = '';
  let added = 0;   // cameras added on this visit
  let logins = [{ username: '', password: '' }, { username: '', password: '' }];
  let remember = true;
  // The other subnets, the hourly check, ignored addresses and what the check found.
  let W = { ranges: [], watch: false, ignored: [], found: [], last_check: null };
  let showIgn = false;
  let moreOpen = false;
  let rangesTyped = null;
  const loadWatch = () => api('/api/discover/watch').then(r => { W = r; paint(); }).catch(() => {});
  loadWatch();
  // xaddr -> { state: 'trying'|'ok'|'auth'|'error', msg, ins, username, password, u, p, cams:[{name, base, pick, added}] }
  const devs = {};
  el.innerHTML = '<div class="ac"></div>';
  const root = el.firstChild;
  const configured = (d) => {
    const hosts = new Set(state.cams.map(c => hostOf(c.url)).concat(state.cams.map(c => hostOf(c.sub_url || ''))));
    return hosts.has(d.ip) || (d.xaddrs || []).some(x => hosts.has(hostOf(x)));
  };
  const tryMsg = (b) => { try { return JSON.parse(b).error || b; } catch (e) { return b; } };
  api('/api/discover/logins').then(r => {
    (r.logins || []).slice(0, 2).forEach((l, i) => { logins[i] = { username: l.username, password: '', saved: l.has_password }; });
    paint();
  }).catch(() => {});

  // The one login most people need (shown), and the optional second one (under More options).
  function loginRow(i) {
    const l = logins[i];
    return '<div class="dl-login"><label>User name <input data-lu="' + i + '" value="' + esc(l.username) + '" autocomplete="off" autocapitalize="none" spellcheck="false" placeholder="' + (i ? 'optional second login' : 'e.g. admin') + '"></label>' +
      '<label>Password <span class="dl-pw"><input type="password" data-lp="' + i + '" value="' + esc(l.password) + '" autocomplete="new-password" placeholder="' + (l.saved ? 'saved (leave blank to keep)' : '') + '">' +
      '<button type="button" class="btn dl-eye" data-eye="' + i + '" aria-pressed="false" title="Show or hide the password">Show</button></span></label></div>';
  }
  const sw = (attr, on, label, tip) => '<label class="sw"' + (tip ? ' title="' + esc(tip) + '"' : '') + '><input type="checkbox" role="switch" ' + attr + (on ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span><span class="sw-text"><b>' + label + '</b></span></label>';
  function paint() {
    let h = lead('Looks for cameras on your network and adds the ones you pick.') +
      (added ? notice('ok', 'Added ' + added + ' camera' + (added === 1 ? '' : 's') + '. You can rename them later in Cameras.', '<button class="btn primary" type="button" data-added-next>Next setup step</button><a class="btn" href="#live">See them on Live</a>') : '') +
      '<div class="ac-card dl-first"><p class="dl-lead">Use the user name and password you set in the camera\'s own app.</p>' +
      loginRow(0) +
      (err ? '<div class="ac-err">' + esc(err) + '</div>' : '') +
      '<div class="ac-row ac-scan"><button class="primary dl-scan" data-scan' + (scanning ? ' disabled' : '') + '>' + (scanning ? 'Scanning...' : 'Scan for cameras') + '</button></div>' +
      '<details class="dl-more" data-more' + (moreOpen ? ' open' : '') + '><summary>More options</summary>' +
      '<div class="dl-moreb"><div class="hint">A second login to try, if some cameras use another one. Each camera is tried with these and stops at the first that works. Many cameras lock a login after a few wrong passwords, so only logins you expect to work belong here.</div>' +
      loginRow(1) +
      '<label class="ac-inl ac-grow">Other subnets <input data-subnet value="' + esc(rangesTyped !== null ? rangesTyped : (W.ranges || []).join(', ')) + '" placeholder="optional, e.g. 192.168.1.0/24, 10.0.20.0/24"></label>' +
      '<div class="hint">Your own network is always scanned. Cameras on other networks answer only when listed here, separated by commas (up to sixteen). They are remembered.</div>' +
      '<label class="ac-inl">Wait <select data-secs><option>3</option><option>5</option><option>8</option><option>15</option></select> seconds for answers</label>' +
      sw('data-watch', W.watch, 'Check every hour for new cameras', 'Tells you (header and notifications) when a new camera shows up, here and on the listed subnets') +
      (W.watch && W.last_check ? '<div class="hint">Last checked ' + esc(new Date(W.last_check * 1000).toLocaleString()) + '.</div>' : '') +
      sw('data-remember', remember, 'Remember these logins on this server', 'Kept like camera logins, so the next scan needs no typing') +
      '</div></details></div>';
    const fresh = W.found || [], ign = W.ignored || [];
    if (fresh.length || ign.length) {
      h += '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">' + (fresh.length ? 'New on your network (' + fresh.length + ')' : 'New on your network') + '</div><span class="ac-sp"></span>' +
        (ign.length ? '<button class="btn" data-showign>' + (showIgn ? 'Hide ignored' : ign.length + ' ignored') + '</button>' : '') + '</div>';
      if (fresh.length) {
        h += '<div class="hint">Found by the hourly check and not added yet. Scan network signs in to the ONVIF ones so you can add them; RTSP-only ones are added by hand with their stream address.</div>' +
          fresh.map(f => '<div class="ac-row nc-row"><div class="ac-devname"><b>' + esc(f.name || (f.kind === 'rtsp' ? 'Answers on RTSP' : 'Camera')) + '</b><span class="hint">' + esc(f.ip) + ' · ' + (f.kind === 'rtsp' ? 'RTSP only' : 'ONVIF') +
            ' · first seen ' + esc(new Date(f.first_seen * 1000).toLocaleString()) + '</span></div><span class="ac-sp"></span><button class="btn" data-ign="' + esc(f.ip) + '" title="Not a camera to add: stop reporting it">Ignore</button></div>').join('');
      } else h += '<div class="hint">Nothing new.</div>';
      if (showIgn && ign.length) h += '<div class="ac-sub">Ignored</div>' + ign.map(ip => '<div class="ac-row nc-row"><span>' + esc(ip) + '</span><span class="ac-sp"></span><button class="btn" data-unign="' + esc(ip) + '">Stop ignoring</button></div>').join('');
      h += '</div>';
    }
    if (res) {
      const list = res.devices || [];
      const picked = Object.values(devs).flatMap(d => (d.cams || []).filter(c => c.pick && !c.added));
      h += '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">' + list.length + ' device' + (list.length === 1 ? '' : 's') + ' found</div><span class="ac-sp"></span>' +
        '<button class="primary" data-addsel' + (picked.length ? '' : ' disabled') + '>Add selected (' + picked.length + ')</button></div>';
      if (!list.length) h += '<p class="hint">Nothing answered. Cameras on another subnet need that range listed above.</p>';
      list.forEach((d, i) => {
        const xa = (d.xaddrs && d.xaddrs[0]) || '';
        const v = devs[xa] || {};
        const isCfg = configured(d);
        const inf = (v.ins && v.ins.info) || {};
        const title = esc(inf.model ? [inf.manufacturer, inf.model].filter(Boolean).join(' ') : (d.name || d.hardware || d.ip));
        let status;
        if (isCfg) status = '<span class="badge on dot">already added</span>';
        else if (v.state === 'trying') status = '<span class="badge info dot">signing in...</span>';
        else if (v.state === 'ok') status = '<span class="badge on dot">signed in as ' + esc(v.username) + '</span>';
        else if (v.state === 'auth') status = '<span class="badge off dot">login did not work</span>';
        else if (v.state === 'error') status = '<span class="badge off dot">could not read</span>';
        else status = '<span class="badge">new</span>';
        const hit = isCfg ? state.cams.find(c => hostOf(c.url) === d.ip || hostOf(c.sub_url || '') === d.ip) : null;
        const snap = hit ? '/api/snapshot/' + hit.id : (v.pic || '');
        const nOk = (v.cams || []).filter(c => c.pick && !c.added).length;
        const addBtn = v.state === 'ok' && !isCfg && (v.cams || []).some(c => !c.added) ? '<button class="primary" data-addone' + (nOk ? '' : ' disabled') + '>Add</button>' : '';
        h += '<div class="ac-dev dl-card' + (isCfg ? ' dl-have' : '') + '" data-xa="' + esc(xa) + '"><div class="dl-head"><div class="dl-snap">' +
          (snap ? '<img src="' + esc(snap) + '" alt="" loading="lazy">' : '') + '<svg class="ic" aria-hidden="true"><use href="#i-camera"/></svg></div>' +
          '<div class="ac-devname"><b>' + title + '</b><span class="hint">' + esc(d.ip) +
          (inf.model ? '' : (d.hardware && d.hardware !== d.name ? ' - ' + esc(d.hardware) : '')) + '</span>' + status + '</div><span class="ac-sp"></span>' + addBtn + '</div>';
        if (!isCfg && (v.state === 'auth' || v.state === 'error' || v.state === 'manual')) {
          h += '<div class="ac-ins">' + (v.msg ? '<div class="ac-err">' + esc(v.msg) + '</div>' : '') +
            '<div class="ac-grid4"><label>Username <input data-u value="' + esc(v.u || '') + '" autocomplete="off"></label>' +
            '<label>Password <input type="password" data-p value="' + esc(v.p || '') + '" autocomplete="new-password"></label>' +
            '<div class="ac-go"><button class="btn" data-try' + (xa ? '' : ' disabled') + '>Try</button></div></div></div>';
        }
        if (!isCfg && !v.state && xa) h += '<div class="ac-ins"><button class="btn" data-manual>Enter a login for this camera</button></div>';
        if (v.state === 'ok') {
          h += (v.cams || []).map((c, ci) => '<div class="ac-cand dl-cand" data-ci="' + ci + '">' +
            (c.added ? '<span class="badge on dot">added</span>' : '<label class="asys-chk"><input type="checkbox" data-pick' + (c.pick ? ' checked' : '') + '></label>') +
            '<label class="dl-name">Name <input data-cn value="' + esc(c.name) + '"' + (c.added ? ' disabled' : '') + '></label>' +
            '<div class="hint ac-url">' + esc((c.base.url || '').replace(/\/\/[^@/]*@/, '//')) + (c.base.sub_url ? ' (+ sub stream)' : '') + '</div></div>').join('');
        }
        h += '</div>';
      });
      if ((res.rtsp_only || []).length) {
        h += '<div class="ac-sub">Answered on RTSP only (not ONVIF)</div><div class="chips">' + res.rtsp_only.map(r => '<span class="badge">' + esc(r.ip) + ':' + r.port + '</span>').join('') + '</div>' +
          '<div class="hint">Add these by hand on the Cameras tab, with their stream address.</div>';
      }
      h += '</div>';
    }
    root.innerHTML = h;
    root.querySelectorAll('.dl-snap img').forEach((im) => { im.onerror = () => im.remove(); });   // no inline handlers: the CSP blocks them
    bind();
  }

  function readLogins() {
    root.querySelectorAll('[data-lu]').forEach(i => { logins[+i.dataset.lu].username = i.value.trim(); });
    root.querySelectorAll('[data-lp]').forEach(i => { logins[+i.dataset.lp].password = i.value; });
    const r = root.querySelector('[data-remember]'); if (r) remember = r.checked;
  }
  const usable = () => logins.filter(l => l.username && (l.password || l.saved));

  // A picture of a camera that is not added yet, so people can tell which is which.
  async function fetchPicture(v) {
    const c = v.cams[0].base;
    try {
      const r = await fetch('/api/discover/preview', { method: 'POST', credentials: 'same-origin', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ url: c.url, sub_url: c.sub_url || null, username: v.username || '', password: v.password || '' }) });
      if (!r.ok) return;
      v.pic = URL.createObjectURL(await r.blob());
      if (root.isConnected) paint();
    } catch (e) { /* no picture: the camera icon stays */ }
  }

  // Sign in to one device with the given logins (or the page's list).
  async function signIn(xa, list) {
    const v = devs[xa] = devs[xa] || {};
    v.state = 'trying'; v.msg = ''; paint();
    try {
      const r = await api('/api/discover/autologin', { json: { xaddr: xa, logins: list.map(l => ({ username: l.username, password: l.password || '' })) } });
      if (r.ok) {
        const typed = list.find(l => l.username === r.username);
        Object.assign(v, { state: 'ok', ins: r.inspection, username: r.username, password: typed ? typed.password : '' });
        const cands = (r.inspection.cameras && r.inspection.cameras.length) ? r.inspection.cameras : [r.inspection.suggested];
        v.cams = cands.filter(c => c && c.url).map(c => ({ base: c, name: c.name || 'Camera', pick: true, added: false }));
        if (!v.cams.length) { v.state = 'error'; v.msg = 'Signed in, but the camera reported no video stream.'; }
        else fetchPicture(v);
      } else {
        Object.assign(v, { state: r.auth_failed ? 'auth' : 'error', msg: r.error, u: v.u || (list[0] && list[0].username) || '' });
      }
    } catch (e) { Object.assign(v, { state: 'error', msg: e.body ? tryMsg(e.body) : e.message }); }
    paint();
  }

  function bind() {
    const q = (s) => root.querySelector(s);
    root.querySelectorAll('[data-lu],[data-lp]').forEach(i => { i.oninput = readLogins; });
    q('[data-remember]').onchange = readLogins;
    q('[data-more]').ontoggle = (ev) => { moreOpen = ev.target.open; };
    root.querySelectorAll('[data-eye]').forEach(b => { b.onclick = () => {
      const inp = root.querySelector('[data-lp="' + b.dataset.eye + '"]');
      const show = inp.type === 'password';
      inp.type = show ? 'text' : 'password'; b.textContent = show ? 'Hide' : 'Show'; b.setAttribute('aria-pressed', show ? 'true' : 'false');
    }; });
    const sub0 = q('[data-subnet]');
    sub0.oninput = () => { rangesTyped = sub0.value; };
    // Save the subnets as typed (checked by the server); false when they are not valid.
    const saveWatch = async (extra) => {
      const body = Object.assign({ ranges: q('[data-subnet]').value }, extra || {});
      try { await api('/api/discover/watch', { method: 'PUT', json: body }); rangesTyped = null; await loadWatch(); return true; }
      catch (e) { err = e.body ? tryMsg(e.body) : e.message; paint(); return false; }
    };
    q('[data-watch]').onchange = async (ev) => {
      err = '';
      if (!(await saveWatch({ watch: ev.target.checked }))) return;
      if (ev.target.checked) toast('Checking now, then every hour');
    };
    root.querySelectorAll('[data-ign]').forEach(b => { b.onclick = async () => { await api('/api/discover/ignore', { json: { ip: b.dataset.ign, ignore: true } }).catch(e => msgDlg(e.message)); loadWatch(); }; });
    root.querySelectorAll('[data-unign]').forEach(b => { b.onclick = async () => { await api('/api/discover/ignore', { json: { ip: b.dataset.unign, ignore: false } }).catch(e => msgDlg(e.message)); loadWatch(); }; });
    const si = q('[data-showign]'); if (si) si.onclick = () => { showIgn = !showIgn; paint(); };
    q('[data-scan]').onclick = async () => {
      readLogins();
      const secs = q('[data-secs]').value, sub = q('[data-subnet]').value.trim();
      err = '';
      if (!(await saveWatch())) return;
      scanning = true; paint();
      q('[data-secs]').value = secs; q('[data-subnet]').value = sub;
      if (remember && usable().length) {
        try { await api('/api/discover/logins', { method: 'PUT', json: { logins: usable().map(l => ({ username: l.username, password: l.password })) } }); }
        catch (e) { err = 'Could not save the logins: ' + (e.body ? tryMsg(e.body) : e.message); }
      }
      try {
        await refreshCameras().catch(() => {});
        res = await api('/api/discover?seconds=' + encodeURIComponent(secs) + (sub ? '&subnet=' + encodeURIComponent(sub) : ''));
      } catch (e) { err = e.message; res = null; }
      scanning = false; paint();
      const s2 = q('[data-secs]'); if (s2) s2.value = secs;
      const n2 = q('[data-subnet]'); if (n2) n2.value = sub;
      // Sign in to the new devices, two at a time.
      const list = usable();
      if (!res || !list.length) return;
      const todo = (res.devices || []).filter(d => !configured(d) && d.xaddrs && d.xaddrs[0]).map(d => d.xaddrs[0]);
      const worker = async () => { while (todo.length) await signIn(todo.shift(), list); };
      await Promise.all([worker(), worker()]);
    };
    root.querySelectorAll('.ac-dev').forEach(card => {
      const xa = card.dataset.xa;
      const v = devs[xa] = devs[xa] || {};
      const u = card.querySelector('[data-u]'), p = card.querySelector('[data-p]');
      const sync = () => { if (u) v.u = u.value; if (p) v.p = p.value; };
      if (u) u.oninput = sync;
      if (p) p.oninput = sync;
      const tr = card.querySelector('[data-try]');
      const go = () => { sync(); if (!v.u) { v.msg = 'Enter a username.'; paint(); return; } signIn(xa, [{ username: v.u, password: v.p || '' }]); };
      if (tr) tr.onclick = go;
      [u, p].forEach(i => { if (i) i.onkeydown = (e) => { if (e.key === 'Enter') go(); }; });
      const man = card.querySelector('[data-manual]');
      if (man) man.onclick = () => { v.state = 'manual'; paint(); };
      card.querySelectorAll('.dl-cand').forEach(row => {
        const c = v.cams[+row.dataset.ci];
        const pk = row.querySelector('[data-pick]'), nm = row.querySelector('[data-cn]');
        if (pk) pk.onchange = () => { c.pick = pk.checked; paint(); };
        if (nm) nm.oninput = () => { c.name = nm.value; };
      });
    });
    const addFrom = async (vs, btn) => {
      if (btn) btn.disabled = true;
      let ok = 0, fail = [];
      for (const v of vs) {
        for (const c of (v.cams || [])) {
          if (!c.pick || c.added) continue;
          const body = Object.assign({ record: true, motion: true, enabled: true, overlay: true, audio: true }, c.base,
            { name: (c.name || '').trim() || c.base.name, username: v.username, password: v.password || '' });
          try { await addCameraConfig(body); c.added = true; ok++; }
          catch (e) { fail.push(body.name + ': ' + (e.body ? tryMsg(e.body) : e.message)); }
        }
      }
      if (ok) added += ok;
      paint();
      if (fail.length) msgDlg('Not added: ' + fail.join('; '));
    };
    const gsNext = q('[data-added-next]');
    if (gsNext) gsNext.onclick = async () => (await import('./gettingstarted.js')).openGettingStarted();
    const addSel = q('[data-addsel]');
    if (addSel) addSel.onclick = () => addFrom(Object.values(devs), addSel);
    root.querySelectorAll('.ac-dev').forEach(card => {
      const one = card.querySelector('[data-addone]');
      if (one) one.onclick = () => addFrom([devs[card.dataset.xa]], one);
    });
  }
  paint();
}

registerAdminPage({ id: 'profiles', title: 'Profiles & schedule', order: 50, render: profilesPage, leave: stopTimers });
registerAdminPage({ id: 'ai', title: 'AI', order: 60, render: aiPage });
registerAdminPage({ id: 'retention', title: 'Storage & retention', order: 69, render: (el) => storagePage(el, 'retention') });
registerAdminPage({ id: 'storage', title: 'Drives & folders', order: 70, render: (el) => storagePage(el, 'drives') });
registerAdminPage({ id: 'discovery', title: 'Discovery', order: 80, render: discoveryPage });

// Shared with the other settings pages built the same way (admin_presence.js).
export { head, pageBar, wireBar, postSettings, guardAdmin };
