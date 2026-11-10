import type { MessageInitShape } from "@bufbuild/protobuf";
import { act, render, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import {
  counterPoint,
  distributionPoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import type { InferenceScope } from "./inference-card-specs";
import { type InferenceMetrics, useInferenceMetrics } from "./use-inference-metrics";

describe("useInferenceMetrics", () => {
  it("reads each deployment's traffic from the attempts that reached it", async () => {
    const watchMetrics = vi.fn(staticWatch(trafficAnswer));
    const metrics = renderHook(watchMetrics, "deployments");

    expect(metrics.current.rowValues("dep-a")).toBeUndefined();
    await waitFor(() => expect(metrics.current.rowValues("dep-a")).toBeDefined());

    expect(metrics.current.rowValues("dep-a")).toEqual({
      generatedTokens: 30,
      requests: 3,
      tokenGeneration: 50,
      ttft: 0.25,
    });
    expect(metrics.current.rowValues("dep-b")).toEqual({
      generatedTokens: 10,
      requests: 1,
      tokenGeneration: null,
      ttft: null,
    });
    // A deployment the answer leaves out had no traffic.
    expect(metrics.current.rowValues("dep-idle")).toEqual({
      generatedTokens: 0,
      requests: 0,
      tokenGeneration: null,
      ttft: null,
    });
    expect(metrics.current.cards.find((card) => card.id === "requests")?.reading?.value).toBe(4);
    expect(watchMetrics).toHaveBeenCalledTimes(1);
  });

  it("reads each route's traffic from the requests it routed", async () => {
    const metrics = renderHook(staticWatch(trafficAnswer), "routes");
    await waitFor(() => expect(metrics.current.rowValues("route-a")).toBeDefined());

    expect(metrics.current.rowValues("route-a")).toEqual({
      generatedTokens: 40,
      requests: 2,
      tokenGeneration: 50,
      ttft: 0.5,
    });
    expect(metrics.current.cards.find((card) => card.id === "requests")?.reading?.value).toBe(2);
  });

  it("updates the cards and each row when the range changes", async () => {
    const watchMetrics = vi.fn(staticWatch(trafficAnswer));
    const metrics = renderHook(watchMetrics, "routes");
    await waitFor(() => expect(metrics.current.rowValues("route-a")?.requests).toBe(2));

    act(() => metrics.current.setRange("6h"));

    await waitFor(() => expect(metrics.current.rowValues("route-a")?.requests).toBe(20));
    expect(metrics.current.range?.label).toBe("6h");
    expect(metrics.current.rowValues("route-a")?.generatedTokens).toBe(400);
    expect(metrics.current.cards.find((card) => card.id === "requests")?.reading?.value).toBe(20);
    expect(watchMetrics).toHaveBeenCalledTimes(2);
  });
});

function renderHook(
  watchMetrics: Parameters<typeof metricsClientWith>[0],
  scope: InferenceScope,
): { current: InferenceMetrics } {
  const metrics = {} as { current: InferenceMetrics };
  const client = metricsClientWith(watchMetrics);
  const watch = new FakeResourceWatch();
  function Probe() {
    metrics.current = useInferenceMetrics(client, scope);
    return null;
  }
  act(() => {
    render(
      <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
        <Probe />
      </ResourceWatchProvider>,
    );
  });

  return metrics;
}

/**
 * Traffic over the range: two requests on one route, reaching two deployments in four attempts
 * with 40 completion tokens. The six-hour range reads ten times the counts.
 */
function trafficAnswer(query: MessageInitShape<typeof MetricQuerySchema> | undefined) {
  const scale = query?.range?.case === "last" && query.range.value.seconds === 21_600n ? 10 : 1;
  const groupBy = query?.groupBy ?? [];
  const counter = (count: number, labels: Record<string, string> = {}) =>
    metricSeries(labels, [counterPoint(0, 0, false, count * scale)]);
  const mean = (sum: number, count: number, labels: Record<string, string> = {}) =>
    metricSeries(labels, [distributionPoint(0, { 0.95: sum }, false, { count, sum })]);
  const series = (() => {
    switch (query?.metric) {
      case "router.requests":
        return [counter(2, groupBy.includes("route") ? { route: "route-a" } : {})];
      case "router.attempts":
        if (groupBy.includes("deployment")) {
          return [counter(3, { deployment: "dep-a" }), counter(1, { deployment: "dep-b" })];
        }
        return [counter(4, groupBy.includes("route") ? { route: "route-a" } : {})];
      case "router.tokens.completion":
        if (groupBy.includes("deployment")) {
          return [counter(30, { deployment: "dep-a" }), counter(10, { deployment: "dep-b" })];
        }
        return [counter(40, groupBy.includes("route") ? { route: "route-a" } : {})];
      case "router.attempt.decode_rate":
        if (groupBy.includes("deployment")) return [mean(200, 4, { deployment: "dep-a" })];
        return [mean(200, 4, groupBy.includes("route") ? { route: "route-a" } : {})];
      case "router.attempt.ttft":
        if (groupBy.includes("deployment")) return [mean(0.5, 2, { deployment: "dep-a" })];
        return [mean(0.5, 2)];
      case "router.request.ttft":
        return [mean(1, 2, groupBy.includes("route") ? { route: "route-a" } : {})];
      default:
        return [];
    }
  })();

  return queryResult({ series, stepCount: 60, stepSeconds: 15 });
}
