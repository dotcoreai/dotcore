import { describe, expect, it } from "vitest";

import { counterPoint, gaugePoint, metricSeries, queryResult } from "./metric-test-support";
import { MetricWatchAccumulator } from "./metric-watch";

describe("MetricWatchAccumulator", () => {
  it("returns the same grid objects until an upsert changes it", () => {
    const accumulator = new MetricWatchAccumulator();
    accumulator.apply(
      queryResult({ series: [metricSeries({}, [gaugePoint(0, 1)])], stepCount: 2 }),
    );

    const grid = accumulator.grid();
    const statsGrid = accumulator.statsGrid();
    expect(accumulator.grid()).toBe(grid);
    expect(accumulator.statsGrid()).toBe(statsGrid);

    accumulator.apply(
      queryResult({ series: [metricSeries({}, [gaugePoint(1, 2)])], stepCount: 2 }),
    );
    expect(accumulator.grid()).not.toBe(grid);
    expect(accumulator.grid()?.series[0].values).toEqual([1, 2]);
    expect(accumulator.statsGrid()).not.toBe(statsGrid);
  });

  it("projects the held counter deltas on the stats grid", () => {
    const accumulator = new MetricWatchAccumulator();
    accumulator.apply(
      queryResult({ series: [metricSeries({}, [counterPoint(0, 1, false, 4)])], stepCount: 2 }),
    );
    accumulator.apply(
      queryResult({ series: [metricSeries({}, [counterPoint(1, 1, true, 3)])], stepCount: 2 }),
    );

    expect(accumulator.statsGrid()?.series[0].steps).toEqual([
      { delta: 4, kind: "counter" },
      { delta: 3, kind: "counter" },
    ]);
  });

  it("has no grid before the first upsert", () => {
    const accumulator = new MetricWatchAccumulator();

    expect(accumulator.grid()).toBeUndefined();
    expect(accumulator.statsGrid()).toBeUndefined();
  });
});
