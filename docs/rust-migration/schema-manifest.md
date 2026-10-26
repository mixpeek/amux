# amux.db — live schema manifest

Generated 2026-08-09 for the Rust rebuild (RR-0019 prerequisite). Source of truth:
`sqlite_master` of the LIVE database, dumped read-only via `.backup` snapshot.

- **DB file (live):** `~/.amux/amux.db` (`_DB_PATH = CC_HOME / "amux.db"`, amux-server.py:9806). WAL mode. The 0-byte `~/.amux/board.db` and repo-root `amux.db` are dead files.
- **User tables:** 47 (plus SQLite-internal `sqlite_sequence`, `sqlite_stat1`, `sqlite_stat4` — auto-managed, not in the baseline migration)
- **Total rows:** 641,805 (as of 2026-08-09)
- **Views / triggers:** none
- **Schema drift note:** the live schema is `_DB_SCHEMA` (amux-server.py:9809) **plus** runtime `ALTER TABLE ... ADD COLUMN` migrations, so several tables (issues, schedules, cmd_history, saved_messages, steering_queue, statuses, issue_tags, graph_nodes, schedule_runs, session_events) have appended columns that are NOT in the `_DB_SCHEMA` string. This manifest and `0001_baseline.sql` reflect the LIVE (post-migration) shape.
- **User-created tables:** the SQL workbench lets users create their own `wb_*` tables; none exist today, but the Rust server must tolerate unknown `wb_*` tables in this file.

## Dead-table candidates

| table | rows | verdict |
|---|---|---|
| `email_accounts` | 0 | **Dead.** Zero code references beyond its CREATE TABLE; Gmail OAuth tokens moved to `~/.amux/gmail-tokens/` (2026-06-29). Safe to drop eventually; kept in the baseline for byte-compat. |
| `dictation_dict` | 0 | Alive (full CRUD API at /api — feature simply unused so far). |
| `org_members`, `waitlist`, `push_subscriptions`, `share_tokens` | 0 | Alive — cloud/PWA feature paths with live read/write code; empty on this local install. |


## Board / issues

### `statuses` — 7 rows

Board status lanes (7 builtins seeded: backlog/todo/doing/review/done/verified/discarded, plus custom), with per-status `gate` text and `mode` (implicit/explicit).

Columns: `id TEXT PK` · `label TEXT NOT NULL` · `position INTEGER NOT NULL DEFAULT 0` · `is_builtin INTEGER NOT NULL DEFAULT 0` · `gate TEXT` · `mode TEXT NOT NULL DEFAULT 'implicit'`

```sql
CREATE TABLE statuses (
    id          TEXT PRIMARY KEY,
    label       TEXT NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    is_builtin  INTEGER NOT NULL DEFAULT 0
, gate TEXT, mode TEXT NOT NULL DEFAULT 'implicit');

```

### `status_scope` — 5 rows

AMUX-2312: opt-in scoping of explicit-mode statuses to a `session` or `tag` scope — a layer of the global>tag>session scope resolver.

Columns: `status TEXT PK NOT NULL` · `scope_type TEXT PK NOT NULL` · `scope_value TEXT PK NOT NULL` · `added_at INTEGER NOT NULL DEFAULT 0` · `added_by TEXT NOT NULL DEFAULT ''`

```sql
CREATE TABLE status_scope (
    status      TEXT NOT NULL,
    scope_type  TEXT NOT NULL,
    scope_value TEXT NOT NULL,
    added_at    INTEGER NOT NULL DEFAULT 0,
    added_by    TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (status, scope_type, scope_value)
);

```

### `session_gates` — 1 rows

Per-session, per-status gate text overrides for board transitions.

Columns: `session TEXT PK NOT NULL` · `status TEXT PK NOT NULL` · `gate TEXT`

```sql
CREATE TABLE session_gates (
    session TEXT NOT NULL,
    status  TEXT NOT NULL,
    gate    TEXT,
    PRIMARY KEY (session, status)
);

```

### `issues` — 6,557 rows

THE BOARD — every card/issue. Soft-delete via `deleted`; gates, `depends_on`, `reviewer`, `type` (drives gate set), optimistic-concurrency `rev`, archive flag, verified timestamp.

Columns: `id TEXT PK` · `title TEXT NOT NULL` · `desc TEXT NOT NULL DEFAULT ''` · `status TEXT NOT NULL DEFAULT 'todo'` · `session TEXT` · `creator TEXT NOT NULL DEFAULT ''` · `due TEXT` · `created INTEGER NOT NULL` · `updated INTEGER NOT NULL` · `deleted INTEGER` · `owner_type TEXT NOT NULL DEFAULT 'human'` · `due_time TEXT` · `pinned INTEGER NOT NULL DEFAULT 0` · `gcal_event_id TEXT` · `pos REAL NOT NULL DEFAULT 0` · `notified INTEGER NOT NULL DEFAULT 0` · `gate TEXT` · `shepherd TEXT` · `type TEXT NOT NULL DEFAULT 'code'` · `archived INTEGER NOT NULL DEFAULT 0` · `depends_on TEXT` · `reviewer TEXT` · `log TEXT` · `rev INTEGER NOT NULL DEFAULT 0` · `source_ref TEXT` · `last_verified_at INTEGER`

```sql
CREATE TABLE issues (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    desc        TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'todo',
    session     TEXT,
    creator     TEXT NOT NULL DEFAULT '',
    due         TEXT,
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    deleted     INTEGER
, owner_type TEXT NOT NULL DEFAULT 'human', due_time TEXT, pinned INTEGER NOT NULL DEFAULT 0, gcal_event_id TEXT, pos REAL NOT NULL DEFAULT 0, notified INTEGER NOT NULL DEFAULT 0, gate TEXT, shepherd TEXT, type TEXT NOT NULL DEFAULT 'code', archived INTEGER NOT NULL DEFAULT 0, depends_on TEXT, reviewer TEXT, log TEXT, rev INTEGER NOT NULL DEFAULT 0, source_ref TEXT, last_verified_at INTEGER);

CREATE INDEX idx_issues_due     ON issues(due) WHERE due IS NOT NULL;

CREATE INDEX idx_issues_session ON issues(session);

CREATE INDEX idx_issues_status  ON issues(status);

CREATE INDEX idx_issues_updated ON issues(updated);

```

### `issue_tags` — 243 rows

Tags per issue (tag-scoped fleet isolation rides on these).

Columns: `issue_id TEXT PK NOT NULL` · `tag TEXT PK NOT NULL` · `added_at INTEGER`

```sql
CREATE TABLE issue_tags (
    issue_id    TEXT NOT NULL,
    tag         TEXT NOT NULL, added_at INTEGER,
    PRIMARY KEY (issue_id, tag),
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE INDEX idx_issue_tags_tag ON issue_tags(tag);

```

### `issue_files` — 1 rows

AMUX-2508: pointer attachments — filesystem paths joined to an issue; deliberately not a blob store (filesystem stays the owner of bytes).

Columns: `issue_id TEXT PK NOT NULL` · `path TEXT PK NOT NULL` · `added_by TEXT` · `added_at INTEGER` · `note TEXT`

```sql
CREATE TABLE issue_files (
    issue_id    TEXT NOT NULL,
    path        TEXT NOT NULL,
    added_by    TEXT,
    added_at    INTEGER,
    note        TEXT,
    PRIMARY KEY (issue_id, path),
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

```

### `issue_counters` — 183 rows

Per-prefix sequence counter for human-readable issue ids (e.g. AMUX-2116).

Columns: `prefix TEXT PK` · `next_n INTEGER NOT NULL DEFAULT 1`

```sql
CREATE TABLE issue_counters (
    prefix      TEXT PRIMARY KEY,
    next_n      INTEGER NOT NULL DEFAULT 1
);

```


## Sessions / workers / messaging

### `tasks` — 1 rows

Per-session lightweight todo checklist items (session sidebar todos, not board cards).

Columns: `id TEXT PK` · `session TEXT NOT NULL` · `text TEXT NOT NULL` · `done INTEGER NOT NULL DEFAULT 0` · `pos INTEGER NOT NULL DEFAULT 0` · `created INTEGER NOT NULL` · `updated INTEGER NOT NULL`

```sql
CREATE TABLE tasks (
    id          TEXT PRIMARY KEY,
    session     TEXT NOT NULL,
    text        TEXT NOT NULL,
    done        INTEGER NOT NULL DEFAULT 0,
    pos         INTEGER NOT NULL DEFAULT 0,
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL
);

CREATE INDEX idx_tasks_session ON tasks(session);

```

### `steering_queue` — 11 rows

Queued steering messages awaiting a session's next turn boundary; `guard` gates delivery.

Columns: `id TEXT PK` · `session TEXT NOT NULL` · `text TEXT NOT NULL` · `queued_at REAL NOT NULL` · `guard TEXT`

```sql
CREATE TABLE steering_queue (
    id          TEXT PRIMARY KEY,
    session     TEXT NOT NULL,
    text        TEXT NOT NULL,
    queued_at   REAL NOT NULL
, guard TEXT);

CREATE INDEX idx_steering_session ON steering_queue(session);

```

### `steering_history` — 2,584 rows

Delivered steering messages log (queue rows are deleted on delivery).

Columns: `id TEXT PK` · `session TEXT NOT NULL` · `text TEXT NOT NULL` · `queued_at REAL` · `delivered_at REAL NOT NULL`

```sql
CREATE TABLE steering_history (
    id           TEXT PRIMARY KEY,
    session      TEXT NOT NULL,
    text         TEXT NOT NULL,
    queued_at    REAL,
    delivered_at REAL NOT NULL
);

CREATE INDEX idx_steering_hist_session ON steering_history(session, delivered_at DESC);

```

### `cmd_history` — 12,625 rows

History of prompts/commands sent to sessions; `origin` = verified sender, `card_id` links to the auto-captured board card.

Columns: `id INTEGER PK` · `text TEXT NOT NULL` · `type TEXT NOT NULL DEFAULT 'direct'` · `session TEXT NOT NULL DEFAULT ''` · `ts INTEGER NOT NULL` · `origin TEXT NOT NULL DEFAULT ''` · `card_id TEXT`

```sql
CREATE TABLE cmd_history (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    text     TEXT NOT NULL,
    type     TEXT NOT NULL DEFAULT 'direct',
    session  TEXT NOT NULL DEFAULT '',
    ts       INTEGER NOT NULL
, origin TEXT NOT NULL DEFAULT '', card_id TEXT);

CREATE INDEX idx_cmd_history_ts ON cmd_history(ts DESC);

```

### `saved_messages` — 3 rows

Per-session canned messages the user saves to re-send later.

Columns: `id INTEGER PK` · `label TEXT NOT NULL DEFAULT ''` · `text TEXT NOT NULL` · `created INTEGER NOT NULL DEFAULT 0` · `pos REAL NOT NULL DEFAULT 0` · `session TEXT NOT NULL DEFAULT ''`

```sql
CREATE TABLE saved_messages (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    label    TEXT NOT NULL DEFAULT '',
    text     TEXT NOT NULL,
    created  INTEGER NOT NULL DEFAULT 0,
    pos      REAL NOT NULL DEFAULT 0
, session TEXT NOT NULL DEFAULT '');

```

### `send_dedup` — 1 rows

10-minute dedupe of inter-session sends keyed (session, msg_id).

Columns: `session TEXT PK NOT NULL` · `msg_id TEXT PK NOT NULL` · `ts INTEGER NOT NULL`

```sql
CREATE TABLE send_dedup (
    session TEXT NOT NULL,
    msg_id  TEXT NOT NULL,
    ts      INTEGER NOT NULL,
    PRIMARY KEY (session, msg_id)
);

```

### `share_tokens` — 0 rows

Tokenized share links granting scoped access (perms, expiry) to one session's output. 0 rows currently; live code path.

Columns: `token TEXT PK` · `session TEXT NOT NULL` · `perms TEXT NOT NULL DEFAULT 'output'` · `created_at INTEGER NOT NULL` · `expires_at INTEGER` · `label TEXT NOT NULL DEFAULT ''`

```sql
CREATE TABLE share_tokens (
    token      TEXT PRIMARY KEY,
    session    TEXT NOT NULL,
    perms      TEXT NOT NULL DEFAULT 'output',
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    label      TEXT NOT NULL DEFAULT ''
);

```

### `group_config` — 1 rows

Per-group (fleet group) config: department, goal, KPIs JSON, human-cost comparison figure.

Columns: `name TEXT PK` · `department TEXT NOT NULL DEFAULT ''` · `goal TEXT NOT NULL DEFAULT ''` · `kpis TEXT NOT NULL DEFAULT '[]'` · `human_cost INTEGER NOT NULL DEFAULT 0` · `updated INTEGER NOT NULL DEFAULT 0`

```sql
CREATE TABLE group_config (
    name       TEXT PRIMARY KEY,
    department TEXT NOT NULL DEFAULT '',
    goal       TEXT NOT NULL DEFAULT '',
    kpis       TEXT NOT NULL DEFAULT '[]',
    human_cost INTEGER NOT NULL DEFAULT 0,
    updated    INTEGER NOT NULL DEFAULT 0
);

```


## Schedules

### `schedules` — 327 rows

Scheduler entries: cron/recurrence (`schedule_expr`), one-shot `run_at`, watch mode (`watch`/`done_pattern`/`done_action`), event triggers (`trigger_on`/`trigger_cooldown`/`trigger_sessions`), `kind` (tmux/...), Google Calendar link.

Columns: `id TEXT PK` · `title TEXT NOT NULL` · `session TEXT NOT NULL` · `command TEXT NOT NULL` · `sched_type TEXT NOT NULL DEFAULT 'once'` · `recurrence TEXT` · `run_at TEXT` · `next_run TEXT` · `last_run TEXT` · `enabled INTEGER NOT NULL DEFAULT 1` · `created INTEGER NOT NULL` · `updated INTEGER NOT NULL` · `deleted INTEGER` · `run_count INTEGER NOT NULL DEFAULT 0` · `schedule_expr TEXT` · `watch INTEGER NOT NULL DEFAULT 0` · `watch_timeout INTEGER NOT NULL DEFAULT 120` · `done_pattern TEXT` · `done_action TEXT NOT NULL DEFAULT 'disable'` · `kind TEXT NOT NULL DEFAULT 'tmux'` · `trigger_on TEXT` · `trigger_cooldown INTEGER NOT NULL DEFAULT 120` · `trigger_sessions TEXT` · `gcal_event_id TEXT` · `exit_actions TEXT`

```sql
CREATE TABLE schedules (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    session     TEXT NOT NULL,
    command     TEXT NOT NULL,
    sched_type  TEXT NOT NULL DEFAULT 'once',
    recurrence  TEXT,
    run_at      TEXT,
    next_run    TEXT,
    last_run    TEXT,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    deleted     INTEGER
, run_count INTEGER NOT NULL DEFAULT 0, schedule_expr TEXT, watch INTEGER NOT NULL DEFAULT 0, watch_timeout INTEGER NOT NULL DEFAULT 120, done_pattern TEXT, done_action TEXT NOT NULL DEFAULT 'disable', kind TEXT NOT NULL DEFAULT 'tmux', trigger_on TEXT, trigger_cooldown INTEGER NOT NULL DEFAULT 120, trigger_sessions TEXT, gcal_event_id TEXT, exit_actions TEXT);

CREATE INDEX idx_schedules_next ON schedules(next_run) WHERE deleted IS NULL AND enabled=1;

```

### `schedule_runs` — 20,469 rows

Run history per schedule; `source` discriminates cron fire vs manual Run-now (ethos rule 4 fix).

Columns: `id INTEGER PK` · `schedule_id TEXT NOT NULL` · `ran_at INTEGER NOT NULL` · `status TEXT NOT NULL DEFAULT 'ok'` · `note TEXT` · `source TEXT NOT NULL DEFAULT 'cron'`

```sql
CREATE TABLE schedule_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    schedule_id TEXT NOT NULL,
    ran_at      INTEGER NOT NULL,
    status      TEXT NOT NULL DEFAULT 'ok',
    note        TEXT
, source TEXT NOT NULL DEFAULT 'cron');

CREATE INDEX idx_sched_runs_ran   ON schedule_runs(ran_at DESC);

CREATE INDEX idx_sched_runs_sched ON schedule_runs(schedule_id, ran_at DESC);

```

### `schedule_audit` — 506 rows

AMUX-1735 forensics: field-level mutation audit for schedules (esp. `enabled` flips), with `source` and `by_who`.

Columns: `id INTEGER PK` · `schedule_id TEXT NOT NULL` · `ts INTEGER NOT NULL` · `field TEXT NOT NULL` · `old_value TEXT` · `new_value TEXT` · `source TEXT` · `by_who TEXT`

```sql
CREATE TABLE schedule_audit (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    schedule_id TEXT NOT NULL,
    ts          INTEGER NOT NULL,
    field       TEXT NOT NULL,
    old_value   TEXT,
    new_value   TEXT,
    source      TEXT,
    by_who      TEXT
);

CREATE INDEX idx_sched_audit_sched ON schedule_audit(schedule_id, ts DESC);

CREATE INDEX idx_sched_audit_ts    ON schedule_audit(ts DESC);

```


## Email

### `email_accounts` — 0 rows

DEAD: legacy Gmail OAuth account storage. Only reference in code is its CREATE TABLE; Gmail tokens live in ~/.amux/gmail-tokens/ since 2026-06-29.

Columns: `id TEXT PK` · `email TEXT NOT NULL` · `access_token TEXT` · `refresh_token TEXT` · `token_expiry INTEGER` · `calendar_id TEXT NOT NULL DEFAULT 'primary'` · `last_synced INTEGER` · `created INTEGER NOT NULL` · `enabled INTEGER NOT NULL DEFAULT 1`

```sql
CREATE TABLE email_accounts (
    id           TEXT PRIMARY KEY,
    email        TEXT NOT NULL UNIQUE,
    access_token TEXT,
    refresh_token TEXT,
    token_expiry INTEGER,
    calendar_id  TEXT NOT NULL DEFAULT 'primary',
    last_synced  INTEGER,
    created      INTEGER NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 1
);

```

### `email_events` — 89 rows

Email→calendar extraction pipeline: candidate events detected in Gmail messages, with status (pending/synced/dismissed/not_event) and the created calendar event id.

Columns: `id TEXT PK` · `account_id TEXT NOT NULL` · `gmail_message_id TEXT NOT NULL` · `gmail_thread_id TEXT` · `email_subject TEXT` · `email_from TEXT` · `email_date TEXT` · `event_title TEXT` · `event_start TEXT` · `event_end TEXT` · `event_location TEXT` · `event_description TEXT` · `calendar_event_id TEXT` · `status TEXT NOT NULL DEFAULT 'pending'` · `raw_extract TEXT` · `created INTEGER NOT NULL`

```sql
CREATE TABLE email_events (
    id               TEXT PRIMARY KEY,
    account_id       TEXT NOT NULL,
    gmail_message_id TEXT NOT NULL,
    gmail_thread_id  TEXT,
    email_subject    TEXT,
    email_from       TEXT,
    email_date       TEXT,
    event_title      TEXT,
    event_start      TEXT,
    event_end        TEXT,
    event_location   TEXT,
    event_description TEXT,
    calendar_event_id TEXT,
    status           TEXT NOT NULL DEFAULT 'pending',
    raw_extract      TEXT,
    created          INTEGER NOT NULL,
    UNIQUE(account_id, gmail_message_id)
);

CREATE INDEX idx_email_events_account ON email_events(account_id);

CREATE INDEX idx_email_events_status  ON email_events(status);

```


## Calendar

### `cal_events` — 83 rows

Real calendar EVENTS — the only calendar layer that syncs to Google via the ICS feed; ISO-8601 start/end, optional RRULE, soft-delete.

Columns: `id TEXT PK` · `title TEXT NOT NULL` · `start TEXT NOT NULL` · `end TEXT` · `all_day INTEGER NOT NULL DEFAULT 0` · `location TEXT` · `description TEXT` · `rrule TEXT` · `created INTEGER NOT NULL` · `updated INTEGER NOT NULL` · `deleted INTEGER`

```sql
CREATE TABLE cal_events (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    start       TEXT NOT NULL,          -- ISO 8601; date-only => all-day
    end         TEXT,                   -- ISO 8601; optional
    all_day     INTEGER NOT NULL DEFAULT 0,
    location    TEXT,
    description TEXT,
    rrule       TEXT,                   -- optional RFC 5545 RRULE (without the "RRULE:" prefix)
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    deleted     INTEGER
);

CREATE INDEX idx_cal_events_start ON cal_events(start) WHERE deleted IS NULL;

```


## CRM

### `crm_contacts` — 308 rows

CRM contacts (name/company/socials/notes), soft-delete.

Columns: `id TEXT PK` · `name TEXT NOT NULL` · `company TEXT NOT NULL DEFAULT ''` · `role TEXT NOT NULL DEFAULT ''` · `email TEXT NOT NULL DEFAULT ''` · `linkedin TEXT NOT NULL DEFAULT ''` · `twitter TEXT NOT NULL DEFAULT ''` · `phone TEXT NOT NULL DEFAULT ''` · `notes TEXT NOT NULL DEFAULT ''` · `created INTEGER NOT NULL` · `updated INTEGER NOT NULL` · `deleted INTEGER`

```sql
CREATE TABLE crm_contacts (
    id       TEXT PRIMARY KEY,
    name     TEXT NOT NULL,
    company  TEXT NOT NULL DEFAULT '',
    role     TEXT NOT NULL DEFAULT '',
    email    TEXT NOT NULL DEFAULT '',
    linkedin TEXT NOT NULL DEFAULT '',
    twitter  TEXT NOT NULL DEFAULT '',
    phone    TEXT NOT NULL DEFAULT '',
    notes    TEXT NOT NULL DEFAULT '',
    created  INTEGER NOT NULL,
    updated  INTEGER NOT NULL,
    deleted  INTEGER
);

CREATE INDEX idx_crm_contacts_upd ON crm_contacts(updated) WHERE deleted IS NULL;

```

### `crm_tags` — 72 rows

Tags per CRM contact.

Columns: `contact_id TEXT PK NOT NULL` · `tag TEXT PK NOT NULL`

```sql
CREATE TABLE crm_tags (
    contact_id TEXT NOT NULL,
    tag        TEXT NOT NULL,
    PRIMARY KEY (contact_id, tag),
    FOREIGN KEY (contact_id) REFERENCES crm_contacts(id) ON DELETE CASCADE
);

```

### `crm_interactions` — 67 rows

CRM interaction log per contact with follow-up date/note.

Columns: `id TEXT PK` · `contact_id TEXT NOT NULL` · `date TEXT NOT NULL` · `type TEXT NOT NULL DEFAULT 'other'` · `notes TEXT NOT NULL DEFAULT ''` · `follow_up_date TEXT` · `follow_up_note TEXT NOT NULL DEFAULT ''` · `created INTEGER NOT NULL` · `updated INTEGER NOT NULL`

```sql
CREATE TABLE crm_interactions (
    id             TEXT PRIMARY KEY,
    contact_id     TEXT NOT NULL,
    date           TEXT NOT NULL,
    type           TEXT NOT NULL DEFAULT 'other',
    notes          TEXT NOT NULL DEFAULT '',
    follow_up_date TEXT,
    follow_up_note TEXT NOT NULL DEFAULT '',
    created        INTEGER NOT NULL,
    updated        INTEGER NOT NULL,
    FOREIGN KEY (contact_id) REFERENCES crm_contacts(id) ON DELETE CASCADE
);

CREATE INDEX idx_crm_ix_contact   ON crm_interactions(contact_id, date DESC);

CREATE INDEX idx_crm_ix_followup  ON crm_interactions(follow_up_date) WHERE follow_up_date IS NOT NULL;

```


## Prefs / UI config

### `prefs` — 72 rows

Server-side key/value preferences (e.g. rate_limit_action, auto_compact_threshold).

Columns: `key TEXT PK` · `value TEXT NOT NULL`

```sql
CREATE TABLE prefs (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

```

### `layout_presets` — 3 rows

Dashboard tab-layout presets (hidden tabs + tab order as JSON).

Columns: `name TEXT PK` · `hidden TEXT NOT NULL DEFAULT '[]'` · `tab_order TEXT NOT NULL DEFAULT '[]'` · `created_at INTEGER NOT NULL`

```sql
CREATE TABLE layout_presets (
    name       TEXT PRIMARY KEY,
    hidden     TEXT NOT NULL DEFAULT '[]',
    tab_order  TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL
);

```

### `skills` — 9 rows

Skill documents by name (content served to sessions; seeded/synced from disk).

Columns: `name TEXT PK` · `content TEXT NOT NULL DEFAULT ''` · `updated INTEGER NOT NULL DEFAULT 0`

```sql
CREATE TABLE skills (
    name       TEXT PRIMARY KEY,
    content    TEXT NOT NULL DEFAULT '',
    updated    INTEGER NOT NULL DEFAULT 0
);

```

### `reports` — 2 rows

Dashboard report widgets (type e.g. infra-spend) with JSON config and cached refresh data.

Columns: `id TEXT PK` · `name TEXT NOT NULL` · `type TEXT NOT NULL DEFAULT 'infra-spend'` · `config TEXT NOT NULL DEFAULT '{}'` · `position INTEGER NOT NULL DEFAULT 0` · `created INTEGER NOT NULL` · `last_refresh INTEGER` · `cached_data TEXT`

```sql
CREATE TABLE reports (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    type         TEXT NOT NULL DEFAULT 'infra-spend',
    config       TEXT NOT NULL DEFAULT '{}',
    position     INTEGER NOT NULL DEFAULT 0,
    created      INTEGER NOT NULL,
    last_refresh INTEGER,
    cached_data  TEXT
);

```


## Events / observability

### `session_events` — 178,219 rows

Issue #48: append-only event-sourced session state — observable transitions and action receipts only; `idem` is a unique idempotency key (INSERT OR IGNORE).

Columns: `id INTEGER PK` · `ts REAL NOT NULL` · `session TEXT NOT NULL DEFAULT ''` · `type TEXT NOT NULL` · `data TEXT` · `idem TEXT` · `source TEXT NOT NULL DEFAULT ''`

```sql
CREATE TABLE session_events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    ts      REAL NOT NULL,
    session TEXT NOT NULL DEFAULT '',
    type    TEXT NOT NULL,
    data    TEXT,
    idem    TEXT,
    source  TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_sev_session ON session_events(session, id);

CREATE INDEX idx_sev_ts      ON session_events(ts);

CREATE INDEX idx_sev_type    ON session_events(type, id);

CREATE UNIQUE INDEX idx_sev_idem ON session_events(idem) WHERE idem IS NOT NULL;

```

### `interaction_log` — 34,261 rows

Unified action ledger — everything an agent or human DID (kind: session|inter_session|browser|schedule); `detail` replays the step, `before` rolls it back, `seq` orders per (kind,target). NOTE: `ts` is in MILLISECONDS (known 1000x-cutoff trap, see ethos rule 7).

Columns: `id INTEGER PK` · `ts INTEGER NOT NULL` · `kind TEXT NOT NULL` · `actor TEXT NOT NULL DEFAULT ''` · `target TEXT NOT NULL DEFAULT ''` · `action TEXT NOT NULL DEFAULT ''` · `url TEXT NOT NULL DEFAULT ''` · `detail TEXT NOT NULL DEFAULT ''` · `before TEXT NOT NULL DEFAULT ''` · `result TEXT NOT NULL DEFAULT ''` · `ok INTEGER NOT NULL DEFAULT 1` · `ms INTEGER NOT NULL DEFAULT 0` · `seq INTEGER NOT NULL DEFAULT 0`

```sql
CREATE TABLE interaction_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    actor       TEXT NOT NULL DEFAULT '',
    target      TEXT NOT NULL DEFAULT '',
    action      TEXT NOT NULL DEFAULT '',
    url         TEXT NOT NULL DEFAULT '',
    detail      TEXT NOT NULL DEFAULT '',
    before      TEXT NOT NULL DEFAULT '',
    result      TEXT NOT NULL DEFAULT '',
    ok          INTEGER NOT NULL DEFAULT 1,
    ms          INTEGER NOT NULL DEFAULT 0,
    seq         INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_ilog_kind   ON interaction_log(kind, ts DESC);

CREATE INDEX idx_ilog_target ON interaction_log(target, ts DESC);

CREATE INDEX idx_ilog_ts     ON interaction_log(ts DESC);

```

### `logs` — 3 rows

Small structured server log (category/action/session/level). Nearly unused (3 rows) — most activity goes to interaction_log.

Columns: `id INTEGER PK` · `ts INTEGER NOT NULL` · `category TEXT NOT NULL DEFAULT 'system'` · `action TEXT NOT NULL` · `session TEXT` · `actor TEXT` · `detail TEXT` · `level TEXT NOT NULL DEFAULT 'info'`

```sql
CREATE TABLE logs (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    ts       INTEGER NOT NULL,
    category TEXT NOT NULL DEFAULT 'system',
    action   TEXT NOT NULL,
    session  TEXT,
    actor    TEXT,
    detail   TEXT,
    level    TEXT NOT NULL DEFAULT 'info'
);

CREATE INDEX idx_logs_category ON logs(category);

CREATE INDEX idx_logs_session  ON logs(session) WHERE session IS NOT NULL;

CREATE INDEX idx_logs_ts       ON logs(ts);

```

### `token_ledger` — 383,677 rows

Per-Claude-turn token/cost accounting parsed from JSONL usage records; attributed to board task via task_windows.

Columns: `id INTEGER PK` · `ts INTEGER NOT NULL` · `session TEXT NOT NULL DEFAULT ''` · `conversation TEXT NOT NULL` · `model TEXT NOT NULL DEFAULT ''` · `input INTEGER NOT NULL DEFAULT 0` · `cache_read INTEGER NOT NULL DEFAULT 0` · `cache_write INTEGER NOT NULL DEFAULT 0` · `output INTEGER NOT NULL DEFAULT 0` · `cost_usd REAL NOT NULL DEFAULT 0` · `task TEXT NOT NULL DEFAULT ''`

```sql
CREATE TABLE token_ledger (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           INTEGER NOT NULL,               -- unix seconds of the turn
    session      TEXT NOT NULL DEFAULT '',        -- owning amux session (customTitle)
    conversation TEXT NOT NULL,                   -- JSONL stem (Claude session uuid)
    model        TEXT NOT NULL DEFAULT '',
    input        INTEGER NOT NULL DEFAULT 0,       -- fresh input tokens
    cache_read   INTEGER NOT NULL DEFAULT 0,
    cache_write  INTEGER NOT NULL DEFAULT 0,       -- cache_creation
    output       INTEGER NOT NULL DEFAULT 0,
    cost_usd     REAL NOT NULL DEFAULT 0,
    task         TEXT NOT NULL DEFAULT ''          -- attributed board task id ('' = ambient)
);

CREATE INDEX idx_ledger_session ON token_ledger(session, ts DESC);

CREATE INDEX idx_ledger_task ON token_ledger(task, ts DESC);

CREATE INDEX idx_ledger_ts ON token_ledger(ts DESC);

```

### `ledger_cursor` — 537 rows

Incremental-parse cursor per conversation JSONL (byte offset + mtime) for token_ledger.

Columns: `conversation TEXT PK` · `offset INTEGER NOT NULL DEFAULT 0` · `mtime INTEGER NOT NULL DEFAULT 0`

```sql
CREATE TABLE ledger_cursor (
    conversation TEXT PRIMARY KEY,
    offset       INTEGER NOT NULL DEFAULT 0,       -- bytes of the JSONL already ledgered
    mtime        INTEGER NOT NULL DEFAULT 0
);

```

### `task_windows` — 458 rows

Time windows a board task was held in 'doing' by a session — bills token_ledger turns to tasks; left_doing NULL = still open.

Columns: `id INTEGER PK` · `task TEXT NOT NULL` · `title TEXT NOT NULL DEFAULT ''` · `session TEXT NOT NULL DEFAULT ''` · `entered_doing INTEGER NOT NULL` · `left_doing INTEGER`

```sql
CREATE TABLE task_windows (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    task          TEXT NOT NULL,
    title         TEXT NOT NULL DEFAULT '',
    session       TEXT NOT NULL DEFAULT '',
    entered_doing INTEGER NOT NULL,
    left_doing    INTEGER                          -- NULL = still open (currently doing)
);

CREATE INDEX idx_task_windows_session ON task_windows(session, entered_doing);

```

### `owner_alerts` — 73 rows

AMUX-1795: durable provenance-stamped ledger of urgent owner alerts — `origin` is server-verified X-Amux-Session, `claimed` is body-reported; mismatch = red flag.

Columns: `id INTEGER PK` · `ts INTEGER NOT NULL` · `origin TEXT NOT NULL DEFAULT ''` · `claimed TEXT NOT NULL DEFAULT ''` · `message TEXT NOT NULL` · `reason TEXT NOT NULL DEFAULT ''` · `channels TEXT NOT NULL DEFAULT ''` · `deduped INTEGER NOT NULL DEFAULT 0`

```sql
CREATE TABLE owner_alerts (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    ts       INTEGER NOT NULL,
    origin   TEXT NOT NULL DEFAULT '',   -- server-verified X-Amux-Session (authoritative)
    claimed  TEXT NOT NULL DEFAULT '',   -- self-reported body 'session' (mismatch = provenance red flag)
    message  TEXT NOT NULL,
    reason   TEXT NOT NULL DEFAULT '',
    channels TEXT NOT NULL DEFAULT '',   -- JSON of the delivery result (push/sms)
    deduped  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_owner_alerts_ts ON owner_alerts(ts DESC);

```


## Auth / cloud multi-tenant

### `org` — 1 rows

Cloud multi-tenant workspace singleton (id 'default').

Columns: `id TEXT PK DEFAULT 'default'` · `name TEXT NOT NULL DEFAULT 'My Workspace'` · `created_at INTEGER NOT NULL`

```sql
CREATE TABLE org (
    id         TEXT PRIMARY KEY DEFAULT 'default',
    name       TEXT NOT NULL DEFAULT 'My Workspace',
    created_at INTEGER NOT NULL
);

```

### `org_members` — 0 rows

Cloud workspace membership (email, role). 0 rows locally; live code path.

Columns: `id TEXT PK` · `email TEXT NOT NULL` · `name TEXT` · `role TEXT NOT NULL DEFAULT 'member'` · `joined_at INTEGER NOT NULL`

```sql
CREATE TABLE org_members (
    id         TEXT PRIMARY KEY,
    email      TEXT NOT NULL UNIQUE,
    name       TEXT,
    role       TEXT NOT NULL DEFAULT 'member',
    joined_at  INTEGER NOT NULL
);

```

### `org_invites` — 4 rows

Cloud workspace invite tokens with expiry/consumption tracking.

Columns: `token TEXT PK` · `email TEXT` · `created_at INTEGER NOT NULL` · `expires_at INTEGER NOT NULL` · `used_at INTEGER` · `used_by TEXT`

```sql
CREATE TABLE org_invites (
    token      TEXT PRIMARY KEY,
    email      TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at    INTEGER,
    used_by    TEXT
);

```

### `waitlist` — 0 rows

Cloud landing-page waitlist emails. 0 rows locally; live code path.

Columns: `id INTEGER PK` · `email TEXT NOT NULL` · `note TEXT` · `ts INTEGER NOT NULL`

```sql
CREATE TABLE waitlist (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    email    TEXT NOT NULL UNIQUE,
    note     TEXT,
    ts       INTEGER NOT NULL
);

```

### `push_subscriptions` — 0 rows

Web-push subscriptions (endpoint + keys) for dashboard push notifications. 0 rows locally; live code path.

Columns: `endpoint TEXT PK` · `p256dh TEXT NOT NULL` · `auth TEXT NOT NULL` · `ua TEXT` · `created INTEGER NOT NULL`

```sql
CREATE TABLE push_subscriptions (
    endpoint TEXT PRIMARY KEY,
    p256dh   TEXT NOT NULL,
    auth     TEXT NOT NULL,
    ua       TEXT,
    created  INTEGER NOT NULL
);

```


## Misc (journal, graph, dictation, infra)

### `journal_entries` — 135 rows

Personal journal entries: text, date, geolocation, star, tags, 3 prompt slots, soft-delete.

Columns: `id TEXT PK` · `text TEXT NOT NULL DEFAULT ''` · `date TEXT NOT NULL` · `created INTEGER NOT NULL` · `updated INTEGER NOT NULL` · `lat REAL` · `lng REAL` · `place_name TEXT NOT NULL DEFAULT ''` · `starred INTEGER NOT NULL DEFAULT 0` · `tags TEXT NOT NULL DEFAULT ''` · `prompt1 TEXT NOT NULL DEFAULT ''` · `prompt2 TEXT NOT NULL DEFAULT ''` · `prompt3 TEXT NOT NULL DEFAULT ''` · `deleted INTEGER`

```sql
CREATE TABLE journal_entries (
    id          TEXT PRIMARY KEY,
    text        TEXT NOT NULL DEFAULT '',
    date        TEXT NOT NULL,
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    lat         REAL,
    lng         REAL,
    place_name  TEXT NOT NULL DEFAULT '',
    starred     INTEGER NOT NULL DEFAULT 0,
    tags        TEXT NOT NULL DEFAULT '',
    prompt1     TEXT NOT NULL DEFAULT '',
    prompt2     TEXT NOT NULL DEFAULT '',
    prompt3     TEXT NOT NULL DEFAULT '',
    deleted     INTEGER
);

CREATE INDEX idx_journal_date ON journal_entries(date DESC) WHERE deleted IS NULL;

```

### `journal_media` — 15 rows

Photos/media attached to journal entries (files on disk, row holds filename/mime/position).

Columns: `id TEXT PK` · `entry_id TEXT NOT NULL` · `filename TEXT NOT NULL` · `mime TEXT NOT NULL DEFAULT 'image/jpeg'` · `position INTEGER NOT NULL DEFAULT 0` · `created INTEGER NOT NULL`

```sql
CREATE TABLE journal_media (
    id          TEXT PRIMARY KEY,
    entry_id    TEXT NOT NULL,
    filename    TEXT NOT NULL,
    mime        TEXT NOT NULL DEFAULT 'image/jpeg',
    position    INTEGER NOT NULL DEFAULT 0,
    created     INTEGER NOT NULL,
    FOREIGN KEY (entry_id) REFERENCES journal_entries(id) ON DELETE CASCADE
);

CREATE INDEX idx_journal_media_entry ON journal_media(entry_id);

```

### `graph_nodes` — 68 rows

Knowledge-graph canvas nodes (label/body/color/folder/xy/pinned; `source_path` links a node to a file).

Columns: `id TEXT PK` · `graph_id TEXT NOT NULL DEFAULT 'default'` · `label TEXT NOT NULL` · `body TEXT NOT NULL DEFAULT ''` · `color TEXT NOT NULL DEFAULT '#ffffff'` · `folder TEXT NOT NULL DEFAULT ''` · `x REAL` · `y REAL` · `pinned INTEGER NOT NULL DEFAULT 0` · `created INTEGER NOT NULL` · `updated INTEGER NOT NULL` · `source_path TEXT NOT NULL DEFAULT ''`

```sql
CREATE TABLE graph_nodes (
    id          TEXT PRIMARY KEY,
    graph_id    TEXT NOT NULL DEFAULT 'default',
    label       TEXT NOT NULL,
    body        TEXT NOT NULL DEFAULT '',
    color       TEXT NOT NULL DEFAULT '#ffffff',
    folder      TEXT NOT NULL DEFAULT '',
    x           REAL,
    y           REAL,
    pinned      INTEGER NOT NULL DEFAULT 0,
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL
, source_path TEXT NOT NULL DEFAULT '');

CREATE INDEX idx_graph_nodes_graph ON graph_nodes(graph_id);

```

### `graph_edges` — 103 rows

Knowledge-graph edges between nodes, per graph_id.

Columns: `id TEXT PK` · `graph_id TEXT NOT NULL DEFAULT 'default'` · `source TEXT NOT NULL` · `target TEXT NOT NULL` · `label TEXT NOT NULL DEFAULT ''` · `created INTEGER NOT NULL`

```sql
CREATE TABLE graph_edges (
    id          TEXT PRIMARY KEY,
    graph_id    TEXT NOT NULL DEFAULT 'default',
    source      TEXT NOT NULL,
    target      TEXT NOT NULL,
    label       TEXT NOT NULL DEFAULT '',
    created     INTEGER NOT NULL,
    FOREIGN KEY (source) REFERENCES graph_nodes(id) ON DELETE CASCADE,
    FOREIGN KEY (target) REFERENCES graph_nodes(id) ON DELETE CASCADE
);

CREATE INDEX idx_graph_edges_graph ON graph_edges(graph_id);

```

### `dictation_history` — 20 rows

Voice-dictation transcripts: raw model output, shown/edited text, pre-AI-edit copy for undo, word count, duration.

Columns: `id INTEGER PK` · `session TEXT NOT NULL DEFAULT ''` · `ts INTEGER NOT NULL` · `text TEXT NOT NULL` · `raw_text TEXT NOT NULL DEFAULT ''` · `prev_text TEXT NOT NULL DEFAULT ''` · `ai_edited INTEGER NOT NULL DEFAULT 0` · `words INTEGER NOT NULL DEFAULT 0` · `dur_ms INTEGER NOT NULL DEFAULT 0`

```sql
CREATE TABLE dictation_history (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    session   TEXT NOT NULL DEFAULT '',
    ts        INTEGER NOT NULL,
    text      TEXT NOT NULL,
    raw_text  TEXT NOT NULL DEFAULT '',
    prev_text TEXT NOT NULL DEFAULT '',   -- pre-AI-edit copy, for "Undo AI edit"
    ai_edited INTEGER NOT NULL DEFAULT 0,
    words     INTEGER NOT NULL DEFAULT 0,
    dur_ms    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_dictation_ts ON dictation_history(ts DESC);

```

### `dictation_dict` — 0 rows

Personal dictation vocabulary: plain terms or misspelling→correct mappings. 0 rows currently; live code path.

Columns: `id INTEGER PK` · `word TEXT NOT NULL` · `correct TEXT NOT NULL DEFAULT ''` · `created INTEGER NOT NULL`

```sql
CREATE TABLE dictation_dict (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    word     TEXT NOT NULL,
    correct  TEXT NOT NULL DEFAULT '',
    created  INTEGER NOT NULL,
    UNIQUE(word, correct)
);

```

### `proxies` — 1 rows

Registered reverse-proxy/tunnel entries (amux tunnel: name, port, scheme). Retired with the Proxies tab (MSG-95893): kept on disk as the owner's data and no longer read; boot logs `retired_proxies_data_present` when rows exist.

Columns: `id TEXT PK` · `name TEXT NOT NULL` · `port INTEGER NOT NULL` · `scheme TEXT NOT NULL DEFAULT 'http'` · `created_at INTEGER NOT NULL` · `last_started INTEGER`

```sql
CREATE TABLE proxies (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    port         INTEGER NOT NULL,
    scheme       TEXT NOT NULL DEFAULT 'http',
    created_at   INTEGER NOT NULL,
    last_started INTEGER
);

```
