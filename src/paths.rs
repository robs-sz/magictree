use anyhow::{Context, Result};
use std::path::PathBuf;

/// Machine-global locations owned by magictree.
#[derive(Debug, Clone)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let state_dir = match std::env::var_os("MAGICTREE_STATE_DIR") {
            Some(value) => PathBuf::from(value),
            None => home()?.join(".local/state/magictree"),
        };
        let config_dir = match std::env::var_os("MAGICTREE_CONFIG_DIR") {
            Some(value) => PathBuf::from(value),
            None => home()?.join(".config/magictree"),
        };
        Ok(Self {
            state_dir,
            config_dir,
        })
    }

    pub fn blocks_dir(&self) -> PathBuf {
        self.state_dir.join("blocks")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
