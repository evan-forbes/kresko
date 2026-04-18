use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use zebra_jsonl_trace::{JsonlTraceSendError, JsonlTracer, JsonlWriteEvent};

use crate::txblast::{OrchardBlastRuntimeConfig, TxblastTraceConfig};

use super::{
    state::{LaneRegistry, RegistrySnapshot, TreasuryInventory, TreasurySnapshot},
    types::{
        NoteRole, PendingTraceSummary, PendingTxCounts, PendingTxKind, RecoveredNote, RuntimePhase,
        TrackedNote,
    },
};

const TRACE_ENABLE_ENV: &str = "KRESKO_TXBLAST_TRACE_ENABLE";
const TRACE_DIR_ENV: &str = "KRESKO_TRACE_DIR";
const EVENT_SCHEMA: &str = "kresko.txblast.event.v1";
const REGISTRY_SCHEMA: &str = "kresko.txblast.registry.v1";
const NOTE_SCHEMA: &str = "kresko.txblast.note.v1";
const TRACE_DROPPED_SCHEMA: &str = "kresko.txblast.trace_dropped.v1";

const TXBLAST_EVENT_TABLE: &str = "txblast_event";
const TXBLAST_EVENT_FILE: &str = "txblast_event.jsonl";
const TXBLAST_REGISTRY_TABLE: &str = "txblast_registry";
const TXBLAST_REGISTRY_FILE: &str = "txblast_registry.jsonl";
const TXBLAST_NOTE_TABLE: &str = "txblast_note";
const TXBLAST_NOTE_FILE: &str = "txblast_note.jsonl";
const TXBLAST_TRACE_DROPPED_TABLE: &str = "txblast_trace_dropped";
const TXBLAST_TRACE_DROPPED_FILE: &str = "txblast_trace_dropped.jsonl";

#[derive(Clone, Debug)]
struct TraceRuntime {
    tracer: JsonlTracer,
    event_drops: Arc<AtomicU64>,
    registry_drops: Arc<AtomicU64>,
    note_drops: Arc<AtomicU64>,
}

#[derive(Clone, Debug)]
pub(crate) struct OrchardTxblastTracer {
    node: Arc<str>,
    key_name: Arc<str>,
    runtime: Option<TraceRuntime>,
}

#[derive(Serialize)]
struct EventRecord {
    schema: &'static str,
    ts: String,
    node: String,
    key_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    phase: &'static str,
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lane_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note_role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note_value: Option<u64>,
    pending_total: usize,
    pending_fanout: usize,
    pending_reseed: usize,
    ready_lanes: usize,
    reservoirs: usize,
    treasury_backlog: usize,
    treasury_backlog_value: u64,
    drained: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_class: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rpc_submit_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirm_delay_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirm_delay_blocks: Option<u32>,
}

#[derive(Serialize)]
struct RegistryRecord {
    schema: &'static str,
    ts: String,
    node: String,
    key_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    phase: &'static str,
    event: &'static str,
    ready_lanes: usize,
    pending_lanes: usize,
    pending_fanout: usize,
    pending_reseed: usize,
    reservoir_count: usize,
    reservoir_total_value: u64,
    lane_total_value: u64,
    treasury_backlog: usize,
    treasury_backlog_value: u64,
    treasury_reserved: usize,
    drained_notes: u64,
    submit_credit: f64,
    max_in_flight: usize,
    target_ready_lanes: usize,
    lane_low_watermark: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_pending_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_pending_blocks: Option<u32>,
    rpc_pending_unknown: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_unknown_pending_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_unknown_pending_blocks: Option<u32>,
    rpc_pending_mempool: usize,
    rpc_pending_confirmed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_mempool_pending_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_mempool_pending_blocks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_confirmed_rpc_pending_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oldest_confirmed_rpc_pending_blocks: Option<u32>,
}

#[derive(Serialize)]
struct NoteRecord {
    schema: &'static str,
    ts: String,
    node: String,
    key_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    event: &'static str,
    note_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_note_id: Option<String>,
    origin_txid: String,
    origin_action_idx: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    lane_id: Option<u64>,
    role: &'static str,
    value: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_confirmation_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_txid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct TraceDroppedRecord {
    schema: &'static str,
    ts: String,
    node: String,
    key_name: String,
    table: &'static str,
    queue_full_dropped: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct EventContext<'a> {
    pub(crate) height: Option<u32>,
    pub(crate) phase: RuntimePhase,
    pub(crate) event: &'static str,
    pub(crate) tx_kind: Option<PendingTxKind>,
    pub(crate) txid: Option<&'a str>,
    pub(crate) lane_id: Option<u64>,
    pub(crate) note_id: Option<&'a str>,
    pub(crate) note_role: Option<NoteRole>,
    pub(crate) note_value: Option<u64>,
    pub(crate) pending: PendingTxCounts,
    pub(crate) registry: RegistrySnapshot,
    pub(crate) treasury: TreasurySnapshot,
    pub(crate) reason: Option<&'a str>,
    pub(crate) error: Option<String>,
    pub(crate) error_class: Option<&'static str>,
    pub(crate) build_duration_ms: Option<u64>,
    pub(crate) rpc_submit_duration_ms: Option<u64>,
    pub(crate) confirm_delay_ms: Option<u64>,
    pub(crate) confirm_delay_blocks: Option<u32>,
}

impl OrchardTxblastTracer {
    pub(crate) fn from_config(config: &TxblastTraceConfig, key_name: &str) -> Self {
        let enabled = config.enabled || env_flag_enabled(TRACE_ENABLE_ENV);
        if !enabled {
            return Self::noop(key_name);
        }

        let trace_dir = config
            .directory
            .clone()
            .or_else(|| std::env::var_os(TRACE_DIR_ENV).map(PathBuf::from));

        let Some(trace_dir) = trace_dir.filter(|path| !path.as_os_str().is_empty()) else {
            eprintln!(
                "[shielded][warn] txblast tracing enabled but no trace directory was configured"
            );
            return Self::noop(key_name);
        };

        let tracer = JsonlTracer::spawn(trace_dir);
        Self::new(key_name, tracer)
    }

    pub(crate) fn noop(key_name: &str) -> Self {
        Self::new(key_name, JsonlTracer::noop())
    }

    fn new(key_name: &str, tracer: JsonlTracer) -> Self {
        Self {
            node: Arc::from(node_name()),
            key_name: Arc::from(key_name.to_owned()),
            runtime: tracer.is_enabled().then(|| TraceRuntime {
                tracer,
                event_drops: Arc::new(AtomicU64::new(0)),
                registry_drops: Arc::new(AtomicU64::new(0)),
                note_drops: Arc::new(AtomicU64::new(0)),
            }),
        }
    }

    pub(crate) fn trace_event(&self, event: EventContext<'_>) {
        let record = EventRecord {
            schema: EVENT_SCHEMA,
            ts: timestamp(),
            node: self.node.to_string(),
            key_name: self.key_name.to_string(),
            height: event.height,
            phase: event.phase.as_str(),
            event: event.event,
            tx_kind: event.tx_kind.map(PendingTxKind::as_str),
            txid: event.txid.map(ToOwned::to_owned),
            lane_id: event.lane_id,
            note_id: event.note_id.map(ToOwned::to_owned),
            note_role: event.note_role.map(NoteRole::as_str),
            note_value: event.note_value,
            pending_total: event.pending.total,
            pending_fanout: event.pending.expansion,
            pending_reseed: event.pending.treasury_reseed,
            ready_lanes: event.registry.ready_lanes,
            reservoirs: event.registry.reservoirs,
            treasury_backlog: event.treasury.backlog_utxos,
            treasury_backlog_value: event.treasury.backlog_value,
            drained: event.registry.drained_notes,
            reason: event.reason.map(ToOwned::to_owned),
            error: event.error,
            error_class: event.error_class,
            build_duration_ms: event.build_duration_ms,
            rpc_submit_duration_ms: event.rpc_submit_duration_ms,
            confirm_delay_ms: event.confirm_delay_ms,
            confirm_delay_blocks: event.confirm_delay_blocks,
        };

        self.emit_json(TXBLAST_EVENT_TABLE, TXBLAST_EVENT_FILE, &record);
    }

    pub(crate) fn trace_registry(
        &self,
        height: Option<u32>,
        phase: RuntimePhase,
        event: &'static str,
        registry: &LaneRegistry,
        treasury: &TreasuryInventory,
        pending: PendingTxCounts,
        pending_trace: PendingTraceSummary,
        submit_credit: f64,
        cfg: &OrchardBlastRuntimeConfig,
        reason: Option<&str>,
    ) {
        let snapshot = registry.snapshot();
        let treasury_snapshot = treasury.snapshot();
        let record = RegistryRecord {
            schema: REGISTRY_SCHEMA,
            ts: timestamp(),
            node: self.node.to_string(),
            key_name: self.key_name.to_string(),
            height,
            phase: phase.as_str(),
            event,
            ready_lanes: snapshot.ready_lanes,
            pending_lanes: pending
                .total
                .saturating_sub(pending.expansion + pending.treasury_reseed),
            pending_fanout: pending.expansion,
            pending_reseed: pending.treasury_reseed,
            reservoir_count: snapshot.reservoirs,
            reservoir_total_value: snapshot.reservoir_total_value,
            lane_total_value: snapshot.lane_total_value,
            treasury_backlog: treasury_snapshot.backlog_utxos,
            treasury_backlog_value: treasury_snapshot.backlog_value,
            treasury_reserved: treasury_snapshot.reserved_utxos,
            drained_notes: snapshot.drained_notes,
            submit_credit,
            max_in_flight: cfg.max_in_flight,
            target_ready_lanes: cfg.target_ready_lanes,
            lane_low_watermark: cfg.lane_low_watermark,
            reason: reason.map(ToOwned::to_owned),
            oldest_pending_ms: pending_trace.oldest_pending_ms,
            oldest_pending_blocks: pending_trace.oldest_pending_blocks,
            rpc_pending_unknown: pending_trace.rpc_pending_unknown,
            oldest_unknown_pending_ms: pending_trace.oldest_unknown_pending_ms,
            oldest_unknown_pending_blocks: pending_trace.oldest_unknown_pending_blocks,
            rpc_pending_mempool: pending_trace.rpc_pending_mempool,
            rpc_pending_confirmed: pending_trace.rpc_pending_confirmed,
            oldest_mempool_pending_ms: pending_trace.oldest_mempool_pending_ms,
            oldest_mempool_pending_blocks: pending_trace.oldest_mempool_pending_blocks,
            oldest_confirmed_rpc_pending_ms: pending_trace.oldest_confirmed_rpc_pending_ms,
            oldest_confirmed_rpc_pending_blocks: pending_trace.oldest_confirmed_rpc_pending_blocks,
        };

        self.emit_json(TXBLAST_REGISTRY_TABLE, TXBLAST_REGISTRY_FILE, &record);
    }

    pub(crate) fn trace_recovered_note(
        &self,
        height: Option<u32>,
        event: &'static str,
        note: &RecoveredNote,
        pending_txid: Option<&str>,
        reason: Option<&str>,
    ) {
        let record = NoteRecord {
            schema: NOTE_SCHEMA,
            ts: timestamp(),
            node: self.node.to_string(),
            key_name: self.key_name.to_string(),
            height,
            event,
            note_id: note.note_id.clone(),
            parent_note_id: note.parent_note_id.clone(),
            origin_txid: note.origin_txid.clone(),
            origin_action_idx: note.action_idx,
            lane_id: None,
            role: note.role.as_str(),
            value: note.value(),
            position: None,
            last_confirmation_height: None,
            pending_txid: pending_txid.map(ToOwned::to_owned),
            reason: reason.map(ToOwned::to_owned),
        };

        self.emit_json(TXBLAST_NOTE_TABLE, TXBLAST_NOTE_FILE, &record);
    }

    pub(crate) fn trace_tracked_note(
        &self,
        height: Option<u32>,
        event: &'static str,
        note: &TrackedNote,
        pending_txid: Option<&str>,
        reason: Option<&str>,
    ) {
        let record = NoteRecord {
            schema: NOTE_SCHEMA,
            ts: timestamp(),
            node: self.node.to_string(),
            key_name: self.key_name.to_string(),
            height,
            event,
            note_id: note.note_id.clone(),
            parent_note_id: note.parent_note_id.clone(),
            origin_txid: note.origin_txid.clone(),
            origin_action_idx: note.origin_action_idx,
            lane_id: note.lane_id,
            role: note.role.as_str(),
            value: note.value(),
            position: Some(u64::from(note.position)),
            last_confirmation_height: Some(note.last_confirmation_height),
            pending_txid: pending_txid.map(ToOwned::to_owned),
            reason: reason.map(ToOwned::to_owned),
        };

        self.emit_json(TXBLAST_NOTE_TABLE, TXBLAST_NOTE_FILE, &record);
    }

    fn emit_json<T: Serialize>(&self, table: &'static str, file_name: &'static str, record: &T) {
        let Some(runtime) = &self.runtime else {
            return;
        };

        let Ok(line) = serde_json::to_vec(record) else {
            return;
        };

        let event = JsonlWriteEvent {
            table,
            file_name,
            line,
        };

        match runtime.tracer.try_send(event) {
            Ok(())
            | Err(JsonlTraceSendError::Disabled(_))
            | Err(JsonlTraceSendError::Closed(_)) => {
                if table != TXBLAST_TRACE_DROPPED_TABLE {
                    self.try_emit_drop_records();
                }
            }
            Err(JsonlTraceSendError::Full(_)) => {
                self.drop_counter(table).fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn try_emit_drop_records(&self) {
        let Some(runtime) = &self.runtime else {
            return;
        };

        self.try_emit_drop_record(runtime, TXBLAST_EVENT_TABLE, &runtime.event_drops);
        self.try_emit_drop_record(runtime, TXBLAST_REGISTRY_TABLE, &runtime.registry_drops);
        self.try_emit_drop_record(runtime, TXBLAST_NOTE_TABLE, &runtime.note_drops);
    }

    fn try_emit_drop_record(
        &self,
        runtime: &TraceRuntime,
        table: &'static str,
        counter: &AtomicU64,
    ) {
        let dropped = counter.swap(0, Ordering::Relaxed);
        if dropped == 0 {
            return;
        }

        let record = TraceDroppedRecord {
            schema: TRACE_DROPPED_SCHEMA,
            ts: timestamp(),
            node: self.node.to_string(),
            key_name: self.key_name.to_string(),
            table,
            queue_full_dropped: dropped,
        };
        let Ok(line) = serde_json::to_vec(&record) else {
            counter.fetch_add(dropped, Ordering::Relaxed);
            return;
        };

        let event = JsonlWriteEvent {
            table: TXBLAST_TRACE_DROPPED_TABLE,
            file_name: TXBLAST_TRACE_DROPPED_FILE,
            line,
        };

        match runtime.tracer.try_send(event) {
            Ok(())
            | Err(JsonlTraceSendError::Disabled(_))
            | Err(JsonlTraceSendError::Closed(_)) => {}
            Err(JsonlTraceSendError::Full(_)) => {
                counter.fetch_add(dropped, Ordering::Relaxed);
            }
        }
    }

    fn drop_counter(&self, table: &'static str) -> &AtomicU64 {
        let runtime = self.runtime.as_ref().expect("trace runtime should exist");
        match table {
            TXBLAST_EVENT_TABLE => &runtime.event_drops,
            TXBLAST_REGISTRY_TABLE => &runtime.registry_drops,
            TXBLAST_NOTE_TABLE => &runtime.note_drops,
            _ => &runtime.event_drops,
        }
    }
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn node_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}
