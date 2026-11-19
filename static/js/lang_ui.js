// lang_ui.js - the language pickers. Four mount functions, each used from one place:
//   mountLangSettings(el)   Settings > System > Language (admin): server default, installed packs, install, from a file
//   mountLangStep(el)       Getting started, step 0: language cards, pre-selected from the browser's languages
//   langMenuItem()          account menu entry: the user's own language, kept per user
//   mountSignInLang(el)     small menu on the sign-in page (this browser only)
//
// The server side is src/lang.rs:
//   GET  /api/lang           { default, user, installed:[code], available:[{code,name,native,machine,size}], downloads:{code:{busy,error}}, can_download }
//   POST /api/lang/default   { code | null }   (admin) sets the server default and starts the download
//   POST /api/lang/install   { code }          (admin) download now; progress is "downloads" in GET /api/lang
//   POST /api/lang/upload    raw pack file     (admin) offline install
//   GET  /api/lang/server-default  { code }    public, for the sign-in page
//   The user's own choice is the pref "lang" (prefs.js), a code or absent.
// Each language shows in its own name first ("Deutsch") and the English name second.
// English is always there. A change reloads the page (setLang does), since pages are built once.

import { t, currentLang, setLang, storedClock, setClock } from './i18n.js';
import { api, esc, toast, openDlg, errText, fmtSize } from './core.js';
import * as Prefs from './prefs.js';

const REPO_ISSUES = 'https://github.com/HuskerMinion/AmberSight/issues';
const STORE = 'as-lang';   // the browser's own choice (i18n.js)

const ENGLISH = { code: 'en', name: 'English', native: 'English' };
// Can this browser keep a choice at all? (Site data blocked: setLang/setClock store nothing and don't reload.)
const storedOk = () => { try { localStorage.setItem('as-probe', '1'); localStorage.removeItem('as-probe'); return true; } catch (e) { return false; } };

function css() {
  if (document.getElementById('langUiCss')) return;
  const s = document.createElement('style');
  s.id = 'langUiCss';
  s.textContent = `
.lu-cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(11rem, 1fr)); gap: var(--s2, 8px); margin: var(--s2, 8px) 0; }
.lu-card { display: flex; flex-direction: column; align-items: flex-start; gap: 2px; text-align: start; padding: 10px 12px; border: 1px solid var(--border, #8886); border-radius: 10px; background: transparent; color: inherit; font: inherit; cursor: pointer; min-height: 52px; }
.lu-card:hover { border-color: var(--accent, #d9822b); }
.lu-card[aria-checked="true"] { border-color: var(--accent, #d9822b); box-shadow: 0 0 0 1px var(--accent, #d9822b) inset; }
.lu-card .lu-n { font-weight: 600; }
.lu-card .lu-e, .lu-card .lu-s { font-size: var(--fs-s, 12px); color: var(--muted, #888); }
.lu-row { display: flex; align-items: center; gap: var(--s2, 8px); padding: 8px 0; border-bottom: 1px solid var(--border, #8883); flex-wrap: wrap; }
.lu-row .lu-who { flex: 1; min-width: 10rem; }
.lu-row .lu-e { color: var(--muted, #888); font-size: var(--fs-s, 12px); }
.lu-bar { height: 3px; border-radius: 2px; background: var(--border, #8883); overflow: hidden; width: 5rem; }
.lu-bar::after { content: ''; display: block; height: 100%; width: 40%; background: var(--accent, #d9822b); animation: lu-slide 1s ease-in-out infinite alternate; }
@keyframes lu-slide { from { margin-inline-start: 0; } to { margin-inline-start: 60%; } }
@media (prefers-reduced-motion: reduce) { .lu-bar::after { animation: none; width: 100%; } }
.lu-ok { color: var(--ok, #3a9d5d); font-size: var(--fs-s, 12px); }
.lu-err { color: var(--warn, #c0392b); font-size: var(--fs-s, 12px); }
.lu-menu { background: transparent; color: inherit; font: inherit; border: 1px solid var(--border, #8886); border-radius: 8px; padding: 4px 8px; min-height: 34px; }
`;
  document.head.appendChild(s);
}

// The languages in the order the server lists them, English first.
const names = (l) => '<span class="lu-n">' + esc(l.native) + '</span>' + (l.native !== l.name ? ' <span class="lu-e">' + esc(l.name) + '</span>' : '');
const label = (l) => (l.native === l.name ? l.native : l.native + ' (' + l.name + ')');

// The first of the browser's languages we have a pack for (en when none): what i18n.js would pick.
export function browserPick(codes) {
  for (const p of navigator.languages || [navigator.language || 'en']) {
    if (!p) continue;
    const exact = codes.find(c => c.toLowerCase() === p.toLowerCase());
    if (exact) return exact;
    const near = codes.find(c => c.split('-')[0].toLowerCase() === p.split('-')[0].toLowerCase());
    if (near) return near;
  }
  return 'en';
}

const load = () => api('/api/lang');
const busyAny = (st) => Object.values(st.downloads || {}).some(d => d && d.busy);

// Ask again once a second while a download runs; stops when the element leaves the page.
function poll(el, render) {
  const iv = setInterval(async () => {
    if (!el.isConnected) { clearInterval(iv); return; }
    try { const st = await load(); render(st); if (!busyAny(st)) clearInterval(iv); } catch (e) { clearInterval(iv); }
  }, 1000);
}

// The note the pack's state needs next to its name.
function state(st, l) {
  const d = (st.downloads || {})[l.code];
  if (d && d.busy) return '<span class="lu-bar" role="progressbar" aria-label="' + esc(t('lang_ui.downloading', 'Downloading…')) + '"></span>';
  if ((st.installed || []).includes(l.code)) return '<span class="lu-ok">' + esc(t('lang_ui.installed', 'Installed')) + '</span>';
  if (d && d.error) return '<span class="lu-err">' + esc(d.error) + '</span>';
  return '';
}

const size = (l) => (l.size ? ' · ' + esc(fmtSize(l.size)) : '');

// ---------------------------------------------------------------- Settings > System > Language

export async function mountLangSettings(el) {
  css();
  el.innerHTML = '<p class="empty">' + esc(t('lang_ui.loading', 'Loading…')) + '</p>';
  let st;
  try { st = await load(); } catch (e) { el.innerHTML = '<p class="hint warn">' + esc(errText(e)) + '</p>'; return; }

  const draw = (s) => {
    st = s;
    const langs = [ENGLISH, ...(st.available || [])];
    const def = st.default || 'en';
    const rows = (st.available || []).map(l => {
      const have = (st.installed || []).includes(l.code);
      const d = (st.downloads || {})[l.code] || {};
      return '<div class="lu-row" data-code="' + esc(l.code) + '"><div class="lu-who">' + names(l) + '<div class="lu-e">' + esc(t('lang_ui.machine_short', 'Machine translated')) + size(l) + '</div></div>' +
        state(st, l) +
        (have ? '' : '<button type="button" class="btn sm" data-install' + (d.busy || !st.can_download ? ' disabled' : '') + '>' + esc(t('lang_ui.install', 'Install')) + '</button>') + '</div>';
    }).join('');
    el.innerHTML = '<div class="sp-form">' +
      '<p class="sp-lead sp-one">' + esc(t('lang_ui.lead', 'The language people see by default. Anyone can pick their own in the account menu.')) + '</p>' +
      '<label class="field"><span>' + esc(t('lang_ui.server_default', 'Server default')) + '</span><select data-default>' +
      langs.map(l => '<option value="' + esc(l.code) + '"' + (l.code === def ? ' selected' : '') + '>' + esc(label(l)) + '</option>').join('') + '</select></label>' +
      (st.can_download ? '' : '<p class="hint warn">' + esc(t('lang_ui.downloads_off', 'Downloads are turned off, so languages can only be installed from a file.')) + '</p>') +
      '<h3>' + esc(t('lang_ui.packs', 'Languages')) + '</h3>' +
      (rows || '<p class="hint">' + esc(t('lang_ui.none', 'This version has no other languages yet.')) + '</p>') +
      '<div class="row" style="margin-top:var(--s2,8px)"><button type="button" class="btn" data-file>' + esc(t('lang_ui.from_file', 'Install from file…')) + '</button>' +
      '<input type="file" accept=".json,application/json" data-filein hidden></div>' +
      '<p class="hint">' + t('lang_ui.machine_note', 'These translations were made by a computer and may have mistakes. Fixes are welcome: <a href="{url}" target="_blank" rel="noopener">tell us on GitHub</a>.', { url: REPO_ISSUES }) + '</p>' +
      '</div>';
    wire();
    if (busyAny(st)) poll(el, draw);
  };

  const wire = () => {
    el.querySelector('[data-default]').onchange = async (ev) => {
      const sel = ev.target, code = sel.value === 'en' ? null : sel.value;
      sel.disabled = true;
      try {
        const r = await api('/api/lang/default', { json: { code } });
        if (r.note) toast(r.note, { kind: 'err' });
        else toast(t('lang_ui.saved', 'Saved'), { kind: 'ok', ms: 2200 });
        draw(await load());
        // Nobody chose a language here: the page follows the server default once the pack is there.
        const own = (() => { try { return localStorage.getItem(STORE); } catch (e) { return null; } })();
        if (!own && !st.user && (code === null || (st.installed || []).includes(code)) && currentLang() !== (code || 'en')) location.reload();
      } catch (e) { toast(errText(e), { kind: 'err' }); try { draw(await load()); } catch (x) { sel.disabled = false; } }
    };
    for (const b of el.querySelectorAll('[data-install]')) {
      b.onclick = async () => {
        const code = b.closest('[data-code]').dataset.code;
        b.disabled = true;
        try { await api('/api/lang/install', { json: { code } }); draw(await load()); } catch (e) { toast(errText(e), { kind: 'err' }); b.disabled = false; }
      };
    }
    const file = el.querySelector('[data-filein]');
    el.querySelector('[data-file]').onclick = () => file.click();
    file.onchange = async () => {
      const f = file.files && file.files[0];
      if (!f) return;
      if (f.size > 4 << 20) { toast(t('lang_ui.too_big', 'That file is bigger than 4 MB, so it is not a language pack.'), { kind: 'err' }); file.value = ''; return; }
      try {
        const r = await api('/api/lang/upload', { method: 'POST', body: f, headers: { 'Content-Type': 'application/json' } });
        toast(t('lang_ui.installed_code', 'Installed: {name}', { name: (langs0(st).find(l => l.code === r.code) || { native: r.code }).native }), { kind: 'ok', ms: 3000 });
        draw(await load());
      } catch (e) { toast(errText(e), { kind: 'err' }); }
      file.value = '';
    };
  };
  const langs0 = (s) => [ENGLISH, ...(s.available || [])];
  draw(st);
}

// ---------------------------------------------------------------- Getting started, step 0

export async function mountLangStep(el) {
  css();
  el.innerHTML = '<p class="empty">' + esc(t('lang_ui.loading', 'Loading…')) + '</p>';
  let st;
  try { st = await load(); } catch (e) { el.innerHTML = '<p class="hint warn">' + esc(errText(e)) + '</p>'; return; }
  const langs = [ENGLISH, ...(st.available || [])];
  // Nothing to choose from: the step has nothing to show.
  if (langs.length < 2) { el.hidden = true; return; }
  let pick = st.default || browserPick(langs.map(l => l.code));
  let saving = false;

  const draw = (s) => {
    st = s;
    el.innerHTML = '<p class="sp-lead sp-one">' + esc(t('lang_ui.step_lead', 'Choose the language AmberSight uses. You can change it later in Settings.')) + '</p>' +
      '<div class="lu-cards" role="radiogroup" aria-label="' + esc(t('lang_ui.step_title', 'Language')) + '">' +
      langs.map(l => '<button type="button" class="lu-card" role="radio" aria-checked="' + (l.code === pick) + '" data-code="' + esc(l.code) + '">' + names(l) +
        (l.code === 'en' ? '' : '<span class="lu-s">' + state(st, l) + '</span>') + '</button>').join('') + '</div>' +
      '<p class="hint">' + esc(t('lang_ui.step_note', 'Other languages are machine translated. A language is downloaded the first time it is chosen.')) + '</p>';
    for (const b of el.querySelectorAll('.lu-card')) b.onclick = () => choose(b.dataset.code);
    if (busyAny(st)) poll(el, draw);
  };

  const choose = async (code) => {
    if (saving) return;
    pick = code; saving = true; draw(st);
    try {
      const r = await api('/api/lang/default', { json: { code: code === 'en' ? null : code } });
      if (r.note) toast(r.note, { kind: 'err' });
      // Wait (briefly) for the pack, then show the page in it.
      for (let i = 0; i < 60 && code !== 'en'; i++) {
        const s = await load();
        draw(s);
        if ((s.installed || []).includes(code)) break;
        const d = (s.downloads || {})[code];
        if (!busyAny(s) && !(d && d.busy)) break;
        await new Promise(res => setTimeout(res, 1000));
      }
      st = await load();
      if (code === 'en' || (st.installed || []).includes(code)) setLang(code === 'en' ? null : code);
      else { saving = false; draw(st); }
    } catch (e) { toast(errText(e), { kind: 'err' }); saving = false; draw(st); }
  };
  draw(st);
}

// ---------------------------------------------------------------- account menu

// A button for the account menu: "Language: Deutsch". Opens a list; the choice is kept per user
// (the pref "lang") and in this browser, then the page reloads.
export function langMenuItem() {
  css();
  const b = document.createElement('button');
  b.type = 'button';
  b.setAttribute('role', 'menuitem');
  const show = (txt) => { b.textContent = t('lang_ui.menu', 'Language: {name}', { name: txt }); };
  show(t('lang_ui.server_default_short', 'Server default'));
  load().then((st) => {
    const l = [ENGLISH, ...(st.available || [])].find(x => x.code === (st.user || ''));
    show(l ? l.native : t('lang_ui.server_default_short', 'Server default'));
  }).catch(() => { b.hidden = true; });
  b.onclick = async (ev) => {
    ev.stopPropagation();
    let st;
    try { st = await load(); } catch (e) { toast(errText(e), { kind: 'err' }); return; }
    const have = [ENGLISH, ...(st.available || [])].filter(l => l.code === 'en' || (st.installed || []).includes(l.code));
    const cur = st.user || '';
    const opt = (code, html) => '<button type="button" class="lu-card" role="radio" aria-checked="' + (code === cur) + '" data-code="' + esc(code) + '">' + html + '</button>';
    const clk = storedClock() || '';
    const clockOpt = (v, name, hint) => '<button type="button" class="lu-card" role="radio" aria-checked="' + (v === clk) + '" data-clock="' + v + '"><span class="lu-n">' + esc(name) + '</span><span class="lu-e">' + esc(hint) + '</span></button>';
    const sample = (h12) => new Date(2026, 0, 1, 13, 30).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit', hour12: h12 });
    const d = openDlg(t('lang_ui.menu_title', 'Your language'),
      '<div class="lu-cards" role="radiogroup">' +
      opt('', '<span class="lu-n">' + esc(t('lang_ui.server_default_short', 'Server default')) + '</span>') +
      have.map(l => opt(l.code, names(l))).join('') + '</div>' +
      '<p class="hint">' + esc(t('lang_ui.menu_note', 'Only your own view changes. Other languages are machine translated.')) + '</p>' +
      '<h3>' + esc(t('lang_ui.clock_title', 'Time')) + '</h3><div class="lu-cards" role="radiogroup">' +
      clockOpt('', t('lang_ui.clock_auto', 'Automatic'), t('lang_ui.clock_auto_hint', 'As usual for the language')) +
      clockOpt('12', t('lang_ui.clock_12', '12-hour'), sample(true)) +
      clockOpt('24', t('lang_ui.clock_24', '24-hour'), sample(false)) + '</div>', true);
    for (const c of d.querySelectorAll('[data-clock]')) {
      c.onclick = async () => {
        const v = c.dataset.clock || null;
        Prefs.set({ clock: v });
        await Prefs.flush();
        d.__close();
        setClock(v);
        if (!storedOk()) toast(t('lang_ui.cannot_keep', 'This browser does not let the page keep the choice (site data is blocked), so it is saved to your account only.'), { kind: 'err' });
      };
    }
    for (const c of d.querySelectorAll('.lu-card[data-code]')) {
      c.onclick = async () => {
        const code = c.dataset.code;
        Prefs.set({ lang: code || null });
        await Prefs.flush();
        d.__close();
        // An empty choice: the browser's languages decide again (i18n.js), as the server default is its last step.
        setLang(code || null);
        if (!storedOk()) toast(t('lang_ui.cannot_keep', 'This browser does not let the page keep the choice (site data is blocked), so it is saved to your account only.'), { kind: 'err' });
      };
    }
  };
  return b;
}

// ---------------------------------------------------------------- sign-in page

// A small menu for the sign-in page (nobody is signed in, so only this browser remembers the choice).
// The languages are the ones this server has installed; GET /api/lang needs a sign-in, so the list
// comes from the packs themselves: each code is tried once and only the ones that are served show.
export async function mountSignInLang(el) {
  css();
  const codes = ['de', 'es', 'fr', 'nl', 'it', 'zh-CN', 'pl', 'pt-BR', 'sv', 'ru'];
  const native = { de: 'Deutsch', es: 'Español', fr: 'Français', nl: 'Nederlands', it: 'Italiano', 'zh-CN': '简体中文', pl: 'Polski', 'pt-BR': 'Português (Brasil)', sv: 'Svenska', ru: 'Русский' };
  const have = (await Promise.all(codes.map(async (c) => {
    try { const r = await fetch('/api/lang/' + encodeURIComponent(c), { method: 'HEAD', credentials: 'same-origin' }); return r.ok ? c : null; } catch (e) { return null; }
  }))).filter(Boolean);
  // none answered (also while the server is unreachable): hidden, and asked again the next time the sign-in page shows
  if (!have.length) { el.hidden = true; delete el.dataset.mounting; return; }
  el.hidden = false;
  const cur = currentLang();
  const sel = document.createElement('select');
  sel.className = 'lu-menu';
  sel.setAttribute('aria-label', t('lang_ui.signin_label', 'Language'));
  sel.innerHTML = ['en', ...have].map(c => '<option value="' + esc(c) + '"' + (c === cur ? ' selected' : '') + '>' + esc(c === 'en' ? 'English' : native[c]) + '</option>').join('');
  sel.onchange = () => setLang(sel.value);
  el.replaceChildren(sel);
}
