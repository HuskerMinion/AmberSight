// preview.js - Picture previews while hovering or dragging along a timeline:
// a small bubble with the recording at that moment (the keyframe before it,
// from /api/preview) and the time. One bubble for the page; requests are
// debounced, one at a time, and rounded to the second so the browser's cache
// answers repeats.

import { enc, fmtWall } from './core.js';

const WIDTH = 240;
const DEBOUNCE_MS = 90;

let el = null, img = null, label = null, cv = null;
let want = null;       // { cam, ms, x, y }
let busy = false, timer = 0, shownKey = '';

function ensure() {
  if (el) return;
  el = document.createElement('div');
  el.className = 'tlprev';
  el.setAttribute('aria-hidden', 'true');
  el.innerHTML = '<img alt=""><canvas hidden></canvas><div></div>';
  img = el.querySelector('img');
  cv = el.querySelector('canvas');
  label = el.querySelector('div');
  el.hidden = true;
  document.body.appendChild(el);
}

// Above (x, y), growing upwards: never over the timeline under it.
function place(x, y) {
  const w = el.offsetWidth || WIDTH;
  el.style.left = Math.max(4, Math.min(window.innerWidth - w - 4, x - w / 2)) + 'px';
  el.style.top = 'auto';
  el.style.bottom = (window.innerHeight - y + 12) + 'px';
  el.style.maxHeight = Math.max(40, y - 16) + 'px';
}

async function fetchNext() {
  if (busy || !want) return;
  const w = want;
  const sec = Math.floor(w.ms / 1000) * 1000;
  const key = w.cam + '|' + sec;
  if (key === shownKey) return;
  busy = true;
  const url = '/api/preview?camera=' + enc(w.cam) + '&at=' + sec + '&w=' + WIDTH;
  try {
    const r = await fetch(url, { credentials: 'same-origin' });
    if (want && r.ok) {
      const blob = await r.blob();
      if (want) {
        const old = img.src;
        img.src = URL.createObjectURL(blob);
        if (old && old.startsWith('blob:')) URL.revokeObjectURL(old);
        img.hidden = false;
      }
    } else if (want) {
      img.hidden = true;
    }
    shownKey = key;
  } catch (e) { /* offline: keep what is shown */ }
  busy = false;
  if (want && want !== w) fetchNext();
  else if (want) place(want.x, want.y);
}

// Show a picture already at hand (a filmstrip cell `f`: {img, sx, sy, w, h})
// at once, without asking the server.
export function showPreviewFrame(cam, ms, x, y, f) {
  ensure();
  want = null;
  clearTimeout(timer);
  if (cv.width !== f.w || cv.height !== f.h) { cv.width = f.w; cv.height = f.h; }
  cv.getContext('2d').drawImage(f.img, f.sx, f.sy, f.w, f.h, 0, 0, f.w, f.h);
  cv.hidden = false;
  img.hidden = true;
  shownKey = '';
  label.textContent = cam + ' · ' + fmtWall(ms);
  el.hidden = false;
  place(x, y);
}

// Show the preview of camera `cam` at `ms`, the bubble above (x, y).
export function showPreview(cam, ms, x, y) {
  if (!cam) return hidePreview();
  ensure();
  cv.hidden = true;
  want = { cam, ms, x, y };
  label.textContent = cam + ' · ' + fmtWall(ms);
  if (el.hidden) { el.hidden = false; img.hidden = true; shownKey = ''; }
  place(x, y);
  clearTimeout(timer);
  timer = setTimeout(fetchNext, DEBOUNCE_MS);
}

export function hidePreview() {
  want = null;
  clearTimeout(timer);
  if (el) el.hidden = true;
}
