# Worktrees

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
