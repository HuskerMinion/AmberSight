// controls.js - a camera's own lights and siren in Live: the spotlight on for a while (with its
// brightness), then back to the camera's own setting, and the siren behind a confirm. Shown as a
// small panel: in a popover from a tile's or the full view's button, and in the View panel.
// Only what the camera has, and only what this account may use (the server checks again).

import { t } from './i18n.js';
import { api, esc, ic, toast, camFromId, can, openDlg, afterOverlayClose, errText, refreshCameras } from './core.js';

const DURS = [[60, t('controls.dur_1m', '1 min')], [300, t('controls.dur_5m', '5 min')], [900, t('controls.dur_15m', '15 min')], [3600, t('controls.dur_1h', '1 hour')]];
const SIREN_DURS = [5, 10, 30];
const PREF = 'ambs.ctl';
const pref = () => { try { return JSON.parse(localStorage.getItem(PREF) || '{}') || {}; } catch (e) { return {}; } };
const setPref = (k, v) => { try { const p = pref(); p[k] = v; localStorage.setItem(PREF, JSON.stringify(p)); } catch (e) { /* not remembered */ } };

// The camera has a spotlight or a siren (as last known by the server; asks nothing).
export const hasControls = (c) => !!(c && !c.remote && c.controls && (c.controls.light || c.controls.siren));
// This account may use something of it.
export const mayControls = (c) => hasControls(c) && ((c.controls.light && can('lights')) || (c.controls.siren && can('siren')));
// The spotlight is on now: turned on from here, or on as last read.
export const lightOn = (c) => !!(c && c.controls && ((c.controls.forced_until_ms && c.controls.forced_until_ms > Date.now()) || c.controls.light_on));

const left = (ms) => {
  const s = Math.max(0, Math.round((ms - Date.now()) / 1000));
  return s >= 60 ? Math.floor(s / 60) + ':' + String(s % 60).padStart(2, '0') : t('controls.secs', '{n} s', { n: s });
};

// One camera's panel in `box`; returns a function that stops it (its countdown).
export function mountPanel(box, id) {
  let data = null, busy = false, timer = null, dead = false;
  const post = async (body, what) => {
    if (busy) return;
    busy = true;
    paint();
    try {
      data = await api('/api/cameras/' + id + '/controls', { json: body });
      // Switched again within a second: the server does the latest when the second is over.
      if (data.queued) setTimeout(() => { if (!dead) load(); }, 1500);
      refreshCameras().catch(() => {});
    } catch (e) {
      toast(t('controls.failed', '{what}: {error}', { what, error: errText(e) }), { kind: 'bad' });
      await load();
    }
    busy = false;
    paint();
  };
  const load = async () => {
    try { data = await api('/api/cameras/' + id + '/controls'); } catch (e) { data = { supported: false, why: errText(e) }; }
    if (!dead) paint();
  };
  function paint() {
    if (dead) return;
    clearTimeout(timer);
    const c = camFromId(id);
    if (!data) { box.innerHTML = '<div class="ctl"><div class="ctl-note">' + esc(t('controls.asking', 'Asking the camera…')) + '</div></div>'; return; }
    if (!data.supported) { box.innerHTML = '<div class="ctl"><div class="ctl-note">' + esc(data.why || t('controls.no_controls', 'No controls for this camera.')) + '</div></div>'; return; }
    const st = data.state || {}, may = data.may || {};
    const now = Date.now();
    const forced = st.forced_until_ms && st.forced_until_ms > now;
    const playing = st.siren_until_ms && st.siren_until_ms > now;
    let h = '<div class="ctl' + (busy ? ' busy' : '') + '">';
    if (data.light) {
      const on = st.light === 'on';
      const word = st.light === 'unknown' ? t('controls.state_unknown', 'State unknown') : forced ? t('controls.on_for', 'On for {time} more, then back to the camera\'s setting', { time: left(st.forced_until_ms) }) : on ? t('controls.on_own', 'On (the camera\'s own setting)') : t('controls.off', 'Off');
      const b = data.light.brightness;
      const bv = pref()['b' + id] != null ? pref()['b' + id] : (st.brightness != null ? st.brightness : b ? b.max : 100);
      const dur = Number(pref().dur) || 300;
      h += '<label class="sw ctl-sw"><input type="checkbox" role="switch" data-ctl-light' + (on ? ' checked' : '') + (may.light && !busy ? '' : ' disabled') + '>' +
        '<span class="sw-track" aria-hidden="true"></span><span class="sw-text"><b>' + ic('sun') + esc(t('controls.spotlight', 'Spotlight')) + '</b><small data-ctl-lword>' + esc(word) + '</small></span></label>';
      if (may.light) {
        h += '<div class="ctl-grid">' +
          (b ? '<label class="ctl-f"><span>' + esc(t('controls.brightness', 'Brightness')) + '</span><input type="range" data-ctl-bright min="' + Math.max(1, b.min) + '" max="' + b.max + '" value="' + bv + '"' + (busy ? ' disabled' : '') + '><output data-ctl-bout>' + bv + '</output></label>' : '') +
          '<label class="ctl-f"><span>' + esc(t('controls.on_for_label', 'On for')) + '</span><select data-ctl-dur' + (busy ? ' disabled' : '') + '>' + DURS.map(([s, lbl]) => '<option value="' + s + '"' + (s === dur ? ' selected' : '') + '>' + lbl + '</option>').join('') + '</select></label>' +
          '</div>';
      }
    }
    if (data.siren && (may.siren || may.light)) {
      h += '<div class="ctl-siren">' + ic('alert') + '<span class="ctl-sname">' + esc(t('controls.siren', 'Siren')) + '</span>' +
        (playing ? '<span class="ctl-sstate">' + esc(t('controls.sounding', 'Sounding, {left}', { left: left(st.siren_until_ms) })) + '</span><button type="button" class="primary danger" data-ctl-sstop' + (busy ? ' disabled' : '') + '>' + esc(t('controls.stop', 'Stop')) + '</button>'
          : may.siren ? '<button type="button" class="btn danger" data-ctl-siren' + (busy ? ' disabled' : '') + '>' + esc(t('controls.sound_btn', 'Sound…')) + '</button>'
            : '<span class="ctl-sstate">' + esc(t('controls.may_not_sound', 'This account may not sound it')) + '</span>') + '</div>';
    }
    if (data.light_unswitched) h += '<div class="ctl-note">' + esc(t('controls.light_unswitched', 'This camera has a spotlight. AmberSight does not switch it over Reolink port 9000 yet.')) + '</div>';
    if (data.asleep) h += '<div class="ctl-note">' + esc(t('controls.asleep', 'Asleep: controls work while the camera is awake. They never wake it.')) + '</div>';
    if (st.note) h += '<div class="ctl-note">' + esc(st.note) + '</div>';
    if (data.error) h += '<div class="ctl-note warn">' + esc(data.error) + '</div>';
    else if (st.read_error) h += '<div class="ctl-note warn">' + esc(t('controls.read_error', 'Could not read its state: {error}', { error: st.read_error })) + '</div>';
    h += '</div>';
    box.innerHTML = h;
    const q = (s) => box.querySelector(s);
    const sw = q('[data-ctl-light]');
    if (sw) sw.onchange = () => {
      const bright = q('[data-ctl-bright]');
      if (sw.checked) post({ action: 'light_on', seconds: Number(q('[data-ctl-dur]').value) || 300, brightness: bright ? Number(bright.value) : undefined }, t('controls.spotlight', 'Spotlight'));
      else post({ action: 'light_off' }, t('controls.spotlight', 'Spotlight'));
    };
    const br = q('[data-ctl-bright]');
    if (br) {
      br.oninput = () => { q('[data-ctl-bout]').textContent = br.value; };
      br.onchange = () => {
        setPref('b' + id, Number(br.value));
        // On from here: the new brightness now (and the time starts again).
        if (forced) post({ action: 'light_on', seconds: Number(q('[data-ctl-dur]').value) || 300, brightness: Number(br.value) }, t('controls.spotlight', 'Spotlight'));
      };
    }
    const du = q('[data-ctl-dur]');
    if (du) du.onchange = () => setPref('dur', Number(du.value));
    const sb = q('[data-ctl-siren]');
    if (sb) sb.onclick = () => sirenConfirm(c ? c.name : t('controls.this_camera', 'this camera'), (secs) => post({ action: 'siren', seconds: secs }, t('controls.siren', 'Siren')));
    const ss = q('[data-ctl-sstop]');
    if (ss) ss.onclick = () => post({ action: 'siren_stop' }, t('controls.siren', 'Siren'));
    // The countdowns, and a fresh look when they run out.
    if (forced || playing) {
      const end = Math.min(forced ? st.forced_until_ms : Infinity, playing ? st.siren_until_ms : Infinity);
      timer = setTimeout(() => { if (Date.now() >= end) load(); else paint(); }, 1000);
    }
  }
  paint();
  load();
  return () => { dead = true; clearTimeout(timer); };
}

// "Sound the siren?" with how long; Cancel has the focus.
function sirenConfirm(name, go) {
  const last = SIREN_DURS.includes(Number(pref().siren)) ? Number(pref().siren) : 5;
  const d = openDlg(t('controls.siren_confirm_title', 'Sound the siren?'), '<p class="cfm-body">' + t('controls.siren_confirm_body', 'The siren on <b>{name}</b> sounds out loud, for everyone nearby. It stops by itself.', { name: esc(name) }) + '</p>' +
    '<label class="cs-f"><span>' + esc(t('controls.for', 'For')) + '</span><select data-sdur>' + SIREN_DURS.map(s => '<option value="' + s + '"' + (s === last ? ' selected' : '') + '>' + esc(t('controls.n_seconds', '{n} seconds', { n: s })) + '</option>').join('') + '</select></label>' +
    '<div class="row cfm-row"><button type="button" class="btn" data-no>' + esc(t('controls.cancel', 'Cancel')) + '</button><button type="button" class="primary danger" data-yes>' + ic('alert') + esc(t('controls.sound_siren', 'Sound siren')) + '</button></div>', true);
  const close = d.__close;
  let done = false;
  const finish = async (yes) => {
    if (done) return;
    done = true;
    const secs = Number(d.querySelector('[data-sdur]').value) || 5;
    await afterOverlayClose(close);
    if (yes) { setPref('siren', secs); go(secs); }
  };
  d.__close = () => finish(false);
  d.querySelector('[data-no]').onclick = () => finish(false);
  d.querySelector('[data-yes]').onclick = () => finish(true);
  try { d.querySelector('[data-no]').focus(); } catch (e) { /* gone */ }
}

// ---------- the popover (a tile's or the full view's button) ----------
let pop = null;
export function closePop() {
  if (!pop) return;
  pop.stop();
  pop.el.remove();
  document.removeEventListener('pointerdown', pop.away, true);
  document.removeEventListener('keydown', pop.esc, true);
  pop = null;
}
export function togglePop(anchor, id) {
  const again = pop && pop.anchor === anchor;
  closePop();
  if (again) return;
  const el = document.createElement('div');
  el.className = 'ctl-pop';
  el.setAttribute('role', 'dialog');
  el.setAttribute('aria-label', t('controls.lights_and_siren', 'Lights and siren'));
  document.body.appendChild(el);
  const stop = mountPanel(el, id);
  // Below the button, kept on the screen (its size does not change while it is open).
  const place = () => {
    const r = anchor.getBoundingClientRect(), w = el.offsetWidth, h = el.offsetHeight;
    const x = Math.min(Math.max(8, r.right - w), window.innerWidth - w - 8);
    const y = r.bottom + 6 + h > window.innerHeight - 8 ? Math.max(8, r.top - h - 6) : r.bottom + 6;
    el.style.left = x + 'px';
    el.style.top = y + 'px';
  };
  place();
  new ResizeObserver(place).observe(el);
  const away = (ev) => { if (!el.contains(ev.target) && !anchor.contains(ev.target) && !ev.target.closest('.dlg, .scrim')) closePop(); };
  const esc2 = (ev) => { if (ev.key === 'Escape' && !document.querySelector('.dlg')) { ev.stopPropagation(); closePop(); anchor.focus(); } };
  document.addEventListener('pointerdown', away, true);
  document.addEventListener('keydown', esc2, true);
  pop = { el, anchor, stop, away, esc: esc2 };
}

// A tile's or toolbar's button for camera `c` (null when there is nothing to offer).
export function controlsButton(c, cls) {
  if (!mayControls(c)) return null;
  const b = document.createElement('button');
  b.type = 'button';
  b.className = cls || 'btn glass';
  b.dataset.ctl = '1';
  b.title = t('controls.lights_and_siren', 'Lights and siren');
  b.setAttribute('aria-label', t('controls.lights_and_siren', 'Lights and siren'));
  b.innerHTML = ic('sun');
  b.classList.toggle('lit', lightOn(c));
  b.onclick = (ev) => { ev.stopPropagation(); togglePop(b, c.id); };
  return b;
}
