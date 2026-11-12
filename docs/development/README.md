# Developing Dotcore

How to build and run Dotcore from source. Every workflow is a `just` recipe: `just --list` shows the root ones, and `just --list <module>` those of a module, such as `just --list dev`. The [root `AGENTS.md`](../../AGENTS.md) holds the coding and documentation rules, and [CONTRIBUTING.md](../../CONTRIBUTING.md) how to send a change.

More guides:

- [Testing](testing.md)
- [Remote hosts](remote-hosts.md), to run commands on machines with hardware you lack
- [Development cluster](cluster.md), to spread a development site over several machines
- [Images](images.md), how app and engine images are built and pinned

## Setup

You need [Nix](https://nixos.org/download/), [direnv](https://direnv.net/) and Docker (see [Installing Dotcore](../install.md)). Then, from the repository root:

```sh
direnv allow
just bootstrap
just dev::up
```

- direnv loads the development shell and this checkout's settings, and prints the console's address.
- `just bootstrap` installs the frontend dependencies and sets up a development site with simulated models and apps. Run it once per checkout. `just dev::up` won't start before it.
- `just dev::up` starts the backend, the frontend and the web server in front of them.

## Workspaces

Each checkout, jj workspace or Git worktree is its own workspace. Each one gets its own ports, site and stack, so several can run side by side.

- direnv writes the workspace's settings to `.env.local`, which is ignored by version control. Run `direnv allow` in each new one.
- `DOTCORE_BACKEND_RUN_MODE` in `.env.local` picks how the backend is built, `dev` by default. Use `dev-release` when the site downloads models, since it checks their weights much faster.
- To reach a workspace from another machine, set `DOTCORE_MACHINE_HOST` in `.env.local` to this machine's address.
- Workspaces take their ports from lock files in the main checkout's `.dotcore` folder. Don't delete them.

## Development site

- `just dev::site-init` and `just dev::site-reset` set up or reset the site. They ask what it runs: simulated models and apps work on any machine.
- Simulated models run on simulated hardware instead of the real GPUs: any set of GPUs on one simulated host, or a whole machine such as a GB10 or a Mac. Pass it as the inference choice, such as `just dev::site-reset simulated:rtx-5090,simulated:rtx-4090 docker`; `dotc site init --help` lists the names. Bare `simulated` is one RTX 5090. Asked interactively, the recipes list it beside the real GPUs.
- Stop the stack with `just dev::down` before a reset.

## Stack and logs

- `just dev::up` runs the stack in the foreground, `just dev::up-daemon` in the background, and `just dev::attach` opens it.
- `just dev::logs <process>` shows recent logs. They go away with the stack. The backend also writes them to `logs/` in the site folder.
- `dotc model logs <deployment>` and `dotc app logs <deployment>` show a deployment's own logs, whichever node runs it. Add `--follow` to keep watching.
- `just dev::pc` controls the stack's processes.

## Adding models and apps

Models and apps are added with the `add-model` and `add-app` skills in `.agents/skills/`. Coding agents find them on their own. They're plain Markdown, so you can also follow them by hand.

## Cleanup

`just clean` removes the development site, its containers and build output. Stop the stack first.

## Troubleshooting

- If two workspaces get the same ports, remove one's `.env.local` and run `direnv reload` there.
- If a new workspace doesn't load, run `direnv allow` in it.
- The stack shows `Launching` even for a running process. Check `just dev::logs <process>` instead.
