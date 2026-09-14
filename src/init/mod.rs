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
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One manifest to write.
#[derive(Debug)]
pub struct Generated {
    pub path: PathBuf,
    pub contents: String,
}

/// Build every manifest the report and answers imply.
pub fn plan(report: &Report, answers: &AnswerSet, repo_root: &Path) -> Result<Vec<Generated>> {
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

    let mut generated = Vec::new();

    // Compose services live in the manifest at the repository root, because
    // that is where their compose file's paths resolve from.
    let single_app_at_root = report.apps.len() == 1 && report.apps[0].dir == ".";
    let manage_compose = !shared.is_empty() || !exposed.is_empty();
    let workspace_layer = report.apps.len() > 1 || (manage_compose && !single_app_at_root);

    if workspace_layer {
        let mut out = String::from("version = 1\n\n[workspace]\napps = [");
        for (index, dir) in members.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "\"{dir}\"");
        }
        out.push_str("]\n");
        append_compose_services(&mut out, &compose, report, &shared, &exposed)?;
        generated.push(Generated {
            path: repo_root.join("magictree.toml"),
            contents: out,
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
        let mut out = String::from("version = 1\n\n[app]\n");
        let _ = writeln!(out, "id = \"{}\"", app.id);

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
            if kind == "command" {
                // Running the command directly leaves out whatever the task
                // runner's recipe prepared, so record what that was. The service
                // may still start without these, so they are not guessed at.
                if let Some(parameters) = exported_parameters_for(report, &app.id) {
                    if !parameters.is_empty() {
                        let _ = writeln!(
                            out,
                            "# the recipe this command came from also set: {}",
                            parameters.join(", ")
                        );
                        out.push_str("# add them under [env] if the service needs them\n");
                    }
                }
            }
            out.push_str("\n[[services]]\n");
            let _ = writeln!(out, "id = \"{}\"", app.id);
            if serves_storybook(report, &app.id, spec) {
                // The app's own server is Storybook: it gets the same port
                // handling as the dedicated service below, because Storybook
                // reads no environment variable for it.
                out.push_str(&storybook_target(kind, rest)?);
            } else {
                out.push_str(&target_line(kind, rest, spec)?);
                let port_variable = answers
                    .answers
                    .get(&format!("{}.port_env", app.id))
                    .and_then(|answer| answer.as_one())
                    .filter(|name| !name.is_empty() && *name != "none")
                    .unwrap_or("PORT");
                let _ = writeln!(out, "port = {{ env = \"{port_variable}\" }}");
            }
            out.push_str("health = { http = \"/\", timeout = 120 }\n");
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
            if let Some(comment) = storybook_mcp_comment(report, &app.id) {
                out.push_str(comment);
            }
            out.push_str("\n[[services]]\n");
            let _ = writeln!(out, "id = \"{id}\"");
            out.push_str(&storybook_target(kind, rest)?);
            out.push_str("health = { http = \"/\", timeout = 120 }\n");
        }

        if !workspace_layer {
            // A single app at the repository root owns the compose services too.
            append_compose_services(&mut out, &compose, report, &shared, &exposed)?;
        }
        let _ = shares_root_manifest;

        generated.push(Generated {
            path: app_root.join("magictree.toml"),
            contents: out,
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
    Some("\n# @storybook/addon-mcp answers MCP at /mcp on this service\n")
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

/// Append infrastructure services to the manifest that owns them.
fn append_compose_services(
    out: &mut String,
    compose: &[ComposeServiceFact],
    report: &Report,
    shared: &[String],
    exposed: &[String],
) -> Result<()> {
    if shared.is_empty() {
        return Ok(());
    }
    out.push_str("\n# Shared infrastructure from the repository's compose file.\n");
    for name in shared {
        let fact = compose
            .iter()
            .find(|service| service.name == *name)
            .with_context(|| format!("no compose service named '{name}' in the report"))?;
        out.push_str("\n[[services]]\n");
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
    }
    Ok(())
}

/// Write the planned manifests. Existing files are reported, never overwritten
/// unless `force` is set.
pub fn apply(planned: &[Generated], force: bool) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for file in planned {
        if file.path.exists() && !force {
            bail!(
                "{} already exists; pass --force to overwrite",
                file.path.display()
            );
        }
        if let Some(parent) = file.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&file.path, &file.contents)
            .with_context(|| format!("writing {}", file.path.display()))?;
        written.push(file.path.clone());
    }
    Ok(written)
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
}
