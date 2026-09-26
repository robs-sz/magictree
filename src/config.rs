use crate::paths::Paths;
use anyhow::{Context, Result};
use serde::Deserialize;

/// What `up` and `restart` do with the images of the compose services they
/// start. `Ask` puts the question before the build, and a run without a
/// terminal declines it — scripts, agents, CI — exactly like a bootstrap step
/// marked `ask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildMode {
    /// Build before starting. Compose validates its cache, so an image whose
    /// build context is unchanged costs a cache check, not a rebuild.
    Always,
    /// Ask once per run, naming the services that would build.
    Ask,
    /// Never build; start from the images that are already there.
    Never,
}

impl Default for BuildMode {
    fn default() -> Self {
        Self::Always
    }
}

/// What `up` does with a Compose project that is already up on an unchanged
/// configuration. `Always` (the default, and what `up` has always done)
/// reconciles it every time; `Auto` skips the start, the recreate and a
/// finished one-shot's re-run, which is the opt-in through
/// ~/.config/magictree/config.toml. `up --refresh` forces a reconcile for one
/// run either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReconcileMode {
    #[default]
    Always,
    Auto,
}

/// Optional machine-wide configuration at ~/.config/magictree/config.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Inclusive start of the port range magictree allocates from.
    pub port_range_start: u16,
    /// Inclusive end of the port range magictree allocates from.
    pub port_range_end: u16,
    /// Spacing between worktree blocks inside the range.
    pub port_stride: u16,
    /// Seconds to wait for a host process to exit after SIGTERM before SIGKILL.
    pub stop_timeout_secs: u64,
    /// Default seconds to wait for a service to become healthy.
    pub health_timeout_secs: u64,
    /// Whether `up` and `restart` build the compose services they start.
    pub build: BuildMode,
    /// What `up` does with a Compose project that is already up on an unchanged
    /// configuration.
    pub reconcile: ReconcileMode,
    /// Whether `[bootstrap] sync` may link a path from the primary checkout.
    /// `true` (the default) honours the manifest; `false` makes every checkout
    /// install its own dependencies, and unlinks anything a previous run linked.
    pub sync: bool,
    /// Whether a command may mention a release that has landed since this
    /// binary was installed.
    pub check_for_updates: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port_range_start: 20000,
            port_range_end: 32767,
            port_stride: 20,
            stop_timeout_secs: 10,
            health_timeout_secs: 60,
            build: BuildMode::default(),
            reconcile: ReconcileMode::default(),
            sync: true,
            check_for_updates: true,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.port_stride > 0,
            "config: port_stride must be greater than zero"
        );
        anyhow::ensure!(
            self.port_range_start < self.port_range_end,
            "config: port_range_start must be less than port_range_end"
        );
        Ok(())
    }

    pub fn slots(&self) -> u32 {
        let span = (self.port_range_end - self.port_range_start) as u32 + 1;
        span / self.port_stride as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_without_a_build_key_builds() {
        // The key is optional, and an existing config written before it existed
        // has to keep loading with the default.
        let config: Config = toml::from_str("port_stride = 10").expect("parse");
        assert_eq!(config.build, BuildMode::Always);
        assert_eq!(config.port_stride, 10);
    }

    #[test]
    fn a_build_mode_is_named_in_lowercase() {
        let config: Config = toml::from_str(r#"build = "never""#).expect("parse");
        assert_eq!(config.build, BuildMode::Never);
        let config: Config = toml::from_str(r#"build = "ask""#).expect("parse");
        assert_eq!(config.build, BuildMode::Ask);
        assert!(toml::from_str::<Config>(r#"build = "sometimes""#).is_err());
    }

    #[test]
    fn a_config_without_a_reconcile_key_always_reconciles() {
        let config: Config = toml::from_str("port_stride = 10").expect("parse");
        assert_eq!(config.reconcile, ReconcileMode::Always);
    }

    #[test]
    fn a_reconcile_mode_is_named_in_lowercase() {
        let config: Config = toml::from_str(r#"reconcile = "auto""#).expect("parse");
        assert_eq!(config.reconcile, ReconcileMode::Auto);
        assert!(toml::from_str::<Config>(r#"reconcile = "sometimes""#).is_err());
    }
}
