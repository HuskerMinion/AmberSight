// ai_review.js - AI review: "why did (or didn't) this event alert?" in plain
// words, the frames the AI looked at with what it saw drawn on them, and
// one-click changes. Also the Settings > Smart features > AI review page (recent events by verdict).
import { api, esc, toast, openDlg, fmtTime, state, ic, stillWaiting, can } from './core.js';

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
const list = (a, word = 'or') => a.length <= 1 ? (a[0] || '') : a.slice(0, -1).join(', ') + ' ' + word + ' ' + a[a.length - 1];
const listAnd = (a) => list(a, 'and');
const an = (w) => (/^[aeiou]/i.test(w) ? 'an ' : 'a ') + w;

// Short verdict words for lists.
export const KIND = {
  alerted: ['Alert sent', 'on'],
  near_miss: ['Almost', 'warnb'],
  strict: ['Not confirmed', 'warnb'],
  quiet_person: ['Known person', 'info'],
  known_only: ['Not a stranger', 'info'],
  quiet_rule: ['Quiet rule', 'info'],
  notify_area: ['Quiet area', 'info'],
  notify_objects: ['Not notified', 'info'],
  other_object: ['Other object', 'info'],
  nothing: ['Nothing found', ''],
  alerts_off: ['Alerts off', ''],
  ai_failed: ['AI failed', 'off'],
  parked: ['Parked', ''],
  fixed_spot: ['Same spot', ''],
  camera_only: ['Camera only', 'info'],
};

// What a thing of this kind is called in plain words ("a person-shaped object").
const kindWords = (label) => label === 'person' ? 'person-shaped object' : ANIMALS.includes(label) ? 'animal-shaped object' : label;
// A vehicle that has not moved is parked; anything else is still.
const parkedWord = (label) => VEHICLES.includes(label) ? 'parked' : 'not moving';
const VEHICLES = ['car', 'truck', 'bus', 'motorcycle', 'vehicle', 'van', 'suv', 'bicycle', 'boat', 'train', 'airplane'];
const ANIMALS = ['animal', 'dog', 'cat', 'bird', 'horse', 'sheep', 'cow', 'bear', 'elephant', 'zebra', 'giraffe'];
const kindOf = (label) => label === 'person' ? 'person' : VEHICLES.includes(label) ? 'vehicle' : ANIMALS.includes(label) ? 'animal' : label;
const notA = (label) => 'Not ' + an(kindOf(label) === 'vehicle' ? label : kindOf(label));

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
  const where = state.isAdmin ? '<a href="#settings/detection/objects">Settings \u203a Detection &amp; alerts \u203a Objects (AI)</a>' : 'Settings \u203a Detection &amp; alerts \u203a Objects (AI)';
  const v = r.review, s = r.summary, cam = r.camera || {};
  const out = { head: '', lines: [], fixes: [] };
  if (!v) {
    if (!r.ai.enabled) out.head = 'AI did not check this event: AI is turned off. Turn it on in ' + where + '.';
    else if (!r.ai.model && !r.ai.server) out.head = 'AI did not check this event: no AI model is installed. Choose or download one in ' + where + '.';
    else if (cam.ai === false) {
      out.head = 'AI did not check this event: AI is turned off for ' + esc(cam.name) + '.';
      out.lines.push('Turn it on in the camera\'s settings, AI tab.');
    } else out.head = 'There is no AI record for this event. It may be from before AI review was added, or motion ended before the first check.';
    return out;
  }
  const min = v.min_score, wanted = v.wanted || [];
  const checks = v.checks || [];
  const span = checks.length ? secs(Math.max(0, checks[checks.length - 1].offset_ms)) : '';
  const firstHit = checks.find(c => (c.seen || []).some(d => wanted.includes(d.label) && d.score >= min));
  const hit = firstHit && firstHit.seen.filter(d => wanted.includes(d.label) && d.score >= min).sort((a, b) => b.score - a.score)[0];
  const bw = s.best_wanted, bo = s.best_other;
  const looked = checks.length ? 'AI looked at ' + checks.length + ' frame' + (checks.length === 1 ? '' : 's') + (checks.length > 1 ? ' over ' + span : '') : '';
  switch (s.kind) {
    case 'alerted':
      if (hit) out.head = 'Alert sent: AI saw ' + an(esc(hit.label)) + ' (' + pct(hit.score) + ' sure) ' + (firstHit.offset_ms < 500 ? 'as soon as it looked.' : secs(firstHit.offset_ms) + ' into the event.');
      else out.head = 'Alert sent: the camera itself reported ' + esc(listAnd(v.camera_labels || [])) + '.';
      if (checks.length) out.lines.push(looked + ' and stopped once it was sure.');
      break;
    case 'near_miss':
      out.head = 'No alert: AI thought it might be ' + an(esc(bw.label)) + ', but only ' + pct(bw.score) + ' sure. ' + esc(cam.name || 'This camera') + ' needs ' + pct(min) + '.';
      out.lines.push(looked + '.');
      out.lines.push('If that was real, lowering the minimum would have sent this alert. Lower it a little at a time: too low and shadows or plants start to count.');
      out.fixes.push({ kind: 'min', value: Math.max(0.25, Math.floor(bw.score * 20) / 20), label: 'Lower ' + esc(cam.name || 'this camera') + '\'s minimum to ' + pct(Math.max(0.25, Math.floor(bw.score * 20) / 20)) });
      break;
    case 'quiet_person':
      out.head = 'No alert: the only person seen was recognized as someone marked "no alerts" in Settings \u203a Smart features \u203a Faces.';
      out.lines.push(looked + '.');
      out.lines.push('To get alerts for them again, turn that off for the person in Settings \u203a Smart features \u203a Faces.');
      break;
    case 'known_only':
      out.head = 'No alert: this camera alerts only for strangers, and everyone seen was known (or, with \u201cfirst time only\u201d, had been seen before).';
      out.lines.push(looked + '.');
      out.lines.push('Change this in the camera\'s Alerts settings, under Strangers.');
      break;
    case 'quiet_rule':
      out.head = 'No alert: a quiet alert rule matched this event\'s pictures.';
      out.lines.push(looked + '.');
      out.lines.push('Quiet rules are in Settings \u203a Alert rules.');
      break;
    case 'notify_area':
      out.head = 'No notification: everything AI saw stood inside an area of ' + esc(cam.name || 'this camera') + ' that is set not to notify.';
      out.lines.push(looked + '.');
      out.lines.push('It is still detected, recorded and searchable, and shows here and in Events. Change the areas in the camera\'s Alerts settings, under \u201cAreas that don\'t notify\u201d.');
      break;
    case 'notify_objects':
      out.head = 'No notification: ' + esc(cam.name || 'this camera') + ' only notifies for certain objects, and this was not one of them.';
      out.lines.push(looked + '.');
      out.lines.push('It is still detected, recorded and searchable. Change this in the camera\'s Alerts settings, under \u201cNotify me for\u201d.');
      break;
    case 'strict':
      out.head = 'No alert: AI saw ' + an(esc(bw.label)) + ' (' + pct(bw.score) + ' sure), but stricter confirmation is on and it was not seen moving in 2 of 3 looks in a row.';
      out.lines.push(looked + '.');
      out.lines.push('This stops a single wrong guess, or something that never moves (a statue, a bush), from alerting. Someone who walks through quickly can be seen only once, though.');
      out.fixes.push({ kind: 'strict', value: false, label: 'Turn stricter confirmation off for ' + esc(cam.name || 'this camera') });
      break;
    case 'other_object':
      out.head = 'No alert: AI saw ' + an(esc(bo.label)) + ' (' + pct(bo.score) + '), but ' + esc(cam.name || 'this camera') + ' only alerts for ' + esc(list(wanted)) + '.';
      out.lines.push(looked + '.');
      out.fixes.push({ kind: 'label', value: bo.label, label: 'Also alert for ' + esc(bo.label) + ' on ' + esc(cam.name || 'this camera') });
      break;
    case 'nothing':
      out.head = 'No alert: AI did not recognize anything it alerts for.';
      out.lines.push(looked + ' and found ' + (bo ? 'only a faint guess (' + esc(bo.label) + ', ' + pct(bo.score) + ')' : 'no objects') + '.');
      out.lines.push('Usually this is shadows, sunlight, rain, insects, or plants moving: the motion was real, but nothing worth an alert.');
      out.lines.push('If something real was there, it may have been too small or too far away. The Small model (Settings › Detection &amp; alerts › Objects (AI)) catches more of those; a motion zone can keep detection to the area that matters.');
      break;
    case 'parked': {
      const bp = s.best_parked;
      out.head = 'No alert: AI saw ' + an(esc(bp.label)) + ' (' + pct(bp.score) + '), but it has been in the same place for minutes (' + parkedWord(bp.label) + '), so it does not count.';
      out.lines.push(looked + '.');
      out.lines.push('This keeps a parked car (or someone sitting still) from confirming every motion event. One that arrives or leaves still counts.');
      out.lines.push('To count objects even when they have not moved, turn off "Ignore objects that have not moved" in this camera\'s AI settings or in ' + where + '.');
      break;
    }
    case 'fixed_spot': {
      const b = s.best_spot;
      out.head = b.marked
        ? 'No alert: AI saw ' + an(esc(b.label)) + ' (' + pct(b.score) + '), in a spot where an earlier event was marked “' + esc(notA(b.label).toLowerCase()) + '”.'
        : 'No alert: AI saw ' + an(esc(kindWords(b.label))) + ' (' + pct(b.score) + ') that has been in that spot in several earlier events, so it does not count.';
      out.lines.push(looked + '.');
      out.lines.push(b.marked
        ? 'A mark lasts 30 days. Something there that moves still alerts, and so does anything elsewhere in the picture.'
        : 'This keeps a bush, a bin or a parked car that AI keeps mistaking from alerting again and again. Something there that moves still alerts, and so does anything elsewhere in the picture. A spot not seen for a day is forgotten.');
      out.lines.push('It is still detected, recorded and searchable. A camera\'s AI settings show its remembered spots and can forget them.');
      break;
    }
    case 'camera_only': {
      const l = (v.camera_unconfirmed || []).map(x => esc(x));
      out.head = 'No alert: the camera itself reported ' + listAnd(l.map(an)) + ', but AI did not see ' + (l.length === 1 ? 'one' : 'any of them') + '.';
      out.lines.push(looked + '.');
      out.lines.push('With AI on, the camera\'s own detection of a person, vehicle or animal counts only when AI sees it too (or cannot look). Doorbell presses, sounds and packages count as always. It is still recorded and labeled.');
      // (The setting is off unless turned on: an older event may come from when it was on.)
      if (cam.confirm_camera === false) out.lines.push('\u201cCamera\'s own detection needs AI to agree\u201d was on for ' + esc(cam.name || 'this camera') + ' at the time. It is off now, so the camera\'s own detection alerts on its own.');
      else out.lines.push('To let the camera\'s own detection alert on its own, turn off \u201cCamera\'s own detection needs AI to agree\u201d in the camera\'s AI settings.');
      break;
    }
    case 'ai_failed': {
      const err = (checks.find(c => c.error) || {}).error || '';
      out.head = 'AI could not check this event, so it was treated as if AI were off' + (v.alert && v.alert.sent ? ' (the alert was sent).' : '.');
      out.lines.push('The error: ' + esc(err));
      out.lines.push('Check ' + where + ': the model may need downloading again, or the AI server may be down.');
      break;
    }
    case 'alerts_off': {
      // Say which reason applied when the review knows it.
      const why = v.alert && v.alert.why, nm = esc(cam.name || 'this camera');
      const reason = {
        paused: 'notifications for ' + nm + ' were paused at the time',
        alerts_off: 'alerts were turned off for ' + nm,
        alert_window: 'it was outside the hours ' + nm + ' sends alerts',
        alert_interval: 'an alert for ' + nm + ' had just been sent (the pause between alerts)',
        notify_objects: 'what AI saw is not among the objects ' + nm + ' notifies for',
        notify_area: 'it was in an area of ' + nm + ' that never notifies',
      }[why];
      out.head = 'AI confirmed ' + esc(listAnd(v.labels || [])) + ', but no alert was sent: ' + (reason ? reason + '.' : 'alerts were off for ' + nm + ' at the time (its alert setting, schedule, a notification pause, its "Notify me for" objects, or Disarmed).');
      break;
    }
  }
  if ((v.camera_labels || []).length && s.kind !== 'alerted' && s.kind !== 'camera_only') out.lines.push('The camera itself reported: ' + esc(v.camera_labels.join(', ')) + '.');
  // Settings changed since?
  const nowMin = cam.min_score, nowLabels = cam.labels || [];
  const changed = [];
  if (nowMin != null && Math.abs(nowMin - min) > 0.001) changed.push('minimum is now ' + pct(nowMin) + ' (was ' + pct(min) + ')');
  if (nowLabels.length && (nowLabels.length !== wanted.length || nowLabels.some(l => !wanted.includes(l)))) changed.push('alert objects are now ' + esc(nowLabels.join(', ')));
  if (changed.length) out.lines.push('Settings have changed since this event: the ' + changed.join('; the ') + '.');
  // Hide fixes already applied.
  out.fixes = out.fixes.filter(f => !(f.kind === 'label' && nowLabels.includes(f.value)) && !(f.kind === 'min' && nowMin != null && nowMin <= f.value + 0.001));
  if (v.source) out.lines.push('Checked by: ' + esc(v.source) + '.');
  return out;
}

// One frame card with the AI's boxes.
function frameCard(id, c, v, again) {
  const min = v.min_score, wanted = v.wanted || [];
  const why = new Map((c.seen || []).map((d, i) => [d, (c.spot && c.spot[i]) || ((c.parked && c.parked[i]) ? 'parked' : '')]));
  const cls = (d) => why.get(d) ? 'faint' : wanted.includes(d.label) ? (d.score >= min ? 'hit' : 'low') : (d.score >= min ? 'other' : 'faint');
  const tag = (d) => ({ parked: ' (' + parkedWord(d.label) + ')', remembered: ' (same spot)', marked: ' (marked not real)' })[why.get(d)] || '';
  const seen = (c.seen || []).slice().sort((a, b) => b.score - a.score);
  const boxes = seen.filter(d => d.w > 0 && d.h > 0).map(d => '<rect class="air-' + cls(d) + '" x="' + d.x * 100 + '" y="' + d.y * 100 + '" width="' + d.w * 100 + '" height="' + d.h * 100 + '"/>').join('');
  const chips = seen.length ? seen.map(d => '<span class="air-chip air-' + cls(d) + '">' + esc(d.label) + ' ' + pct(d.score) + tag(d) + '</span>').join('') : '<span class="muted">nothing</span>';
  const re = again ? '<div class="air-again"><b>Today:</b> ' + ((again.seen || []).length ? again.seen.slice().sort((a, b) => b.score - a.score).map(d => '<span class="air-chip air-' + cls(d) + '">' + esc(d.label) + ' ' + pct(d.score) + '</span>').join('') : '<span class="muted">nothing</span>') + '</div>' : '';
  return '<div class="air-frame"><div class="air-img">' +
    (c.frame ? '<img loading="lazy" src="/api/events/' + encodeURIComponent(id) + '/ai/' + c.n + '" alt="">' : '<div class="air-noimg">frame not kept</div>') +
    '<svg viewBox="0 0 100 100" preserveAspectRatio="none">' + boxes + '</svg></div>' +
    '<div class="air-cap"><span class="muted">+' + secs(Math.max(0, c.offset_ms)) + '</span> ' + (c.error ? '<span class="air-err">' + esc(c.error) + '</span>' : chips) + '</div>' + re + '</div>';
}

export async function openAiReview(eventId) {
  let r;
  try { r = await api('/api/events/' + encodeURIComponent(eventId) + '/ai'); }
  catch (e) { toast('Could not load the AI review: ' + errText(e)); return; }
  const d = openDlg('AI review - ' + r.event.camera + ', ' + fmtTime(r.event.start), '<div class="air" data-air></div>');
  d.classList.add('wide', 'air-dlg');
  let again = null;
  const paint = () => {
    const x = explain(r), v = r.review;
    let h = '<div class="air-head air-' + (r.summary ? r.summary.kind : 'none') + '">' + x.head + '</div>';
    if (x.lines.length) h += '<ul class="air-lines">' + x.lines.map(l => '<li>' + l + '</li>').join('') + '</ul>';
    if (x.fixes.length && mayFix(r)) h += '<div class="air-fixes">' + x.fixes.map((f, i) => '<button class="btn" data-fix="' + i + '">' + f.label + '</button>').join('') + '</div>';
    if (v && (v.checks || []).length) {
      h += '<div class="air-legend"><span class="air-chip air-hit">counts</span><span class="air-chip air-low">wanted, not sure enough</span><span class="air-chip air-other">not on the alert list</span><span class="air-chip air-faint">faint guess</span></div>';
      h += '<div class="air-grid">' + v.checks.map(c => frameCard(r.event.id, c, v, again && again.find(a => a.n === c.n))).join('') + '</div>';
      if (state.isAdmin && v.checks.some(c => c.frame)) h += '<div class="row"><button class="btn" data-recheck>' + ic('swap') + ' Check these frames again with today\'s AI</button><span class="hint">After switching models or changing settings: nothing is saved.</span></div>';
    }
    const body = d.querySelector('[data-air]');
    body.innerHTML = h;
    body.querySelectorAll('.air-head a[href^="#"]').forEach(a => a.addEventListener('click', () => { if (d.__close) d.__close(); }));
    body.querySelectorAll('[data-fix]').forEach(b => { b.onclick = () => applyFix(x.fixes[+b.dataset.fix], b); });
    const rc = body.querySelector('[data-recheck]');
    if (rc) rc.onclick = async () => {
      rc.disabled = true; rc.textContent = 'Checking...';
      try { again = (await api('/api/events/' + encodeURIComponent(r.event.id) + '/ai/recheck', { json: {} })).checks || []; paint(); }
      catch (e) { toast('Could not check again: ' + errText(e)); rc.disabled = false; }
    };
  };
  async function applyFix(f, b) {
    const cam = r.camera;
    if (!cam || cam.id == null) { toast('That camera no longer exists'); return; }
    const body = f.kind === 'label' ? { ai_labels: (cam.labels || []).concat([f.value]) } : f.kind === 'strict' ? { ai_strict: f.value } : { ai_min_score: f.value };
    b.disabled = true;
    try {
      await api('/api/cameras/' + cam.id, { method: 'PUT', json: body });
      toast(f.kind === 'label' ? cam.name + ' now also alerts for ' + f.value : f.kind === 'strict' ? 'Stricter confirmation is off for ' + cam.name : cam.name + '\'s minimum is now ' + pct(f.value));
      r = await api('/api/events/' + encodeURIComponent(eventId) + '/ai');
      paint();
    } catch (e) { toast('Could not change it: ' + errText(e)); b.disabled = false; }
  }
  paint();
}

// "Not a person" (or "Not a car" ...): the event was not real, and AI remembers that
// object's spot on this camera. Returns the server's answer, or null.
export async function markNotReal(eventId, label) {
  try {
    const r = await api('/api/events/' + encodeURIComponent(eventId) + '/review', { json: { verdict: 'false_alarm', label } });
    const m = r.marked;
    toast(m && m.spots ? 'Marked as not real. ' + m.camera + ' will not alert for ' + an(kindWords(m.label)) + ' in ' + (m.spots === 1 ? 'that spot' : 'those ' + m.spots + ' spots') + ' for 30 days, unless it moves.'
      : 'Marked as not a real alert. ' + (m && m.why === 'one_look' ? 'AI looked only once in this event: too little to tell what stood still, so it remembers no spot.' : 'Nothing of that kind sat still in it, so AI remembers no spot (something that moves is never remembered).'), { kind: 'ok', ms: 5000 });
    return r;
  } catch (e) { toast('Could not mark it: ' + errText(e), { kind: 'err' }); return null; }
}

// Compact verdict for the Events page player: headline, the first reason, one-click
// fixes (admins; operators allowed to change the camera), "Not a person" marks (those who may
// mark events) and a button for the full frame-by-frame
// dialog. `opts.onReview(review)` hears of a mark. Returns a promise.
export async function renderVerdict(box, eventId, opts = {}) {
  box.innerHTML = '<span class="muted">AI review...</span>';
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
    h += kinds.map(l => '<button class="btn" data-notreal="' + esc(l) + '" title="AI got this wrong: if it sat still, AI will not alert for this ' + esc(kindOf(l)) + ' in this spot again for 30 days, unless it moves">' + ic('x') + '<span>' + esc(notA(l)) + '</span></button>').join('');
    if (r.review && (r.review.checks || []).length) h += '<button class="btn" data-frames>Frames and details</button>';
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
      if (!cam || cam.id == null) { toast('That camera no longer exists'); return; }
      const body = f.kind === 'label' ? { ai_labels: (cam.labels || []).concat([f.value]) } : f.kind === 'strict' ? { ai_strict: f.value } : { ai_min_score: f.value };
      b.disabled = true;
      try {
        await api('/api/cameras/' + cam.id, { method: 'PUT', json: body });
        toast(f.kind === 'label' ? cam.name + ' now also alerts for ' + f.value : f.kind === 'strict' ? 'Stricter confirmation is off for ' + cam.name : cam.name + '\'s minimum is now ' + pct(f.value), { kind: 'ok' });
        r = await api('/api/events/' + encodeURIComponent(eventId) + '/ai');
        paint();
      } catch (e) { toast('Could not change it: ' + errText(e), { kind: 'err' }); b.disabled = false; }
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
const FILTERS = [['all', 'All'], ['alerted', 'Alerts sent'], ['noalert', 'No alert'], ['near_miss', 'Almost'], ['strict', 'Not confirmed'], ['other_object', 'Other objects'], ['parked', 'Parked'], ['fixed_spot', 'Same spot'], ['camera_only', 'Camera only']];

function reviewPage(el) {
  let days = 7, cam = '', filter = 'all', data = null;
  el.innerHTML = '<div class="asys"><div class="asys-head"><span class="hint"><b>Optional.</b> Shows what the AI saw in each motion event, and why it did or did not alert. Click an event for the frames and one-click fixes. Use it when alerts are missing or false.</span></div><div data-ar><p class="empty">Loading...</p></div></div>';
  const box = el.querySelector('[data-ar]');
  stillWaiting(el.querySelector('.asys'), () => load());
  async function load() {
    // (One kind: the server looks for it through the whole period, not only the newest events.)
    try { data = await api('/api/ai/reviews?days=' + days + (cam ? '&camera=' + encodeURIComponent(cam) : '') + (filter !== 'all' ? '&kind=' + encodeURIComponent(filter) : '')); }
    catch (e) { box.innerHTML = '<p class="empty">Could not load: ' + esc(errText(e)) + '</p>'; return; }
    paint();
  }
  function paint() {
    const n = (o, k) => o[k] || 0;
    let h = '<div class="ar-bar"><select data-days>' + [1, 7, 30].map(x => '<option value="' + x + '"' + (x === days ? ' selected' : '') + '>Last ' + (x === 1 ? 'day' : x + ' days') + '</option>').join('') + '</select>' +
      '<select data-cam><option value="">All cameras</option>' + (data.cameras || []).map(c => '<option' + (c.camera === cam ? ' selected' : '') + '>' + esc(c.camera) + '</option>').join('') + '</select>' +
      '<span class="ar-chips">' + FILTERS.map(f => '<button class="chip' + (f[0] === filter ? ' on' : '') + '" data-f="' + f[0] + '">' + f[1] + '</button>').join('') + '</span></div>';
    if (!(data.cameras || []).length) {
      box.innerHTML = h + '<p class="empty">No AI-checked events in this period. AI review starts recording from now on; events from before have no AI record. Check that AI is on in Settings › Detection &amp; alerts › Objects (AI).</p>';
      bind(); return;
    }
    if (!cam) {
      h += '<div class="asys-scroll"><table class="asys-tbl ar-tbl"><thead><tr><th>Camera</th><th class="num">Checked</th><th class="num">Alerts sent</th><th class="num">Almost</th><th class="num">Other objects</th><th class="num">Parked</th><th class="num" title="Something that has been in the same spot in several earlier events, or that was marked not real there">Same spot</th><th class="num" title="The camera itself reported a person, vehicle or animal, and AI did not see one">Camera only</th><th class="num">Nothing found</th><th class="ar-opt">Alerts for</th><th class="num ar-opt">Minimum</th></tr></thead><tbody>' +
        data.cameras.map(c => '<tr data-pickcam="' + esc(c.camera) + '" class="ar-row"><td><b>' + esc(c.camera) + '</b></td><td class="num">' + n(c, 'total') + '</td><td class="num">' + n(c, 'alerted') + '</td>' +
          '<td class="num">' + (n(c, 'near_miss') ? '<b>' + n(c, 'near_miss') + '</b>' : 0) + '</td><td class="num">' + n(c, 'other_object') + '</td><td class="num">' + n(c, 'parked') + '</td><td class="num">' + n(c, 'fixed_spot') + '</td><td class="num">' + n(c, 'camera_only') + '</td><td class="num">' + n(c, 'nothing') + '</td>' +
          '<td class="ar-opt ar-for" title="' + esc(((c.now || {}).labels || []).join(', ')) + '">' + esc(((c.now || {}).labels || []).join(', ')) + '</td><td class="num ar-opt">' + (c.now && c.now.min_score != null ? pct(c.now.min_score) : '-') + '</td></tr>').join('') + '</tbody></table></div>' +
        (data.cameras.some(c => c.sampled < c.total) ? '<p class="hint">Checked and Alerts sent count the whole period. For ' + data.cameras.filter(c => c.sampled < c.total).map(c => esc(c.camera)).join(', ') + ', the other columns are based on the latest ' + data.per_camera_limit + ' events of each camera.</p>' : '') +
        '<p class="hint">"Almost" means AI saw something it alerts for but was not sure enough: worth a look if you are missing alerts. Many "Nothing found" usually means shadows, lighting or plants set off motion; a motion zone or mask can help.</p>';
    }
    const evs = (data.events || []).filter(e => filter === 'all' || (filter === 'noalert' ? !e.alerted : e.summary.kind === filter));
    const most = data.list_limit || 300;
    h += '<h4 class="asys-sub">Events' + (cam ? ' - ' + esc(cam) : '') + '</h4>' + ((data.events || []).length >= most ? '<p class="hint">Showing the latest ' + most + ' events' + (filter !== 'all' && data.kind ? ' of this kind' : '') + '.</p>' : '');
    h += evs.length ? '<div class="ar-list">' + evs.map(e => {
      const k = e.summary.kind === 'parked' && e.summary.best_parked && !VEHICLES.includes(e.summary.best_parked.label) ? ['Not moving', ''] : KIND[e.summary.kind] || ['?', ''];
      const b = e.summary.kind === 'parked' ? e.summary.best_parked : e.summary.kind === 'fixed_spot' ? e.summary.best_spot : e.summary.best_wanted && (e.summary.kind === 'alerted' || e.summary.kind === 'near_miss' || e.summary.kind === 'strict') ? e.summary.best_wanted : e.summary.best_other;
      return '<button class="ar-ev" data-ev="' + esc(e.id) + '"><img loading="lazy" src="/api/events/' + encodeURIComponent(e.id) + '/thumb" alt="">' +
        '<span class="ar-evt"><b>' + esc(e.camera) + '</b><span class="muted">' + esc(fmtTime(e.start)) + '</span>' +
        '<span><span class="badge ' + k[1] + '">' + k[0] + '</span> ' + (b ? esc(b.label) + ' ' + pct(b.score) : (e.summary.camera_labels || []).length ? 'camera: ' + esc(e.summary.camera_labels.join(', ')) : '<span class="muted">no objects</span>') + '</span></span></button>';
    }).join('') + '</div>' : '<p class="empty">No events match.</p>';
    box.innerHTML = h;
    bind();
  }
  function bind() {
    box.querySelector('[data-days]').onchange = (e) => { days = +e.target.value; load(); };
    box.querySelector('[data-cam]').onchange = (e) => { cam = e.target.value; load(); };
    // An older server ignores the kind and sends the latest events: they are filtered here as well.
    box.querySelectorAll('[data-f]').forEach(b => { b.onclick = () => { filter = b.dataset.f; if (data && data.kind !== undefined) load(); else paint(); }; });
    box.querySelectorAll('[data-pickcam]').forEach(t => { t.onclick = () => { cam = t.dataset.pickcam; load(); }; });
    box.querySelectorAll('[data-ev]').forEach(b => { b.onclick = () => openAiReview(b.dataset.ev); });
  }
  load();
}

registerAdminPage({ id: 'aireview', title: 'AI review', order: 65, render: reviewPage });
