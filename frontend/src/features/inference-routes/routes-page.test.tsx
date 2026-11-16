import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import {
  act,
  fireEvent,
  render as rtlRender,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { type ReactNode, useState } from "react";
import { createMemoryRouter, MemoryRouter, Outlet, RouterProvider } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { ClientsProvider } from "@/api/clients-provider";
import type { MetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import type { RouteClient } from "@/api/route-client";
import type { SiteClient } from "@/api/site-client";
import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import {
  counterPoint,
  distributionPoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { ModelsPage } from "@/features/models/deployed/models-page";
import { modelsResourceWatch } from "@/features/models/deployed/models-section-test-support";
import { ModelsPollingProvider } from "@/features/models/polling/models-polling-provider";
import { ModelInputModality } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  ListEngineReleasesResponseSchema,
  ListHardwareClassesResponseSchema,
  ListModelsResponseSchema,
  ListUpstreamModelTemplatesResponseSchema,
} from "@/gen/dotcore/catalog/v1/service_pb";
import {
  UpstreamModelTemplateSchema,
  UpstreamWireProtocol,
} from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";
import {
  CreateRouteResponseSchema,
  DeleteRouteResponseSchema,
  FallbackGroupAvailabilitySchema,
  FallbackGroupSchema,
  FallbackGroupSelection,
  InferenceRouteAvailabilitySchema,
  InferenceRouteMemberAvailabilitySchema,
  InferenceRouteMemberExclusionReason,
  InferenceRouteSchema,
  InferenceRouteUnavailabilityReason,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
  ReplaceRouteResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import {
  ListMetricDescriptorsResponseSchema,
  MetricResolutionSchema,
} from "@/gen/dotcore/metrics/v1/service_pb";
import {
  ModelDeploymentDesiredState,
  ModelDeploymentSchema,
  ModelDeploymentSnapshotSchema,
  OpenAiCompatibleEndpointConfigurationSchema,
  UpstreamModelDeploymentSnapshotSchema,
  UpstreamModelDesiredConfigurationSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import { ListConfiguredExecutionTargetsResponseSchema } from "@/gen/dotcore/site/v1/service_pb";
import { productionTiming, setTiming } from "@/lib/timing";
import { PanelLink } from "@/panels/panel-navigation";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";
import { rejectConsoleSettingsUpdate } from "@/test/fake-site-client";
import { expectHeadingOutline } from "@/test/heading-outline";
import { chooseMoreAction, moreActionNames, waitForInspectorFocus } from "@/test/inspector-actions";
import { clientsWrapper, testClients } from "@/test/render-with-clients";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { RoutesPage as RealRoutesPage } from "./routes-page";

/** A metrics store with one resolution and no samples, so route charts render empty. */
const idleMetricsClient: MetricsClient = {
  metrics: {
    listMetricDescriptors: () =>
      Promise.resolve(
        create(ListMetricDescriptorsResponseSchema, {
          resolutions: [
            create(MetricResolutionSchema, { retention: { seconds: 900n }, step: { seconds: 1n } }),
          ],
        }),
      ),
    watchMetrics: staticWatch(() => queryResult({ series: [], stepCount: 1 })),
  },
};

const watches: FakeResourceWatch[] = [];

/** The names of the table's route rows in display order, without the header row. */
function routeRowNames() {
  return screen
    .queryAllByRole("row", { hidden: true })
    .filter((row) => row.hasAttribute("aria-controls"))
    .map((row) => row.getAttribute("aria-label"));
}

/** Reads the latest page's sources again and sends them, like the manager's next tick. */
async function tick() {
  await act(async () => {
    await watches.at(-1)?.tick();
  });
}

/**
 * The Routes page under a resource watch answering routes and model deployments from the clients'
 * list reads, read again every manager tick like the management API does.
 */
function RoutesPage({
  client,
  metricsClient,
  modelClient,
}: {
  client: RouteClient;
  metricsClient: MetricsClient;
  modelClient: ModelClient;
}) {
  const [watch] = useState(() => {
    const watch = new FakeResourceWatch({
      sources: {
        modelDeployments: () => modelClient.model.listModelDeployments({}),
        routes: () => client.route.listRoutes({}),
      },
    });
    watches.push(watch);

    return watch;
  });

  const [clients] = useState(() =>
    testClients({ metricsClient, modelClient, routeClient: client }),
  );

  return (
    <ClientsProvider clients={clients}>
      <ResourceWatchProvider client={watch.client()}>
        <RealRoutesPage />
      </ResourceWatchProvider>
    </ClientsProvider>
  );
}

function render(ui: ReactNode, initialEntries = ["/routes"]) {
  return rtlRender(ui, {
    wrapper: ({ children }) => (
      <MemoryRouter initialEntries={initialEntries}>{children}</MemoryRouter>
    ),
  });
}

function renderAt(
  initialEntries: string[],
  client: RouteClient,
  modelClient: ModelClient,
  initialIndex = initialEntries.length - 1,
) {
  const router = createMemoryRouter(
    [
      {
        path: "*",
        element: (
          <PanelNavigationProvider>
            <RoutesPage
              client={client}
              metricsClient={idleMetricsClient}
              modelClient={modelClient}
            />
          </PanelNavigationProvider>
        ),
      },
    ],
    { initialEntries, initialIndex },
  );
  rtlRender(<RouterProvider router={router} />);
  return router;
}

function renderIntegratedAt(
  initialEntries: string[],
  client: RouteClient,
  modelClient: ModelClient,
  initialIndex = initialEntries.length - 1,
) {
  const router = createMemoryRouter(
    [
      {
        path: "*",
        element: (
          <PanelNavigationProvider>
            <PanelLink to="/machines">Machines navigation</PanelLink>
            <RoutesPage
              client={client}
              metricsClient={idleMetricsClient}
              modelClient={modelClient}
            />
          </PanelNavigationProvider>
        ),
      },
    ],
    { initialEntries, initialIndex },
  );
  rtlRender(<RouterProvider router={router} />);
  return router;
}

function renderConsoleAt(
  initialEntries: string[],
  client: RouteClient,
  modelClient: ModelClient,
  initialIndex = initialEntries.length - 1,
) {
  const watch = modelsResourceWatch({
    machines: [],
    model: modelClient,
    route: client,
    site: emptySiteClient,
  });
  watches.push(watch);
  const router = createMemoryRouter(
    [
      {
        element: (
          <ResourceWatchProvider client={watch.client()}>
            <PanelNavigationProvider>
              <Outlet />
            </PanelNavigationProvider>
          </ResourceWatchProvider>
        ),
        children: [
          {
            path: "/models",
            element: (
              <ModelsPollingProvider client={modelClient}>
                <ModelsPage />
              </ModelsPollingProvider>
            ),
          },
          {
            path: "/routes",
            element: <RealRoutesPage />,
          },
        ],
      },
    ],
    { initialEntries, initialIndex },
  );
  rtlRender(<RouterProvider router={router} />, {
    wrapper: clientsWrapper({
      metricsClient: idleMetricsClient,
      modelClient,
      routeClient: client,
      siteClient: emptySiteClient,
    }),
  });
  return router;
}

/** A site with no machines or execution targets. */
const emptySiteClient: SiteClient = {
  updateConsoleSettings: rejectConsoleSettingsUpdate,
  listConfiguredExecutionTargets: () =>
    Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
};

function routeView({
  id,
  name,
  enabled = true,
  available = true,
  generation = 1n,
  groups = [{ selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] }],
}: {
  id: string;
  name: string;
  enabled?: boolean;
  available?: boolean;
  generation?: bigint;
  groups?: { selection: FallbackGroupSelection; modelDeploymentIds: string[] }[];
}) {
  return create(InferenceRouteViewSchema, {
    route: create(InferenceRouteSchema, {
      inferenceRouteId: id,
      name,
      enabled,
      generation,
      fallbackGroups: groups.map((group) => create(FallbackGroupSchema, group)),
    }),
    // Every member reports the route's availability, so an available route reads Available.
    availability: create(InferenceRouteAvailabilitySchema, {
      available,
      fallbackGroups: groups.map((group, index) =>
        create(FallbackGroupAvailabilitySchema, {
          fallbackGroupIndex: index,
          members: group.modelDeploymentIds.map((modelDeploymentId) =>
            create(InferenceRouteMemberAvailabilitySchema, {
              availableInputModalities: available ? [ModelInputModality.TEXT] : [],
              eligible: available,
              modelDeploymentId,
            }),
          ),
        }),
      ),
    }),
  });
}

function deployment(id: string, name: string, desiredState = ModelDeploymentDesiredState.PRESENT) {
  return create(ModelDeploymentSchema, {
    modelDeploymentId: id,
    desiredState,
    snapshot: create(ModelDeploymentSnapshotSchema, {
      source: {
        case: "local",
        value: { modelId: name, modelDisplayName: name, modelVersion: "" } as never,
      },
    }),
    state: { status: ModelDeploymentStatus.READY },
  });
}

function externalDeployment(id: string, provider: string) {
  return create(ModelDeploymentSchema, {
    modelDeploymentId: id,
    desiredState: ModelDeploymentDesiredState.PRESENT,
    snapshot: create(ModelDeploymentSnapshotSchema, {
      source: {
        case: "upstream",
        value: create(UpstreamModelDeploymentSnapshotSchema, {
          upstreamModelTemplate: create(UpstreamModelTemplateSchema, {
            upstreamModelTemplateId: `${id}-template`,
            displayName: provider,
            wireProtocol: UpstreamWireProtocol.OPENAI_COMPATIBLE,
          }),
        }),
      },
    }),
    upstreamDesiredConfiguration: create(UpstreamModelDesiredConfigurationSchema, {
      upstreamModelId: "private-model-id",
      apiKey: "private-api-key",
      endpoint: create(OpenAiCompatibleEndpointConfigurationSchema, {
        baseUrl: "https://private.example.test/v1",
      }),
    }),
  });
}

function clients(
  routes: ReturnType<typeof routeView>[],
  deployments = [deployment("deployment-a", "Local model")],
) {
  const route: RouteClient = {
    route: {
      listRoutes: vi.fn(() => Promise.resolve(create(ListRoutesResponseSchema, { routes }))),
      getRoute: vi.fn(),
      createRoute: vi.fn(() => Promise.resolve(create(CreateRouteResponseSchema))),
      replaceRoute: vi.fn(() => Promise.resolve(create(ReplaceRouteResponseSchema))),
      deleteRoute: vi.fn(() => Promise.resolve(create(DeleteRouteResponseSchema))),
    },
  };
  const model: ModelClient = {
    catalog: {
      listEngineReleases: vi.fn(() => Promise.resolve(create(ListEngineReleasesResponseSchema))),
      listHardwareClasses: vi.fn(() => Promise.resolve(create(ListHardwareClassesResponseSchema))),
      listModels: vi.fn(() => Promise.resolve(create(ListModelsResponseSchema))),
      listUpstreamModelTemplates: vi.fn(() =>
        Promise.resolve(create(ListUpstreamModelTemplatesResponseSchema)),
      ),
    },
    model: {
      listModelRecommendations: vi.fn(),
      listModelDeployments: vi.fn(() =>
        Promise.resolve(create(ListModelDeploymentsResponseSchema, { deployments })),
      ),
      createModelDeployment: vi.fn(),
      updateModelDeployment: vi.fn(),
      updateModelDeploymentMetadata: vi.fn(),
      removeModelDeployment: vi.fn(),
      pauseModel: vi.fn(),
      resumeModel: vi.fn(),
    },
  };
  return { model, route };
}

/** Opens the Create inspector and waits for its opening focus, so typing can't lose keys to it. */
async function openCreateRoute(user: UserEvent) {
  await user.click(await screen.findByRole("button", { name: "Create route" }));
  await waitForInspectorFocus(await screen.findByTestId("inspector-content"));
}

async function submitSingleMemberCreate(user: UserEvent, name: string) {
  await openCreateRoute(user);
  await user.type(screen.getByRole("textbox", { name: "Name" }), name);
  await user.click(screen.getByRole("button", { name: "Show Primary deployments options" }));
  await user.click(await screen.findByRole("option", { name: "Local model Ready" }));
  await user.click(screen.getByRole("button", { name: "Save route" }));
}

/** The accessible names of a route form's tiers, in order. */
function tierNames(form: HTMLElement): string[] {
  return within(form)
    .getAllByRole("group", { name: /^(Primary|Fallback)/ })
    .map((group) => group.getAttribute("aria-labelledby")!)
    .map((id) => document.getElementById(id)!.textContent);
}

function setViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
  window.dispatchEvent(new Event("resize"));
}

afterEach(() => {
  setViewportWidth(1024);
  vi.useRealTimers();
});

describe("RoutesPage", () => {
  it("reads h1 page, h2 sections, and the route table with no skipped level", async () => {
    const { model, route } = clients([
      routeView({ id: "route-available", name: "available", enabled: true, available: true }),
    ]);
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);
    await screen.findByRole("row", { name: "available" });

    expect(screen.getAllByRole("heading", { level: 2 }).map((h) => h.textContent)).toEqual([
      "Inference",
      "Routes",
    ]);
    expectHeadingOutline(document.body, [1, 2, 2]);
  });

  it("shows the site's inference and opens a route on the page's range", async () => {
    const user = setupUser();
    const watchMetrics = vi.fn(
      staticWatch((query) =>
        queryResult({
          series:
            query?.metric === "router.requests"
              ? [metricSeries({}, [counterPoint(0, 0, false, 4)])]
              : [],
          stepCount: 1,
        }),
      ),
    );
    const { model, route } = clients([routeView({ id: "route-a", name: "chat" })]);
    render(
      <RoutesPage
        client={route}
        metricsClient={metricsClientWith(watchMetrics)}
        modelClient={model}
      />,
    );

    const inference = await screen.findByRole("region", { name: "Inference" });
    await waitFor(() =>
      expect(within(inference).getByRole("button", { name: /^Requests/ })).toHaveTextContent(
        "Requests4",
      ),
    );
    // Scoped to routes, the cards count the requests routes handled.
    expect(watchMetrics.mock.calls[0][0].queries?.[0].query?.metric).toBe("router.requests");

    const range = screen.getByRole("radiogroup", { name: "Routes range" });
    await user.click(within(range).getByRole("radio", { name: "6h" }));
    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(2));

    await user.click(await screen.findByRole("row", { name: "chat" }));
    const panel = await screen.findByRole("complementary", { name: "chat" });
    expect(await within(panel).findByRole("radio", { name: "6h" })).toBeChecked();
  });

  it("delays initial feedback and holds completed Routes content for the minimum", async () => {
    setTiming(productionTiming);
    vi.useFakeTimers();
    let resolveRoutes!: (response: Awaited<ReturnType<RouteClient["route"]["listRoutes"]>>) => void;
    const { model, route } = clients([]);
    route.route.listRoutes = vi.fn(
      () =>
        new Promise<Awaited<ReturnType<RouteClient["route"]["listRoutes"]>>>((resolve) => {
          resolveRoutes = resolve;
        }),
    );

    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    expect(screen.queryByText("Loading routes…")).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(productionTiming.loadingRevealMs);
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading routes…");

    await act(async () => {
      resolveRoutes(create(ListRoutesResponseSchema));
      await Promise.resolve();
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading routes…");
    expect(screen.queryByText("No routes yet")).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(productionTiming.loadingMinimumVisibleMs);
    });
    expect(screen.queryByText("Loading routes…")).not.toBeInTheDocument();
    expect(screen.getByText("No routes yet")).toBeVisible();
  });

  it("delays route-member option feedback without delaying the options request", async () => {
    setTiming(productionTiming);
    vi.useFakeTimers();
    let resolveDeployments!: (
      response: Awaited<ReturnType<ModelClient["model"]["listModelDeployments"]>>,
    ) => void;
    const { model, route } = clients([]);
    model.model.listModelDeployments = vi.fn(
      () =>
        new Promise<Awaited<ReturnType<ModelClient["model"]["listModelDeployments"]>>>(
          (resolve) => {
            resolveDeployments = resolve;
          },
        ),
    );

    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);
    fireEvent.click(screen.getByRole("button", { name: "Create route" }));
    await act(async () => {
      await Promise.resolve();
    });
    fireEvent.click(screen.getByRole("button", { name: "Show Primary deployments options" }));

    expect(model.model.listModelDeployments).toHaveBeenCalledTimes(1);
    expect(screen.queryByText("Loading options…")).not.toBeInTheDocument();
    expect(screen.queryByText("No options available.")).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(productionTiming.loadingRevealMs);
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading options…");

    await act(async () => {
      resolveDeployments(
        create(ListModelDeploymentsResponseSchema, {
          deployments: [deployment("deployment-a", "Local model")],
        }),
      );
      await Promise.resolve();
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading options…");
    expect(screen.queryByRole("option", { name: /Local model Ready/i })).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(productionTiming.loadingMinimumVisibleMs);
    });
    expect(screen.queryByText("Loading options…")).not.toBeInTheDocument();
    expect(screen.getByRole("option", { name: /Local model Ready/i })).toBeVisible();
  });
  it("lists routes as rows with status and members, and opens details inside the inspector", async () => {
    const user = setupUser();
    const { model, route } = clients([
      routeView({ id: "route-disabled", name: "disabled", enabled: false, available: true }),
      routeView({ id: "route-available", name: "available", enabled: true, available: true }),
      routeView({ id: "route-unavailable", name: "unavailable", available: false }),
    ]);
    const router = renderIntegratedAt(["/routes?tab=traffic"], route, model);

    const disabled = await screen.findByRole("row", { name: "disabled" });
    expect(within(disabled).getByText("Disabled")).toHaveAttribute("data-variant", "neutral");
    expect(disabled).not.toHaveTextContent("Local model");
    expect(disabled).not.toHaveTextContent("route-disabled");
    expect(disabled).not.toHaveTextContent("deployment-a");
    const available = await screen.findByRole("row", { name: "available" });
    expect(within(available).getByText("Available")).toHaveAttribute("data-variant", "success");
    expect(routeRowNames()).toEqual(["available", "disabled", "unavailable"]);

    await user.click(disabled);
    expect(disabled).toHaveAttribute("data-state", "selected");
    expect(disabled).toHaveAttribute("aria-controls", "route-inspector-panel");
    expect(screen.getByTestId("inspector-content")).toContainElement(
      screen.getByTestId("route-inspector"),
    );
    expect(screen.queryByRole("form", { name: "Edit" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "More actions" })).toBeVisible();
    expectResourceMarker(within(disabled).getByText("disabled"), "route-disabled");
    expectResourceMarker(
      inspectorTitle(screen.getByRole("complementary", { name: "disabled" })),
      "route-disabled",
    );
    expect(router.state.location.pathname + router.state.location.search).toBe(
      "/routes?tab=traffic&panel=routes%2Froute%3Aroute-disabled",
    );

    await user.click(available);
    expect(router.state.location.pathname + router.state.location.search).toBe(
      "/routes?tab=traffic&panel=routes%2Froute%3Aroute-available",
    );
    expect(disabled).not.toHaveAttribute("data-state", "selected");
    expect(available).toHaveAttribute("data-state", "selected");
    expectResourceMarker(
      inspectorTitle(screen.getByRole("complementary", { name: "available" })),
      "route-available",
    );
  });

  it("opens a row with Enter and Space", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-a", name: "chat" })]);
    const router = renderIntegratedAt(["/routes"], route, model);

    (await screen.findByRole("row", { name: "chat" })).focus();
    await user.keyboard("{Enter}");
    expect(router.state.location.search).toBe("?panel=routes%2Froute%3Aroute-a");
  });

  it("shows each route's traffic over the range, with a dash where it has no speed", async () => {
    const watchMetrics = staticWatch((query) => {
      const byRoute = query?.groupBy?.includes("route");
      const labels: Record<string, string> = byRoute ? { route: "route-busy" } : {};
      switch (query?.metric) {
        case "router.requests":
          return queryResult({
            series: [metricSeries(labels, [counterPoint(0, 0, false, 1200)])],
            stepCount: 1,
          });
        case "router.tokens.completion":
          return queryResult({
            series: [metricSeries(labels, [counterPoint(0, 0, false, 4000)])],
            stepCount: 1,
          });
        case "router.attempt.decode_rate":
          return queryResult({
            series: [
              metricSeries(labels, [
                distributionPoint(0, { 0.95: 200 }, false, { count: 4, sum: 200 }),
              ]),
            ],
            stepCount: 1,
          });
        default:
          return queryResult({ series: [], stepCount: 1 });
      }
    });
    const { model, route } = clients([
      routeView({ id: "route-busy", name: "busy" }),
      routeView({ id: "route-idle", name: "idle" }),
    ]);
    render(
      <RoutesPage
        client={route}
        metricsClient={metricsClientWith(watchMetrics)}
        modelClient={model}
      />,
    );

    const cells = (name: string) =>
      within(screen.getByRole("row", { name }))
        .getAllByRole("cell")
        .map((cell) => cell.textContent);
    await waitFor(() => expect(cells("busy")[2]).toBe("1,200"));
    expect(cells("busy")).toEqual(["busy", "Available", "1,200", "4,000", "50 tokens/s"]);
    // No traffic in the range: zero counts, no speed.
    expect(cells("idle")).toEqual(["idle", "Available", "0", "0", "—"]);
    // Members are not a column; the View menu does not offer them either.
    expect(screen.queryByRole("columnheader", { name: "Members" })).not.toBeInTheDocument();
    // Time to first token is hidden until the View menu shows it.
    expect(
      screen.queryByRole("columnheader", { name: /Time to first token/ }),
    ).not.toBeInTheDocument();
  });

  it("loads an old URL that names the removed members column", async () => {
    const { model, route } = clients([routeView({ id: "route-a", name: "selected" })]);
    renderAt(["/routes?cols=route%2Cstatus%2Cmembers%2Crequests"], route, model);

    const table = await screen.findByRole("table");
    expect(
      within(table)
        .getAllByRole("columnheader")
        .map((header) => header.textContent),
    ).toEqual(["Route", "Status", "Requests"]);
  });

  it("returns from Connect to the selected route without changing the URL", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-a", name: "selected" })]);
    const router = renderAt(["/routes?panel=routes%2Froute%3Aroute-a"], route, model);

    expect(await screen.findByTestId("route-inspector")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Connect" }));
    expect(screen.getByTestId("route-connection-view")).toBeInTheDocument();
    expectResourceMarker(
      inspectorTitle(screen.getByRole("complementary", { name: "selected · Connect" })),
      "route-a",
    );
    expect(router.state.location.search).toBe(
      "?panel=routes%2Froute%3Aroute-a&panel=routes%2Fconnect%3Aroute-a",
    );

    await user.click(screen.getByRole("button", { name: "Back to selected" }));
    expect(screen.getByTestId("route-inspector")).toBeInTheDocument();
    expect(screen.queryByTestId("route-connection-view")).not.toBeInTheDocument();
    expect(router.state.location.search).toBe("?panel=routes%2Froute%3Aroute-a");
  });

  it("preserves model ancestry across a named route URL and browser traversal", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-a", name: "selected" })]);
    const router = renderConsoleAt(["/models"], route, model);
    const modelLocation = "/models?panel=models%2Fdeployment%3Adeployment-a";
    const routeLocation =
      "/routes?panel=models%2Fdeployment%3Adeployment-a&panel=routes%2Froute%3Aroute-a";

    await user.click(await screen.findByRole("row", { name: "Local model" }));
    expect(router.state.location.pathname + router.state.location.search).toBe(modelLocation);
    await user.click(await screen.findByRole("link", { name: "selected" }));

    await waitFor(() =>
      expect(router.state.location.pathname + router.state.location.search).toBe(routeLocation),
    );
    expect(await screen.findByRole("button", { name: "Back to Local model" })).toBeVisible();

    await router.navigate(-1);
    await waitFor(() =>
      expect(router.state.location.pathname + router.state.location.search).toBe(modelLocation),
    );
    // The panel keeps the published name while the models page swaps in its inspector.
    await waitFor(() =>
      expect(screen.getByRole("complementary", { name: "Local model" })).toBeVisible(),
    );

    await router.navigate(1);
    await waitFor(() =>
      expect(router.state.location.pathname + router.state.location.search).toBe(routeLocation),
    );
    await user.click(await screen.findByRole("button", { name: "Back to Local model" }));
    await waitFor(() =>
      expect(router.state.location.pathname + router.state.location.search).toBe(modelLocation),
    );
    expect(await screen.findByRole("complementary", { name: "Local model" })).toBeVisible();
  });

  it("names route members by deployment name and links them to the model deployment", async () => {
    const user = setupUser();
    const named = deployment("deployment-a", "Local model");
    named.modelDeploymentName = "Lab GPU";
    const { model, route } = clients([routeView({ id: "route-a", name: "selected" })], [named]);
    const router = renderConsoleAt(["/routes?panel=routes%2Froute%3Aroute-a"], route, model);

    const member = await screen.findByRole("listitem", { name: "Lab GPU, Ready" });
    await user.click(within(member).getByRole("link", { name: "Lab GPU" }));

    await waitFor(() =>
      expect(router.state.location.pathname + router.state.location.search).toBe(
        "/models?panel=routes%2Froute%3Aroute-a&panel=models%2Fdeployment%3Adeployment-a",
      ),
    );
    expect(await screen.findByRole("button", { name: "Back to selected" })).toBeVisible();
  });

  it("refreshes route and deployment status from the watch without a reload", async () => {
    const starting = deployment("deployment-a", "Local model");
    starting.state!.status = ModelDeploymentStatus.LOADING;
    const { model, route } = clients(
      [routeView({ id: "route-a", name: "selected", available: false })],
      [starting],
    );
    renderAt(["/routes?panel=routes%2Froute%3Aroute-a"], route, model);

    expect(await screen.findByRole("listitem", { name: "Local model, Loading" })).toBeVisible();

    vi.mocked(route.route.listRoutes).mockResolvedValue(
      create(ListRoutesResponseSchema, {
        routes: [routeView({ id: "route-a", name: "selected" })],
      }),
    );
    vi.mocked(model.model.listModelDeployments).mockResolvedValue(
      create(ListModelDeploymentsResponseSchema, {
        deployments: [deployment("deployment-a", "Local model")],
      }),
    );
    await tick();

    expect(await screen.findByRole("listitem", { name: "Local model, Ready" })).toBeVisible();
  });

  it("lists routes created elsewhere and drops routes deleted elsewhere", async () => {
    const { model, route } = clients([routeView({ id: "route-a", name: "alpha" })]);
    renderAt(["/routes"], route, model);
    expect(await screen.findByRole("row", { name: "alpha" })).toBeVisible();

    vi.mocked(route.route.listRoutes).mockResolvedValue(
      create(ListRoutesResponseSchema, {
        routes: [
          routeView({ id: "route-a", name: "alpha" }),
          routeView({ id: "route-b", name: "beta" }),
        ],
      }),
    );
    await tick();
    expect(routeRowNames()).toEqual(["alpha", "beta"]);

    vi.mocked(route.route.listRoutes).mockResolvedValue(
      create(ListRoutesResponseSchema, { routes: [routeView({ id: "route-b", name: "beta" })] }),
    );
    await tick();
    expect(routeRowNames()).toEqual(["beta"]);
  });

  it("holds the listed routes while a route is being edited, then shows what changed", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-edit", name: "editable" })]);
    renderIntegratedAt(
      ["/routes?panel=routes%2Froute%3Aroute-edit&panel=routes%2Fedit%3Aroute-edit"],
      route,
      model,
    );
    expect(await screen.findByRole("form", { name: "Edit route" })).toBeInTheDocument();
    await user.type(screen.getByRole("textbox", { name: "Name" }), "-draft");

    vi.mocked(route.route.listRoutes).mockResolvedValue(
      create(ListRoutesResponseSchema, {
        routes: [routeView({ id: "route-edit", name: "renamed", generation: 2n })],
      }),
    );
    await tick();

    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("editable-draft");
    expect(screen.queryByText("renamed")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    await user.click(await screen.findByRole("button", { name: "Discard changes" }));
    expect(await screen.findByRole("row", { name: "renamed" })).toBeVisible();
  });

  it("keeps an accepted change over a stream payload read before it", async () => {
    const user = setupUser();
    const groups = [{ selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["a"] }];
    const { model, route } = clients([routeView({ id: "route-chat", name: "chat", groups })]);
    vi.mocked(route.route.replaceRoute).mockResolvedValue(
      create(ReplaceRouteResponseSchema, {
        route: routeView({
          id: "route-chat",
          name: "chat",
          enabled: false,
          generation: 2n,
          groups,
        }),
      }),
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await user.click(await screen.findByRole("row", { name: "chat" }));
    const panel = screen.getByRole("complementary", { name: "chat" });
    await chooseMoreAction(user, "Disable", panel);
    expect(await within(panel).findByText("Disabled")).toBeVisible();

    // The stream still shows the earlier generation, which does not undo the accepted change.
    await tick();
    expect(within(panel).getByText("Disabled")).toBeVisible();

    // A change made elsewhere after the stream showed the accepted one is not hidden by it.
    vi.mocked(route.route.listRoutes).mockResolvedValue(
      create(ListRoutesResponseSchema, {
        routes: [
          routeView({ id: "route-chat", name: "chat", enabled: false, generation: 2n, groups }),
        ],
      }),
    );
    await tick();
    vi.mocked(route.route.listRoutes).mockResolvedValue(
      create(ListRoutesResponseSchema, {
        routes: [routeView({ id: "route-chat", name: "chat", generation: 3n, groups })],
      }),
    );
    await tick();
    expect(await within(panel).findByText("Available")).toBeVisible();
  });

  it("uses the exact upstream title for nested Routes inspector ancestry", async () => {
    const user = setupUser();
    const upstream = externalDeployment("deployment-a", "Deliberately divergent provider");
    const { model, route } = clients(
      [
        routeView({
          id: "route-a",
          name: "selected",
          groups: [
            { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
          ],
        }),
      ],
      [upstream],
    );
    const routeLocation =
      "/routes?panel=models%2Fdeployment%3Adeployment-a&panel=routes%2Froute%3Aroute-a";
    renderConsoleAt([routeLocation], route, model);

    const back = await screen.findByRole("button", { name: "Back to Upstream model" });
    expect(back).toBeVisible();
    expect(
      screen.queryByRole("button", { name: /Deliberately divergent provider/ }),
    ).not.toBeInTheDocument();
    await user.click(back);
    expect(await screen.findByRole("complementary", { name: "Upstream model" })).toBeVisible();
    expect(screen.getByText("deployment-a")).toBeVisible();
  });

  it("restores direct cross-page ancestry and returns to the model inspector", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-a", name: "selected" })]);
    const routeLocation =
      "/routes?panel=models%2Fdeployment%3Adeployment-a&panel=routes%2Froute%3Aroute-a";
    const router = renderConsoleAt([routeLocation], route, model);

    await user.click(await screen.findByRole("button", { name: "Back to Local model" }));

    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/models");
      expect(router.state.location.search).toBe("?panel=models%2Fdeployment%3Adeployment-a");
    });
    expect(await screen.findByRole("complementary", { name: "Local model" })).toBeVisible();
  });

  it("restores direct cross-page ancestry and closes to the Routes base", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-a", name: "selected" })]);
    const routeLocation =
      "/routes?panel=models%2Fdeployment%3Adeployment-a&panel=routes%2Froute%3Aroute-a";
    const router = renderConsoleAt([routeLocation], route, model);

    expect(await screen.findByRole("button", { name: "Back to Local model" })).toBeVisible();
    expect(router.state.location.pathname + router.state.location.search).toBe(routeLocation);
    await user.click(screen.getByRole("button", { name: "Close inspector" }));

    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/routes");
      expect(router.state.location.search).toBe("");
    });
    expect(screen.queryByTestId("route-inspector")).not.toBeInTheDocument();
  });

  it("uses the wide inspector and switches to the narrow modal without losing the route", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-responsive", name: "responsive" })]);
    setViewportWidth(1024);
    renderConsoleAt(["/routes"], route, model);

    await user.click(await screen.findByRole("row", { name: "responsive" }));
    expect(screen.getByTestId("inspector-content").tagName).toBe("ASIDE");
    expect(screen.getByTestId("route-inspector")).toBeInTheDocument();

    act(() => setViewportWidth(500));
    await waitFor(() => expect(screen.getByTestId("inspector-content").tagName).toBe("DIV"));
    expect(screen.getByRole("dialog")).toContainElement(screen.getByTestId("route-inspector"));
    const inspectorHeading = within(screen.getByTestId("inspector-content")).getByRole("heading", {
      name: "responsive",
    });
    await waitFor(() => expect(inspectorHeading).toHaveFocus());

    await user.click(screen.getByRole("button", { name: "Connect" }));
    expect(screen.getByRole("dialog")).toContainElement(
      screen.getByTestId("route-connection-view"),
    );
    await user.click(screen.getByRole("button", { name: "Back to responsive" }));
    expect(screen.getByRole("dialog")).toContainElement(screen.getByTestId("route-inspector"));
  });

  it("replaces a dismissed root Routes selection so Back or Forward cannot reopen it", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-a", name: "selected" })]);
    const router = renderAt(
      ["/routes?tab=traffic", "/routes?tab=traffic&panel=routes%2Froute%3Aroute-a"],
      route,
      model,
    );

    expect(await screen.findByTestId("route-inspector")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/routes");
      expect(router.state.location.search).toBe("?tab=traffic");
    });

    await router.navigate(-1);
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/routes");
      expect(router.state.location.search).toBe("?tab=traffic");
    });
    expect(screen.queryByTestId("route-inspector")).not.toBeInTheDocument();

    await router.navigate(1);
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/routes");
      expect(router.state.location.search).toBe("?tab=traffic");
    });
    expect(screen.queryByTestId("route-inspector")).not.toBeInTheDocument();
  });

  it("loads every non-teardown deployment and keeps stale route members available for editing", async () => {
    const user = setupUser();
    const { model, route } = clients(
      [
        routeView({
          id: "route-a",
          name: "route-a",
          groups: [{ selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["missing"] }],
        }),
      ],
      [
        deployment("ready", "Ready model"),
        deployment("paused", "Paused model", ModelDeploymentDesiredState.PAUSED),
        externalDeployment("external", "OpenAI-compatible upstream"),
        deployment("teardown", "Removed", ModelDeploymentDesiredState.TEARDOWN),
      ],
    );
    renderConsoleAt(["/routes"], route, model);
    await user.click(await screen.findByRole("row", { name: "route-a" }));
    await chooseMoreAction(user, "Edit");
    await user.click(screen.getByRole("combobox", { name: "Primary deployments" }));
    expect(screen.getByRole("option", { name: "Ready model Ready" })).toBeVisible();
    expect(screen.getByRole("option", { name: "Paused model Pausing" })).toBeVisible();
    expect(screen.getByRole("option", { name: "Upstream model Unknown" })).toBeVisible();
    expect(
      screen.queryByRole("option", { name: "OpenAI-compatible upstream external" }),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("https://private.example.test/v1")).not.toBeInTheDocument();
    expect(screen.queryByText("private-api-key")).not.toBeInTheDocument();
    expect(screen.queryByText("private-model-id")).not.toBeInTheDocument();
    expect(screen.queryByRole("option", { name: /Removed teardown/ })).not.toBeInTheDocument();
    expect(
      screen.getByRole("option", { name: /Unavailable deployment \(missing\) missing/ }),
    ).toHaveAttribute("aria-disabled", "true");
    expectResourceMarker(
      screen
        .getByRole("option", { name: "Ready model Ready" })
        .querySelector('[data-slot="resource-marker"]')?.parentElement,
      "ready",
    );
    expectResourceMarker(
      screen
        .getByRole("option", { name: "Upstream model Unknown" })
        .querySelector('[data-slot="resource-marker"]')?.parentElement,
      "external",
    );
    expect(
      screen
        .getByRole("option", { name: /Unavailable deployment \(missing\) missing/ })
        .querySelector('[data-slot="resource-marker"]'),
    ).toBeNull();
  });

  it("saves reordered groups with selection derived from member count", async () => {
    const user = setupUser();
    const { model, route } = clients(
      [
        routeView({
          id: "route-fallback",
          name: "fallback",
          groups: [
            { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
            {
              selection: FallbackGroupSelection.ROUND_ROBIN,
              modelDeploymentIds: ["deployment-b", "deployment-c"],
            },
          ],
        }),
      ],
      [
        deployment("deployment-a", "Primary"),
        deployment("deployment-b", "Backup"),
        deployment("deployment-c", "Recovery"),
      ],
    );
    vi.mocked(route.route.replaceRoute).mockResolvedValue(
      create(ReplaceRouteResponseSchema, {
        route: routeView({
          id: "route-fallback",
          name: "fallback",
          generation: 2n,
          groups: [
            {
              selection: FallbackGroupSelection.ROUND_ROBIN,
              modelDeploymentIds: ["deployment-b", "deployment-c"],
            },
            { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
          ],
        }),
      }),
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await user.click(await screen.findByRole("row", { name: "fallback" }));
    await chooseMoreAction(user, "Edit");
    await user.click(screen.getByRole("button", { name: "Remove Recovery" }));
    await user.click(screen.getByRole("button", { name: "Show Fallback 1 deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Recovery Ready" }));
    await user.click(screen.getByRole("button", { name: "Move Fallback 1 up" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(route.route.replaceRoute).toHaveBeenCalledWith({
      inferenceRouteId: "route-fallback",
      expectedGeneration: 1n,
      name: "fallback",
      enabled: true,
      fallbackGroups: [
        {
          selection: FallbackGroupSelection.ROUND_ROBIN,
          modelDeploymentIds: ["deployment-b", "deployment-c"],
        },
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
      ],
    });
  });

  it("creates groups with SINGLE for one member and ROUND_ROBIN for two, without a kind", async () => {
    const user = setupUser();
    const { model, route } = clients(
      [],
      [
        deployment("deployment-a", "Primary"),
        deployment("deployment-b", "Backup"),
        deployment("deployment-c", "Recovery"),
      ],
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await openCreateRoute(user);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "chat");
    await user.click(screen.getByRole("button", { name: "Show Primary deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Primary Ready" }));
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Add fallback" }));
    await user.click(screen.getByRole("button", { name: "Show Fallback 1 deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Backup Ready" }));
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Show Fallback 1 deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Recovery Ready" }));
    await user.click(screen.getByRole("button", { name: "Save route" }));

    expect(route.route.createRoute).toHaveBeenCalledWith({
      name: "chat",
      enabled: true,
      fallbackGroups: [
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
        {
          selection: FallbackGroupSelection.ROUND_ROBIN,
          modelDeploymentIds: ["deployment-b", "deployment-c"],
        },
      ],
    });
    expect(vi.mocked(route.route.createRoute).mock.calls[0][0]).not.toHaveProperty("kind");
  });

  it("adds a fallback group to an auto-created single-member route", async () => {
    const user = setupUser();
    const { model, route } = clients(
      [routeView({ id: "route-gemma", name: "gemma", generation: 3n })],
      [deployment("deployment-a", "Gemma"), deployment("deployment-b", "Qwen")],
    );
    vi.mocked(route.route.replaceRoute).mockResolvedValue(
      create(ReplaceRouteResponseSchema, {
        route: routeView({
          id: "route-gemma",
          name: "gemma",
          generation: 4n,
          groups: [
            { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
            { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-b"] },
          ],
        }),
      }),
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await user.click(await screen.findByRole("row", { name: "gemma" }));
    await chooseMoreAction(user, "Edit");
    await user.click(screen.getByRole("button", { name: "Add fallback" }));
    await user.click(screen.getByRole("button", { name: "Show Fallback 1 deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Qwen Ready" }));
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(route.route.replaceRoute).toHaveBeenCalledWith({
      inferenceRouteId: "route-gemma",
      expectedGeneration: 3n,
      name: "gemma",
      enabled: true,
      fallbackGroups: [
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-b"] },
      ],
    });
    expect(vi.mocked(route.route.replaceRoute).mock.calls[0][0]).not.toHaveProperty("kind");
    await waitFor(() => expect(route.route.replaceRoute).toHaveBeenCalledTimes(1));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("rejects invalid mutations, preserves stale generations, and deletes exactly", async () => {
    const user = setupUser();
    const { model, route } = clients([
      routeView({ id: "route-a", name: "route-a", generation: 7n }),
    ]);
    vi.mocked(route.route.replaceRoute).mockRejectedValue(new Error("stale generation"));
    vi.mocked(route.route.deleteRoute).mockRejectedValue(new Error("delete failed"));
    renderConsoleAt(["/routes"], route, model);

    await user.click(await screen.findByRole("row", { name: "route-a" }));
    await chooseMoreAction(user, "Edit");
    expectResourceMarker(
      inspectorTitle(screen.getByRole("complementary", { name: "route-a · Edit" })),
      "route-a",
    );
    const name = screen.getByRole("textbox", { name: "Name" });
    await user.clear(name);
    await user.type(name, "renamed");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(route.route.replaceRoute).toHaveBeenCalledWith({
      inferenceRouteId: "route-a",
      expectedGeneration: 7n,
      name: "renamed",
      enabled: true,
      fallbackGroups: [
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
      ],
    });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Route update outcome is uncertain (Unknown): stale generation",
    );
    expect(screen.getByRole("form", { name: "Edit route" })).toBeInTheDocument();

    // The renamed draft is dirty, so leaving the editor asks before discarding it.
    await user.click(screen.getByRole("button", { name: "Back to route-a" }));
    await user.click(await screen.findByRole("button", { name: "Discard changes" }));
    await chooseMoreAction(user, "Delete…");
    const confirmation = screen.getByRole("dialog", { name: "Delete route?" });
    expect(confirmation).toBeVisible();
    expectResourceMarker(
      within(confirmation).getByText("Delete route route-a? This cannot be undone."),
      "route-a",
    );
    expect(route.route.deleteRoute).not.toHaveBeenCalled();
    await user.click(within(confirmation).getByRole("button", { name: "Delete" }));
    expect(route.route.deleteRoute).toHaveBeenCalledWith({
      inferenceRouteId: "route-a",
      expectedGeneration: 7n,
    });
    expect(await within(confirmation).findByRole("alert")).toHaveTextContent(
      "Route deletion outcome is uncertain (Unknown): delete failed",
    );
    expect(screen.getByTestId("route-inspector")).toBeInTheDocument();
  });

  it("stays on the routes page after deleting a route reached from the models page", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-a", name: "selected" })]);
    const router = renderConsoleAt(["/models", "/routes"], route, model);
    const visited: string[] = [];
    router.subscribe((state) => visited.push(state.location.pathname));

    await user.click(await screen.findByRole("row", { name: "selected" }));
    await chooseMoreAction(user, "Delete…");
    const confirmation = screen.getByRole("dialog", { name: "Delete route?" });
    await user.click(within(confirmation).getByRole("button", { name: "Delete" }));

    await waitFor(() => expect(route.route.deleteRoute).toHaveBeenCalledOnce());
    await waitFor(() => expect(screen.queryByTestId("route-inspector")).not.toBeInTheDocument());
    expect(router.state.location.pathname + router.state.location.search).toBe("/routes");
    expect(visited).not.toContain("/models");
  });

  it("renders human-readable availability and member exclusion reasons", async () => {
    const { model, route } = clients([
      create(InferenceRouteViewSchema, {
        route: create(InferenceRouteSchema, {
          inferenceRouteId: "route-unavailable",
          name: "unavailable",
          enabled: false,
          fallbackGroups: [
            create(FallbackGroupSchema, {
              selection: FallbackGroupSelection.SINGLE,
              modelDeploymentIds: ["deployment-a"],
            }),
          ],
        }),
        availability: create(InferenceRouteAvailabilitySchema, {
          available: false,
          exclusionReasons: [InferenceRouteUnavailabilityReason.DISABLED],
          fallbackGroups: [
            create(FallbackGroupAvailabilitySchema, {
              fallbackGroupIndex: 0,
              members: [
                create(InferenceRouteMemberAvailabilitySchema, {
                  modelDeploymentId: "deployment-a",
                  eligible: false,
                  exclusionReasons: [
                    InferenceRouteMemberExclusionReason.MODEL_DEPLOYMENT_NOT_READY,
                  ],
                }),
              ],
            }),
          ],
        }),
      }),
    ]);
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await setupUser().click(await screen.findByRole("row", { name: "unavailable" }));
    const inspector = screen.getByTestId("route-inspector");
    // The card and the inspector header project the same status, so a disabled route reads
    // Disabled on both rather than Disabled on the card and Unavailable in the inspector.
    const panel = screen.getByRole("complementary", { name: "unavailable" });
    expect(within(panel).getByText("Disabled")).toHaveAttribute("data-slot", "badge");
    expect(inspector).not.toContainElement(within(panel).getByText("Disabled"));
    expect(
      within(screen.getByRole("row", { name: "unavailable" })).getByText("Disabled"),
    ).toBeVisible();
    const card = within(within(inspector).getByRole("list", { name: "Primary members" })).getByRole(
      "listitem",
    );
    expect(card).toHaveTextContent("Local model");
    expect(card).toHaveTextContent("Model deployment not ready");
    expectResourceMarker(within(card).getByText("Local model"), "deployment-a");
    // The page now supplies the member name, so the raw deployment ID no longer leaks into the root.
    expect(inspector).not.toHaveTextContent("deployment-a");
  });

  it("creates enabled routes and replaces exact payloads with opaque generations", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    const created = routeView({
      id: "route-new",
      name: "chat-new",
    });
    const replaced = routeView({
      id: "route-new",
      name: "chat-renamed",
      generation: 2n,
    });
    vi.mocked(route.route.createRoute).mockResolvedValue(
      create(CreateRouteResponseSchema, { route: created }),
    );
    vi.mocked(route.route.replaceRoute).mockResolvedValue(
      create(ReplaceRouteResponseSchema, { route: replaced }),
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);
    await openCreateRoute(user);
    expect(screen.queryByRole("switch")).not.toBeInTheDocument();
    await user.type(screen.getByRole("textbox", { name: "Name" }), "chat-new");
    await user.click(
      await screen.findByRole("button", { name: "Show Primary deployments options" }),
    );
    await user.click(await screen.findByRole("option", { name: "Local model Ready" }));
    expect(screen.getByRole("button", { name: "Remove Local model" })).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Clear Primary deployments" }),
    ).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Save route" }));
    expect(route.route.createRoute).toHaveBeenCalledWith({
      name: "chat-new",
      enabled: true,
      fallbackGroups: [
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
      ],
    });
    await user.click(await screen.findByRole("row", { name: "chat-new" }));
    await chooseMoreAction(user, "Edit");
    expect(screen.queryByRole("switch")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save route" })).not.toBeInTheDocument();
    const name = screen.getByRole("textbox", { name: "Name" });
    await user.clear(name);
    await user.type(name, "chat-renamed");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(route.route.replaceRoute).toHaveBeenCalledWith({
      inferenceRouteId: "route-new",
      expectedGeneration: 1n,
      name: "chat-renamed",
      enabled: true,
      fallbackGroups: [
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
      ],
    });
  });

  it("disables and enables a route from its details", async () => {
    const user = setupUser();
    const groups = [
      { selection: FallbackGroupSelection.ROUND_ROBIN, modelDeploymentIds: ["a", "b"] },
    ];
    const { model, route } = clients([routeView({ id: "route-chat", name: "chat", groups })]);
    vi.mocked(route.route.replaceRoute)
      .mockResolvedValueOnce(
        create(ReplaceRouteResponseSchema, {
          route: routeView({
            id: "route-chat",
            name: "chat",
            enabled: false,
            generation: 2n,
            groups,
          }),
        }),
      )
      .mockResolvedValueOnce(
        create(ReplaceRouteResponseSchema, {
          route: routeView({ id: "route-chat", name: "chat", generation: 3n, groups }),
        }),
      );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await user.click(await screen.findByRole("row", { name: "chat" }));
    const panel = screen.getByRole("complementary", { name: "chat" });
    expect(within(panel).getByText("Available")).toHaveAttribute("data-slot", "badge");
    await chooseMoreAction(user, "Disable", panel);
    expect(route.route.replaceRoute).toHaveBeenLastCalledWith({
      inferenceRouteId: "route-chat",
      expectedGeneration: 1n,
      name: "chat",
      enabled: false,
      fallbackGroups: groups,
    });
    expect(await within(panel).findByText("Disabled")).toHaveAttribute("data-slot", "badge");
    expect(await moreActionNames(user, panel)).toEqual(["Edit", "Enable", "Delete…"]);
    await user.click(screen.getByRole("menuitem", { name: "Enable" }));
    expect(route.route.replaceRoute).toHaveBeenLastCalledWith({
      inferenceRouteId: "route-chat",
      expectedGeneration: 2n,
      name: "chat",
      enabled: true,
      fallbackGroups: groups,
    });
    expect(await within(panel).findByText("Available")).toHaveAttribute("data-slot", "badge");
    await user.click(within(panel).getByRole("button", { name: "More actions" }));
    expect(await screen.findByRole("menuitem", { name: "Disable" })).not.toHaveAttribute(
      "aria-disabled",
    );
  });

  it("shows a failed Disable inside the route details and keeps the route enabled", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-chat", name: "chat" })]);
    vi.mocked(route.route.replaceRoute).mockRejectedValue(
      new ConnectError("stale generation", Code.FailedPrecondition),
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await user.click(await screen.findByRole("row", { name: "chat" }));
    await chooseMoreAction(user, "Disable");
    expect(
      await within(screen.getByTestId("route-inspector")).findByRole("alert"),
    ).toHaveTextContent("Disable was rejected (FailedPrecondition): stale generation");
    expect(screen.getAllByRole("alert")).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "More actions" }));
    expect(await screen.findByRole("menuitem", { name: "Disable" })).not.toHaveAttribute(
      "aria-disabled",
    );
  });

  it("drops a cancelled edit's save error when returning to the route details", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-edit", name: "editable" })]);
    vi.mocked(route.route.replaceRoute).mockRejectedValue(
      new ConnectError("stale generation", Code.FailedPrecondition),
    );
    renderIntegratedAt(
      ["/routes?panel=routes%2Froute%3Aroute-edit&panel=routes%2Fedit%3Aroute-edit"],
      route,
      model,
    );

    await user.type(await screen.findByRole("textbox", { name: "Name" }), "-renamed");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(
      await within(screen.getByRole("form", { name: "Edit route" })).findByRole("alert"),
    ).toHaveTextContent("Route update was rejected (FailedPrecondition): stale generation");

    await user.click(screen.getByRole("button", { name: "Back to editable" }));
    await user.click(screen.getByRole("button", { name: "Discard changes" }));
    expect(await screen.findByTestId("route-inspector")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("guards dirty Create dismissal and warns before document unload", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    const router = renderIntegratedAt(["/routes"], route, model);

    await openCreateRoute(user);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "draft");
    const beforeUnload = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(beforeUnload);
    expect(beforeUnload.defaultPrevented).toBe(true);

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    expect(screen.getByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Discard changes" }));
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/routes");
      expect(router.state.location.search).toBe("");
    });
  });

  it("guards dirty Back, Close, Escape, and global internal navigation", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-edit", name: "editable" })]);
    const editLocation =
      "/routes?panel=routes%2Froute%3Aroute-edit&panel=routes%2Fedit%3Aroute-edit";
    const router = renderIntegratedAt([editLocation], route, model);

    const name = await screen.findByRole("textbox", { name: "Name" });
    await user.type(name, " changed");

    await user.click(screen.getByRole("button", { name: "Back to editable" }));
    expect(screen.getByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument(),
    );
    expect(router.state.location.pathname + router.state.location.search).toBe(editLocation);
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("editable changed");
    const beforeUnload = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(beforeUnload);
    expect(beforeUnload.defaultPrevented).toBe(true);

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    expect(screen.getByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument(),
    );
    expect(screen.getByRole("form", { name: "Edit route" })).toBeInTheDocument();

    await user.keyboard("{Escape}");
    expect(screen.getByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();
    expect(screen.getByRole("form", { name: "Edit route" })).toBeInTheDocument();

    await user.click(screen.getByRole("link", { name: "Machines navigation" }));
    expect(screen.getByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Discard changes" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/machines"));
    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();
  });

  it("restores canceled browser Back and replays a confirmed traversal exactly once", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-edit", name: "editable" })]);
    const editLocation =
      "/routes?panel=routes%2Froute%3Aroute-edit&panel=routes%2Fedit%3Aroute-edit";
    const router = renderIntegratedAt(["/routes", editLocation], route, model);

    await user.type(await screen.findByRole("textbox", { name: "Name" }), " changed");
    void router.navigate(-1);
    expect(await screen.findByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    expect(router.state.location.pathname + router.state.location.search).toBe(editLocation);
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    expect(router.state.location.pathname + router.state.location.search).toBe(editLocation);

    void router.navigate(-1);
    expect(await screen.findByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Discard changes" }));
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/routes");
      expect(router.state.location.search).toBe("");
    });
    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();
  });

  it("restores canceled browser Forward and replays a confirmed traversal exactly once", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-edit", name: "editable" })]);
    const editLocation =
      "/routes?panel=routes%2Froute%3Aroute-edit&panel=routes%2Fedit%3Aroute-edit";
    const router = renderIntegratedAt(["/routes", editLocation, "/machines"], route, model, 1);

    await user.type(await screen.findByRole("textbox", { name: "Name" }), " changed");
    void router.navigate(1);
    expect(await screen.findByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    expect(router.state.location.pathname + router.state.location.search).toBe(editLocation);
    await user.click(screen.getByRole("button", { name: "Keep editing" }));

    void router.navigate(1);
    expect(await screen.findByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Discard changes" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/machines"));
    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();
  });

  it("disables inspector stack navigation with an explanation while saving", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-edit", name: "editable" })]);
    let resolveUpdate!: (value: Awaited<ReturnType<RouteClient["route"]["replaceRoute"]>>) => void;
    vi.mocked(route.route.replaceRoute).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveUpdate = resolve;
        }),
    );
    renderIntegratedAt(
      ["/routes?panel=routes%2Froute%3Aroute-edit&panel=routes%2Fedit%3Aroute-edit"],
      route,
      model,
    );

    await user.type(await screen.findByRole("textbox", { name: "Name" }), "-changed");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByRole("button", { name: "Saving…" })).toBeDisabled();
    const back = screen.getByRole("button", { name: "Back to editable" });
    const close = screen.getByRole("button", { name: "Close inspector" });
    expect(back).toBeDisabled();
    expect(close).toBeDisabled();
    expect(back).toHaveAccessibleDescription(
      "Saving changes. Inspector navigation is unavailable.",
    );
    expect(close).toHaveAccessibleDescription(
      "Saving changes. Inspector navigation is unavailable.",
    );
    await user.keyboard("{Escape}");
    expect(screen.getByRole("form", { name: "Edit route" })).toBeInTheDocument();
    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();

    resolveUpdate(
      create(ReplaceRouteResponseSchema, {
        route: routeView({ id: "route-edit", name: "editable-changed", generation: 2n }),
      }),
    );
    // A saved edit returns to the route's details rather than staying in the form.
    expect(await screen.findByTestId("route-inspector")).toBeVisible();
    expect(screen.queryByRole("form", { name: "Edit route" })).not.toBeInTheDocument();
  });

  it("dismisses untouched Create through Close", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    const router = renderIntegratedAt(["/routes"], route, model);

    await user.click(await screen.findByRole("button", { name: "Create route" }));
    expect(screen.getByRole("form", { name: "Create route" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Close inspector" }));

    expect(router.state.location.pathname).toBe("/routes");
    expect(router.state.location.search).toBe("");
    expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument();
    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();
  });

  it("dismisses untouched Create through Escape", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    const router = renderIntegratedAt(["/routes"], route, model);

    await user.click(await screen.findByRole("button", { name: "Create route" }));
    await user.keyboard("{Escape}");

    expect(router.state.location.pathname).toBe("/routes");
    expect(router.state.location.search).toBe("");
    expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument();
  });

  it("dismisses untouched Edit through Close", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-edit", name: "editable" })]);
    const router = renderIntegratedAt(
      ["/routes?panel=routes%2Froute%3Aroute-edit&panel=routes%2Fedit%3Aroute-edit"],
      route,
      model,
    );

    expect(await screen.findByRole("form", { name: "Edit route" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Close inspector" }));

    expect(router.state.location.pathname).toBe("/routes");
    expect(router.state.location.search).toBe("");
    expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument();
  });

  it("dismisses untouched Edit through Escape without enabling outside-click dismissal", async () => {
    const user = setupUser();
    setViewportWidth(500);
    const { model, route } = clients([routeView({ id: "route-edit", name: "editable" })]);
    const router = renderIntegratedAt(
      ["/routes?panel=routes%2Froute%3Aroute-edit&panel=routes%2Fedit%3Aroute-edit"],
      route,
      model,
    );

    expect(await screen.findByRole("form", { name: "Edit route" })).toBeInTheDocument();
    fireEvent.pointerDown(document.body, { button: 0 });
    expect(screen.getByRole("form", { name: "Edit route" })).toBeInTheDocument();

    await user.keyboard("{Escape}");
    expect(router.state.location.pathname).toBe("/routes");
    expect(router.state.location.search).toBe("");
    expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument();
    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();
  });

  it("creates successfully and opens the created route's inspector with focus on its heading", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockResolvedValue(
      create(CreateRouteResponseSchema, {
        route: routeView({ id: "route-created", name: "created" }),
      }),
    );
    const router = renderIntegratedAt(["/routes"], route, model);

    await openCreateRoute(user);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "created");
    await user.click(screen.getByRole("button", { name: "Show Primary deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Local model Ready" }));
    await user.click(screen.getByRole("button", { name: "Save route" }));
    expect(route.route.createRoute).toHaveBeenCalledWith({
      name: "created",
      enabled: true,
      fallbackGroups: [
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
      ],
    });
    await waitFor(() =>
      expect(router.state.location.search).toBe("?panel=routes%2Froute%3Aroute-created"),
    );
    expect(router.state.historyAction).toBe("REPLACE");
    const panel = await screen.findByRole("complementary", { name: "created" });
    await waitForInspectorFocus(panel, "created");
    expect(within(panel).getByRole("heading", { name: "created" })).toHaveFocus();
    expect(within(panel).getByTestId("route-inspector")).toBeVisible();
    expect(screen.getByRole("row", { name: "created" })).toBeVisible();
  });

  it("opens the created route above a cross-page model deployment parent", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockResolvedValue(
      create(CreateRouteResponseSchema, {
        route: routeView({ id: "route-created", name: "created" }),
      }),
    );
    const router = renderConsoleAt(
      ["/routes?panel=models%2Fdeployment%3Adeployment-a&panel=routes%2Fcreate"],
      route,
      model,
    );

    await user.type(await screen.findByRole("textbox", { name: "Name" }), "created");
    await user.click(screen.getByRole("button", { name: "Show Primary deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Local model Ready" }));
    await user.click(screen.getByRole("button", { name: "Save route" }));

    await waitFor(() =>
      expect(router.state.location.search).toBe(
        "?panel=models%2Fdeployment%3Adeployment-a&panel=routes%2Froute%3Aroute-created",
      ),
    );
    const panel = await screen.findByRole("complementary", { name: "created" });
    expect(within(panel).getByRole("button", { name: "Back to Local model" })).toBeVisible();
    await waitForInspectorFocus(panel, "created");
    expect(within(panel).getByRole("heading", { name: "created" })).toHaveFocus();
  });

  it("starts Create with one empty Primary tier, validates it inline, and focuses its picker", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await openCreateRoute(user);
    const form = screen.getByRole("form", { name: "Create route" });
    expect(tierNames(form)).toEqual(["Primary"]);
    expect(screen.getByRole("combobox", { name: "Primary deployments" })).toHaveValue("");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    await user.type(screen.getByRole("textbox", { name: "Name" }), "valid-name");
    await user.click(screen.getByRole("button", { name: "Save route" }));
    expect(route.route.createRoute).not.toHaveBeenCalled();
    const picker = screen.getByRole("combobox", { name: "Primary deployments" });
    expect(picker).toHaveFocus();
    expect(picker).toHaveAttribute("aria-invalid", "true");
    expect(picker).toHaveAccessibleDescription("Choose one to 32 deployments.");
    await user.click(screen.getByRole("button", { name: "Show Primary deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Local model Ready" }));
    expect(screen.queryByText("Choose one to 32 deployments.")).not.toBeInTheDocument();
    expect(within(form).getByRole("group", { name: "Primary · single" })).toBeVisible();
  });

  it("checks the name format and uniqueness next to the name field before saving", async () => {
    const user = setupUser();
    const { model, route } = clients([
      routeView({ id: "route-chat", name: "chat" }),
      routeView({ id: "route-other", name: "other" }),
    ]);
    renderAt(
      ["/routes?panel=routes%2Froute%3Aroute-chat&panel=routes%2Fedit%3Aroute-chat"],
      route,
      model,
    );

    const name = await screen.findByRole("textbox", { name: "Name" });
    const save = screen.getByRole("button", { name: "Save" });
    const expectNameError = async (value: string, message: string) => {
      await user.clear(name);
      if (value) await user.type(name, value);
      await user.click(save);
      expect(name).toHaveAccessibleDescription(message);
      expect(name).toHaveFocus();
    };

    await expectNameError("", "Route name is required.");
    await expectNameError(
      "Chat Route",
      "Use lowercase letters, digits, dashes, and periods, starting with a letter, up to 63 characters.",
    );
    await expectNameError(
      "1chat",
      "Use lowercase letters, digits, dashes, and periods, starting with a letter, up to 63 characters.",
    );
    await expectNameError(
      `a${"b".repeat(63)}`,
      "Use lowercase letters, digits, dashes, and periods, starting with a letter, up to 63 characters.",
    );
    await expectNameError("other", "A route named other already exists.");
    expect(route.route.replaceRoute).not.toHaveBeenCalled();

    // The route being edited may keep its own name.
    await user.clear(name);
    await user.type(name, "chat");
    await user.click(save);
    expect(name).not.toHaveAttribute("aria-invalid");
    expect(route.route.replaceRoute).toHaveBeenCalledOnce();
  });

  it("rejects a deployment repeated in another group next to that group's picker", async () => {
    const user = setupUser();
    const { model, route } = clients(
      [],
      [deployment("deployment-a", "Gemma"), deployment("deployment-b", "Qwen")],
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await openCreateRoute(user);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "chat");
    await user.click(screen.getByRole("button", { name: "Show Primary deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Gemma Ready" }));
    await user.click(screen.getByRole("button", { name: "Add fallback" }));
    await user.click(screen.getByRole("button", { name: "Show Fallback 1 deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Gemma Ready" }));
    await user.click(screen.getByRole("button", { name: "Save route" }));

    expect(route.route.createRoute).not.toHaveBeenCalled();
    const fallback = screen.getByRole("combobox", { name: "Fallback 1 deployments" });
    expect(fallback).toHaveAccessibleDescription("Gemma is already in another group.");
    expect(fallback).toHaveFocus();
    expect(screen.getByRole("combobox", { name: "Primary deployments" })).not.toHaveAttribute(
      "aria-invalid",
    );
  });

  it("does not submit on Enter in a deployment picker", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await openCreateRoute(user);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "chat");
    const picker = screen.getByRole("combobox", { name: "Primary deployments" });
    act(() => picker.focus());
    await user.keyboard("{Enter}");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    await user.type(picker, "Local");
    await user.keyboard("{ArrowDown}{Enter}");

    expect(route.route.createRoute).not.toHaveBeenCalled();
    // Enter still selects the highlighted option, and nothing was validated as a submit attempt.
    expect(screen.getByRole("button", { name: "Remove Local model" })).toBeVisible();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("keeps focus in the edited picker instead of jumping to the next invalid group", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await openCreateRoute(user);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "chat");
    await user.click(screen.getByRole("button", { name: "Add fallback" }));
    await user.click(screen.getByRole("button", { name: "Save route" }));
    const primary = screen.getByRole("combobox", { name: "Primary deployments" });
    const fallback = screen.getByRole("combobox", { name: "Fallback 1 deployments" });
    expect(primary).toHaveFocus();
    expect(fallback).toHaveAttribute("aria-invalid", "true");

    await user.type(primary, "Local");
    await user.keyboard("{ArrowDown}{Enter}");
    expect(screen.getByRole("button", { name: "Remove Local model" })).toBeVisible();
    expect(primary).toHaveFocus();
    expect(fallback).not.toHaveFocus();
    expect(fallback).toHaveAttribute("aria-invalid", "true");
  });

  it("adds, removes, and reorders tiers into the expected fallback groups payload", async () => {
    const user = setupUser();
    const { model, route } = clients(
      [],
      [
        deployment("deployment-a", "Gemma"),
        deployment("deployment-b", "Qwen"),
        deployment("deployment-c", "Mistral"),
      ],
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await openCreateRoute(user);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "chat");
    const pick = async (tier: string, option: string) => {
      await user.click(screen.getByRole("button", { name: `Show ${tier} deployments options` }));
      await user.click(await screen.findByRole("option", { name: `${option} Ready` }));
    };
    await pick("Primary", "Gemma");
    await user.click(screen.getByRole("button", { name: "Add fallback" }));
    await pick("Fallback 1", "Qwen");
    await user.click(screen.getByRole("button", { name: "Add fallback" }));
    await pick("Fallback 2", "Mistral");
    expect(screen.getByRole("button", { name: "Move Primary up" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Move Fallback 2 down" })).toBeDisabled();

    // Drop the middle tier, then lift the remaining fallback into the primary position.
    await user.click(screen.getByRole("button", { name: "Remove Fallback 1" }));
    await user.click(screen.getByRole("button", { name: "Move Fallback 1 up" }));
    const form = screen.getByRole("form", { name: "Create route" });
    expect(tierNames(form)).toEqual(["Primary · single", "Fallback · single"]);
    const [first, second] = within(form).getAllByRole("group", { name: /^(Primary|Fallback)/ });
    expect(within(first).getByRole("button", { name: "Remove Mistral" })).toBeVisible();
    expect(within(second).getByRole("button", { name: "Remove Gemma" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Save route" }));

    expect(route.route.createRoute).toHaveBeenCalledWith({
      name: "chat",
      enabled: true,
      fallbackGroups: [
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-c"] },
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
      ],
    });
  });

  it("caps tiers at eight and keeps the last tier", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await openCreateRoute(user);
    expect(screen.getByRole("button", { name: "Remove Primary" })).toBeDisabled();
    const add = screen.getByRole("button", { name: "Add fallback" });
    for (let count = 1; count < 8; count += 1) await user.click(add);
    expect(tierNames(screen.getByRole("form"))).toEqual([
      "Primary",
      ...Array<string>(7).fill("Fallback"),
    ]);
    expect(add).toBeDisabled();
  });

  it("shows a server error inside the sidebar rather than behind it", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockRejectedValue(
      new ConnectError("Inference route declaration is invalid", Code.InvalidArgument),
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await submitSingleMemberCreate(user, "chat");

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(
      "Route creation was rejected (InvalidArgument): Inference route declaration is invalid",
    );
    expect(screen.getByRole("form", { name: "Create route" })).toContainElement(alert);
    expect(screen.getAllByRole("alert")).toHaveLength(1);
  });

  it("shows the route status badge in the edit header", async () => {
    const { model, route } = clients([routeView({ id: "route-edit", name: "editable" })]);
    renderAt(
      ["/routes?panel=routes%2Froute%3Aroute-edit&panel=routes%2Fedit%3Aroute-edit"],
      route,
      model,
    );

    const form = await screen.findByRole("form", { name: "Edit route" });
    const panel = screen.getByRole("complementary", { name: "editable · Edit" });
    const badge = within(panel).getByText("Available");
    expect(badge).toHaveAttribute("data-variant", "success");
    expect(form).not.toContainElement(badge);
  });

  it("reconciles one exact uncertain Create match without retrying", async () => {
    const user = setupUser();
    const accepted = routeView({
      id: "route-reconciled",
      name: "reconciled",
    });
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockRejectedValue(
      new ConnectError("timed out", Code.Unavailable),
    );
    vi.mocked(route.route.listRoutes)
      .mockResolvedValueOnce(create(ListRoutesResponseSchema))
      .mockResolvedValueOnce(create(ListRoutesResponseSchema, { routes: [accepted] }));
    const router = renderIntegratedAt(["/routes"], route, model);

    await openCreateRoute(user);
    await user.type(screen.getByRole("textbox", { name: "Name" }), " reconciled ");
    await user.click(screen.getByRole("button", { name: "Show Primary deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Local model Ready" }));
    await user.click(screen.getByRole("button", { name: "Save route" }));
    await waitFor(() =>
      expect(router.state.location.search).toBe("?panel=routes%2Froute%3Aroute-reconciled"),
    );
    expect(route.route.createRoute).toHaveBeenCalledOnce();
    expect(route.route.listRoutes).toHaveBeenCalledTimes(2);
    expect(screen.getAllByRole("row", { name: "reconciled" })).toHaveLength(1);
    const panel = await screen.findByRole("complementary", { name: "reconciled" });
    await waitForInspectorFocus(panel, "reconciled");
    expect(within(panel).getByRole("heading", { name: "reconciled" })).toHaveFocus();
  });

  it("retains an uncertain Create draft when reconciliation has no exact match", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockRejectedValue(
      new ConnectError("timed out", Code.Unavailable),
    );
    vi.mocked(route.route.listRoutes).mockResolvedValue(
      create(ListRoutesResponseSchema, { routes: [] }),
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await openCreateRoute(user);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "missing");
    await user.click(screen.getByRole("button", { name: "Show Primary deployments options" }));
    await user.click(await screen.findByRole("option", { name: "Local model Ready" }));
    await user.click(screen.getByRole("button", { name: "Save route" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Route creation outcome is uncertain (Unavailable): timed out",
    );
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("missing");
    expect(route.route.createRoute).toHaveBeenCalledOnce();
    expect(route.route.listRoutes).toHaveBeenCalledTimes(2);
  });

  it("retains the draft when reconciliation finds multiple exact normalized matches", async () => {
    const user = setupUser();
    const accepted = routeView({
      id: "route-reconciled-a",
      name: "reconciled",
    });
    const duplicate = routeView({
      id: "route-reconciled-b",
      name: "reconciled",
    });
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockRejectedValue(
      new ConnectError("timed out", Code.Unavailable),
    );
    vi.mocked(route.route.listRoutes)
      .mockResolvedValueOnce(create(ListRoutesResponseSchema))
      .mockResolvedValueOnce(create(ListRoutesResponseSchema, { routes: [accepted, duplicate] }));
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await submitSingleMemberCreate(user, " reconciled ");

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Route creation outcome is uncertain (Unavailable): timed out",
    );
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue(" reconciled ");
    expect(routeRowNames()).toEqual([]);
    expect(route.route.createRoute).toHaveBeenCalledOnce();
    expect(route.route.listRoutes).toHaveBeenCalledTimes(2);
  });

  it("retains the draft when the same normalized name has a different declaration", async () => {
    const user = setupUser();
    const conflicting = routeView({
      id: "route-conflicting",
      name: "reconciled",
      enabled: false,
    });
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockRejectedValue(
      new ConnectError("timed out", Code.Unavailable),
    );
    vi.mocked(route.route.listRoutes)
      .mockResolvedValueOnce(create(ListRoutesResponseSchema))
      .mockResolvedValueOnce(create(ListRoutesResponseSchema, { routes: [conflicting] }));
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await submitSingleMemberCreate(user, " reconciled ");

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Route creation outcome is uncertain (Unavailable): timed out",
    );
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue(" reconciled ");
    expect(routeRowNames()).toEqual([]);
    expect(route.route.createRoute).toHaveBeenCalledOnce();
    expect(route.route.listRoutes).toHaveBeenCalledTimes(2);
  });

  it("retains the draft when uncertain Create reconciliation fails", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockRejectedValue(
      new ConnectError("timed out", Code.Unavailable),
    );
    vi.mocked(route.route.listRoutes)
      .mockResolvedValueOnce(create(ListRoutesResponseSchema))
      .mockRejectedValueOnce(new ConnectError("refresh failed", Code.Internal));
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await submitSingleMemberCreate(user, "reconciled");

    expect(
      await screen.findByText("Route creation outcome is uncertain (Unavailable): timed out"),
    ).toHaveAttribute("role", "alert");
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("reconciled");
    expect(routeRowNames()).toEqual([]);
    expect(route.route.createRoute).toHaveBeenCalledOnce();
    expect(route.route.listRoutes).toHaveBeenCalledTimes(2);
  });

  it("retains a definitive Create service error without reconciliation", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockRejectedValue(
      new ConnectError("name already exists", Code.AlreadyExists),
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await submitSingleMemberCreate(user, "duplicate");

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Route creation was rejected (AlreadyExists): name already exists",
    );
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("duplicate");
    expect(routeRowNames()).toEqual([]);
    expect(route.route.createRoute).toHaveBeenCalledOnce();
    expect(route.route.listRoutes).toHaveBeenCalledOnce();
  });

  it("retains a malformed successful Create response without reconciliation", async () => {
    const user = setupUser();
    const { model, route } = clients([]);
    vi.mocked(route.route.createRoute).mockResolvedValue(create(CreateRouteResponseSchema));
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    await submitSingleMemberCreate(user, "malformed");

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Route creation response protocol error: missing created route.",
    );
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("malformed");
    expect(routeRowNames()).toEqual([]);
    expect(route.route.createRoute).toHaveBeenCalledOnce();
    expect(route.route.listRoutes).toHaveBeenCalledOnce();
  });

  it("prevents duplicate pending Delete confirmation and dismissal", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-delete", name: "to-delete" })]);
    let resolveDelete!: () => void;
    vi.mocked(route.route.deleteRoute).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveDelete = () => resolve(create(DeleteRouteResponseSchema));
        }),
    );
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    const heading = screen.getByRole("heading", { level: 1, name: "Routes" });
    await user.click(await screen.findByRole("row", { name: "to-delete" }));
    await chooseMoreAction(user, "Delete…");
    const confirmation = screen.getByRole("dialog", { name: "Delete route?" });
    const cancel = within(confirmation).getByRole("button", { name: "Cancel" });
    const confirm = within(confirmation).getByRole("button", { name: "Delete" });
    await user.click(confirm);
    await user.click(confirm);
    await user.keyboard("{Escape}");

    expect(route.route.deleteRoute).toHaveBeenCalledOnce();
    expect(cancel).toBeDisabled();
    expect(confirm).toBeDisabled();
    expect(screen.getByRole("dialog", { name: "Delete route?" })).toBeVisible();
    expect(routeRowNames()).toEqual(["to-delete"]);
    expect(route.route.listRoutes).toHaveBeenCalledOnce();

    resolveDelete();
    await waitFor(() => expect(screen.queryByTestId("route-delete")).not.toBeInTheDocument());
    await waitFor(() => expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument());
    expect(heading).toHaveFocus();
  });

  it("removes a deleted route, closes the inspector, and returns focus to its opener", async () => {
    const user = setupUser();
    const { model, route } = clients([routeView({ id: "route-delete", name: "to-delete" })]);
    render(<RoutesPage client={route} metricsClient={idleMetricsClient} modelClient={model} />);

    const card = await screen.findByRole("row", { name: "to-delete" });
    const heading = screen.getByRole("heading", { level: 1, name: "Routes" });
    await user.click(card);
    await chooseMoreAction(user, "Delete…");
    const confirmation = screen.getByRole("dialog", { name: "Delete route?" });
    await waitFor(() =>
      expect(within(confirmation).getByRole("button", { name: "Cancel" })).toHaveFocus(),
    );
    expect(route.route.deleteRoute).not.toHaveBeenCalled();
    await user.click(within(confirmation).getByRole("button", { name: "Delete" }));
    expect(route.route.deleteRoute).toHaveBeenCalledWith({
      inferenceRouteId: "route-delete",
      expectedGeneration: 1n,
    });
    expect(screen.queryByTestId("route-delete")).not.toBeInTheDocument();
    await waitFor(() => expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument());
    expect(heading).toHaveFocus();
  });
});
