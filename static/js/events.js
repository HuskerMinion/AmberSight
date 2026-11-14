// events.js - Events tab: a list beside a player (split view). Filters are chips kept in the URL
// hash (#events?cam=..&obj=..&range=..), the list pages in as you scroll, rows can be ticked for
// bulk actions, and the keyboard moves through the events.

import { renderVerdict, aiFrames } from './ai_review.js';
import { $, state, api, esc, fmtTime, stillWaiting, openDlg, msgDlg, toast, lowQ, enc, topDlgOpen, overlayOpen, backLayer, overlaysChanged, afterOverlayClose, returnOnBack, viewOnlyNote, downloadUrl, speedOptions, applySpeed, safeRate, MAX_RATE, can } from './core.js';
import { isActive, showTab } from './nav.js';
import { openFullCam, closeFull } from './live.js';

// New-event pop-ups are off unless the user turns them on (account menu).
const POPUP_KEY = 'ambs.alertPopups';

// The tab icon turns red when an alert arrives while the page is in the background,
// and goes back to amber when you look at it again.
function setFavicon(alert) {
  const l = document.getElementById('favicon');
  if (l) l.href = '/static/icons/' + (alert ? 'favicon-alert.svg' : 'favicon.svg');
}
function alertFavicon() { if (document.hidden || !document.hasFocus()) setFavicon(true); }
document.addEventListener('visibilitychange', () => { if (!document.hidden) setFavicon(false); });
window.addEventListener('focus', () => setFavicon(false));
export function alertPopups() { try { return localStorage.getItem(POPUP_KEY) === '1'; } catch (e) { return false; } }
export function setAlertPopups(on) { try { localStorage.setItem(POPUP_KEY, on ? '1' : '0'); } catch (e) { /* ignore */ } }

// ---------- classes (what an event is about) ----------
const VEH = ['car', 'truck', 'bus', 'motorcycle', 'bicycle', 'vehicle', 'van', 'bike', 'motorbike', 'suv', 'train', 'boat'];
const ANI = ['dog', 'cat', 'bird', 'animal', 'pet', 'horse', 'cow', 'sheep', 'bear', 'fox', 'deer', 'squirrel', 'rabbit', 'raccoon', 'coyote'];
const PER = ['person', 'visitor', 'people', 'human'];
const CLASS_ICON = { unusual: 'alert', person: 'person', vehicle: 'car', animal: 'paw', package: 'pkg', face: 'face', other: 'dot', motion: 'dot' };
const CLASS_COLOR = { unusual: '--warn', person: '--person', vehicle: '--vehicle', animal: '--animal', package: '--package', face: '--face', other: '--muted', motion: '--motion' };
const CLASS_NAME = { unusual: 'Unusual', person: 'Person', vehicle: 'Vehicle', animal: 'Animal', package: 'Package', face: 'Face', other: 'Other', motion: 'Motion' };
const CLASS_ORDER = ['unusual', 'person', 'face', 'package', 'vehicle', 'animal', 'other', 'motion'];
const ico = (n) => n === 'check' ? '<svg class="ic ok" viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7"/></svg>' : '<svg class="ic"><use href="#i-' + n + '"/></svg>';

// Tags made by rules, zones and plates hold the user's own words: they never say what was seen.
const TAG_PREFIX = ['rule: ', 'quiet: ', 'loitering: ', 'crossed: ', 'plate ', 'unknown plate '];

function classesOf(e) {
  if (e._cls) return e._cls;
  const set = new Set();
  const objs = (e.objects || []).map(o => String(o).toLowerCase());
  for (const o of objs) {
    if (o === 'unusual') set.add('unusual');
    else if (TAG_PREFIX.some(p => o.startsWith(p))) set.add('other');
    else if (PER.includes(o)) set.add('person');
    else if (VEH.includes(o)) set.add('vehicle');
    else if (ANI.includes(o)) set.add('animal');
    else if (o.includes('package') || o.includes('parcel')) set.add('package');
    else if (o.includes('face')) set.add('face');
    else set.add('other');
  }
  if (!set.size) set.add('motion');
  e._cls = CLASS_ORDER.filter(c => set.has(c));
  return e._cls;
}

// ---------- filters (kept in the URL hash) ----------
const F = { cams: [], objs: new Set(), range: '', from: 0, to: 0, prot: false, all: false, ev: '', hfa: false };
let V = 'list';   // 'list' or 'grid' (#events?view=grid)

function parseHash() {
  const p = new URLSearchParams(location.hash.split('?')[1] || '');
  const g = { cams: [], objs: new Set(), range: '', from: 0, to: 0, prot: false, all: false, ev: '', hfa: false, view: 'list' };
  g.view = p.get('view') === 'grid' ? 'grid' : 'list';
  // Each name is percent-encoded on its own, so a comma inside a camera name survives the comma-joined list.
  if (p.get('cam')) g.cams = p.get('cam').split(',').filter(Boolean).map(n => { try { return decodeURIComponent(n); } catch (e) { return n; } });
  if (p.get('obj')) p.get('obj').split(',').forEach(o => { if (CLASS_ORDER.includes(o)) g.objs.add(o); });
  const r = p.get('range') || '';
  if (['1h', 'today', '24h', '7d', '30d', 'custom'].includes(r)) g.range = r;
  if (g.range === 'custom') { g.from = +p.get('from') || 0; g.to = +p.get('to') || 0; if (!g.from && !g.to) g.range = ''; }
  g.prot = p.get('prot') === '1';
  g.all = p.get('all') === '1';
  g.hfa = p.get('hfa') === '1';
  g.ev = p.get('ev') || '';
  return g;
}
function hashOf() {
  const p = [];
  if (V === 'grid') p.push('view=grid');
  if (F.cams.length) p.push('cam=' + enc(F.cams.map(enc).join(',')));
  if (F.objs.size) p.push('obj=' + [...F.objs].join(','));
  if (F.range) p.push('range=' + F.range);
  if (F.range === 'custom') { if (F.from) p.push('from=' + F.from); if (F.to) p.push('to=' + F.to); }
  if (F.prot) p.push('prot=1');
  if (F.all) p.push('all=1');
  if (F.hfa) p.push('hfa=1');
  if (F.ev) p.push('ev=' + enc(F.ev));
  return '#events' + (p.length ? '?' + p.join('&') : '');
}
function writeHash() { if (isActive('events') && location.hash !== hashOf()) history.replaceState(history.state, '', hashOf()); }
function applyParsed(g) { F.cams = g.cams; F.objs = g.objs; F.range = g.range; F.from = g.from; F.to = g.to; F.prot = g.prot; F.all = g.all; F.hfa = g.hfa; F.ev = g.ev; V = g.view; }

function rangeBounds() {
  const now = Math.floor(Date.now() / 1000);
  switch (F.range) {
    case '1h': return [now - 3600, 0];
    case '24h': return [now - 86400, 0];
    case '7d': return [now - 7 * 86400, 0];
    case '30d': return [now - 30 * 86400, 0];
    case 'today': { const d = new Date(); d.setHours(0, 0, 0, 0); return [Math.floor(d.getTime() / 1000), 0]; }
    case 'custom': return [F.from || 0, F.to || 0];
    default: return [0, 0];
  }
}
// The server filters on all of these (several cameras, labels, protected, false alarms), so a page is
// a page of matches. Labels are matched as parts of words, so passes() below makes the classes exact.
const CLASS_LABELS = { unusual: ['unusual'], person: PER, vehicle: VEH, animal: ANI, package: ['package', 'parcel'], face: ['face'] };
// "Other" is everything the classes do not name, which the server cannot say: those pages are filtered here.
// Camera names may contain commas, but the server splits `cameras=` on commas: with several cameras
// selected and a comma in any name, the camera filter is applied here instead (passes()).
const commaCams = () => F.cams.length > 1 && F.cams.some(n => n.includes(','));
const needsClient = () => F.objs.has('other') || commaCams();
function serverQuery() {
  let q = '';
  const [a, b] = rangeBounds();
  if (F.cams.length === 1) q += '&camera=' + enc(F.cams[0]);
  else if (F.cams.length > 1 && !commaCams()) q += '&cameras=' + enc(F.cams.join(','));
  if (a) q += '&since=' + a;
  if (b) q += '&until=' + b;
  if (F.all) q += '&all=1';
  if (F.prot) q += '&protect=1';
  if (F.hfa) q += '&review=ok,none';
  if (F.objs.size && !needsClient()) q += '&objects=' + enc([...new Set([...F.objs].flatMap(c => CLASS_LABELS[c] || []))].join(','));
  return q;
}
export function buildFilterQuery() { return serverQuery(); }
function passes(e) {
  if (F.cams.length > 1 && !F.cams.includes(e.camera)) return false;
  if (F.prot && !e.protect) return false;
  if (F.hfa && e.review === 'false_alarm') return false;
  if (F.objs.size) { const c = classesOf(e); if (!c.some(x => F.objs.has(x) || (x === 'motion' && F.objs.has('other')))) return false; }
  return true;
}

// ---------- the list ----------
const PAGE_L = 60, PAGE_G = 36;   // events per request: the list, the grid
const pageSize = () => V === 'grid' ? PAGE_G : PAGE_L;
let aiOff = null;         // null: not known yet; true when AI review is turned off
const TS_KEY = 'as.events.lastSeen';
let visitCut = 0;        // events newer than this browser's last visit are 'new' (bold)
try { visitCut = +localStorage.getItem(TS_KEY) || Date.now(); } catch (e) { visitCut = Date.now(); }
let shownOnce = false;
window.addEventListener('pagehide', () => { if (shownOnce) { try { localStorage.setItem(TS_KEY, String(Date.now())); } catch (e) { /* not remembered */ } } });
let items = [];            // events that pass the filters, newest first
const byId = new Map();
let rawOldest = 0, rawMore = false, gen = 0, loading = false, lastRender = '';
const rowEls = new Map();
const tileEls = new Map();
const rawIds = new Set();
let selId = '', anchorId = '';
const checks = new Set();
const seen = new Set();
let started = false;
const REV_KEY = 'ambs.evRead';
let reviewed = new Set();
try { reviewed = new Set(JSON.parse(localStorage.getItem(REV_KEY) || '[]')); } catch (e) { /* none */ }
function saveReviewed() { try { localStorage.setItem(REV_KEY, JSON.stringify([...reviewed].slice(-3000))); } catch (e) { /* not remembered */ } }

export function resetEvents() { lastRender = ''; items = []; byId.clear(); rowEls.clear(); tileEls.clear(); aiOff = null; started = false; selId = ''; checks.clear(); $('eventRows') && $('eventRows').replaceChildren(); }

const isPhone = () => window.matchMedia('(max-width: 760px), (max-height: 520px) and (pointer: coarse)').matches;
const sortItems = () => items.sort((a, b) => b.start - a.start || (a.id < b.id ? 1 : -1));
const pad = (n) => String(n).padStart(2, '0');
const fmtDur = (s) => Math.floor(s / 60) + ':' + pad(Math.round(s % 60 === 0 ? 0 : s % 60));
const fmtClock = (ts) => new Date(ts * 1000).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit', second: '2-digit' });
function dayLabel(ts) {
  const d = new Date(ts * 1000), t = new Date();
  const k = (x) => x.getFullYear() * 400 + x.getMonth() * 32 + x.getDate();
  const y = new Date(t); y.setDate(t.getDate() - 1);
  if (k(d) === k(t)) return 'Today';
  if (k(d) === k(y)) return 'Yesterday';
  return d.toLocaleDateString([], { weekday: 'short', month: 'short', day: 'numeric' });
}
const hourLabel = (ts) => { const d = new Date(ts * 1000); d.setMinutes(0, 0, 0); return dayLabel(ts) + ' · ' + d.toLocaleTimeString([], { hour: 'numeric' }); };

async function fetchPage(until) {
  let q = '/api/events?limit=' + pageSize() + serverQuery();
  if (until) q = q.replace(/&until=\d+/, '') + '&until=' + until;
  return (await api(q)).events || [];
}
function noteNew(list) {
  for (const e of list) {
    if (seen.has(e.id)) continue;
    seen.add(e.id);
    if (state.toastsOn) alertFavicon();
    if (state.toastsOn && alertPopups()) toast('Motion: ' + e.camera + ' ' + fmtTime(e.start), () => openFromAlert(e));
  }
}
function absorb(list) {
  let changed = false;
  for (const e of list) {
    const old = byId.get(e.id);
    if (old) {
      if (old.protect !== e.protect || old.review !== e.review || old.clip !== e.clip || old.end !== e.end || old.thumb !== e.thumb ||
          String(old.objects || '') !== String(e.objects || '')) { Object.assign(old, e, { _cls: null }); changed = true; }
    } else if (passes(e)) { byId.set(e.id, e); items.push(e); changed = true; }
  }
  return changed;
}

// Load (or reload) from the top after the filters changed.
async function loadFirst() {
  const g = ++gen;
  items = []; byId.clear(); rawIds.clear(); rawOldest = 0; rawMore = false; lastRender = '';
  loading = true; paintStatus(); render();
  try {
    const page = await fetchPage(0);
    if (g !== gen) return;
    noteNew(page); page.forEach(e => rawIds.add(e.id));
    rawMore = page.length >= pageSize(); if (page.length) rawOldest = page[page.length - 1].start;
    absorb(page); sortItems();
    // Only the "Other" class is filtered here, which can leave a page short: keep paging until there are some rows.
    if (needsClient()) for (let i = 0; i < 6 && rawMore && items.length < 25; i++) await pageMore(g, true);
  } finally { if (g === gen) loading = false; }
  if (g === gen) { render(); autoSelect(); probeAi(); }
}
// One look at the newest event tells whether AI review is off (shown once above the list, not under every event).
// again: look once more (coming back to Events: AI may have been turned on or off meanwhile).
async function probeAi(again) {
  if ((aiOff !== null && !again) || !items.length) return;
  try { const r = await api('/api/events/' + enc(items[0].id) + '/ai'); aiOff = !(r.ai && r.ai.enabled); } catch (e) { return; }
  const n = $('evAiOff'); if (n) n.hidden = !aiOff;
  if (aiOff) document.querySelectorAll('#evPlayer [data-verdict]').forEach(v => { v.hidden = true; });
  else if (selId) document.querySelectorAll('#evPlayer [data-verdict][hidden]').forEach(v => { v.hidden = false; renderVerdict(v, selId, verdictOpts(byId.get(selId))).catch(() => {}); });
}
// The verdict box's "Not a person" marks change the event's review like "Not a real alert".
function verdictOpts(e) {
  return { onReview: (rv) => {
    if (!e) return;
    e.review = rv;
    if (rv === 'false_alarm' && F.hfa) { dropItem(e.id); lastRender = ''; render(); return; }
    lastRender = ''; render(); paintActions(e);
  } };
}
async function pageMore(g, quiet) {
  if (!rawMore || !rawOldest) return;
  const page = await fetchPage(rawOldest);
  if (g !== gen) return;
  const before = byId.size;
  const fresh = page.filter(e => !rawIds.has(e.id));
  fresh.forEach(e => rawIds.add(e.id));
  rawMore = page.length >= pageSize() && fresh.length > 0;
  if (page.length) rawOldest = page[page.length - 1].start;
  absorb(page); sortItems();
  if (!quiet) render();
  return byId.size - before;
}
async function loadMore() {
  if (loading || !rawMore) return;
  loading = true; paintStatus();
  const g = gen;
  try { for (let i = 0; i < 6 && rawMore; i++) { const n = await pageMore(g, true); if (g !== gen) return; if (n >= 20) break; } }
  catch (e) { /* try again on the next scroll */ }
  loading = false; render();
}

// The 10 s poll: new events come in on top, nothing else moves.
export async function refreshEvents() {
  if (!started) { started = true; await loadFirst(); return; }
  const g = gen;
  const page = await fetchPage(0);
  if (g !== gen) return;
  noteNew(page);
  if (absorb(page)) { sortItems(); render(); }
}

// ---------- rendering ----------
function paintStatus() {
  const m = $('evMoreRow'); if (!m) return;
  m.hidden = !(rawMore || loading);
  const b = $('evLoadMore'); if (b) { b.disabled = loading; b.textContent = loading ? 'Loading...' : 'Load more'; }
  // Still in view after a page came in (a short page): look again, so the next page loads.
  if (!m.hidden && !loading && moreIO) { moreIO.unobserve(m); moreIO.observe(m); }
}
let moreIO = null;
const isNew = (e) => e.start * 1000 > visitCut && !reviewed.has(e.id) && !e.review;
const DOT_TIP = 'Blue dot: you have not looked at this event yet';
const typeText = (e) => classesOf(e).filter(c => c !== 'other' || classesOf(e).length === 1).map(c => CLASS_NAME[c]).slice(0, 2).join(', ');
function decorate(el, e) {
  el.classList.toggle('on', e.id === selId);
  el.classList.toggle('rd', (reviewed.has(e.id) || e.review === 'ok') && e.id !== selId);
  el.classList.toggle('fa', e.review === 'false_alarm');
  el.classList.toggle('new', isNew(e));
  el.setAttribute('aria-selected', e.id === selId ? 'true' : 'false');
}
function tileFor(e) {
  let el = tileEls.get(e.id);
  if (!el) {
    el = document.createElement('div');
    el.className = 'evtile'; el.dataset.id = e.id; el.setAttribute('role', 'option');
    const lab = e.camera + ', ' + fmtClock(e.start);
    el.innerHTML = '<div class="tpic">' + (e.thumb ? '<img loading="lazy" decoding="async" src="/api/events/' + enc(e.id) + '/thumb" alt="' + esc(lab) + '">' : '<span class="nothumb" role="img" aria-label="' + esc(lab) + ', no picture"></span>') + '</div>' +
      '<span class="tp tcam"><span class="ud" role="img" aria-label="New" title="' + DOT_TIP + '"></span><span class="nm">' + esc(e.camera) + '</span></span>' +
      '<span class="tp ttype"></span><span class="tp ttime"></span>';
    el.onclick = () => selectEvent(e.id, { open: true });
    tileEls.set(e.id, el);
  }
  // What it is (type, icon, colour) follows the event: an AI result can arrive after the tile was made.
  const cl = classesOf(e), kind = cl.join();
  if (el.__kind !== kind) {
    el.__kind = kind;
    el.style.setProperty('--c', 'var(' + CLASS_COLOR[cl[0]] + ')');
    const tt = el.querySelector('.ttype');
    tt.title = typeText(e);
    tt.innerHTML = '<svg class="ic"><use href="#i-' + CLASS_ICON[cl[0]] + '"/></svg><span class="tt">' + esc(typeText(e)) + '</span>';
  }
  decorate(el, e);
  el.querySelector('.ttime').innerHTML = fmtClock(e.start) + (e.protect ? ' ' + ico('lock') : '') + (e.review === 'ok' ? ' ' + ico('check') : '') + (e.review === 'false_alarm' ? ' · not real' : '') + (e.clip ? '' : ' · no clip');
  return el;
}
function rowFor(e) {
  let el = rowEls.get(e.id);
  if (!el) {
    el = document.createElement('div');
    el.className = 'evr'; el.dataset.id = e.id; el.setAttribute('role', 'option');
    el.innerHTML = '<label class="evck" title="Select (Shift-click for a range)"><input type="checkbox" aria-label="Select event"></label>' +
      (e.thumb ? '<img class="evt" loading="lazy" decoding="async" src="/api/events/' + enc(e.id) + '/thumb" alt="">' : '<span class="nothumb"></span>') +
      '<div class="evi"><div class="t1"><span class="ud" role="img" aria-label="New" title="' + DOT_TIP + '"></span><span class="evk"></span><span class="nm">' + esc(e.camera) + '</span></div><div class="t2"></div></div>';
    el.querySelector('.evck').onclick = (ev) => ev.stopPropagation();
    el.querySelector('input').onclick = (ev) => { ev.stopPropagation(); tick(e.id, ev.target.checked, ev.shiftKey); };
    el.onclick = (ev) => { if (ev.shiftKey && anchorId) { ev.preventDefault(); tick(e.id, true, true); return; } selectEvent(e.id, { open: true }); };
    wireHover(el, e);
    rowEls.set(e.id, el);
  }
  const cl = classesOf(e), kind = cl.join();
  if (el.__kind !== kind) {
    el.__kind = kind;
    el.style.setProperty('--c', 'var(' + CLASS_COLOR[cl[0]] + ')');
    el.querySelector('.evk').innerHTML = cl.filter(c => c !== 'motion' && c !== 'other').map(c => '<svg class="ic" style="color:var(' + CLASS_COLOR[c] + ')"><use href="#i-' + CLASS_ICON[c] + '"/></svg>').join('');
  }
  decorate(el, e);
  const ck = checks.has(e.id);
  el.classList.toggle('ck', ck);
  el.querySelector('input').checked = ck;
  el.querySelector('.t2').innerHTML = fmtClock(e.start) + ' · ' + fmtDur(Math.max(0, e.end - e.start)) + (e.protect ? ' ' + ico('lock') : '') + (e.review === 'ok' ? ' ' + ico('check') : '') + (e.review === 'false_alarm' ? ' · not a real alert' : '') + (e.clip ? '' : ' · no clip');
  return el;
}
function render() {
  const rows = $('eventRows'), scr = $('evList');
  if (!rows) return;
  const sig = items.map(e => e.id + (e.protect ? 'p' : '') + (e.review ? e.review[0] : '') + (e.clip ? 'c' : '') + e.end + classesOf(e).join('+')).join() + '|' + selId + '|' + [...checks].join() + '|' + reviewed.size + '|' + rawMore + loading + V + aiOff;
  if (sig === lastRender) return;
  lastRender = sig;
  // Remember where the viewer is, so rows added on top do not push the list around.
  // The open event's row when it is on screen (a link to an older event scrolls to it, and a
  // page loaded above it must not push it away), else the top row in view.
  let anchor = null, anchorTop = 0;
  const box = scr.getBoundingClientRect();
  const sel = selId ? (V === 'grid' ? tileEls.get(selId) : rowEls.get(selId)) : null;
  if (sel && sel.isConnected) {
    const r = sel.getBoundingClientRect();
    if (r.bottom > box.top && r.top < box.bottom) { anchor = sel; anchorTop = r.top; }
  }
  if (!anchor && scr.scrollTop > 4) {
    for (const el of rows.querySelectorAll('.evr, .evtile')) { const r = el.getBoundingClientRect(); if (r.bottom > box.top + 20) { anchor = el; anchorTop = r.top; break; } }
  }
  const nodes = [];
  let lastH = '';
  if (!items.length) {
    const p = document.createElement('p'); p.className = 'empty';
    const filtered = F.cams.length || F.objs.size || F.range || F.prot || F.hfa;
    p.textContent = loading ? 'Loading...' : (filtered ? 'No events match these filters.' : 'No events yet. Walk past a camera to test it. Events appear here when something moves.');
    nodes.push(p);
    if (!loading && !filtered && !state.cams.length) {
      const a = document.createElement('a'); a.className = 'btn primary'; a.href = '#settings/cameras/find'; a.textContent = 'Add a camera';
      const w = document.createElement('p'); w.className = 'evnone'; w.appendChild(a); nodes.push(w);
    }
  }
  let grid = null;
  for (const e of items) {
    if (V === 'grid') {
      const h = dayLabel(e.start);
      if (h !== lastH) { lastH = h; const d = document.createElement('div'); d.className = 'evh'; d.textContent = h; nodes.push(d); grid = document.createElement('div'); grid.className = 'evgrid'; nodes.push(grid); }
      grid.appendChild(tileFor(e));
    } else {
      const h = hourLabel(e.start);
      if (h !== lastH) { lastH = h; const d = document.createElement('div'); d.className = 'evh'; d.textContent = h; nodes.push(d); }
      nodes.push(rowFor(e));
    }
  }
  if (items.length && !rawMore && !loading) { const d = document.createElement('div'); d.className = 'evend'; d.textContent = 'End of list'; nodes.push(d); }
  rows.replaceChildren(...nodes);
  for (const id of [...rowEls.keys()]) if (!byId.has(id)) rowEls.delete(id);
  for (const id of [...tileEls.keys()]) if (!byId.has(id)) tileEls.delete(id);
  if (anchor && anchor.isConnected) scr.scrollTop += anchor.getBoundingClientRect().top - anchorTop;
  paintStatus(); paintBulk(); paintChips();
  const lst = $('evListWrap'); if (lst) lst.classList.toggle('bulk', checks.size > 0);
}

// ---------- selection and the player ----------
let readTimer = 0, curVideo = null;
function markReviewed(id) { if (!reviewed.has(id)) { reviewed.add(id); saveReviewed(); lastRender = ''; render(); } }

// A link to one event (#events?ev=<id>): the id the address brought in, until it has been opened.
let deepId = '', missingEv = '';
// The event is older than the pages loaded so far (or filtered out): ask for it by its id. The server
// answers only for cameras this account may see. Not there or not allowed: say so, leave the address alone.
async function openDeep(id) {
  const g = gen;
  let ev = null;
  try { ev = ((await api('/api/events?limit=1&all=1&ids=' + enc(id))).events || [])[0] || null; } catch (e) { ev = null; }
  // (Only that event: never another one in its place.)
  if (ev && ev.id !== id) ev = null;
  if (g !== gen || F.ev !== id) return;
  if (!ev) { missingEv = id; deepId = ''; toast('That event is not there any more, or you are not allowed to see it.', { kind: 'err' }); return; }
  if (!byId.has(id)) { byId.set(id, ev); items.push(ev); sortItems(); }
  lastRender = ''; render();
  deepId = ''; selectEvent(id, { scroll: true, open: true });
}
function autoSelect() {
  if (F.ev && !byId.has(F.ev)) {
    if (missingEv === F.ev) return;
    if (deepId === F.ev) { openDeep(F.ev); return; }
  }
  if (F.ev && byId.has(F.ev)) {
    if (selId !== F.ev || deepId === F.ev) { const open = deepId === F.ev; deepId = ''; selectEvent(F.ev, { scroll: true, open }); }
    return;
  }
  if (isPhone()) return;
  if (!selId || !byId.has(selId)) { if (items.length) selectEvent(items[0].id, {}); else { selId = ''; paintPlayer(null); } }
}
export function selectEvent(id, o) {
  o = o || {};
  const e = byId.get(id);
  if (!e) return;
  const changed = selId !== id;
  selId = id; F.ev = id; anchorId = id;
  lastRender = ''; render();
  clearTimeout(readTimer); readTimer = setTimeout(() => markReviewed(id), 1500);
  if (changed || !$('evPlayer').querySelector('.evp-vid')) paintPlayer(e);
  if (o.open && isPhone()) setPanel(true);
  const el = (V === 'grid' ? tileEls : rowEls).get(id);
  if (el && (o.scroll || o.key)) el.scrollIntoView({ block: 'nearest' });
  writeHash();
}

let panelOpen = false;
function setPanel(on) {
  panelOpen = on;
  // (The player is out of sight on a phone: its clip stops.)
  if (!on && isPhone() && curVideo) { try { curVideo.pause(); } catch (x) { /* gone */ } }
  $('tab-events').classList.toggle('evx-open', on);
  overlaysChanged();
}

// Phone sheet: swipe down from the top (or the title) to close it.
function wireSwipe() {
  const box = $('evPlayer');
  let y0 = 0, x0 = 0, drag = false;
  box.addEventListener('touchstart', (ev) => {
    drag = false;
    if (!panelOpen || ev.touches.length !== 1 || box.scrollTop > 2 || ev.target.closest('video, .evp-mtl, select, .evv')) return;
    y0 = ev.touches[0].clientY; x0 = ev.touches[0].clientX; drag = true;
  }, { passive: true });
  box.addEventListener('touchmove', (ev) => {
    if (!drag) return;
    const dy = ev.touches[0].clientY - y0;
    if (Math.abs(ev.touches[0].clientX - x0) > Math.abs(dy)) { drag = false; box.style.transform = ''; return; }
    box.style.transform = dy > 0 ? 'translateY(' + dy + 'px)' : '';
  }, { passive: true });
  const end = (ev) => {
    if (!drag) return; drag = false;
    const dy = (ev.changedTouches[0] || {}).clientY - y0;
    box.style.transform = '';
    if (dy > 90) setPanel(false);
  };
  box.addEventListener('touchend', end);
  box.addEventListener('touchcancel', () => { drag = false; box.style.transform = ''; });
}

async function openOnTimeline(e) {
  const cam = state.cams.find(c => c.name === e.camera);
  if (!cam) { toast(e.camera + ' is no longer a camera here', { kind: 'err' }); return; }
  if (cam.remote) { toast(e.camera + ': its recordings are on ' + ((cam.remote && cam.remote.site) || 'the other server')); return; }
  // (The phone sheet's history step is taken back first, so it does not undo the page change.)
  if (panelOpen) await afterOverlayClose(() => setPanel(false));
  const back = location.hash;
  history.pushState(null, '', back);   // the browser's Back comes back to this list...
  returnOnBack(back);                  // ...in one press, whatever the timeline opened on the way
  showTab('live');
  import('./livetl.js').then(m => m.openAt(cam.id, e.start * 1000));
}
// Open the camera's full view for an alert (its timeline button then starts at the alert).
function openFromAlert(e) {
  const cam = state.cams.find(c => c.name === e.camera);
  if (!cam) { toast(e.camera + ' is no longer a camera here', { kind: 'err' }); return; }
  showTab('live');
  openFullCam(cam.id, { alertAt: e.start * 1000 });
}

// Leaving Events (another page, or the browser tab hidden): the clip stops, and the place in the list is kept.
export function hideEvents() {
  if (curVideo) { try { curVideo.pause(); } catch (x) { /* gone */ } }
  if (isActive('events') && $('evList')) place = { top: $('evList').scrollTop, sel: selId, h: hashSansEv() };
}

// ---------- keep the place in the list (power user) ----------
// Opening an event on the timeline and coming back (Back, or G then E) returns to the same scroll position and event.
let place = null;
const hashSansEv = () => { const e = F.ev; F.ev = ''; const h = hashOf(); F.ev = e; return h; };
function restorePlace() {
  const p = place; place = null;
  if (!p || p.h !== hashSansEv()) return;
  if (p.sel && byId.has(p.sel) && selId !== p.sel && !isPhone()) selectEvent(p.sel, {});
  requestAnimationFrame(() => { const s = $('evList'); if (s) s.scrollTop = p.top; });
}

// ---------- saved filters (power user) ----------
// Up to 12 named filter sets, kept in this browser. They show as chips at the start of the filter row.
// savedFilters() -> [{ name, hash }]; the hash opens the filters (location.hash = hash). For the command palette.
const SAVED_KEY = 'ambs.evSaved', SAVED_MAX = 12;
export function savedFilters() {
  try { const a = JSON.parse(localStorage.getItem(SAVED_KEY) || '[]'); return Array.isArray(a) ? a.filter(x => x && typeof x.name === 'string' && typeof x.hash === 'string').slice(0, SAVED_MAX) : []; } catch (e) { return []; }
}
function storeSaved(list) { try { localStorage.setItem(SAVED_KEY, JSON.stringify(list.slice(0, SAVED_MAX))); } catch (e) { toast('Could not save in this browser', { kind: 'err' }); } paintSaved(); }
function paintSaved() {
  const row = $('evChips'); if (!row) return;
  row.querySelectorAll('.evsaved').forEach(n => n.remove());
  const list = savedFilters(); if (!list.length) return;
  const cur = hashSansEv(), first = row.firstChild, frag = document.createDocumentFragment();
  for (const s of list) {
    const b = document.createElement('span'); b.className = 'chip evchip evsaved'; b.dataset.hash = s.hash;
    const on = s.hash === cur;
    b.classList.toggle('on', on);
    b.innerHTML = '<button type="button" class="evs-go" aria-pressed="' + on + '" title="Show these filters">' + esc(s.name) + '</button><button type="button" class="evs-x" aria-label="Remove ' + esc(s.name) + '" title="Remove">&times;</button>';
    b.querySelector('.evs-go').onclick = () => { if (location.hash !== s.hash) location.hash = s.hash; };
    b.querySelector('.evs-x').onclick = () => storeSaved(savedFilters().filter(x => !(x.name === s.name && x.hash === s.hash)));
    frag.appendChild(b);
  }
  const sep = document.createElement('span'); sep.className = 'evsep evsaved'; sep.setAttribute('aria-hidden', 'true'); frag.appendChild(sep);
  row.insertBefore(frag, first);
}
function saveFiltersPopup(btn) {
  const h = hashSansEv();
  if (h === '#events' || h === '#events?view=grid') { toast('Choose some filters first (cameras, what, when)', { kind: 'err' }); return; }
  openPop(btn, '<label class="sub" style="display:block">Name for these filters<input type="text" id="evSaveName" maxlength="40" placeholder="Driveway people today"></label><div class="pr"><button type="button" class="btn primary" data-go>Save</button></div>', (d) => {
    const inp = d.querySelector('#evSaveName');
    const go = () => {
      const name = inp.value.trim(); if (!name) { inp.focus(); return; }
      const list = savedFilters().filter(x => x.name !== name);
      if (list.length >= SAVED_MAX) { toast('You can keep ' + SAVED_MAX + ' saved filters; remove one first', { kind: 'err' }); return; }
      list.push({ name, hash: h }); storeSaved(list); closePop(); toast('Saved "' + name + '"', { kind: 'ok', ms: 2000 });
    };
    d.querySelector('[data-go]').onclick = go;
    inp.onkeydown = (e) => { if (e.key === 'Enter') { e.preventDefault(); go(); } e.stopPropagation(); };
    inp.focus();
  });
}
document.addEventListener('visibilitychange', () => { if (document.hidden) hideEvents(); });

function paintPlayer(e) {
  const box = $('evPlayer');
  if (shareFile && (!e || shareFile.id !== e.id)) shareFile = null;   // frees the fetched clip
  if (curVideo) { try { curVideo.pause(); curVideo.removeAttribute('src'); curVideo.load(); } catch (x) { /* gone */ } curVideo = null; }
  if (!e) { box.innerHTML = '<p class="empty evnone">Select an event to see it here.</p>'; return; }
  const cl = classesOf(e);
  const objs = (e.objects && e.objects.length) ? e.objects.join(', ') : 'motion';
  box.style.setProperty('--c', 'var(' + CLASS_COLOR[cl[0]] + ')');
  box.innerHTML =
    '<div class="evp-top"><button type="button" class="btn icon evp-back" aria-label="Close" title="Close"><svg class="ic"><use href="#i-x"/></svg></button>' +
    '<h3>' + esc(e.camera) + '</h3><small>' + esc(fmtTime(e.start)) + ' · ' + fmtDur(Math.max(0, e.end - e.start)) + ' · ' + esc(objs) + '</small></div>' +
    '<div class="evp-vid">' + (e.clip ? '<video controls playsinline preload="auto" poster="' + (e.thumb ? '/api/events/' + enc(e.id) + '/thumb' : '') + '"></video>' : (e.thumb ? '<img src="/api/events/' + enc(e.id) + '/thumb" alt="">' : '') + '<span class="nc">No clip</span>') + '</div>' +
    (e.clip ? '<div class="evp-ctl"><div class="evp-mtl" title="Click to jump"><i></i><b></b></div><label class="evp-speed">Speed <select aria-label="Playback speed">' + speedOptions(state.speed, MAX_RATE) + '</select></label></div>' : '') +
    '<div class="evp-acts"></div>' +
    '<div class="evp-keys"><kbd>↑</kbd><kbd>↓</kbd> or <kbd>J</kbd><kbd>K</kbd> next and previous · <kbd>Space</kbd> play · ' + (can('events') ? '<kbd>P</kbd> keep forever · <kbd>X</kbd> not a real alert · ' : '') + '<kbd>O</kbd> looks fine · <kbd>Enter</kbd> open on the timeline</div>' +
    '<div class="evv" data-verdict></div><div data-evfaces></div><div data-evplates></div>';
  box.querySelector('.evp-back').onclick = () => setPanel(false);
  const v = box.querySelector('video');
  if (v) {
    curVideo = v;
    v.defaultPlaybackRate = safeRate(state.speed);
    v.src = '/api/clips/' + enc(e.id) + lowQ(true);
    v.addEventListener('loadedmetadata', () => { try { v.playbackRate = safeRate(state.speed); } catch (x) { /* unsupported */ } });
    const sp = box.querySelector('.evp-speed select');
    sp.onchange = () => { state.speed = parseFloat(sp.value || '1'); applySpeed(); };
    const bar = box.querySelector('.evp-mtl'), head = bar.querySelector('b');
    v.addEventListener('timeupdate', () => { if (v.duration) head.style.left = (v.currentTime / v.duration * 100) + '%'; });
    bar.onclick = (ev) => { const r = bar.getBoundingClientRect(); if (v.duration) v.currentTime = Math.max(0, Math.min(1, (ev.clientX - r.left) / r.width)) * v.duration; };
    // Plays by itself only while Events is on screen: the list is also kept
    // up to date in the background, and a new event there must not start
    // playing its sound while another page is showing.
    if (isActive('events') && !document.hidden) { const p = v.play(); if (p && p.catch) p.catch(() => {}); }
  }
  paintActions(e);
  const vbox = box.querySelector('[data-verdict]');
  vbox.hidden = aiOff === true;
  if (aiOff !== true) renderVerdict(vbox, e.id, verdictOpts(e)).then(() => { if (aiOff === true) vbox.hidden = true; }).catch(() => {});
  if (state.isAdmin) showFaces(e, box.querySelector('[data-evfaces]'));
  if ((e.objects || []).some(o => /plate/i.test(o))) showPlates(e, box.querySelector('[data-evplates]'));
}
function paintActions(e) {
  const a = $('evPlayer').querySelector('.evp-acts');
  if (!a || selId !== e.id) return;
  const fa = e.review === 'false_alarm', ok = isOk(e);
  const cam = can('pause') ? state.cams.find(c => c.id === e.camera_id && !c.remote) : null;
  a.innerHTML = (e.clip ? '<a class="btn" data-a="save" href="/api/clips/' + enc(e.id) + '?download=1" download title="Download this clip">' + ico('dl') + '<span>Save</span></a>' +
      '<button type="button" class="btn" data-a="share" title="Share this clip">' + ico('share') + '<span>Share</span></button>' : '') +
    // (Keeping and "not a real alert" change the event for everyone: admins and operators allowed to, as in the bulk bar.)
    (!can('events') ? '' : '<button type="button" class="btn' + (e.protect ? ' on' : '') + '" data-a="p" aria-pressed="' + !!e.protect + '" title="' + (e.protect ? 'Kept: it will not be deleted. Click to let it expire normally (P)' : 'Keep this clip forever, even when old recordings are cleared (P)') + '">' + ico('lock') + '<span>' + (e.protect ? 'Kept' : 'Keep forever') + '</span></button>') +
    '<button type="button" class="btn ok' + (ok ? ' on' : '') + '" data-a="ok" aria-pressed="' + ok + '" title="' + (ok ? 'Marked as looks fine. Click to undo (O)' : 'You looked at it and it is fine (O)') + '">' + ico('check') + '<span>Looks fine</span></button>' +
    (!can('events') ? '' : '<button type="button" class="btn fa' + (fa ? ' on' : '') + '" data-a="fa" aria-pressed="' + fa + '" title="' + (fa ? 'Marked as not a real alert. Click to undo (X)' : 'Nothing was there: mark as not a real alert (X)') + '">' + ico('x') + '<span>Not a real alert</span></button>') +
    (!cam ? '' : '<button type="button" class="btn' + (cam.alerts_paused ? ' on' : '') + '" data-a="np" aria-pressed="' + !!cam.alerts_paused + '" title="' + (cam.alerts_paused ? 'Notifications from this camera are paused; it is still recording. Click to resume.' : 'Stop notifications from this camera for an hour. It keeps recording and detecting.') + '">' + ico('belloff') + '<span>' + (cam.alerts_paused ? 'Resume notifications' : 'Pause notifications 1 h') + '</span></button>') +
    (smartOn && classesOf(e).some(c => c === 'person' || c === 'vehicle') ? '<button type="button" class="btn" data-a="fw" title="Find this person or vehicle on other cameras around this time">' + ico('person') + '<span>Follow</span></button>' : '') +
    '<button type="button" class="btn" data-a="tl" title="Play the recording around this event (Enter)">' + ico('timeline') + '<span>Open on timeline</span></button>';
  const sh = a.querySelector('[data-a=share]');
  if (sh) { sh.onclick = () => shareClip(e, sh); if (shareFile && shareFile.id === e.id) shareLabel(sh, 'Share now'); }
  const fw = a.querySelector('[data-a=fw]');
  if (fw) fw.onclick = () => import('./follow.js').then(m => m.openFollow(e.id));
  const np = a.querySelector('[data-a=np]');
  if (np) np.onclick = () => import('./cameras.js').then(m => m.pauseAlertsFor(cam, cam.alerts_paused ? 0 : 60)).then(() => paintActions(e));
  const pb = a.querySelector('[data-a=p]'), fb = a.querySelector('[data-a=fa]');
  if (pb) pb.onclick = () => toggleProtect(e);
  if (fb) fb.onclick = () => toggleReview(e, 'false_alarm');
  a.querySelector('[data-a=ok]').onclick = () => toggleReview(e, 'ok');
  a.querySelector('[data-a=tl]').onclick = () => openOnTimeline(e);
}
// Share: the phone's share sheet where there is one, else the clip's link goes to the clipboard.
// Phones share the video itself, so it reaches people who cannot sign in to this server. Fetching it
// takes a moment, and a browser may refuse a share that does not follow a tap at once: then the
// button turns into "Share now" and the next tap shares the file that is ready.
let shareFile = null;   // { id, file } ready for the next tap; dropped when another event is shown
function shareLabel(btn, text, busy) {
  if (!btn || !btn.isConnected) return;
  const s = btn.querySelector('span'); if (s) s.textContent = text;
  btn.disabled = !!busy;
  btn.classList.toggle('primary', text === 'Share now');
}
async function shareClip(e, btn) {
  const url = location.origin + '/api/clips/' + enc(e.id);
  const title = e.camera + ', ' + fmtTime(e.start);
  if (navigator.canShare && navigator.share) {
    let file = shareFile && shareFile.id === e.id ? shareFile.file : null;
    const ready = !!file;
    if (!file) {
      shareLabel(btn, 'Preparing…', true);
      try {
        const r = await fetch('/api/clips/' + enc(e.id), { credentials: 'same-origin' });
        if (r.ok) {
          const blob = await r.blob();
          const name = (e.camera + ' ' + new Date(e.start * 1000).toISOString().slice(0, 16)).replace(/[^\w -]+/g, '-') + '.mp4';
          file = new File([blob], name, { type: 'video/mp4' });
          if (!navigator.canShare({ files: [file] })) file = null;
        }
      } catch (x) { file = null; }
      shareLabel(btn, 'Share');
      if (selId !== e.id) return;   // another event was opened meanwhile
    }
    if (file) {
      try { await navigator.share({ files: [file], title }); shareFile = null; shareLabel(btn, 'Share'); return; }
      catch (x) {
        if (x && x.name === 'AbortError') { shareFile = null; shareLabel(btn, 'Share'); return; }
        // Refused for want of a fresh tap: keep the file for the next one.
        if (x && x.name === 'NotAllowedError' && !ready) { shareFile = { id: e.id, file }; shareLabel(btn, 'Share now'); return; }
        shareFile = null; shareLabel(btn, 'Share');
      }
    }
  }
  if (navigator.share) { try { await navigator.share({ title, url }); return; } catch (x) { if (x && x.name === 'AbortError') return; } }
  let ok = false;
  try { await navigator.clipboard.writeText(url); ok = true; } catch (x) {
    const t = document.createElement('textarea'); t.value = url; t.style.position = 'fixed'; t.style.opacity = '0';
    document.body.appendChild(t); t.select(); try { ok = document.execCommand('copy'); } catch (y) { /* no clipboard */ } t.remove();
  }
  if (ok) toast('Link copied', { kind: 'ok', ms: 2000 }); else msgDlg('Copy this link: ' + url);
}
async function showFaces(e, box) {
  try {
    const m = await import('./admin_faces.js');
    const h = await m.eventFacesHtml(e.id);
    if (!box.isConnected) return;
    box.innerHTML = h;
    m.wireEventFaces(box, e.id, () => showFaces(e, box));
  } catch (x) { /* no faces */ }
}

async function showPlates(e, box) {
  try {
    const m = await import('./admin_lpr.js');
    const h = await m.eventPlatesHtml(e.id);
    if (!box.isConnected) return;
    box.innerHTML = h;
    m.wireEventPlates(box, e.id, () => showPlates(e, box));
  } catch (x) { /* no plates */ }
}

async function setProtect(id, val, quiet) {
  if (!can('events')) { if (!quiet) viewOnlyNote(); return false; }
  try {
    await api('/api/events/' + enc(id) + '/protect', { json: { protect: val } });
    const e = byId.get(id); if (e) e.protect = val;
    return true;
  } catch (x) { if (!quiet) msgDlg(x.message); return false; }
}
async function toggleProtect(e) {
  if (await setProtect(e.id, !e.protect)) { lastRender = ''; render(); paintActions(e); toast(e.protect ? 'Kept: it will not be deleted' : 'No longer kept forever', { kind: 'ok', ms: 2500 }); }
}

// ---------- verdicts: false alarm / reviewed OK ----------
// OK is kept on the server for accounts that may change events; a view-only account keeps it in this browser.
const isOk = (e) => e.review === 'ok' || (!can('events') && reviewed.has(e.id));
async function setReview(id, verdict, quiet) {
  if (!can('events')) { if (!quiet) viewOnlyNote(); return false; }
  try {
    const r = await api('/api/events/' + enc(id) + '/review', { json: { verdict } });
    const e = byId.get(id); if (e) { e.review = r.review || null; e.reviewed_at = r.reviewed_at || null; }
    return r;
  } catch (x) { if (!quiet) msgDlg(x.message); return false; }
}
function dropItem(id) {
  const i = items.findIndex(x => x.id === id);
  if (i < 0) return;
  items.splice(i, 1); byId.delete(id); checks.delete(id);
  if (selId === id) {
    const n = items[Math.min(i, items.length - 1)];
    if (n) selectEvent(n.id, { key: true }); else { selId = ''; F.ev = ''; paintPlayer(null); writeHash(); }
  }
}
async function toggleReview(e, verdict) {
  if (verdict === 'ok' && !can('events')) {
    if (reviewed.has(e.id)) reviewed.delete(e.id); else reviewed.add(e.id);
    saveReviewed(); lastRender = ''; render(); paintActions(e); return;
  }
  const next = e.review === verdict ? null : verdict;
  const r = await setReview(e.id, next);
  if (!r) return;
  if (next === 'false_alarm' && F.hfa) { dropItem(e.id); lastRender = ''; render(); }
  else { lastRender = ''; render(); paintActions(e); }
  // (Only "Not a person" under the AI verdict also teaches AI the spot: see ai_review.js markNotReal.)
  toast(next === 'false_alarm' ? 'Marked as not a real alert' : next === 'ok' ? 'Marked as looks fine' : 'Mark cleared', { kind: 'ok', ms: 2000 });
}

// ---------- moving by keyboard ----------
function step(d) {
  if (!items.length) return;
  let i = items.findIndex(e => e.id === selId);
  i = i < 0 ? 0 : Math.max(0, Math.min(items.length - 1, i + d));
  selectEvent(items[i].id, { key: true });
  if (i >= items.length - 8) loadMore();
}
// Grid view: the arrow keys move between tiles, up and down to the nearest tile in the next row.
function gridStep(k) {
  if (!items.length) return;
  const cur = tileEls.get(selId);
  if (!cur) { selectEvent(items[0].id, { key: true }); return; }
  const i = items.findIndex(e => e.id === selId);
  if (k === 'ArrowLeft' || k === 'ArrowRight') { step(k === 'ArrowLeft' ? -1 : 1); return; }
  const r = cur.getBoundingClientRect(), cx = r.left + r.width / 2, cy = r.top + r.height / 2, dir = k === 'ArrowDown' ? 1 : -1;
  let best = null, bestScore = Infinity;
  for (const e of items) {
    const t = tileEls.get(e.id); if (!t || e.id === selId) continue;
    const b = t.getBoundingClientRect(), dy = (b.top + b.height / 2 - cy) * dir;
    if (dy < b.height / 2) continue;
    const score = dy * 4 + Math.abs(b.left + b.width / 2 - cx);
    if (score < bestScore) { bestScore = score; best = e; }
  }
  if (best) selectEvent(best.id, { key: true });
  if (dir > 0 && (!best || i >= items.length - 8)) loadMore();
}
function onKey(ev) {
  if (!isActive('events') || overlayOpen(ev) || ev.ctrlKey || ev.metaKey || ev.altKey) return;
  const t = ev.target, tag = t && t.tagName;
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (t && t.isContentEditable)) return;
  if ($('smartResults') && !$('smartResults').hidden) return;
  const onBtn = tag === 'BUTTON' || tag === 'A' || tag === 'SUMMARY';
  const e = byId.get(selId);
  const k = ev.key;
  if (V === 'grid' && (k === 'ArrowDown' || k === 'ArrowUp' || k === 'ArrowLeft' || k === 'ArrowRight')) { gridStep(k); ev.preventDefault(); }
  else if (k === 'ArrowDown' || k === 'j' || k === 'J') { step(1); ev.preventDefault(); }
  else if (k === 'ArrowUp' || k === 'k' || k === 'K') { step(-1); ev.preventDefault(); }
  else if (k === 'Enter' && !onBtn && e) { openOnTimeline(e); ev.preventDefault(); }
  else if ((k === 'p' || k === 'P') && e) { toggleProtect(e); ev.preventDefault(); }
  else if ((k === 'x' || k === 'X') && e) { toggleReview(e, 'false_alarm'); ev.preventDefault(); }
  else if ((k === 'o' || k === 'O') && e) { toggleReview(e, 'ok'); ev.preventDefault(); }
  else if (k === ' ' && !onBtn && curVideo) { if (curVideo.paused) curVideo.play().catch(() => {}); else curVideo.pause(); ev.preventDefault(); }
  else if (k === 'Escape' && panelOpen) { setPanel(false); ev.preventDefault(); }
  else if (k === 'Escape') { if (checks.size) { checks.clear(); lastRender = ''; render(); } }
}

// ---------- bulk ----------
function tick(id, on, range) {
  if (range && anchorId && anchorId !== id) {
    const a = items.findIndex(e => e.id === anchorId), b = items.findIndex(e => e.id === id);
    if (a >= 0 && b >= 0) for (let i = Math.min(a, b); i <= Math.max(a, b); i++) checks.add(items[i].id);
  } else if (on) checks.add(id); else checks.delete(id);
  anchorId = id; lastRender = ''; render();
}
function paintBulk() {
  const b = $('evBulk'); if (!b) return;
  b.hidden = !checks.size;
  if (!checks.size) return;
  b.innerHTML = '<b>' + checks.size + ' selected</b>' + (can('events') ? '<button type="button" class="btn" data-b="p">' + ico('lock') + ' Keep selected forever</button>' : '') +
    (can('events') ? '<button type="button" class="btn" data-b="fa">' + ico('x') + ' Not a real alert</button>' : '') + '<button type="button" class="btn" data-b="x">' + ico('dl') + ' Export selected</button><button type="button" class="btn" data-b="c">Clear</button>';
  b.querySelector('[data-b=c]').onclick = () => { checks.clear(); lastRender = ''; render(); };
  // One ZIP of the selected events (clips + events.csv); a big selection comes as several.
  b.querySelector('[data-b=x]').onclick = async () => {
    const ids = [...checks];
    const BATCH = 100;
    for (let i = 0; i < ids.length; i += BATCH) {
      downloadUrl('/api/export?ids=' + ids.slice(i, i + BATCH).map(enc).join(','));
      if (i + BATCH < ids.length) await new Promise(r => setTimeout(r, 1500));
    }
    toast('Preparing ' + Math.ceil(ids.length / BATCH) + ' ZIP of ' + ids.length + ' event' + (ids.length === 1 ? '' : 's'), { kind: 'ok', ms: 3000 });
  };
  const fa = b.querySelector('[data-b=fa]');
  if (fa) fa.onclick = async () => {
    fa.disabled = true; let n = 0; const total = checks.size;
    for (const id of [...checks]) if (await setReview(id, 'false_alarm', true)) { n++; if (F.hfa) dropItem(id); }
    toast('Marked ' + n + ' of ' + total + ' as not a real alert', { kind: n ? 'ok' : 'err' });
    checks.clear(); lastRender = ''; render(); const e = byId.get(selId); if (e) paintActions(e);
  };
  const p = b.querySelector('[data-b=p]');
  if (p) p.onclick = async () => {
    p.disabled = true; let n = 0; const total = checks.size;
    for (const id of [...checks]) if (await setProtect(id, true, true)) n++;
    toast('Kept ' + n + ' of ' + total + ' forever', { kind: n ? 'ok' : 'err' });
    checks.clear(); lastRender = ''; render(); const e = byId.get(selId); if (e) paintActions(e);
  };
}

// ---------- hover preview: the frames the AI kept ----------
function wireHover(el, e) {
  let t = 0, iv = 0, orig = '';
  const stop = () => { clearTimeout(t); clearInterval(iv); const im = el.querySelector('.evt'); if (im && orig) { im.src = orig; orig = ''; } };
  el.addEventListener('mouseenter', () => {
    if (!e.thumb || !window.matchMedia('(hover: hover)').matches) return;
    t = setTimeout(async () => {
      const ns = await aiFrames(e.id);
      const im = el.querySelector('.evt');
      if (ns.length < 2 || !im || !el.matches(':hover')) return;
      orig = im.getAttribute('src'); let i = 0;
      // (Stops by itself when the row is gone or out of sight without a mouseleave.)
      const show = () => { if (!el.isConnected || !el.matches(':hover') || !isActive('events')) { stop(); return; } im.src = '/api/events/' + enc(e.id) + '/ai/' + ns[i++ % ns.length]; };
      show(); iv = setInterval(show, 600);
    }, 300);
  });
  el.addEventListener('mouseleave', stop);
}

// ---------- chips, popovers ----------
let pop = null;
function closePop() { if (pop) { pop.remove(); pop = null; } }
function openPop(btn, html, wire) {
  const was = pop && pop.__btn === btn;
  closePop(); if (was) return;
  const d = document.createElement('div');
  d.className = 'evpop'; d.innerHTML = html; d.__btn = btn;
  document.body.appendChild(d);
  const r = btn.getBoundingClientRect();
  d.style.top = Math.min(r.bottom + 6, window.innerHeight - d.offsetHeight - 8) + 'px';
  d.style.left = Math.max(8, Math.min(r.left, window.innerWidth - d.offsetWidth - 8)) + 'px';
  pop = d; wire(d);
}
document.addEventListener('mousedown', (ev) => { if (pop && !pop.contains(ev.target) && !pop.__btn.contains(ev.target)) closePop(); });
document.addEventListener('keydown', (ev) => { if (ev.key === 'Escape' && pop) { const b = pop.__btn; closePop(); b.focus(); ev.stopPropagation(); } }, true);

function viewChanged() {
  paintChips(); lastRender = ''; render();
  const el = (V === 'grid' ? tileEls : rowEls).get(selId); if (el) el.scrollIntoView({ block: 'nearest' });
  sizeSplit();
}
function setView(v) {
  if (v === V) return;
  V = v; F.ev = selId; writeHash(); viewChanged();
  if (rawMore && items.length < pageSize()) loadMore();
}
function filtersChanged() { gen++; started = true; writeHash(); paintChips(); loadFirst().catch(() => {}); if ($('fSmart').value.trim()) runSmart(); }

function paintChips() {
  const cb = $('fCam');
  if (!cb) return;
  const label = !F.cams.length ? 'All cameras' : F.cams.length === 1 ? F.cams[0] : F.cams.length + ' cameras';
  cb.querySelector('span').textContent = label;
  cb.classList.toggle('on', F.cams.length > 0);
  document.querySelectorAll('#evChips [data-obj]').forEach(b => { const on = F.objs.has(b.dataset.obj); b.classList.toggle('on', on); b.setAttribute('aria-pressed', on); });
  document.querySelectorAll('#evChips [data-range]:not([data-range=all])').forEach(b => { const on = F.range === b.dataset.range; b.classList.toggle('on', on); b.setAttribute('aria-pressed', on); });
  const cu = $('evCustom').querySelector('span');
  const f = (s) => new Date(s * 1000).toLocaleDateString([], { month: 'short', day: 'numeric' });
  cu.textContent = F.range === 'custom' ? (F.from ? f(F.from) : '...') + ' – ' + (F.to ? f(F.to) : 'now') : 'Custom';
  $('evHideFA').checked = F.hfa;
  $('evProt').classList.toggle('on', F.prot); $('evProt').setAttribute('aria-pressed', F.prot);
  document.querySelectorAll('#evView [data-view]').forEach(b => { const on = V === b.dataset.view; b.classList.toggle('on', on); b.setAttribute('aria-pressed', on); });
  const al = document.querySelector('#evChips [data-range=all]'); if (al) { al.classList.toggle('on', !F.range); al.setAttribute('aria-pressed', !F.range); }
  const split = $('eventTable'); if (split) split.classList.toggle('grid', V === 'grid');
  const note = $('evAiOff'); if (note) note.hidden = aiOff !== true;
  paintSaved();
  $('clearFilter').hidden = !(F.cams.length || F.objs.size || F.range || F.prot || F.hfa || F.all || $('fSmart').value);
}
const toLocal = (s) => { if (!s) return ''; const d = new Date(s * 1000); return d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()) + 'T' + pad(d.getHours()) + ':' + pad(d.getMinutes()); };

function camPopup(btn) {
  const names = state.cams.map(c => c.name);
  openPop(btn, names.map((n, i) => '<label><input type="checkbox" data-i="' + i + '"' + (F.cams.includes(n) ? ' checked' : '') + '> ' + esc(n) + '</label>').join('') +
    '<div class="pr"><button type="button" class="btn" data-all>All cameras</button></div>', (d) => {
    d.querySelectorAll('[data-i]').forEach(c => { c.onchange = () => { F.cams = names.filter((n, i) => d.querySelector('[data-i="' + i + '"]').checked); F.ev = ''; filtersChanged(); }; });
    d.querySelector('[data-all]').onclick = () => { F.cams = []; F.ev = ''; closePop(); filtersChanged(); };
    const f = d.querySelector('input'); if (f) f.focus();
  });
}
function customPopup(btn) {
  openPop(btn, '<label class="sub" style="display:block">From<input type="datetime-local" id="evFrom" value="' + toLocal(F.from) + '"></label>' +
    '<label class="sub" style="display:block">To (blank: now)<input type="datetime-local" id="evTo" value="' + toLocal(F.to) + '"></label>' +
    '<div class="pr"><button type="button" class="btn primary" data-go>Apply</button></div>', (d) => {
    d.querySelector('[data-go]').onclick = () => {
      const a = d.querySelector('#evFrom').value, b = d.querySelector('#evTo').value;
      F.from = a ? Math.floor(new Date(a).getTime() / 1000) : 0; F.to = b ? Math.floor(new Date(b).getTime() / 1000) : 0;
      if (F.to && F.from && F.to < F.from) { toast('The end is before the start', { kind: 'err' }); return; }
      F.range = (F.from || F.to) ? 'custom' : ''; F.ev = ''; closePop(); filtersChanged();
    };
  });
}
function morePopup(btn) {
  openPop(btn, '<label title="Also show motion AI checked and found nothing that counts in, on cameras set to show only events AI confirms"><input type="checkbox" data-m="all"' + (F.all ? ' checked' : '') + '> Show motion AI did not confirm too</label><button type="button" class="mi" data-m="save">Save these filters...</button><button type="button" class="mi" data-m="zip">Export ZIP of these events</button><button type="button" class="mi" data-m="rd">Mark all shown as looked at</button>', (d) => {
    d.querySelector('[data-m=all]').onchange = (ev) => { F.all = ev.target.checked; F.ev = ''; closePop(); filtersChanged(); };
    d.querySelector('[data-m=save]').onclick = () => { closePop(); saveFiltersPopup(btn); };
    d.querySelector('[data-m=zip]').onclick = async () => {
      closePop();
      if (!needsClient()) { window.location.href = '/api/export?zip=1' + buildFilterQuery(); return; }
      // The server cannot apply this filter, so export exactly the matching events loaded in the list.
      const ids = items.map(e => e.id), BATCH = 200;
      if (!ids.length) { toast('No events to export', { kind: 'err' }); return; }
      for (let i = 0; i < ids.length; i += BATCH) {
        downloadUrl('/api/export?ids=' + ids.slice(i, i + BATCH).map(enc).join(','));
        if (i + BATCH < ids.length) await new Promise(r => setTimeout(r, 1500));
      }
    };
    d.querySelector('[data-m=rd]').onclick = () => { closePop(); items.forEach(e => reviewed.add(e.id)); saveReviewed(); lastRender = ''; render(); };
  });
}

// ---------- smart search ("red truck", "person with a yellow hat") ----------

let smartOn = false, smartGen = 0;
async function checkSmart() {
  try { const st = await api('/api/smart/status'); smartOn = st.enabled && st.loaded; } catch (e) { smartOn = false; }
  $('fSmartL').hidden = !smartOn;
  if (!smartOn) showSmart(null);
}

function showSmart(html) {
  const box = $('smartResults');
  box.hidden = html === null;
  $('eventTable').hidden = html !== null;
  if (html !== null) box.innerHTML = html;
  paintChips();
}

const fmtAt = (ms) => new Date(ms).toLocaleString(undefined, { weekday: 'short', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit', second: '2-digit' });

async function runSmart() {
  const q = $('fSmart').value.trim();
  const gen2 = ++smartGen;
  if (!q) { showSmart(null); sizeSplit(); return; }
  let p = '&camera=' + enc(F.cams.length === 1 ? F.cams[0] : '');
  const [a, b] = rangeBounds();
  if (a) p += '&since=' + a;
  if (b) p += '&until=' + b;
  showSmart('<p class="empty">Searching...</p>');
  let r;
  try { r = await api('/api/smart/search?q=' + enc(q) + p); } catch (e) {
    if (gen2 !== smartGen) return;
    let msg = e.message; try { msg = JSON.parse(e.body).error || msg; } catch (x) { /* plain text */ }
    showSmart('<p class="empty">' + esc(msg) + '</p>'); return;
  }
  if (gen2 !== smartGen) return;
  const res = r.results.filter(x => F.cams.length < 2 || F.cams.includes(x.camera));
  if (!res.length) { showSmart('<p class="empty">Nothing found for "' + esc(q) + '". Try other words, more cameras or more days. <button class="btn" data-smclose>Back to the list</button></p>'); wireSmartClose(); return; }
  const best = res[0].score;
  const h = '<div class="smq-head"><b>' + res.length + '</b> event' + (res.length === 1 ? '' : 's') + ' that look most like "' + esc(q) + '", best first <button class="btn" data-smclose>Back to the list</button></div><div class="smq-grid">' +
    res.map((x, i) => {
      const bx = x.box;
      // How close to the best match (the first is 100%).
      const rel = Math.max(0, Math.round(100 - (best - x.score) * 1000));
      return '<button class="smq-card" data-i="' + i + '" title="Open ' + esc(x.camera) + ' at this moment"><span class="smq-pic"><img loading="lazy" src="/api/smart/pic/' + x.pic + '" alt="">' +
        (bx ? '<span class="smq-box" style="left:' + (bx[0] * 100) + '%;top:' + (bx[1] * 100) + '%;width:' + (bx[2] * 100) + '%;height:' + (bx[3] * 100) + '%"></span>' : '') + '</span>' +
        '<span class="smq-meta"><b>' + esc(x.camera) + '</b><small>' + esc(fmtAt(x.at_ms)) + ' · ' + rel + '%</small></span></button>';
    }).join('') + '</div>';
  showSmart(h);
  wireSmartClose();
  $('smartResults').querySelectorAll('[data-i]').forEach(el => { el.onclick = () => {
    const x = res[+el.dataset.i];
    const cam = state.cams.find(c => c.name === x.camera);
    if (!cam) { toast(x.camera + ' is no longer a camera here'); return; }
    if (cam.remote) { toast(x.camera + ': its recordings are on ' + ((cam.remote && cam.remote.site) || 'the other server')); return; }
    showTab('live');
    import('./livetl.js').then(m => m.openAt(cam.id, x.at_ms));
  }; });
}
function wireSmartClose() {
  const c = $('smartResults').querySelector('[data-smclose]');
  if (c) c.onclick = () => { $('fSmart').value = ''; runSmart(); };
}

// ---------- start ----------
function sizeSplit() {
  const sp = $('eventTable');
  if (!sp || sp.hidden || !isActive('events')) return;
  const top = sp.getBoundingClientRect().top + window.scrollY;
  sp.style.height = Math.max(isPhone() ? 340 : 420, window.innerHeight - top - 12) + 'px';
}

export function initEvents() {
  document.addEventListener('keydown', onKey);
  window.addEventListener('resize', sizeSplit);
  backLayer(40, () => panelOpen, () => setPanel(false));
  $('fCam').onclick = () => camPopup($('fCam'));
  document.querySelectorAll('#evChips [data-obj]').forEach(b => { b.onclick = () => { const o = b.dataset.obj; if (F.objs.has(o)) F.objs.delete(o); else F.objs.add(o); F.ev = ''; filtersChanged(); }; });
  document.querySelectorAll('#evChips [data-range]').forEach(b => { b.onclick = () => {
    const r = b.dataset.range;
    if (r === 'all') { F.range = ''; F.ev = ''; closePop(); filtersChanged(); return; }
    if (r === 'custom') { customPopup(b); return; }
    F.range = F.range === r ? '' : r; F.ev = ''; closePop(); filtersChanged();
  }; });
  $('evHideFA').onchange = () => { F.hfa = $('evHideFA').checked; F.ev = ''; filtersChanged(); };
  document.querySelectorAll('#evView [data-view]').forEach(b => { b.onclick = () => setView(b.dataset.view); });
  wireSwipe();
  $('evProt').onclick = () => { F.prot = !F.prot; F.ev = ''; filtersChanged(); };
  $('clearFilter').onclick = () => {
    F.cams = []; F.objs = new Set(); F.range = ''; F.from = F.to = 0; F.prot = false; F.hfa = false; F.all = false; F.ev = '';
    $('fSmart').value = ''; showSmart(null); filtersChanged();
  };
  $('evMore').onclick = () => morePopup($('evMore'));
  $('evLoadMore').onclick = () => loadMore();
  // The first page of events not answered for a while: say so, with a Retry.
  stillWaiting($('evList'), () => { loadFirst().catch(() => {}); });
  // Scrolling near the end of the list pages in more.
  if ('IntersectionObserver' in window) {
    moreIO = new IntersectionObserver((en) => { if (en.some(x => x.isIntersecting)) loadMore(); }, { root: $('evList'), rootMargin: '500px' });
    moreIO.observe($('evMoreRow'));
  } else $('evList').addEventListener('scroll', () => { const s = $('evList'); if (s.scrollHeight - s.scrollTop - s.clientHeight < 400) loadMore(); }, { passive: true });
  $('fSmart').onkeydown = (e) => { if (e.key === 'Enter') { e.preventDefault(); runSmart(); } e.stopPropagation(); };
  $('fSmart').addEventListener('search', () => { if (!$('fSmart').value.trim()) runSmart(); });
  window.addEventListener('hashchange', () => {
    if (!isActive('events')) return;
    const g = parseHash();
    const cur = hashOf();
    if (location.hash === cur) return;
    const viewBefore = V;
    applyParsed(g);
    // (The event id is last in the address, after "?" when it is the only part.)
    const noView = (h) => h.replace(/[?&]view=grid/, '').replace(/[?&]ev=[^&]*$/, '');
    if (noView(hashOf()) !== noView(cur)) {
      // An event named in the new address is opened (or said to be gone), never the newest in its place.
      if (F.ev) { deepId = F.ev; missingEv = ''; }
      paintChips(); filtersChanged(); return;
    }
    if (V !== viewBefore) viewChanged();
    if (F.ev && byId.has(F.ev)) selectEvent(F.ev, { scroll: true });
    else if (F.ev) { deepId = F.ev; missingEv = ''; autoSelect(); }
  });
}

export const showEvents = () => {
  shownOnce = true;
  // A link with filters in it applies them; a bare #events keeps the last ones.
  if (location.hash.includes('?')) {
    const before = hashOf(), evBefore = F.ev;
    applyParsed(parseHash());
    if (F.ev && F.ev !== evBefore) { deepId = F.ev; missingEv = ''; }
    if (started && hashOf() !== before) { started = false; }
  }
  writeHash(); paintChips();
  checkSmart();
  requestAnimationFrame(sizeSplit);
  if (!started) { started = true; place = null; return loadFirst().catch(() => {}).then(startClip); }
  sizeSplit();
  return refreshEvents().catch(() => {}).then(() => { render(); autoSelect(); restorePlace(); startClip(); probeAi(true); });
};
// On coming to Events: the selected clip starts if it has not been played yet.
function startClip() {
  const v = curVideo;
  if (!v || !v.isConnected || !v.paused || v.currentTime > 0.1 || !isActive('events') || (isPhone() && !panelOpen)) return;
  const p = v.play(); if (p && p.catch) p.catch(() => {});
}
// Go to the Grid view of Events filtered to one camera (from the dock or the full view).
export function openClipsFor(cam) {
  // (The full view's history step is taken back first, so it does not undo the page change.)
  afterOverlayClose(closeFull).then(() => { location.hash = '#events?view=grid&cam=' + enc(enc(cam.name)); });
}
export const eventsActive = () => isActive('events');
