import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";
import type { ReactNode } from "react";
import { createMemoryRouter, Outlet, RouterProvider } from "react-router-dom";
import { vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import type { RouteClient } from "@/api/route-client";
import type { SiteClient } from "@/api/site-client";
import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import { ChatOpenerContext } from "@/features/chat/chat-opener";
import { idleNodeMemory } from "@/features/memory/machine-test-support";
import {
  nodeMemory as nodeMemoryOf,
  nodeMemoryPayload,
} from "@/features/memory/node-memory-test-support";
import { idleMetricsClient } from "@/features/metrics/metric-test-support";
import { ModelCatalogPage } from "@/features/models/catalog/model-catalog-page";
import { ModelsPollingProvider } from "@/features/models/polling/models-polling-provider";
import { type NodeMemory, NodeMemoryState } from "@/gen/dotcore/capacity/v1/memory_pb";
import {
  type EngineRelease,
  EngineReleaseSchema,
} from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { type HardwareClass } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import { type Model, ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  ListEngineReleasesResponseSchema,
  ListHardwareClassesResponseSchema,
  ListModelsResponseSchema,
  ListUpstreamModelTemplatesResponseSchema,
} from "@/gen/dotcore/catalog/v1/service_pb";
import {
  type UpstreamModelTemplate,
  UpstreamModelTemplateSchema,
  UpstreamWireProtocol,
} from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";
import { ListRoutesResponseSchema } from "@/gen/dotcore/inference/v1/routes_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
  ModelDeploymentSchema,
  ModelDeploymentSnapshotSchema,
  ResolvedLocalModelDeploymentSnapshotSchema,
  UpstreamModelDeploymentSnapshotSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  type ListModelDeploymentsResponse,
  ListModelDeploymentsResponseSchema,
} from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStateSchema, ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import {
  AcceleratorMemoryArchitecture,
  type HardwareSnapshot,
  HardwareSnapshotSchema,
} from "@/gen/dotcore/observation/v1/observation_pb";
import {
  type ConfiguredExecutionTarget,
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  ListConfiguredExecutionTargetsResponseSchema,
  ListMachinesResponseSchema,
  type Machine,
  MachineHardwareSnapshotSchema,
  MachineSchema,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch, hardwareSnapshotsPayload } from "@/test/fake-resource-watch";
import { renderWithClients } from "@/test/render-with-clients";

import { ModelsPage } from "./models-page";

/**
 * Renders the Models page, the Catalog page and, when given, the overview under one shared models
 * polling instance, as the console layout holds it, with fake clients answering from the given
 * catalog and deployments.
 */
export function renderModelsSection({
  chatOpener,
  compatibility,
  deployments = [],
  engineReleases = [],
  estimate,
  hardwareClasses = [],
  executionTargets = [modelTarget("node", "pool", "Simulated models")],
  initialEntry = "/models",
  machines,
  listRoutes = () => Promise.resolve(create(ListRoutesResponseSchema)),
  logs,
  metricsClient = idleMetricsClient(),
  models = [],
  modelsResponse,
  nodeMemory,
  overview,
  snapshots = {},
  templates = [],
}: {
  /** Provides the chat opener, which inspectors need to offer Chat. */
  chatOpener?: (routeName: string) => void;
  /** Answers the profile compatibility read; absent leaves the model service without it. */
  compatibility?: ModelClient["model"]["listModelProfileCompatibility"];
  deployments?: ModelDeployment[];
  engineReleases?: EngineRelease[];
  /** Answers resource estimates; absent leaves the model service without estimates. */
  estimate?: ModelClient["model"]["estimateLocalModelResources"];
  /** The site's configured execution targets; defaults to one simulated model target. */
  executionTargets?: ConfiguredExecutionTarget[];
  hardwareClasses?: HardwareClass[];
  initialEntry?: string;
  /** The site's machines; absent leaves the machine list unanswered. */
  machines?: Machine[];
  /** Answers the route reads; defaults to no routes. */
  listRoutes?: RouteClient["route"]["listRoutes"];
  /** Answers deployment log reads; absent leaves the model client without logs. */
  logs?: ModelClient["logs"];
  /** The metrics service; defaults to one that never answers. */
  metricsClient?: MetricsClient;
  models?: Model[];
  modelsResponse?: Promise<Awaited<ReturnType<ModelClient["catalog"]["listModels"]>>>;
  /** The machines' node memory; defaults to each idle machine's snapshot, as `modelsResourceWatch`. */
  nodeMemory?: NodeMemory[];
  /** The overview page to mount at `/overview`, reading the section's clients. */
  overview?: ReactNode;
  snapshots?: Record<string, HardwareSnapshot>;
  templates?: UpstreamModelTemplate[];
}) {
  const client: ModelClient = {
    catalog: {
      listEngineReleases: vi.fn(() =>
        Promise.resolve(create(ListEngineReleasesResponseSchema, { engineReleases })),
      ),
      listHardwareClasses: vi.fn(() =>
        Promise.resolve(create(ListHardwareClassesResponseSchema, { hardwareClasses })),
      ),
      listModels: vi.fn(
        () => modelsResponse ?? Promise.resolve(create(ListModelsResponseSchema, { models })),
      ),
      listUpstreamModelTemplates: vi.fn(() =>
        Promise.resolve(
          create(ListUpstreamModelTemplatesResponseSchema, { upstreamModelTemplates: templates }),
        ),
      ),
    },
    model: {
      listModelRecommendations: vi.fn(),
      listModelDeployments: vi.fn(() =>
        Promise.resolve(create(ListModelDeploymentsResponseSchema, { deployments })),
      ),
      createLocalModelDeployment: vi.fn(),
      createModelDeployment: vi.fn(),
      estimateLocalModelResources: estimate,
      listModelProfileCompatibility: compatibility,
      updateModelDeployment: vi.fn(),
      updateModelDeploymentMetadata: vi.fn(),
      removeModelDeployment: vi.fn(),
      pauseModel: vi.fn(),
      resumeModel: vi.fn(),
    },
    logs,
  };
  const routeClient: RouteClient = {
    route: {
      listRoutes: vi.fn(listRoutes),
      getRoute: vi.fn(),
      createRoute: vi.fn(),
      replaceRoute: vi.fn(),
      deleteRoute: vi.fn(),
    },
  };
  const siteClient: SiteClient = {
    listConfiguredExecutionTargets: vi.fn(() =>
      Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema, { executionTargets })),
    ),
  };
  const watch = modelsResourceWatch({
    model: client,
    route: routeClient,
    site: siteClient,
    machines,
    nodeMemory,
    snapshots,
  });
  const router = createMemoryRouter(
    [
      {
        element: (
          <ResourceWatchProvider client={watch.client()}>
            <ModelsPollingProvider client={client}>
              <PanelNavigationProvider>
                <Outlet />
              </PanelNavigationProvider>
            </ModelsPollingProvider>
          </ResourceWatchProvider>
        ),
        children: [
          ...(overview ? [{ path: "/overview", element: overview }] : []),
          {
            path: "/models",
            children: [
              {
                index: true,
                element: <ModelsPage />,
              },
              {
                path: "catalog",
                element: <ModelCatalogPage />,
              },
            ],
          },
        ],
      },
    ],
    { initialEntries: [initialEntry] },
  );
  renderWithClients(
    chatOpener ? (
      <ChatOpenerContext.Provider value={chatOpener}>
        <RouterProvider router={router} />
      </ChatOpenerContext.Provider>
    ) : (
      <RouterProvider router={router} />
    ),
    {
      metricsClient,
      modelClient: client,
      routeClient,
      siteClient,
    },
  );
  return { client, router, siteClient, watch };
}

/**
 * A resource watch answering model deployments, and the execution targets and routes of the site
 * and route clients given, from their list reads, read again every manager tick like the
 * management API does. The machines, their hardware snapshots and their node memory are sent as
 * the given payloads.
 */
export function modelsResourceWatch({
  machines,
  model,
  nodeMemory,
  route,
  site,
  snapshots = {},
}: {
  /** The site's machines; absent leaves the machine list unanswered, with no snapshots either. */
  machines?: Machine[];
  model: ModelClient;
  /**
   * The machines' node memory; defaults to each machine's snapshot read as an idle machine, and a
   * machine without one as unavailable.
   */
  nodeMemory?: NodeMemory[];
  route?: RouteClient;
  site?: SiteClient;
  /** The hardware snapshot of each machine id. */
  snapshots?: Record<string, HardwareSnapshot>;
}): FakeResourceWatch {
  return new FakeResourceWatch({
    sources: {
      modelDeployments: () => model.model.listModelDeployments({}),
      ...(route && { routes: () => route.route.listRoutes({}) }),
      ...(site && { executionTargets: () => site.listConfiguredExecutionTargets({}) }),
      ...(machines && {
        hardwareSnapshots: () =>
          Promise.resolve(
            hardwareSnapshotsPayload(
              Object.fromEntries(
                machines.map(({ machineId }) => [
                  machineId,
                  create(MachineHardwareSnapshotSchema, {
                    snapshot: snapshots[machineId],
                  }),
                ]),
              ),
            ),
          ),
        machines: () => Promise.resolve(create(ListMachinesResponseSchema, { machines })),
        nodeMemory: () =>
          Promise.resolve(
            nodeMemoryPayload(
              nodeMemory ??
                machines.map(({ machineId }) => {
                  const snapshot = snapshots[machineId];
                  return snapshot
                    ? idleNodeMemory(machineId, snapshot)
                    : nodeMemoryOf(machineId, [], NodeMemoryState.UNAVAILABLE);
                }),
            ),
          ),
      }),
    },
    tickMs: 1_000,
  });
}

/** The router's current pathname and search, as one string. */
export function currentLocation(router: ReturnType<typeof createMemoryRouter>): string {
  return router.state.location.pathname + router.state.location.search;
}

export function catalogModel(modelId: string, displayName: string, profileIds: string[]) {
  return create(ModelSchema, {
    modelId,
    displayName,
    version: "1",
    profiles: profileIds.map((profileId) => ({ profileId })),
  });
}

export function localDeployment(
  modelDeploymentId: string,
  model: Model,
  state: MessageInitShape<typeof ModelDeploymentStateSchema>,
) {
  return create(ModelDeploymentSchema, {
    modelDeploymentId,
    desiredState: ModelDeploymentDesiredState.PRESENT,
    snapshot: create(ModelDeploymentSnapshotSchema, {
      source: {
        case: "local",
        value: create(ResolvedLocalModelDeploymentSnapshotSchema, {
          modelId: model.modelId,
          modelDisplayName: model.displayName,
          modelVersion: model.version,
        }),
      },
    }),
    state: create(ModelDeploymentStateSchema, state),
  });
}

export function deferredRead() {
  let resolve!: (value: ListModelDeploymentsResponse) => void;
  const promise = new Promise<ListModelDeploymentsResponse>((promiseResolve) => {
    resolve = promiseResolve;
  });
  return { promise, resolve };
}

export function deployableModel() {
  return create(ModelSchema, {
    modelId: "model-1",
    displayName: "Test Model",
    version: "1",
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
    ],
    configurations: [
      {
        configurationId: "cuda",
        engineArtifacts: [{ engineReleaseId: "llama", engineArtifactId: "cuda" }],
        artifactBindings: [{ bindingId: "q4-binding", modelArtifactId: "q4" }],
      },
    ],
    profiles: [
      {
        profileId: "default",
        displayName: "Fast start",
        configurationId: "cuda",
        defaultModelArtifactBindingId: "q4-binding",
        engineParameterDefaults: {
          values: [
            { parameterId: "total-context", value: { case: "unsignedInteger", value: 8192n } },
          ],
        },
      },
    ],
  });
}

export function llamaRelease() {
  return create(EngineReleaseSchema, {
    engineReleaseId: "llama",
    version: "1",
    parameterSchema: {
      schemaId: "schema",
      parameters: [
        {
          parameterId: "total-context",
          displayName: "Total context",
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 1024n, maximum: 32768n, unit: "tokens" },
          },
        },
      ],
    },
  });
}

export function upstreamTemplate(
  upstreamModelTemplateId = "openai-compatible",
  displayName = "OpenAI-compatible",
) {
  return create(UpstreamModelTemplateSchema, {
    upstreamModelTemplateId,
    displayName,
    wireProtocol: UpstreamWireProtocol.OPENAI_COMPATIBLE,
  });
}

export function upstreamDeployment(template: UpstreamModelTemplate, status: ModelDeploymentStatus) {
  return create(ModelDeploymentSchema, {
    modelDeploymentId: "upstream-new",
    modelDeploymentName: "Private model",
    desiredState: ModelDeploymentDesiredState.PRESENT,
    snapshot: create(ModelDeploymentSnapshotSchema, {
      source: {
        case: "upstream",
        value: create(UpstreamModelDeploymentSnapshotSchema, { upstreamModelTemplate: template }),
      },
    }),
    state: create(ModelDeploymentStateSchema, { status }),
  });
}

export function readableMachine(machineId: string, displayName: string) {
  return create(MachineSchema, {
    currentHardwareSnapshotAvailability: {
      availability: { case: "available", value: create(EmptySchema) },
    },
    displayName,
    machineId,
  });
}

/** A connected model target on the site manager, with an available runtime. */
export function modelTarget(
  siteNodeId: string,
  executionTargetId: string,
  displayName: string,
  siteNodeDisplayName = "",
) {
  return create(ConfiguredExecutionTargetSchema, {
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: { availability: { case: "available", value: {} } },
    },
    displayName,
    executionTargetId,
    siteManager: true,
    siteNodeDisplayName,
    siteNodeId,
    workloadKind: ExecutionTargetWorkloadKind.MODEL,
  });
}

/** A unified machine: 128 GiB of system memory, 68 GiB in use. */
export function unifiedSnapshot() {
  return create(HardwareSnapshotSchema, {
    accelerators: [
      {
        result: {
          case: "facts",
          value: {
            facts: [
              {
                deviceId: "GB10",
                memoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
                model: "NVIDIA GB10",
              },
            ],
          },
        },
      },
    ],
    host: {
      result: {
        case: "facts",
        value: { availableMemoryBytes: 60n * 1024n ** 3n, totalMemoryBytes: 128n * 1024n ** 3n },
      },
    },
    observedAt: { seconds: 1n },
  });
}
