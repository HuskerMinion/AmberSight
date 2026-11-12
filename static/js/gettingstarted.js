// gettingstarted.js - "Getting started" checklist for a new install: the steps that
// matter, each ticked off by looking at the real setup, with a button to the page that
// does it. Admins see a slim banner on Live until a camera AND alerts are set up (or
// they choose "Don't show again"); the full checklist is always in Help.
//
// Steps: Add a camera, Get alerts on your phone, Add a password, Choose where recordings
// go; then optional: Use HTTPS, Turn on smart detection (AI). A test or demo camera never
// counts. "Don't show again" is kept on the server with the other interface prefs.

import { $, state, api, esc, openDlg, toast, afterOverlayClose } from './core.js';
import * as Prefs from './prefs.js';

const HIDE_KEY = 'ambs.gsHide';
// The cap older starter configs set (new installs are Automatic: max_mb = 0).
const OLD_DEFAULT_MAX_MB = 2048;

const isTest = (c) => /^(lavfi|testsrc):/i.test(c.url || '') || /^(test pattern|zone demo|demo)\b/i.test(c.name || '');

// Where a step's button goes: a settings page (hash route), or 'trust' for HTTPS.
async function go(where) {
  if (where === 'trust') {
    // HTTPS off: its settings; on but this page not secure yet: the Trust dialog.
    let off = false;
    try { off = (await api('/api/tls')).mode === 'off'; } catch (e) { /* the dialog says */ }
    if (off) { location.hash = '#settings/users/https'; return; }
    const m = $('menuTrust');
    if (m) m.click();
    return;
  }
  location.hash = where;
}

// What the steps look at, fetched together.
async function context() {
  const [s, u, p] = await Promise.all([
    api('/api/settings'),
    api('/api/users').catch(() => ({ users: [] })),
    api('/api/push/devices').catch(() => ({ devices: [] })),
  ]);
  return { s, users: u.users || [], pushDevs: (p.devices || []).length, cams: (state.cams || []).filter(c => !c.remote && !isTest(c)) };
}

const STEPS = [
  { id: 'cameras', title: 'Add a camera', why: 'Find the cameras on your network, or add one by its address.', where: '#settings/cameras/find', label: 'Find cameras',
    done: (x) => x.cams.length > 0 },
  { id: 'alerts', title: 'Get alerts on your phone', why: 'A message with a picture when something moves at your door.', where: '#settings/detection/alerts', label: 'Set up', special: 'alerts',
    done: (x) => (x.s.alert_channels || []).some(c => c.configured) || x.pushDevs > 0 },
  { id: 'accounts', title: 'Add a password', why: 'Without a sign-in account, anyone who can reach this computer can watch and change everything.', where: '#settings/users/users', label: 'Add account',
    done: (x) => x.users.length > 0 },
  { id: 'storage', title: 'Choose where recordings go', why: 'Pick the drive for recordings and how much of it they may use. The default uses what the drive can spare.', where: '#settings/recording/drives', label: 'Choose',
    done: (x) => (x.s.storage.folders || []).length > 0 || x.s.storage.max_mb !== OLD_DEFAULT_MAX_MB },
  { id: 'https', title: 'Use HTTPS', why: 'Keeps the connection private, and phones and browsers only allow notifications and some other features over it.', where: 'trust', label: 'Set up', optional: true,
    done: () => location.protocol === 'https:' },
  { id: 'ai', title: 'Turn on smart detection (AI)', why: 'Alerts only when a person, car or animal is seen: far fewer false alarms from shadows, rain and bugs.', where: '#settings/detection/objects', label: 'Turn on', optional: true,
    done: (x) => !!x.s.ai.enabled && (!!x.s.ai_status.builtin_loaded || !!(x.s.ai.url || '').trim()) },
];

async function results() {
  const x = await context();
  return STEPS.map(st => Object.assign({}, st, { done: (() => { try { return !!st.done(x); } catch (e) { return false; } })() }));
}

// ---------- the alerts step: one recommended path ----------
function alertsExtra(host, done) {
  host.innerHTML = '<div class="gs-sub"><button class="btn primary" data-pon>Turn on notifications on this device</button>' +
    '<button class="btn" data-ptest hidden>Send a test</button>' +
    '<a href="#settings/detection/alerts" data-other>Other ways (Telegram, email, ntfy…)</a></div>' +
    '<div class="hint" data-pmsg></div>';
  const on = host.querySelector('[data-pon]'), test = host.querySelector('[data-ptest]'), msg = host.querySelector('[data-pmsg]');
  const sync = async () => {
    const push = await import('./push.js');
    const why = push.pushUnsupported();
    const mine = !why && await push.thisDeviceOn();
    on.hidden = mine;
    test.hidden = !(mine || done);
    if (why && !done) { on.disabled = true; msg.textContent = why; msg.classList.add('warn'); }
  };
  on.onclick = async () => {
    on.disabled = true; msg.classList.remove('warn'); msg.textContent = '';
    try {
      const push = await import('./push.js');
      await push.enableThisDevice();
      toast('Notifications are on for this device', { kind: 'ok' });
      msg.textContent = 'Done. Send a test to see how it looks.';
    } catch (e) { msg.classList.add('warn'); msg.textContent = e.message; }
    on.disabled = false;
    sync();
    refreshGettingStarted();
  };
  test.onclick = async () => {
    test.disabled = true; msg.classList.remove('warn'); msg.textContent = 'Sending...';
    try {
      const r = await (await import('./push.js')).sendTestPush();
      msg.textContent = r.sent ? 'Sent. It should pop up in a few seconds.' : 'Nothing was sent. Turn on notifications on this device first.';
    } catch (e) { msg.classList.add('warn'); msg.textContent = e.message; }
    test.disabled = false;
  };
  host.querySelector('[data-other]').addEventListener('click', (ev) => {
    const d = host.closest('.dlg'), to = ev.currentTarget.getAttribute('href');
    ev.preventDefault();
    // (The dialog's history step is taken back first, so it does not undo the page change.)
    afterOverlayClose(() => { if (d && d.__close) d.__close(); }).then(() => { location.hash = to; });
  });
  sync();
}

let dlg = null;
export async function openGettingStarted() {
  if (!state.isAdmin) return;
  let r;
  try { r = await results(); } catch (e) { return; }
  const core = r.filter(x => !x.optional), opt = r.filter(x => x.optional);
  const n = core.filter(x => x.done).length;
  const li = (x) => '<li class="' + (x.done ? 'done' : '') + (x.optional ? ' opt' : '') + '" data-step="' + x.id + '"><span class="gs-mark" aria-label="' + (x.done ? 'done' : 'to do') + '">' + (x.done ? '&#10003;' : '') + '</span>' +
    '<div class="gs-text"><b>' + esc(x.title) + '</b>' + (x.optional ? '<span class="gs-opt">Optional</span>' : '') + '<div class="hint">' + esc(x.why) + '</div>' +
    (x.special ? '<div data-extra></div>' : '') + '</div>' +
    (x.special ? '' : '<button class="btn' + (x.done || x.optional ? '' : ' primary') + '" data-go="' + esc(x.where) + '">' + (x.done ? 'Change' : esc(x.label)) + '</button>') + '</li>';
  const html = '<div class="hint">' + n + ' of ' + core.length + ' done. Each step is checked against the real setup.</div>' +
    '<ol class="gs-list">' + core.map(li).join('') + '</ol>' +
    '<div class="hint">Optional</div><ol class="gs-list">' + opt.map(li).join('') + '</ol>' +
    '<div class="gs-foot"><label class="sw"><input type="checkbox" role="switch" data-hide' + (hidden() ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span><span class="sw-text">Don\'t show the reminder on Live again</span></label></div>';
  if (dlg && dlg.isConnected) dlg.__close();
  const d = dlg = openDlg('Getting started', html, true);
  d.classList.add('gsdlg');
  d.querySelectorAll('[data-go]').forEach(b => { b.onclick = () => { afterOverlayClose(() => d.__close()).then(() => go(b.dataset.go)); }; });
  const al = r.find(x => x.special === 'alerts');
  const host = d.querySelector('[data-step="alerts"] [data-extra]');
  if (al && host) alertsExtra(host, al.done);
  d.querySelector('[data-hide]').onchange = (e) => { setHidden(e.target.checked); refreshGettingStarted(); };
}

const hidden = () => {
  if (Prefs.get('gsHide', false) === true) return true;
  try { return localStorage.getItem(HIDE_KEY) === '1'; } catch (e) { return false; }
};
const setHidden = (on) => {
  Prefs.set({ gsHide: on ? true : null });
  try { localStorage.setItem(HIDE_KEY, on ? '1' : '0'); } catch (e) { /* not remembered here */ }
};
const folded = () => { try { return localStorage.getItem('ambs.gsFold') === '1'; } catch (e) { return false; } };
const setFolded = (on) => { try { localStorage.setItem('ambs.gsFold', on ? '1' : '0'); } catch (e) { /* ignore */ } };

// The banner on Live: only for admins, until a camera and alerts are both set up.
export async function refreshGettingStarted() {
  const host = document.querySelector('#tab-live .livemain');
  let b = $('gsBanner');
  const off = () => { if (b) b.remove(); };
  if (!host || !state.isAdmin || hidden() || window.top !== window) return off();
  let r;
  try { r = await results(); } catch (e) { return off(); }
  const core = r.filter(x => !x.optional);
  const left = core.filter(x => !x.done);
  if (r.filter(x => x.id === 'cameras' || x.id === 'alerts').every(x => x.done)) return off();
  if (!b) {
    b = document.createElement('div');
    b.id = 'gsBanner';
    b.className = 'gs-banner';
    host.prepend(b);
  }
  const fold = folded();
  b.dataset.fold = fold ? '1' : '0';
  const next = left[0];
  b.innerHTML = '<span class="gs-sum"><b>Getting started</b>: ' + (core.length - left.length) + ' of ' + core.length + ' done' + (next ? '. Next: ' + esc(next.title) : '') + '</span>' +
    (fold ? '' : '<button class="btn primary" data-gs-open>Show steps</button><button class="btn" data-gs-hide title="Hide this reminder (the steps stay in Help)">Don\'t show again</button>') +
    '<button class="btn icon" data-gs-fold aria-expanded="' + (fold ? 'false' : 'true') + '" title="' + (fold ? 'Show more' : 'Fold to one line') + '" aria-label="' + (fold ? 'Show more' : 'Fold to one line') + '">' + (fold ? '▾' : '▴') + '</button>';
  const open = b.querySelector('[data-gs-open]');
  if (open) open.onclick = openGettingStarted;
  const hide = b.querySelector('[data-gs-hide]');
  if (hide) hide.onclick = () => { setHidden(true); b.remove(); };
  b.querySelector('[data-gs-fold]').onclick = () => { setFolded(!fold); refreshGettingStarted(); };
  if (fold) b.querySelector('.gs-sum').onclick = openGettingStarted;
}

// Links elsewhere (Help) open the checklist.
document.addEventListener('click', (e) => {
  const a = e.target.closest && e.target.closest('[data-gs]');
  if (!a) return;
  e.preventDefault();
  openGettingStarted();
});
