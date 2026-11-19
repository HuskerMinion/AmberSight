// push.js - Notifications on this device (browser Web Push): alerts pop up
// like any app's notifications, even with AmberSight closed. Turn them on in
// this browser, see this account's devices (browsers and phone apps that use
// UnifiedPush), send a test. Admins also pick the push servers apps may use.
// Server: /api/push/{key,subscribe,unsubscribe,devices,test}; /sw.js shows them;
// /api/settings {push: {app_hosts}}.

import { t } from './i18n.js';
import { state, api, esc, openDlg, toast, formBar } from './core.js';
import { more } from './sp_kit.js';

const isIos = () => /iPhone|iPad|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
const standalone = () => matchMedia('(display-mode: standalone)').matches || navigator.standalone === true;

// Why notifications cannot be turned on here ('' = they can).
export function pushUnsupported() {
  if (!window.isSecureContext) return t('push.need_secure', 'Notifications need the secure https:// address, on a device that trusts AmberSight\'s certificate (account menu, Trust this server).');
  if (isIos() && !standalone()) return t('push.ios_instructions', 'On iPhone and iPad (iOS 16.4 or newer): open AmberSight in Safari, tap Share, then Add to Home Screen. Open it from the Home Screen and turn notifications on there.');
  if (!('serviceWorker' in navigator) || !('PushManager' in window) || !('Notification' in window)) return t('push.browser_no_support', 'This browser cannot show notifications from web pages.');
  return '';
}

function keyBytes(b64) {
  const s = (b64 + '='.repeat((4 - b64.length % 4) % 4)).replace(/-/g, '+').replace(/_/g, '/');
  return Uint8Array.from(atob(s), c => c.charCodeAt(0));
}
const sameBytes = (a, b) => a && b && a.byteLength === b.byteLength && new Uint8Array(a).every((x, i) => x === b[i]);

async function endpointKey(endpoint) {
  const h = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(endpoint)));
  return [...h.slice(0, 8)].map(b => b.toString(16).padStart(2, '0')).join('');
}

async function registration() {
  try {
    await navigator.serviceWorker.register('/sw.js', { scope: '/' });
  } catch (e) {
    throw new Error(t('push.untrusted_cert', 'This device does not trust AmberSight\'s certificate yet, so it cannot get notifications. Use Trust this server in the account menu, then try again.'));
  }
  return navigator.serviceWorker.ready;
}

async function currentSub() {
  if (pushUnsupported()) return null;
  const reg = await navigator.serviceWorker.getRegistration('/');
  return reg ? reg.pushManager.getSubscription() : null;
}

// Subscribe this browser (asks for permission first). Call from a click.
async function turnOn() {
  // Asked first, while the click still counts (Safari insists).
  const perm = await Notification.requestPermission();
  if (perm !== 'granted') throw new Error(perm === 'denied'
    ? t('push.blocked', 'Notifications are blocked for AmberSight in this browser. Allow them in the site settings (the lock or info icon next to the address), then try again.')
    : t('push.not_allowed', 'Notifications were not allowed.'));
  const [{ key }, reg] = await Promise.all([api('/api/push/key'), registration()]);
  const want = keyBytes(key);
  let sub = await reg.pushManager.getSubscription();
  // Made for another server key (for example after restoring a backup): start over.
  if (sub && !sameBytes(sub.options.applicationServerKey, want)) { await sub.unsubscribe().catch(() => {}); sub = null; }
  if (!sub) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: want });
  await api('/api/push/subscribe', { json: { subscription: sub.toJSON() } });
}

async function turnOff() {
  const sub = await currentSub();
  if (!sub) return;
  await api('/api/push/unsubscribe', { json: { endpoint: sub.endpoint } }).catch(() => {});
  await sub.unsubscribe().catch(() => {});
}

// At sign-in: a browser with notifications on is told to the server again
// (it may have forgotten it, or someone else signed in on this browser).
export async function syncPush() {
  try {
    if (pushUnsupported() || Notification.permission !== 'granted') return;
    navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch(() => {});
    const sub = await currentSub();
    if (sub) await api('/api/push/subscribe', { json: { subscription: sub.toJSON() } });
  } catch (e) { /* tried again next time */ }
}

// At sign-out: this browser stops getting this account's alerts.
export async function dropThisDevice() {
  // (Never holds up signing out for long.)
  try { await Promise.race([turnOff(), new Promise(r => setTimeout(r, 3000))]); } catch (e) { /* signed out anyway */ }
}

const ago = (ms) => {
  if (!ms) return '';
  const d = new Date(ms);
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
};

// The "Notifications on this device" card, inside `el`.
// (`bare`: no heading, inside a dialog that has it as its title.)
export function mountPushCard(el, bare) {
  let busy = false;
  const why = pushUnsupported();
  el.innerHTML = '<div class="ac-card pushcard">' + (bare ? '' : '<div class="ac-card-t">' + t('push.notifications_here', 'Notifications on this device') + '</div>') +
    '<label class="sw"><input type="checkbox" role="switch" data-pon' + (why ? ' disabled' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
    '<span class="sw-text"><b>' + t('push.alerts_here', 'Alerts on this device') + '</b><small>' + (state.isAdmin ? t('push.pop_up_admin', 'Camera alerts and system problems pop up here like any app\'s notifications, even when AmberSight is closed. No other app needed.') : t('push.pop_up_user', 'Camera alerts from the cameras you can see pop up here like any app\'s notifications, even when AmberSight is closed. No other app needed.')) + '</small></span></label>' +
    '<div class="hint' + (why ? ' warn' : '') + '" data-pmsg>' + esc(why) + '</div>' +
    '<div class="pushdevs" data-plist></div>' +
    '<div class="row"><button class="btn" data-ptest disabled>' + t('push.send_test', 'Send a test') + '</button><span class="hint" data-pout></span></div>' +
    (state.isAdmin ? '<div class="hint">' + t('push.each_camera', 'Each camera can leave them out (camera settings, Alerts tab, Send through).') + '</div>' : '') +
    (state.isAdmin ? '<div class="pushhosts" data-phosts></div>' : '') +
    '</div>';
  const sw = el.querySelector('[data-pon]'), msg = el.querySelector('[data-pmsg]'), list = el.querySelector('[data-plist]');
  const test = el.querySelector('[data-ptest]'), out = el.querySelector('[data-pout]');

  async function refresh() {
    let mine = '';
    try {
      const sub = await currentSub();
      sw.checked = !!sub && Notification.permission === 'granted';
      if (sub) mine = await endpointKey(sub.endpoint);
    } catch (e) { /* unsupported here */ }
    let devs = [];
    try { devs = (await api('/api/push/devices')).devices || []; } catch (e) { list.innerHTML = '<div class="hint warn">' + esc(e.message) + '</div>'; return; }
    test.disabled = !devs.length;
    list.innerHTML = devs.length
      ? '<div class="pushdev-h">' + t('push.your_devices', 'Your devices with notifications on') + '</div>' + devs.map(d =>
        '<div class="pushdev"><span class="pushdev-n">' + esc(d.device) + (d.key === mine ? ' ' + t('push.this_device', '<em>this device</em>') : '') +
          (d.kind === 'app' ? '<small class="pushdev-s' + (d.allowed === false ? ' warn' : '') + '">' +
            esc(d.allowed === false ? t('push.server_not_allowed', 'Push server {server} is no longer allowed', { server: d.server || '' }) : t('push.through', 'through {server}', { server: d.server || t('push.its_server', 'its push server') })) + '</small>' : '') + '</span>' +
        '<span class="badge' + (d.kind === 'app' ? ' info' : '') + '">' + (d.kind === 'app' ? t('push.app', 'App') : t('push.browser', 'Browser')) + '</span>' +
        '<span class="pushdev-t">' + (d.last_ok_ms ? t('push.last_alert', 'last alert {when}', { when: ago(d.last_ok_ms) }) : t('push.added', 'added {when}', { when: ago(d.added_ms) })) + '</span>' +
        '<button class="btn" data-rm="' + d.id + '" title="' + esc(t('push.stop_here', 'Stop notifications on this device')) + '">' + t('push.remove', 'Remove') + '</button></div>').join('')
      : '';
    list.querySelectorAll('[data-rm]').forEach(b => {
      b.onclick = async () => {
        b.disabled = true;
        const dev = devs.find(x => String(x.id) === b.dataset.rm);
        try {
          if (dev && dev.key === mine) await turnOff();
          else await api('/api/push/unsubscribe', { json: { id: parseInt(b.dataset.rm, 10) } });
        } catch (e) { toast(e.message); }
        refresh();
      };
    });
  }

  sw.onchange = async () => {
    if (busy) return;
    busy = true; sw.disabled = true;
    const on = sw.checked;
    msg.classList.remove('warn'); msg.textContent = on ? t('push.turning_on', 'Turning on...') : '';
    try {
      if (on) { await turnOn(); msg.textContent = ''; toast(t('push.notif_on', 'Notifications are on for this device')); }
      else { await turnOff(); toast(t('push.notif_off', 'Notifications are off for this device')); }
    } catch (e) {
      msg.classList.add('warn'); msg.textContent = e.message;
    }
    busy = false; sw.disabled = false;
    refresh();
  };
  test.onclick = async () => {
    test.disabled = true; out.textContent = t('push.sending', 'Sending...');
    try {
      const r = await api('/api/push/test', { json: {} });
      out.textContent = r.sent ? t('push.sent_to', 'Sent to {n, plural, one {# device} other {# devices}}. It should pop up in a few seconds.', { n: r.sent }) : (r.failed ? t('push.not_sent_logs', 'Not sent (see Settings, System, Logs).') : t('push.not_sent', 'Not sent.'));
      if (r.removed) out.textContent += ' ' + t('push.removed_n', '{n, plural, one {# device had turned notifications off and was removed.} other {# devices had turned notifications off and were removed.}}', { n: r.removed });
    } catch (e) { out.textContent = e.message; }
    refresh();
  };
  refresh();
  const hostsEl = el.querySelector('[data-phosts]');
  if (hostsEl) mountAppHosts(hostsEl);
}

// Admins: the push servers phone apps may use (UnifiedPush distributors, usually ntfy).
async function mountAppHosts(el) {
  let hosts;
  try { hosts = ((await api('/api/settings')).push || {}).app_hosts || []; }
  catch (e) { el.innerHTML = '<div class="hint warn">' + esc(e.message) + '</div>'; return; }
  el.innerHTML = more(t('push.advanced', 'Advanced: push servers for phone apps'),
    '<label class="fld"><span class="fld-l">' + t('push.allowed_servers', 'Allowed servers') + '</span><input data-hosts value="' + esc(hosts.join(', ')) + '" placeholder="ntfy.sh, ntfy.home.lan:8080" spellcheck="false" autocapitalize="off"></label>' +
    '<div class="hint">' + t('push.hosts_hint', "Phone apps that use UnifiedPush deliver alerts through a push server, usually ntfy. ntfy.sh is the public ntfy server; add your own ntfy server as host:port to use it (separated by commas). This server may post to any path on a listed host and port, so give ntfy a host name or port of its own. Keep your ntfy server on the home network or Tailscale; do not port-forward it. Plain http:// works only for a server on the home network or Tailscale (only its private addresses are used). Leave it empty to allow apps only through the browsers' push services.") + '</div>');
  const inp = el.querySelector('[data-hosts]');
  formBar(el, { save: async () => {
    const list = inp.value.split(/[,;\s]+/).map(x => x.trim()).filter(Boolean);
    const r = await api('/api/settings', { json: { push: { app_hosts: list } } });
    if (r && r.error) throw new Error(r.error);
    inp.value = (((await api('/api/settings')).push || {}).app_hosts || []).join(', ');
  } });
}

// For the Getting started steps: turn notifications on in this browser (call from a click),
// whether this browser has them on, and send a test to every device of this account.
export async function enableThisDevice() { await turnOn(); }
export async function thisDeviceOn() {
  try { return !!(await currentSub()) && Notification.permission === 'granted'; } catch (e) { return false; }
}
export async function sendTestPush() { return api('/api/push/test', { json: {} }); }

// User menu > Notifications on this device (any account, viewers too).
export function openPushDlg() {
  const d = openDlg(t('push.notifications_here', 'Notifications on this device'), '<div data-pushcard></div>', true);
  mountPushCard(d.querySelector('[data-pushcard]'), true);
}
