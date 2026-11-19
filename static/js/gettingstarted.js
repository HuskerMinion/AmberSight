// gettingstarted.js - "Getting started" checklist for a new install: the steps that
// matter, each ticked off by looking at the real setup, with a button to the page that
// does it. Admins see a slim banner on Live until a camera AND alerts are set up (or
// they choose "Don't show again"); the full checklist is always in Help.
//
// Steps: Add a camera, Get alerts on your phone, Add a password, Choose where recordings
// go; then optional: Use HTTPS, Turn on smart detection (AI). A test or demo camera never
// counts. "Don't show again" is kept on the server with the other interface prefs.

import { t } from './i18n.js';
import { mountLangStep } from './lang_ui.js';
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
  { id: 'cameras', title: t('gettingstarted.add_camera', 'Add a camera'), why: t('gettingstarted.camera_why', 'Find the cameras on your network, or add one by its address.'), where: '#settings/cameras/find', label: t('gettingstarted.find_cameras', 'Find cameras'),
    done: (x) => x.cams.length > 0 },
  { id: 'alerts', title: t('gettingstarted.get_alerts','Get alerts on your phone'), why: t('gettingstarted.alerts_why','A message with a picture when something moves at your door.'), where: '#settings/detection/alerts', label: t('gettingstarted.set_up','Set up'), special: 'alerts',
    done: (x) => (x.s.alert_channels || []).some(c => c.configured) || x.pushDevs > 0 },
  { id: 'accounts', title: t('gettingstarted.add_password','Add a password'), why: t('gettingstarted.password_why','Without a sign-in account, anyone who can reach this computer can watch and change everything.'), where: '#settings/users/users', label: t('gettingstarted.add_account','Add account'),
    done: (x) => x.users.length > 0 },
  { id: 'storage', title: t('gettingstarted.choose_recordings','Choose where recordings go'), why: t('gettingstarted.recordings_why','Pick the drive for recordings and how much of it they may use. The default uses what the drive can spare.'), where: '#settings/recording/drives', label: t('gettingstarted.choose','Choose'),
    done: (x) => (x.s.storage.folders || []).length > 0 || x.s.storage.max_mb !== OLD_DEFAULT_MAX_MB },
  { id: 'https', title: t('gettingstarted.use_https','Use HTTPS'), why: t('gettingstarted.https_why','Keeps the connection private, and phones and browsers only allow notifications and some other features over it.'), where: 'trust', label: t('gettingstarted.set_up_https','Set up'), optional: true,
    done: () => location.protocol === 'https:' },
  { id: 'ai', title: t('gettingstarted.turn_on_ai','Turn on smart detection (AI)'), why: t('gettingstarted.ai_why','Alerts only when a person, car or animal is seen: far fewer false alarms from shadows, rain and bugs.'), where: '#settings/detection/objects', label: t('gettingstarted.turn_on','Turn on'), optional: true,
    done: (x) => !!x.s.ai.enabled && (!!x.s.ai_status.builtin_loaded || !!(x.s.ai.url || '').trim()) },
];

async function results() {
  const x = await context();
  return STEPS.map(st => Object.assign({}, st, { done: (() => { try { return !!st.done(x); } catch (e) { return false; } })() }));
}

// ---------- the alerts step: one recommended path ----------
function alertsExtra(host, done) {
  host.innerHTML = '<div class="gs-sub"><button class="btn primary" data-pon>' + t('gettingstarted.turn_on_notif', 'Turn on notifications on this device') + '</button>' +
    '<button class="btn" data-ptest hidden>' + t('gettingstarted.send_test', 'Send a test') + '</button>' +
    '<a href="#settings/detection/alerts" data-other>' + t('gettingstarted.other_ways', 'Other ways (Telegram, email, ntfy…)') + '</a></div>' +
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
      toast(t('gettingstarted.notif_on','Notifications are on for this device'), { kind: 'ok' });
      msg.textContent = t('gettingstarted.done_test','Done. Send a test to see how it looks.');
    } catch (e) { msg.classList.add('warn'); msg.textContent = e.message; }
    on.disabled = false;
    sync();
    refreshGettingStarted();
  };
  test.onclick = async () => {
    test.disabled = true; msg.classList.remove('warn'); msg.textContent = t('gettingstarted.sending','Sending...');
    try {
      const r = await (await import('./push.js')).sendTestPush();
      msg.textContent = r.sent ? t('gettingstarted.sent','Sent. It should pop up in a few seconds.') : t('gettingstarted.nothing_sent','Nothing was sent. Turn on notifications on this device first.');
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
  const li = (x) => '<li class="' + (x.done ? 'done' : '') + (x.optional ? ' opt' : '') + '" data-step="' + x.id + '"><span class="gs-mark" aria-label="' + (x.done ? t('gettingstarted.done', 'done') : t('gettingstarted.to_do', 'to do')) + '">' + (x.done ? '&#10003;' : '') + '</span>' +
    '<div class="gs-text"><b>' + esc(x.title) + '</b>' + (x.optional ? '<span class="gs-opt">' + t('gettingstarted.optional','Optional') + '</span>' : '') + '<div class="hint">' + esc(x.why) + '</div>' +
    (x.special ? '<div data-extra></div>' : '') + '</div>' +
    (x.special ? '' : '<button class="btn' + (x.done || x.optional ? '' : ' primary') + '" data-go="' + esc(x.where) + '">' + (x.done ? t('gettingstarted.change','Change') : esc(x.label)) + '</button>') + '</li>';
  const html = '<div data-langstep></div><div class="hint">' + t('gettingstarted.n_done', '{n} of {total} done. Each step is checked against the real setup.', { n, total: core.length }) + '</div>' +
    '<ol class="gs-list">' + core.map(li).join('') + '</ol>' +
    '<div class="hint">' + t('gettingstarted.optional', 'Optional') + '</div><ol class="gs-list">' + opt.map(li).join('') + '</ol>' +
    '<div class="gs-foot"><label class="sw"><input type="checkbox" role="switch" data-hide' + (hidden() ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span><span class="sw-text">' + t('gettingstarted.dont_show','Don\'t show the reminder on Live again') + '</span></label></div>';
  if (dlg && dlg.isConnected) dlg.__close();
  const d = dlg = openDlg(t('gettingstarted.title', 'Getting started'), html, true);
  d.classList.add('gsdlg');
  mountLangStep(d.querySelector('[data-langstep]'));
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
  // Another call may have added it while this one waited (two at once when
  // the app opens from a notification link).
  b = $('gsBanner');
  if (!state.isAdmin || hidden()) return off();
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
  b.innerHTML = '<span class="gs-sum">' + (next ? t('gettingstarted.sum_next', '<b>Getting started</b>: {completed} of {total} done. Next: {next}', { completed: core.length - left.length, total: core.length, next: esc(next.title) }) : t('gettingstarted.sum', '<b>Getting started</b>: {completed} of {total} done', { completed: core.length - left.length, total: core.length })) + '</span>' +
    (fold ? '' : '<button class="btn primary" data-gs-open>' + t('gettingstarted.show_steps', 'Show steps') + '</button><button class="btn" data-gs-hide title="' + esc(t('gettingstarted.hide_reminder', 'Hide this reminder (the steps stay in Help)')) + '">' + t('gettingstarted.dont_show_again', 'Don\'t show again') + '</button>') +
    '<button class="btn icon" data-gs-fold aria-expanded="' + (fold ? 'false' : 'true') + '" title="' + (fold ? t('gettingstarted.show_more', 'Show more') : t('gettingstarted.fold', 'Fold to one line')) + '" aria-label="' + (fold ? t('gettingstarted.show_more', 'Show more') : t('gettingstarted.fold', 'Fold to one line')) + '">' + (fold ? '▾' : '▴') + '</button>';
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
