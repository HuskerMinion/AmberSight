// Tests for static/js/display_kit.js (display mode's pure parts). Run: node scripts/test-display.mjs
// (scripts/check-js.sh runs it too.) No framework: a failed check prints and exits 1.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, '..', 'static', 'js', 'display_kit.js'), 'utf8');
const K = await import('data:text/javascript;base64,' + Buffer.from(src).toString('base64'));

let n = 0, bad = 0;
const eq = (got, want, what) => {
  n++;
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { bad++; console.log('FAIL ' + what + '\n  got  ' + g + '\n  want ' + w); }
};

// Settings: junk falls back to the defaults, numbers are held to their range.
eq(K.cleanSettings(null), K.DEFAULTS, 'defaults');
eq(K.cleanSettings({ trigger: 'x', hold: '9999', grid: 'sub', dimFrom: '25:00', source: 'layout:' }).trigger, 'alerts', 'bad trigger');
eq(K.cleanSettings({ hold: '9999' }).hold, 600, 'hold capped');
eq(K.cleanSettings({ hold: '1' }).hold, 5, 'hold floor');
eq(K.cleanSettings({ dimFrom: '25:00' }).dimFrom, '22:00', 'bad time');
eq(K.cleanSettings({ source: 'layout:' }).source, 'all', 'empty layout id');
eq(K.cleanSettings({ source: 'group:Outside' }).source, 'group:Outside', 'group kept');

// Which cameras.
const cams = [{ id: 1, name: 'A', groups: ['Out'] }, { id: 2, name: 'B' }, { id: 3, name: 'C', group: 'Out' }, { id: 4, name: 'D', groups: ['In'] }];
const ids = (l) => l.map(c => c.id);
const S = (o) => K.cleanSettings(o);
eq(ids(K.chooseCams(S({}), cams, [], {})), [1, 2, 3, 4], 'all');
eq(ids(K.chooseCams(S({}), cams, [], { all: ['D', 'C'] })), [4, 3, 1, 2], 'all in the shared order');
eq(ids(K.chooseCams(S({ source: 'group:Out' }), cams, [], { 'group:Out': ['C'] })), [3, 1], 'group, its order');
eq(ids(K.chooseCams(S({ source: 'layout:l1' }), cams, [{ id: 'l1', name: 'x', cams: ['D', 'B', 'gone'] }], {})), [4, 2], 'layout: its cameras');
eq(ids(K.chooseCams(S({ source: 'layout:l2' }), cams, [{ id: 'l2', name: 'x', cams: [], group: 'Out' }], {})), [1, 3], 'layout following a group');
eq(ids(K.chooseCams(S({ source: 'layout:nope' }), cams, [], {})), [1, 2, 3, 4], 'a layout that is gone: all');
eq(ids(K.chooseCams(S({ source: 'pick', cams: ['C', 'A'] }), cams, [], {})), [3, 1], 'picked');
const many = Array.from({ length: 30 }, (_, i) => ({ id: i, name: 'c' + i }));
eq(K.chooseCams(S({}), many, [], {}).length, K.MAX_TILES, 'at most 16');

// Grid fit.
eq(K.fitGrid(4, 1920, 1080), { cols: 2, rows: 2 }, '4 on a TV');
eq(K.fitGrid(1, 1920, 1080), { cols: 1, rows: 1 }, 'one');
eq(K.fitGrid(3, 1920, 1080), { cols: 2, rows: 2 }, '3 on a TV');
eq(K.fitGrid(2, 1920, 1080), { cols: 2, rows: 1 }, '2 side by side');
eq(K.fitGrid(2, 800, 1280), { cols: 1, rows: 2 }, '2 on an upright tablet');
eq(K.fitGrid(9, 1920, 1080), { cols: 3, rows: 3 }, '9');
eq(K.fitGrid(16, 1920, 1080), { cols: 4, rows: 4 }, '16');

// Dimming window.
eq([K.inWindow(23 * 60, '22:00', '07:00'), K.inWindow(3 * 60, '22:00', '07:00'), K.inWindow(12 * 60, '22:00', '07:00'), K.inWindow(7 * 60, '22:00', '07:00')], [true, true, false, false], 'overnight window');
eq([K.inWindow(13 * 60, '12:00', '14:00'), K.inWindow(14 * 60, '12:00', '14:00'), K.inWindow(0, '08:00', '08:00')], [true, false, false], 'day window, empty window');

// What sends a camera big.
const now = 1e12;
eq(K.triggerOf({ ai_last: { label: 'person', age_ms: 1000 } }, 'alerts', now, 0), { cls: 'person', text: 'Person' }, 'AI person');
eq(K.triggerOf({ ai_last: { label: 'person', age_ms: 9000 } }, 'alerts', now, 0), null, 'old AI, no motion');
eq(K.triggerOf({ ai_last: { label: 'car', age_ms: 60000 }, motion: true }, 'alerts', now, 0), { cls: 'vehicle', text: 'Car' }, 'AI during the same motion');
eq(K.triggerOf({ motion: true, ai_active: true }, 'alerts', now, 0), null, 'motion alone is not an alert');
eq(K.triggerOf({ motion: true }, 'alerts', now, 0), null, 'a server that does not say: no fallback');
eq(K.triggerOf({ motion: true, ai_active: false }, 'alerts', now, 0), { cls: 'motion', text: 'Motion' }, 'no AI on this camera: its motion');
eq(K.triggerOf({ motion: false, ai_active: false }, 'alerts', now, 0), null, 'no AI, no motion');
eq(K.triggerOf({ motion: true }, 'motion', now, 0), { cls: 'motion', text: 'Motion' }, 'any motion');
eq(K.triggerOf({ manual_until: now / 1000 + 5 }, 'alerts', now, 0), { cls: 'clip', text: 'Recording' }, 'clip by hand or API');
eq(K.triggerOf({ manual_until: now / 1000 + 5, motion: true }, 'off', now, 0), null, 'off');
eq(K.triggerOf({ motion: true, enabled: false }, 'motion', now, 0), null, 'disabled camera');

// Activity: big at once, longer while it goes on, back after `hold`; dismissed stays down.
const act = new Map(), s = S({ trigger: 'motion', hold: 20 });
const at = (t, on) => K.updateActivity(act, [{ id: 1, name: 'A', motion: on }], s, t, 0);
at(0, true); eq(K.bigNow(act, s, 0, null), [1], 'goes big');
at(30000, true); eq(K.bigNow(act, s, 45000, null), [1], 'extended while it goes on');
at(46000, false); eq(K.bigNow(act, s, 49000, null), [1], 'held after it stops');
eq(K.bigNow(act, s, 66001, null), [], 'back to the grid');
at(67000, false); eq(act.size, 0, 'forgotten');
at(70000, true); act.get(1).dismissed = true; act.get(1).until = 0;
at(72000, true); eq(K.bigNow(act, s, 72000, null), [], 'dismissed: not again while it goes on');
at(73000, false); at(74000, true); eq(K.bigNow(act, s, 74000, null), [1], 'a new activity brings it back');

// Several at once: newest first, two side by side or the newest only.
const a2 = new Map();
K.updateActivity(a2, [{ id: 1, motion: true }, { id: 2, motion: false }, { id: 3, motion: false }], s, 0, 0);
K.updateActivity(a2, [{ id: 1, motion: true }, { id: 2, motion: true }, { id: 3, motion: false }], s, 1000, 0);
K.updateActivity(a2, [{ id: 1, motion: true }, { id: 2, motion: true }, { id: 3, motion: true }], s, 2000, 0);
eq(K.bigNow(a2, s, 2000, null), [3, 2], 'two newest');
eq(K.bigNow(a2, S({ multi: 'newest' }), 2000, null), [3], 'newest only');
eq(K.bigNow(a2, s, 2000, { id: 1, until: 9000 }), [1, 3], 'tapped first');
// A camera no longer shown is forgotten.
K.updateActivity(a2, [{ id: 1, motion: true }], s, 3000, 0);
eq([...a2.keys()], [1], 'gone cameras dropped');

// Burn-in steps go round.
eq([K.shiftAt(0), K.shiftAt(9), K.shiftAt(-1)], [[0, 0], [0, 0], [6, -6]], 'shift steps');

console.log(bad ? 'display: ' + bad + ' of ' + n + ' checks FAILED' : 'display: all ' + n + ' checks passed');
process.exit(bad ? 1 : 0);
