// settings.js - the Settings page: a left rail of sections, the content on the right.
// Settings and the old Admin pages are one place. Every section is a route:
//   #settings/<section>[/<sub>]      (e.g. #settings/detection/alerts, #settings/system/logs)
// The admin pages (admin*.js, status_page.js, ai_review.js) register through admin.js and are
// mounted here; the forms that used to be dialog panes (motion, storage, alerts, watchdog,
// object detection hook) are built below.
// POST /api/settings MERGES: sections or fields we leave out keep their stored value, so every
// form sends only its own fields.

import { t } from './i18n.js';
import { state, api, esc, toast, ic, errText, savedToast, confirmDialog, confirmLeave, hasUnsaved, clearUnsaved, liveSwitch, swHtml, fld, formBar, backLayer, overlaysChanged, afterOverlayClose, trapSheet } from './core.js';
import { refreshStatus } from './status.js';
import { isActive, showTab, hashPath } from './nav.js';
import { adminPage } from './admin.js';
import { mountCameraList } from './cameras.js';
import { mountGroups } from './groups.js';
import { mountServers } from './servers.js';
import { mountPushCard } from './push.js';
import { mountHttps } from './https_settings.js';
import { mountLangSettings } from './lang_ui.js';
import { mountShare } from './share_settings.js';
import { mountHomekit } from './homekit_settings.js';
import { mountDisplays } from './display_admin.js';
import { lead, more, how, notice, seg, tabAttrs, MOTION_PRESETS, tablistKeys } from './sp_kit.js';

/* ---------------------------------------------------------------- small form kit */

// Text field: label above, hint below. Numbers with a unit go through fld() (core.js).
const inp = (label, key, val, extra, hint) =>
  '<label class="fld"><span class="fld-l">' + label + '</span><span class="fld-in"><input data-k="' + key + '" value="' + esc(val == null ? '' : val) + '" ' + (extra || '') + '></span></label>' + (hint ? '<div class="hint fld-h">' + hint + '</div>' : '');
// Number with its unit next to it.
const num = (label, key, val, extra, unit, hint) => fld(label, 'data-k="' + key + '" value="' + esc(val == null ? '' : val) + '" ' + (extra || ''), unit, hint);
const slid = (label, key, min, max, step, val, unit) =>
  '<label class="fld"><span class="fld-l">' + label + ' <span class="rv" data-rv="' + key + '">' + val + unit + '</span></span><input type="range" data-k="' + key + '" data-unit="' + unit + '" min="' + min + '" max="' + max + '" step="' + step + '" value="' + val + '"></label>';
// On/off setting: a switch that saves the moment it is flipped (data-live).
const sw = (key, title, small, on) => swHtml('data-k="' + key + '" data-live', on, title, small);
const selectHtml = (label, key, opts, val) =>
  '<label class="fld"><span class="fld-l">' + label + '</span><select data-k="' + key + '">' + opts.map(([v, text]) => '<option value="' + v + '"' + (String(v) === String(val) ? ' selected' : '') + '>' + text + '</option>').join('') + '</select></label>';
const rd = (root, k) => { const e = root.querySelector('[data-k="' + k + '"]'); return e ? (e.type === 'checkbox' ? e.checked : e.value) : ''; };

// Time window "HH:MM-HH:MM": two time pickers and an on/off switch. Reads/writes the exact
// string the server expects; a value in any other shape is kept editable as raw text.
const TR_RE = /^\s*(\d{1,2}):(\d{2})\s*-\s*(\d{1,2}):(\d{2})\s*$/;
function trParse(v) {
  const m = TR_RE.exec(v || '');
  if (!m || +m[1] > 23 || +m[3] > 23 || +m[2] > 59 || +m[4] > 59) return null;
  const p = (h, mi) => String(h).padStart(2, '0') + ':' + mi;
  return [p(m[1], m[2]), p(m[3], m[4])];
}
function trHtml(key, val, title, small) {
  const win = trParse(val), raw = !!(val && val.trim() && !win);
  return '<div class="tr" data-tr="' + key + '">' +
    '<label class="sw"><input type="checkbox" role="switch" data-tr-on' + (val && val.trim() ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
    '<span class="sw-text"><b>' + title + '</b>' + (small ? '<small>' + small + '</small>' : '') + '</span></label>' +
    '<div class="tr-fields"' + (val && val.trim() && !raw ? '' : ' hidden') + '><input type="time" data-tr-from value="' + (win ? win[0] : '22:00') + '" aria-label="' + t('settings.from', 'From') + '"> <span>' + t('settings.to', 'to') + '</span> ' +
      '<input type="time" data-tr-to value="' + (win ? win[1] : '06:00') + '" aria-label="' + t('settings.until', 'Until') + '"> <span class="tr-note hint"></span></div>' +
    '<div class="tr-raw"' + (raw ? '' : ' hidden') + '><input data-tr-raw value="' + esc(raw ? val : '') + '" aria-label="' + t('settings.custom_window', 'Custom window') + '"><div class="hint">' + t('settings.custom_window_hint', 'This window uses a custom format, so it is shown as text. Use HH:MM-HH:MM to switch to the time pickers.') + '</div></div>' +
    '<div class="tr-err" role="alert"></div></div>';
}
function trWire(root) {
  root.querySelectorAll('.tr').forEach(box => {
    const on = box.querySelector('[data-tr-on]'), f = box.querySelector('.tr-fields'), r = box.querySelector('.tr-raw');
    const from = box.querySelector('[data-tr-from]'), to = box.querySelector('[data-tr-to]'), raw = box.querySelector('[data-tr-raw]');
    const sync = () => {
      const isRaw = !r.hidden;
      f.hidden = !on.checked || isRaw; r.hidden = !on.checked || !isRaw;
      box.querySelector('.tr-note').textContent = from.value && to.value && from.value > to.value ? t('settings.ends_next_day', 'ends the next day') : '';
      box.querySelector('.tr-err').textContent = '';
    };
    on.onchange = () => { if (on.checked && !raw.value.trim()) r.hidden = true; else if (on.checked) r.hidden = false; sync(); };
    from.oninput = to.oninput = sync;
    raw.oninput = () => { const win = trParse(raw.value); if (win) { from.value = win[0]; to.value = win[1]; r.hidden = true; sync(); } };
    sync();
  });
}
// { ok, value } - value is '' (always), or "HH:MM-HH:MM"
function trRead(root, key) {
  const box = root.querySelector('[data-tr="' + key + '"]'), err = box.querySelector('.tr-err');
  const bad = (m) => { err.textContent = m; return { ok: false }; };
  err.textContent = '';
  if (!box.querySelector('[data-tr-on]').checked) return { ok: true, value: '' };
  if (!box.querySelector('.tr-raw').hidden) {
    const v = box.querySelector('[data-tr-raw]').value.trim();
    return v ? { ok: true, value: v } : bad(t('settings.window_enter', 'Enter a window like 22:00-06:00, or turn this off.'));
  }
  const a = box.querySelector('[data-tr-from]').value, b = box.querySelector('[data-tr-to]').value;
  if (!a || !b) return bad(t('settings.window_pick_both', 'Pick both a start and an end time.'));
  if (a === b) return bad(t('settings.window_same', 'Start and end are the same time. Turn the window off for all day.'));
  return { ok: true, value: a + '-' + b };
}

async function postSettings(body) {
  const r = await api('/api/settings', { json: body });
  if (r && r.error) throw new Error(r.error);
  refreshStatus();
  return r;
}

// Load /api/settings (+ status when asked), paint html, wire sliders / live switches / the save bar.
//   spec.html(s, st) -> markup
//   spec.body(root, s) -> body for POST, or null to stop
//   spec.live[key](on) -> body for a switch flipped (saved at once)
//   spec.after(root, s) optional extra wiring
async function mountForm(el, spec) {
  let s, st;
  try { s = await api('/api/settings'); if (spec.status) st = await api('/api/status'); }
  catch (e) { el.innerHTML = '<p class="empty">' + t('settings.load_failed', 'Could not load settings: {error}', { error: esc(errText(e)) }) + '</p>'; return; }
  el.innerHTML = '<div class="sp-form">' + spec.html(s, st) + '</div>';
  const form = el.firstChild;
  form.querySelectorAll('input[type=range]').forEach(r => { r.oninput = () => { form.querySelector('[data-rv="' + r.dataset.k + '"]').textContent = r.value + r.dataset.unit; }; });
  trWire(form);
  if (spec.fill) spec.fill(form, s);
  form.querySelectorAll('[data-live]').forEach(c => {
    const make = spec.live && spec.live[c.dataset.k];
    if (!make) return;
    liveSwitch(c, async (on) => { await postSettings(make(on, s, form)); if (spec.liveAfter) spec.liveAfter(form, c); });
  });
  if (spec.after) spec.after(form, s);
  formBar(form, { save: async () => {
    const body = spec.body(form, s);
    if (!body) throw new Error(t('settings.fix_fields', 'Fix the highlighted fields first.'));
    await postSettings(body);
  } });
}

/* ---------------------------------------------------------------- forms */

const near = (a, b) => Math.abs(parseFloat(a) - b) < 1e-6;
function motionLevel(el) {
  for (const [lv, p] of Object.entries(MOTION_PRESETS)) if (Object.entries(p).every(([k, v]) => near(rd(el, k), v))) return lv;
  return '';
}

function motionPane(el) {
  mountForm(el, {
    html: (s) => { const mo = s.motion;
      const cur = { stMinObj: +((mo.min_object != null ? mo.min_object : 0.002) * 100).toFixed(3), stContrast: mo.contrast != null ? mo.contrast : 25,
        stTravel: +((mo.min_travel != null ? mo.min_travel : 0.03) * 100).toFixed(2), stMinFrames: mo.min_frames, stSens: mo.sensitivity };
      return lead(t('settings.motion_lead', 'How easily a camera decides that something moved.'), t('settings.motion_lead2', 'Medium. Choose High if small things are missed, Low if shadows, rain or trees cause false alerts.')) +
      '<div class="sec">' + t('settings.motion_label', 'Motion sensitivity') + '</div>' +
      seg('stLevel', t('settings.motion_label', 'Motion sensitivity'), [['low', t('settings.low', 'Low')], ['medium', t('settings.medium', 'Medium')], ['high', t('settings.high', 'High')]], '', 'data-nt') +
      '<div class="hint" data-lvhint></div>' +
      '<p class="hint">' + t('settings.motion_every_camera', "Every camera uses this unless you change it in that camera's own settings (Motion tab).") + '</p>' +
      trHtml('stSched', mo.schedule || '', t('settings.watch_window','Only watch during a time window'), t('settings.watch_off','Off = motion detection runs all day. Applies to every camera that has no window of its own.')) +
      more(t('settings.more_settings', 'More settings'),
        '<div class="hint">' + t('settings.motion_presets_hint', 'These are what Low, Medium and High set. Change one and the choice above shows no selection (custom).') + '</div>' +
        selectHtml(t('settings.method', 'Method'), 'stAlgo', [['objects', t('settings.method_smart', 'Smart (recommended)')], ['frame-diff', t('settings.method_simple', 'Simple')]], mo.algorithm || 'objects') +
        '<div class="hint fld-h">' + t('settings.smart_hint', 'Smart follows moving objects and ignores small or jittery changes. Simple reacts to how much of the picture changed.') + '</div>' +
        num(t('settings.smallest_object', 'Smallest object'), 'stMinObj', cur.stMinObj, 'min="0.05" max="5" step="0.05"', t('settings.pct_picture', '% of the picture'), t('settings.lower_sensitive','Lower = more sensitive: smaller things count.')) +
        num(t('settings.largest_object', 'Largest object'), 'stMaxObj', +((mo.max_object != null ? mo.max_object : 0.5) * 100).toFixed(2), 'min="1" max="100" step="1"', t('settings.pct_picture', '% of the picture'), t('settings.bigger_ignored','Anything bigger is treated as a lighting change or camera shake and ignored.')) +
        num(t('settings.contrast', 'Contrast'), 'stContrast', cur.stContrast, 'min="0" max="255" step="1"', t('settings.out_of_255', 'out of 255'), t('settings.brightness_desc','How much a spot must change in brightness to count. Lower = more sensitive; raise it to ignore shadows and noise.')) +
        num(t('settings.min_movement', 'Minimum movement'), 'stTravel', cur.stTravel, 'min="0" max="20" step="0.5"', t('settings.pct_diagonal', '% of the picture diagonal'), t('settings.movement_desc','How far something must move before it counts. Lower = more sensitive; higher filters swaying branches.')) +
        num(t('settings.simple_threshold', 'Simple method threshold'), 'stSens', cur.stSens, 'step="0.001"', '', t('settings.simple_method','Only for the Simple method. Higher = less sensitive (the opposite of the others).')) +
        '<div class="sp-grid3">' + num(t('settings.frames_in_row', 'Frames in a row'), 'stMinFrames', cur.stMinFrames, 'min="1"', t('settings.unit_frames', 'frames')) +
        num(t('settings.keep_before', 'Keep before motion'), 'stPre', mo.pre_roll, 'min="0"', t('settings.unit_seconds', 'seconds')) +
        num(t('settings.keep_after', 'Keep after motion'), 'stPost', mo.post_roll, 'min="0"', t('settings.unit_seconds', 'seconds')) + '</div>'); },
    after: (form) => {
      const hintEl = form.querySelector('[data-lvhint]');
      const sync = () => {
        const lv = motionLevel(form);
        form.querySelectorAll('input[name="stLevel"]').forEach(r => { r.checked = r.value === lv; });
        hintEl.textContent = lv ? '' : t('settings.custom_hint','Custom: set under More settings.');
      };
      form.querySelectorAll('input[name="stLevel"]').forEach(r => { r.onchange = () => {
        const p = MOTION_PRESETS[r.value]; if (!p) return;
        for (const [k, v] of Object.entries(p)) { const i = form.querySelector('[data-k="' + k + '"]'); i.value = v; i.dispatchEvent(new Event('input', { bubbles: true })); }
        sync();
      }; });
      form.addEventListener('input', (e) => { if (e.target.closest('[data-k]')) sync(); });
      sync();
    },
    body: (el, s) => {
      const win = trRead(el, 'stSched'); if (!win.ok) return null;
      return { motion: {
        algorithm: rd(el, 'stAlgo'), min_object: parseFloat(rd(el, 'stMinObj')) / 100, max_object: parseFloat(rd(el, 'stMaxObj')) / 100,
        contrast: parseInt(rd(el, 'stContrast'), 10), min_travel: parseFloat(rd(el, 'stTravel')) / 100, sensitivity: parseFloat(rd(el, 'stSens')),
        min_frames: parseInt(rd(el, 'stMinFrames'), 10), pre_roll: parseInt(rd(el, 'stPre'), 10), post_roll: parseInt(rd(el, 'stPost'), 10),
        schedule: win.value || null } };
    },
  });
}

function watchdogPane(el) {
  mountForm(el, {
    html: (s) => { const w = s.watchdog || {};
      return lead(t('settings.wd_lead', 'Fixes a camera that stops sending video, and tells you if it cannot.'), t('settings.wd_lead2', 'Turn it on for all cameras.')) +
      sw('stWdOn', t('settings.wd_watch_all', 'Watch all cameras'), t('settings.watchdog_each','Each camera can be turned on or off on its own too.'), w.enabled) +
      trHtml('stWdQuiet', w.quiet_hours || '', t('settings.no_restart_night','No restarts at night'), t('settings.during_hours','During these hours it only sends the offline alert and does not restart anything.')) +
      how(t('settings.watchdog_desc','When a camera stops sending video, AmberSight restarts the camera, then cuts its power for a moment if a smart plug or PoE port is set up for it (camera settings, Watchdog tab), then tells you if it is still down. Everything it does is listed in Settings, System, Audit. It never acts in the first 5 minutes after AmberSight starts, at most once an hour per camera, and not at all while most cameras are offline at once (that is the network or the power, not the cameras). Disabled, sleeping battery and other servers\' cameras are left alone.')) +
      more(t('settings.more_settings', 'More settings'),
        num(t('settings.wd_act_after', 'Act after no video for'), 'stWdMin', w.minutes || 10, 'min="2" max="1440"', t('settings.unit_minutes', 'minutes')) +
        num(t('settings.wd_at_most', 'At most'), 'stWdMax', w.max_per_day != null ? w.max_per_day : 3, 'min="1" max="24"', t('settings.wd_per_day', 'restarts or power cycles per camera per day'))); },
    live: { stWdOn: (on) => ({ watchdog: { enabled: on } }) },
    body: (el) => {
      const win = trRead(el, 'stWdQuiet'); if (!win.ok) return null;
      return { watchdog: {
        minutes: Math.min(1440, Math.max(2, parseInt(rd(el, 'stWdMin'), 10) || 10)),
        max_per_day: Math.min(24, Math.max(1, parseInt(rd(el, 'stWdMax'), 10) || 3)), quiet_hours: win.value || null } };
    },
  });
}

/* ---------------------------------------------------------------- hardware decoding */

// The server-wide Hardware decoding preference ([hwdecode] prefer): saved the moment it changes;
// decoders on Auto move at their next keyframe. Below it, what each camera uses now (GET
// /api/hwdecode, looked at again every 5 s while the page is open).
const HW_PREFS = [
  ['auto', t('settings.hw_pref_auto', 'Auto (recommended)')], ['nvidia', t('settings.hw_pref_nvidia', 'Prefer NVIDIA')], ['intel', t('settings.hw_pref_intel', 'Prefer Intel / AMD (Quick Sync, VAAPI)')], ['off', t('settings.hw_pref_off', 'Off (processor)')]];
// "NVIDIA (NVDEC)" / "processor" for one camera's decode, and the why.
function hwCamLine(c) {
  const x = c.decode || {};
  if (!x.stream) return { on: '', text: t('settings.hw_not_running','not running'), why: '' };
  const what = x.on === 'gpu' ? x.gpu : t('settings.hw_processor', 'processor');
  const text = x.hw_setting === 'camera' ? t('settings.hw_own_setting', '{what} (its own setting)', { what }) : what;
  return { on: x.on === 'gpu' ? 'gpu' : 'cpu', text, why: x.note || '' };
}
function hwSummary(cams) {
  const n = {};
  cams.forEach(c => { const l = hwCamLine(c); if (l.on) n[l.text] = (n[l.text] || 0) + 1; });
  const parts = Object.entries(n).sort((a, b) => b[1] - a[1]).map(([where, k]) => t('settings.hw_n_on', '{n} on {where}', { n: k, where }));
  return parts.length ? t('settings.hw_now', 'Now: {list}.', { list: parts.join(', ') }) : t('settings.hw_none', 'No camera is decoding right now.');
}
// The NVIDIA card's video memory, and what Auto keeps free of it.
function hwMemText(r) {
  const n = r.nvidia;
  if (!n || !n.present) return '';
  const gb = (mb) => (mb / 1000).toFixed(1);
  const keep = r.nvidia_min_free_mb > 0 ? t('settings.hw_auto_keeps', 'Auto keeps {gb} GB free before it adds a camera.', { gb: gb(r.nvidia_min_free_mb) }) : t('settings.hw_check_off', 'The free-memory check is off.');
  if (n.memory) return t('settings.hw_nvidia_mem', 'NVIDIA card memory: {used} of {total} GB used ({free} GB free). {keep}', { used: gb(n.memory.used_mb), total: gb(n.memory.total_mb), free: gb(n.memory.free_mb), keep });
  if (n.smi === 'missing') return t('settings.hw_nvidia_missing','NVIDIA card memory: nvidia-smi was not found, so Auto does not check for free memory.');
  return t('settings.hw_nvidia_reading','NVIDIA card memory: reading...');
}
function hwTests(r) {
  const out = [];
  const say = (ts, ok) => ts.filter(x => x.ok === ok).map(x => x.codec === 'hevc' ? 'H.265' : 'H.264').join(' ' + t('settings.and', 'and') + ' ');
  // One line per card; ways with the same result are said together ("VAAPI, Quick Sync: failed ...").
  const line = (name, ts) => {
    if (!ts || !ts.length) return;
    const res = new Map();
    [...new Set(ts.map(x => x.kind))].forEach(k => {
      const mine = ts.filter(x => x.kind === k), ok = say(mine, true), bad = mine.filter(x => !x.ok);
      const r = (ok ? t('settings.hw_decodes', 'decodes {codecs}', { codecs: ok }) : '') + (ok && bad.length ? '; ' : '') + (bad.length ? t('settings.hw_failed_test', 'failed the test ({reason})', { reason: bad[0].reason || t('settings.hw_failed', 'failed') }) : '');
      const label = k === 'cuda' ? 'NVDEC' : k === 'qsv' ? 'Quick Sync' : 'VAAPI';
      res.set(r, (res.get(r) || []).concat(label));
    });
    out.push('<li><b>' + esc(name) + '</b><span>' + [...res].map(([r, ways]) => esc(ways.join(', ') + ': ' + r)).join('<br>') + '</span></li>');
  };
  if (r.nvidia && r.nvidia.present) line(t('settings.hw_nvidia_card', 'NVIDIA card'), r.nvidia.tests);
  (r.nodes || []).forEach(n => { if (n.driver !== 'nvidia') line(n.path + (n.driver ? ' (' + n.driver + ')' : ''), n.tests); });
  return out.length ? '<ul class="hw-tests">' + out.join('') + '</ul>' : '<p class="hint">' + (r.tested ? t('settings.hw_no_card','No graphics card that can decode was found: motion detection decodes on the processor.') : t('settings.hw_test_running','The start-up test of the graphics cards is still running.')) + '</p>';
}
function hardwarePane(el) {
  let gone = false, timer = 0, busy = false;
  const paint = (r) => {
    const cams = r.cameras || [];
    el.innerHTML = '<div class="sp-form">' +
      lead(t('settings.hw_lead','Where motion detection decodes the picture: on a graphics card, or on the processor.'), t('settings.hw_auto_short','Auto.')) +
      selectHtml(t('settings.hw_decoding', 'Hardware decoding'), 'hwPrefer', HW_PREFS, r.prefer || 'auto') +
      '<div class="hint fld-h">' + t('settings.hw_hint', 'Auto tries an NVIDIA card first, then Intel Quick Sync (VAAPI on older Intel chips), then VAAPI on Intel or AMD, and uses the processor when none works. A camera with its own Hardware decoding setting keeps it. A change applies within seconds, without a restart.') + '</div>' +
      (r.nvidia && r.nvidia.present ? num(t('settings.hw_keep_free', 'Keep free on the NVIDIA card'), 'hwMinFree', r.nvidia_min_free_mb, 'min="0" max="1000000" step="100"', 'MB',
        t('settings.hw_min_free_desc','Auto and Prefer Intel / AMD put a camera on the NVIDIA card only while this much video memory stays free after it (a decoder takes about 300 MB), so the AI and other programs on the card keep room. Cameras that did not fit use Intel or the processor and move back when there is 1 GB more. 0 turns the check off. Prefer NVIDIA and a camera\'s own NVIDIA setting ignore it.')) : '') +
      '<p class="hw-now" data-hwsum role="status">' + esc(hwSummary(cams)) + '</p>' +
      '<p class="hint" data-hwmem>' + esc(hwMemText(r)) + '</p>' +
      (r.order && r.order.length ? '<p class="hint">' + t('settings.hw_order', 'Order tried now: {list}.', { list: esc(r.order.join(', ')) }) + '</p>' : '') +
      more(t('settings.hw_per_camera', 'Per camera'), '<ul class="hw-cams" data-hwcams>' + hwCamsHtml(cams) + '</ul>') +
      more(t('settings.hw_startup_test', 'Start-up test'), hwTests(r)) +
      how('<p>' + t('settings.hw_test_desc1', "At start, each way (NVIDIA NVDEC, Intel Quick Sync, VAAPI) decodes a short H.264 and H.265 sample and its pictures are compared with the processor's; a way that fails is never used by Auto.") + '</p>' +
        '<p>' + t('settings.hw_test_desc2', '<b>Prefer NVIDIA</b> puts the NVIDIA card first with no memory check. <b>Prefer Intel / AMD</b> puts it last.') + '</p>' +
        t('settings.hw_test_desc3','<p>When a way fails while running, that camera moves to the next way, or the processor, at once, and tries the failed way again after 15 minutes, then after longer waits (up to 4 hours). A second failure in a row (with no 30 minutes of good running between) makes it leave that way until AmberSight restarts or a setting on this page changes. The NVIDIA card running out of memory is only waited out.</p>') +
        '<p>' + t('settings.hw_test_desc4', 'A camera keeps its place on the NVIDIA card through a short reconnect and gives it up after about 2 minutes offline.') + '</p>' +
        '<p>' + t('settings.hw_test_desc5', "<b>Linux:</b> Quick Sync on Intel Gen12 and newer needs Intel's GPU runtime (<code>sudo apt install libmfx-gen1.2</code> on Debian and Ubuntu, <code>intel-vpl-gpu-rt</code> on Fedora; then restart AmberSight). <b>Windows:</b> an NVIDIA card that passed the test, otherwise the processor. <b>macOS:</b> the processor.") + '</p>' +
        '<p>' + t('settings.hw_test_desc6', '<b>In the config:</b> <code>[hwdecode] prefer = &quot;auto&quot;</code> (or nvidia, intel, off) and <code>nvidia_min_free_mb = 1536</code>.') + '</p>') +
      '</div>';
    const s = el.querySelector('[data-k="hwPrefer"]');
    s.onchange = async () => {
      s.disabled = true;
      try { await postSettings({ hwdecode: { prefer: s.value } }); savedToast(); setTimeout(refresh, 2500); }
      catch (e) { toast(t('settings.save_error', 'Could not save: {error}', { error: errText(e) })); }
      s.disabled = false;
    };
    const f = el.querySelector('[data-k="hwMinFree"]');
    if (f) f.onchange = async () => {
      const v = Math.min(1000000, Math.max(0, parseInt(f.value, 10) || 0));
      f.value = v;
      try { await postSettings({ hwdecode: { nvidia_min_free_mb: v } }); savedToast(); setTimeout(refresh, 2500); }
      catch (e) { toast(t('settings.save_error', 'Could not save: {error}', { error: errText(e) })); }
    };
  };
  const hwCamsHtml = (cams) => cams.length ? cams.map(c => {
    const l = hwCamLine(c);
    return '<li><b>' + esc(c.name) + '</b><span class="' + (l.on === 'gpu' ? 'hw-gpu' : 'muted') + '">' + esc(l.text) + '</span>' + (l.why ? '<div class="hint">' + esc(l.why.charAt(0).toUpperCase() + l.why.slice(1)) + '</div>' : '') + '</li>';
  }).join('') : '<li class="hint">' + t('settings.no_cameras', 'No cameras.') + '</li>';
  const refresh = async () => {
    if (gone || busy) return;
    busy = true;
    try {
      const r = await api('/api/hwdecode');
      if (gone) return;
      const sum = el.querySelector('[data-hwsum]');
      if (!sum) paint(r);
      else { sum.textContent = hwSummary(r.cameras || []); el.querySelector('[data-hwmem]').textContent = hwMemText(r); el.querySelector('[data-hwcams]').innerHTML = hwCamsHtml(r.cameras || []); }
    } catch (e) { if (!el.querySelector('[data-hwsum]')) el.innerHTML = '<p class="empty">' + t('settings.could_not_load', 'Could not load: {error}', { error: esc(errText(e)) }) + '</p>'; }
    finally { busy = false; }
  };
  el.innerHTML = '<p class="hint">' + t('settings.loading', 'Loading...') + '</p>';
  refresh();
  timer = setInterval(refresh, 5000);
  return () => { gone = true; clearInterval(timer); };
}

function hookPane(el) {
  mountForm(el, {
    html: (s) => lead(t('settings.odt_optional', 'Optional. Runs a program of your own on each event picture to decide what is in it.'), t('settings.odt_leave_off', 'Leave this off unless you have your own detector. The built-in AI is under Detection &amp; alerts, Objects (AI).')) +
      sw('stOdtEnabled', t('settings.run_detector_command', 'Run the detector command'), t('settings.runs_on_motion', 'Runs on every motion event.'), s.odt.enabled) +
      inp(t('settings.detector_command', 'Detector command'), 'stOdtCmd', s.odt.command || '', '', t('settings.odt_program', "The program to run; '{image}' is replaced by the event picture.")) +
      sw('stOdtKnown', t('settings.alert_only_when_objects_found', 'Alert only when objects are found'), t('settings.events_no_alert', 'Events where the command finds nothing are still recorded, but send no alert.'), s.odt.alert_only_known),
    live: { stOdtEnabled: (on) => ({ odt: { enabled: on } }), stOdtKnown: (on) => ({ odt: { alert_only_known: on } }) },
    body: (el) => ({ odt: { command: rd(el, 'stOdtCmd').trim() || null } }),
  });
}

/* ---------------------------------------------------------------- alerts: limits + service cards */

function rulesPane(el) {
  mountForm(el, {
    html: (s) => { const a = s.alerts;
      return lead(t('settings.limits_lead', 'Keeps alerts from piling up, and sets how urgent they look on your phone.'), t('settings.limits_lead2', 'Leave the limits at 0 to start; add one if a busy camera sends too many alerts.')) +
      '<div class="sec">' + t('settings.how_often', 'How often') + '</div>' +
      '<div class="hint">' + t('settings.limits_every_camera', 'Every camera uses these unless its own settings (Alerts tab) set a limit of their own.') + '</div>' +
      num(t('settings.at_most_per_camera', 'At most one alert per camera every'), 'stMinInt', a.min_interval || 0, 'min="0" max="86400" step="5"', t('settings.unit_seconds', 'seconds'), t('settings.limit_explanation', "0 = no limit. A camera's own settings can override this.")) +
      num(t('settings.at_most_all', 'At most one alert across all cameras every'), 'stGrpInt', a.group_interval || 0, 'min="0" max="86400" step="5"', t('settings.unit_seconds', 'seconds'), t('settings.zero_no_limit', '0 = no limit.')) +
      num(t('settings.arm_wait', 'Wait this long before arming, after you press Arm'), 'stArmDelay', a.arm_delay || 0, 'min="0" max="3600" step="5"', t('settings.unit_seconds', 'seconds'),
        t('settings.arm_delay_expl', '0 = starts at once. Held-back alerts are still recorded as events and still sent over MQTT; the next alert that goes out says how many were held back.')) +
      '<div class="sec">' + t('settings.all_picture_notifications', 'All picture notifications') + '</div>' +
      selectHtml(t('settings.default_priority', 'Default priority'), 'stPrio', [['low', t('settings.prio_low', 'Low (quiet)')], ['normal', t('settings.prio_normal', 'Normal')], ['high', t('settings.prio_high', 'High (through Do Not Disturb on most phones)')], ['urgent', t('settings.prio_urgent', 'Urgent (Pushover repeats until acknowledged)')]], a.priority || 'normal') +
      '<div class="sec">' + t('settings.system_problems', 'System problems') + '</div>' +
      sw('stSysOn', t('settings.alert_when_wrong', 'Alert when something is wrong'), t('settings.system_alert_desc', 'A camera offline, a camera online but no recordings being written, a drive nearly full or unreadable. Sent once to your services (no picture), and again when it is resolved. Security warnings (a new device signing in, wrong codes, a recovery code used and the like) still go out when this is off.'), a.system_alerts !== false) +
      num(t('settings.alert_after_problem', 'Alert after the problem has lasted'), 'stSysMin', a.system_alert_minutes || 5, 'min="1" max="1440"', t('settings.unit_minutes', 'minutes')) +
      '<div class="row"><button class="btn" type="button" data-systest>' + t('settings.send_test_all', 'Send a test to all services') + '</button><span class="hint" data-systestout role="status"></span></div>' +
      more(t('settings.more_settings', 'More settings'), inp(t('settings.server_address_open_links', "This server's address for \"open\" links"), 'stLink', a.link_url || '', 'placeholder="' + esc(location.origin) + '"',
        t('settings.link_hint', 'Tapping a notification opens that camera at the moment of the alert. Use an address your phone can reach (home network or Tailscale). Leave empty to use the address you are using now.')) +
        '<div class="hint">' + t('settings.mqtt_problem_sensor', 'MQTT gets a Problem sensor for system problems.') + '</div>'); },
    live: { stSysOn: (on) => ({ alerts: { system_alerts: on ? null : false } }) },
    body: (el) => ({ alerts: {
      link_url: rd(el, 'stLink').trim() || null, priority: rd(el, 'stPrio') || 'normal',
      min_interval: Math.max(0, parseInt(rd(el, 'stMinInt'), 10) || 0), group_interval: Math.max(0, parseInt(rd(el, 'stGrpInt'), 10) || 0),
      arm_delay: Math.min(3600, Math.max(0, parseInt(rd(el, 'stArmDelay'), 10) || 0)),
      system_alert_minutes: Math.min(1440, Math.max(1, parseInt(rd(el, 'stSysMin'), 10) || 5)) } }),
    after: (el) => { el.querySelector('[data-systest]').onclick = (e) => runSystemTest(e.target, el.querySelector('[data-systestout]')); },
  });
}

// POST /api/alerts/system_test sends to every service that is set up (saved settings).
async function runSystemTest(btn, out, matcher) {
  btn.disabled = true; out.classList.remove('bad'); out.textContent = t('settings.sending_saved', 'Sending (saved settings)...');
  let r;
  try { r = await api('/api/alerts/system_test', { json: {} }); } catch (err) { out.textContent = errText(err); out.classList.add('bad'); btn.disabled = false; return; }
  const parts = [];
  if (r.sent.length) parts.push(t('settings.sent', 'Sent: {list}', { list: r.sent.join(', ') }));
  if (r.failed.length) parts.push(t('settings.failed', 'Failed: {list} (see Settings, System, Logs)', { list: r.failed.join(', ') }));
  if (!r.sent.length && !r.failed.length) parts.push(t('settings.no_service_set_up', 'No notification service is set up{mqtt, select, yes {; MQTT shows problems on its own} other {}}', { mqtt: r.mqtt ? 'yes' : 'no' }));
  let head = '';
  if (matcher) {
    const hit = (list) => list.some(n => matcher.test(n));
    head = hit(r.sent) ? t('settings.service_received', 'This service received it.') + ' ' : hit(r.failed) ? t('settings.service_failed', 'This service FAILED.') + ' ' : t('settings.service_not_reached', 'This service was not reached (is it saved?).') + ' ';
    if (hit(r.failed) || !hit(r.sent)) out.classList.add('bad');
  }
  out.textContent = head + t('settings.test_goes_every_service', 'The test goes to every configured service.') + ' ' + parts.join('. ') + '.';
  btn.disabled = false;
}

// One card per service. fields: k = alert setting, secret = blank keeps the stored value.
const CARDS = [
  { id: 'push', label: t('settings.card_push', 'Browser and phone push'), ch: 'push', custom: 'push', grp: 'rec', gloss: t('settings.card_push_gloss', 'Alerts on this phone or computer, no account needed'), badge: t('settings.recommended', 'Recommended'),
    sum: (a, cfg) => cfg ? t('settings.card_push_on', 'Devices signed up') : t('settings.card_push_off', 'No device has notifications on yet') },
  { id: 'telegram', label: 'Telegram', ch: 'telegram', test: /telegram/i, grp: 'apps', gloss: t('settings.card_telegram_gloss', 'Messages in the Telegram app (needs a free bot)'),
    fields: [['telegram_bot_token', t('settings.f_bot_token', 'Bot token'), 'secret'], ['telegram_chat_id', t('settings.f_chat_id', 'Chat id')], ['telegram_api_url', t('settings.f_api_base', 'API base (advanced)')]],
    sum: (a) => t('settings.sum_chat', 'Chat {id}', { id: a.telegram_chat_id }) },
  { id: 'ntfy', label: 'ntfy', ch: 'ntfy', test: /ntfy/i, grp: 'apps', gloss: t('settings.card_ntfy_gloss', 'Free phone alerts app, no account needed'),
    fields: [['ntfy_url', t('settings.f_topic_url', 'Topic URL (ntfy.sh or your own server)'), '', 'placeholder="https://ntfy.example.com/cameras"'], ['ntfy_token', t('settings.f_access_token', 'Access token (only for protected topics)'), 'secret']],
    note: t('settings.card_ntfy_note', 'Alerts include the event picture. If your server has attachments turned off, they arrive as text.'),
    sum: (a) => a.ntfy_url },
  { id: 'pushover', label: 'Pushover', ch: 'pushover', test: /pushover/i, grp: 'apps', gloss: t('settings.card_pushover_gloss', 'Paid phone alerts app that can repeat until you answer'),
    fields: [['pushover_token', t('settings.f_app_token', 'Application token'), 'secret'], ['pushover_user', t('settings.f_user_key', 'User or group key')], ['pushover_device', t('settings.f_device', 'Device (blank = all your devices)')], ['pushover_sound', t('settings.f_sound', 'Sound (blank = your default)'), '', 'placeholder="' + t('settings.ph_siren', 'e.g. siren') + '"']],
    sum: (a) => t('settings.sum_pushover', 'User {user}{device, select, yes {, device {name}} other {, all devices}}', { user: a.pushover_user, device: a.pushover_device ? 'yes' : 'no', name: a.pushover_device }) },
  { id: 'email', label: t('settings.card_email', 'Email'), ch: 'email', test: /email/i, grp: 'apps', gloss: t('settings.card_email_gloss', 'A message to your inbox, with the picture'),
    fields: [['smtp_host', t('settings.f_smtp_server', 'SMTP server')], ['smtp_port', t('settings.f_port', 'Port'), '', 'type="number" min="1" max="65535" placeholder="587"'], ['smtp_tls', t('settings.f_encryption', 'Encryption'), 'tls'],
      ['smtp_user', t('settings.f_user_name', 'User name')], ['smtp_password', t('settings.f_password', 'Password'), 'secret'], ['smtp_from', t('settings.f_from_address', 'From address')], ['email_to', t('settings.f_send_to', 'Send to (comma separated)'), 'list']],
    sum: (a) => t('settings.sum_email', '{to} via {host}', { to: (a.email_to || []).join(', '), host: a.smtp_host }) },
  { id: 'webhook', label: t('settings.card_webhook', 'Webhook'), ch: 'webhook', test: /webhook/i, grp: 'adv', gloss: t('settings.card_webhook_gloss', 'Send each alert to a web address as data'),
    fields: [['webhook_url', t('settings.f_webhook_url', 'Webhook URL (JSON POST)'), 'secret']], sum: (a) => (a.webhook_url ? t('settings.sum_set', 'set') : '') },
  { id: 'home_assistant', label: 'Home Assistant', ch: 'home_assistant', grp: 'apps', gloss: t('settings.card_ha_gloss', 'Lets your smart home react to events'),
    fields: [['ha_url', t('settings.f_ha_url', 'Home Assistant URL')], ['ha_token', t('settings.f_ha_token', 'Long-lived access token'), 'secret'], ['ha_entity', t('settings.f_entity', 'Entity')]],
    note: t('settings.card_ha_note', 'Called on every event. Not part of the test: it has no system-problem message.'), sum: (a) => a.ha_entity ? t('settings.sum_ha', '{entity} at {url}', { entity: a.ha_entity, url: a.ha_url }) : a.ha_url },
  { id: 'image_post', label: t('settings.card_image_post', 'Image post'), ch: 'image_post', grp: 'adv', gloss: t('settings.card_image_post_gloss', 'Send the event picture to a web address'),
    fields: [['image_post_url', t('settings.f_image_post_url', 'Image post URL (binary JPEG)'), 'secret']], note: t('settings.card_image_post_note', 'Sends the event picture to this address. It runs on events only, so there is no test.'), sum: (a) => (a.image_post_url ? t('settings.sum_set', 'set') : '') },
  { id: 'action', label: t('settings.card_action', 'Action command'), ch: 'action', grp: 'adv', gloss: t('settings.card_action_gloss', 'Run a program of yours on every event'),
    fields: [['action_command', t('settings.f_command', "Command (argv, '{image}' '{clip}' macros)")]], note: t('settings.card_action_note', 'Runs a program on every event; no shell is used. Events only, so there is no test.'), sum: (a) => a.action_command },
  { id: 'mqtt', label: 'MQTT', ch: 'mqtt', grp: 'adv', gloss: t('settings.card_mqtt_gloss', 'Publish events to a message broker (smart-home setups)'),
    fields: [['mqtt_url', t('settings.f_broker_url', 'Broker URL'), 'secret', 'placeholder="mqtt://user:pass@host:1883"'], ['mqtt_prefix', t('settings.f_topic_prefix', 'Topic prefix'), '', 'placeholder="ambersight"'], ['mqtt_discovery', t('settings.f_discovery', 'Publish Home Assistant discovery messages'), 'bool'], ['mqtt_siren', t('settings.f_siren', 'Offer camera sirens to Home Assistant (anyone who can publish to the broker can sound them)'), 'bool']],
    note: t('settings.card_mqtt_note', 'MQTT reports every event (it is not held back by the alert limits) and system problems. Changes to the broker apply after AmberSight restarts.'), sum: (a) => a.mqtt_url === '<set>' ? t('settings.sum_broker_set', 'Broker set') : a.mqtt_url },
];

function cardFieldHtml(f, a) {
  const [k, label, kind, extra] = f, v = a[k];
  if (kind === 'bool') return swHtml('data-f="' + k + '" data-live', !!v, esc(label));
  if (kind === 'tls') return '<label class="fld"><span class="fld-l">' + esc(label) + '</span><select data-f="' + k + '">' + [['starttls', t('settings.tls_starttls', 'STARTTLS (port 587)')], ['tls', t('settings.tls_tls', 'TLS (port 465)')], ['none', t('settings.tls_none', 'None')]].map(([o, text]) => '<option value="' + o + '"' + ((v || 'starttls') === o ? ' selected' : '') + '>' + text + '</option>').join('') + '</select></label>';
  if (kind === 'secret') {
    const set = v === '<set>';
    return '<label class="fld"><span class="fld-l">' + esc(label) + '</span><span class="fld-in"><input data-f="' + k + '" type="password" autocomplete="new-password" value="" placeholder="' + (set ? t('settings.stored_keep', 'stored - leave blank to keep') : '') + '" ' + (set ? '' : (extra || '')) + '></span></label>';
  }
  const show = kind === 'list' ? (v || []).join(', ') : (v == null || (k === 'smtp_port' && !v) ? '' : v);
  return '<label class="fld"><span class="fld-l">' + esc(label) + '</span><span class="fld-in"><input data-f="' + k + '" value="' + esc(show) + '" ' + (extra || '') + '></span></label>';
}

function servicesPane(el) {
  el.innerHTML = '<p class="empty">' + t('settings.loading', 'Loading...') + '</p>';
  let s, open = null;
  const load = async () => {
    try { s = await api('/api/settings'); } catch (e) { el.innerHTML = '<p class="empty">' + t('settings.load_failed', 'Could not load settings: {error}', { error: esc(errText(e)) }) + '</p>'; return; }
    draw();
  };
  const configured = (c) => c.ch === 'mqtt' ? !!s.alerts.mqtt_url : !!(s.alert_channels || []).find(x => x.id === c.ch && x.configured);
  function cardHtml(c) {
    const a = s.alerts, on = configured(c), isOpen = open === c.id;
    return '<section class="sv-card' + (isOpen ? ' open' : '') + '" data-card="' + c.id + '">' +
      '<button class="sv-head" type="button" aria-expanded="' + isOpen + '"><span class="sv-dot ' + (on ? 'on' : 'off') + '" aria-hidden="true"></span><b>' + esc(c.label) + '</b>' +
      (c.badge ? '<span class="sv-badge">' + c.badge + '</span>' : '') +
      '<span class="sv-sum">' + (on ? esc(c.sum(a, on) || t('settings.on', 'On')) : esc(c.gloss)) + '</span><span class="sv-state">' + (on ? '&#10003; ' + t('settings.on', 'On') : t('settings.off', 'Off')) + '</span><span class="sv-chev" aria-hidden="true">' + ic('down') + '</span></button>' +
      (isOpen ? '<div class="sv-body">' + (c.custom === 'push' ? '<div data-pushcard></div>' :
        ('<div class="sv-form" data-svform><div class="hint">' + esc(c.gloss) + '.</div>' + c.fields.map(f => cardFieldHtml(f, a)).join('') + (c.note ? '<div class="hint">' + c.note + '</div>' : '') +
        ((c.test || on) ? '<div class="row sv-row">' + (c.test ? '<button class="btn" type="button" data-svtest title="' + t('settings.test_title', 'Sends a system test to every configured service') + '">' + t('settings.send_test', 'Send test') + '</button>' : '') +
          (on ? '<button class="btn danger" type="button" data-svclear>' + t('settings.remove', 'Remove') + '</button>' : '') + '</div>' : '') +
        '<div class="hint sv-out" data-svout role="status"></div></div>')) + '</div>' : '') + '</section>';
  }
  function draw() {
    const grp = (g) => CARDS.filter(c => c.grp === g).map(cardHtml).join('');
    const advOpen = CARDS.some(c => c.grp === 'adv' && (c.id === open || configured(c)));
    el.innerHTML = lead(t('settings.choose_alerts', 'Choose where alerts are sent. Set each one up once; every camera uses all of them unless its own settings (Cameras, a camera, Alerts tab) pick some.'), t('settings.browser_push', 'Browser and phone push is enough for most homes.')) +
      '<p class="hint">' + t('settings.alerts_hint', 'How often alerts go out, and their priority, are under {limits}; what an alert says is under {rules}.', { limits: '<a href="#settings/detection/limits">' + t('settings.limits_link', 'Limits &amp; priority') + '</a>', rules: '<a href="#settings/detection/rules">' + t('settings.rules_label', 'Alert rules') + '</a>' }) + '</p>' +
      '<div class="sp-grp"><b>' + t('settings.recommended', 'Recommended') + '</b></div><div class="sv-list">' + grp('rec') + '</div>' +
      '<div class="sp-grp"><b>' + t('settings.phone_apps_email', 'Phone apps and email') + '</b></div><div class="sv-list">' + grp('apps') + '</div>' +
      more(t('settings.advanced_connections', 'Advanced connections'), '<p class="hint">' + t('settings.advanced_hint', 'For automations and other software: Webhook, Image post, Action command and MQTT.') + '</p><div class="sv-list">' + grp('adv') + '</div>', advOpen);
    el.querySelectorAll('.sv-head').forEach(b => {
      b.onclick = async () => {
        if (!(await confirmLeave())) return;
        const id = b.parentNode.dataset.card; open = open === id ? null : id; draw();
      };
    });
    const card = open && CARDS.find(c => c.id === open);
    if (!card) return;
    const body = el.querySelector('[data-card="' + open + '"] .sv-body');
    if (card.custom === 'push') { mountPushCard(body.querySelector('[data-pushcard]'), true); return; }
    const out = body.querySelector('[data-svout]');
    const form = body.querySelector('[data-svform]');
    // On/off inside a service (MQTT discovery) saves when flipped; the rest goes through the bar.
    form.querySelectorAll('input[data-live]').forEach(i => liveSwitch(i, async (on) => { await postSettings({ alerts: { [i.dataset.f]: on } }); s.alerts[i.dataset.f] = on; }));
    formBar(form, { save: async () => {
      const patch = {};
      for (const [k, , kind] of card.fields) {
        if (kind === 'bool') continue;
        const i = body.querySelector('[data-f="' + k + '"]');
        let v = i.value.trim();
        if (kind === 'secret' && !v) { if (s.alerts[k]) continue; patch[k] = null; continue; }
        if (k === 'smtp_port') { if (v && (!/^\d+$/.test(v) || +v < 1 || +v > 65535)) throw new Error(t('settings.port_error', 'Port must be a number from 1 to 65535.')); if (!v) continue; patch[k] = +v; continue; }
        if (kind === 'list') { patch[k] = v.split(/[,;\s]+/).filter(Boolean); continue; }
        patch[k] = v || null;
      }
      out.classList.remove('bad'); out.textContent = '';
      await postSettings({ alerts: patch });
      await load();
    } });
    const testBtn = body.querySelector('[data-svtest]');
    if (testBtn) testBtn.onclick = () => runSystemTest(testBtn, out, card.test);
    const clr = body.querySelector('[data-svclear]');
    if (clr) clr.onclick = async () => {
      if (!(await confirmDialog({ title: t('settings.remove_title', 'Remove {label}?', { label: card.label }), body: t('settings.remove_body', '{label} stops receiving alerts. Its saved details are deleted from this server.', { label: card.label }), ok: t('settings.remove', 'Remove'), danger: true }))) return;
      clearUnsaved(shell);
      const patch = {};
      for (const [k, , kind] of card.fields) patch[k] = kind === 'bool' ? false : kind === 'list' ? [] : null;
      try { await postSettings({ alerts: patch }); savedToast(t('settings.removed', '{label} removed', { label: card.label })); await load(); }
      catch (err) { out.textContent = errText(err); out.classList.add('bad'); }
    };
    const f0 = body.querySelector('[data-f]:not([data-live])');
    if (f0) f0.focus({ preventScroll: true });
  }
  load();
}

/* ---------------------------------------------------------------- routing */

const adminSub = (id, label, wide, page) => ({ id, label, page: page || id, wide });
// Section -> subs. A sub is { id, label, render(el) -> cleanup? } or { page: <admin page id> }.
// D2: eight sections. Route ids are a contract (see ui-brief): #settings/<section>/<sub>.
const SECTIONS = [
  { id: 'cameras', group: 'Everyday', label: t('settings.cameras_label', 'Cameras'), desc: t('settings.cameras_desc', 'Add, edit and find cameras, and arrange them in groups.'), subs: [
    { id: 'list', label: t('settings.cameras_label', 'Cameras'), render: (el) => mountCameraList(el) },
    adminSub('find', t('settings.find_cameras', 'Find cameras'), false, 'discovery'),
    { id: 'groups', label: t('settings.groups_label', 'Groups'), render: (el) => mountGroups(el) }] },
  { id: 'recording', group: 'Everyday', label: t('settings.recording_label', 'Recording'), desc: t('settings.recording_desc', 'How long footage is kept, how much space it may use, and where it is stored.'), subs: [
    adminSub('retention', t('settings.retention_label', 'Storage & retention'), false, 'retention'), adminSub('drives', t('settings.drives_label', 'Drives & folders'), false, 'storage')] },
  { id: 'detection', group: 'Everyday', label: t('settings.detection_label', 'Detection & alerts'), desc: t('settings.detection_desc', 'What counts as motion, what AI looks for, and where alerts go.'), subs: [
    { id: 'motion', label: t('settings.motion_label', 'Motion sensitivity'), render: motionPane },
    adminSub('objects', t('settings.objects_label', 'Objects (AI)'), false, 'ai'),
    { id: 'alerts', label: t('settings.alerts_label', 'Alerts'), render: servicesPane },
    { id: 'limits', label: t('settings.limits_label', 'Limits & priority'), render: rulesPane },
    adminSub('rules', t('settings.rules_label', 'Alert rules'), false, 'alertrules'),
    adminSub('summary', t('settings.summary_label', 'Morning summary'), false, 'digest')] },
  { id: 'smart', group: 'Optional', label: t('settings.smart_label', 'Smart features'), desc: t('settings.smart_desc', 'All optional: AI review, faces, license plates, sounds and smart search.'), subs: [
    adminSub('review', t('settings.review_label', 'AI review'), true, 'aireview'), adminSub('faces', t('settings.faces_label', 'Faces'), true), adminSub('plates', t('settings.plates_label', 'License plates'), true, 'lpr'),
    adminSub('sounds', t('settings.sounds_label', 'Sounds'), false, 'sound'), adminSub('search', t('settings.search_label', 'Smart search'), false, 'smart')] },
  { id: 'schedules', group: 'Optional', label: t('settings.schedules_label', 'Schedules & presence'), desc: t('settings.schedules_desc', 'Profiles that change how cameras behave, the weekly schedule, and arming by phones at home or away.'), subs: [
    adminSub('profiles', t('settings.profiles_label', 'Profiles & schedule'), true), adminSub('presence', t('settings.presence_label', 'Presence'))] },
  { id: 'users', group: 'Admin', label: t('settings.users_label', 'Users & security'), desc: t('settings.users_desc', 'Accounts, which cameras they can see, HTTPS, and displays (wall TVs, tablets).'), subs: [
    adminSub('users', t('settings.users_sub', 'Users'), true), { id: 'https', label: 'HTTPS', render: mountHttps }, { id: 'displays', label: t('settings.displays_label', 'Displays'), render: (el) => mountDisplays(el) }] },
  { id: 'connections', group: 'Admin', label: t('settings.connections_label', 'Connections'), desc: t('settings.connections_desc', 'Advanced: share streams, Apple Home, link servers, an external detector and the camera watchdog.'), subs: [
    { id: 'share', label: t('settings.share_label', 'Share streams'), render: (el) => mountShare(el) },
    { id: 'homekit', label: t('settings.homekit_label', 'Apple Home'), render: (el) => mountHomekit(el) },
    { id: 'servers', label: t('settings.servers_label', 'Servers'), render: (el) => mountServers(el) },
    { id: 'detector', label: t('settings.detector_label', 'External detector'), render: hookPane },
    { id: 'watchdog', label: t('settings.watchdog_label', 'Watchdog'), render: watchdogPane }] },
  { id: 'system', group: 'Admin', label: t('settings.system_label', 'System'), desc: t('settings.system_desc', 'Health and updates, logs, the audit trail, backups and hardware decoding.'), subs: [
    adminSub('status', t('settings.status_label', 'Status & updates'), true), adminSub('logs', t('settings.logs_label', 'Logs'), true), adminSub('audit', t('settings.audit_label', 'Audit'), true), adminSub('backup', t('settings.backup_label', 'Backup'), false, 'offsite'),
    { id: 'hardware', label: t('settings.hardware_label', 'Hardware'), render: hardwarePane },
    { id: 'language', label: t('settings.language_label', 'Language'), render: (el) => { mountLangSettings(el); } }] },
];
const GROUPS = ['Everyday', 'Optional', 'Admin'];
// (The group ids are compared in code; this is what the page says.)
const GROUP_NAME = { Everyday: t('settings.group_everyday', 'Everyday'), Optional: t('settings.group_optional', 'Optional'), Admin: t('settings.group_admin', 'Admin') };

// Old route (before the D2 regroup) -> new route. Also used by help links, the palette,
// gettingstarted and old bookmarks: goSettings() and the URL both accept either.
const OLD_ROUTES = {
  'cameras/discovery': 'cameras/find', groups: 'cameras/groups', 'groups/list': 'cameras/groups',
  recording: 'recording/retention', 'recording/limits': 'recording/retention', 'recording/storage': 'recording/drives',
  motion: 'detection/motion', 'motion/defaults': 'detection/motion',
  'detection/ai': 'detection/objects', 'detection/review': 'smart/review', 'detection/lpr': 'smart/plates',
  'detection/sound': 'smart/sounds', 'detection/smart': 'smart/search', 'detection/hook': 'connections/detector', 'detection/faces': 'smart/faces',
  alerts: 'detection/alerts', 'alerts/services': 'detection/alerts', 'alerts/rules': 'detection/limits', 'alerts/digest': 'detection/summary', 'alerts/words': 'detection/rules',
  profiles: 'schedules/profiles', 'profiles/profiles': 'schedules/profiles', 'profiles/presence': 'schedules/presence',
  watchdog: 'connections/watchdog', 'watchdog/watchdog': 'connections/watchdog',
  share: 'connections/share', 'share/share': 'connections/share', servers: 'connections/servers', 'servers/list': 'connections/servers',
  backup: 'system/backup', 'backup/offsite': 'system/backup',
};
const find = (path) => {
  const [a, b] = (path || '').split('/');
  const sec = SECTIONS.find(x => x.id === a);
  const sub = sec && sec.subs.find(x => x.id === b);
  return sec && sub ? { sec, sub } : null;
};
// Any route (old or new) -> the canonical "section/sub", or '' when it names nothing.
export function settingsRoute(path) {
  path = String(path || '').replace(/^#?settings\/?/, '').split('?')[0].replace(/\/+$/, '');
  const hit = find(path) || find(OLD_ROUTES[path]);
  if (hit) return hit.sec.id + '/' + hit.sub.id;
  const sec = SECTIONS.find(x => x.id === path.split('/')[0]);
  return sec ? sec.id + '/' + sec.subs[0].id : '';
}

// Route -> help topic (help.js ids). Pages without a topic of their own fall back to the section's.
const HELP_TOPICS = {
  'cameras/list': 'settings-cameras', 'cameras/find': 'admin-discovery', 'cameras/groups': 'settings-groups',
  'recording/retention': 'settings-storage', 'recording/drives': 'admin-storage', 'detection/motion': 'settings-motion',
  'detection/objects': 'admin-ai', 'smart/review': 'admin-aireview', 'smart/faces': 'admin-faces', 'smart/plates': 'admin-lpr', 'connections/detector': 'settings-odt',
  'detection/alerts': 'settings-alerts', 'detection/limits': 'settings-alerts', 'detection/summary': 'settings-digest', 'detection/rules': 'admin-alertrules',
  'schedules/profiles': 'admin-profiles', 'schedules/presence': 'admin-presence', 'connections/watchdog': 'settings-watchdog', 'connections/servers': 'settings-servers', 'connections/share': 'settings-share', 'connections/homekit': 'settings-homekit',
  'users/users': 'admin-users', 'users/https': 'settings-https', 'users/displays': 'display-mode', 'system/status': 'admin-status', 'system/logs': 'admin-logs', 'system/audit': 'admin-audit', 'system/hardware': 'settings-hardware',
};
const helpTopic = (sec, sub) => HELP_TOPICS[sec.id + '/' + sub.id] || 'settings';

let shell = null, cleanup = null, rendered = '', token = 0, lastRoute = '';

function resolve(path) {
  const r = settingsRoute(path) || (lastRoute ? settingsRoute(lastRoute) : '') || 'cameras/list';
  return find(r);
}

const railHtml = () => GROUPS.map(g => '<div class="sp-group" role="presentation">' + GROUP_NAME[g] + '</div>' +
  SECTIONS.filter(s => s.group === g).map(s => '<a class="sp-item" href="#settings/' + s.id + '/' + s.subs[0].id + '" data-sec="' + s.id + '">' + esc(s.label) + '</a>').join('')).join('');

function build(el) {
  el.innerHTML = '<div class="sp"><nav class="sp-rail" aria-label="' + t('settings.aria_label_sections', 'Settings sections') + '">' + railHtml() + '</nav>' +
    '<div class="sp-main"><button type="button" class="sp-sectbtn" aria-haspopup="dialog">' + t('settings.tab_label', 'Settings') + ' &middot; <b data-sectname></b> <span aria-hidden="true">&#9662;</span></button>' +
    '<div class="sp-head"><div class="sp-title"><h2></h2><a href="#" class="helpq sp-help" data-help="settings" title="' + t('settings.help_title', 'Help for this page') + '">' + t('settings.help', 'Help') + '</a></div><div class="sp-desc hint"></div><div class="sp-subs chips" role="tablist"></div></div>' +
    '<div class="sp-body"></div></div></div>';
  shell = el;
  el.querySelector('.sp-rail').addEventListener('click', (e) => {
    const a = e.target.closest('a[data-sec]');
    if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button) return;
    e.preventDefault();
    goSettings(a.getAttribute('href').slice('#settings/'.length));
  });
  tablistKeys(el.querySelector('.sp-subs'));
  el.querySelector('.sp-subs').addEventListener('click', (e) => {
    const b = e.target.closest('button[data-sub]');
    if (b) goSettings(b.dataset.sub);
  });
  el.querySelector('.sp-sectbtn').onclick = openPicker;
}

// Phone: the list of the eight sections, full screen.
// Modal while open (the page behind is inert, Tab stays in it); closing gives the focus back.
let picker = null, pickerRelease = null;
function closePicker() {
  if (!picker) return;
  picker.remove(); picker = null;
  document.removeEventListener('keydown', pickerKey, true);
  if (pickerRelease) { pickerRelease(false); pickerRelease = null; }
  const b = shell && shell.querySelector('.sp-sectbtn');
  if (b) { try { b.focus({ preventScroll: true }); } catch (e) { /* gone */ } }
  overlaysChanged();
}
function pickerKey(e) { if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); closePicker(); } }
function openPicker() {
  if (picker) return;
  const cur = resolve(hashPath()).sec.id;
  picker = document.createElement('div');
  picker.className = 'sp-picker';
  picker.setAttribute('role', 'dialog'); picker.setAttribute('aria-modal', 'true'); picker.setAttribute('aria-label', t('settings.aria_label_sections', 'Settings sections'));
  picker.innerHTML = '<div class="sp-pbar"><b>' + t('settings.tab_label', 'Settings') + '</b><button type="button" class="btn icon" aria-label="' + t('settings.close', 'Close') + '" data-pclose>' + ic('x') + '</button></div><div class="sp-plist">' +
    GROUPS.map(g => '<div class="sp-group">' + GROUP_NAME[g] + '</div>' + SECTIONS.filter(s => s.group === g).map(s =>
      '<a class="sp-prow' + (s.id === cur ? ' on' : '') + '" href="#settings/' + s.id + '/' + s.subs[0].id + '"><span><b>' + esc(s.label) + '</b><small>' + esc(s.desc) + '</small></span><span class="sp-pchev" aria-hidden="true">' + ic('down') + '</span></a>').join('')).join('') + '</div>';
  document.body.appendChild(picker);
  pickerRelease = trapSheet(picker);
  picker.querySelector('[data-pclose]').onclick = closePicker;
  picker.addEventListener('click', (e) => {
    const a = e.target.closest('a.sp-prow');
    if (!a) return;
    e.preventDefault();
    const to = a.getAttribute('href').slice('#settings/'.length);
    // (The picker's history step is taken back first, so it does not undo the page change.)
    afterOverlayClose(closePicker).then(() => goSettings(to));
  });
  document.addEventListener('keydown', pickerKey, true);
  const first = picker.querySelector('.sp-prow.on') || picker.querySelector('.sp-prow');
  if (first) first.focus();
  overlaysChanged();
}
backLayer(25, () => !!picker, closePicker);

function render() {
  if (!shell) return;
  const { sec, sub } = resolve(hashPath());
  const route = sec.id + '/' + sub.id;
  if (route === rendered) return;
  if (cleanup) { try { cleanup(); } catch (e) { /* page already gone */ } cleanup = null; }
  clearUnsaved(shell);
  rendered = route; lastRoute = route;
  if (hashPath() !== route) history.replaceState(null, '', '#settings/' + route);
  const my = ++token;
  shell.querySelectorAll('.sp-item').forEach(a => { const on = a.dataset.sec === sec.id; a.classList.toggle('on', on); if (on) { a.setAttribute('aria-current', 'page'); try { a.scrollIntoView({ block: 'nearest', inline: 'nearest' }); } catch (e) { /* old browsers */ } } else a.removeAttribute('aria-current'); });
  shell.querySelector('h2').textContent = sec.label;
  shell.querySelector('[data-sectname]').textContent = sec.label;
  shell.querySelector('.helpq').dataset.topic = helpTopic(sec, sub);
  shell.querySelector('.sp-desc').textContent = sec.desc;
  const subs = shell.querySelector('.sp-subs');
  subs.hidden = sec.subs.length < 2;
  subs.innerHTML = sec.subs.map(x => '<button type="button" role="tab" class="chip' + (x.id === sub.id ? ' on' : '') + '" aria-selected="' + (x.id === sub.id) + '" data-sub="' + sec.id + '/' + x.id + '">' + esc(x.label) + '</button>').join('');
  const body = shell.querySelector('.sp-body');
  body.className = 'sp-body' + (sub.wide ? ' wide' : '');
  body.innerHTML = '';
  body.scrollTop = 0;
  window.scrollTo(0, 0);
  // Each page gets its own host: a slow page that finishes after the user moved on writes into a
  // detached node (cleanup removes it) instead of the page now showing.
  const host = document.createElement('div');
  body.appendChild(host);
  const unhost = () => host.remove();
  if (sub.page) {
    const p = adminPage(sub.page);
    if (!p) { host.innerHTML = '<p class="empty">' + t('settings.page_unavailable','This page is not available.') + '</p>'; return; }
    p.render(host);
    cleanup = () => { try { if (p.leave) p.leave(); } finally { unhost(); } };
  } else {
    const r = sub.render(host);
    if (my !== token) { if (typeof r === 'function') { try { r(); } catch (e) { /* gone */ } } return; }
    cleanup = () => { try { if (typeof r === 'function') r(); } finally { unhost(); } };
  }
}

// Go to a route, e.g. goSettings('detection/alerts'); old routes work too. Adds a history entry
// (back button works). Unsaved fields on the page being left ask first.
export async function goSettings(path) {
  const { sec, sub } = resolve(path);
  const route = sec.id + '/' + sub.id;
  const target = '#settings/' + route;
  if (isActive('settings')) {
    if (route === rendered) return;
    if (!(await confirmLeave())) return;
    if (location.hash !== target) history.pushState(null, '', target);
    render();
  } else {
    if (location.hash !== target) history.pushState(null, '', target);
    showTab('settings', true);
  }
}

// Kept for the places that used to open the settings dialog: pane = 'cameras', 'motion',
// 'storage', 'alerts', 'odt', 'watchdog', 'servers', 'groups'.
const OLD_PANES = { storage: 'recording/retention', alerts: 'detection/alerts', odt: 'connections/detector', motion: 'detection/motion', cameras: 'cameras/list' };
export function openSettingsDialog(pane) {
  if (!state.isAdmin) { toast(t('settings.view_only','View-only account: settings require an admin sign-in'), { kind: 'err' }); return; }
  goSettings(OLD_PANES[pane] || pane || lastRoute || 'cameras/list');
}

export const settingsTab = {
  id: 'settings', label: t('settings.tab_label','Settings'), order: 50, admin: true,
  init(el) { build(el); },
  show() { rendered = ''; syncTop(); render(); },
  hide() { if (cleanup) { try { cleanup(); } catch (e) { /* gone */ } cleanup = null; } clearUnsaved(shell); closePicker(); rendered = ''; token++; },
  // nav.js asks this to put the address back when leaving was cancelled.
  leaveHash: () => (rendered ? '#settings/' + rendered : ''),
};

// The header can wrap on a phone: keep the sticky rail just under it.
function syncTop() { const h = document.querySelector('header'); if (h) document.documentElement.style.setProperty('--sp-top', h.offsetHeight + 'px'); }

export function initSettings() {
  window.addEventListener('resize', syncTop); syncTop();
  // Back / forward and edited hashes while Settings is showing.
  window.addEventListener('hashchange', async () => {
    if (!isActive('settings') || !location.hash.startsWith('#settings')) return;
    const { sec, sub } = resolve(hashPath());
    if (sec.id + '/' + sub.id === rendered) return;
    if (hasUnsaved()) {
      const to = location.hash;
      history.replaceState(null, '', '#settings/' + rendered);
      if (!(await confirmLeave())) return;
      history.replaceState(null, '', to);
    }
    render();
  });
}
