use crate::paths::Paths;
use anyhow::{Context, Result};
use serde::Deserialize;

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port_range_start: 20000,
            port_range_end: 32767,
            port_stride: 20,
            stop_timeout_secs: 10,
            health_timeout_secs: 60,
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
