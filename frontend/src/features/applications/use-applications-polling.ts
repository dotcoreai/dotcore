import { useCallback, useEffect, useMemo, useState } from "react";

import type { ApplicationClient } from "@/api/application-client";
import {
  type ApplicationSnapshot,
  projectApplicationSnapshot,
} from "@/domain/application-projection";
import type { DeploymentMetadata } from "@/domain/deployment-metadata";
import {
  acceptsOverlay,
  type DeploymentFields,
  type DeploymentOverlay,
  mergeDeploymentOverlays,
  withoutSettled,
  withOverlay,
} from "@/features/deployments/deployment-overlays";
import {
  type ApplicationDeployment,
  ApplicationDeploymentDesiredState,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  type ResourceWatchRead,
  useWatchedApplicationDeployments,
  useWatchedExecutionTargets,
  useWatchedMachines,
} from "@/resource-watch/resource-watch";
import { useSessionRead } from "@/resource-watch/use-session-read";

export interface ApplicationSliceStatus {
  error?: string;
  hasData: boolean;
  loading: boolean;
  stale: boolean;
}

export interface ApplicationsPollingStatuses {
  catalog: ApplicationSliceStatus;
  deployments: ApplicationSliceStatus;
  executionTargets: ApplicationSliceStatus;
}

const sliceErrors = {
  catalog: "Catalog is unavailable. Last successful catalog data remains visible.",
  deployments:
    "Installed deployments are unavailable. Last successful Installed data remains visible. Creation is disabled until deployments load successfully.",
  executionTargets:
    "Application execution targets are stale because the site service is unavailable. Creation is disabled until a fresh result succeeds.",
};

/**
 * The apps feature's view of the catalog applications, live application deployments, execution
 * targets and machines. Everything but the catalog comes from the console's resource watch; the
 * catalog is read once per manager session. Each slice keeps its last data through a failure and
 * reports it stale. Machines carry no status of their own: they only name placements, which fall
 * back to SiteNode IDs while machines are unknown. An accepted mutation shows at once as an overlay
 * over the streamed deployments, until the stream shows it.
 */
export function useApplicationsPolling(client: ApplicationClient) {
  const readCatalog = useCallback(
    (signal: AbortSignal) =>
      client.catalog.listApplications({}, { signal }).then((response) => response.applications),
    [client],
  );
  const catalog = useSessionRead(readCatalog);
  const watchedDeployments = useWatchedApplicationDeployments();
  const watchedTargets = useWatchedExecutionTargets();
  const watchedMachines = useWatchedMachines();
  const [overlays, setOverlays] = useState<ReadonlyMap<string, ApplicationOverlay>>(
    () => new Map(),
  );

  const merged = useMemo(
    () =>
      watchedDeployments?.value
        ? {
            ...mergeDeploymentOverlays(
              watchedDeployments.value.deployments,
              overlays,
              applicationFields,
            ),
            overlays,
          }
        : undefined,
    [overlays, watchedDeployments?.value],
  );

  // Overlays the stream shows are dropped, so a later change made elsewhere is not hidden by them.
  useEffect(() => {
    if (!merged?.settled.length) return;
    setOverlays((current) => withoutSettled(current, merged.overlays, merged.settled));
  }, [merged]);

  const statuses = useMemo<ApplicationsPollingStatuses>(
    () => ({
      catalog: sliceStatus(catalog, sliceErrors.catalog),
      deployments: sliceStatus(watchedDeployments, sliceErrors.deployments),
      executionTargets: sliceStatus(watchedTargets, sliceErrors.executionTargets),
    }),
    [catalog, watchedDeployments, watchedTargets],
  );
  const settled = [statuses.catalog, statuses.deployments, statuses.executionTargets].every(
    (status) => !status.loading,
  );
  const catalogApplications = catalog.value;
  const executionTargets = watchedTargets?.value?.executionTargets;
  const machines = watchedMachines?.value?.machines;
  const snapshot = useMemo<ApplicationSnapshot | undefined>(
    () =>
      settled
        ? projectApplicationSnapshot(
            catalogApplications ?? [],
            executionTargets ?? [],
            merged?.deployments ?? [],
            machines ?? [],
          )
        : undefined,
    [catalogApplications, executionTargets, machines, merged?.deployments, settled],
  );

  const listed = (applicationDeploymentId: string) =>
    merged?.deployments.find(
      (candidate) => candidate.applicationDeploymentId === applicationDeploymentId,
    );

  /** Shows an accepted create, pause, resume or delete until the stream shows it. */
  const applyDeploymentOverlay = (deployment: ApplicationDeployment) => {
    const created = !watchedDeployments?.value?.deployments.some(
      (candidate) => candidate.applicationDeploymentId === deployment.applicationDeploymentId,
    );
    setOverlays((current) => {
      const pending = current.get(deployment.applicationDeploymentId)?.deployment;
      const shown = listed(deployment.applicationDeploymentId);
      if (!acceptsOverlay(deployment, pending, shown, applicationFields)) return current;

      return withOverlay(current, deployment.applicationDeploymentId, {
        deployment,
        fence: { created, kind: "lifecycle" },
      });
    });
  };

  /** Shows an accepted rename until the stream shows the new name. */
  const applyDeploymentMetadataOverlay = (
    applicationDeploymentId: string,
    metadata: DeploymentMetadata,
  ) => {
    const deployment = listed(applicationDeploymentId);
    if (!deployment || applicationFields.tearingDown(deployment)) return;

    const apply = (streamed: ApplicationDeployment) => ({
      ...streamed,
      applicationDeploymentName: metadata.displayName,
    });
    setOverlays((current) =>
      withOverlay(current, applicationDeploymentId, {
        deployment: apply(deployment),
        fence: {
          apply,
          kind: "content",
          shown: (streamed) => streamed.applicationDeploymentName === metadata.displayName,
        },
      }),
    );
  };

  return {
    applyDeploymentMetadataOverlay,
    applyDeploymentOverlay,
    snapshot,
    statuses,
  };
}

type ApplicationOverlay = DeploymentOverlay<ApplicationDeployment>;

const applicationFields: DeploymentFields<ApplicationDeployment> = {
  desiredState: (deployment) => deployment.desiredState,
  id: (deployment) => deployment.applicationDeploymentId,
  tearingDown: (deployment) =>
    deployment.desiredState === ApplicationDeploymentDesiredState.TEARDOWN,
};

function sliceStatus(
  read: ResourceWatchRead<unknown> | undefined,
  error: string,
): ApplicationSliceStatus {
  const hasData = read?.value !== undefined;
  const failed = read?.failed ?? false;

  return {
    error: failed ? error : undefined,
    hasData,
    loading: !hasData && !failed,
    stale: failed && hasData,
  };
}
