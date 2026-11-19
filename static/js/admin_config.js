// admin_config.js - Admin pages: Profiles & schedule, AI, Storage, Discovery.
// Every page edits a draft copy of what the server holds, shows an "unsaved changes" bar,
// and saves with POST /api/settings (sections MERGE on the server; unsent fields keep their value).

import { t } from './i18n.js';
import { api, esc, toast, msgDlg, fmtSize, state, ic, refreshCameras, pageBar, wireBar, confirmDialog, liveSwitch, swHtml, fld, savedToast, errText, stillWaiting, objName } from './core.js';
import { lead, how, more, notice } from './sp_kit.js';
import { registerAdminPage } from './admin.js';
import { showTab } from './nav.js';
import { addCameraConfig } from './cameras.js';
import { openSettingsDialog } from './settings.js';
import { armStrip, armData } from './armstrip.js';

const clone = (o) => JSON.parse(JSON.stringify(o));
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const DAYS = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];
const DAY_LABEL = [t('admin_config.day_mon', 'Mon'), t('admin_config.day_tue', 'Tue'), t('admin_config.day_wed', 'Wed'), t('admin_config.day_thu', 'Thu'), t('admin_config.day_fri', 'Fri'), t('admin_config.day_sat', 'Sat'), t('admin_config.day_sun', 'Sun')];
const HUES = [38, 205, 150, 330, 270, 8, 180, 75];
let timers = [];
const later = (fn, ms) => { const id = setInterval(fn, ms); timers.push(id); };
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
  el.innerHTML = '<p class="empty">' + t('admin_config.admin_signin_required','Admin sign-in required.') + '</p>';
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

const OVR = [['motion', t('admin_config.ovr_motion', 'Motion')], ['alerts', t('admin_config.alerts', 'Alerts')], ['ai', 'AI'], ['record', t('admin_config.ovr_record', 'Record')], ['record_motion_only', t('admin_config.ovr_motion_only', 'Motion only')]];

function profilesPage(el) {
  if (!guardAdmin(el)) return;
  let saved = null, draft = null, sel = 0, live = null, gMotion = null, shieldOn = true;
  const shown = new Set(), strips = new Map();   // cameras whose "when it watches" strip is open
  el.innerHTML = '<div class="ac"><p class="empty">' + t('admin_config.loading','Loading...') + '</p></div>';

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
    const untilTxt = man && man.until ? new Date(man.until * 1000).toLocaleString([], { weekday: 'short', hour: '2-digit', minute: '2-digit' }) : '';
    const h = hue(live.active);
    box.innerHTML =
      '<div class="ac-live-main"><span class="ac-label">' + t('admin_config.active_now','Active now') + '</span>' +
      (live.active ? '<span class="ac-pill" style="--h:' + (h == null ? 38 : h) + '">' + esc(live.active) + '</span>' : '<span class="ac-pill none">' + t('admin_config.no_profile', 'No profile') + '</span>') +
      '<span class="hint ac-inl">' + (man ? (untilTxt ? t('admin_config.switched_by_hand_until', 'switched by hand until {when}', { when: esc(untilTxt) }) : t('admin_config.switched_by_hand','switched by hand')) : (live.scheduled ? t('admin_config.from_schedule','from the schedule') : t('admin_config.cameras_own','cameras use their own settings'))) + '</span></div>' +
      '<div class="ac-live-ctl">' +
      '<select data-sw-name aria-label="' + t('admin_config.profile','Profile') + '">' + names.map(n => '<option' + (n === live.active ? ' selected' : '') + '>' + esc(n) + '</option>').join('') + '</select>' +
      '<select data-sw-min aria-label="' + t('admin_config.for_how_long','For how long') + '"><option value="">' + t('admin_config.until_changed','until changed') + '</option><option value="60">' + t('admin_config.one_hour','1 hour') + '</option><option value="240">' + t('admin_config.four_hours', '4 hours') + '</option><option value="480">' + t('admin_config.eight_hours','8 hours') + '</option><option value="1440">' + t('admin_config.twentyfour_hours','24 hours') + '</option></select>' +
      '<button class="btn" data-sw-go' + (names.length ? '' : ' disabled') + '>' + t('admin_config.switch','Switch') + '</button>' +
      (man ? '<button class="btn" data-sw-back>' + t('admin_config.back_to_schedule','Back to schedule') + '</button>' : '') + '</div>';
    const sw = async (name, minutes) => {
      try { live = await api('/api/profile', { json: { name, minutes } }); paintLive(); toast(name ? t('admin_config.profile_name_toast', 'Profile: {name}', { name }) : t('admin_config.following_schedule', 'Following the schedule')); } catch (e) { msgDlg(e.message); }
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
        const clock = String(Math.floor(k / 2)).padStart(2, '0') + ':' + (k % 2 ? '30' : '00');
        h += '<i class="' + (hh == null ? 'none' : pr.src === 'default' ? 'dflt' : '') + (d === today && k === nowSlot ? ' now' : '') + '"' +
          (hh == null ? '' : ' style="--h:' + hh + '"') + ' title="' + DAY_LABEL[d] + ' ' + clock + ' - ' + esc(pr.name || t('admin_config.no_profile_lc', 'no profile')) + (pr.src === 'default' && pr.name ? ' ' + t('admin_config.default_paren', '(default)') : '') + '"></i>';
      }
      h += '</div></div>';
    }
    box.innerHTML = h;
    const lg = el.querySelector('[data-legend]');
    if (lg) lg.innerHTML = draft.profiles.map(p => '<span class="ac-key"><i style="--h:' + hue(p.name) + '"></i>' + esc(p.name) + '</span>').join('') +
      (draft.default_profile ? '<span class="ac-key"><i class="dflt" style="--h:' + hue(draft.default_profile) + '"></i>' + t('admin_config.default_no_rule', 'default (no rule matches)') + '</span>' : '<span class="ac-key"><i class="none"></i>' + t('admin_config.no_profile_lc', 'no profile') + '</span>');
  }

  // No profile yet: only the way in. The schedule and location come once a profile exists.
  function paintEmpty() {
    el.innerHTML = '<div class="ac">' + lead(t('admin_config.profile_mode_desc', 'A profile is a mode such as Home or Away that changes how your cameras behave.'), t('admin_config.start_home_away', 'Start with Home and Away.')) +
      '<div class="sp-empty"><h3>' + t('admin_config.no_profiles_yet', 'No profiles yet') + '</h3>' +
      '<p>' + t('admin_config.home_away_desc', 'Home and Away: Away turns detection on (Armed); Home leaves every camera as it is now. You can switch by hand or set a weekly schedule afterwards.') + '</p>' +
      '<div class="row"><button class="primary" type="button" data-homeaway>' + t('admin_config.create_home_away', 'Create Home and Away') + '</button><button class="btn" type="button" data-newprof>' + t('admin_config.create_a_profile', 'Create a profile') + '</button></div></div></div>';
    el.querySelector('[data-homeaway]').onclick = async (e) => {
      e.target.disabled = true;
      try {
        const next = clone(draft);
        // starter profiles get names in the user's language (they are ordinary, renamable profile names)
        const home = t('admin_config.profile_home', 'Home'), away = t('admin_config.profile_away', 'Away');
        next.profiles = [{ name: home, cameras: {} }, { name: away, armed: true, cameras: {} }];
        next.default_profile = home;
        await postSettings({ automation: cleanAuto(next) });
        savedToast(t('admin_config.home_away_created', 'Home and Away created'));
        await load();
      } catch (err) { toast(errText(err), { kind: 'err' }); e.target.disabled = false; }
    };
    el.querySelector('[data-newprof]').onclick = () => {
      draft.profiles.push({ name: t('admin_config.profile_n', 'Profile {n}', { n: 1 }), cameras: {} }); sel = 0; paint();
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
    let h = lead(t('admin_config.switch_cameras_desc', 'Switch cameras between modes like Home and Away, by hand or on a weekly schedule.'), t('admin_config.one_profile_per_situation', 'One profile per situation, with a schedule only if the times are regular.')) +
      how(t('admin_config.profile_overrides_desc', 'A profile is a named set of overrides (arming, alerts, per-camera motion / AI / recording). The schedule picks one by day and time; a manual switch wins until you go back to the schedule.')) +
      '<div class="ac-card ac-live" data-live></div>' +
      '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">' + t('admin_config.profiles', 'Profiles') + '</div><span class="ac-sp"></span>' +
      '<label class="ac-inl">' + t('admin_config.default_label', 'Default') + ' <select data-default><option value="">' + t('admin_config.none_cameras_own', 'none (cameras use their own settings)') + '</option>' +
      P.map(x => '<option' + (x.name === draft.default_profile ? ' selected' : '') + '>' + esc(x.name) + '</option>').join('') + '</select></label></div>' +
      '<div class="chips ac-profs">' + P.map((x, i) => '<button class="chip' + (i === sel ? ' on' : '') + '" data-sel="' + i + '"><i class="ac-dot" style="--h:' + HUES[i % HUES.length] + '"></i>' + esc(x.name) + '</button>').join('') +
      '<button class="chip" data-add>' + t('admin_config.new_profile', '+ New profile') + '</button></div>';
    if (p) {
      h += '<div class="ac-edit"><div class="ac-grid3">' +
        '<label>' + t('admin_config.name', 'Name') + ' <input data-pname value="' + esc(p.name) + '" maxlength="40"></label>' +
        '<label>' + t('admin_config.armed_or_disarmed', 'Armed or Disarmed') + ' <select data-parm>' + optYesNo(triOf(p.armed), [t('admin_config.leave_as_is', 'leave as is'), t('admin_config.armed', 'Armed'), t('admin_config.disarmed', 'Disarmed')]) + '</select></label>' +
        '<label>' + t('admin_config.alerts', 'Alerts') + ' <select data-palr>' + optYesNo(triOf(p.alerts), [t('admin_config.leave_as_is', 'leave as is'), t('admin_config.send', 'send'), t('admin_config.mute', 'mute')]) + '</select></label></div>' +
        '<details class="ac-more sp-more"><summary>' + t('admin_config.per_camera_overrides', 'Per-camera overrides') + '</summary><div class="hint">' + t('admin_config.dash_means', 'A dash means the camera keeps its own setting.') + '</div>';
      const keys = camNames.slice();
      for (const k of Object.keys(p.cameras)) if (!keys.includes(k)) keys.push(k);
      if (!keys.length) h += '<p class="hint">' + t('admin_config.no_cameras_yet', 'No cameras yet.') + '</p>';
      else {
        h += '<div class="ac-ovr"><div class="ac-ovr-h"><span>' + t('admin_config.camera', 'Camera') + '</span>' + OVR.map(o => '<span>' + o[1] + '</span>').join('') + '</div>';
        for (const k of keys) {
          const o = p.cameras[k] || {};
          h += '<div class="ac-ovr-r"><span class="ac-cam' + (camNames.includes(k) ? '' : ' gone') + '" title="' + (camNames.includes(k) ? '' : t('admin_config.no_camera_this_name', 'No camera with this name')) + '">' + esc(k) +
            (camNames.includes(k) ? ' <button type="button" class="as-show" data-armshow="' + esc(k) + '" title="' + t('admin_config.show_when_camera_watches', 'Show when this camera detects and alerts over the next 7 days') + '">' + (shown.has(k) ? t('admin_config.hide_lc', 'hide') : t('admin_config.show_lc', 'show')) + '</button>' : '') + '</span>' +
            OVR.map(o2 => '<label data-l="' + o2[1] + '"><select data-ovr="' + esc(k) + '" data-f="' + o2[0] + '">' + optYesNo(triOf(o[o2[0]]), ['-', t('admin_config.on_lc', 'on'), t('admin_config.off_lc', 'off')]) + '</select></label>').join('') + '</div>' +
            '<div class="ac-armrow" data-armrow="' + esc(k) + '"' + (shown.has(k) ? '' : ' hidden') + '></div>';
        }
        h += '</div>';
      }
      h += '</details><div class="ac-row"><button class="btn danger" data-del>' + t('admin_config.delete_profile', 'Delete profile') + '</button></div></div>';
    }
    h += '</div>';
    // schedule
    h += '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">' + t('admin_config.weekly_schedule', 'Weekly schedule') + '</div><span class="ac-sp"></span><button class="btn" data-addrule' + (P.length ? '' : ' disabled') + '>' + ic('plus') + ' ' + t('admin_config.add_rule', 'Add rule') + '</button></div>' +
      '<div class="hint">' + t('admin_config.rules_explanation', 'Rules are checked top to bottom; the first one that covers the moment wins, otherwise the default profile applies. A window that ends before it starts runs past midnight.') + '</div>' +
      '<details class="ac-more sp-more"' + (draft.latitude != null ? ' open' : '') + '><summary>' + t('admin_config.location_for_sun', 'Location (for sunrise and sunset times)') + '</summary><div class="ac-row ac-loc"><span class="ac-label">' + t('admin_config.location', 'Location') + '</span>' +
        '<input data-lat type="number" step="0.0001" min="-90" max="90" placeholder="' + t('admin_config.latitude', 'Latitude') + '" aria-label="' + t('admin_config.latitude', 'Latitude') + '" value="' + (draft.latitude != null ? draft.latitude : '') + '">' +
        '<input data-lon type="number" step="0.0001" min="-180" max="180" placeholder="' + t('admin_config.longitude', 'Longitude') + '" aria-label="' + t('admin_config.longitude', 'Longitude') + '" value="' + (draft.longitude != null ? draft.longitude : '') + '">' +
        '<button class="btn" data-geo title="' + t('admin_config.fill_from_device', 'Fill in from this device (the browser asks first)') + '">' + t('admin_config.use_device_location', "Use this device's location") + '</button>' +
        '<span class="hint ac-inl">' + (live && live.sun ? t('admin_config.today_sun', 'Today: sunrise {rise}, sunset {set}', { rise: esc(live.sun.sunrise), set: esc(live.sun.sunset) }) : t('admin_config.set_use_sun_rules', 'Set it to use sunrise / sunset in rules')) + '</span></div></details>' +
      '<div class="ac-wk" data-strip></div><div class="ac-legend" data-legend></div><div class="ac-rules">';
    if (!draft.schedule.length) h += '<p class="hint">' + t('admin_config.no_rules', 'No rules. The default profile is always active.') + '</p>';
    draft.schedule.forEach((r, i) => {
      h += '<div class="ac-rule" data-i="' + i + '"><span class="ac-n">' + (i + 1) + '</span><div class="ac-days">' +
        DAYS.map((d, di) => '<button class="chip' + ((r.days || []).includes(d) ? ' on' : '') + '" data-day="' + d + '" title="' + DAY_LABEL[di] + '">' + DAY_LABEL[di] + '</button>').join('') + '</div>' +
        '<div class="ac-times">' + timeCtl('from', r.from) + '<span>' + t('admin_config.to', 'to') + '</span>' + timeCtl('to', r.to) + '</div>' +
        '<select data-rp>' + P.map(x => '<option' + (x.name === r.profile ? ' selected' : '') + '>' + esc(x.name) + '</option>').join('') + '</select>' +
        '<div class="ac-mv"><button class="btn icon" data-mv="-1" title="' + t('admin_config.move_up', 'Move up') + '" aria-label="' + t('admin_config.move_rule_up', 'Move rule up') + '"' + (i === 0 ? ' disabled' : '') + '>&uarr;</button><button class="btn icon" data-mv="1" title="' + t('admin_config.move_down', 'Move down') + '" aria-label="' + t('admin_config.move_rule_down', 'Move rule down') + '"' + (i === draft.schedule.length - 1 ? ' disabled' : '') + '>&darr;</button><button class="btn icon" data-rm title="' + t('admin_config.delete_rule', 'Delete rule') + '" aria-label="' + t('admin_config.delete_rule', 'Delete rule') + '">&times;</button></div></div>';
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
    if (b) b.textContent = t('admin_config.hide_lc', 'hide');
  }

  // One end of a rule: Time / Sunrise / Sunset, then a clock time or an offset in minutes.
  function timeCtl(which, val) {
    const s = parseSpec(val);
    return '<span class="ac-tctl" data-tc="' + which + '"><select data-tk aria-label="' + (which === 'from' ? t('admin_config.start', 'Start') : t('admin_config.end', 'End')) + '">' +
      [['time', t('admin_config.time_option', 'Time')], ['sunrise', t('admin_config.sunrise_option', 'Sunrise')], ['sunset', t('admin_config.sunset_option', 'Sunset')]].map(o => '<option value="' + o[0] + '"' + (o[0] === s.kind ? ' selected' : '') + '>' + o[1] + '</option>').join('') + '</select>' +
      (s.kind === 'time'
        ? '<input type="time" data-tv value="' + esc(s.time) + '">'
        : '<input type="number" data-to step="5" min="-720" max="720" value="' + (s.off || 0) + '" title="' + t('admin_config.offset_tooltip', 'Minutes before (-) or after (+)') + '" aria-label="' + t('admin_config.offset_in_minutes', 'Offset in minutes') + '"><span class="hint">' + t('admin_config.min', 'min') + '</span>') +
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
      do { name = t('admin_config.profile_n', 'Profile {n}', { n: n++ }); } while (P.some(x => x.name === name));
      P.push({ name, cameras: {} }); sel = P.length - 1; paint(); const i = q('[data-pname]'); if (i) { i.focus(); i.select(); }
    };
    q('[data-default]').onchange = (e) => { draft.default_profile = e.target.value; mark(); };
    if (p) {
      q('[data-pname]').onchange = (e) => {
        const n = e.target.value.trim(), old = p.name;
        if (!n) { toast(t('admin_config.profile_name_required', 'A profile needs a name')); e.target.value = old; return; }
        if (P.some((x, i) => i !== sel && x.name.toLowerCase() === n.toLowerCase())) { toast(t('admin_config.name_taken', 'That name is taken')); e.target.value = old; return; }
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
          if (shown.has(n)) { shown.delete(n); const e = strips.get(n); if (e) e.strip.destroy(); strips.delete(n); host.hidden = true; host.textContent = ''; b.textContent = t('admin_config.show_lc', 'show'); }
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
        if (!(await confirmDialog({ title: t('admin_config.delete_profile_q', 'Delete profile {name}?', { name: p.name }), body: used ? t('admin_config.rules_deleted_too', '{n, plural, one {Its # schedule rule is} other {Its # schedule rules are}} deleted too. Takes effect when you save.', { n: used }) : t('admin_config.takes_effect_on_save', 'Takes effect when you save.'), ok: t('admin_config.delete_profile', 'Delete profile'), danger: true }))) return;
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
      if (!navigator.geolocation) { toast(t('admin_config.no_geolocation', 'This browser cannot share its location')); return; }
      navigator.geolocation.getCurrentPosition((pos) => {
        q('[data-lat]').value = pos.coords.latitude.toFixed(4);
        q('[data-lon]').value = pos.coords.longitude.toFixed(4);
        setLoc();
      }, (e) => toast(t('admin_config.location_unavailable', 'Location not available: {msg}', { msg: e.message })), { timeout: 10000 });
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
  el.innerHTML = '<div class="ac"><p class="empty">' + t('admin_config.loading', 'Loading...') + '</p></div>';
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
    if (!mstat) return '<p class="hint">' + t('admin_config.loading_models', 'Loading models...') + '</p>';
    const job = mstat.job || {};
    const busy = job.state === 'downloading' || job.state === 'loading';
    const pick = mpick || mstat.current || 's';
    const pct = job.total ? Math.round(job.bytes / job.total * 100) : null;
    return '<div class="ai-choice">' + mstat.models.map(m => {
      const inUse = mstat.current === m.id && mstat.loaded;
      return '<label class="ai-opt' + (pick === m.id ? ' on' : '') + '"><input type="radio" name="aimodel" value="' + m.id + '"' + (pick === m.id ? ' checked' : '') + (busy ? ' disabled' : '') + '>' +
        '<span><span class="ai-name"><b>' + esc(m.name) + '</b>' + (inUse ? '<span class="badge on dot">' + t('admin_config.in_use', 'in use') + '</span>' : '') + (m.id === 's' ? '<span class="badge info">' + t('admin_config.recommended', 'recommended') + '</span>' : '') + '</span>' +
        '<span class="hint">' + esc(m.about) + (m.downloaded === false ? ' ' + t('admin_config.downloaded_when_choose', 'Downloaded when you choose it.') : '') + '</span>' +
        (inUse && mstat.p50 != null ? '<span class="hint">' + (mstat.device ? t('admin_config.measured_here_on', 'Measured here: {time} per check on {device}.', { time: mstat.p50 >= 1000 ? (mstat.p50 / 1000).toFixed(1) + ' s' : Math.round(mstat.p50) + ' ms', device: esc(mstat.device) }) : t('admin_config.measured_here', 'Measured here: {time} per check.', { time: mstat.p50 >= 1000 ? (mstat.p50 / 1000).toFixed(1) + ' s' : Math.round(mstat.p50) + ' ms' })) + '</span>' : '') + '</span></label>';
    }).join('') + '</div>' +
      (busy ? '<div class="ai-prog"><div class="ai-bar"><i style="width:' + (pct == null ? 30 : pct) + '%"></i></div><span class="hint">' +
        (job.state === 'loading' ? t('admin_config.loading_the_model', 'Loading the model...') : (job.total ? t('admin_config.downloading_of', 'Downloading {done} of {total} MB', { done: (job.bytes / 1e6).toFixed(1), total: (job.total / 1e6).toFixed(1) }) : t('admin_config.downloading', 'Downloading...'))) + '</span></div>'
        : '<div class="ac-row">' + ((mstat.current === pick && mstat.loaded) ? '<span class="hint">' + t('admin_config.model_in_use', 'This model is in use.') + '</span>'
          : '<button class="primary" data-getmodel>' + (mstat.current && mstat.loaded ? t('admin_config.switch_to', 'Switch to {name}', { name: esc((mstat.models.find(m => m.id === pick) || {}).name || pick) }) : t('admin_config.download_and_turn_on_ai', 'Download and turn on AI')) + '</button>') +
          (job.state === 'error' ? '<span class="ac-err">' + t('admin_config.model_download_failed', 'Could not download the AI model: the download site did not answer, or the file did not arrive intact. Nothing is wrong with your cameras or this server. Try again in a few minutes.') + '</span>' : '') + '</div>' +
          '<div class="hint">' + t('admin_config.model_source_checksum', 'The model comes from the official YOLOX release and is checked against its published checksum. It runs on this server; pictures never leave it.') + '</div>');
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
      catch (e) { toast(t('admin_config.could_not_start', 'Could not start: {why}', { why: e.body || e.message })); }
      refreshModels();
    };
  }

  function paint() {
    const labels = AI_LABELS.concat(draft.labels.filter(l => !AI_LABELS.includes(l)));
    const sBuiltin = draft.source === 'builtin';
    let badge = '';
    if (draft.enabled && sBuiltin && saved.model) {
      badge = status.builtin_loaded ? '<span class="badge on dot">' + t('admin_config.model_loaded', 'model loaded') + '</span>' :
        (status.model_exists === false ? '<span class="badge off dot">' + t('admin_config.model_file_not_found', 'model file not found') + '</span>' : '<span class="badge off dot">' + t('admin_config.model_not_loaded', 'model not loaded') + '</span>');
    } else if (saved.model && status.model_exists === false) badge = '<span class="badge off dot">' + t('admin_config.model_file_not_found', 'model file not found') + '</span>';
    else if (saved.model && status.model_exists) badge = '<span class="badge info dot">' + t('admin_config.model_file_found', 'model file found') + '</span>';
    const camRows = state.cams.map(c => '<tr><td>' + esc(c.name) + '</td><td>' + (c.ai === false ? '<span class="badge off">' + t('admin_config.off_lc', 'off') + '</span>' : c.ai ? '<span class="badge on">' + t('admin_config.on_lc', 'on') + '</span>' : '<span class="badge">' + t('admin_config.default_lc', 'default') + '</span>') +
      '</td><td class="hint">' + (c.ai_labels && c.ai_labels.length ? esc(c.ai_labels.map(objName).join(', ')) : t('admin_config.global_labels', 'global labels')) + '</td></tr>').join('');
    el.innerHTML = '<div class="ac">' + lead(t('admin_config.checks_what_moved', 'Checks what moved, so you are alerted for a person or car and not for shadows, rain or bugs.'), t('admin_config.turn_on_builtin', 'Turn it on and keep the built-in model.')) +
      how(t('admin_config.ai_confirms', 'AI confirms motion events by checking what moved. Events with none of the chosen labels are still recorded but send no alert. The model comes from the official YOLOX release and runs on this server; pictures never leave it.')) +
      '<div class="ac-card"><div class="ac-row">' + swHtml('data-en', draft.enabled, t('admin_config.enable_ai_checks', 'Enable AI checks')) + '<span class="ac-sp"></span>' + badge + '</div>' +
      '<div class="ac-sub">' + t('admin_config.detector', 'Detector') + '</div><div class="seg ac-seg"><button data-src="builtin" class="' + (sBuiltin ? 'on' : '') + '">' + t('admin_config.builtin_model', 'Built-in model') + '</button><button data-src="server" class="' + (!sBuiltin ? 'on' : '') + '" title="' + t('admin_config.use_another_computer', 'Use another computer for AI (advanced)') + '">' + t('admin_config.another_computer', 'Another computer (advanced)') + '</button></div>' +
      (sBuiltin ?
        '<div class="ai-models" data-models>' + modelCards() + '</div>' +
        more(t('admin_config.advanced', 'Advanced'), '<label class="ac-f">' + t('admin_config.different_model_file', 'Use a different model file (YOLOX .onnx)') + ' <input data-model value="' + esc(draft.model) + '" placeholder="' + t('admin_config.eg_model_path', 'e.g. D:/models/yolox_m.onnx') + '"></label><div class="hint">' + t('admin_config.path_on_server', 'A path on the server. Takes effect when you save.') + '</div>', !!draft.model) :
        '<label class="ac-f">' + t('admin_config.address_other_computer', 'Address of the other computer') + ' <input data-url value="' + esc(draft.url) + '" placeholder="http://192.168.1.20:32168"></label><div class="hint">' + t('admin_config.ai_server_desc', 'A computer running an AI server with the common <code>/v1/vision/detection</code> web API. Used when no built-in model is set.') + '</div>') +
      (sBuiltin ? hardwareCard() : '') +
      '</div><div class="ac-card"><div class="ac-card-t">' + t('admin_config.sensitivity', 'Sensitivity') + '</div>' +
      '<label class="ac-f">' + t('admin_config.how_sure_ai', 'How sure AI must be') + ' <b data-sv>' + Math.round(draft.min_score * 100) + '%</b><input type="range" data-score min="0.05" max="0.95" step="0.05" value="' + draft.min_score + '"></label>' +
      '<div class="hint">' + t('admin_config.higher_lower', 'Higher = fewer false alerts but real ones may be missed; lower = more alerts, more false ones.') + '</div>' +
      swHtml('data-parked', draft.ignore_parked, t('admin_config.ignore_unmoved', 'Ignore objects that have not moved'), t('admin_config.ignore_unmoved_desc', 'A car parked in view (or someone sitting still) for a few minutes no longer confirms every motion event; one that arrives or leaves still does. AI also remembers spots where the same kind of thing is seen event after event (a bush it takes for a person, a car parked between events): they stop alerting until they move or go. Cameras can override this (AI tab).')) +
      swHtml('data-strict', draft.strict, t('admin_config.stricter_confirmation', 'Stricter confirmation'), t('admin_config.stricter_desc', 'An object alerts only when AI sees it in 2 of 3 looks in a row and it moved between them. Stops one-off wrong guesses and things that never move (a statue, a bush taken for a person); someone passing very quickly may be missed. Cameras can override this (AI tab).')) +
      swHtml('data-hideun', draft.hide_unconfirmed, t('admin_config.show_only_confirmed', 'Show only events AI confirms'), t('admin_config.show_only_confirmed_desc', 'Motion where AI found none of the alert labels (a flag or branches in the wind) stays out of Events and the timeline. Nothing is deleted; Events can still show them. Cameras can override this (AI tab).')) +
      more(t('admin_config.more_settings', 'More settings'), '<div class="ac-grid3">' + fld(t('admin_config.frames_per_event', 'Frames checked per event'), 'data-mc min="1" max="100" value="' + draft.max_checks + '"', t('admin_config.unit_frames', 'frames')) + fld(t('admin_config.cpu_threads', 'CPU threads'), 'data-th min="1" max="32" value="' + draft.threads + '"', t('admin_config.unit_threads', 'threads')) + '</div>' +
      '<div class="ac-sub">' + t('admin_config.alert_zones_sub', 'Alert zones (loitering, line crossing)') + '</div>' +
      '<div class="ac-grid3">' + fld(t('admin_config.look_every', 'Look every'), 'data-zint min="250" max="5000" step="50" value="' + draft.zone_interval_ms + '"', t('admin_config.unit_milliseconds', 'milliseconds')) + fld(t('admin_config.keep_watching', 'Keep watching a person standing still for'), 'data-zhold min="0" max="3600" step="30" value="' + draft.zone_hold_seconds + '"', t('admin_config.unit_seconds', 'seconds')) + '</div>' +
      '<div class="hint">' + t('admin_config.zone_look_desc', 'Cameras with alert zones look at the picture this often while there is motion, to follow people and cars. Faster follows quick movers better and uses more CPU. After the motion stops, it keeps watching while someone is still inside an area, for the time above.') + '</div>') + '</div>' +
      '<div class="ac-card"><div class="ac-card-t">' + t('admin_config.alert_labels', 'Alert labels') + '</div><div class="hint">' + t('admin_config.objects_confirm', 'Objects that confirm an event.') + '</div><div class="chips">' +
      labels.map(l => '<button class="chip' + (draft.labels.includes(l) ? ' on' : '') + '" data-lab="' + esc(l) + '">' + esc(objName(l)) + '</button>').join('') +
      '</div><div class="ac-row"><input data-newlab placeholder="' + t('admin_config.add_a_label', 'Add a label') + '" maxlength="30"><button class="btn" data-addlab>' + t('admin_config.add', 'Add') + '</button></div></div>' +
      '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">' + t('admin_config.per_camera', 'Per camera') + '</div><span class="ac-sp"></span><button class="btn" data-cams>' + t('admin_config.camera_settings', 'Camera settings') + '</button></div>' +
      '<div class="hint">' + t('admin_config.each_camera_ai', 'Each camera can turn AI on or off and use its own labels; set that in each camera&#39;s settings (Motion &amp; AI).') + '</div>' +
      (camRows ? '<table class="ac-tbl"><thead><tr><th>' + t('admin_config.camera', 'Camera') + '</th><th>AI</th><th>' + t('admin_config.labels', 'Labels') + '</th></tr></thead><tbody>' + camRows + '</tbody></table>' : '<p class="hint">' + t('admin_config.no_cameras_yet', 'No cameras yet.') + '</p>') + '</div>' +
      pageBar() + '</div>';
    bind();
  }
  // Where the built-in model runs: a graphics card (Windows, Linux with
  // NVIDIA or Intel, Apple silicon) or the CPU, and why not the card when one is there.
  function hardwareCard() {
    const gpus = status.gpus || [];
    const on = status.running_on;
    const now = '<div class="hint">' + t('admin_config.running_on', 'Running on: <b>{what}</b>', { what: esc(on || (status.builtin_loaded ? t('admin_config.the_cpu', 'the CPU') : t('admin_config.not_loaded', 'not loaded'))) }) +
        (on && status.running_api ? ' (' + esc(status.running_api) + ')' : '') + '</div>' +
      (!on && status.builtin_loaded && draft.gpu && status.gpu_note ? '<div class="hint">' + t('admin_config.not_on_graphics_card', 'Not on the graphics card: {why}', { why: esc(status.gpu_note) }) + '</div>' : '');
    if (!gpus.length) return '<div class="ac-sub">' + t('admin_config.hardware', 'Hardware') + '</div>' + now + '<div class="hint">' + t('admin_config.no_graphics_card', 'No graphics card the AI can use was found (Windows: a DirectX 12 card; Linux: an NVIDIA card with its driver, or Intel graphics; Mac: Apple silicon), so it uses the CPU.') + '</div>';
    // (The server's own choice: NVIDIA over Intel, then the most memory.)
    const best = gpus.find(g => g.name === status.auto_gpu) || gpus.slice().sort((a, b) => b.memory_mb - a.memory_mb || a.index - b.index)[0];
    const gb = (g) => g.memory_mb >= 1024 ? ' (' + Math.round(g.memory_mb / 1024) + ' GB)' : '';
    return '<div class="ac-sub">' + t('admin_config.hardware', 'Hardware') + '</div>' + now +
      swHtml('data-gpu', draft.gpu, t('admin_config.use_graphics_card', 'Use a graphics card'), t('admin_config.graphics_card_desc', 'Much faster than the CPU; the CPU takes over if the card cannot be used.')) +
      '<label class="ac-f">' + t('admin_config.graphics_card', 'Graphics card') + ' <select data-gpuname' + (draft.gpu ? '' : ' disabled') + '>' +
        '<option value="">' + t('admin_config.automatic_name', 'Automatic: {name}', { name: esc(best.name) + gb(best) }) + '</option>' +
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
      if (r.warning) { toast(t('admin_config.saved_but', 'Saved, but: {warning}', { warning: r.warning })); return false; }
    }, () => { draft = clone(saved); paint(); });
    // On/off settings save when flipped (only that value is sent).
    const flip = (sel, key) => { const i = q(sel); if (!i) return; liveSwitch(i, async (on) => {
      const r = await postSettings({ ai: { [key]: on } });
      saved[key] = on; draft[key] = on; upd();
      if (r.warning) { toast(t('admin_config.saved_but', 'Saved, but: {warning}', { warning: r.warning })); return false; }
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
const camsWord = (n) => t('admin_config.n_cameras', '{n, plural, one {# camera} other {# cameras}}', { n });
function whoUses(f, n) {
  if (!f.archive) return camsWord(n);
  const others = state.cams.filter(c => !c.remote).length - n;
  return n ? t('admin_config.archive_for_record', 'archive for {others}; {mine} record here', { others: camsWord(Math.max(0, others)), mine: camsWord(n) }) : t('admin_config.archive_for', 'archive for {others}', { others: camsWord(Math.max(0, others)) });
}

function storagePage(el, mode) {
  if (!guardAdmin(el)) return;
  const retention = mode === 'retention';
  let saved, draft, st, dataDir = '', drives = [], plan = null, priv = {}, probes = {}, ends = {}, privTimer = null;
  el.innerHTML = '<div class="ac"><p class="empty">' + t('admin_config.loading', 'Loading...') + '</p></div>';
  stillWaiting(el.firstElementChild, () => load());
  const norm = (s) => ({
    segment_seconds: s.segment_seconds, retention_days: s.retention_days, max_mb: s.max_mb,
    event_keep_days: s.event_keep_days == null ? 21 : s.event_keep_days,
    sub_copy: !!s.sub_copy,
    folders: (s.folders || []).map(f => ({ name: f.name, path: String(f.path), max_mb: f.max_mb || 0, archive: !!f.archive, private: f.private === true ? true : f.private === false ? false : null })),
    min_free_percent: s.min_free_percent != null ? s.min_free_percent : 10,
  });
  // Recordings whose files are gone (src/missing.rs): what the start-up check noticed (both
  // pages) and the check with its Remove button (Drives & folders). Drawn into their own boxes so
  // the progress can update without drawing the page again.
  let miss = null, missErr = '', missTimer = null, missBusy = false;
  const missInclude = {};
  async function loadMissing() {
    clearTimeout(missTimer);
    try { miss = await api('/api/storage/missing'); missErr = ''; } catch (e) { missErr = errText(e); }
    if (!el.isConnected) return;
    paintMissing();
    if (miss && ['scanning', 'applying'].includes(miss.job.state)) missTimer = setTimeout(loadMissing, 1000);
  }
  const fmtDay = (ms) => ms ? new Date(ms).toLocaleDateString() : '';
  const MISS_PHASE = { recordings: t('admin_config.phase_recordings', 'Looking for recordings'), events: t('admin_config.phase_events', 'Looking at events'), backup: t('admin_config.phase_backup', 'Backing up the database'), checking: t('admin_config.phase_checking', 'Looking for each one again'), removing: t('admin_config.phase_removing', 'Removing from the database') };
  function missNoteHtml() {
    const notes = miss && miss.detected ? miss.detected.notes || [] : [];
    if (!notes.length) return '';
    return '<div class="ac-card ac-warncard"><div class="ac-card-t">' + ic('alert') + ' ' + t('admin_config.recordings_missing', 'Recordings missing') + '</div>' +
      notes.map(n => '<p class="hint warn">' + esc(n.text.charAt(0).toUpperCase() + n.text.slice(1)) + '.</p>').join('') +
      (retention ? '<div class="hint">' + t('admin_config.to_remove_them', 'To remove them from the database: Drives &amp; folders, Recordings whose files are gone.') + '</div>' : '') + '</div>';
  }
  // Why a folder's missing recordings are left alone unless ticked (see src/missing.rs confirm_why).
  const WHY = {
    not_found: () => t('admin_config.not_found', 'the folder is not there'),
    not_configured: () => t('admin_config.not_configured', 'no folder of this name is in the settings'),
    all_missing: () => t('admin_config.all_missing', 'none of its recordings are there'),
    most_missing: () => t('admin_config.most_missing', 'most of its recordings are missing'),
    cameras_all_missing: (f) => t('admin_config.cameras_all_missing', 'every recording of {cameras} in it is missing', { cameras: f.whole_cameras.join(', ') }),
    not_mounted: () => t('admin_config.not_mounted', 'is the drive mounted?'),
  };
  function folderLine(f) {
    const n = f.missing.toLocaleString(), rows = f.rows.toLocaleString();
    const what = {
      ok: f.rows ? t('admin_config.ok_all_there', 'all {rows} there', { rows }) : t('admin_config.ok_no_recordings', 'no recordings'),
      some_missing: t('admin_config.some_missing', '{n} of {rows} missing ({size}, {from} to {to})', { n, rows, size: fmtSize(f.missing_bytes), from: fmtDay(f.oldest_missing_ms), to: fmtDay(f.newest_missing_ms) }),
      all_missing: t('admin_config.folder_all_missing', 'the folder is there but none of its {rows} recordings are ({size}). If it is a drive or network share that is not connected, connect it first.', { rows, size: fmtSize(f.missing_bytes) }),
      not_found: f.rows ? t('admin_config.folder_not_found_rows', 'folder not found ({rows} recordings in the database). If the drive is not connected or the folder moved, fix its path above and save first.', { rows }) : t('admin_config.folder_not_found', 'folder not found'),
      not_configured: t('admin_config.folder_not_configured', 'no folder of this name in the settings any more; {n} of {rows} are not in the data folder either. If it moved, add it again above.', { n, rows }),
    }[f.state] || f.state;
    let h = '<div class="ac-mfolder"><b>' + esc(f.label) + '</b> <span class="ac-path-i">' + esc(f.path) + '</span><div class="hint' + (f.needs_confirm ? ' warn' : '') + '">' + esc(what) + '</div>';
    if (f.not_mounted && f.missing) h += '<p class="hint warn">' + t('admin_config.drive_mounted_q', '<b>Is the drive mounted?</b> This folder looks like the mount point of a drive that is not mounted: mount it and restart AmberSight. If this folder is meant to be on the system disk, tick it to remove the missing ones.') + '</p>';
    if (f.unknown) h += '<div class="hint">' + t('admin_config.n_not_looked_for', '{n} could not be looked for and are kept.', { n: f.unknown.toLocaleString() }) + '</div>';
    if (f.case_differs) h += '<div class="hint warn">' + t('admin_config.n_other_capitals', '{n} are there under a name with other capitals (a folder renamed?) and are kept.', { n: f.case_differs.toLocaleString() }) + '</div>';
    if (f.needs_confirm) {
      h += '<div class="hint warn">' + t('admin_config.left_alone_unless_ticked', 'Left alone unless ticked: {why}. A wrong path or a drive that is not connected looks the same.', { why: esc(f.confirm_why.map(w => WHY[w] ? WHY[w](f) : w).join('; ')) }) + '</div>' +
        swHtml('data-minc="' + esc(f.name) + '"', !!missInclude[f.name], t('admin_config.remove_these_too', 'Remove these {n} too', { n }), t('admin_config.only_if_gone', 'Only if they are gone for good.'));
    }
    return h + '</div>';
  }
  function missCardHtml() {
    let h = '<div class="ac-card-t">' + t('admin_config.files_gone_title', 'Recordings whose files are gone') + '</div>' +
      '<div class="hint">' + t('admin_config.files_gone_desc', 'Finds recordings the database lists whose files are no longer on disk (a folder emptied, a drive replaced, a database brought from another computer) and removes them from the database, after backing it up. No file is deleted. If a folder moved, set its new path above and save first.') + '</div>';
    if (missErr) return h + '<p class="hint warn">' + esc(missErr) + '</p>';
    if (!miss) return h + '<p class="hint">' + t('admin_config.loading', 'Loading...') + '</p>';
    const j = miss.job, sc = miss.scan, ap = miss.applied;
    const running = ['scanning', 'applying'].includes(j.state);
    if (running) {
      h += '<div class="ac-meterrow"><div class="ac-meter"><i style="width:' + (j.total ? Math.min(100, j.done / j.total * 100).toFixed(1) : 0) + '%"></i></div><span class="hint">' +
        (j.total ? t('admin_config.phase_progress', '{phase}: {done} of {total}', { phase: esc(MISS_PHASE[j.phase] || t('admin_config.starting', 'Starting')), done: j.done.toLocaleString(), total: j.total.toLocaleString() }) : t('admin_config.phase_dots', '{phase}...', { phase: esc(MISS_PHASE[j.phase] || t('admin_config.starting', 'Starting')) })) + '</span></div>';
      return h;
    }
    if (j.state === 'failed') h += '<p class="hint warn">' + esc(j.error || t('admin_config.failed', 'failed')) + '</p>';
    if (ap && !sc) {
      h += '<p>' + t('admin_config.removed_n', 'Removed <b>{n}</b> recordings ({size}) from the database.', { n: ap.removed.toLocaleString(), size: fmtSize(ap.removed_bytes) }) +
        (ap.events_marked ? ' ' + t('admin_config.events_no_video', '{n} events have no video to play: they stay in the list, marked No clip.', { n: ap.events_marked.toLocaleString() }) : '') +
        (ap.found_again ? ' ' + t('admin_config.found_again', '{n} were found again and kept.', { n: ap.found_again.toLocaleString() }) : '') +
        (ap.moved ? ' ' + t('admin_config.had_moved', '{n} had moved to another folder and were kept.', { n: ap.moved.toLocaleString() }) : '') +
        (ap.case_differs ? ' ' + t('admin_config.under_other_capitals', '{n} are there under other capitals and were kept.', { n: ap.case_differs.toLocaleString() }) : '') +
        (ap.skipped_folders.length ? ' ' + t('admin_config.left_alone_list', 'Left alone: {list}.', { list: esc(ap.skipped_folders.join(', ')) }) : '') + '</p>' +
        (ap.backup ? '<div class="hint">' + t('admin_config.db_before_this', 'The database before this is in {path}. It is one of the backups kept, so newer ones replace it in time; putting it back loses the events recorded since.', { path: '<span class="ac-path-i">' + esc(ap.backup) + '</span>' }) + '</div>' : '');
    }
    if (sc) {
      const shown = sc.folders.filter(f => f.missing || f.state !== 'ok' || f.unknown);
      h += '<p>' + (sc.missing ? t('admin_config.n_of_rows_missing', '<b>{n}</b> of {rows} recordings are missing ({size}).', { n: sc.missing.toLocaleString(), rows: sc.rows.toLocaleString(), size: fmtSize(sc.missing_bytes) }) : t('admin_config.all_rows_there', 'All {rows} recordings are there.', { rows: sc.rows.toLocaleString() })) +
        ' <span class="hint">' + t('admin_config.checked_at', 'Checked {time}.', { time: new Date(sc.made_ms).toLocaleTimeString() }) + '</span></p>' + shown.map(folderLine).join('');
      if (sc.events_no_video || sc.events_no_video_all || sc.protected_no_video) {
        const a = sc.events_no_video_all.toLocaleString(), n = sc.events_no_video.toLocaleString();
        const lead = sc.events_no_video_all === sc.events_no_video ? t('admin_config.no_video_same', "{n} events would have no video to play (none left, or older than what would be their camera's oldest recording): they stay in the event list with their pictures, marked No clip.", { n })
          : sc.events_no_video ? t('admin_config.no_video_some', "{n} events would have no video to play ({a} with the folders above ticked) (none left, or older than what would be their camera's oldest recording): they stay in the event list with their pictures, marked No clip.", { n, a })
          : t('admin_config.no_video_ticked', "With the folders above ticked, {a} events would have no video to play (none left, or older than what would be their camera's oldest recording): they stay in the event list with their pictures, marked No clip.", { a });
        h += '<div class="hint">' + (sc.events_no_video_all ? lead + ' ' : '') +
          (sc.protected_no_video ? t('admin_config.protected_no_video', '{n} protected events would have none either; they are not changed.', { n: sc.protected_no_video.toLocaleString() }) + ' ' : '') + '</div>';
      }
      const count = sc.folders.reduce((a, f) => a + (!f.needs_confirm || missInclude[f.name] ? f.missing : 0), 0);
      if (sc.stale) h += '<p class="hint warn">' + esc(sc.stale.charAt(0).toUpperCase() + sc.stale.slice(1)) + '.</p>';
      else if (count) h += '<div class="ac-row"><span class="ac-sp"></span><button class="btn danger" type="button" data-mapply>' + t('admin_config.remove_n_from_db', 'Remove {n} from the database', { n: count.toLocaleString() }) + '</button></div>';
    }
    return h + '<div class="ac-row"><button class="btn" type="button" data-mscan' + (missBusy ? ' disabled' : '') + '>' + (sc ? t('admin_config.check_again', 'Check again') : t('admin_config.check_now', 'Check now')) + '</button></div>';
  }
  function paintMissing() {
    const note = el.querySelector('[data-missnote]');
    if (note) note.innerHTML = missNoteHtml();
    const card = el.querySelector('[data-missing]');
    if (!card) return;
    card.innerHTML = missCardHtml();
    const scanBtn = card.querySelector('[data-mscan]');
    if (scanBtn) scanBtn.onclick = async () => {
      missBusy = true; scanBtn.disabled = true;
      try { await api('/api/storage/missing/scan', { json: {} }); } catch (e) { toast(errText(e), { kind: 'err' }); }
      missBusy = false; loadMissing();
    };
    card.querySelectorAll('[data-minc]').forEach(inp => { inp.onchange = () => { missInclude[inp.dataset.minc] = inp.checked; paintMissing(); }; });
    const applyBtn = card.querySelector('[data-mapply]');
    if (applyBtn) applyBtn.onclick = async () => {
      const sc = miss.scan;
      const include = sc.folders.filter(f => f.needs_confirm && missInclude[f.name]).map(f => f.name);
      const count = sc.folders.reduce((a, f) => a + (!f.needs_confirm || missInclude[f.name] ? f.missing : 0), 0);
      const ticked = sc.folders.filter(f => f.needs_confirm && missInclude[f.name]);
      const named = ticked.length ? ' ' + t('admin_config.including_list', 'Including {list}: make sure these are gone for good.', { list: ticked.map(f => f.label + ' (' + f.confirm_why.map(w => WHY[w] ? WHY[w](f) : w).join('; ') + ')').join(', ') }) : '';
      if (!(await confirmDialog({ title: t('admin_config.remove_n', 'Remove {count} recordings from the database?', { count: count.toLocaleString() }), body: t('admin_config.remove_body', 'Their files are already gone; nothing is deleted from disk.{named} The database is backed up first; putting that backup back undoes this but loses events recorded since. Recording carries on meanwhile.', { named }), ok: t('admin_config.remove_ok', 'Remove'), danger: true }))) return;
      applyBtn.disabled = true;
      try { await api('/api/storage/missing/apply', { json: { scan_id: sc.id, include } }); } catch (e) { toast(errText(e), { kind: 'err' }); }
      loadMissing();
    };
  }
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
    const msg = {
      private: t('admin_config.state_private', 'Private: only administrators and AmberSight can open it.'),
      open: t('admin_config.state_open', 'Open: other accounts on this computer can open it.'),
      applying: t('admin_config.state_applying', 'Being applied in the background; recording carries on.'),
      skipped: t('admin_config.state_skipped', 'Left as it is: {detail}.', { detail: e.detail }),
      refused: t('admin_config.state_refused', 'Left as it is: {detail}.', { detail: e.detail || t('admin_config.not_a_folder_may_change', 'not a folder AmberSight may change') }),
      failed: t('admin_config.state_failed', 'Could not be made private: {detail}', { detail: e.detail }),
      unsupported: t('admin_config.state_unsupported', 'Cannot be made private on this kind of drive.'),
      missing: t('admin_config.state_missing', 'The folder is not there yet.'),
      unknown: t('admin_config.state_unknown', 'No answer from this folder yet: the drive or network share may be off or slow.'),
    }[e.state] || '';
    return msg ? '<div class="hint">' + esc(msg) + '</div>' : '';
  }
  const PRIV_TEXT = {
    private: t('admin_config.priv_private', 'Private now: only administrators and AmberSight can open it.'),
    open: t('admin_config.priv_open', 'Open: other accounts on this computer can open it.'),
    applying: t('admin_config.priv_applying', 'Being applied in the background. A folder with many recordings can take several minutes; recording carries on.'),
    missing: t('admin_config.priv_missing', 'The folder is not there yet.'),
    unsupported: t('admin_config.priv_unsupported', 'Cannot be made private on this kind of drive.'),
    unknown: t('admin_config.priv_unknown', 'No answer from this folder yet: the drive or network share may be off or slow. You can still change or remove it here.'),
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
      if (pr && pr.state === 'refused') return t('admin_config.refused', 'Refused: {detail}', { detail: pr.detail || t('admin_config.choose_folder_on_drive', 'choose a folder on that drive, not the whole drive') });
      if (pr && pr.state === 'unsupported') return t('admin_config.unsupported', 'Cannot be made private on this kind of drive.');
      return sv ? t('admin_config.applied_when_save_old', 'Applied when you save. The old location is left as it is.') : t('admin_config.applied_when_save', 'Applied when you save.');
    }
    const s = priv[f.name];
    if (!s) return privLoaded ? t('admin_config.applied_when_save', 'Applied when you save.') : t('admin_config.checking', 'Checking...');
    if (s.state === 'failed') return t('admin_config.could_not_apply', 'Could not apply: {detail}', { detail: s.detail });
    if (s.state === 'refused') return t('admin_config.refused', 'Refused: {detail}', { detail: s.detail || t('admin_config.choose_folder_on_drive', 'choose a folder on that drive, not the whole drive') });
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
    return confirmDialog({ title: t('admin_config.keep_private_title', 'Keep {name} private?', { name: f.name }), body: t('admin_config.keep_private_body', 'This folder already holds files. Only administrators and AmberSight will be able to open it afterwards, so File Explorer, a network share or other programs that use it may stop working. On a big folder this can take several minutes in the background.'), ok: t('admin_config.make_private_ok', 'Make private'), danger: true });
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
      const [s, stt] = await Promise.all([api('/api/settings'), api('/api/status'), refreshCameras().catch(() => {})]);
      saved = norm(s.storage); draft = clone(saved); st = stt.storage || {}; dataDir = stt.data_dir || String(s.storage.data_dir);
      try { drives = (await api('/api/drives')).drives || []; } catch (e) { drives = []; }
      // The page is drawn first. What the folders say comes when it comes: a drive that does not
      // answer must not keep the page on Loading (or the folder from being removed).
      paint();
      if (!retention) loadPrivacy().then(() => { if (el.isConnected) { repaintQuietly(); watchPrivacy(); } });
      loadMissing();
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
    if (auto && !(info && info.auto && info.drive_total)) return '<div class="ac-meter"><i style="width:0"></i></div><span class="hint">' + t('admin_config.auto_worked_out', 'automatic: worked out from the drive once saved') + '</span>';
    const capB = auto ? info.cap_bytes || 0 : capMb * 1048576;
    const pc = capB ? Math.min(100, used / capB * 100) : 100;
    const same = info && !!info.auto === auto && (auto || info.cap_bytes === capB);
    const lvl = same ? ({ crit: ' crit', warn: ' warn' }[info.level] || '') : '';
    const note = same && info.label ? '; ' + (rolls ? t('admin_config.label_older_move', '{label}, older ones move to the archive', { label: info.label.toLowerCase() }) : info.label.toLowerCase()) : (rolls && pc >= 95 ? '; ' + t('admin_config.full_oldest_move', 'full, the oldest move to the archive') : '');
    return '<div class="ac-meter' + lvl + '"><i style="width:' + pc.toFixed(1) + '%"></i></div><span class="hint">' + (auto ? t('admin_config.pct_of_about', '{pc}% of about {size}', { pc: pc.toFixed(0), size: fmtSize(capB) }) : t('admin_config.pct_of', '{pc}% of {size}', { pc: pc.toFixed(0), size: fmtSize(capB) })) +
      (auto ? ' ' + t('admin_config.what_drive_can_spare_paren', '(what the drive can spare now)') : '') + esc(note) + '</span>';
  };
  const pctFree = () => (draft.min_free_percent != null ? draft.min_free_percent : 10);
  // Space for one folder: Automatic, or a fixed size in GB. Stored as MB.
  const capField = (box, mb) =>
    '<label class="fld"><span class="fld-l">' + t('admin_config.space_for_recordings', 'Space for recordings') + '</span><select data-capmode><option value="auto"' + (mb ? '' : ' selected') + '>' + t('admin_config.automatic_spare', 'Automatic (what the drive can spare)') + '</option><option value="fixed"' + (mb ? ' selected' : '') + '>' + t('admin_config.fixed_size', 'Fixed size') + '</option></select></label>' +
    (mb ? fld(t('admin_config.size', 'Size'), 'data-gb="' + box + '" min="0.1" step="0.5" value="' + mbToGb(mb) + '"', 'GB') : '');
  // "Holds about N days at X/day" for a saved folder ("" = data folder).
  const fits = (name) => {
    const x = plan && name != null && (plan.folders || []).find(p => p.name === name);
    if (!x || !x.bytes_per_day) return '';
    const d = x.days_fit;
    return '<div class="hint">' + t('admin_config.recording_about', 'Recording about {size}/day here{inflow, select, yes { (including what other folders move in)} other {}}{room, select, yes {: room for about <b>{days} days</b>} other {}}.', { size: fmtSize(x.bytes_per_day), inflow: x.inflow_per_day ? 'yes' : 'no', room: d != null ? 'yes' : 'no', days: d != null ? (d >= 10 ? Math.round(d) : d.toFixed(1)) : '' }) + '</div>';
  };
  const tiles = () => '<div class="ac-tiles">' +
    '<div class="ac-tile"><b>' + fmtSize(st.bytes || 0) + '</b><span>' + t('admin_config.used_in_data_folder', 'used in data folder') + '</span></div>' +
    '<div class="ac-tile"><b>' + (st.segments || 0) + '</b><span>' + t('admin_config.recording_files', 'recording files') + '</span></div>' +
    '<div class="ac-tile"><b>' + (st.clips || 0) + '</b><span>' + t('admin_config.saved_clips', 'saved clips') + '</span></div>' +
    '<div class="ac-tile"><b>' + fmtSize(st.indexed_bytes || 0) + '</b><span>' + t('admin_config.indexed_recordings', 'indexed recordings') + '</span></div></div>' +
    (st.kept_bookmarks && st.kept_bookmarks.count ? '<div class="hint">' + t('admin_config.kept_bookmarks_hint', '{count, plural, one {# kept bookmark} other {# kept bookmarks}} hold {size} of recordings back from cleanup (2 min either side of each). Only a full drive deletes them, as a last resort.', { count: st.kept_bookmarks.count, size: fmtSize(st.kept_bookmarks.bytes || 0) }) + '</div>' : '');

  function paintRetention() {
    const usage = {}, capInfo = {}; (st.folders || []).forEach(f => { usage[f.name] = f.bytes; capInfo[f.name] = f.cap; });
    const hasArchive = saved.folders.some(f => f.archive);
    const auto = !draft.max_mb;
    const dc = st.data_cap || {};
    let h = lead(t('admin_config.how_long_kept', 'How long recordings are kept, and how much space they may use.'), t('admin_config.default_uses_spare', 'The default uses what the drive can spare. Change it only if you want a fixed number of days.')) +
      how(t('admin_config.over_the_limit', 'Over the limit, the oldest recordings move to the archive folder if one is set, otherwise they are deleted. Events you keep forever are never removed.')) + '<div data-missnote>' + missNoteHtml() + '</div>' + tiles() +
      '<div class="ac-card"><div class="ac-card-t">' + t('admin_config.how_long_to_keep', 'How long to keep') + '</div>' +
      fld(t('admin_config.keep_recordings_for', 'Keep recordings for'), 'data-k="retention_days" min="1" value="' + draft.retention_days + '"', t('admin_config.unit_days', 'days')) +
      '</div><div class="ac-card"><div class="ac-card-t">' + t('admin_config.how_much_space', 'How much space') + '</div>' +
      '<div class="hint">' + t('admin_config.data_folder_hint', 'Data folder: {path}', { path: '<span class="ac-path-i">' + esc(dataDir) + '</span>' }) + '</div>' +
      '<div class="ac-meterrow">' + bar(st.data_cap ? st.data_cap.used_bytes : st.bytes || 0, draft.max_mb, hasArchive, st.data_cap) + '</div>' + fits('') +
      '<div data-capbox="data">' + capField('data', draft.max_mb) + '</div>' +
      (auto ? fld(t('admin_config.always_keep_disk_free', 'Always keep this much of the disk free'), 'data-k="min_free_percent" min="0" max="90" value="' + pctFree() + '"', t('admin_config.pct_of_disk', '% of the disk'),
        dc.auto && dc.drive_total ? t('admin_config.now_about', 'Now about {cap} for recordings: {used} used, {free} free of {total} on the drive. Other files on the drive make it smaller.', { cap: fmtSize(dc.cap_bytes || 0), used: fmtSize(dc.used_bytes || 0), free: fmtSize(dc.drive_free || 0), total: fmtSize(dc.drive_total) }) : t('admin_config.size_worked_out', 'The size is worked out once saved.')) : '') +
      '</div>';
    if (draft.folders.length) {
      h += '<div class="ac-card"><div class="ac-card-t">' + t('admin_config.space_in_extra', 'Space in extra folders') + '</div>';
      draft.folders.forEach((f, i) => {
        const n = state.cams.filter(c => c.folder === (saved.folders[i] ? saved.folders[i].name : f.name)).length;
        h += '<div class="ac-folder" data-i="' + i + '"><div class="ac-card-t">' + esc(f.name) + (f.archive ? ' <span class="badge info">' + t('admin_config.archive_lc', 'archive') + '</span>' : '') + '</div>' +
          '<div class="hint">' + t('admin_config.used_by', '{size} used - {who}', { size: fmtSize(usage[f.name] || 0), who: whoUses(f, n) }) + '</div>' +
          '<div class="ac-meterrow">' + bar(usage[f.name] || 0, f.max_mb, hasArchive && !f.archive, saved.folders[i] && saved.folders[i].name === f.name ? capInfo[f.name] : null) + '</div>' + fits(saved.folders[i] ? saved.folders[i].name : null) +
          '<div data-capbox="' + i + '">' + capField(String(i), f.max_mb) + '</div></div>';
      });
      h += '<div class="hint">' + t('admin_config.add_rename_remove', 'Add, rename or remove folders under Drives &amp; folders.') + '</div></div>';
    }
    h += '<details class="ac-more"' + (more ? ' open' : '') + '><summary>' + t('admin_config.more_settings', 'More settings') + '</summary>' +
      fld(t('admin_config.keep_event_footage', 'Keep event footage first for'), 'data-k="event_keep_days" min="0" max="3650" value="' + draft.event_keep_days + '"', t('admin_config.unit_days', 'days'), t('admin_config.keep_event_footage_desc', '0 = off. When a folder is full, footage with no events in it is deleted before footage around events from this many days, so alerts stay viewable longer than the rest. It never keeps anything past "Keep recordings for".')) +
      fld(t('admin_config.segment_length', 'Segment length'), 'data-k="segment_seconds" min="1" value="' + draft.segment_seconds + '"', t('admin_config.unit_seconds', 'seconds'), t('admin_config.segment_desc', 'Recordings are written in pieces this long.')) +
      swHtml('data-subcopy', draft.sub_copy, t('admin_config.record_sub_stream', 'Also record the sub stream of continuous cameras'), t('admin_config.record_sub_stream_desc', 'A small second copy (about a tenth more space) so phones with Data saver on play recordings with little data. Their streams reconnect when this changes.')) +
      '</details>' + pageBar();
    el.innerHTML = '<div class="ac">' + h + '</div>';
    bindCommon();
    bindRetention();
  }

  function paintDrives() {
    const usage = {}, capInfo = {}; (st.folders || []).forEach(f => { usage[f.name] = f.bytes; capInfo[f.name] = f.cap; });
    const camsIn = (n) => state.cams.filter(c => c.folder === n).length;
    let h = lead(t('admin_config.where_stored', 'Where recordings are stored.'), t('admin_config.most_homes_default', 'Most homes only need the default folder; add a drive when it fills up.')) +
      how(t('admin_config.cameras_without_folder', 'Cameras without a folder of their own record into the data folder; a camera can pick another folder in its own settings. The archive folder receives the oldest recordings pushed out of the others. How much space each may use is set under Storage &amp; retention.')) +
      '<div data-missnote>' + missNoteHtml() + '</div>' +
      '<div class="ac-card"><div class="ac-card-t">' + t('admin_config.data_folder', 'Data folder') + '</div><div class="ac-path">' + esc(dataDir) + '</div><div class="hint">' + t('admin_config.size_used_changing', '{size} used. Changing this folder needs a config edit and a restart.', { size: fmtSize(st.bytes || 0) }) + '</div>' + endLine(ends.data) + '</div>' +
      (ends.program && ends.program.state !== 'na' ? '<div class="ac-card"><div class="ac-card-t">' + t('admin_config.program_folder', 'Program folder') + '</div><div class="ac-path">' + esc(ends.program.path) + '</div>' + endLine(ends.program) + '</div>' : '') +
      '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">' + t('admin_config.extra_folders', 'Extra folders') + '</div><span class="ac-sp"></span><button class="btn" type="button" data-addf>' + ic('plus') + ' ' + t('admin_config.add_folder', 'Add folder') + '</button></div>' +
      '<div class="hint">' + t('admin_config.other_drives', 'Other drives or network shares.') + '</div>';
    if (!draft.folders.length) h += '<p class="hint">' + t('admin_config.none_everything', 'None. Everything records into the data folder.') + '</p>';
    draft.folders.forEach((f, i) => {
      const n = camsIn(saved.folders[i] ? saved.folders[i].name : f.name);
      h += '<div class="ac-folder" data-i="' + i + '"><div class="ac-grid4">' +
        '<label class="fld"><span class="fld-l">' + t('admin_config.name', 'Name') + '</span><input data-f="name" value="' + esc(f.name) + '"></label>' +
        '<label class="fld ac-wide"><span class="fld-l">' + t('admin_config.path', 'Path') + '</span><input data-f="path" value="' + esc(f.path) + '" placeholder="' + t('admin_config.path_placeholder', '/mnt/nas/cameras or D:\\Cameras') + '"></label>' +
        '</div>' +
        (drives.length ? '<div class="ac-row"><label class="ac-inl">' + t('admin_config.pick_a_drive', 'Pick a drive') + ' <select data-drive><option value="">' + t('admin_config.choose', 'choose...') + '</option>' +
          drives.map(dv => '<option value="' + esc(dv.path) + '">' + t('admin_config.drive_option', '{path} - {free} free of {total}', { path: esc(dv.path), free: fmtSize(dv.free), total: fmtSize(dv.total) }) + '</option>').join('') +
          '</select></label><span class="hint ac-inl">' + t('admin_config.fills_in_path', 'fills in the path with an AmberSight folder on that drive') + '</span></div>' : '') +
        swHtml('data-f="archive"', f.archive, t('admin_config.archive_folder', 'Archive folder'), t('admin_config.archive_folder_desc', 'Receives recordings pushed out of the other folders. Only one folder can be the archive.')) +
        swHtml('data-f="private"', shownPrivate(f, i), t('admin_config.keep_folder_private', 'Keep this folder private'), t('admin_config.keep_folder_private_desc', 'Only administrators and AmberSight can open it, so browsing it in File Explorer or over a network share may stop working.'),
          priv[f.name] && priv[f.name].state === 'refused' && f.private !== true) +
        '<div class="hint" data-privline="' + i + '">' + esc(privLine(f, i)) + '</div>' +
        '<div class="ac-row"><span class="hint ac-inl">' + t('admin_config.used_by', '{size} used - {who}', { size: fmtSize(usage[f.name] || 0), who: whoUses(f, n) }) + '</span><span class="ac-sp"></span><button class="btn danger" type="button" data-rm>' + t('admin_config.remove', 'Remove') + '</button></div></div>';
    });
    h += '</div><div class="ac-card" data-missing>' + missCardHtml() + '</div>' + pageBar();
    el.innerHTML = '<div class="ac">' + h + '</div>';
    bindCommon();
    bindDrives();
    paintMissing();
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
              if (!on && !(await confirmDialog({ title: t('admin_config.open_again', 'Open {name} again?', { name: esc(f.name) }), body: t('admin_config.folder_back', 'The folder goes back to the permissions of the folder it is in, so other accounts on this computer can open it again (on Windows, whatever its parent allows; on Linux and macOS, mode 755).'), ok: t('admin_config.open_it_again', 'Open it again'), danger: true }))) throw { cancelled: true };
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
        if (!(await confirmDialog({ title: t('admin_config.remove_folder_q', 'Remove folder {name}?', { name: f.name }), body: (n ? t('admin_config.cams_use_folder', '{n, plural, one {# camera uses} other {# cameras use}} this folder and will record into the data folder instead.', { n }) + ' ' : '') + t('admin_config.recordings_stay', 'Recordings already in it stay on the drive but are no longer managed by AmberSight. Takes effect when you save.'), ok: t('admin_config.remove_folder', 'Remove folder'), danger: true }))) return;
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
    return '<div class="dl-login"><label>' + t('admin_config.user_name', 'User name') + ' <input data-lu="' + i + '" value="' + esc(l.username) + '" autocomplete="off" autocapitalize="none" spellcheck="false" placeholder="' + (i ? t('admin_config.optional_second_login', 'optional second login') : t('admin_config.example_admin', 'e.g. admin')) + '"></label>' +
      '<label>' + t('admin_config.password', 'Password') + ' <span class="dl-pw"><input type="password" data-lp="' + i + '" value="' + esc(l.password) + '" autocomplete="new-password" placeholder="' + (l.saved ? t('admin_config.saved_leave_blank', 'saved (leave blank to keep)') : '') + '">' +
      '<button type="button" class="btn dl-eye" data-eye="' + i + '" aria-pressed="false" title="' + t('admin_config.show_hide_password', 'Show or hide the password') + '">' + t('admin_config.show', 'Show') + '</button></span></label></div>';
  }
  const sw = (attr, on, label, tip) => '<label class="sw"' + (tip ? ' title="' + esc(tip) + '"' : '') + '><input type="checkbox" role="switch" ' + attr + (on ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span><span class="sw-text"><b>' + label + '</b></span></label>';
  function paint() {
    let h = lead(t('admin_config.looks_for_cameras', 'Looks for cameras on your network and adds the ones you pick.')) +
      (added ? notice('ok', t('admin_config.added_notice', '{added, plural, one {Added # camera. You can rename them later in Cameras.} other {Added # cameras. You can rename them later in Cameras.}}', { added }), '<button class="btn primary" type="button" data-added-next>' + t('admin_config.next_setup_step', 'Next setup step') + '</button><a class="btn" href="#live">' + t('admin_config.see_them_on_live', 'See them on Live') + '</a>') : '') +
      '<div class="ac-card dl-first"><p class="dl-lead">' + t('admin_config.use_camera_app_login', "Use the user name and password you set in the camera's own app.") + '</p>' +
      loginRow(0) +
      (err ? '<div class="ac-err">' + esc(err) + '</div>' : '') +
      '<div class="ac-row ac-scan"><button class="primary dl-scan" data-scan' + (scanning ? ' disabled' : '') + '>' + (scanning ? t('admin_config.scanning', 'Scanning...') : t('admin_config.scan_for_cameras', 'Scan for cameras')) + '</button></div>' +
      '<details class="dl-more" data-more' + (moreOpen ? ' open' : '') + '><summary>' + t('admin_config.more_options', 'More options') + '</summary>' +
      '<div class="dl-moreb"><div class="hint">' + t('admin_config.second_login', 'A second login to try, if some cameras use another one. Each camera is tried with these and stops at the first that works. Many cameras lock a login after a few wrong passwords, so only logins you expect to work belong here.') + '</div>' +
      loginRow(1) +
      '<label class="ac-inl ac-grow">' + t('admin_config.other_subnets', 'Other subnets') + ' <input data-subnet value="' + esc(rangesTyped !== null ? rangesTyped : (W.ranges || []).join(', ')) + '" placeholder="' + t('admin_config.subnets_placeholder', 'optional, e.g. 192.168.1.0/24, 10.0.20.0/24') + '"></label>' +
      '<div class="hint">' + t('admin_config.own_network_scanned', 'Your own network is always scanned. Cameras on other networks answer only when listed here, separated by commas (up to sixteen). They are remembered.') + '</div>' +
      '<label class="ac-inl">' + t('admin_config.wait_secs', 'Wait {select} seconds for answers', { select: '<select data-secs><option>3</option><option>5</option><option>8</option><option>15</option></select>' }) + '</label>' +
      sw('data-watch', W.watch, t('admin_config.check_every_hour', 'Check every hour for new cameras'), t('admin_config.check_every_hour_tip', 'Tells you (header and notifications) when a new camera shows up, here and on the listed subnets')) +
      (W.watch && W.last_check ? '<div class="hint">' + t('admin_config.last_checked', 'Last checked {time}.', { time: esc(new Date(W.last_check * 1000).toLocaleString()) }) + '</div>' : '') +
      sw('data-remember', remember, t('admin_config.remember_logins', 'Remember these logins on this server'), t('admin_config.remember_logins_tip', 'Kept like camera logins, so the next scan needs no typing')) +
      '</div></details></div>';
    const fresh = W.found || [], ign = W.ignored || [];
    if (fresh.length || ign.length) {
      h += '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">' + (fresh.length ? t('admin_config.new_on_network_n', 'New on your network ({n})', { n: fresh.length }) : t('admin_config.new_on_network', 'New on your network')) + '</div><span class="ac-sp"></span>' +
        (ign.length ? '<button class="btn" data-showign>' + (showIgn ? t('admin_config.hide_ignored', 'Hide ignored') : t('admin_config.n_ignored', '{n} ignored', { n: ign.length })) + '</button>' : '') + '</div>';
      if (fresh.length) {
        h += '<div class="hint">' + t('admin_config.found_by_hourly', 'Found by the hourly check and not added yet. Scan network signs in to the ONVIF ones so you can add them; RTSP-only ones are added by hand with their stream address.') + '</div>' +
          fresh.map(f => '<div class="ac-row nc-row"><div class="ac-devname"><b>' + esc(f.name || (f.kind === 'rtsp' ? t('admin_config.answers_on_rtsp', 'Answers on RTSP') : t('admin_config.camera', 'Camera'))) + '</b><span class="hint">' + esc(f.ip) + ' · ' + (f.kind === 'rtsp' ? t('admin_config.rtsp_only', 'RTSP only') : t('admin_config.onvif', 'ONVIF')) +
            ' · ' + t('admin_config.first_seen', 'first seen {time}', { time: esc(new Date(f.first_seen * 1000).toLocaleString()) }) + '</span></div><span class="ac-sp"></span><button class="btn" data-ign="' + esc(f.ip) + '" title="' + t('admin_config.ignore_tip', 'Not a camera to add: stop reporting it') + '">' + t('admin_config.ignore', 'Ignore') + '</button></div>').join('');
      } else h += '<div class="hint">' + t('admin_config.nothing_new', 'Nothing new.') + '</div>';
      if (showIgn && ign.length) h += '<div class="ac-sub">' + t('admin_config.ignored', 'Ignored') + '</div>' + ign.map(ip => '<div class="ac-row nc-row"><span>' + esc(ip) + '</span><span class="ac-sp"></span><button class="btn" data-unign="' + esc(ip) + '">' + t('admin_config.stop_ignoring', 'Stop ignoring') + '</button></div>').join('');
      h += '</div>';
    }
    if (res) {
      const list = res.devices || [];
      const picked = Object.values(devs).flatMap(d => (d.cams || []).filter(c => c.pick && !c.added));
      h += '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">' + t('admin_config.devices_found', '{n, plural, one {# device found} other {# devices found}}', { n: list.length }) + '</div><span class="ac-sp"></span>' +
        '<button class="primary" data-addsel' + (picked.length ? '' : ' disabled') + '>' + t('admin_config.add_selected', 'Add selected ({n})', { n: picked.length }) + '</button></div>';
      if (!list.length) h += '<p class="hint">' + t('admin_config.nothing_answered', 'Nothing answered. Cameras on another subnet need that range listed above.') + '</p>';
      list.forEach((d, i) => {
        const xa = (d.xaddrs && d.xaddrs[0]) || '';
        const v = devs[xa] || {};
        const isCfg = configured(d);
        const inf = (v.ins && v.ins.info) || {};
        const title = esc(inf.model ? [inf.manufacturer, inf.model].filter(Boolean).join(' ') : (d.name || d.hardware || d.ip));
        let status;
        if (isCfg) status = '<span class="badge on dot">' + t('admin_config.already_added', 'already added') + '</span>';
        else if (v.state === 'trying') status = '<span class="badge info dot">' + t('admin_config.signing_in', 'signing in...') + '</span>';
        else if (v.state === 'ok') status = '<span class="badge on dot">' + t('admin_config.signed_in_as', 'signed in as {user}', { user: esc(v.username) }) + '</span>';
        else if (v.state === 'auth') status = '<span class="badge off dot">' + t('admin_config.login_did_not_work', 'login did not work') + '</span>';
        else if (v.state === 'error') status = '<span class="badge off dot">' + t('admin_config.could_not_read','could not read') + '</span>';
        else status = '<span class="badge">' + t('admin_config.new','new') + '</span>';
        const hit = isCfg ? state.cams.find(c => hostOf(c.url) === d.ip || hostOf(c.sub_url || '') === d.ip) : null;
        const snap = hit ? '/api/snapshot/' + hit.id : (v.pic || '');
        const nOk = (v.cams || []).filter(c => c.pick && !c.added).length;
        const addBtn = v.state === 'ok' && !isCfg && (v.cams || []).some(c => !c.added) ? '<button class="primary" data-addone' + (nOk ? '' : ' disabled') + '>' + t('admin_config.add','Add') + '</button>' : '';
        h += '<div class="ac-dev dl-card' + (isCfg ? ' dl-have' : '') + '" data-xa="' + esc(xa) + '"><div class="dl-head"><div class="dl-snap">' +
          (snap ? '<img src="' + esc(snap) + '" alt="" loading="lazy">' : '') + '<svg class="ic" aria-hidden="true"><use href="#i-camera"/></svg></div>' +
          '<div class="ac-devname"><b>' + title + '</b><span class="hint">' + esc(d.ip) +
          (inf.model ? '' : (d.hardware && d.hardware !== d.name ? ' - ' + esc(d.hardware) : '')) + '</span>' + status + '</div><span class="ac-sp"></span>' + addBtn + '</div>';
        if (!isCfg && (v.state === 'auth' || v.state === 'error' || v.state === 'manual')) {
          h += '<div class="ac-ins">' + (v.msg ? '<div class="ac-err">' + esc(v.msg) + '</div>' : '') +
            '<div class="ac-grid4"><label>' + t('admin_config.username','Username') + ' <input data-u value="' + esc(v.u || '') + '" autocomplete="off"></label>' +
            '<label>' + t('admin_config.password','Password') + ' <input type="password" data-p value="' + esc(v.p || '') + '" autocomplete="new-password"></label>' +
            '<div class="ac-go"><button class="btn" data-try' + (xa ? '' : ' disabled') + '>' + t('admin_config.try','Try') + '</button></div></div></div>';
        }
        if (!isCfg && !v.state && xa) h += '<div class="ac-ins"><button class="btn" data-manual>' + t('admin_config.enter_login','Enter a login for this camera') + '</button></div>';
        if (v.state === 'ok') {
          if (v.note) h += '<div class="hint ac-note">' + esc(v.note) + '</div>';
          h += (v.cams || []).map((c, ci) => '<div class="ac-cand dl-cand" data-ci="' + ci + '">' +
            (c.added ? '<span class="badge on dot">' + t('admin_config.added_lc', 'added') + '</span>' : '<label class="asys-chk"><input type="checkbox" data-pick' + (c.pick ? ' checked' : '') + '></label>') +
            '<label class="dl-name">' + t('admin_config.name', 'Name') + ' <input data-cn value="' + esc(c.name) + '"' + (c.added ? ' disabled' : '') + '></label>' +
            '<div class="hint ac-url">' + esc((c.base.url || '').replace(/\/\/[^@/]*@/, '//')) + (c.base.sub_url ? ' ' + t('admin_config.plus_sub_stream', '(+ sub stream)') : '') + (c.base.connect === 'auto' ? t('admin_config.connect_auto_note', ". Connect with: Auto (Reolink port 9000 first, RTSP as the backup; change it in the camera''s Stream settings)") : '') + '</div></div>').join('');
        }
        h += '</div>';
      });
      if ((res.rtsp_only || []).length) {
        h += '<div class="ac-sub">' + t('admin_config.answered_rtsp_only', 'Answered on RTSP only (not ONVIF)') + '</div><div class="chips">' + res.rtsp_only.map(r => '<span class="badge">' + esc(r.ip) + ':' + r.port + '</span>').join('') + '</div>' +
          '<div class="hint">' + t('admin_config.add_by_hand','Add these by hand on the Cameras tab, with their stream address.') + '</div>';
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
    v.state = 'trying'; v.msg = ''; v.note = ''; paint();
    try {
      const r = await api('/api/discover/autologin', { json: { xaddr: xa, logins: list.map(l => ({ username: l.username, password: l.password || '' })) } });
      if (r.ok) {
        const typed = list.find(l => l.username === r.username);
        Object.assign(v, { state: 'ok', ins: r.inspection, username: r.username, password: typed ? typed.password : '', note: r.inspection.note || '' });
        const cands = (r.inspection.cameras && r.inspection.cameras.length) ? r.inspection.cameras : [r.inspection.suggested];
        v.cams = cands.filter(c => c && c.url).map(c => ({ base: c, name: c.name || 'Camera', pick: true, added: false }));
        if (!v.cams.length) { v.state = 'error'; v.msg = t('admin_config.no_stream','Signed in, but the camera reported no video stream.'); }
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
      inp.type = show ? 'text' : 'password'; b.textContent = show ? t('admin_config.hide', 'Hide') : t('admin_config.show', 'Show'); b.setAttribute('aria-pressed', show ? 'true' : 'false');
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
      if (ev.target.checked) toast(t('admin_config.checking_now_hourly', 'Checking now, then every hour'));
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
        catch (e) { err = t('admin_config.could_not_save_logins', 'Could not save the logins: {why}', { why: e.body ? tryMsg(e.body) : e.message }); }
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
      const go = () => { sync(); if (!v.u) { v.msg = t('admin_config.enter_username', 'Enter a username.'); paint(); return; } signIn(xa, [{ username: v.u, password: v.p || '' }]); };
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
          const body = Object.assign({ record: true, motion: true, enabled: true, audio: true }, c.base,
            { name: (c.name || '').trim() || c.base.name, username: v.username, password: v.password || '' });
          try { await addCameraConfig(body); c.added = true; ok++; }
          catch (e) { fail.push(body.name + ': ' + (e.body ? tryMsg(e.body) : e.message)); }
        }
      }
      if (ok) added += ok;
      paint();
      if (fail.length) msgDlg(t('admin_config.not_added', 'Not added: {list}', { list: fail.join('; ') }));
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

registerAdminPage({ id: 'profiles', title: t('admin_config.profiles_schedule', 'Profiles & schedule'), order: 50, render: profilesPage, leave: stopTimers });
registerAdminPage({ id: 'ai', title: t('admin_config.ai', 'AI'), order: 60, render: aiPage });
registerAdminPage({ id: 'retention', title: t('admin_config.storage_retention', 'Storage & retention'), order: 69, render: (el) => storagePage(el, 'retention') });
registerAdminPage({ id: 'storage', title: t('admin_config.drives_folders', 'Drives & folders'), order: 70, render: (el) => storagePage(el, 'drives') });
registerAdminPage({ id: 'discovery', title: t('admin_config.discovery', 'Discovery'), order: 80, render: discoveryPage });

// Shared with the other settings pages built the same way (admin_presence.js).
export { head, pageBar, wireBar, postSettings, guardAdmin };
