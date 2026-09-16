//! Worktree lifecycle and `gc` reclamation, against real git repositories.

mod support;

use magictree::config::Config;
use magictree::paths::Paths;
use magictree::ports::{self, PortRequest};
use magictree::repo::Repo;
use magictree::run;
use magictree::worktrees;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use support::Fixture;

/// Any timeout works: every service here is a `sleep` that dies on SIGTERM.
const STOP_TIMEOUT: Duration = Duration::from_secs(1);

fn paths(fixture: &Fixture) -> Paths {
    let state = fixture.state_dir();
    Paths {
        state_dir: state.clone(),
        config_dir: state,
    }
}

fn request(name: &str) -> PortRequest {
    PortRequest {
        name: name.to_string(),
        prefer: None,
        require: None,
    }
}

/// A repository with one worktree, both holding port assignments.
fn repo_with_wt(name: &str) -> (Fixture, std::path::PathBuf, std::path::PathBuf) {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 300\"\nport = { env = \"PORT\" }\n",
    );
    fixture.git_repo();

    let path = fixture.git(&["worktree", "add", "-q", name, "-b", name]);
    assert!(path.status.success(), "git worktree add");
    let worktree = fixture.join(name);

    let paths = paths(&fixture);
    let config = Config::default();
    let main = Repo::open(fixture.path()).expect("main repo");
    ports::ensure(
        &paths,
        &config,
        &main.key(),
        &main.worktree_id(),
        &main.worktree_root,
        &[request("web")],
        None,
    )
    .expect("main assignment");

    let linked = Repo::open(&worktree).expect("linked repo");
    ports::ensure(
        &paths,
        &config,
        &linked.key(),
        &linked.worktree_id(),
        &linked.worktree_root,
        &[request("web")],
        None,
    )
    .expect("worktree assignment");

    (fixture, worktree, paths.state_dir.clone())
}

fn block_count(state: &std::path::Path) -> usize {
    std::fs::read_dir(state.join("blocks"))
        .map(|entries| entries.flatten().count())
        .unwrap_or(0)
}

#[test]
fn gc_keeps_blocks_for_worktrees_that_still_exist() {
    let (fixture, _worktree, state) = repo_with_wt("keeper");
    let repo = Repo::open(fixture.path()).expect("repo");

    assert_eq!(block_count(&state), 2);
    worktrees::gc(&paths(&fixture), &repo, STOP_TIMEOUT, true).expect("gc");
    assert_eq!(block_count(&state), 2, "live worktrees keep their ports");

    let main_repo = Repo::open(fixture.path()).expect("repo");
    let main_id = main_repo.worktree_id();
    assert!(
        ports::load(&paths(&fixture), &main_repo.key(), &main_id)
            .expect("load")
            .is_some(),
        "the primary checkout keeps its assignment"
    );
}

#[test]
fn gc_dry_run_reports_without_releasing() {
    let (fixture, worktree, state) = repo_with_wt("ghost");
    std::fs::remove_dir_all(&worktree).expect("delete checkout behind git's back");
    let repo = Repo::open(fixture.path()).expect("repo");

    worktrees::gc(&paths(&fixture), &repo, STOP_TIMEOUT, false).expect("dry run");
    assert_eq!(block_count(&state), 2, "a dry run releases nothing");

    worktrees::gc(&paths(&fixture), &repo, STOP_TIMEOUT, true).expect("apply");
    assert_eq!(
        block_count(&state),
        1,
        "the deleted worktree's block is reclaimed"
    );

    // The checkout is gone, so git's registration is stale too.
    worktrees::prune(&repo, false).expect("prune");
    let listed = String::from_utf8_lossy(&fixture.git(&["worktree", "list"]).stdout).to_string();
    assert!(
        !listed.contains("ghost"),
        "prune drops the registration of a deleted checkout: {listed}"
    );
}

#[test]
fn gc_is_idempotent() {
    let (fixture, worktree, state) = repo_with_wt("ghost");
    std::fs::remove_dir_all(&worktree).expect("delete checkout");
    let repo = Repo::open(fixture.path()).expect("repo");

    worktrees::gc(&paths(&fixture), &repo, STOP_TIMEOUT, true).expect("first");
    worktrees::gc(&paths(&fixture), &repo, STOP_TIMEOUT, true).expect("second");
    assert_eq!(block_count(&state), 1);
}

/// Runtime state for a worktree, as `up` leaves it: an owner record plus a
/// supervised process. Returns the state directory and the process id.
fn runtime_with_process(paths: &Paths, linked: &Repo, fixture: &Fixture) -> (PathBuf, i32) {
    let runtime = paths.worktree_dir(&linked.key(), &linked.worktree_id());
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let assignment = ports::ensure(
        paths,
        &Config::default(),
        &linked.key(),
        &linked.worktree_id(),
        &linked.worktree_root,
        &[request("web")],
        None,
    )
    .expect("assignment");
    std::fs::write(
        runtime.join("ports.json"),
        serde_json::to_string(&assignment).expect("json"),
    )
    .expect("ports.json");
    let pid = run::start(
        &runtime,
        "web",
        "sleep 300",
        fixture.path(),
        &BTreeMap::new(),
    )
    .expect("start");
    (runtime, pid)
}

#[test]
fn gc_all_reclaims_a_repository_that_is_gone() {
    // A repository-scoped sweep needs the repository. This is the case it cannot
    // reach: the repository itself has been deleted, and the records are all
    // that is left of it.
    let (fixture, worktree, state) = repo_with_wt("ghost");
    let paths = paths(&fixture);
    let linked = Repo::open(&worktree).expect("linked repo");
    let repo_key = linked.key();
    let (runtime, pid) = runtime_with_process(&paths, &linked, &fixture);
    assert!(run::is_alive(pid));

    std::fs::remove_dir_all(&worktree).expect("delete checkout");
    worktrees::gc_all(&paths, STOP_TIMEOUT, true).expect("gc --all");

    assert!(!run::is_alive(pid), "the orphaned process is stopped");
    assert!(!runtime.exists(), "its state is dropped");
    assert_eq!(block_count(&state), 1, "its block is released");
    assert!(
        !paths.worktrees_dir().join(&repo_key).exists(),
        "the emptied repository directory is dropped too"
    );
}

#[test]
fn gc_all_keeps_the_block_of_a_checkout_that_still_exists() {
    // The safety half: sweeping without a repository must not treat a live
    // checkout as a leftover, or it would reclaim the port of a running stack.
    let (fixture, worktree, state) = repo_with_wt("kept");
    let paths = paths(&fixture);

    worktrees::gc_all(&paths, STOP_TIMEOUT, true).expect("gc --all");

    assert_eq!(
        block_count(&state),
        2,
        "both the primary checkout and the linked worktree are still live"
    );
    assert!(worktree.exists());
}

#[test]
fn gc_stops_host_processes_whose_checkout_is_gone() {
    // Nothing can read a deleted checkout's pid files, so the state dir is the
    // only surviving record that a process is still running. This is what a
    // worktree removed outside magictree leaves behind.
    let (fixture, worktree, state) = repo_with_wt("ghost");
    let paths = paths(&fixture);
    let repo = Repo::open(fixture.path()).expect("repo");
    let linked = Repo::open(&worktree).expect("linked repo");
    let (runtime, pid) = runtime_with_process(&paths, &linked, &fixture);
    assert!(run::is_alive(pid), "the fixture process is running");

    std::fs::remove_dir_all(&worktree).expect("delete checkout");

    // A dry run reports the reclamation without performing any of it: stopping a
    // process is the least reversible thing gc does.
    worktrees::gc(&paths, &repo, STOP_TIMEOUT, false).expect("dry run");
    assert!(run::is_alive(pid), "a dry run must not stop the process");
    assert!(runtime.exists(), "a dry run must not drop the state");
    assert_eq!(
        block_count(&state),
        2,
        "a dry run must not release the block"
    );

    worktrees::gc(&paths, &repo, STOP_TIMEOUT, true).expect("gc");

    assert!(!run::is_alive(pid), "the orphaned process is stopped");
    assert!(!runtime.exists(), "and its state is dropped");
    assert_eq!(block_count(&state), 1, "its port block is released too");
}

#[test]
fn gc_keeps_state_it_cannot_attribute() {
    // An unattributable directory must not be treated as somebody's leftovers:
    // dropping it would be a guess about which checkout it described.
    let (fixture, _worktree, state) = repo_with_wt("ghost");
    let paths = paths(&fixture);
    let repo = Repo::open(fixture.path()).expect("repo");
    let stray = paths
        .state_dir
        .join("worktrees")
        .join(repo.key())
        .join("mystery");
    std::fs::create_dir_all(&stray).expect("stray dir");

    worktrees::gc(&paths, &repo, STOP_TIMEOUT, true).expect("gc");
    assert!(stray.exists(), "state with no recorded owner is left alone");
    let _ = state;
}

#[test]
fn the_generated_override_labels_the_repository_that_owns_it() {
    // gc sweeps by this label, so a run in one repository cannot collect
    // another repository's worktree containers.
    use magictree::compose::{write_override, ComposeGroup, GroupService, PortMapping};
    use magictree::manifest::Expose;

    let fixture = Fixture::new();
    let runtime = fixture.join("runtime");
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    let group = ComposeGroup {
        file: fixture.join("compose.yaml"),
        project: "wt".to_string(),
        worktree_path: "/tmp/some-worktree".to_string(),
        repo_key: "deadbeef".to_string(),
        services: vec![GroupService {
            name: "api".to_string(),
            expose: Expose::Port,
            mappings: vec![PortMapping {
                host: Some(25000),
                target: Some(8000),
            }],
        }],
    };
    let path = write_override(&runtime, &group).expect("write override");
    let contents = std::fs::read_to_string(path).expect("read override");

    assert!(
        contents.contains("magictree.repo: \"deadbeef\""),
        "{contents}"
    );
    assert!(
        contents.contains("magictree.worktree: \"/tmp/some-worktree\""),
        "{contents}"
    );
    assert!(
        contents.contains("127.0.0.1:25000:8000"),
        "the allocated port is published on loopback: {contents}"
    );
}

#[test]
fn gc_leaves_other_repositories_alone() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    fixture.write("magictree.toml", "version = 1\n");
    fixture.git_repo();

    ports::ensure(
        &paths,
        &config,
        "another-repo",
        "some-worktree",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("foreign assignment");

    let repo = Repo::open(fixture.path()).expect("repo");
    worktrees::gc(&paths, &repo, STOP_TIMEOUT, true).expect("gc");

    assert_eq!(
        block_count(&paths.state_dir),
        1,
        "blocks belonging to another repository must survive"
    );
}

#[test]
fn listing_reports_each_linked_worktree_with_its_ports() {
    let (fixture, _worktree, _state) = repo_with_wt("feature");
    let repo = Repo::open(fixture.path()).expect("repo");
    let rows = worktrees::worktree_rows(&repo, &paths(&fixture)).expect("rows");

    assert_eq!(rows.len(), 1, "only the linked worktree is listed");
    let (id, path, ports) = &rows[0];
    assert_eq!(id, "feature");
    assert!(path.ends_with("feature"));
    assert!(
        ports.contains("web="),
        "worktree {id} should report its port, got {ports:?}"
    );
}

/// Every row `list` prints must name a worktree `rm` accepts. The primary
/// checkout has no administrative directory, so it is not a removable worktree
/// and must not appear.
#[test]
fn every_listed_worktree_is_removable() {
    let (fixture, _worktree, state) = repo_with_wt("feature");
    let listed = support::run(&["list"], fixture.path(), &state);
    assert!(listed.ok(), "{}", listed.combined());

    let ids: Vec<String> = listed
        .stdout
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect();
    assert_eq!(
        ids,
        vec!["feature".to_string()],
        "list should show only the linked worktree, got:\n{}",
        listed.stdout
    );

    for id in &ids {
        let dry = support::run(&["--dry-run", "rm", id], fixture.path(), &state);
        assert!(
            dry.ok(),
            "list advertised '{id}' but rm refuses it: {}",
            dry.combined()
        );
    }
}

#[test]
fn worktree_paths_are_recorded_for_later_reclamation() {
    let (fixture, worktree, _state) = repo_with_wt("recorded");
    let linked = Repo::open(&worktree).expect("repo");
    let assignment = ports::load(&paths(&fixture), &linked.key(), &linked.worktree_id())
        .expect("load")
        .expect("assignment");

    let recorded = assignment
        .worktree_path
        .expect("the owning path is recorded so gc can match it");
    assert!(
        recorded.ends_with("recorded"),
        "recorded path {recorded} should point at the worktree"
    );
}

#[test]
fn remove_refuses_a_dirty_worktree_unless_forced() {
    let (fixture, worktree, _state) = repo_with_wt("dirty");
    std::fs::write(worktree.join("untracked.txt"), "x").expect("dirty the worktree");
    let repo = Repo::open(fixture.path()).expect("repo");

    let error = worktrees::remove(&repo, &worktree, false).expect_err("must refuse");
    assert!(error.to_string().contains("--force"), "{error}");
    assert!(
        worktree.exists(),
        "a refused removal leaves the worktree alone"
    );

    worktrees::remove(&repo, &worktree, true).expect("forced removal");
    assert!(!worktree.exists());
    let branches = fixture.git(&["branch", "--list", "dirty"]);
    assert!(
        String::from_utf8_lossy(&branches.stdout).contains("dirty"),
        "removing a worktree never deletes the branch"
    );
}

#[test]
fn remove_refuses_the_current_worktree() {
    let fixture = Fixture::new();
    fixture.write("magictree.toml", "version = 1\n");
    fixture.git_repo();
    let repo = Repo::open(fixture.path()).expect("repo");

    let error = worktrees::remove(&repo, fixture.path(), true).expect_err("must refuse");
    assert!(error.to_string().contains("current worktree"), "{error}");
}

/// Git names a worktree's administrative directory after the checkout's
/// basename, so a checkout at `.../main` is registered as `main`, the identity
/// the primary checkout reserves. Left indistinguishable, the two would resolve
/// to one port block, one runtime directory and one compose project.
#[test]
fn a_worktree_named_main_does_not_share_the_primary_block() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 300\"\nport = { env = \"PORT\" }\n",
    );
    fixture.git_repo();

    let status = fixture.git(&["worktree", "add", "-q", "main", "-b", "collided"]);
    assert!(status.status.success(), "git worktree add: {status:?}");
    let worktree = fixture.join("main");

    let paths = paths(&fixture);
    let config = Config::default();
    let primary = Repo::open(fixture.path()).expect("primary repo");
    let linked = Repo::open(&worktree).expect("linked repo");
    assert_ne!(
        linked.worktree_id(),
        primary.worktree_id(),
        "a linked worktree named main must not claim the primary's identity"
    );

    let primary_block = ports::ensure(
        &paths,
        &config,
        &primary.key(),
        &primary.worktree_id(),
        &primary.worktree_root,
        &[request("web")],
        None,
    )
    .expect("primary assignment");
    let linked_block = ports::ensure(
        &paths,
        &config,
        &linked.key(),
        &linked.worktree_id(),
        &linked.worktree_root,
        &[request("web")],
        None,
    )
    .expect("linked assignment");

    assert_ne!(primary_block.base, linked_block.base);
    assert_eq!(
        block_count(&paths.state_dir),
        2,
        "each worktree owns a block"
    );

    let rows = worktrees::worktree_rows(&primary, &paths).expect("rows");
    assert_eq!(rows.len(), 1, "only the linked worktree is listed");
    assert_eq!(
        rows[0].0,
        linked.worktree_id(),
        "list must resolve the same identity the block was written under"
    );
    assert_ne!(rows[0].2, "-", "list must find the linked worktree's ports");
}

#[test]
fn a_detached_worktree_is_created_at_the_requested_path() {
    let fixture = Fixture::new();
    fixture.write("magictree.toml", "version = 1\n");
    fixture.git_repo();
    let repo = Repo::open(fixture.path()).expect("repo");

    let target = fixture.join("detached");
    let path = worktrees::create(&repo, "scratch", None, Some(&target), true).expect("create");

    assert_eq!(path, target);
    // The order this pins: git receives `add --detach <path> <commit-ish>`.
    // The reversed order made git treat the revision as the directory, so
    // every detached create failed after its pre-flight checks.
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .expect("git rev-parse");
    assert!(
        head.status.success(),
        "{}",
        String::from_utf8_lossy(&head.stderr)
    );
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .expect("git symbolic-ref");
    assert!(
        !branch.status.success(),
        "a detached worktree must not sit on a branch"
    );
}
