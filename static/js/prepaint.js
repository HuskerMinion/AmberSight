// prepaint.js - before the page is drawn: the saved theme, text size and
// event picture size (no flash). A file of its own: the page allows no
// scripts written into it (Content-Security-Policy).
// Apply the saved theme before first paint (no flash). Absent = follow the system.
try { var t = localStorage.getItem('ambs.theme'); if (t) document.documentElement.dataset.theme = t; } catch (e) {}
// Text size and event picture size, per device, also before first paint.
try { var z = localStorage.getItem('ambs.textSize'); if (z) document.documentElement.dataset.text = z; } catch (e) {}
// Timeline controls, per device: docked (default) or auto-hide over the cameras.
try { var c = localStorage.getItem('ambs.tlCtl'); if (c) document.documentElement.dataset.tlctl = c; } catch (e) {}
try { var v = localStorage.getItem('ambs.evThumb'); if (v) document.documentElement.dataset.evthumb = v; } catch (e) {}
// Device class, the one rule for both CSS and JS (device.js reads it): html[data-device]
// = phone | tablet | desktop, and html[data-dock] = how the Live panel shows:
//   sheet  - a phone held upright: a bottom sheet
//   drawer - a phone on its side, a tablet or a narrow window: a slide-over from its side
//   pinned - a desktop: beside the page, collapsible
// "phone" is exactly PHONE_MQ (the CSS phone blocks use the same media query text).
// Re-evaluated on resize, rotation and pointer change; 'ambs-device' fires on a change.
(function () {
  var PHONE_MQ = '(max-width: 560px), (max-height: 500px) and (pointer: coarse)';
  var mm = function (q) { try { return window.matchMedia(q).matches; } catch (e) { return false; } };
  function classify() {
    var w = window.innerWidth;
    var touch = mm('(pointer: coarse)') || mm('(hover: none)');
    var device = mm(PHONE_MQ) ? 'phone' : (touch || w < 900) ? 'tablet' : 'desktop';
    var dock = device === 'desktop' ? 'pinned' : (device === 'phone' && w <= 560) ? 'sheet' : 'drawer';
    return { device: device, dock: dock, touch: touch };
  }
  var cur = null;
  function apply() {
    var c = classify(), el = document.documentElement;
    if (cur && cur.device === c.device && cur.dock === c.dock && cur.touch === c.touch) return;
    cur = c;
    el.dataset.device = c.device;
    el.dataset.dock = c.dock;
    el.dataset.touch = c.touch ? '1' : '0';
    try { window.dispatchEvent(new CustomEvent('ambs-device', { detail: c })); } catch (e) {}
  }
  window.ambsDevice = { PHONE_MQ: PHONE_MQ, get: function () { apply(); return cur; } };
  apply();
  window.addEventListener('resize', apply);
  window.addEventListener('orientationchange', apply);
  try { window.matchMedia('(pointer: coarse)').addEventListener('change', apply); } catch (e) {}
})();
