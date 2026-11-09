import { create } from "@bufbuild/protobuf";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router-dom";
import { describe, expect, it } from "vitest";

import type { MetricCards } from "@/features/metrics/use-metric-cards";
import { ApplicationDeploymentSchema } from "@/gen/dotcore/application/v1/deployment_pb";
import { ListApplicationDeploymentsResponseSchema } from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import {
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import { ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { InferenceCountCards, OverviewAppsSection } from "./overview-count-cards";

// The count cards read no metrics, so the apps section gets none to show.
const noMetrics: MetricCards = {
  cards: [],
  chart: undefined,
  failed: false,
  groupValues: () => [],
  loadingVisible: false,
  range: undefined,
  ranges: [],
  selected: undefined,
  setRange: () => undefined,
  setSelected: () => undefined,
  setSplit: () => undefined,
  split: undefined,
  unavailable: false,
};

const modelStatuses = [
  ModelDeploymentStatus.READY,
  ModelDeploymentStatus.READY,
  ModelDeploymentStatus.FAILED,
  ModelDeploymentStatus.PAUSED,
];
const modelDeployments = modelStatuses.map((status, index) =>
  create(ModelDeploymentSchema, { modelDeploymentId: `model-${index}`, state: { status } }),
);
const routes = [
  routeView("route-a", true, true),
  routeView("route-b", true, false),
  routeView("route-c", false, false),
];
const appStates = [
  [ApplicationLifecycleState.READY, ApplicationReadinessState.READY],
  [ApplicationLifecycleState.FAILED, ApplicationReadinessState.NOT_READY],
  [ApplicationLifecycleState.PAUSED, ApplicationReadinessState.NOT_READY],
  [ApplicationLifecycleState.UNAVAILABLE, ApplicationReadinessState.NOT_READY],
] as const;
const appDeployments = appStates.map(([lifecycleState, readinessState], index) =>
  create(ApplicationDeploymentSchema, {
    applicationDeploymentId: `app-${index}`,
    state: { lifecycleState, readinessState },
  }),
);

describe("overview count cards", () => {
  it("counts what is ready out of the total and calls out failed and unavailable items", async () => {
    renderCards();

    await waitFor(() => expect(cardTexts()).toEqual(expectedCards));
    expect(screen.getByRole("heading", { name: "Apps" })).toBeInTheDocument();
  });

  it("leaves out the attention text when nothing needs it", async () => {
    renderCards({ appDeployments: [appDeployments[0]], modelDeployments: [], routes: [routes[0]] });

    await waitFor(() =>
      expect(cardTexts()).toEqual([
        "Models | 0 ready | of 0 deployed",
        "Routes | 1 available | of 1 configured",
        "Apps | 1 ready | of 1 deployed",
      ]),
    );
  });

  it.each([
    ["Models", "/models"],
    ["Routes", "/routes"],
    ["Apps", "/apps"],
  ])("opens the %s list page from its card", async (name, path) => {
    const user = userEvent.setup();
    renderCards();
    await waitFor(() => expect(cardTexts()).toHaveLength(3));

    await user.click(screen.getByRole("link", { name }));

    expect(screen.getByTestId("location")).toHaveTextContent(path);
  });
});

const expectedCards = [
  "Models | 2 ready | of 4 deployed · 1 failed",
  "Routes | 1 available | of 3 configured · 1 unavailable",
  "Apps | 1 ready | of 4 deployed · 1 failed",
];

function renderCards(data = { appDeployments, modelDeployments, routes }) {
  const watch = new FakeResourceWatch({
    sources: {
      applicationDeployments: () =>
        Promise.resolve(
          create(ListApplicationDeploymentsResponseSchema, { deployments: data.appDeployments }),
        ),
      modelDeployments: () =>
        Promise.resolve(
          create(ListModelDeploymentsResponseSchema, { deployments: data.modelDeployments }),
        ),
      routes: () => Promise.resolve(create(ListRoutesResponseSchema, { routes: data.routes })),
    },
  });

  render(
    <MemoryRouter initialEntries={["/overview"]}>
      <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
        <div data-testid="cards">
          <InferenceCountCards />
        </div>
        <OverviewAppsSection metrics={noMetrics} />
        <CurrentPath />
      </ResourceWatchProvider>
    </MemoryRouter>,
  );
}

function CurrentPath() {
  return <output data-testid="location">{useLocation().pathname}</output>;
}

function routeView(id: string, enabled: boolean, available: boolean) {
  return create(InferenceRouteViewSchema, {
    availability: { available },
    route: { enabled, inferenceRouteId: id, name: id },
  });
}

function cardTexts(): string[] {
  return Array.from(document.querySelectorAll('[data-slot="metric-card"]')).map((card) =>
    [
      card.querySelector('[data-slot="metric-card-label"]'),
      card.querySelector('[data-slot="metric-card-value"]'),
      card.querySelector('[data-slot="metric-card-line"]'),
    ]
      .map((part) => part?.textContent?.replace(/\s+/g, " ").trim())
      .filter(Boolean)
      .join(" | "),
  );
}
