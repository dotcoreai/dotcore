import { create } from "@bufbuild/protobuf";
import { screen, waitFor, within } from "@testing-library/react";
import { useRef } from "react";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";

import type { RouteClient } from "@/api/route-client";
import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import {
  metricsClientWith,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import {
  DeleteRouteResponseSchema,
  FallbackGroupSchema,
  FallbackGroupSelection,
  InferenceRouteSchema,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
  ReplaceRouteResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import { ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";
import { chooseMoreAction, moreActionNames } from "@/test/inspector-actions";
import { renderWithClients } from "@/test/render-with-clients";
import { setupUser } from "@/test/user";

import { RouteSidebarHost } from "./route-sidebar-host";

describe("RouteSidebarHost", () => {
  it("opens a route on the overview with its sections, actions and the page range", async () => {
    const user = setupUser();
    const { router } = renderOnOverview(routeClient(), [routePanel]);

    const panel = await screen.findByRole("complementary", { name: "chat" });
    expect(
      within(panel)
        .getAllByRole("heading", { level: 2 })
        .map((heading) => heading.textContent),
    ).toEqual(["chat", "Routing", "Requests", "Details"]);
    expect(
      within(panel).getByRole("listitem", { name: "Local model, Not reported" }),
    ).toBeVisible();
    expect(await within(panel).findByRole("radio", { name: "6h" })).toBeChecked();
    expect(within(panel).getByRole("button", { name: "Connect" })).toBeVisible();
    expect(await moreActionNames(user)).toEqual(["Edit", "Disable", "Delete…"]);
    await user.keyboard("{Escape}");

    await user.click(within(panel).getByRole("button", { name: "Connect" }));
    await screen.findByTestId("route-connection-view");
    expect(currentLocation(router)).toBe(`/overview?${routePanel}&${connectPanel}`);

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/overview"));
    await waitFor(() => expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument());
  });

  it("edits a route on the overview behind the dirty guard, then returns to its details", async () => {
    const user = setupUser();
    const client = routeClient();
    const { router } = renderOnOverview(client, [routePanel]);

    await screen.findByRole("complementary", { name: "chat" });
    await chooseMoreAction(user, "Edit");
    const name = await screen.findByRole("textbox", { name: "Name" });
    expect(currentLocation(router)).toBe(`/overview?${routePanel}&${editPanel}`);
    await user.type(name, "-two");

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    expect(screen.getByRole("dialog", { name: "Discard changes?" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument(),
    );
    expect(currentLocation(router)).toBe(`/overview?${routePanel}&${editPanel}`);
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("chat-two");

    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(client.route.replaceRoute).toHaveBeenCalledWith({
      inferenceRouteId: "route-a",
      expectedGeneration: 1n,
      name: "chat-two",
      enabled: true,
      fallbackGroups: [
        { selection: FallbackGroupSelection.SINGLE, modelDeploymentIds: ["deployment-a"] },
      ],
    });
    expect(await screen.findByRole("complementary", { name: "chat-two" })).toBeVisible();
    await waitFor(() => expect(currentLocation(router)).toBe(`/overview?${routePanel}`));
  });

  it("deletes a route on the overview and closes to the overview", async () => {
    const user = setupUser();
    const client = routeClient();
    const { router } = renderOnOverview(client, [routePanel]);

    await screen.findByRole("complementary", { name: "chat" });
    await chooseMoreAction(user, "Delete…");
    const confirmation = screen.getByRole("dialog", { name: "Delete route?" });
    await user.click(within(confirmation).getByRole("button", { name: "Delete" }));

    expect(client.route.deleteRoute).toHaveBeenCalledWith({
      inferenceRouteId: "route-a",
      expectedGeneration: 1n,
    });
    await waitFor(() => expect(currentLocation(router)).toBe("/overview"));
    await waitFor(() => expect(screen.queryByTestId("inspector-content")).not.toBeInTheDocument());
  });
});

const routePanel = "panel=routes%2Froute%3Aroute-a";
const connectPanel = "panel=routes%2Fconnect%3Aroute-a";
const editPanel = "panel=routes%2Fedit%3Aroute-a";

/** Renders only the route host on the overview, at the overview's 6h range. */
function renderOnOverview(client: RouteClient, panels: string[]) {
  const watch = new FakeResourceWatch({
    sources: {
      modelDeployments: () =>
        Promise.resolve(
          create(ListModelDeploymentsResponseSchema, {
            deployments: [
              create(ModelDeploymentSchema, {
                modelDeploymentId: "deployment-a",
                snapshot: {
                  source: {
                    case: "local",
                    value: { modelDisplayName: "Local model", modelId: "local" },
                  },
                },
                state: { status: ModelDeploymentStatus.READY },
              }),
            ],
          }),
        ),
      routes: () => client.route.listRoutes({}),
    },
  });
  const router = createMemoryRouter(
    [
      {
        path: "/overview",
        element: (
          <ResourceWatchProvider client={watch.client()}>
            <PanelNavigationProvider>
              <OverviewRouteHost />
            </PanelNavigationProvider>
          </ResourceWatchProvider>
        ),
      },
    ],
    { initialEntries: ["/overview", `/overview?${panels.join("&")}`] },
  );

  return {
    router,
    ...renderWithClients(<RouterProvider router={router} />, {
      metricsClient,
      routeClient: client,
    }),
  };
}

function OverviewRouteHost() {
  const openerRef = useRef<HTMLElement>(null);

  return <RouteSidebarHost initialRange="6h" openerRef={openerRef} />;
}

/** A route client listing one enabled route, `chat`, whose primary tier is `deployment-a`. */
function routeClient(): RouteClient {
  let route = create(InferenceRouteSchema, {
    enabled: true,
    fallbackGroups: [
      create(FallbackGroupSchema, {
        modelDeploymentIds: ["deployment-a"],
        selection: FallbackGroupSelection.SINGLE,
      }),
    ],
    generation: 1n,
    inferenceRouteId: "route-a",
    name: "chat",
  });
  const routes = () => [create(InferenceRouteViewSchema, { route })];

  return {
    route: {
      createRoute: vi.fn(),
      deleteRoute: vi.fn(() => Promise.resolve(create(DeleteRouteResponseSchema))),
      getRoute: vi.fn(),
      listRoutes: vi.fn(() =>
        Promise.resolve(create(ListRoutesResponseSchema, { routes: routes() })),
      ),
      replaceRoute: vi.fn(({ name }: { name: string }) => {
        route = create(InferenceRouteSchema, { ...route, generation: 2n, name });
        return Promise.resolve(create(ReplaceRouteResponseSchema, { route: routes()[0] }));
      }),
    },
  } as unknown as RouteClient;
}

/** A store serving 15m and 6h ranges with no samples. */
const metricsClient = metricsClientWith(
  staticWatch(() => queryResult({ series: [], stepCount: 1 })),
);

function currentLocation(router: { state: { location: { pathname: string; search: string } } }) {
  return router.state.location.pathname + router.state.location.search;
}
