//! `gc` against real Docker.
//!
//! Reclaiming compose resources is the one part of `gc` that cannot be checked
//! without a daemon, so these tests use one: a throwaway alpine service that owns
//! a named volume, one compose project per test.
//!
//! They are skipped unless Docker, Compose and a local `alpine:3` are all
//! available (`docker pull alpine:3`), so the suite still runs anywhere. Each
//! test cleans its project up through `Project`, which force-removes the
//! project's containers and volumes even when an assertion fails.

mod support;

use magictree::paths::Paths;
use magictree::repo::Repo;
use magictree::worktrees;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use support::Fixture;

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Lines of stdout from a docker command that is expected to succeed.
fn docker_lines(args: &[&str]) -> Vec<String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("run docker");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Everything one compose project created, removed however the test ended.
struct Project(String);

impl Project {
    fn project_filter(&self) -> String {
        format!("label=com.docker.compose.project={}", self.0)
    }

    fn containers(&self) -> Vec<String> {
        docker_lines(&["ps", "-a", "-q", "--filter", &self.project_filter()])
    }

    fn volumes(&self) -> Vec<String> {
        docker_lines(&["volume", "ls", "-q", "--filter", &self.project_filter()])
    }

    /// `docker <prefix> <ids...>`, skipped when there is nothing to act on.
    fn force(&self, prefix: &[&str], ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let mut args = prefix.to_vec();
        args.extend(ids.iter().map(String::as_str));
        let _ = Command::new("docker").args(&args).output();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        self.force(&["rm", "-f"], &self.containers());
        self.force(&["volume", "rm", "-f"], &self.volumes());
        let _ = Command::new("docker")
            .args(["network", "rm", &format!("{}_default", self.0)])
            .output();
    }
}

fn docker_ready() -> bool {
    support::have("docker", &["compose", "version"])
        && support::have("docker", &["image", "inspect", "alpine:3"])
}

/// The published host ports of a container.
fn published_ports(container: &str) -> Vec<String> {
    docker_lines(&["port", container])
}

fn paths(fixture: &Fixture) -> Paths {
    let state = fixture.state_dir();
    Paths {
        state_dir: state.clone(),
        config_dir: state,
    }
}

/// A repository with one worktree running a compose service that owns a volume.
///
/// The compose project name is derived from the worktree's directory name, so
/// each test owns a project nothing else on the machine shares.
fn compose_fixture(worktree_name: &str) -> (Fixture, PathBuf) {
    let fixture = Fixture::new();
    fixture.write(
        "compose.yaml",
        "services:\n  app:\n    image: alpine:3\n    command: sleep 300\n    volumes:\n      - data:/data\nvolumes:\n  data:\n",
    );
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[[services]]\nid = \"app\"\ncompose = { file = \"compose.yaml\", service = \"app\" }\n",
    );
    fixture.git_repo();
    let added = fixture.git(&["worktree", "add", "-q", worktree_name, "-b", worktree_name]);
    assert!(
        added.status.success(),
        "git worktree add: {}",
        String::from_utf8_lossy(&added.stderr)
    );
    let worktree = fixture.join(worktree_name);
    (fixture, worktree)
}

/// Bring the fixture's stack up and hand back the state dir it used.
fn start(fixture: &Fixture, worktree: &Path) -> Paths {
    let state = fixture.state_dir();
    let up = support::run(&["up"], worktree, &state);
    assert!(up.ok(), "{}", up.combined());
    paths(fixture)
}

#[test]
fn a_compose_service_that_publishes_a_port_starts() {
    // The other branch of the generated override: `!override` plus the allocated
    // mapping. Both branches have to produce YAML Compose accepts, and only one
    // of them used to.
    if !docker_ready() {
        return;
    }
    let fixture = Fixture::new();
    fixture.write(
        "compose.yaml",
        "services:\n  app:\n    image: alpine:3\n    command: sleep 300\n",
    );
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[[services]]\nid = \"app\"\ncompose = { file = \"compose.yaml\", service = \"app\" }\nport = { target = 8080 }\n",
    );
    fixture.git_repo();
    let added = fixture.git(&[
        "worktree",
        "add",
        "-q",
        "mtcg-published",
        "-b",
        "mtcg-published",
    ]);
    assert!(added.status.success(), "git worktree add");
    let worktree = fixture.join("mtcg-published");
    let project = Project("mtcg-published".to_string());

    let state = fixture.state_dir();
    let up = support::run(&["up"], &worktree, &state);
    assert!(up.ok(), "{}", up.combined());

    let containers = project.containers();
    assert_eq!(containers.len(), 1, "the service is up");
    let ports = published_ports(&containers[0]);
    assert!(
        ports
            .iter()
            .any(|line| line.contains("8080/tcp") && line.contains("127.0.0.1:")),
        "the allocated host port is published on loopback: {ports:?}"
    );
}

#[test]
fn gc_reclaims_a_dead_worktrees_container_and_volume() {
    if !docker_ready() {
        return;
    }
    let (fixture, worktree) = compose_fixture("mtcg-dead");
    let project = Project("mtcg-dead".to_string());
    let paths = start(&fixture, &worktree);

    assert_eq!(project.containers().len(), 1, "the service is up");
    assert_eq!(project.volumes().len(), 1, "and owns a volume");

    std::fs::remove_dir_all(&worktree).expect("delete checkout");
    let repo = Repo::open(fixture.path()).expect("repo");
    worktrees::gc(&paths, &repo, STOP_TIMEOUT, true).expect("gc");

    assert!(
        project.containers().is_empty(),
        "the container is reclaimed"
    );
    assert!(project.volumes().is_empty(), "so is its volume");
}

#[test]
fn gc_reclaims_a_volume_whose_containers_are_already_gone() {
    // A teardown done by hand (`docker compose down` without `-v`) leaves the
    // volume behind with nothing to attribute it to but the project name the
    // worktree recorded before its checkout went away.
    if !docker_ready() {
        return;
    }
    let (fixture, worktree) = compose_fixture("mtcg-orphan");
    let project = Project("mtcg-orphan".to_string());
    let paths = start(&fixture, &worktree);

    let down = Command::new("docker")
        .args(["compose", "-p", "mtcg-orphan", "down"])
        .current_dir(&worktree)
        .output()
        .expect("run docker compose down");
    assert!(down.status.success(), "docker compose down");
    assert!(project.containers().is_empty(), "the containers are gone");
    assert_eq!(project.volumes().len(), 1, "the volume is kept");

    std::fs::remove_dir_all(&worktree).expect("delete checkout");
    let repo = Repo::open(fixture.path()).expect("repo");
    worktrees::gc(&paths, &repo, STOP_TIMEOUT, true).expect("gc");

    assert!(
        project.volumes().is_empty(),
        "the recorded project name still reaches the volume"
    );
}

#[test]
fn gc_keeps_a_live_worktrees_container_and_volume() {
    // The safety half of the same rule: a worktree that still exists keeps its
    // stack, and above all its data.
    if !docker_ready() {
        return;
    }
    let (fixture, worktree) = compose_fixture("mtcg-live");
    let project = Project("mtcg-live".to_string());
    let paths = start(&fixture, &worktree);

    let repo = Repo::open(fixture.path()).expect("repo");
    worktrees::gc(&paths, &repo, STOP_TIMEOUT, true).expect("gc");

    assert_eq!(
        project.containers().len(),
        1,
        "a running stack's container survives gc"
    );
    assert_eq!(project.volumes().len(), 1, "and so does its data volume");
}
