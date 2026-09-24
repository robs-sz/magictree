use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "magictree.toml";
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default)]
    pub workspace: Option<Workspace>,
    #[serde(default)]
    pub app: Option<AppMeta>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub bootstrap: Bootstrap,
    #[serde(default)]
    pub services: Vec<Service>,
    #[serde(default)]
    pub jobs: BTreeMap<String, Job>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub apps: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AppMeta {
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Bootstrap {
    #[serde(default)]
    pub sync: Vec<String>,
    #[serde(default)]
    pub run: Vec<RunStep>,
    /// Steps that run once every service the `up` selected is healthy, so a
    /// script can use the stack it just started instead of racing it.
    #[serde(default)]
    pub after: Vec<RunStep>,
}

impl Bootstrap {
    /// True when this manifest has no steps around `up`, and so needs no
    /// bootstrap target of its own.
    pub fn is_empty(&self) -> bool {
        self.sync.is_empty() && self.run.is_empty() && self.after.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RunStep {
    Simple(String),
    Detailed {
        command: String,
        #[serde(default)]
        inputs: Vec<String>,
        /// Ask before running: `up` prints `Run task: <command> (y/N)` and
        /// runs the step only on a yes. A run without a terminal — scripts,
        /// agents, CI — always declines, so nothing unattended is run.
        #[serde(default)]
        ask: bool,
    },
}

impl RunStep {
    pub fn command(&self) -> &str {
        match self {
            RunStep::Simple(command) => command,
            RunStep::Detailed { command, .. } => command,
        }
    }

    pub fn inputs(&self) -> &[String] {
        match self {
            RunStep::Simple(_) => &[],
            RunStep::Detailed { inputs, .. } => inputs,
        }
    }

    pub fn asks(&self) -> bool {
        match self {
            RunStep::Simple(_) => false,
            RunStep::Detailed { ask, .. } => *ask,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Compose,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Expose {
    #[default]
    Port,
    None,
}

/// What "up" waits for. Most services keep running; an initialiser is finished
/// when its container exits, and dependents must not start before that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Wait {
    #[default]
    Running,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum When {
    #[default]
    Up,
    Manual,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Service {
    pub id: String,
    #[serde(default)]
    pub runtime: Option<Runtime>,
    #[serde(default)]
    pub compose: Option<ComposeRef>,
    #[serde(default)]
    pub target: Option<Target>,
    #[serde(default)]
    pub command: Option<String>,
    /// One entry per exposed port. A compose service may publish several
    /// (`${WT_PORT_OBJECTS}` and `${WT_PORT_OBJECTS_CONSOLE}`).
    #[serde(default)]
    pub port: Option<PortSpec>,
    #[serde(default)]
    pub ports: Vec<PortSpec>,
    #[serde(default)]
    pub expose: Expose,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub health: Option<Health>,
    /// `running` (default) waits for a healthy or running container; `exit`
    /// waits for a one-shot container to finish successfully.
    #[serde(default)]
    pub wait: Wait,
}

impl Service {
    /// All port declarations, singular form first.
    pub fn ports(&self) -> Vec<PortSpec> {
        let mut ports = Vec::new();
        if let Some(port) = &self.port {
            let mut port = port.clone();
            if port.name.is_none() {
                port.name = Some(self.id.clone());
            }
            ports.push(port);
        }
        for port in &self.ports {
            let mut port = port.clone();
            if port.name.is_none() {
                port.name = Some(self.id.clone());
            }
            ports.push(port);
        }
        ports
    }
}

/// How a host service is launched. `mise` and `just` are task-runner
/// front-ends for the same command string; `profile` selects a mise
/// environment, `args` are appended to the recipe.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Target {
    Command {
        command: String,
    },
    Mise {
        task: String,
        #[serde(default)]
        profile: Option<String>,
    },
    Just {
        recipe: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Npm {
        script: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Pnpm {
        script: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Uv {
        #[serde(default)]
        group: Option<String>,
        #[serde(default)]
        module: Option<String>,
        #[serde(default)]
        script: Option<String>,
        #[serde(default)]
        args: Vec<String>,
    },
    Python {
        module: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl Target {
    /// The exact shell command magictree runs, before env expansion.
    pub fn command(&self) -> String {
        match self {
            Target::Command { command } => command.clone(),
            Target::Mise { task, profile } => match profile {
                Some(profile) => format!("mise --profile {profile} run {task}"),
                None => format!("mise run {task}"),
            },
            Target::Just { recipe, args } => {
                let mut command = format!("just {recipe}");
                for arg in args {
                    command.push(' ');
                    command.push_str(&quote_arg(arg));
                }
                command
            }
            Target::Npm { script, args } => with_args(format!("npm run {script}"), args),
            Target::Pnpm { script, args } => with_args(format!("pnpm run {script}"), args),
            Target::Uv {
                group,
                module,
                script,
                args,
            } => {
                let prefix = match group {
                    Some(group) => format!("uv run --group {group}"),
                    None => "uv run".to_string(),
                };
                if let Some(script) = script {
                    with_args(format!("{prefix} {script}"), args)
                } else if let Some(module) = module {
                    let mut command = format!("{prefix} python -m {module}");
                    for arg in args {
                        command.push(' ');
                        command.push_str(&quote_arg(arg));
                    }
                    command
                } else {
                    // Validation rejects this; keep the command inert rather
                    // than inventing one.
                    prefix
                }
            }
            Target::Python { module, args } => {
                let mut command = format!("python3 -m {module}");
                for arg in args {
                    command.push(' ');
                    command.push_str(&quote_arg(arg));
                }
                command
            }
        }
    }
}

fn with_args(base: String, args: &[String]) -> String {
    let mut command = base;
    for arg in args {
        command.push(' ');
        command.push_str(&quote_arg(arg));
    }
    command
}

/// Quote one argument for the shell command `Target::command` builds.
///
/// Arguments are repository-controlled, and `${VAR}` expansion inside them is
/// a feature (`args = ["-p", "${APP_PORT}"]` relies on it), so quoting only
/// fires when the argument would otherwise change tokenization: whitespace,
/// quotes, backslashes, and shell control characters.
fn quote_arg(arg: &str) -> String {
    let needs_quoting = arg.is_empty()
        || arg.chars().any(|c| {
            matches!(
                c,
                ' ' | '\t' | '\n' | '\'' | '"' | '\\' | ';' | '|' | '&' | '(' | ')' | '<' | '>'
            )
        });
    if !needs_quoting {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, Deserialize)]
pub struct ComposeRef {
    pub file: String,
    pub service: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PortSpec {
    /// Stable identifier for this port inside the service, used to derive the
    /// environment variable name. Defaults to the service id.
    #[serde(default)]
    pub name: Option<String>,
    /// Container-side port to publish on the allocated host port.
    #[serde(default)]
    pub target: Option<u16>,
    /// Environment variable receiving the allocated port for host processes.
    #[serde(default)]
    pub env: Option<String>,
    /// Use this port when it is free, otherwise fall back to an allocated one.
    #[serde(default)]
    pub prefer: Option<u16>,
    /// Require this port; fail loudly when it is unavailable.
    #[serde(default)]
    pub require: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    #[serde(default)]
    pub http: Option<String>,
    #[serde(default)]
    pub tcp: Option<bool>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
}

impl Health {
    pub fn timeout_secs(&self, default: u64) -> u64 {
        self.timeout.unwrap_or(default)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub run: String,
    #[serde(default)]
    pub when: When,
    #[serde(default)]
    pub needs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct App {
    pub id: String,
    pub dir: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub workspace_dir: PathBuf,
    pub workspace: Manifest,
    pub apps: Vec<App>,
    pub current_app: Option<String>,
    /// True when the resolved manifest is a single app with no `[workspace]`.
    pub root_is_app: bool,
}

impl Loaded {
    pub fn load(cwd: &Path) -> Result<Self> {
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("resolving {}", cwd.display()))?;
        let manifest_path = find_up(&cwd)?.ok_or_else(|| {
            anyhow!(
                "no {MANIFEST_FILE} found in {} or any parent directory",
                cwd.display()
            )
        })?;
        let manifest_dir = manifest_path
            .parent()
            .expect("manifest path has a parent")
            .to_path_buf();
        let nearest = parse(&manifest_path)?;

        if nearest.workspace.is_some() {
            let apps = load_apps(&manifest_dir, &nearest)?;
            let current_app = apps
                .iter()
                .filter(|app| cwd.starts_with(&app.dir))
                .max_by_key(|app| app.dir.components().count())
                .map(|app| app.id.clone());
            return Ok(Self {
                workspace_dir: manifest_dir,
                workspace: nearest,
                apps,
                current_app,
                root_is_app: false,
            });
        }

        if let Some(parent) = manifest_dir.parent() {
            if let Some(candidate) = find_up(parent)? {
                let workspace_dir = candidate
                    .parent()
                    .expect("manifest path has a parent")
                    .to_path_buf();
                let workspace = parse(&candidate)?;
                if workspace.workspace.is_some() {
                    let apps = load_apps(&workspace_dir, &workspace)?;
                    let id = app_id(&manifest_dir, &nearest);
                    if !apps.iter().any(|app| app.id == id) {
                        bail!(
                            "app '{id}' at {} is not listed in [workspace].apps of {}",
                            manifest_dir.display(),
                            candidate.display()
                        );
                    }
                    return Ok(Self {
                        workspace_dir,
                        workspace,
                        apps,
                        current_app: Some(id),
                        root_is_app: false,
                    });
                }
            }
        }

        let id = app_id(&manifest_dir, &nearest);
        Ok(Self {
            workspace_dir: manifest_dir.clone(),
            workspace: nearest.clone(),
            apps: vec![App {
                id: id.clone(),
                dir: manifest_dir,
                manifest: nearest,
            }],
            current_app: Some(id),
            root_is_app: true,
        })
    }

    pub fn validate(&self) -> Result<()> {
        if !self.root_is_app {
            validate_manifest(&self.workspace, "workspace")?;
        }
        let mut seen = HashSet::new();
        for app in &self.apps {
            if !seen.insert(app.id.clone()) {
                bail!("duplicate app id '{}'", app.id);
            }
            validate_manifest(&app.manifest, &format!("app '{}'", app.id))?;
        }
        Ok(())
    }

    pub fn app(&self, id: &str) -> Option<&App> {
        self.apps.iter().find(|app| app.id == id)
    }
}

fn find_up(from: &Path) -> Result<Option<PathBuf>> {
    let mut dir = Some(from);
    while let Some(current) = dir {
        let candidate = current.join(MANIFEST_FILE);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        dir = current.parent();
    }
    Ok(None)
}

fn parse(path: &Path) -> Result<Manifest> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Parse a manifest that is already in memory, for callers that read the file
/// themselves; `init` compares an existing manifest with what it planned.
pub fn parse_str(contents: &str) -> Result<Manifest> {
    toml::from_str(contents).context("parsing manifest")
}

fn load_apps(workspace_dir: &Path, workspace: &Manifest) -> Result<Vec<App>> {
    let Some(config) = &workspace.workspace else {
        return Ok(Vec::new());
    };
    let mut apps = Vec::new();
    let mut seen = HashSet::new();
    for rel in &config.apps {
        let dir = workspace_dir.join(rel);
        let path = dir.join(MANIFEST_FILE);
        if !path.is_file() {
            bail!(
                "[workspace].apps lists '{rel}' but {} does not exist",
                path.display()
            );
        }
        let manifest = parse(&path)?;
        let id = app_id(&dir, &manifest);
        if !seen.insert(id.clone()) {
            bail!("duplicate app id '{id}' (from {})", path.display());
        }
        apps.push(App {
            id,
            dir: dir.clone(),
            manifest,
        });
    }
    Ok(apps)
}

fn app_id(dir: &Path, manifest: &Manifest) -> String {
    manifest
        .app
        .as_ref()
        .and_then(|app| app.id.clone())
        .unwrap_or_else(|| {
            dir.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "app".to_string())
        })
}

fn validate_manifest(manifest: &Manifest, scope: &str) -> Result<()> {
    if manifest.version != MANIFEST_VERSION {
        bail!(
            "{scope}: unsupported manifest version {} (expected {MANIFEST_VERSION})",
            manifest.version
        );
    }
    let mut ids = HashSet::new();
    for service in &manifest.services {
        if !ids.insert(service.id.clone()) {
            bail!("{scope}: duplicate service id '{}'", service.id);
        }
        let runtime = service.runtime.unwrap_or(if service.compose.is_some() {
            Runtime::Compose
        } else {
            Runtime::Host
        });
        match runtime {
            Runtime::Compose => {
                let Some(reference) = &service.compose else {
                    bail!(
                        "{scope}: service '{}' uses runtime = \"compose\" but has no compose reference",
                        service.id
                    );
                };
                if reference.file.is_empty() || reference.service.is_empty() {
                    bail!(
                        "{scope}: service '{}' needs compose.file and compose.service",
                        service.id
                    );
                }
                if service.target.is_some() || service.command.is_some() {
                    bail!(
                        "{scope}: service '{}' is a compose service and cannot also declare a command or target",
                        service.id
                    );
                }
            }
            Runtime::Host => {
                if service.target.is_none() && service.command.is_none() {
                    bail!(
                        "{scope}: service '{}' is a host service and needs either command or [services.target]",
                        service.id
                    );
                }
                if service.target.is_some() && service.command.is_some() {
                    bail!(
                        "{scope}: service '{}' cannot set both command and target",
                        service.id
                    );
                }
                if let Some(Target::Uv {
                    script: None,
                    module: None,
                    ..
                }) = &service.target
                {
                    bail!(
                        "{scope}: service '{}' uses a uv target with neither script nor module",
                        service.id
                    );
                }
            }
        }
        let mut names = HashSet::new();
        for port in service.ports() {
            let name = port.name.clone().unwrap_or_else(|| service.id.clone());
            if !names.insert(name.clone()) {
                bail!(
                    "{scope}: service '{}' declares port '{name}' more than once",
                    service.id
                );
            }
            if port.prefer.is_some() && port.require.is_some() {
                bail!(
                    "{scope}: service '{}' port '{name}' cannot set both prefer and require",
                    service.id
                );
            }
            if let Some(variable) = &port.env {
                if is_reserved_env(variable) {
                    bail!(
                        "{scope}: service '{}' port '{name}' names reserved environment \
                         variable '{variable}'",
                        service.id
                    );
                }
            }
        }
    }
    if let Some(id) = service_job_overlap(manifest).first() {
        bail!("{scope}: '{id}' is declared as both a service and a job");
    }
    for key in manifest.env.keys() {
        if is_reserved_env(key) {
            bail!("{scope}: environment key '{key}' is reserved by magictree");
        }
    }
    Ok(())
}

fn service_job_overlap(manifest: &Manifest) -> Vec<String> {
    let services: HashSet<&String> = manifest
        .services
        .iter()
        .map(|service| &service.id)
        .collect();
    manifest
        .jobs
        .keys()
        .filter(|id| services.contains(id))
        .cloned()
        .collect()
}

pub fn is_reserved_env(key: &str) -> bool {
    key.starts_with("MAGICTREE_") || key == "COMPOSE_PROJECT_NAME" || key == "COMPOSE_FILE"
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Service(Box<Service>),
    Job(Box<Job>),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub app: Option<String>,
    pub manifest_dir: PathBuf,
    pub dir: PathBuf,
    pub id: String,
    pub kind: NodeKind,
}

impl Node {
    pub fn qual(&self) -> String {
        match &self.app {
            Some(app) => format!("{app}:{}", self.id),
            None => self.id.clone(),
        }
    }

    pub fn service(&self) -> Option<&Service> {
        match &self.kind {
            NodeKind::Service(service) => Some(service),
            NodeKind::Job(_) => None,
        }
    }

    pub fn job(&self) -> Option<&Job> {
        match &self.kind {
            NodeKind::Job(job) => Some(job),
            NodeKind::Service(_) => None,
        }
    }

    pub fn needs(&self) -> &[String] {
        match &self.kind {
            NodeKind::Service(service) => &service.needs,
            NodeKind::Job(job) => &job.needs,
        }
    }

    pub fn is_running_service(&self) -> bool {
        matches!(self.kind, NodeKind::Service(_))
    }

    /// Services with no explicit runtime default to a host process.
    pub fn runtime(&self) -> Option<Runtime> {
        self.service().map(|service| {
            service.runtime.unwrap_or(if service.compose.is_some() {
                Runtime::Compose
            } else {
                Runtime::Host
            })
        })
    }

    /// The shell command for a host service, whatever its target flavour.
    pub fn command(&self) -> Option<String> {
        let service = self.service()?;
        if let Some(target) = &service.target {
            return Some(target.command());
        }
        service.command.clone()
    }
}

/// Flatten the loaded manifests into a single node list, workspace-level
/// services first so shared infrastructure starts before the apps that use it.
pub fn nodes(loaded: &Loaded) -> Vec<Node> {
    let mut nodes = Vec::new();
    let push_workspace = |nodes: &mut Vec<Node>| {
        if loaded.root_is_app {
            return;
        }
        for service in &loaded.workspace.services {
            nodes.push(Node {
                app: None,
                manifest_dir: loaded.workspace_dir.clone(),
                dir: loaded.workspace_dir.clone(),
                id: service.id.clone(),
                kind: NodeKind::Service(Box::new(service.clone())),
            });
        }
        for (id, job) in &loaded.workspace.jobs {
            nodes.push(Node {
                app: None,
                manifest_dir: loaded.workspace_dir.clone(),
                dir: loaded.workspace_dir.clone(),
                id: id.clone(),
                kind: NodeKind::Job(Box::new(job.clone())),
            });
        }
    };
    push_workspace(&mut nodes);
    for app in &loaded.apps {
        // A repository with no workspace layer has one app; tagging its
        // services would only add noise to names and port variables.
        let tag = if loaded.root_is_app {
            None
        } else {
            Some(app.id.clone())
        };
        for service in &app.manifest.services {
            nodes.push(Node {
                app: tag.clone(),
                manifest_dir: app.dir.clone(),
                dir: app.dir.clone(),
                id: service.id.clone(),
                kind: NodeKind::Service(Box::new(service.clone())),
            });
        }
        for (id, job) in &app.manifest.jobs {
            nodes.push(Node {
                app: tag.clone(),
                manifest_dir: app.dir.clone(),
                dir: app.dir.clone(),
                id: id.clone(),
                kind: NodeKind::Job(Box::new(job.clone())),
            });
        }
    }
    nodes
}

/// Resolve `needs` strings into node indices.
///
/// A bare name resolves to the service of the same app first, then to a
/// workspace-level service. `app:service` always resolves globally.
pub fn dependencies(nodes: &[Node]) -> Result<Vec<Vec<usize>>> {
    let mut by_qual: HashMap<String, usize> = HashMap::new();
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        by_qual.insert(node.qual(), index);
        by_id.entry(node.id.clone()).or_default().push(index);
    }

    let mut edges = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut deps = Vec::new();
        for need in node.needs() {
            let target = if need.contains(':') {
                *by_qual.get(need).ok_or_else(|| {
                    anyhow!("{}: dependency '{}' does not exist", node.qual(), need)
                })?
            } else {
                let same_app = node
                    .app
                    .as_ref()
                    .and_then(|app| by_qual.get(&format!("{app}:{need}")).copied());
                let workspace_level = nodes
                    .iter()
                    .position(|candidate| candidate.app.is_none() && candidate.id == *need);
                match (same_app, workspace_level, by_id.get(need)) {
                    (Some(index), _, _) => index,
                    (None, Some(index), _) => index,
                    (None, None, Some(candidates)) if candidates.len() == 1 => candidates[0],
                    (None, None, Some(_)) => bail!(
                        "{}: dependency '{}' is ambiguous; qualify it as app:service",
                        node.qual(),
                        need
                    ),
                    _ => bail!("{}: dependency '{}' does not exist", node.qual(), need),
                }
            };
            deps.push(target);
        }
        edges.push(deps);
    }
    Ok(edges)
}

/// Expand the selection to include dependencies and return a topological order
/// with dependencies first.
pub fn order(nodes: &[Node], edges: &[Vec<usize>], selection: &[usize]) -> Result<Vec<usize>> {
    let mut scope = vec![false; nodes.len()];
    let mut stack = selection.to_vec();
    while let Some(index) = stack.pop() {
        if scope[index] {
            continue;
        }
        scope[index] = true;
        for &dep in &edges[index] {
            stack.push(dep);
        }
    }

    order_scoped(nodes, edges, selection, &scope)
}

/// Topologically order exactly the selected nodes; unselected dependencies are
/// left out, while dependencies selected explicitly still precede their users.
pub(crate) fn order_selected(
    nodes: &[Node],
    edges: &[Vec<usize>],
    selection: &[usize],
) -> Result<Vec<usize>> {
    let mut scope = vec![false; nodes.len()];
    for &index in selection {
        scope[index] = true;
    }
    order_scoped(nodes, edges, selection, &scope)
}

fn order_scoped(
    nodes: &[Node],
    edges: &[Vec<usize>],
    selection: &[usize],
    scope: &[bool],
) -> Result<Vec<usize>> {
    let mut state = vec![0u8; nodes.len()];
    let mut result = Vec::new();
    for &index in selection {
        visit(
            index,
            nodes,
            edges,
            scope,
            &mut state,
            &mut result,
            &mut Vec::new(),
        )?;
    }
    Ok(result)
}

fn visit(
    index: usize,
    nodes: &[Node],
    edges: &[Vec<usize>],
    scope: &[bool],
    state: &mut [u8],
    result: &mut Vec<usize>,
    path: &mut Vec<usize>,
) -> Result<()> {
    match state[index] {
        2 => return Ok(()),
        1 => {
            let names: Vec<String> = path
                .iter()
                .chain(std::iter::once(&index))
                .map(|&i| nodes[i].qual())
                .collect();
            bail!("dependency cycle: {}", names.join(" -> "));
        }
        _ => {}
    }
    state[index] = 1;
    path.push(index);
    for &dep in &edges[index] {
        if scope[dep] {
            visit(dep, nodes, edges, scope, state, result, path)?;
        }
    }
    path.pop();
    state[index] = 2;
    result.push(index);
    Ok(())
}
