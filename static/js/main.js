// main.js - boot, tab registration, polling timers.
//
// Extending the UI: build a module that calls registerTab() (see nav.js) and import it below.
// Admin-only pages (AI, automation, users, health, logs) are Settings pages: register with
// registerAdminPage() (admin.js) and give them a place in SECTIONS (settings.js).

import { mark } from './diag.js';
import { $, state, bus, api, refreshCameras, closeAllDialogs, openDlg, toast } from './core.js';
import { registerTab, buildNav, showTab, applyRole, hideAllTabs, activeTab, hashTab, allTabs, startEventBadge } from './nav.js';
import { initAuthUi, whoami, applyWho, showGate, hideGate, takePairCode, redeemPairCode, homeScreenHint } from './auth.js';
import { refreshStatus, initStatus, markUnreachable } from './status.js';
import { initServerMenu } from './servers.js';
import { initLive, showLive, hideLive, closeFull, openFullCam, setTool, finishArrange } from './live.js';
import { initEvents, showEvents, hideEvents, refreshEvents, resetEvents } from './events.js';
import { initCameras } from './cameras.js';
import { initSettings, settingsTab } from './settings.js';
import { adminTab } from './admin.js';
import './admin_system.js';
import './status_page.js';
import './ai_review.js';
import './admin_config.js';
import './admin_faces.js';
import './admin_lpr.js';
import './digest.js';
import './admin_smart.js';
import './admin_rules.js';
import './admin_sound.js';
import './admin_offsite.js';
import './admin_presence.js';
import { initLiveTimeline, exit as exitLiveTimeline, enter as enterLiveTimeline } from './livetl.js';
import { initShortcuts } from './shortcuts.js';
import { helpTab } from './help.js';
import { timelapseTab } from './timelapse.js';
import { startSysmon, stopSysmon } from './sysmon.js';
import * as Layouts from './layouts.js';
import * as Prefs from './prefs.js';
import { initDock, applyDockPrefs, resetDock } from './dock.js';
import { isSplit, paneId, paneSite, startSplitPage, mountPaneBar } from './split.js';
import { initWake } from './wake.js';
import { refreshGettingStarted } from './gettingstarted.js';
import { syncPush } from './push.js';
import './palette.js';   // Ctrl/Cmd+K command search (installs itself)

// (The getting-started reminder waits a moment for the camera list.)
registerTab({ id: 'live', label: 'Live', order: 10, show: (el) => { showLive(el); setTimeout(() => refreshGettingStarted().catch(() => {}), 1200); }, hide: hideLive });
registerTab({ id: 'events', label: 'Events', order: 20, show: showEvents, hide: hideEvents });
// Clips is now the Grid view of Events: #clips (any old link or bookmark) goes there. Its camera=<name>
// becomes the Events camera filter (each name encoded on its own, as events.js writes it); id= is dropped.
registerTab({ id: 'clips', label: 'Clips', order: 31, noNav: true,
  redirect: () => {
    const p = new URLSearchParams(location.hash.split('?')[1] || ''), out = ['view=grid'];
    for (const [k, v] of p) {
      if (k === 'camera') { if (v) out.push('cam=' + encodeURIComponent(encodeURIComponent(v))); }
      else if (k !== 'id' && k !== 'view' && k !== 'cam') out.push(encodeURIComponent(k) + '=' + encodeURIComponent(v));
    }
    return { id: 'events', hash: 'events?' + out.join('&') };
  } });
registerTab(settingsTab);
// The old Timeline tab is retired: #timeline and G T open Live with the timeline on.
registerTab({ id: 'timeline', label: 'Timeline', order: 60, noNav: true,
  redirect: () => { setTimeout(() => enterLiveTimeline().catch(() => {}), 0); return { id: 'live', hash: 'live' }; } });
registerTab(Object.assign({}, timelapseTab, { order: 30 }));   // desktop nav: Live, Events, Time-lapse, Settings, Help
registerTab(adminTab);
registerTab(helpTab);

let timersOn = false;
function startTimers() {
  if (timersOn) return;
  timersOn = true;
  const guard = (fn) => () => { if (state.signedIn && !document.hidden) Promise.resolve(fn()).catch(() => {}); };
  // Polling pauses while the browser tab is hidden and catches up the moment it is shown again.
  document.addEventListener('visibilitychange', () => { if (!document.hidden) { guard(refreshStatus)(); guard(refreshCameras)(); guard(refreshEvents)(); } });
  setInterval(guard(refreshStatus), 5000);
  setInterval(guard(refreshCameras), 3000);
  setInterval(guard(refreshEvents), 10000);
}

// The page this user opens on (and the logo returns to). Saved per user.
export function startTab() {
  const id = Prefs.get('startTab', 'live');
  if (id === 'clips') return 'clips';   // the old Clips page: its redirect opens the Events grid
  const t = allTabs().find(x => x.id === id);
  return t && !t.noNav && (!t.admin || state.isAdmin) ? id : 'live';
}

function goHome() {
  // Back to a clean start page: no dialogs, full view, playback timeline, tool or arranging.
  closeAllDialogs();
  closeFull();
  exitLiveTimeline();
  setTool(null);
  finishArrange(false);
  showTab(startTab());
  scrollTo({ top: 0 });
}

// User menu > Start page: pick from the tabs this account can see.
function startPageDlg() {
  const cur = startTab() === 'clips' ? 'events' : startTab();
  const tabs = allTabs().filter(t => !t.noNav && (!t.admin || state.isAdmin));
  const d = openDlg('Start page',
    '<div class="hint">The page AmberSight opens on, and where the AmberSight logo takes you.</div>' +
    '<div class="startlist">' + tabs.map(t => '<label class="asys-chk"><input type="radio" name="startTab" value="' + t.id + '"' + (t.id === cur ? ' checked' : '') + '> ' + t.label + '</label>').join('') + '</div>' +
    '<div class="row"><button class="primary" id="spSave">Save</button><button class="btn" id="spCancel">Cancel</button></div>', true);
  d.querySelector('#spCancel').onclick = () => d.__close();
  d.querySelector('#spSave').onclick = () => {
    const v = d.querySelector('input[name=startTab]:checked');
    if (v) Prefs.set({ startTab: v.value });
    d.__close();
    toast('Start page: ' + (tabs.find(t => t.id === (v && v.value)) || {}).label);
  };
}

function start() {
  state.signedIn = true;
  hideGate();
  buildNav();
  applyRole();
  const linked = hashTab();
  // Pop-out window (right-click a camera > Open in new window): just that camera.
  // (A link from a notification or summary also has cam=, with the camera's name: that is not a pop-out.)
  const startQ = new URLSearchParams(location.search);
  const popCam = startQ.get('alert') !== null ? null : startQ.get('cam');
  if (popCam !== null) document.body.classList.add('popout');
  initServerMenu();
  // A pane of the split view: Live only, slim, with its server picker.
  if (paneId) document.body.classList.add('pane');
  if (paneId && paneSite !== 'here') document.body.classList.add('pane-remote');
  // (The start page waits only for what it draws: the cameras and the layouts.
  // The status line fills in when it comes.)
  refreshStatus().catch(() => {});
  Promise.all([refreshCameras().catch(() => {}), Layouts.init().then(applyDockPrefs).catch(() => {})]).then(() => {
    mark('lists');
    if (paneId) {
      showTab('live', true);
      api('/api/links').then(l => mountPaneBar(state.allCams || [], l)).catch(() => mountPaneBar(state.allCams || [], {}));
      startTimers();
      return;
    }
    showTab(popCam !== null ? 'live' : (linked && allTabs().some(x => x.id === linked) ? linked : startTab()), true);
    // Link from a notification: ?alert=<camera id>&at=<ms> opens that camera at the alert.
    const qp = new URLSearchParams(location.search);
    if (qp.get('alert') !== null && popCam === null) {
      history.replaceState(null, '', location.pathname + location.hash);
      openAlertLink(qp);
    }
    syncPush();
    if (popCam !== null) {
      const id = parseInt(popCam, 10);
      const c = state.cams.find(x => x.id === id);
      if (c) { document.title = (c.short_name || c.name) + ' · AmberSight'; openFullCam(id); }
      else document.body.classList.remove('popout');   // that camera is gone: the normal page, not a blank one
    }
    refreshEvents().catch(() => {}).finally(() => { state.toastsOn = true; });
  });
  startTimers();
  if (state.isAdmin) startSysmon();   // the server figures are for admins
  startEventBadge();
}

// Open what a notification link points at: a camera (at the moment of the
// alert), or a tab (#admin).
function openAlertLink(qp, hash) {
  if (qp.get('alert') !== null) {
    // cam=<name> (newer links) is used when present: ids can change when cameras are added or removed.
    const byName = qp.get('cam') ? state.cams.find(c => c.name === qp.get('cam')) : null;
    const id = byName ? byName.id : parseInt(qp.get('alert'), 10), at = parseInt(qp.get('at'), 10);
    if (state.cams.some(c => c.id === id)) {
      closeAllDialogs(); showTab('live'); openFullCam(id, at ? { alertAt: at } : undefined);
      // An alert from a while ago: show that moment (a very recent one may still be happening, so it stays live).
      if (at && Date.now() - at > 2 * 60 * 1000) {
        import('./livetl.js').then(m => m.enterFromFull()).then(() => toast('Showing the alert from ' + new Date(at).toLocaleString([], { hour: 'numeric', minute: '2-digit', month: 'short', day: 'numeric' }) + '. Close the timeline to go back to live.')).catch(() => {});
      }
    }
    return;
  }
  const tab = (hash || '').replace(/^#/, '').split(/[/?]/)[0];
  if (tab && allTabs().some(t => t.id === tab && (!t.noNav || t.redirect) && (!t.admin || state.isAdmin))) {
    // Settings and Admin read their sub-page from the address bar.
    if ((tab === 'settings' || tab === 'admin') && hash.length > tab.length + 1) history.replaceState(null, '', hash);
    showTab(tab, tab === 'settings' && hash.length > 9);
  }
}

// A tapped notification while this page is open: the notification helper
// (sw.js) asks this page to open it, instead of loading a new page.
if (!paneId && 'serviceWorker' in navigator) {
  navigator.serviceWorker.addEventListener('message', (ev) => {
    if (!ev.data || ev.data.type !== 'ambs-open' || !state.signedIn) return;
    try {
      const u = new URL(ev.data.url, location.origin);
      if (u.origin === location.origin) openAlertLink(u.searchParams, u.hash);
    } catch (e) { /* not a link of ours */ }
  });
}

function stop() {
  state.signedIn = false;
  state.toastsOn = false;
  closeFull();
  stopSysmon();
  Prefs.reset();
  resetDock();
  closeAllDialogs();
  hideAllTabs();
  resetEvents();
  for (const id of ['eventRows', 'clipGrid', 'tlTracks', 'tlPlayers']) { const el = $(id); if (el) el.innerHTML = ''; }
  $('statusText').textContent = '';
}

async function checkSession() {
  const pairMsg = await redeemPairCode();
  let who;
  mark('main');
  try { who = await whoami(); mark('whoami'); } catch (e) {
    $('statusText').textContent = 'server unreachable';
    markUnreachable();
    setTimeout(checkSession, 3000);
    return;
  }
  applyWho(who);
  if (who.auth_required && !who.signed_in) { showGate(pairMsg || ''); return; }
  start();
  homeScreenHint();
}

function boot() {
  // Settings page styles (loaded from here so index.html stays untouched).
  if (!document.querySelector('link[href="/static/css/settings.css"]')) { const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = '/static/css/settings.css'; document.head.appendChild(l); }
  takePairCode();
  const logo = document.querySelector('header .logo');
  logo.onclick = goHome;
  logo.onkeydown = (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); goHome(); } };
  $('menuStart').onclick = () => { $('userDrop').hidden = true; startPageDlg(); };
  $('menuDbl').onclick = async () => { $('userDrop').hidden = true; (await import('./live.js')).dblClickDlg(null); };
  initAuthUi();
  initStatus();
  initDock();
  initLive();
  initLiveTimeline();
  initEvents();
  initCameras();
  initSettings();
  initShortcuts();
  bus.on('signedin', start);
  bus.on('signedout', stop);
  bus.on('ui-updated', () => {
    if (paneId && paneId !== 'a') return;   // (split view: the first pane says it for both)
    toast('AmberSight was updated. Reload this page to use the new version.', { action: { label: 'Reload', run: () => location.reload() }, dismiss: true, ms: 24 * 3600 * 1000 });
  });
  checkSession();
}

// The split view page holds two panes (each one is this app); it runs no app itself.
initWake();
if (isSplit) startSplitPage(); else boot();
