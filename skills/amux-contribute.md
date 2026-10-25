---
description: Change your local amux and send the change upstream as a pull request to github.com/mixpeek/amux, or pull the latest amux from remote and replace what you have locally. e.g. /amux-contribute make the board column headers sticky, or /amux-contribute pull
argument-hint: <the change you want> | pull
allowed-tools: Bash, Read, Edit, Write, Grep, Glob
amux-managed: true
---

# Contribute to amux, or pull the latest

The request: **$ARGUMENTS**

amux ships this skill to `~/.claude/commands/amux-contribute.md` on every
server start. Its source is `skills/amux-contribute.md` in the amux repo. If you
edit the installed copy, delete the `amux-managed: true` line, or the next
server start puts the shipped version back.

Pick the mode from the request:

- **pull**: the request is `pull`, "update", "get the latest", or "match remote".
- **contribute**: anything else. The request describes a change to amux.

## Find the amux checkout

Both modes work on the git checkout your amux server was built from.

```bash
REPO="$(curl -sk "$(amux url)/api/pull" | python3 -c "import json,sys; print(json.load(sys.stdin).get('repo') or '')" 2>/dev/null)"
[ -n "$REPO" ] || REPO="${AMUX_REPO_DIR:-$HOME/Dev/amux}"
git -C "$REPO" remote get-url origin && git -C "$REPO" log -1 --oneline
```

`repo` is the checkout the server reports (`GET /api/pull`). The origin should be
`mixpeek/amux` or the user's fork of it. If that path is not an amux checkout,
ask the user where theirs is.

## Mode: pull (replace local with remote)

This is the Settings button "Pull from remote", with one difference: the button
refuses when local work would be lost, and this mode replaces local work. Every
local commit, edit and untracked file is saved under
`refs/amux/pre-override/<time>` first, so nothing is gone for good.

1. Show what would be replaced, changing nothing:

   ```bash
   cd "$REPO" && amux sync-checkout --override --check
   ```

2. If it lists local commits or changed paths, show the user that list and get
   a yes before going on. Other amux workers may share this checkout; their
   uncommitted edits are in the list too.
   ```bash
   curl -sk "$(amux url)/api/sessions" | python3 -c "import json,sys; [print(s['name'], s.get('dir','')) for s in json.load(sys.stdin) if '$REPO' in (s.get('dir') or '')]"
   ```
   Name any worker that shares the checkout when you ask.

3. Replace, then start the server's builder so the new code runs:

   ```bash
   cd "$REPO" && amux sync-checkout --override
   curl -sk -X POST "$(amux url)/api/pull" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('deploy',{}).get('summary',''))"
   ```

4. Report: the commit the checkout now matches, how many commits and paths
   were replaced, the restore command it printed (`git switch -c restored-local
   refs/amux/pre-override/...`), and the deploy summary. The server picks the
   new build up within a few minutes; Settings shows when it has.

## Mode: contribute (local change, sent as a pull request)

### 1. Work in a fresh worktree at the latest main

Never edit the shared checkout in place: other workers may be using it.

```bash
cd "$REPO" && git fetch -q origin main
SLUG=<short-kebab-name-for-the-change>
WT="$REPO/.worktrees/contribute-$SLUG"
git worktree add -q -b "contrib/$SLUG" "$WT" origin/main && cd "$WT"
```

### 2. Make the change

Read `CLAUDE.md` and `.claude/rules/` in the checkout first. They say where
things live (server in `crates/amux-server`, dashboard in
`crates/amux-dashboard/static`, Bash CLI in `amux`) and what each surface needs.
Keep the change to what was asked. Add or update a test that fails without it.

### 3. Check it

Run what covers the files you touched:

```bash
git diff --name-only origin/main
node --check crates/amux-dashboard/static/app.js     # dashboard JS
bash -n amux                                          # Bash CLI
scripts/safe-cargo.sh check --workspace               # any .rs file
scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings
npx playwright test e2e/<the spec you added>.spec.ts  # dashboard behaviour
```

If something fails, fix it and run it again. Do not open a pull request on a red check.

### 4. Try it before it merges (if the user wants to)

The user's server only runs builds of `origin/main`, so a local branch does not
deploy to it. The Playwright suite boots its own server from this worktree's
code against a throwaway `AMUX_HOME` (`e2e/playwright.config.ts`), so a spec
that clicks through the change, plus a screenshot of it, is the way to see it
working without touching their real amux.

### 5. Commit

```bash
git add -A && git commit -m "<type>: <what changed, in plain words>" -m "<why, and how it was checked>"
```

### 6. Show the user before anything leaves the machine

A pull request is public. Show the user the title, the body, and
`git diff --stat origin/main`, and wait for a yes. The body says what changed,
why, and how it was checked (the commands and their result lines). Write it
plainly: no em-dashes, no "it's not X, it's Y".

### 7. Push and open the pull request

Push to `mixpeek/amux` when the user can, otherwise to their fork:

```bash
UP=mixpeek/amux
if [ "$(gh api "repos/$UP" --jq .permissions.push 2>/dev/null)" = true ]; then
  git push -u origin "contrib/$SLUG"; HEAD_REF="contrib/$SLUG"
else
  gh repo fork "$UP" --remote=false >/dev/null 2>&1 || true
  ME="$(gh api user --jq .login)"
  git push -u "https://github.com/$ME/amux.git" "contrib/$SLUG"; HEAD_REF="$ME:contrib/$SLUG"
fi
gh pr create --repo "$UP" --base main --head "$HEAD_REF" --title "<title>" --body-file <body file>
```

If `gh` is not signed in, tell the user to run `! gh auth login` and stop there.

### 8. Report

Give the user the pull request URL and the checks you ran with their results.
Leave the worktree in place until the pull request merges; remove it after with
`git worktree remove "$WT"`. Once it merges, `/amux-contribute pull` brings
their checkout and server up to date with it.
