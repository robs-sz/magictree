//! Port allocation: stable, collision-free, and honest about conflicts.

mod support;

use magictree::config::Config;
use magictree::paths::Paths;
use magictree::ports::{self, PortRequest};
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
    )
    .expect("allocate");
    let second = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[request("web")],
    )
    .expect("allocate again");

    assert_eq!(first.base, second.base);
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
    )
    .expect("allocate a");
    let right = ports::ensure(
        &paths,
        &config,
        "repo",
        "wt-b",
        fixture.path(),
        &[request("web")],
    )
    .expect("allocate b");

    assert_ne!(left.base, right.base);
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
    )
    .expect("allocate a");
    let right = ports::ensure(
        &paths,
        &config,
        "repo-b",
        "main",
        fixture.path(),
        &[request("web")],
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
    )
    .expect("extend");

    assert_eq!(second.ports.get("web"), Some(&web_port));
    assert!(second.ports.contains_key("api"));
}

#[test]
fn preferred_port_is_used_when_free() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    // Another test binary may take the port between picking it and allocating,
    // in which case a fresh one is tried; the property under test is unchanged.
    let mut last = None;
    for _ in 0..5 {
        let wanted = free_port();
        let assignment = ports::ensure(
            &paths,
            &config,
            "repo",
            "main",
            fixture.path(),
            &[PortRequest {
                name: "web".to_string(),
                prefer: Some(wanted),
                require: None,
            }],
        )
        .expect("allocate");
        let assigned = *assignment.ports.get("web").expect("a port");
        if assigned == wanted {
            return;
        }
        last = Some((wanted, assigned));
        // Release so the next attempt starts clean.
        let _ = ports::reassign(&paths, "repo", "main");
    }
    panic!("a free preference was never honoured, last: {last:?}");
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
        "main",
        fixture.path(),
        &[PortRequest {
            name: "web".to_string(),
            prefer: Some(taken),
            require: None,
        }],
    )
    .expect("allocate");

    let assigned = *assignment.ports.get("web").expect("a port");
    assert_ne!(
        assigned, taken,
        "a preference bound on the wildcard must not be used"
    );
}

#[test]
fn preferred_port_falls_back_when_taken() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();
    let guard = PortGuard::occupy(0);
    let taken = guard.port();

    let assignment = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[PortRequest {
            name: "web".to_string(),
            prefer: Some(taken),
            require: None,
        }],
    )
    .expect("allocate");

    let assigned = *assignment.ports.get("web").expect("a port");
    assert_ne!(assigned, taken, "a busy preference must not be used");
    assert!(assigned >= config.port_range_start && assigned <= config.port_range_end);
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
        &[PortRequest {
            name: "oauth".to_string(),
            prefer: None,
            require: Some(guard.port()),
        }],
    )
    .expect_err("must refuse");

    assert!(error.to_string().contains("not available"), "{error}");
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
        "main",
        fixture.path(),
        &[request("web")],
    )
    .expect("allocate");
    let base = first.base;
    let _guard = PortGuard::occupy(base);

    ports::reassign(&paths, "repo", "main").expect("release");
    let second = ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[request("web")],
    )
    .expect("allocate around the conflict");

    assert_ne!(
        second.base, base,
        "a block whose ports are busy must be skipped"
    );
}

#[test]
fn reassign_releases_the_assignment() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    ports::ensure(
        &paths,
        &config,
        "repo",
        "main",
        fixture.path(),
        &[request("web")],
    )
    .expect("allocate");
    let released = ports::reassign(&paths, "repo", "main").expect("release");
    assert!(released.is_some());

    let loaded = ports::load(&paths, "repo", "main").expect("load");
    assert!(loaded.is_none(), "released blocks are gone");
}

#[test]
fn a_preferred_port_stays_reserved_for_the_worktree_that_recorded_it() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    // A port can be taken by another process between picking it and allocating,
    // which would make the assertions vacuous; retry until the preference holds.
    let mut last = None;
    for _ in 0..5 {
        let wanted = free_port();
        let owner = ports::ensure(
            &paths,
            &config,
            "repo",
            "owner",
            fixture.path(),
            &[PortRequest {
                name: "web".to_string(),
                prefer: Some(wanted),
                require: None,
            }],
        )
        .expect("allocate owner");
        if owner.ports.get("web") != Some(&wanted) {
            last = Some(wanted);
            let _ = ports::reassign(&paths, "repo", "owner");
            continue;
        }

        // The owner is stopped, so nothing binds its port. A second worktree
        // must not take it anyway: the owner's assignment still holds it, and
        // handing it over makes the owner's dead service report healthy against
        // the new worktree's process.
        let other = ports::ensure(
            &paths,
            &config,
            "repo",
            "other",
            fixture.path(),
            &[PortRequest {
                name: "web".to_string(),
                prefer: Some(wanted),
                require: None,
            }],
        )
        .expect("allocate other");
        assert_ne!(
            other.ports.get("web"),
            Some(&wanted),
            "a port recorded by another worktree is reserved: {other:?}"
        );

        // Releasing the owner releases its reservation.
        ports::reassign(&paths, "repo", "owner").expect("release owner");
        let next = ports::ensure(
            &paths,
            &config,
            "repo",
            "next",
            fixture.path(),
            &[PortRequest {
                name: "web".to_string(),
                prefer: Some(wanted),
                require: None,
            }],
        )
        .expect("allocate next");
        assert_eq!(
            next.ports.get("web"),
            Some(&wanted),
            "a released preference is available again"
        );
        return;
    }
    panic!("a free preference was never honoured, last: {last:?}");
}

#[test]
fn required_port_held_by_another_worktree_fails_loudly() {
    let fixture = Fixture::new();
    let paths = paths(&fixture);
    let config = Config::default();

    let mut last = None;
    for _ in 0..5 {
        let wanted = free_port();
        let owner = ports::ensure(
            &paths,
            &config,
            "repo",
            "owner",
            fixture.path(),
            &[PortRequest {
                name: "web".to_string(),
                prefer: Some(wanted),
                require: None,
            }],
        )
        .expect("allocate owner");
        if owner.ports.get("web") != Some(&wanted) {
            last = Some(wanted);
            let _ = ports::reassign(&paths, "repo", "owner");
            continue;
        }

        let error = ports::ensure(
            &paths,
            &config,
            "repo",
            "other",
            fixture.path(),
            &[PortRequest {
                name: "oauth".to_string(),
                prefer: None,
                require: Some(wanted),
            }],
        )
        .expect_err("must refuse a port another worktree holds");
        assert!(error.to_string().contains("held by"), "{error}");
        return;
    }
    panic!("a free preference was never honoured, last: {last:?}");
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
    )
    .expect("allocate");

    for port in assignment.ports.values() {
        assert!(
            *port >= config.port_range_start && *port <= config.port_range_end,
            "{port} is outside the configured range"
        );
    }
}
