// nav.js - tab registry. Add a page from any module (or a new file imported by main.js):
//
//   registerTab({
//     id: 'health',            // section id is "tab-health" (created in <main> if missing)
//     label: 'Health',
//     order: 80,               // position in the nav (lower = further left)
//     admin: true,             // hidden from view-only accounts
//     noNav: true,             // no nav button (an alias route)
//     redirect() { return { id, hash } },  // alias: showTab sends you to another tab (and hash)
//     init(section) {},        // once, the first time the tab is shown: build the DOM
//     show(section) {},        // every time the tab is shown
//     hide(section) {},        // every time it is left (stop streams/timers here)
//   });
//
// The URL hash (#health) follows the active tab so reload and back/forward work.

import { state, api, backLayer, overlaysChanged, afterOverlayClose, trapSheet, hasUnsaved, confirmLeave } from './core.js';

// Go-to keys (G then letter) per page; shortcuts.js reads this too.
export const GO_KEYS = { live: 'l', events: 'e', clips: 'v', timeline: 't', settings: 's', admin: 'a', help: 'h' };
const touchDevice = () => { try { return matchMedia('(pointer: coarse)').matches; } catch (e) { return false; } };
const tabs = [];
let current = null;

export function registerTab(t) {
  const i = tabs.findIndex(x => x.id === t.id);
  if (i >= 0) tabs.splice(i, 1);
  tabs.push(Object.assign({ order: 100, admin: false, inited: false }, t));
  tabs.sort((a, b) => a.order - b.order);
}
export function activeTab() { return current; }
// The tab id in the URL hash; anything after '?' is that page's own filter.
// "#settings/system/logs": the tab is "settings"; hashPath() is "system/logs".
export function hashTab() { return location.hash.slice(1).split('?')[0].split('/')[0]; }
export function hashPath() { return location.hash.slice(1).split('?')[0].split('/').slice(1).join('/'); }
export function isActive(id) { return current === id; }
export function allTabs() { return tabs.slice(); }

function section(t) {
  let el = document.getElementById('tab-' + t.id);
  if (!el) {
    el = document.createElement('section');
    el.id = 'tab-' + t.id;
    el.hidden = true;
    document.querySelector('main').appendChild(el);
  }
  return el;
}

// The phone's fixed bottom bar: [tab id, label, icon]. "More" opens the sheet below.
const BAR = [['live', 'Live', 'live'], ['events', 'Events', 'alert'], ['settings', 'Settings', 'gear']];
const MORE = [['timelapse', 'Time-lapse', 'timeline'], ['help', 'Help', 'help']];
const svgUse = (n) => '<svg class="ic" aria-hidden="true"><use href="#i-' + n + '"/></svg>';

export function buildNav() {
  const nav = document.getElementById('nav');
  wireNavFade();
  nav.innerHTML = '';
  const coarse = touchDevice();
  for (const t of tabs) {
    if (t.noNav) continue;
    const b = document.createElement('button');
    b.dataset.tab = t.id;
    b.textContent = t.label;
    const g = GO_KEYS[t.id];
    if (g && !coarse) b.title = t.label + ' (G then ' + g.toUpperCase() + ')';
    b.hidden = t.admin && !state.isAdmin;
    if (t.id === 'events') b.insertAdjacentHTML('beforeend', '<span class="nbadge" hidden></span>');
    b.onclick = () => showTab(t.id);
    nav.appendChild(b);
  }
  navFade();
  buildTabBar();
  paintBadge();
}

function buildTabBar() {
  let bar = document.getElementById('tabbar');
  if (!bar) {
    bar = document.createElement('nav');
    bar.id = 'tabbar';
    bar.setAttribute('aria-label', 'Main');
    document.body.appendChild(bar);
  }
  bar.innerHTML = BAR.map(([id, label, icon]) =>
    '<button type="button" data-tab="' + id + '">' + svgUse(icon) + (id === 'events' ? '<span class="nbadge" hidden></span>' : '') + '<span>' + label + '</span></button>').join('') +
    '<button type="button" data-more aria-haspopup="true" aria-expanded="false">' + svgUse('dots') + '<span>More</span></button>';
  bar.onclick = (ev) => {
    const b = ev.target.closest('button');
    if (!b) return;
    if (b.dataset.more !== undefined) { moreOpen() ? closeMore() : openMore(); return; }
    // (The sheet's history step is taken back first, so it does not undo the page change.)
    afterOverlayClose(() => closeMore(true)).then(() => {
      // Live while on Live: the same hooks as the top Live button (leave the timeline / full view).
      if (b.dataset.tab === 'live' && current === 'live') { const top = document.querySelector('#nav button[data-tab="live"]'); if (top) top.click(); }
      else goTab(b.dataset.tab);
    });
  };
  syncBar();
  buildMore();
}

let more = null;
function buildMore() {
  if (more) more.remove();
  more = document.createElement('div');
  more.id = 'moreSheet';
  more.hidden = true;
  more.innerHTML = '<div class="msscrim" data-close></div><div class="mssheet" role="menu" aria-label="More">' +
    MORE.map(([id, label, icon]) => '<button type="button" role="menuitem" data-tab="' + id + '">' + svgUse(icon) + '<span>' + label + '</span></button>').join('') +
    '<button type="button" role="menuitem" data-keys>' + svgUse('keys') + '<span>Keyboard shortcuts</span></button></div>';
  more.onclick = (ev) => {
    const b = ev.target.closest('button,[data-close]');
    if (!b) return;
    const go = b.dataset.tab, keys = b.hasAttribute('data-keys');
    afterOverlayClose(() => closeMore(!!(go || keys))).then(() => {
      if (go) goTab(go);
      else if (keys) document.getElementById('keysBtn').click();
    });
  };
  document.body.appendChild(more);
}
const moreOpen = () => !!more && !more.hidden;
let moreRelease = null;
// Modal while open: the page behind is inert, Tab stays in the sheet, Esc or Back closes it and the
// focus goes back to the More button.
function openMore() {
  more.hidden = false;
  const m = document.querySelector('#tabbar [data-more]');
  if (m) m.setAttribute('aria-expanded', 'true');
  moreRelease = trapSheet(more, more.querySelector('.mssheet'));
  const f = more.querySelector('.mssheet button');
  if (f) f.focus();
  overlaysChanged();
}
// going: true when a page or dialog opens next (the focus is not put back on the More button).
export function closeMore(going) {
  if (!moreOpen()) return;
  more.hidden = true;
  const m = document.querySelector('#tabbar [data-more]');
  if (m) m.setAttribute('aria-expanded', 'false');
  if (moreRelease) { moreRelease(going !== true); moreRelease = null; }
  overlaysChanged();
}
backLayer(45, moreOpen, () => closeMore());
window.addEventListener('keydown', (ev) => { if (ev.key === 'Escape' && moreOpen()) { closeMore(); ev.stopPropagation(); } }, true);

// Active tab on the bottom bar (aria-current), and "More" lit while one of its pages shows.
function syncBar() {
  const bar = document.getElementById('tabbar');
  if (!bar) return;
  for (const b of bar.querySelectorAll('button[data-tab]')) {
    const on = b.dataset.tab === current;
    b.classList.toggle('active', on);
    if (on) b.setAttribute('aria-current', 'page'); else b.removeAttribute('aria-current');
    const t = tabs.find(x => x.id === b.dataset.tab);
    b.hidden = !!(t && t.admin && !state.isAdmin);
  }
  const m = bar.querySelector('[data-more]');
  if (m) m.classList.toggle('active', MORE.some(x => x[0] === current));
}

// ---- unseen events: events newer than the last time this browser opened Events ----
const SEEN_KEY = 'as.events.lastSeen';   // epoch ms; the Events page keeps the same key
let unseen = 0, badgeTimer = 0;
const seenAt = () => { try { const v = parseInt(localStorage.getItem(SEEN_KEY), 10); return v > 0 ? Math.floor(v / 1000) : 0; } catch (e) { return 0; } };
function markSeen() {
  try { localStorage.setItem(SEEN_KEY, String(Date.now())); } catch (e) { /* not remembered */ }
  unseen = 0; paintBadge();
}
function paintBadge() {
  for (const b of document.querySelectorAll('.nbadge')) {
    b.hidden = !unseen;
    b.textContent = !unseen ? '' : unseen > 99 ? '99+' : String(unseen);
  }
  const lab = unseen ? ' (' + unseen + ' new)' : '';
  for (const b of document.querySelectorAll('#tabbar button[data-tab="events"]')) b.setAttribute('aria-label', 'Events' + lab);
}
async function pollBadge() {
  if (!state.signedIn || document.hidden || current === 'events') return;
  const since = seenAt();
  if (!since) { markSeen(); return; }   // first visit on this browser: nothing is "new" yet
  try {
    const r = await api('/api/events?since=' + since + '&limit=100');
    unseen = ((r && r.events) || []).filter(e => e.start > since).length;
    paintBadge();
  } catch (e) { /* keep the last count */ }
}
export function startEventBadge() {
  if (badgeTimer) return;
  badgeTimer = setInterval(pollBadge, 30000);
  setTimeout(pollBadge, 2500);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) pollBadge(); });
}

// Edge fades on the phone's scrolling menu hint at tabs that are cut off.
function navFade() {
  const nav = document.getElementById('nav');
  if (!nav) return;
  nav.classList.toggle('more-l', nav.scrollLeft > 4);
  nav.classList.toggle('more-r', nav.scrollLeft + nav.clientWidth < nav.scrollWidth - 4);
}
let navFadeWired = false;
function wireNavFade() {
  if (navFadeWired) return;
  navFadeWired = true;
  const nav = document.getElementById('nav');
  nav.addEventListener('scroll', navFade, { passive: true });
  window.addEventListener('resize', navFade);
}

export function applyRole() {
  document.body.classList.toggle('viewonly', !state.isAdmin);
  for (const b of document.querySelectorAll('#nav button')) {
    const t = tabs.find(x => x.id === b.dataset.tab);
    b.hidden = !!(t && t.admin && !state.isAdmin);
  }
  const t = tabs.find(x => x.id === current);
  if (t && t.admin && !state.isAdmin) showTab(tabs[0].id);
  navFade();
  syncBar();
}

// Go to a page by id. Unknown or admin-only: ignored.
export function goTab(id) {
  const t = tabs.find(x => x.id === id);
  if (!t || (t.admin && !state.isAdmin)) return;
  if (id === 'settings' && location.hash.startsWith('#settings')) showTab(id, true);   // keep a pasted sub-path
  else showTab(id);
}

export function showTab(id, fromHash) {
  const t = tabs.find(x => x.id === id) || tabs[0];
  if (!t) return;
  // Unsaved fields on the page being left (Settings): ask first; on "keep editing" the address goes back.
  if (current && current !== t.id && hasUnsaved()) {
    const prev = tabs.find(x => x.id === current), keep = prev && prev.leaveHash ? prev.leaveHash() : '';
    confirmLeave().then((ok) => {
      if (ok) showTab(id, fromHash);
      else if (keep && location.hash !== keep) history.replaceState(null, '', keep);
    });
    return;
  }
  if (t.admin && !state.isAdmin) return showTab(tabs[0].id);
  if (t.redirect) { const r = t.redirect(); history.replaceState(null, '', '#' + r.hash); return showTab(r.id, true); }
  if (current && current !== t.id) {
    const prev = tabs.find(x => x.id === current);
    if (prev && prev.hide) prev.hide(section(prev));
    // Events seen while sitting on the page are not new: the badge counts from when Events was left.
    if (current === 'events') markSeen();
  }
  current = t.id;
  document.body.dataset.tab = t.id;   // CSS: page-specific layout (e.g. no Live panel on Settings)
  for (const x of tabs) section(x).hidden = x.id !== t.id;
  for (const b of document.querySelectorAll('#nav button')) {
    const on = b.dataset.tab === t.id;
    b.classList.toggle('active', on);
    if (on) b.setAttribute('aria-current', 'page'); else b.removeAttribute('aria-current');
    if (on && b.scrollIntoView) { try { b.scrollIntoView({ block: 'nearest', inline: 'nearest' }); } catch (e) { /* old browsers */ } }
  }
  syncBar();
  if (t.id === 'events') markSeen();
  const el = section(t);
  if (!t.inited) { t.inited = true; if (t.init) t.init(el); }
  if (t.show) t.show(el);
  if (!fromHash && hashTab() !== t.id) history.replaceState(null, '', '#' + t.id);
}

// Leave the active tab without showing another (used when signing out).
export function hideAllTabs() {
  const t = tabs.find(x => x.id === current);
  if (t && t.hide) t.hide(section(t));
  current = null;
}

window.addEventListener('hashchange', () => {
  let id = hashTab();
  if (id.startsWith('clips')) id = 'clips';   // #clips, #clips?camera=...: the Events grid now
  if (id && id !== current && tabs.some(x => x.id === id)) showTab(id, true);
});
