//! Shared types: enums, walk output, and the JSON-serializable command results.
//!
//! Every result type here is what the CLI prints to stdout. The shapes are a
//! stable contract that the thin language wrappers parse, so think twice before
//! renaming a field.

use serde::{Deserialize, Serialize};

/// Ordering policy applied when a sweep is opened. All three drain a one-pass
/// coverage board; they only differ in the order folders are handed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Pure least-recently-visited-first (equal static weight).
    RoundRobin,
    /// Blend of recency and static weight (see [`crate::scheduler`]).
    Weighted,
    /// Seeded, reproducible pseudo-random order.
    Random,
}

impl Strategy {
    /// Stable string used in the database and CLI.
    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::RoundRobin => "round-robin",
            Strategy::Weighted => "weighted",
            Strategy::Random => "random",
        }
    }

    /// Parse the database/CLI string form. Unknown values fall back to weighted.
    pub fn from_db(s: &str) -> Strategy {
        match s {
            "round-robin" => Strategy::RoundRobin,
            "random" => Strategy::Random,
            _ => Strategy::Weighted,
        }
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which static signal becomes a folder's weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaticSignal {
    FileCount,
    SizeBytes,
    Churn,
}

/// Per-folder static signals captured during a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderStat {
    /// Repo-relative folder path (the root folder is ".").
    pub path: String,
    /// Files directly contained in this folder (not recursive).
    pub file_count: i64,
    /// Sum of the direct files' sizes in bytes.
    pub size_bytes: i64,
    /// Optional git churn score (0 unless the `churn` feature computed it).
    pub churn: i64,
}

impl FolderStat {
    /// The raw signal value for the configured [`StaticSignal`].
    pub fn signal(&self, signal: StaticSignal) -> i64 {
        match signal {
            StaticSignal::FileCount => self.file_count,
            StaticSignal::SizeBytes => self.size_bytes,
            StaticSignal::Churn => self.churn,
        }
    }
}

/// Terminal-or-not state of a folder within a single sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    Pending,
    Leased,
    Done,
    Skipped,
}

impl WorkStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkStatus::Pending => "pending",
            WorkStatus::Leased => "leased",
            WorkStatus::Done => "done",
            WorkStatus::Skipped => "skipped",
        }
    }

    pub fn from_db(s: &str) -> Option<WorkStatus> {
        match s {
            "pending" => Some(WorkStatus::Pending),
            "leased" => Some(WorkStatus::Leased),
            "done" => Some(WorkStatus::Done),
            "skipped" => Some(WorkStatus::Skipped),
            _ => None,
        }
    }
}

// --- Command results (printed as JSON to stdout) ---------------------------

/// Result of `trail init`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitResult {
    /// Folders registered in the snapshot (meeting `min_files`).
    pub folders: usize,
    /// Directories encountered that did not qualify (below `min_files`).
    pub excluded: usize,
    /// True if a `.trail.toml.example` was written this run.
    pub wrote_example_config: bool,
}

/// Result of `trail next`. The `status` tag drives the process exit code:
/// ok -> 0, none-available -> 4, sweep-complete -> 3.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum NextResult {
    /// A folder was leased to the caller.
    Ok {
        task: String,
        sweep: i64,
        path: String,
        score: f64,
        /// Unix seconds at which the lease expires if not completed.
        lease_expires_at: i64,
        /// Pending folders still waiting after this claim.
        remaining: i64,
    },
    /// No pending folders right now, but other agents still hold leases.
    /// The caller should briefly wait and retry.
    NoneAvailable {
        task: String,
        sweep: i64,
        leased_outstanding: i64,
    },
    /// Every folder in the sweep is done/skipped. The outer loop owns starting
    /// the next sweep (`trail sweep new`, or pass `--auto-sweep`).
    SweepComplete {
        task: String,
        sweep: i64,
        /// Folders covered (done) in the completed sweep.
        covered: i64,
        /// Total folders in the sweep. `total == 0` means nothing was
        /// registered (likely a missing `trail init`), not real completion.
        total: i64,
        /// Present only when something looks off (e.g. an empty sweep).
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

/// Result of `trail done` / `trail skip`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteResult {
    pub status: String, // "done" | "skipped"
    pub task: String,
    pub sweep: i64,
    pub path: String,
    /// Pending + leased folders still outstanding in the sweep.
    pub remaining: i64,
    /// True if this completion finished the sweep.
    pub sweep_complete: bool,
}

/// Result of `trail status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub task: String,
    pub sweep: i64,
    pub sweep_status: String, // "active" | "complete" | "none"
    pub total: i64,
    pub done: i64,
    pub leased: i64,
    pub pending: i64,
    pub skipped: i64,
    pub percent: f64,
}

/// One row of `trail list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRow {
    pub path: String,
    pub status: String,
    pub score: f64,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub visited_at: Option<i64>,
}

/// Result of `trail sweep new` / `trail sweep show`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepInfo {
    pub task: String,
    pub sweep: i64,
    pub sweep_status: String, // "active" | "complete" | "none"
    pub total: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
}

/// Result of `trail reset`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetResult {
    pub task: String,
    pub cleared_sweeps: i64,
    pub cleared_history: bool,
}

/// Result of `trail gc`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcResult {
    pub reclaimed_leases: i64,
}
