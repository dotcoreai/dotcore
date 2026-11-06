import {
  inferenceCardSpecs,
  inferenceSplitIds,
  inferenceSplits,
} from "@/features/inference/inference-card-specs";
import type { MetricCardSpec, MetricCardSplit } from "@/features/metrics/use-metric-cards";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import { type MachineFacts, noMachineFacts } from "./machine-site-values";
import { appCardIds, appCardSpecs, appSplit, appSplitId } from "./overview-app-specs";
import {
  machineCardIds,
  machineCardSpecs,
  machineSplit,
  machineSplitId,
} from "./overview-machine-specs";

/** The ways the overview breaks a card's chart down, besides the whole site. */
export const overviewSplitIds = {
  ...inferenceSplitIds,
  app: appSplitId,
  machine: machineSplitId,
} as const;

/**
 * The splits that apply to a card, the first being the one choosing the card moves the chart to
 * when the current split belongs to another kind of card.
 */
export function cardSplitIds(cardId: string): readonly string[] {
  if ((machineCardIds as readonly string[]).includes(cardId)) return [machineSplitId];
  if ((appCardIds as readonly string[]).includes(cardId)) return [appSplitId];

  return [inferenceSplitIds.model, inferenceSplitIds.route];
}

/**
 * Every card the overview page reads in its one stream: the site's inference cards over all
 * routes, then the app cards, then the machine cards. Each section contributes its cards here, so the page opens a
 * single stream however many sections it shows.
 */
export function overviewCardSpecs(facts: MachineFacts = noMachineFacts): MetricCardSpec[] {
  return [...inferenceCardSpecs("routes"), ...appCardSpecs(facts), ...machineCardSpecs(facts)];
}

/**
 * The splits of the overview's cards: by model and route for the inference cards, by app for the
 * app cards, by machine for the machine cards. A group the console no longer lists keeps its id.
 */
export function overviewSplits(
  deployments: readonly ModelDeployment[] = [],
  routes: readonly InferenceRouteView[] = [],
  machines: readonly Machine[] = [],
  apps: readonly ApplicationDeployment[] = [],
): MetricCardSplit[] {
  return [...inferenceSplits(deployments, routes), appSplit(apps), machineSplit(machines)];
}
