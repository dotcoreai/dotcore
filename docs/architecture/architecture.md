---
date: 2026-07-29
source: approved architecture and product plans
---

# Dotcore system architecture

## Purpose and context

- Run models and approved applications on customer-owned, on-premises GPU infrastructure.
- Define target system structure and durable design constraints—not an inventory of implemented features.
- Keep the customer site authoritative; cloud connectivity extends management without becoming a dependency for healthy local serving.
- Use the [product specification](../product/product-spec.md) for user outcomes and broad delivery stages; project/phase documents track implementation.

## System overview

```text
Clients           Console / CLIs        Inference clients        App users
                        |                       |                    |
Interfaces        Management API          Inference API      Application access
                        |                       |                    |
Control plane     Site manager          Runtime plane        Application
                  Node agents           Inference router    Identity-aware proxy
                  Identity and access          |                    |
                  Cloud connector        Inference engines     App workloads
                        |                       ^                    ^
                        |                 Runtime backends ----------+
                        |                       ^
                        +-- Management --> Workload controllers
                                           Route manager

Platform services: Artifact manager / Backups manager / Site store
                   Observability / Metering and entitlement manager
```

- **Clients**: Console, CLIs, inference clients/harnesses, and application users; no direct product-state ownership.
- **Ingress interfaces**: Management API, inference API, and application access; separate entry points, not extra components or mandatory network hops.
- **Control plane**: site and node administration, capacity evaluation, identity/access decisions, and controlled remote management.
- **Runtime plane**: workload controllers, route management, serving components, runtime backends, and workloads.
- **Platform services and storage**: artifacts, durability, backups, observability, entitlement, and accounting.
- **Logical boundaries**: independent of packaging, process co-location, and physical adapter placement.

## Components and responsibilities

### Clients and interfaces

- **Console**: graphical management and observation through the management API; also an inference client for trying inference routes, chat and decisions.
- **CLIs**: process lifecycle, approved offline initialization/validation, and owner-controlled management API operations.
- **Inference clients / harnesses**: SDKs, applications, and evaluation tools using the inference API.
- **Apps users**: people accessing hosted applications through application access.
- **Management API**: exposes operations through each resource's owner, and serves owner reads, and the capacity component's node memory projection, as live watches, each change stamped with the aligned interval it belongs to; not a separate product-state owner.
- **Inference API**: serving interface addressed by inference route name: OpenAI-compatible chat APIs and decision APIs (System One, OpenAI Decisions).
- **Application access**: user-facing application entry point, distinct from private workload endpoints.

### Control plane

- **Site manager**
  - Owns site identity, membership, topology, runtime configuration, and the site-wide inventory, health, and maintenance view; the capacity component derives capacity from that inventory.
  - In a clustered site, authoritative site intent and owner-controlled management remain in the process bound to the `site_manager` SiteNode; every SiteNode's realization, including the `site_manager` SiteNode's, consumes only a disposable read-only node-scoped view derived from that authority.
  - Requests node facts and supported administrative actions; does not place or operate workloads.
  - Owns the reconciled node-scoped view of current hardware observations; management APIs and owner admission read that view, not node-local transport.
  - Receives each SiteNode's hardware observations and typed controller status over its node control stream (see Cluster coordination).
  - Hosts observability's ingest of node-local metric buckets; node observations feed only the current-observation view, admission, liveness, and freshness, and metric timestamps never set freshness.
  - Observes all production-supported accelerator vendors on a node independently of configured execution-target intent; observations do not establish execution readiness or allocatable capacity, which the capacity component derives from them as memory pools and occupancy.
  - Reconciles target-attributed platform and accelerator observations with explicit freshness; owner admission consumes this inventory rather than combining direct runtime, node-agent, or console snapshots.
  - Derives SiteNode liveness from the node control stream of the current incarnation, including the `site_manager` SiteNode's: any message is contact, the node sends a heartbeat while idle, and contact lapses after a silence bound; metric bucket streams are never contact.
  - A SiteNode whose node control stream closes is disconnected at once, unless a newer stream of the same incarnation replaced it; a new incarnation is admitted only once the previous incarnation's stream has closed or fallen silent.
  - Judges observation freshness and status freshness separately by their own ages; no node clock sets manager freshness.
  - Accelerator observations state each device's memory architecture: dedicated memory, or unified allocation from the node's host memory pool.
  - On unified nodes, host memory facts, including available memory, describe the shared memory; host memory is never reported as accelerator memory.
  - Host available memory counts memory the operating system can reclaim without displacing application memory, file cache included; used memory is the total minus available.
  - Each dedicated device reports its own total and used memory, driver-reserved memory counted as used; devices of different sizes on one node stay distinct facts.
  - The reconciled view keeps each node's memory facts on their own, for every device and the host, including host-only nodes and nodes without execution targets; it maps each execution target to its configured devices' facts and its node's host memory facts, filtered from that same reading, so consumers judge a target by its own devices, never by another device on the node.
  - Used and available memory are transient facts, never retained in a deployment snapshot.
  - The public node projection marks the `site_manager` SiteNode without giving it a separate connection state.
  - Future scheduling consumes the capacity component's evaluation of site-manager-reconciled inventory with attributable identity and freshness; runtime remains the final workload-realization boundary.
- **Node agent**
  - Consumes site-derived node-local infrastructure configuration without retaining site membership or topology authority.
  - Probes configured execution-target infrastructure through bounded read-only capabilities; availability does not prove workload fit, launch readiness, or realization.
  - Per-node interface for the site manager; supplies hardware, capacity, and health observations and node-management results.
  - Supplies device observations to the site manager; per-process resource observations stay on the reporting node and carry no product workload identity.
  - Observes either its real hardware or a declared machine, never both. A declared machine is a machine description standing in for the hardware, resolved when the node is created (from GPU descriptions on a default simulated host, or from a whole named machine) and kept with the node: the node reports it through the same observations as real readings, so reconciliation and capacity evaluation treat it like a real machine. Real readings, named machines, and tests all describe hardware in this one form.
  - Samples hardware signals and records them through observability's node-local metric forwarder; retains no metric history.
  - Does not inspect or manage workloads, receive workload instructions, or call runtime backends.
- **Capacity component**
  - Owns capacity evaluation: memory pools, occupancy, demands, fit, the largest fitting total context, the starting total context, target ranking, and placement previews. Evaluates data only; owns no product state and retains nothing.
  - Reads each node's memory facts, per-device and host, and the target-to-device mapping from the site manager's reconciled view, and builds pools itself; it does not read node-local transport or runtime backends.
  - Builds one memory pool per dedicated device and one host pool per node, shared by unified devices and host-memory estimates. A target draws from its own devices' pools and its node's host pool.
  - Builds occupancy per pool from measured use, attributed per deployment, model or application, where workload controllers attribute it, the rest counted as other use, plus claims.
  - A local model deployment that is starting claims its resolved estimate until it is ready and measured, less the use measured for it. A claim is advisory: it never blocks another deployment and is not a reservation.
  - Application deployments occupy pools by the accelerator memory attributed to them, in the host pool on unified devices; they carry no estimate and claim nothing.
  - Turns a resolved sizing estimate into a demand: bytes per pool on one target, so a launch across several devices needs no new shape.
  - Judges fit per pool against free memory (total less occupancy) less a headroom, or against the total on request; unknown or stale facts give an unknown verdict and no largest fit, never a false one.
  - Its placement preview is what a view draws for a target: each pool's occupancy with the new demand, the fit line, and the starting total context. Its node memory projection is what site memory views draw for each node: every pool with its occupancy, use past the total, and the deployments it can't size.
  - Evaluation is advisory: admission never blocks a deployment on fit. A future scheduler is its named consumer and uses the same inputs and rules.
  - The inference controller's compatibility read, model recommendations and resource estimates, and the site's memory views, take their verdicts, previews, and pools from it; clients render them and hold no fit, headroom, or starting-value rule.
- **Identity and access manager**
  - Owns users, machine identities, roles, permissions, sessions, API keys, identity-provider configuration, and access policy.
  - Supplies authentication/authorization decisions to enforcing components; not a traffic proxy.
- **Cloud connector**
  - Provides controlled remote-management connectivity between the site and Dotcore services.
  - Does not own product state or override local owners.

### Runtime plane

- Workload controllers remain single logical owners while reconciliation is physically distributed to every SiteNode hosting exact placement, including the `site_manager` SiteNode; each node reconciles only exact placement assigned to it.
- Each workload controller's node-local reconciliation attributes its workloads' accelerator memory from the hosting node's per-process observations, matched by the runtime's node-qualified process identity, or from the runtime's own process-memory report where it supplies one, read from the workload's first running observation; every section of one node status attributes from the same node hardware collection, runtime reports included; memory that cannot be attributed reads as unavailable with its reason, never as a device-wide value or a false zero. Both controllers apply one owner-neutral attribution scope that owns no product state, so the two lanes never attribute the same hardware differently.
- **Apps controller**
  - Owns application deployment intent, placement, lifecycle, reconciliation, readiness, exposure policy, and status.
  - Owns application configuration resolution and endpoint projection, including the site inference base URL and optional inference-route links.
  - Owns typed application parameter resolution: catalog defaults, operator overrides at creation, resolved values retained in the snapshot and projected into service configuration.
  - Retains the selected execution target's accelerator device set in the deployment snapshot at creation; every launch passes that whole set to the group's units. Reconciliation rechecks the retained set against the target's current configured devices before compiling any launch and fails the deployment as incompatible when they differ.
  - Prepares service images only for targets whose runtime backend requires physical materialization.
  - Readiness of a deployment on a virtualized runtime needs no probe: it is ready once its group runs with an assigned endpoint, since nothing it runs serves.
  - Projects deployment accelerator memory attributed by the hosting node, per unit, from that node's current status at read time, never from retained state; a unit without current status or attribution reads as unavailable with its reason, never as a false zero.
- **Inference controller**
  - Owns model deployment intent, source-specific configuration, lifecycle, placement where applicable, reconciliation, readiness, status, and serving eligibility.
  - Owns model option resolution, compatible artifact selection from each model-local approved set, concrete engine state, accepted deployment snapshots, and portable launch plans.
  - Selects the engine artifact and the hardware class by matching their selectors against the target's concrete platform and observed facts; the most specific match wins.
  - Selects only engine artifacts of a kind the target's runtime binding can realize, and only hardware classes the configuration supports.
  - Exposes an advisory compatibility read over catalog model profiles and configured model execution targets, evaluated from the reconciled inventory through the same resolution as admission; a target without fresh observations is unknown. The read may evaluate a sizing scenario (total context and optional context per request) and report resource estimates per hardware class and the capacity component's two advisory memory fits per target, on free memory and on total memory; declared memory minimums always compare with the total. Fits never block a deployment.
  - The compatibility read also reports, per running target, the capacity component's largest fitting total context and starting total context; narrowing by scenario uses the fit.
  - Computes model recommendations for a site as an advisory read: each target takes its most specific class's list per use case, entries are kept or dropped by the capacity component's verdicts, entry minimums included, and the result is one ranking per use case across the site. It never affects admission or placement.
  - Resource estimates for a launch configuration carry the capacity component's placement preview for the selected target.
  - Enforces, at admission, target constraints such as requiring a target's accelerator devices to number exactly the devices the resolved launch uses, so an incompatible target is rejected before acceptance rather than discovered at realization.
  - Uses typed engine adapters provided by the engine integration registry for engine-specific declaration validation, realization, and local engine transport; adapters own no product state.
  - Realizes local workloads through node-local reconciliation on the hosting SiteNode's runtime.
  - Reconciles upstream deployments on the site manager through a separate private verification boundary, outside runtime and any node's realization.
  - Projects deployment accelerator memory attributed by the hosting node from that node's current status at read time, never from retained state; a deployment without current status or attribution reads as unavailable with its reason, never as a false zero.
  - Retains the selected execution target's memory architecture in accepted deployment snapshots; engine adapters derive launch settings and non-blocking advisories from that immutable target fact.
  - Retains the selected execution target's device memory total in accepted deployment snapshots, the smallest of its devices when it has several, or its host memory total on unified targets; engine adapters use it as a memory-fraction denominator, not as reserved capacity, and engines requiring it reject targets without one.
  - Admission rechecks retained memory architecture and the retained hardware class match against current observations.
  - The retained memory total is fixed at creation; deployments pick up changed hardware only when recreated.
  - Evaluates catalog sizing declared for the configuration, the selected hardware class, and the binding at resolution, deriving engine parameter values the caller omitted and advisory estimates; supplied values take precedence.
  - A local request that omits its engine configuration takes the selected profile's default binding; engine parameter values merge per parameter as supplied value, then profile default, then engine release default, then sizing derivation, for every supplied set.
  - Fills an omitted context per request as the smaller of the maximum context and the total context, retained as a derived value.
  - Resolved values are retained in the deployment snapshot.
  - Clients receive derived values and estimates, never catalog sizing declarations.
  - Sizing produces estimates only; the capacity component turns them into demands and verdicts, and neither reserves capacity nor blocks admission.
  - The hosting node emits workload resource telemetry for its ready local deployments, at readiness and when attributed accelerator memory grows; retains none.
- **Engine integration registry**
  - Owns engine-kind-specific launch translation, runtime workload materialization, readiness probes, and inference binding creation.
  - Retains exact bindings by deployment, runtime realization, service, and reconciliation revision only in the process hosting that realization.
  - Is shared by composition across controller readiness and router invocation, independently of runtime backend selection.
- **Route manager**
  - Owns inference route identity, callable names, membership, routing policy, availability, and status.
  - Consumes deployment identity, eligibility, and capabilities; no source-private endpoint, credential, or dispatch data.
- **Inference router**
  - Owns live inference-byte orchestration, cancellation, upstream protocol transport, and transient request selection.
  - Measures request and per-attempt latency, outcome, and token metrics for every public request and records them through observability without delaying serving; these are telemetry, not usage accounting.
  - Applies route-manager policy to inference-controller eligible targets and dispatches node-hosted targets in process or through an authenticated peer endpoint, and upstream targets through upstream transport; owns neither desired state nor readiness.
- **Application identity-aware proxy**
  - Enforces application access policy and forwards allowed traffic to controller-ready workloads.
  - Holds derived serving state, not deployment intent, status, or authoritative readiness.
- **Runtime backends**
  - Accept owner-neutral workload intent, act only on execution infrastructure local to their hosting process, publish a primary endpoint at the host port its owner requests, and report effective execution-target platforms, other infrastructure observations, and assigned endpoints.
  - Report exact realization identity and opaque named service bindings; inference-owned engine integrations resolve those bindings into readiness and invocation capabilities.
  - Validate the destination against retained workload constraints, including platform and artifact kind, before launch without substituting artifacts, platforms, or execution targets.
  - Report opaque node-qualified host-process identity for owned local workloads; identity is ephemeral observation, not product state or telemetry.
  - Do not own product state, select placement, resolve catalog profiles, interpret product readiness, or make reconciliation decisions.
  - Declare whether workload assets require physical host materialization or are virtualized; virtualized backends own realization-specific startup behavior without claiming artifact transfer or artifact cache ownership.
  - Provision node-local storage attachments requested by owner-neutral workload intent, by kind (recreatable cache, or application data) and an owner-resolved namespace and key. Engine releases and applications declare the storage they need in the catalog; the deployment snapshot retains it. Owners name attachments and never receive host paths; owners also mark storage shared beyond one deployment as expiring when unused, and the runtime records when such storage was last used.
  - Advertise support for optional owner-neutral realization capabilities that affect isolation; reject unsupported explicit requirements before workload creation without silent fallback.
  - Preserve isolation-sensitive workload intent in realization identity so a changed isolation requirement cannot reuse an incompatible workload.
  - Keep workload IPC isolated by default. Explicit host-shared IPC is a narrow workload requirement and grants no additional devices, credentials, mounts, network exposure, privileged execution, or product authority.
  - May expose infrastructure diagnostics; do not collect or retain telemetry.
  - Keep realized workloads running across control-plane restarts, unless their runtime binding ties workload lifetime to the hosting control-plane process, and support their rediscovery with ownership evidence; rediscovery fails closed.
  - Simulated runtime is a concrete development implementation of the same realization contract; inference requests and readiness probes do not pass through runtime.
  - Simulated runtime runs model workloads on its node's declared machine: each workload uses its resolved estimate of that machine's memory, so observations, capacity, and fit move as on the real machine. Its targets name the declared machine's devices and pass the same checks as real targets, and it never downloads engine artifacts. A runtime that launches from artifact references alone, as this one does, pins synthetic launch identities and never resolves, builds, loads, or roots its artifacts.
  - The launch contract carries a workload's expected memory per device and in host memory; the simulated runtime uses it, and real runtimes ignore it.
  - Simulation is not a catalog engine kind, routing authority, or product-state owner; local and upstream deployments remain the durable serving-source distinction.
  - Native runtime realizes workloads as host processes on the host platform, each held by a per-workload launcher process.
  - Process composition registers compiled runtime adapter factories independently for model and application workloads, selects configured implementations, and injects completed backend maps; controllers do not select or enumerate implementations.
- **Workloads**
  - Apps workloads: running instances of hosted applications.
  - Inference engines: model servers executing inference, distinct from engine release declarations.

### Platform services and storage

- **Artifact manager**
  - Owns immutable catalog declarations and their lifecycle.
  - Catalog models may carry an intelligence score: display metadata outside the model's release identity and deployment snapshots, with no admission, placement, or fit authority.
  - Catalog models declare a maximum context, a property of the weights; a model-local engine configuration may declare a lower one when its engine can't serve the model's. Resolution enforces the lower of the two at deploy time, and engine limits stay separate engine parameter bounds.
  - Catalog models may declare the upstream licence, with a link to its terms: display metadata outside the model's release identity and deployment snapshots, with no admission, placement, or fit authority.
  - Validates engine-neutral catalog structure, including hardware class references, and invokes the selected typed engine declaration validator without interpreting engine payloads or option effects.
  - Option kinds may fix engine-neutral parameter shapes and cross-option constraints; engine adapters own their effects.
  - Owns the shape of the shared engine parameters (total context, context per request, parallel requests): every engine release declares them, exposed and unsigned, with a default on total context and parallel requests and none on context per request; releases set their own bounds, default values and descriptions, and adapters map them to engine flags.
  - Engine releases may declare defaults on unsigned parameters, within their bounds; a default on a parameter that sizing derives is rejected.
  - Profiles supply no default for total context or context per request.
  - Rejects selectors that overlap without strict nesting, among one configuration's approved engine artifacts, and among its supported hardware classes.
  - Hardware class selectors may name exact accelerator device models, matching a target only when every device it uses reports one; engine artifact selectors don't.
  - Owns model recommendations: per hardware class and use case, a priority-ordered list of model profiles with optional memory minimums. Only the master catalog declares them; site and development catalogs can't add or replace them. Validation keeps each entry runnable on its class and for its use case's request kind, and the classes that declare lists disjoint or nested.
  - Requires engine releases sharing a parameter schema identity to declare identical parameter definitions.
  - Rejects a configuration approving artifacts from releases of different engines or different parameter schemas.
  - Sizing equations must be linear in the total context, so the largest fitting total context follows from the estimates; repository catalog checks enforce it.
  - Rejects model-local engine configurations whose engine requires sizing when a binding lacks a footprint estimate for a supported hardware class or a derivation for an engine parameter that a profile omits and the engine release does not default.
  - Owns product artifact verification, storage, transfer, inventory, and cache lifecycle; for OCI artifacts it asks Docker to pull the pinned digest for the retained concrete platform, then verifies the pinned repository digest and realized operating system, architecture, and empty variant (arm64 `v8` counts as the default arm64 variant).
  - For Nix flake artifacts it prepares, on the hosting node, the store path for that node's own Nix system with `nix build` and verifies the built path matches; the flake reference is retained as provenance, not identity. A Nix artifact's package provides a native executable or a Docker image. `self` resolves only through the hosting node's development source. A node builds a locked revision from a local Dotcore source when one holds it, the source its packaged runtime was built from or its development checkout, and otherwise fetches it; the store path match verifies it either way.
  - For Docker-delivered Nix artifacts it builds the hosting node's loader, runs it against the Docker endpoint the Docker runtime uses, and verifies the realized image against the identity its Docker image derivation records.
  - Keeps every Nix artifact the hosting node pins alive with a garbage-collector root for as long as the node pins it, and releases the root once the deployment's cleanup is confirmed; transient development workloads and synthetic launch identities root nothing.
  - Docker images realized from Nix artifacts are a machine-shared cache, released only when no Nix root on the host keeps their loader, through operator-triggered cleanup that never removes an image a container uses. The same machine cleanup removes shared workload storage, of any site on the machine, that no container mounts and no workload has used for a retention period.
  - For node-local development images it resolves an immutable image identity in the node-local image store and prepares them by inspecting that identity, never pulling.
  - For development-source Nix artifacts it evaluates the store path from the hosting node's own development source.
  - Publishes verified artifacts of every kind without invalidating content that workloads may still consume.
  - Reports preparation output to its caller and retains none.
  - Coordinates one writer per publication across backend processes sharing an artifact cache; sharing cache custody does not grant shared site authority.
- **Site store**
  - Supplies site-local durability through owner-specific repositories; each component retains record authority.
  - Site-local durability does not authorize concurrent backend controllers for one site; shared controller authority requires separate state coordination.
  - Does not own product state, application data, artifact bytes, telemetry, or secret custody.
- **Backups manager**
  - Owns backup-set custody; source components remain authoritative for protected records.
- **Metering and entitlement manager**
  - Owns local entitlement policy and durable usage/request-accounting records.
  - Serving and enforcement consume its decisions without taking ownership of policy or accounting state.
- **Observability**
  - Owns collected and retained metrics, logs, traces, diagnostics, and alerts.
  - Retains a node-local dataset of workload resource records received through its telemetry sink from the inference controller and development tooling; records carry resolved launch inputs, the hardware class, and observed accelerator memory, never protected values.
  - Workload resource records have no query, export, or console surface; development tooling reads them directly to fit catalog sizing constants per hardware class, and their retention and aggregation stay open decisions.
  - Owns a site-wide metric store hosted with the site manager behind a backend-neutral boundary: typed metrics with stable identity labels, aggregated into time buckets aligned to the epoch so all sources share timestamps, served as queries and live metric streams through the management API.
  - Writers own measurement and observability owns retention; recording never blocks the measured work.
  - Every SiteNode sends its metrics through a node-local forwarder to the site manager over the authenticated node channel; a node writes only node-measured metrics about itself.
  - Metric samples carry their observation time; alignment across nodes assumes synchronized node wall clocks.
  - Does not own node health, deployment status, operation status, or routing readiness.

## Resource relationships

Use the [repository glossary](../../AGENTS.md#glossary) for canonical definitions.

### Catalog and deployments

- Site or development overlays may add unique catalog identities, not shadow or mutate master catalog records.
- Model profiles supply defaults, not ongoing deployment authority.
- Model profiles carry no platform; the hardware class follows from exact placement, so one profile serves every hardware class its configuration supports.
- Engine-specific model configuration and resolved state remain typed engine-owned payloads inside engine-neutral catalog and deployment wrappers.
- Application graphs carry portable artifact, configuration, storage, secret, dependency, readiness, and endpoint requirements.
- Application deployments retain stable node placement, one site-owned execution-target identity and concrete platform with its configured accelerator device set, exact immutable service launch identities, the inference base URL composed at creation for the hosting node, a site-unique published host port allocated at creation, and any optional inference-route link. The published port belongs to the deployment, not to its snapshot or portable launch plan.
- Deployment identity is immutable and never reused; display names are mutable and non-unique. Deployment lifecycle generations are likewise never reused.
- Deployment snapshots retain normalized option values, selected immutable artifacts and concrete platforms, the selected hardware class with its resolved selector, concrete engine state and capabilities, and the portable desired launch plan; later catalog edits, removal, or artifact alternatives do not redefine existing deployments.
- Deployment snapshots retain the engine-neutral sizing result when sizing is declared, and which engine parameter values were derived rather than supplied.
- Portable launch plans reference the engine executable, artifacts, and service endpoints symbolically, never as runtime-specific paths, mounts, or ports.
- Controllers validate retained snapshots through the selected engine adapter and reconcile the retained portable plan without catalog lookup or product-configuration translation.
- Runtime performs target-specific materialization of owner-neutral workload intent, including resolution of its symbolic executable, artifact, and service references, not catalog or engine-option interpretation.
- Workloads never depend on their container's DNS for site host names; the hosting node's runtime resolves them and pins them as host entries.

### Routing and execution

- Routes reference deployments; deployments and catalog models are not directly callable through the inference API.
- Upstream deployments retain a template snapshot and private endpoint configuration, with no local model artifact, engine, runtime workload, or exact placement.
- Every local deployment retains exact placement naming one SiteNode and one execution target owned by that node; controllers reject mismatched pairs rather than selecting or correcting placement on the hosting node.
- Each execution target declares one concrete supported platform, the exact accelerator devices its workloads receive, and one typed model or application runtime binding; configured devices name individual devices, and a workload uses the target's whole device set.
- Targets on one node may name overlapping devices, so a node can offer single-device and multi-device targets at once; overlap and shared runtime infrastructure imply no reservation or exclusive resource ownership.
- Each execution target carries a site-owned display name, unique among its node's targets and set when the node is declared; it is presentation only, never an identity, placement, or snapshot input.
- Platform validity depends on the runtime binding: Docker targets declare a Linux platform; native targets declare the host platform, including darwin/arm64.
- Exact-target mappings do not permit silent fallback between runtime backends or substitution of platforms, devices, artifacts, or targets.
- Supported topology changes cannot remove, reparent, or alter the platform, devices, or runtime binding of an execution target while accepted deployment or cleanup intent references it, except when removing its SiteNode withdraws that intent.
- Application service and engine artifact selections retain an immutable launch identity before runtime realization, as an immutable remote artifact or a development artifact.
- Immutable remote artifacts are identified by OCI digest or evaluated Nix store path; a Docker-delivered Nix artifact is identified by its loader's store path, and a Docker image ID is never an artifact identity. Its loader program and Docker launch reference derive from that store path alone, so neither the catalog nor a deployment snapshot names a loader or holds a Docker reference.
- Node-local development images are identified by image ID.
- Development-source artifacts are identified by evaluated Nix store path; the flake reference and package are provenance.
- A node-specific launch identity is pinned once by the SiteNode named by the exact placement: a local-development image ID, checked against the retained platform, or a Nix store path for that node's own Nix system, evaluated from its development source or selected from a locked artifact's catalog store paths. A target whose runtime launches from artifact references alone, such as a simulated one, pins instead without Nix or Docker: a synthetic identity of the same shape, or any of a locked artifact's catalog store paths. The owning controller retains it and never re-resolves it for the same deployment. Private development workload tooling pins its transient workload's identity the same way on the node that runs it.
- Site manager owns site topology; controllers and the route manager retain authority over their own configuration records.
- Server-local node binding selects declared membership; it cannot declare membership or become site desired state.

## Major flows

### Management and reconciliation

```text
Management client --> Owner-controlled API --> Owning component
                                                   |       ^
                                             intent|       |observations
                                                   v       |
                                              Managed reality
```

- Owners validate and accept mutations; clients and remote services do not write records directly.
- Site manager applies node intent through node agents; node administration need not imply continuous reconciliation.
- Workload controllers continuously reconcile local deployment intent through runtime backends and evaluate protocol readiness themselves.
- Upstream deployments reconcile on the site manager through endpoint verification, outside runtime and any node's realization; locality does not turn an upstream endpoint into a local workload.
- Runtime failure changes observations, not desired intent; no adoption of unowned workloads or targets.
- Physical product artifact preparation belongs to the artifact manager, not runtime; runtime owns virtualized realization startup, while controllers retain operation progress, failure, and cancellation ownership.
- Paused intent retains deployment identity, configuration, and storage while suppressing active workload resources and serving eligibility; teardown removes the deployment and deletes the storage scoped to it alone; storage an application shares across its deployments on a node outlives teardown and is removed by machine cleanup once unused for its retention period, or by site reset.
- Private development workload tooling may use runtime only for its separately owned transient workload; it creates no product state or serving eligibility.

### Cluster coordination

```text
Authoritative site -- node-scoped desired view ------------> Node-local reconciliation
Owner controllers  <-- typed status, readiness, observations - Node-local reconciliation
Manager router     -- exact-target dispatch ----------------> Hosting-node serving admission
```

- Site manager distributes complete revisioned node-scoped views derived from one authoritative site snapshot. Distribution, acknowledgement, and fencing are generic control-plane capabilities; typed owner sections preserve controller authority.
- Each SiteNode incarnation holds one long-lived authenticated node control stream to the site manager: observations and typed controller status flow up, view publications and acknowledgements flow down, and the site manager accepts or rejects each message individually under the generic fencing. A newer stream of the same incarnation replaces the older one.
- On every stream open the node re-sends its current observations and status; neither end buffers node state across a reconnect.
- Nodes take readings on epoch-aligned slots, so readings from every node in one slot describe the same time.
- The site manager periodically republishes a node's current view, which keeps that node's action fencing current and serves as the manager's heartbeat; a node reopens its stream when the manager stays silent.
- Each node view contains only the destination node's realization configuration and typed owner intent; unrelated topology, route policy, trust custody, upstream credentials, artifact bytes, and mutable cache state remain outside it.
- Application owner sections carry a linked route's identity and name as apps-controller launch projection, never route policy or live availability.
- Every node atomically installs disposable desired views and reports typed controller status without becoming a product-state owner. Transport acceptance is not workload readiness, and one owner section's progress does not gate another's status.
- An invalid owner assignment is that deployment's typed failure; it does not reject the node view or freeze other owners.
- Node agent remains outside workload instruction and runtime paths. Its observations qualify infrastructure but do not supply desired state or complete runtime construction configuration.
- The `site_manager` SiteNode realizes workloads placed on it with the same node-local reconciliation, status and cleanup evidence, action fencing, diagnostic reads, serving admission, and observations as any other SiteNode; only its control-plane transport differs, using in-process adapters instead of a private loopback RPC, so it needs no private listener.
- Sharing the manager process grants the `site_manager` SiteNode no extra authority: it reads owner intent only through its node-scoped view, and owners accept its status, cleanup evidence, and readiness through the same acceptance boundary as any node.
- Owners never resolve development artifacts or probe application data; the hosting node does both.
- The manager inference router selects from route-eligible deployments before dispatching an exact deployment to its hosting node; the hosting node applies the local binding and owns no route lookup or fallback policy.
- The node control stream, peer inference dispatch, and deployment and backend diagnostic reads are authenticated private interfaces, not public node entry points; no remote caller may present the receiving SiteNode's identity.
- The owning controller reads diagnostics of a node-hosted deployment on demand from the hosting node's local runtime; the data crosses neither node agent nor observability and is not retained.
- The hosting node retains bounded preparation output per deployment in its node store until cleanup is confirmed; the owning controller reads it on demand like workload diagnostics, and it crosses neither node agent nor observability.
- Management diagnostics read each SiteNode's retained backend logs on demand from that node; the data crosses neither node agent nor observability and is not retained.
- Bounded failure diagnostics a hosting node reports in deployment status are retained on the deployment record, unlike on-demand diagnostic reads.
- The hosting node removes protected launch values from deployment diagnostic data, and each SiteNode removes filesystem paths from backend diagnostic data, before it leaves the node.
- Preparation output keeps host paths; only backend diagnostic data has filesystem paths removed.
- Hosting nodes retain cleanup evidence for withdrawn deployments until the manager acknowledges it. The manager accepts cleanup evidence only from the SiteNode named by the deployment's exact placement, and acknowledges only after the teardown's removal is durable. A deployment withdrawn by removing its SiteNode finalizes without cleanup evidence; any remaining infrastructure is cleaned only by the removed node, using exact site and execution-target ownership evidence, never by the manager.

### Inference serving

```text
Engine integration registry -- readiness + exact binding --+
                                                        v
Route manager -------- route policy -------+
                                          v
Inference client --> Inference API --> Inference router --> Selected target
                                          ^                 node (in process or peer) / upstream
Inference controller -- eligible targets -+
```

- Resolve public requests by route name, never catalog model or deployment identity.
- Carry chat and decision requests as inference request kinds through one pipeline: public wire formats decode into one API-agnostic internal contract per kind, which travels through route admission, peer calls, and engine invocation unchanged by wire shape.
- Admit a request only to route members whose model-local engine configuration declares its kind. A kind no member declares is an invalid request; a declared kind with no ready member is unavailable.
- Join route-selected deployment identity to the controller's eligible target; expose source-private dispatch data only after selection.
- Keep route manager and inference controller off the live-byte path.
- Preserve public route identity while isolating source-private model, endpoint, and credential details.
- Retry/fallback safety: no change of target after public output commits, no repeat of an attempted deployment, and one provider request per upstream attempt.
- Invalid-request refusal is never fallback-eligible, wherever it is detected; only availability or identity refusal before dispatch may advance to another target.
- An engine transport may classify an engine rejection it recognizes as a platform invalid request, answered with a platform message; other rejections keep their status, with their message projected through the platform's safe error envelope. An engine that does not serve the kind refuses before any work, which is safe to fall back from.
- Validate platform-owned request semantics at every entry point; bounded engine-specific generation controls travel only to the admitted target and carry no routing, endpoint, credential, or deployment authority.
- Keep dispatch authority and revocation scoped to the selected deployment; changing one target does not revoke another.
- Project each node-hosted runtime realization as a serving target only from fresh hosting-node readiness, dispatched by in-process admission when the node shares the router's process and through an authenticated peer endpoint otherwise; bindings never cross process boundaries, and runtime backend identity does not select router behavior.
- Publish a node-hosted target only from fresh hosting-node readiness evidence for the declared deployment generation, scoped to that deployment; it expires with that evidence, the node's incarnation, or contact lapsing past the node's silence bound, and neither a closed node control stream nor unrelated changes to the node's desired view revoke it, so a stream interruption does not cancel in-flight requests.
- Invoke the hosting node's binding, in process or through its peer endpoint, after route admission; the router does not send inference through runtime.
- Treat binding revocation as terminal.

### Application access and dependencies

```text
App user --> Application access --> Identity-aware proxy --> Ready app workload
                                          ^
                              Access policy + controller readiness
```

- Enforce identity/access decisions at the proxy; do not trust or forward caller-asserted identity.
- Permit optional inference-route links only when the immutable application graph declares the corresponding input.
- Retain stable route references rather than copied callable names; the apps controller composes, at creation, the inference base URL at which the hosting node reaches the manager and the deployment's own published URL, and retains both rather than deriving them later.
- Neither application creation nor launch is gated on route availability or enabled state; a linked route that is Unknown is a typed failure for that deployment.
- Reconcile dependent launch projections when route identity-preserving changes affect them; never substitute another route automatically.
- Retained application route links block route deletion; not every application links a route.

## Cross-cutting constraints

### Authority and durability

- Each mutable product-state kind has one owner; each component writes only its own records.
- The running site's shared in-memory desired state is authoritative; persistence does not become a competing authority.
- Report persistence failures without rolling back already accepted in-memory mutations. Such unpersisted mutations remain authoritative but cannot justify irreversible cross-node effects until persisted.
- Keep desired configuration distinct from observations, progress, and historical records.
- Coordinate operations spanning owners through their acceptance boundaries; deployment creation with its Direct-exposure route, and deployment teardown with its sole-member routes, are all-or-nothing in site authority.
- Keep resource identities portable and independent of runtime-native identities.
- Accepted mutation audit and operation status remain with the accepting owner.
- Offline initialization authors inputs before startup; it does not mutate a running site or bypass deployment owners, except through SiteNode removal.
- Removing a SiteNode is an exclusive administrative action of site authority on a stopped site. It withdraws the deployments placed on that node, and the routes they alone serve, in one revision by applying the owners' teardown rules; owners treat withdrawn deployments as finalized without cleanup evidence.
- Destructive stopped-site recovery is an exclusive administrative boundary, not reconciliation or a console capability; exact ownership evidence is required before infrastructure cleanup. Native runtime evidence is location-scoped: it names the site, SiteNode, and canonical state-root location, so a reset, delete, or node leave removes only workloads recorded under its own state root and never touches another site's state.
- Runtime mutation fails closed when ownership evidence is missing, foreign, malformed, or ambiguous.

### Trust and local autonomy

- Customer-site owners retain authority over configuration, workload intent, access policy, readiness, and locally retained state.
- Site machine trust is established before worker enrollment. Each provisioned SiteNode, including the site manager, holds its own site-issued node identity; the site signing key is separate and held only by site authority.
- A node joins through a handshake hosted by the site manager: the joining node generates its own key and authenticates the site and its manager with a short-lived, enrollment-only join token. Site authority issues the node's identity, then records its membership and pin only after operator approval. Join tokens never authorize runtime traffic. Removing a node withdraws its membership and pin together; a removed node's identity no longer authenticates, even while its certificate is valid under the site CA. A node with an intact folder joins again by proving its pinned identity; only its addresses change.
- Private node connections require a valid site-local node identity, exact peer pins, and directional authorization derived from site authority and trusted bootstrap policy, not from a manager-specific certificate type. This applies even when the site has no workers. Site reset rotates trust, so previously issued worker credentials cannot authenticate with the new manager.
- Site authority records, per SiteNode, the host (an IP address or DNS name) the manager reaches it at and the host it reaches the manager at, each resolved by the node using it, including on behalf of its hosted workloads; peer authentication depends on neither the dialed address nor addresses in certificates.
- A site manager's network entry points (console and inference) listen by default only on its recorded SiteNode host, never on every interface; listening elsewhere takes an explicit operator-named address.
- Healthy local serving continues without Dotcore services, cloud management, or public internet.
- Artifact acquisition may need its source registry; external identity establishment may need its identity provider; upstream inference needs its configured endpoint.
- Upstream locality, trust domain, and data-egress boundaries are explicit; endpoint failure affects that deployment, not healthy local targets.
- Authorization remains local even when an external identity provider establishes identity.
- Hosted workloads receive no platform credentials, site store access, control-plane interfaces, or host-runtime privileges.
- Purpose-scoped workload credentials and permitted workload communication do not confer platform authority.
- Target secret handling uses references, not durable plaintext values; verification and serving receive only the credential capability each requires.
- Secret custody must not transfer product authority or introduce a cloud dependency for local serving.
- Local entitlement decisions and durable usage accounting continue during cloud outages; synchronization and settlement are separate concerns.

### Data boundaries

- Catalog declarations: Artifact-manager-owned immutable metadata, distinct from artifact bytes and caches.
- Product artifacts and caches: Artifact manager custody; cache location does not define product inventory or desired state.
- Workload caches: runtime custody of recreatable workload scratch, keyed per site on each machine by the owner (per deployment or per engine artifact); not product artifacts, inventory, or desired state. Site reset and deletion remove the site's workload caches on the machine they run on; machine cleanup removes shared caches unused for a retention period.
- Native workload state and logs: Runtime custody in `<site dir>/runtime/native`, separate from the node store; ownership evidence names the site, SiteNode, and canonical state-root location, so a copied site directory never adopts or mutates the original's workloads, and native removal never requires the site id; not product state.
- Application/customer content: customer and application authority; apps controller owns storage attachments and their scope (one deployment, or every deployment of the application on a node), not their content.
- Site store: durable owner records, independent of the selected storage implementation.
- Nix roots: Artifact manager custody beside the node store in the site or node folder, derived from the node's pins and recreatable; not records, and need no backup.
- Node store: hosting-node custody of disposable realization state, cleanup evidence, and resolved launch identities, separate from site store owner records; owners learn of it only through accepted status and on-demand diagnostic reads; it also holds bounded preparation logs.
- Telemetry: Observability custody; direct diagnostic access does not transfer retained-log ownership.
- Backups: custody of copies, not transfer of source authority; no protection guarantee before coverage is supported and configured. Native workload logs, preparation logs, and live process records sit inside the site directory and fall under the open backup coverage decision.
- Storage location or protocol does not change logical ownership.

## Deployment shapes

- **Single managed Dotcore OS node**: may co-host site management, node agent, workload controllers, platform services, execution targets, and workloads; optional cloud connector.
- **Clustered customer site**: one non-HA process bound to the `site_manager` SiteNode hosts authoritative site intent and public management/inference entry points; every SiteNode, including the `site_manager` SiteNode, may host execution targets and node-local reconciliation. No parallel `master` or `head` role exists.
- **Customer-managed macOS host**: Apple Silicon Mac running Dotcore without Dotcore OS, as a single node or as the site manager or a worker of a clustered site; model workloads run on the native runtime and applications on Docker Desktop, which Dotcore reaches through its default Unix socket; container workloads there run on a linux/arm64 Docker Desktop target that reports no accelerator, so GPU-class applications run CPU-only on the host.
- **Nix-managed host direction**: Dotcore host software and dependencies move toward Nix management; every Dotcore host requires Nix, which builds the Nix artifacts it hosts, including the loaders of Docker-delivered Nix artifacts.
- **Placement rule**: co-location or distribution does not move logical responsibility; adapter placement remains an implementation/design choice behind the same boundaries.
- **Federated/hybrid horizon**: not an approved deployment commitment; product scope and approval status live in the product specification.

## Open architecture decisions

- Inference access enforcement, entitlement integration, serving-state persistence, remote exposure, and high availability.
- Validated site reload, post-start configuration tooling, and node-binding changes.
- Secret custody, credential lifecycle, and protected diagnostic access.
- Additional runtime adapters; distributed engine profiles, GPU sharing, scheduling, interconnect, and remaining isolation mechanics.
- Artifact/storage adapters, registry credentials, signing, acquisition policy, cache management, stale staging cleanup, automatic image cleanup, model-cache garbage collection, OCI and development image cleanup, build cache policy, and the artifacts of abandoned site or node folders.
- Cache size limits and automatic reclaim of workload storage.
- Whether admission or a scheduler ever enforces the capacity component's fit, and how claims then become reservations.
- Backup coverage, application-data protection, retention, recovery objectives, and restore coordination.
- Application access topology, identity forwarding/federation, provisioning, and role mapping.
- Durable telemetry retention, external time-series backends, metrics export, identity-provider selection, usage synchronization, billing, settlement, and export.
- Cloud connector transport, connectivity policy, disablement, failover, and safe disconnected changes.
- Batch inference ownership, capacity sharing, scheduling, and result retention.
- External-provider and federation authority, delegated identity, cross-site lifecycle, commercial policy, and routing beyond bounded safe fallback.

Agentic workloads, global scheduling, federation settlement, training, and a memory service are outside current architecture scope.
