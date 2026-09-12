//! Shell completion output: generated for every supported shell and complete.

mod support;

use clap::{CommandFactory, ValueEnum};
use clap_complete::Shell;
use magictree::Cli;
use support::{run, Fixture};

fn generate(shell: Shell) -> String {
    let mut command = Cli::command();
    let mut buffer = Vec::new();
    clap_complete::generate(shell, &mut command, "magictree", &mut buffer);
    String::from_utf8(buffer).expect("completion output is UTF-8")
}

fn subcommands() -> Vec<String> {
    Cli::command()
        .get_subcommands()
        .map(|command| command.get_name().to_string())
        .collect()
}

#[test]
fn every_supported_shell_produces_a_script() {
    let shells: Vec<Shell> = Shell::value_variants().to_vec();
    assert!(!shells.is_empty());
    for shell in shells {
        let script = generate(shell);
        assert!(
            !script.trim().is_empty(),
            "{shell} produced an empty completion script"
        );
        assert!(
            script.contains("magictree"),
            "{shell} script does not mention the command name"
        );
    }
}

#[test]
fn completions_cover_every_subcommand_in_every_shell() {
    // The generated scripts must stay in step with the CLI: this fails if a
    // subcommand is added that a shell cannot complete.
    let commands = subcommands();
    assert!(commands.len() > 5, "sanity: the CLI has subcommands");
    for shell in Shell::value_variants() {
        let script = generate(*shell);
        for command in &commands {
            assert!(
                script.contains(command.as_str()),
                "{shell} completion omits subcommand '{command}'"
            );
        }
    }
}

#[test]
fn completions_include_nested_argument_values_where_the_shell_supports_them() {
    // bash and zsh emit candidates for positional values; the fish, elvish and
    // powershell generators only emit subcommands and flags.
    for shell in [Shell::Bash, Shell::Zsh] {
        let script = generate(shell);
        assert!(
            script.contains("powershell"),
            "{shell} completion does not list the shell argument values"
        );
    }
}

#[test]
fn completions_include_flags() {
    // Fish writes long options as `-l dry-run` rather than `--dry-run`, so match
    // the name without its dashes.
    for shell in Shell::value_variants() {
        let script = generate(*shell);
        for flag in ["dry-run", "cwd", "report"] {
            assert!(script.contains(flag), "{shell} completion omits {flag}");
        }
    }
}

#[test]
fn the_cli_prints_completions_to_stdout() {
    let fixture = Fixture::new();
    let state = fixture.state_dir();
    for shell in ["zsh", "bash", "fish"] {
        let result = run(&["completion", shell], fixture.path(), &state);
        assert!(result.ok(), "completion {shell}: {}", result.combined());
        assert!(
            result.stdout.contains("magictree"),
            "completion {shell} printed nothing useful"
        );
    }
}

#[test]
fn an_unknown_shell_is_rejected() {
    let fixture = Fixture::new();
    let state = fixture.state_dir();
    let result = run(&["completion", "tcsh"], fixture.path(), &state);
    assert!(!result.ok());
    assert!(
        result.stderr.contains("possible values"),
        "the error should list the supported shells: {}",
        result.stderr
    );
}

#[test]
fn completion_works_outside_a_repository() {
    // Completion must not require a manifest, or it breaks in every directory.
    let fixture = Fixture::new();
    let state = fixture.state_dir();
    let result = run(&["completion", "zsh"], fixture.path(), &state);
    assert!(result.ok(), "{}", result.combined());
}
