//! Discovery: what the extractors determine, and what they refuse to guess.

mod support;

use magictree::discover::extract;
use magictree::discover::report::{FactData, FactKind, UnknownKind};
use support::Fixture;

const COMPOSE: &str = r#"
services:
  db:
    image: example/db:18
    ports:
      - "${WT_PORT_DB:-5432}:5432"
    healthcheck:
      test: ["CMD-SHELL", "true"]
  cache:
    image: example/cache:8
    ports:
      - "${WT_PORT_CACHE:-6379}:6379"
  worker:
    build: ./deployment/worker
    depends_on:
      db:
        condition: service_healthy
"#;

fn monorepo() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("pnpm-workspace.yaml", "packages:\n  - \"apps/*\"\n");
    fixture.write("compose.yaml", COMPOSE);
    fixture.write(
        "apps/web/package.json",
        r#"{"name":"web","packageManager":"pnpm@9","scripts":{"dev":"vite dev","test":"vitest"}}"#,
    );
    fixture.write("apps/web/pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.write(
        "apps/api/pyproject.toml",
        "[project]\nname = \"api\"\n[project.scripts]\nserve = \"api.main:run\"\n",
    );
    fixture.write("apps/api/uv.lock", "version = 1\n");
    fixture.write("justfile", "set shell := [\"bash\"]\n\ndev:\n  echo dev\n");
    fixture.write(".env.example", "DATABASE_URL=\nCACHE_URL=\n");
    fixture.git_repo();
    fixture
}

#[test]
fn finds_apps_from_workspace_globs() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");

    let dirs: Vec<&str> = report.apps.iter().map(|app| app.dir.as_str()).collect();
    assert_eq!(dirs, vec!["apps/api", "apps/web"]);
    assert_eq!(report.apps[0].source, "pnpm");
}

#[test]
fn extracts_compose_services_with_ports_and_dependencies() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");

    let compose = report
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::Compose)
        .expect("compose fact");
    let FactData::Compose {
        services,
        has_build,
        ..
    } = &compose.data
    else {
        panic!("expected compose data");
    };
    let names: Vec<&str> = services
        .iter()
        .map(|service| service.name.as_str())
        .collect();
    assert_eq!(names, vec!["cache", "db", "worker"]);

    let db = services.iter().find(|s| s.name == "db").unwrap();
    assert_eq!(db.ports, vec!["${WT_PORT_DB:-5432}:5432"]);
    assert!(db.has_healthcheck);
    assert!(!db.has_build);

    let worker = services.iter().find(|s| s.name == "worker").unwrap();
    assert!(worker.has_build);
    assert_eq!(worker.depends_on, vec!["db"]);

    assert!(*has_build, "at least one service builds from source");
}

#[test]
fn records_language_facts_per_app() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");

    let web = report
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::Node && fact.app.as_deref() == Some("web"))
        .expect("node fact");
    let FactData::Node {
        scripts,
        package_manager,
        ..
    } = &web.data
    else {
        panic!("expected node data");
    };
    assert_eq!(package_manager.as_deref(), Some("pnpm@9"));
    assert!(scripts.iter().any(|script| script.name == "dev"));

    let api = report
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::Python && fact.app.as_deref() == Some("api"))
        .expect("python fact");
    let FactData::Python {
        manager, scripts, ..
    } = &api.data
    else {
        panic!("expected python data");
    };
    assert_eq!(manager, "uv");
    assert_eq!(scripts, &vec!["serve".to_string()]);
}

#[test]
fn asks_about_what_it_cannot_know() {
    let fixture = monorepo();
    let report = extract(fixture.path()).expect("extract");
    let ids: Vec<&str> = report.unknowns.iter().map(|u| u.id.as_str()).collect();

    assert!(
        ids.contains(&"stack.members"),
        "multi-app needs a stack choice"
    );
    assert!(
        ids.contains(&"compose.shared"),
        "infra vs app-owned is a choice"
    );
    assert!(ids.contains(&"compose.expose"));
    assert!(ids.contains(&"web.run"), "how to run an app is a choice");
    assert!(ids.contains(&"api.run"));

    // The compose file describes the stack, so every service is managed by
    // default. A repository can deselect the ones an app manifest covers.
    let shared = report
        .unknowns
        .iter()
        .find(|u| u.id == "compose.shared")
        .expect("compose.shared");
    assert_eq!(shared.kind, UnknownKind::MultiChoice);
    assert_eq!(
        shared.default.as_deref(),
        Some("cache,db,worker"),
        "services built from source are part of the stack too"
    );
    assert!(shared.options.contains(&"worker".to_string()));

    // Nothing is exposed unless something outside the network needs it.
    let expose = report
        .unknowns
        .iter()
        .find(|u| u.id == "compose.expose")
        .expect("compose.expose");
    assert_eq!(expose.default.as_deref(), Some(""));
}

#[test]
fn run_options_come_only_from_files_that_exist() {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"solo","scripts":{"dev":"node server.js","lint":"eslint"}}"#,
    );
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let run = report
        .unknowns
        .iter()
        .find(|u| u.id.ends_with(".run"))
        .expect("a run unknown");
    let values = &run.options;
    assert!(values.contains(&"npm:dev".to_string()));
    assert!(
        !values.iter().any(|value| value.contains("lint")),
        "a linter is not a way to start the app: {values:?}"
    );
    assert_eq!(run.default.as_deref(), Some("npm:dev"));
    assert!(
        values.contains(&"skip".to_string()),
        "opting out is always possible"
    );
    let _ = run.kind;
}

#[test]
fn asks_which_variable_carries_an_apps_port() {
    // A service that reads its port from a specific variable ignores PORT, so
    // the generated manifest must name that variable or health checks fail.
    let fixture = Fixture::new();
    fixture.write("pnpm-workspace.yaml", "packages:\n  - \"apps/*\"\n");
    fixture.write(
        "apps/api/justfile",
        "wt_port_api := env(\"WT_PORT_API\", \"8000\")\nwt_mode := env(\"WT_MODE\", \"\")\n\ndev $APP__SERVER__PORT=wt_port_api:\n  uv run uvicorn app:main --port 8000\n",
    );
    fixture.write("apps/api/pyproject.toml", "[project]\nname = \"api\"\n");
    fixture.write("apps/api/uv.lock", "version = 1\n");
    fixture.write(
        "apps/web/package.json",
        r#"{"name":"web","packageManager":"pnpm@9","scripts":{"dev":"vite dev"}}"#,
    );
    fixture.write("apps/web/pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let api = report
        .unknowns
        .iter()
        .find(|u| u.id == "api.port_env")
        .expect("the api names its own port variable");
    assert!(
        api.options.contains(&"WT_PORT_API".to_string()),
        "{:?}",
        api.options
    );
    // The recipe's exported parameter is handed to the process, not read from
    // the environment, so it can never carry an allocated port.
    assert!(
        !api.options.contains(&"APP__SERVER__PORT".to_string()),
        "a variable the justfile sets for a recipe must not be offered: {:?}",
        api.options
    );
    assert_eq!(api.default.as_deref(), Some("WT_PORT_API"));

    let ids: Vec<&str> = report.unknowns.iter().map(|u| u.id.as_str()).collect();
    assert!(
        !ids.contains(&"web.port_env"),
        "an app that names no port variable is not asked about: {ids:?}"
    );
}

#[test]
fn offers_a_direct_command_recovered_from_a_task_runner_recipe() {
    // The point: run uv/pnpm without going through the repository's task runner.
    let fixture = Fixture::new();
    fixture.write(
        "justfile",
        concat!(
            "svc_port := env(\"WT_PORT_SVC\", \"8000\")\n",
            "mode := env(\"MODE\", \"dev\")\n",
            "\n",
            "dev $MY_PORT=svc_port:\n",
            "  {{ if mode == \"ui\" { error(\"nope\") } else { \"\" } }}\n",
            "  uv run uvicorn app.main:app --reload --host 0.0.0.0 --port {{svc_port}} --no-access-log\n",
        ),
    );
    fixture.write("package.json", r#"{"name":"solo"}"#);
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let run = report
        .unknowns
        .iter()
        .find(|u| u.id.ends_with(".run"))
        .expect("a run question");
    let direct = run
        .options
        .iter()
        .find(|option| option.starts_with("command:"))
        .unwrap_or_else(|| panic!("no direct command offered: {:?}", run.options));
    assert!(
        direct.contains("uv run uvicorn app.main:app"),
        "the command comes from the recipe: {direct}"
    );
    assert!(
        direct.contains("--port $WT_PORT_SVC"),
        "the port variable is substituted, not left as a template: {direct}"
    );
    assert!(
        !direct.contains("error("),
        "literal guards are dropped: {direct}"
    );
    assert!(
        !direct.contains("{{"),
        "no unresolved template remains: {direct}"
    );
}

#[test]
fn does_not_offer_a_direct_command_that_a_native_runner_already_covers() {
    let fixture = Fixture::new();
    fixture.write("justfile", "dev:\n  pnpm dev\n");
    fixture.write(
        "package.json",
        r#"{"name":"solo","packageManager":"pnpm@9","scripts":{"dev":"vite dev"}}"#,
    );
    fixture.write("pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let run = report
        .unknowns
        .iter()
        .find(|u| u.id.ends_with(".run"))
        .expect("a run question");
    assert!(
        !run.options
            .iter()
            .any(|option| option.starts_with("command:")),
        "`pnpm dev` is already expressible as a pnpm target: {:?}",
        run.options
    );
}

#[test]
fn report_hash_is_stable_and_content_dependent() {
    let fixture = monorepo();
    let first = extract(fixture.path()).expect("extract");
    let second = extract(fixture.path()).expect("extract");
    assert_eq!(first.report_hash, second.report_hash);
    assert!(first.report_hash.starts_with("sha256:"));

    fixture.write(
        "apps/web/package.json",
        r#"{"name":"web","scripts":{"dev":"vite dev","preview":"vite preview"}}"#,
    );
    let changed = extract(fixture.path()).expect("extract");
    assert_ne!(
        first.report_hash, changed.report_hash,
        "adding a script must change the report fingerprint"
    );
}

#[test]
fn single_app_repository_needs_no_workspace_layer() {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"solo","scripts":{"dev":"vite"}}"#,
    );
    fixture.write(
        "compose.yaml",
        "services:\n  db:\n    image: example/db:18\n",
    );
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    assert_eq!(report.apps.len(), 1);
    assert_eq!(report.apps[0].dir, ".");
    let ids: Vec<&str> = report.unknowns.iter().map(|u| u.id.as_str()).collect();
    assert!(
        !ids.contains(&"stack.members"),
        "one app needs no stack membership question"
    );
}

#[test]
fn discovery_reads_nothing_it_cannot_parse() {
    let fixture = Fixture::new();
    // Malformed compose and package.json must not fail discovery outright.
    fixture.write("compose.yaml", "services: [this is not valid\n");
    fixture.write("package.json", "{ not json");
    fixture.write("justfile", "dev:\n  echo hi\n");
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract still succeeds");

    assert!(
        report
            .facts
            .iter()
            .all(|fact| fact.kind != FactKind::Compose),
        "unparseable compose contributes no facts"
    );
    assert!(report.facts.iter().any(|fact| fact.kind == FactKind::Just));
}

#[test]
fn compose_files_in_infra_directories_are_found() {
    let fixture = Fixture::new();
    fixture.write(
        "infra/compose.yml",
        "services:\n  db:\n    image: example/db:18\n",
    );
    fixture.write("package.json", r#"{"name":"solo"}"#);
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let compose = report
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::Compose)
        .expect("compose fact from infra/");
    assert_eq!(compose.source, "infra/compose.yml");
}

#[test]
fn a_compose_file_one_level_inside_infra_is_found() {
    let fixture = Fixture::new();
    fixture.write(
        "infra/local/docker-compose.yml",
        "services:\n  db:\n    image: example/db:18\n    ports:\n      - \"5433:5432\"\n",
    );
    fixture.write("package.json", r#"{"name":"solo"}"#);
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    let compose = report
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::Compose)
        .expect("compose fact from infra/local/");
    assert_eq!(compose.source, "infra/local/docker-compose.yml");
    let FactData::Compose { services, .. } = &compose.data else {
        panic!("expected compose data");
    };
    assert_eq!(services[0].name, "db");
}

#[test]
fn records_ports_pinned_in_scripts_and_env_templates() {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"app","scripts":{"dev":"next dev --turbo -p 3005","seed":"tsx scripts/seed.ts --port 5433"}}"#,
    );
    fixture.write(".env.example", "DATABASE_PORT=\"5433\"\n");
    fixture.git_repo();

    let report = extract(fixture.path()).expect("extract");

    let node = report
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::Node)
        .expect("node fact");
    let FactData::Node { ports, .. } = &node.data else {
        panic!("expected node data");
    };
    let dev = ports
        .iter()
        .find(|literal| literal.container.as_deref() == Some("dev"))
        .expect("the dev script pins a port");
    assert_eq!(dev.port, 3005);
    assert_eq!(dev.literal, "-p 3005");
    assert!(ports.iter().any(|literal| literal.port == 5433));

    let env = report
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::EnvExample)
        .expect("env fact");
    let FactData::EnvExample { ports, .. } = &env.data else {
        panic!("expected env data");
    };
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].container.as_deref(), Some("DATABASE_PORT"));
    assert_eq!(ports[0].port, 5433);
}

#[test]
fn a_script_that_reads_a_variable_pins_nothing() {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"app","scripts":{"dev":"next dev --turbo -p ${APP_PORT:-3005}"}}"#,
    );
    fixture.write(".env.example", "${WT_PORT_DB:-5432}:5432\n");
    fixture.git_repo();

    let report = extract(fixture.path()).expect("extract");
    let facts: Vec<&magictree::discover::report::Fact> = report.facts.iter().collect();
    assert!(
        magictree::discover::ports::all(&facts).is_empty(),
        "an expansion is not a literal"
    );
}

#[test]
fn finds_storybook_and_offers_the_script_that_serves_it() {
    let fixture = monorepo();
    fixture.write(
        "apps/web/package.json",
        r#"{"name":"web","packageManager":"pnpm@9","scripts":{"dev":"vite dev","storybook":"storybook dev -p 6006","build-storybook":"storybook build"},"devDependencies":{"@storybook/react-vite":"8.6.14","@storybook/addon-mcp":"0.1.0"}}"#,
    );
    fixture.write(
        "apps/web/.storybook/main.ts",
        "export default { stories: [] };\n",
    );
    let report = extract(fixture.path()).expect("extract");

    let fact = report
        .facts
        .iter()
        .find(|fact| fact.kind == FactKind::Storybook && fact.app.as_deref() == Some("web"))
        .expect("a storybook fact");
    assert_eq!(fact.source, "apps/web/.storybook/main.ts");
    let FactData::Storybook {
        config_dir,
        dev_scripts,
        has_mcp,
    } = &fact.data
    else {
        panic!("expected storybook data");
    };
    assert_eq!(config_dir.as_deref(), Some(".storybook"));
    assert_eq!(dev_scripts, &vec!["pnpm:storybook".to_string()]);
    assert!(has_mcp, "the addon that answers MCP is installed");

    // Storybook runs beside the app, so the app keeps its own run question.
    let question = report
        .unknowns
        .iter()
        .find(|unknown| unknown.id == "web.storybook")
        .expect("a storybook question");
    assert_eq!(question.kind, UnknownKind::Choice);
    assert_eq!(question.options, vec!["skip", "pnpm:storybook"]);
    assert_eq!(question.default.as_deref(), Some("pnpm:storybook"));
    assert!(report
        .unknowns
        .iter()
        .any(|unknown| unknown.id == "web.run"));
    assert!(
        !report
            .unknowns
            .iter()
            .any(|unknown| unknown.id == "api.storybook"),
        "an app without Storybook is not asked about it"
    );
}

#[test]
fn asks_nothing_about_storybook_without_the_tool() {
    // A script name alone is not evidence: the app has to declare Storybook or
    // read its configuration before it is offered a service for it.
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"app","scripts":{"storybook":"./scripts/not-storybook.sh"}}"#,
    );
    fixture.git_repo();
    let report = extract(fixture.path()).expect("extract");

    assert!(
        !report
            .facts
            .iter()
            .any(|fact| fact.kind == FactKind::Storybook),
        "nothing indicates Storybook here"
    );
    assert!(
        !report
            .unknowns
            .iter()
            .any(|unknown| unknown.id.ends_with(".storybook")),
        "the app must not be asked about a tool it does not use"
    );
}
