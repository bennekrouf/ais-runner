# Scenarios and the ais-test CLI

A scenario is a recorded, replayable test: set up state, trigger a workflow,
assert on what came out. Scenarios are plain JSON files committed in the
project, next to the workflows they test:

```text
<project>/.ais-runner/scenarios/<name>.json
<project>/.ais-runner/scenarios/<group>/<name>.json   # grouped in the Tests tab
```

The easiest way to make one is **● Create scenario** in the Tests tab: every
successful action becomes a step, to review and edit before saving. Edit the
JSON by hand for anything the recorder can't capture (waits, assertions,
helper processes, settings changes).

## File shape

```json
{
  "name": "Order is written to SQL",
  "description": "A new order message ends up in dbo.Orders",
  "vars": { "orderId": "TEST-001" },
  "steps": [
    { "action": "drain_queue", "queue": "orders.in" },
    { "action": "send_message", "queue": "orders.in",
      "body": "{\"id\": \"{{orderId}}\", \"amount\": 42}" },
    { "action": "wait_for_run", "workflow": "Process-Order", "timeout_ms": 90000 },
    { "action": "wait_for_sql", "database": "orders",
      "sql": "SELECT 1 FROM dbo.Orders WHERE Id = '{{orderId}}'", "min_rows": 1 }
  ]
}
```

- `{{var}}` is replaced with a value from `vars`, or from an earlier step's
  `capture`.
- Relative file paths (`upload_file`, `download_blob`, `run_process` workdir)
  resolve against the project root, so a scenario replays in any checkout.
- Timeouts are in milliseconds and default to 30 000. Service Bus–triggered
  workflows poll once a minute, so give their `wait_for_run` 90 000 or more.

## Steps

Every step has an `"action"`; the other fields are listed with their defaults.

**Blob storage (Azurite)**
- `create_container`: `container`
- `upload_file`: `container`, `file`, `blob_name`. A blob name with `/`
  creates the virtual folder; there's no separate folder step.
- `upload_inline`: `container`, `blob_name`, `content`
- `clear_container`: `container`
- `check_blob_exists`: `container`, `blob_name`, `exists`
- `rename_folder`: `container`, `from`, `to`
- `download_blob`: `container`, `blob_name`, `dest`

**Service Bus**
- `create_queue`: `queue`. Only adds it to the emulator's config; the emulator
  must restart before the queue exists, so create queues before the run.
- `send_message`: `queue`, `body`, `content_type` (default
  `application/json`)
- `drain_queue`: `queue`
- `wait_for_message`: `queue`, `path` (dot path into the message, empty counts
  every message), `expected`, `min_count` (default 1), `timeout_ms`
- `expect`: same fields as `wait_for_message`, checked once without waiting

**SQL (bundled SQL emulator)**
- `create_sql_database` / `drop_sql_database`: `name`
- `run_sql`: `database`, `sql` (`GO`-separated batches are fine), optional
  `capture`
- `truncate_table` / `drop_table`: `database`, `schema`, `table`
- `wait_for_sql`: `database`, `sql`, `min_rows` (default 1), `timeout_ms`

**Cosmos DB**
- `create_cosmos_database`: `database`
- `create_cosmos_container`: `database`, `container`, `partition_key`
  (default `/id`)
- `upsert_cosmos_document`: `database`, `container`, `document`
- `run_cosmos_query`: `database`, `container`, `query`, optional `capture`

**Workflows**
- `run_workflow`: `workflow`, `trigger` (default `manual`), `body`, optional
  `capture`, `expect_trigger_error` (for a trigger that should reject the
  request)
- `wait_for_run`: `workflow`, `timeout_ms`, `expect_status` (default
  `Succeeded`; set `Failed` when the failure path is what's under test). Waits
  for a run that started after the most recent `run_workflow`.
- `expect_action`: `workflow`, `action_name`, `status` (default
  `Succeeded`), optional `contains`
  (text expected in its inputs or outputs), `timeout_ms`. The reliable way to
  check a message that another workflow consumes straight away: assert on the
  action that sent it.

**Environment**
- `set_settings`: `values` (key/value pairs written into
  `local.settings.json`; the old values are kept and put back after the
  scenario or a failure)
- `restore_settings`: puts back everything `set_settings` changed; safe to
  add at the end even when nothing changed
- `restart_func`: `timeout_ms`. Needed after `set_settings`, because the
  runtime reads settings only at startup.
- `run_process`: `command`, `args`, optional `workdir`, `env`,
  `wait_for_port`, `wait_timeout_ms`, `stop_at_end` (default true). Starts a
  helper (typically a stub server) for the scenario's duration, for APIs the
  mock can't intercept. It runs the command as written, with no prompt, so
  read any `run_process` step in a scenario you didn't write.
- `sleep`: `ms`. Prefer a `wait_for_*` step: a fixed sleep is either too short
  or wasted time.

## Writing good scenarios

- Start by putting the emulators in a known state (`drain_queue`,
  `clear_container`, `truncate_table`), so a scenario doesn't pass or fail
  because of what the previous one left behind.
- Assert on outcomes, not only on the run status: a run can succeed while
  writing the wrong thing.
- Keep fixtures under `.ais-runner/fixtures/` and refer to them with relative
  paths.

## Running scenarios headlessly: ais-test

`ais-test` runs the same scenario files without the app, for a terminal or a
CI agent. It drives scenarios only; Azurite, the Service Bus emulator and func
must already be running.

```text
ais-test <project-dir> [OPTIONS]

  --scenario <name>        Only scenarios whose name contains <name>
                           (repeatable, case-insensitive)
  --junit <file>           Write a JUnit XML report
  --list                   List the scenarios found and exit
  --emit-ci <dir>          Write docker-compose.test.yml and the Service Bus
                           emulator's Config.json into <dir>, then exit.
                           Regenerate when workflows change queues.
  --sb-host <host>         Service Bus host (default 127.0.0.1)
  --cosmos-endpoint <url>  Cosmos endpoint (default: emulator)
  --cosmos-key <key>       Cosmos key (default: emulator)
```

Exit codes: `0` every step passed, `1` at least one step failed, `2` usage or
setup error (bad arguments, no scenarios found, unreadable files).

A typical CI job: generate the compose file once with `--emit-ci`, start it,
start `func`, wait for it, then run `ais-test . --junit results.xml` and
publish the report.
