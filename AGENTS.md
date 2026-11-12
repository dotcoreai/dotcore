# Dotcore agentic instructions

## About Dotcore

Dotcore runs AI on hardware people own. It loads models, fits them to each machine's memory, and
serves them behind one OpenAI-compatible endpoint. A site groups one or more machines; apps such as
Open WebUI run next to the models.

## Repository map

Each area's `AGENTS.md` owns the rules for its code, commands and artifacts. Read it before working
there, and read every relevant one when work crosses areas. A rule lives in one file; others point
to it instead of repeating it. This file owns the repository-wide rules and the glossary.

- `backend/`: Rust server and the `dotc` CLI. [`backend/AGENTS.md`](backend/AGENTS.md)
- `frontend/`: the console, a React app. [`frontend/AGENTS.md`](frontend/AGENTS.md)
- `proto/`: the API contracts between the backend, the console and `dotc`.
  [`proto/AGENTS.md`](proto/AGENTS.md)
- `catalog/`: the models, engines, apps and hardware classes Dotcore can deploy. Use the
  `add-model` skill to add a model and the `add-app` skill to add an app.
- `apps/`: app images. [`apps/AGENTS.md`](apps/AGENTS.md) owns the rules for app and engine
  images and their folder docs.
- `engines/`: inference engine builds. [`engines/AGENTS.md`](engines/AGENTS.md) says where
  engine rules live.
- `nix/`: packaging and builds, including the image derivations. [`nix/AGENTS.md`](nix/AGENTS.md)
- `installer/`: the guided host installer. [`installer/AGENTS.md`](installer/AGENTS.md)
- `dev/`: the development stack, workspace environment, development site, remote hosts and
  cluster. [`dev/AGENTS.md`](dev/AGENTS.md)
- `ci/`: the recipes CI runs (`just ci::check`). The workflows live in `.github/workflows/`;
  [`.github/AGENTS.md`](.github/AGENTS.md) owns their rules.
- `docs/`: the user guides, the development guides and the product and architecture specs.
  [`docs/README.md`](docs/README.md) lists them and [`docs/AGENTS.md`](docs/AGENTS.md) says which
  one holds what.
- `utils/`: standalone tools with no build integration; each has its own README.
- `.agents/skills/`: skills, linked from `.claude/skills/`.

The backend and frontend `AGENTS.md` files start with a code map of their folders. In a code map,
each line gives a folder's path and its responsibility in a few words. List a file only when it
really matters, and add no asides; a pointer to a section of the same file is fine. Update the map
in the same change that adds, removes or renames a folder it lists.

## Nix, just, dotcore and dotc

- Nix builds everything: the Dotcore package, each engine's native package, every app and engine
  Docker image, and the dev shell. Master catalog entries name flake packages, and each node builds
  the ones it runs for its own system. [`nix/AGENTS.md`](nix/AGENTS.md) owns everything about Nix:
  packaging, the flake, image derivations and the master catalog build.
- `just` runs every developer workflow, inside the Nix dev shell that direnv loads. Call recipes by
  their module path (`just dev::site-init`, `just backend::test`), the form shells complete, and
  discover them with `just --list <module>` or `just --list --list-submodules` from the root. Use
  named recipes rather than running workflow files directly.
- `dotcore` is the app's entry point: it runs the server, the console and the models on a machine.
- `dotc` is Dotcore's command-line tool. It sets up sites and machines, manages model and app
  deployments, starts coding agents and runs one-off development workloads; `dotcore` uses it to run
  the server. Use it, and `dotc <command> --help`, to inspect or change a site rather than the
  console or the site's files.

## Safety

- This repository is public. Never commit personal names, real machine or host names, non-local
  IP addresses, credentials, or links to private documents. Examples use made-up names (`gpu1`,
  `example.ts.net`) and local or documentation addresses (`127.0.0.1`, `192.0.2.10`).
- Development tooling may pull public artifacts without separate approval: `docker pull`, `docker
  build`, gated test harnesses, and image recipes such as `just apps::<app>::image-load`,
  `image-lock` and `image-test` may fetch images and packages from public registries. Ask before
  pushing images, logging in to a registry, using private credentials, or changing any external
  service.

## Product and architecture

- The product and architecture specs (`docs/specs/`) hold the product intent and the target
  architecture. They don't describe current implementation status.
- Dotcore has no deployments to stay compatible with. Align decisions with the specs and the
  future design, not with existing code, unless there is a concrete need.
- If a request conflicts with a spec, name the affected decision, propose the spec change, and ask
  for a brief confirmation. Don't refuse, and don't change the decision silently.
- A technical plan selects or changes design, architecture, ownership boundaries, protocols,
  topology, milestones or capability placement. Task lists, runbooks, investigation steps and
  status summaries are not technical plans.
- After writing or materially revising a technical plan, request an architecture review. Before
  implementing it, report its effects on the facts above and the document updates it needs.
- [`docs/specs/AGENTS.md`](docs/specs/AGENTS.md) says when a change updates a spec page.

## Development

- Read [`dev/AGENTS.md`](dev/AGENTS.md) before any development work: running the stack, the
  development site, tests, remote hosts or the cluster.
- Give every repeatable developer workflow a recipe in the folder where developers do it, with a
  one-line doc comment saying what it does for the person running it. Docs name that recipe, not
  the command it runs; a one-off lookup, such as reading an upstream release's digest, can be
  written out as is.
- Automated qualification agents invoke their documented `dotc` and harness commands directly
  instead of human convenience recipes.
- Run the owning area's documented gates before completing work; `just check` runs the
  repository-wide aggregate.
- Each area's `ci-check` recipe runs its gates without changing files, and `just ci::check` runs
  them all, as CI does. An area that adds a `check` recipe adds a `ci-check` and joins
  `ci::check`.

## Platform scope

- For plans and implementations affecting artifacts, packaging, native dependencies, runtime
  integration or tests, cover x86_64 and arm64 Linux and arm64 macOS unless the scope is
  explicitly narrowed.
- On macOS nodes, cover darwin/arm64 native runtime model targets and linux/arm64 Docker Desktop
  application targets.
- A host runs builds for its own architecture only. Build and run there; for the other
  architecture, evaluate and exercise contracts and fixtures. Never claim an unexecuted build
  passed.

## Code

- Add code when something uses it, not for a hypothetical or future use unless asked; delete it
  when that use goes away.
- When a review finding goes against a recorded decision, point to the decision instead of adding
  code around it.
- Organize code top-down for readers: the primary or public API first, then its direct
  dependencies, then supporting types and helpers. Keep related definitions together and don't
  interleave unrelated ones.
- Doc comments describe the concept and its responsibility in domain terms, at the abstraction
  level of the code they document. They don't name details owned elsewhere: CLI commands and
  flags, environment variables, other components' interfaces, literal example values, or exact
  output and error strings. Those go stale silently.
- Inline comments may name such a detail when it is the reason the code is written that way.
- When a change edits a doc comment, bring the whole comment in line with these rules.

## Tests

- Grow the tests and end-to-end checks that already exist before adding a new one. Delete a
  throwaway script or fake once its experiment is done.
- Make timing policies configurable or clock-controlled, so tests use the smallest meaningful
  delays, retry intervals and expiry windows without changing production defaults. Keep one timing
  policy per area, not a knob per timer. Prefer a controlled clock when elapsed time is the
  behavior under test, but keep meaningful real-process timing coverage.
- Keep behavior timing separate from test deadlines: behavior stays fast, deadlines leave room for
  CPU, memory and resource contention. Correctness never depends on scheduling speed.
- Wait on observable conditions, acknowledgments or explicit gates, never fixed sleeps. Prefer
  notifications; otherwise poll with a short delay until a bounded deadline and report the last
  observed state.
- Wait loops observe progress; they don't retry failed tests or repeat mutations. A negative
  assertion first establishes that the action was processed. Keep exact assertions, visible
  failures and bounded cleanup.

## Documentation

- Write every document, README and `AGENTS.md` in plain English: short sentences, bullets and
  outlines over paragraphs, no jargon or internal shorthand.
- Order every document, and a folder's documents, from overview to detail (progressive
  disclosure). Open with what it is and the shortest path to using or understanding it; each
  section then adds one level of detail, so a reader can stop at any point with a correct picture.
  Put the common case before the exception.
- A document is enough to start, not a reference or a changelog. Say what a task is and which
  command or console screen does it, then point to `--help`, `just --list`, the code or another
  page for the rest. Don't list every command, option or limit, and don't copy flag lists,
  defaults, prompts or step-by-step screens; they go stale.
- An `AGENTS.md` keeps the rules agents get wrong and the risky ones; everything else is a pointer
  to `--help`, a recipe or a doc. Title it `# <Area> agentic instructions`.
- A README is for a person browsing: what the folder is, how it fits, where to go next. Procedures,
  rules and checks for changing it go in the folder's `AGENTS.md`, which the README may link.
  `docs/` holds the guides people follow to use and develop Dotcore, and the product and
  architecture specs.
- Docs state current facts once: when a change makes one false (`docs/`, READMEs, agent files),
  rewrite or delete it in the same change; never append dated addenda or migration notes.
- Guides and READMEs tell a reader how to do something. A code change needs a doc change only
  when it changes what the reader does. They list what a task needs, so don't write that a
  command doesn't need something ("works without a configured site").
- Keep run logs, evidence and qualification results out of READMEs and docs; version control holds
  their history.

## Version control

- Use semantic commit messages: `<type>(<scope>): <description>`
- Types: `feat`, `fix`, `refactor`, `perf`, `style`, `test`, `docs`, `chore`
  - Type a change by the work that carries it: code arriving with its tests, documents, or a
    dependency bump takes the code's type
- Scopes: the top-level area the change lives in: `apps`, `backend`, `catalog`, `docs`, `engines`,
  `frontend`, `installer`, `proto`, `dev`
  - `dev` is for the development stack, workspace environment, and harness (`dev/**`, `justfile`,
    `nix`, etc.)
  - A `docs` change takes the area the document covers
  - Use `*` for repository-wide changes, such as the root `README.md` or the root instructions, for
    example `docs(*): restructure the root README`
- Split work that spans backend, frontend and protos into one change per area. A change may not be
  fully functional on its own (a proto change may not compile without its backend change)

## Glossary

- Add a term here only when it is Dotcore's own and not obvious from its words, or when it gets
  confused with another. Implementation and development vocabulary (data files, test fixtures,
  internal types) stays in the code and its area's `AGENTS.md`.
- Common concepts keep their usual meaning. Reuse these words in code, docs and plans; don't
  invent synonyms. The specs (`docs/specs/`) explain how they relate.
- Write domain nouns in lowercase mid-sentence in every human-readable text: docs, comments, logs,
  errors, test names, CLI output and UI copy (`the console`, `the master catalog`, `site not
  found`). Capitalize only sentence and heading starts, proper names (Dotcore, OpenAI, Docker),
  acronyms (API, OS) and exact code identifiers, which go in backticks (`` `Site` ``).

- **catalog**: the immutable declarations Dotcore can deploy: models, applications, engine
  releases, hardware classes, `UpstreamModelTemplate`s. The **master catalog** is the platform's,
  apart from site or development additions.
- **model-local engine configuration**: one model's launch recipe on one engine (artifact bindings,
  approved engine artifacts, request kinds, sizing, hardware support).
- **model profile**: a preset of defaults for one engine configuration; never holds total context
  or context per request.
- **engine release**: one version of an engine (llama.cpp, vLLM) and the parameters it exposes.
  An **engine artifact** is one runnable build of it.
- **hardware class**: a kind of execution hardware, nothing model-specific. **Hardware support** is
  a configuration's memory minimums and sizing constants for one class.
- **use case**: what an operator runs Dotcore for (chat assistant, coding, image generation); a
  fixed list, distinct from inference request kinds. Each use case has at most one app; models
  serve the use cases that have model lists.
- **model recommendation**: a master-catalog entry naming a model profile to run on a hardware
  class for a use case, in priority order, with optional memory minimums and an optional context
  per request (the profile still holds none); advisory, never used for admission or placement.
- **maximum context**: the largest context one request can use with the model's weights; a
  configuration may lower it. No "effective" variant.
- **total context** (`total-context`): the KV pool an engine holds, shared by active requests.
- **context per request** (`context-per-request`): the largest context one request may use in a
  deployment; at most the maximum and the total context.
- **parallel requests** (`parallel-requests`): requests generated at once; more wait in a queue.
- **sizing scenario**: context values an operator picks to estimate memory; advisory, never used for
  admission or placement.
- **memory pool**: memory a target draws from: one pool per dedicated GPU, and one host pool per
  node, which unified GPUs share.
- **occupancy**: what is in use in a memory pool: measured use per deployment, other use, and
  claims. A **claim** is a starting model deployment's estimate, counted until it reports measured
  use; advisory, never a reservation.
- **demand**: a configuration's estimated bytes per memory pool on one target.
- **placement preview**: the capacity component's answer for one configuration on one target: each
  pool's occupancy with the demand, the fit line and the starting total context.
- **machine description**: hardware as data: platform, host memory, and each GPU's memory and
  architecture. A **declared machine** is the description a node reports instead of its real
  hardware, resolved when the node is created and kept with it.
- **model deployment** / **application deployment**: one deployed model or app, desired and
  observed state. An **upstream model deployment** points at a configured upstream endpoint
  instead of a local workload.
- **inference route**: the `model` name callers use; its members are model deployments.
- **inference request kind**: chat or decision. A **decision request** answers typed questions with
  per-option probabilities from one model pass, not generated text.
- **`SiteNode`**: a node's stable membership in a site; not its health or workloads.
- **execution target**: where workloads run on one node: one platform, its devices, one model or
  application runtime. Its platform is the workloads' (linux/arm64 for Docker Desktop on a Mac),
  not the node's OS.
- **native runtime**: workloads as host processes, each held by a launcher that can outlive server
  restarts.
- **simulated**: a fake realization of one local deployment on a declared machine, for development;
  it uses its estimate of the machine's memory. Not a catalog kind or site mode.
- **Docker image derivation**: a flake package defining one Docker image (a loader, plus an
  optional stored image). A **Docker-delivered Nix artifact** is a catalog artifact whose package
  is one.
