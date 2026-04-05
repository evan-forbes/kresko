use incrementalmerkletree::Position;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NoteRole {
    Lane,
    Reservoir,
}

impl NoteRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Lane => "lane",
            Self::Reservoir => "reservoir",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PlannedOutput {
    pub(crate) role: NoteRole,
    pub(crate) value: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimePhase {
    BootstrapScan,
    BootstrapShield,
    SteadyState,
}

impl RuntimePhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BootstrapScan => "bootstrap_scan",
            Self::BootstrapShield => "bootstrap_shield",
            Self::SteadyState => "steady_state",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TrackedNote {
    pub(crate) note_id: String,
    pub(crate) parent_note_id: Option<String>,
    pub(crate) origin_txid: String,
    pub(crate) origin_action_idx: usize,
    pub(crate) lane_id: Option<u64>,
    pub(crate) note: orchard::Note,
    pub(crate) position: Position,
    pub(crate) role: NoteRole,
    pub(crate) last_confirmation_height: u32,
}

impl TrackedNote {
    pub(crate) fn value(&self) -> u64 {
        self.note.value().inner()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RecoveredNote {
    pub(crate) note_id: String,
    pub(crate) parent_note_id: Option<String>,
    pub(crate) origin_txid: String,
    pub(crate) action_idx: usize,
    pub(crate) note: orchard::Note,
    pub(crate) role: NoteRole,
}

impl RecoveredNote {
    pub(crate) fn pending(action_idx: usize, note: orchard::Note, role: NoteRole) -> Self {
        Self {
            note_id: String::new(),
            parent_note_id: None,
            origin_txid: String::new(),
            action_idx,
            note,
            role,
        }
    }

    pub(crate) fn with_origin(mut self, txid: &str, parent_note_id: Option<String>) -> Self {
        self.origin_txid = txid.to_owned();
        self.note_id = format!("{txid}:{}", self.action_idx);
        self.parent_note_id = parent_note_id;
        self
    }

    pub(crate) fn value(&self) -> u64 {
        self.note.value().inner()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingTxKind {
    WarmupShielding,
    LaneAdvance,
    ReservoirExpand,
    TreasuryReseed,
}

impl PendingTxKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::WarmupShielding => "warmup_shielding",
            Self::LaneAdvance => "lane_advance",
            Self::ReservoirExpand => "reservoir_expand",
            Self::TreasuryReseed => "treasury_reseed",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PendingTx {
    pub(crate) recovered_notes: Vec<RecoveredNote>,
    pub(crate) num_actions: usize,
    pub(crate) kind: PendingTxKind,
    pub(crate) spent_transparent_outpoint: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PendingTxCounts {
    pub(crate) total: usize,
    pub(crate) expansion: usize,
    pub(crate) treasury_reseed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TreasuryUtxo {
    pub(crate) outpoint_id: String,
    pub(crate) txid: String,
    pub(crate) output_index: u32,
    pub(crate) script: String,
    pub(crate) satoshis: u64,
    pub(crate) height: u32,
}

#[derive(Debug)]
pub(crate) enum ScheduledWork {
    LaneAdvance(TrackedNote),
    ReservoirExpand(TrackedNote),
    TreasuryReseed(TreasuryUtxo),
}
