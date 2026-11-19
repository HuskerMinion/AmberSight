// sysmon.js - the server figures (CPU, memory, FPS, disk) in the Live page's View sidebar.
//
// Self-contained: builds its own DOM, styles live in the "server status pod" block of
// app.css, data comes from GET /api/stats. hostIn(el) puts the figures inside the dock's
// "Server figures" section; hostIn(null) takes them out. They are fetched only while they can
// be seen: docked, the section open, on Live, the browser tab visible. Two states are
// remembered in localStorage: 'tiles' and 'detail' (rows plus per-drive meters,
// per-camera fps, AmberSight's own CPU/RSS, uptime).

import { t } from './i18n.js';
import { api, fmtSize, esc } from './core.js';
import { paneId, paneSite } from './split.js';

// A split-view pane for another server shows that server's status.
const statsUrl = () => (paneId && paneSite && paneSite !== 'here' ? '/api/links/' + encodeURIComponent(paneSite) + '/stats?since=' : '/api/stats?since=');

const KEY = 'ambs.sysmon';
const N = 150;                 // samples kept (matches the server ring)
const W = 100, H = 28;         // sparkline viewBox

const ICONS = {
  ok: '<path d="M5 12.5l4.5 4.5L19 7.5"/>',
  warn: '<path d="M12 4l9 16H3z"/><path d="M12 10v4M12 17.2h.01"/>',
  crit: '<circle cx="12" cy="12" r="9"/><path d="M12 7.5v5M12 16.2h.01"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v5M12 7.8h.01"/>',
  moon: '<path d="M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z"/>',
  up: '<path d="M6 14l6-6 6 6"/>',
  down: '<path d="M6 10l6 6 6-6"/>',
  x: '<path d="M6 6l12 12M18 6L6 18"/>',
  pulse: '<path d="M3 12h4l2.5-6 4 12 2.5-6H21"/>',
};
const icon = (n, cls = '') => '<svg class="ic ' + cls + '" viewBox="0 0 24 24" aria-hidden="true">' + ICONS[n] + '</svg>';

let root, mode, timer = null, started = false, inflight = false, docked = false;
let since = 0, last = null;
let hist = emptyHist();
const els = {};                // cached nodes
let detailKey = '';
let tipTimer = 0;

function emptyHist() { return { cpu: [], process_cpu: [], mem: [], fps: [], disk: [], ai_p50: [], gpu: [] }; }

function loadMode() {
  let m = null;
  try { m = localStorage.getItem(KEY); } catch (e) { /* private mode */ }
  return m === 'detail' ? 'detail' : 'tiles';
}
function setMode(m) {
  mode = m === 'detail' ? 'detail' : 'tiles';
  m = mode;
  try { localStorage.setItem(KEY, m); } catch (e) { /* ignore */ }
  root.dataset.mode = m;
  root.querySelector('[data-act=detail]').innerHTML = icon(m === 'detail' ? 'down' : 'up');
  root.querySelector('[data-act=detail]').title = m === 'detail' ? t('sysmon.hide_details','Hide details') : t('sysmon.show_details','Show details');
  detailKey = '';
  if (last) render();
}

// ---------- build ----------
// One thin row per measure: label | meter (or, for FPS, a tiny trend line) | value,
// and a muted second line under it (detail, plus any warning with icon + label).
function tileHtml(k, label, spark) {
  return '<div class="sm-tile" data-k="' + k + '">' +
    '<div class="sm-line"><span class="sm-lbl">' + label + '</span>' +
    (spark
      ? '<div class="sm-spark" tabindex="0"><svg viewBox="0 0 ' + W + ' ' + H + '" preserveAspectRatio="none" aria-hidden="true">' +
        '<path class="a" d=""/><path class="l" d=""/></svg><i class="sm-rule"></i><i class="sm-dot"></i></div>'
      : '<div class="sm-meter" role="meter" aria-label="' + label + '" aria-valuemin="0" aria-valuemax="100"><i></i></div>') +
    '<span class="sm-val"><b>--</b><small></small></span></div>' +
    '<div class="sm-sub"><span class="sm-chip" hidden></span><span class="sm-cap">&nbsp;</span></div>' +
    '</div>';
}

function build() {
  if (!document.querySelector('link[href$="css/health.css"]')) {
    const l = document.createElement('link');
    l.rel = 'stylesheet'; l.href = '/static/css/health.css';
    document.head.appendChild(l);
  }
  root = document.createElement('aside');
  root.id = 'sysmon';
  root.className = 'sm';
  root.setAttribute('aria-label', t('sysmon.server_status','Server status'));
  root.innerHTML =
    '<div class="sm-panel" role="region" aria-label="' + esc(t('sysmon.server_status', 'Server status')) + '">' +
      '<div class="sm-head"><span class="sm-title">' + t('sysmon.server', 'Server') + '</span><span class="sm-up" data-p="up"></span>' +
        '<button class="sm-btn" data-act="detail" aria-label="' + esc(t('sysmon.toggle_details', 'Toggle details')) + '"></button></div>' +
      '<div class="sm-tiles">' +
        tileHtml('cpu', 'CPU', false) + tileHtml('mem', t('sysmon.memory', 'Memory'), false) +
        tileHtml('fps', 'FPS', true) + tileHtml('ai', 'AI', true) + tileHtml('gpu', 'GPU', true) + tileHtml('disk', t('sysmon.disk', 'Disk'), false) +
      '</div>' +
      '<div class="sm-detail"></div>' +
    '</div>' +
    '<div class="sm-tip" hidden></div>';
  // (Not in the page until hostIn() puts it in the dock.)

  for (const tl of root.querySelectorAll('.sm-tile')) {
    const k = tl.dataset.k;
    els[k] = {
      tile: tl, chip: tl.querySelector('.sm-chip'), val: tl.querySelector('.sm-val b'), unit: tl.querySelector('.sm-val small'),
      cap: tl.querySelector('.sm-cap'), spark: tl.querySelector('.sm-spark'), meter: tl.querySelector('.sm-meter'),
      area: tl.querySelector('.a'), line: tl.querySelector('.l'), rule: tl.querySelector('.sm-rule'), dot: tl.querySelector('.sm-dot'),
    };
    if (els[k].spark) bindSpark(k);
  }
  // GPU: memory bar under the trend; Disk: one bar per tier. Both hidden until there is data.
  els.gpu.tile.hidden = true; els.ai.tile.hidden = true;
  els.gpu.tile.insertAdjacentHTML('beforeend', '<div class="sm-meter hl-vram" role="meter" aria-label="' + esc(t('sysmon.video_memory', 'Video memory')) + '" aria-valuemin="0" aria-valuemax="100"><i></i></div>');
  els.gpu.vram = els.gpu.tile.querySelector('.hl-vram');
  els.disk.tile.insertAdjacentHTML('beforeend', '<div class="hl-tiers"></div>');
  els.disk.tiers = els.disk.tile.querySelector('.hl-tiers');
  for (const p of root.querySelectorAll('[data-p]')) els['p_' + p.dataset.p] = p;
  els.detail = root.querySelector('.sm-detail');
  els.tip = root.querySelector('.sm-tip');

  root.addEventListener('click', (e) => {
    const b = e.target.closest('[data-act]');
    if (b && b.dataset.act === 'detail') setMode(mode === 'detail' ? 'tiles' : 'detail');
  });
  setMode(loadMode());
}

// ---------- sparkline ----------
const SPARK = {
  cpu: { src: 'cpu', top: (v) => Math.max(25, Math.ceil(Math.max(...v) / 25) * 25), fmt: (v) => v.toFixed(1) + '%' },
  mem: { src: 'mem', top: () => 100, fmt: (v) => v.toFixed(1) + '%' },
  ai: { src: 'ai_p50', top: (v) => Math.max(500, Math.ceil(Math.max(...v) * 1.2 / 500) * 500), fmt: (v) => (v ? fmtMs(v) : t('sysmon.no_checks','no checks')) },
  gpu: { src: 'gpu', top: () => 100, fmt: (v) => v.toFixed(0) + '%' },
  fps: { src: 'fps', top: (v) => Math.max(5, Math.ceil(Math.max(...v) * 1.2)), fmt: (v) => v.toFixed(1) + ' fps' },
};

function sparkGeom(vals, top) {
  const n = vals.length;
  const x = (i) => W - (n - 1 - i) * (W / (N - 1));
  const y = (v) => H - 2 - Math.min(1, Math.max(0, v / top)) * (H - 4);
  return { x, y };
}

function drawSpark(k) {
  const vals = hist[SPARK[k].src];
  const e = els[k];
  if (vals.length < 2) { e.line.setAttribute('d', ''); e.area.setAttribute('d', ''); return; }
  const top = SPARK[k].top(vals);
  const { x, y } = sparkGeom(vals, top);
  let d = '';
  for (let i = 0; i < vals.length; i++) d += (i ? 'L' : 'M') + x(i).toFixed(2) + ' ' + y(vals[i]).toFixed(2);
  e.line.setAttribute('d', d);
  e.area.setAttribute('d', d + 'L' + x(vals.length - 1).toFixed(2) + ' ' + H + 'L' + x(0).toFixed(2) + ' ' + H + 'Z');
}

function bindSpark(k) {
  const e = els[k];
  const show = (ev) => {
    const vals = hist[SPARK[k].src];
    if (vals.length < 2) return;
    const r = e.spark.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width));
    const step = W / (N - 1), n = vals.length;
    const i = Math.min(n - 1, Math.max(0, n - 1 - Math.round((W - f * W) / step)));
    const { x, y } = sparkGeom(vals, SPARK[k].top(vals));
    const px = x(i) / W;
    e.rule.style.left = (px * 100) + '%';
    e.dot.style.left = (px * 100) + '%';
    e.dot.style.top = (y(vals[i]) / H * 100) + '%';
    e.spark.classList.add('on');
    const ago = (n - 1 - i) * (last ? last.interval_s : 2);
    const tip = els.tip;
    tip.hidden = false;
    tip.textContent = SPARK[k].fmt(vals[i]) + '  \u00b7  ' + (ago === 0 ? t('sysmon.now', 'now') : fmtAgo(ago));
    const rr = root.getBoundingClientRect();
    const tx = Math.min(rr.width - tip.offsetWidth - 4, Math.max(4, r.left - rr.left + px * r.width - tip.offsetWidth / 2));
    tip.style.left = tx + 'px';
    tip.style.top = (r.top - rr.top - tip.offsetHeight - 6) + 'px';
  };
  const hide = () => { e.spark.classList.remove('on'); els.tip.hidden = true; };
  e.spark.addEventListener('pointermove', show);
  e.spark.addEventListener('pointerdown', (ev) => { show(ev); clearTimeout(tipTimer); });
  e.spark.addEventListener('pointerleave', hide);
  e.spark.addEventListener('pointerup', (ev) => { if (ev.pointerType !== 'mouse') tipTimer = setTimeout(hide, 1800); });
  e.spark.addEventListener('pointercancel', hide);
}

// [number, unit] of a duration in ms (the tile shows them apart).
function msParts(v) { return v >= 1000 ? [(v / 1000).toFixed(1), t('sysmon.unit_s', 's')] : [String(Math.round(v)), t('sysmon.unit_ms', 'ms')]; }
function fmtMs(v) { return msParts(v).join(' '); }
function fmtAgo(s) {
  return s >= 60 ? (s % 60 ? t('sysmon.ago_min_s', '{m} min {s} s ago', { m: Math.floor(s / 60), s: s % 60 }) : t('sysmon.ago_min', '{m} min ago', { m: Math.floor(s / 60) })) : t('sysmon.ago_s', '{s} s ago', { s });
}

// ---------- status levels ----------
// Each returns null (fine) or { lvl: 'info'|'warn'|'crit', label }; 'info' is a neutral note.
function cpuStatus() {
  const v = hist.cpu.slice(-5);
  if (v.length < 5) return null;
  if (v.every((x) => x > 85)) return { lvl: 'crit', label: t('sysmon.high_load', 'High load') };
  if (v.every((x) => x > 70)) return { lvl: 'warn', label: t('sysmon.busy', 'Busy') };
  return null;
}
function memStatus(j) {
  const p = j.mem.total ? 100 * j.mem.used / j.mem.total : 0;
  if (p > 92) return { lvl: 'crit', label: t('sysmon.low_memory', 'Low memory') };
  if (p > 85) return { lvl: 'warn', label: t('sysmon.memory_high', 'Memory high') };
  return null;
}
// The server works the disk level out (full while the oldest make room is
// 'info'; below the free space to keep 'warn'; cannot free it / under 2% 'crit').
// Older linked servers do not send it: their rule below.
function diskStatus(d) {
  if (d.level !== undefined) return d.level ? { lvl: d.level, label: d.label || '' } : null;
  const free = d.drive_total ? d.drive_free / d.drive_total : 1;
  // A folder that hands its oldest recordings to the archive runs full.
  const capped = d.cap > 0 && !d.rolls;
  if (capped && d.pct > 90) return { lvl: 'crit', label: t('sysmon.near_cap', 'Near cap') };
  if (d.drive_total && (free < 0.05)) return { lvl: 'crit', label: t('sysmon.drive_full', 'Drive full') };
  if (capped && d.pct > 75) return { lvl: 'warn', label: t('sysmon.filling_up', 'Filling up') };
  if (d.drive_total && free < 0.1) return { lvl: 'warn', label: t('sysmon.low_space', 'Low space') };
  if (d.cap === 0 && d.pct > 90) return { lvl: 'crit', label: t('sysmon.drive_full', 'Drive full') };
  return null;
}
function camStatus(j) {
  const off = j.cameras.filter((c) => c.enabled && !c.online && !c.sleeping).length;
  if (!off) return null;
  const all = off === j.fps.cameras - j.fps.sleeping;
  return { lvl: all ? 'crit' : 'warn', label: t('sysmon.offline', '{off} offline', { off }) };
}

// AI: stalled is a fault, a slow 95th percentile a warning.
function aiStatus(j) {
  const a = j.ai;
  if (!a) return null;
  if (a.state === 'stalled') return { lvl: 'crit', label: t('sysmon.ai_stalled', 'AI stalled') };
  if (a.p95_ms != null && a.p95_ms > 2000) return { lvl: 'warn', label: t('sysmon.ai_slow', 'AI slow') };
  return null;
}

// Tiers ("new" recordings, "stored" archive): the fullest folder's fill and
// the least free space among them.
// The tier's word as shown ("new"/"stored" stay the API's values).
const tierName = (tier) => (tier === 'stored' ? t('sysmon.tier_stored', 'stored') : t('sysmon.tier_new', 'new'));
function tiersOf(j) {
  const out = [];
  for (const tier of ['new', 'stored']) {
    const f = (j.storage || []).filter((d) => (d.tier || 'new') === tier);
    if (!f.length) continue;
    const frees = f.map((d) => d.free).filter((x) => x != null);
    out.push({
      tier, pct: Math.max(...f.map((d) => d.pct)), free: frees.length ? Math.min(...frees) : null,
      bps: f.reduce((a, d) => a + (d.write_bps || 0), 0), st: worstOf(f.map(diskStatus)),
    });
  }
  return out;
}
const RANK = { info: 1, warn: 2, crit: 3 };
const worstOf = (list) => list.filter(Boolean).sort((a, b) => RANK[b.lvl] - RANK[a.lvl])[0] || null;
const rate = (b) => (b >= 1e6 ? (b / 1e6).toFixed(1) + ' MB/s' : b >= 1e3 ? Math.round(b / 1e3) + ' kB/s' : Math.round(b) + ' B/s');

// The headline: with every folder capped, all recording space together
// (a folder handing its oldest to the archive is full by design); otherwise
// the fullest folder.
function worstDisk(j) {
  const st = j.storage || [];
  if (!st.length) return null;
  if (st.every(d => d.cap > 0)) {
    const used = st.reduce((a, d) => a + d.used, 0), cap = st.reduce((a, d) => a + d.cap, 0);
    // An automatic cap follows the free space and can dip under what is stored.
    const pct = 100 * used / cap;
    return { name: t('sysmon.all_recordings', 'All recordings'), used, cap, pct: st.some(d => d.auto) ? Math.min(100, pct) : pct };
  }
  let w = null;
  for (const d of st) if (!w || d.pct > w.pct) w = d;
  return w;
}

function setMeter(m, pct) {
  const v = Math.max(0, Math.min(100, pct || 0));
  m.firstChild.style.width = v + '%';
  m.setAttribute('aria-valuenow', Math.round(v));
}

function setChip(e, st) {
  if (!st) { e.chip.hidden = true; e.tile.dataset.lvl = ''; return; }
  const html = icon(st.lvl) + '<span>' + esc(st.label) + '</span>';
  if (e.chip.dataset.h !== html) { e.chip.innerHTML = html; e.chip.dataset.h = html; }
  e.chip.dataset.lvl = st.lvl;
  e.chip.hidden = false;
  // A neutral note keeps the meter in its normal color.
  e.tile.dataset.lvl = st.lvl === 'info' ? '' : st.lvl;
}
const text = (el, s) => { if (el.textContent !== s) el.textContent = s; };

// ---------- render ----------
function fmtUp(s) {
  const d = Math.floor(s / 86400), h = Math.floor(s % 86400 / 3600), m = Math.floor(s % 3600 / 60);
  return d ? t('sysmon.up_dh', '{d}d {h}h', { d, h }) : h ? t('sysmon.up_hm', '{h}h {m}m', { h, m }) : t('sysmon.up_m', '{m}m', { m });
}
const gb = (b) => fmtSize(b).replace(/\.0 /, ' ');
const pctNum = (v) => (v >= 10 ? Math.round(v) : v.toFixed(1).replace(/\.0$/, ''));

function render() {
  const j = last;
  if (!j) return;
  const cpu = j.cpu.machine;
  const memPct = j.mem.total ? 100 * j.mem.used / j.mem.total : 0;
  const w = worstDisk(j);
  const sCpu = cpuStatus(), sMem = memStatus(j), sCam = camStatus(j), sAi = aiStatus(j);
  const sDisk = worstOf(j.storage.map(diskStatus));

  const fpsTxt = j.fps.total >= 100 ? String(Math.round(j.fps.total)) : j.fps.total.toFixed(1);
  // A neutral note (a folder full as intended) does not count as a problem.
  const worst = worstOf([sCpu, sMem, sCam, sDisk, sAi].filter((x) => x && x.lvl !== 'info'));
  root.dataset.lvl = worst ? worst.lvl : '';
  text(els.p_up, t('sysmon.up', 'up {time}', { time: fmtUp(j.uptime_s) }));

  // tiles
  text(els.cpu.val, pctNum(cpu)); text(els.cpu.unit, '%');
  text(els.cpu.cap, t('sysmon.cpu_cap', 'AmberSight {pct}%', { pct: pctNum(j.cpu.process) }));
  setChip(els.cpu, sCpu);
  text(els.mem.val, pctNum(memPct)); text(els.mem.unit, '%');
  text(els.mem.cap, t('sysmon.mem_of', '{used} of {total}', { used: gb(j.mem.used), total: gb(j.mem.total) }));
  setChip(els.mem, sMem);
  text(els.fps.val, fpsTxt); text(els.fps.unit, t('sysmon.fps', 'fps'));
  const live = j.fps.cameras - j.fps.sleeping;
  text(els.fps.cap, t('sysmon.fps_cap', '{streaming} of {live, plural, one {# camera} other {# cameras}}', { streaming: j.fps.streaming, live: live }));
  els.fps.cap.title = t('sysmon.fps_cap_title', '{streaming} of {live} cameras streaming', { streaming: j.fps.streaming, live: live });
  setChip(els.fps, sCam);
  drawSpark('fps');
  renderAi(j, sAi);
  renderGpu(j);
  setMeter(els.cpu.meter, cpu);
  setMeter(els.mem.meter, memPct);
  if (w) {
    text(els.disk.val, pctNum(w.pct)); text(els.disk.unit, '%');
    text(els.disk.cap, w.name + (w.cap ? ' ' + gb(w.used) + ' / ' + gb(w.cap) : ' ' + t('sysmon.drive_paren', '(drive)')));
    setMeter(els.disk.meter, w.pct);
  } else { text(els.disk.val, '--'); text(els.disk.unit, ''); text(els.disk.cap, t('sysmon.no_storage', 'no storage')); }
  setChip(els.disk, sDisk);
  const tiers = tiersOf(j);
  const th = tiers.map((tr) => '<div class="hl-tier"' + (tr.st && tr.st.lvl !== 'info' ? ' data-lvl="' + tr.st.lvl + '"' : '') + '><span>' + esc(tierName(tr.tier)) + '</span>' +
    '<div class="sm-meter" role="meter" aria-label="' + esc(t('sysmon.tier_recordings', '{tier} recordings', { tier: tierName(tr.tier) })) + '" aria-valuemin="0" aria-valuemax="100" aria-valuenow="' + Math.round(tr.pct) + '"><i style="width:' + Math.min(100, tr.pct) + '%"></i></div>' +
    '<em>' + (tr.free != null ? t('sysmon.free', '{size} free', { size: gb(tr.free) }) : '') + '</em></div>' +
    (tr.bps > 0 ? '<div class="hl-w">' + t('sysmon.writing', 'writing {rate}', { rate: rate(tr.bps) }) + '</div>' : '')).join('');
  if (els.disk.tiers.dataset.h !== th) { els.disk.tiers.innerHTML = th; els.disk.tiers.dataset.h = th; }
  els.disk.tiers.hidden = tiers.length < 1;

  if (mode === 'detail') renderDetail(j);
}

function renderAi(j, st) {
  const a = j.ai, e = els.ai;
  e.tile.hidden = !a || a.state === 'off';
  if (e.tile.hidden) return;
  if (a.p50_ms != null) { const pr = msParts(a.p50_ms); text(e.val, pr[0]); text(e.unit, pr[1]); }
  else { text(e.val, a.state === 'idle' ? t('sysmon.idle', 'idle') : '--'); text(e.unit, ''); }
  text(e.cap, t('sysmon.checks_per_min', '{n, plural, one {# check} other {# checks}}/min · {device}', { n: a.checks_1m, device: a.device || '' }));
  e.cap.title = t('sysmon.ai_title', 'AI object detection on {device}. Median time per check{p95}', { device: a.device || '?', p95: a.p95_ms != null ? t('sysmon.ai_95th', ', 95th percentile {time}', { time: fmtMs(a.p95_ms) }) : '' }) +
    (a.errors_1m ? t('sysmon.ai_failed', '; {n} failed in the last minute', { n: a.errors_1m }) : '') + (a.last_ok_s != null ? t('sysmon.ai_last_ok', '; last success {n} s ago', { n: a.last_ok_s }) : '');
  setChip(e, st);
  drawSpark('ai');
}

function renderGpu(j) {
  const g = j.gpu, e = els.gpu;
  e.tile.hidden = !g;
  if (!g) return;
  const a = (g.adapters || []).slice().sort((x, y) => (y.vram_total || 0) - (x.vram_total || 0))[0];
  text(e.val, pctNum(g.util)); text(e.unit, '%');
  const vu = a && a.vram_used, vt = a && a.vram_total;
  text(e.cap, a ? a.name + (vu != null && vt ? ' · ' + gb(vu) + ' / ' + gb(vt) + ' ' + t('sysmon.all_programs', '(all programs)') : '') : '');
  // Busy % and memory are for the whole card: every program on it, not only AmberSight.
  e.cap.title = (g.adapters || []).map((x) => x.name + ' ' + pctNum(x.util) + '%' + (x.vram_used != null ? ', ' + t('sysmon.mem_used', '{size} memory used', { size: gb(x.vram_used) }) : '')).join('\n') +
    '\n' + t('sysmon.whole_card', 'Whole card: all programs on it, not only AmberSight.');
  e.vram.title = t('sysmon.vram_title', 'Video memory used by all programs on this card');
  e.vram.hidden = !(vu != null && vt);
  if (!e.vram.hidden) setMeter(e.vram, 100 * vu / vt);
  drawSpark('gpu');
}

function meterRow(d) {
  const st = diskStatus(d);
  const free = d.drive_total != null && d.drive_free != null ? t('sysmon.free_of', '{free} free of {total}', { free: gb(d.drive_free), total: gb(d.drive_total) }) : '';
  const usedTxt = d.cap > 0 || d.auto ? gb(d.used) + ' / ' + gb(d.cap) + (d.auto ? ' (auto)' : '') : t('sysmon.used_no_cap', '{size} used, no cap', { size: gb(d.used) });
  return '<div class="sm-row" ' + (st && st.lvl !== 'info' ? 'data-lvl="' + st.lvl + '"' : '') + '>' +
    '<div class="sm-rh"><span class="sm-rn">' + esc(d.name) + '</span>' +
    (st ? '<span class="sm-chip" data-lvl="' + st.lvl + '">' + icon(st.lvl) + '<span>' + esc(st.label) + '</span></span>' : '') +
    '<span class="sm-rv">' + usedTxt + '</span></div>' +
    '<div class="sm-meter" role="meter" aria-valuemin="0" aria-valuemax="100" aria-valuenow="' + Math.round(d.pct) + '"><i style="width:' + Math.min(100, d.pct) + '%"></i></div>' +
    '<div class="sm-rs" title="' + esc(d.path) + '">' + (d.cap > 0 ? (d.auto ? t('sysmon.pct_spare', '{pct}% of what the drive can spare · ', { pct: pctNum(d.pct) }) : t('sysmon.pct_cap', '{pct}% of cap · ', { pct: pctNum(d.pct) })) : '') + (d.rolls && d.pct >= 95 ? t('sysmon.full_archive', 'full: oldest move to the archive · ') : '') + free + '</div></div>';
}

function camRow(c) {
  let st, cls = '';
  if (!c.enabled) { st = '<span class="sm-cs mut">' + t('sysmon.disabled', 'Disabled') + '</span>'; cls = 'mut'; }
  else if (c.sleeping) { st = '<span class="sm-cs mut">' + icon('moon') + t('sysmon.asleep', 'Asleep') + '</span>'; cls = 'mut'; }
  else if (!c.online) { st = '<span class="sm-cs" data-lvl="crit">' + icon('crit') + t('sysmon.cam_offline', 'Offline') + '</span>'; cls = 'off'; }
  else st = '<span class="sm-cv">' + c.fps.toFixed(1) + ' <small>' + t('sysmon.fps', 'fps') + '</small></span>';
  return '<li class="' + cls + '"><span class="sm-cn">' + esc(c.name) + '</span>' + st + '</li>';
}

function renderDetail(j) {
  const memPct = j.mem.total ? 100 * j.mem.used / j.mem.total : 0;
  const html =
    '<div class="sm-sec"><h4>' + t('sysmon.proc_mem_header', 'Processor and memory') + '</h4><dl>' +
    '<dt>' + t('sysmon.machine_cpu', 'Machine CPU') + '</dt><dd>' + pctNum(j.cpu.machine) + '% <small>' + t('sysmon.n_cores', '{n} cores', { n: j.cpu.cores }) + '</small></dd>' +
    '<dt>' + t('sysmon.amber_cpu', 'AmberSight CPU') + '</dt><dd>' + pctNum(j.cpu.process) + '% <small>' + t('sysmon.of_machine', 'of the machine') + '</small></dd>' +
    '<dt>' + t('sysmon.memory_used_label', 'Memory used') + '</dt><dd>' + gb(j.mem.used) + ' <small>' + t('sysmon.mem_detail', 'of {total} ({pct}%)', { total: gb(j.mem.total), pct: pctNum(memPct) }) + '</small></dd>' +
    '<dt>' + t('sysmon.amber_memory', 'AmberSight memory') + '</dt><dd>' + gb(j.mem.process) + '</dd>' +
    '<dt>' + t('sysmon.uptime', 'Uptime') + '</dt><dd>' + fmtUp(j.uptime_s) + '</dd>' +
    (j.net ? '<dt>' + t('sysmon.network', 'Network') + '</dt><dd>&darr; ' + rate(j.net.rx_bps) + ' <small>&uarr; ' + rate(j.net.tx_bps) + '</small></dd>' : '') +
    (j.ai && j.ai.state !== 'off' ? '<dt>AI</dt><dd>' + j.ai.state + ' <small>' + (j.ai.p95_ms != null ? 'p95 ' + fmtMs(j.ai.p95_ms) : '') + '</small></dd>' : '') +
    '</dl></div>' +
    '<div class="sm-sec"><h4>' + t('sysmon.storage_header', 'Storage') + '</h4>' + (j.storage.map(meterRow).join('') || '<p class="sm-none">' + t('sysmon.no_storage_info', 'No storage information yet') + '</p>') + '</div>' +
    '<div class="sm-sec"><h4>' + t('sysmon.cameras_header', 'Cameras') + ' <small>' + t('sysmon.streaming_of', '{streaming} of {live} streaming', { streaming: j.fps.streaming, live: j.fps.cameras - j.fps.sleeping }) + (j.fps.sleeping ? t('sysmon.n_asleep', ', {n} asleep', { n: j.fps.sleeping }) : '') + '</small></h4>' +
    '<ul class="sm-cams">' + (j.cameras.map(camRow).join('') || '<li class="mut">' + t('sysmon.no_cameras', 'No cameras') + '</li>') + '</ul></div>';
  if (html !== detailKey) { els.detail.innerHTML = html; detailKey = html; }
}

// ---------- data ----------
function ingest(j) {
  const h = j.history;
  if (h.seq < since) {                 // server restarted: start over
    since = 0; hist = emptyHist(); return false;
  }
  for (const k of Object.keys(hist)) {
    const a = hist[k].concat(h[k] || []);
    hist[k] = a.length > N ? a.slice(a.length - N) : a;
  }
  since = h.seq;
  last = j;
  return true;
}

// Seen at all: in the dock, its "Server figures" section open, on Live, the browser tab visible.
function visible() {
  if (!started || !docked || !root || !root.isConnected || document.hidden) return false;
  const fold = root.closest('details');
  return (!fold || fold.open) && document.body.dataset.tab === 'live';
}
async function tick() {
  if (inflight || !visible()) return;
  inflight = true;
  try {
    let j = await api(statsUrl() + since);
    if (started && !ingest(j)) { j = await api(statsUrl() + '0'); if (started) ingest(j); }
    if (started) { root.hidden = false; render(); }
  } catch (e) { /* signed out or server restarting: try again next tick */ }
  inflight = false;
}

function schedule() {
  clearInterval(timer);
  timer = setInterval(tick, 2000);
}
function onVisibility() {
  if (document.hidden) { clearInterval(timer); timer = null; }
  else if (started) { tick(); schedule(); }
}

// Put the figures into `el` (the dock's section) or, with null, take them out (nothing is fetched then).
export function hostIn(el) {
  const want = !!el;
  if (!root) { pendingHost = el; return; }
  if (want === docked && (!want || root.parentNode === el)) return;
  docked = want;
  root.classList.toggle('indock', docked);
  if (el) {
    el.appendChild(root);
    // Opening the section (or coming back to Live) fetches at once instead of at the next tick.
    const fold = el.closest('details');
    if (fold && !fold.__smWired) { fold.__smWired = true; fold.addEventListener('toggle', () => { if (fold.open) tick(); }); }
    tick();
  } else root.remove();
}
let pendingHost = null;

export function startSysmon() {
  if (started) return;
  started = true;
  if (!root) { build(); document.addEventListener('visibilitychange', onVisibility); if (pendingHost) hostIn(pendingHost); }
  root.hidden = true;               // shown with the first answer
  since = 0; hist = emptyHist(); last = null;
  tick();
  if (!document.hidden) schedule();
}

export function stopSysmon() {
  started = false;
  clearInterval(timer); timer = null;
  if (root) { root.hidden = true; els.tip.hidden = true; }
  last = null; detailKey = '';
}
