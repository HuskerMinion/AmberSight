// account.js - "Your account" (user menu): two-step codes and device tokens.
// Loaded the first time it is opened. Secrets are shown once and kept nowhere here.

import { api, esc, enc, toast, openDlg, confirmDialog, errText, ApiError, state, bus, topDlgOpen } from './core.js';

const when = (s) => s ? new Date(s * 1000).toLocaleDateString([], { year: 'numeric', month: 'short', day: 'numeric' }) : 'never';

// "5 min ago", "3 h ago", "2 days ago"; an older day by its date.
function ago(s) {
  if (!s) return 'unknown';
  const d = Math.max(0, Date.now() / 1000 - s);
  if (d < 90) return 'just now';
  if (d < 3600) return Math.floor(d / 60) + ' min ago';
  if (d < 86400) return Math.floor(d / 3600) + ' h ago';
  if (d < 14 * 86400) { const n = Math.floor(d / 86400); return n + (n === 1 ? ' day ago' : ' days ago'); }
  return when(s);
}

// The signed-in devices: this one first, then the most recently used.
function deviceRow(x) {
  const title = x.signed_in ? (x.label || 'Unknown browser') : 'Signed in before this list existed';
  const bits = [];
  if (x.ip) bits.push('<span class="acct-ip">' + esc(x.ip) + '</span>');
  if (x.signed_in) bits.push('signed in ' + esc(new Date(x.signed_in * 1000).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' })));
  bits.push('active ' + esc(ago(x.last_seen)));
  return '<li><div class="acct-tname"><b>' + esc(title) + '</b> ' + (x.this ? '<span class="badge on">This device</span>' : '') +
    '<div class="hint">' + bits.join(' &middot; ') + '</div></div>' +
    '<button class="btn" data-so="' + esc(x.id) + '" data-this="' + (x.this ? '1' : '') + '" aria-label="Sign out ' + esc(title) + '">Sign out</button></li>';
}

async function signOutThis() {
  const { logout } = await import('./auth.js');
  await logout();
}

// What a person typed into "code": six digits are a code, anything else a recovery code.
export function codeFields(typed) {
  const t = (typed || '').trim();
  if (!t) return {};
  return /^[\d\s-]+$/.test(t) ? { code: t } : { recovery_code: t };
}

const reason = (e) => (e instanceof ApiError && e.body) ? e.body.charAt(0).toUpperCase() + e.body.slice(1) + '.' : errText(e);

async function copyText(text, btn) {
  let ok = false;
  try { await navigator.clipboard.writeText(text); ok = true; } catch (e) {
    // (Not a secure page: the old way.)
    const t = document.createElement('textarea');
    t.value = text; document.body.appendChild(t); t.select();
    try { ok = document.execCommand('copy'); } catch (y) { /* no clipboard */ }
    t.remove();
  }
  if (btn) { const old = btn.textContent; btn.textContent = ok ? 'Copied' : 'Select and copy it by hand'; setTimeout(() => { btn.textContent = old; }, 1400); }
}

// Ask for the password (and the code, with two-step on), then run `call(fields)`.
// Stays open with the reason when it is refused.
function proofDlg({ title, intro = '', ok, danger = false, needCode, extra = '', call }) {
  const d = openDlg(title,
    (intro ? '<p class="hint">' + intro + '</p>' : '') + extra +
    '<label>Your password <input id="pfPw" type="password" autocomplete="current-password"></label>' +
    (needCode ? '<label>Code from your authenticator app<input id="pfCode" autocomplete="one-time-code" autocapitalize="none" spellcheck="false"></label><div class="hint">You can also enter a recovery code.</div>' : '') +
    '<div class="asys-err" id="pfErr" role="alert"></div>' +
    '<div class="row"><button class="' + (danger ? 'btn danger' : 'primary') + '" id="pfGo">' + esc(ok) + '</button><button class="btn" id="pfNo">Cancel</button></div>', true);
  const err = d.querySelector('#pfErr'), go = d.querySelector('#pfGo');
  d.querySelector('#pfNo').onclick = () => d.__close();
  const run = async () => {
    err.textContent = '';
    const password = d.querySelector('#pfPw').value;
    if (!password) { err.textContent = 'Enter your password.'; d.querySelector('#pfPw').focus(); return; }
    const box = d.querySelector('#pfCode');
    if (box && !box.value.trim()) { err.textContent = 'Enter the code from your app.'; box.focus(); return; }
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
  const d = openDlg('Save your recovery codes',
    '<p class="hint">If you lose your phone, each of these signs you in once instead of a code. <b>This is the only time they are shown.</b> Keep them somewhere safe that is not your phone, such as a password manager or on paper.</p>' +
    '<div class="acct-codes" data-recovery>' + codes.map(c => '<code>' + esc(c) + '</code>').join('') + '</div>' +
    '<div class="row"><button class="btn" id="rcCopy">Copy</button><button class="btn" id="rcSave">Save as a file</button></div>' +
    '<div class="row"><button class="primary" id="rcDone">I have saved them</button></div>', true);
  d.querySelector('#rcCopy').onclick = (ev) => copyText(text, ev.currentTarget);
  d.querySelector('#rcSave').onclick = () => {
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob(['AmberSight recovery codes for ' + state.user + '\nEach works once.\n\n' + text + '\n'], { type: 'text/plain' }));
    a.download = 'ambersight-recovery-codes.txt';
    document.body.appendChild(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 2000);
  };
  d.querySelector('#rcDone').onclick = () => { d.__close(); if (done) done(); };
}

// Turning it on: password, then the QR code and a first code, then the recovery codes.
function turnOn(refresh) {
  proofDlg({
    title: 'Turn on two-step codes',
    intro: 'After this, signing in takes your password and a 6-digit code from an app on your phone (Google Authenticator, Microsoft Authenticator, 1Password, Aegis and others). First, your password.',
    ok: 'Continue', needCode: false,
    call: async (f, pd) => {
      const r = await api('/api/me/two-step/start', { json: { password: f.password } });
      pd.__close();
      scanDlg(r, refresh);
    },
  });
}

function scanDlg(r, refresh) {
  let finished = false;
  const d = openDlg('Scan this with your app',
    '<div class="acct-qr">' + r.svg + '</div>' +
    '<details class="acct-secret"><summary>Can\'t scan it?</summary><p class="hint">Add an account by hand in the app and type this key (time-based):</p><code id="tsSecret">' + esc(r.secret) + '</code> <button class="btn" id="tsCopy" type="button">Copy</button></details>' +
    '<label>The 6-digit code the app shows now<input id="tsCode" inputmode="numeric" autocomplete="one-time-code" maxlength="7" pattern="[0-9 ]*"></label>' +
    '<div class="asys-err" id="tsErr" role="alert"></div>' +
    '<div class="row"><button class="primary" id="tsGo">Turn on</button><button class="btn" id="tsNo">Cancel</button></div>', true);
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
    if (!box.value.trim()) { err.textContent = 'Enter the code from your app.'; box.focus(); return; }
    go.disabled = true;
    try {
      const a = await api('/api/me/two-step/confirm', { json: { code: box.value } });
      finished = true;
      d.__close();
      bus.emit('two-step', true);   // (lists showing it, such as Users, redraw)
      if (refresh) refresh();
      showRecovery(a.recovery_codes, refresh);
      toast(a.warning || 'Two-step codes are on. Other devices were signed out.');
    } catch (e) { err.textContent = reason(e); box.value = ''; box.focus(); }
    finally { go.disabled = false; }
  };
  go.onclick = run;
  box.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') { ev.preventDefault(); run(); } });
  setTimeout(() => box.focus(), 0);
}

function turnOff(refresh) {
  proofDlg({
    title: 'Turn off two-step codes',
    intro: 'Signing in will take only your password again, and your recovery codes stop working. Other devices are signed out.',
    ok: 'Turn off', danger: true, needCode: true,
    call: async (f, pd) => {
      await api('/api/me/two-step/off', { json: f });
      pd.__close();
      bus.emit('two-step', false);
      toast('Two-step codes are off');
      refresh();
    },
  });
}

function newRecovery(refresh) {
  proofDlg({
    title: 'New recovery codes',
    intro: 'You get 10 new codes. The old ones stop working.',
    ok: 'Make new codes', needCode: true,
    call: async (f, pd) => {
      const r = await api('/api/me/two-step/recovery', { json: f });
      pd.__close();
      showRecovery(r.recovery_codes, refresh);
    },
  });
}

function addToken(acct, refresh) {
  const scope = acct.admin
    ? '<div class="sec">What it may do</div>' +
      '<label class="asys-chk"><input type="radio" name="tkScope" value="viewer" checked> Watch only (recommended): live video, events, clips and recordings</label>' +
      '<label class="asys-chk"><input type="radio" name="tkScope" value="admin"> Also day-to-day control: arm and disarm, profiles and pausing notifications (for a day at most), moving cameras, recording a clip, reviewing events. Admins are told when it disarms or pauses. Not for RTSP</label>'
    : '';
  proofDlg({
    title: 'New device token',
    intro: 'For a program that only needs to watch or do everyday things, such as Home Assistant or an RTSP player. It works until you revoke it. It can never change settings, passwords or accounts, and cannot get notifications or report presence: an app that needs those signs in with a password.',
    extra: '<label>Name<input id="tkName" maxlength="64" autocomplete="off" placeholder="Pixel app"></label>' + scope,
    ok: 'Make token', needCode: acct.two_step.on,
    call: async (f, pd) => {
      const name = pd.querySelector('#tkName').value.trim();
      if (!name) { pd.querySelector('#tkName').focus(); throw new Error('Give it a name, like the app or device that will use it.'); }
      const picked = pd.querySelector('input[name=tkScope]:checked');
      const r = await api('/api/me/tokens', { json: Object.assign({ name, scope: picked ? picked.value : 'viewer' }, f) });
      pd.__close();
      showToken(r.token, r.info, refresh);
    },
  });
}

function showToken(token, info, refresh) {
  const d = openDlg('Your new token',
    '<p class="hint"><b>Copy it now: it is not shown again.</b> Anyone who has it can ' + (info.scope === 'admin' ? 'see your cameras, arm and disarm, and move them' : 'see your cameras') + ', so treat it like a password.</p>' +
    '<code class="acct-token" id="tkValue">' + esc(token) + '</code>' +
    '<p class="hint">In a program: use it as a bearer token. ' + (info.scope === 'admin' ? 'It does not work for RTSP, which is not encrypted: make a watch-only token for that.' : 'For RTSP and older programs: your username, with this as the password. RTSP is not encrypted, so use it only on a network you trust.') + '</p>' +
    '<div class="row"><button class="btn" id="tkCopy">Copy</button><button class="primary" id="tkDone">Done</button></div>', true);
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
    title: 'Confirm it is you', intro: esc(reason), ok, danger, needCode: on,
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
    title: 'Turn on two-step codes?',
    body: 'After that, signing in also takes a 6-digit code from an app on your phone, so a stolen password alone will not get anyone in.' + (a.admin ? ' Recommended for admin accounts.' : '') + ' It takes about a minute: your password once more, then scan a code with the app. You can also do it later in Your account, in the menu at the top.',
    ok: 'Turn on', cancel: 'Not now', safeFocus: true,   // (a stray Enter is a Not now)
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
      '<section class="acct-sec"><div class="acct-head"><b>Two-step codes</b> ' + (on ? '<span class="badge on">On</span>' : '<span class="badge">Off</span>') + '</div>' +
      (on
        ? (a.two_step.codes_locked ? '<p class="asys-err">Too many wrong codes were tried for this account, so someone has your password: change it now (Change password in the menu). Your code still works here.</p>' : '') + '<p class="hint">Signing in takes your password and a code from your app. You have <b>' + left + '</b> recovery code' + (left === 1 ? '' : 's') + ' left.' + (left <= 2 ? ' Make new ones soon.' : '') + '</p>' +
          '<div class="row"><button class="btn" data-recovery-new>New recovery codes</button><button class="btn" data-off>Turn off</button></div>'
        : '<p class="hint">A stolen password alone will not get anyone in.' + (a.admin ? ' Recommended for admin accounts: this one can change everything.' : '') + '</p>' +
          '<div class="row"><button class="primary" data-on>Turn on</button></div>') +
      '</section>' +
      '<section class="acct-sec" data-devices><div class="acct-head"><b>Signed-in devices</b></div>' +
      '<p class="hint">Where this account is signed in now. Sign out any you do not recognize, and change your password. Video open on your other devices stops for a moment and reconnects. Notifications turned on before this list existed cannot be matched to a device: remove older devices under Notifications as well.</p>' +
      (a.sessions && a.sessions.length
        ? '<ul class="acct-tokens acct-devs">' + a.sessions.map(deviceRow).join('') + '</ul>'
        : '<p class="muted">Only this device.</p>') +
      (a.sessions && a.sessions.some(x => !x.this) ? '<div class="row"><button class="btn" data-so-others>Sign out all other devices</button></div>' : '') +
      '</section>' +
      '<section class="acct-sec"><div class="acct-head"><b>Device tokens</b></div>' +
      '<p class="hint">For programs that watch or do everyday things. Each works until you revoke it, also after you change your password.' + (on ? ' With two-step codes on, your password no longer works in them, so they need a token.' : '') + '</p>' +
      (a.tokens.length
        ? '<ul class="acct-tokens">' + a.tokens.map(t =>
            '<li><div class="acct-tname"><b>' + esc(t.name) + '</b> ' + (t.scope === 'admin' ? '<span class="badge on">Can control</span>' : '<span class="badge">Watch only</span>') +
            '<div class="hint">Made ' + esc(when(t.created)) + ', last used ' + esc(when(t.last_used)) + '</div></div>' +
            '<button class="btn" data-rv="' + esc(t.id) + '" data-name="' + esc(t.name) + '">Revoke</button></li>').join('') + '</ul>'
        : '<p class="muted">No tokens yet.</p>') +
      '<div class="row"><button class="btn" data-add' + (a.tokens.length >= a.token_limit ? ' disabled title="Revoke one first"' : '') + '>New token</button></div></section>';
    if (d && d.isConnected) d.querySelector('.body').innerHTML = body;
    else {
      d = open = openDlg('Your account', body, true);
    }
    const q = (s) => d.querySelector(s);
    if (q('[data-on]')) q('[data-on]').onclick = () => turnOn(draw);
    if (q('[data-off]')) q('[data-off]').onclick = () => turnOff(draw);
    if (q('[data-recovery-new]')) q('[data-recovery-new]').onclick = () => newRecovery(draw);
    q('[data-add]').onclick = () => addToken(a, draw);
    d.querySelectorAll('[data-so]').forEach(b => {
      b.onclick = async () => {
        const mine = !!b.dataset.this;
        if (mine && !(await confirmDialog({ title: 'Sign out of this device?', body: 'You will need to sign in again here.', ok: 'Sign out', danger: true }))) return;
        b.disabled = true;
        try {
          if (mine) { d.__close(); await signOutThis(); return; }
          await api('/api/me/sessions/' + enc(b.dataset.so) + '/sign-out', { json: {} });
          toast('Signed out');
        } catch (e) { toast(errText(e), { kind: 'err' }); }
        draw();
      };
    });
    if (q('[data-so-others]')) q('[data-so-others]').onclick = async (ev) => {
      const btn = ev.currentTarget; // (null after an await)
      const yes = await confirmDialog({ title: 'Sign out all other devices?', body: 'Every other browser and phone signed in to this account is signed out now, and any video they have open ends. This device stays signed in.', ok: 'Sign out others', danger: true });
      if (!yes) return;
      btn.disabled = true;
      try { const r = await api('/api/me/sessions/sign-out-others', { json: {} }); toast(r.ended === 1 ? '1 device signed out' : r.ended + ' devices signed out'); }
      catch (e) { toast(errText(e), { kind: 'err' }); }
      draw();
    };
    d.querySelectorAll('[data-rv]').forEach(b => {
      b.onclick = async () => {
        const yes = await confirmDialog({ title: 'Revoke ' + b.dataset.name + '?', body: 'It stops working now, and any video it has open ends within seconds. Other video on this account may reconnect.', ok: 'Revoke', danger: true });
        if (!yes) return;
        try { await api('/api/me/tokens/' + enc(b.dataset.rv), { method: 'DELETE' }); toast('Token revoked'); draw(); }
        catch (e) { toast(errText(e), { kind: 'err' }); }
      };
    });
  }
  await draw();
  return open;
}
