//! `init` is a pure function of (report, answers) and must stay that way.

mod support;

use magictree::discover::report::{Answer, AnswerSet, REPORT_VERSION};
use magictree::discover::{extract, Report};
use magictree::init::{apply, default_answers, record, warnings, Applied, Generated, Recorded};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use support::Fixture;

/// `init::plan` with the record a run with no history behind it writes: every
/// question decided against the options on offer now.
fn plan(report: &Report, answers: &AnswerSet, root: &Path) -> anyhow::Result<Vec<Generated>> {
    let asked: BTreeSet<String> = answers.answers.keys().cloned().collect();
    let record = record(report, answers, &Recorded::default(), &asked);
    magictree::init::plan(report, answers, &record, root)
}

fn answers_for(report: &Report) -> AnswerSet {
    default_answers(report)
}

fn monorepo() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("pnpm-workspace.yaml", "packages:\n  - \"apps/*\"\n");
    fixture.write(
        "compose.yaml",
        "services:\n  db:\n    image: example/db:18\n    ports:\n      - \"${WT_PORT_DB:-5432}:5432\"\n  cache:\n    image: example/cache:8\n    ports:\n      - \"${WT_PORT_CACHE:-6379}:6379\"\n",
    );
    fixture.write(
        "apps/web/package.json",
        r#"{"name":"web","packageManager":"pnpm@9","scripts":{"dev":"vite dev"}}"#,
    );
    fixture.write("apps/web/pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.write(
        "apps/api/pyproject.toml",
        "[project]\nname = \"api\"\n[project.scripts]\nserve = \"api.main:run\"\n",
    );
    fixture.write("apps/api/uv.lock", "version = 1\n");
    fixture.git_repo();
    fixture
}

#[test]
fn same_report_and_answers_produce_identical_files() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let answers = answers_for(&report);

    let first = plan(&report, &answers, fixture.path()).expect("plan");
    let second = plan(&report, &answers, fixture.path()).expect("plan again");

    assert_eq!(first.len(), second.len());
    for (left, right) in first.iter().zip(second.iter()) {
        assert_eq!(left.path, right.path);
        assert_eq!(
            left.contents(),
            right.contents(),
            "init must be deterministic"
        );
    }
}

#[test]
fn answers_from_a_different_report_are_rejected() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let mut answers = answers_for(&report);
    answers.report_hash = "sha256:someone-elses-report".to_string();

    let error = plan(&report, &answers, fixture.path()).expect_err("must reject");
    let message = error.to_string();
    assert!(
        message.contains("re-run discovery"),
        "unhelpful error: {message}"
    );
}

#[test]
fn unanswered_unknowns_block_init() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let mut answers = answers_for(&report);
    answers.answers.remove("compose.shared");

    let error = plan(&report, &answers, fixture.path()).expect_err("must reject");
    assert!(
        error.to_string().contains("compose.shared"),
        "the error must name the missing answer: {error}"
    );
}

#[test]
fn generates_a_workspace_manifest_and_one_manifest_per_app() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");

    let mut paths: Vec<String> = planned
        .iter()
        .map(|file| {
            file.path
                .strip_prefix(fixture.path())
                .unwrap()
                .display()
                .to_string()
        })
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "apps/api/magictree.toml",
            "apps/web/magictree.toml",
            "magictree.toml"
        ]
    );

    let workspace = planned
        .iter()
        .find(|file| file.path.ends_with("magictree.toml") && !file.contents().contains("[app]"))
        .expect("workspace manifest")
        .contents();
    assert!(workspace.contains("[workspace]"));
    assert!(workspace.contains("apps = [\"apps/api\", \"apps/web\"]"));
    // Shared infrastructure is declared once, at the workspace level, and is not
    // published to the host.
    assert!(workspace.contains("id = \"db\""));
    assert!(workspace.contains("expose = \"none\""));

    let web = planned
        .iter()
        .find(|file| file.path.ends_with("apps/web/magictree.toml"))
        .expect("web manifest")
        .contents();
    assert!(web.contains("kind = \"pnpm\", script = \"dev\""));
    assert!(
        !web.contains("needs"),
        "shared infrastructure is implicit, so app manifests stay free of boilerplate:\n{web}"
    );
    assert!(web.contains("inputs = [\"pnpm-lock.yaml\", \"package.json\"]"));
}

#[test]
fn answers_change_the_output_predictably() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");

    let mut answers: AnswerSet = answers_for(&report);
    answers.answers.insert(
        "compose.expose".to_string(),
        Answer::Many(vec!["db".to_string()]),
    );
    answers.answers.insert(
        "stack.members".to_string(),
        Answer::Many(vec!["apps/web".to_string()]),
    );
    answers
        .answers
        .insert("api.run".to_string(), Answer::One("skip".to_string()));

    let planned = plan(&report, &answers, fixture.path()).expect("plan");

    let workspace = planned
        .iter()
        .find(|file| file.contents().contains("[workspace]"))
        .expect("workspace manifest")
        .contents();
    assert!(
        workspace.contains("port = { target = 5432, env = \"WT_PORT_DB\" }"),
        "an exposed service keeps a host port and the variable compose derives from:\n{workspace}"
    );
    assert!(workspace.contains("apps = [\"apps/web\"]"));

    assert!(
        !planned
            .iter()
            .any(|file| file.path.ends_with("apps/api/magictree.toml")),
        "an app outside the stack membership gets no manifest"
    );
}

#[test]
fn exposes_nothing_by_default_and_keeps_targets_unresolvable() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");
    let workspace = planned
        .iter()
        .find(|file| file.contents().contains("[workspace]"))
        .expect("workspace manifest")
        .contents();
    assert!(
        !workspace.contains("port = {"),
        "nothing is published unless asked for:\n{workspace}"
    );
}

#[test]
fn apply_adds_the_services_an_existing_manifest_lacks() {
    // The case this exists for: the repository gained a service, and the
    // manifest already carries choices that regenerating it would discard.
    let fixture = storybook_app(
        r#"{"dev":"vite dev","storybook":"storybook dev -p 6006"}"#,
        "app",
    );
    let report = extract(fixture.path()).expect("extract");
    let derived = report.apps[0].id.clone();
    fixture.write(
        "magictree.toml",
        r#"version = 1

[app]
id = "design-system"

# Kept as it was written, under a name of its own.
[[services]]
id = "web"
target = { kind = "pnpm", script = "dev" }
port = { env = "PORT", prefer = 5173 }
health = { http = "/", timeout = 60 }
"#,
    );
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");

    let applied = apply(&planned, false).expect("update");
    assert_eq!(
        applied,
        vec![Applied::Updated {
            path: fixture.join("magictree.toml"),
            added: vec!["storybook".to_string()],
            // The answers the update was planned from are recorded as well.
            recorded: Some(Vec::new()),
        }],
        "the step the manifest already runs is not added a second time"
    );

    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert!(
        manifest.contains("answers = { \"") && manifest.contains(".run\" = \"pnpm:dev\""),
        "the answers are recorded:\n{manifest}"
    );
    assert!(
        manifest.contains("prefer = 5173") && manifest.contains("# Kept as it was written"),
        "the update must not rewrite what was there:\n{manifest}"
    );
    assert!(
        manifest.contains("id = \"storybook\"") && manifest.contains("${STORYBOOK_PORT:-6006}"),
        "the missing service is appended:\n{manifest}"
    );
    assert_eq!(
        manifest.matches("[[services]]").count(),
        2,
        "only the missing service is added:\n{manifest}"
    );
    assert!(
        !manifest.contains(&format!("id = \"{derived}\"")),
        "the plan's name for the app's own service must not be declared:\n{manifest}"
    );

    // A second run has nothing to do, and leaves the file as it is.
    assert_eq!(
        apply(&planned, false).expect("second update"),
        vec![Applied::Unchanged(fixture.join("magictree.toml"))]
    );

    let loaded = magictree::manifest::Loaded::load(fixture.path()).expect("load updated");
    loaded.validate().expect("the updated manifest is valid");
    assert_eq!(loaded.apps[0].manifest.services.len(), 2);
}

#[test]
fn the_answers_are_recorded_in_the_manifest_at_the_root() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");

    // One file records them: the one `init` was told to work in. Every question
    // is recorded, including the ones an app manifest answers, because the next
    // run has to know whether it asked them at all.
    let workspace = planned
        .iter()
        .find(|file| file.path.ends_with("magictree.toml") && !file.contents().contains("[app]"))
        .expect("workspace manifest")
        .contents();
    assert!(
        workspace.contains("# Recorded by `magictree init`"),
        "{workspace}"
    );
    assert!(
        workspace.contains("\"web.run\" = \"pnpm:dev\""),
        "{workspace}"
    );
    assert!(
        workspace.contains("\"compose.shared\" = [\"cache\", \"db\"]"),
        "a multi-choice answer is a list:\n{workspace}"
    );
    // The line is a bare key, so it has to sit above the first table.
    let answers = workspace.find("answers = {").expect("the answers line");
    let table = workspace.find("[workspace]").expect("the workspace table");
    assert!(
        answers < table,
        "a bare key above the first table:\n{workspace}"
    );

    let web = planned
        .iter()
        .find(|file| file.path.ends_with("apps/web/magictree.toml"))
        .expect("web manifest")
        .contents();
    assert!(!web.contains("answers = {"), "{web}");

    // The runtime never reads the line.
    apply(&planned, false).expect("write");
    let loaded = magictree::manifest::Loaded::load(fixture.path()).expect("load generated");
    loaded.validate().expect("generated manifests are valid");
}

#[test]
fn a_recorded_answer_is_replayed_and_a_stale_one_is_asked_again() {
    let fixture = storybook_app(
        r#"{"dev":"vite dev","storybook":"storybook dev -p 6006"}"#,
        "app",
    );
    let report = extract(fixture.path()).expect("extract");
    let id = report.apps[0].id.clone();
    fixture.write(
        "magictree.toml",
        &format!(
            r#"version = 1

answers = {{ "{id}.run" = "pnpm:dev", "{id}.storybook" = "skip", "{id}.port_env" = "GONE", "leftover.question" = "x" }}

[app]
id = "{id}"
"#
        ),
    );

    let recorded = magictree::init::recorded(fixture.path(), &report).expect("recorded");

    assert_eq!(
        recorded
            .answers
            .get(&format!("{id}.run"))
            .map(Answer::as_one),
        Some(Some("pnpm:dev")),
        "an answer the report still offers is replayed"
    );
    assert_eq!(
        recorded
            .answers
            .get(&format!("{id}.storybook"))
            .map(Answer::as_one),
        Some(Some("skip")),
        "a deliberate skip is remembered"
    );
    let dropped_ids: Vec<String> = recorded.dropped.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(
        dropped_ids,
        vec![format!("{id}.port_env"), "leftover.question".to_string()],
        "a question that is gone, and one whose options no longer offer the answer"
    );
}

#[test]
fn an_update_replaces_the_recorded_answers_in_place() {
    let fixture = storybook_app(
        r#"{"dev":"vite dev","storybook":"storybook dev -p 6006"}"#,
        "app",
    );
    let report = extract(fixture.path()).expect("extract");
    let id = report.apps[0].id.clone();
    fixture.write(
        "magictree.toml",
        &format!(
            r#"version = 1

# Recorded by `magictree init`; replayed so only new questions are asked.
answers = {{ "{id}.run" = "skip" }}

[app]
id = "{id}"

[[services]]
id = "{id}"
target = {{ kind = "pnpm", script = "dev" }}
port = {{ env = "PORT" }}
"#
        ),
    );
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");

    let applied = apply(&planned, false).expect("update");
    assert_eq!(
        applied,
        vec![Applied::Updated {
            path: fixture.join("magictree.toml"),
            added: vec!["storybook".to_string()],
            // `run` differs from what the manifest was built with, which an
            // additive update cannot apply to the service already written.
            recorded: Some(vec![format!("{id}.run")]),
        }]
    );

    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert_eq!(
        manifest.matches("answers = {").count(),
        1,
        "the line is replaced, not added:\n{manifest}"
    );
    assert!(
        manifest.contains(&format!("\"{id}.run\" = \"pnpm:dev\"")),
        "{manifest}"
    );
    assert!(
        !manifest.contains("\"skip\""),
        "the replaced line leaves nothing behind:\n{manifest}"
    );
    assert_eq!(manifest.matches("[[services]]").count(), 2, "{manifest}");
}

#[test]
fn a_service_added_to_the_compose_file_reopens_the_question_that_manages_it() {
    // The case the record exists for: someone adds a service to the compose
    // file after onboarding. Its answer was given against the services of that
    // day, so the new one is undecided rather than declined.
    let fixture = compose_app();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");
    apply(&planned, false).expect("first write");

    // The record carries what the answer turned down, so it can tell that the
    // new service was never decided.
    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert!(
        manifest.contains("declined = { \"compose.expose\" = [\"db\"] }"),
        "what the answer passed over is recorded:\n{manifest}"
    );

    fixture.write(
        "compose.yaml",
        "services:\n  db:\n    image: example/db:18\n    ports:\n      - \"5432:5432\"\n  cache:\n    image: example/cache:8\n  traces:\n    image: example/traces:latest\n",
    );
    let report = extract(fixture.path()).expect("extract");
    let recorded = magictree::init::recorded(fixture.path(), &report).expect("recorded");

    assert_eq!(
        recorded.reopened.get("compose.shared"),
        Some(&vec!["traces".to_string()]),
        "the new service reopens the question that decides it"
    );
    assert!(
        !recorded.settled("compose.shared"),
        "and the question is no longer settled"
    );

    // Running again with the recorded answers keeps them and leaves the new
    // service undecided; it is not quietly marked as declined.
    let mut answers = answers_for(&report);
    for (id, answer) in &recorded.answers {
        answers.answers.insert(id.clone(), answer.clone());
    }
    let asked: BTreeSet<String> = answers
        .answers
        .keys()
        .filter(|id| !recorded.answers.contains_key(*id))
        .cloned()
        .collect();
    let stored = record(&report, &answers, &recorded, &asked);
    let planned = magictree::init::plan(&report, &answers, &stored, fixture.path()).expect("plan");
    let manifest = planned
        .iter()
        .find(|file| file.path == fixture.join("magictree.toml"))
        .expect("the app manifest")
        .contents();
    assert!(
        !manifest.contains("traces"),
        "nothing decides the new service yet:\n{manifest}"
    );
    assert_eq!(
        stored.declined.get("compose.shared"),
        None,
        "a question that was not answered again decides nothing new"
    );

    // Asking it again with everything on offer makes `traces` shared.
    let answered: BTreeSet<String> = report.unknowns.iter().map(|u| u.id.clone()).collect();
    let stored = record(&report, &answers_for(&report), &recorded, &answered);
    let answers = answers_for(&report);
    let planned = magictree::init::plan(&report, &answers, &stored, fixture.path()).expect("plan");
    let manifest = planned
        .iter()
        .find(|file| file.path == fixture.join("magictree.toml"))
        .expect("the app manifest")
        .contents();
    assert!(manifest.contains("id = \"traces\""), "{manifest}");
}

/// A single app with a compose file it manages.
fn compose_app() -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "compose.yaml",
        "services:\n  db:\n    image: example/db:18\n    ports:\n      - \"5432:5432\"\n  cache:\n    image: example/cache:8\n",
    );
    fixture.write(
        "package.json",
        r#"{"name":"solo","packageManager":"pnpm@9","scripts":{"dev":"vite dev"}}"#,
    );
    fixture.write("pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.git_repo();
    fixture
}

#[test]
fn apply_regenerates_the_whole_manifest_when_forced() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");

    apply(&planned, false).expect("first write");
    fixture.write(
        "apps/web/magictree.toml",
        "version = 1\n\n[app]\nid = \"web\"\n\n[size]\nhand = \"written\"\n",
    );

    assert_eq!(
        apply(&planned, true).expect("forced write").len(),
        planned.len()
    );
    let web = std::fs::read_to_string(fixture.join("apps/web/magictree.toml")).expect("read");
    assert!(
        !web.contains("hand = \"written\""),
        "--force regenerates from discovery:\n{web}"
    );
}

#[test]
fn apply_refuses_a_manifest_it_cannot_read() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");

    apply(&planned, false).expect("first write");
    fixture.write("apps/web/magictree.toml", "version = 1\n[[services]\n");

    let error = apply(&planned, false).expect_err("must not touch an unreadable manifest");
    assert!(
        error.to_string().contains("apps/web/magictree.toml"),
        "the error must name the file: {error}"
    );
}

#[test]
fn generated_manifests_load_and_resolve() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");
    apply(&planned, false).expect("write");

    // The whole point: what init writes must be loadable by the runtime.
    let loaded = magictree::manifest::Loaded::load(fixture.path()).expect("load generated");
    loaded.validate().expect("generated manifests are valid");
    assert_eq!(loaded.apps.len(), 2);

    let nodes = magictree::manifest::nodes(&loaded);
    let edges = magictree::manifest::dependencies(&nodes).expect("dependencies resolve");
    let all: Vec<usize> = (0..nodes.len()).collect();
    let order = magictree::manifest::order(&nodes, &edges, &all).expect("order");

    // Infrastructure comes before the apps that depend on it.
    let position = |needle: &str| {
        order
            .iter()
            .position(|index| nodes[*index].qual() == needle)
            .unwrap_or_else(|| panic!("{needle} missing from the plan"))
    };
    assert!(position("db") < position("api:api"));
    assert!(position("cache") < position("web:web"));
}

#[test]
fn the_answered_port_variable_reaches_the_manifest() {
    let fixture = Fixture::new();
    fixture.write(
        "justfile",
        "wt_port_app := env(\"WT_PORT_APP\", \"8000\")\n\ndev:\n  echo hi\n",
    );
    fixture.write("package.json", r#"{"name":"solo"}"#);
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let mut answers = answers_for(&report);
    let id = report
        .unknowns
        .iter()
        .find(|u| u.id.ends_with(".port_env"))
        .map(|u| u.id.clone())
        .expect("a port variable question");
    answers
        .answers
        .insert(id, Answer::One("WT_PORT_APP".to_string()));

    let planned = plan(&report, &answers, fixture.path()).expect("plan");
    let manifest = planned[0].contents();
    assert!(
        manifest.contains("port = { env = \"WT_PORT_APP\" }"),
        "{manifest}"
    );
}

#[test]
fn a_direct_command_answer_becomes_a_command_target() {
    let fixture = Fixture::new();
    fixture.write(
        "justfile",
        "svc_port := env(\"WT_PORT_SVC\", \"8000\")\n\ndev:\n  uv run uvicorn app.main:app --port {{svc_port}}\n",
    );
    fixture.write("package.json", r#"{"name":"solo"}"#);
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let mut answers = answers_for(&report);
    let run_id = report
        .unknowns
        .iter()
        .find(|u| u.id.ends_with(".run"))
        .map(|u| u.id.clone())
        .expect("a run question");
    answers.answers.insert(
        run_id,
        Answer::One("command:uv run uvicorn app.main:app --port $WT_PORT_SVC".to_string()),
    );

    let planned = plan(&report, &answers, fixture.path()).expect("plan");
    let manifest = planned[0].contents();
    assert!(
        manifest.contains("command = \"uv run uvicorn app.main:app --port $WT_PORT_SVC\""),
        "{manifest}"
    );
    assert!(
        !manifest.contains("target ="),
        "a direct command replaces the runner target:\n{manifest}"
    );
}

#[test]
fn setup_steps_are_detected_and_run_after_install() {
    // A dev server can import files that are generated, not committed, so the
    // steps that produce them belong in bootstrap ahead of the services.
    let fixture = Fixture::new();
    fixture.write(
        "justfile",
        "export-openapi:\n  echo generated > openapi.json\n\ndev:\n  echo serve\n",
    );
    fixture.write("pyproject.toml", "[project]\nname = \"api\"\n");
    fixture.write("uv.lock", "version = 1\n");
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let setup = report
        .unknowns
        .iter()
        .find(|u| u.id.ends_with(".setup"))
        .expect("generation steps are offered");
    assert!(
        setup.options.contains(&"just:export-openapi".to_string()),
        "{:?}",
        setup.options
    );
    assert_eq!(setup.default.as_deref(), Some("just:export-openapi"));

    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");
    let manifest = planned[0].contents();
    let install = manifest.find("uv sync").expect("install step");
    let generate = manifest.find("just export-openapi").expect("setup step");
    assert!(install < generate, "install must come first:\n{manifest}");
}

#[test]
fn compose_ports_carry_the_variable_the_compose_file_derives_from() {
    // Publishing on an allocated port without setting the compose file's own
    // variable leaves its derived URLs pointing at the default.
    let fixture = Fixture::new();
    fixture.write(
        "compose.yaml",
        r#"
services:
  auth:
    image: ghcr.io/example/auth:latest
    ports:
      - "${WT_PORT_AUTH:-8080}:8080"
      - "${WT_PORT_AUTH_LOGIN:-3100}:3000"
  objects:
    image: example/objects:latest
    ports:
      - "${WT_PORT_OBJECTS:-9000}:9000"
"#,
    );
    fixture.write("package.json", r#"{"name":"solo"}"#);
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let mut answers = answers_for(&report);
    answers.answers.insert(
        "compose.expose".to_string(),
        Answer::Many(vec!["auth".to_string(), "objects".to_string()]),
    );
    let planned = plan(&report, &answers, fixture.path()).expect("plan");
    let manifest = planned[0].contents();

    assert!(
        manifest.contains("env = \"WT_PORT_AUTH\""),
        "the published port must also drive the compose variable:\n{manifest}"
    );
    assert!(
        manifest.contains("env = \"WT_PORT_AUTH_LOGIN\""),
        "{manifest}"
    );
    // A single-port service keeps the plain form, still carrying its variable.
    assert!(
        manifest.contains("port = { target = 9000, env = \"WT_PORT_OBJECTS\" }"),
        "{manifest}"
    );
    // Multi-port services get readable names derived from the variables.
    assert!(manifest.contains("name = \"auth\""), "{manifest}");
    assert!(
        manifest.contains("name = \"auth_login\""),
        "the login port is named from its variable, not its position:\n{manifest}"
    );
}

#[test]
fn initializer_services_are_told_to_run_to_completion() {
    let fixture = Fixture::new();
    fixture.write(
        "compose.yaml",
        r#"
services:
  db:
    image: example/db:18
    ports:
      - "5432:5432"
  stack-init:
    image: hashicorp/terraform:1.14
  seed-server:
    image: example/seed:latest
    ports:
      - "${WT_PORT_SEED:-9090}:9090"
"#,
    );
    fixture.write("package.json", r#"{"name":"solo"}"#);
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");
    let manifest = planned[0].contents();

    // An initialiser finishes; the services after it must wait for that.
    let stack_init = manifest
        .split("[[services]]")
        .find(|block| block.contains("id = \"stack-init\""))
        .expect("stack-init service");
    assert!(
        stack_init.contains("wait = \"exit\""),
        "stack-init must declare wait = exit:\n{stack_init}"
    );
    // A server whose name merely starts with "seed" must not be mistaken for one.
    let seed = manifest
        .split("[[services]]")
        .find(|block| block.contains("id = \"seed-server\""))
        .expect("seed-server service");
    assert!(
        !seed.contains("wait = \"exit\""),
        "seed-server keeps running:\n{seed}"
    );
}

#[test]
fn single_app_repo_gets_no_workspace_manifest() {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"solo","scripts":{"dev":"vite"}}"#,
    );
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");
    assert_eq!(report.report_version, REPORT_VERSION);
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");

    assert_eq!(planned.len(), 1, "a single app is one file");
    assert_eq!(planned[0].path, fixture.path().join("magictree.toml"));
    assert!(!planned[0].contents().contains("[workspace]"));
    assert!(planned[0].contents().contains("[app]"));
}

#[test]
fn unrecognised_run_answer_is_rejected_rather_than_guessed() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let mut answers = answers_for(&report);
    answers.answers.insert(
        "web.run".to_string(),
        Answer::One("make:whatever".to_string()),
    );

    let error = plan(&report, &answers, fixture.path()).expect_err("must reject");
    assert!(error.to_string().contains("make"), "{error}");
}

#[test]
fn exposing_a_service_with_no_published_port_fails_with_advice() {
    let fixture = Fixture::new();
    // The compose file never publishes a port, so the container-side port is
    // unknowable and init must say so rather than invent one.
    fixture.write("compose.yaml", "services:\n  app:\n    image: alpine\n");
    fixture.write("package.json", r#"{"name":"solo"}"#);
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");
    let mut answers = answers_for(&report);
    answers.answers.insert(
        "compose.expose".to_string(),
        Answer::Many(vec!["app".to_string()]),
    );

    let error = plan(&report, &answers, fixture.path()).expect_err("must reject");
    let message = error.to_string();
    assert!(message.contains("no published port"), "{message}");
    assert!(
        message.contains("compose.expose"),
        "must say how to proceed: {message}"
    );
}

#[test]
fn answer_members_must_exist_in_the_report() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let mut answers = answers_for(&report);
    let mut map = BTreeMap::new();
    map.insert(
        "stack.members".to_string(),
        Answer::Many(vec!["apps/ghost".to_string()]),
    );
    for (key, value) in answers.answers.clone() {
        map.entry(key).or_insert(value);
    }
    answers.answers = map;

    let error = plan(&report, &answers, fixture.path()).expect_err("must reject");
    assert!(error.to_string().contains("apps/ghost"), "{error}");
}

/// A repository whose dev script decides its own port.
fn pinned_app(dev: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        &format!(r#"{{"name":"app","packageManager":"npm@10","scripts":{{"dev":"{dev}"}}}}"#),
    );
    fixture.write("package-lock.json", "{}");
    fixture.git_repo();
    fixture
}

fn answers_with(report: &Report, run: &str, port_env: &str) -> AnswerSet {
    let app = &report.apps[0].id;
    let mut answers = default_answers(report);
    answers
        .answers
        .insert(format!("{app}.run"), Answer::One(run.to_string()));
    answers
        .answers
        .insert(format!("{app}.port_env"), Answer::One(port_env.to_string()));
    answers
}

#[test]
fn warns_when_the_chosen_run_step_pins_its_own_port() {
    let fixture = pinned_app("next dev --turbo -p 3005");
    let report = extract(fixture.path()).expect("extract");
    let answers = answers_with(&report, "npm:dev", "APP_PORT");

    let found = warnings(&report, &answers);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("script 'dev'"), "{found:?}");
    assert!(found[0].contains("${APP_PORT:-3005}"), "{found:?}");
}

#[test]
fn says_nothing_when_the_run_step_reads_the_variable() {
    let fixture = pinned_app("next dev --turbo -p ${APP_PORT:-3005}");
    let report = extract(fixture.path()).expect("extract");
    let answers = answers_with(&report, "npm:dev", "APP_PORT");

    assert!(warnings(&report, &answers).is_empty());
}

/// An app that has Storybook beside its own dev server.
fn storybook_app(scripts: &str, name: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        &format!(
            r#"{{"name":"{name}","packageManager":"pnpm@9","scripts":{scripts},"devDependencies":{{"@storybook/react-vite":"8.6.14"}}}}"#
        ),
    );
    fixture.write("pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.write(".storybook/main.ts", "export default { stories: [] };\n");
    fixture.git_repo();
    fixture
}

#[test]
fn the_storybook_answer_adds_a_service_on_its_own_port() {
    let fixture = storybook_app(
        r#"{"dev":"vite dev","storybook":"storybook dev -p 6006"}"#,
        "app",
    );
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");
    let manifest = planned[0].contents();
    let id = report.apps[0].id.clone();

    let storybook = manifest
        .split("[[services]]")
        .skip(1)
        .find(|block| block.contains("id = \"storybook\""))
        .expect("a service of its own");
    // Storybook reads its port from a flag and from no variable at all, so the
    // allocated port is passed on the command line, after the script's own.
    assert!(
        storybook.contains("args = [\"-p\", \"${STORYBOOK_PORT:-6006}\", \"--no-open\"]"),
        "{storybook}"
    );
    assert!(
        storybook.contains("port = { env = \"STORYBOOK_PORT\", prefer = 6006 }"),
        "{storybook}"
    );
    assert!(storybook.contains("health = { http = \"/\""), "{storybook}");

    // The app's own service is untouched: it keeps the variable it was answered.
    let app = manifest
        .split("[[services]]")
        .skip(1)
        .find(|block| block.contains(&format!("id = \"{id}\"")))
        .expect("the app service");
    assert!(app.contains("port = { env = \"PORT\" }"), "{app}");

    apply(&planned, false).expect("write");
    let loaded = magictree::manifest::Loaded::load(fixture.path()).expect("load generated");
    loaded.validate().expect("generated manifests are valid");
    assert_eq!(loaded.apps[0].manifest.services.len(), 2);
}

#[test]
fn an_app_started_by_storybook_itself_is_not_started_twice() {
    // A component library whose only server is Storybook: the run answer points
    // at the same script, so it is one service, with Storybook's port handling.
    let fixture = storybook_app(r#"{"dev":"storybook dev -p 6006"}"#, "app");
    let report = extract(fixture.path()).expect("extract");
    let answers = answers_for(&report);
    let id = report.apps[0].id.clone();
    assert_eq!(
        answers
            .answers
            .get(&format!("{id}.run"))
            .and_then(Answer::as_one),
        Some("pnpm:dev"),
    );
    assert_eq!(
        answers
            .answers
            .get(&format!("{id}.storybook"))
            .and_then(Answer::as_one),
        Some("pnpm:dev"),
        "the same step answers both questions"
    );

    let planned = plan(&report, &answers, fixture.path()).expect("plan");
    let manifest = planned[0].contents();
    assert_eq!(
        manifest.matches("[[services]]").count(),
        1,
        "the same server must not be declared twice:\n{manifest}"
    );
    assert!(
        manifest.contains("args = [\"-p\", \"${STORYBOOK_PORT:-6006}\", \"--no-open\"]"),
        "{manifest}"
    );
    assert!(
        warnings(&report, &answers).is_empty(),
        "the manifest hands Storybook its port, so nothing is left to warn about"
    );
}

#[test]
fn an_app_named_storybook_keeps_its_own_service_id() {
    let fixture = Fixture::new();
    fixture.write("pnpm-workspace.yaml", "packages:\n  - \"apps/*\"\n");
    fixture.write(
        "apps/storybook/package.json",
        r#"{"name":"stories","packageManager":"pnpm@9","scripts":{"dev":"vite dev","storybook":"storybook dev -p 6006"},"devDependencies":{"@storybook/react-vite":"8.6.14"}}"#,
    );
    fixture.write("apps/storybook/pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.write(
        "apps/storybook/.storybook/main.ts",
        "export default { stories: [] };\n",
    );
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");
    let manifest = planned
        .iter()
        .find(|file| file.path.ends_with("apps/storybook/magictree.toml"))
        .expect("the app manifest")
        .contents();

    assert!(manifest.contains("id = \"storybook\""), "{manifest}");
    assert!(
        manifest.contains("id = \"storybook-dev\""),
        "the app already owns the id 'storybook':\n{manifest}"
    );
    apply(&planned, false).expect("write");
    let loaded =
        magictree::manifest::Loaded::load(&fixture.join("apps/storybook")).expect("load generated");
    loaded.validate().expect("generated manifests are valid");
}
