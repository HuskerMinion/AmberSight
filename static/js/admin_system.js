// admin_system.js - Admin pages: Users, Logs, Audit (Status is in status_page.js).
import { api, esc, toast, openDlg, fmtTime, state, ic, enc, downloadUrl, confirmDialog, bus } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, notice, more } from './sp_kit.js';

const errText = (e) => (e && e.body) ? e.body : (e && e.message) || String(e);
const wrap = (html) => '<div class="asys-scroll">' + html + '</div>';

// ---------------------------------------------------------------- Users
function usersPage(el) {
  el.innerHTML = '<div class="asys">' + lead('Who can sign in, and which cameras each person can see.', 'Give everyone their own account, and make most of them Viewers.') +
    '<div class="asys-head"><span class="hint" id="usHint"></span>' +
    '<button class="btn" id="usAdd">' + ic('plus') + ' Add user</button></div><div id="usBody"><p class="empty">Loading...</p></div></div>';
  let users = [];
  const body = el.querySelector('#usBody');
  // Two-step codes turned on or off for this account: the badges and an open
  // Edit dialog follow. (Gone with the page.)
  const onTwoStep = (v) => {
    if (!el.isConnected) { bus.off('two-step', onTwoStep); return; }
    const mine = users.find(x => x.username === state.user);
    if (mine) mine.two_step = v;
    if (twoStepBox && twoStepBox.isConnected && mine) accountBox(twoStepBox, mine, true, () => load());
    load();
  };
  if (el.__twoStep) bus.off('two-step', el.__twoStep);
  el.__twoStep = onTwoStep;
  bus.on('two-step', onTwoStep);
  let twoStepBox = null;

  async function load() {
    try {
      const d = await api('/api/users');
      users = d.users || [];
    } catch (e) { body.innerHTML = '<p class="empty">Could not load users: ' + esc(errText(e)) + '</p>'; return; }
    draw();
  }

  // Up to four camera chips, then "N more" that opens the rest.
  const camChips = (list) => {
    const chip = (c) => '<span class="badge">' + esc(c) + '</span>';
    if (list.length <= 4) return list.map(chip).join(' ');
    return list.slice(0, 4).map(chip).join(' ') + ' <details class="us-more"><summary>' + (list.length - 4) + ' more (' + list.length + ' cameras)</summary>' + list.slice(4).map(chip).join(' ') + '</details>';
  };

  function draw() {
    const admins = users.filter(u => u.admin && !u.disabled).length;
    el.querySelector('#usHint').textContent = users.length
      ? users.length + ' account' + (users.length === 1 ? '' : 's') + ', ' + admins + ' admin' + (users.some(u => u.disabled) ? ', ' + users.filter(u => u.disabled).length + ' disabled' : '')
      : '';
    el.querySelector('#usAdd').hidden = !users.length;
    if (!users.length) {
      body.innerHTML = '<div class="sp-empty"><h3>Sign-in is off</h3><p>Anyone on your network can open AmberSight, watch the cameras and change settings. ' +
        'The first account you create becomes an admin and turns sign-in on; you will then need to sign in.</p>' +
        '<div class="row"><button class="primary" type="button" id="usFirst">Create an admin account</button></div></div>';
      body.querySelector('#usFirst').onclick = () => edit(null, true);
      return;
    }
    body.innerHTML = '<div class="hint us-legend"><b>Admin</b>: everything, including settings and users. <b>Viewer</b>: watch and review, cannot change settings.' + (users.some(u => u.admin && !u.disabled && !u.two_step) ? ' We recommend two-step codes for admins: each person turns them on in <b>Your account</b>, in the menu at the top.' : '') + '</div>' + wrap('<table class="asys-tbl us-tbl"><thead><tr><th>User</th><th>Role</th><th>Cameras</th><th></th></tr></thead><tbody>' +
      users.map((u, i) => {
        const me = u.username === state.user;
        const cams = u.admin ? '<span class="muted">All (admin)</span>'
          : (u.cameras && u.cameras.length ? camChips(u.cameras) : '<span class="muted">All cameras</span>');
        return '<tr' + (u.disabled ? ' class="us-off"' : '') + '><td class="us-name"><b>' + esc(u.username) + '</b>' + (me ? ' <span class="badge info">you</span>' : '') + (u.disabled ? ' <span class="badge off">Disabled</span>' : '') + (u.two_step ? ' <span class="badge on" title="Signing in also takes a code from an app">Two-step on</span>' : '') + '</td>' +
          '<td>' + (u.admin ? '<span class="badge on">Admin</span>' : '<span class="badge">Viewer</span>') + '</td>' +
          '<td class="asys-cams">' + cams + '</td>' +
          '<td class="asys-act"><button class="btn" data-qr="' + i + '"' + (u.disabled ? ' disabled' : '') + ' title="Show a QR code to sign in on a phone" aria-label="Phone sign-in: show a QR code to sign in on a phone">' + ic('qr') + ' Phone sign-in</button> <button class="btn" data-ed="' + i + '">Edit</button> ' +
          '<button class="btn" data-off="' + i + '"' + (me ? ' disabled title="You cannot disable your own account"' : '') + '>' + (u.disabled ? 'Enable' : 'Disable') + '</button>' +
          '<button class="btn danger" data-del="' + i + '"' + (me ? ' disabled title="You cannot delete your own account"' : '') + '>Delete</button>' +
          (me ? '<div class="hint us-why">You cannot disable or delete your own account</div>' : '') + '</td></tr>';
      }).join('') + '</tbody></table>');
    body.querySelectorAll('[data-ed]').forEach(b => { b.onclick = () => edit(users[+b.dataset.ed]); });
    body.querySelectorAll('[data-qr]').forEach(b => { b.onclick = () => phoneQr(users[+b.dataset.qr]); });
    body.querySelectorAll('[data-del]').forEach(b => { b.onclick = () => del(users[+b.dataset.del]); });
    body.querySelectorAll('[data-off]').forEach(b => { b.onclick = () => setDisabled(users[+b.dataset.off]); });
  }

  function edit(u, first) {
    const isNew = !u;
    u = u || { username: '', admin: !!first, cameras: [] };
    const me = !isNew && u.username === state.user;
    const lastAdmin = !isNew && u.admin && users.filter(x => x.admin).length <= 1;
    const lock = me || lastAdmin || !!first;
    const camNames = state.cams.map(c => c.name);
    for (const c of u.cameras || []) if (!camNames.includes(c)) camNames.push(c);
    const d = openDlg(isNew ? 'Add user' : 'Edit ' + u.username,
      '<label>Username<input id="ufName" autocomplete="off" value="' + esc(u.username) + '"' + (isNew ? '' : ' disabled') + '></label>' +
      (me ? '<div class="hint">To change your own password, use Change password in the menu at the top.</div>'
        : '<label>' + (isNew ? 'Password' : 'New password') + '<input id="ufPw" type="password" autocomplete="new-password" placeholder="' + (isNew ? 'at least 8 characters' : 'leave empty to keep the current one') + '"></label>') +
      '<label class="asys-chk"><input type="checkbox" id="ufAdmin"' + (u.admin ? ' checked' : '') + (lock ? ' disabled' : '') + '> Administrator (full access, all cameras)</label>' +
      (lock ? '<div class="hint">' + (first ? 'The first account is always an admin.' : me ? 'You cannot remove your own admin rights.' : 'This is the last admin; make another user an admin first.') + '</div>' : '') +
      (!isNew ? '<div id="ufAcct"></div>' : '') +
      '<div id="ufCamsBox"><div class="sec">Cameras this user may see</div><div class="hint">None ticked = all cameras.</div>' +
      '<div class="asys-camlist">' + (camNames.length ? camNames.map(n => '<label class="asys-chk"><input type="checkbox" data-cam="' + esc(n) + '"' + ((u.cameras || []).includes(n) ? ' checked' : '') + '> ' + esc(n) + '</label>').join('') : '<span class="muted">No cameras configured.</span>') + '</div></div>' +
      '<div class="asys-err" id="ufErr"></div>' +
      '<div class="row"><button class="primary" id="ufSave">' + (isNew ? 'Add user' : 'Save') + '</button><button class="btn" id="ufCancel">Cancel</button></div>');
    const adm = d.querySelector('#ufAdmin'), box = d.querySelector('#ufCamsBox');
    const sync = () => { box.hidden = adm.checked; };
    adm.onchange = sync; sync();
    d.querySelector('#ufCancel').onclick = () => d.__close();
    if (!isNew) accountBox(d.querySelector('#ufAcct'), u, me, () => { load(); });
    if (me) twoStepBox = d.querySelector('#ufAcct');
    d.querySelector('#ufSave').onclick = async () => {
      const err = d.querySelector('#ufErr');
      err.textContent = '';
      const name = d.querySelector('#ufName').value.trim();
      const pw = me ? '' : d.querySelector('#ufPw').value;
      if (!name) { err.textContent = 'Enter a username.'; return; }
      if (isNew && !pw) { err.textContent = 'Enter a password.'; return; }
      const payload = { username: name, admin: adm.checked, cameras: adm.checked ? [] : [...d.querySelectorAll('[data-cam]:checked')].map(x => x.dataset.cam) };
      if (pw) payload.password = pw;
      const save = async (proof) => {
        await api('/api/users', { json: Object.assign({}, payload, proof) });
        d.__close();
        load();
        if (isNew) toast('User added. Tap here to sign them in on their phone.', () => phoneQr({ username: name, admin: adm.checked }));
        else toast('User saved');
      };
      // Making an admin, or setting someone else's password, takes your own password.
      const reason = users.length && adm.checked && (isNew || !u.admin) ? 'Making ' + name + ' an admin gives them everything, so it takes your own password.'
        : !isNew && !me && pw ? 'Setting the password of ' + name + ' takes your own password.' : '';
      if (reason) {
        (await import('./account.js')).confirmWithPassword({ reason, ok: 'Confirm', call: save });
        return;
      }
      try { await save({}); } catch (e) { err.textContent = errText(e); }
    };
    setTimeout(() => { const f = d.querySelector(isNew ? '#ufName' : '#ufPw'); if (f) f.focus(); }, 0);
  }

  // In Edit user: their two-step state and device tokens. An admin can help someone
  // who lost a phone and cut off a token, but never read one.
  async function accountBox(box, u, me, changed) {
    let tokens = [];
    try { tokens = (await api('/api/users/' + enc(u.username) + '/tokens')).tokens || []; }
    catch (e) { box.innerHTML = '<div class="hint">Could not load device tokens: ' + esc(errText(e)) + '</div>'; return; }
    box.innerHTML = '<div class="sec">Two-step codes</div>' +
      (u.two_step
        ? (me ? '<div class="hint">On.</div><div class="row"><button class="btn" type="button" data-ts-acct>Manage in Your account</button></div>'
          : '<div class="hint">On. If they lost their phone and their recovery codes, you can turn it off so they can sign in with their password.</div><div class="row"><button class="btn" type="button" data-ts-off>Turn off two-step codes</button></div>')
        : me ? '<div class="hint">Off. Signing in would also take a 6-digit code from an app on your phone, so a stolen password alone will not get anyone in.</div><div class="row"><button class="btn" type="button" data-ts-on>Turn on two-step codes</button></div>'
          : '<div class="hint">Off. Only ' + esc(u.username) + ' can turn it on (it needs their phone): in Your account, in the menu at the top.</div>') +
      '<div class="sec">Device tokens</div>' +
      (tokens.length ? '<ul class="us-tok-list">' + tokens.map(t => '<li><span>' + esc(t.name) + ' <span class="muted">' + (t.scope === 'admin' ? 'can control' : 'watch only') + ', last used ' + (t.last_used ? esc(new Date(t.last_used * 1000).toLocaleDateString()) : 'never') + '</span></span><button class="btn" type="button" data-tk="' + esc(t.id) + '" data-name="' + esc(t.name) + '">Revoke</button></li>').join('') + '</ul>' : '<div class="hint">None.</div>');
    const on = box.querySelector('[data-ts-on]');
    if (on) on.onclick = async () => { (await import('./account.js')).startTwoStep(null); };
    const acct = box.querySelector('[data-ts-acct]');
    if (acct) acct.onclick = async () => { (await import('./account.js')).openAccount(); };
    const off = box.querySelector('[data-ts-off]');
    if (off) off.onclick = async () => {
      (await import('./account.js')).confirmWithPassword({
        reason: u.username + ' can then sign in with only their password, and is signed out everywhere now. This is in the audit log, and it takes your own password.',
        ok: 'Turn off', danger: true,
        call: async (proof) => {
          await api('/api/users/' + enc(u.username) + '/two-step/off', { json: proof });
          toast('Two-step codes are off for ' + u.username); u.two_step = false; accountBox(box, u, me, changed); changed();
        },
      });
    };
    box.querySelectorAll('[data-tk]').forEach(b => {
      b.onclick = async () => {
        if (!await confirmDialog({ title: 'Revoke ' + b.dataset.name + '?', body: 'It stops working now, and video it has open ends within seconds.', ok: 'Revoke', danger: true })) return;
        try { await api('/api/users/' + enc(u.username) + '/tokens/' + enc(b.dataset.tk), { method: 'DELETE' }); toast('Token revoked'); accountBox(box, u, me, changed); changed(); }
        catch (e) { toast(errText(e), { kind: 'err' }); }
      };
    });
  }

  // A one-time QR code that opens AmberSight on a phone already signed in as `u`.
  async function phoneQr(u) {
    // Addresses the phone could use: this page's, plus the names on the certificate.
    const port = location.port ? ':' + location.port : '';
    const loopback = /^(localhost|127\.|\[?::1\]?$)/.test(location.hostname);
    const origins = [];
    if (!loopback) origins.push(location.origin);
    try {
      const t = await api('/api/tls');
      for (const n of t.names || []) {
        if (/^(localhost|127\.|::1$)/.test(n)) continue;
        const o = 'https://' + (n.includes(':') ? '[' + n + ']' : n) + port;
        if (!origins.includes(o)) origins.push(o);
      }
    } catch (e) { /* the page address is enough */ }
    // Without HTTPS there are no certificate names: offer this computer's
    // own network addresses (a page opened as localhost is no use to a phone).
    let primary = null;
    if (location.protocol === 'http:') {
      try {
        const a = await api('/api/addresses');
        for (const ip of a.addresses || []) {
          const o = 'http://' + (ip.includes(':') ? '[' + ip + ']' : ip) + port;
          if (!primary) primary = o; // the server lists the one it reaches the network with first
          if (!origins.includes(o)) origins.push(o);
        }
      } catch (e) { /* the page address is enough */ }
    }
    // Home-network addresses first (what a phone on the Wi-Fi reaches).
    // (172.16-31 is often a virtual adapter's, e.g. Hyper-V or Docker.)
    const rank = (o) => /\/\/(192\.168\.|10\.)/.test(o) ? 0 : /\/\/172\.(1[6-9]|2\d|3[01])\./.test(o) ? 1 : /\/\/[\d.]+[:\/]?/.test(o) || o.includes('[') ? 2 : 1;
    const page = loopback ? null : location.origin;
    const first = (o) => o === page ? -2 : o === primary ? -1 : rank(o);
    origins.sort((x, y) => first(x) - first(y));
    if (!origins.length) origins.push(location.origin);
    const d = openDlg('Phone sign-in for ' + u.username,
      '<div class="asys-qr"><div class="asys-qrimg" id="pqImg"><p class="muted">Making a code...</p></div>' +
      '<div class="asys-qrside">' +
      '<ol class="asys-qrsteps"><li>Open the camera app on ' + esc(u.username) + '\'s phone and point it at the code.</li>' +
      '<li>Tap the link that appears. AmberSight opens already signed in as <b>' + esc(u.username) + '</b>.</li>' +
      '<li>Add it to the home screen when asked, so it opens like an app.</li></ol>' +
      (origins.length > 1 ? '<label>Address the phone will use<select id="pqOrigin">' + origins.map(o => '<option>' + esc(o) + '</option>').join('') + '</select></label>' : '') +
      '<div class="hint" id="pqLeft"></div>' +
      '<div class="hint">The code works once, only for this account, and expires after 10 minutes. No password is in it.' +
      '<span id="pqCert"> If the phone warns that the connection is not private, install the certificate first (account menu, Trust this server) or continue past the warning.</span></div>' +
      '<div class="asys-err" id="pqErr"></div>' +
      '<div class="row"><button class="btn" id="pqNew">New code</button><button class="primary" id="pqDone">Done</button></div></div></div>');
    const img = d.querySelector('#pqImg'), left = d.querySelector('#pqLeft'), err = d.querySelector('#pqErr');
    let until = 0, timer = null;
    const tickLeft = () => {
      if (!d.isConnected) { clearInterval(timer); return; }
      const s = Math.max(0, Math.round((until - Date.now()) / 1000));
      left.textContent = s ? 'Expires in ' + Math.floor(s / 60) + ':' + String(s % 60).padStart(2, '0') : 'This code has expired. Make a new one.';
      if (!s) img.classList.add('expired');
    };
    async function make() {
      err.textContent = '';
      // Stop the old code's countdown: while the password dialog below is open
      // it would keep re-marking the image expired, and that outlives the new code.
      clearInterval(timer);
      img.classList.remove('expired');
      const sel = d.querySelector('#pqOrigin');
      const origin = sel ? sel.value : origins[0];
      // The certificate advice only applies to an https:// address.
      d.querySelector('#pqCert').hidden = !origin.startsWith('https:');
      try {
        const get = async (proof) => {
          const r = await api('/api/users/' + enc(u.username) + '/pair', { json: Object.assign({ origin }, proof) });
          img.innerHTML = r.svg;
          img.classList.remove('expired'); // a fresh code is not expired, even if the old one lapsed while confirming
          until = Date.now() + r.expires_in * 1000;
          clearInterval(timer); timer = setInterval(tickLeft, 1000); tickLeft();
        };
        // A code for an admin account signs in with all those rights, and no password.
        if (u.admin || u.two_step) {
          (await import('./account.js')).confirmWithPassword({ reason: 'This QR code signs in as ' + u.username + (u.admin ? ', an admin,' : '') + ' with no password or code, so it takes your own password.', ok: 'Make code', call: get });
          return;
        }
        await get({});
      } catch (e) { err.textContent = errText(e); }
    }
    d.querySelector('#pqNew').onclick = make;
    const sel = d.querySelector('#pqOrigin');
    if (sel) sel.onchange = make;
    d.querySelector('#pqDone').onclick = () => { clearInterval(timer); d.__close(); };
    make();
  }

  async function setDisabled(u) {
    const off = !u.disabled;
    const yes = await confirmDialog(off
      ? { title: 'Disable ' + u.username + '?', body: u.username + ' is signed out everywhere within seconds, any video they are watching stops, and they cannot sign in or get notifications. Their device tokens are deleted; nothing else is, and only an admin can enable it. After that, each device has to sign in again. Alert services shared by everyone (email, ntfy and so on), linked servers and camera passwords they knew are separate: change those too if needed.', ok: 'Disable ' + u.username, danger: true }
      : { title: 'Enable ' + u.username + '?', body: u.username + ' can sign in again with their password. Devices that were signed in must sign in again.', ok: 'Enable ' + u.username });
    if (!yes) return;
    try { await api('/api/users', { json: { username: u.username, disabled: off } }); toast(off ? u.username + ' is disabled' : u.username + ' is enabled'); load(); }
    catch (e) { toast(errText(e), { kind: 'err' }); }
  }

  function del(u) {
    confirmDialog({ title: 'Delete user ' + u.username + '?', body: u.username + ' is signed out on every device and cannot sign in again. Their phone app and device tokens stop working.', ok: 'Delete ' + u.username, danger: true }).then(async (yes) => {
      if (!yes) return;
      try { await api('/api/users/' + enc(u.username), { method: 'DELETE' }); toast('User deleted'); load(); }
      catch (e) { toast(errText(e), { kind: 'err' }); }
    });
  }

  el.querySelector('#usAdd').onclick = () => edit(null);
  load();
}

// ---------------------------------------------------------------- Logs
let logsTimer = null;
function logsPage(el) {
  el.innerHTML = '<div class="asys"><div class="asys-head"><span class="hint" id="lgCount"></span></div>' +
    '<div class="asys-bar-row">' +
    '<select id="lgLevel" title="Minimum level"><option value="error">Errors</option><option value="warn">Warnings+</option><option value="info" selected>Info+</option></select>' +
    '<input id="lgText" type="search" placeholder="Filter text..." autocomplete="off">' +
    '<select id="lgLimit" title="Lines to load"><option>300</option><option>1000</option><option>2000</option></select>' +
    '<label class="asys-chk"><input type="checkbox" id="lgFollow" checked> Follow</label>' +
    '<span class="grow"></span>' +
    '<button class="btn" id="lgCopy">Copy</button><button class="btn" id="lgDl">Download</button></div>' +
    '<pre class="asys-log" id="lgOut"></pre></div>';
  const out = el.querySelector('#lgOut'), $ = (s) => el.querySelector(s);
  let lines = [], busy = false, sig = '';

  const vis = () => {
    const q = $('#lgText').value.trim().toLowerCase();
    return q ? lines.filter(l => (l.level + ' ' + l.target + ' ' + l.message).toLowerCase().includes(q)) : lines;
  };
  const clock = (l) => new Date(l.ts * 1000).toLocaleTimeString([], { hour12: false });
  const fmtLine = (l) => clock(l) + ' ' + String(l.level).padEnd(5) + ' ' + l.target + ': ' + l.message;
  function draw() {
    const v = vis();
    $('#lgCount').textContent = v.length + ' of ' + lines.length + ' lines';
    out.innerHTML = v.length ? v.map(l => '<div class="lv-' + esc(String(l.level).toLowerCase()) + '"><span class="t">' + esc(clock(l)) + '</span> <span class="l">' + esc(l.level) + '</span> <span class="g">' + esc(l.target) + '</span> ' + esc(l.message) + '</div>').join('') : '<div class="muted">No log lines.</div>';
    if ($('#lgFollow').checked) out.scrollTop = out.scrollHeight;
  }
  async function tick() {
    if (busy || !el.isConnected) return;
    busy = true;
    try {
      const d = await api('/api/logs?level=' + $('#lgLevel').value + '&limit=' + $('#lgLimit').value);
      const nl = d.lines || [];
      const s = nl.length + ':' + (nl.length ? nl[nl.length - 1].ts + nl[nl.length - 1].message : '');
      if (s !== sig) { sig = s; lines = nl; draw(); }
    } catch (e) { out.textContent = 'Could not load logs: ' + errText(e); }
    busy = false;
  }
  $('#lgLevel').onchange = $('#lgLimit').onchange = () => { sig = ''; tick(); };
  $('#lgText').oninput = draw;
  $('#lgFollow').onchange = () => { if ($('#lgFollow').checked) out.scrollTop = out.scrollHeight; };
  const text = () => vis().map(fmtLine).join('\n') + '\n';
  $('#lgCopy').onclick = async () => {
    try { await navigator.clipboard.writeText(text()); toast('Copied ' + vis().length + ' lines'); }
    catch (e) {
      const r = document.createRange(); r.selectNodeContents(out); const sel = getSelection(); sel.removeAllRanges(); sel.addRange(r);
      toast('Selected: press Ctrl+C to copy');
    }
  };
  $('#lgDl').onclick = () => {
    const u = URL.createObjectURL(new Blob([text()], { type: 'text/plain' }));
    downloadUrl(u, 'ambersight-log-' + new Date().toISOString().slice(0, 19).replace(/[:T]/g, '-') + '.txt');
    setTimeout(() => URL.revokeObjectURL(u), 5000);
  };
  tick();
  logsLeave();
  logsTimer = setInterval(() => { if (!document.hidden) tick(); }, 3000);
}
function logsLeave() { clearInterval(logsTimer); logsTimer = null; }

// ---------------------------------------------------------------- Audit
function auditPage(el) {
  el.innerHTML = '<div class="asys"><div class="asys-head"><span class="hint" id="auCount"></span></div>' +
    '<div class="asys-bar-row">' +
    '<select id="auUser" title="User"><option value="">All users</option></select>' +
    '<select id="auKind" title="What"><option value="">Everything</option><option value="sign">Sign-ins</option><option value="chg">Changes</option><option value="fail">Failures</option></select>' +
    '<input id="auText" type="search" placeholder="Filter text..." autocomplete="off">' +
    '<select id="auLimit" title="Entries to load"><option>200</option><option>1000</option><option>5000</option></select>' +
    '<span class="grow"></span><button class="btn" id="auNow">' + ic('swap') + ' Refresh</button></div>' +
    '<div id="auBody"><p class="empty">Loading...</p></div><div class="asys-pager" id="auPager"></div></div>';
  const $ = (s) => el.querySelector(s);
  const PER = 50;
  let rows = [], page = 0;

  const isSign = (r) => /^sign-/.test(r.action);
  function filtered() {
    const u = $('#auUser').value, k = $('#auKind').value, q = $('#auText').value.trim().toLowerCase();
    return rows.filter(r => (!u || r.user === u) &&
      (!k || (k === 'sign' ? isSign(r) : k === 'chg' ? !isSign(r) : r.status >= 400)) &&
      (!q || (r.user + ' ' + r.ip + ' ' + r.action + ' ' + r.detail + ' ' + r.status).toLowerCase().includes(q)));
  }
  function what(r) {
    if (isSign(r)) return '<b>' + esc(r.action.charAt(0).toUpperCase() + r.action.slice(1).replace('-', ' ')) + '</b>' + (r.detail ? ' <span class="asys-path">' + esc(r.detail) + '</span>' : '');
    return '<span class="badge">' + esc(r.action) + '</span> <span class="asys-path">' + esc(r.detail) + '</span>';
  }
  function draw() {
    const f = filtered();
    const pages = Math.max(1, Math.ceil(f.length / PER));
    page = Math.min(page, pages - 1);
    $('#auCount').textContent = f.length + ' of ' + rows.length + ' entries';
    const slice = f.slice(page * PER, page * PER + PER);
    $('#auBody').innerHTML = slice.length ? wrap('<table class="asys-tbl"><thead><tr><th>When</th><th>User</th><th>What</th><th>Result</th><th>From</th></tr></thead><tbody>' +
      slice.map(r => '<tr><td class="nowrap">' + esc(fmtTime(r.ts)) + '</td><td>' + esc(r.user || '-') + '</td><td>' + what(r) + '</td>' +
        '<td><span class="badge ' + (r.status >= 400 ? 'off' : 'on') + '">' + r.status + '</span></td><td class="muted">' + esc(r.ip) + '</td></tr>').join('') + '</tbody></table>') : '<p class="empty">No matching entries.</p>';
    $('#auPager').innerHTML = pages > 1 ? '<button class="btn" id="auPrev"' + (page ? '' : ' disabled') + '>' + ic('left') + ' Newer</button><span class="muted">Page ' + (page + 1) + ' of ' + pages + '</span><button class="btn" id="auNext"' + (page < pages - 1 ? '' : ' disabled') + '>Older ' + ic('right') + '</button>' : '';
    const p = $('#auPrev'), n = $('#auNext');
    if (p) p.onclick = () => { page--; draw(); };
    if (n) n.onclick = () => { page++; draw(); };
  }
  async function load() {
    try {
      const d = await api('/api/audit?limit=' + $('#auLimit').value);
      rows = d.entries || [];
      const sel = $('#auUser'), cur = sel.value;
      const users = [...new Set(rows.map(r => r.user).filter(Boolean))].sort();
      sel.innerHTML = '<option value="">All users</option>' + users.map(u => '<option' + (u === cur ? ' selected' : '') + ' value="' + esc(u) + '">' + esc(u) + '</option>').join('');
      draw();
    } catch (e) { $('#auBody').innerHTML = '<p class="empty">Could not load the audit log: ' + esc(errText(e)) + '</p>'; }
  }
  const re = () => { page = 0; draw(); };
  $('#auUser').onchange = $('#auKind').onchange = re;
  $('#auText').oninput = re;
  $('#auLimit').onchange = load;
  $('#auNow').onclick = load;
  load();
}

registerAdminPage({ id: 'users', title: 'Users', order: 10, render: usersPage });
registerAdminPage({ id: 'logs', title: 'Logs', order: 30, render: logsPage, leave: logsLeave });
registerAdminPage({ id: 'audit', title: 'Audit', order: 40, render: auditPage });
