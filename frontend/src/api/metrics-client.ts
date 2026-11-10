import { type Client, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import { resolveManagementOrigin } from "@/api/management-origin";
import { MetricsService } from "@/gen/dotcore/metrics/v1/service_pb";

export type MetricsServiceClient = Pick<
  Client<typeof MetricsService>,
  "listMetricDescriptors" | "watchMetrics"
>;

export interface MetricsClient {
  metrics: MetricsServiceClient;
}

const metricsServerUrl = resolveManagementOrigin(import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN, {
  allowTestFallback: import.meta.env.MODE === "test",
  allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
});

export function createMetricsClient(baseUrl = metricsServerUrl): MetricsClient {
  const transport = createConsoleTransport(baseUrl);
  const metrics = createClient(MetricsService, transport);

  return {
    metrics: {
      listMetricDescriptors: metrics.listMetricDescriptors,
      watchMetrics: metrics.watchMetrics,
    },
  };
}
