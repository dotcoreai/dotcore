import { create } from "@bufbuild/protobuf";
import { cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent, { type UserEvent } from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import type { DeploymentLogOperations } from "@/api/deployment-log-client";
import { hardwareSnapshot, listedMachine } from "@/features/memory/machine-test-support";
import {
  catalogModel,
  currentLocation,
  localDeployment,
  renderModelsSection,
  upstreamDeployment,
  upstreamTemplate,
} from "@/features/models/models-section-test-support";
import {
  FallbackGroupSelection,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import { inspectorActionList } from "@/test/inspector-actions";

import { OverviewPage } from "./overview-page";

vi.mock("uplot", () => ({
  default: class {
    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const gemma = catalogModel("gemma", "Gemma", ["default"]);
const localPanel = "panel=models%2Fdeployment%3Adeployment-a";
const upstreamPanel = "panel=models%2Fdeployment%3Aupstream-new";

describe("OverviewSidebar deployments", () => {
  it("opens a local deployment with the sections and actions of the models page", async () => {
    const user = userEvent.setup();
    const deployments = [
      localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
    ];

    const onModels = await sidebarShape(user, deployments, `/models?${localPanel}`, "Gemma");
    cleanup();
    const onOverview = await sidebarShape(user, deployments, `/overview?${localPanel}`, "Gemma");

    expect(onOverview).toEqual(onModels);
    expect(onOverview.sections).toEqual(["Status", "Configuration", "Deployment", "Requests"]);
    expect(onOverview.actions.menu).toEqual(["Edit", "Pause", "Teardown…"]);
  });

  it("opens an upstream deployment with the sections and actions of the models page", async () => {
    const user = userEvent.setup();
    const deployments = [upstreamDeployment(upstreamTemplate(), ModelDeploymentStatus.READY)];

    const onModels = await sidebarShape(
      user,
      deployments,
      `/models?${upstreamPanel}`,
      "Private model",
    );
    cleanup();
    const onOverview = await sidebarShape(
      user,
      deployments,
      `/overview?${upstreamPanel}`,
      "Private model",
    );

    expect(onOverview).toEqual(onModels);
    expect(onOverview.sections).toEqual(["Deployment", "Source", "Requests"]);
    expect(onOverview.actions.menu).toEqual(["Edit", "Teardown…"]);
  });

  it("opens a deployment's logs over its sidebar on the overview, and back to the sidebar", async () => {
    const user = userEvent.setup();
    const logs: DeploymentLogOperations = {
      follow: vi.fn(async function* () {}),
      read: vi.fn().mockResolvedValue({
        entries: [],
        historyState: "no-older-output-observed",
        latestPositions: [],
        responseTruncated: false,
        sources: [],
      }),
    };
    const { router } = renderOverviewSection({
      deployments: [
        localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
      ],
      initialEntry: `/overview?${localPanel}`,
      logs,
      models: [gemma],
    });

    await user.click(await screen.findByRole("button", { name: "Logs" }));

    expect(await screen.findByRole("dialog", { name: "Gemma · Logs" })).toBeVisible();
    expect(currentLocation(router)).toBe(
      `/overview?${localPanel}&panel=models%2Flogs%3Adeployment-a`,
    );

    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(currentLocation(router)).toBe(`/overview?${localPanel}`);
    expect(screen.getByRole("complementary", { name: "Gemma" })).toBeVisible();
  });

  it("returns to the overview when the deployment sidebar closes", async () => {
    const user = userEvent.setup();
    const { router } = renderOverviewSection({
      deployments: [
        localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
      ],
      initialEntry: `/overview?${localPanel}`,
      models: [gemma],
    });
    const sidebar = await screen.findByRole("complementary", { name: "Gemma" });

    await user.click(within(sidebar).getByRole("button", { name: "Close inspector" }));

    await waitFor(() => expect(currentLocation(router)).toBe("/overview"));
    expect(screen.queryByRole("complementary", { name: "Gemma" })).not.toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1, name: "Overview" })).toBeVisible();
  });
});

describe("OverviewSidebar routes and machines", () => {
  it("opens a route with its sections and actions, and closes to the overview", async () => {
    const user = userEvent.setup();
    const { router } = renderOverviewSection({
      deployments: [
        localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
      ],
      initialEntry: "/overview?panel=routes%2Froute%3Aroute-a",
      listRoutes: () =>
        Promise.resolve(
          create(ListRoutesResponseSchema, {
            routes: [
              create(InferenceRouteViewSchema, {
                route: {
                  enabled: true,
                  fallbackGroups: [
                    {
                      modelDeploymentIds: ["deployment-a"],
                      selection: FallbackGroupSelection.SINGLE,
                    },
                  ],
                  generation: 1n,
                  inferenceRouteId: "route-a",
                  name: "chat",
                },
              }),
            ],
          }),
        ),
      models: [gemma],
    });
    const sidebar = await screen.findByRole("complementary", { name: "chat" });

    expect(
      within(sidebar)
        .getAllByRole("region")
        .map((section) => within(section).getByRole("heading", { level: 2 }).textContent),
    ).toEqual(["Routing", "Requests", "Details"]);
    const actions = await inspectorActionList(
      user,
      within(sidebar).getByTestId("inspector-footer"),
    );
    expect(actions.menu).toEqual(["Edit", "Disable", "Delete…"]);

    await user.click(within(sidebar).getByRole("button", { name: "Close inspector" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/overview"));
    expect(screen.queryByRole("complementary", { name: "chat" })).not.toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1, name: "Overview" })).toBeVisible();
  });

  it("opens a machine and closes to the overview", async () => {
    const user = userEvent.setup();
    const { router } = renderOverviewSection({
      initialEntry: "/overview?panel=machines%2Fmachine%3Anode",
      machines: [listedMachine("node", "Machine one")],
      snapshots: { node: hardwareSnapshot({ cpuCount: 8 }) },
    });
    const sidebar = await screen.findByRole("complementary", { name: "Machine one" });

    expect(
      await within(sidebar).findByRole("button", {
        name: "View execution target Simulated models",
      }),
    ).toBeVisible();
    expect(currentLocation(router)).toBe("/overview?panel=machines%2Fmachine%3Anode");

    await user.click(within(sidebar).getByRole("button", { name: "Close inspector" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/overview"));
    expect(screen.queryByRole("complementary", { name: "Machine one" })).not.toBeInTheDocument();
  });
});

/** The section headings and footer actions of the deployment sidebar opened at `initialEntry`. */
async function sidebarShape(
  user: UserEvent,
  deployments: ModelDeployment[],
  initialEntry: string,
  name: string,
) {
  renderOverviewSection({ deployments, initialEntry, models: [gemma] });
  const sidebar = await screen.findByRole("complementary", { name });
  const sections = within(sidebar)
    .getAllByRole("region")
    .map((section) => within(section).getByRole("heading", { level: 2 }).textContent);
  const actions = await inspectorActionList(user, within(sidebar).getByTestId("inspector-footer"));
  return { actions, sections };
}

/** Renders the models section with the overview mounted at `/overview`. */
function renderOverviewSection(options: Parameters<typeof renderModelsSection>[0]) {
  return renderModelsSection({
    ...options,
    overview: <OverviewPage />,
  });
}
