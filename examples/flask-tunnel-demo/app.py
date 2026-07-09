#!/usr/bin/env python3
"""amux tunnel demo — a tiny Flask app exposed publicly through the amux tunnel.

Run locally:   python3 app.py [PORT]        (default 8940)
Expose it:     amux tunnel start 8940        → https://<id>.t.amux.io/
Persist it:    set AMUX_TUNNEL_PORT=8940 in ~/.amux/server.env so the tunnel
               auto-targets this app across amux restarts.

It exercises the relay: root-absolute assets, a JSON API, a live counter, and
the request headers as seen by the app (proving traffic really transits the
cloud gateway, not localhost).
"""
import os, sys, time, json
from flask import Flask, request, jsonify, Response

app = Flask(__name__)
STARTED = None          # set on first request (Date.now-free at import time)
HITS = {"n": 0}

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>amux tunnel demo</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="stylesheet" href="/static/style.css">
</head><body>
  <main>
    <div class="badge">live via amux tunnel</div>
    <h1>Hello from <code>localhost:%(port)s</code></h1>
    <p>This page is served by a Flask app on the owner's laptop, reaching you
       through <code>%(host)s</code> over the public internet — no inbound port,
       no router config.</p>
    <div class="row">
      <button id="ping">Ping the local server</button>
      <span id="out">—</span>
    </div>
    <pre id="info">loading…</pre>
  </main>
  <script src="/static/app.js"></script>
</body></html>"""

@app.route("/")
def index():
    return PAGE % {"port": app.config["PORT"], "host": request.headers.get("Host", "?")}

@app.route("/api/ping")
def ping():
    HITS["n"] += 1
    return jsonify({
        "ok": True,
        "hits": HITS["n"],
        "served_by": "flask on 127.0.0.1:%s" % app.config["PORT"],
        "host_header": request.headers.get("Host"),
        "forwarded_proto": request.headers.get("X-Forwarded-Proto"),
        "your_ip_as_seen": request.headers.get("X-Forwarded-For", request.remote_addr),
        "time": int(time.time()),
    })

@app.route("/static/style.css")
def css():
    return Response(
        "*{box-sizing:border-box}body{margin:0;font-family:system-ui,-apple-system,sans-serif;"
        "background:#0d1117;color:#e6edf3;display:flex;min-height:100vh;align-items:center;justify-content:center}"
        "main{max-width:560px;padding:2rem}h1{margin:.4rem 0 1rem;font-size:1.6rem}"
        "code{background:#161b22;padding:.1rem .4rem;border-radius:5px;color:#79c0ff}"
        ".badge{display:inline-block;background:#238636;color:#fff;font-size:.72rem;font-weight:600;"
        "padding:.25rem .6rem;border-radius:999px;text-transform:uppercase;letter-spacing:.04em}"
        "p{color:#9da7b3;line-height:1.5}.row{display:flex;gap:.75rem;align-items:center;margin:1.2rem 0}"
        "button{background:#238636;color:#fff;border:0;border-radius:7px;padding:.6rem 1rem;font-size:.9rem;cursor:pointer}"
        "button:hover{background:#2ea043}pre{background:#161b22;border:1px solid #30363d;border-radius:8px;"
        "padding:1rem;overflow:auto;font-size:.8rem;color:#8b949e}#out{color:#56d364;font-weight:600}",
        mimetype="text/css")

@app.route("/static/app.js")
def js():
    # Root-absolute fetch('/api/ping') — the thing that only works over subdomain tunnels.
    return Response(
        "async function refresh(){const r=await fetch('/api/ping');const j=await r.json();"
        "document.getElementById('info').textContent=JSON.stringify(j,null,2);"
        "document.getElementById('out').textContent='hit #'+j.hits;}"
        "document.getElementById('ping').addEventListener('click',refresh);refresh();",
        mimetype="application/javascript")

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else int(os.environ.get("PORT", "8940"))
    app.config["PORT"] = port
    print("amux tunnel demo on http://127.0.0.1:%s" % port)
    app.run(host="127.0.0.1", port=port, threaded=True)
