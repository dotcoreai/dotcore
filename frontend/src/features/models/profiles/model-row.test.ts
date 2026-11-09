import { clone, create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import {
  nvidiaEstimates,
  syntheticCatalog,
} from "@/features/models/catalog/catalog-table-test-support";
import {
  localDeployment,
  upstreamDeployment,
  upstreamTemplate,
} from "@/features/models/deployed/models-section-test-support";
import { HardwareClassSchema } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import { InferenceRequestKind } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import { ModelConfigurationOptionSchema, ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

import {
  formatSpeculativeDefault,
  formatSpeculativeRange,
  profileEstimateKey,
  type ProfileEstimates,
  projectModelRows,
  speculativeDefaultMethod,
} from "./model-row";

const { engineReleases, hardwareClasses, models } = syntheticCatalog();

function project(estimates: ProfileEstimates = new Map(), deployments = []) {
  return projectModelRows({ deployments, engineReleases, estimates, hardwareClasses, models });
}

function alphaProfile(profileId: string, estimates?: ProfileEstimates) {
  return project(estimates)[0].profiles.find((profile) => profile.profileId === profileId)!;
}

describe("projectModelRows", () => {
  it("projects a model with several configurations, engine kinds, artifacts, and profiles", () => {
    const [alpha] = project();

    expect(alpha).toMatchObject({
      colorKey: "alpha-7b",
      displayName: "Alpha 7B",
      modelId: "alpha-7b",
      profileCount: 5,
      requestKinds: [InferenceRequestKind.CHAT, InferenceRequestKind.DECISION],
      runningCount: 0,
      runningStatus: undefined,
      supportsVision: true,
    });
    expect(alpha.profiles.map((profile) => profile.profileId)).toEqual([
      "fast",
      "quality",
      "sized",
      "cpu",
      "orphan",
    ]);
    expect(alphaProfile("fast")).toEqual({
      acceleratorRequirements: [
        {
          displayName: "NVIDIA",
          hardwareClassId: "nvidia",
          minimumMemoryBytesPerDevice: 8_000_000_000,
          vendors: ["nvidia"],
        },
      ],
      configurationId: "cuda-llama",
      description: "Starts quickly.",
      displayName: "Fast",
      engineKinds: ["llama-cpp"],
      hardwareClassEstimates: [],
      scenarioUnsupportedClasses: [],
      imageInput: true,
      maximumContext: 262144,
      memory: { kind: "minimum", max: 8_000_000_000, min: 8_000_000_000 },
      platforms: [
        { architecture: "amd64", operatingSystem: "linux" },
        { architecture: "arm64", operatingSystem: "linux" },
      ],
      modelId: "alpha-7b",
      profileId: "fast",
      quantization: "Q4_K_M",
      referenceScore: 41.5,
      speculativeOptions: [
        { defaultCount: 3, displayName: "MTP", maximum: 8, minimum: 0, optionId: "mtp" },
      ],
      totalContext: undefined,
    });
    expect(alphaProfile("quality")).toMatchObject({
      description: undefined,
      quantization: "Q8_0",
    });
    expect(alphaProfile("sized")).toMatchObject({
      acceleratorRequirements: [
        {
          displayName: "NVIDIA",
          hardwareClassId: "nvidia",
          minimumMemoryBytesPerDevice: 24_000_000_000,
          vendors: ["nvidia"],
        },
      ],
      engineKinds: ["vllm"],
      hardwareClassEstimates: [],
      scenarioUnsupportedClasses: [],
      imageInput: false,
      // The vLLM configuration lowers the model's maximum context.
      maximumContext: 65536,
      memory: { kind: "minimum", max: 24_000_000_000, min: 24_000_000_000 },
      platforms: [{ architecture: "amd64", operatingSystem: "linux" }],
      quantization: "NVFP4",
    });
  });

  it("carries both platforms of a configuration with linux/amd64 and linux/arm64 engine artifacts", () => {
    expect(alphaProfile("quality").platforms).toEqual([
      { architecture: "amd64", operatingSystem: "linux" },
      { architecture: "arm64", operatingSystem: "linux" },
    ]);
  });

  it("leaves platforms absent for a profile whose engine release is missing, keeping the rest", () => {
    expect(alphaProfile("orphan")).toEqual({
      acceleratorRequirements: [
        {
          displayName: "NVIDIA",
          hardwareClassId: "nvidia",
          minimumMemoryBytesPerDevice: 6_000_000_000,
          vendors: ["nvidia"],
        },
      ],
      configurationId: "orphan",
      description: undefined,
      displayName: "Orphan",
      engineKinds: ["vllm"],
      hardwareClassEstimates: [],
      scenarioUnsupportedClasses: [],
      imageInput: false,
      maximumContext: 262144,
      memory: { kind: "minimum", max: 6_000_000_000, min: 6_000_000_000 },
      modelId: "alpha-7b",
      platforms: [],
      profileId: "orphan",
      quantization: "Q8_0",
      referenceScore: 41.5,
      speculativeOptions: [],
      totalContext: undefined,
    });
  });

  it("shows system RAM for a CPU-only profile and falls back to the profile id for its name", () => {
    expect(alphaProfile("cpu")).toMatchObject({
      acceleratorRequirements: [],
      displayName: "cpu",
      memory: { kind: "system-ram", max: 16_000_000_000, min: 16_000_000_000 },
    });
    // An estimate does not override the system RAM of a CPU-only profile.
    expect(alphaProfile("cpu", nvidiaEstimates([["alpha-7b", "cpu", 1_000n]])).memory).toEqual({
      kind: "system-ram",
      max: 16_000_000_000,
      min: 16_000_000_000,
    });
  });

  it("takes images on a configuration that implements a vision option without the image modality", () => {
    const model = clone(ModelSchema, models[0]);
    model.configurations
      .find((configuration) => configuration.configurationId === "cpu-llama")!
      .options.push(create(ModelConfigurationOptionSchema, { optionId: "vision" }));
    const [row] = projectModelRows({
      deployments: [],
      engineReleases,
      hardwareClasses,
      estimates: new Map(),
      models: [model],
    });

    expect(row.profiles.map((profile) => [profile.profileId, profile.imageInput])).toEqual([
      ["fast", true],
      ["quality", true],
      ["sized", false],
      ["cpu", true],
      ["orphan", false],
    ]);
  });

  it("carries every accelerator vendor of a multi-vendor hardware class", () => {
    const multiVendor = create(HardwareClassSchema, {
      displayName: "Any GPU",
      hardwareClassId: "nvidia",
      selector: { accelerators: ["nvidia", "apple"] },
    });
    const [row] = projectModelRows({
      deployments: [],
      engineReleases,
      estimates: new Map(),
      hardwareClasses: [multiVendor],
      models,
    });

    expect(
      row.profiles.find((profile) => profile.profileId === "fast")!.acceleratorRequirements,
    ).toEqual([
      {
        displayName: "Any GPU",
        hardwareClassId: "nvidia",
        minimumMemoryBytesPerDevice: 8_000_000_000,
        vendors: ["nvidia", "apple"],
      },
    ]);
  });

  it("projects chat-only, decision-only, and mixed request kinds", () => {
    const decisionOnly = clone(ModelSchema, models[1]);
    decisionOnly.configurations[0].requestKinds = [InferenceRequestKind.DECISION];
    const rows = projectModelRows({
      deployments: [],
      engineReleases,
      hardwareClasses,
      estimates: new Map(),
      models: [models[0], models[1], decisionOnly],
    });

    expect(rows.map((row) => row.requestKinds)).toEqual([
      [InferenceRequestKind.CHAT, InferenceRequestKind.DECISION],
      [InferenceRequestKind.CHAT],
      [InferenceRequestKind.DECISION],
    ]);
  });

  it("projects a model without hardware requirements with absent memory", () => {
    const bare = project()[1];

    expect(bare.supportsVision).toBe(false);
    expect(bare.profiles).toEqual([
      {
        acceleratorRequirements: [],
        configurationId: "plain",
        description: undefined,
        displayName: "plain",
        engineKinds: ["vllm"],
        hardwareClassEstimates: [],
        scenarioUnsupportedClasses: [],
        imageInput: false,
        maximumContext: undefined,
        memory: undefined,
        modelId: "bare",
        platforms: [{ architecture: "amd64", operatingSystem: "linux" }],
        profileId: "plain",
        quantization: undefined,
        referenceScore: undefined,
        speculativeOptions: [],
        totalContext: undefined,
      },
    ]);
  });

  it("derives speculative methods and per-profile options from the model, configuration, and profile", () => {
    const [alpha, bare] = project();

    expect(alpha.speculativeMethods).toEqual(["MTP"]);
    expect(bare.speculativeMethods).toEqual([]);
    // Configuration default 3 on cuda-llama.
    expect(alphaProfile("fast").speculativeOptions).toEqual([
      { defaultCount: 3, displayName: "MTP", maximum: 8, minimum: 0, optionId: "mtp" },
    ]);
    // The profile overrides the default to 0 and keeps the range.
    expect(alphaProfile("quality").speculativeOptions).toEqual([
      { defaultCount: 0, displayName: "MTP", maximum: 8, minimum: 0, optionId: "mtp" },
    ]);
    // Configurations without the option, on a model that offers it.
    expect(alphaProfile("sized").speculativeOptions).toEqual([]);
    expect(alphaProfile("cpu").speculativeOptions).toEqual([]);
  });

  it("formats the speculative default, range, and default method", () => {
    const fast = alphaProfile("fast").speculativeOptions;
    const quality = alphaProfile("quality").speculativeOptions;

    expect(formatSpeculativeDefault(fast)).toBe("MTP, 3 tokens");
    expect(formatSpeculativeDefault([{ ...fast[0], defaultCount: 1 }])).toBe("MTP, 1 token");
    expect(formatSpeculativeDefault(quality)).toBe("Off");
    expect(formatSpeculativeDefault([])).toBeUndefined();
    expect(formatSpeculativeRange(fast[0])).toBe("0–8 tokens");
    expect(speculativeDefaultMethod(fast)).toEqual(["MTP"]);
    expect(speculativeDefaultMethod(quality)).toEqual(["Off"]);
    expect(speculativeDefaultMethod([])).toEqual([]);
  });

  it("carries the model's score on the model row and every profile, and leaves it absent without one", () => {
    const [alpha, bare] = project();

    expect(alpha.referenceScore).toBe(41.5);
    expect(alpha.profiles.map((profile) => [profile.profileId, profile.referenceScore])).toEqual([
      ["fast", 41.5],
      ["quality", 41.5],
      ["sized", 41.5],
      ["cpu", 41.5],
      ["orphan", 41.5],
    ]);
    expect(bare.referenceScore).toBeUndefined();
    expect(bare.profiles.map((profile) => profile.referenceScore)).toEqual([undefined]);
  });

  it("prefers an estimate over the declared minimum and keeps the minimum when no estimate exists", () => {
    const estimates = nvidiaEstimates([
      ["alpha-7b", "fast", 7_500_000_000n],
      ["alpha-7b", "quality", 0n],
      ["bare", "plain", 3_000_000_000n],
    ]);
    const [alpha, bare] = project(estimates);

    expect(alpha.profiles.map((profile) => profile.memory)).toEqual([
      { kind: "estimate", max: 7_500_000_000, min: 7_500_000_000 },
      { kind: "minimum", max: 8_000_000_000, min: 8_000_000_000 },
      { kind: "minimum", max: 24_000_000_000, min: 24_000_000_000 },
      { kind: "system-ram", max: 16_000_000_000, min: 16_000_000_000 },
      { kind: "minimum", max: 6_000_000_000, min: 6_000_000_000 },
    ]);
    expect(bare.profiles[0].memory).toEqual({
      kind: "estimate",
      max: 3_000_000_000,
      min: 3_000_000_000,
    });
  });

  it("spans the memory across hardware classes and names each class's estimate", () => {
    const estimates = new Map([
      [
        profileEstimateKey("alpha-7b", "fast"),
        [
          { acceleratorMemoryBytes: 9_000_000_000n, hardwareClassId: "nvidia" },
          {
            acceleratorMemoryBytes: 7_500_000_000n,
            hardwareClassId: "nvidia-dedicated",
            hostMemoryBytes: 2_000_000_000n,
          },
        ],
      ],
    ]);
    const fast = alphaProfile("fast", estimates);

    expect(fast.memory).toEqual({ kind: "estimate", max: 9_000_000_000, min: 7_500_000_000 });
    // An unknown class reads as its id.
    expect(fast.hardwareClassEstimates).toEqual([
      { acceleratorMemoryBytes: 9_000_000_000n, displayName: "NVIDIA", hardwareClassId: "nvidia" },
      {
        acceleratorMemoryBytes: 7_500_000_000n,
        displayName: "nvidia-dedicated",
        hardwareClassId: "nvidia-dedicated",
        hostMemoryBytes: 2_000_000_000n,
      },
    ]);
  });

  it("reads the location's class without another class's estimate standing in for it", () => {
    // Fast has an estimate only for the apple class; its location selects another class.
    const estimates = new Map([
      [
        profileEstimateKey("alpha-7b", "fast"),
        [{ acceleratorMemoryBytes: 9_500_000_000n, hardwareClassId: "apple" }],
      ],
    ]);
    const atLocation = (classIds: string[], unsupportedClassIds: string[] = []) =>
      projectModelRows({
        deployments: [],
        engineReleases,
        estimates,
        hardwareClasses,
        models,
        sizing: {
          basis: "total",
          classIds: () => classIds,
          fit: () => ({ fit: "unknown" }),
          location: "gpu-box",
          totalContext: () => 409600,
          unsupportedClassIds: () => unsupportedClassIds,
          unsupportedReason: () => undefined,
        },
      })[0].profiles.find((profile) => profile.profileId === "fast")!;

    // The location's class has a declared minimum, which stands in for its missing estimate.
    expect(atLocation(["nvidia"]).memory).toEqual({
      kind: "minimum",
      max: 8_000_000_000,
      min: 8_000_000_000,
    });
    expect(atLocation(["nvidia"]).fit).toEqual({
      basis: "total",
      fit: "unknown",
      location: "gpu-box",
    });
    expect(atLocation(["nvidia"]).totalContext).toBe(409600);
    // A class with neither an estimate nor a minimum reads nothing.
    expect(atLocation(["gb10"]).memory).toBeUndefined();
    // Without a class at the location, every supported class applies.
    expect(atLocation([]).memory).toEqual({
      kind: "estimate",
      max: 9_500_000_000,
      min: 9_500_000_000,
    });
    expect(atLocation(["nvidia"], ["nvidia"]).scenarioUnsupportedClasses).toEqual([
      { displayName: "NVIDIA", hardwareClassId: "nvidia" },
    ]);
  });

  it("counts local deployments in any state by model id and ignores upstream deployments", () => {
    const [alphaModel, bareModel] = models;
    const deployments = [
      localDeployment("d1", alphaModel, { status: ModelDeploymentStatus.READY }),
      localDeployment("d2", alphaModel, { status: ModelDeploymentStatus.LOADING }),
      localDeployment("d3", alphaModel, { status: ModelDeploymentStatus.PAUSED }),
      upstreamDeployment(upstreamTemplate(), ModelDeploymentStatus.READY),
    ];
    const rows = projectModelRows({
      deployments,
      engineReleases,
      hardwareClasses,
      estimates: new Map(),
      models: [alphaModel, bareModel],
    });

    expect(rows.map((row) => [row.modelId, row.runningCount, row.runningStatus])).toEqual([
      ["alpha-7b", 3, { label: "3 deployed", tone: "success" }],
      ["bare", 0, undefined],
    ]);
  });
});
