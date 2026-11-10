import { localModelSnapshot } from "@/domain/model-projection";
import type { RouteMention } from "@/features/deployments/panel-mention";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import { appendPanelHref, type PanelLocation } from "@/panels/panel-stack";

export function containingRoutesForDeployment(
  routes: ReadonlyArray<InferenceRouteView>,
  modelDeploymentId: string,
  location: PanelLocation,
): readonly RouteMention[] {
  return routes.flatMap((view) => {
    const route = view.route;
    if (
      !route ||
      !route.fallbackGroups.some((group) => group.modelDeploymentIds.includes(modelDeploymentId))
    ) {
      return [];
    }
    return [
      {
        href: appendPanelHref(location, {
          page: "routes",
          resourceId: route.inferenceRouteId,
          screen: "route",
        }),
        inferenceRouteId: route.inferenceRouteId,
        name: route.name,
      },
    ];
  });
}

/** Destination of a deployment's catalog model mention, pushed above the deployment panel. */
export function catalogModelHrefForDeployment(
  deployment: ModelDeployment,
  location: PanelLocation,
): string | undefined {
  const modelId = localModelSnapshot(deployment.snapshot)?.modelId;
  if (!modelId) return undefined;

  return appendPanelHref(location, {
    page: "model-catalog",
    resourceId: modelId,
    screen: "catalog",
  });
}

/** Destination of a deployment's placement machine mention, pushed above the deployment panel. */
export function placementMachineHrefForDeployment(
  machine: Machine | undefined,
  location: PanelLocation,
): string | undefined {
  return machine
    ? appendPanelHref(location, {
        page: "machines",
        resourceId: machine.machineId,
        screen: "machine",
      })
    : undefined;
}
