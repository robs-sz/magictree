//! Shell activation.
//!
//! `magictree env` resolves a worktree's environment, but it only reaches a
//! process magictree launched itself. A shell that evaluates
//! `magictree activate <shell>` keeps its own environment in step with the
//! worktree it sits in, so anything started afterwards — an agent, an editor, a
//! recipe — reads the same ports and `[env]` a service would, without wrapping
//! every command in `magictree exec`.
//!
//! The sync reads the mirror `up` writes in magictree's state dir rather than
//! rebuilding the plan: a prompt hook costs one file read, allocates nothing,
//! and keeps the repository's promise that nothing is ever written into a
//! checkout. The mirror carries the workspace layers (computed ports and
//! `[env]`); a running stack writes it, so activation is a no-op in a worktree
//! whose stack was never started.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ctx::runtime_dir_for;
use crate::env::quote;
use crate::paths::Paths;
use crate::repo::Repo;

/// Shells `activate` knows how to hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    pub fn name(self) -> &'static str {
        match self {
            Shell::Zsh => "zsh",
            Shell::Bash => "bash",
            Shell::Fish => "fish",
        }
    }

    /// The shell `$SHELL` names, when it is one we can hook.
    ///
    /// Login shells are named by path (`/bin/zsh`), and a shell we do not hook
    /// (`elvish`, `pwsh`, `nu`) is reported as unknown rather than guessed at.
    pub fn from_shell_env(value: Option<&str>) -> Option<Self> {
        match Path::new(value?).file_name()?.to_str()? {
            "zsh" => Some(Shell::Zsh),
            "bash" => Some(Shell::Bash),
            "fish" => Some(Shell::Fish),
            _ => None,
        }
    }
}

/// The snippet a shell's rc file evaluates once, usually with
/// `eval "$(magictree activate zsh)"`.
///
/// It defines a sync function, hooks it to the directory change and the prompt
/// of `shell`, and runs it once for the shell that is reading it. The function
/// applies the environment of the worktree it is called in and unsets the keys
/// a previous worktree exported, so leaving a checkout does not leave its ports
/// behind.
pub fn snippet(shell: Shell, binary: &Path) -> String {
    let binary = quote(&binary.to_string_lossy());
    let template = match shell {
        Shell::Zsh => ZSH,
        Shell::Bash => BASH,
        Shell::Fish => FISH,
    };
    template.replace("__MAGICTREE_BIN__", &binary)
}

const ZSH: &str = r##"# magictree activate (zsh) — keep this shell's environment in step with the
# worktree it sits in. Evaluate once from ~/.zshrc:
#   eval "$(magictree activate zsh)"

__magictree_bin=__MAGICTREE_BIN__
typeset -ga __magictree_keys=()

__magictree_sync() {
  local lines line key keys=""
  lines="$("$__magictree_bin" activate --emit zsh 2>/dev/null)" || return 0
  for line in ${(f)lines}; do
    [[ $line == "# magictree keys: "* ]] && keys="${line#"# magictree keys: "}"
  done
  for key in ${=__magictree_keys}; do
    [[ " $keys " == *" $key "* ]] || unset "$key"
  done
  [[ -n $lines ]] && eval "$lines"
  __magictree_keys=(${=keys})
}

autoload -Uz add-zsh-hook
add-zsh-hook chpwd __magictree_sync
add-zsh-hook precmd __magictree_sync
__magictree_sync
"##;

const BASH: &str = r##"# magictree activate (bash) — keep this shell's environment in step with the
# worktree it sits in. Evaluate once from ~/.bashrc:
#   eval "$(magictree activate bash)"

__magictree_bin=__MAGICTREE_BIN__
__MAGICTREE_KEYS=""

__magictree_sync() {
  local lines line key keys=""
  lines="$("$__magictree_bin" activate --emit bash 2>/dev/null)" || return 0
  while IFS= read -r line; do
    case $line in
      "# magictree keys: "*) keys="${line#"# magictree keys: "}" ;;
    esac
  done <<<"$lines"
  for key in $__MAGICTREE_KEYS; do
    case " $keys " in
      *" $key "*) ;;
      *) unset "$key" ;;
    esac
  done
  [ -n "$lines" ] && eval "$lines"
  __MAGICTREE_KEYS="$keys"
}

case ";${PROMPT_COMMAND:-};" in
  *";__magictree_sync;"*) ;;
  *) PROMPT_COMMAND="__magictree_sync${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac
__magictree_sync
"##;

const FISH: &str = r##"# magictree activate (fish) — keep this shell's environment in step with the
# worktree it sits in. Evaluate once from ~/.config/fish/config.fish:
#   magictree activate fish | source

set -g __magictree_bin __MAGICTREE_BIN__
set -g __magictree_keys

function __magictree_sync --on-variable PWD --on-event fish_prompt
    set -l lines ("$__magictree_bin" activate --emit fish 2>/dev/null)
    set -l keys
    for line in $lines
        if string match -q '# magictree keys: *' -- $line
            set keys (string split ' ' -- (string replace '# magictree keys: ' '' -- $line))
        end
    end
    for key in $__magictree_keys
        contains -- $key $keys; or set -e $key
    end
    if test (count $lines) -gt 0
        eval (string join \n $lines)
    end
    set -g __magictree_keys $keys
end

__magictree_sync
"##;

/// The environment of the worktree enclosing `cwd`, in `shell` syntax.
///
/// Reads the mirror `up` wrote (`<runtime dir>/env`) instead of rebuilding the
/// plan, so a hook never allocates ports and never fails because a manifest
/// gained a problem since the stack started. Nothing is printed when `cwd` is
/// outside a repository, or inside one whose stack was never started: the hook
/// then has nothing to sync, and the caller's environment is left alone.
pub fn emit(shell: Shell, cwd: &Path) -> Result<String> {
    let paths = Paths::new()?;
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let Some(repo) = Repo::open_optional(&cwd)? else {
        return Ok(String::new());
    };
    let mirror = runtime_dir_for(&paths, &repo).join("env");
    let Ok(contents) = std::fs::read_to_string(mirror) else {
        return Ok(String::new());
    };
    Ok(render(shell, &contents))
}

/// Rewrite the dotenv mirror into `shell` assignments, followed by the list of
/// keys they set.
///
/// Values keep the quoting `up` wrote them with: `quote` emits double-quoted
/// strings with backslash escapes, which zsh, bash and fish all read the same
/// way, so a value with spaces or quotes survives the round trip. The trailing
/// comment names the keys — a hook has to know which ones a previous worktree
/// exported, and a comment is inert under `eval` while staying trivial to read
/// in every shell.
fn render(shell: Shell, mirror: &str) -> String {
    let mut assignments = String::new();
    let mut keys: Vec<&str> = Vec::new();
    for line in mirror.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        keys.push(key);
        match shell {
            Shell::Fish => assignments.push_str(&format!("set -gx {key} {value}\n")),
            Shell::Zsh | Shell::Bash => assignments.push_str(&format!("export {line}\n")),
        }
    }
    if keys.is_empty() {
        return String::new();
    }
    assignments.push_str(&format!("# magictree keys: {}\n", keys.join(" ")));
    assignments
}

/// The rc file `--install` appends to.
///
/// zsh honors `$ZDOTDIR` the way zsh itself does (`${ZDOTDIR:-$HOME}/.zshrc`);
/// bash and fish are fixed (`~/.bashrc`, `~/.config/fish/config.fish`).
pub fn rc_file(shell: Shell, home: &Path, zdotdir: Option<&Path>) -> PathBuf {
    match shell {
        Shell::Zsh => zdotdir.unwrap_or(home).join(".zshrc"),
        Shell::Bash => home.join(".bashrc"),
        Shell::Fish => home.join(".config/fish/config.fish"),
    }
}

/// The comment that ends the line and the string `install` looks for to stay
/// idempotent.
///
/// It names the shell, so a file that already carries one shell's hook is not
/// mistaken for another's, and it tells a reader of the rc file where the line
/// came from.
fn marker(shell: Shell) -> String {
    format!("# added by magictree activate {}", shell.name())
}

/// The line `--install` appends: how this shell evaluates the snippet, then the
/// marker.
fn activation_line(shell: Shell, binary: &Path) -> String {
    let binary = quote(&binary.to_string_lossy());
    let marker = marker(shell);
    match shell {
        Shell::Zsh | Shell::Bash => {
            format!("eval \"$({binary} activate {})\" {marker}", shell.name())
        }
        Shell::Fish => format!("{binary} activate fish | source {marker}"),
    }
}

/// What [`install`] did, so the caller can report it.
pub struct Install {
    /// The rc file that was written, or already held the marker.
    pub rc: PathBuf,
    /// The line appended, exactly as it was written.
    pub line: String,
    /// False when the marker was already in the file, so nothing was appended.
    pub wrote: bool,
}

/// Append this shell's activation line to its rc file.
///
/// The file is never parsed and never rewritten, only appended to, and the
/// marker keeps a rerun a no-op. A file that already carries an equivalent hook
/// added by hand has no marker, so this cannot see it: it appends a second line,
/// which is why `wrote` is reported rather than assumed.
pub fn install(
    shell: Shell,
    binary: &Path,
    home: &Path,
    zdotdir: Option<&Path>,
) -> Result<Install> {
    let rc = rc_file(shell, home, zdotdir);
    let line = activation_line(shell, binary);
    // A file we cannot read as UTF-8 is treated as without a marker. The worst
    // that costs is one extra line; nothing already in the file is ever changed.
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains(&marker(shell)) {
        return Ok(Install {
            rc,
            line,
            wrote: false,
        });
    }
    if let Some(parent) = rc.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rc)
        .with_context(|| format!("opening {}", rc.display()))?;
    writeln!(file, "\n{line}").with_context(|| format!("appending to {}", rc.display()))?;
    Ok(Install {
        rc,
        line,
        wrote: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_posix_exports_and_names_the_keys() {
        let mirror = "MAGICTREE_PORT_web_web=21789\nSTORYBOOK_PORT=21790\nEMPTY=\"\"\n";
        let expected = concat!(
            "export MAGICTREE_PORT_web_web=21789\n",
            "export STORYBOOK_PORT=21790\n",
            "export EMPTY=\"\"\n",
            "# magictree keys: MAGICTREE_PORT_web_web STORYBOOK_PORT EMPTY\n",
        );
        assert_eq!(render(Shell::Zsh, mirror), expected);
        assert_eq!(render(Shell::Bash, mirror), expected);
    }

    #[test]
    fn emits_fish_globals_that_are_exported() {
        let mirror = "PORT=21790\nGREETING=\"hi there\"\n";
        let expected = concat!(
            "set -gx PORT 21790\n",
            "set -gx GREETING \"hi there\"\n",
            "# magictree keys: PORT GREETING\n",
        );
        assert_eq!(render(Shell::Fish, mirror), expected);
    }

    #[test]
    fn skips_blank_and_commented_mirror_lines() {
        assert_eq!(
            render(Shell::Zsh, "\n# note\nA=1\n\n"),
            "export A=1\n# magictree keys: A\n"
        );
        assert_eq!(render(Shell::Fish, ""), "");
    }

    #[test]
    fn snippets_hook_the_directory_change_and_the_prompt() {
        let binary = Path::new("/opt/bin/magictree");
        let zsh = snippet(Shell::Zsh, binary);
        assert!(zsh.contains("add-zsh-hook chpwd __magictree_sync"));
        assert!(zsh.contains("add-zsh-hook precmd __magictree_sync"));
        assert!(zsh.contains("__magictree_bin=/opt/bin/magictree"));
        assert!(zsh.contains("activate --emit zsh"));

        let bash = snippet(Shell::Bash, binary);
        assert!(bash.contains("PROMPT_COMMAND="));
        assert!(bash.contains("activate --emit bash"));

        let fish = snippet(Shell::Fish, binary);
        assert!(fish.contains("--on-variable PWD"));
        assert!(fish.contains("activate --emit fish"));
    }

    #[test]
    fn every_snippet_reads_the_key_list_the_emitter_writes() {
        for shell in [Shell::Zsh, Shell::Bash, Shell::Fish] {
            assert!(
                snippet(shell, Path::new("/opt/bin/magictree")).contains("# magictree keys: "),
                "{} must parse the key list",
                shell.name()
            );
        }
    }

    #[test]
    fn a_binary_path_with_spaces_survives_quoting() {
        let zsh = snippet(Shell::Zsh, Path::new("/opt/my tools/magictree"));
        assert!(zsh.contains(r#"__magictree_bin="/opt/my tools/magictree""#));
    }

    #[test]
    fn only_the_shells_we_hook_are_recognised() {
        assert_eq!(Shell::from_shell_env(Some("/bin/zsh")), Some(Shell::Zsh));
        assert_eq!(Shell::from_shell_env(Some("bash")), Some(Shell::Bash));
        assert_eq!(Shell::from_shell_env(Some("fish")), Some(Shell::Fish));
        assert_eq!(Shell::from_shell_env(Some("/usr/bin/elvish")), None);
        assert_eq!(Shell::from_shell_env(None), None);
    }

    #[test]
    fn rc_file_follows_each_shell_and_zdotdir() {
        let home = Path::new("/home/dev");
        let zdotdir = Path::new("/home/dev/zdot");
        assert_eq!(rc_file(Shell::Zsh, home, None), home.join(".zshrc"));
        assert_eq!(
            rc_file(Shell::Zsh, home, Some(zdotdir)),
            zdotdir.join(".zshrc")
        );
        assert_eq!(
            rc_file(Shell::Bash, home, Some(zdotdir)),
            home.join(".bashrc")
        );
        assert_eq!(
            rc_file(Shell::Fish, home, None),
            home.join(".config/fish/config.fish")
        );
    }

    #[test]
    fn activation_lines_evaluate_the_snippet_and_carry_the_marker() {
        let binary = Path::new("/opt/bin/magictree");
        assert_eq!(
            activation_line(Shell::Zsh, binary),
            r#"eval "$(/opt/bin/magictree activate zsh)" # added by magictree activate zsh"#
        );
        assert_eq!(
            activation_line(Shell::Bash, binary),
            r#"eval "$(/opt/bin/magictree activate bash)" # added by magictree activate bash"#
        );
        assert_eq!(
            activation_line(Shell::Fish, binary),
            r#"/opt/bin/magictree activate fish | source # added by magictree activate fish"#
        );
    }

    #[test]
    fn activation_lines_quote_a_binary_path_with_spaces() {
        let binary = Path::new("/opt/my tools/magictree");
        assert_eq!(
            activation_line(Shell::Zsh, binary),
            r#"eval "$("/opt/my tools/magictree" activate zsh)" # added by magictree activate zsh"#
        );
        assert_eq!(
            activation_line(Shell::Fish, binary),
            r#""/opt/my tools/magictree" activate fish | source # added by magictree activate fish"#
        );
    }

    #[test]
    fn install_appends_once_and_is_a_no_op_after_that() {
        let home = tempfile::tempdir().expect("temp home");
        let binary = Path::new("/opt/bin/magictree");

        let first = install(Shell::Zsh, binary, home.path(), None).expect("install");
        assert!(first.wrote);
        let contents = std::fs::read_to_string(home.path().join(".zshrc")).expect("read rc");
        assert!(contents.contains(&first.line), "{contents}");
        assert!(
            contents.ends_with('\n'),
            "the file stays newline-terminated"
        );

        let second = install(Shell::Zsh, binary, home.path(), None).expect("install again");
        assert!(!second.wrote);
        let contents = std::fs::read_to_string(home.path().join(".zshrc")).expect("read rc");
        assert_eq!(
            contents
                .matches("# added by magictree activate zsh")
                .count(),
            1
        );
    }

    #[test]
    fn install_writes_into_zdotdir_and_creates_fish_directories() {
        let home = tempfile::tempdir().expect("temp home");
        let zdotdir = home.path().join("zdot");
        std::fs::create_dir_all(&zdotdir).expect("create zdotdir");
        let binary = Path::new("/opt/bin/magictree");

        install(Shell::Zsh, binary, home.path(), Some(&zdotdir)).expect("install zsh");
        assert!(zdotdir.join(".zshrc").is_file());
        assert!(
            !home.path().join(".zshrc").exists(),
            "ZDOTDIR wins over HOME"
        );

        install(Shell::Fish, binary, home.path(), None).expect("install fish");
        assert!(home.path().join(".config/fish/config.fish").is_file());
    }
}
