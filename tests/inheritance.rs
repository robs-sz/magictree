//! Manifest inheritance: a committed manifest reaches every worktree, an
//! uncommitted one does not, and magictree says which case you are in.

mod support;

use std::time::Duration;
use support::{run, Fixture};

const MANIFEST: &str = r#"
version = 1

[[services]]
id = "web"
command = "python3 -m http.server $PORT --bind 127.0.0.1"
port = { env = "PORT" }
"#;

fn repo_with_manifest(committed: bool) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("package.json", r#"{"name":"app"}"#);
    fixture.git_repo();
    fixture.write("magictree.toml", MANIFEST);
    if committed {
        fixture.git(&["add", "magictree.toml"]);
        fixture.git(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-qm",
            "add manifest",
        ]);
    }
    fixture
}

#[test]
fn a_committed_manifest_is_inherited_and_starts_in_the_new_worktree() {
    let fixture = repo_with_manifest(true);
    let state = fixture.state_dir();

    let new = run(&["new", "feat/inherit", "--no-up"], fixture.path(), &state);
    assert!(new.ok(), "{}", new.combined());

    let worktree = fixture.join("..").canonicalize().expect("parent");
    let worktree = worktree.join(format!(
        "{}-feat-inherit",
        fixture.path().file_name().unwrap().to_string_lossy()
    ));
    assert!(
        worktree.join("magictree.toml").is_file(),
        "a committed manifest is checked out with the worktree"
    );

    // The whole point: the stack can be started there without any extra setup.
    let dry = run(&["--dry-run", "up"], &worktree, &state);
    assert!(dry.ok(), "{}", dry.combined());
    assert!(dry.stdout.contains("http.server"), "{}", dry.stdout);
}

#[test]
fn new_refuses_before_creating_when_the_manifest_is_uncommitted() {
    let fixture = repo_with_manifest(false);
    let state = fixture.state_dir();

    let new = run(&["new", "feat/uncommitted"], fixture.path(), &state);
    assert!(!new.ok());
    assert!(
        new.stderr.contains("uncommitted"),
        "must explain the cause: {}",
        new.stderr
    );
    // The command is given with `git -C <main>` so it works from anywhere.
    assert!(
        new.stderr.contains("add magictree.toml") && new.stderr.contains("commit"),
        "must give the command that fixes it: {}",
        new.stderr
    );

    // Nothing should be left half-created, or a retry fails on an existing path.
    let parent = fixture.join("..").canonicalize().expect("parent");
    let target = parent.join(format!(
        "{}-feat-uncommitted",
        fixture.path().file_name().unwrap().to_string_lossy()
    ));
    assert!(
        !target.exists(),
        "a refused new must not leave a worktree behind at {}",
        target.display()
    );

    // And a retry after committing works.
    fixture.git(&["add", "magictree.toml"]);
    fixture.git(&[
        "-c",
        "user.email=test@example.com",
        "-c",
        "user.name=test",
        "commit",
        "-qm",
        "add manifest",
    ]);
    let retry = run(
        &["new", "feat/uncommitted", "--no-up"],
        fixture.path(),
        &state,
    );
    assert!(retry.ok(), "{}", retry.combined());
}

#[test]
fn new_without_up_still_creates_a_worktree_when_nothing_will_be_started() {
    let fixture = repo_with_manifest(false);
    let state = fixture.state_dir();

    let new = run(&["new", "feat/bare", "--no-up"], fixture.path(), &state);
    assert!(new.ok(), "{}", new.combined());
    assert!(new.stdout.contains("stack not started"), "{}", new.stdout);
}

#[test]
fn doctor_reports_an_uncommitted_manifest_exactly_once() {
    let fixture = repo_with_manifest(false);
    let state = fixture.state_dir();

    let doctor = run(&["doctor"], fixture.path(), &state);
    assert!(!doctor.ok(), "an uncommitted manifest is drift");
    let occurrences = doctor.stdout.matches("is not committed").count();
    assert_eq!(
        occurrences, 1,
        "a single-app repo must not report the same file twice:\n{}",
        doctor.stdout
    );
    assert!(
        doctor.stdout.contains("git add magictree.toml"),
        "{}",
        doctor.stdout
    );

    // Committing clears it.
    fixture.git(&["add", "magictree.toml"]);
    fixture.git(&[
        "-c",
        "user.email=test@example.com",
        "-c",
        "user.name=test",
        "commit",
        "-qm",
        "add manifest",
    ]);
    let clean = run(&["doctor"], fixture.path(), &state);
    assert!(clean.ok(), "{}", clean.combined());
}

#[test]
fn up_in_a_worktree_whose_branch_lacks_the_manifest_explains_why() {
    let fixture = repo_with_manifest(true);
    let state = fixture.state_dir();

    assert!(run(&["new", "feat/gone", "--no-up"], fixture.path(), &state).ok());
    let parent = fixture.join("..").canonicalize().expect("parent");
    let worktree = parent.join(format!(
        "{}-feat-gone",
        fixture.path().file_name().unwrap().to_string_lossy()
    ));

    // Simulate a branch that never had the manifest.
    std::fs::remove_file(worktree.join("magictree.toml")).expect("remove manifest");

    let up = run(&["up"], &worktree, &state);
    assert!(!up.ok());
    assert!(
        up.stderr.contains("this branch does not"),
        "the error must distinguish a missing branch file from an uncommitted one: {}",
        up.stderr
    );
    assert!(
        up.stderr.contains("cp ") && up.stderr.contains("magictree.toml"),
        "must offer a way forward: {}",
        up.stderr
    );
}

#[test]
fn worktrees_get_independent_ports_from_the_same_manifest() {
    if !support::have("python3", &["-c", "pass"]) {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let fixture = repo_with_manifest(true);
    let state = fixture.state_dir();
    let parent = fixture.join("..").canonicalize().expect("parent");
    let name = fixture
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();

    assert!(run(&["new", "feat/one", "--no-up"], fixture.path(), &state).ok());
    assert!(run(&["new", "feat/two", "--no-up"], fixture.path(), &state).ok());

    let mut ports = Vec::new();
    for suffix in ["feat-one", "feat-two"] {
        let worktree = parent.join(format!("{name}-{suffix}"));
        let up = run(&["up"], &worktree, &state);
        assert!(up.ok(), "{}", up.combined());
        let listing = run(&["ports"], &worktree, &state);
        let port: u16 = listing
            .stdout
            .lines()
            .find(|line| line.contains("web"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .expect("an assigned port");
        ports.push(port);
    }

    assert_ne!(
        ports[0], ports[1],
        "two worktrees of one repository must not share a port"
    );

    for suffix in ["feat-one", "feat-two"] {
        let worktree = parent.join(format!("{name}-{suffix}"));
        let down = run(&["down"], &worktree, &state);
        assert!(down.ok(), "{}", down.combined());
    }
    let _ = Duration::from_secs(1);
}
