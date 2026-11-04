import { render, screen } from "@testing-library/react";
import type uPlot from "uplot";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { MetricGrid } from "./metric-grid";
import { MetricRowHistory } from "./metric-row-history";
import type { MetricPanel } from "./use-metric-panel";

const plots = vi.hoisted(() => ({ created: [] as { data: unknown[][] }[] }));

vi.mock("uplot", () => ({
  default: class {
    constructor(_opts: uPlot.Options, data: unknown[][]) {
      plots.created.push({ data });
    }
    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const fifteenMinutes = { durationMs: 900_000, label: "15m", stepMs: 1000 };

beforeEach(() => {
  plots.created.length = 0;
});

describe("MetricRowHistory", () => {
  it("draws an empty chart over the selected range before history is read", async () => {
    render(<MetricRowHistory chart={undefined} label="CPU history" panel={panel()} unit="ratio" />);

    expect(screen.getByRole("img", { name: "CPU history plot" })).toBeInTheDocument();
    await vi.waitFor(() => expect(plots.created).toHaveLength(1));
    const [x] = plots.created[0].data as number[][];
    expect(x).toHaveLength(900);
    expect(x[x.length - 1] - x[0]).toBe(899);
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });

  it("says it is loading only once the panel shows loading", () => {
    render(
      <MetricRowHistory
        chart={undefined}
        label="CPU history"
        panel={panel({ historyLoadingVisible: true })}
        unit="ratio"
      />,
    );

    expect(screen.getByRole("status")).toHaveTextContent("Loading history…");
  });

  it("holds the range picker's place while the ranges are read", () => {
    const { container } = render(
      <MetricRowHistory
        chart={undefined}
        label="CPU history"
        panel={panel({ range: undefined, ranges: [] })}
        unit="ratio"
      />,
    );

    expect(screen.getByRole("img", { name: "CPU history plot" })).toBeInTheDocument();
    expect(container.querySelector('[aria-hidden="true"].h-control-xs')).not.toBeNull();
  });

  it("replaces the chart with a notice when history cannot be read", () => {
    render(
      <MetricRowHistory
        chart={undefined}
        label="CPU history"
        panel={panel({ historyFailed: true })}
        unit="ratio"
      />,
    );

    expect(screen.getByText("History could not be read.")).toBeVisible();
    expect(screen.queryByRole("img", { name: "CPU history plot" })).not.toBeInTheDocument();
  });

  it("draws the read history in the chart's place", () => {
    const grid: MetricGrid = {
      incarnation: { id: "a", startedAtMs: 0 },
      series: [],
      startMs: 0,
      stepCount: 2,
      stepMs: 1000,
    };
    render(
      <MetricRowHistory
        chart={{
          grid,
          series: [{ color: "chart-1", id: "cpu", label: "CPU", values: [0.5, 0.6] }],
        }}
        label="CPU history"
        panel={panel()}
        unit="ratio"
      />,
    );

    expect(screen.getByRole("img", { name: "CPU history plot" })).toBeInTheDocument();
    expect(screen.getByRole("group", { name: "CPU history" })).toHaveTextContent("CPU60%");
  });
});

function panel(overrides: Partial<MetricPanel> = {}): MetricPanel {
  return {
    current: () => undefined,
    currentFailed: false,
    expanded: () => true,
    history: () => undefined,
    historyFailed: false,
    historyLoadingVisible: false,
    historyUnavailable: false,
    range: fifteenMinutes,
    ranges: [fifteenMinutes, { durationMs: 21_600_000, label: "6h", stepMs: 10_000 }],
    setExpanded: () => {},
    setRange: () => {},
    ...overrides,
  };
}
