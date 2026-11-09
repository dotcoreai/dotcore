import {
  type AcceleratorFacts,
  AcceleratorMemoryArchitecture,
  type HardwareSnapshot,
} from "@/gen/dotcore/observation/v1/observation_pb";

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
