import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  ModelDeploymentDesiredState,
  ModelDeploymentSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  CreateLocalModelDeploymentRequestSchema,
  EstimateLocalModelResourcesRequestSchema,
  EstimateLocalModelResourcesResponseSchema,
  FollowDeploymentLogsRequestSchema,
  ListModelProfileCompatibilityResponseSchema,
  ModelCompatibilityStatus,
  PauseModelResponseSchema,
  ReadDeploymentLogsRequestSchema,
  ReadDeploymentLogsResponseSchema,
  ResumeModelResponseSchema,
} from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStateSchema, ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

const { catalogService, createGrpcWebTransport, modelService, createClient } = vi.hoisted(() => {
  const catalogService = {
    listEngineReleases: vi.fn(),
    listModels: vi.fn(),
    listUpstreamModelTemplates: vi.fn(),
  };
  const modelService = {
    listModelDeployments: vi.fn(),
    estimateLocalModelResources: vi.fn(),
    listModelProfileCompatibility: vi.fn(),
    createModelDeployment: vi.fn(),
    removeModelDeployment: vi.fn(),
    pauseModel: vi.fn(),
    resumeModel: vi.fn(),
    updateModelDeployment: vi.fn(),
    readDeploymentLogs: vi.fn(),
    followDeploymentLogs: vi.fn(),
    updateModelDeploymentMetadata: vi.fn(),
  };
  const createClient = vi.fn((...args: unknown[]) => {
    const [service] = args;
    return (service as { typeName?: string }).typeName === "dotcore.model.v1.ModelDeploymentService"
      ? modelService
      : catalogService;
  });

  return {
    catalogService,
    createGrpcWebTransport: vi.fn(() => ({}) as never),
    modelService,
    createClient,
  };
});

vi.mock("@connectrpc/connect-web", () => ({ createGrpcWebTransport }));
vi.mock("@connectrpc/connect", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@connectrpc/connect")>()),
  createClient,
}));

describe("Model client endpoint resolution", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    vi.unstubAllEnvs();
  });

  it("uses the loopback test fallback when no test management origin is configured", async () => {
    delete process.env.VITE_DOTCORE_MANAGEMENT_ORIGIN;

    const { modelClient } = await import("./model-client");

    expect(modelClient).toBeDefined();
    expect(createGrpcWebTransport).toHaveBeenCalledWith({
      baseUrl: "http://127.0.0.1:50051",
      interceptors: [expect.any(Function)],
    });
    expect(createClient).toHaveBeenCalledTimes(2);
    expect(createClient.mock.calls.map(([service]) => service)).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ typeName: "dotcore.catalog.v1.CatalogService" }),
        expect.objectContaining({ typeName: "dotcore.model.v1.ModelDeploymentService" }),
      ]),
    );
  });

  it("uses the Vite-exposed workspace management origin", async () => {
    vi.stubEnv("VITE_DOTCORE_MANAGEMENT_ORIGIN", "http://100.64.0.7:6207");

    await import("./model-client");

    expect(createGrpcWebTransport).toHaveBeenCalledWith({
      baseUrl: "http://100.64.0.7:6207",
      interceptors: [expect.any(Function)],
    });
  });

  it("keeps explicit client URL injection available", async () => {
    const { createModelClient } = await import("./model-client");

    createModelClient("http://127.0.0.1:50052");

    expect(createGrpcWebTransport).toHaveBeenLastCalledWith({
      baseUrl: "http://127.0.0.1:50052",
      interceptors: [expect.any(Function)],
    });
  });

  it("forwards immutable engine release schema reads through the catalog client", async () => {
    const response = { engineReleases: [] } as never;
    catalogService.listEngineReleases.mockResolvedValue(response);
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");

    await expect(client.catalog.listEngineReleases({})).resolves.toBe(response);
    expect(catalogService.listEngineReleases).toHaveBeenCalledExactlyOnceWith({});
  });

  it("forwards exact local deployment and execution-target requests", async () => {
    const response = { deployment: undefined } as never;
    modelService.createModelDeployment.mockResolvedValue(response);
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");
    const exposure = { kind: { case: "hidden", value: {} } } as never;
    const localRequest = create(CreateLocalModelDeploymentRequestSchema, {
      modelId: "model-42",
      profileId: "fast-start",
      placement: { siteNodeId: "node", executionTargetId: "pool-1" },
    });

    await expect(
      client.model.createLocalModelDeployment?.({ ...localRequest, exposure }),
    ).resolves.toBe(response);
    expect(modelService.createModelDeployment).toHaveBeenCalledWith({
      source: {
        case: "local",
        value: localRequest,
      },
      exposure,
      modelDeploymentName: undefined,
    });
  });

  it("forwards exact local resource-estimate requests and responses", async () => {
    const response = create(EstimateLocalModelResourcesResponseSchema, {
      hardwareClassEstimates: [
        {
          estimates: [{ resource: "acceleratorMemory", value: 3_418_638_016n, unit: "bytes" }],
          hardwareClassId: "nvidia-dedicated",
        },
      ],
    });
    modelService.estimateLocalModelResources.mockResolvedValue(response);
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");
    const request = create(EstimateLocalModelResourcesRequestSchema, {
      modelId: "model-42",
      profileId: "fast-start",
      configuration: {
        modelArtifactBindingId: "q4-k-m",
        engineParameterValues: {
          values: [
            {
              parameterId: "total-context",
              value: { case: "unsignedInteger", value: 1024n },
            },
          ],
        },
        customArgumentTokens: ["--threads", "8"],
      },
    });

    await expect(client.model.estimateLocalModelResources?.(request)).resolves.toBe(response);
    expect(modelService.estimateLocalModelResources).toHaveBeenCalledExactlyOnceWith(request);
  });

  it("preserves an empty local resource-estimate response", async () => {
    const response = create(EstimateLocalModelResourcesResponseSchema, {
      hardwareClassEstimates: [],
    });
    modelService.estimateLocalModelResources.mockResolvedValue(response);
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");
    const request = create(EstimateLocalModelResourcesRequestSchema, {
      modelId: "model-42",
      profileId: "fast-start",
      configuration: {
        modelArtifactBindingId: "q4-k-m",
        engineParameterValues: { values: [] },
        customArgumentTokens: [],
      },
    });

    await expect(client.model.estimateLocalModelResources?.(request)).resolves.toBe(response);
  });

  it("preserves local resource-estimate transport failures", async () => {
    const error = new ConnectError("estimate unavailable", Code.Unavailable);
    modelService.estimateLocalModelResources.mockRejectedValue(error);
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");
    const request = create(EstimateLocalModelResourcesRequestSchema, {
      modelId: "model-42",
      profileId: "fast-start",
      configuration: {
        modelArtifactBindingId: "q4-k-m",
        engineParameterValues: { values: [] },
        customArgumentTokens: [],
      },
    });

    await expect(client.model.estimateLocalModelResources?.(request)).rejects.toBe(error);
  });

  it("forwards profile compatibility requests and responses", async () => {
    const response = create(ListModelProfileCompatibilityResponseSchema, {
      entries: [
        {
          modelId: "model-42",
          profileId: "fast-start",
          status: ModelCompatibilityStatus.RUNS,
          target: { executionTargetId: "gpu", siteNodeId: "node" },
        },
      ],
      targets: [
        {
          status: ModelCompatibilityStatus.RUNS,
          target: { executionTargetId: "gpu", siteNodeId: "node" },
        },
      ],
    });
    modelService.listModelProfileCompatibility.mockResolvedValue(response);
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");

    await expect(
      client.model.listModelProfileCompatibility?.({ modelId: "model-42" }),
    ).resolves.toBe(response);
    expect(modelService.listModelProfileCompatibility).toHaveBeenCalledExactlyOnceWith({
      modelId: "model-42",
    });
  });

  it("preserves the ordinary failed-precondition message without custom detail decoding", async () => {
    const error = new ConnectError(
      "Upstream endpoint preflight failed: The endpoint could not be reached; request URL: http://10.0.0.9:8000/v1/models; cause: network connection failed",
      Code.FailedPrecondition,
    );
    modelService.createModelDeployment.mockRejectedValue(error);

    const { createModelClient } = await import("./model-client");
    const client = createModelClient();

    await expect(client.model.createModelDeployment({})).rejects.toBe(error);
    expect(error.code).toBe(Code.FailedPrecondition);
    expect(error.rawMessage).toBe(
      "Upstream endpoint preflight failed: The endpoint could not be reached; request URL: http://10.0.0.9:8000/v1/models; cause: network connection failed",
    );
    expect(error.details).toEqual([]);
  });

  it("keeps status-only and malformed transport errors as ordinary ConnectErrors", async () => {
    const statusOnly = new ConnectError("", Code.FailedPrecondition);
    const malformed = new ConnectError("fallback", Code.FailedPrecondition);
    malformed.details = [{ type: "unknown", value: Uint8Array.of(255) }];
    modelService.createModelDeployment.mockRejectedValueOnce(statusOnly);
    modelService.updateModelDeployment.mockRejectedValueOnce(malformed);

    const { createModelClient } = await import("./model-client");
    const client = createModelClient();

    await expect(client.model.createModelDeployment({})).rejects.toBe(statusOnly);
    await expect(client.model.updateModelDeployment({})).rejects.toBe(malformed);
  });

  it("forwards exact typed Update requests and accepted responses", async () => {
    const response = { deployment: undefined } as never;
    modelService.updateModelDeployment.mockResolvedValue(response);
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");
    const request = {
      modelDeploymentId: "deployment-42",
      upstreamDesiredConfiguration: { upstreamModelId: "private-model", apiKey: "" },
    };

    await expect(client.model.updateModelDeployment?.(request as never)).resolves.toBe(response);
    expect(modelService.updateModelDeployment).toHaveBeenCalledExactlyOnceWith(request);
  });

  it("sends exact model log requests and forwards cancellation", async () => {
    modelService.readDeploymentLogs.mockResolvedValue(create(ReadDeploymentLogsResponseSchema));
    modelService.followDeploymentLogs.mockReturnValue(
      (function* () {
        yield { event: { case: undefined, value: undefined } };
      })(),
    );
    const signal = new AbortController().signal;
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");
    const beforeTimestamp = { seconds: 42n, nanos: 7 };
    const resumePositions = [{ sourceName: "worker", latestTimestamp: beforeTimestamp }];

    await client.logs?.read(
      { deploymentId: "deployment", sourceNames: ["worker"], pageSize: 5, beforeTimestamp },
      signal,
    );
    await (async () => {
      for await (const _event of client.logs?.follow(
        { deploymentId: "deployment", sourceNames: ["worker"], resumePositions },
        signal,
      ) ?? []) {
        void _event;
      }
    })();

    expect(modelService.readDeploymentLogs).toHaveBeenCalledWith(
      create(ReadDeploymentLogsRequestSchema, {
        modelDeploymentId: "deployment",
        sourceNames: ["worker"],
        pageSize: 5,
        beforeTimestamp,
      }),
      { signal },
    );
    expect(modelService.followDeploymentLogs).toHaveBeenCalledWith(
      create(FollowDeploymentLogsRequestSchema, {
        modelDeploymentId: "deployment",
        sourceNames: ["worker"],
        resumePositions,
      }),
      { signal },
    );
  });

  it("sends only normalized metadata Name fields and the exact update mask", async () => {
    const accepted = deployment(
      "deployment-42",
      ModelDeploymentDesiredState.PRESENT,
      ModelDeploymentStatus.READY,
    );
    accepted.modelDeploymentName = "Research Model";
    const response = { deployment: accepted } as never;
    modelService.updateModelDeploymentMetadata.mockResolvedValue(response);
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");

    await expect(
      client.model.updateModelDeploymentMetadata("deployment-42", {
        metadata: { displayName: "  Research Model  " },
        fields: ["displayName"],
      }),
    ).resolves.toEqual({ displayName: "Research Model" });
    expect(modelService.updateModelDeploymentMetadata).toHaveBeenCalledExactlyOnceWith({
      modelDeploymentId: "deployment-42",
      metadata: { displayName: "Research Model" },
      updateMask: { paths: ["display_name"] },
    });
  });

  it.each([
    ["missing accepted deployment", { deployment: undefined }],
    [
      "mismatched accepted deployment ID",
      {
        deployment: deployment(
          "deployment-other",
          ModelDeploymentDesiredState.PRESENT,
          ModelDeploymentStatus.READY,
        ),
      },
    ],
  ])("rejects %s", async (_case, response) => {
    modelService.updateModelDeploymentMetadata.mockResolvedValue(response);
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");

    await expect(
      client.model.updateModelDeploymentMetadata("deployment-42", {
        metadata: { displayName: "  Research Model  " },
        fields: ["displayName"],
      }),
    ).rejects.toThrow(
      "Update metadata response protocol error: missing or mismatched accepted deployment ID.",
    );
  });

  it("rejects an accepted deployment name that differs from the normalized request", async () => {
    const accepted = deployment(
      "deployment-42",
      ModelDeploymentDesiredState.PRESENT,
      ModelDeploymentStatus.READY,
    );
    accepted.modelDeploymentName = "Different Model";
    modelService.updateModelDeploymentMetadata.mockResolvedValue({ deployment: accepted });
    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");

    await expect(
      client.model.updateModelDeploymentMetadata("deployment-42", {
        metadata: { displayName: "  Research Model  " },
        fields: ["displayName"],
      }),
    ).rejects.toThrow(
      "Update metadata response protocol error: accepted deployment name does not match the requested normalized name.",
    );
  });

  it("sends exact Pause and Resume requests and returns accepted non-terminal responses", async () => {
    const paused = deployment(
      "deployment",
      ModelDeploymentDesiredState.PAUSED,
      ModelDeploymentStatus.PAUSING,
    );
    const resumed = deployment(
      "deployment",
      ModelDeploymentDesiredState.PRESENT,
      ModelDeploymentStatus.LOADING,
    );
    const pauseResponse = create(PauseModelResponseSchema, { deployment: paused });
    const resumeResponse = create(ResumeModelResponseSchema, { deployment: resumed });
    modelService.pauseModel.mockResolvedValue(pauseResponse);
    modelService.resumeModel.mockResolvedValue(resumeResponse);

    const { createModelClient } = await import("./model-client");
    const client = createModelClient("http://127.0.0.1:50052");

    await expect(client.model.pauseModel({ modelDeploymentId: "deployment" })).resolves.toBe(
      pauseResponse,
    );
    await expect(client.model.resumeModel({ modelDeploymentId: "deployment" })).resolves.toBe(
      resumeResponse,
    );
    expect(modelService.pauseModel).toHaveBeenCalledWith({ modelDeploymentId: "deployment" });
    expect(modelService.resumeModel).toHaveBeenCalledWith({ modelDeploymentId: "deployment" });
  });

  it.each([
    [
      "missing pause deployment",
      "pauseModel",
      create(PauseModelResponseSchema),
      "Pause response protocol error: missing or mismatched accepted deployment ID.",
    ],
    [
      "mismatched pause deployment",
      "pauseModel",
      create(PauseModelResponseSchema, {
        deployment: deployment(
          "other",
          ModelDeploymentDesiredState.PAUSED,
          ModelDeploymentStatus.PAUSING,
        ),
      }),
      "Pause response protocol error: missing or mismatched accepted deployment ID.",
    ],
    [
      "wrong pause desired state",
      "pauseModel",
      create(PauseModelResponseSchema, {
        deployment: deployment(
          "deployment",
          ModelDeploymentDesiredState.PRESENT,
          ModelDeploymentStatus.PAUSING,
        ),
      }),
      "Pause response protocol error: accepted deployment is not PAUSED.",
    ],
    [
      "wrong pause lifecycle",
      "pauseModel",
      create(PauseModelResponseSchema, {
        deployment: deployment(
          "deployment",
          ModelDeploymentDesiredState.PAUSED,
          ModelDeploymentStatus.READY,
        ),
      }),
      "Pause response protocol error: accepted deployment has an invalid status.",
    ],
    [
      "missing resume deployment",
      "resumeModel",
      create(ResumeModelResponseSchema),
      "Resume response protocol error: missing or mismatched accepted deployment ID.",
    ],
    [
      "wrong resume desired state",
      "resumeModel",
      create(ResumeModelResponseSchema, {
        deployment: deployment(
          "deployment",
          ModelDeploymentDesiredState.PAUSED,
          ModelDeploymentStatus.LOADING,
        ),
      }),
      "Resume response protocol error: accepted deployment is not PRESENT.",
    ],
    [
      "wrong resume lifecycle",
      "resumeModel",
      create(ResumeModelResponseSchema, {
        deployment: deployment(
          "deployment",
          ModelDeploymentDesiredState.PRESENT,
          ModelDeploymentStatus.PAUSED,
        ),
      }),
      "Resume response protocol error: accepted deployment has an invalid status.",
    ],
  ])(
    "rejects the %s response with an accessible protocol error",
    async (_case, method, response, message) => {
      const mutation = method as MutationMethod;
      const operation = modelService[mutation];
      operation.mockResolvedValue(response);

      const { createModelClient } = await import("./model-client");
      const client = createModelClient();

      await expect(
        client.model[mutation]({
          modelDeploymentId: "deployment",
        }),
      ).rejects.toThrow(message);
    },
  );

  it.each([
    ["pauseModel", Code.FailedPrecondition],
    ["resumeModel", Code.Unavailable],
  ])("preserves rejected and outcome-uncertain %s errors", async (method, code) => {
    const error = new ConnectError("controller response", code);
    const mutation = method as MutationMethod;
    modelService[mutation].mockRejectedValue(error);

    const { createModelClient } = await import("./model-client");
    const client = createModelClient();

    await expect(client.model[mutation]({ modelDeploymentId: "deployment" })).rejects.toBe(error);
  });
});

type MutationMethod = "pauseModel" | "resumeModel";

function deployment(
  modelDeploymentId: string,
  desiredState: ModelDeploymentDesiredState,
  status: ModelDeploymentStatus,
) {
  return create(ModelDeploymentSchema, {
    modelDeploymentId,
    desiredState,
    state: create(ModelDeploymentStateSchema, { status }),
  });
}
