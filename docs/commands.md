# Commands

| | |
|---|---|
| `discover`, `init`, `doctor` | read the repository, write manifests, detect drift |
| `up`, `down`, `restart`, `status` | start, stop, restart specific services, inspect a worktree's stack |
| *(no command)* | the TUI: every service's state, its log, ports and environment on one screen, with `up`, `restart`, `down` a key away |
| `logs`, `env`, `ports` | service output (one service, or `--all` for the whole stack, Compose included), resolved environment, port assignment |
| `exec` | run a command with this worktree's resolved environment |
| `activate` | keep a shell's own environment in step with the worktree it sits in |
| `new`, `rm`, `list`, `gc` | worktree lifecycle and resource reclamation |
| `completion` | a shell completion script, printed to stdout |
| `update` | install the latest release over this binary |

`--dry-run` works on every command and creates nothing: `magictree -n up`.

`up` prints a numbered step per unit of work. `up --quiet` keeps only the step headers,
failures, and the URL summary; `up --verbose` streams a bootstrap command's output under its
step. With `reconcile = "auto"` in `config.toml`, `up --refresh` forces a full Compose
reconcile for one run.

Flags with an unambiguous short form carry one — `-n` for `--dry-run`, `-C` for `--cwd`,
`-f` for `--force`, `-v` for `--volumes`, `-b` for `--build`, `-e` for `--export`, `-A` for
`--all`, and so on.
A letter is only reused where the command has no clash, so each command's `--help` is the
list for that command.

```sh
magictree exec -- just api::seed    # run a command as if the stack had launched it
```

`exec` takes the command and its arguments verbatim (no shell), gives them this worktree's
resolved ports and `[env]` on top of your own environment, and runs them in the directory
`--cwd` names (the current one by default). The child owns the terminal and its exit status
becomes magictree's, so a recipe can delegate to it instead of hand-rolling
`eval "$(magictree env --export)"`. Use `magictree exec -- sh -c '...'` when you need a
shell.

## The TUI

`magictree` on its own opens the TUI on the worktree it is run in. The screen is a row of
sections — `services`, `worktrees`, `ports`, `env`, `logs` — and a digit, `tab`, or `:` and
the section's name (or an alias: `:svc`, `:wt`, `:p`, `:e`, `:logs web`) switches between
them. In `services`, `l` (or `enter`) follows the selected service's log, `s` starts it with
what it needs, `r` restarts it, `u` runs `up` and `d` runs `down` after a `y` (those two
also from `ports` and `env`), and
`o` opens its URL (its `browser_alias` one, shown in its place, when the port has one). The
prompt only navigates — sections and `:q` — so nothing that changes a stack is ever a typed
word. Each key runs the command a shell would, and its output stays on screen; under `-n`, each is a
dry run. `worktrees` lists every worktree magictree has recorded: `enter` opens that
one's stack, `x` removes it (`magictree rm`, after a `y`: a dirty checkout is refused and
the branch is kept), and `c` runs `magictree gc --all` to reclaim what gone ones left
behind. Outside a repository the screen starts there. Without a terminal, a bare
`magictree` prints the help instead.


## Completion

`completion` prints a script for bash, zsh, fish, elvish or powershell:

```sh
magictree completion zsh  > ~/.zsh/completions/_magictree
magictree completion bash > ~/.local/share/bash-completion/completions/magictree
magictree completion fish > ~/.config/fish/completions/magictree.fish
```

zsh loads a completion only from a directory on `$fpath`, so `~/.zsh/completions` has to be on
`$fpath` *before* `compinit` runs: put it there in `.zshrc`, above the line that sets up
completion, and open a new shell. A shell that was already running reuses its `~/.zcompdump`,
keeps the registration even when the file it names has moved, and fails on Tab with
`_magictree: function definition file not found` until the shell is restarted.

## Activation

`magictree env` reaches a process magictree launched, and `exec` wraps one command. A shell
can instead keep its *own* environment in step with the worktree it sits in, so anything
started from it afterwards — an agent, an editor, a recipe — reads the same ports and `[env]`
a service does. Evaluate one line from the shell's rc file:

```sh
eval "$(magictree activate zsh)"      # or bash
magictree activate fish | source      # or fish
```

zsh, bash and fish are supported; name the shell as the argument, and `$SHELL` picks it when
it is omitted. The snippet hooks the directory change and the prompt, exports the environment
`up` wrote, and unsets the keys a previous worktree exported, so leaving a checkout leaves
nothing behind.

`magictree activate <shell> --install` appends that line to the shell's rc file for you —
`~/.zshrc` (honoring `$ZDOTDIR`), `~/.bashrc`, or `~/.config/fish/config.fish` — and reports
the path and how to load it. It only ever appends, never rewrites, and a marker comment keeps
a second run from adding the line twice.

It syncs from the environment mirror `up` writes in the state dir (`env` under the worktree's
runtime directory) rather than rebuilding the plan: activation costs a file read, allocates
nothing, and stays a no-op in a worktree whose stack was never started. That also means the
mirror carries the workspace layers — the computed ports and `[env]` — so an app's own `[env]`
still wants `magictree env --app <app>` or `exec --app <app>`.

`magictree activate --emit <shell>` prints that environment once, in the shell's syntax, for a
script or a tool that reads it directly from the process environment instead of from a
snippet.

## Updating

`magictree update` replaces the binary that is running with the latest release built for it —
the artifact for the triple magictree was compiled for, never another platform's:

```sh
magictree update          # install the latest release
magictree update --check  # say whether one exists, and install nothing
magictree update --force  # reinstall the latest release over this one
```

The release is downloaded from GitHub and put in place only after the new binary has proved
that it runs and reports the version it should. A download that is truncated, built for
another machine, or no longer matches the digest the release publishes leaves the installed
magictree exactly as it was, and `magictree -n update` prints the artifact and the file it
would replace without downloading anything. The replacement is a rename beside the old file,
so nothing has to be restarted and a running stack keeps running.

The file replaced is the one that is running, wherever it is: `~/.local/bin`, `~/.cargo/bin`,
`/usr/local/bin`. Where that directory belongs to another user, `update` says so and names
`sudo`.

Every other command ends by naming a release that has landed since this binary was installed,
and how to install it:

```
magictree 0.1.3 is available (this is 0.1.2); run `magictree update`
```

The answer is kept for a day in `~/.local/state/magictree/update-check.json`, so at most one
command a day asks GitHub anything; a check that reached nothing is retried half an hour later
rather than on every command, and a check that fails is never an error the command reports.
The notice is the last line on stderr — stdout, the part a script reads, is untouched.

Two switches turn the notice off, for a machine that never reaches GitHub and for anything
that has no business asking: `check_for_updates = false` in `~/.config/magictree/config.toml`,
and `MAGICTREE_NO_UPDATE_CHECK` set to anything but `0` in the environment. `magictree update`
works either way.
