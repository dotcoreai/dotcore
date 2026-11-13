import { render, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import {
  ExecutionTargetsWatch,
  executionTargetsWatch,
} from "@/features/deployments/execution-targets-watch-test-support";
import { syntheticCatalog } from "@/features/models/catalog/catalog-table-test-support";
import { ModelDeployDialog } from "@/features/models/deploy/model-deploy-dialog";

import {
  changedEngineParameters,
  defaultLocalDeploymentConfiguration,
  localDeploymentConfiguration,
  type LocalDeploymentConfigurationEdits,
  runtimeCustomizations,
} from "./local-deployment-configuration";
import {
  llamaCppRelease,
  runtimeSchemaModel,
  runtimeSchemaReleases,
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
      modelArtifactBindingId: "q4-binding",
    });
  });

  it("starts from the engine defaults the profile leaves out, without a context per request", () => {
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
      ["total-context", 32768n],
      ["parallel-requests", 4n],
    ]);
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
        bindingId: "nvfp4",
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

  it("returns nothing when an exposed parameter has no valid value or no binding is selected", () => {
    const release = engineReleases[0];

    expect(
      localDeploymentConfiguration(fast, release, { bindingId: "q4-binding", inputs: new Map() }),
    ).toBeUndefined();
    expect(
      localDeploymentConfiguration(fast, release, {
        bindingId: "",
        inputs: new Map([["total-context", "8192"]]),
      }),
    ).toBeUndefined();
  });

  it("carries edited inputs and custom arguments", () => {
    expect(
      localDeploymentConfiguration(fast, engineReleases[0], {
        bindingId: "q8-binding",
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
      modelArtifactBindingId: "q8-binding",
    });
  });

  it("names only the runtime settings that differ from the profile defaults, leaving out context sizes", () => {
    const profile = runtimeSchemaModel.profiles.find(({ profileId }) => profileId === "q4-xl")!;
    const defaults = new Map([
      ["prompt-cache-memory-mib", "8192"],
      ["total-context", "32768"],
      ["parallel-requests", "4"],
    ]);
    const customizations = (inputs: [string, string][], bindingId = "q4-xl") =>
      runtimeCustomizations(runtimeSchemaModel, profile, llamaCppRelease, {
        bindingId,
        inputs: new Map([...defaults, ...inputs]),
      });

    expect(customizations([])).toEqual([]);
    expect(customizations([["prompt-cache-memory-mib", "4096"]], "other")).toEqual([
      "Prompt cache memory 4 GiB",
      "Artifact not selected",
    ]);
    // An out-of-range value stays as typed rather than being rounded into a plausible one.
    expect(customizations([["prompt-cache-memory-mib", "999999"]])).toEqual([
      "Prompt cache memory 999999",
    ]);
    expect(
      customizations([
        ["total-context", "16384"],
        ["context-per-request", "8192"],
        ["parallel-requests", "2"],
      ]),
    ).toEqual([]);
  });

  it("lists the changed engine parameters with parallel requests, leaving out context sizes", () => {
    const profile = runtimeSchemaModel.profiles.find(({ profileId }) => profileId === "q4-xl")!;
    const changed = (inputs: [string, string][]) =>
      changedEngineParameters(profile, llamaCppRelease, {
        bindingId: "q4-xl",
        inputs: new Map([
          ["prompt-cache-memory-mib", "8192"],
          ["total-context", "32768"],
          ["parallel-requests", "4"],
          ...inputs,
        ]),
      }).map(({ parameter, value }) => [parameter.parameterId, value]);

    expect(changed([["total-context", "16384"]])).toEqual([]);
    expect(
      changed([
        ["parallel-requests", "2"],
        ["prompt-cache-memory-mib", "4096"],
      ]),
    ).toEqual([
      ["parallel-requests", "2 requests"],
      ["prompt-cache-memory-mib", "4 GiB"],
    ]);
  });
});

function pairs(values: LocalDeploymentConfigurationEdits["declaredValues"]) {
  return values?.values.map(({ parameterId, value }) => [parameterId, value.value]);
}
