import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { ResolvedLocalModelDeploymentSnapshotSchema } from "@/gen/dotcore/model/v1/deployment_pb";

import {
  configurationKvCachePrecision,
  currentKvCachePrecision,
  deploymentKvCachePrecisionLabel,
  kvCachePrecisionCustomizations,
  kvCachePrecisionOverrides,
} from "./kv-cache-precision";
import {
  modelWithKvCachePrecision,
  runtimeSchemaModel,
  stringValue,
} from "./runtime-schema-test-support";

describe("KV cache precision", () => {
  function option(model: ReturnType<typeof modelWithKvCachePrecision>, withProfile = true) {
    return configurationKvCachePrecision(
      model,
      model.configurations[0],
      withProfile ? model.profiles[0] : undefined,
    );
  }

  it("is absent when the configuration does not declare the option", () => {
    expect(
      configurationKvCachePrecision(
        runtimeSchemaModel,
        runtimeSchemaModel.configurations[0],
        runtimeSchemaModel.profiles[0],
      ),
    ).toBeUndefined();
  });

  it("defaults to the configuration's precision, then the profile's", () => {
    expect(option(modelWithKvCachePrecision())).toEqual({
      choices: ["f16", "q8_0"],
      defaultPrecision: "f16",
      displayName: "KV cache precision",
      optionId: "kv-cache",
    });
    expect(option(modelWithKvCachePrecision("q8_0"))?.defaultPrecision).toBe("q8_0");
    expect(option(modelWithKvCachePrecision("q8_0"), false)?.defaultPrecision).toBe("f16");
  });

  it("falls back to the configuration default when the profile default is not a choice", () => {
    expect(option(modelWithKvCachePrecision("q4_0"))?.defaultPrecision).toBe("f16");
  });

  it("sends an override only for a precision other than the default", () => {
    const declared = option(modelWithKvCachePrecision());
    expect(kvCachePrecisionOverrides(declared, "f16")).toEqual([]);

    const [override] = kvCachePrecisionOverrides(declared, "q8_0");
    expect(override.optionId).toBe("kv-cache");
    expect(override.values.map((value) => [value.parameterId, value.value])).toEqual([
      ["precision", { case: "string", value: "q8_0" }],
    ]);
    expect(kvCachePrecisionCustomizations(declared, "q8_0")).toEqual(["KV cache precision q8_0"]);
    expect(kvCachePrecisionCustomizations(declared, "f16")).toEqual([]);
  });

  it("falls back to the default for a pick the option does not offer", () => {
    const declared = option(modelWithKvCachePrecision());
    expect(currentKvCachePrecision(declared, "")).toBe("f16");
    expect(currentKvCachePrecision(declared, "q8_0")).toBe("q8_0");
    expect(currentKvCachePrecision(declared, "q4_0")).toBe("f16");
  });

  it("reads the resolved precision of a deployment", () => {
    const model = modelWithKvCachePrecision();
    const snapshot = create(ResolvedLocalModelDeploymentSnapshotSchema, {
      resolvedOptions: [
        { values: { optionId: "kv-cache", values: [stringValue("precision", "q8_0")] } },
      ],
    });
    expect(deploymentKvCachePrecisionLabel(snapshot, model)).toBe("q8_0");
    expect(deploymentKvCachePrecisionLabel(snapshot, runtimeSchemaModel)).toBeUndefined();
    expect(deploymentKvCachePrecisionLabel(snapshot, undefined)).toBeUndefined();
  });
});
