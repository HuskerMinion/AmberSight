// admin_sound.js - Settings > Smart features > Sounds: name the loud sounds cameras hear (the
// sound model downloads by itself when turned on). The loud-sound trigger
// itself is set per camera (camera settings, Alerts tab).

import { api, esc, toast, errText, savedToast } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, how, notice } from './sp_kit.js';


function soundPage(el) {
  let st = null, timer = 0, busy = false;
  async function load() {
    try { st = await api('/api/sound/status'); } catch (e) { el.innerHTML = '<p class="empty">' + esc(errText(e)) + '</p>'; return; }
    paint();
    clearTimeout(timer);
    // Keep the progress current while the model downloads or loads.
    if (st.supported !== false && st.labels && (st.download.running || !st.loaded) && !st.download.error) timer = setTimeout(() => { if (el.isConnected) load(); }, 1500);
  }
  function paint() {
    const d = st.download;
    let state = '';
    if (!st.supported) state = '<p class="hint warn">Sound names cannot be used on this computer; loud sounds still start events.</p>';
    else if (st.labels) {
      if (d.running) state = '<div class="hint">Downloading the sound model... ' + Math.round(100 * d.done / Math.max(1, d.total)) + '% of ' + st.model_mb + ' MB</div><progress class="smq-prog" max="' + d.total + '" value="' + d.done + '"></progress>';
      else if (d.error) state = notice('warn', esc(d.error), '<button class="btn" data-retry>Try again</button>');
      else if (!st.loaded) state = '<div class="hint">Starting...</div>';
      else state = '<div class="hint">Ready. Loud sounds are named: ' + st.names.map(esc).join(', ') + '.</div>';
    }
    const cams = st.cameras.length ? 'Cameras listening for loud sounds: ' + st.cameras.map(esc).join(', ') + '.' : 'No camera listens for loud sounds yet: turn it on in a camera\'s settings, Alerts tab.';
    el.innerHTML = '<div class="ac">' +
      lead('Optional. Names the loud sounds a camera hears, such as glass breaking or a dog barking.', 'Turn it on if a camera with a microphone watches somewhere sound matters.') +
      how('A camera with a microphone can start an event when it hears a sudden loud sound. Turn that on per camera (camera settings, Alerts tab), where a live meter helps choose how loud is loud. The model labels sound events and lets a camera react to only some of them.') +
      '<div class="ac-card"><div class="ac-card-t">Sound names</div>' +
      '<label class="sw"><input type="checkbox" role="switch" data-en' + (st.labels ? ' checked' : '') + (st.supported ? '' : ' disabled') + '><span class="sw-track" aria-hidden="true"></span>' +
      '<span class="sw-text"><b>Name what was heard</b><small>Labels sound events (glass break, dog bark, alarm, shouting, bang, doorbell, knock, car horn, crying) and lets a camera react to only some of them. Turning it on downloads a small model once (about ' + st.model_mb + ' MB: Google YAMNet, free to use). It runs on this server\'s processor, a few milliseconds per sound; nothing leaves this server.</small></span></label>' +
      state + '</div>' +
      '<div class="hint">' + cams + '</div></div>';
    const en = el.querySelector('[data-en]');
    en.onchange = () => save(en.checked);
    const rt = el.querySelector('[data-retry]');
    if (rt) rt.onclick = async () => { await save(false, true); await save(true); };
  }
  async function save(on, quiet) {
    if (busy) return;
    busy = true;
    try {
      const r = await api('/api/settings', { json: { sound: { labels: on } } });
      if (r.error) throw new Error(r.error);
      if (!quiet) savedToast();
    } catch (e) { toast(errText(e), { kind: 'err' }); }
    busy = false;
    load();
  }
  el.innerHTML = '<div class="ac"><p class="empty">Loading...</p></div>';
  load();
  return () => clearTimeout(timer);
}

let stop = null;
registerAdminPage({ id: 'sound', title: 'Sounds', order: 67, render(el) { stop = soundPage(el); }, leave() { if (stop) stop(); } });
