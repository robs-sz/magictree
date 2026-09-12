use crate::run;
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Wait for a service to answer on its allocated port.
pub fn wait_for_port(port: u16, http_path: Option<&str>, timeout: Duration) -> Result<()> {
    wait_for_port_while(port, http_path, timeout, || true)
}

/// As `wait_for_port`, but gives up as soon as the supervised process is gone:
/// a service that died during startup will never answer, so waiting out the
/// full timeout only delays the error.
pub fn wait_for_port_while<F: Fn() -> bool>(
    port: u16,
    http_path: Option<&str>,
    timeout: Duration,
    alive: F,
) -> Result<()> {
    if !alive() {
        bail!("the process exited during startup");
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .build();
    let path = http_path.unwrap_or("/");
    let what = match http_path {
        Some(path) => format!("http://localhost:{port}{path}"),
        None => format!("tcp localhost:{port}"),
    };
    poll(
        || match http_path {
            Some(_) => LOOPBACK_HTTP.iter().any(|host| {
                match agent.get(&format!("http://{host}:{port}{path}")).call() {
                    // Any HTTP response means the server is accepting connections.
                    Ok(_) => true,
                    Err(ureq::Error::Status(_, _)) => true,
                    Err(_) => false,
                }
            }),
            None => LOOPBACK.iter().any(|host| connect(host, port)),
        },
        timeout,
        &what,
        Some(&alive),
    )
}

/// Loopback addresses a local service may bind. A dev server asked to listen on
/// `localhost` often binds IPv6 only, so probing a single family reports a
/// healthy service as unreachable.
pub const LOOPBACK: [&str; 2] = ["127.0.0.1", "[::1]"];
const LOOPBACK_HTTP: [&str; 2] = ["127.0.0.1", "[::1]"];

fn connect(host: &str, port: u16) -> bool {
    // Bracketed for IPv6 (`[::1]:3000`), plain otherwise.
    let address = format!("{host}:{port}");
    match std::net::ToSocketAddrs::to_socket_addrs(&address) {
        Ok(mut addrs) => addrs.any(|addr| {
            TcpStream::connect_timeout(&addr, Duration::from_secs(2))
                .map(|stream| {
                    let _ = stream.shutdown(Shutdown::Both);
                })
                .is_ok()
        }),
        Err(_) => false,
    }
}

pub fn wait_for_command(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<()> {
    poll(
        || run::run_once(command, cwd, env).is_ok(),
        timeout,
        &format!("`{command}`"),
        None,
    )
}

fn poll<F: FnMut() -> bool>(
    mut check: F,
    timeout: Duration,
    what: &str,
    alive: Option<&dyn Fn() -> bool>,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return Ok(());
        }
        if let Some(alive) = alive {
            if !alive() {
                bail!("the process exited during startup");
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out after {}s waiting for {what}", timeout.as_secs());
        }
        sleep(Duration::from_millis(250));
    }
}
