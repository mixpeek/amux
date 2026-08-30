# `/api/workers` verb classification (AF-203, epic AF-201)

Ethan's direction call, 2026-08-24: complete `/api/workers`. The legacy per-name
dispatcher is a compatibility surface, not the supported path.

Input: the verb inventory an external contributor posted on
[#134](https://github.com/mixpeek/amux/issues/134), re-enumerated from
`crates/amux-server/src/api/session_verbs.rs`.

## The authoritative count: 42, and what it counts

```
get_dispatch  (9719-10131)   23 match arms
post_dispatch (10742-11160)  21 match arms
                             -4 appear in both (git, instructions, memory, transcripts)
                             = 40 distinct dispatcher verbs
guards that never reach either dispatcher:
  share   (9497)  `if action == "share"` — own handler family, any method
  config  (9480)  the PATCH path
                             = 42
```

Every one of the 42 has exactly one verdict below, and the groups sum to 42.

**NOT counted, and why.** `commits`, `commit-detail` and `diff` are sub-verbs
*inside* `git_get` (10230), reached as `git/<sub>` via `subid`. They are documented
under the git sub-resource because the surface matters, but they are not catch-all
entries and counting them inflates the total. An earlier revision of this doc did
exactly that and reported three different numbers (45 in the header, 41 across the
groups, 42 on re-enumeration) with none named authoritative.

## NOT VERBS — investigated and rejected

Kept rather than deleted, so the next person does not re-derive them from the same
grep that produced them.

| string | what it actually is |
|---|---|
| `manual` | a string ARGUMENT: `"transcripts" => match backup_session_jsonl(name, "manual")` (10750). Its only occurrence in the file. |
| `done` | two match arms on unrelated things: `"stop" \| "done" => -1` (11746, an event delta) and `"done" => "idle"` (11888, status normalization). Never an action. |

Both came from matching quoted strings in Rust source, which cannot separate an
action literal from any other literal. `done` is the more dangerous: an earlier
revision carried it under DUPLICATE with "survivor: `PATCH /api/workers/{id}`" — a
plausible-sounding verdict about a verb that was never there.

## ALREADY PROMOTED (3)

`start` · `stop` · `peek` — first-class routes today. They still resolve through the
catch-all as well, so they are listed for completeness.

## RESOURCE — operations on a worker as a resource. Promote. (10)

| verb | note |
|---|---|
| `send` | **First.** The gap #134's title names. `peek` is promoted and its counterpart is not, so a rust-managed worker can be observed and not driven. AF-202 — and see its scope finding: this is not a route addition, because tmux does not implement `ProcessBackend::send_text` and its delivery discipline lives in `session_verbs`. |
| `report` | The harness reporting its own state — D1's exit condition in `ethos.md`, the durable inverse of terminal scraping. |
| `steer` | How board state reaches a lane at its turn boundary (the 2026-08-03 decision against a global bus). Load-bearing. |
| `keys` | **Not** a duplicate of `send`: `keys` writes to the terminal, `send` delivers a prompt at a turn boundary. Both are needed and the names should say which is which. |
| `duplicate` | Survivor of the `clone`/`duplicate` pair. **Precondition [#137](https://github.com/mixpeek/amux/issues/137) is MET — promoted.** See below. |
| `resize` | Terminal geometry. |
| `wake` | |
| `clear` | |
| `reset` | |
| `apply-template` | |

### `duplicate` had a precondition; it is met, and the verb is promoted

**SETTLED 2026-08-26 (AF-236).** `register_twin` in `session_verbs.rs` writes the
`_amux_workers` row for the copy before either verb returns, and rolls the copied env
file back if the store refuses — so there is no window in which the twin exists
unregistered. `duplicate` is now in `NATIVE_ONLY_HERE` and reachable on a store-managed
worker; `clone` shares the same helper rather than being left to be fixed by whoever
promotes it next, because "slated for retirement" and "cannot mint an invisible session
today" are different properties. The section below is kept as the reasoning, in the past
tense, because the ORDER is the reusable part: the registration had to land before the
exemption, and reversing it re-opens the defect.

The original analysis follows.

### Why it was blocked: promoting it is what made the defect reachable

Reported by @tsukimiya on [#137](https://github.com/mixpeek/amux/issues/137), re-verified
on current `main` by @esteininger — every claim holds, only the line numbers moved.

Both `duplicate` and `clone` copy the env file and **never create a store row**.
`clone_post` then calls `start_session`, so the new session is *running* and unregistered.
Across the whole of `session_verbs.rs` the `workers` table is touched in exactly one
place: `get_worker`, the dispatch guard's own read.

This is NOT a live defect today, and that is the point. `dispatch()` answers 501 for every
verb when the name resolves to a worker row, so neither verb can currently be called on a
store-managed worker. **Exempting `duplicate` from that guard is precisely what would make
it reachable** — and this document is the input to doing exactly that.

So the classification above is incomplete on its own: `duplicate` is the right survivor of
the pair, and promoting it before #137 is settled ships a route that mints unregistered
sessions. Recorded here rather than left on the issue because the issue is a store this
document's reader does not open (ethos rule 4), and because AF-204's own acceptance says
no verb is left to decide later — "decided, blocked on a named precondition" is a
disposition; silently promoting it is not.

## DUPLICATE — another route already expresses this. Retire the verb. (7)

| verb | survivor |
|---|---|
| `info` | `GET /api/workers/{id}` |
| `meta` | `GET /api/workers/{id}` |
| `simple` | `GET /api/workers/{id}` with field selection — not a second endpoint whose only difference is how much it returns |
| `rename` | `PATCH /api/workers/{id}` |
| `archive` | `PATCH /api/workers/{id}` |
| `delete` | `DELETE /api/workers/{id}` |
| `clone` | `duplicate` — pick ONE. Having both is how callers end up split across two spellings of one act. |

Retiring means the alias keeps answering until callers move, not breaking them the
day the canonical route lands.

## SUB-RESOURCE — real capability, wrong shape. Group, do not promote flat. (6)

`git` · `git-push` · `dirty` · `tracked-files` · `commit-guard` · `commit-report`

These are operations on the worker's **checkout**, not on the worker. Promoting them
flat puts six sibling routes on a worker for one sub-resource; they belong under
`/api/workers/{id}/git/...`. The existing sub-verbs `git/commits`,
`git/commit-detail` and `git/diff` already have that shape and are the argument for
it.

This is where "awkward composition is a UX defect *in* the primitives" applies most
directly — fixing the shape here beats routing around it.

## OBSERVABILITY READS — NOT DECIDED, and that is the verdict. (9)

`log` · `transcript` · `transcripts` · `last-message` · `tasks` · `subagents` ·
`stats` · `status-explain` · `search`

The question that decides them: is this "what did this worker do" (a worker
sub-resource) or "what happened, filtered by worker" (an observability query with a
worker filter)? The second belongs with the request log and `/api/logs/*`, and
promoting it onto the worker would be the ninth-thing-that-re-expresses-the-primitives
`CLAUDE.md` warns about.

**No verb here is promoted until that pass runs with the actual callers in hand.**
Deferring with the deciding question stated is a verdict; deferring silently is not.

## CONFIG READS — fold into the scope API. (5)

`memory` · `memory-inherited` · `memory-explain` · `env-explain` · `instructions`

These read per-scope config, which the uniform scope read/write endpoint already
exists for (`api/mod.rs`, AMUX-2608). Route them there rather than giving the worker
five bespoke config verbs.

## GUARDS — reachable ONLY through the catch-all today. (2)

| verb | site | verdict |
|---|---|---|
| `share` | 9497, own handler family, any method | RESOURCE. Needs a first-class route before AF-204. |
| `config` | 9480, PATCH path | DUPLICATE of `PATCH /api/workers/{id}` — but confirm the rename-resume interaction at 9478 first, which is why it is not filed under DUPLICATE above. |

These two are why this section exists rather than being folded upward: neither has a
first-class route (`GET /api/debug/routes` mounts only `/api/workers/{id}`,
`/start`, `/stop`, `/peek`, `/dead-letters` and the catch-all), so **deleting the
catch-all deletes them**. An earlier revision of this doc omitted both entirely,
which reads to the next reader as "does not exist" rather than "undecided".

## Acceptance for the epic

`AF-204` deletes the catch-all once every verb above has landed on its verdict.

The check that can fail is `GET /api/health/invariants` →
`route.callers_have_routes`, which enumerates SPA and CLI call sites against the
mounted table and names each miss. The route table alone cannot tell you a caller was
orphaned.

**Its one blind spot, stated because `share` sits in it:** that invariant enumerates
SPA and CLI callers, and `share` is a public share-link family whose callers are
link holders outside both. It is the single verb here the backstop can least speak
for, so `share` needs its route verified by hand rather than by the invariant.
