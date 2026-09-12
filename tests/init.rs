//! `init` is a pure function of (report, answers) and must stay that way.

mod support;

use magictree::discover::report::{Answer, AnswerSet, REPORT_VERSION};
use magictree::discover::{extract, Report};
use magictree::init::{apply, default_answers, plan, warnings};
use std::collections::BTreeMap;
use support::Fixture;

fn answers_for(report: &Report) -> AnswerSet {
    default_answers(report)
}

fn monorepo() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("pnpm-workspace.yaml", "packages:\n  - \"apps/*\"\n");
    fixture.write(
        "compose.yaml",
        "services:\n  postgres:\n    image: postgres:18\n    ports:\n      - \"${WT_PORT_DB:-5432}:5432\"\n  redis:\n    image: redis:8\n    ports:\n      - \"${WT_PORT_REDIS:-6379}:6379\"\n",
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
        assert_eq!(left.contents, right.contents, "init must be deterministic");
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
        .find(|file| file.path.ends_with("magictree.toml") && !file.contents.contains("[app]"))
        .expect("workspace manifest");
    assert!(workspace.contents.contains("[workspace]"));
    assert!(workspace
        .contents
        .contains("apps = [\"apps/api\", \"apps/web\"]"));
    // Shared infrastructure is declared once, at the workspace level, and is not
    // published to the host.
    assert!(workspace.contents.contains("id = \"postgres\""));
    assert!(workspace.contents.contains("expose = \"none\""));

    let web = planned
        .iter()
        .find(|file| file.path.ends_with("apps/web/magictree.toml"))
        .expect("web manifest");
    assert!(web.contents.contains("kind = \"pnpm\", script = \"dev\""));
    assert!(
        !web.contents.contains("needs"),
        "shared infrastructure is implicit, so app manifests stay free of boilerplate:\n{}",
        web.contents
    );
    assert!(web
        .contents
        .contains("inputs = [\"pnpm-lock.yaml\", \"package.json\"]"));
}

#[test]
fn answers_change_the_output_predictably() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");

    let mut answers: AnswerSet = answers_for(&report);
    answers.answers.insert(
        "compose.expose".to_string(),
        Answer::Many(vec!["postgres".to_string()]),
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
        .find(|file| file.contents.contains("[workspace]"))
        .expect("workspace manifest");
    assert!(
        workspace
            .contents
            .contains("port = { target = 5432, env = \"WT_PORT_DB\" }"),
        "an exposed service keeps a host port and the variable compose derives from:\n{}",
        workspace.contents
    );
    assert!(workspace.contents.contains("apps = [\"apps/web\"]"));

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
        .find(|file| file.contents.contains("[workspace]"))
        .unwrap();
    assert!(
        !workspace.contents.contains("port = {"),
        "nothing is published unless asked for:\n{}",
        workspace.contents
    );
}

#[test]
fn apply_refuses_to_overwrite_without_force() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let planned = plan(&report, &answers_for(&report), fixture.path()).expect("plan");

    let written = apply(&planned, false).expect("first write");
    assert_eq!(written.len(), planned.len());

    let error = apply(&planned, false).expect_err("second write must refuse");
    assert!(error.to_string().contains("--force"), "{error}");

    apply(&planned, true).expect("forced overwrite");
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
    assert!(position("postgres") < position("api:api"));
    assert!(position("redis") < position("web:web"));
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
    let manifest = &planned[0].contents;
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
    let manifest = &planned[0].contents;
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
    let manifest = &planned[0].contents;
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
  zitadel:
    image: ghcr.io/zitadel/zitadel:latest
    ports:
      - "${WT_PORT_ZITADEL:-8080}:8080"
      - "${WT_PORT_ZITADEL_LOGIN:-3100}:3000"
  minio:
    image: minio/minio
    ports:
      - "${WT_PORT_MINIO:-9000}:9000"
"#,
    );
    fixture.write("package.json", r#"{"name":"solo"}"#);
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let mut answers = answers_for(&report);
    answers.answers.insert(
        "compose.expose".to_string(),
        Answer::Many(vec!["zitadel".to_string(), "minio".to_string()]),
    );
    let planned = plan(&report, &answers, fixture.path()).expect("plan");
    let manifest = &planned[0].contents;

    assert!(
        manifest.contains("env = \"WT_PORT_ZITADEL\""),
        "the published port must also drive the compose variable:\n{manifest}"
    );
    assert!(
        manifest.contains("env = \"WT_PORT_ZITADEL_LOGIN\""),
        "{manifest}"
    );
    // A single-port service keeps the plain form, still carrying its variable.
    assert!(
        manifest.contains("port = { target = 9000, env = \"WT_PORT_MINIO\" }"),
        "{manifest}"
    );
    // Multi-port services get readable names derived from the variables.
    assert!(manifest.contains("name = \"zitadel\""), "{manifest}");
    assert!(
        manifest.contains("name = \"zitadel_login\""),
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
  postgres:
    image: postgres:18
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
    let manifest = &planned[0].contents;

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
    assert!(!planned[0].contents.contains("[workspace]"));
    assert!(planned[0].contents.contains("[app]"));
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
