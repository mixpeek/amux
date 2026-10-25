// Pages host (api/pages.rs, docs/research/pages.md).
//
// Mounts one amux page in a sandboxed iframe and is its only way out. The
// page's HTML runs with an opaque origin (no allow-same-origin), so its
// scripts can read neither this document, amux's bearer, nor any amux API.
// What it can do is post a message to this host, which relays four calls for
// THIS page only: sql, batch, ask, file. Change detection polls the page's
// file signature, because the worker writes page.db with the sqlite3 CLI and
// the server never sees those writes.
//
// Used by the dashboard's Pages tab and by the standalone /pages/<name>.
(function () {
  'use strict';
  if (window.AmuxPages) return;

  // Injected first in the page's <head>: window.amux exists before any page
  // script runs. Kept dependency-free and small; it is the page's whole API.
  const BRIDGE = function (NAME) {
    var seq = 0, pending = {}, subs = {};
    function call(op, args) {
      return new Promise(function (resolve, reject) {
        var id = ++seq;
        pending[id] = { resolve: resolve, reject: reject };
        parent.postMessage({ amuxPage: 1, id: id, op: op, args: args }, '*');
      });
    }
    addEventListener('message', function (e) {
      if (e.source !== parent) return;
      var m = e.data || {};
      if (!m.amuxPage) return;
      if (m.event) {
        (subs[m.event] || []).forEach(function (fn) { try { fn(m.detail); } catch (err) { console.error(err); } });
        return;
      }
      var p = pending[m.id];
      if (!p) return;
      delete pending[m.id];
      if (m.error) p.reject(new Error(m.error)); else p.resolve(m.result);
    });
    window.amux = {
      page: NAME,
      sql: function (sql, params) { return call('sql', { sql: sql, params: params == null ? null : params }); },
      batch: function (sql) { return call('batch', { batch: sql }); },
      ask: function (text, data, kind) { return call('ask', { text: text, data: data == null ? null : data, kind: kind || 'ask' }); },
      file: function (path) { return call('file', { path: path }); },
      on: function (ev, fn) { (subs[ev] = subs[ev] || []).push(fn); },
    };
    // A srcdoc frame resolves "#section" against the HOST's URL, so a plain
    // in-page link navigated the frame to the host page instead of
    // scrolling. Same-page links scroll here (CSS scroll-behavior and
    // scroll-padding still apply); a page's own handler runs first.
    addEventListener('click', function (e) {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey) return;
      var a = e.target && e.target.closest ? e.target.closest('a[href^="#"]') : null;
      if (!a) return;
      e.preventDefault();
      var id = decodeURIComponent(a.getAttribute('href').slice(1));
      var el = id ? (document.getElementById(id) || document.getElementsByName(id)[0]) : null;
      if (el) el.scrollIntoView(); else if (!id) window.scrollTo(0, 0);
    });
    function title() { parent.postMessage({ amuxPage: 1, op: 'title', args: { title: document.title } }, '*'); }
    addEventListener('DOMContentLoaded', title);
    addEventListener('error', function (e) {
      parent.postMessage({ amuxPage: 1, op: 'error', args: { message: String(e.message || e), where: (e.filename || '') + ':' + (e.lineno || 0) } }, '*');
    });
  };

  function inject(html, name) {
    const tag = '<script>(' + BRIDGE.toString() + ')(' + JSON.stringify(name) + ');<\/script>';
    const m = /<head[^>]*>/i.exec(html);
    if (m) return html.slice(0, m.index + m[0].length) + tag + html.slice(m.index + m[0].length);
    const d = /<!doctype[^>]*>/i.exec(html);
    if (d) return html.slice(0, d.index + d[0].length) + tag + html.slice(d.index + d[0].length);
    return tag + html;
  }

  const POLL_MS = 1500;

  function mount(container, name, opts) {
    opts = opts || {};
    const headers = () => Object.assign({ 'Content-Type': 'application/json' }, (opts.headers && opts.headers()) || {});
    const base = '/api/pages/' + encodeURIComponent(name);
    let frame = null, version = null, timer = null, gone = false;

    async function api(path, init) {
      const r = await fetch(base + path, Object.assign({ headers: headers() }, init || {}));
      let body = null;
      try { body = await r.json(); } catch (_) {}
      if (!r.ok) throw new Error((body && body.error) || ('HTTP ' + r.status));
      return body;
    }

    function showError(msg) {
      container.innerHTML = '';
      const d = document.createElement('div');
      d.className = 'amux-page-err';
      d.textContent = msg;
      container.appendChild(d);
      if (opts.onError) opts.onError(msg);
    }

    async function load() {
      let page;
      try { page = await api(''); } catch (e) { showError('This page could not be loaded: ' + e.message); return; }
      version = page.version;
      const f = document.createElement('iframe');
      // No allow-same-origin: the page gets an opaque origin of its own.
      f.setAttribute('sandbox', 'allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-modals allow-downloads');
      f.setAttribute('title', page.title || name);
      f.setAttribute('data-page', name);
      f.srcdoc = inject(page.html || '', name);
      container.innerHTML = '';
      container.appendChild(f);
      frame = f;
      if (opts.onTitle) opts.onTitle(page.title || name);
      if (opts.onLoad) opts.onLoad(page);
    }

    function post(msg) {
      if (frame && frame.contentWindow) frame.contentWindow.postMessage(Object.assign({ amuxPage: 1 }, msg), '*');
    }

    async function onMessage(e) {
      if (!frame || e.source !== frame.contentWindow) return; // only this page's frame
      const m = e.data || {};
      if (!m.amuxPage || !m.op) return;
      if (m.op === 'title') { if (opts.onTitle) opts.onTitle(m.args && m.args.title); return; }
      if (m.op === 'error') { if (opts.onPageError) opts.onPageError(m.args); return; }
      const reply = (result, error) => post({ id: m.id, result: result, error: error || null });
      try {
        const a = m.args || {};
        if (m.op === 'sql') reply(await api('/sql', { method: 'POST', body: JSON.stringify({ sql: a.sql, params: a.params }) }));
        else if (m.op === 'batch') reply(await api('/sql', { method: 'POST', body: JSON.stringify({ batch: a.batch }) }));
        else if (m.op === 'ask') reply(await api('/ask', { method: 'POST', body: JSON.stringify({ text: a.text, data: a.data, kind: a.kind }) }));
        else if (m.op === 'file') {
          const r = await fetch(base + '/file?path=' + encodeURIComponent(a.path || ''), { headers: headers() });
          if (!r.ok) throw new Error('HTTP ' + r.status);
          const type = r.headers.get('Content-Type') || '';
          if (/^(text\/|application\/json)/.test(type)) reply({ type: type, text: await r.text() });
          else {
            const blob = await r.blob();
            const url = await new Promise(res => { const fr = new FileReader(); fr.onload = () => res(fr.result); fr.readAsDataURL(blob); });
            reply({ type: type, data_url: url });
          }
        } else reply(null, 'unknown call ' + m.op);
        if (m.op === 'sql' || m.op === 'batch' || m.op === 'ask') poll();
      } catch (err) {
        reply(null, err.message || String(err));
      }
    }

    async function poll() {
      if (gone) return;
      let v;
      try { v = await api('/version'); } catch (_) { return; }
      if (!version) { version = v; return; }
      if (v.html !== version.html) {
        version = v;
        if (opts.onReload) opts.onReload();
        load();
        return;
      }
      if (v.db !== version.db) {
        version = v;
        post({ event: 'change', detail: { db: v.db } });
        if (opts.onChange) opts.onChange(v);
      }
    }

    function tick() {
      clearTimeout(timer);
      if (gone) return;
      if (document.visibilityState === 'visible') poll();
      timer = setTimeout(tick, POLL_MS);
    }

    window.addEventListener('message', onMessage);
    const onVis = () => { if (document.visibilityState === 'visible') poll(); };
    document.addEventListener('visibilitychange', onVis);
    load().then(() => { timer = setTimeout(tick, POLL_MS); });

    return {
      reload: load,
      frame: () => frame,
      destroy() {
        gone = true;
        clearTimeout(timer);
        window.removeEventListener('message', onMessage);
        document.removeEventListener('visibilitychange', onVis);
        container.innerHTML = '';
      },
    };
  }

  window.AmuxPages = { mount: mount, inject: inject };
})();
