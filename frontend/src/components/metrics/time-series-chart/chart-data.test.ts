import { describe, expect, it } from "vitest";

import {
  buildChartData,
  chartAxisIncrements,
  formatChartAxisValue,
  formatChartValue,
} from "./chart-data";

describe("buildChartData", () => {
  it("keeps absent points null and lays x values on the step grid in epoch seconds", () => {
    const data = buildChartData(10_000, 2000, [
      { color: "chart-1", id: "used", label: "Used", values: [1, null, 3] },
    ]);

    expect(data.x).toEqual([10, 12, 14]);
    expect(data.columns[0]).toEqual([1, null, 3]);
  });

  it("spans at least the given step count, so a chart without series still has a time axis", () => {
    expect(buildChartData(10_000, 2000, [], 3)).toEqual({
      bases: [],
      columns: [],
      x: [10, 12, 14],
    });
    expect(
      buildChartData(0, 1000, [{ color: "chart-1", id: "used", label: "Used", values: [1] }], 2).x,
    ).toEqual([0, 1]);
  });

  it("moves provisional points to an overlay joined to the last closed point", () => {
    const data = buildChartData(0, 1000, [
      {
        color: "chart-1",
        id: "used",
        label: "Used",
        provisional: [false, false, true],
        values: [1, 2, 3],
      },
    ]);

    expect(data.columns).toEqual([
      [1, 2, null],
      [null, 2, 3],
    ]);
  });
});

describe("buildChartData for bars", () => {
  it("keeps an absent bar null and leaves an open bar unjoined to its closed neighbour", () => {
    const data = buildChartData(0, 1000, [
      {
        color: "chart-1",
        id: "requests",
        label: "Requests",
        mark: "bars",
        provisional: [false, false, false, true],
        values: [4, null, 2, 1],
      },
    ]);

    expect(data.columns).toEqual([
      [4, null, 2, null],
      [null, null, null, 1],
    ]);
    expect(data.bases).toEqual([null, null]);
  });

  it("stacks bars in series order, each rising from the total of the bars below it", () => {
    const data = buildChartData(0, 1000, [
      { color: "chart-1", id: "a", label: "A", mark: "stackedBars", values: [1, null, 3] },
      { color: "chart-2", id: "b", label: "B", mark: "stackedBars", values: [2, 5, null] },
      { color: "chart-3", id: "p95", label: "p95", values: [9, 9, 9] },
    ]);

    expect(data.columns.slice(0, 3)).toEqual([
      [1, null, 3],
      [3, 5, null],
      [9, 9, 9],
    ]);
    expect(data.bases.slice(0, 3)).toEqual([[0, 0, 0], [1, 0, 3], null]);
  });

  it("stacks open bars on the bars below them", () => {
    const data = buildChartData(0, 1000, [
      {
        color: "chart-1",
        id: "a",
        label: "A",
        mark: "stackedBars",
        provisional: [false, true],
        values: [1, 2],
      },
      {
        color: "chart-2",
        id: "b",
        label: "B",
        mark: "stackedBars",
        provisional: [false, true],
        values: [3, 4],
      },
    ]);

    expect(data.columns).toEqual([
      [1, null],
      [4, null],
      [null, 2],
      [null, 6],
    ]);
    expect(data.bases).toEqual([
      [0, 0],
      [1, 2],
      [0, 0],
      [1, 2],
    ]);
  });
});

describe("formatChartValue", () => {
  it("formats bytes in binary units and clamps negatives to zero", () => {
    expect(formatChartValue("bytes", 1024 ** 3 * 11.25)).toBe("11.3 GiB");
    expect(formatChartValue("bytes", 512 * 1024 ** 2)).toBe("512 MiB");
    expect(formatChartValue("bytes", -5)).toBe("0 bytes");
    expect(formatChartValue("bytes", Number.NaN)).toBe("No data");
  });
});

describe("formatChartValue for request units", () => {
  it("formats durations in the largest fitting unit", () => {
    expect(formatChartValue("seconds", 0.00025)).toBe("250 µs");
    expect(formatChartValue("seconds", 0.8504)).toBe("850 ms");
    expect(formatChartValue("seconds", 1.234)).toBe("1.23 s");
    expect(formatChartValue("seconds", 90)).toBe("1.5 min");
  });

  it("formats counts to three significant digits", () => {
    expect(formatChartValue("requests", 3)).toBe("3");
    expect(formatChartValue("requests", 0.2345)).toBe("0.235");
    expect(formatChartValue("requests", 1240)).toBe("1,240");
  });
});

describe("token rate formatting", () => {
  it("formats readouts with a suffix and axis ticks without", () => {
    expect(formatChartValue("tokensPerSecond", 42.46)).toBe("42.5 tokens/s");
    expect(formatChartValue("tokensPerSecond", 1234)).toBe("1.23k tokens/s");
    expect(formatChartValue("tokensPerSecond", 2_500_000)).toBe("2.5M tokens/s");
    expect(formatChartAxisValue("tokensPerSecond", 1500)).toBe("1.5k");
    expect(formatChartAxisValue("seconds", 0.85)).toBe("850 ms");
  });
});

describe("chartAxisIncrements", () => {
  it("steps seconds and request counts by round decimal multiples", () => {
    expect(chartAxisIncrements("seconds")).toContain(0.25);
    expect(chartAxisIncrements("requests")).toContain(0.5);
  });

  it("steps bytes by round multiples of each binary unit", () => {
    const increments = chartAxisIncrements("bytes");

    expect(increments).toContain(20 * 1024 ** 3);
    expect(increments).toContain(500 * 1024 ** 2);
    expect(increments).not.toContain(20 * 1000 ** 3);
  });
});

describe("formatChartValue for hardware units", () => {
  it("formats a ratio as a percentage, temperature in Celsius and power in watts", () => {
    expect(formatChartValue("ratio", 0.1234)).toBe("12%");
    expect(formatChartValue("ratio", 1)).toBe("100%");
    expect(formatChartValue("celsius", 61)).toBe("61 °C");
    expect(formatChartValue("watts", 312.46)).toBe("312 W");
  });

  it("rounds percentages, temperatures and power to whole numbers", () => {
    expect(formatChartValue("ratio", 0.4567)).toBe("46%");
    expect(formatChartValue("ratio", 0.004)).toBe("0%");
    expect(formatChartValue("celsius", 61.5)).toBe("62 °C");
    expect(formatChartValue("watts", 87.46)).toBe("87 W");
    expect(formatChartValue("watts", 1234.5)).toBe("1,235 W");
  });

  it("keeps axis ticks on round values in the unit", () => {
    expect(formatChartAxisValue("ratio", 0.25)).toBe("25%");
    expect(chartAxisIncrements("ratio")).toContain(0.25);
    expect(chartAxisIncrements("celsius")).toContain(10);
    expect(chartAxisIncrements("watts")).toContain(100);
  });
});
