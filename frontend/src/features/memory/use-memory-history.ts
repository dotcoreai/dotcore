import { useMemo } from "react";

import { machineName } from "@/domain/machine-name";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import { type DescribedMachine, useDescribedMachines } from "./described-machines";
import type { MachineSnapshotDetail } from "./use-hardware-snapshots";

/** What the RAM and VRAM history charts need beyond their grids. */
export interface MemoryHistory {
  /** Every listed machine with the latest snapshot this page read, counted now or not. */
  described: DescribedMachine[];
  /** Every listed machine's name by id, for history lines of machines not counted now. */
  machineNames: ReadonlyMap<string, string>;
}

/**
 * The machines a page's memory history names: described machines and every listed machine's
 * name.
 */
export function useMemoryHistory(
  machines: readonly Machine[] | undefined,
  details: ReadonlyMap<string, MachineSnapshotDetail>,
): MemoryHistory {
  const described = useDescribedMachines(machines, details);
  const machineNames = useMemo(
    () => new Map((machines ?? []).map((machine) => [machine.machineId, machineName(machine)])),
    [machines],
  );

  return useMemo(() => ({ described, machineNames }), [described, machineNames]);
}
