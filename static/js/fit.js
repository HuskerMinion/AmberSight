// fit.js - where a picture sits inside its box (CSS object-fit / object-position),
// so the camera's drawn text can be placed on the picture itself and not on
// the letterbox around it. No imports.

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
