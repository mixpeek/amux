# amux tunnel demo (Flask → public HTTPS)

A tiny Flask app that gets exposed publicly through the **amux tunnel** — your
laptop dials out to the amux cloud gateway, which relays public requests back
down. No inbound port, no router config.

## Run

```bash
python3 app.py 8940                 # serve locally on 127.0.0.1:8940
amux tunnel start 8940              # publish it → prints https://<id>.t.amux.io/
amux tunnel url                     # the public URL
```

Open the public URL from any device — the page, its CSS/JS (root-absolute
paths), and the `/api/ping` JSON all transit the cloud gateway.

## Keep it running

- **Across amux restarts:** set `AMUX_TUNNEL_PORT=8940` in `~/.amux/server.env`
  so the tunnel auto-targets this app (otherwise a restart reverts it to the
  dashboard on 8822).
- **Across reboots / logout:** load the launchd job:
  ```bash
  cp com.amux.flask-demo.plist ~/Library/LaunchAgents/
  launchctl load ~/Library/LaunchAgents/com.amux.flask-demo.plist
  ```

## Note

Anything you tunnel is **public** — the URL is unguessable, not authenticated.
This demo exposes only harmless read endpoints.
