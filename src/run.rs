use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

pub fn sanitize(name: &str) -> String {
    name.replace(':', "_")
}

pub fn pid_file(runtime_dir: &Path, name: &str) -> PathBuf {
    runtime_dir
        .join("run")
        .join(format!("{}.pid", sanitize(name)))
}

pub fn log_file(runtime_dir: &Path, name: &str) -> PathBuf {
    runtime_dir
        .join("log")
        .join(format!("{}.log", sanitize(name)))
}

pub fn read_pid(runtime_dir: &Path, name: &str) -> Option<i32> {
    let raw = std::fs::read_to_string(pid_file(runtime_dir, name)).ok()?;
    raw.split_whitespace().next()?.parse::<i32>().ok()
}

/// The start-time stamp recorded next to the pid, when the pid file carries
/// one. Files written by older magictree versions hold only the pid.
fn read_stamp(runtime_dir: &Path, name: &str) -> Option<String> {
    let raw = std::fs::read_to_string(pid_file(runtime_dir, name)).ok()?;
    raw.split_whitespace()
        .nth(1)
        .map(str::to_string)
        .filter(|stamp| !stamp.is_empty())
}

/// The kernel start time of `pid`, collapsed to a single token (the pid file
/// keeps pid and stamp on one space-separated line), or None when the pid has
/// no process behind it. Compared against the pid file's stamp, so a pid
/// recycled after a reboot or wraparound is never mistaken for the recorded
/// process and signalled.
fn process_start(pid: i32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token: String = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_");
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

pub fn is_alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), Option::<Signal>::None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// Launch a long-running process in its own session so the whole tree can be
/// signalled later, with output captured to a per-run log file.
pub fn start(
    runtime_dir: &Path,
    name: &str,
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<i32> {
    std::fs::create_dir_all(runtime_dir.join("run"))?;
    std::fs::create_dir_all(runtime_dir.join("log"))?;
    let log_path = log_file(runtime_dir, name);
    let log =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;

    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .envs(env);
    unsafe {
        process.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|_| std::io::Error::last_os_error())
        });
    }
    let mut child = process
        .spawn()
        .with_context(|| format!("starting '{name}': {command}"))?;
    let pid = child.id() as i32;

    // Reap the child when it exits. Without this a process that dies during
    // startup lingers as a zombie, and liveness checks keep reporting it as
    // running, which makes a failure report actively misleading.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    let stamp = process_start(pid).unwrap_or_default();
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(pid_file(runtime_dir, name))?;
    writeln!(file, "{pid} {stamp}")?;
    Ok(pid)
}

/// Stop a supervised host process tree. Returns true when something was stopped.
pub fn stop(runtime_dir: &Path, name: &str, timeout: Duration) -> Result<bool> {
    let Some(pid) = read_pid(runtime_dir, name) else {
        return Ok(false);
    };
    let path = pid_file(runtime_dir, name);
    let stamp = read_stamp(runtime_dir, name);
    // The pid file names this worktree's process only while the kernel start
    // time still matches what was recorded. Without the stamp (an older pid
    // file) liveness alone decides, as before.
    let owned = |pid: i32| match &stamp {
        Some(stamp) => process_start(pid).as_deref() == Some(stamp.as_str()),
        None => is_alive(pid),
    };
    if !is_alive(pid) || !owned(pid) {
        let _ = std::fs::remove_file(&path);
        return Ok(false);
    }
    let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            break;
        }
        sleep(Duration::from_millis(100));
    }
    if is_alive(pid) {
        if owned(pid) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        // The pid was recycled mid-wait; the original process is gone either
        // way, and the newcomer is not ours to kill.
    }
    let _ = std::fs::remove_file(&path);
    Ok(true)
}

/// Every supervised process this worktree recorded, including entries left by
/// services that no longer exist in the manifest. Sorted by name.
pub fn recorded(runtime_dir: &Path) -> Vec<(String, i32)> {
    let dir = runtime_dir.join("run");
    let mut entries = Vec::new();
    let Ok(read) = std::fs::read_dir(&dir) else {
        return entries;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("pid") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if let Some(pid) = read_pid(runtime_dir, name) {
            entries.push((name.to_string(), pid));
        }
    }
    entries.sort();
    entries
}

/// Stop every supervised process recorded in this worktree, including pid
/// files left behind by services that no longer exist in the manifest.
/// Returns `(name, was_running)` for each entry, sorted by name.
pub fn stop_all(runtime_dir: &Path, timeout: Duration) -> Result<Vec<(String, bool)>> {
    let mut stopped = Vec::new();
    for (name, _) in recorded(runtime_dir) {
        let running = stop(runtime_dir, &name, timeout)?;
        stopped.push((name, running));
    }
    Ok(stopped)
}

/// The last `lines` lines a supervised process wrote, for failure reports.
pub fn log_tail(runtime_dir: &Path, name: &str, lines: usize) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(log_file(runtime_dir, name)) else {
        return Vec::new();
    };
    let all: Vec<&str> = raw.lines().filter(|line| !line.trim().is_empty()).collect();
    let start = all.len().saturating_sub(lines);
    all[start..].iter().map(|line| line.to_string()).collect()
}

/// Run a command to completion in the foreground. With `verbose`, the command's
/// output is relayed through magictree, indented, instead of the child writing
/// to the terminal itself.
pub fn run_once(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    verbose: bool,
) -> Result<()> {
    if !verbose {
        let status = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .envs(env)
            .status()
            .with_context(|| format!("running '{command}'"))?;
        anyhow::ensure!(status.success(), "'{command}' exited with {status}");
        return Ok(());
    }
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running '{command}'"))?;
    let reader = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_thread = std::thread::spawn(move || relay(reader));
    let err_thread = std::thread::spawn(move || relay(stderr));
    let status = child
        .wait()
        .with_context(|| format!("running '{command}'"))?;
    let _ = out_thread.join();
    let _ = err_thread.join();
    anyhow::ensure!(status.success(), "'{command}' exited with {status}");
    Ok(())
}

/// Relay a child's output line by line, indented under its step.
fn relay(reader: impl std::io::Read) {
    use std::io::BufRead;
    let mut stdout = std::io::stdout();
    for line in std::io::BufReader::new(reader).lines() {
        match line {
            Ok(line) => {
                let _ = writeln!(stdout, "     | {line}");
            }
            Err(_) => break,
        }
    }
    let _ = stdout.flush();
}
