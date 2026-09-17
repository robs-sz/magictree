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

No manifest means magictree cannot start a stack. Onboard the repository first;
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
magictree init --reanswer                  # ask again, ignoring the recorded answers
magictree init --print                     # show the manifests, write nothing
magictree init --save-answers answers.json # record what was answered

magictree doctor                           # compare manifests against the repository
```

`discover` only reads files. It never runs anything from the repository, and it
never guesses: whatever it cannot determine becomes an explicit question.
`init` refuses to write anything while a question is unanswered, and it refuses
answers that were computed from a different report, so re-run `discover` after
changing the repository, rather than editing an old answer file.

An existing manifest is **added to, not replaced**: `init` appends the service
blocks the manifest is missing (a Storybook, a compose service the answers now
manage) and prints `added service 'storybook'`. Every other line, comment and
value stays as written, and a step the manifest already runs is never added a
second time under the plan's name for it.

`init` records what it was told in the manifest at the repository root (the answers, and the
options each one turned down) and replays it: the next run asks only about what the
repository has since gained, and a question answered `skip` stays skipped. A question that
offers a set of things is decided one option at a time, so when someone adds a compose
service, `init` reports that its answer never decided it and leaves it undecided rather than
assuming a no. The next interactive run asks about it, marking the new options. Do not edit
that line to change an answer: run `magictree init --reanswer`, and keep the manifest as the
place where choices live. `init --force` regenerates the file from the recorded answers,
which is how an answer that changed reaches a service that was already written.

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
magictree up            # build images, allocate ports, bootstrap, start services, wait for health
magictree up --no-build # skip the image build for a stack already built
magictree status        # what is running, on which port
magictree ports         # the port assignment as URLs
magictree logs web -f   # follow one service's output
magictree restart web   # stop one service and start it again (rebuilds its image); deps untouched
magictree down          # stop everything, keep volumes
```

`up` finishes by printing every reachable URL, plus any service that is internal
to the stack (`expose = "none"`), so the addresses to use are always in the
output. It is idempotent: run it any time. It is safe to re-run after editing `magictree.toml`.
It exits non-zero and explains itself when a service fails to become healthy: on failure
everything stays running so it can be inspected.

`up` and `restart` build the compose services they start, which is how a changed Dockerfile or
build context is picked up; Compose validates its cache, so an unchanged context is a cache
hit rather than a rebuild. `build` in `~/.config/magictree/config.toml` sets the default —
`"always"` (the default), `"ask"` (one question per run, naming the services that would
build, declined without a terminal), or `"never"` — and `--build`/`--no-build` override it for
one run. A service that only names an `image:` is never built and never asked about.

In the primary checkout `up` uses the ports the manifest declares with `prefer`, because the
repository's own tooling and the files it generates were written against them. A linked
worktree ignores `prefer` and allocates every port from its own block. Two flags step outside
that, both refused while the stack is running:

```bash
magictree up --ports generated   # this checkout's stack on block ports, beside another stack
magictree up --ports declared    # back to the declared ports
magictree ports --release        # drop the assignment: its ports stop being reserved
```

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

The primary checkout's ports are the ones its manifest declares, and it keeps them while
stopped: `ports --release` drops the assignment, its reservations included, and the next
command that needs them allocates again. A linked worktree always allocates its own block, so
the same service binds a different port in every checkout.

## Selecting services

In a single-app repository, service names are bare (`web`, `api`).

In a monorepo, app services are qualified as `app:service` (`web:web`, `api:api`), and
repository-level services such as shared `db` or `cache` are bare.

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
magictree exec -- <cmd> ...    # run a command with that environment
```

Values come from three layers, lowest first: repository `[env]`, app `[env]`, then values
magictree computes (slugs, ports). Ports appear as `MAGICTREE_PORT_<name>`, and every
variable a service names in `port.env` is a computed value too: the whole stack's set reaches
every launched process and `magictree env`, so a host tool reads a peer service's declared
port without restating it in `[env]`. Restating one in `[env]` is an error, not an override.

`magictree exec -- <cmd> ...` runs a command with that whole environment, verbatim (no
shell), and exits with the command's own status. Prefer it over hand-rolling
`eval "$(magictree env --export)"`; reach for `magictree exec -- sh -c '...'` when the
command needs a shell (`|`, `&&`, globs).

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
after = ["pnpm db:seed"]                     # once every selected service is healthy

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
- `prefer` names the port the service uses in the primary checkout. Declare it wherever the
  repository already assumes a fixed number — its own start script, a generated `.env`, a
  callback URL registered with an identity provider — because those files are written against
  that number and a stack moved off it silently splits them. A linked worktree ignores `prefer`
  and allocates from its block, so declaring one never costs another worktree anything. Do not
  declare one just to prettify an allocated port. `require` pins a port in every worktree and
  fails loudly when it is unavailable.
- A command that pins a port (`next dev -p 3005`, `uvicorn --port 8000`,
  `DATABASE_PORT=5433`) ignores `port.env`, so the second worktree tries to bind the
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
- Bootstrap steps with `inputs` are skipped when those files are unchanged. An app
  manifest's `sync` paths and `inputs` are relative to that manifest's own directory, the
  same directory its commands run in: `inputs = ["uv.lock"]` in `api/magictree.toml` means
  `api/uv.lock`. Only a root manifest's paths are relative to the repository root.
- `[jobs.<id>]` with `when = "up"` runs once after its `needs` are healthy; `when = "manual"`
  never runs automatically. A script that needs the whole stack (seed, smoke test, browser)
  belongs in `[bootstrap] after`, which runs once every selected service is healthy:
  `when = "up"` only waits for its own `needs`. Both take the same step shape, `inputs`
  included.
- A step with `ask = true` asks before it runs: `up` prints `Run task: <command> (y/N)` and
  runs it only on y/yes. A decline is cached nowhere, so the next run asks again, and a run
  without a terminal (scripts, agents, CI) always declines. Use it for an after step you only
  sometimes want, like a smoke test or a browser session.
- A one-shot compose initialiser (provisioning, migrations) needs `wait = "exit"`, so
  `up` waits for it to finish successfully instead of moving on while it runs.
- Services that derive their own URLs from a port (`${WT_PORT_AUTH}` in another
  service's environment, or a callback URL registered at startup) need that variable
  set for the whole stack. Declare it with `port.env`; it is then in every launched
  process and in `magictree env`, so a host CLI reads a peer's port without a `[env]`
  restatement. Other cross-service values go in `[env]`, where
  `${MAGICTREE_PORT_<service>}` resolves to the port this worktree published.
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

- **Port already taken.** A port outside the primary checkout's declared `prefer` ports comes
  from a block allocated per worktree. Run `magictree ports --reassign` to take a fresh block.
  If a *declared* port is already in use, another stack owns it (the repository's own start
  script, most likely): stop that one, or run `magictree up --ports generated` to put this
  checkout on its block ports instead.
- **A container exited.** `magictree logs` only covers host processes; the failure report
  prints the exact `docker compose -p <project> logs <service>` command to use instead. A
  one-shot initialiser that exits zero is treated as finished, not failed.
- **A build is slow or unwanted.** `up` and `restart` build the compose services they start,
  which is how a changed Dockerfile or build context is picked up. There is no cheaper check
  to ask Docker for, so the build is the check: an unchanged context is a cache hit. Pass
  `--no-build` to skip it for one run, or set `build = "never"` (or `"ask"`) in
  `~/.config/magictree/config.toml`.
- **Bootstrap failed.** The failing command is printed. Re-run `magictree up` after fixing
  the cause; already-satisfied steps are cached. An `after` step that fails says so and leaves
  the stack running, with its URLs already printed: fix the script and run `up` again.
- **Dependencies are missing.** `needs` may reference a service that does not exist, or the
  manifest predates a renamed service. Read the error; it names the service and dependency.
  `magictree doctor` finds the same class of problem before you hit it at runtime.

## Environment notes

- Ports are per worktree, stable across `down`/`up`, and never user-visible inside
  containers: compose services talk to each other over compose DNS (`db:5432`), not over
  magictree ports. Publish a port only when the host needs to reach it. The primary
  checkout's ports are the ones its manifest declares; a linked worktree's come from its block.
- `MAGICTREE_SLUG` is stable per worktree and is the default `COMPOSE_PROJECT_NAME`.
- Generated state lives outside the checkout, in
  `~/.local/state/magictree/worktrees/<repo>/<worktree>/`. A checkout is never written to,
  so it needs no `.gitignore` or `git/info/exclude` entry. The state is safe to delete;
  `up` rebuilds it.
- `magictree gc` is the only command that reclaims what deleted worktrees left behind:
  port blocks, compose containers and volumes, and the host processes whose pid files
  only survive because the state is kept outside the checkout. Run it after removing
  checkouts outside magictree.

## Updating magictree

The installed binary is the authority on flags, and updating it is the user's call:

```bash
magictree update          # install the latest release over the binary that is running
magictree update --check  # say whether a newer release exists; install nothing
```

A command that ends with

```
magictree 0.1.3 is available (this is 0.1.2); run `magictree update`
```

is telling the user a release has landed since magictree was installed. Say so and carry on;
do not install it unless asked. Never fetch a release by hand either, or point the user at a
package manager: `update` is the way, and it replaces whichever file is running.
