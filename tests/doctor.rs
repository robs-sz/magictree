//! `doctor` reports real drift and stays quiet about valid choices.

mod support;

use magictree::discover::extract;
use magictree::doctor::compare;
use magictree::manifest::Loaded;
use support::Fixture;

fn repo() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("pnpm-workspace.yaml", "packages:\n  - \"apps/*\"\n");
    fixture.write(
        "compose.yaml",
        "services:\n  db:\n    image: postgres:18\n    ports:\n      - \"5432:5432\"\n",
    );
    fixture.write(
        "apps/web/package.json",
        r#"{"name":"web","packageManager":"pnpm@9","scripts":{"dev":"vite dev"}}"#,
    );
    fixture.write("apps/web/pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[workspace]
apps = ["apps/web"]

[[services]]
id = "db"
runtime = "compose"
compose = { file = "compose.yaml", service = "db" }
expose = "none"
"#,
    );
    fixture.write(
        "apps/web/magictree.toml",
        r#"
version = 1
[app]
id = "web"

[[services]]
id = "web"
target = { kind = "pnpm", script = "dev" }
port = { env = "PORT" }
"#,
    );
    fixture.git_repo();
    fixture
}

fn drift_summaries(fixture: &Fixture) -> Vec<String> {
    let report = extract(fixture.path()).expect("extract");
    let loaded = Loaded::load(fixture.path()).expect("load");
    compare(&report, &loaded)
        .into_iter()
        .filter(|entry| entry.is_drift())
        .map(|entry| entry.summary)
        .collect()
}

#[test]
fn a_matching_repository_has_no_drift() {
    let fixture = repo();
    assert_eq!(drift_summaries(&fixture), Vec::<String>::new());
}

#[test]
fn an_unmanaged_compose_service_is_information_not_drift() {
    let fixture = repo();
    fixture.write(
        "compose.yaml",
        "services:\n  db:\n    image: postgres:18\n    ports:\n      - \"5432:5432\"\n  mailpit:\n    image: axllent/mailpit\n",
    );

    let report = extract(fixture.path()).expect("extract");
    let loaded = Loaded::load(fixture.path()).expect("load");
    let findings = compare(&report, &loaded);

    let mailpit: Vec<&_> = findings
        .iter()
        .filter(|entry| entry.summary.contains("mailpit"))
        .collect();
    assert_eq!(mailpit.len(), 1, "the new service is mentioned once");
    assert!(
        !mailpit[0].is_drift(),
        "an optional service is a valid choice, not breakage"
    );
    assert!(drift_summaries(&fixture).is_empty());
}

#[test]
fn a_removed_script_is_drift() {
    let fixture = repo();
    fixture.write(
        "apps/web/package.json",
        r#"{"name":"web","packageManager":"pnpm@9","scripts":{"build":"vite build"}}"#,
    );

    let summaries = drift_summaries(&fixture);
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert!(summaries[0].contains("dev"), "{summaries:?}");
}

#[test]
fn a_compose_service_that_disappeared_is_drift() {
    let fixture = repo();
    fixture.write("compose.yaml", "services:\n  cache:\n    image: redis:8\n");

    let summaries = drift_summaries(&fixture);
    assert!(
        summaries.iter().any(|summary| summary.contains("db")),
        "removing a referenced service must be reported: {summaries:?}"
    );
}

#[test]
fn an_app_that_is_no_longer_declared_is_drift() {
    let fixture = repo();
    fixture.write(
        "apps/api/package.json",
        r#"{"name":"api","scripts":{"dev":"node server.js"}}"#,
    );
    fixture.write("apps/api/pnpm-lock.yaml", "lockfileVersion: 9\n");

    let summaries = drift_summaries(&fixture);
    assert!(
        summaries.iter().any(|summary| summary.contains("apps/api")),
        "a new app outside [workspace].apps must be reported: {summaries:?}"
    );
}

#[test]
fn a_port_variable_the_process_cannot_read_is_drift() {
    // The failure this guards against: the service listens on its own default
    // while the health check watches the allocated port, so `up` times out.
    let fixture = Fixture::new();
    fixture.write(
        "justfile",
        "svc_port := env(\"WT_PORT_SVC\", \"8000\")\n\nserve $MY_SERVICE_PORT=svc_port:\n  echo hi\n",
    );
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "srv"
target = { kind = "just", recipe = "serve" }
port = { env = "MY_SERVICE_PORT" }
"#,
    );
    fixture.git_repo();

    let summaries = drift_summaries(&fixture);
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert!(
        summaries[0].contains("MY_SERVICE_PORT"),
        "the message must name the offending variable: {summaries:?}"
    );

    // Pointing it at the variable the justfile actually reads clears the drift.
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "srv"
target = { kind = "just", recipe = "serve" }
port = { env = "WT_PORT_SVC" }
"#,
    );
    assert_eq!(drift_summaries(&fixture), Vec::<String>::new());
}

#[test]
fn a_bootstrap_input_that_no_longer_exists_is_drift() {
    let fixture = repo();
    fixture.write(
        "apps/web/magictree.toml",
        r#"
version = 1
[app]
id = "web"

[bootstrap]
run = [
  { command = "pnpm install", inputs = ["pnpm-lock.yaml", "gone.lock"] },
]

[[services]]
id = "web"
target = { kind = "pnpm", script = "dev" }
port = { env = "PORT" }
"#,
    );

    let summaries = drift_summaries(&fixture);
    assert!(
        summaries
            .iter()
            .any(|summary| summary.contains("gone.lock")),
        "an input that can never match means the cache never hits: {summaries:?}"
    );
}
