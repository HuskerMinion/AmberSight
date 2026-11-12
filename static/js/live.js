// live.js - Live tab: grid with paging/layouts/cycle, MSE player, MJPEG fallback, full screen
// (swipe / arrows), motion boxes, zone drawing.

import { savedToast, $, state, bus, api, refreshCameras, sleep, esc, msgDlg, toast, camFromId, ic, downloadUrl, bigQ, lowQ, openDlg, overlayOpen, backLayer, overlaysChanged, playVideo } from './core.js';
import { isActive } from './nav.js';
import { tvOn, tvOne, enterTv, setOne, onTvChange } from './fullscreen.js';
import * as Layouts from './layouts.js';
import * as Prefs from './prefs.js';
import { enableArrange } from './arrange.js';
import { openClipsFor } from './events.js';
import * as Speaker from './speaker.js';
import * as Zoom from './zoom.js';
import { plan, BIG, STYLES } from './viewfit.js';
import { camStep, camErr, camCount, mark } from './diag.js';
import { isPhone, device } from './device.js';
import { lowDelayInit, hasCompositionOffsets, videoTimescale, Dejitter } from './lowdelay.js';
import { openCameraDialog, setCameraEnabled, pauseAlertsDlg, resumeAlerts, pausedText, rebootDlg, canReboot } from './cameras.js';

// ---------- MSE player: fragmented MP4 over Media Source Extensions ----------
const MSEClass = window.ManagedMediaSource || window.MediaSource;
// Streams (camera + quality) found to have B-frames: their init segment is used
// as sent (see lowdelay.js).
const reorders = new Set();
// Streams (camera + quality) whose sound the browser could not decode: they are
// played without it from then on, so a fault in a camera's audio never costs the picture.
// It clears when the page is reloaded (nothing is stored), so a camera that was fixed gets its sound back.
const badAudio = new Set();
// Does the browser know this camera's sound cannot be played? (The speaker button says so.)
export function noSound(camId) { for (const k of badAudio) if (k.startsWith(camId + '/')) return true; return false; }

// Plays /api/live/:id/ws?q=main|sub (a WebSocket carrying fragmented MP4) into <video>.
// A WebSocket, not a streaming fetch: browsers allow only six HTTP connections per
// server, and a grid of streams would take them all and stall every other request.
// onFallback() is called when MSE playback is impossible (unsupported codec, stream
// not running) on the first attempt. Authentication is the session cookie, which the
// WebSocket handshake carries.
export function startLive(video, camId, q, onFallback) {
  const st = { stopped: false, ctrl: null, url: null, first: true, tries: 0, cur: null, now: false, errs: 0, lastErr: 0, wait: 0 };
  // A muted picture asks for the stream without sound: less data, and no audio decoder to fail.
  // When the picture is unmuted later (sound on), the stream starts again with sound.
  video.addEventListener('volumechange', () => {
    if (st.stopped || !st.cur || !st.cur.quiet || st.cur.noAudio || video.muted) return;
    st.now = true; st.cur.sock.close();
  });
  const cleanup = () => {
    if (st.ctrl) { try { st.ctrl.close(); } catch (e) { /* already closed */ } st.ctrl = null; }
    if (st.url) { URL.revokeObjectURL(st.url); st.url = null; }
  };
  async function session() {
    // The camera's latest picture (kept in memory by the server) shows at
    // once, until the first video frame is decoded: a big stream can take the
    // browser a few seconds to start, and the screen is never empty meanwhile.
    // (A fresh one each time: after a reconnect an old picture would mislead.)
    if (!video.classList.contains('spkhidden')) video.poster = '/api/snapshot/' + camId + '?t=' + Date.now();
    camStep(camId, 'ask');
    if (st.tries) camCount(camId, 'retries');
    const info = await api('/api/live/' + camId + '/info');
    // A camera without a separate sub stream serves its main stream for 'sub' (and the
    // other way round); the server picks the same one.
    const s = info[q] || info[q === 'sub' ? 'main' : 'sub'];
    // A battery camera that is still waking up: ask again shortly (not a failure).
    if (!s && info.waking) return 'waking';
    const key = camId + '/' + q;
    // Without sound when it is not wanted (muted), not playable here, or failed before.
    // (Not playable: starting again cannot bring it back, so the speaker says so instead.)
    const noAudio = !!(s && s.audio && s.video_mime && (badAudio.has(key) || !(MSEClass && MSEClass.isTypeSupported(s.mime))));
    const quiet = noAudio || !!(s && s.audio && s.video_mime && video.muted);
    if (noAudio) badAudio.add(key);
    if (noAudio && !video.muted) window.dispatchEvent(new CustomEvent('ambersight:nosound', { detail: { cam: camId } }));
    const mime = quiet ? s.video_mime : (s && s.mime);
    if (!s || !MSEClass || !MSEClass.isTypeSupported(mime)) return st.first ? 'fallback' : 'retry';
    camStep(camId, 'info', { q, size: s.width ? s.width + 'x' + s.height : '?' });
    if (s.width && s.height) video.style.aspectRatio = s.width + ' / ' + s.height;
    const ms = new MSEClass();
    st.url = URL.createObjectURL(ms);
    if (window.ManagedMediaSource && ms instanceof window.ManagedMediaSource) video.disableRemotePlayback = true;
    video.src = st.url;
    await new Promise(res => ms.addEventListener('sourceopen', res, { once: true }));
    if (st.stopped) return 'stop';
    const sb = ms.addSourceBuffer(mime);
    const queue = [];
    let gotData = false;
    const sock = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/api/live/' + camId + '/ws?q=' + q + (quiet ? '&audio=0' : ''));
    sock.binaryType = 'arraybuffer';
    st.ctrl = sock;
    st.cur = { sock, quiet, noAudio };
    sock.addEventListener('open', () => camStep(camId, 'open'));
    video.addEventListener('playing', () => camStep(camId, 'play'), { once: true });
    video.addEventListener('error', () => {
      camErr(camId, 'video' + (video.error ? video.error.code : ''));
      if (st.stopped || st.cur === null || st.cur.sock !== sock) return;
      // The browser stopped on this stream. With sound in it, that may be the sound (a camera
      // whose audio the decoder rejects): start again without it, at once, so the picture returns.
      // Only a decode error says so, or this stream failing again soon: one network hiccup must
      // not cost the camera its sound until the page is reloaded.
      const code = video.error ? video.error.code : 0;
      const again = st.lastErr && Date.now() - st.lastErr < 60000;
      if (!quiet && s.audio && s.video_mime && (code === 3 || code === 4 || again)) {
        badAudio.add(key); st.now = true;
        window.dispatchEvent(new CustomEvent('ambersight:nosound', { detail: { cam: camId } }));
      } else {
        // Anything else (the picture-only stream failing too, a server without picture-only
        // streams): try again, waiting longer each time it fails again soon after.
        const now = Date.now();
        st.errs = now - st.lastErr > 60000 ? 1 : st.errs + 1;
        st.lastErr = now;
        st.wait = Math.min(15000, 1000 * Math.pow(2, st.errs));
      }
      sock.close();
    }, { once: true });
    const trim = (force) => {
      if (st.stopped || ms.readyState !== 'open' || sb.updating) return;
      let b;
      try { b = sb.buffered; } catch (e) { return; }   // stopped meanwhile
      if (!b.length) return;
      const cut = video.currentTime - (force ? 5 : 30);
      if (b.start(0) < cut - 1) { try { sb.remove(b.start(0), cut); } catch (e) { /* ignore */ } }
    };
    const pump = () => {
      if (sb.updating || !queue.length || ms.readyState !== 'open') return;
      const chunk = queue.shift();
      try { sb.appendBuffer(chunk); } catch (e) {
        if (e.name === 'QuotaExceededError') { queue.unshift(chunk); trim(true); } else throw e;
      }
    };
    // Latency: the player aims to keep just enough buffered to ride out the
    // stream's own unevenness (cameras send in bursts). It watches how low the
    // buffer gets just before each frame arrives (the low-water mark over the
    // last few seconds) and plays a little faster while that stays well above
    // `spare`, a little slower when it nears zero; a hard seek to the live edge is
    // kept for big gaps (a hidden tab, a long network hiccup). Seeking whenever
    // the buffer went over a fixed mark made bursty cameras jump and stall.
    const lows = [];             // [time, seconds buffered ahead when a fragment arrived]
    let since = 0;               // when the low-water history (re)started
    let lastRate = 0, snapped = false, dropped = 0, lastDrop = 0;
    // How much to keep in hand beyond the stream's unevenness. Raised while the
    // browser drops frames (some decoders, hardware ones especially, need more
    // frames queued to keep up; so does a busy machine), lowered again slowly.
    let spare = 0.12;
    const FAR = 3;
    const ahead = () => { try { const b = sb.buffered; return b.length ? b.end(b.length - 1) - video.currentTime : 0; } catch (e) { return 0; } };
    const steer = (end) => {
      const now = performance.now();
      const lead = end - video.currentTime;
      // The frames since the last keyframe come in one burst at the start:
      // begin near their end, not at the keyframe.
      if (lead > FAR || (!snapped && now - since > 300 && lead > 0.6)) {
        snapped = true;
        video.currentTime = end - 0.3; video.playbackRate = 1;
        lows.length = 0; since = now; return;
      }
      if (now - lastRate < 500 || now - since < 2500 || video.paused || !lows.length) return;
      lastRate = now;
      snapped = true;
      const q = video.getVideoPlaybackQuality ? video.getVideoPlaybackQuality().droppedVideoFrames : 0;
      if (q > dropped) { spare = Math.min(0.7, spare + 0.05); lastDrop = now; }
      else if (now - lastDrop > 30000 && spare > 0.12) { spare -= 0.05; lastDrop = now; }
      dropped = q;
      while (lows.length && lows[0][0] < now - 4000) lows.shift();
      const excess = Math.min(...lows.map(l => l[1])) - spare;
      // Few, small changes (each one can cost a frame): 5 % faster (10 % when
      // far behind) until the extra delay is worked off, 5 % slower while the
      // buffer is nearly empty.
      const cur = video.playbackRate;
      let rate = cur;
      if (excess > (cur > 1 ? 0.05 : 0.2)) rate = excess > 0.6 ? 1.1 : 1.05;
      else if (excess < (cur < 1 ? 0 : -0.08)) rate = 0.95;
      else rate = 1;
      if (video.playbackRate !== rate) video.playbackRate = rate;
    };
    sb.addEventListener('updateend', () => {
      if (st.stopped || ms.readyState !== 'open') return;
      let b;
      try { b = sb.buffered; } catch (e) { return; }
      if (b.length) {
        const end = b.end(b.length - 1);
        if (!gotData) {
          gotData = true; st.first = false; st.tries = 0;
          camStep(camId, 'buf');
          video.currentTime = Math.max(b.start(0), b.end(0) - 0.3);
          since = performance.now();
          playVideo(video);
        } else steer(end);
        if (video.paused && !video.ended && video.readyState >= 2) playVideo(video);
      }
      trim(false);
      pump();
    });
    // Resolves when the socket closes (camera restarted, viewer fell behind, network)
    // or we stop; 'fallback' if it closed before any data on the first attempt.
    return await new Promise((resolve) => {
      let any = false;
      let even = null;
      sock.onmessage = (m) => {
        if (st.stopped) return;
        let chunk = new Uint8Array(m.data);
        if (!any) {
          // The init segment: let the decoder show frames without delay.
          if (!reorders.has(key)) {
            chunk = lowDelayInit(chunk);
            const ts = videoTimescale(chunk);
            if (ts) even = new Dejitter(ts, s.audio ? 0.15 : 0.5);
          }
        } else if (!reorders.has(key) && hasCompositionOffsets(chunk)) {
          // B-frames after all: start again with the camera's own parameters.
          reorders.add(key); sock.close(); return;
        } else {
          if (even && !even.apply(chunk)) even = null;
          if (gotData && !video.paused) lows.push([performance.now(), ahead()]);
        }
        if (!any) camStep(camId, 'data');
        any = true;
        queue.push(chunk);
        pump();
        if (queue.length > 500) sock.close();   // consumer far behind: reconnect
      };
      sock.onerror = () => {};
      sock.onclose = () => resolve(st.stopped ? 'stop' : (!any && st.first && !st.now ? 'fallback' : 'retry'));
    });
  }
  (async () => {
    while (!st.stopped) {
      let r;
      try { r = await session(); } catch (e) { r = st.stopped ? 'stop' : 'retry'; camErr(camId, String(e && (e.name || e.message) || e).slice(0, 40)); }
      if (r === 'retry' || r === 'fallback') camErr(camId, r);
      cleanup();
      st.cur = null;
      if (st.stopped) return;
      if (st.now && r === 'retry') { st.now = false; st.tries = 0; continue; }
      if (r === 'waking') { await sleep(1000); continue; }
      if (r === 'retry' && st.first && st.tries >= 3) r = 'fallback';
      if (r === 'fallback') { video.removeAttribute('src'); onFallback(); return; }
      const pause = Math.max(Math.min(10000, 500 * Math.pow(2, st.tries++)), st.wait);
      st.wait = 0;
      await sleep(pause);
    }
  })();
  return { stop() { st.stopped = true; cleanup(); video.removeAttribute('src'); try { video.load(); } catch (e) { /* ignore */ } } };
}

// ---------- light player: JPEG frames over a WebSocket ----------
// The server pushes each camera's latest motion-analysis frame (about 5-6 fps,
// already decoded for motion detection), so a grid of many cameras stays light
// for the browser and costs the server nothing extra. Reconnects by itself.
export function startJpeg(img, camId) {
  const st = { stopped: false, sock: null, url: null, pending: null, tries: 0, timer: 0 };
  const open = () => {
    if (st.stopped) return;
    const sock = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/api/live/' + camId + '/jpeg?fps=6');
    sock.binaryType = 'blob';
    st.sock = sock;
    sock.onmessage = (m) => {
      if (st.stopped) return;
      st.tries = 0;
      const url = URL.createObjectURL(m.data);
      // (A frame still loading when the next arrives is dropped and freed.)
      if (st.pending) URL.revokeObjectURL(st.pending);
      st.pending = url;
      img.onload = () => { if (st.url && st.url !== url) URL.revokeObjectURL(st.url); st.url = url; if (st.pending === url) st.pending = null; markFirstFrame(); camStep(camId, 'jpeg'); };
      img.src = url;
    };
    sock.onerror = () => {};
    sock.onclose = () => { if (!st.stopped) st.timer = setTimeout(open, Math.min(10000, 500 * Math.pow(2, st.tries++))); };
  };
  open();
  return { stop() {
    st.stopped = true; clearTimeout(st.timer);
    if (st.sock) { try { st.sock.close(); } catch (e) { /* closed */ } }
    if (st.url) { URL.revokeObjectURL(st.url); st.url = null; }
    if (st.pending) { URL.revokeObjectURL(st.pending); st.pending = null; }
  } };
}
function smoothGrid() { return Layouts.getPrefs().quality === 'smooth'; }

// ---------- right-click menu on a tile ----------
// "Open in new window" shows just that camera in its own browser window (handy
// on a second monitor); it uses the same sign-in.
export function popOut(id) {
  const c = camFromId(id);
  const w = window.open('/?cam=' + id + '#live', 'ambs-cam-' + id, 'popup,width=1280,height=760');
  if (!w) toast('The browser blocked the new window: allow pop-ups for this site');
  else if (c) try { w.focus(); } catch (e) { /* ignore */ }
}
let ctxMenu = null;
function closeCtx() { if (ctxMenu) { ctxMenu.remove(); ctxMenu = null; } }
// The per-camera menu: right-click a tile or the full view, or press a camera's gear.
// `anchor` (a button) places the menu under it instead of at the pointer.
// Back closes the full view (not in a pop-out window: that is all it shows).
backLayer(20, () => fullCamId !== null && !document.body.classList.contains('popout'), () => closeFull());

// ---------- what a double-click on a camera opens (per user) ----------
// Prefs 'dblclick': { all: 'full' | 'timeline', cams: { <camera name>: 'full' | 'timeline' } }.
const DBL = { full: 'Full view (live)', timeline: 'Timeline, at live' };
function dblPref() {
  const d = Prefs.get('dblclick', null);
  return { all: d && d.all === 'timeline' ? 'timeline' : 'full', cams: (d && d.cams && typeof d.cams === 'object') ? d.cams : {} };
}
export function dblOpens(c) {
  const d = dblPref();
  const own = d.cams[c.name];
  return own === 'full' || own === 'timeline' ? own : d.all;
}
// Per-user "a single click opens the camera" (default off: double-click opens). Pref 'oneclick'.
export const oneClickOpens = () => Prefs.get('oneclick', false) === true;
function openTileTarget(id) {
  const c = camFromId(id);
  if (c && !c.remote && dblOpens(c) === 'timeline') bus.emit('timeline-live', id); else openFullCam(id);
}
// Choose for all cameras, and (given `c`) for that camera: switches that
// show what is set now and save as they are flipped.
export function dblClickDlg(c) {
  const sw = (attr, on, label, hint) => '<label class="sw"><input type="checkbox" role="switch" ' + attr + (on ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
    '<span class="sw-text"><b>' + esc(label) + '</b>' + (hint ? '<small>' + esc(hint) + '</small>' : '') + '</span></label>';
  const d = dblPref();
  const own = c ? d.cams[c.name] : undefined;
  const html = '<div class="hint">Your choice only, kept by this server (set it on each server you use). Off: the live full view. The Timeline button or L opens the timeline either way.</div>' +
    '<div class="sec">All cameras</div>' +
    sw('data-one', oneClickOpens(), 'A single click opens the camera', 'Instead of only selecting it. Double-click then does the same.') +
    sw('data-all', d.all === 'timeline', 'Double-click opens the timeline, at live') +
    (c ? '<div class="sec">' + esc(camLabel(c)) + '</div>' +
      sw('data-own', own === 'full' || own === 'timeline', 'Its own setting', 'Otherwise it follows all cameras') +
      sw('data-cam', dblOpens(c) === 'timeline', 'Double-click opens the timeline, at live') : '');
  const dlg = openDlg('Double-click opens', html, true);
  const q = (a) => dlg.querySelector('[' + a + ']');
  const sync = () => {
    if (!c) return;
    const p = dblPref();
    const has = p.cams[c.name] === 'full' || p.cams[c.name] === 'timeline';
    q('data-own').checked = has;
    q('data-cam').checked = dblOpens(c) === 'timeline';
    q('data-cam').disabled = !has;
    q('data-cam').closest('.sw').classList.toggle('off', !has);
  };
  const save = (all, cams) => { Prefs.set({ dblclick: { all, cams } }); sync(); };
  q('data-one').onchange = (e) => { Prefs.set({ oneclick: e.target.checked }); savedToast(); };
  q('data-all').onchange = (e) => { const p = dblPref(); save(e.target.checked ? 'timeline' : 'full', p.cams); };
  if (c) {
    q('data-own').onchange = (e) => {
      const p = dblPref(), cams = Object.assign({}, p.cams);
      if (e.target.checked) cams[c.name] = p.all; else delete cams[c.name];
      save(p.all, cams);
    };
    q('data-cam').onchange = (e) => {
      const p = dblPref(), cams = Object.assign({}, p.cams);
      cams[c.name] = e.target.checked ? 'timeline' : 'full';
      save(p.all, cams);
    };
    sync();
  }
}

function tileMenu(ev, c, anchor) {
  ev.preventDefault();
  ev.stopPropagation();
  const wasOpenFor = ctxMenu && ctxMenu.dataset.cam === String(c.id) && anchor;
  closeCtx();
  if (wasOpenFor) return;   // the gear toggles its menu
  c = camFromId(c.id) || c;
  const inFull = fullCamId === c.id;
  const spk = inFull ? fullSpk : (liveTiles[c.id] && liveTiles[c.id].spk);
  const items = [];
  const sep = () => { if (items.length && items[items.length - 1] !== null) items.push(null); };
  if (!inFull) items.push(['expand', 'Open full size', () => openFullCam(c.id)]);
  items.push(['grid', 'Open in new window', () => popOut(c.id)]);
  sep();
  if (spk && c.audio) items.push([spk.muted ? 'vol' : 'mute', spk.muted ? 'Sound on' : 'Sound off', () => spk.toggle()]);
  items.push(['camera', 'Save snapshot', () => snapshot(c.id)]);
  if (c.remote) {
    // Another server's camera: recordings, clips and settings live there.
    sep();
    items.push(['timeline', 'Open on ' + c.remote.site, () => openOnSite(c)]);
  } else {
  if (state.isAdmin) items.push(['rec', 'Record a 20 s clip', () => clipNow(c.id)]);
  sep();
  items.push(['timeline', 'Timeline', () => bus.emit('timeline-for', c.id)]);
  items.push(['play', 'Clips', () => { if (inFull) closeFull(); openClipsFor(c); }]);
  items.push(['expand', 'Double-click opens: ' + (dblOpens(c) === 'timeline' ? 'Timeline' : 'Full view') + '...', () => dblClickDlg(c)]);
  }
  if (state.isAdmin && !c.remote) {
    sep();
    if (c.alerts_paused) items.push(['play', 'Resume notifications (paused ' + pausedText(c) + ')', () => resumeAlerts(c)]);
    else items.push(['pause', 'Pause notifications...', () => pauseAlertsDlg(c)]);
    if (!inFull) items.push(['zone', 'Draw a motion zone', () => { setSelected(c.id); import('./zones.js').then(m => m.openZoneEditor(c.id)); }]);
    items.push(['gear', 'Camera settings...', () => openCameraDialog(c.id)]);
    if (canReboot(c)) items.push(['power', 'Reboot camera...', () => rebootDlg(c)]);
    items.push([c.enabled === false ? 'play' : 'pause', c.enabled === false ? 'Enable camera' : 'Disable camera', () => setCameraEnabled(c.id, c.enabled === false)]);
  }
  const m = document.createElement('div');
  m.className = 'ctxmenu'; m.setAttribute('role', 'menu');
  m.dataset.cam = String(c.id);
  m.innerHTML = '<div class="ctxhead">' + esc(camLabel(c)) + '</div>' +
    items.map((it, i) => it === null ? '<div class="ctxsep"></div>' : '<button role="menuitem" data-i="' + i + '">' + ic(it[0]) + '<span>' + esc(it[1]) + '</span></button>').join('');
  document.body.appendChild(m);
  const r = m.getBoundingClientRect();
  let x = ev.clientX, y = ev.clientY;
  if (anchor) { const a = anchor.getBoundingClientRect(); x = a.right - r.width; y = a.bottom + 4; }
  m.style.left = Math.max(6, Math.min(x, innerWidth - r.width - 6)) + 'px';
  m.style.top = Math.max(6, Math.min(y, innerHeight - r.height - 6)) + 'px';
  m.onclick = (e) => { const b = e.target.closest('[data-i]'); if (!b) return; closeCtx(); items[+b.dataset.i][2](); };
  m.querySelector('button').focus();
  ctxMenu = m;
}
// The "Layouts" button: edit the saved layouts, or arrange the cameras in the one on screen.
function layoutsMenu(ev, anchor) {
  ev.preventDefault();
  ev.stopPropagation();
  const was = ctxMenu && ctxMenu.dataset.layouts;
  closeCtx();
  if (was) return;
  const items = [['grid', 'Edit layouts...', () => Layouts.openEditor()]];
  if (canArrange()) items.push(['arrange', 'Arrange cameras...', () => startArrange()]);
  const m = document.createElement('div');
  m.className = 'ctxmenu'; m.setAttribute('role', 'menu'); m.dataset.layouts = '1';
  m.innerHTML = items.map((it, i) => '<button role="menuitem" data-i="' + i + '">' + ic(it[0]) + '<span>' + esc(it[1]) + '</span></button>').join('');
  document.body.appendChild(m);
  const r = m.getBoundingClientRect(), a = anchor.getBoundingClientRect();
  m.style.left = Math.max(6, Math.min(a.left, innerWidth - r.width - 6)) + 'px';
  m.style.top = Math.max(6, Math.min(a.bottom + 4, innerHeight - r.height - 6)) + 'px';
  m.onclick = (e) => { const b = e.target.closest('[data-i]'); if (!b) return; closeCtx(); items[+b.dataset.i][2](); };
  m.querySelector('button').focus();
  ctxMenu = m;
}
document.addEventListener('pointerdown', (e) => { if (ctxMenu && !ctxMenu.contains(e.target) && !(e.target.closest && e.target.closest('[data-menu]'))) closeCtx(); }, true);
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && ctxMenu) { e.preventDefault(); e.stopPropagation(); closeCtx(); } }, true);
window.addEventListener('blur', closeCtx);
window.addEventListener('scroll', closeCtx, true);

// ---------- grid ----------
let liveSig = null, liveTiles = {};
let hoverId = null;      // tile under the mouse: keyboard shortcuts act on it
let selId = null;        // camera chosen by clicking a tile (or from the dock)
let tool = null;         // 'aim': what a drag on the selected camera's picture does
let arrange = null;      // { key, names, ctl } while arranging tiles
let lastDrag = 0;
let escTs = 0;

// Timing for 'page load -> first live frame' (read by tests via window.__ambs).
window.__ambs = window.__ambs || {};
function markFirstFrame() {
  mark('firstFrame');
  if (window.__ambs.firstFrameMs === undefined) window.__ambs.firstFrameMs = Math.round(performance.now());
}
export function selectedCam() {
  return selId !== null ? selId : (hoverId !== null ? hoverId : (order.length ? order[0] : null));
}
export const getSelected = () => selId;
// The camera the dock controls: the one open full size, else the selected tile.
export const dockCam = () => (fullCamId !== null ? fullCamId : selId);
function markSelection() {
  for (const k in liveTiles) liveTiles[k].tile.classList.toggle('sel', Number(k) === selId);
}
export function setSelected(id) {
  if (id === selId) return;
  selId = id;
  if (tool && fullCamId === null) setTool(null);
  markSelection();
  bus.emit('select', id);
}
export const getTool = () => tool;
export function setTool(t) {
  if (t === tool) return;
  tool = t;
  document.body.dataset.tool = t || '';
  bus.emit('tool', t);
}
export function camIdAt(n) { return order[n] !== undefined ? order[n] : null; }
export function snapshot(id) {
  const c = camFromId(id);
  downloadUrl('/api/snapshot/' + id + '?download=1&t=' + Date.now(), ((c && c.name) || 'camera') + '-' + new Date().toISOString().replace(/[:.]/g, '-') + '.jpg');
}
export async function clipNow(id) {
  if (!state.isAdmin) { msgDlg('view-only account: sign in as an admin to record clips'); return; }
  try {
    const r = await api('/api/clips/record', { json: { camera: id, seconds: 20 } });
    if (r.error) { msgDlg(r.error); return; }
    const c = camFromId(id);
    toast('Recording a 20 s clip from ' + ((c && c.name) || id));
  } catch (e) { msgDlg(e.message); }
}
// Timeline mode (see livetl.js): while a hook is installed the tiles play recordings
// instead of live video. hook = { mount(frame, cam) -> {el, stop()}, shown(ids), tileClick(id) }.
let tlHook = null;
let soloId = null;
export function setTimelineHook(h) {
  if (h && arrange) finishArrange(false);
  tlHook = h;
  if (!h) soloId = null;
  liveSig = null;
  if (isActive('live')) renderLive();
}
export function setSolo(id) { soloId = id; liveSig = null; renderLive(); }
export function soloCam() { return soloId; }
let page = 0;
let order = [];          // camera ids of the whole ordered list (all pages), used by full screen
let cycleTimer = null;

export function stopTiles() {
  for (const id in liveTiles) { const t = liveTiles[id]; if (t.spk) t.spk.destroy(); if (t.player) t.player.stop(); clearTimeout(t.sleepTimer); }
  liveTiles = {}; liveSig = null;
}

// Motion boxes are a tuning aid: off unless the user turns them on in the dock.
function drawBlobs(box, c) {
  const bl = (Layouts.getPrefs().boxes === 'motion' && c.motion && c.blobs) ? c.blobs : [];
  box.innerHTML = bl.map(b => '<div class="blob" style="left:' + (b.x * 100) + '%;top:' + (b.y * 100) + '%;width:' + (b.w * 100) + '%;height:' + (b.h * 100) + '%"></div>').join('');
}

// ---------- the camera's text on the picture ----------
const OVL_POS = ['top-left', 'top-right', 'bottom-left', 'bottom-right'];
export function overlayPos(c) { return OVL_POS.includes(c.overlay_pos) ? c.overlay_pos : 'bottom-left'; }
// Fill-ins: {name} {short} {site} {date} {time}, and {v:NAME} for a named
// overlay variable an automation pushes in (weather, a temperature, ...).
export function overlayText(c, now) {
  const tpl = (c.overlay_text || '').trim();
  if (!tpl) return '';
  const d = now || new Date();
  const name = c.remote ? c.remote.camera : c.name;
  const vals = {
    name, short: c.short_name || name,
    site: c.remote ? c.remote.site : (state.serverName || ''),
    date: d.toLocaleDateString(), time: d.toLocaleTimeString(),
  };
  // One pass, so a value that itself holds braces is shown as it is. An
  // unset or expired variable, or an unknown fill-in, shows nothing.
  return tpl.replace(/\{([^}]*)\}/g, (_, k) => {
    if (k.startsWith('v:')) { const v = state.overlayVars && state.overlayVars[k.slice(2)]; return v == null ? '' : String(v); }
    return Object.prototype.hasOwnProperty.call(vals, k) ? vals[k] : '';
  });
}
function paintOverlayText(el) {
  const c = camFromId(Number(el.dataset.camid));
  el.textContent = c ? overlayText(c) : '';
}
// Once a second, so {time} ticks.
setInterval(() => { document.querySelectorAll('.ovltext[data-camid]').forEach(paintOverlayText); }, 1000);

// What a tile calls a camera: its short name, or for another server's camera
// its own name and where it is.
export function camLabel(c) {
  if (c.remote) return (c.short_name || c.remote.camera) + ' · ' + c.remote.site;
  return c.short_name || c.name;
}

// Open another server's camera there (signed in as the same user).
async function openOnSite(c) {
  try {
    const o = await api('/api/links/' + encodeURIComponent(c.remote.link) + '/open', { json: {} });
    // The whole window, not a split-view pane: another server cannot be shown in a frame.
    (window.top || window).location.href = o.url;
  } catch (e) { toast((e && e.body) || e.message); }
}

// ---------- picture labels (HUD): Clean / Standard / Detailed, kept per browser ----------
// body[data-hud] drives the CSS. Clean: only the alert ring, and the name pill on hover.
// Standard: the pills. Detailed: also frames per second, bitrate, size and the AI label with its score.
const HUD_KEY = 'ambs.hud', HUD_LEVELS = ['clean', 'standard', 'detailed'];
let hud = 'standard';
try { const v = localStorage.getItem(HUD_KEY); if (HUD_LEVELS.includes(v)) hud = v; } catch (e) { /* default */ }
document.body.dataset.hud = hud;
export const hudLevel = () => hud;
export function setHud(l, quiet) {
  if (!HUD_LEVELS.includes(l) || l === hud) return;
  hud = l; document.body.dataset.hud = l;
  try { localStorage.setItem(HUD_KEY, l); } catch (e) { /* not remembered */ }
  if (!quiet) toast('Labels on pictures: ' + l[0].toUpperCase() + l.slice(1), { ms: 1500 });
  bus.emit('hud');
  if (l === 'detailed') pollRates();
}
export function cycleHud() { setHud(HUD_LEVELS[(HUD_LEVELS.indexOf(hud) + 1) % HUD_LEVELS.length]); }
// "B": motion outlines on / off (the dock's "Motion outlines").
export function toggleBoxes() {
  const on = Layouts.getPrefs().boxes !== 'motion';
  Layouts.setPrefs({ boxes: on ? 'motion' : 'off' });
  toast('Motion outlines ' + (on ? 'on' : 'off'), { ms: 1500 });
}
// Bitrate and frames per second come from the server's figures (admins); polled only while Detailed is on.
let rates = {}, ratesAt = 0;
async function pollRates() {
  if (hud !== 'detailed' || !state.isAdmin || Date.now() - ratesAt < 4000) return;
  ratesAt = Date.now();
  try { const s = await api('/api/stats'); const m = {}; for (const c of s.cameras || []) m[c.name] = c; rates = m; } catch (e) { /* none shown */ }
}
function detailText(p, c) {
  const r = rates[c.name], v = p.root.querySelector('video'), bits = [];
  if (r && r.fps_in != null) bits.push(Number(r.fps_in).toFixed(1) + ' fps');
  if (r && r.kbps != null) bits.push(r.kbps >= 1000 ? (r.kbps / 1000).toFixed(1) + ' Mbit/s' : r.kbps + ' kbit/s');
  if (v && v.videoWidth) bits.push(v.videoWidth + '\u00d7' + v.videoHeight);
  return bits.join(' \u00b7 ');
}

// ---------- tile pills, edge states, reconnect cover (design B2) ----------
// Every picture (grid tile, Fit/Focus tile, full view) gets the same corner pills and an edge
// drawn by CSS from data-edge on its host. The camera JSON may carry the newest AI result as
//   ai_last: { label: "person", age_ms: 1200, score: 0.91 }   (age_ms at the time of the reply)
// (or `at`: epoch seconds/ms). Without it only motion is shown.
// AI_HOLD: how long a detection shows after the AI's look; while the camera's
// motion goes on (the same event; the AI stops looking once it is sure) it
// stays up to AI_EVENT_HOLD.
const AI_HOLD = 5000, AI_EVENT_HOLD = 120000, RECON_AFTER = 3000, OFFLINE_AFTER = 15000;
let camsRx = Date.now();
const conn = {};          // camera id -> { down: ms it dropped, seen: online since this page opened, lastOk }
function trackConn(c, now) {
  let s = conn[c.id];
  if (!s) s = conn[c.id] = { down: 0, seen: false, lastOk: 0 };
  if (c.online) { s.down = 0; s.seen = true; s.lastOk = now; }
  else if (!s.down) s.down = now;
  return s;
}
const AI_CLASS = { person: 'person', people: 'person', face: 'face', car: 'vehicle', truck: 'vehicle', bus: 'vehicle', motorcycle: 'vehicle', bicycle: 'vehicle', vehicle: 'vehicle',
  train: 'vehicle', boat: 'vehicle', van: 'vehicle', suv: 'vehicle',
  dog: 'animal', cat: 'animal', bird: 'animal', animal: 'animal', deer: 'animal', bear: 'animal', horse: 'animal', sheep: 'animal', cow: 'animal',
  elephant: 'animal', zebra: 'animal', giraffe: 'animal', package: 'package', parcel: 'package' };
const AI_ICON = { person: 'person', face: 'user', vehicle: 'car', animal: 'paw', package: 'alert' };
function aiRecent(c, now) {
  const a = c.ai_last;
  if (!a || !a.label) return null;
  let age = Infinity;
  if (typeof a.age_ms === 'number') age = a.age_ms + (now - camsRx);
  else if (typeof a.at === 'number') age = now - (a.at < 1e12 ? a.at * 1000 : a.at);
  if (!(age < AI_HOLD || (c.motion && age < AI_EVENT_HOLD))) return null;
  const label = String(a.label).toLowerCase();
  return { label, cls: AI_CLASS[label] || 'package', score: a.score };
}
// What a picture shows now: { edge: ''|motion|ai|recon|offline, dot, ai, s }.
function statusOf(c, now) {
  const s = trackConn(c, now);
  if (c.enabled === false) return { edge: '', dot: 'gray', s };
  // A battery camera that pushed PIR motion: show it at once, even before
  // (or without) a picture.
  if (c.push_motion && !c.online) return { edge: 'motion', dot: 'motion', s };
  if (!c.online && c.on_demand) return { edge: '', dot: 'gray', s };
  if (!c.online) {
    const down = now - s.down;
    if (!s.seen || down >= OFFLINE_AFTER) return { edge: 'offline', dot: 'fault', s };
    return { edge: down >= RECON_AFTER ? 'recon' : '', dot: 'gray', s };
  }
  const ai = aiRecent(c, now);
  if (ai) return { edge: 'ai', dot: 'ai', ai, s };
  if (c.motion || c.push_motion) return { edge: 'motion', dot: 'motion', s };
  return { edge: '', dot: 'ok', s };
}
const fmtClock = (d) => d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
function ago(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  return s < 60 ? s + ' s' : (s < 3600 ? Math.floor(s / 60) + ' min' : Math.floor(s / 3600) + ' h ' + (Math.floor(s / 60) % 60) + ' min');
}

// Build the pills inside `host` (a tile's frame or the full view's content). `root` carries
// data-edge / data-cls. o.actions: the hover buttons; o.retry(): the reconnect action.
function buildPills(host, root, c, o) {
  const mk = (cls, html, into) => { const e = document.createElement('div'); e.className = cls; if (html) e.innerHTML = html; (into || host).appendChild(e); return e; };
  const p = { root, c, retry: o.retry || null, chipKey: null };
  const top = mk('ptop'), bot = mk('pbot');
  p.tl = o.name === false ? null : mk('pill ptl', '<i class="sdot"></i><span class="nm"></span><span class="pz" hidden>' + ic('belloff') + '<span class="pzt">Notifications paused</span></span>', top);
  if (p.tl) { p.dot = p.tl.querySelector('.sdot'); p.nm = p.tl.querySelector('.nm'); p.pz = p.tl.querySelector('.pz'); }
  p.tr = mk('ptr', '', top);
  p.chip = document.createElement('span'); p.chip.className = 'pill dchip'; p.chip.hidden = true;
  p.tr.appendChild(p.chip);
  if (o.actions) p.tr.appendChild(o.actions);
  p.bl = mk('pill pbl', '<i class="rdot"></i>REC', bot); p.bl.hidden = true;
  p.det = mk('pill pdet', '', bot); p.det.hidden = true;
  p.br = mk('pill pbr', '', bot);
  p.cover = mk('cover'); p.cover.hidden = true;
  return p;
}
function paintPills(p, c, now) {
  now = now || Date.now();
  const st = statusOf(c, now);
  const root = p.root;
  if (root.dataset.edge !== st.edge) {
    if (st.edge) root.dataset.last = st.edge;
    root.dataset.edge = st.edge;
  }
  if (st.ai && root.dataset.cls !== st.ai.cls) root.dataset.cls = st.ai.cls;
  if (p.dot) p.dot.className = 'sdot s-' + st.dot;
  if (p.tl) {
    const label = camLabel(c);
    if (p.nm.textContent !== label) p.nm.textContent = label;
    const groups = (c.groups || []).concat(Array.isArray(c.labels) ? c.labels : []);
    const tip = label + (groups.length ? '\n' + groups.join(', ') : '') + (c.enabled === false ? '\nDisabled' : '') + (c.alerts_paused ? '\nNotifications paused ' + pausedText(c) + ' (still recording)' : '');
    if (p.tl.title !== tip) p.tl.title = tip;
    p.pz.hidden = !c.alerts_paused;
  }
  // Detection chip (class icon + name) while an AI result is recent; the hover buttons otherwise.
  const showScore = hud === 'detailed' && st.ai && typeof st.ai.score === 'number';
  const chipKey = st.ai ? st.ai.cls + ':' + st.ai.label + (showScore ? ':' + Math.round(st.ai.score * 100) : '') : '';
  if (p.chipKey !== chipKey) {
    p.chipKey = chipKey;
    p.chip.hidden = !st.ai;
    p.chip.innerHTML = st.ai ? ic(AI_ICON[st.ai.cls] || 'alert') + '<span>' + esc(st.ai.label) + (showScore ? ' ' + Math.round(st.ai.score * 100) + '%' : '') + '</span>' : '';
    p.chip.dataset.cls = st.ai ? st.ai.cls : '';
    p.tr.classList.toggle('has-chip', !!st.ai);
  }
  const rec = !!c.record && !c.remote && c.enabled !== false && c.online && (!c.record_motion_only || !!c.motion);
  p.bl.hidden = !rec;
  if (rec) p.bl.title = c.record_motion_only ? 'Recording (motion)' : (c.record_sub && c.sub_url ? 'Recording (sub stream and events)' : 'Recording');
  p.br.textContent = fmtClock(new Date(now));
  const tip = c.schedule ? 'Armed ' + c.schedule : '';
  if (p.br.title !== tip) p.br.title = tip;
  if (hud === 'detailed') { const t = detailText(p, c); p.det.hidden = !t; if (p.det.textContent !== t) p.det.textContent = t; pollRates(); } else if (!p.det.hidden) p.det.hidden = true;
  // Cover: Reconnecting... / Offline.
  const cv = p.cover;
  if (st.edge !== 'recon' && st.edge !== 'offline') { cv.hidden = true; cv.dataset.k = ''; return st; }
  cv.hidden = false;
  if (cv.dataset.k !== st.edge) {
    cv.dataset.k = st.edge;
    cv.innerHTML = st.edge === 'recon'
      ? '<span class="cv-spin"></span><span class="cv-big">Reconnecting…</span><span class="cv-small"></span>'
      : '<span class="cv-big"></span><span class="cv-small"></span>' + (p.retry ? '<button type="button" class="cv-rbtn">Retry</button>' : '');
    const rb = cv.querySelector('.cv-rbtn');
    if (rb) {
      rb.onpointerdown = (ev) => ev.stopPropagation();
      rb.ondblclick = (ev) => ev.stopPropagation();
      rb.onclick = (ev) => { ev.stopPropagation(); p.retry(); };
    }
  }
  const s = st.s, small = cv.querySelector(".cv-small");
  if (st.edge === 'recon') small.textContent = 'dropped ' + ago(now - s.down) + ' ago';
  else {
    cv.querySelector(".cv-big").textContent = s.seen ? 'Offline · ' + ago(now - s.down) : 'Offline';
    small.textContent = s.lastOk ? 'Last picture ' + fmtClock(new Date(s.lastOk)) : '';
  }
  return st;
}
// Once a second: clocks, hold timers and elapsed times.
setInterval(() => {
  if (!isActive('live') && fullCamId === null) return;
  const now = Date.now();
  if (!tlHook) for (const id in liveTiles) { const t = liveTiles[id], c = camFromId(Number(id)); if (c && t.pills) paintPills(t.pills, c, now); }
  if (fullCamId !== null && fullPills) { const c = camFromId(fullCamId); if (c) paintPills(fullPills, c, now); }
}, 1000);

// Real picture ratios (width / height), remembered across reloads so tiles do not jump.
let arCache = {};
try { arCache = JSON.parse(localStorage.getItem('ambs.ar') || '{}') || {}; } catch (e) { arCache = {}; }
let arSaveT = 0;
function setAr(t, id, ar) {
  if (!(ar > 0.2 && ar < 10)) return false;
  ar = Math.round(ar * 1000) / 1000;
  if (t.ar === ar) return false;
  t.ar = ar;
  // Grid boxes: very wide cameras are capped at 2.2:1 (the picture is letterboxed in it), tall ones not below square.
  t.tile.style.setProperty('--ar', String(ar));
  t.tile.style.setProperty('--arc', String(Math.min(2.2, Math.max(1, ar))));
  if (arCache[id] !== ar) {
    arCache[id] = ar;
    clearTimeout(arSaveT);
    arSaveT = setTimeout(() => { try { localStorage.setItem('ambs.ar', JSON.stringify(arCache)); } catch (e) { /* not remembered */ } }, 2000);
  }
  return true;
}
let fitRaf2 = 0;
const scheduleFit = () => { cancelAnimationFrame(fitRaf2); fitRaf2 = requestAnimationFrame(() => fitGrid()); };

function camStatus(c) { return c.online ? 'LIVE' : (c.enabled === false ? 'DISABLED' : 'OFFLINE'); }
// What a tile shows: 'live' video, 'asleep' (battery camera: tap to wake; the grid
// never wakes it on its own), or 'offline'. A live tile rides out short drops (its
// player reconnects by itself) and only turns 'offline' after 15 s.
function wantedMode(t, c) {
  if (tlHook) return 'tl';
  const now = Date.now();
  if (c.online) t.offSince = 0; else if (!t.offSince) t.offSince = now;
  if (c.on_demand && now >= t.wakeUntil) return 'asleep';
  if (c.online || c.on_demand) return 'live';
  if (t.forceLive > now) return 'live';   // Retry pressed
  if (t.mode === 'live' && now - t.offSince < 15000) return 'live';
  return 'offline';
}

// The picture a live tile showed last, kept when the stream drops.
function grabLast(t) {
  const m = t.media;
  if (!m || (m.tagName !== 'VIDEO' && m.tagName !== 'IMG')) return;
  const w = m.videoWidth || m.naturalWidth, h = m.videoHeight || m.naturalHeight;
  if (!w || !h || (m.tagName === 'VIDEO' && m.readyState < 2)) return;
  try {
    const k = Math.min(1, 640 / w);
    const cv = document.createElement('canvas');
    cv.width = Math.round(w * k); cv.height = Math.round(h * k);
    cv.getContext('2d').drawImage(m, 0, 0, cv.width, cv.height);
    cv.className = 'lastcv';
    t.last = { cv, at: Date.now() };
  } catch (e) { /* not drawable */ }
}

function setTileMedia(t, c) {
  const mode = wantedMode(t, c);
  if (mode === t.mode) return;
  if (t.spk) t.spk.reset();
  if (t.mode === 'live') grabLast(t);
  if (t.player) { t.player.stop(); t.player = null; }
  if (t.media) { t.media.remove(); t.media = null; }
  t.mode = mode;
  const frame = t.frame;
  if (mode === 'tl') {
    const h = tlHook.mount(frame, c);
    frame.insertBefore(h.el, frame.firstChild);
    t.media = h.el; t.player = h;
    return;
  }
  // Another server's camera always comes as its video sub stream (far less
  // data over the link than JPEG pictures; see Light/Smooth).
  if (mode === 'live' && !smoothGrid() && !t.big && !c.remote) {
    const img = document.createElement('img');
    img.className = 'livevid'; img.alt = c.name;
    frame.insertBefore(img, frame.firstChild);
    t.media = img;
    t.player = startJpeg(img, c.id);
    return;
  }
  if (mode === 'live') {
    const video = document.createElement('video');
    video.className = 'livevid';
    video.muted = true; video.autoplay = true; video.playsInline = true;
    frame.insertBefore(video, frame.firstChild);
    t.media = video;
    // Sub stream in the grid; ffmpeg-pipeline cameras fall back to the MJPEG <img>.
    video.addEventListener('playing', markFirstFrame, { once: true });
    t.player = startLive(video, c.id, t.big && !c.remote ? bigQ() : 'sub', () => {
      if (t.spk) t.spk.reset();
      video.remove();
      const img = document.createElement('img');
      img.className = 'livevid'; img.alt = c.name;
      frame.insertBefore(img, frame.firstChild);
      t.media = img;
      t.player = startJpeg(img, c.id);
    });
    return;
  }
  const ph = document.createElement('div');
  ph.className = 'tileph';
  if (mode === 'asleep') {
    ph.innerHTML = '<span>' + ic('moon') + 'Asleep</span><button class="btn">Tap to wake</button>';
    const wb = ph.querySelector('button');
    wb.onpointerdown = (ev) => ev.stopPropagation();
    wb.onclick = (ev) => { ev.stopPropagation(); wakeTile(t, c); };
  } else if (c.enabled === false) {
    ph.innerHTML = '<span>Disabled</span>';
  } else if (t.last) {
    // Keep the last picture (dimmed by CSS) under the Offline cover.
    ph.classList.add('lastpic');
    ph.appendChild(t.last.cv);
  }
  frame.insertBefore(ph, frame.firstChild);
  t.media = ph;
}

// Wake a battery camera from its tile and show it for its idle timeout; then the
// tile goes back to 'asleep' so an open grid does not keep the camera awake.
async function wakeTile(t, c) {
  const secs = Math.max(30, c.idle_timeout || 300);
  t.wakeUntil = Date.now() + secs * 1000;
  try { await api('/api/cameras/' + c.id + '/wake', { method: 'POST' }); } catch (e) { /* the stream request wakes it too */ }
  setTileMedia(t, c);
  clearTimeout(t.sleepTimer);
  t.sleepTimer = setTimeout(() => { const cur = camFromId(c.id) || c; setTileMedia(t, cur); }, secs * 1000 + 500);
}

function updateTile(t, c) {
  if (tlHook) return;   // (timeline mode: the player draws its own corner pills)
  if (t.pills) paintPills(t.pills, c);
  drawBlobs(t.blobs, c);
}

// A camera's groups ('Ungrouped' when it has none). A camera can be in several.
export const camGroups = (c) => ((c.groups && c.groups.length) ? c.groups : (c.group ? [c.group] : ['Ungrouped']));

function updateGroupFilter() {
  const gsel = $('groupFilter');
  const groups = [...new Set(state.cams.flatMap(camGroups))].sort((a, b) => (a === 'Ungrouped') - (b === 'Ungrouped') || a.localeCompare(b));
  const key = groups.join('|');
  if (key === gsel.dataset.groups) return;
  const cur = gsel.value;
  gsel.innerHTML = '<option value="">All groups</option>' + groups.map(g => '<option value="' + esc(g) + '">' + esc(g) + '</option>').join('');
  if ([...gsel.options].some(o => o.value === cur)) gsel.value = cur;
  gsel.dataset.groups = key;
}

// The shared-order key of what the group filter shows: 'all' or 'group:<name>'.
function viewKey() {
  const gf = $('groupFilter').value || '';
  return gf === '' ? 'all' : 'group:' + gf;
}

// The key a view style is saved under: the saved layout, else the group filter's view.
function styleKey() {
  const lay = Layouts.active();
  return lay.id ? 'layout:' + lay.id : viewKey();
}
// The style in effect: window-filling styles need room (not on phones) and are off in
// timeline mode.
function liveStyle() {
  const st = Layouts.styleFor(styleKey());
  if (tlHook || st === 'grid') return 'grid';
  return window.innerWidth < 700 ? 'grid' : st;
}
export function setViewStyle(st) {
  if (!STYLES.includes(st)) return;
  Layouts.setStyle(styleKey(), st);
  liveSig = null;
  renderLive();
}
export function cycleViewStyle() {
  const cur = Layouts.styleFor(styleKey());
  setViewStyle(STYLES[(STYLES.indexOf(cur) + 1) % STYLES.length]);
}
function syncStyleUi() {
  const cur = Layouts.styleFor(styleKey());
  for (const b of $('viewStyle').querySelectorAll('button')) {
    const on = b.dataset.style === cur;
    b.classList.toggle('on', on);
    b.setAttribute('aria-pressed', on ? 'true' : 'false');
  }
}

// Size the window-filling styles: columns, rows and the large-tile span for the cameras
// shown, so the grid fills the space from its top edge to the bottom of the window.
let fitN = 0;
// ---------- where one camera's picture goes ----------
// The timeline's one-camera view and the full view put the picture in the
// same place: under the toolbar, centered, leaving room below for the timeline
// dock. Opening the timeline from the full view (or going back) then only
// adds or removes the dock under a picture that stays where it is.
const DOCK_RESERVE_KEY = 'ambs.tlReserve';
const stored = (k) => { try { return parseFloat(localStorage.getItem(k)) || 0; } catch (e) { return 0; } };
const store = (k, v) => { try { localStorage.setItem(k, String(v)); } catch (e) { /* not remembered */ } };
let area = null; // the grid's area as distances from the window edges: {left, right, top}
// The timeline dock's height with its margins, and the solo tile's name bar:
// remembered from the timeline view so the full view matches it.
let dockReserve = stored(DOCK_RESERVE_KEY) || 186;
// Auto-hide timeline controls float over the cameras: no room kept for them.
const dockSpace = () => (document.documentElement.dataset.tlctl === 'autohide' ? 0 : dockReserve);

function measureArea() {
  const grid = $('camGrid');
  if (!grid || !grid.offsetParent || document.body.classList.contains('fullview')) return;
  const host = grid.parentElement;
  const r = host.getBoundingClientRect();
  const cs = getComputedStyle(host);
  const left = r.left + (parseFloat(cs.paddingLeft) || 0) + (parseFloat(cs.borderLeftWidth) || 0);
  const right = window.innerWidth - (r.right - (parseFloat(cs.paddingRight) || 0) - (parseFloat(cs.borderRightWidth) || 0));
  // Under the toolbar with the one-camera view's gap (window-filling views
  // use a smaller one).
  const bar = grid.previousElementSibling;
  const s3 = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--s3')) || 12;
  const top = (bar ? bar.getBoundingClientRect().bottom + s3 : grid.getBoundingClientRect().top) + window.scrollY;
  if (r.width > 0) area = { left, right, top };
  const dock = $('tlDock');
  if (dock && !dock.hidden) {
    // (Its height, not where it is: it is sticky and sits right under a
    // short grid.)
    const h = dock.getBoundingClientRect().height;
    if (h > 0) {
      const ds = getComputedStyle(dock);
      const v = Math.round(h + (parseFloat(ds.marginTop) || 0) + (parseFloat(ds.bottom) || 0));
      if (v !== dockReserve) { dockReserve = v; store(DOCK_RESERVE_KEY, v); }
    }
  }
}

// The picture's box (16:9) in window coordinates.
function soloBox() {
  measureArea();
  const a = area || { left: 16, right: 16, top: 170 };
  const meta = 0;   // (no strip above the picture any more)
  const availW = Math.max(200, window.innerWidth - a.left - a.right);
  const availH = window.innerHeight - dockSpace() - a.top - meta - 2;
  const width = Math.round(Math.max(Math.min(360, availW), Math.min(availW, availH * 16 / 9)));
  const height = Math.round(width * 9 / 16);
  const left = Math.round(a.left + (availW - width) / 2);
  return { left, top: Math.round(a.top + meta), width, height };
}

// Full view: the picture as large as the space under its toolbar allows, at the
// camera's own ratio, centered (no cap: a small stream is scaled up too). Phones and a
// pop-out window: the CSS gives it the whole screen.
function fullAr() {
  const m = $('fullVid') || $('fullImg');
  const w = m && (m.videoWidth || m.naturalWidth), h = m && (m.videoHeight || m.naturalHeight);
  if (w && h) return w / h;
  const t = liveTiles[fullCamId];
  return (t && t.ar) || arCache[fullCamId] || 16 / 9;
}
function placeFull() {
  const c = $('lightContent'), lb = $('lightbox');
  const use = fullCamId !== null && lb.classList.contains('cam') && lb.style.display === 'flex' && !isPhone() && !document.body.classList.contains('popout');
  c.classList.toggle('placed', use);
  if (!use) { for (const k of ['left', 'top', 'width', 'height']) c.style[k] = ''; return; }
  const r = lb.getBoundingClientRect();
  const bar = lb.querySelector('.lbtop').getBoundingClientRect();
  const pad = 8;
  const top = Math.max(r.top + pad, bar.bottom);
  const availW = Math.max(160, r.width - 2 * pad), availH = Math.max(90, r.bottom - pad - top);
  const ar = fullAr();
  const width = Math.floor(Math.min(availW, availH * ar));
  const height = Math.floor(width / ar);
  c.style.left = Math.round(r.left + (r.width - width) / 2) + 'px';
  c.style.top = Math.round(top + (availH - height) / 2) + 'px';
  c.style.width = width + 'px';
  c.style.height = height + 'px';
}

// Grid style with the automatic layout: the number of columns that makes the tiles
// largest for this window (its orientation and the cameras' ratios), all cameras on one
// screen when they fit at a readable size; else as many readable columns as fit (the
// page scrolls). Phones keep their own one- or two-column CSS.
function autoCols(grid) {
  const n = grid.querySelectorAll(':scope > .tile').length;
  const on = n > 0 && grid.dataset.per === 'auto' && !isPhone();
  if (!on) { grid.style.gridTemplateColumns = ''; grid.style.maxWidth = grid.dataset.per === 'solo' ? grid.style.maxWidth : ''; grid.style.marginInline = ''; return; }
  const r = grid.getBoundingClientRect();
  const W = grid.clientWidth;
  if (W <= 0) return;
  const main = grid.closest('main');
  const padB = main ? parseFloat(getComputedStyle(main).paddingBottom) || 0 : 0;
  let H = window.innerHeight - (r.top + window.scrollY) - padB;
  const tl = $('tlDock');
  if (tl && !tl.hidden) H -= dockSpace();
  H = Math.max(200, H);
  const gap = parseFloat(getComputedStyle(grid).columnGap) || 0;
  const tlm = grid.classList.contains('tlmode');
  // (The grid style's picture boxes: the camera's ratio held between 1:1 and 2.2:1; 16:9 in the timeline.)
  const ars = [...grid.children].filter(el => el.classList.contains('tile')).map(el => {
    const t = liveTiles[el.dataset.id];
    return tlm ? 16 / 9 : Math.min(2.2, Math.max(1, (t && t.ar) || 16 / 9));
  });
  // (The tiles of a row share the tallest picture box, so plan with the tallest ratio.)
  const arMin = Math.min(...ars);
  const p = plan(n, W, H, gap, 'grid', arMin);
  let C = p.cols;
  const minW = device() === 'desktop' ? 300 : 260;
  let cw = (W - (C - 1) * gap) / C;
  let maxW = '';
  if (cw < minW) {
    C = Math.max(1, Math.min(n, Math.floor((W + gap) / (minW + gap))));
  } else {
    // All on one screen: if the rows would still run past the bottom, narrow the
    // grid (centered) so they do not.
    const R = Math.ceil(n / C);
    const ch = (H - (R - 1) * gap) / R;
    if (cw / arMin > ch) {
      cw = ch * arMin;
      maxW = Math.floor(C * cw + (C - 1) * gap) + 'px';
    }
  }
  grid.style.gridTemplateColumns = 'repeat(' + C + ', minmax(0, 1fr))';
  grid.style.maxWidth = maxW;
  grid.style.marginInline = maxW ? 'auto' : '';
}

// Full screen: the grid is the whole screen; pick the columns x rows that make the pictures largest.
function fitTv(grid) {
  const W = window.innerWidth, gap = 2, n = Math.max(1, fitN);
  // (The timeline's controls take the bottom edge, unless they float over the cameras.)
  const tl = $('tlDock');
  const H = window.innerHeight - (tl && !tl.hidden && document.documentElement.dataset.tlctl !== 'autohide' ? tl.getBoundingClientRect().height : 0);
  const ars = [...grid.children].map(el => { const t = liveTiles[el.dataset.id]; return (t && t.ar) || 16 / 9; });
  const ar = ars.length ? Math.min(...ars) : 16 / 9;
  let best = { cols: 1, rows: n, size: 0 };
  for (let cols = 1; cols <= n; cols++) {
    const rows = Math.ceil(n / cols);
    const cw = (W - (cols - 1) * gap) / cols, ch = (H - (rows - 1) * gap) / rows;
    const size = Math.min(cw, ch * ar);   // the picture's width in its cell
    if (size > best.size + 0.5) best = { cols, rows, size };
  }
  grid.style.height = H + 'px';
  grid.style.maxWidth = '';
  grid.style.marginInline = '';
  grid.style.gridTemplateColumns = 'repeat(' + best.cols + ', minmax(0, 1fr))';
  grid.style.gridTemplateRows = 'repeat(' + best.rows + ', minmax(0, 1fr))';
  grid.style.setProperty('--k', '1');
  grid.dataset.big = '0';
}

function fitGrid() {
  const grid = $('camGrid');
  grid.style.maxWidth = grid.dataset.per === 'solo' ? soloBox().width + 2 + 'px' : '';
  if (fullCamId !== null) placeFull();
  if (!grid.classList.contains('fill')) {
    for (const p of ['gridTemplateColumns', 'gridTemplateRows', 'height', 'marginInline']) grid.style[p] = '';
    autoCols(grid);
    return;
  }
  if (tvOn()) { fitTv(grid); return; }
  const r = grid.getBoundingClientRect();
  const main = grid.closest('main');
  const padB = main ? parseFloat(getComputedStyle(main).paddingBottom) || 0 : 0;
  const H = Math.max(tvOn() ? 120 : 240, window.innerHeight - (r.top + window.scrollY) - (tvOn() ? 0 : padB));
  const W = grid.clientWidth;
  const gap = parseFloat(getComputedStyle(grid).columnGap) || 0;
  // The cameras' real ratios, in tile order (unknown ones count as 16:9).
  const ars = [...grid.children].map(el => { const t = liveTiles[el.dataset.id]; return (t && t.ar) || 16 / 9; });
  const p = plan(fitN, W, H, gap, grid.dataset.style, ars.length === fitN ? ars : undefined);
  // Cells no taller than the tallest picture needs and no wider than the widest needs:
  // the spare space goes to the page (below, and evenly at the sides), not to black
  // bars inside the tiles (a portrait screen with few cameras, or a wide one).
  const cw = (W - (p.cols - 1) * gap) / p.cols, ch = (H - (p.rows - 1) * gap) / p.rows;
  const list = ars.length ? ars : [16 / 9];
  const rh = Math.min(ch, cw / Math.min(...list));
  const cw2 = Math.min(cw, rh * Math.max(...list));
  const gh = Math.floor(p.rows * rh + (p.rows - 1) * gap);
  const gw = Math.floor(p.cols * cw2 + (p.cols - 1) * gap);
  grid.style.height = gh + 'px';
  grid.style.maxWidth = gw < W - 1 ? gw + 'px' : '';
  grid.style.marginInline = gw < W - 1 ? 'auto' : '';
  grid.style.gridTemplateColumns = 'repeat(' + p.cols + ', minmax(0, 1fr))';
  grid.style.gridTemplateRows = 'repeat(' + p.rows + ', minmax(0, 1fr))';
  grid.style.setProperty('--k', String(p.k));
  grid.dataset.big = String(p.big);
}

// Cameras after group filter + order: the active layout's own list, or the saved shared
// order of the view (cameras it does not name come last, in the server's order).
function orderedCams() {
  const gf = $('groupFilter').value || '';
  let pool = gf === '' ? state.cams : state.cams.filter(c => camGroups(c).includes(gf));
  const lay = Layouts.active();
  if (Layouts.isCustom(lay)) {
    const list = Layouts.camsOf(lay).map(n => pool.find(c => c.name === n)).filter(Boolean);
    return arrange ? Prefs.applyOrder(list, arrange.names) : list;
  }
  return Prefs.applyOrder(pool, arrange ? arrange.names : Prefs.orderFor(viewKey()));
}

// ---------- arrange mode ----------
export const isArranging = () => !!arrange;
function canArrange() { return !tlHook && (Layouts.isCustom(Layouts.active()) ? true : state.isAdmin); }
function syncArrangeUi() {
  const b = $('arrangeBtn');
  const on = !!arrange;
  b.classList.toggle('on', on);
  b.setAttribute('aria-pressed', on ? 'true' : 'false');
  b.querySelector('span').textContent = 'Done';
  b.hidden = !on;
  const ok = on || canArrange();
  b.disabled = !ok;
  b.title = on ? 'Save this order (Esc cancels)'
    : (tlHook ? 'Leave timeline mode to arrange the tiles'
      : (!state.isAdmin && !Layouts.isCustom(Layouts.active()) ? 'Only an administrator can change the shared camera order (make a layout for your own)'
        : 'Drag the tiles into the order you want' + (BIG[Layouts.styleFor(styleKey())] ? ' (the first places get the large tiles)' : '')));
  $('arrangeCancel').hidden = !on;
  $('camGrid').classList.toggle('arranging', on);
}
export function startArrange() {
  if (arrange || !canArrange()) return;
  const lay = Layouts.active();
  arrange = { key: viewKey(), layout: Layouts.isCustom(lay) ? lay.id : null, names: orderedCams().map(c => c.name), ctl: null };
  page = 0;
  setTool(null);
  liveSig = null;
  renderLive();
  arrange.ctl = enableArrange($('camGrid'), (names) => { if (arrange) arrange.names = names; });
  syncArrangeUi();
  $('camGrid').querySelector('.tile')?.focus();
}
export async function finishArrange(save) {
  const a = arrange;
  if (!a) return;
  if (save) {
    if (a.layout) {
      Layouts.reorderLayout(a.layout, a.names);
      toast('Camera order saved for ' + Layouts.active().name);
    } else {
      try { await Prefs.saveOrder(a.key, a.names); toast('Camera order saved for ' + (a.key === 'all' ? 'all cameras' : a.key.slice(6))); }
      catch (e) { msgDlg('Could not save the order: ' + e.message); return; }
    }
  }
  if (a.ctl) a.ctl.disable();
  arrange = null;
  liveSig = null;
  renderLive();
  syncArrangeUi();
}

// Live with no camera at all (a new install): one large card with the two ways in.
function firstCameraCard() {
  const admin = state.isAdmin;
  return '<div class="firstcam"><div class="fc-card">' +
    '<svg class="ic fc-ic" aria-hidden="true"><use href="#i-camera"/></svg>' +
    '<h2>' + (admin ? 'Add your first camera' : 'No cameras to show') + '</h2>' +
    (admin
      ? '<p>All you need is the camera\'s user name and password, from the camera\'s own app.</p>' +
        '<div class="fc-btns"><a class="btn primary" href="#settings/cameras/find">Find cameras</a>' +
        '<button type="button" class="btn" data-fc-add>Add by address</button></div>'
      : '<p>No cameras are shared with this account. Ask an administrator.</p>') +
    '</div></div>';
}
function wireFirstCameraCard(grid) {
  const b = grid.querySelector('[data-fc-add]');
  if (b) b.onclick = async () => { (await import('./cameras.js')).openCameraDialog(null); };
}

function renderBar(pages, per) {
  const chips = $('layoutChips');
  const sig = Layouts.list().map(l => l.id + ':' + l.name).join('|') + '#' + Layouts.active().id;
  if (chips.dataset.sig !== sig) {
    chips.dataset.sig = sig;
    chips.innerHTML = '';
    for (const l of Layouts.list()) {
      const b = document.createElement('button');
      b.textContent = l.name;
      b.className = 'chip' + (l.id === Layouts.active().id ? ' on' : '');
      b.onclick = () => { bus.emit('view-pick'); page = 0; Layouts.setActive(l.id); };
      chips.appendChild(b);
    }
  }
  syncArrangeUi();
  syncStyleUi();
  $('pager').hidden = pages <= 1;
  $('pageLbl').textContent = (page + 1) + ' / ' + pages;
}

export function renderLive() {
  if (state.cams && state.cams.length) mark('render');
  updateGroupFilter();
  const grid = $('camGrid');
  if (arrange && arrange.ctl && arrange.ctl.dragging()) return;
  if (!isActive('live')) { stopTiles(); grid.innerHTML = ''; return; }
  const all = orderedCams();
  order = all.map(c => c.id);
  const lay = Layouts.active();
  const per = lay.per === 'auto' || arrange ? 0 : lay.per;
  const pages = per ? Math.max(1, Math.ceil(all.length / per)) : 1;
  if (page >= pages) page = pages - 1;
  if (page < 0) page = 0;
  let shown = per ? all.slice(page * per, page * per + per) : all;
  if (tlHook && soloId !== null) {
    const one = all.find(c => c.id === soloId);
    if (one) shown = [one]; else soloId = null;
  }
  const solo = !!tlHook && soloId !== null;
  // Full screen: one camera alone, or the grid in a window-filling style.
  if (tvOn() && !tlHook && tvOne() !== null) {
    const one = all.find(c => c.id === tvOne());
    if (one) shown = [one]; else setOne(null);
  }
  renderBar(pages, per);
  let style = solo ? 'grid' : liveStyle();
  if (tvOn() && style === 'grid') style = 'fit';
  const fill = style !== 'grid';
  grid.classList.toggle('tlmode', !!tlHook);
  grid.classList.toggle('fill', fill);
  grid.dataset.style = style;
  document.body.classList.toggle('livefill', fill);
  fitN = shown.length;
  const bigN = fill && shown.length > BIG[style] ? BIG[style] : 0;
  grid.dataset.per = solo ? 'solo' : (per ? String(per) : 'auto');
  grid.style.setProperty('--cols', per ? String(Math.round(Math.sqrt(per))) : '');
  if (!all.length) {
    grid.classList.remove('fill');
    document.body.classList.remove('livefill');
    fitGrid();
    stopTiles();
    liveSig = '';
    grid.innerHTML = state.cams.length ? '<p class="empty">No cameras in this layout.</p>' : firstCameraCard();
    if (!state.cams.length) wireFirstCameraCard(grid);
    return;
  }
  if (tlHook) tlHook.shown(shown.map(c => c.id));
  const sigCams = arrange ? shown.slice().sort((x, y) => x.id - y.id) : shown;
  const sig = JSON.stringify([!!tlHook, smoothGrid(), style, soloId, page, per, !!arrange, sigCams.map(c => [c.id, c.enabled, c.name, c.short_name, c.groups, c.record, c.record_motion_only, c.record_sub, c.schedule, c.zones, c.ignore_zones, c.ingest, c.overlay_text, c.overlay_pos, c.overlay])]);
  if (sig === liveSig) {
    for (const c of shown) if (liveTiles[c.id]) { setTileMedia(liveTiles[c.id], c); updateTile(liveTiles[c.id], c); }
    fitGrid();
    return;
  }
  stopTiles();
  liveSig = sig;
  grid.innerHTML = '';
  for (const c of shown) {
    const tile = document.createElement('div');
    tile.className = 'tile';
    const frame = document.createElement('div');
    frame.className = 'frame';
    // Large tiles (Focus styles) play the main stream; the rest the light sub stream.
    const t = { frame, tile, player: null, media: null, mode: null, offSince: 0, wakeUntil: 0, big: shown.indexOf(c) < bigN };
    setTileMedia(t, c);
    frame.addEventListener('contextmenu', (ev) => tileMenu(ev, c));
    // Zones and motion boxes sit on a layer the size of the picture itself
    // (cameras that are not 16:9, and the Fit/Focus styles, letterbox it).
    t.pic = document.createElement('div'); t.pic.className = 'pic'; frame.appendChild(t.pic);
    t.blobs = document.createElement('div'); t.blobs.className = 'blobs'; t.pic.appendChild(t.blobs);
    for (const z of (c.zones || [])) addZoneBox(t.pic, z, false);
    for (const z of (c.ignore_zones || [])) addZoneBox(t.pic, z, true);
    const fit = () => {
      fitPic(frame, t.pic);
      const m = t.media;
      const w = m && (m.videoWidth || m.naturalWidth), h = m && (m.videoHeight || m.naturalHeight);
      if (w && h && !tlHook && setAr(t, c.id, w / h) && frame.closest('.grid.fill')) scheduleFit();
    };
    frame.addEventListener('loadedmetadata', fit, true);
    frame.addEventListener('load', fit, true);
    frame.addEventListener('resize', fit, true);
    picObserver.observe(frame);
    fit();
    tile.dataset.name = c.name;
    tile.dataset.id = String(c.id);
    tile.tabIndex = 0;
    tile.setAttribute('aria-label', c.name);
    if (!arrange) attachTileInput(tile, frame, c.id);
    // The camera's own text on the picture (drawn here; costs the server
    // nothing). When it is burned into the video, the picture already has it.
    // (Not over recorded footage in timeline mode: {time} would be today's.)
    if ((c.overlay_text || '').trim() && !c.overlay && !tlHook) {
      const ot = document.createElement('div');
      ot.className = 'ovltext pos-' + overlayPos(c);
      ot.dataset.camid = String(c.id);
      frame.appendChild(ot);
      paintOverlayText(ot);
    }
    const act = document.createElement('div');
    act.className = 'tileact';
    act.onpointerdown = (ev) => ev.stopPropagation();
    act.onclick = (ev) => ev.stopPropagation();
    const mk = (icon, title, fn) => {
      const b = document.createElement('button');
      b.className = 'btn glass'; b.title = title; b.setAttribute('aria-label', title); b.innerHTML = ic(icon);
      b.onclick = (ev) => { ev.stopPropagation(); fn(); };
      act.appendChild(b);
    };
    mk('camera', 'Save snapshot (P)', () => snapshot(c.id));
    if (state.isAdmin && !c.remote) mk('rec', 'Record a 20 s clip now (C)', () => clipNow(c.id));
    mk('expand', 'Full screen', () => enterTv(c.id));
    const gear = document.createElement('button');
    gear.className = 'btn glass'; gear.title = 'Camera menu (or right-click the picture)'; gear.setAttribute('aria-label', 'Camera menu');
    gear.dataset.menu = '1'; gear.innerHTML = ic('gear');
    gear.onclick = (ev) => tileMenu(ev, c, gear);
    act.appendChild(gear);
    // Corner pills (name, detection or buttons, REC, clock), the edge and the cover.
    // (Timeline mode: the player adds its own name and time pills.)
    if (!tlHook) {
      t.pills = buildPills(frame, frame, c, { actions: act, retry: () => { t.forceLive = Date.now() + 20000; setTileMedia(t, camFromId(c.id) || c); paintPills(t.pills, camFromId(c.id) || c); } });
      const ar = arCache[c.id];
      if (ar) setAr(t, c.id, ar);
    }
    tile.onmouseenter = () => { hoverId = c.id; };
    if (!arrange && (!tlHook || solo)) {
      t.spk = Speaker.attach(frame, {
        id: c.id, q: t.big ? bigQ() : 'sub', above: true, info: !tlHook,
        videos: () => {
          return t.media && t.media.tagName === 'VIDEO' ? [t.media] : [];
        },
        apply: tlHook ? (on, vol) => { if (tlHook && tlHook.sound) tlHook.sound(c.id, on, vol); } : undefined,
      });
    }
    tile.appendChild(frame);
    grid.appendChild(tile);
    updateTile(t, c);
    liveTiles[c.id] = t;
  }
  fitGrid();
  markSelection();
}

export function setPage(p, wrap) {
  const lay = Layouts.active();
  const per = lay.per === 'auto' ? 0 : lay.per;
  if (!per) return;
  const pages = Math.max(1, Math.ceil(orderedCams().length / per));
  page = wrap ? (p + pages) % pages : Math.min(Math.max(p, 0), pages - 1);
  renderLive();
  armCycle();
}

// ---------- cycle: rotate pages every N seconds ----------
function armCycle() {
  clearInterval(cycleTimer); cycleTimer = null;
  const p = Layouts.getPrefs();
  if (!p.cycle) return;
  cycleTimer = setInterval(() => {
    if (!isActive('live') || tlHook || arrange || fullCamId !== null || tvOne() !== null || document.hidden || !state.signedIn) return;
    const lay = Layouts.active();
    if (lay.per === 'auto') return;
    const pages = Math.ceil(orderedCams().length / lay.per);
    if (pages <= 1) return;
    page = (page + 1) % pages;
    renderLive();
  }, p.sec * 1000);
}

// ---------- zones ----------
function addZoneBox(frame, z, ignore) {
  const box = document.createElement('div');
  box.className = 'zbox' + (ignore ? ' ignore' : '');
  box.style.left = (z.x * 100) + '%';
  box.style.top = (z.y * 100) + '%';
  box.style.width = (z.w * 100) + '%';
  box.style.height = (z.h * 100) + '%';
  frame.appendChild(box);
}


// Keep a tile's picture layer over the picture (re-run when the tile or the
// stream changes size).
function fitPic(frame, pic) {
  if (!pic) return;
  const f = frame.getBoundingClientRect();
  if (!f.width || !f.height) return;
  const r = pictureRect(frame);
  pic.style.left = (r.left - f.left) + 'px';
  pic.style.top = (r.top - f.top) + 'px';
  pic.style.width = r.width + 'px';
  pic.style.height = r.height + 'px';
}
const picObserver = new ResizeObserver(entries => {
  for (const e of entries) fitPic(e.target, e.target.querySelector(':scope > .pic'));
});

// The part of an element that shows the picture (cameras that are not 16:9 are letterboxed).
function pictureRect(host) {
  const m = host.querySelector('video, img');
  const r = (m || host).getBoundingClientRect();
  const nw = m ? (m.videoWidth || m.naturalWidth) : 0;
  const nh = m ? (m.videoHeight || m.naturalHeight) : 0;
  if (!nw || !nh) return r;
  const sc = Math.min(r.width / nw, r.height / nh);
  const w = nw * sc, h = nh * sc;
  return { left: r.left + (r.width - w) / 2, top: r.top + (r.height - h) / 2, width: w, height: h };
}

// A drag (or click) with the pointer on a camera picture. Calls done({x,y}) for a click or
// done({x,y,w,h}) for a dragged box (center and size, all 0..1 of rect()).
function toolDrag(ev, kind, rect, done) {
  ev.preventDefault();
  const r0 = rect();
  if (!r0.width || !r0.height) return;
  const x0 = ev.clientX, y0 = ev.clientY;
  const cl = (v) => Math.min(1, Math.max(0, v));
  const norm = (e) => ({ x: cl((e.clientX - r0.left) / r0.width), y: cl((e.clientY - r0.top) / r0.height) });
  const box = document.createElement('div');
  box.className = 'dragbox ' + kind;
  box.hidden = true;
  document.body.appendChild(box);
  const move = (e) => {
    const moved = Math.abs(e.clientX - x0) > 6 || Math.abs(e.clientY - y0) > 6;
    box.hidden = !moved;
    box.style.left = Math.min(x0, e.clientX) + 'px';
    box.style.top = Math.min(y0, e.clientY) + 'px';
    box.style.width = Math.abs(e.clientX - x0) + 'px';
    box.style.height = Math.abs(e.clientY - y0) + 'px';
  };
  const end = (e, cancel) => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    window.removeEventListener('pointercancel', cancelUp);
    box.remove();
    lastDrag = performance.now();
    if (cancel) return;
    const a = norm(ev), b = norm(e);
    const moved = Math.abs(e.clientX - x0) > 6 || Math.abs(e.clientY - y0) > 6;
    if (!moved) done({ x: a.x, y: a.y });
    else done({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2, w: Math.abs(b.x - a.x), h: Math.abs(b.y - a.y) });
  };
  const up = (e) => end(e, false);
  const cancelUp = (e) => end(e, true);
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
  window.addEventListener('pointercancel', cancelUp);
}

function pingAt(x, y) {
  const d = document.createElement('div');
  d.className = 'aimping';
  d.style.left = x + 'px'; d.style.top = y + 'px';
  document.body.appendChild(d);
  setTimeout(() => d.remove(), 700);
}

// Click-to-aim: center the camera on a point, or zoom into a dragged box.
async function aimAt(id, spot, sx, sy) {
  pingAt(sx, sy);
  const body = spot.w !== undefined ? { x: spot.x, y: spot.y, w: spot.w, h: spot.h } : { x: spot.x, y: spot.y };
  try { await api('/api/cameras/' + id + '/ptz/aim', { json: body }); }
  catch (e) { toast('Aim failed: ' + (e.body || e.message)); }
}

// Tiles. A plain click selects the camera (the dock then shows its controls); double-click,
// Enter or the expand button opens it full size. Dragging on the picture does nothing unless
// Click to aim was chosen in the dock for this camera. (Zones are drawn in the zone editor.) Touch: a tap
// opens the camera full size, as before.
function attachTileInput(tile, frame, id) {
  let pt = 'mouse';
  tile.addEventListener('pointerdown', (ev) => { pt = ev.pointerType || 'mouse'; });
  frame.addEventListener('pointerdown', (ev) => {
    if (tlHook || !tool || id !== selId || ev.button !== 0) return;
    if (tool === 'aim') {
      toolDrag(ev, 'aim', () => pictureRect(frame), (spot) => aimAt(id, spot, ev.clientX, ev.clientY));
    }
  });
  tile.addEventListener('click', (ev) => {
    if (performance.now() - lastDrag < 400) return;
    if (tlHook) { if (frame.contains(ev.target)) { setSelected(id); tlHook.tileClick(id, ev); } return; }
    if (tool && id === selId) return;
    if (pt === 'touch') { openFullCam(id); return; }
    setSelected(id);
    if (oneClickOpens() && !ev.target.closest('button, a, input, select, .tileact, .cover')) openTileTarget(id);
  });
  tile.addEventListener('dblclick', (ev) => {
    if (tlHook) return;
    ev.preventDefault();
    if (oneClickOpens()) return;   // the first click already opened it
    openTileTarget(id);
  });
  tile.addEventListener('keydown', (ev) => {
    if (ev.target !== tile) return;
    if (ev.key === 'Enter') { openFullCam(id); ev.preventDefault(); }
    else if (ev.key === ' ') { setSelected(id); ev.preventDefault(); }
  });
  tile.addEventListener('focus', () => { if (!tlHook) setSelected(id); });
}

// ---------- full screen ----------
let fullCamId = null;
let fullPlayer = null;

let fullSpk = null;
let fullPills = null;   // corner pills + edge of the full view

export function stopFull() {
  if (fullSpk) { fullSpk.destroy(); fullSpk = null; }
  if (fullPlayer) { fullPlayer.stop(); fullPlayer = null; }
  clearInterval(window.__fullTimer);
  fullPills = null;
  const lc = $('lightContent');
  lc.innerHTML = '';
  delete lc.dataset.edge; delete lc.dataset.last; delete lc.dataset.cls;
}

function mountFull() {
  // A tile that was audible keeps its camera audible in the full view.
  const keepAudio = Speaker.activeCam() === fullCamId;
  stopFull();
  Zoom.reset();
  const content = $('lightContent');
  const video = document.createElement('video');
  video.id = 'fullVid'; video.muted = true; video.autoplay = true; video.playsInline = true;
  // (Its real ratio is known once the stream starts: the box follows it.)
  video.addEventListener('loadedmetadata', () => placeFull());
  video.addEventListener('resize', () => placeFull());
  content.appendChild(video);
  $('lightbox').dataset.cam = String(fullCamId);
  const c = camFromId(fullCamId);
  if (c && (c.overlay_text || '').trim() && !c.overlay) {
    const ot = document.createElement('div');
    ot.className = 'ovltext big pos-' + overlayPos(c);
    ot.dataset.camid = String(fullCamId);
    content.appendChild(ot);
    paintOverlayText(ot);
  }
  if (c) video.title = c.name;
  // Same pills and edge as the tiles (the name is in the toolbar above).
  if (c) {
    fullPills = buildPills(content, content, c, { name: false, retry: () => { if (fullCamId !== null) mountFull(); } });
    paintPills(fullPills, c);
  }
  $('fullName').textContent = c ? camLabel(c) : '';
  $('fullName').hidden = false;
  // Another server's camera: its recordings and clips are on that server.
  for (const b of ['fullTl', 'fullClips', 'fullClip']) $(b).hidden = !!(c && c.remote);
  fullSpk = Speaker.attach(content, { id: fullCamId, q: bigQ(), videos: () => [$('fullVid')] });
  if (keepAudio && c && c.audio) fullSpk.unmute();
  fullPlayer = startLive(video, fullCamId, bigQ(), () => {
    if (fullSpk) fullSpk.destroy();
    fullSpk = null;
    video.remove();
    const img = document.createElement('img');
    img.id = 'fullImg'; img.alt = c ? c.name : 'camera';
    img.addEventListener('load', () => placeFull(), { once: true });
    content.appendChild(img);
    Zoom.reset();
    refreshFullCam();
    window.__fullTimer = setInterval(refreshFullCam, 1000);
  });
}

export function toggleFullMute() {
  if (fullSpk) fullSpk.toggle();
}
export const fullIsOpen = () => fullCamId !== null;
export function fullSnapshot() { if (fullCamId !== null) snapshot(fullCamId); }
export function fullClip() { const c = camFromId(fullCamId); if (fullCamId !== null && !(c && c.remote)) clipNow(fullCamId); }
export function stepFullBy(d) { stepFull(d); }
export function fullZoom(k) { if (fullCamId === null) return; if (k === 0) Zoom.reset(); else Zoom.zoomBy(k); }
let fullAlert = null;
export const fullAlertAt = () => fullAlert;
// opts.alertAt (ms): the full view was opened from an alert at that time.
export function openFullCam(id, opts) {
  if (arrange) return;
  if (tvOn()) {
    if (tlHook) tlHook.solo(soloId === id ? null : id); else setOne(tvOne() === id ? null : id);
    setSelected(id);
    return;
  }
  fullAlert = (opts && opts.alertAt) || null;
  fullCamId = id;
  measureArea();
  document.body.classList.add('fullview');
  placeFull();
  setSelected(id);
  bus.emit('full', id);
  const lb = $('lightbox');
  lb.classList.add('cam');
  lb.style.display = 'flex';
  lb.dataset.cam = String(id);
  mountFull();
  placeFull();
  overlaysChanged();
}

function refreshFullCam() {
  if (fullCamId === null) return;
  const img = $('fullImg');
  if (!img) return;
  // MJPEG fallback only: cache-bust so the <img> shows the live snapshot stream.
  img.src = '/api/live/' + fullCamId + '?t=' + Date.now();
}

export function closeFull() {
  // A pop-out window shows only this camera: keep it (close the window instead).
  if (document.body.classList.contains('popout') && fullCamId !== null && state.signedIn) return;
  const wasOpen = fullCamId !== null;
  document.body.classList.remove('fullview');
  if (tool === 'aim' && wasOpen) setTool(null);
  const lb = $('lightbox');
  lb.style.display = 'none';
  lb.classList.remove('cam');
  placeFull();
  $('fullName').hidden = true;
  stopFull();
  Zoom.reset();
  fullCamId = null;
  if (wasOpen) bus.emit('full', null);
  overlaysChanged();
}

// Play a recorded event clip in the lightbox (used by the timeline).
export function playClipLightbox(id) {
  stopFull();
  fullCamId = null;
  const lb = $('lightbox');
  lb.classList.remove('cam');
  placeFull();
  $('fullName').hidden = true;
  const v = document.createElement('video');
  v.src = '/api/clips/' + id + lowQ(true);
  v.controls = true;
  v.autoplay = true;
  v.playsInline = true;
  $('lightContent').appendChild(v);
  lb.style.display = 'flex';
}

function stepFull(delta) {
  if (fullCamId === null) return;
  const ids = order.length ? order : state.cams.map(c => c.id);
  if (!ids.length) return;
  const i = ids.indexOf(fullCamId);
  if (ids.length < 2) return;
  fullCamId = ids[(i + delta + ids.length) % ids.length];
  fullAlert = null;
  setSelected(fullCamId);
  bus.emit('full', fullCamId);
  mountFull();
}

function initFull() {
  $('closeLight').onclick = () => closeFull();
  $('fullPrev').onclick = () => stepFull(-1);
  $('fullNext').onclick = () => stepFull(1);
  $('fullSnap').onclick = fullSnapshot;
  $('fullClip').onclick = fullClip;
  $('fullClips').onclick = () => { const c = camFromId(fullCamId); if (c) openClipsFor(c); };
  // As F does: leave the full view for full screen of this camera alone.
  $('fullFs').onclick = async () => { if (fullCamId !== null) (await import('./shortcuts.js')).fullToTv(); };
  $('fullMenu').dataset.menu = '1';
  $('fullMenu').onclick = (ev) => { const c = camFromId(fullCamId); if (c) tileMenu(ev, c, $('fullMenu')); };
  $('lightContent').addEventListener('contextmenu', (ev) => { const c = camFromId(fullCamId); if (c && ev.target.closest('video, img')) tileMenu(ev, c); });
  window.addEventListener('keydown', (ev) => {
    if ($('lightbox').style.display !== 'flex') return;
    if (ev.key === 'Escape') { escTs = ev.timeStamp; if (tool) setTool(null); else closeFull(); return; }
    if (fullCamId === null || overlayOpen(ev)) return;
    const tg = ev.target;
    if (tg && (/^(INPUT|SELECT|TEXTAREA)$/.test(tg.tagName) || tg.isContentEditable)) return;
    if (tg && tg.closest && tg.closest('#dock, .cs-drawer, .ze, .cp-scrim')) return;
    if (ev.key === 'ArrowRight' || ev.key === 'ArrowDown') stepFull(1);
    else if (ev.key === 'ArrowLeft' || ev.key === 'ArrowUp') stepFull(-1);
  });
  // Swipe left = next camera, swipe right = previous.
  let sx = 0, sy = 0, st = 0;
  const lb = $('lightbox');
  // Click / tap on the empty black space around the video closes the view (not after a drag or swipe).
  let down = null;
  lb.addEventListener('pointerdown', (ev) => { down = { x: ev.clientX, y: ev.clientY }; });
  lb.addEventListener('click', (ev) => {
    const moved = down && (Math.abs(ev.clientX - down.x) > 8 || Math.abs(ev.clientY - down.y) > 8);
    down = null;
    if (moved) return;
    if (ev.target === lb || ev.target === $('lightContent')) closeFull();
  });
  lb.addEventListener('touchstart', (ev) => {
    if (ev.touches.length !== 1) { st = 0; return; }
    sx = ev.touches[0].clientX; sy = ev.touches[0].clientY; st = Date.now();
  }, { passive: true });
  // Digital zoom: wheel / pinch / double-click on the picture; drag pans (not while aiming).
  Zoom.initZoom($('lightContent'), () => tool !== 'aim' && fullCamId !== null);
  // Click to aim in full size: click a point or drag a box on the picture.
  $('lightContent').addEventListener('pointerdown', (ev) => {
    if (tool !== 'aim' || fullCamId === null || ev.button !== 0 || !ev.target.closest('video, img')) return;
    const id = fullCamId;
    toolDrag(ev, 'aim', () => pictureRect($('lightContent')), (spot) => aimAt(id, spot, ev.clientX, ev.clientY));
  });
  lb.addEventListener('touchend', (ev) => {
    if (!st || fullCamId === null || tool || Zoom.isZoomed()) return;
    const t = ev.changedTouches[0];
    const dx = t.clientX - sx, dy = t.clientY - sy;
    const fast = Date.now() - st < 800;
    st = 0;
    if (fast && Math.abs(dx) > 50 && Math.abs(dx) > Math.abs(dy) * 1.5) stepFull(dx < 0 ? 1 : -1);
  }, { passive: true });
}

// ---------- tab wiring ----------
function syncCycleUi() {
  const p = Layouts.getPrefs();
  $('cycleOn').checked = !!p.cycle;
  $('cycleSec').value = String(p.sec);
  $('cycleSec').hidden = !p.cycle;
}

export function initLive() {
  initFull();
  // Split view: offered once another server's cameras are shown here.
  const splitBtn = $('splitBtn');
  if (splitBtn) {
    splitBtn.onclick = () => { location.href = '/?split=1'; };
    bus.on('cameras', () => { splitBtn.hidden = document.body.classList.contains('pane') || !(state.allCams || []).some(c => c.remote); });
  }
  $('groupFilter').onchange = () => { bus.emit('view-pick'); page = 0; if (arrange) finishArrange(false); renderLive(); };
  $('pagePrev').onclick = () => setPage(page - 1, true);
  $('pageNext').onclick = () => setPage(page + 1, true);
  $('layoutEdit').onclick = (ev) => layoutsMenu(ev, $('layoutEdit'));
  $('arrangeBtn').onclick = () => { if (arrange) finishArrange(true); else startArrange(); };
  $('arrangeCancel').onclick = () => finishArrange(false);
  for (const b of $('viewStyle').querySelectorAll('button')) b.onclick = () => {
    setViewStyle(b.dataset.style);
    // (A touch screen shows no tooltips: say what the style does.)
    if (document.documentElement.dataset.touch === '1') toast(b.dataset.tip || b.title.split(' (')[0]);
  };
  // Window-filling styles follow the window and the panel (dock open/closed/side).
  let fitRaf = 0;
  const refit = () => { cancelAnimationFrame(fitRaf); fitRaf = requestAnimationFrame(() => {
    if (!isActive('live')) return;
    const want = liveStyle();
    if (($('camGrid').dataset.style || 'grid') !== want) { liveSig = null; renderLive(); } else fitGrid();
  }); };
  window.addEventListener('resize', refit);
  window.addEventListener('ambs-tlctl', refit);
  // The full view's box follows the window and the panel beside it.
  new ResizeObserver(() => { if (fullCamId !== null) placeFull(); }).observe($('lightbox'));
  new ResizeObserver(refit).observe($('camGrid').parentElement);
  if ($('tlDock')) new ResizeObserver(refit).observe($('tlDock'));
  $('cycleOn').onchange = () => { Layouts.setPrefs({ cycle: $('cycleOn').checked }); };
  $('cycleSec').onchange = () => { Layouts.setPrefs({ sec: parseInt($('cycleSec').value, 10) }); };
  let lastQuality = null;
  bus.on('layouts-prefs', () => {
    syncCycleUi();
    armCycle();
    const q = Layouts.getPrefs().quality;
    if (lastQuality !== null && q !== lastQuality) { liveSig = null; renderLive(); }
    lastQuality = q;
  });
  bus.on('layouts', () => { page = 0; if (arrange) finishArrange(false); renderLive(); armCycle(); });
  bus.on('cameras', () => { camsRx = Date.now(); renderLive(); const fc = fullCamId !== null && camFromId(fullCamId); if (fc && fullPills) paintPills(fullPills, fc); });
  // Esc: stop arranging, else drop the tool, else clear the selection.
  window.addEventListener('keydown', (ev) => {
    if (ev.key !== 'Escape' || ev.defaultPrevented || ev.timeStamp === escTs || !isActive('live') || fullCamId !== null) return;
    if (document.querySelector('.dlg')) return;
    if (arrange) finishArrange(false);
    else if (tool) setTool(null);
    else if (selId !== null) setSelected(null);
  });
  // Hidden browser tab: drop the streams (saves bandwidth and camera sessions), restart on return.
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) { if (isActive('live') && fullCamId === null) stopTiles(); }
    else if (state.signedIn && isActive('live')) { liveSig = null; renderLive(); }
  });
  bus.on('signedout', () => { if (arrange) finishArrange(false); selId = null; hoverId = null; setTool(null); });
  onTvChange(() => { liveSig = null; renderLive(); requestAnimationFrame(() => requestAnimationFrame(() => { if (tvOn()) fitGrid(); })); });
  document.addEventListener('fullscreenchange', () => requestAnimationFrame(() => { if (tvOn()) fitGrid(); }));
  const fb = $('fsBtn');
  if (fb) fb.onclick = () => enterTv();
  syncCycleUi();
  armCycle();
}

export function showLive() { liveSig = null; renderLive(); bus.emit('live-visible', true); }
export function hideLive() { document.body.classList.remove('livefill'); if (arrange) finishArrange(false); stopTiles(); $('camGrid').innerHTML = ''; bus.emit('live-visible', false); }
