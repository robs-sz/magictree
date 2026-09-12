//! The interactive half of `init`.
//!
//! The wizard collects answers; `init::plan` turns them into manifests. Both the
//! wizard and an agent therefore produce identical output, because neither one
//! writes files — they only supply answers.

use crate::discover::report::{
    Answer, AnswerSet, Report, Scope, Unknown, UnknownKind, ANSWERS_VERSION,
};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};

/// Interpret one line of input for one question. Pure, so it can be tested
/// without a terminal.
pub fn parse_answer(unknown: &Unknown, input: &str) -> Result<Answer> {
    let input = input.trim();
    if input.is_empty() {
        return match unknown.kind {
            UnknownKind::MultiChoice => Ok(Answer::Many(unknown.default_options())),
            UnknownKind::Bool => Ok(Answer::Flag(
                unknown
                    .default
                    .as_deref()
                    .map(|value| value == "true")
                    .unwrap_or(false),
            )),
            _ => unknown
                .default
                .clone()
                .map(Answer::One)
                .context("this question has no default; an answer is required"),
        };
    }

    match unknown.kind {
        UnknownKind::Bool => match input.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" => Ok(Answer::Flag(true)),
            "n" | "no" | "false" => Ok(Answer::Flag(false)),
            other => bail!("expected yes or no, got '{other}'"),
        },
        UnknownKind::MultiChoice => {
            if input.eq_ignore_ascii_case("none") {
                return Ok(Answer::Many(Vec::new()));
            }
            if input.eq_ignore_ascii_case("all") {
                return Ok(Answer::Many(unknown.options.clone()));
            }
            let mut picked = Vec::new();
            for token in input.split(',') {
                let token = token.trim();
                if token.is_empty() {
                    continue;
                }
                if let Some(value) = resolve_option(&unknown.options, token) {
                    picked.push(value);
                } else {
                    bail!(
                        "'{token}' is not one of: {}\n(or 'all', or 'none')",
                        unknown.options.join(", ")
                    );
                }
            }
            Ok(Answer::Many(picked))
        }
        UnknownKind::Choice => match resolve_option(&unknown.options, input) {
            Some(value) => Ok(Answer::One(value)),
            None => bail!(
                "'{input}' is not one of: {}\n(enter a number, or the value itself)",
                unknown.options.join(", ")
            ),
        },
        UnknownKind::Text => Ok(Answer::One(input.to_string())),
    }
}

/// Accept either the value or its 1-based position in the option list.
fn resolve_option(options: &[String], token: &str) -> Option<String> {
    if let Ok(index) = token.parse::<usize>() {
        if index >= 1 && index <= options.len() {
            return Some(options[index - 1].clone());
        }
    }
    options
        .iter()
        .find(|option| option.as_str() == token)
        .cloned()
}

/// Ask every question on the terminal, in report order.
pub fn prompt(
    report: &Report,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<AnswerSet> {
    let mut answers: BTreeMap<String, Answer> = BTreeMap::new();

    for unknown in &report.unknowns {
        let scope = match &unknown.scope {
            Scope::Workspace => "workspace".to_string(),
            Scope::App { app } => format!("app {app}"),
        };
        writeln!(output, "\n[{scope}] {}", unknown.question)?;
        for (index, option) in unknown.options.iter().enumerate() {
            let marker = match (&unknown.default, unknown.kind) {
                (Some(default), UnknownKind::Choice) if default == option => " (default)",
                (Some(default), UnknownKind::MultiChoice)
                    if default.split(',').any(|item| item.trim() == option) =>
                {
                    " (default)"
                }
                _ => "",
            };
            writeln!(output, "  {}. {option}{marker}", index + 1)?;
        }
        if unknown.kind == UnknownKind::MultiChoice {
            writeln!(output, "  enter a comma-separated list, or 'all' / 'none'")?;
        }
        write!(output, "> ")?;
        output.flush()?;

        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            bail!("input ended before every question was answered");
        }
        match parse_answer(unknown, &line) {
            Ok(answer) => {
                answers.insert(unknown.id.clone(), answer);
            }
            Err(error) => {
                writeln!(output, "{error}")?;
                // Ask again rather than aborting a long session on a typo.
                write!(output, "> ")?;
                output.flush()?;
                let mut retry = String::new();
                if input.read_line(&mut retry)? == 0 {
                    bail!("input ended before every question was answered");
                }
                answers.insert(unknown.id.clone(), parse_answer(unknown, &retry)?);
            }
        }
    }

    Ok(AnswerSet {
        answers_version: ANSWERS_VERSION,
        report_hash: report.report_hash.clone(),
        answers,
    })
}

/// Run the wizard on the real terminal.
pub fn run(report: &Report) -> Result<AnswerSet> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "stdin is not a terminal, so the wizard cannot ask anything\n\
             use `magictree init --accept-defaults`, or `magictree discover --default-answers > answers.json` and edit it"
        );
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    prompt(report, &mut stdin.lock(), &mut stdout.lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown(kind: UnknownKind, options: &[&str], default: Option<&str>) -> Unknown {
        named("x", kind, options, default)
    }

    fn named(id: &str, kind: UnknownKind, options: &[&str], default: Option<&str>) -> Unknown {
        Unknown {
            id: id.to_string(),
            scope: Scope::Workspace,
            kind,
            question: "q".to_string(),
            options: options.iter().map(|value| value.to_string()).collect(),
            evidence: Vec::new(),
            default: default.map(|value| value.to_string()),
        }
    }

    #[test]
    fn empty_input_takes_the_default() {
        let question = unknown(UnknownKind::Choice, &["pnpm:dev", "skip"], Some("pnpm:dev"));
        assert!(matches!(
            parse_answer(&question, "").unwrap(),
            Answer::One(value) if value == "pnpm:dev"
        ));
    }

    #[test]
    fn empty_input_without_a_default_is_an_error() {
        let question = unknown(UnknownKind::Choice, &["a", "b"], None);
        assert!(parse_answer(&question, "").is_err());
    }

    #[test]
    fn choice_accepts_a_number_or_the_value() {
        let question = unknown(UnknownKind::Choice, &["pnpm:dev", "skip"], None);
        assert!(matches!(
            parse_answer(&question, "2").unwrap(),
            Answer::One(value) if value == "skip"
        ));
        assert!(matches!(
            parse_answer(&question, "pnpm:dev").unwrap(),
            Answer::One(value) if value == "pnpm:dev"
        ));
    }

    #[test]
    fn multi_choice_accepts_lists_and_keywords() {
        let question = unknown(
            UnknownKind::MultiChoice,
            &["postgres", "redis", "worker"],
            Some("postgres,redis"),
        );
        assert!(matches!(
            parse_answer(&question, "1,3").unwrap(),
            Answer::Many(values) if values == vec!["postgres".to_string(), "worker".to_string()]
        ));
        assert!(matches!(
            parse_answer(&question, "all").unwrap(),
            Answer::Many(values) if values.len() == 3
        ));
        assert!(matches!(
            parse_answer(&question, "none").unwrap(),
            Answer::Many(values) if values.is_empty()
        ));
        // Empty input takes the declared default, not every option.
        assert!(matches!(
            parse_answer(&question, "").unwrap(),
            Answer::Many(values) if values == vec!["postgres".to_string(), "redis".to_string()]
        ));
    }

    #[test]
    fn invalid_answers_are_rejected_with_the_options_listed() {
        let question = unknown(UnknownKind::Choice, &["a", "b"], None);
        let error = parse_answer(&question, "c").unwrap_err().to_string();
        assert!(error.contains('a') && error.contains('b'), "{error}");

        let multi = unknown(UnknownKind::MultiChoice, &["a", "b"], None);
        assert!(parse_answer(&multi, "a,z").is_err());
    }

    #[test]
    fn bool_accepts_common_spellings() {
        let question = unknown(UnknownKind::Bool, &[], None);
        assert!(matches!(
            parse_answer(&question, "yes").unwrap(),
            Answer::Flag(true)
        ));
        assert!(matches!(
            parse_answer(&question, "n").unwrap(),
            Answer::Flag(false)
        ));
        assert!(parse_answer(&question, "maybe").is_err());
    }

    #[test]
    fn text_is_taken_verbatim() {
        let question = unknown(UnknownKind::Text, &[], None);
        assert!(matches!(
            parse_answer(&question, "/health").unwrap(),
            Answer::One(value) if value == "/health"
        ));
    }

    #[test]
    fn prompt_answers_every_question_in_order() {
        let report = Report {
            report_version: crate::discover::report::REPORT_VERSION,
            report_hash: "sha256:x".to_string(),
            repo: crate::discover::report::RepoFacts {
                root: "/tmp".to_string(),
                git_common_dir: "/tmp/.git".to_string(),
                is_git_repo: true,
                worktrees: Vec::new(),
            },
            apps: Vec::new(),
            facts: Vec::new(),
            unknowns: vec![
                named("members", UnknownKind::MultiChoice, &["a", "b"], Some("a")),
                named("run", UnknownKind::Choice, &["x", "y"], Some("y")),
            ],
        };
        let mut input = std::io::Cursor::new(b"b\n1\n".to_vec());
        let mut output = Vec::new();
        let answers = prompt(&report, &mut input, &mut output).unwrap();

        assert_eq!(answers.report_hash, "sha256:x");
        assert!(matches!(
            answers.answers.get("members"),
            Some(Answer::Many(values)) if values == &vec!["b".to_string()]
        ));
        assert!(matches!(
            answers.answers.get("run"),
            Some(Answer::One(value)) if value == "x"
        ));
        let rendered = String::from_utf8_lossy(&output);
        assert!(rendered.contains("workspace"), "{rendered}");
    }
}
