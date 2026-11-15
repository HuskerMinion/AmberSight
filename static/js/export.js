// export.js - the export dialog (download a time range as MP4), used by the live timeline.
import { state, enc, esc, fmtWall, toLocalInput, openDlg, msgDlg, downloadUrl } from './core.js';

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
    '<button class="primary" data-go="1">Download MP4</button>');
  d.querySelector('[data-go]').onclick = async () => {
    const cam = d.querySelector('#exCam').value;
    const a = new Date(d.querySelector('#exFrom').value).getTime();
    const b = new Date(d.querySelector('#exTo').value).getTime();
    if (!(a > 0) || !(b > a)) { msgDlg('End must be after start'); return; }
    const url = (n) => '/api/playback?camera=' + enc(n) + '&from=' + a + '&to=' + b + '&download=1';
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

