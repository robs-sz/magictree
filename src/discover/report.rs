use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const REPORT_VERSION: u32 = 1;
pub const ANSWERS_VERSION: u32 = 1;

/// Everything discovery could determine, plus what it could not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub report_version: u32,
    /// Fingerprint of the facts. Answers are only accepted for the report they
    /// were computed from, which keeps `init` a pure function.
    pub report_hash: String,
    pub repo: RepoFacts,
    pub apps: Vec<AppFacts>,
    pub facts: Vec<Fact>,
    pub unknowns: Vec<Unknown>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoFacts {
    pub root: String,
    pub git_common_dir: String,
    pub is_git_repo: bool,
    pub worktrees: Vec<WorktreeFact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeFact {
    pub path: String,
    pub branch: Option<String>,
    pub is_main: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppFacts {
    pub id: String,
    /// Path relative to the repository root.
    pub dir: String,
    /// Where this app was discovered from.
    pub source: String,
}

/// One extracted fact, traceable back to the file it came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: String,
    /// Path relative to the repository root.
    pub source: String,
    pub kind: FactKind,
    /// App this fact belongs to, when it is app-scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub confidence: Confidence,
    pub data: FactData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    Compose,
    Node,
    Storybook,
    Workspace,
    Mise,
    Just,
    Python,
    EnvExample,
    Procfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// Extracted data. Every variant is a plain description of what a file said.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FactData {
    Compose {
        services: Vec<ComposeServiceFact>,
        has_build: bool,
        files: Vec<String>,
    },
    Node {
        name: Option<String>,
        package_manager: Option<String>,
        scripts: Vec<ScriptFact>,
        has_workspaces: bool,
        dependencies: Vec<String>,
        /// Ports written into a script rather than read from the environment.
        #[serde(default)]
        ports: Vec<PortLiteralFact>,
    },
    Storybook {
        /// Configuration directory Storybook reads, when the app has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_dir: Option<String>,
        /// Steps that serve the dev server, in the `<runner>:<script>` form the
        /// run answers use.
        #[serde(default)]
        dev_scripts: Vec<String>,
        /// `@storybook/addon-mcp` is installed, so the dev server also answers
        /// the MCP endpoint an agent connects to.
        has_mcp: bool,
    },
    Workspace {
        tool: String,
        packages: Vec<String>,
    },
    Mise {
        tasks: Vec<String>,
        tools: Vec<String>,
        has_env: bool,
        has_profiles: bool,
    },
    Just {
        recipes: Vec<String>,
        modules: Vec<String>,
        /// Variables the justfile reads from the environment. Only these can
        /// carry a value in from outside.
        #[serde(default)]
        env_variables: Vec<String>,
        /// Variables the justfile sets for a recipe (`$NAME=default`). These are
        /// outputs: an ambient value is overwritten, never read.
        #[serde(default)]
        exported_parameters: Vec<String>,
        /// Recipe name to its body, so a direct command can be recovered from
        /// the tooling without inventing one.
        #[serde(default)]
        recipe_bodies: std::collections::BTreeMap<String, String>,
        /// Just variable to the environment variable it is read from.
        #[serde(default)]
        variables: std::collections::BTreeMap<String, String>,
        /// Ports written into a recipe body.
        #[serde(default)]
        ports: Vec<PortLiteralFact>,
    },
    Python {
        manager: String,
        has_uv_lock: bool,
        dependency_groups: Vec<String>,
        scripts: Vec<String>,
    },
    EnvExample {
        variables: Vec<String>,
        file: String,
        /// Ports pinned to a number in the template.
        #[serde(default)]
        ports: Vec<PortLiteralFact>,
    },
    Procfile {
        processes: Vec<ScriptFact>,
        /// Ports pinned by a process command.
        #[serde(default)]
        ports: Vec<PortLiteralFact>,
    },
}

/// A port written into a file rather than read from the environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortLiteralFact {
    pub port: u16,
    pub kind: PortKind,
    /// The literal as written: `-p 3005`, `--port=8000`, `DATABASE_PORT=5433`,
    /// `localhost:5173`.
    pub literal: String,
    /// Step it sits in: a script, recipe, or task name; a variable name in an
    /// env file; absent for a raw command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// The text containing it, so the fix can be shown in context.
    pub text: String,
}

/// How the port was written, which decides what the fix looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortKind {
    /// A command-line flag: `-p 3005`, `--port=8000`.
    Flag,
    /// An inline or env-file assignment: `PORT=3005`.
    Assignment,
    /// A URL or address literal: `localhost:5173`.
    Url,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeServiceFact {
    pub name: String,
    pub image: Option<String>,
    pub has_build: bool,
    /// Host:container mappings as written, including variable defaults.
    pub ports: Vec<String>,
    pub depends_on: Vec<String>,
    pub has_healthcheck: bool,
    pub profiles: Vec<String>,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptFact {
    pub name: String,
    pub command: String,
}

/// A question the CLI could not answer from the repository alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unknown {
    pub id: String,
    pub scope: Scope,
    pub kind: UnknownKind,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum Scope {
    Workspace,
    App { app: String },
}

impl Scope {
    pub fn app(&self) -> Option<&str> {
        match self {
            Scope::Workspace => None,
            Scope::App { app } => Some(app),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Scope::Workspace => "workspace".to_string(),
            Scope::App { app } => app.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownKind {
    Choice,
    MultiChoice,
    Bool,
    Text,
}

/// Answers to a report's unknowns, produced by the wizard or an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerSet {
    pub answers_version: u32,
    pub report_hash: String,
    pub answers: std::collections::BTreeMap<String, Answer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Answer {
    One(String),
    Many(Vec<String>),
    Flag(bool),
}

impl Answer {
    pub fn as_one(&self) -> Option<&str> {
        match self {
            Answer::One(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_many(&self) -> Vec<String> {
        match self {
            Answer::Many(values) => values.clone(),
            Answer::One(value) => vec![value.clone()],
            Answer::Flag(_) => Vec::new(),
        }
    }

    pub fn as_flag(&self) -> Option<bool> {
        match self {
            Answer::Flag(value) => Some(*value),
            _ => None,
        }
    }
}

impl AnswerSet {
    pub fn read_json(path: &Path) -> Result<Self> {
        read_json(path)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        write_json(path, self)
    }
}

impl Report {
    /// Stable fingerprint over facts and apps, independent of formatting.
    pub fn compute_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.report_version.to_le_bytes());
        for app in &self.apps {
            hasher.update(app.id.as_bytes());
            hasher.update(app.dir.as_bytes());
            hasher.update(app.source.as_bytes());
        }
        let mut facts: Vec<String> = self
            .facts
            .iter()
            .map(|fact| serde_json::to_string(fact).unwrap_or_default())
            .collect();
        facts.sort();
        for fact in facts {
            hasher.update(fact.as_bytes());
        }
        format!("sha256:{:x}", hasher.finalize())
    }

    pub fn finalize(mut self) -> Self {
        self.report_hash = self.compute_hash();
        self
    }

    pub fn unknowns_for(&self, scope: &Scope) -> Vec<&Unknown> {
        self.unknowns
            .iter()
            .filter(|unknown| match (&unknown.scope, scope) {
                (Scope::Workspace, Scope::Workspace) => true,
                (Scope::App { app: left }, Scope::App { app: right }) => left == right,
                _ => false,
            })
            .collect()
    }

    pub fn fact_evidence(&self, id: &str) -> Option<&Fact> {
        self.facts.iter().find(|fact| fact.id == id)
    }

    /// The steps an app declares that serve Storybook, as `<runner>:<script>`.
    ///
    /// They are the ones that need their port passed on the command line:
    /// Storybook's dev server reads no environment variable of its own.
    pub fn storybook_scripts(&self, app: &str) -> Vec<&str> {
        self.facts
            .iter()
            .filter(|fact| fact.app.as_deref() == Some(app))
            .filter_map(|fact| match &fact.data {
                FactData::Storybook { dev_scripts, .. } => {
                    Some(dev_scripts.iter().map(String::as_str))
                }
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// True when an app installs the addon that answers MCP from the dev server.
    pub fn storybook_has_mcp(&self, app: &str) -> bool {
        self.facts
            .iter()
            .filter(|fact| fact.app.as_deref() == Some(app))
            .any(|fact| match &fact.data {
                FactData::Storybook { has_mcp, .. } => *has_mcp,
                _ => false,
            })
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        write_json(path, self)
    }

    pub fn read(path: &Path) -> Result<Self> {
        read_json(path)
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let payload = serde_json::to_string_pretty(value)?;
    std::fs::write(path, format!("{payload}\n"))
        .with_context(|| format!("writing {}", path.display()))
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Absolute path helper used by extractors.
pub fn join(root: &Path, relative: &str) -> PathBuf {
    root.join(relative)
}
