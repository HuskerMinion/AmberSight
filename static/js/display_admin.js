// display_admin.js - Settings > Users & security > Displays (admins): pair a wall TV or tablet
// with the code it shows, and see and sign out the displays that are paired.
//   POST /api/display/lookup {code}                 where the waiting display is (admin)
//   POST /api/display/approve {code, user, name}     it signs in as that viewer account (admin)
//   GET  /api/display/sessions                       the paired displays (admin)
//   POST /api/display/sessions/:id/sign-out          sign one out (admin)
//   GET  /api/users                                  the accounts (viewers are offered)
// The display side is display.js (/display); the server side src/display.rs.

import { api, esc, toast, errText, confirmDialog } from './core.js';
import { lead, how, notice } from './sp_kit.js';

const when = (s) => (s ? new Date(s * 1000).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' }) : '');
const ago = (s) => { const m = Math.round(s / 60); return m < 1 ? 'just now' : m === 1 ? '1 min ago' : m + ' min ago'; };
const fmtCode = (v) => { const c = String(v || '').toUpperCase().replace(/[^A-Z0-9]/g, '').slice(0, 8); return c.length > 4 ? c.slice(0, 4) + '-' + c.slice(4) : c; };

export function mountDisplays(el) {
  let gone = false, timer = 0, viewers = [], found = null;
  el.innerHTML = '<p class="empty">Loading...</p>';
  load();

  async function load() {
    let users = [];
    try { users = (await api('/api/users')).users || []; } catch (e) { if (!gone) el.innerHTML = '<p class="empty">Could not load: ' + esc(errText(e)) + '</p>'; return; }
    viewers = users.filter(u => u.role === 'viewer' && !u.disabled);
    if (gone) return;
    draw();
    list();
  }

  function draw() {
    const noViewer = !viewers.length;
    el.innerHTML = '<div class="dsa">' +
      lead('Show cameras full screen on a wall TV, an old tablet or a spare monitor. It signs in with a code it shows, so it needs no keyboard.',
        'a viewer account just for displays, limited to the cameras they should show (Users).') +
      '<div class="sec">Pair a display</div>' +
      '<div class="hint">On the TV or tablet, open <code>' + esc(location.origin) + '/display</code> (bookmark it). It shows a code: enter it here.</div>' +
      (noViewer ? notice('warn', 'A display signs in as a <b>viewer</b> account, never an admin. Make one first in Users (for example <b>display</b>, limited to the cameras it shows).',
        '<a class="btn" href="#settings/users/users">Users</a>') : '') +
      '<div class="row dsa-code"><input data-code placeholder="ABCD-EFGH" maxlength="9" autocomplete="off" autocapitalize="characters" spellcheck="false" aria-label="Display code"' + (noViewer ? ' disabled' : '') + '>' +
      '<button type="button" class="btn" data-look' + (noViewer ? ' disabled' : '') + '>Find display</button></div>' +
      '<div data-found></div>' +
      '<div class="sec">Paired displays</div><div data-list><p class="empty">Loading...</p></div>' +
      how('The code lasts ten minutes and works once, only for the screen that shows it. The display signs in as the viewer account you pick: it sees that account\'s cameras and can only watch (no settings, no account changes, no arming). ' +
        'It stays signed in while it is on, signs out by itself when it has not been used for two weeks, and is paired again every six months. Admins are told when it is used from another network. Treat a display like a signed-in phone: someone who can use its browser could copy its sign-in. A copy cannot keep itself signed in (only the browser it was paired in renews it), but it works while the real display stays on, up to the six months. If a display is lost or stolen, sign it out here; a password change or removing the account signs it out too. ' +
        'Over plain HTTP a TV browser cannot keep its screen on by itself: turn on HTTPS, or set the device\'s screen timeout to Never.') +
      '<p><a class="btn" href="/display">Open display mode on this device</a></p></div>';
    const code = el.querySelector('[data-code]');
    code.oninput = () => { const p = code.selectionStart === code.value.length; code.value = fmtCode(code.value); if (p) code.selectionStart = code.selectionEnd = code.value.length; };
    code.onkeydown = (e) => { if (e.key === 'Enter') { e.preventDefault(); look(); } };
    el.querySelector('[data-look]').onclick = look;
  }

  async function look() {
    const code = el.querySelector('[data-code]').value;
    const box = el.querySelector('[data-found]');
    if (fmtCode(code).length !== 9) { box.innerHTML = '<p class="hint bad" role="alert">The code has eight letters and digits, like ABCD-EFGH.</p>'; return; }
    try { found = await api('/api/display/lookup', { json: { code } }); } catch (e) { found = null; box.innerHTML = '<p class="hint bad" role="alert">' + esc(errText(e)) + '</p>'; return; }
    if (found.approved) { box.innerHTML = '<p class="hint">That display was approved already; it signs in within a few seconds.</p>'; return; }
    box.innerHTML = '<div class="dsa-card">' +
      '<div class="dsa-who"><b>' + esc(found.browser) + '</b> from ' + esc(found.ip) + ', asked ' + ago(found.waiting_s) + '.</div>' +
      '<div class="hint">If you do not know this device, do not pair it.</div>' +
      '<label class="fld"><span class="fld-l">Signs in as</span><select data-user>' + viewers.map(u => '<option value="' + esc(u.username) + '">' + esc(u.username) + (u.cameras && u.cameras.length ? ' (' + u.cameras.length + ' camera' + (u.cameras.length === 1 ? '' : 's') + ')' : ' (all cameras)') + '</option>').join('') + '</select></label>' +
      '<label class="fld"><span class="fld-l">Name</span><input data-name maxlength="40" placeholder="Living room TV"></label>' +
      '<div class="row"><button type="button" class="primary" data-ok>Pair display</button><button type="button" class="btn" data-no>Cancel</button></div></div>';
    box.querySelector('[data-name]').focus();
    box.querySelector('[data-no]').onclick = () => { box.innerHTML = ''; found = null; };
    box.querySelector('[data-ok]').onclick = async (ev) => {
      ev.target.disabled = true;
      try {
        const r = await api('/api/display/approve', { json: { code: found.code, user: box.querySelector('[data-user]').value, name: box.querySelector('[data-name]').value } });
        toast('Paired: ' + r.name + ' signs in as ' + r.user + ' in a few seconds', { kind: 'ok' });
        box.innerHTML = ''; found = null;
        el.querySelector('[data-code]').value = '';
        setTimeout(list, 4000);
      } catch (e) { ev.target.disabled = false; if (e.status !== 403 || e.body !== 'Canceled') toast(errText(e), { kind: 'err' }); }
    };
  }

  async function list() {
    clearTimeout(timer);
    if (gone || !el.isConnected) return;
    const box = el.querySelector('[data-list]');
    let d;
    try { d = await api('/api/display/sessions'); } catch (e) { if (box) box.innerHTML = '<p class="hint bad">' + esc(errText(e)) + '</p>'; return; }
    if (gone || !box) return;
    const rows = d.displays || [];
    box.innerHTML = rows.length ? '<div class="asys-scroll"><table class="asys-tbl"><thead><tr><th>Display</th><th>Account</th><th>Last used</th><th>Signs out if unused until</th><th>Pair again by</th><th></th></tr></thead><tbody>' +
      rows.map(r => '<tr><td>' + esc(r.label) + '<div class="hint">' + esc(r.ip) + '</div></td><td>' + esc(r.user) + '</td><td>' + esc(when(r.last_seen)) + '</td><td>' + esc(when(r.expires)) + '</td><td>' + esc(when(r.pair_by)) + '</td>' +
        '<td><button type="button" class="btn sm" data-out="' + esc(r.id) + '" data-label="' + esc(r.label) + '">Sign out</button></td></tr>').join('') + '</tbody></table></div>'
      : '<p class="hint">No displays are paired yet.</p>';
    for (const b of box.querySelectorAll('[data-out]')) b.onclick = async () => {
      if (!await confirmDialog({ title: 'Sign out this display?', body: b.dataset.label + ' stops showing cameras and shows a new code.', ok: 'Sign out', danger: true })) return;
      try { await api('/api/display/sessions/' + encodeURIComponent(b.dataset.out) + '/sign-out', { method: 'POST' }); toast('Signed out', { kind: 'ok' }); } catch (e) { toast(errText(e), { kind: 'err' }); }
      list();
    };
    timer = setTimeout(list, 30000);
  }

  return () => { gone = true; clearTimeout(timer); };
}
