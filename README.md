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
answers = { "web.run" = "pnpm:dev", "web.storybook" = "skip", "compose.shared" = ["postgres"] }
declined = { "compose.shared" = ["mailpit"] }
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

```sh
magictree list               # linked worktrees with their ports; not the primary checkout
magictree ports              # this worktree's assignment
magictree down               # stop; volumes and ports are kept
magictree rm feat/billing    # stop and remove the worktree; the branch is kept
magictree gc                 # reclaim what deleted checkouts left behind
```

`rm` accepts a path, a branch name, or a directory name, refuses a dirty worktree unless
`--force`, and never deletes a branch.

## Commands

| | |
|---|---|
| `discover`, `init`, `doctor` | read the repository, write manifests, detect drift |
| `up`, `down`, `status` | start, stop, inspect a worktree's stack |
| `logs`, `env`, `ports` | service output, resolved environment, port assignment |
| `new`, `rm`, `list`, `gc` | worktree lifecycle and resource reclamation |

`--dry-run` works on every command and creates nothing.

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
pins. `prefer = 6006` keeps Storybook on its familiar port in the first worktree. When
`@storybook/addon-mcp` is installed, that URL also answers MCP at `/mcp`, which is how an agent
reads and drives the components.

### Top level

| key | required | meaning |
|---|---|---|
| `version` | yes | Manifest format; only `1` is accepted. |
| `[app] id` | no | App id, which qualifies its services in a monorepo. Defaults to the directory name. |
| `answers` | root only | What `init` was told, replayed so a later run asks only about what is new. Written by `init`; the runtime never reads it. |
| `declined` | root only | The options those answers turned down, so a new one is asked about instead of assumed. Written by `init`; the runtime never reads it. |
| `[workspace] apps` | root only | Relative directories of the apps this repository contains. |
| `[env]` | no | `KEY = "value"`, layered under the values magictree computes. |
| `[bootstrap]` | no | Setup that runs before services start. |
| `[[services]]` | no | Host processes and compose services. |
| `[jobs.<id>]` | no | One-shot commands. |

`[env]` values may interpolate `${MAGICTREE_SLUG}` and `${MAGICTREE_PORT_<...>}`, but not
redefine the keys magictree owns: `MAGICTREE_*`, `COMPOSE_PROJECT_NAME`, `COMPOSE_FILE`.
Layers, lowest first: computed values (slug, worktree paths, ports), workspace `[env]`, app
`[env]`. A port variable is `MAGICTREE_PORT_<service>`, prefixed with `<app>_` for an app
service and suffixed with `_<port>` when a service has several (`-` and `:` become `_`).

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
| `env` | Variable receiving the allocated port: for a host process the one it reads, for a compose service the one its compose file interpolates. |
| `prefer` | Use this port when it is free, otherwise allocate one. |
| `require` | Fail loudly when this port is unavailable. |

`prefer` and `require` are mutually exclusive. A port a worktree has recorded is reserved
machine-wide even while that worktree is stopped, so a `prefer` port stays stable instead of
being taken by a worktree that starts later and then answers the owner's health probe;
`ports --reassign` or `gc` releases it. `require` treats a port held by another worktree as
unavailable.

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

The root manifest holds `[workspace] apps`, the services every app shares (`db`, `redis`), and
workspace-level jobs; each directory it lists has its own `magictree.toml` with bare service
ids. Services and jobs are addressed as `app:id` (`web:web`, `api:migrate`), and
workspace-level ones by their bare id (`db`); a `needs` entry resolves inside its own app
first, then at the workspace. [`examples/monorepo`](examples/monorepo/README.md) is a complete
one.

## Layout

| | |
|---|---|
| `magictree.toml` | committed per app; a root one with `[workspace]` for monorepos |
| `~/.local/state/magictree/blocks/` | machine-wide port assignments |
| `~/.local/state/magictree/worktrees/<repo>/<worktree>/` | generated per worktree: `env`, `ports.json`, `run/`, `log/` |
| `~/.config/magictree/config.toml` | optional: port range, timeouts |

Ports come from `20000-32767`, are stable across restarts, and are never visible inside
containers. Generated state lives in magictree's state dir, keyed by repository and worktree,
so **nothing is ever written into the checkout** (`git status` stays clean without a
`.gitignore` or `git/info/exclude` entry) and `gc` can stop a worktree's processes after its
checkout is gone. `gc` reconciles one repository; `magictree gc --all` sweeps every repository
the state dir knows about, the only way to reclaim one that is itself gone.

magictree never writes to repository `.env` files.

## Agents

`skills/magictree/SKILL.md` teaches a coding agent to operate the CLI. Onboarding belongs to
the primary checkout: `init` writes the manifest where it runs, so a manifest written inside a
linked worktree is untracked and no other worktree inherits the stack.
