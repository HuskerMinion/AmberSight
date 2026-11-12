// fullscreen.js - Live "Full screen": the camera grid (all, a group, a layout) or one camera fills the
// screen with no header, toolbar or tab bar. The whole page goes into the browser's Fullscreen API, so
// dialogs, menus and toasts (children of <body>) still show, and the Live area fills it with CSS (.tv);
// where the API is missing (iPhone Safari) the same view fills the window instead.
// With the timeline on, the cameras fill the screen above its controls; the timeline keeps saying which
// cameras show.
// Esc: whatever is open on top first (dialog, menu), then one camera -> back to the grid -> leave; on the
// timeline it leaves full screen and stays on the timeline. A small exit button shows on pointer movement
// or a tap and fades after 3 s.
import { bus, overlayOpen } from './core.js';
import { isActive } from './nav.js';

let on = false;
let one = null;        // camera id shown alone, or null for the grid
let fake = false;      // CSS "fill the window" instead of the Fullscreen API
let hideT = null;
let onChange = () => {};
let bar = null;
let escFirst = null;   // something of the timeline's that Esc closes before anything else

export const tvOn = () => on;
export const tvOne = () => one;
export function onTvChange(fn) { onChange = fn; }
export function setEscFirst(fn) { escFirst = fn; }

const host = () => document.querySelector('#tab-live .livemain');
const realEl = () => document.fullscreenElement || document.webkitFullscreenElement || null;
const canReal = (el) => !!(el && (el.requestFullscreen || el.webkitRequestFullscreen));

function showBar() {
  if (!bar) return;
  bar.classList.add('show');
  clearTimeout(hideT);
  hideT = setTimeout(() => bar.classList.remove('show'), 3000);
}

function buildBar(h) {
  bar = document.createElement('div');
  bar.className = 'tvbar';
  bar.innerHTML = '<button type="button" class="btn glass tvback" hidden>Back to all cameras</button>' +
    '<button type="button" class="btn glass tvexit" title="Leave full screen (Esc)" aria-label="Leave full screen">Exit full screen</button>';
  bar.querySelector('.tvback').onclick = (ev) => { ev.stopPropagation(); setOne(null); };
  bar.querySelector('.tvexit').onclick = (ev) => { ev.stopPropagation(); exitTv(); };
  h.appendChild(bar);
}

function paint() {
  const h = host();
  if (h) h.classList.toggle('tv', on);
  document.body.classList.toggle('tvmode', on);
  if (h) h.classList.toggle('one', on && one !== null);
  if (bar) bar.querySelector('.tvback').hidden = one === null;
  const tb = document.getElementById('tlFs');
  if (tb) tb.setAttribute('aria-pressed', String(on));
}
function sync() { paint(); onChange(); }

export function setOne(id) { one = id; sync(); showBar(); }
// The same without drawing again: the timeline does that itself as it opens or closes.
export function setOneQuiet(id) { if (on) { one = id; paint(); } }

// `back`: what leaving full screen goes back to (the opened camera it was started from).
let back = null;
export async function enterTv(id, backTo) {
  const h = host();
  if (!h || on) { if (on && id !== undefined) setOne(id); return; }
  on = true; one = id === undefined ? null : id; fake = false; back = backTo || null;
  if (!bar) buildBar(h);
  sync();
  showBar();
  const page = document.documentElement;
  if (canReal(page)) {
    try {
      await (page.requestFullscreen ? page.requestFullscreen({ navigationUI: 'hide' }) : page.webkitRequestFullscreen());
      if (navigator.keyboard && navigator.keyboard.lock) navigator.keyboard.lock(['Escape']).catch(() => {});
      return;
    } catch (e) { /* refused: fill the window instead */ }
  }
  fake = true;
}

export function exitTv() {
  if (!on) return;
  on = false; one = null;
  clearTimeout(hideT);
  if (bar) bar.classList.remove('show');
  if (navigator.keyboard && navigator.keyboard.unlock) { try { navigator.keyboard.unlock(); } catch (e) { /* none */ } }
  if (realEl()) { try { (document.exitFullscreen || document.webkitExitFullscreen).call(document); } catch (e) { /* gone */ } }
  fake = false;
  sync();
  const b = back; back = null;
  if (b) b();
}

export function toggleTv() { if (on) exitTv(); else enterTv(); }

// Esc: what is open on top closes first (it hears Esc itself), then one camera -> the grid -> leave.
// (With the keyboard locked the page gets Esc; otherwise the browser leaves full screen itself and
// fullscreenchange tidies up.)
window.addEventListener('keydown', (ev) => {
  if (!on || ev.key !== 'Escape') return;
  const lb = document.getElementById('lightbox');
  if (overlayOpen(ev) || document.querySelector('.ctxmenu') || (lb && lb.style.display === 'flex')) return;
  ev.preventDefault(); ev.stopPropagation();
  if (escFirst && escFirst(ev)) return;
  // (Started from an opened camera: Esc goes back to it, not to the whole grid.)
  if (one !== null && !back) setOne(null); else exitTv();
}, true);

function leftByBrowser() {
  if (on && !fake && !realEl()) { on = false; one = null; clearTimeout(hideT); if (bar) bar.classList.remove('show'); sync(); const b = back; back = null; if (b) b(); }
}
document.addEventListener('fullscreenchange', leftByBrowser);
document.addEventListener('webkitfullscreenchange', leftByBrowser);
for (const t of ['mousemove', 'pointerdown', 'touchstart']) document.addEventListener(t, () => { if (on) showBar(); }, { passive: true });
bus.on('signedout', exitTv);
window.addEventListener('hashchange', () => setTimeout(() => { if (on && !isActive('live')) exitTv(); }, 60));
