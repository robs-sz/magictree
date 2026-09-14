use crate::bootstrap;
use crate::compose::{self, ComposeRunner, ContainerState};
use crate::config::Config;
use crate::ctx::{runtime_dir_for, Ctx};
use crate::discover;
use crate::doctor;
use crate::dryrun;
use crate::health;
use crate::init;
use crate::manifest::{self, Node, NodeKind, Runtime, Wait};
use crate::paths::Paths;
use crate::ports;
use crate::repo::Repo;
use crate::run;
use crate::worktrees;
use anyhow::{anyhow, bail, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "magictree",
    version,
    about = "Per-worktree development environments"
)]
pub struct Cli {
    /// Print what would happen and create nothing.
    #[arg(long, global = true)]
    pub dry_run: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Read the repository and report what a stack would need.
    Discover(DiscoverArgs),
    /// Write magictree.toml from discovered facts and answers.
    Init(InitArgs),
    /// Check the manifests against what the repository now says.
    Doctor(DoctorArgs),
    /// Create a worktree and start its stack.
    New(NewArgs),
    /// Create and start this worktree's stack.
    Up(UpArgs),
    /// Stop this worktree's stack. Volumes are kept unless --volumes.
    Down(DownArgs),
    /// Remove a worktree after stopping its stack.
    Rm(RmArgs),
    /// List worktrees of this repository with their ports.
    List(ListArgs),
    /// Reclaim port assignments and compose resources of deleted worktrees.
    Gc(GcArgs),
    /// Show each service's process/container state and port.
    Status(StatusArgs),
    /// Print a service's captured output.
    Logs(LogsArgs),
    /// Show this worktree's port assignment.
    Ports(PortsArgs),
    /// Print the resolved environment.
    Env(EnvArgs),
    /// Print a shell completion script.
    Completion(CompletionArgs),
}

#[derive(Args)]
pub struct CompletionArgs {
    /// Shell to generate completions for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Args)]
pub struct DiscoverArgs {
    /// Write the report to this path instead of stdout.
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Emit JSON (the default when writing to a file).
    #[arg(long)]
    pub json: bool,
    /// Print the answer set implied by the report's defaults.
    #[arg(long)]
    pub default_answers: bool,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct InitArgs {
    /// Answers produced from a discovery report.
    #[arg(long, value_name = "FILE")]
    pub answers: Option<PathBuf>,
    /// Accept every default without prompting.
    #[arg(long, alias = "yes")]
    pub accept_defaults: bool,
    /// Overwrite existing manifests.
    #[arg(long)]
    pub force: bool,
    /// Print the manifests without writing them.
    #[arg(long)]
    pub print: bool,
    /// Also write the answers used, so the run can be replayed.
    #[arg(long, value_name = "FILE")]
    pub save_answers: Option<PathBuf>,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct DoctorArgs {
    /// Refresh the cached discovery report first.
    #[arg(long)]
    pub refresh: bool,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct NewArgs {
    /// Branch to check out, creating it when it does not exist.
    pub branch: String,
    /// Base revision for a new branch (defaults to HEAD).
    #[arg(long)]
    pub base: Option<String>,
    /// Explicit checkout directory.
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Create a detached worktree at the base revision.
    #[arg(long)]
    pub detach: bool,
    /// Create the worktree without starting its stack.
    #[arg(long)]
    pub no_up: bool,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct RmArgs {
    /// Worktree path, or a branch name to resolve.
    pub target: String,
    /// Discard uncommitted changes in the worktree.
    #[arg(long)]
    pub force: bool,
    /// Skip `down` before removing.
    #[arg(long)]
    pub no_down: bool,
    /// Also remove compose volumes.
    #[arg(long)]
    pub volumes: bool,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct GcArgs {
    /// Also run `git worktree prune` for removed checkouts.
    #[arg(long)]
    pub prune: bool,
    /// Sweep every repository the state dir holds a record of, including ones
    /// whose repository is itself gone.
    #[arg(long, conflicts_with_all = ["prune", "cwd"])]
    pub all: bool,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct UpArgs {
    /// Services to start (id or app:id). Defaults to the current app, or
    /// everything at the workspace root.
    pub services: Vec<String>,
    /// Restrict to whole apps.
    #[arg(long = "app", value_name = "APP")]
    pub apps: Vec<String>,
    /// Start every service in the repository.
    #[arg(long)]
    pub all: bool,
    /// Directory to resolve the repository from.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct DownArgs {
    /// Also remove named volumes (destroys per-worktree database state).
    #[arg(long)]
    pub volumes: bool,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Also run each service's health probe (may take a few seconds).
    #[arg(long)]
    pub probe: bool,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct LogsArgs {
    pub service: String,
    #[arg(short, long)]
    pub follow: bool,
    #[arg(long, default_value_t = 40)]
    pub lines: usize,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct PortsArgs {
    /// Discard the current assignment and allocate a new block.
    #[arg(long)]
    pub reassign: bool,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct EnvArgs {
    /// Show the merged environment of a specific app.
    #[arg(long)]
    pub app: Option<String>,
    /// Emit `export KEY=value` lines.
    #[arg(long)]
    pub export: bool,
    /// Show which layer set each value.
    #[arg(long)]
    pub explain: bool,
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

pub fn dispatch(cli: Cli) -> Result<()> {
    let dry_run = cli.dry_run;
    match cli.command {
        Command::Discover(args) => cmd_discover(args),
        Command::Init(args) => cmd_init(args, dry_run),
        Command::Doctor(args) => cmd_doctor(args),
        Command::New(args) => cmd_new(args, dry_run),
        Command::Up(args) => cmd_up(args, dry_run),
        Command::Down(args) => cmd_down(args, dry_run),
        Command::Rm(args) => cmd_rm(args, dry_run),
        Command::List(args) => cmd_list(args),
        Command::Gc(args) => cmd_gc(args, dry_run),
        Command::Status(args) => cmd_status(args),
        Command::Logs(args) => cmd_logs(args),
        Command::Ports(args) => cmd_ports(args, dry_run),
        Command::Env(args) => cmd_env(args, dry_run),
        Command::Completion(args) => cmd_completion(args),
    }
}

/// Print a completion script to stdout. Install it with, for example:
///   magictree completion zsh > ~/.zfunc/_magictree
fn cmd_completion(args: CompletionArgs) -> Result<()> {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(args.shell, &mut command, name, &mut std::io::stdout());
    Ok(())
}

fn cmd_discover(args: DiscoverArgs) -> Result<()> {
    let root = resolve_cwd(args.cwd)?;
    let report = discover::extract(&root)?;
    if args.default_answers {
        let answers = init::default_answers(&report);
        print!("{}", serde_json::to_string_pretty(&answers)?);
        return Ok(());
    }
    let payload = serde_json::to_string_pretty(&report)?;
    match args.report {
        Some(path) => {
            report.write(&path)?;
            println!(
                "wrote {} ({} facts, {} unknowns)",
                path.display(),
                report.facts.len(),
                report.unknowns.len()
            );
        }
        None => {
            if args.json || !std::io::stdout().is_terminal() {
                println!("{payload}");
            } else {
                println!("repo   {}", report.repo.root);
                println!(
                    "apps   {}",
                    if report.apps.is_empty() {
                        "(none detected)".to_string()
                    } else {
                        report
                            .apps
                            .iter()
                            .map(|app| app.dir.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                );
                println!("\n# facts");
                for fact in &report.facts {
                    println!(
                        "{:<6} {:<10} {:<8} {}",
                        fact.id,
                        fact.source,
                        format!("{:?}", fact.confidence).to_lowercase(),
                        describe_fact(&fact.data)
                    );
                }
                println!("\n# hardcoded ports");
                let facts: Vec<&discover::report::Fact> = report.facts.iter().collect();
                let pinned = discover::ports::all(&facts);
                if pinned.is_empty() {
                    println!("(none)");
                }
                for (fact, literal) in pinned {
                    let container = literal.container.clone().unwrap_or_default();
                    let suggestion_variable = port_variable_for(&report, fact, literal);
                    println!(
                        "{:<6} {:<24} {:<24} {}",
                        fact.id,
                        fact.source,
                        format!(
                            "{} {}",
                            discover::ports::step_label(fact.kind, &container),
                            literal.literal
                        ),
                        discover::ports::suggestion(literal, suggestion_variable)
                    );
                }
                println!("\n# unknowns");
                if report.unknowns.is_empty() {
                    println!("(none)");
                }
                for unknown in &report.unknowns {
                    println!(
                        "{:<22} {:<10} {}",
                        unknown.id,
                        unknown.scope.label(),
                        unknown.question
                    );
                    if !unknown.options.is_empty() {
                        println!("{:<22} options: {}", "", unknown.options.join(", "));
                    }
                }
                println!("\nreport hash {}", report.report_hash);
                println!("write it with: magictree discover --report discovery.json");
            }
        }
    }
    Ok(())
}

/// The variable a pinned port should read in `discover`'s advice.
///
/// With no manifest there is nothing to point at yet, so the answer is generic —
/// except for a script that serves Storybook, which reads no variable at all:
/// the one `init` hands it is the one a rewrite has to name.
fn port_variable_for(
    report: &discover::Report,
    fact: &discover::Fact,
    literal: &discover::report::PortLiteralFact,
) -> Option<&'static str> {
    use discover::report::FactKind;
    if fact.kind == FactKind::EnvExample {
        // A committed template is nobody's process yet.
        return None;
    }
    let step = literal.container.as_deref().unwrap_or_default();
    let app = fact.app.as_deref().unwrap_or_default();
    let storybook = report
        .storybook_scripts(app)
        .iter()
        .any(|spec| spec.split_once(':').map(|(_, script)| script) == Some(step));
    Some(if storybook {
        init::STORYBOOK_PORT
    } else {
        "PORT"
    })
}

fn describe_fact(data: &discover::FactData) -> String {
    use discover::FactData as Data;
    match data {
        Data::Compose { services, .. } => format!(
            "{} compose service(s): {}",
            services.len(),
            services
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Data::Node {
            scripts,
            package_manager,
            ..
        } => format!(
            "{} script(s), manager {}",
            scripts.len(),
            package_manager.clone().unwrap_or_else(|| "unknown".into())
        ),
        Data::Workspace { tool, packages } => format!("{tool}, {} package(s)", packages.len()),
        Data::Storybook {
            config_dir,
            dev_scripts,
            has_mcp,
        } => format!(
            "storybook, config {}, {} dev script(s){}",
            config_dir.clone().unwrap_or_else(|| "none".into()),
            dev_scripts.len(),
            if *has_mcp { ", MCP addon" } else { "" }
        ),
        Data::Mise { tasks, tools, .. } => {
            format!("{} task(s), {} tool(s)", tasks.len(), tools.len())
        }
        Data::Just {
            recipes,
            modules,
            env_variables,
            ..
        } => format!(
            "{} recipe(s), {} module(s), {} env var(s)",
            recipes.len(),
            modules.len(),
            env_variables.len()
        ),
        Data::Python {
            manager,
            dependency_groups,
            ..
        } => format!("{manager}, groups: {}", dependency_groups.join(", ")),
        Data::EnvExample { variables, .. } => format!("{} variable(s)", variables.len()),
        Data::Procfile { processes, .. } => format!("{} process(es)", processes.len()),
    }
}

fn cmd_init(args: InitArgs, dry_run: bool) -> Result<()> {
    let root = resolve_cwd(args.cwd)?;
    let report = discover::extract(&root)?;
    let answers = match &args.answers {
        Some(path) => discover::AnswerSet::read_json(path)?,
        None if args.accept_defaults => init::default_answers(&report),
        None => init::wizard::run(&report)?,
    };
    if let Some(path) = &args.save_answers {
        answers.write(path)?;
    }
    let planned = init::plan(&report, &answers, &root)?;
    for warning in init::warnings(&report, &answers) {
        eprintln!("warning: {warning}");
    }
    if dry_run || args.print {
        println!("dry run — no manifest is written\n");
        for file in &planned {
            println!("# {}", file.path.display());
            print!("{}", file.contents);
            println!();
        }
        return Ok(());
    }
    for path in init::apply(&planned, args.force)? {
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn cmd_doctor(args: DoctorArgs) -> Result<()> {
    let root = resolve_cwd(args.cwd)?;
    let _ = args.refresh;
    let drift = doctor::check(&root)?;
    if drift.is_empty() {
        println!("no drift: manifests match the repository");
        return Ok(());
    }
    let mut real = 0usize;
    for entry in &drift {
        let marker = if entry.is_drift() { "drift" } else { "info " };
        println!("{marker} {}", entry.summary);
        if let Some(suggestion) = &entry.suggestion {
            println!("      {suggestion}");
        }
        if entry.is_drift() {
            real += 1;
        }
    }
    if real == 0 {
        println!(
            "
no drift: manifests match the repository"
        );
        return Ok(());
    }
    bail!("{real} drift finding(s)");
}

fn cmd_new(args: NewArgs, dry_run: bool) -> Result<()> {
    let start = resolve_cwd(args.cwd)?;
    let repo = Repo::open(&start)?;
    let path = match &args.path {
        Some(path) => {
            if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()?.join(path)
            }
        }
        None => worktrees::plan_path(&repo, &args.branch)?,
    };
    if dry_run {
        println!("dry run — no worktree is created");
        println!(
            "would run git worktree add {} {}",
            args.branch,
            path.display()
        );
        println!("would then run ensure in {}", path.display());
        return Ok(());
    }
    // Starting a stack needs a manifest the new worktree will actually have.
    // Checking first avoids leaving a half-created worktree behind.
    if !args.no_up {
        if let Some(hint) = worktrees::untracked_manifest_hint(&repo) {
            bail!("cannot start a stack in the new worktree\n\n{hint}");
        }
    }
    let path = worktrees::create(
        &repo,
        &args.branch,
        args.base.as_deref(),
        args.path.as_deref(),
        args.detach,
    )?;
    println!("created {}", path.display());
    if args.no_up {
        println!(
            "stack not started (--no-up); run `magictree up` in {}",
            path.display()
        );
        return Ok(());
    }
    if let Some(hint) = worktrees::missing_manifest_hint(&repo, &path) {
        // Nothing to start, and the generic "no magictree.toml" would not say why.
        bail!("{} has no stack definition\n\n{hint}", path.display());
    }
    let mut ctx = Ctx::load(&path)?;
    let selection = ctx.scope(&[], &[], false)?;
    ensure(&mut ctx, &selection).map(|_| ())
}

fn cmd_rm(args: RmArgs, dry_run: bool) -> Result<()> {
    let start = resolve_cwd(args.cwd)?;
    let repo = Repo::open(&start)?;
    let target = resolve_target(&repo, &args.target)?;
    if dry_run {
        println!("dry run — nothing is stopped or removed");
        println!("would run down in {}", target.display());
        println!(
            "would run git worktree remove {}{}",
            target.display(),
            if args.force { " --force" } else { "" }
        );
        println!("the branch would be kept");
        return Ok(());
    }

    if !args.no_down {
        // Stop whatever this worktree recorded, even if its manifest is gone or
        // no longer parses. Without this the checkout disappears while its
        // processes keep running and their pid files with it.
        let runtime_dir = runtime_dir_for(&Paths::new()?, &Repo::open(&target)?);
        match ctx_stop_recorded(&runtime_dir) {
            Ok(stopped) => {
                for name in stopped {
                    println!("{name}: stopped");
                }
            }
            Err(error) => println!("could not stop recorded processes: {error}"),
        }
        match Ctx::load(&target) {
            Ok(_) => {
                let _ = cmd_down(
                    DownArgs {
                        volumes: args.volumes,
                        cwd: Some(target.clone()),
                    },
                    false,
                );
            }
            Err(error) => println!("skipping compose teardown: {error}"),
        }
    }

    worktrees::remove(&repo, &target, args.force)?;
    println!("removed {} (branch kept)", target.display());
    let _ = paths_cleanup(&repo);
    Ok(())
}

fn cmd_list(args: ListArgs) -> Result<()> {
    let paths = Paths::new()?;
    let repo = Repo::open(&resolve_cwd(args.cwd)?)?;
    println!("{:<10} {:<44} ports", "worktree", "path");
    for (id, path, ports) in worktrees::worktree_rows(&repo, &paths)? {
        println!("{id:<10} {:<44} {ports}", path.display());
    }
    Ok(())
}

fn cmd_gc(args: GcArgs, dry_run: bool) -> Result<()> {
    let paths = Paths::new()?;
    let config = Config::load(&paths)?;
    let apply = !dry_run;
    if !apply {
        println!("dry run — nothing is released");
    }
    let timeout = Duration::from_secs(config.stop_timeout_secs);
    if args.all {
        return worktrees::gc_all(&paths, timeout, apply);
    }
    let repo = Repo::open(&resolve_cwd(args.cwd)?)?;
    worktrees::gc(&paths, &repo, timeout, apply)?;
    if args.prune {
        worktrees::prune(&repo, !apply)?;
    }
    Ok(())
}

fn paths_cleanup(repo: &Repo) -> Result<()> {
    worktrees::prune(repo, true)
}

/// Stop host processes recorded in a worktree without loading its manifest.
fn ctx_stop_recorded(runtime_dir: &Path) -> Result<Vec<String>> {
    let timeout = Duration::from_secs(Config::load(&Paths::new()?)?.stop_timeout_secs);
    run::stop_all(runtime_dir, timeout).map(|entries| {
        entries
            .into_iter()
            .filter(|(_, running)| *running)
            .map(|(name, _)| name)
            .collect()
    })
}

/// Accept a path, a branch name, or a directory name for `rm`.
fn resolve_target(repo: &Repo, target: &str) -> Result<PathBuf> {
    let direct = PathBuf::from(target);
    if direct.is_dir() {
        return Ok(direct.canonicalize().unwrap_or(direct));
    }
    for entry in repo.worktrees()? {
        let matches_branch = entry.branch.as_deref() == Some(target);
        let matches_dir = entry
            .path
            .file_name()
            .map(|name| name.to_string_lossy() == target)
            .unwrap_or(false);
        if matches_branch || matches_dir {
            return Ok(entry.path);
        }
    }
    bail!("no worktree matches '{target}'");
}

fn resolve_cwd(cwd: Option<PathBuf>) -> Result<PathBuf> {
    match cwd {
        Some(path) => Ok(path),
        None => Ok(std::env::current_dir()?),
    }
}

fn cmd_up(args: UpArgs, dry_run: bool) -> Result<()> {
    let mut ctx = Ctx::load(&resolve_cwd(args.cwd)?)?;
    let selection = ctx.scope(&args.services, &args.apps, args.all)?;
    if dry_run {
        return dryrun::up(&ctx, &selection);
    }
    ensure(&mut ctx, &selection).map(|_| ())
}

/// The idempotent core: allocate ports, materialise env, bootstrap, then start
/// services in dependency order, waiting for each to become healthy.
fn ensure(ctx: &mut Ctx, selection: &[usize]) -> Result<ports::Assignment> {
    // Every service gets an assignment, not just the selected ones: a later
    // partial `up` still has to write an override for its dependencies, and a
    // stable worktree-wide assignment is easier to reason about.
    let all_services = ctx.all_service_indices();
    let assignment = ports::ensure(
        &ctx.paths,
        &ctx.config,
        &ctx.repo.key(),
        &ctx.repo.worktree_id(),
        &ctx.repo.worktree_root,
        &ctx.port_requests(&all_services),
    )?;
    ctx.adopt_legacy_runtime()?;
    ctx.ensure_runtime_dirs()?;

    let mirror = ctx.build_env(None, &assignment)?;
    std::fs::write(ctx.runtime_dir.join("env"), mirror.dotenv())?;
    // `rm` needs the assignment to stop services even when the worktree's
    // manifest is already gone or unusable.
    std::fs::write(
        ctx.runtime_dir.join("ports.json"),
        format!("{}\n", serde_json::to_string_pretty(&assignment)?),
    )?;

    for (dir, app, steps) in ctx.bootstrap_targets(selection) {
        for message in bootstrap::sync_files(&ctx.repo, &ctx.repo.worktree_root, &steps.sync)? {
            println!("{message}");
        }
        if !steps.run.is_empty() {
            let env = ctx.build_env(app.as_deref(), &assignment)?;
            for message in bootstrap::run_steps(
                &ctx.runtime_dir,
                &ctx.repo.worktree_root,
                &dir,
                &steps.run,
                &env.vars,
            )? {
                println!("{message}");
            }
        }
    }

    let runners = ctx.compose_runners(selection, &assignment)?;
    if !runners.is_empty() {
        compose::ensure_docker()?;
    }

    for &index in selection {
        let node = &ctx.nodes[index];
        let env = ctx.node_env(node, &assignment)?;
        let outcome = match &node.kind {
            NodeKind::Job(job) => {
                println!("{}: running", node.qual());
                match run::run_once(&job.run, &node.dir, &env) {
                    Ok(()) => {
                        println!("{}: done", node.qual());
                        Ok(None)
                    }
                    Err(error) => Err((None, format!("job failed: {error}"))),
                }
            }
            NodeKind::Service(service) => match node.runtime() {
                Some(Runtime::Compose) => {
                    let key = ctx
                        .runner_key(node)
                        .ok_or_else(|| anyhow!("{}: missing compose reference", node.qual()))?;
                    let runner = runners
                        .get(&key)
                        .ok_or_else(|| anyhow!("{}: no compose runner", node.qual()))?;
                    let container = service
                        .compose
                        .as_ref()
                        .expect("validated compose reference")
                        .service
                        .clone();
                    println!("{}: starting container", node.qual());
                    match runner.up_service(&container, &env).and_then(|()| {
                        match service.wait {
                            // An initialiser is finished when its container
                            // exits; dependents must not start before that.
                            Wait::Exit => wait_for_exit(
                                runner,
                                &container,
                                &env,
                                Duration::from_secs(initializer_timeout(ctx)),
                            ),
                            Wait::Running => {
                                wait_ready(ctx, node, &env, &assignment, Some((runner, &container)))
                            }
                        }
                    }) {
                        Ok(()) => Ok(Some(runner)),
                        Err(error) => Err((Some(runner), error.to_string())),
                    }
                }
                _ => {
                    let alive = run::read_pid(&ctx.runtime_dir, &node.qual())
                        .map(run::is_alive)
                        .unwrap_or(false);
                    let started = if alive {
                        println!("{}: already running", node.qual());
                        Ok(())
                    } else {
                        match node.command() {
                            Some(command) => match run::start(
                                &ctx.runtime_dir,
                                &node.qual(),
                                &command,
                                &node.dir,
                                &env,
                            ) {
                                Ok(pid) => {
                                    println!("{}: started (pid {pid})", node.qual());
                                    Ok(())
                                }
                                Err(error) => Err(error),
                            },
                            None => Err(anyhow!("no command or target")),
                        }
                    };
                    match started.and_then(|()| wait_ready(ctx, node, &env, &assignment, None)) {
                        Ok(()) => Ok(None),
                        Err(error) => Err((None, error.to_string())),
                    }
                }
            },
        };

        if let Err((runner, reason)) = outcome {
            let report = ctx.describe_failure(node, &assignment, runner, &reason);
            return Err(anyhow!(report));
        }
    }

    print_summary(ctx, selection, &assignment);
    Ok(assignment)
}

/// Everything reachable, so it is obvious how to use the stack once it is up.
fn print_summary(ctx: &Ctx, selection: &[usize], assignment: &ports::Assignment) {
    let mut reachable: Vec<(String, u16)> = Vec::new();
    let mut internal: Vec<String> = Vec::new();

    for &index in selection {
        let node = &ctx.nodes[index];
        if node.service().is_none() {
            continue;
        }
        let ports = ctx.assigned_ports(node, assignment);
        if ports.is_empty() {
            internal.push(node.qual());
        } else {
            reachable.extend(ports);
        }
    }
    if reachable.is_empty() && internal.is_empty() {
        return;
    }

    println!();
    if !reachable.is_empty() {
        let width = reachable
            .iter()
            .map(|(label, _)| label.len())
            .max()
            .unwrap_or(4);
        for (label, port) in &reachable {
            println!("{label:<width$}   http://localhost:{port}");
        }
    }
    if !internal.is_empty() {
        println!(
            "\n{} service(s) are internal to the stack (expose = \"none\"): {}",
            internal.len(),
            internal.join(", ")
        );
    }
    println!("\n  magictree status --probe   magictree logs <service>   magictree down");
}

fn cmd_down(args: DownArgs, dry_run: bool) -> Result<()> {
    let ctx = Ctx::load(&resolve_cwd(args.cwd)?)?;
    let ordered = manifest::order(&ctx.nodes, &ctx.edges, &ctx.all_service_indices())?;
    if dry_run {
        println!("dry run — nothing is stopped");
        for &index in ordered.iter().rev() {
            let node = &ctx.nodes[index];
            if node.service().is_none() {
                continue;
            }
            if node.runtime() == Some(Runtime::Host) {
                if let Some(pid) = run::read_pid(&ctx.runtime_dir, &node.qual()) {
                    println!("would stop {} (pid {pid})", node.qual());
                }
            }
        }
        for (file, project) in ctx.compose_identities() {
            println!(
                "would run docker compose -f {} -p {project} down --remove-orphans{}",
                file.display(),
                if args.volumes { " --volumes" } else { "" }
            );
        }
        return Ok(());
    }
    let assignment = ports::load(&ctx.paths, &ctx.repo.key(), &ctx.repo.worktree_id())?;
    let env = match &assignment {
        Some(assignment) => ctx.build_env(None, assignment)?.vars,
        None => BTreeMap::new(),
    };

    let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
    for &index in ordered.iter().rev() {
        let node = &ctx.nodes[index];
        if node.runtime() != Some(Runtime::Host) {
            continue;
        }
        known.insert(run::sanitize(&node.qual()));
        if run::stop(
            &ctx.runtime_dir,
            &node.qual(),
            Duration::from_secs(ctx.config.stop_timeout_secs),
        )? {
            println!("{}: stopped", node.qual());
        }
    }

    // Reap processes whose service no longer exists in the manifest.
    for (name, running) in run::stop_all(
        &ctx.runtime_dir,
        Duration::from_secs(ctx.config.stop_timeout_secs),
    )? {
        if running && !known.contains(&name) {
            println!("{name}: stopped (stale)");
        }
    }

    for (file, project) in ctx.compose_identities() {
        let runner = ComposeRunner::new(file, None, project);
        runner.down(args.volumes, &env)?;
    }
    Ok(())
}

fn cmd_status(args: StatusArgs) -> Result<()> {
    let ctx = Ctx::load(&resolve_cwd(args.cwd)?)?;
    let assignment = ports::load(&ctx.paths, &ctx.repo.key(), &ctx.repo.worktree_id())?;
    let runners: HashMap<(PathBuf, String), ComposeRunner> = ctx
        .compose_identities()
        .into_iter()
        .map(|(file, project)| {
            let key = (file.clone(), project.clone());
            (key, ComposeRunner::new(file, None, project))
        })
        .collect();
    let mut states: HashMap<(PathBuf, String), Vec<ContainerState>> = HashMap::new();

    for &index in &ctx.all_service_indices() {
        let node = &ctx.nodes[index];
        let service = node.service().expect("service node");
        let port = assignment
            .as_ref()
            .and_then(|assignment| ctx.assigned_ports(node, assignment).first().cloned())
            .map(|(_, port)| port);
        let mut state = match node.runtime() {
            Some(Runtime::Compose) => {
                let key = ctx.runner_key(node).expect("compose runner key");
                if !states.contains_key(&key) {
                    let runner = runners
                        .get(&key)
                        .ok_or_else(|| anyhow!("{}: no compose runner", node.qual()))?;
                    states.insert(key.clone(), runner.ps(&BTreeMap::new())?);
                }
                let container = service
                    .compose
                    .as_ref()
                    .expect("validated compose reference")
                    .service
                    .clone();
                states[&key]
                    .iter()
                    .find(|entry| entry.service == container)
                    .map(|entry| match &entry.health {
                        Some(health) if !health.is_empty() => {
                            format!("{} ({})", entry.state, health)
                        }
                        _ => entry.state.clone(),
                    })
                    .unwrap_or_else(|| "not created".to_string())
            }
            _ => match run::read_pid(&ctx.runtime_dir, &node.qual()) {
                Some(pid) if run::is_alive(pid) => format!("running (pid {pid})"),
                _ => "stopped".to_string(),
            },
        };

        if args.probe {
            if let (Some(health_spec), Some(port)) = (&service.health, port) {
                let timeout = Duration::from_secs(2);
                let env = ctx.node_env(node, assignment.as_ref().unwrap_or(&EMPTY_ASSIGNMENT))?;
                let result = if let Some(path) = &health_spec.http {
                    health::wait_for_port(port, Some(path), timeout)
                } else if health_spec.tcp == Some(true) {
                    health::wait_for_port(port, None, timeout)
                } else if let Some(command) = &health_spec.command {
                    health::wait_for_command(command, &node.dir, &env, timeout)
                } else {
                    Ok(())
                };
                state = match result {
                    Ok(()) => format!("{state} healthy"),
                    Err(_) => format!("{state} unhealthy"),
                };
            }
        }

        let url = port
            .map(|port| format!("http://localhost:{port}"))
            .unwrap_or_default();
        println!(
            "{:<24} {:<8} {:<28} {}",
            node.qual(),
            match node.runtime() {
                Some(Runtime::Compose) => "compose",
                _ => "host",
            },
            state,
            url
        );
    }
    Ok(())
}

fn cmd_logs(args: LogsArgs) -> Result<()> {
    let ctx = Ctx::load(&resolve_cwd(args.cwd)?)?;
    let node = ctx
        .find_node(&args.service)
        .ok_or_else(|| anyhow!("unknown service '{}'", args.service))?;
    let path = run::log_file(&ctx.runtime_dir, &node.qual());
    if !path.exists() {
        bail!("no log file yet at {}", path.display());
    }
    let mut file = File::open(&path)?;
    let length = file.metadata()?.len();
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(args.lines);
    for line in &lines[start..] {
        println!("{line}");
    }
    if args.follow {
        follow(&path, length)?;
    }
    Ok(())
}

fn follow(path: &Path, mut offset: u64) -> Result<()> {
    loop {
        sleep(Duration::from_millis(300));
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        if length > offset {
            file.seek(SeekFrom::Start(offset))?;
            let mut buffer = String::new();
            file.read_to_string(&mut buffer)?;
            print!("{buffer}");
            std::io::stdout().flush()?;
            offset = length;
        }
    }
}

fn cmd_ports(args: PortsArgs, dry_run: bool) -> Result<()> {
    let ctx = Ctx::load(&resolve_cwd(args.cwd)?)?;
    if dry_run {
        println!("dry run — no assignment is read, claimed, or released");
        let requests = ctx.port_requests(&ctx.all_service_indices());
        if requests.is_empty() {
            println!("no service exposes a port");
        }
        for request in requests {
            println!(
                "{:<24} {}",
                request.name,
                match request.require {
                    Some(port) => format!("required {port}"),
                    None => match request.prefer {
                        Some(port) => format!("prefers {port}, otherwise allocated"),
                        None => "allocated on first real up".to_string(),
                    },
                }
            );
        }
        println!(
            "distinct blocks are assigned per worktree from {}..{} (stride {})",
            ctx.config.port_range_start, ctx.config.port_range_end, ctx.config.port_stride
        );
        return Ok(());
    }
    if args.reassign {
        match ports::reassign(&ctx.paths, &ctx.repo.key(), &ctx.repo.worktree_id())? {
            Some(path) => println!("released {}", path.display()),
            None => println!("no assignment to release"),
        }
    }
    let assignment = ports::ensure(
        &ctx.paths,
        &ctx.config,
        &ctx.repo.key(),
        &ctx.repo.worktree_id(),
        &ctx.repo.worktree_root,
        &ctx.port_requests(&ctx.all_service_indices()),
    )?;
    println!(
        "worktree {} (block {}-{})",
        ctx.slug,
        assignment.base,
        assignment.base + ctx.config.port_stride - 1
    );
    for (name, port) in &assignment.ports {
        println!("{name:<24} {port:<6} http://localhost:{port}");
    }
    Ok(())
}

fn cmd_env(args: EnvArgs, dry_run: bool) -> Result<()> {
    let ctx = Ctx::load(&resolve_cwd(args.cwd)?)?;
    let app = args.app.or_else(|| ctx.loaded.current_app.clone());
    let assignment = if dry_run {
        ctx.preview_assignment()
    } else {
        ports::ensure(
            &ctx.paths,
            &ctx.config,
            &ctx.repo.key(),
            &ctx.repo.worktree_id(),
            &ctx.repo.worktree_root,
            &ctx.port_requests(&ctx.all_service_indices()),
        )?
    };
    let plan = ctx.build_env(app.as_deref(), &assignment)?;
    let plan = if dry_run {
        ctx.with_placeholders(plan)
    } else {
        plan
    };
    if args.explain {
        print!("{}", plan.explain());
    } else if args.export {
        print!("{}", plan.export());
    } else {
        print!("{}", plan.dotenv());
    }
    Ok(())
}

fn wait_ready(
    ctx: &Ctx,
    node: &Node,
    env: &BTreeMap<String, String>,
    assignment: &ports::Assignment,
    compose: Option<(&ComposeRunner, &str)>,
) -> Result<()> {
    let service = node.service().expect("service node");
    let port = ctx
        .assigned_ports(node, assignment)
        .first()
        .map(|(_, port)| *port);
    let default_timeout = Duration::from_secs(ctx.config.health_timeout_secs);
    // Host services are supervised, so their death is observable and worth
    // failing on immediately. Compose services are handled by their own state.
    let probe = || match run::read_pid(&ctx.runtime_dir, &node.qual()) {
        Some(pid) => run::is_alive(pid),
        None => true,
    };

    if let Some(health_spec) = &service.health {
        let timeout = Duration::from_secs(health_spec.timeout_secs(ctx.config.health_timeout_secs));
        if let Some(path) = &health_spec.http {
            let port = port
                .ok_or_else(|| anyhow!("{}: health.http needs an allocated port", node.qual()))?;
            return health::wait_for_port_while(port, Some(path), timeout, probe);
        }
        if health_spec.tcp == Some(true) {
            let port =
                port.ok_or_else(|| anyhow!("{}: health.tcp needs an allocated port", node.qual()))?;
            return health::wait_for_port_while(port, None, timeout, probe);
        }
        if let Some(command) = &health_spec.command {
            return health::wait_for_command(command, &node.dir, env, timeout);
        }
    }

    match compose {
        Some((runner, container)) => wait_container(runner, container, env, default_timeout),
        None => Ok(()),
    }
}

static EMPTY_ASSIGNMENT: ports::Assignment = ports::Assignment {
    version: ports::ASSIGNMENT_VERSION,
    repo_key: String::new(),
    worktree_id: String::new(),
    worktree_path: None,
    base: 0,
    ports: BTreeMap::new(),
};

/// Timeout for a one-shot initialiser: generous, because it may run migrations
/// or provision an identity provider.
fn initializer_timeout(ctx: &Ctx) -> u64 {
    ctx.config.health_timeout_secs.max(600)
}

/// Wait for a one-shot container to run to completion, and require success.
fn wait_for_exit(
    runner: &ComposeRunner,
    container: &str,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let states = runner.ps(env)?;
        if let Some(entry) = states.iter().find(|entry| entry.service == container) {
            if entry.state.eq_ignore_ascii_case("exited") {
                return match entry.exit_code {
                    Some(0) | None => Ok(()),
                    Some(code) => bail!("'{container}' exited with status {code}"),
                };
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out after {}s waiting for '{container}' to finish",
                timeout.as_secs()
            );
        }
        sleep(Duration::from_millis(500));
    }
}

fn wait_container(
    runner: &ComposeRunner,
    container: &str,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let states = runner.ps(env)?;
        if let Some(entry) = states.iter().find(|entry| entry.service == container) {
            let running = entry.state.eq_ignore_ascii_case("running");
            let healthy = entry
                .health
                .as_deref()
                .map(|health| health.eq_ignore_ascii_case("healthy"))
                .unwrap_or(true);
            if running && healthy {
                return Ok(());
            }
            if entry.state.eq_ignore_ascii_case("exited") {
                // An initialiser that ran to completion has done its job, so it
                // is not a startup failure.
                if entry.exit_code == Some(0) {
                    return Ok(());
                }
                match entry.exit_code {
                    Some(code) => bail!("container exited with status {code} during startup"),
                    None => bail!("container exited during startup"),
                }
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out after {}s waiting for container '{container}'",
                timeout.as_secs()
            );
        }
        sleep(Duration::from_millis(250));
    }
}
