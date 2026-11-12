// share_settings.js - Settings > Connections > Share streams: let other apps (Home Assistant, Scrypted, VLC,
// another NVR) use AmberSight as their video source over RTSP, without connecting to the cameras.
//   GET /api/restream   {enabled, port, listening, error, auth, cameras: [{id, name, slug, share, has_sub, blocked, masked, unmasked_ok, waiting, main, sub}]}
//   PUT /api/restream   {enabled?, port?, cameras?: {name: bool}, unmasked?: {name: bool}}   (admin)

import { api, esc, toast, errText, liveSwitch, swHtml, fld, formBar, hasUnsaved, ic } from './core.js';
import { lead, how, more, notice } from './sp_kit.js';

export function mountShare(el) {
  let st = null, timer = 0, busy = false, gone = false;
  const host = location.hostname.includes(':') ? '[' + location.hostname + ']' : location.hostname;

  el.innerHTML = '<p class="empty">Loading...</p>';
  load();

  async function load() {
    try { st = await api('/api/restream'); } catch (e) { if (!gone) el.innerHTML = '<p class="empty">Could not load: ' + esc(errText(e)) + '</p>'; return; }
    if (gone) return;
    paint();
    clearTimeout(timer);
    // Viewer counts and stream state change on their own.
    timer = setTimeout(() => { if (!gone && el.isConnected && !busy && !hasUnsaved() && !el.querySelector('input:focus')) load(); }, 5000);
  }

  // PUT part of the sharing settings; throws when the server refuses. The page then shows the new state
  // (unless the port is being edited: that is kept).
  async function save(body) {
    busy = true;
    try { st = await api('/api/restream', { method: 'PUT', json: body }); }
    finally { busy = false; }
    if (!gone && !hasUnsaved()) { paint(); load(); }
  }

  function urlRow(label, url) {
    return '<div class="cs-pushrow"><span>' + label + '</span><code>' + esc(url) + '</code>' +
      '<button type="button" class="cs-reset" data-copytext="' + esc(url) + '">Copy</button></div>';
  }

  function streamInfo(s) {
    if (!s) return '';
    const bits = [];
    if (s.live) {
      bits.push(s.codec + (s.width ? ' ' + s.width + 'x' + s.height : '') + (s.audio ? ' + audio' : ', video only'));
      bits.push(s.clients ? s.clients + (s.clients === 1 ? ' viewer' : ' viewers') : 'no viewers');
    } else bits.push('not streaming');
    return bits.join(', ');
  }

  // One compact row per camera: name, switch, copy-address button. Details only when shared.
  function camHtml(c) {
    const creds = st.auth ? 'USER:PASSWORD@' : '';
    const base = 'rtsp://' + creds + host + ':' + st.port + '/' + c.slug;
    const live = c.share && !c.blocked && !c.waiting;
    const sub = c.blocked ? esc(c.blocked) : c.share && c.waiting ? 'Waiting for your confirmation below.' : c.share ? (st.enabled ? 'Shared. ' : 'Marked to share; sharing is off. ') + esc(streamInfo(c.main)) : '';
    let h = '<div class="sh-row" data-cam="' + esc(c.name) + '"><div class="sh-main">' + swHtml('data-share', c.share, esc(c.name), sub, !!c.blocked) +
      (live ? '<button type="button" class="btn sh-copy" data-copytext="' + esc(base) + '" title="Copy this camera\'s stream address">' + ic('share') + ' Copy address</button>' : '') + '</div>';
    if (c.masked && c.share && !c.blocked) {
      h += '<div class="hint' + (c.unmasked_ok ? '' : ' warn') + '"><b>This camera has a privacy mask or text overlay.</b> Shared streams are passed on exactly as the camera sends them, so other apps see the picture <b>without</b> the mask or overlay.</div>' +
        swHtml('data-unmasked', c.unmasked_ok, 'I understand: share it without the mask', c.unmasked_ok ? 'Shared without the mask or overlay.' : 'Not shared until you turn this on.');
    }
    if (live) {
      h += '<details class="sh-more"><summary>Other addresses</summary><div class="cs-push">' + (c.has_sub ? urlRow('Sub stream', base + '/sub') + '<div class="hint">Sub stream: ' + esc(streamInfo(c.sub)) + '</div>' : '') +
        urlRow('Snapshot', location.origin + '/api/snapshot/' + c.id) + urlRow('MJPEG', location.origin + '/api/live/' + c.id) + '</div></details>';
    }
    return h + '</div>';
  }

  function paint() {
    const warn = [];
    const shareable = st.cameras.filter(c => !c.blocked);
    const anyShared = st.cameras.some(c => c.share && !c.blocked);
    const allShared = shareable.length > 0 && shareable.every(c => c.share);
    if (st.error) warn.push(notice('bad', '<b>' + esc(st.error) + '</b> Choose another port under More settings.'));
    else if (st.enabled && !st.listening) warn.push('<div class="hint">Opening the port...</div>');
    if (st.enabled && !st.auth) warn.push(notice('warn', 'No accounts are set up, so anyone who can reach this server can watch the shared streams.', '<a class="btn" href="#settings/users/users">Add an account</a>'));
    if (anyShared) warn.push(notice('warn', 'Shared streams are not encrypted: use them on your own network or over a private link such as Tailscale.'));
    const cams = st.cameras.length ? st.cameras.map(camHtml).join('') : '<p class="empty">No cameras yet.</p>';
    el.innerHTML = '<div class="ac">' + lead('Lets other apps, like Home Assistant or VLC, watch your cameras through AmberSight.', 'Leave it off unless you use another app that needs the video.') +
      how('Other apps connect here instead of to each camera, so the cameras only ever serve AmberSight. The video is passed on as it arrives (no re-encoding, almost no extra load), with sound when the camera has AAC audio. Off until you turn it on, and only for the cameras you switch on below. Apps sign in with an AmberSight account (user name and password, sent as HTTP Basic); accounts limited to some cameras only see those.') +
      warn.join('') +
      '<div class="ac-card" data-sharecard>' + swHtml('data-en', st.enabled, 'Share camera streams', st.enabled ? (st.listening ? 'On. Listening on port ' + st.port + '.' : 'On. Not listening yet.') : 'Off') +
      more('More settings', fld('Port', 'min="1" max="65535" data-port value="' + st.port + '"', '', 'Default 8554.')) + '</div>' +
      '<div class="sp-grp"><b>Cameras</b></div>' +
      (shareable.length > 1 ? swHtml('data-shareall', allShared, 'Share all cameras', allShared ? 'Every camera is shared.' : '') : '') +
      '<div class="sh-list">' + cams + '</div>' +
      more('How to use these addresses',
      '<p class="hint"><b>Home Assistant</b>: Settings, Devices &amp; services, Add integration, <b>Generic Camera</b>. Stream source: the main or sub stream address, with <b>USER</b> and <b>PASSWORD</b> filled in (a password with special characters must be percent-encoded, @ is %40). RTSP transport protocol: TCP. Still image URL: the Snapshot address, with the same user name and password in the authentication fields (basic); the MJPEG address works as a stream source too. If this server uses HTTPS with its own certificate, turn off certificate verification there.</p>' +
      '<p class="hint"><b>Scrypted</b>: add an <b>RTSP Camera</b> (or ONVIF-less generic) and paste the stream address; use the sub stream for low-resolution uses such as detection.</p>' +
      '<p class="hint"><b>VLC</b>: Media, Open Network Stream, paste the address (it asks for the user name and password if they are not in it). <b>Another NVR</b>: add a generic RTSP camera with the address; choose TCP.</p>' +
      '<p class="hint">Only TCP is offered (UDP is not). A camera with a privacy mask or text overlay is shared only after you confirm it, because the shared video shows the picture without them. The camera sound is shared when it is AAC; G.711 sound is not (video only).</p>') + '</div>';
    // Switches save when flipped; the port goes through the bar.
    liveSwitch(el.querySelector('[data-en]'), (on) => save({ enabled: on }));
    formBar(el.querySelector('[data-sharecard]'), { save: async () => {
      const port = el.querySelector('[data-port]');
      const v = parseInt(port.value, 10);
      if (!(v >= 1 && v <= 65535)) throw new Error('Enter a port between 1 and 65535.');
      await save({ port: v });
      paint(); load();
    } });
    const all = el.querySelector('[data-shareall]');
    if (all) liveSwitch(all, (on) => save({ cameras: Object.fromEntries(st.cameras.filter(c => !c.blocked).map(c => [c.name, on])) }));
    el.querySelectorAll('[data-cam]').forEach(card => {
      liveSwitch(card.querySelector('[data-share]'), (on) => save({ cameras: { [card.dataset.cam]: on } }));
      const um = card.querySelector('[data-unmasked]');
      if (um) liveSwitch(um, (on) => save({ unmasked: { [card.dataset.cam]: on } }));
    });
  }

  el.addEventListener('click', async (e) => {
    const b = e.target.closest('[data-copytext]'); if (!b) return;
    try { await navigator.clipboard.writeText(b.dataset.copytext); const old = b.innerHTML; b.textContent = 'Copied'; setTimeout(() => { b.innerHTML = old; }, 1200); }
    catch (err) { toast('Copy did not work here: select the text instead'); }
  });
  return () => { gone = true; clearTimeout(timer); };
}
