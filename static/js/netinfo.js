// A camera's "Network" section in its settings panel: address, MAC, ports, stream,
// maker/model, with Copy, an "Open camera's web page" link and Refresh.
// Admin only (the server answers 403 to viewers). Loaded when a camera's panel opens.
import { api, esc, toast } from './core.js';

export function netinfoHtml() {
  return '<div class="ni" data-netinfo><div class="ni-rows" aria-live="polite"><p class="muted ni-wait">Looking up the camera...</p></div>' +
    '<div class="ni-bar"><a class="btn" data-ni-open target="_blank" rel="noopener noreferrer" hidden>Open camera\'s web page</a>' +
    '<button type="button" class="btn" data-ni-refresh>Refresh</button></div></div>';
}

async function copyText(text, b) {
  let ok = false;
  try { await navigator.clipboard.writeText(text); ok = true; } catch (e) { /* not a secure page: try the old way */ }
  if (!ok) {
    const t = document.createElement('textarea');
    t.value = text; t.setAttribute('readonly', ''); t.style.cssText = 'position:fixed;top:0;left:0;opacity:0';
    document.body.appendChild(t); t.select();
    try { ok = document.execCommand('copy'); } catch (e) { ok = false; }
    t.remove();
    if (b) b.focus();
  }
  toast(ok ? 'Copied ' + text : 'Could not copy');
}

const row = (label, value, copy) => value == null || value === '' ? '' :
  '<div class="ni-row"><span class="ni-k">' + esc(label) + '</span><span class="ni-v">' + esc(value) + '</span>' +
  (copy ? '<button type="button" class="btn ni-copy" data-copy="' + esc(copy) + '" aria-label="Copy ' + esc(label) + '">Copy</button>' : '<span></span>') + '</div>';

function render(box, j) {
  const rows = box.querySelector('.ni-rows');
  const open = box.querySelector('[data-ni-open]');
  if (!j.host) {
    rows.innerHTML = '<p class="muted">' + esc(j.note || 'This camera is not on the network.') + '</p>';
    open.hidden = true;
    return;
  }
  const ports = [];
  if (j.stream_port) ports.push(j.stream_port + ' video');
  if (j.sub && j.sub.port && j.sub.port !== j.stream_port) ports.push(j.sub.port + ' sub stream');
  if (j.onvif_port) ports.push(j.onvif_port + ' ONVIF');
  const state = j.reachable ? 'Reachable' + (j.connect_ms != null ? ' (' + j.connect_ms + ' ms)' : '') : 'Not reachable' + (j.error ? ' (' + j.error + ')' : '');
  const same = j.ip && j.ip !== j.host;
  rows.innerHTML =
    '<div class="ni-state" data-ok="' + (j.reachable ? '1' : '0') + '">' + esc(state) + '</div>' +
    row('IP address', j.ip || 'Not found', j.ip) +
    (same ? row('Address in settings', j.host) : '') +
    row('MAC address', j.mac || 'Not known (only seen on the same network)', j.mac) +
    row('Ports', ports.join(', ')) +
    row('Stream', (j.scheme ? j.scheme + ' ' : '') + (j.stream_path || '/') + (j.sub ? '   sub: ' + (j.sub.path || '/') : '')) +
    row('Maker', j.maker) + row('Model', j.model) + row('Firmware', j.firmware) + row('Serial number', j.serial) +
    (j.onvif_port && !j.maker ? '<p class="muted ni-note">The camera did not answer the ONVIF question (check its sign-in and ONVIF port).</p>' : '') +
    row('Video', j.online ? 'Online (' + (j.ingest || '') + ')' : 'Offline');
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
    btn.disabled = true; btn.textContent = 'Checking...';
    try {
      const j = await api('/api/cameras/' + c.id + '/netinfo' + (refresh ? '?refresh=1' : ''));
      if (!gone && my === seq && box.isConnected) render(box, j);
    } catch (e) {
      if (!gone && box.isConnected) box.querySelector('.ni-rows').innerHTML = '<p class="muted">Could not get the network details' + (e && e.status === 403 ? ': only admins can see them.' : '.') + '</p>';
    }
    if (!gone && box.isConnected) { btn.disabled = false; btn.textContent = 'Refresh'; }
  };
  btn.onclick = () => load(true);
  box.addEventListener('click', (e) => {
    const b = e.target.closest('[data-copy]');
    if (b) copyText(b.dataset.copy, b);
  });
  load(false);
  return () => { gone = true; };
}
