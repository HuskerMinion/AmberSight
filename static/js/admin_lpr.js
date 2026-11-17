// admin_lpr.js - Settings > License plates: turn plate reading on, choose the
// cameras, keep the list of known plates and see the plates read lately.
// Also the "plates in this event" block of the event window (everyone).

import { api, esc, toast, openDlg, state, errText, liveSwitch, swHtml, fld, formBar, confirmDialog, confirmLeave, hasUnsaved } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, how, more, notice } from './sp_kit.js';

const thumb = (id) => '<img class="lp-img" loading="lazy" src="/api/plates/' + id + '/img" alt="">';
const when = (ms) => new Date(ms).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
const sw = swHtml;
const nameOf = (p) => p.name ? '<b class="lp-known">' + esc(p.name) + '</b>' : '<b class="lp-unk">Unknown</b>';

// "Name this plate": text is fixed when it comes from a reading. done() after a change.
export function knownPlateDialog(plate, done) {
  const edit = !!plate.known_id;
  const d = openDlg(edit ? 'Known plate' : 'Add a known plate',
    '<div class="lp-who">' + (plate.id ? thumb(plate.id) : '') + '<div>' +
      '<label class="cs-f"><span>Plate</span><input data-t value="' + esc(plate.text || '') + '" placeholder="ABC123" maxlength="16" autocomplete="off" spellcheck="false"></label>' +
      '<label class="cs-f"><span>Name</span><input data-n value="' + esc(plate.name || '') + '" placeholder="e.g. Mom\'s car" maxlength="60" autocomplete="off"></label>' +
      '<label class="cs-f"><span>Notes (optional)</span><input data-o value="' + esc(plate.notes || '') + '" maxlength="300" autocomplete="off"></label>' +
    '</div></div>' +
    '<div class="hint">Spaces, dashes and capital letters do not matter. A reading with one character wrong, or O/0 and I/1 mixed up, still matches (set under Matching).</div>' +
    '<div class="row"><button class="btn primary" data-ok>Save</button>' + (edit ? '<button class="btn danger" data-del>Remove</button>' : '') + '<button class="btn" data-cancel>Cancel</button></div>', true);
  const q = (s) => d.querySelector(s);
  (plate.text ? q('[data-n]') : q('[data-t]')).focus();
  q('[data-ok]').onclick = async () => {
    const body = { text: q('[data-t]').value, name: q('[data-n]').value, notes: q('[data-o]').value };
    if (!body.name.trim()) { toast('Type a name'); return; }
    try {
      if (edit) await api('/api/plates/known/' + plate.known_id, { method: 'PUT', json: body });
      else await api('/api/plates/known', { json: body });
      toast('Saved');
      d.__close();
      if (done) done();
    } catch (e) { toast(errText(e)); }
  };
  d.querySelectorAll('input').forEach(i => { i.onkeydown = (e) => { if (e.key === 'Enter') q('[data-ok]').click(); }; });
  const del = q('[data-del]');
  if (del) del.onclick = async () => {
    if (!(await confirmDialog({ title: 'Remove ' + plate.text + '?', body: 'It is taken off the known plates. Its readings stay, as unknown.', ok: 'Remove', danger: true }))) return;
    try { await api('/api/plates/known/' + plate.known_id, { method: 'DELETE' }); toast('Removed'); d.__close(); if (done) done(); } catch (e) { toast(errText(e)); }
  };
  q('[data-cancel]').onclick = () => d.__close();
}

// The plates read in one event, for the event window.
export async function eventPlatesHtml(eventId) {
  let rows = [];
  try { rows = (await api('/api/plates?event=' + encodeURIComponent(eventId))).plates; } catch (e) { return ''; }
  if (!rows.length) return '';
  return '<div class="sec">License plates</div><div class="lp-ev">' + rows.map(p =>
    '<button type="button" class="lp-card" data-plate="' + p.id + '"' + (state.isAdmin ? '' : ' disabled') + '>' + thumb(p.id) +
    '<span><b class="lp-text">' + esc(p.text) + '</b>' + nameOf(p) + '<small>' + Math.round(p.confidence * 100) + '% sure</small></span></button>').join('') + '</div>';
}
export function wireEventPlates(root, eventId, refresh) {
  root.querySelectorAll('[data-plate]').forEach(b => { b.onclick = async () => {
    const rows = (await api('/api/plates?event=' + encodeURIComponent(eventId))).plates;
    const p = rows.find(x => String(x.id) === b.dataset.plate);
    if (p) knownPlateDialog(p, refresh);
  }; });
}

function lprPage(el) {
  let st = null, known = [], recent = [], timer = 0, filter = 'all', search = '', busy = false;
  const cams = () => state.cams.filter(c => !c.remote);
  // Save part of the plate settings; throws when the server refuses.
  async function save(patch) {
    const r = await api('/api/settings', { json: { lpr: patch } });
    if (r.error) throw new Error(r.error);
    if (r.warning) toast(r.warning);
  }
  async function loadLists() {
    const qs = '?limit=120' + (filter === 'known' ? '&known=1' : filter === 'unknown' ? '&known=0' : '') + (search ? '&q=' + encodeURIComponent(search) : '');
    [known, recent] = await Promise.all([api('/api/plates/known').then(r => r.known), api('/api/plates/recent' + qs).then(r => r.plates)]);
  }
  async function load() {
    try {
      st = await api('/api/plates/status');
      if (st.models && st.settings.enabled) await loadLists();
    } catch (e) { el.innerHTML = '<p class="empty">' + esc(errText(e)) + '</p>'; return; }
    paint();
    clearTimeout(timer);
    if (st.download.running) timer = setTimeout(() => { if (el.isConnected) load(); }, 1000);
  }
  // Reload after a switch flip without losing what is typed in the bar's fields.
  const EDIT_SEL = ['[data-conf]', '[data-px]', '[data-diff]', '[data-keep]'];
  function editFields() {
    const m = new Map();
    EDIT_SEL.forEach(x => { const e = el.querySelector(x); if (e) m.set(x, e); });
    el.querySelectorAll('[data-cam]').forEach(r => ['[data-cc]', '[data-cp]'].forEach(x => { const e = r.querySelector(x); if (e) m.set(r.dataset.cam + '|' + x, e); }));
    return m;
  }
  async function reload() {
    const snap = hasUnsaved() ? new Map([...editFields()].map(([k, e]) => [k, e.value])) : null;
    await load();
    if (!snap) return;
    for (const [k, e] of editFields()) {
      if (snap.has(k) && e.value !== snap.get(k)) { e.value = snap.get(k); e.dispatchEvent(new Event('input', { bubbles: true })); }
    }
  }
  function paint() {
    const s = st.settings, d = st.download;
    const ready = st.models;
    let h = '<div class="ac">' + lead('Optional. Reads the license plates of cars at your cameras, so alerts can say which car it was.', 'Use it on a driveway or gate camera that sees plates close and straight on.');
    if (st.open) h += notice('warn', 'This server has no sign-in accounts: anyone on your network can see these plates.', '<a class="btn" href="#settings/users/users">Add an account</a>');
    h += '<div class="ac-card"><div class="ac-card-t">Set up</div>';
    if (!st.ai) h += '<p class="hint">Plates are read on vehicles AI has found: turn AI on in Settings &rsaquo; Detection &amp; alerts &rsaquo; Objects (AI) first (or choose "whole picture" for a camera below).</p>';
    h += sw('data-en', s.enabled, 'Read license plates', 'Downloads a small free reader (about 5 MB) when you turn this on.');
    h += how('When AI sees a vehicle, AmberSight finds the plate in the full-quality recording, reads it, and labels the event (&quot;plate ABC123 (Mom\'s car)&quot; or &quot;unknown plate ABC123&quot;), so alerts and the event list say which car it was. Reading runs on this server\'s processor; plates and their small pictures are kept in its database. A plate under about 50 pixels wide is hard to read.');
    if (d.running) h += '<div class="hint">Downloading the plate reader... ' + Math.round(100 * d.done / Math.max(1, d.total)) + '%</div>';
    else if (d.error) h += notice('warn', esc(d.error), '<button class="btn primary" data-dl>Try again</button>');
    else if (s.enabled && !ready) h += '<button class="btn primary" data-dl>Download the plate reader</button>';
    if (s.enabled && ready) {
      h += more('Matching', '<div class="ac-grid3">' +
        fld('Read at least this sure', 'data-conf min="0.3" max="0.99" step="0.05" value="' + (+s.min_confidence).toFixed(2) + '"', '0.30 to 0.99') +
        fld('Smallest plate', 'data-px min="20" max="1000" value="' + s.min_plate_px + '"', 'pixels wide') +
        '<label class="fld"><span class="fld-l">A known plate may differ by</span><select data-diff>' + [[0, 'nothing'], [1, '1 character'], [2, '2 characters']].map(o => '<option value="' + o[0] + '"' + (s.max_diff === o[0] ? ' selected' : '') + '>' + o[1] + '</option>').join('') + '</select></label>' +
        '<label class="fld"><span class="fld-l">Keep plate readings for</span><select data-keep>' + [[30, '30 days'], [90, '90 days'], [180, '180 days'], [365, '1 year']].concat([30, 90, 180, 365].includes(s.keep_days) ? [] : [[s.keep_days, s.keep_days + ' days']]).map(o => '<option value="' + o[0] + '"' + (s.keep_days === o[0] ? ' selected' : '') + '>' + o[1] + '</option>').join('') + '</select></label></div>' +
        sw('data-conf-like data-live', s.confusions, 'Treat look-alikes as the same', 'O and 0, I and 1, B and 8, S and 5, Z and 2, G and 6, D and Q and 0.') +
        '<div class="hint">Plates under 5 characters must match exactly. Fewer wrong readings with a higher "sure"; more plates read with a lower one. The smallest-plate size can be set per camera too.</div>');
    }
    h += '</div>';
    if (s.enabled && ready) {
      // Cameras
      h += '<div class="ac-card"><div class="ac-card-t">Cameras</div><div class="hint">Plates are read only on the cameras switched on here. Leave the sure and size boxes empty to use the settings above.</div>';
      h += cams().map(c => {
        const cc = s.cameras.find(x => x.name === c.name);
        return '<div class="lp-cam" data-cam="' + esc(c.name) + '">' + sw('data-on data-live', !!cc, esc(c.name)) +
          (cc ? '<div class="lp-cam-o">' + fld('Sure', 'data-cc min="0.3" max="0.99" step="0.05" placeholder="' + (+s.min_confidence).toFixed(2) + '" value="' + (cc.min_confidence == null ? '' : cc.min_confidence) + '"', '0.30 to 0.99') +
            fld('Plate size', 'data-cp min="20" max="1000" placeholder="' + s.min_plate_px + '" value="' + (cc.min_plate_px == null ? '' : cc.min_plate_px) + '"', 'pixels wide') +
            '<span title="Also look for plates in the whole picture when AI saw no vehicle (a camera aimed at a gate)">' + sw('data-ff data-live', cc.full_frame, 'Whole picture') + '</span></div>' : '') + '</div>';
      }).join('') || '<p class="hint">No cameras yet.</p>';
      h += '</div>';
      // Known plates
      h += '<div class="ac-card"><div class="ac-card-t">Known plates</div>' +
        (known.length ? '<div class="lp-list">' + known.map(k => '<button type="button" class="lp-row" data-known="' + k.id + '"><b class="lp-text">' + esc(k.text) + '</b><span><b>' + esc(k.name) + '</b>' + (k.notes ? '<small>' + esc(k.notes) + '</small>' : '') + '</span><small>' + (k.seen ? 'seen ' + k.seen + (k.last_ms ? ' · ' + esc(when(k.last_ms)) : '') : 'not seen yet') + '</small></button>').join('') + '</div>'
          : '<p class="hint">None yet. Add one here, or name a plate from the list below.</p>') +
        '<div class="row"><button class="btn" data-add>Add a plate</button></div></div>';
      // Recent
      h += '<div class="ac-card"><div class="ac-card-t">Seen lately</div><div class="lp-bar">' +
        '<input type="search" data-nt data-q placeholder="Search a plate or name" value="' + esc(search) + '" spellcheck="false" autocomplete="off">' +
        '<div class="chips">' + [['all', 'All'], ['unknown', 'Unknown'], ['known', 'Known']].map(f => '<button type="button" class="chip' + (filter === f[0] ? ' on' : '') + '" data-f="' + f[0] + '">' + f[1] + '</button>').join('') + '</div></div>' +
        (recent.length ? '<div class="lp-grid">' + recent.map(p => '<div class="lp-card"><div>' + thumb(p.id) + '</div><span><b class="lp-text">' + esc(p.text) + '</b>' + nameOf(p) +
          '<small>seen ' + p.count + (p.count === 1 ? ' time' : ' times') + '<br>first ' + esc(when(p.first_ms)) + '<br>last ' + esc(when(p.last_ms)) + (p.camera_names && p.camera_names.length ? '<br>' + esc(p.camera_names.join(', ')) : '') + '</small>' +
          '<button class="btn" data-name="' + esc(p.text) + '">' + (p.known ? 'Edit' : 'Add to known') + '</button></span></div>').join('') + '</div>'
          : '<p class="hint">' + (search || filter !== 'all' ? 'No plate matches.' : 'None yet. Plates show up here after a vehicle with a readable plate passes a camera.') + '</p>') +
        '<div class="hint">Unknown plates are forgotten after 6 months; known ones stay.</div></div>';
    }
    h += '</div>';
    el.innerHTML = h;
    wire();
  }
  function wire() {
    const q = (x) => el.querySelector(x);
    const s = st.settings;
    // On/off settings save when flipped. Numbers and choices go through the bar below.
    // A flip that changes what the page shows reloads it, carrying the unsaved edits across;
    // `local` (when given) updates the page's own copy instead, so nothing is redrawn.
    const flip = (sel, make, local) => { const i = q(sel); if (i) liveSwitch(i, async (on) => { await save(make(on)); if (local) local(on); else await reload(); }); };
    flip('[data-en]', (on) => ({ enabled: on }));
    const dl = q('[data-dl]'); if (dl) dl.onclick = async () => { await api('/api/plates/models', { json: {} }); load(); };
    if (!(s.enabled && st.models)) return;
    flip('[data-conf-like]', (on) => ({ confusions: on }), (on) => { s.confusions = on; });
    const camsNow = () => s.cameras.map(c => Object.assign({}, c));
    el.querySelectorAll('[data-cam]').forEach(row => {
      const name = row.dataset.cam;
      flip2(row.querySelector('[data-on]'), (on) => {
        const list = camsNow().filter(c => c.name !== name);
        if (on) list.push({ name, min_confidence: null, min_plate_px: null, full_frame: false });
        return { cameras: list };
      });
      flip2(row.querySelector('[data-ff]'), (on) => ({ cameras: camsNow().map(c => (c.name === name ? Object.assign(c, { full_frame: on }) : c)) }),
        (on) => { const cc = s.cameras.find(x => x.name === name); if (cc) cc.full_frame = on; });
    });
    function flip2(i, make, local) { if (i) liveSwitch(i, async (on) => { await save(make(on)); if (local) local(on); else await reload(); }); }
    const confirmed = (x) => { const v = x.value.trim(); return v === '' ? null : +v; };
    formBar(q('.ac'), { save: async () => {
      const patch = {
        min_confidence: Math.max(0.3, Math.min(0.99, +q('[data-conf]').value || 0.7)),
        min_plate_px: Math.max(20, Math.min(1000, Math.round(+q('[data-px]').value) || 50)),
        max_diff: +q('[data-diff]').value, keep_days: +q('[data-keep]').value,
        cameras: camsNow().map(c => {
          const row = [...el.querySelectorAll('[data-cam]')].find(r => r.dataset.cam === c.name);
          const cc = row && row.querySelector('[data-cc]'), cp = row && row.querySelector('[data-cp]');
          if (cc) { const v = confirmed(cc); c.min_confidence = v == null ? null : Math.max(0.3, Math.min(0.99, v)); }
          if (cp) { const v = confirmed(cp); c.min_plate_px = v == null ? null : Math.max(20, Math.min(1000, Math.round(v))); }
          return c;
        }),
      };
      await save(patch);
      load();
    } });
    el.querySelectorAll('[data-known]').forEach(b => { b.onclick = () => { const k = known.find(x => String(x.id) === b.dataset.known); if (k) knownPlateDialog({ text: k.text, name: k.name, notes: k.notes, known_id: k.id }, load); }; });
    q('[data-add]').onclick = () => knownPlateDialog({}, load);
    el.querySelectorAll('[data-name]').forEach(b => { b.onclick = () => {
      const p = recent.find(x => x.text === b.dataset.name);
      if (p) knownPlateDialog(p, load);
    }; });
    const search_ = q('[data-q]');
    search_.onkeydown = async (e) => {
      if (e.key !== 'Enter' || busy) return;
      if (!(await confirmLeave())) return;
      busy = true; search = search_.value.trim();
      try { await loadLists(); paint(); const n = q('[data-q]'); if (n) { n.focus(); n.setSelectionRange(n.value.length, n.value.length); } } catch (x) { toast(errText(x)); }
      busy = false;
    };
    el.querySelectorAll('[data-f]').forEach(b => { b.onclick = async () => { if (!(await confirmLeave())) return; filter = b.dataset.f; try { await loadLists(); paint(); } catch (x) { toast(errText(x)); } }; });
  }
  el.innerHTML = '<div class="ac"><p class="empty">Loading...</p></div>';
  load();
  return () => clearTimeout(timer);
}

let stopLpr = null;
registerAdminPage({ id: 'lpr', title: 'License plates', order: 66, render(el) { stopLpr = lprPage(el); }, leave() { if (stopLpr) stopLpr(); } });
