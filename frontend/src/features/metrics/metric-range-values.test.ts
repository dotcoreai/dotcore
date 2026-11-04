import { describe, expect, it } from "vitest";

import { fillMetricStatsGrid, type MetricStatsGrid } from "./metric-grid";
import {
  counterTotal,
  coveredTime,
  gaugeRange,
  perStepValues,
  weightedMean,
} from "./metric-range-values";
import {
  counterPoint,
  distributionPoint,
  gaugePoint,
  gridStartSeconds,
  metricSeries,
  queryResult,
} from "./metric-test-support";

describe("counterTotal", () => {
  it("sums the deltas across steps, including the provisional one", () => {
    const grid = statsGrid(
      [counterPoint(0, 1, false, 4), counterPoint(1, 1, false, 6), counterPoint(3, 1, true, 2)],
      4,
    );

    expect(counterTotal(grid.series[0].steps)).toBe(12);
  });

  it("reads zero for an idle counter and null when the range holds no point", () => {
    expect(counterTotal(statsGrid([counterPoint(1, 0, false, 0)], 3).series[0].steps)).toBe(0);
    expect(counterTotal(statsGrid([], 3).series[0].steps)).toBeNull();
  });
});

describe("weightedMean", () => {
  it("divides the summed sums by the summed counts, not the mean of step means", () => {
    const grid = statsGrid(
      [
        distributionPoint(0, {}, false, { count: 1, sum: 10 }),
        distributionPoint(2, {}, false, { count: 9, sum: 18 }),
      ],
      3,
    );

    expect(weightedMean(grid.series[0].steps)).toBe(2.8);
  });

  it("is null without samples", () => {
    expect(weightedMean(statsGrid([], 3).series[0].steps)).toBeNull();
    expect(
      weightedMean(
        statsGrid([distributionPoint(0, {}, false, { count: 0, sum: 0 })], 2).series[0].steps,
      ),
    ).toBeNull();
  });
});

describe("gaugeRange", () => {
  it("averages the per-step means and takes the peak from the per-step highs, skipping gaps", () => {
    expect(gaugeRange([2, null, 8, 5, null, 0], [3, null, 9, 12, null, 1])).toEqual({
      average: 3.75,
      peak: 12,
    });
  });

  it("reads the peak from the means without highs, and keeps a zero apart from no data", () => {
    expect(gaugeRange([0, null, 4])).toEqual({ average: 2, peak: 4 });
    expect(gaugeRange([null, 0])).toEqual({ average: 0, peak: 0 });
    expect(gaugeRange([null, null])).toEqual({ average: null, peak: null });
  });
});

describe("coveredTime", () => {
  // A clock past every range below, so they end at their last step.
  const afterRange = (gridStartSeconds + 10_000) * 1000;

  it("is the whole range when the incarnation began before it", () => {
    const grid = statsGrid([], 10, { startedAtOffset: -50 });

    expect(coveredTime(grid, afterRange)).toEqual({ ms: 10_000, startsInside: false });
  });

  it("clips the range at an incarnation that began inside it", () => {
    const grid = statsGrid([], 10, { startedAtOffset: 4 });

    expect(coveredTime(grid, afterRange)).toEqual({ ms: 6000, startsInside: true });
  });

  it("uses the step the response reports, not the requested hint", () => {
    const grid = statsGrid([], 4, { startedAtOffset: 60, stepSeconds: 360 });

    expect(grid.stepMs).toBe(360_000);
    expect(coveredTime(grid, afterRange)).toEqual({ ms: 4 * 360_000 - 60_000, startsInside: true });
  });

  it("ends at now, not at the end of the open step", () => {
    // Six-minute steps; the store restarted a minute into the last one, two minutes ago.
    const grid = statsGrid([], 4, { startedAtOffset: 3 * 360 + 60, stepSeconds: 360 });
    const nowMs = (gridStartSeconds + 3 * 360 + 180) * 1000;

    expect(coveredTime(grid, nowMs)).toEqual({ ms: 120_000, startsInside: true });
  });

  it("is zero when the incarnation began after the range", () => {
    expect(coveredTime(statsGrid([], 10, { startedAtOffset: 20 }), afterRange).ms).toBe(0);
  });
});

describe("perStepValues", () => {
  it("gives gauge values, counter deltas and distribution means with gaps kept", () => {
    const gauge = statsGrid([gaugePoint(0, 3), gaugePoint(2, 0)], 3);
    const counter = statsGrid([counterPoint(1, 1, false, 7)], 3);
    const latency = statsGrid(
      [
        distributionPoint(0, {}, false, { count: 4, sum: 10 }),
        distributionPoint(2, {}, false, { count: 0, sum: 0 }),
      ],
      3,
    );

    expect(perStepValues(gauge.series[0].steps)).toEqual([3, null, 0]);
    expect(perStepValues(counter.series[0].steps)).toEqual([null, 7, null]);
    expect(perStepValues(latency.series[0].steps)).toEqual([2.5, null, null]);
  });
});

function statsGrid(
  points: Parameters<typeof metricSeries>[1],
  stepCount: number,
  options: { startedAtOffset?: number; stepSeconds?: number } = {},
): MetricStatsGrid {
  return fillMetricStatsGrid(
    queryResult({
      series: [metricSeries({}, points)],
      startedAtOffset: options.startedAtOffset,
      stepCount,
      stepSeconds: options.stepSeconds,
    }),
  );
}
