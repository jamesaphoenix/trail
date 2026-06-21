//! `.trail.toml` loading. Every field has a default, so a missing or partial
//! config is always valid. The config is committed; the state DB is not.

use crate::error::{Error, Result};
use crate::model::{StaticSignal, Strategy};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Parsed `.trail.toml`. Missing sections/fields fall back to defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub scan: ScanConfig,
    pub strategy: StrategyConfig,
    pub lease: LeaseConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    /// Extra include globs layered on top of the ignore-crate defaults. The
    /// catch-all `["**/*"]` (the default) is treated as "no whitelist".
    pub include: Vec<String>,
    /// Extra exclude globs (gitignore syntax, applied as overrides).
    pub exclude: Vec<String>,
    /// Honor `.gitignore` / `.ignore` files while walking.
    pub respect_gitignore: bool,
    /// Skip folders that directly contain fewer than this many files.
    pub min_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyConfig {
    pub default: Strategy,
    pub seed: u64,
    /// Weighted blend: 1.0 = pure recency, 0.0 = pure static weight.
    pub alpha: f64,
    /// Recency decay half-life in seconds.
    pub half_life_secs: u64,
    pub static_signal: StaticSignal,
    /// Weighted strategy only: how much a folder's most recent reported outcome
    /// (`done --found N`) pulls future priority. 0.0 ignores outcomes (default,
    /// so behavior is unchanged unless you opt in); 1.0 lets outcome dominate.
    pub outcome_weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LeaseConfig {
    /// Seconds a leased folder is held before it can be reclaimed.
    pub ttl_secs: i64,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            include: vec!["**/*".to_string()],
            exclude: Vec::new(),
            respect_gitignore: true,
            min_files: 1,
        }
    }
}

impl Default for StrategyConfig {
    fn default() -> Self {
        StrategyConfig {
            default: Strategy::Weighted,
            seed: 42,
            alpha: 0.6,
            half_life_secs: 604_800, // 7 days
            static_signal: StaticSignal::FileCount,
            outcome_weight: 0.0,
        }
    }
}

impl Default for LeaseConfig {
    fn default() -> Self {
        LeaseConfig { ttl_secs: 900 } // 15 minutes
    }
}

impl Config {
    /// Load `<dir>/.trail.toml`, or return defaults if the file is absent.
    /// The result is always validated, so callers get a clear error rather
    /// than silently-broken scheduling from a bad value.
    pub fn load(dir: &Path) -> Result<Config> {
        let path = dir.join(".trail.toml");
        let cfg: Config = if !path.exists() {
            Config::default()
        } else {
            let text = std::fs::read_to_string(&path)?;
            toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject semantically invalid settings. A non-positive lease TTL is the
    /// dangerous one: it would make every freshly leased folder look already
    /// expired, so two agents could claim the same folder.
    pub fn validate(&self) -> Result<()> {
        if self.lease.ttl_secs <= 0 {
            return Err(Error::Config(format!(
                "lease.ttl_secs must be > 0 (got {})",
                self.lease.ttl_secs
            )));
        }
        if !(0.0..=1.0).contains(&self.strategy.alpha) {
            return Err(Error::Config(format!(
                "strategy.alpha must be between 0.0 and 1.0 (got {})",
                self.strategy.alpha
            )));
        }
        if self.strategy.half_life_secs < 1 {
            return Err(Error::Config(
                "strategy.half_life_secs must be >= 1".to_string(),
            ));
        }
        if !(0.0..=1.0).contains(&self.strategy.outcome_weight) {
            return Err(Error::Config(format!(
                "strategy.outcome_weight must be between 0.0 and 1.0 (got {})",
                self.strategy.outcome_weight
            )));
        }
        Ok(())
    }

    /// True when the include list is the universal catch-all (no real
    /// whitelist), so the walker should not narrow to a whitelist.
    pub fn include_is_catch_all(&self) -> bool {
        self.scan.include.is_empty() || self.scan.include == ["**/*"] || self.scan.include == ["**"]
    }
}

/// The contents written by `trail init` when no example config exists.
///
/// Inlined (not `include_str!` of the workspace-root file) so `trail-core`
/// packages/publishes cleanly: `cargo package` only bundles crate-local files,
/// and a `../../../` path would escape the crate. The committed
/// `.trail.toml.example` at the workspace root is kept identical to this and a
/// test enforces that (see `example_file_matches_embedded_default`).
pub const EXAMPLE_CONFIG: &str = r#"# trail config. Commit this file; the state DB under .trail/ is gitignored.
# Copy to `.trail.toml` and edit. `trail init` writes this example if absent.

[scan]
# Extra glob patterns layered on top of the ignore-crate defaults.
# .gitignore and sane language defaults (target, node_modules, .git, dist,
# __pycache__, .venv, etc.) are already excluded for free.
include = ["**/*"]
exclude = ["**/migrations/**"]
# Respect .gitignore / .ignore files while walking.
respect_gitignore = true
# Skip folders that directly contain fewer than this many files.
min_files = 1

[strategy]
# round-robin | weighted | random
default = "weighted"
# Seed for the `random` strategy (reproducible orderings).
seed = 42
# weighted: blend of recency (staleness) vs static weight. 1.0 = pure recency.
alpha = 0.6
# Recency decay half-life in seconds (default 7 days). A folder visited one
# half-life ago has ~half the recency priority of a never-visited folder.
half_life_secs = 604800
# Static signal used as folder weight: file_count | size_bytes | churn
static_signal = "file_count"
# weighted only: how much a folder's most recent reported outcome
# (`done --found N`) pulls future priority. 0.0 ignores outcomes (default).
outcome_weight = 0.0

[lease]
# How long a claimed folder stays leased before it is reclaimed for another
# agent (covers crashed/stalled agents). Default 15 minutes.
ttl_secs = 900
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.strategy.default, Strategy::Weighted);
        assert_eq!(c.strategy.alpha, 0.6);
        assert_eq!(c.lease.ttl_secs, 900);
        assert_eq!(c.scan.min_files, 1);
        assert!(c.scan.respect_gitignore);
        assert!(c.include_is_catch_all());
    }

    #[test]
    fn partial_config_fills_defaults() {
        let toml = r#"
            [strategy]
            default = "round-robin"
            seed = 7
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.strategy.default, Strategy::RoundRobin);
        assert_eq!(c.strategy.seed, 7);
        // Untouched fields keep defaults.
        assert_eq!(c.strategy.alpha, 0.6);
        assert_eq!(c.lease.ttl_secs, 900);
    }

    #[test]
    fn example_config_parses() {
        let c: Config = toml::from_str(EXAMPLE_CONFIG).unwrap();
        assert_eq!(c.strategy.static_signal, StaticSignal::FileCount);
        assert_eq!(c.scan.exclude, vec!["**/migrations/**".to_string()]);
    }

    #[test]
    fn defaults_validate() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_bad_values() {
        let bad_ttl = |secs: i64| {
            let mut c = Config::default();
            c.lease.ttl_secs = secs;
            c
        };
        assert!(matches!(bad_ttl(0).validate(), Err(Error::Config(_))));
        assert!(matches!(bad_ttl(-100).validate(), Err(Error::Config(_))));

        let mut a = Config::default();
        a.strategy.alpha = 2.5;
        assert!(matches!(a.validate(), Err(Error::Config(_))));
        a.strategy.alpha = -1.0;
        assert!(matches!(a.validate(), Err(Error::Config(_))));

        let mut hl = Config::default();
        hl.strategy.half_life_secs = 0;
        assert!(matches!(hl.validate(), Err(Error::Config(_))));
    }

    #[test]
    fn invalid_toml_is_a_config_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".trail.toml"), "this = = not valid").unwrap();
        let err = Config::load(dir.path()).unwrap_err();
        match err {
            Error::Config(msg) => assert!(msg.contains(".trail.toml")),
            other => panic!("expected config error, got {other:?}"),
        }
    }

    #[test]
    fn missing_config_loads_validated_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config::load(dir.path()).unwrap();
        assert_eq!(c.strategy.default, Strategy::Weighted);
    }

    #[test]
    fn example_file_matches_embedded_default() {
        // The committed workspace .trail.toml.example must stay identical to the
        // embedded EXAMPLE_CONFIG so docs and `trail init` output never drift.
        // Read at test time via the manifest dir (not include_str!, which would
        // re-introduce the publish-breaking path escape).
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.trail.toml.example");
        let on_disk = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert_eq!(
            on_disk,
            EXAMPLE_CONFIG,
            "{} drifted from EXAMPLE_CONFIG",
            path.display()
        );
    }

    #[test]
    fn include_catch_all_variants() {
        let mut c = Config::default();
        assert!(c.include_is_catch_all());
        c.scan.include = vec!["**".to_string()];
        assert!(c.include_is_catch_all());
        c.scan.include = vec![];
        assert!(c.include_is_catch_all());
        c.scan.include = vec!["src/**".to_string()];
        assert!(!c.include_is_catch_all());
    }
}
