import { create } from "@bufbuild/protobuf";
import { act, render, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { ResourceWatchProvider } from "@/app/resource-watch-provider";
import {
  gaugePoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import {
  ListHardwareSnapshotsResponseSchema,
  ListMachinesResponseSchema,
  MachineSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { overviewSplitIds } from "./overview-card-specs";
import { type OverviewMetrics, useOverviewMetrics } from "./use-overview-metrics";

describe("useOverviewMetrics", () => {
  it("moves the split to the kind of the selected card and keeps one stream", async () => {
    const watchMetrics = vi.fn(
      staticWatch(() =>
        queryResult({
          series: [metricSeries({ node: "spark" }, [gaugePoint(0, 0.5)])],
          stepCount: 1,
        }),
      ),
    );
    const metrics = renderHook(watchMetrics);
    await waitFor(() => expect(metrics.current.cards[0].reading).toBeDefined());
    expect(metrics.current.split).toBe(overviewSplitIds.model);

    act(() => metrics.current.setSelected("cpu"));
    expect(metrics.current.split).toBe(overviewSplitIds.machine);

    // Total stays Total across cards of either kind.
    act(() => metrics.current.setSplit(undefined));
    act(() => metrics.current.setSelected("requests"));
    expect(metrics.current.split).toBeUndefined();

    act(() => metrics.current.setSplit(overviewSplitIds.machine));
    act(() => metrics.current.setSelected("requests"));
    expect(metrics.current.split).toBe(overviewSplitIds.model);

    act(() => metrics.current.setSelected("app-cpu"));
    expect(metrics.current.split).toBe(overviewSplitIds.app);

    act(() => metrics.current.setSelected("cpu"));
    expect(metrics.current.split).toBe(overviewSplitIds.machine);
    expect(watchMetrics).toHaveBeenCalledTimes(1);
  });

  it("lists the watched machines in the fleet", async () => {
    const metrics = renderHook(staticWatch(() => queryResult({ series: [], stepCount: 1 })));

    await waitFor(() =>
      expect(metrics.current.fleet.machines?.map((machine) => machine.machineId)).toEqual([
        "spark",
      ]),
    );
  });
});

function renderHook(watchMetrics: Parameters<typeof metricsClientWith>[0]) {
  const metrics = {} as { current: OverviewMetrics };
  const client = metricsClientWith(watchMetrics);
  const watch = new FakeResourceWatch({
    sources: {
      hardwareSnapshots: () => Promise.resolve(create(ListHardwareSnapshotsResponseSchema)),
      machines: () =>
        Promise.resolve(
          create(ListMachinesResponseSchema, {
            machines: [create(MachineSchema, { displayName: "Spark", machineId: "spark" })],
          }),
        ),
    },
  });
  function Probe() {
    metrics.current = useOverviewMetrics(client);
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
