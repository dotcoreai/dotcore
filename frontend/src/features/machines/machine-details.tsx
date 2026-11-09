import { useEffect, useRef } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { DeploymentListSection, DeploymentRow } from "@/components/deployment-list/deployment-list";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorRow, InspectorRowList } from "@/components/ui/inspector-row";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { Notice } from "@/components/ui/notice";
import { StatusBadge } from "@/components/ui/status-badge";
import { projectApplicationDeploymentStatus } from "@/features/applications/application-projection";
import {
  executionTargetPresentation,
  nodeConnectionStatus,
} from "@/features/deployments/execution-target";
import { PanelMention } from "@/features/deployments/panel-mention";
import { projectNodeMemory } from "@/features/memory/node-memory";
import type { MachineMemory } from "@/features/memory/site-memory";
import type {
  MachineSnapshotDetail,
  MachineSnapshotUpdateFailure,
} from "@/features/memory/use-hardware-snapshots";
import {
  localModelSnapshot,
  modelDeploymentPresentationName,
  projectModelDeploymentStatus,
} from "@/features/models/model-projection";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import type { NodeMemory } from "@/gen/dotcore/capacity/v1/memory_pb";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type {
  HardwareSnapshot,
  HostFacts,
  ObservationUnavailable,
} from "@/gen/dotcore/observation/v1/observation_pb";
import type {
  ConfiguredExecutionTarget,
  NodeObservationFreshness,
} from "@/gen/dotcore/site/v1/service_pb";
import { ExecutionTargetWorkloadKind } from "@/gen/dotcore/site/v1/service_pb";
import { applicationDeploymentPresentationName } from "@/lib/application-deployment-name";
import { formatCount, formatRelativeTime } from "@/lib/format";
import type { ReadState } from "@/lib/read-state";
import type { StatusProjection } from "@/lib/status-projection";
import type { PanelEntry } from "@/panels/panel-stack";

import { MachineMetricSections, PendingMachineResources } from "./machine-metrics";

/**
 * The machine inspector body: a stack of titled sections over the machine's manager-observed
 * connection, its hardware facts, its live resources with their history, and its execution
 * targets. No section repeats the machine's identity, because the panel title already names the
 * machine and its inventory card already shows its ID. Its memory rows draw the node's memory as
 * the capacity component gives it, named after the site's deployments. A loading snapshot renders its loading status at once, so the
 * caller gates it behind delayed loading.
 */
export function MachineDetailContent({
  applicationDeployments,
  connection,
  deployments,
  detail,
  executionTargetFocus,
  executionTargets,
  initialRange,
  loadingStatus = true,
  machineId,
  machineName = machineId,
  metricsClient,
  nodeMemory,
  onSelectExecutionTarget,
  siteManager = false,
}: {
  /** The site's application deployments, counted on the targets they are placed on. */
  applicationDeployments?: ReadState<readonly ApplicationDeployment[]>;
  /** The manager's freshness classification of this machine's observations. */
  connection?: NodeObservationFreshness;
  deployments?: ReadState<readonly ModelDeployment[]>;
  detail: MachineSnapshotDetail;
  /** A target row to focus once rendered, such as the row a target screen was opened from. */
  executionTargetFocus?: ExecutionTargetFocus;
  /** The site's configured execution targets; only this machine's are shown. */
  executionTargets?: ReadState<readonly ConfiguredExecutionTarget[]>;
  /** The range label the resource history charts start at. */
  initialRange?: string;
  /**
   * Whether a loading snapshot states that it loads; off while the empty rows only hold their
   * place, before the caller's delayed loading shows.
   */
  loadingStatus?: boolean;
  machineId: string;
  /** The machine's display name for memory details; defaults to its ID. */
  machineName?: string;
  /** Reads the current readings and history the snapshot does not carry. */
  metricsClient: MetricsClient;
  /** Every site node's memory, of which this machine's draws its memory rows. */
  nodeMemory?: ReadState<readonly NodeMemory[]>;
  /** Opens a target's own screen; without it, target rows are static. */
  onSelectExecutionTarget?: (target: ConfiguredExecutionTarget) => void;
  /** Whether this machine hosts the site manager. */
  siteManager?: boolean;
}) {
  return (
    <div className="inspector-sections">
      {connection ? <ConnectionSection connection={connection} siteManager={siteManager} /> : null}
      {detail.kind === "success" ? (
        <HardwareSnapshotSections
          applications={applicationDeployments}
          deployments={deployments}
          initialRange={initialRange}
          machineId={machineId}
          machineName={machineName}
          metricsClient={metricsClient}
          nodeMemory={nodeMemory}
          snapshot={detail.snapshot}
          updateFailure={detail.updateFailure}
        />
      ) : (
        <>
          <SnapshotStatusSection detail={detail} loadingStatus={loadingStatus} />
          {detail.kind === "loading" ? <PendingMachineResources /> : null}
        </>
      )}
      {executionTargets ? (
        <ExecutionTargetsSection
          applicationDeployments={applicationDeployments}
          deployments={deployments}
          executionTargetFocus={executionTargetFocus}
          executionTargets={executionTargets}
          machineId={machineId}
          onSelect={onSelectExecutionTarget}
        />
      ) : null}
    </div>
  );
}

/** A target row to focus, and the callback that clears the request once the row has focus. */
export interface ExecutionTargetFocus {
  executionTargetId: string;
  onFocused: () => void;
}

/**
 * The execution target inspector body: the target's configured facts, its runtime state only when
 * that runtime is not available, then the model and application deployments whose exact placement
 * names the target. Each deployment row opens that deployment's own inspector.
 */
export function ExecutionTargetContent({
  applicationDeployments,
  deployments,
  executionTargetId,
  executionTargets,
  machineId,
  machineName = machineId,
  onOpenDeployment,
}: {
  applicationDeployments?: ReadState<readonly ApplicationDeployment[]>;
  deployments?: ReadState<readonly ModelDeployment[]>;
  executionTargetId: string;
  /** The site's configured execution targets, read for this target's facts. */
  executionTargets?: ReadState<readonly ConfiguredExecutionTarget[]>;
  machineId: string;
  /** The owning machine's display name; defaults to its ID. */
  machineName?: string;
  onOpenDeployment: (entry: PanelEntry) => void;
}) {
  const target = executionTargets?.value?.find(
    (candidate) => candidate.executionTargetId === executionTargetId,
  );

  return (
    <div className="inspector-sections">
      {target ? (
        <>
          <TargetFactsSection machineId={machineId} machineName={machineName} target={target} />
          <RunningDeploymentsSection
            applicationDeployments={applicationDeployments}
            deployments={deployments}
            onOpenDeployment={onOpenDeployment}
            target={target}
          />
        </>
      ) : executionTargets?.failed ? (
        <section aria-labelledby="execution-target-heading">
          <InspectorSectionHeader headingId="execution-target-heading" title="Target" />
          <DetailList>
            <Detail label="Status" value={<UnreachableBadge />} />
          </DetailList>
        </section>
      ) : null}
    </div>
  );
}

/**
 * The machine's role and its connection as the manager classifies it, with the time of its latest
 * accepted observation report. A stale or disconnected machine keeps that time, so the reader can
 * see how old the retained evidence is.
 */
function ConnectionSection({
  connection,
  siteManager,
}: {
  connection: NodeObservationFreshness;
  siteManager: boolean;
}) {
  const status = nodeConnectionStatus(connection.state);
  const lastReport = connection.receivedAt ? formatRelativeTime(connection.receivedAt) : undefined;

  return (
    <section aria-labelledby="machine-status-heading">
      <InspectorSectionHeader headingId="machine-status-heading" title="Status" />
      <DetailList>
        <Detail label="Role" value={siteManager ? "Site manager" : "Worker"} />
        <Detail label="Connection" value={<StatusBadge status={status} />} />
        {lastReport ? (
          <Detail label="Last report" title={lastReport.title} value={lastReport.text} />
        ) : null}
      </DetailList>
      {status.attention ? (
        <p className="mt-2 break-words text-xs text-muted-foreground">{status.attention}</p>
      ) : null}
    </section>
  );
}

/**
 * This machine's configured execution targets as a row list, model targets first. A row shows the
 * target's kind, platform, configured devices and deployment count, and a badge with the reason
 * only when its runtime is not available. A target list that could not be read states so; one still
 * loading renders nothing yet.
 */
function ExecutionTargetsSection({
  applicationDeployments,
  deployments,
  executionTargetFocus,
  executionTargets,
  machineId,
  onSelect,
}: {
  applicationDeployments?: ReadState<readonly ApplicationDeployment[]>;
  deployments?: ReadState<readonly ModelDeployment[]>;
  executionTargetFocus?: ExecutionTargetFocus;
  executionTargets: ReadState<readonly ConfiguredExecutionTarget[]>;
  machineId: string;
  onSelect?: (target: ConfiguredExecutionTarget) => void;
}) {
  const targets = executionTargets.value
    ?.filter((target) => target.siteNodeId === machineId)
    .sort(compareExecutionTargets);
  if (!targets && !executionTargets.failed) return null;

  return (
    <section aria-labelledby="machine-execution-targets-heading">
      <InspectorSectionHeader
        headingId="machine-execution-targets-heading"
        meta={targets ? formatCount(targets.length, "target") : undefined}
        title="Execution targets"
      />
      {targets ? (
        <InspectorRowList ariaLabel="Execution targets">
          {targets.map((target) => (
            <ExecutionTargetRow
              deploymentCount={
                modelDeploymentsOnTarget(target, deployments?.value ?? []).length +
                applicationDeploymentsOnTarget(target, applicationDeployments?.value ?? []).length
              }
              focus={
                executionTargetFocus?.executionTargetId === target.executionTargetId
                  ? executionTargetFocus.onFocused
                  : undefined
              }
              key={target.executionTargetId}
              onSelect={onSelect ? () => onSelect(target) : undefined}
              target={target}
            />
          ))}
        </InspectorRowList>
      ) : (
        <DetailList>
          <Detail label="Status" value={<UnreachableBadge />} />
        </DetailList>
      )}
    </section>
  );
}

/**
 * One target as a row: its display name, a description of what it runs, and its runtime state when degraded.
 * With `onSelect` the row opens the target's screen; `focus` asks the row to take focus once.
 */
function ExecutionTargetRow({
  deploymentCount,
  focus,
  onSelect,
  target,
}: {
  deploymentCount: number;
  focus?: () => void;
  onSelect?: () => void;
  target: ConfiguredExecutionTarget;
}) {
  const rowRef = useRef<HTMLButtonElement>(null);
  const presentation = executionTargetPresentation(target);
  const runtimeAvailable = isRuntimeAvailable(target);

  // Runs after the inspector's own focus handling, so returning from the target screen lands on
  // the row that opened it rather than the panel heading.
  useEffect(() => {
    if (!focus || !rowRef.current) return;
    rowRef.current.focus();
    focus();
  }, [focus]);

  const description = [
    presentation.workloadLabel,
    target.platformLabel || "Unknown",
    target.acceleratorDeviceIds.length ? presentation.devicesLabel : undefined,
    deploymentCount ? formatCount(deploymentCount, "deployment") : undefined,
  ].filter(Boolean);

  const content = {
    description: (
      <>
        <span className="block truncate">{description.join(" · ")}</span>
        {!runtimeAvailable && presentation.runtimeDetail ? (
          <span className="block break-words">{presentation.runtimeDetail}</span>
        ) : null}
      </>
    ),
    label: target.displayName,
    trailing: runtimeAvailable ? undefined : <RuntimeBadge target={target} />,
  };

  return onSelect ? (
    <InspectorRow
      {...content}
      accessibleLabel={`View execution target ${target.displayName}${
        runtimeAvailable ? "" : `, ${presentation.runtimeLabel}`
      }`}
      onSelect={onSelect}
      ref={rowRef}
    />
  ) : (
    <InspectorRow {...content} />
  );
}

/**
 * The target's configured facts. Its Runtime row and reason appear only when the runtime is not
 * available; machine connection problems stay in the machine's own Status section.
 */
function TargetFactsSection({
  machineId,
  machineName,
  target,
}: {
  machineId: string;
  machineName: string;
  target: ConfiguredExecutionTarget;
}) {
  const presentation = executionTargetPresentation(target);
  const runtimeAvailable = isRuntimeAvailable(target);

  return (
    <section aria-labelledby="execution-target-heading">
      <InspectorSectionHeader headingId="execution-target-heading" title="Target" />
      <DetailList>
        <Detail label="Target ID" value={target.executionTargetId} />
        <Detail
          label="Machine"
          value={<PanelMention colorKey={machineId}>{machineName}</PanelMention>}
        />
        <Detail label="Workload kind" value={presentation.workloadLabel} />
        <Detail label="Platform" value={target.platformLabel || "Unknown"} />
        <Detail label="Configured devices" value={presentation.devicesLabel} />
        {runtimeAvailable ? null : (
          <Detail label="Runtime" value={<RuntimeBadge target={target} />} />
        )}
      </DetailList>
      {!runtimeAvailable && presentation.runtimeDetail ? (
        <p className="mt-2 break-words text-xs text-muted-foreground">
          {presentation.runtimeDetail}
        </p>
      ) : null}
    </section>
  );
}

/**
 * The model and application deployments placed on the target, each with the status badge its card
 * shows. A deployment list whose first read failed makes the section unreachable rather than
 * partial; one still loading renders nothing yet.
 */
function RunningDeploymentsSection({
  applicationDeployments,
  deployments,
  onOpenDeployment,
  target,
}: {
  applicationDeployments?: ReadState<readonly ApplicationDeployment[]>;
  deployments?: ReadState<readonly ModelDeployment[]>;
  onOpenDeployment: (entry: PanelEntry) => void;
  target: ConfiguredExecutionTarget;
}) {
  const reads = [deployments, applicationDeployments];
  const unreachable = reads.some((read) => read?.failed && !read.value);
  if (!unreachable && reads.some((read) => !read?.value)) return null;

  const models = modelDeploymentsOnTarget(target, deployments?.value ?? []);
  const applications = applicationDeploymentsOnTarget(target, applicationDeployments?.value ?? []);

  return (
    <DeploymentListSection
      count={models.length + applications.length}
      emptyText="Nothing is deployed on this target."
      headingId="execution-target-running-heading"
      listLabel={`Running on ${target.displayName}`}
      title="Running"
      unreachable={unreachable}
    >
      {models.map((deployment) => {
        const model = localModelSnapshot(deployment.snapshot);
        return (
          <DeploymentRow
            description={[model?.modelDisplayName, model?.profileId]}
            key={deployment.modelDeploymentId}
            name={modelDeploymentPresentationName(deployment)}
            onSelect={() =>
              onOpenDeployment({
                page: "models",
                resourceId: deployment.modelDeploymentId,
                screen: "deployment",
              })
            }
            status={projectModelDeploymentStatus(deployment)}
          />
        );
      })}
      {applications.map((deployment) => (
        <DeploymentRow
          description={[deployment.snapshot?.displayName, deployment.snapshot?.version]}
          key={deployment.applicationDeploymentId}
          name={applicationDeploymentPresentationName(deployment)}
          onSelect={() =>
            onOpenDeployment({
              page: "apps",
              resourceId: deployment.applicationDeploymentId,
              screen: "deployment",
            })
          }
          status={projectApplicationDeploymentStatus(deployment)}
        />
      ))}
    </DeploymentListSection>
  );
}

/** The target's runtime availability word in warning tone, shown only when it is not available. */
function RuntimeBadge({ target }: { target: ConfiguredExecutionTarget }) {
  return (
    <StatusBadge
      status={{ label: executionTargetPresentation(target).runtimeLabel, tone: "warning" }}
    />
  );
}

function isRuntimeAvailable(target: ConfiguredExecutionTarget): boolean {
  return target.availability?.runtimeAvailability?.availability.case === "available";
}

/** The model deployments whose exact placement names this target. */
function modelDeploymentsOnTarget(
  target: ConfiguredExecutionTarget,
  deployments: readonly ModelDeployment[],
): ModelDeployment[] {
  return deployments.filter((deployment) => isPlacedOnTarget(deployment.intent?.placement, target));
}

/** The application deployments whose exact placement names this target. */
function applicationDeploymentsOnTarget(
  target: ConfiguredExecutionTarget,
  deployments: readonly ApplicationDeployment[],
): ApplicationDeployment[] {
  return deployments.filter((deployment) => isPlacedOnTarget(deployment.placement, target));
}

function isPlacedOnTarget(
  placement: ExactPlacement | undefined,
  target: ConfiguredExecutionTarget,
): boolean {
  return (
    placement?.siteNodeId === target.siteNodeId &&
    placement.executionTargetId === target.executionTargetId
  );
}

/** Model targets before application targets, each by display name. */
function compareExecutionTargets(a: ConfiguredExecutionTarget, b: ConfiguredExecutionTarget) {
  const rank = (target: ConfiguredExecutionTarget) =>
    target.workloadKind === ExecutionTargetWorkloadKind.MODEL ? 0 : 1;

  return rank(a) - rank(b) || a.displayName.localeCompare(b.displayName);
}

/**
 * `Hardware` facts, then the machine's resources, accelerators and storage as metric rows. Host
 * and accelerator collection fail independently, so each section states its own failure rather
 * than dropping it.
 */
function HardwareSnapshotSections({
  applications,
  deployments,
  initialRange,
  machineId,
  machineName,
  metricsClient,
  nodeMemory,
  snapshot,
  updateFailure,
}: {
  applications?: ReadState<readonly ApplicationDeployment[]>;
  deployments?: ReadState<readonly ModelDeployment[]>;
  initialRange?: string;
  machineId: string;
  machineName: string;
  metricsClient: MetricsClient;
  nodeMemory?: ReadState<readonly NodeMemory[]>;
  snapshot: HardwareSnapshot;
  updateFailure?: MachineSnapshotUpdateFailure;
}) {
  const host = snapshot.host?.result;
  const hostFacts = host?.case === "facts" ? host.value : undefined;
  const machine = { machineId, name: machineName };
  const node = nodeMemory?.value?.find((entry) => entry.siteNodeId === machineId);
  // A read that failed before its first value, or that doesn't list the machine, has no memory to
  // draw; one still loading has nothing to say yet.
  const unread = Boolean(nodeMemory && (nodeMemory.failed || nodeMemory.value));
  const memory = node
    ? projectNodeMemory(node, machine, {
        applications: applications?.value,
        deployments: deployments?.value ?? [],
        readFailed: Boolean(nodeMemory?.failed || deployments?.failed || applications?.failed),
      })
    : unread
      ? { ...noMemory, unavailable: [{ deployments: [], machine }] }
      : noMemory;

  return (
    <>
      <section aria-labelledby="machine-hardware-heading">
        <InspectorSectionHeader headingId="machine-hardware-heading" title="Hardware" />
        {hostFacts ? (
          <HostFactList facts={hostFacts} />
        ) : (
          <UnavailableObservation
            unavailable={host?.case === "unavailable" ? host.value : undefined}
          />
        )}
        {updateFailure ? (
          <DetailList>
            <Detail
              label="Update"
              value={<StatusBadge status={{ label: "Failed", tone: "neutral" }} />}
            />
          </DetailList>
        ) : null}
      </section>
      <MachineMetricSections
        client={metricsClient}
        initialRange={initialRange}
        key={machineId}
        machineId={machineId}
        memory={memory}
        snapshot={snapshot}
      />
    </>
  );
}

// A machine without node memory draws no memory rows.
const noMemory: MachineMemory = { deviceSpans: new Map(), spans: [], unavailable: [] };

/**
 * The three states that carry no snapshot — a load in flight, a transport failure, and an
 * observation the site reports as unavailable — each as the `Hardware` section, so no state renders
 * outside the section stack. A load in flight states it unless `loadingStatus` is off. A transport
 * failure has no observation code to state; an unavailable observation does.
 */
function SnapshotStatusSection({
  detail,
  loadingStatus,
}: {
  detail: Exclude<MachineSnapshotDetail, { kind: "success" }>;
  loadingStatus: boolean;
}) {
  return (
    <section aria-labelledby="machine-hardware-heading">
      <InspectorSectionHeader headingId="machine-hardware-heading" title="Hardware" />
      {detail.kind === "loading" && loadingStatus ? (
        <DetailList>
          <Detail
            label="Status"
            value={<span role="status">Loading current hardware snapshot…</span>}
          />
        </DetailList>
      ) : null}
      {detail.kind === "error" ? (
        <>
          <DetailList>
            <Detail
              label="Status"
              value={<StatusBadge status={hardwareSnapshotStatus("error")} />}
            />
          </DetailList>
          <div className="mt-3">
            <Notice role="alert" tone="destructive">
              {detail.message}
            </Notice>
          </div>
        </>
      ) : null}
      {detail.kind === "unavailable" ? (
        <UnavailableObservation unavailable={detail.unavailable} />
      ) : null}
    </section>
  );
}

function HostFactList({ facts }: { facts: HostFacts }) {
  return (
    <DetailList>
      <Detail label="Host name" value={facts.hostName} />
      <Detail label="OS" value={facts.operatingSystemOrKernel} />
      <Detail label="Logical CPUs" value={String(facts.cpuCount)} />
    </DetailList>
  );
}

/** An unavailable observation as a quiet status, with diagnostics where the surrounding UI needs them. */
function UnavailableObservation({
  showDiagnostics = true,
  unavailable,
}: {
  showDiagnostics?: boolean;
  unavailable: ObservationUnavailable | undefined;
}) {
  return (
    <>
      <DetailList>
        <Detail
          label="Status"
          value={<StatusBadge status={hardwareSnapshotStatus("unavailable")} />}
        />
        {showDiagnostics && unavailable?.code ? (
          <Detail label="Code" value={unavailable.code} />
        ) : null}
      </DetailList>
      {showDiagnostics && unavailable?.message ? (
        <div className="mt-3">
          {/* An unavailable observation is a reported state, not a live error, so the notice
           * carries the destructive tone without claiming the alert role. */}
          <Notice tone="destructive">{unavailable.message}</Notice>
        </div>
      ) : null}
    </>
  );
}

/** A list the console could not read, such as the site's execution targets or deployments. */
function UnreachableBadge() {
  return <StatusBadge status={{ label: "Unreachable", tone: "destructive" }} />;
}

/**
 * The status of the console's hardware observation for a machine, which is not a statement about the
 * machine itself: `Unreachable` means the snapshot could not be fetched, `Unavailable` means the
 * site reported that it holds none. This owns both tones, since a snapshot outcome has no state enum
 * behind it.
 */
function hardwareSnapshotStatus(kind: "error" | "unavailable"): StatusProjection {
  return kind === "error"
    ? { label: "Unreachable", tone: "destructive" }
    : { label: "Unavailable", tone: "neutral" };
}
