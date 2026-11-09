import { useEffect, useMemo, useRef } from "react";

import { machineName } from "@/domain/machine-name";
import { modelDeploymentPresentationName } from "@/domain/model-projection";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import { type DescribedMachine, useDescribedMachines } from "./described-machines";
import type { ModelLineRef } from "./memory-history";
import type { MachineSnapshotDetail } from "./use-hardware-snapshots";

/** What the RAM and VRAM history charts need beyond their grids. */
export interface MemoryHistory {
  /** Every listed machine with the latest snapshot this page read, counted now or not. */
  described: DescribedMachine[];
  /** Every listed machine's name by id, for history lines of machines not counted now. */
  machineNames: ReadonlyMap<string, string>;
  /** Every deployment seen while the page was open, by id, so a removed one keeps its line name. */
  models: ReadonlyMap<string, ModelLineRef>;
}

/**
 * The machines and deployments a page's memory history names: described machines, machine names,
 * and every deployment seen since the page opened.
 */
export function useMemoryHistory(
  machines: readonly Machine[] | undefined,
  details: ReadonlyMap<string, MachineSnapshotDetail>,
  deployments: readonly ModelDeployment[] | undefined,
): MemoryHistory {
  const described = useDescribedMachines(machines, details);
  const machineNames = useMemo(
    () => new Map((machines ?? []).map((machine) => [machine.machineId, machineName(machine)])),
    [machines],
  );
  const models = useSeenDeployments(deployments);

  return useMemo(() => ({ described, machineNames, models }), [described, machineNames, models]);
}

/**
 * Every deployment listed since mount, with the name and color its card had the last time it was
 * listed.
 */
function useSeenDeployments(
  deployments: readonly ModelDeployment[] | undefined,
): ReadonlyMap<string, ModelLineRef> {
  const seenRef = useRef<ReadonlyMap<string, ModelLineRef>>(new Map());
  const seen = useMemo(() => {
    const next = new Map(seenRef.current);
    for (const deployment of deployments ?? []) {
      next.set(deployment.modelDeploymentId, {
        colorKey: deployment.modelDeploymentId,
        name: modelDeploymentPresentationName(deployment),
      });
    }
    return next;
  }, [deployments]);

  useEffect(() => {
    seenRef.current = seen;
  }, [seen]);

  return seen;
}
