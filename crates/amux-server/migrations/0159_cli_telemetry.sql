-- The agent CLIs' own OpenTelemetry (governance PR 9, api/cli_telemetry.rs).
-- Tool results, rejected tool decisions and API requests a worker's CLI
-- reports land in _amux_tool_events beside the status hook's rows. Each row
-- says where it came from (hook, otel, or hook+otel when both saw the same
-- tool call), which event it was, and which trace its turn belongs to and how
-- that was known (delivered, or unattributed).
ALTER TABLE _amux_tool_events ADD COLUMN trace_id TEXT;
ALTER TABLE _amux_tool_events ADD COLUMN parent_span_id TEXT;
ALTER TABLE _amux_tool_events ADD COLUMN trace_source TEXT;
ALTER TABLE _amux_tool_events ADD COLUMN source TEXT;
ALTER TABLE _amux_tool_events ADD COLUMN event_name TEXT;
CREATE INDEX IF NOT EXISTS idx_amux_tool_events_trace ON _amux_tool_events(trace_id);
CREATE INDEX IF NOT EXISTS idx_amux_tool_events_turn ON _amux_tool_events(worker_id, turn_id);
