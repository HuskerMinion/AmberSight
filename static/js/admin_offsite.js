// admin_offsite.js - Settings > System > Backup: copy the settings, the database
// and protected clips to a network folder / USB drive or to S3-style cloud
// storage on a schedule, encrypted with a passphrase. Secrets (S3 secret key,
// passphrase) never come back from the server: "<set>" means one is saved.

import { api, esc, toast, fmtSize, state, pageBar, wireBar, confirmDialog, liveSwitch, swHtml, fld, savedToast, errText as coreErr } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, how, more } from './sp_kit.js';

const SET = '<set>';
const clone = (o) => JSON.parse(JSON.stringify(o));
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const errText = (e) => (e && e.body) ? (() => { try { return JSON.parse(e.body).error || e.body; } catch (x) { return e.body; } })() : (e && e.message) || String(e);
const when = (t) => (t ? new Date(t * 1000).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' }) : 'never');

function ago(t) {
  const s = Math.max(0, Date.now() / 1000 - t);
  if (s < 90) return 'just now';
  if (s < 5400) return Math.round(s / 60) + ' min ago';
  if (s < 129600) return Math.round(s / 3600) + ' h ago';
  return Math.round(s / 86400) + ' days ago';
}

function offsitePage(el) {
  if (!state.isAdmin) { el.innerHTML = '<p class="empty">Admin sign-in required.</p>'; return () => {}; }
  let saved = null, draft = null, st = null, timer = 0, pass = '', pass2 = '', busy = false;

  async function load(full) {
    let r;
    try { r = await api('/api/offsite'); } catch (e) { el.innerHTML = '<div class="ac"><p class="empty">' + esc(errText(e)) + '</p></div>'; return; }
    st = r.status;
    if (full || !saved) { saved = r.settings; draft = clone(saved); pass = pass2 = ''; paint(); } else paintStatus();
    clearTimeout(timer);
    if (st.running) timer = setTimeout(() => { if (el.isConnected) load(false); }, 2000);
  }

  const dirty = () => !same(draft, saved) || pass !== '' || pass2 !== '';
  const q = (s) => el.querySelector(s);

  function field(label, key, attrs) {
    return '<label>' + label + ' <input data-k="' + key + '" value="' + esc(draft[key] || '') + '"' + (attrs || '') + '></label>';
  }

  function paint() {
    const s3 = draft.kind === 's3';
    const hasPass = saved.passphrase === SET;
    el.innerHTML = '<div class="ac">' +
      lead('Copies your settings, events, faces, users and the clips you keep to somewhere else, so a broken or stolen computer does not lose them.', 'Back up to a USB drive or cloud storage, and set a passphrase.') +
      how('Copies go out on a schedule and only what is new is sent. On a new computer with AmberSight installed, run <code>ambersight offsite-restore --from &lt;folder or s3://bucket/folder&gt; --to &lt;empty folder&gt;</code> (add <code>--list</code> to see the copies). It asks for the passphrase. Then put config.toml next to the program and the rest into the data folder.') +

      '<div class="ac-card"><div class="ac-card-t">Where the copy goes</div>' +
      swHtml('data-en', saved.enabled, 'Make off-site copies', 'Every ' + saved.every_hours + ' hours' + (saved.enabled ? '.' : ', starting at once when you turn this on.')) +
      '<div class="seg ac-seg" role="group" aria-label="Destination"><button data-kind="folder" class="' + (s3 ? '' : 'on') + '">Another folder or USB drive</button><button data-kind="s3" class="' + (s3 ? 'on' : '') + '">Cloud storage (S3-compatible)</button></div>' +
      (s3
        ? '<div class="hint">Works with Amazon S3, Backblaze B2, Wasabi, Cloudflare R2, MinIO and others that speak S3. Make a private bucket and a key that can read, write, list and delete in it.</div>' +
          '<div class="ac-grid3">' +
          field('Address', 's3_endpoint', ' placeholder="https://s3.us-west-004.backblazeb2.com"') +
          field('Region', 's3_region', ' placeholder="us-east-1"') +
          field('Bucket', 's3_bucket', ' placeholder="my-nvr-backup"') +
          field('Folder in the bucket', 's3_prefix', ' placeholder="ambersight (optional)"') +
          field('Access key', 's3_access_key', ' autocomplete="off" spellcheck="false"') +
          '<label>Secret key <input type="password" data-k="s3_secret_key" autocomplete="new-password" value="" placeholder="' + (saved.s3_secret_key === SET ? 'saved - type to replace' : '') + '"></label>' +
          '</div>' +
          more('More settings', swHtml('data-ps', saved.s3_path_style, 'Bucket name in the path', 'Turn on for MinIO and other servers on your own network. Leave off for AWS, B2, Wasabi, R2.'))
        : '<label class="ac-f">Folder <input data-k="folder" value="' + esc(draft.folder || '') + '" placeholder="\\\\nas\\backup  or  E:\\AmberSight backup  or  /mnt/backup" spellcheck="false"></label>' +
          '<div class="hint">A network share, a USB drive or any folder. On Windows, AmberSight runs as a service: give its account (or "Everyone") write access to the share, and use the \\\\server\\share form rather than a mapped drive letter.</div>') +
      '</div>' +

      '<div class="ac-card"><div class="ac-card-t">What and when</div>' +
      '<div class="ac-grid3">' + fld('Copy every', 'data-n="every_hours" min="1" max="720" value="' + draft.every_hours + '"', 'hours') +
      fld('Copies to keep', 'data-n="keep" min="1" max="365" value="' + draft.keep + '"', 'copies') + '</div>' +
      '<div class="hint">Settings and the database are always copied. Older copies beyond the number kept are removed.</div>' +
      swHtml('data-b="clips"', saved.clips, 'Clips of events you keep', 'Each clip is sent once. Keep an event forever (lock icon) to include it.') +
      swHtml('data-b="pictures"', saved.pictures, 'Pictures of events you keep', 'The event picture and the frames AI checked.') +
      '</div>' +

      '<div class="ac-card"><div class="ac-card-t">Encryption</div>' +
      '<div class="hint' + (hasPass ? '' : ' warn') + '">' + (hasPass ? 'A passphrase is set: copies are encrypted before they leave this computer.' : (s3 ? 'Cloud copies must be encrypted: choose a passphrase.' : '<b>No passphrase:</b> copies in the folder can be read by anyone with access to it, including camera passwords in the settings.')) + '</div>' +
      '<div class="ac-grid3"><label>' + (hasPass ? 'New passphrase' : 'Passphrase') + ' <input type="password" data-pass autocomplete="new-password" placeholder="at least 8 characters" value="' + esc(pass) + '"></label>' +
      '<label>Type it again <input type="password" data-pass2 autocomplete="new-password" value="' + esc(pass2) + '"></label></div>' +
      '<div class="hint warn">Write it down somewhere safe (not only on this computer). Without it the copies cannot be opened. You are asked to confirm this when you save.</div>' +
      (hasPass && !s3 ? '<div class="ac-row"><button class="btn danger" data-nopass>Stop encrypting</button></div>' : '') +
      '</div>' +

      '<div class="ac-card" data-stcard></div>' +

      pageBar() + '</div>';
    bind();
    paintStatus();
  }

  function paintStatus() {
    const box = q('[data-stcard]');
    if (!box || !st) return;
    let line;
    if (st.running) line = '<div class="hint"><b>Copying now:</b> ' + esc(st.phase || 'working') + '...</div>';
    else if (st.last_ok === true) line = '<div class="hint">Last copy ' + esc(ago(st.last_end)) + ' (' + esc(when(st.last_end)) + '): sent ' + fmtSize(st.last_bytes) + ' in ' + st.last_sent + ' file' + (st.last_sent === 1 ? '' : 's') + (st.last_skipped ? ', ' + st.last_skipped + ' already there' : '') + '.</div>';
    else if (st.last_ok === false) line = '<div class="hint warn">Last try ' + esc(ago(st.last_end)) + ' failed: ' + esc(st.last_error) + (st.failures > 1 ? ' (' + st.failures + ' times in a row)' : '') + '</div>' +
      (st.last_success ? '<div class="hint">Last good copy: ' + esc(when(st.last_success)) + '</div>' : '');
    else line = '<div class="hint">No copy made yet.</div>';
    const next = st.next_run ? '<div class="hint">Next copy: ' + esc(when(st.next_run)) + '</div>' : (saved.enabled ? '' : '<div class="hint">Scheduled copies are off.</div>');
    box.innerHTML = '<div class="ac-card-t">Status</div>' + line + next +
      '<div class="ac-row"><button class="btn" data-test' + (busy ? ' disabled' : '') + '>Test connection</button><button class="btn" data-run' + (st.running || busy ? ' disabled' : '') + '>Back up now</button></div>' +
      '<div class="hint" data-testres></div>';
    box.querySelector('[data-test]').onclick = test;
    box.querySelector('[data-run]').onclick = runNow;
  }

  let updBar = () => {};

  function bind() {
    updBar = wireBar(el, dirty, () => save(), () => { draft = clone(saved); pass = pass2 = ''; paint(); });
    // On/off settings save when flipped: the saved settings with only that value changed.
    const flip = (i, key) => { if (i) liveSwitch(i, async (on) => {
      // The switch saves the SAVED settings with only this value flipped, so unsaved edits must be saved first.
      if (key === 'enabled' && dirty()) throw new Error('Save your changes first, then turn this on or off.');
      if (key === 'enabled' && on && saved.kind === 'folder' && saved.passphrase !== SET &&
        !(await confirmDialog({ title: 'Copy without a passphrase?', body: 'The copies will not be encrypted. Anyone who can open the folder can read them, including camera passwords. Add a passphrase below first to encrypt them.', ok: 'Copy anyway', danger: true }))) throw Object.assign(new Error('cancelled'), { cancelled: true });
      const b = clone(saved); b.passphrase = saved.passphrase; b[key] = on;
      const r = await api('/api/offsite', { method: 'PUT', json: b });
      if (r.error) throw new Error(r.error);
      saved = r.settings; st = r.status; draft[key] = on; updBar(); paintStatus();
    }); };
    flip(q('[data-en]'), 'enabled'); flip(q('[data-ps]'), 's3_path_style');
    el.querySelectorAll('[data-b]').forEach(i => flip(i, i.dataset.b));
    el.querySelectorAll('[data-kind]').forEach(b => { b.onclick = () => { if (draft.kind !== b.dataset.kind) { draft.kind = b.dataset.kind; paint(); } }; });
    el.querySelectorAll('[data-k]').forEach(i => { i.oninput = () => { draft[i.dataset.k] = i.value; updBar(); }; });
    el.querySelectorAll('[data-n]').forEach(i => { i.oninput = () => { draft[i.dataset.n] = Math.max(1, parseInt(i.value, 10) || 1); updBar(); }; });
    q('[data-pass]').oninput = (e) => { pass = e.target.value; updBar(); };
    q('[data-pass2]').oninput = (e) => { pass2 = e.target.value; updBar(); };
    const np = q('[data-nopass]');
    if (np) np.onclick = async () => {
      if (!(await confirmDialog({ title: 'Stop encrypting?', body: 'New copies in the folder will be readable by anyone who can open it, including camera passwords. Copies already made stay encrypted.', ok: 'Stop encrypting', danger: true }))) return;
      try { if (await save({ passphrase: null }) !== false) savedToast(); } catch (e) { toast(coreErr(e), { kind: 'err' }); }
    };
  }

  // What to send: the draft, with the passphrase only when a new one was typed.
  function body(extra) {
    const b = clone(draft);
    if (b.s3_secret_key === '' && saved.s3_secret_key === SET) b.s3_secret_key = SET;
    b.passphrase = pass ? pass : SET;
    return Object.assign(b, extra || {});
  }

  function checkPass() {
    if (pass || pass2) {
      if (pass !== pass2) throw new Error('The two passphrases are different.');
      if (pass.length < 8) throw new Error('The passphrase needs at least 8 characters.');
    }
  }

  // Returns false when the person cancels. Errors go to the bar (or the caller).
  async function save(extra) {
    if (!extra) checkPass();
    // Copies to a plain folder without a passphrase: say plainly what that means.
    const plainNow = draft.enabled && draft.kind === 'folder' && saved.passphrase !== SET && !pass;
    const plainBefore = saved.enabled && saved.kind === 'folder' && saved.passphrase !== SET;
    if (!extra && plainNow && !plainBefore && !(await confirmDialog({ title: 'Save without a passphrase?', body: 'The copies will not be encrypted. Anyone who can open the folder can read them, including camera passwords.', ok: 'Save anyway', danger: true }))) return false;
    // A new or changed passphrase: it cannot be recovered, so ask for an acknowledgement first.
    if (!extra && pass && !(await confirmDialog({ title: 'Save this passphrase?', body: 'I have saved this passphrase somewhere safe. Without it the backup cannot be read.', ok: 'I have saved it', cancel: 'Go back' }))) return false;
    const r = await api('/api/offsite', { method: 'PUT', json: body(extra) });
    if (r.error) throw new Error(r.error);
    saved = r.settings; draft = clone(saved); st = r.status; pass = pass2 = '';
    paint();
  }

  async function test() {
    const out = q('[data-testres]');
    try { checkPass(); } catch (e) { out.className = 'hint warn'; out.textContent = e.message; return; }
    busy = true; paintStatus();
    const o = q('[data-testres]');
    o.className = 'hint'; o.textContent = 'Testing...';
    let msg, ok = false;
    try { const r = await api('/api/offsite/test', { json: body() }); ok = r.ok; msg = r.message; } catch (e) { msg = errText(e); }
    busy = false; paintStatus();
    const o2 = q('[data-testres]');
    o2.className = ok ? 'hint' : 'hint warn';
    o2.textContent = msg;
  }

  async function runNow() {
    if (dirty()) { toast('Save your changes first'); return; }
    try {
      const r = await api('/api/offsite/run', { json: {} });
      if (r.error) throw new Error(r.error);
      toast('Off-site copy started');
      st.running = true; st.phase = 'Starting';
      paintStatus();
      clearTimeout(timer);
      timer = setTimeout(() => { if (el.isConnected) load(false); }, 1500);
    } catch (e) { toast(errText(e)); }
  }

  el.innerHTML = '<div class="ac"><p class="empty">Loading...</p></div>';
  load(true);
  return () => clearTimeout(timer);
}

let stop = null;
registerAdminPage({ id: 'offsite', title: 'Off-site backup', order: 72, render(el) { stop = offsitePage(el); }, leave() { if (stop) stop(); } });
