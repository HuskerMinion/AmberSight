// device.js - which kind of screen this is: 'phone' | 'tablet' | 'desktop', and how the
// Live panel shows on it ('sheet' | 'drawer' | 'pinned'). The rule itself lives in
// prepaint.js (it runs before the first paint and sets html[data-device] / [data-dock]),
// so CSS and JS always agree.

const fallback = () => ({ device: 'desktop', dock: 'pinned', touch: false });
export const current = () => (window.ambsDevice ? window.ambsDevice.get() : fallback());
export const device = () => current().device;
export const dockMode = () => current().dock;
export const isPhone = () => device() === 'phone';
// Called with the new class whenever it changes (resize, rotation, pointer change).
export function onDeviceChange(fn) { window.addEventListener('ambs-device', (ev) => fn(ev.detail)); }
