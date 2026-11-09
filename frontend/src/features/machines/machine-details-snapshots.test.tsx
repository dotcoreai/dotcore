import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  sendSnapshots,
  snapshotEntry,
  snapshotMachine,
} from "@/features/memory/hardware-snapshot-test-support";
import { useHardwareSnapshots } from "@/features/memory/use-hardware-snapshots";
import { idleMetricsClient } from "@/features/metrics/metric-test-support";
import {
  ListHardwareSnapshotsResponseSchema,
  MachineHardwareSnapshotSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { MachineDetailContent } from "./machine-details";

const machineOne = snapshotMachine("one", "Machine one");
const machineTwo = snapshotMachine("two", "Machine two");

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("machine details from hardware snapshots", () => {
  it("renders an entry with neither snapshot nor refresh state as a protocol error", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <SnapshotProbe selectedMachine={machineOne} />
      </Console>,
    );
    await flush();

    watch.payload(
      "hardwareSnapshots",
      create(ListHardwareSnapshotsResponseSchema, {
        machines: [create(MachineHardwareSnapshotSchema, { machineId: "one" })],
      }),
    );
    await flush();

    expect(screen.getByRole("alert")).toHaveTextContent(
      "Current hardware snapshot response protocol error: missing snapshot.",
    );
  });

  it("retains a snapshot through a backend refresh failure and shows the recovery", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <SnapshotProbe selectedMachine={machineOne} />
      </Console>,
    );
    await flush();

    sendSnapshots(watch, snapshotEntry("one", { hostName: "observed" }));
    await flush();
    expect(screen.getByText("observed")).toBeVisible();
    expect(screen.queryByText("Failed")).not.toBeInTheDocument();

    sendSnapshots(
      watch,
      snapshotEntry("one", {
        hostName: "observed",
        refresh: { code: "COLLECTION_FAILED", message: "The latest collection attempt failed." },
      }),
    );
    await flush();
    expect(screen.getByText("observed")).toBeVisible();
    expect(screen.getByText("Failed")).toBeVisible();
    expect(screen.queryByText("The latest collection attempt failed.")).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    sendSnapshots(watch, snapshotEntry("one", { hostName: "recovered" }));
    await flush();
    expect(screen.getByText("recovered")).toBeVisible();
    expect(screen.queryByText("Failed")).not.toBeInTheDocument();
  });

  it("shows an error before the first payload when the watch is down, then retains and recovers the latest snapshot", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <SnapshotProbe selectedMachine={machineOne} />
      </Console>,
    );
    await flush();

    watch.close(new ConnectError("backend unavailable", Code.Unavailable));
    await flush();
    expect(screen.getByRole("alert")).toHaveTextContent(
      "The hardware snapshot watch is unavailable.",
    );

    // The loop reopens the stream after a backoff step.
    await act(async () => vi.advanceTimersByTimeAsync(1_000));
    sendSnapshots(watch, snapshotEntry("one", { hostName: "first-success" }));
    await flush();
    expect(screen.getByText("first-success")).toBeVisible();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    watch.close(new ConnectError("connection dropped", Code.Unavailable));
    await flush();
    expect(screen.getByText("first-success")).toBeVisible();
    expect(screen.getByText("Failed")).toBeVisible();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    await act(async () => vi.advanceTimersByTimeAsync(2_000));
    sendSnapshots(watch, snapshotEntry("one", { hostName: "recovered" }));
    await flush();
    expect(screen.getByText("recovered")).toBeVisible();
    expect(screen.queryByText("Failed")).not.toBeInTheDocument();
  });

  it("follows the selected machine and drops the watch once none is selected", async () => {
    const watch = new FakeResourceWatch();
    const { rerender } = render(
      <Console watch={watch}>
        <SnapshotProbe selectedMachine={machineOne} />
      </Console>,
    );
    await flush();
    sendSnapshots(
      watch,
      snapshotEntry("one", { hostName: "host-one" }),
      snapshotEntry("two", { hostName: "host-two" }),
    );
    await flush();
    expect(screen.getByText("host-one")).toBeVisible();

    rerender(
      <Console watch={watch}>
        <SnapshotProbe selectedMachine={machineTwo} />
      </Console>,
    );
    await flush();
    expect(screen.getByText("host-two")).toBeVisible();
    expect(screen.queryByText("host-one")).not.toBeInTheDocument();

    rerender(
      <Console watch={watch}>
        <SnapshotProbe />
      </Console>,
    );
    await flush();
    expect(screen.getByText("Closed")).toBeVisible();
  });
});

function SnapshotProbe({
  selectedMachine,
}: {
  selectedMachine?: ReturnType<typeof snapshotMachine>;
}) {
  const details = useHardwareSnapshots(selectedMachine ? [selectedMachine] : []);
  const detail = selectedMachine ? details.get(selectedMachine.machineId) : undefined;
  return detail ? (
    <MachineDetailContent
      metricsClient={idleMetricsClient()}
      detail={detail}
      machineId={selectedMachine?.machineId ?? ""}
    />
  ) : (
    <output>Closed</output>
  );
}

function Console({ children, watch }: { children: ReactNode; watch: FakeResourceWatch }) {
  return (
    <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
      {children}
    </ResourceWatchProvider>
  );
}

async function flush() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
}
