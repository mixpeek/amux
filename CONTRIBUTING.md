# Contributing to amux

Thanks for your interest in amux! It's an intentionally small, hackable project — one Python file with an inline dashboard — so contributing is meant to be low-friction.

## The one rule: it's a single file

The entire server **and** dashboard live in [`amux-server.py`](amux-server.py) — Python backend with inline HTML/CSS/JS. This is a deliberate design choice ([why](https://amux.io/features/single-file-architecture/)), not tech debt.

- **Never split it** into modules or separate asset files.
- After any edit, verify it still parses:
  ```bash
  python3 -c "import ast; ast.parse(open('amux-server.py').read())"
  ```
- The server watches its own mtime and **auto-restarts on save**, so changes are live immediately.

## Local development

```bash
./install.sh          # or just run it directly:
python3 amux-server.py
```

Open `https://localhost:8822`. It uses a self-signed cert, so accept the browser warning (or trust the local CA it serves at `/ca`).

## Project layout

| Path | What |
|------|------|
| `amux-server.py` | The whole product — server + dashboard (single file) |
| `amux`, `amux-remote` | CLI entry points (bash) |
| `mcp.json` | Shared MCP server config |
| `assets/` | App/PWA icons |
| `desktop/` | macOS desktop wrapper (Swift + build script) |
| `android/`, `ios/` | Native mobile wrappers |
| `cloud/` | Cloud gateway + VM provisioning (the hosted tunnel/SSO tier) |
| `docs/` | Guides and reference |
| `examples/` | Runnable examples (e.g. the Flask → tunnel demo) |
| `scripts/`, `tests/` | Tooling and tests |
| `site/` | The marketing site (deployed separately) |

## Making changes

1. **Branch** off `main`.
2. Keep the change focused — one logical change per PR.
3. If you touch the **client** (HTML/CSS/JS in `amux-server.py`), bump both `const APP_VER` and `const CACHE = 'amux-vX.Y.Z'` together so the service worker updates. Server-only Python changes don't need a bump.
4. **Verify it works end-to-end**, not just that it parses — drive the actual UI/endpoint you changed.
5. Match the surrounding style; the dashboard has no build step or framework.

## Building the harness roadmap

amux is becoming the **durable operating system around agents**: it owns execution, state, isolation, recovery, observability, and verification — the model owns reasoning. The roadmap for that layer lives in **[the roadmap epic](https://github.com/mixpeek/amux/issues/46)** and its linked issues.

### Seams first, then leaves

The big architectural pieces are built **seams-first**:

- **Seams (maintainer-owned):** the two interfaces everything else plugs into — the [agent runtime contract + capability registry](https://github.com/mixpeek/amux/issues/47) and [event-sourced session state](https://github.com/mixpeek/amux/issues/48). These land first.
- **Leaves (great for contributors):** once a seam exists, the work it unlocks is naturally parallel and self-contained — **provider adapters, verification runners, MCP tools, eval scenarios, policy hooks**. If a leaf issue is blocked on a seam, it says so; watch the epic for when it opens up.

If you want to help, comment on a `help wanted` roadmap issue to claim it, or propose a smaller sub-task on the epic.

### Working in one file, in parallel

The whole product is one ~50k-line file, so a dozen contributors can't hack on it at once without colliding. Keep merges clean:

- **Claim before you start** — comment on the issue so two people don't edit the same region.
- **One issue per PR, one region per PR.** Small, single-purpose diffs merge; sprawling ones rot in conflicts.
- **Rebase on `main` right before you push** — the file moves fast.
- **Be additive.** Prefer new functions/handlers behind a clearly-labelled `# ── <feature> ──` section header over rewriting shared regions, so conflicts localize and reviewers can find your code.
- New subsystems still obey the single-file rule — a "module" here is a well-marked section, not a new file.

## Mobile & CSS

The dashboard is a mobile-first PWA. New UI must fit the existing `@media (max-width: 600px)` breakpoints, keep touch targets ≥ 44×44, and use `env(safe-area-inset-*)` for anything at screen edges (iOS notch).

## Reporting bugs / ideas

Open an issue with what you did, what you expected, and what happened. Screenshots of the dashboard help. Security-sensitive reports: please disclose privately rather than in a public issue.

## License

By contributing, you agree your contributions are licensed under the repository's [LICENSE](LICENSE) (MIT + Commons Clause).
