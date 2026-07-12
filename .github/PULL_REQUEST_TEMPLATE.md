<!-- Thanks for contributing to amux! Keep PRs small and single-purpose. -->

## What & why

<!-- One or two sentences. What does this change and why? -->

Closes #<!-- issue number -->

## Checklist

- [ ] **One focused change** — single issue, single region of `amux-server.py` where possible
- [ ] **It still parses:** `python3 -c "import ast; ast.parse(open('amux-server.py').read())"`
- [ ] **Client change?** bumped both `const APP_VER` and `const CACHE = 'amux-vX.Y.Z'` (skip for server-only Python)
- [ ] **Tested end-to-end** — I drove the actual UI/endpoint, not just verified it parses
- [ ] **Single-file rule respected** — no new modules/asset files; new subsystems are a well-marked `# ── section ──`, not a new file
- [ ] Rebased on latest `main`

## How I tested

<!-- Commands run, endpoints/UI exercised, before/after screenshots for dashboard changes. -->
