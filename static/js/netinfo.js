// A camera's "Network" section in its settings panel: address, MAC, ports, stream,
// maker/model, with Copy, an "Open camera's web page" link, Refresh and Camera report
// (a text file for a bug report: GET /api/cameras/:id/report).
// Admin only (the server answers 403 to viewers). Loaded when a camera's panel opens.
import { t } from './i18n.js';
import { api, esc, toast, downloadUrl } from './core.js';

export function netinfoHtml() {
  return '<div class="ni" data-netinfo><div class="ni-rows" aria-live="polite"><p class="muted ni-wait">' + t('netinfo.looking_up', 'Looking up the camera...') + '</p></div>' +
    '<div class="ni-bar"><a class="btn" data-ni-open target="_blank" rel="noopener noreferrer" hidden>' + t('netinfo.open_web_page', 'Open camera\'s web page') + '</a>' +
    '<button type="button" class="btn" data-ni-refresh>' + t('netinfo.refresh', 'Refresh') + '</button>' +
    '<button type="button" class="btn" data-ni-report title="' + esc(t('netinfo.camera_report_title', 'Download a text file for a bug report about this camera')) + '">' + t('netinfo.camera_report', 'Camera report') + '</button></div>' +
    '<p class="hint">' + t('netinfo.camera_report_hint', 'Camera report: a text file to attach to a bug report about this camera (its streams, ONVIF profiles and recent log lines, without user names or passwords). Takes up to 20 seconds.') + '</p></div>';
}

// Download the camera report (the server names the file).
async function saveReport(c, b) {
  b.disabled = true; b.textContent = t('netinfo.collecting', 'Collecting...');
  try {
    const r = await fetch('/api/cameras/' + c.id + '/report', { credentials: 'same-origin' });
    if (!r.ok) {
      let why = '';
      try { why = (await r.text()).trim(); } catch (e) { /* no body */ }
      throw new Error(r.status === 429 ? t('netinfo.one_being_made_already', 'one is being made already') : (why || t('netinfo.error_status', 'error {status}', { status: r.status })));
    }
    const name = (/filename="([^"]+)"/.exec(r.headers.get('content-disposition') || '') || [])[1] || 'ambersight-camera-report.txt';
    const u = URL.createObjectURL(await r.blob());
    downloadUrl(u, name);
    setTimeout(() => URL.revokeObjectURL(u), 5000);
    toast(t('netinfo.camera_report_saved', 'Camera report saved: attach it to your GitHub issue'), { kind: 'ok' });
  } catch (e) {
    toast(t('netinfo.could_not_make_report', 'Could not make the camera report: {msg}', { msg: e.message || e }));
  }
  b.disabled = false; b.textContent = t('netinfo.camera_report', 'Camera report');
}

async function copyText(text, b) {
  let ok = false;
  try { await navigator.clipboard.writeText(text); ok = true; } catch (e) { /* not a secure page: try the old way */ }
  if (!ok) {
    const ta = document.createElement('textarea');
    ta.value = text; ta.setAttribute('readonly', ''); ta.style.cssText = 'position:fixed;top:0;left:0;opacity:0';
    document.body.appendChild(ta); ta.select();
    try { ok = document.execCommand('copy'); } catch (e) { ok = false; }
    ta.remove();
    if (b) b.focus();
  }
  toast(ok ? t('netinfo.copied', 'Copied {text}', { text }) : t('netinfo.could_not_copy', 'Could not copy'));
}

const row = (label, value, copy) => value == null || value === '' ? '' :
  '<div class="ni-row"><span class="ni-k">' + esc(label) + '</span><span class="ni-v">' + esc(value) + '</span>' +
  (copy ? '<button type="button" class="btn ni-copy" data-copy="' + esc(copy) + '" aria-label="' + esc(t('netinfo.copy_label', 'Copy {label}', { label })) + '">' + t('netinfo.copy', 'Copy') + '</button>' : '<span></span>') + '</div>';

function render(box, j) {
  const rows = box.querySelector('.ni-rows');
  const open = box.querySelector('[data-ni-open]');
  if (!j.host) {
    rows.innerHTML = '<p class="muted">' + esc(j.note || t('netinfo.not_on_network', 'This camera is not on the network.')) + '</p>';
    open.hidden = true;
    return;
  }
  const ports = [];
  if (j.stream_port) ports.push(t('netinfo.port_video', '{port} video', { port: j.stream_port }));
  if (j.sub && j.sub.port && j.sub.port !== j.stream_port) ports.push(t('netinfo.port_sub', '{port} sub stream', { port: j.sub.port }));
  if (j.onvif_port) ports.push(t('netinfo.port_onvif', '{port} ONVIF', { port: j.onvif_port }));
  const state = j.reachable ? t('netinfo.reachable', 'Reachable') + (j.connect_ms != null ? ' ' + t('netinfo.connect_ms', '({n} ms)', { n: j.connect_ms }) : '') : t('netinfo.not_reachable', 'Not reachable') + (j.error ? ' (' + j.error + ')' : '');
  const same = j.ip && j.ip !== j.host;
  rows.innerHTML =
    '<div class="ni-state" data-ok="' + (j.reachable ? '1' : '0') + '">' + esc(state) + '</div>' +
    row(t('netinfo.ip_address', 'IP address'), j.ip || t('netinfo.not_found', 'Not found'), j.ip) +
    (same ? row(t('netinfo.address_in_settings', 'Address in settings'), j.host) : '') +
    row(t('netinfo.mac_address', 'MAC address'), j.mac || t('netinfo.mac_not_known', 'Not known (only seen on the same network)'), j.mac) +
    row(t('netinfo.ports', 'Ports'), ports.join(', ')) +
    row(t('netinfo.stream', 'Stream'), (j.scheme ? j.scheme + ' ' : '') + (j.stream_path || '/') + (j.sub ? '   ' + t('netinfo.sub_path', 'sub: {path}', { path: j.sub.path || '/' }) : '')) +
    row(t('netinfo.maker', 'Maker'), j.maker) + row(t('netinfo.model', 'Model'), j.model) + row(t('netinfo.firmware', 'Firmware'), j.firmware) + row(t('netinfo.serial_number', 'Serial number'), j.serial) +
    (j.onvif_port && !j.maker ? '<p class="muted ni-note">' + t('netinfo.onvif_question_failed', 'The camera did not answer the ONVIF question (check its sign-in and ONVIF port).') + '</p>' : '') +
    row(t('netinfo.video', 'Video'), j.online ? t('netinfo.online_ingest', 'Online ({ingest})', { ingest: j.ingest || '' }) : t('netinfo.offline', 'Offline'));
  open.hidden = !/^https?$|^rtsps?$/.test(j.scheme || '') || !j.host;
  if (!open.hidden) open.href = 'http://' + (/:/.test(j.host) ? '[' + j.host + ']' : j.host) + '/';
}

export function wireNetinfo(d, c) {
  const box = d.querySelector('[data-netinfo]');
  if (!box) return;
  const btn = box.querySelector('[data-ni-refresh]');
  let gone = false, seq = 0;
  const load = async (refresh) => {
    const my = ++seq;
    btn.disabled = true; btn.textContent = t('netinfo.checking', 'Checking...');
    try {
      const j = await api('/api/cameras/' + c.id + '/netinfo' + (refresh ? '?refresh=1' : ''));
      if (!gone && my === seq && box.isConnected) render(box, j);
    } catch (e) {
      if (!gone && box.isConnected) box.querySelector('.ni-rows').innerHTML = '<p class="muted">' + (e && e.status === 403 ? t('netinfo.details_admins_only', 'Could not get the network details: only admins can see them.') : t('netinfo.details_failed', 'Could not get the network details.')) + '</p>';
    }
    if (!gone && box.isConnected) { btn.disabled = false; btn.textContent = t('netinfo.refresh', 'Refresh'); }
  };
  btn.onclick = () => load(true);
  const rep = box.querySelector('[data-ni-report]');
  rep.onclick = () => saveReport(c, rep);
  box.addEventListener('click', (e) => {
    const b = e.target.closest('[data-copy]');
    if (b) copyText(b.dataset.copy, b);
  });
  load(false);
  return () => { gone = true; };
}
