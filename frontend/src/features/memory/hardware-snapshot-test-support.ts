import { create } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";

import {
  HardwareSnapshotSchema,
  HostFactsSchema,
  HostObservationSchema,
  ObservationUnavailableSchema,
} from "@/gen/dotcore/observation/v1/observation_pb";
import {
  CurrentHardwareSnapshotAvailabilitySchema,
  HardwareRefreshStateSchema,
  ListHardwareSnapshotsResponseSchema,
  MachineHardwareSnapshotSchema,
  MachineSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import type { FakeResourceWatch } from "@/test/fake-resource-watch";

/** A listed machine whose hardware snapshot is readable. */
export function snapshotMachine(machineId: string, displayName: string) {
  return create(MachineSchema, {
    currentHardwareSnapshotAvailability: create(CurrentHardwareSnapshotAvailabilitySchema, {
      availability: { case: "available", value: create(EmptySchema) },
    }),
    displayName,
    machineId,
  });
}

/** A hardware snapshot naming its host, with fixed host facts. */
export function hostSnapshot(hostName: string) {
  return create(HardwareSnapshotSchema, {
    host: create(HostObservationSchema, {
      result: {
        case: "facts",
        value: create(HostFactsSchema, {
          availableMemoryBytes: 34359738368n,
          cpuCount: 16,
          hostName,
          operatingSystemOrKernel: "Linux 6.12",
          filesystems: [{ fstype: "ext4", mountpoint: "/", sizeBytes: 1099511627776n }],
          totalMemoryBytes: 68719476736n,
        }),
      },
    }),
    observedAt: { seconds: 1n },
  });
}

/** One machine's entry: its snapshot, if observed, and the outcome of its latest refresh. */
export function snapshotEntry(
  machineId: string,
  {
    hostName,
    refresh = hostName ? "succeeded" : "pending",
  }: { hostName?: string; refresh?: "pending" | "succeeded" | { code: string; message: string } },
) {
  return create(MachineHardwareSnapshotSchema, {
    machineId,
    refresh: create(HardwareRefreshStateSchema, {
      outcome:
        typeof refresh === "string"
          ? { case: refresh, value: create(EmptySchema) }
          : { case: "failed", value: create(ObservationUnavailableSchema, refresh) },
    }),
    snapshot: hostName ? hostSnapshot(hostName) : undefined,
  });
}

/** Sends one hardware snapshots payload with the given machine entries. */
export function sendSnapshots(
  watch: FakeResourceWatch,
  ...machines: Array<ReturnType<typeof snapshotEntry>>
) {
  watch.payload("hardwareSnapshots", create(ListHardwareSnapshotsResponseSchema, { machines }));
}
