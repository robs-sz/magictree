# magictree

Per-worktree development stacks. Every git worktree gets its own ports, environment,
bootstrap, and supervised processes, so several branches run side by side without
colliding.

## Build

```sh
cargo build --release
install -m755 target/release/magictree ~/.local/bin/magictree
cargo test
```

`cargo test` needs no daemon. The compose tests (`tests/compose.rs`) exercise `gc`
against real containers and are skipped unless Docker, Compose and a local `alpine:3`
are present (`docker pull alpine:3`).

## Quick start

```sh
cd your-repo
magictree discover   # what the repository is, and what it cannot know
magictree init       # answer the questions, write magictree.toml
magictree up         # allocate ports, bootstrap, start, wait for health
magictree ports      # URLs for this worktree
```

`discover` only reads files and never runs anything from the repository. `init` refuses to
write a manifest while a question is unanswered, so a manifest only ever contains what was
determined.

## Commands

| | |
|---|---|
| `discover`, `init`, `doctor` | read the repository, write manifests, detect drift |
| `up`, `down`, `status` | start, stop, inspect a worktree's stack |
| `logs`, `env`, `ports` | service output, resolved environment, port assignment |
| `new`, `rm`, `list`, `gc` | worktree lifecycle and resource reclamation |

`--dry-run` works on every command and creates nothing.

## Manifest

One `magictree.toml` per app. A monorepo adds a workspace manifest at the repository root
listing its apps and shared infrastructure.

```toml
# apps/web/magictree.toml, in a repository whose root manifest lists it in [workspace]
version = 1

[app]
id = "web"                                   # defaults to the directory name

[env]                                        # shared by every service in this app
PUBLIC_URL = "http://localhost:${MAGICTREE_PORT_web_web}"

[bootstrap]
sync = ["node_modules", ".generated"]        # link from the main checkout when absent
run = [{ command = "pnpm install", inputs = ["pnpm-lock.yaml"] }]

[[services]]
id = "web"
target = { kind = "pnpm", script = "dev" }   # or mise, just, npm, uv, python, command
port = { env = "PORT", prefer = 5173 }
needs = ["api"]
health = { http = "/", timeout = 60 }

[jobs.migrate]                               # runs once per `up`, after `needs` are healthy
run = "pnpm prisma migrate deploy"
needs = ["web"]
```

### Top level

| key | required | meaning |
|---|---|---|
| `version` | yes | Manifest format; only `1` is accepted. |
| `[app] id` | no | App id, which qualifies its services in a monorepo. Defaults to the manifest directory's name. |
| `[workspace] apps` | root only | Relative directories of the apps this repository contains. |
| `[env]` | no | `KEY = "value"`, layered under the values magictree computes. |
| `[bootstrap]` | no | Setup that runs before services start. |
| `[[services]]` | no | Host processes and compose services. |
| `[jobs.<id>]` | no | One-shot commands. |

`[env]` values may interpolate magictree's computed variables — `${MAGICTREE_SLUG}`,
`${MAGICTREE_PORT_<service>}` — and may not redefine the keys magictree owns:
`MAGICTREE_*`, `COMPOSE_PROJECT_NAME`, `COMPOSE_FILE`.

The environment a service receives is layered, lowest first: computed values (slug,
worktree paths, ports), the workspace `[env]`, then the app's `[env]`. Port variables are
named `MAGICTREE_PORT_<service>`, or `MAGICTREE_PORT_<app>_<service>` for an app service,
with `_<port>` appended for a named port (`-` and `:` become `_`). A single-app repository
has no app layer, so services are bare and their ports are `MAGICTREE_PORT_<service>`.

### Bootstrap

```toml
[bootstrap]
sync = ["node_modules", ".generated"]
run = [
  "pnpm install",
  { command = "pnpm prisma generate", inputs = ["prisma/schema.prisma"] },
]
```

`sync` links the named paths from the primary checkout when a worktree is missing them.
`run` steps are shell commands; a step with `inputs` is skipped while those files are
unchanged since its last successful run.

### Services

```toml
[[services]]
id = "web"
runtime = "host"                              # default: compose when `compose` is set
target = { kind = "pnpm", script = "dev" }   # host services: target or command, not both
port = { env = "PORT", prefer = 5173 }
expose = "port"                               # or "none"
needs = ["api"]
health = { http = "/", timeout = 60 }
wait = "running"                              # or "exit" for a one-shot initialiser
```

| field | default | meaning |
|---|---|---|
| `id` | required | Service name. Unique in the manifest, and not also a job id. |
| `runtime` | `compose` when `compose` is set, otherwise `host` | How the service is launched. |
| `compose` | — | `{ file = "compose.yaml", service = "api" }`; required for a compose service. |
| `target` / `command` | — | Exactly one, host services only. |
| `port` / `ports` | — | Port declarations; see below. |
| `expose` | `"port"` | `"none"` publishes nothing. A service that declares no ports publishes none either, whatever this says; `"none"` is for clearing ports the repository's compose file declares. |
| `needs` | `[]` | Services or jobs that must be healthy first. |
| `health` | — | Probe that decides a host service is ready. |
| `wait` | `"running"` | `"exit"` waits for a one-shot container to finish successfully. |

A host service must set `command` or `target`, never both; a compose service must set
`compose` and neither of the other two.

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

```toml
port = { env = "PORT", prefer = 5173 }        # one port: the plain name
ports = [                                     # several: each one needs a name
  { name = "http", env = "HTTP_PORT", prefer = 8080 },
  { name = "grpc", env = "GRPC_PORT" },
]
```

| field | meaning |
|---|---|
| `name` | Identifies the port inside its service; defaults to the service id. Must be distinct when a service declares more than one. |
| `target` | Container-side port published on the allocated host port. A compose service that publishes anything must set it. |
| `env` | Variable receiving the allocated port: for a host process the one it reads, for a compose service the one its compose file interpolates. |
| `prefer` | Use this port when it is free, otherwise allocate one. |
| `require` | Fail loudly when this port is unavailable. |

`prefer` and `require` are mutually exclusive. Inside the compose network services reach
each other over compose DNS (`db:5432`), so a port is only worth publishing when the host
needs to reach it.

#### Health

```toml
health = { http = "/healthz", timeout = 60 }
```

`http` polls that path over the service's allocated port, `tcp = true` only checks the
port, and `command` runs a shell command that must exit zero. The first of those present
wins. `timeout` is in seconds and defaults to `health_timeout_secs` (60) from
`~/.config/magictree/config.toml`.

### Jobs

A job is a one-shot command, declared as a `[jobs.<id>]` table. Add one whenever a step
must run once around startup — a migration, a seed, a code generator — with its
prerequisites expressed as `needs` instead of wrapped into a service command.

```toml
[jobs.migrate]
run = "pnpm prisma migrate deploy"
when = "up"            # default: run on every `up` that selects this job
needs = ["db"]

[jobs.seed]
run = "pnpm prisma db seed"
when = "manual"        # never runs on its own
needs = ["migrate"]
```

| field | default | meaning |
|---|---|---|
| `run` | required | Shell command, run in the manifest's directory with the resolved environment. |
| `when` | `"up"` | `"up"` runs it during `up`; `"manual"` runs it only when named. |
| `needs` | `[]` | Services or jobs that must be healthy before it runs. |

`needs` resolves like a service's: a bare name is the service or job of the same app
first, then a workspace-level one; `app:job` always resolves globally. A named job's
`needs` are started first, so `magictree up seed` above starts `db`, runs `migrate`, then
runs `seed`.

`when = "up"` jobs run on every `up` that selects their scope — the current app in a
monorepo, or the whole repository when it is a single app — after their `needs` are
healthy. They are not remembered as done, so `run` must be idempotent. `when = "manual"`
is the escape hatch for a job that must not run unattended; run it by naming it:

```sh
magictree up                # db, then migrate
magictree up seed           # db, migrate, then seed
magictree up web:seed       # inside a monorepo, one app's job
```

Naming a manual job selects its scope too, so the `when = "up"` jobs of that app run
alongside it (in a single-app repository, every `up` job does). `magictree --dry-run up`
prints the jobs it would run and the resulting start order; a failing job fails `up` and
leaves everything running for inspection.

### Monorepo manifests

```toml
# magictree.toml at the repository root
version = 1

[workspace]
apps = ["apps/web", "apps/api"]

[[services]]                                   # shared infrastructure, used by every app
id = "db"
runtime = "compose"
compose = { file = "compose.yaml", service = "postgres" }
port = { target = 5432, env = "WT_PORT_DB" }

[jobs.shared-seed]
run = "pnpm db seed"
when = "manual"
```

Each listed directory has its own `magictree.toml` with bare service ids and its own
`[jobs.<id>]`. Services and jobs are addressed as `app:id` (`web:web`, `api:migrate`), and
workspace-level ones by their bare id (`db`).

## Layout

| | |
|---|---|
| `magictree.toml` | committed per app; a root one with `[workspace]` for monorepos |
| `~/.local/state/magictree/blocks/` | machine-wide port assignments |
| `~/.local/state/magictree/worktrees/<repo>/<worktree>/` | generated per worktree: `env`, `ports.json`, `run/`, `log/` |
| `~/.config/magictree/config.toml` | optional: port range, timeouts |

Ports come from `20000-32767`, are stable across restarts, and are never user-visible
inside containers — compose services reach each other over compose DNS.

Generated state lives in magictree's own state dir, keyed by repository and worktree, so
**nothing is ever written into the checkout** and `git status` stays clean without any
`.gitignore` or `git/info/exclude` entry. Keeping it out of the checkout is also what lets
`gc` stop a worktree's host processes after the checkout is gone; `up` adopts state left
in a `<worktree>/.magictree/` by an older build.

`gc` normally takes a repository to reconcile against. `magictree gc --all` sweeps every
repository the state dir holds a record of instead, which is the only way to reclaim a
repository that is itself gone: the records are all that is left of it.

magictree never writes to repository `.env` files.

## Agents

`skills/magictree/SKILL.md` teaches a coding agent to operate the CLI.
