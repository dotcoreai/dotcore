import { describe, expect, it } from "vitest";

import { SizingScenarioUnsupportedReason } from "@/gen/dotcore/model/v1/service_pb";

import {
  applySizingScenario,
  formatSizingScenario,
  parseSizingScenario,
  scenarioFromInputs,
  scenarioInputErrors,
  sizingScenarioRequest,
  sizingUnsupportedText,
} from "./sizing-scenario";

describe("sizing scenario", () => {
  it("round-trips the context per request, parallel requests and total context through req, par and ctx", () => {
    const search = applySizingScenario("?f=runs&runs=node%3Aa&panel=model-catalog%2Fcatalog%3Aa", {
      contextTokensPerRequest: 65536,
      parallelRequests: 4,
      totalContextTokens: 1048576,
    });

    expect(search).toBe(
      "?f=runs&runs=node%3Aa&req=65536&par=4&ctx=1048576&panel=model-catalog%2Fcatalog%3Aa",
    );
    expect(parseSizingScenario(search)).toEqual({
      contextTokensPerRequest: 65536,
      parallelRequests: 4,
      totalContextTokens: 1048576,
    });
    expect(applySizingScenario(search, undefined)).toBe(
      "?f=runs&runs=node%3Aa&panel=model-catalog%2Fcatalog%3Aa",
    );
  });

  it("round-trips a context per request that leaves the rest to each profile", () => {
    const search = applySizingScenario("?ctx=4096&par=2", { contextTokensPerRequest: 32768 });

    expect(search).toBe("?req=32768");
    expect(parseSizingScenario(search)).toEqual({ contextTokensPerRequest: 32768 });
  });

  it.each([
    ["no parameters", ""],
    ["only a total context", "?ctx=32768"],
    ["a context per request above the total context", "?req=65536&ctx=32768"],
    ["no parallel requests", "?req=32768&par=0"],
    ["a fraction", "?req=32768.5"],
    ["a value no number holds exactly", "?req=9007199254740992"],
  ])("reads defaults from %s", (_, search) => {
    expect(parseSizingScenario(search)).toBeUndefined();
  });

  it("requests the context per request, and the parallel requests and total only when chosen", () => {
    expect(
      sizingScenarioRequest({
        contextTokensPerRequest: 32768,
        parallelRequests: 4,
        totalContextTokens: 131072,
      }),
    ).toEqual({
      contextTokensPerRequest: 32768n,
      parallelRequests: 4,
      totalContextTokens: 131072n,
    });
    expect(sizingScenarioRequest({ contextTokensPerRequest: 32768 })).toEqual({
      contextTokensPerRequest: 32768n,
    });
  });

  it("names a scenario compactly", () => {
    expect(formatSizingScenario({ contextTokensPerRequest: 65536 })).toBe("64k per request");
    expect(
      formatSizingScenario({
        contextTokensPerRequest: 65536,
        parallelRequests: 4,
        totalContextTokens: 1048576,
      }),
    ).toBe("64k per request · 4 parallel · 1M total");
  });

  it("explains each input's range, leaving engine ranges to each profile", () => {
    const inputs = (perRequest: string, parallel = "", total = "") => ({
      parallel,
      perRequest,
      total,
    });

    expect(scenarioInputErrors(inputs("1"))).toEqual({});
    expect(scenarioInputErrors(inputs(" 4096 ", " 4 ", " 8192 "))).toEqual({});
    expect(scenarioInputErrors(inputs(""))).toEqual({
      perRequest: "Context per request must be a whole number of tokens.",
    });
    expect(scenarioInputErrors(inputs("9007199254740992"))).toEqual({
      perRequest: "Context per request must be a whole number of tokens.",
    });
    expect(scenarioInputErrors(inputs("4096", "0"))).toEqual({
      parallel: "Parallel requests must be a whole number of at least 1.",
    });
    expect(scenarioInputErrors(inputs("4096", "", "8k"))).toEqual({
      total: "Total context must be a whole number of tokens.",
    });
    expect(scenarioInputErrors(inputs("16384", "", "8192"))).toEqual({
      perRequest: "Context per request must be at most the total context.",
    });
  });

  it("reads a scenario from valid inputs only", () => {
    expect(scenarioFromInputs({ parallel: "", perRequest: "32768", total: "" })).toEqual({
      contextTokensPerRequest: 32768,
    });
    expect(scenarioFromInputs({ parallel: "4", perRequest: "32768", total: "131072" })).toEqual({
      contextTokensPerRequest: 32768,
      parallelRequests: 4,
      totalContextTokens: 131072,
    });
    expect(
      scenarioFromInputs({ parallel: "", perRequest: "262144", total: "131072" }),
    ).toBeUndefined();
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
