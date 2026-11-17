// masks.js - a camera's privacy masks drawn over its video in the browser.
//
// With "Hide on screen and in pictures" (mask_mode "screen") the camera's own
// stream is recorded and served as it is, and every player in the web app
// draws the masks as solid black boxes over the picture (the server tells the
// client to do so: it sends draws=masks). Pictures the server makes (JPEG
// feed, snapshots, thumbnails, timeline pictures) are masked there already.
//
// One helper for every player: watchMasks(host, cam) finds the <video>
// elements in `host` (now and later), puts a layer of black boxes right after
// each, and keeps it exactly over the picture: the element's box, its
// object-fit / object-position letterboxing, and its CSS transform (the
// digital zoom moves the video by a transform; the layer copies it). It
// follows resizes, new stream sizes and videos that are swapped or hidden.
// No imports: the display page and the players use it too.

// The camera's masks as clean 0..1 rectangles (clamped to the picture).
export function maskList(cam) {
  const ms = cam && Array.isArray(cam.privacy_masks) ? cam.privacy_masks : [];
  const out = [];
  for (const m of ms) {
    if (!m) continue;
    let x = +m.x, y = +m.y, w = +m.w, h = +m.h;
    if (![x, y, w, h].every(Number.isFinite)) continue;
    if (x < 0) { w += x; x = 0; }
    if (y < 0) { h += y; y = 0; }
    w = Math.min(w, 1 - x); h = Math.min(h, 1 - y);
    if (w > 0 && h > 0) out.push({ x, y, w, h });
  }
  return out;
}

// '&m=<mask_rev>' for a picture address of this camera that the browser
// keeps (previews, filmstrips, AI frames): a mask change makes it a new
// address, so a picture made before the change is not shown from the cache.
// (`sep`: '?' when it is the address's first parameter.)
// (A camera of a linked server comes without mask_rev: a revision is made
// here from its masks and level instead.)
export function maskRevQ(cam, sep = '&') {
  const r = maskRev(cam);
  return r ? sep + 'm=' + encodeURIComponent(r) : '';
}

// A camera's mask revision: the server's, else one made from its masks.
export function maskRev(cam) {
  if (!cam) return '';
  if (cam.mask_rev) return String(cam.mask_rev);
  const list = maskList(cam);
  return list.length ? 'c' + fnv(JSON.stringify([cam.mask_mode || '', list])) : '';
}

// All cameras' mask revisions as one, for pictures whose camera the page does
// not know (a plate's best picture): '?m=...' or '' when no camera has masks.
export function allMasksRevQ(cams, sep = '?') {
  const revs = (cams || []).map(c => c && (c.id + ':' + maskRev(c))).filter(r => r && !r.endsWith(':'));
  return revs.length ? sep + 'm=' + fnv(revs.sort().join(',')) : '';
}

// A short hash (FNV-1a, 32 bits, hex).
function fnv(s) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 0x01000193) >>> 0; }
  return h.toString(16).padStart(8, '0');
}

// A pushed (push://) or battery (baichuan://) camera: no ffmpeg reader, so it
// cannot hide its masks in its recordings. The server's rule exactly
// (config.rs no_ffmpeg_reader): push:// in any case, after spaces; baichuan://
// as written.
export function noFfmpegReader(url) {
  const u = String(url || '');
  return u.trim().toLowerCase().startsWith('push://') || u.startsWith('baichuan://');
}

// One object-position value ("50%", "12px", "left", ...) as an offset in px,
// given the free space (box size minus picture size; negative for cover).
export function posOffset(tok, free) {
  const t = String(tok == null ? '50%' : tok).trim();
  if (t === 'left' || t === 'top') return 0;
  if (t === 'right' || t === 'bottom') return free;
  if (t === 'center' || t === '') return free / 2;
  const n = parseFloat(t);
  if (!Number.isFinite(n)) return free / 2;
  return t.endsWith('%') ? free * n / 100 : n;
}

// Where a nw x nh picture is drawn inside a bw x bh box for CSS object-fit
// `fit` and object-position `pos` ("50% 50%" by default). Returns
// {left, top, width, height} in px, relative to the box (it can reach
// outside the box for cover). Unknown picture size: the whole box.
export function fitRect(bw, bh, nw, nh, fit, pos) {
  if (!(bw > 0) || !(bh > 0)) return { left: 0, top: 0, width: 0, height: 0 };
  if (!(nw > 0) || !(nh > 0) || fit === 'fill') return { left: 0, top: 0, width: bw, height: bh };
  let s;
  if (fit === 'cover') s = Math.max(bw / nw, bh / nh);
  else if (fit === 'none') s = 1;
  else if (fit === 'scale-down') s = Math.min(1, bw / nw, bh / nh);
  else s = Math.min(bw / nw, bh / nh);   // contain (a <video>'s default)
  const w = nw * s, h = nh * s;
  const p = String(pos || '50% 50%').trim().split(/\s+/);
  const left = posOffset(p[0], bw - w), top = posOffset(p.length > 1 ? p[1] : '50%', bh - h);
  return { left, top, width: w, height: h };
}

// The masks as px boxes over picture rect `r`, each edge moved outward to a
// whole pixel so no sliver of the picture shows at its edge.
const EPS = 1e-6;
export function maskBoxes(r, masks) {
  const out = [];
  for (const m of masks || []) {
    // (A rounding error of the sums, 300.00000000000006, is not a pixel more.)
    const l = Math.floor(r.left + m.x * r.width + EPS), t = Math.floor(r.top + m.y * r.height + EPS);
    const rr = Math.ceil(r.left + (m.x + m.w) * r.width - EPS), b = Math.ceil(r.top + (m.y + m.h) * r.height - EPS);
    if (rr > l && b > t) out.push({ left: l, top: t, width: rr - l, height: b - t });
  }
  return out;
}

// Paints the masks black into a canvas holding a whole w x h picture (a
// frame grabbed from the video, e.g. a tile's last picture).
export function paintMasks(ctx, w, h, cam) {
  const list = maskList(cam);
  if (!list.length) return;
  ctx.save();
  ctx.fillStyle = '#000';
  for (const b of maskBoxes({ left: 0, top: 0, width: w, height: h }, list)) ctx.fillRect(b.left, b.top, b.width, b.height);
  ctx.restore();
}

// A <video> under drawn masks plays the camera's unmasked video: the browser's
// own ways of taking that video away from the page (its full screen, Picture
// in Picture, casting, "Save video as" and the Download menu) would show or
// save it without the black boxes. guardVideo(v) turns them off; a <video
// controls> gets a full-screen button of ours that puts its box (the video
// and the mask layer) into full screen instead. Safe to call again.
const fsEl = () => document.fullscreenElement || document.webkitFullscreenElement || null;
const quiet = (p) => { if (p && p.catch) p.catch(() => {}); };
const exitFs = () => { try { quiet((document.exitFullscreen || document.webkitExitFullscreen).call(document)); } catch (e) { /* not in it */ } };
export function guardVideo(v) {
  if (!v || v.tagName !== 'VIDEO' || v.dataset.pmaskGuard) return;
  v.dataset.pmaskGuard = '1';
  // Chrome, Edge: the ⋮ menu's Download, full screen and cast entries.
  try { v.setAttribute('controlslist', 'nodownload nofullscreen noremoteplayback'); } catch (e) { /* old browser */ }
  v.disablePictureInPicture = true; v.setAttribute('disablepictureinpicture', '');
  v.disableRemotePlayback = true; v.setAttribute('disableremoteplayback', '');
  v.setAttribute('x-webkit-airplay', 'deny');
  // iPhone: play in the page, never in its own full-screen player.
  v.playsInline = true; v.setAttribute('playsinline', ''); v.setAttribute('webkit-playsinline', '');
  v.addEventListener('contextmenu', (e) => e.preventDefault());
  // Where a browser goes to full screen or Picture in Picture anyway (Firefox's
  // controls, a double click, Safari's own buttons): straight back.
  v.addEventListener('enterpictureinpicture', () => { try { quiet(document.exitPictureInPicture()); } catch (e) { /* not in it */ } });
  v.addEventListener('webkitbeginfullscreen', () => { try { v.webkitExitFullscreen(); } catch (e) { /* not in it */ } });
  v.addEventListener('webkitpresentationmodechanged', () => {
    try { if (v.webkitPresentationMode && v.webkitPresentationMode !== 'inline') v.webkitSetPresentationMode('inline'); } catch (e) { /* unsupported */ }
  });
  const back = () => { if (fsEl() === v) exitFs(); };
  v.addEventListener('fullscreenchange', back);
  v.addEventListener('webkitfullscreenchange', back);
  if (v.hasAttribute('controls')) addFsButton(v);
}

// Our full-screen button for a guarded <video controls>: its box goes full
// screen (where the browser can put an element there; not on iPhone).
function addFsButton(v) {
  const box = v.parentElement;
  if (!box || box.querySelector(':scope > .pmask-fs')) return;
  if (!(box.requestFullscreen || box.webkitRequestFullscreen)) return;
  box.classList.add('pmask-fsbox');
  const b = document.createElement('button');
  b.type = 'button';
  b.className = 'pmask-fs';
  b.title = 'Full screen';
  b.setAttribute('aria-label', 'Full screen');
  b.innerHTML = '<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true"><path fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5"/></svg>';
  b.onclick = (e) => {
    e.stopPropagation();
    if (fsEl() === box) { exitFs(); return; }
    try { quiet((box.requestFullscreen || box.webkitRequestFullscreen).call(box)); } catch (x) { /* refused */ }
  };
  box.appendChild(b);
}

const watchers = new Set();
// After the cameras were read again (masks changed): redraw every layer.
// (A watcher whose host has left the page stops by itself here.)
export function refreshMasks() { for (const w of [...watchers]) w.refresh(); }

// Draws `cam`'s masks over every <video> in `host` (and `host` itself when it
// is one). `cam`: the camera object, or a function returning the current one.
// opts.selector: other elements to cover ('video' by default).
// Returns { refresh(), stop() }.
export function watchMasks(host, cam, opts) {
  const sel = (opts && opts.selector) || 'video';
  const get = typeof cam === 'function' ? cam : () => cam;
  const on = new Map();   // media element -> { layer, update, stop }
  let stopped = false;
  function attach(m) {
    const layer = document.createElement('div');
    layer.className = 'pmask';
    layer.setAttribute('aria-hidden', 'true');
    layer.hidden = true;
    m.after(layer);
    let last = '';
    const update = () => {
      if (stopped) return;
      if (!m.isConnected || !host.contains(m)) { detach(m); return; }
      const list = maskList(get());
      const w = m.offsetWidth, h = m.offsetHeight;
      if (list.length) guardVideo(m);
      if (!list.length || !w || !h) { layer.hidden = true; return; }
      if (layer.previousElementSibling !== m) m.after(layer);
      const cs = getComputedStyle(m);
      const cw = m.clientWidth || w, ch = m.clientHeight || h;
      const nw = m.videoWidth || m.naturalWidth || (m.tagName === 'CANVAS' ? m.width : 0);
      const nh = m.videoHeight || m.naturalHeight || (m.tagName === 'CANVAS' ? m.height : 0);
      const r = fitRect(cw, ch, nw, nh, cs.objectFit, cs.objectPosition);
      r.left += m.clientLeft; r.top += m.clientTop;
      const boxes = maskBoxes(r, list);
      const tf = cs.transform && cs.transform !== 'none' ? cs.transform : '';
      const key = [m.offsetLeft, m.offsetTop, w, h, tf, cs.transformOrigin, JSON.stringify(boxes)].join('|');
      layer.hidden = false;
      if (key === last) return;
      last = key;
      const s = layer.style;
      s.left = m.offsetLeft + 'px'; s.top = m.offsetTop + 'px'; s.width = w + 'px'; s.height = h + 'px';
      s.transform = tf; s.transformOrigin = tf ? cs.transformOrigin : '';
      while (layer.children.length > boxes.length) layer.lastChild.remove();
      while (layer.children.length < boxes.length) layer.appendChild(document.createElement('i'));
      boxes.forEach((b, i) => {
        const e = layer.children[i].style;
        e.left = b.left + 'px'; e.top = b.top + 'px'; e.width = b.width + 'px'; e.height = b.height + 'px';
      });
    };
    const ro = new ResizeObserver(update);
    ro.observe(m);
    if (m.parentElement) ro.observe(m.parentElement);
    // (The zoom sets the video's transform; a player sets its aspect ratio or hides it.)
    const mo = new MutationObserver(update);
    mo.observe(m, { attributes: true, attributeFilter: ['style', 'class', 'hidden', 'width', 'height'] });
    const evs = ['loadedmetadata', 'resize', 'load', 'emptied', 'playing'];
    for (const e of evs) m.addEventListener(e, update);
    on.set(m, { layer, update, stop() { ro.disconnect(); mo.disconnect(); for (const e of evs) m.removeEventListener(e, update); layer.remove(); } });
    update();
  }
  function detach(m) { const x = on.get(m); if (x) { on.delete(m); x.stop(); } }
  function scan() {
    if (stopped) return;
    const found = [...host.querySelectorAll(sel)];
    if (host.matches && host.matches(sel)) found.unshift(host);
    for (const m of found) if (!on.has(m)) attach(m);
    for (const m of [...on.keys()]) if (!m.isConnected || !(m === host || host.contains(m))) detach(m);
  }
  const hostObs = new MutationObserver(scan);
  hostObs.observe(host, { childList: true, subtree: true });
  scan();
  const w = {
    refresh() { if (!host.isConnected) { w.stop(); return; } scan(); for (const x of on.values()) x.update(); },
    stop() { stopped = true; hostObs.disconnect(); for (const m of [...on.keys()]) { const x = on.get(m); on.delete(m); x.stop(); } watchers.delete(w); },
  };
  watchers.add(w);
  return w;
}
