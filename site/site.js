(function () {
  // Synchronous theme boot — runs before paint to prevent flash
  var stored = localStorage.getItem('amux-theme');
  var pageDefault = document.documentElement.getAttribute('data-theme') || null;
  var theme = stored
    ? stored
    : pageDefault
    ? pageDefault
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

  // PostHog analytics — async, non-blocking
  !function(t,e){var o,n,p,r;e.__SV||(window.posthog=e,e._i=[],e.init=function(i,s,a){function g(t,e){var o=e.split(".");2==o.length&&(t=t[o[0]],e=o[1]),t[e]=function(){t.push([e].concat(Array.prototype.slice.call(arguments,0)))}}(p=t.createElement("script")).type="text/javascript",p.crossOrigin="anonymous",p.async=!0,p.src=s.api_host.replace(".i.posthog.com","-assets.i.posthog.com")+"/static/array.js",(r=t.getElementsByTagName("script")[0]).parentNode.insertBefore(p,r);var u=e;for(void 0!==a?u=e[a]=[]:a="posthog",u.people=u.people||[],u.toString=function(t){var e="posthog";return"posthog"!==a&&(e+="."+a),t||(e+=" (stub)"),e},u.people.toString=function(){return u.people.toString(1)+" (stub)"},o="init capture register register_once register_for_session unregister unregister_for_session getFeatureFlag getFeatureFlagPayload isFeatureEnabled reloadFeatureFlags updateEarlyAccessFeatureEnrollment getEarlyAccessFeatures on onFeatureFlags onSessionId getSurveys getActiveMatchingSurveys renderSurvey canRenderSurvey getNextSurveyStep identify setPersonProperties group resetGroups setPersonPropertiesForFlags resetPersonPropertiesForFlags setGroupPropertiesForFlags resetGroupPropertiesForFlags reset get_distinct_id getGroups get_session_id get_session_replay_url alias set_config startSessionRecording stopSessionRecording sessionRecordingStarted captureException loadToolbar get_property getSessionProperty createPersonProfile opt_in_capturing opt_out_capturing has_opted_in_capturing has_opted_out_capturing clear_opt_in_out_capturing debug getPageViewId captureTrackedMutations setTrackedMutations".split(" "),n=0;n<o.length;n++)g(u,o[n]);e._i.push([i,s,a])},e.__SV=1)}(document,window.posthog||[]);
  posthog.init('phx_z3rBWTAM8jzH7MYoJt396nNPfmC8CuE5sTn86Yt5k4i4vgmG', {
    api_host: 'https://us.i.posthog.com',
    person_profiles: 'identified_only',
    capture_pageview: true,
    capture_pageleave: true,
  });

  document.addEventListener('DOMContentLoaded', function () {
    var nav = document.querySelector('.site-header nav') || document.querySelector('header.site nav') || document.querySelector('header nav');
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
    // Insert just before the first CTA button so toggle sits between text links and action buttons
    var firstCta = nav.querySelector('.nav-cta, .cta, [class*="cta"]');
    if (firstCta) {
      nav.insertBefore(btn, firstCta);
    } else {
      nav.appendChild(btn);
    }

    // EXP-002: sticky mobile iOS CTA bar — only on small screens
    // Inject CSS + DOM element so the App Store button is always reachable on mobile
    var expStyle = document.createElement('style');
    expStyle.textContent = [
      '@media(max-width:600px){',
      '  .nav-cta-primary.hide-on-mobile-exp{display:none!important}',
      '  .mobile-ios-sticky{',
      '    position:fixed;bottom:0;left:0;right:0;z-index:9999;',
      '    background:var(--bg);border-top:1px solid var(--border);',
      '    padding:10px 16px calc(10px + env(safe-area-inset-bottom)) 16px;',
      '    display:flex;align-items:center;justify-content:center;gap:10px;',
      '  }',
      '  .mobile-ios-sticky a{',
      '    display:block;flex:1;max-width:320px;text-align:center;',
      '    background:var(--accent);color:#fff;font-weight:600;font-size:.92rem;',
      '    padding:12px 20px;border-radius:10px;text-decoration:none;min-height:44px;',
      '    line-height:1.2;display:flex;align-items:center;justify-content:center;',
      '  }',
      '  .mobile-ios-sticky a:active{opacity:.85}',
      '  body{padding-bottom:calc(64px + env(safe-area-inset-bottom))}',
      '}',
      '@media(min-width:601px){.mobile-ios-sticky{display:none!important}}'
    ].join('');
    document.head.appendChild(expStyle);

    // Hide the nav iOS link on mobile to avoid duplicate CTAs
    var iosNavLink = nav.querySelector('a[href*="apps.apple.com"].nav-cta-primary');
    if (iosNavLink) iosNavLink.classList.add('hide-on-mobile-exp');

    // Inject sticky bar
    var stickyBar = document.createElement('div');
    stickyBar.className = 'mobile-ios-sticky';
    stickyBar.innerHTML = '<a href="https://apps.apple.com/us/app/amux-agent-multiplexer/id6760410435" target="_blank" rel="noopener" data-ph-event="exp002-ios-sticky-tap">Get the iOS App — Manage Agents From Your Phone</a>';
    document.body.appendChild(stickyBar);

    // Track EXP-002 tap in PostHog
    stickyBar.querySelector('a').addEventListener('click', function() {
      if (window.posthog) posthog.capture('exp002_ios_sticky_tap', { experiment: 'EXP-002' });
    });
  });
})();
