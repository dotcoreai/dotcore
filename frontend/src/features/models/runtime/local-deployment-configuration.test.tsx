import { clone, create } from "@bufbuild/protobuf";
import { render, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import {
  ExecutionTargetsWatch,
  executionTargetsWatch,
} from "@/features/deployments/execution-targets-watch-test-support";
import { syntheticCatalog } from "@/features/models/catalog/catalog-table-test-support";
import { ModelDeployDialog } from "@/features/models/deploy/model-deploy-dialog";
import { EngineReleaseSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { ModelProfileSchema, ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { ParameterVisibility } from "@/gen/dotcore/catalog/v1/parameter_pb";

import { engineParameterError, engineParameterInputs } from "./engine-parameter-values";
import {
  changedEngineParameters,
  defaultLocalDeploymentConfiguration,
  localDeploymentConfiguration,
  type LocalDeploymentConfigurationEdits,
  profileEngineRelease,
  runtimeCustomizations,
} from "./local-deployment-configuration";
import {
  llamaCppRelease,
  modelWithStrata,
  runtimeSchemaModel,
  runtimeSchemaReleases,
  strataRelease,
  unsignedValue,
  vllmRelease,
} from "./runtime-schema-test-support";

const { engineReleases, models } = syntheticCatalog();
const alpha = models[0];
const fast = alpha.profiles[0];

describe("local deployment configuration", () => {
  it("builds the untouched profile configuration from its defaults", () => {
    expect(defaultLocalDeploymentConfiguration(alpha, fast, engineReleases)).toMatchObject({
      customArgumentTokens: [],
      engineParameterValues: {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: 8192n } },
          { parameterId: "prefix-caching", value: { case: "boolean", value: true } },
        ],
      },
      modelArtifactBindingId: "",
    });
  });

  it("starts from the engine defaults the profile leaves out, without a total context", () => {
    const profile = runtimeSchemaModel.profiles.find(
      ({ profileId }) => profileId === "nvfp4-text",
    )!;

    expect(
      pairs(
        defaultLocalDeploymentConfiguration(runtimeSchemaModel, profile, runtimeSchemaReleases)
          ?.engineParameterValues,
      ),
    ).toEqual([
      ["maximum-batched-tokens", 4096n],
      ["maximum-cudagraph-capture-size", 32n],
      ["kv-offloading-size-gib", 0n],
      ["context-per-request", 32768n],
      ["parallel-requests", 4n],
    ]);
  });

  it("leaves out the engine parameters the profile's configuration excludes", () => {
    const catalogProfile = runtimeSchemaModel.profiles.find(
      ({ profileId }) => profileId === "nvfp4-text",
    )!;
    // A profile can't default a parameter its configuration excludes.
    const profile = clone(ModelProfileSchema, catalogProfile);
    profile.engineParameterDefaults!.values = profile.engineParameterDefaults!.values.filter(
      ({ parameterId }) => parameterId !== "kv-offloading-size-gib",
    );
    const model = create(ModelSchema, {
      ...runtimeSchemaModel,
      configurations: runtimeSchemaModel.configurations.map((configuration) =>
        configuration.configurationId === profile.configurationIds[0]
          ? { ...configuration, excludedEngineParameterIds: ["kv-offloading-size-gib"] }
          : configuration,
      ),
    });

    expect(
      profileEngineRelease(model, profile, runtimeSchemaReleases)?.parameterSchema?.parameters.map(
        ({ parameterId }) => parameterId,
      ),
    ).toEqual(
      vllmRelease.parameterSchema?.parameters
        .map(({ parameterId }) => parameterId)
        .filter((parameterId) => parameterId !== "kv-offloading-size-gib"),
    );
    // The host memory group loses its only member, so it goes too.
    expect(
      profileEngineRelease(model, profile, runtimeSchemaReleases)?.parameterSchema?.parameterGroups,
    ).toEqual([]);
    expect(
      pairs(
        defaultLocalDeploymentConfiguration(model, profile, runtimeSchemaReleases)
          ?.engineParameterValues,
      ),
    ).toEqual([
      ["maximum-batched-tokens", 4096n],
      ["maximum-cudagraph-capture-size", 32n],
      ["context-per-request", 32768n],
      ["parallel-requests", 4n],
    ]);
  });

  it("keeps a parameter group while the configuration keeps one of its members", () => {
    const model = modelWithStrata({ excludedEngineParameterIds: ["expert-host-memory-mib"] });
    const profile = model.profiles.find(({ profileId }) => profileId === "strata");
    const schema = profileEngineRelease(model, profile, [strataRelease])?.parameterSchema;

    expect(schema?.parameters.map(({ parameterId }) => parameterId)).toEqual([
      "total-context",
      "context-per-request",
      "parallel-requests",
      "expert-vram-mib",
    ]);
    expect(schema?.parameterGroups.map(({ groupId }) => groupId)).toEqual(["expert-memory"]);
  });

  it("submits a typed context per request and omits a cleared one", () => {
    const profile = runtimeSchemaModel.profiles.find(
      ({ profileId }) => profileId === "nvfp4-text",
    )!;
    const declaredValues = {
      values: [
        unsignedValue("total-context", 131072n),
        unsignedValue("parallel-requests", 2n),
        unsignedValue("context-per-request", 65536n),
      ],
    } as LocalDeploymentConfigurationEdits["declaredValues"];
    const configured = (perRequest: string) =>
      localDeploymentConfiguration(profile, vllmRelease, {
        declaredValues,
        inputs: new Map([
          ["total-context", "131072"],
          ["parallel-requests", "2"],
          ["kv-offloading-size-gib", "0"],
          ["context-per-request", perRequest],
        ]),
      })?.engineParameterValues;

    expect(pairs(configured("16384"))).toEqual([
      ["total-context", 131072n],
      ["parallel-requests", 2n],
      ["context-per-request", 16384n],
      ["kv-offloading-size-gib", 0n],
    ]);
    expect(pairs(configured(""))).toEqual([
      ["total-context", 131072n],
      ["parallel-requests", 2n],
      ["kv-offloading-size-gib", 0n],
    ]);
  });

  it("matches the request the deploy dialog sends for an untouched profile", async () => {
    const onEstimate = vi.fn().mockResolvedValue({});

    render(
      <ExecutionTargetsWatch watch={executionTargetsWatch()}>
        <ModelDeployDialog
          engineReleases={engineReleases}
          hardwareClasses={[]}
          initialProfileId="fast"
          model={alpha}
          onEstimate={onEstimate}
          onOpenChange={vi.fn()}
          onPreview={vi.fn()}
          onSubmit={vi.fn()}
          open
          pending={false}
        />
      </ExecutionTargetsWatch>,
    );

    await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
    expect(onEstimate).toHaveBeenCalledWith({
      configuration: defaultLocalDeploymentConfiguration(alpha, fast, engineReleases),
      profileId: "fast",
    });
  });

  it("returns nothing when an exposed parameter has no valid value", () => {
    expect(
      localDeploymentConfiguration(fast, engineReleases[0], {
        inputs: new Map([["total-context", "8k"]]),
      }),
    ).toBeUndefined();
  });

  it("carries edited inputs and custom arguments, and leaves the weights to the profile", () => {
    expect(
      localDeploymentConfiguration(fast, engineReleases[0], {
        customArgumentTokens: ["--flag"],
        inputs: new Map([["total-context", "4096"]]),
      }),
    ).toMatchObject({
      customArgumentTokens: ["--flag"],
      engineParameterValues: {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: 4096n } },
          { parameterId: "prefix-caching", value: { case: "boolean", value: true } },
        ],
      },
      modelArtifactBindingId: "",
    });
  });

  it("names only the runtime settings that differ from the profile defaults, leaving out context sizes", () => {
    const profile = runtimeSchemaModel.profiles.find(({ profileId }) => profileId === "q4-xl")!;
    const defaults = new Map([
      ["prompt-cache-memory-mib", "8192"],
      ["total-context", "32768"],
      ["parallel-requests", "4"],
    ]);
    const customizations = (inputs: [string, string][]) =>
      runtimeCustomizations(profile, llamaCppRelease, {
        inputs: new Map([...defaults, ...inputs]),
      });

    expect(customizations([])).toEqual([]);
    expect(customizations([["prompt-cache-memory-mib", "4096"]])).toEqual([
      "Prompt cache memory 4 GiB",
    ]);
    // An out-of-range value stays as typed rather than being rounded into a plausible one.
    expect(customizations([["prompt-cache-memory-mib", "3000000000"]])).toEqual([
      "Prompt cache memory 3000000000",
    ]);
    expect(
      customizations([
        ["total-context", "16384"],
        ["context-per-request", "8192"],
        ["parallel-requests", "2"],
      ]),
    ).toEqual([]);
  });

  it("lists the changed engine parameters, leaving out the context parameters", () => {
    const profile = runtimeSchemaModel.profiles.find(({ profileId }) => profileId === "q4-xl")!;
    const changed = (inputs: [string, string][]) =>
      changedEngineParameters(profile, llamaCppRelease, {
        inputs: new Map([
          ["prompt-cache-memory-mib", "8192"],
          ["total-context", "32768"],
          ["parallel-requests", "4"],
          ...inputs,
        ]),
      }).map(({ parameter, value }) => [parameter.parameterId, value]);

    expect(
      changed([
        ["total-context", "16384"],
        ["parallel-requests", "2"],
      ]),
    ).toEqual([]);
    expect(changed([["prompt-cache-memory-mib", "4096"]])).toEqual([
      ["prompt-cache-memory-mib", "4 GiB"],
    ]);
  });
});

describe("secret engine parameters", () => {
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
          parameterId: "api-key",
          displayName: "API key",
          visibility: ParameterVisibility.ADVANCED,
          parameterType: { case: "secret", value: { minimumLength: 12 } },
        },
      ],
    },
  });
  const profile = create(ModelProfileSchema, {});
  const apiKey = release.parameterSchema!.parameters[1];
  const inputs = (key: string) =>
    new Map([
      ["total-context", "100"],
      ["api-key", key],
    ]);
  const configured = (key: string) =>
    localDeploymentConfiguration(profile, release, { inputs: inputs(key) });

  it("leaves an empty secret out so it is generated at deploy, without blocking Deploy", () => {
    expect(engineParameterError(apiKey, inputs(""))).toBeUndefined();
    expect(pairs(configured("")?.engineParameterValues)).toEqual([["total-context", 100n]]);
  });

  it("sends a typed secret as a string and keeps it as an input", () => {
    const values = configured("correct-horse-battery")?.engineParameterValues;

    expect(pairs(values)).toEqual([
      ["total-context", 100n],
      ["api-key", "correct-horse-battery"],
    ]);
    expect(engineParameterInputs(values).get("api-key")).toBe("correct-horse-battery");
  });

  it("blocks Deploy on a typed secret below its minimum length", () => {
    expect(engineParameterError(apiKey, inputs("short"))).toBe(
      "API key must be at least 12 bytes.",
    );
    expect(configured("short")).toBeUndefined();
  });
});

function pairs(values: LocalDeploymentConfigurationEdits["declaredValues"]) {
  return values?.values.map(({ parameterId, value }) => [parameterId, value.value]);
}
