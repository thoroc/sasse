//! The queue's per-repository configuration.
//!
//! Read from `sasse.toml` in the repository, and specifically from the base
//! branch tip rather than from the candidate under test. See
//! `docs/adr/gate-provenance.md`: the worker runs the gate on a developer's own
//! machine as their own user, so taking the command from a commit that has not
//! yet passed the queue would make enqueuing a branch equivalent to granting it
//! code execution.

use eyre::{Result, WrapErr, eyre};
use serde::Deserialize;

/// Where the configuration lives, relative to the repository root.
pub const CONFIG_PATH: &str = "sasse.toml";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The command run against a candidate to decide whether it may land.
    pub gate: String,

    /// Most entries in one candidate.
    ///
    /// A larger batch lands more per gate run when everything is green, and
    /// costs more bisection rounds when it is not.
    #[serde(default = "default_max_batch")]
    pub max_batch: usize,

    /// How many times an entry may be the culprit of a failed candidate before
    /// it is evicted.
    ///
    /// Without a budget a flaky gate destroys throughput; with an unlimited one
    /// a genuinely broken branch blocks the queue.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

fn default_max_batch() -> usize {
    8
}

fn default_max_attempts() -> u32 {
    3
}

impl Config {
    /// Parse the configuration, rejecting anything unusable.
    ///
    /// There is deliberately no default gate. A worker that fell back to a
    /// built-in command when the config was missing or malformed would be a
    /// second route to running something nobody chose.
    pub fn parse(toml_source: &str) -> Result<Self> {
        let config: Self =
            toml::from_str(toml_source).wrap_err_with(|| format!("parsing {CONFIG_PATH}"))?;

        if config.gate.trim().is_empty() {
            return Err(eyre!("{CONFIG_PATH} declares an empty gate"));
        }
        if config.max_batch == 0 {
            return Err(eyre!(
                "{CONFIG_PATH} sets max_batch to 0, which would queue forever without ever gating"
            ));
        }
        if config.max_attempts == 0 {
            return Err(eyre!(
                "{CONFIG_PATH} sets max_attempts to 0, which would evict every entry on its first failure"
            ));
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gate_is_the_only_required_setting() {
        let config = Config::parse(r#"gate = "cargo test""#).unwrap();
        assert_eq!(config.gate, "cargo test");
        assert_eq!(config.max_batch, default_max_batch());
        assert_eq!(config.max_attempts, default_max_attempts());
    }

    #[test]
    fn the_defaults_can_be_overridden() {
        let config = Config::parse(
            r#"
            gate = "mise run gate"
            max_batch = 3
            max_attempts = 1
            "#,
        )
        .unwrap();
        assert_eq!(config.max_batch, 3);
        assert_eq!(config.max_attempts, 1);
    }

    #[test]
    fn a_config_without_a_gate_is_rejected() {
        assert!(Config::parse("max_batch = 4").is_err());
    }

    #[test]
    fn an_empty_gate_is_rejected() {
        assert!(Config::parse(r#"gate = "   ""#).is_err());
    }

    /// A batch of zero would select nothing and gate nothing, so the queue
    /// would sit still while looking healthy.
    #[test]
    fn a_zero_batch_is_rejected() {
        assert!(Config::parse("gate = \"x\"\nmax_batch = 0").is_err());
    }

    #[test]
    fn a_zero_retry_budget_is_rejected() {
        assert!(Config::parse("gate = \"x\"\nmax_attempts = 0").is_err());
    }

    /// A typo in a setting name should be an error rather than a silently
    /// ignored line that leaves the operator believing it took effect.
    #[test]
    fn an_unknown_setting_is_rejected() {
        assert!(Config::parse("gate = \"x\"\nmax_batches = 4").is_err());
    }

    #[test]
    fn malformed_toml_is_rejected() {
        assert!(Config::parse("gate = ").is_err());
    }
}
