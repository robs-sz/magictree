use crate::manifest::Expose;
use crate::slug::short_hash;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct PortMapping {
    /// Allocated host port, when one is assigned.
    pub host: Option<u16>,
    /// Container-side port.
    pub target: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct GroupService {
    pub name: String,
    pub expose: Expose,
    pub mappings: Vec<PortMapping>,
}

#[derive(Debug, Clone)]
pub struct ComposeGroup {
    /// Absolute path to the repository's compose file (the first `-f`).
    pub file: PathBuf,
    pub project: String,
    /// Worktree this stack belongs to, recorded as a container label.
    pub worktree_path: String,
    /// Repository this stack belongs to. `gc` sweeps by this so a run in one
    /// repository can never collect another repository's containers.
    pub repo_key: String,
    pub services: Vec<GroupService>,
}

#[derive(Debug, Clone)]
pub struct ContainerState {
    pub service: String,
    pub state: String,
    pub health: Option<String>,
    /// Exit status for a container that has stopped. A one-shot init container
    /// that exits zero has done its job rather than failed.
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ComposeRunner {
    pub file: PathBuf,
    pub override_file: Option<PathBuf>,
    pub project: String,
}

impl ComposeRunner {
    pub fn new(file: PathBuf, override_file: Option<PathBuf>, project: String) -> Self {
        Self {
            file,
            override_file,
            project,
        }
    }

    pub fn run(
        &self,
        args: &[&str],
        env: &BTreeMap<String, String>,
    ) -> Result<std::process::Output> {
        let mut command = Command::new("docker");
        command.arg("compose").arg("-f").arg(&self.file);
        if let Some(override_file) = &self.override_file {
            if override_file.exists() {
                command.arg("-f").arg(override_file);
            }
        }
        command.arg("-p").arg(&self.project).args(args).envs(env);
        let output = command
            .output()
            .context("running docker compose (is docker installed?)")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "docker compose {} failed for {}: {}",
                args.join(" "),
                self.file.display(),
                stderr.trim()
            );
        }
        Ok(output)
    }

    pub fn up_service(&self, service: &str, env: &BTreeMap<String, String>) -> Result<()> {
        self.run(&["up", "-d", service], env).map(|_| ())
    }

    /// Stop one service's containers, keeping them: the next `up -d` starts
    /// them again with fresh configuration. A service with no container is a
    /// no-op.
    pub fn stop_service(&self, service: &str, env: &BTreeMap<String, String>) -> Result<()> {
        self.run(&["stop", service], env).map(|_| ())
    }

    pub fn down(&self, volumes: bool, env: &BTreeMap<String, String>) -> Result<()> {
        let mut args = vec!["down", "--remove-orphans"];
        if volumes {
            args.push("--volumes");
        }
        self.run(&args, env).map(|_| ())
    }

    /// All containers for this project, including ones that have exited, so a
    /// one-shot initialiser is visible after it finishes.
    pub fn ps(&self, env: &BTreeMap<String, String>) -> Result<Vec<ContainerState>> {
        let output = self.run(&["ps", "-a", "--format", "json"], env)?;
        parse_ps(&String::from_utf8_lossy(&output.stdout))
    }
}

pub fn parse_ps(raw: &str) -> Result<Vec<ContainerState>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let entries: Vec<Value> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).context("parsing docker compose ps output")?
    } else {
        trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).context("parsing docker compose ps line"))
            .collect::<Result<Vec<Value>>>()?
    };

    let mut states = Vec::new();
    for entry in entries {
        let service = string_field(&entry, &["Service", "service"]).unwrap_or_default();
        let state = string_field(&entry, &["State", "state"]).unwrap_or_default();
        let health = string_field(&entry, &["Health", "health"]).filter(|value| !value.is_empty());
        let exit_code = ["ExitCode", "exitCode", "exit_code"]
            .iter()
            .find_map(|key| entry.get(*key))
            .and_then(|value| match value {
                Value::Number(number) => number.as_i64(),
                Value::String(text) => text.parse::<i64>().ok(),
                _ => None,
            });
        states.push(ContainerState {
            service,
            state,
            health,
            exit_code,
        });
    }
    Ok(states)
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(|field| field.as_str())
        .map(|text| text.to_string())
}

/// Write the generated override for one compose file. The repository's compose
/// file is never modified. The service label records which worktree the stack
/// belongs to so `gc` can find leftovers after the checkout is gone.
pub fn write_override(runtime_dir: &Path, group: &ComposeGroup) -> Result<PathBuf> {
    let name = format!("override-{}.yml", short_hash(&group.file.to_string_lossy()));
    let path = runtime_dir.join(name);
    let mut out = String::from("# generated by magictree - do not edit\nservices:\n");
    for service in &group.services {
        out.push_str(&format!("  \"{}\":\n", service.name));
        out.push_str("    labels:\n");
        out.push_str(&format!(
            "      magictree.worktree: \"{}\"\n",
            group.worktree_path
        ));
        out.push_str(&format!("      magictree.project: \"{}\"\n", group.project));
        out.push_str(&format!("      magictree.repo: \"{}\"\n", group.repo_key));
        // Publish only when there is something to publish. `expose` defaults to
        // `port`, so a compose service that declares no ports used to emit
        // `ports: !override` with nothing under it, YAML null, which Compose
        // rejects outright ("services.x.ports must be a array"), so the service
        // could not start at all. Nothing declared means publish nothing, which
        // is also how `expose = "none"` clears ports the base file declares.
        let publish = service.expose == Expose::Port && !service.mappings.is_empty();
        if !publish {
            out.push_str("    ports: !reset []\n");
            continue;
        }
        let mut lines = String::new();
        for mapping in &service.mappings {
            let host = mapping
                .host
                .ok_or_else(|| anyhow!("service '{}' has an unallocated port", service.name))?;
            let target = mapping.target.ok_or_else(|| {
                anyhow!(
                    "service '{}' has a port without a container-side target",
                    service.name
                )
            })?;
            lines.push_str(&format!("      - \"127.0.0.1:{host}:{target}\"\n"));
        }
        out.push_str("    ports: !override\n");
        out.push_str(&lines);
    }
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub fn ensure_docker() -> Result<()> {
    Command::new("docker")
        .arg("--version")
        .output()
        .map(|_| ())
        .context("docker is not available on PATH")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_container_exit_codes() {
        // `docker compose ps --format json` may emit a bare object per line or
        // an array; both are accepted, and ExitCode decides whether an exited
        // container is an initialiser that finished or a failure.
        let line = r#"{"Service":"objects-init","State":"exited","ExitCode":0,"Health":""}"#;
        let states = parse_ps(line).expect("parse");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].service, "objects-init");
        assert_eq!(states[0].exit_code, Some(0));

        let array = r#"[{"Service":"api","State":"exited","ExitCode":2},{"Service":"db","State":"running","Health":"healthy"}]"#;
        let states = parse_ps(array).expect("parse");
        assert_eq!(states[0].exit_code, Some(2));
        assert_eq!(states[1].health.as_deref(), Some("healthy"));
        assert_eq!(states[1].exit_code, None);
    }

    #[test]
    fn empty_output_is_not_an_error() {
        assert!(parse_ps("").expect("parse").is_empty());
    }
}
