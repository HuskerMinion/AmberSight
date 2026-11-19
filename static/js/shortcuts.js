// shortcuts.js - keyboard shortcuts, the "?" sheet, and the header theme/shortcut buttons.

import { t } from './i18n.js';
import { $, state, openDlg, cycleTheme, topDlgOpen, overlayOpen, can } from './core.js';
import { showTab, isActive, goTab, GO_KEYS } from './nav.js';
import { timelineKey, enterFromFull, isOn as tlIsOn, toggle as toggleTl, setSolo as tlSolo } from './livetl.js';
import { shieldKey } from './status.js';
import { selectedCam, getSelected, camIdAt, openFullCam, closeFull, fullIsOpen, toggleFullMute, fullSnapshot, fullClip, fullZoom, snapshot, cycleViewStyle, cycleHud, toggleBoxes, dockCam } from './live.js';
import { tvOn, enterTv, exitTv } from './fullscreen.js';

const K = (x) => '<kbd>' + x + '</kbd>';
// Grouped rows: a string is a heading, an array is [keys, what it does].
const KEYS = [
  t('shortcuts.global', 'Global'),
  [K('G') + ' ' + t('shortcuts.then', 'then') + ' ' + K('L') + K('E') + K('T') + K('S') + K('H'), t('shortcuts.go_live', 'Go to Live / Events / the timeline / Settings / Help')],
  [K('Ctrl') + ' ' + K('K'), t('shortcuts.search', 'Search pages, settings, cameras and actions')],
  [K('/'), t('shortcuts.search_events', 'Search events')],
  [K('S'), t('shortcuts.disarm_motion', 'Disarm motion detection on every camera (asks first); again to arm'), 'arm'],
  [K('Esc'), t('shortcuts.close_dialog', 'Close dialog / full screen; clear the selection; cancel arranging')],
  [K('?'), t('shortcuts.this_sheet', 'This sheet')],
  t('shortcuts.live', 'Live'),
  [t('shortcuts.click', 'Click'), t('shortcuts.select_camera', 'Select a camera (the side panel switches to it); double-click or {enter} opens it full size', { enter: K('Enter') })],
  [K('1') + ' - ' + K('9'), t('shortcuts.open_camera_full', 'Open camera N full screen (or switch to it)')],
  [K('F'), t('shortcuts.fullscreen_toggle', 'Full screen for the cameras / leave full screen')],
  [K('V'), t('shortcuts.change_layout', 'Change layout (Grid, Fit, Focus, Focus 2)')],
  [K('H'), t('shortcuts.labels', 'Labels on pictures: Clean, Standard, Detailed')],
  [K('B'), t('shortcuts.motion_outlines', 'Motion outlines on / off (on the timeline: bookmark)')],
  [K('T') + ' ' + K('L'), t('shortcuts.timeline_mode', 'Timeline mode on (T also turns it off)')],
  [K('P'), t('shortcuts.save_snapshot_tile', 'Save a snapshot of the highlighted tile')],
  [K('Shift') + ' ' + K('&larr;') + K('&rarr;'), t('shortcuts.arrange_mode', 'Arrange mode: move the focused tile')],
  [K('&larr;') + K('&uarr;') + K('&darr;') + K('&rarr;'), t('shortcuts.side_panel_ptz', 'Side panel: hold to move a PTZ camera')],
  [t('shortcuts.wheel', 'Wheel'), t('shortcuts.wheel_zoom', 'Over the selected tile: zoom that camera (drag to move around, click the zoom badge, e.g. 2.0&times;, to reset); anywhere else: scroll')],
  t('shortcuts.events_list_or_grid', 'Events (list or grid)'),
  [K('&darr;') + K('&uarr;') + ' ' + t('shortcuts.or', 'or') + ' ' + K('J') + K('K'), t('shortcuts.next_prev_event', 'Next / previous event (the arrows also move across the grid)')],
  [K('Space'), t('shortcuts.play_pause_clip', 'Play / pause the clip')],
  [K('Enter'), t('shortcuts.open_event', 'Open the event on the timeline (Back returns to the same place)')],
  [K('P') + ' ' + K('X') + ' ' + K('O'), t('shortcuts.keep_forever', 'Keep forever / not a real alert / looks fine')],
  [K('Esc'), t('shortcuts.close_player', 'Close the player on a phone; clear ticked events')],
  t('shortcuts.full_view', 'Full view'),
  [K('&larr;') + ' ' + K('&rarr;'), t('shortcuts.prev_next_camera', 'Previous / next camera')],
  [K('M'), t('shortcuts.mute_unmute', 'Mute / unmute')],
  [K('P'), t('shortcuts.save_snapshot', 'Save a snapshot')],
  [K('C'), t('shortcuts.record_clip', 'Record a 20 s clip now')],
  [K('L'), t('shortcuts.playback_recordings', "Play back this camera's recordings")],
  [t('shortcuts.wheel', 'Wheel'), t('shortcuts.wheel_zoom_pointer', 'Zoom at the pointer; drag to pan, double-click to zoom in or reset ({plus} {minus} {zero} too; pinch on touch)', { plus: K('+'), minus: K('-'), zero: K('0') })],
  t('shortcuts.timeline', 'Timeline'),
  [K('Space') + ' ' + t('shortcuts.or', 'or') + ' ' + K('K'), t('shortcuts.play_pause', 'Play / pause')],
  [K('J') + ' ' + K('L'), t('shortcuts.slower_faster', 'Slower (reverse) / faster')],
  [K('&larr;') + ' ' + K('&rarr;'), t('shortcuts.back_forward', 'Back / forward 10 s')],
  [K('Shift') + ' ' + K('&larr;') + K('&rarr;'), t('shortcuts.one_frame', 'One frame, when paused')],
  [K(',') + ' ' + K('.'), t('shortcuts.prev_next_alert', 'Previous / next alert')],
  [K('I') + ' ' + K('O'), t('shortcuts.mark_in_out', 'Mark in / out')],
  [K('E'), t('shortcuts.export_range', 'Export the marked range')],
  [K('B'), t('shortcuts.bookmark_note', 'Bookmark this moment, with a note'), 'events'],
  [K('End'), t('shortcuts.jump_live', 'Jump to live')],
  [K('[') + ' ' + K(']'), t('shortcuts.playback_speed', 'Playback speed down / up')],
  [K('+') + ' ' + K('-'), t('shortcuts.zoom_strip', 'Zoom the strip')],
  [K('T') + ' ' + t('shortcuts.or', 'or') + ' ' + K('Esc'), t('shortcuts.leave_timeline', 'Leave the timeline')],
];

function sheet() {
  // (A row tied to a right is listed only for accounts that have it.)
  openDlg(t('shortcuts.keyboard_shortcuts', 'Keyboard shortcuts'), '<div class="keys">' + KEYS.filter(k => typeof k === 'string' || !k[2] || can(k[2])).map(k => typeof k === 'string' ? '<h4>' + k + '</h4>' : '<span>' + k[0] + '</span><span>' + k[1] + '</span>').join('') + '</div>');
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
