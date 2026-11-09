import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import {
  ExecutionTargetWorkloadKind,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import type { StatusProjection } from "@/lib/status-projection";

export interface ExecutionTargetPresentation {
  connectionLabel: string;
  deployable: boolean;
  deploymentGate?: string;
  devicesLabel: string;
  runtimeDetail?: string;
  runtimeLabel: string;
  workloadLabel: string;
}

export function executionTargetPresentation(
  target: ConfiguredExecutionTarget,
): ExecutionTargetPresentation {
  const availability = target.availability;
  const runtime = availability?.runtimeAvailability?.availability;
  const connectionLabel = nodeConnectionLabel(availability?.connectionState);
  const runtimeLabel = runtimeAvailabilityLabel(runtime?.case);
  const runtimeDetail =
    runtime?.case === "unavailable" || runtime?.case === "unsupported"
      ? runtime.value.message || runtime.value.code || undefined
      : undefined;
  const gate = deploymentGate(availability?.connectionState, runtime?.case);

  return {
    connectionLabel,
    deployable: gate === undefined,
    deploymentGate: gate,
    devicesLabel: target.acceleratorDeviceIds.length
      ? target.acceleratorDeviceIds.join(", ")
      : "None configured",
    runtimeDetail,
    runtimeLabel,
    workloadLabel: workloadKindLabel(target.workloadKind),
  };
}

/** The pair of IDs that identifies an execution target across the site. */
export interface ExecutionTargetIdentity {
  executionTargetId: string;
  siteNodeId: string;
}

/** Identifies a target across the site: its execution target ID is only unique within its node. */
export function executionTargetKey(target: ExecutionTargetIdentity): string {
  return `${target.siteNodeId}\u0000${target.executionTargetId}`;
}

/**
 * Names a target for people: its SiteNode's display name (the SiteNode ID when none is configured),
 * then the target's display name (its execution target ID when none is reported). A target's
 * display name is unique within its node, so the pair tells every target of the site apart.
 */
export function executionTargetName(target: ConfiguredExecutionTarget): string {
  return joinTargetName(
    target.siteNodeDisplayName || target.siteNodeId,
    target.displayName || target.executionTargetId,
  );
}

/**
 * Names the target a placement points at, from the configured target it names. A target missing
 * from `targets`, such as one since removed, is named by `nodeName` and its execution target ID.
 */
export function placedExecutionTargetName(
  placement: ExecutionTargetIdentity,
  targets: readonly ConfiguredExecutionTarget[],
  nodeName = placement.siteNodeId,
): string {
  const key = executionTargetKey(placement);
  const target = targets.find((candidate) => executionTargetKey(candidate) === key);

  return target
    ? executionTargetName(target)
    : joinTargetName(nodeName, placement.executionTargetId);
}

export function configuredExecutionTargetOptionLabel(target: ConfiguredExecutionTarget): string {
  const presentation = executionTargetPresentation(target);
  const identity = executionTargetName(target);
  if (presentation.deployable) return identity;

  // The reason follows the identity, so it survives when a narrow control truncates the label.
  return [
    identity,
    presentation.deploymentGate,
    target.platformLabel || "Unknown platform",
    presentation.devicesLabel,
    presentation.workloadLabel,
    presentation.connectionLabel,
    `Runtime ${presentation.runtimeLabel.toLocaleLowerCase()}`,
  ]
    .filter(Boolean)
    .join(" — ");
}

/**
 * Picks the target a deploy dialog preselects. A target on the site manager's machine wins when it
 * is deployable, so a quick deploy that never shows the selector stays on the manager.
 */
export function defaultExecutionTarget(
  targets: ConfiguredExecutionTarget[],
): ConfiguredExecutionTarget | undefined {
  const deployable = targets.filter((target) => executionTargetPresentation(target).deployable);

  return deployable.find((target) => target.siteManager) ?? deployable[0];
}

/** The single word naming a SiteNode's manager-observed connection state. */
export function nodeConnectionLabel(state: NodeObservationState | undefined): string {
  switch (state) {
    case NodeObservationState.MISSING:
      return "Missing";
    case NodeObservationState.CONNECTED:
      return "Connected";
    case NodeObservationState.STALE:
      return "Stale";
    case NodeObservationState.DISCONNECTED:
      return "Disconnected";
    default:
      return "Unknown";
  }
}

/**
 * The tone of a machine's connection: a live machine with current facts is healthy, a live machine
 * whose observation is overdue is a warning, a machine out of contact is an error, and a machine
 * that never reported is neutral. The two degraded states explain which signal lapsed.
 */
export function nodeConnectionStatus(state: NodeObservationState | undefined): StatusProjection {
  const label = nodeConnectionLabel(state);
  switch (state) {
    case NodeObservationState.CONNECTED:
      return { label, tone: "success" };
    case NodeObservationState.STALE:
      return {
        attention: "Observation overdue: the machine is in contact, but its latest report is late.",
        label,
        tone: "warning",
      };
    case NodeObservationState.DISCONNECTED:
      return {
        attention: "No recent contact from the machine; its last report is retained.",
        label,
        tone: "destructive",
      };
    default:
      return { label, tone: "neutral" };
  }
}

// The node comes first, so a width-capped label keeps the part that tells nodes apart. A slash, not
// a middle dot, separates the two, since target display names can contain a middle dot.
function joinTargetName(nodeName: string, targetName: string): string {
  return `${nodeName} / ${targetName}`;
}

function deploymentGate(
  connectionState: NodeObservationState | undefined,
  runtimeCase: "available" | "unavailable" | "unsupported" | undefined,
): string | undefined {
  if (connectionState === undefined || connectionState === NodeObservationState.UNSPECIFIED)
    return "Target availability has not been reported.";
  if (connectionState === NodeObservationState.MISSING) return "Machine has not connected.";
  if (connectionState === NodeObservationState.STALE) return "Machine observation is overdue.";
  if (connectionState === NodeObservationState.DISCONNECTED) return "Machine is disconnected.";
  if (runtimeCase === "unsupported") return "Runtime observation is unsupported.";
  if (runtimeCase !== "available") return "Runtime is unavailable.";
  return undefined;
}

function runtimeAvailabilityLabel(
  availability: "available" | "unavailable" | "unsupported" | undefined,
): string {
  switch (availability) {
    case "available":
      return "Available";
    case "unavailable":
      return "Unavailable";
    case "unsupported":
      return "Unsupported";
    default:
      return "Unknown";
  }
}

function workloadKindLabel(kind: ExecutionTargetWorkloadKind): string {
  switch (kind) {
    case ExecutionTargetWorkloadKind.MODEL:
      return "Model";
    case ExecutionTargetWorkloadKind.APPLICATION:
      return "Application";
    default:
      return "Unknown";
  }
}
