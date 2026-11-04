/** Why a deployment's memory measurement has no reading. */
export type MeasurementGap = "notRunning" | "pending" | "unavailable";

/**
 * The console words for an unavailable accelerator-memory code, shared by model and application
 * deployments. An unknown code reads as a generic unavailable measurement; the reported message is
 * detail behind the words, never the words themselves.
 */
export function unavailableMemoryReason(code: string): string {
  return unavailableMemoryCodes[code]?.reason ?? genericReason;
}

/** The measurement gap an unavailable code names; unknown codes are failures. */
export function measurementGap(code: string | undefined): MeasurementGap {
  return (code === undefined ? undefined : unavailableMemoryCodes[code]?.gap) ?? "unavailable";
}

const genericReason = "measurement unavailable";

/** Every unavailable code the attribution reports, with its gap and its words. */
const unavailableMemoryCodes: Record<string, { gap: MeasurementGap; reason: string }> = {
  NODE_OBSERVATION_PENDING: { gap: "pending", reason: "waiting for the first reading" },
  NODE_STATUS_UNAVAILABLE: { gap: "unavailable", reason: "machine not reporting" },
  PROCESS_MEMORY_UNAVAILABLE: { gap: "unavailable", reason: "process memory unavailable" },
  PROCESS_NOT_OBSERVED: { gap: "unavailable", reason: "process not seen using a GPU" },
  PROCESS_OBSERVATION_UNAVAILABLE: {
    gap: "unavailable",
    reason: "GPU process readings unavailable",
  },
  WORKLOAD_NODE_MISMATCH: { gap: "unavailable", reason: "process on another machine" },
  WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE: { gap: "unavailable", reason: "process not identified" },
  WORKLOAD_PROCESS_MEMORY_PENDING: { gap: "pending", reason: "waiting for the first reading" },
  WORKLOAD_PROCESS_MEMORY_UNAVAILABLE: { gap: "unavailable", reason: "process memory unavailable" },
  WORKLOAD_PROCESS_NOT_RUNNING: { gap: "notRunning", reason: "process not running" },
};
