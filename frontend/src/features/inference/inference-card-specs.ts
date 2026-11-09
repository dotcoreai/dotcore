import { modelDeploymentPresentationName } from "@/domain/model-projection";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import {
  counterQuery,
  distributionQuery,
  requestCardIds,
  requestCardOrder,
  requestCardSpecs,
} from "@/features/metrics/request-card-specs";
import type { MetricCardSpec, MetricCardSplit } from "@/features/metrics/use-metric-cards";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";

/**
 * What a page's inference cards count: `routes` counts the requests routes handled, as the
 * overview does; `deployments` counts the attempts that reached model deployments, so retries and
 * fallbacks count once per deployment they reached.
 */
export type InferenceScope = "deployments" | "routes";

/** The ways inference cards break their chart down, besides the whole site. */
export const inferenceSplitIds = {
  model: "model",
  route: "route",
} as const;

/** The ids of the inference cards, in the order the section shows them: the sidebars' order. */
export const inferenceCardIds = requestCardOrder;

/**
 * The six site inference cards over all routes and deployments, the request cards of the sidebars
 * in the same order, split by model and route only. Scoped to routes, requests and time to first
 * token read the request metrics; broken down by model they read the attempt metrics, since only
 * attempts name a deployment, so a model's count includes the retries and fallbacks that reached
 * it. Scoped to deployments, every card reads the attempt metrics.
 */
export function inferenceCardSpecs(scope: InferenceScope): MetricCardSpec[] {
  const splits = [inferenceSplitIds.model, inferenceSplitIds.route];

  if (scope === "deployments") {
    return requestCardSpecs({
      matchers: {},
      requests: metricNames.routerAttempts,
      ttft: metricNames.routerAttemptTtft,
    }).map((spec) => ({ ...spec, splits }));
  }

  const byModel: Record<string, MetricCardSpec["splitQueries"]> = {
    [requestCardIds.requests]: {
      [inferenceSplitIds.model]: counterQuery(metricNames.routerAttempts, {}),
    },
    [requestCardIds.ttft]: {
      [inferenceSplitIds.model]: distributionQuery(metricNames.routerAttemptTtft),
    },
  };

  return requestCardSpecs({
    matchers: {},
    requests: metricNames.routerRequests,
    ttft: metricNames.routerRequestTtft,
  }).map((spec) => ({ ...spec, splitQueries: byModel[spec.id], splits }));
}

/**
 * The model and route splits of inference cards. Groups are named, and colored, like the
 * deployments and routes the console lists; a group the console no longer lists keeps its id.
 */
export function inferenceSplits(
  deployments: readonly ModelDeployment[] = [],
  routes: readonly InferenceRouteView[] = [],
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
      id: inferenceSplitIds.model,
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
      id: inferenceSplitIds.route,
      label: "By route",
      seriesLabel: (labels) => {
        const id = labels[dimensionNames.route] ?? "";
        return routeNameById.get(id) ?? id;
      },
    },
  ];
}
