//! The notice a command prints when a release has landed since this binary was
//! installed.
//!
//! The check itself reaches GitHub, so every test here seeds the answer it
//! would have got and leaves the network out of it: the cache is the only
//! thing the notice reads while it is fresh.

mod support;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use support::{run, run_notifying, Fixture};

/// The check's cache, as a command that had just asked GitHub would leave it.
fn seed_check(state: &Path, version: Option<&str>) {
    let checked_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is past 1970")
        .as_secs();
    let version = match version {
        Some(version) => format!("\"{version}\""),
        None => "null".to_string(),
    };
    std::fs::write(
        state.join("update-check.json"),
        format!("{{\"checked_at\":{checked_at},\"version\":{version}}}"),
    )
    .expect("seed the update check");
}

fn config(state: &Path, contents: &str) {
    let dir = state.join("config");
    std::fs::create_dir_all(&dir).expect("create the config dir");
    std::fs::write(dir.join("config.toml"), contents).expect("write the config");
}

#[test]
fn a_newer_release_is_mentioned_after_a_command() {
    let fixture = Fixture::new();
    let state = fixture.state_dir();
    seed_check(&state, Some("99.0.0"));

    let result = run_notifying(&["completion", "zsh"], fixture.path(), &state);

    assert!(result.ok(), "{}", result.combined());
    assert!(
        result.stderr.contains("99.0.0") && result.stderr.contains("magictree update"),
        "the notice should name the release and how to install it: {:?}",
        result.stderr
    );
    assert!(
        result.stdout.contains("_magictree"),
        "the command's own output is untouched: {:?}",
        result.stdout
    );
}

#[test]
fn a_release_this_binary_already_is_says_nothing() {
    // An installed build ahead of the latest release — one built from a
    // checkout — is left alone rather than told it is out of date.
    let fixture = Fixture::new();
    let state = fixture.state_dir();
    seed_check(&state, Some("0.0.1"));

    let result = run_notifying(&["completion", "zsh"], fixture.path(), &state);

    assert!(result.ok(), "{}", result.combined());
    assert_eq!(result.stderr, "", "nothing to say about an older release");
}

#[test]
fn a_check_that_found_nothing_says_nothing() {
    let fixture = Fixture::new();
    let state = fixture.state_dir();
    seed_check(&state, None);

    let result = run_notifying(&["completion", "zsh"], fixture.path(), &state);

    assert!(result.ok(), "{}", result.combined());
    assert_eq!(result.stderr, "", "a check that reached nothing is silent");
}

#[test]
fn the_check_is_off_when_the_environment_asks() {
    let fixture = Fixture::new();
    let state = fixture.state_dir();
    seed_check(&state, Some("99.0.0"));

    let result = run(&["completion", "zsh"], fixture.path(), &state);

    assert!(result.ok(), "{}", result.combined());
    assert_eq!(
        result.stderr, "",
        "MAGICTREE_NO_UPDATE_CHECK turns the notice off"
    );
}

#[test]
fn the_check_is_off_when_the_config_asks() {
    let fixture = Fixture::new();
    let state = fixture.state_dir();
    seed_check(&state, Some("99.0.0"));
    config(&state, "check_for_updates = false\n");

    let result = run_notifying(&["completion", "zsh"], fixture.path(), &state);

    assert!(result.ok(), "{}", result.combined());
    assert_eq!(
        result.stderr, "",
        "check_for_updates = false turns the notice off"
    );
}
