// digest.js - the morning summary: its settings page (Settings > Alerts &
// connections > Morning summary, with a preview and "Send a summary now") and
// the page a summary notification opens (#digest?id=N, for everyone).

import { api, esc, toast, state, errText, liveSwitch, swHtml, formBar } from './core.js';
import { registerAdminPage } from './admin.js';
import { registerTab, isActive } from './nav.js';
import { lead, how } from './sp_kit.js';

const sw = (attrs, on, title, sub) => swHtml(attrs, on, title, sub);
const when = (ms) => new Date(ms).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
const CH_LABEL = { push: 'Browser notifications and apps', email: 'Email', telegram: 'Telegram', ntfy: 'ntfy', pushover: 'Pushover', webhook: 'Webhook' };
// Services a summary can go through (the others are for pictures and automations).
const CHANNELS = ['push', 'ntfy', 'telegram', 'email', 'pushover', 'webhook'];
const KIND_ICON = { person: '\u{1F464}', vehicle: '\u{1F697}', package: '\u{1F4E6}', zone: '\u{1F6A7}', unusual: '❓', tamper: '⚠️', offline: '\u{1F50C}' };

// A summary as the page shows it (r = the API answer).
export function digestHtml(r) {
  const d = r.digest;
  const busy = d.cameras.filter(c => c.events || c.tamper || c.offline_count);
  let h = '<div class="dg-head"><div class="dg-range">' + esc(r.period_name) + ' &middot; ' + esc(r.range) + '</div><div class="dg-over">' + esc(d.overview) + '</div></div>';
  if (!busy.length) h += '<p class="hint">Nothing happened at the cameras in this time.</p>';
  h += busy.map(c => '<div class="dg-cam"><div class="dg-cam-t">' + esc(c.camera) + '</div><div class="dg-line">' + esc(c.line) + '</div>' +
    (c.highlights.length ? '<div class="dg-hl">' + c.highlights.map(x => {
      const pic = x.thumb ? '<img loading="lazy" alt="" src="/api/events/' + encodeURIComponent(x.thumb) + '/thumb">' : '<span class="dg-ic">' + (KIND_ICON[x.kind] || '') + '</span>';
      return '<a class="dg-item" href="' + esc(x.url) + '">' + pic + '<span>' + esc(x.text) + '</span></a>';
    }).join('') + '</div>' : '') + '</div>').join('');
  return h;
}

// ---------------------------------------------------------------- settings page
function settingsPage(el) {
  let st = null, list = [], preview = null, busy = false;
  const cams = () => state.cams.filter(c => !c.remote);
  // One value, saved at once (the switches).
  async function save(patch) {
    const r = await api('/api/settings', { json: { digest: patch } });
    if (r.error) throw new Error(r.error);
  }
  async function load() {
    try {
      st = await api('/api/settings');
      st.digest.cameras = st.digest.cameras || [];
      list = (await api('/api/digests')).digests;
    } catch (e) { el.innerHTML = '<p class="empty">' + esc(errText(e)) + '</p>'; return; }
    paint();
  }
  function paint() {
    const s = st.digest;
    const chans = (st.alert_channels || []).filter(c => CHANNELS.includes(c.id));
    const all = s.channels == null;
    let h = '<div class="ac">' + lead('One message a day about what happened at your cameras overnight.', 'Turn it on, send it at about the time you wake up, and skip quiet days.') +
      how('It covers people (known by name or unknown), vehicles (known plates by name, unknown plates), packages, loitering and line crossings, tamper alerts, unusual events and cameras that went offline. It is worked out on this server from its own events; nothing leaves it. Each highlight links to its event. Push messages are short and link to the full summary page; email gets a richer version with pictures.') +
      '<div class="ac-card"><div class="ac-card-t">Schedule</div>' +
      sw('data-en data-live', s.enabled, 'Send a morning summary', 'Off until you turn it on.') +
      '<div class="ac-grid3"><label class="fld"><span class="fld-l">Send at</span><input type="time" data-time value="' + esc(s.time) + '"></label>' +
      '<label class="fld"><span class="fld-l">Covers</span><select data-period>' + [['overnight', 'Overnight (from the evening before)'], ['24h', 'The last 24 hours'], ['since_last', 'Since the last summary']].map(o => '<option value="' + o[0] + '"' + (s.period === o[0] ? ' selected' : '') + '>' + o[1] + '</option>').join('') + '</select></label>' +
      '<label class="fld" data-nightl' + (s.period === 'overnight' ? '' : ' hidden') + '><span class="fld-l">Night starts</span><input type="time" data-night value="' + esc(s.night_start) + '"></label></div>' +
      sw('data-skip data-live', s.skip_if_empty, 'Skip it when nothing happened', 'No message on a quiet day.') +
      '<div class="hint">Times are this server\'s local time. If the server was off at the sending time, it still sends within 3 hours.</div></div>' +
      '<div class="ac-card"><div class="ac-card-t">Send through</div>' +
      sw('data-chall data-live', all, 'Every service that is set up') +
      '<div class="dg-chans">' + chans.map(c => '<label class="asys-chk' + (c.configured ? '' : ' muted') + '"><input type="checkbox" data-ch="' + c.id + '"' + (all ? ' checked disabled' : (s.channels.includes(c.id) ? ' checked' : '')) + '> ' + esc(CH_LABEL[c.id] || c.id) + (c.configured ? '' : ' (not set up)') + '</label>').join('') + '</div>' +
      '<div class="hint">Services are set up under Detection &amp; alerts, Alerts. Browser notifications go to administrators. The link in a message needs the server address ("Address in links") set there.</div></div>' +
      '<div class="ac-card"><div class="ac-card-t">Cameras</div><div class="hint">None picked: all cameras.</div><div class="cs-groups" data-cams>' +
      cams().map(c => '<button type="button" class="chip' + (s.cameras.includes(c.name) ? ' on' : '') + '" data-cam="' + esc(c.name) + '" aria-pressed="' + s.cameras.includes(c.name) + '">' + esc(c.name) + '</button>').join('') + '</div></div>' +
      '<div class="ac-card"><div class="ac-card-t">Try it</div><div class="row"><button class="btn" data-prev>Preview</button><button class="btn primary" data-now' + (busy ? ' disabled' : '') + '>Send a summary now</button></div>' +
      '<div class="hint">The preview shows what the next summary would say, without sending anything. "Send a summary now" sends one for the same period to the services above (even when the schedule is off).</div>' +
      '<div data-prevbox class="dg-box">' + (preview ? digestHtml(preview) : '') + '</div></div>' +
      '<div class="ac-card"><div class="ac-card-t">Sent lately</div>' +
      (list.length ? '<div class="dg-list">' + list.map(x => '<a class="dg-row" href="#digest?id=' + x.id + '"><b>' + esc(x.range) + '</b><small>' + esc(when(x.created_ms)) + (x.auto ? '' : ' &middot; sent by hand') + '</small></a>').join('') + '</div>' : '<p class="hint">None yet.</p>') + '</div></div>';
    el.innerHTML = h;
    wire();
    // Time, period, services and cameras go through the one save bar; the switches save themselves.
    formBar(el.firstChild, { save: async () => {
      const patch = { time: q('[data-time]').value, period: q('[data-period]').value };
      const night = q('[data-night]'); if (night.value) patch.night_start = night.value;
      if (!patch.time) throw new Error('Pick the time to send at.');
      if (!q('[data-chall]').checked) patch.channels = [...el.querySelectorAll('[data-ch]:checked')].map(x => x.dataset.ch);
      patch.cameras = [...el.querySelectorAll('[data-cam][aria-pressed="true"]')].map(x => x.dataset.cam);
      await save(patch);
      await load();
    } });
  }
  const q = (x) => el.querySelector(x);
  function wire() {
    const s = st.digest;
    liveSwitch(q('[data-en]'), (on) => save({ enabled: on }));
    liveSwitch(q('[data-skip]'), (on) => save({ skip_if_empty: on }));
    const chosen = () => [...el.querySelectorAll('[data-ch]:checked')].map(x => x.dataset.ch);
    liveSwitch(q('[data-chall]'), async (on) => {
      const ch = on ? null : (s.channels || chosen());
      await save({ channels: ch });
      // Only the services row changes: update it in place so unsaved edits elsewhere stay.
      s.channels = ch;
      el.querySelectorAll('[data-ch]:not([data-off])').forEach(x => { x.disabled = on; if (on) x.checked = true; });
    });
    q('[data-period]').addEventListener('change', (e) => { q('[data-nightl]').hidden = e.target.value !== 'overnight'; });
    q('[data-cams]').onclick = (e) => {
      const b = e.target.closest('.chip'); if (!b) return;
      const on = b.getAttribute('aria-pressed') !== 'true';
      b.setAttribute('aria-pressed', on); b.classList.toggle('on', on);
    };
    q('[data-prev]').onclick = async () => {
      q('[data-prevbox]').innerHTML = '<p class="hint">Working it out...</p>';
      try { preview = await api('/api/digest/preview'); q('[data-prevbox]').innerHTML = digestHtml(preview); } catch (e) { q('[data-prevbox]').innerHTML = '<p class="hint warn">' + esc(errText(e)) + '</p>'; }
    };
    q('[data-now]').onclick = async () => {
      if (busy) return;
      busy = true; q('[data-now]').disabled = true;
      try {
        const r = await api('/api/digest/send', { json: {} });
        preview = r;
        const ok = r.sent.filter(x => x.ok).map(x => x.service), bad = r.sent.filter(x => !x.ok).map(x => x.service);
        toast(!r.sent.length ? 'Nothing to send through: no service is set up' : (ok.length ? 'Sent through ' + ok.join(', ') : 'Sending failed') + (bad.length ? '; failed: ' + bad.join(', ') : ''));
      } catch (e) { toast(errText(e)); }
      busy = false;
      await load();
    };
  }
  el.innerHTML = '<div class="ac"><p class="empty">Loading...</p></div>';
  load();
}

registerAdminPage({ id: 'digest', title: 'Morning summary', order: 67, render(el) { settingsPage(el); } });

// ---------------------------------------------------------------- the summary page
let root = null;
async function show() {
  if (!root) return;
  const id = new URLSearchParams(location.hash.split('?')[1] || '').get('id') || 'latest';
  root.innerHTML = '<div class="ac dg-page"><p class="empty">Loading...</p></div>';
  try {
    const r = await api('/api/digests/' + encodeURIComponent(id));
    root.innerHTML = '<div class="ac dg-page"><h3>Morning summary</h3>' + digestHtml(r) + '</div>';
  } catch (e) {
    root.innerHTML = '<div class="ac dg-page"><h3>Morning summary</h3><p class="hint">' + (e && e.status === 404 ? 'That summary is gone or does not exist.' : esc(errText(e))) + '</p></div>';
  }
}
registerTab({ id: 'digest', label: 'Summary', order: 66, noNav: true, init(e) { root = e; }, show });
// From one summary to another (the hash changes, the page stays).
window.addEventListener('hashchange', () => { if (isActive('digest')) show(); });
