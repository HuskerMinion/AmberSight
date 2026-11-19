// admin_smart.js - Settings > Smart features > Smart search: turn it on (the model downloads
// by itself) and see how far making events searchable has got. Searching
// is on the Events tab.

import { t } from './i18n.js';
import { api, esc, toast, errText, savedToast } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, how, notice } from './sp_kit.js';

const num = (n) => Number(n || 0).toLocaleString();

function smartPage(el) {
  let st = null, timer = 0, busy = false;
  async function load() {
    try { st = await api('/api/smart/status'); } catch (e) { el.innerHTML = '<p class="empty">' + esc(errText(e)) + '</p>'; return; }
    paint();
    clearTimeout(timer);
    // Keep the progress current while something is happening.
    if (st.enabled && (st.download.running || !st.loaded || st.events_waiting > 0)) timer = setTimeout(() => { if (el.isConnected) load(); }, 2000);
  }
  function paint() {
    const d = st.download;
    let state = '';
    if (!st.supported) state = '<p class="hint warn">' + t('admin_smart.cannot_run','Smart search cannot run on this computer.') + '</p>';
    else if (st.enabled) {
      if (d.running) state = '<div class="hint">' + t('admin_smart.downloading', 'Downloading the model... {pct}% of {mb} MB', { pct: Math.round(100 * d.done / Math.max(1, d.total)), mb: st.model_mb }) + '</div><progress class="smq-prog" max="' + d.total + '" value="' + d.done + '"></progress>';
      else if (d.error) state = notice('warn', esc(d.error), '<button class="btn" data-retry>' + t('admin_smart.try_again', 'Try again') + '</button>');
      else if (!st.loaded) state = '<div class="hint">' + t('admin_smart.starting', 'Starting...') + '</div>';
      else {
        const all = st.events_done + st.events_waiting;
        state = '<div class="hint">' + (st.events_waiting > 0
          ? t('admin_smart.making', 'Making events searchable, newest first: {done} of {all} done. Recent events can be searched already.', { done: num(st.events_done), all: num(all) })
          : t('admin_smart.ready', 'Ready: {events} events ({pictures} pictures and objects) can be searched.', { events: num(st.events_done), pictures: num(st.pictures) })) + '</div>' +
          (st.events_waiting > 0 ? '<progress class="smq-prog" max="' + all + '" value="' + st.events_done + '"></progress>' : '') +
          '<div class="hint">' + t('admin_smart.search_hint', 'Search on the Events tab: type what you are looking for in "Describe".') + '</div>';
      }
    }
    el.innerHTML = '<div class="ac">' +
      lead(t('admin_smart.lead', 'Optional. Find events by describing them, like &quot;red truck&quot; or &quot;dog in the yard&quot;.'), t('admin_smart.lead_help', 'Turn it on if you search your events often.')) +
      how(t('admin_smart.how_desc','AmberSight looks at each event\'s pictures and the objects AI found in them, and ranks events by how well they match your words. It runs on this server\'s processor (no graphics card needed) and nothing leaves it. Works best in daylight; at night (infrared) colors cannot be told apart. People\'s names are found with the Object filter instead (Settings &rsaquo; Smart features &rsaquo; Faces).')) +
      '<div class="ac-card"><div class="ac-card-t">' + t('admin_smart.set_up', 'Set up') + '</div>' +
      '<label class="sw"><input type="checkbox" role="switch" data-en' + (st.enabled ? ' checked' : '') + (st.supported ? '' : ' disabled') + '><span class="sw-track" aria-hidden="true"></span>' +
      '<span class="sw-text"><b>' + t('admin_smart.smart_search', 'Smart search') + '</b><small>' + t('admin_smart.turning_on', 'Turning it on downloads the model once (about {mb} MB: OpenAI CLIP, free to use) and then makes past and new events searchable in the background. Each event adds about 3 KB to the database.', { mb: st.model_mb }) + '</small></span></label>' +
      state + '</div>' +
      '</div>';
    const en = el.querySelector('[data-en]');
    en.onchange = () => save(en.checked);
    const rt = el.querySelector('[data-retry]');
    if (rt) rt.onclick = async () => { await save(false, true); await save(true); };
  }
  async function save(on, quiet) {
    if (busy) return;
    busy = true;
    try {
      const r = await api('/api/settings', { json: { smart_search: { enabled: on } } });
      if (r.error) throw new Error(r.error);
      if (!quiet) savedToast();
    } catch (e) { toast(errText(e), { kind: 'err' }); }
    busy = false;
    load();
  }
  el.innerHTML = '<div class="ac"><p class="empty">' + t('admin_smart.loading', 'Loading...') + '</p></div>';
  load();
  return () => clearTimeout(timer);
}

let stop = null;
registerAdminPage({ id: 'smart', title: t('admin_smart.title','Smart search'), order: 66, render(el) { stop = smartPage(el); }, leave() { if (stop) stop(); } });
