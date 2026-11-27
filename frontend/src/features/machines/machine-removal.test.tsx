import { create } from "@bufbuild/protobuf";
import { screen, waitFor, within } from "@testing-library/react";
import { useRef } from "react";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";

import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import { fakeSiteMembershipClient } from "@/features/machines/add-machine/join-test-support";
import { hardwareSnapshot, listedMachine } from "@/features/memory/machine-test-support";
import { nodeMemoryPayload } from "@/features/memory/node-memory-test-support";
import {
  metricsClientWith,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { ListApplicationDeploymentsResponseSchema } from "@/gen/dotcore/application/v1/service_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import {
  MachineRemovalDeploymentKind,
  MachineRemovalRefusalReason,
  RemoveMachineResponseSchema,
} from "@/gen/dotcore/site/v1/membership_pb";
import {
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

const panel = "panel=machines%2Fmachine%3Aone";

const modelDeployment = {
  deploymentId: "qwen-1",
  kind: MachineRemovalDeploymentKind.MODEL,
  name: "Qwen chat",
};
const tearingDownApplication = {
  deploymentId: "webui-1",
  kind: MachineRemovalDeploymentKind.APPLICATION,
  name: "Web UI",
  tearingDown: true,
};

function response(init: Parameters<typeof create<typeof RemoveMachineResponseSchema>>[1]) {
  return create(RemoveMachineResponseSchema, init);
}

describe("machine removal", () => {
  it("offers no removal for the site manager's machine", async () => {
    const manager = listedMachine("one", "Machine one");
    manager.siteManager = true;
    const { membership } = renderHost({ machine: manager });

    const inspector = await screen.findByRole("complementary", { name: "Machine one" });
    await within(inspector).findByRole("heading", { name: "Execution targets" });

    expect(screen.queryByRole("button", { name: "More actions" })).not.toBeInTheDocument();
    expect(membership.spies.removeMachine).not.toHaveBeenCalled();
  });

  it("confirms a removal without deployments, sends it without forgetting and closes", async () => {
    const user = setupUser();
    const { membership, router } = renderHost();
    membership.spies.removeMachine
      .mockResolvedValueOnce(
        response({
          outcome: { case: "wouldRemove", value: { displayName: "Machine one", nodeId: "one" } },
        }),
      )
      .mockResolvedValueOnce(
        response({
          outcome: { case: "removed", value: { displayName: "Machine one", nodeId: "one" } },
        }),
      );

    await openRemove(user);
    const dialog = await screen.findByRole("dialog", { name: "Remove Machine one?" });
    expect(dialog).toHaveTextContent(
      "Machine one is told to stop and leaves the site. Run dotc node leave on it to clean its folder.",
    );
    expect(membership.spies.removeMachine).toHaveBeenLastCalledWith({
      forgetDeployments: false,
      nodeId: "one",
      validateOnly: true,
    });

    await user.click(within(dialog).getByRole("button", { name: "Remove" }));

    await waitFor(() => expect(router.state.location.search).toBe(""));
    expect(membership.spies.removeMachine).toHaveBeenLastCalledWith({
      forgetDeployments: false,
      nodeId: "one",
      validateOnly: false,
    });
    expect(membership.spies.removeMachine).toHaveBeenCalledTimes(2);
  });

  it("lists the blocking deployments of a connected machine and offers no removal", async () => {
    const user = setupUser();
    const { membership } = renderHost();
    membership.spies.removeMachine.mockResolvedValueOnce(
      response({
        outcome: {
          case: "refused",
          value: {
            deployments: [modelDeployment, tearingDownApplication],
            message: "deployments are placed on the machine",
            reason: MachineRemovalRefusalReason.DEPLOYMENTS_PLACED,
          },
        },
      }),
    );

    await openRemove(user);

    const dialog = await screen.findByRole("dialog", { name: "Machine one has deployments" });
    expect(dialog).toHaveTextContent("Remove its deployments first, then remove the machine.");
    const items = within(within(dialog).getByRole("list", { name: "Deployments" })).getAllByRole(
      "listitem",
    );
    expect(items.map((item) => item.textContent)).toEqual(["Qwen chat", "Web UI · Tearing down"]);
    expect(within(items[0]).getByRole("link")).toHaveAttribute(
      "href",
      `/overview?${panel}&panel=models%2Fdeployment%3Aqwen-1`,
    );
    expect(within(items[1]).getByRole("link")).toHaveAttribute(
      "href",
      `/apps?${panel}&panel=apps%2Fdeployment%3Awebui-1`,
    );
    expect(
      within(dialog)
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Close"]);
  });

  it("forgets the deployments of a disconnected machine after naming them and their routes", async () => {
    const user = setupUser();
    const { membership, router } = renderHost({ state: NodeObservationState.DISCONNECTED });
    membership.spies.removeMachine
      .mockResolvedValueOnce(
        response({
          outcome: {
            case: "refused",
            value: {
              deployments: [modelDeployment, tearingDownApplication],
              reason: MachineRemovalRefusalReason.DEPLOYMENTS_PLACED,
            },
          },
        }),
      )
      .mockResolvedValueOnce(
        response({
          outcome: {
            case: "wouldRemove",
            value: {
              deletedRoutes: [{ inferenceRouteId: "route-1", name: "chat" }],
              displayName: "Machine one",
              forgottenDeployments: [modelDeployment, tearingDownApplication],
              nodeId: "one",
            },
          },
        }),
      )
      .mockResolvedValueOnce(
        response({ outcome: { case: "removed", value: { displayName: "Machine one" } } }),
      );

    await openRemove(user);
    const blocked = await screen.findByRole("dialog", { name: "Machine one has deployments" });
    await user.click(
      within(blocked).getByRole("button", { name: "Remove and forget 2 deployments…" }),
    );

    const dialog = await screen.findByRole("dialog", {
      name: "Remove Machine one and forget its deployments?",
    });
    expect(membership.spies.removeMachine).toHaveBeenLastCalledWith({
      forgetDeployments: true,
      nodeId: "one",
      validateOnly: true,
    });
    expect(
      within(within(dialog).getByRole("list", { name: "Deployments" }))
        .getAllByRole("listitem")
        .map((item) => item.textContent),
    ).toEqual(["Qwen chat", "Web UI · Tearing down"]);
    expect(dialog).toHaveTextContent("Routes deleted with them: chat");
    expect(dialog).toHaveTextContent("may keep running on the machine until it is cleaned");

    await user.click(
      within(dialog).getByRole("button", { name: "Remove and forget 2 deployments" }),
    );

    await waitFor(() => expect(router.state.location.search).toBe(""));
    expect(membership.spies.removeMachine).toHaveBeenLastCalledWith({
      forgetDeployments: true,
      nodeId: "one",
      validateOnly: false,
    });
  });

  it("shows the reason when the removal is refused after confirming", async () => {
    const user = setupUser();
    const { membership, router } = renderHost();
    membership.spies.removeMachine
      .mockResolvedValueOnce(
        response({
          outcome: { case: "wouldRemove", value: { displayName: "Machine one", nodeId: "one" } },
        }),
      )
      .mockResolvedValueOnce(
        response({
          outcome: {
            case: "refused",
            value: {
              deployments: [modelDeployment],
              message: "a deployment was placed on the machine meanwhile",
              reason: MachineRemovalRefusalReason.INVALID_SITE,
            },
          },
        }),
      );

    await openRemove(user);
    const dialog = await screen.findByRole("dialog", { name: "Remove Machine one?" });
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));

    const refused = await screen.findByRole("dialog", { name: "Can't remove machine" });
    expect(refused).toHaveTextContent("a deployment was placed on the machine meanwhile");
    expect(within(refused).getByRole("link", { name: "Qwen chat" })).toBeVisible();
    expect(router.state.location.search).toBe(`?${panel}`);
  });

  it("shows the failure when the removal call fails", async () => {
    const user = setupUser();
    const { membership } = renderHost();
    membership.spies.removeMachine.mockRejectedValueOnce(new Error("manager unreachable"));

    await openRemove(user);

    const dialog = await screen.findByRole("dialog", { name: "Can't remove machine" });
    expect(dialog).toHaveTextContent("manager unreachable");
  });
});

/** A worker's machine keeps its consequential actions in the inspector's more-actions menu. */
async function openRemove(user: ReturnType<typeof setupUser>) {
  await user.click(await screen.findByRole("button", { name: "More actions" }));
  await user.click(await screen.findByRole("menuitem", { name: "Remove…" }));
}

function renderHost({
  machine,
  state = NodeObservationState.CONNECTED,
}: {
  machine?: ReturnType<typeof listedMachine>;
  state?: NodeObservationState;
} = {}) {
  const membership = fakeSiteMembershipClient();
  const listed = machine ?? listedMachine("one", "Machine one", state);
  const watch = new FakeResourceWatch({
    sources: {
      applicationDeployments: () =>
        Promise.resolve(create(ListApplicationDeploymentsResponseSchema)),
      executionTargets: () => Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
      hardwareSnapshots: () =>
        Promise.resolve(
          hardwareSnapshotsPayload({
            one: create(MachineHardwareSnapshotSchema, {
              snapshot: hardwareSnapshot({
                cpuCount: 8,
                memory: { availableGib: 16, totalGib: 32 },
              }),
            }),
          }),
        ),
      machines: () => Promise.resolve(create(ListMachinesResponseSchema, { machines: [listed] })),
      modelDeployments: () => Promise.resolve(create(ListModelDeploymentsResponseSchema)),
      nodeMemory: () => Promise.resolve(nodeMemoryPayload([])),
    },
  });
  const router = createMemoryRouter(
    [
      {
        path: "/overview",
        element: (
          <ResourceWatchProvider client={watch.client()}>
            <PanelNavigationProvider>
              <Host />
            </PanelNavigationProvider>
          </ResourceWatchProvider>
        ),
      },
    ],
    { initialEntries: ["/overview", `/overview?${panel}`] },
  );
  renderWithClients(<RouterProvider router={router} />, {
    metricsClient: metricsClientWith(staticWatch(() => queryResult({ series: [], stepCount: 1 }))),
    siteMembershipClient: membership.client,
  });

  return { membership, router };
}

function Host() {
  const openerRef = useRef<HTMLElement>(null);

  return <MachineSidebarHost initialRange="6h" openerRef={openerRef} />;
}
