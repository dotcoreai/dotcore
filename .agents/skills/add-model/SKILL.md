---
name: add-model
description: Add a new model (or a new quantization tier of one) to the Dotcore master catalog, from finding its facts to a smoke-tested declaration. Use when asked to add, port or import a model from Hugging Face or a link to one.
---

# Add a model

## Context

A model is one YAML file in `catalog/models/`: artifacts (pinned weights), configurations (how an engine runs them, with sizing and hardware support) and profiles (named launch presets). Adding a model is a catalog-only change. The neighbouring model files are the reference: their header comments carry provenance and qualification notes, and their naming shows the conventions. Platform coverage follows the root "Platform scope" rule unless the user names a narrower target; record that scope in the change description.

Sizing is owned by the `model-sizing` skill. Catalog loading and validation live in `backend/catalog/`; `backend/AGENTS.md` says what catalog tests may assert.

## Instructions

1. Gather the facts from the Hugging Face repository, its card and `config.json`; the public HF API serves revisions, file digests and file metadata without credentials. A post or page link may block fetching, so follow it to the repository.
   * Collect what the declaration needs: architecture (dense or mixture of experts), maximum context (confirmed against the weight files' own metadata), license, modalities, sampling, and reasoning and tool-call parser needs.
   * Prefer official quantizations, then reputable third-party ones, and check which weight formats the target hardware and engines can run. Say when a source is new or from an individual uploader.

2. Choose artifacts by what fits the target hardware with some KV cache. Among builds of similar size, prefer the higher-quality quantization method, and leave out tiers that cannot load.
   * Pin every file to a full repository revision and a SHA-256 digest. HF reports digests only for LFS files, so hash the small files yourself; launches verify downloads against them.

3. Write the model file from the closest existing one by engine, weight format and features (vision, MTP, mixture of experts).
   * Prefer the engine-embedded chat template; add a template artifact only when the smoke test shows the embedded one fails.
   * Size each configuration with the `model-sizing` skill.
   * Leave out optional fields you cannot support.

4. Validate the declaration with the catalog tests and the area gates.

5. Smoke-test one tier per engine on the target hardware with `dotc dev workload` (remote hardware follows `dev/AGENTS.md`).
   * Check a plain chat, a reasoning request and a tool call.
   * A finished launch leaves its handle file behind; give each further launch a fresh handle path.
   * Stop and ask the user if the template, parser or load fails; do not improvise workarounds.

6. Record in the change description the chosen sources and why, tiers left out, and each smoke test with its hardware.
