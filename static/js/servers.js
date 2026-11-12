// servers.js - Linked AmberSight servers: Settings > Connections > Servers (name this server,
// link another with a one-time code, rename/remove) and the header switcher
// that opens a linked server already signed in.
import { api, esc, toast, openDlg, state, ic, errText, liveSwitch, swHtml, confirmDialog, promptDialog, savedToast } from './core.js';
import { lead, how, more } from './sp_kit.js';
const grouped = (c) => c.replace(/(.{4})(?=.)/g, '$1-');

// ------------------------------------------------------------ Settings tab
export function mountServers(host) {
  let data = null;
  async function load() {
    try { data = await api('/api/links?check=1'); } catch (e) { host.innerHTML = '<p class="empty">' + esc(errText(e)) + '</p>'; return; }
    paint();
    initServerMenu();
  }
  function paint() {
    const links = data.links || [];
    let h = lead('Link another AmberSight (a second home, a cabin) to switch between them from the menu next to the logo.', 'Only needed if you run more than one AmberSight.') +
      '<div class="sec">This server</div>' +
      '<div class="cs-ovin"><input data-selfname value="' + esc(data.name) + '" maxlength="64"><button class="btn" data-saveself>Rename</button></div>' +
      '<div class="hint">Shown to the servers linked with this one and in their server menu.</div>';
    h += '<div class="sec">Linked servers</div>';
    h += links.length ? '<div class="cs-list">' + links.map(l =>
      '<div class="lk-item"><div class="cs-name"><b>' + esc(l.name) + '</b><div class="cs-url">' + esc(l.url || '') + (l.version ? ' - version ' + esc(l.version) : '') + '</div>' +
        (l.error ? '<div class="hint">' + esc(l.error) + '</div>' : '') +
        swHtml('data-share="' + esc(l.id) + '"', l.share, 'Share this server\'s cameras with ' + esc(l.name), 'Live view only.') +
        swHtml('data-show="' + esc(l.id) + '"', l.show, 'Show ' + esc(l.name) + '\'s cameras here') +
        '<details class="sh-more"><summary>More settings</summary><label class="srv-opt" title="Opening this server from ' + esc(l.name) + '\'s menu signs you in here as the same account. ' + esc(l.name) + ' is trusted to say who you are, so only allow what you trust it with.">Sign-in from ' + esc(l.name) + ': <select data-signin="' + esc(l.id) + '">' +
          [['viewers', 'same account, never as admin (recommended)'], ['all', 'same account, admins too'], ['off', 'off (sign in here yourself)']].map(([v, t]) => '<option value="' + v + '"' + ((l.signin || 'viewers') === v ? ' selected' : '') + '>' + t + '</option>').join('') + '</select></label><div class="hint">Admins sign in here themselves when they switch; other accounts carry over. With "admins too", any admin on ' + esc(l.name) + ' can act as an admin here, even after you disable their account here.</div></details>' +
        (l.show && l.show_error ? '<div class="hint">' + esc(l.show_error) + '</div>' : '') + '</div>' +
      '<span class="badge dot ' + (l.online ? 'on' : 'off') + '">' + (l.online ? 'reachable' : 'not reachable') + '</span>' +
      '<div class="lk-acts"><button class="btn" data-addr="' + esc(l.id) + '" title="Reach it at another address, e.g. its Tailscale IP">Change address</button><button class="btn" data-ren="' + esc(l.id) + '">Rename</button><button class="btn danger" data-rm="' + esc(l.id) + '">Unlink</button></div></div>').join('') + '</div>'
      : '<p class="hint">No linked servers yet.</p>';
    h += '<div class="row"><button class="btn" data-mkcode>' + ic('plus') + ' Let another server link to this one</button><button class="btn" data-link>' + ic('plus') + ' Link to another server</button></div>';
    h += how('Switching signs you in on the other server as the user with the same name there, unless that is an admin account: admins sign in there themselves unless the other server allows otherwise (More settings). Each device must trust each server\'s certificate once (account menu, Trust this server). <b>Only link servers you control:</b> a linked server is trusted to say who is signing in, so an admin there can sign in here as any account with the same name.');
    host.innerHTML = h;
    host.querySelector('[data-saveself]').onclick = async () => {
      const name = host.querySelector('[data-selfname]').value.trim();
      try { await api('/api/links/self', { method: 'PUT', json: { name } }); toast('This server is now called ' + name); load(); }
      catch (e) { toast(errText(e)); }
    };
    host.querySelectorAll('[data-ren]').forEach(b => { b.onclick = async () => {
      const l = links.find(x => x.id === b.dataset.ren);
      const name = ((await promptDialog({ title: 'Rename server', label: 'Name for this server', value: l.name, ok: 'Rename' })) || '').trim(); if (!name || name === l.name) return;
      try { await api('/api/links/' + encodeURIComponent(l.id), { method: 'PUT', json: { name } }); load(); } catch (e) { toast(errText(e)); }
    }; });
    host.querySelectorAll('[data-addr]').forEach(b => { b.onclick = async () => {
      const l = links.find(x => x.id === b.dataset.addr);
      const url = ((await promptDialog({ title: 'Change address', label: 'Address of ' + l.name + ' (checked before it is saved)', value: l.url || '', placeholder: 'https://192.168.1.20:8090', ok: 'Change' })) || '').trim();
      if (!url || url === l.url) return;
      b.disabled = true;
      try { await api('/api/links/' + encodeURIComponent(l.id), { method: 'PUT', json: { url } }); toast(l.name + ' is now reached at ' + url); load(); }
      catch (e) { toast(errText(e)); b.disabled = false; }
    }; });
    host.querySelectorAll('[data-rm]').forEach(b => { b.onclick = async () => {
      const l = links.find(x => x.id === b.dataset.rm);
      if (!(await confirmDialog({ title: 'Unlink ' + l.name + '?', body: 'Both servers forget the link, and the cameras shared between them disappear from each other. To link again you need a new code.', ok: 'Unlink', danger: true }))) return;
      try { const r = await api('/api/links/' + encodeURIComponent(l.id), { method: 'DELETE' }); toast(r.other_side_told ? 'Unlinked' : 'Unlinked here; ' + l.name + ' could not be told (unlink it there too)'); load(); }
      catch (e) { toast(errText(e)); }
    }; });
    const toggle = (attr, key) => host.querySelectorAll('[' + attr + ']').forEach(cb => liveSwitch(cb, async (on) => {
      await api('/api/links/' + encodeURIComponent(cb.getAttribute(attr)), { method: 'PUT', json: { [key]: on } });
      const { refreshCameras } = await import('./core.js');
      if (refreshCameras) refreshCameras().catch(() => {});
      load();
    }));
    toggle('data-share', 'share');
    toggle('data-show', 'show');
    host.querySelectorAll('[data-signin]').forEach(sel => { sel.onchange = async () => {
      sel.disabled = true;
      try { await api('/api/links/' + encodeURIComponent(sel.dataset.signin), { method: 'PUT', json: { signin: sel.value } }); savedToast(); load(); }
      catch (e) { toast(errText(e)); sel.disabled = false; }
    }; });
    host.querySelector('[data-mkcode]').onclick = codeDialog;
    host.querySelector('[data-link]').onclick = () => linkDialog(load);
  }
  host.innerHTML = '<p class="empty">Loading...</p>';
  load();
}

async function codeDialog() {
  let r;
  try { r = await api('/api/links/code', { json: {} }); } catch (e) { toast(errText(e)); return; }
  const d = openDlg('Link code for ' + r.name,
    '<div class="hint">On the other server: Settings &rsaquo; Servers &rsaquo; Link to another server, and enter:</div>' +
    '<div class="sec">This server\'s address</div><code class="tlsfp">' + esc(location.origin) + '</code>' +
    '<div class="sec">Code (works once, for 15 minutes)</div><code class="tlsfp lk-code">' + esc(grouped(r.code)) + '</code>' +
    '<div class="sec">Certificate fingerprint to compare</div><code class="tlsfp">' + esc(r.fingerprint || '') + '</code>' +
    '<div class="row"><button class="btn" data-close>Close</button></div>', true);
  d.querySelector('[data-close]').onclick = () => d.__close();
}

function linkDialog(done) {
  const d = openDlg('Link to another server',
    '<label class="cs-f"><span>Its address</span><input data-url placeholder="https://cabin:8090" spellcheck="false"></label>' +
    '<label class="cs-f"><span>Its link code</span><input data-code placeholder="XXXX-XXXX-XXXX" spellcheck="false" autocomplete="off"></label>' +
    '<div class="hint">Make the code on the other server: Settings &rsaquo; Servers &rsaquo; Let another server link to this one.</div>' +
    '<details><summary class="hint">This server\'s address, as the other server reaches it</summary><input data-own value="' + esc(location.origin) + '" spellcheck="false"><div class="hint">Change it if the other server reaches this one by another name (e.g. a Tailscale name).</div></details>' +
    '<div data-step2 hidden><div class="sec">Compare the certificate fingerprint</div><code class="tlsfp" data-fp></code>' +
      '<div class="hint">It must match the fingerprint shown with the code on the other server. If not, do not link.</div></div>' +
    '<div class="row"><button class="btn primary" data-go>Check</button><button class="btn" data-cancel>Cancel</button></div>', true);
  const q = (s) => d.querySelector(s);
  q('[data-cancel]').onclick = () => d.__close();
  let probed = null;
  q('[data-go]').onclick = async () => {
    const b = q('[data-go]');
    b.disabled = true;
    try {
      if (!probed) {
        probed = await api('/api/links/probe', { json: { url: q('[data-url]').value } });
        q('[data-url]').value = probed.url;
        q('[data-fp]').textContent = probed.fingerprint;
        q('[data-step2]').hidden = false;
        b.textContent = 'It matches: link';
      } else {
        const r = await api('/api/links', { json: { url: probed.url, code: q('[data-code]').value, fingerprint: probed.fingerprint, own_url: q('[data-own]').value } });
        toast('Linked with ' + r.name);
        d.__close();
        done();
        return;
      }
    } catch (e) { toast(errText(e)); probed = probed && q('[data-step2]').hidden ? null : probed; }
    b.disabled = false;
  };
  q('[data-url]').oninput = () => { probed = null; q('[data-step2]').hidden = true; q('[data-go]').textContent = 'Check'; };
}

// ------------------------------------------------------------ header switcher
let menuBuilt = false;
export async function initServerMenu() {
  let data;
  try { data = await api('/api/links'); } catch (e) { return; }
  let btn = document.getElementById('srvBtn');
  const links = data.links || [];
  if (!links.length) { if (btn) btn.remove(); return; }
  if (!btn) {
    btn = document.createElement('button');
    btn.id = 'srvBtn';
    btn.className = 'srv-btn';
    btn.title = 'Switch server';
    document.querySelector('header .logo').after(btn);
  }
  btn.innerHTML = '<span>' + esc(data.name) + '</span><svg class="ic"><use href="#i-down"/></svg>';
  btn.onclick = (e) => { e.stopPropagation(); toggleMenu(btn); };
  if (!menuBuilt) {
    menuBuilt = true;
    document.addEventListener('click', () => { const m = document.getElementById('srvMenu'); if (m) m.remove(); });
  }
}

async function toggleMenu(btn) {
  let m = document.getElementById('srvMenu');
  if (m) { m.remove(); return; }
  m = document.createElement('div');
  m.id = 'srvMenu';
  m.className = 'srv-menu';
  m.innerHTML = '<div class="hint">Checking...</div>';
  const r = btn.getBoundingClientRect();
  m.style.left = r.left + 'px'; m.style.top = (r.bottom + 4) + 'px';
  m.onclick = (e) => e.stopPropagation();
  document.body.appendChild(m);
  let data;
  try { data = await api('/api/links?check=1'); } catch (e) { m.innerHTML = '<div class="hint">' + esc(errText(e)) + '</div>'; return; }
  m.innerHTML = '<div class="srv-here">' + ic('eye') + ' ' + esc(data.name) + ' <span class="muted">(this one)</span></div>' +
    (data.links || []).map(l => '<button data-open="' + esc(l.id) + '"' + (l.online ? '' : ' disabled') + '><span class="srv-dot ' + (l.online ? 'on' : 'off') + '" aria-hidden="true"></span>' + esc(l.name) + (l.online ? ' <span class="muted">online</span>' : ' <span class="muted">not reachable</span>') + '</button>').join('') +
    (state.isAdmin ? '<div class="srv-sep"></div><button data-manage>Manage servers...</button>' : '');
  m.querySelectorAll('[data-open]').forEach(b => { b.onclick = async () => {
    b.disabled = true; b.textContent = 'Opening...';
    try { const o = await api('/api/links/' + encodeURIComponent(b.dataset.open) + '/open', { json: {} }); (window.top || window).location.href = o.url; }
    catch (e) { toast(errText(e)); m.remove(); }
  }; });
  const mg = m.querySelector('[data-manage]');
  if (mg) mg.onclick = async () => { m.remove(); const { openSettingsDialog } = await import('./settings.js'); openSettingsDialog('servers'); };
}
