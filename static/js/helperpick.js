// helperpick.js - "Another computer" camera source: pick a paired capture helper,
// then one of its screens, windows or a region (with live thumbnails), and pair new
// computers. See docs/helper-protocol.md.

import { api, esc, openDlg, toast } from './core.js';

const field = (label, input, hint) => '<label class="cs-f"><span>' + label + '</span>' + input + '</label>' + (hint ? '<div class="hint">' + hint + '</div>' : '');
const sameSource = (a, b) => !!a && !!b && JSON.stringify(a) === JSON.stringify(b);

export function helperHtml(c) {
  const cap = c.capture || {};
  return '<div data-src="helper"' + (c.capture ? '' : ' hidden') + '>' +
    field('Computer', '<span class="cs-ovin"><select data-k="hlp"><option value="">loading...</option></select>' +
      '<button type="button" class="cs-reset" data-pairnew>Add a computer...</button></span>',
      'A computer running AmberSight Capture sends what is on its screen. It connects to this server, so nothing has to be opened on its side.') +
    '<div class="sec">What to capture</div><div class="hp-grid" data-hlpsrc><p class="hint">Pick a computer first.</p></div>' +
    '<input type="hidden" data-k="hlp_source" value="' + esc(cap.source ? JSON.stringify(cap.source) : '') + '">' +
    '<details class="hp-region"><summary>Part of the screen instead</summary>' +
      '<div class="hint">x, y, width, height in screen pixels (for example 0,0,1280,720).</div>' +
      field('Region', '<input data-k="hlp_region" placeholder="x,y,width,height" spellcheck="false" value="' + esc(cap.source && cap.source.kind === 'region' ? [cap.source.x, cap.source.y, cap.source.w, cap.source.h].join(',') : '') + '">') +
    '</details>' +
    field('Frame rate', '<input data-k="hlp_fps" type="number" min="1" max="30" value="' + (cap.fps || 10) + '">') +
    field('Max bitrate (kbit/s)', '<input data-k="hlp_kbps" type="number" min="200" max="20000" step="100" value="' + (cap.max_kbps || 2500) + '">', 'Lower for computers on a slow link (Tailscale from another site).') +
    field('Max width (px)', '<input data-k="hlp_width" type="number" min="320" max="3840" step="16" value="' + (cap.max_width || 1920) + '">') +
    (c.capture_status ? '<div class="hint">Right now: ' + esc(c.capture_status.state || '') + (c.capture_status.error ? ' (' + esc(c.capture_status.error) + ')' : '') + '</div>' : '') +
    '</div>';
}

export function wireHelper(d, c) {
  const sel = d.querySelector('[data-k="hlp"]');
  const grid = d.querySelector('[data-hlpsrc]');
  const hidden = d.querySelector('[data-k="hlp_source"]');
  const region = d.querySelector('[data-k="hlp_region"]');
  let helpers = [];
  let loadedFor = null;
  const want = c.capture ? c.capture.helper : '';

  async function loadHelpers(select) {
    try { helpers = (await api('/api/helpers')).helpers || []; } catch (e) { helpers = []; }
    sel.innerHTML = (helpers.length ? '' : '<option value="">no computers yet: add one</option>') +
      helpers.map(h => '<option value="' + esc(h.id) + '">' + esc(h.name) + (h.online ? '' : ' (offline)') + '</option>').join('');
    const pick = select || want || (helpers[0] && helpers[0].id) || '';
    if (pick) sel.value = pick;
    loadSources();
  }

  async function loadSources() {
    const id = sel.value;
    if (!id) { grid.innerHTML = '<p class="hint">Add a computer to choose what to capture.</p>'; return; }
    if (loadedFor === id) return;
    loadedFor = id;
    const h = helpers.find(x => x.id === id);
    if (h && !h.online) { grid.innerHTML = '<p class="hint">' + esc(h.name) + ' is not connected right now. Its saved choice is kept.</p>'; loadedFor = null; return; }
    grid.innerHTML = '<p class="hint">Asking ' + esc(h ? h.name : 'the computer') + ' what it can show...</p>';
    let r;
    try { r = await api('/api/helpers/' + encodeURIComponent(id) + '/sources', { method: 'POST' }); }
    catch (e) { grid.innerHTML = '<p class="hint">Could not ask it: ' + esc(e.body || e.message) + '</p>'; loadedFor = null; return; }
    const items = [{ label: 'Whole desktop', source: { kind: 'desktop' } }]
      .concat((r.monitors || []).map(m => ({ label: (m.name || 'Monitor ' + (m.index + 1)) + (m.primary ? ' (main)' : '') + (m.w && m.h ? ' ' + m.w + 'x' + m.h : ''), source: { kind: 'monitor', index: m.index } })))
      .concat((r.windows || []).map(w => ({ label: w.title + (w.exe ? ' - ' + w.exe : ''), source: { kind: 'window', title: w.title, exe: w.exe || undefined } })));
    let cur = null;
    try { cur = hidden.value ? JSON.parse(hidden.value) : null; } catch (e) { cur = null; }
    grid.innerHTML = items.map((it, i) => '<button type="button" class="hp-item' + (sameSource(it.source, cur) ? ' on' : '') + '" data-i="' + i + '" title="' + esc(it.label) + '">' +
      '<span class="hp-thumb"><img alt="" loading="lazy"></span><span class="hp-label">' + esc(it.label) + '</span></button>').join('');
    grid.querySelectorAll('.hp-item').forEach(b => {
      b.onclick = () => {
        grid.querySelectorAll('.hp-item').forEach(x => x.classList.toggle('on', x === b));
        hidden.value = JSON.stringify(items[+b.dataset.i].source);
        if (region) region.value = '';
      };
    });
    // Thumbnails, two at a time (each is a fresh grab on that computer).
    const queue = items.map((it, i) => [it, grid.querySelector('.hp-item[data-i="' + i + '"] img')]);
    const worker = async () => {
      while (queue.length && d.isConnected && loadedFor === id) {
        const [it, img] = queue.shift();
        try {
          const res = await fetch('/api/helpers/' + encodeURIComponent(id) + '/thumb', { method: 'POST', credentials: 'same-origin', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ source: it.source, width: 240 }) });
          if (res.ok) img.src = URL.createObjectURL(await res.blob()); else img.parentElement.classList.add('none');
        } catch (e) { img.parentElement.classList.add('none'); }
      }
    };
    worker(); worker();
  }

  sel.onchange = () => { loadedFor = null; loadSources(); };
  d.querySelector('[data-pairnew]').onclick = () => pairDialog((id) => loadHelpers(id));
  return { load: () => { if (!helpers.length) loadHelpers(); } };
}

// The capture settings from the form; null when incomplete.
export function readHelper(d) {
  const v = (k) => { const el = d.querySelector('[data-k="' + k + '"]'); return el ? el.value.trim() : ''; };
  const helper = v('hlp');
  let source = null;
  const reg = v('hlp_region').split(',').map(x => parseInt(x, 10));
  if (reg.length === 4 && reg.every(n => Number.isFinite(n)) && reg[2] > 0 && reg[3] > 0) source = { kind: 'region', x: reg[0], y: reg[1], w: reg[2], h: reg[3] };
  else { try { source = v('hlp_source') ? JSON.parse(v('hlp_source')) : null; } catch (e) { source = null; } }
  if (!helper || !source) return null;
  return {
    helper, source,
    fps: Math.max(1, Math.min(30, parseInt(v('hlp_fps'), 10) || 10)),
    max_kbps: Math.max(200, Math.min(20000, parseInt(v('hlp_kbps'), 10) || 2500)),
    max_width: Math.max(320, Math.min(3840, parseInt(v('hlp_width'), 10) || 1920)),
  };
}

// Pair a new computer: one-time code, downloads, the command to run, and the
// certificate fingerprint to compare. Calls done(id) once it connects.
export async function pairDialog(done) {
  let r, tls = {}, before = [];
  try { r = await api('/api/helpers/pair', { method: 'POST' }); } catch (e) { toast('Could not make a code: ' + (e.body || e.message)); return; }
  try { tls = await api('/api/tls'); } catch (e) { /* fingerprint shown when known */ }
  try { before = ((await api('/api/helpers')).helpers || []).map(h => h.id); } catch (e) { /* none */ }
  const origin = location.origin;
  const dl = (f) => origin + '/helper/download/' + f + '?code=' + encodeURIComponent(r.code);
  const cmd = 'ambersight.exe capture --pair ' + origin + ' ' + r.code;
  const row = (label, val, link) => '<div class="cs-pushrow"><span>' + label + '</span><code>' + esc(val) + '</code>' +
    (link ? '<a class="cs-reset" href="' + esc(val) + '" download>Get</a>' : '<button type="button" class="cs-reset" data-copytext="' + esc(val) + '">Copy</button>') + '</div>';
  const d = openDlg('Add a computer',
    '<div class="hint">Do this on the computer whose screen you want to record. The code works for 15 minutes.</div>' +
    '<div class="sec">1. Download (on that computer)</div><div class="cs-push">' +
      row('Helper', dl('ambersight.exe'), true) + row('ffmpeg', dl('ffmpeg.exe'), true) + '</div>' +
    '<div class="hint">Put both in one folder, e.g. C:\\AmberSight Capture. Windows may warn that the program is not recognized: choose More info, then Run anyway. Skip ffmpeg if that computer already has it.</div>' +
    '<div class="sec">2. Run this in that folder (Command Prompt)</div><div class="cs-push">' + row('Command', cmd) + '</div>' +
    '<div class="hint">It shows a certificate fingerprint: it must match this one.</div>' +
    '<code class="tlsfp">' + esc(tls.ca_fingerprint_sha256 || '(this server has no certificate yet)') + '</code>' +
    '<div class="sec">3. Wait here</div><div class="hp-wait" data-wait>Waiting for the computer to connect...</div>' +
    '<div class="row"><button class="btn" data-close>Close</button></div>', true);
  d.classList.add('wide');
  d.addEventListener('click', async (e) => {
    const b = e.target.closest('[data-copytext]'); if (!b) return;
    try { await navigator.clipboard.writeText(b.dataset.copytext); b.textContent = 'Copied'; setTimeout(() => { b.textContent = 'Copy'; }, 1200); }
    catch (err) { toast('Copy did not work here: select the text instead'); }
  });
  d.querySelector('[data-close]').onclick = () => d.__close();
  const until = Date.now() + r.expires_in * 1000;
  const poll = async () => {
    if (!d.isConnected) return;
    if (Date.now() > until) { d.querySelector('[data-wait]').textContent = 'The code has expired. Close this and add the computer again.'; return; }
    try {
      const hs = (await api('/api/helpers')).helpers || [];
      const n = hs.find(h => !before.includes(h.id));
      if (n) {
        d.querySelector('[data-wait]').innerHTML = '<b>Connected: ' + esc(n.name) + '</b>. It will start with that computer from now on.';
        toast(n.name + ' added');
        if (done) done(n.id);
        return;
      }
    } catch (e) { /* keep waiting */ }
    setTimeout(poll, 2000);
  };
  poll();
}

// Settings > Cameras: the paired computers, with rename and remove.
export function helpersListHtml(list) {
  if (!list.length) return '';
  return '<div class="sec">Computers sending their screen</div><div class="cs-list">' + list.map(h =>
    '<div class="cs-item"><div class="cs-name"><b>' + esc(h.name) + '</b><div class="cs-url">' + (h.online ? 'connected' : 'not connected') + (h.version ? ' - version ' + esc(h.version) : '') + '</div></div>' +
    '<span class="badge dot ' + (h.online ? 'on' : 'off') + '">' + (h.online ? 'online' : 'offline') + '</span>' +
    '<button class="btn" data-hren="' + esc(h.id) + '">Rename</button><button class="btn danger" data-hdel="' + esc(h.id) + '">Remove</button></div>').join('') + '</div>';
}
