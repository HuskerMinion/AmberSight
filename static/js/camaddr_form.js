// camaddr_form.js - the address part of a network camera's Stream settings: protocol, address
// and port, user name and password, and the main and sub stream paths, with brand presets,
// "Find via ONVIF" and a switch to the two full addresses. The hidden-or-shown full address
// inputs (data-k url / sub_url) always hold what is saved, so the rest of the form reads them
// as before. Split and join live in camaddr.js.

import { api, esc } from './core.js';
import { PROTOCOLS, DEFAULT_PORT, BRANDS, splitUrl, hasScheme, fieldsFromUrls, urlsFromFields, addrProblems, brandOf, brandPort } from './camaddr.js';

const GUIDE = 'https://github.com/HuskerMinion/AmberSight/blob/master/docs/cameras.md';
const WHY = {
  scheme: 'This kind of address is edited as a full address.',
  bad: 'This address has no protocol (like rtsp://), so it is shown as it is.',
  'query-login': 'This address carries its sign-in in it, so it is shown as a full address.',
  'sub-place': 'The sub stream is on another address, port or protocol, so both are shown as full addresses.',
  'sub-login': 'The sub stream signs in with its own user, so both are shown as full addresses.',
};
const errText = (e) => { const b = (e && e.body) || ''; try { const j = JSON.parse(b); if (j && j.error) return j.error; } catch (x) { /* text */ } return b || (e && e.message) || String(e); };

const cell = (cls, label, input) => '<div class="ca-cell ' + cls + '"><label class="cs-f ca-f"><span>' + label + '</span>' + input + '</label></div>';
const inp = (k, val, extra) => '<input data-k="' + k + '" value="' + esc(val == null ? '' : val) + '" ' + (extra || '') + '>';

function brandOptions(cur) {
  const opt = (b) => '<option value="' + b.id + '"' + (b.id === cur ? ' selected' : '') + '>' + esc(b.name) + '</option>';
  return '<option value="">Pick a brand...</option>' +
    '<optgroup label="Tested">' + BRANDS.filter(b => b.tested).map(opt).join('') + '</optgroup>' +
    '<optgroup label="Not tested">' + BRANDS.filter(b => !b.tested).map(opt).join('') + '</optgroup>';
}

// The address fields for camera c. `net`: the camera is a network camera now (its url belongs here).
export function addrHtml(c, isNew, net) {
  const url = net ? (c.url || '') : '';
  const f = fieldsFromUrls(url, net ? c.sub_url : '', c.username);
  const full = f.mode === 'full';
  const ns = 'data-nosig spellcheck="false" autocomplete="off" autocapitalize="off"';
  return '<div class="ca" data-addr data-mode="' + f.mode + '">' +
    '<div class="ca-r1" data-ca-split' + (full ? ' hidden' : '') + '>' +
      cell('ca-host', 'Camera address', inp('addr_host', f.host, ns + ' inputmode="url" placeholder="192.168.1.50"')) +
      cell('ca-port', 'Port', inp('addr_port', f.port, 'data-nosig inputmode="numeric" maxlength="5" autocomplete="off" placeholder="' + (DEFAULT_PORT[f.scheme] || 554) + '"')) +
      cell('ca-proto', 'Protocol', '<select data-k="addr_scheme" data-nosig>' + PROTOCOLS.map(([v, t]) => '<option value="' + v + '"' + (v === f.scheme ? ' selected' : '') + '>' + t + '</option>').join('') + '</select>') +
    '</div>' +
    '<div class="ca-full" data-ca-full' + (full ? '' : ' hidden') + '>' +
      cell('ca-wide', 'Main stream address <em>records, full size</em>', inp('url', url, 'placeholder="rtsp://192.168.1.50:554/stream1" spellcheck="false" autocomplete="off"')) +
      cell('ca-wide', 'Sub stream address <em>motion and the Live grid (optional)</em>', inp('sub_url', net ? c.sub_url : '', 'placeholder="lower resolution, for the grid and motion" spellcheck="false" autocomplete="off"')) +
    '</div>' +
    '<div class="ca-r2">' +
      cell('ca-user', 'User name', inp('username', c.username, 'autocomplete="off" autocapitalize="off" spellcheck="false"')) +
      cell('ca-pass', 'Password', '<input data-k="password" type="password" autocomplete="new-password" placeholder="' + (isNew ? 'camera password' : 'blank keeps the saved one') + '">') +
    '</div>' +
    '<div class="ca-paths" data-ca-split' + (full ? ' hidden' : '') + '>' +
      cell('ca-wide', 'Main stream <em>records, full size</em>', inp('addr_main', f.main, ns + ' placeholder="/stream1"')) +
      cell('ca-wide', 'Sub stream <em>motion and the Live grid (optional)</em>', inp('addr_sub', f.sub, ns + ' placeholder="/stream2"')) +
      '<div class="ca-prev" data-ca-prev aria-live="off"></div>' +
    '</div>' +
    '<div class="ca-tools" data-ca-split' + (full ? ' hidden' : '') + '>' +
      '<label class="cs-f ca-f ca-brand"><span>Brand</span><select data-ca-brand data-nosig aria-describedby="ca-brandhint">' + brandOptions(brandOf(f.main, f.sub)) + '</select></label>' +
      '<div class="ca-brandnote"><span id="ca-brandhint">Fills both paths.</span> <a href="' + GUIDE + '" target="_blank" rel="noopener noreferrer">Camera guide</a></div>' +
    '</div>' +
    '<div class="ca-acts">' +
      '<button type="button" class="btn" data-ca-onvif data-ca-split data-touch' + (full ? ' hidden' : '') + '>Find via ONVIF</button>' +
      '<label class="asys-chk ca-fullchk"><input type="checkbox" data-ca-fullchk data-nosig' + (full ? ' checked' : '') + '> Use full addresses</label>' +
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
  const f = full ? { url: v('url'), sub_url: v('sub_url') } : { host: v('addr_host'), port: v('addr_port'), main: v('addr_main'), sub: v('addr_sub') };
  const key = { host: 'addr_host', port: 'addr_port', main: 'addr_main', sub: 'addr_sub', url: 'url', sub_url: 'sub_url' };
  return addrProblems(f, full ? 'full' : 'split').map(([k, m]) => [key[k], m]);
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
  const net = !/^(push|device|screen):\/\//i.test(c.url || '') && !c.capture;
  const storedUrl = net ? (c.url || '') : '', storedSub = net ? (c.sub_url || '') : '';
  const msg = (t) => { msgEl.textContent = t || ''; };
  const isFull = () => box.dataset.mode === 'full';
  const fields = () => ({ scheme: sch.value, host: host.value.trim(), port: port.value.trim(), main: main.value, sub: sub.value });
  // Something changed without a keystroke (ONVIF, a sub stream from elsewhere): the form repaints.
  const changed = () => box.dispatchEvent(new Event('input', { bubbles: true }));

  const paintPrev = () => {
    const r = urlsFromFields(fields(), storedUrl, storedSub);
    const strip = (u) => { const s = splitUrl(u); return s && s.creds ? u.replace(/\/\/[^/]*@/, '//') : u; };
    prev.innerHTML = r.url ? '<span>Saves as:</span><span class="ca-u">' + esc(strip(r.url)) + '</span>' + (r.sub_url ? '<span class="ca-u">' + esc(strip(r.sub_url)) + '</span>' : '') : '';
  };
  // Split fields -> the full addresses that are saved.
  let lastScheme = sch.value;
  const sync = () => {
    lastScheme = sch.value;
    port.placeholder = String(DEFAULT_PORT[sch.value] || '');
    if (isFull()) return;
    const r = urlsFromFields(fields(), storedUrl, storedSub);
    url.value = r.url; subUrl.value = r.sub_url;
    paintPrev();
  };
  const setMode = (mode, why) => {
    box.dataset.mode = mode;
    box.querySelectorAll('[data-ca-split]').forEach(x => { x.hidden = mode === 'full'; });
    q('[data-ca-full]').hidden = mode !== 'full';
    fullChk.checked = mode === 'full';
    msg(mode === 'full' && why ? WHY[why] : '');
    found.innerHTML = '';
  };
  const fill = (f, withCreds) => {
    sch.value = f.scheme; host.value = f.host; port.value = f.port; main.value = f.main; sub.value = f.sub;
    if (withCreds && f.user) { user.value = f.user; pw.value = f.pass; }
    brand.value = brandOf(f.main, f.sub);
    sync();
  };
  const toFull = (why) => { sync(); setMode('full', why); };
  // Full addresses -> split fields, when they fit.
  const toSplit = () => {
    const f = fieldsFromUrls(url.value, subUrl.value, user.value.trim() || c.username);
    if (f.mode === 'full') { setMode('full', f.why); fullChk.checked = true; return false; }
    setMode('split');
    fill(f, true);
    return true;
  };

  // A full address pasted or typed into the address or a path field.
  const takeAddress = (text, which) => {
    const s = splitUrl(text);
    if (!s) return false;
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
    msg('Split the address into the fields' + (s.creds ? '; the password went into Password.' : '.'));
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
  sch.addEventListener('change', () => {
    // The usual port follows the protocol; a port typed by hand stays.
    if (port.value.trim() === '' || port.value.trim() === String(DEFAULT_PORT[lastScheme])) port.value = String(DEFAULT_PORT[sch.value]);
    sync();
  });

  brand.addEventListener('change', () => {
    const b = BRANDS.find(x => x.id === brand.value);
    if (!b) return;
    sch.value = 'rtsp';
    port.value = brandPort(port.value, b, sch.value);
    main.value = b.main; sub.value = b.sub;
    sync();
    msg('Filled the ' + b.name.replace(/ \(.*\)$/, '') + ' paths' + (b.sub ? '' : ' (it has one stream)') + '.' +
      (b.id.startsWith('reolink') ? ' On a Home Hub or NVR, camera 2 is Preview_02_main, and so on.' : '') +
      (b.tested ? '' : ' Not tested by us: check them against the camera\'s manual.'));
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
    msg('Filled from ' + what + (cam.sub_url ? '' : '. It has no sub stream') + '.');
    changed();
  };
  onvifBtn.addEventListener('click', async () => {
    const f = fields();
    found.innerHTML = '';
    if (!f.host) { msg('Enter the camera address first.'); host.focus(); return; }
    onvifBtn.disabled = true;
    try {
      if (!pw.value && !isNew && c.id != null && placeIsStored()) {
        msg('Asking the camera for its sub stream with the saved sign-in...');
        let r;
        try { r = await api('/api/cameras/' + c.id + '/sub-suggest', { json: {} }); } catch (e) { msg('Could not ask just now: ' + errText(e)); return; }
        const ok = (r.suggestions || []).find(x => x.works);
        if (!ok) { msg('No sub stream found that plays. To ask the camera for both streams, type its password and press Find via ONVIF again.'); return; }
        box.__setSub(ok.url);
        msg('Found the sub stream ' + (ok.from === 'onvif' ? 'from the camera (ONVIF)' : 'at the usual address for this kind of camera') + '. Type the password to look up both streams.');
        changed();
        return;
      }
      const ovf = d.querySelector('[data-k="onvif_port"]');
      const ports = [ovf && ovf.value.trim(), c.onvif_port, 80, 8000, 2020].map(Number).filter((p, i, a) => p >= 1 && p <= 65535 && a.indexOf(p) === i);
      const h = f.host.includes(':') ? '[' + f.host + ']' : f.host;
      let ins = null, last = '';
      for (const p of ports) {
        msg('Asking the camera over ONVIF (port ' + p + ')...');
        try { ins = await api('/api/discover/inspect', { json: { xaddr: 'http://' + h + ':' + p + '/onvif/device_service', username: user.value.trim(), password: pw.value } }); break; }
        catch (e) { last = errText(e); }
      }
      if (!ins) { msg('The camera did not answer over ONVIF (ports ' + ports.join(', ') + ')' + (last ? ': ' + last : '') + '. Check that ONVIF is on in the camera and the user name and password are right, or pick its brand.'); return; }
      const cams = (ins.cameras && ins.cameras.length ? ins.cameras : [ins.suggested]).filter(x => x && x.url);
      const what = ins.info && ins.info.model ? 'the camera (' + ins.info.model + ')' : 'the camera';
      if (!cams.length) { msg('The camera answered but gave no stream addresses.'); return; }
      if (cams.length === 1) { applyCam(cams[0], what); return; }
      msg('This device has ' + cams.length + ' cameras. Pick one:');
      found.innerHTML = cams.map((x, i) => {
        const s = splitUrl(x.url);
        return '<div class="ca-cand"><span><b>' + esc(x.name || 'Camera ' + (i + 1)) + '</b> <code>' + esc(s ? s.path : x.url) + '</code></span> <button type="button" class="btn" data-ca-use="' + i + '" data-touch>Use</button></div>';
      }).join('');
      found.querySelectorAll('[data-ca-use]').forEach(b => {
        b.onclick = () => { const x = cams[Number(b.dataset.caUse)]; found.innerHTML = ''; applyCam(x, esc(x.name || what)); };
      });
    } finally { onvifBtn.disabled = false; }
  });

  box.__setSub = (u) => {
    if (isFull()) { subUrl.value = u; return; }
    const s = splitUrl(u);
    const fit = fieldsFromUrls(url.value, u, user.value.trim() || c.username);
    // Only the sub stream changes: the main address is left as the fields say (no sync()).
    if (s && fit.mode === 'split') { sub.value = fit.sub; brand.value = brandOf(main.value, sub.value); subUrl.value = u; paintPrev(); }
    else { subUrl.value = u; setMode('full', fit.why); }
  };
  if (!isFull()) paintPrev();
  ADDR_KEYS.forEach(k => { const e = el(k); if (e) e.dataset.init = e.value; });
}

// "N changed" for the address: one per part edited (address, port, protocol, a path), or per
// full address in full-address mode, not the two saved addresses each part rewrites.
const ADDR_KEYS = ['addr_host', 'addr_port', 'addr_scheme', 'addr_main', 'addr_sub', 'url', 'sub_url'];
export function addrChanged(d) {
  const box = d.querySelector('[data-addr]');
  if (!box) return 0;
  const keys = box.dataset.mode === 'full' ? ['url', 'sub_url'] : ['addr_host', 'addr_port', 'addr_scheme', 'addr_main', 'addr_sub'];
  return keys.filter(k => { const e = box.querySelector('[data-k="' + k + '"]'); return e && e.value.trim() !== (e.dataset.init || '').trim(); }).length;
}
// The sub stream was saved on its own: it is no longer an unsaved change.
export function addrRebaseSub(d) {
  const box = d.querySelector('[data-addr]');
  if (!box) return;
  ['addr_sub', 'sub_url'].forEach(k => { const e = box.querySelector('[data-k="' + k + '"]'); if (e) e.dataset.init = e.value; });
}
