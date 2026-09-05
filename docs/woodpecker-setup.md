# Woodpecker CI — setup

Replaces the ad-hoc `docker --context <host> build -f <scratch-Dockerfile>`
invocations run by hand throughout 2026-09-01 (see `frustrations.md` and
`CLAUDE.local.md`'s remote-build-hosts section) with a real, versioned
pipeline: `.woodpecker.yml` at the repo root.

Two decisions this deliberately leaves open — see AMUX-XX (filed alongside
this doc) for the current state of both:

1. **Which host runs the Woodpecker server + agent(s).** The remote build
   host already in use this session (see `CLAUDE.local.md`, not this file —
   this repo is public and hostnames don't belong here) is the natural
   first candidate: it already has Docker, it's the fastest confirmed
   remote build host by a wide margin, and it already has `amux-rust-base`
   built and warm. Not committing to it here because host selection is
   infra's call, not a build-pipeline design decision.
2. **How builds get triggered** — a GitHub forge connection (webhook,
   needs a GitHub OAuth App or PAT with repo access), a local trigger
   matching how `rust-auto-build.sh` watches this shared checkout's own
   commits, or both. `.woodpecker.yml` itself is trigger-agnostic; nothing
   about the pipeline steps changes based on this choice.

## The one hard prerequisite

Whatever host ends up running the agent, `amux-rust-base` must already be
built there — the pipeline does not build the toolchain itself; that's the
whole point (baking it every run is the slowness this replaces).

```bash
docker context create <name> --docker "host=ssh://<user>@<host>"
docker --context <name> build -t amux-rust-base -f Dockerfile.rust-base .
```

See `Dockerfile.rust-base`'s own header for the full reasoning. Rebuild it
whenever `Cargo.lock` changes enough that the cached deps go meaningfully
stale — there's no automation for this yet; it's a manual, occasional step.

**If the toolchain download itself is flaky on the target host** (this hit
one of this session's remote hosts specifically — its uplink handles
bulk/resumable transfers fine but times out on rustup's single-shot
component downloads; see `CLAUDE.local.md` for which one): don't fight it. `rsync` an already-working `~/.rustup` + `~/.cargo/registry` from
a machine where they exist, `docker cp` them into a plain `debian:trixie`
container, `docker commit` that as `amux-rust-base`. Confirmed working
end-to-end 2026-09-01.

## Installing Woodpecker itself

Not done as part of this — infra owns standing up the actual
server + agent (`docker-compose` is the standard path: a `woodpecker-server`
container plus one or more `woodpecker-agent` containers pointed at it via
`WOODPECKER_AGENT_SECRET`). Official docs:
https://woodpecker-ci.org/docs/administration/deployment/docker

Once a server + agent are live and a repo is registered (whether via forge
webhook or manual trigger), `.woodpecker.yml` at this repo's root is picked
up automatically — nothing else in this repo needs to change for that half.

## Gotchas already paid for, worth carrying forward

- **Disk fills fast on a shared host.** Confirmed live: two `cargo test`
  image builds back to back took a 20G volume from 3.9G free to 0 bytes.
  `docker image prune -f` (no `-a`) between runs is the safe reclaim —
  dangling/untagged images only, never a tagged image nothing currently
  references. `-af` deleted `amux-rust-base` itself once already.
- **`docker container prune -f` is NOT safe on a host with other people's
  infrastructure on it.** It removes every stopped container regardless of
  owner — confirmed live, took down an already-crashed-but-intentionally-
  `restart: unless-stopped` service belonging to unrelated monitoring
  infra on the same host, and recovering it hit a stuck Docker network
  endpoint that needed a full daemon restart to clear. Prune images only,
  never containers, on a host that isn't dedicated to this pipeline.
- **Never `git clone` a commit that only exists locally.** If Woodpecker
  ever needs to build something that hasn't been pushed anywhere (matching
  the local shared-checkout's own auto-builder model), it must build the
  actual worktree, not clone a remote's idea of the branch — a clone
  falling back to the wrong ref on a missing sha is silent and produces a
  binary that looks correct and isn't.
