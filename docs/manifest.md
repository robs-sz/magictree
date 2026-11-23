# Manifest

One `magictree.toml` per app; a monorepo adds a workspace manifest at the repository root. Two
worked manifests live in the repository:
[`examples/single-app`](../examples/single-app/README.md) and
[`examples/monorepo`](../examples/monorepo/README.md).

## Technologies

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

## Top level

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

## Services

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

### Targets

| `kind` | fields | command |
|---|---|---|
| `command` | `command` | the string, as written |
| `mise` | `task`, `profile` | `mise [--profile <p>] run <task>` |
| `just` | `recipe`, `args` | `just <recipe> <args...>` |
| `npm`, `pnpm` | `script`, `args` | `npm run <script> <args...>` |
| `uv` | `script` or `module`, `group`, `args` | `uv run [--group <g>] <script>` or `uv run python -m <module>` |
| `python` | `module`, `args` | `python3 -m <module> <args...>` |

### Ports

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
| `browser_alias` | Rewrite URLs that target this assigned port to the worktree's `<slug>.localhost` host; list the port under the alias, in place of its `localhost` URL, in `up`, `status`, and `ports`. A Compose service's environment is rewritten only when the service publishes a port. Disabled by default. |

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

### Health

`health = { http = "/healthz", timeout = 60 }`: `http` polls that path on the allocated port,
`tcp = true` only checks the port, and `command` must exit zero; the first present wins.
`timeout` is in seconds, defaulting to `health_timeout_secs` (60) from
`~/.config/magictree/config.toml`.

## Bootstrap

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

## Jobs

A job is a one-shot command, declared as a `[jobs.<id>]` table: a migration, a seed, a code
generator, with its prerequisites in `needs` rather than wrapped into a service command.
[`examples/single-app`](../examples/single-app/README.md) declares both kinds.

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

## Monorepo manifests

The root manifest holds `[workspace] apps`, the services every app shares (`db`, `cache`), and
workspace-level jobs; each directory it lists has its own `magictree.toml` with bare service
ids. Services and jobs are addressed as `app:id` (`web:web`, `api:migrate`), and
workspace-level ones by their bare id (`db`); a `needs` entry resolves inside its own app
first, then at the workspace. [`examples/monorepo`](../examples/monorepo/README.md) is a complete
one.
