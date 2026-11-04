import { useMemo } from "react";

import {
  useWatchedApplicationDeployments,
  useWatchedMachines,
  useWatchedNodeMemory,
  type WatchOptions,
} from "@/app/resource-watch";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import type { NodeMemory } from "@/gen/dotcore/capacity/v1/memory_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import type { ReadState } from "@/lib/read-state";

import { projectSiteMemory } from "./node-memory";
import type { SiteMemory } from "./site-memory";

/** The site's memory while its reads load, once drawn, or when nothing is readable. */
export type SiteMemoryState =
  | { kind: "loading" }
  | { kind: "ready"; memory: SiteMemory }
  | { kind: "unavailable"; memory?: SiteMemory };

/**
 * Watches the site's machines, node memory and application deployments while mounted and enabled,
 * and draws them with the given model deployments into memory rows.
 */
export function useSiteMemory(
  deployments: readonly ModelDeployment[] | undefined,
  deploymentsReadFailed: boolean,
  options?: WatchOptions,
): SiteMemoryState {
  const machines = useWatchedMachines(options);
  const applications = useWatchedApplicationDeployments(options);
  const nodeMemory = useWatchedNodeMemory(options);

  return useMemo(
    () =>
      siteMemoryState({
        applications: applications?.value?.deployments,
        deployments,
        deploymentsReadFailed: deploymentsReadFailed || Boolean(applications?.failed),
        machines: machines?.value?.machines,
        machinesReadFailed: machines?.failed ?? false,
        nodeMemory: nodeMemory && {
          failed: nodeMemory.failed,
          value: nodeMemory.value?.nodes,
        },
      }),
    [applications, deployments, deploymentsReadFailed, machines, nodeMemory],
  );
}

/**
 * Draws the site's memory from reads a caller already holds: the listed machines, the node memory
 * and the deployments. A caller that counts only some machines passes only those.
 */
export function siteMemoryState({
  applications,
  deployments,
  deploymentsReadFailed,
  machines,
  machinesReadFailed,
  nodeMemory,
}: {
  /** The application deployments; absent names none. */
  applications?: readonly ApplicationDeployment[];
  deployments: readonly ModelDeployment[] | undefined;
  /** Whether the latest model or application deployment read failed. */
  deploymentsReadFailed: boolean;
  /** The listed machines; undefined until the first list read settles. */
  machines: readonly Machine[] | undefined;
  machinesReadFailed: boolean;
  nodeMemory: ReadState<readonly NodeMemory[]> | undefined;
}): SiteMemoryState {
  if (!deployments) return { kind: "loading" };
  if (!machines) return machinesReadFailed ? { kind: "unavailable" } : { kind: "loading" };
  if (!nodeMemory?.value) return nodeMemory?.failed ? { kind: "unavailable" } : { kind: "loading" };

  const memory = projectSiteMemory({
    applications,
    deployments,
    machines: machines.map((machine) => ({
      machineId: machine.machineId,
      name: machine.displayName || machine.machineId,
    })),
    nodes: nodeMemory.value,
    readFailed: deploymentsReadFailed || nodeMemory.failed,
  });
  return memory.rows.length ? { kind: "ready", memory } : { kind: "unavailable", memory };
}
