# Decisions and current work

<!-- Example record. `knos export` writes this file in an adopting repo; it is
     plain markdown, so a fresh clone reads it with nothing installed. -->

## Decisions

- **rule 1** - Whenever you fix a bug: 1) fix it at the root cause, 2) make it surface in amux logs so a sweep would catch it. _(source: CLAUDE.md)_
- **rule 2** - `crates/amux-server` -- axum server: `src/api/`, `src/db/`, `migrations/`, `src/runtime_jobs/` _(source: CLAUDE.md)_

## Being worked on right now

_Nothing claimed._

---
<sub>One record every agent working in this repo reads. Claims lapse after 30
minutes or on `knos done`.</sub>
