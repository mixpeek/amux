<!-- rules: parent=../CLAUDE.md owner=amux-vision enforces=A4,A6,A11 -->
# scripts

The land gate, the builder, the git hooks and the Claude Code hooks, plus test harnesses.

## Shared by every lane (A11)

Every lane's push runs `git-hooks/pre-push` and every deploy runs the builder, so a change
here can stop the whole fleet. It ships with a test that can fail (`test-*.sh`, wired into
`.github/workflows/checks.yml`; `test-harness-wired.sh` refuses one that is wired to nothing)
and a way to turn it off.

- **Never write a script in place while something may be running it.** `cp` and a plain write
  truncate the same inode, and bash reads a running script by offset, so it runs a seam that
  exists in no version of the file. Replace with `scripts/atomic-replace.sh`; run committed bytes
  from a `mktemp` copy (`git show origin/main:<path> > "$(mktemp)"`).
- **Scratch files go to `mktemp`,** never a fixed `/tmp` name: every lane shares `/tmp`.

## The builder

`rust-auto-build-authority.sh` (launchd `com.amux.server-rs-builder`, every minute) picks the
newest `origin/main` commit whose required workflows passed and hands it to
`rust-auto-build.sh`, which builds it in `~/.amux/deploy-build-src`, installs it, and puts the
previous binary back if the new one does not report healthy. `rust-prebuild.sh` compiles the
tip while its CI runs. Deploy settings (`AMUX_CI_REQUIRED_WORKFLOWS`, default `checks`;
`AMUX_DEPLOY_CI_GATE`, `AMUX_DEPLOY_PREBUILD`, `AMUX_ACTIVATION_ROLLBACK`,
`AMUX_ACTIVATION_HEALTH_S`) are read from `~/.amux/server.env`. Log:
`~/.amux/logs/rust-auto-build.log`.

## Hooks

Claude Code tool-event hooks need a `matcher` (regex): `"*"` is invalid, use `".*"`. Verify a
hook by what it wrote, not by the settings file. `install-hooks.sh` installs them.

## Rule tree and slop

`check_rules_tree.py` keeps every `CLAUDE.md` in the tree that `VISION.md` roots (header,
parent, invariant ids, `AGENTS.md` beside it); `--map` prints the tree. `check_slop_ratchet.py`
refuses a push that makes a file it touches sloppier than on its base; `--report` prints the
totals. Both run in `git-hooks/pre-push`.
