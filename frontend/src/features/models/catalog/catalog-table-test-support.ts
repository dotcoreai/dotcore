import { create, type MessageInitShape } from "@bufbuild/protobuf";

import { profileEstimateKey, type ProfileEstimates } from "@/features/models/profiles/model-row";
import {
  type EngineRelease,
  EngineReleaseSchema,
} from "@/gen/dotcore/catalog/v1/engine_release_pb";
import {
  type HardwareClass,
  HardwareClassSchema,
} from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import {
  InferenceRequestKind,
  ModelInputModality,
} from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  type Model,
  ModelSchema,
  SpeculativeDecodingMethod,
} from "@/gen/dotcore/catalog/v1/model_pb";
import { EngineKind } from "@/gen/dotcore/engine/v1/engine_pb";

/**
 * Profile estimates on the synthetic catalog's one `nvidia` class, from (model id, profile id,
 * accelerator memory bytes) entries.
 */
export function nvidiaEstimates(
  entries: readonly (readonly [string, string, bigint])[],
): ProfileEstimates {
  return new Map(
    entries.map(([modelId, profileId, acceleratorMemoryBytes]) => [
      profileEstimateKey(modelId, profileId),
      [{ acceleratorMemoryBytes, hardwareClassId: "nvidia" }],
    ]),
  );
}

type ParameterSchemaInit = NonNullable<
  MessageInitShape<typeof EngineReleaseSchema>["parameterSchema"]
>;

const revision = "1111111111111111111111111111111111111111";

/**
 * Synthetic catalog content for the Catalog table tests: engine releases for two engine kinds and
 * two models that cover several configurations, platforms, quantizations, a CPU-only profile, a
 * profile whose engine release is missing, and a model without hardware requirements, a score, or a licence.
 */
export function syntheticCatalog(): {
  engineReleases: EngineRelease[];
  hardwareClasses: HardwareClass[];
  models: Model[];
} {
  return {
    engineReleases: [llamaRelease(), vllmRelease()],
    hardwareClasses: [
      create(HardwareClassSchema, {
        displayName: "NVIDIA",
        hardwareClassId: "nvidia",
        selector: { accelerators: ["nvidia"] },
      }),
    ],
    models: [alphaModel(), bareModel()],
  };
}

function llamaRelease(): EngineRelease {
  return create(EngineReleaseSchema, {
    engineKind: EngineKind.LLAMA_CPP,
    engineReleaseId: "llama-1",
    parameterSchema: contextParameterSchema("llama-schema", true),
    version: "1",
    artifacts: [
      engineArtifact("linux-amd64-cuda", "amd64"),
      engineArtifact("linux-arm64-cuda", "arm64"),
      engineArtifact("linux-amd64-cpu", "amd64"),
    ],
  });
}

function vllmRelease(): EngineRelease {
  return create(EngineReleaseSchema, {
    engineKind: EngineKind.VLLM,
    engineReleaseId: "vllm-1",
    parameterSchema: contextParameterSchema("vllm-schema", false),
    version: "2",
    artifacts: [engineArtifact("linux-amd64-cuda", "amd64")],
  });
}

function engineArtifact(artifactId: string, architecture: string) {
  return {
    artifactId,
    selectors: [{ accelerators: ["nvidia"], platform: { architecture, operatingSystem: "linux" } }],
  };
}

// The engine exposes its total context to the profile only when `exposed` is set.
function contextParameterSchema(schemaId: string, exposed: boolean): ParameterSchemaInit {
  return {
    parameters: [
      {
        displayName: "Total context",
        exposed,
        parameterId: "total-context",
        parameterType: {
          case: "unsignedInteger" as const,
          value: { maximum: 262144n, minimum: 1024n, unit: "tokens" },
        },
      },
      {
        displayName: "Prefix caching",
        parameterId: "prefix-caching",
        parameterType: { case: "boolean" as const, value: {} },
      },
    ],
    schemaId,
  };
}

function contextDefaults(tokens: bigint) {
  return {
    values: [
      { parameterId: "total-context", value: { case: "unsignedInteger" as const, value: tokens } },
      { parameterId: "prefix-caching", value: { case: "boolean" as const, value: true } },
    ],
  };
}

function artifact(artifactId: string, weightQuantization: string) {
  return {
    artifactId,
    source: {
      content: { case: "file" as const, value: { path: `${artifactId}.gguf` } },
      repository: "test/alpha",
      revision,
    },
    weightQuantization,
  };
}

function alphaModel(): Model {
  return create(ModelSchema, {
    artifacts: [artifact("q4", "Q4_K_M"), artifact("q8", "Q8_0"), artifact("fp4", "NVFP4")],
    configurations: [
      {
        artifactBindings: [
          { bindingId: "q4-binding", modelArtifactId: "q4" },
          { bindingId: "q8-binding", modelArtifactId: "q8" },
        ],
        configurationId: "cuda-llama",
        requestKinds: [InferenceRequestKind.CHAT],
        engineArtifacts: [
          { engineArtifactId: "linux-amd64-cuda", engineReleaseId: "llama-1" },
          { engineArtifactId: "linux-arm64-cuda", engineReleaseId: "llama-1" },
        ],
        engineConfiguration: { engineKind: EngineKind.LLAMA_CPP },
        hardwareSupport: [
          {
            hardwareClassId: "nvidia",
            hardwareRequirements: {
              minimumMemoryBytesPerDevice: 8_000_000_000n,
              minimumSystemMemoryBytes: 4_000_000_000n,
            },
          },
        ],
        inputCapabilities: {
          supportedModalities: [ModelInputModality.TEXT, ModelInputModality.IMAGE],
        },
        options: [
          {
            defaultValues: tokenCountValues(3n),
            optionId: "mtp",
            parameters: [tokenCountParameter(8n)],
          },
        ],
      },
      {
        artifactBindings: [{ bindingId: "fp4-binding", modelArtifactId: "fp4" }],
        configurationId: "cuda-vllm",
        requestKinds: [InferenceRequestKind.DECISION],
        engineArtifacts: [{ engineArtifactId: "linux-amd64-cuda", engineReleaseId: "vllm-1" }],
        engineConfiguration: { engineKind: EngineKind.VLLM },
        hardwareSupport: [
          {
            hardwareClassId: "nvidia",
            hardwareRequirements: { minimumMemoryBytesPerDevice: 24_000_000_000n },
          },
        ],
        inputCapabilities: { supportedModalities: [ModelInputModality.TEXT] },
        maximumContextTokens: 65536,
      },
      {
        artifactBindings: [{ bindingId: "q4-cpu-binding", modelArtifactId: "q4" }],
        configurationId: "cpu-llama",
        requestKinds: [InferenceRequestKind.CHAT],
        engineArtifacts: [{ engineArtifactId: "linux-amd64-cpu", engineReleaseId: "llama-1" }],
        engineConfiguration: { engineKind: EngineKind.LLAMA_CPP },
        hardwareSupport: [
          {
            hardwareClassId: "host",
            hardwareRequirements: { minimumSystemMemoryBytes: 16_000_000_000n },
          },
        ],
      },
      {
        artifactBindings: [{ bindingId: "q8-orphan-binding", modelArtifactId: "q8" }],
        configurationId: "orphan",
        engineArtifacts: [{ engineArtifactId: "linux-amd64-cuda", engineReleaseId: "gone-1" }],
        engineConfiguration: { engineKind: EngineKind.VLLM },
        hardwareSupport: [
          {
            hardwareClassId: "nvidia",
            hardwareRequirements: { minimumMemoryBytesPerDevice: 6_000_000_000n },
          },
        ],
      },
    ],
    displayName: "Alpha 7B",
    intelligence: { overall: 41.5 },
    license: { id: "Apache-2.0", termsUrl: "https://www.apache.org/licenses/LICENSE-2.0" },
    maximumContextTokens: 262144,
    modelId: "alpha-7b",
    options: [
      { displayName: "Vision", kind: { case: "vision", value: {} }, optionId: "vision" },
      {
        displayName: "MTP",
        kind: {
          case: "speculativeDecoding",
          value: { method: SpeculativeDecodingMethod.MTP },
        },
        optionId: "mtp",
      },
    ],
    profiles: [
      {
        configurationId: "cuda-llama",
        defaultModelArtifactBindingId: "q4-binding",
        description: "Starts quickly.",
        displayName: "Fast",
        engineParameterDefaults: contextDefaults(8192n),
        profileId: "fast",
      },
      {
        configurationId: "cuda-llama",
        defaultModelArtifactBindingId: "q8-binding",
        displayName: "Quality",
        engineParameterDefaults: contextDefaults(131072n),
        optionDefaults: [tokenCountValues(0n)],
        profileId: "quality",
      },
      {
        configurationId: "cuda-vllm",
        defaultModelArtifactBindingId: "fp4-binding",
        displayName: "Sized",
        engineParameterDefaults: contextDefaults(65536n),
        profileId: "sized",
      },
      {
        configurationId: "cpu-llama",
        defaultModelArtifactBindingId: "q4-cpu-binding",
        engineParameterDefaults: contextDefaults(4096n),
        profileId: "cpu",
      },
      {
        configurationId: "orphan",
        defaultModelArtifactBindingId: "q8-orphan-binding",
        displayName: "Orphan",
        profileId: "orphan",
      },
    ],
    version: "1",
  });
}

function tokenCountParameter(maximum: bigint) {
  return {
    displayName: "Token count",
    parameterId: "token-count",
    parameterType: {
      case: "unsignedInteger" as const,
      value: { maximum, minimum: 0n, unit: "tokens" },
    },
  };
}

function tokenCountValues(count: bigint, optionId = "mtp") {
  return {
    optionId,
    values: [
      { parameterId: "token-count", value: { case: "unsignedInteger" as const, value: count } },
    ],
  };
}

function bareModel(): Model {
  return create(ModelSchema, {
    artifacts: [artifact("only", "")],
    configurations: [
      {
        artifactBindings: [{ bindingId: "only-binding", modelArtifactId: "only" }],
        configurationId: "plain",
        requestKinds: [InferenceRequestKind.CHAT],
        engineArtifacts: [{ engineArtifactId: "linux-amd64-cuda", engineReleaseId: "vllm-1" }],
        engineConfiguration: { engineKind: EngineKind.VLLM },
      },
    ],
    displayName: "Bare",
    modelId: "bare",
    profiles: [
      {
        configurationId: "plain",
        defaultModelArtifactBindingId: "only-binding",
        profileId: "plain",
      },
    ],
    version: "1",
  });
}
