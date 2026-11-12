// lowdelay.js - helpers for the live (MSE) player: show frames as soon as they
// are decoded (lowDelayInit), and play them at an even pace (Dejitter).
//
// Many cameras (Reolink, for one) send an H.264 SPS that says level 5.1 and
// carries no VUI "bitstream_restriction". A decoder must then assume frames may
// be reordered and hold up to the level's whole picture buffer before showing
// the first one: 16 frames for a small sub stream (about 1 s at 15 fps, over
// 2 s at 7 fps). Hardware decoders (Edge/Chrome on Windows) do exactly that;
// the live player keeps only ~1 s buffered, so such a tile froze and jumped.
// These streams have no B-frames, so lowDelayInit() adds the restriction
// (max_num_reorder_frames = 0) to the SPS in the init segment's avcC box, and
// the decoder outputs each frame at once. Anything it does not understand is
// returned unchanged.
// The server now does the same (src/ingest/sps.rs) once a stream's frames
// have shown it has no B-frames, so an init segment usually arrives patched:
// lowDelaySps() sees the restriction and leaves it alone. This stays for
// older servers and for the first viewers of a stream that just started.

// Containers on the path moov/trak/mdia/minf/stbl/stsd/avc1/avcC, with the
// offset of their first child box.
const CHILD_AT = { moov: 8, trak: 8, mdia: 8, minf: 8, stbl: 8, stsd: 16, avc1: 86, avc3: 86 };
const fourcc = (b, o) => String.fromCharCode(b[o], b[o + 1], b[o + 2], b[o + 3]);
const rd32 = (b, o) => ((b[o] << 24) | (b[o + 1] << 16) | (b[o + 2] << 8) | b[o + 3]) >>> 0;
const wr32 = (b, o, v) => { b[o] = v >>> 24; b[o + 1] = (v >>> 16) & 255; b[o + 2] = (v >>> 8) & 255; b[o + 3] = v & 255; };

// Finds the avcC box: { at, size, parents: [offsets of the boxes holding it] }.
function findAvcC(b) {
  const walk = (start, end, parents) => {
    for (let o = start; o + 8 <= end;) {
      const size = rd32(b, o), type = fourcc(b, o + 4);
      if (size < 8 || o + size > end) return null;
      if (type === 'avcC') return { at: o, size, parents };
      if (CHILD_AT[type] && CHILD_AT[type] < size) {
        const r = walk(o + CHILD_AT[type], o + size, parents.concat(o));
        if (r) return r;
      }
      o += size;
    }
    return null;
  };
  return walk(0, b.length, []);
}

class Bits {
  constructor(bytes) { this.b = bytes; this.p = 0; }
  u(n) { let v = 0; for (let i = 0; i < n; i++) { if (this.p >= this.b.length * 8) throw new Error('eof'); v = v * 2 + ((this.b[this.p >> 3] >> (7 - (this.p & 7))) & 1); this.p++; } return v; }
  ue() { let z = 0; while (this.u(1) === 0) { if (++z > 31) throw new Error('bad ue'); } return z ? (2 ** z - 1 + this.u(z)) : 0; }
  se() { const k = this.ue(); return k & 1 ? (k + 1) / 2 : -k / 2; }
}
class BitOut {
  constructor() { this.bits = []; }
  u(n, v) { for (let i = n - 1; i >= 0; i--) this.bits.push(Math.floor(v / 2 ** i) & 1); }
  ue(v) { const x = v + 1, n = Math.floor(Math.log2(x)); this.u(n, 0); this.u(n + 1, x); }
  bytes() { const out = new Uint8Array(Math.ceil(this.bits.length / 8)); this.bits.forEach((bit, i) => { if (bit) out[i >> 3] |= 0x80 >> (i & 7); }); return out; }
}
const unescape = (n) => { const o = []; let zeros = 0; for (const x of n) { if (zeros >= 2 && x === 3) { zeros = 0; continue; } o.push(x); zeros = x === 0 ? zeros + 1 : 0; } return Uint8Array.from(o); };
const escape = (r) => { const o = []; let zeros = 0; for (const x of r) { if (zeros >= 2 && x <= 3) { o.push(3); zeros = 0; } o.push(x); zeros = x === 0 ? zeros + 1 : 0; } return Uint8Array.from(o); };

function skipHrd(r) {
  const cnt = r.ue(); r.u(4); r.u(4);
  for (let i = 0; i <= cnt; i++) { r.ue(); r.ue(); r.u(1); }
  r.u(20);
}

// Returns the SPS NAL (header byte included) with bitstream_restriction added,
// or null when it already has one, needs none, or cannot be parsed.
export function lowDelaySps(nal) {
  const rbsp = unescape(nal.subarray(1));
  const r = new Bits(rbsp);
  const profile = r.u(8); r.u(16); r.ue();
  if ([100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].includes(profile)) {
    const chroma = r.ue(); if (chroma === 3) r.u(1);
    r.ue(); r.ue(); r.u(1);
    if (r.u(1)) {
      for (let i = 0; i < (chroma !== 3 ? 8 : 12); i++) {
        if (!r.u(1)) continue;
        let last = 8, next = 8;
        for (let j = 0; j < (i < 6 ? 16 : 64); j++) { if (next !== 0) { next = (last + r.se() + 256) % 256; } last = next === 0 ? last : next; }
      }
    }
  }
  r.ue();
  const poc = r.ue();
  if (poc === 0) r.ue();
  else if (poc === 1) { r.u(1); r.se(); r.se(); const n = r.ue(); for (let i = 0; i < n; i++) r.se(); }
  // Picture order type 2 already means "no reordering".
  if (poc === 2) return null;
  const refs = r.ue();
  r.u(1); r.ue(); r.ue();
  if (!r.u(1)) r.u(1);
  r.u(1);
  if (r.u(1)) { r.ue(); r.ue(); r.ue(); r.ue(); }
  const vuiAt = r.p;
  const out = new BitOut();
  const copy = (n) => { const c = new Bits(rbsp); c.p = 0; for (let i = 0; i < n; i++) out.u(1, c.u(1)); };
  if (!r.u(1)) {
    // No VUI: add one that holds only the restriction.
    copy(vuiAt); out.u(1, 1);
    out.u(8, 0);   // aspect ratio, overscan, signal type, chroma loc, timing, nal hrd, vcl hrd, pic struct: absent
  } else {
    if (r.u(1)) { if (r.u(8) === 255) r.u(32); }
    if (r.u(1)) r.u(1);
    if (r.u(1)) { r.u(4); if (r.u(1)) r.u(24); }
    if (r.u(1)) { r.ue(); r.ue(); }
    if (r.u(1)) r.u(65);
    const nal = r.u(1); if (nal) skipHrd(r);
    const vcl = r.u(1); if (vcl) skipHrd(r);
    if (nal || vcl) r.u(1);
    r.u(1);
    if (r.u(1)) return null;   // already restricted
    copy(r.p - 1);
  }
  out.u(1, 1);                 // bitstream_restriction_flag
  out.u(1, 1);                 // motion_vectors_over_pic_boundaries_flag
  out.ue(2); out.ue(1);        // max_bytes_per_pic_denom, max_bits_per_mb_denom (the defaults)
  out.ue(15); out.ue(15);      // log2_max_mv_length_horizontal/vertical (the defaults)
  out.ue(0);                   // max_num_reorder_frames
  out.ue(Math.max(1, refs));   // max_dec_frame_buffering
  out.u(1, 1);                 // rbsp_stop_one_bit (bytes() pads with zeros)
  const body = escape(out.bytes());
  const res = new Uint8Array(body.length + 1);
  res[0] = nal[0]; res.set(body, 1);
  return res;
}

// Returns the init segment with low-delay SPSs, or the original bytes.
export function lowDelayInit(init) {
  try {
    const b = init instanceof Uint8Array ? init : new Uint8Array(init);
    const f = findAvcC(b);
    if (!f) return init;
    const a = b.subarray(f.at + 8, f.at + f.size);
    const nsps = a[5] & 31;
    let o = 6; const sps = [];
    for (let i = 0; i < nsps; i++) { const n = (a[o] << 8) | a[o + 1]; sps.push(a.subarray(o + 2, o + 2 + n)); o += 2 + n; }
    const rest = a.subarray(o);
    const fixed = sps.map(s => lowDelaySps(s));
    if (!fixed.some(Boolean)) return init;
    const parts = [a.subarray(0, 6)];
    sps.forEach((s, i) => { const n = fixed[i] || s; parts.push(Uint8Array.of(n.length >> 8, n.length & 255), n); });
    parts.push(rest);
    const body = parts.reduce((s, p) => s + p.length, 0);
    const delta = body + 8 - f.size;
    const res = new Uint8Array(b.length + delta);
    res.set(b.subarray(0, f.at), 0);
    let w = f.at;
    res.set(b.subarray(f.at, f.at + 8), w); wr32(res, w, body + 8); w += 8;
    for (const p of parts) { res.set(p, w); w += p.length; }
    res.set(b.subarray(f.at + f.size), w);
    for (const p of f.parents) wr32(res, p, rd32(b, p) + delta);
    return res;
  } catch (e) {
    return init;
  }
}

// True when a media fragment (moof+mdat) gives any sample a composition time
// offset, i.e. the stream has B-frames and its frames must not lose reordering.
export function hasCompositionOffsets(frag) {
  const b = frag;
  const scan = (start, end) => {
    for (let o = start; o + 8 <= end;) {
      const size = rd32(b, o), type = fourcc(b, o + 4);
      if (size < 8 || o + size > end) return false;
      if (type === 'moof' || type === 'traf') { if (scan(o + 8, o + size)) return true; }
      else if (type === 'trun') {
        const flags = rd32(b, o + 8) & 0xffffff;
        if (flags & 0x800) {
          const n = rd32(b, o + 12);
          const first = o + 16 + (flags & 1 ? 4 : 0) + (flags & 4 ? 4 : 0);
          const step = 4 * [0x100, 0x200, 0x400, 0x800].filter(x => flags & x).length;
          const cto = first + 4 * [0x100, 0x200, 0x400].filter(x => flags & x).length;
          for (let i = 0; i < n; i++) { if (rd32(b, cto + i * step) !== 0) return true; }
        }
      } else if (type === 'mdat') return false;
      o += size;
    }
    return false;
  };
  try { return scan(0, b.length); } catch (e) { return false; }
}

// The video timescale (ticks per second) of an init segment: the first trak's
// mdhd (the server puts video first). 0 when not found.
export function videoTimescale(init) {
  const b = init;
  for (let i = 0; i + 24 < b.length; i++) {
    if (b[i] === 0x6d && fourcc(b, i) === 'mdhd') return rd32(b, i + 4 + (b[i + 4] === 1 ? 20 : 12));
  }
  return 0;
}

// Evens out frame times. Many cameras (Reolink especially) stamp each frame
// when it is sent rather than when it was captured, so a steady 15 fps stream
// arrives with frame durations anywhere from 4 to 400 ms; played as stamped,
// frames bunch up (several fall into one screen refresh and are dropped) and
// then the picture hangs. Each fragment's video decode time and duration are
// rewritten in place to a steady cadence (the average of the last ~5 s) that
// follows the camera's clock slowly, never straying more than maxDev seconds
// from it (kept small when the stream has audio, so lips stay in sync).
// Fragments are the server's: one moof+mdat per video frame, video = track 1.
export class Dejitter {
  constructor(timescale, maxDev) {
    this.ts = timescale; this.maxDev = maxDev * timescale;
    this.real = [];    // recent real decode times
    this.out = null;   // output decode time of the next frame
  }
  // Rewrites one fragment in place; returns false when it is not one it knows.
  apply(f) {
    const v = findVideoRun(f);
    if (!v || v.n !== 1) return false;
    const r = v.tfdt64 ? rd32(f, v.tfdt) * 4294967296 + rd32(f, v.tfdt + 4) : rd32(f, v.tfdt);
    const rNext = r + rd32(f, v.dur);
    const hist = this.real;
    if (hist.length && r <= hist[hist.length - 1]) { hist.length = 0; this.out = null; }
    hist.push(r);
    while (hist.length > 150 || (hist.length > 9 && r - hist[1] > 5 * this.ts)) hist.shift();
    if (this.out === null || Math.abs(r - this.out) > this.maxDev) this.out = r;
    const o = this.out;
    // Steady pace: the mean frame interval of the recent frames.
    const step = hist.length > 8 ? (hist[hist.length - 1] - hist[0]) / (hist.length - 1) : rNext - r;
    let next = o + step + 0.04 * (rNext - (o + step));
    next = Math.min(Math.max(next, rNext - this.maxDev, o + step / 4), rNext + this.maxDev);
    next = Math.round(next);
    if (next <= o) next = o + 1;
    this.out = next;
    if (v.tfdt64) { wr32(f, v.tfdt, Math.floor(o / 4294967296)); wr32(f, v.tfdt + 4, o >>> 0); } else wr32(f, v.tfdt, o >>> 0);
    wr32(f, v.dur, next - o);
    return true;
  }
}

// Locates the video track's tfdt value and first sample duration in a fragment.
function findVideoRun(b) {
  if (b.length < 16 || fourcc(b, 4) !== 'moof') return null;
  const end = rd32(b, 0);
  for (let o = 8; o + 8 <= end;) {
    const size = rd32(b, o);
    if (size < 8 || o + size > end) return null;
    if (fourcc(b, o + 4) === 'traf') {
      let id = 0, tfdt = 0, tfdt64 = false, dur = 0, n = 0;
      for (let c = o + 8; c + 8 <= o + size;) {
        const cs = rd32(b, c), ct = fourcc(b, c + 4);
        if (cs < 8) return null;
        if (ct === 'tfhd') id = rd32(b, c + 12);
        else if (ct === 'tfdt') { tfdt64 = b[c + 8] === 1; tfdt = c + 12; }
        else if (ct === 'trun') {
          const flags = rd32(b, c + 8) & 0xffffff;
          if (!(flags & 0x100)) return null;
          n = rd32(b, c + 12);
          dur = c + 16 + (flags & 1 ? 4 : 0) + (flags & 4 ? 4 : 0);
        }
        c += cs;
      }
      if (id === 1) return tfdt && dur ? { tfdt, tfdt64, dur, n } : null;
    }
    o += size;
  }
  return null;
}
