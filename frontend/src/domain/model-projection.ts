import { ModelInputModality } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
  type ModelDeploymentSnapshot,
  type ResolvedLocalModelDeploymentSnapshot,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import type { LifecycleTone, StatusProjection } from "@/lib/status-projection";

export interface ModelSnapshot {
  availableModels: Model[];
  runningModels: ModelDeployment[];
}

const transitioningStates = new Set([
  ModelDeploymentStatus.QUEUED,
  ModelDeploymentStatus.DOWNLOADING,
  ModelDeploymentStatus.PREPARING,
  ModelDeploymentStatus.LOADING,
  ModelDeploymentStatus.WARMING_UP,
  ModelDeploymentStatus.UNLOADING,
  ModelDeploymentStatus.PAUSING,
]);

const inProgressStatuses = new Set([
  ModelDeploymentStatus.PREPARING,
  ModelDeploymentStatus.DOWNLOADING,
  ModelDeploymentStatus.LOADING,
  ModelDeploymentStatus.WARMING_UP,
]);

const pausedStatuses = new Set([
  ModelDeploymentStatus.PAUSING,
  ModelDeploymentStatus.PAUSED,
  ModelDeploymentStatus.UNAVAILABLE,
]);

export function modelDeploymentPresentationName(deployment: ModelDeployment): string {
  if (deployment.modelDeploymentName) return deployment.modelDeploymentName;
  const model = localModelSnapshot(deployment.snapshot);
  return model?.modelDisplayName ?? "Upstream model";
}

export function localModelSnapshot(
  snapshot: ModelDeploymentSnapshot | undefined,
): ResolvedLocalModelDeploymentSnapshot | undefined {
  if (snapshot?.source.case !== "local") {
    return undefined;
  }

  return snapshot.source.value;
}

export function modelStatusLabel(state: ModelDeploymentStatus): string {
  switch (state) {
    case ModelDeploymentStatus.QUEUED:
      return "Queued";
    case ModelDeploymentStatus.PREPARING:
      return "Preparing";
    case ModelDeploymentStatus.DOWNLOADING:
      return "Downloading";
    case ModelDeploymentStatus.LOADING:
      return "Loading";
    case ModelDeploymentStatus.WARMING_UP:
      return "Warming up";
    case ModelDeploymentStatus.READY:
      return "Ready";
    case ModelDeploymentStatus.UNLOADING:
      return "Tearing down";
    case ModelDeploymentStatus.PAUSING:
      return "Pausing";
    case ModelDeploymentStatus.PAUSED:
      return "Paused";
    case ModelDeploymentStatus.FAILED:
      return "Failed";
    case ModelDeploymentStatus.UNAVAILABLE:
      return "Unavailable";
    default:
      return "Unknown";
  }
}

/**
 * The deployment target a model deployment is being driven toward. It names the target itself, so it
 * stays distinguishable from the status reported alongside it: a deployment whose target is
 * `Teardown` reports `Tearing down` while the controller works through it.
 */
export function modelDeploymentDesiredStateLabel(state: ModelDeploymentDesiredState): string {
  switch (state) {
    case ModelDeploymentDesiredState.PRESENT:
      return "Present";
    case ModelDeploymentDesiredState.PAUSED:
      return "Paused";
    case ModelDeploymentDesiredState.TEARDOWN:
      return "Teardown";
    default:
      return "Unspecified";
  }
}

/** A lowercase name for a model input modality, such as "text" or "image". */
export function modelInputModalityLabel(modality: ModelInputModality): string {
  return (
    ModelInputModality[modality]?.replace("MODEL_INPUT_MODALITY_", "").toLowerCase() ||
    "Unspecified"
  );
}

/** The declared weight quantization of a model artifact, or a placeholder when none is declared. */
export function weightQuantizationLabel(quantization: string): string {
  return quantization || "Not declared";
}

export function deploymentStatus(deployment: ModelDeployment): ModelDeploymentStatus {
  if (deployment.desiredState === ModelDeploymentDesiredState.TEARDOWN) {
    return ModelDeploymentStatus.UNLOADING;
  }

  const status = deployment.state?.status ?? ModelDeploymentStatus.UNSPECIFIED;
  if (
    deployment.desiredState === ModelDeploymentDesiredState.PAUSED &&
    !pausedStatuses.has(status)
  ) {
    return ModelDeploymentStatus.PAUSING;
  }

  return status;
}

export function isTransitioningModelStatus(state: ModelDeploymentStatus): boolean {
  return transitioningStates.has(state);
}

/**
 * The tone one model deployment status carries. It is the only place that decision is made, so a
 * status reads the same colour on a card, which shows an aggregate, and in an inspector, which
 * shows this status on its own row.
 *
 * Every named status is listed rather than folded into the default, so adding one to the proto
 * forces a colour decision here. The default covers only the unspecified value and a value from a
 * newer backend than this build knows.
 */
export function modelStatusTone(state: ModelDeploymentStatus): LifecycleTone {
  switch (state) {
    case ModelDeploymentStatus.QUEUED:
    case ModelDeploymentStatus.PREPARING:
    case ModelDeploymentStatus.DOWNLOADING:
    case ModelDeploymentStatus.LOADING:
    case ModelDeploymentStatus.WARMING_UP:
    case ModelDeploymentStatus.UNLOADING:
    case ModelDeploymentStatus.PAUSING:
      return "info";
    case ModelDeploymentStatus.READY:
      return "success";
    // Unavailable is a fault, not an intentional pause: the proto defines it as a deployment
    // unavailable for controller operation.
    case ModelDeploymentStatus.FAILED:
    case ModelDeploymentStatus.UNAVAILABLE:
      return "destructive";
    case ModelDeploymentStatus.PAUSED:
      return "neutral";
    default:
      return "neutral";
  }
}

/**
 * The tone one deployment target carries. A target is an intent rather than an observation, so only
 * teardown is toned at all and every other target reads as a plain fact.
 */
export function modelDeploymentDesiredStateTone(state: ModelDeploymentDesiredState): LifecycleTone {
  return state === ModelDeploymentDesiredState.TEARDOWN ? "info" : "neutral";
}

/**
 * The status a model deployment card renders: one status word, the tone it carries, whether it is
 * being deployed, and an attention label qualifying it. The attention label reports the backend failure message once the
 * deployment has failed; download progress is shown in the inspector, not on the card.
 */
export function projectModelDeploymentStatus(deployment: ModelDeployment): StatusProjection {
  const status = deploymentStatus(deployment);

  return {
    attention: modelAttentionLabel(deployment, status),
    ...(inProgressStatuses.has(status) && { inProgress: true }),
    label: modelStatusLabel(status),
    tone: modelStatusTone(status),
  };
}

/**
 * Whether an inspector shows the deployment target next to its status. The status word already
 * names a pause or teardown target, so the target only adds information when the deployment is
 * meant to be present but is paused or pausing, such as a resume that has not taken effect yet.
 */
export function isModelDesiredStateVisible(deployment: ModelDeployment): boolean {
  const status = deploymentStatus(deployment);

  return (
    deployment.desiredState === ModelDeploymentDesiredState.PRESENT &&
    (status === ModelDeploymentStatus.PAUSED || status === ModelDeploymentStatus.PAUSING)
  );
}

/**
 * The backend's free-text status explanation, or undefined when it adds nothing: the backend
 * repeats the status word in normal states, and the failure message has its own notice.
 */
export function modelStatusMessage(deployment: ModelDeployment): string | undefined {
  const message = deployment.state?.statusMessage.trim();
  if (!message) {
    return undefined;
  }

  const normalized = message.toLowerCase();
  const failureMessage = deployment.state?.failure?.message.trim().toLowerCase();
  if (
    normalized === modelStatusLabel(deploymentStatus(deployment)).toLowerCase() ||
    normalized === failureMessage
  ) {
    return undefined;
  }

  return message;
}

/**
 * The status a catalog model card carries: how many deployments of it exist, whatever their state. A catalog model
 * is a declaration with no lifecycle of its own, so the count is the whole badge rather than a
 * qualifier appended to a status word, and this function owns its tone because no state enum backs
 * it. A model nobody has deployed carries no badge rather than a "0 deployed" one.
 */
export function projectCatalogModelStatus(runningCount: number): StatusProjection | undefined {
  return runningCount > 0 ? { label: `${runningCount} deployed`, tone: "success" } : undefined;
}

export function isUnloadableModel(deployment: ModelDeployment): boolean {
  return (
    deployment.desiredState === ModelDeploymentDesiredState.PRESENT ||
    deployment.desiredState === ModelDeploymentDesiredState.PAUSED
  );
}

export function isPausableModel(deployment: ModelDeployment): boolean {
  return deployment.desiredState === ModelDeploymentDesiredState.PRESENT;
}

export function isResumableModel(deployment: ModelDeployment): boolean {
  return deployment.desiredState === ModelDeploymentDesiredState.PAUSED;
}

export function projectModelSnapshot(
  models: readonly Model[],
  deployments: readonly ModelDeployment[],
): ModelSnapshot {
  return {
    availableModels: [...models],
    runningModels: [...deployments],
  };
}

function modelAttentionLabel(
  deployment: ModelDeployment,
  state: ModelDeploymentStatus,
): string | undefined {
  // A pause or teardown rewrites the status word, so the failure still has to stay reachable.
  const failureVisible =
    state === ModelDeploymentStatus.FAILED ||
    deployment.desiredState === ModelDeploymentDesiredState.PAUSED ||
    deployment.desiredState === ModelDeploymentDesiredState.TEARDOWN;

  return failureVisible ? deployment.state?.failure?.message || undefined : undefined;
}
