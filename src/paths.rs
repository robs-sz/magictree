use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

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

    /// Runtime state of every worktree magictree has touched, keyed by
    /// repository then worktree so it outlives the checkout it describes.
    pub fn worktrees_dir(&self) -> PathBuf {
        self.state_dir.join("worktrees")
    }

    pub fn worktree_dir(&self, repo_key: &str, worktree_id: &str) -> PathBuf {
        self.worktrees_dir().join(repo_key).join(worktree_id)
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

/// Move a directory into place, falling back to a copy when source and
/// destination are on different filesystems — a checkout can live on an
/// external volume while the state dir sits under `$HOME`.
pub fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("moving {} to {}", from.display(), to.display())
            })
        }
    }
    copy_dir(from, to)?;
    std::fs::remove_dir_all(from).with_context(|| format!("removing {}", from.display()))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from)
        .with_context(|| format!("reading {}", from.display()))?
        .flatten()
    {
        let target = to.join(entry.file_name());
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
