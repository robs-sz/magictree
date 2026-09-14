---
name: magictree
description: "Run per-worktree development stacks with magictree. Use when a repository has a magictree.toml, when the user asks to start/stop a stack, run services, find a service's port or URL, work on several branches at once, or create/remove git worktrees. Also use when asked to write or change magictree.toml, or when a stack fails to start."
---

# magictree

magictree gives every git worktree its own development stack: allocated ports, generated
environment, bootstrap, and supervised processes. Worktrees of the same repository run
side by side without colliding.

The repository's `magictree.toml` is the source of truth. Read it before acting.

## Before anything else

```bash
magictree --help
magictree up --help
```

The installed binary is the authority on flags. Do not invent commands.

Check whether this repository is onboarded:

```bash
ls magictree.toml
```

No manifest means magictree cannot start a stack. Onboard the repository first —
see "Onboarding a repository" below.

## Onboarding a repository

A repository needs a `magictree.toml` before any stack can start. If there is
none, discover what the repository is, then write one.

```bash
magictree discover                         # facts and open questions, human summary
magictree discover --report discovery.json # machine-readable report
magictree discover --default-answers       # the answer set the report implies

magictree init                             # interactive: asks each question
magictree init --accept-defaults           # non-interactive: take every default
magictree init --answers answers.json      # replay a reviewed answer set
magictree init --print                     # show the manifests, write nothing
magictree init --save-answers answers.json # record what was answered

magictree doctor                           # compare manifests against the repository
```

`discover` only reads files. It never runs anything from the repository, and it
never guesses: whatever it cannot determine becomes an explicit question.
`init` refuses to write anything while a question is unanswered, and it refuses
answers that were computed from a different report — so re-run `discover` after
changing the repository, rather than editing an old answer file.

**Do not hand-write a manifest when the repository can be discovered.** Run
`discover`, answer the questions, and let `init` write it. If you do write one by
hand, follow the shape below and run `doctor` afterwards.

A service usually reads its port from a variable the repository already uses, and
that variable is often not `PORT`. Discovery lists the variables it found, and
`init` writes the chosen one as `port = { env = "..." }`. Getting this wrong means
the service listens somewhere the health check is not looking, so pick the
variable the app actually reads rather than assuming `PORT`.

`doctor` distinguishes two kinds of finding: `drift` means something the manifest
relies on no longer exists (the command fails), and `info` means something exists
that no manifest manages (the command still succeeds).

## Core loop

```bash
magictree up            # allocate ports, bootstrap, start services, wait for health
magictree status        # what is running, on which port
magictree ports         # the port assignment as URLs
magictree logs web -f   # follow one service's output
magictree down          # stop everything, keep volumes
```

`up` finishes by printing every reachable URL, plus any service that is internal
to the stack (`expose = "none"`), so the addresses to use are always in the
output. It is idempotent: run it any time. It is safe to re-run after editing `magictree.toml`.
It exits non-zero and explains itself when a service fails to become healthy — on failure
everything stays running so it can be inspected.

## Prefer --dry-run before acting

`--dry-run` is a global flag and works on `up`, `down`, `ports`, and `env`. It prints what
would happen and **creates nothing**: no port block claimed, no files written, no processes
started.

```bash
magictree --dry-run up
magictree --dry-run up --app web
```

Use it when you are unsure what a change does, before removing a worktree, or when asked
to explain a stack without starting it.

## Worktrees

```bash
magictree new feat/billing          # create the worktree and start its stack
magictree new feat/billing --no-up  # create only
magictree list                      # worktrees with their ports
magictree rm feat/billing           # stop and remove; the branch is kept
magictree rm feat/billing --force   # discard uncommitted changes
magictree gc                        # reclaim ports and compose resources of deleted worktrees
magictree gc --dry-run              # report what gc would reclaim
magictree gc --prune                # also drop git's record of deleted checkouts
magictree gc --all                  # every repository the state dir knows, no checkout needed
```

`new` places the checkout beside the primary checkout as `<repo>-<slug>`. `rm` accepts a
path, a branch name, or a directory name. It refuses a dirty worktree unless `--force`.
Never delete a branch as part of cleanup; `rm` never does.

`list` shows only the repository's linked worktrees, so every row it prints is a valid
`rm` target. The primary checkout is not a linked worktree and never appears; use `ports`
or `status` in it for the primary stack's ports.

## Selecting services

In a single-app repository, service names are bare (`web`, `api`).

In a monorepo, app services are qualified as `app:service` (`web:web`, `api:api`), and
repository-level services such as shared `db` or `redis` are bare.

```bash
magictree up web:web           # one service plus its dependencies
magictree up --app web         # every service of one app plus its dependencies
magictree up --all             # everything in the repository
magictree up                   # current app; at the repository root, everything
```

Dependencies always come along, and selecting anything inside an app also brings
the repository's shared infrastructure, so an app is never started without its
database or cache. `up --app web` and `up web:web` therefore both start the shared
services plus `web:web`.

## Reading the environment

```bash
magictree env                  # KEY=value for the current app
magictree env --app web        # a specific app
magictree env --export         # export KEY=value, safe to eval
magictree env --explain        # which layer set each value
```

Values come from three layers, lowest first: repository `[env]`, app `[env]`, then values
magictree computes (slugs, ports). Ports appear as `MAGICTREE_PORT_<name>`, and each host
service additionally receives the variable its manifest names in `port.env`.

Never edit or overwrite repository `.env` files. magictree injects the environment into the
processes it launches; the repository's own files are left alone.

## Writing magictree.toml

Read an existing manifest first and follow its style. The shape:

```toml
version = 1

[bootstrap]
sync = ["node_modules", ".generated"]        # link from the main checkout when absent
run = [
  { command = "pnpm install", inputs = ["pnpm-lock.yaml"] },
]

[[services]]
id = "web"
target = { kind = "pnpm", script = "dev" }   # or mise / just / npm / uv / python / command
port = { env = "PORT", prefer = 5173 }
needs = ["api"]
health = { http = "/", timeout = 60 }
```

Rules that matter:

- `runtime` defaults to `compose` when `compose` is present, otherwise a host process.
- `port.env` must be the variable the process actually reads. Check the app's own
  configuration before assuming `PORT`.
- A command that pins a port — `next dev -p 3005`, `uvicorn --port 8000`,
  `DATABASE_PORT=5433` — ignores `port.env`, so the second worktree tries to bind the
  same number. `magictree discover` lists these literals, `init` warns about the step it
  chose, and `doctor` reports the service's own command as drift with the rewrite
  (`next dev -p ${APP_PORT:-3005}`). Never paper over one by hardcoding the port in the
  manifest instead.
- Storybook is the exception, because it reads no port variable at all: `init` starts it with
  `args = ["-p", "${STORYBOOK_PORT:-6006}", "--no-open"]`, the appended flag beating whatever
  the script pins. Keep that shape on a `storybook` service written by hand.
- Host services need either `command` or `target`. Compose services need neither.
- A compose service must give `port.target` (the container-side port) for every port it
  exposes. `expose = "none"` publishes nothing.
- Several ports on one service use `ports = [ { name = "api", target = 9000 }, ... ]`.
  A single `port = {...}` keeps the plain name; multiple ports are named.
- Bootstrap steps with `inputs` are skipped when those files are unchanged.
- `[jobs.<id>]` with `when = "up"` runs once after its `needs` are healthy; `when = "manual"`
  never runs automatically.
- A one-shot compose initialiser (provisioning, migrations) needs `wait = "exit"`, so
  `up` waits for it to finish successfully instead of moving on while it runs.
- Services that derive their own URLs from a port — `${WT_PORT_ZITADEL}` in another
  service's environment, or a callback URL registered at startup — need that variable
  set for the whole stack. Ports declare it with `port.env`, and cross-service
  references go in `[env]`, where `${MAGICTREE_PORT_<service>}` resolves to the port
  this worktree published.
- `[workspace] apps = [...]` at the repository root lists apps; each app has its own
  `magictree.toml` with bare service ids.

## When a stack will not start

```bash
magictree status --probe   # runs each health probe
magictree logs <service>
```

A failed `up` prints a report naming the service, the port and the variable
carrying it, the probe, the state of the process or container, and the last lines
of the service's own output. Read that block first: it usually contains the
actual error. A service that exits during startup fails within seconds rather
than waiting out the probe timeout.

Common causes:

- **Port already taken.** Host ports outside the manifest's `prefer` come from a block
  allocated per worktree. Run `magictree ports --reassign` to take a fresh block.
- **A container exited.** `magictree logs` only covers host processes; the failure report
  prints the exact `docker compose -p <project> logs <service>` command to use instead. A
  one-shot initialiser that exits zero is treated as finished, not failed.
- **Bootstrap failed.** The failing command is printed. Re-run `magictree up` after fixing
  the cause; already-satisfied steps are cached.
- **Dependencies are missing.** `needs` may reference a service that does not exist, or the
  manifest predates a renamed service. Read the error; it names the service and dependency.
  `magictree doctor` finds the same class of problem before you hit it at runtime.

## Environment notes

- Ports are per worktree, stable across `down`/`up`, and never user-visible inside
  containers: compose services talk to each other over compose DNS (`db:5432`), not over
  magictree ports. Publish a port only when the host needs to reach it.
- `MAGICTREE_SLUG` is stable per worktree and is the default `COMPOSE_PROJECT_NAME`.
- Generated state lives outside the checkout, in
  `~/.local/state/magictree/worktrees/<repo>/<worktree>/`. A checkout is never written to,
  so it needs no `.gitignore` or `git/info/exclude` entry. The state is safe to delete;
  `up` rebuilds it.
- `magictree gc` is the only command that reclaims what deleted worktrees left behind:
  port blocks, compose containers and volumes, and the host processes whose pid files
  only survive because the state is kept outside the checkout. Run it after removing
  checkouts outside magictree.
