// viewfit.js - plans the window-filling Live views.
//
// Styles: 'grid' (equal tiles, the page scrolls), 'fit' (equal tiles sized to fill the
// window), 'focus' (the first camera large, the rest around it), 'focus2' (the first two
// large). plan() picks the columns, rows and large-tile span that show the most picture:
// every option is scored by the total visible picture area at the cameras' aspect ratio.

export const STYLES = ['grid', 'fit', 'focus', 'focus2'];
export const BIG = { grid: 0, fit: 0, focus: 1, focus2: 2 };

// Visible picture area of an aspect-`a` picture letterboxed into a w x h cell.
function pic(w, h, a) {
  const pw = Math.min(w, h * a);
  return pw * (pw / a);
}

// n tiles in a W x H box with `gap` px between cells. Returns { cols, rows, k, big }:
// a cols x rows grid where the first `big` tiles each span k x k cells.
// `a` is one aspect ratio for every camera, or an array with each camera's own (tile order).
export function plan(n, W, H, gap, style, a) {
  const arr = Array.isArray(a) ? a : null;
  const at = (i) => (arr ? arr[i] : a) || 16 / 9;
  let big = Math.min(BIG[style] || 0, n);
  // Focus needs other cameras to place around the large one(s).
  if (big && n - big < 1) big = 0;
  let best = null;
  const ks = big ? [2, 3, 4] : [1];
  for (const k of ks) {
    const cells = big * k * k + (n - big);
    // (As few columns as the large tiles' own width: on a portrait screen they stack
    // across the full width with the rest below; the score picks the better shape.)
    const minC = Math.max(1, k);
    for (let C = minC; C <= Math.max(minC, cells); C++) {
      const R = Math.max(big ? k : 1, Math.ceil(cells / C));
      const cw = (W - (C - 1) * gap) / C, ch = (H - (R - 1) * gap) / R;
      if (cw < 48 || ch < 32) continue;
      const bw = k * cw + (k - 1) * gap, bh = k * ch + (k - 1) * gap;
      let area = 0;
      for (let i = 0; i < n; i++) area += i < big ? pic(bw, bh, at(i)) : pic(cw, ch, at(i));
      // Small penalty per empty cell keeps the window tidy when scores are close.
      const score = area * (1 - 0.01 * (C * R - cells));
      if (!best || score > best.score) best = { cols: C, rows: R, k, big, score };
    }
  }
  return best || { cols: 1, rows: n, k: 1, big: 0, score: 0 };
}
