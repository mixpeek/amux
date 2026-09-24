# Security Policy

## Reporting a vulnerability

Please report security issues **privately** — do not open a public issue.

Email **info@mixpeek.com** (or open a [GitHub security advisory](https://github.com/mixpeek/amux/security/advisories/new)) with:

- what the issue is and where (file / endpoint),
- steps to reproduce or a proof of concept,
- the impact you think it has.

We'll acknowledge within a few business days and keep you posted on the fix.

## Scope notes

amux is designed to run on your own machine and is exposed to your local network
by default. A few things are intentional, not bugs:

- **The dashboard has no built-in auth** beyond a bearer token
  (`~/.amux/auth_token`; localhost callers are exempt) — protect it at the
  network layer (Tailscale, firewall) and never expose the server port
  (8824; plus the retired 8822 while its compatibility bind survives) to the internet. See the README §Security.
- **Anything exposed via the tunnel is public** — the URL is unguessable, not
  authenticated.

Reports about these are still welcome if you've found a way they fail worse than
documented.
