import {
  inferenceCardSpecs,
  inferenceSplitIds,
  inferenceSplits,
} from "@/features/inference/inference-card-specs";
import type { MetricCardSpec, MetricCardSplit } from "@/features/metrics/use-metric-cards";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import { type MachineFacts, noMachineFacts } from "./machine-site-values";
import { machineCardSpecs, machineSplit, machineSplitId } from "./overview-machine-specs";

/** The ways the overview breaks a card's chart down, besides the whole site. */
export const overviewSplitIds = {
  ...inferenceSplitIds,
  machine: machineSplitId,
} as const;

/**
 * Every card the overview page reads in its one stream: the site's inference cards over all
 * routes, then the machine cards. Each section contributes its cards here, so the page opens a
 * single stream however many sections it shows.
 */
export function overviewCardSpecs(facts: MachineFacts = noMachineFacts): MetricCardSpec[] {
  return [...inferenceCardSpecs("routes"), ...machineCardSpecs(facts)];
}

/**
 * The splits of the overview's cards: by model and route for the inference cards, by machine for
 * the machine cards. A group the console no longer lists keeps its id.
 */
export function overviewSplits(
  deployments: readonly ModelDeployment[] = [],
  routes: readonly InferenceRouteView[] = [],
  machines: readonly Machine[] = [],
): MetricCardSplit[] {
  return [...inferenceSplits(deployments, routes), machineSplit(machines)];
}
