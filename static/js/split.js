// split.js - Split view: two Live panes side by side (or one above the other),
// each showing one server's cameras with its own layout, group filter and full
// view. Off by default; offered once another server's cameras are shown here.
//
//  * /?split=1                 the split page: two panes, a draggable divider.
//  * /?pane=a&site=<site>      one pane (inside the split page): Live only, the
//                              cameras of <site> ("here" = this server's own,
//                              or a linked server's id), choices kept per pane.
//
// Settings (direction, divider position, each pane's server) are kept in this
// browser: localStorage "ambs.split".

const KEY = 'ambs.split';
const qp = new URLSearchParams(location.search);

export const paneId = /^[ab]$/.test(qp.get('pane') || '') ? qp.get('pane') : null;
export const paneSite = paneId ? (qp.get('site') || 'here') : null;
export const isSplit = qp.get('split') === '1';

export function loadSplit() {
  let s = {};
  try { s = JSON.parse(localStorage.getItem(KEY) || '{}') || {}; } catch (e) { s = {}; }
  return {
    dir: s.dir === 'col' ? 'col' : 'row',
    ratio: Math.min(0.85, Math.max(0.15, Number(s.ratio) || 0.5)),
    sites: Object.assign({ a: 'here', b: '' }, s.sites || {}),
  };
}

export function saveSplit(s) {
  try { localStorage.setItem(KEY, JSON.stringify(s)); } catch (e) { /* storage off: not remembered */ }
}

// Which cameras a pane shows.
export function paneFilter(cams) {
  if (!paneId) return cams;
  if (paneSite === 'here') return cams.filter(c => !c.remote);
  return cams.filter(c => c.remote && c.remote.link === paneSite);
}

// Pane-only preference keys live in this browser, per pane.
const PANE_KEYS = ['activeLayout', 'quality', 'boxes', 'cycle', 'viewStyles', 'dock', 'startTab'];
export function paneKey(key) { return paneId && PANE_KEYS.includes(key) ? 'ambs.pane.' + paneId + '.' + key : null; }

// The split page itself (no app inside: each pane is the app).
export function startSplitPage() {
  const s = loadSplit();
  document.body.classList.add('splitpage');
  document.body.innerHTML =
    '<div class="sp-bar"><span class="sp-title"><b>Amber</b>Sight split view</span><span class="sp-sp"></span>' +
      '<button class="btn" id="spFlip" title="Side by side, or one above the other"></button>' +
      '<button class="btn" id="spClose">Close split view</button></div>' +
    '<div class="sp-wrap" id="spWrap"><iframe id="spA" title="Pane 1"></iframe><div class="sp-div" id="spDiv" role="separator" tabindex="0" aria-label="Drag to resize the panes"></div><iframe id="spB" title="Pane 2"></iframe></div>';
  const wrap = document.getElementById('spWrap');
  const a = document.getElementById('spA'), b = document.getElementById('spB');
  const src = (p) => '/?pane=' + p + '&site=' + encodeURIComponent(s.sites[p] || 'here') + '#live';
  a.src = src('a');
  b.src = src('b');
  const layout = () => {
    wrap.classList.toggle('col', s.dir === 'col');
    const pct = (s.ratio * 100).toFixed(2);
    a.style.flex = '0 0 calc(' + pct + '% - 3px)';
    b.style.flex = '1 1 0';
    document.getElementById('spFlip').textContent = s.dir === 'col' ? 'Side by side' : 'One above the other';
  };
  layout();
  document.getElementById('spFlip').onclick = () => { s.dir = s.dir === 'col' ? 'row' : 'col'; saveSplit(s); layout(); };
  document.getElementById('spClose').onclick = () => { location.href = '/#live'; };
  // Divider: drag (mouse/touch) or arrow keys.
  const div = document.getElementById('spDiv');
  div.addEventListener('pointerdown', (e) => {
    e.preventDefault();
    div.setPointerCapture(e.pointerId);
    wrap.classList.add('dragging');   // iframes ignore the pointer while dragging
    const r = wrap.getBoundingClientRect();
    const move = (ev) => {
      const v = s.dir === 'col' ? (ev.clientY - r.top) / r.height : (ev.clientX - r.left) / r.width;
      s.ratio = Math.min(0.85, Math.max(0.15, v));
      layout();
    };
    const up = () => { wrap.classList.remove('dragging'); div.removeEventListener('pointermove', move); div.removeEventListener('pointerup', up); saveSplit(s); };
    div.addEventListener('pointermove', move);
    div.addEventListener('pointerup', up);
  });
  div.addEventListener('keydown', (e) => {
    const d = { ArrowLeft: -0.02, ArrowUp: -0.02, ArrowRight: 0.02, ArrowDown: 0.02 }[e.key];
    if (!d) return;
    e.preventDefault();
    s.ratio = Math.min(0.85, Math.max(0.15, s.ratio + d));
    layout();
    saveSplit(s);
  });
  // A pane switched its server: remember it (the pane reloads itself).
  window.addEventListener('message', (e) => {
    if (e.origin !== location.origin || !e.data || e.data.type !== 'ambs-pane-site') return;
    if (e.data.pane === 'a' || e.data.pane === 'b') { s.sites[e.data.pane] = String(e.data.site || 'here'); saveSplit(s); }
  });
}

// Inside a pane: the server picker shown above Live.
export function mountPaneBar(cams, links) {
  if (!paneId) return;
  let bar = document.getElementById('paneBar');
  if (!bar) {
    bar = document.createElement('div');
    bar.id = 'paneBar';
    bar.className = 'pane-bar';
    document.body.prepend(bar);
  }
  const sites = [{ id: 'here', name: links.name || 'This server' }].concat((links.links || []).filter(l => l.id === paneSite || cams.some(c => c.remote && c.remote.link === l.id)).map(l => ({ id: l.id, name: l.name })));
  // The pane's server is always listed, even while it has no cameras here.
  if (!sites.some(x => x.id === paneSite)) sites.push({ id: paneSite, name: 'unavailable server' });
  bar.innerHTML = '<select aria-label="Server shown in this pane">' + sites.map(x => '<option value="' + x.id + '"' + (x.id === paneSite ? ' selected' : '') + '>' + x.name.replace(/[<>&"]/g, '') + '</option>').join('') + '</select>';
  bar.querySelector('select').onchange = (e) => {
    const site = e.target.value;
    try { window.parent.postMessage({ type: 'ambs-pane-site', pane: paneId, site }, location.origin); } catch (err) { /* not in the split page */ }
    location.href = '/?pane=' + paneId + '&site=' + encodeURIComponent(site) + '#live';
  };
}
