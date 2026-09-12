use crate::compose::{self, ComposeGroup, ComposeRunner, GroupService, PortMapping};
use crate::config::Config;
use crate::env::{self, EnvPlan};
use crate::manifest::{self, Bootstrap, Expose, Loaded, Node, Runtime, When};
use crate::paths::Paths;
use crate::ports::{self, Assignment, PortRequest};
use crate::repo::Repo;
use crate::run;
use crate::slug::slugify;
use crate::worktrees;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

static EMPTY_ENV: BTreeMap<String, String> = BTreeMap::new();

/// Sentinel used by dry runs: port 0 is never a valid allocated port, so it
/// renders as a placeholder instead of a number.
const PLACEHOLDER_PORT: u16 = 0;
const PLACEHOLDER: &str = "<allocated>";

fn render_placeholder(port: u16) -> String {
    if port == PLACEHOLDER_PORT {
        PLACEHOLDER.to_string()
    } else {
        port.to_string()
    }
}

pub struct Ctx {
    pub paths: Paths,
    pub config: Config,
    pub repo: Repo,
    pub runtime_dir: PathBuf,
    pub loaded: Loaded,
    pub nodes: Vec<Node>,
    pub edges: Vec<Vec<usize>>,
    pub slug: String,
}

impl Ctx {
    pub fn load(cwd: &Path) -> Result<Self> {
        let paths = Paths::new()?;
        let config = Config::load(&paths)?;
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let repo = Repo::open(&cwd)?;
        let loaded = match Loaded::load(&cwd) {
            Ok(loaded) => loaded,
            Err(error) => match worktrees::missing_manifest_hint(&repo, &cwd) {
                Some(hint) => return Err(anyhow::anyhow!("{error}\n\n{hint}")),
                None => return Err(error),
            },
        };
        loaded.validate()?;
        let nodes = manifest::nodes(&loaded);
        let edges = manifest::dependencies(&nodes)?;
        let slug = slugify(&repo.worktree_id());
        let runtime_dir = repo.worktree_root.join(".magictree");
        Ok(Self {
            paths,
            config,
            repo,
            runtime_dir,
            loaded,
            nodes,
            edges,
            slug,
        })
    }

    pub fn ensure_runtime_dirs(&self) -> Result<()> {
        for dir in ["", "run", "log"] {
            let path = if dir.is_empty() {
                self.runtime_dir.clone()
            } else {
                self.runtime_dir.join(dir)
            };
            std::fs::create_dir_all(&path)?;
        }
        Ok(())
    }

    /// Resolve a selector set into a dependency-closed, topologically ordered
    /// list of node indices.
    pub fn scope(&self, services: &[String], apps: &[String], all: bool) -> Result<Vec<usize>> {
        let explicit = !services.is_empty() || !apps.is_empty();
        let mut chosen: BTreeSet<usize> = BTreeSet::new();
        let mut touched_apps: BTreeSet<String> = BTreeSet::new();
        let mut touched_workspace = false;

        let default_app = self
            .loaded
            .current_app
            .clone()
            .filter(|_| !self.loaded.root_is_app);

        if all || !explicit {
            match default_app {
                Some(app_id) => {
                    for (index, node) in self.nodes.iter().enumerate() {
                        if node.app.as_deref() == Some(app_id.as_str()) && node.is_running_service()
                        {
                            chosen.insert(index);
                            touched_apps.insert(app_id.clone());
                        }
                    }
                }
                None => {
                    for (index, node) in self.nodes.iter().enumerate() {
                        if !node.is_running_service() {
                            continue;
                        }
                        chosen.insert(index);
                        match &node.app {
                            Some(app) => {
                                touched_apps.insert(app.clone());
                            }
                            None => touched_workspace = true,
                        }
                    }
                }
            }
        } else {
            for (index, node) in self.nodes.iter().enumerate() {
                let by_service = services
                    .iter()
                    .any(|want| want == &node.id || want == &node.qual());
                let by_app = node
                    .app
                    .as_ref()
                    .map(|app| apps.iter().any(|want| want == app))
                    .unwrap_or(false);
                if by_service || by_app {
                    chosen.insert(index);
                    match &node.app {
                        Some(app) => {
                            touched_apps.insert(app.clone());
                        }
                        None => touched_workspace = true,
                    }
                }
            }
            if chosen.is_empty() {
                bail!("no services matched the given selectors");
            }
            // Selecting anything that belongs to an app brings the shared
            // infrastructure with it, so a running app is never left without
            // the database or cache it was written against.
            if !touched_apps.is_empty() {
                for (index, node) in self.nodes.iter().enumerate() {
                    if node.app.is_none() && node.is_running_service() {
                        chosen.insert(index);
                        touched_workspace = true;
                    }
                }
            }
        }

        for (index, node) in self.nodes.iter().enumerate() {
            let Some(job) = node.job() else { continue };
            if job.when != When::Up {
                continue;
            }
            let include = match &node.app {
                Some(app) => touched_apps.contains(app),
                None => touched_workspace || !explicit,
            };
            if include {
                chosen.insert(index);
            }
        }

        let selection: Vec<usize> = chosen.into_iter().collect();
        manifest::order(&self.nodes, &self.edges, &selection)
    }

    pub fn all_service_indices(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.is_running_service())
            .map(|(index, _)| index)
            .collect()
    }

    /// Every port assigned to this service, labelled for display. A single-port
    /// service uses its plain name; multi-port services append the port name.
    pub fn assigned_ports(&self, node: &Node, assignment: &Assignment) -> Vec<(String, u16)> {
        let Some(service) = node.service() else {
            return Vec::new();
        };
        let declared = service.ports();
        let multiple = declared.len() > 1;
        let mut out = Vec::new();
        for port in declared {
            let name = if multiple {
                port.name.as_deref().filter(|name| *name != node.id)
            } else {
                None
            };
            if let Some(value) = assignment.ports.get(&self.port_key(node, name)) {
                let label = match name {
                    Some(name) => format!("{}:{}", node.qual(), name),
                    None => node.qual(),
                };
                out.push((label, *value));
            }
        }
        if out.is_empty() {
            if let Some(value) = assignment.ports.get(&node.qual()) {
                out.push((node.qual(), *value));
            }
        }
        out
    }

    /// Registry key for one of a service's ports. The service id alone is used
    /// when the service has a single port, so existing manifests keep their key.
    pub fn port_key(&self, node: &Node, port_name: Option<&str>) -> String {
        match port_name {
            None => node.qual(),
            Some(name) if name == node.id => node.qual(),
            Some(name) => format!("{}:{}", node.qual(), name),
        }
    }

    pub fn port_var_name(&self, node: &Node, port_name: Option<&str>) -> String {
        let base = match &node.app {
            Some(app) => format!("{app}_{}", node.id),
            None => node.id.clone(),
        };
        let stem = match port_name {
            Some(name) => format!("{base}_{name}"),
            None => base,
        };
        format!("MAGICTREE_PORT_{}", stem.replace([':', '-'], "_"))
    }

    /// Every port this selection must be assigned.
    pub fn port_requests(&self, selection: &[usize]) -> Vec<PortRequest> {
        let mut requests = Vec::new();
        for &index in selection {
            let node = &self.nodes[index];
            let Some(service) = node.service() else {
                continue;
            };
            if service.expose != Expose::Port {
                continue;
            }
            let declared = service.ports();
            let multiple = declared.len() > 1;
            for port in declared {
                let name = if multiple { port.name.as_deref() } else { None };
                requests.push(PortRequest {
                    name: self.port_key(node, name.filter(|name| *name != node.id)),
                    prefer: port.prefer,
                    require: port.require,
                });
            }
        }
        requests
    }

    pub fn computed_env(&self, assignment: &Assignment) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::new();
        vars.insert("MAGICTREE_SLUG".to_string(), self.slug.clone());
        vars.insert(
            "MAGICTREE_WORKTREE".to_string(),
            self.repo.worktree_root.to_string_lossy().to_string(),
        );
        vars.insert(
            "MAGICTREE_WORKSPACE".to_string(),
            self.loaded.workspace_dir.to_string_lossy().to_string(),
        );
        vars.insert("COMPOSE_PROJECT_NAME".to_string(), self.slug.clone());
        for node in &self.nodes {
            let Some(service) = node.service() else {
                continue;
            };
            let declared = service.ports();
            let multiple = declared.len() > 1;
            for port in declared {
                let name = if multiple {
                    port.name.as_deref().filter(|name| *name != node.id)
                } else {
                    None
                };
                if let Some(value) = assignment.ports.get(&self.port_key(node, name)) {
                    vars.insert(self.port_var_name(node, name), value.to_string());
                }
            }
        }
        vars
    }

    pub fn build_env(&self, app: Option<&str>, assignment: &Assignment) -> Result<EnvPlan> {
        let workspace_env = if self.loaded.root_is_app {
            &EMPTY_ENV
        } else {
            &self.loaded.workspace.env
        };
        let app_env = app
            .and_then(|id| self.loaded.app(id))
            .map(|app| &app.manifest.env)
            .unwrap_or(&EMPTY_ENV);
        env::build(self.computed_env(assignment), workspace_env, app_env)
    }

    pub fn node_env(
        &self,
        node: &Node,
        assignment: &Assignment,
    ) -> Result<BTreeMap<String, String>> {
        let plan = self.build_env(node.app.as_deref(), assignment)?;
        let mut vars = plan.vars;
        let compose = node.runtime() == Some(Runtime::Compose);

        for other in &self.nodes {
            // A compose file interpolates variables wherever they are declared.
            // Starting one service therefore needs the whole stack's port
            // variables, not only that service's own.
            if !compose && other.qual() != node.qual() {
                continue;
            }
            for (variable, port) in self.declared_port_variables(other, assignment) {
                vars.insert(variable, port.to_string());
            }
        }
        Ok(vars)
    }

    /// Every `port.env` a service declares, paired with its allocated port.
    pub fn declared_port_variables(
        &self,
        node: &Node,
        assignment: &Assignment,
    ) -> Vec<(String, u16)> {
        let Some(service) = node.service() else {
            return Vec::new();
        };
        if service.expose != Expose::Port {
            return Vec::new();
        }
        let declared = service.ports();
        let multiple = declared.len() > 1;
        let mut out = Vec::new();
        for port in declared {
            let Some(variable) = port.env.clone() else {
                continue;
            };
            let name = if multiple {
                port.name.as_deref().filter(|name| *name != node.id)
            } else {
                None
            };
            if let Some(value) = assignment.ports.get(&self.port_key(node, name)) {
                out.push((variable, *value));
            }
        }
        out
    }

    pub fn project_for(&self, node: &Node) -> String {
        match &node.app {
            Some(app) => format!("{}-{app}", self.slug),
            None => self.slug.clone(),
        }
    }

    pub fn runner_key(&self, node: &Node) -> Option<(PathBuf, String)> {
        let service = node.service()?;
        if node.runtime() != Some(Runtime::Compose) {
            return None;
        }
        let reference = service.compose.as_ref()?;
        Some((
            node.manifest_dir.join(&reference.file),
            self.project_for(node),
        ))
    }

    /// Compose groups plus their runners, with generated overrides on disk.
    pub fn compose_runners(
        &self,
        selection: &[usize],
        assignment: &Assignment,
    ) -> Result<HashMap<(PathBuf, String), ComposeRunner>> {
        let groups = self.compose_groups(selection, assignment)?;
        let mut runners = HashMap::new();
        for group in &groups {
            let override_file = compose::write_override(&self.runtime_dir, group)?;
            let key = (group.file.clone(), group.project.clone());
            runners.insert(
                key,
                ComposeRunner::new(
                    group.file.clone(),
                    Some(override_file),
                    group.project.clone(),
                ),
            );
        }
        Ok(runners)
    }

    fn compose_groups(
        &self,
        selection: &[usize],
        assignment: &Assignment,
    ) -> Result<Vec<ComposeGroup>> {
        // Compose starts a service's dependencies too, so the override has to
        // describe every service in a file we touch. Covering only the selected
        // ones would let a dependency start with its own published ports.
        let mut touched: BTreeSet<(PathBuf, String)> = BTreeSet::new();
        for &index in selection {
            if let Some(key) = self.runner_key(&self.nodes[index]) {
                touched.insert(key);
            }
        }

        let mut groups: BTreeMap<(PathBuf, String), Vec<GroupService>> = BTreeMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            let Some(key) = self.runner_key(node) else {
                continue;
            };
            if !touched.contains(&key) {
                continue;
            }
            let _ = index;
            let Some(service) = node.service() else {
                continue;
            };
            let Some(reference) = service.compose.as_ref() else {
                continue;
            };
            let mut mappings = Vec::new();
            if service.expose == Expose::Port {
                let declared = service.ports();
                let multiple = declared.len() > 1;
                for port in declared {
                    let name = if multiple {
                        port.name.as_deref().filter(|name| *name != node.id)
                    } else {
                        None
                    };
                    mappings.push(PortMapping {
                        host: assignment.ports.get(&self.port_key(node, name)).copied(),
                        target: port.target,
                    });
                }
            }
            groups.entry(key).or_default().push(GroupService {
                name: reference.service.clone(),
                expose: service.expose,
                mappings,
            });
        }
        let worktree_path = self.repo.worktree_root.to_string_lossy().to_string();
        let repo_key = self.repo.key();
        Ok(groups
            .into_iter()
            .map(|((file, project), services)| ComposeGroup {
                file,
                project,
                worktree_path: worktree_path.clone(),
                repo_key: repo_key.clone(),
                services,
            })
            .collect())
    }

    /// Distinct compose projects in this repository, used for `down` and `status`.
    pub fn compose_identities(&self) -> Vec<(PathBuf, String)> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for node in &self.nodes {
            if let Some(key) = self.runner_key(node) {
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
        }
        out
    }

    pub fn bootstrap_targets(
        &self,
        selection: &[usize],
    ) -> Vec<(PathBuf, Option<String>, Bootstrap)> {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut targets = Vec::new();
        if !self.loaded.root_is_app {
            let bootstrap = self.loaded.workspace.bootstrap.clone();
            if !bootstrap.sync.is_empty() || !bootstrap.run.is_empty() {
                seen.insert(self.loaded.workspace_dir.clone());
                targets.push((self.loaded.workspace_dir.clone(), None, bootstrap));
            }
        }
        let mut touched: BTreeSet<String> = BTreeSet::new();
        for &index in selection {
            if let Some(app) = &self.nodes[index].app {
                touched.insert(app.clone());
            }
        }
        // A standalone repository has one app that is always in scope.
        for app in &self.loaded.apps {
            if !self.loaded.root_is_app && !touched.contains(&app.id) {
                continue;
            }
            let bootstrap = app.manifest.bootstrap.clone();
            if bootstrap.sync.is_empty() && bootstrap.run.is_empty() {
                continue;
            }
            if seen.insert(app.dir.clone()) {
                targets.push((app.dir.clone(), Some(app.id.clone()), bootstrap));
            }
        }
        targets
    }

    /// A failure report that says which service failed, on which port, in what
    /// state, and where its output went. The port number alone is not useful.
    pub fn describe_failure(
        &self,
        node: &Node,
        assignment: &Assignment,
        runner: Option<&ComposeRunner>,
        reason: &str,
    ) -> String {
        let mut report = String::new();
        let _ = write!(report, "{}: {reason}", node.qual());

        let assigned = self.assigned_ports(node, assignment);
        if let Some((label, port)) = assigned.first() {
            let variable = node
                .service()
                .and_then(|service| service.ports().iter().find_map(|p| p.env.clone()));
            let _ = match (variable, label == &node.qual()) {
                (Some(variable), true) => {
                    write!(report, "\n  port     {port} (injected as {variable})")
                }
                (Some(variable), false) => {
                    write!(
                        report,
                        "\n  port     {label} -> {port} (injected as {variable})"
                    )
                }
                (None, true) => write!(report, "\n  port     {port}"),
                (None, false) => write!(report, "\n  port     {label} -> {port}"),
            };
        }

        if let Some(health) = node.service().and_then(|service| service.health.as_ref()) {
            if let (Some(path), Some((_, port))) = (&health.http, assigned.first()) {
                let _ = write!(report, "\n  probe    http://127.0.0.1:{port}{path}");
            } else if health.tcp == Some(true) {
                if let Some((_, port)) = assigned.first() {
                    let _ = write!(report, "\n  probe    tcp 127.0.0.1:{port}");
                }
            } else if let Some(command) = &health.command {
                let _ = write!(report, "\n  probe    `{command}`");
            }
        }

        match runner {
            Some(runner) => {
                let container = node
                    .service()
                    .and_then(|service| service.compose.as_ref())
                    .map(|reference| reference.service.clone())
                    .unwrap_or_default();
                let _ = write!(
                    report,
                    "\n  container {container} (project {})",
                    runner.project
                );
                let _ = write!(
                    report,
                    "\n  logs     docker compose -f {} -p {} logs {container}",
                    runner.file.display(),
                    runner.project
                );
            }
            None => {
                if let Some(pid) = run::read_pid(&self.runtime_dir, &node.qual()) {
                    let state = if run::is_alive(pid) {
                        format!("running (pid {pid}) but not answering")
                    } else {
                        "exited".to_string()
                    };
                    let _ = write!(report, "\n  process  {state}");
                }
                let log = run::log_file(&self.runtime_dir, &node.qual());
                let _ = write!(report, "\n  log      {}", log.display());
                let tail = run::log_tail(&self.runtime_dir, &node.qual(), 15);
                if !tail.is_empty() {
                    report.push_str("\n\n  last output:");
                    for line in tail {
                        let _ = write!(report, "\n  | {line}");
                    }
                }
            }
        }

        report.push_str("\n\n  other services were left running:");
        let _ = write!(
            report,
            "\n    magictree status --probe   (what is up, and which probes fail)"
        );
        if runner.is_none() {
            let _ = write!(
                report,
                "\n    magictree logs {}   (full output)",
                node.qual()
            );
        }
        report.push_str("\n    magictree down             (stop this worktree's stack)");
        report
    }

    pub fn find_node(&self, name: &str) -> Option<&Node> {
        self.nodes
            .iter()
            .find(|node| node.qual() == name || node.id == name)
    }

    /// Environment for a dry run: same shape as a real run, with every
    /// allocated port shown as a placeholder so nothing is claimed.
    pub fn preview_env(&self) -> Result<EnvPlan> {
        let assignment = self.preview_assignment();
        let mut plan = self.build_env(self.loaded.current_app.as_deref(), &assignment)?;
        for (key, value) in plan.vars.iter_mut() {
            if key.starts_with("MAGICTREE_PORT_") && *value == PLACEHOLDER_PORT.to_string() {
                *value = PLACEHOLDER.to_string();
            }
        }
        for node in &self.nodes {
            if let Some(variable) = node
                .service()
                .and_then(|service| service.port.as_ref())
                .and_then(|port| port.env.clone())
            {
                let value = assignment
                    .ports
                    .get(&node.qual())
                    .map(|port| render_placeholder(*port))
                    .unwrap_or_else(|| PLACEHOLDER.to_string());
                plan.vars.entry(variable).or_insert(value);
            }
        }
        Ok(plan)
    }

    /// Replace the dry-run sentinel with a readable placeholder.
    pub fn with_placeholders(&self, mut plan: EnvPlan) -> EnvPlan {
        for (key, value) in plan.vars.iter_mut() {
            if key.starts_with("MAGICTREE_PORT_") && *value == PLACEHOLDER_PORT.to_string() {
                *value = PLACEHOLDER.to_string();
            }
        }
        plan
    }

    /// Placeholder port assignment used by dry runs; never written anywhere.
    pub fn preview_assignment(&self) -> Assignment {
        let mut assignment = Assignment {
            version: ports::ASSIGNMENT_VERSION,
            repo_key: self.repo.key(),
            worktree_id: self.repo.worktree_id(),
            worktree_path: Some(self.repo.worktree_root.to_string_lossy().to_string()),
            base: 0,
            ports: BTreeMap::new(),
        };
        for node in &self.nodes {
            let Some(service) = node.service() else {
                continue;
            };
            if service.expose != Expose::Port {
                continue;
            }
            let declared = service.ports();
            let multiple = declared.len() > 1;
            for port in declared {
                let name = if multiple {
                    port.name.as_deref().filter(|name| *name != node.id)
                } else {
                    None
                };
                let fixed = port.require.or(port.prefer).unwrap_or(PLACEHOLDER_PORT);
                assignment.ports.insert(self.port_key(node, name), fixed);
            }
        }
        assignment
    }
}
