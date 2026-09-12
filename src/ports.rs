use crate::config::Config;
use crate::paths::Paths;
use crate::slug::hash64;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};

pub const ASSIGNMENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub version: u32,
    pub repo_key: String,
    pub worktree_id: String,
    /// Absolute path of the worktree this block belongs to, used to detect
    /// blocks left behind by removed worktrees.
    #[serde(default)]
    pub worktree_path: Option<String>,
    /// First port of the worktree's block.
    pub base: u16,
    /// Qualified service name to allocated host port.
    pub ports: BTreeMap<String, u16>,
}

#[derive(Debug, Clone)]
pub struct PortRequest {
    pub name: String,
    pub prefer: Option<u16>,
    pub require: Option<u16>,
}

enum BlockOutcome {
    Assigned(BTreeMap<String, u16>),
    /// The block cannot satisfy the request; another slot may work.
    Unusable,
}

/// Return this worktree's port assignment, allocating a block on first use and
/// extending it when new services appear.
pub fn ensure(
    paths: &Paths,
    config: &Config,
    repo_key: &str,
    worktree_id: &str,
    worktree_path: &Path,
    requests: &[PortRequest],
) -> Result<Assignment> {
    let blocks = paths.blocks_dir();
    std::fs::create_dir_all(&blocks).with_context(|| format!("creating {}", blocks.display()))?;

    let registry = read_registry(&blocks)?;
    // Every port another worktree recorded stays reserved, even while that
    // worktree is stopped: a `prefer` port is outside its own block, so nothing
    // else bounds it. Without this a second worktree takes the preferred port of
    // a stopped one, and when the owner comes back its dead service is reported
    // healthy by the new owner's process answering the probe. Released when the
    // owning assignment is, by `ports --reassign` or `gc`.
    let mut taken: HashMap<u16, String> = HashMap::new();
    for other in &registry {
        if other.repo_key == repo_key && other.worktree_id == worktree_id {
            continue;
        }
        for port in other.ports.values() {
            taken.insert(*port, other.worktree_id.clone());
        }
    }

    if let Some(mut assignment) = registry
        .iter()
        .find(|other| other.repo_key == repo_key && other.worktree_id == worktree_id)
        .cloned()
    {
        let mut changed = false;
        let recorded = Some(worktree_path.to_string_lossy().to_string());
        if assignment.worktree_path != recorded {
            assignment.worktree_path = recorded;
            changed = true;
        }
        for request in requests {
            if assignment.ports.contains_key(&request.name) {
                continue;
            }
            let port = extend(config, &assignment, request, &taken)?;
            assignment.ports.insert(request.name.clone(), port);
            changed = true;
        }
        if changed {
            write_assignment(&block_path(&blocks, assignment.base), &assignment)?;
        }
        return Ok(assignment);
    }

    let slots = config.slots();
    if slots == 0 {
        bail!("config: port range is too small for the configured stride");
    }
    let start = (hash64(&format!("{repo_key}:{worktree_id}")) % slots as u64) as u32;
    for step in 0..slots {
        let slot = (start + step) % slots;
        let base = config.port_range_start + (slot * config.port_stride as u32) as u16;
        let path = block_path(&blocks, base);
        if path.exists() {
            continue;
        }
        let ports = match assign_in_block(config, base, requests, &taken)? {
            BlockOutcome::Assigned(ports) => ports,
            BlockOutcome::Unusable => continue,
        };
        let assignment = Assignment {
            version: ASSIGNMENT_VERSION,
            repo_key: repo_key.to_string(),
            worktree_id: worktree_id.to_string(),
            worktree_path: Some(worktree_path.to_string_lossy().to_string()),
            base,
            ports,
        };
        if claim(&path, &assignment)? {
            return Ok(assignment);
        }
    }
    bail!(
        "no free port block in {}..{} — run `magictree gc` or widen the range in {}",
        config.port_range_start,
        config.port_range_end,
        paths.config_file().display()
    )
}

/// Read this worktree's existing assignment without allocating one.
pub fn load(paths: &Paths, repo_key: &str, worktree_id: &str) -> Result<Option<Assignment>> {
    find_assignment(&paths.blocks_dir(), repo_key, worktree_id)
}

/// Drop this worktree's assignment so the next `up` allocates fresh ports.
pub fn reassign(paths: &Paths, repo_key: &str, worktree_id: &str) -> Result<Option<PathBuf>> {
    let blocks = paths.blocks_dir();
    let Some(assignment) = find_assignment(&blocks, repo_key, worktree_id)? else {
        return Ok(None);
    };
    let path = block_path(&blocks, assignment.base);
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(Some(path))
}

fn extend(
    config: &Config,
    assignment: &Assignment,
    request: &PortRequest,
    taken: &HashMap<u16, String>,
) -> Result<u16> {
    let used: HashSet<u16> = assignment.ports.values().copied().collect();
    if let Some(required) = request.require {
        if used.contains(&required) {
            bail!("port.require {required} is already used in this worktree");
        }
        if let Some(owner) = taken.get(&required) {
            bail!(
                "port.require {required} for '{}' is held by worktree '{owner}'",
                request.name
            );
        }
        if !port_free(required) {
            bail!(
                "port.require {required} for '{}' is not available",
                request.name
            );
        }
        return Ok(required);
    }
    if let Some(preferred) = request.prefer {
        if !used.contains(&preferred) && !taken.contains_key(&preferred) && port_free(preferred) {
            return Ok(preferred);
        }
    }
    for offset in 0..config.port_stride {
        let candidate = assignment.base + offset;
        if candidate > config.port_range_end {
            break;
        }
        if used.contains(&candidate) || taken.contains_key(&candidate) || !port_free(candidate) {
            continue;
        }
        return Ok(candidate);
    }
    bail!(
        "no free port left in block {} for '{}' — run `magictree ports --reassign`",
        assignment.base,
        request.name
    )
}

fn assign_in_block(
    config: &Config,
    base: u16,
    requests: &[PortRequest],
    taken: &HashMap<u16, String>,
) -> Result<BlockOutcome> {
    let mut ports = BTreeMap::new();
    let mut used = HashSet::new();

    for request in requests {
        if let Some(required) = request.require {
            if !used.insert(required) {
                bail!("port.require {required} is requested more than once");
            }
            if let Some(owner) = taken.get(&required) {
                bail!(
                    "port.require {required} for '{}' is held by worktree '{owner}'",
                    request.name
                );
            }
            if !port_free(required) {
                bail!(
                    "port.require {required} for '{}' is not available",
                    request.name
                );
            }
            ports.insert(request.name.clone(), required);
        }
    }

    for request in requests {
        if ports.contains_key(&request.name) {
            continue;
        }
        if let Some(preferred) = request.prefer {
            if !used.contains(&preferred) && !taken.contains_key(&preferred) && port_free(preferred)
            {
                used.insert(preferred);
                ports.insert(request.name.clone(), preferred);
            }
        }
    }

    let mut offset = 0u16;
    for request in requests {
        if ports.contains_key(&request.name) {
            continue;
        }
        let mut assigned = None;
        while offset < config.port_stride {
            let candidate = base + offset;
            offset += 1;
            if candidate > config.port_range_end {
                return Ok(BlockOutcome::Unusable);
            }
            if used.contains(&candidate) || taken.contains_key(&candidate) {
                continue;
            }
            if !port_free(candidate) {
                return Ok(BlockOutcome::Unusable);
            }
            used.insert(candidate);
            assigned = Some(candidate);
            break;
        }
        match assigned {
            Some(port) => {
                ports.insert(request.name.clone(), port);
            }
            None => return Ok(BlockOutcome::Unusable),
        }
    }
    Ok(BlockOutcome::Assigned(ports))
}

/// A port is free only when every address shape a service might bind is free.
///
/// Probing loopback alone is not enough. On macOS a `SO_REUSEADDR` bind to
/// `127.0.0.1` or `::1` still succeeds while another process holds the wildcard,
/// so a dev server listening on `::` looked free: two worktrees were handed the
/// same port, the second service died with `EADDRINUSE`, and its health probe was
/// answered by the first worktree's process — recording a stack that was never up.
///
/// Each shape is caught only by its own probe, so all four are needed:
///
/// | holder | caught by |
/// |---|---|
/// | `127.0.0.1` | `127.0.0.1` |
/// | `::1` | `::1` |
/// | `0.0.0.0` | `0.0.0.0` |
/// | `::` | `::` and `0.0.0.0` |
///
/// A service bound to one specific non-loopback interface is still invisible
/// here; nothing magictree starts binds that way.
pub fn port_free(port: u16) -> bool {
    [
        ("0.0.0.0", port),
        ("::", port),
        ("127.0.0.1", port),
        ("::1", port),
    ]
    .into_iter()
    .all(is_bindable)
}

/// `AddrInUse` is the only failure that means another process holds the port: a
/// host without an IPv6 stack must not make every port look taken.
fn is_bindable<A: ToSocketAddrs>(address: A) -> bool {
    match TcpListener::bind(address) {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) => error.kind() != std::io::ErrorKind::AddrInUse,
    }
}

fn read_registry(blocks: &Path) -> Result<Vec<Assignment>> {
    let mut assignments = Vec::new();
    if !blocks.exists() {
        return Ok(assignments);
    }
    for entry in
        std::fs::read_dir(blocks).with_context(|| format!("reading {}", blocks.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(assignment) = serde_json::from_str::<Assignment>(&raw) else {
            continue;
        };
        assignments.push(assignment);
    }
    Ok(assignments)
}

fn find_assignment(blocks: &Path, repo_key: &str, worktree_id: &str) -> Result<Option<Assignment>> {
    Ok(read_registry(blocks)?.into_iter().find(|assignment| {
        assignment.repo_key == repo_key && assignment.worktree_id == worktree_id
    }))
}

fn block_path(blocks: &Path, base: u16) -> PathBuf {
    blocks.join(format!("{base}.json"))
}

fn claim(path: &Path, assignment: &Assignment) -> Result<bool> {
    let payload = serde_json::to_string_pretty(assignment)?;
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(payload.as_bytes())?;
            file.write_all(b"\n")?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error).with_context(|| format!("claiming {}", path.display())),
    }
}

fn write_assignment(path: &Path, assignment: &Assignment) -> Result<()> {
    let payload = serde_json::to_string_pretty(assignment)?;
    std::fs::write(path, format!("{payload}\n"))
        .with_context(|| format!("writing {}", path.display()))
}
