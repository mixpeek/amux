# Browser methods: new ways to use a browser, added as configuration

Owner ask, 2026-10-17: browser use keeps getting better, so amux's browser use
should be able to adopt new methods as they come out. The test case was
OpAgent (codefuse-ai, Apache-2.0), a vision-language web agent. Two follow-ups
came in the same session: the route "frequently defaults to chrome CDP which
isn't right", and the work should be verified by replaying the browser
commands actually sent across amux.

## Two kinds of method

`browser-methods.toml` (repo root) is the registry. The owner overrides it in
`~/.amux/browser-methods.toml`, which workers cannot edit (boundaries.toml,
`boundaries-self`).

- **Backends** are where the browser runs: `amux` (the server's Chrome for a
  saved profile), `cdp` (a headless copy of the Chrome profile chosen in
  Browser route settings) and `cua` (the sandboxed desktop). A route tries them
  in `ladder` order. The driver holds them in one registry
  (`BACKENDS` in `scripts/browser-route-driver.mjs`), so a new backend is one
  entry with `start` and `verb`.
- **Agents** decide what to do. `amux browser route goal '{"task":"..."}'`
  hands a natural-language task to an agent, which drives whichever backend
  the route is on through the route's own screenshot and action verbs. An
  agent names a `protocol`, implemented by an adapter module
  (`scripts/browser-agents/<protocol>.mjs`, or the owner's own via `adapter`).
  An adapter is two pure functions: `request(step)` builds the model call from
  the screenshot, task and recent history, and `parse(reply, step)` turns the
  answer into one action in amux's neutral vocabulary, or a final answer.

The loop, the browser, coordinate scaling, the trajectory log and the safety
rules are shared, so a new agent costs an adapter and a registry entry. The
adapter is where a better model plugs in. Nothing in the loop assumes a
particular model, which is the ethos question this design answers: when the
next agent is better, it is one file away.

## OpAgent

`scripts/browser-agents/opagent-v1.mjs` ports OpAgent's single-model mode
(`opagent_single_model/model_interface_llama_cpp.py`): the same 15-tool list,
the same prompt, and the same reply parser. Two tests keep it honest against
the published code. `tests/fixtures/opagent-v1-prompt.txt` is OpAgent's own
Python output for a two-step history, and the JS prompt must match it byte for
byte. `tests/fixtures/opagent-v1-parse.json` holds the Python parser's results
for five replies, and the JS parser must agree on each.

Serving it locally (INT4, about 20 GB of memory, so check `amux resources`):

```bash
# llama.cpp with Metal; OpAgent.Q4_K_M.gguf and mmproj-f16.gguf from
# https://modelscope.cn/models/codefuse-ai/OpAgent-32B-Q4
llama-server --model OpAgent.Q4_K_M.gguf --mmproj mmproj-f16.gguf \
  --port 18080 --ctx-size 4096 --n-gpu-layers 99
```

Then enable it in `~/.amux/browser-methods.toml`:

```toml
[[agent]]
id = "opagent"
enabled = true
# endpoint = "http://<gpu-host>:18080/v1/chat/completions"   # for remote hardware
```

Every agent ships disabled. An agent sees what the browser shows, signed-in
pages included, so pointing it at a remote endpoint sends those screenshots
there. That decision belongs to the owner.

## Why the route kept landing on CDP

Measured on 2026-10-17 from two days of server logs: all 13 CDP routes
followed one cause. The amux backend answered 409 because another session
already ran the same profile (12 of 13 times `mixpeek-general` holding
`ethan`). The ladder then dropped to the CDP rung, which is not the human's
live Chrome. It is a headless copy of the Chrome profile, made once (Oct 8 on
this machine) and never refreshed, so its sign-ins age.

The fix is join. `POST /api/browser/start` with `join: true` (which the route
sends) opens a NEW tab of the caller's own in the browser already running the
profile. The browser already has the live sign-ins. The joiner never adopts
the owner's tabs. A joiner's stop closes only its tab and leaves the browser
running (`browser_joined`, `browser_join_released` verdicts). The route falls
to CDP only if joining fails, or when a caller passes `"join": false`.

A second finding came from replaying real commands. A route started without a
profile always opened the saved default. `amux browser for` named
`ethan-tubescience` for the TubeScience app, yet the route opened `ethan` and
landed on a sign-in page. Now a start with no profile uses the same selector
as `amux browser for` (`site_choice` in `api/browser.rs`), and the receipt
records `profile_chosen_by` (`named`, `site` or `default`).

## Verification

- `tests/browser-methods.test.mjs`: the adapter against OpAgent's fixtures,
  the request shape, the ladder rules and agent selection.
- `tests/browser-goal-e2e.mjs`: a real route and page, and an endpoint that
  answers in OpAgent's exact format. The page itself must record the agent's
  click. It runs once directly and once through the shipped endpoint, enabled
  by an owner override file.
- `tests/browser-routing-e2e.mjs` and `browser-routing-api-e2e.mjs`: the
  existing ladder suites, now asserting that a busy profile is joined and that
  `join: false` still reaches CDP and CUA.
