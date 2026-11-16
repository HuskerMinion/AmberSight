// bookmarks.js - timeline bookmarks: a moment marked with a note, on one camera or all of them.
//
// The timeline (livetl.js) shows them as flags on its strip, adds them at the playhead (B) and
// opens the list; the list also opens from the command palette. Notes are plain text: always
// escaped where they are shown. "Keep" holds the recordings 2 minutes either side of the moment
// back from automatic cleanup (the server's KEEP_WINDOW_MS).

import { $, state, api, esc, enc, ic, can, toast, openDlg, confirmDialog, errText, fmtWall } from './core.js';
import { exportDialog } from './export.js';

const NOTE_MAX = 500;
const KEEP_MS = 120000;
const BM_SVG = '<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 3h12v18l-6-4-6 4z"/></svg>';
export const bookmarkIcon = () => BM_SVG;
// The timeline's two buttons look different: add (a bookmark with a plus) and the list (lines
// with a bookmark), so they are not mixed up on a phone, where the list loses its label.
export const bookmarkAddIcon = () => '<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 3h12v18l-6-4-6 4z"/><path d="M12 7v6M9 10h6"/></svg>';
export const bookmarkListIcon = () => '<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h9M3 12h9M3 18h9"/><path d="M15 4h6v15l-3-2-3 2z"/></svg>';

// What the timeline loaded: bookmarks in [a, b].
const D = { list: [], range: null };
// The timeline's side of things (set by attach): playhead(), shown() names, solo() name|null,
// seek(ms), note(text), redraw(), pause().
let T = null;
export function attach(t) { T = t; }

const localCams = () => (state.cams || []).filter(c => !c.remote).map(c => c.name);
const camId = (name) => { const c = (state.cams || []).find(x => x.name === name && !x.remote); return c ? c.id : null; };
const when = (ms) => fmtWall(ms);
const where = (b) => (b.camera ? b.camera : 'All cameras');
// ("All cameras" are this server's: never drawn over a linked server's camera.)
const isLocal = (name) => localCams().includes(name);

// ---------- data for the strip ----------
export async function load(a, b) {
  try {
    const r = await api('/api/bookmarks?from=' + Math.floor(a) + '&to=' + Math.ceil(b) + '&limit=1000');
    D.list = r.bookmarks || [];
    D.range = [a, b];
  } catch (e) { /* the strip shows none */ }
}
function changed() { if (T) T.redraw(); bmListRefresh(); }
function upsert(b) {
  const i = D.list.findIndex(x => x.id === b.id);
  if (i >= 0) D.list[i] = b; else D.list.push(b);
  changed();
}
function drop(id) { D.list = D.list.filter(x => x.id !== id); changed(); }

// Flags on the strip: the shown cameras' bookmarks and the all-cameras ones.
export function marksHtml(from, to, W, shown) {
  const ppm = W / (to - from);
  const anyLocal = [...shown].some(isLocal);
  return D.list.filter(b => b.at >= from && b.at <= to && (b.camera ? shown.has(b.camera) : anyLocal)).sort((x, y) => x.at - y.at).map(b => {
    const x = (b.at - from) * ppm;
    const label = 'Bookmark at ' + when(b.at) + ', ' + where(b) + ': ' + b.note;
    return '<button type="button" class="tlbm' + (b.keep ? ' kept' : '') + (b.camera ? '' : ' all') + '" data-id="' + b.id + '" style="left:' + x.toFixed(1) + 'px" aria-label="' + esc(label) + '">' + BM_SVG + '</button>';
  }).join('');
}

// ---------- hover note ----------
let tipEl = null;
function hideTip() { if (tipEl) tipEl.hidden = true; }
function showTip(btn) {
  const b = D.list.find(x => x.id === +btn.dataset.id);
  // (Not over the bookmark's own popover.)
  if (!b || (pop && !pop.hidden)) return;
  if (!tipEl) { tipEl = document.createElement('div'); tipEl.className = 'tltip bmtip'; tipEl.setAttribute('role', 'tooltip'); document.body.appendChild(tipEl); }
  tipEl.innerHTML = '<div class="tt1">' + BM_SVG + '<b>' + esc(when(b.at)) + '</b></div><div class="bmnote">' + esc(b.note) + '</div><div class="tm">' + esc(where(b)) + ' · ' + esc(b.user) + (b.keep ? ' · kept' : '') + '</div>';
  tipEl.hidden = false;
  const r = btn.getBoundingClientRect(), tw = tipEl.offsetWidth;
  tipEl.style.left = Math.max(6, Math.min(window.innerWidth - tw - 6, r.left + r.width / 2 - tw / 2)) + 'px';
  tipEl.style.top = 'auto';
  tipEl.style.bottom = (window.innerHeight - r.top + 8) + 'px';
}

// ---------- popover (one bookmark, or the add / edit form) ----------
let pop = null, popOff = null;
export function closePop() {
  if (!pop || pop.hidden) return false;
  pop.hidden = true;
  if (popOff) { document.removeEventListener('pointerdown', popOff, true); popOff = null; }
  return true;
}
function openPopAt(anchor, html) {
  hideTip();
  if (!pop) { pop = document.createElement('div'); pop.className = 'tlpop bmpop'; pop.setAttribute('role', 'dialog'); document.body.appendChild(pop); }
  pop.innerHTML = html;
  pop.hidden = false;
  const r = anchor.getBoundingClientRect(), pw = pop.offsetWidth;
  const area = $('tlArea');
  const top = Math.min(r.top, area && !area.closest('[hidden]') ? area.getBoundingClientRect().top : r.top);
  pop.style.left = Math.max(6, Math.min(window.innerWidth - pw - 6, r.left + r.width / 2 - pw / 2)) + 'px';
  pop.style.bottom = (window.innerHeight - top + 8) + 'px';
  pop.style.maxHeight = Math.max(160, top - 16) + 'px';
  if (popOff) document.removeEventListener('pointerdown', popOff, true);
  popOff = (ev) => { if (!pop.contains(ev.target) && !ev.target.closest('.tlbm, #tlBm')) closePop(); };
  setTimeout(() => { if (popOff) document.addEventListener('pointerdown', popOff, true); }, 0);
  // Esc closes the popover only (not the timeline behind it).
  pop.onkeydown = (ev) => { if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); closePop(); } };
  return pop;
}

// The add / edit form. `b` = an existing bookmark (edit), else a new one at `at`.
function formHtml(b, at, cams, pick) {
  const limited = !!state.limited;
  const opts = (limited ? '' : '<option value=""' + (pick === '' ? ' selected' : '') + '>All cameras</option>') +
    cams.map(n => '<option' + (pick === n ? ' selected' : '') + '>' + esc(n) + '</option>').join('');
  const keep = b ? b.keep : false;
  return '<form class="bmform" novalidate>' +
    '<div class="bmh">' + BM_SVG + '<b>' + (b ? 'Edit bookmark' : 'Bookmark') + '</b><span class="tm">' + esc(when(b ? b.at : at)) + '</span></div>' +
    '<textarea name="note" rows="3" maxlength="' + NOTE_MAX + '" placeholder="What happened here?" aria-label="Note" required>' + esc(b ? b.note : '') + '</textarea>' +
    '<div class="bmrow"><select name="camera" aria-label="Camera">' + opts + '</select>' +
    '<label class="bmkeep" title="Hold the recordings from 2 minutes before to 2 minutes after this moment back from automatic cleanup"><input type="checkbox" name="keep"' + (keep ? ' checked' : '') + '> Keep video</label></div>' +
    '<div class="bmerr" role="alert" hidden></div>' +
    '<div class="bmbtns"><span class="tm bmcount"></span><button type="button" class="btn" data-x>Cancel</button><button type="submit" class="primary">Save</button></div>' +
    '</form>';
}
function wireForm(p, b, at, onSaved) {
  const f = p.querySelector('form');
  const ta = f.elements.note, cnt = f.querySelector('.bmcount'), er = f.querySelector('.bmerr');
  const count = () => { const n = [...ta.value].length; cnt.textContent = n > NOTE_MAX - 60 ? n + ' / ' + NOTE_MAX : ''; };
  ta.oninput = count; count();
  // Keeping the video of all cameras is for admins (a kept one holds every camera's recordings).
  const camSel = f.elements.camera, keepEl = f.elements.keep, keepLabel = keepEl.closest('label');
  const keepTitle = keepLabel.title;
  const syncKeep = () => {
    const lock = !state.isAdmin && !camSel.value && !(b && b.keep && !b.camera);
    keepEl.disabled = lock;
    if (lock) keepEl.checked = false;
    keepLabel.title = lock ? 'Only an admin can keep video for all cameras. Pick one camera.' : keepTitle;
  };
  camSel.onchange = syncKeep; syncKeep();
  // Enter saves, Shift+Enter is a new line; the keys stay here (not the timeline's shortcuts).
  ta.onkeydown = (ev) => {
    ev.stopPropagation();
    if (ev.key === 'Enter' && !ev.shiftKey && !ev.isComposing) { ev.preventDefault(); f.requestSubmit(); }
    if (ev.key === 'Escape') { ev.preventDefault(); closePop(); }
  };
  f.querySelector('[data-x]').onclick = () => closePop();
  f.onsubmit = async (ev) => {
    ev.preventDefault();
    const note = ta.value.trim();
    if (!note) { er.textContent = 'Write a note'; er.hidden = false; ta.focus(); return; }
    const body = bodyOf(f, b, note);
    if (!b) body.at = Math.round(at);
    const btn = f.querySelector('[type=submit]');
    btn.disabled = true;
    try {
      const r = b ? await api('/api/bookmarks/' + b.id, { method: 'PUT', json: body }) : await api('/api/bookmarks', { json: body });
      closePop();
      onSaved(r);
      toast(b ? 'Bookmark saved' : 'Bookmarked ' + fmtWall(r.at) + (r.keep ? ' (video kept)' : ''), { kind: 'ok', ms: 2200 });
    } catch (e) {
      er.textContent = errText(e); er.hidden = false; btn.disabled = false;
    }
  };
  setTimeout(() => { try { ta.focus(); ta.setSelectionRange(ta.value.length, ta.value.length); } catch (e) { /* gone */ } }, 0);
}

// What the form sends: the camera only for a new bookmark or when it changed.
function bodyOf(f, b, note) {
  const body = { note, keep: f.elements.keep.checked };
  const cam = f.elements.camera.value || null;
  if (!b || cam !== (b.camera || null)) body.camera = cam;
  return body;
}

// B / the Bookmark button: a new bookmark at the playhead.
export function addAtPlayhead(anchor) {
  if (!T) return;
  if (!can('events')) { T.note('This account cannot add bookmarks'); return; }
  const at = Math.min(Date.now(), Math.round(T.playhead()));
  T.pause();
  const shown = T.shown().filter(n => localCams().includes(n));
  const solo = T.solo();
  const cams = solo ? [solo] : shown;
  if (!cams.length) { T.note('No cameras with recordings here'); return; }
  // One camera in view: that one; several: all of them (an account limited to some cameras: the first).
  const pick = cams.length === 1 || state.limited ? cams[0] : '';
  const p = openPopAt(anchor || $('tlHead'), formHtml(null, at, cams, pick));
  wireForm(p, null, at, upsert);
}

function openEdit(b, anchor) {
  const cams = localCams();
  const p = openPopAt(anchor, formHtml(b, b.at, b.camera && !cams.includes(b.camera) ? cams.concat([b.camera]) : cams, b.camera || ''));
  wireForm(p, b, b.at, (r) => { upsert(r); });
}

async function remove(b) {
  const ok = await confirmDialog({ title: 'Delete bookmark?', body: '"' + b.note.slice(0, 80) + (b.note.length > 80 ? '...' : '') + '"' + (b.keep ? ' Its video is no longer kept.' : ''), ok: 'Delete', danger: true });
  if (!ok) return false;
  try { await api('/api/bookmarks/' + b.id, { method: 'DELETE' }); drop(b.id); toast('Bookmark deleted', { kind: 'ok', ms: 2000 }); return true; } catch (e) { toast(errText(e), { kind: 'err' }); return false; }
}

// Save the 4 minutes around a bookmark (the existing export dialog).
export function exportAround(b) {
  const cams = b.camera ? [b.camera] : localCams();
  if (T) T.pause();
  exportDialog({ cams, camera: b.camera || undefined, from: b.at - KEEP_MS, to: b.at + KEEP_MS });
}

// A flag clicked: play from just before it and show it.
function viewPop(b, anchor) {
  const p = openPopAt(anchor, '<div class="bmview">' +
    '<div class="bmh">' + BM_SVG + '<b>' + esc(when(b.at)) + '</b><span class="tm">' + esc(where(b)) + '</span></div>' +
    '<div class="bmnote">' + esc(b.note) + '</div>' +
    '<div class="tm">By ' + esc(b.user) + (b.keep ? ' · video kept 2 min either side' : '') + '</div>' +
    '<div class="bmbtns">' +
      '<button type="button" class="btn" data-ex title="Save the 4 minutes around it as MP4">' + ic('dl') + '<span>Export</span></button>' +
      (b.can_edit ? '<button type="button" class="btn" data-ed>Edit</button><button type="button" class="btn danger" data-del>Delete</button>' : '') +
    '</div></div>');
  p.querySelector('[data-ex]').onclick = () => { closePop(); exportAround(b); };
  const ed = p.querySelector('[data-ed]');
  if (ed) ed.onclick = () => openEdit(b, anchor);
  const del = p.querySelector('[data-del]');
  if (del) del.onclick = async () => { closePop(); await remove(b); };
}

// The strip's flags: hover shows the note, click plays from there.
export function initMarks(el) {
  el.addEventListener('pointerover', (e) => { const b = e.target.closest('.tlbm'); if (b && e.pointerType !== 'touch') showTip(b); });
  el.addEventListener('pointerout', (e) => { if (e.target.closest('.tlbm')) hideTip(); });
  el.addEventListener('focusin', (e) => { const b = e.target.closest('.tlbm'); if (b) showTip(b); });
  el.addEventListener('focusout', hideTip);
  el.addEventListener('click', (e) => {
    const btn = e.target.closest('.tlbm');
    if (!btn) return;
    const b = D.list.find(x => x.id === +btn.dataset.id);
    if (!b) return;
    if (T) T.seek(b.at - 2000);
    viewPop(b, btn);
  });
}

// ---------- the list ----------
let listDlg = null, listSeq = 0;
function bmListRefresh() { if (listDlg && listDlg.isConnected && listDlg.__reload) listDlg.__reload(); }

// Play a bookmark's moment on the Live page's timeline.
async function jump(b) {
  const nav = await import('./nav.js');
  await nav.goTab('live');
  const tl = await import('./livetl.js');
  const id = b.camera ? camId(b.camera) : null;
  tl.openMoment(b.at, id);
}

export function openList() {
  const cams = localCams();
  const html = '<div class="bmlist">' +
    '<div class="bmfil"><label class="bmq">' + ic('search') + '<input type="search" placeholder="Search notes" aria-label="Search notes" autocomplete="off" spellcheck="false"></label>' +
    '<select aria-label="Camera"><option value="">All cameras</option>' + cams.map(n => '<option>' + esc(n) + '</option>').join('') + '</select></div>' +
    '<div class="bmrows" role="list" aria-live="polite"></div>' +
    '<div class="hint">Click a bookmark to play that moment. ' + (can('events') ? 'Add one on the timeline: Bookmark, or ' + '<kbd>B</kbd>.' : '') + '</div>' +
    '</div>';
  const d = openDlg('Bookmarks', html);
  d.classList.add('bmdlg');
  listDlg = d;
  const q = d.querySelector('input[type=search]'), sel = d.querySelector('select'), rows = d.querySelector('.bmrows');
  let shown = [];
  const reload = async () => {
    const seq = ++listSeq;
    let r;
    try {
      r = await api('/api/bookmarks?limit=300' + (q.value.trim() ? '&q=' + enc(q.value.trim()) : '') + (sel.value ? '&camera=' + enc(sel.value) : ''));
    } catch (e) { if (seq === listSeq) rows.innerHTML = '<div class="bmempty">' + esc(errText(e)) + '</div>'; return; }
    if (seq !== listSeq || !d.isConnected) return;
    shown = r.bookmarks || [];
    rows.innerHTML = shown.length ? shown.map((b, i) =>
      '<div class="bmr" role="listitem">' +
        '<button type="button" class="bmgo" data-i="' + i + '" title="Play this moment">' +
          '<span class="bmt"><b>' + esc(fmtWall(b.at)) + '</b><span class="tm">' + esc(where(b)) + ' · ' + esc(b.user) + '</span></span>' +
          (b.keep ? '<span class="bmk" title="Video kept 2 min either side">' + ic('lock') + '</span>' : '') +
          '<span class="bmnote">' + esc(b.note) + '</span>' +
        '</button>' +
        '<span class="bmacts">' +
          '<button type="button" class="btn icon" data-ex="' + i + '" title="Export the 4 minutes around it" aria-label="Export">' + ic('dl') + '</button>' +
          (b.can_edit ? '<button type="button" class="btn" data-ed="' + i + '">Edit</button><button type="button" class="btn icon" data-del="' + i + '" title="Delete" aria-label="Delete">' + ic('x') + '</button>' : '') +
        '</span>' +
      '</div>').join('') : '<div class="bmempty">' + (q.value.trim() || sel.value ? 'No bookmarks match.' : 'No bookmarks yet.') + '</div>';
  };
  d.__reload = reload;
  let t = 0;
  q.oninput = () => { clearTimeout(t); t = setTimeout(reload, 250); };
  q.onkeydown = (ev) => { ev.stopPropagation(); if (ev.key === 'Escape' && q.value) { ev.preventDefault(); q.value = ''; reload(); } else if (ev.key === 'Escape') { d.__close(); } };
  sel.onchange = reload;
  rows.onclick = async (ev) => {
    const go = ev.target.closest('.bmgo'), ex = ev.target.closest('[data-ex]'), ed = ev.target.closest('[data-ed]'), del = ev.target.closest('[data-del]');
    if (ex) { exportAround(shown[+ex.dataset.ex]); return; }
    if (del) { if (await remove(shown[+del.dataset.del])) reload(); return; }
    if (ed) { listEdit(shown[+ed.dataset.ed]); return; }
    if (go) { const b = shown[+go.dataset.i]; d.__close(); jump(b); }
  };
  reload();
  setTimeout(() => { try { q.focus(); } catch (e) { /* gone */ } }, 0);
}

// Edit from the list: the same form, in a small dialog of its own.
function listEdit(b) {
  const cams = localCams();
  const d = openDlg('Edit bookmark', formHtml(b, b.at, b.camera && !cams.includes(b.camera) ? cams.concat([b.camera]) : cams, b.camera || ''), true);
  d.classList.add('bmdlg');
  wireFormIn(d, b, () => d.__close(), upsert);
}
// As wireForm, for a form inside a dialog (`done` closes it).
function wireFormIn(d, b, done, onSaved) {
  const f = d.querySelector('form');
  const ta = f.elements.note, cnt = f.querySelector('.bmcount'), er = f.querySelector('.bmerr');
  const count = () => { const n = [...ta.value].length; cnt.textContent = n > NOTE_MAX - 60 ? n + ' / ' + NOTE_MAX : ''; };
  ta.oninput = count; count();
  ta.onkeydown = (ev) => { ev.stopPropagation(); if (ev.key === 'Enter' && !ev.shiftKey && !ev.isComposing) { ev.preventDefault(); f.requestSubmit(); } else if (ev.key === 'Escape') { ev.preventDefault(); done(); } };
  f.querySelector('[data-x]').onclick = done;
  f.onsubmit = async (ev) => {
    ev.preventDefault();
    const note = ta.value.trim();
    if (!note) { er.textContent = 'Write a note'; er.hidden = false; ta.focus(); return; }
    const btn = f.querySelector('[type=submit]');
    btn.disabled = true;
    try {
      const r = await api('/api/bookmarks/' + b.id, { method: 'PUT', json: bodyOf(f, b, note) });
      done();
      onSaved(r);
      toast('Bookmark saved', { kind: 'ok', ms: 2000 });
    } catch (e) { er.textContent = errText(e); er.hidden = false; btn.disabled = false; }
  };
  setTimeout(() => { try { ta.focus(); } catch (e) { /* gone */ } }, 0);
}
