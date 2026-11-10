import { create } from "@bufbuild/protobuf";
import { act, render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import {
  ListMetricDescriptorsResponseSchema,
  MetricResolutionSchema,
  WatchMetricsResponseSchema,
} from "@/gen/dotcore/metrics/v1/service_pb";
import { ListMachinesResponseSchema } from "@/gen/dotcore/site/v1/service_pb";
import { useWatchedMachines } from "@/resource-watch/resource-watch";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import { metricDescriptorsFor } from "./metric-descriptors";
import { queryResult } from "./metric-test-support";
import { useMetricRanges } from "./use-metric-ranges";
import { useMetricWatch } from "./use-metric-watch";

describe("metric descriptors", () => {
  it("shares one read between panels and reads again once for a new store incarnation", async () => {
    const { client, listMetricDescriptors } = descriptorsClient();
    render(
      <>
        <Ranges id="a" client={client} />
        <Ranges id="b" client={client} />
      </>,
    );

    await screen.findByText("a: 15m");
    expect(screen.getByTestId("b")).toHaveTextContent("b: 15m");
    expect(listMetricDescriptors).toHaveBeenCalledTimes(1);

    // The first incarnation seen is the baseline; repeating it reads nothing.
    const store = metricDescriptorsFor(client);
    store.observeIncarnation("incarnation-a");
    store.observeIncarnation("incarnation-a");
    await flush();
    expect(listMetricDescriptors).toHaveBeenCalledTimes(1);

    store.observeIncarnation("incarnation-b");
    store.observeIncarnation("incarnation-b");
    await flush();
    expect(listMetricDescriptors).toHaveBeenCalledTimes(2);
    expect(screen.getByTestId("a")).toHaveTextContent("a: 6h");
    expect(screen.getByTestId("b")).toHaveTextContent("b: 6h");
  });

  it("reads again once when the resource watch reports a new manager session", async () => {
    const { client, listMetricDescriptors } = descriptorsClient();
    const watch = new FakeResourceWatch();
    render(
      <ResourceWatchProvider client={watch.client()}>
        <Machines />
        <Ranges id="a" client={client} />
        <Ranges id="b" client={client} />
      </ResourceWatchProvider>,
    );
    await screen.findByText("a: 15m");
    await vi.waitFor(() => expect(watch.requests).toHaveLength(1));
    watch.payload("machines", create(ListMachinesResponseSchema, {}));
    await flush();
    expect(listMetricDescriptors).toHaveBeenCalledTimes(1);

    // Same session again: nothing to read.
    watch.payload("machines", create(ListMachinesResponseSchema, {}));
    await flush();
    expect(listMetricDescriptors).toHaveBeenCalledTimes(1);

    watch.sessionId = "session-b";
    watch.payload("machines", create(ListMachinesResponseSchema, {}));
    await vi.waitFor(() => expect(listMetricDescriptors).toHaveBeenCalledTimes(2));
    expect(screen.getByTestId("a")).toHaveTextContent("a: 6h");
    expect(screen.getByTestId("b")).toHaveTextContent("b: 6h");
  });

  it("reads again when a metric watch delivers a new store incarnation", async () => {
    const { client, listMetricDescriptors } = descriptorsClient();
    let release: () => void = () => {};
    const gate = new Promise<void>((resolve) => (release = resolve));
    const upsert = (incarnationId: string) =>
      create(WatchMetricsResponseSchema, {
        event: {
          case: "upsert",
          value: { key: "0", result: queryResult({ incarnationId, series: [], stepCount: 1 }) },
        },
      });
    client.metrics.watchMetrics = () =>
      (async function* () {
        yield upsert("incarnation-a");
        await gate;
        yield upsert("incarnation-b");
        yield upsert("incarnation-b");
        // The watch stays open, so no reopen replays the first incarnation.
        await new Promise<never>(() => {});
      })();
    render(<Panel client={client} />);

    await screen.findByText("a: 15m");
    expect(listMetricDescriptors).toHaveBeenCalledTimes(1);

    release();
    await vi.waitFor(() => expect(listMetricDescriptors).toHaveBeenCalledTimes(2));
    await flush();
    expect(listMetricDescriptors).toHaveBeenCalledTimes(2);
    expect(screen.getByTestId("a")).toHaveTextContent("a: 6h");
  });

  it("keeps the read across panels unmounting and mounting", async () => {
    const { client, listMetricDescriptors } = descriptorsClient();
    const { rerender } = render(<Ranges id="a" client={client} />);
    await screen.findByText("a: 15m");

    rerender(<></>);
    rerender(<Ranges id="a" client={client} />);

    expect(screen.getByTestId("a")).toHaveTextContent("a: 15m");
    expect(listMetricDescriptors).toHaveBeenCalledTimes(1);
  });
});

/** A client whose descriptors list one range, a different one after each read but the first. */
function descriptorsClient() {
  let reads = 0;
  const listMetricDescriptors = vi.fn(() => {
    reads += 1;
    return Promise.resolve(
      create(ListMetricDescriptorsResponseSchema, {
        resolutions: [
          create(MetricResolutionSchema, {
            retention: { seconds: reads === 1 ? 900n : 21_600n },
            step: { seconds: 1n },
          }),
        ],
      }),
    );
  });
  const client: MetricsClient = {
    metrics: { listMetricDescriptors, watchMetrics: () => (async function* () {})() },
  };

  return { client, listMetricDescriptors };
}

function Ranges({ client, id }: { client: MetricsClient; id: string }): ReactNode {
  const ranges = useMetricRanges(client);

  return (
    <p
      data-testid={id}
    >{`${id}: ${ranges?.value?.map((range) => range.label).join(",") ?? "none"}`}</p>
  );
}

function Panel({ client }: { client: MetricsClient }): ReactNode {
  useMetricWatch(client, [{ groupBy: [], matchers: {}, metric: "cpu" }], {
    durationMs: 900_000,
    label: "current",
    stepMs: 1000,
  });

  return <Ranges client={client} id="a" />;
}

function Machines(): ReactNode {
  useWatchedMachines();

  return null;
}

async function flush() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 10));
  });
}
