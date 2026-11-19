// cam_controls.js - the camera window's "Lights & siren" section (admins): what the camera has
// (asked of the camera), its infrared and status lights, and the actions next to alerts (the
// spotlight or the siren when the AI confirms a person). The siren and the light themselves are
// in Live (controls.js).

import { t } from './i18n.js';
import { api, esc, toast, errText } from './core.js';

const sel = (k, opts, val) => '<select data-k="' + k + '">' + opts.map(([v, t]) => '<option value="' + esc(v) + '"' + (String(v) === String(val) ? ' selected' : '') + '>' + esc(t) + '</option>').join('') + '</select>';
const field = (label, input, hint) => '<label class="cs-f"><span>' + label + '</span>' + input + '</label>' + (hint ? '<div class="hint">' + hint + '</div>' : '');
const sw = (attr, on, title, small) => '<label class="sw"><input type="checkbox" role="switch" ' + attr + (on ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
  '<span class="sw-text"><b>' + title + '</b>' + (small ? '<small>' + small + '</small>' : '') + '</span></label>';
// (Not a form field: changed on the camera at once, not with Save.)
const live = (k, opts, val) => '<select data-nosig data-live-' + k + '>' + opts.map(([v, t]) => '<option value="' + esc(v) + '"' + (String(v) === String(val) ? ' selected' : '') + '>' + esc(t) + '</option>').join('') + '</select>';
const num = (k, v, attrs) => '<input data-k="' + k + '" type="number" ' + attrs + ' value="' + (v == null ? '' : v) + '">';

export function controlsHtml(c) {
  const k = c.controls_cfg || {};
  const basic =
    '<div class="hint" style="margin-top:0">' + t('cam_controls.basic_hint','A camera\'s own spotlight and siren, and its infrared and status lights. Only what the camera has shows; it is asked of the camera (Reolink cameras so far). The light and the siren themselves are in Live: a camera\'s button, the full view, or the View panel.') + '</div>' +
    '<div class="wd-state" data-ctlstate><span class="hint">' + t('cam_controls.asking','Asking the camera…') + '</span></div>' +
    '<div class="cs-ctl-ab" data-ctlab></div>' +
    '<div data-ctlset></div>' +
    '<div class="sec">' + t('cam_controls.actions_next','Actions next to alerts') + '</div>' +
    '<div class="hint">' + t('cam_controls.when_ai','When the AI confirms what you pick below on this camera (not motion alone, not the camera\'s own report alone). Off by default.') + '</div>' +
    sw('data-ct-light', !!k.light_on_detect, t('cam_controls.turn_spotlight','Turn the spotlight on'), t('cam_controls.stays_on','Stays on while it is still seen, then the camera gets its own setting back.')) +
    field(t('cam_controls.for_minutes','For (minutes)'), num('ct_lmin', k.light_minutes || 2, 'min="1" max="30"')) +
    sw('data-ct-lnight', k.light_night_only !== false, t('cam_controls.only_at_night','Only at night'), t('cam_controls.between_sunset','Between sunset and sunrise when the location is set (Settings, Profiles), else 19:00 to 07:00.')) +
    sw('data-ct-siren', !!k.siren_on_detect, t('cam_controls.sound_siren','Sound the siren while armed'), t('cam_controls.only_while_armed','Only while AmberSight is armed for this camera. At most once per pause below, and 3 times an hour.')) +
    field(t('cam_controls.siren_for_seconds','Siren for (seconds)'), num('ct_ssec', k.siren_seconds || 5, 'min="1" max="30"')) +
    field(t('cam_controls.pause_between_sirens','Pause between sirens (minutes)'), num('ct_scool', k.siren_cooldown_minutes || 10, 'min="1" max="1440"')) +
    sw('data-ct-snight', !!k.siren_night_only, t('cam_controls.siren_only_at_night','Siren only at night')) +
    field(t('cam_controls.react_to','React to'), '<input data-k="ct_labels" value="' + esc((k.labels && k.labels.length ? k.labels : ['person']).join(', ')) + '" placeholder="person" spellcheck="false">', t('cam_controls.objects_hint','Objects the AI must confirm, comma separated (person, car, dog...).'));
  const more =
    field(t('cam_controls.controls_label','Controls'), sel('ct_kind', [['auto', t('cam_controls.auto_option','Automatic (Reolink cameras are found by their stream address)')], ['reolink', t('cam_controls.reolink_option','Reolink')], ['off', t('cam_controls.off_option','Off: no controls for this camera')]], k.kind || 'auto')) +
    field(t('cam_controls.web_port','Web port (optional)'), num('ct_port', k.http_port, 'min="1" max="65535" placeholder="443, then 80"')) +
    field(t('cam_controls.connect_over','Connect over'), sel('ct_scheme', [['https', 'HTTPS'], ['http', t('cam_controls.http_option','HTTP (sends the camera\'s password unencrypted)')], ['baichuan', t('cam_controls.port9000_option', 'Reolink port 9000 (for cameras with no web interface)')]], k.scheme === 'http' || k.scheme === 'baichuan' ? k.scheme : 'https'),
      t('cam_controls.https_hint_9000', 'HTTPS unless the camera has only plain HTTP on: over HTTP its password crosses the network unencrypted. Reolink port 9000 is for Reolink cameras whose web interface is off, such as the E1 Pro: the siren, the infrared lights and the status light. It is not for battery cameras or cameras behind a hub. When the camera\'s video setting “Connect with” is Reolink port 9000, or Auto, port 9000 is used on its own when the web interface does not answer.'));
  return [basic, more];
}

// The section's settings (undefined when the window has none).
export function readControls(d) {
  const light = d.querySelector('[data-ct-light]');
  if (!light) return undefined;
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const chk = (a) => d.querySelector('[' + a + ']').checked;
  const n = (k, lo, hi, dflt) => { const x = Math.round(Number(v(k))); return Number.isFinite(x) && v(k) !== '' ? Math.min(hi, Math.max(lo, x)) : dflt; };
  const labels = v('ct_labels').split(',').map(s => s.trim().toLowerCase()).filter(Boolean).slice(0, 20);
  const o = {
    kind: v('ct_kind') === 'auto' ? null : v('ct_kind'),
    http_port: v('ct_port') ? n('ct_port', 1, 65535, null) : null,
    scheme: v('ct_scheme') === 'http' || v('ct_scheme') === 'baichuan' ? v('ct_scheme') : null,
    light_on_detect: light.checked,
    light_minutes: n('ct_lmin', 1, 30, 2) === 2 ? null : n('ct_lmin', 1, 30, 2),
    light_night_only: chk('data-ct-lnight') ? null : false,
    siren_on_detect: chk('data-ct-siren'),
    siren_seconds: n('ct_ssec', 1, 30, 5) === 5 ? null : n('ct_ssec', 1, 30, 5),
    siren_cooldown_minutes: n('ct_scool', 1, 1440, 10) === 10 ? null : n('ct_scool', 1, 1440, 10),
    siren_night_only: chk('data-ct-snight') ? true : null,
    labels: labels.length === 1 && labels[0] === 'person' ? [] : labels,
  };
  const empty = Object.entries(o).every(([key, x]) => x == null || x === false || (Array.isArray(x) && !x.length) || key === '');
  return empty ? null : o;
}

export function summary(d) {
  const l = d.querySelector('[data-ct-light]'), s = d.querySelector('[data-ct-siren]');
  if (!l) return '';
  const parts = [];
  if (l.checked) parts.push(t('cam_controls.spotlight_on_detection','spotlight on detection'));
  if (s.checked) parts.push(t('cam_controls.siren_while_armed','siren while armed'));
  return t('cam_controls.actions_summary', 'Actions: {list}', { list: parts.length ? parts.join(', ') : t('cam_controls.none', 'none') });
}

export function wireControls(d, c) {
  if (!d.querySelector('[data-ctlstate]')) return;
  const post = async (body, what) => {
    try { paint(await api('/api/cameras/' + c.id + '/controls', { json: body })); toast(t('cam_controls.what_done', '{what}: done', { what }), { kind: 'ok', ms: 2000 }); }
    catch (e) { toast(t('cam_controls.what_failed', '{what}: {error}', { what, error: errText(e) }), { kind: 'bad' }); load(); }
  };
  function paint(v) {
    const box = d.querySelector('[data-ctlstate]');
    if (!box) return;
    if (!v.supported) {
      box.innerHTML = '<span class="badge">' + t('cam_controls.no_controls', 'No controls') + '</span><span class="hint">' + esc(v.why || '') + '</span>';
      d.querySelector('[data-ctlab]').innerHTML = '';
      d.querySelector('[data-ctlset]').innerHTML = '';
      return;
    }
    const st = v.state || {};
    const found = v.probed_ms ? t('cam_controls.asked', 'Asked {when}', { when: new Date(v.probed_ms).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' }) }) : t('cam_controls.not_asked', 'Not asked yet');
    box.innerHTML = '<span class="badge' + (v.error ? ' off' : ' on dot') + '">' + (v.error ? t('cam_controls.problem','Problem') : v.via === 'port9000' ? t('cam_controls.reolink_port9000', 'Reolink port 9000') : 'Reolink') + '</span><span class="hint">' + esc(v.error || (v.asleep ? t('cam_controls.asleep','Asleep: read the next time it is awake.') : found + '.')) + '</span>' +
      '<button type="button" class="btn" data-ctlredo title="' + t('cam_controls.ask_again_title','Ask the camera again what it has') + '">' + t('cam_controls.ask_again','Ask again') + '</button>';
    const ab = [];
    if (v.light) {
      const what = [v.light.brightness ? t('cam_controls.m_brightness', 'brightness') : null, v.light.smart ? t('cam_controls.m_smart', 'smart mode') : null, v.light.keep_on ? t('cam_controls.m_keep_on', 'keep-on mode') : null, v.light.schedule ? t('cam_controls.m_schedule', 'schedule mode') : null].filter(Boolean);
      ab.push(t('cam_controls.spotlight','Spotlight') + (what.length ? ' (' + what.join(', ') + ')' : ''));
    }
    if (v.siren) ab.push(t('cam_controls.siren','Siren'));
    if (v.ir) ab.push(t('cam_controls.infrared_lights','Infrared lights'));
    if (v.status_led) ab.push(t('cam_controls.status_light','Status light'));
    if (v.doorbell_led) ab.push(t('cam_controls.doorbell_light','Doorbell light'));
    if (v.light_unswitched) ab.push(t('cam_controls.spotlight_unswitched', 'Spotlight (not switched over port 9000 yet)'));
    d.querySelector('[data-ctlab]').innerHTML = ab.length ? ab.map(x => '<span class="badge">' + esc(x) + '</span>').join('') : '<span class="hint">' + t('cam_controls.none_of_these','The camera has none of these.') + '</span>';
    let h = '';
    // The camera's own words for a state, shown translated (the value sent back stays the camera's word).
    const WORD = { On: t('cam_controls.db_on', 'On'), Off: t('cam_controls.db_off', 'Off'), Auto: t('cam_controls.state_auto', 'Auto'), on: t('cam_controls.light_on_lc', 'on'), off: t('cam_controls.light_off_lc', 'off') };
    const word = (x) => WORD[x] || x;
    if (v.light) {
      // The camera names none of its modes: shown as its own setting, never guessed.
      h += '<div class="hint">' + t('cam_controls.spotlight_state', 'Spotlight now: {light} · mode: {mode}{code}. Turning it on from AmberSight gives it this back afterwards.', { light: esc(word(st.light)), mode: esc(st.mode_label || t('cam_controls.own_setting', 'Camera\'s own setting')), code: st.mode != null ? ' (' + esc(String(st.mode)) + ')' : '' }) + '</div>';
    }
    if (v.ir) h += field(t('cam_controls.infrared_lights_label','Infrared lights'), live('ir', [['', t('cam_controls.unknown', 'Unknown')]].filter(() => st.ir === 'unknown').concat(v.ir.states.map(s => [s, word(s)])), st.ir === 'unknown' ? '' : st.ir), t('cam_controls.changed_camera','Changed on the camera right away.'));
    if (v.status_led) h += field(t('cam_controls.status_light_label','Status light'), live('led', [['', t('cam_controls.unknown', 'Unknown')]].filter(() => st.status_led === 'unknown').concat(v.status_led.states.map(s => [s, word(s)])), st.status_led === 'unknown' ? '' : st.status_led), t('cam_controls.changed_camera','Changed on the camera right away.'));
    // The doorbell's own words for its light, in plain ones.
    const DB = { On: t('cam_controls.db_on', 'On'), Off: t('cam_controls.db_off', 'Off'), KeepOn: t('cam_controls.db_always_on', 'Always on'), KeepOff: t('cam_controls.db_always_off', 'Always off') };
    if (v.doorbell_led) h += field(t('cam_controls.doorbell_light_label','Doorbell light'), live('db', [['', t('cam_controls.unknown', 'Unknown')]].filter(() => st.doorbell_led === 'unknown').concat(v.doorbell_led.states.map(s => [s, DB[s] || s])), st.doorbell_led === 'unknown' ? '' : st.doorbell_led), t('cam_controls.doorbell_hint','The light around the button. Changed on the doorbell right away.'));
    const set = d.querySelector('[data-ctlset]');
    set.innerHTML = h;
    const ir = set.querySelector('[data-live-ir]');
    if (ir) ir.onchange = () => { if (ir.value) post({ action: 'ir', state: ir.value }, t('cam_controls.infrared_lights_action','Infrared lights')); };
    const led = set.querySelector('[data-live-led]');
    if (led) led.onchange = () => { if (led.value) post({ action: 'status_led', state: led.value }, t('cam_controls.status_light_action','Status light')); };
    const db = set.querySelector('[data-live-db]');
    if (db) db.onchange = () => { if (db.value) post({ action: 'doorbell_led', state: db.value }, t('cam_controls.doorbell_light_action','Doorbell light')); };
    box.querySelector('[data-ctlredo]').onclick = () => post({ action: 'redetect' }, t('cam_controls.asked_again','Asked again'));
  }
  async function load() {
    try { paint(await api('/api/cameras/' + c.id + '/controls')); }
    catch (e) { const box = d.querySelector('[data-ctlstate]'); if (box) box.innerHTML = '<span class="hint">' + esc(errText(e)) + '</span>'; }
  }
  load();
}
