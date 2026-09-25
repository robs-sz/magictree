use crate::config::Config;
use crate::paths::Paths;
use crate::ports::{self, Assignment};
use crate::repo::is_tracked;
use crate::repo::{linked_worktree_id, Repo};
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

    let args = add_args(repo, branch, base, &path, detach);

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

/// The exact git arguments `create` runs, shared with the dry-run printer so
/// the two can never disagree.
pub fn add_args(
    repo: &Repo,
    branch: &str,
    base: Option<&str>,
    path: &Path,
    detach: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    if detach {
        // git's contract is `worktree add [--detach] <path> [<commit-ish>]`:
        // the path comes first, the revision after it.
        args.push("--detach".into());
        args.push(path.to_string_lossy().to_string());
        args.push(base.unwrap_or("HEAD").to_string());
    } else if branch_exists(repo, branch) {
        args.push(path.to_string_lossy().to_string());
        args.push(branch.to_string());
    } else {
        args.push("-b".into());
        args.push(branch.to_string());
        args.push(path.to_string_lossy().to_string());
        args.push(base.unwrap_or("HEAD").to_string());
    }
    args
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

/// What a recorded checkout resolves to.
enum Checkout {
    /// Still a worktree of the recorded repository.
    Live(Repo),
    /// Gone, or no longer a worktree of that repository.
    Gone,
    /// git could not be run at all: not an answer, so records are left alone
    /// rather than reclaimed or reported as gone on a guess.
    Unknown,
}

/// Whether a recorded checkout is still a worktree of the repository the record
/// names.
///
/// A directory at the path is not enough. A worktree removed outside magictree
/// leaves the directory behind, and a path can be reused by something else; both
/// pin the block forever if mere existence counts as live. git is the authority
/// on whether a path is still a worktree of the recorded repository, so it is
/// asked before the record is kept.
fn live_checkout(path: &Path, repo_key: &str) -> bool {
    matches!(checkout_state(path, repo_key), Checkout::Live(_))
}

fn checkout_state(path: &Path, repo_key: &str) -> Checkout {
    if !path.exists() {
        return Checkout::Gone;
    }
    match Repo::open_optional(path) {
        // A path git resolves is live only when it is the worktree root of the
        // repository the record was written for: an unrelated checkout at a
        // recycled path, or a leftover directory that git resolves through its
        // parent repository, is not the worktree this block was allocated to.
        Ok(Some(repo)) => {
            if repo.key() == repo_key && same_worktree(&repo.worktree_root, path) {
                Checkout::Live(repo)
            } else {
                Checkout::Gone
            }
        }
        // No worktree resolves here — an empty husk, not a running stack.
        Ok(None) => Checkout::Gone,
        Err(_) => Checkout::Unknown,
    }
}

/// Release port blocks whose worktree no longer exists, stop the processes they
/// left running, and reclaim compose resources of worktrees that are gone.
pub fn gc(paths: &Paths, repo: &Repo, timeout: Duration, apply: bool) -> Result<()> {
    // A checkout whose directory is gone is dead even when git still lists it,
    // because nothing can be running there. `--prune` clears the registration.
    // A directory that is left but is no longer a worktree is dead for the same
    // reason: nothing can run there.
    let repo_key = repo.key();
    let live: Vec<PathBuf> = repo
        .worktrees()?
        .into_iter()
        .map(|entry| entry.path)
        .filter(|path| live_checkout(path, &repo_key))
        .collect();
    gc_scoped(paths, &repo_key, &live, timeout, apply)
}

/// `gc` for every repository the state dir holds a record of.
///
/// A repository-scoped sweep needs the repository, so it cannot reach the case
/// that matters most: the repository itself is gone, while its worktrees'
/// processes, volumes and port blocks are still there. This walks the records
/// instead. A checkout is live when its recorded path is still a worktree of
/// the recorded repository, the same rule the repository-scoped sweep applies
/// to git's list; a path that exists but no longer resolves as one is a leftover
/// directory, not a stack to protect.
pub fn gc_all(paths: &Paths, timeout: Duration, apply: bool) -> Result<()> {
    for repo_key in recorded_repositories(paths) {
        let live: Vec<PathBuf> = recorded_worktrees(paths, &repo_key)
            .into_iter()
            .filter(|path| live_checkout(path, &repo_key))
            .collect();
        gc_scoped(paths, &repo_key, &live, timeout, apply)?;
    }
    Ok(())
}

/// Drop everything magictree recorded for a worktree whose checkout has just
/// been removed: the processes it left running, its state directory, and its
/// port block.
///
/// `gc` reclaims the same things from the other side, for records whose
/// checkout disappeared without a command. `rm` calls this itself so a removal
/// leaves nothing behind for a later sweep to find: without it the block stays
/// reserved machine-wide and the state directory keeps the environment and pid
/// files of a checkout that is gone.
pub fn forget(paths: &Paths, repo_key: &str, worktree_id: &str, timeout: Duration) -> Result<()> {
    let dir = paths.worktree_dir(repo_key, worktree_id);
    if dir.is_dir() {
        for (name, pid) in run::recorded(&dir) {
            if run::is_alive(pid) {
                println!("stopping {name} (pid {pid}) of worktree {worktree_id}");
            }
        }
        run::stop_all(&dir, timeout)?;
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        // An empty repository directory would otherwise be read as a repository
        // by the next `gc --all`.
        let _ = std::fs::remove_dir(paths.worktrees_dir().join(repo_key));
    }
    // Past this point the record reserves nothing: the ports are read before
    // the release, because releasing is what deletes the assignment.
    let Some(assignment) = ports::load(paths, repo_key, worktree_id)? else {
        return Ok(());
    };
    ports::release(paths, repo_key, worktree_id)?;
    let stride = Config::load(paths)?.port_stride;
    println!(
        "released block {}-{} (worktree {})",
        assignment.block_start,
        assignment.block_start.saturating_add(stride - 1),
        assignment.worktree_id
    );
    Ok(())
}

/// One repository's reclamation, given the checkouts that are still live.
fn gc_scoped(
    paths: &Paths,
    repo_key: &str,
    live: &[PathBuf],
    timeout: Duration,
    apply: bool,
) -> Result<()> {
    let blocks = paths.blocks_dir();
    let stride = Config::load(paths)?.port_stride;
    let mut released = 0usize;
    if blocks.exists() {
        for entry in std::fs::read_dir(&blocks)?.flatten() {
            let path = entry.path();
            let Some(assignment) = read_assignment(&path) else {
                continue;
            };
            // Blocks belong to a repository; leave other repositories alone.
            if assignment.repo_key != repo_key {
                continue;
            }
            let stale = match assignment.worktree_path.clone() {
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
                    assignment.block_start,
                    assignment.block_start.saturating_add(stride - 1),
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
    let projects = sweep_worktree_state(paths, repo_key, live, timeout, apply)?;
    sweep_compose(live, repo_key, &projects, apply)?;
    Ok(())
}

/// Repository keys the state dir holds blocks or worktree state for.
fn recorded_repositories(paths: &Paths) -> Vec<String> {
    let mut keys: BTreeSet<String> = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(paths.blocks_dir()) {
        for entry in entries.flatten() {
            if let Some(assignment) = read_assignment(&entry.path()) {
                keys.insert(assignment.repo_key);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(paths.worktrees_dir()) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                keys.insert(name.to_string());
            }
        }
    }
    keys.into_iter().collect()
}

/// Every checkout recorded for a repository, from its blocks and its state dirs.
fn recorded_worktrees(paths: &Paths, repo_key: &str) -> Vec<PathBuf> {
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(paths.blocks_dir()) {
        for entry in entries.flatten() {
            let Some(assignment) = read_assignment(&entry.path()) else {
                continue;
            };
            if assignment.repo_key != repo_key {
                continue;
            }
            if let Some(path) = assignment.worktree_path {
                found.insert(PathBuf::from(path));
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(paths.worktrees_dir().join(repo_key)) {
        for entry in entries.flatten() {
            if let Some(owner) = state_owner(&entry.path()) {
                found.insert(PathBuf::from(owner));
            }
        }
    }
    found.into_iter().collect()
}

fn read_assignment(path: &Path) -> Option<Assignment> {
    if path.extension().and_then(|value| value.to_str()) != Some("json") {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Assignment>(&raw) {
        Ok(assignment) => Some(assignment),
        Err(_) => {
            // A corrupt block must not read as "no ports reserved": another
            // worktree would claim them while the owner still holds them.
            eprintln!("warning: ignoring unreadable port block {}", path.display());
            None
        }
    }
}

/// Stop the supervised processes of worktrees whose checkout is gone, then drop
/// their state directories. Returns the compose project names those worktrees
/// recorded, which is the only way to reach a project whose containers are
/// already gone.
///
/// This is the half of `gc` that cannot be done from the checkout: the checkout
/// has been deleted, so the state dir is the only surviving record of what was
/// running. State written before runtime state moved into the state dir died
/// with its checkout and cannot be reclaimed, only reported as gone.
fn sweep_worktree_state(
    paths: &Paths,
    repo_key: &str,
    live: &[PathBuf],
    timeout: Duration,
    apply: bool,
) -> Result<Vec<String>> {
    let root = paths.worktrees_dir().join(repo_key);
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
    // An empty repository directory would otherwise be read as a repository by
    // the next `gc --all`. `remove_dir` refuses a non-empty one, so anything
    // skipped above keeps its directory.
    if apply {
        let _ = std::fs::remove_dir(&root);
    }
    Ok(projects)
}

/// The checkout a state directory describes, from its own `ports.json`.
fn state_owner(dir: &Path) -> Option<String> {
    read_state_assignment(dir)?.worktree_path
}

/// A state directory's `ports.json` mirror. Unlike a block, this file reserves
/// nothing, so an unreadable one is skipped in silence: `gc` and `list --all`
/// both treat it as state they cannot attribute rather than as an error.
fn read_state_assignment(dir: &Path) -> Option<Assignment> {
    let raw = std::fs::read_to_string(dir.join("ports.json")).ok()?;
    serde_json::from_str(&raw).ok()
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
fn sweep_compose(live: &[PathBuf], repo_key: &str, recorded: &[String], apply: bool) -> Result<()> {
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

/// Run a docker command, returning None when docker cannot be consulted. Both
/// failure shapes are reported, so a sweep never presents a clean result that
/// is really "docker is broken".
fn docker_ids(args: &[&str]) -> Result<Option<String>> {
    let output = match Command::new("docker").args(args).output() {
        Ok(output) => output,
        Err(_) => {
            eprintln!("warning: docker is not available; compose checks were skipped");
            return Ok(None);
        }
    };
    if !output.status.success() {
        eprintln!(
            "warning: docker {} failed ({}); compose checks were skipped",
            args.first().copied().unwrap_or("command"),
            String::from_utf8_lossy(&output.stderr).trim()
        );
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
/// worktree `rm` can remove: `remove` refuses the current worktree, and it has
/// no administrative directory to name it by. Listing it advertised a row that
/// `rm` always rejected.
pub fn worktree_rows(repo: &Repo, paths: &Paths) -> Result<Vec<(String, PathBuf, String)>> {
    let mut rows = Vec::new();
    for entry in repo.worktrees()? {
        let root = canonical_or_self(&entry.path);
        // A linked worktree has an administrative directory, and the identity
        // derived from its name is what both `list` and `rm` resolve against.
        // The primary checkout has none, so it is skipped.
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

/// One worktree the state dir holds a record of, for `list --all`.
pub struct RecordedWorktree {
    /// Repository the record belongs to, as its state dir is keyed.
    pub repo_key: String,
    /// The repository's primary checkout, when a record names one. The key is
    /// a hash, so this is what tells two repositories apart.
    pub repo_path: Option<PathBuf>,
    /// Identity git assigned the worktree; `main` is the primary checkout.
    pub worktree_id: String,
    /// The recorded checkout path.
    pub path: Option<PathBuf>,
    /// Ports the record still reserves. Empty once `ports --release` dropped
    /// the block, whatever the state directory's stale mirror still says.
    pub ports: String,
    /// Whether the recorded path is still a worktree of the recorded
    /// repository. `None` when no record ever named a path, which is also what
    /// `gc` leaves alone.
    pub live: Option<bool>,
}

/// Rows for `list --all`: every worktree the state dir holds a record of, in
/// every repository, whether or not its checkout still exists.
///
/// A worktree is recorded on two shapes: the port block that reserves its
/// ports, and its state directory. Neither implies the other — `ports
/// --release` drops the block and keeps the state, and a checkout removed
/// outside magictree keeps both until `gc` — so the union is what magictree
/// watched, rather than what is running.
pub fn all_worktree_rows(paths: &Paths) -> Vec<RecordedWorktree> {
    let mut records: BTreeMap<(String, String), RecordedWorktree> = BTreeMap::new();

    if let Ok(entries) = std::fs::read_dir(paths.blocks_dir()) {
        for entry in entries.flatten() {
            let Some(assignment) = read_assignment(&entry.path()) else {
                continue;
            };
            let key = (assignment.repo_key.clone(), assignment.worktree_id.clone());
            let record = records.entry(key).or_insert_with(|| RecordedWorktree {
                repo_key: assignment.repo_key.clone(),
                repo_path: None,
                worktree_id: assignment.worktree_id.clone(),
                path: None,
                ports: String::new(),
                live: None,
            });
            if record.path.is_none() {
                record.path = assignment.worktree_path.clone().map(PathBuf::from);
            }
            record.ports = describe_ports(&assignment);
        }
    }

    // A state directory is the only record left once its block is released, and
    // the only one that names a checkout whose block was written before paths
    // were recorded.
    if let Ok(repositories) = std::fs::read_dir(paths.worktrees_dir()) {
        for repository in repositories.flatten() {
            let Ok(worktrees) = std::fs::read_dir(repository.path()) else {
                continue;
            };
            for worktree in worktrees.flatten() {
                let dir = worktree.path();
                if !dir.is_dir() {
                    continue;
                }
                // Unreadable state cannot be attributed to a checkout, so it is
                // skipped here for the same reason `gc` refuses to guess.
                let Some(assignment) = read_state_assignment(&dir) else {
                    continue;
                };
                let Some(owner) = assignment.worktree_path.clone() else {
                    continue;
                };
                let key = (assignment.repo_key.clone(), assignment.worktree_id.clone());
                let record = records.entry(key).or_insert_with(|| RecordedWorktree {
                    repo_key: assignment.repo_key.clone(),
                    repo_path: None,
                    worktree_id: assignment.worktree_id.clone(),
                    path: None,
                    ports: String::new(),
                    live: None,
                });
                if record.path.is_none() {
                    record.path = Some(PathBuf::from(owner));
                }
            }
        }
    }

    let mut rows: Vec<RecordedWorktree> = records.into_values().collect();
    let mut roots: BTreeMap<String, PathBuf> = BTreeMap::new();
    for row in rows.iter_mut() {
        let Some(path) = row.path.as_deref() else {
            continue;
        };
        match checkout_state(path, &row.repo_key) {
            Checkout::Live(repo) => {
                row.live = Some(true);
                // The primary checkout names the repository, but only a
                // checkout that was itself used has a record: a linked worktree
                // that was upped first still names the repository through git.
                if !roots.contains_key(&row.repo_key) {
                    roots.insert(row.repo_key.clone(), repo.main_worktree_root());
                }
            }
            Checkout::Gone => row.live = Some(false),
            Checkout::Unknown => row.live = None,
        }
    }
    // A record of the primary checkout is authoritative over one derived from a
    // linked worktree.
    for row in &rows {
        if row.worktree_id == crate::repo::PRIMARY_WORKTREE_ID {
            if let Some(path) = &row.path {
                roots.insert(row.repo_key.clone(), path.clone());
            }
        }
    }
    for row in &mut rows {
        row.repo_path = roots.get(&row.repo_key).cloned();
    }
    rows
}

/// The worktree of `repo` a `rm` target names: a path on disk, a branch, a
/// directory name, or the identity `list` prints.
///
/// `None` means this repository has no such worktree; a git that cannot answer
/// at all is an error, so `rm` never mistakes a broken git for a missing
/// worktree and looks somewhere else.
pub fn find_worktree(repo: &Repo, target: &str) -> Result<Option<PathBuf>> {
    let direct = PathBuf::from(target);
    if direct.is_dir() {
        return Ok(Some(direct.canonicalize().unwrap_or(direct)));
    }
    for entry in repo.worktrees()? {
        let id = admin_id(&repo.common_dir, &canonical_or_self(&entry.path));
        let matches = entry.branch.as_deref() == Some(target)
            || entry
                .path
                .file_name()
                .map(|name| name.to_string_lossy() == target)
                .unwrap_or(false)
            || id.as_deref() == Some(target);
        if matches {
            return Ok(Some(entry.path));
        }
    }
    Ok(None)
}

/// The records a `rm` target names, wherever they live: the identity `list`
/// prints, a directory name, or the path of the checkout.
pub fn recorded_matches(paths: &Paths, target: &str) -> Vec<RecordedWorktree> {
    let named = Path::new(target);
    all_worktree_rows(paths)
        .into_iter()
        .filter(|row| {
            if row.worktree_id == target {
                return true;
            }
            match row.path.as_deref() {
                // The same checkout spelled with and without a symlink (`/var`
                // and `/private/var` on macOS) is still the same checkout.
                Some(path) => {
                    same_worktree(path, named)
                        || path
                            .file_name()
                            .map(|name| name.to_string_lossy() == target)
                            .unwrap_or(false)
                }
                None => false,
            }
        })
        .collect()
}

/// Find the identity of the worktree at `worktree`, from the administrative
/// directory git assigned it.
///
/// The `gitdir` file may hold either form: an absolute path, or a relative one,
/// which git writes with `worktree.useRelativePaths` and resolves against the
/// administrative directory that holds the file — not against the directory the
/// reader happens to be running in. Read the second way, the pointer resolves
/// somewhere that is not the worktree, no administrative directory is attributed
/// to it, and `list` drops every linked worktree instead of naming it.
fn admin_id(common_dir: &Path, worktree: &Path) -> Option<String> {
    let dir = common_dir.join("worktrees");
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let gitdir_file = entry.path().join("gitdir");
        let Ok(raw) = std::fs::read_to_string(&gitdir_file) else {
            continue;
        };
        let pointer = Path::new(raw.trim());
        let pointer = if pointer.is_absolute() {
            pointer.to_path_buf()
        } else {
            entry.path().join(pointer)
        };
        if pointer.parent().map(canonical_or_self).as_deref() == Some(worktree) {
            return entry
                .file_name()
                .to_str()
                .map(|name| linked_worktree_id(name, &entry.path()));
        }
    }
    None
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
