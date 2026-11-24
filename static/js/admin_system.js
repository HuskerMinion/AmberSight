// admin_system.js - Admin pages: Users, Logs, Audit (Status is in status_page.js).
import { t } from './i18n.js';
import { api, esc, toast, openDlg, fmtTime, state, ic, enc, downloadUrl, confirmDialog, bus } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, notice, more } from './sp_kit.js';

const errText = (e) => (e && e.body) ? e.body : (e && e.message) || String(e);
const wrap = (html) => '<div class="asys-scroll">' + html + '</div>';

// ---------------------------------------------------------------- Users
const ROLES = [
  ['viewer', t('admin_system.role_viewer', 'Viewer'), t('admin_system.role_viewer_desc', 'watch live video, events and recordings')],
  ['operator', t('admin_system.role_operator', 'Operator'), t('admin_system.role_operator_desc', 'also day-to-day control, as ticked below')],
  ['admin', t('admin_system.role_admin', 'Admin'), t('admin_system.role_admin_desc', 'everything, including settings and users (all cameras)')],
];
// An operator's rights (the server's names), each on unless unticked.
const PERMS = [
  ['ptz', t('admin_system.perm_ptz', 'Move PTZ cameras, go to presets, click to aim')],
  ['talk_clip', t('admin_system.perm_talk', 'Talk through a camera’s speaker, record a clip now')],
  ['arm', t('admin_system.perm_arm', 'Arm and disarm, switch profiles, pause notifications (an operator limited to some cameras can only pause theirs: arming and profiles change every camera)')],
  ['events', t('admin_system.perm_events', 'Keep events forever, mark them “not a real alert” or “looks fine”, add timeline bookmarks')],
  ['camera_detection', t('admin_system.perm_detection', 'Change detection and alert settings of their cameras (zones, sensitivity, AI, alert times)')],
  ['lights', t('admin_system.perm_lights', 'Turn a camera’s spotlight on for a while, and off')],
  ['siren', t('admin_system.perm_siren', 'Sound a camera’s siren (off unless you tick it; never from a device token)')],
  ['reboot', t('admin_system.perm_reboot', 'Reboot cameras (off unless you tick it: a camera is offline for a minute or two and records nothing while it restarts; never from a device token)')],
];
// Every right is on for a new operator except the siren and rebooting cameras.
const OFF_BY_DEFAULT = ['siren', 'reboot'];
const ALL_PERMS = Object.fromEntries(PERMS.map(([k]) => [k, !OFF_BY_DEFAULT.includes(k)]));
// (A server before operators sends no role.)
const roleOf = (u) => u.role || (u.admin ? 'admin' : 'viewer');
function permsText(p) {
  const on = PERMS.filter(([k]) => (p && p[k] != null ? p[k] : ALL_PERMS[k]));
  if (on.length === PERMS.length) return t('admin_system.all_operator_rights', 'All operator rights, the siren and reboots too');
  if (PERMS.every(([k]) => on.some(([x]) => x === k) === ALL_PERMS[k])) return t('admin_system.usual_operator_rights', 'The usual operator rights (no siren, no reboots)');
  return on.length ? on.map(([, label]) => label).join('; ') : t('admin_system.no_operator_rights', 'No operator rights ticked');
}

function usersPage(el) {
  el.innerHTML = '<div class="asys">' + lead(t('admin_system.signin_guide', 'Who can sign in, and which cameras each person can see.'), t('admin_system.account_advice', 'Give everyone their own account, and make most of them Viewers.')) +
    '<div class="asys-head"><span class="hint" id="usHint"></span>' +
    '<button class="btn" id="usAdd">' + ic('plus') + ' ' + t('admin_system.add_user', 'Add user') + '</button></div><div id="usBody"><p class="empty">' + t('admin_system.loading', 'Loading...') + '</p></div></div>';
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
    } catch (e) { body.innerHTML = '<p class="empty">' + t('admin_system.load_users_failed', 'Could not load users: {error}', { error: esc(errText(e)) }) + '</p>'; return; }
    draw();
  }

  // Up to four camera chips, then "N more" that opens the rest.
  const camChips = (list) => {
    const chip = (c) => '<span class="badge">' + esc(c) + '</span>';
    if (list.length <= 4) return list.map(chip).join(' ');
    return list.slice(0, 4).map(chip).join(' ') + ' <details class="us-more"><summary>' + t('admin_system.n_more_cameras', '{n} more ({total} cameras)', { n: list.length - 4, total: list.length }) + '</summary>' + list.slice(4).map(chip).join(' ') + '</details>';
  };

  function draw() {
    const admins = users.filter(u => u.admin && !u.disabled).length;
    el.querySelector('#usHint').textContent = users.length
      ? t('admin_system.account_count', '{accounts, plural, one {# account} other {# accounts}}, {admins} admin{off, plural, =0 {} other {, {off} disabled}}', { accounts: users.length, admins, off: users.filter(u => u.disabled).length })
      : '';
    el.querySelector('#usAdd').hidden = !users.length;
    if (!users.length) {
      body.innerHTML = '<div class="sp-empty"><h3>' + t('admin_system.signin_off', 'Sign-in is off') + '</h3><p>' + t('admin_system.signin_off_desc', 'Anyone on your network can open AmberSight, watch the cameras and change settings. The first account you create becomes an admin and turns sign-in on; you will then need to sign in.') + '</p>' +
        '<div class="row"><button class="primary" type="button" id="usFirst">' + t('admin_system.create_admin', 'Create an admin account') + '</button></div></div>';
      body.querySelector('#usFirst').onclick = () => edit(null, true);
      return;
    }
    body.innerHTML = '<div class="hint us-legend">' + t('admin_system.legend_admin', '<b>Admin</b>: everything, including settings and users.') + ' ' + t('admin_system.legend_operator', '<b>Operator</b>: day-to-day control (arming, moving cameras, talking, keeping events, detection and alert settings of their cameras), as far as their rights go; no other settings.') + ' ' + t('admin_system.legend_viewer', '<b>Viewer</b>: watch and review, cannot change anything.') + (users.some(u => u.admin && !u.disabled && !u.two_step) ? ' ' + t('admin_system.recommend_two_step', 'We recommend two-step codes for admins: each person turns them on in <b>Your account</b>, in the menu at the top.') : '') + '</div>' + wrap('<table class="asys-tbl us-tbl"><thead><tr><th>' + t('admin_system.th_user', 'User') + '</th><th>' + t('admin_system.th_role', 'Role') + '</th><th>' + t('admin_system.th_cameras', 'Cameras') + '</th><th></th></tr></thead><tbody>' +
      users.map((u, i) => {
        const me = u.username === state.user;
        const cams = u.admin ? '<span class="muted">' + t('admin_system.all_admin', 'All (admin)') + '</span>'
          : (u.cameras && u.cameras.length ? camChips(u.cameras) : '<span class="muted">' + t('admin_system.all_cameras', 'All cameras') + '</span>');
        return '<tr' + (u.disabled ? ' class="us-off"' : '') + '><td class="us-name"><b>' + esc(u.username) + '</b>' + (me ? ' <span class="badge info">' + t('admin_system.you', 'you') + '</span>' : '') + (u.disabled ? ' <span class="badge off">' + t('admin_system.badge_disabled', 'Disabled') + '</span>' : '') + (u.two_step ? ' <span class="badge on" title="' + t('admin_system.two_step_title', 'Signing in also takes a code from an app') + '">' + t('admin_system.two_step_on', 'Two-step on') + '</span>' : '') + '</td>' +
          '<td>' + (u.admin ? '<span class="badge on">' + t('admin_system.role_admin', 'Admin') + '</span>' : roleOf(u) === 'operator' ? '<span class="badge info" title="' + esc(permsText(u.perms)) + '">' + t('admin_system.role_operator', 'Operator') + '</span>' : '<span class="badge">' + t('admin_system.role_viewer', 'Viewer') + '</span>') + '</td>' +
          '<td class="asys-cams">' + cams + '</td>' +
          '<td class="asys-act"><button class="btn" data-qr="' + i + '"' + (u.disabled ? ' disabled' : '') + ' title="' + t('admin_system.qr_title', 'Show a QR code to sign in on a phone') + '" aria-label="' + t('admin_system.phone_signin_aria', 'Phone sign-in: show a QR code to sign in on a phone') + '">' + ic('qr') + ' ' + t('admin_system.phone_signin', 'Phone sign-in') + '</button> <button class="btn" data-ed="' + i + '">' + t('admin_system.edit', 'Edit') + '</button> ' +
          '<button class="btn" data-off="' + i + '"' + (me ? ' disabled title="' + t('admin_system.cannot_disable_self', 'You cannot disable your own account') + '"' : '') + '>' + (u.disabled ? t('admin_system.enable', 'Enable') : t('admin_system.disable', 'Disable')) + '</button>' +
          '<button class="btn danger" data-del="' + i + '"' + (me ? ' disabled title="' + t('admin_system.cannot_delete_self', 'You cannot delete your own account') + '"' : '') + '>' + t('admin_system.delete', 'Delete') + '</button>' +
          (me ? '<div class="hint us-why">' + t('admin_system.cannot_self', 'You cannot disable or delete your own account') + '</div>' : '') + '</td></tr>';
      }).join('') + '</tbody></table>');
    body.querySelectorAll('[data-ed]').forEach(b => { b.onclick = () => edit(users[+b.dataset.ed]); });
    body.querySelectorAll('[data-qr]').forEach(b => { b.onclick = () => phoneQr(users[+b.dataset.qr]); });
    body.querySelectorAll('[data-del]').forEach(b => { b.onclick = () => del(users[+b.dataset.del]); });
    body.querySelectorAll('[data-off]').forEach(b => { b.onclick = () => setDisabled(users[+b.dataset.off]); });
  }

  function edit(u, first) {
    const isNew = !u;
    u = u || { username: '', admin: !!first, cameras: [] };
    const role0 = roleOf(u);
    const perms0 = Object.assign({}, ALL_PERMS, u.perms || {});
    const me = !isNew && u.username === state.user;
    const lastAdmin = !isNew && u.admin && users.filter(x => x.admin).length <= 1;
    const lock = me || lastAdmin || !!first;
    const camNames = state.cams.map(c => c.name);
    for (const c of u.cameras || []) if (!camNames.includes(c)) camNames.push(c);
    const d = openDlg(isNew ? t('admin_system.add_user', 'Add user') : t('admin_system.edit_user', 'Edit {username}', { username: u.username }),
      '<label>' + t('admin_system.username', 'Username') + '<input id="ufName" autocomplete="off" value="' + esc(u.username) + '"' + (isNew ? '' : ' disabled') + '></label>' +
      (me ? '<div class="hint">' + t('admin_system.change_own_password', 'To change your own password, use Change password in the menu at the top.') + '</div>'
        : '<label>' + (isNew ? t('admin_system.password', 'Password') : t('admin_system.new_password', 'New password')) + '<input id="ufPw" type="password" autocomplete="new-password" placeholder="' + (isNew ? t('admin_system.pw_min', 'at least 8 characters') : t('admin_system.pw_keep', 'leave empty to keep the current one')) + '"></label>') +
      '<fieldset class="us-role"><legend class="sec">' + t('admin_system.role', 'Role') + '</legend>' + ROLES.map(([v, label, h]) =>
        '<label class="asys-chk"><input type="radio" name="ufRole" value="' + v + '"' + (role0 === v ? ' checked' : '') + (lock ? ' disabled' : '') + '> <span><b>' + label + '</b> <span class="muted">' + h + '</span></span></label>').join('') + '</fieldset>' +
      (lock && u.admin ? '<div class="hint">' + (first ? t('admin_system.first_is_admin', 'The first account is always an admin.') : me ? t('admin_system.cannot_remove_own_admin', 'You cannot remove your own admin rights.') : t('admin_system.last_admin', 'This is the last admin; make another user an admin first.')) + '</div>' : '') +
      '<div id="ufPermsBox"><div class="sec">' + t('admin_system.operator_may', 'What this operator may do') + '</div><div class="hint">' + t('admin_system.perms_default', 'Ticked by default, except the siren and restarting cameras. Saving presets, other settings, users and storage stay with admins.') + '</div>' +
      PERMS.map(([k, label]) => '<label class="asys-chk"><input type="checkbox" data-perm="' + k + '"' + (perms0[k] ? ' checked' : '') + '> ' + label + '</label>').join('') + '</div>' +
      (!isNew ? '<div id="ufAcct"></div>' : '') +
      '<div id="ufCamsBox"><div class="sec">' + t('admin_system.cams_may_see', 'Cameras this user may see') + '</div><div class="hint">' + t('admin_system.cams_none_all', 'None ticked = all cameras. An operator controls only these too.') + '</div>' +
      '<div class="asys-camlist">' + (camNames.length ? camNames.map(n => '<label class="asys-chk"><input type="checkbox" data-cam="' + esc(n) + '"' + ((u.cameras || []).includes(n) ? ' checked' : '') + '> ' + esc(n) + '</label>').join('') : '<span class="muted">' + t('admin_system.no_cameras', 'No cameras configured.') + '</span>') + '</div></div>' +
      '<div class="asys-err" id="ufErr"></div>' +
      '<div class="row"><button class="primary" id="ufSave">' + (isNew ? t('admin_system.add_user', 'Add user') : t('admin_system.save', 'Save')) + '</button><button class="btn" id="ufCancel">' + t('admin_system.cancel', 'Cancel') + '</button></div>');
    const box = d.querySelector('#ufCamsBox'), pbox = d.querySelector('#ufPermsBox');
    const roleNow = () => { const r = d.querySelector('input[name="ufRole"]:checked'); return r ? r.value : role0; };
    const sync = () => { box.hidden = roleNow() === 'admin'; pbox.hidden = roleNow() !== 'operator'; };
    d.querySelectorAll('input[name="ufRole"]').forEach(r => { r.onchange = sync; });
    sync();
    d.querySelector('#ufCancel').onclick = () => d.__close();
    if (!isNew) accountBox(d.querySelector('#ufAcct'), u, me, () => { load(); });
    if (me) twoStepBox = d.querySelector('#ufAcct');
    d.querySelector('#ufSave').onclick = async () => {
      const err = d.querySelector('#ufErr');
      err.textContent = '';
      const name = d.querySelector('#ufName').value.trim();
      const pw = me ? '' : d.querySelector('#ufPw').value;
      if (!name) { err.textContent = t('admin_system.enter_username', 'Enter a username.'); return; }
      if (isNew && !pw) { err.textContent = t('admin_system.enter_password', 'Enter a password.'); return; }
      const role = roleNow(), isAdm = role === 'admin';
      const perms = {};
      d.querySelectorAll('[data-perm]').forEach(x => { perms[x.dataset.perm] = x.checked; });
      const payload = { username: name, role, cameras: isAdm ? [] : [...d.querySelectorAll('[data-cam]:checked')].map(x => x.dataset.cam) };
      if (role === 'operator') payload.perms = perms;
      if (pw) payload.password = pw;
      // (As the server judges it: more operator rights than now take your own password.)
      const morePerms = role === 'operator' && (role0 !== 'operator' || PERMS.some(([k]) => perms[k] && !perms0[k]));
      const save = async (proof) => {
        await api('/api/users', { json: Object.assign({}, payload, proof) });
        d.__close();
        load();
        if (isNew) toast(t('admin_system.user_added', 'User added. Tap here to sign them in on their phone.'), () => phoneQr({ username: name, admin: isAdm, role }));
        else toast(t('admin_system.user_saved', 'User saved'));
      };
      // Making an admin, or setting someone else's password, takes your own password.
      const reason = users.length && isAdm && (isNew || !u.admin) ? t('admin_system.making_admin', 'Making {name} an admin gives them everything, so it takes your own password.', { name })
        : users.length && morePerms && !u.admin ? t('admin_system.giving_operator', 'Giving {name} operator rights lets them disarm and control cameras, so it takes your own password.', { name })
        : !isNew && !me && pw ? t('admin_system.setting_password', 'Setting the password of {name} takes your own password.', { name }) : '';
      if (reason) {
        (await import('./account.js')).confirmWithPassword({ reason, ok: t('admin_system.confirm', 'Confirm'), call: save });
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
    catch (e) { box.innerHTML = '<div class="hint">' + t('admin_system.tokens_failed', 'Could not load device tokens: {error}', { error: esc(errText(e)) }) + '</div>'; return; }
    box.innerHTML = '<div class="sec">' + t('admin_system.two_step_codes', 'Two-step codes') + '</div>' +
      (u.two_step
        ? (me ? '<div class="hint">' + t('admin_system.two_step_on_hint', 'On.') + '</div><div class="row"><button class="btn" type="button" data-ts-acct>' + t('admin_system.manage_account', 'Manage in Your account') + '</button></div>'
          : '<div class="hint">' + t('admin_system.two_step_on_off', 'On. If they lost their phone and their recovery codes, you can turn it off so they can sign in with their password.') + '</div><div class="row"><button class="btn" type="button" data-ts-off>' + t('admin_system.turn_off_codes', 'Turn off two-step codes') + '</button></div>')
        : me ? '<div class="hint">' + t('admin_system.two_step_off_on', 'Off. Signing in would also take a 6-digit code from an app on your phone, so a stolen password alone will not get anyone in.') + '</div><div class="row"><button class="btn" type="button" data-ts-on>' + t('admin_system.turn_on_codes', 'Turn on two-step codes') + '</button></div>'
          : '<div class="hint">' + t('admin_system.two_step_off_only', 'Off. Only {username} can turn it on (it needs their phone): in Your account, in the menu at the top.', { username: esc(u.username) }) + '</div>') +
      '<div class="sec">' + t('admin_system.device_tokens', 'Device tokens') + '</div>' +
      (tokens.length ? '<ul class="us-tok-list">' + tokens.map(tk => '<li><span>' + esc(tk.name) + ' <span class="muted">' + t('admin_system.token_line', '{scope}, last used {when}', { scope: tk.scope === 'admin' ? t('admin_system.can_control', 'can control') : t('admin_system.watch_only', 'watch only'), when: tk.last_used ? esc(new Date(tk.last_used * 1000).toLocaleDateString()) : t('admin_system.never', 'never') }) + '</span></span><button class="btn" type="button" data-tk="' + esc(tk.id) + '" data-name="' + esc(tk.name) + '">' + t('admin_system.revoke', 'Revoke') + '</button></li>').join('') + '</ul>' : '<div class="hint">' + t('admin_system.none', 'None.') + '</div>');
    const on = box.querySelector('[data-ts-on]');
    if (on) on.onclick = async () => { (await import('./account.js')).startTwoStep(null); };
    const acct = box.querySelector('[data-ts-acct]');
    if (acct) acct.onclick = async () => { (await import('./account.js')).openAccount(); };
    const off = box.querySelector('[data-ts-off]');
    if (off) off.onclick = async () => {
      (await import('./account.js')).confirmWithPassword({
        reason: t('admin_system.turn_off_reason', '{username} can then sign in with only their password, and is signed out everywhere now. This is in the audit log, and it takes your own password.', { username: u.username }),
        ok: t('admin_system.turn_off', 'Turn off'), danger: true,
        call: async (proof) => {
          await api('/api/users/' + enc(u.username) + '/two-step/off', { json: proof });
          toast(t('admin_system.two_step_off_for', 'Two-step codes are off for {username}', { username: u.username })); u.two_step = false; accountBox(box, u, me, changed); changed();
        },
      });
    };
    box.querySelectorAll('[data-tk]').forEach(b => {
      b.onclick = async () => {
        if (!await confirmDialog({ title: t('admin_system.revoke_title', 'Revoke {name}?', { name: b.dataset.name }), body: t('admin_system.revoke_body', 'It stops working now, and video it has open ends within seconds.'), ok: t('admin_system.revoke', 'Revoke'), danger: true })) return;
        try { await api('/api/users/' + enc(u.username) + '/tokens/' + enc(b.dataset.tk), { method: 'DELETE' }); toast(t('admin_system.token_revoked', 'Token revoked')); accountBox(box, u, me, changed); changed(); }
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
      const tls = await api('/api/tls');
      for (const n of tls.names || []) {
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
    const d = openDlg(t('admin_system.phone_signin_for', 'Phone sign-in for {username}', { username: u.username }),
      '<div class="asys-qr"><div class="asys-qrimg" id="pqImg"><p class="muted">' + t('admin_system.qr_making_code', 'Making a code...') + '</p></div>' +
      '<div class="asys-qrside">' +
      '<ol class="asys-qrsteps"><li>' + t('admin_system.qr_step1', "Open the camera app on {username}'s phone and point it at the code.", { username: esc(u.username) }) + '</li>' +
      '<li>' + t('admin_system.qr_step2', 'Tap the link that appears. AmberSight opens already signed in as <b>{username}</b>.', { username: esc(u.username) }) + '</li>' +
      '<li>' + t('admin_system.qr_step3', 'Add it to the home screen when asked, so it opens like an app.') + '</li></ol>' +
      (origins.length > 1 ? '<label>' + t('admin_system.qr_address', 'Address the phone will use') + '<select id="pqOrigin">' + origins.map(o => '<option>' + esc(o) + '</option>').join('') + '</select></label>' : '') +
      '<div class="hint" id="pqLeft"></div>' +
      '<div class="hint">' + t('admin_system.qr_hint', 'The code works once, only for this account, and expires after 10 minutes. No password is in it.') +
      '<span id="pqCert"> ' + t('admin_system.qr_cert_warning', 'If the phone warns that the connection is not private, install the certificate first (account menu, Trust this server) or continue past the warning.') + '</span></div>' +
      '<div class="asys-err" id="pqErr"></div>' +
      '<div class="row"><button class="btn" id="pqNew">' + t('admin_system.new_code', 'New code') + '</button><button class="primary" id="pqDone">' + t('admin_system.done', 'Done') + '</button></div></div></div>');
    const img = d.querySelector('#pqImg'), left = d.querySelector('#pqLeft'), err = d.querySelector('#pqErr');
    let until = 0, timer = null;
    const tickLeft = () => {
      if (!d.isConnected) { clearInterval(timer); return; }
      const s = Math.max(0, Math.round((until - Date.now()) / 1000));
      left.textContent = s ? t('admin_system.expires_in', 'Expires in {minutes}:{seconds}', { minutes: Math.floor(s / 60), seconds: String(s % 60).padStart(2, '0') }) : t('admin_system.expired', 'This code has expired. Make a new one.');
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
        if (u.admin || roleOf(u) === 'operator' || u.two_step) {
          (await import('./account.js')).confirmWithPassword({ reason: t('admin_system.qr_reason', 'This QR code signs in as {username}{role, select, admin {, an admin,} operator {, an operator,} other {}} with no password or code, so it takes your own password.', { username: u.username, role: u.admin ? 'admin' : roleOf(u) === 'operator' ? 'operator' : 'viewer' }), ok: t('admin_system.make_code', 'Make code'), call: get });
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
      ? { title: t('admin_system.disable_title', 'Disable {username}?', { username: u.username }), body: t('admin_system.disable_body', '{username} is signed out everywhere within seconds, any video they are watching stops, and they cannot sign in or get notifications. Their device tokens are deleted; nothing else is, and only an admin can enable it. After that, each device has to sign in again. Alert services shared by everyone (email, ntfy and so on), linked servers and camera passwords they knew are separate: change those too if needed.', { username: u.username }), ok: t('admin_system.disable_ok', 'Disable {username}', { username: u.username }), danger: true }
      : { title: t('admin_system.enable_title', 'Enable {username}?', { username: u.username }), body: t('admin_system.enable_body', '{username} can sign in again with their password. Devices that were signed in must sign in again.', { username: u.username }), ok: t('admin_system.enable_ok', 'Enable {username}', { username: u.username }) });
    if (!yes) return;
    try { await api('/api/users', { json: { username: u.username, disabled: off } }); toast(off ? t('admin_system.disabled', '{username} is disabled', { username: u.username }) : t('admin_system.enabled', '{username} is enabled', { username: u.username })); load(); }
    catch (e) { toast(errText(e), { kind: 'err' }); }
  }

  function del(u) {
    confirmDialog({ title: t('admin_system.delete_title', 'Delete user {username}?', { username: u.username }), body: t('admin_system.delete_body', '{username} is signed out on every device and cannot sign in again. Their phone app and device tokens stop working.', { username: u.username }), ok: t('admin_system.delete_ok', 'Delete {username}', { username: u.username }), danger: true }).then(async (yes) => {
      if (!yes) return;
      try { await api('/api/users/' + enc(u.username), { method: 'DELETE' }); toast(t('admin_system.user_deleted', 'User deleted')); load(); }
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
    '<select id="lgLevel" title="' + t('admin_system.min_level', 'Minimum level') + '"><option value="error">' + t('admin_system.lvl_errors', 'Errors') + '</option><option value="warn">' + t('admin_system.lvl_warnings', 'Warnings+') + '</option><option value="info" selected>' + t('admin_system.lvl_info', 'Info+') + '</option></select>' +
    '<input id="lgText" type="search" placeholder="' + t('admin_system.filter_placeholder', 'Filter text...') + '" autocomplete="off">' +
    '<select id="lgLimit" title="' + t('admin_system.lines_to_load', 'Lines to load') + '"><option>300</option><option>1000</option><option>2000</option></select>' +
    '<label class="asys-chk"><input type="checkbox" id="lgFollow" checked> ' + t('admin_system.follow', 'Follow') + '</label>' +
    '<span class="grow"></span>' +
    '<button class="btn" id="lgCopy">' + t('admin_system.copy', 'Copy') + '</button><button class="btn" id="lgDl">' + t('admin_system.download', 'Download') + '</button></div>' +
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
    $('#lgCount').textContent = t('admin_system.lines_count', '{shown} of {total} lines', { shown: v.length, total: lines.length });
    out.innerHTML = v.length ? v.map(l => '<div class="lv-' + esc(String(l.level).toLowerCase()) + '"><span class="t">' + esc(clock(l)) + '</span> <span class="l">' + esc(l.level) + '</span> <span class="g">' + esc(l.target) + '</span> ' + esc(l.message) + '</div>').join('') : '<div class="muted">' + t('admin_system.no_log_lines', 'No log lines.') + '</div>';
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
    } catch (e) { out.textContent = t('admin_system.logs_failed', 'Could not load logs: {error}', { error: errText(e) }); }
    busy = false;
  }
  $('#lgLevel').onchange = $('#lgLimit').onchange = () => { sig = ''; tick(); };
  $('#lgText').oninput = draw;
  $('#lgFollow').onchange = () => { if ($('#lgFollow').checked) out.scrollTop = out.scrollHeight; };
  const text = () => vis().map(fmtLine).join('\n') + '\n';
  $('#lgCopy').onclick = async () => {
    try { await navigator.clipboard.writeText(text()); toast(t('admin_system.copied_lines', 'Copied {n, plural, one {# line} other {# lines}}', { n: vis().length })); }
    catch (e) {
      const r = document.createRange(); r.selectNodeContents(out); const sel = getSelection(); sel.removeAllRanges(); sel.addRange(r);
      toast(t('admin_system.selected_copy', 'Selected: press Ctrl+C to copy'));
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
    '<select id="auUser" title="' + t('admin_system.th_user', 'User') + '"><option value="">' + t('admin_system.all_users', 'All users') + '</option></select>' +
    '<select id="auKind" title="' + t('admin_system.what', 'What') + '"><option value="">' + t('admin_system.k_everything', 'Everything') + '</option><option value="sign">' + t('admin_system.k_signins', 'Sign-ins') + '</option><option value="chg">' + t('admin_system.k_changes', 'Changes') + '</option><option value="fail">' + t('admin_system.k_failures', 'Failures') + '</option></select>' +
    '<input id="auText" type="search" placeholder="' + t('admin_system.filter_placeholder', 'Filter text...') + '" autocomplete="off">' +
    '<select id="auLimit" title="' + t('admin_system.entries_to_load', 'Entries to load') + '"><option>200</option><option>1000</option><option>5000</option></select>' +
    '<span class="grow"></span><button class="btn" id="auNow">' + ic('swap') + ' ' + t('admin_system.refresh', 'Refresh') + '</button></div>' +
    '<div id="auBody"><p class="empty">' + t('admin_system.loading', 'Loading...') + '</p></div><div class="asys-pager" id="auPager"></div></div>';
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
    $('#auCount').textContent = t('admin_system.entries_count', '{shown} of {total} entries', { shown: f.length, total: rows.length });
    const slice = f.slice(page * PER, page * PER + PER);
    $('#auBody').innerHTML = slice.length ? wrap('<table class="asys-tbl"><thead><tr><th>' + t('admin_system.th_when', 'When') + '</th><th>' + t('admin_system.th_user', 'User') + '</th><th>' + t('admin_system.what', 'What') + '</th><th>' + t('admin_system.th_result', 'Result') + '</th><th>' + t('admin_system.th_from', 'From') + '</th></tr></thead><tbody>' +
      slice.map(r => '<tr><td class="nowrap">' + esc(fmtTime(r.ts)) + '</td><td>' + esc(r.user || '-') + '</td><td>' + what(r) + '</td>' +
        '<td><span class="badge ' + (r.status >= 400 ? 'off' : 'on') + '">' + r.status + '</span></td><td class="muted">' + esc(r.ip) + '</td></tr>').join('') + '</tbody></table>') : '<p class="empty">' + t('admin_system.no_entries', 'No matching entries.') + '</p>';
    $('#auPager').innerHTML = pages > 1 ? '<button class="btn" id="auPrev"' + (page ? '' : ' disabled') + '>' + ic('left') + ' ' + t('admin_system.newer', 'Newer') + '</button><span class="muted">' + t('admin_system.page_of', 'Page {page} of {pages}', { page: page + 1, pages }) + '</span><button class="btn" id="auNext"' + (page < pages - 1 ? '' : ' disabled') + '>' + t('admin_system.older', 'Older') + ' ' + ic('right') + '</button>' : '';
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
      sel.innerHTML = '<option value="">' + t('admin_system.all_users','All users') + '</option>' + users.map(u => '<option' + (u === cur ? ' selected' : '') + ' value="' + esc(u) + '">' + esc(u) + '</option>').join('');
      draw();
    } catch (e) { $('#auBody').innerHTML = '<p class="empty">' + t('admin_system.could_not_load_audit', 'Could not load the audit log: {error}', { error: esc(errText(e)) }) + '</p>'; }
  }
  const re = () => { page = 0; draw(); };
  $('#auUser').onchange = $('#auKind').onchange = re;
  $('#auText').oninput = re;
  $('#auLimit').onchange = load;
  $('#auNow').onclick = load;
  load();
}

registerAdminPage({ id: 'users', title: t('admin_system.title_users','Users'), order: 10, render: usersPage });
registerAdminPage({ id: 'logs', title: t('admin_system.title_logs','Logs'), order: 30, render: logsPage, leave: logsLeave });
registerAdminPage({ id: 'audit', title: t('admin_system.title_audit','Audit'), order: 40, render: auditPage });
