import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import {
  configuredExecutionTargetOptionLabel,
  defaultExecutionTarget,
  executionTargetName,
  executionTargetPresentation,
  placedExecutionTargetName,
} from "@/domain/execution-target";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetAvailabilitySchema,
  ExecutionTargetWorkloadKind,
  NodeObservationState,
  RuntimeTargetAvailabilitySchema,
} from "@/gen/dotcore/site/v1/service_pb";

function target(
  connectionState: NodeObservationState,
  runtimeCase: "available" | "unavailable" | "unsupported",
  overrides: {
    displayName?: string;
    executionTargetId?: string;
    siteManager?: boolean;
    siteNodeDisplayName?: string;
    siteNodeId?: string;
    workloadKind?: ExecutionTargetWorkloadKind;
  } = {},
) {
  return create(ConfiguredExecutionTargetSchema, {
    displayName: overrides.displayName ?? "2× NVIDIA GeForce RTX 5090",
    executionTargetId: overrides.executionTargetId ?? "gpu-a",
    siteNodeId: overrides.siteNodeId ?? "node-a",
    siteNodeDisplayName: overrides.siteNodeDisplayName ?? "Node A",
    siteManager: overrides.siteManager ?? false,
    platformLabel: "linux/amd64",
    acceleratorDeviceIds: ["GPU-1", "GPU-2"],
    workloadKind: overrides.workloadKind ?? ExecutionTargetWorkloadKind.MODEL,
    availability: create(ExecutionTargetAvailabilitySchema, {
      connectionState,
      runtimeAvailability: create(RuntimeTargetAvailabilitySchema, {
        availability:
          runtimeCase === "available"
            ? { case: "available", value: {} }
            : {
                case: runtimeCase,
                value: { code: runtimeCase.toUpperCase(), message: `${runtimeCase} message` },
              },
      }),
    }),
  });
}

describe("execution target presentation", () => {
  it("shows configured topology and permits an available site manager target", () => {
    const configured = target(NodeObservationState.CONNECTED, "available", { siteManager: true });

    expect(executionTargetPresentation(configured)).toEqual({
      connectionLabel: "Connected",
      deployable: true,
      deploymentGate: undefined,
      devicesLabel: "GPU-1, GPU-2",
      runtimeDetail: undefined,
      runtimeLabel: "Available",
      workloadLabel: "Model",
    });
    expect(configuredExecutionTargetOptionLabel(configured)).toBe(
      "Node A / 2× NVIDIA GeForce RTX 5090",
    );
  });

  it("permits a connected worker model target with an available runtime", () => {
    const configured = target(NodeObservationState.CONNECTED, "available");

    expect(executionTargetPresentation(configured)).toEqual({
      connectionLabel: "Connected",
      deployable: true,
      deploymentGate: undefined,
      devicesLabel: "GPU-1, GPU-2",
      runtimeDetail: undefined,
      runtimeLabel: "Available",
      workloadLabel: "Model",
    });
    expect(configuredExecutionTargetOptionLabel(configured)).toBe(
      "Node A / 2× NVIDIA GeForce RTX 5090",
    );
  });

  it("labels a gated worker target with its deployment gate right after its identity", () => {
    const gated = target(NodeObservationState.DISCONNECTED, "available");

    expect(configuredExecutionTargetOptionLabel(gated)).toBe(
      "Node A / 2× NVIDIA GeForce RTX 5090 — Machine is disconnected. — linux/amd64 — GPU-1, GPU-2 — Model — Disconnected — Runtime available",
    );
  });

  it("names every target by its node and its own display name, without its ID", () => {
    const first = target(NodeObservationState.CONNECTED, "available", {
      displayName: "NVIDIA GeForce RTX 5090 · GPU 0",
    });
    const second = target(NodeObservationState.CONNECTED, "available", {
      displayName: "NVIDIA GeForce RTX 5090 · GPU 1",
      executionTargetId: "gpu-b",
    });
    const application = target(NodeObservationState.CONNECTED, "available", {
      displayName: "Docker applications",
      executionTargetId: "apps",
      workloadKind: ExecutionTargetWorkloadKind.APPLICATION,
    });
    const otherNode = target(NodeObservationState.CONNECTED, "available", {
      displayName: "Apple M3 Max",
      executionTargetId: "gpu-c",
      siteNodeDisplayName: "Node B",
      siteNodeId: "node-b",
    });

    expect([first, second, application, otherNode].map(executionTargetName)).toEqual([
      "Node A / NVIDIA GeForce RTX 5090 · GPU 0",
      "Node A / NVIDIA GeForce RTX 5090 · GPU 1",
      "Node A / Docker applications",
      "Node B / Apple M3 Max",
    ]);
  });

  it("falls back to the SiteNode ID without a node display name", () => {
    const unnamed = target(NodeObservationState.CONNECTED, "available", {
      siteNodeDisplayName: "",
    });

    expect(executionTargetName(unnamed)).toBe("node-a / 2× NVIDIA GeForce RTX 5090");
  });

  it("falls back to the execution target ID without a target display name", () => {
    const unnamed = target(NodeObservationState.CONNECTED, "available", { displayName: "" });

    expect(executionTargetName(unnamed)).toBe("Node A / gpu-a");
  });

  it("names a placed target from the configured targets, or by its ID once removed", () => {
    const configured = target(NodeObservationState.CONNECTED, "available");
    const sameIdOtherNode = target(NodeObservationState.CONNECTED, "available", {
      displayName: "Apple M3 Max",
      siteNodeDisplayName: "Node B",
      siteNodeId: "node-b",
    });
    const targets = [sameIdOtherNode, configured];

    expect(
      placedExecutionTargetName({ executionTargetId: "gpu-a", siteNodeId: "node-a" }, targets),
    ).toBe("Node A / 2× NVIDIA GeForce RTX 5090");
    expect(
      placedExecutionTargetName({ executionTargetId: "gpu-z", siteNodeId: "node-a" }, targets),
    ).toBe("node-a / gpu-z");
    expect(
      placedExecutionTargetName(
        { executionTargetId: "gpu-z", siteNodeId: "node-a" },
        targets,
        "Node A",
      ),
    ).toBe("Node A / gpu-z");
  });

  it.each([
    [NodeObservationState.MISSING, "available", "Machine has not connected."],
    [NodeObservationState.STALE, "available", "Machine observation is overdue."],
    [NodeObservationState.DISCONNECTED, "available", "Machine is disconnected."],
    [NodeObservationState.DISCONNECTED, "unavailable", "Machine is disconnected."],
    [NodeObservationState.CONNECTED, "unsupported", "Runtime observation is unsupported."],
    [NodeObservationState.CONNECTED, "unavailable", "Runtime is unavailable."],
  ] as const)(
    "gates a model target with connection %s and runtime %s",
    (connection, runtime, gate) => {
      expect(executionTargetPresentation(target(connection, runtime))).toMatchObject({
        deployable: false,
        deploymentGate: gate,
      });
    },
  );

  it("permits a connected worker application target with an available runtime", () => {
    expect(
      executionTargetPresentation(
        target(NodeObservationState.CONNECTED, "available", {
          workloadKind: ExecutionTargetWorkloadKind.APPLICATION,
        }),
      ),
    ).toMatchObject({ deployable: true, deploymentGate: undefined, workloadLabel: "Application" });
  });

  it.each([
    [NodeObservationState.MISSING, "Machine has not connected."],
    [NodeObservationState.STALE, "Machine observation is overdue."],
    [NodeObservationState.DISCONNECTED, "Machine is disconnected."],
  ] as const)("gates a worker application target with connection %s", (connection, gate) => {
    expect(
      executionTargetPresentation(
        target(connection, "available", { workloadKind: ExecutionTargetWorkloadKind.APPLICATION }),
      ),
    ).toMatchObject({ deployable: false, deploymentGate: gate });
  });

  it("defaults to the site manager's target before a deployable worker target", () => {
    const worker = target(NodeObservationState.CONNECTED, "available", { siteNodeId: "worker" });
    const manager = target(NodeObservationState.CONNECTED, "available", {
      siteManager: true,
      siteNodeId: "manager",
    });
    const unavailableManager = target(NodeObservationState.CONNECTED, "unavailable", {
      siteManager: true,
      siteNodeId: "manager",
    });
    const staleManager = target(NodeObservationState.STALE, "available", {
      siteManager: true,
      siteNodeId: "manager",
    });
    const staleWorker = target(NodeObservationState.STALE, "available", { siteNodeId: "worker" });

    expect(defaultExecutionTarget([worker, manager])).toBe(manager);
    expect(defaultExecutionTarget([unavailableManager, worker])).toBe(worker);
    expect(defaultExecutionTarget([unavailableManager, staleWorker])).toBeUndefined();
    expect(defaultExecutionTarget([staleManager, worker])).toBe(worker);
  });

  it("does not infer fit from a target without devices or observations", () => {
    const configured = create(ConfiguredExecutionTargetSchema, {
      executionTargetId: "cpu",
      siteNodeId: "node-b",
      platformLabel: "linux/arm64",
      workloadKind: ExecutionTargetWorkloadKind.APPLICATION,
    });

    expect(executionTargetPresentation(configured)).toEqual({
      connectionLabel: "Unknown",
      deployable: false,
      deploymentGate: "Target availability has not been reported.",
      devicesLabel: "None configured",
      runtimeDetail: undefined,
      runtimeLabel: "Unknown",
      workloadLabel: "Application",
    });
  });
});
