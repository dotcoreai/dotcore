import { useMemo } from "react";

import { machineSummary } from "@/features/machines/machine-resources";
import { useDescribedMachines } from "@/features/memory/described-machines";
import type { MachineSnapshotDetail } from "@/features/memory/use-hardware-snapshots";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import {
  type MachineFacts,
  machineFacts,
  machineFactsFromKey,
  machineFactsKey,
} from "./machine-site-values";

/** The listed machines and the facts of their hardware that readings combine by. */
export interface MachineFleet {
  facts: MachineFacts;
  /** Whether the facts still wait on a machine's hardware; they say nothing until then. */
  factsPending: boolean;
  /** The listed machines; absent until the console has read them. */
  machines: readonly Machine[] | undefined;
  /** Each described machine's hardware in a line, such as its GPU models and CPU count. */
  summaries: ReadonlyMap<string, string>;
}

/**
 * The listed machines with the facts of their latest snapshots, pending until the machines and
 * each one's first snapshot are read. The facts keep their identity while they stay equal, so a
 * snapshot refresh does not rebuild what reads them.
 */
export function useMachineFleet(
  machines: readonly Machine[] | undefined,
  details: ReadonlyMap<string, MachineSnapshotDetail>,
): MachineFleet {
  const described = useDescribedMachines(machines, details);
  const key = machineFactsKey(useMemo(() => machineFacts(described), [described]));
  const facts = useMemo(() => machineFactsFromKey(key), [key]);
  const factsPending =
    machines === undefined || [...details.values()].some((detail) => detail.kind === "loading");

  const summaries = useMemo(
    () =>
      new Map(described.map((machine) => [machine.machineId, machineSummary(machine.snapshot)])),
    [described],
  );

  return useMemo(
    () => ({ facts, factsPending, machines, summaries }),
    [facts, factsPending, machines, summaries],
  );
}
