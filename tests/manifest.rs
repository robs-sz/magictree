//! Manifest resolution, dependency closure, and validation.

mod support;

use magictree::manifest::{dependencies, nodes, order, Loaded, Runtime, Wait};
use support::Fixture;

fn single_app() -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "db"
runtime = "compose"
compose = { file = "compose.yml", service = "db" }
expose = "none"

[[services]]
id = "api"
command = "python3 -m http.server $PORT"
port = { env = "PORT" }
needs = ["db"]

[[services]]
id = "web"
command = "python3 -m http.server $PORT"
port = { env = "PORT" }
needs = ["api"]
"#,
    );
    fixture.git_repo();
    fixture
}

#[test]
fn resolves_needs_into_dependency_order() {
    let fixture = single_app();
    let loaded = Loaded::load(fixture.path()).expect("load");
    loaded.validate().expect("valid");

    let all = nodes(&loaded);
    let edges = dependencies(&all).expect("resolve needs");
    let ordered = order(&all, &edges, &(0..all.len()).collect::<Vec<_>>()).expect("order");

    let names: Vec<String> = ordered.iter().map(|index| all[*index].id.clone()).collect();
    assert_eq!(names, vec!["db", "api", "web"]);
}

#[test]
fn selecting_one_service_pulls_in_its_closure() {
    let fixture = single_app();
    let loaded = Loaded::load(fixture.path()).expect("load");
    let all = nodes(&loaded);
    let edges = dependencies(&all).expect("resolve");
    let web = all.iter().position(|node| node.id == "web").expect("web");

    let ordered = order(&all, &edges, &[web]).expect("order");
    let names: Vec<String> = ordered.iter().map(|index| all[*index].id.clone()).collect();
    assert_eq!(names, vec!["db", "api", "web"]);
}

#[test]
fn dependency_cycles_are_reported_with_the_cycle() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "a"
command = "sleep 1"
needs = ["b"]

[[services]]
id = "b"
command = "sleep 1"
needs = ["a"]
"#,
    );
    fixture.git_repo();
    let loaded = Loaded::load(fixture.path()).expect("load");
    let all = nodes(&loaded);
    let edges = dependencies(&all).expect("resolve");

    let error = order(&all, &edges, &(0..all.len()).collect::<Vec<_>>()).expect_err("cycle");
    let message = error.to_string();
    assert!(message.contains("cycle"), "{message}");
    assert!(message.contains('a') && message.contains('b'), "{message}");
}

#[test]
fn unknown_dependency_names_the_service_and_the_reference() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 1\"\nneeds = [\"ghost\"]\n",
    );
    fixture.git_repo();
    let loaded = Loaded::load(fixture.path()).expect("load");
    let error = dependencies(&nodes(&loaded)).expect_err("must fail");
    let message = error.to_string();
    assert!(
        message.contains("web") && message.contains("ghost"),
        "{message}"
    );
}

#[test]
fn runtime_defaults_from_the_presence_of_a_compose_reference() {
    let fixture = single_app();
    let loaded = Loaded::load(fixture.path()).expect("load");
    let all = nodes(&loaded);

    let db = all.iter().find(|node| node.id == "db").unwrap();
    assert_eq!(db.runtime(), Some(Runtime::Compose));
    let api = all.iter().find(|node| node.id == "api").unwrap();
    assert_eq!(api.runtime(), Some(Runtime::Host));

    // A host service exposes the command its target or command field implies.
    assert!(api.command().unwrap().contains("http.server"));
}

#[test]
fn validation_rejects_incomplete_services() {
    let cases = [
        (
            "host service without a command",
            "version = 1\n\n[[services]]\nid = \"web\"\nruntime = \"host\"\n",
        ),
        (
            "compose service without a reference",
            "version = 1\n\n[[services]]\nid = \"db\"\nruntime = \"compose\"\n",
        ),
        (
            "duplicate service id",
            "version = 1\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 1\"\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 2\"\n",
        ),
        (
            "reserved environment key",
            "version = 1\n\n[env]\nMAGICTREE_SLUG = \"x\"\n",
        ),
        (
            "uv target without a script or module",
            "version = 1\n\n[[services]]\nid = \"api\"\ntarget = { kind = \"uv\" }\n",
        ),
        (
            "both prefer and require",
            "version = 1\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 1\"\nport = { prefer = 3000, require = 3001 }\n",
        ),
        (
            "reserved port.env variable",
            "version = 1\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 1\"\nport = { env = \"MAGICTREE_SLUG\" }\n",
        ),
    ];

    for (label, contents) in cases {
        let fixture = Fixture::new();
        fixture.write("magictree.toml", contents);
        fixture.git_repo();
        let loaded = Loaded::load(fixture.path()).expect("parse");
        let error = loaded
            .validate()
            .expect_err(&format!("{label} must be rejected"));
        assert!(!error.to_string().is_empty(), "{label}");
    }
}

#[test]
fn unsupported_manifest_version_is_rejected() {
    let fixture = Fixture::new();
    fixture.write("magictree.toml", "version = 99\n");
    fixture.git_repo();
    let loaded = Loaded::load(fixture.path()).expect("parse");
    let error = loaded.validate().expect_err("must reject");
    assert!(error.to_string().contains("version"), "{error}");
}

#[test]
fn workspace_lists_apps_and_resolves_qualified_dependencies() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[workspace]
apps = ["apps/api", "apps/web"]

[[services]]
id = "db"
runtime = "compose"
compose = { file = "compose.yml", service = "db" }
expose = "none"
"#,
    );
    fixture.write(
        "apps/api/magictree.toml",
        r#"
version = 1
[app]
id = "api"

[[services]]
id = "api"
command = "sleep 100"
needs = ["db"]
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
command = "sleep 100"
needs = ["api:api"]
"#,
    );
    fixture.write(
        "compose.yml",
        "services:\n  db:\n    image: example/db:18\n",
    );
    fixture.git_repo();

    let loaded = Loaded::load(&fixture.join("apps/web")).expect("load from inside an app");
    loaded.validate().expect("valid");
    assert_eq!(loaded.current_app.as_deref(), Some("web"));

    let all = nodes(&loaded);
    let edges = dependencies(&all).expect("qualified needs resolve");
    let web = all
        .iter()
        .position(|node| node.qual() == "web:web")
        .expect("web");
    let ordered = order(&all, &edges, &[web]).expect("order");
    let names: Vec<String> = ordered.iter().map(|index| all[*index].qual()).collect();
    assert_eq!(names, vec!["db", "api:api", "web:web"]);
    assert_eq!(
        all.iter().find(|node| node.qual() == "db").unwrap().app,
        None
    );
}

#[test]
fn workspace_order_puts_infrastructure_first_without_explicit_needs() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[workspace]
apps = ["apps/web"]

[[services]]
id = "db"
runtime = "compose"
compose = { file = "compose.yml", service = "db" }
expose = "none"

[[services]]
id = "cache"
runtime = "compose"
compose = { file = "compose.yml", service = "cache" }
expose = "none"
"#,
    );
    fixture.write(
        "apps/web/magictree.toml",
        "version = 1\n[app]\nid = \"web\"\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 100\"\n",
    );
    fixture.git_repo();
    let loaded = Loaded::load(&fixture.join("apps/web")).expect("load");
    let all = nodes(&loaded);
    let edges = dependencies(&all).expect("resolve");
    let ordered = order(&all, &edges, &(0..all.len()).collect::<Vec<_>>()).expect("order");
    let names: Vec<String> = ordered.iter().map(|index| all[*index].qual()).collect();

    assert_eq!(names, vec!["db", "cache", "web:web"]);
}

#[test]
fn selecting_a_single_app_service_still_brings_shared_infrastructure() {
    // The rule lives in Ctx::scope, which needs a real repository; this test
    // asserts the manifest-level invariant it relies on: workspace services are
    // reachable from any app scope.
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[workspace]
apps = ["apps/web"]

[[services]]
id = "db"
runtime = "compose"
compose = { file = "compose.yml", service = "db" }
expose = "none"
"#,
    );
    fixture.write(
        "apps/web/magictree.toml",
        "version = 1\n[app]\nid = \"web\"\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 100\"\n",
    );
    fixture.git_repo();

    let loaded = Loaded::load(&fixture.join("apps/web")).expect("load");
    let all = nodes(&loaded);
    let workspace: Vec<&str> = all
        .iter()
        .filter(|node| node.app.is_none())
        .map(|node| node.id.as_str())
        .collect();
    assert_eq!(
        workspace,
        vec!["db"],
        "workspace services are app-independent"
    );

    // Workspace nodes sort first, so an app scope sees them before its own work.
    assert_eq!(all[0].qual(), "db");
}

#[test]
fn wait_defaults_to_running_and_can_be_set_to_exit() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "server"
runtime = "compose"
compose = { file = "compose.yml", service = "server" }
expose = "none"

[[services]]
id = "migrate-db"
runtime = "compose"
compose = { file = "compose.yml", service = "migrate-db" }
expose = "none"
wait = "exit"
"#,
    );
    fixture.git_repo();
    let loaded = Loaded::load(fixture.path()).expect("load");
    loaded.validate().expect("valid");
    let all = nodes(&loaded);

    let server = all.iter().find(|n| n.id == "server").unwrap();
    assert_eq!(server.service().unwrap().wait, Wait::Running);
    let migration = all.iter().find(|n| n.id == "migrate-db").unwrap();
    assert_eq!(migration.service().unwrap().wait, Wait::Exit);
}

#[test]
fn every_process_carries_every_declared_port_variable() {
    // A declared `port.env` names a variable the whole stack publishes: compose
    // interpolates it wherever it is declared, and host tooling reads it. So
    // every process — and `magictree env` — sees the stack's whole set.
    use magictree::manifest::Loaded;
    use magictree::paths::Paths;
    use magictree::ports::Assignment;
    use std::collections::BTreeMap;

    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1
[workspace]
apps = ["apps/web"]

[[services]]
id = "auth"
runtime = "compose"
compose = { file = "compose.yml", service = "auth" }
ports = [
  { name = "auth", target = 8080, env = "WT_PORT_AUTH" },
  { name = "auth_login", target = 3000, env = "WT_PORT_AUTH_LOGIN" },
]

[[services]]
id = "stack-init"
runtime = "compose"
compose = { file = "compose.yml", service = "stack-init" }
expose = "none"
wait = "exit"
"#,
    );
    fixture.write(
        "apps/web/magictree.toml",
        "version = 1\n[app]\nid = \"web\"\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 1\"\nport = { env = \"WEB_PORT\" }\n",
    );
    fixture.git_repo();

    let mut paths_env = BTreeMap::new();
    paths_env.insert("MAGICTREE_SLUG".to_string(), "wt".to_string());
    let mut ports = BTreeMap::new();
    // The service's first port keeps the plain name; later ones are qualified,
    // matching what `magictree ports` reports.
    ports.insert("auth".to_string(), 24283u16);
    ports.insert("auth:auth_login".to_string(), 24284u16);
    ports.insert("web:web".to_string(), 24286u16);
    let assignment = Assignment {
        version: magictree::ports::ASSIGNMENT_VERSION,
        repo_key: "repo".to_string(),
        worktree_id: "wt".to_string(),
        worktree_path: None,
        base: 24280,
        mode: magictree::ports::PortMode::Block,
        ports,
    };

    let state = fixture.state_dir();
    let paths = Paths {
        state_dir: state.clone(),
        config_dir: state,
    };
    let loaded = Loaded::load(fixture.path()).expect("loaded");
    let all = magictree::manifest::nodes(&loaded);
    let edges = magictree::manifest::dependencies(&all).expect("dependencies");
    let ctx = magictree::Ctx {
        paths,
        config: magictree::Config::default(),
        repo: magictree::Repo::open(fixture.path()).expect("repo"),
        runtime_dir: fixture.join("runtime"),
        loaded,
        nodes: all.clone(),
        edges,
        slug: "wt".to_string(),
    };
    let stack_init = all
        .iter()
        .find(|n| n.id == "stack-init")
        .expect("stack-init");
    let env = ctx.node_env(stack_init, &assignment).expect("env");

    assert_eq!(
        env.get("WT_PORT_AUTH").map(String::as_str),
        Some("24283"),
        "a compose service must see ports declared by other services: {env:?}"
    );
    assert_eq!(
        env.get("WT_PORT_AUTH_LOGIN").map(String::as_str),
        Some("24284")
    );

    // A host service owns its own variable and also sees its peers', so host
    // tooling cannot silently fall back to a port the stack never published.
    let web = all
        .iter()
        .find(|n| n.id == "web" && n.app.is_some())
        .expect("web");
    let web_env = ctx.node_env(web, &assignment).expect("env");
    assert_eq!(web_env.get("WEB_PORT").map(String::as_str), Some("24286"));
    assert_eq!(
        web_env.get("WT_PORT_AUTH").map(String::as_str),
        Some("24283"),
        "a host process must see the stack's declared port variables: {web_env:?}"
    );
    assert_eq!(
        web_env.get("WT_PORT_AUTH_LOGIN").map(String::as_str),
        Some("24284")
    );
}

#[test]
fn an_app_missing_from_the_workspace_list_is_rejected() {
    let fixture = Fixture::new();
    fixture.write("magictree.toml", "version = 1\n[workspace]\napps = []\n");
    fixture.write(
        "apps/ghost/magictree.toml",
        "version = 1\n[app]\nid = \"ghost\"\n",
    );
    fixture.git_repo();

    let error = Loaded::load(&fixture.join("apps/ghost")).expect_err("must reject");
    assert!(error.to_string().contains("ghost"), "{error}");
}

#[test]
fn missing_manifest_is_a_clear_error() {
    let fixture = Fixture::new();
    fixture.git_repo();
    let error = Loaded::load(fixture.path()).expect_err("must fail");
    assert!(error.to_string().contains("magictree.toml"), "{error}");
}

#[test]
fn jobs_run_on_up_only_when_declared() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "db"
runtime = "compose"
compose = { file = "compose.yml", service = "db" }
expose = "none"

[jobs.migrate]
run = "echo migrate"
when = "up"
needs = ["db"]

[jobs.seed]
run = "echo seed"
when = "manual"
"#,
    );
    fixture.git_repo();
    let loaded = Loaded::load(fixture.path()).expect("load");
    let all = nodes(&loaded);
    let job = all
        .iter()
        .find(|node| node.id == "migrate")
        .expect("job node");
    assert!(job.job().is_some());
    assert!(!job.is_running_service());
    assert_eq!(job.needs(), ["db".to_string()]);
}
