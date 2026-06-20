//! trail-core: a coverage scheduler with memory.
//!
//! Hands an agent the next folder to work on for a named task, leases it so
//! parallel agents never collide, records the visit, and biases future ordering
//! from accumulated per-task history. See the crate README and `SKILL.md`.

pub mod config;
pub mod error;
pub mod model;
pub mod scheduler;
pub mod store;
pub mod walk;

pub use config::Config;
pub use error::{Error, Result};
pub use model::{
    CompleteResult, FolderStat, GcResult, InitResult, ListRow, NextResult, ResetResult,
    StaticSignal, StatusReport, Strategy, SweepInfo, WorkStatus,
};
pub use store::Store;
pub use walk::{normalize_rel, scan, WalkOutcome};
