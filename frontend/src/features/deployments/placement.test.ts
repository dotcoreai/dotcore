import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { ExactPlacementSchema } from "@/gen/dotcore/deployment/v1/placement_pb";
import { ConfiguredExecutionTargetSchema, MachineSchema } from "@/gen/dotcore/site/v1/service_pb";

import { machineName, placementLabel, placementMachine } from "./placement";

const placement = create(ExactPlacementSchema, {
  executionTargetId: "spark-gpu",
  siteNodeId: "node-51c47a4eca70f113",
});

function machine(displayName = "gb10-host-2") {
  return create(MachineSchema, { displayName, machineId: "node-51c47a4eca70f113" });
}

const targets = [
  create(ConfiguredExecutionTargetSchema, {
    displayName: "NVIDIA GeForce RTX 5090 · GPU 1",
    executionTargetId: "spark-gpu",
    siteNodeDisplayName: "gb10-host-2",
    siteNodeId: "node-51c47a4eca70f113",
  }),
  create(ConfiguredExecutionTargetSchema, {
    displayName: "Docker applications",
    executionTargetId: "spark-gpu",
    siteNodeDisplayName: "gpu1",
    siteNodeId: "node-other",
  }),
];

describe("placement label", () => {
  it("names the machine and the placed target's display name", () => {
    expect(placementLabel(placement, machine(), targets)).toBe(
      "gb10-host-2 / NVIDIA GeForce RTX 5090 · GPU 1",
    );
  });

  it("names a target no longer configured by its execution target ID", () => {
    expect(placementLabel(placement, machine(), targets.slice(1))).toBe("gb10-host-2 / spark-gpu");
    expect(placementLabel(placement, machine(), [])).toBe("gb10-host-2 / spark-gpu");
  });

  it("falls back to the node ID when the machine or its display name is unknown", () => {
    expect(placementLabel(placement, undefined, [])).toBe("node-51c47a4eca70f113 / spark-gpu");
    expect(placementLabel(placement, machine(""), [])).toBe("node-51c47a4eca70f113 / spark-gpu");
    expect(machineName("node-51c47a4eca70f113", undefined)).toBe("node-51c47a4eca70f113");
  });

  it("names only the machine until the targets are read", () => {
    expect(placementLabel(placement, machine(), undefined)).toBe("gb10-host-2");
    expect(placementLabel(placement, undefined, undefined)).toBe("node-51c47a4eca70f113");
  });

  it("states a missing placement as unavailable", () => {
    expect(placementLabel(undefined, undefined, targets)).toBe("Unavailable");
  });

  it("finds the machine a placement names", () => {
    const spark = machine();
    const other = create(MachineSchema, { displayName: "gpu1", machineId: "node-other" });

    expect(placementMachine([other, spark], placement.siteNodeId)).toBe(spark);
    expect(placementMachine([other], placement.siteNodeId)).toBeUndefined();
    expect(placementMachine(undefined, placement.siteNodeId)).toBeUndefined();
    expect(placementMachine([spark], undefined)).toBeUndefined();
  });
});
