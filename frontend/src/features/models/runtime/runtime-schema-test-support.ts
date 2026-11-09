import { clone, create } from "@bufbuild/protobuf";

import { EngineReleaseSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import {
  ModelConfigurationOptionSchema,
  ModelOptionSchema,
  ModelOptionValuesSchema,
  ModelSchema,
  SpeculativeDecodingMethod,
} from "@/gen/dotcore/catalog/v1/model_pb";
import { EngineKind } from "@/gen/dotcore/engine/v1/engine_pb";

// Engine releases and a model mirroring the master catalog `catalog/engines/{llama-cpp,vllm,ninfer}.yaml`
// parameter schemas and the `qwen3-8-27b` profiles that run on them. The llama.cpp configuration
// declares the MTP speculative option; the vLLM and ninfer ones leave it out.

export const llamaCppRelease = create(EngineReleaseSchema, {
  engineReleaseId: "llama-cpp-2026-07-28",
  engineKind: EngineKind.LLAMA_CPP,
  version: "b10156",
  parameterSchema: {
    schemaId: "llama-cpp-v2",
    parameters: [
      unsignedParameter("total-context", "Total context", true, 1024n, 16777216n, "tokens", {
        defaultValue: 32768n,
        description:
          "Context memory the server holds from the start and shares among the requests it generates at once. Each active request stores its own tokens in it, even when requests start with the same prompt. When it fills, the server frees idle requests first, then fails the active ones.",
      }),
      unsignedParameter(
        "context-per-request",
        "Context per request",
        true,
        1024n,
        16777216n,
        "tokens",
        {
          description:
            "Largest context one request may use from the total context. It reserves nothing: requests take memory from the total context as they grow. A new request reuses the start of an earlier prompt from an idle request or the prompt cache, but not from requests running at the same time.",
        },
      ),
      unsignedParameter("parallel-requests", "Parallel requests", true, 1n, 64n, "requests", {
        defaultValue: 4n,
        description:
          "Requests the server generates at the same time; further requests wait in a queue. All of them share the total context, so each added request only adds its own small state.",
      }),
      unsignedParameter("prompt-cache-memory-mib", "Prompt cache memory", true, 0n, 65536n, "MiB", {
        description:
          "Memory kept to reuse repeated prompts, such as agent system prompts, without processing them again. Idle conversations move here and free their accelerator memory. It uses host memory, which unified-memory machines share with the model; 0 turns the cache off.",
      }),
    ],
  },
});

export const vllmRelease = create(EngineReleaseSchema, {
  engineReleaseId: "vllm-qwen38-cu130",
  engineKind: EngineKind.VLLM,
  version: "qwen38-x86_64-cu130",
  parameterSchema: {
    schemaId: "vllm-v2",
    parameters: [
      unsignedParameter("total-context", "Total context", true, 1024n, 16777216n, "tokens", {
        defaultValue: 32768n,
        description:
          "Context memory the server holds from the start and shares among the requests it generates at once.",
      }),
      unsignedParameter(
        "context-per-request",
        "Context per request",
        true,
        1024n,
        1048576n,
        "tokens",
        { description: "Largest context one request may use from the total context." },
      ),
      unsignedParameter("parallel-requests", "Parallel requests", true, 1n, 1024n, "requests", {
        defaultValue: 4n,
        description:
          "Requests the server generates at the same time; further requests wait in a queue.",
      }),
      unsignedParameter(
        "maximum-batched-tokens",
        "Maximum batched tokens",
        false,
        1n,
        1048576n,
        "tokens",
      ),
      unsignedParameter(
        "maximum-cudagraph-capture-size",
        "Maximum CUDA graph capture size",
        false,
        1n,
        1024n,
        "sequences",
      ),
      unsignedParameter(
        "kv-cache-memory-bytes",
        "KV cache memory",
        false,
        1n,
        1099511627776n,
        "bytes",
      ),
      unsignedParameter("kv-offloading-size-gib", "KV offloading size", true, 0n, 1024n, "GiB", {
        description: "Host memory used to offload the KV cache, in GiB.",
      }),
    ],
  },
});

export const ninferRelease = create(EngineReleaseSchema, {
  engineReleaseId: "ninfer-68c5435",
  engineKind: EngineKind.NINFER,
  version: "68c5435",
  parameterSchema: {
    schemaId: "ninfer-v2",
    parameters: [
      unsignedParameter("total-context", "Total context", true, 1024n, 2097152n, "tokens", {
        defaultValue: 32768n,
        description:
          "Context memory the server holds from the start and shares among the requests it generates at once.",
      }),
      unsignedParameter(
        "context-per-request",
        "Context per request",
        true,
        1024n,
        262144n,
        "tokens",
        { description: "Largest context one request may use from the total context." },
      ),
      unsignedParameter("parallel-requests", "Parallel requests", true, 1n, 8n, "requests", {
        defaultValue: 4n,
        description:
          "Requests the server generates at the same time; further requests wait in a queue.",
      }),
      unsignedParameter("host-context-mib", "Host context memory", true, 0n, 1048576n, "MiB", {
        description:
          "Host memory where idle conversations and paused requests are kept, so they resume without processing their prompt again. 0 turns it off.",
      }),
      unsignedParameter("device-state-slots", "Device state slots", false, 0n, 64n, "slots"),
    ],
  },
});

export const runtimeSchemaReleases = [llamaCppRelease, vllmRelease, ninferRelease];

export const runtimeSchemaModel = create(ModelSchema, {
  modelId: "qwen3-8-27b",
  displayName: "Qwen3.8 27B",
  artifacts: [
    artifact("q4-xl", "Q4_K_XL"),
    artifact("nvfp4", "NVFP4"),
    artifact("ninfer-nvfp4", "NVFP4"),
  ],
  options: [
    {
      optionId: "mtp",
      displayName: "MTP",
      kind: { case: "speculativeDecoding", value: { method: SpeculativeDecodingMethod.MTP } },
    },
  ],
  configurations: [
    {
      configurationId: "nvidia-cuda",
      engineArtifacts: [
        { engineReleaseId: llamaCppRelease.engineReleaseId, engineArtifactId: "cuda13" },
      ],
      artifactBindings: [{ bindingId: "q4-xl", modelArtifactId: "q4-xl" }],
      options: [
        {
          optionId: "mtp",
          parameters: [
            {
              parameterId: "token-count",
              displayName: "Token count",
              parameterType: {
                case: "unsignedInteger",
                value: { minimum: 0n, maximum: 5n, unit: "tokens" },
              },
            },
          ],
          defaultValues: { optionId: "mtp", values: [unsignedValue("token-count", 3n)] },
        },
      ],
      engineConfiguration: {
        engineKind: EngineKind.LLAMA_CPP,
        configuration: { case: "llamaCpp", value: { fixedSettings: { gpuLayers: 99 } } },
      },
    },
    {
      configurationId: "vllm-nvidia-cuda",
      engineArtifacts: [
        { engineReleaseId: vllmRelease.engineReleaseId, engineArtifactId: "linux-amd64-cuda" },
      ],
      artifactBindings: [{ bindingId: "nvfp4", modelArtifactId: "nvfp4" }],
      engineConfiguration: {
        engineKind: EngineKind.VLLM,
        configuration: {
          case: "vllm",
          value: {
            fixedSettings: {
              kvCacheMemoryBytesParameterId: "kv-cache-memory-bytes",
              maximumBatchedTokensParameterId: "maximum-batched-tokens",
              maximumCudaGraphCaptureSizeParameterId: "maximum-cudagraph-capture-size",
              kvOffloadingSizeGibParameterId: "kv-offloading-size-gib",
            },
          },
        },
      },
    },
    {
      configurationId: "ninfer-nvidia-gb202",
      engineArtifacts: [
        { engineReleaseId: ninferRelease.engineReleaseId, engineArtifactId: "linux-amd64-cuda" },
      ],
      artifactBindings: [{ bindingId: "ninfer-nvfp4", modelArtifactId: "ninfer-nvfp4" }],
      engineConfiguration: {
        engineKind: EngineKind.NINFER,
        configuration: {
          case: "ninfer",
          value: {
            fixedSettings: {
              deviceStateSlotsParameterId: "device-state-slots",
              hostContextMibParameterId: "host-context-mib",
            },
          },
        },
      },
    },
  ],
  profiles: [
    {
      profileId: "q4-xl",
      displayName: "Q4_K_XL",
      configurationId: "nvidia-cuda",
      defaultModelArtifactBindingId: "q4-xl",
      engineParameterDefaults: {
        values: [unsignedValue("prompt-cache-memory-mib", 8192n)],
      },
    },
    {
      profileId: "q4-xl-no-mtp",
      displayName: "Q4_K_XL without MTP",
      configurationId: "nvidia-cuda",
      defaultModelArtifactBindingId: "q4-xl",
      engineParameterDefaults: {
        values: [unsignedValue("prompt-cache-memory-mib", 8192n)],
      },
      optionDefaults: [{ optionId: "mtp", values: [unsignedValue("token-count", 0n)] }],
    },
    {
      profileId: "nvfp4-text",
      displayName: "NVFP4 text",
      description: "Runs Qwen3.8 27B through vLLM for text inference.",
      configurationId: "vllm-nvidia-cuda",
      defaultModelArtifactBindingId: "nvfp4",
      engineParameterDefaults: {
        values: [
          unsignedValue("maximum-batched-tokens", 4096n),
          unsignedValue("maximum-cudagraph-capture-size", 32n),
          unsignedValue("kv-offloading-size-gib", 0n),
        ],
      },
    },
    {
      profileId: "ninfer-nvfp4",
      displayName: "NVFP4 ninfer",
      description: "Runs Qwen3.8 27B through ninfer on an RTX 5090 for text and image inference.",
      configurationId: "ninfer-nvidia-gb202",
      defaultModelArtifactBindingId: "ninfer-nvfp4",
      engineParameterDefaults: {
        values: [
          unsignedValue("parallel-requests", 4n),
          unsignedValue("host-context-mib", 20480n),
          unsignedValue("device-state-slots", 4n),
        ],
      },
    },
  ],
});

/** A string engine or option parameter value, as profile defaults and requests carry it. */
export function stringValue(parameterId: string, value: string) {
  return { parameterId, value: { case: "string" as const, value } };
}

/**
 * The fixture model whose llama.cpp configuration declares the KV cache precision option with
 * f16 and q8_0 choices defaulting to f16; `profileDefault` makes the "q4-xl" profile default to
 * another precision.
 */
export function modelWithKvCachePrecision(profileDefault?: string) {
  const model = clone(ModelSchema, runtimeSchemaModel);
  model.options.push(
    create(ModelOptionSchema, {
      optionId: "kv-cache",
      displayName: "KV cache precision",
      kind: { case: "kvCachePrecision", value: {} },
    }),
  );
  model.configurations[0].options.push(
    create(ModelConfigurationOptionSchema, {
      optionId: "kv-cache",
      parameters: [
        {
          parameterId: "precision",
          displayName: "Precision",
          parameterType: { case: "stringEnum", value: { values: ["f16", "q8_0"] } },
        },
      ],
      defaultValues: { optionId: "kv-cache", values: [stringValue("precision", "f16")] },
    }),
  );
  if (profileDefault)
    model.profiles[0].optionDefaults.push(
      create(ModelOptionValuesSchema, {
        optionId: "kv-cache",
        values: [stringValue("precision", profileDefault)],
      }),
    );
  return model;
}

/** An unsigned-integer engine parameter value, as profile defaults and requests carry it. */
export function unsignedValue(parameterId: string, value: bigint) {
  return { parameterId, value: { case: "unsignedInteger" as const, value } };
}

function unsignedParameter(
  parameterId: string,
  displayName: string,
  exposed: boolean,
  minimum: bigint,
  maximum: bigint,
  unit: string,
  { defaultValue, description = "" }: { defaultValue?: bigint; description?: string } = {},
) {
  return {
    parameterId,
    displayName,
    exposed,
    description,
    parameterType: {
      case: "unsignedInteger" as const,
      value: { minimum, maximum, unit, defaultValue },
    },
  };
}

function artifact(artifactId: string, weightQuantization: string) {
  return {
    artifactId,
    weightQuantization,
    source: {
      repository: "unsloth/Qwen3.8-27B",
      revision: "4ca720788d1e01f1bff70c033e0d0028fd02e502",
      content: { case: "file" as const, value: { path: `${artifactId}.bin` } },
    },
  };
}
