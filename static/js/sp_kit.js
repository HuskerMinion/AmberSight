// sp_kit.js - small markup helpers shared by the Settings pages, so every page opens the same way:
// one plain sentence of what it does (lead), an optional "Recommended: ..." line, a "How this works"
// disclosure for the long explanation, and expert-only fields folded under "More settings" / "Advanced".
import { esc } from './core.js';

// One sentence, then (optionally) what to do. Both are HTML (already escaped by the caller).
export const lead = (text, recommended) => '<p class="sp-lead sp-one">' + text + '</p>' +
  (recommended ? '<p class="sp-rec"><b>Recommended:</b> ' + recommended + '</p>' : '');
// A collapsed group of fields or text. title is HTML.
export const more = (title, html, open) => '<details class="ac-more sp-more"' + (open ? ' open' : '') + '><summary>' + title + '</summary>' + html + '</details>';
// The long explanation, collapsed.
export const how = (html) => more('How this works', '<div class="hint sp-how">' + html + '</div>');
// A notice box. kind: 'warn' | 'bad' | 'info' | 'ok'. action is ready-made button HTML (optional).
export const notice = (kind, html, action) => '<div class="sp-note ' + kind + '" role="' + (kind === 'bad' ? 'alert' : 'status') + '"><span class="sp-note-t">' + html + '</span>' + (action || '') + '</div>';
// A segmented control (radio group): opts = [[value, label], ...]. Pass 'data-nt' as extra to keep
// the save bar from watching the radios themselves (when the fields they set are watched).
export function seg(name, label, opts, val, extra) {
  return '<div class="segc" role="radiogroup" aria-label="' + esc(label) + '">' + opts.map(([v, t]) =>
    '<label class="segc-o"><input type="radio" name="' + esc(name) + '" value="' + esc(v) + '" ' + (extra || '') + (String(v) === String(val) ? ' checked' : '') + '><span>' + esc(t) + '</span></label>').join('') + '</div>';
}
// Tabs semantics for a chip in a sub-page strip (the container gets role="tablist").
export const tabAttrs = (on) => ' role="tab" aria-selected="' + (on ? 'true' : 'false') + '"';
// A tick, for "OK" states (the sprite has no check mark).
export const okIc = '<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7"/></svg>';

// Motion sensitivity levels, shared by Settings and the camera panel. The fields are the Settings
// field names; values are in the units the forms show (% of the picture, 0-255, frames). Medium is
// the built-in default (see src/config.rs). Lower "smallest object", "contrast", "minimum movement"
// and "frames in a row" all mean more sensitive; the Simple method's threshold runs the other way.
export const MOTION_PRESETS = {
  low: { stMinObj: 0.5, stContrast: 40, stTravel: 6, stMinFrames: 5, stSens: 0.2 },
  medium: { stMinObj: 0.2, stContrast: 25, stTravel: 3, stMinFrames: 3, stSens: 0.12 },
  high: { stMinObj: 0.1, stContrast: 15, stTravel: 1.5, stMinFrames: 2, stSens: 0.06 },
};

// Arrow keys for a strip of role="tab" buttons: Left/Right (Up/Down too) move to the next or previous
// tab and open it, Home/End go to the first/last, and focus follows. Tab still moves through them all.
// Only keys pressed on a tab are handled, so typing and the app's own keys elsewhere are untouched.
export function tablistKeys(list) {
  list.addEventListener('keydown', (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey || e.shiftKey) return;
    const cur = e.target.closest && e.target.closest('[role="tab"]');
    if (!cur || cur.parentNode !== list) return;
    const tabs = [...list.querySelectorAll(':scope > [role="tab"]')].filter(t => !t.disabled && t.offsetParent !== null);
    const i = tabs.indexOf(cur);
    const rtl = getComputedStyle(list).direction === 'rtl';
    const step = { ArrowRight: rtl ? -1 : 1, ArrowLeft: rtl ? 1 : -1, ArrowDown: 1, ArrowUp: -1 }[e.key];
    let n = -1;
    if (step) n = (i + step + tabs.length) % tabs.length;
    else if (e.key === 'Home') n = 0;
    else if (e.key === 'End') n = tabs.length - 1;
    if (n < 0 || i < 0) return;
    e.preventDefault(); e.stopPropagation();
    if (n === i) return;
    const at = n;
    tabs[n].click();
    // The page may redraw the strip: put focus on the tab now at the same place.
    const refocus = () => { const t = [...list.querySelectorAll(':scope > [role="tab"]')].filter(x => !x.disabled && x.offsetParent !== null)[at]; if (t && document.activeElement !== t) t.focus(); };
    tabs[n].focus();
    setTimeout(refocus, 0); setTimeout(refocus, 120);
  });
}
