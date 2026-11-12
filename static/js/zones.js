// zones.js - the motion zone editor: a large overlay with the camera's picture, where
// detect zones, ignore zones (rectangles or polygons, 0..1 of the frame) and privacy masks
// (rectangles only) are drawn, selected, moved, resized, reshaped, deleted, undone, and saved together.
// Named alert zones live here too: areas (polygons; loitering alerts) and lines (2+ points with an arrow;
// crossing alerts), each with a name and options (objects, seconds or direction, alert, record, window).
// Opened from the camera panel's Motion section and from the Live dock.

import { topDlgOpen, state, api, esc, toast, camFromId, refreshCameras, msgDlg, backLayer, overlaysChanged } from './core.js';

const MIN = 0.01;
const MIN_AREA = 0.0005;    // smallest polygon area (fraction of the frame)           // smallest side of a zone (fraction of the frame)
const NAMES = ['nw', 'n', 'ne', 'e', 'se', 's', 'sw', 'w'];
const CURSOR = { nw: 'nwse-resize', se: 'nwse-resize', ne: 'nesw-resize', sw: 'nesw-resize', n: 'ns-resize', s: 'ns-resize', e: 'ew-resize', w: 'ew-resize' };
let open = null;            // the editor that is showing
let askNow = null;          // asks to leave the editor that is showing (Esc, the browser's Back)
// Below dialogs (30 in core.js): a dialog opened over the editor closes first on Back.
backLayer(28, () => !!open && !!askNow, () => { if (askNow) askNow(); });

// Counts rectangles and polygons, and mentions privacy masks. Takes a camera object.
export function zoneCounts(c) {
  c = c || {};
  return { zn: (c.zones || []).length + (c.zone_polys || []).length, iz: (c.ignore_zones || []).length + (c.ignore_polys || []).length, pm: (c.privacy_masks || []).length, nz: (c.named_zones || []).length };
}
export function zonesSummary(c) {
  const { zn, iz, pm, nz } = zoneCounts(c), parts = [];
  if (zn || iz) parts.push(zn + ' zone' + (zn === 1 ? '' : 's'));
  if (iz) parts.push(iz + ' ignore');
  if (pm) parts.push(pm + ' privacy mask' + (pm === 1 ? '' : 's'));
  if (nz) parts.push(nz + ' alert zone' + (nz === 1 ? '' : 's'));
  return 'Zones: ' + (parts.length ? parts.join(' \u00b7 ') : 'none (the whole picture counts)') + '.';
}

function ensureCss() {
  if (document.getElementById('cameraPanelCss')) return;
  const l = document.createElement('link');
  l.id = 'cameraPanelCss'; l.rel = 'stylesheet'; l.href = '/static/css/camera-panel.css';
  document.head.appendChild(l);
}

const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const r4 = (v) => Math.round(v * 10000) / 10000;
const pc = (v) => (Math.round(v * 1000) / 10).toString();

export async function openZoneEditor(id, opts) {
  opts = opts || {};
  if (open) return;
  if (!state.isAdmin) { msgDlg('View-only account: zones need an admin sign-in'); return; }
  let c0 = camFromId(id);
  if (!c0) { msgDlg('That camera no longer exists'); return; }
  if (c0.remote) { msgDlg('Zones of a camera on another server are edited on that server'); return; }
  open = true;
  ensureCss();
  try { await refreshCameras(); } catch (e) { /* use what we have */ }
  c0 = camFromId(id) || c0;

  // Shapes: rectangle {k:'r', t, x, y, w, h} or polygon {k:'p', t, pts:[[x,y],...]}; t is detect | ignore | mask.
  const R = (t) => (z) => ({ k: 'r', t, x: z.x, y: z.y, w: z.w, h: z.h });
  const P = (t) => (z) => ({ k: 'p', t, pts: (z.points || []).map(q => [q[0], q[1]]) });
  // Named alert zones: area {k:'p'|'r', t:'area'} or line {k:'l', t:'line'}, with name and options.
  const NZ = (z) => ({ k: z.kind === 'line' ? 'l' : 'p', t: z.kind === 'line' ? 'line' : 'area', pts: (z.points || []).map(q => [q[0], q[1]]),
    name: z.name, labels: (z.labels && z.labels.length ? z.labels : ['person']).slice(), seconds: z.seconds || 30, direction: z.direction || 'both',
    enabled: z.enabled !== false, alert: z.alert !== false, record: z.record !== false, ignore_parked: z.ignore_parked !== false, schedule: z.schedule || '' });
  // Quiet mode: the same editor draws the areas that never send a notification (alert_ignore_polys).
  // They are ignore-style shapes here and saved to that field instead of the motion zones.
  const Q = opts.mode === 'quiet';
  let zones = Q ? (c0.alert_ignore_polys || []).map(P('ignore')) : [].concat((c0.zones || []).map(R('detect')), (c0.ignore_zones || []).map(R('ignore')),
    (c0.zone_polys || []).map(P('detect')), (c0.ignore_polys || []).map(P('ignore')), (c0.privacy_masks || []).map(R('mask')), (c0.named_zones || []).map(NZ));
  const hadMasks = (c0.privacy_masks || []).length > 0;
  const key = () => JSON.stringify(zones);
  let saved = key();
  let sel = -1, hover = -1, type = Q ? 'ignore' : 'detect', tool = 'rect', selV = -1, poly = null;
  let hist = [key()], hi = 0, histLabel = '', histAt = 0;
  let ar = 16 / 9, img = null, havePic = false;
  let gone = false, flashUntil = new Map(), confirming = false, saving = false;
  let drag = null;
  const coarse = window.matchMedia('(pointer: coarse)').matches;
  const opener = opts.opener || document.activeElement;

  const root = document.createElement('div');
  root.className = 'ze';
  root.innerHTML =
    '<div class="ze-win" role="dialog" aria-modal="true" aria-label="' + (Q ? 'Edit areas that do not notify' : 'Edit motion zones') + '" tabindex="-1">' +
      '<div class="ze-head">' +
        '<h2 class="ze-title">' + (Q ? 'Areas that don\'t notify' : 'Zones') + ' <span class="muted">' + esc(c0.name) + '</span></h2>' +
        '<span class="ze-lamp" data-lamp>Quiet</span>' +
        '<div class="ze-seg" role="group" aria-label="New zone type"' + (Q ? ' hidden' : '') + '>' +
          '<button type="button" data-type="detect" aria-pressed="true" title="Motion counts only inside detect zones (if there are any)">Detect zone</button>' +
          '<button type="button" data-type="ignore" aria-pressed="false" title="Motion inside an ignore zone never counts">Ignore zone</button>' +
          '<button type="button" data-type="mask" aria-pressed="false" title="Blacked out in recordings, live view and snapshots">Privacy mask</button>' +
          '<button type="button" data-type="area" aria-pressed="false" title="A named area: alerts when someone stays inside too long (loitering). Needs AI.">Alert area</button>' +
          '<button type="button" data-type="line" aria-pressed="false" title="A named line with a direction arrow: alerts when someone crosses it. Needs AI.">Alert line</button></div>' +
        '<div class="ze-seg" role="group" aria-label="Shape tool">' +
          '<button type="button" data-tool="rect" aria-pressed="true" title="Drag to draw a rectangle">Rectangle</button>' +
          '<button type="button" data-tool="poly" aria-pressed="false" title="Click to add points; double-click, Enter or the first point closes the shape">Polygon</button></div>' +
        '<div class="ze-hist"><button type="button" class="btn" data-undo title="Undo (Ctrl+Z)">Undo</button><button type="button" class="btn" data-redo title="Redo (Ctrl+Shift+Z)">Redo</button></div>' +
        '<span class="ze-sp"></span>' +
        '<button type="button" class="btn" data-zcancel>Cancel</button>' +
        '<button type="button" class="primary" data-zsave>' + (Q ? 'Save areas' : 'Save zones') + '</button>' +
      '</div>' +
      '<div class="ze-confirm" data-confirm hidden><span>Discard your changes to the zones?</span><button type="button" class="btn" data-keep>Keep editing</button><button type="button" class="btn danger" data-discard>Discard</button></div>' +
      '<div class="ze-err" data-err hidden></div>' +
      '<div class="ze-note" data-masknote hidden>Masks are blacked out in recordings, live view and snapshots. The camera\'s video is re-encoded to do this, which uses more CPU.</div>' +
      '<div class="ze-main">' +
        '<div class="ze-stage" data-stage><canvas data-cv aria-label="Camera picture: drag to draw a zone; click a zone to select it"></canvas><div class="ze-nopic" data-nopic hidden>No picture right now. You can still draw zones.</div>' +
          '<div class="ze-polybar" data-polybar hidden><span data-pcount></span><button type="button" class="btn" data-pclose>Close</button><button type="button" class="btn" data-pback>Undo point</button><button type="button" class="btn" data-pcancel>Cancel</button></div></div>' +
        '<aside class="ze-side"><div class="ze-hint"><span data-hintdraw></span> Click a shape to select it; drag to move, handles to resize or reshape. Delete removes it. Arrow keys nudge (Shift: more). Polygons: drag a point, click a dot on an edge to add a point, Alt-click a point (or its x) to remove it.</div>' +
          '<ul class="ze-list" data-list></ul><div class="ze-opts" data-opts hidden></div></aside>' +
      '</div>' +
    '</div>';
  document.body.appendChild(root);
  // The page behind the editor is inert (Tab, clicks and screen readers stay inside); restored on close.
  const behind = [...document.body.children].filter(e => e !== root && !/^(SCRIPT|STYLE|LINK|SVG)$/i.test(e.tagName) && e.id !== 'toasts' && !e.hasAttribute('inert') && !e.classList.contains('dlg') && e.id !== 'dlgScrim');
  behind.forEach(e => e.setAttribute('inert', ''));
  const $q = (s) => root.querySelector(s);
  const win = $q('.ze-win'), cv = $q('[data-cv]'), stage = $q('[data-stage]'), list = $q('[data-list]');
  const noteEl = $q('[data-masknote]'), polybar = $q('[data-polybar]');
  const saveBtn = $q('[data-zsave]'), undoBtn = $q('[data-undo]'), redoBtn = $q('[data-redo]');
  const ctx = cv.getContext('2d');
  let cw = 0, ch = 0, dpr = 1;
  const colors = { motion: '#e3c650', accent: '#7fa8e0' };
  const probe = document.createElement('i');
  probe.style.cssText = 'position:absolute;width:0;height:0;visibility:hidden';
  root.appendChild(probe);
  const resolve = (v, dflt) => { probe.style.color = ''; probe.style.color = 'var(' + v + ')'; const r = getComputedStyle(probe).color; return r && r !== '' ? r : dflt; };
  const readColors = () => { colors.motion = resolve('--motion', colors.motion); colors.accent = resolve('--info', colors.accent); };
  readColors();
  // hatching for ignore zones
  const pat = document.createElement('canvas'); pat.width = pat.height = 10;
  { const p = pat.getContext('2d'); p.fillStyle = 'rgba(40,44,50,.35)'; p.fillRect(0, 0, 10, 10); p.strokeStyle = 'rgba(190,196,204,.75)'; p.lineWidth = 1.5; p.beginPath(); p.moveTo(-2, 12); p.lineTo(12, -2); p.moveTo(-2, 2); p.lineTo(2, -2); p.moveTo(8, 12); p.lineTo(12, 8); p.stroke(); }
  const hatch = ctx.createPattern(pat, 'repeat');

  // ---------- model ----------
  const dirty = () => key() !== saved;
  function record(label) {
    const k = key();
    if (k === hist[hi]) return;
    const now = Date.now();
    if (label && label === histLabel && now - histAt < 800 && hi === hist.length - 1 && hi > 0) { hist[hi] = k; } else { hist = hist.slice(0, hi + 1); hist.push(k); hi = hist.length - 1; }
    histLabel = label || ''; histAt = now;
    changed();
  }
  function goto(i) {
    if (i < 0 || i >= hist.length) return;
    hi = i; histLabel = '';
    zones = JSON.parse(hist[hi]);
    if (sel >= zones.length) sel = -1;
    renderOpts(true);
    changed();
  }
  function changed() { selV = -1; renderList(); renderOpts(false); draw(); status(); updateUi(); }
  function status() {
    saveBtn.disabled = !dirty() || saving;
    undoBtn.disabled = hi <= 0; redoBtn.disabled = hi >= hist.length - 1;
  }
  const TNAME = { detect: 'Detect', ignore: Q ? 'No notification' : 'Ignore', mask: 'Privacy mask', area: 'Area', line: 'Line' };
  const isNamed = (z) => z.t === 'area' || z.t === 'line';
  const isPts = (z) => z.k === 'p' || z.k === 'l';          // corner / vertex list (polygon or line)
  const minPts = (z) => (z.k === 'l' ? 2 : 3);
  const nameOf = (i) => {
    const z = zones[i]; let n = 0;
    if (isNamed(z)) return z.name || TNAME[z.t];
    for (let j = 0; j <= i; j++) if (zones[j].t === z.t) n++;
    return (z.t === 'mask' ? 'Mask ' : TNAME[z.t] + ' ') + n;
  };
  const rowName = (z) => (isNamed(z) ? (z.name || '(unnamed)') : TNAME[z.t] + (z.k === 'p' ? ' polygon (' + z.pts.length + ' points)' : ''));
  const effTool = () => (type === 'mask' ? 'rect' : tool);
  // A fresh name: "Area 1", "Line 2" ... the first free number.
  const freeName = (t) => { const w = TNAME[t]; for (let n = 1; ; n++) if (!zones.some(z => isNamed(z) && (z.name || '').toLowerCase() === (w + ' ' + n).toLowerCase())) return w + ' ' + n; };
  const newNamed = (t, k, pts) => ({ k, t, pts, name: freeName(t), labels: ['person'], seconds: 30, direction: 'both', enabled: true, alert: true, record: true, ignore_parked: true, schedule: '' });
  function updateUi() {
    root.querySelectorAll('[data-type]').forEach(x => x.setAttribute('aria-pressed', String(x.dataset.type === type)));
    root.querySelectorAll('[data-tool]').forEach(x => {
      x.setAttribute('aria-pressed', String(x.dataset.tool === effTool())); if (x.dataset.tool === 'poly') x.disabled = type === 'mask';
      x.textContent = type === 'line' ? (x.dataset.tool === 'rect' ? 'Straight' : 'Bent') : (x.dataset.tool === 'rect' ? 'Rectangle' : 'Polygon');
    });
    const what = type === 'mask' ? 'privacy mask' : type === 'area' ? 'alert area' : type === 'line' ? 'alert line' : type + ' zone';
    $q('[data-hintdraw]').textContent = type === 'line'
      ? (effTool() === 'poly' ? 'Click to add the points of a line; double-click or Enter finishes it. Esc cancels.' : 'Drag on the picture to draw a line. The arrow shows which way counts as in.')
      : effTool() === 'poly'
        ? 'Click on the picture to add points of a ' + what + ' polygon; double-click, Enter or the first point closes it. Esc cancels, Backspace removes the last point.'
        : 'Drag on the picture to draw a ' + what + '.';
    if (Q) $q('[data-hintdraw]').textContent = (effTool() === 'poly' ? 'Click on the picture to add the corners of an area; double-click, Enter or the first point closes it.' : 'Drag on the picture to draw an area.') + ' Things here are still detected, recorded and searchable. They just don\'t send a notification.';
    noteEl.hidden = !(!hadMasks && zones.some(z => z.t === 'mask'));
    polybar.hidden = !poly;
    if (poly) {
      const min = poly.t === 'line' ? 2 : 3;
      $q('[data-pcount]').textContent = poly.pts.length + ' point' + (poly.pts.length === 1 ? '' : 's');
      $q('[data-pclose]').disabled = poly.pts.length < min; $q('[data-pclose]').textContent = poly.t === 'line' ? 'Finish' : 'Close';
    }
  }
  function setSel(i) { sel = i; selV = -1; renderList(); renderOpts(false); draw(); }
  function cancelPoly() { if (!poly) return; poly = null; updateUi(); draw(); }
  function del(i) { if (i < 0 || i >= zones.length) return; zones.splice(i, 1); sel = -1; hover = -1; record(); }
  const cloneZ = (z) => JSON.parse(JSON.stringify(z));

  // ---------- geometry ----------
  const toN = (ev) => { const r = cv.getBoundingClientRect(); return { x: clamp((ev.clientX - r.left) / r.width, 0, 1), y: clamp((ev.clientY - r.top) / r.height, 0, 1) }; };
  const hitPx = () => (coarse ? 16 : 8);
  function bbox(z) {
    if (!isPts(z)) return { x: z.x, y: z.y, w: z.w, h: z.h };
    const xs = z.pts.map(q => q[0]), ys = z.pts.map(q => q[1]), x = Math.min(...xs), y = Math.min(...ys);
    return { x, y, w: Math.max(...xs) - x, h: Math.max(...ys) - y };
  }
  const area = (pts) => { let a = 0; for (let i = 0; i < pts.length; i++) { const p = pts[i], q = pts[(i + 1) % pts.length]; a += p[0] * q[1] - q[0] * p[1]; } return Math.abs(a) / 2; };
  function inPoly(pts, x, y) {
    let c = false;
    for (let i = 0, j = pts.length - 1; i < pts.length; j = i++) {
      const a = pts[i], b = pts[j];
      if ((a[1] > y) !== (b[1] > y) && x < (b[0] - a[0]) * (y - a[1]) / (b[1] - a[1]) + a[0]) c = !c;
    }
    return c;
  }
  const pxOf = (q) => { const r = cv.getBoundingClientRect(); return [r.left + q[0] * r.width, r.top + q[1] * r.height]; };
  const dpx = (ev, q) => { const [x, y] = pxOf(q); return Math.hypot(ev.clientX - x, ev.clientY - y); };
  // polygon handles: vertex index, the x badge of the selected vertex, or an edge midpoint (insert)
  function polyHit(z, ev) {
    const t = hitPx();
    if (selV >= 0 && z.pts.length > minPts(z)) { const q = z.pts[selV]; if (q) { const [x, y] = pxOf(q); if (Math.hypot(ev.clientX - x - XOFF, ev.clientY - y + XOFF) <= t) return { kind: 'x' }; } }
    let best = -1, bd = 1e9;
    z.pts.forEach((q, i) => { const d = dpx(ev, q); if (d <= t && d < bd) { best = i; bd = d; } });
    if (best >= 0) return { kind: 'v', i: best };
    for (let i = 0; i < (z.k === 'l' ? z.pts.length - 1 : z.pts.length); i++) {
      const a = z.pts[i], b = z.pts[(i + 1) % z.pts.length];
      const [ax, ay] = pxOf(a), [bx, by] = pxOf(b);
      if (Math.hypot(ax - bx, ay - by) < 24) continue;
      if (dpx(ev, [(a[0] + b[0]) / 2, (a[1] + b[1]) / 2]) <= t) return { kind: 'm', i };
    }
    return null;
  }
  const XOFF = 15;
  function shiftZ(z, z0, dx, dy) {
    const b = bbox(z0);
    dx = clamp(dx, -b.x, 1 - b.x - b.w); dy = clamp(dy, -b.y, 1 - b.y - b.h);
    if (isPts(z)) z.pts = z0.pts.map(q => [q[0] + dx, q[1] + dy]); else { z.x = z0.x + dx; z.y = z0.y + dy; }
  }
  function handlePts(z) {
    const x2 = z.x + z.w, y2 = z.y + z.h, mx = z.x + z.w / 2, my = z.y + z.h / 2;
    return { nw: [z.x, z.y], n: [mx, z.y], ne: [x2, z.y], e: [x2, my], se: [x2, y2], s: [mx, y2], sw: [z.x, y2], w: [z.x, my] };
  }
  function handleAt(z, ev) {
    const r = cv.getBoundingClientRect(), hp = handlePts(z), t = hitPx();
    let best = null, bd = 1e9;
    for (const n of NAMES) {
      const px = r.left + hp[n][0] * r.width, py = r.top + hp[n][1] * r.height;
      const d = Math.max(Math.abs(ev.clientX - px), Math.abs(ev.clientY - py));
      if (d <= t && d < bd) { best = n; bd = d; }
    }
    return best;
  }
  // a line is hit near its stroke (a few pixels), the other shapes inside
  function nearLine(z, p) {
    const t = hitPx() + 2;
    for (let i = 0; i + 1 < z.pts.length; i++) {
      const ax = z.pts[i][0] * cw, ay = z.pts[i][1] * ch, bx = z.pts[i + 1][0] * cw, by = z.pts[i + 1][1] * ch, px = p.x * cw, py = p.y * ch;
      const dx = bx - ax, dy = by - ay, l2 = dx * dx + dy * dy;
      const u = l2 ? clamp(((px - ax) * dx + (py - ay) * dy) / l2, 0, 1) : 0;
      if (Math.hypot(px - (ax + u * dx), py - (ay + u * dy)) <= t) return true;
    }
    return false;
  }
  const inside = (z, p) => (z.k === 'l' ? nearLine(z, p) : z.k === 'p' ? inPoly(z.pts, p.x, p.y) : p.x >= z.x && p.x <= z.x + z.w && p.y >= z.y && p.y <= z.y + z.h);
  function zoneAt(p) {
    if (sel >= 0 && inside(zones[sel], p)) return sel;
    for (let i = zones.length - 1; i >= 0; i--) if (inside(zones[i], p)) return i;
    return -1;
  }

  // ---------- drawing ----------
  function size() {
    const sw = stage.clientWidth, sh = stage.clientHeight;
    if (!sw || !sh) return;
    let w = Math.min(sw, sh * ar), h = w / ar;
    if (h > sh) { h = sh; w = h * ar; }
    w = Math.floor(w); h = Math.floor(h);
    dpr = window.devicePixelRatio || 1;
    if (w !== cw || h !== ch || cv.width !== Math.round(w * dpr)) {
      cw = w; ch = h;
      cv.style.width = w + 'px'; cv.style.height = h + 'px';
      cv.width = Math.round(w * dpr); cv.height = Math.round(h * dpr);
    }
    draw();
  }
  function rectPath(z) { return [z.x * cw, z.y * ch, z.w * cw, z.h * ch]; }
  function draw() {
    if (gone || !cw) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, cw, ch);
    ctx.fillStyle = '#05070a'; ctx.fillRect(0, 0, cw, ch);
    if (img && havePic) ctx.drawImage(img, 0, 0, cw, ch);
    const now = Date.now();
    zones.forEach((z, i) => {
      const bb = bbox(z), x = bb.x * cw, y = bb.y * ch, w = bb.w * cw, h = bb.h * ch;
      const hot = i === hover || i === sel;
      const path = () => {
        ctx.beginPath();
        if (isPts(z)) { z.pts.forEach((q, n) => (n ? ctx.lineTo(q[0] * cw, q[1] * ch) : ctx.moveTo(q[0] * cw, q[1] * ch))); if (z.k === 'p') ctx.closePath(); } else ctx.rect(x, y, w, h);
      };
      if (z.t === 'line') {
        drawLine(z, hot, i === sel);
        return;
      }
      if (z.t === 'area') {
        const occ = ((camFromId(id) || {}).zones_occupied || []).includes(z.name), col = z.enabled ? AREA : 'rgba(170,176,184,.9)';
        path(); ctx.fillStyle = occ ? 'rgba(227,198,80,.30)' : hot ? 'rgba(79,209,197,.26)' : 'rgba(79,209,197,.16)'; ctx.fill();
        ctx.lineJoin = 'round'; ctx.setLineDash(z.enabled ? [] : [6, 4]);
        ctx.lineWidth = 4; ctx.strokeStyle = 'rgba(0,0,0,.45)'; ctx.stroke();
        ctx.lineWidth = 2; ctx.strokeStyle = occ ? colors.motion : col; ctx.stroke(); ctx.setLineDash([]);
        const top = z.pts.reduce((m, q) => (q[1] < m[1] ? q : m), z.pts[0]);
        tagLabel((z.name || 'Area') + ' \u00b7 ' + (+z.seconds || 30) + ' s', top[0] * cw, top[1] * ch, col);
        if (i === sel) { ctx.lineWidth = 1; ctx.strokeStyle = colors.accent; ctx.strokeRect(x - 3.5, y - 3.5, w + 7, h + 7); }
        return;
      }
      if (z.t === 'mask') {
        path(); ctx.fillStyle = 'rgba(0,0,0,.6)'; ctx.fill();
        if (hot) { ctx.fillStyle = 'rgba(255,255,255,.1)'; ctx.fill(); }
        ctx.lineWidth = 1.5; ctx.strokeStyle = 'rgba(255,255,255,.55)'; ctx.stroke();
        lock(ctx, w > 34 && h > 34 ? x + 8 : x + w / 2 - 6, w > 34 && h > 34 ? y + 8 : y + h / 2 - 7);
      } else if (z.t === 'ignore') {
        path(); ctx.fillStyle = hatch; ctx.fill();
        if (hot) { ctx.fillStyle = 'rgba(255,255,255,.08)'; ctx.fill(); }
        ctx.setLineDash([6, 4]); ctx.lineWidth = 1.5; ctx.strokeStyle = 'rgba(0,0,0,.6)'; ctx.stroke();
        ctx.lineDashOffset = 5; ctx.strokeStyle = 'rgba(205,210,216,.95)'; ctx.stroke();
        ctx.setLineDash([]); ctx.lineDashOffset = 0;
      } else {
        path(); ctx.fillStyle = hot ? 'rgba(255,255,255,.24)' : 'rgba(255,255,255,.15)'; ctx.fill();
        const fl = (flashUntil.get(z) || 0) > now;
        ctx.lineJoin = 'round';
        ctx.lineWidth = 4; ctx.strokeStyle = 'rgba(0,0,0,.45)'; ctx.stroke();
        ctx.lineWidth = fl ? 3 : 2; ctx.strokeStyle = fl ? colors.motion : '#fff'; ctx.stroke();
      }
      if (i === sel) { ctx.lineWidth = 1; ctx.strokeStyle = colors.accent; ctx.strokeRect(x - 3.5, y - 3.5, w + 7, h + 7); }
      if (z.t !== 'mask') {
        ctx.font = '600 11px system-ui, sans-serif'; ctx.textBaseline = 'top';
        const label = nameOf(i), tw = ctx.measureText(label).width;
        let lx = x, ly = y;
        if (z.k === 'p') { const top = z.pts.reduce((m, q) => (q[1] < m[1] ? q : m), z.pts[0]); lx = top[0] * cw - tw / 2 - 4; ly = top[1] * ch; lx = clamp(lx, x, x + Math.max(0, w - tw - 8)); }
        if (w > tw + 12 && h > 22) { ctx.fillStyle = 'rgba(0,0,0,.55)'; ctx.fillRect(lx + 4, ly + 4, tw + 8, 16); ctx.fillStyle = '#fff'; ctx.fillText(label, lx + 8, ly + 7); }
      }
    });
    // motion blobs
    const c = camFromId(id);
    const bl = (c && c.motion && c.blobs) || [];
    ctx.lineWidth = 2; ctx.strokeStyle = colors.motion; ctx.fillStyle = colors.motion;
    for (const b of bl) {
      const x = b.x * cw, y = b.y * ch, w = b.w * cw, h = b.h * ch;
      ctx.globalAlpha = .2; ctx.fillRect(x, y, w, h); ctx.globalAlpha = 1; ctx.strokeRect(x, y, w, h);
    }
    // draft rectangle while drawing
    if (drag && drag.mode === 'draw' && drag.cur && drag.t === 'line') {
      const { a, b } = drag.cur;
      ctx.beginPath(); ctx.moveTo(a.x * cw, a.y * ch); ctx.lineTo(b.x * cw, b.y * ch);
      ctx.setLineDash([5, 4]); ctx.lineWidth = 3; ctx.strokeStyle = LINE; ctx.stroke(); ctx.setLineDash([]);
    } else if (drag && drag.mode === 'draw' && drag.cur) {
      const z = drag.cur, [x, y, w, h] = rectPath(z);
      ctx.setLineDash([5, 4]); ctx.lineWidth = 2; ctx.strokeStyle = '#fff';
      ctx.fillStyle = drag.t === 'mask' ? 'rgba(0,0,0,.6)' : drag.t === 'ignore' ? 'rgba(150,156,164,.35)' : drag.t === 'area' ? 'rgba(79,209,197,.2)' : 'rgba(255,255,255,.15)'; ctx.fillRect(x, y, w, h); ctx.strokeRect(x, y, w, h); ctx.setLineDash([]);
    }
    // draft polygon
    if (poly) {
      const pts = poly.pts, cur = poly.cur;
      ctx.beginPath(); pts.forEach((q, n) => (n ? ctx.lineTo(q[0] * cw, q[1] * ch) : ctx.moveTo(q[0] * cw, q[1] * ch)));
      if (cur) ctx.lineTo(cur.x * cw, cur.y * ch);
      if (poly.t !== 'line') { ctx.fillStyle = poly.t === 'ignore' ? 'rgba(150,156,164,.35)' : poly.t === 'area' ? 'rgba(79,209,197,.2)' : 'rgba(255,255,255,.15)'; ctx.fill(); }
      ctx.setLineDash([5, 4]); ctx.lineWidth = 2; ctx.strokeStyle = '#fff'; ctx.stroke(); ctx.setLineDash([]);
      pts.forEach((q, n) => {
        const closeable = n === 0 && pts.length >= 3 && poly.t !== 'line', near = closeable && cur && Math.hypot((cur.x - q[0]) * cw, (cur.y - q[1]) * ch) <= hitPx() + 2;
        ctx.beginPath(); ctx.arc(q[0] * cw, q[1] * ch, near ? 8 : closeable ? 6 : 4, 0, 7);
        ctx.fillStyle = near ? colors.accent : '#fff'; ctx.fill(); ctx.lineWidth = 1.5; ctx.strokeStyle = '#0b0e12'; ctx.stroke();
      });
    }
    // handles
    if (sel >= 0 && zones[sel] && !(drag && drag.mode === 'draw') && !poly) {
      const z = zones[sel], s = coarse ? 12 : 8;
      if (isPts(z)) {
        const n = z.pts.length;
        for (let i = 0; i < (z.k === 'l' ? n - 1 : n); i++) {
          const a = z.pts[i], b = z.pts[(i + 1) % n];
          if (Math.hypot((a[0] - b[0]) * cw, (a[1] - b[1]) * ch) < 24) continue;
          ctx.beginPath(); ctx.arc((a[0] + b[0]) / 2 * cw, (a[1] + b[1]) / 2 * ch, coarse ? 6 : 4, 0, 7);
          ctx.fillStyle = colors.accent; ctx.fill(); ctx.lineWidth = 1.5; ctx.strokeStyle = '#0b0e12'; ctx.stroke();
        }
        z.pts.forEach((q, i) => {
          const px = q[0] * cw, py = q[1] * ch;
          ctx.fillStyle = i === selV ? colors.accent : '#fff'; ctx.strokeStyle = '#0b0e12'; ctx.lineWidth = 1.5;
          ctx.fillRect(px - s / 2, py - s / 2, s, s); ctx.strokeRect(px - s / 2, py - s / 2, s, s);
        });
        if (selV >= 0 && n > minPts(z) && z.pts[selV]) {
          const bx = z.pts[selV][0] * cw + XOFF, by = z.pts[selV][1] * ch - XOFF;
          ctx.beginPath(); ctx.arc(bx, by, 9, 0, 7); ctx.fillStyle = '#c0392b'; ctx.fill(); ctx.lineWidth = 1.5; ctx.strokeStyle = '#fff'; ctx.stroke();
          ctx.beginPath(); ctx.moveTo(bx - 3.5, by - 3.5); ctx.lineTo(bx + 3.5, by + 3.5); ctx.moveTo(bx + 3.5, by - 3.5); ctx.lineTo(bx - 3.5, by + 3.5); ctx.stroke();
        }
      } else {
        const hp = handlePts(z);
        for (const n of NAMES) {
          const px = hp[n][0] * cw, py = hp[n][1] * ch;
          ctx.fillStyle = '#fff'; ctx.strokeStyle = '#0b0e12'; ctx.lineWidth = 1.5;
          ctx.fillRect(px - s / 2, py - s / 2, s, s); ctx.strokeRect(px - s / 2, py - s / 2, s, s);
        }
      }
    }
  }

  const AREA = '#4fd1c5', LINE = '#f6ad55';
  function tagLabel(text, x, y, col) {
    ctx.font = '600 11px system-ui, sans-serif'; ctx.textBaseline = 'top';
    const tw = ctx.measureText(text).width, lx = clamp(x - tw / 2 - 5, 2, Math.max(2, cw - tw - 12)), ly = clamp(y - 20, 2, Math.max(2, ch - 20));
    ctx.fillStyle = 'rgba(0,0,0,.7)'; ctx.fillRect(lx, ly, tw + 10, 17);
    ctx.fillStyle = col; ctx.fillRect(lx, ly, 3, 17);
    ctx.fillStyle = '#fff'; ctx.fillText(text, lx + 7, ly + 3);
  }
  // an arrow from (x, y) along (ux, uy), len px long
  function arrow(x, y, ux, uy, len, col) {
    const ex = x + ux * len, ey = y + uy * len, hx = -uy, hy = ux;
    ctx.beginPath(); ctx.moveTo(x, y); ctx.lineTo(ex, ey);
    ctx.moveTo(ex, ey); ctx.lineTo(ex - ux * 9 + hx * 6, ey - uy * 9 + hy * 6);
    ctx.moveTo(ex, ey); ctx.lineTo(ex - ux * 9 - hx * 6, ey - uy * 9 - hy * 6);
    ctx.lineCap = 'round'; ctx.lineWidth = 5; ctx.strokeStyle = 'rgba(0,0,0,.55)'; ctx.stroke();
    ctx.lineWidth = 2.5; ctx.strokeStyle = col; ctx.stroke(); ctx.lineCap = 'butt';
  }
  // A line: the stroke, A and B ends, and arrows at the middle of its longest part pointing the way that counts
  // as "in" (the right-hand side walking from A to B), "out" the other way, or both.
  function drawLine(z, hot, selected) {
    const col = z.enabled ? LINE : 'rgba(170,176,184,.9)';
    ctx.beginPath(); z.pts.forEach((q, n) => (n ? ctx.lineTo(q[0] * cw, q[1] * ch) : ctx.moveTo(q[0] * cw, q[1] * ch)));
    ctx.lineJoin = 'round'; ctx.lineCap = 'round'; ctx.setLineDash(z.enabled ? [] : [6, 4]);
    ctx.lineWidth = hot ? 7 : 6; ctx.strokeStyle = 'rgba(0,0,0,.5)'; ctx.stroke();
    ctx.lineWidth = hot ? 3.5 : 3; ctx.strokeStyle = col; ctx.stroke(); ctx.setLineDash([]); ctx.lineCap = 'butt';
    let bi = 0, bl = -1;
    for (let n = 0; n + 1 < z.pts.length; n++) { const l = Math.hypot((z.pts[n + 1][0] - z.pts[n][0]) * cw, (z.pts[n + 1][1] - z.pts[n][1]) * ch); if (l > bl) { bl = l; bi = n; } }
    const a = z.pts[bi], b = z.pts[bi + 1], dx = (b[0] - a[0]) * cw, dy = (b[1] - a[1]) * ch, L = Math.hypot(dx, dy) || 1;
    const nx = -dy / L, ny = dx / L, mx = (a[0] + b[0]) / 2 * cw, my = (a[1] + b[1]) / 2 * ch, len = Math.min(34, Math.max(18, L / 3));
    if (z.direction !== 'ba') arrow(mx, my, nx, ny, len, col);
    if (z.direction !== 'ab') arrow(mx, my, -nx, -ny, len, col);
    if (z.direction === 'ab' || z.direction === 'ba') {
      const s = z.direction === 'ab' ? 1 : -1;
      ctx.font = '700 10px system-ui, sans-serif'; ctx.textBaseline = 'middle'; ctx.textAlign = 'center'; ctx.fillStyle = '#fff'; ctx.strokeStyle = 'rgba(0,0,0,.7)'; ctx.lineWidth = 3;
      const tx = mx + s * nx * (len + 12), ty = my + s * ny * (len + 12);
      ctx.strokeText('in', tx, ty); ctx.fillText('in', tx, ty); ctx.textAlign = 'start';
    }
    [['A', z.pts[0]], ['B', z.pts[z.pts.length - 1]]].forEach(([t, q]) => {
      ctx.beginPath(); ctx.arc(q[0] * cw, q[1] * ch, 8, 0, 7); ctx.fillStyle = col; ctx.fill(); ctx.lineWidth = 1.5; ctx.strokeStyle = '#0b0e12'; ctx.stroke();
      ctx.font = '700 10px system-ui, sans-serif'; ctx.textBaseline = 'middle'; ctx.textAlign = 'center'; ctx.fillStyle = '#10141a'; ctx.fillText(t, q[0] * cw, q[1] * ch + 0.5); ctx.textAlign = 'start';
    });
    tagLabel(z.name || 'Line', mx, my - 6 - (z.direction === 'ba' ? 0 : 0), col);
    if (selected) { /* handles are drawn with the others */ }
  }

  // a small padlock, top-left at (x, y)
  function lock(c, x, y) {
    c.save(); c.lineWidth = 2; c.strokeStyle = '#fff'; c.fillStyle = '#fff';
    c.beginPath(); c.arc(x + 6, y + 5, 4, Math.PI, 0); c.stroke();
    c.fillRect(x, y + 5, 12, 9);
    c.fillStyle = '#000'; c.fillRect(x + 5, y + 8, 2, 4);
    c.restore();
  }

  function renderList() {
    if (!zones.length) {
      list.innerHTML = '<li class="ze-empty">' + (Q ? 'No areas yet: every detected object can notify. Draw on the picture to add one.' : 'No zones: the whole picture counts. Draw on the picture to add one.') + '</li>';
      return;
    }
    const row = (z, i) =>
      '<li class="ze-row' + (i === sel ? ' on' : '') + (isNamed(z) && !z.enabled ? ' off' : '') + '" data-i="' + i + '"><button type="button" class="ze-pick" data-pick="' + i + '" aria-pressed="' + (i === sel) + '">' +
        '<span class="ze-sw ' + (z.t === 'ignore' ? 'ig' : z.t === 'mask' ? 'pm' : z.t === 'area' ? 'ar' : z.t === 'line' ? 'ln' : 'de') + '"></span><span class="ze-nm">' + esc(rowName(z)) + '</span>' +
        (isNamed(z) ? '<span class="ze-sz">' + (z.t === 'line' ? 'line' : (+z.seconds || 30) + ' s') + '</span>' : isPts(z) ? '' : '<span class="ze-sz">' + pc(z.w) + ' x ' + pc(z.h) + '%</span>') + '</button>' +
        '<button type="button" class="ze-del" data-del="' + i + '" aria-label="Delete ' + esc(nameOf(i)) + '" title="Delete">&times;</button></li>';
    const zs = [], ms = [], ns = [];
    zones.forEach((z, i) => (z.t === 'mask' ? ms : isNamed(z) ? ns : zs).push(row(z, i)));
    list.innerHTML = zs.join('') + (ns.length ? '<li class="ze-sub">Alert zones</li>' + ns.join('') : '') + (ms.length ? '<li class="ze-sub">Privacy masks</li>' + ms.join('') : '');
  }

  // ---------- options of the selected alert zone ----------
  const OBJECTS = ['person', 'car', 'truck', 'bus', 'motorcycle', 'bicycle', 'dog', 'cat'];
  let optsFor = null;
  function renderOpts(force) {
    const box = $q('[data-opts]'), z = sel >= 0 ? zones[sel] : null;
    if (!z || !isNamed(z)) { box.hidden = true; box.innerHTML = ''; optsFor = null; return; }
    if (!force && optsFor === z) { box.hidden = false; return; }
    optsFor = z; box.hidden = false;
    const objs = OBJECTS.concat(z.labels.filter(l => !OBJECTS.includes(l)));
    const sw = (k, label, hint) => '<label class="sw"><input type="checkbox" role="switch" data-o="' + k + '"' + (z[k] ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span><span class="sw-text"><b>' + label + '</b>' + (hint ? '<small>' + hint + '</small>' : '') + '</span></label>';
    box.innerHTML =
      '<label class="ze-f">Name<input type="text" data-o="name" maxlength="40" value="' + esc(z.name) + '" autocomplete="off"></label>' +
      '<div class="ze-f">Counts<div class="ze-chips" data-chips>' + objs.map(l => '<button type="button" data-lab="' + esc(l) + '" aria-pressed="' + z.labels.includes(l) + '">' + esc(l) + '</button>').join('') + '</div></div>' +
      (z.t === 'area'
        ? '<label class="ze-f">Loitering after<span class="ze-num"><input type="number" data-o="seconds" min="1" max="3600" step="1" value="' + (+z.seconds || 30) + '" inputmode="numeric"> seconds inside</span></label>'
        : '<div class="ze-f">Alert when crossing<div class="ze-seg ze-dir" role="group" data-dir>' +
            [['ab', 'In (arrow)'], ['ba', 'Out'], ['both', 'Both']].map(([v, t]) => '<button type="button" data-d="' + v + '" aria-pressed="' + (z.direction === v) + '">' + t + '</button>').join('') + '</div></div>') +
      sw('enabled', 'Watching') + sw('alert', 'Send alerts', 'Push, ntfy, email, MQTT and the rest') + sw('ignore_parked', 'Ignore parked vehicles', 'A car that has sat in one place for minutes does not count') + sw('record', 'Save events', 'With a clip, searchable as "' + (z.t === 'area' ? 'loitering' : 'crossed') + ': ' + esc(z.name) + '"') +
      '<label class="ze-f">Only between <input type="text" data-o="schedule" placeholder="any time, or like 22:00-06:00" maxlength="11" value="' + esc(z.schedule || '') + '" autocomplete="off"></label>' +
      '<div class="ze-foot">Needs AI object checks to be on. ' + (z.t === 'line' ? 'Counts when the feet (bottom center of the object) cross.' : 'Counts when the feet are inside the area.') + '</div>';
  }
  $q('[data-opts]').addEventListener('input', (ev) => {
    const z = zones[sel], k = ev.target.dataset && ev.target.dataset.o;
    if (!z || !k) return;
    if (ev.target.type === 'checkbox') { z[k] = ev.target.checked; record('opt-' + k); return; }
    if (k === 'seconds') z.seconds = ev.target.value === '' ? '' : Math.round(+ev.target.value);
    else z[k] = ev.target.value;
    record('opt-' + k);
  });
  $q('[data-opts]').addEventListener('click', (ev) => {
    const z = zones[sel]; if (!z) return;
    const l = ev.target.closest('[data-lab]'), d = ev.target.closest('[data-d]');
    if (l) {
      const v = l.dataset.lab, on = z.labels.includes(v);
      z.labels = on ? z.labels.filter(x => x !== v) : z.labels.concat(v);
      l.setAttribute('aria-pressed', String(!on)); record('opt-labels');
    } else if (d) {
      z.direction = d.dataset.d;
      d.parentNode.querySelectorAll('[data-d]').forEach(b => b.setAttribute('aria-pressed', String(b === d))); record('opt-dir');
    }
  });

  // ---------- pointer ----------
  function cursorFor(p, ev) {
    if (poly) return 'crosshair';
    if (sel >= 0) {
      const z = zones[sel];
      if (isPts(z)) { const h = polyHit(z, ev); if (h) return h.kind === 'x' ? 'pointer' : h.kind === 'm' ? 'copy' : ev.altKey ? 'not-allowed' : 'grab'; }
      else { const h = handleAt(z, ev); if (h) return CURSOR[h]; }
    }
    return zoneAt(p) >= 0 ? 'move' : 'crosshair';
  }
  function closePoly() {
    if (!poly || poly.pts.length < (poly.t === 'line' ? 2 : 3)) return false;
    const pts = poly.pts.map(q => [r4(q[0]), r4(q[1])]);
    if (poly.t === 'line') {
      if (lineLen(pts) < 0.03) { showErr('That line is too short.'); return false; }
      showErr('');
      zones.push(newNamed('line', 'l', pts));
    } else {
      if (area(pts) <= MIN_AREA) { showErr('That polygon is too small or flat. Add points that enclose some area.'); return false; }
      showErr('');
      zones.push(poly.t === 'area' ? newNamed('area', 'p', pts) : { k: 'p', t: poly.t, pts });
    }
    sel = zones.length - 1; poly = null; record(); updateUi();
    return true;
  }
  const lineLen = (pts) => { let l = 0; for (let i = 0; i + 1 < pts.length; i++) l += Math.hypot(pts[i + 1][0] - pts[i][0], pts[i + 1][1] - pts[i][1]); return l; };
  function polyClick(ev, p) {
    const pts = poly.pts, n = pts.length, last = pts[n - 1];
    showErr('');
    if (poly.t !== 'line' && n >= 3 && dpx(ev, pts[0]) <= hitPx() + 2) { closePoly(); return; }
    if (dpx(ev, last) < 6) { if (n >= (poly.t === 'line' ? 2 : 3) && Date.now() - poly.at < 500) closePoly(); return; }   // second click of a double-click
    pts.push([p.x, p.y]); poly.at = Date.now(); poly.cur = p; updateUi(); draw();
  }
  cv.addEventListener('pointerdown', (ev) => {
    if (ev.button > 0 || confirming) return;
    ev.preventDefault();
    try { cv.setPointerCapture(ev.pointerId); } catch (e) { /* synthetic or gone pointer */ }
    const p = toN(ev);
    if (poly) { polyClick(ev, p); return; }
    if (sel >= 0) {
      const z = zones[sel];
      if (isPts(z)) {
        const h = polyHit(z, ev);
        if (h && h.kind === 'x') { z.pts.splice(selV, 1); selV = -1; record(); return; }
        if (h && h.kind === 'v') {
          if (ev.altKey) {
            if (z.pts.length > minPts(z)) { z.pts.splice(h.i, 1); record(); } else showErr(z.k === 'l' ? 'A line needs at least 2 points.' : 'A polygon needs at least 3 points.');
            return;
          }
          selV = h.i; draw();
          drag = { mode: 'vtx', vi: h.i, z0: cloneZ(z), id: ev.pointerId, moved: false }; return;
        }
        if (h && h.kind === 'm') {
          const z0 = cloneZ(z), a = z.pts[h.i], b = z.pts[(h.i + 1) % z.pts.length];
          z.pts.splice(h.i + 1, 0, [(a[0] + b[0]) / 2, (a[1] + b[1]) / 2]); selV = h.i + 1;
          drag = { mode: 'vtx', vi: h.i + 1, z0, id: ev.pointerId, moved: true }; renderList(); draw(); return;
        }
      } else {
        const h = handleAt(z, ev);
        if (h) { drag = { mode: 'size', h, z0: cloneZ(z), id: ev.pointerId }; return; }
      }
    }
    const i = zoneAt(p);
    if (i >= 0) {
      if (i !== sel) selV = -1;
      sel = i; renderList();
      drag = { mode: 'move', p0: p, z0: cloneZ(zones[i]), id: ev.pointerId, moved: false };
      draw(); return;
    }
    showErr('');
    if (effTool() === 'poly') {
      sel = -1; selV = -1; renderList();
      poly = { pts: [[p.x, p.y]], t: type, cur: p, at: Date.now() };
      updateUi(); draw(); return;
    }
    drag = { mode: 'draw', p0: p, cur: null, t: type, id: ev.pointerId };
  });
  cv.addEventListener('pointermove', (ev) => {
    const p = toN(ev);
    if (poly && !drag) { poly.cur = p; cv.style.cursor = 'crosshair'; draw(); return; }
    if (!drag) { cv.style.cursor = cursorFor(p, ev); return; }
    if (drag.id !== ev.pointerId) return;
    if (drag.mode === 'draw') {
      const a = drag.p0;
      drag.cur = drag.t === 'line' ? { a, b: p } : { x: Math.min(a.x, p.x), y: Math.min(a.y, p.y), w: Math.abs(p.x - a.x), h: Math.abs(p.y - a.y) };
    } else if (drag.mode === 'move') {
      const dx = p.x - drag.p0.x, dy = p.y - drag.p0.y;
      if (Math.abs(dx) + Math.abs(dy) > 0.002) drag.moved = true;
      shiftZ(zones[sel], drag.z0, dx, dy);
    } else if (drag.mode === 'vtx') {
      const q = zones[sel].pts[drag.vi], r = cv.getBoundingClientRect();
      if (!drag.moved && Math.hypot((p.x - q[0]) * r.width, (p.y - q[1]) * r.height) < 3) return;
      drag.moved = true; q[0] = p.x; q[1] = p.y;
    } else if (drag.mode === 'size') {
      const z = zones[sel], z0 = drag.z0, h = drag.h;
      let x1 = z0.x, y1 = z0.y, x2 = z0.x + z0.w, y2 = z0.y + z0.h;
      if (h.includes('w')) x1 = clamp(p.x, 0, x2 - MIN);
      if (h.includes('e')) x2 = clamp(p.x, x1 + MIN, 1);
      if (h.includes('n')) y1 = clamp(p.y, 0, y2 - MIN);
      if (h.includes('s')) y2 = clamp(p.y, y1 + MIN, 1);
      z.x = x1; z.y = y1; z.w = x2 - x1; z.h = y2 - y1;
    }
    draw();
    if (drag.mode !== 'draw') renderList();
  });
  const end = (ev) => {
    if (!drag || drag.id !== ev.pointerId) return;
    const d = drag; drag = null;
    if (ev.type === 'pointercancel') { if (d.z0 && zones[sel]) zones[sel] = d.z0; renderList(); draw(); return; }
    if (d.mode === 'draw') {
      const z = d.cur;
      if (d.t === 'line') {
        if (!z || Math.hypot((z.b.x - z.a.x) * cw, (z.b.y - z.a.y) * ch) < 16) { sel = -1; renderList(); renderOpts(false); draw(); return; }
        zones.push(newNamed('line', 'l', [[r4(z.a.x), r4(z.a.y)], [r4(z.b.x), r4(z.b.y)]]));
        sel = zones.length - 1; record(); return;
      }
      if (!z || z.w < MIN || z.h < MIN) { sel = -1; renderList(); renderOpts(false); draw(); return; }
      if (d.t === 'area') zones.push(newNamed('area', 'p', [[r4(z.x), r4(z.y)], [r4(z.x + z.w), r4(z.y)], [r4(z.x + z.w), r4(z.y + z.h)], [r4(z.x), r4(z.y + z.h)]]));
      else zones.push({ k: 'r', t: d.t, x: z.x, y: z.y, w: z.w, h: z.h });
      sel = zones.length - 1; record();
    } else if (d.mode === 'vtx' && !d.moved) { renderList(); draw(); }
    else if (d.mode === 'move' && !d.moved) { renderList(); draw(); } else { record(); }
  };
  cv.addEventListener('pointerup', end);
  cv.addEventListener('pointercancel', end);
  cv.addEventListener('pointerleave', () => { if (!drag) { cv.style.cursor = ''; if (poly) { poly.cur = null; draw(); } } });
  cv.style.touchAction = 'none';

  list.addEventListener('click', (ev) => {
    const d = ev.target.closest('[data-del]');
    if (d) { del(+d.dataset.del); return; }
    const p = ev.target.closest('[data-pick]');
    if (p) setSel(+p.dataset.pick === sel ? -1 : +p.dataset.pick);
  });
  list.addEventListener('pointerover', (ev) => { const r = ev.target.closest('.ze-row'); const i = r ? +r.dataset.i : -1; if (i !== hover) { hover = i; draw(); } });
  list.addEventListener('pointerleave', () => { hover = -1; draw(); });
  list.addEventListener('focusin', (ev) => { const r = ev.target.closest('.ze-row'); if (r) { hover = +r.dataset.i; draw(); } });
  list.addEventListener('focusout', () => { hover = -1; draw(); });

  root.querySelectorAll('[data-type]').forEach(b => {
    b.onclick = () => {
      type = b.dataset.type;
      if (type === 'mask' || (poly && (type === 'line') !== (poly.t === 'line'))) cancelPoly(); else if (poly) poly.t = type;
      updateUi();
    };
  });
  root.querySelectorAll('[data-tool]').forEach(b => {
    b.onclick = () => { tool = b.dataset.tool; if (tool !== 'poly') cancelPoly(); updateUi(); };
  });
  $q('[data-pclose]').onclick = () => { closePoly(); win.focus(); };
  $q('[data-pback]').onclick = () => { if (poly) { poly.pts.pop(); if (!poly.pts.length) poly = null; updateUi(); draw(); } };
  $q('[data-pcancel]').onclick = () => { cancelPoly(); win.focus(); };
  undoBtn.onclick = () => goto(hi - 1);
  redoBtn.onclick = () => goto(hi + 1);

  // ---------- save / close ----------
  function validated() {
    const seen = [];
    for (const z of zones) {
      if (isNamed(z)) {
        const n = (z.name || '').trim();
        if (!n) return 'Give every alert zone a name.';
        if (n.length > 40) return 'Zone names can be up to 40 characters.';
        const k = n.toLowerCase().replace(/[^a-z0-9]/g, '_');
        if (seen.includes(k)) return 'Two alert zones are named "' + n + '". Names must differ.';
        seen.push(k);
        if (!z.labels.length) return '"' + n + '": pick at least one object that counts.';
        if (z.t === 'area' && !(+z.seconds >= 1 && +z.seconds <= 3600)) return '"' + n + '": loitering time must be 1 to 3600 seconds.';
        if (z.schedule && !/^\s*([01]?\d|2[0-3]):[0-5]\d\s*-\s*([01]?\d|2[0-3]):[0-5]\d\s*$/.test(z.schedule)) return '"' + n + '": write the time window like 22:00-06:00.';
      }
      if (isPts(z)) {
        if (z.pts.length < minPts(z)) return z.k === 'l' ? 'A line needs at least 2 points.' : 'A polygon needs at least 3 points.';
        if (z.pts.some(q => q[0] < -1e-9 || q[0] > 1 + 1e-9 || q[1] < -1e-9 || q[1] > 1 + 1e-9)) return 'A zone reaches outside the picture.';
        if (z.k === 'l' ? lineLen(z.pts) < 0.02 : !(area(z.pts) > MIN_AREA)) return z.k === 'l' ? 'A line is too short. Move its points apart or delete it.' : 'A polygon is too small or flat. Move its points apart or delete it.';
        continue;
      }
      if (!(z.w >= MIN - 1e-9 && z.h >= MIN - 1e-9)) return 'Every zone must be at least 1% wide and 1% high.';
      if (z.x < -1e-9 || z.y < -1e-9 || z.x + z.w > 1 + 1e-9 || z.y + z.h > 1 + 1e-9) return 'A zone reaches outside the picture.';
    }
    return '';
  }
  const showErr = (m) => { const e = $q('[data-err]'); e.textContent = m; e.hidden = !m; };
  async function save() {
    if (saving || !dirty()) return;
    const bad = validated();
    if (bad) { showErr(bad); return; }
    showErr('');
    const fix = (z) => {
      const x = clamp(r4(z.x), 0, 1), y = clamp(r4(z.y), 0, 1);
      return { x, y, w: Math.min(r4(z.w), r4(1 - x)), h: Math.min(r4(z.h), r4(1 - y)) };
    };
    const rects = (t) => zones.filter(z => z.k === 'r' && z.t === t).map(fix);
    const polys = (t) => zones.filter(z => z.k === 'p' && z.t === t).map(z => ({ points: z.pts.map(q => [clamp(r4(q[0]), 0, 1), clamp(r4(q[1]), 0, 1)]) }));
    const named = zones.filter(isNamed).map(z => ({
      name: z.name.trim(), kind: z.t, points: z.pts.map(q => [clamp(r4(q[0]), 0, 1), clamp(r4(q[1]), 0, 1)]), enabled: !!z.enabled, labels: z.labels.slice(),
      seconds: Math.max(1, Math.min(3600, Math.round(+z.seconds) || 30)), direction: z.direction || 'both', alert: !!z.alert, record: !!z.record, ignore_parked: z.ignore_parked !== false, schedule: (z.schedule || '').trim() || null }));
    // (A rectangle drawn for a quiet area is saved as a four-corner polygon.)
    const quiet = () => polys('ignore').concat(rects('ignore').map(r => ({ points: [[r.x, r.y], [r.x + r.w, r.y], [r.x + r.w, r.y + r.h], [r.x, r.y + r.h]].map(q => [r4(q[0]), r4(q[1])]) })));
    const body = Q ? { alert_ignore_polys: quiet() } : { zones: rects('detect'), ignore_zones: rects('ignore'), zone_polys: polys('detect'), ignore_polys: polys('ignore'), privacy_masks: rects('mask'), named_zones: named };
    saving = true; saveBtn.textContent = 'Saving...'; status();
    try {
      await api('/api/cameras/' + id, { method: 'PUT', json: body });
    } catch (e) {
      saving = false; saveBtn.textContent = Q ? 'Save areas' : 'Save zones'; status();
      let m = e.body || e.message; try { const j = JSON.parse(m); if (j && j.error) m = j.error; } catch (x) { /* plain */ }
      showErr(m); return;
    }
    saving = false;
    try { await refreshCameras(); } catch (e) { /* next poll */ }
    const cam = camFromId(id) || Object.assign({}, c0, body);
    saved = key();
    toast((Q ? 'Saved areas that don\'t notify for ' : 'Saved zones for ') + (cam.name || c0.name), { kind: 'ok' });
    if (opts.onSaved) { try { opts.onSaved(cam); } catch (e) { /* the opener went away */ } }
    close();
  }
  function ask() {
    if (!dirty()) { close(); return; }
    confirming = true; $q('[data-confirm]').hidden = false;
    $q('[data-keep]').focus();
  }
  function unask() { confirming = false; $q('[data-confirm]').hidden = true; }
  saveBtn.onclick = save;
  $q('[data-zcancel]').onclick = ask;
  $q('[data-keep]').onclick = () => { unask(); win.focus(); };
  $q('[data-discard]').onclick = () => close();
  root.addEventListener('pointerdown', (ev) => { if (ev.target === root) ask(); });

  function onKey(ev) {
    if (gone || topDlgOpen()) return;
    const k = ev.key, mod = ev.ctrlKey || ev.metaKey;
    const typing = ev.target && /^(INPUT|TEXTAREA|SELECT)$/.test(ev.target.tagName);
    const eat = () => { ev.preventDefault(); ev.stopPropagation(); };
    // Ctrl+S must never reach the browser's Save dialog, even from a field.
    if (mod && !ev.altKey && (k === 's' || k === 'S')) { eat(); if (!confirming) save(); return; }
    if (k === 'Escape') { eat(); if (confirming) unask(); else if (poly) cancelPoly(); else ask(); return; }
    // Undo / redo work from the buttons and selects too; real text fields keep their own undo.
    if (mod && !confirming && !(typing && /^(TEXTAREA|INPUT)$/.test(ev.target.tagName) && /^(text|search|number|)$/.test(ev.target.type || ''))) {
      if (k === 'z' || k === 'Z') { eat(); goto(ev.shiftKey ? hi + 1 : hi - 1); return; }
      if (k === 'y' || k === 'Y') { eat(); goto(hi + 1); return; }
    }
    if (k === 'Tab') {
      const f = [...root.querySelectorAll('button:not([disabled]), [href], input, select, [tabindex]:not([tabindex="-1"])')].filter(e => e.offsetParent !== null);
      if (!f.length) return;
      const a = document.activeElement, i = f.indexOf(a);
      if (ev.shiftKey && (i <= 0)) { eat(); f[f.length - 1].focus(); } else if (!ev.shiftKey && (i === f.length - 1 || i < 0)) { eat(); f[0].focus(); }
      return;
    }
    if (typing || confirming || (ev.target.closest && ev.target.closest('[data-opts]'))) return;
    if (poly) {
      if (k === 'Enter') { eat(); closePoly(); return; }
      if (k === 'Backspace' || k === 'Delete') { eat(); poly.pts.pop(); if (!poly.pts.length) poly = null; updateUi(); draw(); return; }
    }
    if ((k === 'Delete' || k === 'Backspace') && sel >= 0) { eat(); del(sel); return; }
    if (sel >= 0 && k.startsWith('Arrow') && !(ev.target.closest && ev.target.closest('[data-seg]'))) {
      const st = ev.shiftKey ? 0.05 : 0.01, z = zones[sel];
      let dx = 0, dy = 0;
      if (k === 'ArrowLeft') dx = -st; else if (k === 'ArrowRight') dx = st; else if (k === 'ArrowUp') dy = -st; else if (k === 'ArrowDown') dy = st; else return;
      shiftZ(z, cloneZ(z), dx, dy);
      eat(); record('nudge'); return;
    }
  }
  askNow = () => { if (confirming) unask(); else if (poly) cancelPoly(); else ask(); };
  overlaysChanged();
  document.addEventListener('keydown', onKey, true);
  const onBefore = (e) => { if (dirty()) { e.preventDefault(); e.returnValue = ''; } };
  window.addEventListener('beforeunload', onBefore);

  // ---------- picture + live motion ----------
  const ro = new ResizeObserver(size);
  ro.observe(stage);
  window.addEventListener('resize', size);
  let picTimer = 0, tickTimer = 0;
  const nopic = $q('[data-nopic]');
  function pic() {
    if (gone) return;
    const n = new Image();
    n.onload = () => {
      if (gone) return;
      img = n; havePic = true; nopic.hidden = true;
      const a = n.naturalWidth / n.naturalHeight;
      if (a > 0.2 && a < 10 && Math.abs(a - ar) > 0.001) { ar = a; size(); } else draw();
      picTimer = setTimeout(pic, 1000);
    };
    n.onerror = () => { if (gone) return; if (!havePic) nopic.hidden = false; picTimer = setTimeout(pic, 3000); };
    n.src = '/api/snapshot/' + id + '?t=' + Date.now();
  }
  function tick() {
    if (gone) return;
    const c = camFromId(id), lamp = $q('[data-lamp]'), mo = !!(c && c.motion);
    lamp.textContent = mo ? 'Motion now' : 'Quiet'; lamp.classList.toggle('now', mo);
    if (mo && c.blobs) {
      const now = Date.now();
      for (const z of zones) {
        if (z.t !== 'detect') continue;
        const hit = z.k === 'p'
          ? (b) => [[b.x, b.y], [b.x + b.w, b.y], [b.x, b.y + b.h], [b.x + b.w, b.y + b.h], [b.x + b.w / 2, b.y + b.h / 2]].some(q => inPoly(z.pts, q[0], q[1])) ||
              z.pts.some(q => q[0] >= b.x && q[0] <= b.x + b.w && q[1] >= b.y && q[1] <= b.y + b.h)
          : (b) => b.x < z.x + z.w && b.x + b.w > z.x && b.y < z.y + z.h && b.y + b.h > z.y;
        if (c.blobs.some(hit)) flashUntil.set(z, now + 700);
      }
    }
    if (document.documentElement.dataset.theme !== tick.th) { tick.th = document.documentElement.dataset.theme; readColors(); }
    draw();
    tickTimer = setTimeout(tick, 200);
  }

  function close() {
    if (gone) return;
    gone = true; open = null; askNow = null;
    clearTimeout(picTimer); clearTimeout(tickTimer);
    ro.disconnect();
    window.removeEventListener('resize', size);
    window.removeEventListener('beforeunload', onBefore);
    document.removeEventListener('keydown', onKey, true);
    behind.forEach(e => e.removeAttribute('inert'));
    root.remove();
    overlaysChanged();
    if (opener && opener.isConnected && opener.focus) { try { opener.focus({ preventScroll: true }); } catch (e) { /* gone */ } }
  }

  renderList(); status(); updateUi();
  size();
  pic(); tick();
  win.focus();
}
