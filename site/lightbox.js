// Shared lightbox for site clips and images. Opt in with markup only:
//   <video data-lightbox ...>  or  <img data-lightbox ...>
// Optional: data-lightbox-src (a larger file to show), data-lightbox-title.
// A clip inside a link opens here, and the dialog offers that link instead.
(function () {
  'use strict';
  if (window.amuxLightbox) return;

  var SEL = 'video[data-lightbox], img[data-lightbox]';
  var ICON = '<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7"/></svg>';
  var dlg, stage, titleEl, linkEl, fsBtn, closeBtn, media, src, srcState, lastFocus;

  function titleOf(el) {
    return el.getAttribute('data-lightbox-title') || el.getAttribute('title') || el.getAttribute('alt') || (el.tagName === 'VIDEO' ? 'Video' : 'Image');
  }

  function build() {
    dlg = document.createElement('div');
    dlg.className = 'lb';
    dlg.hidden = true;
    dlg.setAttribute('role', 'dialog');
    dlg.setAttribute('aria-modal', 'true');
    dlg.setAttribute('aria-labelledby', 'lb-title');
    dlg.innerHTML =
      '<div class="lb-bar">' +
        '<p class="lb-title" id="lb-title"></p>' +
        '<a class="lb-link" hidden></a>' +
        '<button type="button" class="lb-btn lb-fs" aria-label="Full screen"><svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3H3v5M16 3h5v5M8 21H3v-5M16 21h5v-5"/></svg></button>' +
        '<button type="button" class="lb-btn lb-close" aria-label="Close"><svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18"/></svg></button>' +
      '</div>' +
      '<div class="lb-stage"></div>';
    document.body.appendChild(dlg);
    stage = dlg.querySelector('.lb-stage');
    titleEl = dlg.querySelector('.lb-title');
    linkEl = dlg.querySelector('.lb-link');
    fsBtn = dlg.querySelector('.lb-fs');
    closeBtn = dlg.querySelector('.lb-close');
    closeBtn.addEventListener('click', close);
    fsBtn.addEventListener('click', fullscreen);
    // A click on the dimmed area around the clip closes it; a click on the clip does not.
    stage.addEventListener('click', function (e) { if (e.target === stage) close(); });
    dlg.addEventListener('keydown', onKey);
    window.addEventListener('resize', function () { if (!dlg.hidden) fit(); });
  }

  function focusables() {
    return Array.prototype.filter.call(dlg.querySelectorAll('a[href], button, video[controls]'), function (n) {
      return !n.hidden && n.offsetParent !== null;
    });
  }

  function onKey(e) {
    if (e.key === 'Escape') { e.preventDefault(); close(); return; }
    if (e.key !== 'Tab') return;
    var f = focusables(); if (!f.length) return;
    var first = f[0], last = f[f.length - 1];
    if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
  }

  // Scale the media up to the largest size that fits the stage, keeping its shape.
  function fit() {
    if (!media || !stage) return;
    var w = media.videoWidth || media.naturalWidth || (src && (src.videoWidth || src.naturalWidth));
    var h = media.videoHeight || media.naturalHeight || (src && (src.videoHeight || src.naturalHeight));
    if (!w || !h) return;
    var cs = getComputedStyle(stage);
    var sw = stage.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
    var sh = stage.clientHeight - parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom);
    var k = Math.min(sw / w, sh / h);
    media.style.width = Math.floor(w * k) + 'px';
    media.style.height = Math.floor(h * k) + 'px';
  }

  function canFullscreen(el) {
    return !!(el.requestFullscreen || el.webkitRequestFullscreen || el.webkitEnterFullscreen);
  }

  function fullscreen() {
    if (!media) return;
    if (media.requestFullscreen) media.requestFullscreen().catch(function () {});
    else if (media.webkitRequestFullscreen) media.webkitRequestFullscreen();
    else if (media.webkitEnterFullscreen) media.webkitEnterFullscreen(); // iPhone Safari, video only
  }

  function open(el) {
    if (!dlg) build();
    src = el;
    lastFocus = document.activeElement;
    var isVideo = el.tagName === 'VIDEO';
    var url = el.getAttribute('data-lightbox-src') || el.currentSrc || el.getAttribute('src') || el.getAttribute('data-src') || '';
    if (isVideo) {
      srcState = { paused: el.paused, t: el.currentTime || 0 };
      el.pause();
      media = document.createElement('video');
      media.src = url;
      if (el.poster) media.poster = el.poster;
      media.controls = true;
      media.loop = el.loop;
      media.muted = el.muted;
      media.playsInline = true;
      media.setAttribute('playsinline', '');
      media.preload = 'auto';
      var t = srcState.t;
      media.addEventListener('loadedmetadata', function () {
        fit();
        if (t) { try { media.currentTime = t; } catch (e) {} }
      }, { once: true });
      var p = media.play(); if (p && p.catch) p.catch(function () {});
    } else {
      srcState = null;
      media = document.createElement('img');
      media.src = url;
      media.alt = el.getAttribute('alt') || '';
      media.addEventListener('load', fit, { once: true });
    }
    media.className = 'lb-media';
    media.setAttribute('aria-label', titleOf(el));
    stage.innerHTML = '';
    stage.appendChild(media);
    titleEl.textContent = titleOf(el);
    var a = el.closest('a[href]');
    linkEl.hidden = !a;
    if (a) { linkEl.href = a.getAttribute('href'); var label = el.getAttribute('data-lightbox-link');
      if (label) linkEl.textContent = label;
      else linkEl.innerHTML = '<span class="lb-wide">Open the demo page</span><span class="lb-narrow">Demo page</span>'; }
    fsBtn.hidden = !canFullscreen(media);

    document.documentElement.classList.add('lb-open');
    dlg.hidden = false;
    fit();
    closeBtn.focus();
    if (window.posthog) {
      var spot = el.closest('[data-cta-loc]');
      posthog.capture('cta_click', { goal: 'lightbox', spot: spot ? spot.getAttribute('data-cta-loc') : location.pathname, href: url.split('?')[0], label: titleOf(el).slice(0, 60), plan: '' });
    }
  }

  function close() {
    if (!dlg || dlg.hidden) return;
    if (document.fullscreenElement && document.exitFullscreen) document.exitFullscreen().catch(function () {});
    if (media && media.tagName === 'VIDEO') {
      var t = media.currentTime;
      media.pause();
      media.removeAttribute('src');
      media.load();
      // The inline clip picks up where the big one stopped, and plays again if it was playing.
      if (src && srcState) {
        try { if (t) src.currentTime = t; } catch (e) {}
        if (!srcState.paused) { var p = src.play(); if (p && p.catch) p.catch(function () {}); }
      }
    }
    stage.innerHTML = '';
    media = null;
    dlg.hidden = true;
    document.documentElement.classList.remove('lb-open');
    var back = src && (src.closest('a[href]') || src.parentElement.querySelector('button.lb-cue'));
    var to = back || lastFocus;
    if (to && to.focus) to.focus({ preventScroll: true });
    src = null;
  }

  // Each clip gets a small expand badge so people can see it opens larger.
  // Inside a link the badge is decoration (a button cannot sit in a link);
  // elsewhere it is a real button for keyboard users.
  function decorate(root) {
    (root || document).querySelectorAll(SEL).forEach(function (el) {
      if (el.hasAttribute('data-lb-ready')) return;
      el.setAttribute('data-lb-ready', '');
      var host = el.parentElement; if (!host) return;
      if (getComputedStyle(host).position === 'static') host.style.position = 'relative';
      host.classList.add('lb-host');
      var inLink = !!el.closest('a[href]');
      var cue = document.createElement(inLink ? 'span' : 'button');
      cue.className = 'lb-cue';
      cue.innerHTML = ICON;
      if (inLink) cue.setAttribute('aria-hidden', 'true');
      else { cue.type = 'button'; cue.setAttribute('aria-label', 'Enlarge: ' + titleOf(el)); }
      host.appendChild(cue);
    });
  }

  // Capture phase, so a clip inside a card link opens here instead of following the link.
  document.addEventListener('click', function (e) {
    if (e.metaKey || e.ctrlKey || e.shiftKey || e.button > 0) return;
    var t = e.target;
    var el = t.closest && t.closest(SEL);
    if (!el && t.closest && t.closest('.lb-cue')) el = t.closest('.lb-host').querySelector(SEL);
    if (!el || (dlg && dlg.contains(el))) return;
    e.preventDefault();
    e.stopPropagation();
    open(el);
  }, true);

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', function () { decorate(); });
  else decorate();

  window.amuxLightbox = { open: open, close: close, decorate: decorate };
})();
