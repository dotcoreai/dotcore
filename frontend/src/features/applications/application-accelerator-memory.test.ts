import { create } from "@bufbuild/protobuf";
import { TimestampSchema } from "@bufbuild/protobuf/wkt";
import { describe, expect, it } from "vitest";

import {
  ApplicationDeploymentSchema,
  ApplicationServiceAcceleratorMemorySchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  ApplicationDeploymentStateSchema,
  ApplicationLifecycleState,
} from "@/gen/dotcore/application/v1/state_pb";
import {
  type AcceleratorMemoryObservation,
  AcceleratorMemoryObservationSchema,
  AcceleratorMemoryRefreshStateSchema,
} from "@/gen/dotcore/deployment/v1/accelerator_memory_pb";

import {
  applicationAcceleratorMemory,
  formatUnitAcceleratorMemory,
  type UnitAcceleratorMemory,
} from "./application-accelerator-memory";

const OBSERVED_AT = create(TimestampSchema, { nanos: 0, seconds: 1_700_000_000n });

function available(devices: Array<{ deviceId: string; usedMemoryBytes: bigint }>) {
  return create(AcceleratorMemoryObservationSchema, {
    observedAt: OBSERVED_AT,
    result: { case: "available", value: { devices } },
  });
}

function unavailable(code: string, message: string) {
  return create(AcceleratorMemoryObservationSchema, {
    result: { case: "unavailable", value: { code, message } },
  });
}

const notApplicable = create(AcceleratorMemoryObservationSchema, {
  result: { case: "notApplicable", value: {} },
});

function withServices(
  services: Array<{ name: string; observation?: AcceleratorMemoryObservation }>,
  lifecycleState = ApplicationLifecycleState.READY,
) {
  return create(ApplicationDeploymentSchema, {
    applicationDeploymentId: "deployment-1",
    state: create(ApplicationDeploymentStateSchema, { lifecycleState }),
    serviceAcceleratorMemory: services.map(({ name, observation }) =>
      create(ApplicationServiceAcceleratorMemorySchema, {
        acceleratorMemory: observation,
        serviceName: name,
      }),
    ),
  });
}

describe("applicationAcceleratorMemory", () => {
  it("projects available usage per device in the order reported", () => {
    const memory = applicationAcceleratorMemory(
      withServices([
        {
          name: "web",
          observation: available([
            { deviceId: "GPU-a", usedMemoryBytes: 1_000n },
            { deviceId: "GPU-b", usedMemoryBytes: 2_000n },
          ]),
        },
      ]),
    );

    expect(memory).toEqual([
      {
        devices: [
          { bytes: 1_000n, deviceId: "GPU-a", managedNodeId: "" },
          { bytes: 2_000n, deviceId: "GPU-b", managedNodeId: "" },
        ],
        kind: "available",
        observedAt: OBSERVED_AT,
        serviceName: "web",
        stale: false,
      },
    ]);
  });

  it("reads an unavailable code in console words, with the reported message as detail", () => {
    const memory = applicationAcceleratorMemory(
      withServices([
        {
          name: "web",
          observation: unavailable(
            "PROCESS_NOT_OBSERVED",
            "An authoritative workload process was not observed on an accelerator.",
          ),
        },
      ]),
    );

    expect(memory).toEqual([
      {
        kind: "unavailable",
        reason: "process not seen using a GPU",
        reasonDetail: "An authoritative workload process was not observed on an accelerator.",
        serviceName: "web",
        stale: false,
      },
    ]);
  });

  it("reads an unknown code with an empty message as a generic reason, never a blank", () => {
    const memory = applicationAcceleratorMemory(
      withServices([{ name: "web", observation: unavailable("NEW_CODE", "") }]),
    );

    expect(memory).toEqual([
      {
        kind: "unavailable",
        reason: "measurement unavailable",
        reasonDetail: undefined,
        serviceName: "web",
        stale: false,
      },
    ]);
  });

  it("marks a reading stale only after a failed refresh", () => {
    const failedRefresh = available([{ deviceId: "GPU-a", usedMemoryBytes: 1n }]);
    failedRefresh.refresh = create(AcceleratorMemoryRefreshStateSchema, {
      outcome: { case: "failed", value: { code: "COLLECTION_FAILED", message: "" } },
    });
    const fresh = available([{ deviceId: "GPU-a", usedMemoryBytes: 1n }]);

    expect(
      applicationAcceleratorMemory(withServices([{ name: "web", observation: failedRefresh }])).map(
        (unit) => unit.stale,
      ),
    ).toEqual([true]);
    expect(
      applicationAcceleratorMemory(withServices([{ name: "web", observation: fresh }])).map(
        (unit) => unit.stale,
      ),
    ).toEqual([false]);
  });

  it.each([
    ApplicationLifecycleState.PAUSED,
    ApplicationLifecycleState.STOPPED,
    ApplicationLifecycleState.FAILED,
  ])("reads every unit of a deployment in lifecycle %s as not running", (lifecycleState) => {
    const memory = applicationAcceleratorMemory(
      withServices(
        [
          { name: "web", observation: notApplicable },
          { name: "worker", observation: unavailable("NODE_STATUS_UNAVAILABLE", "") },
        ],
        lifecycleState,
      ),
    );

    expect(memory).toEqual([
      { kind: "notRunning", serviceName: "web", stale: false },
      { kind: "notRunning", serviceName: "worker", stale: false },
    ]);
  });

  it("projects a simulated unit as not applicable", () => {
    const memory = applicationAcceleratorMemory(
      withServices([{ name: "web", observation: notApplicable }]),
    );

    expect(memory).toEqual([{ kind: "notApplicable", serviceName: "web", stale: false }]);
  });

  it("leaves out units the projection does not cover", () => {
    const memory = applicationAcceleratorMemory(
      withServices([
        { name: "web", observation: available([{ deviceId: "GPU-a", usedMemoryBytes: 1n }]) },
        { name: "db" },
      ]),
    );

    expect(memory.map((unit) => unit.serviceName)).toEqual(["web"]);
  });

  it("reads an available result without devices as absent, never a zero", () => {
    const memory = applicationAcceleratorMemory(
      withServices([{ name: "web", observation: available([]) }]),
    );

    expect(memory).toEqual([]);
  });

  it("reads nothing from a deployment without unit memory", () => {
    expect(applicationAcceleratorMemory(withServices([]))).toEqual([]);
    expect(
      applicationAcceleratorMemory(
        create(ApplicationDeploymentSchema, {
          applicationDeploymentId: "deployment-1",
        }),
      ),
    ).toEqual([]);
  });
});

describe("formatUnitAcceleratorMemory", () => {
  const unit = (partial: Partial<UnitAcceleratorMemory> & { serviceName: string }) =>
    ({ kind: "available", stale: false, ...partial }) satisfies UnitAcceleratorMemory;

  it("reads a single device as its bytes and keeps the exact figure and observation time in the title", () => {
    const formatted = formatUnitAcceleratorMemory(
      unit({
        devices: [{ bytes: 2_684_354_560n, deviceId: "GPU-a", managedNodeId: "" }],
        observedAt: OBSERVED_AT,
        serviceName: "web",
      }),
      new Date(1_700_000_600_000),
    );

    expect(formatted).toEqual({
      lines: ["2.5 GiB"],
      muted: false,
      stale: false,
      title: "GPU-a: 2684354560 bytes · observed 10 minutes ago",
    });
  });

  it("reads several devices as one named line per device", () => {
    const formatted = formatUnitAcceleratorMemory(
      unit({
        devices: [
          { bytes: 1_000n, deviceId: "GPU-a", managedNodeId: "" },
          { bytes: 2_048n, deviceId: "GPU-b", managedNodeId: "" },
        ],
        serviceName: "web",
      }),
    );

    expect(formatted).toEqual({
      lines: ["GPU-a: 1,000 bytes", "GPU-b: 2 KiB"],
      muted: false,
      stale: false,
      title: "GPU-a: 1000 bytes; GPU-b: 2048 bytes",
    });
  });

  it("shows a stale reading's observation time as its own line", () => {
    const formatted = formatUnitAcceleratorMemory(
      unit({
        devices: [{ bytes: 2_684_354_560n, deviceId: "GPU-a", managedNodeId: "" }],
        observedAt: OBSERVED_AT,
        serviceName: "web",
        stale: true,
      }),
      new Date(1_700_000_600_000),
    );

    expect(formatted).toEqual({
      lines: ["2.5 GiB", "stale, observed 10 minutes ago"],
      muted: false,
      stale: true,
      title: "GPU-a: 2684354560 bytes · observed 10 minutes ago",
    });
  });

  it.each([
    [unit({ kind: "notApplicable", serviceName: "web" }), "not applicable", undefined],
    [unit({ kind: "notRunning", serviceName: "web" }), "not running", undefined],
    [
      unit({
        kind: "unavailable",
        reason: "process not seen using a GPU",
        reasonDetail: "The reported reason.",
        serviceName: "web",
      }),
      "process not seen using a GPU",
      "The reported reason.",
    ],
  ])("reads %# muted, with any reported reason behind it", (input, line, title) => {
    expect(formatUnitAcceleratorMemory(input)).toEqual({
      lines: [line],
      muted: true,
      stale: false,
      title,
    });
  });
});
