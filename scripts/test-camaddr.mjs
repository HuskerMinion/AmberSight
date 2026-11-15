// Tests for static/js/camaddr.js (the camera form's address split/join). Run: node scripts/test-camaddr.mjs
// (scripts/check-js.sh runs it too.) No framework: a failed check prints and exits 1.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, '..', 'static', 'js', 'camaddr.js'), 'utf8');
const A = await import('data:text/javascript;base64,' + Buffer.from(src).toString('base64'));

let n = 0, bad = 0;
const eq = (got, want, what) => {
  n++;
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { bad++; console.log('FAIL ' + what + '\n  got  ' + g + '\n  want ' + w); }
};
const pick = (o, ks) => Object.fromEntries(ks.map(k => [k, o && o[k]]));
const F = ['mode', 'why', 'scheme', 'host', 'port', 'main', 'sub', 'user', 'pass'];

// Reolink: default port written, both paths split.
eq(pick(A.fieldsFromUrls('rtsp://192.168.1.50:554/h264Preview_01_main', 'rtsp://192.168.1.50:554/h264Preview_01_sub', 'admin'), F),
  { mode: 'split', why: '', scheme: 'rtsp', host: '192.168.1.50', port: '554', main: '/h264Preview_01_main', sub: '/h264Preview_01_sub', user: '', pass: '' }, 'reolink split');
eq(A.brandOf('/h264Preview_01_main', '/h264Preview_01_sub'), 'reolink-h264', 'reolink brand detected');
// Dahua: the query stays in the path field.
const dh = A.fieldsFromUrls('rtsp://192.0.2.7/cam/realmonitor?channel=1&subtype=0', 'rtsp://192.0.2.7:554/cam/realmonitor?channel=1&subtype=1');
eq(pick(dh, ['mode', 'port', 'main', 'sub']), { mode: 'split', port: '554', main: '/cam/realmonitor?channel=1&subtype=0', sub: '/cam/realmonitor?channel=1&subtype=1' }, 'dahua query paths');
eq([A.brandOf(dh.main, dh.sub), A.BRANDS.find(b => b.id === 'dahua').tested], ['', false], 'dahua paths are also Foscam\'s: no brand named; dahua not tested');
eq([A.brandOf('/stream1', ''), A.brandOf('/live', ''), A.brandOf('stream1', 'stream2')], ['', 'wyze', 'tapo'], 'brand needs both paths to match');
// Hikvision.
eq(pick(A.fieldsFromUrls('rtsp://cam.local/Streaming/Channels/101', 'rtsp://cam.local/Streaming/Channels/102'), ['mode', 'host', 'main', 'sub']),
  { mode: 'split', host: 'cam.local', main: '/Streaming/Channels/101', sub: '/Streaming/Channels/102' }, 'hikvision');
eq(A.joinUrl({ scheme: 'rtsp', host: 'cam.local', port: '554', path: 'Streaming/Channels/101' }), 'rtsp://cam.local/Streaming/Channels/101', 'join drops the usual port, adds the leading /');

// Sign-in in the address goes to the user and password fields, never the path.
const cr = A.splitUrl('rtsp://admin:hunter2@192.168.1.9:554/stream1');
eq(pick(cr, ['host', 'port', 'path', 'user', 'pass', 'creds']), { host: '192.168.1.9', port: '554', path: '/stream1', user: 'admin', pass: 'hunter2', creds: true }, 'creds split');
eq(pick(A.fieldsFromUrls('rtsp://admin:hunter2@192.168.1.9/stream1', ''), ['main', 'user', 'pass']), { main: '/stream1', user: 'admin', pass: 'hunter2' }, 'creds to fields');
// Special characters: percent-encoding round trip, and a raw '@' or '/' typed in a password.
const pw = 'p@ss:w/rd#?% &!*';
const built = A.joinUrl({ scheme: 'rtsp', host: 'h', port: '', path: '/x', user: 'adm in', pass: pw });
eq(built, 'rtsp://adm%20in:p%40ss%3Aw%2Frd%23%3F%25%20%26%21%2A@h/x', 'creds encoded like the server');
eq(pick(A.splitUrl(built), ['user', 'pass', 'host', 'path']), { user: 'adm in', pass: pw, host: 'h', path: '/x' }, 'creds round trip');
eq(pick(A.splitUrl('rtsp://adm:p@ss@192.0.2.5:554/main'), ['user', 'pass', 'host', 'port', 'path']), { user: 'adm', pass: 'p@ss', host: '192.0.2.5', port: '554', path: '/main' }, 'raw @ in password');
eq(pick(A.splitUrl('rtsp://adm:a/b@192.0.2.5:554/main'), ['user', 'pass', 'host', 'path']), { user: 'adm', pass: 'a/b', host: '192.0.2.5', path: '/main' }, 'raw / in password');
// Same rule as the server: '#' does not end the sign-in, and a host:port start keeps '@' in the path.
eq(pick(A.splitUrl('rtsp://echo:Long/Pass word#1@192.0.2.9:554/main'), ['user', 'pass', 'host', 'path']), { user: 'echo', pass: 'Long/Pass word#1', host: '192.0.2.9', path: '/main' }, 'raw / and # in password');
eq(pick(A.splitUrl('rtsp://adm:a#b@192.0.2.9/main'), ['user', 'pass', 'host']), { user: 'adm', pass: 'a#b', host: '192.0.2.9' }, 'raw # in password');
eq(pick(A.splitUrl('rtsp://192.0.2.5:554/live@2'), ['user', 'host', 'path']), { user: '', host: '192.0.2.5', path: '/live@2' }, '@ in path after host:port');
eq(pick(A.splitUrl('rtsp://192.0.2.5/live@2'), ['user', 'host', 'path']), { user: '', host: '192.0.2.5', path: '/live@2' }, '@ in a path is not a sign-in');

// IPv6.
const v6 = A.fieldsFromUrls('rtsp://[fe80::1]:8554/live', 'rtsp://[FE80::1]:8554/sub');
eq(pick(v6, ['mode', 'host', 'port', 'main', 'sub']), { mode: 'split', host: 'fe80::1', port: '8554', main: '/live', sub: '/sub' }, 'ipv6 split');
eq(A.joinUrl({ scheme: 'rtsp', host: 'fe80::1', port: '8554', path: '/live' }), 'rtsp://[fe80::1]:8554/live', 'ipv6 join');
eq(A.joinUrl({ scheme: 'rtsp', host: '[fe80::1]', port: '554', path: '' }), 'rtsp://[fe80::1]', 'ipv6 typed with brackets');

// A port that is not the usual one is kept; the usual one written or not is the same stream.
eq(A.urlsFromFields({ scheme: 'rtsp', host: '192.0.2.2', port: '10554', main: '/a', sub: 'b' }, '', ''), { url: 'rtsp://192.0.2.2:10554/a', sub_url: 'rtsp://192.0.2.2:10554/b' }, 'non-default port');
eq(A.urlsFromFields({ scheme: 'http', host: '192.0.2.2', port: '80', main: '/video.mjpg', sub: '' }, '', ''), { url: 'http://192.0.2.2/video.mjpg', sub_url: '' }, 'http default port');
// Unchanged fields keep the stored text exactly (no reconnect on save).
eq(A.urlsFromFields({ scheme: 'rtsp', host: '192.168.1.50', port: '554', main: '/m', sub: '/s' }, 'rtsp://192.168.1.50:554/m', 'rtsp://admin:x%40y@192.168.1.50/s'),
  { url: 'rtsp://192.168.1.50:554/m', sub_url: 'rtsp://admin:x%40y@192.168.1.50/s' }, 'unchanged keeps stored text');
eq(A.urlsFromFields({ scheme: 'rtsp', host: '192.168.1.51', port: '554', main: '/m', sub: '/s' }, 'rtsp://192.168.1.50:554/m', 'rtsp://admin:x@192.168.1.50/s'),
  { url: 'rtsp://192.168.1.51/m', sub_url: 'rtsp://192.168.1.51/s' }, 'changed host rebuilds both');

// A sub stream elsewhere, or with its own user, switches to full addresses.
eq(pick(A.fieldsFromUrls('rtsp://192.0.2.3/main', 'rtsp://192.0.2.3:8554/sub'), ['mode', 'why']), { mode: 'full', why: 'sub-place' }, 'sub on another port');
eq(pick(A.fieldsFromUrls('rtsp://192.0.2.3/main', 'rtsp://192.0.2.4/sub'), ['mode', 'why']), { mode: 'full', why: 'sub-place' }, 'sub on another host');
eq(pick(A.fieldsFromUrls('rtsp://192.0.2.3/main', 'rtsps://192.0.2.3/sub'), ['mode', 'why']), { mode: 'full', why: 'sub-place' }, 'sub on another protocol');
eq(pick(A.fieldsFromUrls('rtsp://192.0.2.3/main', 'rtsp://viewer:pw@192.0.2.3/sub', 'admin'), ['mode', 'why']), { mode: 'full', why: 'sub-login' }, 'sub with its own user');
eq(pick(A.fieldsFromUrls('rtsp://192.0.2.3/main', 'rtsp://admin:pw@192.0.2.3/sub', 'admin'), ['mode', 'sub', 'pass']), { mode: 'split', sub: '/sub', pass: '' }, 'sub with the same user fits; its password is not shown');

// Other sources stay full addresses.
for (const [u, why] of [['push://abc123', 'scheme'], ['baichuan://192.168.1.60', 'scheme'], ['lavfi://testsrc=size=640x360', 'scheme'], ['rtmp://h/live/x', 'scheme'], ['device://USB Cam', 'scheme'], ['screen://0?fps=15', 'scheme'], ['not a url', 'bad']]) {
  eq(pick(A.fieldsFromUrls(u, ''), ['mode', 'why']), { mode: 'full', why }, 'full for ' + u);
}
// A sign-in in the query (Reolink HTTP-FLV) is not put in a path field.
eq(pick(A.fieldsFromUrls('http://192.0.2.8/flv?port=1935&app=bcs&stream=channel0_main.bcs&user=admin&password=pw', ''), ['mode', 'why']), { mode: 'full', why: 'query-login' }, 'query sign-in stays full');
// New camera: empty split form.
eq(pick(A.fieldsFromUrls('', '', ''), ['mode', 'scheme', 'port']), { mode: 'split', scheme: 'rtsp', port: '554' }, 'new camera');

// Paths and validation.
eq([A.normPath('Preview_01_main'), A.normPath('/x'), A.normPath('//x'), A.normPath(' '), A.normPath('?a=1')], ['/Preview_01_main', '/x', '/x', '', '/?a=1'], 'normPath');
eq(A.addrProblems({ host: '', port: '554' }).map(x => x[0]), ['host'], 'host needed');
eq(A.addrProblems({ host: '192.0.2.1', port: '70000' }).map(x => x[0]), ['port'], 'port range');
eq(A.addrProblems({ host: '192.0.2.1', port: '0' }).map(x => x[0]), ['port'], 'port zero');
eq(A.addrProblems({ host: '192.0.2.1/x', port: '' }).map(x => x[0]), ['host'], 'path in host');
eq(A.addrProblems({ host: 'h', port: '554', main: '/a b' }).map(x => x[0]), ['main'], 'space in path');
eq(A.addrProblems({ url: '192.0.2.1/x' }, 'full').map(x => x[0]), ['url'], 'full needs a scheme');
eq(A.addrProblems({ url: 'rtsp://h/x', sub_url: 'h/y' }, 'full').map(x => x[0]), ['sub_url'], 'full sub needs a scheme');
eq(A.addrProblems({ url: 'rtsp://h/x' }, 'full'), [], 'full ok');
eq(A.brandPort('554', A.BRANDS.find(b => b.id === 'foscam-old'), 'rtsp'), '88', 'older foscam port');
eq([A.BRANDS.find(b => b.id === 'foscam').tested, A.BRANDS.find(b => b.id === 'foscam').main, A.brandPort('88', A.BRANDS.find(b => b.id === 'foscam'), 'rtsp')], [true, '/cam/realmonitor?channel=1&subtype=0', '554'], 'foscam tested, dahua-style paths');
eq(A.brandPort('10554', A.BRANDS.find(b => b.id === 'foscam-old'), 'rtsp'), '10554', 'typed port kept');

// The sign-in rule, against the same cases the server's test reads (creds.rs
// userinfo_cases_shared_with_the_web_form): same user, password and host.
const cases = JSON.parse(readFileSync(join(here, 'userinfo-cases.json'), 'utf8'));
for (const c of cases) {
  const r = A.userinfoAt(c.rest);
  let got = null;
  if (r) {
    const ui = c.rest.slice(0, r[0]);
    const k = ui.indexOf(':');
    got = { user: k < 0 ? ui : ui.slice(0, k), pass: k < 0 ? '' : ui.slice(k + 1), host: c.rest.slice(r[0] + 1, r[1]) };
  }
  eq(got, c.user === null ? null : { user: c.user, pass: c.pass, host: c.host }, 'userinfo case ' + c.rest);
  const s = A.splitUrl('rtsp://' + c.rest);
  eq([s.creds, s.user], [c.user !== null, c.user === null ? '' : c.user], 'splitUrl follows userinfoAt: ' + c.rest);
}

console.log(bad ? bad + ' of ' + n + ' camaddr checks failed' : 'camaddr: all ' + n + ' checks passed');
process.exit(bad ? 1 : 0);
