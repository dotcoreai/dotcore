import { create } from "@bufbuild/protobuf";
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ApplicationClient } from "@/api/application-client";
import type { ModelClient } from "@/api/model-client";
import {
  ApplicationDeploymentDesiredState,
  ApplicationDeploymentSchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  ApplicationTargetOptionSchema,
  ListApplicationDeploymentsResponseSchema,
  type ListApplicationTargetsRequest,
  ListApplicationTargetsResponseSchema,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import { ApplicationSchema } from "@/gen/dotcore/catalog/v1/application_pb";
import { EngineParameterValuesSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";
import { ListApplicationsResponseSchema } from "@/gen/dotcore/catalog/v1/service_pb";
import {
  ModelDeploymentDesiredState,
  ModelDeploymentSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  ListModelDeploymentsResponseSchema,
  type ListModelRecommendationsRequest,
  ListModelRecommendationsResponseSchema,
  ModelRecommendationFit,
  RecommendedModelProfileSchema,
  RecommendedProfileTargetSchema,
  UseCaseModelRecommendationsSchema,
} from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import {
  ListMachinesResponseSchema,
  MachineSchema,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { timing } from "@/lib/timing";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { type UseCaseProposal } from "./get-started-proposals";
import { useGetStartedData } from "./use-get-started-data";

const { CHAT_ASSISTANT, CODING, IMAGE_GENERATION } = UseCase;

/** What the fake backend answers; tests change it between reads. */
interface Backend {
  rows: Partial<Record<UseCase, FakeRow[]>>;
  /** Targets per application id, as `[target id, matches preference, deployment id?]`. */
  targets: Record<string, [string, boolean, string?][]>;
}

type FakeRow = [model: string, profile: string, context: number, target: [string, string?]];

let backend: Backend;
let modelReads: ListModelRecommendationsRequest[];
let targetReads: ListApplicationTargetsRequest[];
let failReads = false;
let hook: { current: ReturnType<typeof useGetStartedData> };

beforeEach(() => {
  vi.useFakeTimers();
  modelReads = [];
  targetReads = [];
  backend = { rows: {}, targets: {} };
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useGetStartedData", () => {
  it("asks for every use case that can start now, and the targets of each use case's app", async () => {
    await mount();

    expect(modelReads).toHaveLength(1);
    expect(modelReads[0].useCases).toEqual([CHAT_ASSISTANT, CODING, IMAGE_GENERATION]);
    expect(modelReads[0].canStartNow).toBe(true);
    expect(targetReads.map((read) => read.applicationId)).toEqual(["open-webui", "comfyui"]);
  });

  it("proposes the first row and its app on an idle machine, the rest as other choices", async () => {
    backend.rows[CODING] = [
      ["qwen-27b", "nvfp4", 131072, ["gpu1"]],
      ["qwen-8b", "q4", 32768, ["gpu1"]],
      ["qwen-8b", "q8", 16384, ["gpu1"]],
    ];
    backend.rows[CHAT_ASSISTANT] = [["qwen-8b", "q4", 32768, ["gpu1"]]];
    backend.targets["open-webui"] = [["docker", true]];
    await mount();

    expect(describeProposal(CODING)).toEqual({
      app: undefined,
      model: "qwen-27b/nvfp4 @gpu1 context 131072",
      otherChoices: ["qwen-8b/q4 context 32768", "qwen-8b/q8 context 16384"],
      ready: false,
    });
    expect(describeProposal(CHAT_ASSISTANT)).toEqual({
      app: "open-webui @docker cpu=false",
      model: "qwen-8b/q4 @gpu1 context 32768",
      otherChoices: [],
      ready: false,
    });
    expect(hook.current.machines.map((machine) => machine.machineId)).toEqual(["m1"]);
  });

  it("shows a running model first, with the context its deployment runs", async () => {
    backend.rows[CHAT_ASSISTANT] = [["qwen-8b", "q4", 32768, ["gpu1", "dep-1"]]];
    backend.targets["open-webui"] = [["docker", true, "app-1"]];
    const watch = await mount({
      modelDeployments: [modelDeployment("dep-1", ModelDeploymentStatus.READY, 65536n)],
    });

    expect(proposal(CHAT_ASSISTANT).model).toMatchObject({
      contextTokensPerRequest: 65536n,
      deploymentId: "dep-1",
    });
    expect(proposal(CHAT_ASSISTANT).model?.deployment?.modelDeploymentId).toBe("dep-1");
    expect(proposal(CHAT_ASSISTANT).ready).toBe(false);

    watch.payload(
      "applicationDeployments",
      create(ListApplicationDeploymentsResponseSchema, {
        deployments: [applicationDeployment("app-1", true)],
      }),
    );
    await flush();
    expect(proposal(CHAT_ASSISTANT).ready).toBe(true);
  });

  it("falls back to the row's context while the watch has not listed the running deployment", async () => {
    backend.rows[CODING] = [["qwen-8b", "q4", 32768, ["gpu1", "dep-1"]]];
    await mount();

    expect(proposal(CODING).model).toMatchObject({
      contextTokensPerRequest: 32768n,
      deployment: undefined,
      deploymentId: "dep-1",
    });
  });

  it("proposes no model on a full machine but keeps the app", async () => {
    backend.targets["open-webui"] = [["docker", true]];
    await mount();

    expect(describeProposal(CHAT_ASSISTANT)).toEqual({
      app: "open-webui @docker cpu=false",
      model: undefined,
      otherChoices: [],
      ready: false,
    });
  });

  it("proposes only the app for image generation and flags a target without the preferred accelerator", async () => {
    backend.targets["comfyui"] = [["docker", false]];
    await mount();

    expect(describeProposal(IMAGE_GENERATION)).toEqual({
      app: "comfyui @docker cpu=true",
      model: undefined,
      otherChoices: [],
      ready: false,
    });
  });

  it("keeps the app without a target when no Docker target can run it", async () => {
    backend.rows[CHAT_ASSISTANT] = [["qwen-8b", "q4", 32768, ["gpu1"]]];
    await mount();

    expect(proposal(CHAT_ASSISTANT).app).toMatchObject({
      application: { applicationId: "open-webui" },
      deployment: undefined,
      runsOnCpu: false,
      target: undefined,
    });
  });

  it("marks image generation ready only when its app deployment is ready", async () => {
    backend.targets["comfyui"] = [["docker", true, "app-2"]];
    const watch = await mount({ applicationDeployments: [applicationDeployment("app-2", false)] });
    expect(proposal(IMAGE_GENERATION).ready).toBe(false);

    watch.payload(
      "applicationDeployments",
      create(ListApplicationDeploymentsResponseSchema, {
        deployments: [applicationDeployment("app-2", true)],
      }),
    );
    await flush();
    expect(proposal(IMAGE_GENERATION).ready).toBe(true);
  });

  it("reads again after a deployment changes, and not for unrelated updates", async () => {
    backend.rows[CODING] = [["qwen-8b", "q4", 32768, ["gpu1"]]];
    const watch = await mount();
    expect(modelReads).toHaveLength(1);

    // Same machines and deployments, delivered again.
    watch.payload("machines", machines());
    await flush();
    await advance(timing().recommendationReadMs);
    expect(modelReads).toHaveLength(1);

    backend.rows[CODING] = [["qwen-8b", "q4", 32768, ["gpu1", "dep-1"]]];
    watch.payload(
      "modelDeployments",
      create(ListModelDeploymentsResponseSchema, {
        deployments: [modelDeployment("dep-1", ModelDeploymentStatus.DOWNLOADING, 32768n)],
      }),
    );
    await flush();
    await advance(timing().recommendationReadMs);

    expect(modelReads).toHaveLength(2);
    expect(proposal(CODING).model?.deploymentId).toBe("dep-1");
  });

  it("reads again after a machine stops reporting", async () => {
    backend.rows[CODING] = [["qwen-8b", "q4", 32768, ["gpu1"]]];
    const watch = await mount();

    backend.rows[CODING] = [];
    watch.payload("machines", machines(NodeObservationState.STALE));
    await flush();
    await advance(timing().recommendationReadMs);

    expect(modelReads).toHaveLength(2);
    expect(proposal(CODING).model).toBeUndefined();
  });

  it("keeps the last proposals and reports an error when a read fails, then recovers", async () => {
    backend.rows[CODING] = [["qwen-8b", "q4", 32768, ["gpu1"]]];
    const watch = await mount();
    backend.rows = { [CODING]: undefined };
    failReads = true;

    watch.payload("machines", machines(NodeObservationState.STALE));
    await flush();
    await advance(timing().recommendationReadMs);
    expect(hook.current.error).toMatch(/unavailable/);
    expect(proposal(CODING).model?.modelId).toBe("qwen-8b");

    failReads = false;
    await advance(timing().pollRetryMs[0]);
    expect(hook.current.error).toBeUndefined();
    expect(proposal(CODING).model).toBeUndefined();
  });

  it("reports an error and stays loading while the app catalog read fails", async () => {
    await mount({ failCatalog: true });

    expect(hook.current.error).toBe(
      "Site data is unavailable. Check that the backend is running; the last result stays shown.",
    );
    expect(hook.current.loading).toBe(true);
    expect(hook.current.useCases).toEqual([]);
  });
});

async function mount({
  applicationDeployments = [],
  failCatalog = false,
  modelDeployments = [],
}: {
  applicationDeployments?: ReturnType<typeof applicationDeployment>[];
  /** Whether the app catalog read fails. */
  failCatalog?: boolean;
  modelDeployments?: ReturnType<typeof modelDeployment>[];
} = {}) {
  failReads = false;
  const watch = new FakeResourceWatch();
  const modelClient = {
    model: {
      listModelRecommendations: vi.fn((request: ListModelRecommendationsRequest) => {
        modelReads.push(request);
        if (failReads) return Promise.reject(new Error("unavailable"));
        return Promise.resolve(
          create(ListModelRecommendationsResponseSchema, {
            useCases: request.useCases.map((useCase) =>
              create(UseCaseModelRecommendationsSchema, {
                rows: (backend.rows[useCase] ?? []).map(recommendedRow),
                useCase,
              }),
            ),
          }),
        );
      }),
    },
  } as unknown as ModelClient;
  const applicationClient = {
    application: {
      listApplicationTargets: vi.fn((request: ListApplicationTargetsRequest) => {
        targetReads.push(request);
        return Promise.resolve(
          create(ListApplicationTargetsResponseSchema, {
            targets: (backend.targets[request.applicationId] ?? []).map(
              ([executionTargetId, matchesPreference, applicationDeploymentId]) =>
                create(ApplicationTargetOptionSchema, {
                  applicationDeploymentId,
                  matchesPreference,
                  target: { executionTargetId, siteNodeId: "m1" },
                }),
            ),
          }),
        );
      }),
    },
    catalog: {
      listApplications: vi.fn(() =>
        failCatalog
          ? Promise.reject(new Error("unavailable"))
          : Promise.resolve(
              create(ListApplicationsResponseSchema, {
                applications: [
                  create(ApplicationSchema, {
                    applicationId: "open-webui",
                    useCases: [CHAT_ASSISTANT],
                  }),
                  create(ApplicationSchema, {
                    applicationId: "comfyui",
                    preferredAccelerators: ["nvidia"],
                    useCases: [IMAGE_GENERATION],
                  }),
                ],
              }),
            ),
      ),
    },
  } as unknown as ApplicationClient;

  const rendered = renderHook(() => useGetStartedData({ applicationClient, modelClient }), {
    wrapper: ({ children }) => (
      <ResourceWatchProvider client={watch.client()}>{children}</ResourceWatchProvider>
    ),
  });
  hook = rendered.result;
  await flush();
  watch.payload("machines", machines());
  watch.payload(
    "modelDeployments",
    create(ListModelDeploymentsResponseSchema, { deployments: modelDeployments }),
  );
  watch.payload(
    "applicationDeployments",
    create(ListApplicationDeploymentsResponseSchema, { deployments: applicationDeployments }),
  );
  await flush();
  await flush();
  return watch;
}

function proposal(useCase: UseCase): UseCaseProposal {
  const found = hook.current.useCases.find((candidate) => candidate.useCase === useCase);
  if (!found) throw new Error(`no proposal for use case ${useCase}`);
  return found;
}

/** A proposal as `model/profile @target context N` strings, for exact comparisons. */
function describeProposal(useCase: UseCase) {
  const { app, model, otherChoices, ready } = proposal(useCase);
  const row = (m: NonNullable<typeof model>) =>
    `${m.modelId}/${m.profileId} context ${m.contextTokensPerRequest}`;
  const placed = (m: NonNullable<typeof model>) =>
    `${m.modelId}/${m.profileId} @${m.target.executionTargetId} context ${m.contextTokensPerRequest}`;

  return {
    app:
      app?.target &&
      `${app.application.applicationId} @${app.target.target?.executionTargetId} cpu=${app.runsOnCpu}`,
    model: model && placed(model),
    otherChoices: otherChoices.map(row),
    ready,
  };
}

function recommendedRow([modelId, profileId, context, [targetId, deploymentId]]: FakeRow) {
  return create(RecommendedModelProfileSchema, {
    contextTokensPerRequest: BigInt(context),
    modelId,
    profileId,
    targets: [
      create(RecommendedProfileTargetSchema, {
        fit: ModelRecommendationFit.FITS_NOW,
        modelDeploymentId: deploymentId,
        target: { executionTargetId: targetId, siteNodeId: "m1" },
      }),
    ],
  });
}

function machines(state = NodeObservationState.CONNECTED) {
  return create(ListMachinesResponseSchema, {
    machines: [create(MachineSchema, { machineId: "m1", observationFreshness: { state } })],
  });
}

function modelDeployment(id: string, status: ModelDeploymentStatus, contextPerRequest: bigint) {
  return create(ModelDeploymentSchema, {
    desiredState: ModelDeploymentDesiredState.PRESENT,
    modelDeploymentId: id,
    snapshot: {
      source: {
        case: "local",
        value: {
          engineParameterValues: create(EngineParameterValuesSchema, {
            values: [
              {
                parameterId: "context-per-request",
                value: { case: "unsignedInteger", value: contextPerRequest },
              },
            ],
          }),
        },
      },
    },
    state: { status },
  });
}

function applicationDeployment(id: string, ready: boolean) {
  return create(ApplicationDeploymentSchema, {
    applicationDeploymentId: id,
    desiredState: ApplicationDeploymentDesiredState.PRESENT,
    state: {
      lifecycleState: ready ? ApplicationLifecycleState.READY : ApplicationLifecycleState.STARTING,
      readinessState: ready ? ApplicationReadinessState.READY : ApplicationReadinessState.NOT_READY,
    },
  });
}

async function flush() {
  await act(() => vi.advanceTimersByTimeAsync(0));
}

async function advance(ms: number) {
  await act(() => vi.advanceTimersByTimeAsync(ms));
}
