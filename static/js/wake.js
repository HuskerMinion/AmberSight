// wake.js - Keep this computer awake (screen on, no sleep) while AmberSight
// is open and showing, using the browser's Screen Wake Lock. On by default;
// a per-device choice in the account menu ("Keep screen awake"). The browser
// drops the lock whenever the page is hidden (another tab, minimized) and it
// is taken again when the page shows. Panes inside the split view leave it
// to the split page itself.

const KEY = 'ambs.awake';
let lock = null;
let asking = false;

export const wakeSupported = () => window.isSecureContext && 'wakeLock' in navigator;

export function keepAwake() {
  try { return localStorage.getItem(KEY) !== '0'; } catch (e) { return true; }
}

async function acquire() {
  if (lock || asking || !keepAwake() || document.hidden || !wakeSupported()) return;
  asking = true;
  try {
    lock = await navigator.wakeLock.request('screen');
    lock.addEventListener('release', () => { lock = null; });
  } catch (e) { /* refused (battery saver, policy): the system may sleep as usual */ }
  asking = false;
}

export function setKeepAwake(on) {
  try { localStorage.setItem(KEY, on ? '1' : '0'); } catch (e) { /* not remembered */ }
  if (on) acquire();
  else if (lock) { lock.release().catch(() => {}); lock = null; }
}

export function initWake() {
  if (window.top !== window) return;   // a split-view pane: the split page holds it
  document.addEventListener('visibilitychange', () => { if (!document.hidden) acquire(); });
  // Some browsers only grant it after the page has been used once.
  document.addEventListener('pointerdown', acquire, { passive: true });
  document.addEventListener('keydown', acquire);
  acquire();
}
