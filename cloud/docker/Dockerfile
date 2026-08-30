# amux per-user workspace image — RUST server (cloud.amux.io).
#
# Replaces the python:3.11-slim image that copied a generated `amux-server.py`
# into /app. That server no longer exists in this repo (deleted 792ce1f), so
# this file is the only thing that can produce a cloud image at all.
#
# BUILD CONTEXT IS THE REPO ROOT, not this directory:
#     docker build -f cloud/docker/Dockerfile -t ghcr.io/mixpeek/amux:latest .
# (the old image built with `context: .` too — the `-f` is what changed).
#
# SINGLE-CODEBASE RULE (CLAUDE.md): there is no cloud-only code in the server.
# Everything environment-specific in here is an env var or the presence of a
# binary. Nothing in crates/ knows it is in a container.

# ─────────────────────────────── build ────────────────────────────────────────
FROM rust:1-slim-bookworm AS build

# Three native deps need a toolchain and none of them announce themselves until
# late in a cold build, which is why they are installed up front rather than
# discovered:
#   * libsqlite3-sys (rusqlite `bundled`) compiles SQLite from C   -> cc
#   * ring (rustls/rcgen) compiles C + asm                          -> cc, perl
#   * aws-lc-sys IS in the dependency graph (rustls 0.23 pulls both
#     providers even though lib.rs pins `ring` at runtime)          -> cmake
# `cargo tree` confirms aws-lc-sys -> aws-lc-rs -> rustls; without cmake the
# build dies ~8 minutes in with a message that reads like a cargo bug.
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake perl pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# THE `amux` CLI IS A BUILD INPUT, NOT ONLY A RUNTIME FILE. It is copied again
# into the runtime stage below (that copy is what puts it on PATH), but the
# build needs it FIRST: invariants/monitor.rs:215 does
#   include_str!("../../../../amux")
# to scan the shell CLI's calls, so cargo cannot compile amux-server without it.
# Missing here, the image build dies with
#   couldn't read crates/amux-server/src/invariants/../../../../amux
# which reads like a path bug in the source rather than an absent build input —
# it broke the cloud deploy on 2026-08-12 and the runtime COPY at the bottom
# makes it look already handled.
COPY amux ./amux
# scripts/ ARE BUILD INPUTS too — the SAME class as `amux` above, recurred
# (AMUX-3033 + AMUX-2936). invariants/monitor.rs include_str!s the versioned hook
# sources to compare the RUNNING hook against the committed one:
#   include_str!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/git-hooks/git-shared-guard.py")
#   include_str!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/hooks/hook-report.sh")
# so cargo cannot compile amux-server without scripts/ present. Missing here, the
# build dies with `couldn't read .../scripts/git-hooks/git-shared-guard.py: No
# such file` — and it stays LATENT until a deploy actually RUNS the build, because
# rust CI compiles from the full checkout where scripts/ exists. It broke the
# FIRST green deploy after main went green (2026-08-15): every prior deploy had
# skipped on red CI, so the build job never ran to surface it. Copy the whole dir
# so the next scripts/ include_str! does not re-break it; build-stage only (the
# content is baked into the binary, the runtime stage needs none of it).
COPY scripts scripts
# frustrations.md is a BUILD INPUT for the same reason, third recurrence of the
# class after `amux` and `scripts/` (AMUX-3633). The ledger checks added in
# 14573a02 include_str! the file to parse the REAL ledger rather than a fixture,
# so a format change that silently zeroes the parser fails there instead of
# turning both checks green.
#
# It is reached from a #[test] today, so a --release image build does not compile
# it and this COPY is insurance rather than a live break. The guard
# (tests/dockerfile_build_inputs.rs) does not distinguish test-only include_str!
# from production ones, and I would rather satisfy it than teach it an exception:
# every previous file in this list started as "probably fine" and then broke a
# deploy. One 200KB text file in the build stage only, which the runtime stage
# does not receive.
COPY frustrations.md frustrations.md

# --release is REQUIRED, not a preference. `rust-embed` is built without the
# `debug-embed` feature, so a DEBUG binary reads the dashboard from
# CARGO_MANIFEST_DIR at runtime — a path that does not exist in the runtime
# stage. The container would boot, answer /health, and serve "dashboard not
# embedded" for every page, with nothing in the logs naming the cause.
#
# The cache mounts make an incremental rebuild cheap, and they are also why the
# binaries are copied to /out INSIDE this RUN: a cache mount is not part of the
# layer, so anything left in /src/target vanishes when the step ends.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release -p amux-server -p amux-cli \
    && mkdir -p /out \
    && cp target/release/amux-server /out/amux-server-rs \
    && cp target/release/amux-rs     /out/amux-rs

# ────────────────────────────── runtime ───────────────────────────────────────
FROM debian:bookworm-slim AS runtime
ENV DEBIAN_FRONTEND=noninteractive

# Runtime binaries the server actually spawns (every one of these is a
# `Command::new` site in crates/amux-server — see cloud/README.md for the map):
#   tmux      session backend (the default; herdr is deliberately absent)
#   git       /api/sessions/<n>/git, worktree + branch resolution
#   python3   tmux `pipe-pane` log writer — WITHOUT IT EVERY SESSION LOG IS
#             EMPTY and nothing reports why (session_verbs.rs:2904)
#   procps    ps / pgrep / pkill — child discovery and session kill
#   ripgrep   /api/fs/search (falls back to an honest "rg not found" message)
#   ffmpeg    /api/file/prepare + transcode
#   chromium  the browser primitive drives Chrome over CDP directly
#   curl      the image HEALTHCHECK below
#   xdg-utils /api/fs reveal-in-file-manager on Linux
# python3-pip / openssh-client / jq / less are not server dependencies: they are
# for the AGENT working in the workspace, which is the actual product. The old
# python image had all of them by virtue of its base image; dropping them would
# be a silent capability regression for every customer.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git tmux procps ripgrep ffmpeg \
      python3 python3-pip python3-venv \
      openssh-client less jq unzip \
      chromium fonts-liberation xdg-utils \
    && rm -rf /var/lib/apt/lists/*

# Node 22 — Debian's `nodejs` is far too old for the Claude Code CLI.
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Claude Code. This MUST run its postinstall: it downloads the platform-native
# binary. A previous build used `--ignore-scripts ... 2>/dev/null || true`,
# which skipped that download and swallowed the error, so every cloud container
# shipped a `claude` that failed at runtime with "native binary not installed".
RUN npm install -g @anthropic-ai/claude-code

# Fail the BUILD, not the customer's session, if the CLI is not runnable.
# Same reasoning for chromium: the browser primitive resolving to nothing is a
# capability that silently does not exist, and this is the last moment it is
# cheap to notice.
RUN claude --version && echo "claude CLI verified at build time" \
    && chromium --version && echo "chromium verified at build time"

# Chrome cannot start with the server's launch flags in a container: there is no
# display and the process is root, so it spawns and then never opens its CDP
# port — which surfaces as `Chrome spawned (pid N) but CDP on port P never
# answered within 12s`, a message that names the symptom and not the cause.
# Verified in the built image before this shim existed.
#
# The server resolves a browser by NAME on PATH (`google-chrome`,
# `google-chrome-stable`, `chromium`, `chromium-browser` — integrations/
# browser.rs), and /usr/local/bin precedes /usr/bin, so a shim here is found
# first. This is a deployment property (a display-less single-tenant container),
# not a server branch: nothing in crates/ learns it is in docker, and an agent
# in the workspace typing `chromium` gets flags that also work for it.
RUN printf '%s\n' \
      '#!/bin/sh' \
      '# amux cloud: chromium with the flags a container needs. See cloud/docker/Dockerfile.' \
      '#   --headless=new        no X display exists here' \
      '#   --no-sandbox          the workspace process is root; the CONTAINER is the sandbox' \
      '#   --disable-dev-shm-usage  /dev/shm defaults to 64MB and chrome dies on it' \
      'exec /usr/bin/chromium --headless=new --no-sandbox --disable-dev-shm-usage --disable-gpu "$@"' \
      > /usr/local/bin/chromium \
    && chmod 0755 /usr/local/bin/chromium \
    && [ "$(command -v chromium)" = "/usr/local/bin/chromium" ] \
    && echo "chromium container shim installed and resolves first on PATH"

COPY --from=build /out/amux-server-rs /usr/local/bin/amux-server-rs
COPY --from=build /out/amux-rs        /usr/local/bin/amux-rs
# The bash CLI is an HTTP CLIENT of the server, not part of the deleted python
# server. Fleet muscle memory (`amux send`, `amux board`) runs through it and
# every CLAUDE.md tells agents to use it, so a workspace without it hands the
# agent instructions it cannot follow (ethos rule 1: capability that never
# reaches the model). It needs curl + python3, both installed above.
COPY amux /usr/local/bin/amux
RUN chmod 0755 /usr/local/bin/amux-server-rs /usr/local/bin/amux-rs /usr/local/bin/amux

WORKDIR /app
ENV HOME=/root

# 8822, not the rust default 8823 or the installer's 8824. One reason now, not
# two: the gateway proxies 127.0.0.1:<host_port> -> :8822 in the container, so
# this must match cloud/docker/docker-compose.template.yml and gateway.py.
#
# The SECOND reason is gone as of 2026-08-10, and the fix is worth knowing about
# because it is what freed this line: the server used to inject
# `AMUX_URL=https://localhost:8822` into every tmux lane with the port HARDCODED
# (session_verbs.rs), so moving this listener silently pointed every agent in the
# workspace at nothing — a failure that surfaced as "amux board did nothing"
# rather than as a port mismatch. It now derives the injected URL from
# `config::canonical_port()`, i.e. from THIS variable. So the container can be
# moved to any port by changing this line plus the two proxy targets, and lanes
# follow automatically.
#
# NOTE: the container's 8822 is unrelated to the LOCAL install's retired 8822
# (crates/amux-server/src/legacy_port.rs). Same number, different deployment;
# nothing here is affected by that retirement.
ENV AMUX_RS_PORT=8822

# WITHOUT THIS, EVERY AGENT LANE IN EVERY WORKSPACE DIES AT LAUNCH.
# The server launches `claude --dangerously-skip-permissions` (provider/
# claude.rs), and Claude Code v2.1.69+ refuses that flag as root:
#     --dangerously-skip-permissions cannot be used with root/sudo privileges
# The pane exits 1 immediately and the worker reports `idle`, so the failure
# looks like an idle session rather than a dead one. Reproduced in this image,
# then fixed and re-checked with a control run: without IS_SANDBOX the refusal
# appears, with it the CLI proceeds (to "Not logged in", i.e. past the gate).
#
# The python image did not need this because the python server STRIPPED the flag
# when `os.getuid() == 0` (amux-server.py:24348) and compensated with a
# permissions allow-list in ~/.claude/settings.json. The rust server has no such
# branch — and it should not grow one, since "am I root" is not the question;
# "am I in an isolated single-tenant container" is, and only the deployment can
# answer that. So the environment declares it here.
ENV IS_SANDBOX=1

# IS_SANDBOX gets the CLI *past* the root-refusal, but Claude Code v2.1.x then
# renders a SEPARATE one-time acceptance screen for --dangerously-skip-permissions
# on every launch ("Claude can make mistakes... only use with code you trust...
# Press Enter to continue"), and it does NOT persist an acceptance — so a fresh
# worker pane sits at that screen forever and the steered first-run prompt lands
# nowhere. (Onboarding is NOT the gate — hasCompletedOnboarding is already true;
# verified on a live container, AC-346.) `skipDangerousModePermissionPrompt` is
# the global setting that suppresses it — same "this is an isolated single-tenant
# container, so accept it up front" justification as IS_SANDBOX above, and like it
# a deployment property, not a server branch. VERIFIED against the repro: with this
# flag, `claude --dangerously-skip-permissions` boots straight to the input prompt,
# no Press-Enter screen. Merge (don't clobber): claude writes `theme` here on first
# run, and jq -s '.[0] * .[1]' preserves whatever else is present.
#
# The OTHER first-run gate — the per-DIRECTORY folder-trust dialog
# (projects[<dir>].hasTrustDialogAccepted) — has no global flag, so it is seeded
# at pane spawn by the server (it knows each worker's CC_DIR). This file only
# handles the global half.
# No process substitution here: Docker RUN is /bin/sh (dash), so merge via a
# temp file, not `<(...)`. jq -s '.[0] * .[1]' deep-merges (the flag wins).
RUN mkdir -p /root/.claude \
    && printf '{"skipDangerousModePermissionPrompt": true}' > /tmp/skip.json \
    && if [ -f /root/.claude/settings.json ]; then \
         jq -s '.[0] * .[1]' /root/.claude/settings.json /tmp/skip.json > /root/.claude/settings.json.tmp \
         && mv /root/.claude/settings.json.tmp /root/.claude/settings.json ; \
       else \
         cp /tmp/skip.json /root/.claude/settings.json ; \
       fi \
    && rm -f /tmp/skip.json \
    && jq -e '.skipDangerousModePermissionPrompt == true' /root/.claude/settings.json > /dev/null \
    && echo "claude skipDangerousModePermissionPrompt seeded (gate-1 suppressed)"

# PER-TENANT: keep the server's OWN health/ops cards off the CUSTOMER board
# (AMUX-3014). autofix files invariant-failure / disk-floor / dead-route cards
# for an operator to action — correct on the single local ops box (its board IS
# the ops board), but each cloud org runs its own server, so those cards land on
# the customer's board. Same "a deployment property, not a server branch"
# justification as IS_SANDBOX: the operator reads cloud health from /health +
# /api/health/invariants, not from cards inside a tenant. Default (unset) is on.
ENV AMUX_OPS_HEALTH_CARDS=0

# The server ALWAYS serves TLS (crates/amux-server/src/lib.rs — there is no
# no-tls switch), so the loopback hop from the gateway is https with a
# self-signed cert minted into /root/.amux/tls on first boot. `curl -k` here is
# not laziness: the cert is per-container and per-volume, and the trust boundary
# is the gateway, not this hop. See cloud/README.md for the one-knob change that
# would remove this hop's TLS entirely.
#
# The healthcheck lives HERE and nowhere else — the compose template used to
# carry its own copy, which is one more thing to keep in step for no gain. It
# checks /health rather than /api/sessions because /health is the endpoint that
# reports `store:"hung"` — a wedged store with a live port is precisely the
# failure a liveness probe on a list endpoint cannot see.
HEALTHCHECK --interval=5s --timeout=3s --start-period=20s --retries=6 \
  CMD curl -fsk https://127.0.0.1:8822/health || exit 1

EXPOSE 8822

# NOTE — capability that does NOT carry over from the python image:
# `AMUX_AUTOSTART_SESSIONS=1` was set here so a replaced container brought its
# owner's sessions back. The Rust server does not implement that variable
# (grep: zero hits in crates/), so setting it would be a lie the deploy tooling
# then acts on. It is left UNSET and tracked in cloud/README.md instead.

CMD ["amux-server-rs"]
