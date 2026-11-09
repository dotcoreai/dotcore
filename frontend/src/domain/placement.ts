import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import type { ConfiguredExecutionTarget, Machine } from "@/gen/dotcore/site/v1/service_pb";

import { placedExecutionTargetName } from "./execution-target";
import { machineName } from "./machine-name";

/**
 * Names where a deployment runs: the machine's display name and the execution target's display
 * name. A target missing from `targets`, such as one since removed, shows its execution target ID,
 * and an unknown machine shows its SiteNode ID. Until `targets` is read, the machine alone names
 * the placement.
 */
export function placementLabel(
  placement: Pick<ExactPlacement, "executionTargetId" | "siteNodeId"> | undefined,
  machine: Machine | undefined,
  targets: readonly ConfiguredExecutionTarget[] | undefined,
): string {
  if (!placement) return "Unavailable";

  const name = placementMachineName(placement.siteNodeId, machine);
  return targets ? placedExecutionTargetName(placement, targets, name) : name;
}

/**
 * The name of the machine a placement's SiteNode runs on. A machine the site doesn't list shows the
 * SiteNode ID, which is also its machine ID.
 */
export function placementMachineName(siteNodeId: string, machine: Machine | undefined): string {
  return machine ? machineName(machine) : siteNodeId;
}

/** The listed machine a placement's SiteNode names, if the site reports it. */
export function placementMachine(
  machines: readonly Machine[] | undefined,
  siteNodeId: string | undefined,
): Machine | undefined {
  return siteNodeId ? machines?.find((machine) => machine.machineId === siteNodeId) : undefined;
}
