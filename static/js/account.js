// account.js - "Your account" (user menu): two-step codes and device tokens.
// Loaded the first time it is opened. Secrets are shown once and kept nowhere here.

import { t } from './i18n.js';
import { api, esc, enc, toast, openDlg, confirmDialog, errText, ApiError, state, bus, topDlgOpen } from './core.js';

const when = (s) => s ? new Date(s * 1000).toLocaleDateString([], { year: 'numeric', month: 'short', day: 'numeric' }) : t('account.never', 'never');

// "5 min ago", "3 h ago", "2 days ago"; an older day by its date.
function ago(s) {
  if (!s) return t('account.unknown', 'unknown');
  const d = Math.max(0, Date.now() / 1000 - s);
  if (d < 90) return t('account.just_now', 'just now');
  if (d < 3600) return t('account.min_ago', '{n} min ago', { n: Math.floor(d / 60) });
  if (d < 86400) return t('account.h_ago', '{n} h ago', { n: Math.floor(d / 3600) });
  if (d < 14 * 86400) { const n = Math.floor(d / 86400); return t('account.day_ago', '{n, plural, one {# day ago} other {# days ago}}', { n }); }
  return when(s);
}

// The signed-in devices: this one first, then the most recently used.
function deviceRow(x) {
  const title = x.signed_in ? (x.label || t('account.unknown_browser', 'Unknown browser')) : t('account.signed_before_list', 'Signed in before this list existed');
  const bits = [];
  if (x.ip) bits.push('<span class="acct-ip">' + esc(x.ip) + '</span>');
  if (x.signed_in) bits.push(t('account.signed_in', 'signed in {time}', { time: esc(new Date(x.signed_in * 1000).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' })) }));
  bits.push(t('account.active', 'active {time}', { time: esc(ago(x.last_seen)) }));
  return '<li><div class="acct-tname"><b>' + esc(title) + '</b> ' + (x.this ? '<span class="badge on">' + t('account.this_device', 'This device') + '</span>' : '') +
    '<div class="hint">' + bits.join(' &middot; ') + '</div></div>' +
    '<button class="btn" data-so="' + esc(x.id) + '" data-this="' + (x.this ? '1' : '') + '" aria-label="' + t('account.sign_out_title', 'Sign out {title}', { title: esc(title) }) + '">' + t('account.sign_out', 'Sign out') + '</button></li>';
}

async function signOutThis() {
  const { logout } = await import('./auth.js');
  await logout();
}

// What a person typed into "code": six digits are a code, anything else a recovery code.
export function codeFields(typed) {
  const v = (typed || '').trim();
  if (!v) return {};
  return /^[\d\s-]+$/.test(v) ? { code: v } : { recovery_code: v };
}

const reason = (e) => (e instanceof ApiError && e.body) ? e.body.charAt(0).toUpperCase() + e.body.slice(1) + '.' : errText(e);

async function copyText(text, btn) {
  let ok = false;
  try { await navigator.clipboard.writeText(text); ok = true; } catch (e) {
    // (Not a secure page: the old way.)
    const ta = document.createElement('textarea');
    ta.value = text; document.body.appendChild(ta); ta.select();
    try { ok = document.execCommand('copy'); } catch (y) { /* no clipboard */ }
    ta.remove();
  }
  if (btn) { const old = btn.textContent; btn.textContent = ok ? t('account.copied', 'Copied') : t('account.select_copy', 'Select and copy it by hand'); setTimeout(() => { btn.textContent = old; }, 1400); }
}

// Ask for the password (and the code, with two-step on), then run `call(fields)`.
// Stays open with the reason when it is refused.
function proofDlg({ title, intro = '', ok, danger = false, needCode, extra = '', call }) {
  const d = openDlg(title,
    (intro ? '<p class="hint">' + intro + '</p>' : '') + extra +
    '<label>' + t('account.your_password', 'Your password') + ' <input id="pfPw" type="password" autocomplete="current-password"></label>' +
    (needCode ? '<label>' + t('account.code_from_app', 'Code from your authenticator app') + '<input id="pfCode" autocomplete="one-time-code" autocapitalize="none" spellcheck="false"></label><div class="hint">' + t('account.or_recovery', 'You can also enter a recovery code.') + '</div>' : '') +
    '<div class="asys-err" id="pfErr" role="alert"></div>' +
    '<div class="row"><button class="' + (danger ? 'btn danger' : 'primary') + '" id="pfGo">' + esc(ok) + '</button><button class="btn" id="pfNo">' + t('account.cancel', 'Cancel') + '</button></div>', true);
  const err = d.querySelector('#pfErr'), go = d.querySelector('#pfGo');
  d.querySelector('#pfNo').onclick = () => d.__close();
  const run = async () => {
    err.textContent = '';
    const password = d.querySelector('#pfPw').value;
    if (!password) { err.textContent = t('account.enter_password', 'Enter your password.'); d.querySelector('#pfPw').focus(); return; }
    const box = d.querySelector('#pfCode');
    if (box && !box.value.trim()) { err.textContent = t('account.enter_code', 'Enter the code from your app.'); box.focus(); return; }
    go.disabled = true;
    try { await call(Object.assign({ password }, box ? codeFields(box.value) : {}), d); }
    catch (e) { err.textContent = reason(e); if (box) { box.value = ''; box.focus(); } }
    finally { go.disabled = false; }
  };
  go.onclick = run;
  d.addEventListener('keydown', (ev) => { if (ev.key === 'Enter' && ev.target.tagName === 'INPUT') { ev.preventDefault(); run(); } });
  setTimeout(() => d.querySelector('#pfPw').focus(), 0);
  return d;
}

// Recovery codes: shown once, then only their number is known here.
function showRecovery(codes, done) {
  const text = codes.join('\n');
  const d = openDlg(t('account.save_recovery_codes', 'Save your recovery codes'),
    '<p class="hint">' + t('account.recovery_hint', 'If you lose your phone, each of these signs you in once instead of a code. <b>This is the only time they are shown.</b> Keep them somewhere safe that is not your phone, such as a password manager or on paper.') + '</p>' +
    '<div class="acct-codes" data-recovery>' + codes.map(c => '<code>' + esc(c) + '</code>').join('') + '</div>' +
    '<div class="row"><button class="btn" id="rcCopy">' + t('account.copy', 'Copy') + '</button><button class="btn" id="rcSave">' + t('account.save_file', 'Save as a file') + '</button></div>' +
    '<div class="row"><button class="primary" id="rcDone">' + t('account.saved_them', 'I have saved them') + '</button></div>', true);
  d.querySelector('#rcCopy').onclick = (ev) => copyText(text, ev.currentTarget);
  d.querySelector('#rcSave').onclick = () => {
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([t('account.codes_file', 'AmberSight recovery codes for {user}\nEach works once.', { user: state.user }) + '\n\n' + text + '\n'], { type: 'text/plain' }));
    a.download = 'ambersight-recovery-codes.txt';
    document.body.appendChild(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 2000);
  };
  d.querySelector('#rcDone').onclick = () => { d.__close(); if (done) done(); };
}

// Turning it on: password, then the QR code and a first code, then the recovery codes.
function turnOn(refresh) {
  proofDlg({
    title: t('account.turn_on_two_step', 'Turn on two-step codes'),
    intro: t('account.turn_on_intro', 'After this, signing in takes your password and a 6-digit code from an app on your phone (Google Authenticator, Microsoft Authenticator, 1Password, Aegis and others). First, your password.'),
    ok: t('account.continue', 'Continue'), needCode: false,
    call: async (f, pd) => {
      const r = await api('/api/me/two-step/start', { json: { password: f.password } });
      pd.__close();
      scanDlg(r, refresh);
    },
  });
}

function scanDlg(r, refresh) {
  let finished = false;
  const d = openDlg(t('account.scan_with_app', 'Scan this with your app'),
    '<div class="acct-qr">' + r.svg + '</div>' +
    '<details class="acct-secret"><summary>' + t('account.cant_scan', "Can't scan it?") + '</summary><p class="hint">' + t('account.add_by_hand', 'Add an account by hand in the app and type this key (time-based):') + '</p><code id="tsSecret">' + esc(r.secret) + '</code> <button class="btn" id="tsCopy" type="button">' + t('account.copy', 'Copy') + '</button></details>' +
    '<label>' + t('account.six_digit', 'The 6-digit code the app shows now') + '<input id="tsCode" inputmode="numeric" autocomplete="one-time-code" maxlength="7" pattern="[0-9 ]*"></label>' +
    '<div class="asys-err" id="tsErr" role="alert"></div>' +
    '<div class="row"><button class="primary" id="tsGo">' + t('account.turn_on', 'Turn on') + '</button><button class="btn" id="tsNo">' + t('account.cancel', 'Cancel') + '</button></div>', true);
  const box = d.querySelector('#tsCode'), err = d.querySelector('#tsErr'), go = d.querySelector('#tsGo');
  d.querySelector('#tsCopy').onclick = (ev) => copyText(r.secret, ev.currentTarget);
  // Left without a code (Cancel, X, Esc): the server forgets this secret.
  const shut = d.__close;
  d.__close = () => {
    shut();
    if (!finished) { finished = true; api('/api/me/two-step/cancel', { json: { setup_id: r.setup_id } }).catch(() => { /* it expires by itself */ }); }
  };
  d.querySelector('#tsNo').onclick = () => d.__close();
  const run = async () => {
    err.textContent = '';
    if (!box.value.trim()) { err.textContent = t('account.enter_code', 'Enter the code from your app.'); box.focus(); return; }
    go.disabled = true;
    try {
      const a = await api('/api/me/two-step/confirm', { json: { code: box.value } });
      finished = true;
      d.__close();
      bus.emit('two-step', true);   // (lists showing it, such as Users, redraw)
      if (refresh) refresh();
      showRecovery(a.recovery_codes, refresh);
      toast(a.warning || t('account.two_step_on_warning','Two-step codes are on. Other devices were signed out.'));
    } catch (e) { err.textContent = reason(e); box.value = ''; box.focus(); }
    finally { go.disabled = false; }
  };
  go.onclick = run;
  box.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') { ev.preventDefault(); run(); } });
  setTimeout(() => box.focus(), 0);
}

function turnOff(refresh) {
  proofDlg({
    title: t('account.turn_off_two_step_codes','Turn off two-step codes'),
    intro: t('account.turn_off_intro','Signing in will take only your password again, and your recovery codes stop working. Other devices are signed out.'),
    ok: t('account.turn_off','Turn off'), danger: true, needCode: true,
    call: async (f, pd) => {
      await api('/api/me/two-step/off', { json: f });
      pd.__close();
      bus.emit('two-step', false);
      toast(t('account.two_step_off','Two-step codes are off'));
      refresh();
    },
  });
}

function newRecovery(refresh) {
  proofDlg({
    title: t('account.new_recovery_codes','New recovery codes'),
    intro: t('account.new_recovery_intro','You get 10 new codes. The old ones stop working.'),
    ok: t('account.make_new_codes','Make new codes'), needCode: true,
    call: async (f, pd) => {
      const r = await api('/api/me/two-step/recovery', { json: f });
      pd.__close();
      showRecovery(r.recovery_codes, refresh);
    },
  });
}

function addToken(acct, refresh) {
  const scope = acct.admin || acct.role === 'operator'
    ? '<div class="sec">' + t('account.what_may_do', 'What it may do') + '</div>' +
      '<label class="asys-chk"><input type="radio" name="tkScope" value="viewer" checked> ' + t('account.scope_watch', 'Watch only (recommended): live video, events, clips and recordings') + '</label>' +
      '<label class="asys-chk"><input type="radio" name="tkScope" value="admin"> ' + t('account.scope_control', 'Also day-to-day control: arm and disarm, profiles and pausing notifications (for a day at most), moving cameras, recording a clip, reviewing events{op, select, yes { (as far as your account may)} other {}}. Admins are told when it disarms or pauses. Not for RTSP', { op: acct.role === 'operator' ? 'yes' : 'no' }) + '</label>'
    : '';
  proofDlg({
    title: t('account.new_device_token','New device token'),
    intro: t('account.new_token_intro','For a program that only needs to watch or do everyday things, such as Home Assistant or an RTSP player. It works until you revoke it. It can never change settings, passwords or accounts, and cannot get notifications or report presence: an app that needs those signs in with a password.'),
    extra: '<label>' + t('account.name', 'Name') + '<input id="tkName" maxlength="64" autocomplete="off" placeholder="' + t('account.name_example', 'Pixel app') + '"></label>' + scope,
    ok: t('account.make_token','Make token'), needCode: acct.two_step.on,
    call: async (f, pd) => {
      const name = pd.querySelector('#tkName').value.trim();
      if (!name) { pd.querySelector('#tkName').focus(); throw new Error(t('account.give_name','Give it a name, like the app or device that will use it.')); }
      const picked = pd.querySelector('input[name=tkScope]:checked');
      const r = await api('/api/me/tokens', { json: Object.assign({ name, scope: picked ? picked.value : 'viewer' }, f) });
      pd.__close();
      showToken(r.token, r.info, refresh);
    },
  });
}

function showToken(token, info, refresh) {
  const d = openDlg(t('account.your_new_token','Your new token'),
    '<p class="hint">' + t('account.token_copy_now', '<b>Copy it now: it is not shown again.</b> Anyone who has it can {scope, select, admin {see your cameras, arm and disarm, and move them} other {see your cameras}}, so treat it like a password.', { scope: info.scope === 'admin' ? 'admin' : 'viewer' }) + '</p>' +
    '<code class="acct-token" id="tkValue">' + esc(token) + '</code>' +
    '<p class="hint">' + t('account.token_use', 'In a program: use it as a bearer token.') + ' ' + (info.scope === 'admin' ? t('account.token_no_rtsp', 'It does not work for RTSP, which is not encrypted: make a watch-only token for that.') : t('account.token_rtsp', 'For RTSP and older programs: your username, with this as the password. RTSP is not encrypted, so use it only on a network you trust.')) + '</p>' +
    '<div class="row"><button class="btn" id="tkCopy">' + t('account.copy', 'Copy') + '</button><button class="primary" id="tkDone">' + t('account.done', 'Done') + '</button></div>', true);
  d.querySelector('#tkCopy').onclick = (ev) => copyText(token, ev.currentTarget);
  d.querySelector('#tkDone').onclick = () => { d.__close(); refresh(); };
}

// A change that hands out admin rights or someone else's way in: asks only for what is
// needed (your password, and your code if you use them). `call(proof)` makes the request
// with these fields; if it is refused, the reason shows here and the dialog stays.
export async function confirmWithPassword({ reason, ok, danger = false, call, onCancel }) {
  let on = true;
  try { on = (await api('/api/me/account')).two_step.on; } catch (e) { /* ask for it: the server decides */ }
  let done = false;
  const d = proofDlg({
    title: t('account.confirm_it_is_you','Confirm it is you'), intro: esc(reason), ok, danger, needCode: on,
    call: async (f, dlg) => {
      await call({ my_password: f.password, my_code: f.code, my_recovery_code: f.recovery_code });
      done = true;
      dlg.__close();
    },
  });
  const shut = d.__close;
  d.__close = () => { shut(); if (!done && onCancel) { done = true; onCancel(); } };
}

// Turning on two-step codes from elsewhere (Edit user, the offer after
// signing in): the same steps as in Your account.
export function startTwoStep(done) { turnOn(done); }

// Once per account on this browser, after signing in with a password: offer
// two-step codes to an account without them. "Not now" is not asked again here
// (Your account, in the menu at the top, still has it).
export async function offerTwoStep() {
  // Not in a split-view pane or a pop-out camera window, and not over an
  // alert a notification link is opening.
  const q = new URLSearchParams(location.search);
  if (q.get('pane') || q.has('cam') || q.has('alert')) return;
  const user = state.user, key = 'ambersight.twoStepOffered.' + user;
  try { if (localStorage.getItem(key)) return; } catch (e) { return; }
  // After the page has settled; another dialog already open wins (asked next sign-in).
  await new Promise(r => setTimeout(r, 2500));
  if (topDlgOpen() || state.user !== user) return;
  let a;
  try { a = await api('/api/me/account'); } catch (e) { return; }
  if (!a.two_step || a.two_step.on || topDlgOpen() || state.user !== user) return;
  const yes = await confirmDialog({
    title: t('account.turn_on_two_step_codes','Turn on two-step codes?'),
    body: t('account.offer_body', 'After that, signing in also takes a 6-digit code from an app on your phone, so a stolen password alone will not get anyone in.{admin, select, yes { Recommended for admin accounts.} other {}} It takes about a minute: your password once more, then scan a code with the app. You can also do it later in Your account, in the menu at the top.', { admin: a.admin ? 'yes' : 'no' }),
    ok: t('account.turn_on','Turn on'), cancel: t('account.not_now','Not now'), safeFocus: true,   // (a stray Enter is a Not now)
  });
  try { localStorage.setItem(key, '1'); } catch (e) { /* asked again next time */ }
  if (yes) turnOn(null);
}

let open = null;
export async function openAccount() {
  let d = null;
  async function draw() {
    let a;
    try { a = await api('/api/me/account'); }
    catch (e) { toast(errText(e), { kind: 'err' }); return; }
    const on = a.two_step.on, left = a.two_step.recovery_left;
    const body =
      '<section class="acct-sec"><div class="acct-head"><b>' + t('account.two_step_codes', 'Two-step codes') + '</b> ' + (on ? '<span class="badge on">' + t('account.on', 'On') + '</span>' : '<span class="badge">' + t('account.off', 'Off') + '</span>') + '</div>' +
      (on
        ? (a.two_step.codes_locked ? '<p class="asys-err">' + t('account.codes_locked', 'Too many wrong codes were tried for this account, so someone has your password: change it now (Change password in the menu). Your code still works here.') + '</p>' : '') + '<p class="hint">' + t('account.signin_takes', 'Signing in takes your password and a code from your app.') + ' ' + t('account.recovery_left', 'You have <b>{n}</b> {n, plural, one {recovery code} other {recovery codes}} left.', { n: left }) + (left <= 2 ? ' ' + t('account.make_new_soon', 'Make new ones soon.') : '') + '</p>' +
          '<div class="row"><button class="btn" data-recovery-new>' + t('account.new_recovery_codes', 'New recovery codes') + '</button><button class="btn" data-off>' + t('account.turn_off', 'Turn off') + '</button></div>'
        : '<p class="hint">' + t('account.stolen_password', 'A stolen password alone will not get anyone in.') + (a.admin ? ' ' + t('account.admin_recommend', 'Recommended for admin accounts: this one can change everything.') : '') + '</p>' +
          '<div class="row"><button class="primary" data-on>' + t('account.turn_on', 'Turn on') + '</button></div>') +
      '</section>' +
      '<section class="acct-sec" data-devices><div class="acct-head"><b>' + t('account.signed_in_devices', 'Signed-in devices') + '</b></div>' +
      '<p class="hint">' + t('account.devices_hint', 'Where this account is signed in now. Sign out any you do not recognize, and change your password. Video open on your other devices stops for a moment and reconnects. Notifications turned on before this list existed cannot be matched to a device: remove older devices under Notifications as well.') + '</p>' +
      (a.sessions && a.sessions.length
        ? '<ul class="acct-tokens acct-devs">' + a.sessions.map(deviceRow).join('') + '</ul>'
        : '<p class="muted">' + t('account.only_this_device', 'Only this device.') + '</p>') +
      (a.sessions && a.sessions.some(x => !x.this) ? '<div class="row"><button class="btn" data-so-others>' + t('account.sign_out_others', 'Sign out all other devices') + '</button></div>' : '') +
      '</section>' +
      '<section class="acct-sec"><div class="acct-head"><b>' + t('account.device_tokens', 'Device tokens') + '</b></div>' +
      '<p class="hint">' + t('account.token_hint', 'For programs that watch or do everyday things. Each works until you revoke it, also after you change your password.') + (on ? ' ' + t('account.token_hint_2fa', 'With two-step codes on, your password no longer works in them, so they need a token.') : '') + '</p>' +
      (a.tokens.length
        ? '<ul class="acct-tokens">' + a.tokens.map(tk =>
            '<li><div class="acct-tname"><b>' + esc(tk.name) + '</b> ' + (tk.scope === 'admin' ? '<span class="badge on">' + t('account.can_control', 'Can control') + '</span>' : '<span class="badge">' + t('account.watch_only', 'Watch only') + '</span>') +
            '<div class="hint">' + t('account.token_dates', 'Made {made}, last used {used}', { made: esc(when(tk.created)), used: esc(when(tk.last_used)) }) + '</div></div>' +
            '<button class="btn" data-rv="' + esc(tk.id) + '" data-name="' + esc(tk.name) + '">' + t('account.revoke', 'Revoke') + '</button></li>').join('') + '</ul>'
        : '<p class="muted">' + t('account.no_tokens', 'No tokens yet.') + '</p>') +
      '<div class="row"><button class="btn" data-add' + (a.tokens.length >= a.token_limit ? ' disabled title="' + t('account.revoke_one_first', 'Revoke one first') + '"' : '') + '>' + t('account.new_token', 'New token') + '</button></div></section>';
    if (d && d.isConnected) d.querySelector('.body').innerHTML = body;
    else {
      d = open = openDlg(t('account.title', 'Your account'), body, true);
    }
    const q = (s) => d.querySelector(s);
    if (q('[data-on]')) q('[data-on]').onclick = () => turnOn(draw);
    if (q('[data-off]')) q('[data-off]').onclick = () => turnOff(draw);
    if (q('[data-recovery-new]')) q('[data-recovery-new]').onclick = () => newRecovery(draw);
    q('[data-add]').onclick = () => addToken(a, draw);
    d.querySelectorAll('[data-so]').forEach(b => {
      b.onclick = async () => {
        const mine = !!b.dataset.this;
        if (mine && !(await confirmDialog({ title: t('account.sign_out_this_title', 'Sign out of this device?'), body: t('account.sign_out_this_body', 'You will need to sign in again here.'), ok: t('account.sign_out', 'Sign out'), danger: true }))) return;
        b.disabled = true;
        try {
          if (mine) { d.__close(); await signOutThis(); return; }
          await api('/api/me/sessions/' + enc(b.dataset.so) + '/sign-out', { json: {} });
          toast(t('account.signed_out', 'Signed out'));
        } catch (e) { toast(errText(e), { kind: 'err' }); }
        draw();
      };
    });
    if (q('[data-so-others]')) q('[data-so-others]').onclick = async (ev) => {
      const btn = ev.currentTarget; // (null after an await)
      const yes = await confirmDialog({ title: t('account.sign_out_others_title', 'Sign out all other devices?'), body: t('account.sign_out_others_body', 'Every other browser and phone signed in to this account is signed out now, and any video they have open ends. This device stays signed in.'), ok: t('account.sign_out_others_ok', 'Sign out others'), danger: true });
      if (!yes) return;
      btn.disabled = true;
      try { const r = await api('/api/me/sessions/sign-out-others', { json: {} }); toast(t('account.devices_signed_out', '{n, plural, one {1 device signed out} other {# devices signed out}}', { n: r.ended })); }
      catch (e) { toast(errText(e), { kind: 'err' }); }
      draw();
    };
    d.querySelectorAll('[data-rv]').forEach(b => {
      b.onclick = async () => {
        const yes = await confirmDialog({ title: t('account.revoke_title', 'Revoke {name}?', { name: b.dataset.name }), body: t('account.revoke_body', 'It stops working now, and any video it has open ends within seconds. Other video on this account may reconnect.'), ok: t('account.revoke', 'Revoke'), danger: true });
        if (!yes) return;
        try { await api('/api/me/tokens/' + enc(b.dataset.rv), { method: 'DELETE' }); toast(t('account.token_revoked', 'Token revoked')); draw(); }
        catch (e) { toast(errText(e), { kind: 'err' }); }
      };
    });
  }
  await draw();
  return open;
}
