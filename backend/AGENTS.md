# Backend agentic instructions

## Code map

```
backend/
├── cli/               `dotc` executable (dotc): commands, root path options, guided prompts, terminal layout
├── core/              top crate (dotcore): controllers, router, dispatch, logs, server, composition
│   ├── src/composition/    composition root: process configuration and wiring
│   ├── src/model/          inference controller
│   ├── src/application/    application controller
│   ├── src/hardware/       owner-neutral hardware selection, attribution and the site workload source
│   │   └── capacity/       capacity component: memory pools, occupancy, demand, fit, largest fit, starting value, placement preview, node pools
│   ├── src/route_manager/  inference routes
│   ├── src/router/         inference request handling
│   ├── src/dispatch/       controller runtime actions under site lifecycle fencing
│   ├── src/inference/      engine-neutral inference contracts
│   ├── src/server/         listeners and transport assembly
│   ├── src/{diagnostic/,deployment_logs/}, management_logs.rs, logging.rs   logs
│   ├── src/{metrics.rs,metric_stream.rs}   observability transport: management metrics API and node metric bucket streams
│   ├── src/resource_watch/ management resource watches: owner reads published per manager tick
│   ├── src/node_memory/    node memory read: every node's pools with every lane's deployments, and its management service
│   ├── src/telemetry/      workload resource records and sizing fit
│   ├── src/development/    direct development workload launch
│   └── {src/testing.rs,tests/}   integration test binaries and their test-only entry points (see Tests)
├── site/              site state authority (dotcore-site): repository, site store and node store, desired views and fencing, nodes, membership, initial site, reset
├── artifact/          artifact manager (dotcore-artifact)
├── catalog/           catalog declarations and sizing (dotcore-catalog)
├── runtime/           owner-neutral runtime contracts (dotcore-runtime)
├── runtime_backends/  Docker, native and simulated backends, backend registry (dotcore-runtime-backends)
├── node_hardware/     node hardware facts, signal sampling, declared machines and the hardware simulated inference offers, in `simulated/` (dotcore-node-hardware)
├── observability/     metric descriptors, time-aligned aggregates, metric store boundary and in-memory store, node metric forwarder and bucket stream ingest (dotcore-observability)
├── domain/            shared primitives, backend clock and timing policy, fs_sync, shared unit-test support (dotcore-domain)
├── host_process/      raw host process facts (dotcore-host-process)
├── launcher/          per-workload process holder binary and its supervisor client (dotcore-launcher)
├── nvml/              sole NVML boundary (dotcore-nvml)
├── proto/             generated bindings
└── fixtures/          shared test fixtures
```

- Crate layers, lowest first: the leaves `proto`, `nvml`, `host_process` and `launcher`; then `domain`; then `catalog`, `runtime`, `node_hardware`, `observability` and `artifact`; then `site` and `runtime_backends`; then `core`; then `cli`. A crate imports only lower layers; each crate's `Cargo.toml` holds its exact dependency list and its `description`.

## Workflow

- Use `just backend::<recipe>` from the repository root. Before completing backend changes, run `format`, `clippy`, `test` and `build`.
- Follow [`dev/AGENTS.md`](../dev/AGENTS.md) for the development stack, the workspace environment and `DOTCORE_BACKEND_RUN_MODE`. Composition owns the `DOTCORE_*` variables the backend reads; `dev/workspace-environment.mjs` derives them.
- Every crate is published to crates.io, and each one must build from its own package: no build input outside its folder. `proto/dotcore` is a link to the repository's contracts for that reason; keep it a link. A new workspace crate:
  - inherits the shared package fields (`version`, `license`, `repository`, `homepage`, `readme`) with `.workspace = true`, and gets a `[workspace.dependencies]` entry with its version
  - must pass `cargo package` before it lands
  - tell the maintainers in the change description: once it is first published, a maintainer adds the maintainers team as its owner with `cargo owner --add github:dotcoreai:maintainers <crate>`
- Publishing is a maintainer action through `just backend::publish`. Agents may run `cargo package` or `cargo publish --dry-run`, never a real publish.

## Design

- The running server's in-memory site is the declarative authority (`docs/architecture/architecture.md`). Owner-scoped mutations update only their section, write `site.yaml` and notify controllers to reconcile from memory; a write failure is an error but never rolls back the in-memory mutation. Never add a hidden desired-state store.
- Retained `state.pb` holds observed and historical model and application state in owner sections, never desired declarations or deployment configuration. Owner repositories are scoped access to the shared `site.yaml` and `state.pb`, not separate files.
- Lower crates never import higher ones (layers under Code map). Library crates stay invocation-neutral: docs and errors never name `dotc` commands or flags, and state remedies in domain terms ("reset the site"); the CLI adds command hints by matching typed errors or transport reason codes.
- A component's public service lives in the crate owning that component. `core` hosts the transport adapter of a proto-free crate (`observability`) and services combining several owners (`resource_watch/`), which owner crates cannot import.
- Run blocking work outside async executors, bound its concurrency by its physical owner, and keep cancellation and partial failure observable. [`proto/AGENTS.md`](../proto/AGENTS.md) owns each operation's latency and cost contract.
- Workload containers never use container DNS for site host names: the launch names them in `host_aliases`, and the hosting node's runtime resolves and pins them as host entries.
- Only `backend/nvml` depends on `nvml-wrapper` and discovers the vendor library, in code. Every other crate goes through `dotcore-nvml`; never rely on `LD_LIBRARY_PATH` or `opengl-driver` paths.
- Keep hardware behind a description. Only the code that reads real hardware (`nvml`, the `node_hardware` collectors and the runtime backends) touches the actual GPU or computer; everything else works from the machine description and never checks for a specific vendor, GPU model or the local computer. When a hardware difference matters and the description can't express it, extend the description (or the simulated runtime) so it can, rather than adding code for one machine.
- Prefer trait objects at injectable boundaries that are not performance-sensitive. Keep a generic only for a measured performance need or an ownership or lifetime constraint, and document why; declare a generic struct parameter's single trait role as a bound on the struct.

## Code style

- The backend targets only Linux and macOS. Use `std::os::unix` directly; never add `cfg(unix)`, `cfg(not(unix))` or `cfg(windows)` gates. Use `cfg(target_os = ...)` only for real Linux versus macOS differences.
- A module with child modules uses `<module>/mod.rs`, never also `<module>.rs`. Use snake_case file names.
- Give an item the narrowest visibility that compiles: `pub` only when another crate uses it, `pub(crate)` for cross-module use, `pub(super)` when only its parent uses it, private otherwise. Never widen visibility for symmetry with neighbours.
- A `pub use` re-exports only from a private module; an item has one public path. `core/tests/backend/public_paths.rs` enforces it for `lib.rs` and `mod.rs`.
- Order `use` blocks std, then external crates (`dotcore_*` workspace crates included), then `crate`/`super`/`self`, with one blank line between groups; rustfmt only sorts within a group.
- Keep one inherent impl directly after its type unless responsibility modules justify a split. Put exactly one blank line between functions, impl blocks and adjacent enum variants (the blank line goes before a variant's attributes and docs).
- Every authored module opens with a `//!` doc a junior developer can read first: a one-line summary, a blank line, then two or three plain sentences on what it does for its callers and its guarantees, naming no internal items. Library crates name the component or capability. Major components also state their boundary. Model: `backend/catalog/src/sizing/expression.rs`.
- Give public value and error types and non-trivial public APIs short capability-focused docs.
- Prefer root-cause fixes over lint suppressions. Every suppression gives `reason = "..."`, with `#[expect]` when the lint reliably fires; generated bindings may use one justified crate-level suppression.
- `backend/proto` holds only generated bindings and generic protobuf utilities; [`proto/AGENTS.md`](../proto/AGENTS.md) owns generation.

## Errors

- Authored error types derive `Debug` and `thiserror::Error` and declare messages and sources through `thiserror` attributes; a manual `Error` impl needs a documented constraint.
- Messages carry stable, source-independent context; dynamic fields and sources supplement it but never form the whole message. Use a transparent variant only to forward the wrapped error as the complete message.
- Preserve source chains and backtraces: add context with `anyhow::Error::new(source).context(..)` or `.context(..)`, and use `anyhow!(..)` only for a dynamic leaf with no source. Never stringify or repeat a retained source; use a `String` payload only when an external API forces it.
- Keep error data in its owning variant. Box only the field behind `clippy::result_large_err`; add a standalone error type only for independent behavior or several owners.
- Transport boundaries project explicit safe messages, never internal diagnostics. Only the local site-administrator CLI may print the full diagnostic chain on stderr.

## Logging

- Make important operations and decisions visible with direct `tracing` events (`info!`, `debug!`, `warn!`, `error!`) carrying identifiers and decision or outcome context; spans alone are not enough.
- Spans carry immutable correlation and duration. Record changing lifecycle state and repeated outcomes as events, never repeated `Span::record` on a long-lived span; a one-time terminal field on a short-lived span is fine.
- Emit success, transition or readiness events only after the state change or publication succeeds. Avoid unchanged-loop noise, especially at `info!`.

## Tests

- Test-only items with no production caller compile only under their crate's `test-support` feature, which test builds enable and production never does. `dotcore`'s feature enables the lower crates' features; integration and CLI tests reach test-only items through `dotcore::testing`, unit tests through `crate::`. The feature only adds items and never changes default behavior, because CLI process tests run the `dotc` built with it.
- Test-only methods stay in their type's impl, last in that impl, gated `#[cfg(any(test, feature = "test-support"))]` (`#[cfg(test)]` when only unit tests use them). Test-only free items (fixtures, fakes, helpers) go after `mod tests`.
- Every behavior duration lives in `Timing` (`domain/src/timing.rs`), composed into the component that waits on it; never add a behavior duration const or a per-timer test knob. Review holds this rule; a constant stays outside `Timing` only when it bounds real I/O or a real process, or is a fixed contract. Production code waits through the backend clock; a wait on real I/O or a real process goes through `dotcore_domain::real_time` instead, and clippy rejects any other Tokio timer.
- Tests run `Timing::test()` on one of two clocks: by default a manual clock (real time plus the test's advances; in-process harnesses start it at a fixed wall origin), and for exact counts or exact elapsed time a frozen `VirtualClock` the test advances. Subprocesses get their timing through `ProcessSettings` or `TEST_TIMING_ENVIRONMENT`.
- Unit tests stay in the module file; a test block over about 1k lines may move to `<module>/tests.rs`. Module-local test support goes after the tests that use it.
- Whole-backend, transport, multi-node and Docker-, NVIDIA- or Apple-gated suites are modules of the matching `core/tests/` binary (`backend`, `transport`, `multi_node`, `gated`), never a new binary. Run gated suites the local machine cannot host on a remote development host (`just dev::remote::run`, see `dev/AGENTS.md`).
- A port a test chooses before something else binds it comes from `dotcore_domain::test_support::ports::reserve_port` (`dotcore::testing` for CLI tests), held while it may be bound; never from a `bind(…, 0)` released before use.
- Never add artificial load to reproduce a flaky test: no busy loops, load generators, background test runs or parallel copies of the suite. The machine is the developer's own and stays usable. Repeat a test in the foreground with nextest's stress mode (`--stress-count`) and a capped `--test-threads`. When a reproduction needs more load, more runs or a long duration, stop and ask the user with a plan: what runs, for how long, and the CPU and memory it takes.
- Test catalog behavior with minimal synthetic fixtures. Never assert individual committed declarations or enumerate them; repository tests may only check that the whole catalog loads and generic invariants hold.
- Test hardware-dependent behavior on simulated hardware: compose the GPUs the test needs on the default simulated host, or declare a description in the test, rather than defaulting to a whole machine from `node_hardware/simulated/machines/`; it then runs everywhere in `just check`. If the simulation can't reproduce what you need to test, improve the simulation first; reaching for real hardware is the exception.
- Real-hardware tests only confirm that the simulation matches reality: that hardware readings match the description, that engines use the memory we estimate, and that a platform runtime behaves as the simulated runtime assumes. Run them when that match may have changed (hardware reading code, an engine version, a sizing formula, a new kind of machine), not for each feature. The real machines they compare readings with are test data in `core/tests/gated/machines/`, never simulated choices.
- Hardware tests are named `hw_docker_`, `hw_nvidia_`, `hw_nvidia5090_`, `hw_gb10_` or `hw_apple_` and `#[ignore]`d with a reason naming their recipe; other ignored tests end in `_helper` or `_stub`, or start with `net_`. The coverage gate in `just check` enforces it.
- Real-host tests use the smallest pinned model fixture the engine supports. Run production-sized models only for separately approved model-specific qualification.
- Whenever work tests, deploys or downloads models, run every backend involved, local or remote, in `dev-release` mode first; a debug backend verifies model digests so slowly that downloads look stuck. [`dev/AGENTS.md`](../dev/AGENTS.md) says how to switch.

## CLI code

- `dotc` writes the exact three-line startup protocol to stdout after backend preparation; diagnostics and failures go to stderr, so stdout stays machine-readable.
- Guided prompts use the controlling terminal (`/dev/tty`) only, never stdout or stderr.
- Human output goes through the shared terminal layout (`cli/src/terminal/`) and failure rendering (`cli/src/failure.rs`); never hand-format tables, alignment or spacing. A failure is a one-line verdict, facts and copyable next commands; the cause chain shows only with `--verbose`.
- Wording is short and sentence case: say what happened or is asked, in operator words (`node`, `site`, `model`) rather than code identifiers, one success line per outcome; review blocks show only what the operator decides on. Explanations belong in `--help` and `docs/usage.md`.
- Result records, the startup protocol, JSON, exit codes and non-terminal stderr keep their plain stable form.
