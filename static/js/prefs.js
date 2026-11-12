// prefs.js - interface state kept on the server.
//
//  * Per-user prefs: one JSON object (GET/PUT /api/ui/prefs) holding layouts, the
//    active layout, dock side/collapse, grid quality and cycle settings.
//  * Shared camera order (GET /api/ui/orders, PUT /api/ui/orders/:key, admin): the
//    order of the "All" view (key "all") and of each group ("group:<name>").
//
// Writes are debounced and send only the keys that changed (PATCH), so two open
// pages cannot undo each other's changes to different settings.

import { api, enc } from './core.js';
import { paneKey } from './split.js';

// In a split-view pane, view choices (layout, quality, styles ...) are kept per
// pane in this browser, so the two panes do not change each other.
function paneGet(key) {
  const k = paneKey(key);
  if (!k) return undefined;
  try { const v = localStorage.getItem(k); return v === null ? null : JSON.parse(v); } catch (e) { return null; }
}

let P = {};
let orders = {};
let timer = 0;
let dirty = false;
let dirtyKeys = new Set();
export let serverOk = true;

export async function load() {
  try { P = await api('/api/ui/prefs'); serverOk = true; } catch (e) { P = {}; serverOk = false; }
  if (!P || typeof P !== 'object' || Array.isArray(P)) P = {};
  await loadOrders();
}

export async function loadOrders() {
  try { orders = await api('/api/ui/orders'); } catch (e) { /* keep what we have */ }
  if (!orders || typeof orders !== 'object') orders = {};
}

export function reset() { P = {}; orders = {}; dirty = false; dirtyKeys = new Set(); clearTimeout(timer); }

export const get = (key, dflt) => {
  if (paneKey(key)) { const v = paneGet(key); return v === null || v === undefined ? dflt : v; }
  return P[key] === undefined ? dflt : P[key];
};

// Merge keys into the prefs and save them shortly.
export function set(obj) {
  const shared = {};
  for (const [k, v] of Object.entries(obj)) {
    const pk = paneKey(k);
    if (pk) { try { localStorage.setItem(pk, JSON.stringify(v)); } catch (e) { /* not remembered */ } }
    else shared[k] = v;
  }
  obj = shared;
  if (!Object.keys(obj).length) return;
  Object.assign(P, obj);
  for (const k of Object.keys(obj)) dirtyKeys.add(k);
  dirty = true;
  clearTimeout(timer);
  timer = setTimeout(flush, 300);
}

// Save now; resolves true when the server has the prefs.
export async function flush() {
  clearTimeout(timer);
  if (!dirty) return true;
  dirty = false;
  const keys = [...dirtyKeys];
  dirtyKeys = new Set();
  const patch = {};
  for (const k of keys) patch[k] = P[k] === undefined ? null : P[k];
  try { await api('/api/ui/prefs', { method: 'PATCH', json: patch }); serverOk = true; return true; } catch (e) {
    for (const k of keys) dirtyKeys.add(k);
    dirty = true; serverOk = false; return false;
  }
}

// Saved order for a view key ('all' or 'group:<name>'), or null.
export const orderFor = (key) => (Array.isArray(orders[key]) ? orders[key] : null);

export async function saveOrder(key, names) {
  // Cameras not in `names` (a split-view pane lists only one server's) keep
  // their places; the named ones take the new order in the places they held.
  const old = Array.isArray(orders[key]) ? orders[key] : [];
  const set = new Set(names);
  const queue = names.slice();
  const merged = old.map(n => (set.has(n) ? queue.shift() : n)).filter(n => n !== undefined);
  const full = merged.concat(queue);
  await api('/api/ui/orders/' + enc(key), { method: 'PUT', json: full });
  orders[key] = full.slice();
}

// Apply a saved order to a list of cameras (anything not named goes last, in its own order).
export function applyOrder(cams, names) {
  if (!names || !names.length) return cams;
  const byName = new Map(cams.map(c => [c.name, c]));
  const out = [];
  for (const n of names) { const c = byName.get(n); if (c) { out.push(c); byName.delete(n); } }
  for (const c of cams) if (byName.has(c.name)) out.push(c);
  return out;
}

window.addEventListener('pagehide', () => {
  if (!dirty) return;
  // Only what changed (like flush), so another page's newer settings stay.
  const patch = {};
  for (const k of dirtyKeys) patch[k] = P[k] === undefined ? null : P[k];
  try { fetch('/api/ui/prefs', { method: 'PATCH', credentials: 'same-origin', keepalive: true, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(patch) }); } catch (e) { /* leaving anyway */ }
});
