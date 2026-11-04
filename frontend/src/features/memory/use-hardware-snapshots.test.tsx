import { create } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ResourceWatchProvider } from "@/app/resource-watch-provider";
import { MachineDetailContent } from "@/features/machines/machine-details";
import { idleMetricsClient } from "@/features/metrics/metric-test-support";
import {
  HardwareSnapshotSchema,
  HostFactsSchema,
  HostObservationSchema,
  ObservationUnavailableSchema,
} from "@/gen/dotcore/observation/v1/observation_pb";
import {
  CurrentHardwareSnapshotAvailabilitySchema,
  HardwareRefreshStateSchema,
  ListHardwareSnapshotsResponseSchema,
  MachineHardwareSnapshotSchema,
  MachineSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { listedMachine } from "./machine-test-support";
import { useHardwareSnapshots } from "./use-hardware-snapshots";

const machineOne = machine("one", "Machine one");
const machineTwo = machine("two", "Machine two");

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

function SnapshotProbe({ selectedMachine }: { selectedMachine?: ReturnType<typeof machine> }) {
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

function SitesProbe({
  enabled,
  machines,
}: {
  enabled?: boolean;
  machines: Array<ReturnType<typeof machine>>;
}) {
  const details = useHardwareSnapshots(machines, { enabled });
  return (
    <ul>
      {[...details].map(([machineId, detail]) => (
        <li data-testid={machineId} key={machineId}>
          {detail.kind}{" "}
          {detail.kind === "success"
            ? detail.snapshot.host?.result.case === "facts"
              ? detail.snapshot.host.result.value.hostName
              : ""
            : detail.kind === "error"
              ? detail.message
              : ""}
        </li>
      ))}
    </ul>
  );
}

function Console({ children, watch }: { children: React.ReactNode; watch: FakeResourceWatch }) {
  return (
    <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
      {children}
    </ResourceWatchProvider>
  );
}

function machine(machineId: string, displayName: string) {
  return create(MachineSchema, {
    currentHardwareSnapshotAvailability: create(CurrentHardwareSnapshotAvailabilitySchema, {
      availability: { case: "available", value: create(EmptySchema) },
    }),
    displayName,
    machineId,
  });
}

function snapshot(hostName: string) {
  return create(HardwareSnapshotSchema, {
    host: create(HostObservationSchema, {
      result: {
        case: "facts",
        value: create(HostFactsSchema, {
          availableMemoryBytes: 34359738368n,
          cpuCount: 16,
          hostName,
          operatingSystemOrKernel: "Linux 6.12",
          filesystems: [{ fstype: "ext4", mountpoint: "/", sizeBytes: 1099511627776n }],
          totalMemoryBytes: 68719476736n,
        }),
      },
    }),
    observedAt: { seconds: 1n },
  });
}

/** One machine's entry: its snapshot, if observed, and the outcome of its latest refresh. */
function entry(
  machineId: string,
  {
    hostName,
    refresh = hostName ? "succeeded" : "pending",
  }: { hostName?: string; refresh?: "pending" | "succeeded" | { code: string; message: string } },
) {
  return create(MachineHardwareSnapshotSchema, {
    machineId,
    refresh: create(HardwareRefreshStateSchema, {
      outcome:
        typeof refresh === "string"
          ? { case: refresh, value: create(EmptySchema) }
          : { case: "failed", value: create(ObservationUnavailableSchema, refresh) },
    }),
    snapshot: hostName ? snapshot(hostName) : undefined,
  });
}

function sendSnapshots(watch: FakeResourceWatch, ...machines: Array<ReturnType<typeof entry>>) {
  watch.payload("hardwareSnapshots", create(ListHardwareSnapshotsResponseSchema, { machines }));
}

async function flush() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
}

describe("useHardwareSnapshots", () => {
  it("shows loading until the watch delivers the machine, then its snapshot", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <SitesProbe machines={[machineOne, machineTwo]} />
      </Console>,
    );
    await flush();

    expect(watch.watchedKinds()).toEqual(["hardwareSnapshots"]);
    expect(screen.getByTestId("one")).toHaveTextContent("loading");
    expect(screen.getByTestId("two")).toHaveTextContent("loading");

    sendSnapshots(watch, entry("two", { hostName: "host-two" }));
    await flush();

    expect(screen.getByTestId("one")).toHaveTextContent("loading");
    expect(screen.getByTestId("two")).toHaveTextContent("success host-two");
  });

  it("maps each machine to its own entry whatever the entry order", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <SitesProbe machines={[machineOne, machineTwo]} />
      </Console>,
    );
    await flush();

    sendSnapshots(
      watch,
      entry("two", { hostName: "host-two" }),
      entry("one", { hostName: "host-one" }),
    );
    await flush();

    expect(screen.getByTestId("one")).toHaveTextContent("success host-one");
    expect(screen.getByTestId("two")).toHaveTextContent("success host-two");
  });

  it("shows a machine with a failed first refresh as an error and a pending one as loading", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <SitesProbe machines={[machineOne, machineTwo]} />
      </Console>,
    );
    await flush();

    sendSnapshots(
      watch,
      entry("one", { refresh: { code: "COLLECTION_FAILED", message: "collection failed" } }),
      entry("two", { refresh: "pending" }),
    );
    await flush();

    expect(screen.getByTestId("one")).toHaveTextContent("error collection failed");
    expect(screen.getByTestId("two")).toHaveTextContent("loading");
  });

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

    sendSnapshots(watch, entry("one", { hostName: "observed" }));
    await flush();
    expect(screen.getByText("observed")).toBeVisible();
    expect(screen.queryByText("Failed")).not.toBeInTheDocument();

    sendSnapshots(
      watch,
      entry("one", {
        hostName: "observed",
        refresh: { code: "COLLECTION_FAILED", message: "The latest collection attempt failed." },
      }),
    );
    await flush();
    expect(screen.getByText("observed")).toBeVisible();
    expect(screen.getByText("Failed")).toBeVisible();
    expect(screen.queryByText("The latest collection attempt failed.")).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    sendSnapshots(watch, entry("one", { hostName: "recovered" }));
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
    sendSnapshots(watch, entry("one", { hostName: "first-success" }));
    await flush();
    expect(screen.getByText("first-success")).toBeVisible();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    watch.close(new ConnectError("connection dropped", Code.Unavailable));
    await flush();
    expect(screen.getByText("first-success")).toBeVisible();
    expect(screen.getByText("Failed")).toBeVisible();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    await act(async () => vi.advanceTimersByTimeAsync(2_000));
    sendSnapshots(watch, entry("one", { hostName: "recovered" }));
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
      entry("one", { hostName: "host-one" }),
      entry("two", { hostName: "host-two" }),
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

  it("picks up a machine that becomes readable in a later inventory", async () => {
    const watch = new FakeResourceWatch();
    const { rerender } = render(
      <Console watch={watch}>
        <SitesProbe machines={[machineOne]} />
      </Console>,
    );
    await flush();
    sendSnapshots(
      watch,
      entry("one", { hostName: "host-one" }),
      entry("two", { hostName: "host-two" }),
    );
    await flush();
    expect(screen.queryByTestId("two")).not.toBeInTheDocument();

    rerender(
      <Console watch={watch}>
        <SitesProbe machines={[machineOne, machineTwo]} />
      </Console>,
    );
    await flush();

    expect(screen.getByTestId("one")).toHaveTextContent("success host-one");
    expect(screen.getByTestId("two")).toHaveTextContent("success host-two");
  });

  it("shows a machine without a readable snapshot as unavailable without waiting on the watch", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <SitesProbe machines={[listedMachine("down", "Down machine", 0)]} />
      </Console>,
    );
    await flush();

    expect(screen.getByTestId("down")).toHaveTextContent("unavailable");
  });

  it("does not watch snapshots while disabled", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <SitesProbe enabled={false} machines={[machineOne]} />
      </Console>,
    );
    await flush();

    expect(watch.requests).toHaveLength(0);
    expect(screen.getByTestId("one")).toHaveTextContent("loading");
  });
});
