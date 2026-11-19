// admin_lpr.js - Settings > License plates: turn plate reading on, choose the
// cameras, keep the list of known plates and see the plates read lately.
// Also the "plates in this event" block of the event window (everyone).

import { t } from './i18n.js';
import { api, esc, toast, openDlg, state, errText, liveSwitch, swHtml, fld, formBar, confirmDialog, confirmLeave, hasUnsaved } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, how, more, notice } from './sp_kit.js';

const thumb = (id) => '<img class="lp-img" loading="lazy" src="/api/plates/' + id + '/img" alt="">';
const when = (ms) => new Date(ms).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
const sw = swHtml;
const nameOf = (p) => p.name ? '<b class="lp-known">' + esc(p.name) + '</b>' : '<b class="lp-unk">' + t('admin_lpr.unknown', 'Unknown') + '</b>';

// "Name this plate": text is fixed when it comes from a reading. done() after a change.
export function knownPlateDialog(plate, done) {
  const edit = !!plate.known_id;
  const d = openDlg(edit ? t('admin_lpr.known_plate','Known plate') : t('admin_lpr.add_known_plate','Add a known plate'),
    '<div class="lp-who">' + (plate.id ? thumb(plate.id) : '') + '<div>' +
      '<label class="cs-f"><span>' + t('admin_lpr.plate', 'Plate') + '</span><input data-t value="' + esc(plate.text || '') + '" placeholder="ABC123" maxlength="16" autocomplete="off" spellcheck="false"></label>' +
      '<label class="cs-f"><span>' + t('admin_lpr.name', 'Name') + '</span><input data-n value="' + esc(plate.name || '') + '" placeholder="' + t('admin_lpr.name_ph', 'e.g. Mom\'s car') + '" maxlength="60" autocomplete="off"></label>' +
      '<label class="cs-f"><span>' + t('admin_lpr.notes', 'Notes (optional)') + '</span><input data-o value="' + esc(plate.notes || '') + '" maxlength="300" autocomplete="off"></label>' +
    '</div></div>' +
    '<div class="hint">' + t('admin_lpr.match_hint', 'Spaces, dashes and capital letters do not matter. A reading with one character wrong, or O/0 and I/1 mixed up, still matches (set under Matching).') + '</div>' +
    '<div class="row"><button class="btn primary" data-ok>' + t('admin_lpr.save', 'Save') + '</button>' + (edit ? '<button class="btn danger" data-del>' + t('admin_lpr.remove', 'Remove') + '</button>' : '') + '<button class="btn" data-cancel>' + t('admin_lpr.cancel', 'Cancel') + '</button></div>', true);
  const q = (s) => d.querySelector(s);
  (plate.text ? q('[data-n]') : q('[data-t]')).focus();
  q('[data-ok]').onclick = async () => {
    const body = { text: q('[data-t]').value, name: q('[data-n]').value, notes: q('[data-o]').value };
    if (!body.name.trim()) { toast(t('admin_lpr.type_name', 'Type a name')); return; }
    try {
      if (edit) await api('/api/plates/known/' + plate.known_id, { method: 'PUT', json: body });
      else await api('/api/plates/known', { json: body });
      toast(t('admin_lpr.saved', 'Saved'));
      d.__close();
      if (done) done();
    } catch (e) { toast(errText(e)); }
  };
  d.querySelectorAll('input').forEach(i => { i.onkeydown = (e) => { if (e.key === 'Enter') q('[data-ok]').click(); }; });
  const del = q('[data-del]');
  if (del) del.onclick = async () => {
    if (!(await confirmDialog({ title: t('admin_lpr.remove_title', 'Remove {plate}?', { plate: plate.text }), body: t('admin_lpr.remove_body', 'It is taken off the known plates. Its readings stay, as unknown.'), ok: t('admin_lpr.remove', 'Remove'), danger: true }))) return;
    try { await api('/api/plates/known/' + plate.known_id, { method: 'DELETE' }); toast(t('admin_lpr.removed', 'Removed')); d.__close(); if (done) done(); } catch (e) { toast(errText(e)); }
  };
  q('[data-cancel]').onclick = () => d.__close();
}

// The plates read in one event, for the event window.
export async function eventPlatesHtml(eventId) {
  let rows = [];
  try { rows = (await api('/api/plates?event=' + encodeURIComponent(eventId))).plates; } catch (e) { return ''; }
  if (!rows.length) return '';
  return '<div class="sec">' + t('admin_lpr.license_plates', 'License plates') + '</div><div class="lp-ev">' + rows.map(p =>
    '<button type="button" class="lp-card" data-plate="' + p.id + '"' + (state.isAdmin ? '' : ' disabled') + '>' + thumb(p.id) +
    '<span><b class="lp-text">' + esc(p.text) + '</b>' + nameOf(p) + '<small>' + t('admin_lpr.pct_sure', '{pct}% sure', { pct: Math.round(p.confidence * 100) }) + '</small></span></button>').join('') + '</div>';
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
    let h = '<div class="ac">' + lead(t('admin_lpr.lead1', 'Optional. Reads the license plates of cars at your cameras, so alerts can say which car it was.'), t('admin_lpr.lead2', 'Use it on a driveway or gate camera that sees plates close and straight on.'));
    if (st.open) h += notice('warn', t('admin_lpr.no_accounts', 'This server has no sign-in accounts: anyone on your network can see these plates.'), '<a class="btn" href="#settings/users/users">' + t('admin_lpr.add_account', 'Add an account') + '</a>');
    h += '<div class="ac-card"><div class="ac-card-t">' + t('admin_lpr.set_up', 'Set up') + '</div>';
    if (!st.ai) h += '<p class="hint">' + t('admin_lpr.needs_ai', 'Plates are read on vehicles AI has found: turn AI on in Settings &rsaquo; Detection &amp; alerts &rsaquo; Objects (AI) first (or choose "whole picture" for a camera below).') + '</p>';
    h += sw('data-en', s.enabled, t('admin_lpr.read_plates', 'Read license plates'), t('admin_lpr.read_plates_hint', 'Downloads a small free reader (about 5 MB) when you turn this on.'));
    h += how(t('admin_lpr.how', 'When AI sees a vehicle, AmberSight finds the plate in the full-quality recording, reads it, and labels the event (&quot;plate ABC123 (Mom\'s car)&quot; or &quot;unknown plate ABC123&quot;), so alerts and the event list say which car it was. Reading runs on this server\'s processor; plates and their small pictures are kept in its database. A plate under about 50 pixels wide is hard to read.'));
    if (d.running) h += '<div class="hint">' + t('admin_lpr.downloading', 'Downloading the plate reader... {pct}%', { pct: Math.round(100 * d.done / Math.max(1, d.total)) }) + '</div>';
    else if (d.error) h += notice('warn', esc(d.error), '<button class="btn primary" data-dl>' + t('admin_lpr.try_again', 'Try again') + '</button>');
    else if (s.enabled && !ready) h += '<button class="btn primary" data-dl>' + t('admin_lpr.download_reader', 'Download the plate reader') + '</button>';
    if (s.enabled && ready) {
      h += more(t('admin_lpr.matching', 'Matching'), '<div class="ac-grid3">' +
        fld(t('admin_lpr.min_sure', 'Read at least this sure'), 'data-conf min="0.3" max="0.99" step="0.05" value="' + (+s.min_confidence).toFixed(2) + '"', '0.30 to 0.99') +
        fld(t('admin_lpr.smallest_plate', 'Smallest plate'), 'data-px min="20" max="1000" value="' + s.min_plate_px + '"', t('admin_lpr.pixels_wide', 'pixels wide')) +
        '<label class="fld"><span class="fld-l">' + t('admin_lpr.differ_by', 'A known plate may differ by') + '</span><select data-diff>' + [[0, t('admin_lpr.diff_0', 'nothing')], [1, t('admin_lpr.diff_1', '1 character')], [2, t('admin_lpr.diff_2', '2 characters')]].map(o => '<option value="' + o[0] + '"' + (s.max_diff === o[0] ? ' selected' : '') + '>' + o[1] + '</option>').join('') + '</select></label>' +
        '<label class="fld"><span class="fld-l">' + t('admin_lpr.keep_for', 'Keep plate readings for') + '</span><select data-keep>' + [[30, t('admin_lpr.days_30', '30 days')], [90, t('admin_lpr.days_90', '90 days')], [180, t('admin_lpr.days_180', '180 days')], [365, t('admin_lpr.year_1', '1 year')]].concat([30, 90, 180, 365].includes(s.keep_days) ? [] : [[s.keep_days, t('admin_lpr.n_days', '{n} days', { n: s.keep_days })]]).map(o => '<option value="' + o[0] + '"' + (s.keep_days === o[0] ? ' selected' : '') + '>' + o[1] + '</option>').join('') + '</select></label></div>' +
        sw('data-conf-like data-live', s.confusions, t('admin_lpr.lookalikes', 'Treat look-alikes as the same'), t('admin_lpr.lookalikes_hint', 'O and 0, I and 1, B and 8, S and 5, Z and 2, G and 6, D and Q and 0.')) +
        '<div class="hint">' + t('admin_lpr.exact_hint', 'Plates under 5 characters must match exactly. Fewer wrong readings with a higher "sure"; more plates read with a lower one. The smallest-plate size can be set per camera too.') + '</div>');
    }
    h += '</div>';
    if (s.enabled && ready) {
      // Cameras
      h += '<div class="ac-card"><div class="ac-card-t">' + t('admin_lpr.cameras', 'Cameras') + '</div><div class="hint">' + t('admin_lpr.cameras_hint', 'Plates are read only on the cameras switched on here. Leave the sure and size boxes empty to use the settings above.') + '</div>';
      h += cams().map(c => {
        const cc = s.cameras.find(x => x.name === c.name);
        return '<div class="lp-cam" data-cam="' + esc(c.name) + '">' + sw('data-on data-live', !!cc, esc(c.name)) +
          (cc ? '<div class="lp-cam-o">' + fld(t('admin_lpr.sure', 'Sure'), 'data-cc min="0.3" max="0.99" step="0.05" placeholder="' + (+s.min_confidence).toFixed(2) + '" value="' + (cc.min_confidence == null ? '' : cc.min_confidence) + '"', '0.30 to 0.99') +
            fld(t('admin_lpr.plate_size', 'Plate size'), 'data-cp min="20" max="1000" placeholder="' + s.min_plate_px + '" value="' + (cc.min_plate_px == null ? '' : cc.min_plate_px) + '"', t('admin_lpr.pixels_wide', 'pixels wide')) +
            '<span title="' + t('admin_lpr.whole_tip', 'Also look for plates in the whole picture when AI saw no vehicle (a camera aimed at a gate)') + '">' + sw('data-ff data-live', cc.full_frame, t('admin_lpr.whole_picture', 'Whole picture')) + '</span></div>' : '') + '</div>';
      }).join('') || '<p class="hint">' + t('admin_lpr.no_cameras', 'No cameras yet.') + '</p>';
      h += '</div>';
      // Known plates
      h += '<div class="ac-card"><div class="ac-card-t">' + t('admin_lpr.known_plates', 'Known plates') + '</div>' +
        (known.length ? '<div class="lp-list">' + known.map(k => '<button type="button" class="lp-row" data-known="' + k.id + '"><b class="lp-text">' + esc(k.text) + '</b><span><b>' + esc(k.name) + '</b>' + (k.notes ? '<small>' + esc(k.notes) + '</small>' : '') + '</span><small>' + (k.seen ? t('admin_lpr.seen_n', 'seen {n}', { n: k.seen }) + (k.last_ms ? ' · ' + esc(when(k.last_ms)) : '') : t('admin_lpr.not_seen', 'not seen yet')) + '</small></button>').join('') + '</div>'
          : '<p class="hint">' + t('admin_lpr.none_known', 'None yet. Add one here, or name a plate from the list below.') + '</p>') +
        '<div class="row"><button class="btn" data-add>' + t('admin_lpr.add_plate', 'Add a plate') + '</button></div></div>';
      // Recent
      h += '<div class="ac-card"><div class="ac-card-t">' + t('admin_lpr.seen_lately', 'Seen lately') + '</div><div class="lp-bar">' +
        '<input type="search" data-nt data-q placeholder="' + t('admin_lpr.search_placeholder', 'Search a plate or name') + '" value="' + esc(search) + '" spellcheck="false" autocomplete="off">' +
        '<div class="chips">' + [['all', t('admin_lpr.all', 'All')], ['unknown', t('admin_lpr.unknown', 'Unknown')], ['known', t('admin_lpr.known', 'Known')]].map(f => '<button type="button" class="chip' + (filter === f[0] ? ' on' : '') + '" data-f="' + f[0] + '">' + f[1] + '</button>').join('') + '</div></div>' +
        (recent.length ? '<div class="lp-grid">' + recent.map(p => '<div class="lp-card"><div>' + thumb(p.id) + '</div><span><b class="lp-text">' + esc(p.text) + '</b>' + nameOf(p) +
          '<small>' + t('admin_lpr.seen_times', 'seen {n} {n, plural, one {time} other {times}}<br>first {first}<br>last {last}', { n: p.count, first: esc(when(p.first_ms)), last: esc(when(p.last_ms)) }) + (p.camera_names && p.camera_names.length ? '<br>' + esc(p.camera_names.join(', ')) : '') + '</small>' +
          '<button class="btn" data-name="' + esc(p.text) + '">' + (p.known ? t('admin_lpr.edit', 'Edit') : t('admin_lpr.add_to_known', 'Add to known')) + '</button></span></div>').join('') + '</div>'
          : '<p class="hint">' + (search || filter !== 'all' ? t('admin_lpr.no_plate_matches', 'No plate matches.') : t('admin_lpr.none_yet', 'None yet. Plates show up here after a vehicle with a readable plate passes a camera.')) + '</p>') +
        '<div class="hint">' + t('admin_lpr.unknown_forget', 'Unknown plates are forgotten after 6 months; known ones stay.') + '</div></div>';
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
  el.innerHTML = '<div class="ac"><p class="empty">' + t('admin_lpr.loading', 'Loading...') + '</p></div>';
  load();
  return () => clearTimeout(timer);
}

let stopLpr = null;
registerAdminPage({ id: 'lpr', title: t('admin_lpr.title', 'License plates'), order: 66, render(el) { stopLpr = lprPage(el); }, leave() { if (stopLpr) stopLpr(); } });
