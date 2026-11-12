// admin_faces.js - Settings > Smart features > Faces: turn face recognition on, name the faces
// AmberSight has seen, and manage people (rename, no alerts, forget).
// Also the "Who is this?" dialog used from the event window.

import { api, esc, toast, openDlg, state, errText, liveSwitch, swHtml, fld, formBar, confirmDialog, promptDialog, hasUnsaved } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, how, more, notice } from './sp_kit.js';

const img = (id, cls) => '<img class="' + (cls || 'fc-img') + '" loading="lazy" src="/api/faces/' + id + '/img" alt="">';
const when = (ms) => new Date(ms).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });

// "Who is this?" for one face. done(name|null) after a change.
export async function nameFaceDialog(face, done) {
  let people = [];
  try { people = (await api('/api/people')).people; } catch (e) { /* new names still work */ }
  const d = openDlg('Who is this?',
    '<div class="fc-who">' + img(face.id, 'fc-big') + '<div>' +
      '<label class="cs-f"><span>Name</span><input data-n list="fcPeople" value="' + esc(face.person || '') + '" placeholder="e.g. Alex" maxlength="60" autocomplete="off"></label>' +
      '<datalist id="fcPeople">' + people.map(p => '<option value="' + esc(p.name) + '">').join('') + '</datalist>' +
      (people.length ? '<div class="chips fc-pick">' + people.map(p => '<button class="chip" data-p="' + esc(p.name) + '">' + esc(p.name) + '</button>').join('') + '</div>' : '') +
      '<label class="asys-chk"><input type="checkbox" data-sim checked> Also name faces that look like this one</label>' +
      '<div class="hint">Naming a few clear faces of each person (front-on, in daylight and at night) makes recognition much more reliable.' + (face.clear === false ? ' This face is small or blurry: it can be named, but it is not used to recognize anyone.' : '') + '</div>' +
    '</div></div>' +
    '<div class="row"><button class="btn primary" data-ok>Save</button>' + (face.person ? '<button class="btn" data-clear>Not ' + esc(face.person) + '</button>' : '') +
    '<button class="btn danger" data-del title="Not a face, or not worth keeping">Delete</button><button class="btn" data-cancel>Cancel</button></div>', true);
  const q = (s) => d.querySelector(s);
  q('[data-n]').focus();
  d.querySelectorAll('[data-p]').forEach(b => { b.onclick = () => { q('[data-n]').value = b.dataset.p; }; });
  const save = async (name, similar) => {
    try {
      const r = await api('/api/faces/' + face.id + '/name', { json: { name, similar } });
      toast(name ? 'Named ' + name + (r.named > 1 ? ' (and ' + (r.named - 1) + ' look-alike' + (r.named > 2 ? 's' : '') + ')' : '') : 'No longer named');
      d.__close();
      if (done) done(name);
    } catch (e) { toast(errText(e)); }
  };
  q('[data-ok]').onclick = () => { const n = q('[data-n]').value.trim(); if (!n) { toast('Type a name'); return; } save(n, q('[data-sim]').checked); };
  q('[data-n]').onkeydown = (e) => { if (e.key === 'Enter') q('[data-ok]').click(); };
  const cl = q('[data-clear]'); if (cl) cl.onclick = () => save(null, false);
  q('[data-del]').onclick = async () => {
    try { await api('/api/faces/' + face.id, { method: 'DELETE' }); toast('Face deleted'); d.__close(); if (done) done(null); } catch (e) { toast(errText(e)); }
  };
  q('[data-cancel]').onclick = () => d.__close();
}

// Faces of one event, for the event window (admins).
export async function eventFacesHtml(eventId) {
  if (!state.isAdmin) return '';
  let faces = [];
  try { faces = (await api('/api/faces?event=' + encodeURIComponent(eventId))).faces; } catch (e) { return ''; }
  if (!faces.length) return '';
  return '<div class="sec">Faces</div><div class="fc-grid fc-ev">' + faces.map(f =>
    '<button class="fc-card" data-face="' + f.id + '">' + img(f.id) + '<span>' + (f.person ? esc(f.person) + (f.labeled ? '' : ' ?') : 'Who is this?') + '</span></button>').join('') + '</div>';
}
export function wireEventFaces(root, eventId, refresh) {
  root.querySelectorAll('[data-face]').forEach(b => { b.onclick = async () => {
    const faces = (await api('/api/faces?event=' + encodeURIComponent(eventId))).faces;
    const f = faces.find(x => String(x.id) === b.dataset.face);
    if (f) nameFaceDialog(f, refresh);
  }; });
}

function facesPage(el) {
  let st = null, people = [], unnamed = [], person = null, personFaces = [], timer = 0;
  // Reload after the on/off switch, keeping what is typed in the setup fields.
  const keyOf = (e) => ((([...e.attributes].find(a => a.name.startsWith('data-')) || {}).name) || '') + (e.dataset.c || '');
  const setupFields = () => new Map([...el.querySelectorAll('[data-setup] input:not([data-live]):not([data-nt])')].map(e => [keyOf(e), e]));
  async function reload() {
    const snap = hasUnsaved() ? new Map([...setupFields()].map(([k, e]) => [k, e.type === 'checkbox' ? e.checked : e.value])) : null;
    await load();
    if (!snap) return;
    for (const [k, e] of setupFields()) {
      if (!snap.has(k)) continue;
      const v = snap.get(k);
      if (e.type === 'checkbox') { if (e.checked !== v) { e.checked = v; e.dispatchEvent(new Event('change', { bubbles: true })); } }
      else if (e.value !== v) { e.value = v; e.dispatchEvent(new Event('input', { bubbles: true })); }
    }
  }
  async function load() {
    try {
      st = await api('/api/faces/status');
      if (st.models && st.settings.enabled) {
        people = (await api('/api/people')).people;
        unnamed = (await api('/api/faces?unnamed=1&limit=120')).faces;
        personFaces = person ? (await api('/api/faces?person=' + person + '&limit=200')).faces : [];
      }
    } catch (e) { el.innerHTML = '<p class="empty">' + esc(errText(e)) + '</p>'; return; }
    paint();
    clearTimeout(timer);
    if (st.download.running) timer = setTimeout(() => { if (el.isConnected) load(); }, 1000);
  }
  function paint() {
    const s = st.settings;
    let h = '<div class="ac">' + lead('Optional. Puts names to the people your cameras see, so alerts say who is at the door.', 'Use it on a doorbell or a camera that sees people close and front-on.') +
      how('When AI sees a person, AmberSight looks for a face in the full-quality recording, compares it with the faces you have named, and labels the event with the name (or &quot;unknown face&quot;). Recognition runs on this server; the face pictures and fingerprints are kept in its database (and so in its backups), and names appear in event labels and alerts. A photo of someone can fool it, so do not rely on &quot;no alerts&quot; for security.');
    if (st.open) h += notice('warn', 'This server has no sign-in accounts: anyone on your network can see these faces.', '<a class="btn" href="#settings/users/users">Add an account</a>');
    // Setup card
    h += '<div class="ac-card" data-setup><div class="ac-card-t">Set up</div>';
    if (!st.ai) h += notice('info', 'Face recognition needs AI to find people first.', '<a class="btn" href="#settings/detection/objects">Open Objects (AI)</a>');
    if (!st.models) {
      const d = st.download;
      h += '<p class="hint">Two small models are needed (about 39 MB, from the OpenCV model zoo; free to use).</p>' +
        (d.running ? '<div class="hint">Downloading... ' + Math.round(100 * d.done / Math.max(1, d.total)) + '%</div>'
          : d.error ? notice('warn', esc(d.error), '<button class="btn primary" data-dl>Try again</button>') : '<button class="btn primary" data-dl>Download the face models</button>');
    }
    h += swHtml('data-en', s.enabled, 'Recognize faces on person events', '', !st.models) +
      more('More settings', '<div class="ac-grid3">' + fld('Smallest face', 'data-min min="20" max="400" value="' + s.min_face + '"', 'pixels wide') +
      fld('Match strictness', 'data-match min="0.3" max="0.8" step="0.05" value="' + (+s.match_score).toFixed(2) + '"', '0.30 to 0.80') + '</div>' +
      '<div class="hint">Smaller faces are skipped: too few pixels to tell people apart. Higher strictness means fewer wrong names but more &quot;unknown&quot;.</div>') +
      '<div class="sec">Cameras</div>' + swHtml('data-all', !s.cameras.length, 'All cameras with AI') +
      '<div class="cs-copygrid" data-camset' + (!s.cameras.length ? ' hidden' : '') + '>' + state.cams.filter(c => !c.remote).map(c => '<label class="asys-chk"><input type="checkbox" data-c="' + esc(c.name) + '"' + (!s.cameras.length || s.cameras.includes(c.name) ? ' checked' : '') + '> ' + esc(c.name) + '</label>').join('') + '</div>' +
      '</div>';
    if (st.models && s.enabled) {
      // People
      h += '<div class="ac-card"><div class="ac-card-t">People</div>';
      h += people.length ? '<div class="fc-grid">' + people.map(p => '<button class="fc-card' + (person === p.id ? ' on' : '') + '" data-person="' + p.id + '">' + (p.face ? img(p.face) : '<span class="fc-img"></span>') +
        '<span><b>' + esc(p.name) + '</b><br><small>' + p.named + ' named · seen ' + p.seen + (p.last_ms ? ' · ' + esc(when(p.last_ms)) : '') + (p.quiet ? ' · no alerts' : '') + '</small></span></button>').join('') + '</div>'
        : '<p class="hint">Nobody named yet. Name a face below to start, or import faces you named before.</p>';
      h += '<details class="fc-import"><summary class="hint">Import named faces from a folder on this server</summary>' +
        '<div class="ac-row"><input data-imp placeholder="e.g. F:\\Faces" spellcheck="false"><button class="btn" data-impgo>Import</button></div>' +
        '<div class="hint">One folder per person, named after them, holding pictures of their face (for example the named-faces folder of the program you used before). Pictures directly in the folder, not in a person\'s folder, are skipped.</div></details>';
      const P = people.find(p => p.id === person);
      if (P) {
        h += '<div class="fc-person"><div class="ac-row"><b>' + esc(P.name) + '</b><span class="ac-sp"></span>' +
          '<span title="When only this person is recognized (and nothing else AI alerts for), the event is kept but sends no alert">' + swHtml('data-quiet', P.quiet, 'No alerts when only they are seen') + '</span>' +
          '<button class="btn" data-ren>Rename</button><button class="btn danger" data-forget>Forget</button></div>' +
          '<div class="fc-grid">' + personFaces.map(f => '<button class="fc-card" data-face="' + f.id + '">' + img(f.id) + '<span><small>' + (f.labeled ? (f.clear ? 'named' : '<span class="fc-blur" title="Named, but too small or blurry to recognize others by (used only if this person has no clear face)">named · not clear</span>') : 'matched ' + Math.round((f.sim || 0) * 100) + '%') + ' · ' + esc(when(f.at_ms)) + '</small></span></button>').join('') + '</div>' +
          '<div class="hint">Only clear faces are used to recognize ' + esc(P.name) + '; blurry or far-away ones can still be named (for the record) without making recognition worse.</div></div>';
      }
      h += '</div>';
      // Unnamed
      h += '<div class="ac-card"><div class="ac-card-t">Faces to name</div>' +
        (unnamed.length ? '<div class="hint">Click a face to say who it is, or delete it. Faces nobody names are forgotten after 60 days.</div><div class="fc-grid">' +
          unnamed.map(f => '<button class="fc-card" data-face="' + f.id + '">' + img(f.id) + '<span>' + (f.person ? esc(f.person) + ' ' + Math.round((f.sim || 0) * 100) + '%?' : 'Who is this?') + '<br><small>' + esc(when(f.at_ms)) + '</small></span></button>').join('') + '</div>'
          : '<p class="hint">None right now. Faces appear here after AI sees a person close enough to recognize.</p>') + '</div>';
    }
    h += '</div>';
    el.innerHTML = h;
    const q = (x) => el.querySelector(x);
    const dl = q('[data-dl]'); if (dl) dl.onclick = async () => { await api('/api/faces/models', { json: {} }); load(); };
    q('[data-all]').onchange = (e) => { q('[data-camset]').hidden = e.target.checked; };
    // On/off saves at once; the numbers and the camera choice go through the bar.
    liveSwitch(q('[data-en]'), async (on) => { const r = await api('/api/settings', { json: { faces: { enabled: on } } }); if (r.error) throw new Error(r.error); st.settings.enabled = on; await reload(); });
    formBar(q('[data-setup]'), { save: async () => {
      const body = {
        min_face: Math.max(20, Math.min(400, +q('[data-min]').value || 40)),
        match_score: Math.max(0.3, Math.min(0.8, +q('[data-match]').value || 0.4)),
        cameras: q('[data-all]').checked ? [] : [...el.querySelectorAll('[data-c]:checked')].map(x => x.dataset.c),
      };
      const r = await api('/api/settings', { json: { faces: body } });
      if (r.error) throw new Error(r.error);
      load();
      if (r.warning) { toast(r.warning); return false; }
    } });
    el.querySelectorAll('[data-person]').forEach(b => { b.onclick = () => { person = person === +b.dataset.person ? null : +b.dataset.person; load(); }; });
    el.querySelectorAll('[data-face]').forEach(b => { b.onclick = () => {
      const f = unnamed.concat(personFaces).find(x => String(x.id) === b.dataset.face);
      if (f) nameFaceDialog(f, () => load());
    }; });
    const ig = q('[data-impgo]');
    if (ig) ig.onclick = async () => {
      const path = q('[data-imp]').value.trim(); if (!path) { toast('Type the folder'); return; }
      ig.disabled = true; ig.textContent = 'Importing...';
      try {
        const r = await api('/api/faces/import', { json: { path } });
        toast('Imported ' + r.faces + ' face' + (r.faces === 1 ? '' : 's') + ' of ' + r.people.join(', ') + (r.skipped.length ? '; no clear face in ' + r.skipped.join(', ') : ''));
        load();
      } catch (e) { toast(errText(e)); ig.disabled = false; ig.textContent = 'Import'; }
    };
    const P = people.find(p => p.id === person);
    if (P) {
      liveSwitch(q('[data-quiet]'), async (on) => { await api('/api/people/' + P.id, { method: 'PUT', json: { quiet: on } }); P.quiet = on; });
      q('[data-ren]').onclick = async () => {
        const n = await promptDialog({ title: 'Rename ' + P.name, label: 'New name', value: P.name, ok: 'Rename' }); if (!n || n.trim() === P.name) return;
        try { await api('/api/people/' + P.id, { method: 'PUT', json: { name: n.trim() } }); load(); } catch (x) { toast(errText(x)); }
      };
      q('[data-forget]').onclick = async () => {
        if (!(await confirmDialog({ title: 'Forget ' + P.name + '?', body: 'Their name and every picture of their face are deleted. Events already labeled keep the name.', ok: 'Forget ' + P.name, danger: true }))) return;
        try { await api('/api/people/' + P.id, { method: 'DELETE' }); person = null; load(); } catch (x) { toast(errText(x)); }
      };
    }
  }
  el.innerHTML = '<div class="ac"><p class="empty">Loading...</p></div>';
  load();
  return () => clearTimeout(timer);
}

let stopFaces = null;
registerAdminPage({ id: 'faces', title: 'Faces', order: 65, render(el) { stopFaces = facesPage(el); }, leave() { if (stopFaces) stopFaces(); } });
