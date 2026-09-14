# single-app

A single-app repository: one `magictree.toml` at the root of the checkout, using every
section of the manifest.

| file | why it is here |
|---|---|
| `magictree.toml` | the manifest |
| `compose.yaml` | the two containers `db` and `migrate-db` refer to |
| `package.json`, `pnpm-lock.yaml` | so the `pnpm` target, and the install step's `inputs`, resolve |

There is no application source: the manifest is the example. Run `magictree doctor` here and
it reports no drift.

## magictree.toml

`version = 1` is the only required key.

`[app] id` names the app. It defaults to the directory name, and because this repository has
no `[workspace]` the app layer adds no qualification to service names.

`[env]` is layered under the values magictree computes, so it can interpolate the ports
allocated to this worktree. `${MAGICTREE_PORT_web}` is the plain form of a port variable:
a single-app repository has no app layer, so nothing is prefixed. Magictree's own keys
(`MAGICTREE_*`, `COMPOSE_PROJECT_NAME`, `COMPOSE_FILE`) cannot be redefined here.

`[bootstrap] sync` links the named paths from the primary checkout when a worktree is
missing them; build output like `node_modules` is untracked, so a new worktree does not
inherit it. `run` installs from the lockfile, and `inputs` skips that step while
`pnpm-lock.yaml` is unchanged since it last succeeded.

`db` is a container. `compose` names the file and the service in it, `port.target` is the
container-side port to publish on an allocated one, and `port.env` is the variable the
compose file interpolates (`${WT_PORT_DB:-5432}` in `compose.yaml`). `health.tcp` waits for
that port to accept a connection.

`web` is a host process run through pnpm. `prefer` takes 5173 when it is free and falls back
to an allocated port otherwise; `needs` holds the service back until `db` is healthy.
`port.env` must name the variable the process actually reads: the manifest cannot fix a
service that reads a different one.

`api` declares two ports. A service with several ports names each one, and both variables
are set for the process. `$API_PORT` inside the command is expanded from the allocation, so
the command itself never pins a port; a command that did (`--port 8000`) would ignore the
variable and collide with the next worktree.

`migrate-db` is a one-shot container. `expose = "none"` publishes nothing, and
`wait = "exit"` makes `up` wait for it to finish successfully instead of moving on while it
runs.

`jobs.migrate` runs on every `up` that selects this app, after `db` is healthy. It is not
remembered as done, so its command has to be safe to repeat. `jobs.seed` is
`when = "manual"`, so it runs only when named: `magictree up seed` starts `db`, runs
`migrate`, then runs `seed`.

## Running it

```sh
magictree --dry-run up    # what would start, on which ports, in which order
magictree up              # docker for the containers, pnpm for web, uv for api
```
