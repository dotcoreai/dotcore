import type { ComboboxOption } from "@/components/ui/combobox";
import { localModelSnapshot, projectModelDeploymentStatus } from "@/domain/model-projection";
import { deploymentLabel } from "@/domain/route-projection";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentDesiredState } from "@/gen/dotcore/model/v1/deployment_pb";

/**
 * The model deployments a route member picker offers, leaving out those being torn down. A selected
 * deployment that is no longer listed stays as a disabled option, so the selection remains visible.
 */
export function deploymentOptions(
  deployments: readonly ModelDeployment[],
  selectedIds: readonly string[],
): ComboboxOption[] {
  const options = deployments
    .filter((deployment) => deployment.desiredState !== ModelDeploymentDesiredState.TEARDOWN)
    .map((deployment) => ({
      id: deployment.modelDeploymentId,
      label: deploymentLabel(deployment),
      detail: deploymentModelName(deployment),
      markerColorKey: deployment.modelDeploymentId,
      status: projectModelDeploymentStatus(deployment),
    }));
  const knownIds = new Set(options.map((option) => option.id));

  return [
    ...options,
    ...selectedIds
      .filter((id) => !knownIds.has(id))
      .map((id) => ({ id, label: `Unavailable deployment (${id})`, disabled: true })),
  ];
}

/**
 * The model display name shown beside a deployment's own name so deployments of one model can be told
 * apart. Empty when the deployment has no name of its own, since the label is already the model name.
 */
function deploymentModelName(deployment: ModelDeployment): string {
  if (!deployment.modelDeploymentName) return "";

  return localModelSnapshot(deployment.snapshot)?.modelDisplayName ?? "";
}
