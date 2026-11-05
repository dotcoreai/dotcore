import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import {
  act,
  fireEvent,
  render as rtlRender,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type ReactNode } from "react";
import {
  createMemoryRouter,
  MemoryRouter,
  Outlet,
  RouterProvider,
  useLocation,
} from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ApplicationClient } from "@/api/application-client";
import type { ModelClient } from "@/api/model-client";
import type { SiteClient } from "@/api/site-client";
import { PanelNavigationProvider, usePanelNavigation } from "@/app/console-layout";
import type { ResourcePayload } from "@/app/resource-watch";
import { hardwareSnapshot, listedMachine } from "@/features/memory/machine-test-support";
import {
  devicePool,
  hostPool,
  measured,
  nodeMemory,
  nodeMemoryPayload,
  other,
} from "@/features/memory/node-memory-test-support";
import {
  gaugePoint,
  idleMetricsClient,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import {
  type ApplicationDeployment,
  ApplicationDeploymentSchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  GetApplicationDeploymentResponseSchema,
  ListApplicationDeploymentsResponseSchema,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import { AcceleratorMemoryObservationSchema } from "@/gen/dotcore/deployment/v1/accelerator_memory_pb";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";
import { type ModelDeployment, ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import {
  AcceleratorFactsSetSchema,
  AcceleratorObservationSchema,
  AcceleratorVendor,
  HardwareSnapshotSchema,
  HostFactsSchema,
  HostObservationSchema,
  ObservationUnavailableSchema,
} from "@/gen/dotcore/observation/v1/observation_pb";
import {
  ConfiguredExecutionTargetSchema,
  CurrentHardwareSnapshotAvailabilitySchema,
  ExecutionTargetWorkloadKind,
  ListConfiguredExecutionTargetsResponseSchema,
  ListMachinesResponseSchema,
  MachineHardwareSnapshotSchema,
  MachineSchema,
  NodeObservationFreshnessSchema,
  NodeObservationState,
  type RuntimeTargetAvailabilitySchema,
} from "@/gen/dotcore/site/v1/service_pb";
import { LOADING_MINIMUM_VISIBLE_MS, LOADING_REVEAL_DELAY_MS } from "@/hooks/use-delayed-loading";
import { failedSnapshot, hardwareSnapshotsPayload } from "@/test/fake-resource-watch";
import { expectHeadingOutline } from "@/test/heading-outline";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";

import { MachinesPage, type MachinesSite, tick } from "./machines-page-test-support";

const writeClipboard = vi.fn();
vi.mock("@/lib/clipboard", () => ({
  writeClipboard: (value: string) => writeClipboard(value) as Promise<void>,
}));

afterEach(() => {
  vi.useRealTimers();
});
function render(ui: ReactNode, initialEntries = ["/machines"]) {
  return rtlRender(ui, {
    wrapper: ({ children }) => (
      <MemoryRouter initialEntries={initialEntries}>{children}</MemoryRouter>
    ),
  });
}

function LocationProbe() {
  const location = useLocation();
  return <output data-testid="location">{`${location.pathname}${location.search}`}</output>;
}

type Presentation = "continue" | "enter" | "idle" | "restore";

function PresentationProbe() {
  // Route-owned inspector shells consume this provider behavior seam once the presentation
  // classification is implemented. The cast keeps the red integration tests compilable until then.
  const navigation = usePanelNavigation() as ReturnType<typeof usePanelNavigation> & {
    presentation?: Presentation;
  };
  return <output data-testid="presentation">{navigation.presentation ?? "unclassified"}</output>;
}

const unavailableCode = "CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE";
const unavailableMessage = "A current hardware snapshot is unavailable for this machine.";
const invalidAvailabilityMessage =
  "Machine inventory protocol error: missing or unknown current hardware snapshot availability.";

function availableMachine(machineId: string, displayName: string) {
  return create(MachineSchema, {
    currentHardwareSnapshotAvailability: create(CurrentHardwareSnapshotAvailabilitySchema, {
      availability: { case: "available", value: create(EmptySchema) },
    }),
    displayName,
    machineId,
  });
}

function unavailableMachine(machineId: string, displayName: string) {
  return create(MachineSchema, {
    currentHardwareSnapshotAvailability: create(CurrentHardwareSnapshotAvailabilitySchema, {
      availability: {
        case: "unavailable",
        value: create(ObservationUnavailableSchema, {
          code: unavailableCode,
          message: unavailableMessage,
        }),
      },
    }),
    displayName,
    machineId,
  });
}

function missingAvailabilityMachine(machineId: string, displayName: string) {
  return create(MachineSchema, { displayName, machineId });
}

/** A snapshot response of a machine whose first refresh has not completed. */
function pendingSnapshot() {
  return create(MachineHardwareSnapshotSchema, {
    refresh: { outcome: { case: "pending", value: create(EmptySchema) } },
  });
}

function snapshotFor(hostName = "machine-host") {
  return create(HardwareSnapshotSchema, {
    host: create(HostObservationSchema, {
      result: {
        case: "facts",
        value: create(HostFactsSchema, {
          availableMemoryBytes: 34359738368n,
          cpuCount: 16,
          hostName,
          operatingSystemOrKernel: "Linux 6.12",
          filesystems: [{ fstype: "ext4", mountpoint: "/", sizeBytes: 1099511627776n }],
          totalMemoryBytes: 68719476736n,
        }),
      },
    }),
    accelerators: [
      create(AcceleratorObservationSchema, {
        vendor: AcceleratorVendor.NVIDIA,
        result: {
          case: "facts",
          value: create(AcceleratorFactsSetSchema, {
            facts: [
              {
                deviceId: "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                vendor: AcceleratorVendor.NVIDIA,
                model: "NVIDIA RTX 6000 Ada Generation",
                totalMemoryBytes: 103079215104n,
                usedMemoryBytes: 10737418240n,
                driverOrRuntimeVersion: "570.86.16",
              },
              {
                deviceId: "GPU-11111111-2222-3333-4444-555555555555",
                vendor: AcceleratorVendor.NVIDIA,
                model: "NVIDIA A100 80GB",
                totalMemoryBytes: 85899345920n,
                usedMemoryBytes: 0n,
                driverOrRuntimeVersion: "570.86.16",
              },
            ],
          }),
        },
      }),
    ],
  });
}

/** A site listing `machines`, whose readable machines have a failed first hardware refresh. */
function clientFor(machines: ReturnType<typeof availableMachine>[]): MachinesSite {
  return {
    listConfiguredExecutionTargets: vi.fn(() =>
      Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
    ),
    watched: {
      hardwareSnapshots: () =>
        Promise.resolve(
          hardwareSnapshotsPayload(
            Object.fromEntries(
              machines
                .filter(({ currentHardwareSnapshotAvailability }) => {
                  return currentHardwareSnapshotAvailability?.availability.case === "available";
                })
                .map(({ machineId }) => [machineId, failedSnapshot("not configured")]),
            ),
          ),
        ),
      machines: () => Promise.resolve(create(ListMachinesResponseSchema, { machines })),
    },
  };
}

/** A current snapshot read answering with the facts of `hostName`. */
function readable(hostName?: string) {
  return create(MachineHardwareSnapshotSchema, { snapshot: snapshotFor(hostName) });
}

/** A model client whose deployment list attributes accelerator memory for the inspector. */
function modelClientFor(deployments: ModelDeployment[] = []): ModelClient {
  return {
    catalog: {} as ModelClient["catalog"],
    model: {
      listModelDeployments: vi.fn(() =>
        Promise.resolve(create(ListModelDeploymentsResponseSchema, { deployments })),
      ),
    } as unknown as ModelClient["model"],
  };
}

function renderWithPanelNavigation(
  client: MachinesSite,
  initialEntries: string[],
  initialIndex = initialEntries.length - 1,
) {
  const router = createMemoryRouter(
    [
      {
        path: "/machines",
        element: (
          <PanelNavigationProvider>
            <MachinesPage
              applicationClient={applicationClientFor()}
              client={client}
              modelClient={modelClientFor()}
            />
            <LocationProbe />
            <PresentationProbe />
          </PanelNavigationProvider>
        ),
      },
    ],
    { initialEntries, initialIndex },
  );
  return { router, ...rtlRender(<RouterProvider router={router} />) };
}

/**
 * An application client that lists `deployments` and reads each by ID, for the inspector's
 * deployment reads and a deployment beneath the inspector.
 */
function applicationClientFor(deployments: ApplicationDeployment[] = []): ApplicationClient {
  return {
    application: {
      getApplicationDeployment: vi.fn(
        ({ applicationDeploymentId }: { applicationDeploymentId: string }) =>
          Promise.resolve(
            create(GetApplicationDeploymentResponseSchema, {
              deployment: deployments.find(
                (deployment) => deployment.applicationDeploymentId === applicationDeploymentId,
              ),
            }),
          ),
      ),
      listApplicationDeployments: vi.fn(() =>
        Promise.resolve(create(ListApplicationDeploymentsResponseSchema, { deployments })),
      ),
    },
  } as unknown as ApplicationClient;
}

/** Renders the Machines page beside the deployment pages a machine inspector can be pushed from. */
function renderAboveDeployment(
  initialEntries: string[],
  clients: { applicationClient?: ApplicationClient; modelClient?: ModelClient },
) {
  const router = createMemoryRouter(
    [
      {
        element: (
          <PanelNavigationProvider>
            <Outlet />
          </PanelNavigationProvider>
        ),
        children: [
          {
            path: "/machines",
            element: (
              <MachinesPage
                applicationClient={clients.applicationClient ?? applicationClientFor()}
                client={clientFor([availableMachine("worker", "gb10-host-2")])}
                modelClient={clients.modelClient ?? modelClientFor()}
              />
            ),
          },
          { path: "/apps", element: <p>Applications page</p> },
          { path: "/models", element: <p>Models page</p> },
        ],
      },
    ],
    { initialEntries },
  );
  rtlRender(<RouterProvider router={router} />);
  return router;
}

describe("MachinesPage", () => {
  it("returns to the application deployment it was pushed from", async () => {
    const user = userEvent.setup();
    const deploymentLocation = "/apps?panel=apps%2Fdeployment%3Aapp-deployment-1";
    const router = renderAboveDeployment(
      [
        deploymentLocation,
        "/machines?panel=apps%2Fdeployment%3Aapp-deployment-1&panel=machines%2Fmachine%3Aworker",
      ],
      {
        applicationClient: applicationClientFor([
          create(ApplicationDeploymentSchema, {
            applicationDeploymentId: "app-deployment-1",
            applicationDeploymentName: "Team chat",
          }),
        ]),
      },
    );

    expect(await screen.findByRole("complementary", { name: "gb10-host-2" })).toBeVisible();
    await user.click(await screen.findByRole("button", { name: "Back to Team chat" }));

    await waitFor(() =>
      expect(router.state.location.pathname + router.state.location.search).toBe(
        deploymentLocation,
      ),
    );
    expect(screen.getByText("Applications page")).toBeVisible();
  });

  it("returns to the model deployment it was pushed from", async () => {
    const user = userEvent.setup();
    const router = renderAboveDeployment(
      [
        "/machines?panel=models%2Fdeployment%3Amodel-deployment-1&panel=machines%2Fmachine%3Aworker",
      ],
      {
        modelClient: modelClientFor([
          create(ModelDeploymentSchema, {
            modelDeploymentId: "model-deployment-1",
            modelDeploymentName: "Gemma chat",
          }),
        ]),
      },
    );

    await user.click(await screen.findByRole("button", { name: "Back to Gemma chat" }));

    await waitFor(() =>
      expect(router.state.location.pathname + router.state.location.search).toBe(
        "/models?panel=models%2Fdeployment%3Amodel-deployment-1",
      ),
    );
  });

  it("offers no Back control for a machine opened from its own page", async () => {
    renderAboveDeployment(["/machines?panel=machines%2Fmachine%3Aworker"], {});

    expect(await screen.findByRole("complementary", { name: "gb10-host-2" })).toBeVisible();
    expect(screen.queryByRole("button", { name: /^Back to/ })).toBeNull();
  });

  it("reads h1 page, h2 section with no skipped level", async () => {
    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={clientFor([availableMachine("one", "Machine one"), availableMachine("two", "Two")])}
        modelClient={modelClientFor()}
      />,
    );
    await screen.findByRole("table", { name: "Machine inventory" });

    expect(
      screen.getAllByRole("heading", { level: 2 }).map((heading) => heading.textContent),
    ).toEqual(["Resources", "Inventory"]);
    expectHeadingOutline(document.body, [1, 2, 2]);
  });

  it("delays inventory feedback and holds completed Machines content for the minimum", async () => {
    vi.useFakeTimers();
    let resolveMachines!: (response: ResourcePayload<"machines">) => void;
    const client = clientFor([]);
    client.watched.machines = () =>
      new Promise<ResourcePayload<"machines">>((resolve) => {
        resolveMachines = resolve;
      });

    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        modelClient={modelClientFor()}
      />,
    );

    expect(screen.queryByText("Loading machines…")).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_REVEAL_DELAY_MS);
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading machines…");

    await act(async () => {
      resolveMachines(create(ListMachinesResponseSchema));
      await Promise.resolve();
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading machines…");
    expect(screen.queryByText("No machines are configured for this site.")).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_MINIMUM_VISIBLE_MS);
    });
    expect(screen.queryByText("Loading machines…")).not.toBeInTheDocument();
    expect(screen.getByText("No machines are configured for this site.")).toBeVisible();
  });
  it("lists each machine as an unselected row", async () => {
    const client = clientFor([
      availableMachine("one", "Machine one"),
      availableMachine("two", "Machine two"),
    ]);

    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        modelClient={modelClientFor()}
      />,
    );

    const table = await screen.findByRole("table", { name: "Machine inventory" });
    expect(
      within(table)
        .getAllByRole("row")
        .slice(1)
        .map((row) => [row.getAttribute("aria-label"), row.getAttribute("data-state")]),
    ).toEqual([
      ["Machine one", null],
      ["Machine two", null],
    ]);
    expect(screen.getByText("2 of 2 machines")).toBeVisible();
    expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument();
  });

  it("shows an opened inspector the machine's snapshot from the shared watch", async () => {
    const user = userEvent.setup();
    const client = clientFor([
      availableMachine("one", "Machine one"),
      unavailableMachine("two", "Machine two"),
    ]);
    client.watched.hardwareSnapshots = () =>
      Promise.resolve(hardwareSnapshotsPayload({ one: readable("host-one") }));

    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        metricsClient={idleMetricsClient()}
        modelClient={modelClientFor()}
      />,
    );

    await user.click(await screen.findByRole("row", { name: "Machine one" }));
    expect(await screen.findByText("host-one")).toBeVisible();
  });

  it("shows readings and running counts for a connected machine, dashes for the others", async () => {
    const client = clientFor([
      listedMachine("one", "Machine one"),
      listedMachine("two", "Machine two", NodeObservationState.DISCONNECTED),
      listedMachine("three", "Machine three", NodeObservationState.STALE),
    ]);
    const snapshot = hardwareSnapshot({
      cpuCount: 8,
      devices: [{ deviceId: "gpu0", model: "RTX 5090", vram: { totalGib: 24, usedGib: 6 } }],
      memory: { availableGib: 32, totalGib: 64 },
    });
    client.watched.hardwareSnapshots = () =>
      Promise.resolve(
        hardwareSnapshotsPayload({
          one: create(MachineHardwareSnapshotSchema, { snapshot }),
          three: create(MachineHardwareSnapshotSchema, { snapshot }),
          two: create(MachineHardwareSnapshotSchema, { snapshot }),
        }),
      );
    const placed = (siteNodeId: string) => ({ executionTargetId: "target", siteNodeId });
    const modelDeployment = (id: string, siteNodeId: string) =>
      create(ModelDeploymentSchema, {
        intent: { placement: placed(siteNodeId) },
        modelDeploymentId: id,
      });
    const appDeployment = (id: string, siteNodeId: string) =>
      create(ApplicationDeploymentSchema, {
        applicationDeploymentId: id,
        placement: placed(siteNodeId),
      });

    render(
      <MachinesPage
        applicationClient={applicationClientFor([
          appDeployment("app-1", "one"),
          appDeployment("app-2", "three"),
        ])}
        client={client}
        metricsClient={metricsClientWith(staticWatch(machineAnswer))}
        modelClient={modelClientFor([
          modelDeployment("m-1", "one"),
          modelDeployment("m-2", "one"),
          modelDeployment("m-3", "two"),
        ])}
      />,
    );

    const table = await screen.findByRole("table", { name: "Machine inventory" });
    await waitFor(() => expect(rowTexts(table)[0]).toContain("32 / 64 GiB"));
    expect(rowTexts(table)).toEqual([
      "Machine one | RTX 5090 · 8 CPUs | Connected | 50% | 32 / 64 GiB | 6 / 24 GiB | 2 models · 1 app",
      "Machine three | Hardware unavailable | Stale | — | — | — | 1 app",
      "Machine two | Hardware unavailable | Disconnected | — | — | — | 1 model",
    ]);
  });

  it("filters machines by connection from the URL", async () => {
    const client = clientFor([
      listedMachine("one", "Machine one"),
      listedMachine("two", "Machine two", NodeObservationState.DISCONNECTED),
    ]);

    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        metricsClient={idleMetricsClient()}
        modelClient={modelClientFor()}
      />,
      ["/machines?connection=Disconnected"],
    );

    const table = await screen.findByRole("table", { name: "Machine inventory" });
    expect(
      within(table)
        .getAllByRole("row")
        .slice(1)
        .map((row) => row.getAttribute("aria-label")),
    ).toEqual(["Machine two"]);
    expect(screen.getByText("1 of 2 machines")).toBeVisible();
  });

  it("sorts machines by their connection label, descending from the URL", async () => {
    const client = clientFor([
      listedMachine("one", "Machine one"),
      listedMachine("two", "Machine two", NodeObservationState.DISCONNECTED),
      listedMachine("three", "Machine three", NodeObservationState.STALE),
    ]);

    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        metricsClient={idleMetricsClient()}
        modelClient={modelClientFor()}
      />,
      ["/machines?sort=-connection"],
    );

    const table = await screen.findByRole("table", { name: "Machine inventory" });
    expect(
      within(table)
        .getAllByRole("row")
        .slice(1)
        .map((row) => row.getAttribute("aria-label")),
    ).toEqual(["Machine three", "Machine two", "Machine one"]);
  });

  it.each([
    ["Enter", "{Enter}"],
    ["Space", " "],
    ["a click", undefined],
  ])("opens the machine panel from a row with %s", async (_, key) => {
    const user = userEvent.setup();
    render(
      <>
        <MachinesPage
          applicationClient={applicationClientFor()}
          client={clientFor([availableMachine("one", "Machine one")])}
          metricsClient={idleMetricsClient()}
          modelClient={modelClientFor()}
        />
        <LocationProbe />
      </>,
    );
    const row = await screen.findByRole("row", { name: "Machine one" });

    if (key) {
      row.focus();
      await user.keyboard(key);
    } else {
      await user.click(row);
    }

    expect(await screen.findByRole("complementary", { name: "Machine one" })).toBeVisible();
    expect(screen.getByTestId("location")).toHaveTextContent(
      "/machines?panel=machines%2Fmachine%3Aone",
    );
    expect(row).toHaveAttribute("data-state", "selected");
  });

  it("opens a selected row in the shared inspector with an exact panel URL and selected accent", async () => {
    const client = clientFor([availableMachine("one", "Machine one")]);
    let resolveSnapshot!: (value: ResourcePayload<"hardwareSnapshots">) => void;
    client.watched.hardwareSnapshots = () =>
      new Promise<ResourcePayload<"hardwareSnapshots">>((resolve) => {
        resolveSnapshot = resolve;
      });

    render(
      <>
        <MachinesPage
          applicationClient={applicationClientFor()}
          client={client}
          modelClient={modelClientFor()}
        />
        <LocationProbe />
      </>,
    );
    fireEvent.click(await screen.findByRole("row", { name: "Machine one" }));

    const panel = await screen.findByRole("complementary", { name: "Machine one" });
    expect(panel).toBeVisible();
    expectResourceMarker(inspectorTitle(panel), "one");
    expectResourceMarker(
      Array.from(screen.getByRole("row", { name: "Machine one" }).querySelectorAll("td div")).find(
        (line) => !line.querySelector("div"),
      ),
      "one",
    );
    expect(screen.getByTestId("location")).toHaveTextContent(
      "/machines?panel=machines%2Fmachine%3Aone",
    );
    expect(screen.getByRole("row", { name: "Machine one" })).toHaveAttribute(
      "data-state",
      "selected",
    );
    expect(
      within(screen.getByTestId("inspector-content")).queryByRole("status"),
    ).not.toBeInTheDocument();

    resolveSnapshot(hardwareSnapshotsPayload({ one: readable() }));
    expect(await screen.findByText("machine-host")).toBeVisible();
    // The panel title above is the machine's only identity; the body no longer repeats it.
    expect(
      within(screen.getByTestId("inspector-body")).queryByRole("heading", { name: "Machine one" }),
    ).not.toBeInTheDocument();
  });

  it("restores a direct machine inspector link after inventory resolves", async () => {
    const client = clientFor([availableMachine("one", "Machine one")]);
    client.watched.hardwareSnapshots = () =>
      Promise.resolve(hardwareSnapshotsPayload({ one: readable() }));

    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        modelClient={modelClientFor()}
      />,
      ["/machines?panel=machines%2Fmachine%3Aone"],
    );

    expect(await screen.findByRole("complementary", { name: "Machine one" })).toBeVisible();
    expect(await screen.findByText("machine-host")).toBeVisible();
  });

  it("keeps the shown machine in view until a history-selected machine's body is ready", async () => {
    const client = clientFor([
      availableMachine("one", "Machine one"),
      availableMachine("two", "Machine two"),
    ]);
    // Machine two is listed with its first refresh pending, then gets its snapshot.
    let twoSnapshot = pendingSnapshot();
    client.watched.hardwareSnapshots = () =>
      Promise.resolve(hardwareSnapshotsPayload({ one: readable("host-one"), two: twoSnapshot }));
    const { router } = renderWithPanelNavigation(
      client,
      ["/machines?panel=machines%2Fmachine%3Aone", "/machines?panel=machines%2Fmachine%3Atwo"],
      0,
    );

    expect(await screen.findByText("host-one")).toBeVisible();
    await act(async () => {
      await router.navigate(1);
    });

    // Machine one stays whole, title included, so its snapshot never sits under machine two.
    const held = screen.getByRole("complementary", { name: "Machine one" });
    expect(within(held).getByRole("region", { name: "Hardware" })).toHaveTextContent("host-one");
    expect(screen.queryByRole("complementary", { name: "Machine two" })).not.toBeInTheDocument();

    twoSnapshot = readable("host-two");
    await tick();
    const shown = await screen.findByRole("complementary", { name: "Machine two" });
    expect(within(shown).getByRole("region", { name: "Hardware" })).toHaveTextContent("host-two");
    expect(shown).not.toHaveTextContent("host-one");
  });

  it("shows a newly selected machine's loading once its snapshot outlasts the reveal delay", async () => {
    vi.useFakeTimers();
    const client = clientFor([
      availableMachine("one", "Machine one"),
      availableMachine("two", "Machine two"),
    ]);
    let twoSnapshot = pendingSnapshot();
    client.watched.hardwareSnapshots = () =>
      Promise.resolve(hardwareSnapshotsPayload({ one: readable("host-one"), two: twoSnapshot }));
    const { router } = renderWithPanelNavigation(
      client,
      ["/machines?panel=machines%2Fmachine%3Aone", "/machines?panel=machines%2Fmachine%3Atwo"],
      0,
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(screen.getByText("host-one")).toBeVisible();

    await act(async () => {
      void router.navigate(1);
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(screen.getByTestId("location")).toHaveTextContent("machine%3Atwo");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_REVEAL_DELAY_MS - 1);
    });
    expect(screen.getByRole("complementary", { name: "Machine one" })).toHaveTextContent(
      "host-one",
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    const loading = screen.getByRole("complementary", { name: "Machine two" });
    expect(within(loading).getByRole("status")).toHaveTextContent(
      "Loading current hardware snapshot…",
    );
    expect(loading).not.toHaveTextContent("host-one");

    twoSnapshot = readable("host-two");
    await tick();
    expect(loading).toHaveTextContent("Loading current hardware snapshot…");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_MINIMUM_VISIBLE_MS);
    });
    expect(loading).not.toHaveTextContent("Loading current hardware snapshot…");
    expect(within(loading).getByRole("region", { name: "Hardware" })).toHaveTextContent("host-two");
  });

  it("switches at once to a machine without a readable snapshot", async () => {
    const client = clientFor([
      availableMachine("one", "Machine one"),
      unavailableMachine("two", "Machine two"),
    ]);
    client.watched.hardwareSnapshots = () =>
      Promise.resolve(hardwareSnapshotsPayload({ one: readable("host-one") }));
    const { router } = renderWithPanelNavigation(
      client,
      ["/machines?panel=machines%2Fmachine%3Aone", "/machines?panel=machines%2Fmachine%3Atwo"],
      0,
    );

    expect(await screen.findByText("host-one")).toBeVisible();
    await act(async () => {
      await router.navigate(1);
    });

    const shown = screen.getByRole("complementary", { name: "Machine two" });
    expect(within(shown).getByRole("region", { name: "Hardware" })).toHaveTextContent(
      unavailableCode,
    );
  });

  it("draws a first-opened machine's empty rows at once, and its sections once its reads settle", async () => {
    const client = clientFor([availableMachine("one", "Machine one")]);
    const readSnapshots = vi.fn(() =>
      Promise.resolve(hardwareSnapshotsPayload({ one: readable("host-one") })),
    );
    client.watched.hardwareSnapshots = readSnapshots;
    let resolveTargets!: (
      value: Awaited<ReturnType<SiteClient["listConfiguredExecutionTargets"]>>,
    ) => void;
    client.listConfiguredExecutionTargets = vi.fn(
      () =>
        new Promise<Awaited<ReturnType<SiteClient["listConfiguredExecutionTargets"]>>>(
          (resolve) => (resolveTargets = resolve),
        ),
    );
    renderWithPanelNavigation(client, ["/machines?panel=machines%2Fmachine%3Aone"]);

    const panel = await screen.findByRole("complementary", { name: "Machine one" });
    await waitFor(() => expect(readSnapshots).toHaveBeenCalled());
    await act(async () => {
      await Promise.resolve();
    });
    // The title names the machine at once, with empty rows holding the body's place quietly until
    // the execution targets are read too.
    expect(within(panel).getByRole("region", { name: "Hardware" })).not.toHaveTextContent(
      "host-one",
    );
    expect(within(panel).getByRole("region", { name: "Resources" })).toHaveTextContent("RAM—CPU—");
    expect(within(panel).queryByRole("status")).not.toBeInTheDocument();
    expect(within(panel).queryByRole("region", { name: "Execution targets" })).toBeNull();

    await act(async () => {
      resolveTargets(create(ListConfiguredExecutionTargetsResponseSchema));
      await Promise.resolve();
    });
    expect(within(panel).getByRole("region", { name: "Hardware" })).toHaveTextContent("host-one");
    expect(within(panel).getByRole("region", { name: "Execution targets" })).toBeVisible();
  });

  it("releases a held machine when the inspector closes", async () => {
    const client = clientFor([
      availableMachine("one", "Machine one"),
      availableMachine("two", "Machine two"),
    ]);
    client.watched.hardwareSnapshots = () =>
      Promise.resolve(
        hardwareSnapshotsPayload({ one: readable("host-one"), two: pendingSnapshot() }),
      );
    const { router } = renderWithPanelNavigation(
      client,
      [
        "/machines?panel=machines%2Fmachine%3Aone",
        "/machines",
        "/machines?panel=machines%2Fmachine%3Atwo",
      ],
      0,
    );

    expect(await screen.findByText("host-one")).toBeVisible();
    await act(async () => {
      await router.navigate(1);
    });
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();

    await act(async () => {
      await router.navigate(2);
    });
    // Reopening on another machine shows its title, never the machine shown before the close.
    const panel = screen.getByRole("complementary", { name: "Machine two" });
    expect(panel).not.toHaveTextContent("host-one");
  });

  it("opens a missing machine panel from the URL, then recovers to base content after inventory resolves", async () => {
    const client = clientFor([availableMachine("one", "Machine one")]);

    render(
      <>
        <MachinesPage
          applicationClient={applicationClientFor()}
          client={client}
          modelClient={modelClientFor()}
        />
        <LocationProbe />
      </>,
      ["/machines?panel=machines%2Fmachine%3Amissing"],
    );

    // The URL opens the panel while the inventory is read; the inventory then closes it.
    expect(screen.getByTestId("inspector-content")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent(/^\/machines$/));
    await waitFor(() => expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument());
  });

  it("canonically recovers to a valid machine inspector as an immediate continuation", async () => {
    const client = clientFor([availableMachine("one", "Machine one")]);
    client.watched.hardwareSnapshots = () =>
      new Promise<ResourcePayload<"hardwareSnapshots">>(() => {});

    renderWithPanelNavigation(client, [
      "/machines?panel=machines%2Fmachine%3Aone&panel=machines%2Fmachine%3Amissing",
    ]);

    await waitFor(() =>
      expect(screen.getByTestId("location")).toHaveTextContent(
        "/machines?panel=machines%2Fmachine%3Aone",
      ),
    );
    const destination = await screen.findByRole("complementary", { name: "Machine one" });

    expect(screen.getByTestId("presentation")).toHaveTextContent("continue");
    expect(destination).not.toHaveTextContent("Loading current hardware snapshot…");
    expect(destination.className).not.toMatch(/animate-(in|out)|fade|zoom|slide|translate/);
  });

  it("closes to base content and restores focus to the selected opener", async () => {
    const client = clientFor([availableMachine("one", "Machine one")]);
    client.watched.hardwareSnapshots = () =>
      new Promise<ResourcePayload<"hardwareSnapshots">>(() => {});
    render(
      <>
        <MachinesPage
          applicationClient={applicationClientFor()}
          client={client}
          modelClient={modelClientFor()}
        />
        <LocationProbe />
      </>,
    );
    const opener = await screen.findByRole("row", { name: "Machine one" });
    fireEvent.click(opener);
    fireEvent.click(await screen.findByRole("button", { name: "Close inspector" }));

    await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent("/machines"));
    expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument();
    expect(opener).toHaveFocus();
  });

  it("keeps unavailable and malformed snapshot states inside an explicitly selected inspector", async () => {
    const unavailableClient = clientFor([unavailableMachine("remote", "Remote machine")]);
    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={unavailableClient}
        modelClient={modelClientFor()}
      />,
      ["/machines?panel=machines%2Fmachine%3Aremote"],
    );
    expect(await screen.findByText(unavailableCode)).toBeVisible();
    expect(screen.getByText(unavailableMessage)).toBeVisible();

    const malformedClient = clientFor([missingAvailabilityMachine("missing", "Missing machine")]);
    const { rerender } = render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={malformedClient}
        modelClient={modelClientFor()}
      />,
      ["/machines?panel=machines%2Fmachine%3Amissing"],
    );
    rerender(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={malformedClient}
        modelClient={modelClientFor()}
      />,
    );
    expect(await screen.findByRole("alert")).toHaveTextContent(invalidAvailabilityMessage);
  });

  it("uses the full-screen modal inspector at 720px and below", async () => {
    const width = window.innerWidth;
    Object.defineProperty(window, "innerWidth", { configurable: true, value: 720 });
    const client = clientFor([availableMachine("one", "Machine one")]);
    client.watched.hardwareSnapshots = () =>
      new Promise<ResourcePayload<"hardwareSnapshots">>(() => {});

    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        modelClient={modelClientFor()}
      />,
      ["/machines?panel=machines%2Fmachine%3Aone"],
    );

    expect(await screen.findByRole("dialog", { name: "Machine one" })).toHaveAttribute(
      "aria-modal",
      "true",
    );
    Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
  });

  it("states a failed first inventory read and recovers when the watch reconnects", async () => {
    vi.useFakeTimers();
    const client = clientFor([]);
    client.watched.machines = vi
      .fn()
      .mockRejectedValueOnce(new ConnectError("manager restarting", Code.Unavailable))
      .mockResolvedValue(
        create(ListMachinesResponseSchema, { machines: [availableMachine("one", "Machine one")] }),
      );

    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        modelClient={modelClientFor()}
      />,
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(screen.getByRole("alert")).toHaveTextContent(
      "The machines request failed: the resource watch is unavailable.",
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("row", { name: "Machine one" })).toBeVisible();
  });

  it("streams machines so a worker's connection change reaches its open inspector", async () => {
    vi.useFakeTimers({ now: new Date("2026-09-30T12:00:30Z") });
    const receivedAt = { seconds: 1_790_769_600n };
    const connected = availableMachine("worker", "Worker");
    connected.observationFreshness = create(NodeObservationFreshnessSchema, {
      receivedAt,
      state: NodeObservationState.CONNECTED,
    });
    const stale = unavailableMachine("worker", "Worker");
    stale.observationFreshness = create(NodeObservationFreshnessSchema, {
      receivedAt,
      state: NodeObservationState.STALE,
    });
    const client = clientFor([connected]);
    let listed = connected;
    client.watched.machines = () =>
      Promise.resolve(create(ListMachinesResponseSchema, { machines: [listed] }));
    client.watched.hardwareSnapshots = () =>
      Promise.resolve(hardwareSnapshotsPayload({ worker: readable() }));
    client.listConfiguredExecutionTargets = vi.fn(() =>
      Promise.resolve(
        create(ListConfiguredExecutionTargetsResponseSchema, {
          executionTargets: [
            create(ConfiguredExecutionTargetSchema, {
              availability: {
                connectionState: NodeObservationState.CONNECTED,
                runtimeAvailability: { availability: { case: "available", value: {} } },
              },
              displayName: "NVIDIA GB10",
              executionTargetId: "worker-gpu",
              platformLabel: "linux/arm64",
              siteNodeId: "worker",
              workloadKind: ExecutionTargetWorkloadKind.MODEL,
            }),
          ],
        }),
      ),
    );

    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        modelClient={modelClientFor()}
      />,
      ["/machines?panel=machines%2Fmachine%3Aworker"],
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_REVEAL_DELAY_MS);
    });

    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).getByText("Connected")).toBeVisible();
    expect(within(status).getByText("30 seconds ago")).toBeVisible();
    expect(screen.getByText("machine-host")).toBeVisible();
    const targets = screen.getByRole("region", { name: "Execution targets" });
    expect(within(targets).getByText("NVIDIA GB10")).toBeVisible();

    listed = stale;
    await tick();

    expect(within(screen.getByRole("region", { name: "Status" })).getByText("Stale")).toBeVisible();
    expect(screen.queryByText("machine-host")).not.toBeInTheDocument();
    expect(screen.getByText(unavailableCode)).toBeVisible();
  });

  it("names the site manager's role in its machine inspector", async () => {
    vi.useFakeTimers();
    const manager = availableMachine("manager", "Manager");
    manager.siteManager = true;
    manager.observationFreshness = create(NodeObservationFreshnessSchema, {
      state: NodeObservationState.CONNECTED,
    });
    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={clientFor([manager])}
        modelClient={modelClientFor()}
      />,
      ["/machines?panel=machines%2Fmachine%3Amanager"],
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_REVEAL_DELAY_MS);
    });

    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).getByText("Site manager")).toBeVisible();
    expect(within(status).getByText("Connected")).toBeVisible();
  });

  it("retains exact snapshot facts and attributes memory to models in the inspector", async () => {
    const client = clientFor([availableMachine("one", "Machine one")]);
    client.watched.hardwareSnapshots = () =>
      Promise.resolve(hardwareSnapshotsPayload({ one: readable() }));
    const gib = 1024n ** 3n;
    client.watched.nodeMemory = () =>
      Promise.resolve(
        nodeMemoryPayload([
          nodeMemory("one", [
            devicePool(96n * gib, [measured("deployment-1", 8n * gib), other(2n * gib)], {
              deviceId: "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
              deviceIndex: 0,
              deviceName: "NVIDIA RTX 6000 Ada Generation",
            }),
            devicePool(80n * gib, [], {
              deviceId: "GPU-11111111-2222-3333-4444-555555555555",
              deviceIndex: 1,
              deviceName: "NVIDIA A100 80GB",
            }),
            hostPool(64n * gib, [other(32n * gib)]),
          ]),
        ]),
      );
    const modelClient = modelClientFor([
      create(ModelDeploymentSchema, {
        acceleratorMemory: create(AcceleratorMemoryObservationSchema, {
          result: {
            case: "available",
            value: {
              devices: [
                {
                  deviceId: "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                  managedNodeId: "one",
                  usedMemoryBytes: 8589934592n,
                },
              ],
            },
          },
        }),
        modelDeploymentId: "deployment-1",
        modelDeploymentName: "Qwen3.8 27B",
        snapshot: {
          source: { case: "local", value: { modelDisplayName: "Qwen3.8 27B", modelId: "qwen" } },
        },
        state: {
          observedPlacements: [{ managedNodeId: "one" }],
          status: ModelDeploymentStatus.READY,
        },
      }),
    ]);
    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={client}
        modelClient={modelClient}
      />,
      ["/machines?panel=machines%2Fmachine%3Aone"],
    );

    const inspector = await screen.findByTestId("inspector-content");
    expect(
      await within(inspector).findByText("GPU 0 · NVIDIA RTX 6000 Ada Generation"),
    ).toBeVisible();
    const nvidia = within(inspector).getByRole("region", { name: "Accelerators" });
    expect(within(nvidia).queryByText(/^GPU-/)).not.toBeInTheDocument();
    // Each GPU has its own bar, with the model on the GPU it runs on.
    expect(
      within(nvidia)
        .getAllByRole("img")
        .map((bar) => bar.getAttribute("aria-label")),
    ).toEqual([
      "VRAM on Machine one (NVIDIA RTX 6000 Ada Generation · GPU 0): 10 GiB used of 96 GiB; deployments: Qwen3.8 27B 8 GiB",
      "VRAM on Machine one (NVIDIA A100 80GB · GPU 1): 0 bytes used of 80 GiB",
    ]);

    // Host RAM sits under the display name; VRAM is never summed across GPUs.
    const resources = within(inspector).getByRole("region", { name: "Resources" });
    expect(
      within(resources)
        .getAllByRole("img")
        .map((bar) => bar.getAttribute("aria-label")),
    ).toEqual(["RAM on Machine one: 32 GiB used of 64 GiB"]);
    expect(within(inspector).queryByRole("meter", { name: "CPU" })).not.toBeInTheDocument();
  });
});

describe("MachinesPage add machine", () => {
  function renderPage() {
    render(
      <MachinesPage
        applicationClient={applicationClientFor()}
        client={clientFor([availableMachine("one", "Machine one")])}
        modelClient={modelClientFor()}
      />,
    );
  }

  it("opens the add machine steps and copies the node add command", async () => {
    const user = userEvent.setup();
    writeClipboard.mockResolvedValue(undefined);
    renderPage();

    await user.click(screen.getByRole("button", { name: "Add machine" }));

    const dialog = await screen.findByRole("dialog", { name: "Add a machine" });
    expect(
      within(dialog)
        .getAllByRole("listitem")
        .map((step) => step.textContent),
    ).toEqual([
      "Stop the manager, then run on its machine:dotc node addCopy",
      "On the new machine, run the dotc node join command it prints.",
      "Allow the machine on the manager, then start the manager and the new machine.",
    ]);

    await user.click(within(dialog).getByRole("button", { name: "Copy dotc node add command" }));
    expect(writeClipboard).toHaveBeenCalledExactlyOnceWith("dotc node add");
  });

  it("closes from Close and Escape and returns focus to Add machine", async () => {
    const user = userEvent.setup();
    renderPage();
    const opener = screen.getByRole("button", { name: "Add machine" });

    await user.click(opener);
    await user.click(
      within(await screen.findByRole("dialog", { name: "Add a machine" })).getByRole("button", {
        name: "Close",
      }),
    );
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(opener).toHaveFocus());

    await user.click(opener);
    await screen.findByRole("dialog", { name: "Add a machine" });
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(opener).toHaveFocus());
  });
});

describe("MachinesPage execution target screen", () => {
  it("opens a target row's screen and returns to the machine with the row focused", async () => {
    const user = userEvent.setup();
    const { router } = renderExecutionTargets(["/machines?panel=machines%2Fmachine%3Aone"]);

    const machinePanel = await screen.findByRole("complementary", { name: "Machine one" });
    await user.click(
      await within(machinePanel).findByRole("button", {
        name: "View execution target NVIDIA GeForce RTX 5090 · GPU 0",
      }),
    );

    const targetPanel = await screen.findByRole("complementary", {
      name: "NVIDIA GeForce RTX 5090 · GPU 0",
    });
    expect(currentLocation(router)).toBe(
      "/machines?panel=machines%2Fmachine%3Aone&panel=machines%2Fexecution-target%3Aone-models",
    );
    expect(screen.getByRole("row", { name: "Machine one" })).toHaveAttribute(
      "data-state",
      "selected",
    );
    const facts = within(targetPanel).getByRole("region", { name: "Target" });
    expect([...facts.querySelectorAll("dl > *")].map((row) => row.textContent)).toEqual([
      "Target ID",
      "one-models",
      "Machine",
      "Machine one",
      "Workload kind",
      "Model",
      "Platform",
      "linux/amd64",
      "Configured devices",
      "nvidia.com/gpu=0",
    ]);

    await user.click(within(targetPanel).getByRole("button", { name: "Back to Machine one" }));

    expect(await screen.findByRole("complementary", { name: "Machine one" })).toBeVisible();
    expect(currentLocation(router)).toBe("/machines?panel=machines%2Fmachine%3Aone");
    await waitFor(() =>
      expect(
        screen.getByRole("button", {
          name: "View execution target NVIDIA GeForce RTX 5090 · GPU 0",
        }),
      ).toHaveFocus(),
    );
  });

  it("lists the model deployments placed on a model target", async () => {
    renderExecutionTargets([
      "/machines?panel=machines%2Fmachine%3Aone&panel=machines%2Fexecution-target%3Aone-models",
    ]);

    const models = await screen.findByRole("complementary", {
      name: "NVIDIA GeForce RTX 5090 · GPU 0",
    });
    const modelRunning = await within(models).findByRole("region", { name: "Running" });
    expect(within(modelRunning).getByText("1 deployment")).toBeVisible();
    expect(
      within(
        within(modelRunning).getByRole("list", {
          name: "Running on NVIDIA GeForce RTX 5090 · GPU 0",
        }),
      )
        .getAllByRole("button")
        .map((row) => [row.getAttribute("aria-label"), row.textContent]),
    ).toEqual([["View deployment Qwen chat, Ready", "Qwen chatQwen3 32B · nvidia-gpuReady"]]);
  });

  it("lists the application deployments and degraded runtime of an application target", async () => {
    renderExecutionTargets([
      "/machines?panel=machines%2Fmachine%3Aone&panel=machines%2Fexecution-target%3Aone-apps",
    ]);

    const apps = await screen.findByRole("complementary", { name: "Docker applications" });
    const appRunning = await within(apps).findByRole("region", { name: "Running" });
    expect(
      within(appRunning)
        .getAllByRole("button")
        .map((row) => [row.getAttribute("aria-label"), row.textContent]),
    ).toEqual([["View deployment Team wiki, Ready", "Team wikiWiki · 1.4.0Ready"]]);
    // The Runtime row and its reason appear only because this target's runtime is unavailable.
    const facts = within(apps).getByRole("region", { name: "Target" });
    expect(within(facts).getByText("Runtime")).toBeVisible();
    expect(within(facts).getByText("Unavailable")).toBeVisible();
    expect(within(facts).getByText("Docker daemon is not reachable.")).toBeVisible();
  });

  it("states an empty target and omits the Runtime row for an available target", async () => {
    renderExecutionTargets([
      "/machines?panel=machines%2Fmachine%3Aone&panel=machines%2Fexecution-target%3Aone-spare",
    ]);

    const panel = await screen.findByRole("complementary", {
      name: "NVIDIA GeForce RTX 5090 · GPU 1",
    });
    const running = await within(panel).findByRole("region", { name: "Running" });
    expect(within(running).getByText("0 deployments")).toBeVisible();
    expect(within(running).getByText("Nothing is deployed on this target.")).toBeVisible();
    expect(within(running).queryByRole("list")).not.toBeInTheDocument();
    const facts = within(panel).getByRole("region", { name: "Target" });
    expect(within(facts).getByText("None configured")).toBeVisible();
    expect(within(facts).queryByText("Runtime")).not.toBeInTheDocument();
  });

  it.each([
    [
      "model",
      "one-models",
      "View deployment Qwen chat, Ready",
      "/models?panel=machines%2Fmachine%3Aone&panel=machines%2Fexecution-target%3Aone-models&panel=models%2Fdeployment%3Aqwen-1",
    ],
    [
      "Application",
      "one-apps",
      "View deployment Team wiki, Ready",
      "/apps?panel=machines%2Fmachine%3Aone&panel=machines%2Fexecution-target%3Aone-apps&panel=apps%2Fdeployment%3Awiki-1",
    ],
  ])("opens a running %s deployment's panel above the target", async (_, target, row, href) => {
    const user = userEvent.setup();
    const { router } = renderExecutionTargets([
      `/machines?panel=machines%2Fmachine%3Aone&panel=machines%2Fexecution-target%3A${target}`,
    ]);

    await user.click(await screen.findByRole("button", { name: row }));

    await waitFor(() => expect(currentLocation(router)).toBe(href));
  });

  it.each([
    ["an unknown target", "missing"],
    ["another machine's target", "two-models"],
  ])("recovers %s to its machine once targets are read", async (_, targetId) => {
    const { router } = renderExecutionTargets([
      `/machines?panel=machines%2Fmachine%3Aone&panel=machines%2Fexecution-target%3A${targetId}`,
    ]);

    await waitFor(() =>
      expect(currentLocation(router)).toBe("/machines?panel=machines%2Fmachine%3Aone"),
    );
    expect(await screen.findByRole("complementary", { name: "Machine one" })).toBeVisible();
  });

  it("reads application deployments for the memory overview before any inspector opens", async () => {
    const applicationClient = applicationClientFor();
    renderExecutionTargets(["/machines"], applicationClient);

    await screen.findByRole("row", { name: "Machine one" });

    await waitFor(() =>
      expect(applicationClient.application.listApplicationDeployments).toHaveBeenCalledTimes(1),
    );
  });
});

function currentLocation(router: { state: { location: { pathname: string; search: string } } }) {
  return router.state.location.pathname + router.state.location.search;
}

/** Renders the Machines page with targets on two machines and deployments placed on them. */
function renderExecutionTargets(
  initialEntries: string[],
  applicationClient = applicationClientFor([
    create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "wiki-1",
      applicationDeploymentName: "Team wiki",
      placement: { executionTargetId: "one-apps", siteNodeId: "one" },
      snapshot: { applicationId: "wiki", displayName: "Wiki", version: "1.4.0" },
      state: {
        lifecycleState: ApplicationLifecycleState.READY,
        readinessState: ApplicationReadinessState.READY,
      },
    }),
  ]),
) {
  const client = clientFor([availableMachine("one", "Machine one")]);
  client.watched.hardwareSnapshots = () =>
    Promise.resolve(hardwareSnapshotsPayload({ one: readable() }));
  client.listConfiguredExecutionTargets = vi.fn(() =>
    Promise.resolve(
      create(ListConfiguredExecutionTargetsResponseSchema, {
        executionTargets: [
          target(
            "one",
            "one-models",
            "NVIDIA GeForce RTX 5090 · GPU 0",
            ExecutionTargetWorkloadKind.MODEL,
            ["nvidia.com/gpu=0"],
          ),
          target(
            "one",
            "one-spare",
            "NVIDIA GeForce RTX 5090 · GPU 1",
            ExecutionTargetWorkloadKind.MODEL,
          ),
          target(
            "one",
            "one-apps",
            "Docker applications",
            ExecutionTargetWorkloadKind.APPLICATION,
            [],
            {
              case: "unavailable",
              value: { code: "RUNTIME_UNAVAILABLE", message: "Docker daemon is not reachable." },
            },
          ),
          target("two", "two-models", "Apple M3 Max", ExecutionTargetWorkloadKind.MODEL),
        ],
      }),
    ),
  );
  const modelClient = modelClientFor([
    create(ModelDeploymentSchema, {
      intent: { placement: { executionTargetId: "one-models", siteNodeId: "one" } },
      modelDeploymentId: "qwen-1",
      modelDeploymentName: "Qwen chat",
      snapshot: {
        source: {
          case: "local",
          value: { modelDisplayName: "Qwen3 32B", modelId: "qwen", profileId: "nvidia-gpu" },
        },
      },
      state: { status: ModelDeploymentStatus.READY },
    }),
  ]);
  const router = createMemoryRouter(
    [
      {
        element: (
          <PanelNavigationProvider>
            <Outlet />
          </PanelNavigationProvider>
        ),
        children: [
          {
            path: "/machines",
            element: (
              <MachinesPage
                applicationClient={applicationClient}
                client={client}
                modelClient={modelClient}
              />
            ),
          },
          { path: "/apps", element: <p>Applications page</p> },
          { path: "/models", element: <p>Models page</p> },
        ],
      },
    ],
    { initialEntries },
  );
  return { router, ...rtlRender(<RouterProvider router={router} />) };
}

function target(
  siteNodeId: string,
  executionTargetId: string,
  displayName: string,
  workloadKind: ExecutionTargetWorkloadKind,
  acceleratorDeviceIds: string[] = [],
  runtime: MessageInitShape<typeof RuntimeTargetAvailabilitySchema>["availability"] = {
    case: "available",
    value: {},
  },
) {
  return create(ConfiguredExecutionTargetSchema, {
    acceleratorDeviceIds,
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: { availability: runtime },
    },
    displayName,
    executionTargetId,
    platformLabel: "linux/amd64",
    siteNodeId,
    workloadKind,
  });
}

/** The text of each body row: the name cell's lines joined by " · ", then the other cells. */
function rowTexts(table: HTMLElement): string[] {
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) =>
      Array.from(row.querySelectorAll("td"))
        .map((cell, index) =>
          index === 0
            ? Array.from(cell.querySelectorAll("div"))
                .filter((line) => !line.querySelector("div"))
                .map((line) => line.textContent)
                .join(" | ")
            : cell.textContent,
        )
        .join(" | "),
    );
}

const GiB = 1024 ** 3;

/** Steady readings for every machine, so only the connection decides what a row shows. */
function machineAnswer(query: MessageInitShape<typeof MetricQuerySchema> | undefined) {
  const nodes = ["one", "two", "three"];
  const perNode = (value: number, extra: Record<string, string> = {}) =>
    nodes.map((node) =>
      metricSeries(
        { node, ...extra },
        Array.from({ length: 10 }, (_, step) => gaugePoint(step, value)),
      ),
    );
  const series = (() => {
    switch (query?.metric) {
      case "node.host.cpu.utilization":
        return perNode(0.5);
      case "node.host.memory.total":
        return perNode(64 * GiB);
      case "node.host.memory.available":
        return perNode(32 * GiB);
      case "node.accelerator.memory.total":
        return perNode(24 * GiB, { device: "gpu0" });
      case "node.accelerator.memory.used":
        return perNode(6 * GiB, { device: "gpu0" });
      default:
        return [];
    }
  })();

  return queryResult({ series, stepCount: 10 });
}
