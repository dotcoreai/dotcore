import type { Timestamp } from "@bufbuild/protobuf/wkt";

import { unavailableMemoryReason } from "@/features/memory/unavailable-memory-codes";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import { ApplicationLifecycleState } from "@/gen/dotcore/application/v1/state_pb";
import { formatBytes, formatRelativeTime } from "@/lib/format";

/** One accelerator device's attributed usage as reported for one unit. */
export interface UnitAcceleratorDeviceMemory {
  bytes: bigint;
  deviceId: string;
  /** The machine the device sits in. */
  managedNodeId: string;
}

/**
 * One unit's accelerator memory as the deployment inspector reads it: attributed usage per device,
 * a reason when it is unavailable, a marker that the deployment is not running, or a marker that
 * attribution does not apply to the unit's target. A unit the observation does not cover has no
 * entry at all.
 */
export interface UnitAcceleratorMemory {
  /** The unit's service name, as the immutable application snapshot names it. */
  serviceName: string;
  kind: "available" | "unavailable" | "notApplicable" | "notRunning";
  /** Attributed usage per device, in the order the observation reported it. */
  devices?: UnitAcceleratorDeviceMemory[];
  /** The console words for the unavailable state. */
  reason?: string;
  /** The reported message behind the reason, absent when empty. */
  reasonDetail?: string;
  observedAt?: Timestamp;
  /** Whether the reading may be outdated because its last refresh failed. */
  stale: boolean;
}

/** One unit's rendered row: one line per value, muted when no usage is shown. */
export interface FormattedUnitAcceleratorMemory {
  lines: string[];
  muted: boolean;
  stale: boolean;
  title?: string;
}

/**
 * The per-unit accelerator memory of one application deployment, from its read-time unit
 * projection: only units the projection covers, each reading its own result, never a value for a
 * unit the projection left out. A paused, stopped, or failed deployment reads as not running.
 */
export function applicationAcceleratorMemory(
  deployment: ApplicationDeployment,
): UnitAcceleratorMemory[] {
  const running = !notRunningStates.has(
    deployment.state?.lifecycleState ?? ApplicationLifecycleState.UNSPECIFIED,
  );
  return deployment.serviceAcceleratorMemory.flatMap((unit): UnitAcceleratorMemory[] => {
    const observation = unit.acceleratorMemory;
    const result = observation?.result;
    if (!result) return [];

    if (!running) return [{ kind: "notRunning", serviceName: unit.serviceName, stale: false }];

    const stale = observation.refresh?.outcome.case === "failed";

    if (result.case === "available") {
      // The projection reports available usage only with at least one device, so an empty set
      // reads as absent rather than a zero.
      if (!result.value.devices.length) return [];

      return [
        {
          devices: result.value.devices.map(({ deviceId, managedNodeId, usedMemoryBytes }) => ({
            bytes: usedMemoryBytes,
            deviceId,
            managedNodeId,
          })),
          kind: "available",
          observedAt: observation.observedAt,
          serviceName: unit.serviceName,
          stale,
        },
      ];
    }

    if (result.case === "unavailable") {
      return [
        {
          kind: "unavailable",
          reason: unavailableMemoryReason(result.value.code),
          reasonDetail: result.value.message || undefined,
          serviceName: unit.serviceName,
          stale,
        },
      ];
    }

    return [{ kind: "notApplicable", serviceName: unit.serviceName, stale: false }];
  });
}

/**
 * One unit's row: the attributed bytes, one line per device when it spans several, or the unit's
 * reason. A stale reading adds a line with its observation time; the exact bytes and the reported
 * message sit behind the row on hover.
 */
export function formatUnitAcceleratorMemory(
  unit: UnitAcceleratorMemory,
  now: Date = new Date(),
): FormattedUnitAcceleratorMemory {
  if (unit.kind === "unavailable") {
    return {
      lines: [unit.reason ?? "measurement unavailable"],
      muted: true,
      stale: unit.stale,
      title: unit.reasonDetail,
    };
  }

  if (unit.kind === "notApplicable") {
    return { lines: ["not applicable"], muted: true, stale: false };
  }

  if (unit.kind === "notRunning") {
    return { lines: ["not running"], muted: true, stale: false };
  }

  const devices = unit.devices ?? [];
  const observed = unit.observedAt ? formatRelativeTime(unit.observedAt, now).text : undefined;
  const lines =
    devices.length === 1
      ? [formatBytes(devices[0].bytes)]
      : devices.map((device) => `${device.deviceId}: ${formatBytes(device.bytes)}`);
  if (unit.stale) lines.push(observed ? `stale, observed ${observed}` : "stale");
  const title = [
    devices.map((device) => `${device.deviceId}: ${device.bytes} bytes`).join("; "),
    observed ? `observed ${observed}` : undefined,
  ]
    .filter(Boolean)
    .join(" · ");

  return { lines, muted: false, stale: unit.stale, title };
}

/**
 * Lifecycle states in which no unit runs, so attribution does not apply; mirrors the inactive set
 * the backend's attribution scope reads as not applicable.
 */
const notRunningStates = new Set([
  ApplicationLifecycleState.PAUSED,
  ApplicationLifecycleState.STOPPED,
  ApplicationLifecycleState.FAILED,
]);
