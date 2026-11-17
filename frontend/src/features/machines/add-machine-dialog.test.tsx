import { Code, ConnectError } from "@connectrpc/connect";
import { screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { JoinHostSource, JoinTargetRuntime } from "@/gen/dotcore/site/v1/join_pb";
import { ExecutionTargetWorkloadKind } from "@/gen/dotcore/site/v1/service_pb";
import * as clipboard from "@/lib/clipboard";
import { renderWithClients } from "@/test/render-with-clients";
import { setupUser } from "@/test/user";

import { AddMachineDialog } from "./add-machine-dialog";
import { fakeSiteJoinClient } from "./join-test-support";

const writeClipboard = vi.fn<typeof clipboard.writeClipboard>();

beforeEach(() => {
  vi.spyOn(clipboard, "writeClipboard").mockImplementation(writeClipboard);
  writeClipboard.mockResolvedValue(undefined);
});

function Harness() {
  const [open, setOpen] = useState(true);
  return <AddMachineDialog open={open} onOpenChange={setOpen} />;
}

async function openDialog() {
  const join = fakeSiteJoinClient();
  renderWithClients(<Harness />, { siteJoinClient: join.client });
  const dialog = await screen.findByRole("dialog", { name: "Add a machine" });
  await within(dialog).findByText("Waiting for the machine");
  return { dialog, join, user: setupUser() };
}

const newMachine = {
  state: {
    case: "awaitingApproval" as const,
    value: {
      proposalId: "proposal-1",
      proposal: {
        displayName: "Worker one",
        host: "192.0.2.10",
        hostDiffersFromPeer: true,
        hostSource: JoinHostSource.PROPOSED,
        keyFingerprint: "ab12cd34",
        nodeId: "worker-one",
        peerAddress: "192.0.2.99",
        privatePort: 50052,
        kind: {
          case: "newNode" as const,
          value: {
            targets: [
              {
                acceleratorDeviceIds: ["gpu0"],
                platformLabel: "linux/amd64",
                runtime: JoinTargetRuntime.SIMULATED,
                workloadKind: ExecutionTargetWorkloadKind.MODEL,
              },
            ],
          },
        },
      },
    },
  },
};

describe("AddMachineDialog", () => {
  it("shows the join command with its expiry and copies it", async () => {
    const { dialog, join, user } = await openDialog();

    expect(within(dialog).getByText("dotc node join --token token-1")).toBeVisible();
    expect(within(dialog).getByText("Expires at 14:32")).toBeVisible();
    expect(join.spies.startJoin).toHaveBeenCalledTimes(1);
    expect(join.spies.watchJoin).toHaveBeenCalledWith({ joinId: "join-1" }, expect.anything());

    await user.click(within(dialog).getByRole("button", { name: "Copy join command" }));
    expect(writeClipboard).toHaveBeenCalledExactlyOnceWith("dotc node join --token token-1");
  });

  it("shows why an attempt ended and keeps waiting", async () => {
    const { dialog, join } = await openDialog();

    join.report({
      state: {
        case: "waiting",
        value: { lastAttempt: { outcome: { case: "failed", value: "dial-back timed out" } } },
      },
    });
    expect(
      await within(dialog).findByText("Last attempt failed: dial-back timed out"),
    ).toBeVisible();
    expect(within(dialog).getByText("Waiting for the machine")).toBeVisible();

    join.report({
      state: {
        case: "waiting",
        value: { lastAttempt: { outcome: { case: "rejected", value: {} } } },
      },
    });
    expect(await within(dialog).findByText("The last attempt was rejected.")).toBeVisible();
    expect(within(dialog).getByText("dotc node join --token token-1")).toBeVisible();
  });

  it("shows the machine's details and approves it", async () => {
    const { dialog, join, user } = await openDialog();

    join.report(newMachine);
    await within(dialog).findByRole("button", { name: "Approve" });

    const details = Object.fromEntries(
      within(dialog)
        .getAllByRole("term")
        .map((term) => [term.textContent, term.nextElementSibling?.textContent]),
    );
    expect(details).toEqual({
      Name: "Worker one",
      Address: "192.0.2.10:50052",
      "Connected from": "192.0.2.99",
      Targets: "Model · Simulated · linux/amd64 · gpu0",
      "Key fingerprint": "ab12cd34",
    });

    await user.click(within(dialog).getByRole("button", { name: "Approve" }));
    expect(join.spies.approveJoin).toHaveBeenCalledExactlyOnceWith({
      joinId: "join-1",
      proposalId: "proposal-1",
    });
    expect(within(dialog).getByRole("button", { name: "Approve" })).toBeDisabled();
    expect(within(dialog).getByRole("button", { name: "Reject" })).toBeDisabled();

    join.report({ state: { case: "committed", value: { nodeId: "worker-one" } } });
    expect(await within(dialog).findByText("Worker one was added.")).toBeVisible();

    // A committed join is final: closing leaves nothing to cancel.
    await user.click(within(dialog).getByRole("button", { name: "Done" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(join.spies.cancelJoin).not.toHaveBeenCalled();
  });

  it("rejects a proposal and returns to waiting", async () => {
    const { dialog, join, user } = await openDialog();

    join.report(newMachine);
    await user.click(await within(dialog).findByRole("button", { name: "Reject" }));
    expect(join.spies.rejectJoin).toHaveBeenCalledExactlyOnceWith({
      joinId: "join-1",
      proposalId: "proposal-1",
    });

    join.report({
      state: {
        case: "waiting",
        value: { lastAttempt: { outcome: { case: "rejected", value: {} } } },
      },
    });
    expect(await within(dialog).findByText("The last attempt was rejected.")).toBeVisible();
  });

  it("shows a rejoin's changes", async () => {
    const { dialog, join } = await openDialog();

    join.report({
      state: {
        case: "awaitingApproval",
        value: {
          proposalId: "proposal-2",
          proposal: {
            ...newMachine.state.value.proposal,
            hostDiffersFromPeer: false,
            kind: {
              case: "rejoin",
              value: {
                deployments: 2,
                managerHost: "",
                previousHost: "192.0.2.11",
                previousManagerHost: "",
              },
            },
          },
        },
      },
    });

    await within(dialog).findByText("Existing machine again");
    expect(within(dialog).getByText("192.0.2.11")).toBeVisible();
    expect(within(dialog).getByText("2")).toBeVisible();
  });

  it("shows a decision that failed because dotc decided first and lets the operator retry", async () => {
    const { dialog, join, user } = await openDialog();
    join.spies.approveJoin.mockRejectedValueOnce(
      new ConnectError("proposal already decided", Code.FailedPrecondition),
    );

    join.report(newMachine);
    await user.click(await within(dialog).findByRole("button", { name: "Approve" }));

    expect(await within(dialog).findByText("proposal already decided")).toBeVisible();
    expect(within(dialog).getByRole("button", { name: "Approve" })).toBeEnabled();
  });

  it.each([
    [
      "expired",
      { state: { case: "expired" as const, value: {} } },
      "The command expired.",
      "New command",
    ],
    [
      "cancelled",
      { state: { case: "cancelled" as const, value: {} } },
      "The join was cancelled.",
      "Start again",
    ],
    [
      "failed",
      { state: { case: "failed" as const, value: { reason: "too many failed attempts" } } },
      "too many failed attempts",
      "Start again",
    ],
  ])("offers a new join when it is %s", async (_name, state, message, action) => {
    const { dialog, join, user } = await openDialog();

    join.report(state);
    expect(await within(dialog).findByText(message)).toBeVisible();

    await user.click(within(dialog).getByRole("button", { name: action }));
    expect(await within(dialog).findByText("dotc node join --token token-2")).toBeVisible();
    expect(join.spies.startJoin).toHaveBeenCalledTimes(2);
    // Final states leave nothing to cancel.
    expect(join.spies.cancelJoin).not.toHaveBeenCalled();
  });

  it("cancels the join when closed before it is committed", async () => {
    const { dialog, join, user } = await openDialog();

    await user.click(within(dialog).getByRole("button", { name: "Close" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(join.spies.cancelJoin).toHaveBeenCalledExactlyOnceWith({ joinId: "join-1" });
  });

  it("shows why the join could not start", async () => {
    const join = fakeSiteJoinClient();
    join.spies.startJoin.mockRejectedValueOnce(
      new ConnectError("too many active joins", Code.ResourceExhausted),
    );
    renderWithClients(<Harness />, { siteJoinClient: join.client });

    const dialog = await screen.findByRole("dialog", { name: "Add a machine" });
    expect(await within(dialog).findByText("too many active joins")).toBeVisible();

    await setupUser().click(within(dialog).getByRole("button", { name: "Try again" }));
    expect(await within(dialog).findByText("dotc node join --token token-1")).toBeVisible();
  });

  it("reports a watch that ends without a final state", async () => {
    const { dialog, join } = await openDialog();

    join.end();

    expect(await within(dialog).findByText("Lost the connection to the manager.")).toBeVisible();
  });
});
