import { create } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";

import {
  MemoryPoolKind,
  MemorySegmentSource,
  type NodeMemory,
  NodeMemorySchema,
  NodeMemoryState,
} from "@/gen/dotcore/capacity/v1/memory_pb";
import {
  AcceleratorMemoryArchitecture,
  AcceleratorSignal,
  AcceleratorVendor,
  type HardwareSnapshot,
  HardwareSnapshotSchema,
} from "@/gen/dotcore/observation/v1/observation_pb";
import {
  type Machine,
  MachineSchema,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";

const gib = 1024 ** 3;

/** One GPU of a test snapshot; memory sizes in GiB. */
export interface TestDevice {
  deviceId: string;
  model?: string;
  supportedSignals?: AcceleratorSignal[];
  /** Dedicated memory as used and total GiB, used absent when unread; absent for a unified GPU. */
  vram?: { totalGib: number; usedGib?: number };
}

/** A listed machine in the given connection state, with a snapshot to read when connected. */
export function listedMachine(
  machineId: string,
  displayName: string,
  state = NodeObservationState.CONNECTED,
): Machine {
  return create(MachineSchema, {
    currentHardwareSnapshotAvailability: {
      availability:
        state === NodeObservationState.CONNECTED
          ? { case: "available", value: create(EmptySchema) }
          : { case: "unavailable", value: { code: "NOT_CONNECTED", message: "not connected" } },
    },
    displayName,
    machineId,
    observationFreshness: { state },
  });
}

/** A hardware snapshot with host memory, logical CPUs, filesystems and GPUs, sizes in GiB. */
export function hardwareSnapshot({
  cpuCount,
  devices = [],
  filesystemGib = [],
  memory = { availableGib: 0, totalGib: 0 },
}: {
  cpuCount: number;
  devices?: TestDevice[];
  filesystemGib?: number[];
  memory?: { availableGib: number; totalGib: number };
}): HardwareSnapshot {
  return create(HardwareSnapshotSchema, {
    accelerators: devices.length
      ? [
          {
            result: {
              case: "facts",
              value: {
                facts: devices.map((device) => ({
                  deviceId: device.deviceId,
                  memoryArchitecture: device.vram
                    ? AcceleratorMemoryArchitecture.DEDICATED
                    : AcceleratorMemoryArchitecture.UNIFIED,
                  model: device.model ?? "GPU",
                  supportedSignals: device.supportedSignals ?? [
                    AcceleratorSignal.UTILIZATION,
                    AcceleratorSignal.POWER,
                  ],
                  totalMemoryBytes: device.vram ? BigInt(device.vram.totalGib * gib) : undefined,
                  usedMemoryBytes:
                    device.vram?.usedGib === undefined
                      ? undefined
                      : BigInt(device.vram.usedGib * gib),
                  vendor: AcceleratorVendor.NVIDIA,
                })),
              },
            },
            vendor: AcceleratorVendor.NVIDIA,
          },
        ]
      : [],
    host: {
      result: {
        case: "facts",
        value: {
          availableMemoryBytes: BigInt(memory.availableGib * gib),
          cpuCount,
          filesystems: filesystemGib.map((size, index) => ({
            fstype: "ext4",
            mountpoint: index ? `/data${index}` : "/",
            sizeBytes: BigInt(size * gib),
          })),
          hostName: "host",
          totalMemoryBytes: BigInt(memory.totalGib * gib),
        },
      },
    },
    observedAt: { seconds: BigInt(Math.floor(Date.now() / 1000) - 1) },
  });
}

/**
 * The node memory an idle machine reports for a test snapshot: a pool per dedicated GPU, then its
 * host pool, each holding its used memory as other use. Unified GPUs add no pool.
 */
export function idleNodeMemory(machineId: string, snapshot: HardwareSnapshot): NodeMemory {
  const devices = snapshot.accelerators.flatMap((observation) =>
    observation.result?.case === "facts" ? observation.result.value.facts : [],
  );
  const host = snapshot.host?.result;
  const otherUse = (bytes: bigint) => [{ bytes, source: MemorySegmentSource.OTHER }];

  return create(NodeMemorySchema, {
    observedAt: snapshot.observedAt,
    pools: [
      ...devices.flatMap((device, index) =>
        device.memoryArchitecture === AcceleratorMemoryArchitecture.DEDICATED
          ? [
              {
                pool: {
                  deviceId: device.deviceId,
                  deviceIndex: index,
                  deviceName: device.model,
                  kind: MemoryPoolKind.DEVICE,
                  segments: otherUse(device.usedMemoryBytes ?? 0n),
                  totalBytes: device.totalMemoryBytes,
                },
              },
            ]
          : [],
      ),
      ...(host?.case === "facts"
        ? [
            {
              pool: {
                kind: MemoryPoolKind.HOST,
                segments: otherUse(host.value.totalMemoryBytes - host.value.availableMemoryBytes),
                totalBytes: host.value.totalMemoryBytes,
              },
            },
          ]
        : []),
    ],
    siteNodeId: machineId,
    state: NodeMemoryState.AVAILABLE,
  });
}

/** A workstation: one dedicated RTX 5090 reporting every signal, 32 CPUs, two filesystems. */
export function workstationSnapshot(): HardwareSnapshot {
  return hardwareSnapshot({
    cpuCount: 32,
    devices: [
      { deviceId: "gpu-0", model: "NVIDIA RTX 5090", vram: { totalGib: 32, usedGib: 0.5 } },
    ],
    filesystemGib: [1863, 3726],
    memory: { availableGib: 47, totalGib: 62 },
  });
}

/** A GB10 box: one unified GPU reporting every signal, 20 CPUs, one filesystem. */
export function gb10Snapshot(): HardwareSnapshot {
  return hardwareSnapshot({
    cpuCount: 20,
    devices: [{ deviceId: "gb10", model: "NVIDIA GB10" }],
    filesystemGib: [1000],
    memory: { availableGib: 31, totalGib: 119 },
  });
}

/** A Mac: one unified GPU without supported signals, 16 CPUs, one filesystem. */
export function macSnapshot(): HardwareSnapshot {
  return hardwareSnapshot({
    cpuCount: 16,
    devices: [{ deviceId: "m4", model: "Apple M4 Max", supportedSignals: [] }],
    filesystemGib: [1858],
    memory: { availableGib: 23, totalGib: 64 },
  });
}
