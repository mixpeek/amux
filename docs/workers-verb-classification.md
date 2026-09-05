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

## ALREADY PROMOTED (16)

`start` · `stop` · `peek` · `send` · `duplicate` · `wake` · `reset` · `clear` ·
`resize` · `keys` · `report` · `steer` · `config` · `share` · `instructions` ·
`memory` — first-class routes today. They still resolve through
the catch-all as well, so they are listed for completeness.

`send` landed on AF-202; the rest on AF-288 (a474bbc4, 6ec23d21 and the `report`
commit). Each delegates to the same `*_verb` fn the catch-all runs, so the verb is
addressed from the canonical surface without its behaviour being forked.

## RESOURCE — operations on a worker as a resource. Promote. (0 left)

| verb | note |
|---|---|
| `send` | **First.** The gap #134's title names. `peek` is promoted and its counterpart is not, so a rust-managed worker can be observed and not driven. AF-202 — and see its scope finding: this is not a route addition, because tmux does not implement `ProcessBackend::send_text` and its delivery discipline lives in `session_verbs`. |
| `report` | The harness reporting its own state — D1's exit condition in `ethos.md`, the durable inverse of terminal scraping. |
| `steer` | How board state reaches a lane at its turn boundary (the 2026-08-03 decision against a global bus). Load-bearing. **Promoted (AF-288); see the note below — it is the one verb that is read AND write at a single action.** |
| `keys` | **Not** a duplicate of `send`: `keys` writes to the terminal, `send` delivers a prompt at a turn boundary. Both are needed and the names should say which is which. |
| `duplicate` | Survivor of the `clone`/`duplicate` pair. **Precondition [#137](https://github.com/mixpeek/amux/issues/137) is MET — promoted.** See below. |
| `resize` | Terminal geometry. |
| `wake` | |
| `clear` | |
| `reset` | |

### `steer` is read AND write at one action, and a promoter has to know that

The GET arm lists the lane's steering queue and reads exactly like an
observability endpoint. It is not the whole verb: `dispatch` routes every non-GET
method on this action to `steer_mutate` BEFORE the GET match is reached, so POST
queues and DELETE cancels. A promoted route serving only the arm you find by
searching for `"steer" =>` would drop the half that queues work, and drop it
silently — the read would keep answering.

The write path a reader expects to find is not a route at all: `steer_enqueue`,
`steer_enqueue_store` and `steer_enqueue_precond` are called internally by
board-drive. So "load-bearing" in the row above describes the MECHANISM, and the
verb is the queue's own surface. Both are true and they are different things.

`/api/workers/{id}/steer` is mounted with `any` for this reason: the method split
already lives inside the verb, and expressing it again at the router would put one
decision in two places that can disagree.

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

## SUB-RESOURCE — real capability, wrong shape. GROUPED (AF-291). (6 done, 1 other)

`git` · `git-push` · `dirty` · `tracked-files` · `commit-guard` · `commit-report` ·
`apply-template`

**GROUPED 2026-08-28 (AF-291, d8ae41c9).** The checkout six now live under
`/api/workers/{id}/git/...`: `POST /git` (checkout), `GET /git/{commits,
commit-detail, diff, dirty}`, `POST /git/{push, commit-report}`, and
`/git/{tracked-files, commit-guard}` carrying their own method splits.
`git-push` became `git/push`, since the prefix was doing the grouping the path
now does.

Each sub-verb is routed EXPLICITLY. A `/{id}/git/{*sub}` wildcard would reproduce
the defect AF-204 exists to remove, one level down: an unrouted sub-verb would
answer rather than 404, and the table could not say which parts of the
sub-resource exist.

**`apply-template` RECLASSIFIED out of RESOURCE, 2026-08-28 (AF-288).** It takes no
session. The handler reads its target directory from the BODY (`dir`) and never
consults the name it was addressed to — the compiler said so the moment the arm
became a function, and the parameter is now gone rather than underscored. So it is
an operation on a DIRECTORY that happens to be reachable at a session URL, and
mounting it at `/api/workers/{id}/apply-template` would make the id decorative and
freeze that fiction into the supported API. That is precisely the failure this
epic's scope note warns a one-for-one promotion causes. Its real shape is a
template applied to a work dir; grouping it belongs with the sub-resource question,
not with the worker's verb set.

These are operations on the worker's **checkout**, not on the worker. Promoting them
flat puts six sibling routes on a worker for one sub-resource; they belong under
`/api/workers/{id}/git/...`. The existing sub-verbs `git/commits`,
`git/commit-detail` and `git/diff` already have that shape and are the argument for
it.

This is where "awkward composition is a UX defect *in* the primitives" applies most
directly — fixing the shape here beats routing around it.

## OBSERVABILITY READS — DECIDED 2026-08-28 (AF-292). (9)

Every one is a GET arm, so this was only ever a shape question. The verdicts below
come from reading what each arm DOES, and two of the nine turned out not to belong
in this bucket at all.

| verb | verdict | why |
|---|---|---|
| `transcript` | **promote** `/transcript` | the live conversation, uniform TranscriptEvent for Codex/Ollama and ANSI for Claude. No equivalent anywhere; this is the bucket's real member. |
| `transcripts` | **sub-resource** `/transcript/archives` + `/transcript/archives/{subid}` | NOT the plural of the above. It lists transcript BACKUP files and downloads one by subid. The name `transcripts` belongs to nobody. |
| `last-message` | **fold** into `/transcript` | it is one field of the transcript read, not a resource. |
| `log` | **group** `/log`, `/log/info` | already sub-resource shaped and already called that way by the SPA (`/log/info?plain=1`). Nothing to decide, only to route. |
| `stats` | **promote** `/stats` | `get_claude_stats(CC_DIR)`. Flat, no sub-resource. |
| `subagents` | **promote** `/subagents` | live Claude Code state, flat. |
| `tasks` | **promote** `/tasks` | live Claude Code state, flat. |
| `status-explain` | **promote** `/status/explain` | the status derivation's WHY over the same snapshot the list uses. It is an EXPLAIN, the same family as `env-explain`, and the grouped spelling says so. |
| `search` | **RECLASSIFY to DUPLICATE** | it greps `session_work_dir`, so it is a file read, not an observability read — and `/api/fs/search?root=&q=` already expresses it. What the verb adds is resolving the work dir for you, which `GET /api/workers/{id}` already returns. |

### `transcript` and `transcripts` differ by one letter and are different resources

**THE DANGER IS NOT THE WRONG GUESS. IT IS THAT BOTH ANSWER 200** (amux, 2026-08-28,
sharpening this entry). A 404 is self-correcting: you misspelled it, you find out
at once, nothing downstream believes anything. Two near-identical names over
DIFFERENT resources both returning plausible JSON is the failure that survives —
the caller gets transcript-shaped data and never learns it asked the wrong
question. Same class as an empty commit reporting success, and as a stored green
served off a dead monitor: the tell is always that the wrong answer LOOKS like the
right one. A route table's job is to make a wrong guess FAIL, and one letter
between "the live conversation" and "a list of backup files" defeats that by
construction. No amount of documentation fixes a name that reads correct.

**Decide it NOW, at promotion, because that is when it is free.** The first
version of this entry deferred the rename as "a bigger call than routing one".
That is backwards. Nothing depends on the promoted spelling yet, so naming these
at promotion costs nothing; renaming a SUPPORTED public route later costs a
deprecation cycle and an alias that outlives everyone who remembers why. AF-201's
warning about freezing today's naming into the supported API is an argument for
deciding now — deferring is how the freeze happens.

So the archives become a SUB-RESOURCE of the transcript rather than a homograph
beside it: `/transcript/archives` and `/transcript/archives/{subid}`. That
expresses the real relationship (they are archives OF the transcript) and makes
every wrong guess a 404. The name `transcripts` then belongs to nobody, which is
the correct state for a name that misleads.

**Review status, stated precisely.** The naming above is reviewed and agreed by
amux. The other seven verdicts are NOT reviewed — they wanted a reader with the
room to go through the classification properly, and said so rather than nodding
them through.



`log` · `transcript` · `transcripts` · `last-message` · `tasks` · `subagents` ·
`stats` · `status-explain` · `search`

The question that decides them: is this "what did this worker do" (a worker
sub-resource) or "what happened, filtered by worker" (an observability query with a
worker filter)? The second belongs with the request log and `/api/logs/*`, and
promoting it onto the worker would be the ninth-thing-that-re-expresses-the-primitives
`CLAUDE.md` warns about.

**No verb here is promoted until that pass runs with the actual callers in hand.**
Deferring with the deciding question stated is a verdict; deferring silently is not.

## CONFIG READS — fold into the scope API. (3 left, and 2 were not reads)

`memory` · `memory-inherited` · `memory-explain` · `env-explain` · `instructions`

These read per-scope config, which the uniform scope read/write endpoint already
exists for (`api/mod.rs`, AMUX-2608). Route them there rather than giving the worker
five bespoke config verbs.

### `instructions` and `memory` were NOT reads, and are promoted (AF-293)

Each has a GET arm in `get_dispatch` AND a POST arm in `post_dispatch`:
`instructions` reads and writes `meta.instructions`, `memory` reads the memory
file and writes it plus the project CLAUDE.md. Filing them under CONFIG READS put
a write in a read bucket, and a route mounted with `get` would have promoted half
a verb while the POST half kept falling to the catch-all. Both are now `any`.

`memory` the VERB is also not `memory` the SCOPE CAPABILITY. The capability is
about which level a value comes from; the verb is the file's content. Same word,
two surfaces, and the fold verdict came from reading the word.

### The remaining three are scope's OWN explain surface, and two of them 501

`env-explain` and `memory-explain` are not verbs waiting to be folded somewhere.
They are what `SCOPE_CAPS` already points at: the `explain` field on the `memory`,
`rules` and `env` capabilities names them as the per-worker "why did I get this
value" endpoint. Both answer 501 NOT_IMPLEMENTED today. `memory-inherited` is the
genuine read of the layered composition those two would explain.

So the verdict "fold into the scope API" is right and incomplete: scope already
claims them. Folding without implementing moves a 501 behind a different URL. The
claim is now stated in the descriptor itself rather than left for a caller to
find (ethos rule 6).

## GUARDS — reachable ONLY through the catch-all. PROMOTED, 0 left. (2)

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
