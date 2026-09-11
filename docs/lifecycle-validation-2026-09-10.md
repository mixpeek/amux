# Amux lifecycle validation — 2026-09-10

The consolidated suite is in [consolidated-lifecycle.md](consolidated-lifecycle.md)
and runs through `python3 scripts/lifecycle/run.py`. It inventories existing
acceptance coverage and retains selected scope, served-asset hashes, screenshots,
traces, failures, and live-worker evidence. A focused pass is explicitly partial.

## Result

- Real two-worker Claude Sonnet 5 run: **passed after recorded interruptions and
  operator review**. Eleven real tasks and epics reached Verified against the
  amended gate; ten incidental message-capture cards were folded into real work
  and discarded with reasons. No run-owned work remains open.
- Real Sonnet semantic intake: **passed**. Five requests produced three task IDs:
  paraphrase appended, requirement refinement updated, and distinct UI/payroll
  work created separate tasks. Comparison receipts name Sonnet and candidate counts.
- Full browser rerun: **749 passed, four failed, none skipped or flaky**.
  Three failures exposed a delayed global-history refresh erasing new local
  attribution; one Safari failure was teardown restoring an overlay over its
  navigation target. Both causes were repaired, with focused reruns recorded
  below. The original full run remains red in the retained evidence.
- Thirty focused terminal/navigation/composer checks passed across desktop,
  mobile and Safari. Nine additional phone-header, linked-record and changed-gate
  checks passed across those browser projects.
- Final merged browser regression: 50 of 51 passed on clean `d9dc39e8`; the Safari failure was an unrelated-panel scroll dismissing the worker menu during cleanup. The menu now ignores those scrolls; its regression and all delivery outcomes were rerun (12/12 delivery cases passed; the new menu fixture initially needed an explicit reload after creation, then all three menu cases passed)..
- Publication: target branch `main`; the containing commit and delivered validation manifest identify publication and final Clippy evidence.

## User-visible fixes

Ordinary messages persist locally before their accepted draft clears; the local
queue sends to the server in the background. Newer edits and attachment references
survive delayed replies, retry and reload. Failed local storage retains the draft
and causes no network submission. Pending or uncertain server reservations cannot
masquerade as delivery receipts; confirmed retries retain the original response ID.
Logs identify pending, uncertain and accepted message identities. A delayed global
history snapshot merges unechoed local messages instead of erasing them, including
identical text sent to different workers. Background retries continue while the
client believes it is offline, and ordinary queued sends stay quiet.

The reported `mixpeek-general` incident had a confirmed message at 15:20:44 ET and
another history entry with the same content at 15:21:05 ET while its text remained
in the input. History lacked transport IDs, so those entries do not establish that
one transport identity was duplicated. Focused/replaced composer clearing and
server receipt semantics are covered separately.

The board and task details now show linked messages, epics/children/dependencies,
clickable produced files, web/file URLs, commits, and current acceptance evidence.
Changed Verified criteria make old evidence stale and require independent rechecks.
Phone column headers retain the full Verified label and terminal designation.

Further audit repairs include immediate attribution when a terminal frame races
local queue acceptance, authoritative task deep links absent from the board cache,
and stopped-worker status that cannot be resurrected by a stale live report.
Repeated Stop leaves an already-exited shell unchanged. Earlier conversation output
remains available after Stop through Load earlier output; the final observer
explicitly loads those saved records before searching peer messages. Stopping never completes
board tasks. CI now invokes the board-help safety harness and uses Bash for the
cross-group shell harness.

Semantic comparison currently examines up to 80 recent open tasks in the same
ownership scope. It does not merge across owners or automatically reopen finished
work. Explicit graph/gate/callback metadata is preserved. Failed or ambiguous
comparison retains the request and records that comparison was unavailable.

## Actual Sonnet work and interventions

Run `lc-sonnet-complex-1789069500` used an author and reviewer in the same private
group. They built an invoice reconciliation CLI, JSON output, a responsive HTML
report and documentation. They decomposed and linked real board work, exchanged
messages, committed changes, and independently checked each other's outputs.
The reviewer initially ran 15 black-box checks; the author ran 22 checks.

The observer amended the gate to require independent reproduction of invoice totals
and malformed-input diagnostics, duplicate-ID rejection (including identical rows),
and negative-amount rejection. The author expanded its suite to 26 passing checks;
the reviewer committed 22 independent checks. All eleven real records have current
`independent_harness` verification authored by the opposite peer for the exact
three amended criteria. The actual report contains Acme $20, Bravo $5, total $25.

Operator review caught missing produced report files and later missing direct
output references on the three epics. Findings went through actual messages;
workers produced or attached the real evidence themselves. The final epics have
four, three and three direct artifact references. The graph verifier reports a
valid measured graph, and the workers produced genuine `completion.json` and
`review.json` receipts. The observer did not write worker code/evidence or advance
unfinished deliverables.

The reviewer hit a weekly quota banner; the author later reported an account/auth
change. Provider access recovered, and the same conversations were resumed. The
author then picked LSC1A-10 from Todo into Doing and completed it through peer
verification. Permission cancellation, environment repair, operator findings,
resume attempts, accepted-message receipts and final stops are retained. This is
**not** an uninterrupted-autonomy result. Both workers are now stopped, with board
standing orders disabled and their files/messages/tasks preserved.

Earlier same-group/cross-group Sonnet evidence from the already-published
`aa7ca1c9` run includes reject/fix/approve review, uploads, cross-group awareness,
and four Backlog/Todo pickups reaching Done. Those are prior-run evidence,
not additional passes in the latest full browser run.

## Scope and provenance

The first expanded full browser run at clean `89973788` finished 727 passed and
26 failed. Those failures were retained and investigated: some fixtures still
expected inactive subagents and collapsed worker input, while real attribution
and deep-link bugs required repairs. The 30-case rerun passed.

The next full browser run pins clean `9c908fa8`, independently checks actual
served JavaScript/CSS/service-worker hashes, and runs one worker per isolated
browser project. Follow-up server-stop and phone-header changes have their own
scoped validation. The live run spans candidate builds and provider interruptions;
its final proof includes the server health identity and operator interventions.

Additional passing checks: 23 merged outbox (including reconnect retry while believed offline and quiet/stuck banner behavior); 5 CLI help; 3 stable-send-identity; 9 runner
contracts; 89 board API; 5 harness enforcement; 4 message-acceptance plus one real
steering-route retry; 10 mac-health; 2 runtime attribution and one epic-nudge check.
The stopped-report test and real repeated-Stop terminal-byte comparison passed.
The cross-group harness has 12 passing checks; the incoming shepherd CLI harness
has three passing checks. Disk-clear has 23 passed and zero
failed, with the no-peer case explicitly skipped while cargo/rustc were present.

An earlier broad Rust-library run had 2,214 passed, 8 failed and 7 ignored. Six
failures involved host admission under roughly 31 GB swap; two involved a shared
legacy cache race whose serial rerun passed. This is not a full-Rust-suite green
claim. Real external integrations and every possible UI combination are not
certified merely by a successful browser fixture or control-discovery count.

Desktop and phone screenshots were inspected, including terminal messages, long
composer drafts, linked records, full phone column labels, and real produced HTML.
The Send button stays 44 px high; output totals remain readable at 375 px without
horizontal overflow. Overlays are captured at the viewport size so background
page height does not produce misleading oversized phone screenshots.

The latest live observer (`live-complex-final6`) passed on the final menu candidate
after explicitly loading saved output from the stopped workers. Its desktop and
phone screenshots show actual peer messages, linked Verified epics, and outputs.
The prior observer attempt retained a failure because it searched only the shell
view after Stop; saved conversation records were present and load successfully.

CI on `9c908fa8` also exposed two narrowly identified Rust failures: the new
stop-error literals lacked explicit classification in the outcome inventory,
and the sticky board-truth HTTP test did not retry the route's explicit
concurrent-change response. The outcome inventory now explicitly classifies failed stops; the HTTP test
retries only that named response. A concurrent local rerun also exposed cached
rows crossing database instances, so all cache paths now require the owning
store identity. A regression alternates reads across two independent stores.
The full CI run is retained as failed (2,227 passed, two failed, seven ignored).

The local worker module rerun recorded 27 passes and six failures before the
store-identity fix: five worker-start cases received the host's memory-admission
503 (about 30 GB swap), and one received another store's cached rows. Host limits
were not weakened. The scoped final legacy-session and refusal runs identify
what was actually revalidated; this report does not claim a clean full Rust run.

Final scoped Rust repairs passed: seven refusal-status checks and three
legacy-session checks, including alternating stores and sticky board truth.
The consolidated browser/full entry point now executes the 23 outbox contracts
before the browser journey, so queue reliability remains part of the single suite.
