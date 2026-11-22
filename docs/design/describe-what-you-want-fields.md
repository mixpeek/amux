# Describe what you want: presets and fields that fill themselves

Owner request, 2026-11-21. Status: built (`openOutcome` in `app.js`; specs
`e2e/describe-outcome.spec.ts`, `e2e/describe-outcome-fields.spec.ts`).

The "Defaults and constraints" section of Describe what you want asked for a
directory, a group and a model as blank text. The owner wanted them filled from
what amux already knows, kept across devices, and saved as presets.

## What each field offers

| Field | Lists | Comes from | Remove an entry |
|---|---|---|---|
| Preset | None, then each saved preset | prefs `outcome_presets` | Delete (asks on the same button) |
| Groups | Pills for the chosen groups; a sideways row of known groups to tap | `outcome_history.groups`, then the fleet's groups (`sessions[].tags`) | x on a known group |
| Working directory | Recent, then folders matching what is typed; Browse | `outcome_history.dirs`, then the fleet's directories; `/api/autocomplete/dir`; `/api/ls` | x on a recent |
| Provider | claude, codex, gemini, then other catalog providers | `/api/models` (`worker_selectable`), never fewer than the three CLIs | none |
| Model | Recent for the provider, then the provider's models | `outcome_history.models[provider]`; `/api/models` | x on a recent |

Every field still takes free text: a directory nobody listed, a group that does
not exist yet, a model id newer than the catalog.

## Where the model list comes from

`/api/models` is the one catalog every worker form already reads (Create, Edit,
Settings, the model filters). It holds a reviewed list per provider
(`catalog_updated_at`, `sources` link each vendor's model page) and, for each
vendor whose key is set in the vault, the vendor's live model list
(`live_catalog.providers[].live`). Only `worker_selectable` models are offered.
Ollama is read from `/api/ollama/models`. Nothing new was added: the dialog
calls `_workerModelsFor(provider)`, the same function as Create.

## Decisions

- **Browse on every device, not only on the Mac running amux.** The worker runs
  on the amux host, so the host's folders are the right list wherever the page
  is open. A native folder picker on the Mac would give the same path, and a
  phone could never use one. Browse reuses the Files view's rows
  (`_feBuildRow`, `_feCrumbsHtml`), folders only, hidden folders left out.
- **History lives in prefs (`outcome_history`), not localStorage.** One list for
  every device; localStorage is the offline copy. Each edit re-reads the
  server's copy and applies one change, so two devices do not undo each other.
- **x on a fleet entry hides it** (`outcome_history.hidden`) until it is used
  again, because the fleet would otherwise offer it back on the next open.
- **The dialog opens on the last preset started with** (`last_preset`, shared
  across devices). Open in worker -> New worker uses the same preset.
- **Lists are capped** at 25 recent entries per field (500 hidden), most recent
  first, so the pref stays small.
