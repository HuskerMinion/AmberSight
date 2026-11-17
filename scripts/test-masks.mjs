// Tests for static/js/masks.js (privacy-mask geometry). Run: node scripts/test-masks.mjs
// (scripts/check-js.sh runs it too.) No framework: a failed check prints and exits 1.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, '..', 'static', 'js', 'masks.js'), 'utf8');
const M = await import('data:text/javascript;base64,' + Buffer.from(src).toString('base64'));

let n = 0, bad = 0;
const eq = (got, want, what) => {
  n++;
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { bad++; console.log('FAIL ' + what + '\n  got  ' + g + '\n  want ' + w); }
};
const R = (left, top, width, height) => ({ left, top, width, height });
const rd = (r) => ({ left: +r.left.toFixed(6), top: +r.top.toFixed(6), width: +r.width.toFixed(6), height: +r.height.toFixed(6) });

// ---- maskList: clean 0..1 rectangles ----
eq(M.maskList(null), [], 'no camera');
eq(M.maskList({}), [], 'no masks');
eq(M.maskList({ privacy_masks: [{ x: 0.1, y: 0.2, w: 0.3, h: 0.4 }] }), [{ x: 0.1, y: 0.2, w: 0.3, h: 0.4 }], 'one mask');
eq(M.maskList({ privacy_masks: [{ x: -0.5, y: 0.5, w: 1, h: 1 }] }), [{ x: 0, y: 0.5, w: 0.5, h: 0.5 }], 'clamped to the picture');
eq(M.maskList({ privacy_masks: [{ x: 0.2, y: 0.2, w: 0, h: 0.5 }, null, { x: 'a', y: 0, w: 1, h: 1 }, { x: 0.5, y: 0.5, w: 0.1, h: 0.1 }] }), [{ x: 0.5, y: 0.5, w: 0.1, h: 0.1 }], 'empty and junk dropped');
eq(M.maskList({ privacy_masks: [{ x: 1, y: 0, w: 0.5, h: 1 }] }), [], 'outside the picture');

// ---- fitRect: where the picture is in its box ----
// contain, 16:9 video in a 4:3 box: bars top and bottom.
eq(M.fitRect(800, 600, 1920, 1080, 'contain'), R(0, 75, 800, 450), 'contain, wide video, tall box');
// contain, 4:3 video in a 16:9 box: bars left and right.
eq(M.fitRect(1600, 900, 640, 480, 'contain'), R(200, 0, 1200, 900), 'contain, 4:3 video, wide box');
// contain, same ratio: the whole box.
eq(M.fitRect(1280, 720, 1920, 1080, 'contain'), R(0, 0, 1280, 720), 'contain, same ratio');
// portrait video (doorbell 3:4) in a 16:9 box.
eq(M.fitRect(1600, 900, 1536, 2048, 'contain'), R(462.5, 0, 675, 900), 'contain, portrait video');
// cover: the picture overflows the box, centered.
eq(rd(M.fitRect(800, 600, 1920, 1080, 'cover')), R(-133.333333, 0, 1066.666667, 600), 'cover, wide video');
// fill: stretched over the box.
eq(M.fitRect(800, 600, 1920, 1080, 'fill'), R(0, 0, 800, 600), 'fill');
// none: natural size, centered.
eq(M.fitRect(800, 600, 400, 300, 'none'), R(200, 150, 400, 300), 'none, small video');
// scale-down: like none when smaller, like contain when bigger.
eq(M.fitRect(800, 600, 400, 300, 'scale-down'), R(200, 150, 400, 300), 'scale-down, small');
eq(M.fitRect(800, 600, 1920, 1080, 'scale-down'), R(0, 75, 800, 450), 'scale-down, big');
// object-position.
eq(M.fitRect(1600, 900, 640, 480, 'contain', '0% 0%'), R(0, 0, 1200, 900), 'position 0% 0%');
eq(M.fitRect(1600, 900, 640, 480, 'contain', '100% 50%'), R(400, 0, 1200, 900), 'position 100%');
eq(M.fitRect(1600, 900, 640, 480, 'contain', '10px 0px'), R(10, 0, 1200, 900), 'position px');
eq(M.fitRect(1600, 900, 640, 480, 'contain', 'right top'), R(400, 0, 1200, 900), 'position keywords');
eq(M.fitRect(1600, 900, 640, 480, 'contain', '25%'), R(100, 0, 1200, 900), 'one value: the other is center');
// size not known yet (no metadata): the whole box; empty box: nothing.
eq(M.fitRect(800, 600, 0, 0, 'contain'), R(0, 0, 800, 600), 'unknown video size');
eq(M.fitRect(0, 600, 1920, 1080, 'contain'), R(0, 0, 0, 0), 'empty box');
// no object-fit reported: contain (a video's default).
eq(M.fitRect(1600, 900, 640, 480, ''), R(200, 0, 1200, 900), 'no fit given = contain');

// ---- posOffset ----
eq([M.posOffset('50%', 100), M.posOffset('left', 100), M.posOffset('bottom', 100), M.posOffset('center', 100), M.posOffset('7px', 100), M.posOffset('junk', 100)], [50, 0, 100, 50, 7, 50], 'position values');
eq(M.posOffset('50%', -200), -100, 'cover: negative free space');

// ---- maskBoxes: px boxes, edges moved outward to whole pixels ----
eq(M.maskBoxes(R(0, 0, 1000, 500), [{ x: 0.1, y: 0.2, w: 0.3, h: 0.4 }]), [R(100, 100, 300, 200)], 'whole pixels already');
eq(M.maskBoxes(R(0.5, 0.5, 333, 333), [{ x: 0.5, y: 0.5, w: 0.25, h: 0.25 }]), [R(167, 167, 84, 84)], 'fractional edges go outward');
{
  // Outward means the box always covers the exact rectangle.
  const r = R(13.3, 7.7, 641.9, 359.1), m = { x: 0.123, y: 0.456, w: 0.321, h: 0.2 };
  const [b] = M.maskBoxes(r, [m]);
  const ex = { l: r.left + m.x * r.width, t: r.top + m.y * r.height, r: r.left + (m.x + m.w) * r.width, b: r.top + (m.y + m.h) * r.height };
  eq([b.left <= ex.l, b.top <= ex.t, b.left + b.width >= ex.r, b.top + b.height >= ex.b], [true, true, true, true], 'covers the exact area');
  eq([b.left > ex.l - 1, b.top > ex.t - 1, b.left + b.width < ex.r + 1, b.top + b.height < ex.b + 1], [true, true, true, true], 'by less than a pixel more');
}
eq(M.maskBoxes(R(0, 0, 100, 100), []), [], 'no masks, no boxes');

// ---- the chain a player runs: letterboxed video, mask on the picture, not on the bars ----
{
  // 4:3 camera in a 16:9 tile, 1280x720: the picture is 960x720 at left 160.
  const r = M.fitRect(1280, 720, 640, 480, 'contain');
  eq(r, R(160, 0, 960, 720), 'chain: picture rect');
  // The camera's left half masked: the box starts at the picture's edge, not the tile's.
  eq(M.maskBoxes(r, M.maskList({ privacy_masks: [{ x: 0, y: 0, w: 0.5, h: 1 }] })), [R(160, 0, 480, 720)], 'chain: left half of the picture');
  // The whole picture masked: exactly the picture, the bars stay as they are.
  eq(M.maskBoxes(r, [{ x: 0, y: 0, w: 1, h: 1 }]), [R(160, 0, 960, 720)], 'chain: whole picture');
}
{
  // Wide camera in a tall phone box (16:9 in 390x700): bars above and below.
  const r = M.fitRect(390, 700, 2560, 1440, 'contain');
  eq(M.maskBoxes(r, [{ x: 0.75, y: 0.5, w: 0.25, h: 0.5 }]), [R(292, 350, 98, 110)], 'chain: phone, corner mask');
}

// ---- paintMasks: into a grabbed frame ----
{
  const calls = [];
  const ctx = { save() {}, restore() {}, set fillStyle(v) { calls.push('fill ' + v); }, fillRect(...a) { calls.push(a); } };
  M.paintMasks(ctx, 640, 360, { privacy_masks: [{ x: 0.5, y: 0, w: 0.5, h: 0.5 }] });
  eq(calls, ['fill #000', [320, 0, 320, 180]], 'paintMasks');
  const none = [];
  M.paintMasks({ save() { none.push(1); }, restore() {}, fillRect() { none.push(2); } }, 640, 360, { privacy_masks: [] });
  eq(none, [], 'paintMasks: nothing without masks');
}

// ---- maskRevQ: the masks' revision in cached picture addresses ----
eq(M.maskRevQ({ mask_rev: 'a1b2c3' }), '&m=a1b2c3', 'maskRevQ');
eq(M.maskRevQ({ mask_rev: 'a b' }, '?'), '?m=a%20b', 'maskRevQ: first parameter, escaped');
eq([M.maskRevQ({ mask_rev: '' }), M.maskRevQ({}), M.maskRevQ(null), M.maskRevQ(undefined, '?')], ['', '', '', ''], 'maskRevQ: nothing without masks');

// A linked server's camera (no mask_rev): a revision from its masks, new when they change.
{
  const lc = { id: 'r1', privacy_masks: [{ x: 0.1, y: 0.1, w: 0.2, h: 0.2 }], mask_mode: 'screen' };
  const q = M.maskRevQ(lc);
  eq(/^&m=c[0-9a-f]{8}$/.test(q), true, 'maskRevQ: made from the masks without mask_rev');
  eq(M.maskRevQ({ ...lc }) === q, true, 'maskRevQ: made one is stable');
  eq(M.maskRevQ({ ...lc, privacy_masks: [{ x: 0.1, y: 0.1, w: 0.3, h: 0.2 }] }) === q, false, 'maskRevQ: made one follows the masks');
  eq(M.maskRevQ({ ...lc, mask_mode: 'recording' }) === q, false, 'maskRevQ: made one follows the level');
  // All cameras as one (plate pictures).
  const all = M.allMasksRevQ([{ id: 1 }, { id: 2, mask_rev: 'aa' }]);
  eq(/^\?m=[0-9a-f]{8}$/.test(all), true, 'allMasksRevQ');
  eq(M.allMasksRevQ([{ id: 2, mask_rev: 'aa' }, { id: 1 }]) === all, true, 'allMasksRevQ: order does not matter');
  eq(M.allMasksRevQ([{ id: 1 }, { id: 2, mask_rev: 'bb' }]) === all, false, 'allMasksRevQ: follows a change');
  eq([M.allMasksRevQ([{ id: 1 }]), M.allMasksRevQ([]), M.allMasksRevQ(null)], ['', '', ''], 'allMasksRevQ: nothing without masks');
}

// ---- noFfmpegReader: the server's rule (the address only) ----
eq(['push://door', ' PUSH://door', 'baichuan://192.0.2.9?uid=X', 'rtsp://192.0.2.9/', '', null, 'BAICHUAN://192.0.2.9'].map(M.noFfmpegReader),
  [true, true, true, false, false, false, false], 'noFfmpegReader');

// ---- guardVideo: the browser's own ways out are turned off ----
{
  // A small stand-in for a <video controls> in a box (no DOM in node).
  const fakeEl = (tag) => {
    const attrs = new Map(), evs = {}, kids = [];
    return {
      tagName: tag, dataset: {}, classList: { set: new Set(), add(c) { this.set.add(c); } }, kids, evs, attrs,
      setAttribute(k, v) { attrs.set(k, String(v)); }, hasAttribute(k) { return attrs.has(k); },
      addEventListener(t, f) { (evs[t] = evs[t] || []).push(f); },
      appendChild(c) { kids.push(c); c.parentElement = this; return c; },
      querySelector() { return kids.find(k => k.className === 'pmask-fs') || null; },
      requestFullscreen() { this.fs = (this.fs || 0) + 1; return Promise.resolve(); },
    };
  };
  globalThis.document = { fullscreenElement: null, exitFullscreen() { this.exits = (this.exits || 0) + 1; return Promise.resolve(); }, createElement: (t) => fakeEl(t.toUpperCase()) };
  const box = fakeEl('DIV'), v = fakeEl('VIDEO');
  box.appendChild(v);
  v.setAttribute('controls', '');
  M.guardVideo(v);
  eq([v.attrs.get('controlslist'), v.attrs.has('disablepictureinpicture'), v.attrs.has('disableremoteplayback'), v.attrs.get('x-webkit-airplay'), v.attrs.has('playsinline'), v.attrs.has('webkit-playsinline')],
    ['nodownload nofullscreen noremoteplayback', true, true, 'deny', true, true], 'guardVideo: attributes');
  eq([v.disablePictureInPicture, v.disableRemotePlayback, v.playsInline], [true, true, true], 'guardVideo: properties');
  let stopped = false;
  v.evs.contextmenu[0]({ preventDefault() { stopped = true; } });
  eq(stopped, true, 'guardVideo: no context menu (Save video as)');
  const btns = box.kids.filter(k => k.className === 'pmask-fs');
  eq(btns.length, 1, 'guardVideo: our full-screen button on the box');
  btns[0].onclick({ stopPropagation() {} });
  eq(box.fs, 1, 'guardVideo: the button puts the box (video and masks) in full screen');
  // The video itself in full screen (a double click, Firefox's controls): straight back.
  document.fullscreenElement = v;
  v.evs.fullscreenchange[0]();
  eq(document.exits, 1, 'guardVideo: the video alone leaves full screen');
  M.guardVideo(v);
  eq(box.kids.filter(k => k.className === 'pmask-fs').length, 1, 'guardVideo: once');
  delete globalThis.document;
}

console.log(bad ? 'masks: ' + bad + ' of ' + n + ' checks FAILED' : 'masks: all ' + n + ' checks passed');
process.exit(bad ? 1 : 0);
