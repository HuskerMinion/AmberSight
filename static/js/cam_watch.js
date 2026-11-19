// cam_watch.js - the camera window's "Tamper" and "Unusual activity" sections.
// Tamper: a covered, blurred or moved camera (reference pictures, state, recent
// alerts with reference vs current). Unusual: flags events that are out of the
// camera's usual pattern for that hour of the week.

import { t } from './i18n.js';
import { api, esc, openDlg, toast, objName, camFromId } from './core.js';
// A battery camera asleep right now (from the live camera list).
const asleep = (c) => { const s = camFromId(c.id); return !!(s && s.sleeping && !s.online); };

const sel = (k, opts, val) => '<select data-k="' + k + '">' + opts.map(([v, label]) => '<option value="' + v + '"' + (String(v) === String(val) ? ' selected' : '') + '>' + esc(label) + '</option>').join('') + '</select>';
const field = (label, input, hint) => '<label class="cs-f"><span>' + label + '</span>' + input + '</label>' + (hint ? '<div class="hint">' + hint + '</div>' : '');
const sw = (attr, on, title, small) => '<label class="sw"><input type="checkbox" role="switch" ' + attr + (on ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
  '<span class="sw-text"><b>' + title + '</b>' + (small ? '<small>' + small + '</small>' : '') + '</span></label>';
const when = (s) => new Date(s * 1000).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
const errText = (e) => { try { const j = JSON.parse(e.body); if (j && j.error) return j.error; } catch (x) { /* not JSON */ } return e.body || e.message; };

const SENS = [['1', t('cam_watch.sens_1', '1 - calm: only obvious cases')], ['2', '2'], ['3', t('cam_watch.sens_3', '3 - normal')], ['4', '4'], ['5', t('cam_watch.sens_5', '5 - touchy: small changes too')]];
// What the object labels are called on screen; the label itself is what the server sends and stores.
const labelName = (l) => objName(l);
const lightName = (l) => l === 'day' ? t('cam_watch.light_day', 'day') : l === 'night' ? t('cam_watch.light_night', 'night') : l;

// ---------------------------------------------------------------- tamper
export function tamperHtml(c) {
  const tp = c.tamper || {};
  return ['<div class="hint" style="margin-top:0">' + t('cam_watch.tamper_intro', 'Tells you when this camera is covered or blacked out (a cloth, tape, spray paint), blurred, or moved so that it no longer sees its usual view. AmberSight keeps a reference picture for the day (color) and one for the night (gray, infrared) and compares the camera\'s picture with them every few seconds. Headlights, sunrise, rain, the infrared switch and someone walking past do not count.') + '</div>' +
    sw('data-tp-en', !!tp.enabled, t('cam_watch.watch_tamper','Watch this camera for tampering',{}), t('cam_watch.tamper_off','Off by default. Alerts through this camera\'s notification settings, and shows as a Home Assistant sensor.',{})) +
    '<div class="wd-state" data-tpstate></div>' +
    '<div class="tp-pics" data-tppics></div>' +
    '<div class="row"><button type="button" class="btn" data-tpref>' + t('cam_watch.set_reference', 'Set reference now') + '</button><span class="hint" data-tpout></span></div>' +
    field(t('cam_watch.sensitivity','Sensitivity',{}), sel('tp_sens', SENS, String(tp.sensitivity || 3)), t('cam_watch.sensitivity_hint','Higher notices smaller changes (and gives more false alarms).',{})) +
    field(t('cam_watch.alert_seconds','Alert after it lasts (seconds)',{}), '<input data-k="tp_secs" type="number" min="10" max="3600" step="5" value="' + (tp.seconds || 60) + '">', t('cam_watch.alert_seconds_hint','The view must stay wrong this long. A restored view is reported once it has been right again for 30 seconds.',{})) +
    '<div class="sec">' + t('cam_watch.recent_alerts', 'Recent alerts') + '</div><div class="wd-acts" data-tpacts><div class="hint">' + t('cam_watch.none_yet', 'None yet.') + '</div></div>', ''];
}

export function readTamper(d) {
  const en = d.querySelector('[data-tp-en]');
  if (!en) return undefined;
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const o = { enabled: en.checked, sensitivity: Number(v('tp_sens')) || 3, seconds: Math.max(10, Math.min(3600, Math.round(Number(v('tp_secs')) || 60))) };
  return !o.enabled && o.sensitivity === 3 && o.seconds === 60 ? null : o;
}

const KIND_TEXT = { covered: t('cam_watch.covered','Covered or blacked out',{}), blurred: t('cam_watch.blurred','Blurred',{}), moved: t('cam_watch.moved','Moved',{}) };

function compareDlg(c, inc) {
  const key = 'Tamper ' + c.id + ' ' + inc.id;
  const base = '/api/cameras/' + c.id + '/tamper/incidents/' + inc.id;
  const d = openDlg((KIND_TEXT[inc.kind] || inc.kind) + ': ' + c.name,
    '<div class="hint">' + esc(when(inc.at)) + ' ' + (inc.cleared_at ? t('cam_watch.view_restored', '- view restored {when}', { when: esc(when(inc.cleared_at)) }) : t('cam_watch.not_restored', '- not restored yet')) + '</div>' +
    '<div class="tp-cmp"><figure><img alt="' + t('cam_watch.ref_alt', 'Reference picture') + '" src="' + base + '/ref"><figcaption>' + t('cam_watch.ref_caption', 'Reference ({light})', { light: esc(lightName(inc.light)) }) + '</figcaption></figure>' +
    '<figure><img alt="' + t('cam_watch.cur_alt', 'Picture when the alert was raised') + '" src="' + base + '/cur"><figcaption>' + t('cam_watch.cur_caption', 'When the alert was raised') + '</figcaption></figure></div>', true);
  d.dataset.key = key;
}

async function loadTamper(d, c) {
  let st;
  try { st = await api('/api/cameras/' + c.id + '/tamper'); } catch (e) { return; }
  const box = d.querySelector('[data-tpstate]');
  if (!box) return;
  const bad = ['covered', 'blurred', 'moved'].includes(st.state);
  const text = {
    off: t('cam_watch.state_off','Off.',{}),
    learning: t('cam_watch.state_learning','Taking reference pictures when the view is still. Nothing is judged until the picture for the current light (day or night) exists.',{}),
    ok: t('cam_watch.state_ok','The view matches its reference.',{}),
  }[st.state] || (st.active ? t('cam_watch.kind_since', '{kind} since {time}.', { kind: KIND_TEXT[st.state], time: new Date(st.active.since * 1000).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) }) : KIND_TEXT[st.state] + '.');
  box.innerHTML = '<span class="badge' + (bad ? ' off' : st.state === 'ok' ? ' on dot' : '') + '">' + esc(st.state === 'off' ? t('cam_watch.badge_off','Off',{}) : bad ? t('cam_watch.badge_tampering','Tampering',{}) : st.state === 'ok' ? t('cam_watch.badge_watching','Watching',{}) : t('cam_watch.badge_learning','Learning',{})) + '</span><span class="hint">' + esc(text) + '</span>';
  const pics = d.querySelector('[data-tppics]');
  const now = Date.now();
  const fig = (label, src, none) => '<figure>' + (none ? '<div class="tp-none">' + esc(none) + '</div>' : '<img alt="' + esc(label) + '" src="' + src + '">') + '<figcaption>' + esc(label) + (none ? '' : '') + '</figcaption></figure>';
  const ref = (l) => st.refs && st.refs[l] ? { src: '/api/cameras/' + c.id + '/tamper/ref/' + l + '?t=' + (st.refs[l].set_at || now) } : null;
  const day = ref('day'), night = ref('night');
  pics.innerHTML = fig(t('cam_watch.day_ref','Day reference',{}) + (st.refs && st.refs.day && st.refs.day.set_at ? ' (' + when(st.refs.day.set_at) + ')' : ''), day && day.src, day ? '' : t('cam_watch.not_taken','Not taken yet',{})) +
    fig(t('cam_watch.night_ref', 'Night reference') + (st.refs && st.refs.night && st.refs.night.set_at ? ' (' + when(st.refs.night.set_at) + ')' : ''), night && night.src, night ? '' : t('cam_watch.not_taken', 'Not taken yet')) +
    // (A battery camera asleep has no picture now: the server answers 503, so it is not asked.)
    fig(t('cam_watch.now','Now',{}), '/api/snapshot/' + c.id + '?t=' + now, asleep(c) ? t('cameras.asleep', 'Asleep (a battery camera saving power)') : '');
  const acts = d.querySelector('[data-tpacts]');
  if (acts && st.incidents.length) {
    acts.innerHTML = st.incidents.map(i => '<button type="button" class="wd-act tp-act" data-inc="' + i.id + '"><span class="muted">' + esc(when(i.at)) + '</span><span class="badge off">' + esc(KIND_TEXT[i.kind] || i.kind) + '</span><span>' + (i.cleared_at ? t('cam_watch.restored','restored ',{}) + esc(when(i.cleared_at)) : t('cam_watch.not_restored_btn','not restored',{})) + '</span><span class="muted">' + t('cam_watch.compare', 'compare') + '</span></button>').join('');
    acts.querySelectorAll('[data-inc]').forEach(b => { b.onclick = () => compareDlg(c, st.incidents.find(i => String(i.id) === b.dataset.inc)); });
  }
}

export function wireTamper(d, c, ctx) {
  if (!d.querySelector('[data-tp-en]')) return;
  const out = d.querySelector('[data-tpout]');
  d.querySelector('[data-tpref]').onclick = async () => {
    out.textContent = t('cam_watch.taking_it','Taking it...',{});
    try {
      const r = await api('/api/cameras/' + c.id + '/tamper/reference', { json: {} });
      out.textContent = t('cam_watch.ref_set', 'The {light} reference is set from the picture now.', { light: lightName(r.light) });
      loadTamper(d, c);
    } catch (e) { out.textContent = errText(e); }
  };
  autoSave(d, c, ctx, 'tamper', '[data-tp-en]', () => readTamper(d), () => loadTamper(d, c));
  loadTamper(d, c);
  const timer = setInterval(() => { if (d.isConnected && d.__inView && d.__inView('tamper')) loadTamper(d, c); }, 5000);
  return () => clearInterval(timer);
}

// A switch saves the section when flipped (the other fields of the section go with it).
function autoSave(d, c, ctx, key, swSel, read, after) {
  const el = d.querySelector(swSel);
  const section = el.closest('.cs-sec');
  el.addEventListener('change', async () => {
    const body = {}; body[key] = read();
    try {
      await api('/api/cameras/' + c.id, { method: 'PUT', json: body });
      toast(el.checked ? t('cam_watch.turned_on','Turned on for {name}',{ name: c.name }) : t('cam_watch.turned_off','Turned off for {name}',{ name: c.name }), { kind: 'ok' });
      ctx.rebase(section);
      if (after) after();
    } catch (e) {
      el.checked = !el.checked;
      toast(errText(e), { kind: 'error' });
    }
  });
}

// ---------------------------------------------------------------- unusual activity
const COMMON = ['person', 'car', 'truck', 'dog', 'cat', 'package', 'motion'];
const MODES = [['tag', t('cam_watch.mode_tag', 'Only mark the event "unusual"')], ['notice', t('cam_watch.mode_notice', 'Quiet notice (low priority, "Unusual: ...")')], ['alert', t('cam_watch.mode_alert', 'Full alert (high priority, "Unusual: ...")')]];
const DAYS = [t('cam_watch.day_mon', 'Mon'), t('cam_watch.day_tue', 'Tue'), t('cam_watch.day_wed', 'Wed'), t('cam_watch.day_thu', 'Thu'), t('cam_watch.day_fri', 'Fri'), t('cam_watch.day_sat', 'Sat'), t('cam_watch.day_sun', 'Sun')];
const modeName = (m) => m === 'tag' ? t('cam_watch.m_tag', 'tag') : m === 'notice' ? t('cam_watch.m_notice', 'notice') : m === 'alert' ? t('cam_watch.m_alert', 'alert') : m;

export function unusualHtml(c) {
  const u = c.unusual || {};
  const mine = u.labels && u.labels.length ? u.labels : ['person'];
  const labels = [...new Set(COMMON.concat(mine))];
  return ['<div class="hint" style="margin-top:0">' + t('cam_watch.unusual_intro', 'Learns how much activity normally happens at this camera for each hour of the week (from its event history) and flags events that are out of pattern, such as a person at the back gate at 3 AM when that hour almost never has anyone. Flagged events get the label <b>unusual</b> (a chip in Events) and say "Unusual: person at Back Gate at 3:12 AM". It needs about two weeks of history before it judges anything.') + '</div>' +
    sw('data-un-en', !!u.enabled, t('cam_watch.flag_unusual','Flag unusual activity on this camera',{}), t('cam_watch.unusual_off','Off by default.',{})) +
    '<div class="wd-state" data-unstate></div>' +
    '<div class="cs-f"><span>' + t('cam_watch.labels_judged', 'Labels it judges') + '</span><div class="cs-groups" data-ulist>' +
    labels.map(l => '<button type="button" class="chip' + (mine.includes(l) ? ' on' : '') + '" data-sn="' + esc(l) + '" aria-pressed="' + mine.includes(l) + '">' + esc(labelName(l)) + '</button>').join('') +
    '</div></div><div class="hint">' + t('cam_watch.motion_note', '"motion" is an event with no object recognized (needs no AI). Other labels are the ones AI reports for this camera.') + '</div>' +
    field(t('cam_watch.un_sensitivity','Sensitivity',{}), sel('un_sens', SENS, String(u.sensitivity || 3)), t('cam_watch.un_sensitivity_hint','Higher flags activity at hours that are only a little uncommon. 1 flags only what has never happened at that hour.',{})) +
    field(t('cam_watch.what_it_does','What it does',{}), sel('un_mode', MODES, u.mode || 'notice'), t('cam_watch.what_it_does_hint','The notification follows this camera\'s other alert settings (services, hours, pause). A quiet notice has low priority.',{})) +
    '<div class="sec">' + t('cam_watch.what_normal', 'What is normal here') + '</div><div class="hint">' + t('cam_watch.share_days', 'Share of days with this label in each hour of the week (darker = more usual).') + '</div>' +
    '<div class="row"><select data-k="un_show" aria-label="' + t('cam_watch.label_shown', 'Label shown') + '"></select></div><div class="un-grid" data-ungrid></div>',
    field(t('cam_watch.days_needed','Days of history needed',{}), '<input data-k="un_days" type="number" min="3" max="90" step="1" value="' + (u.min_days || 14) + '">', t('cam_watch.days_needed_hint','Nothing is flagged before this many days of events are known.',{}))];
}

export function readUnusual(d) {
  const en = d.querySelector('[data-un-en]');
  if (!en) return undefined;
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const labels = [...d.querySelectorAll('[data-ulist] .chip.on')].map(b => b.dataset.sn);
  const o = { enabled: en.checked, sensitivity: Number(v('un_sens')) || 3, labels: labels.length ? labels : ['person'], mode: v('un_mode') || 'notice', min_days: Math.max(3, Math.min(90, Math.round(Number(v('un_days')) || 14))) };
  return !o.enabled && o.sensitivity === 3 && o.labels.join() === 'person' && o.mode === 'notice' && o.min_days === 14 ? null : o;
}

async function loadUnusual(d, c, st) {
  try { st = st || await api('/api/cameras/' + c.id + '/unusual'); } catch (e) { return; }
  const box = d.querySelector('[data-unstate]');
  if (!box) return;
  const txt = st.state === 'off' ? t('cam_watch.state_off', 'Off.') : st.state === 'ready' ? t('cam_watch.un_ready', 'Ready: the pattern is built from {days} days.', { days: st.days }) : t('cam_watch.un_learning', 'Learning: {days} of {needed} days of history{extra}. Nothing is flagged yet.', { days: st.days, needed: st.days_needed, extra: st.active_hours < 10 ? ' ' + t('cam_watch.un_little', '(and not enough activity yet)') : '' });
  box.innerHTML = '<span class="badge' + (st.state === 'ready' ? ' on dot' : '') + '">' + esc(st.state === 'off' ? t('cam_watch.badge_off', 'Off') : st.state === 'ready' ? t('cam_watch.badge_ready', 'Ready') : t('cam_watch.badge_learning', 'Learning')) + '</span><span class="hint">' + esc(txt) + '</span>' +
    '<button type="button" class="btn" data-unrefresh title="' + t('cam_watch.unrefresh_title', 'Read the latest events into the history now (done hourly anyway)') + '">' + t('cam_watch.update_now', 'Update now') + '</button>';
  box.querySelector('[data-unrefresh]').onclick = async () => {
    try { loadUnusual(d, c, await api('/api/cameras/' + c.id + '/unusual/refresh', { json: {} })); } catch (e) { toast(errText(e), { kind: 'error' }); }
  };
  const pick = d.querySelector('[data-k="un_show"]');
  const labels = Object.keys(st.typical || {});
  const keep = pick.value;
  pick.innerHTML = labels.map(l => '<option value="' + esc(l) + '">' + esc(labelName(l)) + '</option>').join('');
  if (labels.includes(keep)) pick.value = keep;
  const draw = () => {
    const week = (st.typical || {})[pick.value] || [];
    let h = '<div class="un-row un-hd"><span></span>' + [0, 6, 12, 18].map(x => '<span style="grid-column:' + (x + 2) + '/span 6">' + (x < 10 ? '0' : '') + x + ':00</span>').join('') + '</div>';
    for (let wd = 0; wd < 7; wd++) {
      h += '<div class="un-row"><span class="un-day">' + DAYS[wd] + '</span>';
      for (let hr = 0; hr < 24; hr++) {
        const s = week[wd * 24 + hr] || 0;
        h += '<i style="--p:' + Math.round(Math.min(1, s) * 100) + '%" title="' + t('cam_watch.cell_tip', '{day} {hr}:00 - {pct}% of days', { day: DAYS[wd], hr, pct: Math.round(s * 100) }) + '"></i>';
      }
      h += '</div>';
    }
    d.querySelector('[data-ungrid]').innerHTML = labels.length ? h : '<div class="hint">' + t('cam_watch.nothing_to_show', 'Nothing to show yet.') + '</div>';
  };
  pick.onchange = draw;
  draw();
}

export function wireUnusual(d, c, ctx) {
  if (!d.querySelector('[data-un-en]')) return;
  const box = d.querySelector('[data-ulist]');
  box.onclick = (e) => { const b = e.target.closest('.chip'); if (!b) return; b.classList.toggle('on'); b.setAttribute('aria-pressed', String(b.classList.contains('on'))); };
  autoSave(d, c, ctx, 'unusual', '[data-un-en]', () => readUnusual(d), () => loadUnusual(d, c));
  loadUnusual(d, c);
}

export function summaries(d) {
  const tp = readTamper(d), u = readUnusual(d);
  return {
    tamper: !d.querySelector('[data-tp-en]') ? null : tp && tp.enabled ? t('cam_watch.tamper_on', 'Tamper watch: on · sensitivity {sens} · after {secs} s', { sens: tp.sensitivity, secs: tp.seconds }) : t('cam_watch.tamper_off_sum', 'Tamper watch: off'),
    unusual: !d.querySelector('[data-un-en]') ? null : u && u.enabled ? t('cam_watch.unusual_on', 'Unusual activity: on · {labels} · {mode}', { labels: u.labels.map(labelName).join(', '), mode: modeName(u.mode) }) : t('cam_watch.unusual_off_sum', 'Unusual activity: off'),
  };
}
