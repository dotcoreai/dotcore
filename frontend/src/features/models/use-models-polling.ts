import { useCallback, useEffect, useMemo, useState } from "react";

import { type ModelClient } from "@/api/model-client";
import {
  acceptsOverlay,
  type DeploymentFields,
  type DeploymentOverlay,
  mergeDeploymentOverlays,
  withoutSettled,
  withOverlay,
} from "@/features/deployments/deployment-overlays";
import { type ModelSnapshot, projectModelSnapshot } from "@/features/models/model-projection";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
  type UpstreamModelDesiredConfiguration,
} from "@/gen/dotcore/model/v1/deployment_pb";
import type { DeploymentMetadata } from "@/lib/deployment-metadata";
import { useWatchedModelDeployments } from "@/resource-watch/resource-watch";
import { useSessionRead } from "@/resource-watch/use-session-read";

const SERVICE_ERROR_MESSAGE =
  "The catalog or model service is unavailable. Check that the backend is running and try again.";

/**
 * The models feature's view of the catalog models and the live model deployments. Deployments come
 * from the console's resource watch; the catalog is read once per manager session. An accepted
 * mutation shows at once as an overlay over the streamed deployments, until the stream shows it.
 * While disabled, nothing is watched or read and accepted overlays are kept.
 */
export function useModelsPolling(
  client: ModelClient,
  { enabled = true }: { enabled?: boolean } = {},
) {
  const readModels = useCallback(
    (signal: AbortSignal) =>
      client.catalog.listModels({}, { signal }).then((response) => response.models),
    [client],
  );
  const catalog = useSessionRead(readModels, { enabled });
  const watched = useWatchedModelDeployments({ enabled });
  const [overlays, setOverlays] = useState<ReadonlyMap<string, ModelOverlay>>(() => new Map());

  const merged = useMemo(
    () =>
      watched?.value
        ? {
            ...mergeDeploymentOverlays(watched.value.deployments, overlays, modelFields),
            overlays,
          }
        : undefined,
    [overlays, watched?.value],
  );
  const snapshot = useMemo<ModelSnapshot | undefined>(
    () =>
      catalog.value && merged ? projectModelSnapshot(catalog.value, merged.deployments) : undefined,
    [catalog.value, merged],
  );

  // Overlays the stream shows are dropped, so a later change made elsewhere is not hidden by them.
  useEffect(() => {
    if (!merged?.settled.length) return;
    setOverlays((current) => withoutSettled(current, merged.overlays, merged.settled));
  }, [merged]);

  const listed = (modelDeploymentId: string) =>
    merged?.deployments.find((candidate) => candidate.modelDeploymentId === modelDeploymentId);

  /** Shows an accepted create, pause, resume or delete until the stream shows it. */
  const applyDeploymentOverlay = (deployment: ModelDeployment) => {
    const created = !watched?.value?.deployments.some(
      (candidate) => candidate.modelDeploymentId === deployment.modelDeploymentId,
    );
    setOverlays((current) => {
      const pending = current.get(deployment.modelDeploymentId)?.deployment;
      const shown = listed(deployment.modelDeploymentId);
      if (!acceptsOverlay(deployment, pending, shown, modelFields)) return current;

      return withOverlay(current, deployment.modelDeploymentId, {
        deployment,
        fence: { created, kind: "lifecycle" },
      });
    });
  };

  /** Shows an accepted rename until the stream shows the new name. */
  const applyDeploymentMetadataOverlay = (
    modelDeploymentId: string,
    metadata: DeploymentMetadata,
  ) => {
    applyContentOverlay(modelDeploymentId, {
      apply: (streamed) => ({ ...streamed, modelDeploymentName: metadata.displayName }),
      shown: (streamed) => streamed.modelDeploymentName === metadata.displayName,
    });
  };

  /** Shows an accepted endpoint edit until the stream shows the new endpoint. */
  const applyDeploymentEndpointOverlay = (
    modelDeploymentId: string,
    configuration: UpstreamModelDesiredConfiguration,
  ) => {
    applyContentOverlay(modelDeploymentId, {
      apply: (streamed) => ({ ...streamed, upstreamDesiredConfiguration: configuration }),
      shown: (streamed) => matchesEndpoint(streamed.upstreamDesiredConfiguration, configuration),
    });
  };

  const applyContentOverlay = (
    modelDeploymentId: string,
    change: Pick<Extract<ModelOverlay["fence"], { kind: "content" }>, "apply" | "shown">,
  ) => {
    const deployment = listed(modelDeploymentId);
    if (!deployment || modelFields.tearingDown(deployment)) return;

    setOverlays((current) =>
      withOverlay(current, modelDeploymentId, {
        deployment: change.apply(deployment),
        fence: { kind: "content", ...change },
      }),
    );
  };

  const deploymentsFailed = watched?.failed ?? false;
  const error =
    (catalog.failed && !catalog.value) || (deploymentsFailed && !watched?.value)
      ? SERVICE_ERROR_MESSAGE
      : undefined;

  return {
    applyDeploymentEndpointOverlay,
    applyDeploymentMetadataOverlay,
    applyDeploymentOverlay,
    /** Whether the live deployments are reconnecting; the last ones stay in view. */
    deploymentsFailed,
    error,
    snapshot,
  };
}

/**
 * Whether an observed upstream configuration matches a requested one. The API key is left out:
 * reads never return it.
 */
export function matchesEndpoint(
  observed: UpstreamModelDesiredConfiguration | undefined,
  requested: UpstreamModelDesiredConfiguration,
): boolean {
  return (
    observed?.upstreamModelId === requested.upstreamModelId &&
    observed.endpoint?.baseUrl === requested.endpoint?.baseUrl &&
    observed.endpoint?.allowInsecureHttp === requested.endpoint?.allowInsecureHttp &&
    observed.inputCapabilities?.supportsText === requested.inputCapabilities?.supportsText &&
    observed.inputCapabilities?.supportsImage === requested.inputCapabilities?.supportsImage &&
    observed.boundary?.locality === requested.boundary?.locality &&
    observed.boundary?.trustDomain === requested.boundary?.trustDomain &&
    observed.boundary?.dataEgressClass === requested.boundary?.dataEgressClass
  );
}

type ModelOverlay = DeploymentOverlay<ModelDeployment>;

const modelFields: DeploymentFields<ModelDeployment> = {
  desiredState: (deployment) => deployment.desiredState,
  id: (deployment) => deployment.modelDeploymentId,
  tearingDown: (deployment) => deployment.desiredState === ModelDeploymentDesiredState.TEARDOWN,
};
