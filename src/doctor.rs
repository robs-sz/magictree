//! `doctor`: compare what the repository now says against what the manifests
//! claim. Reuses discovery, so drift detection cannot silently diverge from it.

use crate::discover::extract;
use crate::discover::ports;
use crate::discover::report::{Fact, FactData, FactKind, PortLiteralFact, Report};
use crate::manifest::{Expose, Loaded, NodeKind, Runtime, Service, Target};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// How much a finding matters. Only `Drift` findings fail the command; `Info`
/// findings are worth knowing but describe a valid choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Drift,
    Info,
}

#[derive(Debug)]
pub struct Drift {
    pub severity: Severity,
    pub summary: String,
    pub suggestion: Option<String>,
}

impl Drift {
    fn new(summary: impl Into<String>, suggestion: Option<String>) -> Self {
        Self {
            severity: Severity::Drift,
            summary: summary.into(),
            suggestion,
        }
    }

    fn info(summary: impl Into<String>, suggestion: Option<String>) -> Self {
        Self {
            severity: Severity::Info,
            summary: summary.into(),
            suggestion,
        }
    }

    pub fn is_drift(&self) -> bool {
        self.severity == Severity::Drift
    }
}

/// Compare the manifest set at `root` against a fresh discovery report.
pub fn check(root: &Path) -> Result<Vec<Drift>> {
    let report = extract(root)?;
    let loaded = Loaded::load(root)?;
    Ok(compare(&report, &loaded))
}

pub fn compare(report: &Report, loaded: &Loaded) -> Vec<Drift> {
    let mut drift = Vec::new();

    // Apps: discovery versus [workspace].apps.
    if !loaded.root_is_app {
        let declared: BTreeSet<String> = loaded
            .apps
            .iter()
            .map(|app| relative(&loaded.workspace_dir, &app.dir))
            .collect();
        let discovered: BTreeSet<String> = report.apps.iter().map(|app| app.dir.clone()).collect();
        for dir in discovered.difference(&declared) {
            drift.push(Drift::new(
                format!("app '{dir}' exists but is not listed in [workspace].apps"),
                Some(format!(
                    "add \"{dir}\" to [workspace].apps in magictree.toml, or run `magictree init` to regenerate"
                )),
            ));
        }
        for dir in declared.difference(&discovered) {
            drift.push(Drift::new(
                format!("[workspace].apps lists '{dir}', which discovery no longer finds"),
                Some("remove it from [workspace].apps".to_string()),
            ));
        }
    } else if report.apps.len() > 1 {
        drift.push(Drift::new(
            "the repository now has several apps but magictree.toml has no [workspace] section",
            Some("run `magictree init` to add a workspace manifest".to_string()),
        ));
    }

    // Compose services: referenced, discovered, and available.
    let discovered_compose: BTreeSet<String> = report
        .facts
        .iter()
        .filter(|fact| fact.kind == FactKind::Compose)
        .filter_map(|fact| match &fact.data {
            FactData::Compose { services, .. } => Some(services.iter().map(|s| s.name.clone())),
            _ => None,
        })
        .flatten()
        .collect();
    let referenced: BTreeSet<String> = loaded
        .apps
        .iter()
        .chain(std::iter::once(&crate::manifest::App {
            id: String::new(),
            dir: loaded.workspace_dir.clone(),
            manifest: loaded.workspace.clone(),
        }))
        .flat_map(|app| app.manifest.services.iter())
        .filter(|service| service.compose.is_some())
        .map(|service| {
            service
                .compose
                .as_ref()
                .map(|reference| reference.service.clone())
                .unwrap_or_default()
        })
        .collect();
    for name in referenced.difference(&discovered_compose) {
        drift.push(Drift::new(
            format!("manifest references compose service '{name}', which no longer exists"),
            Some(
                "remove that service from magictree.toml, or restore it in the compose file"
                    .to_string(),
            ),
        ));
    }
    for name in discovered_compose.difference(&referenced) {
        drift.push(Drift::info(
            format!("compose service '{name}' is not managed by any manifest"),
            Some("leave it if it is optional tooling, or add it to a manifest".to_string()),
        ));
    }

    // Targets: the script, recipe, or task must still exist.
    for app in &loaded.apps {
        let rel = relative(&loaded.workspace_dir, &app.dir);
        let facts = app_facts(report, &app.id, &rel);
        for service in &app.manifest.services {
            let Some(target) = &service.target else {
                continue;
            };
            if let Some(missing) = missing_target(target, &facts) {
                drift.push(Drift::new(
                    format!(
                        "service '{}' runs {missing}, which discovery no longer finds",
                        service.id
                    ),
                    Some(format!(
                        "update the service in {}/magictree.toml",
                        if rel == "." { ".".into() } else { rel.clone() }
                    )),
                ));
            }
        }
    }

    // A port variable the app cannot read means the service listens on its own
    // default while the health probe watches the allocated port.
    for app in &loaded.apps {
        let rel = relative(&loaded.workspace_dir, &app.dir);
        let facts = app_facts(report, &app.id, &rel);
        for service in &app.manifest.services {
            let Some(variable) = service
                .port
                .as_ref()
                .and_then(|port| port.env.clone())
                .or_else(|| service.ports.iter().find_map(|port| port.env.clone()))
            else {
                continue;
            };
            if service.expose == Expose::None {
                continue;
            }
            let reads: BTreeSet<&String> = facts
                .iter()
                .filter_map(|fact| match &fact.data {
                    FactData::Just { env_variables, .. } => Some(env_variables),
                    _ => None,
                })
                .flatten()
                .collect();
            let sets: BTreeSet<&String> = facts
                .iter()
                .filter_map(|fact| match &fact.data {
                    FactData::Just {
                        exported_parameters,
                        ..
                    } => Some(exported_parameters),
                    _ => None,
                })
                .flatten()
                .collect();
            if sets.contains(&variable) && !reads.contains(&variable) {
                drift.push(Drift::new(
                    format!(
                        "service '{}' takes its port from '{variable}', but the justfile only sets that variable for a recipe; it never reads it from the environment",
                        service.id
                    ),
                    Some(format!(
                        "point port.env at a variable the justfile reads, e.g. one assigned from env(\"NAME\") in {}",
                        if rel == "." { "the justfile".to_string() } else { format!("{rel}/justfile") }
                    )),
                ));
            }
        }
    }

    // A port pinned in the command a service runs overrides the variable
    // magictree injects, so that service can never follow a per-worktree port:
    // the second worktree to start would try to bind the same number.
    let mut reported: BTreeSet<(String, String)> = BTreeSet::new();
    for app in &loaded.apps {
        let rel = relative(&loaded.workspace_dir, &app.dir);
        let facts = app_facts(report, &app.id, &rel);
        for service in &app.manifest.services {
            let advice = port_advice(loaded, &app.id, service);
            for pinned in pinned_ports(service, &facts) {
                if let Some(step) = &pinned.step {
                    reported.insert(step.clone());
                    // The service's own command line hands the allocated port to
                    // the step, so the literal the step pins never binds: the
                    // value appended after it is the one that takes effect.
                    if command_hands_over(service, &advice.variable) {
                        continue;
                    }
                }
                let rewrite = ports::rewrite(&pinned.command, &advice.variable)
                    .unwrap_or_else(|| pinned.command.clone());
                let mut list: Vec<u16> = pinned.literals.iter().map(|item| item.port).collect();
                list.sort_unstable();
                list.dedup();
                let shown: Vec<String> = list.iter().map(u16::to_string).collect();
                let mut suggestion = format!("change it to `{rewrite}`");
                if advice.declared {
                    suggestion.push_str(&format!(
                        "; the manifest already names {} as this service's port variable",
                        advice.variable
                    ));
                } else {
                    suggestion.push_str(&format!(
                        " and set port = {{ env = \"{}\" }} on service '{}'",
                        advice.variable, service.id
                    ));
                }
                if let Some(clash) = &advice.clash {
                    suggestion.push_str(&format!(" ({clash})"));
                }
                drift.push(Drift::new(
                    format!(
                        "service '{}' runs {} (`{}`), which pins port {}; every worktree would try to bind the same port",
                        service.id,
                        pinned.origin,
                        pinned.command,
                        shown.join(" and ")
                    ),
                    Some(suggestion),
                ));
            }
        }
    }

    // Ports pinned anywhere else — a seed script, a test helper, a committed
    // env template. Worth naming, but only the service's own command is
    // guaranteed to be started by magictree.
    let all_facts: Vec<&Fact> = report.facts.iter().collect();
    for (fact, literal) in ports::all(&all_facts) {
        let container = literal.container.clone().unwrap_or_default();
        if !reported.insert((fact.id.clone(), container.clone())) {
            continue;
        }
        // Only a port some service actually allocates can be pointed at; for
        // anything else the honest answer is that the manifest owes it a port.
        let variable = variable_for_port(loaded, literal.port);
        let suggestion = ports::suggestion(literal, variable.as_deref());
        drift.push(Drift::info(
            format!(
                "{} pins port {} in {} (`{}`)",
                fact.source,
                literal.port,
                ports::step_label(fact.kind, &container),
                literal.text
            ),
            Some(suggestion),
        ));
    }

    // A manifest that git does not track is invisible to every new worktree, so
    // `magictree new` there would have nothing to start.
    let mut manifests: Vec<std::path::PathBuf> = Vec::new();
    for dir in std::iter::once(&loaded.workspace_dir).chain(loaded.apps.iter().map(|app| &app.dir))
    {
        let manifest = dir.join("magictree.toml");
        if !manifests.contains(&manifest) {
            manifests.push(manifest);
        }
    }
    for manifest in manifests {
        if !manifest.is_file() {
            continue;
        }
        if !crate::repo::is_tracked(&loaded.workspace_dir, &manifest) {
            let name = manifest
                .strip_prefix(&loaded.workspace_dir)
                .unwrap_or(&manifest)
                .display()
                .to_string();
            drift.push(Drift::new(
                format!("{name} is not committed, so new worktrees will not inherit it"),
                Some(format!("git add {name} && git commit -m \"add {name}\"")),
            ));
        }
    }

    // Bootstrap inputs must still exist, or caching will never hit.
    for app in loaded
        .apps
        .iter()
        .map(|app| (app.dir.clone(), &app.manifest))
        .chain(std::iter::once((
            loaded.workspace_dir.clone(),
            &loaded.workspace,
        )))
    {
        for step in &app.1.bootstrap.run {
            for input in step.inputs() {
                if !app.0.join(input).exists() {
                    drift.push(Drift::new(
                        format!(
                            "bootstrap input '{input}' for `{}` does not exist",
                            step.command()
                        ),
                        Some(
                            "fix the path, or drop the step's inputs so it always runs".to_string(),
                        ),
                    ));
                }
            }
        }
    }

    // Exposed compose services need a container-side port.
    for app in loaded
        .apps
        .iter()
        .map(|app| &app.manifest)
        .chain(std::iter::once(&loaded.workspace))
    {
        for service in &app.services {
            if service.expose == Expose::Port
                && service.compose.is_some()
                && service.ports().iter().all(|port| port.target.is_none())
            {
                drift.push(Drift::new(
                    format!(
                        "compose service '{}' exposes a port but has no port.target",
                        service.id
                    ),
                    Some("add port = { target = <container port> }".to_string()),
                ));
            }
        }
    }

    let _ = Runtime::Compose;
    let _ = NodeKind::Service;
    drift
}

/// Facts belonging to one app, plus the repository-level facts it inherits.
fn app_facts<'a>(report: &'a Report, app: &str, relative_dir: &str) -> Vec<&'a Fact> {
    report
        .facts
        .iter()
        .filter(|fact| fact.app.as_deref() == Some(app) || relative_dir == ".")
        .collect()
}

/// A port the service's own command pins, with where it came from.
struct PinnedPort {
    /// How the source reads in a finding: `script 'dev' in package.json`.
    origin: String,
    /// The text that has to change.
    command: String,
    literals: Vec<PortLiteralFact>,
    /// Recorded step this covers, so the informational pass does not repeat it.
    step: Option<(String, String)>,
}

/// Every place the command magictree starts for this service pins a port.
fn pinned_ports(service: &Service, facts: &[&Fact]) -> Vec<PinnedPort> {
    let mut out: Vec<PinnedPort> = Vec::new();
    let target = service.target.as_ref();
    let step = match target {
        Some(Target::Npm { script, .. }) | Some(Target::Pnpm { script, .. }) => {
            Some((FactKind::Node, "script", script))
        }
        Some(Target::Just { recipe, .. }) => Some((FactKind::Just, "recipe", recipe)),
        Some(Target::Mise { task, .. }) => Some((FactKind::Mise, "task", task)),
        Some(Target::Uv {
            script: Some(script),
            ..
        }) => Some((FactKind::Python, "script", script)),
        _ => None,
    };
    if let Some((kind, word, name)) = step {
        let mut by_file: BTreeMap<String, PinnedPort> = BTreeMap::new();
        for (fact, literal) in ports::sites(facts, kind, name) {
            let entry = by_file
                .entry(fact.id.clone())
                .or_insert_with(|| PinnedPort {
                    origin: format!("{word} '{name}' in {}", fact.source),
                    command: literal.text.clone(),
                    literals: Vec::new(),
                    step: Some((fact.id.clone(), name.clone())),
                });
            entry.literals.push(literal.clone());
        }
        out.extend(by_file.into_values());
    }
    // The target's own command line, which is where a `--port` in `args` lives.
    let own = match target {
        Some(target) => Some(target.command()),
        None => service.command.clone(),
    };
    if let Some(command) = own {
        let literals: Vec<PortLiteralFact> = ports::scan(&command)
            .into_iter()
            .map(|literal| PortLiteralFact {
                port: literal.port,
                kind: literal.kind,
                literal: literal.literal,
                container: None,
                text: command.clone(),
            })
            .collect();
        if !literals.is_empty() {
            out.push(PinnedPort {
                origin: "the command in magictree.toml".to_string(),
                command,
                literals,
                step: None,
            });
        }
    }
    out
}

/// True when the service's own command line passes `variable` on to the step it
/// runs, as `args = ["-p", "${APP_PORT}"]` does for one that pins `-p`.
///
/// The appended arguments land after the step's own, so the last flag wins and
/// the literal it pins never binds. `npm run` stops parsing at `--`, so an npm
/// command only counts with the separator; pnpm, yarn and bun forward
/// everything after the script name themselves.
fn command_hands_over(service: &Service, variable: &str) -> bool {
    let command = match (&service.target, &service.command) {
        (Some(target), _) => target.command(),
        (None, Some(command)) => command.clone(),
        _ => return false,
    };
    let Some(at) = command.find(&format!("${{{variable}")) else {
        return false;
    };
    !command.starts_with("npm ") || command[..at].contains(" -- ")
}

/// Where a service's port should come from, and whether the manifest says so.
struct PortAdvice {
    variable: String,
    declared: bool,
    /// Set when the declared variable had to be replaced, and why.
    clash: Option<String>,
}

fn port_advice(loaded: &Loaded, app_id: &str, service: &Service) -> PortAdvice {
    let fallback = format!("{}_PORT", app_id.to_ascii_uppercase().replace('-', "_"));
    let Some(declared) = service.ports().iter().find_map(|port| port.env.clone()) else {
        return PortAdvice {
            variable: fallback,
            declared: false,
            clash: None,
        };
    };
    // magictree injects each service's own port under the name it declares, so
    // a variable two services claim cannot be right for both of them.
    if let Some(owner) = other_service_using(loaded, &service.id, &declared) {
        return PortAdvice {
            variable: fallback,
            declared: false,
            clash: Some(format!(
                "'{declared}' is also service '{owner}'s port variable"
            )),
        };
    }
    PortAdvice {
        variable: declared,
        declared: true,
        clash: None,
    }
}

fn other_service_using(loaded: &Loaded, service_id: &str, variable: &str) -> Option<String> {
    let manifests = loaded
        .apps
        .iter()
        .map(|app| &app.manifest)
        .chain(std::iter::once(&loaded.workspace));
    for manifest in manifests {
        for service in &manifest.services {
            if service.id == service_id {
                continue;
            }
            if service
                .ports()
                .iter()
                .any(|port| port.env.as_deref() == Some(variable))
            {
                return Some(service.id.clone());
            }
        }
    }
    None
}

/// The variable that carries an already-declared port, so an unrelated literal
/// can point at the same number instead of repeating it.
fn variable_for_port(loaded: &Loaded, port: u16) -> Option<String> {
    let manifests = loaded
        .apps
        .iter()
        .map(|app| &app.manifest)
        .chain(std::iter::once(&loaded.workspace));
    for manifest in manifests {
        for service in &manifest.services {
            for spec in service.ports() {
                if (spec.prefer == Some(port) || spec.require == Some(port)) && spec.env.is_some() {
                    return spec.env.clone();
                }
            }
        }
    }
    None
}

fn missing_target(target: &Target, facts: &[&Fact]) -> Option<String> {
    let has_script = |name: &str| {
        facts.iter().any(|fact| match &fact.data {
            FactData::Node { scripts, .. } => scripts.iter().any(|script| script.name == name),
            FactData::Python { scripts, .. } => scripts.iter().any(|script| script == name),
            _ => false,
        })
    };
    let has_recipe = |name: &str| {
        facts.iter().any(|fact| match &fact.data {
            FactData::Just {
                recipes, modules, ..
            } => {
                recipes.iter().any(|recipe| recipe == name)
                    || modules.iter().any(|module| {
                        name.split_once("::")
                            .map(|(head, _)| head == module)
                            .unwrap_or(false)
                    })
            }
            _ => false,
        })
    };
    let has_task = |name: &str| {
        facts.iter().any(|fact| match &fact.data {
            FactData::Mise { tasks, .. } => tasks.iter().any(|task| task == name),
            _ => false,
        })
    };

    match target {
        Target::Npm { script, .. } | Target::Pnpm { script, .. } => {
            (!has_script(script)).then(|| format!("script '{script}'"))
        }
        Target::Uv { script, .. } => script
            .as_ref()
            .filter(|script| !has_script(script))
            .map(|script| format!("script '{script}'")),
        Target::Just { recipe, .. } => (!has_recipe(recipe)).then(|| format!("recipe '{recipe}'")),
        Target::Mise { task, .. } => (!has_task(task)).then(|| format!("task '{task}'")),
        Target::Command { .. } | Target::Python { .. } => None,
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|value| {
            let text = value.display().to_string();
            if text.is_empty() {
                ".".to_string()
            } else {
                text
            }
        })
        .unwrap_or_else(|_| path.display().to_string())
}
