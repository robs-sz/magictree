//! Hardcoded ports: a file that pins a port magictree cannot vary.
//!
//! Every worktree gets its own ports, handed to the process as environment
//! variables. A command that pins a number (`next dev -p 3005`,
//! `uvicorn --port=8000`, `DATABASE_PORT=5433`) ignores that: two worktrees
//! fight over the same number, and the health probe watches a port nothing
//! listens on. This module only finds the literals and renders the rewrite.
//! Whether a literal is a problem is the caller's call.
//!
//! Only files whose step bodies discovery already reads are scanned, which is
//! why a mise task or a uv script is not covered here: the extractors keep
//! their names, not their commands.

use super::report::{Fact, FactData, FactKind, PortKind, PortLiteralFact};

/// A port literal found in a command line or an env file line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortLiteral {
    pub port: u16,
    pub kind: PortKind,
    /// The literal as written, including the flag or variable that carries it.
    pub literal: String,
    /// Byte range of `literal` within the scanned text.
    pub start: usize,
    pub end: usize,
    /// Everything before the number that a fix has to keep: `-p `, `--port=`,
    /// `localhost:`, and an opening quote when the value was quoted.
    prefix: String,
    /// A closing quote, kept so the rewrite stays valid shell.
    suffix: String,
}

impl PortLiteral {
    /// What replaces this literal when the port comes from `variable`, or
    /// `None` when the fix is to drop it: an assignment of a number cannot
    /// defer to the variable it assigns.
    pub fn fix(&self, variable: &str) -> Option<String> {
        match self.kind {
            PortKind::Assignment => None,
            PortKind::Flag | PortKind::Url => Some(format!(
                "{}${{{variable}:-{}}}{}",
                self.prefix, self.port, self.suffix
            )),
        }
    }
}

/// Every port literal in `text`, in the order it appears, without duplicates.
pub fn scan(text: &str) -> Vec<PortLiteral> {
    let mut out: Vec<PortLiteral> = Vec::new();
    for (index, byte) in text.bytes().enumerate() {
        let literal = match byte {
            b'-' if token_start(text, index) => scan_flag(text, index),
            b'=' => scan_assignment(text, index),
            b':' => scan_host_port(text, index),
            _ => None,
        };
        if let Some(literal) = literal {
            out.push(literal);
        }
    }
    out.sort_by_key(|literal| (literal.start, literal.end));
    out.dedup_by(|left, right| left.start == right.start && left.end == right.end);
    out
}

/// Rewrite `text` so its ports come from `variable`. An inline assignment is
/// dropped: it would overwrite the value magictree injects. `None` when there
/// is nothing to rewrite.
pub fn rewrite(text: &str, variable: &str) -> Option<String> {
    let literals = scan(text);
    if literals.is_empty() {
        return None;
    }
    let mut out = text.to_string();
    // Back to front, so replacing one literal never moves another's range.
    for literal in literals.iter().rev() {
        match literal.fix(variable) {
            Some(replacement) => out.replace_range(literal.start..literal.end, &replacement),
            None => {
                let mut start = literal.start;
                let mut end = literal.end;
                while start > 0 && out.as_bytes()[start - 1] == b' ' {
                    start -= 1;
                }
                while end < out.len() && out.as_bytes()[end] == b' ' {
                    end += 1;
                }
                out.replace_range(start..end, "");
            }
        }
    }
    Some(out)
}

/// Every recorded literal, paired with the fact it came from.
pub fn all<'a>(facts: &[&'a Fact]) -> Vec<(&'a Fact, &'a PortLiteralFact)> {
    let mut out = Vec::new();
    for fact in facts {
        for literal in literals_of(fact) {
            out.push((*fact, literal));
        }
    }
    out
}

/// Literals recorded for one step of one file kind: the `dev` script of a
/// package.json, a justfile recipe, a Procfile process, an env template line.
pub fn sites<'a>(
    facts: &[&'a Fact],
    kind: FactKind,
    step: &str,
) -> Vec<(&'a Fact, &'a PortLiteralFact)> {
    all(facts)
        .into_iter()
        .filter(|(fact, literal)| fact.kind == kind && literal.container.as_deref() == Some(step))
        .collect()
}

/// The rewrite that makes a literal follow `variable`, or `None` when the
/// literal has to go: an assignment to a number cannot defer to the variable it
/// assigns.
pub fn fix_for(literal: &PortLiteralFact, variable: &str) -> Option<String> {
    match literal.kind {
        PortKind::Assignment => None,
        PortKind::Flag | PortKind::Url => rewrite(&literal.text, variable),
    }
}

/// The proposed fix for one literal, as the sentence to show a user.
///
/// `variable` is the variable that carries this port; `None` when no service in
/// the manifest allocates it, so no rewrite can be offered.
pub fn suggestion(literal: &PortLiteralFact, variable: Option<&str>) -> String {
    let container = literal.container.clone().unwrap_or_default();
    if literal.kind == PortKind::Assignment {
        return format!("leave `{container}` without a value, or read the variable at runtime");
    }
    match variable {
        Some(variable) => match fix_for(literal, variable) {
            Some(rewritten) => format!("read the port from `{variable}` instead: `{rewritten}`"),
            None => format!("read the port from `{variable}` instead"),
        },
        None => {
            "no service in the manifest allocates this port: declare the one that should, then read its variable here".to_string()
        }
    }
}

/// How a step reads in a finding: `script 'dev'`, `variable 'DATABASE_PORT'`.
pub fn step_label(kind: FactKind, container: &str) -> String {
    match kind {
        FactKind::Node => format!("script '{container}'"),
        FactKind::Just => format!("recipe '{container}'"),
        FactKind::Procfile => format!("process '{container}'"),
        FactKind::EnvExample => format!("variable '{container}'"),
        _ => format!("step '{container}'"),
    }
}

fn literals_of(fact: &Fact) -> &[PortLiteralFact] {
    match &fact.data {
        FactData::Node { ports, .. }
        | FactData::Just { ports, .. }
        | FactData::EnvExample { ports, .. }
        | FactData::Procfile { ports, .. } => ports.as_slice(),
        _ => &[],
    }
}

/// True when `index` starts a token, so a `-p` inside a word is not a flag.
fn token_start(text: &str, index: usize) -> bool {
    match index
        .checked_sub(1)
        .and_then(|before| text.as_bytes().get(before))
    {
        None => true,
        Some(byte) => matches!(
            byte,
            b' ' | b'\t' | b'\n' | b'"' | b'\'' | b'&' | b'|' | b';' | b'(' | b'='
        ),
    }
}

/// `-p 3005`, `-p=3005`, `--port 3005`, `--port=3005`, `--http-port=9000`.
fn scan_flag(text: &str, start: usize) -> Option<PortLiteral> {
    let bytes = text.as_bytes();
    let mut end_of_flag = start;
    while end_of_flag < bytes.len()
        && (bytes[end_of_flag].is_ascii_alphanumeric() || bytes[end_of_flag] == b'-')
    {
        end_of_flag += 1;
    }
    let flag = &text[start..end_of_flag];
    let named = flag.trim_start_matches('-');
    let is_port_flag = flag == "-p"
        || (flag.starts_with("--")
            && !named.is_empty()
            && named.to_ascii_lowercase().ends_with("port"));
    if !is_port_flag {
        return None;
    }
    let (value, quote) = separated_value(text, end_of_flag)?;
    let (port, mut end) = digits(text, value)?;
    let mut suffix = String::new();
    if let Some(quote) = quote {
        if text.as_bytes().get(end) == Some(&quote) {
            end += 1;
            suffix.push(quote as char);
        }
    }
    Some(PortLiteral {
        port,
        kind: PortKind::Flag,
        literal: text[start..end].to_string(),
        start,
        end,
        prefix: text[start..value].to_string(),
        suffix,
    })
}

/// `PORT=3005`, `DATABASE_PORT="5433"`, `export APP_PORT=5173`.
fn scan_assignment(text: &str, equals: usize) -> Option<PortLiteral> {
    let bytes = text.as_bytes();
    let mut start = equals;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    if start == equals {
        return None;
    }
    let name = &text[start..equals];
    let upper = name.to_ascii_uppercase();
    if !upper.contains("PORT") || !matches!(bytes[start], b'A'..=b'Z' | b'a'..=b'z' | b'_') {
        return None;
    }
    // Part of a longer expression: `${DB_PORT:-5432}`, `--port=3005`, `x.PORT=1`.
    if start > 0 && matches!(bytes[start - 1], b'$' | b'{' | b'.' | b'-') {
        return None;
    }
    let mut value = equals + 1;
    let quote = match bytes.get(value) {
        Some(b'"') => Some(b'"'),
        Some(b'\'') => Some(b'\''),
        _ => None,
    };
    if quote.is_some() {
        value += 1;
    }
    let (port, mut end) = digits(text, value)?;
    if let Some(quote) = quote {
        if bytes.get(end) == Some(&quote) {
            end += 1;
        }
    }
    Some(PortLiteral {
        port,
        kind: PortKind::Assignment,
        literal: text[start..end].to_string(),
        start,
        end,
        prefix: text[start..equals + 1].to_string(),
        suffix: String::new(),
    })
}

/// `localhost:5173`, `127.0.0.1:5433`, `http://api:8000`.
fn scan_host_port(text: &str, colon: usize) -> Option<PortLiteral> {
    let bytes = text.as_bytes();
    // `${WT_PORT_DB:-5432}` is a default for a variable, not a literal.
    if colon == 0 || bytes[colon - 1] == b'-' {
        return None;
    }
    let mut start = colon;
    while start > 0
        && (bytes[start - 1].is_ascii_alphanumeric()
            || matches!(bytes[start - 1], b'.' | b'-' | b'_'))
    {
        start -= 1;
    }
    if start == colon {
        return None;
    }
    let host = &text[start..colon];
    if !host.chars().any(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    if start > 0 && matches!(bytes[start - 1], b'{' | b'$') {
        return None;
    }
    let (port, end) = digits(text, colon + 1)?;
    // Below 1024 a pair like `10:30` is a time or a version, not a port.
    if port < 1024 {
        return None;
    }
    Some(PortLiteral {
        port,
        kind: PortKind::Url,
        literal: text[start..end].to_string(),
        start,
        end,
        prefix: text[start..colon + 1].to_string(),
        suffix: String::new(),
    })
}

/// Step over `=`, whitespace, and an opening quote for a flag's value. `None`
/// when the flag has no separated value: `-p3005` is another tool's spelling,
/// not a port here.
fn separated_value(text: &str, cursor: usize) -> Option<(usize, Option<u8>)> {
    let bytes = text.as_bytes();
    let mut value = cursor;
    if bytes.get(value) == Some(&b'=') {
        value += 1;
    } else {
        let mut spaces = 0usize;
        while bytes.get(value) == Some(&b' ') {
            value += 1;
            spaces += 1;
        }
        if spaces == 0 {
            return None;
        }
    }
    let quote = match bytes.get(value) {
        Some(b'"') => Some(b'"'),
        Some(b'\'') => Some(b'\''),
        _ => None,
    };
    if quote.is_some() {
        value += 1;
    }
    Some((value, quote))
}

/// A port number at `start`, with the index just past it.
fn digits(text: &str, start: usize) -> Option<(u16, usize)> {
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == start {
        return None;
    }
    let value: u32 = text[start..end].parse().ok()?;
    if value == 0 || value > u16::MAX as u32 {
        return None;
    }
    // `30050` and `3005.2` are numbers, not port 3005.
    if matches!(bytes.get(end), Some(b'.') | Some(b'_')) {
        return None;
    }
    Some((value as u16, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literals(text: &str) -> Vec<(u16, PortKind, String)> {
        scan(text)
            .into_iter()
            .map(|literal| (literal.port, literal.kind, literal.literal))
            .collect()
    }

    #[test]
    fn finds_port_flags() {
        assert_eq!(
            literals("next dev --turbo -p 3005"),
            vec![(3005, PortKind::Flag, "-p 3005".to_string())]
        );
        assert_eq!(
            literals("uvicorn app:app --port=8000"),
            vec![(8000, PortKind::Flag, "--port=8000".to_string())]
        );
        assert_eq!(
            literals("node server.js --http-port 9000"),
            vec![(9000, PortKind::Flag, "--http-port 9000".to_string())]
        );
        assert_eq!(
            literals("vite --port \"5173\""),
            vec![(5173, PortKind::Flag, "--port \"5173\"".to_string())]
        );
    }

    #[test]
    fn flags_that_are_not_ports_are_ignored() {
        for text in [
            "mkdir -p build",
            "cp -p file",
            "docker compose -p myproject up",
            "next dev",
            "cargo build --profile release",
            "git commit -p3005",
        ] {
            assert!(scan(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn finds_assignments_and_urls() {
        assert_eq!(
            literals("PORT=3005 vite"),
            vec![(3005, PortKind::Assignment, "PORT=3005".to_string())]
        );
        assert_eq!(
            literals("DATABASE_PORT=\"5433\""),
            vec![(
                5433,
                PortKind::Assignment,
                "DATABASE_PORT=\"5433\"".to_string()
            )]
        );
        assert_eq!(
            literals("wait-on http://localhost:3005/health"),
            vec![(3005, PortKind::Url, "localhost:3005".to_string())]
        );
        assert_eq!(
            literals("psql -h 127.0.0.1:5433"),
            vec![(5433, PortKind::Url, "127.0.0.1:5433".to_string())]
        );
    }

    #[test]
    fn variable_expansions_are_not_literals() {
        for text in [
            "${WT_PORT_DB:-5432}:5432",
            "-p ${APP_PORT:-3005}",
            "--port=${PORT}",
            "PORT=${PORT:-3005}",
            "wait-on http://localhost:${PORT}",
        ] {
            assert!(scan(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn small_numbers_after_a_colon_are_not_ports() {
        assert!(scan("12:30 and 3:2").is_empty());
    }

    #[test]
    fn rewrites_flags_and_drops_inline_assignments() {
        assert_eq!(
            rewrite("next dev --turbo -p 3005", "APP_PORT").as_deref(),
            Some("next dev --turbo -p ${APP_PORT:-3005}")
        );
        assert_eq!(
            rewrite("wait-on http://localhost:3005/health", "APP_PORT").as_deref(),
            Some("wait-on http://localhost:${APP_PORT:-3005}/health")
        );
        assert_eq!(
            rewrite("PORT=3005 vite", "APP_PORT").as_deref(),
            Some("vite")
        );
        assert_eq!(rewrite("vite dev", "APP_PORT"), None);
    }
}
