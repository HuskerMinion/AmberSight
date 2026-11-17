// export.js - the export dialog (download a time range as MP4), used by the live timeline.
import { state, enc, esc, fmtWall, toLocalInput, openDlg, msgDlg, downloadUrl } from './core.js';

// "Burn the text into this clip": a switch for every place clips are
// downloaded. A downloaded clip has the time as a subtitle track; burning
// the text in as well re-encodes the clip, so it is off unless asked for.
// Remembered per browser (the same choice for every download).
const BURN_KEY = 'ambs.burnText';
export function burnText() { try { return localStorage.getItem(BURN_KEY) === '1'; } catch (e) { return false; } }
function setBurnText(on) { try { if (on) localStorage.setItem(BURN_KEY, '1'); else localStorage.removeItem(BURN_KEY); } catch (e) { /* not kept */ } }
export function burnTextSwitch(short) {
  return '<label class="sw' + (short ? ' sw-burn' : '') + '"><input type="checkbox" role="switch" data-burntext' + (burnText() ? ' checked' : '') + '><span class="sw-track" aria-hidden="true"></span>' +
    '<span class="sw-text"><b>Burn the text into this clip</b>' + (short ? '' : '<small>The camera\'s text on the picture (or its name and the time) becomes part of the video. Takes longer: the clip is re-encoded. Without it the time comes as subtitles a player can show.</small>') + '</span></label>';
}
// Wires the switches in `root` (saved when flipped); onChange() after each flip.
export function wireBurnText(root, onChange) {
  root.querySelectorAll('[data-burntext]').forEach(i => { i.onchange = () => { setBurnText(i.checked); if (onChange) onChange(); }; });
}
// '&burn_text=1' (or '?burn_text=1' with first) when the text is to be burned in.
export const burnTextQuery = (first) => (burnText() ? (first ? '?' : '&') + 'burn_text=1' : '');

// opts = { cams: [names shown], camera, from, to } (ms) for the marked range.
// The server saves one camera per MP4, so with several cameras the list also
// offers "all", which downloads one file per camera.
export function exportDialog(opts) {
  opts = opts || {};
  const cams = opts.cams && opts.cams.length ? opts.cams : state.cams.map(c => c.name);
  const mid = Date.now() - 60000;
  const pick = opts.camera || (opts.cams && opts.cams.length > 1 ? '*' : undefined);
  const all = opts.cams && opts.cams.length > 1;
  const sel = (all ? '<option value="*"' + (pick === '*' ? ' selected' : '') + '>All ' + cams.length + ' cameras (one file each)</option>' : '') +
    cams.map(n => '<option' + (pick === n ? ' selected' : '') + '>' + esc(n) + '</option>').join('');
  const from = opts.from != null ? opts.from : mid - 30000;
  const to = opts.to != null ? opts.to : mid + 30000;
  const d = openDlg('Export recording',
    '<label>Camera <select id="exCam" style="width:100%">' + sel + '</select></label>' +
    '<label>Start <input id="exFrom" type="datetime-local" step="1" value="' + toLocalInput(from) + '"></label>' +
    '<label>End <input id="exTo" type="datetime-local" step="1" value="' + toLocalInput(to) + '"></label>' +
    '<div class="hint">' + (opts.from != null ? 'The range you marked on the timeline. ' : 'Defaults to 30 s either side of the playhead. ') + 'The saved MP4 starts at the nearest earlier keyframe.</div>' +
    burnTextSwitch(false) +
    '<button class="primary" data-go="1">Download MP4</button>');
  wireBurnText(d);
  d.querySelector('[data-go]').onclick = async () => {
    const cam = d.querySelector('#exCam').value;
    const a = new Date(d.querySelector('#exFrom').value).getTime();
    const b = new Date(d.querySelector('#exTo').value).getTime();
    if (!(a > 0) || !(b > a)) { msgDlg('End must be after start'); return; }
    const url = (n) => '/api/playback?camera=' + enc(n) + '&from=' + a + '&to=' + b + '&download=1' + burnTextQuery();
    const one = (n) => downloadUrl(url(n), n + '-' + fmtWall(a).replace(/[: ]/g, '-') + '.mp4');
    // Ask first (nothing is made): a camera whose recordings can only be
    // exported as saved event clips says so instead of a failed download.
    // Returns the reason, or '' when the download can go ahead.
    const why = async (n) => {
      try {
        const r = await fetch(url(n) + '&check=1');
        if (r.ok || r.status >= 500) return '';
        return n + ': ' + ((await r.text()) || 'nothing is recorded in that range');
      } catch (e) { return ''; }
    };
    const list = cam === '*' ? cams : [cam];
    const reasons = [];
    const good = [];
    for (const n of list) { const w = await why(n); if (w) reasons.push(w); else good.push(n); }
    // One dialog for all the cameras that cannot be exported.
    if (reasons.length) msgDlg(reasons.join('  '));
    if (!good.length) return;
    good.forEach((n, i) => setTimeout(() => one(n), i * 800));
    d.__close();
  };
}

