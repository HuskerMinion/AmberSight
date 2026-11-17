// Tests for static/js/fit.js (where a picture sits in its box). Run: node scripts/test-fit.mjs
// (scripts/check-js.sh runs it too.) No framework: a failed check prints and exits 1.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, '..', 'static', 'js', 'fit.js'), 'utf8');
const M = await import('data:text/javascript;base64,' + Buffer.from(src).toString('base64'));

let n = 0, bad = 0;
const eq = (got, want, what) => {
  n++;
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { bad++; console.log('FAIL ' + what + '\n  got  ' + g + '\n  want ' + w); }
};
const R = (left, top, width, height) => ({ left, top, width, height });
const rd = (r) => ({ left: +r.left.toFixed(6), top: +r.top.toFixed(6), width: +r.width.toFixed(6), height: +r.height.toFixed(6) });


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
// ---- the chain the text overlay runs: letterboxed video, text on the picture, not on the bars ----
eq(M.fitRect(1280, 720, 640, 480, 'contain'), R(160, 0, 960, 720), 'chain: 4:3 camera in a 16:9 tile');
eq(M.fitRect(390, 700, 2560, 1440, 'contain'), R(0, 240.3125, 390, 219.375), 'chain: wide camera in a phone box');

console.log(bad ? 'fit: ' + bad + ' of ' + n + ' checks FAILED' : 'fit: all ' + n + ' checks passed');
process.exit(bad ? 1 : 0);
