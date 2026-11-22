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
import { t } from './i18n.js';
import { lead, how, more, notice, seg } from './sp_kit.js';

export function mountHomekit(el) {
  let st = null, timer = 0, tick = 0, busy = false, gone = false;

  el.innerHTML = '<p class="empty">' + t('homekit_settings.loading', 'Loading...') + '</p>';
  load();

  async function load() {
    try { st = got(await api('/api/homekit')); } catch (e) { if (!gone) el.innerHTML = '<p class="empty">' + t('homekit_settings.could_not_load', 'Could not load: ') + esc(errText(e)) + '</p>'; return; }
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
    const title = a.kind === 'bridge' ? t('homekit_settings.bridge_title', 'Bridge "{name}"', { name: esc(a.name) }) : esc(a.camera || a.name);
    const live = a.streams ? ' · ' + t('homekit_settings.live_views', '{n, plural, one {# live view} other {# live views}}', { n: a.streams }) : '';
    const key = a.kind === 'camera' ? esc(a.camera) : '';
    let body;
    if (a.paired) {
      body = '<p class="hint">' + t('homekit_settings.in_apple_home', 'In Apple Home ({n, plural, one {# Apple device} other {# Apple devices}} paired){live}.', { n: a.controllers.length, live }) + '</p>' +
        (a.kind === 'camera' ? '<button type="button" class="btn" data-unpair="' + key + '">' + t('homekit_settings.remove_pairings', 'Remove pairings') + '</button>' : '');
    } else if (a.too_many_tries) {
      body = notice('bad', t('homekit_settings.too_many_tries', 'Too many wrong setup codes were tried. Pairing is closed until you reset it below.'));
    } else if (a.pairing_open) {
      body = windowHtml(a);
    } else {
      body = '<p class="hint">' + t('homekit_settings.not_in_apple_home', 'Not in Apple Home yet.') + '</p><button type="button" class="btn primary" data-open="' + key + '">' + t('homekit_settings.pair_a_device', 'Pair a device') + '</button>';
    }
    return '<div class="sh-row"><b>' + title + '</b> <small class="muted">' + t('homekit_settings.port_n', 'port {port}', { port: a.port }) + '</small>' + body + '</div>';
  }

  // The open pairing window: the code, time left, and who is pairing now.
  function windowHtml(a) {
    const w = st.pairing || {};
    const since = w.pairing_for_seconds ? ' (' + t('homekit_settings.for_seconds', '{n} s', { n: w.pairing_for_seconds }) + ')' : '';
    const who = !w.pairing_from ? '<p class="hint">' + t('homekit_settings.waiting_for_device', 'Waiting for a device to start pairing...') + '</p>'
      : w.pairing_from_this_device
        ? notice('info', t('homekit_settings.pairing_now_this_device', 'Pairing now: <b>{from}</b> (this device){since}. It is the device showing this page.', { from: esc(w.pairing_from), since }),
            '<button type="button" class="btn" data-restart="' + esc(w.pairing_from) + '">' + t('homekit_settings.stop_and_start_again', 'Stop and start again') + '</button>')
        : notice('info', t('homekit_settings.pairing_now_other', 'Pairing now: <b>{from}</b>{since}. Not your device? Stop it: that address cannot pair again until this window closes.', { from: esc(w.pairing_from), since }),
            '<button type="button" class="btn danger" data-restart="' + esc(w.pairing_from) + '">' + t('homekit_settings.stop_and_start_again', 'Stop and start again') + '</button>');
    const refused = (w.refused || []).length ? '<p class="hint">' + t('homekit_settings.stopped_in_window', 'Stopped in this window: {list}', { list: w.refused.map(esc).join(', ') }) + '</p>' : '';
    return '<p class="hint" data-left-line>' + t('homekit_settings.pairing_open_for', '<b>Pairing is open for <span data-left>{time}</span>.</b> It closes when a device is added, or when you stop it.', { time: mmss(w.seconds_left || 0) }) + '</p>' +
      (a.qr_svg ? '<div class="hk-pair"><div class="acct-qr">' + a.qr_svg + '</div><div>' +
        '<p class="hint">' + t('homekit_settings.scan_code', 'In the Home app on an iPhone or iPad: <b>+</b>, <b>Add Accessory</b>, then scan this code, or tap <b>More options</b> and enter the setup code:') + '</p>' +
        (st.setup_code ? '<p class="hk-code"><code>' + esc(st.setup_code) + '</code></p>' : '') +
        '<p class="hint">' + t('homekit_settings.not_certified', 'Home says the accessory is not certified: that is expected, choose <b>Add Anyway</b>. The phone must be on the same network as this server.') + '</p></div></div>' : '') +
      who + refused +
      '<button type="button" class="btn" data-stop>' + t('homekit_settings.stop', 'Stop') + '</button>';
  }

  function pairingHtml() {
    if (!st.enabled) return '';
    const accs = st.accessories || [];
    if (!accs.length) return '<p class="hint">' + (standalone() ? t('homekit_settings.switch_on_camera', 'Switch on a camera below: each one is added to Apple Home on its own.') : t('homekit_settings.starting', 'Starting...')) + '</p>';
    return '<div class="sp-grp"><b>' + (standalone() ? t('homekit_settings.cameras_to_add', 'Cameras to add in the Home app') : t('homekit_settings.add_to_apple_home', 'Add to Apple Home')) + '</b></div>' +
      '<p class="hint">' + (standalone()
        ? t('homekit_settings.pair_window_standalone', 'Apple devices can pair only after you press <b>Pair a device</b> for a camera, for 5 minutes. The setup code and QR code show then (the same code for every camera).')
        : t('homekit_settings.pair_window_bridged', 'Apple devices can pair only after you press <b>Pair a device</b>, for 5 minutes. The setup code and QR code show then.')) + '</p>' +
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
      el.querySelectorAll('[data-left-line]').forEach(n => { n.innerHTML = '<b>' + t('homekit_settings.pairing_closed_now', 'Pairing has closed.') + '</b>'; });
    };
    step();
    tick = setInterval(step, 1000);
  }

  function paint() {
    const warn = [];
    if (st.error) warn.push(notice('bad', '<b>' + esc(st.error) + '</b>'));
    if (st.firewall_note) warn.push(notice('warn', esc(st.firewall_note)));
    else if (st.enabled && !st.listening) warn.push('<div class="hint">' + t('homekit_settings.starting', 'Starting...') + '</div>');
    const shown = st.cameras.filter(c => c.homekit).length;
    const cams = st.cameras.length ? st.cameras.map(c => '<div class="sh-row" data-cam="' + esc(c.name) + '">' + swHtml('data-hkcam', c.homekit, esc(c.name), '') + '</div>').join('') : '<p class="empty">' + t('homekit_settings.no_cameras_yet', 'No cameras yet.') + '</p>';
    const onText = st.enabled ? (st.listening ? (standalone() ? t('homekit_settings.on_standalone', 'On. Each camera on its own, ports from {port}.', { port: st.port + 1 }) : t('homekit_settings.on_bridged', 'On. Bridge "{name}" on port {port}.', { name: esc(st.bridge_name), port: st.port })) : t('homekit_settings.on_not_listening', 'On. Not listening yet.')) : t('homekit_settings.off', 'Off');
    el.innerHTML = '<div class="ac">' + lead(t('homekit_settings.lead_main', 'Shows your cameras in Apple Home, with live video, motion alerts and snapshots.'), t('homekit_settings.lead_sub', 'Leave it off unless you use the Home app.')) +
      notice('info', t('homekit_settings.tested_with', '<b>Tested with an iPhone and a HomePod mini hub</b> (one camera per accessory). If you try another setup, please tell us how it went in GitHub Discussions.')) +
      how(t('homekit_settings.how', 'Each camera you switch on below is a HomeKit camera with live video, a motion sensor (Home can notify you of motion) and a snapshot picture. Live video is made for Home at the size it asks for (on the graphics card when there is one, else on the processor; sound as Opus when the camera has sound). By default each camera is added to Apple Home on its own, as Apple prefers for cameras: add each one in the Home app, with the same setup code. <b>Bridged</b> puts them all behind one bridge you add once. Apple devices find AmberSight over Bonjour (mDNS, UDP port 5353), so they must be on the same network (or reach it through a home hub such as a HomePod or Apple TV). <b>HomeKit does not know AmberSight\'s accounts:</b> everyone in the Apple Home you add them to sees the cameras you switch on here, whatever their AmberSight account may see.')) +
      warn.join('') +
      '<div class="ac-card" data-hkcard>' + swHtml('data-en', st.enabled, t('homekit_settings.apple_home_homekit', 'Apple Home (HomeKit)'), onText) +
      '<div class="fld"><span class="fld-l">' + t('homekit_settings.cameras_in_home', 'Cameras in Home') + '</span>' + seg('hkmode', t('homekit_settings.cameras_in_home', 'Cameras in Home'), [['standalone', t('homekit_settings.each_on_its_own', 'Each on its own')], ['bridged', t('homekit_settings.behind_one_bridge', 'Behind one bridge')]], st.mode, 'data-nt') + '</div>' +
      '<div class="hint fld-h">' + t('homekit_settings.changing_this', 'Changing this means adding the cameras to Apple Home again.') + '</div>' +
      more(t('homekit_settings.more_settings', 'More settings'), fld(t('homekit_settings.bridge_name', 'Bridge name'), 'data-name maxlength="63" value="' + esc(st.name || '') + '" placeholder="' + esc(st.bridge_name) + '"', '', t('homekit_settings.bridge_name_hint', "The name Home shows for the bridge (bridged). Empty: this server's name."), 'text') +
        fld(t('homekit_settings.port_label', 'Port'), 'min="1" max="65535" data-port value="' + st.port + '"', '', t('homekit_settings.port_hint', 'Default 51830 (Homebridge uses 51826). Standalone cameras use the next ports, one each. Open these TCP ports, UDP 5353 (Bonjour) and UDP 51900-51999 (live video, [homekit] rtp_ports) in the firewall; on Windows AmberSight adds the rule itself when it runs as Administrator.'))) + '</div>' +
      pairingHtml() +
      '<div class="sp-grp"><b>' + t('homekit_settings.cameras_in_apple_home', 'Cameras in Apple Home') + '</b></div>' +
      '<p class="hint">' + (shown ? t('homekit_settings.cameras_shown', '{n, plural, one {# camera is} other {# cameras are}} shown in Apple Home.', { n: shown }) : t('homekit_settings.no_camera_shown', 'No camera is shown yet: switch on the ones to show.')) + ' ' + t('homekit_settings.same_switch', "The same switch is in each camera's settings (Audio &amp; text).") + '</p>' +
      '<div class="sh-list">' + cams + '</div>' +
      (st.enabled || st.paired ? '<div class="sp-grp"><b>' + t('homekit_settings.reset_pairing', 'Reset pairing') + '</b></div><p class="hint">' + t('homekit_settings.reset_hint', 'Every camera (and the bridge) forgets its Apple devices and a new setup code is made. Not needed to add one again: a camera or bridge removed in the Home app can be added again with the same code, after <b>Pair a device</b>.') + '</p><button type="button" class="btn danger" data-reset>' + t('homekit_settings.reset_pairing', 'Reset pairing') + '</button>' : '') +
      '</div>';
    liveSwitch(el.querySelector('[data-en]'), (on) => save({ enabled: on }));
    el.querySelectorAll('input[name="hkmode"]').forEach(r => r.addEventListener('change', async () => {
      const yes = await confirmDialog({ title: t('homekit_settings.change_mode_title', 'Change how cameras are in Home?'), body: t('homekit_settings.change_mode_body', 'The cameras then have to be added to Apple Home again, and the old ones removed there.'), ok: t('homekit_settings.change', 'Change') });
      if (!yes) { paint(); return; }
      try { await save({ mode: r.value }); toast(t('homekit_settings.saved', 'Saved')); } catch (e) { toast(errText(e), { kind: 'err' }); paint(); }
    }));
    formBar(el.querySelector('[data-hkcard]'), { save: async () => {
      const v = parseInt(el.querySelector('[data-port]').value, 10);
      if (!(v >= 1 && v <= 65535)) throw new Error(t('homekit_settings.port_range', 'Enter a port between 1 and 65535.'));
      await save({ port: v, name: el.querySelector('[data-name]').value.trim() });
      paint(); load();
    } });
    el.querySelectorAll('[data-cam]').forEach(row => liveSwitch(row.querySelector('[data-hkcam]'), (on) => save({ cameras: { [row.dataset.cam]: on } })));
    el.querySelectorAll('[data-open]').forEach(b => { b.onclick = () => pairCall(b.dataset.open ? { action: 'open', camera: b.dataset.open } : { action: 'open' }); });
    const sb = el.querySelector('[data-stop]');
    if (sb) sb.onclick = () => pairCall({ action: 'stop' }, t('homekit_settings.pairing_closed', 'Pairing closed'));
    const rs = el.querySelector('[data-restart]');
    if (rs) rs.onclick = async () => {
      const yes = await confirmDialog({ title: t('homekit_settings.stop_pairing_title', 'Stop this pairing?'), body: t('homekit_settings.stop_pairing_body', 'The device at {address} is stopped and cannot pair again until this window closes. Pairing stays open for 5 more minutes for your own device.', { address: rs.dataset.restart }), ok: t('homekit_settings.stop_and_start_again', 'Stop and start again'), danger: true });
      if (yes) pairCall({ action: 'restart', address: rs.dataset.restart }, t('homekit_settings.stopped_open_again', 'Stopped; pairing is open again'));
    };
    el.querySelectorAll('[data-unpair]').forEach(b => { b.onclick = async () => {
      const yes = await confirmDialog({ title: t('homekit_settings.remove_title', "Remove this camera's pairings?"), body: t('homekit_settings.remove_body', 'Apple Home loses this camera. It can be added again with the same setup code (press Pair a device first).'), ok: t('homekit_settings.remove', 'Remove'), danger: true });
      if (!yes) return;
      busy = true;
      try { st = got(await api('/api/homekit/unpair', { method: 'POST', json: { camera: b.dataset.unpair } })); toast(t('homekit_settings.pairings_removed', 'Pairings removed')); }
      catch (e) { toast(errText(e), { kind: 'err' }); }
      finally { busy = false; }
      if (!gone) { paint(); load(); }
    }; });
    const rb = el.querySelector('[data-reset]');
    if (rb) rb.onclick = async () => {
      const yes = await confirmDialog({ title: t('homekit_settings.reset_title', 'Reset HomeKit pairing?'), body: t('homekit_settings.reset_body', 'Every paired Apple device loses every camera (and the bridge), and a new setup code is made. Home then sees new accessories: remove the old ones there, and set up rooms, scenes and automations again.'), ok: t('homekit_settings.reset', 'Reset'), danger: true });
      if (!yes) return;
      busy = true;
      try { st = got(await api('/api/homekit/reset', { method: 'POST', json: {} })); toast(t('homekit_settings.pairing_reset', 'Pairing reset')); }
      catch (e) { toast(errText(e), { kind: 'err' }); }
      finally { busy = false; }
      if (!gone) { paint(); load(); }
    };
    countdown();
  }

  return () => { gone = true; clearTimeout(timer); clearInterval(tick); };
}
