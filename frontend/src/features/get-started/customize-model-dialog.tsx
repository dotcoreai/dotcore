import { useCallback, useMemo } from "react";

import type { ModelClient } from "@/api/model-client";
import type { SiteClient } from "@/api/site-client";
import { modelDeployRequests } from "@/features/models/catalog/use-catalog-deploy";
import { ModelDeployDialog } from "@/features/models/deploy/model-deploy-dialog";
import type { LocalDeploymentValues } from "@/features/models/deploy/model-deploy-launch";
import type { RouteNameConflict } from "@/features/models/deployment-exposure";
import {
  type SizingScenario,
  sizingScenarioRequest,
} from "@/features/models/profiles/sizing-scenario";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { useWatchedRoutes } from "@/resource-watch/resource-watch";
import { useSessionRead } from "@/resource-watch/use-session-read";

import { launchScenario } from "./get-started-launch";

/**
 * The deploy dialog opened on a proposal's launch, or on the one last applied, with every value it
 * carries. It ends on Apply, which hands the edited launch back to Get started without deploying
 * anything.
 */
export function CustomizeModelDialog({
  deployments,
  launch,
  model,
  modelClient,
  onApply,
  onClose,
  routeConflict,
  siteClient,
}: {
  /** The site's model deployments, which name what occupies the target's memory. */
  deployments: readonly ModelDeployment[];
  launch: LocalDeploymentValues;
  model: Model;
  modelClient: ModelClient;
  onApply: (launch: LocalDeploymentValues) => void;
  onClose: () => void;
  /** The route name the last Deploy found taken. */
  routeConflict?: RouteNameConflict;
  siteClient: SiteClient;
}) {
  const engineReleases = useSessionRead(
    useCallback(
      (signal: AbortSignal) =>
        modelClient.catalog
          .listEngineReleases({}, { signal })
          .then((response) => response.engineReleases),
      [modelClient],
    ),
  );
  const hardwareClasses = useSessionRead(
    useCallback(
      (signal: AbortSignal) =>
        modelClient.catalog
          .listHardwareClasses({}, { signal })
          .then((response) => response.hardwareClasses),
      [modelClient],
    ),
  );
  const routes = useWatchedRoutes()?.value?.routes;
  const listConfiguredExecutionTargets = useCallback(
    () => siteClient.listConfiguredExecutionTargets({}),
    [siteClient],
  );
  const readCompatibility = modelClient.model.listModelProfileCompatibility;
  const listProfileCompatibility = useMemo(
    () =>
      readCompatibility
        ? (modelId: string, scenario?: SizingScenario) =>
            readCompatibility({
              modelId,
              ...(scenario ? { scenario: sizingScenarioRequest(scenario) } : {}),
            })
        : undefined,
    [readCompatibility],
  );

  // The dialog sizes its profiles and targets from the catalog's engines and hardware classes.
  if (!engineReleases.value || !hardwareClasses.value) return null;

  return (
    <ModelDeployDialog
      {...modelDeployRequests(modelClient, model)}
      deployments={deployments}
      engineReleases={engineReleases.value}
      existingRouteNames={routes?.flatMap((view) => (view.route ? [view.route.name] : []))}
      hardwareClasses={hardwareClasses.value}
      initialConfiguration={launch.configuration}
      initialExposure={launch.exposure}
      initialName={launch.name}
      initialPlacement={launch.placement}
      initialProfileId={launch.profileId}
      labels={customizeLabels}
      listConfiguredExecutionTargets={listConfiguredExecutionTargets}
      listProfileCompatibility={listProfileCompatibility}
      model={model}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      onSubmit={onApply}
      open
      pending={false}
      routeConflict={routeConflict}
      scenario={launchScenario(launch)}
    />
  );
}

const customizeLabels = { pending: "Applying…", submit: "Apply", title: "Customize model" };
