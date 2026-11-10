import { describe, expect, it } from "vitest";

import { SizingScenarioUnsupportedReason } from "@/gen/dotcore/model/v1/service_pb";

import {
  applySizingScenario,
  followContextInputs,
  formatSizingScenario,
  parseSizingScenario,
  scenarioFromInputs,
  scenarioInputErrors,
  sizingScenarioRequest,
  sizingUnsupportedText,
} from "./sizing-scenario";

describe("sizing scenario", () => {
  it("round-trips the total context and context per request through ctx and req", () => {
    const search = applySizingScenario("?f=runs&runs=node%3Aa&panel=model-catalog%2Fcatalog%3Aa", {
      contextTokensPerRequest: 65536,
      totalContextTokens: 1048576,
    });

    expect(search).toBe(
      "?f=runs&runs=node%3Aa&ctx=1048576&req=65536&panel=model-catalog%2Fcatalog%3Aa",
    );
    expect(parseSizingScenario(search)).toEqual({
      contextTokensPerRequest: 65536,
      totalContextTokens: 1048576,
    });
    expect(applySizingScenario(search, undefined)).toBe(
      "?f=runs&runs=node%3Aa&panel=model-catalog%2Fcatalog%3Aa",
    );
  });

  it("round-trips a total context without a context per request", () => {
    const search = applySizingScenario("?req=4096", { totalContextTokens: 262144 });

    expect(search).toBe("?ctx=262144");
    expect(parseSizingScenario(search)).toEqual({ totalContextTokens: 262144 });
  });

  it.each([
    ["no parameters", ""],
    ["only a context per request", "?req=32768"],
    ["a total context below 1024 tokens", "?ctx=1023"],
    ["a context per request above the total context", "?ctx=32768&req=65536"],
    ["a context per request below 1024 tokens", "?ctx=32768&req=512"],
    ["a fraction", "?ctx=32768.5"],
    ["a value above the 32-bit range", "?ctx=4294967296"],
  ])("reads defaults from %s", (_, search) => {
    expect(parseSizingScenario(search)).toBeUndefined();
  });

  it("requests the total context, and the context per request only when chosen", () => {
    expect(
      sizingScenarioRequest({ contextTokensPerRequest: 32768, totalContextTokens: 131072 }),
    ).toEqual({ contextTokensPerRequest: 32768, totalContextTokens: 131072 });
    expect(sizingScenarioRequest({ totalContextTokens: 131072 })).toEqual({
      totalContextTokens: 131072,
    });
  });

  it("names a scenario compactly", () => {
    expect(formatSizingScenario({ totalContextTokens: 1048576 })).toBe("1M");
    expect(
      formatSizingScenario({ contextTokensPerRequest: 65536, totalContextTokens: 1048576 }),
    ).toBe("1M · 64k per request");
  });

  it("explains each input's range", () => {
    expect(scenarioInputErrors({ perRequest: "", total: "1024" })).toEqual({});
    expect(scenarioInputErrors({ perRequest: " 4096 ", total: " 8192 " })).toEqual({});
    expect(scenarioInputErrors({ perRequest: "", total: "512" })).toEqual({
      total: "Total context must be a whole number of at least 1024 tokens.",
    });
    expect(scenarioInputErrors({ perRequest: "10", total: "8192" })).toEqual({
      perRequest: "Context per request must be a whole number of at least 1024 tokens.",
    });
    expect(scenarioInputErrors({ perRequest: "16384", total: "8192" })).toEqual({
      perRequest: "Context per request must be at most the total context.",
    });
  });

  it("reads a scenario from valid inputs only", () => {
    expect(scenarioFromInputs({ perRequest: "", total: "131072" })).toEqual({
      totalContextTokens: 131072,
    });
    expect(scenarioFromInputs({ perRequest: "32768", total: "131072" })).toEqual({
      contextTokensPerRequest: 32768,
      totalContextTokens: 131072,
    });
    expect(scenarioFromInputs({ perRequest: "262144", total: "131072" })).toBeUndefined();
  });

  it("says why a profile can't serve the scenario", () => {
    const text = (reason: SizingScenarioUnsupportedReason) =>
      sizingUnsupportedText(reason, { engine: "ninfer", maximumContextTokens: 262144 });

    expect(text(SizingScenarioUnsupportedReason.CONTEXT_ABOVE_MAXIMUM)).toBe(
      "Context per request is above the 256k maximum context",
    );
    expect(text(SizingScenarioUnsupportedReason.PARAMETER_OUT_OF_RANGE)).toBe(
      "ninfer doesn't accept this scenario's settings",
    );
    expect(text(SizingScenarioUnsupportedReason.DERIVED_PARAMETER_OUT_OF_RANGE)).toBe(
      "ninfer can't size memory for this scenario",
    );
  });
});

describe("followContextInputs", () => {
  it("moves the input the operator didn't edit so the total covers the context per request", () => {
    expect(followContextInputs({ perRequest: "65536", total: "32768" }, "perRequest")).toEqual({
      perRequest: "65536",
      total: "65536",
    });
    expect(followContextInputs({ perRequest: "65536", total: "8192" }, "total")).toEqual({
      perRequest: "8192",
      total: "8192",
    });
  });

  it("leaves inputs that already fit, an empty or invalid one, and an edit below 1024 tokens", () => {
    for (const [inputs, edited] of [
      [{ perRequest: "8192", total: "32768" }, "perRequest"],
      [{ perRequest: "", total: "8192" }, "total"],
      [{ perRequest: "65536", total: "8k" }, "total"],
      [{ perRequest: "65536", total: "512" }, "total"],
    ] as const) {
      expect(followContextInputs(inputs, edited)).toBe(inputs);
    }
  });
});
