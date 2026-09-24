//! End-to-end CLI behaviour through the real binary.
//!
//! These tests avoid a Docker daemon so they run anywhere. Compose startup uses
//! a fake CLI for command and ordering assertions; daemon-backed cleanup tests
//! live in `tests/compose.rs`.

mod support;

use std::path::Path;
use std::time::{Duration, Instant};
use support::{free_port, have, run, runtime_dirs, Fixture};

/// A host-process stack. Bootstrap writes into the state dir on purpose: a
/// checkout must come out of a run with nothing new in it.
fn host_stack_fixture() -> Fixture {
    let fixture = Fixture::new();
    let marker = fixture.state_dir().join("boot.txt");
    fixture.write(
        "magictree.toml",
        &format!(
            r#"
version = 1

[bootstrap]
run = ["echo bootstrapped > '{}'"]

[[services]]
id = "idle"
command = "sleep 300"
port = {{ env = "PORT" }}
"#,
            marker.display()
        ),
    );
    fixture.git_repo();
    fixture
}

#[test]
fn dry_run_creates_nothing() {
    let fixture = host_stack_fixture();
    let state = fixture.state_dir();

    let result = run(&["--dry-run", "up"], fixture.path(), &state);
    assert!(result.ok(), "{}", result.combined());
    assert!(result.stdout.contains("dry run"));

    assert!(
        !fixture.join(".magictree").exists(),
        "a dry run must not create the runtime directory"
    );
    assert!(
        runtime_dirs(&state).is_empty(),
        "a dry run must not claim any worktree state"
    );
    assert_eq!(
        std::fs::read_dir(state.join("blocks"))
            .map(|e| e.flatten().count())
            .unwrap_or(0),
        0,
        "a dry run must not claim a port block"
    );
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(fixture.path())
        .output()
        .expect("git status");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "",
        "a dry run must leave the checkout untouched"
    );
}

#[test]
fn up_status_ports_down_round_trip() {
    let fixture = host_stack_fixture();
    let state = fixture.state_dir();

    let up = run(&["up"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());
    assert!(up.stdout.contains("idle: started"), "{}", up.stdout);

    // Runtime state lives in magictree's own state dir, so the checkout is
    // untouched by a run and there is nothing to keep out of git status.
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(fixture.path())
        .output()
        .expect("git status");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "",
        "a run must leave the checkout untouched"
    );
    let runtime = runtime_dirs(&state);
    assert_eq!(runtime.len(), 1, "one worktree owns state: {runtime:?}");
    assert!(runtime[0].join("ports.json").exists());
    assert!(
        fixture.state_dir().join("boot.txt").exists(),
        "bootstrap ran"
    );

    let status_cmd = run(&["status"], fixture.path(), &state);
    assert!(status_cmd.ok(), "{}", status_cmd.combined());
    assert!(
        status_cmd.stdout.contains("running"),
        "{}",
        status_cmd.stdout
    );

    let ports = run(&["ports"], fixture.path(), &state);
    assert!(ports.ok(), "{}", ports.combined());
    assert!(ports.stdout.contains("block"), "{}", ports.stdout);

    let port = assigned_port(&ports.stdout, "idle");
    let local_url = format!("http://localhost:{port}");
    assert!(up.stdout.contains(&local_url), "{}", up.stdout);
    assert!(
        status_cmd.stdout.contains(&local_url),
        "{}",
        status_cmd.stdout
    );
    assert!(ports.stdout.contains(&local_url), "{}", ports.stdout);
    assert!(
        !ports.stdout.contains(".localhost:"),
        "aliases are opt-in: {}",
        ports.stdout
    );

    let down = run(&["down"], fixture.path(), &state);
    assert!(down.ok(), "{}", down.combined());

    let after = run(&["status"], fixture.path(), &state);
    assert!(after.stdout.contains("stopped"), "{}", after.stdout);
}

#[test]
fn ports_survive_a_down_up_cycle() {
    let fixture = host_stack_fixture();
    let state = fixture.state_dir();

    assert!(run(&["up"], fixture.path(), &state).ok());
    let first = run(&["ports"], fixture.path(), &state).stdout;
    assert!(run(&["down"], fixture.path(), &state).ok());
    assert!(run(&["up"], fixture.path(), &state).ok());
    let second = run(&["ports"], fixture.path(), &state).stdout;

    let port_of = |text: &str| -> String {
        text.lines()
            .find(|line| line.contains("idle"))
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_string()
    };
    assert!(
        !port_of(&first).is_empty(),
        "first run assigned a port: {first}"
    );
    assert_eq!(port_of(&first), port_of(&second), "ports are stable");

    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn up_is_idempotent() {
    let fixture = host_stack_fixture();
    let state = fixture.state_dir();

    assert!(run(&["up"], fixture.path(), &state).ok());
    let again = run(&["up"], fixture.path(), &state);
    assert!(again.ok(), "{}", again.combined());
    assert!(
        again.stdout.contains("already running"),
        "a second up must not start a duplicate process:\n{}",
        again.stdout
    );

    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn up_starts_each_compose_project_once_and_waits_for_one_shot_before_jobs() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let fixture = Fixture::new();
    let state = fixture.state_dir();
    let log = state.join("docker.log");
    let init_exited = state.join("docker.log.init-exited");
    fixture.write(
        "compose.yaml",
        r#"services:
  db:
    image: example/db
  cache:
    image: example/cache
    depends_on:
      db:
        condition: service_started
  dependent:
    image: example/dependent
    depends_on:
      db:
        condition: service_started
  init:
    image: example/init
  after-init:
    image: example/after-init
    depends_on:
      init:
        condition: service_completed_successfully
"#,
    );
    let manifest = r#"
version = 1

[[services]]
id = "db"
compose = { file = "compose.yaml", service = "db" }

[[services]]
id = "cache"
compose = { file = "compose.yaml", service = "cache" }

[[services]]
id = "dependent"
compose = { file = "compose.yaml", service = "dependent" }
needs = ["db"]

[[services]]
id = "init"
compose = { file = "compose.yaml", service = "init" }
wait = "exit"

[[services]]
id = "after-init"
compose = { file = "compose.yaml", service = "after-init" }
needs = ["init"]

[jobs]
verify-init = { run = "test -f '__INIT_EXITED__'", needs = ["init"] }
"#
    .replace("__INIT_EXITED__", &init_exited.display().to_string());
    fixture.write("magictree.toml", &manifest);
    fixture.git_repo();

    let docker_dir = state.join("bin");
    std::fs::create_dir_all(&docker_dir).expect("create fake docker directory");
    let docker = docker_dir.join("docker");
    std::fs::write(
        &docker,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$MAGICTREE_DOCKER_LOG"
case "$*" in
  *" ps -a --format json"*)
    count_file="${MAGICTREE_DOCKER_LOG}.ps-count"
    count=0
    if [ -f "$count_file" ]; then count=$(cat "$count_file"); fi
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    if [ "$count" -ge 4 ]; then
      init_state=exited
      init_exit=',"ExitCode":0'
      : > "${MAGICTREE_DOCKER_LOG}.init-exited"
    else
      init_state=running
      init_exit=''
    fi
    printf '[{"Service":"db","State":"running"},{"Service":"cache","State":"running"},{"Service":"dependent","State":"running"},{"Service":"init","State":"%s"%s},{"Service":"after-init","State":"running"}]\n' "$init_state" "$init_exit"
    ;;
esac
"#,
    )
    .expect("write fake docker executable");
    let mut permissions = std::fs::metadata(&docker)
        .expect("fake docker metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&docker, permissions).expect("make fake docker executable");

    let path = format!(
        "{}:{}",
        docker_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(support::bin())
        .args(["up", "--no-build"])
        .current_dir(fixture.path())
        .env("MAGICTREE_STATE_DIR", &state)
        .env("MAGICTREE_CONFIG_DIR", state.join("config"))
        .env("MAGICTREE_NO_UPDATE_CHECK", "1")
        .env("MAGICTREE_DOCKER_LOG", &log)
        .env("PATH", path)
        .output()
        .expect("run magictree with fake docker");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let calls = std::fs::read_to_string(log).expect("read docker call log");
    let up_calls: Vec<_> = calls
        .lines()
        .filter_map(|call| call.split_once(" up -d ").map(|(_, services)| services))
        .collect();
    assert_eq!(
        up_calls,
        ["db cache dependent init after-init"],
        "Compose starts its project once; magictree waits for init before the job"
    );
    assert!(
        init_exited.exists(),
        "the one-shot completed before its job"
    );
}

#[test]
fn down_reaps_a_process_whose_service_was_renamed_away() {
    let fixture = host_stack_fixture();
    let state = fixture.state_dir();
    assert!(run(&["up"], fixture.path(), &state).ok());

    // Rename the service: the pid file no longer matches any manifest entry.
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "renamed"
command = "sleep 300"
"#,
    );
    let down = run(&["down"], fixture.path(), &state);
    assert!(down.ok(), "{}", down.combined());
    assert!(
        down.stdout.contains("stale"),
        "the orphaned process must be reported and reaped:\n{}",
        down.stdout
    );
}

#[test]
fn failures_exit_non_zero_with_an_explanation() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 1\"\nneeds = [\"ghost\"]\n",
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let result = run(&["up"], fixture.path(), &state);
    assert!(!result.ok());
    assert!(result.stderr.contains("ghost"), "{}", result.stderr);
}

#[test]
fn a_repository_without_a_manifest_says_so() {
    let fixture = Fixture::new();
    fixture.git_repo();
    let state = fixture.state_dir();

    let result = run(&["up"], fixture.path(), &state);
    assert!(!result.ok());
    assert!(
        result.stderr.contains("magictree.toml"),
        "{}",
        result.stderr
    );
}

#[test]
fn gc_outside_a_repository_names_the_machine_wide_sweep() {
    // No checkout means no scope to reconcile, and sweeping the whole state dir
    // instead would be a scope the user never asked for. The failure has to
    // carry the way to ask for it, since that is the only way to reclaim a
    // repository that is itself gone.
    let fixture = Fixture::new();
    let state = fixture.state_dir();

    let scoped = run(&["gc"], fixture.path(), &state);
    assert!(!scoped.ok(), "{}", scoped.combined());
    assert!(
        scoped.stderr.contains("--all"),
        "the failure must name the sweep that needs no repository: {}",
        scoped.stderr
    );

    let swept = run(&["gc", "--all"], fixture.path(), &state);
    assert!(swept.ok(), "{}", swept.combined());
}

#[test]
fn health_probe_gates_up_until_the_service_answers() {
    if !have("python3", &["-c", "pass"]) {
        eprintln!("skipping: python3 is unavailable for a listener fixture");
        return;
    }
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "web"
command = "python3 -m http.server $PORT --bind 127.0.0.1"
port = { env = "PORT" }
health = { http = "/", timeout = 60 }
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let started = Instant::now();
    let up = run(&["up"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "up returned only after the health probe passed"
    );

    let status = run(&["status", "--probe"], fixture.path(), &state);
    assert!(status.stdout.contains("healthy"), "{}", status.stdout);

    let down = run(&["down"], fixture.path(), &state);
    assert!(down.ok(), "{}", down.combined());

    let after = run(&["status"], fixture.path(), &state);
    assert!(after.stdout.contains("stopped"), "{}", after.stdout);
}

#[test]
fn bootstrap_generates_files_before_services_start() {
    // Mirrors the failure this guards against: a dev server importing a file
    // that a generation step has to produce first.
    let fixture = Fixture::new();
    fixture.write(
        "justfile",
        "generate:\n  echo 'export const client = 1' > client.gen.ts\n",
    );
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[bootstrap]
run = ["just generate"]

[[services]]
id = "web"
command = "test -f client.gen.ts && python3 -m http.server $PORT --bind 127.0.0.1"
port = { env = "PORT" }
health = { http = "/", timeout = 30 }
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let up = run(&["up"], fixture.path(), &state);
    assert!(
        up.ok(),
        "the service must start only after its generated input exists:\n{}",
        up.combined()
    );
    assert!(
        fixture.join("client.gen.ts").exists(),
        "bootstrap produced the generated file"
    );
    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn an_after_step_runs_once_the_whole_stack_is_up() {
    // A `when = "up"` job runs as soon as its own `needs` are healthy, which is
    // too early for a script that needs the stack it just started. `after` runs
    // once every selected service is healthy.
    if !have("python3", &["-c", "pass"]) {
        eprintln!("skipping: python3 is unavailable for a listener fixture");
        return;
    }
    let fixture = Fixture::new();
    let reached = fixture.state_dir().join("reached.txt");
    fixture.write(
        "magictree.toml",
        &format!(
            r#"
version = 1

[bootstrap]
after = [
  "python3 -c \"import os, socket; [socket.create_connection(('127.0.0.1', int(os.environ[name]))) for name in ('FIRST_PORT', 'SECOND_PORT')]\"",
  "echo \"$FIRST_PORT $SECOND_PORT\" > '{reached}'",
]

[[services]]
id = "first"
command = "python3 -m http.server $FIRST_PORT --bind 127.0.0.1"
port = {{ env = "FIRST_PORT" }}
health = {{ http = "/", timeout = 30 }}

[[services]]
id = "second"
command = "python3 -m http.server $SECOND_PORT --bind 127.0.0.1"
port = {{ env = "SECOND_PORT" }}
needs = ["first"]
health = {{ http = "/", timeout = 30 }}
"#,
            reached = reached.display()
        ),
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let plan = run(&["--dry-run", "up"], fixture.path(), &state);
    assert!(plan.ok(), "{}", plan.combined());
    assert!(
        plan.stdout.contains("# after"),
        "the plan has to name the after phase:\n{}",
        plan.stdout
    );
    assert!(
        plan.stdout.contains("reached.txt"),
        "the plan has to list the after steps:\n{}",
        plan.stdout
    );

    let up = run(&["up"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());
    // The first step only exits zero if both ports answered, so the second one
    // leaves the marker behind only when every service was up before it ran.
    assert!(
        reached.exists(),
        "an after step must see the whole stack:\n{}",
        up.combined()
    );
    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn the_compose_variable_for_a_published_port_is_injected() {
    // Renders the compose configuration the way magictree would run it, so the
    // published port and the file's own derived values agree.
    if !have("docker", &["compose", "version"]) {
        eprintln!("skipping: docker compose is unavailable");
        return;
    }
    let fixture = Fixture::new();
    fixture.write(
        "compose.yaml",
        r#"
services:
  api:
    image: busybox
    command: ["sleep", "300"]
    ports:
      - "${WT_PORT_API:-8000}:8000"
    environment:
      SELF_URL: "http://localhost:${WT_PORT_API:-8000}/health"
"#,
    );
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "api"
runtime = "compose"
compose = { file = "compose.yaml", service = "api" }
port = { target = 8000, env = "WT_PORT_API" }
expose = "port"
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let port = run(&["ports"], fixture.path(), &state);
    assert!(port.ok(), "{}", port.combined());
    let allocated: u16 = port
        .stdout
        .lines()
        .find(|line| line.contains("api"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("an allocated port");

    // What the override publishes, and what the compose file derives, must match.
    let rendered = std::process::Command::new("docker")
        .args(["compose", "config", "--format", "json"])
        .current_dir(fixture.path())
        .env("WT_PORT_API", allocated.to_string())
        .output()
        .expect("docker compose config");
    let text = String::from_utf8_lossy(&rendered.stdout).to_string();
    assert!(
        text.contains(&format!("http://localhost:{allocated}/health")),
        "the compose file's derived URL must use the allocated port:
{text}"
    );
}

#[test]
fn up_prints_every_reachable_url() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "internal"
command = "sleep 300"
expose = "none"

[[services]]
id = "one"
command = "sleep 300"
port = { env = "PORT" }

[[services]]
id = "two"
command = "sleep 300"
ports = [
  { name = "plain", env = "PLAIN_PORT" },
  { name = "extra", env = "EXTRA_PORT" },
]
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let up = run(&["up"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());

    // Single-port services keep a plain name; multi-port services are labelled.
    assert!(up.stdout.contains("one"), "{}", up.stdout);
    assert!(up.stdout.contains("two:plain"), "{}", up.stdout);
    assert!(up.stdout.contains("two:extra"), "{}", up.stdout);

    let urls = up
        .stdout
        .lines()
        .filter(|line| line.contains("http://localhost:"))
        .count();
    assert_eq!(urls, 3, "every assigned port gets a URL:\n{}", up.stdout);

    // A service with nothing published is reported as internal, not silently dropped.
    assert!(
        up.stdout.contains("internal") && up.stdout.contains("expose = \"none\""),
        "{}",
        up.stdout
    );
    // And the summary says how to go further.
    assert!(up.stdout.contains("magictree down"), "{}", up.stdout);
    assert!(up.stdout.contains("logs"), "{}", up.stdout);

    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn opted_in_ports_rewrite_only_their_local_urls() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[env]
WEB_CALLBACK = "http://localhost:${MAGICTREE_PORT_web_http}/auth/callback"
WEB_SOCKET = "ws://localhost:${MAGICTREE_PORT_web_http}/socket"
WEB_METRICS_URL = "http://localhost:${MAGICTREE_PORT_web_metrics}/metrics"
API_URL = "http://localhost:${MAGICTREE_PORT_api}/api"

[[services]]
id = "web"
command = "sleep 300"
ports = [
  { name = "http", env = "WEB_PORT", browser_alias = true },
  { name = "metrics", env = "METRICS_PORT" },
]

[[services]]
id = "api"
command = "sleep 300"
port = { env = "API_PORT" }
"#,
    );
    fixture.git_repo();
    let added = fixture.git(&["worktree", "add", "-q", "feature-web", "-b", "feature-web"]);
    assert!(added.status.success(), "git worktree add");
    let worktree = fixture.join("feature-web");
    let state = fixture.state_dir();

    let env = run(&["env"], &worktree, &state);
    assert!(env.ok(), "{}", env.combined());
    let ports = run(&["ports"], &worktree, &state);
    assert!(ports.ok(), "{}", ports.combined());
    let web_port = assigned_port(&ports.stdout, "web:http");
    let metrics_port = assigned_port(&ports.stdout, "web:metrics");
    let api_port = assigned_port(&ports.stdout, "api");
    assert!(
        env.stdout.contains(&format!(
            "WEB_CALLBACK=http://feature-web.localhost:{web_port}/auth/callback"
        )),
        "only the opted-in web URL uses the alias:\n{}",
        env.stdout
    );
    assert!(
        env.stdout.contains(&format!(
            "WEB_SOCKET=ws://feature-web.localhost:{web_port}/socket"
        )),
        "WebSocket URLs use the same selected alias:\n{}",
        env.stdout
    );
    assert!(
        env.stdout.contains(&format!(
            "WEB_METRICS_URL=http://localhost:{metrics_port}/metrics"
        )),
        "the unmarked port of the same service stays on localhost:\n{}",
        env.stdout
    );
    assert!(
        env.stdout
            .contains(&format!("API_URL=http://localhost:{api_port}/api")),
        "an unmarked service URL stays on localhost:\n{}",
        env.stdout
    );
    assert!(
        ports
            .stdout
            .contains(&format!("http://feature-web.localhost:{web_port}")),
        "ports shows the opted-in alias:\n{}",
        ports.stdout
    );
    let api_line = ports
        .stdout
        .lines()
        .find(|line| line.starts_with("api"))
        .expect("unaliased API port line");
    assert!(!api_line.contains(".localhost:"), "{api_line}");
    let metrics_line = ports
        .stdout
        .lines()
        .find(|line| line.starts_with("web:metrics"))
        .expect("unaliased metrics port line");
    assert!(!metrics_line.contains(".localhost:"), "{metrics_line}");

    let up = run(&["up"], &worktree, &state);
    assert!(up.ok(), "{}", up.combined());
    assert!(
        up.stdout
            .contains(&format!("http://feature-web.localhost:{web_port}")),
        "up shows the opted-in alias:\n{}",
        up.stdout
    );
    let status = run(&["status"], &worktree, &state);
    assert!(status.ok(), "{}", status.combined());
    assert!(
        status
            .stdout
            .contains(&format!("http://feature-web.localhost:{web_port}")),
        "status shows the opted-in alias:\n{}",
        status.stdout
    );
    let api_line = up
        .stdout
        .lines()
        .find(|line| line.starts_with("api"))
        .expect("unaliased API URL line");
    assert!(!api_line.contains(".localhost:"), "{api_line}");
    let metrics_line = up
        .stdout
        .lines()
        .find(|line| line.starts_with("web:metrics"))
        .expect("unaliased metrics URL line");
    assert!(!metrics_line.contains(".localhost:"), "{metrics_line}");
    assert!(run(&["down"], &worktree, &state).ok());
}

#[test]
fn env_publishes_every_declared_port_variable() {
    // `magictree env --export` is how a host command adopts a worktree's
    // environment. It must carry the whole stack's declared `port.env` values,
    // not just the ones the invoking service happens to own, or a host tool
    // silently falls back to a port that belongs to another worktree.
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "api"
command = "sleep 300"
port = { env = "WT_PORT_API" }

[[services]]
id = "seed"
command = "sleep 300"
port = { env = "WT_PORT_SEED" }
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let ports = run(&["ports"], fixture.path(), &state);
    assert!(ports.ok(), "{}", ports.combined());
    let allocated = |name: &str| -> String {
        ports
            .stdout
            .lines()
            .find(|line| line.starts_with(name))
            .unwrap_or_else(|| panic!("no port line for {name}:\n{}", ports.stdout))
            .split_whitespace()
            .nth(1)
            .expect("port column")
            .to_string()
    };

    let env = run(&["env", "--export"], fixture.path(), &state);
    assert!(env.ok(), "{}", env.combined());
    assert!(
        env.stdout
            .contains(&format!("export WT_PORT_API={}", allocated("api"))),
        "the declaring service's variable is exported:\n{}",
        env.stdout
    );
    assert!(
        env.stdout
            .contains(&format!("export WT_PORT_SEED={}", allocated("seed"))),
        "a peer-declared port variable must reach `env`:\n{}",
        env.stdout
    );

    // Restating a computed port variable in `[env]` is a conflict, not a
    // silent override: magictree already publishes it.
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[env]
WT_PORT_SEED = "${MAGICTREE_PORT_seed}"

[[services]]
id = "seed"
command = "sleep 300"
port = { env = "WT_PORT_SEED" }
"#,
    );
    let duplicate = run(&["env"], fixture.path(), &state);
    let message = duplicate.combined();
    assert!(
        !duplicate.ok() && message.contains("WT_PORT_SEED"),
        "a restated port variable must fail loudly:\n{message}"
    );
    assert!(
        message.contains("service 'seed'") && message.contains("port.env"),
        "the error must name the service that already publishes it:\n{message}"
    );
}

#[test]
fn exec_runs_with_the_worktrees_resolved_environment() {
    // `exec` replaces the `eval "$(magictree env --export)"` a repository with a
    // host-side CLI would otherwise hand-roll.
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "api"
command = "sleep 300"
port = { env = "WT_PORT_API" }

[[services]]
id = "seed"
command = "sleep 300"
port = { env = "WT_PORT_SEED" }
"#,
    );
    fixture.mkdir("sub");
    fixture.git_repo();
    let state = fixture.state_dir();

    // A dry run runs nothing and claims nothing.
    let dry = run(
        &["--dry-run", "exec", "--", "echo", "ran"],
        fixture.path(),
        &state,
    );
    assert!(dry.ok(), "{}", dry.combined());
    assert!(dry.stdout.contains("would run"), "{}", dry.stdout);
    assert!(
        !dry.stdout.lines().any(|line| line == "ran"),
        "a dry run must not run the command:\n{}",
        dry.stdout
    );
    assert!(
        runtime_dirs(&state).is_empty(),
        "a dry run must not claim any worktree state"
    );

    let ports = run(&["ports"], fixture.path(), &state);
    assert!(ports.ok(), "{}", ports.combined());
    let allocated = |name: &str| -> String {
        ports
            .stdout
            .lines()
            .find(|line| line.starts_with(name))
            .unwrap_or_else(|| panic!("no port line for {name}:\n{}", ports.stdout))
            .split_whitespace()
            .nth(1)
            .expect("port column")
            .to_string()
    };

    let exec = run(
        &[
            "exec",
            "--",
            "sh",
            "-c",
            "echo \"$WT_PORT_API $WT_PORT_SEED\"",
        ],
        fixture.path(),
        &state,
    );
    assert!(exec.ok(), "{}", exec.combined());
    assert_eq!(
        exec.stdout.trim(),
        format!("{} {}", allocated("api"), allocated("seed")),
        "the whole stack's declared ports reach the child"
    );

    // The child's exit status is magictree's.
    let failed = run(
        &["exec", "--", "sh", "-c", "exit 7"],
        fixture.path(),
        &state,
    );
    assert_eq!(failed.status, 7, "{}", failed.combined());

    // The command runs in the directory magictree was pointed at.
    let pwd = run(
        &["exec", "--cwd", "sub", "--", "pwd"],
        fixture.path(),
        &state,
    );
    assert!(pwd.ok(), "{}", pwd.combined());
    assert!(
        pwd.stdout.trim().ends_with("sub"),
        "the command runs in --cwd:\n{}",
        pwd.stdout
    );
}

#[test]
fn health_check_accepts_a_service_bound_only_to_ipv6() {
    // A dev server asked to listen on `localhost` may bind ::1 only, which is
    // what Vite does. Probing 127.0.0.1 alone reported it as unreachable.
    if !have(
        "python3",
        &["-c", "import socket; socket.socket(socket.AF_INET6)"],
    ) {
        eprintln!("skipping: no IPv6 python3");
        return;
    }
    let fixture = Fixture::new();
    fixture.write(
        "ipv6_server.py",
        concat!(
            "import http.server, os, socket, socketserver\n",
            "socketserver.TCPServer.address_family = socket.AF_INET6\n",
            "socketserver.TCPServer(\n",
            "    ('::1', int(os.environ['PORT'])), http.server.SimpleHTTPRequestHandler\n",
            ").serve_forever()\n",
        ),
    );
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "web"
command = "python3 ipv6_server.py"
port = { env = "PORT" }
health = { http = "/", timeout = 30 }
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let up = run(&["up"], fixture.path(), &state);
    assert!(
        up.ok(),
        "an IPv6-only listener is healthy, not unreachable:\n{}",
        up.combined()
    );
    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn a_service_that_dies_reports_why_immediately() {
    // The failure that prompted this: uvicorn could not bind, died, and the
    // user only saw a bare "timed out after 120s waiting for 127.0.0.1:22062".
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "api"
command = "echo 'ERROR: [Errno 48] Address already in use' >&2; exit 1"
port = { env = "PORT" }
health = { http = "/health", timeout = 120 }
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let started = Instant::now();
    let up = run(&["up"], fixture.path(), &state);
    let elapsed = started.elapsed();

    assert!(!up.ok(), "a dead service must fail the command");
    assert!(
        elapsed < Duration::from_secs(20),
        "a service that already exited must not wait out the probe timeout (took {elapsed:?})"
    );

    let message = up.stderr.clone();
    assert!(message.contains("api"), "must name the service: {message}");
    assert!(
        message.contains("exited"),
        "must say the process is gone: {message}"
    );
    assert!(
        message.contains("Address already in use"),
        "must surface the service's own output: {message}"
    );
    assert!(
        message.contains("magictree logs api"),
        "must say how to see more: {message}"
    );
    assert!(
        message.contains("PORT"),
        "must name the variable carrying the port: {message}"
    );
}

#[test]
fn a_service_that_never_becomes_healthy_fails_up() {
    if !have("python3", &["-c", "pass"]) {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let fixture = Fixture::new();
    // The command never listens, so the probe must time out and fail.
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "broken"
command = "sleep 300"
port = { env = "PORT" }
health = { tcp = true, timeout = 2 }
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let up = run(&["up"], fixture.path(), &state);
    assert!(!up.ok(), "unhealthy services must fail the command");
    assert!(up.stderr.contains("timed out"), "{}", up.stderr);
    // The report has to identify the service, not just a port number.
    assert!(up.stderr.contains("broken"), "{}", up.stderr);
    assert!(
        up.stderr.contains("still running") || up.stderr.contains("running (pid"),
        "{}",
        up.stderr
    );
    let runtime = runtime_dirs(&state);
    let log = runtime[0].join("log/broken.log");
    assert!(
        up.stderr.contains(&log.display().to_string()),
        "the report names the service's log: {}\n{}",
        log.display(),
        up.stderr
    );

    // The process is left running so the failure can be inspected.
    let status = run(&["status"], fixture.path(), &state);
    assert!(status.stdout.contains("running"), "{}", status.stdout);
    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn init_adds_a_service_the_repository_gained_to_an_existing_manifest() {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"solo","packageManager":"pnpm@9","scripts":{"dev":"sleep 300"}}"#,
    );
    fixture.write("pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.write(
        "magictree.toml",
        r#"version = 1

# Written before Storybook was in the stack.
[[services]]
id = "solo"
target = { kind = "pnpm", script = "dev" }
port = { env = "PORT", prefer = 5173 }
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    // The repository gains Storybook after the manifest was written.
    fixture.write(
        "package.json",
        r#"{"name":"solo","packageManager":"pnpm@9","scripts":{"dev":"sleep 300","storybook":"storybook dev -p 6006"},"devDependencies":{"@storybook/react-vite":"8.6.14"}}"#,
    );
    fixture.write(".storybook/main.ts", "export default {};\n");

    let init = run(&["init", "--accept-defaults"], fixture.path(), &state);
    assert!(init.ok(), "{}", init.combined());
    assert!(
        init.stdout.contains("added service 'storybook'"),
        "{}",
        init.combined()
    );

    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert!(
        manifest.contains("prefer = 5173"),
        "the manifest keeps its own choices:\n{manifest}"
    );
    assert_eq!(
        manifest.matches("[[services]]").count(),
        2,
        "the dev server the manifest already runs is not duplicated:\n{manifest}"
    );

    let up = run(&["--dry-run", "up"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());
    assert!(
        up.stdout
            .contains("pnpm run storybook -p ${STORYBOOK_PORT:-6006} --no-open"),
        "{}",
        up.stdout
    );

    let again = run(&["init", "--accept-defaults"], fixture.path(), &state);
    assert!(again.ok(), "{}", again.combined());
    assert!(again.stdout.contains("unchanged"), "{}", again.combined());
}

#[test]
fn init_records_its_answers_and_asks_again_only_from_them() {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"solo","packageManager":"pnpm@9","scripts":{"dev":"sleep 300"}}"#,
    );
    fixture.write("pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.git_repo();
    let state = fixture.state_dir();
    let id = fixture
        .path()
        .file_name()
        .expect("fixture name")
        .to_string_lossy()
        .to_string();

    let first = run(&["init", "--accept-defaults"], fixture.path(), &state);
    assert!(first.ok(), "{}", first.combined());
    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert!(
        manifest.contains(&format!("\"{id}.run\" = \"pnpm:dev\"")),
        "{manifest}"
    );

    // Nothing has changed, so the second run records nothing new and rewrites
    // nothing.
    let second = run(&["init", "--accept-defaults"], fixture.path(), &state);
    assert!(second.ok(), "{}", second.combined());
    assert!(
        second.stdout.contains("keeping 1 answer(s) recorded in"),
        "{}",
        second.combined()
    );
    assert!(second.stdout.contains("unchanged"), "{}", second.combined());
    assert_eq!(
        std::fs::read_to_string(fixture.join("magictree.toml")).expect("read"),
        manifest,
        "an unchanged run leaves the file alone"
    );

    // A question answered `skip` is remembered: the service the repository
    // gained afterwards is offered, not added.
    fixture.write(
        "package.json",
        r#"{"name":"solo","packageManager":"pnpm@9","scripts":{"dev":"sleep 300","storybook":"storybook dev -p 6006"},"devDependencies":{"@storybook/react-vite":"8.6.14"}}"#,
    );
    fixture.write(".storybook/main.ts", "export default {};\n");
    fixture.write(
        "magictree.toml",
        &manifest.replace(
            &format!("\"{id}.run\" = \"pnpm:dev\""),
            &format!("\"{id}.run\" = \"pnpm:dev\", \"{id}.storybook\" = \"skip\""),
        ),
    );
    let third = run(&["init", "--accept-defaults"], fixture.path(), &state);
    assert!(third.ok(), "{}", third.combined());
    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert!(
        !manifest.contains("id = \"storybook\""),
        "the recorded skip is honoured:\n{manifest}"
    );

    // Ignoring the record asks about it again, so this time it is added.
    let reanswered = run(
        &["init", "--accept-defaults", "--reanswer"],
        fixture.path(),
        &state,
    );
    assert!(reanswered.ok(), "{}", reanswered.combined());
    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert!(manifest.contains("id = \"storybook\""), "{manifest}");
    assert!(
        manifest.contains(&format!("\"{id}.storybook\" = \"pnpm:storybook\"")),
        "the record is brought up to date:\n{manifest}"
    );
}

#[test]
fn a_compose_service_added_later_is_reported_rather_than_ignored() {
    let fixture = Fixture::new();
    fixture.write(
        "compose.yaml",
        "services:\n  db:\n    image: example/db:18\n    ports:\n      - \"5432:5432\"\n  cache:\n    image: example/cache:8\n",
    );
    fixture.write(
        "package.json",
        r#"{"name":"solo","packageManager":"pnpm@9","scripts":{"dev":"sleep 300"}}"#,
    );
    fixture.write("pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.git_repo();
    let state = fixture.state_dir();

    let first = run(&["init", "--accept-defaults"], fixture.path(), &state);
    assert!(first.ok(), "{}", first.combined());
    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert!(manifest.contains("id = \"cache\""), "{manifest}");

    // Someone adds a service the manifest was never told about.
    fixture.write(
        "compose.yaml",
        "services:\n  db:\n    image: example/db:18\n    ports:\n      - \"5432:5432\"\n  cache:\n    image: example/cache:8\n  traces:\n    image: example/traces:latest\n",
    );
    let second = run(&["init", "--accept-defaults"], fixture.path(), &state);
    assert!(second.ok(), "{}", second.combined());
    assert!(
        second.stderr.contains("traces") && second.stderr.contains("never decided"),
        "the new service is reported: {}",
        second.combined()
    );
    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert!(
        !manifest.contains("id = \"traces\""),
        "an unanswered question is not decided for the user:\n{manifest}"
    );

    // Answering it again takes discovery's default, which manages every service.
    let reanswered = run(
        &["init", "--accept-defaults", "--reanswer"],
        fixture.path(),
        &state,
    );
    assert!(reanswered.ok(), "{}", reanswered.combined());
    let manifest = std::fs::read_to_string(fixture.join("magictree.toml")).expect("read");
    assert!(manifest.contains("id = \"traces\""), "{manifest}");
}

#[test]
fn discover_init_doctor_round_trip() {
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        r#"{"name":"solo","packageManager":"pnpm@9","scripts":{"dev":"sleep 300"}}"#,
    );
    fixture.write("pnpm-lock.yaml", "lockfileVersion: 9\n");
    fixture.git_repo();
    let state = fixture.state_dir();

    let report = run(
        &["discover", "--report", "discovery.json"],
        fixture.path(),
        &state,
    );
    assert!(report.ok(), "{}", report.combined());
    assert!(fixture.join("discovery.json").exists());

    let answers = run(&["discover", "--default-answers"], fixture.path(), &state);
    assert!(answers.ok(), "{}", answers.combined());
    fixture.write("answers.json", &answers.stdout);

    let init = run(
        &["init", "--answers", "answers.json", "--print"],
        fixture.path(),
        &state,
    );
    assert!(init.ok(), "{}", init.combined());
    assert!(init.stdout.contains("[[services]]"), "{}", init.stdout);

    let written = run(
        &["init", "--answers", "answers.json"],
        fixture.path(),
        &state,
    );
    assert!(written.ok(), "{}", written.combined());

    // A manifest only reaches worktrees once committed, and doctor says so.
    fixture.git(&["add", "magictree.toml", "answers.json", "discovery.json"]);
    fixture.git(&[
        "-c",
        "user.email=test@example.com",
        "-c",
        "user.name=test",
        "commit",
        "-qm",
        "manifest",
    ]);

    let doctor = run(&["doctor"], fixture.path(), &state);
    assert!(
        doctor.ok(),
        "doctor on fresh manifests:\n{}",
        doctor.combined()
    );

    // The generated manifest must actually drive a stack.
    let up = run(&["--dry-run", "up"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());
    assert!(up.stdout.contains("pnpm run dev"), "{}", up.stdout);
}

#[test]
fn up_all_starts_every_app_even_inside_one() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[workspace]\napps = [\"apps/a\", \"apps/b\"]\n",
    );
    fixture.write(
        "apps/a/magictree.toml",
        "version = 1\n\n[app]\nid = \"a\"\n\n[[services]]\nid = \"web\"\ncommand = \"sleep 300\"\nport = { env = \"PORT\" }\n",
    );
    fixture.write(
        "apps/b/magictree.toml",
        "version = 1\n\n[app]\nid = \"b\"\n\n[[services]]\nid = \"api\"\ncommand = \"sleep 300\"\nport = { env = \"PORT\" }\n",
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    // Run from inside app a, where the default app exists: `--all` must still
    // select the whole repository, not just the current app.
    let up = run(&["up", "--all"], &fixture.join("apps/a"), &state);
    assert!(up.ok(), "{}", up.combined());
    assert!(up.stdout.contains("a:web: started"), "{}", up.stdout);
    assert!(up.stdout.contains("b:api: started"), "{}", up.stdout);

    let down = run(&["down"], fixture.path(), &state);
    assert!(down.ok(), "{}", down.combined());
}

/// Standing in an app narrows the selection to that app, never to a stack
/// without the repository's shared infrastructure: an app started without the
/// database it was written against crashes on boot.
#[test]
fn up_inside_an_app_still_starts_the_shared_infrastructure() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[workspace]
apps = ["apps/api"]

[[services]]
id = "db"
command = "sleep 300"
port = { env = "DB_PORT" }
"#,
    );
    fixture.write(
        "apps/api/magictree.toml",
        "version = 1\n\n[app]\nid = \"api\"\n\n[[services]]\nid = \"api\"\ncommand = \"sleep 300\"\nport = { env = \"PORT\" }\n",
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let up = run(&["up"], &fixture.join("apps/api"), &state);
    assert!(up.ok(), "{}", up.combined());
    let db = up
        .stdout
        .find("db: started")
        .unwrap_or_else(|| panic!("the shared database was not started:\n{}", up.stdout));
    let api = up
        .stdout
        .find("api:api: started")
        .unwrap_or_else(|| panic!("the app was not started:\n{}", up.stdout));
    assert!(
        db < api,
        "the database starts before the app that reads it:\n{}",
        up.stdout
    );

    let down = run(&["down"], fixture.path(), &state);
    assert!(down.ok(), "{}", down.combined());
}

/// An app's bootstrap `inputs` name files inside the app, matching the working
/// directory its command runs in: `uv.lock` means `apps/api/uv.lock`, not
/// `uv.lock` at the worktree root, where no such file exists.
#[test]
fn monorepo_app_bootstrap_resolves_inputs_inside_the_app() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[workspace]\napps = [\"apps/api\"]\n",
    );
    fixture.write("apps/api/uv.lock", "version = 1\n");
    fixture.write(
        "apps/api/magictree.toml",
        r#"
version = 1
[app]
id = "api"

[bootstrap]
run = [{ command = "echo api bootstrapped", inputs = ["uv.lock"] }]

[[services]]
id = "api"
command = "sleep 300"
port = { env = "PORT" }
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let up = run(&["up", "--all"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());
    assert!(up.stdout.contains("api bootstrapped"), "{}", up.stdout);

    let down = run(&["down"], fixture.path(), &state);
    assert!(down.ok(), "{}", down.combined());
}

/// Two apps can declare the same install command over different lockfiles, so
/// each step caches against its own inputs: app a's lockfile must not decide
/// app b's step is up to date.
#[test]
fn identical_bootstrap_commands_in_two_apps_cache_separately() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[workspace]\napps = [\"apps/a\", \"apps/b\"]\n",
    );
    for (app, lock) in [("a", "lockfileVersion: 9\n"), ("b", "lockfileVersion: 8\n")] {
        fixture.write(&format!("apps/{app}/pnpm-lock.yaml"), lock);
        fixture.write(
            &format!("apps/{app}/magictree.toml"),
            &format!(
                r#"
version = 1
[app]
id = "{app}"

[bootstrap]
run = [{{ command = "echo install {app}", inputs = ["pnpm-lock.yaml"] }}]

[[services]]
id = "web"
command = "sleep 300"
port = {{ env = "PORT" }}
"#
            ),
        );
    }
    fixture.git_repo();
    let state = fixture.state_dir();

    let up = run(&["up", "--all"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());
    assert!(
        up.stdout.contains("install a") && up.stdout.contains("install b"),
        "{}",
        up.stdout
    );

    let again = run(&["up", "--all"], fixture.path(), &state);
    assert!(again.ok(), "{}", again.combined());
    assert_eq!(
        again.stdout.matches("cached").count(),
        2,
        "every app's step caches against its own inputs: {}",
        again.stdout
    );

    let down = run(&["down"], fixture.path(), &state);
    assert!(down.ok(), "{}", down.combined());
}

/// An app's `sync` paths are inside the app too: a linked worktree links
/// `apps/web/node_modules`, so the app's own generated dependencies appear.
#[test]
fn monorepo_app_sync_links_inside_the_app() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        "version = 1\n\n[workspace]\napps = [\"apps/web\"]\n",
    );
    fixture.write(
        "apps/web/magictree.toml",
        r#"
version = 1
[app]
id = "web"

[bootstrap]
sync = ["node_modules"]

[[services]]
id = "web"
command = "sleep 300"
port = { env = "PORT" }
"#,
    );
    fixture.git_repo();
    // Installed dependencies are untracked, so a new worktree starts without
    // them and `sync` is what supplies them.
    fixture.mkdir("apps/web/node_modules");

    let added = fixture.git(&["worktree", "add", "-q", "wt", "-b", "wt"]);
    assert!(added.status.success(), "git worktree add");
    let worktree = fixture.join("wt");
    let state = fixture.state_dir();

    let up = run(&["up", "--all"], &worktree, &state);
    assert!(up.ok(), "{}", up.combined());
    let linked = worktree.join("apps/web/node_modules");
    assert!(
        linked
            .symlink_metadata()
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false),
        "sync must link the app's own node_modules: {}",
        up.combined()
    );

    let down = run(&["down"], &worktree, &state);
    assert!(down.ok(), "{}", down.combined());
}

/// A host-process stack whose single port is declared, the way a repository
/// whose own tooling expects a fixed port declares one.
fn declared_stack_fixture(port: u16) -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        &format!(
            "version = 1\n\n[[services]]\nid = \"idle\"\ncommand = \"sleep 300\"\nport = {{ env = \"PORT\", prefer = {port} }}\n"
        ),
    );
    fixture.git_repo();
    fixture
}

fn assigned_port(text: &str, service: &str) -> u16 {
    text.lines()
        .find(|line| line.starts_with(service))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("no port for '{service}' in:\n{text}"))
}

fn blocks_claimed(state: &Path) -> usize {
    std::fs::read_dir(state.join("blocks"))
        .map(|entries| entries.flatten().count())
        .unwrap_or(0)
}

#[test]
fn the_primary_checkout_uses_a_declared_port() {
    let declared = free_port();
    let fixture = declared_stack_fixture(declared);
    let state = fixture.state_dir();

    let ports = run(&["ports"], fixture.path(), &state);
    assert!(ports.ok(), "{}", ports.combined());
    assert_eq!(
        assigned_port(&ports.stdout, "idle"),
        declared,
        "{}",
        ports.stdout
    );
    assert!(
        ports.stdout.contains("declared ports"),
        "the header says where the ports come from: {}",
        ports.stdout
    );
}

#[test]
fn up_ports_generated_moves_the_primary_checkout_off_its_declared_port() {
    // How a primary checkout runs a stack beside another one: generated ports
    // come from its own block, so the declared ones stay untouched.
    let declared = free_port();
    let fixture = declared_stack_fixture(declared);
    let state = fixture.state_dir();

    let generated = run(&["up", "--ports", "generated"], fixture.path(), &state);
    assert!(generated.ok(), "{}", generated.combined());
    let ports = run(&["ports"], fixture.path(), &state);
    let assigned = assigned_port(&ports.stdout, "idle");
    assert_ne!(assigned, declared, "{}", ports.stdout);
    assert!(ports.stdout.contains("generated ports"), "{}", ports.stdout);
    assert!(
        generated.stdout.contains(&format!("localhost:{assigned}")),
        "the run reports the ports it is actually on: {}",
        generated.stdout
    );

    // And back: the declared port was released with the generated assignment.
    assert!(run(&["down"], fixture.path(), &state).ok());
    let declared_again = run(&["up", "--ports", "declared"], fixture.path(), &state);
    assert!(declared_again.ok(), "{}", declared_again.combined());
    let ports = run(&["ports"], fixture.path(), &state);
    assert_eq!(
        assigned_port(&ports.stdout, "idle"),
        declared,
        "{}",
        ports.stdout
    );
    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn changing_ports_under_a_running_stack_is_refused() {
    let declared = free_port();
    let fixture = declared_stack_fixture(declared);
    let state = fixture.state_dir();

    assert!(run(&["up"], fixture.path(), &state).ok());

    let refused = run(&["up", "--ports", "generated"], fixture.path(), &state);
    assert!(!refused.ok(), "{}", refused.combined());
    assert!(
        refused.combined().contains("is running"),
        "{}",
        refused.combined()
    );

    // The mode it is already on is not a change, so `up` stays idempotent.
    let same = run(&["up", "--ports", "declared"], fixture.path(), &state);
    assert!(same.ok(), "{}", same.combined());

    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn releasing_ports_under_a_running_stack_is_refused() {
    let declared = free_port();
    let fixture = declared_stack_fixture(declared);
    let state = fixture.state_dir();

    assert!(run(&["up"], fixture.path(), &state).ok());
    let refused = run(&["ports", "--release"], fixture.path(), &state);
    assert!(!refused.ok(), "{}", refused.combined());
    assert!(
        refused.combined().contains("is running"),
        "{}",
        refused.combined()
    );
    assert_eq!(blocks_claimed(&state), 1);

    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn ports_release_clears_the_reservation_and_the_next_up_reclaims_it() {
    let declared = free_port();
    let fixture = declared_stack_fixture(declared);
    let state = fixture.state_dir();

    assert!(run(&["up"], fixture.path(), &state).ok());
    assert!(run(&["down"], fixture.path(), &state).ok());
    assert_eq!(blocks_claimed(&state), 1);

    let released = run(&["ports", "--release"], fixture.path(), &state);
    assert!(released.ok(), "{}", released.combined());
    assert_eq!(
        blocks_claimed(&state),
        0,
        "nothing stays reserved: {}",
        released.stdout
    );

    let up = run(&["up"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());
    let ports = run(&["ports"], fixture.path(), &state);
    assert_eq!(assigned_port(&ports.stdout, "idle"), declared);
    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn a_dry_run_previews_the_ports_the_run_would_use() {
    let declared = free_port();
    let fixture = declared_stack_fixture(declared);
    let state = fixture.state_dir();

    let declared_plan = run(&["--dry-run", "up"], fixture.path(), &state);
    assert!(declared_plan.ok(), "{}", declared_plan.combined());
    assert!(
        declared_plan.stdout.contains(&format!("PORT={declared}")),
        "a declared port is previewed as itself: {}",
        declared_plan.stdout
    );

    let generated_plan = run(
        &["--dry-run", "up", "--ports", "generated"],
        fixture.path(),
        &state,
    );
    assert!(generated_plan.ok(), "{}", generated_plan.combined());
    assert!(
        generated_plan.stdout.contains("PORT=<allocated>"),
        "generated ports are allocated, so they are previewed as such: {}",
        generated_plan.stdout
    );
    assert!(!generated_plan.stdout.contains(&format!("PORT={declared}")));
    assert_eq!(blocks_claimed(&state), 0, "a dry run claims nothing");
}

#[test]
fn a_dry_run_shows_a_placeholder_inside_a_value_built_from_a_port() {
    // A `[env]` value that interpolates a port is the shape that used to leak
    // the dry run's sentinel: it printed `PUBLIC_URL=http://localhost:0` for a
    // port no run had allocated.
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[env]
PUBLIC_URL = "http://localhost:${MAGICTREE_PORT_idle}"

[[services]]
id = "idle"
command = "sleep 300"
port = { env = "PORT" }
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let plan = run(&["--dry-run", "up"], fixture.path(), &state);
    assert!(plan.ok(), "{}", plan.combined());
    assert!(
        plan.stdout
            .contains("PUBLIC_URL=http://localhost:<allocated>"),
        "{}",
        plan.stdout
    );
    assert!(plan.stdout.contains("PORT=<allocated>"), "{}", plan.stdout);
}

#[test]
fn a_dry_run_keeps_a_declared_port_it_builds_a_value_from() {
    let declared = free_port();
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        &format!(
            r#"
version = 1

[env]
PUBLIC_URL = "http://localhost:${{MAGICTREE_PORT_idle}}"

[[services]]
id = "idle"
command = "sleep 300"
port = {{ env = "PORT", prefer = {declared} }}
"#
        ),
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let plan = run(&["--dry-run", "up"], fixture.path(), &state);
    assert!(plan.ok(), "{}", plan.combined());
    assert!(
        plan.stdout
            .contains(&format!("PUBLIC_URL=http://localhost:{declared}")),
        "a declared port is previewed as itself, the value built from it too: {}",
        plan.stdout
    );

    // Under generated ports the same run allocates, and the value says so.
    let generated = run(
        &["--dry-run", "up", "--ports", "generated"],
        fixture.path(),
        &state,
    );
    assert!(generated.ok(), "{}", generated.combined());
    assert!(
        generated
            .stdout
            .contains("PUBLIC_URL=http://localhost:<allocated>"),
        "{}",
        generated.stdout
    );
}

/// The pid a status line reports, when the service is running.
fn pid_in_status(text: &str, service: &str) -> Option<i32> {
    text.lines()
        .find(|line| line.split_whitespace().next() == Some(service))
        .and_then(|line| {
            line.split("pid ")
                .nth(1)
                .and_then(|rest| rest.split(')').next())
                .and_then(|pid| pid.parse().ok())
        })
}

#[test]
fn restart_does_not_restart_dependencies_of_the_selected_service() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[workspace]
apps = ["apps/web"]

[[services]]
id = "postgres"
command = "sleep 300"

[[services]]
id = "redis"
command = "sleep 300"

[[services]]
id = "rustfs"
command = "sleep 300"
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
command = "sleep 300"
needs = ["postgres", "redis", "rustfs"]
"#,
    );
    fixture.git_repo();

    let plan = run(
        &["--dry-run", "restart", "web:web"],
        &fixture.join("apps/web"),
        &fixture.state_dir(),
    );

    assert!(plan.ok(), "{}", plan.combined());
    assert!(
        plan.stdout.contains("web:web: would start again"),
        "{}",
        plan.stdout
    );
    for dependency in ["postgres", "redis", "rustfs"] {
        assert!(
            !plan.stdout.contains(&format!("{dependency}: would")),
            "restart must leave dependency {dependency} alone:\n{}",
            plan.stdout
        );
    }
}

#[test]
fn restart_replaces_a_running_host_service() {
    let fixture = host_stack_fixture();
    let state = fixture.state_dir();

    assert!(run(&["up"], fixture.path(), &state).ok());
    let before = run(&["status"], fixture.path(), &state);
    let old_pid = pid_in_status(&before.stdout, "idle").expect("pid before restart");

    let restart = run(&["restart", "idle"], fixture.path(), &state);
    assert!(restart.ok(), "{}", restart.combined());
    assert!(
        restart.stdout.contains("idle: stopped"),
        "{}",
        restart.stdout
    );
    assert!(
        restart.stdout.contains("idle: started (pid"),
        "{}",
        restart.stdout
    );

    let after = run(&["status"], fixture.path(), &state);
    let new_pid = pid_in_status(&after.stdout, "idle").expect("pid after restart");
    assert_ne!(new_pid, old_pid, "restart must start a new process");

    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn restart_starts_a_stopped_service_again() {
    // A restart never moves ports: the assignment outlives `down`, and the
    // service comes back on the port it had.
    let fixture = host_stack_fixture();
    let state = fixture.state_dir();

    assert!(run(&["up"], fixture.path(), &state).ok());
    let port_before = run(&["ports"], fixture.path(), &state).stdout;
    assert!(run(&["down"], fixture.path(), &state).ok());

    let restart = run(&["restart", "idle"], fixture.path(), &state);
    assert!(restart.ok(), "{}", restart.combined());
    assert!(
        restart.stdout.contains("idle: not running"),
        "{}",
        restart.stdout
    );
    assert!(
        restart.stdout.contains("idle: started (pid"),
        "{}",
        restart.stdout
    );

    let port_after = run(&["ports"], fixture.path(), &state).stdout;
    assert_eq!(port_before, port_after, "a restart keeps the assignment");

    assert!(run(&["down"], fixture.path(), &state).ok());
}

#[test]
fn restart_rejects_unknown_names_jobs_and_missing_assignments() {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[[services]]
id = "idle"
command = "sleep 300"
port = { env = "PORT" }

[jobs.seed]
run = "echo seeded"
when = "manual"
"#,
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let unknown = run(&["restart", "nosuch"], fixture.path(), &state);
    assert!(!unknown.ok(), "{}", unknown.combined());
    assert!(
        unknown.combined().contains("unknown service 'nosuch'"),
        "{}",
        unknown.combined()
    );

    let job = run(&["restart", "seed"], fixture.path(), &state);
    assert!(!job.ok(), "{}", job.combined());
    assert!(job.combined().contains("is a job"), "{}", job.combined());

    // Without a stack there is nothing to restart onto: the assignment, the
    // ports and the environment a restart needs do not exist yet.
    let cold = run(&["restart", "idle"], fixture.path(), &state);
    assert!(!cold.ok(), "{}", cold.combined());
    assert!(
        cold.combined().contains("run `magictree up` first"),
        "{}",
        cold.combined()
    );
}

#[test]
fn a_dry_run_of_restart_creates_nothing() {
    let fixture = host_stack_fixture();
    let state = fixture.state_dir();

    let plan = run(&["--dry-run", "restart", "idle"], fixture.path(), &state);
    assert!(plan.ok(), "{}", plan.combined());
    assert!(plan.stdout.contains("dry run"), "{}", plan.stdout);
    assert!(plan.stdout.contains("idle"), "{}", plan.stdout);
    assert!(plan.stdout.contains("would start again"), "{}", plan.stdout);

    assert!(
        runtime_dirs(&state).is_empty(),
        "a dry run must not create worktree state"
    );
}

#[test]
fn after_steps_marked_ask_are_declined_without_a_terminal() {
    // A run without a terminal — scripts, agents, CI — must neither block nor
    // answer the prompt itself: the step is skipped and nothing is cached.
    let fixture = Fixture::new();
    let marker = fixture.state_dir().join("seeded.txt");
    fixture.write(
        "magictree.toml",
        &format!(
            r#"
version = 1

[bootstrap]
after = [
  {{ command = "echo seeded > '{}'", ask = true }},
]

[[services]]
id = "idle"
command = "sleep 300"
port = {{ env = "PORT" }}
"#,
            marker.display()
        ),
    );
    fixture.git_repo();
    let state = fixture.state_dir();

    let plan = run(&["--dry-run", "up"], fixture.path(), &state);
    assert!(plan.ok(), "{}", plan.combined());
    assert!(
        plan.stdout.contains("asks before running"),
        "the plan has to say the step asks:\n{}",
        plan.stdout
    );

    let up = run(&["up"], fixture.path(), &state);
    assert!(up.ok(), "{}", up.combined());
    assert!(
        !marker.exists(),
        "a declined after step must not run:\n{}",
        up.combined()
    );
    assert!(
        up.stdout.contains("skipped"),
        "the decline has to be visible:\n{}",
        up.stdout
    );

    // A decline is never cached: the next run asks again.
    let again = run(&["up"], fixture.path(), &state);
    assert!(again.ok(), "{}", again.combined());
    assert!(
        !marker.exists() && again.stdout.contains("skipped"),
        "a declined step must stay uncached:\n{}",
        again.stdout
    );

    assert!(run(&["down"], fixture.path(), &state).ok());
}
