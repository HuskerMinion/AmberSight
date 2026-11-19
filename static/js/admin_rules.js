// admin_rules.js - Settings > Detection & alerts > Alert rules: rules written in plain words ("a delivery truck
// stopped out front"). The words are compared with the pictures of every new event (smart search's
// model); a match tags the event and sends the rule's alert. "Quiet" rules silence alerts instead.
// The test shows how recent events would have scored, so the threshold can be set from real pictures.

import { t } from './i18n.js';
import { api, esc, toast, openDlg, state, enc, errText, liveSwitch, swHtml, fld, confirmDialog, savedToast } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, how, notice } from './sp_kit.js';

const when = (ms) => new Date(ms).toLocaleString(undefined, { weekday: 'short', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
// Scores are shown as points (the raw number times 100): 0 is "no more like the words than anything else".
const pts = (s) => (s * 100).toFixed(1);
const ACTIONS = [['alert_high', t('admin_rules.action_alert_high','Alert, high priority', {})], ['alert', t('admin_rules.action_alert','Alert', {})], ['notice', t('admin_rules.action_notice','Notice (low priority, quiet)', {})], ['tag', t('admin_rules.action_tag','Only tag the event', {})]];
const sw = swHtml;

function summary(r) {
  const cams = r.cameras.length ? r.cameras.join(', ') : t('admin_rules.all_cameras','all cameras', {});
  const what = r.kind === 'quiet' ? t('admin_rules.silences_alerts','silences alerts', {}) : (ACTIONS.find(a => a[0] === r.action) || [0, r.action])[1].toLowerCase();
  return esc(cams) + ' · ' + what + (r.schedule ? ' · ' + esc(r.schedule) : '') + ' · ' + t('admin_rules.match_at_least_n', 'match at least {pts}', { pts: pts(r.threshold) });
}

function rulesPage(el) {
  let data = null;
  async function load() {
    try { data = await api('/api/alert-rules'); } catch (e) { el.innerHTML = '<p class="empty">' + esc(errText(e)) + '</p>'; return; }
    paint();
  }
  function paint() {
    const off = !data.smart_enabled;
    el.innerHTML = '<div class="ac">' +
      lead(t('admin_rules.lead_desc','Get an alert when something you describe in words appears, like &quot;a delivery truck stopped out front&quot;.', {}), t('admin_rules.lead_help','Start with one rule, then use its test to check it against recent events.', {})) +
      (off ? notice('warn', t('admin_rules.rules_warn','Rules only work while <b>smart search</b> is on, and it is off. You can write rules now; they start working once it is on.', {}), '<button class="primary" type="button" data-smarton>' + t('admin_rules.turn_on_smart', 'Turn on smart search') + '</button>')
        : !data.smart_loaded ? '<div class="hint">' + t('admin_rules.smart_startup', 'Smart search is starting up; rules begin to work when it is ready.') + '</div>' : '') +
      how(t('admin_rules.how', 'Every new event\'s pictures are compared with the words (the smart search model, on this server\'s processor); when one matches well enough the event is tagged &quot;rule: name&quot; and the rule\'s alert is sent with the rule\'s name: &quot;Delivery truck out front at Front Door&quot;. A <b>quiet rule</b> works the other way round: &quot;my own car in the driveway&quot; stops alerts for events that match it. Matching words to pictures is approximate: it can miss, and it can match something that only looks a bit like the words. Use the test to see how recent events scored and set the threshold between the ones you want and the ones you do not. Rules look at events as they happen (not at old ones), and the rule alert comes a few seconds after the ordinary one, once the pictures are checked.')) +
      '<div class="row"><button class="btn primary" data-add>' + t('admin_rules.add_a_rule', 'Add a rule') + '</button></div>' +
      (data.rules.length ? data.rules.map((r, i) =>
        '<div class="ac-card ar-rule"><div class="ar-head">' + sw('data-on="' + i + '"', r.enabled, esc(r.name) + ' <span class="ar-kind' + (r.kind === 'quiet' ? ' quiet' : '') + '">' + (r.kind === 'quiet' ? t('admin_rules.quiet_rule','quiet rule', {}) : t('admin_rules.alert_rule','alert rule', {})) + '</span>', '&ldquo;' + esc(r.text) + '&rdquo;') +
        '<button class="btn" data-edit="' + i + '">' + t('admin_rules.edit', 'Edit') + '</button></div><div class="ar-meta">' + summary(r) + '</div></div>').join('')
        : '<p class="empty">' + t('admin_rules.no_rules', 'No rules yet.') + '</p>') + '</div>';
    el.querySelector('[data-add]').onclick = () => editRule(null);
    const so = el.querySelector('[data-smarton]');
    if (so) so.onclick = async () => {
      so.disabled = true;
      try {
        const r = await api('/api/settings', { json: { smart_search: { enabled: true } } });
        if (r && r.error) throw new Error(r.error);
        savedToast(t('admin_rules.smart_on','Smart search is on', {}));
        await load();
      } catch (e) { toast(errText(e), { kind: 'err' }); so.disabled = false; }
    };
    el.querySelectorAll('[data-edit]').forEach(b => { b.onclick = () => editRule(data.rules[+b.dataset.edit]); });
    el.querySelectorAll('[data-on]').forEach(c => liveSwitch(c, async (on) => {
      await api('/api/alert-rules', { json: Object.assign({}, data.rules[+c.dataset.on], { enabled: on }) });
      data.rules[+c.dataset.on].enabled = on;
    }));
  }

  function editRule(r0) {
    const isNew = !r0;
    const r = Object.assign({ id: '', name: '', text: '', kind: 'alert', cameras: [], threshold: data.default_threshold, action: 'alert', schedule: null, enabled: true, cooldown_min: 5, hold_secs: 30 }, r0 || {});
    const cams = state.cams.filter(c => !c.remote).map(c => c.name);
    const [t0, t1] = (r.schedule || '').split('-');
    const d = openDlg(isNew ? t('admin_rules.add_alert_rule','Add an alert rule', {}) : t('admin_rules.alert_rule_title','Alert rule', {}),
      '<div class="ar-form">' +
      '<label>' + t('admin_rules.name', 'Name') + '<input data-name maxlength="60" value="' + esc(r.name) + '" placeholder="' + t('admin_rules.placeholder_name','Delivery truck out front', {}) + '"></label>' +
      '<label>' + t('admin_rules.what_to_look_for', 'What to look for, in words') + '<input data-text maxlength="200" value="' + esc(r.text) + '" placeholder="' + t('admin_rules.placeholder_text','a delivery truck stopped in front of the house', {}) + '"></label>' +
      '<div class="ar-two"><label>' + t('admin_rules.this_rule', 'This rule') + '<select data-kind><option value="alert">' + t('admin_rules.opt_sends_alert','sends an alert', {}) + '</option><option value="quiet">' + t('admin_rules.opt_silences_alerts','silences alerts (quiet rule)', {}) + '</option></select></label>' +
      '<label data-actl>' + t('admin_rules.action', 'Action') + '<select data-action>' + ACTIONS.map(a => '<option value="' + a[0] + '"' + (a[0] === r.action ? ' selected' : '') + '>' + a[1] + '</option>').join('') + '</select></label></div>' +
      '<div class="hint" data-kindhint></div>' +
      '<div><b>' + t('admin_rules.cameras','Cameras', {}) + '</b><div class="ar-cams"><label><input type="checkbox" data-allcams' + (r.cameras.length ? '' : ' checked') + '> ' + t('admin_rules.all_cameras_cap', 'All cameras') + '</label>' +
      cams.map(c => '<label><input type="checkbox" data-cam value="' + esc(c) + '"' + (r.cameras.some(x => x.toLowerCase() === c.toLowerCase()) ? ' checked' : '') + '> ' + esc(c) + '</label>').join('') + '</div></div>' +
      '<div class="ar-two"><label class="sw-line"><span><input type="checkbox" data-sched' + (r.schedule ? ' checked' : '') + '> ' + t('admin_rules.only_between','Only between', {}) + '</span><span><input type="time" data-t0 value="' + esc(t0 || '18:00') + '"> ' + t('admin_rules.and', 'and') + ' <input type="time" data-t1 value="' + esc(t1 || '23:59') + '"></span></label>' +
      '<span data-cdl>' + fld(t('admin_rules.label_cooldown','Alert again after at least', {}), 'data-cool min="0" max="1440" value="' + r.cooldown_min + '"', t('admin_rules.minutes', 'minutes')) + '</span>' +
      '<span data-holdl>' + fld(t('admin_rules.label_hold','Wait for the check up to', {}), 'data-hold min="5" max="90" value="' + r.hold_secs + '"', t('admin_rules.seconds', 'seconds')) + '</span></div>' +
      '<label>' + t('admin_rules.match_at_least_label','Match at least', {}) + '  <b data-thv></b> ' + t('admin_rules.points', 'points') + '<input type="range" data-th min="-2" max="15" step="0.5"></label>' +
      '<div class="hint">' + t('admin_rules.hint_match', 'Higher is stricter (fewer, surer matches). Around 3 usually means &quot;clearly looks like it&quot;; 0 is no more like the words than any picture. Set it with the test below.') + '</div>' +
      '<div class="ar-prev"><div class="row"><button class="btn" data-test>' + t('admin_rules.test_against_recent','Test against recent events', {}) + '</button> <select data-hours><option value="24">' + t('admin_rules.last_day','last day', {}) + '</option><option value="72" selected>' + t('admin_rules.last_3_days','last 3 days', {}) + '</option><option value="168">' + t('admin_rules.last_week','last week', {}) + '</option><option value="720">' + t('admin_rules.last_30_days','last 30 days', {}) + '</option></select></div>' +
      '<div data-pnote class="hint"></div><div class="ar-prev-list" data-plist></div></div>' +
      '<div class="row"><button class="btn primary" data-save>' + t('admin_rules.save','Save', {}) + '</button>' + (isNew ? '' : '<button class="btn danger" data-del>' + t('admin_rules.delete','Delete', {}) + '</button>') + '<button class="btn" data-cancel>' + t('admin_rules.cancel','Cancel', {}) + '</button></div></div>');
    d.style.width = 'min(760px, 96vw)';
    const q = (s) => d.querySelector(s);
    q('[data-kind]').value = r.kind;
    const th = q('[data-th]');
    th.value = Math.max(-2, Math.min(15, Math.round(r.threshold * 200) / 2));
    let items = null;
    const paintPreview = () => {
      q('[data-thv]').textContent = pts(th.value / 100);
      if (!items) return;
      const thr = th.value / 100, on = items.filter(i => i.score >= thr).length;
      q('[data-pnote]').textContent = t('admin_rules.preview_note', '{on} of the {total} best-scoring events would match at {pts} or more{all}. Green ones match.', { on, total: items.length, pts: pts(thr), all: items.length < data.scanned ? t('admin_rules.preview_all', ' ({scanned} events in all; the median scored {median})', { scanned: data.scanned, median: pts(data.median || 0) }) : '' });
      q('[data-plist]').innerHTML = items.map(i => {
        const b = i.box;
        return '<div class="ar-pcard' + (i.score >= thr ? ' on' : '') + '"><span class="fw-pic"><img loading="lazy" src="/api/smart/pic/' + i.pic + '" alt="">' +
          (b ? '<span class="smq-box" style="left:' + (b[0] * 100) + '%;top:' + (b[1] * 100) + '%;width:' + (b[2] * 100) + '%;height:' + (b[3] * 100) + '%"></span>' : '') + '</span>' +
          '<small><b>' + pts(i.score) + '</b> · ' + esc(i.camera) + '<br>' + esc(when(i.at_ms)) + '</small></div>';
      }).join('');
    };
    const kindUi = () => {
      const quiet = q('[data-kind]').value === 'quiet';
      q('[data-actl]').hidden = quiet; q('[data-cdl]').hidden = quiet; q('[data-holdl]').hidden = !quiet;
      q('[data-kindhint]').textContent = quiet
        ? t('admin_rules.kindhint_quiet','An event that matches is tagged "quiet: name" and its alerts are not sent: not the ordinary alert and not another rule\'s alert. So that this can work, ordinary alerts of the cameras chosen below wait up to the seconds set here for the event\'s pictures to be checked (other cameras\' alerts are never delayed); if they are not checked in time the alert goes out as usual. Events are still recorded, listed and sent over MQTT.', {})
        : t('admin_rules.kindhint_alert','An event that matches is tagged "rule: name" and the alert goes out under the rule\'s name. The ordinary alert for the same event is not replaced: it comes first, this one follows a few seconds later.', {});
    };
    q('[data-kind]').onchange = kindUi;
    th.oninput = paintPreview;
    const allc = q('[data-allcams]');
    allc.onchange = () => { if (allc.checked) d.querySelectorAll('[data-cam]').forEach(c => { c.checked = false; }); };
    d.querySelectorAll('[data-cam]').forEach(c => { c.onchange = () => { if (c.checked) allc.checked = false; else if (![...d.querySelectorAll('[data-cam]')].some(x => x.checked)) allc.checked = true; }; });
    kindUi(); paintPreview();
    const chosen = () => allc.checked ? [] : [...d.querySelectorAll('[data-cam]')].filter(c => c.checked).map(c => c.value);
    q('[data-test]').onclick = async (e) => {
      const text = q('[data-text]').value.trim();
      if (!text) { q('[data-pnote]').textContent = t('admin_rules.write_first','Write what to look for first.', {}); return; }
      e.target.disabled = true; q('[data-pnote]').textContent = t('admin_rules.looking','Looking at recent events...', {});
      try {
        const p = await api('/api/alert-rules/preview', { json: { text, cameras: chosen(), hours: +q('[data-hours]').value } });
        data.scanned = p.scanned; data.median = p.median; items = p.items;
        if (!items.length) q('[data-pnote]').textContent = t('admin_rules.no_events','No searched events in that time', {}) + (chosen().length ? t('admin_rules.on_those_cameras', ' on those cameras') : '') + '.';
        else paintPreview();
      } catch (err) { q('[data-pnote]').textContent = errText(err); }
      e.target.disabled = false;
    };
    q('[data-cancel]').onclick = () => d.__close();
    const del = q('[data-del]');
    if (del) del.onclick = async () => {
      if (!(await confirmDialog({ title: t('admin_rules.delete_rule_title', 'Delete rule {name}?', { name: r.name }), body: t('admin_rules.delete_rule_body', 'It stops tagging events and sending its alert.'), ok: t('admin_rules.delete_rule_ok', 'Delete rule'), danger: true }))) return;
      try { await api('/api/alert-rules/' + enc(r.id), { method: 'DELETE' }); d.__close(); toast(t('admin_rules.rule_deleted','Rule deleted', {})); load(); } catch (err) { toast(errText(err)); }
    };
    q('[data-save]').onclick = async (e) => {
      const rule = Object.assign({}, r, {
        name: q('[data-name]').value.trim(), text: q('[data-text]').value.trim(), kind: q('[data-kind]').value, action: q('[data-action]').value,
        cameras: chosen(), threshold: th.value / 100, schedule: q('[data-sched]').checked ? q('[data-t0]').value + '-' + q('[data-t1]').value : null,
        cooldown_min: Math.max(0, parseInt(q('[data-cool]').value, 10) || 0), hold_secs: Math.max(5, parseInt(q('[data-hold]').value, 10) || 30),
      });
      e.target.disabled = true;
      try { await api('/api/alert-rules', { json: rule }); d.__close(); toast(t('admin_rules.rule_saved','Rule saved', {})); load(); } catch (err) { toast(errText(err)); e.target.disabled = false; }
    };
    q('[data-name]').focus();
  }
  el.innerHTML = '<div class="ac"><p class="empty">' + t('admin_rules.loading', 'Loading...') + '</p></div>';
  load();
  return () => {};
}

registerAdminPage({ id: 'alertrules', title: t('admin_rules.alert_rules', 'Alert rules'), order: 64, render(el) { rulesPage(el); } });
