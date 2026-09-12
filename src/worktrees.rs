use crate::paths::Paths;
use crate::ports::{self, Assignment};
use crate::repo::is_tracked;
use crate::repo::Repo;
use crate::run;
use crate::slug::slugify;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Create a worktree for `branch` and return its path.
///
/// Placement mirrors the primary checkout's parent directory, unless an
/// explicit path is given. An existing local branch is checked out; otherwise
/// the branch is created from `base`.
pub fn create(
    repo: &Repo,
    branch: &str,
    base: Option<&str>,
    explicit_path: Option<&Path>,
    detach: bool,
) -> Result<PathBuf> {
    if branch.trim().is_empty() {
        bail!("branch name is required");
    }
    if base.is_some() && detach {
        bail!("--base and --detach cannot be combined");
    }

    let path = match explicit_path {
        Some(path) => absolute(path)?,
        None => default_path(repo, branch)?,
    };

    if path.exists() {
        bail!("target path already exists: {}", path.display());
    }

    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    if detach {
        args.push("--detach".into());
        if let Some(base) = base {
            args.push(base.to_string());
        } else {
            args.push("HEAD".into());
        }
        args.push(path.to_string_lossy().to_string());
    } else if branch_exists(repo, branch) {
        args.push(path.to_string_lossy().to_string());
        args.push(branch.to_string());
    } else {
        args.push("-b".into());
        args.push(branch.to_string());
        args.push(path.to_string_lossy().to_string());
        args.push(base.unwrap_or("HEAD").to_string());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo.worktree_root)
        .args(&args)
        .output()
        .context("running git worktree add")?;
    if !output.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(path)
}

/// Remove a worktree. The branch is never deleted.
pub fn remove(repo: &Repo, target: &Path, force: bool) -> Result<()> {
    let target = absolute(target)?;
    if same_worktree(&target, &repo.worktree_root) {
        bail!("refusing to remove the current worktree");
    }
    let mut args = vec!["worktree".to_string(), "remove".to_string()];
    if force {
        args.push("--force".to_string());
    }
    args.push(target.to_string_lossy().to_string());

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo.worktree_root)
        .args(&args)
        .output()
        .context("running git worktree remove")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("contains modified or untracked files") {
            bail!(
                "{} has uncommitted changes; re-run with --force to discard them",
                target.display()
            );
        }
        bail!("git worktree remove failed: {}", stderr.trim());
    }
    Ok(())
}

/// Explain what a prune would drop from the worktree administrative list.
pub fn prune(repo: &Repo, dry_run: bool) -> Result<()> {
    let args: Vec<&str> = if dry_run {
        vec!["worktree", "prune", "--dry-run", "--verbose"]
    } else {
        vec!["worktree", "prune", "--verbose"]
    };
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo.worktree_root)
        .args(&args)
        .output()
        .context("running git worktree prune")?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        println!("{line}");
    }
    if !output.status.success() {
        bail!(
            "git worktree prune failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The main checkout has a manifest that git does not track, so no new worktree
/// will inherit it. Returns the explanation, or `None` when there is nothing to
/// warn about.
pub fn untracked_manifest_hint(repo: &Repo) -> Option<String> {
    let main = repo.main_worktree_root();
    let manifest = main.join(crate::manifest::MANIFEST_FILE);
    if !manifest.is_file() || is_tracked(&main, &manifest) {
        return None;
    }
    let name = crate::manifest::MANIFEST_FILE;
    let mut hint = String::new();
    let _ = writeln!(
        hint,
        "the main checkout has an uncommitted {name}, so new worktrees will not inherit it"
    );
    let _ = writeln!(
        hint,
        "  commit it there so every worktree inherits the stack definition:"
    );
    let _ = writeln!(
        hint,
        "    git -C {} add {} && git -C {} commit -m \"add {name}\"",
        main.display(),
        name,
        main.display()
    );
    Some(hint.trim_end().to_string())
}

/// Explain why `worktree` has no manifest when the main checkout has one.
///
/// A committed manifest is inherited by every worktree for free; an uncommitted
/// one is not, and the resulting "no magictree.toml found" says nothing about
/// why. Returns `None` when there is nothing useful to add.
pub fn missing_manifest_hint(repo: &Repo, worktree: &Path) -> Option<String> {
    let manifest = worktree.join(crate::manifest::MANIFEST_FILE);
    if manifest.is_file() {
        return None;
    }
    let main = repo.main_worktree_root();
    if same_worktree(&main, &repo.worktree_root) {
        // Already in the main checkout: the manifest simply is not there.
        return None;
    }
    let main_manifest = main.join(crate::manifest::MANIFEST_FILE);
    if !main_manifest.is_file() {
        return None;
    }

    let name = crate::manifest::MANIFEST_FILE;
    if let Some(hint) = untracked_manifest_hint(repo) {
        return Some(format!(
            "{hint}\n  or copy it into this worktree for a one-off:  cp {}/{} .",
            main.display(),
            name
        ));
    }

    let mut hint = String::new();
    let _ = writeln!(
        hint,
        "the main checkout has {name}, but this branch does not, so this worktree has no stack definition"
    );
    let _ = writeln!(
        hint,
        "  check out a branch that contains it, or copy it:  cp {}/{} .",
        main.display(),
        name
    );
    Some(hint.trim_end().to_string())
}

/// Where `new` would place a worktree for this branch.
pub fn plan_path(repo: &Repo, branch: &str) -> Result<PathBuf> {
    default_path(repo, branch)
}

fn default_path(repo: &Repo, branch: &str) -> Result<PathBuf> {
    let parent = repo
        .worktree_root
        .parent()
        .context("worktree root has no parent directory")?;
    let name = repo
        .worktree_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    Ok(parent.join(format!("{name}-{}", slugify(branch))))
}

fn branch_exists(repo: &Repo, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(&repo.worktree_root)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

/// Compare two worktree paths without requiring either to exist on disk.
/// A path may be reached through a symlink (`/var` and `/private/var` on
/// macOS), so canonicalize before falling back to name-plus-parent matching.
fn same_worktree(a: &Path, b: &Path) -> bool {
    let absolute = |path: &Path| -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    };
    let (a, b) = (absolute(a), absolute(b));
    if a == b {
        return true;
    }
    let canonical =
        |path: &Path| -> PathBuf { path.canonicalize().unwrap_or_else(|_| path.to_path_buf()) };
    if canonical(&a) == canonical(&b) {
        return true;
    }
    // A deleted checkout cannot be canonicalized, so compare its directory name
    // with a canonicalized parent.
    let key = |path: &Path| {
        (
            path.file_name().map(|name| name.to_os_string()),
            path.parent().map(canonical),
        )
    };
    key(&a) == key(&b) && a.parent().is_some()
}

/// Release port blocks whose worktree no longer exists, stop the processes they
/// left running, and report compose resources still labelled for them.
pub fn gc(paths: &Paths, repo: &Repo, timeout: Duration, apply: bool) -> Result<()> {
    // A checkout whose directory is gone is dead even when git still lists it,
    // because nothing can be running there. `--prune` clears the registration.
    let live: Vec<PathBuf> = repo
        .worktrees()?
        .into_iter()
        .filter(|entry| entry.path.exists())
        .map(|entry| entry.path)
        .collect();

    let blocks = paths.blocks_dir();
    let mut released = 0usize;
    if blocks.exists() {
        for entry in std::fs::read_dir(&blocks)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(assignment) = serde_json::from_str::<Assignment>(&raw) else {
                continue;
            };
            // Blocks belong to a repository; leave other repositories alone.
            if assignment.repo_key != repo.key() {
                continue;
            }
            let owner = assignment.worktree_path.clone();
            let stale = match owner {
                Some(owner) => {
                    let owner = PathBuf::from(owner);
                    !live.iter().any(|path| same_worktree(path, &owner))
                }
                // Blocks written before paths were recorded cannot be checked
                // against the worktree list; leave them alone.
                None => false,
            };
            if stale {
                released += 1;
                println!(
                    "{} block {}-{} (worktree {})",
                    if apply { "released" } else { "would release" },
                    assignment.base,
                    assignment.base.saturating_add(1),
                    assignment.worktree_id
                );
                if apply {
                    std::fs::remove_file(&path)?;
                }
            }
        }
    }

    if released == 0 {
        println!("no stale port blocks for this repository");
    }
    let projects = sweep_worktree_state(paths, repo, &live, timeout, apply)?;
    sweep_compose(&live, &repo.key(), &projects, apply)?;
    Ok(())
}

/// Stop the supervised processes of worktrees whose checkout is gone, then drop
/// their state directories. Returns the compose project names those worktrees
/// recorded, which is the only way to reach a project whose containers are
/// already gone.
///
/// This is the half of `gc` that cannot be done from the checkout: the checkout
/// has been deleted, so the state dir is the only surviving record of what was
/// running. State written before runtime state moved into the state dir died
/// with its checkout and cannot be reclaimed — only reported as gone.
fn sweep_worktree_state(
    paths: &Paths,
    repo: &Repo,
    live: &[PathBuf],
    timeout: Duration,
    apply: bool,
) -> Result<Vec<String>> {
    let root = paths.worktrees_dir().join(repo.key());
    let mut projects = Vec::new();
    let mut stale = 0usize;
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            // Without an owner this state cannot be attributed to a checkout,
            // and dropping it would be a guess.
            let Some(owner) = state_owner(&dir) else {
                continue;
            };
            if live
                .iter()
                .any(|path| same_worktree(path, Path::new(&owner)))
            {
                continue;
            }
            stale += 1;
            projects.extend(recorded_project(&dir));
            for (name, pid) in run::recorded(&dir) {
                if run::is_alive(pid) {
                    println!(
                        "{} {name} (pid {pid}) of worktree {owner}",
                        if apply { "stopping" } else { "would stop" }
                    );
                }
            }
            if apply {
                run::stop_all(&dir, timeout)?;
                std::fs::remove_dir_all(&dir)
                    .with_context(|| format!("removing {}", dir.display()))?;
            }
        }
    }
    if stale == 0 {
        println!("no stale worktree state for this repository");
    }
    Ok(projects)
}

/// The checkout a state directory describes, from its own `ports.json`.
fn state_owner(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join("ports.json")).ok()?;
    serde_json::from_str::<Assignment>(&raw).ok()?.worktree_path
}

/// The compose project a worktree last ran, from the environment `up` wrote.
fn recorded_project(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join("env")).ok()?;
    raw.lines()
        .find_map(|line| line.strip_prefix("COMPOSE_PROJECT_NAME="))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Remove containers and volumes of worktrees that no longer exist.
/// Resources of unrelated projects are never touched.
///
/// `recorded` carries the compose project names of dead worktrees, because a
/// project whose containers are already gone (a manual `docker compose down`
/// without `-v`, say) is otherwise invisible.
fn sweep_compose(
    live: &[PathBuf],
    repo_key: &str,
    recorded: &[String],
    apply: bool,
) -> Result<()> {
    // Scoped to this repository: another repository's worktrees are not ours to
    // decide about, however stale their paths look from here.
    let filter = format!("label=magictree.repo={repo_key}");
    let mut owner_projects: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(containers) = docker_ids(&[
        "ps",
        "-a",
        "--filter",
        &filter,
        "--format",
        "{{.ID}}\t{{.Label \"magictree.worktree\"}}\t{{.Label \"magictree.project\"}}",
    ])? {
        for line in containers.lines().filter(|line| !line.trim().is_empty()) {
            let mut parts = line.split('\t');
            let Some(id) = parts.next() else { continue };
            let owner = parts.next().unwrap_or_default().to_string();
            let project = parts.next().unwrap_or_default().to_string();
            if owner.is_empty() {
                continue;
            }
            if live
                .iter()
                .any(|path| same_worktree(path, Path::new(&owner)))
            {
                continue;
            }
            owner_projects
                .entry(project)
                .or_default()
                .push(id.to_string());
        }
    }

    let mut projects: BTreeSet<String> = owner_projects.keys().cloned().collect();
    projects.extend(recorded.iter().cloned());
    if projects.is_empty() {
        println!("no orphaned compose containers for this repository");
        return Ok(());
    }

    for (project, ids) in &owner_projects {
        println!(
            "{} {} orphaned container(s) in project '{project}'",
            if apply { "removing" } else { "would remove" },
            ids.len()
        );
        if apply {
            let mut args: Vec<&str> = vec!["rm", "-f"];
            args.extend(ids.iter().map(String::as_str));
            docker_run(&args)?;
        }
    }

    // Volumes are looked up through their project, not through `magictree.repo`:
    // compose labels volumes with its own keys and a service label never reaches
    // them, so a label filter matched nothing and every volume survived gc.
    for project in &projects {
        let filter = format!("label=com.docker.compose.project={project}");
        let Some(volumes) =
            docker_ids(&["volume", "ls", "--filter", &filter, "--format", "{{.Name}}"])?
        else {
            continue;
        };
        for name in volumes.lines().filter(|line| !line.trim().is_empty()) {
            let name = name.trim();
            println!(
                "{} volume {name}",
                if apply { "removing" } else { "would remove" }
            );
            if apply {
                if let Err(error) = docker_run(&["volume", "rm", name]) {
                    // A volume still attached to a container of another project
                    // is not ours to force: report it and reclaim the rest.
                    println!("could not remove volume {name}: {error}");
                }
            }
        }
    }
    Ok(())
}

/// Run a docker command, returning None when docker is unavailable.
fn docker_ids(args: &[&str]) -> Result<Option<String>> {
    let output = match Command::new("docker").args(args).output() {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
}

fn docker_run(args: &[&str]) -> Result<()> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .context("running docker")?;
    if !output.status.success() {
        bail!(
            "docker {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// One-line summary of a block's ports, for `list`.
pub fn describe_ports(assignment: &Assignment) -> String {
    let mut parts: Vec<String> = assignment
        .ports
        .iter()
        .map(|(name, port)| format!("{name}={port}"))
        .collect();
    parts.sort();
    parts.join(" ")
}

/// Rows for `list`: the linked worktrees of this repository with their ports.
///
/// `git worktree list` also reports the primary checkout, but that is not a
/// worktree `rm` can remove — `remove` refuses the current worktree, and it has
/// no administrative directory to name it by. Listing it advertised a row that
/// `rm` always rejected.
pub fn worktree_rows(repo: &Repo, paths: &Paths) -> Result<Vec<(String, PathBuf, String)>> {
    let mut rows = Vec::new();
    for entry in repo.worktrees()? {
        let root = canonical_or_self(&entry.path);
        // A linked worktree has an administrative directory, and that directory
        // name is the identity both `list` and `rm` resolve against. The primary
        // checkout has none, so it is skipped.
        let Some(id) = admin_id(&repo.common_dir, &root) else {
            continue;
        };
        let assignment = ports::load(paths, &repo.key(), &id)?;
        let ports = match assignment {
            Some(assignment) => describe_ports(&assignment),
            None => "-".to_string(),
        };
        rows.push((id, entry.path.clone(), ports));
    }
    Ok(rows)
}

/// Find the administrative directory name git assigned to a worktree path.
fn admin_id(common_dir: &Path, worktree: &Path) -> Option<String> {
    let dir = common_dir.join("worktrees");
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let gitdir_file = entry.path().join("gitdir");
        let Ok(raw) = std::fs::read_to_string(&gitdir_file) else {
            continue;
        };
        let pointer = raw.trim();
        let pointer_path = Path::new(pointer).parent().map(canonical_or_self);
        if pointer_path.as_deref() == Some(worktree) {
            return entry.file_name().to_str().map(|name| name.to_string());
        }
    }
    None
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
