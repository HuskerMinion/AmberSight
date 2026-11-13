// cameras.js - camera settings: the per-camera settings side panel (opened from a tile's
// gear or right-click menu, or from Settings > Cameras), the camera list in Settings,
// and adding a camera. PUT /api/cameras/:id merges, so fields this window does not
// show (zones, ONVIF port, folder ...) keep their stored values.

import { state, bus, api, esc, ic, msgDlg, toast, openDlg, raiseDlg, refreshCameras, camFromId, backLayer, overlaysChanged, topDlgOpen, paletteOpen } from './core.js';
import { helperHtml, wireHelper, readHelper, pairDialog, helpersListHtml } from './helperpick.js';
import { helpButton } from './help.js';
import { netinfoHtml, wireNetinfo } from './netinfo.js';
import { openZoneEditor, zonesSummary } from './zones.js';
import { MOTION_PRESETS } from './sp_kit.js';
import { armStrip, loadArmData } from './armstrip.js';
import { tamperHtml, readTamper, wireTamper, unusualHtml, readUnusual, wireUnusual, summaries as watchSummaries } from './cam_watch.js';


const sel = (k, opts, val) => '<select data-k="' + k + '">' + opts.map(([v, t]) => '<option value="' + v + '"' + (String(v) === String(val) ? ' selected' : '') + '>' + esc(t) + '</option>').join('') + '</select>';
const onoff = (k, val) => sel(k, [['1', 'on'], ['0', 'off']], val ? '1' : '0');
const field = (label, input, hint) => '<label class="cs-f"><span>' + label + '</span>' + input + '</label>' + (hint ? '<div class="hint">' + hint + '</div>' : '');
const text = (k, val, extra) => '<input data-k="' + k + '" value="' + esc(val == null ? '' : val) + '" ' + (extra || '') + '>';

// ---------- stream source: network camera, local capture device, or screen ----------
// Local sources are URLs too: device://<name>?size=WxH&fps=N&audio=<name> and
// screen://[<monitor>]?fps=N&region=x,y,w,h. Only % ? & # = are escaped, so names stay readable.
const encPart = (s) => String(s).replace(/[%?&#=]/g, ch => '%' + ch.charCodeAt(0).toString(16).toUpperCase().padStart(2, '0'));
const OTHER = '*other*';
const decPart = (s) => { try { return decodeURIComponent(s); } catch (e) { return s; } };

function parseSource(url) {
  const pm = /^push:\/\/([^/?#]+)\/?$/i.exec(String(url || '').trim());
  if (pm) return { kind: 'push', name: pm[1], q: {} };
  const m = /^(device|screen):\/\/([^?]*)(?:\?(.*))?$/.exec(String(url || '').trim());
  if (!m) return { kind: 'net', name: '', q: {} };
  const q = {};
  (m[3] || '').split('&').filter(Boolean).forEach(kv => { const i = kv.indexOf('='); q[decPart(i < 0 ? kv : kv.slice(0, i)).toLowerCase()] = i < 0 ? '' : decPart(kv.slice(i + 1)); });
  return { kind: m[1], name: decPart(m[2]), q };
}

function buildLocalUrl(kind, name, q) {
  const qs = Object.keys(q).filter(k => q[k] !== '' && q[k] != null).map(k => k + '=' + encPart(q[k])).join('&');
  return kind + '://' + encPart(name) + (qs ? '?' + qs : '');
}

const LOCAL_HINT = 'Raw video is encoded to H.264 for recording, which uses CPU. On Windows, the AmberSight service runs in session 0 and cannot capture the screen; some cameras (virtual ones especially) also refuse to open there. Run AmberSight in your own session (<code>ambersight run</code>) for those.';

// Pan/tilt/zoom: the camera's ONVIF port, found by AmberSight when left empty.
function onvifHtml(c) {
  return '<div class="cs-onvif" data-onvif>' +
    field('ONVIF port', text('onvif_port', c.onvif_port, 'type="number" min="1" max="65535" placeholder="find automatically"'), 'Lets AmberSight move a camera that can pan, tilt or zoom. Leave empty to let it look for the port itself.') +
    '<div class="cs-onvif-st" data-onvif-st role="status" aria-live="polite">Checking...</div>' +
    '<div class="cs-onvif-act"><button type="button" class="btn" data-onvif-save hidden>Save</button><button type="button" class="btn" data-onvif-again>Look for PTZ again</button></div></div>';
}

function wireOnvif(d, c, ctx) {
  const box = d.querySelector('[data-onvif]');
  if (!box) return () => {};
  const st = box.querySelector('[data-onvif-st]'), save = box.querySelector('[data-onvif-save]'), again = box.querySelector('[data-onvif-again]');
  const inp = box.querySelector('[data-k="onvif_port"]');
  let found = null, gone = false, timer = 0;
  const paint = (r) => {
    found = null; save.hidden = true;
    if (r.state === 'configured') st.textContent = 'ONVIF port ' + r.configured + ' is set.';
    else if (r.state === 'found') { found = r.found; st.textContent = 'ONVIF found on port ' + r.found + '. Save it to keep using it.'; save.hidden = false; save.textContent = 'Save port ' + r.found; }
    else if (r.state === 'searching') st.textContent = 'Looking for ONVIF on this camera...';
    else if (r.state === 'none') st.textContent = 'This camera doesn\'t offer ONVIF, so AmberSight can\'t move it. Some firmware has an ONVIF switch in the camera\'s own app.';
    else st.textContent = '';
    again.hidden = r.state === 'na';
  };
  const load = async () => {
    clearTimeout(timer);
    try {
      const r = await api('/api/cameras/' + c.id + '/onvif');
      if (gone) return;
      paint(r);
      if (r.state === 'searching') timer = setTimeout(load, 4000);
    } catch (e) { if (!gone) st.textContent = ''; }
  };
  again.onclick = async () => {
    again.disabled = true; st.textContent = 'Looking for ONVIF on this camera...'; save.hidden = true;
    try { paint(await api('/api/cameras/' + c.id + '/onvif/find', { json: {} })); refreshCameras(); }
    catch (e) { st.textContent = 'Could not search just now.'; }
    again.disabled = false;
  };
  save.onclick = async () => {
    if (found == null) return;
    save.disabled = true;
    try {
      await api('/api/cameras/' + c.id, { method: 'PUT', json: { onvif_port: found } });
      inp.value = found;
      ctx.rebase(box.closest('.cs-sec'));
      toast('Saved', { kind: 'ok' });
      refreshCameras();
      load();
    } catch (e) { st.textContent = 'Could not save: ' + (e.message || e); }
    save.disabled = false;
  };
  load();
  return () => { gone = true; clearTimeout(timer); };
}

function streamHtml(c, isNew) {
  const s = parseSource(c.url);
  if (c.capture) s.kind = 'helper';
  const net = s.kind === 'net';
  const q = s.q;
  return field('Source', sel('src', [['net', 'Network camera (RTSP / HTTP)'], ['device', 'Local device (webcam or capture card)'], ['screen', 'Screen'], ['helper', 'Another computer (screen capture)'], ['push', 'Pushed stream (OBS, ffmpeg...)']], s.kind)) +
    helperHtml(c) +
    '<div data-src="net"' + (net ? '' : ' hidden') + '>' +
      field('Stream URL', text('url', net ? c.url : '', 'placeholder="rtsp://host:554/stream" spellcheck="false"')) +
      field('Sub stream URL (optional)', text('sub_url', c.sub_url, 'placeholder="lower resolution, for the grid and motion" spellcheck="false"')) +
      field('Stream user', text('username', c.username, 'autocomplete="off"')) +
      field('Stream password', '<input data-k="password" type="password" autocomplete="new-password" placeholder="' + (isNew ? 'device password' : 'blank keeps the stored password') + '">') +
      (isNew || c.id == null ? '' : onvifHtml(c)) +
    '</div>' +
    '<div data-src="device"' + (s.kind === 'device' ? '' : ' hidden') + '>' +
      field('Device', '<select data-k="dev"><option value="">loading...</option></select>') +
      '<div data-devother hidden>' + field('Device name or path', text('dev_name', s.kind === 'device' ? s.name : '', 'placeholder="USB Video  or  /dev/video0" spellcheck="false"')) + '</div>' +
      field('Size (optional)', text('dev_size', s.kind === 'device' ? q.size : '', 'placeholder="device default, e.g. 1920x1080" spellcheck="false"')) +
      field('Frame rate (optional)', text('dev_fps', s.kind === 'device' ? q.fps : '', 'type="number" min="1" max="120" placeholder="device default"')) +
      field('Audio input', '<select data-k="dev_audio"><option value="">none</option></select>') +
    '</div>' +
    '<div data-src="screen"' + (s.kind === 'screen' ? '' : ' hidden') + '>' +
      field('Monitor', text('scr_mon', s.kind === 'screen' ? s.name : '', 'type="number" min="0" placeholder="whole desktop"'), 'Blank captures the whole desktop. A number picks one monitor (0 = first; Windows).') +
      field('Frame rate', text('scr_fps', s.kind === 'screen' ? (q.fps || 15) : 15, 'type="number" min="1" max="60"')) +
      field('Region (optional)', text('scr_region', s.kind === 'screen' ? q.region : '', 'placeholder="x,y,width,height" spellcheck="false"')) +
    '</div>' +
    '<div data-src="push"' + (s.kind === 'push' ? '' : ' hidden') + '>' +
      field('Stream key', '<span class="cs-ovin"><input data-k="push_key" value="' + esc(s.kind === 'push' ? s.name : newKey()) + '" spellcheck="false" readonly><button type="button" class="cs-reset" data-newkey title="Make a new key (senders using the old one stop working)">New key</button></span>',
        'The key is the password for sending to this camera. Keep it private.') +
      '<div class="cs-push" data-pushinfo></div>' +
      (isNew ? '' : '<div class="hint" data-pushstate>' + pushState(c) + '</div>') +
    '</div>' +
    '<div class="hint" data-localhint' + (s.kind === 'device' || s.kind === 'screen' ? '' : ' hidden') + '>' + LOCAL_HINT + '</div>' +
    '<div class="hint" data-devnote hidden></div>' +
    (isNew ? '' : '<div class="hint">Receiving: ' + esc(c.ingest || 'unknown') + '</div>');
}

// A random stream key for a pushed camera (letters and digits only, URL-safe).
function newKey() {
  const a = new Uint8Array(18);
  crypto.getRandomValues(a);
  return [...a].map(b => 'abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'[b % 56]).join('');
}
function pushState(c) {
  const s = c.push_sender;
  if (!s) return 'Waiting for a sender. Nothing is being received right now.';
  const m = Math.floor(s.seconds / 60);
  return 'Receiving from ' + esc(s.from) + ' for ' + (m ? m + ' min' : s.seconds + ' s') + '.';
}
// Where and how to send: OBS (RTMP) and ffmpeg (HTTPS) settings for this key.
function drawPushInfo(d) {
  const box = d.querySelector('[data-pushinfo]');
  if (!box) return;
  const key = d.querySelector('[data-k="push_key"]').value;
  const host = location.hostname.includes(':') ? '[' + location.hostname + ']' : location.hostname;
  const srv = (D && D.server) || {};
  const rtmp = srv.rtmp_port ? 'rtmp://' + host + ':' + srv.rtmp_port + '/live' : '';
  const https = location.origin + '/push/' + key;
  const row = (label, val) => '<div class="cs-pushrow"><span>' + label + '</span><code>' + esc(val) + '</code><button type="button" class="cs-reset" data-copytext="' + esc(val) + '">Copy</button></div>';
  box.innerHTML =
    '<div class="sec">OBS and other RTMP tools</div>' +
    (rtmp ? row('Server', rtmp) + row('Stream key', key) : '<div class="hint">RTMP is turned off (rtmp_port = 0).</div>') +
    '<div class="hint">In OBS: Settings, Stream, Service "Custom...", then paste these. Use H.264 video and AAC audio, with a keyframe every 2 seconds. RTMP is not encrypted: use it on your own network or over Tailscale.</div>' +
    '<div class="sec">ffmpeg (over HTTPS)</div>' +
    row('Address', https) +
    row('Example', 'ffmpeg -re -i INPUT -c:v libx264 -preset veryfast -g 50 -c:a aac -f flv -tls_verify 0 ' + https) +
    '<div class="hint">Any FLV stream (H.264 or H.265 video, AAC audio) sent with PUT or POST works. The port opens automatically when you save.</div>';
}

// Show the fields for the chosen source; fill the device lists from the server once.
function wireSource(d, c) {
  const srcSel = d.querySelector('[data-k="src"]');
  const devSel = d.querySelector('[data-k="dev"]');
  const audSel = d.querySelector('[data-k="dev_audio"]');
  const other = d.querySelector('[data-devother]');
  const note = d.querySelector('[data-devnote]');
  const cur = parseSource(c.url);
  let loaded = false;
  const hw = wireHelper(d, c);
  const show = () => {
    const k = srcSel.value;
    d.querySelectorAll('[data-src]').forEach(b => { b.hidden = b.dataset.src !== k; });
    const local = k === 'device' || k === 'screen';
    d.querySelector('[data-localhint]').hidden = !local;
    note.hidden = !local || !note.textContent;
    if (local && !loaded) { loaded = true; load(); }
    if (k === 'push') drawPushInfo(d);
    if (k === 'helper') hw.load();
  };
  const nk = d.querySelector('[data-newkey]');
  if (nk) nk.onclick = () => { d.querySelector('[data-k="push_key"]').value = newKey(); drawPushInfo(d); };
  d.addEventListener('click', async (e) => {
    const b = e.target.closest('[data-copytext]'); if (!b) return;
    try { await navigator.clipboard.writeText(b.dataset.copytext); b.textContent = 'Copied'; setTimeout(() => { b.textContent = 'Copy'; }, 1200); }
    catch (err) { toast('Copy did not work here: select the text instead'); }
  });
  devSel.onchange = () => { other.hidden = devSel.value !== OTHER; };
  async function load() {
    let r = { video: [], audio: [], screen: [] };
    try { r = await api('/api/devices'); } catch (e) { r.note = 'Could not list devices: ' + (e.body || e.message); }
    const curName = cur.kind === 'device' ? cur.name : '';
    const names = (r.video || []).map(v => [parseSource(v.url).name, v.name]);
    if (curName && !names.some(([n]) => n === curName)) names.unshift([curName, curName + ' (not found now)']);
    devSel.innerHTML = (names.length ? '' : '<option value="">no devices found</option>') +
      names.map(([n, label]) => '<option value="' + esc(n) + '"' + (n === curName ? ' selected' : '') + '>' + esc(label) + '</option>').join('') +
      '<option value="' + OTHER + '">Other (type a name or path)...</option>';
    if (!names.length) devSel.value = OTHER;
    devSel.onchange();
    const curAud = cur.kind === 'device' ? (cur.q.audio || '') : '';
    const auds = (r.audio || []).map(a => a.name);
    if (curAud && !auds.includes(curAud)) auds.unshift(curAud);
    audSel.innerHTML = '<option value="">none</option>' + auds.map(a => '<option value="' + esc(a) + '"' + (a === curAud ? ' selected' : '') + '>' + esc(a) + '</option>').join('');
    note.textContent = r.note || '';
    note.hidden = srcSel.value === 'net' || !note.textContent;
  }
  srcSel.onchange = show;
  show();
}

// The stream URL from the Stream tab ('' when incomplete).
function sourceUrl(d) {
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const kind = v('src');
  if (kind === 'device') {
    const dev = d.querySelector('[data-k="dev"]').value;
    const name = dev === OTHER || dev === '' ? v('dev_name') : dev;
    if (!name) return '';
    return buildLocalUrl('device', name, { size: v('dev_size'), fps: v('dev_fps'), audio: v('dev_audio') });
  }
  if (kind === 'screen') return buildLocalUrl('screen', v('scr_mon'), { fps: v('scr_fps'), region: v('scr_region').replace(/\s+/g, '') });
  if (kind === 'push') return 'push://' + v('push_key');
  return v('url');
}

// ---- "Default or custom" fields: blank/Default = use the shared setting ----
// D holds the shared defaults (from /api/settings) while a window is being built.
let D = null;
const CH_LABEL = { push: 'Browser notifications', email: 'Email', telegram: 'Telegram', ntfy: 'ntfy', pushover: 'Pushover', webhook: 'Webhook', home_assistant: 'Home Assistant', image_post: 'Image post', action: 'Action command' };
const pct = (x, dp) => (x == null ? '' : +(x * 100).toFixed(dp == null ? 3 : dp));
// A number override: placeholder shows the default; the reset link clears it.
function ovNum(k, label, val, dflt, extra, hint) {
  const ph = dflt == null || dflt === '' ? 'Default' : 'Default: ' + dflt;
  return '<label class="cs-f cs-ov"><span>' + label + '</span><span class="cs-ovin"><input data-ov data-k="' + k + '" value="' + esc(val == null ? '' : val) + '" placeholder="' + esc(ph) + '" ' + (extra || '') + '>' +
    '<button type="button" class="cs-reset" data-reset="' + k + '" title="Use the default"' + (val == null || val === '' ? ' hidden' : '') + '>Default</button></span></label>' +
    (hint ? '<div class="hint">' + hint + '</div>' : '');
}
// A choice override: the first option is "Default (current default)".
function ovSel(k, label, opts, val, dfltLabel, hint) {
  return field(label, sel(k, [['', 'Default (' + dfltLabel + ')']].concat(opts), val == null ? '' : val).replace('<select ', '<select data-ov '), hint);
}

// ---- Remembered spots (AI): how many, and a button to forget them. No inputs here: it is not
// part of the form's saved values. ----
function spotsHtml() {
  return '<div class="cs-f" data-spots><span>Remembered spots</span><span class="cs-spots-v"><span class="muted" data-spots-text>Looking…</span>' +
    '<button type="button" class="btn" data-spots-forget hidden>Forget remembered spots</button></span></div>';
}
function spotsText(v) {
  const n = v.remembered + v.marked;
  if (!n) return v.learning ? 'None yet. AI is watching ' + v.learning + ' spot' + (v.learning === 1 ? '' : 's') + ' where it has seen something.' : 'None.';
  const parts = [];
  if (v.remembered) parts.push(v.remembered + ' remembered' + (v.on ? '' : ' (not used while “Ignore objects that have not moved” is off)'));
  if (v.marked) parts.push(v.marked + ' marked not real');
  return parts.join(', ') + '.';
}
function wireSpots(d, c) {
  const box = d.querySelector('[data-spots]');
  if (!box) return;
  const txt = box.querySelector('[data-spots-text]'), btn = box.querySelector('[data-spots-forget]');
  const load = async () => {
    try {
      const v = await api('/api/cameras/' + c.id + '/spots');
      txt.textContent = spotsText(v);
      btn.hidden = !(v.remembered + v.marked + v.learning);
    } catch (e) { txt.textContent = 'Could not be read.'; }
  };
  btn.onclick = () => {
    confirmDlg('Forget remembered spots', 'This happens right now, not when you press Save. <b>' + esc(c.name) + '</b> forgets every spot it has learned and every spot marked not real, so what AI sees there alerts again until it learns them again. Recordings and past events are not touched.', 'Forget them', async () => {
      btn.disabled = true;
      try {
        const r = await api('/api/cameras/' + c.id + '/spots/forget', { json: {} });
        toast('Forgot ' + r.forgot + ' spot' + (r.forgot === 1 ? '' : 's') + ': what AI sees there alerts again.', { kind: 'ok' });
        await load();
      } finally { btn.disabled = false; }
    }, (e) => toast('Could not forget them: ' + (e.message || e), { kind: 'err' }));
  };
  load();
}

// ---- Motion sensitivity: Low / Medium / High, like Settings. The five fields it sets stay below
// under More settings; a camera whose five fields hold no override uses the server's choice. ----
const MO_FIELDS = { stMinObj: 'min_object', stContrast: 'contrast', stTravel: 'min_travel', stMinFrames: 'min_frames', stSens: 'sensitivity' };
const MO_NAMES = { low: 'Low', medium: 'Medium', high: 'High' };
const moNear = (a, b) => a !== '' && Math.abs(parseFloat(a) - b) < 1e-6;
// The level the server-wide motion settings amount to ('' when none of the three fits).
function serverMotionLevel() {
  const m = (D && D.motion) || null;
  if (!m) return '';
  const cur = { stMinObj: +((m.min_object != null ? m.min_object : 0.002) * 100).toFixed(3), stContrast: m.contrast != null ? m.contrast : 25,
    stTravel: +((m.min_travel != null ? m.min_travel : 0.03) * 100).toFixed(2), stMinFrames: m.min_frames, stSens: m.sensitivity };
  for (const [lv, p] of Object.entries(MOTION_PRESETS)) if (Object.entries(p).every(([k, x]) => moNear(String(cur[k]), x))) return lv;
  return '';
}
function motionLevelHtml() {
  const sv = serverMotionLevel();
  return '<div class="cs-f"><span>Motion sensitivity</span><select data-lv aria-label="Motion sensitivity">' +
    '<option value="">Use the server\'s setting' + (sv ? ' (' + MO_NAMES[sv] + ')' : '') + '</option>' +
    Object.entries(MO_NAMES).map(([k, t]) => '<option value="' + k + '">' + t + '</option>').join('') + '</select></div>' +
    '<div class="hint" data-lvhint>Low ignores more (shadows, rain, trees). High notices smaller and fainter movement. The exact numbers are under More settings.</div>';
}
// Keep the level choice in step with the five fields (and the other way round on a change).
function wireMotionLevel(d) {
  const lv = d.querySelector('[data-lv]');
  if (!lv) return;
  const f = (k) => d.querySelector('[data-k="' + k + '"]');
  const hint = d.querySelector('[data-lvhint]');
  const base = hint.textContent;
  const setOpt = (val, text) => {
    let o = lv.querySelector('option[data-x]');
    if (!val) { if (o) o.remove(); return; }
    if (!o) { o = document.createElement('option'); o.dataset.x = '1'; lv.appendChild(o); }
    o.value = val; o.textContent = text;
  };
  const sync = () => {
    const vals = {}; let mixed = false, empty = 0;
    for (const [k, key] of Object.entries(MO_FIELDS)) {
      const el = f(key); if (!el) return;
      vals[k] = el.value.trim();
      if (vals[k] === '') { empty++; if (el.classList.contains('mixed') && el.placeholder === 'Mixed') mixed = true; }
    }
    let level = null;
    if (mixed) level = 'mixed';
    else if (empty === 5) level = '';
    else for (const [name, p] of Object.entries(MOTION_PRESETS)) if (Object.entries(p).every(([k, x]) => moNear(vals[k], x))) level = name;
    if (level === null) {
      level = 'custom';
      setOpt('custom', 'Custom' + (vals.stMinObj !== '' ? ' (smallest object ' + vals.stMinObj + '%)' : ''));
    } else if (level === 'mixed') setOpt('mixed', 'Mixed');
    else setOpt('', '');
    lv.value = level;
    hint.textContent = level === 'custom' ? 'Custom: set under More settings. Pick Low, Medium or High to replace it.' : level === 'mixed' ? 'These cameras differ. Pick a level to set it on all of them.' : base;
  };
  lv.addEventListener('change', () => {
    const val = lv.value;
    if (val !== '' && !MOTION_PRESETS[val]) return;
    for (const [k, key] of Object.entries(MO_FIELDS)) {
      const el = f(key);
      el.value = val === '' ? '' : MOTION_PRESETS[val][k];
      if (val === '') el.placeholder = 'Default';
      el.dispatchEvent(new Event('input', { bubbles: true }));
    }
    sync();
  });
  d.addEventListener('input', (e) => { if (e.target !== lv) sync(); });
  d.addEventListener('click', (e) => { if (e.target.closest('[data-reset]')) setTimeout(sync, 0); });
  sync();
}

function groupsHtml(c) {
  const all = [...new Set(state.cams.flatMap(x => x.groups || (x.group ? [x.group] : [])))].sort((x, y) => x.localeCompare(y));
  const mine = new Set(c.groups || []);
  return '<div class="cs-f"><span>Groups</span><div class="cs-groups" data-groups>' +
    all.map(g => '<button type="button" class="chip' + (mine.has(g) ? ' on' : '') + '" data-g="' + esc(g) + '" aria-pressed="' + mine.has(g) + '">' + esc(g) + '</button>').join('') +
    '<input data-k="newgroup" placeholder="New group..." class="cs-newgroup"></div></div>' +
    '<div class="hint">A camera can be in several groups. Click a group to add or remove it; type a new name and press Enter.</div>';
}
function readGroups(d) {
  const out = [...d.querySelectorAll('[data-groups] .chip.on')].map(b => b.dataset.g);
  const extra = d.querySelector('[data-k="newgroup"]').value.split(',').map(x => x.trim()).filter(Boolean);
  for (const g of extra) if (!out.some(x => x.toLowerCase() === g.toLowerCase())) out.push(g);
  return out;
}
function wireGroups(d) {
  const box = d.querySelector('[data-groups]');
  box.onclick = (e) => { const b = e.target.closest('.chip'); if (!b) return; b.classList.toggle('on'); b.setAttribute('aria-pressed', String(b.classList.contains('on'))); };
  const inp = d.querySelector('[data-k="newgroup"]');
  inp.onkeydown = (e) => {
    if (e.key !== 'Enter') return;
    e.preventDefault();
    for (const g of inp.value.split(',').map(x => x.trim()).filter(Boolean)) {
      let b = [...box.querySelectorAll('.chip')].find(x => x.dataset.g.toLowerCase() === g.toLowerCase());
      if (!b) { b = document.createElement('button'); b.type = 'button'; b.className = 'chip'; b.dataset.g = g; b.textContent = g; box.insertBefore(b, inp); }
      b.classList.add('on'); b.setAttribute('aria-pressed', 'true');
    }
    inp.value = '';
  };
}

function alertsHtml(c, multi) {
  const chans = (D && D.alert_channels) || [];
  const all = c.alert_channels == null;
  const arm = multi || !c.name ? '' : '<div class="cs-arm"><div class="sec" style="margin-top:0">When this camera watches</div><div class="hint" style="margin-top:0">From the active profile and schedule, whether you are Armed and this camera\'s windows. Includes your unsaved changes.</div><div data-armhost><span class="muted">Loading...</span></div></div>';
  return [arm +
    ovSel('alerts', 'Notifications', [['1', 'on'], ['0', 'off']], c.alerts == null ? null : c.alerts ? '1' : '0', 'on',
      'Off: events are still recorded and still reported over MQTT, just not sent anywhere.') +
    timeRange('alert_schedule', 'Alert window', c.alert_schedule, '07:00', '22:00', 'Notify only during these hours. Recording is not affected.') +
    (multi ? '' : notifyForHtml(c)) +
    (multi ? '' : '<div class="cs-f"><span>Send through</span><div class="cs-chans">' +
    '<label class="asys-chk"><input type="checkbox" data-chall' + (all ? ' checked' : '') + '> All services that are set up (default)</label>' +
    chans.map(ch => '<label class="asys-chk' + (ch.configured ? '' : ' muted') + '"><input type="checkbox" data-ch="' + ch.id + '"' +
      (all ? ' checked disabled' : (c.alert_channels.includes(ch.id) ? ' checked' : '')) + '> ' + esc(CH_LABEL[ch.id] || ch.id) + (ch.configured ? '' : ' (not set up)') + '</label>').join('') +
    '</div></div><div class="hint">Services are set up once under Settings, Detection &amp; alerts, Alerts.</div>') + '',
    ovSel('alert_priority', 'Priority', [['low', 'low (quiet)'], ['normal', 'normal'], ['high', 'high'], ['urgent', 'urgent']], c.alert_priority, (D && D.alerts.priority) || 'normal',
      'For ntfy and Pushover. High usually gets through Do Not Disturb; urgent makes Pushover repeat until you acknowledge it.') +
    ovNum('alert_interval', 'At most one alert every (s)', c.alert_interval, D ? (D.alerts.min_interval || 0) + ' s' : '', 'type="number" min="0" step="5"', '0 = no limit.') +
    (multi ? '' : strangersHtml(c)) +
    '<div class="sec">No-motion alert</div>' +
    field('Alert after no motion for (min)', text('no_motion_minutes', c.no_motion_minutes || '', 'type="number" min="0" max="10080" step="5" placeholder="off"'),
      'For a pet or a person living alone: a message when nothing has moved here for this long, and another once something moves again. Empty or 0 = off. Works even when you are Disarmed.') +
    timeRange('no_motion_schedule', 'Watch only during', c.no_motion_schedule, '07:00', '21:00', 'Counting starts when this window opens.') +
    (multi ? '' : soundHtml(c))];
}
// "Notify me for": everything (default) or only some objects. The rest are still
// detected, recorded and searchable.
function notifyForHtml(c) {
  const mine = c.alert_labels || [];
  const some = mine.length > 0;
  const base = ['person', 'vehicle', 'animal'].concat(c.ai_labels || (D && D.ai && D.ai.labels) || []);
  const names = [...new Set(base.map(x => String(x).toLowerCase()).concat(mine.map(x => String(x).toLowerCase())))];
  return field('Notify me for', sel('nf', [['all', 'Everything it detects'], ['some', 'Only certain objects']], some ? 'some' : 'all'), '') +
    '<div class="cs-f" data-nfbox' + (some ? '' : ' hidden') + '><span>Objects</span><div class="cs-groups" data-nflist>' +
    names.map(n => '<button type="button" class="chip' + (mine.map(x => x.toLowerCase()).includes(n) ? ' on' : '') + '" data-nl="' + esc(n) + '" aria-pressed="' + mine.map(x => x.toLowerCase()).includes(n) + '">' + esc(n) + '</button>').join('') +
    '</div></div><div class="hint">Doorbell presses, sounds and packages always notify. Other objects are still detected, recorded and searchable; they just don\'t send a notification.' + (some ? '' : ' Pick this to cut down on notifications.') + '</div>' +
    quietAreasHtml(c);
}
// "Areas that don't notify": drawn with the zone editor in its quiet mode. Saved by the editor itself.
function quietAreasHtml(c) {
  const n = (c.alert_ignore_polys || []).length;
  return '<div class="cs-f"><span>Areas that don\'t notify</span><div class="cs-zones"><span data-qsum>' + quietSummary(n) + '</span>' +
    (c.id == null || c.remote ? '<span class="muted"> Save the camera first to draw areas.</span>' : ' <button type="button" class="btn" data-qedit>Draw areas</button>') + '</div></div>' +
    '<div class="hint">Things here are still detected, recorded and searchable. They just don\'t send a notification.</div>';
}
const quietSummary = (n) => (n ? n + ' area' + (n === 1 ? '' : 's') + ' (nothing seen only there sends a notification)' : 'None: every object it detects can notify.');
// Strangers: alert only for unknown faces and plates.
function strangersHtml(c) {
  const s = c.strangers || {};
  return '<div class="sec">Strangers</div>' +
    '<div class="hint" style="margin-top:0">Alert only when the person or vehicle is not known. Known faces (Settings, Smart features, Faces) and known plates (License plates) stay quiet: the event is still recorded and labeled. Needs AI on this camera, and faces or plates turned on for it. Alerts then say \u201cUnknown person at ...\u201d or \u201cUnknown car ABC123 in ...\u201d.</div>' +
    swHtml('data-st-en', !!s.enabled, 'Only alert for unknown people and vehicles', 'Off by default. The alert for this camera waits a few seconds for the faces to be looked at.') +
    swHtml('data-st-un', s.unidentified_is_unknown !== false, 'Count people and vehicles that could not be identified as unknown', 'A person with no face visible, or a car whose plate was not read. Off: they stay quiet.') +
    timeRange('strangers_schedule', 'Only during', s.schedule, '22:00', '06:00', 'Leave off to use this filter all day. With hours, it applies only inside them.') +
    field('Outside those hours', sel('st_outside', [['normal', 'Alert as usual'], ['quiet', 'No alerts for people and vehicles']], s.outside || 'normal'), '') +
    swHtml('data-st-first', !!s.first_seen, 'Alert only the first time an unknown person or vehicle appears', 'Someone or something unknown that was already seen in the last days stays quiet. Needs faces / plates on.') +
    field('Not seen in the last (days)', '<input data-k="st_days" type="number" min="1" max="60" step="1" value="' + (s.first_seen_days || 30) + '">', 'Up to 60: unnamed faces are forgotten after 60 days.');
}
function readStrangers(d) {
  const en = d.querySelector('[data-st-en]');
  if (!en) return undefined;
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const o = {
    enabled: en.checked, unidentified_is_unknown: d.querySelector('[data-st-un]').checked,
    schedule: v('strangers_schedule') || null, outside: v('st_outside') || 'normal',
    first_seen: d.querySelector('[data-st-first]').checked, first_seen_days: Math.max(1, Math.min(60, Math.round(Number(v('st_days')) || 30))),
  };
  return !o.enabled && !o.first_seen && o.unidentified_is_unknown && !o.schedule && o.outside === 'normal' && o.first_seen_days === 30 ? null : o;
}
// Loud-sound trigger: switch, two levels, a live meter, and (with sound
// names on) which sounds count.
const SOUND_NAMES = ['glass break', 'dog bark', 'alarm', 'shouting', 'bang', 'doorbell', 'knock', 'car horn', 'crying'];
function soundHtml(c) {
  const mine = new Set(c.audio_sounds || []);
  return '<div class="sec">Loud sounds</div>' +
    '<label class="sw"><input type="checkbox" role="switch" data-snd' + (c.audio_trigger ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
    '<span class="sw-text"><b>A loud sound starts an event</b><small>A sudden sound much louder than usual here (glass, a bang, a shout, a dog) is recorded and alerts like motion, in the same arm window. The camera must send sound; the Audio setting can stay off.</small></span></label>' +
    ovNum('audio_margin_db', 'Louder than usual by (dB)', c.audio_margin_db, '20 dB', 'type="number" min="3" max="60" step="1"', 'Lower hears quieter sounds, and gives more false alarms.') +
    ovNum('audio_min_db', 'And at least (dB)', c.audio_min_db, '-45 dB', 'type="number" min="-90" max="0" step="1"', 'How loud in itself: 0 is the loudest the microphone reports, a quiet room is about -60.') +
    (c.id == null ? '' : '<div class="snd-meter" data-sndm><div class="snd-bar" title="Sound level now (bar), usual level (gray line), loud from (amber line)"><span class="snd-lvl"></span><span class="snd-bg"></span><span class="snd-thr"></span></div><div class="hint" data-sndt>Listening...</div></div>') +
    '<div class="cs-f"><span>Only these sounds</span><div class="cs-groups" data-sndlist>' +
    SOUND_NAMES.map(n => '<button type="button" class="chip' + (mine.has(n) ? ' on' : '') + '" data-sn="' + esc(n) + '" aria-pressed="' + mine.has(n) + '">' + esc(n) + '</button>').join('') +
    '</div></div><div class="hint" data-sndhint>None picked: any loud sound counts. Picking some needs sound names on (Settings &rsaquo; Smart features &rsaquo; Sounds).</div>';
}
const dbNum = (x) => Math.round(x) + ' dB';
// Poll the camera's sound level while its Alerts tab is open.
function wireSound(d, c) {
  const box = d.querySelector('[data-sndlist]');
  if (box) box.onclick = (e) => { const b = e.target.closest('.chip'); if (!b) return; b.classList.toggle('on'); b.setAttribute('aria-pressed', String(b.classList.contains('on'))); };
  api('/api/sound/status').then(st => {
    const h = d.querySelector('[data-sndhint]');
    if (h && st.loaded) h.textContent = 'None picked: any loud sound counts (and is named when it can be).';
  }).catch(() => {});
  const m = d.querySelector('[data-sndm]');
  if (!m) return;
  const pos = (db) => Math.max(0, Math.min(100, (db + 90) / 90 * 100)) + '%';
  const num = (k, dflt) => { const el = d.querySelector('[data-k="' + k + '"]'); const v = el && el.value.trim() !== '' ? Number(el.value) : NaN; return isFinite(v) ? v : dflt; };
  let timer = 0;
  const tick = async () => {
    if (!d.isConnected) return;
    if (m.offsetParent !== null && d.__inView && d.__inView('alerts')) {
      try {
        const r = await api('/api/cameras/' + c.id + '/sound');
        const t = m.querySelector('[data-sndt]');
        const lvl = m.querySelector('.snd-lvl'), bg = m.querySelector('.snd-bg'), thr = m.querySelector('.snd-thr');
        if (r.level == null) {
          lvl.style.width = '0'; bg.hidden = thr.hidden = true;
          t.textContent = r.error ? r.error : !r.native ? 'Sound is only heard on cameras using the built-in stream reader.' : 'No sound from this camera (yet). Does it send audio?';
        } else {
          // Loud from: the usual level plus the margin, but never below the floor (as typed, before saving).
          const from = Math.max(r.background + num('audio_margin_db', 20), num('audio_min_db', -45));
          lvl.style.width = pos(r.level);
          lvl.classList.toggle('loud', r.level >= from);
          bg.hidden = thr.hidden = false;
          bg.style.left = pos(r.background); thr.style.left = pos(from);
          let txt = 'Now ' + dbNum(r.level) + ', usually ' + dbNum(r.background) + ', loud from ' + dbNum(from) + '.';
          if (r.heard_at) txt += ' Last loud sound ' + new Date(r.heard_at * 1000).toLocaleTimeString() + ((r.heard || []).length > 1 ? ' (' + r.heard.slice(1).join(', ') + ')' : '') + '.';
          if (!r.trigger) txt += ' Turn the switch on and save to use it.';
          t.textContent = txt;
        }
      } catch (e) { /* the next tick tries again */ }
    }
    timer = setTimeout(tick, 400);
  };
  tick();
  return () => clearTimeout(timer);
}

function wireAlerts(d) {
  const nf = d.querySelector('[data-k="nf"]'), nfbox = d.querySelector('[data-nfbox]');
  if (nf && nfbox) {
    nf.onchange = () => { nfbox.hidden = nf.value !== 'some'; };
    nfbox.querySelector('[data-nflist]').onclick = (e) => { const b = e.target.closest('.chip'); if (!b) return; b.classList.toggle('on'); b.setAttribute('aria-pressed', String(b.classList.contains('on'))); };
  }
  const all = d.querySelector('[data-chall]');
  if (!all) return;
  all.onchange = () => d.querySelectorAll('[data-ch]').forEach(x => { x.disabled = all.checked; if (all.checked) x.checked = true; });
}

// Other cameras / groups this camera's own motion also triggers (none by default).
function triggerHtml(c) {
  const mine = new Set(c.also_trigger || []);
  const groups = [...new Set(state.cams.flatMap(x => x.groups || []))].sort((a, b) => a.localeCompare(b));
  const cams = state.cams.filter(x => x.id !== c.id && !x.remote);
  const box = (val, label) => '<label class="asys-chk"><input type="checkbox" data-trig="' + esc(val) + '"' + (mine.has(val) ? ' checked' : '') + '> ' + label + '</label>';
  return '<div class="sec" style="margin-top:0">Also trigger</div>' +
    '<div class="hint">When this camera detects motion, these record an event too (each with its own pre- and post-roll). Off unless you tick something. Cameras that are only triggered never pass it on.</div>' +
    '<div class="cs-copygrid">' + groups.map(g => box('group:' + g, '<b>Group:</b> ' + esc(g))).join('') + cams.map(x => box(x.name, esc(x.name))).join('') + '</div>';
}

// ---------- watchdog: restart a camera that lost its video, cut its power, tell the owner ----------
const errText = (e) => { try { const j = JSON.parse(e.body); if (j && j.error) return j.error; } catch (x) { /* not JSON */ } return e.body || e.message; };
const isNetCam = (c) => /^(rtsp|https?):\/\//i.test(c.url || '') && !c.capture;
const WD_METHODS = [['auto', 'Automatic'], ['onvif', 'ONVIF'], ['reolink', 'Reolink'], ['dahua', 'Dahua / Amcrest'], ['hikvision', 'Hikvision'], ['none', 'Don\'t restart it']];
const PLUGS = [['', 'None'], ['shelly', 'Shelly plug (Gen1)'], ['shelly2', 'Shelly Plus / Pro / Gen2'], ['tasmota', 'Tasmota plug'], ['urls', 'Other (an off and an on address)']];
function swHtml(attr, on, title, small) {
  return '<label class="sw"><input type="checkbox" role="switch" ' + attr + (on ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
    '<span class="sw-text"><b>' + title + '</b>' + (small ? '<small>' + small + '</small>' : '') + '</span></label>';
}
function watchdogHtml(c) {
  const w = c.watchdog || {};
  const p = w.power || {};
  const g = (D && D.watchdog) || {};
  const on = w.enabled == null ? !!g.enabled : w.enabled;
  return [
    '<div class="hint" style="margin-top:0">When this camera has no video for ' + (g.minutes || 10) + ' min, AmberSight restarts it, then cuts its power for a moment (if a switch is set up under More settings), then tells you if it is still down.' +
      (g.enabled ? '' : ' The watchdog is off in Settings, Connections, Watchdog: turning it on here watches just this camera.') + '</div>' +
    swHtml('data-wd-en', on, 'Watch this camera', 'Default: ' + (g.enabled ? 'on' : 'off') + ' (Settings, Connections, Watchdog)') +
    '<div class="wd-state" data-wdstate></div>' +
    field('How to restart it', sel('wd_reboot', WD_METHODS, w.reboot || 'auto'), 'Automatic picks by what the camera is (Reolink, Dahua/Amcrest, Hikvision), else ONVIF. It signs in with the stream user and password.'),
    field('Web port (optional)', text('wd_port', w.http_port || '', 'type="number" min="1" max="65535" placeholder="the usual one"')) +
    '<div class="sec">Power switch (optional)</div>' +
    '<div class="hint">A smart plug or a PoE switch port that powers this camera. Used when a restart did not help, or the camera does not answer at all.</div>' +
    field('Switch', sel('wd_kind', PLUGS, p.kind || '')) +
    '<div data-wdplug' + (p.kind && p.kind !== 'urls' ? '' : ' hidden') + '>' +
      field('Plug address', text('wd_host', p.host, 'placeholder="192.168.1.50" spellcheck="false"')) +
      field('Outlet', text('wd_ch', p.channel || 0, 'type="number" min="0" max="15"'), '0 = the first (most plugs have just one).') +
    '</div>' +
    '<div data-wdurls' + (p.kind === 'urls' ? '' : ' hidden') + '>' +
      field('Off address', text('wd_off', p.off_url, 'placeholder="http://192.168.1.2/port/4/off" spellcheck="false"')) +
      field('On address', text('wd_on', p.on_url, 'placeholder="http://192.168.1.2/port/4/on" spellcheck="false"')) +
      field('Send as', sel('wd_method', [['GET', 'GET'], ['POST', 'POST']], (p.method || 'GET').toUpperCase())) +
    '</div>' +
    '<div data-wdlogin' + (p.kind ? '' : ' hidden') + '>' +
      field('Switch user', text('wd_user', p.username, 'autocomplete="off" placeholder="if it has a sign-in"')) +
      field('Switch password', '<input data-k="wd_pass" type="password" autocomplete="new-password" placeholder="' + (p.has_password ? 'blank keeps the stored password' : 'if it has a sign-in') + '">') +
      field('Power off for (s)', text('wd_secs', p.off_seconds || 10, 'type="number" min="3" max="120"')) +
      '<div class="row"><button type="button" class="btn" data-wdtest>Test power cycle</button><span class="hint" data-wdtestout></span></div>' +
    '</div>' +
    '<div class="sec">Recent actions</div><div class="wd-acts" data-wdacts><div class="hint">None yet.</div></div>'];
}
// The watchdog settings from the form (undefined when this window has no Watchdog tab).
function readWatchdog(d) {
  if (!d.querySelector('[data-wd-en]')) return undefined;
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const g = (D && D.watchdog) || {};
  const en = d.querySelector('[data-wd-en]').checked;
  const out = {
    // Same as the shared setting: follow it.
    enabled: en === !!g.enabled ? null : en,
    reboot: v('wd_reboot') === 'auto' ? null : v('wd_reboot'),
    http_port: v('wd_port') ? Number(v('wd_port')) : null,
    power: readPower(d),
  };
  return out.enabled == null && !out.reboot && !out.http_port && !out.power ? null : out;
}
function readPower(d) {
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const kind = v('wd_kind');
  if (!kind) return null;
  const p = { kind, username: v('wd_user'), password: d.querySelector('[data-k="wd_pass"]').value, off_seconds: Number(v('wd_secs')) || 10 };
  if (kind === 'urls') Object.assign(p, { off_url: v('wd_off'), on_url: v('wd_on'), method: v('wd_method') });
  else Object.assign(p, { host: v('wd_host'), channel: Math.max(0, Math.round(Number(v('wd_ch')) || 0)) });
  return p;
}
const ACT_LABEL = { 'camera restart': 'Restart', 'camera power cycle': 'Power cycle', 'watchdog gave up': 'Told you' };
function wireWatchdog(d, c) {
  const kind = d.querySelector('[data-k="wd_kind"]');
  if (!kind) return;
  kind.onchange = () => {
    d.querySelector('[data-wdplug]').hidden = !kind.value || kind.value === 'urls';
    d.querySelector('[data-wdurls]').hidden = kind.value !== 'urls';
    d.querySelector('[data-wdlogin]').hidden = !kind.value;
  };
  const out = d.querySelector('[data-wdtestout]');
  d.querySelector('[data-wdtest]').onclick = () => {
    const power = readPower(d);
    if (!power) return;
    confirmDlg('Test power cycle', 'Turn the power to <b>' + esc(c.name) + '</b> off for ' + power.off_seconds + ' s and on again? It will be offline for a minute or two while it starts up.', 'Power cycle', async () => {
      out.textContent = 'Power off, waiting ' + power.off_seconds + ' s, power on...';
      await api('/api/cameras/' + c.id + '/powercycle', { json: { power } });
      out.textContent = 'Done: the power went off and on again.';
      loadWatchdog(d, c);
    }, (e) => { out.textContent = errText(e); });
  };
  loadWatchdog(d, c);
}
async function loadWatchdog(d, c) {
  let st;
  try { st = await api('/api/cameras/' + c.id + '/watchdog'); } catch (e) { return; }
  const box = d.querySelector('[data-wdstate]');
  if (!box) return;
  const how = st.can_reboot ? 'Restarts it through ' + st.method : 'Cannot restart it from here';
  box.innerHTML = '<span class="badge' + (st.enabled ? ' on dot' : '') + '">' + esc(st.state) + '</span><span class="hint">' + esc(how) +
    (st.can_power ? ', then uses the power switch' : '') + '.' + (st.network_down ? ' Most cameras are offline right now, so it waits.' : '') + '</span>';
  const acts = d.querySelector('[data-wdacts]');
  if (acts && st.actions.length) {
    acts.innerHTML = st.actions.map(a => '<div class="wd-act"><span class="muted">' + esc(new Date(a.ts * 1000).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' })) + '</span>' +
      '<span class="badge' + (a.ok ? '' : ' off') + '">' + esc(ACT_LABEL[a.action] || a.action) + '</span><span>' + esc(a.detail.replace(/^[^:]*:\s*/, '')) + '</span>' +
      '<span class="muted">' + esc(a.user === '(watchdog)' ? 'automatic' : a.user) + '</span></div>').join('');
  }
}
// A small centered "are you sure" window, one at a time per title. `go` runs on
// confirm (the window closes at once); its errors go to `fail`.
const confirmDlgs = {};
function confirmDlg(title, html, yes, go, fail) {
  if (raiseDlg(confirmDlgs[title])) return;
  const d = confirmDlgs[title] = openDlg(title, '<div>' + html + '</div>' +
    '<div class="row"><button class="primary" data-yes>' + esc(yes) + '</button><button class="btn" data-no>Cancel</button></div>', true);
  const close = () => { delete confirmDlgs[title]; d.__close(); };
  d.querySelector('.bar button').onclick = close;
  d.querySelector('[data-no]').onclick = close;
  d.querySelector('[data-yes]').onclick = async () => {
    close();
    try { await go(); } catch (e) { fail(e); }
  };
}
// "Reboot camera..." in a camera's menu (admins).
export function rebootDlg(c) {
  confirmDlg('Reboot camera', 'Restart <b>' + esc(c.name) + '</b> now? Its video stops for a minute or two while it starts up again.', 'Restart', async () => {
    toast('Restarting ' + c.name + '...');
    const r = await api('/api/cameras/' + c.id + '/reboot', { json: {} });
    toast(c.name + ' is restarting (' + r.method + ')');
  }, (e) => msgDlg('Could not restart ' + c.name + ': ' + errText(e)));
}
export const canReboot = (c) => !c.remote && isNetCam(c) && !(c.watchdog && c.watchdog.reboot === 'none');

// null = everything; undefined = not on this form (several cameras at once: left alone).
function readAlertLabels(d) {
  const nf = d.querySelector('[data-k="nf"]');
  if (!nf) return undefined;
  if (nf.value !== 'some') return null;
  const l = [...d.querySelectorAll('[data-nflist] .chip.on')].map(b => b.dataset.nl);
  return l.length ? l : null;
}

// Override values from the form: '' = null (use the default).
function readOverrides(d) {
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const n = (k, f) => (v(k) === '' ? null : f(Number(v(k))));
  const chall = d.querySelector('[data-chall]');
  return {
    motion: {
      motion_algorithm: v('motion_algorithm') || null,
      min_object: n('min_object', x => x / 100), max_object: n('max_object', x => x / 100),
      contrast: n('contrast', x => Math.round(x)), min_travel: n('min_travel', x => x / 100),
      min_frames: n('min_frames', x => Math.max(1, Math.round(x))), break_time: n('break_time', x => Math.round(x)),
      sensitivity: n('sensitivity', x => x), schedule: v('schedule') || null,
    },
    recording: { pre_roll: n('pre_roll', x => Math.round(x)), post_roll: n('post_roll', x => Math.round(x)) },
    alerts: {
      alerts: v('alerts') === '' ? null : v('alerts') === '1',
      alert_schedule: v('alert_schedule') || null,
      no_motion_minutes: n('no_motion_minutes', x => Math.max(0, Math.round(x))) || null,
      no_motion_schedule: v('no_motion_schedule') || null,
      audio_trigger: !!(d.querySelector('[data-snd]') || {}).checked,
      audio_margin_db: n('audio_margin_db', x => Math.max(3, Math.min(60, x))),
      audio_min_db: n('audio_min_db', x => Math.max(-90, Math.min(0, x))),
      audio_sounds: [...d.querySelectorAll('[data-sndlist] .chip.on')].map(b => b.dataset.sn),
      alert_interval: n('alert_interval', x => Math.round(x)),
      alert_priority: v('alert_priority') || null,
      alert_labels: readAlertLabels(d),
      alert_channels: !chall || chall.checked ? null : [...d.querySelectorAll('[data-ch]:checked')].map(x => x.dataset.ch),
    },
    ai: {
      ai: v('ai') === '' ? null : v('ai') === '1',
      ai_labels: v('ai_labels') ? v('ai_labels').split(',').map(x => x.trim()).filter(Boolean) : null,
      ai_min_score: n('ai_min_score', x => x),
      ai_ignore_parked: v('ai_ignore_parked') === '' ? null : v('ai_ignore_parked') === '1',
      ai_strict: v('ai_strict') === '' ? null : v('ai_strict') === '1',
      ai_hide_unconfirmed: v('ai_hide_unconfirmed') === '' ? null : v('ai_hide_unconfirmed') === '1',
      ai_confirm_camera: v('ai_confirm_camera') === '1' ? true : null,
    },
  };
}

function readForm(d, c, isNew) {
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const num = (k, dflt) => { const x = v(k); return x === '' ? dflt : Number(x); };
  const o = readOverrides(d);
  const body = Object.assign({
    name: v('name'), short_name: v('short_name'), groups: readGroups(d),
    url: v('url'), sub_url: v('sub_url') || null, username: v('username'), password: d.querySelector('[data-k="password"]').value,
    record: v('record') === '1', record_motion_only: v('record_mode') === '1', record_sub: v('record_mode') === '2', direct: v('direct') === '1',
    on_demand: v('on_demand') === '1', idle_timeout: Math.min(604800, Math.max(30, num('idle_timeout', 300) || 300)),
    fps: num('fps', 10), scale: num('scale', 640),
    audio: v('audio') === '1', overlay: v('overlay') === '1', packages: v('packages') === '1' ? true : null,
    overlay_text: v('overlay_text') || null, overlay_pos: v('overlay_pos') || null,
  }, o.motion, o.recording, o.alerts, o.ai);
  const stg = readStrangers(d);
  if (stg !== undefined) body.strangers = stg;
  if (isNew) { body.enabled = true; } else {
    body.enabled = v('enabled') === '1'; // zones / ignore_zones are owned by the zones editor; the server keeps what it has.
    body.also_trigger = [...d.querySelectorAll('[data-trig]:checked')].map(x => x.dataset.trig);
    const wd = readWatchdog(d);
    if (wd !== undefined && v('src') === 'net') body.watchdog = wd;
    const op = d.querySelector('[data-k="onvif_port"]');
    if (op) body.onvif_port = op.value.trim() === '' ? null : Number(op.value);
    const tp = readTamper(d);
    if (tp !== undefined) body.tamper = tp;
    const un = readUnusual(d);
    if (un !== undefined) body.unusual = un;
  }
  // GET /api/cameras "motion" is the LIVE motion flag, not the setting, so the switch starts on
  // (or at `motion_enabled` if the API ever reports it) and is only written when the user changes it.
  if (v('motion') !== (c.motion_enabled !== false ? '1' : '0')) body.motion = v('motion') === '1';
  body.capture = null;
  if (v('src') === 'helper') {
    // Another computer's screen arrives as a pushed stream with its own key.
    const key = (/^push:\/\/([^/?#]+)/i.exec(c.url || '') || [])[1] || newKey();
    body.capture = readHelper(d);
    Object.assign(body, { url: body.capture ? 'push://' + key : '', sub_url: null, username: '', direct: false });
    delete body.password;
  } else if (v('src') !== 'net') {
    // Local sources: no sign-in, no sub stream, nothing to stream-copy.
    Object.assign(body, { url: sourceUrl(d), sub_url: null, username: '', direct: false });
    delete body.password;
  }
  return body;
}

// Copy chosen sections of this window's settings to other cameras.
function copyDlg(d, c) {
  const others = state.cams.filter(x => x.id !== c.id && !x.remote);
  const secs = [['motion', 'Motion'], ['recording', 'Pre- and post-roll'], ['alerts', 'Alerts'], ['ai', 'AI'], ['own', 'Loud sounds and no-motion alert']];
  // Loud sounds and the no-motion alert suit one camera (a microphone, a
  // pet's room), so they are copied only when picked.
  const OWN = ['audio_trigger', 'audio_margin_db', 'audio_min_db', 'audio_sounds', 'no_motion_minutes', 'no_motion_schedule'];
  const cd = openDlg('Copy settings from ' + c.name,
    '<div class="hint">Copies what is in the camera window now (saved or not). Fields left on Default stay on Default.</div>' +
    '<div class="sec">What to copy</div><div class="cs-copygrid">' + secs.map(([k, l]) => '<label class="asys-chk"><input type="checkbox" data-sec="' + k + '"' + (k === 'own' ? '' : ' checked') + '> ' + l + '</label>').join('') + '</div>' +
    '<div class="sec">To these cameras</div><div class="cs-copygrid">' + others.map(x => '<label class="asys-chk"><input type="checkbox" data-to="' + x.id + '"> ' + esc(x.name) + '</label>').join('') + '</div>' +
    '<div class="asys-err" data-err></div><div class="row"><button class="primary" data-go>Copy</button><button class="btn" data-no>Cancel</button></div>', true);
  cd.querySelector('[data-no]').onclick = () => cd.__close();
  cd.querySelector('[data-go]').onclick = async () => {
    const o = readOverrides(d);
    const own = {};
    for (const k of OWN) if (o.alerts && k in o.alerts) { own[k] = o.alerts[k]; delete o.alerts[k]; }
    o.own = own;
    const patch = {};
    cd.querySelectorAll('[data-sec]:checked').forEach(x => Object.assign(patch, o[x.dataset.sec]));
    const to = [...cd.querySelectorAll('[data-to]:checked')].map(x => +x.dataset.to);
    if (!to.length || !Object.keys(patch).length) { cd.querySelector('[data-err]').textContent = 'Pick at least one camera and one kind of setting.'; return; }
    try {
      for (const id of to) await api('/api/cameras/' + id, { method: 'PUT', json: patch });
      cd.__close();
      toast('Copied to ' + to.length + ' camera' + (to.length === 1 ? '' : 's'));
      refreshCameras();
    } catch (e) { cd.querySelector('[data-err]').textContent = e.body || e.message; }
  };
}

// ---------- time range control (HH:MM-HH:MM, the only schedule format the server reads) ----------
const TR_RE = /^\s*(\d{1,2}):(\d{2})\s*-\s*(\d{1,2}):(\d{2})\s*$/;
const p2 = (n) => String(n).padStart(2, '0');
function trParse(v) {
  const m = TR_RE.exec(v || '');
  if (!m || +m[1] > 23 || +m[3] > 23 || +m[2] > 59 || +m[4] > 59) return null;
  return [p2(+m[1]) + ':' + m[2], p2(+m[3]) + ':' + m[4]];
}
// A schedule field: "Only between [time] and [time]", written as the text the server expects.
// A stored value the pickers cannot express stays editable as text.
function timeRange(k, label, val, dFrom, dTo, hint) {
  const v = (val || '').trim();
  const t = trParse(v);
  const raw = v !== '' && !t;
  return '<div class="cs-f cs-tr" data-trf="' + k + '" data-dfrom="' + dFrom + '" data-dto="' + dTo + '"><span>' + label + '</span><div class="cs-trbox">' +
    '<label class="asys-chk"><input type="checkbox" data-tron' + (v ? ' checked' : '') + '> Only between</label>' +
    '<span class="cs-trin" data-trpick' + (v && !raw ? '' : ' hidden') + '><input type="time" data-trs aria-label="' + esc(label) + ' from" value="' + (t ? t[0] : dFrom) + '"> <em>and</em> <input type="time" data-tre aria-label="' + esc(label) + ' until" value="' + (t ? t[1] : dTo) + '"> <button type="button" class="cs-reset" data-trtype title="Type it instead">Type</button></span>' +
    '<span class="cs-trin" data-trraw' + (raw ? '' : ' hidden') + '><input data-k="' + k + '" value="' + esc(v) + '" placeholder="07:00-21:00" spellcheck="false"> <button type="button" class="cs-reset" data-trbacktopick title="Use the time pickers">Pickers</button></span>' +
    '<span class="cs-trnote" data-trnote></span></div></div>' + (hint ? '<div class="hint">' + hint + '</div>' : '');
}
// The text field data-k=k holds the value in both modes; in picker mode it is kept in step.
function wireTimeRanges(d) {
  d.querySelectorAll('[data-trf]').forEach(box => {
    const on = box.querySelector('[data-tron]');
    const s = box.querySelector('[data-trs]'), e = box.querySelector('[data-tre]');
    const pick = box.querySelector('[data-trpick]'), rawBox = box.querySelector('[data-trraw]');
    const raw = rawBox.querySelector('input');
    const note = box.querySelector('[data-trnote]');
    const rawMode = () => !rawBox.hidden;
    const sync = () => {
      if (rawMode()) return;
      raw.value = on.checked && s.value && e.value ? s.value + '-' + e.value : '';
      note.textContent = on.checked && s.value && e.value && s.value > e.value ? 'overnight: ends the next day' : '';
      raw.dispatchEvent(new Event('input', { bubbles: true }));
    };
    const show = () => {
      pick.hidden = !on.checked || rawMode();
      if (!on.checked) { rawBox.hidden = true; pick.hidden = true; }
    };
    if (!raw.value && !on.checked) { raw.value = ''; }
    on.onchange = () => {
      if (on.checked && rawMode() && !raw.value.trim()) rawBox.hidden = true;
      if (!on.checked) { rawBox.hidden = true; raw.value = ''; }
      show(); sync();
      if (!on.checked) clearFieldErr(raw);
    };
    s.oninput = e.oninput = sync;
    box.querySelector('[data-trtype]').onclick = () => { rawBox.hidden = false; pick.hidden = true; raw.focus(); };
    box.querySelector('[data-trbacktopick]').onclick = () => {
      const t = trParse(raw.value);
      s.value = t ? t[0] : box.dataset.dfrom; e.value = t ? t[1] : box.dataset.dto;
      rawBox.hidden = true; pick.hidden = false; clearFieldErr(raw); sync();
    };
    raw.addEventListener('change', () => { if (raw.value.trim() && !trParse(raw.value)) fieldErr(raw, 'Write it like 07:00-21:00 (hours 00-23).'); });
    note.textContent = on.checked && !rawMode() && s.value > e.value ? 'overnight: ends the next day' : '';
  });
}

// ---------- inline field errors ----------
function errHost(el) { return el.closest('.cs-tr, .cs-ov, .cs-f') || el.parentNode; }
function clearFieldErr(el) {
  el.classList.remove('bad');
  const h = errHost(el);
  const x = h.nextElementSibling;
  if (x && x.classList.contains('cs-ferr')) x.remove();
}
function fieldErr(el, msg) {
  clearFieldErr(el);
  el.classList.add('bad');
  const h = errHost(el);
  const x = document.createElement('div');
  x.className = 'cs-ferr'; x.setAttribute('role', 'alert'); x.textContent = msg;
  h.insertAdjacentElement('afterend', x);
  const more = el.closest('details');
  if (more) more.open = true;
  return x;
}

// ---------- the sections ----------
const SECTIONS = [['general', 'General'], ['stream', 'Stream'], ['network', 'Network'], ['recording', 'Recording'], ['motion', 'Motion'], ['alerts', 'Alerts'], ['ai', 'AI'], ['av', 'Audio & text'], ['tamper', 'Tamper'], ['unusual', 'Unusual activity'], ['watchdog', 'Watchdog']];
const MORE_KEY = 'ambs.cs.more';
const moreState = () => { try { return JSON.parse(localStorage.getItem(MORE_KEY) || '{}') || {}; } catch (e) { return {}; } };
const setMore = (k, v) => { try { const s = moreState(); s[k] = v; localStorage.setItem(MORE_KEY, JSON.stringify(s)); } catch (e) { /* not remembered */ } };

// Each section: [basics html, "More settings" html].
function sectionsOf(c, isNew, multi) {
  const m = (D && D.motion) || {};
  const ai = (D && D.ai) || {};
  const out = {
    general: [
      field('Name', text('name', c.name, 'placeholder="Porch"')) +
      (isNew ? '' : field('Enabled', onoff('enabled', c.enabled !== false))) +
      groupsHtml(c),
      field('Short name', text('short_name', c.short_name, 'placeholder="Door"'), 'Shown where space is tight.')],
    stream: [streamHtml(c, isNew),
      field('Battery camera (connect only when needed)', onoff('on_demand', !!c.on_demand)) +
      field('Disconnect after idle (s)', text('idle_timeout', c.idle_timeout || 300, 'type="number" min="30" max="604800" step="10"'))],
    recording: [
      field('Record', onoff('record', c.record !== false)) +
      field('Record mode', sel('record_mode', [['0', 'continuous'], ['2', 'continuous: sub stream, main stream around events'], ['1', 'motion only']], c.record_motion_only ? '1' : (c.record_sub ? '2' : '0')),
        'The middle choice keeps the full picture only around events and the sub stream the rest of the time, using far less space. It needs a sub stream URL; without one the camera records continuously.'),
      ovNum('pre_roll', 'Pre-roll (s)', c.pre_roll, m.pre_roll != null ? m.pre_roll + ' s' : '', 'type="number" min="0"', 'Video kept from before an event starts.') +
      ovNum('post_roll', 'Post-roll (s)', c.post_roll, m.post_roll != null ? m.post_roll + ' s' : '', 'type="number" min="0"', 'Video kept after it ends.') +
      field('Direct copy', onoff('direct', !!c.direct), 'Saves the camera\'s own video without re-encoding (lowest CPU).')],
    motion: [
      (isNew || multi ? '' : '<div class="cs-prev" data-prev><img alt="Live picture from this camera" hidden><span class="cs-pstate" data-pst>Quiet</span><span class="cs-pnone" data-pnone hidden>No picture right now</span></div>') +
      field('Motion detection', onoff('motion', c.motion_enabled !== false)) +
      motionLevelHtml() +
      (multi ? '' : '<div class="cs-zones"><span data-zsum>' + zonesSummary(c) + '</span>' +
        (isNew || c.remote ? '<span class="muted"> Save the camera first to edit its zones.</span>' : ' <button type="button" class="btn" data-zedit>Edit zones</button>') + '</div>') +
      timeRange('schedule', 'Arm window', c.schedule, '20:00', '07:00', m.schedule ? 'Motion counts only during these hours. Shared default: ' + esc(m.schedule) + '.' : 'Motion counts only during these hours. Off: all day.'),
      ovSel('motion_algorithm', 'Algorithm', [['objects', 'objects (moving objects)'], ['frame-diff', 'frame-diff (whole-frame change)']], c.motion_algorithm, m.algorithm || 'objects') +
      ovNum('max_object', 'Max object size (% of frame)', pct(c.max_object, 2), m.max_object != null ? pct(m.max_object, 2) + '%' : '', 'type="number" min="1" max="100" step="1"', 'Larger changes are treated as lighting changes.') +
      ovNum('min_object', 'Min object size (% of frame)', pct(c.min_object), m.min_object != null ? pct(m.min_object) + '%' : '', 'type="number" min="0.01" max="50" step="0.05"',
        'The smallest moving object that counts. Lower notices smaller things (and more false alarms).') +
      ovNum('contrast', 'Contrast (0-255)', c.contrast, m.contrast, 'type="number" min="0" max="255"', 'How much a pixel must change. Raise it to ignore shadows and noise.') +
      ovNum('min_travel', 'Min travel (% of frame)', pct(c.min_travel, 2), m.min_travel != null ? pct(m.min_travel, 2) + '%' : '', 'type="number" min="0" max="20" step="0.5"', 'How far something must move before it counts. Filters swaying branches.') +
      ovNum('min_frames', 'Min frames', c.min_frames, m.min_frames, 'type="number" min="1"', 'Frames in a row with motion before an event starts.') +
      ovNum('break_time', 'Event break time (s)', c.break_time, m.break_time != null ? m.break_time + ' s' : '', 'type="number" min="0"', 'An event ends only after this long without motion.') +
      ovNum('sensitivity', 'Threshold (frame-diff only)', c.sensitivity, m.sensitivity, 'type="number" step="0.001"') +
      field('Analysis FPS', text('fps', c.fps || 10, 'type="number" min="1" max="30"')) +
      field('Analysis width (px)', text('scale', c.scale || 640, 'type="number" min="160" step="32"')) +
      (isNew || multi ? '' : triggerHtml(c))],
    alerts: alertsHtml(c, multi),
    ai: [
      ovSel('ai', 'Object detection', [['1', 'on'], ['0', 'off']], c.ai == null ? null : c.ai ? '1' : '0', ai.enabled ? 'on' : 'off') +
      ovNum('ai_labels', 'Objects that count', (c.ai_labels || null) && c.ai_labels.join(', '), (ai.labels || []).join(', '), '', 'Comma separated, e.g. person, car, dog.') +
      ovNum('ai_min_score', 'Min confidence (0-1)', c.ai_min_score, ai.min_score, 'type="number" min="0.05" max="0.99" step="0.05"') +
      // (Only "on" changes anything: there is no shared setting, and the default is off.)
      ovSel('ai_confirm_camera', 'Camera\'s own detection needs AI to agree', [['1', 'on']], c.ai_confirm_camera === true ? '1' : null, 'off',
        'When the camera itself reports a person, vehicle or animal, it alerts only if AI sees one too (AI keeps looking while the camera reports). Doorbell presses, sounds and packages always count. If AI cannot look, the camera\'s word counts. Off: the camera\'s own detection alerts on its own.') +
      (isNew || multi || c.remote ? '' : spotsHtml()),
      ovSel('ai_ignore_parked', 'Ignore objects that have not moved', [['1', 'yes'], ['0', 'no']], c.ai_ignore_parked == null ? null : c.ai_ignore_parked ? '1' : '0', ai.ignore_parked === false ? 'no' : 'yes',
        'A car parked in view (or someone sitting still) for a few minutes no longer confirms every motion event; one that arrives or leaves still does. AI also remembers spots where the same kind of thing is seen event after event (a bush or a bin it takes for a person, a parked car): they stop alerting until they move or go.') +
      ovSel('ai_strict', 'Stricter confirmation', [['1', 'on'], ['0', 'off']], c.ai_strict == null ? null : c.ai_strict ? '1' : '0', ai.strict ? 'on' : 'off',
        'Alert only when AI sees the object in 2 of 3 looks in a row and it moved between them. Fewer false alerts; someone passing very quickly may be missed.') +
      ovSel('ai_hide_unconfirmed', 'Show only events AI confirms', [['1', 'on'], ['0', 'off']], c.ai_hide_unconfirmed == null ? null : c.ai_hide_unconfirmed ? '1' : '0', ai.hide_unconfirmed ? 'on' : 'off',
        'Motion where AI found none of the objects that count (a flag or branches in the wind, or only an umbrella) stays out of Events and the timeline. Nothing is deleted and recording is unaffected; Events can still show them.') +
      field('Package detection', onoff('packages', !!c.packages),
        'For doorbells: after someone has been at the door, AmberSight compares the picture from before the visit with one a little after. Something new that looks like a box, parcel or mailer is a delivery ("Package at ..." alert, event and Home Assistant sensor); when it is gone after a later visit, "Package picked up". Uses the smart search picture model (downloaded the first time).')],
    av: [
      field('Audio', onoff('audio', isNew ? true : !!c.audio), 'Sound in the live view and in recordings, when the camera sends it.') +
      field('Text on the picture', text('overlay_text', c.overlay_text || '', 'maxlength="120" placeholder="e.g. {short} · {site}   {date} {time}"'),
        'Shown on this camera in Live and full size. Fill-ins: {name}, {short} (short name), {site} (this server), {date}, {time}, and {v:NAME} for a value an automation sends in (weather or a temperature from Home Assistant, for example) over MQTT (<code>&lt;prefix&gt;/overlay/NAME/set</code>) or <code>POST /api/overlay/NAME</code>. Empty = no text. {v:NAME} shows in live view, not in burned-in recordings.'),
      field('Corner', sel('overlay_pos', [['bottom-left', 'bottom left'], ['bottom-right', 'bottom right'], ['top-left', 'top left'], ['top-right', 'top right']], c.overlay_pos || 'bottom-left')) +
      field('Also burn it into recordings', onoff('overlay', !!c.overlay),
        'The text becomes part of the video files (without text set: name, date and time). This re-encodes the camera\'s video, which uses much more CPU and turns off smooth live view for this camera: Live and the apps fall back to a snapshot that refreshes, with no sound. Recordings are unaffected; leave it off unless they must carry the text.')],
  };
  if (!isNew && !multi && !c.remote) { out.tamper = tamperHtml(c); out.unusual = unusualHtml(c); }
  if (!isNew && !multi && !c.remote && isNetCam(c)) out.network = [netinfoHtml(), ''];
  if (!isNew && !multi && isNetCam(c)) out.watchdog = watchdogHtml(c);
  if (multi) {
    // Per-camera things make no sense in bulk: name, groups, stream URLs, credentials, zones, sounds, watchdog.
    delete out.general;
    out.stream = [out.stream[1], ''];
  }
  return out;
}

function sectionsHtml(c, isNew, multi) {
  const secs = sectionsOf(c, isNew, multi);
  const open = moreState();
  return SECTIONS.filter(([k]) => secs[k]).map(([k, title]) =>
    '<section class="cs-sec" data-p="' + k + '" aria-labelledby="cs-h-' + k + '"><div class="cs-sechd"><h3 id="cs-h-' + k + '">' + title + '</h3><span class="cs-chg" data-chg hidden></span></div>' +
    '<div class="cs-sum" data-sum></div>' +
    '<div class="cs-pane cs-basic">' + secs[k][0] + '</div>' +
    (secs[k][1] ? '<details class="cs-extra" data-more="' + k + '"' + (open[k] ? ' open' : '') + '><summary>More settings</summary><div class="cs-pane">' + secs[k][1] + '</div></details>' : '') +
    '</section>').join('');
}

function motionLevelText(d) {
  const lv = d.querySelector('[data-lv]');
  if (!lv) return '?';
  return lv.value === '' ? 'server\'s setting' : lv.value === 'mixed' ? 'mixed' : (MO_NAMES[lv.value] || 'custom').toLowerCase();
}

// ---------- one-line summaries and "N changed from defaults" ----------
function updateSummaries(d, c, base) {
  const m = (D && D.motion) || {}, ai = (D && D.ai) || {}, g = (D && D.watchdog) || {};
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const chk = (a) => { const el = d.querySelector('[' + a + ']'); return el ? el.checked : false; };
  const num = (k, dflt) => v(k) !== '' ? v(k) : dflt;
  const set = (k, txt) => { const el = d.querySelector('.cs-sec[data-p="' + k + '"] [data-sum]'); if (el) el.textContent = txt; };
  const onOff = (x) => (x ? 'on' : 'off');
  const grp = readGroups(d);
  set('general', [v('name') || 'Unnamed', grp.length ? grp.length + ' group' + (grp.length === 1 ? '' : 's') : 'no groups', d.querySelector('[data-k="enabled"]') && v('enabled') === '0' ? 'disabled' : 'enabled'].join(' · '));
  const src = v('src');
  let where = '';
  if (src === 'net') { try { where = new URL(v('url')).host; } catch (e) { where = v('url') ? 'custom URL' : 'no URL yet'; } }
  set('stream', ({ net: 'Network camera', device: 'Local device', screen: 'Screen', helper: 'Another computer', push: 'Pushed stream' }[src] || '') + (where ? ' · ' + where : '') + (src === 'net' && v('sub_url') ? ' · with sub stream' : ''));
  set('recording', v('record') === '0' ? 'Recording: off' : 'Recording: ' + ({ 0: 'continuous', 1: 'motion only', 2: 'continuous sub stream, main around events' }[v('record_mode')] || 'continuous'));
  const zn = (c.zones || []).length + (c.zone_polys || []).length;
  set('motion', v('motion') === '0' ? 'Motion: off' : 'Motion: ' + (v('motion_algorithm') || m.algorithm || 'objects') + ' · ' + zn + ' zone' + (zn === 1 ? '' : 's') + ' · sensitivity ' + motionLevelText(d) + (v('schedule') ? ' · ' + v('schedule') : ''));
  const al = v('alerts');
  const chans = d.querySelector('[data-chall]');
  set('alerts', 'Alerts: ' + (al === '' ? 'on (default)' : al === '1' ? 'on' : 'off') + ' · ' + (v('alert_schedule') || 'any time') + ' · ' + (v('nf') === 'some' && d.querySelector('[data-nflist] .chip.on') ? 'only ' + [...d.querySelectorAll('[data-nflist] .chip.on')].map(b => b.dataset.nl).join(', ') + ' · ' : '') + (!chans || chans.checked ? 'all services' : d.querySelectorAll('[data-ch]:checked').length + ' service(s)'));
  const aiOn = v('ai') === '' ? !!ai.enabled : v('ai') === '1';
  set('ai', 'AI: ' + onOff(aiOn) + (aiOn ? ' · ' + (v('ai_labels') || (ai.labels || []).join(', ') || 'default objects') + ' · min ' + num('ai_min_score', ai.min_score != null ? ai.min_score : '?') : ''));
  set('av', 'Audio: ' + onOff(v('audio') === '1') + ' · text ' + (v('overlay_text') ? 'set' : 'none'));
  const ws = watchSummaries(d);
  if (ws.tamper) set('tamper', ws.tamper);
  if (ws.unusual) set('unusual', ws.unusual);
  const wdEl = d.querySelector('[data-wd-en]');
  if (wdEl) set('watchdog', 'Watchdog: ' + onOff(wdEl.checked) + ' · restart via ' + (v('wd_reboot') || 'auto') + (v('wd_kind') ? ' · power switch' : ''));
  // "N changed": how many fields of the section differ from the saved values (formSig's order).
  const parts = base ? base.split('\u0001') : null;
  const els = parts ? [...d.querySelectorAll('.cs-body input, .cs-body select, .cs-body textarea')] : [];
  const chips = parts ? [...d.querySelectorAll('.cs-body .chip')] : [];
  d.querySelectorAll('.cs-sec').forEach(sec => {
    let n = 0;
    if (parts && parts.length === els.length + chips.length * 2) {
      els.forEach((el, i) => { if (sec.contains(el) && String(el.type === 'checkbox' ? (el.checked ? 1 : 0) : el.value) !== parts[i]) n++; });
      chips.forEach((b, i) => { if (sec.contains(b) && String(b.classList.contains('on') ? 1 : 0) !== parts[els.length + i * 2 + 1]) n++; });
    }
    const b = sec.querySelector('[data-chg]');
    b.hidden = !n; b.textContent = n + ' changed';
  });
}

// The camera as the form currently describes it (so the strip shows unsaved edits too).
function camFromForm(d, c) {
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : null; };
  const o = Object.assign({}, c);
  if (v('motion') != null) o.motion_enabled = v('motion') === '1';
  if (v('record') != null) o.record = v('record') === '1';
  if (v('record_mode') != null) o.record_motion_only = v('record_mode') === '1';
  if (v('schedule') != null) o.schedule = v('schedule') || null;
  if (v('alert_schedule') != null) o.alert_schedule = v('alert_schedule') || null;
  if (v('alerts') != null) o.alerts = v('alerts') === '' ? null : v('alerts') === '1';
  const en = v('enabled'); if (en != null) o.enabled = en === '1';
  const sn = d.querySelector('[data-snd]'); if (sn) o.audio_trigger = sn.checked;
  return o;
}
function wireArmStrip(d, c, cleanups) {
  const host = d.querySelector('[data-armhost]');
  if (!host) return;
  let strip = null, data = null, t = 0, dead = false;
  const redraw = () => { if (strip) strip.update(camFromForm(d, c), data); };
  const onChange = () => { clearTimeout(t); t = setTimeout(redraw, 250); };
  loadArmData(api).then(dt => {
    if (dead || !host.isConnected) return;
    data = dt; strip = armStrip(host, camFromForm(d, c), dt);
  }).catch(() => { host.textContent = 'Could not work out the schedule.'; });
  d.addEventListener('input', onChange); d.addEventListener('change', onChange);
  cleanups.push(() => { dead = true; clearTimeout(t); if (strip) strip.destroy(); });
}

// ---------- the side panel ----------
let drawer = null;          // the one panel element
let cur = null;             // { key, id, isNew, root, c, cleanup, dirty() }
let openSeq = 0;
let markTimer = 0;
const reduced = () => window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;

function ensureCss() {
  if (document.getElementById('cameraPanelCss')) return;
  const l = document.createElement('link');
  l.id = 'cameraPanelCss'; l.rel = 'stylesheet'; l.href = '/static/css/camera-panel.css';
  document.head.appendChild(l);
}
function ensureDrawer() {
  if (drawer && drawer.isConnected) return drawer;
  ensureCss();
  drawer = document.createElement('aside');
  drawer.className = 'cs-drawer';
  drawer.setAttribute('role', 'dialog');
  drawer.setAttribute('aria-label', 'Camera settings');
  drawer.setAttribute('aria-modal', 'true');
  drawer.tabIndex = -1;
  // Tab stays inside the panel (Esc closes it and gives the focus back to what opened it).
  drawer.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Tab' || !cur) return;
    const f = [...drawer.querySelectorAll('a[href], button:not([disabled]), input:not([disabled]):not([type=hidden]), select:not([disabled]), textarea:not([disabled]), summary, [tabindex]:not([tabindex="-1"])')].filter(e => e.offsetParent !== null);
    if (!f.length) { ev.preventDefault(); drawer.focus(); return; }
    const a = document.activeElement;
    if (ev.shiftKey && (a === f[0] || a === drawer)) { ev.preventDefault(); f[f.length - 1].focus(); }
    else if (!ev.shiftKey && a === f[f.length - 1]) { ev.preventDefault(); f[0].focus(); }
  });
  // Where the panel covers the page (narrow screens), a tap beside it closes it.
  let scrim = document.querySelector('.cs-scrim');
  if (!scrim) {
    scrim = document.createElement('div');
    scrim.className = 'cs-scrim';
    scrim.addEventListener('click', () => { closePanel(); });
    document.body.appendChild(scrim);
  }
  document.body.appendChild(drawer);
  return drawer;
}
// Outline the tile of the camera being edited.
function markTile() {
  document.querySelectorAll('.tile.editing').forEach(t => { if (!cur || t.dataset.id !== String(cur.id)) t.classList.remove('editing'); });
  if (cur && !cur.isNew) { const t = document.querySelector('.tile[data-id="' + cur.id + '"]'); if (t) t.classList.add('editing'); }
}
bus.on('cameras', () => {
  if (!cur) return;
  markTile();
  const c = camFromId(cur.id);
  const dot = cur.root.querySelector('[data-dot]');
  if (c && dot) dot.className = 'cs-dot ' + dotClass(c);
});
const dotClass = (c) => (c.enabled === false ? 'off' : c.online ? 'ok' : 'bad');
const dotTitle = (c) => (c.enabled === false ? 'Disabled' : c.online ? 'Online' : 'Offline');

// Everything the user can change, as one string: dirty = it differs from the opening state.
function formSig(root) {
  const parts = [...root.querySelectorAll('.cs-body input, .cs-body select, .cs-body textarea')].map(el => (el.type === 'checkbox' ? (el.checked ? 1 : 0) : el.value));
  root.querySelectorAll('.cs-body .chip').forEach(b => parts.push(b.dataset.g || b.dataset.sn || b.dataset.nl || '', b.classList.contains('on') ? 1 : 0));
  return parts.join('\u0001');
}

// A three-way "unsaved changes" question: 'save' | 'discard' | 'keep'.
function unsavedDlg(name) {
  return new Promise(res => {
    let done = false;
    const fin = (v) => { if (!done) { done = true; res(v); } d.__close(); };
    const d = openDlg('Unsaved changes', '<div>You changed settings for <b>' + esc(name) + '</b> and have not saved them.</div>' +
      '<div class="row"><button class="primary" data-s>Save</button><button class="btn danger" data-d>Discard changes</button><button class="btn" data-keep>Keep editing</button></div>', true);
    const orig = d.__close;
    d.__close = () => { if (!done) { done = true; res('keep'); } orig(); };
    d.querySelector('[data-s]').onclick = () => fin('save');
    d.querySelector('[data-d]').onclick = () => fin('discard');
    d.querySelector('[data-keep]').onclick = () => fin('keep');
    d.querySelector('.bar button').onclick = () => d.__close();
    d.querySelector('[data-keep]').focus();
  });
}
// True when it is fine to replace/close the panel (nothing unsaved, or the user chose).
async function leave() {
  if (!cur || !cur.dirty()) return true;
  const r = await unsavedDlg(cur.c.name || 'this camera');
  if (r === 'keep') return false;
  if (r === 'save') return await cur.save();
  return true;
}
async function closePanel() {
  if (!cur) return true;
  if (!(await leave())) return false;
  teardown();
  return true;
}
function teardown() {
  if (!cur) return;
  const c = cur;
  cur = null;
  try { c.cleanup(); } catch (e) { /* nothing */ }
  markTile();
  document.body.classList.remove('cs-open');
  if (drawer) {
    drawer.classList.remove('open');
    const dr = drawer;
    setTimeout(() => { if (!cur && dr.isConnected) dr.replaceChildren(); }, 260);
  }
  overlaysChanged();
  const op = c.opener;
  if (op && op.isConnected && typeof op.focus === 'function') { try { op.focus({ preventScroll: true }); } catch (e) { /* gone */ } }
}
backLayer(25, () => !!cur, () => { closePanel(); });
window.addEventListener('keydown', (ev) => {
  if (ev.key !== 'Escape' || !cur || topDlgOpen() || paletteOpen() || ev.defaultPrevented) return;
  if (document.querySelector('.ctxmenu, .ze')) return;
  ev.preventDefault(); ev.stopImmediatePropagation();
  closePanel();
}, true);

function panelHtml(c, isNew) {
  const head = '<div class="cs-head"><div class="cs-title"><span class="cs-dot ' + (isNew ? 'off' : dotClass(c)) + '" data-dot title="' + (isNew ? '' : dotTitle(c)) + '"></span><h2>' + esc(isNew ? 'Add camera' : c.name) + '</h2></div>' +
    '<span class="cs-state" data-state aria-live="polite"></span>' + helpButton('camera') +
    '<button type="button" class="cs-x" data-x title="Close (Esc)" aria-label="Close"><svg class="ic"><use href="#i-x"/></svg></button></div>';
  const secs = sectionsOf(c, isNew);
  const idx = '<div class="cs-idx" role="navigation" aria-label="Sections">' + SECTIONS.filter(([k]) => secs[k]).map(([k, t]) => '<button type="button" data-jump="' + k + '">' + t + '</button>').join('') + '</div>';
  const actions = isNew ? '' : '<div class="cs-acts"><button class="btn" data-copy title="Copy motion, recording, alert or AI settings to other cameras">Copy to...</button><span class="cs-sp"></span><button class="btn danger" data-remove>Remove camera</button></div>';
  return head + idx + '<div class="cs-body" data-body>' + sectionsHtml(c, isNew) + actions + '</div>' +
    '<div class="cs-foot"><div class="asys-err" data-err role="alert"></div><div class="cs-footrow"><button class="primary" data-save disabled>' + (isNew ? 'Add camera' : 'Save') + '</button><button class="btn" data-cancel>Close</button></div></div>';
}

// Build the panel contents for camera c (or a new one) and wire everything.
function render(id, c, isNew, keepScroll, savedNote) {
  const dr = ensureDrawer();
  const opener = cur ? cur.opener : document.activeElement;
  if (cur) { try { cur.cleanup(); } catch (e) { /* nothing */ } }
  const root = document.createElement('div');
  root.className = 'cs-root';
  root.innerHTML = panelHtml(c, isNew);
  dr.replaceChildren(root);
  const d = root;
  const body = d.querySelector('[data-body]');
  const err = d.querySelector('[data-err]');
  const state = { key: isNew ? 'new' : String(id), id, isNew, root, c, opener, cleanup: null, touched: false, base: '' };
  const cleanups = [];
  state.cleanup = () => { cleanups.forEach(f => { try { f(); } catch (e) { /* nothing */ } }); clearInterval(markTimer); };
  cur = state;
  document.body.classList.add('cs-open');

  d.__inView = (k) => {
    const sec = d.querySelector('.cs-sec[data-p="' + k + '"]');
    if (!sec) return false;
    const a = sec.getBoundingClientRect(), b = body.getBoundingClientRect();
    return a.bottom > b.top && a.top < b.bottom && !document.hidden;
  };
  wireSource(d, c);
  wireGroups(d);
  wireAlerts(d);
  wireTimeRanges(d);
  wireArmStrip(d, c, cleanups);
  const stopSound = wireSound(d, c);
  if (stopSound) cleanups.push(stopSound);
  if (!isNew) wireWatchdog(d, c);
  if (!isNew) { const stopN = wireNetinfo(d, c); if (stopN) cleanups.push(stopN); }
  if (!isNew) cleanups.push(wirePreview(d, id));
  const zb = d.querySelector('[data-zedit]');
  if (zb) zb.onclick = () => openZoneEditor(id, {
    opener: zb,
    onSaved: (cam) => { const z = d.querySelector('[data-zsum]'); if (z) z.textContent = zonesSummary(cam); },
  });

  const qb = d.querySelector('[data-qedit]');
  if (qb) qb.onclick = () => openZoneEditor(id, {
    mode: 'quiet', opener: qb,
    onSaved: (cam) => { const z = d.querySelector('[data-qsum]'); if (z) z.textContent = quietSummary((cam.alert_ignore_polys || []).length); },
  });

  // "Default" buttons clear an override; typing shows them again.
  d.addEventListener('click', (e) => {
    const r = e.target.closest('[data-reset]'); if (!r) return;
    const inp = d.querySelector('[data-k="' + r.dataset.reset + '"]'); inp.value = ''; r.hidden = true; inp.focus();
    inp.dispatchEvent(new Event('input', { bubbles: true }));
  });
  d.addEventListener('input', (e) => {
    const k = e.target.dataset && e.target.dataset.k;
    if (k) { const r = d.querySelector('[data-reset="' + k + '"]'); if (r) r.hidden = e.target.value.trim() === ''; }
    if (e.target.classList && e.target.classList.contains('bad') && e.isTrusted) clearFieldErr(e.target);
  });

  wireMotionLevel(d);

  // dirty tracking and the header's save state
  const stEl = d.querySelector('[data-state]');
  const saveBtn = d.querySelector('[data-save]');
  const rebase = () => { state.base = formSig(d); };
  rebase();
  const rebaseT = setTimeout(() => { if (!state.touched) rebase(); }, 900);
  cleanups.push(() => clearTimeout(rebaseT));
  state.dirty = () => state.touched && formSig(d) !== state.base;
  let saved = !!savedNote;
  const paint = () => {
    const dirty = state.dirty();
    saveBtn.disabled = !dirty;
    stEl.className = 'cs-state' + (dirty ? ' dirty' : saved ? ' saved' : '');
    stEl.textContent = dirty ? 'Unsaved changes' : saved ? 'Saved' : '';
    updateSummaries(d, c, state.base);
  };
  const touch = (e) => { if (e.isTrusted) { state.touched = true; saved = false; } paint(); };
  d.addEventListener('input', touch);
  d.addEventListener('change', touch);
  d.addEventListener('click', (e) => { if (e.isTrusted && e.target.closest('.chip, [data-reset], .cs-reset, [data-newkey]')) { state.touched = true; saved = false; } setTimeout(paint, 0); });
  paint();
  // A switch in the Tamper or Unusual section saves by itself; the form then counts that section as saved.
  const ctx = {
    rebase: (sec) => {
      const els = [...d.querySelectorAll('.cs-body input, .cs-body select, .cs-body textarea')];
      const chips = [...d.querySelectorAll('.cs-body .chip')];
      const parts = state.base.split('\u0001');
      els.forEach((el, i) => { if (sec.contains(el)) parts[i] = el.type === 'checkbox' ? (el.checked ? 1 : 0) : el.value; });
      chips.forEach((b, i) => { if (sec.contains(b)) parts[els.length + i * 2 + 1] = b.classList.contains('on') ? 1 : 0; });
      state.base = parts.join('\u0001');
      paint();
    },
  };
  if (!isNew) {
    const stopT = wireTamper(d, c, ctx); if (stopT) cleanups.push(stopT);
    wireUnusual(d, c, ctx);
    wireSpots(d, c);
    cleanups.push(wireOnvif(d, c, ctx));
  }

  // section index
  const idxBtns = [...d.querySelectorAll('[data-jump]')];
  const secEls = [...d.querySelectorAll('.cs-sec')];
  const hdH = () => 0;
  idxBtns.forEach(b => {
    b.onclick = () => {
      const sec = d.querySelector('.cs-sec[data-p="' + b.dataset.jump + '"]');
      body.scrollTo({ top: sec.offsetTop - hdH(), behavior: reduced() ? 'auto' : 'smooth' });
    };
  });
  let curSec = '';
  const onScroll = () => {
    let k = secEls[0].dataset.p;
    for (const s of secEls) if (s.offsetTop - 24 <= body.scrollTop) k = s.dataset.p;
    if (body.scrollTop + body.clientHeight >= body.scrollHeight - 2) k = secEls[secEls.length - 1].dataset.p;
    if (k === curSec) return;
    curSec = k;
    idxBtns.forEach(b => { const on = b.dataset.jump === k; b.classList.toggle('on', on); if (on) b.setAttribute('aria-current', 'true'); else b.removeAttribute('aria-current'); if (on) { const nav = b.parentNode; const l = b.offsetLeft - nav.clientWidth / 2 + b.offsetWidth / 2; nav.scrollTo({ left: l }); } });
  };
  body.addEventListener('scroll', onScroll, { passive: true });
  d.querySelectorAll('details.cs-more').forEach(det => det.addEventListener('toggle', () => { setMore(det.dataset.more, det.open); onScroll(); }));
  if (keepScroll) body.scrollTop = keepScroll;
  onScroll();

  // buttons
  d.querySelector('[data-x]').onclick = () => closePanel();
  d.querySelector('[data-cancel]').onclick = () => closePanel();
  const doSave = async () => {
    err.textContent = '';
    d.querySelectorAll('.cs-ferr').forEach(x => x.remove());
    d.querySelectorAll('.bad').forEach(x => x.classList.remove('bad'));
    if (!validate(d, isNew)) return false;
    const rawBody = readForm(d, c, isNew);
    if (!isNew && !rawBody.password) delete rawBody.password;
    saveBtn.disabled = true; stEl.className = 'cs-state'; stEl.textContent = 'Saving...';
    try {
      if (isNew) await api('/api/cameras', { json: rawBody });
      else await api('/api/cameras/' + id, { method: 'PUT', json: rawBody });
    } catch (e) {
      showServerError(d, e.body || e.message, err);
      paint();
      return false;
    }
    toast(isNew ? 'Camera added' : 'Saved ' + rawBody.name, { kind: 'ok' });
    if (isNew) { cur = state; teardown(); await refreshCameras(); return true; }
    const scroll = body.scrollTop;
    await refreshCameras();
    const nc = camFromId(id);
    if (cur === state && nc) render(id, nc, false, scroll, true);
    return true;
  };
  state.save = doSave;
  saveBtn.onclick = doSave;
  d.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && !saveBtn.disabled) { e.preventDefault(); doSave(); } });
  const rm = d.querySelector('[data-remove]');
  if (rm) rm.onclick = () => confirmRemove(id, () => { cur = state; teardown(); });
  const cp = d.querySelector('[data-copy]');
  if (cp) cp.onclick = () => copyDlg(d, c);

  markTile();
  clearInterval(markTimer);
  markTimer = setInterval(markTile, 1500);
  const first = !dr.classList.contains('open');
  if (first) {
    dr.classList.remove('open');
    void dr.offsetWidth;
    // Below the header on wide screens, so the shield, health chip and clock stay visible.
    const hd = document.querySelector('header');
    document.documentElement.style.setProperty('--hdr-h', (hd ? hd.offsetHeight : 0) + 'px');
    requestAnimationFrame(() => dr.classList.add('open'));
    overlaysChanged();
  }
  if (first || isNew) setTimeout(() => { const f = isNew ? d.querySelector('[data-k="name"]') : null; (f || dr).focus({ preventScroll: true }); }, 30);
  return state;
}

// Client-side checks with the errors next to the fields. True when fine.
function validate(d, isNew) {
  let first = null;
  const bad = (k, msg) => { const el = d.querySelector('[data-k="' + k + '"]') || d.querySelector('[data-k="src"]'); const x = fieldErr(el, msg); if (!first) first = el; return x; };
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  if (!v('name')) bad('name', 'Give the camera a name.');
  const src = v('src');
  if (!sourceUrl(d) && src !== 'helper') {
    if (src === 'device') bad('dev', 'Pick a capture device (or type its name).');
    else bad('url', 'A stream URL is needed.');
  } else if (src === 'helper' && !readHelper(d)) bad('src', 'Pick a computer and what to capture.');
  d.querySelectorAll('[data-trf]').forEach(box => {
    const raw = box.querySelector('[data-trraw] input');
    if (raw.value.trim() && !trParse(raw.value)) bad(raw.dataset.k, 'Write it like 07:00-21:00 (hours 00-23).');
  });
  if (first) {
    first.scrollIntoView({ block: 'center', behavior: reduced() ? 'auto' : 'smooth' });
    try { if (first.type !== 'hidden' && !first.hidden) first.focus({ preventScroll: true }); } catch (e) { /* hidden */ }
    return false;
  }
  return true;
}
// The server names the setting in its message: put it next to that field when we can.
function showServerError(d, msg, errBox) {
  let text = msg;
  try { const j = JSON.parse(msg); if (j && j.error) text = j.error; } catch (e) { /* plain text */ }
  const map = [[/alert window/i, 'alert_schedule'], [/no-motion watch/i, 'no_motion_schedule'], [/arm window|motion schedule/i, 'schedule'], [/^name\b|camera name|name is/i, 'name'], [/stream url|\burl\b/i, 'url']];
  for (const [re, k] of map) {
    const el = d.querySelector('[data-k="' + k + '"]');
    if (re.test(text) && el) { fieldErr(el, text); el.scrollIntoView({ block: 'center' }); errBox.textContent = ''; return; }
  }
  errBox.textContent = text;
}

// A live picture while the Motion section is in view, about once a second.
function wirePreview(d, id) {
  const box = d.querySelector('[data-prev]');
  if (!box) return () => {};
  const img = box.querySelector('img'), st = box.querySelector('[data-pst]'), none = box.querySelector('[data-pnone]');
  let stop = false, timer = 0;
  const tick = () => {
    if (stop || !d.isConnected) return;
    const c = camFromId(id);
    const mo = !!(c && c.motion);
    st.textContent = mo ? 'Motion now' : 'Quiet';
    st.classList.toggle('now', mo);
    if (!d.__inView || !d.__inView('motion') || (c && c.enabled === false)) { timer = setTimeout(tick, 1000); return; }
    const n = new Image();
    n.onload = () => { img.src = n.src; img.hidden = false; none.hidden = true; timer = setTimeout(tick, 1000); };
    n.onerror = () => { img.hidden = true; none.hidden = false; timer = setTimeout(tick, 3000); };
    n.src = '/api/snapshot/' + id + '?t=' + Date.now();
  };
  tick();
  return () => { stop = true; clearTimeout(timer); };
}

// The settings panel for one camera (id), or for a new camera (id null).
export async function openCameraDialog(id) {
  if (!state.isAdmin) { msgDlg('View-only account: camera settings need an admin sign-in'); return; }
  const isNew = id == null;
  const key = isNew ? 'new' : String(id);
  if (cur && cur.key === key) { ensureDrawer().focus({ preventScroll: true }); return; }
  const c = isNew ? { fps: 10, scale: 640 } : camFromId(id);
  if (!c) { msgDlg('That camera no longer exists'); return; }
  const seq = ++openSeq;
  if (!(await leave())) return;
  try { D = await api('/api/settings'); } catch (e) { D = null; }
  if (seq !== openSeq) return;
  const keep = cur;
  const c2 = isNew ? c : (camFromId(id) || c);
  render(id, c2, isNew);
  if (keep && keep !== cur) { /* replaced: its timers were stopped in render */ }
}

// ---------- several cameras at once ----------
// Same sections and fields as the single panel. A field whose value differs between the chosen
// cameras shows "Mixed" and is left alone unless the user edits it; only edited fields are
// sent, one PUT per camera with just those keys.
const MIXED = '\u0001mixed';
// data-k of a field -> the config keys it writes (default: the same name).
const BULK_KEYS = { record_mode: ['record_motion_only', 'record_sub'] };
const BULK_BASE = (v, num) => ({
  record: v('record') === '1', record_motion_only: v('record_mode') === '1', record_sub: v('record_mode') === '2', direct: v('direct') === '1',
  on_demand: v('on_demand') === '1', idle_timeout: Math.min(604800, Math.max(30, num('idle_timeout', 300) || 300)), motion: v('motion') === '1',
  fps: num('fps', 10), scale: num('scale', 640), audio: v('audio') === '1', overlay: v('overlay') === '1', packages: v('packages') === '1' ? true : null,
  overlay_text: v('overlay_text') || null, overlay_pos: v('overlay_pos') || null,
});
// What each chosen camera's form would hold, per field.
function fieldValues(c) {
  const box = document.createElement('div');
  box.innerHTML = sectionsHtml(c, false, true);
  const m = {};
  box.querySelectorAll('[data-k]').forEach(el => { m[el.dataset.k] = el.value; });
  return m;
}
function bulkBody(d, touched) {
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const num = (k, dflt) => { const x = v(k); return x === '' ? dflt : Number(x); };
  const o = readOverrides(d);
  const all = Object.assign(BULK_BASE(v, num), o.motion, o.recording, o.alerts, o.ai);
  const body = {};
  for (const k of touched) {
    const el = d.querySelector('[data-k="' + k + '"]');
    if (el && el.value === MIXED) continue;      // touched, then put back to Mixed
    for (const bk of (BULK_KEYS[k] || [k])) if (bk in all) body[bk] = all[bk];
  }
  return body;
}

export async function openCamerasBulk(ids) {
  if (!state.isAdmin) { msgDlg('View-only account: camera settings need an admin sign-in'); return; }
  const cams = ids.map(camFromId).filter(Boolean);
  if (cams.length < 1) return;
  const seq = ++openSeq;
  if (!(await leave())) return;
  try { D = await api('/api/settings'); } catch (e) { D = null; }
  if (seq !== openSeq) return;
  renderMulti(cams);
}

function renderMulti(cams) {
  const dr = ensureDrawer();
  const opener = cur ? cur.opener : document.activeElement;
  if (cur) { try { cur.cleanup(); } catch (e) { /* nothing */ } }
  const n = cams.length;
  const root = document.createElement('div');
  root.className = 'cs-root';
  const secs = sectionsOf(cams[0], false, true);
  const keys = SECTIONS.filter(([k]) => secs[k]);
  root.innerHTML = '<div class="cs-head"><div class="cs-title"><h2>Edit ' + n + ' camera' + (n === 1 ? '' : 's') + '</h2></div><span class="cs-state" data-state aria-live="polite"></span>' +
    '<button type="button" class="cs-x" data-x title="Close (Esc)" aria-label="Close"><svg class="ic"><use href="#i-x"/></svg></button></div>' +
    '<div class="cs-idx" role="navigation" aria-label="Sections">' + keys.map(([k, t]) => '<button type="button" data-jump="' + k + '">' + t + '</button>').join('') + '</div>' +
    '<div class="cs-body" data-body><div class="cs-bulkhint"><b>' + cams.map(c => esc(c.name)).join(', ') + '</b><div class="hint">A field that differs between these cameras says Mixed and is left alone. Change a field and it is set on all of them; nothing else is touched. Name, groups, stream, sign-in, zones and sounds are per camera, so they are edited one at a time.</div></div>' +
    sectionsHtml(cams[0], false, true) + '<div class="cs-bulkres" data-res hidden></div></div>' +
    '<div class="cs-foot"><div class="asys-err" data-err role="alert"></div><div class="cs-footrow"><button class="primary" data-save disabled>Save to ' + n + ' camera' + (n === 1 ? '' : 's') + '</button><button class="btn" data-cancel>Close</button></div></div>';
  dr.replaceChildren(root);
  const d = root, body = d.querySelector('[data-body]');
  const st = { key: 'multi', id: null, isNew: false, multi: true, root, c: { name: n + ' cameras' }, opener, touched: false, base: '' };
  const cleanups = [];
  st.cleanup = () => { cleanups.forEach(f => { try { f(); } catch (e) { /* nothing */ } }); clearInterval(markTimer); };
  cur = st;
  document.body.classList.add('cs-open');
  wireTimeRanges(d);

  // Which fields differ across the cameras?
  const vals = cams.map(fieldValues);
  const mixed = new Set();
  Object.keys(vals[0]).forEach(k => { if (vals.some(m => m[k] !== vals[0][k])) mixed.add(k); });
  mixed.forEach(k => {
    const el = d.querySelector('[data-k="' + k + '"]');
    if (!el) return;
    el.classList.add('mixed');
    if (el.tagName === 'SELECT') {
      const o = document.createElement('option');
      o.value = MIXED; o.textContent = 'Mixed'; el.insertBefore(o, el.firstChild); el.value = MIXED;
    } else if (el.closest('[data-trf]')) {
      const box = el.closest('[data-trf]');
      el.value = '';
      box.querySelector('[data-tron]').checked = false;
      box.querySelector('[data-trpick]').hidden = true; box.querySelector('[data-trraw]').hidden = true;
      box.dataset.mixed = '1';
      const note = box.querySelector('[data-trnote]');
      note.textContent = 'Mixed: tick to set the same window on all ';
      const none = document.createElement('button');
      none.type = 'button'; none.className = 'cs-reset'; none.textContent = 'Remove it on all';
      none.onclick = () => { box.dataset.none = '1'; note.textContent = 'The window will be removed on all'; touched.add(box.dataset.trf); paint(); };
      box.dataset.noteMixed = note.textContent;
      note.after(none); box.__none = none;
    } else {
      el.value = ''; el.placeholder = 'Mixed';
      const r = d.querySelector('[data-reset="' + k + '"]'); if (r) { r.hidden = false; r.title = 'Use the default on all'; }
    }
  });
  wireMotionLevel(d);
  // Mixed selects: a stored choice replaces the Mixed option on change; an override select with Default keeps both.
  const touched = new Set();
  const keyOf = (t) => (t.dataset && t.dataset.k) || (t.closest && t.closest('[data-trf]') && t.closest('[data-trf]').dataset.trf) || '';
  const stEl = d.querySelector('[data-state]'), saveBtn = d.querySelector('[data-save]'), err = d.querySelector('[data-err]'), res = d.querySelector('[data-res]');
  const paint = () => {
    saveBtn.disabled = touched.size === 0;
    stEl.className = 'cs-state' + (touched.size ? ' dirty' : '');
    stEl.textContent = touched.size ? touched.size + ' field' + (touched.size === 1 ? '' : 's') + ' to change' : '';
    d.querySelectorAll('.cs-sec').forEach(sec => {
      const c = [...sec.querySelectorAll('[data-k]')].filter(el => touched.has(el.dataset.k)).length;
      const b = sec.querySelector('[data-chg]'); b.hidden = !c; b.textContent = c + ' to change';
      const sm = sec.querySelector('[data-sum]'); if (sm) sm.textContent = '';
    });
  };
  const mark = (e) => {
    const k = keyOf(e.target);
    const tb = e.target.closest && e.target.closest('[data-trf]');
    if (tb && tb.dataset.mixed) {
      // Mixed windows: ticking "Only between" then unticking goes back to Mixed (nothing is sent),
      // unless "Remove it on all" was chosen on purpose.
      const on = tb.querySelector('[data-tron]');
      if (on.checked) delete tb.dataset.none;
      else if (!tb.dataset.none) { touched.delete(k); tb.querySelector('[data-trnote]').textContent = tb.dataset.noteMixed; paint(); return; }
    }
    if (k && k !== 'newgroup') touched.add(k);
    paint();
  };
  d.addEventListener('input', mark); d.addEventListener('change', mark);
  d.addEventListener('click', (e) => {
    const r = e.target.closest('[data-reset]');
    if (!r) return;
    const inp = d.querySelector('[data-k="' + r.dataset.reset + '"]'); inp.value = ''; inp.placeholder = 'Default'; r.hidden = true;
    touched.add(r.dataset.reset); paint();
  });
  d.querySelectorAll('details.cs-more').forEach(det => { det.open = true; });
  st.dirty = () => touched.size > 0;
  paint();
  const idxBtns = [...d.querySelectorAll('[data-jump]')];
  idxBtns.forEach(b => { b.onclick = () => { const sec = d.querySelector('.cs-sec[data-p="' + b.dataset.jump + '"]'); body.scrollTo({ top: sec.offsetTop, behavior: reduced() ? 'auto' : 'smooth' }); }; });

  const doSave = async () => {
    err.textContent = ''; res.hidden = true;
    d.querySelectorAll('.cs-ferr').forEach(x => x.remove());
    let bad = null;
    d.querySelectorAll('[data-trf]').forEach(box => {
      const raw = box.querySelector('[data-trraw] input');
      if (touched.has(box.dataset.trf) && raw.value.trim() && !trParse(raw.value)) bad = bad || fieldErr(raw, 'Write it like 07:00-21:00 (hours 00-23).');
    });
    if (bad) return false;
    const patch = bulkBody(d, touched);
    if (!Object.keys(patch).length) { paint(); return true; }
    saveBtn.disabled = true;
    const fails = [];
    let done = 0;
    for (const c of cams) {
      stEl.className = 'cs-state'; stEl.textContent = 'Saving ' + (done + 1) + ' of ' + n + '...';
      try { await api('/api/cameras/' + c.id, { method: 'PUT', json: patch }); }
      catch (e) { fails.push([c.name, errText(e)]); }
      done++;
    }
    await refreshCameras();
    res.hidden = false;
    res.innerHTML = '<div class="' + (fails.length ? 'bad' : 'ok') + '"><b>' + (n - fails.length) + ' of ' + n + ' saved.</b></div>' +
      fails.map(f => '<div class="cs-bulkfail"><b>' + esc(f[0]) + '</b>: ' + esc(f[1]) + '</div>').join('');
    res.scrollIntoView({ block: 'nearest' });
    if (!fails.length) { touched.clear(); stEl.className = 'cs-state saved'; stEl.textContent = 'Saved'; toast('Saved ' + n + ' cameras', { kind: 'ok' }); renderMulti(cams.map(c => camFromId(c.id) || c)); return true; }
    stEl.className = 'cs-state dirty'; stEl.textContent = fails.length + ' failed';
    saveBtn.disabled = false;
    return false;
  };
  st.save = doSave;
  saveBtn.onclick = doSave;
  d.querySelector('[data-x]').onclick = () => closePanel();
  d.querySelector('[data-cancel]').onclick = () => closePanel();
  d.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && !saveBtn.disabled) { e.preventDefault(); doSave(); } });
  markTile();
  const first = !dr.classList.contains('open');
  if (first) {
    dr.classList.remove('open'); void dr.offsetWidth;
    const hd = document.querySelector('header');
    document.documentElement.style.setProperty('--hdr-h', (hd ? hd.offsetHeight : 0) + 'px');
    requestAnimationFrame(() => dr.classList.add('open'));
    overlaysChanged();
    setTimeout(() => dr.focus({ preventScroll: true }), 30);
  }
}

function confirmRemove(id, after) {
  const c = camFromId(id);
  if (!c) return;
  const d = openDlg('Remove camera', '<div>Remove <b>' + esc(c.name) + '</b>? Its recordings stay on disk until they age out.</div>' +
    '<div class="asys-err" data-err></div><div class="row"><button class="primary danger" data-yes>Remove</button><button class="btn" data-no>Cancel</button></div>', true);
  d.querySelector('[data-no]').onclick = () => d.__close();
  d.querySelector('[data-yes]').onclick = async () => {
    try { await api('/api/cameras/' + id, { method: 'DELETE' }); d.__close(); if (after) after(); toast('Removed ' + c.name); await refreshCameras(); }
    catch (e) { d.querySelector('[data-err]').textContent = e.body || e.message; }
  };
}

export async function setCameraEnabled(id, on) {
  try { await api('/api/cameras/' + id + '/enable', { json: { enabled: on } }); await refreshCameras(); }
  catch (e) { msgDlg(e.message); }
}

// ---------- pause a camera's notifications for a while ----------
// Motion, recording and events carry on; only the notifications stop.
const lbl = (c) => c.short_name || c.name;
export function pausedText(c) {
  if (!c.alerts_paused) return '';
  if (!c.alerts_paused_until) return 'until resumed';
  const end = new Date(c.alerts_paused_until * 1000);
  const mins = Math.max(1, Math.round((end - Date.now()) / 60000));
  return 'until ' + end.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) + ' (' + (mins >= 60 ? Math.floor(mins / 60) + ' h ' + (mins % 60) + ' min' : mins + ' min') + ' left)';
}
async function setPause(c, minutes) {
  try {
    await api('/api/cameras/' + c.id + '/pause', { json: { minutes } });
    await refreshCameras();
    const cur = camFromId(c.id) || c;
    toast(minutes === 0 ? 'Notifications on again for ' + lbl(cur) : 'Notifications paused for ' + lbl(cur) + ' ' + pausedText(cur) + '. It still records.');
  } catch (e) { msgDlg(e.message); }
}
export function resumeAlerts(c) { return setPause(c, 0); }
export function pauseAlertsFor(c, minutes) { return setPause(c, minutes); }
export function pauseAlertsDlg(c) {
  const opts = [[30, '30 min'], [60, '1 hour'], [120, '2 hours'], [240, '4 hours'], [480, '8 hours'], [null, 'Until I resume']];
  const d = openDlg('Pause notifications', '<div class="hint"><b>' + esc(lbl(c)) + '</b>: no notifications for a while. ' +
    'It is still recording, detecting motion and listing events (and MQTT still reports them).</div>' +
    (c.alerts_paused ? '<div class="hint">Notifications paused ' + esc(pausedText(c)) + '.</div>' : '') +
    '<div class="pausebtns">' + opts.map((o, i) => '<button class="btn" data-i="' + i + '">' + o[1] + '</button>').join('') + '</div>' +
    (c.alerts_paused ? '<div class="pausebtns"><button class="btn primary" data-resume>Resume notifications now</button></div>' : ''), true);
  d.querySelectorAll('[data-i]').forEach(b => { b.onclick = () => { d.__close(); setPause(c, opts[+b.dataset.i][0]); }; });
  const r = d.querySelector('[data-resume]');
  if (r) r.onclick = () => { d.__close(); setPause(c, 0); };
}

// Create a camera from a ready config (Discovery uses this); refreshes the list.
export async function addCameraConfig(body) {
  if (!state.isAdmin) throw new Error('view-only account');
  const r = await api('/api/cameras', { json: body });
  await refreshCameras();
  return r;
}

// The camera list inside Settings > Cameras. Re-renders itself while it is on screen.
// This server's own cameras (other servers' are edited there).
const own = () => state.cams.filter(c => !c.remote);

export function mountCameraList(host) {
  ensureCss();
  const picked = new Set();   // ids ticked for bulk editing
  // Paired computers (capture helpers) under the camera list.
  async function drawHelpers() {
    let box = host.querySelector('[data-helpers]');
    if (!box) { box = document.createElement('div'); box.dataset.helpers = '1'; host.appendChild(box); }
    let list = [];
    try { list = (await api('/api/helpers')).helpers || []; } catch (e) { return; }
    box.innerHTML = helpersListHtml(list);
    box.querySelectorAll('[data-hren]').forEach(b => {
      b.onclick = () => {
        const h = list.find(x => x.id === b.dataset.hren);
        const dd = openDlg('Rename computer', '<label>Name <input data-n value="' + esc(h.name) + '"></label><div class="asys-err" data-err></div><div class="row"><button class="primary" data-ok>Save</button><button class="btn" data-no>Cancel</button></div>', true);
        dd.querySelector('[data-no]').onclick = () => dd.__close();
        dd.querySelector('[data-ok]').onclick = async () => {
          try { await api('/api/helpers/' + encodeURIComponent(h.id), { method: 'PUT', json: { name: dd.querySelector('[data-n]').value } }); dd.__close(); drawHelpers(); }
          catch (e) { dd.querySelector('[data-err]').textContent = e.body || e.message; }
        };
      };
    });
    box.querySelectorAll('[data-hdel]').forEach(b => {
      b.onclick = () => {
        const h = list.find(x => x.id === b.dataset.hdel);
        const dd = openDlg('Remove computer', '<div>Remove <b>' + esc(h.name) + '</b>? It stops sending at once and has to be added again to come back. Cameras that used it stop receiving.</div><div class="asys-err" data-err></div><div class="row"><button class="primary danger" data-ok>Remove</button><button class="btn" data-no>Cancel</button></div>', true);
        dd.querySelector('[data-no]').onclick = () => dd.__close();
        dd.querySelector('[data-ok]').onclick = async () => {
          try { await api('/api/helpers/' + encodeURIComponent(h.id), { method: 'DELETE' }); dd.__close(); toast(h.name + ' removed'); drawHelpers(); }
          catch (e) { dd.querySelector('[data-err]').textContent = e.body || e.message; }
        };
      };
    });
  }
  // What the camera is, in plain words: "RTSP \u00b7 host" (the full address is in its panel).
  function srcLabel(c) {
    const m = /^([a-z0-9+.-]+):\/\/(.*)$/i.exec(c.url || '');
    if (!m) return '';
    // The host only. A password may hold an unencoded '@', '/' or ':', so cut at the LAST '@' before
    // any query (a query may itself hold '@'), then take the host up to its port or path.
    const pre = m[2].split(/[?#]/)[0];
    const rest = pre.slice(pre.lastIndexOf('@') + 1);
    m[2] = rest.startsWith('[') ? rest.slice(0, rest.indexOf(']') + 1) : rest.split(/[:\/]/)[0];
    const s = m[1].toLowerCase();
    const kinds = { rtsp: 'RTSP', rtsps: 'RTSP', http: 'Web stream', https: 'Web stream', rtmp: 'RTMP', lavfi: 'Test picture', device: 'Attached camera', screen: 'Screen', helper: 'Other computer' };
    const k = kinds[s] || m[1].toUpperCase();
    return m[2] && !['lavfi', 'device', 'screen'].includes(s) ? k + ' \u00b7 ' + m[2] : k;
  }
  function draw() {
    const n = own().length;
    const touch = !!(window.matchMedia && window.matchMedia('(pointer: coarse)').matches);
    host.innerHTML =
      '<div class="cs-listbar"><span class="hint">' + n + ' camera' + (n === 1 ? '' : 's') + '. ' + (touch ? 'Tap a camera\'s gear for its settings.' : 'Right-click a camera (or its gear) in the live view for the same settings.') + '</span>' +
      '<a class="btn" href="#settings/cameras/find">Find cameras</a><button class="btn primary" data-add>Add by address</button></div>' +
      (n > 1 ? '<label class="cs-all asys-chk"><input type="checkbox" data-all aria-label="Select all cameras"> Select all</label>' : '') +
      '<div class="cs-list">' + state.cams.filter(c => !c.remote).map(c => {
        const off = c.enabled === false;
        const st = off ? '<span class="badge">off</span>' : c.online ? '<span class="badge on dot">online</span>' : c.sleeping ? '<span class="badge info dot" title="A battery camera asleep to save power. It wakes for motion or when you open it.">asleep</span>' : '<span class="badge off dot">offline</span>';
        return '<div class="cs-item"><label class="cs-pick" title="Select for editing several cameras"><input type="checkbox" data-pick="' + c.id + '"' + (picked.has(c.id) ? ' checked' : '') + ' aria-label="Select ' + esc(c.name) + '"></label>' +
          '<div class="cs-name"><b title="' + esc(c.name) + '">' + esc(c.name) + '</b>' + '' +
          '<div class="cs-url">' + esc(srcLabel(c) + ((c.groups || []).length ? ' \u00b7 ' + c.groups.join(', ') : '')) + '</div>' +
          (c.alerts_paused ? '<div class="cs-np"><span class="badge paused" title="' + esc('Notifications paused ' + pausedText(c) + '. It is still recording.') + '">' + ic('belloff') + ' Notifications paused \u00b7 still recording</span></div>' : '') + '</div>' + st +
          '<span class="cs-acts"><button class="btn" data-set="' + c.id + '">Settings</button>' +
          '<label class="sw" title="Off stops live view and recording; old footage is kept"><input type="checkbox" role="switch" data-en="' + c.id + '"' + (off ? '' : ' checked') + ' aria-label="' + esc(c.name) + ' on"><span class="sw-track" aria-hidden="true"></span><span class="sw-text">' + (off ? 'Off' : 'On') + '</span></label></span></div>';
      }).join('') + '</div><div class="cs-bulkbar" data-bulk hidden></div>' +
      '<div class="cs-aside"><button class="btn link" data-addpc title="Record another computer\'s screen with AmberSight">Capture from another computer</button></div>';
    host.querySelector('[data-add]').onclick = () => openCameraDialog(null);
    host.querySelector('[data-addpc]').onclick = () => pairDialog(() => drawHelpers());
    const ids = own().map(c => c.id);
    for (const id of [...picked]) if (!ids.includes(id)) picked.delete(id);
    const all = host.querySelector('[data-all]');
    const bar = host.querySelector('[data-bulk]');
    const paintBar = () => {
      const k = picked.size;
      if (all) { all.checked = k > 0 && k === ids.length; all.indeterminate = k > 0 && k < ids.length; }
      bar.hidden = !k;
      bar.innerHTML = k ? '<b>' + k + ' selected</b><span class="cs-sp"></span><button class="primary" data-bedit>Edit ' + k + ' camera' + (k === 1 ? '' : 's') + '</button>' +
        '<button class="btn" data-ben="1">Enable</button><button class="btn" data-ben="0">Disable</button><button class="btn" data-bclr>Clear</button><span class="cs-bres" data-bres></span>' : '';
      if (!k) return;
      bar.querySelector('[data-bedit]').onclick = () => openCamerasBulk([...picked]);
      bar.querySelector('[data-bclr]').onclick = () => { picked.clear(); host.querySelectorAll('[data-pick]').forEach(x => { x.checked = false; }); paintBar(); };
      bar.querySelectorAll('[data-ben]').forEach(b => { b.onclick = async () => {
        const on = b.dataset.ben === '1', res = bar.querySelector('[data-bres]');
        bar.querySelectorAll('button').forEach(x => { x.disabled = true; });
        const fails = [];
        let i = 0;
        for (const id of [...picked]) {
          res.textContent = (on ? 'Enabling ' : 'Disabling ') + (++i) + ' of ' + picked.size + '...';
          try { await api('/api/cameras/' + id + '/enable', { json: { enabled: on } }); } catch (e) { const c = camFromId(id); fails.push((c ? c.name : id) + ': ' + errText(e)); }
        }
        await refreshCameras();
        toast(fails.length ? 'Some cameras failed: ' + fails.join('; ') : (on ? 'Enabled ' : 'Disabled ') + i + ' camera' + (i === 1 ? '' : 's'), { kind: fails.length ? undefined : 'ok' });
        bar.querySelectorAll('button').forEach(x => { x.disabled = false; });
        const r2 = bar.querySelector('[data-bres]'); if (r2) r2.textContent = fails.join('; ');
      }; });
    };
    host.querySelectorAll('[data-pick]').forEach(x => { x.onchange = () => { if (x.checked) picked.add(+x.dataset.pick); else picked.delete(+x.dataset.pick); paintBar(); }; });
    if (all) all.onchange = () => { ids.forEach(id => { if (all.checked) picked.add(id); else picked.delete(id); }); host.querySelectorAll('[data-pick]').forEach(x => { x.checked = all.checked; }); paintBar(); };
    paintBar();
    drawHelpers();
    host.querySelectorAll('[data-set]').forEach(b => { b.onclick = () => openCameraDialog(+b.dataset.set); });
    host.querySelectorAll('[data-en]').forEach(b => { b.onchange = async () => { const c = camFromId(+b.dataset.en); if (c) { await setCameraEnabled(c.id, b.checked); const c2 = camFromId(c.id); if (c2) b.checked = c2.enabled !== false; } }; });
  }
  // Only redraw when the list itself changed (status polls every few seconds).
  let sig = JSON.stringify(state.cams.map(c => [c.id, c.name, c.groups, c.url, c.enabled, c.online]));
  draw();
  const onCams = () => {
    if (!host.isConnected) { bus.off('cameras', onCams); return; }
    const s = JSON.stringify(state.cams.map(c => [c.id, c.name, c.groups, c.url, c.enabled, c.online]));
    if (s !== sig) { sig = s; draw(); }
  };
  bus.on('cameras', onCams);
}

export function initCameras() { /* nothing to wire up front: everything opens on demand */ }
