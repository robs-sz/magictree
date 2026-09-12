//! Fact extractors. Pure file reads: nothing from the repository is executed.

use super::report::*;
use crate::repo::Repo;
use anyhow::Result;
use serde_yaml::Value as Yaml;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Files that reveal a stack, in the order they are scanned.
const COMPOSE_NAMES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

pub fn extract(root: &Path) -> Result<Report> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let repo = Repo::open(&root).ok();

    let repo_facts = RepoFacts {
        root: root.to_string_lossy().to_string(),
        git_common_dir: repo
            .as_ref()
            .map(|repo| repo.common_dir.to_string_lossy().to_string())
            .unwrap_or_default(),
        is_git_repo: repo.is_some(),
        worktrees: repo
            .as_ref()
            .and_then(|repo| repo.worktrees().ok())
            .map(|entries| {
                entries
                    .into_iter()
                    .enumerate()
                    .map(|(index, entry)| WorktreeFact {
                        path: entry.path.to_string_lossy().to_string(),
                        branch: entry.branch,
                        is_main: index == 0,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };

    let mut builder = Builder {
        root: root.clone(),
        facts: Vec::new(),
        apps: Vec::new(),
        counter: 0,
    };

    builder.extract_workspace();
    builder.extract_root_stack();
    let report = builder.finish(repo_facts);
    Ok(report.finalize())
}

struct Builder {
    root: std::path::PathBuf,
    facts: Vec<Fact>,
    apps: Vec<AppFacts>,
    counter: usize,
}

impl Builder {
    fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("f{}", self.counter)
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|value| value.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }

    fn push(
        &mut self,
        path: &Path,
        kind: FactKind,
        app: Option<&str>,
        confidence: Confidence,
        data: FactData,
    ) {
        let id = self.next_id();
        let source = self.relative(path);
        self.facts.push(Fact {
            id,
            source,
            kind,
            app: app.map(|value| value.to_string()),
            confidence,
            data,
        });
    }

    /// Workspace tooling decides which directories are apps.
    fn extract_workspace(&mut self) {
        let mut packages: Vec<String> = Vec::new();
        let mut tool: Option<(String, std::path::PathBuf)> = None;

        let pnpm = self.root.join("pnpm-workspace.yaml");
        if let Some(value) = read_yaml(&pnpm) {
            let globs = string_list(value.get("packages"));
            if !globs.is_empty() {
                packages.extend(expand_globs(&self.root, &globs));
                tool = Some(("pnpm".to_string(), pnpm));
            }
        }

        let package_json = self.root.join("package.json");
        if let Some(value) = read_json(&package_json) {
            let workspaces = value.get("workspaces");
            let globs = match workspaces {
                Some(serde_json::Value::Object(map)) => string_list_json(map.get("packages")),
                other => string_list_json(other),
            };
            if !globs.is_empty() {
                packages.extend(expand_globs(&self.root, &globs));
                if tool.is_none() {
                    tool = Some(("npm-workspaces".to_string(), package_json.clone()));
                }
            }
        }

        let lerna = self.root.join("lerna.json");
        if let Some(value) = read_json(&lerna) {
            let globs = string_list_json(value.get("packages"));
            if !globs.is_empty() {
                packages.extend(expand_globs(&self.root, &globs));
                tool = Some(("lerna".to_string(), lerna));
            }
        }

        let mut seen = BTreeSet::new();
        let mut apps: Vec<AppFacts> = packages
            .into_iter()
            .filter(|dir| seen.insert(dir.clone()))
            .map(|dir| AppFacts {
                id: dir.rsplit('/').next().unwrap_or(&dir).to_string(),
                dir,
                source: tool
                    .as_ref()
                    .map(|(name, _)| name.clone())
                    .unwrap_or_else(|| "layout".to_string()),
            })
            .collect();

        if apps.is_empty() {
            apps = self.detect_apps_by_layout();
        }

        // App ids must be unique for `app:service` references to be meaningful.
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for app in &apps {
            *counts.entry(app.id.clone()).or_default() += 1;
        }
        for app in &mut apps {
            if counts.get(&app.id).copied().unwrap_or(0) > 1 {
                app.id = app.dir.replace('/', "-");
            }
        }

        if let Some((name, path)) = tool {
            self.push(
                &path,
                FactKind::Workspace,
                None,
                Confidence::High,
                FactData::Workspace {
                    tool: name,
                    packages: apps.iter().map(|app| app.dir.clone()).collect(),
                },
            );
        }

        let app_dirs: Vec<(String, String)> = apps
            .iter()
            .map(|app| (app.id.clone(), app.dir.clone()))
            .collect();
        self.apps = apps;

        for (id, dir) in app_dirs {
            self.extract_app(&id, &dir);
        }
    }

    /// No workspace tooling: look for conventional app directories.
    fn detect_apps_by_layout(&self) -> Vec<AppFacts> {
        let mut apps = Vec::new();
        for parent in ["apps", "packages", "services", "cmd", "web", "api"] {
            let dir = self.root.join(parent);
            if !dir.is_dir() {
                continue;
            }
            if is_app_dir(&dir) {
                apps.push(AppFacts {
                    id: parent.to_string(),
                    dir: parent.to_string(),
                    source: "layout".to_string(),
                });
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut children: Vec<String> = entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .filter(|entry| is_app_dir(&entry.path()))
                .filter_map(|entry| entry.file_name().to_str().map(|name| name.to_string()))
                .collect();
            children.sort();
            for child in children {
                apps.push(AppFacts {
                    id: child.clone(),
                    dir: format!("{parent}/{child}"),
                    source: "layout".to_string(),
                });
            }
        }
        // A plain single-app repository.
        if apps.is_empty()
            && (self.root.join("package.json").is_file()
                || self.root.join("pyproject.toml").is_file())
        {
            let id = self
                .root
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "app".to_string());
            apps.push(AppFacts {
                id,
                dir: ".".to_string(),
                source: "root".to_string(),
            });
        }
        apps
    }

    fn extract_app(&mut self, app_id: &str, dir: &str) {
        let app_root = if dir == "." {
            self.root.clone()
        } else {
            self.root.join(dir)
        };

        let package_json = app_root.join("package.json");
        if let Some(value) = read_json(&package_json) {
            let scripts = value
                .get("scripts")
                .and_then(|scripts| scripts.as_object())
                .map(|scripts| {
                    scripts
                        .iter()
                        .map(|(name, command)| ScriptFact {
                            name: name.clone(),
                            command: command.as_str().unwrap_or_default().to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let dependencies = ["dependencies", "devDependencies"]
                .iter()
                .filter_map(|key| value.get(key))
                .filter_map(|deps| deps.as_object())
                .flat_map(|deps| deps.keys().cloned())
                .collect();
            self.push(
                &package_json,
                FactKind::Node,
                Some(app_id),
                Confidence::High,
                FactData::Node {
                    name: value
                        .get("name")
                        .and_then(|name| name.as_str())
                        .map(|name| name.to_string()),
                    package_manager: value
                        .get("packageManager")
                        .and_then(|manager| manager.as_str())
                        .map(|manager| manager.to_string()),
                    scripts,
                    has_workspaces: value.get("workspaces").is_some(),
                    dependencies,
                },
            );
        }

        let pyproject = app_root.join("pyproject.toml");
        if let Some(value) = read_toml(&pyproject) {
            let groups = value
                .get("dependency-groups")
                .and_then(|groups| groups.as_table())
                .map(|groups| groups.keys().cloned().collect())
                .unwrap_or_default();
            let scripts = value
                .get("project")
                .and_then(|project| project.get("scripts"))
                .and_then(|scripts| scripts.as_table())
                .map(|scripts| scripts.keys().cloned().collect())
                .unwrap_or_default();
            let manager = if app_root.join("uv.lock").is_file() {
                "uv"
            } else if app_root.join("poetry.lock").is_file() {
                "poetry"
            } else {
                "pip"
            };
            self.push(
                &pyproject,
                FactKind::Python,
                Some(app_id),
                Confidence::High,
                FactData::Python {
                    manager: manager.to_string(),
                    has_uv_lock: app_root.join("uv.lock").is_file(),
                    dependency_groups: groups,
                    scripts,
                },
            );
        }

        let justfile = app_root.join("justfile");
        if justfile.is_file() {
            let facts = parse_justfile(&justfile);
            self.push(
                &justfile,
                FactKind::Just,
                Some(app_id),
                Confidence::High,
                FactData::Just {
                    recipes: facts.recipes,
                    modules: facts.modules,
                    env_variables: facts.env_variables,
                    exported_parameters: facts.exported_parameters,
                    recipe_bodies: facts.recipe_bodies,
                    variables: facts.variables,
                },
            );
        }

        let mise = app_root.join("mise.toml");
        if mise.is_file() {
            let (tasks, tools, has_env, has_profiles) = parse_mise(&mise);
            self.push(
                &mise,
                FactKind::Mise,
                Some(app_id),
                Confidence::High,
                FactData::Mise {
                    tasks,
                    tools,
                    has_env,
                    has_profiles,
                },
            );
        }
    }

    /// Repository-level stack: compose, task runners, env templates, Procfiles.
    fn extract_root_stack(&mut self) {
        let mut compose_files: Vec<std::path::PathBuf> = Vec::new();
        for name in COMPOSE_NAMES {
            let path = self.root.join(name);
            if path.is_file() {
                compose_files.push(path);
            }
        }
        for dir in ["infra", "deployment", "docker"] {
            for name in COMPOSE_NAMES {
                let path = self.root.join(dir).join(name);
                if path.is_file() {
                    compose_files.push(path);
                }
            }
        }

        for path in compose_files {
            if let Some(value) = read_yaml(&path) {
                if let Some(services) = value
                    .get("services")
                    .and_then(|services| services.as_mapping())
                {
                    let mut extracted = Vec::new();
                    for (name, service) in services {
                        let Some(name) = name.as_str() else { continue };
                        extracted.push(compose_service(name, service));
                    }
                    extracted.sort_by(|left, right| left.name.cmp(&right.name));
                    let has_build = extracted.iter().any(|service| service.has_build);
                    self.push(
                        &path,
                        FactKind::Compose,
                        None,
                        Confidence::High,
                        FactData::Compose {
                            services: extracted,
                            has_build,
                            files: vec![self.relative(&path)],
                        },
                    );
                }
            }
        }

        let mise = self.root.join("mise.toml");
        if mise.is_file() {
            let (tasks, tools, has_env, has_profiles) = parse_mise(&mise);
            self.push(
                &mise,
                FactKind::Mise,
                None,
                Confidence::High,
                FactData::Mise {
                    tasks,
                    tools,
                    has_env,
                    has_profiles,
                },
            );
        }

        let justfile = self.root.join("justfile");
        if justfile.is_file() {
            let facts = parse_justfile(&justfile);
            self.push(
                &justfile,
                FactKind::Just,
                None,
                Confidence::High,
                FactData::Just {
                    recipes: facts.recipes,
                    modules: facts.modules,
                    env_variables: facts.env_variables,
                    exported_parameters: facts.exported_parameters,
                    recipe_bodies: facts.recipe_bodies,
                    variables: facts.variables,
                },
            );
        }

        for name in [".env.example", ".env.sample", ".env.template"] {
            let path = self.root.join(name);
            if let Ok(raw) = std::fs::read_to_string(&path) {
                let variables = raw
                    .lines()
                    .filter_map(|line| {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            return None;
                        }
                        line.split('=').next().map(|key| key.trim().to_string())
                    })
                    .filter(|key| !key.is_empty())
                    .collect();
                self.push(
                    &path,
                    FactKind::EnvExample,
                    None,
                    Confidence::Medium,
                    FactData::EnvExample {
                        variables,
                        file: name.to_string(),
                    },
                );
            }
        }

        let procfile = self.root.join("Procfile");
        if let Ok(raw) = std::fs::read_to_string(&procfile) {
            let processes = raw
                .lines()
                .filter_map(|line| {
                    let (name, command) = line.split_once(':')?;
                    Some(ScriptFact {
                        name: name.trim().to_string(),
                        command: command.trim().to_string(),
                    })
                })
                .collect();
            self.push(
                &procfile,
                FactKind::Procfile,
                None,
                Confidence::High,
                FactData::Procfile { processes },
            );
        }
    }

    fn finish(self, repo: RepoFacts) -> Report {
        let unknowns = derive_unknowns(&self.root, &self.apps, &self.facts);
        Report {
            report_version: REPORT_VERSION,
            report_hash: String::new(),
            repo,
            apps: self.apps,
            facts: self.facts,
            unknowns,
        }
    }
}

fn compose_service(name: &str, service: &Yaml) -> ComposeServiceFact {
    let ports = service
        .get("ports")
        .and_then(|ports| ports.as_sequence())
        .map(|ports| ports.iter().filter_map(port_string).collect())
        .unwrap_or_default();
    let depends_on = match service.get("depends_on") {
        Some(Yaml::Sequence(values)) => values
            .iter()
            .filter_map(|value| value.as_str().map(|name| name.to_string()))
            .collect(),
        Some(Yaml::Mapping(values)) => values
            .keys()
            .filter_map(|key| key.as_str().map(|name| name.to_string()))
            .collect(),
        _ => Vec::new(),
    };
    let profiles = string_list(service.get("profiles"));
    let command = match service.get("command") {
        Some(Yaml::String(value)) => Some(value.clone()),
        Some(Yaml::Sequence(values)) => Some(
            values
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    };
    ComposeServiceFact {
        name: name.to_string(),
        image: service
            .get("image")
            .and_then(|image| image.as_str())
            .map(|image| image.to_string()),
        has_build: service.get("build").is_some(),
        ports,
        depends_on,
        has_healthcheck: service.get("healthcheck").is_some(),
        profiles,
        command,
    }
}

fn port_string(value: &Yaml) -> Option<String> {
    match value {
        Yaml::String(text) => Some(text.clone()),
        Yaml::Number(number) => Some(number.to_string()),
        Yaml::Mapping(map) => {
            let target = scalar(map.get(Yaml::String("target".into()))?)?;
            let published = map.get(Yaml::String("published".into())).and_then(scalar);
            Some(match published {
                Some(published) => format!("{published}:{target}"),
                None => target,
            })
        }
        _ => None,
    }
}

/// Render a YAML scalar as text, without relying on `Display`.
fn scalar(value: &Yaml) -> Option<String> {
    match value {
        Yaml::String(text) => Some(text.clone()),
        Yaml::Number(number) => Some(number.to_string()),
        Yaml::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

/// Terminal port of a compose mapping, and any variable name used for the host
/// side. `${WT_PORT_DB:-5432}:5432` yields target 5432 and variable WT_PORT_DB.
pub fn parse_port_mapping(raw: &str) -> (Option<u16>, Option<String>) {
    let raw = raw.trim().trim_matches('"');
    let parts = split_mapping(raw);
    let (host, target) = match parts.len() {
        0 => return (None, None),
        1 => (None, parts[0]),
        _ => (Some(parts[parts.len() - 2]), parts[parts.len() - 1]),
    };
    let target = target
        .split('/')
        .next()
        .and_then(|value| value.trim().parse::<u16>().ok());
    let variable = host.and_then(|host| {
        let host = host.trim();
        let start = host.find("${")?;
        let rest = &host[start + 2..];
        let end = rest.find('}')?;
        let inner = &rest[..end];
        Some(inner.split(":-").next().unwrap_or(inner).trim().to_string())
    });
    (target, variable)
}

/// Split a compose port mapping on `:` at brace depth zero, so a default value
/// inside `${VAR:-default}` is not mistaken for a separator.
fn split_mapping(raw: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let bytes = raw.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'$' if index + 1 < bytes.len() && bytes[index + 1] == b'{' => depth += 1,
            b'}' if depth > 0 => depth -= 1,
            b':' if depth == 0 => {
                parts.push(&raw[start..index]);
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(&raw[start..]);
    parts
}

fn is_app_dir(path: &Path) -> bool {
    [
        "package.json",
        "pyproject.toml",
        "go.mod",
        "Cargo.toml",
        "justfile",
    ]
    .iter()
    .any(|name| path.join(name).is_file())
}

fn expand_globs(root: &Path, globs: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for pattern in globs {
        if let Some(prefix) = pattern.strip_suffix("/*") {
            let dir = root.join(prefix);
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.path().is_dir() && is_app_dir(&entry.path()) {
                        if let Some(name) = entry.file_name().to_str() {
                            out.insert(format!("{prefix}/{name}"));
                        }
                    }
                }
            }
        } else {
            let candidate = root.join(pattern);
            if candidate.is_dir() {
                out.insert(pattern.clone());
            }
        }
    }
    out.into_iter().collect()
}

/// Package-manager install command implied by the lockfiles present.
pub fn install_command(app_root: &Path) -> Option<String> {
    let has = |name: &str| app_root.join(name).is_file();
    if has("pnpm-lock.yaml") {
        return Some("pnpm install".to_string());
    }
    if has("yarn.lock") {
        return Some("yarn install".to_string());
    }
    if has("bun.lockb") || has("bun.lock") {
        return Some("bun install".to_string());
    }
    if has("package-lock.json") {
        return Some("npm install".to_string());
    }
    if has("uv.lock") {
        return Some("uv sync --group dev".to_string());
    }
    if has("poetry.lock") {
        return Some("poetry install".to_string());
    }
    if has("pyproject.toml") {
        return Some("uv sync".to_string());
    }
    if has("package.json") {
        return Some("npm install".to_string());
    }
    None
}

/// Install inputs, used as bootstrap cache keys.
pub fn install_inputs(app_root: &Path) -> Vec<String> {
    let has = |name: &str| app_root.join(name).is_file();
    let mut inputs = Vec::new();
    for name in [
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lockb",
        "bun.lock",
        "package-lock.json",
        "package.json",
        "uv.lock",
        "poetry.lock",
        "pyproject.toml",
    ] {
        if has(name) {
            inputs.push(name.to_string());
        }
    }
    inputs
}

fn derive_unknowns(root: &Path, apps: &[AppFacts], facts: &[Fact]) -> Vec<Unknown> {
    let mut unknowns = Vec::new();

    if apps.len() > 1 {
        unknowns.push(Unknown {
            id: "stack.members".to_string(),
            scope: Scope::Workspace,
            kind: UnknownKind::MultiChoice,
            question: "Which apps are part of the default stack?".to_string(),
            options: apps.iter().map(|app| app.dir.clone()).collect(),
            evidence: Vec::new(),
            default: None,
        });
    }

    let compose_fact = facts.iter().find(|fact| fact.kind == FactKind::Compose);
    if let Some(fact) = compose_fact {
        if let FactData::Compose { services, .. } = &fact.data {
            let names: Vec<String> = services
                .iter()
                .map(|service| service.name.clone())
                .collect();
            // The compose file describes the stack, so every service is managed
            // by default. A repository that builds per-app dev containers can
            // deselect the ones an app manifest already covers.
            let infra: Vec<String> = services
                .iter()
                .map(|service| service.name.clone())
                .collect();
            unknowns.push(Unknown {
                id: "compose.shared".to_string(),
                scope: Scope::Workspace,
                kind: UnknownKind::MultiChoice,
                question: "Which compose services are shared infrastructure rather than app-owned?"
                    .to_string(),
                options: names.clone(),
                evidence: vec![fact.id.clone()],
                default: Some(infra.join(",")),
            });
            let published: Vec<String> = services
                .iter()
                .filter(|service| !service.ports.is_empty())
                .map(|service| service.name.clone())
                .collect();
            if !published.is_empty() {
                unknowns.push(Unknown {
                    id: "compose.expose".to_string(),
                    scope: Scope::Workspace,
                    kind: UnknownKind::MultiChoice,
                    question: "Which compose services must be reachable from the host?".to_string(),
                    options: published,
                    evidence: vec![fact.id.clone()],
                    // Nothing is exposed by default: a service only needs a host
                    // port when something outside its compose network calls it.
                    default: Some(String::new()),
                });
            }
        }
    }

    for app in apps {
        // An app that reads its port from a specific variable will ignore PORT,
        // so the generated manifest has to name that variable.
        let port_variables = port_variable_candidates(app, facts);
        if !port_variables.is_empty() {
            let preferred = port_variables
                .iter()
                .find(|name| {
                    name.to_ascii_uppercase()
                        .contains(&app.id.to_ascii_uppercase())
                })
                .cloned()
                .unwrap_or_else(|| port_variables[0].clone());
            unknowns.push(Unknown {
                id: format!("{}.port_env", app.id),
                scope: Scope::App {
                    app: app.id.clone(),
                },
                kind: UnknownKind::Choice,
                question: "Which variable should carry this app's port?".to_string(),
                options: port_variables,
                evidence: Vec::new(),
                default: Some(preferred),
            });
        }

        // Files a dev server imports may be generated rather than committed, so
        // the steps that produce them belong in bootstrap.
        let setup = setup_candidates(root, app, facts);
        if !setup.is_empty() {
            let mut options: Vec<String> = vec!["none".to_string()];
            options.extend(setup.iter().map(|candidate| candidate.value.clone()));
            let has_recipe = setup
                .iter()
                .any(|candidate| candidate.value.starts_with("just:"));
            let defaults: Vec<String> = if has_recipe {
                setup
                    .iter()
                    .filter(|candidate| candidate.value.starts_with("just:"))
                    .map(|candidate| candidate.value.clone())
                    .collect()
            } else {
                setup
                    .first()
                    .map(|candidate| vec![candidate.value.clone()])
                    .unwrap_or_default()
            };
            unknowns.push(Unknown {
                id: format!("{}.setup", app.id),
                scope: Scope::App {
                    app: app.id.clone(),
                },
                kind: UnknownKind::MultiChoice,
                question: "Which steps generate files before this app can start?".to_string(),
                options,
                evidence: setup
                    .iter()
                    .map(|candidate| candidate.evidence.clone())
                    .collect(),
                default: Some(defaults.join(",")),
            });
        }

        let candidates = run_candidates(root, app, facts);
        if candidates.is_empty() {
            continue;
        }
        let mut options: Vec<String> = vec!["skip".to_string()];
        options.extend(candidates.iter().map(|candidate| candidate.value.clone()));
        let default = candidates.first().map(|candidate| candidate.value.clone());
        let evidence: Vec<String> = candidates
            .iter()
            .map(|candidate| candidate.evidence.clone())
            .collect();
        unknowns.push(Unknown {
            id: format!("{}.run", app.id),
            scope: Scope::App {
                app: app.id.clone(),
            },
            kind: UnknownKind::Choice,
            question: "How should this app be started?".to_string(),
            options,
            evidence,
            default: Some(default.unwrap_or_else(|| "skip".to_string())),
        });
    }

    unknowns
}

/// A way to run an app, with a crude score used only to pick a default.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Stable spec, for example `pnpm:dev` or `just:api::dev`.
    pub value: String,
    pub score: u32,
    /// Fact this candidate came from.
    pub evidence: String,
}

/// True when a command is already expressible with a runner we have a target
/// for, so offering it again as a raw command would just duplicate it.
fn native_runner_covers(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or("");
    matches!(first, "npm" | "pnpm" | "yarn" | "bun" | "mise" | "just")
        || command.starts_with("uv run npx")
}

/// Setup steps a repository declares for generating files a dev server needs
/// (API clients, schema types). Only names that exist in the repository are
/// offered, and only ones that are for generation rather than serving.
fn setup_candidates(root: &Path, app: &AppFacts, facts: &[Fact]) -> Vec<Candidate> {
    let is_generation = |name: &str| {
        let lowered = name.to_ascii_lowercase();
        [
            "openapi", "codegen", "generate", "protobuf", "prisma", "graphql",
        ]
        .iter()
        .any(|needle| lowered.contains(needle))
            || lowered == "gen"
            || lowered.starts_with("gen:")
    };

    let mut out: Vec<Candidate> = Vec::new();
    for fact in facts
        .iter()
        .filter(|fact| fact.app.as_deref() == Some(app.id.as_str()))
    {
        match &fact.data {
            // Recipe names win: the task runner usually supplies the extra
            // environment a generation step needs.
            FactData::Just { recipes, .. } => {
                for recipe in recipes {
                    if is_generation(recipe) {
                        out.push(Candidate {
                            value: format!("just:{recipe}"),
                            score: 80,
                            evidence: fact.id.clone(),
                        });
                    }
                }
            }
            FactData::Node {
                scripts,
                package_manager,
                ..
            } => {
                let app_root = if app.dir == "." {
                    root.to_path_buf()
                } else {
                    root.join(&app.dir)
                };
                let from_lock = install_command(&app_root)
                    .and_then(|command| command.split_whitespace().next().map(str::to_string));
                let runner = match package_manager.as_deref().map(str::to_string).or(from_lock) {
                    Some(value) if value.starts_with("pnpm") => "pnpm",
                    Some(value) if value.starts_with("yarn") => "yarn",
                    Some(value) if value.starts_with("bun") => "bun",
                    _ => "npm",
                };
                for script in scripts {
                    if is_generation(&script.name) {
                        out.push(Candidate {
                            value: format!("{runner}:{}", script.name),
                            score: 70,
                            evidence: fact.id.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then(left.value.cmp(&right.value))
    });
    out.dedup_by(|left, right| left.value == right.value);
    out
}

/// Environment variables that plausibly carry an app's port, gathered from the
/// justfile it uses and any committed env template.
fn port_variable_candidates(app: &AppFacts, facts: &[Fact]) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    let looks_like_port = |name: &str| {
        let upper = name.to_ascii_uppercase();
        upper.contains("PORT") && upper != "SUPPORT"
    };
    for fact in facts
        .iter()
        .filter(|fact| fact.app.as_deref() == Some(app.id.as_str()))
    {
        if let FactData::Just { env_variables, .. } = &fact.data {
            for name in env_variables {
                if looks_like_port(name) {
                    names.insert(name.clone());
                }
            }
        }
    }
    for fact in facts.iter().filter(|fact| fact.app.is_none()) {
        if let FactData::EnvExample { variables, .. } = &fact.data {
            for name in variables {
                if looks_like_port(name) {
                    names.insert(name.clone());
                }
            }
        }
    }
    // A variable naming the app itself is the most specific option.
    let mut ordered: Vec<String> = names.into_iter().collect();
    ordered.sort_by_key(|name| {
        let upper = name.to_ascii_uppercase();
        let app_upper = app.id.to_ascii_uppercase();
        (
            !upper.contains(&app_upper),
            !upper.starts_with("WT_"),
            name.clone(),
        )
    });
    ordered
}

/// Candidate ways to run an app, drawn only from files that exist.
fn run_candidates(root: &Path, app: &AppFacts, facts: &[Fact]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for fact in facts
        .iter()
        .filter(|fact| fact.app.as_deref() == Some(app.id.as_str()))
    {
        match &fact.data {
            FactData::Node {
                scripts,
                package_manager,
                ..
            } => {
                // `packageManager` is optional, so fall back to whatever the
                // lockfiles say rather than assuming npm.
                let app_root = if app.dir == "." {
                    root.to_path_buf()
                } else {
                    root.join(&app.dir)
                };
                let from_lock = install_command(&app_root)
                    .and_then(|command| command.split_whitespace().next().map(str::to_string));
                let runner = match package_manager.as_deref().map(str::to_string).or(from_lock) {
                    Some(value) if value.starts_with("pnpm") => "pnpm",
                    Some(value) if value.starts_with("yarn") => "yarn",
                    Some(value) if value.starts_with("bun") => "bun",
                    Some(value) if value.starts_with("npm") => "npm",
                    Some(value) => {
                        // A manager we have no target for: fall through to a
                        // plain command so nothing is reinterpreted.
                        let _ = value;
                        "npm"
                    }
                    None => "npm",
                };
                for script in scripts {
                    let score = match script.name.as_str() {
                        "dev" => 100,
                        "start" | "serve" => 90,
                        name if name.starts_with("dev:") => 80,
                        name if name.starts_with("start:") => 70,
                        _ => 0,
                    };
                    if score > 0 {
                        out.push(Candidate {
                            value: format!("{runner}:{}", script.name),
                            score,
                            evidence: fact.id.clone(),
                        });
                    }
                }
            }
            FactData::Just {
                recipes,
                modules,
                recipe_bodies,
                variables,
                ..
            } => {
                for recipe in recipes {
                    let score = match recipe.as_str() {
                        "dev" => 95,
                        "serve" | "start" => 85,
                        _ => 0,
                    };
                    if score == 0 {
                        continue;
                    }
                    out.push(Candidate {
                        value: format!("just:{recipe}"),
                        score,
                        evidence: fact.id.clone(),
                    });
                    // The recipe body often names a command we can run without
                    // the task runner. Offer it so the repository's tooling can
                    // be left out of the loop, but only when no native runner
                    // already covers it — otherwise it is the same command twice.
                    if let Some(body) = recipe_bodies.get(recipe) {
                        if let Some(command) = inline_recipe(body, variables) {
                            if !native_runner_covers(&command) {
                                out.push(Candidate {
                                    value: format!("command:{command}"),
                                    score: score - 5,
                                    evidence: fact.id.clone(),
                                });
                            }
                        }
                    }
                }
                for module in modules {
                    let score = match module.as_str() {
                        "dev" | "api" | "web" | "stack" => 75,
                        _ => 0,
                    };
                    if score > 0 {
                        out.push(Candidate {
                            value: format!("just:{module}::dev"),
                            score,
                            evidence: fact.id.clone(),
                        });
                    }
                }
            }
            FactData::Mise { tasks, .. } => {
                for task in tasks {
                    let score = match task.as_str() {
                        "dev" => 90,
                        "serve" | "start" => 80,
                        _ => 0,
                    };
                    if score > 0 {
                        out.push(Candidate {
                            value: format!("mise:{task}"),
                            score,
                            evidence: fact.id.clone(),
                        });
                    }
                }
            }
            FactData::Python { scripts, .. } => {
                for script in scripts {
                    out.push(Candidate {
                        value: format!("uv:{script}"),
                        score: 60,
                        evidence: fact.id.clone(),
                    });
                }
            }
            FactData::Procfile { processes } => {
                for process in processes {
                    out.push(Candidate {
                        value: format!("procfile:{}", process.name),
                        score: 50,
                        evidence: fact.id.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    out.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then(left.value.cmp(&right.value))
    });
    out.dedup_by(|left, right| left.value == right.value);
    out
}

pub fn read_yaml(path: &Path) -> Option<Yaml> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&raw).ok()
}

pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn read_toml(path: &Path) -> Option<toml::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    toml::from_str(&raw).ok()
}

pub fn get_toml<'a>(value: &'a toml::Value, path: &[&str]) -> Option<&'a toml::Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn string_list(value: Option<&Yaml>) -> Vec<String> {
    match value {
        Some(Yaml::Sequence(values)) => values
            .iter()
            .filter_map(|value| value.as_str().map(|text| text.to_string()))
            .collect(),
        Some(Yaml::String(text)) => vec![text.clone()],
        _ => Vec::new(),
    }
}

fn string_list_json(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(|value| value.as_str().map(|text| text.to_string()))
            .collect(),
        Some(serde_json::Value::String(text)) => vec![text.clone()],
        _ => Vec::new(),
    }
}

/// What a justfile reveals: recipes, modules, the variables it reads from the
/// environment, the variables it sets for recipes, and the recipe bodies
/// themselves so a direct command can be recovered without inventing one.
#[derive(Debug, Default, Clone)]
pub struct JustFacts {
    pub recipes: Vec<String>,
    pub modules: Vec<String>,
    pub env_variables: Vec<String>,
    pub exported_parameters: Vec<String>,
    pub recipe_bodies: BTreeMap<String, String>,
    /// Just variable name to the environment variable it reads.
    pub variables: BTreeMap<String, String>,
}

fn parse_justfile(path: &Path) -> JustFacts {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return JustFacts::default();
    };
    let mut facts = JustFacts::default();
    let mut reads = BTreeSet::new();
    for name in env_reads(&raw) {
        reads.insert(name);
    }
    facts.exported_parameters = exported_parameters(&raw);

    let lines: Vec<&str> = raw.lines().collect();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.is_empty() || line.starts_with([' ', '\t', '#', '[']) {
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("mod ") {
            if let Some(name) = rest.split_whitespace().next() {
                facts.modules.push(name.to_string());
            }
            index += 1;
            continue;
        }
        let directives = ["set ", "import ", "alias ", "unexport "];
        if directives.iter().any(|keyword| line.starts_with(keyword)) {
            index += 1;
            continue;
        }

        let Some((head, rest)) = line.split_once(':') else {
            index += 1;
            continue;
        };
        let name = head
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            index += 1;
            continue;
        }

        // `name := value` binds a variable; `export name := value` also exposes it.
        let assignment = rest.trim_start().starts_with('=');
        if assignment || line.starts_with("export ") {
            let value = rest.trim_start().trim_start_matches('=').trim();
            if let Some(env_name) = env_reads(value).into_iter().next() {
                facts.variables.insert(name, env_name);
            }
            index += 1;
            continue;
        }

        if !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            index += 1;
            continue;
        }

        // Collect the indented body that follows.
        let mut body = Vec::new();
        let mut cursor = index + 1;
        while cursor < lines.len() {
            let next = lines[cursor];
            if next.trim().is_empty() || !next.starts_with([' ', '\t']) {
                break;
            }
            body.push(next.trim().to_string());
            cursor += 1;
        }
        facts.recipes.push(name.clone());
        facts.recipe_bodies.insert(name, body.join("\n"));
        index = cursor;
    }

    facts.env_variables = reads
        .into_iter()
        .filter(|name| !facts.variables.values().any(|env| env == name))
        .chain(facts.variables.values().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    facts
}

/// Recover a single shell command from a recipe body, substituting just
/// variables that are read from the environment with their environment form.
/// Returns `None` when the body cannot be reduced to one plain command.
pub fn inline_recipe(body: &str, variables: &BTreeMap<String, String>) -> Option<String> {
    let mut commands: Vec<&str> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A guard such as `{{ if mode == "ui" { error(...) } else { "" } }}`
        // only exists to reject a case we are not reproducing.
        if trimmed.starts_with("{{") && trimmed.ends_with("}}") && trimmed.contains("error(") {
            continue;
        }
        commands.push(trimmed);
    }
    if commands.len() != 1 {
        return None;
    }
    let mut command = commands[0].to_string();
    while let Some(start) = command.find("{{") {
        let end = command[start..].find("}}")? + start;
        let inner = command[start + 2..end].trim().to_string();
        let environment = variables.get(&inner)?;
        command.replace_range(start..end + 2, &format!("${environment}"));
    }
    if command.contains("{{") {
        return None;
    }
    Some(command)
}

/// Names a justfile sets for a recipe with `$NAME=`. An ambient value for these
/// is overwritten by the parameter, so they are not inputs.
fn exported_parameters(raw: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    for line in raw.lines() {
        for (index, _) in line.match_indices('$') {
            let after = &line[index + 1..];
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let rest = &after[name.len()..];
            if !name.is_empty() && rest.trim_start().starts_with('=') {
                names.insert(name);
            }
        }
    }
    names.into_iter().collect()
}

/// Environment variable names a justfile reads from the ambient environment:
/// only `env("X")` and `env_var("X")` qualify. Values the justfile assigns,
/// exports, or passes to a recipe are outputs — an ambient value for those is
/// overwritten rather than read.
fn env_reads(raw: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    for marker in ["env(\"", "env_var(\"", "env('", "env_var('"] {
        let mut rest = raw;
        while let Some(start) = rest.find(marker) {
            let after = &rest[start + marker.len()..];
            let quote = marker.chars().last().unwrap_or('"');
            if let Some(end) = after.find(quote) {
                let name = after[..end].trim();
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    names.insert(name.to_string());
                }
            }
            rest = after;
        }
    }
    names.into_iter().collect()
}

fn parse_mise(path: &Path) -> (Vec<String>, Vec<String>, bool, bool) {
    let Some(value) = read_toml(path) else {
        return (Vec::new(), Vec::new(), false, false);
    };
    let tasks = value
        .get("tasks")
        .and_then(|tasks| tasks.as_table())
        .map(|tasks| tasks.keys().cloned().collect())
        .unwrap_or_default();
    let tools = value
        .get("tools")
        .and_then(|tools| tools.as_table())
        .map(|tools| tools.keys().cloned().collect())
        .unwrap_or_default();
    let has_env = value.get("env").is_some();
    let has_profiles = value.get("profiles").is_some();
    (tasks, tools, has_env, has_profiles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_variable_port_mappings() {
        let (target, variable) = parse_port_mapping("${WT_PORT_DB:-5432}:5432");
        assert_eq!(target, Some(5432));
        assert_eq!(variable.as_deref(), Some("WT_PORT_DB"));

        let (target, variable) = parse_port_mapping("9000:9000");
        assert_eq!(target, Some(9000));
        assert_eq!(variable, None);

        let (target, variable) = parse_port_mapping("127.0.0.1:3100:3000");
        assert_eq!(target, Some(3000));
        assert_eq!(variable, None);

        let (target, _) = parse_port_mapping("${PORT:-8080}:8080/tcp");
        assert_eq!(target, Some(8080));
    }

    #[test]
    fn reads_justfile_recipes_and_modules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("justfile");
        std::fs::write(
            &path,
            "set shell := [\"bash\", \"-eu\"]\nset dotenv-load\nexport FOO := \"bar\"\nmod stack 'deployment/local'\nalias b := build\n\n# comment\ndev port='3000':\n  echo {{port}}\n\nserve:\n  echo hi\n",
        )
        .unwrap();
        let facts = parse_justfile(&path);
        let (recipes, modules) = (&facts.recipes, &facts.modules);
        assert_eq!(recipes, &vec!["dev".to_string(), "serve".to_string()]);
        assert!(!recipes.contains(&"set".to_string()));
        assert!(!recipes.contains(&"export".to_string()));
        assert!(!recipes.contains(&"alias".to_string()));
        assert_eq!(modules, &vec!["stack".to_string()]);
    }

    #[test]
    fn inlines_a_recipe_command_and_substitutes_env_variables() {
        let mut variables = BTreeMap::new();
        variables.insert("api_port".to_string(), "WT_PORT_API".to_string());

        // A guard line exists only to reject a mode we are not reproducing.
        let body = "{{ if mode == \"ui\" { error(\"nope\") } else { \"\" } }}\nuv run uvicorn app:main --port {{api_port}} --reload";
        assert_eq!(
            inline_recipe(body, &variables).as_deref(),
            Some("uv run uvicorn app:main --port $WT_PORT_API --reload")
        );

        // Spaced interpolation and a single command with no template work.
        assert_eq!(
            inline_recipe("pnpm dev --port {{ api_port }}", &variables).as_deref(),
            Some("pnpm dev --port $WT_PORT_API")
        );

        // Anything we cannot resolve mechanically must not be guessed at.
        assert_eq!(inline_recipe("echo {{unknown_var}}", &variables), None);
        assert_eq!(inline_recipe("echo one\necho two", &variables), None);
        assert_eq!(inline_recipe("", &variables), None);
    }

    #[test]
    fn records_recipe_bodies_and_variable_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("justfile");
        std::fs::write(
            &path,
            concat!(
                "svc_port := env(\"WT_PORT_SVC\", \"8000\")\n",
                "\n",
                "dev $MY_PORT=svc_port:\n",
                "  uv run uvicorn app:main --port {{svc_port}}\n",
                "\n",
                "lint:\n",
                "  ruff check .\n",
            ),
        )
        .unwrap();
        let facts = parse_justfile(&path);
        assert_eq!(facts.recipes, vec!["dev".to_string(), "lint".to_string()]);
        assert_eq!(
            facts.variables.get("svc_port").map(String::as_str),
            Some("WT_PORT_SVC")
        );
        assert!(
            facts.recipe_bodies["dev"].contains("uvicorn"),
            "{:?}",
            facts.recipe_bodies
        );
        let inlined = inline_recipe(&facts.recipe_bodies["dev"], &facts.variables).unwrap();
        assert!(inlined.contains("--port $WT_PORT_SVC"), "{inlined}");
    }

    #[test]
    fn only_env_reads_count_as_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("justfile");
        std::fs::write(
            &path,
            concat!(
                "set shell := [\"bash\"]\n",
                "svc_port := env(\"WT_PORT_SVC\", \"8000\")\n",
                "mode := env_var(\"MODE\", \"dev\")\n",
                "export PLAIN := \"constant\"\n",
                "\n",
                "serve $MY_SERVICE_PORT=svc_port:\n",
                "  echo $MY_SERVICE_PORT\n",
            ),
        )
        .unwrap();
        let facts = parse_justfile(&path);
        let (env_variables, exported) = (&facts.env_variables, &facts.exported_parameters);

        // Read from the environment: usable to pass a port in.
        assert!(
            env_variables.contains(&"WT_PORT_SVC".to_string()),
            "{env_variables:?}"
        );
        assert!(
            env_variables.contains(&"MODE".to_string()),
            "{env_variables:?}"
        );
        // Outputs: an ambient value is overwritten, so they cannot carry one.
        assert!(
            !env_variables.contains(&"MY_SERVICE_PORT".to_string()),
            "a recipe parameter is not an input: {env_variables:?}"
        );
        assert!(
            !env_variables.contains(&"PLAIN".to_string()),
            "an exported constant is not an input: {env_variables:?}"
        );
        assert_eq!(exported, &vec!["MY_SERVICE_PORT".to_string()]);
    }
}
