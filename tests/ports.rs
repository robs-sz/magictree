//! Port allocation: stable, collision-free, and honest about conflicts.

mod support;

use magictree::config::Config;
use magictree::paths::Paths;
use magictree::ports::{self, PortMode, PortRequest};
use support::{free_port, Fixture, PortGuard};

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

fn preferred(name: &str, port: u16) -> PortRequest {
    PortRequest {
        name: name.to_string(),
        prefer: Some(port),
        require: None,
    }
}

fn required(name: &str, port: u16) -> PortRequest {
    PortRequest {
        name: name.to_string(),
        prefer: None,
        require: Some(port),
    }
}

#[test]
fn assignments_are_stable_across_calls() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    let first = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate");
    let second = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate again");

    assert_eq!(first.block_start, second.block_start);
    assert_eq!(first.ports.get("web"), second.ports.get("web"));
}

#[test]
fn different_worktrees_of_one_repository_get_different_blocks() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    let left = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-a",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate a");
    let right = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-b",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate b");

    assert_ne!(left.block_start, right.block_start);
    assert_ne!(left.ports.get("web"), right.ports.get("web"));
}

#[test]
fn different_repositories_do_not_share_assignments() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    let left = ports::ensure(
        &paths,
        &config,
        "repo-a",
        "main",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate a");
    let right = ports::ensure(
        &paths,
        &config,
        "repo-b",
        "main",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate b");
    assert_ne!(
        left.ports.get("web"),
        right.ports.get("web"),
        "two repositories on one machine must not collide"
    );
}

#[test]
fn a_new_service_does_not_move_existing_ports() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    let first = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate");
    let web_port = *first.ports.get("web").unwrap();

    let second = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[request("web"), request("api")],
        None,
    )
    .expect("extend");

    assert_eq!(second.ports.get("web"), Some(&web_port));
    assert!(second.ports.contains_key("api"));
}

#[test]
fn the_primary_checkout_takes_a_declared_port() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let wanted = free_port();

    let assignment = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[preferred("web", wanted)],
        None,
    )
    .expect("allocate");

    assert_eq!(assignment.ports.get("web"), Some(&wanted));
    assert_eq!(assignment.mode, PortMode::Declared);
}

#[test]
fn a_declared_port_is_taken_even_while_another_process_holds_it() {
    // The declared port is the checkout's own: the repository's tooling, its
    // generated `.env` files and anything registered against its callback URL
    // were written against it. Moving the stack to another port to dodge a busy
    // one is what silently splits the two, so the port is recorded as declared
    // and the launch reports the conflict.
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let guard = PortGuard::occupy(0);
    let held = guard.port();

    let assignment = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[preferred("web", held)],
        None,
    )
    .expect("allocate");

    assert_eq!(
        assignment.ports.get("web"),
        Some(&held),
        "a declared port is not traded away for a free one"
    );
}

#[test]
fn a_linked_worktree_allocates_from_its_block_and_ignores_a_declaration() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let declared = free_port();

    let assignment = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-a",
        fixture.path(),
        &[preferred("web", declared)],
        None,
    )
    .expect("allocate");

    let assigned = *assignment.ports.get("web").expect("a port");
    assert_ne!(
        assigned, declared,
        "a declared port belongs to the primary checkout"
    );
    assert!(assigned >= config.port_range_start && assigned <= config.port_range_end);
    assert_eq!(assignment.mode, PortMode::Block);
}

#[test]
fn a_linked_worktree_cannot_ask_for_declared_ports() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    let error = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-a",
        fixture.path(),
        &[request("web")],
        Some(PortMode::Declared),
    )
    .expect_err("must refuse");

    assert!(error.to_string().contains("primary checkout"), "{error}");
}

#[test]
fn generated_ports_ignore_declarations_on_the_primary_checkout() {
    // `up --ports generated` is how a primary checkout keeps a stack off the
    // ports the repository declares, for local testing beside another stack.
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let declared = free_port();

    let assignment = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[preferred("web", declared)],
        Some(PortMode::Block),
    )
    .expect("allocate");

    let assigned = *assignment.ports.get("web").expect("a port");
    assert_ne!(assigned, declared);
    assert!(assigned >= config.port_range_start && assigned <= config.port_range_end);
    assert_eq!(assignment.mode, PortMode::Block);
}

#[test]
fn switching_mode_reallocates() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let declared = free_port();

    let first = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[preferred("web", declared)],
        None,
    )
    .expect("allocate declared");
    assert_eq!(first.ports.get("web"), Some(&declared));

    let generated = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[preferred("web", declared)],
        Some(PortMode::Block),
    )
    .expect("allocate generated");
    let moved = *generated.ports.get("web").expect("a port");
    assert_ne!(
        moved, declared,
        "asking for generated ports moves the stack off the declared ones"
    );
    assert_eq!(generated.mode, PortMode::Block);

    let back = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[preferred("web", declared)],
        Some(PortMode::Declared),
    )
    .expect("allocate declared again");
    assert_eq!(
        back.ports.get("web"),
        Some(&declared),
        "and back again: the declared port was released with the old assignment"
    );
    assert_eq!(back.mode, PortMode::Declared);
}

#[test]
fn a_port_bound_on_the_wildcard_is_not_free() {
    // A dev server binds `::`/`0.0.0.0`, not loopback. Probing loopback alone
    // reported such a port as free, so two worktrees were handed the same one:
    // the second service died with EADDRINUSE while its health probe was
    // answered by the first worktree's process, and the stack was recorded ready.
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let guard = PortGuard::occupy_wildcard(0);
    assert!(guard.held(), "the fixture needs a dual-stack wildcard");
    let taken = guard.port();

    assert!(
        !ports::port_free(taken),
        "a port listening on the wildcard is not free"
    );

    let assignment = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-a",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate");

    for port in assignment.ports.values() {
        assert_ne!(*port, taken, "a busy port must not be handed out");
    }
}

#[test]
fn a_linked_worktree_never_takes_the_primary_checkouts_port() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let declared = free_port();

    // The primary checkout declares its port. Nothing binds it here, which is
    // exactly the case that used to hand it to a worktree started first.
    let primary = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[preferred("web", declared)],
        None,
    )
    .expect("allocate primary");
    assert_eq!(primary.ports.get("web"), Some(&declared));

    let other = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-a",
        fixture.path(),
        &[preferred("web", declared)],
        None,
    )
    .expect("allocate linked");
    assert_ne!(other.ports.get("web"), Some(&declared));

    // Releasing the primary's assignment does not put its port up for grabs:
    // a linked worktree allocates from its block whatever the manifest declares.
    ports::release(&paths, "repo", "main").expect("release primary");
    let after = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-b",
        fixture.path(),
        &[preferred("web", declared)],
        None,
    )
    .expect("allocate refreshed linked");
    assert_ne!(after.ports.get("web"), Some(&declared));

    let reclaimed = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[preferred("web", declared)],
        None,
    )
    .expect("allocate primary again");
    assert_eq!(reclaimed.ports.get("web"), Some(&declared));
}

#[test]
fn a_declared_port_held_by_another_checkout_fails_loudly() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let declared = free_port();

    ports::ensure(
        &paths,
        &config,
        "repo-a",
        "main",
        fixture.path(),
        &[preferred("web", declared)],
        None,
    )
    .expect("allocate the first checkout");

    let error = ports::ensure(
        &paths,
        &config,
        "repo-b",
        "main",
        fixture.path(),
        &[preferred("web", declared)],
        None,
    )
    .expect_err("must refuse a port another checkout holds");

    assert!(error.to_string().contains("held by"), "{error}");
    assert!(error.to_string().contains("--ports"), "{error}");
}

#[test]
fn required_port_that_is_taken_fails_loudly() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let guard = PortGuard::occupy(0);

    let error = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[required("oauth", guard.port())],
        None,
    )
    .expect_err("must refuse");

    assert!(error.to_string().contains("not available"), "{error}");
}

#[test]
fn required_port_held_by_another_worktree_fails_loudly() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let declared = free_port();

    ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[preferred("web", declared)],
        None,
    )
    .expect("allocate the primary checkout");

    let error = ports::ensure(
        &paths,
        &config,
        "repo",
        "other",
        fixture.path(),
        &[required("oauth", declared)],
        None,
    )
    .expect_err("must refuse a port another worktree holds");
    assert!(error.to_string().contains("held by"), "{error}");
}

#[test]
fn blocks_skip_over_a_range_that_is_already_busy() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    // Occupy the first port of the block this repository would otherwise take,
    // forcing allocation elsewhere rather than handing out a busy port.
    let first = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-a",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate");
    let block_start = first.block_start;
    let _guard = PortGuard::occupy(block_start);

    ports::release(&paths, "repo", "wt-a").expect("release");
    let second = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-a",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate around the conflict");

    assert_ne!(
        second.block_start, block_start,
        "a block whose ports are busy must be skipped"
    );
}

#[test]
fn release_frees_the_assignment() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-a",
        fixture.path(),
        &[request("web")],
        None,
    )
    .expect("allocate");
    let released = ports::release(&paths, "repo", "wt-a").expect("release");
    assert!(released.is_some());

    let loaded = ports::load(&paths, "repo", "wt-a").expect("load");
    assert!(loaded.is_none(), "released blocks are gone");

    let again = ports::release(&paths, "repo", "wt-a").expect("release nothing");
    assert!(again.is_none(), "releasing twice is not an error");
}

#[test]
fn configured_range_is_respected() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config {
        port_range_start: 41000,
        port_range_end: 41200,
        port_stride: 10,
        ..Config::default()
    };

    let assignment = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[request("web"), request("api")],
        None,
    )
    .expect("allocate");

    for port in assignment.ports.values() {
        assert!(
            *port >= config.port_range_start && *port <= config.port_range_end,
            "{port} is outside the configured range"
        );
    }
}
