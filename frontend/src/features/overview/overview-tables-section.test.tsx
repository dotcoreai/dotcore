import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { hardwareSnapshot, listedMachine } from "@/features/memory/machine-test-support";
import {
  counterPoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import {
  catalogModel,
  currentLocation,
  localDeployment,
  modelTarget,
  renderModelsSection,
  upstreamDeployment,
  upstreamTemplate,
} from "@/features/models/models-section-test-support";
import { ExactPlacementSchema } from "@/gen/dotcore/deployment/v1/placement_pb";
import {
  FallbackGroupSelection,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";
import { ModelDeploymentIntentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

vi.mock("uplot", () => ({
  default: class {
    static paths = { bars: () => undefined };

    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const gemma = catalogModel("gemma", "Gemma", ["default"]);

describe("OverviewTablesSection", () => {
  it("shows a row per deployment and route with its figures, idle ones at zero", async () => {
    renderOverview();

    const models = await screen.findByRole("table", { name: "Models" });
    await waitFor(() =>
      expect(rowTexts(models)).toContain("Gemma | Machine one / Simulated models | 3 | 90 | 30"),
    );
    expect(rowTexts(models)).toEqual([
      "Gemma | Machine one / Simulated models | 3 | 90 | 30",
      "Private model | upstream | 0 | 0 | 0",
    ]);

    const routes = screen.getByRole("table", { name: "Routes" });
    expect(rowTexts(routes)).toEqual([
      "chat | Gemma → Private model | 2 | 40 | 12",
      "idle | Private model | 0 | 0 | 0",
    ]);
  });

  it("opens the deployment and route sidebars from a row, by click and by Enter", async () => {
    const user = userEvent.setup();
    const { router } = renderOverview();
    const models = await screen.findByRole("table", { name: "Models" });

    await user.click(within(models).getByRole("row", { name: "Gemma" }));
    expect(await screen.findByRole("complementary", { name: "Gemma" })).toBeVisible();
    expect(currentLocation(router)).toBe("/overview?panel=models%2Fdeployment%3Adeployment-a");
    expect(within(models).getByRole("row", { name: "Gemma" })).toHaveAttribute(
      "data-state",
      "selected",
    );

    const routes = screen.getByRole("table", { name: "Routes" });
    within(routes).getByRole("row", { name: "chat" }).focus();
    await user.keyboard("{Enter}");
    expect(await screen.findByRole("complementary", { name: "chat" })).toBeVisible();
    expect(currentLocation(router)).toBe(
      "/overview?panel=models%2Fdeployment%3Adeployment-a&panel=routes%2Froute%3Aroute-a",
    );
  });
});

function renderOverview() {
  const placed = localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY });
  placed.intent = create(ModelDeploymentIntentSchema, {
    placement: create(ExactPlacementSchema, { executionTargetId: "pool", siteNodeId: "node" }),
  });
  const upstream = upstreamDeployment(upstreamTemplate(), ModelDeploymentStatus.READY);

  return renderModelsSection({
    deployments: [placed, upstream],
    initialEntry: "/overview",
    listRoutes: () =>
      Promise.resolve(
        create(ListRoutesResponseSchema, {
          routes: [
            route("route-a", "chat", ["deployment-a", "upstream-new"]),
            route("route-b", "idle", ["upstream-new"]),
          ],
        }),
      ),
    executionTargets: [modelTarget("node", "pool", "Simulated models", "Machine one")],
    machines: [listedMachine("node", "Machine one")],
    metricsClient: metricsClientWith(staticWatch(trafficAnswer)),
    models: [gemma],
    snapshots: { node: hardwareSnapshot({ cpuCount: 8 }) },
  });
}

function route(id: string, name: string, members: string[]) {
  return create(InferenceRouteViewSchema, {
    route: {
      enabled: true,
      fallbackGroups: [
        { modelDeploymentIds: members.slice(0, 1), selection: FallbackGroupSelection.SINGLE },
        ...(members.length > 1
          ? [{ modelDeploymentIds: members.slice(1), selection: FallbackGroupSelection.SINGLE }]
          : []),
      ],
      generation: 1n,
      inferenceRouteId: id,
      name,
    },
  });
}

/** Each row as "name | subtitle | figures..." from its rendered cells. */
function rowTexts(table: HTMLElement): string[] {
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) =>
      within(row)
        .getAllByRole("cell")
        .map(
          (cell) =>
            Array.from(cell.querySelectorAll("div"), (part) => part.textContent).join(" | ") ||
            cell.textContent,
        )
        .join(" | "),
    );
}

/**
 * Traffic grouped by deployment and route: Gemma took three attempts, processing 90 tokens and
 * generating 30; the chat route ended two requests, processing 40 tokens and generating 12.
 */
function trafficAnswer(query: MessageInitShape<typeof MetricQuerySchema> | undefined) {
  const groupBy = query?.groupBy ?? [];
  const byDeployment = groupBy.includes("deployment");
  const byRoute = groupBy.includes("route");
  const counter = (labels: Record<string, string>, delta: number) =>
    metricSeries(labels, [counterPoint(0, 0, false, delta)]);
  const series = (() => {
    switch (query?.metric) {
      case "router.attempts":
        return byDeployment ? [counter({ deployment: "deployment-a" }, 3)] : [];
      case "router.requests":
        return byRoute ? [counter({ route: "route-a" }, 2)] : [];
      case "router.tokens.completion":
        if (byDeployment) return [counter({ deployment: "deployment-a" }, 30)];
        return byRoute ? [counter({ route: "route-a" }, 12)] : [];
      case "router.tokens.prompt":
        if (byDeployment) return [counter({ deployment: "deployment-a" }, 90)];
        return byRoute ? [counter({ route: "route-a" }, 40)] : [];
      default:
        return [];
    }
  })();

  return queryResult({ series, stepCount: 60, stepSeconds: 15 });
}
