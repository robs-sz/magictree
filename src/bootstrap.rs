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
/// worktree. Paths are relative to the manifest that declares them, so an app
/// manifest's `sync = ["node_modules"]` links `<app>/node_modules`.
/// Existing paths are never replaced.
pub fn sync_files(
    repo: &Repo,
    worktree_root: &Path,
    manifest_dir: &Path,
    paths: &[String],
) -> Result<Vec<String>> {
    let mut messages = Vec::new();
    if repo.is_main_worktree() {
        return Ok(messages);
    }
    let source_root = repo.main_worktree_root();
    let prefix = manifest_dir
        .strip_prefix(worktree_root)
        .unwrap_or(Path::new(""));
    for relative in paths {
        let source = source_root.join(prefix).join(relative);
        let destination = manifest_dir.join(relative);
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

/// Run one phase's bootstrap commands, skipping steps whose declared inputs are
/// unchanged. Inputs are relative to the manifest that declares them, matching
/// the command's working directory and `doctor`'s existence check: an app
/// manifest's `inputs = ["uv.lock"]` means `<app>/uv.lock`.
///
/// `phase` names the phase in messages only: the cache entry belongs to the
/// manifest that declared the command and to the command itself, so the same
/// command in `run` and in `after` means the same thing by `inputs`.
///
/// Steps marked `ask` run only when `confirm` says yes; a decline skips the
/// step and caches nothing, so the next run asks again. A cached step never
/// reaches `confirm`: there is nothing to decide.
pub fn run_steps(
    runtime_dir: &Path,
    manifest_dir: &Path,
    phase: &str,
    steps: &[crate::manifest::RunStep],
    env: &BTreeMap<String, String>,
    confirm: &dyn Fn(&str) -> bool,
) -> Result<Vec<String>> {
    let mut state = load_state(runtime_dir);
    let mut messages = Vec::new();
    for step in steps {
        let command = step.command();
        // Two apps can declare the same command with different inputs, so the
        // cache entry belongs to the manifest that declared it.
        let key = format!("{}::{command}", manifest_dir.display());
        let digest = if step.inputs().is_empty() {
            None
        } else {
            Some(inputs_digest(manifest_dir, step.inputs())?)
        };
        if let Some(digest) = &digest {
            if state.bootstrap.get(&key).map(String::as_str) == Some(digest.as_str()) {
                messages.push(format!("run {command}: cached"));
                continue;
            }
        }
        if step.asks() && !confirm(command) {
            messages.push(format!("run {command}: skipped"));
            continue;
        }
        run::run_once(command, manifest_dir, env)
            .with_context(|| format!("{phase} step '{command}' failed"))?;
        if let Some(digest) = digest {
            state.bootstrap.insert(key, digest);
        }
        messages.push(format!("run {command}: ok"));
    }
    save_state(runtime_dir, &state)?;
    Ok(messages)
}

/// The question `up` puts before a step marked `ask`.
pub fn prompt(command: &str) -> bool {
    confirm(&format!("Run task: {command}"))
}

/// Ask a yes/no question on the terminal. Anything but y/yes is a no, and a run
/// without a terminal — scripts, agents, CI — always answers no, so an
/// unattended `up` neither blocks nor takes unconfirmed action.
pub fn confirm(question: &str) -> bool {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} (y/N) ");
    if std::io::stdout().flush().is_err() {
        return false;
    }
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn inputs_digest(manifest_dir: &Path, inputs: &[String]) -> Result<String> {
    let mut hasher = Sha256::new();
    for relative in inputs {
        hasher.update(relative.as_bytes());
        let path = manifest_dir.join(relative);
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("bootstrap input '{relative}' does not exist"))?;
        if metadata.is_dir() {
            hash_dir(&mut hasher, &path)
                .with_context(|| format!("hashing bootstrap input '{relative}'"))?;
        } else {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading bootstrap input '{relative}'"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::RunStep;

    fn step(command: &str, inputs: &[&str], ask: bool) -> RunStep {
        RunStep::Detailed {
            command: command.to_string(),
            inputs: inputs.iter().map(|input| input.to_string()).collect(),
            ask,
        }
    }

    #[test]
    fn an_asked_step_runs_only_on_a_yes() {
        let worktree = tempfile::tempdir().expect("temp dir");
        let runtime = worktree.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("runtime dir");
        let marker = worktree.path().join("marker");
        let command = format!("echo done > {}", marker.display());

        let declined = run_steps(
            &runtime,
            worktree.path(),
            "after",
            &[step(&command, &[], true)],
            &BTreeMap::new(),
            &|_| false,
        )
        .expect("run_steps");
        assert_eq!(declined, vec![format!("run {command}: skipped")]);
        assert!(!marker.exists(), "a declined step must not run");

        let accepted = run_steps(
            &runtime,
            worktree.path(),
            "after",
            &[step(&command, &[], true)],
            &BTreeMap::new(),
            &|_| true,
        )
        .expect("run_steps");
        assert_eq!(accepted, vec![format!("run {command}: ok")]);
        assert!(marker.exists(), "an accepted step runs");
    }

    #[test]
    fn a_cached_step_is_never_asked_about() {
        let worktree = tempfile::tempdir().expect("temp dir");
        let runtime = worktree.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("runtime dir");
        std::fs::write(worktree.path().join("input.txt"), "v1").expect("input");

        let messages = run_steps(
            &runtime,
            worktree.path(),
            "after",
            &[step("true", &["input.txt"], true)],
            &BTreeMap::new(),
            &|_| true,
        )
        .expect("first run");
        assert_eq!(messages, vec!["run true: ok"]);

        let again = run_steps(
            &runtime,
            worktree.path(),
            "after",
            &[step("true", &["input.txt"], true)],
            &BTreeMap::new(),
            &|_| panic!("a cached step must not be asked about"),
        )
        .expect("second run");
        assert_eq!(again, vec!["run true: cached"]);
    }

    #[test]
    fn a_declined_step_is_not_cached() {
        let worktree = tempfile::tempdir().expect("temp dir");
        let runtime = worktree.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("runtime dir");
        std::fs::write(worktree.path().join("input.txt"), "v1").expect("input");

        run_steps(
            &runtime,
            worktree.path(),
            "after",
            &[step("true", &["input.txt"], true)],
            &BTreeMap::new(),
            &|_| false,
        )
        .expect("declined run");

        // The decline cached nothing, so a later yes runs the step.
        let messages = run_steps(
            &runtime,
            worktree.path(),
            "after",
            &[step("true", &["input.txt"], true)],
            &BTreeMap::new(),
            &|_| true,
        )
        .expect("later run");
        assert_eq!(messages, vec!["run true: ok"]);
    }
}
