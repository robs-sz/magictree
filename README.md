<div align="center" style="background:transparent"><samp>&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;/33&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;<br>
&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;|__/&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;<br>
&nbsp;/333333/3333&nbsp;&nbsp;&nbsp;/333333&nbsp;&nbsp;&nbsp;/333333&nbsp;&nbsp;/33&nbsp;&nbsp;/3333333<br>
|&nbsp;33_&nbsp;&nbsp;33_&nbsp;&nbsp;33&nbsp;|____&nbsp;&nbsp;33&nbsp;/33__&nbsp;&nbsp;33|&nbsp;33&nbsp;/33_____/<br>
|&nbsp;33&nbsp;\&nbsp;33&nbsp;\&nbsp;33&nbsp;&nbsp;/3333333|&nbsp;33&nbsp;&nbsp;\&nbsp;33|&nbsp;33|&nbsp;33&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;<br>
|&nbsp;33&nbsp;|&nbsp;33&nbsp;|&nbsp;33&nbsp;/33__&nbsp;&nbsp;33|&nbsp;33&nbsp;&nbsp;|&nbsp;33|&nbsp;33|&nbsp;33&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;<br>
|&nbsp;33&nbsp;|&nbsp;33&nbsp;|&nbsp;33|&nbsp;&nbsp;3333333|&nbsp;&nbsp;3333333|&nbsp;33|&nbsp;&nbsp;3333333<br>
|__/&nbsp;|__/&nbsp;|__/&nbsp;\_______/&nbsp;\____&nbsp;&nbsp;33|__/&nbsp;\_______/<br>
&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;/33&nbsp;&nbsp;\&nbsp;33&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;<br>
&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;|&nbsp;&nbsp;333333/&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;<br>
&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;\______/&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;</samp></div>

# magictree

Per-worktree development stacks. Every git worktree gets its own ports, environment,
bootstrap, and supervised processes, so several branches run side by side without colliding.

A worktree is the unit: the primary checkout and every linked worktree run a stack of their
own, with their own ports and their own state.

## Worktrees

Onboard once, in the primary checkout, then commit the manifest:

```sh
magictree discover && magictree init
git add magictree.toml           # in a monorepo, each app's manifest as well
git commit -m "add magictree.toml"
```

A worktree inherits only what git tracks, so an uncommitted `magictree.toml` is invisible to
every worktree created afterwards: `up` there fails with the fix, rather than starting a
stack that silently does not exist. Run `discover` and `init` in the primary checkout (the
one `git worktree list` prints first, and the one `magictree list` never shows).

Onboarding does not end at the first run. Run `discover` and `init` again after the
repository gains something (a Storybook, a new compose service) and `init` asks only about
what it has not been told before: the answers it was given are recorded in the manifest at
the repository root, and replayed on the next run. A question answered `skip` stays skipped
even after the repository gains what it declined.

```toml
# Recorded by `magictree init`; replayed so only new questions are asked.
answers = { "web.run" = "pnpm:dev", "web.storybook" = "skip", "compose.shared" = ["db"] }
declined = { "compose.shared" = ["mail"] }
```

A question that offers several things is answered one option at a time, so what the answer
turned down is recorded beside it: that is what shows a later run that a service someone
added to the compose file has never been decided rather than turned down. `init` says which
services and options are new and leaves them undecided (it never answers a question in your
name), and the next interactive run asks about them, marking the new options. `--reanswer`
asks every question again, taking discovery's defaults.

Otherwise `init` appends the service blocks the manifest is missing and leaves every other
line, comment and value as it was. A step the manifest already runs is not added a second
time, whatever the service is called there, and `init` says which services it added, which
answers it recorded and which manifests it left alone. An answer that changed cannot reach a
service already written (an update never rewrites one), so `init` says so and points at
`--force`, which regenerates the file from the recorded answers.

```sh
magictree new feat/billing          # git worktree add beside the primary checkout, then up
magictree new feat/billing --no-up  # create the checkout only

git worktree add ../repo-billing -b feat/billing    # plain git works too
cd ../repo-billing && magictree up
```

Nothing is configured per worktree: each gets its own port block, its own `MAGICTREE_SLUG`
(the directory name; `main` in the primary checkout), and its own state, so the same service
binds a different port in every checkout. Untracked build output is not inherited either;
`[bootstrap] sync` links it from the primary checkout.

Ports use `localhost` by default. Add `browser_alias = true` to a port declaration to print
and use a second URL at `http://<MAGICTREE_SLUG>.localhost:<port>`. Magictree rewrites HTTP(S)
and WebSocket URLs that target that opted-in port in manifest environment values, and in the
environment of the Compose services a browser can reach — the ones that publish a port; a
service that publishes none keeps `localhost` in its environment, because the processes that
read it (a worker, a provisioning container that writes the value into a file) resolve
hostnames rather than a browser's own special case. URLs for unmarked ports stay unchanged
everywhere. The alias reaches the same loopback listener, so no proxy or hosts-file entry is
needed. Applications must accept the alternate `Host`, and auth providers must allow rewritten
callback URLs. Bare hostname settings without a port (for example, an external-domain setting)
are not rewritten.


```sh
magictree list               # linked worktrees with their ports; not the primary checkout
magictree list --all         # every recorded worktree, in every repository, live or gone
magictree ports              # this worktree's assignment
magictree ports --release    # drop it: nothing stays reserved and no stack is touched
magictree up --ports generated   # this checkout's stack, on its own block ports
magictree down               # stop; volumes and ports are kept
magictree restart web        # stop one service and start it again, on its own ports
magictree rm feat/billing    # stop and remove the worktree; branch kept, ports released
magictree gc                 # reclaim what deleted checkouts left behind
```

In the primary checkout, `up` starts the stack on the ports the manifest declares (`prefer`),
which is where the repository's own commands and generated files expect to find them; a linked
worktree always allocates its own block. `up --ports generated` asks the primary checkout for
block ports instead, so a second stack can run beside the one on the declared ports, and
`ports --release` drops an assignment so its ports stop being reserved; the next command that
needs them (`up`, `ports`, `env`) allocates again.

`list` answers for one checkout, from `git worktree list`; `list --all` answers for the whole
machine, from the state dir, and needs no checkout at all. It groups the records by repository
— named by its primary checkout, since the state dir stores it under a hash — and marks each
one `live`, `gone` (the checkout no longer exists, which is what `gc --all` reclaims), or
`primary` for the checkout a repository's own stack runs in.

`rm` accepts a path, a branch name, a directory name, or an id from either listing — a row
marked `primary` is the repository's own checkout and is refused — refuses a dirty worktree
unless `--force`, and never deletes a branch. An id is resolved in the current repository
first, then in the state dir's records, so a worktree of any repository magictree knows can be
removed from anywhere; an id two repositories share is refused, and the checkouts are named
instead. A removal is the end of the worktree, not only of its checkout: the branch is kept,
while its port block is released and its generated state directory is dropped, so nothing is
left for a later `gc`.

`up` and `restart` build the compose services they start, so a changed Dockerfile or build
context is picked up by running `magictree up` again. There is no cheap way to ask Docker
whether an image is stale (only the builder knows, and it knows by validating its cache), so
the build *is* the check: an unchanged context is a cache hit, not a rebuild.

`build` in `~/.config/magictree/config.toml` decides whether that happens: `"always"` (the
default), `"ask"` — one question per run, naming the services that would build, declined when
there is no terminal — or `"never"`, which is how a machine says "build only when I ask". In
that case `magictree up --build` builds for one run. A service that only names an `image:` is
never built and never asked about.

## Commands

| | |
|---|---|
| `discover`, `init`, `doctor` | read the repository, write manifests, detect drift |
| `up`, `down`, `restart`, `status` | start, stop, restart specific services, inspect a worktree's stack |
| `logs`, `env`, `ports` | service output (one service, or `--all` for the whole stack, Compose included), resolved environment, port assignment |
| `exec` | run a command with this worktree's resolved environment |
| `new`, `rm`, `list`, `gc` | worktree lifecycle and resource reclamation |
| `completion` | a shell completion script, printed to stdout |
| `update` | install the latest release over this binary |

`--dry-run` works on every command and creates nothing: `magictree -n up`.

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

## Manifest

One `magictree.toml` per app; a monorepo adds a workspace manifest at the repository root. Two
worked manifests live in the repository:
[`examples/single-app`](examples/single-app/README.md) and
[`examples/monorepo`](examples/monorepo/README.md).

### Technologies

`discover` reads files and never runs anything from the repository; `init` writes the manifest
from what it found and refuses while a question is unanswered. What it reads, and what it
writes from it:

| technology | read from | becomes |
|---|---|---|
| Docker Compose | `compose.{yaml,yml}`, `docker-compose.{yaml,yml}` at the root or under `infra/`, `deployment/`, `docker/`, one level deep | a compose service, with its published ports and healthcheck |
| npm, pnpm | `package.json` scripts and lockfile | `target = { kind = "npm" \| "pnpm", script = "..." }` |
| yarn, bun | the same files | a plain `command = "yarn run dev"`; both are detected, but neither is a first-class target |
| pnpm / npm / yarn workspaces, Lerna | `pnpm-workspace.yaml`, `package.json` `workspaces`, `lerna.json` | `[workspace] apps` |
| Python | `pyproject.toml`, `uv.lock`, `poetry.lock` | `target = { kind = "uv", script = "..." }`, and the install step |
| Storybook | a `package.json` script that runs `storybook dev`, `.storybook/`, an `@storybook/*` dependency | a second `storybook` service, beside the app's own |
| just | `justfile` recipes, and the variables they read | `target = { kind = "just", recipe = "..." }`, a port variable, or an inlined `command` |
| mise | `mise.toml` tasks, tools, env, profiles | `target = { kind = "mise", task = "..." }` |
| Procfile | process types | a run candidate whose command still has to be filled in |
| env templates | `.env.example`, `.env.sample`, `.env.template` | port variable candidates |
| port literals | `-p 3005`, `PORT=3005`, `localhost:5173` in the files above | `doctor` drift, with the rewrite that follows the allocation |
| app directories | `package.json`, `pyproject.toml`, `go.mod`, `Cargo.toml`, `justfile` in workspace globs | an app; Go and Rust yield no run candidate, so `init` asks |

The install step and its `inputs` cache key come from whichever lockfile is present:
`pnpm-lock.yaml`, `yarn.lock`, `bun.lock{b}`, `package-lock.json`, `uv.lock`, `poetry.lock`,
else `package.json` or `pyproject.toml`.

Storybook is the one service whose flags the manifest supplies. Its dev server takes `-p`/`--port`
and reads no environment variable of its own, so `init` appends the allocated port (and
`--no-open`, which keeps `up` from opening a browser) to the repository's own script:

```toml
[[services]]
id = "storybook"
target = { kind = "pnpm", script = "storybook", args = ["-p", "${STORYBOOK_PORT:-6006}", "--no-open"] }
port = { env = "STORYBOOK_PORT", prefer = 6006 }
health = { http = "/", timeout = 120 }
```

The appended flag lands after the script's own, so it beats whatever the script pins and every
worktree gets its own URL; that is why `doctor` stays quiet about a `-p` a storybook script
pins. `prefer = 6006` keeps Storybook on its familiar port in the primary checkout; a linked
worktree allocates its own. When `@storybook/addon-mcp` is installed, that URL also answers
MCP at `/mcp`, which is how an agent reads and drives the components.

### Top level

| key | required | meaning |
|---|---|---|
| `version` | yes | Manifest format; only `1` is accepted. |
| `[app] id` | no | App id, which qualifies its services in a monorepo. Defaults to the directory name. |
| `answers` | root only | What `init` was told, replayed so a later run asks only about what is new. Written by `init`; the runtime never reads it. |
| `declined` | root only | The options those answers turned down, so a new one is asked about instead of assumed. Written by `init`; the runtime never reads it. |
| `[workspace] apps` | root only | Relative directories of the apps this repository contains. |
| `[env]` | no | `KEY = "value"`, layered under the values magictree computes. |
| `[bootstrap]` | no | Steps that run around `up`: `sync` and `run` before services start, `after` once they are up. |
| `[[services]]` | no | Host processes and compose services. |
| `[jobs.<id>]` | no | One-shot commands. |

`[env]` values may interpolate `${MAGICTREE_SLUG}` and `${MAGICTREE_PORT_<...>}`, but not
redefine the keys magictree owns: `MAGICTREE_*`, `COMPOSE_PROJECT_NAME`, `COMPOSE_FILE`,
and every variable a service names in `port.env`.
Layers, lowest first: computed values (slug, worktree paths, ports), workspace `[env]`, app
`[env]`. A port variable is `MAGICTREE_PORT_<service>`, prefixed with `<app>_` for an app
service and suffixed with `_<port>` when a service has several (`-` and `:` become `_`).
Every declared `port.env` is a computed value too: the whole stack's set reaches every
launched process and `magictree env`, `--export` and `--explain`, because compose
interpolates a variable wherever it is declared and host tooling reads it wherever it runs.
A variable two services declare resolves last-wins in manifest order; name them distinctly.

### Services

A service is either a host process (`target` or `command`) or a container (`compose`).

| field | default | meaning |
|---|---|---|
| `id` | required | Service name. Unique in the manifest, and not also a job id. |
| `runtime` | `compose` when `compose` is set, otherwise `host` | How the service is launched. |
| `compose` | - | `{ file = "compose.yaml", service = "api" }`; required for a compose service. |
| `target` / `command` | - | Exactly one, host services only. |
| `port` / `ports` | - | Port declarations; see below. |
| `expose` | `"port"` | `"none"` publishes nothing, which is also what a service with no ports does; `"none"` clears ports the compose file declares. |
| `needs` | `[]` | Services or jobs that must be healthy first. |
| `health` | - | Probe that decides a host service is ready. |
| `wait` | `"running"` | `"exit"` waits for a one-shot container to finish successfully. |

A host service sets `command` or `target`, never both; a compose service sets `compose` and
neither of the other two.

#### Targets

| `kind` | fields | command |
|---|---|---|
| `command` | `command` | the string, as written |
| `mise` | `task`, `profile` | `mise [--profile <p>] run <task>` |
| `just` | `recipe`, `args` | `just <recipe> <args...>` |
| `npm`, `pnpm` | `script`, `args` | `npm run <script> <args...>` |
| `uv` | `script` or `module`, `group`, `args` | `uv run [--group <g>] <script>` or `uv run python -m <module>` |
| `python` | `module`, `args` | `python3 -m <module> <args...>` |

#### Ports

One port is `port = { env = "PORT", prefer = 5173 }`. Several are
`ports = [ { name = "http", env = "API_PORT" }, { name = "metrics", env = "API_METRICS_PORT" } ]`,
where each needs its own `name`.

| field | meaning |
|---|---|
| `name` | Identifies the port inside its service; defaults to the service id. Must be distinct when a service declares more than one. |
| `target` | Container-side port published on the allocated host port. A compose service that publishes anything must set it. |
| `env` | Variable receiving the allocated port: for a host process the one it reads, for a compose service the one its compose file interpolates. Published for the whole stack, so any process may read it. Reserved magictree names are rejected. |
| `prefer` | The port this service runs on in the primary checkout. Taken as declared there; a linked worktree ignores it and allocates from its own block. |
| `require` | Fail loudly when this port is unavailable, in any worktree. |
| `browser_alias` | Rewrite URLs that target this assigned port to the worktree's `<slug>.localhost` host; show the alias in `up`, `status`, and `ports`. A Compose service's environment is rewritten only when the service publishes a port. Disabled by default. |

A declared port is the checkout's own: the repository's tooling, the `.env` files it generates
and anything registered against a callback URL were written against that number, so the primary
checkout takes it or says why it cannot — it is never traded for a free one. That is what makes
a stack magictree starts in the primary checkout land where the repository's own commands
already look. `magictree up --ports generated` sets that aside and allocates every port from the
checkout's block, which is how a second stack runs beside the first without touching the
declared ports; `--ports declared` takes them back, and neither can happen under a running
stack.

A linked worktree never takes a declared port: every port comes from its own block, so two
worktrees run at once and a worktree started first cannot take the port the primary checkout
expects. `prefer` and `require` are mutually exclusive. A port an assignment has recorded stays
reserved machine-wide even while that worktree is stopped, so a port stays stable instead of
being taken by a worktree that starts later and then answers the owner's health probe;
`ports --release` drops an assignment and its reservations, `ports --reassign` re-allocates in
the mode the worktree recorded, and `gc` reclaims what deleted checkouts left behind. `require`
treats a port held by another worktree as unavailable.

Inside the compose network services reach each other over compose DNS (`db:5432`), so publish
a port only when the host needs to reach it.

#### Health

`health = { http = "/healthz", timeout = 60 }`: `http` polls that path on the allocated port,
`tcp = true` only checks the port, and `command` must exit zero; the first present wins.
`timeout` is in seconds, defaulting to `health_timeout_secs` (60) from
`~/.config/magictree/config.toml`.

### Bootstrap

`sync = ["node_modules", ".generated"]` links those paths from the primary checkout when a
worktree is missing them. `run` is a list of shell commands: a bare string, or
`{ command = "pnpm prisma generate", inputs = ["prisma/schema.prisma"] }`, whose `inputs` skip
the step while those files are unchanged since its last successful run.

`after` takes the same list, and runs it once every service the `up` selected is healthy and
every `up` job has finished: a seed, a smoke test, a browser, a webhook registration, anything
that needs the stack it just started. Steps run in the declaring manifest's directory with the
resolved ports and `[env]`, and `inputs` skips a step the same way. A step without `inputs` runs
on every `up`, so its command must be idempotent. Since the URLs are printed before `after`
runs, a step that fails still leaves the running stack and its addresses on screen. A
`when = "up"` job is the wrong tool for this: it runs as soon as its own `needs` are healthy,
which is early in a partial `up`.

A step marked `ask = true` asks before it runs: `up` prints `Run task: <command> (y/N)` and
runs the step only on a yes. A decline — and a run without a terminal, such as a script, an
agent, or CI — skips the step and caches nothing, so the next `up` asks again. A cached step
(`inputs` unchanged) is never asked about.

```toml
[bootstrap]
after = [
  "pnpm db:seed",
  { command = "pnpm test:smoke", ask = true },   # opt in per run: Run task: pnpm test:smoke (y/N)
]
```

Both `sync` paths and `inputs` are relative to the manifest that declares them, matching the
directory its commands run in: an app manifest's `sync = ["node_modules"]` links the app's own
`node_modules`, and its `inputs = ["uv.lock"]` means the app's `uv.lock`. A root manifest's
paths are relative to the repository root.

### Jobs

A job is a one-shot command, declared as a `[jobs.<id>]` table: a migration, a seed, a code
generator, with its prerequisites in `needs` rather than wrapped into a service command.
[`examples/single-app`](examples/single-app/README.md) declares both kinds.

| field | default | meaning |
|---|---|---|
| `run` | required | Shell command, run in the manifest's directory with the resolved environment. |
| `when` | `"up"` | `"up"` runs it during `up`; `"manual"` runs it only when named. |
| `needs` | `[]` | Services or jobs that must be healthy before it runs. |

`needs` resolves like a service's (its own app first, then the workspace) and is started
before the job, so `magictree up seed` starts `db`, runs `migrate`, then runs `seed`.

`when = "up"` jobs run on every `up` that selects their scope (the current app in a monorepo,
everything in a single app) after their `needs` are healthy, and are not remembered as done,
so `run` must be idempotent. `when = "manual"` never runs unattended: name it to run it, which
selects its scope too, so that app's `up` jobs run alongside it.

```sh
magictree up                # db, then migrate
magictree up seed           # db, migrate, then seed
magictree --dry-run up      # the jobs and the start order, without starting anything
```

### Monorepo manifests

The root manifest holds `[workspace] apps`, the services every app shares (`db`, `cache`), and
workspace-level jobs; each directory it lists has its own `magictree.toml` with bare service
ids. Services and jobs are addressed as `app:id` (`web:web`, `api:migrate`), and
workspace-level ones by their bare id (`db`); a `needs` entry resolves inside its own app
first, then at the workspace. [`examples/monorepo`](examples/monorepo/README.md) is a complete
one.

## Configuration

Optional machine-wide settings at `~/.config/magictree/config.toml`. Every key has a default,
so the file itself is optional.

| key | default | meaning |
|---|---|---|
| `port_range_start` | `20000` | Inclusive start of the range worktree blocks are allocated from. |
| `port_range_end` | `32767` | Inclusive end of that range. |
| `port_stride` | `20` | Ports reserved per worktree block. |
| `stop_timeout_secs` | `10` | Grace period after SIGTERM before a host process is killed. |
| `health_timeout_secs` | `60` | Default health-probe timeout, in seconds. |
| `build` | `"always"` | Whether `up` and `restart` build the compose services they start: `"always"`, `"ask"`, or `"never"`. `--build` and `--no-build` override it for one run. |
| `reconcile` | `"always"` | Whether `up` reconciles an already-up Compose project every time (`"always"`, the default and today's behaviour), or leaves it alone on an unchanged configuration (`"auto"`). `up --refresh` forces a reconcile for one run. |
| `sync` | `true` | Whether `[bootstrap] sync` may link a path from the primary checkout; `false` makes every checkout install its own dependencies. |
| `check_for_updates` | `true` | Whether a command may end by naming a release that has landed since this binary was installed. |

## Layout

| | |
|---|---|
| `magictree.toml` | committed per app; a root one with `[workspace]` for monorepos |
| `~/.local/state/magictree/blocks/` | machine-wide port assignments |
| `~/.local/state/magictree/update-check.json` | what the last release check found, and when it ran |
| `~/.local/state/magictree/worktrees/<repo>/<worktree>/` | generated per worktree: `env`, `ports.json`, `run/`, `log/` |
| `~/.config/magictree/config.toml` | optional: port range, timeouts, build mode, update check |

Ports come from `20000-32767`, are stable across restarts, and are never visible inside
containers. A port the primary checkout declares with `prefer` is wherever the manifest says
instead, and is not carved out of a block. Generated state lives in magictree's state dir,
keyed by repository and worktree, so **nothing is ever written into the checkout**
(`git status` stays clean without a `.gitignore` or `git/info/exclude` entry) and `gc` can
stop a worktree's processes after its checkout is gone. `gc` reconciles one repository;
`magictree gc --all` sweeps every repository the state dir knows about, the only way to
reclaim one that is itself gone, and `magictree list --all` reports those same records —
repository by repository, each marked live, gone, or the primary checkout — from anywhere.
`rm` needs none of that bookkeeping: it accepts a recorded id and drops the worktree's port
block and state itself.

magictree never writes to repository `.env` files.

## Agents

`skills/magictree/SKILL.md` teaches a coding agent to operate the CLI. Onboarding belongs to
the primary checkout: `init` writes the manifest where it runs, so a manifest written inside a
linked worktree is untracked and no other worktree inherits the stack.

## Development

`cargo fmt --check` runs in the pre-commit hook, so a commit the formatter would rewrite never
lands. Git does not copy hooks from a clone, so point it at the tracked ones once:

```sh
git config core.hooksPath .githooks
```
