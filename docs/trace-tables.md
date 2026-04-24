# Trace Tables Reference

This document is the durable reference for the structured trace tables Kresko
either emits itself or commonly collects from Zebra nodes.

It answers four questions for each table:

1. What the trace is for.
2. Which fields it contains.
3. Where the interesting fields come from.
4. Roughly where the trace is emitted in code.

## How Kresko Sees Trace Tables

Kresko currently has two trace families:

- Zebra JSONL traces, written by the Zebra process into a trace directory
  discovered from `ZEBRA_*TRACE*` and `ZEBRA_*TRACING*` env vars.
- Kresko txblast JSONL traces, written by `src/txblast/orchard/tracing.rs`
  into `KRESKO_TRACE_DIR`.

`kresko download traces` will collect every file it finds in discovered trace
directories. `kresko download traces --tables ...` currently has explicit names
for:

- `peer_message`
- `trace_dropped`
- `txblast_event`
- `txblast_registry`
- `txblast_note`
- `txblast_trace_dropped`
- `fork_event`
- `fork_snapshot`

The other Zebra tables below are still worth documenting because Kresko's
"download all discovered trace files" path will pick them up.

## Kresko Txblast Tables

### `txblast_event`

Purpose: high-frequency event stream for shielded txblast lifecycle changes,
build/submit progress, and recoveries.

Schema source: `src/txblast/orchard/tracing.rs`

Fields:

- `schema`
- `ts`
- `node`
- `key_name`
- `height`
- `phase`
- `event`
- `tx_kind`
- `txid`
- `lane_id`
- `note_id`
- `note_role`
- `note_value`
- `pending_total`
- `pending_fanout`
- `pending_reseed`
- `ready_lanes`
- `reservoirs`
- `treasury_backlog`
- `treasury_backlog_value`
- `drained`
- `reason`
- `error`
- `error_class`
- `build_duration_ms`
- `rpc_submit_duration_ms`
- `confirm_delay_ms`
- `confirm_delay_blocks`

Noteworthy field origins:

- `phase` is the Orchard runtime phase from `RuntimePhase` in
  `src/txblast/orchard/types.rs`.
- `tx_kind` is the pending transaction kind from `PendingTxKind` in
  `src/txblast/orchard/types.rs`.
- `pending_*` comes from `PendingTxCounts`.
- `ready_lanes`, `reservoirs`, and `drained` come from `LaneRegistry::snapshot`
  in `src/txblast/orchard/state.rs`.
- `treasury_*` comes from `TreasuryInventory::snapshot` in
  `src/txblast/orchard/state.rs`.
- `error_class` is a normalized bucket from `classify_error()` in
  `src/txblast/shielded.rs`.

Called from:

- `src/txblast/shielded.rs` during bootstrap shielding, steady-state build,
  submit, confirm, retry, and error paths.
- `src/txblast/orchard/scanner.rs` when the scanner observes recoveries or
  reorg-related rebuild behavior.

### `txblast_registry`

Purpose: lower-frequency snapshots of Orchard lane inventory, treasury backlog,
and pending transaction pressure. This is the best table for "what shape was the
 runtime in at this moment?"

Schema source: `src/txblast/orchard/tracing.rs`

Fields:

- `schema`
- `ts`
- `node`
- `key_name`
- `height`
- `phase`
- `event`
- `ready_lanes`
- `pending_lanes`
- `pending_fanout`
- `pending_reseed`
- `reservoir_count`
- `reservoir_total_value`
- `lane_total_value`
- `treasury_backlog`
- `treasury_backlog_value`
- `treasury_reserved`
- `drained_notes`
- `submit_credit`
- `max_in_flight`
- `target_ready_lanes`
- `lane_low_watermark`
- `reason`
- `oldest_pending_ms`
- `oldest_pending_blocks`
- `rpc_pending_unknown`
- `oldest_unknown_pending_ms`
- `oldest_unknown_pending_blocks`
- `rpc_pending_mempool`
- `rpc_pending_confirmed`
- `oldest_mempool_pending_ms`
- `oldest_mempool_pending_blocks`
- `oldest_confirmed_rpc_pending_ms`
- `oldest_confirmed_rpc_pending_blocks`

Noteworthy field origins:

- `ready_lanes`, `reservoir_*`, `lane_total_value`, and `drained_notes` come
  from `LaneRegistry::snapshot()`.
- `treasury_*` comes from `TreasuryInventory::snapshot()`.
- `pending_*` and `oldest_*` come from `pending_counts()` and
  `pending_trace_summary()` in the Orchard txblast runtime.
- `submit_credit`, `max_in_flight`, `target_ready_lanes`, and
  `lane_low_watermark` come from `OrchardBlastRuntimeConfig`.

Called from:

- `src/txblast/shielded.rs` at phase transitions, planner checkpoints, and
  after major runtime state updates.
- `src/txblast/orchard/scanner.rs` after each scanned block.

### `txblast_note`

Purpose: note-level lifecycle stream. This is the table to use when you need to
follow a specific Orchard note from discovery through activation, spend,
requeue, or recovery.

Schema source: `src/txblast/orchard/tracing.rs`

Fields:

- `schema`
- `ts`
- `node`
- `key_name`
- `height`
- `event`
- `note_id`
- `parent_note_id`
- `origin_txid`
- `origin_action_idx`
- `lane_id`
- `role`
- `value`
- `position`
- `last_confirmation_height`
- `pending_txid`
- `reason`

Noteworthy field origins:

- Records are emitted in two shapes:
  one from `RecoveredNote` and one from `TrackedNote`.
- `RecoveredNote` records do not yet have a note tree position or confirmation
  height, so `position` and `last_confirmation_height` are empty there.
- `TrackedNote` records come after activation into the registry, so they can
  carry `lane_id`, note tree `position`, and `last_confirmation_height`.
- `role` is `lane` or `reservoir` from `NoteRole`.

Called from:

- `src/txblast/shielded.rs` when notes are consumed, requeued, promoted, or
  recovered from pending transactions.
- `src/txblast/orchard/scanner.rs` when scanning blocks activates or recovers
  notes.

### `txblast_trace_dropped`

Purpose: reports trace records Kresko wanted to write but could not because the
JSONL writer channel was full.

Schema source: `src/txblast/orchard/tracing.rs`

Fields:

- `schema`
- `ts`
- `node`
- `key_name`
- `table`
- `queue_full_dropped`

Noteworthy field origins:

- `table` names the txblast table that dropped records:
  `txblast_event`, `txblast_registry`, or `txblast_note`.
- Drop counters are maintained per table in `TraceRuntime`.

Called from:

- Emitted internally by `OrchardTxblastTracer::try_emit_drop_records()` in
  `src/txblast/orchard/tracing.rs` after successful sends on the normal tables.

## Zebra P2P and Network Tables

### `peer_message`

Purpose: one record per traced P2P message send or receive, with lightweight
payload summaries and correlation IDs.

Schema source: Zebra repo `zebra-network/src/p2p_tracing.rs`

Fields:

- `ts`
- `node_id`
- `dir`
- `msg`
- `peer`
- `conn`
- `mid`
- `summary`

`summary` subfields:

- `count`
- `hashes`
- `height`
- `nonce`
- `body_bytes`

Noteworthy field origins:

- `dir` is `"send"` or `"recv"`.
- `mid` is a synthetic correlation key. Depending on message type it is derived
  from nonce, content hash, first hash in a list, or connection-local sequence.
- `summary` is intentionally compact. For list-like messages it captures counts
  and up to five hashes. For `block` and `tx` it includes body size and the main
  content hash, not the full payload.

Called from:

- `zebra-network/src/peer/connection.rs`
- `zebra-network/src/peer/handshake.rs`
- tracer setup in `zebra-network/src/peer_set/initialize.rs`

### `trace_dropped`

Purpose: backpressure accounting for `peer_message`. Tells you when the P2P
message trace is incomplete because records were dropped.

Schema source: Zebra repo `zebra-network/src/p2p_tracing.rs`

Fields:

- `ts`
- `node_id`
- `table`
- `queue_full_dropped`
- `sampled_dropped`

Noteworthy field origins:

- `table` is currently the table being protected, which is `peer_message`.
- `queue_full_dropped` counts hard channel overflow.
- `sampled_dropped` counts adaptive sampling drops used when the channel is
  under pressure.

Called from:

- Emitted internally by the P2P tracer in
  `zebra-network/src/p2p_tracing.rs`.

### `peer_session`

Purpose: one summary record per connection lifetime, aggregating message and
byte counters so you do not have to rescan `peer_message` to answer
"who served what over this connection?"

Schema source: Zebra repo `zebra-network/src/peer_session.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `event`
- `peer`
- `conn`
- `direction`
- `duration_s`
- `blocks_served`
- `blocks_served_bytes`
- `blocks_received`
- `blocks_received_bytes`
- `txs_served`
- `txs_served_bytes`
- `txs_received`
- `txs_received_bytes`
- `inv_sent`
- `inv_received`
- `getdata_sent`
- `getdata_received`
- `notfound_sent`
- `notfound_received`
- `close_reason`

Noteworthy field origins:

- Counters are accumulated in `SessionCounters` over the lifetime of one
  connection.
- `direction` is the session direction, not per-message direction.
- `close_reason` comes from the connection shutdown path.

Called from:

- Created in `zebra-network/src/peer/connection.rs`
- emitted when the connection closes from the same module

### `peer_lifecycle`

Purpose: coarse lifecycle events for peer dialing, inbound accepts, handshakes,
and disconnect/error paths.

Schema source: Zebra repo `zebra-network/src/peer_lifecycle.rs`

Fields:

- `ts`
- `node_id`
- `event`
- `direction`
- `peer`
- `reason`
- `peer_version`

Noteworthy field origins:

- `event` includes values such as `dial_attempt`, `dial_ok`, `dial_failed`,
  `inbound_accept`, `inbound_failed`, `handshake_ok`, `handshake_failed`, and
  `disconnect`.
- `reason` is only present for failures or disconnects.
- `peer_version` is only present for successful handshakes.

Called from:

- `zebra-network/src/peer_set/initialize.rs`
- `zebra-network/src/peer/handshake.rs`
- `zebra-network/src/peer_set/set.rs`

### `node_heartbeat`

Purpose: periodic node-wide rollup of message traffic and peer count over a
fixed interval.

Schema source: Zebra repo `zebra-network/src/heartbeat.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `event`
- `interval_s`
- `connected_peers`
- `blocks_served`
- `blocks_served_bytes`
- `blocks_received`
- `blocks_received_bytes`
- `txs_served`
- `txs_served_bytes`
- `txs_received`
- `txs_received_bytes`
- `inv_sent`
- `inv_received`
- `getdata_sent`
- `getdata_received`
- `notfound_sent`
- `notfound_received`
- `tip_height`

Noteworthy field origins:

- All counters except `connected_peers` are interval counters swapped back to
  zero on each heartbeat tick.
- `tip_height` is read from the chain tip handle, not inferred from message
  traffic.
- Default interval is 30 seconds.

Called from:

- initialized in `zebra-network/src/peer_set/initialize.rs`
- updated by connection tasks throughout `zebra-network`

### `send_timing`

Purpose: micro-timing around the send pipeline. Useful for separating message
encoding cost from sink flush and socket backpressure.

Schema source: Zebra repo `zebra-network/src/send_timing.rs`

Fields:

- `ts`
- `node_id`
- `phase`
- `command`
- `peer`
- `conn`
- `elapsed_us`
- `body_bytes`

Noteworthy field origins:

- `phase` is one of `encode`, `sink_send`, or `send_message`.
- `body_bytes` is only set for `encode`.
- `conn` can be `0` when the timing point does not yet have a connection ID.

Called from:

- initialized in `zebra-network/src/peer_set/initialize.rs`
- recorded from send-path instrumentation in `zebra-network`

## Zebra State Tables

### `fork_event`

Purpose: event stream for non-finalized chain/fork state transitions.

Schema source: Zebra repo `zebra-state/src/service/non_finalized_state/fork_tracing.rs`

Fields common to all `fork_event` records:

- `schema`
- `ts`
- `node_id`
- `network`
- `event`
- `trigger`
- `chain_count`

Event-specific shapes:

- `fork_created`
  fields:
  `best_tip_hash`, `best_tip_height`, `tip_hash`, `tip_height`, `root_hash`,
  `root_height`, `fork_height`, `fork_length`, `block_count`, `chain_work`,
  `is_best`
- `fork_pruned`
  fields:
  `reason`, `best_tip_hash`, `best_tip_height`, `tip_hash`, `tip_height`,
  `root_hash`, `root_height`, `fork_height`, `fork_length`,
  `orphaned_block_count`, `chain_work`
- `best_chain_switched`
  fields:
  `previous_best_tip_hash`, `previous_best_tip_height`, `new_best_tip_hash`,
  `new_best_tip_height`
- manual events from operator-triggered state changes
  fields:
  `block_hash`, `best_tip_hash`, `best_tip_height`

Noteworthy field origins:

- `trigger` is derived from `ForkTraceCause` and identifies why the state
  transition happened:
  `commit_block`, `commit_new_chain`, `finalize`, `invalidate_block`, or
  `reconsider_block`.
- `chain_work` is Zebra's `PartialCumulativeWork`, serialized as a string.
- `fork_height` and `fork_length` come from each chain's recent divergence
  metadata.

Called from:

- `zebra-state/src/service/non_finalized_state.rs` after state changes via
  `trace_state_change()`

### `fork_snapshot`

Purpose: full snapshot of the current non-finalized fork set after each traced
state transition.

Schema source: Zebra repo `zebra-state/src/service/non_finalized_state/fork_tracing.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `network`
- `event`
- `trigger`
- `chain_count`
- `best_tip_hash`
- `best_tip_height`
- `chains`

`chains` entry fields:

- `tip_hash`
- `tip_height`
- `root_hash`
- `root_height`
- `fork_height`
- `fork_length`
- `block_count`
- `chain_work`
- `is_best`

Noteworthy field origins:

- The snapshot is built from `ForkTraceSnapshot::from_state()`, which walks the
  full `NonFinalizedState`.
- This is the best table for reconstructing concurrent non-finalized branch
  layout after each mutation.

Called from:

- emitted by `trace_snapshot()` inside the same fork tracer used for
  `fork_event`

## Zebra Service and RPC Tables

### `block_verify_event`

Purpose: timing and outcome of a single block verification attempt.

Schema source: Zebra repo `zebrad/src/components/block_verify_tracing.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `event`
- `source`
- `height`
- `hash`
- `download_ms`
- `verify_ms`
- `total_ms`
- `result`
- `error_class`

Noteworthy field origins:

- `source` is `sync` or `gossip`.
- `result` is `success` or `failure`.
- `total_ms` is `download_ms + verify_ms`.
- `error_class` is only present on failures.

Called from:

- `zebrad/src/components/inbound/downloads.rs`
- `zebrad/src/components/sync/downloads.rs`

### `serving_event`

Purpose: application-level latency for serving block or transaction requests.
This measures request handling time up to "response ready", not TCP send time.

Schema source: Zebra repo `zebrad/src/components/inbound/serving_tracing.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `event`
- `requested`
- `served`
- `not_found`
- `total_bytes`
- `latency_ms`

Noteworthy field origins:

- `event` is `served_blocks` or `served_txs`.
- `requested`, `served`, and `not_found` are counts for one inbound request.
- Use `send_timing` when you care about network flush time after this point.

Called from:

- `zebrad/src/components/inbound.rs`

### `mempool_tx_lifecycle`

Purpose: transaction-level lifecycle inside the mempool.

Schema source: Zebra repo `zebrad/src/components/mempool/tracing_jsonl.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `component`
- `txid`
- `event`
- `source`
- `reason_class`
- `reason_detail`
- `transaction_bytes`
- `tip_height`
- `mempool_transactions`
- `mempool_bytes`

Noteworthy field origins:

- `component` is currently `mempool`.
- `reason_class` is a normalized short category; `reason_detail` carries the
  fuller string when Zebra has one.
- `mempool_transactions` and `mempool_bytes` capture the mempool size at the
  same moment as the event.

Called from:

- `zebrad` mempool instrumentation in
  `zebrad/src/components/mempool/tracing_jsonl.rs`

### `chain_churn`

Purpose: chain-tip changes that force mempool churn or retry behavior.

Schema source: Zebra repo `zebrad/src/components/mempool/tracing_jsonl.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `component`
- `event`
- `old_tip_hash`
- `old_tip_height`
- `new_tip_hash`
- `new_tip_height`
- `reorg_depth`
- `mempool_transactions`
- `mempool_bytes`
- `retry_transactions`
- `note`

Noteworthy field origins:

- `reorg_depth` is only present when the tip action is a reset deep enough to
  imply a reorg.
- `retry_transactions` is the number of mempool transactions Zebra is trying to
  re-evaluate against the new tip.

Called from:

- `zebrad` mempool instrumentation in
  `zebrad/src/components/mempool/tracing_jsonl.rs`

### `template_event`

Purpose: one record per built `getblocktemplate`, including mempool selection
context and long-poll timing.

Schema source: Zebra repo `zebra-rpc/src/methods/template_tracing.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `network`
- `event`
- `tip_height`
- `tip_hash`
- `template_prev_hash`
- `difficulty`
- `target`
- `work`
- `client_long_poll_id`
- `server_long_poll_id`
- `submit_old`
- `mempool_count`
- `mempool_bytes`
- `selected_count`
- `selected_bytes`
- `dependent_count`
- `selected_dependent_count`
- `conventional_fee_count`
- `low_fee_count`
- `long_poll_wait_ms`
- `selection_ms`
- `state_fetch_ms`
- `mempool_fetch_ms`
- `loop_iterations`

Noteworthy field origins:

- `event` is currently `template_built`.
- `target` and `work` are derived from compact difficulty.
- `dependent_count` counts mempool transactions with dependencies.
- `selected_dependent_count` is the subset that actually made it into the
  template.
- `conventional_fee_count` and `low_fee_count` are counts over the mempool view
  used to build this template.

Called from:

- `zebra-rpc/src/methods.rs` via `TemplateTracer::trace_template()`

### `template_diff`

Purpose: change summary between consecutive block templates.

Schema source: Zebra repo `zebra-rpc/src/methods/template_tracing.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `network`
- `event`
- `previous_tip_height`
- `previous_tip_hash`
- `new_tip_height`
- `new_tip_hash`
- `old_long_poll_id`
- `new_long_poll_id`
- `reason_class`
- `old_tx_count`
- `new_tx_count`
- `old_tx_bytes`
- `new_tx_bytes`
- `added_count`
- `removed_count`

Noteworthy field origins:

- `event` is `template_changed`.
- `reason_class` is derived by comparing old and new tip hash plus transaction
  set membership and is one of `both`, `tip_changed`, `mempool_changed`, or
  `unknown`.

Called from:

- emitted internally by `TemplateTracer::trace_diff()` when
  `trace_template()` observes a changed template

### `template_tx_decision`

Purpose: per-transaction inclusion decision for a sampled prefix of the mempool
considered during template construction.

Schema source: Zebra repo `zebra-rpc/src/methods/template_tracing.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `network`
- `tip_height`
- `tip_hash`
- `long_poll_id`
- `txid`
- `decision`
- `reason_class`
- `transaction_bytes`
- `fee_weight_ratio`
- `pays_conventional_fee`
- `has_dependencies`

Noteworthy field origins:

- Records are capped at `MAX_DECISION_RECORDS_PER_TEMPLATE`, currently `256`.
- `decision` is `included` or `excluded`.
- `reason_class` is currently one of `included`, `dependency_missing`,
  `weighted_out_or_limited`, or `low_fee_not_selected`.

Called from:

- emitted internally by `TemplateTracer::trace_decisions()` during
  `trace_template()`

### `long_poll_iteration`

Purpose: one record per long-poll loop iteration while waiting to build the
next template.

Schema source: Zebra repo `zebra-rpc/src/methods/template_tracing.rs`

Fields:

- `schema`
- `ts`
- `node_id`
- `network`
- `event`
- `iteration`
- `wake_reason`
- `tip_height`
- `tip_hash`
- `difficulty`
- `target`
- `work`
- `state_fetch_ms`
- `mempool_fetch_ms`
- `produced_template`
- `iteration_ms`

Noteworthy field origins:

- `event` is `long_poll_iteration`.
- `wake_reason` is one of `initial`, `mempool_timer`, `tip_changed`,
  `tip_spurious`, or `max_time`.
- `produced_template` tells you whether this loop pass ended by building a
  template rather than going around again.

Called from:

- `zebra-rpc/src/methods.rs`

## Maintenance Notes

When a new trace table is added:

1. Add it here with purpose, fields, provenance, and call sites.
2. If it should be selectable by name in `kresko download traces --tables`,
   update `TraceTable` in `src/commands/download.rs`.
3. If it should always be preserved across deploy/bootstrap, make sure the
   relevant `ZEBRA_*TRACE*` or `ZEBRA_*TRACING*` env var is still covered by the
   generic env handling in:
   `src/commands/genesis.rs`, `scripts/node_init.sh`, and
   `src/commands/download.rs`.
