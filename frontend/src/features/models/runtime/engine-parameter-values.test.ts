import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { EngineReleaseSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { ModelProfileSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { ParameterVisibility } from "@/gen/dotcore/catalog/v1/parameter_pb";

import { engineKindLabel } from "../profiles/model-row";
import {
  effectiveEngineParameters,
  engineParameterError,
  engineParameterValues,
  profileStartingValues,
} from "./engine-parameter-values";

describe("engineKindLabel", () => {
  it("names Strata", () => {
    expect(engineKindLabel("strata")).toBe("Strata");
  });
});

describe("sizing-derived parameters", () => {
  const unsigned = (parameterId: string, defaultValue?: bigint) => ({
    parameterId,
    displayName: parameterId,
    visibility: ParameterVisibility.ADVANCED,
    parameterType: {
      case: "unsignedInteger" as const,
      value: { minimum: 1n, maximum: 1000n, unit: "MiB", defaultValue },
    },
  });
  const release = create(EngineReleaseSchema, {
    parameterSchema: {
      parameters: [unsigned("parallel-requests", 1n), unsigned("expert-vram-mib")],
    },
  });
  const parameters = effectiveEngineParameters(release, create(ModelProfileSchema, {}));
  const derived = new Set(["expert-vram-mib"]);
  const expert = parameters.find((parameter) => parameter.parameterId === "expert-vram-mib")!;

  it("omits an empty parameter the backend derives and accepts it as valid", () => {
    const inputs = new Map([
      ["parallel-requests", "1"],
      ["expert-vram-mib", ""],
    ]);

    expect(engineParameterError(expert, inputs, derived)).toBeUndefined();
    expect(
      engineParameterValues(undefined, parameters, inputs, derived)?.values.map(
        (value) => value.parameterId,
      ),
    ).toEqual(["parallel-requests"]);
  });

  it("sends a typed derived parameter and flags an empty one the backend doesn't derive", () => {
    const inputs = new Map([
      ["parallel-requests", "1"],
      ["expert-vram-mib", "512"],
    ]);
    const typed = engineParameterValues(undefined, parameters, inputs, derived)?.values.find(
      (value) => value.parameterId === "expert-vram-mib",
    );

    expect(typed?.value).toEqual({ case: "unsignedInteger", value: 512n });
    expect(engineParameterError(expert, new Map([["expert-vram-mib", ""]]))).toBe(
      "expert-vram-mib must be an integer from 1 to 1000 MiB.",
    );
    expect(
      engineParameterValues(undefined, parameters, new Map([["parallel-requests", "1"]])),
    ).toBeUndefined();
  });
});

describe("starting values", () => {
  const release = create(EngineReleaseSchema, {
    parameterSchema: {
      parameters: [
        {
          parameterId: "total-context",
          displayName: "Total context",
          visibility: ParameterVisibility.BASIC,
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 1n, maximum: 1000n, unit: "tokens", defaultValue: 100n },
          },
        },
        {
          parameterId: "prefix-caching",
          displayName: "Prefix caching",
          visibility: ParameterVisibility.HIDDEN,
          parameterType: { case: "boolean", value: { defaultValue: true } },
        },
        {
          parameterId: "flash-attention",
          displayName: "Flash attention",
          visibility: ParameterVisibility.ADVANCED,
          parameterType: { case: "boolean", value: { defaultValue: false } },
        },
        {
          parameterId: "no-default",
          displayName: "No default",
          visibility: ParameterVisibility.ADVANCED,
          parameterType: { case: "boolean", value: {} },
        },
      ],
    },
  });

  it("takes the engine default of every typed parameter, booleans included, in schema order", () => {
    const values = profileStartingValues(release, create(ModelProfileSchema, {}));

    expect(values?.values.map(({ parameterId, value }) => [parameterId, value])).toEqual([
      ["total-context", { case: "unsignedInteger", value: 100n }],
      ["prefix-caching", { case: "boolean", value: true }],
      ["flash-attention", { case: "boolean", value: false }],
    ]);
  });

  it("keeps a profile's own value over the engine default", () => {
    const profile = create(ModelProfileSchema, {
      engineParameterDefaults: {
        values: [{ parameterId: "prefix-caching", value: { case: "boolean", value: false } }],
      },
    });

    expect(
      profileStartingValues(release, profile)?.values.map(({ parameterId, value }) => [
        parameterId,
        value,
      ]),
    ).toEqual([
      ["prefix-caching", { case: "boolean", value: false }],
      ["total-context", { case: "unsignedInteger", value: 100n }],
      ["flash-attention", { case: "boolean", value: false }],
    ]);
  });

  it("shows every parameter that isn't hidden", () => {
    expect(
      effectiveEngineParameters(release, create(ModelProfileSchema, {})).map(
        (parameter) => parameter.parameterId,
      ),
    ).toEqual(["total-context", "flash-attention", "no-default"]);
  });
});

describe("effective visibility", () => {
  const parameter = (parameterId: string, visibility: ParameterVisibility) => ({
    parameterId,
    displayName: parameterId,
    visibility,
    parameterType: {
      case: "unsignedInteger" as const,
      value: { minimum: 1n, maximum: 10n, unit: "" },
    },
  });
  const release = create(EngineReleaseSchema, {
    parameterSchema: {
      parameters: [
        parameter("total-context", ParameterVisibility.BASIC),
        parameter("parallel-requests", ParameterVisibility.ADVANCED),
        parameter("kv-offload", ParameterVisibility.HIDDEN),
        parameter("cache", ParameterVisibility.ADVANCED),
      ],
    },
  });
  const shown = (overrides: Array<[string, ParameterVisibility]>) =>
    effectiveEngineParameters(
      release,
      create(ModelProfileSchema, {
        engineParameterVisibilityOverrides: overrides.map(([parameterId, visibility]) => ({
          parameterId,
          visibility,
        })),
      }),
    ).map((candidate) => candidate.parameterId);

  it("shows the engine's basic and advanced parameters in schema order", () => {
    expect(shown([])).toEqual(["total-context", "parallel-requests", "cache"]);
  });

  it("lets a profile hide an advanced parameter", () => {
    expect(shown([["parallel-requests", ParameterVisibility.HIDDEN]])).toEqual([
      "total-context",
      "cache",
    ]);
  });

  it("lets a profile show a hidden parameter, in schema order", () => {
    expect(shown([["kv-offload", ParameterVisibility.ADVANCED]])).toEqual([
      "total-context",
      "parallel-requests",
      "kv-offload",
      "cache",
    ]);
  });

  it("lets a profile hide the total context", () => {
    expect(shown([["total-context", ParameterVisibility.HIDDEN]])).toEqual([
      "parallel-requests",
      "cache",
    ]);
  });
});
