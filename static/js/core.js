// core.js - shared plumbing: API helper, event bus, session state, dialogs, toast, small utils.
// Every other module imports from here; this module imports only split.js (which imports nothing).
import { paneFilter } from './split.js';

export const state = {
  signedIn: false,
  user: '',
  isAdmin: true,        // refined by /api/whoami
  role: 'admin',        // 'viewer' | 'operator' | 'admin' (from /api/whoami)
  perms: null,          // an operator's rights: { ptz, talk_clip, arm, events, camera_detection }
  authRequired: false,  // false = open-LAN mode: no sign-in UI
  cams: [],             // last /api/cameras result
  overlayVars: {},      // named overlay variables (name -> value) from /api/cameras
  speed: 1,             // playback speed for <video> elements
  toastsOn: false,
  leaving: false,       // signed out to move to another address (HTTPS turned off): no sign-in gate
};

// ---------- tiny event bus ----------
const handlers = {};
export const bus = {
  on(evt, fn) { (handlers[evt] = handlers[evt] || []).push(fn); },
  off(evt, fn) { handlers[evt] = (handlers[evt] || []).filter(f => f !== fn); },
  emit(evt, arg) { for (const fn of (handlers[evt] || []).slice()) { try { fn(arg); } catch (e) { console.error(evt, e); } } },
};

// ---------- API ----------
export class ApiError extends Error {
  constructor(status, message, body) { super(message); this.status = status; this.body = body || ''; }
}

// The web interface this page was loaded with. A server answering with another
// one was updated while the page stayed open: the page still runs the old
// code (new menus and buttons are missing), so it offers a reload, once.
const PAGE_UI = (document.querySelector('meta[name="ambersight-ui"]') || {}).content || '';
let uiNoticed = false;
function checkUi(r) {
  const ui = r.headers.get('x-ambersight-ui');
  if (!PAGE_UI || !ui || ui === PAGE_UI || uiNoticed) return;
  uiNoticed = true;
  bus.emit('ui-updated');
}

// JSON helper. The session cookie rides along automatically (same origin).
// Pass {json: obj} to POST/PUT a JSON body.
export async function api(url, opts) {
  const o = Object.assign({ credentials: 'same-origin' }, opts || {});
  if (o.json !== undefined) {
    o.method = o.method || 'POST';
    o.headers = Object.assign({ 'Content-Type': 'application/json' }, o.headers || {});
    o.body = JSON.stringify(o.json);
    delete o.json;
  }
  const r = await fetch(url, o);
  checkUi(r);
  if (r.status === 401) {
    bus.emit('unauth');
    throw new ApiError(401, url + ' -> 401 (sign in)');
  }
  if (!r.ok) {
    let body = '';
    try { body = (await r.text()).trim(); } catch (e) { /* ignore */ }
    // A sensitive change wants the caller's own password: the same dialog everywhere,
    // and the same request again with it.
    if (r.status === 403 && body.startsWith('this change needs your own password') && !(opts && opts.proofSent)) {
      const { confirmWithPassword } = await import('./account.js');
      return new Promise((res, rej) => confirmWithPassword({
        reason: 'This change is a sensitive one, so it takes your own password.', ok: 'Confirm',
        onCancel: () => rej(new ApiError(403, 'Canceled', 'Canceled')),
        call: async (proof) => {
          res(await api(url, Object.assign({}, opts, { method: o.method, json: Object.assign({}, opts && opts.json, proof), proofSent: true })));
        },
      }));
    }
    if (r.status === 403) throw new ApiError(403, url + ' -> 403 (' + (body || 'this account is view-only') + ')', body);
    throw new ApiError(r.status, url + ' -> ' + r.status + (body && body.length < 200 ? ': ' + body : ''), body);
  }
  const txt = await r.text();
  if (!txt) return {};
  try { return JSON.parse(txt); } catch (e) { return {}; }
}

// A page part still saying "Loading..." (a p.empty placeholder) after `ms`:
// the server is busy or out of reach, so it says so, with a Retry that calls
// `retry`. Watches `root` (the page's own element, also when it is drawn
// again meanwhile) until it leaves the page.
const WAIT_TEXT = 'Still waiting for the server…';
export function stillWaiting(root, retry, ms = 10000) {
  let since = 0, seen = false;
  const iv = setInterval(() => {
    if (!root.isConnected) { if (seen) clearInterval(iv); return; }
    seen = true;
    const ps = [...root.querySelectorAll('p.empty')];
    const loading = ps.filter(p => /^Loading/.test(p.textContent));
    if (!loading.length && !ps.some(p => p.dataset.waiting)) { since = 0; return; }
    if (!since) since = Date.now();
    if (Date.now() - since < ms) return;
    for (const p of loading) {
      p.dataset.waiting = '1';
      p.innerHTML = esc(WAIT_TEXT) + ' <button type="button" class="btn sm">Retry</button>';
      p.querySelector('button').onclick = () => {
        since = Date.now();
        root.querySelectorAll('p.empty[data-waiting]').forEach(x => { x.textContent = 'Loading...'; delete x.dataset.waiting; });
        retry();
      };
    }
  }, 1000);
  return () => clearInterval(iv);
}

// Fetch the camera list into state.cams and tell everybody.
export async function refreshCameras() {
  const d = await api('/api/cameras');
  // A reply without a list (cut off, unreadable) keeps the cameras there are:
  // an empty list would blank the live view and the timeline until the next one.
  if (!Array.isArray(d.cameras)) return state.cams;
  // Named overlay variables (weather, temperatures, anything an automation pushes).
  state.overlayVars = (d.overlay_vars && typeof d.overlay_vars === 'object') ? d.overlay_vars : {};
  // Every camera (incl. other servers'); a split-view pane shows only its server's.
  state.allCams = d.cameras;
  state.cams = paneFilter(state.allCams);
  bus.emit('cameras', state.cams);
  return state.cams;
}

// ---------- data saver (per device) ----------
// 'on', 'off', or absent = auto: on when the browser says the connection is
// slow or the user asked it to save data. With it on, full-size live view
// uses the sub stream and playback prefers the sub stream's recordings.
const DS_KEY = 'ambs.dataSaver';
export function dataSaverSetting() { try { return localStorage.getItem(DS_KEY) || 'auto'; } catch (e) { return 'auto'; } }
export function setDataSaver(v) { try { if (v === 'auto') localStorage.removeItem(DS_KEY); else localStorage.setItem(DS_KEY, v); } catch (e) { /* not remembered */ } }
export function dataSaver() {
  const v = dataSaverSetting();
  if (v !== 'auto') return v === 'on';
  const c = navigator.connection;
  return !!(c && (c.saveData || /(^|-)2g$|3g/.test(c.effectiveType || '')));
}
// Query string for playback/clip URLs: '&quality=low' (or '?quality=low').
export const lowQ = (first) => (dataSaver() ? (first ? '?' : '&') + 'quality=low' : '');
// Stream for big live views.
export const bigQ = () => (dataSaver() ? 'sub' : 'main');

// ---------- utils ----------
export const $ = (id) => document.getElementById(id);
export const enc = encodeURIComponent;
export const sleep = (ms) => new Promise(r => setTimeout(r, ms));
export const pad2 = (n) => String(n).padStart(2, '0');
export function esc(x) {
  return String(x == null ? '' : x).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}
export function fmtTime(ts) { return new Date(ts * 1000).toLocaleString(); }
export function fmtSize(b) {
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0, v = b;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return (i === 0 ? v : v.toFixed(1)) + ' ' + u[i];
}
export function toLocalInput(ms) {
  const d = new Date(ms);
  return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate()) + 'T' + pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
}
export function fmtWall(ms) {
  const d = new Date(ms);
  return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate()) + ' ' + pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
}
export function camFromId(id) { return state.cams.find(c => String(c.id) === String(id)) || null; }

// ---------- toast ----------
// toast(text, onClick?, opts?)  or  toast(text, opts)
//   opts: { kind: 'info' (default) | 'ok' | 'err', ms: ms before it goes (info/ok 6000; err stays until
//   dismissed), action: { label, run } adds a button (e.g. Undo) }.
// Toasts stack top right (full width on phones); hovering pauses the timer.
function toastBox() {
  let c = document.getElementById('toasts');
  if (!c) {
    c = document.createElement('div');
    c.id = 'toasts';
    c.className = 'toasts';
    c.setAttribute('role', 'status');
    c.setAttribute('aria-live', 'polite');
    document.body.appendChild(c);
  }
  return c;
}
// ---------- video start ----------
// Some browsers (Samsung Internet with its autoplay or power-saving settings,
// Firefox with "Block audio and video") refuse even muted video until the
// person touches the page. play() then rejects with NotAllowedError: the
// video is remembered, a "Tap to start video" button shows, and the first
// tap or key anywhere starts every remembered one.
const blockedVids = new Set();
let playNote = null;
function startBlocked() {
  const list = [...blockedVids];
  blockedVids.clear();
  if (playNote) { playNote.remove(); playNote = null; }
  // (Through playVideo: refused again, the video is listed and the button shown again.)
  for (const v of list) if (v.isConnected && v.paused) { v.muted = true; playVideo(v); }
}
export function playVideo(v) {
  let p;
  try { p = v.play(); } catch (e) { return; }
  if (!p || !p.catch) return;
  p.catch((e) => {
    if (!e || e.name !== 'NotAllowedError' || !v.isConnected) return;
    blockedVids.add(v);
    if (playNote) return;
    playNote = document.createElement('button');
    playNote.type = 'button'; playNote.className = 'playnote';
    playNote.innerHTML = '<svg class="ic"><use href="#i-play"/></svg><span>Tap to start video</span>';
    playNote.onclick = startBlocked;
    document.body.appendChild(playNote);
    for (const ev of ['pointerdown', 'keydown', 'touchend']) document.addEventListener(ev, startBlocked, { once: true, capture: true });
  });
}

// What this account may do beyond watching: admins everything, operators what their rights say
// (the server checks again; this only decides which buttons show). The server gives what this
// session may do now: 'arm' (arming and profiles) is off for an operator limited to some cameras,
// who may still 'pause' notifications of their cameras.
export function can(perm) {
  if (state.isAdmin) return true;
  return state.role === 'operator' && !!(state.perms && state.perms[perm]);
}
export const isOperator = () => !state.isAdmin && state.role === 'operator';

// A view-only account tried something only an admin can do (a key such as S, P, X): say so once
// per page session, quietly, instead of a dialog every time.
let viewOnlyTold = false;
export function viewOnlyNote(what) {
  // (Arming by an operator limited to some cameras: said every time, it is not a missing right.)
  if (what === 'arm' && isOperator() && state.limited && state.perms && state.perms.pause) { toast('This account sees only some cameras, so it cannot arm, disarm or switch profiles: those change every camera.'); return; }
  if (viewOnlyTold) return;
  viewOnlyTold = true;
  toast(isOperator() ? 'This account may not do that: an admin can allow it in Settings, Users.' : 'This account is view-only: an admin can give it more rights in Settings, Users.');
}

export function toast(msg, onClick, opts) {
  if (onClick && typeof onClick === 'object') { opts = onClick; onClick = null; }
  opts = opts || {};
  const kind = opts.kind === 'err' || opts.kind === 'ok' ? opts.kind : 'info';
  const t = document.createElement('div');
  t.className = 'toast ' + kind;
  t.setAttribute('role', kind === 'err' ? 'alert' : 'status');
  const m = document.createElement('span');
  m.className = 'tmsg';
  m.textContent = msg;
  t.appendChild(m);
  const gone = () => { clearTimeout(timer); t.remove(); };
  if (onClick) { t.classList.add('click'); t.title = 'Open this camera'; t.onclick = () => { gone(); onClick(); }; }
  if (opts.action) {
    const a = document.createElement('button');
    a.type = 'button'; a.className = 'tact'; a.textContent = opts.action.label;
    a.onclick = (ev) => { ev.stopPropagation(); gone(); opts.action.run(); };
    t.appendChild(a);
  }
  if (kind === 'err' || opts.dismiss) {
    const x = document.createElement('button');
    x.type = 'button'; x.className = 'tx'; x.setAttribute('aria-label', 'Dismiss'); x.title = 'Dismiss'; x.innerHTML = '&times;';
    x.onclick = (ev) => { ev.stopPropagation(); gone(); };
    t.appendChild(x);
  }
  toastBox().appendChild(t);
  let timer = 0, left = opts.ms || 6000;
  if (kind !== 'err' || opts.ms) {
    let started = 0;
    const run = () => { started = Date.now(); timer = setTimeout(gone, left); };
    t.addEventListener('mouseenter', () => { clearTimeout(timer); left -= Date.now() - started; });
    t.addEventListener('mouseleave', () => { left = Math.max(left, 1500); run(); });
    run();
  }
  return t;
}

// ---------- the Back gesture / button closes what is open ----------
// While something is open over the page (a dialog, a camera's full view, the
// timeline) the browser history holds one extra step, so Back (the phone's
// swipe from the edge) closes the topmost of them instead of leaving the
// page. Modules register what they open, highest priority first closed, and
// call overlaysChanged() when it opens or closes.
const backLayers = [];
let backPending = false;
export function backLayer(prio, isOpen, close) {
  backLayers.push({ prio, isOpen, close });
  backLayers.sort((a, b) => b.prio - a.prio);
}
// An overlay opened from another page can say where Back goes: returnOnBack('#events?...') just before
// opening it. The first Back then closes everything that is open and returns to that page in one press.
let pendingRet = null, overlayRet = null, closingAll = false;
export function returnOnBack(hash) { pendingRet = { hash, at: Date.now() }; }
export function overlaysChanged() {
  if (closingAll) return;
  const open = backLayers.some(l => l.isOpen());
  const ours = !!(history.state && history.state.ambsOverlay);
  if (open && !ours) {
    overlayRet = pendingRet && Date.now() - pendingRet.at < 5000 ? pendingRet.hash : null;
    pendingRet = null;
    history.pushState({ ambsOverlay: 1 }, '', location.href);
  } else if (!open && ours && !backPending) { overlayRet = null; backPending = true; history.back(); }
  else if (!open) overlayRet = null;
}
window.addEventListener('popstate', () => {
  if (backPending) { backPending = false; overlaysChanged(); return; }
  const ret = overlayRet;
  if (ret && !(history.state && history.state.ambsOverlay)) {
    overlayRet = null;
    // A dialog or panel over it closes by itself first; the next Back goes back.
    const top = backLayers.find(x => x.isOpen());
    if (top && top.prio >= 25) { pendingRet = { hash: ret, at: Date.now() }; top.close(); overlaysChanged(); return; }
    // Close everything (a layer may need two closes, e.g. a one-camera timeline), then step back to the page it came from.
    closingAll = true;
    try { for (let i = 0; i < 12; i++) { const l = backLayers.find(x => x.isOpen()); if (!l) break; l.close(); } } catch (e) { /* closed what it could */ }
    closingAll = false;
    if (location.hash !== ret) history.back();
    return;
  }
  const l = backLayers.find(x => x.isOpen());
  if (l) l.close();
  overlaysChanged();
});
// Close an overlay, then wait until the history step it leaves behind has been taken back, so a page
// change made next is not undone by it: `await afterOverlayClose(closeMore); goTab('help')`.
export function afterOverlayClose(closeFn) {
  if (closeFn) closeFn();
  if (!backPending) return Promise.resolve();
  return new Promise((r) => {
    const t = setTimeout(r, 400);
    window.addEventListener('popstate', () => { clearTimeout(t); r(); }, { once: true });
  });
}

// ---------- phone sheets (the More sheet, the Settings section picker) ----------
// trapSheet(el, panel) makes a sheet modal: the page behind is inert and Tab stays inside `panel`
// (default `el`). Returns release(refocus = true): call it when the sheet closes (it gives the focus back).
const ABOVE = (e) => e.id === 'dlgScrim' || e.id === 'toasts' || e.id === 'loginGate' || e.id === 'moreSheet' ||
  (e.classList && (e.classList.contains('cp-scrim') || e.classList.contains('sp-picker') || e.classList.contains('dlg'))) || /^(SCRIPT|STYLE|LINK|SVG)$/i.test(e.tagName);
export function trapSheet(el, panel) {
  panel = panel || el;
  const opener = document.activeElement;
  const marked = [];
  for (const e of document.body.children) {
    if (e === el || ABOVE(e) || e.hasAttribute('inert')) continue;
    e.setAttribute('inert', ''); marked.push(e);
  }
  const onKey = (ev) => {
    if (ev.key !== 'Tab' || dlgStack.length || paletteOpen()) return;
    const f = [...panel.querySelectorAll(FOCUSABLE)].filter(x => x.offsetParent !== null);
    if (!f.length) { ev.preventDefault(); return; }
    const a = document.activeElement;
    if (ev.shiftKey && (a === f[0] || !panel.contains(a))) { ev.preventDefault(); f[f.length - 1].focus(); }
    else if (!ev.shiftKey && (a === f[f.length - 1] || !panel.contains(a))) { ev.preventDefault(); f[0].focus(); }
  };
  document.addEventListener('keydown', onKey, true);
  return (refocus = true) => {
    document.removeEventListener('keydown', onKey, true);
    for (const e of marked) e.removeAttribute('inert');
    if (refocus && opener && opener.isConnected && typeof opener.focus === 'function') { try { opener.focus({ preventScroll: true }); } catch (e) { /* gone */ } }
  };
}

// ---------- dialogs ----------
let dlgStack = [];
export function topDlgOpen() { return dlgStack.length > 0; }
// True while anything modal-ish sits over the page: a dialog, the zones editor, the
// camera side panel or the command palette. Page-level shortcuts must stay quiet then.
// The side panel can stay open beside the page, so it only counts for keys typed
// inside it (pass the keydown event).
export function overlayOpen(ev) {
  const t = ev && ev.target;
  return dlgStack.length > 0 || !!document.querySelector('.ze, .cp-scrim:not([hidden])') ||
    !!(t && t.closest && t.closest('.cs-drawer'));
}
export function paletteOpen() { return !!document.querySelector('.cp-scrim:not([hidden])'); }

// openDlg(title, html, small) -> element. `title` is plain text; `html` is trusted markup
// (escape user data with esc()). d.__close() closes it.
// Bring an already-open dialog back to the front (true) instead of opening a
// second copy of it.
export function raiseDlg(d) {
  if (!d || !d.isConnected) return false;
  const i = dlgStack.indexOf(d);
  if (i >= 0 && i < dlgStack.length - 1) { dlgStack.push(...dlgStack.splice(i, 1)); syncDlgLayers(); } else if (!d.isConnected) document.body.appendChild(d);
  const f = d.querySelector('input, select, textarea, button:not([aria-label="close"])');
  if (f) f.focus();
  return true;
}

const FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]):not([type=hidden]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';
let dlgSeq = 0;
let gateSync = null;   // set by the sign-in gate below
// While a dialog is open the whole page behind it is inert (not only the other dialogs): Tab, clicks and
// screen readers stay inside. What this sets is marked, and removed again when the last dialog closes.
function inertBehind() {
  if (!dlgStack.length) {
    document.querySelectorAll('[data-dlg-inert]').forEach((e) => { e.removeAttribute('inert'); e.removeAttribute('data-dlg-inert'); });
    if (gateSync) gateSync();   // the sign-in gate may still be up: it keeps the page behind it inert
    return;
  }
  for (const e of document.body.children) {
    // (The command palette, the More sheet and the section picker can open over a dialog: never inert.)
    if (dlgStack.includes(e) || e.id === 'dlgScrim' || e.id === 'toasts' || e.id === 'loginGate' || e.id === 'moreSheet' ||
        (e.classList && (e.classList.contains('cp-scrim') || e.classList.contains('sp-picker'))) ||
        /^(SCRIPT|STYLE|LINK|SVG)$/i.test(e.tagName) || e.hasAttribute('inert')) continue;
    e.setAttribute('inert', ''); e.setAttribute('data-dlg-inert', '');
  }
}
// One scrim behind the top dialog; a nested dialog sits above the first, which stays dimmed.
function syncDlgLayers() {
  let sc = document.getElementById('dlgScrim');
  if (!dlgStack.length) { if (sc) sc.remove(); inertBehind(); return; }
  if (!sc) { sc = document.createElement('div'); sc.id = 'dlgScrim'; sc.className = 'dlgscrim'; sc.setAttribute('aria-hidden', 'true'); }
  dlgStack.forEach((d, i) => { d.style.zIndex = String(90 + i * 2); d.toggleAttribute('inert', i < dlgStack.length - 1); });
  inertBehind();
  const top = dlgStack[dlgStack.length - 1];
  sc.style.zIndex = String(89 + (dlgStack.length - 1) * 2);
  // Only touch the DOM when the order is wrong: re-appending a node resets the scroll
  // position and focus of whatever is inside it. z-index already stacks them.
  if (!sc.isConnected) document.body.appendChild(sc);
  if (!top.isConnected) document.body.appendChild(top);
}

export function openDlg(title, html, small) {
  // One copy of a dialog: opening it again replaces the open one.
  for (const o of dlgStack.slice()) if (o.__title === title) o.__close();
  const d = document.createElement('div');
  d.className = 'dlg' + (small ? ' small' : '');
  d.__title = title;
  d.__opener = document.activeElement;
  const tid = 'dlgTitle' + (++dlgSeq);
  d.setAttribute('role', 'dialog');
  d.setAttribute('aria-modal', 'true');
  d.setAttribute('aria-labelledby', tid);
  // Always in the middle of the window, so there is one place to look.
  d.style.left = '50%';
  d.style.top = '50%';
  d.style.transform = 'translate(-50%, -50%)';
  d.innerHTML = '<div class="bar"><span class="ttl" id="' + tid + '">' + esc(title) + '</span><button type="button" title="Close (Esc)" aria-label="close"><svg class="ic"><use href="#i-x"/></svg></button></div><div class="body">' + html + '</div>';
  document.body.appendChild(d);
  dlgStack.push(d);
  syncDlgLayers();
  const close = () => {
    if (!d.isConnected && !dlgStack.includes(d)) return;
    const wasTop = dlgStack[dlgStack.length - 1] === d;
    d.remove(); dlgStack = dlgStack.filter(x => x !== d);
    syncDlgLayers();
    overlaysChanged();
    const op = d.__opener;
    if (wasTop && op && op.isConnected && typeof op.focus === 'function' && !document.querySelector('.dlg input:focus, .dlg textarea:focus')) { try { op.focus({ preventScroll: true }); } catch (e) { /* gone */ } }
  };
  // (Through d.__close, so a dialog that overrides it - confirmDialog, promptDialog - hears the X too.)
  d.querySelector('.bar button').onclick = () => d.__close();
  d.__close = close;
  d.tabIndex = -1;
  try { d.focus({ preventScroll: true }); } catch (e) { /* ignore */ }
  overlaysChanged();
  return d;
}
export function msgDlg(text) {
  const d = openDlg('AmberSight', '<div>' + esc(text) + '</div>', true);
  setTimeout(() => { if (d.parentNode) d.__close(); }, 6000);
}
// Tabbed dialogs: give every pane the height of the tallest, so switching tabs does
// not resize (and re-center) the window. Capped by the dialog's own max height.
export function equalizePanes(d, sel) {
  const panes = [...d.querySelectorAll(sel)];
  const shown = panes.map(p => p.hidden);
  let h = 0;
  for (const p of panes) { p.hidden = false; p.style.minHeight = ''; }
  for (const p of panes) { p.hidden = true; }
  for (const p of panes) { p.hidden = false; h = Math.max(h, p.offsetHeight); p.hidden = true; }
  panes.forEach((p, i) => { p.hidden = shown[i]; p.style.minHeight = h + 'px'; });
}
// Dialogs close before any sheet or panel under them (Events' phone sheet 40, More 45); only the
// command palette, which can open over a dialog, is above them.
backLayer(48, () => dlgStack.length > 0, () => dlgStack[dlgStack.length - 1].__close());
export function closeAllDialogs() { for (const d of dlgStack.slice()) d.__close(); }

window.addEventListener('keydown', (ev) => {
  if (!dlgStack.length || paletteOpen()) return;
  const top = dlgStack[dlgStack.length - 1];
  if (ev.key === 'Escape') {
    ev.stopImmediatePropagation();
    top.__close();
  } else if (ev.key === 'Tab') {
    // Focus stays inside the top dialog.
    const f = [...top.querySelectorAll(FOCUSABLE)].filter(e => e.offsetParent !== null || e === document.activeElement);
    if (!f.length) { ev.preventDefault(); return; }
    const first = f[0], last = f[f.length - 1], a = document.activeElement;
    if (!top.contains(a)) { ev.preventDefault(); first.focus(); }
    else if (ev.shiftKey && a === first) { ev.preventDefault(); last.focus(); }
    else if (!ev.shiftKey && a === last) { ev.preventDefault(); first.focus(); }
  }
}, true);

// The sign-in gate is modal too: the page behind it is inert and Tab stays in the form.
{
  const gate = document.getElementById('loginGate');
  if (gate) {
    gate.setAttribute('aria-modal', 'true');
    const sync = () => {
      const on = gate.classList.contains('on');
      for (const e of document.body.children) {
        if (e === gate || /^(SCRIPT|STYLE|LINK|SVG)$/i.test(e.tagName)) continue;
        if (on && !e.hasAttribute('inert')) { e.setAttribute('inert', ''); e.setAttribute('data-gate-inert', ''); }
        else if (!on && e.hasAttribute('data-gate-inert')) { e.removeAttribute('inert'); e.removeAttribute('data-gate-inert'); }
      }
    };
    new MutationObserver(sync).observe(gate, { attributes: true, attributeFilter: ['class'] });
    sync();
    gateSync = sync;
    gate.addEventListener('keydown', (ev) => {
      if (ev.key !== 'Tab') return;
      const f = [...gate.querySelectorAll(FOCUSABLE)].filter(e => e.offsetParent !== null);
      if (!f.length) { ev.preventDefault(); return; }
      const a = document.activeElement;
      if (ev.shiftKey && (a === f[0] || !gate.contains(a))) { ev.preventDefault(); f[f.length - 1].focus(); }
      else if (!ev.shiftKey && (a === f[f.length - 1] || !gate.contains(a))) { ev.preventDefault(); f[0].focus(); }
    });
  }
}

// ---------- icons (inline SVG sprite lives in index.html) ----------
export function ic(name) { return '<svg class="ic"><use href="#i-' + name + '"/></svg>'; }

// ---------- theme: Auto (follow the system), Light or Dark ----------
// Saved as 'light' / 'dark' in localStorage; no entry = Auto (prepaint.js applies it before first paint).
export function themeMode() {
  const t = document.documentElement.dataset.theme;
  return t === 'light' || t === 'dark' ? t : 'auto';
}
// The theme in effect now ('light' or 'dark'), whatever the mode.
export function currentTheme() {
  const t = themeMode();
  if (t !== 'auto') return t;
  return window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
}
// t: 'auto' | 'light' | 'dark'
export function setTheme(t) {
  if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
  else { t = 'auto'; delete document.documentElement.dataset.theme; }
  try { if (t === 'auto') localStorage.removeItem('ambs.theme'); else localStorage.setItem('ambs.theme', t); } catch (e) { /* private mode */ }
  const eff = currentTheme();
  const u = document.getElementById('themeUse');
  if (u) u.setAttribute('href', t === 'auto' ? '#i-auto' : t === 'light' ? '#i-sun' : '#i-moon');   // the icon shows the mode now
  const b = document.getElementById('themeBtn');
  if (b) b.title = 'Theme: ' + (t === 'auto' ? 'Auto (follows this device)' : t === 'light' ? 'Light' : 'Dark') + '. Click to change.';
  const m = document.querySelector('meta[name=theme-color]');
  if (m) m.content = eff === 'light' ? '#ffffff' : '#14181e';
  for (const x of document.querySelectorAll('.themeseg [data-mode]')) x.setAttribute('aria-pressed', String(x.dataset.mode === t));
}
// Light <-> dark (what a plain "switch theme" command means); the header button cycles Auto/Light/Dark.
export function toggleTheme() { setTheme(currentTheme() === 'light' ? 'dark' : 'light'); }
export function cycleTheme() { setTheme({ auto: 'light', light: 'dark', dark: 'auto' }[themeMode()]); }
try { window.matchMedia('(prefers-color-scheme: light)').addEventListener('change', () => { if (themeMode() === 'auto') setTheme('auto'); }); } catch (e) { /* old browser */ }

// ---------- plain-URL download (the session cookie authenticates it) ----------
export function downloadUrl(url, fname) {
  const a = document.createElement('a');
  a.href = url;
  if (fname) a.download = fname;
  document.body.appendChild(a); a.click(); a.remove();
}

// ---------- playback speed ----------
// Speeds offered for recorded video. Browsers play at most 16x; faster speeds are
// emulated where a player supports it (the playback timeline skips ahead).
export const SPEEDS = [0.125, 0.25, 0.5, 1, 2, 4, 6, 8, 16, 32, 64, 128, 256];
export const MAX_RATE = 16;
// Backwards (the timeline): -1, -2, -4.
export const REVERSE_SPEEDS = [-4, -2, -1];
export const speedLabel = (x) => (x < 0 ? '◀ Reverse ' + (-x) + 'x' : x === 1 ? 'Normal (1x)' : (x < 1 ? String(x) : x) + 'x');
// <option>s for a speed <select>; `max` drops speeds a player cannot do;
// `reverse` adds the backwards speeds first.
export function speedOptions(cur, max, reverse) {
  return (Array.isArray(reverse) ? reverse : reverse ? REVERSE_SPEEDS : []).concat(SPEEDS.filter(x => !max || x <= max))
    .map(x => '<option value="' + x + '"' + (x === cur ? ' selected' : '') + '>' + speedLabel(x) + '</option>').join('');
}
export const safeRate = (x) => Math.min(MAX_RATE, Math.max(0.0625, x || 1));
export function applySpeed() {
  const r = safeRate(state.speed);
  for (const v of document.querySelectorAll('video')) { try { v.playbackRate = r; v.defaultPlaybackRate = r; } catch (e) { /* unsupported rate */ } }
}

// ====================================================================================
// One save model for Settings (settings agent, UI overhaul 2)
//   - on/off settings are .sw switches that save when flipped: liveSwitch() + savedToast()
//   - text / number / select fields share one sticky bar "Unsaved changes - Revert - Save":
//     formBar() watches the fields by itself, wireBar() takes your own dirty() (draft pages)
//   - leaving a page (route change, another tab, closing the window) with unsaved fields asks:
//     confirmLeave(); registerUnsaved() is called for you by the two functions above
// ====================================================================================

// Message of a failed api() call: the server's { error } when there is one.
export function errText(e) {
  if (e && e.body) { try { return JSON.parse(e.body).error || e.body; } catch (x) { return e.body; } }
  return (e && e.message) || String(e);
}

export function savedToast(text = 'Saved') { return toast(text, { kind: 'ok', ms: 2200 }); }

// confirmDialog({ title, body, ok, cancel, danger }) -> Promise<boolean>. A small centred dialog
// that names the consequence; Esc, the close button and Back all mean "cancel". The page behind it
// is inert while it is open.
export function confirmDialog({ title, body = '', ok = 'OK', cancel = 'Cancel', danger = false, safeFocus = false }) {
  return new Promise((resolve) => {
    const d = openDlg(title, (body ? '<p class="cfm-body">' + esc(body) + '</p>' : '') +
      '<div class="row cfm-row"><button type="button" class="btn" data-cfm-no>' + esc(cancel) + '</button>' +
      '<button type="button" class="primary' + (danger ? ' danger' : '') + '" data-cfm-yes>' + esc(ok) + '</button></div>', true);
    let done = false;
    const close = d.__close;
    const finish = async (v) => {
      if (done) return;
      done = true;
      // Closing steps the history back one entry (the dialog's own): wait for that, so a page change
      // the caller makes next is not undone by it.
      await afterOverlayClose(close);
      resolve(v);
    };
    d.__close = () => finish(false);
    d.querySelector('[data-cfm-yes]').onclick = () => finish(true);
    d.querySelector('[data-cfm-no]').onclick = () => finish(false);
    // The safe choice has the focus: Cancel for something destructive (or when asked), OK otherwise.
    try { d.querySelector(danger || safeFocus ? '[data-cfm-no]' : '[data-cfm-yes]').focus(); } catch (e) { /* gone */ }
  });
}

// promptDialog({ title, label, value, ok, placeholder }) -> Promise<string|null>. Same dialog as
// confirmDialog with one text field (focused and selected; Enter = OK). null = cancelled.
export function promptDialog({ title, label = '', value = '', ok = 'OK', cancel = 'Cancel', placeholder = '' }) {
  return new Promise((resolve) => {
    const d = openDlg(title, '<label class="fld pmt-l"><span class="fld-l">' + esc(label) + '</span><input type="text" data-pmt value="' + esc(value) + '" placeholder="' + esc(placeholder) + '" autocomplete="off" spellcheck="false"></label>' +
      '<div class="row cfm-row"><button type="button" class="btn" data-cfm-no>' + esc(cancel) + '</button><button type="button" class="primary" data-cfm-yes>' + esc(ok) + '</button></div>', true);
    const input = d.querySelector('[data-pmt]');
    let done = false;
    const close = d.__close;
    const finish = async (v) => {
      if (done) return;
      done = true;
      await afterOverlayClose(close);
      resolve(v);
    };
    d.__close = () => finish(null);
    d.querySelector('[data-cfm-yes]').onclick = () => finish(input.value);
    d.querySelector('[data-cfm-no]').onclick = () => finish(null);
    input.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); finish(input.value); } });
    try { input.focus(); input.select(); } catch (e) { /* gone */ }
  });
}

// ---------- unsaved changes ----------
const unsaved = new Set();
export function registerUnsaved(el, fn) {
  for (const u of unsaved) if (!u.el.isConnected) unsaved.delete(u);
  const u = { el, fn };
  unsaved.add(u);
  return () => unsaved.delete(u);
}
export function hasUnsaved() {
  for (const u of [...unsaved]) {
    if (!u.el.isConnected) { unsaved.delete(u); continue; }
    try { if (u.fn()) return true; } catch (e) { /* ignore */ }
  }
  return false;
}
// clearUnsaved() forgets everything; clearUnsaved(root) only the bars inside `root` (a page that
// leaves must not forget another tab's unsaved fields).
export function clearUnsaved(root) {
  if (!root) { unsaved.clear(); return; }
  for (const u of [...unsaved]) if (root.contains(u.el) || !u.el.isConnected) unsaved.delete(u);
}
// true = nothing to lose, or the person chose to leave.
export async function confirmLeave() {
  if (!hasUnsaved()) return true;
  const ok = await confirmDialog({ title: 'Leave without saving?', body: 'You changed settings on this page and have not saved them. If you leave now, the changes are lost.', ok: 'Leave', cancel: 'Keep editing', danger: true });
  if (ok) clearUnsaved();
  return ok;
}
window.addEventListener('beforeunload', (e) => { if (hasUnsaved()) { e.preventDefault(); e.returnValue = ''; } });

// ---------- on/off settings: save when flipped ----------
// save(on) -> Promise (resolve false = it showed its own message). Failure puts the switch back and
// shows the error; throw { cancelled: true } to put it back quietly (the person said no).
export function liveSwitch(input, save) {
  input.setAttribute('data-live', '');
  input.addEventListener('change', async () => {
    const on = input.checked;
    input.disabled = true;
    try { const r = await save(on); if (r !== false) savedToast(); }
    catch (e) { input.checked = !on; if (!(e && e.cancelled)) toast(errText(e), { kind: 'err' }); }
    input.disabled = false;
  });
}
// <label class="sw"> markup. attrs: extra attributes on the input (e.g. 'data-en').
export function swHtml(attrs, on, title, small, disabled) {
  return '<label class="sw' + (disabled ? ' off' : '') + '"><input type="checkbox" role="switch" ' + attrs + (on ? ' checked' : '') + (disabled ? ' disabled' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
    '<span class="sw-text"><b>' + title + '</b>' + (small ? '<small>' + small + '</small>' : '') + '</span></label>';
}
// One field: label above, number/text input with its unit after it on the same line, hint below.
//   nf('Keep recordings', 'data-k="days" min="1"', 'days', 'hint text', 'number')
export function fld(label, attrs, unit, hint, type = 'number') {
  return '<label class="fld"><span class="fld-l">' + label + '</span><span class="fld-in"><input type="' + type + '" ' + (attrs || '') + '>' +
    (unit ? '<span class="unit">' + unit + '</span>' : '') + '</span></label>' + (hint ? '<div class="hint fld-h">' + hint + '</div>' : '');
}

// ---------- the sticky save bar ----------
export function pageBar() {
  return '<div class="ac-bar" hidden role="region" aria-label="Unsaved changes"><span class="ac-state" data-bar-state role="status">Unsaved changes</span><span class="ac-sp"></span>' +
    '<button type="button" class="btn" data-bar-revert>Revert</button><button type="button" class="primary" data-bar-save>Save</button></div>';
}
// Wire the bar inside `el`: dirty() says whether there is something to save; save() may return
// false to say it showed its own message. Returns update() - call it after every edit.
export function wireBar(el, dirty, save, revert) {
  const st = el.querySelector('[data-bar-state]'), sv = el.querySelector('[data-bar-save]'), rv = el.querySelector('[data-bar-revert]');
  const bar = el.querySelector('.ac-bar');
  registerUnsaved(bar, dirty);
  let busy = false;
  const update = () => {
    const d = dirty();
    st.textContent = 'Unsaved changes';
    bar.hidden = !d && !busy;
    bar.classList.toggle('dirty', d);
    sv.disabled = !d || busy; rv.disabled = !d || busy;
  };
  sv.onclick = async () => {
    busy = true; sv.disabled = true; sv.textContent = 'Saving...';
    try { const r = await save(); if (r !== false) savedToast(); }
    catch (e) { toast(errText(e), { kind: 'err' }); }
    busy = false; sv.textContent = 'Save'; update();
  };
  rv.onclick = () => { revert(); update(); };
  update();
  return update;
}

// ---------- fields that are watched by themselves ----------
const WATCH = 'input, select, textarea, [aria-pressed]';
const watched = (host) => [...host.querySelectorAll(WATCH)].filter(e => !e.closest('.ac-bar') && !e.hasAttribute('data-live') && !e.hasAttribute('data-nt') && !(e.tagName === 'INPUT' && ['button', 'submit', 'reset', 'file', 'hidden'].includes(e.type)));
const readVal = (e) => (e.hasAttribute('aria-pressed') ? e.getAttribute('aria-pressed') : (e.type === 'checkbox' || e.type === 'radio') ? e.checked : e.value);
const stateOf = (host) => JSON.stringify(watched(host).map(readVal));
// formBar(host, { save, revert }) puts the bar at the end of `host` and watches every text, number,
// select and checkbox field in it (mark a field with data-live = saves itself, data-nt = ignore).
// Chips with aria-pressed count too. After a successful save the current values are the new baseline.
export function formBar(host, { save, revert } = {}) {
  if (host.__fb) { host.__fb.stop(); }
  host.insertAdjacentHTML('beforeend', pageBar());
  let base = watched(host).map(e => [e, readVal(e)]);
  let baseStr = stateOf(host);
  const restore = () => {
    for (const [e, v] of base) {
      if (!e.isConnected) continue;
      if (e.hasAttribute('aria-pressed')) { if (e.getAttribute('aria-pressed') !== v) e.click(); continue; }
      if (e.type === 'checkbox' || e.type === 'radio') e.checked = v; else e.value = v;
      e.dispatchEvent(new Event('input', { bubbles: true })); e.dispatchEvent(new Event('change', { bubbles: true }));
    }
  };
  const upd = wireBar(host, () => stateOf(host) !== baseStr, async () => {
    const r = await save();
    base = watched(host).map(e => [e, readVal(e)]); baseStr = stateOf(host);
    return r;
  }, () => { if (revert) revert(); else restore(); });
  const poke = () => setTimeout(upd, 0);
  host.addEventListener('input', poke); host.addEventListener('change', poke); host.addEventListener('click', poke);
  host.__fb = { stop() { host.removeEventListener('input', poke); host.removeEventListener('change', poke); host.removeEventListener('click', poke); }, update: upd };
  return upd;
}
