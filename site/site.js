(function () {
  // Theme: the visitor picks system, light or dark. "system" follows the OS
  // and keeps following it. Runs synchronously in <head>, before first paint.
  var KEY = 'amux-theme';
  var root = document.documentElement;
  var mq = window.matchMedia ? window.matchMedia('(prefers-color-scheme: light)') : null;
  function getPref() {
    var v = null;
    try { v = localStorage.getItem(KEY); } catch (e) {}
    return v === 'light' || v === 'dark' ? v : 'system';
  }
  function resolve(pref) { return pref === 'system' ? (mq && mq.matches ? 'light' : 'dark') : pref; }
  function apply(pref) {
    var t = resolve(pref);
    root.setAttribute('data-theme', t);
    root.setAttribute('data-theme-pref', pref);
    root.style.colorScheme = t;
    var meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute('content', t === 'light' ? '#fbfaf7' : '#14171d');
    var label = 'Theme: ' + pref + (pref === 'system' ? ' (' + t + ')' : '');
    document.querySelectorAll('.theme-btn').forEach(function (b) {
      b.setAttribute('aria-label', label); b.setAttribute('title', label);
    });
    document.querySelectorAll('[data-theme-set]').forEach(function (b) {
      b.setAttribute('aria-checked', b.getAttribute('data-theme-set') === pref ? 'true' : 'false');
    });
  }
  var pref = getPref();
  var theme = resolve(pref);
  apply(pref);

  // A/B tests. Each visitor is put at random into one variant of every running
  // test and stays there (localStorage). Markup marks a variant's elements with
  // data-ab="<test>:<variant>"; the other variants are hidden before first paint.
  // CSS can also branch on html[data-ab-<test>="<variant>"]. To start a test, add
  // it here; to end one, delete it here and keep the winning markup.
  // `python3 scripts/site-funnel.py --ab` compares goal rates per variant.
  var AB_TESTS = {
    // exp146: { variants: ['control', 'b'], pages: /^\/$/ },
  };
  var ab = {};
  try { ab = JSON.parse(localStorage.getItem('amux-ab') || '{}') || {}; } catch (e) {}
  var abHide = [];
  Object.keys(AB_TESTS).forEach(function (t) {
    var vs = AB_TESTS[t].variants;
    if (vs.indexOf(ab[t]) < 0) {
      var r = new Uint32Array(1);
      (window.crypto || window.msCrypto).getRandomValues(r);
      ab[t] = vs[r[0] % vs.length];
    }
    root.setAttribute('data-ab-' + t, ab[t]);
    vs.forEach(function (v) { if (v !== ab[t]) abHide.push('[data-ab="' + t + ':' + v + '"]'); });
  });
  Object.keys(ab).forEach(function (t) { if (!AB_TESTS[t]) delete ab[t]; });
  try { localStorage.setItem('amux-ab', JSON.stringify(ab)); } catch (e) {}
  if (abHide.length) {
    var abStyle = document.createElement('style');
    abStyle.textContent = abHide.join(',') + '{display:none!important}';
    document.head.appendChild(abStyle);
  }
  if (mq) {
    var onOs = function () { if (getPref() === 'system') apply('system'); };
    if (mq.addEventListener) mq.addEventListener('change', onOs); else if (mq.addListener) mq.addListener(onOs);
  }
  window.addEventListener('storage', function (e) { if (e.key === KEY) apply(getPref()); });

  document.addEventListener('DOMContentLoaded', function () {
    apply(getPref());
    document.querySelectorAll('.theme-pick').forEach(function (pick) {
      var btn = pick.querySelector('.theme-btn');
      var menu = pick.querySelector('.theme-menu');
      if (!btn || !menu) return;
      var items = Array.prototype.slice.call(menu.querySelectorAll('[data-theme-set]'));
      function open(focusItem) {
        menu.hidden = false; btn.setAttribute('aria-expanded', 'true');
        if (focusItem) (menu.querySelector('[aria-checked="true"]') || items[0]).focus();
      }
      function close(refocus) {
        if (menu.hidden) return;
        menu.hidden = true; btn.setAttribute('aria-expanded', 'false');
        if (refocus) btn.focus();
      }
      btn.addEventListener('click', function (e) {
        e.stopPropagation();
        if (menu.hidden) open(e.detail === 0); else close(false);
      });
      btn.addEventListener('keydown', function (e) {
        if (e.key === 'ArrowDown') { e.preventDefault(); open(true); }
      });
      items.forEach(function (it, i) {
        it.addEventListener('click', function () {
          var p = it.getAttribute('data-theme-set');
          try { if (p === 'system') localStorage.removeItem(KEY); else localStorage.setItem(KEY, p); } catch (err) {}
          apply(p);
          if (window.posthog) posthog.capture('theme_set', { theme_pref: p, theme: resolve(p) });
          close(true);
        });
        it.addEventListener('keydown', function (e) {
          if (e.key === 'ArrowDown') { e.preventDefault(); items[(i + 1) % items.length].focus(); }
          else if (e.key === 'ArrowUp') { e.preventDefault(); items[(i - 1 + items.length) % items.length].focus(); }
          else if (e.key === 'Tab') { close(false); }
        });
      });
      document.addEventListener('click', function (e) { if (!pick.contains(e.target)) close(false); });
      document.addEventListener('keydown', function (e) { if (e.key === 'Escape') close(true); });
    });
  });


  // GitHub star count for the header, cached an hour (the unauthenticated
  // API allows 60 requests an hour per IP). Fills any count still empty.
  document.addEventListener('DOMContentLoaded', function () {
    var el = document.getElementById('gh-stars');
    if (!el) return;
    function show(n) { if (n && !/\d/.test(el.textContent)) el.textContent = '\u2605 ' + n; }
    var cached = null;
    try { cached = JSON.parse(localStorage.getItem('amux-gh-stars') || 'null'); } catch (e) {}
    if (cached && Date.now() - cached.t < 3600000) { show(cached.n); return; }
    fetch('https://api.github.com/repos/mixpeek/amux').then(function (r) { return r.json(); }).then(function (d) {
      if (!d.stargazers_count) return;
      try { localStorage.setItem('amux-gh-stars', JSON.stringify({ n: d.stargazers_count, t: Date.now() })); } catch (e) {}
      show(d.stargazers_count);
    }).catch(function () { if (cached) show(cached.n); });
  });

  // EXP-141: a copy button on the one-line install command (getting-started,
  // docs). Measured by the install_copy event; adds no new CTA.
  document.addEventListener('DOMContentLoaded', function () {
    if (!/^\/(guides\/getting-started|docs)\/$/.test(location.pathname)) return;
    document.querySelectorAll('pre > code').forEach(function (code) {
      var cmd = code.textContent.trim();
      if (cmd.indexOf('curl -fsSL https://amux.io/install.sh') !== 0 || cmd.indexOf('\n') !== -1) return;
      var pre = code.parentNode, wrap = document.createElement('div'), btn = document.createElement('button');
      wrap.className = 'copy-wrap';
      pre.parentNode.insertBefore(wrap, pre);
      wrap.appendChild(pre);
      btn.type = 'button'; btn.className = 'copy-btn'; btn.textContent = 'Copy';
      btn.setAttribute('aria-label', 'Copy install command');
      btn.addEventListener('click', function () {
        var done = function () {
          btn.textContent = 'Copied';
          setTimeout(function () { btn.textContent = 'Copy'; }, 1600);
        };
        if (navigator.clipboard) navigator.clipboard.writeText(cmd).then(done, function () {});
        if (window.posthog) posthog.capture('install_copy', { path: location.pathname });
      });
      wrap.appendChild(btn);
    });
  });

  // PostHog analytics — async, non-blocking
  !function(t,e){var o,n,p,r;e.__SV||(window.posthog=e,e._i=[],e.init=function(i,s,a){function g(t,e){var o=e.split(".");2==o.length&&(t=t[o[0]],e=o[1]),t[e]=function(){t.push([e].concat(Array.prototype.slice.call(arguments,0)))}}(p=t.createElement("script")).type="text/javascript",p.crossOrigin="anonymous",p.async=!0,p.src=s.api_host.replace(".i.posthog.com","-assets.i.posthog.com")+"/static/array.js",(r=t.getElementsByTagName("script")[0]).parentNode.insertBefore(p,r);var u=e;for(void 0!==a?u=e[a]=[]:a="posthog",u.people=u.people||[],u.toString=function(t){var e="posthog";return"posthog"!==a&&(e+="."+a),t||(e+=" (stub)"),e},u.people.toString=function(){return u.people.toString(1)+" (stub)"},o="init capture register register_once register_for_session unregister unregister_for_session getFeatureFlag getFeatureFlagPayload isFeatureEnabled reloadFeatureFlags updateEarlyAccessFeatureEnrollment getEarlyAccessFeatures on onFeatureFlags onSessionId getSurveys getActiveMatchingSurveys renderSurvey canRenderSurvey getNextSurveyStep identify setPersonProperties group resetGroups setPersonPropertiesForFlags resetPersonPropertiesForFlags setGroupPropertiesForFlags resetGroupPropertiesForFlags reset get_distinct_id getGroups get_session_id get_session_replay_url alias set_config startSessionRecording stopSessionRecording sessionRecordingStarted captureException loadToolbar get_property getSessionProperty createPersonProfile opt_in_capturing opt_out_capturing has_opted_in_capturing has_opted_out_capturing clear_opt_in_out_capturing debug getPageViewId captureTrackedMutations setTrackedMutations".split(" "),n=0;n<o.length;n++)g(u,o[n]);e._i.push([i,s,a])},e.__SV=1)}(document,window.posthog||[]);
  posthog.init('phc_Ckeacj8y8X8YiLkwHBcpBsJVSnsFKohch2vMv9sJYmE6', {
    api_host: 'https://us.i.posthog.com',
    person_profiles: 'identified_only',
    capture_pageview: true,
    capture_pageleave: true,
    enable_heatmaps: true,
  });
  posthog.register({ theme_preference: pref, theme: theme });

  // Funnel. The site has two goals, a GitHub star and a Cloud signup, and
  // every page is measured against them the same way: one `cta_click` event
  // per goal link, carrying the page, the spot on the page and the visitor's
  // traffic source. scripts/site-funnel.py reads these for the daily AEO run.
  function sourceOf(host, utm) {
    var s = (utm || '').toLowerCase(), h = (host || '').toLowerCase();
    if (/chatgpt|openai|perplexity|claude|gemini|copilot|phind|you\.com|grok|deepseek|mistral|kagi/.test(s + ' ' + h)) return 'ai';
    if (!h) return s ? 'campaign' : 'direct';
    if (h === location.hostname || /(^|\.)amux\.io$/.test(h)) return 'internal';
    if (/(^|\.)(google|bing|duckduckgo|yahoo|yandex|baidu|ecosia|brave|startpage|naver)\./.test(h)) return 'search';
    if (/github\.com$/.test(h)) return 'github';
    if (/reddit|t\.co$|x\.com$|twitter|linkedin|ycombinator|youtube|facebook|threads|bsky|mastodon|discord/.test(h)) return 'social';
    return 'referral';
  }
  var ref = '';
  try { ref = document.referrer ? new URL(document.referrer).hostname : ''; } catch (e) {}
  var utm = new URLSearchParams(location.search).get('utm_source') || '';
  var src = sourceOf(ref, utm);
  if (src !== 'internal') {
    var q = new URLSearchParams(location.search);
    posthog.register_once({ first_source: src, first_landing: location.pathname, first_referrer_host: ref || '$direct',
      first_utm_source: utm, first_utm_medium: q.get('utm_medium') || '', first_utm_campaign: q.get('utm_campaign') || '',
      first_utm_content: q.get('utm_content') || '', first_seen: new Date().toISOString().slice(0, 10) });
    posthog.register_for_session({ session_source: src, session_landing: location.pathname });
  }

  function goalOf(a) {
    var u;
    try { u = new URL(a.getAttribute('href'), location.href); } catch (e) { return null; }
    if (u.hostname === 'github.com' && /^\/mixpeek\/amux\/releases\/(tag|download)\/desktop-latest/.test(u.pathname)) return 'mac';
    if (u.hostname === 'github.com' && /^\/mixpeek\/amux(\/|$)/.test(u.pathname)) return 'github';
    if (u.hostname === location.hostname) {
      if (/^\/start\/(welcome|billing|delete)\//.test(u.pathname)) return null;
      if (/^\/start\/?$/.test(u.pathname)) return 'cloud';
      if (/^\/pricing\/?$/.test(u.pathname) || u.hash === '#get') return 'pricing';
      if (/^\/walkthrough\/?$/.test(u.pathname)) return 'walkthrough';
    }
    if (u.hostname === 'apps.apple.com') return 'ios';
    if (a.classList.contains('js-schedule') || u.hostname === 'calendly.com') return 'concierge';
    return null;
  }
  function spotOf(el) {
    var tagged = el.closest('[data-cta-loc]');
    if (tagged) return tagged.getAttribute('data-cta-loc');
    if (el.closest('.site-header')) return 'header';
    if (el.closest('footer')) return 'footer';
    var sec = el.closest('section[id], [id].hero, main [id]');
    return sec ? sec.id : 'body';
  }
  // A test is exposed on a page that matches its `pages` or carries its markup.
  // From then on every event this visitor sends carries ab_<test>.
  document.addEventListener('DOMContentLoaded', function () {
    Object.keys(AB_TESTS).forEach(function (t) {
      var cfg = AB_TESTS[t];
      if (!(cfg.pages && cfg.pages.test(location.pathname)) && !document.querySelector('[data-ab^="' + t + ':"]')) return;
      var props = {};
      props['ab_' + t] = ab[t];
      props['$feature/' + t] = ab[t];
      posthog.register(props);
      posthog.capture('ab_exposure', { experiment: t, variant: ab[t] });
    });
  });

  document.addEventListener('click', function (ev) {
    var a = ev.target.closest && ev.target.closest('a[href]');
    if (!a) return;
    var goal = goalOf(a);
    if (!goal) return;
    var plan = '';
    try { plan = new URL(a.href).searchParams.get('plan') || ''; } catch (e) {}
    posthog.capture('cta_click', {
      goal: goal, spot: spotOf(a), href: a.getAttribute('href'),
      label: (a.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 60), plan: plan
    }, { transport: 'sendBeacon' });
  }, true);

})();
