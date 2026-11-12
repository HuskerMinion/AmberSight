// groups.js - Settings > Cameras > Groups: create, rename and delete camera groups and pick
// their cameras. A camera can be in several groups; membership is saved on each
// camera (PUT /api/cameras/:id {groups}).

import { state, bus, api, esc, toast, openDlg, refreshCameras, confirmDialog, errText } from './core.js';

const groupsOf = (c) => c.groups || [];
export function allGroups() {
  return [...new Set(state.cams.flatMap(groupsOf))].sort((a, b) => a.localeCompare(b));
}

// Make `name` contain exactly the cameras in `ids` (and rename from `old`, if given).
export async function setGroup(name, ids, old) {
  name = name.trim();
  const want = new Set(ids.map(Number));
  const failed = [];
  for (const c of state.cams) {
    let g = groupsOf(c).filter(x => x !== old && x.toLowerCase() !== name.toLowerCase());
    if (want.has(c.id) && name) g = g.concat(name);
    const before = groupsOf(c);
    if (g.length === before.length && g.every(x => before.includes(x))) continue;
    try { await api('/api/cameras/' + c.id, { method: 'PUT', json: { groups: g } }); }
    catch (e) { failed.push(c.name); }
  }
  await refreshCameras();
  // (Another server's camera can be unreachable for a moment.)
  if (failed.length) throw new Error('not changed for ' + failed.join(', ') + ' (try again in a moment)');
}

function editDlg(name) {
  const isNew = name == null;
  const members = new Set(state.cams.filter(c => !isNew && groupsOf(c).includes(name)).map(c => c.id));
  const d = openDlg(isNew ? 'New group' : 'Group: ' + name,
    '<label>Name <input data-name value="' + esc(name || '') + '" placeholder="e.g. Outside"></label>' +
    '<div class="sec">Cameras in this group</div><div class="cs-copygrid">' +
    state.cams.map(c => '<label class="asys-chk"><input type="checkbox" data-cam="' + c.id + '"' + (members.has(c.id) ? ' checked' : '') + '> ' + esc(c.name) + '</label>').join('') +
    '</div><div class="hint">Groups only filter and organize cameras; they never trigger recording by themselves.</div>' +
    '<div class="asys-err" data-err></div><div class="row"><button class="primary" data-save>Save</button><button class="btn" data-no>Cancel</button></div>', true);
  d.classList.add('wide');
  d.querySelector('[data-no]').onclick = () => d.__close();
  d.querySelector('[data-save]').onclick = async () => {
    const n = d.querySelector('[data-name]').value.trim();
    const err = d.querySelector('[data-err]');
    if (!n) { err.textContent = 'Enter a name.'; return; }
    if (n !== name && allGroups().some(g => g.toLowerCase() === n.toLowerCase())) { err.textContent = 'There is already a group called ' + n + '.'; return; }
    const ids = [...d.querySelectorAll('[data-cam]:checked')].map(x => +x.dataset.cam);
    try { await setGroup(n, ids, name); d.__close(); toast(isNew ? 'Group ' + n + ' created' : 'Group saved'); }
    catch (e) { err.textContent = e.body || e.message; }
  };
  setTimeout(() => d.querySelector('[data-name]').focus(), 0);
}

async function deleteDlg(name) {
  if (!(await confirmDialog({ title: 'Delete group ' + name + '?', body: 'The cameras stay; they just leave this group.', ok: 'Delete group', danger: true }))) return;
  try { await setGroup('', [], name); toast('Group deleted'); }
  catch (e) { toast(errText(e), { kind: 'err' }); }
}

export function mountGroups(host) {
  function draw() {
    const gs = allGroups();
    host.innerHTML = '<div class="cs-listbar"><span class="hint">Groups organize cameras for the live view. A camera can be in several groups.</span><button class="btn" data-new>+ New group</button></div>' +
      (gs.length ? '<div class="cs-list">' + gs.map(g => {
        const cams = state.cams.filter(c => groupsOf(c).includes(g));
        return '<div class="cs-item"><div class="cs-name"><b>' + esc(g) + '</b><div class="cs-url">' + cams.length + ' camera' + (cams.length === 1 ? '' : 's') + ': ' + esc(cams.map(c => c.name).join(', ')) + '</div></div>' +
          '<button class="btn" data-edit="' + esc(g) + '">Edit</button><button class="btn danger" data-del="' + esc(g) + '">Delete</button></div>';
      }).join('') + '</div>' : '<p class="empty">No groups yet. Create one here, from a layout, or in a camera\'s settings.</p>');
    host.querySelector('[data-new]').onclick = () => editDlg(null);
    host.querySelectorAll('[data-edit]').forEach(b => { b.onclick = () => editDlg(b.dataset.edit); });
    host.querySelectorAll('[data-del]').forEach(b => { b.onclick = () => deleteDlg(b.dataset.del); });
  }
  let sig = '';
  const onCams = () => {
    if (!host.isConnected) { bus.off('cameras', onCams); return; }
    const s = JSON.stringify(state.cams.map(c => [c.id, c.name, c.groups]));
    if (s !== sig) { sig = s; draw(); }
  };
  onCams();
  bus.on('cameras', onCams);
}
