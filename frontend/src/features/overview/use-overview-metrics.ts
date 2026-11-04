import { useMemo } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import {
  useWatchedMachines,
  useWatchedModelDeployments,
  useWatchedRoutes,
} from "@/app/resource-watch";
import { machineSummary } from "@/features/machines/machine-resources";
import { useDescribedMachines } from "@/features/memory/described-machines";
import { useHardwareSnapshots } from "@/features/memory/use-hardware-snapshots";
import { type MetricCards, useMetricCards } from "@/features/metrics/use-metric-cards";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import {
  type MachineFacts,
  machineFacts,
  machineFactsFromKey,
  machineFactsKey,
} from "./machine-site-values";
import { overviewCardSpecs, overviewSplitIds, overviewSplits } from "./overview-card-specs";
import { machineCardIds } from "./overview-machine-specs";

/** The machines the overview lists and the facts of their hardware its readings combine by. */
export interface OverviewFleet {
  facts: MachineFacts;
  /** Whether the facts still wait on a machine's hardware; they say nothing until then. */
  factsPending: boolean;
  /** The listed machines; absent until the console has read them. */
  machines: readonly Machine[] | undefined;
  /** Each described machine's hardware in a line, such as its GPU models and CPU count. */
  summaries: ReadonlyMap<string, string>;
}

/** The overview's cards, and the machines they combine over. */
export interface OverviewMetrics extends MetricCards {
  fleet: OverviewFleet;
}

const noMachines: readonly Machine[] = [];

/**
 * The overview page's metric state: one card stream at the page range carrying every section's
 * cards and split groupings, so choosing a card or a split never reopens it and a range change
 * reopens it once. Splits are named from the watched deployments, routes and machines. The model
 * split is the default; choosing a card of the other kind moves the split to that kind's own, since
 * a split applies to one kind of card only.
 */
export function useOverviewMetrics(client: MetricsClient): OverviewMetrics {
  const deployments = useWatchedModelDeployments()?.value?.deployments;
  const routes = useWatchedRoutes()?.value?.routes;
  const fleet = useOverviewFleet();
  const specs = useMemo(() => overviewCardSpecs(fleet.facts), [fleet.facts]);
  const splits = useMemo(
    () => overviewSplits(deployments, routes, fleet.machines),
    [deployments, fleet.machines, routes],
  );
  const metrics = useMetricCards(client, specs, {
    initialSplit: overviewSplitIds.model,
    splits,
  });
  const { setSelected, setSplit, split } = metrics;

  const select = (id: string | undefined) => {
    if (id !== undefined && split !== undefined) {
      const machineCard = (machineCardIds as readonly string[]).includes(id);
      if (machineCard && split !== overviewSplitIds.machine) setSplit(overviewSplitIds.machine);
      if (!machineCard && split === overviewSplitIds.machine) setSplit(overviewSplitIds.model);
    }
    setSelected(id);
  };

  return { ...metrics, fleet, setSelected: select };
}

/**
 * The listed machines with the facts of their latest snapshots, pending until the machines and
 * each one's first snapshot are read. The facts keep their identity while they stay equal, so a
 * snapshot refresh does not rebuild what reads them.
 */
function useOverviewFleet(): OverviewFleet {
  const machines = useWatchedMachines()?.value?.machines;
  const details = useHardwareSnapshots(machines ?? noMachines);
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
