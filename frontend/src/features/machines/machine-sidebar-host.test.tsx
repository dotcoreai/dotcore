import { create } from "@bufbuild/protobuf";
import { act, screen, waitFor, within } from "@testing-library/react";
import { useRef } from "react";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";

import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import { hardwareSnapshot, listedMachine } from "@/features/memory/machine-test-support";
import {
  gib,
  hostPool,
  nodeMemory,
  nodeMemoryPayload,
  other,
} from "@/features/memory/node-memory-test-support";
import {
  metricsClientWith,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { ListApplicationDeploymentsResponseSchema } from "@/gen/dotcore/application/v1/service_pb";
import { ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  ListConfiguredExecutionTargetsResponseSchema,
  ListMachinesResponseSchema,
  MachineHardwareSnapshotSchema,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch, hardwareSnapshotsPayload } from "@/test/fake-resource-watch";
import { renderWithClients } from "@/test/render-with-clients";
import { setupUser } from "@/test/user";

import { MachineSidebarHost } from "./machine-sidebar-host";

vi.mock("uplot", () => ({
  default: class {
    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

describe("MachineSidebarHost", () => {
  it("opens a machine on the overview with its sections and the page range", async () => {
    const user = setupUser();
    const router = renderOnOverview(machinePanel);

    const panel = await screen.findByRole("complementary", { name: "Machine one" });
    await within(panel).findByRole("heading", { name: "Execution targets" });
    expect(
      within(panel)
        .getAllByRole("heading", { level: 2 })
        .map((heading) => heading.textContent),
    ).toEqual([
      "Machine one",
      "Status",
      "Hardware",
      "Resources",
      "Accelerators",
      "Storage",
      "Execution targets",
    ]);

    await user.click(within(panel).getByRole("button", { name: "RAM" }));
    expect(await within(panel).findByRole("radio", { name: "6h" })).toBeChecked();

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/overview"));
    await waitFor(() => expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument());
  });

  it("opens an execution target and its deployment on the overview, and returns to the machine", async () => {
    const user = setupUser();
    const router = renderOnOverview(machinePanel);

    const machine = await screen.findByRole("complementary", { name: "Machine one" });
    await user.click(
      await within(machine).findByRole("button", { name: "View execution target GPU 0" }),
    );
    const target = await screen.findByRole("complementary", { name: "GPU 0" });
    expect(currentLocation(router)).toBe(`/overview?${machinePanel}&${targetPanel}`);

    await user.click(
      await within(target).findByRole("button", { name: "View deployment Qwen chat, Ready" }),
    );
    // The overview hosts deployments too, so the deployment panel stays on it.
    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        `/overview?${machinePanel}&${targetPanel}&panel=models%2Fdeployment%3Aqwen-1`,
      ),
    );
    await act(() => router.navigate(-1));

    await user.click(
      await within(await screen.findByRole("complementary", { name: "GPU 0" })).findByRole(
        "button",
        { name: "Back to Machine one" },
      ),
    );
    expect(await screen.findByRole("complementary", { name: "Machine one" })).toBeVisible();
    expect(currentLocation(router)).toBe(`/overview?${machinePanel}`);
  });
});

const machinePanel = "panel=machines%2Fmachine%3Aone";
const targetPanel = "panel=machines%2Fexecution-target%3Aone-models";

/** Renders only the machine host on the overview, at the overview's 6h range. */
function renderOnOverview(panels: string) {
  const watch = new FakeResourceWatch({
    sources: {
      applicationDeployments: () =>
        Promise.resolve(create(ListApplicationDeploymentsResponseSchema)),
      executionTargets: () =>
        Promise.resolve(
          create(ListConfiguredExecutionTargetsResponseSchema, {
            executionTargets: [
              create(ConfiguredExecutionTargetSchema, {
                availability: {
                  connectionState: NodeObservationState.CONNECTED,
                  runtimeAvailability: { availability: { case: "available", value: {} } },
                },
                displayName: "GPU 0",
                executionTargetId: "one-models",
                platformLabel: "linux/amd64",
                siteNodeId: "one",
                workloadKind: ExecutionTargetWorkloadKind.MODEL,
              }),
            ],
          }),
        ),
      hardwareSnapshots: () =>
        Promise.resolve(
          hardwareSnapshotsPayload({
            one: create(MachineHardwareSnapshotSchema, {
              snapshot: hardwareSnapshot({
                cpuCount: 8,
                filesystemGib: [512],
                memory: { availableGib: 16, totalGib: 32 },
              }),
            }),
          }),
        ),
      machines: () =>
        Promise.resolve(
          create(ListMachinesResponseSchema, { machines: [listedMachine("one", "Machine one")] }),
        ),
      nodeMemory: () =>
        Promise.resolve(
          nodeMemoryPayload([nodeMemory("one", [hostPool(32n * gib, [other(16n * gib)])])]),
        ),
      modelDeployments: () =>
        Promise.resolve(
          create(ListModelDeploymentsResponseSchema, {
            deployments: [
              create(ModelDeploymentSchema, {
                intent: { placement: { executionTargetId: "one-models", siteNodeId: "one" } },
                modelDeploymentId: "qwen-1",
                modelDeploymentName: "Qwen chat",
                snapshot: {
                  source: { case: "local", value: { modelDisplayName: "Qwen3", modelId: "qwen" } },
                },
                state: { status: ModelDeploymentStatus.READY },
              }),
            ],
          }),
        ),
    },
  });
  const router = createMemoryRouter(
    [
      {
        path: "/overview",
        element: (
          <ResourceWatchProvider client={watch.client()}>
            <PanelNavigationProvider>
              <OverviewMachineHost />
            </PanelNavigationProvider>
          </ResourceWatchProvider>
        ),
      },
    ],
    { initialEntries: ["/overview", `/overview?${panels}`] },
  );
  renderWithClients(<RouterProvider router={router} />, { metricsClient });

  return router;
}

function OverviewMachineHost() {
  const openerRef = useRef<HTMLElement>(null);

  return <MachineSidebarHost initialRange="6h" openerRef={openerRef} />;
}

/** A store serving 15m and 6h ranges with no samples. */
const metricsClient = metricsClientWith(
  staticWatch(() => queryResult({ series: [], stepCount: 1 })),
);

function currentLocation(router: { state: { location: { pathname: string; search: string } } }) {
  return router.state.location.pathname + router.state.location.search;
}
