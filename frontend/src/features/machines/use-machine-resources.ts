import { useMemo } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import type { DescribedMachine } from "@/features/memory/described-machines";
import type { MachineSnapshotDetail } from "@/features/memory/use-hardware-snapshots";
import { type MemoryHistory, useMemoryHistory } from "@/features/memory/use-memory-history";
import { type SiteMemoryState, siteMemoryState } from "@/features/memory/use-site-memory";
import { type MetricPanel, useMetricPanel } from "@/features/metrics/use-metric-panel";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import type { NodeMemory } from "@/gen/dotcore/capacity/v1/memory_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import type { ReadState } from "@/lib/read-state";

import {
  countMachines,
  resourceQueryGroups,
  resourcesCurrentWindow,
  type UncountedMachine,
} from "./machine-resources";

/** What the Machines page shows about its machines' resources, from reads it already holds. */
export interface MachineResources {
  counted: DescribedMachine[];
  /**
   * Described machines and their names, for history. A described machine's
   * hardware shapes history and the queries, so a machine that drops out keeps its past.
   */
  history: MemoryHistory;
  /** Site memory of the counted machines only, so its rows and the other rows agree. */
  memory: SiteMemoryState;
  /** Current values for the section, and history while one of its rows is expanded. */
  panel: MetricPanel;
  uncounted: UncountedMachine[];
}

/**
 * The resources of the listed machines: which are counted now, their memory as the capacity
 * component draws it, and the metric panel of the resources section.
 */
export function useMachineResources(
  metricsClient: MetricsClient,
  machines: readonly Machine[] | undefined,
  machinesReadFailed: boolean,
  details: ReadonlyMap<string, MachineSnapshotDetail>,
  deployments: ReadState<ModelDeployment[]> | undefined,
  applications: ReadState<ApplicationDeployment[]> | undefined,
  nodeMemory: ReadState<readonly NodeMemory[]> | undefined,
): MachineResources {
  const { counted, uncounted } = useMemo(
    () => countMachines(machines ?? [], details),
    [details, machines],
  );
  const history = useMemoryHistory(machines, details);
  const countedMachines = useMemo(
    () =>
      machines?.filter((machine) => counted.some((entry) => entry.machineId === machine.machineId)),
    [counted, machines],
  );
  const memory = useMemo(
    () =>
      siteMemoryState({
        applications: applications?.value,
        deployments: deployments?.value,
        deploymentsReadFailed: Boolean(deployments?.failed || applications?.failed),
        machines: countedMachines,
        machinesReadFailed,
        nodeMemory,
      }),
    [applications, countedMachines, deployments, machinesReadFailed, nodeMemory],
  );
  const groups = useMemo(() => resourceQueryGroups(history.described), [history.described]);
  const panel = useMetricPanel(metricsClient, groups, resourcesCurrentWindow);

  return { counted, history, memory, panel, uncounted };
}
