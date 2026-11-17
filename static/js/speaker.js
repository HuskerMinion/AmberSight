// speaker.js - the speaker control shown on a camera picture: a round icon at the
// bottom left that appears while the pointer moves over the picture. Click mutes
// and unmutes; beside it a volume slider. One implementation for grid tiles, the full
// view, pop-out windows and the solo timeline view.
//
// Only one camera is audible at a time (unmuting one mutes the previous). Everything
// starts muted. The volume is remembered per camera name in the user's prefs.
// A tile showing JPEG frames has no audio of its own: unmuting it starts a hidden
// <video> playing the camera's sub stream, stopped again when muted.

import { api, ic, camFromId, toast } from './core.js';
import * as Prefs from './prefs.js';
import { startLive, noSound } from './live.js';

const DEFAULT_VOL = 0.8;
let active = null;       // the control that is currently audible

export const volumeOf = (name) => {
  const v = (Prefs.get('volumes', {}) || {})[name];
  return typeof v === 'number' && v >= 0 && v <= 1 ? v : DEFAULT_VOL;
};
function saveVolume(name, v) {
  const all = Object.assign({}, Prefs.get('volumes', {}) || {});
  all[name] = Math.round(v * 100) / 100;
  Prefs.set({ volumes: all });
}

// /api/live/:id/info, cached briefly and shared between controls.
const infoCache = new Map();
function liveInfo(id) {
  const hit = infoCache.get(id);
  if (hit && Date.now() - hit.at < 15000) return hit.p;
  const p = api('/api/live/' + id + '/info').catch(() => null);
  infoCache.set(id, { at: Date.now(), p });
  return p;
}

// Does the stream in use carry an audio track?
async function hasAudio(id, q) {
  const info = await liveInfo(id);
  if (!info) return false;
  const s = info[q] || info[q === 'sub' ? 'main' : 'sub'];
  return !!(s && s.audio);
}

// Stop whatever is audible (page or tab change).
export function stopAll() { if (active) active.mute(); }
export function activeCam() { return active ? active.id : null; }

// opts: { id, q: 'sub'|'main', videos: () => [<video>...] (elements that can play this
//   camera's sound; empty = use a hidden player), info: false to skip the track check
//   (recorded playback), above: raise the control (tiles with an overlay line),
//   apply: (audible, volume) => {} hands the sound to its owner instead (the
//   timeline picks which of its videos may sound) }
export function attach(host, opts) {
  const id = opts.id;
  const q = opts.q || 'sub';
  const cam = () => camFromId(id);
  const name = () => (cam() ? cam().name : 'camera');
  let muted = true;
  let vol = volumeOf(name());
  let hidden = null;      // { video, player } while a hidden player runs
  let ok = false, checked = 0, checking = false, gone = false;
  let hideT = 0, openT = 0;

  const root = document.createElement('div');
  root.className = 'spk' + (opts.above ? ' above' : '');
  root.hidden = true;
  const btn = document.createElement('button');
  btn.type = 'button'; btn.className = 'spkbtn';
  const slider = document.createElement('input');
  slider.type = 'range'; slider.className = 'spkvol';
  slider.min = '0'; slider.max = '1'; slider.step = '0.01';
  slider.setAttribute('aria-label', 'Volume');
  root.append(btn, slider);
  host.appendChild(root);

  const els = () => (opts.videos ? opts.videos() : []).filter(Boolean);
  const draw = () => {
    const low = muted || vol === 0;
    btn.innerHTML = ic(low ? 'mute' : vol < 0.5 ? 'vol-low' : 'vol');
    const label = (muted ? 'Unmute ' : 'Mute ') + name();
    btn.setAttribute('aria-label', label);
    btn.title = label;
    slider.value = String(vol);
    slider.title = 'Volume ' + Math.round(vol * 100) + '%';
    root.classList.toggle('unmuted', !muted);
  };
  const applyVol = () => {
    if (opts.apply) { opts.apply(!muted, vol); return; }
    for (const v of els()) v.volume = vol;
    if (hidden) hidden.video.volume = vol;
  };
  const kill = () => {
    if (!hidden) return;
    hidden.player.stop();
    hidden.video.remove();
    hidden = null;
  };
  const mute = () => {
    if (muted) return;
    muted = true;
    if (opts.apply) opts.apply(false, vol);
    for (const v of els()) v.muted = true;
    kill();
    if (active === self) active = null;
    draw();
  };
  const say = () => toast((name() || 'This camera') + ': its sound can\'t be played in this browser. You get the picture only. Reloading the page tries again.');
  // The browser gave up on this camera's sound while it was playing.
  const onNoSound = (e) => { if (e.detail && e.detail.cam === id && !muted) { mute(); say(); } };
  window.addEventListener('ambersight:nosound', onNoSound);
  const unmute = () => {
    if (gone) return;
    if (noSound(id)) { say(); return; }
    if (active && active !== self) active.mute();
    active = self;
    muted = false;
    vol = volumeOf(name());
    const list = els();
    if (opts.apply) opts.apply(true, vol);
    else if (list.length) {
      for (const v of list) { v.volume = vol; v.muted = false; }
      const p = list[0].paused ? list[0].play() : null;
      if (p && p.catch) p.catch(() => {});
    } else {
      const video = document.createElement('video');
      video.className = 'spkhidden'; video.playsInline = true; video.autoplay = true;
      video.volume = vol; video.muted = false;
      document.body.appendChild(video);
      const player = startLive(video, id, q, () => { mute(); });
      hidden = { video, player };
    }
    draw();
  };
  const toggle = () => (muted ? unmute() : mute());

  const stay = (ms) => {
    root.classList.add('on');
    clearTimeout(hideT);
    hideT = setTimeout(() => root.classList.remove('on'), ms || 2000);
  };
  const refreshAvail = async () => {
    if (checking || (checked && Date.now() - checked < 15000)) return;
    checking = true;
    let r = !!(cam() && cam().audio);
    if (r && opts.info !== false) r = await hasAudio(id, q);
    checking = false; checked = Date.now();
    if (gone) return;
    ok = r;
    if (!ok) mute();
    root.hidden = !ok;
  };
  const wake = async () => {
    if (gone) return;
    if (ok) stay();
    await refreshAvail();
    if (ok && !gone) stay();
  };
  const onMove = (ev) => { if (ev.pointerType !== 'touch') wake(); };

  host.addEventListener('pointermove', onMove);
  host.addEventListener('pointerdown', wake);
  host.addEventListener('focusin', wake);
  for (const ev of ['pointerdown', 'click', 'dblclick', 'pointerup']) root.addEventListener(ev, (e) => e.stopPropagation());
  root.addEventListener('keydown', (e) => { if (e.target === slider) e.stopPropagation(); });
  const holdOpen = () => {
    // Touch has no hover: a tap on the icon also opens the slider for a moment.
    root.classList.add('open'); clearTimeout(openT);
    openT = setTimeout(() => root.classList.remove('open'), 4000);
  };
  btn.addEventListener('click', () => {
    toggle();
    if (matchMedia('(hover: none)').matches) holdOpen();
    stay(3000);
  });
  slider.addEventListener('input', () => {
    vol = parseFloat(slider.value);
    saveVolume(name(), vol);
    applyVol();
    if (muted && vol > 0) unmute(); else draw();
    stay(3000);
    if (root.classList.contains('open')) holdOpen();
  });
  draw();

  const self = {
    id, mute, unmute, toggle,
    get muted() { return muted; },
    // The picture underneath changed (or went away): silence and re-check.
    reset() { mute(); checked = 0; },
    destroy() {
      gone = true;
      window.removeEventListener('ambersight:nosound', onNoSound);
      mute();
      clearTimeout(hideT); clearTimeout(openT);
      host.removeEventListener('pointermove', onMove);
      host.removeEventListener('pointerdown', wake);
      host.removeEventListener('focusin', wake);
      root.remove();
    },
  };
  return self;
}
