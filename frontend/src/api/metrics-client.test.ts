import { beforeEach, describe, expect, it, vi } from "vitest";

const { createGrpcWebTransport, createClient, metricsService } = vi.hoisted(() => {
  const metricsService = { listMetricDescriptors: vi.fn(), watchMetrics: vi.fn() };

  return {
    createGrpcWebTransport: vi.fn(() => ({}) as never),
    createClient: vi.fn(() => metricsService),
    metricsService,
  };
});

vi.mock("@connectrpc/connect-web", () => ({ createGrpcWebTransport }));
vi.mock("@connectrpc/connect", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@connectrpc/connect")>()),
  createClient,
}));

describe("Metrics client", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
  });

  it("creates a MetricsService client over the shared console transport", async () => {
    const { MetricsService } = await import("@/gen/dotcore/metrics/v1/service_pb");
    const { createMetricsClient } = await import("./metrics-client");
    const client = createMetricsClient("http://100.64.0.7:6207");
    const request = { query: { metric: "node.host.memory.total" } };
    metricsService.watchMetrics.mockReturnValue("stream");

    expect(client.metrics.watchMetrics(request as never)).toBe("stream");

    expect(createGrpcWebTransport).toHaveBeenCalledWith({
      baseUrl: "http://100.64.0.7:6207",
      interceptors: [expect.any(Function)],
    });
    expect(createClient).toHaveBeenCalledWith(MetricsService, expect.anything());
    expect(metricsService.watchMetrics).toHaveBeenCalledWith(request);
  });
});
