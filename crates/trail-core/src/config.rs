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
        Ok(())
    }

    /// True when the include list is the universal catch-all (no real
    /// whitelist), so the walker should not narrow to a whitelist.
    pub fn include_is_catch_all(&self) -> bool {
        self.scan.include.is_empty() || self.scan.include == ["**/*"] || self.scan.include == ["**"]
    }
}

/// The contents written by `trail init` when no example config exists.
pub const EXAMPLE_CONFIG: &str = include_str!("../../../.trail.toml.example");

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
}
