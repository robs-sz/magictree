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

/// The primary checkout as a source for `sync`ed outputs. `None` in the
/// primary checkout itself: it *is* the source, and never links into itself.
pub struct Source {
    /// The primary checkout's copy of the manifest directory being bootstrapped.
    pub manifest_dir: PathBuf,
    /// What the primary checkout's own bootstrap recorded, so a linked output
    /// can be checked against the inputs it was installed for.
    pub state: State,
}

/// Resolve the primary checkout's copy of `manifest_dir`, and the state it
/// recorded. Paths are relative per manifest, so the same app directory
/// strip/join rule `sync_files` uses applies here.
pub fn share_source(
    repo: &Repo,
    paths: &crate::paths::Paths,
    manifest_dir: &Path,
) -> Result<Option<Source>> {
    if repo.is_main_worktree() {
        return Ok(None);
    }
    let source_root = repo.main_worktree_root();
    let prefix = manifest_dir
        .strip_prefix(&repo.worktree_root)
        .unwrap_or(Path::new(""));
    // `join("")` would leave a trailing separator, and the state key is the
    // manifest path verbatim as the primary checkout recorded it.
    let source_manifest = if prefix.as_os_str().is_empty() {
        source_root
    } else {
        source_root.join(prefix)
    };
    let runtime_dir = paths.worktree_dir(&repo.key(), crate::repo::PRIMARY_WORKTREE_ID);
    let state = load_state(&runtime_dir);
    Ok(Some(Source {
        manifest_dir: source_manifest,
        state,
    }))
}

/// Link untracked or generated paths from the primary checkout into this
/// worktree. Paths are relative to the manifest that declares them, so an app
/// manifest's `sync = ["node_modules"]` links `<app>/node_modules`.
/// Existing paths are never replaced.
///
/// A path a step declares as one of its `outputs` is only linked while the
/// primary checkout's copy answers this checkout's inputs: the same input
/// digests on both sides, and the primary's own state recorded that digest for
/// the step's command. Without that proof the path is left to install rather
/// than linked, so a branch with different lockfiles never runs the primary
/// checkout's dependencies.
pub fn sync_files(
    repo: &Repo,
    worktree_root: &Path,
    manifest_dir: &Path,
    bootstrap: &crate::manifest::Bootstrap,
    source: Option<&Source>,
    link: bool,
) -> Result<Vec<String>> {
    let mut messages = Vec::new();
    if repo.is_main_worktree() {
        return Ok(messages);
    }
    let source_root = repo.main_worktree_root();
    let prefix = manifest_dir
        .strip_prefix(worktree_root)
        .unwrap_or(Path::new(""));
    for relative in &bootstrap.sync {
        if !link {
            messages.push(format!(
                "sync {relative}: disabled by config (sync = false), left to install"
            ));
            continue;
        }
        let source_path = source_root.join(prefix).join(relative);
        let destination = manifest_dir.join(relative);
        if destination.exists() {
            messages.push(format!("sync {relative}: present"));
            continue;
        }
        if !source_path.exists() {
            messages.push(format!("sync {relative}: missing in main checkout"));
            continue;
        }
        // The gate: a step that declares this path as an output is what proves
        // the primary checkout's copy answers this checkout's lockfile.
        let gate = bootstrap
            .run
            .iter()
            .chain(bootstrap.after.iter())
            .find(|step| step.outputs().iter().any(|output| output == relative));
        if let Some(step) = gate {
            let Some(source) = source else {
                messages.push(format!(
                    "sync {relative}: no inputs prove the primary checkout's copy matches, left to install"
                ));
                continue;
            };
            if step.inputs().is_empty() {
                messages.push(format!(
                    "sync {relative}: no inputs prove the primary checkout's copy matches, left to install"
                ));
                continue;
            }
            if !source_installed(step, manifest_dir, source)? {
                messages.push(format!(
                    "sync {relative}: the primary checkout installed different {}, left to install",
                    step.inputs().join(", ")
                ));
                continue;
            }
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        symlink(&source_path, &destination)
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
    source: Option<&Source>,
    verbose: bool,
    progress: &mut crate::progress::Progress,
) -> Result<Vec<String>> {
    let mut state = load_state(runtime_dir);
    let mut messages = Vec::new();
    for step in steps {
        let mut bar = progress.step();
        let mut local: Vec<String> = Vec::new();
        let command = step.command();
        // Two apps can declare the same command with different inputs, so the
        // cache entry belongs to the manifest that declared it.
        let key = format!("{}::{command}", manifest_dir.display());
        let digest = if step.inputs().is_empty() {
            None
        } else {
            Some(inputs_digest(manifest_dir, step.inputs())?)
        };
        let outputs = step.outputs();
        let mut present = outputs
            .iter()
            .all(|output| manifest_dir.join(output).exists());
        let any_link = outputs
            .iter()
            .any(|output| is_symlink(&manifest_dir.join(output)));
        let linked_ok = any_link && linked_outputs_current(step, manifest_dir, source)?;
        // A symlink that is not proven current is not an install: with `sync`
        // disabled, or with a lockfile the primary checkout does not share, the
        // step has to run — unlinking the link first — rather than trust it.
        if any_link && !linked_ok {
            present = false;
        }
        let cached = digest
            .as_deref()
            .is_some_and(|digest| state.bootstrap.get(&key).map(String::as_str) == Some(digest));

        // A declared artefact that is missing is the one thing no record may
        // overrule: fall through and run.
        if !outputs.is_empty() && !present {
            // run
        } else if linked_ok {
            // The link to the primary checkout still answers this step's
            // inputs. Record the digest anyway — this code re-derives the
            // link's validity, but a binary without `outputs` support reads the
            // entry as "already installed" and skips, instead of running the
            // installer through the symlink into the primary checkout.
            if let Some(digest) = digest.clone() {
                state.bootstrap.insert(key.clone(), digest);
            }
            local.push(format!("run {command}: present"));
            emit(&mut bar, &local);
            messages.extend(local);
            continue;
        } else if digest.is_none() && !outputs.is_empty() {
            // Output declared, present, nothing to compare: it is installed.
            local.push(format!("run {command}: present"));
            emit(&mut bar, &local);
            messages.extend(local);
            continue;
        } else if cached {
            local.push(format!("run {command}: cached"));
            emit(&mut bar, &local);
            messages.extend(local);
            continue;
        }

        if step.asks() && !confirm(command) {
            local.push(format!("run {command}: skipped"));
            emit(&mut bar, &local);
            messages.extend(local);
            continue;
        }
        // Never write through a link into the primary checkout: remove every
        // declared output that is a symlink before running the installer.
        for output in step.outputs() {
            let path = manifest_dir.join(output);
            if is_symlink(&path) {
                std::fs::remove_file(&path)
                    .with_context(|| format!("unlinking {}", path.display()))?;
                local.push(format!(
                    "run {command}: unlinked {output} from the primary checkout"
                ));
            }
        }
        run::run_once(command, manifest_dir, env, verbose)
            .with_context(|| format!("{phase} step '{command}' failed"))?;
        if let Some(digest) = digest {
            state.bootstrap.insert(key, digest);
        }
        local.push(format!("run {command}: ok"));
        emit(&mut bar, &local);
        messages.extend(local);
    }
    save_state(runtime_dir, &state)?;
    Ok(messages)
}

/// Print a step's messages: every line but the last as a detail, the last as
/// the step's result.
fn emit(bar: &mut crate::progress::Step, messages: &[String]) {
    if let Some((last, rest)) = messages.split_last() {
        for line in rest {
            bar.detail(line);
        }
        bar.result(last);
    }
}

/// Whether a step's linked outputs still answer the inputs it declares: the
/// primary checkout's copy of those inputs hashes the same, and the primary's
/// own state recorded that digest for this command — the only proof it
/// installed deps for exactly this lockfile rather than an older one.
/// False when nothing is linked: a local install is judged by the record.
fn linked_outputs_current(
    step: &crate::manifest::RunStep,
    manifest_dir: &Path,
    source: Option<&Source>,
) -> Result<bool> {
    let outputs = step.outputs();
    if outputs.is_empty() || step.inputs().is_empty() {
        return Ok(false);
    }
    if !outputs
        .iter()
        .any(|output| is_symlink(&manifest_dir.join(output)))
    {
        return Ok(false);
    }
    let Some(source) = source else {
        return Ok(false);
    };
    source_installed(step, manifest_dir, source)
}

/// Whether `source`'s copy of `step` answers the inputs `manifest_dir`
/// declares: same input digests on both sides, and the source recorded the
/// digest for this command.
fn source_installed(
    step: &crate::manifest::RunStep,
    manifest_dir: &Path,
    source: &Source,
) -> Result<bool> {
    let here = inputs_digest(manifest_dir, step.inputs())?;
    let Ok(there) = inputs_digest(&source.manifest_dir, step.inputs()) else {
        return Ok(false);
    };
    if here != there {
        return Ok(false);
    }
    let key = format!("{}::{}", source.manifest_dir.display(), step.command());
    Ok(source.state.bootstrap.get(&key).map(String::as_str) == Some(here.as_str()))
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
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
///
/// A directory input means sources, not the caches a build writes next to them:
/// `__pycache__`, `node_modules`, virtualenvs and the rest of `IGNORED` are
/// skipped, so a rewritten `.pyc` does not invalidate a step whose real input
/// did not change.
fn hash_dir(hasher: &mut Sha256, dir: &Path) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| !is_ignored_cache(path))
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

/// What a directory input ignores: the caches a build writes next to the
/// sources, plus git's own bookkeeping.
const IGNORED: &[&str] = &[
    "__pycache__",
    ".git",
    "node_modules",
    ".venv",
    ".pytest_cache",
    ".ruff_cache",
    ".mypy_cache",
    ".DS_Store",
];

fn is_ignored_cache(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if IGNORED.contains(&name) {
        return true;
    }
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("pyc" | "pyo" | "pyi")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::RunStep;

    fn step(command: &str, inputs: &[&str], outputs: &[&str], ask: bool) -> RunStep {
        RunStep::Detailed {
            command: command.to_string(),
            inputs: inputs.iter().map(|input| input.to_string()).collect(),
            outputs: outputs.iter().map(|output| output.to_string()).collect(),
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
            &[step(&command, &[], &[], true)],
            &BTreeMap::new(),
            &|_| false,
            None,
            false,
            &mut crate::progress::Progress::silent(),
        )
        .expect("run_steps");
        assert_eq!(declined, vec![format!("run {command}: skipped")]);
        assert!(!marker.exists(), "a declined step must not run");

        let accepted = run_steps(
            &runtime,
            worktree.path(),
            "after",
            &[step(&command, &[], &[], true)],
            &BTreeMap::new(),
            &|_| true,
            None,
            false,
            &mut crate::progress::Progress::silent(),
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
            &[step("true", &["input.txt"], &[], true)],
            &BTreeMap::new(),
            &|_| true,
            None,
            false,
            &mut crate::progress::Progress::silent(),
        )
        .expect("first run");
        assert_eq!(messages, vec!["run true: ok"]);

        let again = run_steps(
            &runtime,
            worktree.path(),
            "after",
            &[step("true", &["input.txt"], &[], true)],
            &BTreeMap::new(),
            &|_| panic!("a cached step must not be asked about"),
            None,
            false,
            &mut crate::progress::Progress::silent(),
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
            &[step("true", &["input.txt"], &[], true)],
            &BTreeMap::new(),
            &|_| false,
            None,
            false,
            &mut crate::progress::Progress::silent(),
        )
        .expect("declined run");

        // The decline cached nothing, so a later yes runs the step.
        let messages = run_steps(
            &runtime,
            worktree.path(),
            "after",
            &[step("true", &["input.txt"], &[], true)],
            &BTreeMap::new(),
            &|_| true,
            None,
            false,
            &mut crate::progress::Progress::silent(),
        )
        .expect("later run");
        assert_eq!(messages, vec!["run true: ok"]);
    }

    #[test]
    fn a_step_with_outputs_skips_while_present() {
        let worktree = tempfile::tempdir().expect("temp dir");
        let runtime = worktree.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("runtime dir");

        let first = run_steps(
            &runtime,
            worktree.path(),
            "bootstrap",
            &[step("true", &[], &["out.txt"], false)],
            &BTreeMap::new(),
            &|_| true,
            None,
            false,
            &mut crate::progress::Progress::silent(),
        )
        .expect("first run");
        assert_eq!(first, vec!["run true: ok"]);

        std::fs::write(worktree.path().join("out.txt"), "made").expect("output");

        let again = run_steps(
            &runtime,
            worktree.path(),
            "bootstrap",
            &[step("true", &[], &["out.txt"], false)],
            &BTreeMap::new(),
            &|_| panic!("a present output must not run the step"),
            None,
            false,
            &mut crate::progress::Progress::silent(),
        )
        .expect("second run");
        assert_eq!(again, vec!["run true: present"]);
    }

    #[test]
    fn a_missing_output_makes_a_cached_step_run() {
        let worktree = tempfile::tempdir().expect("temp dir");
        let runtime = worktree.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("runtime dir");
        std::fs::write(worktree.path().join("input.txt"), "v1").expect("input");
        let command = "touch out.txt";
        let step = step(command, &["input.txt"], &["out.txt"], false);

        let first = run_steps(
            &runtime,
            worktree.path(),
            "bootstrap",
            std::slice::from_ref(&step),
            &BTreeMap::new(),
            &|_| true,
            None,
            false,
            &mut crate::progress::Progress::silent(),
        )
        .expect("first run");
        assert_eq!(first, vec![format!("run {command}: ok")]);

        let cached = run_steps(
            &runtime,
            worktree.path(),
            "bootstrap",
            std::slice::from_ref(&step),
            &BTreeMap::new(),
            &|_| true,
            None,
            false,
            &mut crate::progress::Progress::silent(),
        )
        .expect("second run");
        assert_eq!(cached, vec![format!("run {command}: cached")]);

        // The declared artefact is missing even though the digest is unchanged:
        // the record must not overrule that.
        std::fs::remove_file(worktree.path().join("out.txt")).expect("remove output");
        let run_again = run_steps(
            &runtime,
            worktree.path(),
            "bootstrap",
            std::slice::from_ref(&step),
            &BTreeMap::new(),
            &|_| true,
            None,
            false,
            &mut crate::progress::Progress::silent(),
        )
        .expect("third run");
        assert_eq!(run_again, vec![format!("run {command}: ok")]);
    }

    #[test]
    fn hash_dir_ignores_python_caches() {
        let worktree = tempfile::tempdir().expect("temp dir");
        let src = worktree.path().join("src");
        std::fs::create_dir_all(src.join("__pycache__")).expect("create src");
        std::fs::write(src.join("app.py"), "print('v1')").expect("source");
        std::fs::write(src.join("__pycache__/app.pyc"), "cache-v1").expect("cache");

        let digest = || inputs_digest(worktree.path(), &["src".to_string()]).expect("digest");
        let before = digest();

        // A rewritten bytecode cache must not invalidate the input.
        std::fs::write(src.join("__pycache__/app.pyc"), "cache-v2").expect("cache");
        assert_eq!(before, digest(), "a .pyc change is not a source change");

        // A real source change still does.
        std::fs::write(src.join("app.py"), "print('v2')").expect("source");
        assert_ne!(before, digest(), "a source change invalidates the input");
    }
}
