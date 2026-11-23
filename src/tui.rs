//! The TUI a bare `magictree` opens: a worktree's stack on one screen.
//!
//! The screen is a row of sections ([`SECTIONS`]): the services of one
//! worktree, every worktree the state dir records, the worktree's ports and
//! environment, and one service's log. A digit or `tab` switches between them,
//! and so does `:` followed by a section's name. The prompt only navigates;
//! what acts on the stack (`u` up, `r` restart, `d` down, `l` logs) is a key.
//! The screen only reads: anything that changes a stack runs the same
//! `magictree` command a shell would, as a child process whose output lands in
//! a pane, so the TUI can never do something the command line cannot.
//!
//! [`App`] is the whole state and knows nothing about threads or processes: a
//! key or a message goes in, an [`Effect`] comes out, and [`Driver`] carries
//! the effect out. That keeps every decision testable without a terminal.

use crate::banner;
use crate::cli;
use crate::ctx::Ctx;
use crate::manifest::Runtime;
use crate::paths::Paths;
use crate::ports;
use crate::repo::{Repo, PRIMARY_WORKTREE_ID};
use crate::run;
use crate::worktrees::{self, RecordedWorktree};
use anyhow::{bail, Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the services view asks for the stack's state again.
const REFRESH: Duration = Duration::from_secs(2);
/// How long the event loop waits for a key before drawing again.
const TICK: Duration = Duration::from_millis(100);
/// Lines of a log kept in memory; older ones scroll away.
const LOG_LINES: usize = 10_000;
/// Lines of a command's output kept for its pane.
const OUTPUT_LINES: usize = 500;
/// How much of an existing host log is read when its view opens.
const LOG_BACKLOG_BYTES: u64 = 256 * 1024;
/// How much of a container's log is asked for when its view opens.
const COMPOSE_BACKLOG_LINES: &str = "500";

/// Where a service's output comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum LogSource {
    /// The file a supervised host process writes to.
    Host(PathBuf),
    /// A container of the worktree's Compose project.
    Compose {
        file: PathBuf,
        project: String,
        container: String,
    },
}

/// One row of the services view.
#[derive(Clone, Debug)]
pub struct ServiceRow {
    pub qual: String,
    pub runtime: &'static str,
    pub state: String,
    pub port: Option<u16>,
    pub url: Option<String>,
    /// The `<slug>.localhost` URL of a port that opted into `browser_alias`:
    /// shown in the plain one's place, and what `o` opens when there is one.
    pub alias: Option<String>,
    pub log: LogSource,
}

/// A worktree's stack at one moment.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// `<repository> · <worktree id>`.
    pub title: String,
    /// What the terminal's tab is named after: the branch checked out, or the
    /// worktree id when the checkout is detached.
    pub tab: String,
    pub rows: Vec<ServiceRow>,
    /// The worktree's port assignment; empty until an `up` claims one.
    pub ports: Vec<PortRow>,
    /// Whether the worktree has an assignment at all.
    pub assigned: bool,
    /// The resolved environment, or why it could not be resolved. Before the
    /// worktree has an assignment this is the preview a dry run would print.
    pub env: Result<Vec<(String, String)>, String>,
}

/// One assigned port, as `magictree ports` lists it.
#[derive(Clone, Debug)]
pub struct PortRow {
    pub name: String,
    pub port: u16,
    pub url: String,
    pub alias: Option<String>,
}

/// Read the stack of the worktree at `path`. The manifest is loaded afresh
/// every time, so an edit shows up on the next refresh.
fn snapshot(path: &Path) -> Result<Snapshot, Problem> {
    // A repository magictree was never set up in is not a failure: it is the
    // first thing most people open the screen on, so it gets the way in
    // instead of the loader's error.
    let manifest = crate::manifest::MANIFEST_FILE;
    if !path.ancestors().any(|dir| dir.join(manifest).is_file()) {
        let hint = Repo::open_optional(path)
            .ok()
            .flatten()
            .and_then(|repo| worktrees::missing_manifest_hint(&repo, path));
        return Err(Problem::NoManifest { hint });
    }
    read_stack(path).map_err(|error| Problem::Failed(format!("{error:#}")))
}

fn read_stack(path: &Path) -> Result<Snapshot> {
    let ctx = Ctx::load(path)?;
    let repository = ctx
        .repo
        .main_worktree_root()
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| ctx.repo.key());
    let rows = cli::status_rows(&ctx, false)?
        .into_iter()
        .map(|row| {
            let node = &ctx.nodes[row.index];
            let log = match (node.runtime(), ctx.runner_key(node)) {
                (Some(Runtime::Compose), Some((file, project))) => LogSource::Compose {
                    file,
                    project,
                    container: node
                        .service()
                        .and_then(|service| service.compose.as_ref())
                        .map(|reference| reference.service.clone())
                        .unwrap_or_default(),
                },
                _ => LogSource::Host(run::log_file(&ctx.runtime_dir, &row.qual)),
            };
            ServiceRow {
                url: row.url(),
                alias: row.alias,
                qual: row.qual,
                runtime: row.runtime,
                state: row.state,
                port: row.port,
                log,
            }
        })
        .collect();
    let tab = crate::repo::run_git(&ctx.repo.worktree_root, &["branch", "--show-current"])
        .ok()
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| ctx.repo.worktree_id());
    // Read, never claimed: looking at a worktree must not reserve its ports.
    let assignment = ports::load(&ctx.paths, &ctx.repo.key(), &ctx.repo.worktree_id())?;
    let aliases = assignment
        .as_ref()
        .map(|assignment| ctx.browser_url_aliases(assignment))
        .unwrap_or_default();
    let port_rows = assignment
        .iter()
        .flat_map(|assignment| &assignment.ports)
        .map(|(name, port)| PortRow {
            name: name.clone(),
            port: *port,
            url: format!("http://localhost:{port}"),
            alias: aliases
                .get(port)
                .map(|host| format!("http://{host}:{port}")),
        })
        .collect();
    let env = match &assignment {
        Some(assignment) => ctx.build_env(None, assignment),
        None => ctx.preview_plan(None, None),
    }
    .map(|plan| plan.vars.into_iter().collect())
    .map_err(|error| format!("{error:#}"));
    Ok(Snapshot {
        title: format!("{repository} · {}", ctx.repo.worktree_id()),
        tab,
        rows,
        ports: port_rows,
        assigned: assignment.is_some(),
        env,
    })
}

/// Why there is no stack to show.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// No `magictree.toml` here or above. `hint` is set when the primary
    /// checkout has one and only this worktree is missing it.
    NoManifest { hint: Option<String> },
    /// The stack is defined and could not be read.
    Failed(String),
}

impl Problem {
    /// What the section shows in place of its table, and in which colour: a
    /// repository that is not set up is told how, calmly; a real failure is
    /// the error, in red.
    fn message(&self) -> (String, Color) {
        match self {
            Problem::Failed(error) => (error.clone(), Color::Red),
            Problem::NoManifest { hint: Some(hint) } => (
                format!("No stack in this worktree yet\n\n{hint}"),
                Color::Reset,
            ),
            Problem::NoManifest { hint: None } => (
                "No stack in this repository yet\n\n\
                 magictree runs what a magictree.toml describes, and there is none here.\n\
                 To write one, in the primary checkout:\n\n  \
                 magictree discover && magictree init\n  \
                 git add magictree.toml && git commit -m \"add magictree.toml\"\n\n\
                 2 or :worktrees lists the stacks magictree already knows."
                    .to_string(),
                Color::Reset,
            ),
        }
    }
}

/// What the background threads tell the screen.
pub enum Msg {
    /// A fresh read of the stack. `generation` names the worktree it was read
    /// for, so an answer about one the screen has left is dropped.
    Stack {
        generation: u64,
        result: Result<Snapshot, Problem>,
    },
    /// A line a running command printed.
    Output(String),
    /// The running command ended.
    Finished { ok: bool },
    /// A line of the log being followed.
    Log { generation: u64, line: String },
}

/// What the screen asks the outside world for.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Run `magictree <args>` in the worktree on screen.
    Run { label: String, args: Vec<String> },
    /// Run `magictree <args>` about something other than the worktree on
    /// screen: in `cwd`, or nowhere in particular when a command (`gc --all`)
    /// answers for the whole machine.
    RunIn {
        label: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    },
    /// Start following a service's log.
    Follow(LogSource),
    /// Stop following it.
    Unfollow,
    /// The worktree on screen changed, or its state is worth reading again.
    Refresh,
    /// Read the worktree list again.
    ListWorktrees,
    /// Open a URL in the browser.
    Browse(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum View {
    Services,
    Worktrees,
    Ports,
    Env,
    Logs,
}

/// A section of the screen: a tab, and a word the `:` prompt answers to.
pub struct Section {
    pub view: View,
    pub name: &'static str,
    /// Shorter spellings the prompt accepts.
    pub aliases: &'static [&'static str],
}

/// Every section, in the order the tabs show them and the digits pick them.
/// Adding a view means adding it here: the tab bar, the digit keys, the prompt
/// and its completion all read this list.
pub const SECTIONS: [Section; 5] = [
    Section {
        view: View::Services,
        name: "services",
        aliases: &["service", "svc", "s"],
    },
    Section {
        view: View::Worktrees,
        name: "worktrees",
        aliases: &["worktree", "wt", "w"],
    },
    Section {
        view: View::Ports,
        name: "ports",
        aliases: &["port", "p"],
    },
    Section {
        view: View::Env,
        name: "env",
        aliases: &["environment", "e"],
    },
    Section {
        view: View::Logs,
        name: "logs",
        aliases: &["log", "l"],
    },
];

/// What the prompt takes besides a section. It navigates and nothing more:
/// everything that changes a stack is a key in the section it applies to.
const ACTIONS: [&str; 1] = ["quit"];

/// The word the prompt would complete `input` to: the first section or action
/// it begins, while it is still a single unfinished word.
pub fn suggestion(input: &str) -> Option<&'static str> {
    if input.is_empty() || input.contains(char::is_whitespace) {
        return None;
    }
    let typed = input.to_lowercase();
    SECTIONS
        .iter()
        .map(|section| section.name)
        .chain(ACTIONS)
        .find(|word| word.starts_with(&typed) && *word != typed)
}

/// Where a command runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    /// In the worktree on screen.
    Target,
    /// In this directory.
    Dir(PathBuf),
    /// Anywhere: the command answers for the whole machine.
    Machine,
}

/// A command waiting for `y`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    /// The question, after "run `magictree ".
    pub label: String,
    pub args: Vec<String>,
    pub scope: Scope,
    /// What the dialog adds after the command, so a removal says what goes.
    pub about: &'static str,
}

/// A command the screen started, and what it has printed so far.
pub struct Activity {
    pub label: String,
    /// `None` while it runs.
    pub ok: Option<bool>,
    pub lines: VecDeque<String>,
}

/// The log on screen.
pub struct LogView {
    pub title: String,
    pub lines: VecDeque<String>,
    /// Lines between the bottom of the window and the end of the log: zero
    /// follows new output.
    pub offset: usize,
    /// Columns scrolled to the right.
    pub column: usize,
}

/// Everything the screen shows, and nothing else.
pub struct App {
    pub dry_run: bool,
    pub view: View,
    /// The worktree whose stack the services view shows.
    pub target: Option<PathBuf>,
    pub generation: u64,
    pub stack: Option<Result<Snapshot, Problem>>,
    pub services: TableState,
    pub worktrees: Vec<RecordedWorktree>,
    pub worktree_state: TableState,
    pub ports_state: TableState,
    pub env_state: TableState,
    /// What has been typed after `:`, while the prompt is open.
    pub prompt: Option<String>,
    pub activity: Option<Activity>,
    /// A command waiting for `y`.
    pub confirm: Option<Pending>,
    pub log: Option<LogView>,
    pub log_generation: u64,
    /// Rows of log the last draw had room for, which is what a page scrolls by.
    pub log_height: usize,
    pub notice: Option<String>,
    pub quit: bool,
}

impl App {
    pub fn new(target: Option<PathBuf>, dry_run: bool) -> Self {
        Self {
            dry_run,
            view: if target.is_some() {
                View::Services
            } else {
                View::Worktrees
            },
            target,
            generation: 0,
            stack: None,
            services: TableState::default(),
            worktrees: Vec::new(),
            worktree_state: TableState::default(),
            ports_state: TableState::default(),
            env_state: TableState::default(),
            prompt: None,
            activity: None,
            confirm: None,
            log: None,
            log_generation: 0,
            log_height: 1,
            notice: None,
            quit: false,
        }
    }

    fn rows(&self) -> &[ServiceRow] {
        match &self.stack {
            Some(Ok(snapshot)) => &snapshot.rows,
            _ => &[],
        }
    }

    fn port_rows(&self) -> &[PortRow] {
        match &self.stack {
            Some(Ok(snapshot)) => &snapshot.ports,
            _ => &[],
        }
    }

    fn env_rows(&self) -> &[(String, String)] {
        match &self.stack {
            Some(Ok(Snapshot { env: Ok(env), .. })) => env,
            _ => &[],
        }
    }

    fn selected(&self) -> Option<&ServiceRow> {
        self.rows().get(self.services.selected()?)
    }

    fn busy(&self) -> bool {
        self.activity
            .as_ref()
            .is_some_and(|activity| activity.ok.is_none())
    }

    pub fn set_worktrees(&mut self, rows: Vec<RecordedWorktree>) {
        let selected = self
            .worktree_state
            .selected()
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        self.worktree_state.select(if rows.is_empty() {
            None
        } else {
            Some(selected)
        });
        self.worktrees = rows;
    }

    pub fn on_msg(&mut self, msg: Msg) -> Option<Effect> {
        match msg {
            Msg::Stack { generation, result } => {
                if generation != self.generation {
                    return None;
                }
                // The selection follows the service, not the row it was on: a
                // manifest edit can reorder them between two refreshes.
                let previous = self.selected().map(|row| row.qual.clone());
                let selected = match &result {
                    Ok(snapshot) if !snapshot.rows.is_empty() => Some(
                        previous
                            .and_then(|qual| snapshot.rows.iter().position(|row| row.qual == qual))
                            .or(self.services.selected())
                            .unwrap_or(0)
                            .min(snapshot.rows.len() - 1),
                    ),
                    _ => None,
                };
                self.stack = Some(result);
                self.services.select(selected);
                let (ports, env) = (self.port_rows().len(), self.env_rows().len());
                step(&mut self.ports_state, ports, 0);
                step(&mut self.env_state, env, 0);
                None
            }
            Msg::Output(line) => {
                if let Some(activity) = &mut self.activity {
                    push_capped(&mut activity.lines, plain(&line), OUTPUT_LINES);
                }
                None
            }
            Msg::Finished { ok } => {
                if let Some(activity) = &mut self.activity {
                    activity.ok = Some(ok);
                }
                // What ran may have removed a worktree or reclaimed a record.
                Some(if self.view == View::Worktrees {
                    Effect::ListWorktrees
                } else {
                    Effect::Refresh
                })
            }
            Msg::Log { generation, line } => {
                if generation != self.log_generation {
                    return None;
                }
                if let Some(log) = &mut self.log {
                    // A reader that scrolled up keeps the lines it is reading
                    // where they are while new ones arrive below.
                    if log.offset > 0 && log.lines.len() < LOG_LINES {
                        log.offset += 1;
                    }
                    push_capped(&mut log.lines, plain(&line), LOG_LINES);
                }
                None
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Effect> {
        self.notice = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }
        if let Some(pending) = self.confirm.take() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let removed = (pending.args.first().map(String::as_str) == Some("rm"))
                        .then(|| pending.args.get(1).map(PathBuf::from))
                        .flatten();
                    let effect = self.start_in(pending.label, pending.args, pending.scope);
                    if effect.is_some() && removed.is_some() && removed == self.target {
                        // The stack on screen is the one being removed.
                        self.target = None;
                        self.stack = None;
                        self.generation += 1;
                        self.services.select(None);
                    }
                    effect
                }
                _ => None,
            };
        }
        if self.prompt.is_some() {
            return self.on_prompt_key(key.code);
        }
        let current = SECTIONS
            .iter()
            .position(|section| section.view == self.view)
            .unwrap_or(0);
        match key.code {
            KeyCode::Char(':') => {
                self.prompt = Some(String::new());
                return None;
            }
            KeyCode::Char(digit @ '1'..='9') => {
                let section = SECTIONS.get(digit as usize - '1' as usize)?;
                return self.open(section.view, None);
            }
            KeyCode::Tab => return self.open(SECTIONS[(current + 1) % SECTIONS.len()].view, None),
            KeyCode::BackTab => {
                let previous = (current + SECTIONS.len() - 1) % SECTIONS.len();
                return self.open(SECTIONS[previous].view, None);
            }
            _ => {}
        }
        match self.view {
            View::Services => self.on_services_key(key.code),
            View::Worktrees => self.on_worktrees_key(key.code),
            View::Ports | View::Env => self.on_table_key(key.code),
            View::Logs => self.on_logs_key(key.code),
        }
    }

    /// Switch to a section. `service` names whose log to follow; without it
    /// the log is the selected service's.
    pub fn open(&mut self, view: View, service: Option<&str>) -> Option<Effect> {
        if view != View::Worktrees && self.target.is_none() {
            self.notice = Some("no worktree is open: pick one with enter".to_string());
            return None;
        }
        if view == View::Logs {
            let row = match service {
                Some(name) => self
                    .rows()
                    .iter()
                    .find(|row| row.qual == name || row.qual.rsplit(':').next() == Some(name)),
                None => self.selected(),
            };
            let Some(row) = row else {
                self.notice = Some(match service {
                    Some(name) => format!("unknown service '{name}'"),
                    None => "no service to follow".to_string(),
                });
                return None;
            };
            let (title, source) = (row.qual.clone(), row.log.clone());
            self.log_generation += 1;
            self.log = Some(LogView {
                title,
                lines: VecDeque::new(),
                offset: 0,
                column: 0,
            });
            self.view = View::Logs;
            return Some(Effect::Follow(source));
        }
        if self.log.take().is_some() {
            // Lines still on their way from the log just left belong to it.
            self.log_generation += 1;
        }
        self.view = view;
        (view == View::Worktrees).then_some(Effect::ListWorktrees)
    }

    fn on_prompt_key(&mut self, code: KeyCode) -> Option<Effect> {
        let input = self.prompt.as_mut()?;
        match code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Enter => {
                let input = self.prompt.take().unwrap_or_default();
                return self.command(&input);
            }
            KeyCode::Backspace => {
                if input.pop().is_none() {
                    self.prompt = None;
                }
            }
            KeyCode::Tab => {
                if let Some(word) = suggestion(input) {
                    *input = word.to_string();
                }
            }
            KeyCode::Char(c) => input.push(c),
            _ => {}
        }
        None
    }

    /// Go where the words typed after `:` say: a section's name or alias (and
    /// for `logs`, whose), or out.
    pub fn command(&mut self, input: &str) -> Option<Effect> {
        let mut words = input.split_whitespace();
        let first = words.next()?.to_lowercase();
        if let Some(section) = SECTIONS
            .iter()
            .find(|section| section.name == first || section.aliases.contains(&first.as_str()))
        {
            return self.open(section.view, words.next());
        }
        match first.as_str() {
            "q" | "quit" | "exit" => self.request_quit(),
            // The prompt is the obvious place to try these, so say where they
            // live instead of only that they are unknown.
            "up" | "down" | "restart" | "start" | "open" => {
                self.notice = Some(format!(
                    "'{first}' is a key in services: u up, s start, r restart, d down, o open, l logs"
                ));
            }
            _ => {
                let sections: Vec<&str> = SECTIONS.iter().map(|section| section.name).collect();
                self.notice = Some(format!(
                    "unknown section '{first}': try {}",
                    sections.join(", ")
                ));
            }
        }
        None
    }

    /// The keys that act on the whole stack, which every section about the
    /// worktree answers to: `u` runs `up`, and `d` runs `down` after a yes.
    fn on_stack_key(&mut self, code: KeyCode) -> Option<Effect> {
        match code {
            KeyCode::Char('u') => self.start("up".to_string(), vec!["up".to_string()]),
            KeyCode::Char('d') => {
                self.confirm = Some(Pending {
                    label: "down".to_string(),
                    args: vec!["down".to_string()],
                    scope: Scope::Target,
                    about: "in this worktree",
                });
                None
            }
            _ => None,
        }
    }

    /// The keys of a section that is a table to read.
    fn on_table_key(&mut self, code: KeyCode) -> Option<Effect> {
        let count = match self.view {
            View::Ports => self.port_rows().len(),
            _ => self.env_rows().len(),
        };
        let state = match self.view {
            View::Ports => &mut self.ports_state,
            _ => &mut self.env_state,
        };
        match code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('u') | KeyCode::Char('d') => return self.on_stack_key(code),
            KeyCode::Esc => return self.open(View::Services, None),
            KeyCode::Down | KeyCode::Char('j') => step(state, count, 1),
            KeyCode::Up | KeyCode::Char('k') => step(state, count, -1),
            KeyCode::PageDown | KeyCode::Char(' ') => step(state, count, 10),
            KeyCode::PageUp | KeyCode::Char('b') => step(state, count, -10),
            KeyCode::Home | KeyCode::Char('g') => step(state, count, isize::MIN),
            KeyCode::End | KeyCode::Char('G') => step(state, count, isize::MAX),
            _ => {}
        }
        None
    }

    /// Quit, unless a command is still running: its output is the only record
    /// of what it did, and leaving would cut it off mid-run.
    fn request_quit(&mut self) {
        match &self.activity {
            Some(activity) if activity.ok.is_none() => {
                self.notice = Some(format!(
                    "`{}` is still running; ctrl-c quits and stops it",
                    activity.label
                ));
            }
            _ => self.quit = true,
        }
    }

    fn start(&mut self, label: String, args: Vec<String>) -> Option<Effect> {
        self.start_in(label, args, Scope::Target)
    }

    fn start_in(&mut self, label: String, args: Vec<String>, scope: Scope) -> Option<Effect> {
        if scope == Scope::Target && self.target.is_none() {
            self.notice = Some("no worktree is open: pick one with enter".to_string());
            return None;
        }
        if let Some(activity) = self.activity.as_ref().filter(|a| a.ok.is_none()) {
            self.notice = Some(format!("`{}` is still running", activity.label));
            return None;
        }
        self.activity = Some(Activity {
            label: label.clone(),
            ok: None,
            lines: VecDeque::new(),
        });
        match scope {
            Scope::Target => Some(Effect::Run { label, args }),
            Scope::Dir(cwd) => Some(Effect::RunIn {
                label,
                args,
                cwd: Some(cwd),
            }),
            Scope::Machine => Some(Effect::RunIn {
                label,
                args,
                cwd: None,
            }),
        }
    }

    /// `x` in worktrees: remove the selected one, after a yes. The same
    /// `magictree rm` a shell would run, so a dirty checkout is refused and
    /// the branch is kept.
    fn remove_selected(&mut self) -> Option<Effect> {
        let row = self.worktrees.get(self.worktree_state.selected()?)?;
        if row.live == Some(true) && row.worktree_id == PRIMARY_WORKTREE_ID {
            self.notice = Some(
                "the primary checkout is the repository itself: it is never removed".to_string(),
            );
            return None;
        }
        let path = match (&row.path, row.live) {
            (Some(path), Some(true)) => path.clone(),
            _ => {
                self.notice = Some(format!(
                    "{} has no checkout left to remove: c runs gc, which reclaims what it left",
                    row.worktree_id
                ));
                return None;
            }
        };
        if self.busy() {
            self.notice = Some("a command is still running".to_string());
            return None;
        }
        // Run from the repository the worktree belongs to: `rm` resolves a path
        // against the repository it is run in.
        let scope = Scope::Dir(row.repo_path.clone().unwrap_or_else(|| path.clone()));
        self.confirm = Some(Pending {
            label: format!("rm {}", tilde(&path)),
            args: vec!["rm".to_string(), path.display().to_string()],
            scope,
            about: "stops its stack, deletes the checkout, releases its ports; the branch is kept",
        });
        None
    }

    fn on_services_key(&mut self, code: KeyCode) -> Option<Effect> {
        let count = self.rows().len();
        match code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Esc => {
                if !self.busy() {
                    self.activity = None;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => step(&mut self.services, count, 1),
            KeyCode::Up | KeyCode::Char('k') => step(&mut self.services, count, -1),
            KeyCode::Home | KeyCode::Char('g') => step(&mut self.services, count, isize::MIN),
            KeyCode::End | KeyCode::Char('G') => step(&mut self.services, count, isize::MAX),
            KeyCode::Char('w') => return self.open(View::Worktrees, None),
            KeyCode::Char('u') | KeyCode::Char('d') => return self.on_stack_key(code),
            KeyCode::Char('s') => {
                let qual = self.selected()?.qual.clone();
                return self.start(format!("up {qual}"), vec!["up".to_string(), qual]);
            }
            KeyCode::Char('r') => {
                let qual = self.selected()?.qual.clone();
                return self.start(format!("restart {qual}"), vec!["restart".to_string(), qual]);
            }
            KeyCode::Char('o') => {
                let row = self.selected()?;
                match row.alias.clone().or_else(|| row.url.clone()) {
                    Some(url) => return Some(Effect::Browse(url)),
                    None => self.notice = Some(format!("{} has no port", row.qual)),
                }
            }
            KeyCode::Enter | KeyCode::Char('l') => return self.open(View::Logs, None),
            _ => {}
        }
        None
    }

    fn on_worktrees_key(&mut self, code: KeyCode) -> Option<Effect> {
        let count = self.worktrees.len();
        match code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Esc | KeyCode::Char('w') => {
                if self.target.is_some() {
                    self.view = View::Services;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => step(&mut self.worktree_state, count, 1),
            KeyCode::Up | KeyCode::Char('k') => step(&mut self.worktree_state, count, -1),
            KeyCode::Home | KeyCode::Char('g') => step(&mut self.worktree_state, count, isize::MIN),
            KeyCode::End | KeyCode::Char('G') => step(&mut self.worktree_state, count, isize::MAX),
            KeyCode::Char('r') => return Some(Effect::ListWorktrees),
            KeyCode::Char('x') => return self.remove_selected(),
            KeyCode::Char('c') => {
                self.confirm = Some(Pending {
                    label: "gc --all".to_string(),
                    args: vec!["gc".to_string(), "--all".to_string()],
                    scope: Scope::Machine,
                    about: "reclaims the ports, containers and processes of every gone worktree",
                });
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                let row = self.worktrees.get(self.worktree_state.selected()?)?;
                let path = match (&row.path, row.live) {
                    (Some(path), Some(true)) => path.clone(),
                    _ => {
                        self.notice = Some(format!(
                            "{} has no checkout any more; `magictree gc --all` reclaims it",
                            row.worktree_id
                        ));
                        return None;
                    }
                };
                // A command belongs to the worktree it was started in, and its
                // pane would follow the screen to another one.
                if self.busy() && self.target.as_ref() != Some(&path) {
                    self.notice =
                        Some("a command is still running in the current worktree".to_string());
                    return None;
                }
                self.view = View::Services;
                if self.target.as_ref() != Some(&path) {
                    self.target = Some(path);
                    self.generation += 1;
                    self.stack = None;
                    self.activity = None;
                    self.services.select(None);
                }
                return Some(Effect::Refresh);
            }
            _ => {}
        }
        None
    }

    fn on_logs_key(&mut self, code: KeyCode) -> Option<Effect> {
        let page = self.log_height.max(1);
        let Some(log) = &mut self.log else {
            self.view = View::Services;
            return None;
        };
        let limit = log.lines.len().saturating_sub(page);
        match code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.open(View::Services, None);
                return Some(Effect::Unfollow);
            }
            KeyCode::Up | KeyCode::Char('k') => log.offset = (log.offset + 1).min(limit),
            KeyCode::Down | KeyCode::Char('j') => log.offset = log.offset.saturating_sub(1),
            KeyCode::PageUp | KeyCode::Char('b') => log.offset = (log.offset + page).min(limit),
            KeyCode::PageDown | KeyCode::Char(' ') => log.offset = log.offset.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => log.offset = limit,
            KeyCode::End | KeyCode::Char('G') => log.offset = 0,
            KeyCode::Right | KeyCode::Char('l') => log.column += 8,
            KeyCode::Left | KeyCode::Char('h') => log.column = log.column.saturating_sub(8),
            _ => {}
        }
        None
    }
}

/// Move a table's selection by `delta` rows, stopping at either end.
fn step(state: &mut TableState, count: usize, delta: isize) {
    if count == 0 {
        state.select(None);
        return;
    }
    let current = state.selected().unwrap_or(0) as isize;
    let next = current.saturating_add(delta).clamp(0, count as isize - 1);
    state.select(Some(next as usize));
}

fn push_capped(lines: &mut VecDeque<String>, line: String, cap: usize) {
    if lines.len() == cap {
        lines.pop_front();
    }
    lines.push_back(line);
}

/// A line as a table cell can show it: escape sequences and control characters
/// removed, tabs widened, and only what a carriage return left visible. A dev
/// server colours its output and redraws its progress in place; written to the
/// screen as they are, those bytes would move the cursor out of the pane.
pub fn plain(line: &str) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    let line = line.rsplit('\r').next().unwrap_or(line);
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters, then one final byte in `@`..=`~`.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL, or to the two-byte string terminator.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The colour of a state: what is up, what is on its way, what is broken, and
/// what is simply not running.
fn state_style(state: &str) -> Style {
    let color = if state.contains("unhealthy")
        || state.starts_with("exited")
        || state.starts_with("dead")
    {
        Color::Red
    } else if state.contains("starting")
        || state.starts_with("restarting")
        || state.starts_with("created")
        || state.starts_with("paused")
    {
        Color::Yellow
    } else if state.starts_with("running") {
        Color::Green
    } else {
        Color::DarkGray
    };
    Style::new().fg(color)
}

/// The lime the mark stands on, and the screen's one accent. Asked for exactly
/// where the terminal says it can show it, and as its nearest neighbour in the
/// 256-colour palette everywhere else.
fn lime() -> Color {
    static TRUECOLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *TRUECOLOR.get_or_init(banner::truecolor) {
        let (r, g, b) = banner::LIME;
        Color::Rgb(r, g, b)
    } else {
        Color::Indexed(banner::LIME_INDEXED)
    }
}

/// The mark beside two lines of text. It is five half-rows tall, so in three
/// rows of cells one half-row is empty: the help leaves it under the feet, and
/// here it is above the bar instead, which puts the feet on the baseline of the
/// second line of text rather than half a row above it. The feet are then the
/// lower half of their cells, drawn as the background behind the legs' ends.
fn mark() -> [Line<'static>; 3] {
    let white = Style::new().fg(Color::White);
    let foot = || Span::styled("▀", Style::new().fg(Color::White).bg(lime()));
    [
        Line::styled("▄▄▄▄▄", white),
        Line::styled(banner::MARK[1], white),
        Line::from(vec![foot(), Span::raw(" "), foot(), Span::raw(" "), foot()]),
    ]
}

/// One column on either side of the screen: the mark, the tabs and the
/// panels' borders all start in the same one.
const MARGIN: ratatui::layout::Margin = ratatui::layout::Margin::new(1, 0);

/// The mark's width, and the gap between it and the text beside it.
const MARK_WIDTH: u16 = 5;
const MARK_GAP: u16 = 2;
/// The gap between the text beside the mark and the keys, and between two
/// columns of keys.
const HINT_GAP: u16 = 4;
/// Below this many rows the header gives its two extra rows back to the stack.
const ROOMY: u16 = 18;

fn selection() -> Style {
    Style::new().bg(lime()).fg(Color::Black)
}

fn heading(titles: &[&'static str]) -> Row<'static> {
    Row::new(titles.iter().copied().map(Cell::from))
        .style(Style::new().fg(lime()).add_modifier(Modifier::BOLD))
}

fn panel(title: String) -> Block<'static> {
    Block::bordered()
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().add_modifier(Modifier::BOLD),
        ))
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let roomy = frame.area().height >= ROOMY;
    let chrome = if roomy { 7 } else { 3 };
    let output = match (&app.activity, app.view) {
        (Some(activity), View::Services | View::Worktrees | View::Ports | View::Env) => {
            // The pane takes what the table does not need: a command's output
            // must never push a service off the screen it is reporting on.
            let available = frame.area().height.saturating_sub(chrome);
            let rows = match app.view {
                View::Worktrees => app.worktrees.len(),
                View::Ports => app.port_rows().len(),
                View::Env => app.env_rows().len(),
                _ => app.rows().len(),
            };
            let table = (rows as u16 + 3).min(available * 2 / 3);
            (activity.lines.len() as u16 + 2).clamp(3, available.saturating_sub(table).max(3))
        }
        _ => 0,
    };
    // A roomy screen spends two rows on air: one under the header, one
    // between the tabs and the section they choose.
    let [header, _, tabs, _, body, pane, footer] = Layout::vertical([
        Constraint::Length(if roomy { 3 } else { 1 }),
        Constraint::Length(u16::from(roomy)),
        Constraint::Length(1),
        Constraint::Length(u16::from(roomy)),
        Constraint::Min(3),
        Constraint::Length(output),
        Constraint::Length(1),
    ])
    .areas(frame.area().inner(MARGIN));
    // The footer's keys carry their own leading space, so it takes the whole
    // row and still starts in the same column as everything above it.
    let footer = Rect {
        x: frame.area().x,
        width: frame.area().width,
        ..footer
    };

    draw_header(frame, app, header);
    draw_tabs(frame, app, tabs);
    match app.view {
        View::Services => draw_services(frame, app, body),
        View::Worktrees => draw_worktrees(frame, app, body),
        View::Ports => draw_ports(frame, app, body),
        View::Env => draw_env(frame, app, body),
        View::Logs => draw_log(frame, app, body),
    }
    if output > 0 {
        draw_activity(frame, app, pane);
    }
    draw_footer(frame, app, footer);
    if let Some(pending) = &app.confirm {
        draw_confirm(frame, pending, body);
    }
}

/// The sections as tabs, each under the digit that opens it.
fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    for (index, section) in SECTIONS.iter().enumerate() {
        let label = format!(" {} {} ", index + 1, section.name);
        spans.push(if section.view == app.view {
            Span::styled(label, selection().add_modifier(Modifier::BOLD))
        } else {
            Span::styled(label, Style::new().fg(Color::DarkGray))
        });
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Line::from(spans), area);
}

/// What a section shows instead of its table: the stack is still being read,
/// could not be, or has nothing of the kind.
fn placeholder(app: &App, empty: Option<&str>) -> Option<(String, Color)> {
    match &app.stack {
        None => Some(("reading the stack…".to_string(), Color::DarkGray)),
        Some(Err(problem)) => Some(problem.message()),
        Some(Ok(_)) => empty.map(|text| (text.to_string(), Color::DarkGray)),
    }
}

fn draw_message(frame: &mut Frame, (text, color): (String, Color), block: Block, area: Rect) {
    let paragraph = Paragraph::new(text)
        .style(Style::new().fg(color))
        .wrap(ratatui::widgets::Wrap { trim: false })
        .block(block);
    frame.render_widget(paragraph, area);
}

fn draw_ports(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = panel("ports".to_string());
    let empty = app
        .port_rows()
        .is_empty()
        .then_some("no ports assigned yet: `u` in services runs `up`, which claims them");
    if let Some(message) = placeholder(app, empty) {
        return draw_message(frame, message, block, area);
    }
    let rows = app.port_rows();
    let name_width = rows
        .iter()
        .map(|row| row.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 40) as u16;
    let widths = [
        Constraint::Length(name_width),
        Constraint::Length(5),
        Constraint::Min(22),
    ];
    let table = Table::new(
        rows.iter().map(|row| {
            Row::new([
                Cell::from(row.name.clone()),
                Cell::from(row.port.to_string()),
                // One URL a port: its alias where it opted into one.
                Cell::from(row.alias.clone().unwrap_or_else(|| row.url.clone())),
            ])
        }),
        widths,
    )
    .column_spacing(2)
    .header(heading(&["NAME", "PORT", "URL"]))
    .row_highlight_style(selection())
    .block(block);
    frame.render_stateful_widget(table, area, &mut app.ports_state);
}

fn draw_env(frame: &mut Frame, app: &mut App, area: Rect) {
    let assigned = !matches!(
        &app.stack,
        Some(Ok(Snapshot {
            assigned: false,
            ..
        }))
    );
    let block = panel(if assigned {
        "env".to_string()
    } else {
        "env · preview, no ports assigned yet".to_string()
    });
    let message = match &app.stack {
        Some(Ok(Snapshot {
            env: Err(error), ..
        })) => Some((error.clone(), Color::Red)),
        _ => placeholder(
            app,
            app.env_rows()
                .is_empty()
                .then_some("the manifests set no environment"),
        ),
    };
    if let Some(message) = message {
        return draw_message(frame, message, block, area);
    }
    let rows = app.env_rows();
    let key_width = rows
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 40) as u16;
    let table = Table::new(
        rows.iter()
            .map(|(key, value)| Row::new([Cell::from(key.clone()), Cell::from(plain(value))])),
        [Constraint::Length(key_width), Constraint::Min(10)],
    )
    .column_spacing(2)
    .header(heading(&["VARIABLE", "VALUE"]))
    .row_highlight_style(selection())
    .block(block);
    frame.render_stateful_widget(table, area, &mut app.env_state);
}

/// A path the way a shell prompt writes it: `~` for the home directory.
fn tilde(path: &Path) -> String {
    match crate::paths::home()
        .ok()
        .and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf))
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The keys the screen answers to right now.
fn keys(app: &App) -> &'static [(&'static str, &'static str)] {
    if app.confirm.is_some() {
        return &[("y", "confirm"), ("any key", "cancel")];
    }
    match app.view {
        View::Services => &[
            ("l", "logs"),
            ("u", "up"),
            ("s", "start"),
            ("r", "restart"),
            ("d", "down"),
            ("o", "open"),
            (":", "command"),
            ("q", "quit"),
        ],
        View::Worktrees => &[
            ("enter", "open"),
            ("x", "remove"),
            ("c", "gc"),
            ("r", "refresh"),
            (":", "command"),
            ("q", "quit"),
        ],
        View::Ports | View::Env => &[
            ("↑↓", "move"),
            ("u", "up"),
            ("d", "down"),
            ("esc", "services"),
            (":", "command"),
            ("q", "quit"),
        ],
        View::Logs => &[
            ("↑↓", "scroll"),
            ("pgup/pgdn", "page"),
            ("g/G", "top/follow"),
            ("←→", "pan"),
            (":", "command"),
            ("esc", "back"),
        ],
    }
}

/// The header. On a roomy screen: the mark, three lines beside it — the
/// version, the repository and worktree, the path from `~` — and then the keys, in
/// columns of three the way k9s lists them. On a short one, a single line, and
/// the keys go to the footer instead.
fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::DarkGray);
    let mut name = vec![Span::styled(
        format!("magictree {}", env!("CARGO_PKG_VERSION")),
        bold,
    )];
    if app.dry_run {
        name.push(Span::raw("  "));
        name.push(Span::styled(
            " dry run ",
            Style::new().bg(Color::Yellow).fg(Color::Black),
        ));
    }
    let title = match (&app.stack, app.view) {
        (_, View::Worktrees) => None,
        (Some(Ok(snapshot)), _) => Some(Span::styled(snapshot.title.clone(), bold)),
        _ => None,
    };
    let path = match (app.view, &app.target) {
        (View::Worktrees, _) | (_, None) => None,
        (_, Some(target)) => Some(Span::styled(tilde(target), dim)),
    };

    if area.height < 3 {
        let mut spans = vec![Span::styled("▄", Style::new().fg(lime())), Span::raw(" ")];
        spans.extend(name);
        for span in [title, path].into_iter().flatten() {
            spans.push(Span::raw("  "));
            spans.push(span);
        }
        frame.render_widget(Line::from(spans), area);
        return;
    }

    // Whatever the place has to say, under the name; the tagline when it has
    // nothing, so the mark never stands beside a single line.
    let mut lines = vec![Line::from(name)];
    lines.extend([title, path].into_iter().flatten().map(Line::from));
    if lines.len() == 1 {
        lines.push(Line::styled(banner::TAGLINE, dim));
    }
    let info_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;

    let [mark_area, _, info, _, hints] = Layout::horizontal([
        Constraint::Length(MARK_WIDTH),
        Constraint::Length(MARK_GAP),
        Constraint::Length(info_width),
        Constraint::Length(HINT_GAP),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(mark().to_vec()), mark_area);
    frame.render_widget(Paragraph::new(lines), info);

    // Keys fill a column top to bottom, then the next; a column that does not
    // fit is left out whole rather than cut mid-word.
    let rows = area.height as usize;
    let mut x = hints.x;
    for column in keys(app).chunks(rows) {
        let key_width = column
            .iter()
            .map(|(key, _)| key.chars().count())
            .max()
            .unwrap_or(0)
            + 2;
        let width = column
            .iter()
            .map(|(_, what)| key_width + 1 + what.chars().count())
            .max()
            .unwrap_or(0) as u16;
        if x + width > hints.right() {
            break;
        }
        let lines: Vec<Line> = column
            .iter()
            .map(|(key, what)| {
                Line::from(vec![
                    Span::styled(
                        format!("{:<key_width$}", format!("<{key}>")),
                        Style::new().fg(lime()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {what}"), dim),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), Rect { x, width, ..hints });
        x += width + HINT_GAP;
    }
}

fn draw_services(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = panel("services".to_string());
    let empty = app
        .rows()
        .is_empty()
        .then_some("this manifest declares no services");
    if let Some(message) = placeholder(app, empty) {
        return draw_message(frame, message, block, area);
    }
    let rows = app.rows();
    let name_width = rows
        .iter()
        .map(|row| row.qual.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(7, 32) as u16;
    let state_width = rows
        .iter()
        .map(|row| row.state.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(5, 32) as u16;
    // A port that opted into `browser_alias` has two URLs; the alias is the one
    // a browser should use, so it alone is shown, in the plain one's place.
    let widths = [
        Constraint::Length(name_width),
        Constraint::Length(7),
        Constraint::Length(state_width),
        Constraint::Length(5),
        Constraint::Min(22),
    ];
    let table = Table::new(
        rows.iter().map(|row| {
            Row::new([
                Cell::from(row.qual.clone()),
                Cell::from(row.runtime),
                Cell::from(row.state.clone()).style(state_style(&row.state)),
                Cell::from(row.port.map(|port| port.to_string()).unwrap_or_default()),
                Cell::from(
                    row.alias
                        .clone()
                        .or_else(|| row.url.clone())
                        .unwrap_or_default(),
                ),
            ])
        }),
        widths,
    )
    .column_spacing(2)
    .header(heading(&["SERVICE", "RUNTIME", "STATE", "PORT", "URL"]))
    .row_highlight_style(selection())
    .block(block);
    frame.render_stateful_widget(table, area, &mut app.services);
}

fn draw_worktrees(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = panel("worktrees".to_string());
    if app.worktrees.is_empty() {
        let paragraph = Paragraph::new("no worktrees recorded; `magictree up` records one")
            .style(Style::new().fg(Color::DarkGray))
            .block(block);
        frame.render_widget(paragraph, area);
        return;
    }
    let repository = |row: &RecordedWorktree| match &row.repo_path {
        Some(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string()),
        None => row.repo_key.clone(),
    };
    let repo_width = app
        .worktrees
        .iter()
        .map(|row| repository(row).chars().count())
        .max()
        .unwrap_or(0)
        .clamp(10, 28) as u16;
    let id_width = app
        .worktrees
        .iter()
        .map(|row| row.worktree_id.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 28) as u16;
    let ports_width = app
        .worktrees
        .iter()
        .map(|row| row.ports.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(5, 36) as u16;
    let table = Table::new(
        app.worktrees.iter().map(|row| {
            // The same three words `list --all` prints.
            let (state, color) = match row.live {
                Some(true) if row.worktree_id == PRIMARY_WORKTREE_ID => ("primary", Color::Green),
                Some(true) => ("live", Color::Green),
                Some(false) => ("gone", Color::Red),
                None => ("?", Color::DarkGray),
            };
            let current = row.path.is_some() && row.path == app.target;
            Row::new([
                Cell::from(repository(row)),
                Cell::from(row.worktree_id.clone()),
                Cell::from(state).style(Style::new().fg(color)),
                Cell::from(if row.ports.is_empty() {
                    "-".to_string()
                } else {
                    row.ports.clone()
                }),
                Cell::from(
                    row.path
                        .as_deref()
                        .map(tilde)
                        .unwrap_or_else(|| "-".to_string()),
                ),
            ])
            .style(if current {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            })
        }),
        [
            Constraint::Length(repo_width),
            Constraint::Length(id_width),
            Constraint::Length(7),
            Constraint::Length(ports_width),
            Constraint::Min(10),
        ],
    )
    .column_spacing(2)
    .header(heading(&[
        "REPOSITORY",
        "WORKTREE",
        "STATE",
        "PORTS",
        "PATH",
    ]))
    .row_highlight_style(selection())
    .block(block);
    frame.render_stateful_widget(table, area, &mut app.worktree_state);
}

fn draw_log(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(log) = &mut app.log else {
        return;
    };
    let height = area.height.saturating_sub(2) as usize;
    app.log_height = height.max(1);
    log.offset = log.offset.min(log.lines.len().saturating_sub(height));
    let end = log.lines.len() - log.offset;
    let start = end.saturating_sub(height);
    let title = if log.offset == 0 {
        format!("logs · {} · following", log.title)
    } else {
        format!("logs · {} · {} below", log.title, log.offset)
    };
    let lines: Vec<Line> = if log.lines.is_empty() {
        vec![Line::styled(
            "no output yet",
            Style::new().fg(Color::DarkGray),
        )]
    } else {
        log.lines
            .range(start..end)
            .map(|line| Line::raw(line.chars().skip(log.column).collect::<String>()))
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(panel(title)), area);
}

fn draw_activity(frame: &mut Frame, app: &App, area: Rect) {
    let Some(activity) = &app.activity else {
        return;
    };
    let (status, color) = match activity.ok {
        None => ("running…", Color::Yellow),
        Some(true) => ("done", Color::Green),
        Some(false) => ("failed", Color::Red),
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Line::from(vec![
            Span::styled(
                format!(" magictree {} ", activity.label),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{status} "), Style::new().fg(color)),
        ]));
    let height = area.height.saturating_sub(2) as usize;
    let start = activity.lines.len().saturating_sub(height);
    let lines: Vec<Line> = activity
        .lines
        .range(start..)
        .map(|line| Line::raw(line.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(notice) = &app.notice {
        frame.render_widget(
            Line::styled(format!(" {notice}"), Style::new().fg(Color::Yellow)),
            area,
        );
        return;
    }
    if let Some(input) = &app.prompt {
        let rest = suggestion(input)
            .map(|word| word[input.len()..].to_string())
            .unwrap_or_default();
        let hint = if input.is_empty() {
            "  services, worktrees, ports, env, logs [service], quit".to_string()
        } else {
            rest
        };
        frame.render_widget(
            Line::from(vec![
                Span::styled(" :", Style::new().fg(lime()).add_modifier(Modifier::BOLD)),
                Span::raw(input.clone()),
                Span::styled(hint, Style::new().fg(Color::DarkGray)),
            ]),
            area,
        );
        let column = area.x + 2 + input.chars().count() as u16;
        frame.set_cursor_position((column.min(area.right().saturating_sub(1)), area.y));
        return;
    }
    // On a roomy screen the keys are in the header, and this row is kept for
    // what the screen has to say: a notice, or the prompt.
    if frame.area().height >= ROOMY {
        return;
    }
    let mut spans = Vec::new();
    for (key, what) in keys(app) {
        spans.push(Span::styled(
            format!(" {key}"),
            Style::new().fg(lime()).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {what} "),
            Style::new().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Line::from(spans), area);
}

fn draw_confirm(frame: &mut Frame, pending: &Pending, area: Rect) {
    let question = format!("run `magictree {}`?  y / n", pending.label);
    let width =
        (question.chars().count().max(pending.about.chars().count()) as u16 + 4).min(area.width);
    let [row] = Layout::vertical([Constraint::Length(4)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(row);
    frame.render_widget(Clear, cell);
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(question),
            Line::styled(pending.about, Style::new().fg(Color::DarkGray)),
        ])
        .centered()
        .block(Block::bordered().border_style(Style::new().fg(Color::Yellow))),
        cell,
    );
}

/// The log being followed: the flag its reader threads watch, and the
/// `docker compose logs` process when the service is a container.
struct Following {
    stop: Arc<AtomicBool>,
    child: Option<Child>,
}

impl Following {
    fn end(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The side of the TUI that touches the world: threads, child processes, and
/// the channel they report on.
struct Driver {
    exe: PathBuf,
    dry_run: bool,
    paths: Paths,
    tx: Sender<Msg>,
    /// Wakes the refresher before its interval is up.
    wake: Sender<()>,
    /// The worktree the refresher reads, and the generation it answers for.
    watched: Arc<Mutex<(Option<PathBuf>, u64)>>,
    following: Option<Following>,
    /// The pid of the command in the pane, while it runs.
    running: Arc<Mutex<Option<u32>>>,
}

impl Driver {
    /// A log is followed only while its section is on screen, however the
    /// screen left it.
    fn settle(&mut self, app: &App) {
        if app.view != View::Logs {
            self.unfollow();
        }
    }

    fn apply(&mut self, effect: Effect, app: &mut App) {
        match effect {
            Effect::Refresh => self.watch(app),
            Effect::ListWorktrees => {
                app.set_worktrees(worktrees::all_worktree_rows(&self.paths));
                // The stack behind the list may have changed with it.
                self.watch(app);
            }
            Effect::Run { args, .. } => {
                let Some(target) = app.target.clone() else {
                    return;
                };
                if let Err(error) = self.run(Some(&target), &args) {
                    let _ = self.tx.send(Msg::Output(format!("{error:#}")));
                    let _ = self.tx.send(Msg::Finished { ok: false });
                }
            }
            Effect::RunIn { args, cwd, .. } => {
                if let Err(error) = self.run(cwd.as_deref(), &args) {
                    let _ = self.tx.send(Msg::Output(format!("{error:#}")));
                    let _ = self.tx.send(Msg::Finished { ok: false });
                }
            }
            Effect::Follow(source) => {
                self.unfollow();
                match follow(source, app.log_generation, self.tx.clone()) {
                    Ok(following) => self.following = Some(following),
                    Err(error) => app.notice = Some(format!("{error:#}")),
                }
            }
            Effect::Unfollow => self.unfollow(),
            Effect::Browse(url) => {
                let opener = if cfg!(target_os = "macos") {
                    "open"
                } else {
                    "xdg-open"
                };
                let opened = Command::new(opener)
                    .arg(&url)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                app.notice = Some(match opened {
                    Ok(_) => format!("opened {url}"),
                    Err(error) => format!("{opener} {url}: {error}"),
                });
            }
        }
    }

    fn watch(&self, app: &App) {
        if let Ok(mut watched) = self.watched.lock() {
            *watched = (app.target.clone(), app.generation);
        }
        let _ = self.wake.send(());
    }

    fn unfollow(&mut self) {
        if let Some(following) = self.following.take() {
            following.end();
        }
    }

    /// Run `magictree <args>`, in `target` when there is one, relaying what it prints. The child
    /// has no terminal, so it prints its plain form and asks nothing.
    fn run(&self, target: Option<&Path>, args: &[String]) -> Result<()> {
        let mut command = Command::new(&self.exe);
        if self.dry_run {
            command.arg("--dry-run");
        }
        command.args(args);
        if let Some(target) = target {
            command.arg("-C").arg(target);
        }
        let mut child = command
            // The pane is for the command's own output; the notice belongs to
            // the shell the TUI was started from.
            .env("MAGICTREE_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {}", self.exe.display()))?;
        if let Ok(mut running) = self.running.lock() {
            *running = Some(child.id());
        }
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, errors) = (self.tx.clone(), self.tx.clone());
        let running = self.running.clone();
        std::thread::spawn(move || {
            let relay = std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let _ = errors.send(Msg::Output(line));
                }
            });
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(Msg::Output(line));
            }
            let _ = relay.join();
            let ok = child.wait().map(|status| status.success()).unwrap_or(false);
            if let Ok(mut running) = running.lock() {
                *running = None;
            }
            let _ = tx.send(Msg::Finished { ok });
        });
        Ok(())
    }

    /// Leave nothing behind: the log follower ends, and a command still
    /// running is interrupted the way ctrl-c would interrupt it in a shell.
    fn shutdown(&mut self) {
        self.unfollow();
        if let Some(pid) = self.running.lock().ok().and_then(|running| *running) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGINT,
            );
        }
    }
}

/// Start following a service's output, sending each line as it arrives.
fn follow(source: LogSource, generation: u64, tx: Sender<Msg>) -> Result<Following> {
    let stop = Arc::new(AtomicBool::new(false));
    match source {
        LogSource::Host(path) => {
            let flag = stop.clone();
            std::thread::spawn(move || tail(&path, generation, &tx, &flag));
            Ok(Following { stop, child: None })
        }
        LogSource::Compose {
            file,
            project,
            container,
        } => {
            let mut child = Command::new("docker")
                .arg("compose")
                .arg("-f")
                .arg(&file)
                .arg("-p")
                .arg(&project)
                .args(["logs", "-f", "--no-color", "--tail", COMPOSE_BACKLOG_LINES])
                .arg(&container)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("running docker compose logs")?;
            for stream in [
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn Read + Send>),
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn Read + Send>),
            ]
            .into_iter()
            .flatten()
            {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        if tx.send(Msg::Log { generation, line }).is_err() {
                            break;
                        }
                    }
                });
            }
            Ok(Following {
                stop,
                child: Some(child),
            })
        }
    }
}

/// Follow a host service's log file until `stop` is set: the end of what is
/// already there, then whatever is appended. A file that does not exist yet is
/// waited for, and one that shrank (`up` truncates it on every start) is read
/// again from the top.
fn tail(path: &Path, generation: u64, tx: &Sender<Msg>, stop: &AtomicBool) {
    let mut offset: Option<u64> = None;
    let mut partial: Vec<u8> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if let Ok(mut file) = std::fs::File::open(path) {
            let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
            let from = match offset {
                None => length.saturating_sub(LOG_BACKLOG_BYTES),
                Some(offset) if offset > length => {
                    partial.clear();
                    0
                }
                Some(offset) => offset,
            };
            // The backlog starts mid-line unless it starts the file.
            let mut skip_first = offset.is_none() && from > 0;
            if length > from && file.seek(SeekFrom::Start(from)).is_ok() {
                let mut buffer = Vec::new();
                if file.take(length - from).read_to_end(&mut buffer).is_ok() {
                    partial.extend_from_slice(&buffer);
                    while let Some(newline) = partial.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<u8> = partial.drain(..=newline).collect();
                        if std::mem::take(&mut skip_first) {
                            continue;
                        }
                        let line = String::from_utf8_lossy(&line).to_string();
                        if tx.send(Msg::Log { generation, line }).is_err() {
                            return;
                        }
                    }
                }
            }
            offset = Some(length.max(from));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Read the watched worktree's stack every [`REFRESH`], or as soon as the
/// screen asks, for as long as the screen exists.
fn refresher(watched: Arc<Mutex<(Option<PathBuf>, u64)>>, wake: Receiver<()>, tx: Sender<Msg>) {
    loop {
        let (target, generation) = match watched.lock() {
            Ok(watched) => watched.clone(),
            Err(_) => return,
        };
        if let Some(target) = target {
            let result = snapshot(&target);
            if tx.send(Msg::Stack { generation, result }).is_err() {
                return;
            }
        }
        match wake.recv_timeout(REFRESH) {
            Ok(()) => {
                // One read answers every request that queued up behind it.
                while wake.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// What the terminal's tab should say: the branch on screen, then the program.
fn tab_title(app: &App) -> String {
    match (app.view, &app.stack) {
        (View::Worktrees, _) | (_, None | Some(Err(_))) => "magictree".to_string(),
        (_, Some(Ok(snapshot))) => format!("{} — magictree", snapshot.tab),
    }
}

/// Open the TUI on the worktree `cwd` is in, or on the worktree list when it
/// is in none. With `dry_run`, every command it starts is a dry run.
pub fn run(cwd: &Path, dry_run: bool) -> Result<()> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!(
            "the magictree screen needs a terminal\n\n`magictree status` prints a worktree's \
             services, and `magictree list --all` every worktree"
        );
    }
    let paths = Paths::new()?;
    let target = Repo::open_optional(cwd)?.map(|repo| repo.worktree_root);
    let mut app = App::new(target, dry_run);

    let (tx, rx) = mpsc::channel();
    let (wake, woken) = mpsc::channel();
    let watched = Arc::new(Mutex::new((app.target.clone(), app.generation)));
    {
        let (watched, tx) = (watched.clone(), tx.clone());
        std::thread::spawn(move || refresher(watched, woken, tx));
    }
    let mut driver = Driver {
        exe: std::env::current_exe().context("locating the magictree binary")?,
        dry_run,
        paths,
        tx,
        wake,
        watched,
        following: None,
        running: Arc::new(Mutex::new(None)),
    };
    if app.view == View::Worktrees {
        driver.apply(Effect::ListWorktrees, &mut app);
    }

    let mut terminal = ratatui::try_init().context("opening the terminal")?;
    // Save the tab's title on the terminal's own stack and put it back on the
    // way out; a terminal without the stack ignores both.
    print!("\u{1b}[22;0t");
    let mut title = String::new();
    let outcome = (|| -> Result<()> {
        while !app.quit {
            while let Ok(msg) = rx.try_recv() {
                if let Some(effect) = app.on_msg(msg) {
                    driver.apply(effect, &mut app);
                }
            }
            let wanted = tab_title(&app);
            if wanted != title {
                ratatui::crossterm::execute!(
                    std::io::stdout(),
                    ratatui::crossterm::terminal::SetTitle(&wanted)
                )?;
                title = wanted;
            }
            terminal.draw(|frame| draw(frame, &mut app))?;
            if event::poll(TICK)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        if let Some(effect) = app.on_key(key) {
                            driver.apply(effect, &mut app);
                        }
                        driver.settle(&app);
                    }
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    print!("\u{1b}[23;0t");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    driver.shutdown();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn row(qual: &str, state: &str, port: Option<u16>) -> ServiceRow {
        ServiceRow {
            qual: qual.to_string(),
            runtime: "host",
            state: state.to_string(),
            port,
            url: port.map(|port| format!("http://localhost:{port}")),
            alias: None,
            log: LogSource::Host(PathBuf::from(format!("/log/{qual}.log"))),
        }
    }

    fn app_with(rows: Vec<ServiceRow>) -> App {
        let mut app = App::new(Some(PathBuf::from("/repo")), false);
        app.on_msg(Msg::Stack {
            generation: 0,
            result: Ok(Snapshot {
                title: "repo · main".to_string(),
                tab: "feat/billing".to_string(),
                rows,
                ports: vec![PortRow {
                    name: "web".to_string(),
                    port: 3000,
                    url: "http://localhost:3000".to_string(),
                    alias: None,
                }],
                assigned: true,
                env: Ok(vec![("PORT".to_string(), "3000".to_string())]),
            }),
        });
        app
    }

    fn press(app: &mut App, code: KeyCode) -> Option<Effect> {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One colour of the screenshot, on the logo's own ink and paper.
    fn hex(color: Color, foreground: bool) -> String {
        match color {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            Color::Indexed(_) => "#c8f53c".to_string(),
            Color::Black => "#101410".to_string(),
            Color::White => "#f1f3ea".to_string(),
            Color::DarkGray => "#7d8577".to_string(),
            Color::Green => "#8fd16a".to_string(),
            Color::Red => "#e5645f".to_string(),
            Color::Yellow => "#e8c468".to_string(),
            _ if foreground => "#d5d9cc".to_string(),
            _ => "#101410".to_string(),
        }
    }

    /// Writes `docs/tui.svg`, the README's picture of the screen, from the
    /// renderer itself: `cargo test --lib readme_screenshot -- --ignored`.
    /// Blocks and box lines are drawn as shapes, so the picture does not depend
    /// on which monospace font the reader's browser has.
    #[test]
    #[ignore = "regenerates docs/tui.svg"]
    fn readme_screenshot() {
        std::env::set_var("COLORTERM", "truecolor");
        let home = crate::paths::home().expect("home");
        let mut app = App::new(Some(home.join("code/shop-feat-billing")), false);
        let service = |qual: &str, runtime: &'static str, state: &str, port: Option<u16>| {
            let mut service = row(qual, state, port);
            service.runtime = runtime;
            service
        };
        let mut web = service("web", "host", "running (pid 48211)", Some(4100));
        web.alias = Some("http://feat-billing.localhost:4100".to_string());
        app.on_msg(Msg::Stack {
            generation: 0,
            result: Ok(Snapshot {
                title: "shop · feat-billing".to_string(),
                tab: "feat/billing".to_string(),
                rows: vec![
                    service("postgres", "compose", "running (healthy)", Some(4102)),
                    service("redis", "compose", "running (healthy)", Some(4103)),
                    service("mailpit", "compose", "exited", Some(4104)),
                    service("api", "host", "running (pid 48190)", Some(4101)),
                    web,
                    service("worker", "host", "stopped", None),
                ],
                ports: Vec::new(),
                assigned: true,
                env: Ok(Vec::new()),
            }),
        });
        for _ in 0..4 {
            press(&mut app, KeyCode::Down);
        }

        let (columns, rows) = (96u16, 20u16);
        let mut terminal = Terminal::new(TestBackend::new(columns, rows)).expect("terminal");
        terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();

        let (w, h, pad) = (9.0f32, 20.0f32, 18.0f32);
        let (width, height) = (columns as f32 * w + 2.0 * pad, rows as f32 * h + 2.0 * pad);
        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width} {height}\" \
             width=\"{width}\" height=\"{height}\" role=\"img\" \
             aria-label=\"The magictree TUI: a worktree's services, their state and URLs\">\n\
             <rect width=\"{width}\" height=\"{height}\" rx=\"10\" fill=\"#101410\"/>\n\
             <g font-family=\"ui-monospace, SFMono-Regular, Menlo, Consolas, monospace\" \
             font-size=\"15\" shape-rendering=\"crispEdges\">\n"
        );
        for y in 0..rows {
            let top = pad + y as f32 * h;
            // Backgrounds first, a run of one colour at a time, so the text
            // and shapes of the row all sit on top of them.
            let mut x = 0u16;
            while x < columns {
                let bg = buffer[(x, y)].bg;
                let start = x;
                while x < columns && buffer[(x, y)].bg == bg {
                    x += 1;
                }
                if !matches!(bg, Color::Reset) {
                    svg.push_str(&format!(
                        "<rect x=\"{}\" y=\"{top}\" width=\"{}\" height=\"{h}\" fill=\"{}\"/>",
                        pad + start as f32 * w,
                        (x - start) as f32 * w,
                        hex(bg, false)
                    ));
                }
            }
            let mut x = 0u16;
            while x < columns {
                let cell = &buffer[(x, y)];
                let left = pad + x as f32 * w;
                let fg = hex(cell.fg, true);
                let (mid_x, mid_y) = (left + w / 2.0, top + h / 2.0);
                let (right, bottom) = (left + w, top + h);
                let line = |points: &str| {
                    format!(
                        "<path d=\"{points}\" stroke=\"{fg}\" stroke-width=\"1\" fill=\"none\"/>"
                    )
                };
                let shape = match cell.symbol() {
                    "█" => Some(format!(
                        "<rect x=\"{left}\" y=\"{top}\" width=\"{w}\" height=\"{h}\" fill=\"{fg}\"/>"
                    )),
                    "▀" => Some(format!(
                        "<rect x=\"{left}\" y=\"{top}\" width=\"{w}\" height=\"{}\" fill=\"{fg}\"/>",
                        h / 2.0
                    )),
                    "▄" => Some(format!(
                        "<rect x=\"{left}\" y=\"{mid_y}\" width=\"{w}\" height=\"{}\" fill=\"{fg}\"/>",
                        h / 2.0
                    )),
                    "─" => Some(line(&format!("M{left} {mid_y}H{right}"))),
                    "│" => Some(line(&format!("M{mid_x} {top}V{bottom}"))),
                    "┌" => Some(line(&format!("M{mid_x} {bottom}V{mid_y}H{right}"))),
                    "┐" => Some(line(&format!("M{left} {mid_y}H{mid_x}V{bottom}"))),
                    "└" => Some(line(&format!("M{mid_x} {top}V{mid_y}H{right}"))),
                    "┘" => Some(line(&format!("M{left} {mid_y}H{mid_x}V{top}"))),
                    _ => None,
                };
                if let Some(shape) = shape {
                    svg.push_str(&shape);
                    x += 1;
                    continue;
                }
                if cell.symbol().trim().is_empty() {
                    x += 1;
                    continue;
                }
                // One word in one style, pinned to the cells it covers: a
                // renderer may collapse spaces, so no run ever contains one.
                let bold = cell.modifier.contains(Modifier::BOLD);
                let start = x;
                let mut text = String::new();
                while x < columns {
                    let next = &buffer[(x, y)];
                    let drawn = matches!(
                        next.symbol(),
                        "█" | "▀" | "▄" | "─" | "│" | "┌" | "┐" | "└" | "┘"
                    );
                    if drawn
                        || next.symbol().trim().is_empty()
                        || next.fg != cell.fg
                        || next.modifier.contains(Modifier::BOLD) != bold
                    {
                        break;
                    }
                    text.push_str(next.symbol());
                    x += 1;
                }
                let escaped = text
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                svg.push_str(&format!(
                    "<text x=\"{}\" y=\"{}\" textLength=\"{}\" fill=\"{fg}\"{}>{escaped}</text>",
                    pad + start as f32 * w,
                    top + h * 0.72,
                    text.chars().count() as f32 * w,
                    if bold { " font-weight=\"700\"" } else { "" }
                ));
            }
            svg.push('\n');
        }
        svg.push_str("</g>\n</svg>\n");
        let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
        std::fs::create_dir_all(&docs).expect("docs dir");
        std::fs::write(docs.join("tui.svg"), svg).expect("write the screenshot");
    }

    #[test]
    fn plain_strips_what_would_move_the_cursor() {
        assert_eq!(plain("\u{1b}[32mready\u{1b}[0m in 3ms\r\n"), "ready in 3ms");
        assert_eq!(plain("10%\r50%\r100%"), "100%");
        assert_eq!(plain("a\tb"), "a    b");
        assert_eq!(plain("\u{1b}]0;title\u{7}text\u{8}"), "text");
        assert_eq!(plain("\u{1b}]8;;http://x\u{1b}\\link"), "link");
    }

    #[test]
    fn services_view_shows_each_service_and_its_keys() {
        let mut app = app_with(vec![
            row("web", "running (pid 4242)", Some(3000)),
            row("worker", "stopped", None),
        ]);
        let text = screen(&mut app, 100, 12);
        assert!(text.contains("repo · main"), "{text}");
        assert!(text.contains("running (pid 4242)"), "{text}");
        assert!(text.contains("http://localhost:3000"), "{text}");
        assert!(text.contains("worker"), "{text}");
        assert!(text.contains("restart"), "{text}");
        assert_eq!(tab_title(&app), "feat/billing — magictree");
    }

    #[test]
    fn a_roomy_screen_opens_with_the_mark() {
        let mut app = app_with(vec![row("web", "stopped", None)]);
        let text = screen(&mut app, 100, 22);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].starts_with(&format!(" ▄▄▄▄▄  magictree {}", env!("CARGO_PKG_VERSION"))),
            "{text}"
        );
        assert!(lines[1].starts_with(" █ █ █  repo · main"), "{text}");
        assert!(lines[2].starts_with(" ▀ ▀ ▀  /repo "), "{text}");
        let home = crate::paths::home().expect("home");
        assert_eq!(
            tilde(&home.join(".herdr/worktrees/deceris/threats")),
            "~/.herdr/worktrees/deceris/threats"
        );
        assert_eq!(tilde(&home), "~");
        // The keys stand beside it, three to a column, and leave the footer.
        assert!(
            lines[0].contains("<l> logs") && lines[0].contains("<r> restart"),
            "{text}"
        );
        assert!(
            lines[1].contains("<u> up") && lines[1].contains("<d> down"),
            "{text}"
        );
        assert!(
            lines[2].contains("<s> start") && lines[2].contains("<o> open"),
            "{text}"
        );
        assert!(
            lines[0].contains("<:> command") && lines[1].contains("<q> quit"),
            "{text}"
        );
        assert!(!lines[lines.len() - 1].contains("restart"), "{text}");
    }

    #[test]
    fn selection_follows_the_service_across_a_refresh() {
        let mut app = app_with(vec![
            row("db", "stopped", None),
            row("web", "stopped", None),
        ]);
        press(&mut app, KeyCode::Down);
        app.on_msg(Msg::Stack {
            generation: 0,
            result: Ok(Snapshot {
                title: String::new(),
                tab: String::new(),
                ports: Vec::new(),
                assigned: false,
                env: Ok(Vec::new()),
                rows: vec![
                    row("cache", "stopped", None),
                    row("db", "stopped", None),
                    row("web", "stopped", None),
                ],
            }),
        });
        assert_eq!(app.selected().map(|row| row.qual.as_str()), Some("web"));
    }

    #[test]
    fn a_repository_without_a_manifest_is_shown_the_way_in() {
        let dir = tempfile::tempdir().expect("temp dir");
        let problem = snapshot(dir.path()).expect_err("no manifest");
        assert_eq!(problem, Problem::NoManifest { hint: None });

        let mut app = App::new(Some(dir.path().to_path_buf()), false);
        app.on_msg(Msg::Stack {
            generation: 0,
            result: Err(problem),
        });
        for digit in ['1', '3', '4'] {
            press(&mut app, KeyCode::Char(digit));
            let text = screen(&mut app, 100, 16);
            assert!(text.contains("No stack in this repository yet"), "{text}");
            assert!(
                text.contains("magictree discover && magictree init"),
                "{text}"
            );
            assert!(!text.contains("or any parent directory"), "{text}");
        }
    }

    #[test]
    fn a_stale_stack_is_dropped() {
        let mut app = app_with(vec![row("web", "stopped", None)]);
        app.generation = 1;
        app.on_msg(Msg::Stack {
            generation: 0,
            result: Err(Problem::Failed("old".to_string())),
        });
        assert!(matches!(app.stack, Some(Ok(_))));
    }

    #[test]
    fn keys_run_the_commands_a_shell_would() {
        let mut app = app_with(vec![row("api:web", "stopped", None)]);
        assert_eq!(
            press(&mut app, KeyCode::Char('r')),
            Some(Effect::Run {
                label: "restart api:web".to_string(),
                args: vec!["restart".to_string(), "api:web".to_string()],
            })
        );
        // One command at a time: the second is refused while the first runs.
        assert_eq!(press(&mut app, KeyCode::Char('u')), None);
        assert!(app.notice.is_some());
        assert_eq!(
            app.on_msg(Msg::Finished { ok: true }),
            Some(Effect::Refresh)
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('s')),
            Some(Effect::Run {
                label: "up api:web".to_string(),
                args: vec!["up".to_string(), "api:web".to_string()],
            })
        );
    }

    #[test]
    fn down_waits_for_a_yes() {
        let mut app = app_with(vec![row("web", "running (pid 1)", None)]);
        assert_eq!(press(&mut app, KeyCode::Char('d')), None);
        assert!(screen(&mut app, 80, 12).contains("magictree down"));
        assert!(screen(&mut app, 80, 12).contains("in this worktree"));
        assert_eq!(press(&mut app, KeyCode::Char('n')), None);
        assert!(app.confirm.is_none() && app.activity.is_none());

        press(&mut app, KeyCode::Char('d'));
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            Some(Effect::Run {
                label: "down".to_string(),
                args: vec!["down".to_string()],
            })
        );
    }

    #[test]
    fn quitting_waits_for_a_running_command() {
        let mut app = app_with(vec![row("web", "stopped", None)]);
        press(&mut app, KeyCode::Char('u'));
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit);
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn a_log_holds_its_place_while_scrolled_up() {
        let mut app = app_with(vec![row("web", "running (pid 1)", None)]);
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Some(Effect::Follow(LogSource::Host(PathBuf::from(
                "/log/web.log"
            ))))
        );
        let generation = app.log_generation;
        for n in 0..30 {
            app.on_msg(Msg::Log {
                generation,
                line: format!("line {n}"),
            });
        }
        // 12 rows: header, tabs, footer, two borders, seven lines of log.
        let text = screen(&mut app, 60, 12);
        assert!(
            text.contains("line 29") && text.contains("following"),
            "{text}"
        );
        assert!(!text.contains("line 21"), "{text}");

        press(&mut app, KeyCode::Up);
        app.on_msg(Msg::Log {
            generation,
            line: "line 30".to_string(),
        });
        let text = screen(&mut app, 60, 12);
        assert!(
            text.contains("line 28") && !text.contains("line 30"),
            "{text}"
        );

        // A line from a log the screen has left never reaches the next one.
        assert_eq!(press(&mut app, KeyCode::Esc), Some(Effect::Unfollow));
        press(&mut app, KeyCode::Enter);
        app.on_msg(Msg::Log {
            generation,
            line: "late".to_string(),
        });
        assert!(app.log.as_ref().is_some_and(|log| log.lines.is_empty()));
    }

    fn type_command(app: &mut App, text: &str) -> Option<Effect> {
        press(app, KeyCode::Char(':'));
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
        press(app, KeyCode::Enter)
    }

    #[test]
    fn a_colon_command_switches_sections() {
        let mut app = app_with(vec![row("web", "stopped", Some(3000))]);
        assert_eq!(type_command(&mut app, "ports"), None);
        assert_eq!(app.view, View::Ports);
        let text = screen(&mut app, 90, 12);
        assert!(text.contains("NAME") && text.contains("3000"), "{text}");

        assert_eq!(type_command(&mut app, "e"), None);
        assert_eq!(app.view, View::Env);
        assert!(screen(&mut app, 90, 12).contains("VARIABLE"));

        assert_eq!(type_command(&mut app, "wt"), Some(Effect::ListWorktrees));
        assert_eq!(app.view, View::Worktrees);

        assert_eq!(
            type_command(&mut app, "logs web"),
            Some(Effect::Follow(LogSource::Host(PathBuf::from(
                "/log/web.log"
            ))))
        );
        assert_eq!(app.view, View::Logs);
        // Leaving the log by any road drops it.
        assert_eq!(type_command(&mut app, "svc"), None);
        assert!(app.view == View::Services && app.log.is_none());

        assert_eq!(type_command(&mut app, "pods"), None);
        assert!(app
            .notice
            .as_deref()
            .is_some_and(|n| n.contains("unknown section 'pods'")));
    }

    #[test]
    fn digits_and_tab_walk_the_sections() {
        let mut app = app_with(vec![row("web", "stopped", None)]);
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(app.view, View::Ports);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::Env);
        press(&mut app, KeyCode::BackTab);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.view, View::Worktrees);
        let text = screen(&mut app, 90, 12);
        assert!(
            text.contains("1 services") && text.contains("5 logs"),
            "{text}"
        );
    }

    #[test]
    fn the_prompt_completes_and_only_navigates() {
        let mut app = app_with(vec![row("api:web", "stopped", None)]);
        assert_eq!(suggestion("wo"), Some("worktrees"));
        assert_eq!(suggestion("q"), Some("quit"));
        assert_eq!(suggestion("logs w"), None);

        press(&mut app, KeyCode::Char(':'));
        press(&mut app, KeyCode::Char('p'));
        assert!(screen(&mut app, 90, 12).contains(":ports"));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.prompt.as_deref(), Some("ports"));
        // Typing in the prompt never reaches the section's own keys.
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit);
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.view, View::Ports);

        // What changes a stack is a key, never a word: the prompt says which.
        for word in ["up", "down", "restart api:web"] {
            assert_eq!(type_command(&mut app, word), None);
            assert!(app.activity.is_none() && app.confirm.is_none());
            assert!(app.notice.as_deref().is_some_and(|n| n.contains("u up")));
        }
        type_command(&mut app, "q");
        assert!(app.quit);
    }

    #[test]
    fn up_and_down_answer_in_every_section_about_the_stack() {
        let mut app = app_with(vec![row("web", "stopped", Some(3000))]);
        for digit in ['3', '4'] {
            press(&mut app, KeyCode::Char(digit));
            assert_eq!(press(&mut app, KeyCode::Char('d')), None);
            assert!(app.confirm.is_some());
            press(&mut app, KeyCode::Esc);
            assert_eq!(
                press(&mut app, KeyCode::Char('u')),
                Some(Effect::Run {
                    label: "up".to_string(),
                    args: vec!["up".to_string()],
                })
            );
            app.on_msg(Msg::Output("[1/1] service · web".to_string()));
            // The output is shown where the key was pressed.
            assert!(screen(&mut app, 90, 16).contains("[1/1] service · web"));
            app.on_msg(Msg::Finished { ok: true });
        }
        // `l` is the key for a service's log, as `:logs` is its word.
        press(&mut app, KeyCode::Char('1'));
        assert!(matches!(
            press(&mut app, KeyCode::Char('l')),
            Some(Effect::Follow(_))
        ));
    }

    #[test]
    fn an_alias_is_shown_in_the_urls_place_and_is_what_opens() {
        let mut aliased = row("web", "stopped", Some(3000));
        aliased.alias = Some("http://billing.localhost:3000".to_string());
        let mut app = app_with(vec![aliased, row("api", "stopped", Some(4000))]);
        let text = screen(&mut app, 110, 12);
        assert!(!text.contains("ALIAS"), "{text}");
        assert!(text.contains("http://billing.localhost:3000"), "{text}");
        assert!(!text.contains("http://localhost:3000"), "{text}");
        assert!(text.contains("http://localhost:4000"), "{text}");
        assert_eq!(
            press(&mut app, KeyCode::Char('o')),
            Some(Effect::Browse("http://billing.localhost:3000".to_string()))
        );
        press(&mut app, KeyCode::Down);
        assert_eq!(
            press(&mut app, KeyCode::Char('o')),
            Some(Effect::Browse("http://localhost:4000".to_string()))
        );
    }

    #[test]
    fn worktrees_are_removed_and_reclaimed_from_their_list() {
        let record = |id: &str, live: bool| RecordedWorktree {
            repo_key: "abc".to_string(),
            repo_path: Some(PathBuf::from("/code/repo")),
            worktree_id: id.to_string(),
            path: Some(PathBuf::from(if id == "main" {
                "/code/repo".to_string()
            } else {
                format!("/code/repo-{id}")
            })),
            ports: "4100-4109".to_string(),
            live: Some(live),
        };
        let mut app = App::new(Some(PathBuf::from("/code/repo-billing")), false);
        press(&mut app, KeyCode::Char('2'));
        app.set_worktrees(vec![
            record("main", true),
            record("old", false),
            record("billing", true),
        ]);

        // Neither the repository itself nor a checkout already gone is removed.
        assert_eq!(press(&mut app, KeyCode::Char('x')), None);
        assert!(app.confirm.is_none() && app.notice.is_some());
        press(&mut app, KeyCode::Down);
        assert_eq!(press(&mut app, KeyCode::Char('x')), None);
        assert!(app.confirm.is_none());
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("gc")));

        press(&mut app, KeyCode::Down);
        // Asking is not removing: a no leaves the stack on screen alone.
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.target, Some(PathBuf::from("/code/repo-billing")));
        assert_eq!(press(&mut app, KeyCode::Char('x')), None);
        let text = screen(&mut app, 110, 14);
        assert!(text.contains("magictree rm /code/repo-billing"), "{text}");
        assert!(text.contains("the branch is kept"), "{text}");
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            Some(Effect::RunIn {
                label: "rm /code/repo-billing".to_string(),
                args: vec!["rm".to_string(), "/code/repo-billing".to_string()],
                cwd: Some(PathBuf::from("/code/repo")),
            })
        );
        // The stack that was on screen is the one being removed.
        assert_eq!(app.target, None);
        assert_eq!(
            app.on_msg(Msg::Finished { ok: true }),
            Some(Effect::ListWorktrees)
        );

        assert_eq!(press(&mut app, KeyCode::Char('c')), None);
        assert!(screen(&mut app, 110, 14).contains("magictree gc --all"));
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            Some(Effect::RunIn {
                label: "gc --all".to_string(),
                args: vec!["gc".to_string(), "--all".to_string()],
                cwd: None,
            })
        );
    }

    #[test]
    fn a_gone_worktree_cannot_be_opened() {
        let mut app = App::new(None, false);
        assert_eq!(app.view, View::Worktrees);
        let record = |id: &str, live: bool| RecordedWorktree {
            repo_key: "abc".to_string(),
            repo_path: Some(PathBuf::from("/code/repo")),
            worktree_id: id.to_string(),
            path: Some(PathBuf::from(format!("/code/repo-{id}"))),
            ports: "4100-4109".to_string(),
            live: Some(live),
        };
        app.set_worktrees(vec![record("old", false), record("billing", true)]);
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        assert_eq!(app.view, View::Worktrees);
        assert!(app.notice.is_some());

        press(&mut app, KeyCode::Down);
        assert_eq!(press(&mut app, KeyCode::Enter), Some(Effect::Refresh));
        assert_eq!(app.view, View::Services);
        assert_eq!(app.target, Some(PathBuf::from("/code/repo-billing")));
        assert_eq!(app.generation, 1);
    }

    #[test]
    fn tail_reads_the_backlog_then_what_is_appended() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("web.log");
        std::fs::write(&path, "one\ntwo\npart").expect("write");
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (path, stop) = (path.clone(), stop.clone());
            std::thread::spawn(move || tail(&path, 7, &tx, &stop))
        };
        let next = || match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Msg::Log {
                generation: 7,
                line,
            }) => line,
            _ => panic!("expected a log line"),
        };
        assert_eq!(next(), "one\n");
        assert_eq!(next(), "two\n");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        writeln!(file, "ial").expect("append");
        assert_eq!(next(), "partial\n");
        // `up` truncates the log on a restart: the new run is read from its top.
        std::fs::write(&path, "new\n").expect("truncate");
        assert_eq!(next(), "new\n");
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("tail thread");
    }
}
