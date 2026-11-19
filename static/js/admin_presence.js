// admin_presence.js - Settings > Schedules & presence > Presence.
// Phone apps report whether the phone is at home (POST /api/presence); when everyone has left the
// server arms or switches profile, and when someone comes back it switches back. This page holds
// the [presence] settings (saved with POST /api/settings) and lists the phones (GET /api/presence).

import { t } from './i18n.js';
import { api, esc, toast, msgDlg, enc, swHtml, fld, liveSwitch, fmtAgo } from './core.js';
import { registerAdminPage } from './admin.js';
import { lead, how, more } from './sp_kit.js';
import { head, pageBar, wireBar, postSettings, guardAdmin } from './admin_config.js';

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const errText = (e) => (e && e.body) || (e && e.message) || String(e);

// The settings as compared and saved (unset arm_when_away = null).
function norm(p) {
  p = p || {};
  return {
    enabled: !!p.enabled,
    away_profile: p.away_profile || '',
    home_profile: p.home_profile || '',
    arm_when_away: p.arm_when_away == null ? null : !!p.arm_when_away,
    disarm_when_home: !!p.disarm_when_home,
    away_delay_minutes: p.away_delay_minutes == null ? 10 : +p.away_delay_minutes,
    stale_hours: p.stale_hours == null ? 12 : +p.stale_hours,
    accounts: (p.accounts || []).slice().sort(),
  };
}
const armsAway = (d) => (d.arm_when_away == null ? !d.away_profile : d.arm_when_away);

function ago(ms) { return fmtAgo((Date.now() - ms) / 1000); }

function presencePage(el) {
  if (!guardAdmin(el)) return null;
  let saved = null, draft = null, profiles = [], users = [], live = null, timer = 0;
  el.innerHTML = '<div class="ac"><p class="empty">' + t('admin_presence.loading','Loading...') + '</p></div>';

  async function load() {
    try {
      const [s, p, u] = await Promise.all([api('/api/settings'), api('/api/presence'), api('/api/users').catch(() => ({ users: [] }))]);
      saved = norm(s.presence);
      draft = Object.assign({}, saved, { accounts: saved.accounts.slice() });
      users = (u.users || []).map(x => x.username);
      profiles = ((s.automation && s.automation.profiles) || []).map(x => x.name);
      live = p;
      paint();
    } catch (e) { el.innerHTML = '<div class="ac"><p class="empty">' + esc(errText(e)) + '</p></div>'; }
  }
  async function loadLive() {
    if (!el.isConnected) return;
    try { live = await api('/api/presence'); paintLive(); } catch (e) { /* try again next time */ }
  }

  function profileOpts(cur, extra) {
    const names = profiles.slice();
    if (cur && !names.includes(cur)) names.push(cur);
    return extra + names.map(n => '<option value="p:' + esc(n) + '"' + (n === cur ? ' selected' : '') + '>' + esc(n) + (profiles.includes(n) ? '' : ' (missing)') + '</option>').join('');
  }

  function paint() {
    const d = draft;
    const awayVal = d.away_profile ? 'p:' + d.away_profile : (armsAway(d) ? 'arm' : 'none');
    let h = lead(t('admin_presence.arm_desc','Arm AmberSight when everyone\'s phones have left home, and switch back when someone returns.'), t('admin_presence.off_until','Off until you turn it on. Use it if your family uses the phone app.')) +
      how(t('admin_presence.phone_expl','Phone apps tell this server whether the phone is at home. When everyone has left, the server arms or switches profile; when someone comes back, it switches back. Only those changes act, so arming or switching by hand stays until the next one. No Home Assistant needed.')) +
      '<div class="ac-card ac-live" data-live></div>' +
      '<div class="ac-card">' + swHtml('data-en', d.enabled, t('admin_presence.arm_label','Arm and disarm by phones at home or away'), t('admin_presence.off_default','Off by default. Phones keep reporting while this is off; nothing changes until it is on.')) +
      '<div class="ac-grid3">' +
      '<label>' + t('admin_presence.when_all_left','When everyone has left ') + '<select data-away>' +
        profileOpts(d.away_profile, '<option value="arm"' + (awayVal === 'arm' ? ' selected' : '') + '>' + t('admin_presence.arm_option','Arm') + '</option><option value="none"' + (awayVal === 'none' ? ' selected' : '') + '>' + t('admin_presence.do_nothing','Do nothing') + '</option>') + '</select></label>' +
      '<label>' + t('admin_presence.when_someone_back','When someone comes back ') + '<select data-home>' +
        profileOpts(d.home_profile, '<option value=""' + (d.home_profile ? '' : ' selected') + '>' + (d.away_profile ? t('admin_presence.back_schedule','Back to the schedule') : t('admin_presence.leave_profile','Leave the profile as is')) + '</option>') + '</select></label>' +
      '</div>' +
      (d.away_profile ? swHtml('data-alsoarm', armsAway(d), t('admin_presence.also_arm','Also arm when everyone has left')) : '') +
      swHtml('data-disarm', d.disarm_when_home, t('admin_presence.disarm','Disarm when someone comes back')) +
      more(t('admin_presence.more_settings','More settings'), fld(t('admin_presence.after_away','After everyone is away for'), 'data-delay min="0" max="120" step="1" value="' + d.away_delay_minutes + '"', t('admin_presence.minutes', 'minutes'), t('admin_presence.delay_hint', 'Wait this long before acting, in case a phone only lost its signal.')) +
      fld(t('admin_presence.ignore_silent','Ignore a phone that has been silent for'), 'data-stale min="1" max="168" step="1" value="' + d.stale_hours + '"', t('admin_presence.hours', 'hours'), t('admin_presence.stale_hint', 'A phone that has not reported for that long no longer counts. With no phone counting, the server does nothing now and remembers who was last home or away.')) +
      '<div class="hint">' + t('admin_presence.hint_any_counted','Any counted phone coming back does this (and the home profile). A home profile stays until changed by hand or the next time everyone leaves; with none, the server goes back to the schedule. Leaving replaces a profile chosen by hand.') + '</div>') + '</div>' +
      (users.length ? '<div class="ac-card"><div class="ac-card-t">' + t('admin_presence.phones_of_accounts','Phones of these accounts count') + '</div><div class="ac-row">' +
        users.concat(d.accounts.filter(n => !users.includes(n))).map(n => '<label class="ac-switch"' + (users.includes(n) ? '' : ' title="' + t('admin_presence.no_such_account','No such account') + '"') + '><input type="checkbox" data-acct="' + esc(n) + '"' + (d.accounts.includes(n) ? ' checked' : '') + '> ' + esc(n) + (users.includes(n) ? '' : t('admin_presence.no_account',' (no account)')) + '</label>').join('') +
        '</div><div class="hint">' + t('admin_presence.none_ticked','None ticked: every account\'s phones count. Phones of other accounts are not counted, and those accounts cannot report.') + '</div></div>' : '') +
      '<div class="ac-card"><div class="ac-row"><div class="ac-card-t">' + t('admin_presence.phones','Phones') + '</div><span class="ac-sp"></span><button class="btn" data-refresh>' + t('admin_presence.refresh','Refresh') + '</button></div><div data-devs></div></div>' +
      pageBar();
    el.innerHTML = '<div class="ac">' + h + '</div>';
    bind();
    paintLive();
  }

  function paintLive() {
    const box = el.querySelector('[data-live]'), devs = el.querySelector('[data-devs]');
    if (!box || !live) return;
    const st = live.state;
    const pill = st === 'home' ? '<span class="ac-pill" style="--h:150">' + t('admin_presence.someone_home','Someone is home') + '</span>'
      : st === 'away' ? '<span class="ac-pill" style="--h:38">' + t('admin_presence.everyone_away','Everyone is away') + '</span>'
        : '<span class="ac-pill none">' + t('admin_presence.unknown','Unknown') + '</span>';
    let note = !live.enabled ? t('admin_presence.presence_off','presence is off') : st === 'unknown' ? t('admin_presence.no_report','no phone has reported in the last {h} h', { h: live.stale_hours }) : '';
    if (live.pending_away_at) note = t('admin_presence.away_action_full','away action in {m} min unless someone comes back', { m: Math.max(1, Math.ceil((live.pending_away_at - Date.now()) / 60000)) });
    const la = live.last_action;
    box.innerHTML = '<div class="ac-live-main"><span class="ac-label">' + t('admin_presence.now', 'Now') + '</span>' + pill + (note ? '<span class="hint ac-inl">' + esc(note) + '</span>' : '') + '</div>' +
      (la ? '<div class="hint ac-inl">' + t('admin_presence.last_action', 'Last: {what}, {did} ({ago})', { what: esc(la.event === 'away' ? t('admin_presence.everyone_left','everyone left') : t('admin_presence.someone_came_back','someone came back')), did: esc(la.did.replace(/\+/g, ', ').replace(/profile:/g, 'profile ')), ago: esc(ago(la.at_ms)) }) + '</div>' : '');
    const list = live.devices || [];
    devs.innerHTML = !list.length ? '<p class="hint">' + t('admin_presence.no_phones','No phones yet. A phone app signed in to this server reports here; any account can, admin or not.') + '</p>'
      : '<div class="asys-scroll"><table class="asys-tbl"><thead><tr><th>' + t('admin_presence.header_account','Account') + '</th><th>' + t('admin_presence.header_phone','Phone') + '</th><th>' + t('admin_presence.header_where','Where') + '</th><th>' + t('admin_presence.header_last_report','Last report') + '</th><th></th></tr></thead><tbody>' +
        list.map((x, i) => {
          const off = x.counted === false;
          const title = off ? t('admin_presence.not_counted_account','Not counted: this account\'s phones do not count') : x.stale ? t('admin_presence.not_counted_silent','Not counted: silent for more than {h} h', { h: live.stale_hours }) : '';
          return '<tr' + (title ? ' class="muted" title="' + esc(title) + '"' : '') + '><td>' + esc(x.user || '-') + '</td><td>' + esc(x.name) + '</td>' +
          '<td>' + (off ? '<span class="badge paused">' + t('admin_presence.badge_not_counted', 'not counted') + '</span>' : x.stale ? '<span class="badge paused">' + t('admin_presence.badge_stale', 'stale') + '</span>' : '<span class="badge ' + (x.home ? 'on' : 'info') + '">' + (x.home ? t('admin_presence.badge_home','home') : t('admin_presence.badge_away','away')) + '</span>') + '</td>' +
          '<td class="nowrap" title="' + esc(new Date(x.updated_ms).toLocaleString()) + '">' + esc(ago(x.updated_ms)) + '</td>' +
          '<td class="asys-act"><button class="btn danger" data-rm="' + i + '">' + t('admin_presence.remove','Remove') + '</button></td></tr>';
        }).join('') + '</tbody></table></div>';
    devs.querySelectorAll('[data-rm]').forEach(b => {
      b.onclick = async () => {
        const x = list[+b.dataset.rm];
        if (!x) return;
        b.disabled = true;
        try {
          await api('/api/presence/' + enc(x.device_id) + '?user=' + enc(x.user), { method: 'DELETE' });
          toast(t('admin_presence.removed','Removed {name}', { name: x.name }));
        } catch (e) { msgDlg(errText(e)); }
        loadLive();
      };
    });
  }

  let upd = () => {};
  function bind() {
    const q = (s) => el.querySelector(s);
    upd = wireBar(el, () => !same(norm(draft), saved), async () => {
      await postSettings({ presence: norm(draft) });
      await load();
    }, () => { draft = Object.assign({}, saved, { accounts: saved.accounts.slice() }); paint(); });
    // On/off settings save when flipped (only that value is sent).
    const flip = (sel, make, apply) => { const i = q(sel); if (!i) return; liveSwitch(i, async (on) => { await postSettings({ presence: make(on) }); apply(saved, on); apply(draft, on); upd(); }); };
    flip('[data-en]', (on) => ({ enabled: on }), (o, on) => { o.enabled = on; });
    flip('[data-alsoarm]', (on) => ({ arm_when_away: on ? true : null }), (o, on) => { o.arm_when_away = on ? true : null; });
    flip('[data-disarm]', (on) => ({ disarm_when_home: on }), (o, on) => { o.disarm_when_home = on; });
    q('[data-away]').onchange = (e) => {
      const v = e.target.value;
      if (v === 'arm') { draft.away_profile = ''; draft.arm_when_away = null; }
      else if (v === 'none') { draft.away_profile = ''; draft.arm_when_away = false; }
      else { draft.away_profile = v.slice(2); draft.arm_when_away = null; }
      paint(); upd();
    };
    q('[data-home]').onchange = (e) => { draft.home_profile = e.target.value ? e.target.value.slice(2) : ''; upd(); };
    el.querySelectorAll('[data-acct]').forEach(c => {
      c.onchange = () => {
        const n = c.dataset.acct, a = draft.accounts.filter(x => x !== n);
        if (c.checked) a.push(n);
        draft.accounts = a.sort();
        upd();
      };
    });
    const num = (sel, key, lo, hi) => {
      const i = q(sel);
      i.onchange = () => { const v = Math.round(+i.value); draft[key] = isFinite(v) ? Math.min(hi, Math.max(lo, v)) : saved[key]; i.value = draft[key]; upd(); };
    };
    num('[data-delay]', 'away_delay_minutes', 0, 120);
    num('[data-stale]', 'stale_hours', 1, 168);
    q('[data-refresh]').onclick = loadLive;
  }

  load();
  timer = setInterval(loadLive, 30000);
  return () => clearInterval(timer);
}

let stop = null;
registerAdminPage({ id: 'presence', title: t('admin_presence.presence_phones', 'Presence (phones)'), order: 52, render(el) { stop = presencePage(el); }, leave() { if (stop) stop(); stop = null; } });
