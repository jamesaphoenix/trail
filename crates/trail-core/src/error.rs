//! Crate error type.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("walk error: {0}")]
    Walk(String),

    /// A done/skip targeted a path that is not a work item in any sweep
    /// (typo, wrong task, or excluded by config).
    #[error("{0}")]
    NotInSweep(String),

    /// `sweep new` was asked to open a sweep while one is still active.
    #[error("{0}")]
    SweepActive(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
