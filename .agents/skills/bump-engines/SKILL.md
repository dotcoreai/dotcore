---
name: bump-engines
description: Bump Dotcore's inference engine pins to newer upstream versions and qualify them on every platform. Use when asked to bump, update or upgrade an engine or all engines.
---

# Bump engines

Each engine's `engines/<engine>/AGENTS.md` owns its pin, release id convention and update steps; read the ones in scope first.

1. Pick each target per artifact, not per upstream release: an artifact can lag its release (a llama.cpp tag without its CUDA image, a ninfer commit its Dockerfile cannot build).

2. Audit upstream's changes against the flags the adapter emits (`backend/core/src/model/engine/<engine>/plan.rs`). Change a flag only when upstream forces it.

3. Bump each engine per its `AGENTS.md`. A new pin is a new release id, and it replaces the old release.

4. Sweep for the old release id, version and digest. Keep measurement-provenance comments in model files. A bump that changes no flag never needs backend test edits; if one does, the test asserts a committed declaration, which is a coupling bug to fix. When upstream forces an adapter change (step 2), its tests change with it.

5. Qualify on every platform the engine runs on (root `AGENTS.md` "Platform scope"), macOS included. Flag audits miss default-behaviour changes, which only the hardware tests and real deployments catch, so this step is required:
   * Run the hardware tests in the engine's `AGENTS.md` "Validate" on a host of each platform.
   * Deploy each changed engine's profiles on one platform, each on a host whose memory fits its per-device minimum. On the other platforms, deploy only the profiles most exposed to the audited upstream changes (drafts, the engine's memory paths) and any that read low; full sweeps cost hours of hardware and many GB of downloads. A row passes when it reaches Ready, answers two completions, shows draft acceptance above 0 where speculative, and launches with the argv the adapter produces.
   * Report sizing estimates that read low by more than 5% or 500 MiB, measured with the check of the `model-sizing` skill, and a largest fit that doesn't start or lands far below the engine's real start limit; don't refit sizing in a bump unless upstream changed how the engine holds memory, which needs the user's approval. A model that only overruns its margin gets, with approval, a small offset fitted to the measurements already taken, not a new sizing campaign.
   * Run remote hosts one at a time, with one remote command per row: an ssh agent that prompts for approval rejects concurrent signing.
   * Check a host's free disk before each new model download. The model cache is shared with other workspaces, so report a nearly full disk to the user instead of pruning it.

6. Fix any `AGENTS.md` step that proved wrong.
