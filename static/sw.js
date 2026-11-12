// sw.js - AmberSight's notification helper (service worker). It shows the
// alerts the server pushes, even when no AmberSight page is open, and opens
// the right camera when one is tapped. It does not touch normal page loads.

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (ev) => ev.waitUntil(self.clients.claim()));

// The event picture, fetched with this browser's sign-in, as a data: URL
// (some browsers do not send the sign-in cookie for notification pictures).
async function picture(url) {
  try {
    const r = await fetch(url, { credentials: 'same-origin' });
    if (!r.ok) return undefined;
    const blob = await r.blob();
    if (blob.size > 400 * 1024) return undefined;
    const bytes = new Uint8Array(await blob.arrayBuffer());
    let bin = '';
    for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return 'data:' + (blob.type || 'image/jpeg') + ';base64,' + btoa(bin);
  } catch (e) { return undefined; }
}

self.addEventListener('push', (ev) => {
  let m = {};
  try { m = ev.data ? ev.data.json() : {}; } catch (e) { m = { title: 'AmberSight', body: ev.data ? ev.data.text() : '' }; }
  ev.waitUntil((async () => {
    const opts = {
      body: m.body || '',
      icon: '/static/icons/icon-192.png',
      badge: '/static/icons/badge-96.png',
      tag: m.tag || undefined,
      // A newer alert from the same camera replaces the older one but still rings.
      renotify: !!m.tag,
      requireInteraction: !!m.urgent,
      timestamp: m.ts || Date.now(),
      data: { url: m.url || '/', camera_id: m.camera_id, camera: m.camera },
    };
    // "Pause 1 h" (event notifications): pauses this camera's notifications only.
    if (Array.isArray(m.actions) && typeof m.camera_id === 'number') {
      const acts = m.actions.filter(a => a && a.action === 'pause' && a.title).slice(0, 2);
      if (acts.length) {
        opts.actions = acts.map(a => ({ action: 'pause:' + (Number(a.minutes) || 60), title: String(a.title) }));
      }
    }
    if (m.image && m.image.startsWith('/') && !m.image.startsWith('//')) {
      const img = await picture(m.image);
      if (img) opts.image = img;
    }
    await self.registration.showNotification(m.title || 'AmberSight', opts);
  })());
});

// Pause one camera's notifications (same call the page makes, signed in by the cookie).
async function pauseFromNotification(data, minutes) {
  const name = data.camera || 'this camera';
  try {
    const r = await fetch('/api/cameras/' + data.camera_id + '/pause', {
      method: 'POST', credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ minutes }),
    });
    if (!r.ok) throw new Error(String(r.status));
    const h = minutes % 60 === 0 ? (minutes / 60) + (minutes === 60 ? ' hour' : ' hours') : minutes + ' minutes';
    await self.registration.showNotification('Notifications paused', {
      body: 'Notifications from ' + name + ' paused for ' + h + '. It still records and detects.',
      icon: '/static/icons/icon-192.png', badge: '/static/icons/badge-96.png', tag: 'pause-' + data.camera_id,
      data: { url: '/' },
    });
  } catch (e) {
    await self.registration.showNotification('Could not pause notifications', {
      body: 'Open AmberSight and use the camera’s menu (Pause notifications). Pausing needs an admin sign-in.',
      icon: '/static/icons/icon-192.png', badge: '/static/icons/badge-96.png', tag: 'pause-' + data.camera_id,
      data: { url: '/' },
    });
  }
}

self.addEventListener('notificationclick', (ev) => {
  ev.notification.close();
  if (ev.action && ev.action.startsWith('pause:') && ev.notification.data && typeof ev.notification.data.camera_id === 'number') {
    ev.waitUntil(pauseFromNotification(ev.notification.data, Number(ev.action.slice(6)) || 60));
    return;
  }
  const path = (ev.notification.data && ev.notification.data.url) || '/';
  const url = new URL(path, self.location.origin).href;
  ev.waitUntil((async () => {
    const tabs = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    // A main AmberSight page (not a one-camera pop-out or a split-view pane).
    const ours = tabs.filter(c => new URL(c.url).origin === self.location.origin);
    const main = (c) => { const q = new URL(c.url).searchParams; return !q.has('cam') && !q.has('pane') && q.get('split') !== '1'; };
    const tab = ours.find(c => main(c) && c.focused) || ours.find(main);
    if (tab) {
      // An open AmberSight page opens the camera itself (no reload).
      await tab.focus();
      tab.postMessage({ type: 'ambs-open', url });
      return;
    }
    await self.clients.openWindow(url);
  })());
});
