// diag.js - start-up timings of this page, sent once to the server's log
// (POST /api/ui/diag) about 25 s after the page opens: when the page and its
// scripts arrived, and for each live tile when its stream was asked for,
// connected, sent data and started playing, plus any failure. For finding
// out why the start page is slow on a given device; nothing personal.

const ms = () => Math.round(performance.now());
const D = { boot: ms(), marks: {}, cams: {} };
const cam = (id) => (D.cams[id] = D.cams[id] || {});

// A step of the page (first time only).
export function mark(name) { if (!(name in D.marks)) D.marks[name] = ms(); }
// A step of one camera's stream (first time only), with optional details.
export function camStep(id, step, extra) { const c = cam(id); if (!(step in c)) c[step] = ms(); if (extra) Object.assign(c, extra); }
// Something went wrong for one camera (the first few kept).
export function camErr(id, what) { const c = cam(id); c.err = c.err || []; if (c.err.length < 4) c.err.push(what + '@' + ms()); }
export function camCount(id, key) { const c = cam(id); c[key] = (c[key] || 0) + 1; }

function send() {
  try {
    const n = performance.getEntriesByType('navigation')[0] || {};
    const res = performance.getEntriesByType('resource');
    const scripts = res.filter(r => /\/static\//.test(r.name));
    const con = navigator.connection || {};
    const body = {
      ua: navigator.userAgent,
      screen: [screen.width, screen.height, window.innerWidth, window.innerHeight, window.devicePixelRatio],
      device: document.documentElement.dataset.device, cores: navigator.hardwareConcurrency, mem: navigator.deviceMemory,
      net: con.effectiveType ? [con.effectiveType, con.rtt, con.downlink] : undefined,
      proto: n.nextHopProtocol,
      page: { ttfb: Math.round(n.responseStart || 0), dom: Math.round(n.domContentLoadedEventEnd || 0), load: Math.round(n.loadEventEnd || 0) },
      files: { n: scripts.length, kb: Math.round(scripts.reduce((a, r) => a + (r.transferSize || 0), 0) / 1024), last: Math.round(Math.max(0, ...scripts.map(r => r.responseEnd))) },
      // The first requests to the server: path, start, how long (ms).
      api: res.filter(r => /\/api\//.test(r.name) && !/\/api\/(snapshot|live)\//.test(r.name)).slice(0, 30).map(r => [r.name.replace(/^.*\/api/, '').slice(0, 40), Math.round(r.startTime), Math.round(r.duration)]),
      boot: D.boot, marks: D.marks, cams: D.cams,
    };
    fetch('/api/ui/diag', { method: 'POST', credentials: 'same-origin', keepalive: true, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }).catch(() => {});
  } catch (e) { /* diagnostics only */ }
}
setTimeout(send, 25000);

// Sound tripwire: any video that plays with its sound on is reported at once
// (which camera, which view), so stray sound can be traced. At most 5 a page.
let sounds = 0;
function heard(ev) {
  const v = ev.target;
  if (!(v instanceof HTMLMediaElement) || v.muted || v.volume === 0 || v.paused || sounds >= 5) return;
  sounds++;
  const tile = v.closest('.tile');
  const body = { sound: true, at: ms(), view: location.hash, cls: v.className, tile: tile ? tile.dataset.id : null,
    full: !!v.closest('#lightbox'), timeline: !!v.closest('.tlp'), ua: navigator.userAgent };
  try { fetch('/api/ui/diag', { method: 'POST', credentials: 'same-origin', keepalive: true, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }).catch(() => {}); } catch (e) { /* diagnostics only */ }
}
document.addEventListener('playing', heard, true);
document.addEventListener('volumechange', heard, true);
