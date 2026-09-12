//! End-to-end CLI behaviour through the real binary.
//!
//! These tests avoid Docker so they run anywhere: the stack is host processes
//! only. Compose behaviour is covered by the unit-level tests and by manual
//! verification against real repositories.

mod support;

use std::time::{Duration, Instant};
use support::{have, run, Fixture};

fn host_stack_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "magictree.toml",
        r#"
version = 1

[bootstrap]
run = ["echo bootstrapped > .magictree/boot.txt"]

[[services]]
id = "idle"
command = "sleep 300"
port = { env = "PORT" }
"#,
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

    // `.magictree/` is kept out of git status without touching .gitignore.
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(fixture.path())
        .output()
        .expect("git status");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "",
        "generated state must not appear as an untracked change"
    );
    assert!(fixture.join(".magictree/ports.json").exists());
    assert!(
        fixture.join(".magictree/boot.txt").exists(),
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
    assert!(
        up.stderr.contains(".magictree/log/broken.log"),
        "{}",
        up.stderr
    );

    // The process is left running so the failure can be inspected.
    let status = run(&["status"], fixture.path(), &state);
    assert!(status.stdout.contains("running"), "{}", status.stdout);
    assert!(run(&["down"], fixture.path(), &state).ok());
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
