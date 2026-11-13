# Storage

What Dotcore stores, where, and who owns it: artifacts and caches, durable records, node state,
metrics, logs and diagnostics, and backups. The artifact manager, the site store, observability and
the backups manager own them.

Code: `backend/artifact/`, `backend/site/` (site store and node store), `backend/observability/`,
`backend/core/src/{diagnostic,deployment_logs}/` and `backend/core/src/metrics.rs` (logs and metric
transport).

## Capabilities

- See operational health, metrics, logs, traces and freshness, never implying observations that
  don't exist.
- See request, hardware (memory, CPU, GPU use, power, temperature, storage) and per-deployment
  resource metrics per site, machine, device, deployment and route, over a chosen time range,
  starting from a site-wide overview. They share one time axis, with live updates, gaps where
  nothing was observed, and where retained history begins.
- Read workload and backend diagnostics, apart from platform telemetry.
- See artifact provenance, verification and cache use, and keep artifacts available while
  deployments use them.
- Reclaim image disk no deployment uses, and shared workload storage unused for a configurable time.
- See configured and observed protection honestly.

## Architecture

```mermaid
flowchart TB
  CTRL["Owning controllers"]
  API["Management API"]
  subgraph Manager["Site manager process"]
    SS["Site store: owner records"]
    MS["Site-wide metric store"]
  end
  subgraph Node["Each node"]
    AM["Artifact manager: artifacts, caches, Nix roots"]
    NS["Node store: realization state"]
    FW["Metric forwarder"]
    TS["Workload resource records"]
    W["Workloads"]
  end
  SRC["Source components"] -.->|"copies"| BK["Backups manager"]
  CTRL -->|"records, own repository"| SS
  CTRL -.->|"on-demand diagnostic reads"| NS
  AM -->|"verified artifacts"| W
  FW -->|"authenticated node channel"| MS
  MS -->|"queries, live streams"| API
  IC["Inference controller on the hosting node, development tooling"] -->|"telemetry sink"| TS
```

- The artifact manager verifies, stores and transfers artifacts, and keeps their caches and Nix
  roots on each node.
- The site store keeps the owners' records, each owner in its own repository.
- The node store keeps each node's realization state ([runtime](runtime.md)).
- The authenticated node channel is a node's private connection to the site manager, authenticated
  with its site-local node identity ([access](access.md)). It carries the node's metrics.
- Observability collects and retains metrics, logs, traces and diagnostics. The site-wide metric
  store lives with the site manager.
- The backups manager owns backup sets. The components it protects stay authoritative.

### Preparing an artifact

```mermaid
flowchart TB
  AM["Artifact manager on the hosting node"]
  AM --> OCI["OCI: pinned digest"]
  AM --> NIX["Nix: store path for the node's Nix system"]
  AM --> DN["Docker-delivered Nix: build the loader"]
  AM --> DEVI["Node-local development image: inspect"]
  AM --> DEVN["Development-source Nix: evaluate"]
  SRC["Local Dotcore source, or fetch"] -->|"locked revision"| NIX
  OCI -->|"pull"| DOCK["Docker backend's Docker endpoint"]
  DN -->|"load"| DOCK
  OCI & NIX & DN & DEVI & DEVN --> VER["Verify and publish for workloads"]
  AM -->|"preparation output"| NS["Node store, until cleanup"]
```

The artifact manager prepares each artifact on the hosting node, verifies it, and publishes it for
workloads.

- OCI artifacts: it asks Docker to pull the pinned digest for the retained concrete platform, then
  verifies the pinned repository digest and the realized operating system and architecture, with an
  empty variant.
- Nix artifacts: it prepares the store path for the node's own Nix system and verifies the built path
  matches. The flake reference is kept as provenance, not identity. A Nix artifact's package
  provides a native executable or a Docker image.
- A locked revision builds from a local source when one holds it, and is fetched otherwise. The
  store path match verifies it either way.
- Docker-delivered Nix artifacts: it builds the node's loader, runs it against the Docker endpoint
  the Docker backend uses, and verifies the realized image against the identity its image derivation
  records.
- Node-local development images: it resolves an immutable image identity in the node's image store
  and prepares it by inspecting that identity, never pulling.
- Development-source Nix artifacts: it evaluates the store path from the node's own development
  source.
- It publishes verified artifacts of every kind without invalidating content that workloads may
  still use.
- It reports preparation output to its caller and keeps none. The hosting node keeps bounded
  preparation output per deployment in its node store until cleanup is confirmed.

### Recording metrics

```mermaid
flowchart TB
  subgraph Node["Each node"]
    WR["Writers: own measurements"] -->|"typed samples with observation time"| FW["Metric forwarder"]
  end
  FW -->|"authenticated node channel"| MS["Site-wide metric store: epoch-aligned buckets"]
  MS -->|"queries, live streams"| API["Management API"]
```

- Writers own measurement and observability owns retention.
- Every node sends its metrics through a node-local forwarder to the site manager over the
  authenticated node channel. A node writes only metrics it measured about itself.
- Metrics are typed, with stable identity labels, and aggregated into time buckets aligned to the
  epoch, so all sources share timestamps. They're served as queries and live streams through the
  management API.
- A metric sample carries its observation time. Alignment across nodes assumes synchronized node
  wall clocks.
- A gauge reading stays valid only for a bounded time.

### Reading diagnostics

```mermaid
flowchart LR
  CTRL["Owning controller"] -->|"on-demand read"| HN["Hosting node: diagnostics, preparation output"]
  HN -->|"protected values removed"| CTRL
  MD["Management diagnostics"] -->|"on-demand read"| BL["Each node: retained backend logs"]
  BL -->|"paths removed"| MD
  HN -->|"failure diagnostics in status"| DR["Deployment record"]
```

- The owning controller reads a node-hosted deployment's diagnostics on demand from the hosting
  node's local runtime. Preparation output is read the same way.
- Management diagnostics read each node's retained backend logs on demand from that node.
- On-demand reads cross neither the node agent nor observability, and aren't retained.
- Bounded failure diagnostics a hosting node reports in deployment status are kept on the deployment
  record.
- The hosting node removes protected launch values from deployment diagnostics, and each node removes
  filesystem paths from backend diagnostics, before the data leaves the node. Preparation output
  keeps host paths.

## Resources

### Artifact identity

- An immutable remote artifact is identified by its OCI digest or its evaluated Nix store path.
- A Docker-delivered Nix artifact is identified by its loader's store path. Its loader program and
  Docker launch reference derive from that store path alone, so neither the catalog nor a snapshot
  names a loader or holds a Docker reference.
- A Docker image ID is never an artifact identity, except for node-local development images, which
  are identified by image ID.
- Development-source artifacts are identified by their evaluated Nix store path. The flake reference
  and package are provenance.

### Launch identity

- Every app service and engine artifact selection keeps an immutable launch identity before
  realization: an immutable remote artifact or a development artifact.
- The node named by the exact placement pins a node-specific launch identity once: a local
  development image ID, checked against the retained platform, or a Nix store path for its own Nix
  system, evaluated from its development source or chosen from a locked artifact's catalog store
  paths.
- A target whose runtime launches from artifact references alone, such as a simulated one, pins
  without Nix or Docker, and never resolves, builds, loads or roots artifacts. It pins a synthetic
  identity of the same shape, or any of a locked artifact's catalog store paths.
- The owning controller keeps the pinned identity and never resolves it again for the same
  deployment. Development workload tooling pins its transient workload the same way on the node that
  runs it.

### Site store

- The owners' records, kept durable on the site. Each component keeps authority over its own
  records.
- Each deployment's lifecycle generation mark: the highest generation handed out, so a lifecycle
  generation is never reused.

### Node store

- The hosting node's disposable realization state, cleanup evidence, resolved launch identities and
  bounded preparation output, apart from site store records.
- Owners learn of it only through accepted status and on-demand diagnostic reads.

### Nix roots

- Every Nix artifact a node pins keeps a garbage-collector root while the node pins it. The root is
  released once the deployment's cleanup is confirmed. Transient development workloads and synthetic
  launch identities root nothing.
- Roots are derived from the node's pins, can be recreated, aren't records, and need no backup.

### Docker image cache

- Docker images realized from Nix artifacts are a machine-wide cache. Only
  [machine cleanup](#machine-cleanup) releases them.

### Machine cleanup

Operator-triggered cleanup of one machine's disk.

- It releases a Docker image realized from a Nix artifact only when no Nix root on the host keeps its
  loader, and never removes an image a container uses.
- It removes shared workload storage, from any site on the machine, that no container mounts and no
  workload has used for a retention period.

### Workload resource records

- A node-local dataset of the records sent through observability's telemetry sink: resolved launch
  inputs, the hardware class and observed GPU memory, never protected values. The inference
  controller sends them from the hosting node for its ready local deployments, and development
  tooling for its own workload measurements.
- They have no query, export or console surface. They're read only to fit catalog sizing constants
  per hardware class.

### Backups

- A backup is custody of copies, never a transfer of the source's authority.
- Nothing is protected until coverage is supported and configured. Native workload logs, preparation
  output and live process records sit inside the site folder and fall under the open backup coverage
  decision.

## Components

### Artifact manager (artifact role)

The artifact manager also owns catalog declarations ([catalog](catalog.md)).

#### Responsibilities

- Own product artifact verification, storage, transfer, inventory and cache lifecycle.
- Coordinate one writer per publication across backend processes that share an artifact cache.

#### Boundaries

- Sharing cache custody grants no shared site authority.
- A cache's location never defines product inventory or desired state.

### Site store

#### Responsibilities

- Give each owner its own repository for its records.
- Keep records independent of the storage implementation.
- Report a write failure without undoing the accepted in-memory change
  ([sites](sites.md#invariants)).
- Refuse later reads after a write whose outcome is unknown, so it never shows state that may not be
  durable.

#### Boundaries

- Owns no product state, app data, artifact bytes, telemetry or secrets.
- Never becomes an authority that competes with the running site's in-memory desired state
  ([sites](sites.md#invariants)).
- Never holds node realization state. The node store keeps it.
- Site-local durability doesn't allow several backend controllers for one site at once. Shared
  controller authority needs separate state coordination.

### Node store

#### Responsibilities

- Keep each node's realization state on that node ([node store](#node-store)).

#### Boundaries

- Is neither product state nor an authority over the deployments it realizes.

### Observability

#### Responsibilities

- Own collected and retained metrics, logs, traces, diagnostics and alerts.
- Own the site-wide metric store, hosted with the site manager behind a backend-neutral boundary.
- Keep the node-local workload resource records.

#### Boundaries

- Doesn't own node health, deployment status, operation status or routing readiness.
- Direct diagnostic access doesn't transfer ownership of retained logs.
- Access to logs doesn't imply durable retention or complete history.

### Backups manager

#### Responsibilities

- Own backup sets.

#### Boundaries

- Source components stay authoritative for the records it protects.
- Never implies a backup or recovery guarantee without supported, configured coverage.

## Invariants

- Where data is stored, or over which protocol, never changes who owns it.
- Catalog declarations, artifact bytes and caches are distinct from each other, and from app and
  customer data and backups.
- Recording metrics never slows the work measured. Metric history is operational, never usage
  accounting.

## Future direction

- Artifact metadata, immutable revisions, transfers between machines, cache visibility and automatic
  cleanup.
- Customer-configured local, network and object storage, through storage adapters.
- Broader resource and workload telemetry, diagnostics and durable history.

## Open decisions

- Artifact and storage adapters, and registry credentials.
- Cache management: stale staging cleanup, automatic image cleanup, model cache garbage collection,
  OCI and development image cleanup, build cache policy, and the artifacts of abandoned site or node
  folders.
- Supported storage backends and protection enforcement.
- Backup coverage, app data protection, retention, recovery objectives and restore coordination.
- Durable telemetry, audit and batch result retention, and how workload resource records are
  retained and aggregated.
- Health thresholds, metrics export requirements and external time-series backends.
