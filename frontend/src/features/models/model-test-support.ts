import { create } from "@bufbuild/protobuf";

import { EngineReleaseSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { EngineKind } from "@/gen/dotcore/engine/v1/engine_pb";
import { ResolvedLocalModelDeploymentSnapshotSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ScalarResourceEstimateSchema } from "@/gen/dotcore/model/v1/service_pb";

export const model = create(ModelSchema, {
  modelId: "model-1",
  displayName: "Test Model",
  artifacts: [
    {
      artifactId: "q4",
      weightQuantization: "Q4_K_M",
      source: {
        repository: "test/model",
        revision: "1111111111111111111111111111111111111111",
        content: { case: "file", value: { path: "model-q4.gguf" } },
      },
    },
    {
      artifactId: "q8",
      weightQuantization: "Q8_0",
      source: {
        repository: "test/model",
        revision: "1111111111111111111111111111111111111111",
        content: { case: "snapshot", value: { files: [{ path: "config.json" }] } },
      },
    },
  ],
  configurations: [
    {
      configurationId: "cuda",
      engineArtifacts: [{ engineReleaseId: "llama", engineArtifactId: "cuda" }],
      artifactBindings: [
        { bindingId: "q4-binding", modelArtifactId: "q4" },
        { bindingId: "q8-binding", modelArtifactId: "q8" },
      ],
    },
  ],
  profiles: [
    {
      profileId: "default",
      displayName: "Fast start",
      description: "Balances quality and startup speed.",
      configurationId: "cuda",
      defaultModelArtifactBindingId: "q4-binding",
      engineParameterDefaults: {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: 8192n } },
        ],
      },
    },
    {
      profileId: "large",
      displayName: "Large context",
      description: "Provides a larger context window.",
      configurationId: "cuda",
      defaultModelArtifactBindingId: "q8-binding",
      engineParameterDefaults: {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: 16384n } },
        ],
      },
    },
  ],
});
export const releases = [
  create(EngineReleaseSchema, {
    engineReleaseId: "llama",
    version: "1",
    parameterSchema: {
      schemaId: "schema",
      parameters: [
        {
          parameterId: "total-context",
          displayName: "Total context",
          exposed: true,
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 1024n, maximum: 32768n, unit: "tokens" },
          },
        },
      ],
    },
  }),
];
export const multiParameterReleases = [
  create(EngineReleaseSchema, {
    engineReleaseId: "vllm",
    version: "1",
    parameterSchema: {
      schemaId: "vllm-schema",
      parameters: [
        {
          parameterId: "total-context",
          displayName: "Total context",
          exposed: true,
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 1024n, maximum: 131072n, unit: "tokens" },
          },
        },
        {
          parameterId: "parallel-requests",
          displayName: "Parallel requests",
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 1n, maximum: 1024n, unit: "requests" },
          },
        },
        {
          parameterId: "kv-cache-memory-bytes",
          displayName: "KV cache memory",
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 1n, maximum: 68719476736n, unit: "bytes" },
          },
        },
        {
          parameterId: "enable-prefix-caching",
          displayName: "Prefix caching",
          description: "Caches reusable prompt prefixes.",
          parameterType: { case: "boolean", value: {} },
        },
      ],
    },
  }),
];

// Declares several engine parameters so the form must supply the complete profile default set.
export const multiParameterModel = create(ModelSchema, {
  modelId: "model-2",
  displayName: "Wide Model",
  artifacts: [
    {
      artifactId: "nvfp4",
      weightQuantization: "NVFP4",
      source: {
        repository: "test/model",
        revision: "1111111111111111111111111111111111111111",
        content: { case: "snapshot", value: { files: [{ path: "config.json" }] } },
      },
    },
  ],
  configurations: [
    {
      configurationId: "vllm-cuda",
      engineArtifacts: [{ engineReleaseId: "vllm", engineArtifactId: "cuda" }],
      artifactBindings: [{ bindingId: "nvfp4-binding", modelArtifactId: "nvfp4" }],
    },
  ],
  profiles: [
    {
      profileId: "text",
      displayName: "NVFP4 text",
      description: "Runs the model through vLLM for text inference.",
      configurationId: "vllm-cuda",
      defaultModelArtifactBindingId: "nvfp4-binding",
      exposedParameterIds: ["enable-prefix-caching", "total-context"],
      engineParameterDefaults: {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: 32768n } },
          { parameterId: "parallel-requests", value: { case: "unsignedInteger", value: 4n } },
          {
            parameterId: "kv-cache-memory-bytes",
            value: { case: "unsignedInteger", value: 7400000000n },
          },
          { parameterId: "enable-prefix-caching", value: { case: "boolean", value: false } },
        ],
      },
    },
  ],
});
export const kvOffloadReleases = [
  create(EngineReleaseSchema, {
    engineReleaseId: "vllm-kv",
    version: "1",
    parameterSchema: {
      schemaId: "vllm-kv-schema",
      parameters: [
        {
          parameterId: "total-context",
          displayName: "Total context",
          exposed: true,
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 1024n, maximum: 131072n, unit: "tokens" },
          },
        },
        {
          parameterId: "kv-offloading-size-gib",
          exposed: true,
          displayName: "KV offloading size",
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 1n, maximum: 1024n, unit: "GiB" },
          },
        },
      ],
    },
  }),
];

// Declares the vLLM KV-offload parameter so the form must surface and send it.
export const kvOffloadModel = create(ModelSchema, {
  modelId: "model-kv",
  displayName: "KV Offload Model",
  artifacts: [
    {
      artifactId: "nvfp4",
      weightQuantization: "NVFP4",
      source: {
        repository: "test/model",
        revision: "1111111111111111111111111111111111111111",
        content: { case: "snapshot", value: { files: [{ path: "config.json" }] } },
      },
    },
  ],
  configurations: [
    {
      configurationId: "vllm-kv-cuda",
      engineArtifacts: [{ engineReleaseId: "vllm-kv", engineArtifactId: "cuda" }],
      artifactBindings: [{ bindingId: "nvfp4-binding", modelArtifactId: "nvfp4" }],
      engineConfiguration: {
        engineKind: EngineKind.VLLM,
        configuration: {
          case: "vllm",
          value: { fixedSettings: { kvOffloadingSizeGibParameterId: "kv-offloading-size-gib" } },
        },
      },
    },
  ],
  profiles: [
    {
      profileId: "nvfp4-text",
      displayName: "NVFP4 text",
      description: "Runs the model through vLLM for text inference.",
      configurationId: "vllm-kv-cuda",
      defaultModelArtifactBindingId: "nvfp4-binding",
      engineParameterDefaults: {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: 32768n } },
          {
            parameterId: "kv-offloading-size-gib",
            value: { case: "unsignedInteger", value: 16n },
          },
        ],
      },
    },
  ],
});

export const snapshot = create(ResolvedLocalModelDeploymentSnapshotSchema, {
  modelId: "model-1",
  modelDisplayName: "Test Model",
  modelVersion: "1",
  configurationId: "cuda",
  profileId: "default",
  engineParameterSchema: releases[0].parameterSchema,
  engineParameterValues: {
    values: [{ parameterId: "total-context", value: { case: "unsignedInteger", value: 8192n } }],
  },
  resolvedEngineState: {
    engineKind: EngineKind.LLAMA_CPP,
    semanticsVersion: 1,
    state: {
      case: "llamaCpp",
      value: { fixedSettings: { gpuLayers: 99, flashAttention: true } },
    },
  },
  desiredLaunchPlan: {
    arguments: [
      { value: { case: "literal", value: "--ctx-size" } },
      { value: { case: "literal", value: "8192" } },
    ],
  },
});

export function resourceEstimate(value: bigint, unit = "bytes", resource = "acceleratorMemory") {
  return create(ScalarResourceEstimateSchema, {
    resource,
    value,
    unit,
  });
}
