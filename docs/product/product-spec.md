# Dotcore product specification

## Purpose and users

- **Purpose**: operate private AI on customer-owned infrastructure through a cloud-like management experience.
- **Product promise**: healthy local inference and deployed applications continue operating without cloud-management connectivity.
- **Console**: graphical management and operational visibility.
- **CLI**: local administration and scripted management.
- **APIs**: programmatic management, inference consumption, and application access.
- **Platform administrators**: infrastructure health, workload placement, capacity, and maintenance.
- **Model managers**: model configuration, quality, and release decisions.
- **Developers**: integrate products with served AI and hosted applications.
- **Team managers**: access and resource allocation.
- **Scope**: intended capabilities and broad delivery stages, not a description of every current screen or implemented feature.
- **Architecture**: [system structure and ownership](../architecture/architecture.md); delivery detail and progress belong in project/phase documents.

## Product principles

- **Local autonomy**: customer-site authority and healthy local operation independent of Dotcore cloud availability.
- **Operational clarity**: distinguish desired intent, observed state, in-progress operations, and failure.
- **Honest evidence**: distinguish measured, estimated, representative, stale, unsupported, and unavailable information.
- **Capacity clarity**: distinguish physical, reserved, used, claimed by starting deployments, and available resources; aggregate free capacity is not proof a workload fits.
- **One capacity answer**: every view of memory fit, largest fit, starting context, and memory use shows the same backend evaluation, so they never disagree.
- **Safe actions**: explain impact and constraints; confirm destructive actions and provide actionable failure feedback.
- **Explicit acceptance**: do not present uncertain or cloud-originated changes as accepted at the site before local acceptance.
- **Accessible operation**: keyboard access, visible focus, reduced motion, and narrow-screen support.
- **Consistent identity**: distinguish catalog items, individual deployments, and callable routes; human-readable names do not replace stable identity.

## Capability areas

### Models and inference

- Discover catalog models and their compatible profiles, artifacts, capabilities, and launch requirements.
- Compare catalog models by an intelligence score; the score is advisory, with no admission, placement, or fit authority.
- Distinguish models available for discovery/acquisition from models available for deployment.
- Create independently managed deployments of the same model, with operator-selected configuration and execution target.
- Use profiles as independent presets; allow typed option and engine-parameter overrides when creating a deployment; retain the resolved configuration of an existing deployment when catalog defaults change.
- Support typed engine configuration and expert launch customization without treating an estimate or preset as verified hardware fit.
- Present advisory resource estimates for the resolved configuration when measured evidence is available; estimates do not admit, reserve, or place workloads.
- Show catalog estimates and an advisory memory fit for an operator-chosen scenario (total context and optional context per request) against a location's free memory by default, or its total memory on request; the fit never blocks a deployment.
- Show, per location, the largest total context that fits its free memory, as advice alongside the fit, and start a new deployment's total context from it.
- Judge each location on its own GPUs' memory, so machines with several GPUs of different sizes work; a deployment that is still starting counts its estimate as used.
- Before deploying, preview the memory the deployment would use on its location, beside what already runs there.
- Offer the same context parameters on every engine (total context, context per request, parallel requests) with engine defaults, bounded by the model's maximum context or its configuration's lower maximum; profiles carry no context.
- Derive memory-dependent engine launch values from these parameters and catalog sizing; engines that need these values deploy only when the catalog declares sizing, and derived values are not verified hardware fit.
- Show derived values before deploying and during deployment inspection, distinguished from supplied values.
- Derived launch values still apply under expert arguments; their fit is the operator's risk.
- Inspect deployment identity, configuration, lifecycle, readiness, progress, failures, and available diagnostics.
- Deploy, pause, resume, and tear down individual deployments; pausing retains identity and storage while releasing active workload resources, and removing a deployment deletes the data that belongs to it alone; data an application shares across its deployments on a machine stays until the operator's disk cleanup finds it unused for a configurable number of days, or the site is reset.
- Manage compatible upstream inference endpoints alongside local deployments without inventing local hardware, engine, or placement data for upstream models.
- Keep private provider credentials separate from inference-client credentials and public connection information.

### Inference routes and connections

- Expose inference through stable callable route names rather than catalog model or deployment identities.
- Support direct exposure and hidden deployments; route membership is independent of the local or upstream source.
- Create, inspect, rename, enable/disable, and delete routes with visible membership and availability.
- Support ordered fallback groups and selection among eligible deployments within bounded retry safety.
- Show request, latency, and token metrics over time per route and deployment and across the site; these are operational telemetry, not usage accounting.
- Show why a route or member is unavailable without exposing private provider configuration.
- Provide connection information and integration examples using the public route identity.
- Keep connection guidance distinct from a live inference client or a promise of endpoint reachability.
- Prevent route deletion while retained applications link to it.

### Applications

- Discover approved catalog applications separately from their deployments.
- Deploy configured applications to selected execution targets on managed machines and inspect their retained configuration.
- Applications declare typed deployment parameters, shown as basic or advanced, that operators set when creating a deployment; the resolved values are retained with the deployment.
- Optionally link an inference route to applications that declare such a dependency; route availability does not gate application launch.
- Preserve stable application-to-route links across route renaming; a missing linked route fails the deployment rather than silently substituting another route.
- Inspect application lifecycle, readiness, access information, and available workload diagnostics.
- Pause/resume retained deployments and tear them down through explicit lifecycle operations; teardown remains available while paused.
- Productize protected application access, version selection, updates, rollback, and data-retention/removal policy through the delivery stages below.
- Keep administrator-supplied images distinct from verified catalog applications.

### Infrastructure and capacity

- Inspect configured machines, hardware, accelerator facts, and resource observations where supported.
- Explain that resetting a site rotates its machine trust and requires workers to be reprovisioned before they can reconnect.
- Discover locally present accelerators for production-supported vendors independently of site execution configuration.
- Keep supported-vendor observations explicit when empty or unavailable; distinguish unavailable observations from absent hardware or an unhealthy machine.
- Treat vendor-wide observations as distinct from configured-device attribution, execution readiness, allocatable capacity, and scheduling eligibility.
- Relate model deployments to observed machine/GPU placement and resource use when authoritative attribution exists.
- Provide fleet summaries and machine-level inspection without inferring per-device fit from aggregate capacity.
- Expand toward compatibility checks, placement controls, resource history, diagnostics, maintenance, and automated scheduling as their milestone support is established.

### Identity, access, and governance

- Provide local administration, identity-provider integration, roles, sessions, scoped API keys, and auditability through productization.
- Separate management access, inference consumption, and application access.
- Expose entitlement and usage information with clear provenance and enforcement boundaries.
- Expand toward team capacity, safe model releases, evaluation evidence, and governance as defined by later milestones.

### Artifacts, storage, and operations

- Manage model/application artifact provenance, verification, transfer, cache visibility, and lifecycle.
- Distinguish artifacts and caches from application/customer data and backups.
- Show operational health, supported metrics, logs, traces, and freshness without implying observations that do not exist.
- Show metric history aligned on a common time axis across machines, devices, deployments, and routes, with live updates and gaps where no observation exists; state where retained history begins.
- Give a site-wide overview of inference and machine metrics over a chosen time range, leading to each model deployment, route, and machine.
- Distinguish workload diagnostics from platform telemetry; access to logs does not imply durable retention or complete history.
- Provide updates, rollback, backup/restore, recovery, and controlled remote management as supported product capabilities.
- Report configured and observed protection honestly; do not imply backup or recovery guarantees without support and configuration.

## Broad delivery stages

These stages allocate product outcomes, not current completion status. Capability descriptions above do not imply availability in an earlier stage.

### Milestone 1: Prototype with local serving

- Prove console-to-management-API operation and stable ownership boundaries with limited site-local persistence.
- Provide pre-start local site initialization and validation through the CLI; not Dotcore OS installation or a console workflow.
- Prove local model deployment, reconciliation, lifecycle inspection, pause/resume, and teardown through runtime.
- Provide profile-based configuration, typed engine parameters, expert arguments, advisory accelerator- and host-memory estimates, and launch memory derived from total context and parallel requests, shown against the selected node's measured capacity as advice that never blocks a deployment; show catalog estimates and an advisory memory fit for an operator-chosen scenario against each location's free memory or, on request, its total memory, and the largest total context that fits its free memory; recommend models for the operator's use cases from catalog-curated, priority-ordered lists per hardware class, checked with the same advisory fit and never blocking a deployment, through a guided quick start in the console; show which configured execution targets can run each profile, from the rules deployment itself applies, and narrow the catalog to profiles whose advisory fit allows them there.
- Run development sites and tests on declared machines (named GPU and memory layouts) without their hardware; real hardware only qualifies that each machine's readings match its description.
- Show an intelligence score and the upstream licence, with a link to its terms, per catalog model; the licence is display metadata, not a compliance assessment.
- Support bounded local serving through catalog-selected llama.cpp and vLLM profiles; no customer engine selector or distributed vLLM path.
- Support customer-managed Apple Silicon macOS hosts with bounded native llama.cpp serving, single-node and clustered sites, and Docker Desktop applications; these hosts are not Dotcore OS.
- Add bounded upstream models through one generic OpenAI-compatible template; no provider-specific directory or automatic provider-model discovery.
- Provide source-neutral routes with bounded safe fallback; not unrestricted production routing or federation.
- Offer OpenAI-compatible model discovery, Chat Completions, and limited stateless Responses with text, client-defined function tools, and supported image inputs.
- Offer decisions, per-option probabilities for typed questions from one model pass instead of generated text, through the System One format, with OpenAI Decisions to follow; non-streaming and text-only, served by routes whose models declare them, with upstream models serving chat only.
- Offer a console chat for trying inference routes: conversations with chat routes, and typed questions to decision routes through a form or the raw request. Conversations last for the browser session; decision drafts and saved question sets are kept in browser storage. No assistant tools or console actions.
- Preserve separate reasoning output for supported models; exclude durable Responses storage/continuation, hosted tools, structured outputs, and Conversations from this stage.
- Prove narrow application workloads, with Hermes and n8n as the reference applications, and their declared Inference integration; broader application lifecycle and protected access remain later scope.
- Provide bounded workload and backend diagnostics without implying productized telemetry or remote-access security.
- Provide current per-node hardware and model-deployment accelerator-memory observations with explicit freshness and unavailability.
- Provide bounded in-memory metric history for hardware observations (memory, CPU, GPU utilization, power, temperature, storage) and inference request metrics, per site, machine, device, deployment, and route, with a site overview; history is lost on restart and is not usage accounting.
- Keep representative console/fixture experiences clearly distinct from infrastructure-backed capabilities.
- Limit artifact support to deployment prerequisites: artifacts stay available while deployments use them, and operators can reclaim image disk no deployment uses, and shared workload storage unused for a configurable time; general artifact/storage management is Milestone 2.
- Do not claim production identity, access enforcement, multi-machine scheduling, security, networking, high availability, or recovery beyond limited local reconciliation.

### Milestone 1.5: Single-node vLLM qualification and hardening

- Evaluate engine-local request scheduling, prefix-cache reuse, and concurrent multi-user serving on representative models and hardware.
- Produce qualification evidence for supported local profiles; not a customer engine selector, distributed vLLM path, or broader product commitment.
- Keep distributed vLLM profiles in Milestone 3, subject to measured justification.
- Enforce API keys for inference and management access for the public open-source release; scoped keys, roles, SSO and audit identity remain Milestone 2.
- Offer an Anthropic Messages-compatible endpoint on the same inference routes, bounded like the OpenAI-compatible surface.

### Milestone 2: Installable Dotcore OS

- Productize one managed node; installation and low-level operating procedures are defined separately.
- Add artifact metadata, immutable revisions, verification, transfers, cache visibility, and automatic cleanup.
- Support customer-configured local, shared/network, and object storage through appropriate adapters.
- Add local administrator bootstrap, OIDC/SSO, RBAC, sessions, scoped keys, machine-local secrets, and audit identity.
- Productize application access protection, host/workload isolation, supply-chain policy, and explicit administrator custom images.
- Productize broader single-node resource and workload telemetry, diagnostics, durable history, and advanced GPU compatibility.
- Add signed updates, rollback, backup/restore, recovery, and customer-approved remote-access boundaries.
- Productize multiple approved applications, versions, configuration, updates, rollback, removal, and application-data policy.
- Support GPU-using applications (omni-gen, with ComfyUI as the reference) on Linux NVIDIA: accelerator device access, visibility of the GPU devices a target passes to an application, and measured per-deployment accelerator memory; the app downloads its own checkpoints into application storage that later deployments of the app on that machine reuse.
- Productize and harden supported single-node llama.cpp and vLLM profiles; keep distributed vLLM in its later product stage.
- Support AMD GPUs on Linux through catalog-selected ROCm engine builds, qualified like the other supported profiles.
- Establish local entitlement state and minimum durable usage/request accounting.

### Milestone 3: Multi-machine fleet operations and maturity

- Extend the on-premises control plane across multiple headless Dotcore OS machines.
- Add cross-node inventory, resource history, workload attribution, diagnostics, and placement controls.
- Add compatible placement/moves, capacity checks, coordinated multi-node models, scheduling constraints, and automated placement.
- Introduce distributed vLLM profiles where measured requirements justify operational cost.
- Add drain/maintenance operations, impact previews, and protection against disruptive or over-capacity actions.
- Add team/identity operations and visibility into team capacity and consumption.
- Support safe model releases: evaluation comparison, provenance, canary rollout, explicit promotion, and rollback with retained capacity.
- Mature application version policy, staged updates, and rollback.
- Preserve local serving, reconciliation, telemetry, and durable accounting during cloud outages; identify stale remote views and unavailable controls.
- Add governance audit and reviewable/exportable release evidence.
- Keep the exact set of permitted disconnected administrative changes and specific GPU-sharing modes subject to further design.

### Milestone 4 horizon: Federated and hybrid capacity

- Uncommitted architecture horizon—not an approved roadmap stage.
- Preserve a path to opt-in hosted, peer, externally orchestrated, and third-party inference capacity.
- Keep local capacity the default; distinguish remote ownership, locality, trust, data egress, capabilities, and cost implications.
- Preserve public endpoint continuity without exposing private target addresses.
- Require separate approval for federation protocols, global scheduling, delegated identity, accounting, and commercial policy.

## Non-goals

- Console chat beyond trying a route (persisted conversations, attachments, assistant tools, console actions, run history or route comparison), model training, fine-tuning, and weight editing.
- Physical provisioning/decommissioning, hardware replacement, and GPU firmware tuning through the console; Dotcore OS installation remains a separate experience.
- Adoption or mutation of unmanaged workloads.
- Treating representative UI/fixture data as measured infrastructure support.
- Treating private workload ports as productized application access.
- Defining a third-party application verification process in this specification.
- Uncommitted full-administration workflows such as invitations, team-membership/quota editing, and audit investigations beyond approved milestone scope.

## Uncommitted direction and open decisions

### Capability candidates

- **Data/storage visibility**: inventory, usage categories, access/protection posture, and data relationships; begin with visibility, not implied DLP or arbitrary data management.
- **Capacity/consumption analytics**: attribution, headroom, and expansion what-if analysis; allocation enforcement, procurement, and automatic rebalancing need separate approval.
- **Hardware assessment**: measured model suitability and appliance recommendations; platform lead generation, not a standalone OS sale or automated takeover/purchase workflow.
- **Custom model import**: customer-owned model sources, provenance, integrity, format compatibility, and capacity validation; not training or weight editing.
- **Fleet/governance extensions**: discovery, replacement-hardware migration, tenant isolation, reservations, and hardware capability matrices.
- **Batch inference**: queued/off-peak work, progress, durable results, and capacity sharing with live serving; no scheduling or retention commitment yet.
- **Managed operation**: Dotcore operates a customer's site on their behalf under a service agreement, including monitoring, updates and model releases, through delegated and auditable operator access; on-site hardware work is done by third-party field technician partners. The site stays in charge, and local serving never depends on Dotcore's access.
- Candidate overlap with a milestone requires reconciliation and approval, not independent scope expansion.

### Decisions still required

- Supported model/hardware profiles and the evidence needed to qualify them.
- Roles permitted to perform disruptive operations and later administration workflows.
- Release thresholds, exceptions, and rollback retention.
- Safe disconnected administrative changes and their acceptance boundaries.
- Placement automation, GPU sharing, quotas, reservations, and tenant isolation policy.
- Application signing, data retention, identity integration, and lifecycle policy.
- Supported storage backends, protection enforcement, backup coverage, and recovery objectives.
- Telemetry, audit, and batch-result retention; health thresholds and export requirements.
- Delegated operator access for managed operation: which operations Dotcore staff may perform, how customers grant and revoke access, and how it is audited.
- Approval and milestone placement for candidates not yet on the roadmap.
