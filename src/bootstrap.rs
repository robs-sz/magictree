use crate::repo::Repo;
use crate::run;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub bootstrap: BTreeMap<String, String>,
}

pub fn load_state(runtime_dir: &Path) -> State {
    std::fs::read_to_string(runtime_dir.join("state.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_state(runtime_dir: &Path, state: &State) -> Result<()> {
    let path = runtime_dir.join("state.json");
    let payload = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, format!("{payload}\n"))
        .with_context(|| format!("writing {}", path.display()))
}

/// Link untracked or generated paths from the primary checkout into this
/// worktree. Existing paths are never replaced.
pub fn sync_files(repo: &Repo, worktree_root: &Path, paths: &[String]) -> Result<Vec<String>> {
    let mut messages = Vec::new();
    if repo.is_main_worktree() {
        return Ok(messages);
    }
    let source_root = repo.main_worktree_root();
    for relative in paths {
        let source = source_root.join(relative);
        let destination = worktree_root.join(relative);
        if destination.exists() {
            messages.push(format!("sync {relative}: present"));
            continue;
        }
        if !source.exists() {
            messages.push(format!("sync {relative}: missing in main checkout"));
            continue;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        symlink(&source, &destination)
            .with_context(|| format!("linking {}", destination.display()))?;
        messages.push(format!("sync {relative}: linked"));
    }
    Ok(messages)
}

/// Run bootstrap commands, skipping steps whose declared inputs are unchanged.
pub fn run_steps(
    runtime_dir: &Path,
    worktree_root: &Path,
    manifest_dir: &Path,
    steps: &[crate::manifest::RunStep],
    env: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let mut state = load_state(runtime_dir);
    let mut messages = Vec::new();
    for step in steps {
        let command = step.command();
        let key = command.to_string();
        if !step.inputs().is_empty() {
            let digest = inputs_digest(worktree_root, step.inputs())?;
            if state.bootstrap.get(&key).map(String::as_str) == Some(digest.as_str()) {
                messages.push(format!("run {command}: cached"));
                continue;
            }
            run::run_once(command, manifest_dir, env)
                .with_context(|| format!("bootstrap step '{command}' failed"))?;
            state.bootstrap.insert(key, digest);
        } else {
            run::run_once(command, manifest_dir, env)
                .with_context(|| format!("bootstrap step '{command}' failed"))?;
        }
        messages.push(format!("run {command}: ok"));
    }
    save_state(runtime_dir, &state)?;
    Ok(messages)
}

fn inputs_digest(worktree_root: &Path, inputs: &[String]) -> Result<String> {
    let mut hasher = Sha256::new();
    for relative in inputs {
        hasher.update(relative.as_bytes());
        let path = worktree_root.join(relative);
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("bootstrap input '{relative}' does not exist"))?;
        if metadata.is_dir() {
            hash_dir(&mut hasher, &path)
                .with_context(|| format!("hashing bootstrap input '{relative}'"))?;
        } else {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading bootstrap input '{relative}'"))?;
            hasher.update(&bytes);
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Hash a directory tree: names and contents in sorted order, so the digest
/// changes whenever anything under the directory does. A constant digest would
/// make the step report "cached" forever.
fn hash_dir(hasher: &mut Sha256, dir: &Path) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for entry in entries {
        let name = entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        hasher.update(name.as_bytes());
        if entry.is_dir() {
            hash_dir(hasher, &entry)?;
        } else if entry.is_file() {
            hasher.update(&std::fs::read(&entry)?);
        }
        // Sockets and other specials contribute their name only.
    }
    Ok(())
}
