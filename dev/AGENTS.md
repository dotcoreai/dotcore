# Development agentic instructions

The development stack runs Dotcore from source, one isolated copy per workspace: a Nix dev shell
that direnv loads, a Process Compose stack (backend, frontend, ingress) and a development site.
`just --list dev` and each command's `--help` own the details, and
[docs/development/](../docs/development/README.md) explains the workflows for people. These rules
cover what agents get wrong.

## Nix and the environment

- Run every command in the workspace's direnv shell. Nix provides the tools there, plus `dotc` and
  its launcher; never install tools globally or run binaries from `backend/target/` directly.
- Outside that shell, `dotc` resolves to an older installed build from the user profile, which fails
  newer recipe flags with `unexpected argument`. Check `command -v dotc` points into the workspace's
  shell before blaming a recipe.
- direnv reloads the shell when `flake.nix`, `flake.lock` or `.env.local` change.
- [nix/AGENTS.md](../nix/AGENTS.md) owns the Nix rules, including the one agents miss most: the
  flake sees only files version control knows about.
- Each checkout, jj workspace and Git worktree gets its own `.env.local`, ports, site and stack. Run
  `direnv allow` in every new one.
- Run `just bootstrap` once per workspace before any gate: until then frontend recipes and
  `just check` fail with `prettier: command not found` or `eslint: command not found`, and
  `just dev::up` refuses to start.
- `DOTCORE_BACKEND_RUN_MODE` picks how the stack builds the backend: `dev` (default),
  `dev-release` or `nix`. Before deploying a model whose weights aren't cached, set every backend
  that will download them, local or remote, to `dev-release` in its `.env.local`, then reload
  direnv and restart the stack: the `dev` build verifies multi-GB downloads so slowly that they look
  stuck. Changing the mode of a running stack needs `just dev::down` first.

## Stack

- Start it with `just dev::up-daemon`. `just dev::up` is the interactive terminal UI, not for
  agents.
- Reuse a running stack and leave it running; stop it only if you started it in this task.
- `just dev::pc` runs any Process Compose command against this workspace's stack.
- A running process shows `Launching`, since the stack defines no readiness probe. Judge readiness
  by `is_running` in `just dev::pc process get <process> -o json`, the listening ports and the
  logs.
- Nothing hot-reloads backend changes: restart with `just dev::pc process restart <process>`.
- `just dev::logs <process>` shows recent logs, which are lost when the stack stops. The backend
  also writes durable logs to `$DOTCORE_SITE_DIR/logs/`.
- Open the console at `DOTCORE_PUBLIC_ORIGIN`, which direnv prints, never at
  `DOTCORE_FRONTEND_ORIGIN`: only the public origin serves the APIs.

## Development site

- Change the site with the dev recipes (`site-init`, `site-reset`, `clean`), never with lifecycle
  helpers or by hand-editing its files.
- Simulated inference runs on simulated hardware, chosen as `simulated:<name>` inference choices (`dotc site init --help` lists the names). Compose the GPUs a check needs rather than defaulting to a whole machine. It replaces the node's real GPUs, so never pair it with `nvidia` or `apple` inference on one node; use a cluster with a simulated worker.
- Stop the stack with `just dev::down` before a reset, then retry if the site lock is held.
- Run destructive recipes non-interactively: `DOTCORE_YES=1` plus both runtime choices, such as
  `DOTCORE_YES=1 just dev::site-reset simulated docker`.
- You may reset or clear a development site inside a jj workspace or Git repository without
  asking. Ask before touching a site or resource outside one, or any external resource.

## Real hardware memory

- Before starting a model or other memory-heavy workload on a real machine, local or remote, make
  sure it fits in the memory free there now, counting what already runs and what is still being
  released. On unified-memory machines (GB10, Apple Silicon) the OS shares that memory, so a load
  that doesn't fit freezes the whole machine until someone power-cycles it: leave the OS room.

## Remote hosts

Use a remote development host whenever work needs hardware or an OS this machine lacks, such as
NVIDIA on Linux arm64 or a Mac. It runs your uncommitted workspace state.
[remote-hosts.md](../docs/development/remote-hosts.md) owns setup and behavior, and
`just --list dev::remote` lists the commands.

- Run `just dev::remote::list` first. If no host is usable, stop and report its output; don't retry
  or edit the host configuration.
- `just dev::remote::run <group>[:<host>] <recipe...>` syncs your workspace and runs a recipe
  there. Exit status 125 means the tool failed, any other status is the recipe's.
- Let the tool move code. Never rsync, push to `origin`, or touch a host's own checkout or its
  `default` workspace.
- `exec` runs in the last synced state, so `sync` again after local edits. Use it instead of raw
  `ssh`, and read remote settings with it: a remote workspace derives its own ports and site.
- For a long run, use `run --detach`, then `wait` or `logs`. Start long-lived services with a recipe
  that daemonizes them (`dev::up-daemon`, `dev::cluster::up`); otherwise a foreground `run` waits on
  them.
- Remove your workspace from a host when done: `DOTCORE_YES=1 just dev::remote::clean <host>`.
- A run that stalls on an ssh agent prompt means the user is away: stop, don't retry, and tell the
  user.

## Development cluster

- Read [cluster.md](../docs/development/cluster.md) before starting a cluster, on one machine or
  several. It owns the workflow.
- Pick join arguments and deployments for what you test. Default to simulated or small model
  profiles, and deploy apps only when the task needs them: their data can block worker cleanup.
- Clean up every worker you joined before reporting the work done, and check each remote host for
  leftovers. If `just dev::cluster::clean` refuses or leaves containers behind, stop and tell the
  user instead of deleting worker data or containers by hand.
- Remote hosts are shared: never stop or remove workloads that belong to another site.

## Changing the development tooling

- Run `just dev::test` after changing anything in `dev/`.
