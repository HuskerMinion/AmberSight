// auth.js - cookie-session sign-in gate, account menu, change password.
// The server sets an HttpOnly "as_session" cookie on POST /api/login; after that every
// request (fetch, <img>, <video>, download links) is authenticated by the browser itself.

import { keepAwake, setKeepAwake, wakeSupported } from './wake.js';
import { $, state, bus, api, ApiError, openDlg, msgDlg, toast, esc, raiseDlg, dataSaver, dataSaverSetting, setDataSaver, setTheme, themeMode } from './core.js';
import { alertPopups, setAlertPopups } from './events.js';
import { openPushDlg, dropThisDevice } from './push.js';

export async function whoami() {
  const r = await fetch('/api/whoami', { credentials: 'same-origin' });
  if (r.status === 401) return { signed_in: false, user: '', admin: false, auth_required: true };
  if (!r.ok) throw new Error('whoami -> ' + r.status);
  return r.json();
}

// Phone sign-in QR: the code arrives as #pair=<code>. Take it off the address right
// away (so it is not bookmarked, shared or added to the home screen with the page).
let pairCode = null;
export function takePairCode() {
  const m = /^#pair=([0-9a-f]{16,128})$/i.exec(location.hash || '');
  if (!m) return;
  pairCode = m[1];
  history.replaceState(null, '', location.pathname + location.search);
}

// Use the code (once). Returns a message for the sign-in screen when it fails.
export async function redeemPairCode() {
  if (!pairCode) return null;
  const code = pairCode;
  pairCode = null;
  try {
    const r = await fetch('/api/pair', {
      method: 'POST', credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ code }),
    });
    if (r.ok) { state.justPaired = true; return null; }
    if (r.status === 429) return 'Too many failed sign-ins. Wait a few minutes and try again.';
    return 'That sign-in code has expired or was already used. Ask for a new QR code, or sign in with your password.';
  } catch (e) {
    return 'Server unreachable.';
  }
}

// After a QR sign-in on a phone: how to put AmberSight on the home screen.
export function homeScreenHint() {
  if (!state.justPaired) return;
  state.justPaired = false;
  const standalone = matchMedia('(display-mode: standalone)').matches || navigator.standalone;
  if (standalone) return;
  // Once per browser (for this server) is enough: switching servers signs in
  // the same way every time.
  try {
    if (localStorage.getItem('ambs.signedInTipShown')) return;
    localStorage.setItem('ambs.signedInTipShown', '1');
  } catch (e) { /* private mode: show it */ }
  const ios = /iPhone|iPad|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
  const how = ios
    ? 'In Safari, tap the Share button, then <b>Add to Home Screen</b>.'
    : 'Open the browser menu (&#8942;) and choose <b>Add to Home screen</b> or <b>Install app</b>.';
  const d = openDlg('Signed in as ' + state.user,
    '<p>You are signed in on this device and will stay signed in.</p>' +
    '<p>To open AmberSight like an app, add it to your home screen:</p><p>' + how + '</p>' +
    '<button class="primary" id="hsDone">Done</button>', true);
  d.querySelector('#hsDone').onclick = () => d.__close();
}

export function applyWho(who) {
  state.authRequired = !!who.auth_required;
  state.signedIn = !!who.signed_in;
  state.user = who.user || '';
  state.isAdmin = !!who.admin;
  // (An older server sends no role: admin or viewer from the flag.)
  state.role = who.role || (who.admin ? 'admin' : 'viewer');
  state.perms = who.perms || null;
  state.limited = !!who.limited;
  renderUserMenu();
}

// The account area (top right; the "..." menu on a phone). Without accounts it says so plainly
// and offers to add a password.
function renderUserMenu() {
  const wrap = $('userMenu');
  const authed = state.authRequired && state.signedIn;
  wrap.hidden = !authed && state.authRequired;   // (the sign-in gate is showing)
  wrap.classList.toggle('noauth', !state.authRequired);
  const what = state.isAdmin ? '' : state.role === 'operator' ? ' (operator)' : ' (view only)';
  $('userName').textContent = authed ? state.user + what : '';
  const info = $('acctInfo');
  if (authed) info.innerHTML = '<div>Signed in as <b>' + esc(state.user) + '</b>' + what + '</div>';
  else if (!state.authRequired) {
    info.innerHTML = '<div><b>Not signed in</b> &middot; anyone on your network can use this</div>' +
      (state.isAdmin ? '<a class="btn" href="#settings/users/users" data-addpw>Add a password</a>' : '');
  } else info.textContent = '';
  info.hidden = !info.innerHTML;
  for (const id of ['menuAcct', 'menuPw', 'menuOut']) { const b = $(id); if (b) b.hidden = !authed; }
}

export function showGate(msg) {
  state.signedIn = false;
  renderUserMenu();
  $('loginGate').classList.add('on');
  if (msg !== undefined) $('credMsg').textContent = msg;
  const u = $('credUser');
  if (u && !u.value) u.focus();
}
export function hideGate() {
  $('loginGate').classList.remove('on');
  $('credMsg').textContent = '';
  $('credPass').value = '';
  showStep(null);
}

// The second sign-in step: the password was right, and the code goes with this ticket.
let ticket = null;
let useRecovery = false;
let wrongCodes = 0;
let submitting = false;
function showStep(t, recoveryOnly) {
  ticket = t;
  useRecovery = false;
  wrongCodes = 0;
  const two = !!t;
  $('credStep1').hidden = two;
  $('credStep2').hidden = !two;
  $('credLinks').hidden = !two;
  $('credCodeBox').hidden = false;
  $('credRecBox').hidden = true;
  $('credUseRec').textContent = 'Use a recovery code';
  $('credCode').value = '';
  $('credRec').value = '';
  $('credGo').textContent = two ? 'Continue' : 'Sign in';
  if (two) $('credCode').focus();
  // Six-digit codes are locked here: a recovery code is the way in, and there is no other choice.
  if (two && recoveryOnly) {
    toggleRecovery();
    $('credUseRec').hidden = true;
    $('credMsg').textContent = 'Too many wrong codes were tried for this account, so only a recovery code works right now. Someone may have your password: change it once you are in.';
  } else $('credUseRec').hidden = false;
}
function toggleRecovery() {
  useRecovery = !useRecovery;
  $('credCodeBox').hidden = useRecovery;
  $('credRecBox').hidden = !useRecovery;
  $('credUseRec').textContent = useRecovery ? 'Use a code from your app' : 'Use a recovery code';
  $('credMsg').textContent = '';
  // (What was typed in the box now hidden goes: it must not count for the box in use.)
  $(useRecovery ? 'credCode' : 'credRec').value = '';
  (useRecovery ? $('credRec') : $('credCode')).focus();
}

// A session cookie has arrived: check it stuck, and go in.
async function finishSignIn(recoveryLeft, withCode) {
  const who = await whoami();
  applyWho(who);
  if (!who.signed_in) {
    $('credMsg').textContent = who.session_cookie
      ? 'Sign-in did not stick: the browser sent an old session. Clear this site\'s cookies and try again.'
      : 'Sign-in did not stick: the browser did not keep the sign-in cookie. Allow cookies for this site (and trust the certificate) and try again.';
    return;
  }
  hideGate();
  bus.emit('signedin');
  if (!withCode) import('./account.js').then(m => m.offerTwoStep()).catch(() => {});
  if (recoveryLeft !== undefined) {
    msgDlg('You used a recovery code. It will not work again. You have ' + recoveryLeft + ' left.' + (recoveryLeft <= 2 ? ' Make new ones in Your account, in the menu at the top.' : ''));
  }
}

async function doCode() {
  const typed = (useRecovery ? $('credRec') : $('credCode')).value.trim();
  if (!typed) { $('credMsg').textContent = useRecovery ? 'Enter a recovery code.' : 'Enter the 6-digit code.'; return; }
  // (Not sent: it could not be right, and a wrong code counts against the limit.)
  if (!useRecovery && /[^0-9\s]/.test(typed)) { $('credMsg').textContent = 'A code from your app is 6 digits. For a recovery code, choose Use a recovery code below.'; return; }
  const btn = $('credGo');
  btn.disabled = true;
  try {
    const r = await fetch('/api/login/totp', {
      method: 'POST', credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(Object.assign({ ticket }, useRecovery ? { recovery_code: typed } : { code: typed })),
    });
    if (r.ok) {
      const d = await r.json().catch(() => ({}));
      await finishSignIn(d.recovery_left, true);
      return;
    }
    const text = (await r.text().catch(() => '')) || '';
    if (r.status === 429) {
      wrongCodes = 0;
      if (useRecovery) { $('credMsg').textContent = 'Too many tries at a recovery code. Wait 10 minutes and try again.'; return; }
      // The same ticket takes a recovery code.
      toggleRecovery();
      $('credMsg').textContent = 'Too many wrong codes, so codes wait a while and a recovery code works instead. If your codes keep failing, the clock on your phone or on this server may be off.';
      return;
    }
    if (r.status === 400) { $('credMsg').textContent = useRecovery ? 'Enter a recovery code.' : 'Enter the 6-digit code.'; return; }
    if (r.status === 401 && /expired/.test(text)) {
      showStep(null);
      $('credPass').value = '';
      $('credPass').focus();
      $('credMsg').textContent = 'That took a while: enter your password again.';
      return;
    }
    if (r.status === 401) {
      wrongCodes++;
      $('credMsg').textContent = useRecovery ? 'That recovery code is wrong or was already used.'
        : wrongCodes >= 2 ? 'That code is wrong too. The clock on your phone or on this server may be off. You can also use a recovery code.'
        : 'That code is wrong. Try the newest one.';
      (useRecovery ? $('credRec') : $('credCode')).value = '';
      (useRecovery ? $('credRec') : $('credCode')).focus();
      return;
    }
    // Many recovery codes being checked (it did not count), or this one still is.
    if (r.status === 503 || r.status === 409) { $('credMsg').textContent = text ? text.charAt(0).toUpperCase() + text.slice(1) + '.' : 'Try again in a moment.'; return; }
    $('credMsg').textContent = 'Sign-in failed (' + r.status + ').';
  } catch (e) {
    $('credMsg').textContent = 'Server unreachable.';
  } finally {
    btn.disabled = false;
  }
}

async function doLogin() {
  const user = $('credUser').value.trim();
  const pass = $('credPass').value;
  if (!user) { $('credMsg').textContent = 'Enter a username.'; return; }
  const btn = $('credGo');
  btn.disabled = true;
  try {
    const r = await fetch('/api/login', {
      method: 'POST', credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username: user, password: pass }),
    });
    if (r.status === 401) { $('credMsg').textContent = 'Wrong username or password.'; $('credPass').value = ''; $('credPass').focus(); return; }
    if (r.status === 429) {
      const why = (await r.text().catch(() => '')) || '';
      $('credMsg').textContent = /code/.test(why) ? 'Too many wrong codes were tried. Wait a while and try again, or sign in on a device that is still signed in and change your password.' : 'Too many failed sign-ins. Wait a few minutes and try again.';
      return;
    }
    if (r.status === 503) { $('credMsg').textContent = 'Many sign-ins are being checked right now. Try again in a moment.'; return; }
    if (!r.ok) { $('credMsg').textContent = 'Sign-in failed (' + r.status + ').'; return; }
    const d = await r.json().catch(() => ({}));
    if (d.totp_required) {
      $('credPass').value = '';
      $('credMsg').textContent = '';
      showStep(d.ticket, d.recovery_only);
      return;
    }
    await finishSignIn();
  } catch (e) {
    $('credMsg').textContent = 'Server unreachable.';
  } finally {
    btn.disabled = false;
  }
}

export async function logout() {
  // This browser stops getting this account's alerts.
  await dropThisDevice();
  try { await fetch('/api/logout', { method: 'POST', credentials: 'same-origin' }); } catch (e) { /* offline: still drop the UI */ }
  state.signedIn = false;
  bus.emit('signedout');
  showGate('You have been signed out.');
}

let pwDlg = null;
async function changePasswordDlg() {
  closeMenu();
  if (raiseDlg(pwDlg)) return;
  // With two-step codes on, a code goes with it.
  let twoStep = false;
  try { twoStep = (await api('/api/me/account')).two_step.on; } catch (e) { /* the server says if it is needed */ }
  const d = pwDlg = openDlg('Change password',
    '<label>Current password <input id="cpCur" type="password" autocomplete="current-password"></label>' +
    '<label>New password (at least 8 characters) <input id="cpNew" type="password" autocomplete="new-password"></label>' +
    '<label>Repeat new password <input id="cpNew2" type="password" autocomplete="new-password"></label>' +
    (twoStep ? '<label>Code from your authenticator app<input id="cpCode" autocomplete="one-time-code" autocapitalize="none" spellcheck="false"></label><div class="hint">You can also enter a recovery code.</div>' : '') +
    '<div class="hint">Your other devices are signed out. Device tokens keep working: to cut one off, revoke it in Your account.</div>' +
    '<div class="hint" id="cpMsg" style="color:#e07b5c"></div>' +
    '<button class="primary" id="cpGo">Change password</button>', true);
  const msg = d.querySelector('#cpMsg');
  d.querySelector('#cpGo').onclick = async () => {
    const cur = d.querySelector('#cpCur').value, nw = d.querySelector('#cpNew').value, nw2 = d.querySelector('#cpNew2').value;
    if (nw.length < 8) { msg.textContent = 'The new password needs at least 8 characters.'; return; }
    if (nw !== nw2) { msg.textContent = 'The two new passwords differ.'; return; }
    const box = d.querySelector('#cpCode');
    if (box && !box.value.trim()) { msg.textContent = 'Enter the code from your app.'; return; }
    const fields = box ? (await import('./account.js')).codeFields(box.value) : {};
    try {
      await api('/api/me/password', { json: Object.assign({ current: cur, new: nw }, fields) });
      d.__close();
      toast('Password changed');
    } catch (e) {
      if (e instanceof ApiError && e.status === 403) msg.textContent = /signed out/.test(e.body || '') ? 'Too many wrong tries: you have been signed out.' : /code/.test(e.body || '') ? 'That two-step code is wrong.' : 'The current password is wrong.';
      else if (e instanceof ApiError && e.status === 429) msg.textContent = 'Too many tries. Wait a while and try again.';
      else if (e instanceof ApiError && e.status === 400) msg.textContent = e.body || 'The new password was rejected.';
      else msg.textContent = e.message;
    }
  };
}

// Per-platform steps, the visitor's own platform first and open.
function trustSteps() {
  const ua = navigator.userAgent;
  const mine = /Android/.test(ua) ? 'android'
    : (/iPhone|iPad|iPod/.test(ua) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1)) ? 'ios'
    : /Mac OS X/.test(ua) ? 'mac' : /Windows/.test(ua) ? 'win' : '';
  const steps = {
    android: ['Android (Pixel, Samsung ...)',
      '<li>Tap <b>Download the certificate</b> above. If Android offers to open it and then says "Install CA certificates in Settings" (it may say "from null"), that is normal: tap Close. Android only installs these from inside Settings, and a web page is not allowed to open that screen for you.</li>' +
      '<li>Open the <b>Settings</b> app and search for <b>CA certificate</b> (the search box at the top). On a Pixel the path is Security &amp; privacy, More security &amp; privacy, Encryption &amp; credentials, Install a certificate, CA certificate.</li>' +
      '<li>Tap <b>Install anyway</b>, then pick <b>AmberSight-CA.crt</b> from Downloads.</li>' +
      '<li>Close and reopen the browser.</li>'],
    ios: ['iPhone / iPad',
      '<li>Open this page in Safari and tap <b>Download the certificate</b>, then Allow.</li>' +
      '<li>Settings, <b>Profile Downloaded</b> (near the top), Install.</li>' +
      '<li>Settings, General, About, <b>Certificate Trust Settings</b>: switch on full trust for AmberSight.</li>'],
    win: ['Windows',
      '<li>Open the downloaded file (double-click).</li><li>Install Certificate, then Local Machine or Current User.</li>' +
      '<li>Choose "Place all certificates in the following store" and pick Trusted Root Certification Authorities.</li>'],
    mac: ['macOS',
      '<li>Open Keychain Access and choose the System keychain.</li><li>Import the downloaded file.</li>' +
      '<li>Open it, expand Trust, set "When using this certificate" to Always Trust.</li>'],
  };
  const order = ['android', 'ios', 'win', 'mac'].sort((a, b) => (b === mine) - (a === mine));
  return order.map(k => '<details' + (k === (mine || 'win') ? ' open' : '') + '><summary>' + steps[k][0] + '</summary><ol>' + steps[k][1] + '</ol></details>').join('');
}

let trustDlg = null;
async function trustServerDlg() {
  closeMenu();
  if (raiseDlg(trustDlg)) return;
  let t = { mode: 'off', names: [], expires: 0, ca_fingerprint_sha256: null };
  try { t = await api('/api/tls'); } catch (e) { /* show what we know */ }
  const secure = location.protocol === 'https:';
  const badge = secure
    ? '<div class="tlsbadge ok">This page is secure (HTTPS)</div>'
    : '<div class="tlsbadge warn">This page is not using HTTPS yet</div>';
  let body = badge;
  if (t.mode === 'private-ca') {
    const exp = t.expires ? new Date(t.expires * 1000).toLocaleDateString() : '';
    body +=
      '<p class="hint">AmberSight signs its own certificate; nothing is published anywhere. Install the certificate once on each device and it will trust this server (the microphone for two-way audio needs a trusted HTTPS page).</p>' +
      '<div class="row"><a class="btn primary" href="/tls/ca.crt" download="AmberSight-CA.crt">Download the certificate</a>' +
      '<a class="btn" href="/tls/ca.pem" download="AmberSight-CA.pem">PEM format</a></div>' +
      '<div class="sec">Check this fingerprint (SHA-256)</div><code class="tlsfp">' + esc(t.ca_fingerprint_sha256 || '') + '</code>' +
      '<div class="sec">Addresses covered' + (exp ? ' (current certificate valid until ' + esc(exp) + ')' : '') + '</div>' +
      '<div class="tlsnames">' + (t.names || []).map((n) => '<span>' + esc(n) + '</span>').join('') + '</div>' +
      '<div class="sec">How to trust it</div>' +
      trustSteps() +
      '<p class="hint">Restart the browser afterwards. Plain http:// addresses on this port redirect to https:// automatically.</p>';
  } else if (t.mode === 'files') {
    body += '<p class="hint">This server uses a certificate supplied by the administrator (tls_cert / tls_key), so there is nothing to install here.</p>';
  } else if (t.mode === 'off') {
    body += state.isAdmin
      ? '<p class="hint">HTTPS is turned off. Turn it on in Settings (Users &amp; security, HTTPS); AmberSight then makes its own certificate, and this is where devices get it.</p>' +
        '<div class="row"><button class="primary" data-gohttps>Open HTTPS settings</button></div>'
      : '<p class="hint">HTTPS is turned off on this server. Ask an admin to turn on HTTPS.</p>';
  } else {
    body += '<p class="hint">This server uses a self-signed certificate; your browser will ask you to accept it.</p>';
  }
  trustDlg = openDlg('Trust this server (HTTPS)', body, true);
  const go = trustDlg.querySelector('[data-gohttps]');
  if (go) go.onclick = () => { trustDlg.__close(); openHttpsSettings(); };
}

// Settings > Users & security > HTTPS (admins).
async function openHttpsSettings() {
  (await import('./settings.js')).goSettings('users/https');
}

function closeMenu() { $('userDrop').hidden = true; $('userBtn').setAttribute('aria-expanded', 'false'); }

export function initAuthUi() {
  // One at a time: Enter twice, or Enter while a phone fills in the code, sends one request.
  const submitOnce = async () => {
    if (submitting) return;
    submitting = true;
    try { await (ticket ? doCode() : doLogin()); } finally { submitting = false; }
  };
  $('loginForm').addEventListener('submit', (ev) => { ev.preventDefault(); submitOnce(); });
  $('credUseRec').onclick = toggleRecovery;
  $('credBack').onclick = () => { showStep(null); $('credMsg').textContent = ''; $('credPass').focus(); };
  // Six digits are all there is to a code (and a phone may fill them in by itself).
  $('credCode').addEventListener('input', () => {
    if (ticket && $('credCode').value.replace(/\s/g, '').length === 6) submitOnce();
  });
  $('userBtn').onclick = (ev) => {
    ev.stopPropagation();
    const drop = $('userDrop');
    drop.hidden = !drop.hidden;
    $('userBtn').setAttribute('aria-expanded', String(!drop.hidden));
  };
  document.addEventListener('click', (ev) => { if (!ev.target.closest('#userMenu')) closeMenu(); });
  $('menuPw').onclick = changePasswordDlg;
  $('menuAcct').onclick = async () => { closeMenu(); (await import('./account.js')).openAccount(); };
  // Theme: Auto (follow this device) / Light / Dark.
  setTheme(themeMode());
  document.querySelectorAll('.themeseg [data-mode]').forEach(b => { b.onclick = (ev) => { ev.stopPropagation(); setTheme(b.dataset.mode); }; });
  $('acctInfo').addEventListener('click', (ev) => { if (ev.target.closest('[data-addpw]')) closeMenu(); });
  // The phone's "..." menu also holds the pages the bottom bar leaves out.
  const goFrom = (id) => async () => { closeMenu(); (await import('./nav.js')).goTab(id); };
  $('menuTl').onclick = goFrom('timelapse');
  $('menuHelp').onclick = goFrom('help');
  $('menuKeys').onclick = () => { closeMenu(); $('keysBtn').click(); };
  $('menuPush').onclick = () => { closeMenu(); openPushDlg(); };
  $('menuTrust').onclick = trustServerDlg;
  const showPopups = () => { const on = alertPopups(); $('menuPopups').textContent = 'Alert pop-ups: ' + (on ? 'On' : 'Off'); $('menuPopups').setAttribute('aria-checked', String(on)); };
  showPopups();
  $('menuPopups').onclick = (ev) => { ev.stopPropagation(); setAlertPopups(!alertPopups()); showPopups(); };
  const showAwake = () => { const on = keepAwake(); $('menuAwake').textContent = 'Keep screen awake: ' + (on ? 'On' : 'Off'); $('menuAwake').setAttribute('aria-checked', String(on)); };
  if (wakeSupported()) { showAwake(); $('menuAwake').onclick = (ev) => { ev.stopPropagation(); setKeepAwake(!keepAwake()); showAwake(); }; }
  else $('menuAwake').hidden = true;
  // Text size, per device: Normal -> Large -> Larger (applied before first paint in index.html).
  const SIZES = [['', 'Normal'], ['l', 'Large'], ['xl', 'Larger']];
  const showText = () => { const cur = document.documentElement.dataset.text || ''; $('menuText').textContent = 'Text size: ' + (SIZES.find(x => x[0] === cur) || SIZES[0])[1]; };
  showText();
  $('menuText').onclick = (ev) => {
    ev.stopPropagation();
    const cur = document.documentElement.dataset.text || '';
    const next = SIZES[(SIZES.findIndex(x => x[0] === cur) + 1) % SIZES.length][0];
    if (next) document.documentElement.dataset.text = next; else delete document.documentElement.dataset.text;
    try { localStorage.setItem('ambs.textSize', next); } catch (e) { /* not remembered */ }
    showText();
  };
  // Timeline controls, per device: Docked <-> Auto-hide (applied before first paint by prepaint.js).
  const showTlCtl = () => { $('menuTlCtl').textContent = 'Timeline controls: ' + (document.documentElement.dataset.tlctl === 'autohide' ? 'Auto-hide' : 'Docked'); };
  showTlCtl();
  $('menuTlCtl').onclick = (ev) => {
    ev.stopPropagation();
    const on = document.documentElement.dataset.tlctl !== 'autohide';
    if (on) document.documentElement.dataset.tlctl = 'autohide'; else delete document.documentElement.dataset.tlctl;
    try { if (on) localStorage.setItem('ambs.tlCtl', 'autohide'); else localStorage.removeItem('ambs.tlCtl'); } catch (e) { /* not remembered */ }
    window.dispatchEvent(new Event('ambs-tlctl'));
    showTlCtl();
  };
  // Data saver, per device: Auto -> On -> Off.
  const showData = () => { const v = dataSaverSetting(); $('menuData').textContent = 'Data saver: ' + (v === 'auto' ? 'Auto (' + (dataSaver() ? 'on' : 'off') + ')' : v === 'on' ? 'On' : 'Off'); };
  showData();
  $('menuData').onclick = (ev) => {
    ev.stopPropagation();
    const v = dataSaverSetting();
    setDataSaver(v === 'auto' ? 'on' : v === 'on' ? 'off' : 'auto');
    showData();
    toast('Applies to what you open next');
  };
  $('menuOut').onclick = () => { closeMenu(); logout(); };
  bus.on('unauth', () => {
    // Signed out on purpose while moving to the http:// address (HTTPS settings): the restart dialog stays.
    if (state.leaving) return;
    if (state.signedIn || !$('loginGate').classList.contains('on')) {
      bus.emit('signedout');
      showGate(state.signedIn ? 'Your session ended. Please sign in again.' : '');
    }
  });
}
