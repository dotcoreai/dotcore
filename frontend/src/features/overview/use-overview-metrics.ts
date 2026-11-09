import { useMemo } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { type MachineFleet, useMachineFleet } from "@/features/machines/use-machine-fleet";
import { useHardwareSnapshots } from "@/features/memory/use-hardware-snapshots";
import { type MetricCards, useMetricCards } from "@/features/metrics/use-metric-cards";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import {
  useWatchedApplicationDeployments,
  useWatchedMachines,
  useWatchedModelDeployments,
  useWatchedRoutes,
} from "@/resource-watch/resource-watch";

import {
  cardSplitIds,
  overviewCardSpecs,
  overviewSplitIds,
  overviewSplits,
} from "./overview-card-specs";

/** The overview's cards, and the machines they combine over. */
export interface OverviewMetrics extends MetricCards {
  fleet: MachineFleet;
}

const noMachines: readonly Machine[] = [];

/**
 * The overview page's metric state: one card stream at the page range carrying every section's
 * cards and split groupings, so choosing a card or a split never reopens it and a range change
 * reopens it once. Splits are named from the watched deployments, routes, machines and apps. The
 * model split is the default; choosing a card of another kind moves the split to that kind's own,
 * since a split applies to one kind of card only.
 */
export function useOverviewMetrics(client: MetricsClient): OverviewMetrics {
  const deployments = useWatchedModelDeployments()?.value?.deployments;
  const routes = useWatchedRoutes()?.value?.routes;
  const machines = useWatchedMachines()?.value?.machines;
  const apps = useWatchedApplicationDeployments()?.value?.deployments;
  const details = useHardwareSnapshots(machines ?? noMachines);
  const fleet = useMachineFleet(machines, details);
  const specs = useMemo(() => overviewCardSpecs(fleet.facts), [fleet.facts]);
  const splits = useMemo(
    () => overviewSplits(deployments, routes, fleet.machines, apps),
    [apps, deployments, fleet.machines, routes],
  );
  const metrics = useMetricCards(client, specs, {
    initialSplit: overviewSplitIds.model,
    splits,
  });
  const { setSelected, setSplit, split } = metrics;

  const select = (id: string | undefined) => {
    if (id !== undefined && split !== undefined) {
      const splits = cardSplitIds(id);
      if (!splits.includes(split)) setSplit(splits[0]);
    }
    setSelected(id);
  };

  return { ...metrics, fleet, setSelected: select };
}
