// servers.js - Linked AmberSight servers: Settings > Connections > Servers (name this server,
// link another with a one-time code, rename/remove) and the header switcher
// that opens a linked server already signed in.
import { t } from './i18n.js';
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
    let h = lead(t('servers.link_another', 'Link another AmberSight (a second home, a cabin) to switch between them from the menu next to the logo.'), t('servers.only_needed_one', 'Only needed if you run more than one AmberSight.')) +
      '<div class="sec">' + t('servers.this_server', 'This server') + '</div>' +
      '<div class="cs-ovin"><input data-selfname value="' + esc(data.name) + '" maxlength="64"><button class="btn" data-saveself>' + t('servers.rename', 'Rename') + '</button></div>' +
      '<div class="hint">' + t('servers.hint_shown', 'Shown to the servers linked with this one and in their server menu.') + '</div>';
    h += '<div class="sec">' + t('servers.linked_servers', 'Linked servers') + '</div>';
    h += links.length ? '<div class="cs-list">' + links.map(l =>
      '<div class="lk-item"><div class="cs-name"><b>' + esc(l.name) + '</b><div class="cs-url">' + esc(l.url || '') + (l.version ? t('servers.version_suffix', ' - version {v}', { v: esc(l.version) }) : '') + '</div>' +
        (l.error ? '<div class="hint">' + esc(l.error) + '</div>' : '') +
        swHtml('data-share="' + esc(l.id) + '"', l.share, t('servers.share_cameras', 'Share this server\'\'s cameras with {name}', { name: esc(l.name) }), t('servers.live_view_only', 'Live view only.')) +
        swHtml('data-show="' + esc(l.id) + '"', l.show, t('servers.show_cameras', 'Show {name}\'\'s cameras here', { name: esc(l.name) })) +
        '<details class="sh-more"><summary>' + t('servers.more_settings', 'More settings') + '</summary><label class="srv-opt" title="' + esc(t('servers.signin_title', 'Opening this server from {name}\'\'s menu signs you in here as the same account. {name} is trusted to say who you are, so only allow what you trust it with.', { name: l.name })) + '">' + t('servers.signin_from', 'Sign-in from {name}:', { name: esc(l.name) }) + ' <select data-signin="' + esc(l.id) + '">' +
          [['viewers', t('servers.signin_viewers', 'same account, never as admin (recommended)')], ['all', t('servers.signin_all', 'same account, admins too')], ['off', t('servers.signin_off', 'off (sign in here yourself)')]].map(([v, lbl]) => '<option value="' + v + '"' + ((l.signin || 'viewers') === v ? ' selected' : '') + '>' + lbl + '</option>').join('') + '</select></label><div class="hint">' + t('servers.signin_hint', 'Admins sign in here themselves when they switch; other accounts carry over. With "admins too", any admin on {name} can act as an admin here, even after you disable their account here.', { name: esc(l.name) }) + '</div></details>' +
        (l.show && l.show_error ? '<div class="hint">' + esc(l.show_error) + '</div>' : '') + '</div>' +
      '<span class="badge dot ' + (l.online ? 'on' : 'off') + '">' + (l.online ? t('servers.reachable', 'reachable') : t('servers.not_reachable', 'not reachable')) + '</span>' +
      '<div class="lk-acts"><button class="btn" data-addr="' + esc(l.id) + '" title="' + esc(t('servers.change_address_title', 'Reach it at another address, e.g. its Tailscale IP')) + '">' + t('servers.change_address', 'Change address') + '</button><button class="btn" data-ren="' + esc(l.id) + '">' + t('servers.rename', 'Rename') + '</button><button class="btn danger" data-rm="' + esc(l.id) + '">' + t('servers.unlink', 'Unlink') + '</button></div></div>').join('') + '</div>'
      : '<p class="hint">' + t('servers.no_linked', 'No linked servers yet.') + '</p>';
    h += '<div class="row"><button class="btn" data-mkcode>' + ic('plus') + ' ' + t('servers.link_another_server', 'Let another server link to this one') + '</button><button class="btn" data-link>' + ic('plus') + ' ' + t('servers.link_to_another', 'Link to another server') + '</button></div>';
    h += how(t('servers.switching_info', "Switching signs you in on the other server as the user with the same name there, unless that is an admin account: admins sign in there themselves unless the other server allows otherwise (More settings). Each device must trust each server's certificate once (account menu, Trust this server). <b>Only link servers you control:</b> a linked server is trusted to say who is signing in, so an admin there can sign in here as any account with the same name."));
    host.innerHTML = h;
    host.querySelector('[data-saveself]').onclick = async () => {
      const name = host.querySelector('[data-selfname]').value.trim();
      try { await api('/api/links/self', { method: 'PUT', json: { name } }); toast(t('servers.now_called', 'This server is now called {name}', { name })); load(); }
      catch (e) { toast(errText(e)); }
    };
    host.querySelectorAll('[data-ren]').forEach(b => { b.onclick = async () => {
      const l = links.find(x => x.id === b.dataset.ren);
      const name = ((await promptDialog({ title: t('servers.rename_server', 'Rename server'), label: t('servers.name_label', 'Name for this server'), value: l.name, ok: t('servers.rename_ok', 'Rename') })) || '').trim(); if (!name || name === l.name) return;
      try { await api('/api/links/' + encodeURIComponent(l.id), { method: 'PUT', json: { name } }); load(); } catch (e) { toast(errText(e)); }
    }; });
    host.querySelectorAll('[data-addr]').forEach(b => { b.onclick = async () => {
      const l = links.find(x => x.id === b.dataset.addr);
      const url = ((await promptDialog({ title: t('servers.change_address', 'Change address'), label: t('servers.address_of', 'Address of {name} (checked before it is saved)', { name: l.name }), value: l.url || '', placeholder: t('servers.address_placeholder', 'https://192.168.1.20:8090'), ok: t('servers.change_ok', 'Change') })) || '').trim();
      if (!url || url === l.url) return;
      b.disabled = true;
      try { await api('/api/links/' + encodeURIComponent(l.id), { method: 'PUT', json: { url } }); toast(t('servers.reached_at', '{name} is now reached at {url}', { name: l.name, url })); load(); }
      catch (e) { toast(errText(e)); b.disabled = false; }
    }; });
    host.querySelectorAll('[data-rm]').forEach(b => { b.onclick = async () => {
      const l = links.find(x => x.id === b.dataset.rm);
      if (!(await confirmDialog({ title: t('servers.unlink_title', 'Unlink {name}?', { name: l.name }), body: t('servers.unlink_body', 'Both servers forget the link, and the cameras shared between them disappear from each other. To link again you need a new code.'), ok: t('servers.unlink_ok', 'Unlink'), danger: true }))) return;
      try { const r = await api('/api/links/' + encodeURIComponent(l.id), { method: 'DELETE' }); toast(r.other_side_told ? t('servers.unlinked', 'Unlinked') : t('servers.unlinked_here', 'Unlinked here; {name} could not be told (unlink it there too)', { name: l.name })); load(); }
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
  host.innerHTML = '<p class="empty">' + t('core.loading', 'Loading...') + '</p>';
  load();
}

async function codeDialog() {
  let r;
  try { r = await api('/api/links/code', { json: {} }); } catch (e) { toast(errText(e)); return; }
  const d = openDlg(t('servers.link_code_for', 'Link code for {name}', { name: r.name }),
    '<div class="hint">' + t('servers.link_code_hint', 'On the other server: Settings &rsaquo; Servers &rsaquo; Link to another server, and enter:') + '</div>' +
    '<div class="sec">' + t('servers.this_servers_address', 'This server\'s address') + '</div><code class="tlsfp">' + esc(location.origin) + '</code>' +
    '<div class="sec">' + t('servers.code_once', 'Code (works once, for 15 minutes)') + '</div><code class="tlsfp lk-code">' + esc(grouped(r.code)) + '</code>' +
    '<div class="sec">' + t('servers.fp_compare', 'Certificate fingerprint to compare') + '</div><code class="tlsfp">' + esc(r.fingerprint || '') + '</code>' +
    '<div class="row"><button class="btn" data-close>' + t('servers.close', 'Close') + '</button></div>', true);
  d.querySelector('[data-close]').onclick = () => d.__close();
}

function linkDialog(done) {
  const d = openDlg(t('servers.link_to_another', 'Link to another server'),
    '<label class="cs-f"><span>' + t('servers.its_address', 'Its address') + '</span><input data-url placeholder="https://cabin:8090" spellcheck="false"></label>' +
    '<label class="cs-f"><span>' + t('servers.its_link_code', 'Its link code') + '</span><input data-code placeholder="XXXX-XXXX-XXXX" spellcheck="false" autocomplete="off"></label>' +
    '<div class="hint">' + t('servers.make_code_hint', 'Make the code on the other server: Settings &rsaquo; Servers &rsaquo; Let another server link to this one.') + '</div>' +
    '<details><summary class="hint">' + t('servers.own_address_summary', 'This server\'s address, as the other server reaches it') + '</summary><input data-own value="' + esc(location.origin) + '" spellcheck="false"><div class="hint">' + t('servers.own_address_hint', 'Change it if the other server reaches this one by another name (e.g. a Tailscale name).') + '</div></details>' +
    '<div data-step2 hidden><div class="sec">' + t('servers.compare_fp', 'Compare the certificate fingerprint') + '</div><code class="tlsfp" data-fp></code>' +
      '<div class="hint">' + t('servers.fp_must_match', 'It must match the fingerprint shown with the code on the other server. If not, do not link.') + '</div></div>' +
    '<div class="row"><button class="btn primary" data-go>' + t('servers.check', 'Check') + '</button><button class="btn" data-cancel>' + t('servers.cancel', 'Cancel') + '</button></div>', true);
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
        b.textContent = t('servers.it_matches', 'It matches: link');
      } else {
        const r = await api('/api/links', { json: { url: probed.url, code: q('[data-code]').value, fingerprint: probed.fingerprint, own_url: q('[data-own]').value } });
        toast(t('servers.linked_with', 'Linked with {name}', { name: r.name }));
        d.__close();
        done();
        return;
      }
    } catch (e) { toast(errText(e)); probed = probed && q('[data-step2]').hidden ? null : probed; }
    b.disabled = false;
  };
  q('[data-url]').oninput = () => { probed = null; q('[data-step2]').hidden = true; q('[data-go]').textContent = t('servers.check', 'Check'); };
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
    btn.title = t('servers.switch_server','Switch server');
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
  m.innerHTML = '<div class="hint">' + t('servers.checking', 'Checking...') + '</div>';
  const r = btn.getBoundingClientRect();
  m.style.left = r.left + 'px'; m.style.top = (r.bottom + 4) + 'px';
  m.onclick = (e) => e.stopPropagation();
  document.body.appendChild(m);
  let data;
  try { data = await api('/api/links?check=1'); } catch (e) { m.innerHTML = '<div class="hint">' + esc(errText(e)) + '</div>'; return; }
  m.innerHTML = '<div class="srv-here">' + ic('eye') + ' ' + esc(data.name) + ' <span class="muted">' + t('servers.this_one','(this one)') + '</span></div>' +
    (data.links || []).map(l => '<button data-open="' + esc(l.id) + '"' + (l.online ? '' : ' disabled') + '><span class="srv-dot ' + (l.online ? 'on' : 'off') + '" aria-hidden="true"></span>' + esc(l.name) + (l.online ? ' <span class="muted">' + t('servers.online', 'online') + '</span>' : ' <span class="muted">' + t('servers.not_reachable','not reachable') + '</span>') + '</button>').join('') +
    (state.isAdmin ? '<div class="srv-sep"></div><button data-manage>' + t('servers.manage_servers','Manage servers...') + '</button>' : '');
  m.querySelectorAll('[data-open]').forEach(b => { b.onclick = async () => {
    b.disabled = true; b.textContent = t('servers.opening', 'Opening...');
    try { const o = await api('/api/links/' + encodeURIComponent(b.dataset.open) + '/open', { json: {} }); (window.top || window).location.href = o.url; }
    catch (e) { toast(errText(e)); m.remove(); }
  }; });
  const mg = m.querySelector('[data-manage]');
  if (mg) mg.onclick = async () => { m.remove(); const { openSettingsDialog } = await import('./settings.js'); openSettingsDialog('servers'); };
}
