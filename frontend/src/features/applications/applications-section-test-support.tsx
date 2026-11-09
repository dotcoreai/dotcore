import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { render } from "@testing-library/react";
import { createMemoryRouter, Outlet, RouterProvider } from "react-router-dom";
import { vi } from "vitest";

import type { ApplicationClient } from "@/api/application-client";
import type { MetricsClient } from "@/api/metrics-client";
import type { RouteClient } from "@/api/route-client";
import { ResourceWatchProvider } from "@/app/resource-watch-provider";
import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import { idleMetricsClient } from "@/features/metrics/metric-test-support";
import {
  ApplicationDeploymentDesiredState,
  ApplicationDeploymentSchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  CreateApplicationDeploymentResponseSchema,
  GetApplicationDeploymentResponseSchema,
  ListApplicationDeploymentsResponseSchema,
  RemoveApplicationDeploymentResponseSchema,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationDeploymentStateSchema,
  type ApplicationLifecycleState,
  type ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import {
  ApplicationConfigurationBindingSchema,
  ApplicationDeploymentInputSchema,
  ApplicationGraphSchema,
  ApplicationSchema,
  ApplicationServiceSchema,
} from "@/gen/dotcore/catalog/v1/application_pb";
import { ListApplicationsResponseSchema } from "@/gen/dotcore/catalog/v1/service_pb";
import {
  type InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  ListConfiguredExecutionTargetsResponseSchema,
  ListMachinesResponseSchema,
  type MachineSchema,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { AppCatalogPage } from "./app-catalog-page";
import { ApplicationsPage } from "./applications-page";
import { ApplicationsPollingProvider } from "./applications-polling-provider";

/**
 * Renders the Apps section, the Apps page and the Catalog page under one shared applications
 * polling instance, with the given fake clients.
 */
export function renderAppsSection({
  client,
  initialEntry = "/apps",
  machines = [],
  metricsClient = idleMetricsClient(),
  routeClient = routeClientFor([]),
}: {
  client: ApplicationClient;
  initialEntry?: string;
  /** The machines the watch sends; defaults to none. */
  machines?: MessageInitShape<typeof MachineSchema>[];
  /** The metrics the Apps page watches; defaults to a client that never answers. */
  metricsClient?: MetricsClient;
  routeClient?: RouteClient;
}) {
  const watch = appsResourceWatch(client, routeClient, machines);
  const router = createMemoryRouter(
    [
      {
        element: (
          <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
            <PanelNavigationProvider>
              <Outlet />
            </PanelNavigationProvider>
          </ResourceWatchProvider>
        ),
        children: [
          {
            path: "/apps",
            element: (
              <ApplicationsPollingProvider client={client}>
                <Outlet />
              </ApplicationsPollingProvider>
            ),
            children: [
              {
                index: true,
                element: <ApplicationsPage client={client} metricsClient={metricsClient} />,
              },
              {
                path: "catalog",
                element: <AppCatalogPage client={client} />,
              },
            ],
          },
        ],
      },
    ],
    { initialEntries: [initialEntry] },
  );
  const result = render(<RouterProvider router={router} />);
  return { ...result, router, watch };
}

/**
 * A resource watch answering application deployments and execution targets from the application
 * client's list reads, and routes from the route client's when given, read again every manager
 * tick like the management API does. The machines are sent as given.
 */
export function appsResourceWatch(
  client: ApplicationClient,
  routeClient?: RouteClient,
  machines: MessageInitShape<typeof MachineSchema>[] = [],
): FakeResourceWatch {
  return new FakeResourceWatch({
    sources: {
      applicationDeployments: () => client.application.listApplicationDeployments({}),
      executionTargets: () => client.site.listConfiguredExecutionTargets({}),
      machines: () => Promise.resolve(create(ListMachinesResponseSchema, { machines })),
      ...(routeClient && { routes: () => routeClient.route.listRoutes({}) }),
    },
    tickMs: 1_000,
  });
}

/** The router's current pathname and search, as one string. */
export function currentLocation(router: ReturnType<typeof createMemoryRouter>): string {
  return router.state.location.pathname + router.state.location.search;
}

export function app(applicationId: string, displayName = applicationId) {
  return create(ApplicationSchema, { applicationId, displayName, version: "1.0" });
}

export function resolvedSnapshot(application: Application) {
  return {
    applicationId: application.applicationId,
    displayName: application.displayName,
    version: application.version,
    graph: application.graph,
  };
}

export function routeInputApp(applicationId = "open-webui", displayName = "OpenWebUI") {
  return create(ApplicationSchema, {
    ...app(applicationId, displayName),
    graph: create(ApplicationGraphSchema, {
      services: [
        create(ApplicationServiceSchema, {
          serviceName: "web",
          configurationBindings: [
            create(ApplicationConfigurationBindingSchema, {
              bindingName: "route",
              input: create(ApplicationDeploymentInputSchema, {
                kind: { case: "inferenceRoute", value: {} },
              }),
            }),
          ],
        }),
      ],
    }),
  });
}

export function deployment(
  application: ReturnType<typeof app>,
  lifecycleState: ApplicationLifecycleState,
  readinessState: ApplicationReadinessState,
  applicationDeploymentId = `${application.applicationId}-deployment`,
) {
  return create(ApplicationDeploymentSchema, {
    applicationDeploymentId,
    applicationDeploymentName: `${application.applicationId}-1`,
    desiredState: ApplicationDeploymentDesiredState.PRESENT,
    snapshot: resolvedSnapshot(application),
    state: create(ApplicationDeploymentStateSchema, { lifecycleState, readinessState }),
    placement: { siteNodeId: "local", executionTargetId: "local" },
  });
}

/** A connected application target with an available runtime, on the site manager by default. */
export function applicationTarget(
  siteNodeId: string,
  executionTargetId: string,
  { siteManager = true, siteNodeDisplayName = "" } = {},
) {
  return create(ConfiguredExecutionTargetSchema, {
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: { availability: { case: "available", value: {} } },
    },
    displayName: "Docker applications",
    executionTargetId,
    siteManager,
    siteNodeDisplayName,
    siteNodeId,
    workloadKind: ExecutionTargetWorkloadKind.APPLICATION,
  });
}

export function clientFor(
  applications: ReturnType<typeof app>[],
  deployments: ReturnType<typeof deployment>[],
): ApplicationClient {
  return {
    catalog: {
      listApplications: vi.fn(() =>
        Promise.resolve(create(ListApplicationsResponseSchema, { applications })),
      ),
    },
    site: {
      listConfiguredExecutionTargets: vi.fn(() =>
        Promise.resolve(
          create(ListConfiguredExecutionTargetsResponseSchema, {
            executionTargets: [applicationTarget("local", "local")],
          }),
        ),
      ),
    },
    application: {
      listApplicationDeployments: vi.fn(() =>
        Promise.resolve(create(ListApplicationDeploymentsResponseSchema, { deployments })),
      ),
      getApplicationDeployment: vi.fn(() =>
        Promise.resolve(create(GetApplicationDeploymentResponseSchema)),
      ),
      createApplicationDeployment: vi.fn(() =>
        Promise.resolve(create(CreateApplicationDeploymentResponseSchema)),
      ),
      removeApplicationDeployment: vi.fn(() =>
        Promise.resolve(create(RemoveApplicationDeploymentResponseSchema)),
      ),
      updateApplicationDeploymentMetadata: vi.fn(() =>
        Promise.reject(new Error("name not configured")),
      ),
      pauseDeployment: vi.fn(() => Promise.reject(new Error("pause not configured"))),
      resumeDeployment: vi.fn(() => Promise.reject(new Error("resume not configured"))),
    },
  };
}

export function routeClientFor(
  routes: ReturnType<typeof create<typeof InferenceRouteViewSchema>>[],
): RouteClient {
  return {
    route: {
      listRoutes: vi.fn(() => Promise.resolve(create(ListRoutesResponseSchema, { routes }))),
      getRoute: vi.fn(),
      createRoute: vi.fn(),
      replaceRoute: vi.fn(),
      deleteRoute: vi.fn(),
    },
  };
}
