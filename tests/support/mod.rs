//! Shared helpers for integration tests.
//!
//! Not every test binary uses every helper.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub struct Fixture {
    pub dir: tempfile::TempDir,
    /// State lives outside the repository, as on a real machine, so it can
    /// never show up as an untracked change.
    state: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("temp dir"),
            state: tempfile::tempdir().expect("temp state dir"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn join(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, contents).expect("write fixture file");
    }

    pub fn mkdir(&self, relative: &str) {
        std::fs::create_dir_all(self.join(relative)).expect("create dir");
    }

    /// Initialise a git repository with one commit so worktree commands work.
    pub fn git_repo(&self) {
        self.git(&["init", "-q"]);
        self.git(&["add", "-A"]);
        self.git(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-qm",
            "init",
        ]);
    }

    pub fn git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(args)
            .output()
            .expect("run git")
    }

    /// A state directory isolated from the developer's real one, and from the
    /// repository under test.
    pub fn state_dir(&self) -> PathBuf {
        self.state.path().to_path_buf()
    }
}

/// Every per-worktree runtime directory under a state dir, sorted.
///
/// Runtime state is keyed by repository and worktree rather than living in the
/// checkout, so tests assert on the layout through this rather than guessing
/// hashed directory names.
pub fn runtime_dirs(state: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(repositories) = std::fs::read_dir(state.join("worktrees")) else {
        return found;
    };
    for repository in repositories.flatten() {
        let Ok(worktrees) = std::fs::read_dir(repository.path()) else {
            continue;
        };
        for worktree in worktrees.flatten() {
            if worktree.path().is_dir() {
                found.push(worktree.path());
            }
        }
    }
    found.sort();
    found
}

/// The binary under test, built by cargo for this test run.
pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_magictree"))
}

pub struct Run {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Run the binary with an isolated state directory.
pub fn run(args: &[&str], cwd: &Path, state: &Path) -> Run {
    finish(command(args, cwd, state).output().expect("run magictree"))
}

/// The same run, with the update check left on.
///
/// `run` turns the check off so that no test reaches GitHub. A test that seeds
/// the check's cache can leave it on and stay offline, which is what this is
/// for: it is the only way to see the notice a command prints.
pub fn run_notifying(args: &[&str], cwd: &Path, state: &Path) -> Run {
    finish(
        command(args, cwd, state)
            .env_remove("MAGICTREE_NO_UPDATE_CHECK")
            .output()
            .expect("run magictree"),
    )
}

fn command(args: &[&str], cwd: &Path, state: &Path) -> Command {
    let mut command = Command::new(bin());
    command
        .args(args)
        .current_dir(cwd)
        .env("MAGICTREE_STATE_DIR", state)
        .env("MAGICTREE_CONFIG_DIR", state.join("config"))
        // A test must never ask GitHub for a release: the check is off unless
        // a test asks for it, and then it seeds the cache first.
        .env("MAGICTREE_NO_UPDATE_CHECK", "1");
    command
}

fn finish(output: Output) -> Run {
    Run {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

/// True when a program is runnable, used to skip tests that need it.
pub fn have(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// A port that was free a moment ago. Allocation tests use this to make a
/// deliberate conflict rather than hard-coding a number.
pub fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.local_addr().expect("addr").port()
}

/// Occupy a port until the returned guard is dropped.
///
/// Test binaries each own a separate state directory, so their allocators are
/// independent and can choose the same port. If the port is already taken the
/// premise of the test (that it is unavailable) still holds, so that is not
/// an error.
pub struct PortGuard {
    listener: Option<std::net::TcpListener>,
    port: u16,
}

impl PortGuard {
    /// Occupy `port` on loopback; `0` asks the OS for any free port.
    pub fn occupy(port: u16) -> Self {
        Self::occupy_on(("127.0.0.1", port), port)
    }

    /// Occupy `port` the way a dev server does, on the wildcard rather than
    /// loopback, the shape that has to be probed to be seen.
    pub fn occupy_wildcard(port: u16) -> Self {
        Self::occupy_on(("::", port), port)
    }

    fn occupy_on<A: std::net::ToSocketAddrs>(address: A, fallback: u16) -> Self {
        match std::net::TcpListener::bind(address) {
            Ok(listener) => {
                let actual = listener
                    .local_addr()
                    .map(|local| local.port())
                    .unwrap_or(fallback);
                Self {
                    listener: Some(listener),
                    port: actual,
                }
            }
            Err(_) => Self {
                listener: None,
                port: fallback,
            },
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn held(&self) -> bool {
        self.listener.is_some()
    }
}
