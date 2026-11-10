import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { sendSnapshots, snapshotEntry, snapshotMachine } from "./hardware-snapshot-test-support";
import { listedMachine } from "./machine-test-support";
import { useHardwareSnapshots } from "./use-hardware-snapshots";

const machineOne = snapshotMachine("one", "Machine one");
const machineTwo = snapshotMachine("two", "Machine two");

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

function SitesProbe({
  enabled,
  machines,
}: {
  enabled?: boolean;
  machines: Array<ReturnType<typeof snapshotMachine>>;
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
  return <ResourceWatchProvider client={watch.client()}>{children}</ResourceWatchProvider>;
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

    sendSnapshots(watch, snapshotEntry("two", { hostName: "host-two" }));
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
      snapshotEntry("two", { hostName: "host-two" }),
      snapshotEntry("one", { hostName: "host-one" }),
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
      snapshotEntry("one", {
        refresh: { code: "COLLECTION_FAILED", message: "collection failed" },
      }),
      snapshotEntry("two", { refresh: "pending" }),
    );
    await flush();

    expect(screen.getByTestId("one")).toHaveTextContent("error collection failed");
    expect(screen.getByTestId("two")).toHaveTextContent("loading");
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
      snapshotEntry("one", { hostName: "host-one" }),
      snapshotEntry("two", { hostName: "host-two" }),
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
