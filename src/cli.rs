use crate::banner;
use crate::bootstrap;
use crate::compose::{self, ComposeRunner, ContainerState};
use crate::config::{BuildMode, Config};
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
use crate::update;
use crate::worktrees;
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "magictree",
    version,
    about = "magictree - per-worktree development environments"
)]
pub struct Cli {
    /// Print what would happen and create nothing.
    #[arg(long, short = 'n', global = true)]
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
    /// Stop the named services and start them again on their assigned ports.
    Restart(RestartArgs),
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
    /// Run a command with this worktree's resolved environment.
    Exec(ExecArgs),
    /// Print a shell completion script.
    Completion(CompletionArgs),
    /// Install the latest release over this binary.
    Update(UpdateArgs),
}

#[derive(Args)]
pub struct CompletionArgs {
    /// Shell to generate completions for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Args)]
pub struct UpdateArgs {
    /// Say whether a newer release exists, and install nothing.
    #[arg(long, short = 'c')]
    pub check: bool,
    /// Install the latest release even when this binary is that version.
    #[arg(long, short = 'f')]
    pub force: bool,
}

#[derive(Args)]
pub struct DiscoverArgs {
    /// Write the report to this path instead of stdout.
    #[arg(long, short = 'r')]
    pub report: Option<PathBuf>,
    /// Emit JSON (the default when writing to a file).
    #[arg(long, short = 'j')]
    pub json: bool,
    /// Print the answer set implied by the report's defaults.
    #[arg(long, short = 'd')]
    pub default_answers: bool,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct InitArgs {
    /// Answers produced from a discovery report.
    #[arg(long, short = 'a', value_name = "FILE")]
    pub answers: Option<PathBuf>,
    /// Accept every default without prompting.
    #[arg(long, short = 'y', alias = "yes")]
    pub accept_defaults: bool,
    /// Regenerate existing manifests from discovery, discarding local edits.
    /// Without it, an existing manifest only gains the services it lacks.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Ask every question again, ignoring the answers the manifest records.
    #[arg(long, short = 'r')]
    pub reanswer: bool,
    /// Print the manifests without writing them.
    #[arg(long, short = 'p')]
    pub print: bool,
    /// Also write the answers used, so the run can be replayed.
    #[arg(long, short = 's', value_name = "FILE")]
    pub save_answers: Option<PathBuf>,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct DoctorArgs {
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct NewArgs {
    /// Branch to check out, creating it when it does not exist.
    pub branch: String,
    /// Base revision for a new branch (defaults to HEAD).
    #[arg(long, short = 'b')]
    pub base: Option<String>,
    /// Explicit checkout directory.
    #[arg(long, short = 'p')]
    pub path: Option<PathBuf>,
    /// Create a detached worktree at the base revision.
    #[arg(long, short = 'd')]
    pub detach: bool,
    /// Create the worktree without starting its stack.
    #[arg(long)]
    pub no_up: bool,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct RmArgs {
    /// Worktree path, or a branch name to resolve.
    pub target: String,
    /// Discard uncommitted changes in the worktree.
    #[arg(long, short = 'f')]
    pub force: bool,
    /// Skip `down` before removing.
    #[arg(long)]
    pub no_down: bool,
    /// Also remove compose volumes.
    #[arg(long, short = 'v')]
    pub volumes: bool,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct GcArgs {
    /// Also run `git worktree prune` for removed checkouts.
    #[arg(long, short = 'p')]
    pub prune: bool,
    /// Sweep every repository the state dir holds a record of, including ones
    /// whose repository is itself gone.
    #[arg(long, short = 'A', conflicts_with_all = ["prune", "cwd"])]
    pub all: bool,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct UpArgs {
    /// Services to start (id or app:id). Defaults to the current app, or
    /// everything at the workspace root.
    pub services: Vec<String>,
    /// Restrict to whole apps.
    #[arg(long = "app", short = 'a', value_name = "APP")]
    pub apps: Vec<String>,
    /// Start every service in the repository.
    #[arg(long, short = 'A')]
    pub all: bool,
    /// Build the compose images before starting them, whatever `build` in
    /// config.toml says.
    #[arg(long, short = 'b', overrides_with = "no_build")]
    pub build: bool,
    /// Skip building the images of the compose services being started,
    /// whatever `build` in config.toml says. Without either flag, `up` builds
    /// them: that is how a changed Dockerfile or build context reaches the
    /// stack, and Compose validates its cache, so an unchanged context costs a
    /// cache check rather than a rebuild.
    #[arg(long, overrides_with = "build")]
    pub no_build: bool,
    /// Which ports to use: `declared` takes each service's `prefer`, which is
    /// what the primary checkout's own tooling and generated files expect, so
    /// it is the default there; `generated` allocates every port from this
    /// worktree's block, keeping the stack off the repository's ports. Changing
    /// the mode re-allocates the worktree's ports.
    #[arg(long, short = 'p', value_name = "MODE", value_enum)]
    pub ports: Option<ports::PortMode>,
    /// Directory to resolve the repository from.
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct DownArgs {
    /// Also remove named volumes (destroys per-worktree database state).
    #[arg(long, short = 'v')]
    pub volumes: bool,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct RestartArgs {
    /// Services to restart (id or app:id). Dependencies are left alone; each
    /// service keeps its assigned port and environment.
    #[arg(required = true, value_name = "SERVICE")]
    pub services: Vec<String>,
    /// Build the compose images before starting them, whatever `build` in
    /// config.toml says.
    #[arg(long, short = 'b', overrides_with = "no_build")]
    pub build: bool,
    /// Skip building the images of the compose services being restarted,
    /// whatever `build` in config.toml says.
    #[arg(long, overrides_with = "build")]
    pub no_build: bool,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Also run each service's health probe (may take a few seconds).
    #[arg(long, short = 'p')]
    pub probe: bool,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct LogsArgs {
    pub service: String,
    #[arg(short, long)]
    pub follow: bool,
    #[arg(long, short = 'l', default_value_t = 40)]
    pub lines: usize,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct PortsArgs {
    /// Discard the current assignment and allocate fresh ports in the mode this
    /// worktree recorded.
    #[arg(long, short = 'r', conflicts_with = "release")]
    pub reassign: bool,
    /// Drop the assignment and allocate nothing: this worktree's ports stop
    /// being reserved, and the next `up` allocates from scratch.
    #[arg(long, short = 'R')]
    pub release: bool,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

#[derive(Args)]
pub struct ExecArgs {
    /// Run with a specific app's environment instead of the current app's.
    #[arg(long, short = 'a')]
    pub app: Option<String>,
    /// Directory to resolve the repository from; the command runs there.
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
    /// Command and arguments, taken verbatim. A command that starts with a
    /// flag needs `--` before it.
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct EnvArgs {
    /// Show the merged environment of a specific app.
    #[arg(long, short = 'a')]
    pub app: Option<String>,
    /// Emit `export KEY=value` lines.
    #[arg(long, short = 'e')]
    pub export: bool,
    /// Show which layer set each value.
    #[arg(long)]
    pub explain: bool,
    #[arg(long, short = 'C')]
    pub cwd: Option<PathBuf>,
}

/// The command line as it is parsed: the derived `Cli::command` with the banner
/// printed above its help.
pub fn command() -> clap::Command {
    Cli::command().before_help(banner::ART)
}

/// Parse the command line, printing help or the error the way `Cli::parse` does.
pub fn parse() -> Cli {
    match command().try_get_matches() {
        Ok(matches) => Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit()),
        Err(err) => err.exit(),
    }
}

pub fn dispatch(cli: Cli) -> Result<()> {
    let dry_run = cli.dry_run;
    // `update` is about the binary itself, a failed command has said enough of
    // its own, and a dry run writes nothing — not even the check's cache.
    let announce = !dry_run && !matches!(cli.command, Command::Update(_));
    let outcome = match cli.command {
        Command::Discover(args) => cmd_discover(args, dry_run),
        Command::Init(args) => cmd_init(args, dry_run),
        Command::Doctor(args) => cmd_doctor(args),
        Command::New(args) => cmd_new(args, dry_run),
        Command::Up(args) => cmd_up(args, dry_run),
        Command::Down(args) => cmd_down(args, dry_run),
        Command::Restart(args) => cmd_restart(args, dry_run),
        Command::Rm(args) => cmd_rm(args, dry_run),
        Command::List(args) => cmd_list(args),
        Command::Gc(args) => cmd_gc(args, dry_run),
        Command::Status(args) => cmd_status(args),
        Command::Logs(args) => cmd_logs(args),
        Command::Ports(args) => cmd_ports(args, dry_run),
        Command::Env(args) => cmd_env(args, dry_run),
        Command::Exec(args) => cmd_exec(args, dry_run),
        Command::Completion(args) => cmd_completion(args),
        Command::Update(args) => cmd_update(args, dry_run),
    };
    if announce && outcome.is_ok() {
        update::notice();
    }
    outcome
}

/// Print a completion script to stdout. Install it with, for example:
///   magictree completion zsh > ~/.zsh/completions/_magictree
/// zsh loads it from a directory that was on `$fpath` when `compinit` ran; see the README.
fn cmd_completion(args: CompletionArgs) -> Result<()> {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(args.shell, &mut command, name, &mut std::io::stdout());
    Ok(())
}

/// Replace this binary with the release the repository publishes for it.
///
/// The download is staged beside the binary and moved into place only once it
/// has proved that it runs, so a failed or interrupted update is a no-op.
fn cmd_update(args: UpdateArgs, dry_run: bool) -> Result<()> {
    let release = update::latest()?;
    let (current, latest) = (update::VERSION, release.version());
    let stale = update::is_newer(latest, current);
    if args.check {
        if stale {
            println!("magictree {latest} is available (this is {current})");
        } else {
            println!("magictree {current} is the latest release");
        }
    } else if !stale && !args.force {
        println!("magictree {current} is the latest release");
    } else {
        let artifact = release.artifact()?;
        if dry_run {
            println!(
                "dry run: nothing is downloaded or written\n\nwould install {artifact} over {}",
                update::running_binary()?.display()
            );
            return Ok(());
        }
        println!("magictree {current} -> {latest}");
        println!("downloading {artifact}");
        let installed = update::install(&release)?;
        println!("installed to {}", installed.display());
    }
    // The answer stands for a day, and the user has just seen it.
    if !dry_run {
        update::remember(latest);
    }
    Ok(())
}

fn cmd_discover(args: DiscoverArgs, dry_run: bool) -> Result<()> {
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
            if dry_run {
                println!(
                    "dry run: would write {} ({} facts, {} unknowns)",
                    path.display(),
                    report.facts.len(),
                    report.unknowns.len()
                );
            } else {
                report.write(&path)?;
                println!(
                    "wrote {} ({} facts, {} unknowns)",
                    path.display(),
                    report.facts.len(),
                    report.unknowns.len()
                );
            }
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
/// With no manifest there is nothing to point at yet, so the answer is generic,
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
    // What an earlier run recorded in the manifest, so the questions it already
    // answered are not asked again. An explicit answers file is used as given.
    let manifest = root.join(manifest::MANIFEST_FILE);
    let recorded = if args.reanswer || args.answers.is_some() {
        init::Recorded::default()
    } else {
        init::recorded(&root, &report)?
    };
    for (id, answer) in &recorded.dropped {
        eprintln!(
            "warning: asking again about '{id}': the recorded answer '{}' is no longer offered",
            answer.display()
        );
    }
    for (id, options) in &recorded.reopened {
        eprintln!(
            "warning: {id} now offers '{}', which its recorded answer never decided\n\
             run `magictree init` to be asked about it, or `magictree init --reanswer`",
            options.join("', '")
        );
    }
    // A question is decided against the options on offer when it is answered, so
    // only the ones this run answered count as decided: a replayed answer keeps
    // what it turned down before, which is what leaves a newly offered option
    // open for the next run to ask about.
    let (answers, asked) = match &args.answers {
        Some(path) => {
            let from_file = discover::AnswerSet::read_json(path)?;
            let asked = from_file.answers.keys().cloned().collect();
            (from_file, asked)
        }
        None if args.accept_defaults => {
            let mut answers = init::default_answers(&report);
            for (id, answer) in &recorded.answers {
                answers.answers.insert(id.clone(), answer.clone());
            }
            let asked = answers
                .answers
                .keys()
                .filter(|id| !recorded.answers.contains_key(*id))
                .cloned()
                .collect();
            if !recorded.answers.is_empty() {
                println!(
                    "keeping {} answer(s) recorded in {}",
                    recorded.answers.len(),
                    manifest.display()
                );
            }
            (answers, asked)
        }
        None => {
            let session = init::wizard::run(&report, &recorded, &manifest.display().to_string())?;
            (session.answers, session.asked)
        }
    };
    let record = init::record(&report, &answers, &recorded, &asked);
    let planned = init::plan(&report, &answers, &record, &root)?;
    for warning in init::warnings(&report, &answers) {
        eprintln!("warning: {warning}");
    }
    if dry_run || args.print {
        println!("dry run: no manifest is written\n");
        for (outcome, contents) in init::preview(&planned, args.force)? {
            if matches!(outcome, init::Applied::Unchanged(_)) {
                println!("unchanged {}", outcome.path().display());
                continue;
            }
            println!("# {}", outcome.path().display());
            print!("{contents}");
            println!();
        }
        return Ok(());
    }
    if let Some(path) = &args.save_answers {
        answers.write(path)?;
    }
    for applied in init::apply(&planned, args.force)? {
        match applied {
            init::Applied::Created(path) => println!("wrote {}", path.display()),
            init::Applied::Updated {
                path,
                added,
                recorded,
            } => {
                let mut changes: Vec<String> = Vec::new();
                if !added.is_empty() {
                    changes.push(format!("added service '{}'", added.join("', '")));
                }
                if recorded.is_some() {
                    changes.push("recorded the answers".to_string());
                }
                println!("updated {}: {}", path.display(), changes.join(", "));
                // A changed answer that also added a service did take effect, so
                // the note that says an existing service was left alone would
                // only confuse.
                if let Some(changed) =
                    recorded.filter(|changed| !changed.is_empty() && added.is_empty())
                {
                    eprintln!(
                        "warning: the answer for '{}' changed; the services already in {} keep what they declare\n\
                         re-run with `magictree init --force` to rewrite them",
                        changed.join("', '"),
                        path.display()
                    );
                }
            }
            init::Applied::Unchanged(path) => {
                println!(
                    "unchanged {}: every service is already declared",
                    path.display()
                )
            }
        }
    }
    Ok(())
}

fn cmd_doctor(args: DoctorArgs) -> Result<()> {
    let root = resolve_cwd(args.cwd)?;
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
        println!("dry run: no worktree is created");
        let planned = worktrees::add_args(
            &repo,
            &args.branch,
            args.base.as_deref(),
            &path,
            args.detach,
        );
        println!("would run git {}", planned.join(" "));
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
    let mode = ctx.config.build;
    ensure(&mut ctx, &selection, None, mode).map(|_| ())
}

fn cmd_rm(args: RmArgs, dry_run: bool) -> Result<()> {
    let start = resolve_cwd(args.cwd)?;
    let repo = Repo::open(&start)?;
    let target = resolve_target(&repo, &args.target)?;
    if dry_run {
        println!("dry run: nothing is stopped or removed");
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
        if !target.exists() {
            bail!(
                "worktree checkout {} is already gone\n\nrun `magictree gc --prune` to release its \
                 port block and drop its recorded processes",
                target.display()
            );
        }
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
        println!("dry run: nothing is released");
    }
    let timeout = Duration::from_secs(config.stop_timeout_secs);
    if args.all {
        return worktrees::gc_all(&paths, timeout, apply);
    }
    let repo = match Repo::open_optional(&resolve_cwd(args.cwd)?)? {
        Some(repo) => repo,
        // Sweeping the whole state dir from here would be a scope the user
        // never asked for, so name the flag that asks for it.
        None => bail!(
            "no git repository here\n\n`magictree gc --all` sweeps every repository the state dir \
             knows about, including one whose checkout is gone"
        ),
    };
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
    let mode = build_mode(ctx.config.build, args.build, args.no_build);
    if dry_run {
        return dryrun::up(&ctx, &selection, args.ports, mode);
    }
    ensure(&mut ctx, &selection, args.ports, mode).map(|_| ())
}

/// The build decision for this run. The CLI wins over the configured mode, so
/// `--build` forces a build and `--no-build` skips one whatever `config.toml`
/// says; with neither flag the configured `build` applies.
fn build_mode(configured: BuildMode, force: bool, skip: bool) -> BuildMode {
    if force {
        BuildMode::Always
    } else if skip {
        BuildMode::Never
    } else {
        configured
    }
}

/// Whether this run builds compose images, asking once when the mode says so.
/// A selection with nothing to build — every service names an image — never
/// asks, so `build = "ask"` stays quiet in a stack that only pulls.
fn compose_build(
    ctx: &Ctx,
    selection: &[usize],
    assignment: &ports::Assignment,
    runners: &HashMap<(PathBuf, String), ComposeRunner>,
    mode: BuildMode,
) -> Result<bool> {
    match mode {
        BuildMode::Always => Ok(true),
        BuildMode::Never => Ok(false),
        BuildMode::Ask => {
            let buildable = buildable_services(ctx, selection, assignment, runners)?;
            if buildable.is_empty() {
                return Ok(false);
            }
            Ok(bootstrap::confirm(&format!(
                "Build images for {}",
                buildable.join(", ")
            )))
        }
    }
}

/// The selected services that build an image, named the way the user addresses
/// them. Resolving each compose file's configuration is what separates a
/// service that builds from one that only names an image.
fn buildable_services(
    ctx: &Ctx,
    selection: &[usize],
    assignment: &ports::Assignment,
    runners: &HashMap<(PathBuf, String), ComposeRunner>,
) -> Result<Vec<String>> {
    let mut resolved: HashMap<(PathBuf, String), BTreeSet<String>> = HashMap::new();
    let mut names: Vec<String> = Vec::new();
    for &index in selection {
        let node = &ctx.nodes[index];
        if node.runtime() != Some(Runtime::Compose) {
            continue;
        }
        let key = ctx
            .runner_key(node)
            .ok_or_else(|| anyhow!("{}: missing compose reference", node.qual()))?;
        let container = node
            .service()
            .and_then(|service| service.compose.as_ref())
            .expect("validated compose reference")
            .service
            .clone();
        if !resolved.contains_key(&key) {
            let runner = runners
                .get(&key)
                .ok_or_else(|| anyhow!("{}: no compose runner", node.qual()))?;
            let env = ctx.node_env(node, assignment)?;
            resolved.insert(key.clone(), runner.services_with_build(&env)?);
        }
        if resolved[&key].contains(&container) {
            names.push(node.qual());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Consecutive Compose services sharing a runner and environment. Compose's
/// `depends_on` graph owns their internal start order; magictree still checks
/// each service's declared wait condition before proceeding to host services.
fn compose_group_end(
    ctx: &Ctx,
    selection: &[usize],
    start: usize,
    assignment: &ports::Assignment,
    env: &BTreeMap<String, String>,
) -> Result<usize> {
    let first = &ctx.nodes[selection[start]];
    let key = ctx
        .runner_key(first)
        .ok_or_else(|| anyhow!("{}: missing compose reference", first.qual()))?;
    let mut end = start + 1;
    while end < selection.len() {
        let index = selection[end];
        let candidate = &ctx.nodes[index];
        if candidate.service().is_none()
            || candidate.runtime() != Some(Runtime::Compose)
            || ctx.runner_key(candidate).as_ref() != Some(&key)
            || ctx.node_env(candidate, assignment)? != *env
        {
            break;
        }
        end += 1;
    }
    Ok(end)
}

/// The idempotent core: allocate ports, materialise env, bootstrap, then start
/// services in dependency order, waiting for each to become healthy, and finish
/// with the manifest's `after` steps.
fn ensure(
    ctx: &mut Ctx,
    selection: &[usize],
    requested: Option<ports::PortMode>,
    mode: BuildMode,
) -> Result<ports::Assignment> {
    // Every service gets an assignment, not just the selected ones: a later
    // partial `up` still has to write an override for its dependencies, and a
    // stable worktree-wide assignment is easier to reason about.
    let all_services = ctx.all_service_indices();
    // A mode change re-allocates, which cannot happen under a running stack:
    // containers keep publishing the ports they were created with while
    // `ports`, `env` and the health probes move on to the new ones.
    if let Some(mode) = requested {
        let current = ports::load(&ctx.paths, &ctx.repo.key(), &ctx.repo.worktree_id())?;
        if ports::effective_mode(current.as_ref(), ctx.repo.is_main_worktree()) != mode {
            if let Some(service) = running_service(ctx)? {
                bail!(
                    "'{service}' is running; run `magictree down` before changing this worktree's \
                     ports"
                );
            }
        }
    }
    let assignment = ports::ensure(
        &ctx.paths,
        &ctx.config,
        &ctx.repo.key(),
        &ctx.repo.worktree_id(),
        &ctx.repo.worktree_root,
        &ctx.port_requests(&all_services),
        requested,
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
        for message in bootstrap::sync_files(&ctx.repo, &ctx.repo.worktree_root, &dir, &steps.sync)?
        {
            println!("{message}");
        }
        if !steps.run.is_empty() {
            let env = ctx.build_env(app.as_deref(), &assignment)?;
            for message in bootstrap::run_steps(
                &ctx.runtime_dir,
                &dir,
                "bootstrap",
                &steps.run,
                &env.vars,
                &bootstrap::prompt,
            )? {
                println!("{message}");
            }
        }
    }

    let runners = ctx.compose_runners(selection, &assignment)?;
    if !runners.is_empty() {
        compose::ensure_docker()?;
    }
    let build = compose_build(ctx, selection, &assignment, &runners, mode)?;

    let mut position = 0;
    while position < selection.len() {
        let index = selection[position];
        let node = &ctx.nodes[index];
        let env = ctx.node_env(node, &assignment)?;
        let mut consumed = 1;
        let mut failure_node_index = index;
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
            NodeKind::Service(_) => match node.runtime() {
                Some(Runtime::Compose) => {
                    let key = ctx
                        .runner_key(node)
                        .ok_or_else(|| anyhow!("{}: missing compose reference", node.qual()))?;
                    let runner = runners
                        .get(&key)
                        .ok_or_else(|| anyhow!("{}: no compose runner", node.qual()))?;
                    let batch_end = compose_group_end(ctx, selection, position, &assignment, &env)?;
                    consumed = batch_end - position;
                    let mut args = Vec::with_capacity(3 + consumed);
                    args.extend(["up", "-d"]);
                    if build {
                        args.push("--build");
                    }
                    let services_start = args.len();
                    for &batch_index in &selection[position..batch_end] {
                        let batch_node = &ctx.nodes[batch_index];
                        let container = batch_node
                            .service()
                            .and_then(|service| service.compose.as_ref())
                            .expect("validated compose reference")
                            .service
                            .as_str();
                        println!("{}: starting container", batch_node.qual());
                        args.push(container);
                    }
                    let started = if consumed == 1 {
                        runner.up_service(args[services_start], build, &env)
                    } else {
                        runner.run(&args, &env).map(|_| ())
                    };
                    if let Err(error) = started {
                        if consumed > 1 {
                            let services = args[services_start..].join(", ");
                            bail!(
                                "compose services [{services}] failed to start: {error}\n  compose file {}\n  project {}\n  logs     docker compose -f {} -p {} logs {}",
                                runner.file.display(),
                                runner.project,
                                runner.file.display(),
                                runner.project,
                                args[services_start..].join(" ")
                            );
                        }
                        Err((Some(runner), error.to_string()))
                    } else {
                        let ready = (|| -> Result<()> {
                            for &batch_index in &selection[position..batch_end] {
                                failure_node_index = batch_index;
                                let batch_node = &ctx.nodes[batch_index];
                                let batch_service =
                                    batch_node.service().expect("compose service node");
                                let container = batch_service
                                    .compose
                                    .as_ref()
                                    .expect("validated compose reference")
                                    .service
                                    .as_str();
                                match batch_service.wait {
                                    // Compose owns dependency scheduling; magictree
                                    // still verifies one-shots before host dependants.
                                    Wait::Exit => wait_for_exit(
                                        runner,
                                        container,
                                        &env,
                                        Duration::from_secs(initializer_timeout(ctx)),
                                    ),
                                    Wait::Running => wait_ready(
                                        ctx,
                                        batch_node,
                                        &env,
                                        &assignment,
                                        Some((runner, container)),
                                    ),
                                }?;
                            }
                            Ok(())
                        })();
                        match ready {
                            Ok(()) => Ok(Some(runner)),
                            Err(error) => Err((Some(runner), error.to_string())),
                        }
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
            let report =
                ctx.describe_failure(&ctx.nodes[failure_node_index], &assignment, runner, &reason);
            return Err(anyhow!(report));
        }
        position += consumed;
    }

    print_summary(ctx, selection, &assignment);

    // `after` steps run once every selected service is healthy and every `up`
    // job has finished. They come last so a failure still leaves the URLs on
    // screen: the stack is up, and the failing script is the only thing wrong.
    for (dir, app, steps) in ctx.bootstrap_targets(selection) {
        if steps.after.is_empty() {
            continue;
        }
        let env = ctx.build_env(app.as_deref(), &assignment)?;
        for message in bootstrap::run_steps(
            &ctx.runtime_dir,
            &dir,
            "after",
            &steps.after,
            &env.vars,
            &bootstrap::prompt,
        )? {
            println!("{message}");
        }
    }

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
        println!("dry run: nothing is stopped");
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

/// Resolve the names given to `restart` into dependency-ordered indices.
/// Nothing is pulled in: restart touches exactly the services named.
fn restart_selection(ctx: &Ctx, names: &[String]) -> Result<Vec<usize>> {
    let mut chosen = std::collections::BTreeSet::new();
    for name in names {
        let index = ctx
            .nodes
            .iter()
            .position(|node| node.id == *name || node.qual() == *name)
            .ok_or_else(|| anyhow!("unknown service '{name}'"))?;
        if ctx.nodes[index].job().is_some() {
            bail!("'{name}' is a job, not a service; only services can be restarted");
        }
        chosen.insert(index);
    }
    let selection: Vec<usize> = chosen.into_iter().collect();
    manifest::order_selected(&ctx.nodes, &ctx.edges, &selection)
}

/// Stop the named services — the host process, or the service's containers —
/// and start them again on the ports and with the environment the worktree
/// already has. Dependencies are not started and nothing else is touched, so a
/// restart is safe while the rest of the stack keeps running; each service is
/// waited to health in dependency order.
fn cmd_restart(args: RestartArgs, dry_run: bool) -> Result<()> {
    let mut ctx = Ctx::load(&resolve_cwd(args.cwd)?)?;
    let selection = restart_selection(&ctx, &args.services)?;
    let mode = build_mode(ctx.config.build, args.build, args.no_build);
    if dry_run {
        println!("dry run: nothing is restarted");
        for &index in &selection {
            let node = &ctx.nodes[index];
            match node.runtime() {
                Some(Runtime::Compose) => {
                    let key = ctx
                        .runner_key(node)
                        .ok_or_else(|| anyhow!("{}: missing compose reference", node.qual()))?;
                    let container = node
                        .service()
                        .and_then(|service| service.compose.as_ref())
                        .expect("validated compose reference")
                        .service
                        .clone();
                    println!(
                        "{}: would run docker compose -f {} -p {} stop {container}, then up -d{} {container}",
                        node.qual(),
                        key.0.display(),
                        key.1,
                        dryrun::build_note(mode)
                    );
                }
                _ => {
                    let pid = run::read_pid(&ctx.runtime_dir, &node.qual());
                    let stopping = match pid {
                        Some(pid) if run::is_alive(pid) => format!("stop (pid {pid}), then "),
                        _ => String::new(),
                    };
                    println!("{}: would {}start again", node.qual(), stopping);
                }
            }
        }
        return Ok(());
    }
    // A restart keeps the worktree's assignment: ports and environment stay
    // exactly as the stack had them, so nothing moves under its peers.
    let assignment = ports::load(&ctx.paths, &ctx.repo.key(), &ctx.repo.worktree_id())?
        .ok_or_else(|| anyhow!("no port assignment for this worktree; run `magictree up` first"))?;
    ctx.adopt_legacy_runtime()?;
    ctx.ensure_runtime_dirs()?;
    let runners = ctx.compose_runners(&selection, &assignment)?;
    if !runners.is_empty() {
        compose::ensure_docker()?;
    }
    let build = compose_build(&ctx, &selection, &assignment, &runners, mode)?;

    for &index in &selection {
        let node = &ctx.nodes[index];
        let env = ctx.node_env(node, &assignment)?;
        let outcome = match node.runtime() {
            Some(Runtime::Compose) => {
                let key = ctx
                    .runner_key(node)
                    .ok_or_else(|| anyhow!("{}: missing compose reference", node.qual()))?;
                let runner = runners
                    .get(&key)
                    .ok_or_else(|| anyhow!("{}: no compose runner", node.qual()))?;
                let service = node.service().expect("service node");
                let container = service
                    .compose
                    .as_ref()
                    .expect("validated compose reference")
                    .service
                    .clone();
                println!("{}: restarting container", node.qual());
                let started: Result<()> = (|| {
                    // Only stop what exists: a service that was never started
                    // (or was removed by `down`) just starts.
                    let states = runner.ps(&env)?;
                    if states.iter().any(|state| state.service == container) {
                        runner.stop_service(&container, &env)?;
                    }
                    println!("{}: starting container", node.qual());
                    runner.up_service(&container, build, &env)?;
                    match service.wait {
                        Wait::Exit => wait_for_exit(
                            runner,
                            &container,
                            &env,
                            Duration::from_secs(initializer_timeout(&ctx)),
                        ),
                        Wait::Running => {
                            wait_ready(&ctx, node, &env, &assignment, Some((runner, &container)))
                        }
                    }
                })();
                match started {
                    Ok(()) => Ok(Some(runner)),
                    Err(error) => Err((Some(runner), error.to_string())),
                }
            }
            _ => {
                let started: Result<()> = (|| {
                    if run::stop(
                        &ctx.runtime_dir,
                        &node.qual(),
                        Duration::from_secs(ctx.config.stop_timeout_secs),
                    )? {
                        println!("{}: stopped", node.qual());
                    } else {
                        println!("{}: not running", node.qual());
                    }
                    let command = node
                        .command()
                        .ok_or_else(|| anyhow!("{}: no command or target", node.qual()))?;
                    let pid =
                        run::start(&ctx.runtime_dir, &node.qual(), &command, &node.dir, &env)?;
                    println!("{}: started (pid {pid})", node.qual());
                    wait_ready(&ctx, node, &env, &assignment, None)
                })();
                match started {
                    Ok(()) => Ok(None),
                    Err(error) => Err((None, error.to_string())),
                }
            }
        };

        if let Err((runner, reason)) = outcome {
            let report = ctx.describe_failure(node, &assignment, runner, &reason);
            return Err(anyhow!(report));
        }
    }

    print_summary(&ctx, &selection, &assignment);
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
        if length < offset {
            // Truncated or rotated (`up` truncates the log on every start):
            // restart from the top instead of waiting for the file to regrow.
            offset = 0;
        }
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

/// The first service of this worktree that is still up: a live pid file for a
/// host process, or a container in the worktree's own compose project that has
/// not exited. Enough to refuse a change that would move ports out from under a
/// running stack, and to name what has to stop first.
fn running_service(ctx: &Ctx) -> Result<Option<String>> {
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
        match node.runtime() {
            Some(Runtime::Compose) => {
                let service = node.service().expect("service node");
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
                let up = states[&key]
                    .iter()
                    .find(|entry| entry.service == container)
                    .is_some_and(|entry| {
                        !matches!(entry.state.as_str(), "exited" | "dead" | "created" | "")
                    });
                if up {
                    return Ok(Some(node.qual()));
                }
            }
            _ => {
                if run::read_pid(&ctx.runtime_dir, &node.qual())
                    .map(run::is_alive)
                    .unwrap_or(false)
                {
                    return Ok(Some(node.qual()));
                }
            }
        }
    }
    Ok(None)
}

fn cmd_ports(args: PortsArgs, dry_run: bool) -> Result<()> {
    let ctx = Ctx::load(&resolve_cwd(args.cwd)?)?;
    if dry_run {
        println!("dry run: no assignment is read, claimed, or released");
        let mode = if ctx.repo.is_main_worktree() {
            ports::PortMode::Declared
        } else {
            ports::PortMode::Block
        };
        println!(
            "worktree {} takes {} ports{}",
            ctx.slug,
            mode.describe(),
            match mode {
                ports::PortMode::Declared =>
                    "; `up --ports generated` allocates from its block instead",
                ports::PortMode::Block => ": a linked worktree never takes a declared port",
            }
        );
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
                    None => match (request.prefer, mode) {
                        (Some(port), ports::PortMode::Declared) => format!("declared {port}"),
                        (Some(port), ports::PortMode::Block) =>
                            format!("declares {port}, allocated from the block"),
                        (None, _) => "allocated from the block".to_string(),
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
    if args.release {
        // Releasing under a running stack has the same effect as changing the
        // mode: the next read re-allocates and reports ports nothing is using.
        if let Some(service) = running_service(&ctx)? {
            bail!(
                "'{service}' is running; run `magictree down` before releasing this worktree's \
                 ports"
            );
        }
        return match ports::release(&ctx.paths, &ctx.repo.key(), &ctx.repo.worktree_id())? {
            Some(path) => {
                println!("released {}", path.display());
                println!(
                    "these ports are no longer reserved; the next command that needs them \
                     allocates again"
                );
                Ok(())
            }
            None => {
                println!("no assignment to release");
                Ok(())
            }
        };
    }
    if args.reassign {
        match ports::release(&ctx.paths, &ctx.repo.key(), &ctx.repo.worktree_id())? {
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
        None,
    )?;
    let block = format!(
        "{}-{}",
        assignment.base,
        assignment.base.saturating_add(ctx.config.port_stride - 1)
    );
    let in_block = assignment.ports.values().any(|port| {
        *port >= assignment.base && *port < assignment.base.saturating_add(ctx.config.port_stride)
    });
    println!(
        "worktree {} ({})",
        ctx.slug,
        match (assignment.mode, in_block) {
            (ports::PortMode::Block, _) => format!("generated ports, block {block}"),
            (ports::PortMode::Declared, true) => format!("declared ports plus block {block}"),
            (ports::PortMode::Declared, false) => "declared ports".to_string(),
        }
    );
    for (name, port) in &assignment.ports {
        println!("{name:<24} {port:<6} http://localhost:{port}");
    }
    Ok(())
}

fn cmd_env(args: EnvArgs, dry_run: bool) -> Result<()> {
    let ctx = Ctx::load(&resolve_cwd(args.cwd)?)?;
    let app = args.app.or_else(|| ctx.loaded.current_app.clone());
    let plan = if dry_run {
        ctx.preview_plan(app.as_deref(), None)?
    } else {
        let assignment = ports::ensure(
            &ctx.paths,
            &ctx.config,
            &ctx.repo.key(),
            &ctx.repo.worktree_id(),
            &ctx.repo.worktree_root,
            &ctx.port_requests(&ctx.all_service_indices()),
            None,
        )?;
        ctx.build_env(app.as_deref(), &assignment)?
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

/// Run a command with the environment magictree would give a host service: the
/// resolved ports and layered `[env]`, on top of the caller's own environment,
/// in the directory magictree was pointed at. The child owns the terminal, and
/// its exit status becomes magictree's, so a recipe can delegate to it.
fn cmd_exec(args: ExecArgs, dry_run: bool) -> Result<()> {
    let cwd = resolve_cwd(args.cwd)?;
    let ctx = Ctx::load(&cwd)?;
    let app = args.app.or_else(|| ctx.loaded.current_app.clone());
    if dry_run {
        println!("would run: {}", shell_join(&args.command));
        print!("{}", ctx.preview_plan(app.as_deref(), None)?.dotenv());
        return Ok(());
    }
    let assignment = ports::ensure(
        &ctx.paths,
        &ctx.config,
        &ctx.repo.key(),
        &ctx.repo.worktree_id(),
        &ctx.repo.worktree_root,
        &ctx.port_requests(&ctx.all_service_indices()),
        None,
    )?;
    let plan = ctx.build_env(app.as_deref(), &assignment)?;

    let (program, rest) = args
        .command
        .split_first()
        .expect("clap rejects an empty command");
    let status = std::process::Command::new(program)
        .args(rest)
        .current_dir(&cwd)
        .envs(&plan.vars)
        .status()
        .with_context(|| format!("running '{program}'"))?;
    if !status.success() {
        std::process::exit(exit_code(status));
    }
    Ok(())
}

/// A child's status as an exit code: its own, or 128+signal when a signal
/// killed it (the shell convention), so a caller can see why it stopped.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// A command rendered the way a shell would take it back, for a dry run to
/// show exactly what it would have run.
fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || "-_./:@,+=".contains(ch))
            {
                return arg.clone();
            }
            format!("'{}'", arg.replace('\'', "'\\''"))
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    mode: ports::PortMode::Declared,
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
                    Some(0) => Ok(()),
                    Some(code) => bail!("'{container}' exited with status {code}"),
                    // Fail closed: a migration whose exit code is unknown is
                    // not a success we can vouch for.
                    None => bail!(
                        "'{container}' exited but docker compose did not report its exit code; \
                         inspect it with `docker compose logs {container}`"
                    ),
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
