// player.js - the live players, shared by Live (live.js), the timeline, the speaker and display
// mode (display.js): fragmented MP4 over a WebSocket into Media Source Extensions (startLive),
// and the light player, JPEG pictures over a WebSocket (startJpeg). Both reconnect by themselves
// and free everything they made (sockets, object URLs, listeners) when stopped, so a page can
// run them for weeks. Imports only small modules, so a page of its own can use it.

import { api, sleep, playVideo } from './core.js';
import { camStep, camErr, camCount, mark } from './diag.js';
import { lowDelayInit, hasCompositionOffsets, videoTimescale, Dejitter } from './lowdelay.js';

// Timing for 'page load -> first live frame' (read by tests via window.__ambs).
window.__ambs = window.__ambs || {};
export function markFirstFrame() {
  mark('firstFrame');
  if (window.__ambs.firstFrameMs === undefined) window.__ambs.firstFrameMs = Math.round(performance.now());
}

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
  const st = { stopped: false, ctrl: null, url: null, first: true, tries: 0, cur: null, now: false, errs: 0, lastErr: 0, wait: 0, ac: null };
  // (Listeners on the video are dropped with the session or the player that added them: a
  // player that reconnects for weeks must not pile them up on the same element.)
  const life = new AbortController();
  // A muted picture asks for the stream without sound: less data, and no audio decoder to fail.
  // When the picture is unmuted later (sound on), the stream starts again with sound.
  video.addEventListener('volumechange', () => {
    if (st.stopped || !st.cur || !st.cur.quiet || st.cur.noAudio || video.muted) return;
    st.now = true; st.cur.sock.close();
  }, { signal: life.signal });
  const cleanup = () => {
    if (st.ctrl) { try { st.ctrl.close(); } catch (e) { /* already closed */ } st.ctrl = null; }
    if (st.url) { URL.revokeObjectURL(st.url); st.url = null; }
    if (st.ac) { st.ac.abort(); st.ac = null; }
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
    const other = (x) => (x === 'sub' ? 'main' : 'sub');
    const playable = (x) => !!(x && MSEClass && MSEClass.isTypeSupported(x.video_mime || x.mime));
    let wq = info[q] ? q : other(q);
    // A browser that cannot play this stream's video (H.265 in Firefox and in
    // Chrome on most Linux machines) plays the camera's other stream when it
    // can, rather than falling back to pictures.
    if (info[wq] && !playable(info[wq]) && playable(info[other(wq)])) wq = other(wq);
    const s = info[wq];
    // A battery camera that is still waking up: ask again shortly (not a failure).
    if (!s && info.waking) return 'waking';
    const key = camId + '/' + wq;
    // Without sound when it is not wanted (muted), not playable here, or failed before.
    // (Not playable: starting again cannot bring it back, so the speaker says so instead.)
    const noAudio = !!(s && s.audio && s.video_mime && (badAudio.has(key) || !(MSEClass && MSEClass.isTypeSupported(s.mime))));
    const quiet = noAudio || !!(s && s.audio && s.video_mime && video.muted);
    if (noAudio) badAudio.add(key);
    if (noAudio && !video.muted) window.dispatchEvent(new CustomEvent('ambersight:nosound', { detail: { cam: camId } }));
    const mime = quiet ? s.video_mime : (s && s.mime);
    if (!s || !MSEClass || !MSEClass.isTypeSupported(mime)) return st.first ? 'fallback' : 'retry';
    camStep(camId, 'info', { q: wq, size: s.width ? s.width + 'x' + s.height : '?' });
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
    const sock = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/api/live/' + camId + '/ws?q=' + wq + (quiet ? '&audio=0' : ''));
    sock.binaryType = 'arraybuffer';
    st.ctrl = sock;
    st.cur = { sock, quiet, noAudio };
    sock.addEventListener('open', () => camStep(camId, 'open'));
    const ac = st.ac = new AbortController();
    video.addEventListener('playing', () => camStep(camId, 'play'), { once: true, signal: ac.signal });
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
    }, { once: true, signal: ac.signal });
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
  return { stop() { st.stopped = true; cleanup(); life.abort(); video.removeAttribute('src'); try { video.load(); } catch (e) { /* ignore */ } } };
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
