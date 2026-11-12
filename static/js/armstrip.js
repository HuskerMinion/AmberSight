// armstrip.js - "When does this camera watch?": a 7-day x 24 h strip (rows = days from today,
// columns = half hours) with three thin lanes per day: Recording, Motion detection, Alerts.
// Computed in the browser from what the UI already fetches; nothing is stored.
//
// RULES (mirrors src/engine/profiles.rs, worker.rs, alerts.rs, config.rs; keep in step):
//  Profile at a time: a manual hold (while `until` has not passed; "until changed" holds are
//    only used for NOW), else the first weekly rule that covers the time (days empty = every
//    day; from<=to: from<=min<to; past midnight: late part on the start day, early part on the
//    next day; sunrise/sunset[+-min] use TODAY's sun times for every day), else the default
//    profile, else none (cameras use their own settings).
//  Shield (armed): now = the current state. A profile with `armed` set applies it only when the
//    profile CHANGES (never repeated); so a later slot's shield = the last armed value of a profile
//    switch since now, else the current state (hand changes cannot be predicted).
//  Motion detection on = camera enabled AND (profile camera override `motion` ?? camera.motion
//    (or the sound trigger)) AND shield AND schedule_active(camera `schedule` ?? global [motion]
//    schedule). schedule_active is inclusive of both ends and may cross midnight (HH:MM-HH:MM).
//  Alerts sent = detection on AND profile `alerts` != false AND profile camera `alerts` != false
//    AND camera `alerts` != false AND inside the camera `alert_schedule` AND not paused.
//  Recording = camera enabled AND (override `record` ?? camera.record); "motion only" when
//    (override `record_motion_only` ?? camera.record_motion_only), else continuous. Recording is
//    NOT gated by the shield or windows (the shield only gates detection).
//  Each half hour is judged at its middle minute (the current one at the real minute).

import { esc } from './core.js';

const DAY = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
const KEYS = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];

const CSS = `
.as { margin: 4px 0 var(--s2); font-size: var(--fs-s); }
.as-hours, .as-row { display: grid; grid-template-columns: 34px 1fr; align-items: center; }
.as-hours { grid-template-columns: 34px repeat(24, 1fr); font-size: calc(11px * var(--ts, 1)); color: var(--muted); margin-bottom: 2px; }
.as-hours span { white-space: nowrap; overflow: visible; }
.as-row { margin: 3px 0; }
.as-day { font-size: calc(11px * var(--ts, 1)); color: var(--muted); }
.as-row.today .as-day { color: var(--accent); font-weight: 700; }
.as-lanes { display: grid; gap: 1px; border-radius: 4px; overflow: hidden; background: var(--surface2); position: relative; }
.as-lane { display: grid; grid-template-columns: repeat(48, 1fr); height: 6px; }
.as-lane i { display: block; min-width: 0; }
.as-lane i.rec.on { background: var(--info); }
.as-lane i.rec.mo { background: repeating-linear-gradient(135deg, var(--info) 0 2px, transparent 2px 4px); }
.as-lane i.det.on { background: var(--ok); }
.as-lane i.alr.on { background: var(--accent); }
.as-lane i.past { opacity: .45; }
.as-lane i.now { box-shadow: inset 0 0 0 1px var(--text); }
.as-lane i:hover { outline: 1px solid var(--text); outline-offset: -1px; }
.as-nowline { position: absolute; top: 0; bottom: 0; width: 2px; background: var(--text); pointer-events: none; opacity: .85; }
.as-info { min-height: 2.6em; margin: var(--s2) 0; padding: 6px 8px; border: 1px solid var(--line); border-radius: var(--r1); background: var(--surface2); color: var(--text); line-height: 1.35; overflow-wrap: anywhere; }
.as-leg { display: flex; flex-wrap: wrap; gap: 2px var(--s4); color: var(--muted); }
.as-leg span { display: inline-flex; align-items: center; gap: 6px; }
.as-leg b { display: inline-block; width: 14px; height: 6px; border-radius: 2px; }
.as-leg b.rec { background: var(--info); } .as-leg b.mo { background: repeating-linear-gradient(135deg, var(--info) 0 2px, transparent 2px 4px); }
.as-leg b.det { background: var(--ok); } .as-leg b.alr { background: var(--accent); }
.as-notes { margin: var(--s2) 0 0; padding-left: 16px; color: var(--muted); }
.as-notes li { margin: 2px 0; }
.ac-armrow { padding: 4px var(--s3) var(--s2); background: var(--surface2); }
.ac-armrow .as-info { background: var(--surface); }
.as-show { background: none; border: 0; color: var(--info); cursor: pointer; padding: 0 4px; font-size: inherit; text-decoration: underline; }
`;
function ensureCss() {
  if (document.getElementById('armStripCss')) return;
  const s = document.createElement('style');
  s.id = 'armStripCss'; s.textContent = CSS;
  document.head.appendChild(s);
}

const p2 = (n) => String(n).padStart(2, '0');
const hm = (t) => { const m = /^(\d{1,2}):(\d{2})$/.exec(String(t || '').trim()); return m && +m[1] < 24 && +m[2] < 60 ? +m[1] * 60 + +m[2] : null; };
// "HH:MM-HH:MM" -> [a, b] minutes (the server accepts any 0-23 hours); null = no/invalid window.
function parseWin(s) {
  if (!s || !String(s).trim()) return null;
  const p = String(s).trim().split('-');
  if (p.length < 2) return null;
  const a = hm(p[0]), b = hm(p[1]);
  return a == null || b == null ? null : [a, b];
}
const winActive = (w, m) => !w || (w[0] <= w[1] ? w[0] <= m && m <= w[1] : m >= w[0] || m <= w[1]);

function specMin(t, sun) {
  const s = String(t || '').trim().toLowerCase();
  const m = /^(sunrise|sunset)\s*([+-]\s*\d+)?$/.exec(s);
  if (!m) return hm(s);
  const b = sun ? hm(m[1] === 'sunrise' ? sun.sunrise : sun.sunset) : null;
  if (b == null) return null;
  const off = m[2] ? parseInt(m[2].replace(/\s/g, ''), 10) : 0;
  return (((b + off) % 1440) + 1440) % 1440;
}
function ruleMatches(rule, wd, minute, sun) {
  const from = specMin(rule.from, sun), to = specMin(rule.to, sun);
  if (from == null || to == null) return false;
  const on = (d) => !rule.days || !rule.days.length || rule.days.some(x => String(x).trim().toLowerCase().startsWith(KEYS[(d + 7) % 7]));
  if (from <= to) return on(wd) && minute >= from && minute < to;
  return (on(wd) && minute >= from) || (on(wd - 1) && minute < to);
}

// Fetch what the strip needs. Returns the `data` object armStrip wants.
export async function loadArmData(api) {
  const [s, live, st] = await Promise.all([api('/api/settings'), api('/api/profiles'), api('/api/status').catch(() => ({}))]);
  return armData(s, live, st);
}
// settings = GET /api/settings, live = GET /api/profiles, status = GET /api/status.
export function armData(settings, live, status) {
  const a = (settings && settings.automation) || {};
  return { automation: { profiles: a.profiles || [], schedule: a.schedule || [], default_profile: a.default_profile || '' },
    globalWindow: settings && settings.motion ? settings.motion.schedule || null : null,
    live: live || {}, armed: status && status.armed !== undefined ? !!status.armed : true };
}

// Work out all 7 x 48 slots.
export function computeArm(cam, data, now) {
  now = now || new Date();
  const auto = data.automation, live = data.live || {};
  const sun = live.sun || null;
  const profs = new Map((auto.profiles || []).map(p => [p.name, p]));
  const manual = live.manual || null;
  const camWin = parseWin(cam.schedule), glob = parseWin(data.globalWindow);
  const detWin = camWin || glob;
  const detWinTxt = camWin ? 'camera arm window ' + cam.schedule.trim() : glob ? 'shared arm window ' + String(data.globalWindow).trim() : null;
  const altWin = parseWin(cam.alert_schedule);
  const day0 = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const nowSlot = now.getHours() * 2 + (now.getMinutes() >= 30 ? 1 : 0);
  const nowIdx = nowSlot;
  const pauseUntil = cam.alerts_paused ? (cam.alerts_paused_until ? cam.alerts_paused_until * 1000 : Infinity) : 0;
  const slots = [];
  let shield = data.armed, shieldWhy = data.armed ? 'armed now' : 'disarmed now';
  let prevProf = null;
  for (let d = 0; d < 7; d++) {
    for (let k = 0; k < 48; k++) {
      const idx = d * 48 + k;
      const isNow = idx === nowIdx;
      const past = idx < nowIdx;
      const date = new Date(day0.getFullYear(), day0.getMonth(), day0.getDate() + d, 0, k * 30 + 15);
      let minute = k * 30 + 15;
      if (isNow) minute = now.getHours() * 60 + now.getMinutes();
      const wd = (date.getDay() + 6) % 7;
      // profile
      let pname, psrc;
      if (isNow) {
        pname = live.active || ''; psrc = manual ? 'set by hand' : live.scheduled ? 'schedule' : 'default';
      } else if (manual && manual.until && date.getTime() < manual.until * 1000 && !past) {
        pname = manual.name; psrc = 'set by hand until ' + new Date(manual.until * 1000).toLocaleString([], { weekday: 'short', hour: '2-digit', minute: '2-digit' });
      } else {
        const r = (auto.schedule || []).find(x => ruleMatches(x, wd, minute, sun));
        if (r) { pname = r.profile; psrc = 'rule ' + r.from + ' to ' + r.to; } else { pname = auto.default_profile || ''; psrc = pname ? 'default' : ''; }
      }
      const prof = profs.get(pname) || null;
      const ov = (prof && prof.cameras && prof.cameras[cam.name]) || {};
      // shield: only a profile CHANGE applies the profile's own arm setting
      let shieldNote = '';
      if (past || isNow) { prevProf = pname; } else {
        if (pname !== prevProf && prof && prof.armed != null) { shield = prof.armed; shieldWhy = pname + ' profile ' + (prof.armed ? 'arms' : 'disarms') + ' the system'; }
        prevProf = pname;
      }
      shieldNote = shieldWhy;
      const profTxt = pname ? pname + ' profile (' + psrc + ')' : 'no profile';
      // recording
      let rec, recWhy;
      if (cam.enabled === false) { rec = 'off'; recWhy = 'camera is disabled'; }
      else {
        const on = ov.record != null ? ov.record : cam.record !== false;
        const mo = ov.record_motion_only != null ? ov.record_motion_only : !!cam.record_motion_only;
        const via = (ov.record != null || ov.record_motion_only != null) ? pname + ' profile override' : 'camera setting';
        if (!on) { rec = 'off'; recWhy = 'recording off (' + via + ')'; }
        else if (mo) { rec = 'mo'; recWhy = 'records motion only (' + via + ')'; }
        else { rec = 'on'; recWhy = 'records continuously (' + via + ')'; }
      }
      // detection
      let det = false, detWhy;
      // cam.motion in /api/cameras is the live motion flag, so use motion_enabled (default on).
      const mOn = ov.motion != null ? ov.motion : cam.motion_enabled !== false;
      const sound = !!cam.audio_trigger;
      if (cam.enabled === false) detWhy = 'camera is disabled';
      else if (!mOn && !sound) detWhy = 'motion detection is off' + (ov.motion === false ? ' (' + pname + ' profile override)' : ' for this camera');
      else if (!shield) detWhy = 'disarmed (' + shieldNote + ')';
      else if (!winActive(detWin, minute)) detWhy = 'outside the ' + detWinTxt;
      else { det = true; detWhy = (mOn ? 'detects motion' : 'listens for loud sounds only') + (detWinTxt ? ' inside the ' + detWinTxt : ' all day') + ', armed'; }
      // alerts
      let alr = false, alrWhy;
      if (!det) alrWhy = 'nothing is detected, so nothing is sent';
      else if (prof && prof.alerts === false) alrWhy = pname + ' profile mutes alerts';
      else if (ov.alerts === false) alrWhy = pname + ' profile mutes alerts for this camera';
      else if (cam.alerts === false) alrWhy = 'notifications are off for this camera';
      else if (!winActive(altWin, minute)) alrWhy = 'outside the camera alert window ' + String(cam.alert_schedule).trim();
      else if (date.getTime() < pauseUntil) alrWhy = 'notifications are paused' + (pauseUntil === Infinity ? ' until resumed' : '');
      else { alr = true; alrWhy = 'alerts on' + ((cam.alert_labels || []).length ? ' for ' + cam.alert_labels.join(', ') + ' only' : '') + (altWin ? ' (camera alert window ' + String(cam.alert_schedule).trim() + ')' : ''); }
      slots.push({ d, k, isNow, past, rec, det, alr, profTxt,
        tip: { rec: recWhy, det: detWhy, alr: alrWhy } });
    }
  }
  return { slots, nowIdx, nowSlot, day0, today: (day0.getDay() + 6) % 7 };
}

function notes(cam, data) {
  const out = [], live = data.live || {};
  if (live.manual && !live.manual.until) out.push('The profile "' + live.manual.name + '" is held by hand until changed: that only counts for now; later times assume the weekly schedule.');
  else if (live.manual) out.push('A profile is held by hand for a while; after that the schedule resumes.');
  out.push('Arm and Disarm can be switched by hand at any time, so later times assume it stays as it is now unless a profile change sets it.');
  if ((data.automation.schedule || []).some(r => /sun/i.test(r.from + r.to))) out.push('Sunrise and sunset use today\'s times for every day (they drift by a minute or two per day).');
  if (cam.alerts_paused) out.push('Notifications for this camera are paused ' + (cam.alerts_paused_until ? 'until ' + new Date(cam.alerts_paused_until * 1000).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) : 'until resumed') + '.');
  return out;
}

// Draw into container. Returns { update(camera, data?) }.
export function armStrip(container, camera, data) {
  ensureCss();
  container.textContent = '';
  const root = document.createElement('div');
  root.className = 'as';
  container.appendChild(root);
  let cam = camera, dat = data, res = null, timer = 0;

  const info = () => root.querySelector('.as-info');
  function say(slot, lane) {
    const t = slot.k * 30;
    const when = DAY[(res.day0.getDay() + 6 + slot.d) % 7] + ' ' + p2(Math.floor(t / 60)) + ':' + p2(t % 60) + (slot.isNow ? ' (now)' : slot.past ? ' (earlier today, estimated)' : '');
    const names = { rec: 'Recording', det: 'Detection', alr: 'Alerts' };
    return when + ' · ' + slot.profTxt + ' · ' + names[lane] + ': ' + slot.tip[lane];
  }
  function draw() {
    res = computeArm(cam, dat);
    let h = '<div class="as-hours"><span></span>';
    for (let hr = 0; hr < 24; hr++) h += '<span>' + (hr % 3 === 0 ? p2(hr) : '') + '</span>';
    h += '</div>';
    for (let d = 0; d < 7; d++) {
      const lab = DAY[(res.day0.getDay() + 6 + d) % 7];
      h += '<div class="as-row' + (d === 0 ? ' today' : '') + '"><span class="as-day">' + (d === 0 ? 'Today' : lab) + '</span><div class="as-lanes">';
      for (const lane of ['rec', 'det', 'alr']) {
        h += '<div class="as-lane">';
        for (let k = 0; k < 48; k++) {
          const s = res.slots[d * 48 + k];
          const st = lane === 'rec' ? s.rec : (s[lane] ? 'on' : '');
          h += '<i class="' + lane + ' ' + st + (s.past ? ' past' : '') + (s.isNow ? ' now' : '') + '" data-i="' + (d * 48 + k) + '" data-l="' + lane + '" title="' + esc(say(s, lane)) + '"></i>';
        }
        h += '</div>';
      }
      if (d === 0) h += '<span class="as-nowline" style="left:' + (((res.nowSlot + (new Date().getMinutes() % 30) / 30) / 48) * 100).toFixed(2) + '%"></span>';
      h += '</div></div>';
    }
    h += '<div class="as-info" aria-live="polite"></div>' +
      '<div class="as-leg"><span><b class="rec"></b>recording</span><span><b class="mo"></b>recording on motion only</span><span><b class="det"></b>motion detection on</span><span><b class="alr"></b>alerts sent</span></div>' +
      '<ul class="as-notes">' + notes(cam, dat).map(n => '<li>' + esc(n) + '</li>').join('') + '</ul>';
    root.innerHTML = h;
    const now = res.slots[res.nowIdx];
    info().textContent = 'Now: ' + now.profTxt + ' · Recording: ' + now.tip.rec + ' · Detection: ' + now.tip.det + ' · Alerts: ' + now.tip.alr + '. Hover or tap a cell for the reason.';
  }
  const pick = (e) => {
    const c = e.target.closest('i[data-i]');
    if (c) info().textContent = say(res.slots[+c.dataset.i], c.dataset.l);
  };
  root.addEventListener('mouseover', pick);
  root.addEventListener('click', pick);
  draw();
  timer = setInterval(() => { if (!root.isConnected) clearInterval(timer); else draw(); }, 60000);
  return {
    update(c, d) { cam = c || cam; dat = d || dat; if (root.isConnected) draw(); },
    destroy() { clearInterval(timer); root.remove(); },
  };
}
