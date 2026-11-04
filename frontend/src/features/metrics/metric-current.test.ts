import { describe, expect, it } from "vitest";

import { currentValue, fillFraction, hasActivity, peakValue } from "./metric-current";

describe("currentValue", () => {
  it("reads the latest closed step, skipping open ones", () => {
    expect(currentValue({ provisional: [false, false, true], values: [1, 2, 9] }, 3)).toBe(2);
  });

  it("has no current reading when nothing closed is within the fresh steps", () => {
    const series = { provisional: [false, false, false, false], values: [5, null, null, null] };

    expect(currentValue(series, 3)).toBeUndefined();
    expect(currentValue(series, 4)).toBe(5);
    expect(currentValue(undefined, 3)).toBeUndefined();
  });
});

describe("peakValue", () => {
  it("is the highest closed reading", () => {
    expect(peakValue({ provisional: [false, true, false], values: [3, 99, 7] })).toBe(7);
    expect(peakValue({ values: [null, null] })).toBeUndefined();
  });
});

describe("hasActivity", () => {
  it("sees any non-zero step, open or closed", () => {
    expect(hasActivity({ values: [0, 0, null] })).toBe(false);
    expect(hasActivity({ provisional: [false, true], values: [0, 0.5] })).toBe(true);
    expect(hasActivity(undefined)).toBe(false);
  });
});

describe("fillFraction", () => {
  it("is the reading's share of its reference, unknown without either", () => {
    expect(fillFraction(29, 100)).toBe(0.29);
    expect(fillFraction(undefined, 100)).toBeUndefined();
    expect(fillFraction(3, undefined)).toBeUndefined();
    expect(fillFraction(3, 0)).toBeUndefined();
  });
});
