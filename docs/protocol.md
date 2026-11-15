# Local JSON protocol v1

Socket: `/run/pinwatch/pinwatch.sock`, mode **0660**, normally `root:pinwatch`.
Its parent is 0750 and must not be group/world writable. Read-only clients need
membership of the socket group; they need no NVML/I²C access. Socket write permission
means permission to connect, **not** permission to control hardware. There are no
control operations. Use `pinwatch status --json` and `pinwatch watch --json` rather
than implementing a socket client unless you need to.

Send exactly one UTF-8 JSON line per connection:

```json
{"schema_version":1,"op":"snapshot"}
```

or:

```json
{"schema_version":1,"op":"subscribe"}
```

Snapshot returns one telemetry frame then closes. Subscribe immediately returns
current state plus recent events, then complete frames on sample updates and
approximately one-second heartbeats (age changes even when the hardware worker
is stuck). A heartbeat can repeat a sample/event. Do not interpret it as a new
hardware read. Unsupported versions/operations, unknown request fields, malformed
JSON, overlong requests, or missing newlines close the connection; diagnostics
stay on the daemon's stderr. No mixed plaintext responses exist.

Requests are capped at 4096 bytes and three seconds; frames at 1 MiB. There are at
most 32 simultaneous clients. Each client has a latest-value cache receiver, not
an unbounded sample backlog. A writer blocked for two seconds is disconnected.
Events are repeated in a **64-event ring** so ordinary skipped samples do not lose
interventions. A sufficiently slow/long-offline client can lose intermediate
history: consult journald for the full event stream. Protection never waits for
clients. Daemon shutdown closes streams; consumers must mark their display
unavailable and reconnect (the notifier implements bounded backoff).

## Telemetry frame

[`examples/telemetry.json`](../examples/telemetry.json) is an actual simulated
snapshot with normalized timestamps. Fields are required unless described as
nullable. Version 1 allows new fields; consumers should ignore unknown response
fields but reject an unsupported `schema_version`. Do not parse human-readable
`reason` strings as state identifiers.

| Field | Meaning |
|---|---|
| `schema_version` | Integer `1` |
| `kind` | `telemetry` |
| `daemon_instance` | Random instance UUID; direct mode uses `direct-read-only` |
| `emitted_at_unix_ms` | Frame emission wall time in milliseconds since Unix epoch |
| `max_sample_age_ms` | Freshness policy for this stream |
| `gpus` | Selected GPU snapshots (UUID filtering also filters events) |
| `recent_events` | Bounded recent events, oldest first, repeated on subsequent frames |

Each GPU snapshot contains:

- `device`: `gpu_uuid` (stable identity), `index` (current enumeration only),
  `pci_address` (domain:bus:device.function), `model`, `sensor_supported`,
  `i2c_adapters`, nullable `selected_adapter`, nullable `sensor_error`, and
  `capabilities`. Capability fields are nullable `power_min_w`, `power_max_w`,
  `graphics_min_mhz`, `memory_min_mhz`, `control_status`, and `errors`.
  Queryable ranges do not prove control permissions.
- `reading`: `sample_sequence`, `sampled_at_unix_ms` (start of acquisition),
  `sample_duration_ms`, nullable `channels`, nullable `connector_total_w`,
  nullable `nvml_board_power_w`, nullable `power_limit_w`, nullable
  `power_balance_ratio`, nullable `current_balance_ratio`, `sensor_errors`,
  and `nvml_errors`.
- `channels` is either **null** or exactly six records with `channel_id` 1–6,
  `voltage_v`, `current_a`, and `power_w`. Power is voltage × current. No physical
  contact numbering is promised. The packet must be exactly 24 bytes: six
  big-endian u16 millivolt/u16 milliamp pairs.
- Connector total is the sum of channel powers. NVML board power comes from a
  different acquisition path and may have different timing/averaging. Neither is
  substituted for the other. Cap is the configured NVML cap sampled that poll,
  not an inferred consumption limit. A subsequent action's confirmed cap appears
  in its outcome before the next telemetry read catches up.
- Both balance ratios are minimum/maximum of the named quantity, dimensionless.
  `power_balance_ratio` corresponds to the old `mismatch`. A zero maximum gives
  null, not NaN. Zero current is valid; zero supply voltage invalidates the packet.
- `sample_age_ms` is computed using monotonic elapsed time, including acquisition
  time; `freshness` is `fresh`, `unavailable`, or `stale`. Stale numbers remain
  explicitly labeled rather than becoming zeros. `sensor_errors` and
  `nvml_errors` identify unavailable measurements; empty arrays mean no read error.
  A `fresh` sensor packet alone does not mean NVML or protection is available.
- `protection`: `mode` (`monitor`, `dry_run`, `active`), `state`, `effective`,
  `reason`, nullable `last_action`, nullable `last_outcome`, nullable
  `historical_restriction`. States are `initializing`, `monitoring`,
  `armed_unverified`, `dry_run`, `overload_unprotected`, `waiting`, `restricted`,
  `degraded`. **Check freshness and protection separately**. `effective` records
  known driver-confirmed controls, not connector safety. Historical restrictions
  are records from earlier runs; current hardware state must not be inferred from
  them. See [policy semantics](protection.md).

`devices --json` instead emits `{schema_version:1, kind:"devices", devices:[...]}`.
By default it uses the daemon's cached supported devices; `--direct devices` queries
all NVML GPUs/capabilities without touching I²C, including unsupported hardware.

## Event contract

[`examples/intervention.json`](../examples/intervention.json) demonstrates a
**synthetic** confirmed intervention. Every event includes:

| Field | Meaning |
|---|---|
| `schema_version` | `1` |
| `daemon_instance`, `sequence`, `event_id` | Instance UUID + monotonically increasing event sequence; deduplicate by `event_id` |
| `incident_id` | Overload incident UUID, or `health-GPU-…` for health transitions |
| `timestamp_unix_ms`, `gpu_uuid` | Event time and stable GPU identity |
| `kind` | See stages below |
| `simulation` | True for dry-run or simulated backend |
| `channel_id`, `current_a` | Nullable triggering/peak channel information; absent when telemetry is missing |
| `threshold_a` | Configured threshold in amperes |
| `previous_power_cap_w` | Nullable pre-action configured cap, not board draw |
| `action` | Nullable tagged target (below) |
| `outcome` | Nullable confirmation/partial outcome (below) |
| `detail` | Human explanation; failure details also appear in outcome errors |

Stages: `overload_detected`, `action_proposed` (dry-run only), `action_requested`,
`action_applied`, `action_failed`, `protection_unavailable`, `monitoring_recovered`,
`overload_cleared`. A request/detection is **not** a success. Clock fallback counts
as applied only when both setters return success. `overload_cleared` never means
caps/clocks were restored.

Action: `{"kind":"power_limit","target_w":494.307}` or
`{"kind":"clock_throttle","graphics_mhz":180,"memory_mhz":405}`.
Outcome: `power_limit_w` (nullable confirmed readback), `graphics_locked`,
`memory_locked` (driver acknowledgements), and `errors` (array).
A failed power readback may mean a control did apply but its resulting cap is
unknown; a clock failure may still leave one clock restricted. No rollback occurs.

All action stages are logged to journald. The private history file holds the
bounded replay ring and most recent real control record per UUID. Consumers
connecting after restart receive current state and historical restriction records;
they must label replay/historical outcomes rather than implying new changes.

## Quickshell consumption recipe (no widget/configuration changes)

Use a `Quickshell.Io.Process` with command arguments
`["/usr/local/bin/pinwatch", "watch", "--json"]`. Attach a `SplitParser` to stdout;
for each `onRead` line, call `JSON.parse(data)`, validate `schema_version === 1`,
and select a GPU by `device.gpu_uuid`. Update all six channel values together.
Use `sample_sequence` to distinguish polls from heartbeats and `freshness` /
`sample_age_ms` to gray out stale values. Show `null` as unavailable, never as zero.
Read diagnostics separately from stderr. When the process exits, immediately mark
telemetry unavailable and restart it with bounded backoff, rather than leaving the
last numbers looking current. The CLI flushes every complete line without ANSI.

Notifications are independent: run `pinwatch notify` in the user session.
Quickshell's standard NotificationServer, if already configured by the user, can
receive them. No custom Quickshell API, widget, or shell modification is required.
