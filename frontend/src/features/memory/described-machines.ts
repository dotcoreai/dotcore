import { useEffect, useMemo, useRef } from "react";

import type { HardwareSnapshot } from "@/gen/dotcore/observation/v1/observation_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import type { MachineSnapshotDetail } from "./use-hardware-snapshots";

/**
 * A machine with a hardware snapshot: counted now (connected, with a current snapshot), or, for
 * history and queries, described by the latest snapshot the page read.
 */
export interface DescribedMachine {
  machineId: string;
  name: string;
  snapshot: HardwareSnapshot;
}

/**
 * The listed machines with the latest snapshot read for each, kept after a machine stops being
 * readable. A machine that leaves the inventory is forgotten.
 */
export function useDescribedMachines(
  machines: readonly Machine[] | undefined,
  details: ReadonlyMap<string, MachineSnapshotDetail>,
): DescribedMachine[] {
  const latestRef = useRef<ReadonlyMap<string, HardwareSnapshot>>(new Map());
  const latest = useMemo(() => {
    const next = new Map<string, HardwareSnapshot>();
    for (const machine of machines ?? []) {
      const detail = details.get(machine.machineId);
      const snapshot =
        detail?.kind === "success" ? detail.snapshot : latestRef.current.get(machine.machineId);
      if (snapshot) next.set(machine.machineId, snapshot);
    }
    return next;
  }, [details, machines]);

  useEffect(() => {
    latestRef.current = latest;
  }, [latest]);

  return useMemo(
    () =>
      (machines ?? []).flatMap((machine) => {
        const snapshot = latest.get(machine.machineId);
        return snapshot
          ? [{ machineId: machine.machineId, name: machineName(machine), snapshot }]
          : [];
      }),
    [latest, machines],
  );
}

/** A machine's display name, or its id when it has none. */
export function machineName(machine: Machine): string {
  return machine.displayName || machine.machineId;
}
