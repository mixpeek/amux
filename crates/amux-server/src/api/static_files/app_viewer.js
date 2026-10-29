// Served at /app/<id>/.amux-viewer.js and added to every HTML page on the apps
// origin (apps_origin.rs). Inside the amux viewer it keeps the app in the
// viewer: a link, form or window.open aimed at another page of the same app
// navigates the frame instead of opening a tab, and each page tells the viewer
// where it is, so the viewer can show the path and go back. Outside a frame it
// does nothing. Links to other sites are left as the app wrote them.
(function () {
  if (window.top === window || window.__amuxViewer) return;
  window.__amuxViewer = 1;
  var m = location.pathname.match(/^\/app\/[0-9a-f]{32}/);
  if (!m) return;
  var prefix = m[0] + '/';
  var parentOrigin = (location.ancestorOrigins && location.ancestorOrigins[0]) || '*';
  function inside(href) {
    try {
      var u = new URL(href, location.href);
      return u.protocol === location.protocol && u.host === location.host && u.pathname.indexOf(prefix) === 0 ? u : null;
    } catch (e) { return null; }
  }
  function tell() {
    var at = location.pathname.slice(prefix.length - 1) + location.search + location.hash;
    try { parent.postMessage({ amux_app_nav: 1, at: at, title: document.title || '' }, parentOrigin); } catch (e) {}
  }
  document.addEventListener('click', function (e) {
    if (e.defaultPrevented || e.button !== 0) return;
    var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    if (!a || a.hasAttribute('download')) return;
    var t = (a.getAttribute('target') || '').toLowerCase();
    if (!t || t === '_self') return;
    var u = inside(a.href);
    if (!u) return;
    e.preventDefault();
    location.assign(u.href);
  }, true);
  document.addEventListener('submit', function (e) {
    var f = e.target;
    if (f && f.target && f.target !== '_self' && inside(f.action || location.href)) f.target = '_self';
  }, true);
  var open = window.open;
  window.open = function (url, name, features) {
    var u = url ? inside(String(url)) : null;
    if (u) { location.assign(u.href); return window; }
    return open.apply(window, arguments);
  };
  ['pushState', 'replaceState'].forEach(function (k) {
    var f = history[k];
    history[k] = function () { var r = f.apply(history, arguments); tell(); return r; };
  });
  window.addEventListener('popstate', tell);
  window.addEventListener('hashchange', tell);
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', tell); else tell();
})();
