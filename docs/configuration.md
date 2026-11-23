# Configuration

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
