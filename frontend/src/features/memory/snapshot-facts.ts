import {
  type AcceleratorFacts,
  AcceleratorMemoryArchitecture,
  type HardwareSnapshot,
} from "@/gen/dotcore/observation/v1/observation_pb";

import type { MemoryKind } from "./site-memory";

/**
 * The memory row a machine's model metrics belong to in history: RAM on a machine with a unified
 * GPU or no GPU, VRAM on one with dedicated GPUs. Undefined when the machine's GPUs can't be read,
 * since they may share a unified pool or own their memory.
 */
export function modelMemoryKind(snapshot: HardwareSnapshot): MemoryKind | undefined {
  const accelerators = acceleratorFacts(snapshot);
  if (accelerators.some(isUnified)) return "ram";
  if (accelerators.length) return "vram";

  const acceleratorsUnknown = snapshot.accelerators.some(
    (observation) => observation.result?.case !== "facts",
  );
  return acceleratorsUnknown ? undefined : "ram";
}

/** Every accelerator a snapshot lists, across vendors whose observation succeeded. */
export function acceleratorFacts(snapshot: HardwareSnapshot): AcceleratorFacts[] {
  return snapshot.accelerators.flatMap((observation) =>
    observation.result?.case === "facts" ? observation.result.value.facts : [],
  );
}

/** Whether a GPU owns its memory; a unified GPU allocates from the host's. */
export function isDedicated(device: AcceleratorFacts): boolean {
  return device.memoryArchitecture === AcceleratorMemoryArchitecture.DEDICATED;
}

function isUnified(device: AcceleratorFacts): boolean {
  return device.memoryArchitecture === AcceleratorMemoryArchitecture.UNIFIED;
}
