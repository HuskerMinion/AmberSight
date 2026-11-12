// status_page.js - Settings > System > Status & updates: overview cards, cameras (fps, bitrate,
// GB/day, uptime), storage planning (days that fit, "keep N days needs X"),
// connections, recent warnings. Replaces the old Health page.
import { api, esc, toast, fmtSize, ic, openDlg, raiseDlg, camFromId, refreshCameras, stillWaiting } from './core.js';
import { registerAdminPage, openAdminPage } from './admin.js';
import { lead, more, okIc, tablistKeys } from './sp_kit.js';

const errText = (e) => (e && e.body) ? e.body : (e && e.message) || String(e);
const wrap = (html) => '<div class="asys-scroll">' + html + '</div>';
const dur = (s) => {
  s = Math.max(0, Math.round(s));
  const d = Math.floor(s / 86400), h = Math.floor(s % 86400 / 3600), m = Math.floor(s % 3600 / 60);
  if (d) return d + 'd ' + h + 'h';
  if (h) return h + 'h ' + m + 'm';
  if (m) return m + 'm ' + (s % 60) + 's';
  return s + 's';
};
// A camera's last error in words, without stream addresses (older servers send only the raw text).
const errPlain = (c) => c.last_error_plain != null ? c.last_error_plain : (c.last_error || '').split(/\s+/).filter(w => !w.includes('://')).join(' ');
const ago = (ts) => ts ? dur(Date.now() / 1000 - ts) + ' ago' : '-';
// The Last error cell: with when it happened, and greyed once the camera has been fine since.
const lastErr = (c) => {
  const t = errPlain(c);
  if (!t) return '<td class="asys-errc"></td>';
  const past = c.online && c.last_error_at && c.online_since && c.last_error_at <= c.online_since;
  const when = c.last_error_at ? ago(c.last_error_at) : '';
  const tip = t + (when ? ' (' + when + (past ? '; fine since' : '') + ')' : '');
  return '<td class="asys-errc' + (past ? ' muted' : '') + '" title="' + esc(tip) + '">' + (when ? '<span class="st-errwhen">' + esc(when) + '</span> ' : '') + esc(t) + '</td>';
};
const days = (d) => d == null ? '-' : d >= 100 ? Math.round(d) + ' days' : d >= 10 ? d.toFixed(0) + ' days' : d.toFixed(1) + ' days';
const perDay = (b) => b ? fmtSize(b) + '/day' : '-';
const kbps = (k) => k == null ? '-' : k >= 1000 ? (k / 1000).toFixed(1) + ' Mbit/s' : k + ' kbit/s';

const TABS = [['overview', 'Overview'], ['cameras', 'Cameras'], ['perf', 'Performance'], ['storage', 'Storage'], ['conns', 'Connections']];   // warnings and errors live on the Logs page
const TAB_KEY = 'ambs.statusTab';
let timer = null;

function statusPage(el) {
  const saveTab = (t) => { try { localStorage.setItem(TAB_KEY, t); } catch (e) { /* not remembered */ } };
  let tab = 'overview';
  try { tab = localStorage.getItem(TAB_KEY) || 'overview'; } catch (e) { /* storage blocked */ }
  if (!TABS.some(t => t[0] === tab)) tab = 'overview';
  el.innerHTML = '<div class="asys">' + lead('Shows at a glance whether everything is working, and lets you update AmberSight.', 'Check here first if something looks wrong.') + '<div class="asys-head"><span class="hint" id="stStamp"></span>' +
    '<button class="btn" id="stNow">' + ic('swap') + ' Refresh</button></div>' +
    '<div class="chips st-tabs" role="tablist">' + TABS.map(t => '<button type="button" role="tab" class="chip" data-t="' + t[0] + '">' + t[1] + '</button>').join('') + '</div>' +
    '<div id="stBody"><p class="empty">Loading...</p></div></div>';
  const body = el.querySelector('#stBody');
  const d = { noisy: [], health: null, stats: null, status: null, plan: null, planAt: 0, conns: null, logs: null, upd: null };
  const hushed = new Set();   // suggestions answered here (a refresh already on its way must not bring them back)
  let keepDays = null;   // the storage planner's "keep N days" (defaults to the setting)
  let busy = false;
  const fixing = new Set();
  let syncing = false;   // "Sync all camera clocks" running
  let checking = false;  // "Check for updates" running
  let waitFor = null;    // the version AmberSight is restarting into

  let noisyAt = 0;       // when the noisy-camera suggestions were last asked for
  // A request the server has not answered for a while: say so, and Retry asks again.
  stillWaiting(el.querySelector('.asys'), () => { busy = false; planBusy = false; load(); });
  async function load(force) {
    if (busy || !el.isConnected) return;
    busy = true;
    try {
      const jobs = [];
      if (tab === 'perf') jobs.push(api('/api/stats').then(v => { d.stats = v; }));
      if (tab === 'overview' || tab === 'cameras') {
        jobs.push(api('/api/health').then(v => { d.health = v; }));
        jobs.push(api('/api/stats').then(v => { d.stats = v; }).catch(() => {}));
        jobs.push(api('/api/status').then(v => { d.status = v; }).catch(() => {}));
      }
      if (tab === 'overview') jobs.push(api('/api/update').then(gotUpdate).catch(() => {}));
      if (tab === 'overview' && (force || Date.now() - noisyAt >= 60000)) {
        noisyAt = Date.now();
        jobs.push(api('/api/noisy').then(v => { d.noisy = (v.suggestions || []).filter(x => !hushed.has(x.camera_id)); }).catch(() => {}));
      }
      if (tab === 'conns') jobs.push(api('/api/connections').then(v => { d.conns = v; }));
      // (The storage plan is slow on a big site: it is fetched on its own below.)
      await Promise.all(jobs);
      draw();
      el.querySelector('#stStamp').textContent = 'updated ' + new Date().toLocaleTimeString();
      loadPlan(force);
    } catch (e) {
      // While an update restarts the server, keep the page and wait for it.
      if (waitFor) draw();
      else body.innerHTML = '<p class="empty">Could not load the status: ' + esc(errText(e)) + '</p>';
    }
    busy = false;
  }

  // The plan reads the whole recording index: it is asked for on its own (never
  // two at once) and the page is drawn again when it arrives.
  let planBusy = false;
  async function loadPlan(force) {
    if (planBusy || !el.isConnected) return;
    if (!force && d.plan && Date.now() - d.planAt <= 60000) return;
    planBusy = true;
    try {
      d.plan = await api('/api/storage/plan' + (force ? '?force=true' : ''));
      d.planAt = Date.now();
      if (el.isConnected) draw();
    } catch (e) { /* the cards keep saying Calculating until it works */ }
    planBusy = false;
  }

  function gotUpdate(u) {
    const p = u.progress || {};
    if (p.state === 'restarting') waitFor = u.latest;
    else if (waitFor) {
      // A new server answered: did the new version start?
      toast(u.current === waitFor ? 'AmberSight is now version ' + u.current + '.' : 'AmberSight restarted, but it is still version ' + u.current + ' (see Settings > System > Logs).');
      waitFor = null;
    }
    d.upd = u;
  }

  function draw() {
    // Keep the planner input focused while numbers refresh around it.
    if (tab === 'storage' && body.querySelector('#stKeep') === document.activeElement) return;
    if (tab === 'perf' && body.querySelector('.pf-spark.on')) return;   // keep the hover readout
    el.querySelectorAll('.st-tabs button').forEach(b => { b.classList.toggle('on', b.dataset.t === tab); b.setAttribute('aria-selected', b.dataset.t === tab ? 'true' : 'false'); });
    const f = { overview, cameras, perf, storage, conns }[tab];
    // A redraw keeps which "Details" are open and where the keyboard focus was.
    const open = [...body.querySelectorAll('details')].map(x => x.open);
    const act = document.activeElement && body.contains(document.activeElement) && document.activeElement !== body ? document.activeElement : null;
    let key = null;
    if (act) {
      const da = [...act.attributes].find(a => a.name.startsWith('data-'));
      key = da ? '[' + da.name + (da.value ? '="' + CSS.escape(da.value) + '"' : '') + ']'
        : act.id ? '#' + CSS.escape(act.id) : null;
    }
    body.innerHTML = f();
    body.querySelectorAll('details').forEach((x, i) => { if (open[i]) x.open = true; });
    if (key) { const n = body.querySelector(act.tagName.toLowerCase() + key); if (n) n.focus({ preventScroll: true }); }
  }

  // ------------------------------------------------------------ Overview
  // A card says its state in a word and an icon, not only in the colour of its edge.
  function card(title, big, lines, lvl, go, word) {
    const w = word === null ? '' : lvl === 'bad' ? ic('alert') + ' Problem' : lvl === 'warn' ? ic('alert') + ' Check' : okIc + ' OK';
    return '<button type="button" class="st-card' + (lvl ? ' ' + lvl : '') + '"' + (go ? ' data-go="' + go + '"' : '') + '>' +
      '<span class="st-ct">' + esc(title) + (w ? ' <span class="st-lvl">' + w + '</span>' : '') + '</span><span class="st-cv">' + big + '</span>' +
      lines.filter(Boolean).map(l => '<span class="st-cl">' + l + '</span>').join('') + '</button>';
  }

  // One reading of every card's state: the banner above the cards and the cards
  // themselves both come from here, so they cannot disagree.
  function cardState() {
    const h = d.health || {}, st = d.status || {}, p = d.plan || {};
    const en = (h.cameras || []).filter(c => c.enabled);
    const off = en.filter(c => !c.online && !c.sleeping);
    const asleep = en.filter(c => !c.online && c.sleeping);
    const keep = p.retention_days;
    // Days kept counts the archive folder too (the server works it out).
    let kept = p.days_kept;
    if (kept === undefined) { const sf = shortestFolder(); kept = sf ? Math.min(sf.days_fit, keep || Infinity) : null; }
    const storageLvl = d.plan && kept != null && keep && kept < keep ? (kept < keep / 2 ? 'bad' : 'warn') : '';
    return {
      en, off, asleep, online: en.length - off.length - asleep.length, keep, kept,
      cams: off.length ? 'warn' : '',
      storage: storageLvl,
      detect: st.armed === false ? 'warn' : '',
      hasArchive: ((p.folders) || []).some(f => f.archive),
    };
  }

  // The banner at the top: Everything is OK only when every card is OK; otherwise
  // each problem in a few words, with the button that helps.
  function warningsHtml(h) {
    const ws = (h.warnings || []).slice();
    const cs = cardState();
    const items = ws.map((w, i) => ({ text: w, fix: (h.fixes || []).find(x => x.warning === i), bad: true }));
    const covered = (re) => ws.some(w => re.test(w));
    // What the cards say that the health warnings do not already say.
    // (By the exact name: "Porch" is not covered by "Back Porch has been offline".)
    const unnamed = cs.off.filter(c => !ws.some(w => w.startsWith(c.name + ' has been offline')));
    if (unnamed.length) {
      const txt = unnamed.length === 1 ? unnamed[0].name + ' is offline' : unnamed.length + ' cameras are offline (' + unnamed.map(c => c.name).join(', ') + ')';
      items.push({ text: txt, link: ['#settings/cameras/list', 'Open Cameras'] });
    }
    if (cs.storage && !covered(/disk|drive|storage|space|full|folder/i)) {
      items.push({ text: 'Storage holds fewer days than you asked for: ' + days(cs.kept) + ' kept, ' + cs.keep + ' wanted', bad: cs.storage === 'bad', link: ['#settings/recording/retention', 'Open Storage &amp; retention'] });
    }
    if (cs.detect) items.push({ text: 'Detection is disarmed: nothing is being alerted' });
    if (!items.length) return '<div class="st-banner ok" role="status">' + okIc + '<b>Everything is OK</b><span class="muted">No problems right now.</span></div>';
    const link = (href, text) => '<a class="btn asys-fix" href="' + href + '">' + text + '</a>';
    const anyBad = items.some(x => x.bad);
    const plural = items.length === 1;
    return '<div class="st-banner ' + (anyBad ? 'bad' : 'warn') + '" role="' + (anyBad ? 'alert' : 'status') + '"><div class="st-banner-h">' + ic('alert') + '<b>' + items.length + (anyBad ? (plural ? ' problem' : ' problems') : (plural ? ' thing to check' : ' things to check')) + '</b></div>' + items.map((it) => {
      const x = it.fix, w = it.text;
      let btn = it.link ? link(it.link[0], it.link[1]) : '';
      if (x && x.kind === 'timesync') btn = '<button class="btn asys-fix" data-timesync="' + esc(String(x.camera)) + '"' + (fixing.has(x.camera) ? ' disabled' : '') + '>' + (fixing.has(x.camera) ? 'Turning on...' : 'Turn on time sync') + '</button>';
      else if (!btn && /disk|drive|storage|space|full|folder/i.test(w)) btn = link('#settings/recording/retention', 'Open Storage &amp; retention');
      else if (!btn && /offline|no video|not recording|no recordings/i.test(w)) btn = link('#settings/cameras/list', 'Open Cameras');
      return '<div class="st-prob"><span>' + esc(w.charAt(0).toUpperCase() + w.slice(1)) + '</span>' + btn + '</div>';
    }).join('') + '</div>';
  }

  // Shortest time any folder holds at today's rate (null when nothing records).
  function shortestFolder() {
    const fs = ((d.plan && d.plan.folders) || []).filter(x => x.days_fit != null);
    if (!fs.length) return null;
    return fs.reduce((a, b) => (b.days_fit < a.days_fit ? b : a));
  }

  function overview() {
    if (!d.health) return '<p class="empty">Loading...</p>';
    const h = d.health || {}, s = d.stats || {}, st = d.status || {}, p = d.plan || {};
    const cs = cardState();
    const used = ((p.folders) || []).reduce((a, x) => a + (x.used_bytes || 0), 0);
    const rate = ((p.folders) || []).reduce((a, x) => a + (x.bytes_per_day || 0) - (x.inflow_per_day || 0), 0);
    const db = p.database || {};
    const cpu = s.cpu, mem = s.mem;
    const keep = cs.keep;
    const fpsTxt = s.fps ? ', ' + s.fps.total + ' fps in total' : '';
    const camLines = [cs.asleep.length ? (cs.asleep.length === 1 ? 'battery camera' : 'battery cameras') + ' asleep to save power' + fpsTxt : 'online' + fpsTxt];
    const camBig = cs.asleep.length ? cs.online + ' online, ' + cs.asleep.length + ' asleep' : cs.online + ' / ' + cs.en.length;
    if (cs.off.length) camLines.push('<b>' + cs.off.length + ' offline</b>');
    let html = warningsHtml(h) + noisyHtml() + '<div class="st-cards">' +
      card('Cameras', camBig, camLines, cs.cams, 'cameras') +
      (!d.plan ? card('Storage', 'Calculating...', ['Working out how long recordings are kept'], '', 'storage', null) :
      card('Storage', cs.kept != null ? esc(days(cs.kept)) : fmtSize(used), [
        cs.kept != null ? 'kept at today\'s rate' + (cs.hasArchive ? ', in the folders and the archive' : '') + (keep && cs.kept >= keep ? ' (the ' + keep + '-day setting)' : keep ? ' (you asked for ' + keep + ')' : '') : 'recordings stored',
        rate ? 'Recording ' + perDay(rate) + ', ' + fmtSize(used) + ' stored' : '',
      ], cs.storage, 'storage')) +
      card('Detection', st.armed === false ? 'Disarmed' : 'Armed', [st.armed === false ? 'Disarmed (not alerting)' : 'Watching for motion'], cs.detect, null) +
      '</div>';
    html += updatesHtml();
    html += more('Details',
      '<div class="st-cards">' +
      card('Server', esc(st.version ? 'v' + st.version : '-'), [
        'Up ' + dur(st.uptime_seconds != null ? st.uptime_seconds : (s.uptime_s || 0)),
        cpu ? 'CPU ' + cpu.machine + '% (AmberSight ' + cpu.process + '%)' : '',
        mem ? 'Memory ' + fmtSize(mem.used) + ' of ' + fmtSize(mem.total) : '',
        st.ffmpeg ? 'ffmpeg ' + esc(st.ffmpeg) : '',
      ], '', null, null) +
      card('Database', db.bytes != null ? fmtSize(db.bytes) : d.plan ? '-' : 'Calculating...', [
        db.segments != null ? db.segments.toLocaleString() + ' recordings indexed' : '',
        db.events != null ? db.events.toLocaleString() + ' events' : '',
      ], '', 'storage', null) + '</div>');
    return html;
  }

  // Cameras that sent far too many notifications for one thing in one place: what to do about it.
  function noisyHtml() {
    return (d.noisy || []).map(n => {
      const can = (a) => (n.actions || []).includes(a);
      const imgs = (n.examples || []).map(x => '<img loading="lazy" src="/api/events/' + encodeURIComponent(x.event_id) + '/thumb" alt="An example notification">').join('');
      return '<section class="st-noisy" aria-label="Too many notifications from ' + esc(n.camera) + '"><div class="st-noisy-h">' + ic('alert') + '<b>Lots of notifications from ' + esc(n.camera) + '</b></div>' +
        '<p>' + esc(n.text) + (n.not_real ? ' You marked ' + n.not_real + ' of them "Not a real alert".' : '') +
          (n.held_back ? ' AI already kept ' + n.held_back + ' more quiet: things that sat in the same spot event after event.' : '') + '</p>' +
        '<p class="muted">If AI keeps taking the same thing for a person or a car, open one of those events and press “Not a person” (or “Not a car”) under the AI verdict: it remembers that spot.</p>' +
        (imgs ? '<div class="st-noisy-pics">' + imgs + '</div>' : '') +
        '<p class="muted">Everything is still detected, recorded and searchable. Only the notifications change.</p>' +
        '<div class="st-noisy-act">' +
        (can('people_only') ? '<button type="button" class="primary" data-nz="people_only" data-nzc="' + n.camera_id + '">Notify for people only</button>' : '') +
        (can('ignore_area') ? '<button type="button" class="btn" data-nz="ignore_area" data-nzc="' + n.camera_id + '">Don\'t notify for that area</button>' : '') +
        '<button type="button" class="btn" data-nz="not_now" data-nzc="' + n.camera_id + '">Not now</button></div></section>';
    }).join('');
  }

  async function noisyAct(kind, id) {
    const n = (d.noisy || []).find(x => String(x.camera_id) === String(id));
    if (!n) return;
    const done = async (msg) => {
      hushed.add(n.camera_id);
      try { await api('/api/noisy/' + id + '/dismiss', { json: {} }); } catch (err) { /* it stays listed */ }
      d.noisy = d.noisy.filter(x => x !== n);
      toast(msg, { kind: 'ok' });
      draw();
    };
    if (kind === 'not_now') {
      try { await api('/api/noisy/' + id + '/dismiss', { json: {} }); } catch (err) { toast('Could not hide that: ' + errText(err)); return; }
      hushed.add(n.camera_id); d.noisy = d.noisy.filter(x => x !== n); toast('Okay. It will not suggest this for a week.'); draw(); return;
    }
    if (kind === 'people_only') {
      try { await api('/api/cameras/' + id, { method: 'PUT', json: { alert_labels: ['person'] } }); } catch (err) { toast('Could not change ' + n.camera + ': ' + errText(err)); return; }
      refreshCameras().catch(() => {});
      await done(n.camera + ' now notifies for people only, and for doorbell presses, sounds and packages, which always notify. Other things are still detected and recorded.');
      return;
    }
    if (kind === 'ignore_area') areaDialog(n, done);
  }

  // Shows the camera's picture with the suggested area drawn on it; Save adds it to the camera.
  function areaDialog(n, done) {
    const pts = (n.region || []).map(q => [q[0], q[1]]);
    const poly = pts.map(q => (q[0] * 100).toFixed(2) + ',' + (q[1] * 100).toFixed(2)).join(' ');
    const dlg = openDlg('Don\'t notify for this area', '<p>Things in the shaded area will not send a notification from ' + esc(n.camera) + '. They are still detected, recorded and searchable.</p>' +
      '<div class="st-noisy-snap"><img alt="Picture from ' + esc(n.camera) + ' with the area shaded" src="/api/snapshot/' + n.camera_id + '?t=' + Date.now() + '">' +
      '<svg viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true"><polygon points="' + poly + '"/></svg></div>' +
      '<p class="muted">You can change or remove areas any time in the camera\'s Alerts settings.</p>' +
      '<div class="row"><button type="button" class="btn" data-az-no>Cancel</button><button type="button" class="primary" data-az-yes>Don\'t notify for this area</button></div>');
    dlg.querySelector('[data-az-no]').onclick = () => dlg.__close();
    dlg.querySelector('[data-az-yes]').onclick = async (ev) => {
      ev.target.disabled = true;
      try {
        try { await refreshCameras(); } catch (err) { /* use what we have */ }
        const cur = (camFromId(n.camera_id) || {}).alert_ignore_polys || [];
        await api('/api/cameras/' + n.camera_id, { method: 'PUT', json: { alert_ignore_polys: cur.concat([{ points: pts }]) } });
      } catch (err) { ev.target.disabled = false; toast('Could not save the area: ' + errText(err)); return; }
      dlg.__close();
      refreshCameras().catch(() => {});
      done('Saved. ' + n.camera + ' will not notify for that area. Other things are still detected and recorded.');
    };
  }

  // The Updates row: this version, a newer one, and an install's progress.
  function updatesHtml() {
    const u = d.upd;
    if (!u) return '';
    const p = u.progress || { state: 'idle' };
    const v = (x) => esc('v' + x);
    const working = ['downloading', 'verifying', 'installing'].includes(p.state);
    let txt, act = '', bar = '';
    if (waitFor || p.state === 'restarting') {
      txt = '<b>Restarting into ' + v(waitFor || u.latest) + '</b> <span class="muted">This page reconnects by itself.</span>';
    } else if (working) {
      const what = { downloading: 'Downloading', verifying: 'Checking the signature of', installing: 'Installing' }[p.state];
      txt = '<b>' + what + ' ' + v(u.latest) + '</b>' + (p.state === 'downloading' && p.total ? ' <span class="muted">' + fmtSize(p.done) + ' of ' + fmtSize(p.total) + '</span>' : '');
      bar = '<div class="asys-bar"><i style="width:' + (p.total ? Math.min(100, p.done / p.total * 100) : 100).toFixed(1) + '%"></i></div>';
    } else if (p.state === 'installed') {
      txt = '<b>' + v(u.latest) + ' is installed.</b> <span class="muted">Restart AmberSight to start using it (the previous version is kept in the <i>previous</i> folder).</span>';
    } else if (u.available) {
      const notes = /^https:\/\//.test(u.notes_url || '') ? ' <a href="' + esc(u.notes_url) + '" target="_blank" rel="noopener">What\'s new</a>' : '';
      txt = '<b>' + v(u.latest) + ' is available</b> <span class="muted">(this is ' + v(u.current) + ')</span>' + notes +
        (u.why_not ? '<div class="muted">' + esc(u.why_not) + '</div>' : '');
      if (u.can_install) act = '<button class="primary" data-updinstall>Update to ' + v(u.latest) + '</button>';
    } else {
      txt = '<b>' + v(u.current) + '</b> <span class="muted">' +
        // (A failed check is said once, in the error line below.)
        (u.error && !u.latest ? 'Checked ' + ago(u.checked_at || 0) : (u.latest ? 'Up to date' : u.checked_at ? 'No release found' : 'Not checked yet') +
        (u.checked_at ? ', checked ' + ago(u.checked_at) : '')) +
        (u.check_enabled ? '; checks every 12 hours' : '; automatic checks are off') + '</span>';
    }
    const busyNow = checking || working || waitFor || p.state === 'restarting';
    act += '<button class="btn" data-updcheck' + (busyNow ? ' disabled' : '') + '>' + (checking ? 'Checking...' : 'Check for updates') + '</button>';
    let err = '';
    if (p.state === 'failed' && p.error) err = '<div class="st-upd-err">Update failed: ' + esc(p.error) + '</div>';
    else if (u.error && !working) err = '<div class="st-upd-err">Could not check: ' + esc(u.error) + '</div>';
    return '<div class="st-upd"><span class="st-ct">Updates</span><div class="st-upd-txt">' + txt + '</div>' + act + bar + err + '</div>';
  }

  // "Update to vX": a small window to confirm, built into the page.
  let updDlg = null;
  function confirmUpdate() {
    const u = d.upd;
    if (!u || !u.can_install || raiseDlg(updDlg)) return;
    const restart = u.restarts_itself
      ? 'Then AmberSight restarts by itself; cameras stop recording for a few seconds.'
      : 'Then restart AmberSight yourself to start using it.';
    updDlg = openDlg('Update AmberSight', '<div>Update from <b>v' + esc(u.current) + '</b> to <b>v' + esc(u.latest) + '</b>? ' +
      'AmberSight downloads it, checks its signature, and keeps this version in the <b>previous</b> folder next to the program. ' + restart + '</div>' +
      '<div class="row"><button class="primary" data-yes>Update</button><button class="btn" data-no>Cancel</button></div>', true);
    const dlg = updDlg;
    const close = () => { updDlg = null; dlg.__close(); };
    dlg.querySelector('.bar button').onclick = close;
    dlg.querySelector('[data-no]').onclick = close;
    dlg.querySelector('[data-yes]').onclick = async () => {
      close();
      try {
        gotUpdate(await api('/api/update/install', { json: {} }));
      } catch (err) {
        toast('Could not start the update: ' + errText(err));
      }
      draw();
      load();
    };
  }

  // ------------------------------------------------------------ Cameras
  function cameras() {
    if (!d.health) return '<p class="empty">Loading...</p>';
    const h = d.health || {}, s = d.stats || {}, p = d.plan || {};
    const cams = h.cameras || [];
    if (!cams.length) return '<p class="empty">No cameras configured.</p>';
    const fps = {}; for (const c of s.cameras || []) fps[c.name] = c.fps;
    const plan = {}; for (const c of p.cameras || []) plan[c.name] = c;
    const now = Date.now() / 1000;
    return '<div class="ac-row st-sync"><span class="hint">Camera clocks: the time each camera stamps on its picture. Sync turns on each camera\'s own time sync (NTP) and sets its clock to this computer\'s now.</span><span class="ac-sp"></span><button class="btn" data-syncall' + (syncing ? ' disabled' : '') + '>' + (syncing ? 'Syncing…' : 'Sync all camera clocks') + '</button></div>' +
      wrap('<table class="asys-tbl st-cams"><thead><tr><th>Camera</th><th>State</th><th class="num">Clock</th><th class="num">FPS</th><th class="num">Bitrate</th><th class="num">Per day</th><th class="num">Kept</th>' +
      '<th>Up / down for</th><th class="num">Reconnects</th><th class="num">Viewers</th><th>Last recording</th><th>Last error</th></tr></thead><tbody>' +
      cams.map(c => {
        let b;
        if (!c.enabled) b = '<span class="badge">Disabled</span>';
        else if (c.sleeping) b = '<span class="badge info dot">Asleep</span>';
        else if (c.online) b = '<span class="badge on dot">Online</span>';
        else b = '<span class="badge off dot">Offline</span>';
        const since = c.online ? c.online_since : c.offline_since;
        const f = fps[c.name];
        const pc = plan[c.name] || {};
        const rec = c.last_recording_end_ms ? ago(c.last_recording_end_ms / 1000) : '-';
        const kept = pc.oldest_ms ? dur(now - pc.oldest_ms / 1000) : '-';
        const sk = c.clock_skew_s;
        const clock = sk == null ? '-' : Math.abs(sk) < 2 ? '<span class="muted">right</span>' : '<span' + (Math.abs(sk) > 30 ? ' class="st-bad"' : '') + '>' + (sk > 0 ? '+' : '') + Math.round(sk) + ' s</span>';
        return '<tr><td><b>' + esc(c.name) + '</b></td><td>' + b + '</td><td class="num">' + clock + '</td><td class="num">' + (f != null && c.online ? f : '-') + '</td>' +
          '<td class="num">' + kbps(pc.kbps) + '</td>' +
          '<td class="num"' + (pc.rough ? ' title="Less than a day of recordings so far: a rough figure"' : '') + '>' + perDay(pc.bytes_per_day) + (pc.rough ? '*' : '') + '</td>' +
          '<td class="num" title="Oldest recording still kept">' + kept + '</td>' +
          '<td>' + (c.enabled && since ? dur(now - since) : '-') + '</td>' +
          '<td class="num">' + (c.reconnects != null ? c.reconnects : '-') + '</td><td class="num">' + (c.live_viewers != null ? c.live_viewers : '-') + '</td>' +
          '<td>' + rec + '</td>' + lastErr(c) + '</tr>';
      }).join('') + '</tbody></table>') +
      '<p class="hint">Bitrate is from the last hour of recordings; per day is measured over the last ' + ((p.window_days) || 7) + ' days (* = less than a day so far). Kept is the age of the oldest recording still stored.</p>';
  }

  // ------------------------------------------------------------ Performance
  // Small multiples: one tiny chart per measure over the last hour (the
  // server keeps a point a minute; for its first minutes the 5-minute history
  // stands in), then a sortable per-camera table.
  const pfSeries = {};   // chart id -> { a, b, fmt, step }
  let pfSort = { k: 'name', dir: 1 };
  const fmtMs = (v) => v >= 1000 ? (v / 1000).toFixed(1) + ' s' : Math.round(v) + ' ms';
  const fmtRate = (b) => b >= 1e6 ? (b / 1e6).toFixed(1) + ' MB/s' : b >= 1e3 ? Math.round(b / 1e3) + ' kB/s' : Math.round(b) + ' B/s';
  const fmtBits = (b) => { const bits = b * 8; return bits >= 1e6 ? (bits / 1e6).toFixed(1) + ' Mbit/s' : Math.round(bits / 1e3) + ' kbit/s'; };
  const fmtPct = (v) => (v >= 10 ? Math.round(v) : v.toFixed(1)) + '%';

  function spark(id, a, b, fmt, step, fixedTop) {
    pfSeries[id] = { a, b, fmt, step };
    const vals = a.concat(b || []);
    if (a.length < 2) return '<div class="pf-spark" data-pf="' + esc(id) + '"><div class="none">collecting...</div></div>';
    const top = fixedTop || Math.max(1e-9, Math.max(...vals) * 1.15);
    const W = 100, H = 54, n = a.length;
    const path = (v) => v.map((y, i) => (i ? 'L' : 'M') + (i / (n - 1) * W).toFixed(2) + ' ' + (H - 2 - Math.min(1, y / top) * (H - 4)).toFixed(2)).join('');
    const pa = path(a);
    return '<div class="pf-spark" data-pf="' + esc(id) + '"><svg viewBox="0 0 ' + W + ' ' + H + '" preserveAspectRatio="none" aria-hidden="true">' +
      '<path class="a" d="' + pa + 'L' + W + ' ' + H + 'L0 ' + H + 'Z"/>' +
      (b && b.length === n ? '<path class="l b" d="' + path(b) + '"/>' : '') +
      '<path class="l" d="' + pa + '"/></svg><i class="pf-rule"></i></div>';
  }

  function box(id, title, now, unit, a, b, fmt, step, opts) {
    opts = opts || {};
    const all = a.concat(b || []);
    // (The value shown now counts too: the chart's points are minute averages.)
    const cur = typeof opts.cur === 'number' && isFinite(opts.cur) ? [opts.cur] : [];
    const mx = all.length ? Math.max(...all, ...cur) : 0;
    const mn = a.length ? Math.min(...a, ...cur) : 0;
    // The middle of the foot shows the value under the pointer; the legend has a line of its own.
    return '<div class="pf-box"' + (opts.lvl ? ' data-lvl="' + opts.lvl + '"' : '') + '><div class="pf-top"><span class="pf-ttl" title="' + esc(opts.tip || title) + '">' + esc(title) + '</span>' +
      '<span class="pf-now">' + now + (unit ? '<small>' + unit + '</small>' : '') + '</span></div>' +
      spark(id, a, b, fmt, step, opts.top) +
      '<div class="pf-foot">' + (a.length > 1 ? '<span>low <b>' + fmt(mn) + '</b></span><span></span><span>high <b>' + fmt(mx) + '</b></span>' : '<span>' + (opts.empty || '') + '</span>') + '</div>' +
      (opts.legend ? '<div class="pf-leg" title="' + esc(opts.legendTip || opts.tip || '') + '">' + opts.legend + '</div>' : '') + '</div>';
  }

  function perf() {
    const s = d.stats;
    if (!s) return '<p class="empty">Loading...</p>';
    const long = s.history_1h || {};
    const recent = s.history || {};
    const useLong = (long.cpu || []).length >= 2;
    const H = useLong ? long : recent;
    const step = useLong ? long.step_s : s.interval_s;
    const span = (H.cpu || []).length * step;
    const get = (k) => (H[k] || []);
    const ai = s.ai || {};
    const g = s.gpu, net = s.net;
    let html = '<p class="pf-note">' + (useLong ? 'Last hour, one point a minute (averaged).' : 'The server has run less than two minutes: showing the last ' + Math.max(1, Math.round(span / 60)) + ' min, a point every ' + step + ' s.') +
      ' Hover a chart for the value at that time.</p><div class="pf-grid">';
    html += box('cpu', 'CPU', fmtPct(s.cpu.machine).replace('%', ''), '%', get('cpu'), null, fmtPct, step, { top: 100, lvl: s.cpu.machine > 90 ? 'warn' : '', cur: s.cpu.machine });
    const memPct = s.mem.total ? 100 * s.mem.used / s.mem.total : 0;
    html += box('mem', 'Memory', fmtPct(memPct).replace('%', ''), '%', get('mem'), null, fmtPct, step, { top: 100, cur: memPct });
    if (ai.state && ai.state !== 'off') {
      const lvl = ai.state === 'stalled' ? 'crit' : ai.p95_ms > 2000 ? 'warn' : '';
      html += box('ai_p50', 'AI check time', ai.p50_ms != null ? fmtMs(ai.p50_ms).split(' ')[0] : ai.state, ai.p50_ms != null ? fmtMs(ai.p50_ms).split(' ')[1] : '', get('ai_p50'), null, (v) => (v ? fmtMs(v) : '-'), step,
        { lvl, tip: 'Time the AI takes for one check, on ' + (ai.device || '?'), legend: esc(ai.device || ''), empty: 'No AI checks yet' });
      html += box('ai_checks', 'AI checks per minute', ai.checks_1m, '', get('ai_checks'), null, (v) => String(Math.round(v)), step, { lvl: ai.state === 'stalled' ? 'crit' : '', empty: 'No AI checks yet' });
    }
    if (g) {
      html += box('gpu', 'GPU (whole card)', fmtPct(g.util).replace('%', ''), '%', get('gpu'), null, fmtPct, step, { top: 100, cur: g.util, tip: (g.adapters || []).map(a => a.name).join(', '), legend: esc(((g.adapters || [])[0] || {}).name || '') });
    }
    if (net) {
      html += box('net', 'Network', fmtBits(net.rx_bps), '', get('rx'), get('tx'), fmtBits, step, { cur: net.rx_bps, legend: '<span class="pf-legend"><i></i>in <i class="b"></i>out</span>', tip: 'Received (amber) and sent (grey) by this computer' });
    }
    // Disk writes per folder.
    const w = long.writes || {};
    const folders = (s.storage || []);
    for (const f of folders) {
      const series = useLong ? (w[f.name] || []) : [];
      // ("(stored)" only when the name does not say so already.)
      const tier = f.tier === 'stored' && !/stored/i.test(f.name) ? ' (stored)' : '';
      html += box('w_' + f.name, 'Writes: ' + f.name + tier, fmtRate(f.write_bps || 0), '', series, null, fmtRate, step,
        { cur: useLong ? f.write_bps || 0 : undefined, tip: 'Writes: ' + f.name + tier + (f.path ? ' (' + f.path + ')' : ''), legend: f.free != null ? fmtSize(f.free) + ' free' : '', empty: 'Collecting (needs two minutes)' });
    }
    html += '</div>';

    // Cameras.
    const cams = (s.cameras || []).slice();
    const val = (c) => ({
      name: c.name.toLowerCase(), state: !c.enabled ? 0 : c.sleeping ? 1 : c.online ? 3 : 2, fps: c.fps_in, kbps: c.kbps, drop: c.dropped_1m, rc: c.reconnects,
    }[pfSort.k]);
    cams.sort((a, b) => { const x = val(a), y = val(b); return (x < y ? -1 : x > y ? 1 : 0) * pfSort.dir; });
    const th = (k, label, num) => '<th' + (num ? ' class="num"' : '') + ' data-sort="' + k + '" tabindex="0"' + (pfSort.k === k ? ' aria-sort="' + (pfSort.dir > 0 ? 'ascending' : 'descending') + '"' : '') + '>' + label + '</th>';
    const heat = (lvl) => lvl ? ' class="num pf-' + lvl + '"' : ' class="num"';
    html += '<h4 class="asys-sub">Cameras</h4>' + wrap('<table class="asys-tbl pf-tbl"><thead><tr>' + th('name', 'Camera') + th('state', 'State') + th('fps', 'Frames in / analysed', true) + th('kbps', 'Bitrate', true) + th('drop', 'Dropped / min', true) + th('rc', 'Reconnects', true) + '</tr></thead><tbody>' +
      cams.map(c => {
        let st;
        if (!c.enabled) st = '<span class="pf-chip">Disabled</span>';
        else if (c.sleeping) st = '<span class="pf-chip">Asleep</span>';
        else if (c.online) st = '<span class="pf-chip" data-lvl="ok">Online</span>';
        else st = '<span class="pf-chip" data-lvl="crit">Offline</span>';
        const live = c.enabled && c.online && !c.sleeping;
        // Frames arriving should cover the analysis rate (the camera's fps setting).
        let fl = '';
        if (live && c.cfg_fps) fl = c.fps_in < 0.5 * c.cfg_fps ? 'fault' : c.fps_in < 0.8 * c.cfg_fps ? 'warn' : '';
        const dl = live ? (c.dropped_1m >= 30 ? 'fault' : c.dropped_1m > 0 ? 'warn' : '') : '';
        const rl = c.enabled ? (c.reconnects >= 5 ? 'fault' : c.reconnects > 0 ? 'warn' : '') : '';
        const mut = !live ? ' class="pf-mut"' : '';
        return '<tr' + mut + '><td><b>' + esc(c.name) + '</b></td><td>' + st + '</td>' +
          '<td' + heat(fl) + '>' + (live ? c.fps_in + ' <small>/ ' + c.cfg_fps + '</small>' : '-') + '</td>' +
          '<td class="num">' + (live ? kbps(c.kbps) : '-') + '</td>' +
          '<td' + heat(dl) + '>' + (live ? c.dropped_1m : '-') + '</td>' +
          '<td' + heat(rl) + ' title="Times the stream dropped since AmberSight started">' + c.reconnects + '</td></tr>';
      }).join('') + '</tbody></table>') +
      '<p class="hint">Frames in is what the camera sends per second; analysed is the rate the camera is set to look at. Colored cells: frames in well under the analysis rate, frames the decoder had to drop in the last minute, and stream reconnects since AmberSight started.</p>';
    return html;
  }

  // Chart hover: the value at the pointer.
  let pfTip = null;
  body.addEventListener('pointermove', (e) => {
    const sp = e.target.closest('.pf-spark[data-pf]');
    if (!sp) return;
    const sr = pfSeries[sp.dataset.pf];
    if (!sr || sr.a.length < 2) return;
    const r = sp.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
    const i = Math.round(f * (sr.a.length - 1));
    const rule = sp.querySelector('.pf-rule');
    if (rule) rule.style.left = (i / (sr.a.length - 1) * 100) + '%';
    sp.classList.add('on');
    const ago = (sr.a.length - 1 - i) * sr.step;
    const when = ago === 0 ? 'now' : ago >= 120 ? Math.round(ago / 60) + ' min ago' : ago + ' s ago';
    const foot = sp.parentNode.querySelector('.pf-foot span:nth-child(2)');
    if (foot) {
      if (pfTip === null) pfTip = foot.innerHTML;
      foot.innerHTML = '<b>' + esc(sr.fmt(sr.a[i]) + (sr.b ? ' / ' + sr.fmt(sr.b[i]) : '')) + '</b> ' + when;
      foot.dataset.tip = '1';
    }
  });
  body.addEventListener('pointerleave', () => {}, true);
  body.addEventListener('pointerout', (e) => {
    const sp = e.target.closest && e.target.closest('.pf-spark[data-pf]');
    if (!sp || sp.contains(e.relatedTarget)) return;
    sp.classList.remove('on');
    const foot = sp.parentNode.querySelector('.pf-foot span:nth-child(2)');
    if (foot && foot.dataset.tip && pfTip !== null) { foot.innerHTML = pfTip; delete foot.dataset.tip; }
    pfTip = null;
  });
  const pfResort = (th) => {
    const k = th.dataset.sort;
    pfSort = { k, dir: pfSort.k === k ? -pfSort.dir : 1 };
    draw();
  };
  body.addEventListener('keydown', (e) => { const th = e.target.closest && e.target.closest('th[data-sort]'); if (th && (e.key === 'Enter' || e.key === ' ')) { e.preventDefault(); pfResort(th); } });

  // ------------------------------------------------------------ Storage
  function storage() {
    const p = d.plan;
    if (!p) return '<p class="empty">Loading the storage plan...</p>';
    const keep = keepDays != null ? keepDays : p.retention_days;
    const folders = p.folders || [];
    const hasArchive = folders.some(f => f.archive);
    const bar = (x) => {
      const cap = x.capacity_bytes || 0;
      // (An automatic cap of 0: nothing can be spared, shown full.)
      const pct = cap ? Math.min(100, x.used_bytes / cap * 100) : x.auto && x.cap_bytes === 0 ? 100 : 0;
      // A capped folder that hands its oldest recordings to the archive runs full.
      const rolls = hasArchive && !x.archive && x.cap_bytes != null;
      // The server's level: full while the oldest make room is only a note
      // (older servers: by the fill).
      const lvl = x.level !== undefined ? ({ crit: 'bad', warn: 'warn' }[x.level] || '') : rolls ? '' : pct > 95 ? 'bad' : pct > 85 ? 'warn' : '';
      const kind = x.auto ? ' (automatic: keeps ' + fmtSize(x.target_free || 0) + ' of the drive free)' : x.cap_bytes != null ? ' (cap)' : ' (drive)';
      const note = x.label_status ? '; ' + x.label_status.toLowerCase() + (rolls && !/\+ archive$/.test(x.label_status) ? ', older in the archive' : '') : (rolls && pct >= 95 ? '; full, oldest move to the archive' : '');
      return '<div class="asys-bar ' + lvl + '"><i style="width:' + pct.toFixed(1) + '%"></i></div>' +
        '<span class="muted">' + fmtSize(x.used_bytes) + ' of ' + fmtSize(cap) + kind + esc(note) + '</span>';
    };
    let html = '<h4 class="asys-sub">Recording folders</h4>' + wrap('<table class="asys-tbl"><thead><tr><th>Folder</th><th>Path</th><th style="min-width:200px">Used</th><th class="num">Writing</th><th class="num">Fits</th><th class="num">Drive free</th></tr></thead><tbody>' +
      folders.map(x => '<tr><td><b>' + esc(x.label) + '</b>' + (x.archive ? ' <span class="badge info">archive</span>' : '') + '</td><td class="urlcell st-path">' + esc(x.path).replace(/([\\/])/g, '$1<wbr>') + '</td>' +
        '<td>' + bar(x) + '</td><td class="num">' + perDay(x.bytes_per_day) + (x.inflow_per_day ? '<div class="muted">' + perDay(x.inflow_per_day) + ' moved in</div>' : '') + '</td>' +
        '<td class="num">' + days(x.days_fit) + (x.days_total != null && x.days_total > x.days_fit + 0.05 ? '<div class="muted" style="white-space:nowrap">' + days(x.days_total) + ' with archive</div>' : '') + '</td><td class="num">' + (x.drive_free != null ? fmtSize(x.drive_free) : '-') + '</td></tr>').join('') + '</tbody></table>');
    html += '<p class="hint">"Fits" is how many days of recordings each folder holds at the measured rate' + (folders.some(f => f.auto) ? ' (for automatic folders: in what the drive can spare now, so other files on the drive change it)' : '') + '. Recordings are deleted after ' + p.retention_days + (p.retention_days === 1 ? ' day' : ' days') + ' in any case (Settings &rsaquo; Recording &rsaquo; Storage &amp; retention).</p>';

    // Planner.
    html += '<h4 class="asys-sub">Plan</h4><div class="st-plan"><label>Keep <input id="stKeep" type="number" min="1" max="3650" value="' + keep + '"> days</label>';
    // (Whether it fits comes right after the folder, so a phone shows it without scrolling sideways.)
    html += wrap('<table class="asys-tbl st-plantbl"><thead><tr><th>Folder</th><th></th><th class="num">Needs</th><th class="num">Has room for</th></tr></thead><tbody>' +
      folders.filter(x => x.bytes_per_day).map(x => {
        // The archive holds what other folders move in only after the days those folders hold
        // themselves (as the server's "days kept" counts it); its own cameras need every day.
        const need = !x.archive ? x.bytes_per_day * keep
          : (x.bytes_per_day - (x.inflow_per_day || 0)) * keep +
            folders.filter(s => !s.archive && s.cap_bytes != null && s.bytes_per_day).reduce((a, s) => a + s.bytes_per_day * Math.max(0, keep - (s.days_fit || 0)), 0);
        // A folder that hands its oldest to the archive holds what both hold.
        const withArc = x.days_total != null && x.days_total > x.days_fit + 0.05;
        const has = withArc ? x.days_total * x.bytes_per_day : x.capacity_bytes;
        const ok = need <= has + 1;
        return '<tr><td><b>' + esc(x.label) + '</b></td><td>' + (ok ? '<span class="badge on">fits</span>' : '<span class="badge off">short ' + fmtSize(need - has) + '</span>') + '</td>' +
          '<td class="num">' + fmtSize(need) + '</td><td class="num">' + fmtSize(has) + (withArc ? ' <span class="muted" title="This folder and the archive it moves old recordings to">with archive</span>' : '') + (x.auto ? ' <span class="muted" title="Automatic: what the drive can spare right now">now</span>' : '') + '</td></tr>';
      }).join('') + '</tbody></table>');
    const total = folders.reduce((a, x) => a + (x.bytes_per_day || 0) - (x.inflow_per_day || 0), 0);
    html += '<p class="hint">All cameras together record about ' + perDay(total) + ': ' + keep + (keep === 1 ? ' day needs' : ' days need') + ' about <b>' + fmtSize(total * keep) + '</b>.</p>';
    html += '<details class="st-percam"><summary>Per camera</summary>' + wrap('<table class="asys-tbl"><thead><tr><th>Camera</th><th>Folder</th><th class="num">Per day</th><th class="num">' + keep + ' days</th><th class="num">Stored now</th><th class="num">Kept (est.)</th></tr></thead><tbody>' +
      (p.cameras || []).slice().sort((a, b) => b.bytes_per_day - a.bytes_per_day).map(c => '<tr><td><b>' + esc(c.name) + '</b>' + (c.record && c.enabled ? (c.motion_only ? ' <span class="muted">motion only</span>' : '') : ' <span class="muted">not recording</span>') + '</td>' +
        '<td>' + esc(c.folder || 'Data folder') + '</td><td class="num">' + perDay(c.bytes_per_day) + (c.rough ? '*' : '') + '</td><td class="num">' + (c.bytes_per_day ? fmtSize(c.bytes_per_day * keep) : '-') + '</td>' +
        '<td class="num">' + fmtSize(c.stored_bytes || 0) + '</td><td class="num">' + days(c.days_kept) + '</td></tr>').join('') + '</tbody></table>') + '</details></div>';
    html += '<p class="hint">Add a drive or change caps in <a href="#" data-goadmin="retention">Settings &rsaquo; Recording</a>.</p>';
    return html;
  }

  // ------------------------------------------------------------ Connections
  function conns() {
    const c = d.conns;
    if (!c) return '<p class="empty">Loading...</p>';
    let html = '<h4 class="asys-sub">Browsers and apps (last hour)</h4>';
    html += (c.clients || []).length ? wrap('<table class="asys-tbl"><thead><tr><th>User</th><th>Address</th><th>Using</th><th>Watching</th><th>Last seen</th><th class="num">Requests</th></tr></thead><tbody>' +
      c.clients.map(x => '<tr><td><b>' + esc(x.user) + '</b></td><td>' + esc(x.ip) + '</td><td>' + esc(x.agent) + '</td><td>' + esc(x.watching || '') + '</td>' +
        '<td>' + (x.active ? '<span class="badge on dot">now</span>' : ago(x.last)) + '</td><td class="num">' + x.requests + '</td></tr>').join('') + '</tbody></table>')
      : '<p class="empty">Nobody else in the last hour.</p>';
    html += '<h4 class="asys-sub">Computers sending their screen</h4>';
    html += (c.helpers || []).length ? wrap('<table class="asys-tbl"><thead><tr><th>Computer</th><th>State</th><th>Connected for</th><th>Version</th></tr></thead><tbody>' +
      c.helpers.map(x => '<tr><td><b>' + esc(x.name) + '</b></td><td>' + (x.online ? '<span class="badge on dot">online</span>' : '<span class="badge off dot">offline</span>') + '</td>' +
        '<td>' + (x.online && x.connected_s != null ? dur(x.connected_s) : '-') + '</td><td>' + esc(x.version || '-') + '</td></tr>').join('') + '</tbody></table>')
      : '<p class="empty">None paired.</p>';
    html += '<h4 class="asys-sub">Pushed streams</h4>';
    html += (c.pushes || []).length ? wrap('<table class="asys-tbl"><thead><tr><th>Camera</th><th>Sender</th><th>State</th></tr></thead><tbody>' +
      c.pushes.map(x => '<tr><td><b>' + esc(x.camera) + '</b></td><td>' + esc(x.from || (x.helper ? 'capture helper' : '-')) + '</td>' +
        '<td>' + (x.seconds != null ? '<span class="badge on dot">receiving</span> <span class="muted">for ' + dur(x.seconds) + '</span>' : '<span class="badge off dot">idle</span>') + '</td></tr>').join('') + '</tbody></table>')
      : '<p class="empty">No cameras take a pushed stream.</p>';
    return html;
  }

  // ------------------------------------------------------------ events
  tablistKeys(el.querySelector('.st-tabs'));
  el.querySelector('.st-tabs').addEventListener('click', (e) => {
    const b = e.target.closest('button[data-t]'); if (!b) return;
    tab = b.dataset.t; saveTab(tab);
    draw(); load();
  });
  body.addEventListener('click', async (e) => {
    const sth = e.target.closest('th[data-sort]');
    if (sth) { pfResort(sth); return; }
    const nz = e.target.closest('[data-nz]');
    if (nz) { noisyAct(nz.dataset.nz, nz.dataset.nzc); return; }
    const go = e.target.closest('[data-go]');
    if (go) { if (go.dataset.go === 'log') { openAdminPage('logs'); return; } tab = go.dataset.go; saveTab(tab); draw(); load(); return; }
    const ga = e.target.closest('[data-goadmin]');
    if (ga) { e.preventDefault(); openAdminPage(ga.dataset.goadmin); return; }
    if (e.target.closest('[data-updinstall]')) { confirmUpdate(); return; }
    if (e.target.closest('[data-updcheck]')) {
      checking = true; draw();
      try { gotUpdate(await api('/api/update/check', { json: {} })); } catch (err) { toast('Could not check for updates: ' + errText(err)); }
      checking = false; draw();
      return;
    }
    if (e.target.closest('[data-syncall]')) {
      syncing = true; draw();
      try {
        const r = await api('/api/timesync/all', { json: {} });
        const bad = (r.results || []).filter(x => !x.ok);
        const good = (r.results || []).length - bad.length;
        const off = (r.results || []).filter(x => x.ok && x.offset_after_s != null && Math.abs(x.offset_after_s) > 10);
        toast('Clocks synced on ' + good + ' camera' + (good === 1 ? '' : 's') +
          (off.length ? '; still off: ' + off.map(x => x.camera + ' (' + (x.offset_after_s > 0 ? '+' : '') + x.offset_after_s + ' s)').join(', ') : '') +
          (bad.length ? '; not reached: ' + bad.map(x => x.camera).join(', ') + ' (no ONVIF time settings, or a different login)' : '') + '.');
      } catch (err) { toast('Could not sync clocks: ' + errText(err)); }
      syncing = false;
      load();
      return;
    }
    const b = e.target.closest('[data-timesync]');
    if (!b) return;
    const id = Number(b.dataset.timesync);
    fixing.add(id);
    b.disabled = true; b.textContent = 'Turning on...';
    try {
      const r = await api('/api/cameras/' + id + '/timesync', { json: {} });
      let msg = 'Time sync is on';
      if (r.ntp_server_set) msg += ' (server ' + r.ntp_server_set + ')';
      if (r.offset_after_s != null) msg += '; camera clock now ' + (Math.abs(r.offset_after_s) <= 2 ? 'matches this computer' : (r.offset_after_s > 0 ? '+' : '') + r.offset_after_s + ' s off');
      toast(msg + '. The warning clears once the camera reports its new time.');
    } catch (err) {
      toast('Could not turn on time sync: ' + errText(err));
    }
    fixing.delete(id);
    load();
  });
  body.addEventListener('input', (e) => {
    if (e.target.id !== 'stKeep') return;
    const v = parseInt(e.target.value, 10);
    if (!(v > 0)) return;
    keepDays = Math.min(3650, v);
    const pos = e.target.selectionStart;
    body.innerHTML = storage();
    const i = body.querySelector('#stKeep'); i.focus(); try { i.setSelectionRange(pos, pos); } catch (err) { /* number inputs */ }
  });
  el.querySelector('#stNow').onclick = () => load(true);

  draw();
  load();
  statusLeave();
  timer = setInterval(() => { if (!document.hidden && tab !== 'storage') load(); }, 4000);
}
function statusLeave() { clearInterval(timer); timer = null; }

registerAdminPage({ id: 'status', title: 'Status', order: 5, render: statusPage, leave: statusLeave });
