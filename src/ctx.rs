use crate::compose::{self, ComposeGroup, ComposeRunner, GroupService, PortMapping};
use crate::config::Config;
use crate::env::{self, EnvPlan};
use crate::manifest::{self, Bootstrap, Expose, Loaded, Node, Runtime, When};
use crate::paths::{self, Paths};
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

/// Where runtime state lived before it moved into magictree's own state dir.
/// Kept only so a stack started before the upgrade stays stoppable.
pub fn legacy_runtime_dir(repo: &Repo) -> PathBuf {
    repo.worktree_root.join(".magictree")
}

/// Runtime state for one worktree, wherever it currently lives: the state dir
/// once adopted, otherwise a pre-migration `<checkout>/.magictree`, otherwise
/// the state dir.
///
/// Reads resolve through here so `down`, `status` and `logs` still find a stack
/// that was started by an older build; `up` then adopts the directory.
pub fn runtime_dir_for(paths: &Paths, repo: &Repo) -> PathBuf {
    let current = paths.worktree_dir(&repo.key(), &repo.worktree_id());
    if current.exists() {
        return current;
    }
    let legacy = legacy_runtime_dir(repo);
    if legacy.exists() {
        return legacy;
    }
    current
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
        let runtime_dir = runtime_dir_for(&paths, &repo);
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

    /// Move pre-migration runtime state into the state dir, so runtime state has
    /// exactly one home from here on. Idempotent.
    ///
    /// Called by the commands that write runtime state, never by a dry run and
    /// never by a read: moving a directory is a side effect a `status` should
    /// not have.
    pub fn adopt_legacy_runtime(&mut self) -> Result<()> {
        let current = self
            .paths
            .worktree_dir(&self.repo.key(), &self.repo.worktree_id());
        if self.runtime_dir == current {
            return Ok(());
        }
        paths::move_dir(&self.runtime_dir, &current)?;
        self.runtime_dir = current;
        Ok(())
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
                // `--all` overrides default-app narrowing: the repository's
                // whole stack is selected, wherever it lives.
                Some(app_id) if !all => {
                    for (index, node) in self.nodes.iter().enumerate() {
                        if node.app.as_deref() == Some(app_id.as_str()) && node.is_running_service()
                        {
                            chosen.insert(index);
                            touched_apps.insert(app_id.clone());
                        }
                    }
                    // Standing in an app narrows the selection to that app, not
                    // to a stack without the repository's shared
                    // infrastructure: the API in `api/` was written against the
                    // database, cache and object store the workspace declares.
                    for index in self.shared_service_indices() {
                        chosen.insert(index);
                        touched_workspace = true;
                    }
                }
                _ => {
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
                for index in self.shared_service_indices() {
                    chosen.insert(index);
                    touched_workspace = true;
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

    /// Repository-level services (`app = None`): the database, cache and object
    /// store every app in the repository was written against, so any selection
    /// inside an app comes with them.
    fn shared_service_indices(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.app.is_none() && node.is_running_service())
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
        // A declared `port.env` names a variable the stack publishes: compose
        // interpolates it wherever it is declared and host tooling reads it. So
        // the whole stack's set belongs in the computed layer, where it reaches
        // `magictree env`, `--export`, `--explain` and every launched process.
        for node in &self.nodes {
            for (variable, port) in self.declared_port_variables(node, assignment) {
                vars.insert(variable, port.to_string());
            }
        }
        vars
    }

    pub fn build_env(&self, app: Option<&str>, assignment: &Assignment) -> Result<EnvPlan> {
        self.build_env_layers(app, assignment, false)
    }

    /// The layered environment for a dry run: the same shape as a real run, with
    /// every port shown as a placeholder *in the computed layer*, so a value
    /// that interpolates one (`WT_PORT_WEB = "${MAGICTREE_PORT_web_web}"`, a URL
    /// built from a port) reads as a placeholder too instead of as the sentinel
    /// number a preview assignment carries.
    pub fn preview_plan(
        &self,
        app: Option<&str>,
        mode: Option<ports::PortMode>,
    ) -> Result<EnvPlan> {
        let assignment = self.preview_assignment(mode);
        self.build_env_layers(app, &assignment, true)
    }

    fn build_env_layers(
        &self,
        app: Option<&str>,
        assignment: &Assignment,
        preview: bool,
    ) -> Result<EnvPlan> {
        let workspace_env = if self.loaded.root_is_app {
            &EMPTY_ENV
        } else {
            &self.loaded.workspace.env
        };
        let app_env = app
            .and_then(|id| self.loaded.app(id))
            .map(|app| &app.manifest.env)
            .unwrap_or(&EMPTY_ENV);
        // Every computed key is reserved, so a declared layer can only collide
        // with a variable a service publishes in `port.env`. Name the service
        // that owns it: "set by magictree" alone leaves the reader guessing.
        for (source, layer) in [("workspace", workspace_env), ("app", app_env)] {
            for key in layer.keys() {
                if let Some(owner) = self.port_variable_owner(key) {
                    bail!(
                        "{source} [env] sets '{key}', which service '{owner}' publishes in \
                         port.env; magictree already sets it for the whole stack, so remove \
                         the line"
                    );
                }
            }
        }
        let mut computed = self.computed_env(assignment);
        if preview {
            for value in computed.values_mut() {
                if *value == PLACEHOLDER_PORT.to_string() {
                    *value = PLACEHOLDER.to_string();
                }
            }
        }
        env::build(computed, workspace_env, app_env)
    }

    /// The service that publishes `variable` in `port.env`, when one does.
    fn port_variable_owner(&self, variable: &str) -> Option<String> {
        self.nodes.iter().find_map(|node| {
            let service = node.service()?;
            if service.expose != Expose::Port {
                return None;
            }
            service
                .ports()
                .iter()
                .any(|port| port.env.as_deref() == Some(variable))
                .then(|| node.qual())
        })
    }

    pub fn node_env(
        &self,
        node: &Node,
        assignment: &Assignment,
    ) -> Result<BTreeMap<String, String>> {
        // `computed_env` already carries every declared `port.env` in the stack,
        // so a node's environment is exactly the layered plan.
        Ok(self.build_env(node.app.as_deref(), assignment)?.vars)
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

    /// The bootstrap steps of every manifest the selection touches, workspace
    /// first. `sync` and `run` belong to the start, `after` to once the stack
    /// is healthy; both phases run in this order.
    pub fn bootstrap_targets(
        &self,
        selection: &[usize],
    ) -> Vec<(PathBuf, Option<String>, Bootstrap)> {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut targets = Vec::new();
        if !self.loaded.root_is_app {
            let bootstrap = self.loaded.workspace.bootstrap.clone();
            if !bootstrap.is_empty() {
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
            if bootstrap.is_empty() {
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
    /// allocated port shown as a placeholder so nothing is claimed. `mode` is
    /// the one the run would ask for, so a preview of `up --ports generated`
    /// does not show declared ports the run would not use.
    pub fn preview_env(&self, mode: Option<ports::PortMode>) -> Result<EnvPlan> {
        self.preview_plan(self.loaded.current_app.as_deref(), mode)
    }

    /// Placeholder port assignment used by dry runs; never written anywhere.
    ///
    /// A preview shows the ports the next real `up` would use, so a declared
    /// port is previewed as itself only where it would be taken: a `prefer` is
    /// the primary checkout's port, and a linked worktree ignores it. `mode` is
    /// what the run would ask for, `None` being this worktree's default.
    pub fn preview_assignment(&self, mode: Option<ports::PortMode>) -> Assignment {
        let primary = self.repo.is_main_worktree();
        let mode = mode.unwrap_or(if primary {
            ports::PortMode::Declared
        } else {
            ports::PortMode::Block
        });
        let mut assignment = Assignment {
            version: ports::ASSIGNMENT_VERSION,
            repo_key: self.repo.key(),
            worktree_id: self.repo.worktree_id(),
            worktree_path: Some(self.repo.worktree_root.to_string_lossy().to_string()),
            base: 0,
            mode,
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
                let preferred = match mode {
                    ports::PortMode::Declared => port.prefer,
                    ports::PortMode::Block => None,
                };
                let fixed = port.require.or(preferred).unwrap_or(PLACEHOLDER_PORT);
                assignment.ports.insert(self.port_key(node, name), fixed);
            }
        }
        assignment
    }
}
