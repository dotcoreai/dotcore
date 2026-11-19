import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { JoinHostSource, JoinTargetRuntime } from "@/gen/dotcore/site/v1/membership_pb";
import { ExecutionTargetWorkloadKind } from "@/gen/dotcore/site/v1/service_pb";
import * as clipboard from "@/lib/clipboard";
import { productionTiming, setTiming } from "@/lib/timing";
import { useFakeTimersWithWaits } from "@/test/fake-timers";
import { renderWithClients } from "@/test/render-with-clients";
import { setupUser } from "@/test/user";

import { AddMachineDialog } from "./add-machine-dialog";
import {
  fakeSiteMembershipClient,
  goldenFingerprintPattern,
  goldenKeyFingerprint,
  joinLifetimeMs,
} from "./join-test-support";

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
  const join = fakeSiteMembershipClient();
  renderWithClients(<Harness />, { siteMembershipClient: join.client });
  const dialog = await screen.findByRole("dialog", { name: "Add a machine" });
  await within(dialog).findByText("Waiting for the machine…");
  return { dialog, join, user: setupUser() };
}

const newMachine = {
  state: {
    case: "awaitingApproval" as const,
    value: {
      proposalId: "proposal-1",
      proposal: {
        displayName: "Worker one",
        fingerprintPattern: goldenFingerprintPattern,
        host: "192.0.2.10",
        hostDiffersFromPeer: true,
        hostSource: JoinHostSource.PROPOSED,
        keyFingerprint: goldenKeyFingerprint,
        nodeId: "worker-one",
        peerAddress: "192.0.2.99:41234",
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

function waitingAfter(
  outcome: { case: "failed"; value: string } | { case: "rejected"; value: object },
  at: Date,
) {
  return {
    state: {
      case: "waiting" as const,
      value: { lastAttempt: { at: timestampFromDate(at), outcome } },
    },
  };
}

function stepStatuses(dialog: HTMLElement): string[] {
  return within(dialog)
    .getAllByRole("listitem")
    .filter((item) => item.hasAttribute("data-status"))
    .map((item) => item.getAttribute("data-status") ?? "");
}

describe("AddMachineDialog", () => {
  it("shows the join command in its pane and copies it", async () => {
    const { dialog, join, user } = await openDialog();

    expect(within(dialog).getByText("dotc node join --token token-1")).toBeVisible();
    expect(within(dialog).getByText("on the new machine")).toBeVisible();
    expect(within(dialog).getByText("Needs dotc on that machine.")).toBeVisible();
    expect(stepStatuses(dialog)).toEqual(["active", "active", "todo"]);
    expect(join.spies.startJoin).toHaveBeenCalledTimes(1);
    expect(join.spies.watchJoin).toHaveBeenCalledWith({ joinId: "join-1" }, expect.anything());

    await user.click(within(dialog).getByRole("button", { name: "Copy join command" }));
    expect(writeClipboard).toHaveBeenCalledExactlyOnceWith("dotc node join --token token-1");
  });

  describe("countdown", () => {
    it("counts down to the expiry and warns in the last two minutes", async () => {
      useFakeTimersWithWaits();
      setTiming(productionTiming);
      vi.setSystemTime(new Date(2030, 0, 1, 14, 0, 0));
      const join = fakeSiteMembershipClient();
      renderWithClients(<Harness />, { siteMembershipClient: join.client });

      const pill = (await screen.findByText("15:00 left")).closest("[data-tone]");
      expect(pill).toHaveAttribute("data-tone", "info");

      await act(() => vi.advanceTimersByTimeAsync(60_000));
      expect(screen.getByText("14:00 left")).toBeVisible();
      expect(pill).toHaveAttribute("data-tone", "info");

      await act(() => vi.advanceTimersByTimeAsync(joinLifetimeMs - 60_000 - 121_000));
      expect(screen.getByText("2:01 left")).toBeVisible();
      expect(pill).toHaveAttribute("data-tone", "info");

      await act(() => vi.advanceTimersByTimeAsync(1_000));
      expect(screen.getByText("2:00 left")).toBeVisible();
      expect(pill).toHaveAttribute("data-tone", "warning");
    });
  });

  describe("attempt log", () => {
    it("lists each attempt with its time and reason, telling repeats apart", async () => {
      const { dialog, join } = await openDialog();
      const first = new Date(2030, 0, 1, 22, 8, 4);
      const second = new Date(2030, 0, 1, 22, 8, 31);

      join.report(waitingAfter({ case: "failed", value: "dial-back timed out" }, first));
      join.report(waitingAfter({ case: "failed", value: "dial-back timed out" }, second));
      await within(dialog).findByText("22:08:31");

      // The same state repeated adds nothing.
      join.report(waitingAfter({ case: "failed", value: "dial-back timed out" }, second));
      join.report(waitingAfter({ case: "rejected", value: {} }, new Date(2030, 0, 1, 22, 9, 0)));
      await within(dialog).findByText("22:09:00");

      const rows = [...dialog.querySelectorAll("time")].map(
        (time) => time.parentElement?.textContent,
      );
      expect(rows).toEqual([
        "22:08:04dial-back timed out",
        "22:08:31dial-back timed out",
        "22:09:00You rejected the machine.",
      ]);
      expect(within(dialog).getByText("Waiting for the next try")).toBeVisible();
      expect(stepStatuses(dialog)).toEqual(["active", "warning", "todo"]);
    });
  });

  describe("review", () => {
    it("shows the pattern, the grouped fingerprint and the details, and approves", async () => {
      const { dialog, join, user } = await openDialog();

      join.report(newMachine);
      await within(dialog).findByRole("button", { name: "Approve Worker one" });
      expect(stepStatuses(dialog)).toEqual(["done", "done", "active"]);
      expect(within(dialog).getByText("Command run on Worker one")).toBeVisible();
      expect(within(dialog).getByText("Worker one connected from 192.0.2.99")).toBeVisible();
      expect(within(dialog).getByText("Confirm Worker one fingerprint")).toBeVisible();

      const cells = within(dialog)
        .getByRole("img", { name: "Fingerprint pattern" })
        .querySelectorAll("[data-color]");
      expect([...cells].map((cell) => cell.getAttribute("data-color"))).toEqual(
        // Golden vector A: blue, yellow and magenta fill the cells 1, 2 and 3.
        [
          "BLUE",
          "empty",
          "YELLOW",
          "empty",
          "BLUE",
          "YELLOW",
          "empty",
          "MAGENTA",
          "empty",
          "YELLOW",
          "MAGENTA",
          "empty",
          "empty",
          "empty",
          "MAGENTA",
          "BLUE",
          "MAGENTA",
          "MAGENTA",
          "MAGENTA",
          "BLUE",
          "YELLOW",
          "BLUE",
          "MAGENTA",
          "BLUE",
          "YELLOW",
        ],
      );

      const groups = within(dialog).getByText("8752").parentElement;
      expect(groups?.textContent).toBe(
        "87524a4a3c1f9e0b5d26e8f1a7c4093b6e2d51f8c0a97b34e1d6f2085ab9c7e3",
      );
      expect([...(groups?.children ?? [])].map((group) => group.textContent)).toEqual([
        "8752",
        "4a4a",
        "3c1f",
        "9e0b",
        "5d26",
        "e8f1",
        "a7c4",
        "093b",
        "6e2d",
        "51f8",
        "c0a9",
        "7b34",
        "e1d6",
        "f208",
        "5ab9",
        "c7e3",
      ]);

      const details = Object.fromEntries(
        within(dialog)
          .getAllByRole("term")
          .map((term) => [term.textContent, term.nextElementSibling?.textContent]),
      );
      expect(details).toEqual({
        Address: "192.0.2.10:50052",
        Runs: "Model · Simulated · linux/amd64 · gpu0",
      });
      expect(
        within(dialog).getByText("Connected from 192.0.2.99, which is not the address it gave."),
      ).toBeVisible();

      await user.click(within(dialog).getByRole("button", { name: "Approve Worker one" }));
      expect(join.spies.approveJoin).toHaveBeenCalledExactlyOnceWith({
        joinId: "join-1",
        proposalId: "proposal-1",
      });
      expect(within(dialog).getByRole("button", { name: "Approve Worker one" })).toBeDisabled();
      expect(within(dialog).getByRole("button", { name: "Reject" })).toBeDisabled();
    });

    it("rejects a proposal and logs the rejection while waiting again", async () => {
      const { dialog, join, user } = await openDialog();

      join.report(newMachine);
      await user.click(await within(dialog).findByRole("button", { name: "Reject" }));
      expect(join.spies.rejectJoin).toHaveBeenCalledExactlyOnceWith({
        joinId: "join-1",
        proposalId: "proposal-1",
      });

      join.report(waitingAfter({ case: "rejected", value: {} }, new Date(2030, 0, 1, 22, 10, 0)));
      expect(await within(dialog).findByText("You rejected Worker one.")).toBeVisible();
      expect(within(dialog).getByText("dotc node join --token token-1")).toBeVisible();
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
                  managerHost: "192.0.2.5",
                  previousHost: "192.0.2.11",
                  previousManagerHost: "",
                },
              },
            },
          },
        },
      });

      await within(dialog).findByText("Existing machine again");
      const details = Object.fromEntries(
        within(dialog)
          .getAllByRole("term")
          .map((term) => [term.textContent, term.nextElementSibling?.textContent]),
      );
      expect(details).toEqual({
        Address: "192.0.2.10:50052",
        Joining: "Existing machine again",
        "Previous address": "192.0.2.11",
        "Manager address": "declared to 192.0.2.5",
        "Deployments kept": "2",
      });
    });

    it("shows a decision that failed because dotc decided first and lets the operator retry", async () => {
      const { dialog, join, user } = await openDialog();
      join.spies.approveJoin.mockRejectedValueOnce(
        new ConnectError("proposal already decided", Code.FailedPrecondition),
      );

      join.report(newMachine);
      await user.click(await within(dialog).findByRole("button", { name: "Approve Worker one" }));

      expect(await within(dialog).findByText("proposal already decided")).toBeVisible();
      expect(within(dialog).getByRole("button", { name: "Approve Worker one" })).toBeEnabled();
    });
  });

  describe("added", () => {
    it("shows every step done and closes with Done", async () => {
      const { dialog, join, user } = await openDialog();

      join.report(newMachine);
      await user.click(await within(dialog).findByRole("button", { name: "Approve Worker one" }));
      join.report({ state: { case: "committed", value: { nodeId: "worker-one" } } });

      expect(await within(dialog).findByText("Worker one added to the site")).toBeVisible();
      expect(stepStatuses(dialog)).toEqual(["done", "done", "done"]);
      expect(within(dialog).getByText("Worker one connected from 192.0.2.99")).toBeVisible();

      // A committed join is final: closing leaves nothing to cancel.
      await user.click(within(dialog).getByRole("button", { name: "Done" }));
      await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
      expect(join.spies.cancelJoin).not.toHaveBeenCalled();
    });

    it("starts a new join from Add another", async () => {
      const { dialog, join, user } = await openDialog();

      join.report(newMachine);
      join.report({ state: { case: "committed", value: { nodeId: "worker-one" } } });
      await user.click(await within(dialog).findByRole("button", { name: "Add another" }));

      expect(await within(dialog).findByText("dotc node join --token token-2")).toBeVisible();
      expect(join.spies.startJoin).toHaveBeenCalledTimes(2);
      expect(stepStatuses(dialog)).toEqual(["active", "active", "todo"]);
      expect(join.spies.cancelJoin).not.toHaveBeenCalled();
    });
  });

  describe("expired", () => {
    it("shows the expiry and offers a new command", async () => {
      useFakeTimersWithWaits();
      vi.setSystemTime(new Date(2030, 0, 1, 14, 6, 0));
      const join = fakeSiteMembershipClient();
      renderWithClients(<Harness />, { siteMembershipClient: join.client });
      const dialog = await screen.findByRole("dialog", { name: "Add a machine" });
      await within(dialog).findByText("dotc node join --token token-1");

      join.report({ state: { case: "expired", value: {} } });

      expect(await within(dialog).findByText("Expired")).toBeVisible();
      expect(
        within(dialog).getByText(
          "This command expired at 14:21. A machine that runs it now is refused.",
        ),
      ).toBeVisible();
      expect(stepStatuses(dialog)).toEqual(["warning", "todo", "todo"]);
      expect(within(dialog).queryByRole("button", { name: "Copy join command" })).toBeNull();
      expect(within(dialog).getAllByRole("button", { name: "Close" })).toHaveLength(2);

      within(dialog).getByRole("button", { name: "Get a new command" }).click();
      expect(await within(dialog).findByText("dotc node join --token token-2")).toBeVisible();
      expect(join.spies.startJoin).toHaveBeenCalledTimes(2);
      expect(join.spies.cancelJoin).not.toHaveBeenCalled();
    });
  });

  it.each([
    ["cancelled", { state: { case: "cancelled" as const, value: {} } }, "The join was cancelled."],
    [
      "failed",
      { state: { case: "failed" as const, value: { reason: "too many failed attempts" } } },
      "too many failed attempts",
    ],
  ])("offers a new join when it is %s", async (_name, state, message) => {
    const { dialog, join, user } = await openDialog();

    join.report(state);
    expect(await within(dialog).findByText(message)).toBeVisible();

    await user.click(within(dialog).getByRole("button", { name: "Start again" }));
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

  it("shows why the join could not start and starts again", async () => {
    const join = fakeSiteMembershipClient();
    join.spies.startJoin.mockRejectedValueOnce(
      new ConnectError("too many active joins", Code.ResourceExhausted),
    );
    renderWithClients(<Harness />, { siteMembershipClient: join.client });

    const dialog = await screen.findByRole("dialog", { name: "Add a machine" });
    expect(await within(dialog).findByText("too many active joins")).toBeVisible();

    await setupUser().click(within(dialog).getByRole("button", { name: "Start again" }));
    expect(await within(dialog).findByText("dotc node join --token token-1")).toBeVisible();
  });

  it("reports a watch that ends without a final state and tries again", async () => {
    const { dialog, join, user } = await openDialog();

    join.end();

    expect(await within(dialog).findByText("Lost the connection to the manager.")).toBeVisible();
    await user.click(within(dialog).getByRole("button", { name: "Try again" }));
    expect(await within(dialog).findByText("dotc node join --token token-2")).toBeVisible();
  });
});
