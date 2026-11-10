import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import { setupUser } from "@/test/user";

import {
  gaugePoint,
  idleMetricsClient,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "./metric-test-support";
import { type MetricQueryGroup, useMetricPanel } from "./use-metric-panel";

const groups: MetricQueryGroup[] = [
  {
    current: [],
    history: [{ groupBy: [], matchers: {}, metric: "memory.used" }],
    id: "memory",
  },
  {
    current: [
      { groupBy: [], matchers: {}, metric: "cpu" },
      { groupBy: [], matchers: {}, metric: "cpu.max" },
    ],
    history: [{ groupBy: [], matchers: {}, metric: "cpu" }],
    id: "cpu",
  },
];

describe("useMetricPanel", () => {
  it("watches current values only until a row expands, then every group's history at the finest range", async () => {
    const user = setupUser();
    const watchMetrics = vi.fn(staticWatch(answer));
    render(<Harness client={metricsClientWith(watchMetrics)} />);

    await screen.findByText("cpu current: cpu, cpu.max");
    expect(openStreams(watchMetrics)).toEqual([["cpu", "cpu.max"]]);
    expect(screen.getByTestId("memory-history")).toHaveTextContent("none");

    await user.click(screen.getByRole("button", { name: "Toggle memory" }));
    await screen.findByText("memory history: memory.used");
    expect(openStreams(watchMetrics)).toEqual([
      ["cpu", "cpu.max"],
      ["memory.used", "cpu"],
    ]);
    const history = lastRequest(watchMetrics);
    expect(history.queries?.[0].query?.stepHint?.seconds).toBe(1n);
    expect(history.queries?.[0].query?.range).toMatchObject({
      case: "last",
      value: { seconds: 900n },
    });

    // Expanding another row reads the same stream.
    await user.click(screen.getByRole("button", { name: "Toggle cpu" }));
    expect(watchMetrics).toHaveBeenCalledTimes(2);

    await user.click(screen.getByRole("button", { name: "Toggle memory" }));
    await user.click(screen.getByRole("button", { name: "Toggle cpu" }));
    await waitFor(() => expect(openStreams(watchMetrics)).toEqual([["cpu", "cpu.max"]]));
    expect(screen.getByTestId("memory-history")).toHaveTextContent("none");
  });

  it("re-reads history at the range picked for the whole panel", async () => {
    const user = setupUser();
    const watchMetrics = vi.fn(staticWatch(answer));
    render(<Harness client={metricsClientWith(watchMetrics)} />);

    await user.click(await screen.findByRole("button", { name: "Toggle memory" }));
    await screen.findByText("memory history: memory.used");
    await user.click(screen.getByRole("button", { name: "Pick 6h" }));

    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(3));
    expect(lastRequest(watchMetrics).queries?.[0].query?.stepHint?.seconds).toBe(10n);
    expect(screen.getByTestId("range")).toHaveTextContent("6h");
    expect(openStreams(watchMetrics)).toHaveLength(2);
  });

  it("shows no history from an earlier expansion while a reopened stream backfills", async () => {
    const user = setupUser();
    let historyOpens = 0;
    const answered = staticWatch(answer);
    const watchMetrics = vi.fn<MetricsClient["metrics"]["watchMetrics"]>((request, options) => {
      const history = request.queries?.some(({ query }) => query?.metric === "memory.used");
      if (history && (historyOpens += 1) > 1)
        return idleMetricsClient().metrics.watchMetrics(request, options);
      return answered(request, options);
    });
    render(<Harness client={metricsClientWith(watchMetrics)} />);

    await user.click(await screen.findByRole("button", { name: "Toggle memory" }));
    await screen.findByText("memory history: memory.used");
    await user.click(screen.getByRole("button", { name: "Toggle memory" }));
    await user.click(screen.getByRole("button", { name: "Toggle memory" }));

    await waitFor(() => expect(historyOpens).toBe(2));
    expect(screen.getByTestId("memory-history")).toHaveTextContent("memory history: none");
  });

  it("reports current values that stopped updating", async () => {
    const watchMetrics = vi.fn<MetricsClient["metrics"]["watchMetrics"]>(() =>
      (async function* () {
        await Promise.resolve();
        yield* [];
        throw new Error("dropped");
      })(),
    );
    render(<Harness client={metricsClientWith(watchMetrics)} />);

    expect(await screen.findByText("current failed")).toBeVisible();
  });
});

/** Shows what the panel offers: each group's grids by metric, the range, and toggles. */
function Harness({ client }: { client: MetricsClient }) {
  const panel = useMetricPanel(client, groups, { durationMs: 900_000, stepMs: 60_000 });
  const names = (grids: ReturnType<typeof panel.current>) =>
    grids?.map((grid) => grid.series[0]?.labels.metric).join(", ") ?? "none";

  return (
    <>
      {groups.map((group) => (
        <button
          key={group.id}
          onClick={() => panel.setExpanded(group.id, !panel.expanded(group.id))}
          type="button"
        >
          Toggle {group.id}
        </button>
      ))}
      <button onClick={() => panel.setRange("6h")} type="button">
        Pick 6h
      </button>
      <p>cpu current: {names(panel.current("cpu"))}</p>
      <p data-testid="memory-history">memory history: {names(panel.history("memory"))}</p>
      <p data-testid="range">{panel.range?.label}</p>
      {panel.currentFailed ? <p>current failed</p> : null}
    </>
  );
}

/** One series per query, labeled with its metric so tests can tell the grids apart. */
function answer(query: Parameters<Parameters<typeof staticWatch>[0]>[0]) {
  return queryResult({
    series: [metricSeries({ metric: query?.metric ?? "" }, [gaugePoint(0, 1)])],
    stepCount: 1,
  });
}

type WatchMock = ReturnType<typeof vi.fn<MetricsClient["metrics"]["watchMetrics"]>>;

/** The metrics of each stream still open, in opening order. */
function openStreams(watchMetrics: WatchMock): string[][] {
  return watchMetrics.mock.calls
    .filter(([, options]) => !options?.signal?.aborted)
    .map(([request]) => (request.queries ?? []).map(({ query }) => query?.metric ?? ""));
}

function lastRequest(watchMetrics: WatchMock) {
  return watchMetrics.mock.calls.at(-1)![0];
}
