# magictree

Per-worktree development stacks. Every git worktree gets its own ports, environment,
bootstrap, and supervised processes, so several branches run side by side without
colliding.

## Build

```sh
cargo build --release
install -m755 target/release/magictree ~/.local/bin/magictree
```

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
version = 1

[bootstrap]
run = [{ command = "pnpm install", inputs = ["pnpm-lock.yaml"] }]

[[services]]
id = "web"
target = { kind = "pnpm", script = "dev" }   # or mise, just, npm, uv, python, command
port = { env = "PORT" }
needs = ["api"]
health = { http = "/", timeout = 60 }
```

Services are either `compose` (publishing a container port on an allocated one) or host
processes. Bootstrap steps with `inputs` are skipped when those files are unchanged.

## Layout

| | |
|---|---|
| `magictree.toml` | committed per app; a root one with `[workspace]` for monorepos |
| `<worktree>/.magictree/` | generated: `env`, `ports.json`, `run/`, `log/` |
| `~/.local/state/magictree/` | machine-wide port assignments |
| `~/.config/magictree/config.toml` | optional: port range, timeouts |

Ports come from `20000-32767`, are stable across restarts, and are never user-visible
inside containers — compose services reach each other over compose DNS.

Generated state is ignored through `git/info/exclude`, so it never appears in `git status`.
magictree never writes to repository `.env` files.

## Agents

`skills/magictree/SKILL.md` teaches a coding agent to operate the CLI.
