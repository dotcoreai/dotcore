import type { MetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import type { RouteClient } from "@/api/route-client";
import { OverviewPage } from "@/features/overview/overview-page";

export function OverviewRoute({
  metricsClient,
  modelClient,
  routeClient,
}: {
  metricsClient?: MetricsClient;
  modelClient?: ModelClient;
  routeClient?: RouteClient;
}) {
  return (
    <OverviewPage
      metricsClient={metricsClient}
      modelClient={modelClient}
      routeClient={routeClient}
    />
  );
}
