//! `magictree activate` through the real binary: the snippet a shell evaluates
//! once, and the environment it syncs from the state-dir mirror.

mod support;

use std::process::Command;

use support::{bin, run, runtime_dirs, Fixture};

/// A host stack with one declared port variable, so the mirror carries both the
/// computed `MAGICTREE_PORT_*` name and the service's own `PORT`.
fn stack_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "idle"
command = "sleep 300"
port = { env = "PORT" }
"#,
    );
    fixture.git_repo();
    fixture
}

/// The binary with an isolated state dir, for the cases that need their own
/// environment (`$SHELL`) rather than the developer's.
fn command(fixture: &Fixture) -> Command {
    let state = fixture.state_dir();
    let mut command = Command::new(bin());
    command
        .current_dir(fixture.path())
        .env("MAGICTREE_STATE_DIR", &state)
        .env("MAGICTREE_CONFIG_DIR", state.join("config"))
        .env("MAGICTREE_NO_UPDATE_CHECK", "1");
    command
}

#[test]
fn emit_is_silent_outside_a_repository() {
    let fixture = Fixture::new();
    let state = fixture.state_dir();

    let result = run(&["activate", "--emit", "zsh"], fixture.path(), &state);

    assert!(result.ok(), "{}", result.combined());
    assert!(result.stdout.is_empty(), "{}", result.stdout);
}

#[test]
fn emit_is_silent_before_the_stack_starts() {
    let fixture = stack_fixture();
    let state = fixture.state_dir();

    let result = run(&["activate", "--emit", "zsh"], fixture.path(), &state);

    assert!(result.ok(), "{}", result.combined());
    assert!(result.stdout.is_empty(), "{}", result.stdout);
}

#[test]
fn emit_renders_the_mirror_the_stack_wrote() {
    let fixture = stack_fixture();
    let state = fixture.state_dir();
    assert!(run(&["up"], fixture.path(), &state).ok());

    // Runtime state, not a checkout file: the mirror lives under the state dir.
    let mirror = runtime_dirs(&state)[0].join("env");
    assert!(mirror.is_file(), "up writes the environment mirror");

    let posix = run(&["activate", "--emit", "zsh"], fixture.path(), &state);
    assert!(posix.ok(), "{}", posix.combined());
    assert!(
        posix.stdout.contains("export MAGICTREE_PORT_idle="),
        "{}",
        posix.stdout
    );
    assert!(
        posix.stdout.contains("export PORT="),
        "a declared port.env reaches the shell: {}",
        posix.stdout
    );

    let fish = run(&["activate", "--emit", "fish"], fixture.path(), &state);
    assert!(fish.ok(), "{}", fish.combined());
    assert!(fish.stdout.contains("set -gx PORT "), "{}", fish.stdout);
    assert!(!fish.stdout.contains("export "), "{}", fish.stdout);

    // The snippets read the keys from this comment to unset what a previous
    // worktree exported; it is a contract, not decoration.
    assert!(
        fish.stdout.contains("# magictree keys: ") && fish.stdout.contains(" PORT"),
        "{}",
        fish.stdout
    );

    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn the_snippet_hooks_both_shells_and_carries_the_binary() {
    let fixture = Fixture::new();
    let state = fixture.state_dir();

    let zsh = run(&["activate", "zsh"], fixture.path(), &state);
    assert!(zsh.ok(), "{}", zsh.combined());
    assert!(
        zsh.stdout.contains("add-zsh-hook chpwd __magictree_sync"),
        "{}",
        zsh.stdout
    );
    assert!(zsh.stdout.contains("activate --emit zsh"), "{}", zsh.stdout);
    assert!(
        zsh.stdout.contains(&bin().display().to_string()),
        "the snippet calls this binary by path: {}",
        zsh.stdout
    );

    let bash = run(&["activate", "bash"], fixture.path(), &state);
    assert!(bash.ok(), "{}", bash.combined());
    assert!(bash.stdout.contains("PROMPT_COMMAND="), "{}", bash.stdout);

    let fish = run(&["activate", "fish"], fixture.path(), &state);
    assert!(fish.ok(), "{}", fish.combined());
    assert!(fish.stdout.contains("--on-variable PWD"), "{}", fish.stdout);
}

#[test]
fn the_shell_falls_back_to_shell_env() {
    let fixture = Fixture::new();
    let output = command(&fixture)
        .args(["activate"])
        .env("SHELL", "/bin/zsh")
        .output()
        .expect("run magictree");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("add-zsh-hook"));
}

#[test]
fn a_shell_we_cannot_hook_is_refused() {
    let fixture = Fixture::new();
    let output = command(&fixture)
        .args(["activate"])
        .env("SHELL", "/usr/bin/elvish")
        .output()
        .expect("run magictree");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not tell which shell"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
