import { type ReactNode, useMemo } from "react";

import { ResourceColorContext, type ResourceColors } from "@/app/resource-color-context";
import {
  useWatchedApplicationDeployments,
  useWatchedMachines,
  useWatchedModelDeployments,
  useWatchedRoutes,
} from "@/app/resource-watch";
import { assignResourceColors, getResourceColor } from "@/lib/resource-color";

/**
 * Assigns marker colors to the site's live machines, routes, model deployments and application
 * deployments as one pool, so no two of them share a color while the palette has a free one.
 */
export function ResourceColorProvider({ children }: { children: ReactNode }) {
  const machines = useWatchedMachines()?.value?.machines;
  const routes = useWatchedRoutes()?.value?.routes;
  const modelDeployments = useWatchedModelDeployments()?.value?.deployments;
  const applicationDeployments = useWatchedApplicationDeployments()?.value?.deployments;

  // Kinds keep a fixed order and IDs sort numerically, so resources created later, whose
  // sequence or timestamp IDs sort last, take a free color instead of moving older ones.
  const pool = [
    ...sortedIds(machines?.map((machine) => machine.machineId)),
    ...sortedIds(routes?.flatMap((view) => (view.route ? [view.route.inferenceRouteId] : []))),
    ...sortedIds(modelDeployments?.map((deployment) => deployment.modelDeploymentId)),
    ...sortedIds(applicationDeployments?.map((deployment) => deployment.applicationDeploymentId)),
  ];
  // Keyed by the pool's IDs, so status updates keep the same value and charts aren't redrawn.
  const poolKey = pool.join("\n");

  const colors = useMemo<ResourceColors>(() => {
    const assignment = assignResourceColors(poolKey ? poolKey.split("\n") : []);

    return {
      colorOf: (key) => assignment.colors.get(key) ?? getResourceColor(key),
      nextColor: assignment.nextColor,
    };
  }, [poolKey]);

  return <ResourceColorContext.Provider value={colors}>{children}</ResourceColorContext.Provider>;
}

function sortedIds(ids: string[] | undefined): string[] {
  return (ids ?? []).sort((a, b) => a.localeCompare(b, "en", { numeric: true }));
}
