import { describe, expect, it } from "vitest";

import { fillMetricGrid, fillMetricStatsGrid } from "./metric-grid";
import {
  counterPoint,
  distributionPoint,
  gaugePoint,
  gridStartSeconds,
  metricSeries,
  queryResult,
} from "./metric-test-support";

describe("fillMetricGrid", () => {
  it("fills absent steps with null instead of zero and keeps provisional flags", () => {
    const grid = fillMetricGrid(
      queryResult({
        series: [
          metricSeries({ node: "node-a" }, [
            gaugePoint(0, 10),
            gaugePoint(1, 0),
            gaugePoint(4, 40, true),
          ]),
        ],
        stepCount: 6,
      }),
    );

    expect(grid.startMs).toBe(gridStartSeconds * 1000);
    expect(grid.stepMs).toBe(1000);
    expect(grid.stepCount).toBe(6);
    expect(grid.series).toEqual([
      {
        labels: { node: "node-a" },
        provisional: [false, false, false, false, true, false],
        values: [10, 0, null, null, 40, null],
      },
    ]);
  });

  it("places points on the effective step grid and ignores points outside the covered range", () => {
    const grid = fillMetricGrid(
      queryResult({
        series: [
          metricSeries({}, [
            gaugePoint(-10, 1),
            gaugePoint(10, 2),
            gaugePoint(20, 3),
            gaugePoint(30, 9),
          ]),
        ],
        stepCount: 3,
        stepSeconds: 10,
      }),
    );

    expect(grid.stepMs).toBe(10_000);
    expect(grid.series[0].values).toEqual([null, 2, 3]);
  });

  it("reads counter points as their per-second rate and exposes the incarnation start", () => {
    const grid = fillMetricGrid(
      queryResult({
        incarnationId: "inc-7",
        series: [metricSeries({}, [counterPoint(1, 2.5)])],
        startedAtOffset: 2,
        stepCount: 3,
      }),
    );

    expect(grid.series[0].values).toEqual([null, 2.5, null]);
    expect(grid.incarnation).toEqual({ id: "inc-7", startedAtMs: (gridStartSeconds + 2) * 1000 });
  });

  it("treats non-finite readings as absent", () => {
    const grid = fillMetricGrid(
      queryResult({
        series: [
          metricSeries({}, [gaugePoint(0, Number.NaN), gaugePoint(1, Number.POSITIVE_INFINITY)]),
        ],
        stepCount: 2,
      }),
    );

    expect(grid.series[0].values).toEqual([null, null]);
  });

  it("splits a distribution series into one series per quantile and leaves empty buckets absent", () => {
    const grid = fillMetricGrid(
      queryResult({
        series: [
          metricSeries({}, [
            distributionPoint(0, { 0.5: 0.2, 0.95: 0.9 }),
            distributionPoint(2, { 0.5: 0.3, 0.95: 1.5 }),
          ]),
        ],
        stepCount: 3,
      }),
    );

    expect(grid.series).toEqual([
      { labels: {}, provisional: [false, false, false], quantile: 0.5, values: [0.2, null, 0.3] },
      { labels: {}, provisional: [false, false, false], quantile: 0.95, values: [0.9, null, 1.5] },
    ]);
  });
});

describe("fillMetricStatsGrid", () => {
  it("keeps the gauge value, counter delta and distribution count and sum per step", () => {
    const grid = fillMetricStatsGrid(
      queryResult({
        incarnationId: "inc-3",
        series: [
          metricSeries({ kind: "gauge" }, [gaugePoint(0, 4), gaugePoint(2, 0, true)]),
          metricSeries({ kind: "counter" }, [counterPoint(1, 0.5, false, 5)]),
          metricSeries({ kind: "latency" }, [
            distributionPoint(0, { 0.5: 1 }, false, { count: 3, sum: 12 }),
            distributionPoint(2, { 0.5: 1 }, true, { count: 1, sum: 2 }),
          ]),
        ],
        startedAtOffset: 1,
        stepCount: 3,
      }),
    );

    expect(grid.startMs).toBe(gridStartSeconds * 1000);
    expect(grid.stepMs).toBe(1000);
    expect(grid.stepCount).toBe(3);
    expect(grid.incarnation).toEqual({ id: "inc-3", startedAtMs: (gridStartSeconds + 1) * 1000 });
    expect(grid.series).toEqual([
      {
        labels: { kind: "gauge" },
        provisional: [false, false, true],
        steps: [{ kind: "gauge", value: 4 }, null, { kind: "gauge", value: 0 }],
      },
      {
        labels: { kind: "counter" },
        provisional: [false, false, false],
        steps: [null, { delta: 5, kind: "counter" }, null],
      },
      {
        labels: { kind: "latency" },
        provisional: [false, false, true],
        steps: [
          { count: 3, kind: "distribution", sum: 12 },
          null,
          { count: 1, kind: "distribution", sum: 2 },
        ],
      },
    ]);
  });

  it("places points on the effective step and ignores points outside the range or non-finite", () => {
    const grid = fillMetricStatsGrid(
      queryResult({
        series: [
          metricSeries({}, [
            gaugePoint(-10, 1),
            gaugePoint(10, 2),
            gaugePoint(20, Number.NaN),
            gaugePoint(40, 9),
          ]),
        ],
        stepCount: 3,
        stepSeconds: 10,
      }),
    );

    expect(grid.stepMs).toBe(10_000);
    expect(grid.series[0].steps).toEqual([null, { kind: "gauge", value: 2 }, null]);
  });
});
