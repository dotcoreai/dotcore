import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import type { DiagnosticLogOperations } from "@/api/diagnostic-log-client";
import { DiagnosticLogViewer } from "@/features/diagnostic-logs/diagnostic-log-viewer";

import { MetricPauseProvider } from "./metric-pause-provider";
import {
  gaugePoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "./metric-test-support";
import { useMetricPanel } from "./use-metric-panel";

const groups = [
  { current: [{ groupBy: [], matchers: {}, metric: "cpu" }], history: [], id: "cpu" },
];

describe("MetricPauseProvider", () => {
  it("closes metric streams while a log viewer is open and keeps the values read", async () => {
    const user = userEvent.setup();
    const watchMetrics = vi.fn(
      staticWatch(() =>
        queryResult({
          series: [metricSeries({ metric: "cpu" }, [gaugePoint(0, 42)])],
          stepCount: 1,
        }),
      ),
    );
    render(
      <MetricPauseProvider>
        <Page client={metricsClientWith(watchMetrics)} />
      </MetricPauseProvider>,
    );

    await screen.findByText("cpu value: 42");
    expect(watchMetrics).toHaveBeenCalledTimes(1);
    expect(watchMetrics.mock.calls[0][1]?.signal?.aborted).toBe(false);

    await user.click(screen.getByRole("button", { name: "Open logs" }));
    await waitFor(() => expect(watchMetrics.mock.calls[0][1]?.signal?.aborted).toBe(true));
    expect(screen.getByText("cpu value: 42")).toBeInTheDocument();
    expect(watchMetrics).toHaveBeenCalledTimes(1);

    await user.click(await screen.findByRole("button", { name: "Close" }));
    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(2));
    expect(watchMetrics.mock.calls[1][1]?.signal?.aborted).toBe(false);
    expect(screen.getByText("cpu value: 42")).toBeInTheDocument();
  });
});

function Page({ client }: { client: Parameters<typeof useMetricPanel>[0] }) {
  const [open, setOpen] = useState(false);
  const panel = useMetricPanel(client, groups, { durationMs: 900_000, stepMs: 60_000 });
  const value = panel.current("cpu")?.[0]?.series[0]?.values.find((entry) => entry !== null);

  return (
    <>
      <button onClick={() => setOpen(true)} type="button">
        Open logs
      </button>
      <p>cpu value: {value ?? "none"}</p>
      <DiagnosticLogViewer onOpenChange={setOpen} open={open} operations={idleDiagnostics()} />
    </>
  );
}

function idleDiagnostics(): DiagnosticLogOperations {
  return {
    follow: async function* () {
      await new Promise(() => undefined);
      yield { event: { kind: "entry" as const, entry: undefined as never } };
    },
    read: () =>
      Promise.resolve({
        entries: [],
        historyState: "no-older-output-observed",
        responseTruncated: false,
        sources: [{ sourceLabel: "Manager", sourceName: "node-manager", state: "available" }],
      }),
  };
}
