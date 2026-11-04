import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  ApplicationDeploymentDesiredState,
  ApplicationDeploymentSchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  CreateApplicationDeploymentResponseSchema,
  FollowDeploymentLogsRequestSchema,
  PauseDeploymentResponseSchema,
  ReadDeploymentLogsRequestSchema,
  ReadDeploymentLogsResponseSchema,
  ResumeDeploymentResponseSchema,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationDeploymentStateSchema,
  ApplicationLifecycleState,
} from "@/gen/dotcore/application/v1/state_pb";

const { applicationService, createGrpcWebTransport, createClient } = vi.hoisted(() => {
  const applicationService = {
    listDeployments: vi.fn(),
    getDeployment: vi.fn(),
    createApplicationDeployment: vi.fn(),
    removeDeployment: vi.fn(),
    pauseDeployment: vi.fn(),
    resumeDeployment: vi.fn(),
    readDeploymentLogs: vi.fn(),
    followDeploymentLogs: vi.fn(),
    updateApplicationDeploymentMetadata: vi.fn(),
  };
  const createClient = vi.fn((...args: unknown[]) => {
    const [service] = args;
    return (service as { typeName?: string }).typeName ===
      "dotcore.application.v1.ApplicationDeploymentService"
      ? applicationService
      : {};
  });

  return {
    applicationService,
    createGrpcWebTransport: vi.fn(() => ({}) as never),
    createClient,
  };
});

vi.mock("@connectrpc/connect-web", () => ({ createGrpcWebTransport }));
vi.mock("@connectrpc/connect", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@connectrpc/connect")>()),
  createClient,
}));

describe("Application client endpoint resolution", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    vi.unstubAllEnvs();
  });

  it("uses the established gRPC-Web test fallback", async () => {
    delete process.env.VITE_DOTCORE_MANAGEMENT_ORIGIN;

    const { applicationClient } = await import("./application-client");

    expect(applicationClient).toBeDefined();
    expect(createGrpcWebTransport).toHaveBeenCalledWith({
      baseUrl: "http://127.0.0.1:50051",
      interceptors: [expect.any(Function)],
    });
    expect(createClient).toHaveBeenCalledTimes(3);
    expect(createClient.mock.calls.map(([service]) => service)).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ typeName: "dotcore.catalog.v1.CatalogService" }),
        expect.objectContaining({ typeName: "dotcore.site.v1.SiteService" }),
        expect.objectContaining({
          typeName: "dotcore.application.v1.ApplicationDeploymentService",
        }),
      ]),
    );
  });

  it("uses the Vite-exposed workspace management origin", async () => {
    vi.stubEnv("VITE_DOTCORE_MANAGEMENT_ORIGIN", "http://100.64.0.7:6207");

    await import("./application-client");

    expect(createGrpcWebTransport).toHaveBeenCalledWith({
      baseUrl: "http://100.64.0.7:6207",
      interceptors: [expect.any(Function)],
    });
  });

  it("keeps explicit client URL injection available", async () => {
    const { createApplicationClient } = await import("./application-client");

    createApplicationClient("http://127.0.0.1:50052");

    expect(createGrpcWebTransport).toHaveBeenLastCalledWith({
      baseUrl: "http://127.0.0.1:50052",
      interceptors: [expect.any(Function)],
    });
  });

  it("sends exact application log requests and forwards cancellation", async () => {
    applicationService.readDeploymentLogs.mockResolvedValue(
      create(ReadDeploymentLogsResponseSchema),
    );
    applicationService.followDeploymentLogs.mockReturnValue(
      (function* () {
        yield { event: { case: undefined, value: undefined } };
      })(),
    );
    const signal = new AbortController().signal;
    const { createApplicationClient } = await import("./application-client");
    const client = createApplicationClient("http://127.0.0.1:50052");
    const beforeTimestamp = { seconds: 42n, nanos: 7 };
    const resumePositions = [{ sourceName: "api", latestTimestamp: beforeTimestamp }];

    await client.logs?.read(
      { deploymentId: "deployment", sourceNames: ["api"], pageSize: 5, beforeTimestamp },
      signal,
    );
    await (async () => {
      for await (const _event of client.logs?.follow(
        { deploymentId: "deployment", sourceNames: ["api"], resumePositions },
        signal,
      ) ?? []) {
        void _event;
      }
    })();

    expect(applicationService.readDeploymentLogs).toHaveBeenCalledWith(
      create(ReadDeploymentLogsRequestSchema, {
        applicationDeploymentId: "deployment",
        sourceNames: ["api"],
        pageSize: 5,
        beforeTimestamp,
      }),
      { signal },
    );
    expect(applicationService.followDeploymentLogs).toHaveBeenCalledWith(
      create(FollowDeploymentLogsRequestSchema, {
        applicationDeploymentId: "deployment",
        sourceNames: ["api"],
        resumePositions,
      }),
      { signal },
    );
  });

  it("preserves an optional custom name in the exact application create request", async () => {
    const accepted = create(ApplicationDeploymentSchema, {
      ...deployment(
        "deployment",
        ApplicationDeploymentDesiredState.PRESENT,
        ApplicationLifecycleState.READY,
      ),
      applicationDeploymentName: "Custom • App",
    });
    const response = create(CreateApplicationDeploymentResponseSchema, { deployment: accepted });
    applicationService.createApplicationDeployment.mockResolvedValue(response);

    const { createApplicationClient } = await import("./application-client");
    const client = createApplicationClient("http://127.0.0.1:50052");
    const request = {
      applicationId: "open-webui",
      placement: { siteNodeId: "local", executionTargetId: "local" },
      applicationDeploymentName: "Custom • App",
    };

    await expect(client.application.createApplicationDeployment(request)).resolves.toBe(response);
    expect(applicationService.createApplicationDeployment).toHaveBeenCalledExactlyOnceWith(request);
  });

  it("sends only normalized metadata Name fields and the exact update mask", async () => {
    const accepted = deployment(
      "deployment-42",
      ApplicationDeploymentDesiredState.PRESENT,
      ApplicationLifecycleState.READY,
    );
    accepted.applicationDeploymentName = "Research App";
    const response = { deployment: accepted } as never;
    applicationService.updateApplicationDeploymentMetadata.mockResolvedValue(response);
    const { createApplicationClient } = await import("./application-client");
    const client = createApplicationClient("http://127.0.0.1:50052");

    await expect(
      client.application.updateApplicationDeploymentMetadata("deployment-42", {
        metadata: { displayName: "  Research App  " },
        fields: ["displayName"],
      }),
    ).resolves.toEqual({ displayName: "Research App" });
    expect(applicationService.updateApplicationDeploymentMetadata).toHaveBeenCalledExactlyOnceWith({
      applicationDeploymentId: "deployment-42",
      metadata: { displayName: "Research App" },
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
          ApplicationDeploymentDesiredState.PRESENT,
          ApplicationLifecycleState.READY,
        ),
      },
    ],
  ])("rejects %s", async (_case, response) => {
    applicationService.updateApplicationDeploymentMetadata.mockResolvedValue(response);
    const { createApplicationClient } = await import("./application-client");
    const client = createApplicationClient("http://127.0.0.1:50052");

    await expect(
      client.application.updateApplicationDeploymentMetadata("deployment-42", {
        metadata: { displayName: "  Research App  " },
        fields: ["displayName"],
      }),
    ).rejects.toThrow(
      "Update metadata response protocol error: missing or mismatched accepted deployment ID.",
    );
  });

  it("rejects an accepted deployment name that differs from the normalized request", async () => {
    const accepted = deployment(
      "deployment-42",
      ApplicationDeploymentDesiredState.PRESENT,
      ApplicationLifecycleState.READY,
    );
    accepted.applicationDeploymentName = "Different App";
    applicationService.updateApplicationDeploymentMetadata.mockResolvedValue({
      deployment: accepted,
    });
    const { createApplicationClient } = await import("./application-client");
    const client = createApplicationClient("http://127.0.0.1:50052");

    await expect(
      client.application.updateApplicationDeploymentMetadata("deployment-42", {
        metadata: { displayName: "  Research App  " },
        fields: ["displayName"],
      }),
    ).rejects.toThrow(
      "Update metadata response protocol error: accepted deployment name does not match the requested normalized name.",
    );
  });

  it("sends exact Pause and Resume requests and returns accepted non-terminal responses", async () => {
    const paused = deployment(
      "deployment",
      ApplicationDeploymentDesiredState.PAUSED,
      ApplicationLifecycleState.PAUSING,
    );
    const resumed = deployment(
      "deployment",
      ApplicationDeploymentDesiredState.PRESENT,
      ApplicationLifecycleState.STARTING,
    );
    const pauseResponse = create(PauseDeploymentResponseSchema, { deployment: paused });
    const resumeResponse = create(ResumeDeploymentResponseSchema, { deployment: resumed });
    applicationService.pauseDeployment.mockResolvedValue(pauseResponse);
    applicationService.resumeDeployment.mockResolvedValue(resumeResponse);

    const { createApplicationClient } = await import("./application-client");
    const client = createApplicationClient("http://127.0.0.1:50052");

    await expect(
      client.application.pauseDeployment({ applicationDeploymentId: "deployment" }),
    ).resolves.toBe(pauseResponse);
    await expect(
      client.application.resumeDeployment({ applicationDeploymentId: "deployment" }),
    ).resolves.toBe(resumeResponse);
    expect(applicationService.pauseDeployment).toHaveBeenCalledWith({
      applicationDeploymentId: "deployment",
    });
    expect(applicationService.resumeDeployment).toHaveBeenCalledWith({
      applicationDeploymentId: "deployment",
    });
  });

  it.each([
    [
      "missing pause deployment",
      "pauseDeployment",
      create(PauseDeploymentResponseSchema),
      "Pause response protocol error: missing or mismatched accepted deployment ID.",
    ],
    [
      "mismatched pause deployment",
      "pauseDeployment",
      create(PauseDeploymentResponseSchema, {
        deployment: deployment(
          "other",
          ApplicationDeploymentDesiredState.PAUSED,
          ApplicationLifecycleState.PAUSING,
        ),
      }),
      "Pause response protocol error: missing or mismatched accepted deployment ID.",
    ],
    [
      "wrong pause desired state",
      "pauseDeployment",
      create(PauseDeploymentResponseSchema, {
        deployment: deployment(
          "deployment",
          ApplicationDeploymentDesiredState.PRESENT,
          ApplicationLifecycleState.PAUSING,
        ),
      }),
      "Pause response protocol error: accepted deployment is not PAUSED.",
    ],
    [
      "wrong pause lifecycle",
      "pauseDeployment",
      create(PauseDeploymentResponseSchema, {
        deployment: deployment(
          "deployment",
          ApplicationDeploymentDesiredState.PAUSED,
          ApplicationLifecycleState.READY,
        ),
      }),
      "Pause response protocol error: accepted deployment has an invalid lifecycle state.",
    ],
    [
      "missing resume deployment",
      "resumeDeployment",
      create(ResumeDeploymentResponseSchema),
      "Resume response protocol error: missing or mismatched accepted deployment ID.",
    ],
    [
      "wrong resume desired state",
      "resumeDeployment",
      create(ResumeDeploymentResponseSchema, {
        deployment: deployment(
          "deployment",
          ApplicationDeploymentDesiredState.PAUSED,
          ApplicationLifecycleState.STARTING,
        ),
      }),
      "Resume response protocol error: accepted deployment is not PRESENT.",
    ],
    [
      "wrong resume lifecycle",
      "resumeDeployment",
      create(ResumeDeploymentResponseSchema, {
        deployment: deployment(
          "deployment",
          ApplicationDeploymentDesiredState.PRESENT,
          ApplicationLifecycleState.PAUSED,
        ),
      }),
      "Resume response protocol error: accepted deployment has an invalid lifecycle state.",
    ],
  ])(
    "rejects the %s response with an accessible protocol error",
    async (_case, method, response, message) => {
      const mutation = method as MutationMethod;
      const operation = applicationService[mutation];
      operation.mockResolvedValue(response);

      const { createApplicationClient } = await import("./application-client");
      const client = createApplicationClient();

      await expect(
        client.application[mutation]({ applicationDeploymentId: "deployment" }),
      ).rejects.toThrow(message);
    },
  );

  it.each([
    ["pauseDeployment", Code.FailedPrecondition],
    ["resumeDeployment", Code.Unavailable],
  ])("preserves rejected and outcome-uncertain %s errors", async (method, code) => {
    const error = new ConnectError("controller response", code);
    const mutation = method as MutationMethod;
    applicationService[mutation].mockRejectedValue(error);

    const { createApplicationClient } = await import("./application-client");
    const client = createApplicationClient();

    await expect(
      client.application[mutation]({ applicationDeploymentId: "deployment" }),
    ).rejects.toBe(error);
  });
});

type MutationMethod = "pauseDeployment" | "resumeDeployment";

function deployment(
  applicationDeploymentId: string,
  desiredState: ApplicationDeploymentDesiredState,
  lifecycleState: ApplicationLifecycleState,
) {
  return create(ApplicationDeploymentSchema, {
    applicationDeploymentId,
    desiredState,
    state: create(ApplicationDeploymentStateSchema, { lifecycleState }),
  });
}
