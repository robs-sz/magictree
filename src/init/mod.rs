//! `init`: turn a discovery report plus answers into manifests.
//!
//! This is a pure function. It never reads the repository again and never runs
//! anything, so the same report and answers always produce byte-identical
//! output. The wizard and an agent both go through here, which is what makes
//! their results agree.

pub mod wizard;

use crate::discover::extractors::{install_command, install_inputs, parse_port_mapping};
use crate::discover::ports;
use crate::discover::report::*;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One manifest to write: everything that precedes its first service, then one
/// block per service.
///
/// Kept apart rather than as one string so an existing manifest can be added to
/// without being rewritten: the blocks an update appends are exactly the ones
/// the plan would write into a new file.
#[derive(Debug)]
pub struct Generated {
    pub path: PathBuf,
    /// Everything before the first `[[services]]` table.
    pub header: String,
    /// One entry per service, in the order they are written.
    pub services: Vec<ServiceBlock>,
    /// The answers this manifest records, so a later run asks only about what
    /// the repository has since added. Empty for every manifest but the one at
    /// the repository root, which is where `init` was told to work.
    pub stored: Stored,
    /// True for the manifest that carries the record.
    pub records_answers: bool,
}

impl Generated {
    /// The whole manifest, as written when the file does not exist yet.
    pub fn contents(&self) -> String {
        let mut out = self.header.clone();
        for service in &self.services {
            out.push('\n');
            out.push_str(&service.text);
        }
        out
    }
}

/// One `[[services]]` table, with any comment lines that introduce it.
#[derive(Debug, Clone)]
pub struct ServiceBlock {
    pub id: String,
    /// The step it starts, in the `<runner>:<script>` form the run answers use.
    /// `None` for a compose service, which starts a container rather than a step.
    pub step: Option<String>,
    pub text: String,
}

/// What happened to one manifest.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// The file did not exist and was written whole.
    Created(PathBuf),
    /// The file existed and changed.
    Updated {
        path: PathBuf,
        added: Vec<String>,
        /// `None` when the record was already up to date. `Some` holds the
        /// answers that differed from the ones the manifest was built with: an
        /// additive update cannot apply those to a service already written, so
        /// the caller can say as much.
        recorded: Option<Vec<String>>,
    },
    /// The file existed and already declared every service the answers imply.
    Unchanged(PathBuf),
}

impl Applied {
    pub fn path(&self) -> &Path {
        match self {
            Applied::Created(path) | Applied::Unchanged(path) => path,
            Applied::Updated { path, .. } => path,
        }
    }
}

/// Build every manifest the report and answers imply.
pub fn plan(
    report: &Report,
    answers: &AnswerSet,
    record: &Stored,
    repo_root: &Path,
) -> Result<Vec<Generated>> {
    if answers.answers_version != ANSWERS_VERSION {
        bail!(
            "answers version {} is not supported (expected {ANSWERS_VERSION})",
            answers.answers_version
        );
    }
    if !answers.report_hash.is_empty() && answers.report_hash != report.report_hash {
        bail!(
            "answers were computed for report {} but this report is {}; re-run discovery",
            answers.report_hash,
            report.report_hash
        );
    }

    let unanswered: Vec<&str> = report
        .unknowns
        .iter()
        .filter(|unknown| !answers.answers.contains_key(&unknown.id))
        .map(|unknown| unknown.id.as_str())
        .collect();
    if !unanswered.is_empty() {
        bail!(
            "unanswered unknowns: {}\nrun `magictree init` interactively, or supply --answers",
            unanswered.join(", ")
        );
    }

    let compose = compose_facts(report);
    let members = resolve_members(report, answers)?;
    let shared = answer_list(answers, "compose.shared")?;
    let exposed = answer_list(answers, "compose.expose")?;
    // The record lives in the manifest at the repository root, so the next run
    // can ask only about what the repository has since gained.
    let stored = record.clone();

    let mut generated = Vec::new();

    // Compose services live in the manifest at the repository root, because
    // that is where their compose file's paths resolve from.
    let single_app_at_root = report.apps.len() == 1 && report.apps[0].dir == ".";
    let manage_compose = !shared.is_empty() || !exposed.is_empty();
    let workspace_layer = report.apps.len() > 1 || (manage_compose && !single_app_at_root);

    if workspace_layer {
        let mut out = manifest_head(Some(&stored));
        out.push_str("\n[workspace]\napps = [");
        for (index, dir) in members.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "\"{dir}\"");
        }
        out.push_str("]\n");
        generated.push(Generated {
            path: repo_root.join("magictree.toml"),
            header: out,
            services: compose_services(&compose, report, &shared, &exposed)?,
            stored: stored.clone(),
            records_answers: true,
        });
    }

    // App manifests.
    for app in &report.apps {
        if report.apps.len() > 1 && !members.contains(&app.dir) {
            continue;
        }
        let app_root = if app.dir == "." {
            repo_root.to_path_buf()
        } else {
            repo_root.join(&app.dir)
        };
        let shares_root_manifest = app.dir == ".";
        let mut out = manifest_head(shares_root_manifest.then_some(&stored));
        out.push_str("\n[app]\n");
        let _ = writeln!(out, "id = \"{}\"", app.id);
        let mut services: Vec<ServiceBlock> = Vec::new();

        let setup = answers
            .answers
            .get(&format!("{}.setup", app.id))
            .map(|answer| {
                answer
                    .as_many()
                    .into_iter()
                    .filter(|value| value != "none")
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(section) = bootstrap_section(&app_root, &setup)? {
            out.push('\n');
            out.push_str(&section);
        }

        let run_spec = answers
            .answers
            .get(&format!("{}.run", app.id))
            .and_then(|answer| answer.as_one())
            .filter(|spec| *spec != "skip");
        if let Some(spec) = run_spec {
            let (kind, rest) = spec
                .split_once(':')
                .with_context(|| format!("malformed run answer '{spec}'"))?;
            let mut block = String::from("[[services]]\n");
            let _ = writeln!(block, "id = \"{}\"", app.id);
            if kind == "command" {
                // Running the command directly leaves out whatever the task
                // runner's recipe prepared, so record what that was. The service
                // may still start without these, so they are not guessed at.
                if let Some(parameters) = exported_parameters_for(report, &app.id) {
                    if !parameters.is_empty() {
                        let _ = writeln!(
                            block,
                            "# the recipe this command came from also set: {}",
                            parameters.join(", ")
                        );
                        block.push_str("# add them under [env] if the service needs them\n");
                    }
                }
            }
            if serves_storybook(report, &app.id, spec) {
                // The app's own server is Storybook: it gets the same port
                // handling as the dedicated service below, because Storybook
                // reads no environment variable for it.
                block.push_str(&storybook_target(kind, rest)?);
            } else {
                block.push_str(&target_line(kind, rest, spec)?);
                let port_variable = answers
                    .answers
                    .get(&format!("{}.port_env", app.id))
                    .and_then(|answer| answer.as_one())
                    .filter(|name| !name.is_empty() && *name != "none")
                    .unwrap_or("PORT");
                let _ = writeln!(block, "port = {{ env = \"{port_variable}\" }}");
            }
            block.push_str("health = { http = \"/\", timeout = 120 }\n");
            services.push(ServiceBlock {
                id: app.id.clone(),
                step: Some(spec.to_string()),
                text: block,
            });
        }

        // Storybook runs beside the app rather than instead of it, so it is a
        // service of its own on the worktree's own port.
        let storybook_answer = answers
            .answers
            .get(&format!("{}.storybook", app.id))
            .and_then(|answer| answer.as_one())
            .filter(|spec| *spec != "skip");
        if let Some(spec) = storybook_answer.filter(|spec| Some(*spec) != run_spec) {
            let (kind, rest) = spec
                .split_once(':')
                .with_context(|| format!("malformed storybook answer '{spec}'"))?;
            // An app that is itself named `storybook` already owns that id.
            let id = if app.id == "storybook" {
                "storybook-dev"
            } else {
                "storybook"
            };
            let mut block = String::new();
            if let Some(comment) = storybook_mcp_comment(report, &app.id) {
                block.push_str(comment);
                block.push('\n');
            }
            block.push_str("[[services]]\n");
            let _ = writeln!(block, "id = \"{id}\"");
            block.push_str(&storybook_target(kind, rest)?);
            block.push_str("health = { http = \"/\", timeout = 120 }\n");
            services.push(ServiceBlock {
                id: id.to_string(),
                step: Some(spec.to_string()),
                text: block,
            });
        }

        if !workspace_layer {
            // A single app at the repository root owns the compose services too.
            services.extend(compose_services(&compose, report, &shared, &exposed)?);
        }
        let _ = shares_root_manifest;

        generated.push(Generated {
            path: app_root.join("magictree.toml"),
            header: out,
            services,
            stored: if shares_root_manifest {
                stored.clone()
            } else {
                Stored::default()
            },
            records_answers: shares_root_manifest,
        });
    }

    generated.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(generated)
}

/// Storybook's port variable and the port the dev server listens on when nothing
/// overrides it. Storybook reads no environment variable of its own: the
/// variable carries the allocated port onto the command line.
pub(crate) const STORYBOOK_PORT: &str = "STORYBOOK_PORT";
const STORYBOOK_PREFER: u16 = 6006;

/// True when this run answer starts Storybook itself, so the service that runs
/// it needs Storybook's port handling rather than a variable nothing reads.
fn serves_storybook(report: &Report, app: &str, spec: &str) -> bool {
    report.storybook_scripts(app).contains(&spec)
}

/// A note naming the MCP endpoint the dev server answers on, for the agent that
/// has to find it. Only written when the addon that serves it is installed.
fn storybook_mcp_comment(report: &Report, app: &str) -> Option<&'static str> {
    if !report.storybook_has_mcp(app) {
        return None;
    }
    Some("# @storybook/addon-mcp answers MCP at /mcp on this service\n")
}

/// The target and port for a service that serves Storybook.
///
/// Storybook takes its port from `-p`/`--port` and from nothing else, so the
/// allocated port is passed on the command line. Appending it after the script's
/// own arguments makes it win over whatever the repository's script pins, which
/// is what keeps a second worktree off the first one's port. `--no-open` stops
/// every `up` from opening a browser on the machine running it.
fn storybook_target(kind: &str, script: &str) -> Result<String> {
    let port = format!("${{{STORYBOOK_PORT}:-{STORYBOOK_PREFER}}}");
    let mut out = String::new();
    match kind {
        // `npm run` consumes everything that is not behind the separator; pnpm,
        // yarn and bun forward the rest themselves.
        "npm" => writeln!(
            out,
            "target = {{ kind = \"npm\", script = \"{script}\", args = [\"--\", \"-p\", \"{port}\", \"--no-open\"] }}"
        )?,
        "pnpm" => writeln!(
            out,
            "target = {{ kind = \"pnpm\", script = \"{script}\", args = [\"-p\", \"{port}\", \"--no-open\"] }}"
        )?,
        // Neither is a first-class target, so the command is written out, as it
        // is for any other yarn or bun run answer.
        "yarn" | "bun" => {
            writeln!(out, "command = \"{kind} run {script} -p {port} --no-open\"")?
        }
        other => bail!("unsupported Storybook answer (unknown runner '{other}')"),
    }
    let _ = writeln!(
        out,
        "port = {{ env = \"{STORYBOOK_PORT}\", prefer = {STORYBOOK_PREFER} }}"
    );
    Ok(out)
}

/// The comment that explains the recorded answers, and the keys they sit under.
const ANSWERS_COMMENT: &str =
    "# Recorded by `magictree init`; replayed so only new questions are asked.\n";
const ANSWERS_KEY: &str = "answers";
const DECLINED_KEY: &str = "declined";

/// What a manifest records: the answer to each question, and the options those
/// answers passed over.
///
/// The options matter as much as the answers. A question that offers a choice of
/// several things — which apps are in the stack, which compose services are
/// shared — is decided one option at a time, so an option that was neither
/// chosen nor turned down is one nobody has decided yet. Without this, a service
/// someone added to the compose file would be silently left unmanaged.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stored {
    pub answers: BTreeMap<String, Answer>,
    pub declined: BTreeMap<String, Vec<String>>,
}

impl Stored {
    pub fn is_empty(&self) -> bool {
        self.answers.is_empty()
    }
}

/// The options each set question's answer passed over, in this run.
///
/// A question that offers several things is answered one option at a time, so
/// what the answer turned down is as much a part of the answer as what it chose.
/// A question the previous run already answered keeps what it turned down then:
/// its answer was given against the options of that day, and an option that has
/// appeared since is one nobody has decided — which is exactly what reopens the
/// question. Recomputing it here would quietly mark the new option as declined
/// and leave it out of the manifest for good.
pub fn record(
    report: &Report,
    answers: &AnswerSet,
    recorded: &Recorded,
    asked: &BTreeSet<String>,
) -> Stored {
    let mut declined = BTreeMap::new();
    for unknown in &report.unknowns {
        if unknown.kind != UnknownKind::MultiChoice {
            continue;
        }
        let Some(answer) = answers.answers.get(&unknown.id) else {
            continue;
        };
        let passed_over: Vec<String> = if asked.contains(&unknown.id) {
            let chosen = answer.as_many();
            unknown
                .options
                .iter()
                .filter(|option| !chosen.contains(option))
                .cloned()
                .collect()
        } else {
            recorded
                .declined
                .get(&unknown.id)
                .cloned()
                .unwrap_or_default()
        };
        if !passed_over.is_empty() {
            declined.insert(unknown.id.clone(), passed_over);
        }
    }
    Stored {
        answers: answers.answers.clone(),
        declined,
    }
}

/// The start of a manifest: `version`, then the answers that produced it.
///
/// The answers have to sit here rather than with the services: a bare key
/// belongs to the table above it, so anything after `[app]` or `[bootstrap]`
/// would end up inside that table.
fn manifest_head(stored: Option<&Stored>) -> String {
    let mut out = String::from("version = 1\n");
    if let Some(section) = stored.and_then(answers_section) {
        out.push('\n');
        out.push_str(&section);
    }
    out
}

/// The answers as TOML, with the comment that explains them. `None` when there
/// is nothing to record.
///
/// One line each, because an existing manifest is updated by replacing them: the
/// record is `init`'s own, and nothing else reads it.
fn answers_section(stored: &Stored) -> Option<String> {
    if stored.is_empty() {
        return None;
    }
    let mut out = String::from(ANSWERS_COMMENT);
    let _ = write!(out, "{ANSWERS_KEY} = {{");
    for (index, (id, answer)) in stored.answers.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, " \"{}\" = {}", escape_toml(id), answer_literal(answer));
    }
    out.push_str(" }\n");
    if !stored.declined.is_empty() {
        let _ = write!(out, "{DECLINED_KEY} = {{");
        for (index, (id, options)) in stored.declined.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let quoted: Vec<String> = options
                .iter()
                .map(|option| format!("\"{}\"", escape_toml(option)))
                .collect();
            let _ = write!(out, " \"{}\" = [{}]", escape_toml(id), quoted.join(", "));
        }
        out.push_str(" }\n");
    }
    Some(out)
}

/// One answer as a TOML value: a choice, a list, or a flag.
fn answer_literal(answer: &Answer) -> String {
    match answer {
        Answer::One(value) => format!("\"{}\"", escape_toml(value)),
        Answer::Many(values) => {
            let quoted: Vec<String> = values
                .iter()
                .map(|value| format!("\"{}\"", escape_toml(value)))
                .collect();
            format!("[{}]", quoted.join(", "))
        }
        Answer::Flag(value) => value.to_string(),
    }
}

/// The other direction: a recorded value back into an answer.
fn answer_from_value(value: &toml::Value) -> Option<Answer> {
    match value {
        toml::Value::String(text) => Some(Answer::One(text.clone())),
        toml::Value::Boolean(flag) => Some(Answer::Flag(*flag)),
        toml::Value::Array(values) => {
            let mut answers = Vec::new();
            for value in values {
                answers.push(value.as_str()?.to_string());
            }
            Some(Answer::Many(answers))
        }
        _ => None,
    }
}

/// The answers a manifest records. Anything that is not the shape
/// `answers_section` writes is left out, so a hand-written line cannot put words
/// in `init`'s mouth.
fn read_answers(contents: &str) -> Result<Stored> {
    #[derive(serde::Deserialize)]
    struct Recorded {
        #[serde(default)]
        answers: Option<toml::Value>,
        #[serde(default)]
        declined: Option<toml::Value>,
    }
    let recorded: Recorded = toml::from_str(contents).context("reading the recorded answers")?;
    let table = |value: &Option<toml::Value>| {
        value
            .as_ref()
            .and_then(|value| value.as_table())
            .cloned()
            .unwrap_or_default()
    };
    Ok(Stored {
        answers: table(&recorded.answers)
            .iter()
            .filter_map(|(id, value)| Some((id.clone(), answer_from_value(value)?)))
            .collect(),
        declined: table(&recorded.declined)
            .iter()
            .filter_map(|(id, value)| {
                let options = value
                    .as_array()?
                    .iter()
                    .map(|option| option.as_str().map(str::to_string))
                    .collect::<Option<Vec<String>>>()?;
                Some((id.clone(), options))
            })
            .collect(),
    })
}

/// What a previous run recorded, sorted into what still answers its question and
/// what has to be asked again.
#[derive(Debug, Default)]
pub struct Recorded {
    /// Every recorded answer this report still offers.
    pub answers: BTreeMap<String, Answer>,
    /// The options those answers turned down when they were given, carried
    /// forward so a replayed question is not re-decided against today's options.
    pub declined: BTreeMap<String, Vec<String>>,
    /// The answers whose question now offers options they never decided — a
    /// compose service someone added, an app that appeared — with those options.
    pub reopened: BTreeMap<String, Vec<String>>,
    /// Answers this report no longer offers at all.
    pub dropped: Vec<(String, Answer)>,
}

impl Recorded {
    /// True when this question has been answered and nothing about it is new.
    pub fn settled(&self, id: &str) -> bool {
        self.answers.contains_key(id) && !self.reopened.contains_key(id)
    }
}

/// The answers a previous run recorded in the manifest at `root`.
pub fn recorded(root: &Path, report: &Report) -> Result<Recorded> {
    let path = root.join(crate::manifest::MANIFEST_FILE);
    if !path.is_file() {
        return Ok(Recorded::default());
    }
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let stored = read_answers(&contents)?;
    let mut recorded = Recorded {
        declined: stored.declined.clone(),
        ..Recorded::default()
    };
    for (id, answer) in stored.answers {
        let Some(unknown) = report.unknowns.iter().find(|unknown| unknown.id == id) else {
            recorded.dropped.push((id, answer));
            continue;
        };
        if !unknown.accepts(&answer) {
            recorded.dropped.push((id, answer));
            continue;
        }
        // A set question is decided one option at a time, so it is only answered
        // while it offers nothing new. What it offers now and the answer never
        // decided is what the caller has to ask about.
        if unknown.kind == UnknownKind::MultiChoice {
            let chosen = answer.as_many();
            let passed_over = stored.declined.get(&id).cloned().unwrap_or_default();
            let undecided: Vec<String> = unknown
                .options
                .iter()
                .filter(|option| !chosen.contains(option) && !passed_over.contains(option))
                .cloned()
                .collect();
            if !undecided.is_empty() {
                recorded.reopened.insert(id.clone(), undecided);
            }
        }
        recorded.answers.insert(id, answer);
    }
    Ok(recorded)
}

/// The manifest with its record replaced — or added before the first table, or
/// dropped when there is nothing left to record.
fn with_answers(contents: &str, stored: &Stored) -> String {
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
    let key = lines.iter().position(|line| is_record_key(line));
    let start = match key {
        // The comment belongs to the lines it explains, so it goes with them.
        Some(index) if index > 0 && is_answers_comment(&lines[index - 1]) => Some(index - 1),
        other => other,
    };
    let section: Vec<String> = answers_section(stored)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default();
    match start {
        Some(start) => {
            // Both keys go: the block is replaced as a whole, so a `declined`
            // that is no longer written cannot be left behind.
            let mut end = key.map_or(start + 1, |index| index + 1);
            while lines.get(end).is_some_and(|line| is_record_key(line)) {
                end += 1;
            }
            lines.splice(start..end, section);
        }
        // A bare key belongs to the table above it, so it goes above the first.
        None if !section.is_empty() => {
            let at = lines
                .iter()
                .position(|line| line.starts_with('['))
                .unwrap_or(lines.len());
            lines.splice(at..at, section);
        }
        None => {}
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn is_record_key(line: &str) -> bool {
    [ANSWERS_KEY, DECLINED_KEY].iter().any(|key| {
        line.strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

fn is_answers_comment(line: &str) -> bool {
    line.trim_end() == ANSWERS_COMMENT.trim_end()
}

/// Names that conventionally mark a one-shot initialiser rather than a server.
/// `seed-server` is deliberately not matched: it keeps running.
fn is_initializer(name: &str) -> bool {
    name.ends_with("-init")
        || name == "init"
        || name.starts_with("init-")
        || name == "migrate"
        || name.starts_with("migrate-")
        || name.ends_with("-migrate")
        || name == "setup"
        || name.ends_with("-setup")
}

/// A readable port name derived from the variable the compose file uses, so
/// multi-port services get names like `minio` and `minio_console` rather than
/// positional ones.
fn port_name(variable: Option<&str>, position: usize) -> String {
    let Some(variable) = variable else {
        return format!("port{position}");
    };
    let lowered = variable.to_ascii_lowercase();
    let stripped = lowered
        .strip_prefix("wt_port_")
        .or_else(|| lowered.strip_prefix("port_"))
        .or_else(|| lowered.strip_suffix("_port"))
        .unwrap_or(&lowered);
    let cleaned: String = stripped
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '_') {
        format!("port{position}")
    } else {
        cleaned
    }
}

/// Variables the app's task runner used to set for the recipe it ran.
fn exported_parameters_for(report: &Report, app: &str) -> Option<Vec<String>> {
    report
        .facts
        .iter()
        .filter(|fact| fact.app.as_deref() == Some(app))
        .filter_map(|fact| match &fact.data {
            FactData::Just {
                exported_parameters,
                ..
            } => Some(exported_parameters.clone()),
            _ => None,
        })
        .next()
}

/// Escape a value for a TOML basic string.
fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The comment that introduces the compose services a manifest manages.
const SHARED_SERVICES_COMMENT: &str =
    "# Shared infrastructure from the repository's compose file.\n";

/// The infrastructure services that belong to the manifest that owns them.
fn compose_services(
    compose: &[ComposeServiceFact],
    report: &Report,
    shared: &[String],
    exposed: &[String],
) -> Result<Vec<ServiceBlock>> {
    let mut blocks = Vec::new();
    for name in shared {
        let fact = compose
            .iter()
            .find(|service| service.name == *name)
            .with_context(|| format!("no compose service named '{name}' in the report"))?;
        let mut out = String::new();
        if blocks.is_empty() {
            out.push_str(SHARED_SERVICES_COMMENT);
            out.push('\n');
        }
        out.push_str("[[services]]\n");
        let _ = writeln!(out, "id = \"{name}\"");
        out.push_str("runtime = \"compose\"\n");
        let file = compose_file_for(report, fact);
        let _ = writeln!(
            out,
            "compose = {{ file = \"{file}\", service = \"{name}\" }}"
        );
        if exposed.contains(name) {
            // The compose file derives its own URLs from the same variables that
            // set the published port (`${WT_PORT_ZITADEL:-8080}`). Publishing on
            // an allocated port without telling compose makes those URLs point
            // at the default, so the variable is part of the port declaration.
            let mappings: Vec<(u16, Option<String>)> = fact
                .ports
                .iter()
                .map(|port| parse_port_mapping(port))
                .filter_map(|(target, variable)| target.map(|target| (target, variable)))
                .collect();
            if mappings.is_empty() {
                bail!(
                    "compose service '{name}' has no published port in {file}, so the container-side port is unknown\n\
                     add `ports:` to that service in the compose file, or leave it out of compose.expose"
                );
            }
            if mappings.len() == 1 {
                let (target, variable) = &mappings[0];
                match variable {
                    Some(variable) => {
                        let _ =
                            writeln!(out, "port = {{ target = {target}, env = \"{variable}\" }}");
                    }
                    None => {
                        let _ = writeln!(out, "port = {{ target = {target} }}");
                    }
                }
            } else {
                out.push_str("ports = [\n");
                for (index, (target, variable)) in mappings.iter().enumerate() {
                    let name = port_name(variable.as_deref(), index + 1);
                    match variable {
                        Some(variable) => {
                            let _ = writeln!(
                                out,
                                "  {{ name = \"{name}\", target = {target}, env = \"{variable}\" }},"
                            );
                        }
                        None => {
                            let _ = writeln!(out, "  {{ name = \"{name}\", target = {target} }},");
                        }
                    }
                }
                out.push_str("]\n");
            }
        } else {
            out.push_str("expose = \"none\"\n");
        }
        // An initialiser container runs to completion; the services that follow
        // must wait for it, or they start without what it produces.
        if is_initializer(name) {
            out.push_str("wait = \"exit\"\n");
        }
        blocks.push(ServiceBlock {
            id: name.clone(),
            step: None,
            text: out,
        });
    }
    Ok(blocks)
}

/// Write the planned manifests.
///
/// A file that does not exist is written whole. One that does is added to: the
/// services the answers imply and the file does not declare are appended, and
/// every other line, comment and value is left as it is. `force` regenerates
/// the whole file from discovery instead, which discards local edits.
///
/// Every file is decided before any of them is written, so a manifest that
/// cannot be read leaves the run without half of it applied.
pub fn apply(planned: &[Generated], force: bool) -> Result<Vec<Applied>> {
    let decided = decide(planned, force)?;
    for (applied, contents) in &decided {
        let path = applied.path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(decided.into_iter().map(|(applied, _)| applied).collect())
}

/// What every manifest would become, without writing anything: the outcome for
/// each file, and the whole file that outcome leaves behind.
pub fn preview(planned: &[Generated], force: bool) -> Result<Vec<(Applied, String)>> {
    decide(planned, force)
}

fn decide(planned: &[Generated], force: bool) -> Result<Vec<(Applied, String)>> {
    let mut decided = Vec::new();
    for file in planned {
        if force || !file.path.exists() {
            decided.push((Applied::Created(file.path.clone()), file.contents()));
            continue;
        }
        let existing = std::fs::read_to_string(&file.path)
            .with_context(|| format!("reading {}", file.path.display()))?;
        let declared = crate::manifest::parse_str(&existing)
            .with_context(|| format!("reading the services of {}", file.path.display()))?;
        // The record is init's own, so it is brought up to date even when the
        // manifest needs no new service: a question answered differently is
        // still worth remembering for the next run.
        let record = file.records_answers;
        let before = if record {
            read_answers(&existing)
                .with_context(|| format!("reading the answers of {}", file.path.display()))?
        } else {
            Stored::default()
        };
        let recorded = (record && before != file.stored).then(|| {
            file.stored
                .answers
                .iter()
                .filter(|(id, answer)| {
                    before
                        .answers
                        .get(*id)
                        .is_some_and(|previous| *previous != **answer)
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<String>>()
        });
        // A manifest may declare a step under a name of its own — the app's
        // service renamed, or its app id chosen over the derived one. The step
        // is what must not run twice, so a service the manifest already runs is
        // not added again.
        let declared_steps: BTreeSet<String> =
            declared.services.iter().filter_map(declared_step).collect();
        let missing: Vec<&ServiceBlock> = file
            .services
            .iter()
            .filter(|service| !declared.services.iter().any(|known| known.id == service.id))
            .filter(|service| {
                !service
                    .step
                    .as_ref()
                    .is_some_and(|step| declared_steps.contains(step))
            })
            .collect();
        if missing.is_empty() && recorded.is_none() {
            decided.push((Applied::Unchanged(file.path.clone()), existing));
            continue;
        }
        let mut contents = if recorded.is_some() {
            with_answers(&existing, &file.stored)
        } else {
            existing
        };
        if !missing.is_empty() {
            if !contents.ends_with('\n') {
                contents.push('\n');
            }
            for service in &missing {
                contents.push('\n');
                contents.push_str(without_repeated_comment(&service.text, &contents));
            }
        }
        decided.push((
            Applied::Updated {
                path: file.path.clone(),
                added: missing.iter().map(|service| service.id.clone()).collect(),
                recorded,
            },
            contents,
        ));
    }
    Ok(decided)
}

/// A block's text without a section comment the manifest already carries.
///
/// The comment introduces a group of services, so a manifest that has it must
/// not gain a second copy when a service joins the group later.
fn without_repeated_comment<'a>(text: &'a str, existing: &str) -> &'a str {
    if !existing.contains(SHARED_SERVICES_COMMENT.trim_end()) {
        return text;
    }
    match text.strip_prefix(SHARED_SERVICES_COMMENT) {
        Some(rest) => rest.trim_start_matches('\n'),
        None => text,
    }
}

/// The step an existing service runs, in the `<runner>:<recipe>` form the run
/// answers use, when the service declares a target discovery could have offered.
/// `None` for a compose service or a hand-written command.
fn declared_step(service: &crate::manifest::Service) -> Option<String> {
    use crate::manifest::Target;
    match service.target.as_ref()? {
        Target::Npm { script, .. } => Some(format!("npm:{script}")),
        Target::Pnpm { script, .. } => Some(format!("pnpm:{script}")),
        Target::Just { recipe, .. } => Some(format!("just:{recipe}")),
        Target::Mise { task, .. } => Some(format!("mise:{task}")),
        Target::Uv {
            script: Some(script),
            ..
        } => Some(format!("uv:{script}")),
        // yarn and bun are written as commands, because neither has a target.
        Target::Command { command } => ["yarn", "bun"].iter().find_map(|runner| {
            let rest = command.strip_prefix(&format!("{runner} run "))?;
            let script = rest.split_whitespace().next()?;
            Some(format!("{runner}:{script}"))
        }),
        _ => None,
    }
}

/// Hazards the generated manifest cannot repair on its own: a step the run
/// answer points at pins a port, so the variable magictree injects into that
/// step would be ignored, and a second worktree could not pick its own port.
pub fn warnings(report: &Report, answers: &AnswerSet) -> Vec<String> {
    let mut out = Vec::new();
    for app in &report.apps {
        let Some(spec) = answers
            .answers
            .get(&format!("{}.run", app.id))
            .and_then(|answer| answer.as_one())
            .filter(|spec| *spec != "skip")
        else {
            continue;
        };
        let variable = answers
            .answers
            .get(&format!("{}.port_env", app.id))
            .and_then(|answer| answer.as_one())
            .filter(|name| !name.is_empty() && *name != "none")
            .unwrap_or("PORT")
            .to_string();
        let facts: Vec<&Fact> = report
            .facts
            .iter()
            .filter(|fact| fact.app.as_deref() == Some(app.id.as_str()) || app.dir == ".")
            .collect();
        let Some((kind, rest)) = spec.split_once(':') else {
            continue;
        };
        // Storybook is started with the allocated port on its command line, so
        // the literal its script pins is overridden rather than a hazard.
        if serves_storybook(report, &app.id, spec) {
            continue;
        }
        let sites = match kind {
            "npm" | "pnpm" | "yarn" | "bun" => ports::sites(&facts, FactKind::Node, rest),
            "just" => ports::sites(&facts, FactKind::Just, rest),
            "mise" => ports::sites(&facts, FactKind::Mise, rest),
            "uv" => ports::sites(&facts, FactKind::Python, rest),
            "procfile" => ports::sites(&facts, FactKind::Procfile, rest),
            // A raw command carries its own text; scanned below.
            "command" => Vec::new(),
            _ => Vec::new(),
        };
        // One warning per step, naming every port it pins.
        let mut warned: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (fact, _) in sites {
            if !warned.insert(fact.id.clone()) {
                continue;
            }
            let group: Vec<&PortLiteralFact> = ports::all(&facts)
                .into_iter()
                .filter(|(other, literal)| {
                    other.id == fact.id && literal.container.as_deref() == Some(rest)
                })
                .map(|(_, literal)| literal)
                .collect();
            let listed: Vec<String> = group
                .iter()
                .map(|literal| literal.port.to_string())
                .collect();
            let suggestion = group
                .first()
                .map(|literal| ports::suggestion(literal, Some(&variable)))
                .unwrap_or_default();
            out.push(format!(
                "{} {} pins port {}; {suggestion}",
                fact.source,
                ports::step_label(fact.kind, rest),
                listed.join(" and ")
            ));
        }
        if kind == "command" {
            for literal in ports::scan(rest) {
                let fact = PortLiteralFact {
                    port: literal.port,
                    kind: literal.kind,
                    literal: literal.literal,
                    container: None,
                    text: rest.to_string(),
                };
                out.push(format!(
                    "the command `{rest}` pins port {}; {}",
                    fact.port,
                    ports::suggestion(&fact, Some(&variable))
                ));
            }
        }
    }
    out
}

/// Bootstrap runs the install first, then whatever generates files the dev
/// server imports. Generation steps deliberately declare no `inputs`, so they
/// run on every `up` — a missing generated file is not worth caching around.
fn bootstrap_section(app_root: &Path, setup: &[String]) -> Result<Option<String>> {
    let install = install_command(app_root);
    let inputs = install_inputs(app_root);
    if install.is_none() && setup.is_empty() {
        return Ok(None);
    }
    let mut out = String::from("[bootstrap]\n");
    out.push_str("run = [\n");
    if let Some(command) = &install {
        if !inputs.is_empty() {
            let quoted: Vec<String> = inputs.iter().map(|input| format!("\"{input}\"")).collect();
            let _ = writeln!(
                out,
                "  {{ command = \"{command}\", inputs = [{}] }},",
                quoted.join(", ")
            );
        } else {
            let _ = writeln!(out, "  \"{command}\",");
        }
    }
    for spec in setup {
        let command = step_command(spec)?;
        let _ = writeln!(out, "  \"{command}\",");
    }
    out.push_str("]\n");
    Ok(Some(out))
}

/// Turn a discovered setup spec into the shell command bootstrap runs.
fn step_command(spec: &str) -> Result<String> {
    let (kind, rest) = spec
        .split_once(':')
        .with_context(|| format!("malformed setup step '{spec}'"))?;
    Ok(match kind {
        "just" => format!("just {rest}"),
        "mise" => format!("mise run {rest}"),
        "npm" | "pnpm" | "yarn" | "bun" => format!("{kind} run {rest}"),
        other => bail!("unsupported setup step '{spec}' (unknown runner '{other}')"),
    })
}

fn target_line(kind: &str, rest: &str, spec: &str) -> Result<String> {
    let line = match kind {
        "npm" | "pnpm" | "yarn" | "bun" => {
            // Only npm and pnpm have first-class targets; yarn and bun fall back
            // to the plain command so nothing is silently reinterpreted.
            match kind {
                "npm" => format!("target = {{ kind = \"npm\", script = \"{rest}\" }}\n"),
                "pnpm" => format!("target = {{ kind = \"pnpm\", script = \"{rest}\" }}\n"),
                other => format!("command = \"{other} run {rest}\"\n"),
            }
        }
        "just" => format!("target = {{ kind = \"just\", recipe = \"{rest}\" }}\n"),
        "mise" => format!("target = {{ kind = \"mise\", task = \"{rest}\" }}\n"),
        "uv" => format!("target = {{ kind = \"uv\", script = \"{rest}\" }}\n"),
        "command" => format!("command = \"{}\"\n", escape_toml(rest)),
        "procfile" => format!("command = \"<command from Procfile:{rest}>\"\n"),
        other => bail!("unsupported run answer '{spec}' (unknown runner '{other}')"),
    };
    Ok(line)
}

fn compose_facts(report: &Report) -> Vec<ComposeServiceFact> {
    let mut out = Vec::new();
    for fact in &report.facts {
        if let FactData::Compose { services, .. } = &fact.data {
            out.extend(services.iter().cloned());
        }
    }
    out
}

fn compose_file_for(report: &Report, service: &ComposeServiceFact) -> String {
    for fact in &report.facts {
        if let FactData::Compose { services, .. } = &fact.data {
            if services
                .iter()
                .any(|candidate| candidate.name == service.name)
            {
                return fact.source.clone();
            }
        }
    }
    "compose.yaml".to_string()
}

fn resolve_members(report: &Report, answers: &AnswerSet) -> Result<Vec<String>> {
    let all: Vec<String> = report.apps.iter().map(|app| app.dir.clone()).collect();
    let Some(answer) = answers.answers.get("stack.members") else {
        return Ok(all);
    };
    let picked: BTreeSet<String> = answer.as_many().into_iter().collect();
    for name in &picked {
        if !all.contains(name) {
            bail!("stack.members lists '{name}', which discovery did not find");
        }
    }
    Ok(picked.into_iter().collect())
}

fn answer_list(answers: &AnswerSet, id: &str) -> Result<Vec<String>> {
    Ok(answers
        .answers
        .get(id)
        .map(|answer| answer.as_many())
        .unwrap_or_default())
}

/// The answer set implied by the report's defaults, for non-interactive use.
pub fn default_answers(report: &Report) -> AnswerSet {
    let mut answers = std::collections::BTreeMap::new();
    for unknown in &report.unknowns {
        let value = match unknown.kind {
            UnknownKind::MultiChoice => Answer::Many(unknown.default_options()),
            UnknownKind::Choice | UnknownKind::Text => {
                Answer::One(unknown.default.clone().unwrap_or_default())
            }
            UnknownKind::Bool => Answer::Flag(
                unknown
                    .default
                    .as_deref()
                    .map(|value| value == "true" || value == "yes")
                    .unwrap_or(false),
            ),
        };
        answers.insert(unknown.id.clone(), value);
    }
    AnswerSet {
        answers_version: ANSWERS_VERSION,
        report_hash: report.report_hash.clone(),
        answers,
    }
}

impl Unknown {
    /// Multi-choice defaults. A declared default is authoritative, including an
    /// empty one; a question with no declared default falls back to every option.
    /// Used only by `--accept-defaults`, never by the interactive path.
    pub fn default_options(&self) -> Vec<String> {
        match &self.default {
            Some(value) => value
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect(),
            None => self.options.clone(),
        }
    }

    /// True when this answer is one the question still offers. A recorded answer
    /// that no longer fits is asked again rather than replayed.
    pub fn accepts(&self, answer: &Answer) -> bool {
        match self.kind {
            UnknownKind::Choice => answer
                .as_one()
                .is_some_and(|value| self.options.iter().any(|option| option == value)),
            UnknownKind::MultiChoice => answer
                .as_many()
                .iter()
                .all(|value| self.options.iter().any(|option| option == value)),
            UnknownKind::Bool => answer.as_flag().is_some(),
            UnknownKind::Text => answer.as_one().is_some(),
        }
    }

    /// The same question with a recorded answer as its default, so re-asking it
    /// shows what was chosen last time and keeping it is one keystroke.
    pub fn with_recorded(&self, recorded: Option<&Answer>) -> Unknown {
        let mut question = self.clone();
        if let Some(answer) = recorded {
            question.default = Some(match answer {
                Answer::One(value) => value.clone(),
                Answer::Many(values) => values.join(","),
                Answer::Flag(value) => value.to_string(),
            });
        }
        question
    }
}
