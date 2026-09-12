use crate::slug::short_hash;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Repo {
    pub worktree_root: PathBuf,
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
}

impl Repo {
    pub fn open(from: &Path) -> Result<Self> {
        let worktree_root = PathBuf::from(run_git(from, &["rev-parse", "--show-toplevel"])?);
        let common_dir = PathBuf::from(run_git(
            from,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?);
        let git_dir = PathBuf::from(run_git(
            from,
            &["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
        )?);
        Ok(Self {
            worktree_root,
            common_dir,
            git_dir,
        })
    }

    /// Stable identity for the repository, shared by all of its worktrees.
    pub fn key(&self) -> String {
        short_hash(&self.common_dir.to_string_lossy())
    }

    /// Stable identity for this worktree: `main` for the primary checkout, the
    /// administrative directory name for linked worktrees.
    pub fn worktree_id(&self) -> String {
        if self.git_dir == self.common_dir {
            return "main".to_string();
        }
        self.git_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "worktree".to_string())
    }

    pub fn is_main_worktree(&self) -> bool {
        self.git_dir == self.common_dir
    }

    pub fn worktrees(&self) -> Result<Vec<WorktreeEntry>> {
        let raw = run_git(&self.worktree_root, &["worktree", "list", "--porcelain"])?;
        let mut entries = Vec::new();
        let mut current: Option<WorktreeEntry> = None;
        for line in raw.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                if let Some(entry) = current.take() {
                    entries.push(entry);
                }
                current = Some(WorktreeEntry {
                    path: PathBuf::from(path),
                    branch: None,
                    detached: false,
                    bare: false,
                });
                continue;
            }
            let Some(entry) = current.as_mut() else {
                continue;
            };
            if let Some(branch) = line.strip_prefix("branch ") {
                entry.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_string(),
                );
            } else if line == "detached" {
                entry.detached = true;
            } else if line == "bare" {
                entry.bare = true;
            }
        }
        if let Some(entry) = current {
            entries.push(entry);
        }
        Ok(entries)
    }

    /// The primary checkout, used as the source for bootstrap file sync.
    pub fn main_worktree_root(&self) -> PathBuf {
        self.worktrees()
            .ok()
            .and_then(|entries| {
                entries
                    .into_iter()
                    .find(|entry| !entry.bare)
                    .map(|entry| entry.path)
            })
            .unwrap_or_else(|| self.worktree_root.clone())
    }
}

/// Whether git tracks this path in the repository at `dir`. A manifest that is
/// not tracked is absent from every new worktree, which is the difference
/// between a stack that inherits and one that silently does not.
pub fn is_tracked(dir: &Path, path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(path)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
