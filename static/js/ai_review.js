// ai_review.js - AI review: "why did (or didn't) this event alert?" in plain
// words, the frames the AI looked at with what it saw drawn on them, and
// one-click changes. Also the Settings > Smart features > AI review page (recent events by verdict).
import { t } from './i18n.js';
import { api, esc, toast, openDlg, fmtTime, state, ic, stillWaiting, can, objName } from './core.js';

// One-click fixes change the camera's AI settings: admins, and operators who may change
// detection settings of a camera they can see (not one of another server).
const mayFix = (r) => {
  const c = r.camera && r.camera.id != null ? state.cams.find(x => x.id === r.camera.id) : null;
  return state.isAdmin || (can('camera_detection') && !!c && !c.remote);
};
import { registerAdminPage } from './admin.js';

const pct = (s) => Math.round(s * 100) + '%';
const errText = (e) => (e && e.body) ? e.body : (e && e.message) || String(e);
const secs = (ms) => (ms / 1000).toFixed(1) + ' s';
// "a, b or c" (any of them) and "a, b and c" (all of them).
const list = (a, word = t('ai_review.or', 'or')) => a.length <= 1 ? (a[0] || '') : a.slice(0, -1).join(', ') + ' ' + word + ' ' + a[a.length - 1];
const listAnd = (a) => list(a, t('ai_review.and', 'and'));
const an = (w) => (/^[aeiou]/i.test(w) ? t('ai_review.an', 'an {w}', { w: objName(w) }) : t('ai_review.a', 'a {w}', { w: objName(w) }));
// A thing with its article for messages written as "{vowel, select, yes {an {label}} other {a {label}}}".
// (The label is shown by its translated name; the article follows the English word.)
const art = (w, html = true) => { const n = objName(w); return { label: html ? esc(n) : n, vowel: /^[aeiou]/i.test(w) ? 'yes' : 'no' }; };

// Short verdict words for lists.
export const KIND = {
  alerted: [t('ai_review.alerted_label', 'Alert sent'), 'on'],
  near_miss: [t('ai_review.near_miss_label', 'Almost'), 'warnb'],
  strict: [t('ai_review.strict_label', 'Not confirmed'), 'warnb'],
  quiet_person: [t('ai_review.quiet_person_label', 'Known person'), 'info'],
  known_only: [t('ai_review.known_only_label', 'Not a stranger'), 'info'],
  quiet_rule: [t('ai_review.quiet_rule_label', 'Quiet rule'), 'info'],
  notify_area: [t('ai_review.notify_area_label', 'Quiet area'), 'info'],
  notify_objects: [t('ai_review.notify_objects_label', 'Not notified'), 'info'],
  other_object: [t('ai_review.other_object_label', 'Other object'), 'info'],
  nothing: [t('ai_review.nothing_label', 'Nothing found'), ''],
  alerts_off: [t('ai_review.alerts_off_label', 'Alerts off'), ''],
  ai_failed: [t('ai_review.ai_failed_label', 'AI failed'), 'off'],
  parked: [t('ai_review.parked_label', 'Parked'), ''],
  fixed_spot: [t('ai_review.fixed_spot_label', 'Same spot'), ''],
  camera_only: [t('ai_review.camera_only_label', 'Camera only'), 'info'],
};

// What a thing of this kind is called in plain words ("a person-shaped object").
const kindWords = (label) => label === 'person' ? t('ai_review.person_shaped', 'person-shaped object') : ANIMALS.includes(label) ? t('ai_review.animal_shaped', 'animal-shaped object') : label;
// A vehicle that has not moved is parked; anything else is still.
const parkedWord = (label) => VEHICLES.includes(label) ? t('ai_review.parked_word', 'parked') : t('ai_review.not_moving_word', 'not moving');
const VEHICLES = ['car', 'truck', 'bus', 'motorcycle', 'vehicle', 'van', 'suv', 'bicycle', 'boat', 'train', 'airplane'];
const ANIMALS = ['animal', 'dog', 'cat', 'bird', 'horse', 'sheep', 'cow', 'bear', 'elephant', 'zebra', 'giraffe'];
const kindOf = (label) => label === 'person' ? 'person' : VEHICLES.includes(label) ? 'vehicle' : ANIMALS.includes(label) ? 'animal' : label;
const notA = (label) => t('ai_review.not_a', 'Not {vowel, select, yes {an {label}} other {a {label}}}', art(kindOf(label) === 'vehicle' ? label : kindOf(label), false));

// The kinds of object that counted in an event (one per kind, its surest label), for "Not a ..." marks.
function countedKinds(v) {
  const min = v.min_score, wanted = v.wanted || [], best = new Map();
  for (const c of v.checks || []) (c.seen || []).forEach((d, i) => {
    if (!wanted.includes(d.label) || d.score < min || !(d.w > 0) || (c.parked && c.parked[i])) return;
    const k = kindOf(d.label), b = best.get(k);
    if (!b || d.score > b.score) best.set(k, d);
  });
  return [...best.values()].map(d => d.label);
}

// The plain-language explanation: { head, lines[], fixes[] }.
function explain(r) {
  const cam0 = r.camera || {};
  const path = t('ai_review.settings_path', 'Settings \u203a Detection &amp; alerts \u203a Objects (AI)');
  const where = state.isAdmin ? '<a href="#settings/detection/objects">' + path + '</a>' : path;
  const camName = esc(cam0.name || t('ai_review.this_camera', 'this camera'));
  const v = r.review, s = r.summary, cam = r.camera || {};
  const out = { head: '', lines: [], fixes: [] };
  if (!v) {
    if (!r.ai.enabled) out.head = t('ai_review.ai_off_head', 'AI did not check this event: AI is turned off. Turn it on in {where}.', { where });
    else if (!r.ai.model && !r.ai.server) out.head = t('ai_review.ai_no_model_head', 'AI did not check this event: no AI model is installed. Choose or download one in {where}.', { where });
    else if (cam.ai === false) {
      out.head = t('ai_review.ai_cam_off_head', 'AI did not check this event: AI is turned off for {camera}.', { camera: esc(cam.name) });
      out.lines.push(t('ai_review.ai_cam_off_line', "Turn it on in the camera's settings, AI tab."));
    } else out.head = t('ai_review.no_ai_record_head', 'There is no AI record for this event. It may be from before AI review was added, or motion ended before the first check.');
    return out;
  }
  const min = v.min_score, wanted = v.wanted || [];
  const checks = v.checks || [];
  const span = checks.length ? secs(Math.max(0, checks[checks.length - 1].offset_ms)) : '';
  const firstHit = checks.find(c => (c.seen || []).some(d => wanted.includes(d.label) && d.score >= min));
  const hit = firstHit && firstHit.seen.filter(d => wanted.includes(d.label) && d.score >= min).sort((a, b) => b.score - a.score)[0];
  const bw = s.best_wanted, bo = s.best_other;
  const looked = checks.length ? t('ai_review.looked', 'AI looked at {n, plural, one {# frame} other {# frames}}{over, select, yes { over {span}} other {}}', { n: checks.length, over: checks.length > 1 ? 'yes' : 'no', span }) : '';
  switch (s.kind) {
    case 'alerted':
      if (hit) out.head = t('ai_review.alerted_head', 'Alert sent: AI saw {vowel, select, yes {an {label}} other {a {label}}} ({score} sure) {fast, select, yes {as soon as it looked.} other {{time} into the event.}}', { ...art(hit.label), score: pct(hit.score), fast: firstHit.offset_ms < 500 ? 'yes' : 'no', time: secs(firstHit.offset_ms) });
      else out.head = t('ai_review.alerted_head_cam_report', 'Alert sent: the camera itself reported {labels}.', { labels: esc(listAnd((v.camera_labels || []).map(objName))) });
      if (checks.length) out.lines.push(t('ai_review.alerted_line', '{looked} and stopped once it was sure.', { looked }));
      break;
    case 'near_miss':
      out.head = t('ai_review.near_miss_head', 'No alert: AI thought it might be {vowel, select, yes {an {label}} other {a {label}}}, but only {score} sure. {camera} needs {min}.', { ...art(bw.label), score: pct(bw.score), camera: esc(cam.name || t('ai_review.this_camera_cap', 'This camera')), min: pct(min) });
      out.lines.push(looked + '.');
      out.lines.push(t('ai_review.near_miss_advice', 'If that was real, lowering the minimum would have sent this alert. Lower it a little at a time: too low and shadows or plants start to count.'));
      out.fixes.push({ kind: 'min', value: Math.max(0.25, Math.floor(bw.score * 20) / 20), label: t('ai_review.fix_lower_min', "Lower {camera}'s minimum to {value}", { camera: camName, value: pct(Math.max(0.25, Math.floor(bw.score * 20) / 20)) }) });
      break;
    case 'quiet_person':
      out.head = t('ai_review.quiet_person_head', 'No alert: the only person seen was recognized as someone marked "no alerts" in Settings \u203a Smart features \u203a Faces.');
      out.lines.push(looked + '.');
      out.lines.push(t('ai_review.quiet_person_advice', 'To get alerts for them again, turn that off for the person in Settings \u203a Smart features \u203a Faces.'));
      break;
    case 'known_only':
      out.head = t('ai_review.known_only_head', 'No alert: this camera alerts only for strangers, and everyone seen was known (or, with \u201cfirst time only\u201d, had been seen before).');
      out.lines.push(looked + '.');
      out.lines.push(t('ai_review.known_only_advice', "Change this in the camera's Alerts settings, under Strangers."));
      break;
    case 'quiet_rule':
      out.head = t('ai_review.quiet_rule_head', 'No alert: a quiet alert rule matched this event\'s pictures.');
      out.lines.push(looked + '.');
      out.lines.push(t('ai_review.quiet_rule_advice', 'Quiet rules are in Settings \u203a Alert rules.'));
      break;
    case 'notify_area':
      out.head = t('ai_review.notify_area_head', 'No notification: everything AI saw stood inside an area of {camera} that is set not to notify.', { camera: camName });
      out.lines.push(looked + '.');
      out.lines.push(t('ai_review.notify_area_advice', "It is still detected, recorded and searchable, and shows here and in Events. Change the areas in the camera's Alerts settings, under \u201cAreas that don't notify\u201d."));
      break;
    case 'notify_objects':
      out.head = t('ai_review.notify_objects_head', 'No notification: {camera} only notifies for certain objects, and this was not one of them.', { camera: camName });
      out.lines.push(looked + '.');
      out.lines.push(t('ai_review.notify_objects_advice', "It is still detected, recorded and searchable. Change this in the camera's Alerts settings, under \u201cNotify me for\u201d."));
      break;
    case 'strict':
      out.head = t('ai_review.strict_head', 'No alert: AI saw {vowel, select, yes {an {label}} other {a {label}}} ({score} sure), but stricter confirmation is on and it was not seen moving in 2 of 3 looks in a row.', { ...art(bw.label), score: pct(bw.score) });
      out.lines.push(looked + '.');
      out.lines.push(t('ai_review.strict_advice', 'This stops a single wrong guess, or something that never moves (a statue, a bush), from alerting. Someone who walks through quickly can be seen only once, though.'));
      out.fixes.push({ kind: 'strict', value: false, label: t('ai_review.fix_strict_off', 'Turn stricter confirmation off for {camera}', { camera: camName }) });
      break;
    case 'other_object':
      out.head = t('ai_review.other_object_head', 'No alert: AI saw {vowel, select, yes {an {label}} other {a {label}}} ({score}), but {camera} only alerts for {wanted}.', { ...art(bo.label), score: pct(bo.score), camera: camName, wanted: esc(list(wanted.map(objName))) });
      out.lines.push(looked + '.');
      out.fixes.push({ kind: 'label', value: bo.label, label: t('ai_review.fix_also_alert', 'Also alert for {label} on {camera}', { label: esc(objName(bo.label)), camera: camName }) });
      break;
    case 'nothing':
      out.head = t('ai_review.nothing_head', 'No alert: AI did not recognize anything it alerts for.');
      out.lines.push(t('ai_review.nothing_line', '{looked} and found {found}.', { looked, found: bo ? t('ai_review.nothing_faint', 'only a faint guess ({label}, {score})', { label: esc(objName(bo.label)), score: pct(bo.score) }) : t('ai_review.nothing_line_no_objects', 'no objects') }));
      out.lines.push(t('ai_review.nothing_line_explain', 'Usually this is shadows, sunlight, rain, insects, or plants moving: the motion was real, but nothing worth an alert.'));
      out.lines.push(t('ai_review.nothing_line_advice', 'If something real was there, it may have been too small or too far away. The Small model (Settings › Detection &amp; alerts › Objects (AI)) catches more of those; a motion zone can keep detection to the area that matters.'));
      break;
    case 'parked': {
      const bp = s.best_parked;
      out.head = t('ai_review.parked_head', 'No alert: AI saw {vowel, select, yes {an {label}} other {a {label}}} ({score}), but it has been in the same place for minutes ({word}), so it does not count.', { ...art(bp.label), score: pct(bp.score), word: parkedWord(bp.label) });
      out.lines.push(looked + '.');
      out.lines.push(t('ai_review.parked_line', 'This keeps a parked car (or someone sitting still) from confirming every motion event. One that arrives or leaves still counts.'));
      out.lines.push(t('ai_review.turn_off_ignore', 'To count objects even when they have not moved, turn off "Ignore objects that have not moved" in this camera\'s AI settings or in {where}.', { where }));
      break;
    }
    case 'fixed_spot': {
      const b = s.best_spot;
      out.head = b.marked
        ? t('ai_review.marked_spot_head', 'No alert: AI saw {vowel, select, yes {an {label}} other {a {label}}} ({score}), in a spot where an earlier event was marked “{mark}”.', { ...art(b.label), score: pct(b.score), mark: esc(notA(b.label).toLowerCase()) })
        : t('ai_review.no_alert_spot_repeat', 'No alert: AI saw {vowel, select, yes {an {label}} other {a {label}}} ({score}) that has been in that spot in several earlier events, so it does not count.', { ...art(kindWords(b.label)), score: pct(b.score) });
      out.lines.push(looked + '.');
      out.lines.push(b.marked
        ? t('ai_review.mark_explanation', 'A mark lasts 30 days. Something there that moves still alerts, and so does anything elsewhere in the picture.')
        : t('ai_review.mark_detail', 'This keeps a bush, a bin or a parked car that AI keeps mistaking from alerting again and again. Something there that moves still alerts, and so does anything elsewhere in the picture. A spot not seen for a day is forgotten.'));
      out.lines.push(t('ai_review.still_detected', 'It is still detected, recorded and searchable. A camera\'s AI settings show its remembered spots and can forget them.'));
      break;
    }
    case 'camera_only': {
      const l = (v.camera_unconfirmed || []).map(x => esc(x));
      out.head = t('ai_review.camera_only_head', 'No alert: the camera itself reported {things}, but AI did not see {count, select, one {one} other {any of them}}.', { things: listAnd(l.map(an)), count: l.length === 1 ? 'one' : 'many' });
      out.lines.push(looked + '.');
      out.lines.push(t('ai_review.with_ai_on', 'With AI on, the camera\'s own detection of a person, vehicle or animal counts only when AI sees it too (or cannot look). Doorbell presses, sounds and packages count as always. It is still recorded and labeled.'));
      // (The setting is off unless turned on: an older event may come from when it was on.)
      if (cam.confirm_camera === false) out.lines.push(t('ai_review.own_detection_was_on', '\u201cCamera\'s own detection needs AI to agree\u201d was on for {camera} at the time. It is off now, so the camera\'s own detection alerts on its own.', { camera: esc(cam.name || 'this camera') }));
      else out.lines.push(t('ai_review.turn_off_own_detection', 'To let the camera\'s own detection alert on its own, turn off \u201cCamera\'s own detection needs AI to agree\u201d in the camera\'s AI settings.'));
      break;
    }
    case 'ai_failed': {
      const err = (checks.find(c => c.error) || {}).error || '';
      out.head = t('ai_review.ai_failed_head', 'AI could not check this event, so it was treated as if AI were off{sent, select, yes { (the alert was sent).} other {.}}', { sent: v.alert && v.alert.sent ? 'yes' : 'no' });
      // (One plain line from the server; the full text is in Frames and details.)
      out.lines.push(t('ai_review.the_error', 'The error: {error}.', { error: esc(err) }));
      // A graphics card error is retried by itself (later, or on the processor).
      if (/graphics card/i.test(err)) out.lines.push(t('ai_review.retry_notice', 'AmberSight looks at this event\'s pictures again in a few minutes; if the card keeps failing, the AI runs on the processor for a while. Another program using the card\'s memory is a common cause.'));
      else out.lines.push(t('ai_review.check_model', 'Check {where}: the model may need downloading again, or the AI server may be down.', { where: where }));
      break;
    }
    case 'alerts_off': {
      // Say which reason applied when the review knows it.
      const why = v.alert && v.alert.why, nm = camName;
      const reason = {
        paused: t('ai_review.reason_paused', 'notifications for {name} were paused at the time', { name: nm }),
        alerts_off: t('ai_review.reason_alerts_off', 'alerts were turned off for {name}', { name: nm }),
        alert_window: t('ai_review.reason_outside_hours', 'it was outside the hours {name} sends alerts', { name: nm }),
        alert_interval: t('ai_review.reason_recent_alert', 'an alert for {name} had just been sent (the pause between alerts)', { name: nm }),
        notify_objects: t('ai_review.reason_not_notified', 'what AI saw is not among the objects {name} notifies for', { name: nm }),
        notify_area: t('ai_review.reason_never_notifies', 'it was in an area of {name} that never notifies', { name: nm }),
        from_card: t('ai_review.reason_from_card', "it was imported later from {name}'s own card, too long after it happened to notify", { name: nm }),
      }[why];
      out.head = t('ai_review.head', 'AI confirmed {labels}, but no alert was sent: {msg}', { labels: esc(listAnd((v.labels || []).map(objName))), msg: reason ? reason + '.' : t('ai_review.reason_default', 'alerts were off for {name} at the time (its alert setting, schedule, a notification pause, its "Notify me for" objects, or Disarmed).', { name: nm }) });
      break;
    }
  }
  if ((v.camera_labels || []).length && s.kind !== 'alerted' && s.kind !== 'camera_only') out.lines.push(t('ai_review.camera_reported', 'The camera itself reported: {labels}.', { labels: esc(v.camera_labels.map(objName).join(', ')) }));
  // Settings changed since?
  const nowMin = cam.min_score, nowLabels = cam.labels || [];
  const changed = [];
  if (nowMin != null && Math.abs(nowMin - min) > 0.001) changed.push(t('ai_review.min_changed', 'minimum is now {now} (was {was})', { now: pct(nowMin), was: pct(min) }));
  if (nowLabels.length && (nowLabels.length !== wanted.length || nowLabels.some(l => !wanted.includes(l)))) changed.push(t('ai_review.alert_objects_changed', 'alert objects are now {objects}', { objects: esc(nowLabels.map(objName).join(', ')) }));
  if (changed.length) out.lines.push(t('ai_review.settings_changed', 'Settings have changed since this event: the {changes}.', { changes: changed.join('; the ') }));
  // Hide fixes already applied.
  out.fixes = out.fixes.filter(f => !(f.kind === 'label' && nowLabels.includes(f.value)) && !(f.kind === 'min' && nowMin != null && nowMin <= f.value + 0.001));
  if (v.source) out.lines.push(t('ai_review.checked_by', 'Checked by: {source}.', { source: esc(v.source) }));
  return out;
}

// One frame card with the AI's boxes.
function frameCard(id, c, v, again) {
  const min = v.min_score, wanted = v.wanted || [];
  const why = new Map((c.seen || []).map((d, i) => [d, (c.spot && c.spot[i]) || ((c.parked && c.parked[i]) ? 'parked' : '')]));
  const cls = (d) => why.get(d) ? 'faint' : wanted.includes(d.label) ? (d.score >= min ? 'hit' : 'low') : (d.score >= min ? 'other' : 'faint');
  const tag = (d) => ({ parked: ' (' + parkedWord(d.label) + ')', remembered: ' (' + t('ai_review.tag_same_spot', 'same spot') + ')', marked: ' (' + t('ai_review.tag_marked', 'marked not real') + ')' })[why.get(d)] || '';
  const seen = (c.seen || []).slice().sort((a, b) => b.score - a.score);
  const boxes = seen.filter(d => d.w > 0 && d.h > 0).map(d => '<rect class="air-' + cls(d) + '" x="' + d.x * 100 + '" y="' + d.y * 100 + '" width="' + d.w * 100 + '" height="' + d.h * 100 + '"/>').join('');
  const chips = seen.length ? seen.map(d => '<span class="air-chip air-' + cls(d) + '">' + esc(objName(d.label)) + ' ' + pct(d.score) + tag(d) + '</span>').join('') : '<span class="muted">' + t('ai_review.nothing', 'nothing') + '</span>';
  const re = again ? '<div class="air-again"><b>' + t('ai_review.today', 'Today:') + '</b> ' + ((again.seen || []).length ? again.seen.slice().sort((a, b) => b.score - a.score).map(d => '<span class="air-chip air-' + cls(d) + '">' + esc(objName(d.label)) + ' ' + pct(d.score) + '</span>').join('') : '<span class="muted">' + t('ai_review.nothing', 'nothing') + '</span>') + '</div>' : '';
  return '<div class="air-frame"><div class="air-img">' +
    (c.frame ? '<img loading="lazy" src="/api/events/' + encodeURIComponent(id) + '/ai/' + c.n + '" alt="">' : '<div class="air-noimg">' + t('ai_review.frame_not_kept', 'frame not kept') + '</div>') +
    '<svg viewBox="0 0 100 100" preserveAspectRatio="none">' + boxes + '</svg></div>' +
    '<div class="air-cap"><span class="muted">+' + secs(Math.max(0, c.offset_ms)) + '</span> ' + (c.error ? '<span class="air-err">' + esc(c.error) + '</span>' : chips) + '</div>' + re + '</div>';
}

export async function openAiReview(eventId) {
  let r;
  try { r = await api('/api/events/' + encodeURIComponent(eventId) + '/ai'); }
  catch (e) { toast(t('ai_review.load_error', 'Could not load the AI review: {error}', { error: errText(e) })); return; }
  const d = openDlg(t('ai_review.dlg_title', 'AI review - {camera}, {time}', { camera: r.event.camera, time: fmtTime(r.event.start) }), '<div class="air" data-air></div>');
  d.classList.add('wide', 'air-dlg');
  let again = null;
  const paint = () => {
    const x = explain(r), v = r.review;
    let h = '<div class="air-head air-' + (r.summary ? r.summary.kind : 'none') + '">' + x.head + '</div>';
    if (x.lines.length) h += '<ul class="air-lines">' + x.lines.map(l => '<li>' + l + '</li>').join('') + '</ul>';
    const det = v && (v.checks || []).map(c => c.error_detail).find(Boolean);
    if (det) h += '<details class="air-errdetail"><summary>' + t('ai_review.full_error', 'Full error text') + '</summary><pre>' + esc(det) + '</pre></details>';
    if (x.fixes.length && mayFix(r)) h += '<div class="air-fixes">' + x.fixes.map((f, i) => '<button class="btn" data-fix="' + i + '">' + f.label + '</button>').join('') + '</div>';
    if (v && (v.checks || []).length) {
      h += '<div class="air-legend"><span class="air-chip air-hit">' + t('ai_review.legend_counts', 'counts') + '</span><span class="air-chip air-low">' + t('ai_review.legend_low', 'wanted, not sure enough') + '</span><span class="air-chip air-other">' + t('ai_review.legend_other', 'not on the alert list') + '</span><span class="air-chip air-faint">' + t('ai_review.legend_faint', 'faint guess') + '</span></div>';
      h += '<div class="air-grid">' + v.checks.map(c => frameCard(r.event.id, c, v, again && again.find(a => a.n === c.n))).join('') + '</div>';
      if (state.isAdmin && v.checks.some(c => c.frame)) h += '<div class="row"><button class="btn" data-recheck>' + ic('swap') + ' ' + t('ai_review.recheck', "Check these frames again with today's AI") + '</button><span class="hint">' + t('ai_review.recheck_hint', 'After switching models or changing settings: nothing is saved.') + '</span></div>';
    }
    const body = d.querySelector('[data-air]');
    body.innerHTML = h;
    body.querySelectorAll('.air-head a[href^="#"]').forEach(a => a.addEventListener('click', () => { if (d.__close) d.__close(); }));
    body.querySelectorAll('[data-fix]').forEach(b => { b.onclick = () => applyFix(x.fixes[+b.dataset.fix], b); });
    const rc = body.querySelector('[data-recheck]');
    if (rc) rc.onclick = async () => {
      rc.disabled = true; rc.textContent = t('ai_review.checking', 'Checking...');
      try { again = (await api('/api/events/' + encodeURIComponent(r.event.id) + '/ai/recheck', { json: {} })).checks || []; paint(); }
      catch (e) { toast(t('ai_review.recheck_error', 'Could not check again: {error}', { error: errText(e) })); rc.disabled = false; }
    };
  };
  async function applyFix(f, b) {
    const cam = r.camera;
    if (!cam || cam.id == null) { toast(t('ai_review.camera_missing', 'That camera no longer exists')); return; }
    const body = f.kind === 'label' ? { ai_labels: (cam.labels || []).concat([f.value]) } : f.kind === 'strict' ? { ai_strict: f.value } : { ai_min_score: f.value };
    b.disabled = true;
    try {
      await api('/api/cameras/' + cam.id, { method: 'PUT', json: body });
      toast(f.kind === 'label' ? t('ai_review.label_added', '{camera} now also alerts for {value}', { camera: cam.name, value: f.value }) : f.kind === 'strict' ? t('ai_review.strict_off', 'Stricter confirmation is off for {camera}', { camera: cam.name }) : t('ai_review.min_updated', "{camera}'s minimum is now {value}", { camera: cam.name, value: pct(f.value) }));
      r = await api('/api/events/' + encodeURIComponent(eventId) + '/ai');
      paint();
    } catch (e) { toast(t('ai_review.change_error', 'Could not change it: {error}', { error: errText(e) })); b.disabled = false; }
  }
  paint();
}

// "Not a person" (or "Not a car" ...): the event was not real, and AI remembers that
// object's spot on this camera. Returns the server's answer, or null.
export async function markNotReal(eventId, label) {
  try {
    const r = await api('/api/events/' + encodeURIComponent(eventId) + '/review', { json: { verdict: 'false_alarm', label } });
    const m = r.marked;
    toast(m && m.spots ? t('ai_review.marked_not_real', 'Marked as not real. {camera} will not alert for {vowel, select, yes {an {label}} other {a {label}}} in {spots, plural, one {that spot} other {those # spots}} for 30 days, unless it moves.', { camera: m.camera, ...art(kindWords(m.label), false), spots: m.spots })
      : t('ai_review.marked_plain', 'Marked as not a real alert. {why, select, one_look {AI looked only once in this event: too little to tell what stood still, so it remembers no spot.} other {Nothing of that kind sat still in it, so AI remembers no spot (something that moves is never remembered).}}', { why: m && m.why === 'one_look' ? 'one_look' : 'other' }), { kind: 'ok', ms: 5000 });
    return r;
  } catch (e) { toast(t('ai_review.mark_error', 'Could not mark it: {error}', { error: errText(e) }), { kind: 'err' }); return null; }
}

// Compact verdict for the Events page player: headline, the first reason, one-click
// fixes (admins; operators allowed to change the camera), "Not a person" marks (those who may
// mark events) and a button for the full frame-by-frame
// dialog. `opts.onReview(review)` hears of a mark. Returns a promise.
export async function renderVerdict(box, eventId, opts = {}) {
  box.innerHTML = '<span class="muted">' + t('ai_review.loading', 'AI review...') + '</span>';
  let r;
  try { r = await api('/api/events/' + encodeURIComponent(eventId) + '/ai'); }
  catch (e) { box.innerHTML = ''; return; }
  const paint = () => {
    const x = explain(r);
    const kind = r.summary ? r.summary.kind : 'none';
    let h = '<div class="evv-head air-' + kind + '">' + x.head + '</div>';
    if (x.lines.length) h += '<div class="evv-line">' + x.lines[0] + '</div>';
    h += '<div class="evv-btns">';
    if (mayFix(r)) h += x.fixes.map((f, i) => '<button class="btn" data-fix="' + i + '">' + f.label + '</button>').join('');
    // (Only what made it alert: a mark for each kind of object that counted.)
    const kinds = r.review && can('events') ? countedKinds(r.review) : [];
    h += kinds.map(l => '<button class="btn" data-notreal="' + esc(l) + '" title="' + t('ai_review.not_real_title', 'AI got this wrong: if it sat still, AI will not alert for this {kind} in this spot again for 30 days, unless it moves', { kind: esc(kindOf(l)) }) + '">' + ic('x') + '<span>' + esc(notA(l)) + '</span></button>').join('');
    if (r.review && (r.review.checks || []).length) h += '<button class="btn" data-frames>' + t('ai_review.frames_details', 'Frames and details') + '</button>';
    h += '</div>';
    box.innerHTML = h;
    const fr = box.querySelector('[data-frames]');
    if (fr) fr.onclick = () => openAiReview(eventId);
    box.querySelectorAll('[data-notreal]').forEach(b => { b.onclick = async () => {
      b.disabled = true;
      const res = await markNotReal(eventId, b.dataset.notreal);
      if (!res) { b.disabled = false; return; }
      b.classList.add('on'); b.setAttribute('aria-pressed', 'true');
      if (opts.onReview) opts.onReview(res.review || null);
    }; });
    box.querySelectorAll('[data-fix]').forEach(b => { b.onclick = async () => {
      const f = x.fixes[+b.dataset.fix], cam = r.camera;
      if (!cam || cam.id == null) { toast(t('ai_review.camera_gone', 'That camera no longer exists')); return; }
      const body = f.kind === 'label' ? { ai_labels: (cam.labels || []).concat([f.value]) } : f.kind === 'strict' ? { ai_strict: f.value } : { ai_min_score: f.value };
      b.disabled = true;
      try {
        await api('/api/cameras/' + cam.id, { method: 'PUT', json: body });
        toast(f.kind === 'label' ? t('ai_review.label_added', '{camera} now also alerts for {value}', { camera: cam.name, value: f.value }) : f.kind === 'strict' ? t('ai_review.strict_off', 'Stricter confirmation is off for {camera}', { camera: cam.name }) : t('ai_review.min_updated', "{camera}'s minimum is now {value}", { camera: cam.name, value: pct(f.value) }), { kind: 'ok' });
        r = await api('/api/events/' + encodeURIComponent(eventId) + '/ai');
        paint();
      } catch (e) { toast(t('ai_review.could_not_change', 'Could not change it: {error}', { error: errText(e) }), { kind: 'err' }); b.disabled = false; }
    }; });
  };
  paint();
}

// Frames the AI kept for an event (numbers for /api/events/<id>/ai/<n>), cached.
const frameCache = new Map();
export function aiFrames(eventId) {
  if (!frameCache.has(eventId)) {
    const p = api('/api/events/' + encodeURIComponent(eventId) + '/ai')
      .then(r => ((r.review && r.review.checks) || []).filter(c => c.frame).map(c => c.n))
      .catch(() => []);
    frameCache.set(eventId, p);
    if (frameCache.size > 300) frameCache.delete(frameCache.keys().next().value);
  }
  return frameCache.get(eventId);
}

// ---------------------------------------------------------------- Admin page
const FILTERS = [['all', t('ai_review.f_all', 'All')], ['alerted', t('ai_review.f_alerts_sent', 'Alerts sent')], ['noalert', t('ai_review.f_no_alert', 'No alert')], ['near_miss', t('ai_review.near_miss_label', 'Almost')], ['strict', t('ai_review.strict_label', 'Not confirmed')], ['other_object', t('ai_review.f_other_objects', 'Other objects')], ['parked', t('ai_review.parked_label', 'Parked')], ['fixed_spot', t('ai_review.fixed_spot_label', 'Same spot')], ['camera_only', t('ai_review.camera_only_label', 'Camera only')]];

function reviewPage(el) {
  let days = 7, cam = '', filter = 'all', data = null;
  el.innerHTML = '<div class="asys"><div class="asys-head"><span class="hint"><b>' + t('ai_review.optional', 'Optional.') + '</b> ' + t('ai_review.page_hint', 'Shows what the AI saw in each motion event, and why it did or did not alert. Click an event for the frames and one-click fixes. Use it when alerts are missing or false.') + '</span></div><div data-ar><p class="empty">' + t('ai_review.loading_page', 'Loading...') + '</p></div></div>';
  const box = el.querySelector('[data-ar]');
  stillWaiting(el.querySelector('.asys'), () => load());
  async function load() {
    // (One kind: the server looks for it through the whole period, not only the newest events.)
    try { data = await api('/api/ai/reviews?days=' + days + (cam ? '&camera=' + encodeURIComponent(cam) : '') + (filter !== 'all' ? '&kind=' + encodeURIComponent(filter) : '')); }
    catch (e) { box.innerHTML = '<p class="empty">' + t('ai_review.page_load_error', 'Could not load: {error}', { error: esc(errText(e)) }) + '</p>'; return; }
    paint();
  }
  function paint() {
    const n = (o, k) => o[k] || 0;
    let h = '<div class="ar-bar"><select data-days>' + [1, 7, 30].map(x => '<option value="' + x + '"' + (x === days ? ' selected' : '') + '>' + (x === 1 ? t('ai_review.last_day', 'Last day') : t('ai_review.last_n_days', 'Last {n} days', { n: x })) + '</option>').join('') + '</select>' +
      '<select data-cam><option value="">' + t('ai_review.all_cameras', 'All cameras') + '</option>' + (data.cameras || []).map(c => '<option' + (c.camera === cam ? ' selected' : '') + '>' + esc(c.camera) + '</option>').join('') + '</select>' +
      '<span class="ar-chips">' + FILTERS.map(f => '<button class="chip' + (f[0] === filter ? ' on' : '') + '" data-f="' + f[0] + '">' + f[1] + '</button>').join('') + '</span></div>';
    if (!(data.cameras || []).length) {
      box.innerHTML = h + '<p class="empty">' + t('ai_review.no_events', 'No AI-checked events in this period. AI review starts recording from now on; events from before have no AI record. Check that AI is on in {where}.', { where: t('ai_review.settings_path', 'Settings \u203a Detection &amp; alerts \u203a Objects (AI)') }) + '</p>';
      bind(); return;
    }
    if (!cam) {
      h += '<div class="asys-scroll"><table class="asys-tbl ar-tbl"><thead><tr><th>' + t('ai_review.th_camera', 'Camera') + '</th><th class="num">' + t('ai_review.th_checked', 'Checked') + '</th><th class="num">' + t('ai_review.f_alerts_sent', 'Alerts sent') + '</th><th class="num">' + t('ai_review.near_miss_label', 'Almost') + '</th><th class="num">' + t('ai_review.f_other_objects', 'Other objects') + '</th><th class="num">' + t('ai_review.parked_label', 'Parked') + '</th><th class="num" title="' + t('ai_review.th_same_spot_title', 'Something that has been in the same spot in several earlier events, or that was marked not real there') + '">' + t('ai_review.fixed_spot_label', 'Same spot') + '</th><th class="num" title="' + t('ai_review.th_camera_only_title', 'The camera itself reported a person, vehicle or animal, and AI did not see one') + '">' + t('ai_review.camera_only_label', 'Camera only') + '</th><th class="num">' + t('ai_review.nothing_label', 'Nothing found') + '</th><th class="ar-opt">' + t('ai_review.th_alerts_for', 'Alerts for') + '</th><th class="num ar-opt">' + t('ai_review.th_minimum', 'Minimum') + '</th></tr></thead><tbody>' +
        data.cameras.map(c => '<tr data-pickcam="' + esc(c.camera) + '" class="ar-row"><td><b>' + esc(c.camera) + '</b></td><td class="num">' + n(c, 'total') + '</td><td class="num">' + n(c, 'alerted') + '</td>' +
          '<td class="num">' + (n(c, 'near_miss') ? '<b>' + n(c, 'near_miss') + '</b>' : 0) + '</td><td class="num">' + n(c, 'other_object') + '</td><td class="num">' + n(c, 'parked') + '</td><td class="num">' + n(c, 'fixed_spot') + '</td><td class="num">' + n(c, 'camera_only') + '</td><td class="num">' + n(c, 'nothing') + '</td>' +
          '<td class="ar-opt ar-for" title="' + esc(((c.now || {}).labels || []).join(', ')) + '">' + esc(((c.now || {}).labels || []).map(objName).join(', ')) + '</td><td class="num ar-opt">' + (c.now && c.now.min_score != null ? pct(c.now.min_score) : '-') + '</td></tr>').join('') + '</tbody></table></div>' +
        (data.cameras.some(c => c.sampled < c.total) ? '<p class="hint">' + t('ai_review.sampled_hint', 'Checked and Alerts sent count the whole period. For {cameras}, the other columns are based on the latest {n} events of each camera.', { cameras: data.cameras.filter(c => c.sampled < c.total).map(c => esc(c.camera)).join(', '), n: data.per_camera_limit }) + '</p>' : '') +
        '<p class="hint">' + t('ai_review.almost_hint', '"Almost" means AI saw something it alerts for but was not sure enough: worth a look if you are missing alerts. Many "Nothing found" usually means shadows, lighting or plants set off motion; a motion zone or an ignore zone can help.') + '</p>';
    }
    const evs = (data.events || []).filter(e => filter === 'all' || (filter === 'noalert' ? !e.alerted : e.summary.kind === filter));
    const most = data.list_limit || 300;
    h += '<h4 class="asys-sub">' + t('ai_review.events', 'Events') + (cam ? ' - ' + esc(cam) : '') + '</h4>' + ((data.events || []).length >= most ? '<p class="hint">' + t('ai_review.showing_latest', 'Showing the latest {n} events{kind, select, yes { of this kind} other {}}.', { n: most, kind: filter !== 'all' && data.kind ? 'yes' : 'no' }) + '</p>' : '');
    h += evs.length ? '<div class="ar-list">' + evs.map(e => {
      const k = e.summary.kind === 'parked' && e.summary.best_parked && !VEHICLES.includes(e.summary.best_parked.label) ? [t('ai_review.not_moving_label', 'Not moving'), ''] : KIND[e.summary.kind] || ['?', ''];
      const b = e.summary.kind === 'parked' ? e.summary.best_parked : e.summary.kind === 'fixed_spot' ? e.summary.best_spot : e.summary.best_wanted && (e.summary.kind === 'alerted' || e.summary.kind === 'near_miss' || e.summary.kind === 'strict') ? e.summary.best_wanted : e.summary.best_other;
      return '<button class="ar-ev" data-ev="' + esc(e.id) + '"><img loading="lazy" src="/api/events/' + encodeURIComponent(e.id) + '/thumb" alt="">' +
        '<span class="ar-evt"><b>' + esc(e.camera) + '</b><span class="muted">' + esc(fmtTime(e.start)) + '</span>' +
        '<span><span class="badge ' + k[1] + '">' + k[0] + '</span> ' + (b ? esc(objName(b.label)) + ' ' + pct(b.score) : (e.summary.camera_labels || []).length ? t('ai_review.camera_labels', 'camera: {labels}', { labels: esc(e.summary.camera_labels.map(objName).join(', ')) }) : '<span class="muted">' + t('ai_review.no_objects', 'no objects') + '</span>') + '</span></span></button>';
    }).join('') + '</div>' : '<p class="empty">' + t('ai_review.no_events_match', 'No events match.') + '</p>';
    box.innerHTML = h;
    bind();
  }
  function bind() {
    box.querySelector('[data-days]').onchange = (e) => { days = +e.target.value; load(); };
    box.querySelector('[data-cam]').onchange = (e) => { cam = e.target.value; load(); };
    // An older server ignores the kind and sends the latest events: they are filtered here as well.
    box.querySelectorAll('[data-f]').forEach(b => { b.onclick = () => { filter = b.dataset.f; if (data && data.kind !== undefined) load(); else paint(); }; });
    box.querySelectorAll('[data-pickcam]').forEach(row => { row.onclick = () => { cam = row.dataset.pickcam; load(); }; });
    box.querySelectorAll('[data-ev]').forEach(b => { b.onclick = () => openAiReview(b.dataset.ev); });
  }
  load();
}

registerAdminPage({ id: 'aireview', title: t('ai_review.title', 'AI review'), order: 65, render: reviewPage });
