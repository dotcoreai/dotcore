import { describe, expect, it } from "vitest";

import { measurementGap, unavailableMemoryReason } from "./unavailable-memory-codes";

describe("unavailable memory codes", () => {
  it.each([
    ["NODE_OBSERVATION_PENDING", "pending", "waiting for the first reading"],
    ["NODE_STATUS_UNAVAILABLE", "unavailable", "machine not reporting"],
    ["PROCESS_MEMORY_UNAVAILABLE", "unavailable", "process memory unavailable"],
    ["PROCESS_NOT_OBSERVED", "unavailable", "process not seen using a GPU"],
    ["PROCESS_OBSERVATION_UNAVAILABLE", "unavailable", "GPU process readings unavailable"],
    ["WORKLOAD_NODE_MISMATCH", "unavailable", "process on another machine"],
    ["WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE", "unavailable", "process not identified"],
    ["WORKLOAD_PROCESS_MEMORY_PENDING", "pending", "waiting for the first reading"],
    ["WORKLOAD_PROCESS_MEMORY_UNAVAILABLE", "unavailable", "process memory unavailable"],
    ["WORKLOAD_PROCESS_NOT_RUNNING", "notRunning", "process not running"],
  ])("reads %s as gap %s worded %s", (code, gap, reason) => {
    expect(measurementGap(code)).toBe(gap);
    expect(unavailableMemoryReason(code)).toBe(reason);
  });

  it("reads an unknown or missing code as a generic unavailable measurement", () => {
    expect(measurementGap("NEW_CODE")).toBe("unavailable");
    expect(measurementGap(undefined)).toBe("unavailable");
    expect(unavailableMemoryReason("NEW_CODE")).toBe("measurement unavailable");
  });
});
