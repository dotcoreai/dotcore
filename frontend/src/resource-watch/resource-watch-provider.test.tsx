import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ListRoutesResponseSchema } from "@/gen/dotcore/inference/v1/routes_pb";
import { ListMachinesResponseSchema } from "@/gen/dotcore/site/v1/service_pb";
import { productionTiming, setTiming } from "@/lib/timing";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import {
  type ResourceWatchRead,
  useManagerSession,
  useWatchedMachines,
  useWatchedRoutes,
} from "./resource-watch";
import { ResourceWatchProvider } from "./resource-watch-provider";

beforeEach(() => {
  setTiming(productionTiming);
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("ResourceWatchProvider", () => {
  it("opens one subscription for every component watching the same kind", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <MachinesProbe id="a" />
        <MachinesProbe id="b" />
      </Console>,
    );
    await settle();

    expect(watch.requests).toHaveLength(1);
    expect(watch.requests[0].subscriptions.map(({ key, kind }) => [key, kind.case])).toEqual([
      ["machines", "machines"],
    ]);

    watch.payload("machines", machines("node-a"));
    await flush();
    expect(screen.getByTestId("a")).toHaveTextContent("ok node-a");
    expect(screen.getByTestId("b")).toHaveTextContent("ok node-a");
  });

  it("reopens at once with the union when components add a kind", async () => {
    const watch = new FakeResourceWatch();
    const { rerender } = render(
      <Console watch={watch}>
        <MachinesProbe id="machines" />
      </Console>,
    );
    await settle();
    watch.payload("machines", machines("node-a"));
    await flush();

    rerender(
      <Console watch={watch}>
        <MachinesProbe id="machines" />
        <RoutesProbe id="routes" />
        <RoutesProbe id="routes-2" />
      </Console>,
    );
    await flush();

    expect(watch.requests).toHaveLength(2);
    expect(watch.signals[0].aborted).toBe(true);
    expect(watch.watchedKinds()).toEqual(["machines", "routes"]);
    // The kept kind stays in view across the reopen; the added one loads.
    expect(screen.getByTestId("machines")).toHaveTextContent("ok node-a");
    expect(screen.getByTestId("routes")).toHaveTextContent("loading");
  });

  it("closes the stream and forgets a kind once nothing watches it", async () => {
    const watch = new FakeResourceWatch();
    const { rerender } = render(
      <Console watch={watch}>
        <MachinesProbe id="machines" />
        <RoutesProbe id="routes" />
      </Console>,
    );
    await settle();
    watch.payload("routes", create(ListRoutesResponseSchema, { routes: [{}, {}] }));
    await flush();
    expect(screen.getByTestId("routes")).toHaveTextContent("ok 2 routes");

    rerender(
      <Console watch={watch}>
        <MachinesProbe id="machines" />
      </Console>,
    );
    // Only removing a kind waits, so the open stream survives a quick release and re-register.
    await flush();
    expect(watch.requests).toHaveLength(1);
    await settle();
    expect(watch.watchedKinds()).toEqual(["machines"]);

    rerender(<Console watch={watch}>{null}</Console>);
    await settle();
    expect(watch.requests).toHaveLength(2);
    expect(watch.signals[1].aborted).toBe(true);

    rerender(
      <Console watch={watch}>
        <RoutesProbe id="routes" />
      </Console>,
    );
    expect(screen.getByTestId("routes")).toHaveTextContent("loading");
  });

  it("does not watch a disabled kind", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <MachinesProbe enabled={false} id="machines" />
      </Console>,
    );
    await settle();

    expect(watch.requests).toHaveLength(0);
    expect(screen.getByTestId("machines")).toHaveTextContent("disabled");
  });

  it("replaces a kind's value with each payload", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <MachinesProbe id="machines" />
      </Console>,
    );
    await settle();
    expect(screen.getByTestId("machines")).toHaveTextContent("loading");

    watch.payload("machines", machines("node-a", "node-b"));
    watch.snapshotComplete();
    await flush();
    expect(screen.getByTestId("machines")).toHaveTextContent("ok node-a,node-b");

    watch.payload("machines", machines("node-b"));
    await flush();
    expect(screen.getByTestId("machines")).toHaveTextContent("ok node-b");
  });

  it("resets every kind and counts a generation when the manager session changes", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <MachinesProbe id="machines" />
        <RoutesProbe id="routes" />
        <SessionProbe />
      </Console>,
    );
    await settle();
    watch.payload("machines", machines("node-a"));
    watch.payload("routes", create(ListRoutesResponseSchema, { routes: [{}] }));
    await flush();
    expect(screen.getByTestId("session")).toHaveTextContent("session-a 0");

    watch.close(new ConnectError("manager stopped", Code.Unavailable));
    await flush();
    await act(() => vi.advanceTimersByTimeAsync(productionTiming.pollRetryMs[0]));
    expect(watch.requests).toHaveLength(2);

    watch.sessionId = "session-b";
    watch.payload("machines", machines("node-c"));
    await flush();
    expect(screen.getByTestId("session")).toHaveTextContent("session-b 1");
    expect(screen.getByTestId("machines")).toHaveTextContent("ok node-c");
    expect(screen.getByTestId("routes")).toHaveTextContent("loading");
  });

  it("keeps the last value and marks failed until the reopened stream answers", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <MachinesProbe id="machines" />
      </Console>,
    );
    await settle();
    watch.payload("machines", machines("node-a"));
    watch.snapshotComplete();
    await flush();

    watch.close(new ConnectError("lagged", Code.Aborted));
    await flush();
    expect(screen.getByTestId("machines")).toHaveTextContent("failed node-a");

    await act(() => vi.advanceTimersByTimeAsync(productionTiming.pollRetryMs[0]));
    expect(watch.requests).toHaveLength(2);
    watch.payload("machines", machines("node-a", "node-b"));
    await flush();
    expect(screen.getByTestId("machines")).toHaveTextContent("ok node-a,node-b");
  });

  it("stops reopening after a rejected request", async () => {
    const watch = new FakeResourceWatch();
    render(
      <Console watch={watch}>
        <MachinesProbe id="machines" />
      </Console>,
    );
    await settle();

    watch.close(new ConnectError("too many subscriptions", Code.InvalidArgument));
    await flush();
    // Long enough for every retry of the backoff.
    await act(() =>
      vi.advanceTimersByTimeAsync(productionTiming.pollRetryMs.reduce((sum, ms) => sum + ms)),
    );

    expect(watch.requests).toHaveLength(1);
    expect(screen.getByTestId("machines")).toHaveTextContent("failed none");
  });
});

// The provider reads its client once at mount, so a rerender's new client is ignored.
function Console({ children, watch }: { children: ReactNode; watch: FakeResourceWatch }) {
  return <ResourceWatchProvider client={watch.client()}>{children}</ResourceWatchProvider>;
}

function MachinesProbe({ enabled, id }: { enabled?: boolean; id: string }) {
  const read = useWatchedMachines({ enabled });
  return (
    <span data-testid={id}>
      {formatRead(read, (value) => value.machines.map(({ machineId }) => machineId).join(","))}
    </span>
  );
}

function RoutesProbe({ id }: { id: string }) {
  const read = useWatchedRoutes();
  return (
    <span data-testid={id}>{formatRead(read, (value) => `${value.routes.length} routes`)}</span>
  );
}

function SessionProbe() {
  const { generation, id } = useManagerSession();
  return <span data-testid="session">{`${id ?? "none"} ${generation}`}</span>;
}

function formatRead<T>(read: ResourceWatchRead<T> | undefined, format: (value: T) => string) {
  if (!read) return "disabled";
  if (!read.value) return read.failed ? "failed none" : "loading";
  return `${read.failed ? "failed" : "ok"} ${format(read.value)}`;
}

function machines(...ids: string[]) {
  return create(ListMachinesResponseSchema, { machines: ids.map((machineId) => ({ machineId })) });
}

/** Lets the debounced reopen run and the opened stream start. */
async function settle() {
  await act(() => vi.advanceTimersByTimeAsync(productionTiming.resourceWatchReopenMs));
}

async function flush() {
  await act(() => vi.advanceTimersByTimeAsync(0));
}
