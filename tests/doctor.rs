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

/// A justfile that sets a variable for one of its own recipes says nothing
/// about a service magictree starts some other way: `pnpm run dev` never reads
/// the justfile, so the variable that process reads is the right one to
/// declare, however the justfile spells its own.
#[test]
fn a_service_magictree_does_not_start_through_just_takes_its_declared_variable() {
    let fixture = justfile_port_fixture("target = { kind = \"pnpm\", script = \"dev\" }");
    assert_eq!(drift_summaries(&fixture), Vec::<String>::new());
}

/// The same variable is drift when magictree does launch the recipe: `just`
/// hands the recipe's own value to the process, overwriting the injected one.
#[test]
fn a_service_started_through_just_is_held_to_the_variable_it_declares() {
    let fixture = justfile_port_fixture("target = { kind = \"just\", recipe = \"dev\" }");
    let summaries = drift_summaries(&fixture);
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert!(
        summaries[0].contains("WEB_PORT"),
        "the message must name the offending variable: {summaries:?}"
    );
}

/// A `command` that runs the justfile is on the same hook as a `just` target.
#[test]
fn a_command_that_runs_just_is_held_to_the_same_rule() {
    let fixture = justfile_port_fixture("command = \"just dev\"");
    let summaries = drift_summaries(&fixture);
    assert!(
        summaries.iter().any(|entry| entry.contains("WEB_PORT")),
        "a command running a recipe overwrites the port the same way: {summaries:?}"
    );
}

/// A repository whose justfile exports a port variable for its `dev` recipe,
/// with the service launched by whichever `service` line the caller passes.
fn justfile_port_fixture(service: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "justfile",
        "wt_port_web := env(\"WT_PORT_WEB\", \"3000\")\n\ndev $WEB_PORT=wt_port_web:\n  pnpm dev\n",
    );
    fixture.write(
        "package.json",
        r#"{"name":"web","packageManager":"pnpm@9","scripts":{"dev":"vite dev"}}"#,
    );
    fixture.write("pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.write(
        "magictree.toml",
        &format!(
            r#"
version = 1

[[services]]
id = "web"
{service}
port = {{ env = "WEB_PORT" }}
"#
        ),
    );
    fixture.git_repo();
    fixture
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

/// A repository whose dev script pins the port it listens on, and whose
/// manifest hands it a variable a compose service also claims.
fn pinned_repo() -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"app","packageManager":"npm@10","scripts":{"dev":"next dev --turbo -p 3005","seed":"tsx scripts/seed.ts --port 5433"}}"#,
    );
    fixture.write("package-lock.json", "{}");
    fixture.write(
        "compose.yaml",
        "services:\n  db:\n    image: postgres:18\n    ports:\n      - \"5433:5432\"\n",
    );
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[app]
id = "app"

[[services]]
id = "db"
compose = { file = "compose.yaml", service = "db" }
port = { env = "DATABASE_PORT", target = 5432, prefer = 5433 }

[[services]]
id = "app"
target = { kind = "npm", script = "dev" }
port = { env = "DATABASE_PORT" }
health = { http = "/", timeout = 120 }
"#,
    );
    fixture.git_repo();
    fixture
}

fn findings(fixture: &Fixture) -> Vec<magictree::doctor::Drift> {
    let report = extract(fixture.path()).expect("extract");
    let loaded = Loaded::load(fixture.path()).expect("load");
    compare(&report, &loaded)
}

#[test]
fn a_service_that_pins_its_port_is_drift_with_the_rewrite() {
    let fixture = pinned_repo();
    let all = findings(&fixture);
    let pinned = all
        .iter()
        .find(|entry| entry.summary.contains("pins port 3005"))
        .expect("the pinned port must be reported");

    assert!(
        pinned.is_drift(),
        "a pinned port breaks every later worktree"
    );
    assert!(
        pinned.summary.contains("next dev --turbo -p 3005"),
        "the finding must show the command: {}",
        pinned.summary
    );
    let suggestion = pinned.suggestion.clone().unwrap_or_default();
    assert!(
        suggestion.contains("${APP_PORT:-3005}"),
        "the fix must name a variable the app can own, not the db's: {suggestion}"
    );
    assert!(
        suggestion.contains("'DATABASE_PORT' is also service 'db's"),
        "the reason must be given: {suggestion}"
    );
}

#[test]
fn a_pinned_port_magictree_does_not_run_is_information() {
    let fixture = pinned_repo();
    let all = findings(&fixture);
    let seed = all
        .iter()
        .find(|entry| entry.summary.contains("db:seed") || entry.summary.contains("'seed'"))
        .expect("the seed script pins a port too");

    assert!(
        !seed.is_drift(),
        "a script magictree never starts is not drift"
    );
    let suggestion = seed.suggestion.clone().unwrap_or_default();
    assert!(
        suggestion.contains("${DATABASE_PORT:-5433}"),
        "a literal matching a declared port points at that service's variable: {suggestion}"
    );
}

#[test]
fn a_command_pinned_in_the_manifest_is_drift() {
    let fixture = pinned_repo();
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[app]
id = "app"

[[services]]
id = "app"
command = "node server.js --port 4000"
port = { env = "PORT" }
"#,
    );

    let all = findings(&fixture);
    let pinned = all
        .iter()
        .find(|entry| entry.summary.contains("4000"))
        .expect("the manifest command pins a port");
    assert!(pinned.is_drift());
    let suggestion = pinned.suggestion.clone().unwrap_or_default();
    assert!(suggestion.contains("${PORT:-4000}"), "{suggestion}");
    assert!(
        suggestion.contains("port.env") || suggestion.contains("already names PORT"),
        "the manifest already declares the variable: {suggestion}"
    );
}

#[test]
fn a_script_that_already_reads_its_port_is_clean() {
    let fixture = pinned_repo();
    fixture.write(
        "package.json",
        r#"{"name":"app","packageManager":"npm@10","scripts":{"dev":"next dev --turbo -p ${APP_PORT:-3005}"}}"#,
    );
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[app]
id = "app"

[[services]]
id = "app"
target = { kind = "npm", script = "dev" }
port = { env = "APP_PORT" }
"#,
    );

    assert_eq!(drift_summaries(&fixture), Vec::<String>::new());
}

#[test]
fn a_service_that_hands_its_port_to_the_step_is_clean() {
    // What `init` writes for Storybook: the allocated port is appended after the
    // script's own arguments, so the literal the script pins never binds.
    let fixture = pinned_repo();
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[app]
id = "app"

[[services]]
id = "app"
target = { kind = "pnpm", script = "dev", args = ["-p", "${APP_PORT:-3005}"] }
port = { env = "APP_PORT" }
"#,
    );

    assert_eq!(drift_summaries(&fixture), Vec::<String>::new());
}

#[test]
fn an_npm_service_that_does_not_separate_its_arguments_is_still_drift() {
    // `npm run` consumes anything that is not behind `--`, so those arguments
    // never reach the script and the literal it pins is still the one that binds.
    let fixture = pinned_repo();
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[app]
id = "app"

[[services]]
id = "app"
target = { kind = "npm", script = "dev", args = ["-p", "${APP_PORT:-3005}"] }
port = { env = "APP_PORT" }
"#,
    );

    let summaries = drift_summaries(&fixture);
    assert!(
        summaries
            .iter()
            .any(|summary| summary.contains("pins port 3005")),
        "the port the script pins is still the one that binds: {summaries:?}"
    );
}
