// dock.js - the Live page's side dock: profile, group, grid quality, the selected camera's
// controls (snapshot, clip, full size, talk, PTZ, click-to-aim, zone drawing) and the
// server status rows. Left or right (saved in prefs). How it shows follows the device
// (device.js): pinned beside the page on a desktop (collapsible to a thin strip), a
// slide-over drawer from its side on a tablet or a phone on its side, a bottom sheet on
// a phone held upright. The drawer and the sheet close with their X, Esc, Back, a tap
// outside or a swipe towards their edge.

import { t } from './i18n.js';
import { $, state, bus, api, esc, ic, toast, msgDlg, camFromId, backLayer, overlaysChanged, can } from './core.js';
import { dockMode, onDeviceChange } from './device.js';
import * as Prefs from './prefs.js';
import * as Layouts from './layouts.js';
import * as Live from './live.js';
import { hostIn } from './sysmon.js';
import { enterFromFull } from './livetl.js';
import { openClipsFor } from './events.js';
import { hasControls, mountPanel } from './controls.js';
let ctlStop = null;               // the lights-and-siren panel's countdown

// The panel slides over the page (drawer or sheet) instead of sitting beside it.
const overlayDock = () => dockMode() !== 'pinned';
// collapsed: null = the choice was never made (collapsed while there are few cameras).
let dk = { side: 'left', collapsed: null };
const isCollapsed = () => (dk.collapsed === null ? (state.cams || []).length < 4 : !!dk.collapsed);
let liveVisible = false;
let sheetOpen = false;
const capsCache = new Map();      // camera id -> { at, data, pending }
let prof = null;                  // last /api/profiles
let holdFor = null;               // profile name waiting for a hold duration
let presetOpen = false, presetName = '';
let talk = null;                  // { id, stop() }

// ---------- build ----------
export function initDock() {
  const dock = $('dock');
  dock.dataset.label = t('index.view', 'View');   // the collapsed dock's side label (CSS ::after)
  dock.innerHTML =
    '<button class="dk-handle" id="dkHandle" type="button"></button>' +
    '<div class="dk-body">' +
      '<div class="dk-head"><span class="dk-title">' + t('dock.title_view', 'View') + '</span>' +
        '<button class="dk-ibtn" id="dkSide" type="button" title="' + t('dock.move_side', 'Move this sidebar to the other side') + '" aria-label="' + t('dock.move_side', 'Move this sidebar to the other side') + '">' + ic('swap') + '</button>' +
        '<button class="dk-ibtn dk-phone" id="dkClose" type="button" title="' + t('dock.close', 'Close') + '" aria-label="' + t('dock.close', 'Close') + '">' + ic('x') + '</button></div>' +
      '<section class="dk-sec" id="dkProfile" hidden><h4>' + t('dock.profile', 'Profile') + '</h4>' +
        '<select id="dkProf" aria-label="' + t('dock.profile', 'Profile') + '"></select>' +
        '<div class="dk-hold" id="dkHold" hidden></div>' +
        '<div class="dk-note" id="dkProfNote"></div></section>' +
      '<section class="dk-sec dk-live" id="dkGroup" hidden><h4>' + t('dock.group', 'Group') + '</h4>' +
        '<select id="groupFilter" aria-label="' + t('dock.group', 'Group') + '"><option value="">' + t('dock.all_groups', 'All groups') + '</option></select></section>' +
      '<section class="dk-sec dk-live" id="dkQuality"><h4>' + t('dock.picture', 'Picture') + '</h4>' +
        '<div class="seg" role="group" aria-label="' + t('dock.picture', 'Picture') + '" title="' + t('dock.picture_title', 'Quick: lower delay, a lighter picture (about 5 frames a second per tile). Smooth: a steadier picture at full speed, but heavier.') + '">' +
          '<button type="button" data-q="light" title="' + t('dock.quick_tip', 'Quick: lower delay and lighter on your browser and the server (about 5 frames a second per tile)') + '">' + t('dock.quick', 'Quick') + '</button>' +
          '<button type="button" data-q="smooth" title="' + t('dock.smooth_tip', 'Smooth: a steadier picture at full speed (heavier, slightly more delay)') + '">' + t('dock.smooth', 'Smooth') + '</button></div>' +
        '<h4 class="dk-sub">' + t('dock.labels_on_pictures', 'Labels on pictures') + '</h4>' +
        '<div class="seg" role="group" aria-label="' + t('dock.labels_on_pictures_h', 'Labels on pictures (H)') + '" title="' + t('dock.labels_title', 'How much text sits on the pictures. H cycles.') + '">' +
          '<button type="button" data-hud="clean" title="' + t('dock.clean_tip', 'Clean: only the alert ring; the name shows when you point at a picture') + '">' + t('dock.clean', 'Clean') + '</button>' +
          '<button type="button" data-hud="standard" title="' + t('dock.standard_tip', 'Standard: name, recording and clock') + '">' + t('dock.standard', 'Standard') + '</button>' +
          '<button type="button" data-hud="detailed" title="' + t('dock.detailed_tip', 'Detailed: also frames per second, bitrate, size and the AI label with its score') + '">' + t('dock.detailed', 'Detailed') + '</button></div>' +
        '<h4 class="dk-sub">' + t('dock.motion_outlines', 'Motion outlines') + '</h4>' +
        '<div class="seg" role="group" aria-label="' + t('dock.motion_outlines_label', 'Motion outlines') + '">' +
          '<button type="button" data-b="off" title="' + t('dock.off_tip', 'Do not draw what the motion detector sees') + '">' + t('dock.off', 'Off') + '</button>' +
          '<button type="button" data-b="motion" title="' + t('dock.on_motion_tip', 'Draw outlines around what the motion detector sees moving (useful while tuning zones and sensitivity)') + '">' + t('dock.on_motion', 'On motion') + '</button></div></section>' +
      '<section class="dk-sec dk-live" id="dkCam" hidden></section>' +
      '<section class="dk-sec dk-status" id="dkStatus" hidden><details class="dk-fold" id="dkFold"><summary>' + t('dock.server_figures', 'Server figures') + '</summary><div id="dockStatus"></div></details></section>' +
    '</div>';

  $('dkHandle').onclick = () => { if (overlayDock()) setSheet(false); else setCollapsed(!isCollapsed()); };
  $('dkSide').onclick = () => { dk.side = dk.side === 'left' ? 'right' : 'left'; saveDock(); applyDock(); };
  $('dkClose').onclick = () => setSheet(false);
  $('dockOpen').onclick = () => setSheet(!sheetOpen);
  $('fullPanel').onclick = () => setSheet(!sheetOpen);
  $('dockScrim').onclick = () => setSheet(false);
  initSwipe(dock);
  for (const b of dock.querySelectorAll('[data-q]')) b.onclick = () => Layouts.setPrefs({ quality: b.dataset.q });
  for (const b of dock.querySelectorAll('[data-hud]')) b.onclick = () => Live.setHud(b.dataset.hud, true);
  bus.on('hud', syncQuality);
  for (const b of dock.querySelectorAll('[data-b]')) b.onclick = () => Layouts.setPrefs({ boxes: b.dataset.b });
  $('dkProf').onchange = onProfileChange;
  $('dkHold').onclick = onHoldClick;
  const cam = $('dkCam');
  cam.addEventListener('click', onCamClick);
  cam.addEventListener('change', onCamChange);
  cam.addEventListener('input', (ev) => { if (ev.target.id === 'dkPresetName') presetName = ev.target.value; });
  cam.addEventListener('keydown', (ev) => {
    if (ev.target.id === 'dkPresetName') {
      if (ev.key === 'Enter') { savePreset(); ev.preventDefault(); }
      else if (ev.key === 'Escape') { presetOpen = false; presetName = ''; renderCam(true); ev.preventDefault(); ev.stopPropagation(); }
    }
  });
  cam.addEventListener('pointerdown', onPadDown);
  dock.addEventListener('keydown', onDockKey);
  dock.addEventListener('keyup', onDockKeyUp);
  window.addEventListener('blur', () => endMove());
  // (Capture, and stop it there: Esc closes the panel only, not the full view under it.)
  window.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Escape' || !sheetOpen || ev.defaultPrevented || document.querySelector('.dlg, .ze, .ctxmenu, .cp-scrim:not([hidden])')) return;
    setSheet(false); ev.preventDefault(); ev.stopImmediatePropagation();
  }, true);
  onDeviceChange(() => { sheetOpen = false; applyDock(); overlaysChanged(); });
  backLayer(22, () => sheetOpen, () => setSheet(false));

  bus.on('select', () => { presetOpen = false; presetName = ''; stopTalkIfOther(); renderCam(); });
  bus.on('full', () => { stopTalkIfOther(); renderCam(); });
  bus.on('tool', () => renderCam());
  bus.on('cameras', () => { syncGroup(); renderCam(); if (dk.collapsed === null) applyDock(); });
  bus.on('layouts-prefs', syncQuality);
  bus.on('live-visible', (v) => {
    liveVisible = v;
    // The dock shows on every tab; its camera sections only on Live.
    $('dock').dataset.live = v ? '1' : '0';
    if (v) renderCam(); else { setSheet(false); endMove(); }
    loadProfiles();
    syncHost();
  });
  setInterval(() => { if (state.signedIn && !document.hidden) loadProfiles(); }, 15000);
  applyDock();
  syncQuality();
}

// Call after sign-in once prefs are loaded.
export function applyDockPrefs() {
  const d = Prefs.get('dock', {}) || {};
  dk = { side: d.side === 'right' ? 'right' : 'left', collapsed: typeof d.collapsed === 'boolean' ? d.collapsed : null };
  $('dkStatus').hidden = !state.isAdmin;
  applyDock();
  syncQuality();
  syncGroup();
}

export function resetDock() {
  prof = null; holdFor = null; capsCache.clear(); endMove(); stopTalk();
  $('dkCam').hidden = true; $('dkCam').dataset.key = '';
}

function saveDock() { Prefs.set({ dock: { side: dk.side, collapsed: dk.collapsed === null ? undefined : dk.collapsed } }); }
function setCollapsed(c) { dk.collapsed = c; saveDock(); applyDock(); }

let sheetOpener = null;
function setSheet(open) {
  open = !!open && overlayDock();
  if (open === sheetOpen) return;
  sheetOpen = open;
  applyDock();
  overlaysChanged();
  if (open) {
    sheetOpener = document.activeElement;
    try { $('dkClose').focus({ preventScroll: true }); } catch (e) { /* not focusable */ }
  } else if (sheetOpener && sheetOpener.isConnected && $('dock').contains(document.activeElement)) {
    try { sheetOpener.focus({ preventScroll: true }); } catch (e) { /* gone */ }
  }
}

// A swipe towards the panel's edge closes the drawer (sideways) or the sheet (down).
// Not from the PTZ pad, sliders or text fields (they use the drag themselves).
function initSwipe(dock) {
  let st = null;
  dock.addEventListener('touchstart', (ev) => {
    if (!sheetOpen || ev.touches.length !== 1 || ev.target.closest('.dpad, .zoomcol, input, select, textarea, .sm-spark')) { st = null; return; }
    const t = ev.touches[0];
    st = { x: t.clientX, y: t.clientY, top: dock.querySelector('.dk-body').scrollTop };
  }, { passive: true });
  dock.addEventListener('touchend', (ev) => {
    if (!st || !sheetOpen) return;
    const t = ev.changedTouches[0];
    const dx = t.clientX - st.x, dy = t.clientY - st.y;
    const mode = dockMode();
    if (mode === 'drawer') {
      const out = dk.side === 'left' ? -dx : dx;
      if (out > 60 && Math.abs(dx) > 1.5 * Math.abs(dy)) setSheet(false);
    } else if (mode === 'sheet') {
      // (Only when the sheet's content is scrolled to the top: otherwise it is a scroll.)
      if (dy > 70 && Math.abs(dy) > 1.5 * Math.abs(dx) && st.top <= 0) setSheet(false);
    }
    st = null;
  }, { passive: true });
  dock.addEventListener('touchcancel', () => { st = null; }, { passive: true });
}

function applyDock() {
  const wrap = $('liveWrap'), dock = $('dock');
  const phone = overlayDock();
  wrap.dataset.side = dk.side;
  const col = isCollapsed();
  wrap.dataset.collapsed = !phone && col ? '1' : '0';
  document.body.dataset.dockSide = dk.side;
  document.body.dataset.dockCollapsed = !phone && col ? '1' : '0';
  dock.classList.toggle('open', sheetOpen);
  if (phone) { dock.setAttribute('role', 'dialog'); dock.setAttribute('aria-modal', 'true'); }
  else { dock.removeAttribute('role'); dock.removeAttribute('aria-modal'); }
  // (Closed, the drawer or sheet is off screen: out of the tab order too.)
  dock.inert = phone && !sheetOpen;
  $('dockScrim').hidden = !sheetOpen;
  const h = $('dkHandle');
  if (phone) {
    h.innerHTML = '<span class="dk-grip" aria-hidden="true"></span>';
    h.title = h.ariaLabel = t('dock.close','Close');
    h.removeAttribute('aria-expanded');
  } else {
    const towardsEdge = dk.side === 'left' ? 'left' : 'right';
    const away = dk.side === 'left' ? 'right' : 'left';
    h.innerHTML = ic(col ? away : towardsEdge);
    h.title = col ? t('dock.show_sidebar','Show the sidebar') : t('dock.hide_sidebar','Hide the sidebar');
    h.setAttribute('aria-label', h.title);
    h.setAttribute('aria-expanded', col ? 'false' : 'true');
  }
  $('dkSide').title = t('dock.move_sidebar', 'Move the sidebar to the {side}', { side: dk.side === 'left' ? t('dock.right', 'right') : t('dock.left', 'left') });
  syncHost();
}

function syncHost() {
  // Desktop: the status lives in the dock on every tab while it is open.
  // Tablets and phones: only while the drawer / sheet (opened from Live) is up.
  const want = state.isAdmin && (overlayDock() ? (liveVisible && sheetOpen) : !isCollapsed());
  hostIn(want ? $('dockStatus') : null);
}

// ---------- quality / group ----------
function syncQuality() {
  for (const b of $('dock').querySelectorAll('[data-hud]')) { const on = b.dataset.hud === Live.hudLevel(); b.classList.toggle('on', on); b.setAttribute('aria-pressed', on ? 'true' : 'false'); }
  const { quality: q, boxes } = Layouts.getPrefs();
  for (const b of $('dock').querySelectorAll('[data-q]')) {
    const on = b.dataset.q === q;
    b.classList.toggle('on', on);
    b.setAttribute('aria-pressed', on ? 'true' : 'false');
  }
  for (const b of $('dock').querySelectorAll('[data-b]')) {
    const on = b.dataset.b === boxes;
    b.classList.toggle('on', on);
    b.setAttribute('aria-pressed', on ? 'true' : 'false');
  }
}
function syncGroup() {
  const groups = new Set(state.cams.flatMap(c => (c.groups && c.groups.length) ? c.groups : ['Ungrouped']));
  $('dkGroup').hidden = groups.size < 2;
}

// ---------- profile ----------
async function loadProfiles() {
  try { prof = await api('/api/profiles'); } catch (e) { return; }
  renderProfile();
}

function fmtUntil(epoch) {
  const d = new Date(epoch * 1000);
  const tm = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  return d.toDateString() === new Date().toDateString() ? tm : d.toLocaleDateString([], { weekday: 'short' }) + ' ' + tm;
}

function renderProfile() {
  const sec = $('dkProfile');
  if (!prof || !Array.isArray(prof.profiles) || !prof.profiles.length) { sec.hidden = true; return; }
  sec.hidden = false;
  const sel = $('dkProf');
  const cur = prof.manual ? prof.manual.name : '';
  const html = '<option value="">' + t('dock.auto_schedule', 'Auto (schedule: {name})', { name: esc(prof.scheduled || t('dock.none', 'none')) }) + '</option>' +
    prof.profiles.map(n => '<option value="' + esc(n) + '">' + esc(n) + '</option>').join('');
  if (sel.dataset.h !== html) { sel.innerHTML = html; sel.dataset.h = html; }
  if (holdFor === null) sel.value = cur;
  sel.disabled = !can('arm');
  sel.title = can('arm') ? t('dock.switch_profile','Switch the active profile') : t('dock.cannot_change_profile','This account cannot change the profile');
  let note;
  if (prof.manual) note = prof.manual.until ? t('dock.held_until', 'Held: {name} until {time}', { name: esc(prof.manual.name), time: fmtUntil(prof.manual.until) }) : t('dock.held_changed', 'Held: {name} until changed', { name: esc(prof.manual.name) });
  else note = prof.active ? t('dock.follow_schedule_colon', 'Following the schedule: {name}', { name: esc(prof.active) }) : t('dock.follow_schedule', 'Following the schedule');
  $('dkProfNote').innerHTML = note;
}

function onProfileChange() {
  const v = $('dkProf').value;
  if (!v) { holdFor = null; renderHold(); postProfile(null, null); return; }
  holdFor = v;
  renderHold();
}
function renderHold() {
  const h = $('dkHold');
  if (holdFor === null) { h.hidden = true; h.innerHTML = ''; return; }
  h.hidden = false;
  h.innerHTML = '<div class="dk-note">' + t('dock.hold_note', 'Hold {name} for', { name: esc(holdFor) }) + '</div><div class="seg">' +
    '<button type="button" data-m="60">' + t('dock.hours_1', '1 h') + '</button><button type="button" data-m="240">' + t('dock.hours_4', '4 h') + '</button>' +
    '<button type="button" data-m="0">' + t('dock.until_changed_button', 'Until changed') + '</button></div>' +
    '<button type="button" class="dk-link" data-m="cancel">' + t('dock.cancel', 'Cancel') + '</button>';
  h.querySelector('button').focus();
}
function onHoldClick(ev) {
  const b = ev.target.closest('[data-m]');
  if (!b) return;
  const name = holdFor;
  holdFor = null;
  renderHold();
  if (b.dataset.m === 'cancel') { renderProfile(); return; }
  const m = parseInt(b.dataset.m, 10);
  postProfile(name, m > 0 ? m : null);
}
async function postProfile(name, minutes) {
  try { prof = Object.assign({}, prof, await api('/api/profile', { json: { name, minutes } })); }
  catch (e) { msgDlg(e.message); }
  loadProfiles();
}

// ---------- selected camera ----------
function ptzCaps(c) {
  if (c.ptz === false) return null;
  const e = capsCache.get(c.id);
  const fresh = e && Date.now() - e.at < 60000;
  if (!fresh && !(e && e.pending)) fetchCaps(c.id);
  return e && e.data ? e.data : null;
}
async function fetchCaps(id, force) {
  const prev = capsCache.get(id);
  capsCache.set(id, { at: Date.now(), data: prev && prev.data, pending: true });
  let data;
  try { data = await api('/api/cameras/' + id + '/ptz/caps' + (force ? '?refresh=1' : '')); }
  catch (e) { data = { ptz: false, error: e.message }; }
  capsCache.set(id, { at: Date.now(), data });
  if (Live.dockCam() === id) renderCam();
}

function padBtn(mv, icon, label, dis, cls) {
  return '<button type="button" class="pad-b' + (cls ? ' ' + cls : '') + '" data-mv="' + mv + '" aria-label="' + label + '" title="' + label + ' ' + t('dock.hold_suffix', '(hold)') + '"' + (dis ? ' disabled' : '') + '>' + ic(icon) + '</button>';
}

function renderCam(force) {
  const sec = $('dkCam');
  const id = Live.dockCam();
  const c = id === null ? null : camFromId(id);
  if (!c) { if (ctlStop) { ctlStop(); ctlStop = null; } sec.hidden = true; sec.dataset.key = ''; return; }
  const full = Live.fullIsOpen();
  const caps = ptzCaps(c);
  const tool = Live.getTool();
  const admin = state.isAdmin;
  // (Operators: what their rights allow. The server checks again.)
  const mayClip = can('talk_clip'), mayPtz = can('ptz'), mayZones = can('camera_detection');
  const secure = window.isSecureContext;
  const talking = !!(talk && talk.id === c.id);
  const ctl = hasControls(c);
  const key = JSON.stringify([c.id, c.name, c.online, full, tool, admin, mayClip, mayPtz, mayZones, secure, caps, talking, presetOpen, ctl]);
  if (!force && key === sec.dataset.key) return;
  sec.dataset.key = key;
  sec.hidden = false;
  const ptzOn = !!(caps && caps.ptz);
  let h = '<h4>' + t('dock.camera', 'Camera') + '</h4><div class="dk-camname"><strong>' + esc(c.remote ? (c.short_name || c.remote.camera) : (c.short_name || c.name)) + '</strong>' +
    (c.remote ? '<span class="cam-site">' + esc(c.remote.site) + '</span>' : '') +
    '<span class="badge dot ' + (c.online ? 'on' : 'off') + '">' + (c.online ? t('dock.online', 'Online') : t('dock.offline', 'Offline')) + '</span></div>' +
    '<div class="dk-btns">' +
      '<button type="button" class="btn" data-act="snap" title="' + t('dock.save_snapshot','Save a snapshot (P)') + '">' + ic('camera') + '<span>' + t('dock.snapshot','Snapshot') + '</span></button>' +
      // Another server's camera: its recordings and clips are there.
      (c.remote ? '' :
      '<button type="button" class="btn" data-act="clip" title="' + (mayClip ? t('dock.record_clip','Record a 20 s clip now (C)') : t('dock.cannot_record','This account cannot record clips')) + '"' + (mayClip ? '' : ' disabled') + '>' + ic('rec') + '<span>' + t('dock.clip', 'Clip') + '</span></button>' +
      '<button type="button" class="btn" data-act="tl" title="' + t('dock.playback','Play back this camera\'s recordings (L in full size)') + '">' + ic('timeline') + '<span>' + t('dock.timeline','Timeline') + '</span></button>' +
      '<button type="button" class="btn" data-act="clips" title="' + t('dock.clips_from','Clips from this camera') + '">' + ic('play') + '<span>' + t('dock.clips', 'Clips') + '</span></button>') +
      (full ? '' : '<button type="button" class="btn" data-act="full" title="' + t('dock.open_full','Open full size (Enter)') + '">' + ic('expand') + '<span>' + t('dock.full_size','Full size') + '</span></button>') +
      (secure && mayClip && !c.remote ? '<button type="button" class="btn' + (talking ? ' on' : '') + '" data-act="talk" aria-pressed="' + talking + '" title="' + t('dock.talk','Talk through the camera speaker (needs a microphone)') + '">' + ic('mic') + '<span>' + (talking ? t('dock.stop_talking','Stop talking') : t('dock.talk_btn','Talk')) + '</span></button>' : '') +
    '</div>';
  if (ptzOn) {
    const dis = !mayPtz;
    h += '<div class="dk-lbl">' + t('dock.pan_tilt_zoom', 'Pan / tilt / zoom') + '</div><div class="dk-ptz" id="dkPtz" data-ptz="1">' +
      '<div class="dpad" tabindex="0" role="group" aria-label="' + t('dock.pan_tilt_label','Pan and tilt. Hold an arrow key while this is focused.') + '">' +
        padBtn('-1,1,0', 'up', t('dock.pad_up_left', 'Up and left'), dis, 'diag ul') + padBtn('0,1,0', 'up', t('dock.pad_tilt_up', 'Tilt up'), dis) + padBtn('1,1,0', 'up', t('dock.pad_up_right', 'Up and right'), dis, 'diag ur') +
        padBtn('-1,0,0', 'left', t('dock.pad_pan_left', 'Pan left'), dis) +
        '<button type="button" class="pad-b pad-stop" data-act="ptzstop" aria-label="' + t('dock.stop', 'Stop') + '" title="' + t('dock.stop', 'Stop') + '"' + (dis ? ' disabled' : '') + '><i></i></button>' +
        padBtn('1,0,0', 'right', t('dock.pad_pan_right', 'Pan right'), dis) +
        padBtn('-1,-1,0', 'down', t('dock.pad_down_left', 'Down and left'), dis, 'diag dl') + padBtn('0,-1,0', 'down', t('dock.pad_tilt_down', 'Tilt down'), dis) + padBtn('1,-1,0', 'down', t('dock.pad_down_right', 'Down and right'), dis, 'diag dr') + '</div>' +
      (caps.zoom ? '<div class="zoomcol">' + padBtn('0,0,1', 'plus', t('dock.zoom_in', 'Zoom in'), dis) + padBtn('0,0,-1', 'minus', t('dock.zoom_out', 'Zoom out'), dis) + '</div>' : '') +
      '</div>';
    if (dis) h += '<div class="dk-note">' + (state.role === 'operator' ? t('dock.cannot_move','This account cannot move cameras.') : t('dock.view_only','View-only account: moving cameras needs an operator or admin account.')) + '</div>';
    const pr = caps.presets || [];
    h += '<div class="dk-row">' +
      (pr.length ? '<select id="dkPresets" aria-label="' + t('dock.presets', 'Presets') + '"' + (dis ? ' disabled' : '') + '><option value="">' + t('dock.presets_go', 'Presets (go to)') + '</option>' +
        pr.map(p => '<option value="' + esc(p.token) + '">' + esc(p.name) + '</option>').join('') + '</select>' : '<span class="dk-note">' + t('dock.no_presets', 'No presets saved') + '</span>') +
      '</div>';
    if (admin) {
      h += presetOpen
        ? '<div class="dk-row"><input id="dkPresetName" maxlength="40" placeholder="' + t('dock.preset_name', 'Preset name') + '" value="' + esc(presetName) + '" aria-label="' + t('dock.preset_name', 'Preset name') + '">' +
          '<button type="button" class="btn" data-act="presetsave">' + t('dock.save', 'Save') + '</button><button type="button" class="btn" data-act="presetcancel">' + t('dock.cancel', 'Cancel') + '</button></div>'
        : '<div class="dk-row"><button type="button" class="btn" data-act="presetopen" title="' + t('dock.preset_open_tip', 'Store where the camera points now') + '">' + t('dock.preset_open', 'Save current position as...') + '</button></div>';
    }
    if (caps.aim) {
      h += '<div class="dk-row"><button type="button" class="btn' + (tool === 'aim' ? ' on' : '') + '" data-act="aim" aria-pressed="' + (tool === 'aim') + '"' + (dis ? ' disabled' : '') +
        ' title="' + t('dock.aim_title','Click a point in the picture to center the camera on it; drag a box to zoom into it') + '">' + ic('aim') + '<span>' + t('dock.click_to_aim', 'Click to aim') + '</span></button></div>';
      if (tool === 'aim') h += '<div class="dk-hint">' + t('dock.aim_hint','Click a spot in the picture to center it, or drag a box to zoom in. Esc to stop.') + '</div>';
    }
  } else if (caps && caps.error && c.ptz) {
    h += '<div class="dk-note">' + t('dock.ptz_prefix','PTZ: ') + esc(caps.error) + '</div>';
  }
  // The camera's own spotlight and siren (state for everyone; switches for who may use them).
  if (ctl) h += '<div class="dk-lbl">' + t('dock.lights_siren','Lights and siren') + '</div><div id="dkCtl"></div>';
  if (mayZones && !full && !c.remote) {
    h += '<div class="dk-lbl">' + t('dock.motion_zones','Motion zones') + '</div><div class="dk-btns">' +
      '<button type="button" class="btn" data-act="zedit" title="' + t('dock.edit_zones','Draw, move, resize and delete motion zones and ignore zones') + '">' + ic('zone') + '<span>' + t('dock.edit_zones_btn','Edit zones') + '</span></button></div>';
  }
  if (ctlStop) { ctlStop(); ctlStop = null; }
  sec.innerHTML = h;
  if (ctl) ctlStop = mountPanel($('dkCtl'), c.id);
}

function onCamClick(ev) {
  const b = ev.target.closest('[data-act]');
  if (!b || b.disabled) return;
  const id = Live.dockCam();
  if (id === null) return;
  switch (b.dataset.act) {
    case 'snap': Live.snapshot(id); break;
    case 'clip': Live.clipNow(id); break;
    case 'full': Live.openFullCam(id); break;
    case 'tl': enterFromFull(); break;
    case 'clips': { const cc = camFromId(id); if (cc) { setSheet(false); openClipsFor(cc); } break; }
    case 'talk': if (talk && talk.id === id) stopTalk(); else startTalk(id); break;
    case 'aim': Live.setTool(Live.getTool() === 'aim' ? null : 'aim'); break;
    case 'zedit': import('./zones.js').then(m => m.openZoneEditor(id, { opener: b })); break;
    case 'ptzstop': ptzSend(id, { action: 'stop' }); break;
    case 'presetopen': presetOpen = true; renderCam(true); { const i = $('dkPresetName'); if (i) i.focus(); } break;
    case 'presetcancel': presetOpen = false; presetName = ''; renderCam(true); break;
    case 'presetsave': savePreset(); break;
    default: break;
  }
}

function onCamChange(ev) {
  if (ev.target.id !== 'dkPresets') return;
  const id = Live.dockCam();
  const tok = ev.target.value;
  ev.target.value = '';
  if (id !== null && tok) ptzSend(id, { action: 'goto', preset: tok });
}

async function savePreset() {
  const id = Live.dockCam();
  const name = (presetName || '').trim();
  if (id === null || !name) { toast(t('dock.give_preset_name', 'Give the preset a name')); return; }
  presetOpen = false; presetName = '';
  await ptzSend(id, { action: 'set_preset', name });
  toast(t('dock.saved_preset', 'Saved preset {name}', { name }));
  fetchCaps(id, true);
}

// ---------- PTZ: hold to move, release to stop ----------
let chain = Promise.resolve();
const hold = { id: null, timer: 0, pan: 0, tilt: 0, zoom: 0, warned: false };

function ptzSend(id, body) {
  const p = chain.then(() => api('/api/cameras/' + id + '/ptz', { json: body })).catch((e) => {
    if (!hold.warned) toast(t('dock.ptz_error', 'PTZ: {msg}', { msg: (e.body || e.message) })); hold.warned = true;
  });
  chain = p;
  return p;
}
function sendHeld() {
  if (hold.id === null) return;
  ptzSend(hold.id, { action: 'move', pan: hold.pan, tilt: hold.tilt, zoom: hold.zoom, timeout_ms: 1500 });
}
function startMove(pan, tilt, zoom) {
  const id = Live.dockCam();
  if (id === null || !can('ptz')) return;
  endMove();
  Object.assign(hold, { id, pan: pan * 0.5, tilt: tilt * 0.5, zoom: zoom * 0.5, warned: false });
  sendHeld();
  hold.timer = setInterval(sendHeld, 900);
}
function endMove() {
  if (hold.id === null) return;
  clearInterval(hold.timer);
  const id = hold.id;
  hold.id = null;
  ptzSend(id, { action: 'stop' });
}
function onPadDown(ev) {
  const b = ev.target.closest('[data-mv]');
  if (!b || b.disabled || ev.button > 0) return;
  ev.preventDefault();
  const [pan, tilt, zoom] = b.dataset.mv.split(',').map(Number);
  try { b.setPointerCapture(ev.pointerId); } catch (e) { /* synthetic event */ }
  const stop = () => {
    b.removeEventListener('pointerup', stop);
    b.removeEventListener('pointercancel', stop);
    b.removeEventListener('lostpointercapture', stop);
    endMove();
  };
  b.addEventListener('pointerup', stop);
  b.addEventListener('pointercancel', stop);
  b.addEventListener('lostpointercapture', stop);
  startMove(pan, tilt, zoom);
}
const ARROWS = { ArrowUp: [0, 1], ArrowDown: [0, -1], ArrowLeft: [-1, 0], ArrowRight: [1, 0] };
const keysDown = new Set();   // arrows held now: two together move diagonally
function keyVector() {
  let p = 0, t = 0;
  for (const k of keysDown) { p += ARROWS[k][0]; t += ARROWS[k][1]; }
  return [Math.sign(p), Math.sign(t)];
}
function ptzUsable() { return can('ptz') && !!$('dkPtz'); }
function onDockKey(ev) {
  const mv = ARROWS[ev.key];
  const tg = ev.target;
  if (!mv || !ptzUsable() || /^(INPUT|SELECT|TEXTAREA)$/.test(tg.tagName)) return;
  ev.preventDefault();
  ev.stopPropagation();
  if (ev.repeat || keysDown.has(ev.key)) return;
  keysDown.add(ev.key);
  const [p, t] = keyVector();
  startMove(p, t, 0);
}
function onDockKeyUp(ev) {
  if (!ARROWS[ev.key]) return;
  keysDown.delete(ev.key);
  if (hold.id === null) return;
  ev.preventDefault();
  const [p, t] = keyVector();
  if (p || t) startMove(p, t, 0); else endMove();
}
window.addEventListener('blur', () => { keysDown.clear(); });

// ---------- talk (needs a secure page for the microphone) ----------
async function startTalk(id) {
  stopTalk();
  let stream;
  try { stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true } }); }
  catch (e) { toast(t('dock.microphone_unavailable', 'Microphone not available: {msg}', { msg: (e.message || e.name) })); return; }
  const AC = window.AudioContext || window.webkitAudioContext;
  const ctx = new AC();
  const ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/api/cameras/' + id + '/talk?rate=' + Math.round(ctx.sampleRate));
  ws.binaryType = 'arraybuffer';
  const src = ctx.createMediaStreamSource(stream);
  const proc = ctx.createScriptProcessor(4096, 1, 1);
  proc.onaudioprocess = (e) => {
    if (ws.readyState !== 1) return;
    const f = e.inputBuffer.getChannelData(0);
    const pcm = new Int16Array(f.length);
    for (let i = 0; i < f.length; i++) pcm[i] = Math.max(-1, Math.min(1, f[i])) * 32767;
    ws.send(pcm.buffer);
  };
  src.connect(proc);
  proc.connect(ctx.destination);
  const stop = () => {
    try { proc.disconnect(); src.disconnect(); } catch (e) { /* ignore */ }
    for (const t of stream.getTracks()) t.stop();
    try { ctx.close(); } catch (e) { /* ignore */ }
    try { ws.close(); } catch (e) { /* ignore */ }
    if (talk && talk.stop === stop) { talk = null; renderCam(); }
  };
  ws.onclose = (ev) => { if (talk && talk.stop === stop) { if (ev.reason) toast(t('dock.talk_reason', 'Talk: {reason}', { reason: ev.reason })); stop(); } };
  talk = { id, stop };
  renderCam();
}
function stopTalk() { if (talk) talk.stop(); }
function stopTalkIfOther() { if (talk && talk.id !== Live.dockCam()) talk.stop(); }
