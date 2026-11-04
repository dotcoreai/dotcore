import { render } from "@testing-library/react";
import type uPlot from "uplot";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import type { ChartSeries } from "./chart-data";
import { TimeSeriesChart } from "./time-series-chart";

// Real uPlot, recording each plot it builds so a test can read the scales it settled on.
const plots = vi.hoisted(() => [] as uPlot[]);

vi.mock("uplot", async (importOriginal) => {
  const { default: RealPlot } = await importOriginal<{ default: typeof uPlot }>();

  return {
    default: class extends RealPlot {
      constructor(...args: ConstructorParameters<typeof uPlot>) {
        super(...args);
        plots.push(this);
      }
    },
  };
});

// jsdom has no canvas; uPlot only needs a context that accepts its drawing calls.
beforeAll(() => {
  const context = new Proxy<Record<string | symbol, unknown>>(
    {},
    {
      get: (target, key) => (key in target ? target[key] : () => ({ width: 0 })),
      set: (target, key, value) => {
        target[key] = value;
        return true;
      },
    },
  );
  HTMLCanvasElement.prototype.getContext = (() => context) as never;
  globalThis.Path2D ??= class {
    arc() {}
    bezierCurveTo() {}
    closePath() {}
    lineTo() {}
    moveTo() {}
    rect() {}
  } as unknown as typeof Path2D;
});

beforeEach(() => {
  plots.length = 0;
});

const series = (length: number): ChartSeries[] => [
  { color: "chart-1", id: "used", label: "Used", values: Array.from({ length }, () => 1) },
];

describe("TimeSeriesChart time axis", () => {
  it("spans the new window after the range changes", async () => {
    const { rerender } = render(
      <TimeSeriesChart label="CPU" series={series(3)} startMs={0} stepMs={1000} unit="ratio" />,
    );
    await vi.waitFor(() => expect(plots).toHaveLength(1));
    await vi.waitFor(() => expect([plots[0].scales.x.min, plots[0].scales.x.max]).toEqual([0, 2]));

    rerender(
      <TimeSeriesChart
        label="CPU"
        series={series(4)}
        startMs={600_000}
        stepMs={60_000}
        unit="ratio"
      />,
    );

    await vi.waitFor(() =>
      expect([plots[0].scales.x.min, plots[0].scales.x.max]).toEqual([600, 780]),
    );
    expect(plots).toHaveLength(1);
  });

  it("spans the new window of a bar chart with half a step past each end", async () => {
    const bars = (length: number): ChartSeries[] =>
      series(length).map((entry) => ({ ...entry, mark: "bars" }));
    const { rerender } = render(
      <TimeSeriesChart
        label="Requests"
        series={bars(3)}
        startMs={0}
        stepMs={1000}
        unit="requests"
      />,
    );
    await vi.waitFor(() => expect(plots).toHaveLength(1));

    rerender(
      <TimeSeriesChart
        label="Requests"
        series={bars(4)}
        startMs={600_000}
        stepMs={60_000}
        unit="requests"
      />,
    );

    await vi.waitFor(() =>
      expect([plots[0].scales.x.min, plots[0].scales.x.max]).toEqual([570, 810]),
    );
  });
});
