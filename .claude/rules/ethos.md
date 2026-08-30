---
description: Gut-check for every new feature or enhancement in amux. Read before building.
---

# The amux ethos

**The harness gets better as the models get better. Get out of the model's way.**

Every feature either compounds with model capability or fights it. Run these
checks before you build and again before you call it done. Full incident history
and worked examples: `docs/ethos-incidents.md`.

## The 8 rules

**1. Does capability reach the model, or only exist?**
Who receives this by default, without opting in? Prefer opt-out over opt-in.
When you exempt something from a loop, name what still reaches it. A view must
share the predicate of the mechanism it describes. A caveat about a whole set
belongs at the top level, never inside one arm.

**2. Are you calling the model for something you could just compute?**
Spend model calls on judgment, not string manipulation. A throttle on a model
call usually means the call was wrong.

**3. Can the model comply honestly, or does the design force a lie?**
For every constraint, is there a truthful path forward in every legitimate
state? When a gate does not fit, fix the item's type, not the truth.

**4. Would a wrong answer be detectable from the data you keep?**
When this goes wrong, what will someone see, and will they see it where they
already look? Any output that can read zero or empty must publish, in the same
payload, whether the measurement ran. Ask what your comparison cannot express
(a diff has no MOVE, a count has no identity, a status code has no operand).
Before believing a negative, say what a positive would look like and confirm
the probe could produce it. A wrong answer is rarely wrong-LOOKING, so name what
should appear BESIDE the answer if the probe really ran and check for THAT: a
count beside a zero, a hash beside "adopted", a PASS line beside a green suite,
a key listing beside a None.

**5. Does it accumulate, or does it discriminate?**
At 100x the current volume, is this still coherent? If it becomes a log, it
needed to split, not append.

**6. Is the audit trail real, or just claimed?**
Grep for the thing the docstring promises. Walk every constraint's documented
escape using only the sanctioned tooling. Make the honest path the easy path.

**7. Can your check actually fail?**
Test the shipped code path, not a paraphrase. After any deletion, ask what would
still be green if you broke it. Every name you CALL must exist, and every name
you DEFINE must not already (`tests/dashboard_assets.rs` checks this for the
dashboard). A check pinning the wrong layer is exactly as green as one pinning
the right layer. When a totalizing word appears in your description (whole, all,
every, blanket, total), test at the widest scope the mechanism touches.

**8. Are you deciding something that is the human's to decide?**
Whose data is this? Would they recognise the change as theirs? Report and
recommend; do not sweep.

## Applying this

Before building: 1, 2, 3, 5. Before claiming done: 4, 6, 7. Before touching
anything you did not create: 8.

**The compounding question:** when the next model is meaningfully better than
this one, does this feature get better with it, or does it become the ceiling?

## Known deviations

| ID | Issue | Status | Exit |
|---|---|---|---|
| D1 | Terminal-scraping as control plane | Mitigated. Report endpoint outranks scrape; scrape is fallback for hookless/crashed lanes | Every consumer reads reported state; scrapers demoted to liveness only |
| D2 | amux answering prompts on model's behalf | Mitigated. Policy set once via prefs (`rate_limit_action`, `resume_mode_action`). Only these two prompts, per Ethan | Claude Code exposes rate-limit state via hook/JSON; delete pattern table |
| D3 | Hardcoded weak-model helpers | Fixed. `AMUX_HELPER_MODEL` in server.env | Exit met |
| D4 | Caps on what model may see | Fixed. `AMUX_OBS_EVAL_CAP`/`AMUX_OBS_STATE_CAP` in server.env | Revisit defaults as windows grow |
| D5 | Auto-compact at hardcoded 50% | Mitigated. Pref `auto_compact_threshold` (0 disables) | Models manage own context |
| D6 | Two terminal backends (tmux + herdr) | Accepted. One resolver seam; CI covers tmux only | AgentRuntime seam replaces per-site branches |

## Decisions (do not re-litigate)

**Board state via turn-boundary delivery, not pub-sub.** A session cannot
consume an event faster than its next boundary. The fix is per-case triggers
with named consumers and dedupe keys, not a firehose.

**No CRDT for the board.** The board's property is that every mutation is
attributed and gated. `rev` is a concurrency check whose failure is the
product. CRDTs are for concurrent text editing where merge IS the product.
