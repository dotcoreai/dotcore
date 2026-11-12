import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { EngineReleaseSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { ModelProfileSchema } from "@/gen/dotcore/catalog/v1/model_pb";

import { engineKindLabel } from "../profiles/model-row";
import {
  effectiveEngineParameters,
  engineParameterError,
  engineParameterValues,
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
    exposed: true,
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
