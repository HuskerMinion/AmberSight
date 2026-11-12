// arrange.js - drag-and-drop ordering of the live tiles.
//
// enableArrange(grid, onChange) turns on dragging for every `.tile[data-name]` child of
// `grid`. Mouse: press and drag. Touch: press and hold ~0.35 s, then drag (a quick swipe
// still scrolls). Keyboard: focus a tile, Shift+Arrow moves it one place.
// While dragging, the tile follows the pointer and a dashed placeholder (the "gap") shows
// exactly where it will land; the other tiles slide aside (FLIP animation). Esc while
// dragging puts the tile back. onChange(names) is called after every completed move.

const HOLD_MS = 350;
const SLOP = 6;

export function enableArrange(grid, onChange) {
  let press = null;     // pointer is down on a tile, drag not started yet
  let drag = null;      // active drag
  let raf = 0;

  const tiles = () => [...grid.children].filter(c => c.classList.contains('tile'));
  const names = () => tiles().map(t => t.dataset.name);

  function flip(mutate) {
    const kids = [...grid.children].filter(c => c !== (drag && drag.tile));
    const before = new Map(kids.map(k => [k, k.getBoundingClientRect()]));
    mutate();
    for (const k of kids) {
      const a = before.get(k), b = k.getBoundingClientRect();
      const dx = a.left - b.left, dy = a.top - b.top;
      if (!dx && !dy) continue;
      k.style.transition = 'none';
      k.style.transform = 'translate(' + dx + 'px,' + dy + 'px)';
    }
    void grid.offsetWidth;
    for (const k of kids) {
      if (!k.style.transform) continue;
      k.style.transition = 'transform .18s ease';
      k.style.transform = '';
    }
  }

  // Lift `tile`, grabbed at (gx, gy); the pointer is now at (cx, cy).
  function begin(tile, gx, gy, cx, cy) {
    const r = tile.getBoundingClientRect();
    const ph = document.createElement('div');
    ph.className = 'tile-ph';
    ph.style.width = r.width + 'px';
    ph.style.height = r.height + 'px';
    ph.style.gridColumn = tile.style.gridColumn;
    drag = { tile, ph, ox: gx - r.left, oy: gy - r.top, startNext: tile.nextSibling, cx, cy };
    grid.insertBefore(ph, tile);
    tile.classList.add('lifted');
    tile.style.width = r.width + 'px';
    tile.style.height = r.height + 'px';
    // The lifted tile floats (position: fixed); park it at the end so position-based
    // styling (the large tiles of the Focus views) follows the gap, not the lifted tile.
    grid.appendChild(tile);
    grid.classList.add('dragging');
    document.body.classList.add('arrange-drag');
    if (navigator.vibrate) { try { navigator.vibrate(12); } catch (e) { /* ignore */ } }
    place();
    loop();
  }

  function retarget() {
    if (!drag) return;
    const { ph, tile } = drag;
    const x = drag.cx, y = drag.cy;
    const kids = [...grid.children].filter(c => c !== tile);
    let hit = null, best = Infinity;
    for (const k of kids) {
      const r = k.getBoundingClientRect();
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) { hit = k; best = 0; break; }
      const dx = Math.max(r.left - x, 0, x - r.right), dy = Math.max(r.top - y, 0, y - r.bottom);
      const d = dx * dx + dy * dy;
      if (d < best) { best = d; hit = k; }
    }
    if (!hit || hit === ph) return;
    const r = hit.getBoundingClientRect();
    // Move past the tile when the pointer is in its far half (by column, or by row when stacked).
    const sameRow = y >= r.top && y <= r.bottom;
    const after = sameRow ? x > r.left + r.width / 2 : y > r.bottom;
    const ref = after ? hit.nextSibling : hit;
    // Already in that spot?
    if (after && hit.nextSibling === ph) return;
    if (!after && ph.nextSibling === hit) return;
    flip(() => grid.insertBefore(ph, ref));
  }

  function place() {
    if (!drag) return;
    drag.tile.style.left = (drag.cx - drag.ox) + 'px';
    drag.tile.style.top = (drag.cy - drag.oy) + 'px';
  }

  function loop() {
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(() => {
      if (!drag) return;
      const h = window.innerHeight;
      const edge = 70;
      let dy = 0;
      if (drag.cy < edge) dy = -Math.ceil((edge - drag.cy) / 6);
      else if (drag.cy > h - edge) dy = Math.ceil((drag.cy - (h - edge)) / 6);
      if (dy) { window.scrollBy(0, dy); retarget(); }
      loop();
    });
  }

  function finish(cancel) {
    if (!drag) return;
    const { tile, ph, startNext } = drag;
    cancelAnimationFrame(raf);
    if (cancel) {
      flip(() => grid.insertBefore(ph, startNext && startNext !== tile ? startNext : null));
    }
    const target = ph.getBoundingClientRect();
    drag = null;
    tile.style.transition = 'left .16s ease, top .16s ease, box-shadow .16s ease';
    tile.style.left = target.left + 'px';
    tile.style.top = target.top + 'px';
    const done = () => {
      tile.removeEventListener('transitionend', done);
      clearTimeout(t);
      tile.classList.remove('lifted');
      for (const p of ['width', 'height', 'left', 'top', 'transition']) tile.style[p] = '';
      grid.insertBefore(tile, ph);
      ph.remove();
      grid.classList.remove('dragging');
      document.body.classList.remove('arrange-drag');
      if (!cancel) onChange(names());
    };
    const t = setTimeout(done, 260);
    tile.addEventListener('transitionend', done);
  }

  const onDown = (ev) => {
    if (drag || (ev.pointerType === 'mouse' && ev.button !== 0)) return;
    const tile = ev.target.closest && ev.target.closest('.tile');
    if (!tile || !grid.contains(tile)) return;
    press = { tile, x: ev.clientX, y: ev.clientY, id: ev.pointerId, type: ev.pointerType, timer: 0, ev };
    if (ev.pointerType !== 'mouse') {
      press.timer = setTimeout(() => {
        const p = press;
        if (!p) return;
        press = null;
        begin(p.tile, p.x, p.y, p.x, p.y);
      }, HOLD_MS);
    }
  };
  const onMove = (ev) => {
    if (drag) {
      drag.cx = ev.clientX; drag.cy = ev.clientY;
      place();
      retarget();
      ev.preventDefault();
      return;
    }
    if (!press || ev.pointerId !== press.id) return;
    const dx = ev.clientX - press.x, dy = ev.clientY - press.y;
    if (Math.abs(dx) < SLOP && Math.abs(dy) < SLOP) return;
    if (press.type === 'mouse') {
      const p = press; press = null;
      begin(p.tile, p.x, p.y, ev.clientX, ev.clientY);
      retarget();
    } else {
      // Touch moved before the hold finished: the user is scrolling.
      clearTimeout(press.timer); press = null;
    }
  };
  const onUp = (ev) => {
    if (press) { clearTimeout(press.timer); press = null; }
    if (drag) { ev.preventDefault && ev.preventDefault(); finish(ev.type === 'pointercancel'); }
  };
  const onTouchMove = (ev) => { if (drag && ev.cancelable) ev.preventDefault(); };
  const onKey = (ev) => {
    if (drag && ev.key === 'Escape') { ev.stopPropagation(); ev.preventDefault(); finish(true); return; }
    if (!ev.shiftKey || (ev.key !== 'ArrowLeft' && ev.key !== 'ArrowRight')) return;
    const tile = ev.target.closest && ev.target.closest('.tile');
    if (!tile || !grid.contains(tile)) return;
    const list = tiles();
    const i = list.indexOf(tile);
    const j = i + (ev.key === 'ArrowLeft' ? -1 : 1);
    if (j < 0 || j >= list.length) return;
    ev.preventDefault();
    flip(() => grid.insertBefore(tile, ev.key === 'ArrowLeft' ? list[j] : list[j].nextSibling));
    tile.focus();
    onChange(names());
  };
  const noMenu = (ev) => ev.preventDefault();

  grid.addEventListener('pointerdown', onDown);
  window.addEventListener('pointermove', onMove, { passive: false });
  window.addEventListener('pointerup', onUp);
  window.addEventListener('pointercancel', onUp);
  window.addEventListener('touchmove', onTouchMove, { passive: false });
  window.addEventListener('keydown', onKey, true);
  grid.addEventListener('contextmenu', noMenu);

  return {
    dragging: () => !!drag,
    cancelDrag: () => { if (drag) finish(true); },
    disable() {
      if (drag) finish(true);
      clearTimeout(press && press.timer); press = null;
      grid.removeEventListener('pointerdown', onDown);
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
      window.removeEventListener('touchmove', onTouchMove);
      window.removeEventListener('keydown', onKey, true);
      grid.removeEventListener('contextmenu', noMenu);
      grid.classList.remove('dragging');
      document.body.classList.remove('arrange-drag');
    },
  };
}
