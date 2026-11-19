// i18n.js - the web app's text in the user's language.
//
// Every message is written in the code in English:  t('ev.count', '{n, plural, one {# event} other {# events}}', { n })
// The English is the source; a language pack only maps the same key to a translation. A message
// the pack lacks, or one whose English changed since it was translated (each pack entry carries a
// fingerprint of the English it came from), shows in English. A raw key is never shown.
//
// Message syntax is a small ICU subset, the same as the Android app's ARB files:
//   {name}                                   a value, inserted as given (escape it first in HTML)
//   {n, plural, =0 {...} one {# x} other {# xs}}   plural by the language's own rules; # = n
//   {v, select, a {...} b {...} other {...}}       choice by value
// '' is a literal quote; a quoted '{' is a literal brace.
//
// Safety: translations are put into the page as HTML just like the English is (text and attribute
// values), so a translation is used only if its tags are exactly those of its English, it has no other
// '<' (only '<' can start markup; '>' and '&' alone can't), and no more '"' than its English (a '"' could
// end an attribute value it is placed in). Entities are fine: they decode to text. Else: English.

// The module awaits the pack before anything that imports it runs, so module-level t() calls work.

const STORE = 'as-lang';
const SUPPORTED = ['en', 'de', 'es', 'fr', 'nl', 'it', 'zh-CN', 'pl', 'pt-BR', 'sv', 'ru', 'en-XA'];

let lang = 'en';
let pack = null;            // { key: [translation, fingerprint] }
const plural = new Map();   // cached Intl.PluralRules per language
const checked = new Map();  // key + English -> translation that passed, or null

// FNV-1a 32-bit of the English, hex. The pack tools compute the same.
export function fingerprint(s) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 0x01000193) >>> 0; }
  return h.toString(16).padStart(8, '0');
}

// The tags a message carries, in order. A translation must have exactly these.
const TAG = /<\/?[a-zA-Z][^<>]*>/g;
export function markup(s) { return (s.match(TAG) || []).join('\u0000'); }
const quotes = (s) => (s.match(/"/g) || []).length;
// The argument names a message uses, read by the formatter itself (render/arg below with `collect` set):
// every option body is walked, so a name the formatter could fill in is never missed by a separate parser.
// Two sets: every name read, and the names shown as they are ({name}, not as a plural count or a
// select's choice), so a translation can't print raw a value its English only uses to pick a branch.
let collect = null, shown = null;
function argNames(s) {
  collect = new Set(); shown = new Set();
  try { render(s, 0, null, null); return { all: collect, shown }; } finally { collect = null; shown = null; }
}
export function safeAgainst(tr, en) {
  if (markup(tr) !== markup(en)) return false;
  const rest = tr.replace(TAG, '');
  if (rest.includes('<') || quotes(rest) > quotes(en.replace(TAG, ''))) return false;
  const ea = argNames(en), ta = argNames(tr);
  for (const n of ta.all) if (!ea.all.has(n)) return false;
  for (const n of ta.shown) if (!ea.shown.has(n)) return false;
  return true;
}

function match(p) {
  if (!p) return null;
  const exact = SUPPORTED.find(s => s.toLowerCase() === p.toLowerCase());
  if (exact) return exact;
  const base = p.split('-')[0].toLowerCase();
  return SUPPORTED.find(s => s.split('-')[0].toLowerCase() === base) || null;
}

export function storedLang() {
  try { return localStorage.getItem(STORE) || null; } catch (_) { return null; }
}

// The whole app waits for the language at start, so a request that hangs gives up and English is used.
function fetchSoon(url) {
  const ctl = typeof AbortController === 'function' ? new AbortController() : null;
  const tm = ctl ? setTimeout(() => ctl.abort(), 5000) : 0;
  return fetch(url, { credentials: 'same-origin', signal: ctl ? ctl.signal : undefined }).finally(() => clearTimeout(tm));
}
async function fetchPack(code) {
  const r = await fetchSoon('/api/lang/' + encodeURIComponent(code));
  if (!r.ok) throw new Error('HTTP ' + r.status);
  const j = await r.json();
  if (!j || typeof j.messages !== 'object') throw new Error('bad pack');
  return j.messages;
}

// The user's choice (this browser; prefs.js keeps it in step with the account), then the browser's
// languages, then the server default, then English. A language whose pack isn't installed is skipped.
async function load() {
  const order = [];
  const stored = storedLang();
  if (stored) order.push(match(stored));
  else {
    for (const p of (navigator.languages || [navigator.language || 'en'])) {
      const m = match(p);
      if (m) { order.push(m); break; }   // the browser's first language we know decides, English included
    }
    order.push('server-default');
  }
  for (let c of order) {
    if (c === 'server-default') {
      try { const r = await fetchSoon('/api/lang/server-default'); c = r.ok ? (await r.json()).code : null; } catch (_) { c = null; }
      c = match(c);
    }
    if (!c || c === 'en') { if (c === 'en') break; continue; }
    try { pack = await fetchPack(c); lang = c; return; } catch (e) { console.warn('[i18n] no ' + c + ' pack:', e.message || e); }
  }
  lang = 'en'; pack = null;
}
await load();
document.documentElement.lang = lang;

export function currentLang() { return lang; }

// ---------------------------------------------------------------- dates and times
// Dates and times follow the app's language (the browser's own variant of it when it has one, so
// en-GB stays en-GB), and the user's clock choice: '12', '24' or automatic (the language's usual one).
const CLOCK = 'as-clock';
export function storedClock() {
  try { const c = localStorage.getItem(CLOCK); return c === '12' || c === '24' ? c : null; } catch (_) { return null; }
}
export function setClock(c) {
  c = c === '12' || c === '24' ? c : null;
  try { if (c) localStorage.setItem(CLOCK, c); else localStorage.removeItem(CLOCK); } catch (_) {}
  if (storedClock() === c) location.reload();
}
const base = (l) => String(l || '').split('-')[0].toLowerCase();
let LOCALE = lang;
try {
  const nav = navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language];
  // the browser's own variant of the app's language (en-GB stays en-GB); when the app fell back to
  // English only because the browser's language has no pack (nobody chose English), its own format
  LOCALE = nav.find(l => base(l) === base(lang)) || (lang === 'en' && !storedLang() && nav[0]) || lang;
} catch (_) {}
export function locale() { return LOCALE; }
// Options with the clock choice added; a call that sets hour12/hourCycle itself (timecodes) keeps it.
export function clockOpts(opts) {
  const c = storedClock();
  if (!c || (opts && ('hour12' in opts || 'hourCycle' in opts))) return opts;
  return Object.assign({}, opts, c === '12' ? { hour12: true } : { hourCycle: 'h23' });
}
// Every toLocale*String() call that names no locale ([] or nothing) gets these, so the 70-odd
// places that format a date or time need no change.
{
  const orig = Number.prototype.toLocaleString;
  Object.defineProperty(Number.prototype, 'toLocaleString', {
    configurable: true, writable: true,
    value: function (loc, opts) { return orig.call(this, loc === undefined || (Array.isArray(loc) && !loc.length) ? LOCALE : loc, opts); },
  });
}
for (const m of ['toLocaleString', 'toLocaleDateString', 'toLocaleTimeString']) {
  const orig = Date.prototype[m];
  Object.defineProperty(Date.prototype, m, {
    configurable: true, writable: true,
    value: function (loc, opts) {
      if (loc === undefined || (Array.isArray(loc) && !loc.length)) loc = LOCALE;
      return orig.call(this, loc, m === 'toLocaleDateString' ? opts : clockOpts(opts));
    },
  });
}
// Switch language: remembered in this browser, applied on reload (pages are built once).
// Without browser storage the choice cannot survive the reload, so nothing reloads (no loop).
export function setLang(code) {
  code = typeof code === 'string' && code ? code : null;
  try { if (code) localStorage.setItem(STORE, code); else localStorage.removeItem(STORE); } catch (_) {}
  if (storedLang() === code) location.reload();
}

function translation(key, en) {
  if (!pack) return null;
  const ck = key + '\u0000' + en;
  if (checked.has(ck)) return checked.get(ck);
  const e = pack[key];
  let ok = null;
  try {
    if (Array.isArray(e) && typeof e[0] === 'string' && e[0].length <= 8 * en.length + 2000 && e[1] === fingerprint(en) && safeAgainst(e[0], en)) ok = e[0];
  } catch (_) { ok = null; }   // a broken entry: English
  checked.set(ck, ok);
  return ok;
}

// ---------------------------------------------------------------- formatting

function rules() {
  if (!plural.has(lang)) { let r; try { r = new Intl.PluralRules(lang); } catch (_) { r = new Intl.PluralRules('en'); } plural.set(lang, r); }
  return plural.get(lang);
}

// Parse the text from i up to the closing brace at depth 0 (or the end); returns [string, next i].
function render(s, i, vars, hash) {
  let out = '';
  while (i < s.length) {
    const c = s[i];
    if (c === "'") {
      if (s[i + 1] === "'") { out += "'"; i += 2; continue; }
      if (s[i + 1] === '{' || s[i + 1] === '}' || s[i + 1] === '#') {
        const end = s.indexOf("'", i + 1);
        if (end > 0) { out += s.slice(i + 1, end); i = end + 1; continue; }
      }
      out += c; i++; continue;
    }
    if (c === '}') return [out, i];
    if (c === '#' && hash != null) { out += hash; i++; continue; }
    if (c === '{') { const [v, n] = arg(s, i + 1, vars); out += v; i = n; continue; }
    out += c; i++;
  }
  return [out, i];
}

// Find the '}' that closes text starting at j (nested braces skipped).
function close(s, j) {
  for (let d = 0; j < s.length; j++) { if (s[j] === '{') d++; else if (s[j] === '}') { if (!d) return j; d--; } }
  return s.length;
}

// An argument after its '{': {name} | {name, plural, opts} | {name, select, opts}. Returns [text, next i].
function arg(s, i, vars) {
  const field = (j) => { let k = j; while (k < s.length && s[k] !== ',' && s[k] !== '}') k++; return [s.slice(j, k).trim(), k]; };
  const [name, a] = field(i);
  if (collect) collect.add(name);
  const val = vars ? vars[name] : undefined;
  const plain = () => { if (shown) shown.add(name); return val == null ? '{' + name + '}' : String(val); };
  if (s[a] !== ',') return [plain(), a + 1];
  const [kind, b] = field(a + 1);
  if (s[b] !== ',') return [plain(), b + 1];
  const opts = {};
  let j = b + 1;
  for (;;) {
    while (j < s.length && /\s/.test(s[j])) j++;
    if (j >= s.length || s[j] === '}') break;
    let sel = '';
    while (j < s.length && s[j] !== '{' && !/\s/.test(s[j])) sel += s[j++];
    while (j < s.length && /\s/.test(s[j])) j++;
    if (s[j] !== '{') break;
    const end = close(s, j + 1);
    opts[sel] = s.slice(j + 1, end);
    j = end + 1;
  }
  const next = j + 1;
  if (collect) { for (const body of Object.values(opts)) render(body, 0, null, kind === 'plural' ? '' : null); return ['', next]; }
  if (kind === 'plural') {
    const n = Number(val);
    const body = opts['=' + n] ?? opts[rules().select(n)] ?? opts.other ?? '';
    return [render(body, 0, vars, new Intl.NumberFormat(lang).format(n))[0], next];
  }
  if (kind === 'select') return [render(opts[String(val)] ?? opts.other ?? '', 0, vars, null)[0], next];
  return [plain(), next];
}

export function format(msg, vars) { return render(msg, 0, vars, null)[0]; }

// The message for key in the current language, falling back to its English.
export function t(key, en, vars) {
  return format(translation(key, en) ?? en, vars);
}

// Static page text (index.html, display.html): the English stays in the HTML,
//   <b data-i18n="index.help">Help</b>   and   data-i18n-attr="title:index.menu,aria-label:index.menu".
// Applied once at load; text JS rewrites later uses t() itself.
export function applyDom(root = document) {
  if (!pack) return;
  for (const el of root.querySelectorAll('[data-i18n]')) {
    const tr = translation(el.dataset.i18n, el.textContent);
    if (tr != null) el.textContent = format(tr);
  }
  for (const el of root.querySelectorAll('[data-i18n-attr]')) {
    for (const pair of el.dataset.i18nAttr.split(',')) {
      const [attr, key] = pair.split(':').map(x => x.trim());
      const en = el.getAttribute(attr);
      if (!attr || !key || en == null) continue;
      const tr = translation(key, en);
      if (tr != null) el.setAttribute(attr, format(tr));
    }
  }
}
if (typeof document.querySelectorAll === 'function') applyDom();

// Dates and numbers follow the language. The 12/24-hour choice is its own setting (prefs.js).
export function nf(n, opts) { return new Intl.NumberFormat(lang, opts).format(n); }
