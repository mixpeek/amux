---
description: Publish, deploy, host or share an app, page, site, demo, prototype, form or file with amux's own `amux publish`. Use whenever you are about to put something on a link, and before reaching for Netlify, Vercel, Cloudflare Pages, GitHub Pages, Surge, Render, Fly, ngrok or a hosting connector. An outside host only when the owner names it.
argument-hint: <what to publish, and who should open it>
allowed-tools: Bash, Read, Write, Edit
amux-managed: true
---

# Publish with amux

The request: **$ARGUMENTS**

amux is the default host for anything you publish. Its links are listed in the
Apps tab, where the owner approves them, sees them and turns them off. A page on
an outside host is out of the owner's sight and reach, so do not deploy to
Netlify, Vercel, Cloudflare Pages, GitHub Pages, Surge, Render, Fly, ngrok or
similar unless the owner named that host. A connector or CLI for one being
installed is not a reason to use it.

## Pick the link

| Who opens it | Command | Link |
|---|---|---|
| The owner, inside amux (signed in) | `amux publish --internal <port> [title]` | `<amux>/#view=apps&open=<id>` |
| Anyone with the link, an app with a backend | `amux publish <port> [title]` | `<amux>/pub/<id>/` |
| Anyone with the link, a static file or folder | `amux publish --file <absolute path> [title]` | printed by the command |

A file only for the owner needs no link at all: name it by its absolute path and
amux shows it in its file viewer. Do not start a web server for static files.

## What an outside host gave you, built into the app

- **A password.** A public link has no amux sign-in, so the app checks it: one
  shared password, compared on the server, set in an HTTP-only cookie once
  entered. Generate it, keep it out of the repo and give it to the owner.
- **A feedback form.** The app stores submissions itself (a JSON or SQLite file
  in the workspace) and shows them on a page behind the same password.
- **A build step.** Build locally, then serve the output with the app or
  `amux publish --file <dist folder>`.

## The app's own amux credential

A published app gets its own credential, saved by the command to
`~/.amux/app-tokens/<id>.token`. It has **no amux access** unless the owner
allows some: anyone on the web can reach the app, so it must not act as you.

- If the app needs to call amux (post to its worker, read the board), ask when
  publishing: `amux publish PORT [TITLE] --api "POST /api/sessions/<worker>/send"`.
  The owner sees exactly what you asked for in the Apps tab and approves it.
- Start the app with `AMUX_APP_TOKEN` set to the saved credential and have it
  send `X-Amux-App-Token` on its amux calls. Never give the app your own
  `AMUX_WORKER_TOKEN` or the owner token: a call made with them from the app is
  refused.

## After publishing

1. Everything a worker adds is PENDING until the owner approves it in the Apps
   tab. Tell the owner it is waiting there; never try to approve it yourself.
2. Give the owner the link the command printed, built on this amux's address,
   never localhost or a port. Include the password if there is one.
3. `amux publish --list` shows state; `amux publish --revoke <id>` turns one off.

If amux cannot do what was asked (a custom domain, say), tell the owner what is
missing and ask before using an outside host.
