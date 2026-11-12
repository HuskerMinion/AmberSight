// admin.js - registry of the admin pages (AI, users, logs, status ...) that live inside Settings.
//
// To add a page, from any module loaded by main.js:
//
//   import { registerAdminPage } from './admin.js';
//   registerAdminPage({
//     id: 'health', title: 'Health', order: 20,
//     render(el) { el.innerHTML = '...'; },   // called each time the page is opened
//     leave() {},                             // optional: stop timers when switching away
//   });
//
// then give it a place in Settings: add its id to a section in settings.js (SECTIONS).
// Save model for admin pages: on/off settings are switches that save when flipped (liveSwitch,
// core.js); text / number / select fields use the one sticky bar (formBar or pageBar+wireBar).
// "Admin" is no longer a page of its own: #admin and openAdminPage() go to Settings.

const pages = [];

export function registerAdminPage(p) {
  const i = pages.findIndex(x => x.id === p.id);
  if (i >= 0) pages.splice(i, 1);
  pages.push(Object.assign({ order: 100 }, p));
  pages.sort((a, b) => a.order - b.order);
}
export function adminPage(id) { return pages.find(p => p.id === id); }

// Old admin page id -> its place in Settings (section/sub).
export const ADMIN_ROUTES = {
  status: 'system/status', logs: 'system/logs', audit: 'system/audit',
  users: 'users/users', profiles: 'schedules/profiles', ai: 'detection/objects', aireview: 'smart/review',
  faces: 'smart/faces', lpr: 'smart/plates', sound: 'smart/sounds', smart: 'smart/search', alertrules: 'detection/rules',
  storage: 'recording/drives', retention: 'recording/retention', discovery: 'cameras/find', offsite: 'system/backup', presence: 'schedules/presence', digest: 'detection/summary',
};

// Open an admin page by its old id (the header health chip uses openAdminPage('status')).
export async function openAdminPage(id) {
  const { goSettings } = await import('./settings.js');
  goSettings(ADMIN_ROUTES[id] || 'system/status');
}

// "Admin" stays as an alias of Settings > System so old links, bookmarks and G A keep working.
export const adminTab = {
  id: 'admin', label: 'Admin', order: 70, admin: true, noNav: true,
  // #admin/<old page id> bookmarks go to that page's place in Settings.
  redirect: () => {
    const m = /^#admin\/([a-z]+)/.exec(location.hash);
    return { id: 'settings', hash: 'settings/' + ((m && ADMIN_ROUTES[m[1]]) || 'system/status') };
  },
};
