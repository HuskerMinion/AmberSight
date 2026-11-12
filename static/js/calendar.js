// A small month calendar for the timeline's "Go to": days with recordings are
// tinted, days with alerts get a dot. Days are the browser's own local days;
// their start times go to the server, which only looks them up.
import { api, esc } from './core.js';

const DAY = 86400000;
const TTL = 30000;
let el = null, st = null;
const cache = new Map();

const dstart = (y, m, d) => new Date(y, m, d).getTime();
const key = (y, m, d) => y + '-' + m + '-' + d;

// First day of the week for the browser's language: 1 Monday .. 7 Sunday.
function firstDay() {
  try {
    const l = new Intl.Locale(navigator.language);
    const w = l.getWeekInfo ? l.getWeekInfo() : l.weekInfo;
    if (w && w.firstDay) return w.firstDay;
  } catch { /* older browsers */ }
  return 1;
}

export function closeCalendar(refocus) {
  if (!el) return;
  const s = st;
  window.removeEventListener('keydown', s.onKey, true);
  document.removeEventListener('pointerdown', s.onDown, true);
  window.removeEventListener('resize', s.onSize);
  window.removeEventListener('hashchange', s.onNav);
  el.remove();
  el = null; st = null;
  if (refocus && s.anchor && s.anchor.isConnected) s.anchor.focus();
  if (s.anchor) s.anchor.setAttribute('aria-expanded', 'false');
}
export const calendarOpen = () => !!el;

// opts: anchor (button), selected (ms of the day shown), cameras() -> names,
// onPick(y, m, d) (month 0-11).
export function openCalendar(opts) {
  if (el) { closeCalendar(true); return; }
  const sel = new Date(opts.selected || Date.now());
  const fd = firstDay();
  st = {
    anchor: opts.anchor, opts, fd,
    year: sel.getFullYear(), month: sel.getMonth(),
    focus: { y: sel.getFullYear(), m: sel.getMonth(), d: sel.getDate() },
    sel: { y: sel.getFullYear(), m: sel.getMonth(), d: sel.getDate() },
    oldest: null, seq: 0,
  };
  el = document.createElement('div');
  el.className = 'cal';
  el.setAttribute('role', 'dialog');
  el.setAttribute('aria-label', 'Choose a day');
  document.body.appendChild(el);
  opts.anchor.setAttribute('aria-expanded', 'true');
  st.onKey = (ev) => keyDown(ev);
  st.onDown = (ev) => { if (el && !el.contains(ev.target) && !st.anchor.contains(ev.target)) closeCalendar(false); };
  st.onSize = () => place();
  st.onNav = () => closeCalendar(false);   // another page (Back, a tab shortcut)
  window.addEventListener('hashchange', st.onNav);
  window.addEventListener('keydown', st.onKey, true);
  document.addEventListener('pointerdown', st.onDown, true);
  window.addEventListener('resize', st.onSize);
  render();
  load();
  focusDay();
}

function monthCells() {
  const { year, month, fd } = st;
  const first = new Date(year, month, 1);
  const lead = (first.getDay() - (fd % 7) + 7) % 7; // getDay: 0 Sunday
  const cells = [];
  for (let i = 0; i < 42; i++) {
    const t = new Date(year, month, 1 - lead + i);
    cells.push({ y: t.getFullYear(), m: t.getMonth(), d: t.getDate(), in: t.getMonth() === month });
  }
  return cells;
}

function camsKey() { return (st.opts.cameras() || []).join('\u0001'); }

// The cameras' days for the 6 weeks on screen (days after today have nothing).
function load() {
  const cells = monthCells();
  const now = Date.now();
  const cams = st.opts.cameras() || [];
  // (A cached month drawn now must not be overwritten by an earlier month's late answer.)
  const seq = ++st.seq;
  const k = cells[0].y + '-' + cells[0].m + '-' + cells[0].d + '|' + camsKey();
  const hit = cache.get(k);
  if (hit && now - hit.at < TTL) { apply(hit.data); return; }
  const days = cells.filter(c => dstart(c.y, c.m, c.d) <= now);
  if (!days.length) { st.data = new Map(); paint(); return; }
  const last = days[days.length - 1];
  const starts = days.map(c => dstart(c.y, c.m, c.d));
  const end = dstart(last.y, last.m, last.d + 1);
  const s = st;
  s.loading = true;
  el.setAttribute('aria-busy', 'true');
  api('/api/recordings/days?starts=' + starts.join(',') + '&end=' + end + (cams.length ? '&cameras=' + encodeURIComponent(cams.join(',')) : ''))
    .then(data => {
      if (st !== s || seq !== s.seq) return;
      cache.set(k, { at: Date.now(), data: { cells: days, res: data } });
      apply({ cells: days, res: data });
    })
    .catch(() => { if (st === s && seq === s.seq) { s.loading = false; if (el) el.removeAttribute('aria-busy'); } });
}
function apply({ cells, res }) {
  st.loading = false;
  el.removeAttribute('aria-busy');
  st.data = new Map();
  (res.days || []).forEach((r, i) => { const c = cells[i]; if (c) st.data.set(key(c.y, c.m, c.d), r); });
  st.oldest = res.oldest == null ? null : res.oldest;
  paint();
}

function render() {
  const { year, month, fd } = st;
  const mname = new Intl.DateTimeFormat(undefined, { month: 'long', year: 'numeric' }).format(new Date(year, month, 1));
  const wd = [];
  for (let i = 0; i < 7; i++) {
    const t = new Date(2024, 0, 7 + ((fd % 7) + i)); // 2024-01-07 is a Sunday
    wd.push([new Intl.DateTimeFormat(undefined, { weekday: 'narrow' }).format(t), new Intl.DateTimeFormat(undefined, { weekday: 'long' }).format(t)]);
  }
  el.innerHTML =
    '<div class="calhd">' +
      '<button type="button" class="calnav" data-nav="-1" aria-label="Previous month">&#8249;</button>' +
      '<div class="caltitle" aria-live="polite">' + esc(mname) + '</div>' +
      '<button type="button" class="calnav" data-nav="1" aria-label="Next month">&#8250;</button>' +
    '</div>' +
    '<div class="calgrid" role="grid" aria-label="' + esc(mname) + '">' +
      '<div class="calrow calwd" role="row">' + wd.map(([n, l]) => '<span role="columnheader" aria-label="' + esc(l) + '">' + esc(n) + '</span>').join('') + '</div>' +
      '<div class="calbody"></div>' +
    '</div>' +
    '<div class="calft">' +
      '<span class="calkey"><i class="calsw rec"></i>Recorded</span>' +
      '<span class="calkey"><i class="caldot"></i>Alerts</span>' +
      '<button type="button" class="calnow">Today</button>' +
    '</div>';
  el.querySelectorAll('.calnav').forEach(b => { b.onclick = () => shift(+b.dataset.nav); });
  el.querySelector('.calnow').onclick = () => { const t = new Date(); go(t.getFullYear(), t.getMonth(), t.getDate()); };
  paint();
  place();
}

function paint() {
  if (!el) return;
  const cells = monthCells();
  const today = new Date();
  const tk = key(today.getFullYear(), today.getMonth(), today.getDate());
  const body = el.querySelector('.calbody');
  // Redrawing replaces the day buttons: a focused one gets its focus back below.
  const hadFocus = body.contains(document.activeElement);
  const fmt = new Intl.DateTimeFormat(undefined, { weekday: 'long', day: 'numeric', month: 'long', year: 'numeric' });
  let html = '';
  for (let r = 0; r < 6; r++) {
    html += '<div class="calrow" role="row">';
    for (let c = 0; c < 7; c++) {
      const x = cells[r * 7 + c];
      const k = key(x.y, x.m, x.d);
      const info = st.data && st.data.get(k);
      const future = dstart(x.y, x.m, x.d) > Date.now();
      const isFocus = x.y === st.focus.y && x.m === st.focus.m && x.d === st.focus.d;
      const isSel = x.y === st.sel.y && x.m === st.sel.m && x.d === st.sel.d;
      const cls = 'calday' + (x.in ? '' : ' out') + (info && info.rec ? ' rec' : '') + (info && info.alerts ? ' alert' : '') + (k === tk ? ' today' : '') + (isSel ? ' sel' : '') + (future ? ' future' : '');
      let label = fmt.format(new Date(x.y, x.m, x.d));
      if (info) label += info.rec ? ', recorded' : ', nothing recorded';
      if (info && info.alerts) label += ', ' + (info.alerts >= 100 ? 'many' : info.alerts) + (info.alerts === 1 ? ' alert' : ' alerts');
      html += '<span role="gridcell" aria-selected="' + isSel + '"><button type="button" class="' + cls + '" data-y="' + x.y + '" data-m="' + x.m + '" data-d="' + x.d + '" tabindex="' + (isFocus ? 0 : -1) + '"' +
        (k === tk ? ' aria-current="date"' : '') + ' aria-label="' + esc(label) + '">' + x.d + '</button></span>';
    }
    html += '</div>';
  }
  body.innerHTML = html;
  body.querySelectorAll('.calday').forEach(b => { b.onclick = () => pick(+b.dataset.y, +b.dataset.m, +b.dataset.d); });
  if (hadFocus) focusDay();
  // Arrows stop where there is nothing to look at.
  const prev = el.querySelector('[data-nav="-1"]'), next = el.querySelector('[data-nav="1"]');
  const n = new Date();
  next.disabled = st.year > n.getFullYear() || (st.year === n.getFullYear() && st.month >= n.getMonth());
  // Back always works (three years at most): months before the oldest recording just show
  // nothing recorded, rather than an arrow that seems broken.
  prev.disabled = (n.getFullYear() - st.year) * 12 + (n.getMonth() - st.month) >= 36;
}

function place() {
  if (!el || !st.anchor.isConnected) return;
  const a = st.anchor.getBoundingClientRect();
  const vw = document.documentElement.clientWidth, vh = document.documentElement.clientHeight;
  const w = el.offsetWidth, h = el.offsetHeight;
  let left = Math.min(Math.max(8, a.left), vw - w - 8);
  if (left < 8) left = 8;
  const above = a.top - h - 6;
  const top = above >= 8 ? above : Math.min(Math.max(8, a.bottom + 6), Math.max(8, vh - h - 8));
  el.style.left = left + 'px';
  el.style.top = top + 'px';
}

function focusDay() {
  if (!el) return;
  const f = st.focus;
  const b = el.querySelector('.calday[data-y="' + f.y + '"][data-m="' + f.m + '"][data-d="' + f.d + '"]');
  if (b) b.focus({ preventScroll: true });
}

function shift(n) {
  const t = new Date(st.year, st.month + n, 1);
  const onNav = el.contains(document.activeElement) && document.activeElement.classList.contains('calnav');
  go(t.getFullYear(), t.getMonth(), Math.min(st.focus.d, new Date(t.getFullYear(), t.getMonth() + 1, 0).getDate()), onNav ? n : 0);
}
// Show the month holding this day, with it focused (nav: the arrow button that asked keeps focus).
function go(y, m, d, nav) {
  const t = new Date(y, m, d);
  if (dstart(t.getFullYear(), t.getMonth(), t.getDate()) > Date.now()) return;
  st.focus = { y: t.getFullYear(), m: t.getMonth(), d: t.getDate() };
  if (t.getFullYear() !== st.year || t.getMonth() !== st.month) {
    st.year = t.getFullYear(); st.month = t.getMonth();
    st.data = null;
    render();
    load();
  } else paint();
  const nb = nav && el.querySelector('[data-nav="' + nav + '"]');
  if (nb && !nb.disabled) nb.focus(); else focusDay();
}

function pick(y, m, d) {
  const cb = st.opts.onPick;
  closeCalendar(true);
  cb(y, m, d);
}

function keyDown(ev) {
  if (!el) return;
  // (Focus lost to the page while the grid redrew counts as inside: the keys stay here.)
  const lost = !el.contains(ev.target) && ev.target === document.body;
  if (lost) focusDay();
  else if (!el.contains(ev.target)) {
    if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); closeCalendar(true); }
    return;
  }
  const k = ev.key;
  const f = st.focus;
  const at = lost ? document.activeElement : ev.target;
  const onDay = !!(at && at.classList && at.classList.contains('calday'));
  let handled = true;
  if (k === 'Escape') closeCalendar(true);
  else if (k === 'Tab') {
    // Keep Tab inside the popover.
    const items = [...el.querySelectorAll('button:not(:disabled):not([tabindex="-1"])')];
    const i = items.indexOf(at), n = items.length;
    if (n) { items[(i + (ev.shiftKey ? n - 1 : 1)) % n].focus(); }
  } else if (onDay && k === 'ArrowLeft') go(f.y, f.m, f.d - 1);
  else if (onDay && k === 'ArrowRight') go(f.y, f.m, f.d + 1);
  else if (onDay && k === 'ArrowUp') go(f.y, f.m, f.d - 7);
  else if (onDay && k === 'ArrowDown') go(f.y, f.m, f.d + 7);
  else if (onDay && k === 'Home') { const wd = (new Date(f.y, f.m, f.d).getDay() - (st.fd % 7) + 7) % 7; go(f.y, f.m, f.d - wd); }
  else if (onDay && k === 'End') { const wd = (new Date(f.y, f.m, f.d).getDay() - (st.fd % 7) + 7) % 7; go(f.y, f.m, f.d + 6 - wd); }
  else if (lost && onDay && (k === 'Enter' || k === ' ')) pick(f.y, f.m, f.d);
  else if (k === 'PageUp') shift(-1);
  else if (k === 'PageDown') shift(1);
  else handled = false;
  // Keys the timeline would act on (arrows seek, Space plays) stay here.
  if (handled || k.length === 1 || k.startsWith('Arrow')) ev.stopPropagation();
  if (handled) ev.preventDefault();
}
