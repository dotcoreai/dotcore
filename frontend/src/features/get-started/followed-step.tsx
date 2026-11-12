import { useEffect, useState } from "react";

import type { ApplicationClient } from "@/api/application-client";
import type { ModelClient } from "@/api/model-client";
import { applicationDeploymentLifecycleState } from "@/domain/application-projection";
import { deploymentStatus } from "@/domain/model-projection";
import { ApplicationLifecycleState } from "@/gen/dotcore/application/v1/state_pb";
import type { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import {
  useWatchedApplicationDeployments,
  useWatchedModelDeployments,
  useWatchedRoutes,
} from "@/resource-watch/resource-watch";

import { DeployingStep, type FollowedDeployments } from "./deploying-step";
import { isApplicationReady } from "./get-started-proposals";
import { ReadyStep } from "./ready-step";

/**
 * Follows the deployments a use case started: it shows their progress from the moment Deploy is
 * clicked and, once every one is ready, moves on to what the operator can do with them. A
 * deployment that failed, or that the watch stopped listing after it appeared, offers the way back
 * to the proposal.
 */
export function FollowedStep({
  applicationClient,
  deployments,
  machines,
  modelClient,
  onBack,
  onReady,
  useCase,
}: {
  applicationClient: ApplicationClient;
  deployments: FollowedDeployments;
  machines: readonly Machine[];
  modelClient: ModelClient;
  /** Returns to the use case's proposal. */
  onBack: () => void;
  /** Called while the ready step shows, and again when the callback changes. */
  onReady: () => void;
  useCase: UseCase;
}) {
  const modelId = deployments.model?.id;
  const applicationId = deployments.application?.id;
  const modelDeployment = useWatchedModelDeployments()?.value?.deployments.find(
    (deployment) => modelId !== undefined && deployment.modelDeploymentId === modelId,
  );
  const applicationDeployment = useWatchedApplicationDeployments()?.value?.deployments.find(
    (deployment) =>
      applicationId !== undefined && deployment.applicationDeploymentId === applicationId,
  );
  const routes = useWatchedRoutes()?.value?.routes ?? [];
  // A deployment counts as removed only once it was listed: a new one isn't listed at first.
  const [seen, setSeen] = useState<ReadonlySet<string>>(new Set());
  const listed = [
    modelDeployment?.modelDeploymentId,
    applicationDeployment?.applicationDeploymentId,
  ].filter((id): id is string => id !== undefined && !seen.has(id));
  if (listed.length) setSeen(new Set([...seen, ...listed]));
  const removed = (id: string | undefined, deployment: object | undefined) =>
    Boolean(id && !deployment && seen.has(id));
  const removedKinds = [
    ...(removed(modelId, modelDeployment) ? ["model"] : []),
    ...(removed(applicationId, applicationDeployment) ? ["app"] : []),
  ];
  const failed =
    (modelDeployment !== undefined &&
      deploymentStatus(modelDeployment) === ModelDeploymentStatus.FAILED) ||
    (applicationDeployment !== undefined &&
      applicationDeploymentLifecycleState(applicationDeployment) ===
        ApplicationLifecycleState.FAILED);
  const modelReady =
    !deployments.model ||
    (modelDeployment !== undefined &&
      deploymentStatus(modelDeployment) === ModelDeploymentStatus.READY);
  const applicationReady = !deployments.application || isApplicationReady(applicationDeployment);

  const ready = modelReady && applicationReady;
  useEffect(() => {
    if (ready) onReady();
  }, [onReady, ready]);

  if (ready) {
    return (
      <ReadyStep
        applicationDeployment={applicationDeployment}
        machines={machines}
        modelDeployment={modelDeployment}
        routes={routes}
        useCase={useCase}
      />
    );
  }

  return (
    <DeployingStep
      applicationClient={applicationClient}
      applicationDeployment={applicationDeployment}
      application={deployments.application}
      modelClient={modelClient}
      modelDeployment={modelDeployment}
      model={deployments.model}
      notice={
        removedKinds.length
          ? `The ${removedKinds.join(" and ")} ${removedKinds.length > 1 ? "deployments were" : "deployment was"} removed.`
          : undefined
      }
      onBack={failed || removedKinds.length ? onBack : undefined}
    />
  );
}
