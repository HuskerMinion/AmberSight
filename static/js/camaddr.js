// camaddr.js - a network camera's address taken apart into the fields of the camera form
// (protocol, address, port, main path, sub path, user, password) and put back together.
// No DOM and no imports: scripts/test-camaddr.mjs runs it under node.
//
// Storage does not change: the config and the API keep `url` and `sub_url` as full addresses
// and `username` / `password` on their own. The form only splits and joins.

// The protocols the split form offers. Anything else (rtmp://, lavfi://...) is
// edited as a full address (baichuan:// has its own form: a battery camera, see fieldsFromUrls).
import { t } from './i18n.js';
export const PROTOCOLS = [['rtsp', 'RTSP'], ['rtsps', 'RTSPS'], ['http', 'HTTP'], ['https', 'HTTPS']];
export const DEFAULT_PORT = { rtsp: 554, rtsps: 322, http: 80, https: 443 };

// Query names whose values are a sign-in (the same list the server hides in logs).
const SECRET_Q = /(?:^|[?&])(password|pwd|pass|passwd|user|username|usr|token|key|apikey|api_key|auth)=/i;

const dec = (s) => { try { return decodeURIComponent(s); } catch (e) { return s; } };
// User info: only unreserved characters stay as they are (what the server writes).
const encUi = (s) => encodeURIComponent(s).replace(/[!'()*]/g, ch => '%' + ch.charCodeAt(0).toString(16).toUpperCase());
const HOSTPORT = /^(\[[^\]\s/]*\]|[^\s/:@[\]]+)(?::(\d*))?$/;

export const hasScheme = (s) => /^[a-z][a-z0-9+.-]*:\/\//i.test(String(s || '').trim());

// Where an address's sign-in ends, in `rest` (the address after "scheme://"): [index of the '@'
// before the host, end of the host[:port] after it], or null. The server's rule exactly
// (creds::userinfo_at); scripts/userinfo-cases.json is checked against both.
//  - The last '@' before the first '/' (passwords may hold '@', '?', '#').
//  - Else, only when what comes before the first '/' is not already a host[:port] (so
//    "camhost:554/live@2" and "[fe80::1]/live@2" keep their '@' in the path), a password typed
//    raw with a '/' in it ("u:pa/ss@host/x"): the last '@' before any '?', with user:password
//    before it (the ':' before any '/') and a host after it that looks like one (a dot,
//    brackets, a port, or "localhost").
export function userinfoAt(rest) {
  let slash = rest.indexOf('/');
  if (slash < 0) slash = rest.length;
  const first = rest.slice(0, slash);
  if (first.includes('@')) return [first.lastIndexOf('@'), slash];
  if (HOSTPORT.test(first)) return null;
  const q = rest.indexOf('?');
  const pre = q < 0 ? rest : rest.slice(0, q);
  const at = pre.lastIndexOf('@');
  if (at < 0) return null;
  const head = pre.slice(0, at);
  const colon = head.indexOf(':');
  if (colon < 0 || head.slice(0, colon).includes('/') || !head.slice(colon).includes('/')) return null;
  const after = rest.slice(at + 1);
  let end = after.search(/[/?#]/);
  if (end < 0) end = after.length;
  const host = after.slice(0, end);
  const looks = host.includes('.') || host.startsWith('[') || host.includes(':') || host.toLowerCase() === 'localhost';
  return looks && HOSTPORT.test(host) ? [at, at + 1 + end] : null;
}

// "rtsp://u:p@host:554/path?q" -> { scheme, host, port, path, user, pass, creds }.
// host has no brackets (an IPv6 address is bare); port is the text after ':' ('' when none);
// path keeps its query. null when it is not "scheme://...".
export function splitUrl(s) {
  const m = /^([a-z][a-z0-9+.-]*):\/\/(.*)$/is.exec(String(s || '').trim());
  if (!m) return null;
  const scheme = m[1].toLowerCase();
  const rest = m[2];
  const ua = userinfoAt(rest);
  let user = '', pass = '', creds = false;
  const from = ua ? ua[0] + 1 : 0;
  let end = rest.slice(from).search(/[/?#]/);
  end = end < 0 ? rest.length : from + end;
  let hp = rest.slice(from, end), tail = rest.slice(end);
  if (ua) {
    const ui = rest.slice(0, ua[0]);
    const c = ui.indexOf(':');
    user = dec(c < 0 ? ui : ui.slice(0, c));
    pass = c < 0 ? '' : dec(ui.slice(c + 1));
    creds = true;
  }
  let host = hp, port = '';
  if (hp.startsWith('[')) {
    const close = hp.indexOf(']');
    host = close < 0 ? hp.slice(1) : hp.slice(1, close);
    const r = close < 0 ? '' : hp.slice(close + 1);
    if (r.startsWith(':')) port = r.slice(1);
  } else {
    const parts = hp.split(':');
    // More than one ':' without brackets: a bare IPv6 address, no port.
    if (parts.length === 2) { host = parts[0]; port = parts[1]; }
  }
  if (tail.startsWith('#')) tail = '';
  return { scheme, host, port, path: normPath(tail), user, pass, creds };
}

// A path as typed: "Preview_01_main" -> "/Preview_01_main", "?a=1" -> "/?a=1", "//x" -> "/x".
export function normPath(p) {
  const s = String(p == null ? '' : p).trim();
  if (!s) return '';
  return '/' + s.replace(/^\/+/, '');
}

// "fe80::1" -> "[fe80::1]"; brackets typed by hand are kept once.
const hostPart = (h) => { const x = String(h || '').trim().replace(/^\[|\]$/g, ''); return x.includes(':') ? '[' + x + ']' : x; };

// The other way: { scheme, host, port, path, user?, pass? } -> a full address. The port is left
// out when it is the protocol's usual one; a sign-in is written only when `user` is given.
export function joinUrl(f) {
  const scheme = String(f.scheme || 'rtsp').toLowerCase();
  const host = hostPart(f.host);
  if (!host) return '';
  const port = String(f.port == null ? '' : f.port).trim();
  const p = port && Number(port) !== DEFAULT_PORT[scheme] ? ':' + port : '';
  const ui = f.user ? encUi(f.user) + (f.pass ? ':' + encUi(f.pass) : '') + '@' : '';
  return scheme + '://' + ui + host + p + normPath(f.path);
}

// The same camera stream, ignoring the sign-in, the usual port written or not, and case in the
// protocol and host name.
export function canon(u) {
  const s = splitUrl(u);
  if (!s) return String(u || '').trim();
  const port = s.port === '' ? String(DEFAULT_PORT[s.scheme] || '') : String(Number(s.port));
  return s.scheme + '://' + hostPart(s.host).toLowerCase() + ':' + port + s.path;
}
export const sameAddr = (a, b) => canon(a) === canon(b);

const samePlace = (a, b) => a.scheme === b.scheme && hostPart(a.host).toLowerCase() === hostPart(b.host).toLowerCase() &&
  (a.port === '' ? DEFAULT_PORT[a.scheme] : Number(a.port)) === (b.port === '' ? DEFAULT_PORT[b.scheme] : Number(b.port));

// The form's fields for a stored camera. mode 'full' (with `why`) when the address does not fit
// the split fields:
//   scheme      not RTSP, RTSPS, HTTP or HTTPS (baichuan://, rtmp://, lavfi://...)
//   bad         not an address with a protocol at all
//   query-login the sign-in is in the address's query (Reolink HTTP-FLV)
//   sub-place   the sub stream is on another protocol, host or port
//   sub-login   the sub stream has its own, different user name
// `username` is the camera's stored user name: a sub stream that carries the same one in its
// address still fits (the split form leaves it to the camera's sign-in).
export function fieldsFromUrls(url, subUrl, username, uid) {
  const out = { mode: 'split', why: '', scheme: 'rtsp', host: '', port: '554', main: '', sub: '', user: '', pass: '' };
  const u = String(url || '').trim(), su = String(subUrl || '').trim();
  if (!u && !su) return out;
  const m = splitUrl(u);
  if (!m) return Object.assign(out, { mode: 'full', why: u ? 'bad' : 'sub-place' });
  // A battery Argus (no hub): baichuan://<host>, with its UID in the address or in the `uid` setting.
  if (m.scheme === 'baichuan' && !su && !/[?&](?!uid=)[^=&]+=/i.test(m.path.slice(1))) {
    return Object.assign(out, { mode: 'battery', scheme: 'baichuan', host: m.host, port: m.port, main: '', sub: '', user: m.user, pass: m.pass, uid: addrUid(m.path) || normUid(uid) });
  }
  Object.assign(out, { scheme: m.scheme, host: m.host, port: m.port || String(DEFAULT_PORT[m.scheme] || ''), main: m.path, user: m.user, pass: m.pass });
  if (!DEFAULT_PORT[m.scheme]) return Object.assign(out, { mode: 'full', why: 'scheme' });
  if (SECRET_Q.test(m.path.split('?')[1] ? '?' + m.path.split('?')[1] : '')) return Object.assign(out, { mode: 'full', why: 'query-login' });
  if (su) {
    const s = splitUrl(su);
    if (!s || !samePlace(m, s)) return Object.assign(out, { mode: 'full', why: 'sub-place' });
    if (s.creds && s.user !== (m.user || username || '')) return Object.assign(out, { mode: 'full', why: 'sub-login' });
    if (SECRET_Q.test(s.path.split('?')[1] ? '?' + s.path.split('?')[1] : '')) return Object.assign(out, { mode: 'full', why: 'query-login' });
    out.sub = s.path;
  }
  return out;
}

// ---- battery Argus (Reolink, no hub): baichuan://<host>, UID in its own field ----
// Reolink UIDs are 16 letters and digits; the server takes any text, so this only turns away what
// could never be one (spaces, symbols, a stray address).
const UID_RE = /^[A-Za-z0-9]{6,40}$/;
export const normUid = (s) => String(s == null ? '' : s).trim().toUpperCase();
export const uidProblem = (s) => {
  const u = normUid(s);
  if (!u) return t('camaddr.uid_enter', 'Enter the camera\'s UID (Reolink app > Device info). It is 16 letters and digits.');
  return UID_RE.test(u) ? '' : t('camaddr.uid_invalid', 'A UID is letters and digits only, like 95270000ABCDEFGH (Reolink app > Device info).');
};
// The uid= in an address's query ('' when none).
export function addrUid(pathOrUrl) {
  const m = /[?&]uid=([^&#]*)/i.exec(String(pathOrUrl || ''));
  return m ? normUid(dec(m[1])) : '';
}
// The address to save for a battery camera: baichuan://host[:port], no UID in it (the UID is its
// own setting). A stored address that says the same keeps its text exactly (no reconnect on save).
export function batteryUrl(f, storedUrl) {
  const host = hostPart(f.host);
  if (!host) return '';
  const port = String(f.port == null ? '' : f.port).trim();
  const url = 'baichuan://' + host + (port ? ':' + port : '');
  const st = splitUrl(storedUrl);
  if (st && st.scheme === 'baichuan' && !st.creds && hostPart(st.host).toLowerCase() === host.toLowerCase() && st.port === port &&
    (!addrUid(st.path) || addrUid(st.path) === normUid(f.uid))) return String(storedUrl).trim();
  return url;
}

// The addresses to save from the split fields. A stream whose address means the same as the
// stored one keeps the stored text exactly (so saving other settings never reconnects a camera,
// and a sub stream keeps its own sign-in).
export function urlsFromFields(f, storedUrl, storedSub) {
  const base = { scheme: f.scheme, host: f.host, port: f.port };
  let url = joinUrl(Object.assign({}, base, { path: f.main }));
  let sub = f.sub && String(f.sub).trim() ? joinUrl(Object.assign({}, base, { path: f.sub })) : '';
  if (url && storedUrl && sameAddr(url, storedUrl) && hasScheme(storedUrl)) url = String(storedUrl).trim();
  if (sub && storedSub && sameAddr(sub, storedSub) && hasScheme(storedSub)) sub = String(storedSub).trim();
  return { url, sub_url: sub };
}

// Field problems as [field, message]; field is host, port, main, sub, url or sub_url.
export function addrProblems(f, mode) {
  const out = [];
  if (mode === 'battery') {
    const h = String(f.host || '').trim();
    if (!h) out.push(['host', t('camaddr.host_enter', 'Enter the camera\'s address, like 192.168.1.50 or camera.local.')]);
    else if (/[\s/?#@]/.test(h)) out.push(['host', t('camaddr.host_only', 'Only the camera\'s address goes here, like 192.168.1.50.')]);
    const p = String(f.port == null ? '' : f.port).trim();
    if (p && !(/^\d+$/.test(p) && Number(p) >= 1 && Number(p) <= 65535)) out.push(['port', t('camaddr.port_range', 'The port is a number from 1 to 65535.')]);
    const w = uidProblem(f.uid);
    if (w) out.push(['uid', w]);
    return out;
  }
  if (mode === 'full') {
    const u = String(f.url || '').trim(), s = String(f.sub_url || '').trim();
    if (!u) out.push(['url', t('camaddr.url_enter', 'Enter the camera\'s stream address, like rtsp://192.168.1.50:554/stream1.')]);
    else if (!hasScheme(u)) out.push(['url', t('camaddr.url_start_proto', 'Start the address with its protocol, like rtsp:// or http://.')]);
    if (s && !hasScheme(s)) out.push(['sub_url', t('camaddr.sub_url_start_proto', 'Start the address with its protocol, like rtsp:// or http://.')]);
    return out;
  }
  const h = String(f.host || '').trim();
  if (!h) out.push(['host', t('camaddr.host_enter', 'Enter the camera\'s address, like 192.168.1.50 or camera.local.')]);
  else if (/[\s/?#@]/.test(h)) out.push(['host', t('camaddr.host_only_main', 'Only the camera\'s address goes here, like 192.168.1.50. The rest goes in Main stream.')]);
  const p = String(f.port == null ? '' : f.port).trim();
  if (p && !(/^\d+$/.test(p) && Number(p) >= 1 && Number(p) <= 65535)) out.push(['port', t('camaddr.port_range', 'The port is a number from 1 to 65535.')]);
  if (/\s/.test(String(f.main || '').trim())) out.push(['main', t('camaddr.path_no_spaces', 'A stream path has no spaces. Write a space as %20.')]);
  if (/\s/.test(String(f.sub || '').trim())) out.push(['sub', t('camaddr.path_no_spaces', 'A stream path has no spaces. Write a space as %20.')]);
  return out;
}

// Stream paths by brand, from docs/cameras.md. `tested`: we ran that brand ourselves (the guide's
// "Cameras we tested"). Ports are the usual RTSP port for the brand.
export const BRANDS = [
  { id: 'reolink', name: 'Reolink', tested: true, main: '/Preview_01_main', sub: '/Preview_01_sub', port: 554 },
  { id: 'reolink-h264', name: 'Reolink (h264Preview)', tested: true, main: '/h264Preview_01_main', sub: '/h264Preview_01_sub', port: 554 },
  { id: 'wyze', name: t('camaddr.brand_wyze', 'Wyze (RTSP firmware)'), tested: true, main: '/live', sub: '', port: 554 },
  { id: 'amcrest', name: 'Amcrest', tested: true, main: '/cam/realmonitor?channel=1&subtype=0', sub: '/cam/realmonitor?channel=1&subtype=1', port: 554 },
  { id: 'dahua', name: t('camaddr.brand_dahua', 'Dahua (also Lorex, Empiretech and other rebrands)'), tested: false, main: '/cam/realmonitor?channel=1&subtype=0', sub: '/cam/realmonitor?channel=1&subtype=1', port: 554 },
  { id: 'hikvision', name: 'Hikvision', tested: false, main: '/Streaming/Channels/101', sub: '/Streaming/Channels/102', port: 554 },
  { id: 'uniview', name: 'Uniview', tested: false, main: '/media/video1', sub: '/media/video2', port: 554 },
  { id: 'tapo', name: 'Tapo / TP-Link', tested: false, main: '/stream1', sub: '/stream2', port: 554 },
  { id: 'axis', name: 'Axis', tested: false, main: '/axis-media/media.amp', sub: '/axis-media/media.amp?resolution=640x360', port: 554 },
  { id: 'annke', name: t('camaddr.brand_annke', 'Annke (main only)'), tested: false, main: '/H264/ch1/main/av_stream', sub: '', port: 554 },
  { id: 'foscam', name: 'Foscam', tested: false, main: '/videoMain', sub: '/videoSub', port: 88 },
  // Not a stream path: a battery camera talks Reolink's own protocol (baichuan://) and needs its UID.
  { id: 'reolink-battery', name: t('camaddr.brand_reolink_battery', 'Reolink battery (Argus, no hub)'), tested: true, battery: true, main: '', sub: '', port: 0 },
];
// The brand whose paths these are ('' when none, or when several brands share them, as
// Amcrest and Dahua do).
export function brandOf(main, sub) {
  const a = normPath(main).toLowerCase(), b = normPath(sub).toLowerCase();
  const hits = BRANDS.filter(x => !x.battery && x.main.toLowerCase() === a && x.sub.toLowerCase() === b);
  return hits.length === 1 ? hits[0].id : '';
}
// The port after picking a brand: the brand's usual port, unless one of a kind no brand uses was typed.
export function brandPort(cur, brand, scheme) {
  const p = String(cur == null ? '' : cur).trim();
  const usual = p === '' || BRANDS.some(x => String(x.port) === p) || Object.values(DEFAULT_PORT).some(x => String(x) === p);
  return usual && (scheme || 'rtsp') === 'rtsp' ? String(brand.port) : p;
}
// A battery camera's "Look at the card" choices for its catch_up_every (minutes, as a
// string): a period set in the config file (60 to 1440) is offered as it is, so the form
// shows it and saving keeps it.
export function catchUpOptions(every) {
  const opts = [['60', t('camaddr.catchup_60', 'about hourly, while the camera is awake')], ['180', t('camaddr.catchup_180', 'every 3 hours, while awake')], ['360', t('camaddr.catchup_360', 'every 6 hours, while awake')], ['0', t('camaddr.catchup_0', 'only after a restart, a reconnect or a missed push')]];
  const m = Number(every);
  if (!opts.some(o => o[0] === every) && m > 0) {
    opts.splice(opts.length - 1, 0, [every, m % 60 ? t('camaddr.catchup_min', 'every {n} minutes, while awake', { n: m }) : t('camaddr.catchup_h', 'every {n} hours, while awake', { n: m / 60 })]);
  }
  return opts;
}
