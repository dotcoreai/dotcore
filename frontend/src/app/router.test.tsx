import { create } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, matchRoutes, RouterProvider } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";

import type { ApplicationClient } from "@/api/application-client";
import type { MetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import type { RouteClient } from "@/api/route-client";
import type { SiteClient } from "@/api/site-client";
import { createRoutes } from "@/app/routes";
import { nodeMemoryPayload } from "@/features/memory/node-memory-test-support";
import {
  metricsClientWith,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import {
  catalogModel,
  localDeployment,
  modelTarget,
  readableMachine,
} from "@/features/models/models-section-test-support";
import { ApplicationDeploymentSchema } from "@/gen/dotcore/application/v1/deployment_pb";
import {
  CreateApplicationDeploymentResponseSchema,
  GetApplicationDeploymentResponseSchema,
  ListApplicationDeploymentsResponseSchema,
  RemoveApplicationDeploymentResponseSchema,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  ListEngineReleasesResponseSchema,
  ListHardwareClassesResponseSchema,
  ListModelsResponseSchema,
  ListUpstreamModelTemplatesResponseSchema,
} from "@/gen/dotcore/catalog/v1/service_pb";
import { ListApplicationsResponseSchema } from "@/gen/dotcore/catalog/v1/service_pb";
import {
  FallbackGroupSchema,
  FallbackGroupSelection,
  InferenceRouteSchema,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import { ModelDeploymentIntentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import {
  HardwareSnapshotSchema,
  HostFactsSchema,
  HostObservationSchema,
  ObservationUnavailableSchema,
} from "@/gen/dotcore/observation/v1/observation_pb";
import {
  CurrentHardwareSnapshotAvailabilitySchema,
  ListConfiguredExecutionTargetsResponseSchema,
  ListHardwareSnapshotsResponseSchema,
  ListMachinesResponseSchema,
  MachineHardwareSnapshotSchema,
  MachineSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import type { ResourcePayload } from "@/resource-watch/resource-watch";
import {
  FakeResourceWatch,
  hardwareSnapshotsPayload,
  type ResourceSources,
} from "@/test/fake-resource-watch";
import { ThemeProvider } from "@/theme/theme-provider";

const deterministicModelClient: ModelClient = {
  catalog: {
    listEngineReleases: () => Promise.resolve(create(ListEngineReleasesResponseSchema)),
    listHardwareClasses: () => Promise.resolve(create(ListHardwareClassesResponseSchema)),
    listModels: () => Promise.resolve(create(ListModelsResponseSchema)),
    listUpstreamModelTemplates: () =>
      Promise.resolve(create(ListUpstreamModelTemplatesResponseSchema)),
  },
  model: {
    listModelDeployments: () => Promise.resolve(create(ListModelDeploymentsResponseSchema)),
    createModelDeployment: () => Promise.reject(new Error("load not configured")),
    removeModelDeployment: () => Promise.reject(new Error("unload not configured")),
    updateModelDeployment: () => Promise.reject(new Error("update not configured")),
    updateModelDeploymentMetadata: () => Promise.reject(new Error("metadata not configured")),
    pauseModel: () => Promise.reject(new Error("pause not configured")),
    resumeModel: () => Promise.reject(new Error("resume not configured")),
  },
};

const deterministicRouteClient: RouteClient = {
  route: {
    listRoutes: () => Promise.resolve(create(ListRoutesResponseSchema)),
    getRoute: vi.fn(),
    createRoute: vi.fn(),
    replaceRoute: vi.fn(),
    deleteRoute: vi.fn(),
  },
};

const deterministicSiteClient: SiteClient = {
  listConfiguredExecutionTargets: () =>
    Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });

  return { promise, resolve };
}

function availableMachine(machineId: string, displayName: string) {
  return create(MachineSchema, {
    currentHardwareSnapshotAvailability: create(CurrentHardwareSnapshotAvailabilitySchema, {
      availability: { case: "available", value: create(EmptySchema) },
    }),
    displayName,
    machineId,
  });
}

const deterministicApplicationClient: ApplicationClient = {
  catalog: {
    listApplications: () => Promise.resolve(create(ListApplicationsResponseSchema)),
  },
  application: {
    listApplicationDeployments: () =>
      Promise.resolve(create(ListApplicationDeploymentsResponseSchema)),
    getApplicationDeployment: () => Promise.resolve(create(GetApplicationDeploymentResponseSchema)),
    createApplicationDeployment: () =>
      Promise.resolve(create(CreateApplicationDeploymentResponseSchema)),
    removeApplicationDeployment: () =>
      Promise.resolve(create(RemoveApplicationDeploymentResponseSchema)),
    updateApplicationDeploymentMetadata: () => Promise.reject(new Error("metadata not configured")),
    pauseDeployment: () => Promise.reject(new Error("pause not configured")),
    resumeDeployment: () => Promise.reject(new Error("resume not configured")),
  },
  site: {
    listConfiguredExecutionTargets: () =>
      Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
  },
};

function renderAt(
  initialEntry: string,
  applicationClient?: ApplicationClient,
  siteClient: SiteClient = deterministicSiteClient,
  routeClient: RouteClient = deterministicRouteClient,
  modelClient: ModelClient = deterministicModelClient,
  watched: Pick<ResourceSources, "hardwareSnapshots" | "machines"> = {},
  metricsClient?: MetricsClient,
) {
  const apps = applicationClient ?? deterministicApplicationClient;
  const watch = new FakeResourceWatch({
    sources: {
      applicationDeployments: () => apps.application.listApplicationDeployments({}),
      executionTargets: () => siteClient.listConfiguredExecutionTargets({}),
      hardwareSnapshots:
        watched.hardwareSnapshots ??
        (() => Promise.resolve(create(ListHardwareSnapshotsResponseSchema))),
      machines: watched.machines ?? (() => Promise.resolve(create(ListMachinesResponseSchema))),
      modelDeployments: () => modelClient.model.listModelDeployments({}),
      nodeMemory: () => Promise.resolve(nodeMemoryPayload([])),
      routes: () => routeClient.route.listRoutes({}),
    },
  });
  const router = createMemoryRouter(
    createRoutes({
      applicationClient,
      metricsClient,
      modelClient,
      resourceWatchClient: watch.client(),
      routeClient,
      siteClient,
    }),
    { initialEntries: [initialEntry] },
  );

  render(
    <ThemeProvider>
      <RouterProvider router={router} />
    </ThemeProvider>,
  );

  return { router, watch };
}
describe("application router", () => {
  it("renders the models route with the console shell, active navigation, and empty state", async () => {
    renderAt("/models");

    expect(screen.getByRole("banner")).toBeInTheDocument();
    expect(screen.getByRole("main")).toBeInTheDocument();

    const header = screen.getByRole("banner");
    expect(header).toHaveTextContent("dotcore");

    expect(screen.getByRole("button", { name: "Settings" })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    expect(screen.getByRole("link", { name: "Overview" })).toHaveAttribute("href", "/overview");

    const machinesLink = screen.getByRole("link", { name: "Machines" });
    const modelsChildLink = screen.getByRole("link", { name: "Models" });
    expect(machinesLink).toHaveAttribute("href", "/machines");
    expect(modelsChildLink).toHaveAttribute("href", "/models");
    expect(modelsChildLink).toHaveAttribute("aria-current", "page");
    expect(machinesLink).not.toHaveAttribute("aria-current");

    expect(screen.getByRole("button", { name: "Open profile menu" })).toHaveTextContent("LU");

    const h1 = await screen.findByRole("heading", { level: 1, name: "Models" });
    expect(screen.getByRole("main")).toContainElement(h1);
    expectPageHeader(h1, modelsChildLink, "lucide-boxes");
    expect(await screen.findByText("No models running")).toBeVisible();
    expect(
      screen.getAllByRole("heading", { level: 2 }).map((heading) => heading.textContent),
    ).toEqual(["Inference", "Running"]);
    expect(screen.getByRole("heading", { level: 2, name: "Running" })).toHaveAttribute(
      "data-slot",
      "page-section-heading",
    );
    expect(screen.queryByRole("list", { name: "Catalog models" })).not.toBeInTheDocument();
  });

  it("renders the Catalog route in the Models section with its own navigation entry", async () => {
    renderAt("/models/catalog");

    const catalogLink = screen.getByRole("link", { name: "Catalog" });
    expect(catalogLink).toHaveAttribute("href", "/models/catalog");
    expect(catalogLink).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("link", { name: "Models" })).not.toHaveAttribute("aria-current");

    const h1 = await screen.findByRole("heading", { level: 1, name: "Catalog" });
    expect(screen.getByRole("main")).toContainElement(h1);
    expectPageHeader(h1, catalogLink, "lucide-library");
    expect(await screen.findByText("No models are available to deploy.")).toBeVisible();
    expect(screen.queryByRole("region", { name: "Memory" })).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { level: 2, name: "Running" })).not.toBeInTheDocument();
  });

  it.each([
    ["/models", "Models"],
    ["/models/catalog", "Catalog"],
    ["/routes", "Routes"],
    ["/apps", "Apps"],
    ["/apps/catalog", "Catalog"],
  ])("marks exactly one navigation entry current on %s", (location, label) => {
    renderAt(location);

    expect(
      screen
        .getAllByRole("link")
        .filter((link) => link.getAttribute("aria-current") === "page")
        .map((link) => link.textContent),
    ).toEqual([label]);
  });

  it("renders Preferences with no current navigation entry and an Appearance dark mode switch", async () => {
    renderAt("/preferences");

    const h1 = await screen.findByRole("heading", { level: 1, name: "Preferences" });
    expect(screen.getByRole("main")).toContainElement(h1);
    expect(h1.querySelector("svg")).toHaveClass("lucide-sliders-horizontal");
    expect(screen.getByRole("heading", { level: 2, name: "Appearance" })).toBeVisible();
    expect(screen.getByRole("switch", { name: "Dark mode" })).toBeVisible();
    expect(
      screen.getAllByRole("link").filter((link) => link.getAttribute("aria-current") === "page"),
    ).toEqual([]);
  });

  it("shows the simulated latency indicator in the header while the developer setting is on", async () => {
    const user = userEvent.setup();
    window.localStorage.clear();
    renderAt("/preferences");

    const latencySwitch = await screen.findByRole("switch", { name: "Simulate API latency" });
    expect(screen.queryByRole("link", { name: /Simulated latency/ })).not.toBeInTheDocument();

    await user.click(latencySwitch);
    expect(
      screen.getByRole("link", { name: "Simulated latency 500–2000 ms. Open preferences" }),
    ).toHaveAttribute("href", "/preferences");

    const maximum = screen.getByRole("spinbutton", { name: "Maximum delay (ms)" });
    await user.clear(maximum);
    await user.type(maximum, "3000");
    await user.tab();
    expect(
      screen.getByRole("link", { name: "Simulated latency 500–3000 ms. Open preferences" }),
    ).toBeVisible();

    await user.click(latencySwitch);
    expect(screen.queryByRole("link", { name: /Simulated latency/ })).not.toBeInTheDocument();
    window.localStorage.clear();
  });

  it("opens Preferences from the profile menu and keeps its dark mode switch in sync", async () => {
    const user = userEvent.setup();
    const { router } = renderAt("/machines");

    await user.click(screen.getByRole("button", { name: "Open profile menu" }));
    await user.click(screen.getByRole("menuitem", { name: "Preferences" }));

    await screen.findByRole("heading", { level: 1, name: "Preferences" });
    expect(router.state.location.pathname).toBe("/preferences");

    const darkModeSwitch = screen.getByRole("switch", { name: "Dark mode" });
    const before = darkModeSwitch.getAttribute("aria-checked");
    await user.click(darkModeSwitch);
    expect(darkModeSwitch).toHaveAttribute("aria-checked", before === "true" ? "false" : "true");

    await user.click(screen.getByRole("button", { name: "Open profile menu" }));
    expect(screen.getByRole("menuitemcheckbox", { name: "Dark mode" })).toHaveAttribute(
      "aria-checked",
      before === "true" ? "false" : "true",
    );
  });

  it("does not register the unsupported route-management pathname", () => {
    const routes = createRoutes();
    expect(matchRoutes(routes, "/models/router")).toBeNull();
  });

  it("routes the Routes destination to route management", async () => {
    renderAt("/routes");

    expect(screen.getByRole("banner")).toBeInTheDocument();

    const routesLink = screen.getByRole("link", { name: "Routes" });
    expect(routesLink).toHaveAttribute("href", "/routes");
    expect(routesLink).toHaveAttribute("aria-current", "page");
    expect(routesLink).toHaveAttribute("data-active", "");
    const modelsGroup = screen.getByRole("button", { name: "Models" });
    expect(modelsGroup).toHaveAttribute("aria-expanded", "false");
    expect(modelsGroup).not.toHaveAttribute("data-active");

    const h1 = await screen.findByRole("heading", { level: 1, name: "Routes" });
    expect(screen.getByRole("main")).toContainElement(h1);
    expectPageHeader(h1, routesLink, "lucide-route");
    expect(screen.getByRole("heading", { level: 2, name: "Routes" })).toHaveAttribute(
      "data-slot",
      "page-section-heading",
    );
    expect(screen.queryByText("This page is not yet built.")).not.toBeInTheDocument();
    expect(await screen.findAllByRole("button", { name: "Create route" })).toHaveLength(2);
  });

  it("integrates Routes route management with the configured clients and inspector", async () => {
    const user = userEvent.setup();
    const routeClient: RouteClient = {
      route: {
        ...deterministicRouteClient.route,
        listRoutes: vi.fn(() =>
          Promise.resolve(
            create(ListRoutesResponseSchema, {
              routes: [
                create(InferenceRouteViewSchema, {
                  route: create(InferenceRouteSchema, {
                    inferenceRouteId: "router-route",
                    name: "production",
                    enabled: true,
                    fallbackGroups: [
                      create(FallbackGroupSchema, {
                        selection: FallbackGroupSelection.SINGLE,
                        modelDeploymentIds: ["deployment-a"],
                      }),
                    ],
                    generation: 3n,
                  }),
                }),
              ],
            }),
          ),
        ),
      },
    };
    renderAt("/routes", undefined, deterministicSiteClient, routeClient);

    expect(await screen.findByRole("row", { name: "production" })).toBeVisible();
    expect(routeClient.route.listRoutes).toHaveBeenCalledWith({});
    await user.click(screen.getByRole("row", { name: "production" }));
    const inspector = screen.getByTestId("route-inspector");
    expect(screen.getByTestId("inspector-content")).toContainElement(inspector);
    expect(inspector).not.toHaveTextContent("production");
    expect(screen.getByRole("button", { name: "Connect" })).toBeVisible();
    expect(screen.getByRole("button", { name: "More actions" })).toBeVisible();
  });

  it("names a Routes panel's application deployment parent and returns to it", async () => {
    const user = userEvent.setup();
    const deployment = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "app-deployment-1",
      applicationDeploymentName: "Team chat",
    });
    const getApplicationDeployment = vi.fn(() =>
      Promise.resolve(create(GetApplicationDeploymentResponseSchema, { deployment })),
    );
    const applicationClient: ApplicationClient = {
      ...deterministicApplicationClient,
      application: {
        ...deterministicApplicationClient.application,
        getApplicationDeployment,
        // The Apps page keeps a deployment panel only for a deployment it lists.
        listApplicationDeployments: () =>
          Promise.resolve(
            create(ListApplicationDeploymentsResponseSchema, { deployments: [deployment] }),
          ),
      },
    };
    const routeClient: RouteClient = {
      route: {
        ...deterministicRouteClient.route,
        listRoutes: () =>
          Promise.resolve(
            create(ListRoutesResponseSchema, {
              routes: [
                create(InferenceRouteViewSchema, {
                  route: create(InferenceRouteSchema, {
                    inferenceRouteId: "route-a",
                    name: "selected",
                    enabled: true,
                  }),
                }),
              ],
            }),
          ),
      },
    };
    const { router } = renderAt(
      "/routes?panel=apps%2Fdeployment%3Aapp-deployment-1&panel=routes%2Froute%3Aroute-a",
      applicationClient,
      deterministicSiteClient,
      routeClient,
    );

    await user.click(await screen.findByRole("button", { name: "Back to Team chat" }));

    await waitFor(() =>
      expect(router.state.location.pathname + router.state.location.search).toBe(
        "/apps?panel=apps%2Fdeployment%3Aapp-deployment-1",
      ),
    );
    expect(getApplicationDeployment).toHaveBeenCalledWith(
      { applicationDeploymentId: "app-deployment-1" },
      expect.anything(),
    );
  });

  it("renders the Apps route with active navigation and the installed empty state", async () => {
    renderAt("/apps", deterministicApplicationClient);

    expect(screen.getByRole("link", { name: "Apps" })).toHaveAttribute("aria-current", "page");
    const h1 = await screen.findByRole("heading", { level: 1, name: "Apps" });
    expectPageHeader(h1, screen.getByRole("link", { name: "Apps" }), "lucide-layout-grid");
    expect(screen.getByText("No applications deployed")).toBeVisible();
    expect(
      screen.queryByText("No applications are available in the catalog."),
    ).not.toBeInTheDocument();
  });

  it("renders the Apps Catalog route in the Apps section with its own navigation entry", async () => {
    renderAt("/apps/catalog", deterministicApplicationClient);

    const catalogLink = screen.getByRole("link", { name: "Catalog" });
    expect(catalogLink).toHaveAttribute("href", "/apps/catalog");
    expect(catalogLink).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("link", { name: "Apps" })).not.toHaveAttribute("aria-current");

    const h1 = await screen.findByRole("heading", { level: 1, name: "Catalog" });
    expect(screen.getByRole("main")).toContainElement(h1);
    expectPageHeader(h1, catalogLink, "lucide-library");
    expect(await screen.findByText("No applications are available in the catalog.")).toBeVisible();
    expect(screen.queryByText("No applications deployed")).not.toBeInTheDocument();
  });

  it("redirects the root route to Overview", async () => {
    const { router } = renderAt("/");

    const h1 = await screen.findByRole("heading", { level: 1, name: "Overview" });
    expect(router.state.location.pathname).toBe("/overview");
    const overviewLink = screen.getByRole("link", { name: "Overview" });
    expect(overviewLink).toHaveAttribute("aria-current", "page");
    expectPageHeader(h1, overviewLink, "lucide-layout-dashboard");
  });

  it("reads the overview's metrics through the client the routes were created with", async () => {
    const watchMetrics = vi.fn(staticWatch(() => queryResult({ series: [], stepCount: 1 })));
    renderAt(
      "/overview",
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      metricsClientWith(watchMetrics),
    );

    // The page stream and the machines' "now" stream.
    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(2));
    expect(
      watchMetrics.mock.calls.flatMap(([request]) =>
        (request.queries ?? []).map((entry) => entry.query?.metric),
      ),
    ).toContain("router.requests");
  });

  it("renders the Machines route with active navigation and configured-machine empty state", async () => {
    const user = userEvent.setup();
    renderAt("/machines");

    expect(screen.getByRole("link", { name: "Machines" })).toHaveAttribute("aria-current", "page");
    const h1 = await screen.findByRole("heading", { level: 1, name: "Machines" });
    expectPageHeader(h1, screen.getByRole("link", { name: "Machines" }), "lucide-server");
    expect(await screen.findByText("No machines are configured for this site.")).toBeVisible();

    const modelsGroup = screen.getByRole("button", { name: "Models" });
    expect(modelsGroup).toHaveAttribute("aria-expanded", "false");
    expect(modelsGroup).not.toHaveAttribute("data-active");
    await user.click(modelsGroup);
    await user.click(screen.getByRole("link", { name: "Models" }));

    expect(await screen.findByRole("heading", { level: 1, name: "Models" })).toBeVisible();
    expect(screen.getByRole("link", { name: "Models" })).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("link", { name: "Machines" })).not.toHaveAttribute("aria-current");
  });

  it("renders exact machine inventory before the selected snapshot resolves, then shows its detail", async () => {
    const pendingSnapshot = deferred<ResourcePayload<"hardwareSnapshots">>();
    const machines = [
      create(MachineSchema, {
        currentHardwareSnapshotAvailability: create(CurrentHardwareSnapshotAvailabilitySchema, {
          availability: {
            case: "unavailable",
            value: create(ObservationUnavailableSchema, {
              code: "CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE",
              message: "A current hardware snapshot is unavailable for this machine.",
            }),
          },
        }),
        displayName: "Remote machine",
        machineId: "remote",
      }),
      availableMachine("local", "Local machine"),
    ];
    const siteClient: SiteClient = {
      listConfiguredExecutionTargets: vi.fn(() =>
        Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
      ),
    };

    renderAt(
      "/machines",
      deterministicApplicationClient,
      siteClient,
      deterministicRouteClient,
      deterministicModelClient,
      {
        hardwareSnapshots: () => pendingSnapshot.promise,
        machines: () => Promise.resolve(create(ListMachinesResponseSchema, { machines })),
      },
    );

    const inventory = await screen.findByRole("table", { name: "Machine inventory" });
    expect(within(inventory).getAllByRole("row")).toHaveLength(3);

    fireEvent.click(within(inventory).getByRole("row", { name: "Local machine" }));
    expect(
      await screen.findByRole("complementary", { name: "Local machine" }),
    ).not.toHaveTextContent("Loading current hardware snapshot…");

    await act(async () => {
      pendingSnapshot.resolve(
        hardwareSnapshotsPayload({
          local: create(MachineHardwareSnapshotSchema, {
            snapshot: create(HardwareSnapshotSchema, {
              host: create(HostObservationSchema, {
                result: {
                  case: "facts",
                  value: create(HostFactsSchema, {
                    availableMemoryBytes: 34359738368n,
                    cpuCount: 16,
                    hostName: "route-host",
                    operatingSystemOrKernel: "Linux 6.12",
                    filesystems: [{ fstype: "ext4", mountpoint: "/", sizeBytes: 1099511627776n }],
                    totalMemoryBytes: 68719476736n,
                  }),
                },
              }),
            }),
          }),
        }),
      );
      await Promise.resolve();
    });

    const inspector = screen.getByRole("complementary", { name: "Local machine" });
    await waitFor(() => expect(inspector).toHaveTextContent("route-host"));
    expect(inspector).toHaveTextContent("Linux 6.12");
    expect(inspector).not.toHaveTextContent("Loading current hardware snapshot…");
  });

  it("keeps a panel open as it moves between pages, named from the lists the pages share", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const deployment = localDeployment("deployment-a", gemma, {
      status: ModelDeploymentStatus.READY,
    });
    deployment.intent = create(ModelDeploymentIntentSchema, {
      placement: { executionTargetId: "spark-gpu", siteNodeId: "spark-01" },
    });
    const siteClient: SiteClient = {
      ...deterministicSiteClient,
      listConfiguredExecutionTargets: () =>
        Promise.resolve(
          create(ListConfiguredExecutionTargetsResponseSchema, {
            executionTargets: [modelTarget("spark-01", "spark-gpu", "NVIDIA GB10", "Spark one")],
          }),
        ),
    };
    const modelClient: ModelClient = {
      ...deterministicModelClient,
      catalog: {
        ...deterministicModelClient.catalog,
        listModels: () => Promise.resolve(create(ListModelsResponseSchema, { models: [gemma] })),
      },
      model: {
        ...deterministicModelClient.model,
        listModelDeployments: () =>
          Promise.resolve(
            create(ListModelDeploymentsResponseSchema, { deployments: [deployment] }),
          ),
      },
    };
    const { router, watch } = renderAt(
      "/models?panel=models%2Fdeployment%3Adeployment-a",
      deterministicApplicationClient,
      siteClient,
      deterministicRouteClient,
      modelClient,
      {
        machines: () =>
          Promise.resolve(
            create(ListMachinesResponseSchema, {
              machines: [readableMachine("spark-01", "Spark one")],
            }),
          ),
      },
    );

    const placement = await screen.findByRole("link", { name: "Spark one / NVIDIA GB10" });
    fireEvent.click(placement);

    // The machine's page opens the panel from the URL, named from the machines the pages share.
    await waitFor(() => expect(router.state.location.pathname).toBe("/machines"));
    expect(screen.getByRole("complementary", { name: "Spark one" })).toBeVisible();
    expect(await screen.findByRole("region", { name: "Execution targets" })).toBeVisible();
    // The machines page watches the lists its panels and overview show, the node memory its
    // memory rows draw, plus the routes the console's marker colors are assigned from.
    await waitFor(() =>
      expect(watch.watchedKinds().sort()).toEqual([
        "applicationDeployments",
        "executionTargets",
        "hardwareSnapshots",
        "machines",
        "modelDeployments",
        "nodeMemory",
        "routes",
      ]),
    );

    await act(async () => {
      await router.navigate(-1);
    });

    // Back to the deployment keeps the panel, named and filled from the shared model deployments.
    expect(router.state.location.pathname).toBe("/models");
    expect(await screen.findByRole("region", { name: "Deployment" })).toBeVisible();
  });
});

/**
 * Asserts a visible page heading inside the shared page header, led by the same decorative icon as
 * its sidebar entry.
 */
function expectPageHeader(heading: HTMLElement, sidebarEntry: HTMLElement, iconClass: string) {
  expect(heading).toBeVisible();
  expect(heading).not.toHaveClass("sr-only");
  expect(heading.closest('[data-slot="page-header"]')).not.toBeNull();

  const icon = heading.querySelector("svg");
  const sidebarIcon = sidebarEntry.querySelector("svg");
  expect(icon).toHaveAttribute("aria-hidden", "true");
  expect(lucideClasses(icon)).toContain(iconClass);
  expect(lucideClasses(icon)).toEqual(lucideClasses(sidebarIcon));
}

function lucideClasses(element: Element | null): string[] {
  return Array.from(element?.classList ?? []).filter((name) => name.startsWith("lucide-"));
}
