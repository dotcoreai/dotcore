# Sites

A site is one Dotcore installation: one or more machines, each a `SiteNode`, managed together. The
site manager holds the site's authority, and a node agent on each machine reports its hardware.

Code: `backend/site/` (site authority, stores, desired views and fencing, membership), the node
connection in `backend/core/src/server/`.

## Capabilities

- Create a site on one machine, then add and remove machines.
- See each node's membership, health, connection and maintenance state, and which node runs the site
  manager.
- Keep serving on every node while the site has no connection to Dotcore's cloud.
- Reset a stopped site, then provision its workers again ([access](access.md#connecting-nodes)).

## Architecture

```mermaid
flowchart TB
  subgraph Manager["Site manager process"]
    AUTH["Site authority: owner intent"]
    INV["Reconciled inventory"]
  end
  subgraph Node["Each SiteNode"]
    REC["Node-local reconciliation"]
    NA["Node agent"]
  end
  AUTH -->|"node view: revisioned, fenced"| REC
  REC -->|"typed status, readiness, cleanup evidence"| AUTH
  NA -->|"hardware observations"| INV
```

- The site manager owns site identity, membership, topology and the site-wide inventory.
- Each node runs node-local reconciliation for the deployments placed on it, on behalf of their
  owning controllers.
- The node agent reports the node's hardware and runs node administration. It never touches
  workloads.

### Distributing node views

The site manager derives one complete, revisioned node view per node from a single authoritative
site snapshot. Each node installs its view and reconciles toward it.

- Distribution, acknowledgement and fencing are generic control-plane capabilities. Typed owner
  sections inside a view keep each controller's authority.
- A node view holds only the destination node's realization configuration and typed owner intent.
  Unrelated topology, route policy, trust custody, upstream credentials, artifact bytes and mutable
  cache state stay out of it.
- An app's owner section carries a linked route's identity and name as part of its launch, never
  route policy or live availability.
- A node installs each view atomically and reports typed controller status without becoming a
  product-state owner.
- Transport acceptance isn't workload readiness. One owner section's progress never gates another's
  status.
- An invalid owner assignment fails only that deployment. It doesn't reject the node view or freeze
  other owners.
- Owners never resolve development artifacts or probe app data. The hosting node does both.
- The node view is disposable and read-only. Every node, the `site_manager` node included, realizes
  workloads only from it.

### The node control stream

Each node incarnation holds one long-lived authenticated stream to the site manager. Observations
and typed controller status flow up. View publications and acknowledgements flow down.

- The site manager accepts or rejects each message on its own, under the generic fencing.
- A newer stream of the same incarnation replaces the older one.
- On every stream open, the node sends its current observations and status again. Neither end
  buffers node state across a reconnect.
- Any message counts as contact. An idle node sends a heartbeat, and contact lapses after a silence
  bound. Metric bucket streams never count as contact.
- When a stream closes, its node is disconnected at once, unless a newer stream of the same
  incarnation replaced it.
- A new incarnation is admitted only after the previous incarnation's stream has closed or fallen
  silent.
- The site manager republishes each node's current view periodically. That keeps the node's action
  fencing current and serves as the manager's heartbeat. A node reopens its stream when the manager
  stays silent.
- Nodes take readings on epoch-aligned slots, so readings from every node in one slot describe the
  same moment.

### Cleaning up a withdrawn deployment

- A hosting node keeps cleanup evidence for each withdrawn deployment until the manager acknowledges
  it.
- The manager accepts cleanup evidence only from the node named by the deployment's exact placement,
  and acknowledges it only after the teardown's removal is durable.
- A deployment withdrawn by removing its node finalizes without cleanup evidence. Only the removed
  node can clean up what remains, using exact site and execution-target ownership evidence. The
  manager never does.

## Resources

### Site configuration

The site's declared machines, their execution targets, and its settings, authored before the server
starts.

- Offline initialization writes these inputs. It never changes a running site or bypasses deployment
  owners, except through node removal.

### Server-local node binding

What tells one server process which declared node it is.

- It selects declared membership. It can't declare membership or become site desired state.

### SiteNode

A node's stable membership in the site, not its health or workloads.

- The public node projection marks the `site_manager` node without giving it a separate connection
  state.
- Removing a node withdraws its membership and its identity pin together ([access](access.md)).

## Components

### Site manager

#### Responsibilities

- Own site identity, membership, topology and runtime configuration, and the site-wide inventory,
  health and maintenance view.
- Hold authoritative site intent and owner-controlled management in the process bound to the
  `site_manager` node.
- Request node facts and supported administrative actions from node agents.
- Own the reconciled, node-scoped view of current hardware observations ([machines](machines.md)).
- Receive each node's hardware observations and typed controller status over its node control
  stream.
- Derive each node's liveness from its current incarnation's stream, the `site_manager` node's own
  included.
- Judge observation freshness and status freshness separately, each by its own age.
- Host observability's ingest of node metric buckets ([storage](storage.md)).

#### Boundaries

- Doesn't place or operate workloads. Controllers and their node-local reconciliation do.
- Controllers and the route manager keep authority over their own configuration records.
- Node observations feed only the current-observation view, admission, liveness and freshness.
  Metric timestamps never set freshness.

#### Interfaces

- Receive: node observations and typed controller status, over the node control stream.
- Provide: node views, view republication and acknowledgements.
- Provide: the reconciled inventory, read by the management API, owner admission and the capacity
  component, instead of node-local transport.

### Node agent

#### Responsibilities

- Consume the node-local infrastructure configuration derived from the site.
- Probe the configured execution-target infrastructure through bounded, read-only capabilities.
- Supply hardware, capacity and health observations and the results of node management to the site
  manager.
- Sample hardware signals and record them through observability's node-local metric forwarder.

#### Boundaries

- Keeps no site membership or topology authority.
- Infrastructure being available doesn't prove that a workload fits, is ready to launch, or is
  realized.
- Never inspects or manages workloads, receives workload instructions, or calls runtime backends.
  Its observations qualify infrastructure but never supply desired state or runtime configuration.
- Keeps no metric history.
- Per-process resource observations stay on the reporting node and carry no product workload
  identity.

### The `site_manager` node

The node that hosts the manager process runs workloads like every other node.

#### Boundaries

- It uses the same node-local reconciliation, status and cleanup evidence, action fencing,
  diagnostic reads, serving admission and observations as any node. Only its transport differs: it
  uses in-process adapters, so it needs no private listener.
- Sharing the manager's process grants it no extra authority. It reads owner intent only through its
  node view, and owners accept its status, cleanup evidence and readiness through the same boundary
  as any node.

## Invariants

- Each kind of mutable product state has one owner, and each component writes only its own records.
- The running site's shared in-memory desired state is the authority. Persistence never becomes a
  competing authority.
- A persistence failure is reported without rolling back the accepted in-memory change. Such a
  change stays authoritative but can't justify irreversible effects on other nodes until it's
  persisted.
- Desired configuration stays apart from observations, progress and history.
- Operations spanning owners go through each owner's acceptance boundary. Creating a deployment with
  its direct-exposure route, and tearing down a deployment with the routes only it serves, are each
  all-or-nothing in site authority.
- Resource identities are portable and independent of runtime-native identities.
- The accepting owner keeps the audit of accepted changes and the status of operations.
- Removing a node is an exclusive administrative action on a stopped site. In one revision, it
  withdraws the deployments placed on that node and the routes they alone serve, using the owners'
  teardown rules. Owners treat those deployments as finalized without cleanup evidence.
- Destructive recovery of a stopped site is an exclusive administrative boundary, not reconciliation
  or a console capability. It needs exact ownership evidence before it cleans up infrastructure.
- No node clock sets the manager's freshness.
- A capability observation carries the time its source read it, on the source's own clock, and
  freshness is judged against that time, not the reader's clock.

## Future direction

- Drain and maintenance operations with impact previews, and protection against disruptive or
  over-capacity actions.
- Multi-machine fleet operations: cross-node inventory and workload attribution.
- Signed updates, rollback, backup and restore, and recovery.
- During a cloud outage, keep local serving, reconciliation, telemetry and durable accounting, and
  mark remote views as stale and remote controls as unavailable.

## Open decisions

- How the site reloads validated configuration after start, the tooling for it, and changing a
  node's binding.
- High availability for the site manager.
