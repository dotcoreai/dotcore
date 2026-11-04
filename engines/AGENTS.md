# Engine agentic instructions

An engine has three parts: its folder here (image or package, plus an `AGENTS.md` with update and
validate steps), its releases in [`catalog/engines/`](../catalog/engines), and its adapter in
`backend/core/src/model/engine/<engine>/`, which turns the shared parameters into engine flags and
rejects configurations the engine can't run.

## Where things live

- Image and package rules: [`apps/AGENTS.md`](../apps/AGENTS.md). Engine bumps: the `bump-engines`
  skill.
- Shared parameter ids and their shape: `backend/catalog/src/shared_parameters.rs`.
- Defaults, minimums, maximums and the descriptions people see: each release's parameters in
  `catalog/engines/*.yaml`.
- Flag mapping: the adapter's `plan.rs`. Engine rules and limits: its `declaration.rs`.

## Rules

- Read the adapter before reasoning about how a parameter reaches an engine or why a configuration
  is rejected. Don't infer flags or limits from upstream docs; the adapter decides what the engine
  receives.
- Don't copy flags, defaults or limits into docs or comments: point to the adapter or the YAML.
- Before claiming how an engine shares, evicts or queues its KV cache, read its upstream source at
  the pinned version. Memory estimates assume requests share no prompt prefix.
- After an engine bump, check the adapter's flags and the catalog descriptions against the new
  upstream behavior; upstream can change a default without touching a flag.
- When a shared parameter's meaning changes, update [`README.md`](README.md), the glossary in the
  root [`AGENTS.md`](../AGENTS.md) and the catalog descriptions in the same change.
