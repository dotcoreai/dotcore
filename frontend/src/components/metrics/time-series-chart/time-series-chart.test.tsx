import { act, render, screen } from "@testing-library/react";
import type uPlot from "uplot";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { getResourceColor } from "@/theme/resource-color";

import type { ChartSeries } from "./chart-data";
import { TimeSeriesChart } from "./time-series-chart";

const plots = vi.hoisted(() => ({
  created: [] as { data: unknown[][]; opts: uPlot.Options }[],
  setData: vi.fn(),
}));

vi.mock("uplot", () => ({
  default: class {
    static paths = { bars: (opts: uPlot.Series.BarsPathBuilderOpts) => ({ bars: opts }) };

    constructor(opts: uPlot.Options, data: unknown[][]) {
      plots.created.push({ data, opts });
    }
    setData = plots.setData;
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const gib = 1024 ** 3;
const used: ChartSeries = {
  color: "chart-1",
  id: "used",
  label: "Used",
  values: [2 * gib, null, 4.5 * gib],
};

beforeEach(() => {
  plots.created.length = 0;
  plots.setData.mockClear();
});

async function renderChart(props: Partial<React.ComponentProps<typeof TimeSeriesChart>> = {}) {
  const view = render(
    <TimeSeriesChart
      label="RAM history"
      series={[used]}
      startMs={0}
      stepMs={1000}
      unit="bytes"
      {...props}
    />,
  );
  await vi.waitFor(() => expect(plots.created).toHaveLength(1));
  return view;
}

describe("TimeSeriesChart", () => {
  it("hands the plot a null at the absent step so the line breaks instead of reading zero", async () => {
    await renderChart();

    expect(plots.created[0].data).toEqual([
      [0, 1, 2],
      [2 * gib, null, 4.5 * gib],
      [null, null, null],
    ]);
    expect(plots.created[0].opts.series?.[1]).toMatchObject({ spanGaps: false, label: "Used" });
  });

  it("draws an empty plot over its window with the status in the readout line", async () => {
    await renderChart({
      series: [],
      status: <span role="status">Loading history…</span>,
      stepCount: 3,
    });

    expect(plots.created[0].data).toEqual([[0, 1, 2]]);
    expect(screen.getByRole("img", { name: "RAM history plot" })).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Loading history…");
  });

  it("formats the latest value and the y axis ticks in the unit", async () => {
    await renderChart();

    expect(screen.getByText("4.5 GiB")).toBeInTheDocument();
    const yAxis = plots.created[0].opts.axes?.[1];
    const labels = (yAxis?.values as (...args: unknown[]) => string[])(undefined, [
      0,
      20 * gib,
      512 * 1024 ** 2,
    ]);
    expect(labels).toEqual(["0 bytes", "20 GiB", "512 MiB"]);
  });

  it("reads the hovered step, stating no data where a point is absent", async () => {
    await renderChart();
    const setCursor = plots.created[0].opts.hooks?.setCursor?.[0] as (plot: unknown) => void;

    act(() => setCursor({ cursor: { idx: 1 } }));
    expect(screen.getByText("No data", { selector: "span span:last-child" })).toBeInTheDocument();

    act(() => setCursor({ cursor: { idx: 0 } }));
    expect(screen.getByText("2 GiB")).toBeInTheDocument();
  });

  it("updates data without rebuilding the plot when only values change", async () => {
    const { rerender } = await renderChart();

    rerender(
      <TimeSeriesChart
        label="RAM history"
        series={[{ ...used, values: [1 * gib, 2 * gib, 3 * gib] }]}
        startMs={0}
        stepMs={1000}
        unit="bytes"
      />,
    );

    expect(plots.created).toHaveLength(1);
    expect(plots.setData).toHaveBeenLastCalledWith([
      [0, 1, 2],
      [1 * gib, 2 * gib, 3 * gib],
      [null, null, null],
    ]);
  });

  it("draws a bar series as bars centred on their steps, with no bar at an absent step", async () => {
    await renderChart({
      series: [
        { color: "chart-1", id: "requests", label: "Requests", mark: "bars", values: [4, null, 2] },
      ],
      unit: "requests",
    });
    const opts = plots.created[0].opts;
    const setCursor = opts.hooks?.setCursor?.[0] as (plot: unknown) => void;

    expect(plots.created[0].data).toEqual([
      [0, 1, 2],
      [4, null, 2],
      [null, null, null],
    ]);
    expect(opts.series?.[1]).toMatchObject({ label: "Requests", paths: { bars: {} }, width: 0 });
    expect(opts.series?.[1].points).toMatchObject({ show: false });
    const xRange = opts.scales?.x.range as unknown as (
      plot: unknown,
      min: number,
      max: number,
    ) => number[];
    expect(xRange(undefined, 0, 2)).toEqual([-0.5, 2.5]);

    act(() => setCursor({ cursor: { idx: 1 } }));
    expect(screen.getByText("No data", { selector: "span span:last-child" })).toBeInTheDocument();
  });

  it("stacks a bar pair while the readout shows each series' own value", async () => {
    await renderChart({
      series: [
        { color: "chart-1", id: "a", label: "Route A", mark: "stackedBars", values: [1, 6] },
        { color: "chart-2", id: "b", label: "Route B", mark: "stackedBars", values: [2, 7] },
      ],
      unit: "requests",
    });
    const opts = plots.created[0].opts;
    const setCursor = opts.hooks?.setCursor?.[0] as (plot: unknown) => void;
    const disp = (opts.series?.[2].paths as unknown as { bars: uPlot.Series.BarsPathBuilderOpts })
      .bars.disp;

    expect(plots.created[0].data.slice(1, 3)).toEqual([
      [1, 6],
      [3, 13],
    ]);
    expect(disp?.y0?.values({} as uPlot, 2, 0, 1)).toEqual([1, 6]);
    expect(disp?.y1?.values({} as uPlot, 2, 0, 1)).toEqual([3, 13]);

    act(() => setCursor({ cursor: { idx: 0 } }));
    expect(screen.getByText("Route A").nextElementSibling).toHaveTextContent("1");
    expect(screen.getByText("Route B").nextElementSibling).toHaveTextContent("2");
  });

  it("draws a resource-colored line and swatch in the resource's marker color", async () => {
    await renderChart({ series: [{ ...used, color: { resource: "node-a" }, label: "gpu1" }] });

    expect(plots.created[0].opts.series?.[1]).toMatchObject({ stroke: getResourceColor("node-a") });
    const swatch = screen.getByText("gpu1").previousElementSibling;
    expect(swatch).toHaveAttribute("data-slot", "chart-swatch");
    expect(swatch).toHaveStyle({ backgroundColor: getResourceColor("node-a") });
  });

  it("reads the newest closed step while the last step is provisional and incomplete", async () => {
    await renderChart({
      series: [
        {
          color: "chart-1",
          id: "a",
          label: "Node A",
          values: [1, 5, 2],
          provisional: [false, false, true],
        },
        {
          color: "chart-2",
          id: "b",
          label: "Node B",
          values: [1, 6, null],
          provisional: [false, false, true],
        },
      ],
      unit: "requests",
    });

    expect(screen.getByText("Node A").nextElementSibling).toHaveTextContent("5");
    expect(screen.getByText("Node B").nextElementSibling).toHaveTextContent("6");
    expect(screen.queryByText("No data")).not.toBeInTheDocument();
  });

  it("reads the newest provisional step when no step has closed yet", async () => {
    await renderChart({
      series: [
        { color: "chart-1", id: "a", label: "Node A", values: [3, 4], provisional: [true, true] },
        {
          color: "chart-2",
          id: "b",
          label: "Node B",
          values: [null, 9],
          provisional: [true, true],
        },
      ],
      unit: "requests",
    });

    expect(screen.getByText("Node A").nextElementSibling).toHaveTextContent("4");
    expect(screen.getByText("Node B").nextElementSibling).toHaveTextContent("9");
  });

  it("reads a hovered open step as is", async () => {
    await renderChart({
      series: [
        { color: "chart-1", id: "a", label: "Node A", values: [5, 2], provisional: [false, true] },
        {
          color: "chart-2",
          id: "b",
          label: "Node B",
          values: [6, null],
          provisional: [false, true],
        },
      ],
      unit: "requests",
    });
    const setCursor = plots.created[0].opts.hooks?.setCursor?.[0] as (plot: unknown) => void;

    act(() => setCursor({ cursor: { idx: 1 } }));
    expect(screen.getByText("Node A").nextElementSibling).toHaveTextContent("2");
    expect(screen.getByText("Node B").nextElementSibling).toHaveTextContent("No data");
  });
});
