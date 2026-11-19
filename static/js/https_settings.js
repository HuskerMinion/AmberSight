// https_settings.js - Settings > Users & security > HTTPS: turn HTTPS on or off, extra names for
// the certificate, an own certificate (Advanced), and restarting AmberSight to apply.
//   GET/POST /api/settings  {server: {tls, tls_names, tls_cert, tls_key}}  (admin)
//   GET /api/tls            what the running listener uses (mode, names, CA fingerprint, expiry)
//   POST /api/system/restart  (409 when not run by a service manager, or while an update installs)
//   POST /api/logout          (over https, before moving to http: a Secure cookie can only be cleared over https)

import { t } from './i18n.js';
import { api, esc, toast, openDlg, ic, sleep, state, errText, liveSwitch, swHtml, formBar, confirmDialog } from './core.js';
import { lead, how, more, okIc } from './sp_kit.js';
const MODES = { 'private-ca': t('https_settings.private_ca', 'HTTPS with AmberSight\'s own certificate'), files: t('https_settings.files', 'HTTPS with your own certificate'), 'self-signed': t('https_settings.self_signed', 'HTTPS (self-signed certificate)'), off: t('https_settings.off', 'Plain HTTP (not encrypted)') };
const BY_HAND = t('https_settings.by_hand', 'Restart it by hand: on Linux <code>sudo systemctl restart ambersight</code>; on Windows restart the AmberSight service (Services, or <code>ambersight service stop</code> then <code>ambersight service start</code> in an administrator prompt); on a Mac <code>ambersight service stop</code> then <code>ambersight service start</code>. Started by hand (<code>ambersight run</code>): stop it with Ctrl+C and start it again.');

export function mountHttps(el) {
  let s = null, all = {}, names = [], gone = false;
  el.innerHTML = '<p class="empty">' + t('https_settings.loading', 'Loading...') + '</p>';

  async function load() {
    let tls = { mode: 'off', names: [] };
    try { all = await api('/api/settings'); s = all.server || {}; } catch (e) { el.innerHTML = '<p class="empty">' + t('https_settings.could_not_load', 'Could not load settings: {err}', { err: esc(errText(e)) }) + '</p>'; return; }
    try { tls = await api('/api/tls'); } catch (e) { /* status line shows what settings say */ }
    if (gone) return;
    names = (s.tls_names || []).slice();
    draw(tls);
  }

  function statusHtml(tls) {
    const mode = (s.running && s.running.mode) || tls.mode || 'off';
    const on = mode !== 'off';
    let h = '<div class="hs-status"><span class="hs-pill ' + (on ? 'ok' : 'warn') + '">' + (on ? okIc : ic('alert')) + ' ' + (on ? t('https_settings.now', 'Now: {mode}', { mode: esc(MODES[mode] || mode) }) : t('https_settings.not_encrypted', 'Not encrypted: plain HTTP')) + '</span>';
    const note = (s.running && s.running.note) || tls.note;
    if (note) h += '<div class="hint bad" role="alert"><b>' + esc(note) + '</b></div>';
    if (mode === 'private-ca') {
      const exp = tls.expires ? new Date(tls.expires * 1000).toLocaleDateString() : '';
      if (exp) h += '<div class="hint">' + t('https_settings.valid_until', 'Certificate valid until {date} (renewed automatically).', { date: esc(exp) }) + '</div>';
      if (tls.ca_fingerprint_sha256) h += '<div class="hint">' + t('https_settings.ca_fingerprint', 'Certificate authority fingerprint (SHA-256):') + '</div><code class="tlsfp">' + esc(tls.ca_fingerprint_sha256) + '</code>';
      if ((tls.names || []).length) h += '<div class="hint">' + t('https_settings.covers', 'Covers:') + '</div><div class="tlsnames">' + tls.names.map(n => '<span>' + esc(n) + '</span>').join('') + '</div>';
    }
    return h + '</div>';
  }

  function bannerHtml() {
    if (!s.restart_needed) return '';
    const toHttp = !s.tls && location.protocol === 'https:';
    return '<div class="hs-banner" role="status"><span>' + t('https_settings.restart_banner', '<b>Restart AmberSight to apply</b> the HTTPS change. It saves the setting now; the address changes to <b>{target}</b> after the restart.', { target: esc(newTarget(s.tls)) }) +
      (s.restarts_itself ? '' : '<br><span class="hint">' + BY_HAND +
      (toHttp ? ' ' + t('https_settings.sign_out_first', 'Sign out first (account menu), so this browser can sign in again on the http:// address.') : '') + '</span>') + '</span>' +
      '<button class="primary" data-hsrestart>' + t('https_settings.restart_now', 'Restart now') + '</button></div>';
  }

  function draw(tls) {
    const own = !!(s.tls_cert || s.tls_key);
    el.innerHTML = '<div class="hs">' + lead(t('https_settings.lead_text', 'Needed for phone notifications, talking through a camera and safe sign-in away from home. Each device trusts this server once.'), t('https_settings.lead_btn', 'Turn it on.')) + bannerHtml() + statusHtml(tls) +
      '<label class="sw"><input type="checkbox" role="switch" data-hson data-live' + (s.tls ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
      '<span class="sw-text"><b>' + t('https_settings.use_https', 'Use HTTPS (encrypted connection)') + '</b><small>' + t('https_settings.use_https_hint', 'AmberSight makes its own private certificate; nothing is published anywhere.') + '</small></span></label>' +
      how(t('https_settings.how_text', 'Each device trusts the certificate once (<a href="#" data-hstrust>account menu, Trust this server</a>). Browsers only allow notifications, the microphone (talking through a camera) and keeping the screen awake on a trusted HTTPS page. Plain http:// addresses on the same port redirect to https://.')) +
      '<details class="hs-adv ac-more sp-more"' + (own || names.length ? ' open' : '') + '><summary>' + t('https_settings.advanced', 'Advanced') + '</summary>' +
      '<div class="sec">' + t('https_settings.extra_names_sec', 'Extra names for the certificate') + '</div>' +
      '<div class="hint">' + t('https_settings.extra_names_hint1', 'The certificate already covers this computer\'s addresses, its name and localhost. Add any other name you open AmberSight by: names ending in .lan, .local, .home.arpa or .internal (such as nvr.lan) and home-network or Tailscale addresses (100.x.y.z) can always be added.') + ' ' +
      t('https_settings.extra_names_hint2', 'Other names, such as a Tailscale name (nvr.tail1234.ts.net), can only be covered by a certificate authority made with them: add them before HTTPS is first turned on, or use your own certificate.') + '</div>' +
      '<div class="tlsnames hs-names" data-hsnames></div>' +
      '<div class="row"><input data-nt data-hsname placeholder="nvr.lan" spellcheck="false" autocapitalize="off" autocomplete="off" aria-label="' + t('https_settings.extra_name', 'Extra name') + '"><button type="button" class="btn" data-hsadd>' + ic('plus') + ' ' + t('https_settings.add', 'Add') + '</button></div>' +
      '<div class="sec">' + t('https_settings.own_cert', 'Your own certificate') + '</div>' +
      swHtml('data-hsown', own, t('https_settings.use_own_cert', 'Use my own certificate files')) +
      '<div class="hint">' + t('https_settings.own_cert_hint', 'For a certificate you get elsewhere (for example from your own certificate authority). Give the paths of its PEM files on this server. AmberSight\'s own certificate is then not used, so there is nothing for devices to install. A renewed certificate is loaded at the next restart.') + '</div>' +
      '<div class="hint bad" role="note">' + t('https_settings.own_cert_warn', '<b>Linked servers and capture helpers stop working</b> with your own certificate: they trust only AmberSight\'s own certificate authority, and new links and pairings need it too.') +
      ((s.linked_servers || s.paired_helpers) ? ' ' + t('https_settings.this_server_has', 'This server has {deps}.', { deps: depsText(s) }) : '') +
      ' ' + t('https_settings.work_again', 'They work again when you switch back (AmberSight keeps its certificate authority).') + '</div>' +
      '<div data-hsfiles' + (own ? '' : ' hidden') + '>' +
      '<label class="fld"><span class="fld-l">' + t('https_settings.cert_file', 'Certificate file (full chain)') + '</span><input data-hscert value="' + esc(s.tls_cert || '') + '" placeholder="/etc/ambersight/fullchain.pem" spellcheck="false" autocapitalize="off"></label>' +
      '<label class="fld"><span class="fld-l">' + t('https_settings.key_file', 'Private key file') + '</span><input data-hskey value="' + esc(s.tls_key || '') + '" placeholder="/etc/ambersight/privkey.pem" spellcheck="false" autocapitalize="off"></label></div></details>' +
      '<input type="text" data-hsnamesval hidden aria-hidden="true" tabindex="-1" value="' + esc(names.join(',')) + '"></div>';
    drawNames();
    const q = (x) => el.querySelector(x);
    q('[data-hstrust]').onclick = (e) => { e.preventDefault(); const m = document.getElementById('menuTrust'); if (m) m.click(); };
    q('[data-hsown]').onchange = (e) => { q('[data-hsfiles]').hidden = !e.target.checked; };
    const add = () => {
      const i = q('[data-hsname]');
      for (const n of i.value.split(/[,;\s]+/).map(x => x.trim()).filter(Boolean)) if (!names.includes(n.toLowerCase())) names.push(n.toLowerCase());
      i.value = ''; drawNames();
    };
    q('[data-hsadd]').onclick = add;
    q('[data-hsname]').onkeydown = (e) => { if (e.key === 'Enter') { e.preventDefault(); add(); } };
    // HTTPS on / off saves when flipped (turning it off asks first); names and certificate files use the bar.
    liveSwitch(q('[data-hson]'), setTls);
    formBar(q('.hs'), { save, revert: () => { names = (s.tls_names || []).slice(); load(); } });
    const r = q('[data-hsrestart]');
    if (r) r.onclick = () => restart(!!s.tls);
  }

  function drawNames() {
    const box = el.querySelector('[data-hsnames]');
    box.innerHTML = names.length ? names.map((n, i) => '<span>' + esc(n) + '<button type="button" class="chipx" data-hsdel="' + i + '" aria-label="' + t('https_settings.remove_name', 'Remove {name}', { name: esc(n) }) + '">&times;</button></span>').join('') : '<span class="hs-none">' + t('https_settings.no_extra_names', 'No extra names') + '</span>';
    box.querySelectorAll('[data-hsdel]').forEach(b => { b.onclick = () => { names.splice(+b.dataset.hsdel, 1); drawNames(); }; });
    // The bar watches fields, not chips: mirror the list into one.
    const v = el.querySelector('[data-hsnamesval]');
    if (v && v.value !== names.join(',')) { v.value = names.join(','); v.dispatchEvent(new Event('input', { bubbles: true })); }
  }

  // The switch: HTTPS on or off right away (the restart banner then shows).
  async function setTls(on) {
    if (s.tls && !on && !(await confirmOff(s, all.alerts || {}))) throw Object.assign(new Error('cancelled'), { cancelled: true });
    if (!s.tls && on && !(await confirmDialog({ title: t('https_settings.turn_on_title', 'Turn HTTPS on?'), body: t('https_settings.turn_on_body', 'The address changes to {target} and every device needs to trust this server once.', { target: newTarget(true) }), ok: t('https_settings.turn_on', 'Turn HTTPS on') }))) throw Object.assign(new Error('cancelled'), { cancelled: true });
    const turningOn = !s.tls && on;
    const r = await api('/api/settings', { json: { server: { tls: on } } });
    if (r && r.error) throw new Error(r.error);
    await load();
    // Alert links still point at http://: they keep working (http redirects to https), but should change.
    const lu = ((all.alerts || {}).link_url || '').trim();
    if (turningOn && /^http:\/\//i.test(lu)) toast(t('https_settings.alert_links_http', 'Alert links still use {url}: that keeps working (it redirects to https://), but change it to the https:// address in Settings, Detection & alerts, Limits & priority.', { url: lu }), { ms: 15000 });
    if (r.warning) toast(r.warning, { kind: 'err' });
    if (r.restart_needed) return false;
  }

  // The bar: extra names and the own-certificate files.
  async function save() {
    const q = (x) => el.querySelector(x);
    const typed = q('[data-hsname]').value.trim();
    if (typed) throw new Error(t('https_settings.press_add', 'Press Add for "{typed}" first, or clear it.', { typed }));
    const own = q('[data-hsown]').checked;
    const cert = own ? q('[data-hscert]').value.trim() : '', key = own ? q('[data-hskey]').value.trim() : '';
    if (own && (!cert || !key)) throw new Error(t('https_settings.give_both_files', 'Give both the certificate file and the key file, or turn off "Use my own certificate".'));
    const r = await api('/api/settings', { json: { server: { tls_names: names, tls_cert: cert, tls_key: key } } });
    if (r && r.error) throw new Error(r.error);
    await load();
    if (r.warning) { toast(r.warning, { kind: 'err' }); return false; }
    if (r.restart_needed) { toast(t('https_settings.saved_restart', 'Saved. Restart AmberSight to apply.'), { kind: 'ok', ms: 4000 }); return false; }
  }

  load();
  return () => { gone = true; };
}

// "2 linked servers and 1 capture helper"
function depsText(s) {
  const parts = [];
  if (s.linked_servers) parts.push(t('https_settings.n_linked', '{n, plural, one {# linked server} other {# linked servers}}', { n: s.linked_servers }));
  if (s.paired_helpers) parts.push(t('https_settings.n_helpers', '{n, plural, one {# paired capture helper} other {# paired capture helpers}}', { n: s.paired_helpers }));
  return parts.length === 2 ? t('https_settings.deps_both', '{a} and {b}', { a: parts[0], b: parts[1] }) : parts.join('');
}

// Turning HTTPS off: what stops working, asked before saving.
function confirmOff(s, alerts) {
  const lu = (alerts.link_url || '').trim();
  return new Promise((resolve) => {
    const d = openDlg(t('https_settings.turn_https_off_title', 'Turn HTTPS off?'),
      '<p>' + t('https_settings.turn_https_off_desc', 'After the restart AmberSight answers only at <b>{target}</b>, and the connection is no longer encrypted.', { target: esc(newTarget(false)) }) + '</p>' +
      '<ul class="hs-list"><li>' + t('https_settings.phone_apps_bookmarks', 'Phone apps and bookmarks that use an https:// address must be changed to the http:// address.') + '</li>' +
      '<li>' + t('https_settings.capture_helpers_stop', 'Capture helpers (computers that send their screen) stop: they only connect over HTTPS.{extra}', { extra: s.paired_helpers ? ' ' + t('https_settings.this_server_has', 'This server has {deps}.', { deps: depsText({ paired_helpers: s.paired_helpers }) }) : '' }) + '</li>' +
      '<li>' + t('https_settings.linked_stop', 'Other servers linked to this one, and the server switcher, stop working until HTTPS is on again.') + (s.linked_servers ? ' ' + t('https_settings.this_server_has', 'This server has {deps}.', { deps: depsText({ linked_servers: s.linked_servers }) }) : '') +
      ' ' + t('https_settings.back_on_restores', 'Turning HTTPS back on restores them (AmberSight keeps its certificate authority).') + '</li>' +
      (/^https:\/\//i.test(lu) ? '<li>' + t('https_settings.links_in_alerts', 'Links in alerts still point at <b>{lu}</b> until you change them in {link}.', { lu: esc(lu), link: '<a href="#settings/detection/limits" data-alerts>' + t('https_settings.alerts_path', 'Settings, Detection &amp; alerts, Limits &amp; priority') + '</a>' }) + '</li>' : '') +
      '<li>' + t('https_settings.notifications_browser', 'Notifications in the browser, the microphone (talking through a camera) and keep screen awake stop working: browsers only allow them on HTTPS pages.') + '</li>' +
      '<li>' + t('https_settings.passwords_unencrypted', 'Passwords and video travel unencrypted on the network.') + '</li>' +
      '<li>' + t('https_settings.sign_in_http', 'You will need to sign in again on the http:// address.') + '</li></ul>' +
      '<div class="row"><button class="primary danger" data-yes>' + t('https_settings.btn_turn_off', 'Turn HTTPS off') + '</button><button class="btn" data-no>' + t('https_settings.btn_keep', 'Keep HTTPS') + '</button></div>', true);
    let done = false;
    const end = (v) => { if (done) return; done = true; resolve(v); if (d.isConnected) d.__close(); };
    d.querySelector('[data-yes]').onclick = () => end(true);
    d.querySelector('[data-no]').onclick = () => end(false);
    // Off to the alert settings first: HTTPS stays on.
    const al = d.querySelector('[data-alerts]');
    if (al) al.onclick = () => end(false);
    // Closed another way (Esc, the X): keep HTTPS.
    new MutationObserver((m, o) => { if (!d.isConnected) { o.disconnect(); end(false); } }).observe(document.body, { childList: true });
  });
}

// This page's host and port with the new scheme, and the path the app is served under
// (behind a reverse proxy it may not be '/').
function newTarget(tls) { return (tls ? 'https://' : 'http://') + location.host + location.pathname.replace(/[^/]*$/, ''); }

// Is the server answering? `how`: 'any' (this page's address, anything at all), or 'tls' (this
// plain-http page's address now answering as an HTTPS server: /tls/ca.pem is sent over plain http,
// or a redirect to https:// - no request to the not yet trusted https:// address is needed).
async function answers(how) {
  const ctl = new AbortController();
  const tm = setTimeout(() => ctl.abort(), 2500);
  try {
    if (how === 'tls') {
      const r = await fetch('/tls/ca.pem', { redirect: 'manual', cache: 'no-store', signal: ctl.signal });
      return r.ok || r.type === 'opaqueredirect';
    }
    // A reverse proxy in front answers 502/503 while AmberSight is down: that is not up.
    const r = await fetch('/favicon.ico?restart=' + Date.now(), { cache: 'no-store', signal: ctl.signal });
    return r.status < 500;
  } catch (e) { return false; } finally { clearTimeout(tm); }
}

// Restart (POST /api/system/restart), wait until AmberSight is back, then go to the new address.
async function restart(tls) {
  const target = newTarget(tls);
  const toHttps = tls && location.protocol === 'http:', toHttp = !tls && location.protocol === 'https:';
  // Browser notifications belong to an address (its origin): they do not move along.
  const pushNote = toHttp
    ? '<p>' + t('https_settings.push_note_stop', 'Notifications on this device stop: browsers only allow them on https:// pages. When HTTPS is on again, turn them on again there (account menu, Notifications on this device).') + '</p>'
    : '<p>' + t('https_settings.push_note_belong', 'Notifications on this device belong to the old address: turn them on again at the new address (account menu, Notifications on this device).') + '</p>';
  const d = openDlg(t('https_settings.restarting_title', 'Restarting AmberSight'),
    '<div class="hs-prog"><span class="hs-spin" aria-hidden="true"></span><b data-st role="status">' + t('https_settings.restart_asking', 'Asking AmberSight to restart...') + '</b></div>' +
    '<div class="hint" data-sub>' + t('https_settings.cameras_stop', 'Cameras stop recording for a few seconds.') + '</div>' +
    (toHttps ? '<div class="hs-note"><p>' + t('https_settings.new_address', 'AmberSight\'s new address is <b>{target}</b>.', { target: esc(target) }) + '</p>' +
      '<p>' + t('https_settings.trust_warning', 'Until this device trusts AmberSight\'s certificate, the browser warns that the connection is not private. That is expected here: continue past the warning (Advanced, Continue or Proceed), then use <b>account menu, Trust this server</b> for the steps, once on each device.') + ' ' +
      t('https_settings.download_cert', 'You can also {link}.', { link: '<a href="/tls/ca.crt" download="AmberSight-CA.crt" data-ca hidden>' + t('https_settings.download_now', 'download the certificate now') + '</a><span data-canot>' + t('https_settings.download_there', 'download the certificate from there') + '</span>' }) + '</p>' +
      '<p>' + t('https_settings.signin_again_new', 'You will need to sign in again on the new address.') + '</p>' + pushNote + '</div>' : '') +
    (toHttp ? '<div class="hs-note"><p>' + t('https_settings.new_address_plain', 'AmberSight\'s new address is <b>{target}</b> (not encrypted). This browser is signed out here so it can sign in again there.', { target: esc(target) }) + '</p>' + pushNote + '</div>' : '') +
    '<div class="row" data-go hidden><a class="btn primary" href="' + esc(target) + '" data-golink>' + (toHttp ? t('https_settings.btn_open', 'Open {target}', { target: esc(target) }) : t('https_settings.btn_goto', 'Go to {target}', { target: esc(target) })) + '</a>' +
    (toHttp ? '<button type="button" class="btn" data-again>' + t('https_settings.try_again', 'Try again') + '</button>' : '') + '<span class="hint" data-count></span></div>', true);
  const st = d.querySelector('[data-st]'), sub = d.querySelector('[data-sub]');
  const set = (a, b) => { st.textContent = a; if (b != null) sub.innerHTML = b; };
  try {
    await api('/api/system/restart', { json: {} });
  } catch (e) {
    d.querySelector('.hs-spin').hidden = true;
    d.querySelectorAll('.hs-note').forEach(n => { n.hidden = true; });
    set(e && e.status === 409 ? errText(e) : t('https_settings.could_not_restart', 'Could not restart: {err}', { err: errText(e) }), (e && e.status === 409 && /update/.test(errText(e))) ? t('https_settings.applies_after_update', 'The saved HTTPS setting applies at the restart that finishes the update, or press Restart now once it is done.') : BY_HAND + ' ' + t('https_settings.applies_on_start', 'The saved HTTPS setting applies when it starts again.'));
    return;
  }
  if (toHttp) {
    // The session cookie is Secure (set over https): an http:// page can neither replace nor clear it,
    // so sign-in there would never stick. Clear it now, over https, while the old server still answers
    // (it stops a moment after accepting the restart). Requests failing with 401 from here on are expected.
    state.leaving = true;
    try { await fetch('/api/logout', { method: 'POST', credentials: 'same-origin', cache: 'no-store' }); } catch (e) { /* stopped already */ }
  }
  set(t('https_settings.status_restarting', 'Restarting...'), t('https_settings.waiting', 'Waiting for AmberSight to stop and start again. This usually takes 10 to 30 seconds.'));
  const until = Date.now() + 120000;
  let ready = false;
  if (toHttps) {
    // Ready when this address answers as an HTTPS server.
    while (!ready && Date.now() < until && d.isConnected) { await sleep(1000); ready = await answers('tls'); }
  } else {
    // First it stops answering, then (same scheme) it answers again.
    let down = false;
    while (!down && Date.now() < until && d.isConnected) { await sleep(700); down = !(await answers('any')); }
    if (toHttp) {
      // An https:// page cannot ask the new http:// address (the browser blocks it): give it time,
      // then let the user open it.
      if (down) {
        for (let n = 15; n > 0 && d.isConnected; n--) { set(t('https_settings.starting_again', 'Starting again...'), t('https_settings.opens_in', 'The new address opens in a moment ({n} s).', { n })); await sleep(1000); }
      }
      if (!d.isConnected) return;
      d.querySelector('.hs-spin').hidden = true;
      d.querySelector('[data-go]').hidden = false;
      const msg = () => set(down ? t('https_settings.msg_starting', 'AmberSight should be starting on the new address.') : t('https_settings.msg_not_stopped', 'AmberSight has not stopped yet.'),
        t('https_settings.open_when_ready', 'Open <b>{target}</b> when you are ready. If it does not open yet, wait a little and try again: a Windows service can take up to 30 s to start after repeated restarts.', { target: esc(target) }));
      msg();
      d.querySelector('[data-again]').onclick = async () => {
        // Still answering on https://? Then the restart has not happened (or HTTPS came back on).
        set(t('https_settings.checking', 'Checking...'), '');
        const old = await answers('any');
        if (!d.isConnected) return;
        if (old) set(t('https_settings.still_answers', 'AmberSight still answers on this https:// address.'), t('https_settings.restart_maybe_not', 'The restart may not have happened yet, or HTTPS is still on. Wait a moment and try again, or reload this page and sign in again. {byHand}', { byHand: BY_HAND }));
        else { down = true; msg(); }
      };
      return;
    }
    while (down && !ready && Date.now() < until && d.isConnected) { await sleep(1000); ready = await answers('any'); }
  }
  if (!d.isConnected) return;
  d.querySelector('.hs-spin').hidden = true;
  const go = d.querySelector('[data-go]');
  go.hidden = false;
  if (toHttps) { d.querySelector('[data-ca]').hidden = false; d.querySelector('[data-canot]').hidden = true; }
  if (!ready) { set(t('https_settings.not_answered', 'AmberSight has not answered yet.'), t('https_settings.not_answered_hint', 'It may still be starting (a Windows service can take up to 30 s after repeated restarts). Try the new address in a moment; if it does not open, check that the service is running.') + ' ' + BY_HAND); return; }
  set(t('https_settings.is_back', 'AmberSight is back.'), '');
  // Read the note, then go (or press the button).
  let n = toHttps ? 15 : 5;
  const count = d.querySelector('[data-count]');
  const tick = () => {
    if (!d.isConnected) return;
    if (n <= 0) { location.href = target; return; }
    count.textContent = t('https_settings.going_there', 'Going there in {n} s', { n });
    n--; setTimeout(tick, 1000);
  };
  tick();
}
