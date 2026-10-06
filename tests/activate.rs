//! `magictree activate` through the real binary: the snippet a shell evaluates
//! once, and the environment it syncs from the state-dir mirror.

mod support;

use std::path::Path;
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

/// The binary with an isolated `$HOME` too, for `--install`: it must write into a
/// temp rc file rather than the developer's. `$ZDOTDIR` is cleared unless a test
/// sets it.
fn install_command(fixture: &Fixture, home: &Path) -> Command {
    let mut command = command(fixture);
    command.env("HOME", home).env_remove("ZDOTDIR");
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

#[test]
fn install_appends_the_activation_line_once() {
    let fixture = Fixture::new();
    let home = tempfile::tempdir().expect("temp home");

    let first = install_command(&fixture, home.path())
        .args(["activate", "zsh", "--install"])
        .output()
        .expect("run magictree");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let rc = home.path().join(".zshrc");
    let contents = std::fs::read_to_string(&rc).expect("read rc");
    assert!(contents.contains("activate zsh"), "{contents}");
    assert!(
        contents.contains("# added by magictree activate zsh"),
        "{contents}"
    );

    // A second install finds the marker and appends nothing.
    let second = install_command(&fixture, home.path())
        .args(["activate", "zsh", "--install"])
        .output()
        .expect("run magictree");
    assert!(second.status.success());
    let contents = std::fs::read_to_string(&rc).expect("read rc");
    assert_eq!(
        contents
            .matches("# added by magictree activate zsh")
            .count(),
        1,
        "{contents}"
    );
}

#[test]
fn install_honors_zdotdir_for_zsh() {
    let fixture = Fixture::new();
    let home = tempfile::tempdir().expect("temp home");
    let zdotdir = home.path().join("zdot");
    std::fs::create_dir_all(&zdotdir).expect("create zdotdir");

    let output = install_command(&fixture, home.path())
        .env("ZDOTDIR", &zdotdir)
        .args(["activate", "zsh", "--install"])
        .output()
        .expect("run magictree");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(zdotdir.join(".zshrc").is_file(), "ZDOTDIR is written");
    assert!(!home.path().join(".zshrc").exists(), "HOME is not");
}

#[test]
fn install_creates_the_fish_config_under_home() {
    let fixture = Fixture::new();
    let home = tempfile::tempdir().expect("temp home");

    let output = install_command(&fixture, home.path())
        .args(["activate", "fish", "--install"])
        .output()
        .expect("run magictree");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = home.path().join(".config/fish/config.fish");
    let contents = std::fs::read_to_string(&config).expect("read fish config");
    assert!(contents.contains("activate fish | source"), "{contents}");
}
