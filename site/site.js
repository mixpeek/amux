(function () {
  // Synchronous theme boot — runs before paint to prevent flash
  var stored = localStorage.getItem('amux-theme');
  var theme = stored
    ? stored
    : window.matchMedia('(prefers-color-scheme: light)').matches
    ? 'light'
    : 'dark';

  function applyTheme(t) {
    document.documentElement.setAttribute('data-theme', t);
    var meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute('content', t === 'light' ? '#f5f4ed' : '#1e2330');
    localStorage.setItem('amux-theme', t);
    var btn = document.getElementById('theme-toggle-btn');
    if (btn) btn.textContent = t === 'light' ? '☾' : '☀';
  }

  applyTheme(theme);

  document.addEventListener('DOMContentLoaded', function () {
    var nav = document.querySelector('.site-header nav');
    if (!nav) return;
    var btn = document.createElement('button');
    btn.id = 'theme-toggle-btn';
    btn.className = 'theme-toggle';
    btn.setAttribute('aria-label', 'Toggle light/dark mode');
    btn.setAttribute('title', 'Toggle light/dark mode');
    btn.textContent = theme === 'light' ? '☾' : '☀';
    btn.onclick = function () {
      var next = document.documentElement.getAttribute('data-theme') === 'dark' ? 'light' : 'dark';
      theme = next;
      applyTheme(next);
    };
    nav.insertBefore(btn, nav.firstChild);
  });
})();
