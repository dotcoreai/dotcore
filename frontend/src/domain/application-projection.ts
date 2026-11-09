import {
  type ApplicationDeployment,
  ApplicationDeploymentDesiredState,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import type { ConfiguredExecutionTarget, Machine } from "@/gen/dotcore/site/v1/service_pb";
import type { LifecycleTone, StatusProjection } from "@/lib/status-projection";

export interface ApplicationSnapshot {
  catalogApplications: Application[];
  deployments: ApplicationDeployment[];
  executionTargets: ConfiguredExecutionTarget[];
  machines: Machine[];
}

const transitioningStates = new Set([
  ApplicationLifecycleState.QUEUED,
  ApplicationLifecycleState.PREPARING,
  ApplicationLifecycleState.STARTING,
  ApplicationLifecycleState.STOPPING,
  ApplicationLifecycleState.PAUSING,
]);

const inProgressStates = new Set([
  ApplicationLifecycleState.PREPARING,
  ApplicationLifecycleState.STARTING,
]);

const staleMachineAttention = "Machine stale";

const pausedLifecycleStates = new Set([
  ApplicationLifecycleState.PAUSING,
  ApplicationLifecycleState.PAUSED,
  ApplicationLifecycleState.UNAVAILABLE,
]);

export function applicationLifecycleLabel(state: ApplicationLifecycleState): string {
  switch (state) {
    case ApplicationLifecycleState.QUEUED:
      return "Queued";
    case ApplicationLifecycleState.PREPARING:
      return "Preparing";
    case ApplicationLifecycleState.STARTING:
      return "Starting";
    case ApplicationLifecycleState.RUNNING:
      return "Running";
    case ApplicationLifecycleState.READY:
      return "Ready";
    case ApplicationLifecycleState.STOPPING:
      return "Stopping";
    case ApplicationLifecycleState.PAUSING:
      return "Pausing";
    case ApplicationLifecycleState.PAUSED:
      return "Paused";
    case ApplicationLifecycleState.STOPPED:
      return "Stopped";
    case ApplicationLifecycleState.FAILED:
      return "Failed";
    case ApplicationLifecycleState.UNAVAILABLE:
      return "Unavailable";
    default:
      return "Unknown";
  }
}

export function applicationReadinessLabel(state: ApplicationReadinessState): string {
  switch (state) {
    case ApplicationReadinessState.READY:
      return "Ready";
    case ApplicationReadinessState.NOT_READY:
      return "Not ready";
    default:
      return "Unknown";
  }
}

/**
 * The deployment target an application deployment is being driven toward. It names the target
 * itself, so it stays distinguishable from the lifecycle state reported alongside it: a deployment
 * whose target is `Teardown` reports `Tearing down` while the controller works through it.
 */
export function applicationDeploymentDesiredStateLabel(
  state: ApplicationDeploymentDesiredState,
): string {
  switch (state) {
    case ApplicationDeploymentDesiredState.PRESENT:
      return "Present";
    case ApplicationDeploymentDesiredState.PAUSED:
      return "Paused";
    case ApplicationDeploymentDesiredState.TEARDOWN:
      return "Teardown";
    default:
      return "Unspecified";
  }
}

export function applicationDeploymentLifecycleState(
  deployment: ApplicationDeployment,
): ApplicationLifecycleState {
  const lifecycleState = deployment.state?.lifecycleState ?? ApplicationLifecycleState.UNSPECIFIED;
  if (
    deployment.desiredState === ApplicationDeploymentDesiredState.PAUSED &&
    !pausedLifecycleStates.has(lifecycleState)
  ) {
    return ApplicationLifecycleState.PAUSING;
  }

  return lifecycleState;
}

export function applicationDeploymentReadinessState(
  deployment: ApplicationDeployment,
): ApplicationReadinessState {
  if (deployment.desiredState === ApplicationDeploymentDesiredState.PAUSED) {
    return ApplicationReadinessState.UNSPECIFIED;
  }

  return deployment.state?.readinessState ?? ApplicationReadinessState.UNSPECIFIED;
}

export function applicationDeploymentEndpoint(
  deployment: ApplicationDeployment,
): string | undefined {
  if (deployment.desiredState === ApplicationDeploymentDesiredState.PAUSED) {
    return undefined;
  }

  return deployment.state?.endpoint?.url || undefined;
}

/** The configuration bindings through which an application accepts an optional inference route link. */
export function inferenceRouteBindings(application: Pick<Application, "graph">) {
  return (
    application.graph?.services.flatMap((service) =>
      service.configurationBindings.filter(
        (binding) => binding.input?.kind.case === "inferenceRoute",
      ),
    ) ?? []
  );
}

export function isTransitioningApplicationState(state: ApplicationLifecycleState): boolean {
  return transitioningStates.has(state);
}

/**
 * The tone one application lifecycle state carries. It is the only place that decision is made, so a
 * state reads the same colour on a card, which shows an aggregate, and in an inspector, which shows
 * this state on its own row.
 *
 * Every named state is listed rather than folded into the default, so adding one to the proto forces
 * a colour decision here. The default covers only the unspecified value and a value from a newer
 * backend than this build knows.
 */
export function applicationLifecycleTone(state: ApplicationLifecycleState): LifecycleTone {
  switch (state) {
    case ApplicationLifecycleState.QUEUED:
    case ApplicationLifecycleState.PREPARING:
    case ApplicationLifecycleState.STARTING:
    case ApplicationLifecycleState.STOPPING:
    case ApplicationLifecycleState.PAUSING:
      return "info";
    case ApplicationLifecycleState.READY:
      return "success";
    // Unavailable is a fault, not an intentional pause, matching the model lifecycle.
    case ApplicationLifecycleState.FAILED:
    case ApplicationLifecycleState.UNAVAILABLE:
      return "destructive";
    case ApplicationLifecycleState.RUNNING:
    case ApplicationLifecycleState.STOPPED:
    case ApplicationLifecycleState.PAUSED:
      return "neutral";
    default:
      return "neutral";
  }
}

/**
 * The tone one readiness state carries. A deployment the controller reports as up but not serving is
 * a fault the operator should see, so it is toned as one wherever it appears.
 */
export function applicationReadinessTone(state: ApplicationReadinessState): LifecycleTone {
  switch (state) {
    case ApplicationReadinessState.READY:
      return "success";
    case ApplicationReadinessState.NOT_READY:
      return "destructive";
    default:
      return "neutral";
  }
}

/**
 * The tone one deployment target carries. A target is an intent rather than an observation, so only
 * teardown is toned at all and every other target reads as a plain fact.
 */
export function applicationDeploymentDesiredStateTone(
  state: ApplicationDeploymentDesiredState,
): LifecycleTone {
  return state === ApplicationDeploymentDesiredState.TEARDOWN ? "info" : "neutral";
}

export function isPausableApplication(deployment: ApplicationDeployment): boolean {
  return deployment.desiredState === ApplicationDeploymentDesiredState.PRESENT;
}

export function isResumableApplication(deployment: ApplicationDeployment): boolean {
  return deployment.desiredState === ApplicationDeploymentDesiredState.PAUSED;
}

export function isRemovableApplication(deployment: ApplicationDeployment): boolean {
  return (
    deployment.desiredState === ApplicationDeploymentDesiredState.PRESENT ||
    deployment.desiredState === ApplicationDeploymentDesiredState.PAUSED
  );
}

/**
 * Whether an inspector shows the deployment target next to its status. The status word already
 * names a pause or teardown target, so the target only adds information when the deployment is
 * meant to be present but is stopped, stopping, paused or pausing.
 */
export function isApplicationDesiredStateVisible(deployment: ApplicationDeployment): boolean {
  const lifecycleState = applicationDeploymentLifecycleState(deployment);

  return (
    deployment.desiredState === ApplicationDeploymentDesiredState.PRESENT &&
    (lifecycleState === ApplicationLifecycleState.PAUSED ||
      lifecycleState === ApplicationLifecycleState.PAUSING ||
      lifecycleState === ApplicationLifecycleState.STOPPED ||
      lifecycleState === ApplicationLifecycleState.STOPPING)
  );
}

/**
 * The status an application deployment card and inspector render: one word, the tone it carries,
 * and an attention label qualifying it. Lifecycle and readiness are separate controller-owned
 * values, so the projection picks the one that describes the deployment rather than joining both
 * into one string.
 */
export function projectApplicationDeploymentStatus(
  deployment: ApplicationDeployment,
): StatusProjection {
  // The application lifecycle has no state meaning "being removed", so teardown is read off the
  // target instead.
  if (deployment.desiredState === ApplicationDeploymentDesiredState.TEARDOWN) {
    return {
      attention: deployment.state?.observationStale ? staleMachineAttention : undefined,
      label: "Tearing down",
      tone: applicationDeploymentDesiredStateTone(deployment.desiredState),
    };
  }

  const lifecycleState = applicationDeploymentLifecycleState(deployment);
  const readinessState = applicationDeploymentReadinessState(deployment);

  // A stale state is the machine's last report, which the manager cannot confirm, so the card keeps
  // its word but never presents it as current.
  if (deployment.state?.observationStale) {
    return {
      attention: staleMachineAttention,
      label: applicationLifecycleLabel(lifecycleState),
      tone: "warning",
    };
  }

  // A deployment that is both up and serving carries no attention label: nothing qualifies it.
  if (
    lifecycleState === ApplicationLifecycleState.READY &&
    readinessState === ApplicationReadinessState.READY
  ) {
    return {
      label: applicationLifecycleLabel(lifecycleState),
      tone: applicationLifecycleTone(lifecycleState),
    };
  }

  const attention = applicationAttentionLabel(deployment, lifecycleState);

  // A deployment the controller reports as up but not serving is not ready whatever its lifecycle
  // word says, so the card states the readiness that contradicts it.
  if (
    (lifecycleState === ApplicationLifecycleState.RUNNING ||
      lifecycleState === ApplicationLifecycleState.READY) &&
    readinessState === ApplicationReadinessState.NOT_READY
  ) {
    return {
      attention,
      label: applicationReadinessLabel(readinessState),
      tone: applicationReadinessTone(readinessState),
    };
  }

  return {
    attention,
    ...(inProgressStates.has(lifecycleState) && { inProgress: true }),
    label: applicationLifecycleLabel(lifecycleState),
    tone: applicationLifecycleTone(lifecycleState),
  };
}

export function projectApplicationSnapshot(
  applications: readonly Application[],
  executionTargets: readonly ConfiguredExecutionTarget[],
  deployments: readonly ApplicationDeployment[],
  machines: readonly Machine[],
): ApplicationSnapshot {
  return {
    catalogApplications: [...applications],
    executionTargets: [...executionTargets],
    deployments: [...deployments],
    machines: [...machines],
  };
}

function applicationAttentionLabel(
  deployment: ApplicationDeployment,
  state: ApplicationLifecycleState,
): string | undefined {
  const failureMessage = deployment.state?.failure?.message;
  if (failureMessage) return failureMessage;

  // A deployment on its way up has one thing worth waiting for: the endpoint it will serve from.
  const settling =
    isTransitioningApplicationState(state) || state === ApplicationLifecycleState.RUNNING;
  return settling && !applicationDeploymentEndpoint(deployment)
    ? "Waiting for endpoint"
    : undefined;
}
