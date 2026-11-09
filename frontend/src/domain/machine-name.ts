import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

/** The name the console shows for a machine: its display name, or its ID when it has none. */
export function machineName(machine: Machine): string {
  return machine.displayName || machine.machineId;
}
