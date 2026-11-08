// Swaps a .yt-frame poster for the player on click, so a page makes no YouTube
// request before play. Without this script the poster is a plain YouTube link.
document.addEventListener('click', function (ev) {
  var a = ev.target.closest && ev.target.closest('a.yt-frame[data-yt]');
  if (!a || ev.metaKey || ev.ctrlKey || ev.shiftKey) return;
  ev.preventDefault();
  var f = document.createElement('iframe');
  f.src = 'https://www.youtube-nocookie.com/embed/' + a.getAttribute('data-yt') + '?autoplay=1&rel=0';
  f.title = a.getAttribute('aria-label') || 'Video';
  f.allow = 'autoplay; encrypted-media; picture-in-picture; fullscreen';
  f.allowFullscreen = true;
  var d = document.createElement('div');
  d.className = 'yt-frame';
  d.appendChild(f);
  var spot = a.closest('[data-cta-loc]');
  a.replaceWith(d);
  f.focus();
  if (window.posthog) posthog.capture('cta_click', { goal: 'video', spot: spot ? spot.getAttribute('data-cta-loc') : 'body', href: 'youtube:' + f.src.split('/embed/')[1].split('?')[0], label: 'Play the amux tour', plan: '' });
});
