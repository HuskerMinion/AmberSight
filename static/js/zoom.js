// zoom.js - digital zoom for a camera picture (the full-size view, the
// timeline's one-camera view). The scroll wheel zooms in and out
// at the pointer, dragging pans while zoomed, two fingers pinch on touch, double-click
// zooms in at a point (or back out), + - 0 from the keyboard. Purely a view transform:
// nothing is sent to the camera (PTZ cameras keep their own controls in the dock).

const MAX = 8;

// Zoom for the pictures in `host` (all its video/img/canvas move together,
// so players that swap between several videos keep the view). `opts.pan()`:
// false while something else owns the pointer; `opts.on()`: false where the
// wheel should scroll the page instead.
export function createZoom(host, opts) {
  opts = opts || {};
  const canPan = opts.pan || (() => true);
  const on = opts.on || (() => true);
  let s = 1, tx = 0, ty = 0;  // scale and translation (px) of the picture, origin top-left
  let badge = null;
  const all = () => [...host.querySelectorAll('video, img, canvas')];
  const media = () => all().find(m => !m.hidden && m.offsetWidth > 0) || null;
  // (`opts.box`: the whole box counts as the picture, for players whose
  // videos let the pointer through.)
  const isPic = (t) => !!t && host.contains(t) && (opts.box ? !(t.closest && t.closest('button')) : !!(t.closest && t.closest('video, img, canvas')));

  function apply() {
    const m = media();
    if (s <= 1.001) { s = 1; tx = 0; ty = 0; }
    if (m) {
      // Keep the picture covering its box: no empty edges while zoomed.
      tx = Math.min(0, Math.max(m.offsetWidth * (1 - s), tx));
      ty = Math.min(0, Math.max(m.offsetHeight * (1 - s), ty));
    }
    for (const e of all()) {
      e.style.transformOrigin = '0 0';
      e.style.transform = s === 1 ? '' : 'translate(' + tx + 'px,' + ty + 'px) scale(' + s + ')';
    }
    host.classList.toggle('zoomed', s > 1);
    if (s > 1) {
      if (!badge || !host.contains(badge)) {
        badge = document.createElement('button');
        badge.type = 'button';
        badge.className = 'btn glass zoombadge';
        badge.title = 'Zoomed in: click, double-click the picture or press 0 to reset';
        badge.addEventListener('pointerdown', (e) => e.stopPropagation());
        badge.addEventListener('click', (e) => { e.stopPropagation(); reset(); });
        host.appendChild(badge);
      }
      badge.textContent = s.toFixed(1) + '×';
    } else if (badge) { badge.remove(); badge = null; }
  }

  // Zoom by factor f keeping the picture point under client (cx, cy) still.
  function zoomAt(cx, cy, f) {
    const m = media();
    if (!m) return;
    const r = m.getBoundingClientRect();
    const ns = Math.min(MAX, Math.max(1, s * f));
    const px = cx - (r.left - tx), py = cy - (r.top - ty); // point in the untransformed box
    tx = px - (px - tx) * ns / s;
    ty = py - (py - ty) * ns / s;
    s = ns;
    apply();
  }

  function center() {
    const r = media() ? media().getBoundingClientRect() : null;
    return r ? [r.left + r.width / 2, r.top + r.height / 2] : [0, 0];
  }
  const zoomBy = (f) => { const [x, y] = center(); zoomAt(x, y, f); };
  const reset = () => { s = 1; apply(); };

  host.addEventListener('wheel', (ev) => {
    if (!on() || !media() || !isPic(ev.target)) return;
    ev.preventDefault();
    const dy = ev.deltaMode === 1 ? ev.deltaY * 33 : ev.deltaMode === 2 ? ev.deltaY * 400 : ev.deltaY;
    zoomAt(ev.clientX, ev.clientY, Math.exp(-Math.max(-300, Math.min(300, dy)) * 0.002));
  }, { passive: false });
  host.addEventListener('dblclick', (ev) => {
    if (!on() || !isPic(ev.target)) return;
    ev.preventDefault();
    ev.stopPropagation();
    if (s > 1) reset(); else zoomAt(ev.clientX, ev.clientY, 2.5);
  });
  // Pointer drag pans (mouse: when zoomed; touch: one finger pans, two pinch).
  const pts = new Map();
  host.addEventListener('pointerdown', (ev) => {
    if (!on() || !isPic(ev.target) || !canPan()) return;
    if (ev.pointerType === 'mouse' && (ev.button !== 0 || s === 1)) return;
    pts.set(ev.pointerId, { x: ev.clientX, y: ev.clientY, moved: false });
    if (ev.pointerType === 'mouse' || pts.size === 2 || s > 1) {
      try { host.setPointerCapture(ev.pointerId); } catch (e) { /* pointer already gone */ }
    }
    if (ev.pointerType === 'mouse') { host.classList.add('panning'); ev.preventDefault(); }
  });
  host.addEventListener('pointermove', (ev) => {
    const p = pts.get(ev.pointerId);
    if (!p) return;
    const prev = [...pts.values()];
    pts.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
    if (pts.size >= 2) {
      const [a, b] = [...pts.values()];
      const [a0, b0] = prev;
      const d = Math.hypot(a.x - b.x, a.y - b.y), d0 = Math.hypot(a0.x - b0.x, a0.y - b0.y);
      const mx = (a.x + b.x) / 2, my = (a.y + b.y) / 2;
      const mx0 = (a0.x + b0.x) / 2, my0 = (a0.y + b0.y) / 2;
      if (d0 > 0) zoomAt(mx0, my0, d / d0);
      tx += mx - mx0; ty += my - my0;
      apply();
    } else if (s > 1) {
      tx += ev.clientX - p.x; ty += ev.clientY - p.y;
      apply();
    }
  });
  const up = (ev) => {
    pts.delete(ev.pointerId);
    if (!pts.size) host.classList.remove('panning');
  };
  host.addEventListener('pointerup', up);
  host.addEventListener('pointercancel', up);
  window.addEventListener('resize', () => apply());
  return { reset, zoomBy, isZoomed: () => s > 1, refresh: apply };
}

// The full-size view's zoom (live.js).
let full = null;
export const isZoomed = () => !!full && full.isZoomed();
export function zoomBy(f) { if (full) full.zoomBy(f); }
export function reset() { if (full) full.reset(); }
export function initZoom(el, panAllowed) { full = createZoom(el, { pan: panAllowed }); }
