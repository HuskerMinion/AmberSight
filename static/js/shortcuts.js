// shortcuts.js - keyboard shortcuts, the "?" sheet, and the header theme/shortcut buttons.

import { $, state, openDlg, cycleTheme, topDlgOpen, overlayOpen } from './core.js';
import { showTab, isActive, goTab, GO_KEYS } from './nav.js';
import { timelineKey, enterFromFull, isOn as tlIsOn, toggle as toggleTl, setSolo as tlSolo } from './livetl.js';
import { shieldKey } from './status.js';
import { selectedCam, getSelected, camIdAt, openFullCam, closeFull, fullIsOpen, toggleFullMute, fullSnapshot, fullClip, fullZoom, snapshot, cycleViewStyle, cycleHud, toggleBoxes, dockCam } from './live.js';
import { tvOn, enterTv, exitTv } from './fullscreen.js';

const K = (x) => '<kbd>' + x + '</kbd>';
// Grouped rows: a string is a heading, an array is [keys, what it does].
const KEYS = [
  'Global',
  [K('G') + ' then ' + K('L') + K('E') + K('T') + K('S') + K('H'), 'Go to Live / Events / the timeline / Settings / Help'],
  [K('Ctrl') + ' ' + K('K'), 'Search pages, settings, cameras and actions'],
  [K('/'), 'Search events'],
  [K('S'), 'Disarm motion detection on every camera (asks first); again to arm'],
  [K('Esc'), 'Close dialog / full screen; clear the selection; cancel arranging'],
  [K('?'), 'This sheet'],
  'Live',
  ['Click', 'Select a camera (the side panel switches to it); double-click or ' + K('Enter') + ' opens it full size'],
  [K('1') + ' - ' + K('9'), 'Open camera N full screen (or switch to it)'],
  [K('F'), 'Full screen for the cameras / leave full screen'],
  [K('V'), 'Change layout (Grid, Fit, Focus, Focus 2)'],
  [K('H'), 'Labels on pictures: Clean, Standard, Detailed'],
  [K('B'), 'Motion outlines on / off'],
  [K('T') + ' ' + K('L'), 'Timeline mode on (T also turns it off)'],
  [K('P'), 'Save a snapshot of the highlighted tile'],
  [K('Shift') + ' ' + K('&larr;') + K('&rarr;'), 'Arrange mode: move the focused tile'],
  [K('&larr;') + K('&uarr;') + K('&darr;') + K('&rarr;'), 'Side panel: hold to move a PTZ camera'],
  ['Wheel', 'Over the selected tile: zoom that camera (drag to move around, click the zoom badge, e.g. 2.0&times;, to reset); anywhere else: scroll'],
  'Events (list or grid)',
  [K('&darr;') + K('&uarr;') + ' or ' + K('J') + K('K'), 'Next / previous event (the arrows also move across the grid)'],
  [K('Space'), 'Play / pause the clip'],
  [K('Enter'), 'Open the event on the timeline (Back returns to the same place)'],
  [K('P') + ' ' + K('X') + ' ' + K('O'), 'Keep forever / not a real alert / looks fine'],
  [K('Esc'), 'Close the player on a phone; clear ticked events'],
  'Full view',
  [K('&larr;') + ' ' + K('&rarr;'), 'Previous / next camera'],
  [K('M'), 'Mute / unmute'],
  [K('P'), 'Save a snapshot'],
  [K('C'), 'Record a 20 s clip now'],
  [K('L'), 'Play back this camera\'s recordings'],
  ['Wheel', 'Zoom at the pointer; drag to pan, double-click to zoom in or reset (' + K('+') + ' ' + K('-') + ' ' + K('0') + ' too; pinch on touch)'],
  'Timeline',
  [K('Space') + ' or ' + K('K'), 'Play / pause'],
  [K('J') + ' ' + K('L'), 'Slower (reverse) / faster'],
  [K('&larr;') + ' ' + K('&rarr;'), 'Back / forward 10 s'],
  [K('Shift') + ' ' + K('&larr;') + K('&rarr;'), 'One frame, when paused'],
  [K(',') + ' ' + K('.'), 'Previous / next alert'],
  [K('I') + ' ' + K('O'), 'Mark in / out'],
  [K('E'), 'Export the marked range'],
  [K('End'), 'Jump to live'],
  [K('[') + ' ' + K(']'), 'Playback speed down / up'],
  [K('+') + ' ' + K('-'), 'Zoom the strip'],
  [K('T') + ' or ' + K('Esc'), 'Leave the timeline'],
];

function sheet() {
  openDlg('Keyboard shortcuts', '<div class="keys">' + KEYS.map(k => typeof k === 'string' ? '<h4>' + k + '</h4>' : '<span>' + k[0] + '</span><span>' + k[1] + '</span>').join('') + '</div>');
}

// letter -> page id, for the pages that exist
const GO_KEYS_REV = Object.fromEntries(Object.entries(GO_KEYS).map(([id, ch]) => [ch, id]));
let goPending = 0;

// From an opened camera to full screen of it (F, and the Full screen button): with the timeline
// on, the timeline shows that camera alone in full screen; otherwise the live camera fills it.
export function fullToTv() {
  const id = dockCam();
  closeFull();
  if (tlIsOn()) { tlSolo(id); enterTv(); } else enterTv(id, () => openFullCam(id));
}

export function initShortcuts() {
  $('themeBtn').onclick = cycleTheme;
  $('keysBtn').onclick = sheet;
  window.addEventListener('keydown', (ev) => {
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const tg = ev.target;
    if (tg && (tg.tagName === 'INPUT' || tg.tagName === 'SELECT' || tg.tagName === 'TEXTAREA' || tg.isContentEditable)) return;
    if (!state.signedIn || document.querySelector('#loginGate.on')) return;
    // One spelling of each key: single characters lowercase (Caps Lock safe).
    const k = ev.key.length === 1 ? ev.key.toLowerCase() : ev.key;
    // Behind a dialog only Esc (closes it, handled in core.js) and ? work.
    // (also the zones editor, the camera panel and the command palette)
    if (overlayOpen(ev)) {
      goPending = 0;
      if (k === '?') { sheet(); ev.preventDefault(); }
      return;
    }
    const wasGo = goPending && Date.now() - goPending < 1500;
    goPending = 0;
    if (wasGo && GO_KEYS_REV[k]) { goTab(GO_KEYS_REV[k]); ev.preventDefault(); return; }
    if (k === '?') { sheet(); ev.preventDefault(); return; }
    // The timeline's own keys come first while it is on; handled = nothing else runs.
    const live = isActive('live') && !fullIsOpen();
    if (live && tlIsOn() && timelineKey(ev)) { ev.preventDefault(); return; }
    if (k === 'g' && !ev.shiftKey) { goPending = Date.now(); return; }
    if ((k === 'l' || k === 't') && fullIsOpen()) { enterFromFull(); ev.preventDefault(); return; }
    if (live && (k === 't' || (k === 'l' && !tlIsOn()))) { toggleTl(); ev.preventDefault(); return; }
    if (k === 'Enter' && tg === document.body && isActive('live') && !fullIsOpen() && getSelected() !== null) { openFullCam(getSelected()); ev.preventDefault(); return; }
    if (k === '/') { showTab('events'); setTimeout(() => { const sm = $('fSmart'); if (sm && !$('fSmartL').hidden) sm.focus(); else $('fCam').click(); }, 0); ev.preventDefault(); return; }
    if (k === 's') { if (!ev.repeat) shieldKey(); return; }
    if (/^[1-9]$/.test(k) && (isActive('live') || fullIsOpen())) {
      const id = camIdAt(parseInt(k, 10) - 1);
      if (id !== null) openFullCam(id);
      return;
    }
    if (k === 'v' && live) { cycleViewStyle(); return; }
    if (k === 'h' && isActive('live')) { cycleHud(); return; }
    if (k === 'b' && isActive('live')) { toggleBoxes(); return; }
    if (k === 'f' && isActive('live')) {
      if (tvOn()) exitTv();
      else if (fullIsOpen()) fullToTv();
      else enterTv();
      return;
    }
    if (fullIsOpen()) {
      if (k === 'm') toggleFullMute();
      else if (k === 'p') fullSnapshot();
      else if (k === 'c') fullClip();
      else if (k === '+' || k === '=') fullZoom(1.5);
      else if (k === '-' || k === '_') fullZoom(1 / 1.5);
      else if (k === '0') fullZoom(0);
      return;
    }
    if (k === 'p' && isActive('live')) { const id = selectedCam(); if (id !== null) snapshot(id); }
  });
}
