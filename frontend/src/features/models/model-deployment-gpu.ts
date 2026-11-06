import { useMemo } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import {
  currentDeviceUtilization,
  resourcesCurrentWindow,
} from "@/features/machines/machine-resources";
import type { MetricGrid } from "@/features/metrics/metric-grid";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import { useMetricsPausedByLogs } from "@/features/metrics/metric-pause";
import type { MetricRange } from "@/features/metrics/metric-ranges";
import { type MetricHistoryQuery, useMetricAnswers } from "@/features/metrics/use-metric-watch";
import type { AcceleratorMemoryDeviceUsage } from "@/gen/dotcore/deployment/v1/accelerator_memory_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";

/** A GPU as a metrics series names it: the machine it sits in and its device identity. */
export interface GpuDeviceRef {
  deviceId: string;
  managedNodeId: string;
}

/** The GPU use of the devices a deployment runs on. */
export interface DeploymentGpu {
  /** The other listed deployments (by id) that measure memory on one of the same devices. */
  sharedWith: string[];
  /** Current utilization ratio, averaged over the devices with a reading; absent without one. */
  utilization?: number;
}

/**
 * The GPU use of each listed deployment, given the devices its accelerator memory is measured on
 * by deployment id, watched live in one small stream of per-device utilization. The stream opens
 * only while some deployment measures memory on a device.
 */
export function useDeploymentGpus(
  client: MetricsClient,
  devicesById: ReadonlyMap<string, readonly GpuDeviceRef[]>,
): ReadonlyMap<string, DeploymentGpu> {
  const paused = useMetricsPausedByLogs();
  const measured = [...devicesById.values()].some((devices) => devices.length > 0);
  const watch = useMetricAnswers(
    client,
    measured ? utilizationQueries : [],
    measured ? utilizationWindow : undefined,
    { paused },
  );
  const grid = watch?.value?.[0]?.grid;

  return useMemo(() => deploymentGpus(devicesById, grid), [devicesById, grid]);
}

/**
 * Each deployment's GPU use from a utilization grid grouped by node and device. A deployment maps
 * to the devices its accelerator memory is measured on, so deployments on the same device read the
 * same value and list each other as sharing. A deployment with no measured device is absent.
 */
export function deploymentGpus(
  devicesById: ReadonlyMap<string, readonly GpuDeviceRef[]>,
  utilization: MetricGrid | undefined,
): Map<string, DeploymentGpu> {
  const devices = new Map(
    [...devicesById].map(([deploymentId, refs]) => [deploymentId, uniqueDevices(refs)]),
  );
  const users = new Map<string, string[]>();
  for (const [deploymentId, keys] of devices) {
    for (const key of keys.keys()) users.set(key, [...(users.get(key) ?? []), deploymentId]);
  }

  const gpus = new Map<string, DeploymentGpu>();
  for (const [deploymentId, keys] of devices) {
    if (!keys.size) continue;

    const readings = [...keys.values()].flatMap((device) => {
      const value = currentDeviceUtilization(utilization, device.managedNodeId, device.deviceId);
      return value === undefined ? [] : [value];
    });
    gpus.set(deploymentId, {
      sharedWith: [
        ...new Set(
          [...keys.keys()].flatMap((key) =>
            (users.get(key) ?? []).filter((other) => other !== deploymentId),
          ),
        ),
      ],
      utilization: readings.length
        ? readings.reduce((total, value) => total + value, 0) / readings.length
        : undefined,
    });
  }

  return gpus;
}

/** The GPU use with the sharing deployments' ids replaced by their display names. */
export function withSharerNames(
  gpu: DeploymentGpu | undefined,
  nameOf: (deploymentId: string) => string,
): DeploymentGpu | undefined {
  return gpu && { ...gpu, sharedWith: gpu.sharedWith.map(nameOf) };
}

/** The measured devices of each model deployment by id. */
export function modelDeploymentDevices(
  deployments: readonly ModelDeployment[],
): Map<string, AcceleratorMemoryDeviceUsage[]> {
  return new Map(
    deployments.map((deployment) => [deployment.modelDeploymentId, measuredDevices(deployment)]),
  );
}

/** The devices a deployment's accelerator memory is measured on; none without a measure. */
export function measuredDevices(deployment: ModelDeployment): AcceleratorMemoryDeviceUsage[] {
  const result = deployment.acceleratorMemory?.result;
  return result?.case === "available" ? result.value.devices : [];
}

const utilizationQueries: readonly MetricHistoryQuery[] = [
  {
    groupBy: [dimensionNames.node, dimensionNames.device],
    matchers: {},
    metric: metricNames.nodeAcceleratorUtilization,
  },
];

const utilizationWindow: MetricRange = { ...resourcesCurrentWindow, label: "now" };

/** The devices by their node and device key, each once. */
function uniqueDevices(devices: readonly GpuDeviceRef[]): Map<string, GpuDeviceRef> {
  return new Map(devices.map((device) => [`${device.managedNodeId}/${device.deviceId}`, device]));
}
