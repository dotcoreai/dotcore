import { clone, create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { EngineReleaseSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { ParameterValuesSchema } from "@/gen/dotcore/catalog/v1/parameter_pb";

import {
  profileContextSliderEnds,
  requestCount,
  widestContextSliderEnds,
} from "./context-slider-ends";
import {
  ninferRelease,
  runtimeSchemaModel,
  runtimeSchemaReleases,
  unsignedValue,
} from "./runtime-schema-test-support";

describe("profileContextSliderEnds", () => {
  it("ends context per request at the model maximum below the engine's, and the total per request", () => {
    // llama.cpp accepts up to 2147483647 tokens; the model stops at 262144, 4 requests at a time.
    expect(ends(runtimeSchemaModel, "q4-xl")).toEqual({
      minimum: 512,
      perRequest: 262144,
      total: 1048576,
    });
  });

  it("ends context per request at the engine maximum below the model's, within the total maximum", () => {
    const model = clone(ModelSchema, runtimeSchemaModel);
    model.maximumContextTokens = 4_294_967_295;

    expect(ends(model, "q4-xl")).toEqual({
      minimum: 512,
      perRequest: 2_147_483_647,
      total: 2_147_483_647,
    });
  });

  it("ends context per request at the model maximum when the engine declares none", () => {
    const model = clone(ModelSchema, runtimeSchemaModel);
    model.maximumContextTokens = 4_294_967_295;

    // vLLM declares no maximum on either context, so only the model and parallel requests bound them.
    expect(ends(model, "nvfp4-text")).toEqual({
      minimum: 512,
      perRequest: 4_294_967_295,
      total: 17_179_869_180,
    });
  });
});

describe("widestContextSliderEnds", () => {
  it("covers every profile: the lowest minimum and the largest ends", () => {
    const model = clone(ModelSchema, runtimeSchemaModel);
    // ninfer runs 8 requests at once from a 1024-token minimum; llama.cpp and vLLM run 4 from 512.
    model.profiles.find(
      (profile) => profile.profileId === "ninfer-nvfp4",
    )!.engineParameterDefaults = create(ParameterValuesSchema, {
      values: [unsignedValue("parallel-requests", 8n)],
    });
    const ninfer = clone(EngineReleaseSchema, ninferRelease);
    for (const parameter of ninfer.parameterSchema!.parameters)
      if (
        parameter.parameterType.case === "unsignedInteger" &&
        ["context-per-request", "total-context"].includes(parameter.parameterId)
      )
        parameter.parameterType.value.minimum = 1024n;
    const releases = runtimeSchemaReleases.map((release) =>
      release.engineReleaseId === ninfer.engineReleaseId ? ninfer : release,
    );

    expect(
      widestContextSliderEnds(
        model.profiles.flatMap(
          (profile) => profileContextSliderEnds(model, profile, releases) ?? [],
        ),
      ),
    ).toEqual({ minimum: 512, perRequest: 262144, total: 2_097_152 });
    expect(widestContextSliderEnds([])).toBeUndefined();
  });
});

describe("requestCount", () => {
  it("reads a count, and one while the input holds none", () => {
    expect(requestCount("8")).toBe(8);
    expect(requestCount(" 2 ")).toBe(2);
    expect(requestCount("")).toBe(1);
    expect(requestCount("0")).toBe(1);
    expect(requestCount("2x")).toBe(1);
    expect(requestCount(undefined)).toBe(1);
  });
});

function ends(model: typeof runtimeSchemaModel, profileId: string) {
  const profile = model.profiles.find((candidate) => candidate.profileId === profileId)!;
  return profileContextSliderEnds(model, profile, runtimeSchemaReleases);
}
