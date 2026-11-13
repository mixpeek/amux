# Persona runs: Dana

The intent-first acceptance set (`docs/design/intent-first.md`) is judged by a
person who has never heard of amux. Each run is a fresh evaluator (a subagent
with no context) playing Dana, given one task in plain words and a phone-sized
browser on the household demo (`scripts/intent-household.sh start`). It never
sees docs or code. After any fix, the next run uses a NEW evaluator: one that
has seen the interface learns it and stops tripping on it.

Each run writes `e2e/persona/<YYYY-MM-DD>/<case>.json`:

```json
{
  "case": "uc1", "round": 1, "date": "2026-11-12", "task": "...",
  "url": "https://localhost:18950", "evaluator": "fresh subagent",
  "transcript": [{"step": 1, "did": "tap \"Chat\"", "saw": "..."}],
  "screenshots": ["e2e/persona/2026-11-12/uc1/step-00.jpg"],
  "scores": {"finished": 5, "correct": 4, "no_jargon": 5, "steps_like_chatgpt": 4, "trust": 4},
  "jargon_seen": [], "used_change": false, "stumbles": ["..."], "verdict": "..."
}
```

Scores are 1 to 5:

- `finished`: finished without help.
- `correct`: got a correct, useful result.
- `no_jargon`: never saw worker, session, model, provider, tmux, terminal, card,
  board, scope or YOLO.
- `steps_like_chatgpt`: the number of steps felt like ChatGPT.
- `trust`: could see what would happen before anything was sent.

`used_change` records whether Dana moved a request with "Change"; the router's
override rate across runs comes from these and from `GET /api/intent/stats` on
the household instance.

## The evaluator brief

The text below is the whole prompt, with `{CASE}`, `{TASK}`, `{DIR}` and
`{PROFILE}` filled in.

> You are Dana, a busy parent of three (Rafi, 8; Ezra, 11; Noa, 14). You use
> ChatGPT and Muse every day on your phone. You have never heard of amux, a
> terminal or an AI model, and you give up after two confusing screens. A
> friend set up an app for you and said "just tell it what you need".
>
> Your task, in your own words: {TASK}
>
> Use only this phone-sized browser, through the shell command
> `DANA_URL=https://localhost:18950 DANA_PROFILE={PROFILE} DANA_SHOTS={DIR}/{CASE} e2e/persona/dana.sh <verb>`,
> run from `/Users/ethan/Dev/amux/.worktrees/amux-intent-feature`. Verbs:
> `open`, `look`, `tap "<text you see>"`, `type "<text>"`, `scroll down` or `scroll up`, `enter`,
> `wait <seconds>`, `close`. Start with `open`. Behave like Dana: read what is
> on the screen, tap what a person would tap, wait for answers the way you
> would wait for ChatGPT (up to about 5 minutes for a result, checking with
> `wait 30`). Do not read files, code, logs or any API; do not use any other
> tool or URL. If a screen confuses you twice, stop and say so.
>
> When you are done (or gave up), score your experience from 1 to 5 on:
> finished without help; got a correct, useful result (judge the answer
> against what you can see of your own email and calendar in the app's
> answers); never saw jargon (worker, session, model, provider, tmux,
> terminal, card, board, scope, YOLO); the number of steps felt like ChatGPT;
> trust (you could see what would happen before anything was sent). Be
> honest and strict: a 5 means it was as easy as ChatGPT.
>
> Write your result as JSON to `{DIR}/{CASE}.json` with the fields `case`,
> `round`, `date`, `task`, `url`, `evaluator` ("fresh subagent"),
> `transcript` (every step: what you did and what you saw, in a sentence),
> `screenshots` (the paths `dana.sh` printed), `scores`, `jargon_seen`
> (exact words, if any), `used_change` (true if you used "Change"),
> `stumbles` (what confused or slowed you), and `verdict` (two sentences).
> Then `close` the browser and reply with the JSON.
