use anyhow::{anyhow, Result};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvSource {
    Computed,
    Workspace,
    App,
}

impl EnvSource {
    pub fn label(&self) -> &'static str {
        match self {
            EnvSource::Computed => "computed",
            EnvSource::Workspace => "workspace",
            EnvSource::App => "app",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnvPlan {
    pub vars: BTreeMap<String, String>,
    pub sources: BTreeMap<String, EnvSource>,
}

impl EnvPlan {
    pub fn dotenv(&self) -> String {
        let mut out = String::new();
        for (key, value) in &self.vars {
            out.push_str(&format!("{key}={}\n", quote(value)));
        }
        out
    }

    pub fn export(&self) -> String {
        let mut out = String::new();
        for (key, value) in &self.vars {
            out.push_str(&format!("export {key}={}\n", quote(value)));
        }
        out
    }

    pub fn explain(&self) -> String {
        let width = self.vars.keys().map(|key| key.len()).max().unwrap_or(0);
        let mut out = String::new();
        for (key, value) in &self.vars {
            let source = self
                .sources
                .get(key)
                .map(|source| source.label())
                .unwrap_or("unknown");
            out.push_str(&format!("{key:width$}  {source:<9}  {value}\n"));
        }
        out
    }
}

/// Merge the environment layers. Computed values always win; app values
/// override workspace values. Values may reference `${NAME}` from the
/// computed layer or from values resolved earlier in this call.
pub fn build(
    computed: BTreeMap<String, String>,
    workspace_env: &BTreeMap<String, String>,
    app_env: &BTreeMap<String, String>,
) -> Result<EnvPlan> {
    let mut plan = EnvPlan {
        vars: BTreeMap::new(),
        sources: BTreeMap::new(),
    };
    for (key, value) in computed {
        plan.sources.insert(key.clone(), EnvSource::Computed);
        plan.vars.insert(key, value);
    }
    for (source, layer) in [
        (EnvSource::Workspace, workspace_env),
        (EnvSource::App, app_env),
    ] {
        for (key, raw) in layer {
            if plan.sources.contains_key(key) && plan.sources[key] == EnvSource::Computed {
                return Err(anyhow!(
                    "environment key '{key}' is set by magictree and cannot be overridden"
                ));
            }
            let value = interpolate(raw, &plan.vars)?;
            plan.sources.insert(key.clone(), source);
            plan.vars.insert(key.clone(), value);
        }
    }
    Ok(plan)
}

/// Rewrite HTTP URLs that use an opted-in assigned localhost port.
///
/// Only the URL authority is changed; unrelated localhost URLs and non-HTTP
/// values remain untouched.
pub fn rewrite_localhost_urls(value: &str, aliases: &BTreeMap<u16, String>) -> Option<String> {
    const PREFIXES: [(&str, &str); 4] = [
        ("http://localhost:", "http://"),
        ("https://localhost:", "https://"),
        ("ws://localhost:", "ws://"),
        ("wss://localhost:", "wss://"),
    ];

    if aliases.is_empty() || !value.contains("localhost:") {
        return None;
    }

    let mut output = None;
    let mut cursor = 0;
    while cursor < value.len() {
        let Some((start, marker, scheme)) = PREFIXES
            .iter()
            .filter_map(|(marker, scheme)| {
                value[cursor..]
                    .find(marker)
                    .map(|offset| (cursor + offset, *marker, *scheme))
            })
            .min_by_key(|(start, _, _)| *start)
        else {
            break;
        };
        let port_start = start + marker.len();
        let port_end = value.as_bytes()[port_start..]
            .iter()
            .position(|byte| !byte.is_ascii_digit())
            .map(|offset| port_start + offset)
            .unwrap_or(value.len());
        if port_start == port_end {
            cursor = port_end;
            continue;
        }
        let boundary = value.as_bytes().get(port_end).map_or(true, |byte| {
            !byte.is_ascii_alphanumeric() && !b"-._:".contains(byte)
        });
        if !boundary {
            cursor = port_end;
            continue;
        }
        let Ok(port) = value[port_start..port_end].parse::<u16>() else {
            cursor = port_end;
            continue;
        };
        let Some(host) = aliases.get(&port) else {
            cursor = port_end;
            continue;
        };

        let output = output.get_or_insert_with(|| String::with_capacity(value.len()));
        output.push_str(&value[cursor..start]);
        output.push_str(scheme);
        output.push_str(host);
        output.push(':');
        output.push_str(&value[port_start..port_end]);
        cursor = port_end;
    }

    let mut output = output?;
    output.push_str(&value[cursor..]);
    Some(output)
}

fn interpolate(value: &str, resolved: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::new();
    let mut rest = value;
    loop {
        let Some(start) = rest.find("${") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            break;
        };
        let name = &after[..end];
        let replacement = resolved.get(name).ok_or_else(|| {
            anyhow!("unknown variable '${{{name}}}' in environment value '{value}'")
        })?;
        out.push_str(replacement);
        rest = &after[end + 1..];
    }
    Ok(out)
}

fn quote(value: &str) -> String {
    if value.is_empty() {
        return "\"\"".to_string();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./:@,+=".contains(ch))
    {
        return value.to_string();
    }
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
