import type { ApplicationClient } from "@/api/application-client";
import type { RouteClient } from "@/api/route-client";
import { applicationDeploymentPresentationName } from "@/domain/application-deployment-name";
import type { PanelTitleLookup } from "@/panels/panel-titles-context";

/** Reads a route or application deployment by ID, the only resources the backend returns singly. */
export function createPanelTitleLookup(
  routes: RouteClient,
  applications: ApplicationClient,
): PanelTitleLookup {
  return async (entry, signal) => {
    if (!entry.resourceId) return undefined;

    if (entry.page === "routes") {
      const { route } = await routes.route.getRoute(
        { inferenceRouteId: entry.resourceId },
        { signal },
      );
      return route?.route?.name;
    }
    if (entry.page === "apps") {
      const { deployment } = await applications.application.getApplicationDeployment(
        { applicationDeploymentId: entry.resourceId },
        { signal },
      );
      return deployment ? applicationDeploymentPresentationName(deployment) : undefined;
    }

    return undefined;
  };
}
