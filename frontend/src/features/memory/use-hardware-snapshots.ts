import { useMemo } from "react";

import type {
  HardwareSnapshot,
  ObservationUnavailable,
} from "@/gen/dotcore/observation/v1/observation_pb";
import type { Machine, MachineHardwareSnapshot } from "@/gen/dotcore/site/v1/service_pb";
import { useWatchedHardwareSnapshots, type WatchOptions } from "@/resource-watch/resource-watch";

export interface MachineSnapshotUpdateFailure {
  code?: string;
  message: string;
}

/** One machine's current hardware snapshot as the console last read it. */
export type MachineSnapshotDetail =
  | { kind: "error"; message: string }
  | { kind: "loading" }
  | {
      kind: "success";
      snapshot: HardwareSnapshot;
      updateFailure?: MachineSnapshotUpdateFailure;
    }
  | { kind: "unavailable"; unavailable: ObservationUnavailable };

/**
 * Each given machine's current hardware snapshot from the console's resource watch, while mounted
 * and enabled. A machine whose snapshot is not available yet shows loading, a failed refresh keeps
 * the machine's latest snapshot with the failure, and a watch that is down marks every snapshot
 * stale instead of dropping it.
 */
export function useHardwareSnapshots(
  machines: readonly Machine[],
  options?: WatchOptions,
): ReadonlyMap<string, MachineSnapshotDetail> {
  const watched = useWatchedHardwareSnapshots(options);

  return useMemo(() => {
    const entries = new Map(
      watched?.value?.machines.map((entry) => [entry.machineId, entry] as const),
    );

    return new Map(
      machines.map((machine) => [
        machine.machineId,
        isSnapshotAvailable(machine)
          ? watchedDetail(entries.get(machine.machineId), watched)
          : initialDetail(machine),
      ]),
    );
  }, [machines, watched]);
}

const invalidAvailabilityMessage =
  "Machine inventory protocol error: missing or unknown current hardware snapshot availability.";
const missingSnapshotMessage =
  "Current hardware snapshot response protocol error: missing snapshot.";
const watchDownMessage = "The hardware snapshot watch is unavailable.";

function watchedDetail(
  entry: MachineHardwareSnapshot | undefined,
  watched: ReturnType<typeof useWatchedHardwareSnapshots>,
): MachineSnapshotDetail {
  const watchFailure = watched?.failed ? { message: watchDownMessage } : undefined;
  const snapshot = entry?.snapshot;
  const refresh = entry?.refresh?.outcome;
  if (snapshot) {
    const refreshFailure =
      refresh?.case === "failed"
        ? { code: refresh.value.code || undefined, message: refresh.value.message }
        : undefined;
    const updateFailure = refreshFailure ?? watchFailure;

    return updateFailure
      ? { kind: "success", snapshot, updateFailure }
      : { kind: "success", snapshot };
  }

  if (watchFailure && !entry) return { kind: "error", message: watchFailure.message };
  if (!entry) return { kind: "loading" };
  if (refresh?.case === "failed") return { kind: "error", message: refresh.value.message };
  if (refresh?.case === "pending") return { kind: "loading" };

  return { kind: "error", message: missingSnapshotMessage };
}

function initialDetail(machine: Machine): MachineSnapshotDetail {
  const availability = machine.currentHardwareSnapshotAvailability?.availability;
  if (availability?.case === "available") return { kind: "loading" };
  if (availability?.case === "unavailable") {
    return { kind: "unavailable", unavailable: availability.value };
  }
  return { kind: "error", message: invalidAvailabilityMessage };
}

function isSnapshotAvailable(machine: Machine): boolean {
  return machine.currentHardwareSnapshotAvailability?.availability.case === "available";
}
