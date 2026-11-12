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
  d.querySelector('[data-go]').onclick = () => {
    const cam = d.querySelector('#exCam').value;
    const a = new Date(d.querySelector('#exFrom').value).getTime();
    const b = new Date(d.querySelector('#exTo').value).getTime();
    if (!(a > 0) || !(b > a)) { msgDlg('End must be after start'); return; }
    const one = (n) => downloadUrl('/api/playback?camera=' + enc(n) + '&from=' + a + '&to=' + b + '&download=1', n + '-' + fmtWall(a).replace(/[: ]/g, '-') + '.mp4');
    if (cam === '*') cams.forEach((n, i) => setTimeout(() => one(n), i * 800));
    else one(cam);
    d.__close();
  };
}

