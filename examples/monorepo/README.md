# monorepo

A workspace: a root manifest listing the apps and holding what they share, plus one
`magictree.toml` per app.

| path | what it is |
|---|---|
| `magictree.toml` | the workspace manifest: apps, shared environment, shared services, a shared job |
| `compose.yaml` | the containers `db` and `cache` refer to |
| `apps/web/` | a pnpm app: manifest, `package.json`, lockfile |
| `apps/api/` | a Python app: manifest, `pyproject.toml` |

Run `magictree doctor` here and it reports no drift.

## The root manifest

`[workspace] apps` lists the app directories. Each one must have its own manifest, and the
services in it are addressed as `app:id` (`web:web`).

`[env]` at this level is layered under every app's own `[env]`, so a shared value can
reference a shared port: `DATABASE_URL` resolves to the port allocated to `db` in this
worktree.

`db` and `cache` are workspace-level services, reachable from any app by their bare id.
`cache` is `expose = "none"`: the apps reach it over compose DNS, and nothing is published on
the host. `jobs.shared-seed` belongs to the workspace rather than to an app.

## apps/web

The app manifest keeps bare service ids; from another app or from the command line the
service is `web:web`. `needs = ["db"]` resolves to the workspace service, and
`${MAGICTREE_PORT_web_web}` is the app-qualified form of the port variable:
`MAGICTREE_PORT_<app>_<service>`. `jobs.migrate` needs `web`, which resolves inside its own
app first.

## apps/api

The same shape with a Python service: `target = { kind = "uv", script = "dev" }` runs
`uv run dev`, the console script declared in `pyproject.toml`. It needs both shared
services, and its job is manual.

## Running it

```sh
magictree up --all         # every app plus the shared services
magictree up web:web       # one service and its dependencies; shared services come along
magictree up shared-seed   # the workspace job: db, then the seed
```
