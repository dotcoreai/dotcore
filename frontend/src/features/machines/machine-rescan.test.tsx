import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, screen, waitFor, within } from "@testing-library/react";
import { StrictMode, useRef, useState } from "react";
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
import { RescanDeploymentKind } from "@/gen/dotcore/site/v1/rescan_pb";
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

import { MachineRescanDialog } from "./machine-rescan-dialog";
import { fakeSiteRescanClient, target } from "./machine-rescan-test-support";
import { MachineSidebarHost } from "./machine-sidebar-host";

vi.mock("uplot", () => ({
  default: class {
    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const machinePanel = "panel=machines%2Fmachine%3Aone";
const otherMachinePanel = "panel=machines%2Fmachine%3Atwo";

/** One model target of the simulated GPU, and one application target of Docker. */
const simulatedModels = target({
  acceleratorDeviceIds: ["gpu-0"],
  displayName: "Simulated GPU",
  executionTargetId: "one-models",
  platform: { architecture: "arm64", operatingSystem: "linux" },
  runtimeBinding: {
    case: "modelRuntime",
    value: { adapter: { case: "simulatedRuntime", value: {} } },
  },
  siteNodeId: "one",
});
const dockerApplications = target({
  displayName: "Docker applications",
  executionTargetId: "one-apps",
  platform: { architecture: "amd64", operatingSystem: "linux" },
  runtimeBinding: {
    case: "applicationRuntime",
    value: { adapter: { case: "dockerRuntime", value: {} } },
  },
  siteNodeId: "one",
});

describe("machine target re-scan", () => {
  it("offers no re-scan for the site manager's machine", async () => {
    const manager = listedMachine("one", "Machine one");
    manager.siteManager = true;
    const { rescan } = renderHost({ machine: manager });

    const inspector = await screen.findByRole("complementary", { name: "Machine one" });
    await within(inspector).findByRole("heading", { name: "Execution targets" });

    expect(within(inspector).queryByRole("button", { name: "More actions" })).toBeNull();
    expect(rescan.spies.startRescan).not.toHaveBeenCalled();
  });

  it("returns focus to the inspector action that opened the re-scan", async () => {
    const user = setupUser();
    renderHost();

    const inspector = await screen.findByRole("complementary", { name: "Machine one" });
    await within(inspector).findByRole("heading", { name: "Execution targets" });
    await user.click(within(inspector).getByRole("button", { name: "More actions" }));
    await user.click(await screen.findByRole("menuitem", { name: "Re-scan targets" }));

    const dialog = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
    await within(dialog).findByText("Waiting for Machine one…");
    await user.keyboard("{Escape}");

    expect(await screen.findByRole("button", { name: "More actions" })).toHaveFocus();
  });

  it("re-scans the machine its dialog was opened from, even once another machine is inspected", async () => {
    const user = setupUser();
    const { rescan, router } = renderHost();

    const inspector = await screen.findByRole("complementary", { name: "Machine one" });
    await within(inspector).findByRole("heading", { name: "Execution targets" });
    await user.click(within(inspector).getByRole("button", { name: "More actions" }));
    await user.click(await screen.findByRole("menuitem", { name: "Re-scan targets" }));

    const dialog = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
    expect(rescan.spies.startRescan).toHaveBeenCalledWith({ nodeId: "one" });
    expect(within(dialog).getByText("dotc node rescan")).toBeVisible();
    expect(within(dialog).getByText("on Machine one")).toBeVisible();

    // The re-scan keeps naming the node it was started from while the inspector moves on.
    await act(() => router.navigate(`/overview?${otherMachinePanel}`));
    expect(
      within(await screen.findByRole("dialog", { name: "Re-scan Machine one" })).getByText(
        "dotc node rescan",
      ),
    ).toBeVisible();
    expect(rescan.spies.startRescan.mock.calls.map(([request]) => request.nodeId)).toEqual(["one"]);
  });

  it("shows the command to run on the node and waits for its answer", async () => {
    const { dialog, rescan } = await openDialog();

    expect(within(dialog).getByText("dotc node rescan")).toBeVisible();
    expect(within(dialog).getByText("on Machine one")).toBeVisible();
    expect(
      within(dialog).getByText(
        "Run it in a terminal on Machine one, where its Dotcore folder lives and its server is " +
          "stopped. It scans that machine and proposes the whole replacement here.",
      ),
    ).toBeVisible();
    expect(within(dialog).getByText("Waiting for Machine one…")).toBeVisible();
    expect(rescan.spies.watchRescan).toHaveBeenCalledWith(
      { rescanId: "rescan-1" },
      expect.anything(),
    );

    // A refused answer stays visible while the invitation waits for a corrected one.
    rescan.report({
      state: {
        case: "waiting",
        value: {
          lastAttempt: {
            at: { seconds: 1_700_000_000n },
            outcome: {
              case: "failed",
              value:
                "the replacement would remove or change targets with deployments placed on them:\n" +
                "  - model deployment qwen-1 (Qwen chat) on one-models\n" +
                "the whole replacement is refused",
            },
          },
        },
      },
    });
    expect(
      await within(dialog).findByText(/the replacement would remove or change targets/),
    ).toHaveTextContent("Qwen chat");
    expect(within(dialog).getByText("Waiting for the next try")).toBeVisible();
  });

  it("reviews every target the proposal classifies before deciding on that proposal", async () => {
    const user = setupUser();
    const { dialog, rescan } = await openDialog();

    rescan.report({
      state: {
        case: "awaitingApproval",
        value: {
          proposal: {
            added: [dockerApplications],
            changed: [
              {
                ...simulatedModels,
                acceleratorDeviceIds: ["gpu-0", "gpu-1"],
                displayName: "Simulated GPU pair",
              },
            ],
            deployments: [
              {
                deploymentId: "qwen-1",
                executionTargetId: "one-models",
                kind: RescanDeploymentKind.MODEL,
                name: "Qwen chat",
                tearingDown: false,
              },
            ],
            displayName: "Machine one",
            nodeId: "one",
            removed: [],
            unchanged: [simulatedModels],
          },
          proposalId: "proposal-1",
        },
      },
    });

    const review = within(dialog);
    expect(await review.findByText("Unchanged")).toBeVisible();
    expect(
      review.getByText("Simulated GPU — Model · Simulated · linux/arm64 · gpu-0"),
    ).toBeVisible();
    expect(review.getByText(/Simulated GPU pair — Model · Simulated/)).toBeVisible();
    expect(
      review.getByText(/Docker applications — Application · Docker · linux\/amd64/),
    ).toBeVisible();
    expect(review.getByText("None")).toBeVisible();

    await user.click(review.getByRole("button", { name: "Approve the replacement" }));
    await waitFor(() =>
      expect(rescan.spies.approveRescan).toHaveBeenCalledWith({
        proposalId: "proposal-1",
        rescanId: "rescan-1",
      }),
    );
  });

  it("refuses a proposal, and the re-scan waits for the node to answer again", async () => {
    const user = setupUser();
    const { dialog, rescan } = await openDialog();

    rescan.report({
      state: {
        case: "awaitingApproval",
        value: {
          proposal: {
            added: [],
            changed: [],
            deployments: [],
            displayName: "Machine one",
            nodeId: "one",
            removed: [],
            unchanged: [simulatedModels],
          },
          proposalId: "proposal-1",
        },
      },
    });
    await user.click(await within(dialog).findByRole("button", { name: "Reject" }));
    await waitFor(() =>
      expect(rescan.spies.rejectRescan).toHaveBeenCalledWith({
        proposalId: "proposal-1",
        rescanId: "rescan-1",
      }),
    );

    rescan.report({
      state: {
        case: "waiting",
        value: {
          lastAttempt: {
            at: { seconds: 1_700_000_001n },
            outcome: { case: "rejected", value: {} },
          },
        },
      },
    });
    expect(await within(dialog).findByText("You refused this replacement.")).toBeVisible();
    expect(within(dialog).getByText("Waiting for the next try")).toBeVisible();
  });

  it("states the restart a recorded replacement owes, and never that the node serves under it", async () => {
    const { dialog, rescan } = await openDialog();

    rescan.report({
      state: {
        case: "committed",
        value: {
          nodeId: "one",
          restartRequired: true,
          targets: [simulatedModels, dockerApplications],
        },
      },
    });

    expect(await within(dialog).findByText("Replacement recorded")).toBeVisible();
    expect(
      within(dialog).getByText(
        "Machine one serves its previous targets until it restarts. Restart it to serve under the " +
          "recorded replacement.",
      ),
    ).toBeVisible();
    expect(within(dialog).getByText("Docker applications — Application · Docker · linux/amd64"));
  });

  it("says an unchanged projection wrote nothing and owes no restart", async () => {
    const { dialog, rescan } = await openDialog();

    rescan.report({ state: { case: "noOp", value: { nodeId: "one" } } });

    expect(await within(dialog).findByText("Nothing to replace")).toBeVisible();
    expect(
      within(dialog).getByText(
        "Machine one proposed what the site already records: nothing was written and no restart is " +
          "needed.",
      ),
    ).toBeVisible();
    expect(within(dialog).queryByText(/restart it/i)).toBeNull();
  });

  it("ends the re-scan when its dialog closes", async () => {
    const user = setupUser();
    const { dialog, rescan } = await openDialog();

    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() =>
      expect(rescan.spies.cancelRescan).toHaveBeenCalledWith({ rescanId: "rescan-1" }),
    );
    expect(screen.queryByRole("dialog", { name: "Re-scan Machine one" })).toBeNull();
  });

  it("starts the reopened dialog's re-scan once the closed dialog's cancel was answered", async () => {
    const user = setupUser();
    const rescan = fakeSiteRescanClient();
    const answerCancel = holdNextCancel(rescan);
    renderWithClients(<ReopenHarness />, { siteRescanClient: rescan.client });
    const dialog = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
    await within(dialog).findByText("Waiting for Machine one…");

    // The dialog reopens while the manager has not answered the first one's cancel.
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(rescan.spies.cancelRescan).toHaveBeenCalledTimes(1));
    await user.click(await screen.findByRole("button", { name: "Reopen", hidden: true }));
    expect(rescan.spies.startRescan).toHaveBeenCalledTimes(1);
    answerCancel();

    const reopened = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
    await expectFollowing(reopened, rescan, "rescan-2");
    expect(rescan.spies.startRescan).toHaveBeenCalledTimes(2);
    expect(rescan.spies.cancelRescan.mock.calls).toEqual([[{ rescanId: "rescan-1" }]]);
  });

  it("starts the reopened dialog's re-scan once a replacement being recorded ended", async () => {
    const user = setupUser();
    const rescan = fakeSiteRescanClient();
    refuseNextCancelWhileRecording(rescan);
    renderWithClients(<ReopenHarness />, { siteRescanClient: rescan.client });
    const dialog = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
    await within(dialog).findByText("Waiting for Machine one…");

    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(rescan.spies.watchRescan).toHaveBeenCalledTimes(2));
    await user.click(await screen.findByRole("button", { name: "Reopen", hidden: true }));
    expect(rescan.spies.startRescan).toHaveBeenCalledTimes(1);

    rescan.report({
      state: { case: "committed", value: { nodeId: "one", targets: [simulatedModels] } },
    });
    const reopened = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
    await expectFollowing(reopened, rescan, "rescan-2");
    expect(rescan.spies.cancelRescan.mock.calls).toEqual([[{ rescanId: "rescan-1" }]]);
  });

  it("shows the replacement being recorded when a restart meets it", async () => {
    const user = setupUser();
    const { dialog, rescan } = await openDialog();
    refuseNextCancelWhileRecording(rescan);

    act(() => rescan.end());
    await user.click(await within(dialog).findByRole("button", { name: "Try again" }));
    await waitFor(() => expect(rescan.spies.watchRescan).toHaveBeenCalledTimes(2));
    rescan.report({
      state: { case: "committed", value: { nodeId: "one", targets: [simulatedModels] } },
    });

    expect(await within(dialog).findByText("Replacement recorded")).toBeVisible();
    expect(rescan.spies.startRescan).toHaveBeenCalledTimes(1);

    await user.click(within(dialog).getByRole("button", { name: "Re-scan again" }));
    await expectFollowing(dialog, rescan, "rescan-2");
  });

  it("cancels a re-scan whose start was still pending when the dialog closed", async () => {
    const rescan = fakeSiteRescanClient();
    rescan.holdStarts();
    const { unmount } = renderWithClients(<Harness />, { siteRescanClient: rescan.client });
    await waitFor(() => expect(rescan.spies.startRescan).toHaveBeenCalledTimes(1));

    unmount();
    rescan.releaseStarts();

    await waitFor(() => expect(rescan.open()).toEqual([]));
    expect(rescan.spies.cancelRescan.mock.calls).toEqual([[{ rescanId: "rescan-1" }]]);
    expect(rescan.spies.startRescan).toHaveBeenCalledTimes(1);
    expect(rescan.spies.watchRescan).not.toHaveBeenCalled();
  });

  it("follows one re-scan across React's development remount", async () => {
    const rescan = fakeSiteRescanClient();
    renderWithClients(
      <StrictMode>
        <Harness />
      </StrictMode>,
      { siteRescanClient: rescan.client },
    );

    const dialog = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
    await within(dialog).findByText("Waiting for Machine one…");
    expect(rescan.spies.startRescan).toHaveBeenCalledTimes(1);
    expect(rescan.spies.watchRescan).toHaveBeenCalledTimes(1);
    expect(rescan.spies.cancelRescan).not.toHaveBeenCalled();
    expect(rescan.open()).toEqual(["rescan-1"]);
  });

  it("replaces and then ends the one re-scan it follows across React's development remount", async () => {
    const user = setupUser();
    const rescan = fakeSiteRescanClient();
    renderWithClients(
      <StrictMode>
        <Harness />
      </StrictMode>,
      { siteRescanClient: rescan.client },
    );
    const dialog = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
    await within(dialog).findByText("Waiting for Machine one…");

    rescan.report({ state: { case: "expired", value: {} } });
    await user.click(await within(dialog).findByRole("button", { name: "Get a new command" }));
    await expectFollowing(dialog, rescan, "rescan-2");
    expect(rescan.spies.startRescan).toHaveBeenCalledTimes(2);

    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(rescan.open()).toEqual([]));
    expect(rescan.spies.cancelRescan.mock.calls).toEqual([[{ rescanId: "rescan-2" }]]);
  });

  it("states why a decision was refused and lets the operator decide again", async () => {
    const user = setupUser();
    const { dialog, rescan } = await openDialog();

    rescan.report({
      state: {
        case: "awaitingApproval",
        value: {
          proposal: {
            added: [],
            changed: [],
            deployments: [],
            displayName: "Machine one",
            nodeId: "one",
            removed: [],
            unchanged: [simulatedModels],
          },
          proposalId: "proposal-1",
        },
      },
    });

    rescan.spies.approveRescan.mockRejectedValueOnce(
      new ConnectError(
        "a deployment sits on a target this replacement changes; move it off first",
        Code.FailedPrecondition,
      ),
    );
    await user.click(
      await within(dialog).findByRole("button", { name: "Approve the replacement" }),
    );

    expect(
      await within(dialog).findByText(
        "a deployment sits on a target this replacement changes; move it off first",
      ),
    ).toBeVisible();
    expect(within(dialog).getByRole("button", { name: "Approve the replacement" })).toBeEnabled();
  });

  it("shows an expired command and starts a new invitation when asked", async () => {
    const user = setupUser();
    const { dialog, rescan } = await openDialog();

    rescan.report({ state: { case: "expired", value: {} } });
    expect(
      await within(dialog).findByText(
        /This command expired at .*\. A node that runs it now is refused\./,
      ),
    ).toBeVisible();
    expect(within(dialog).getByText("dotc node rescan")).toBeVisible();

    await user.click(within(dialog).getByRole("button", { name: "Get a new command" }));
    await expectFollowing(dialog, rescan, "rescan-2");
    expect(rescan.spies.startRescan).toHaveBeenLastCalledWith({ nodeId: "one" });
  });

  it("starts again once a re-scan was cancelled", async () => {
    const user = setupUser();
    const { dialog, rescan } = await openDialog();

    rescan.report({ state: { case: "cancelled", value: {} } });
    await user.click(await within(dialog).findByRole("button", { name: "Start again" }));

    await expectFollowing(dialog, rescan, "rescan-2");
    expect(rescan.spies.cancelRescan).not.toHaveBeenCalled();
  });

  it("re-scans again once a replacement was recorded", async () => {
    const user = setupUser();
    const { dialog, rescan } = await openDialog();

    rescan.report({
      state: {
        case: "committed",
        value: { nodeId: "one", restartRequired: true, targets: [simulatedModels] },
      },
    });
    await user.click(await within(dialog).findByRole("button", { name: "Re-scan again" }));

    await expectFollowing(dialog, rescan, "rescan-2");
    expect(rescan.spies.cancelRescan).not.toHaveBeenCalled();
  });

  it("cancels a re-scan whose watch broke off before starting its replacement", async () => {
    const user = setupUser();
    const { dialog, rescan } = await openDialog();

    act(() => rescan.end());
    expect(await within(dialog).findByText("Lost the connection to the manager.")).toBeVisible();
    await user.click(within(dialog).getByRole("button", { name: "Try again" }));

    await expectFollowing(dialog, rescan, "rescan-2");
    expect(rescan.spies.cancelRescan).toHaveBeenCalledWith({ rescanId: "rescan-1" });
  });

  it("states why a re-scan could not start", async () => {
    const rescan = fakeSiteRescanClient();
    rescan.spies.startRescan.mockRejectedValueOnce(
      new ConnectError(
        "node one is this site's manager; re-scan its targets with the command on this machine",
        Code.FailedPrecondition,
      ),
    );
    renderWithClients(<Harness />, { siteRescanClient: rescan.client });

    const dialog = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
    expect(
      await within(dialog).findByText(
        "node one is this site's manager; re-scan its targets with the command on this machine",
      ),
    ).toBeVisible();

    await setupUser().click(within(dialog).getByRole("button", { name: "Start again" }));
    await expectFollowing(dialog, rescan, "rescan-1");
  });
});

/** Waits for the dialog to wait on the node under `rescanId`, the only re-scan still open. */
async function expectFollowing(
  dialog: HTMLElement,
  rescan: ReturnType<typeof fakeSiteRescanClient>,
  rescanId: string,
) {
  await waitFor(() =>
    expect(rescan.spies.watchRescan).toHaveBeenLastCalledWith({ rescanId }, expect.anything()),
  );
  expect(await within(dialog).findByText("Waiting for Machine one…")).toBeVisible();
  expect(within(dialog).queryByText(/already has an open re-scan/)).toBeNull();
  expect(rescan.open()).toEqual([rescanId]);
}

function Harness() {
  const [open, setOpen] = useState(true);
  const triggerRef = useRef<HTMLElement>(null);
  return (
    <MachineRescanDialog
      finalFocus={triggerRef}
      machineId="one"
      machineName="Machine one"
      onOpenChange={setOpen}
      open={open}
    />
  );
}

/** Makes the next cancel refused, as while the manager records an approved replacement. */
function refuseNextCancelWhileRecording(rescan: ReturnType<typeof fakeSiteRescanClient>) {
  rescan.spies.cancelRescan.mockRejectedValueOnce(
    new ConnectError(
      "an approved replacement is being recorded; wait for the re-scan to end",
      Code.FailedPrecondition,
    ),
  );
}

/** Leaves the next cancel unanswered until the returned function is called. */
function holdNextCancel(rescan: ReturnType<typeof fakeSiteRescanClient>): () => void {
  const answer = rescan.spies.cancelRescan.getMockImplementation();
  let release = () => {};
  const gate = new Promise<void>((resolve) => (release = resolve));
  rescan.spies.cancelRescan.mockImplementationOnce((request) => gate.then(() => answer!(request)));
  return release;
}

/** A host whose dialog can be reopened after it closed. */
function ReopenHarness() {
  const [open, setOpen] = useState(true);
  const triggerRef = useRef<HTMLElement>(null);
  return (
    <>
      <button onClick={() => setOpen(true)} type="button">
        Reopen
      </button>
      <MachineRescanDialog
        finalFocus={triggerRef}
        machineId="one"
        machineName="Machine one"
        onOpenChange={setOpen}
        open={open}
      />
    </>
  );
}

async function openDialog() {
  const rescan = fakeSiteRescanClient();
  renderWithClients(<Harness />, { siteRescanClient: rescan.client });
  const dialog = await screen.findByRole("dialog", { name: "Re-scan Machine one" });
  await within(dialog).findByText("Waiting for Machine one…");
  return { dialog, rescan, user: setupUser() };
}

/** Renders the machine sidebar over one worker machine, with a re-scan API to drive. */
function renderHost({ machine }: { machine?: ReturnType<typeof listedMachine> } = {}) {
  const membership = fakeSiteMembershipClient();
  const rescan = fakeSiteRescanClient();
  const listed = machine ?? listedMachine("one", "Machine one", NodeObservationState.CONNECTED);
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
      machines: () =>
        Promise.resolve(
          create(ListMachinesResponseSchema, {
            machines: [
              listed,
              { ...listed, displayName: "Machine two", machineId: "two", siteManager: false },
            ],
          }),
        ),
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
    { initialEntries: ["/overview", `/overview?${machinePanel}`] },
  );
  renderWithClients(<RouterProvider router={router} />, {
    metricsClient: metricsClientWith(staticWatch(() => queryResult({ series: [], stepCount: 1 }))),
    siteMembershipClient: membership.client,
    siteRescanClient: rescan.client,
  });

  return { rescan, router };
}

function Host() {
  const openerRef = { current: null };

  return <MachineSidebarHost initialRange="6h" openerRef={openerRef} />;
}
