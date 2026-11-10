# Using Dotcore

This guide starts with your first model and ends with a site on several machines. It assumes Dotcore is [installed](install.md) and running.

## Run your first model

1. In the console, open **Models**, then **Catalog**, and deploy a model. Dotcore fits it to your machine's memory.
2. Wait until it runs. The first deployment downloads the model, which can take a while.
3. Open **Routes**, select the model's route and use **Chat** to try it.

## Use it from your tools

Each route is an OpenAI-compatible endpoint, and its route name is the `model` your client asks for.

- A route's **Connect** screen shows the address and commands to copy for curl, [OpenCode](https://opencode.ai), [Pi](https://github.com/earendil-works/pi) and [Codex](https://github.com/openai/codex).
- `dotc code` starts OpenCode, Pi or Codex set up with your models. The agent must be installed already.

## Add apps

Apps such as Open WebUI and ComfyUI run next to your models. Deploy them from **Apps**, then **Catalog**.

## Add more machines

A site can span several machines that reach each other directly. The first one is the manager, and the others join it as nodes.

- To add a machine, stop Dotcore on the manager and run `dotc node add` there. It prints a `dotc node join` command to run on the new machine.
- When you deploy a model or an app, you pick the machine that runs it.
- To remove a machine, first remove its deployments in the console. Then stop Dotcore on the manager, run `dotc node remove` there, and `dotc node leave` on the removed machine.

## Free disk space

- `dotc gc` removes engine and app images that no deployment needs anymore, and caches and app data shared across deployments that nothing has used for a week (`--unused-days` changes that). It asks before removing anything.
- Nix garbage collection (`nix-store --gc`) reclaims models and engines that no deployment uses.

## Command line

`dotc` manages your site from a terminal or a script. `dotc --help` lists its commands, and `dotc <command> --help` explains each one.

- `dotc site` sets up, resets or deletes a site.
- `dotc node` adds, lists and removes machines.
- `dotc model` and `dotc app` list, pause, resume and remove deployments.
- `dotc code` starts a coding agent.
- `dotc gc` cleans up unused images and storage.
- `dotc hardware` prints this machine's hardware for bug reports (`show`) and recommends models for it (`recommend`), without a site.
