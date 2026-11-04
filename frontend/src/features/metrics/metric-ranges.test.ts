import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { MetricResolutionSchema } from "@/gen/dotcore/metrics/v1/service_pb";

import { fillMetricStatsGrid } from "./metric-grid";
import { cardRanges, metricRangesFromResolutions } from "./metric-ranges";
import { counterPoint, metricSeries, queryResult } from "./metric-test-support";

const resolution = (stepSeconds: number, retentionSeconds: number) =>
  create(MetricResolutionSchema, {
    retention: { seconds: BigInt(retentionSeconds) },
    step: { seconds: BigInt(stepSeconds) },
  });

describe("metricRangesFromResolutions", () => {
  it("offers one range per advertised resolution, finest first", () => {
    expect(
      metricRangesFromResolutions([
        resolution(1, 900),
        resolution(10, 21_600),
        resolution(60, 172_800),
      ]),
    ).toEqual([
      { durationMs: 900_000, label: "15m", stepMs: 1000 },
      { durationMs: 21_600_000, label: "6h", stepMs: 10_000 },
      { durationMs: 172_800_000, label: "48h", stepMs: 60_000 },
    ]);
  });

  it("skips resolutions without a usable step or retention", () => {
    expect(metricRangesFromResolutions([resolution(0, 900), resolution(10, 0)])).toEqual([]);
  });
});

describe("cardRanges", () => {
  it("hints a sixtieth of each advertised range for the default resolutions", () => {
    expect(
      cardRangesOf([resolution(1, 900), resolution(10, 21_600), resolution(60, 172_800)]),
    ).toEqual([
      { durationMs: 900_000, label: "15m", stepMs: 15_000 },
      { durationMs: 21_600_000, label: "6h", stepMs: 360_000 },
      { durationMs: 172_800_000, label: "48h", stepMs: 2_880_000 },
    ]);
  });

  it("follows a non-default resolution list", () => {
    expect(cardRangesOf([resolution(5, 3600), resolution(30, 43_200)])).toEqual([
      { durationMs: 3_600_000, label: "1h", stepMs: 60_000 },
      { durationMs: 43_200_000, label: "12h", stepMs: 720_000 },
    ]);
  });

  it("rounds a hint that is not a whole millisecond up", () => {
    expect(cardRangesOf([resolution(1, 1)])[0].stepMs).toBe(17);
  });

  it("skips resolutions without a usable step or retention", () => {
    expect(cardRangesOf([resolution(0, 900), resolution(10, 0)])).toEqual([]);
  });

  it("lays answers out on the step the response reports, not the hint", () => {
    const [range] = cardRangesOf([resolution(1, 900)]);
    const grid = fillMetricStatsGrid(
      queryResult({
        series: [metricSeries({}, [counterPoint(40, 1, false, 9)])],
        stepCount: 60,
        // The server may round the hint up to its tier width.
        stepSeconds: 20,
      }),
    );

    expect(range.stepMs).toBe(15_000);
    expect(grid.stepMs).toBe(20_000);
    expect(grid.stepCount).toBe(60);
    expect(grid.series[0].steps[2]).toEqual({ delta: 9, kind: "counter" });
  });
});

/** The card ranges of the advertised resolutions. */
function cardRangesOf(resolutions: Parameters<typeof metricRangesFromResolutions>[0]) {
  return cardRanges(metricRangesFromResolutions(resolutions));
}
