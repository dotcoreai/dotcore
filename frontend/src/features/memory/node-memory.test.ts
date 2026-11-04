import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { runningDeployment } from "@/features/models/model-deploy-memory-test-support";
import { ApplicationDeploymentSchema } from "@/gen/dotcore/application/v1/deployment_pb";
import { NodeMemoryState } from "@/gen/dotcore/capacity/v1/memory_pb";
import { AcceleratorMemoryObservationSchema } from "@/gen/dotcore/deployment/v1/accelerator_memory_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

import { projectNodeMemory, projectSiteMemory, type SiteMemoryInputs } from "./node-memory";
import {
  appSegment,
  claimed,
  devicePool,
  gib,
  hostPool,
  measured,
  mib,
  nodeMemory,
  other,
  unsized,
} from "./node-memory-test-support";
import { pastTotalCapacity } from "./pool-spans";
import { machineMemory } from "./site-memory";

const nodeA = { machineId: "node-a", name: "gpu1" };
const nodeB = { machineId: "node-b", name: "mac1" };

describe("projectSiteMemory", () => {
  it("draws a dedicated node's GPU as VRAM and its host as RAM, named after the deployments", () => {
    const memory = project({
      deployments: [runningDeployment("d-qwen", "Qwen3.5 4B")],
      nodes: [
        nodeMemory("node-a", [
          devicePool(32_607n * mib, [measured("d-qwen", 10n * gib), other(497n * mib)]),
          hostPool(64n * gib, [other(4n * gib)]),
        ]),
      ],
    });

    expect(memory.rows.map((row) => [row.kind, row.totalBytes, row.usedBytes])).toEqual([
      ["vram", 32_607n * mib, 10n * gib + 497n * mib],
      ["ram", 64n * gib, 4n * gib],
    ]);
    const [vram, ram] = memory.rows.map((row) => row.spans[0]);
    expect(
      vram.segments.map((segment) => [
        segment.name,
        segment.bytes,
        segment.source,
        segment.profileId,
      ]),
    ).toEqual([["Qwen3.5 4B", 10n * gib, "observed", "default"]]);
    expect([vram.otherBytes, vram.otherLabel, vram.overTotalBytes]).toEqual([
      497n * mib,
      "Other",
      0n,
    ]);
    expect(vram.devices).toEqual([
      {
        label: "NVIDIA GeForce RTX 5090",
        totalBytes: 32_607n * mib,
        usedBytes: 10n * gib + 497n * mib,
      },
    ]);
    expect([ram.segments, ram.otherBytes, ram.otherLabel]).toEqual([
      [],
      4n * gib,
      "System and other",
    ]);
    expect([memory.unavailable, memory.stale]).toEqual([[], false]);
    expect(pastTotalCapacity(memory.rows)).toEqual({});
  });

  it("sums mixed GPUs into one VRAM span with a line per GPU and a span per device", () => {
    const memory = project({
      deployments: [runningDeployment("d-qwen", "Qwen3.5 4B")],
      nodes: [
        nodeMemory("node-a", [
          devicePool(24n * gib, [other(1n * gib)], {
            deviceId: "GPU-0",
            deviceIndex: 0,
            deviceName: "NVIDIA RTX 4090",
          }),
          devicePool(16n * gib, [measured("d-qwen", 6n * gib), other(1n * gib)], {
            deviceId: "GPU-1",
            deviceIndex: 1,
            deviceName: "NVIDIA RTX 4080",
          }),
          hostPool(64n * gib, [other(4n * gib)]),
        ]),
      ],
    });

    const [vram] = memory.rows[0].spans;
    expect([vram.totalBytes, vram.usedBytes, vram.otherBytes]).toEqual([
      40n * gib,
      8n * gib,
      2n * gib,
    ]);
    expect(
      vram.devices.map((device) => [device.label, device.totalBytes, device.usedBytes]),
    ).toEqual([
      ["NVIDIA RTX 4090 · GPU 0", 24n * gib, 1n * gib],
      ["NVIDIA RTX 4080 · GPU 1", 16n * gib, 7n * gib],
    ]);
    const devices = memory.machines.get("node-a")!.deviceSpans;
    expect(
      [...devices].map(([id, span]) => [
        id,
        span.totalBytes,
        span.segments.map((segment) => segment.deploymentId),
        span.devices,
      ]),
    ).toEqual([
      ["GPU-0", 24n * gib, [], []],
      ["GPU-1", 16n * gib, ["d-qwen"], []],
    ]);
  });

  it("draws a unified node's host pool alone, as RAM, keeping site order across nodes", () => {
    const memory = project({
      deployments: [runningDeployment("d-qwen", "Qwen3.5 4B")],
      nodes: [
        nodeMemory("node-a", [devicePool(32n * gib, []), hostPool(64n * gib, [other(4n * gib)])]),
        nodeMemory("node-b", [
          hostPool(64n * gib, [measured("d-qwen", 4n * gib), other(8n * gib)]),
        ]),
      ],
    });

    expect(
      memory.rows.map((row) => [row.kind, row.spans.map((span) => span.machine.name)]),
    ).toEqual([
      ["vram", ["gpu1"]],
      ["ram", ["gpu1", "mac1"]],
    ]);
    expect(machineMemory(memory, "node-b").rows.map((row) => row.kind)).toEqual(["ram"]);
    expect(
      machineMemory(memory, "node-b").rows[0].spans[0].segments.map((segment) => segment.bytes),
    ).toEqual([4n * gib]);
  });

  it("lists a pool of unknown size as unreadable with the deployments it holds", () => {
    const memory = project({
      deployments: [runningDeployment("d-qwen", "Qwen3.5 4B")],
      nodes: [
        nodeMemory("node-a", [
          devicePool(undefined, [], {}, [unsized({ modelDeploymentId: "d-qwen" })]),
          hostPool(64n * gib, [claimed("d-qwen", 256n * mib), other(4n * gib)]),
        ]),
      ],
    });

    expect(memory.rows.map((row) => row.kind)).toEqual(["ram"]);
    expect(memory.machines.get("node-a")!.deviceSpans.size).toBe(0);
    expect(
      memory.unavailable.map((entry) => [
        entry.machine.name,
        entry.kind,
        entry.deployments.map((ref) => ref.name),
      ]),
    ).toEqual([["gpu1", "vram", ["Qwen3.5 4B"]]]);
  });

  it("marks a claim past the total with the capacity component's over-total bytes", () => {
    const memory = project({
      deployments: [runningDeployment("d-big", "Big model")],
      nodes: [
        nodeMemory("node-a", [
          devicePool(32_607n * mib, [claimed("d-big", 42n * gib), other(497n * mib)], {
            overTotalBytes: 42n * gib + 497n * mib - 32_607n * mib,
          }),
          hostPool(64n * gib, [other(4n * gib)]),
        ]),
      ],
    });

    const [vram] = memory.rows;
    expect(vram.spans[0].segments.map((segment) => [segment.bytes, segment.source])).toEqual([
      [42n * gib, "claimed"],
    ]);
    expect(vram.usedBytes).toBe(42n * gib + 497n * mib);
    expect(pastTotalCapacity(memory.rows)).toEqual({
      vram: {
        beyondTotal: true,
        headroomPercent: 0,
        lineBytes: 32_607n * mib,
        overBytes: 42n * gib + 497n * mib - 32_607n * mib,
      },
    });
  });

  it("lists unsized deployments with the reason their own observation gives", () => {
    const notApplicable = create(AcceleratorMemoryObservationSchema, {
      result: { case: "notApplicable", value: {} },
    });
    const starting = runningDeployment("d-starting", "Starting model");
    starting.state = { ...starting.state!, status: ModelDeploymentStatus.PREPARING };
    starting.acceleratorMemory = notApplicable;
    const pending = runningDeployment("d-pending", "Pending model");
    pending.acceleratorMemory = create(AcceleratorMemoryObservationSchema, {
      result: {
        case: "unavailable",
        value: { code: "WORKLOAD_PROCESS_MEMORY_PENDING", message: "pending" },
      },
    });
    const ready = runningDeployment("d-ready", "Ready model");
    ready.acceleratorMemory = notApplicable;
    // A model not ready yet reads so whatever its measurement says, or before it has one.
    const loading = runningDeployment("d-loading", "Loading model");
    loading.state = { ...loading.state!, status: ModelDeploymentStatus.LOADING };
    loading.acceleratorMemory = undefined;
    const loadingPending = runningDeployment("d-loading-pending", "Loading pending model");
    loadingPending.state = { ...loadingPending.state!, status: ModelDeploymentStatus.LOADING };
    loadingPending.acceleratorMemory = pending.acceleratorMemory;
    const memory = project({
      deployments: [
        starting,
        pending,
        ready,
        runningDeployment("d-unread", "Unread model"),
        loading,
        loadingPending,
      ],
      nodes: [
        nodeMemory("node-a", [
          devicePool(32n * gib, [other(1n * gib)], {}, [
            unsized({ modelDeploymentId: "d-starting" }),
            unsized({ modelDeploymentId: "d-pending" }),
            unsized({ modelDeploymentId: "d-ready" }),
            unsized({ modelDeploymentId: "d-unread" }),
            unsized({ modelDeploymentId: "d-loading" }),
            unsized({ modelDeploymentId: "d-loading-pending" }),
            unsized({ modelDeploymentId: "d-new" }),
          ]),
        ]),
      ],
    });

    expect(memory.rows[0].spans[0].unmeasured.map((usage) => [usage.name, usage.reason])).toEqual([
      ["Starting model", "notReady"],
      ["Pending model", "pending"],
      ["Ready model", "noEstimate"],
      ["Unread model", "unavailable"],
      ["Loading model", "notReady"],
      ["Loading pending model", "notReady"],
      ["Model deployment", "notReady"],
    ]);
  });

  it("draws a segment whose deployment isn't listed yet under a generic name", () => {
    const memory = project({
      nodes: [
        nodeMemory("node-a", [
          devicePool(32n * gib, [
            claimed("d-new", 4n * gib),
            appSegment("a-new", 2n * gib),
            other(1n * gib),
          ]),
        ]),
      ],
    });

    expect(
      memory.rows[0].spans[0].segments.map((segment) => [
        segment.deploymentId,
        segment.colorKey,
        segment.name,
        segment.owner,
        segment.bytes,
      ]),
    ).toEqual([
      ["d-new", "d-new", "Model deployment", undefined, 4n * gib],
      ["a-new", "a-new", "Application deployment", "application", 2n * gib],
    ]);
    expect(memory.rows[0].spans[0].otherBytes).toBe(1n * gib);
  });

  it("keeps a model and an application with the same ID apart", () => {
    const memory = project({
      nodes: [
        nodeMemory("node-a", [
          devicePool(
            32n * gib,
            [measured("shared", 4n * gib), appSegment("shared", 2n * gib)],
            {},
            [
              unsized({ modelDeploymentId: "unsized" }),
              unsized({ applicationDeploymentId: "unsized" }),
            ],
          ),
        ]),
      ],
    });

    const [span] = memory.rows[0].spans;
    expect(
      span.segments.map((segment) => [segment.owner, segment.deploymentId, segment.bytes]),
    ).toEqual([
      [undefined, "shared", 4n * gib],
      ["application", "shared", 2n * gib],
    ]);
    expect(span.unmeasured.map((usage) => [usage.owner, usage.deploymentId])).toEqual([
      [undefined, "unsized"],
      ["application", "unsized"],
    ]);
  });

  it("names application segments after the application deployments", () => {
    const memory = project({
      applications: [
        create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "a-webui",
          applicationDeploymentName: "Open WebUI",
        }),
      ],
      nodes: [nodeMemory("node-a", [devicePool(32n * gib, [appSegment("a-webui", 3n * gib)])])],
    });

    expect(
      memory.rows[0].spans[0].segments.map((segment) => [segment.name, segment.owner]),
    ).toEqual([["Open WebUI", "application"]]);
  });

  it("marks a stale node's spans stale and its unsized deployments as lacking a reading", () => {
    const memory = project({
      deployments: [runningDeployment("d-qwen", "Qwen3.5 4B")],
      nodes: [
        nodeMemory(
          "node-a",
          [devicePool(32n * gib, [], {}, [unsized({ modelDeploymentId: "d-qwen" })])],
          NodeMemoryState.STALE,
        ),
      ],
    });

    const [span] = memory.rows[0].spans;
    expect([span.stale, memory.stale]).toEqual([true, true]);
    expect(span.unmeasured.map((usage) => usage.reason)).toEqual(["stale"]);
  });

  it("lists an unavailable node as unreadable and leaves out unlisted machines", () => {
    const memory = project({
      nodes: [
        nodeMemory("node-a", [], NodeMemoryState.UNAVAILABLE),
        nodeMemory("node-c", [hostPool(64n * gib, [])]),
      ],
    });

    expect(memory.rows).toEqual([]);
    expect(memory.unavailable).toEqual([{ deployments: [], machine: nodeA }]);
  });

  it("marks every span stale after a failed read", () => {
    const memory = project({
      nodes: [nodeMemory("node-a", [hostPool(64n * gib, [])])],
      readFailed: true,
    });

    expect(memory.rows[0].spans[0].stale).toBe(true);
  });
});

describe("projectNodeMemory", () => {
  it("projects one node for its machine's inspector", () => {
    const memory = projectNodeMemory(
      nodeMemory("node-a", [devicePool(32n * gib, [other(1n * gib)])]),
      nodeA,
      { deployments: [], readFailed: false },
    );

    expect(memory.spans.map((span) => [span.kind, span.machine, span.usedBytes])).toEqual([
      ["vram", nodeA, 1n * gib],
    ]);
    expect([...memory.deviceSpans.keys()]).toEqual(["GPU-a"]);
  });
});

function project(inputs: Partial<SiteMemoryInputs> & Pick<SiteMemoryInputs, "nodes">) {
  return projectSiteMemory({
    deployments: [],
    machines: [nodeA, nodeB],
    readFailed: false,
    ...inputs,
  });
}
