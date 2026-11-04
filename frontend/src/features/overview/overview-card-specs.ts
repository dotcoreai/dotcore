import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import {
  counterQuery,
  distributionQuery,
  requestCardIds,
  requestCardOrder,
  requestCardSpecs,
} from "@/features/metrics/request-card-specs";
import type { MetricCardSpec, MetricCardSplit } from "@/features/metrics/use-metric-cards";
import { modelDeploymentPresentationName } from "@/features/models/model-projection";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import { type MachineFacts, noMachineFacts } from "./machine-site-values";
import { machineCardSpecs, machineSplit, machineSplitId } from "./overview-machine-specs";

/** The ways the overview breaks a card's chart down, besides the whole site. */
export const overviewSplitIds = {
  machine: machineSplitId,
  model: "model",
  route: "route",
} as const;

/** The ids of the inference cards, in the order the section shows them: the sidebars' order. */
export const inferenceCardIds = requestCardOrder;

/**
 * Every card the overview page reads in its one stream. Each section contributes its cards here, so
 * the page opens a single stream however many sections it shows.
 */
export function overviewCardSpecs(facts: MachineFacts = noMachineFacts): MetricCardSpec[] {
  return [...inferenceCardSpecs(), ...machineCardSpecs(facts)];
}

/**
 * The six site inference cards over all routes and deployments, the request cards of the sidebars
 * in the same order: request counts and time to first token from the request metrics, tokens and
 * speeds from the attempt metrics. They split by model and route only. Broken down by model,
 * requests and time to first token read the attempt metrics, since only attempts name a
 * deployment, so a model's count includes the retries and fallbacks that reached it.
 */
function inferenceCardSpecs(): MetricCardSpec[] {
  const byModel: Record<string, MetricCardSpec["splitQueries"]> = {
    [requestCardIds.requests]: {
      [overviewSplitIds.model]: counterQuery(metricNames.routerAttempts, {}),
    },
    [requestCardIds.ttft]: {
      [overviewSplitIds.model]: distributionQuery(metricNames.routerAttemptTtft),
    },
  };

  return requestCardSpecs({
    matchers: {},
    requests: metricNames.routerRequests,
    ttft: metricNames.routerRequestTtft,
  }).map((spec) => ({
    ...spec,
    splitQueries: byModel[spec.id],
    splits: [overviewSplitIds.model, overviewSplitIds.route],
  }));
}

/**
 * The splits of the overview's cards. Groups are named, and colored, like the deployments, routes
 * and machines the console lists; a group the console no longer lists keeps its id.
 */
export function overviewSplits(
  deployments: readonly ModelDeployment[] = [],
  routes: readonly InferenceRouteView[] = [],
  machines: readonly Machine[] = [],
): MetricCardSplit[] {
  const deploymentById = new Map(
    deployments.map((deployment) => [deployment.modelDeploymentId, deployment]),
  );
  const routeNameById = new Map(
    routes.flatMap((view) =>
      view.route ? [[view.route.inferenceRouteId, view.route.name] as const] : [],
    ),
  );

  return [
    {
      groupBy: [dimensionNames.deployment],
      id: overviewSplitIds.model,
      label: "By model",
      seriesColorKey: (labels) => {
        const deployment = deploymentById.get(labels[dimensionNames.deployment] ?? "");
        return deployment
          ? deployment.modelDeploymentId
          : (labels[dimensionNames.deployment] ?? "");
      },
      seriesLabel: (labels) => {
        const id = labels[dimensionNames.deployment] ?? "";
        const deployment = deploymentById.get(id);
        return deployment ? modelDeploymentPresentationName(deployment) : id;
      },
    },
    {
      groupBy: [dimensionNames.route],
      id: overviewSplitIds.route,
      label: "By route",
      seriesLabel: (labels) => {
        const id = labels[dimensionNames.route] ?? "";
        return routeNameById.get(id) ?? id;
      },
    },
    machineSplit(machines),
  ];
}
