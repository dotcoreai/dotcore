import { describe, expect, it } from "vitest";

import { fillMetricGrid } from "@/features/metrics/metric-grid";
import { gaugePoint, metricSeries, queryResult } from "@/features/metrics/metric-test-support";

import {
  deviceMemorySeries,
  deviceSignalSeries,
  filesystemSeries,
  hostCpuSeries,
  hostMemorySeries,
} from "./machine-history-series";

const grid = (points: ReturnType<typeof gaugePoint>[], labels: Record<string, string> = {}) =>
  fillMetricGrid(queryResult({ series: [metricSeries(labels, points)], stepCount: 4 }));

describe("hostMemorySeries", () => {
  it("plots total minus available as used, absent where either reading is absent", () => {
    const total = grid([gaugePoint(0, 100), gaugePoint(1, 100), gaugePoint(2, 100)]);
    const available = grid([gaugePoint(0, 30), gaugePoint(2, 130, true)]);

    const [used, capacity] = hostMemorySeries(total, available);

    expect(used.values).toEqual([70, null, 0, null]);
    expect(used.provisional).toEqual([false, false, true, false]);
    expect(capacity).toMatchObject({
      color: "muted",
      dashed: true,
      label: "Capacity",
      values: [100, 100, 100, null],
    });
  });

  it("is all absent when the host never reported", () => {
    const empty = fillMetricGrid(queryResult({ series: [], stepCount: 2 }));

    expect(hostMemorySeries(empty, empty)[0].values).toEqual([null, null]);
  });
});

describe("deviceMemorySeries", () => {
  it("reads only the requested device's series, against the device total", () => {
    const used = fillMetricGrid(
      queryResult({
        series: [
          metricSeries({ device: "gpu-0" }, [gaugePoint(0, 5)]),
          metricSeries({ device: "gpu-1" }, [gaugePoint(0, 9)]),
        ],
        stepCount: 2,
      }),
    );

    const [usedLine, capacity] = deviceMemorySeries("gpu-1", used, 32);

    expect(usedLine.values).toEqual([9, null]);
    expect(capacity).toMatchObject({ dashed: true, label: "Capacity", values: [32, 32] });
    expect(deviceMemorySeries("gpu-2", used, 32)[0].values).toEqual([null, null]);
  });
});

describe("hostMemorySeries alignment", () => {
  it("pairs total and available by time when the second query landed one step later", () => {
    const total = fillMetricGrid(
      queryResult({
        series: [metricSeries({}, [gaugePoint(0, 100), gaugePoint(1, 100), gaugePoint(2, 100)])],
        stepCount: 3,
      }),
    );
    // The second query landed one step later, so its grid starts one second after the total's.
    const available = fillMetricGrid(
      queryResult({
        series: [metricSeries({}, [gaugePoint(1, 10), gaugePoint(2, 20), gaugePoint(3, 30)])],
        startOffset: 1,
        stepCount: 3,
      }),
    );

    expect(hostMemorySeries(total, available)[0].values).toEqual([null, 90, 80]);
  });

  it("marks every step absent when the grids come from different incarnations", () => {
    const points = [metricSeries({}, [gaugePoint(0, 100), gaugePoint(1, 100)])];
    const total = fillMetricGrid(queryResult({ series: points, stepCount: 2 }));
    const other = fillMetricGrid(
      queryResult({ incarnationId: "incarnation-b", series: points, stepCount: 2 }),
    );

    expect(hostMemorySeries(total, other)[0].values).toEqual([null, null]);
  });
});

describe("hostCpuSeries", () => {
  it("plots the ratio with absent steps and open points kept", () => {
    const [cpu] = hostCpuSeries(grid([gaugePoint(0, 0.25), gaugePoint(2, 0.5, true)]));

    expect(cpu.values).toEqual([0.25, null, 0.5, null]);
    expect(cpu.provisional).toEqual([false, false, true, false]);
  });
});

describe("deviceSignalSeries", () => {
  const signal = fillMetricGrid(
    queryResult({
      series: [
        metricSeries({ device: "gpu-0" }, [gaugePoint(0, 40), gaugePoint(1, 41)]),
        metricSeries({ device: "gpu-1" }, [gaugePoint(1, 70)]),
      ],
      stepCount: 2,
    }),
  );

  it("draws the given device's line with the given label and color", () => {
    const line = deviceSignalSeries("gpu-1", signal, "Temperature", "chart-4");

    expect(line).toMatchObject({
      color: "chart-4",
      id: "gpu-1",
      label: "Temperature",
      values: [null, 70],
    });
  });

  it("keeps a silent device as a gap", () => {
    expect(deviceSignalSeries("gpu-2", signal, "Power", "chart-3").values).toEqual([null, null]);
  });
});

describe("filesystemSeries", () => {
  const root = { fstype: "ext4", mountpoint: "/" };
  const boot = { fstype: "vfat", mountpoint: "/boot" };

  it("pairs used with the size capacity of the same mountpoint by time", () => {
    const used = fillMetricGrid(
      queryResult({
        series: [
          metricSeries(root, [gaugePoint(0, 10), gaugePoint(1, 11)]),
          metricSeries(boot, [gaugePoint(0, 1)]),
        ],
        stepCount: 2,
      }),
    );
    // The size query landed one step later, so its grid starts one second after the used grid's.
    const size = fillMetricGrid(
      queryResult({
        series: [
          metricSeries(root, [gaugePoint(1, 100), gaugePoint(2, 100)]),
          metricSeries(boot, [gaugePoint(1, 500)]),
        ],
        startOffset: 1,
        stepCount: 2,
      }),
    );

    const [usedLine, sizeLine] = filesystemSeries(root, used, size);

    expect(usedLine.values).toEqual([10, 11]);
    expect(sizeLine).toMatchObject({ dashed: true, label: "Capacity", values: [null, 100] });
    expect(filesystemSeries(boot, used, size)[0].values).toEqual([1, null]);
    expect(filesystemSeries(boot, used, size)[1].values).toEqual([null, 500]);
  });

  it("matches a filesystem on both its mountpoint and its type", () => {
    // The mount point was reformatted from ext4 to xfs within the range; each type is its own
    // series, and only the one the snapshot lists is charted.
    const used = fillMetricGrid(
      queryResult({
        series: [
          metricSeries(root, [gaugePoint(0, 10)]),
          metricSeries({ fstype: "xfs", mountpoint: "/" }, [gaugePoint(1, 3)]),
        ],
        stepCount: 2,
      }),
    );
    const size = fillMetricGrid(
      queryResult({
        series: [
          metricSeries(root, [gaugePoint(0, 100)]),
          metricSeries({ fstype: "xfs", mountpoint: "/" }, [gaugePoint(1, 50)]),
        ],
        stepCount: 2,
      }),
    );

    const [usedLine, sizeLine] = filesystemSeries({ fstype: "xfs", mountpoint: "/" }, used, size);

    expect(usedLine.values).toEqual([null, 3]);
    expect(sizeLine.values).toEqual([null, 50]);
  });
});
