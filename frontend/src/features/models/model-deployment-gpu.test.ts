import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { fillMetricGrid, type MetricGrid } from "@/features/metrics/metric-grid";
import { gaugePoint, metricSeries, queryResult } from "@/features/metrics/metric-test-support";
import { AcceleratorMemoryObservationSchema } from "@/gen/dotcore/deployment/v1/accelerator_memory_pb";
import { ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";

import { deploymentGpus as gpusOf, modelDeploymentDevices } from "./model-deployment-gpu";

function deploymentGpus(
  deployments: Parameters<typeof modelDeploymentDevices>[0],
  utilization: MetricGrid,
) {
  return gpusOf(modelDeploymentDevices(deployments), utilization);
}

describe("deploymentGpus", () => {
  const utilization = grid([
    deviceGauge("gpu1", "GPU-a", 0.6),
    deviceGauge("gpu1", "GPU-b", 0.2),
    deviceGauge("gpu2", "GPU-a", 0.9),
  ]);

  it("reads the utilization of the one device a deployment runs on", () => {
    const gpus = deploymentGpus([deployment("one", [["gpu1", "GPU-a"]])], utilization);

    expect(gpus).toEqual(new Map([["one", { shared: false, utilization: 0.6 }]]));
  });

  it("averages the devices a deployment spans", () => {
    const gpus = deploymentGpus(
      [
        deployment("split", [
          ["gpu1", "GPU-a"],
          ["gpu1", "GPU-b"],
        ]),
      ],
      utilization,
    );

    expect(gpus.get("split")?.utilization).toBeCloseTo(0.4);
    expect(gpus.get("split")?.shared).toBe(false);
  });

  it("marks deployments on the same device as shared, matching the node too", () => {
    const gpus = deploymentGpus(
      [
        deployment("first", [["gpu1", "GPU-a"]]),
        deployment("second", [["gpu1", "GPU-a"]]),
        // Same vendor id on another machine is another GPU.
        deployment("elsewhere", [["gpu2", "GPU-a"]]),
      ],
      utilization,
    );

    expect(gpus).toEqual(
      new Map([
        ["first", { shared: true, utilization: 0.6 }],
        ["second", { shared: true, utilization: 0.6 }],
        ["elsewhere", { shared: false, utilization: 0.9 }],
      ]),
    );
  });

  it("leaves out a deployment with no measured device", () => {
    const unmeasured = create(ModelDeploymentSchema, { modelDeploymentId: "pending" });
    const gpus = deploymentGpus([unmeasured, deployment("empty", [])], utilization);

    expect(gpus.size).toBe(0);
  });

  it("keeps a measured device without a reading, its utilization absent", () => {
    const gpus = deploymentGpus(
      [deployment("first", [["gpu3", "GPU-z"]]), deployment("second", [["gpu3", "GPU-z"]])],
      utilization,
    );

    expect(gpus.get("first")).toEqual({ shared: true, utilization: undefined });
  });
});

function deployment(modelDeploymentId: string, devices: [node: string, device: string][]) {
  return create(ModelDeploymentSchema, {
    acceleratorMemory: create(AcceleratorMemoryObservationSchema, {
      result: {
        case: "available",
        value: {
          devices: devices.map(([managedNodeId, deviceId]) => ({
            deviceId,
            managedNodeId,
            usedMemoryBytes: 1024n,
          })),
        },
      },
    }),
    modelDeploymentId,
  });
}

/** A one-step grid of per-device gauges. */
function grid(series: ReturnType<typeof metricSeries>[]): MetricGrid {
  return fillMetricGrid(queryResult({ series, stepCount: 1 }));
}

function deviceGauge(node: string, device: string, value: number) {
  return metricSeries({ device, node }, [gaugePoint(0, value)]);
}
