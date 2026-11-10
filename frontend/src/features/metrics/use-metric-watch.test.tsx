import { create } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import { MetricAggregate, MetricReduce } from "@/gen/dotcore/metrics/v1/metric_pb";
import {
  type WatchMetricsRequest,
  type WatchMetricsResponse,
  WatchMetricsResponseSchema,
} from "@/gen/dotcore/metrics/v1/service_pb";
import { productionTiming, setTiming } from "@/lib/timing";

import type { MetricRange } from "./metric-ranges";
import { gaugePoint, metricSeries, queryResult } from "./metric-test-support";
import { type MetricHistoryQuery, useMetricWatch } from "./use-metric-watch";

const range: MetricRange = { durationMs: 900_000, label: "15m", stepMs: 1000 };
const queries: MetricHistoryQuery[] = [
  { groupBy: ["node"], matchers: { node: "node-a" }, metric: "node.host.memory.total" },
];
const twoQueries: MetricHistoryQuery[] = [
  ...queries,
  { groupBy: ["node"], matchers: { node: "node-a" }, metric: "node.host.memory.available" },
];

beforeEach(() => {
  setTiming(productionTiming);
  vi.useFakeTimers();
});

afterEach(() => {
  setVisibility("visible");
  vi.useRealTimers();
});

describe("useMetricWatch", () => {
  it("opens a watch with the range, step, reduce and aggregate", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();

    expect(watch.requests[0].queries[0].query).toMatchObject({
      aggregate: MetricAggregate.SUM,
      groupBy: ["node"],
      matchers: { node: "node-a" },
      metric: "node.host.memory.total",
      range: { case: "last", value: { seconds: 900n } },
      reduce: MetricReduce.LAST,
      stepHint: { seconds: 1n },
    });
  });

  it("sends the query's aggregate, defaulting to the sum", async () => {
    const watch = new FakeWatch();
    render(
      <Probe
        client={watch.client()}
        watched={[
          { ...queries[0], aggregate: MetricAggregate.AVG },
          { ...queries[0], aggregate: MetricAggregate.MAX },
          queries[0],
        ]}
      />,
    );
    await flush();

    expect(watch.requests[0].queries.map(({ query }) => query?.aggregate)).toEqual([
      MetricAggregate.AVG,
      MetricAggregate.MAX,
      MetricAggregate.SUM,
    ]);
  });

  it("publishes once for a batch of upserts and keeps unchanged grids", async () => {
    const watch = new FakeWatch();
    const seen: (readonly unknown[] | undefined)[] = [];
    render(<CountingProbe client={watch.client()} seen={seen} watched={twoQueries} />);
    await flush();
    await watch.upsert(0, [point(0, 5)], { stepCount: 2 }, "0");
    await watch.upsert(0, [point(0, 50)], { stepCount: 2 }, "1");
    await watch.backfillComplete();
    const before = published(seen);
    const firstGrids = seen[seen.length - 1];

    // Both queries answer in one batch, before the next frame is awaited.
    watch.push(upsertOf(0, [point(1, 6)], "0"));
    watch.push(upsertOf(0, [point(1, 51)], "1"));
    await flush();

    expect(published(seen) - before).toBe(1);
    expect(screen.getByTestId("probe")).toHaveTextContent("0:5,6 1:50,51");
    expect(firstGrids).toHaveLength(2);

    // An answer for one query leaves the other query's grid the same object.
    const grids = seen[seen.length - 1] as unknown[];
    watch.push(upsertOf(0, [point(1, 7)], "0"));
    await flush();
    const after = seen[seen.length - 1] as unknown[];
    expect(after[0]).not.toBe(grids[0]);
    expect(after[1]).toBe(grids[1]);
  });

  it("opens one stream carrying every query under its index key", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} watched={twoQueries} />);
    await flush();

    expect(watch.requests).toHaveLength(1);
    expect(watch.requests[0].queries.map(({ key, query }) => [key, query?.metric])).toEqual([
      ["0", "node.host.memory.total"],
      ["1", "node.host.memory.available"],
    ]);
  });

  it("routes an upsert to its key's grid only, once the stream backfill completes", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} watched={twoQueries} />);
    await flush();

    await watch.upsert(0, [point(0, 5)], { stepCount: 2 }, "0");
    await watch.upsert(0, [point(0, 50)], { stepCount: 2 }, "1");
    expect(screen.getByTestId("probe")).toHaveTextContent("idle");

    await watch.backfillComplete();
    expect(screen.getByTestId("probe")).toHaveTextContent("0:5,_ 1:50,_");

    await watch.upsert(0, [point(1, 51)], { stepCount: 2 }, "1");
    expect(screen.getByTestId("probe")).toHaveTextContent("0:5,_ 1:50,51");
  });

  it("reopens a single stream for every query after a failure", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} watched={twoQueries} />);
    await flush();
    await watch.fail(new ConnectError("lagged", Code.Aborted));

    await act(() => vi.advanceTimersByTimeAsync(1000));

    expect(watch.requests).toHaveLength(2);
    expect(watch.requests[1]).toEqual(watch.requests[0]);
  });

  it.each([Code.InvalidArgument, Code.NotFound])(
    "shows the failure without reopening on %s",
    async (code) => {
      const watch = new FakeWatch();
      render(<Probe client={watch.client()} watched={twoQueries} />);
      await flush();
      await watch.upsert(0, [point(0, 5)], { stepCount: 2 }, "0");

      await watch.fail(new ConnectError("unknown metric", code));
      await act(() => vi.advanceTimersByTimeAsync(60_000));

      expect(watch.requests).toHaveLength(1);
      expect(screen.getByTestId("probe")).toHaveTextContent("failed");
    },
  );

  it("stays failed through a hide and show after a rejected query", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();
    await watch.fail(new ConnectError("unknown metric", Code.NotFound));

    setVisibility("hidden");
    await flush();
    setVisibility("visible");
    await flush();
    await act(() => vi.advanceTimersByTimeAsync(60_000));

    expect(watch.requests).toHaveLength(1);
  });

  it("reopens on resource exhaustion", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();
    await watch.fail(new ConnectError("cap", Code.ResourceExhausted));

    await act(() => vi.advanceTimersByTimeAsync(1000));

    expect(watch.requests).toHaveLength(2);
  });

  it("shows the grid once the backfill completes, then follows upserts", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();

    await watch.upsert(0, [point(0, 5), point(1, 6)], { stepCount: 3 });
    expect(screen.getByTestId("probe")).toHaveTextContent("idle");

    await watch.backfillComplete();
    expect(screen.getByTestId("probe")).toHaveTextContent("failed=false incarnation-a|5,6,_");

    await watch.upsert(0, [point(2, 7)], { stepCount: 3 });
    expect(screen.getByTestId("probe")).toHaveTextContent("failed=false incarnation-a|5,6,7");
  });

  it("replaces a revised point and drops points before the sliding range start", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();
    await watch.upsert(0, [point(0, 5), point(1, 6), point(2, 7)], { stepCount: 3 });
    await watch.backfillComplete();

    await watch.upsert(1, [point(2, 8), point(3, 9)], { stepCount: 3 });

    expect(screen.getByTestId("probe")).toHaveTextContent("incarnation-a|6,8,9");
  });

  it("merges the full backfill of a reopen without duplicate or missing buckets", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();
    await watch.upsert(0, [point(0, 5), point(1, 6)], { stepCount: 4 });
    await watch.backfillComplete();

    await watch.fail(new ConnectError("connection lost", Code.Unavailable));
    expect(screen.getByTestId("probe")).toHaveTextContent("failed=true incarnation-a|5,6,_,_");

    await act(() => vi.advanceTimersByTimeAsync(1000));
    expect(watch.requests).toHaveLength(2);
    expect(watch.requests[1]).toEqual(watch.requests[0]);
    // The reopen keeps showing the held points until its backfill arrives.
    expect(screen.getByTestId("probe")).toHaveTextContent("incarnation-a|5,6,_,_");

    // The full backfill repeats every bucket, with a late revision of an old one and the missed ones.
    await watch.upsert(0, [point(0, 15), point(1, 6), point(2, 7), point(3, 8)], { stepCount: 4 });
    await watch.backfillComplete();
    expect(screen.getByTestId("probe")).toHaveTextContent("failed=false incarnation-a|15,6,7,8");
  });

  it("reopens a cleanly ended stream with the same request", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();
    await watch.upsert(0, [point(0, 5)], { stepCount: 2 });
    await watch.backfillComplete();

    await watch.end();
    await act(() => vi.advanceTimersByTimeAsync(1000));

    expect(watch.requests).toHaveLength(2);
    expect(watch.requests[1]).toEqual(watch.requests[0]);
  });

  it("replaces all data when a new incarnation arrives", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();
    await watch.upsert(0, [point(0, 5), point(1, 6)], { stepCount: 3 });
    await watch.backfillComplete();

    await watch.upsert(0, [point(2, 9)], {
      incarnationId: "incarnation-b",
      startedAtOffset: 2,
      stepCount: 3,
    });

    expect(screen.getByTestId("probe")).toHaveTextContent("incarnation-b|_,_,9");
  });

  it("slides the covered range on a point-less upsert", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();
    await watch.upsert(0, [point(0, 5), point(1, 6), point(2, 7)], { stepCount: 3 });
    await watch.backfillComplete();

    await watch.upsert(2, [], { stepCount: 3 });

    expect(screen.getByTestId("probe")).toHaveTextContent("incarnation-a|7,_,_");
  });

  it("keeps backing off while the server keeps dropping a lagging watcher", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();

    for (const delayMs of [1000, 2000, 5000, 5000]) {
      await watch.upsert(0, [point(0, 5)], { stepCount: 2 });
      await watch.backfillComplete();
      await watch.upsert(0, [], { stepCount: 2 });
      await watch.fail(new ConnectError("lagged", Code.Aborted));

      await act(() => vi.advanceTimersByTimeAsync(delayMs - 1));
      const opened = watch.requests.length;
      await act(() => vi.advanceTimersByTimeAsync(1));
      expect(watch.requests.length).toBe(opened + 1);
    }
  });

  it("restarts the backoff after a stream stayed healthy", async () => {
    const watch = new FakeWatch();
    render(<Probe client={watch.client()} />);
    await flush();
    await watch.upsert(0, [point(0, 5)], { stepCount: 2 });
    await watch.backfillComplete();
    await watch.fail(new ConnectError("lost", Code.Unavailable));
    await act(() => vi.advanceTimersByTimeAsync(1000));
    await watch.upsert(0, [point(0, 5)], { stepCount: 2 });
    await watch.backfillComplete();
    await act(() => vi.advanceTimersByTimeAsync(5000));
    await watch.upsert(0, [], { stepCount: 2 });

    await watch.fail(new ConnectError("lost", Code.Unavailable));
    const opened = watch.requests.length;
    await act(() => vi.advanceTimersByTimeAsync(1000));

    expect(watch.requests.length).toBe(opened + 1);
  });

  it("keeps the last grids while paused, then reopens and keeps them until the next answer", async () => {
    const watch = new FakeWatch();
    const { rerender } = render(<Probe client={watch.client()} />);
    await flush();
    await watch.upsert(0, [point(0, 5), point(1, 6)], { stepCount: 2 });
    await watch.backfillComplete();

    rerender(<Probe client={watch.client()} paused />);
    await flush();
    expect(watch.signals[0].aborted).toBe(true);
    expect(screen.getByTestId("probe")).toHaveTextContent("failed=false incarnation-a|5,6");

    rerender(<Probe client={watch.client()} />);
    await flush();
    expect(watch.requests).toHaveLength(2);
    expect(screen.getByTestId("probe")).toHaveTextContent("failed=false incarnation-a|5,6");

    await watch.upsert(0, [point(0, 6), point(1, 7)], { stepCount: 2 });
    await watch.backfillComplete();
    expect(screen.getByTestId("probe")).toHaveTextContent("failed=false incarnation-a|6,7");
  });

  it("closes the stream on unmount", async () => {
    const watch = new FakeWatch();
    const { unmount } = render(<Probe client={watch.client()} />);
    await flush();

    unmount();

    expect(watch.signals[0].aborted).toBe(true);
  });
});

function point(offset: number, value: number) {
  return gaugePoint(offset, value);
}

function CountingProbe({
  client,
  seen,
  watched,
}: {
  client: MetricsClient;
  seen: (readonly unknown[] | undefined)[];
  watched: MetricHistoryQuery[];
}) {
  const watch = useMetricWatch(client, watched, range);
  seen.push(watch?.value);

  return (
    <output data-testid="probe">
      {watch?.value?.map((grid, i) => `${i}:${grid.series[0].values.join(",")}`).join(" ")}
    </output>
  );
}

/** How many distinct grid sets the hook has returned, which is one per publish. */
function published(seen: readonly (readonly unknown[] | undefined)[]): number {
  return new Set(seen.filter(Boolean)).size;
}

function upsertOf(startOffset: number, points: ReturnType<typeof gaugePoint>[], key: string) {
  return create(WatchMetricsResponseSchema, {
    event: {
      case: "upsert",
      value: {
        key,
        result: queryResult({
          series: [metricSeries({ node: "node-a" }, points)],
          startOffset,
          stepCount: 2,
        }),
      },
    },
  });
}

function Probe({
  client,
  paused,
  watched = queries,
}: {
  client: MetricsClient;
  paused?: boolean;
  watched?: MetricHistoryQuery[];
}) {
  const watch = useMetricWatch(client, watched, range, { paused });
  const grids = watch?.value;
  const values = grids?.map((grid) =>
    grid.series[0]?.values.map((value) => value ?? "_").join(","),
  );
  const grid = grids?.[0];
  let text = watch?.failed ? "failed" : "idle";
  if (grid && values) {
    text =
      grids.length === 1
        ? `failed=${watch?.failed} ${grid.incarnation.id}|${values[0]}`
        : values.map((v, i) => `${i}:${v}`).join(" ");
  }

  return <output data-testid="probe">{text}</output>;
}

/** A fake watch endpoint: each call opens a stream the test pushes responses into. */
class FakeWatch {
  readonly requests: WatchMetricsRequest[] = [];
  readonly signals: AbortSignal[] = [];
  private current: {
    close: (error?: Error) => void;
    push: (response: WatchMetricsResponse) => void;
  } = { close: () => {}, push: () => {} };

  client(): MetricsClient {
    return {
      metrics: {
        listMetricDescriptors: vi.fn(),
        watchMetrics: (request, options) => {
          this.requests.push(request as WatchMetricsRequest);
          this.signals.push(options?.signal ?? new AbortController().signal);
          return this.open();
        },
      },
    };
  }

  async upsert(
    startOffset: number,
    points: ReturnType<typeof gaugePoint>[],
    options: Parameters<typeof queryResult>[0] extends infer T
      ? Omit<T & object, "series" | "startOffset">
      : never,
    key = "0",
  ) {
    this.current.push(
      create(WatchMetricsResponseSchema, {
        event: {
          case: "upsert",
          value: {
            key,
            result: queryResult({
              ...options,
              series: [metricSeries({ node: "node-a" }, points)],
              startOffset,
            }),
          },
        },
      }),
    );
    await flush();
  }

  /** Queues a response without yielding, so several arrive within one flush. */
  push(response: WatchMetricsResponse) {
    this.current.push(response);
  }

  async backfillComplete() {
    this.current.push(
      create(WatchMetricsResponseSchema, {
        event: { case: "backfillComplete", value: create(EmptySchema) },
      }),
    );
    await flush();
  }

  async fail(error: Error) {
    this.current.close(error);
    await flush();
  }

  async end() {
    this.current.close();
    await flush();
  }

  private open(): AsyncIterable<WatchMetricsResponse> {
    const queue: WatchMetricsResponse[] = [];
    let wake: (() => void) | undefined;
    let closed: { error?: Error } | undefined;
    this.current = {
      close: (error) => {
        closed = { error };
        wake?.();
      },
      push: (response) => {
        queue.push(response);
        wake?.();
      },
    };

    return (async function* () {
      for (;;) {
        const next = queue.shift();
        if (next) {
          yield next;
        } else if (closed) {
          if (closed.error) throw closed.error;
          return;
        } else {
          await new Promise<void>((resolve) => (wake = resolve));
        }
      }
    })();
  }
}

function setVisibility(state: "hidden" | "visible") {
  Object.defineProperty(document, "visibilityState", { configurable: true, value: state });
  document.dispatchEvent(new Event("visibilitychange"));
}

async function flush() {
  await act(() => vi.advanceTimersByTimeAsync(0));
}
