// camaddr_form.js - the address part of a network camera's Stream settings: protocol, address
// and port, user name and password, and the main and sub stream paths, with brand presets,
// "Find via ONVIF" and a switch to the two full addresses. The hidden-or-shown full address
// inputs (data-k url / sub_url) always hold what is saved, so the rest of the form reads them
// as before. Split and join live in camaddr.js.

import { t } from './i18n.js';
import { api, esc } from './core.js';
import { PROTOCOLS, DEFAULT_PORT, BRANDS, splitUrl, hasScheme, fieldsFromUrls, urlsFromFields, addrProblems, brandOf, brandPort, batteryUrl, normUid, addrUid } from './camaddr.js';

const GUIDE = 'https://github.com/HuskerMinion/AmberSight/blob/master/docs/cameras.md';
const WHY = {
  scheme: t('camaddr_form.scheme','This kind of address is edited as a full address.'),
  bad: t('camaddr_form.bad','This address has no protocol (like rtsp://), so it is shown as it is.'),
  'query-login': t('camaddr_form.query_login','This address carries its sign-in in it, so it is shown as a full address.'),
  'sub-place': t('camaddr_form.sub_place','The sub stream is on another address, port or protocol, so both are shown as full addresses.'),
  'sub-login': t('camaddr_form.sub_login','The sub stream signs in with its own user, so both are shown as full addresses.'),
};
const errText = (e) => { const b = (e && e.body) || ''; try { const j = JSON.parse(b); if (j && j.error) return j.error; } catch (x) { /* text */ } return b || (e && e.message) || String(e); };

const cell = (cls, label, input) => '<div class="ca-cell ' + cls + '"><label class="cs-f ca-f"><span>' + label + '</span>' + input + '</label></div>';
const inp = (k, val, extra) => '<input data-k="' + k + '" value="' + esc(val == null ? '' : val) + '" ' + (extra || '') + '>';

const BATTERY_HINT = t('camaddr_form.battery_hint','This camera has no video address: AmberSight talks to it directly and needs its UID (Reolink app, Device info).');
// (The brand note under the picker already shows BATTERY_HINT; the message line adds only this.)
const BATTERY_PICKED = t('camaddr_form.battery_picked','Battery camera is on, with 30 s idle, and recording is motion only. Set up the camera and your network as the camera guide says.');

function brandOptions(cur) {
  const opt = (b) => '<option value="' + b.id + '"' + (b.id === cur ? ' selected' : '') + '>' + esc(b.name) + '</option>';
  return '<option value="">' + t('camaddr_form.pick_brand', 'Pick a brand...') + '</option>' +
    '<optgroup label="' + t('camaddr_form.tested', 'Tested') + '">' + BRANDS.filter(b => b.tested && !b.battery).map(opt).join('') + '</optgroup>' +
    '<optgroup label="' + t('camaddr_form.battery_nohub', 'Battery camera, no hub') + '">' + BRANDS.filter(b => b.battery).map(opt).join('') + '</optgroup>' +
    '<optgroup label="' + t('camaddr_form.not_tested', 'Not tested') + '">' + BRANDS.filter(b => !b.tested).map(opt).join('') + '</optgroup>';
}

// The address fields for camera c. `net`: the camera is a network camera now (its url belongs here).
export function addrHtml(c, isNew, net) {
  const url = net ? (c.url || '') : '';
  const f = fieldsFromUrls(url, net ? c.sub_url : '', c.username);
  const full = f.mode === 'full';
  const bat = f.mode === 'battery';
  const uid = bat ? f.uid : '';
  const ns = 'data-nosig spellcheck="false" autocomplete="off" autocapitalize="off"';
  return '<div class="ca" data-addr data-mode="' + f.mode + '">' +
    '<div class="ca-r1" data-ca-split' + (full ? ' hidden' : '') + '>' +
      cell('ca-host', t('camaddr_form.camera_address','Camera address'), inp('addr_host', f.host, ns + ' inputmode="url" placeholder="192.168.1.50"')) +
      cell('ca-port', t('camaddr_form.port','Port'), inp('addr_port', f.port, 'data-nosig inputmode="numeric" maxlength="5" autocomplete="off" placeholder="' + (bat ? t('camaddr_form.usual','usual') : DEFAULT_PORT[f.scheme] || 554) + '"')) +
      cell('ca-proto', t('camaddr_form.protocol','Protocol'), '<select data-k="addr_scheme" data-nosig>' + PROTOCOLS.map(([v, label]) => '<option value="' + v + '"' + (v === f.scheme ? ' selected' : '') + '>' + label + '</option>').join('') + '</select>').replace('<div class="ca-cell', '<div data-ca-nobat' + (bat ? ' hidden' : '') + ' class="ca-cell') +
    '</div>' +
    '<div class="ca-r2" data-ca-bat' + (bat ? '' : ' hidden') + '>' +
      cell('ca-user', t('camaddr_form.uid_label','UID <em>Reolink app, Device info</em>'), inp('uid', uid, 'spellcheck="false" autocomplete="off" autocapitalize="characters" maxlength="40" placeholder="95270000ABCDEFGH"')) +
      (isNew ? '' : '<div class="ca-cell ca-batdef"><button type="button" class="btn" data-ca-batdef title="' + t('camaddr_form.battery_title', 'Battery camera on, 30 s idle, motion-only recording') + '">' + t('camaddr_form.use_battery', 'Use battery settings') + '</button></div>') +
    '</div>' +
    '<div class="ca-full" data-ca-full' + (full ? '' : ' hidden') + '>' +
      cell('ca-wide', t('camaddr_form.main_stream_address','Main stream address <em>records, full size</em>'), inp('url', url, 'placeholder="rtsp://192.168.1.50:554/stream1" spellcheck="false" autocomplete="off"')) +
      cell('ca-wide', t('camaddr_form.sub_stream_address','Sub stream address <em>motion and the Live grid (optional)</em>'), inp('sub_url', net ? c.sub_url : '', 'placeholder="' + t('camaddr_form.sub_ph', 'lower resolution, for the grid and motion') + '" spellcheck="false" autocomplete="off"')) +
    '</div>' +
    '<div class="ca-r2">' +
      cell('ca-user', t('camaddr_form.user_name','User name'), inp('username', c.username, 'autocomplete="off" autocapitalize="off" spellcheck="false"')) +
      cell('ca-pass', t('camaddr_form.password', 'Password'), '<input data-k="password" type="password" autocomplete="new-password" placeholder="' + (isNew ? t('camaddr_form.camera_password','camera password') : t('camaddr_form.blank_keeps_saved','blank keeps the saved one')) + '">') +
    '</div>' +
    '<div class="ca-paths" data-ca-split data-ca-nobat' + (full || bat ? ' hidden' : '') + '>' +
      cell('ca-wide', t('camaddr_form.main_stream', 'Main stream <em>records, full size</em>'), inp('addr_main', f.main, ns + ' placeholder="/stream1"')) +
      cell('ca-wide', t('camaddr_form.sub_stream', 'Sub stream <em>motion and the Live grid (optional)</em>'), inp('addr_sub', f.sub, ns + ' placeholder="/stream2"')) +
      '<div class="ca-prev" data-ca-prev aria-live="off"></div>' +
    '</div>' +
    '<div class="ca-tools" data-ca-split' + (full ? ' hidden' : '') + '>' +
      '<label class="cs-f ca-f ca-brand"><span>' + t('camaddr_form.brand', 'Brand') + '</span><select data-ca-brand data-nosig aria-describedby="ca-brandhint">' + brandOptions(bat ? 'reolink-battery' : brandOf(f.main, f.sub)) + '</select></label>' +
      '<div class="ca-brandnote"><span id="ca-brandhint">' + (bat ? esc(BATTERY_HINT) : t('camaddr_form.fills_both_paths','Fills both paths.')) + '</span> <a href="' + GUIDE + '" target="_blank" rel="noopener noreferrer">' + t('camaddr_form.camera_guide', 'Camera guide') + '</a></div>' +
    '</div>' +
    '<div class="ca-acts">' +
      '<button type="button" class="btn" data-ca-onvif data-ca-split data-ca-nobat data-touch' + (full || bat ? ' hidden' : '') + '>' + t('camaddr_form.find_onvif', 'Find via ONVIF') + '</button>' +
      '<label class="asys-chk ca-fullchk"><input type="checkbox" data-ca-fullchk data-nosig' + (full ? ' checked' : '') + '> ' + t('camaddr_form.use_full', 'Use full addresses') + '</label>' +
    '</div>' +
    '<div class="ca-msg" data-ca-msg role="status" aria-live="polite">' + (full && f.why ? esc(WHY[f.why] || '') : '') + '</div>' +
    '<div class="ca-found" data-ca-found></div>' +
  '</div>';
}

// The checks for the save, as [data-k, message]. Empty when fine.
export function addrValidate(d) {
  const box = d.querySelector('[data-addr]');
  if (!box) return [];
  const v = (k) => { const el = box.querySelector('[data-k="' + k + '"]'); return el ? el.value : ''; };
  const full = box.dataset.mode === 'full';
  const bat = box.dataset.mode === 'battery';
  const f = full ? { url: v('url'), sub_url: v('sub_url') } : bat ? { host: v('addr_host'), port: v('addr_port'), uid: v('uid') } : { host: v('addr_host'), port: v('addr_port'), main: v('addr_main'), sub: v('addr_sub') };
  const key = { host: 'addr_host', port: 'addr_port', main: 'addr_main', sub: 'addr_sub', url: 'url', sub_url: 'sub_url', uid: 'uid' };
  return addrProblems(f, full ? 'full' : bat ? 'battery' : 'split').map(([k, m]) => [key[k], m]);
}
// The field a server message about the address belongs next to.
export function addrErrKey(d, k) {
  const box = d.querySelector('[data-addr]');
  if (!box || box.dataset.mode === 'full') return k;
  return k === 'sub_url' ? 'addr_sub' : 'addr_host';
}
// A sub stream address chosen elsewhere (Find the sub stream): into the fields too.
export function addrSetSub(d, u) {
  const box = d.querySelector('[data-addr]');
  if (box && box.__setSub) box.__setSub(u);
  else { const el = d.querySelector('[data-k="sub_url"]'); if (el) el.value = u; }
}

export function wireAddr(d, c, isNew) {
  const box = d.querySelector('[data-addr]');
  if (!box) return;
  const q = (s) => box.querySelector(s);
  const el = (k) => q('[data-k="' + k + '"]');
  const host = el('addr_host'), port = el('addr_port'), sch = el('addr_scheme'), main = el('addr_main'), sub = el('addr_sub');
  const url = el('url'), subUrl = el('sub_url'), user = el('username'), pw = el('password');
  const brand = q('[data-ca-brand]'), fullChk = q('[data-ca-fullchk]'), msgEl = q('[data-ca-msg]'), found = q('[data-ca-found]'), prev = q('[data-ca-prev]');
  const onvifBtn = q('[data-ca-onvif]');
  const uidEl = el('uid');
  const net = !/^(push|device|screen):\/\//i.test(c.url || '') && !c.capture;
  const storedUrl = net ? (c.url || '') : '', storedSub = net ? (c.sub_url || '') : '';
  const msg = (text) => { msgEl.textContent = text || ''; };
  const isFull = () => box.dataset.mode === 'full';
  const isBat = () => box.dataset.mode === 'battery';
  const fields = () => ({ scheme: sch.value, host: host.value.trim(), port: port.value.trim(), main: main.value, sub: sub.value });
  // Something changed without a keystroke (ONVIF, a sub stream from elsewhere): the form repaints.
  const changed = () => box.dispatchEvent(new Event('input', { bubbles: true }));

  const paintPrev = () => {
    const r = urlsFromFields(fields(), storedUrl, storedSub);
    const strip = (u) => { const s = splitUrl(u); return s && s.creds ? u.replace(/\/\/[^/]*@/, '//') : u; };
    prev.innerHTML = r.url ? '<span>' + t('camaddr_form.saves_as', 'Saves as:') + '</span><span class="ca-u">' + esc(strip(r.url)) + '</span>' + (r.sub_url ? '<span class="ca-u">' + esc(strip(r.sub_url)) + '</span>' : '') : '';
  };
  // Split fields -> the full addresses that are saved.
  let lastScheme = sch.value;
  const sync = () => {
    lastScheme = sch.value;
    port.placeholder = isBat() ? t('camaddr_form.usual', 'usual') : String(DEFAULT_PORT[sch.value] || '');
    if (isFull()) return;
    if (isBat()) {
      url.value = batteryUrl({ host: host.value.trim(), port: port.value.trim(), uid: uidEl.value }, storedUrl);
      subUrl.value = '';
      prev.innerHTML = '';
      return;
    }
    const r = urlsFromFields(fields(), storedUrl, storedSub);
    url.value = r.url; subUrl.value = r.sub_url;
    paintPrev();
  };
  const setMode = (mode, why) => {
    box.dataset.mode = mode;
    const hint = box.querySelector('#ca-brandhint');
    if (hint) hint.textContent = mode === 'battery' ? BATTERY_HINT : t('camaddr_form.fill_both_paths','Fills both paths.',{});
    port.placeholder = mode === 'battery' ? t('camaddr_form.usual','usual',{}) : String(DEFAULT_PORT[sch.value] || '');
    box.querySelectorAll('[data-ca-split]').forEach(x => { x.hidden = mode === 'full'; });
    box.querySelectorAll('[data-ca-nobat]').forEach(x => { x.hidden = mode !== 'split'; });
    box.querySelectorAll('[data-ca-bat]').forEach(x => { x.hidden = mode !== 'battery'; });
    q('[data-ca-full]').hidden = mode !== 'full';
    fullChk.checked = mode === 'full';
    msg(mode === 'full' && why ? WHY[why] : '');
    found.innerHTML = '';
  };
  const fill = (f, withCreds) => {
    if (f.mode === 'battery') {
      host.value = f.host; port.value = f.port; uidEl.value = f.uid || '';
      if (withCreds && f.user) { user.value = f.user; pw.value = f.pass; }
      brand.value = 'reolink-battery';
      sync();
      return;
    }
    sch.value = f.scheme; host.value = f.host; port.value = f.port; main.value = f.main; sub.value = f.sub;
    if (withCreds && f.user) { user.value = f.user; pw.value = f.pass; }
    brand.value = brandOf(f.main, f.sub);
    sync();
  };
  const toFull = (why) => { sync(); setMode('full', why); };
  // Full addresses -> split fields, when they fit.
  const toSplit = () => {
    const f = fieldsFromUrls(url.value, subUrl.value, user.value.trim() || c.username, uidEl.value);
    if (f.mode === 'full') { setMode('full', f.why); fullChk.checked = true; return false; }
    setMode(f.mode);
    // (In battery mode the brand note shows BATTERY_HINT.)
    fill(f, true);
    return true;
  };

  // A full address pasted or typed into the address or a path field.
  const takeAddress = (text, which) => {
    const s = splitUrl(text);
    if (!s) return false;
    if (s.scheme === 'baichuan' && !/[?&](?!uid=)[^=&]+=/i.test(s.path.slice(1))) {
      // A battery camera's address: its host, and the UID when the address carries one.
      setMode('battery');
      host.value = s.host; port.value = s.port; brand.value = 'reolink-battery';
      const u = addrUid(s.path);
      if (u) uidEl.value = u;
      if (s.creds) { user.value = s.user; pw.value = s.pass; }
      sync();
      return true;
    }
    if (!DEFAULT_PORT[s.scheme]) {
      if (which === 'sub') { sync(); subUrl.value = text.trim(); sub.value = ''; } else { url.value = text.trim(); main.value = ''; host.value = ''; }
      setMode('full', 'scheme');
      return true;
    }
    const cur = fields();
    if (which === 'sub') {
      const eff = (x) => (x.port === '' ? DEFAULT_PORT[x.scheme] : Number(x.port));
      const samePlace = !cur.host || (s.scheme === cur.scheme && s.host.toLowerCase() === cur.host.replace(/^\[|\]$/g, '').toLowerCase() && eff(s) === eff(cur));
      if (!samePlace) {
        sub.value = '';
        sync();
        subUrl.value = text.trim();
        setMode('full', 'sub-place');
        return true;
      }
      if (!cur.host) { sch.value = s.scheme; host.value = s.host; port.value = s.port || String(DEFAULT_PORT[s.scheme]); }
      sub.value = s.path;
    } else {
      sch.value = s.scheme; host.value = s.host; port.value = s.port || String(DEFAULT_PORT[s.scheme]);
      main.value = s.path;
    }
    if (s.creds) { user.value = s.user; pw.value = s.pass; }
    brand.value = brandOf(main.value, sub.value);
    sync();
    msg(s.creds ? t('camaddr_form.split_pw', 'Split the address into the fields; the password went into Password.') : t('camaddr_form.split', 'Split the address into the fields.'));
    return true;
  };
  let pasted = null;
  [[host, 'host'], [main, 'main'], [sub, 'sub']].forEach(([x, which]) => {
    x.addEventListener('paste', () => { pasted = x; });
    x.addEventListener('input', () => {
      if (pasted === x && hasScheme(x.value)) takeAddress(x.value, which);
      else msg('');
      pasted = null;
      sync();
    });
    x.addEventListener('change', () => {
      if (hasScheme(x.value)) { takeAddress(x.value, which); return; }
      if (which === 'host') {
        // "192.168.1.50:8554" typed in the address: the port goes to Port.
        const m = /^([^\s:[\]/]+):(\d{1,5})$/.exec(x.value.trim()) || /^\[([^\]]+)\]:(\d{1,5})$/.exec(x.value.trim());
        if (m) { host.value = m[1]; port.value = m[2]; }
        else host.value = host.value.trim().replace(/^\[([^\]]*)\]$/, '$1');
      } else if (x.value.trim()) x.value = '/' + x.value.trim().replace(/^\/+/, '');
      if (which !== 'host') brand.value = brandOf(main.value, sub.value);
      sync();
    });
  });
  port.addEventListener('input', sync);
  uidEl.addEventListener('input', () => { sync(); });
  uidEl.addEventListener('change', () => { uidEl.value = normUid(uidEl.value); sync(); });
  sch.addEventListener('change', () => {
    // The usual port follows the protocol; a port typed by hand stays.
    if (port.value.trim() === '' || port.value.trim() === String(DEFAULT_PORT[lastScheme])) port.value = String(DEFAULT_PORT[sch.value]);
    sync();
  });

  // The settings a battery camera needs, set when its type is picked (the person can change them after).
  const setField = (k, v) => {
    const e = d.querySelector('[data-k="' + k + '"]');
    if (!e || e.value === v) return;
    e.value = v;
    e.dispatchEvent(new Event('input', { bubbles: true }));
    e.dispatchEvent(new Event('change', { bubbles: true }));
  };
  const batteryDefaults = () => {
    setField('on_demand', '1');
    setField('idle_timeout', '30');
    setField('record_mode', '1');
  };
  // An existing camera already shows this type, so picking it again changes
  // nothing: this button applies the same settings.
  const batDef = q('[data-ca-batdef]');
  if (batDef) batDef.addEventListener('click', () => { batteryDefaults(); msg(t('camaddr_form.battery_msg','Battery camera is on, with 30 s idle, and recording is motion only.',{})); });
  brand.addEventListener('change', () => {
    const b = BRANDS.find(x => x.id === brand.value);
    if (!b) return;
    if (b.battery) {
      const wasFull = isFull();
      if (wasFull) { url.value = ''; subUrl.value = ''; }
      setMode('battery');
      sch.value = 'rtsp';
      port.value = '';
      batteryDefaults();
      sync();
      msg(BATTERY_PICKED);
      uidEl.focus();
      return;
    }
    if (isBat()) { setMode('split'); port.value = ''; }
    sch.value = 'rtsp';
    port.value = brandPort(port.value, b, sch.value);
    main.value = b.main; sub.value = b.sub;
    sync();
    msg(t('camaddr_form.filled_paths', 'Filled the {name} paths{note}.', { name: b.name.replace(/ \(.*\)$/, ''), note: b.sub ? '' : ' ' + t('camaddr_form.one_stream', '(it has one stream)') }) +
      (b.id.startsWith('reolink') ? ' ' + t('camaddr_form.hub_note', 'On a Home Hub or NVR, camera 2 is Preview_02_main, and so on.') : '') +
      (b.tested ? '' : ' ' + t('camaddr_form.untested_note', 'Not tested by us: check them against the camera\'s manual.')));
  });

  fullChk.addEventListener('change', () => {
    if (fullChk.checked) toFull('');
    else if (!toSplit()) fullChk.focus();
  });

  // Find via ONVIF: ask the camera for its streams (POST /api/discover/inspect). Editing a saved
  // camera with the password left blank, the server's saved sign-in finds the sub stream instead.
  const placeIsStored = () => {
    if (!storedUrl) return false;
    const r = urlsFromFields(Object.assign(fields(), { main: (splitUrl(storedUrl) || {}).path || '' }), storedUrl, '');
    return r.url === storedUrl.trim();
  };
  const applyCam = (cam, what) => {
    const f = fieldsFromUrls(cam.url || '', cam.sub_url || '', user.value.trim());
    if (f.mode === 'full') {
      url.value = cam.url || ''; subUrl.value = cam.sub_url || '';
      setMode('full', f.why);
    } else {
      fill(Object.assign(f, { user: '', pass: '' }), false);
    }
    msg(cam.sub_url ? t('camaddr_form.filled_from', 'Filled from {what}.', { what }) : t('camaddr_form.filled_from_nosub', 'Filled from {what}. It has no sub stream.', { what }));
    changed();
  };
  onvifBtn.addEventListener('click', async () => {
    const f = fields();
    found.innerHTML = '';
    if (isBat()) return;
    if (!f.host) { msg(t('camaddr_form.enter_address', 'Enter the camera address first.')); host.focus(); return; }
    onvifBtn.disabled = true;
    try {
      if (!pw.value && !isNew && c.id != null && placeIsStored()) {
        msg(t('camaddr_form.asking_sub', 'Asking the camera for its sub stream with the saved sign-in...'));
        let r;
        try { r = await api('/api/cameras/' + c.id + '/sub-suggest', { json: {} }); } catch (e) { msg(t('camaddr_form.could_not_ask', 'Could not ask just now: {err}', { err: errText(e) })); return; }
        const ok = (r.suggestions || []).find(x => x.works);
        if (!ok) { msg(t('camaddr_form.no_sub', 'No sub stream found that plays. To ask the camera for both streams, type its password and press Find via ONVIF again.')); return; }
        box.__setSub(ok.url);
        msg(t('camaddr_form.found_sub', 'Found the sub stream {where}. Type the password to look up both streams.', { where: ok.from === 'onvif' ? t('camaddr_form.from_camera', 'from the camera (ONVIF)') : t('camaddr_form.at_usual', 'at the usual address for this kind of camera') }));
        changed();
        return;
      }
      const ovf = d.querySelector('[data-k="onvif_port"]');
      const ports = [ovf && ovf.value.trim(), c.onvif_port, 80, 8000, 2020].map(Number).filter((p, i, a) => p >= 1 && p <= 65535 && a.indexOf(p) === i);
      const h = f.host.includes(':') ? '[' + f.host + ']' : f.host;
      let ins = null, last = '';
      for (const p of ports) {
        msg(t('camaddr_form.asking_onvif', 'Asking the camera over ONVIF (port {port})...', { port: p }));
        try { ins = await api('/api/discover/inspect', { json: { xaddr: 'http://' + h + ':' + p + '/onvif/device_service', username: user.value.trim(), password: pw.value } }); break; }
        catch (e) { last = errText(e); }
      }
      if (!ins) { msg(t('camaddr_form.onvif_no_answer', 'The camera did not answer over ONVIF (ports {ports}){detail}. Check that ONVIF is on in the camera and the user name and password are right, or pick its brand.', { ports: ports.join(', '), detail: last ? ': ' + last : '' })); return; }
      const cams = (ins.cameras && ins.cameras.length ? ins.cameras : [ins.suggested]).filter(x => x && x.url);
      const what = ins.info && ins.info.model ? t('camaddr_form.the_camera_model', 'the camera ({model})', { model: ins.info.model }) : t('camaddr_form.the_camera', 'the camera');
      if (!cams.length) { msg(t('camaddr_form.no_streams', 'The camera answered but gave no stream addresses.')); return; }
      if (cams.length === 1) { applyCam(cams[0], what); return; }
      msg(t('camaddr_form.n_cameras', 'This device has {n, plural, one {# camera} other {# cameras}}. Pick one:', { n: cams.length }));
      found.innerHTML = cams.map((x, i) => {
        const s = splitUrl(x.url);
        return '<div class="ca-cand"><span><b>' + esc(x.name || t('camaddr_form.camera_n', 'Camera {n}', { n: i + 1 })) + '</b> <code>' + esc(s ? s.path : x.url) + '</code></span> <button type="button" class="btn" data-ca-use="' + i + '" data-touch>' + t('camaddr_form.use', 'Use') + '</button></div>';
      }).join('');
      found.querySelectorAll('[data-ca-use]').forEach(b => {
        b.onclick = () => { const x = cams[Number(b.dataset.caUse)]; found.innerHTML = ''; applyCam(x, esc(x.name || what)); };
      });
    } finally { onvifBtn.disabled = false; }
  });

  box.__setSub = (u) => {
    if (isFull()) { subUrl.value = u; return; }
    const s = splitUrl(u);
    if (isBat()) return;
    const fit = fieldsFromUrls(url.value, u, user.value.trim() || c.username);
    // Only the sub stream changes: the main address is left as the fields say (no sync()).
    if (s && fit.mode === 'split') { sub.value = fit.sub; brand.value = brandOf(main.value, sub.value); subUrl.value = u; paintPrev(); }
    else { subUrl.value = u; setMode('full', fit.why); }
  };
  if (!isFull() && !isBat()) paintPrev();
  ADDR_KEYS.forEach(k => { const e = el(k); if (e) e.dataset.init = e.value; });
}

// "N changed" for the address: one per part edited (address, port, protocol, a path), or per
// full address in full-address mode, not the two saved addresses each part rewrites.
const ADDR_KEYS = ['addr_host', 'addr_port', 'addr_scheme', 'addr_main', 'addr_sub', 'url', 'sub_url', 'uid'];
export function addrChanged(d) {
  const box = d.querySelector('[data-addr]');
  if (!box) return 0;
  const keys = box.dataset.mode === 'full' ? ['url', 'sub_url'] : box.dataset.mode === 'battery' ? ['addr_host', 'addr_port', 'uid'] : ['addr_host', 'addr_port', 'addr_scheme', 'addr_main', 'addr_sub'];
  return keys.filter(k => { const e = box.querySelector('[data-k="' + k + '"]'); return e && e.value.trim() !== (e.dataset.init || '').trim(); }).length;
}
// The sub stream was saved on its own: it is no longer an unsaved change.
export function addrRebaseSub(d) {
  const box = d.querySelector('[data-addr]');
  if (!box) return;
  ['addr_sub', 'sub_url'].forEach(k => { const e = box.querySelector('[data-k="' + k + '"]'); if (e) e.dataset.init = e.value; });
}
