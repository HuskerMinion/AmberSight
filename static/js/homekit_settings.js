// homekit_settings.js - Settings > Connections > Apple Home: the cameras switched on here in Apple Home, with
// a motion sensor, snapshots and live video. Each camera on its own (standalone, the default) or behind one bridge.
//   GET  /api/homekit         {enabled, port, bind, mode, name, bridge_name, listening, error, sessions, live_streams, paired,
//                              setup_code?, cameras: [{name, homekit}],
//                              accessories: [{kind, camera?, name, port, paired, too_many_tries, controllers, streams, pairing_open, setup_uri?, qr_svg?}],
//                              pairing: null | {kind, camera?, name, seconds_left, pairing_from?, pairing_for_seconds?, pairing_from_this_device, refused: []}}
//   PUT  /api/homekit         {enabled?, name?, port?, mode?, cameras?: {name: bool}}   (admin)
//   POST /api/homekit/pairing {action: 'open', camera?} | {action: 'stop'} | {action: 'restart', address}: the pairing
//                              window; Apple devices can pair only while it is open (5 minutes)               (admin)
//   POST /api/homekit/reset   forget every paired Apple device, new setup code          (admin)
//   POST /api/homekit/unpair  {camera}: one standalone camera forgets its Apple devices (admin)

import { api, esc, errText, liveSwitch, swHtml, fld, formBar, hasUnsaved, confirmDialog, toast } from './core.js';
import { lead, how, more, notice, seg } from './sp_kit.js';

export function mountHomekit(el) {
  let st = null, timer = 0, tick = 0, busy = false, gone = false;

  el.innerHTML = '<p class="empty">Loading...</p>';
  load();

  async function load() {
    try { st = got(await api('/api/homekit')); } catch (e) { if (!gone) el.innerHTML = '<p class="empty">Could not load: ' + esc(errText(e)) + '</p>'; return; }
    if (gone) return;
    paint();
    clearTimeout(timer);
    // Pairing happens on the phone: notice it without a reload (quicker while
    // the pairing window is open, to show who is pairing).
    timer = setTimeout(() => { if (!gone && el.isConnected && !busy && !hasUnsaved() && !el.querySelector('input:focus')) load(); }, st.pairing ? 1500 : 4000);
  }

  async function save(body) {
    busy = true;
    try { st = got(await api('/api/homekit', { method: 'PUT', json: body })); }
    finally { busy = false; }
    if (!gone && !hasUnsaved()) { paint(); load(); }
  }

  // When the answer came: the time left counts from then.
  // (A function declaration: load() runs before this line is reached.)
  function got(x) {
    if (x && x.pairing) x.pairing.ends = Date.now() + (x.pairing.seconds_left || 0) * 1000;
    return x;
  }

  const standalone = () => st.mode !== 'bridged';

  const mmss = (n) => Math.floor(n / 60) + ':' + String(n % 60).padStart(2, '0');

  // One accessory: who it is paired with, or a button to open pairing (its QR code while open).
  function accHtml(a) {
    const title = a.kind === 'bridge' ? 'Bridge "' + esc(a.name) + '"' : esc(a.camera || a.name);
    const live = a.streams ? ' · ' + a.streams + ' live ' + (a.streams === 1 ? 'view' : 'views') : '';
    const key = a.kind === 'camera' ? esc(a.camera) : '';
    let body;
    if (a.paired) {
      body = '<p class="hint">In Apple Home (' + a.controllers.length + (a.controllers.length === 1 ? ' Apple device' : ' Apple devices') + ' paired)' + live + '.</p>' +
        (a.kind === 'camera' ? '<button type="button" class="btn" data-unpair="' + key + '">Remove pairings</button>' : '');
    } else if (a.too_many_tries) {
      body = notice('bad', 'Too many wrong setup codes were tried. Pairing is closed until you reset it below.');
    } else if (a.pairing_open) {
      body = windowHtml(a);
    } else {
      body = '<p class="hint">Not in Apple Home yet.</p><button type="button" class="btn primary" data-open="' + key + '">Pair a device</button>';
    }
    return '<div class="sh-row"><b>' + title + '</b> <small class="muted">port ' + a.port + '</small>' + body + '</div>';
  }

  // The open pairing window: the code, time left, and who is pairing now.
  function windowHtml(a) {
    const w = st.pairing || {};
    const since = w.pairing_for_seconds ? ' (' + w.pairing_for_seconds + ' s)' : '';
    const who = !w.pairing_from ? '<p class="hint">Waiting for a device to start pairing...</p>'
      : w.pairing_from_this_device
        ? notice('info', 'Pairing now: <b>' + esc(w.pairing_from) + '</b> (this device)' + since + '. It is the device showing this page.',
            '<button type="button" class="btn" data-restart="' + esc(w.pairing_from) + '">Stop and start again</button>')
        : notice('info', 'Pairing now: <b>' + esc(w.pairing_from) + '</b>' + since + '. Not your device? Stop it: that address cannot pair again until this window closes.',
            '<button type="button" class="btn danger" data-restart="' + esc(w.pairing_from) + '">Stop and start again</button>');
    const refused = (w.refused || []).length ? '<p class="hint">Stopped in this window: ' + w.refused.map(esc).join(', ') + '</p>' : '';
    return '<p class="hint" data-left-line><b>Pairing is open for <span data-left>' + mmss(w.seconds_left || 0) + '</span>.</b> It closes when a device is added, or when you stop it.</p>' +
      (a.qr_svg ? '<div class="hk-pair"><div class="acct-qr">' + a.qr_svg + '</div><div>' +
        '<p class="hint">In the Home app on an iPhone or iPad: <b>+</b>, <b>Add Accessory</b>, then scan this code, or tap <b>More options</b> and enter the setup code:</p>' +
        (st.setup_code ? '<p class="hk-code"><code>' + esc(st.setup_code) + '</code></p>' : '') +
        '<p class="hint">Home says the accessory is not certified: that is expected, choose <b>Add Anyway</b>. The phone must be on the same network as this server.</p></div></div>' : '') +
      who + refused +
      '<button type="button" class="btn" data-stop>Stop</button>';
  }

  function pairingHtml() {
    if (!st.enabled) return '';
    const accs = st.accessories || [];
    if (!accs.length) return '<p class="hint">' + (standalone() ? 'Switch on a camera below: each one is added to Apple Home on its own.' : 'Starting...') + '</p>';
    return '<div class="sp-grp"><b>' + (standalone() ? 'Cameras to add in the Home app' : 'Add to Apple Home') + '</b></div>' +
      '<p class="hint">Apple devices can pair only after you press <b>Pair a device</b>' + (standalone() ? ' for a camera' : '') + ', for 5 minutes. The setup code and QR code show then' + (standalone() ? ' (the same code for every camera)' : '') + '.</p>' +
      '<div class="sh-list">' + accs.map(accHtml).join('') + '</div>';
  }

  async function pairCall(body, done) {
    busy = true;
    try { st = got(await api('/api/homekit/pairing', { method: 'POST', json: body })); if (done) toast(done); }
    catch (e) { toast(errText(e), { kind: 'err' }); }
    finally { busy = false; }
    if (!gone) { paint(); load(); }
  }

  // The time left counts down every second on its own (polling pauses while
  // something is being saved, edited or typed in); at 0 the code and QR code
  // go away here at once, whatever the next answer is.
  function countdown() {
    clearInterval(tick);
    tick = 0;
    if (!st || !st.pairing) return;
    const end = st.pairing.ends || 0;
    const step = () => {
      if (gone) { clearInterval(tick); return; }
      const left = Math.max(0, Math.ceil((end - Date.now()) / 1000));
      el.querySelectorAll('[data-left]').forEach(n => { n.textContent = mmss(left); });
      if (left > 0) return;
      clearInterval(tick);
      el.querySelectorAll('.hk-pair, .hk-code').forEach(n => n.remove());
      el.querySelectorAll('[data-left-line]').forEach(n => { n.innerHTML = '<b>Pairing has closed.</b>'; });
    };
    step();
    tick = setInterval(step, 1000);
  }

  function paint() {
    const warn = [];
    if (st.error) warn.push(notice('bad', '<b>' + esc(st.error) + '</b>'));
    if (st.firewall_note) warn.push(notice('warn', esc(st.firewall_note)));
    else if (st.enabled && !st.listening) warn.push('<div class="hint">Starting...</div>');
    const shown = st.cameras.filter(c => c.homekit).length;
    const cams = st.cameras.length ? st.cameras.map(c => '<div class="sh-row" data-cam="' + esc(c.name) + '">' + swHtml('data-hkcam', c.homekit, esc(c.name), '') + '</div>').join('') : '<p class="empty">No cameras yet.</p>';
    const onText = st.enabled ? (st.listening ? 'On. ' + (standalone() ? 'Each camera on its own, ports from ' + (st.port + 1) + '.' : 'Bridge "' + esc(st.bridge_name) + '" on port ' + st.port + '.') : 'On. Not listening yet.') : 'Off';
    el.innerHTML = '<div class="ac">' + lead('Shows your cameras in Apple Home, with live video, motion alerts and snapshots.', 'Leave it off unless you use the Home app.') +
      notice('warn', '<b>Not yet tested with real Apple devices.</b> It follows the HomeKit specification and works with open-source HomeKit controllers. If you try it with an iPhone, please tell us how it went in GitHub Discussions.') +
      how('Each camera you switch on below is a HomeKit camera with live video, a motion sensor (Home can notify you of motion) and a snapshot picture. Live video is made for Home at the size it asks for (on the graphics card when there is one, else on the processor; sound as Opus when the camera has sound). By default each camera is added to Apple Home on its own, as Apple prefers for cameras: add each one in the Home app, with the same setup code. <b>Bridged</b> puts them all behind one bridge you add once. Apple devices find AmberSight over Bonjour (mDNS, UDP port 5353), so they must be on the same network (or reach it through a home hub such as a HomePod or Apple TV). <b>HomeKit does not know AmberSight\'s accounts:</b> everyone in the Apple Home you add them to sees the cameras you switch on here, whatever their AmberSight account may see.') +
      warn.join('') +
      '<div class="ac-card" data-hkcard>' + swHtml('data-en', st.enabled, 'Apple Home (HomeKit)', onText) +
      '<div class="fld"><span class="fld-l">Cameras in Home</span>' + seg('hkmode', 'Cameras in Home', [['standalone', 'Each on its own'], ['bridged', 'Behind one bridge']], st.mode, 'data-nt') + '</div>' +
      '<div class="hint fld-h">Changing this means adding the cameras to Apple Home again.</div>' +
      more('More settings', fld('Bridge name', 'data-name maxlength="63" value="' + esc(st.name || '') + '" placeholder="' + esc(st.bridge_name) + '"', '', 'The name Home shows for the bridge (bridged). Empty: this server\'s name.', 'text') +
        fld('Port', 'min="1" max="65535" data-port value="' + st.port + '"', '', 'Default 51830 (Homebridge uses 51826). Standalone cameras use the next ports, one each. Open these TCP ports, UDP 5353 (Bonjour) and UDP 51900-51999 (live video, [homekit] rtp_ports) in the firewall; on Windows AmberSight adds the rule itself when it runs as Administrator.')) + '</div>' +
      pairingHtml() +
      '<div class="sp-grp"><b>Cameras in Apple Home</b></div>' +
      '<p class="hint">' + (shown ? shown + (shown === 1 ? ' camera is' : ' cameras are') + ' shown in Apple Home.' : 'No camera is shown yet: switch on the ones to show.') + ' The same switch is in each camera\'s settings (Audio &amp; text).</p>' +
      '<div class="sh-list">' + cams + '</div>' +
      (st.enabled || st.paired ? '<div class="sp-grp"><b>Reset pairing</b></div><p class="hint">Every camera (and the bridge) forgets its Apple devices and a new setup code is made. Not needed to add one again: a camera or bridge removed in the Home app can be added again with the same code, after <b>Pair a device</b>.</p><button type="button" class="btn danger" data-reset>Reset pairing</button>' : '') +
      '</div>';
    liveSwitch(el.querySelector('[data-en]'), (on) => save({ enabled: on }));
    el.querySelectorAll('input[name="hkmode"]').forEach(r => r.addEventListener('change', async () => {
      const yes = await confirmDialog({ title: 'Change how cameras are in Home?', body: 'The cameras then have to be added to Apple Home again, and the old ones removed there.', ok: 'Change' });
      if (!yes) { paint(); return; }
      try { await save({ mode: r.value }); toast('Saved'); } catch (e) { toast(errText(e), { kind: 'err' }); paint(); }
    }));
    formBar(el.querySelector('[data-hkcard]'), { save: async () => {
      const v = parseInt(el.querySelector('[data-port]').value, 10);
      if (!(v >= 1 && v <= 65535)) throw new Error('Enter a port between 1 and 65535.');
      await save({ port: v, name: el.querySelector('[data-name]').value.trim() });
      paint(); load();
    } });
    el.querySelectorAll('[data-cam]').forEach(row => liveSwitch(row.querySelector('[data-hkcam]'), (on) => save({ cameras: { [row.dataset.cam]: on } })));
    el.querySelectorAll('[data-open]').forEach(b => { b.onclick = () => pairCall(b.dataset.open ? { action: 'open', camera: b.dataset.open } : { action: 'open' }); });
    const sb = el.querySelector('[data-stop]');
    if (sb) sb.onclick = () => pairCall({ action: 'stop' }, 'Pairing closed');
    const rs = el.querySelector('[data-restart]');
    if (rs) rs.onclick = async () => {
      const yes = await confirmDialog({ title: 'Stop this pairing?', body: 'The device at ' + rs.dataset.restart + ' is stopped and cannot pair again until this window closes. Pairing stays open for 5 more minutes for your own device.', ok: 'Stop and start again', danger: true });
      if (yes) pairCall({ action: 'restart', address: rs.dataset.restart }, 'Stopped; pairing is open again');
    };
    el.querySelectorAll('[data-unpair]').forEach(b => { b.onclick = async () => {
      const yes = await confirmDialog({ title: 'Remove this camera\'s pairings?', body: 'Apple Home loses this camera. It can be added again with the same setup code (press Pair a device first).', ok: 'Remove', danger: true });
      if (!yes) return;
      busy = true;
      try { st = got(await api('/api/homekit/unpair', { method: 'POST', json: { camera: b.dataset.unpair } })); toast('Pairings removed'); }
      catch (e) { toast(errText(e), { kind: 'err' }); }
      finally { busy = false; }
      if (!gone) { paint(); load(); }
    }; });
    const rb = el.querySelector('[data-reset]');
    if (rb) rb.onclick = async () => {
      const yes = await confirmDialog({ title: 'Reset HomeKit pairing?', body: 'Every paired Apple device loses every camera (and the bridge), and a new setup code is made. Home then sees new accessories: remove the old ones there, and set up rooms, scenes and automations again.', ok: 'Reset', danger: true });
      if (!yes) return;
      busy = true;
      try { st = got(await api('/api/homekit/reset', { method: 'POST', json: {} })); toast('Pairing reset'); }
      catch (e) { toast(errText(e), { kind: 'err' }); }
      finally { busy = false; }
      if (!gone) { paint(); load(); }
    };
    countdown();
  }

  return () => { gone = true; clearTimeout(timer); clearInterval(tick); };
}
