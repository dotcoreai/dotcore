import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  appSegment,
  devicePool,
  hostPool,
  measured,
  nodeMemory,
  other,
} from "@/features/memory/node-memory-test-support";
import { idleMetricsClient } from "@/features/metrics/metric-test-support";
import { ApplicationDeploymentSchema } from "@/gen/dotcore/application/v1/deployment_pb";
import { NodeMemoryState } from "@/gen/dotcore/capacity/v1/memory_pb";
import { AcceleratorMemoryObservationSchema } from "@/gen/dotcore/deployment/v1/accelerator_memory_pb";
import { ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import {
  AcceleratorFactsSetSchema,
  AcceleratorMemoryArchitecture,
  AcceleratorObservationSchema,
  AcceleratorVendor,
  HardwareSnapshotSchema,
  HostFactsSchema,
  HostObservationSchema,
  ObservationUnavailableSchema,
} from "@/gen/dotcore/observation/v1/observation_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  NodeObservationFreshnessSchema,
  NodeObservationState,
  type RuntimeTargetAvailabilitySchema,
} from "@/gen/dotcore/site/v1/service_pb";
import { setupUser } from "@/test/user";

import { MachineDetailContent } from "./machine-details";

vi.mock("uplot", () => ({
  default: class {
    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

afterEach(() => {
  vi.useRealTimers();
});

const gib = 1024n ** 3n;
const hostFacts = create(HostFactsSchema, {
  availableMemoryBytes: 34359738368n,
  cpuCount: 32,
  hostName: "machine-host",
  operatingSystemOrKernel: "Linux 6.12.107",
  filesystems: [{ fstype: "ext4", mountpoint: "/", sizeBytes: 1099511627776n }],
  totalMemoryBytes: 68719476736n,
});

/**
 * The one reason the host and a vendor can report identically, which is what the shared-reason path
 * collapses into a single section. Codes are the node's own (`hardware.rs` `unavailable`).
 */
const sharedCollectionFailure = create(ObservationUnavailableSchema, {
  code: "COLLECTION_FAILED",
  message: "Observation is unavailable because collection failed.",
});

/** What the node reports for a vendor whose devices are absent from this machine. */
const noDevice = create(ObservationUnavailableSchema, {
  code: "NO_DEVICE",
  message: "AMD observation is unavailable because no device is present.",
});

/** What the node reports for a vendor whose devices it could not read. */
const collectionFailed = create(ObservationUnavailableSchema, {
  code: "COLLECTION_FAILED",
  message: "AMD observation is unavailable because collection failed.",
});

const missingLibrary = create(ObservationUnavailableSchema, {
  code: "MISSING_LIBRARY",
  message: "NVIDIA observation is unavailable because its library is not available.",
});

const unsupported = create(ObservationUnavailableSchema, {
  code: "UNSUPPORTED",
  message: "AMD observation is unavailable because this host does not support it.",
});

function snapshot({
  accelerators = [],
  host,
}: {
  accelerators?: ReturnType<typeof create<typeof AcceleratorObservationSchema>>[];
  host?: ReturnType<typeof create<typeof HostObservationSchema>>;
}) {
  return create(HardwareSnapshotSchema, { accelerators, host, observedAt: { seconds: 1n } });
}

function hostObservation(available: boolean) {
  return create(HostObservationSchema, {
    result: available
      ? { case: "facts", value: hostFacts }
      : { case: "unavailable", value: sharedCollectionFailure },
  });
}

/** Two NVIDIA devices, the second without a complete memory observation. */
function nvidiaObservation(available: boolean) {
  return create(AcceleratorObservationSchema, {
    result: available
      ? {
          case: "facts",
          value: create(AcceleratorFactsSetSchema, {
            facts: [
              {
                deviceId: "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                driverOrRuntimeVersion: "560.35.03",
                model: "NVIDIA RTX 6000 Ada Generation",
                totalMemoryBytes: 51539607552n,
                usedMemoryBytes: 6442450944n,
                vendor: AcceleratorVendor.NVIDIA,
              },
              {
                deviceId: "GPU-11111111-2222-3333-4444-555555555555",
                driverOrRuntimeVersion: "560.35.03",
                model: "NVIDIA A100 80GB",
                vendor: AcceleratorVendor.NVIDIA,
              },
            ],
          }),
        }
      : { case: "unavailable", value: sharedCollectionFailure },
    vendor: AcceleratorVendor.NVIDIA,
  });
}

function unavailableObservation(
  vendor: AcceleratorVendor,
  unavailable: ReturnType<typeof create<typeof ObservationUnavailableSchema>>,
) {
  return create(AcceleratorObservationSchema, {
    result: { case: "unavailable", value: unavailable },
    vendor,
  });
}

function amdObservation(
  unavailable: ReturnType<typeof create<typeof ObservationUnavailableSchema>>,
) {
  return unavailableObservation(AcceleratorVendor.AMD, unavailable);
}

/** A vendor that reported facts but listed no device. */
function emptyAppleObservation() {
  return create(AcceleratorObservationSchema, {
    result: { case: "facts", value: create(AcceleratorFactsSetSchema, { facts: [] }) },
    vendor: AcceleratorVendor.APPLE,
  });
}

/** Accessible names of the rendered sections, in order. */
function sectionNames(): string[] {
  const wrapper = document.querySelector(".inspector-sections");
  return [...(wrapper?.querySelectorAll(":scope > section") ?? [])].map(
    (section) =>
      document.getElementById(section.getAttribute("aria-labelledby") ?? "")?.textContent ?? "",
  );
}

/** The metric row labels of a section, in order. */
function metricRowLabels(section: HTMLElement): string[] {
  return [
    ...section.querySelectorAll('[data-slot="metric-row-toggle"], [data-slot="metric-row-label"]'),
  ].map((toggle) => toggle.textContent ?? "");
}

function localDeployment(
  name: string,
  devices: Array<{ deviceId: string; usedMemoryBytes: bigint }>,
) {
  return create(ModelDeploymentSchema, {
    acceleratorMemory: create(AcceleratorMemoryObservationSchema, {
      result: {
        case: "available",
        value: { devices: devices.map((device) => ({ ...device, managedNodeId: "node-a" })) },
      },
    }),
    modelDeploymentId: "deployment-1",
    modelDeploymentName: name,
    snapshot: {
      source: { case: "local", value: { modelDisplayName: name, modelId: "model-1" } },
    },
    state: {
      observedPlacements: [{ managedNodeId: "node-a" }],
      status: ModelDeploymentStatus.READY,
    },
  });
}

function unifiedObservation() {
  return create(AcceleratorObservationSchema, {
    result: {
      case: "facts",
      value: create(AcceleratorFactsSetSchema, {
        facts: [
          {
            deviceId: "GPU-gb10",
            driverOrRuntimeVersion: "580.95.05",
            memoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
            model: "NVIDIA GB10",
            vendor: AcceleratorVendor.NVIDIA,
          },
        ],
      }),
    },
    vendor: AcceleratorVendor.NVIDIA,
  });
}

/** The Apple GPU as a Mac's node reports it: unified, with a working-set limit and no driver. */
function appleObservation() {
  return create(AcceleratorObservationSchema, {
    result: {
      case: "facts",
      value: create(AcceleratorFactsSetSchema, {
        facts: [
          {
            deviceId: "apple-gpu-0",
            memoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
            model: "Apple M3 Max",
            vendor: AcceleratorVendor.APPLE,
            workingSetLimitBytes: 30150672384n,
          },
        ],
      }),
    },
    vendor: AcceleratorVendor.APPLE,
  });
}

/** Node A's memory read holding `pools`, as the node memory watch delivers it. */
function nodeAMemory(
  pools: Parameters<typeof nodeMemory>[1],
  state = NodeMemoryState.AVAILABLE,
): { failed: boolean; value: ReturnType<typeof nodeMemory>[] } {
  return { failed: false, value: [nodeMemory("node-a", pools, state)] };
}

/** Node A's host pool: 64 GiB with 32 GiB in use. */
function hostMemory() {
  return hostPool(64n * gib, [other(32n * gib)]);
}

/** Node A's RTX 6000 pool of 48 GiB holding `segments`, and the A100 of unknown size. */
function nvidiaMemory(segments = [other(6n * gib)]) {
  return [
    devicePool(48n * gib, segments, {
      deviceId: "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
      deviceIndex: 0,
      deviceName: "NVIDIA RTX 6000 Ada Generation",
    }),
    devicePool(undefined, [], {
      deviceId: "GPU-11111111-2222-3333-4444-555555555555",
      deviceIndex: 1,
      deviceName: "NVIDIA A100 80GB",
    }),
  ];
}

function nonSectionChildren(): string[] {
  const wrapper = document.querySelector(".inspector-sections");
  return [...(wrapper?.children ?? [])]
    .filter((child) => child.tagName !== "SECTION")
    .map((child) => child.tagName);
}

describe("MachineDetailContent", () => {
  it("omits filesystem rows when the host lists no filesystem", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        machineName="Machine A"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [],
            host: create(HostObservationSchema, {
              result: { case: "facts", value: { ...hostFacts, filesystems: [] } },
            }),
          }),
        }}
      />,
    );

    const hardware = screen.getByRole("region", { name: "Hardware" });
    expect([...hardware.querySelectorAll("dt")].map((label) => label.textContent)).toEqual([
      "Host name",
      "OS",
      "Logical CPUs",
    ]);
  });

  it("renders Hardware facts, then Resources, Accelerators and Storage as metric rows", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        machineName="Machine A"
        nodeMemory={nodeAMemory([...nvidiaMemory(), hostMemory()])}
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [nvidiaObservation(true)],
            host: hostObservation(true),
          }),
        }}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators", "Storage"]);
    expect(nonSectionChildren()).toEqual([]);

    // Filesystems are Storage rows, not Hardware facts.
    const hardware = screen.getByRole("region", { name: "Hardware" });
    expect([...hardware.querySelectorAll("dt")].map((label) => label.textContent)).toEqual([
      "Host name",
      "OS",
      "Logical CPUs",
    ]);
    expect(within(hardware).getByText("machine-host")).toBeVisible();
    expect(within(hardware).queryByRole("img")).not.toBeInTheDocument();

    // Host memory is the RAM row, naming the GPU pool of unknown size; CPU has no reading until
    // the metrics answer.
    const resources = screen.getByRole("region", { name: "Resources" });
    expect(metricRowLabels(resources)).toEqual(["RAM", "CPU"]);
    expect(within(resources).getByRole("img")).toHaveAttribute(
      "aria-label",
      "RAM on Machine A: 32 GiB used of 64 GiB; unreadable: Machine A VRAM",
    );
    expect(within(resources).getByRole("button", { name: "RAM 32 / 64 GiB" })).toBeVisible();

    // Each device is a group: its name, its facts, then its own VRAM row when measured.
    const accelerators = screen.getByRole("region", { name: "Accelerators" });
    expect(within(accelerators).getByText("2 devices")).toBeVisible();
    expect(detailListTexts(accelerators)).toEqual([
      ["Runtime version", "560.35.03"],
      ["Runtime version", "560.35.03", "VRAM", "Unavailable"],
    ]);
    const first = within(accelerators).getByRole("group", {
      name: "GPU 0 · NVIDIA RTX 6000 Ada Generation",
    });
    expect(metricRowLabels(first)).toEqual(["VRAM"]);
    expect(within(first).getByRole("img")).toHaveAttribute(
      "aria-label",
      "VRAM on Machine A (NVIDIA RTX 6000 Ada Generation · GPU 0): 6 GiB used of 48 GiB",
    );
    expect(within(first).getByRole("button", { name: "VRAM 6 / 48 GiB" })).toBeVisible();
    expect(
      metricRowLabels(
        within(accelerators).getByRole("group", { name: "GPU 1 · NVIDIA A100 80GB" }),
      ),
    ).toEqual([]);
    expect(within(accelerators).queryByText(/^GPU-/)).not.toBeInTheDocument();

    const storage = screen.getByRole("region", { name: "Storage" });
    expect(within(storage).getByText("1 filesystem")).toBeVisible();
    expect(metricRowLabels(storage)).toEqual(["/"]);
    expect(within(storage).getByText("— / 1 TiB")).toBeVisible();
  });

  it("attributes VRAM to an application beside a model on a GPU", () => {
    const deviceId = "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    render(
      <MachineDetailContent
        applicationDeployments={{
          failed: false,
          value: [
            create(ApplicationDeploymentSchema, {
              applicationDeploymentId: "app-comfy",
              applicationDeploymentName: "ComfyUI",
              serviceAcceleratorMemory: [
                {
                  acceleratorMemory: create(AcceleratorMemoryObservationSchema, {
                    result: {
                      case: "available",
                      value: {
                        devices: [{ deviceId, managedNodeId: "node-a", usedMemoryBytes: 2n * gib }],
                      },
                    },
                  }),
                  serviceName: "comfyui",
                },
              ],
            }),
          ],
        }}
        deployments={{
          failed: false,
          value: [localDeployment("Qwen3.8 27B", [{ deviceId, usedMemoryBytes: 4n * gib }])],
        }}
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [
              create(AcceleratorObservationSchema, {
                result: {
                  case: "facts",
                  value: create(AcceleratorFactsSetSchema, {
                    facts: [
                      {
                        deviceId,
                        driverOrRuntimeVersion: "560.35.03",
                        model: "NVIDIA RTX 6000 Ada Generation",
                        totalMemoryBytes: 48n * gib,
                        usedMemoryBytes: 7n * gib,
                        vendor: AcceleratorVendor.NVIDIA,
                      },
                    ],
                  }),
                },
                vendor: AcceleratorVendor.NVIDIA,
              }),
            ],
            host: hostObservation(true),
          }),
        }}
        machineId="node-a"
        metricsClient={idleMetricsClient()}
        nodeMemory={nodeAMemory([
          devicePool(
            48n * gib,
            [measured("deployment-1", 4n * gib), appSegment("app-comfy", 2n * gib), other(gib)],
            { deviceId, deviceIndex: 0, deviceName: "NVIDIA RTX 6000 Ada Generation" },
          ),
          hostMemory(),
        ])}
      />,
    );

    const [bar] = within(screen.getByRole("region", { name: "Accelerators" })).getAllByRole("img");
    expect(bar.getAttribute("aria-label")).toBe(
      "VRAM on node-a (NVIDIA RTX 6000 Ada Generation · GPU 0): 7 GiB used of 48 GiB; deployments: Qwen3.8 27B 4 GiB, ComfyUI 2 GiB",
    );
  });

  it("attributes VRAM to models on each GPU", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        deployments={{
          failed: false,
          value: [
            localDeployment("Qwen3.8 27B", [
              { deviceId: "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", usedMemoryBytes: 4n * gib },
            ]),
          ],
        }}
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [
              create(AcceleratorObservationSchema, {
                result: {
                  case: "facts",
                  value: create(AcceleratorFactsSetSchema, {
                    facts: [
                      {
                        deviceId: "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                        driverOrRuntimeVersion: "560.35.03",
                        model: "NVIDIA RTX 6000 Ada Generation",
                        totalMemoryBytes: 48n * gib,
                        usedMemoryBytes: 6n * gib,
                        vendor: AcceleratorVendor.NVIDIA,
                      },
                      {
                        deviceId: "GPU-idle",
                        driverOrRuntimeVersion: "560.35.03",
                        model: "NVIDIA RTX 6000 Ada Generation",
                        totalMemoryBytes: 48n * gib,
                        usedMemoryBytes: 0n,
                        vendor: AcceleratorVendor.NVIDIA,
                      },
                    ],
                  }),
                },
                vendor: AcceleratorVendor.NVIDIA,
              }),
            ],
            host: hostObservation(true),
          }),
        }}
        machineId="node-a"
        nodeMemory={nodeAMemory([
          devicePool(48n * gib, [measured("deployment-1", 4n * gib), other(2n * gib)], {
            deviceId: "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            deviceIndex: 0,
            deviceName: "NVIDIA RTX 6000 Ada Generation",
          }),
          devicePool(48n * gib, [], {
            deviceId: "GPU-idle",
            deviceIndex: 1,
            deviceName: "NVIDIA RTX 6000 Ada Generation",
          }),
          hostMemory(),
        ])}
      />,
    );

    // Each GPU's own bar holds only the model running on it: the model, then 2 GiB outside it.
    const accelerators = screen.getByRole("region", { name: "Accelerators" });
    const [first, second] = within(accelerators).getAllByRole("img");
    expect([first.getAttribute("aria-label"), second.getAttribute("aria-label")]).toEqual([
      "VRAM on node-a (NVIDIA RTX 6000 Ada Generation · GPU 0): 6 GiB used of 48 GiB; deployments: Qwen3.8 27B 4 GiB",
      "VRAM on node-a (NVIDIA RTX 6000 Ada Generation · GPU 1): 0 bytes used of 48 GiB",
    ]);
    expect([...first.children].map((segment) => (segment as HTMLElement).style.width)).toEqual([
      `${(4 / 48) * 100}%`,
      `${(2 / 48) * 100}%`,
    ]);
    expect(
      [...first.querySelectorAll("[data-deployment-id]")].map((segment) =>
        segment.getAttribute("data-deployment-id"),
      ),
    ).toEqual(["deployment-1"]);
    expect(first.closest("[data-stale]")).toHaveAttribute("data-stale", "false");
    expect(second.children).toHaveLength(0);
    expect(
      within(accelerators)
        .getAllByRole("button", { name: /^VRAM / })
        .map((readout) => readout.textContent),
    ).toEqual(["6 / 48 GiB", "0 / 48 GiB"]);

    // The machine level keeps host RAM only; VRAM is never summed across GPUs.
    expect(metricRowLabels(screen.getByRole("region", { name: "Resources" }))).toEqual([
      "RAM",
      "CPU",
    ]);
  });

  it("states memory it cannot read instead of drawing it", () => {
    const { unmount } = render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [nvidiaObservation(true)],
            host: hostObservation(true),
          }),
        }}
        machineId="node-a"
      />,
    );

    // A GPU without memory readings has no VRAM row; its group says so.
    const unmeasured = screen.getByRole("group", { name: "GPU 1 · NVIDIA A100 80GB" });
    expect(detailListTexts(unmeasured)).toEqual([
      ["Runtime version", "560.35.03", "VRAM", "Unavailable"],
    ]);
    expect(within(unmeasured).queryByRole("img")).not.toBeInTheDocument();
    unmount();

    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        deployments={{
          failed: false,
          value: [
            localDeployment("Qwen3.8 27B", [{ deviceId: "GPU-gb10", usedMemoryBytes: 9n * gib }]),
          ],
        }}
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [unifiedObservation()],
            host: hostObservation(false),
          }),
        }}
        machineId="node-a"
      />,
    );

    // A unified machine without host facts has no pool to draw its models in.
    const resources = screen.getByRole("region", { name: "Resources" });
    expect(within(resources).queryByRole("img")).not.toBeInTheDocument();
    expect(within(resources).getByText("Unavailable")).toBeVisible();
  });

  it("reads out each measured GPU, over capacity included, without host facts", () => {
    const facts = [
      {
        deviceId: "GPU-first",
        driverOrRuntimeVersion: "570.86.16",
        model: "NVIDIA RTX 5090",
        totalMemoryBytes: 8n * gib,
        usedMemoryBytes: 4n * gib,
        vendor: AcceleratorVendor.NVIDIA,
      },
      {
        deviceId: "GPU-second",
        driverOrRuntimeVersion: "570.86.16",
        model: "NVIDIA RTX 5090",
        totalMemoryBytes: 8n * gib,
        usedMemoryBytes: 0n,
        vendor: AcceleratorVendor.NVIDIA,
      },
      {
        deviceId: "GPU-missing-used",
        driverOrRuntimeVersion: "570.86.16",
        model: "NVIDIA RTX 5090",
        totalMemoryBytes: 8n * gib,
        vendor: AcceleratorVendor.NVIDIA,
      },
      {
        deviceId: "GPU-above-total",
        driverOrRuntimeVersion: "570.86.16",
        model: "NVIDIA RTX 5090",
        totalMemoryBytes: 8n * gib,
        usedMemoryBytes: 12n * gib,
        vendor: AcceleratorVendor.NVIDIA,
      },
    ];
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        nodeMemory={nodeAMemory([
          devicePool(8n * gib, [other(4n * gib)], { deviceId: "GPU-first", deviceIndex: 0 }),
          devicePool(8n * gib, [], { deviceId: "GPU-second", deviceIndex: 1 }),
          devicePool(undefined, [], { deviceId: "GPU-missing-used", deviceIndex: 2 }),
          devicePool(8n * gib, [other(12n * gib)], {
            deviceId: "GPU-above-total",
            deviceIndex: 3,
            overTotalBytes: 4n * gib,
          }),
        ])}
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [
              create(AcceleratorObservationSchema, {
                result: { case: "facts", value: create(AcceleratorFactsSetSchema, { facts }) },
                vendor: AcceleratorVendor.NVIDIA,
              }),
            ],
          }),
        }}
      />,
    );

    // Without host facts there is no RAM row and no Storage, but CPU still has its row.
    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators"]);
    const resources = screen.getByRole("region", { name: "Resources" });
    expect(metricRowLabels(resources)).toEqual(["CPU"]);
    expect(detailListTexts(resources)).toEqual([["Memory", "Unavailable"]]);

    // Each measured GPU reads out its own memory, use past its total flagged; the incomplete one
    // is unavailable.
    const accelerators = screen.getByRole("region", { name: "Accelerators" });
    expect(
      within(accelerators)
        .getAllByRole("button", { name: /^VRAM / })
        .map((readout) => readout.textContent),
    ).toEqual(["4 / 8 GiB", "0 / 8 GiB", "▲\u00a012 / 8 GiB"]);
    expect(
      within(accelerators).getByRole("button", { name: "VRAM 12 / 8 GiB, over capacity by 4 GiB" }),
    ).toBeVisible();
    expect(detailListTexts(accelerators)[2]).toEqual([
      "Runtime version",
      "570.86.16",
      "VRAM",
      "Unavailable",
    ]);
  });

  it("presents a unified machine's memory as its RAM", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({ accelerators: [unifiedObservation()], host: hostObservation(true) }),
        }}
        nodeMemory={nodeAMemory([hostMemory()])}
      />,
    );

    const nvidia = screen.getByRole("region", { name: "Accelerators" });
    expect(within(nvidia).getByText("GPU 0 · NVIDIA GB10")).toBeVisible();
    expect(
      within(nvidia)
        .getAllByRole("definition")
        .map((definition) => definition.textContent),
    ).toEqual(["580.95.05", "Unified"]);
    expect(within(nvidia).queryByRole("img")).not.toBeInTheDocument();

    const resources = screen.getByRole("region", { name: "Resources" });
    expect(metricRowLabels(resources)).toEqual(["RAM", "CPU"]);
    expect(within(resources).getByRole("img")).toHaveAttribute(
      "aria-label",
      "RAM on node-a: 32 GiB used of 64 GiB",
    );
    expect(within(resources).getByRole("button", { name: "RAM 32 / 64 GiB" })).toBeVisible();
  });

  it("shows an Apple GPU's unified memory and working-set limit without a driver row", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({ accelerators: [appleObservation()], host: hostObservation(true) }),
        }}
        nodeMemory={nodeAMemory([hostMemory()])}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators", "Storage"]);
    const apple = screen.getByRole("region", { name: "Accelerators" });
    expect(within(apple).getByText("1 device")).toBeVisible();
    expect(within(apple).getByText("GPU 0 · Apple M3 Max")).toBeVisible();
    expect(detailListTexts(apple)).toEqual([
      ["Memory", "Unified", "GPU working-set limit", "28.1 GiB"],
    ]);
    expect(within(apple).queryByRole("img")).not.toBeInTheDocument();

    // The shared pool is the machine's RAM; nothing is drawn as dedicated GPU memory.
    const resources = screen.getByRole("region", { name: "Resources" });
    expect(metricRowLabels(resources)).toEqual(["RAM", "CPU"]);
    expect(within(resources).getByRole("button", { name: "RAM 32 / 64 GiB" })).toBeVisible();
  });

  it("shows a dedicated machine's host RAM in Resources and its VRAM under the GPU", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [
              create(AcceleratorObservationSchema, {
                result: {
                  case: "facts",
                  value: create(AcceleratorFactsSetSchema, {
                    facts: [
                      {
                        deviceId: "GPU-dedicated",
                        driverOrRuntimeVersion: "570.86.16",
                        memoryArchitecture: AcceleratorMemoryArchitecture.DEDICATED,
                        model: "NVIDIA RTX 5090",
                        totalMemoryBytes: 32n * gib,
                        usedMemoryBytes: 2n * gib,
                        vendor: AcceleratorVendor.NVIDIA,
                      },
                    ],
                  }),
                },
                vendor: AcceleratorVendor.NVIDIA,
              }),
            ],
            host: hostObservation(true),
          }),
        }}
        nodeMemory={nodeAMemory([
          devicePool(32n * gib, [other(2n * gib)], { deviceId: "GPU-dedicated" }),
          hostMemory(),
        ])}
      />,
    );

    const resources = screen.getByRole("region", { name: "Resources" });
    expect(metricRowLabels(resources)).toEqual(["RAM", "CPU"]);
    expect(within(resources).getByRole("button", { name: "RAM 32 / 64 GiB" })).toBeVisible();
    const gpu = screen.getByRole("group", { name: "GPU 0 · NVIDIA RTX 5090" });
    expect(metricRowLabels(gpu)).toEqual(["VRAM"]);
    expect(within(gpu).getByRole("button", { name: "VRAM 2 / 32 GiB" })).toBeVisible();
  });

  it("retains the snapshot without observation age and shows a quiet failed update", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [nvidiaObservation(true)],
            host: hostObservation(true),
          }),
          updateFailure: {
            code: "COLLECTION_FAILED",
            message: "The latest hardware refresh could not collect accelerator facts.",
          },
        }}
        nodeMemory={nodeAMemory([...nvidiaMemory(), hostMemory()], NodeMemoryState.STALE)}
      />,
    );

    expect(screen.queryByText(/^Observed /)).not.toBeInTheDocument();
    const hardware = screen.getByRole("region", { name: "Hardware" });
    expect(within(hardware).getByText("machine-host")).toBeVisible();
    expect(within(hardware).getByText("Failed")).toBeVisible();
    expect(hardware).not.toHaveTextContent("COLLECTION_FAILED");
    expect(hardware).not.toHaveTextContent(
      "The latest hardware refresh could not collect accelerator facts.",
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    // A stale node memory reading stays visible but dimmed: host memory and the GPU's own bar.
    expect(
      [...document.querySelectorAll("[data-stale]")].map((reading) =>
        reading.getAttribute("data-stale"),
      ),
    ).toEqual(["true", "true"]);
  });

  it("states memory unavailable when the node memory read fails or doesn't list the machine", () => {
    const resourcesWith = (nodeMemory: {
      failed: boolean;
      value?: ReturnType<typeof nodeAMemory>["value"];
    }) => {
      const view = render(
        <MachineDetailContent
          metricsClient={idleMetricsClient()}
          machineId="node-a"
          detail={{
            kind: "success",
            snapshot: snapshot({
              accelerators: [nvidiaObservation(true)],
              host: hostObservation(true),
            }),
          }}
          nodeMemory={nodeMemory}
        />,
      );
      const resources = screen.getByRole("region", { name: "Resources" });
      const result = {
        bars: within(resources).queryAllByRole("img").length,
        details: detailListTexts(resources),
        gpu: detailListTexts(
          screen.getByRole("group", { name: "GPU 0 · NVIDIA RTX 6000 Ada Generation" }),
        ),
      };
      view.unmount();
      return result;
    };
    const unavailable = {
      bars: 0,
      details: [["Memory", "Unavailable"]],
      gpu: [["Runtime version", "560.35.03", "VRAM", "Unavailable"]],
    };

    expect(resourcesWith({ failed: true })).toEqual(unavailable);
    expect(resourcesWith({ failed: false, value: [nodeMemory("node-b", [hostMemory()])] })).toEqual(
      unavailable,
    );
    // A failed refresh keeps drawing the last reading.
    expect(
      resourcesWith({ ...nodeAMemory([...nvidiaMemory(), hostMemory()]), failed: true }),
    ).toMatchObject({ bars: 1, details: [] });
  });

  it("states each vendor's own failure when vendors fail for different reasons", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [nvidiaObservation(true), amdObservation(collectionFailed)],
            host: hostObservation(true),
          }),
        }}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators", "Storage"]);

    // The observed vendor keeps its devices; the failed vendor is one row without diagnostics.
    const accelerators = screen.getByRole("region", { name: "Accelerators" });
    expect(within(accelerators).getByText("2 devices")).toBeVisible();
    expect(detailListTexts(accelerators)).toEqual([
      ["Runtime version", "560.35.03"],
      ["Runtime version", "560.35.03", "VRAM", "Unavailable"],
      ["AMD", "Unavailable"],
    ]);
    expect(accelerators).not.toHaveTextContent("COLLECTION_FAILED");
    expect(accelerators).not.toHaveTextContent(collectionFailed.message);
  });

  it("keeps missing-library and unsupported vendors visible in order", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [
              unavailableObservation(AcceleratorVendor.NVIDIA, missingLibrary),
              amdObservation(unsupported),
            ],
            host: hostObservation(true),
          }),
        }}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators", "Storage"]);

    const accelerators = screen.getByRole("region", { name: "Accelerators" });
    expect(within(accelerators).getByText("0 devices")).toBeVisible();
    expect(detailListTexts(accelerators)).toEqual([
      ["NVIDIA", "Unavailable", "AMD", "Unavailable"],
    ]);
    expect(accelerators).not.toHaveTextContent("MISSING_LIBRARY");
    expect(accelerators).not.toHaveTextContent(missingLibrary.message);
    expect(accelerators).not.toHaveTextContent("UNSUPPORTED");
    expect(accelerators).not.toHaveTextContent(unsupported.message);
  });

  it("merges every vendor's devices into one Accelerators section with the summed count", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [nvidiaObservation(true), amdObservation(noDevice), appleObservation()],
            host: hostObservation(true),
          }),
        }}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators", "Storage"]);
    const accelerators = screen.getByRole("region", { name: "Accelerators" });
    expect(within(accelerators).getByText("3 devices")).toBeVisible();
    expect(detailListTexts(accelerators)).toEqual([
      ["Runtime version", "560.35.03"],
      ["Runtime version", "560.35.03", "VRAM", "Unavailable"],
      ["Memory", "Unified", "GPU working-set limit", "28.1 GiB"],
      ["AMD", "Unavailable"],
    ]);
  });

  it("states no detected devices when no vendor reports one and none failed", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [emptyAppleObservation()],
            host: hostObservation(true),
          }),
        }}
      />,
    );

    const accelerators = screen.getByRole("region", { name: "Accelerators" });
    expect(within(accelerators).getByText("0 devices")).toBeVisible();
    expect(detailListTexts(accelerators)).toEqual([["Devices", "None detected"]]);
  });

  it("keeps configured no-device and empty observations visible", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [
              nvidiaObservation(true),
              amdObservation(noDevice),
              emptyAppleObservation(),
            ],
            host: hostObservation(true),
          }),
        }}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators", "Storage"]);

    const accelerators = screen.getByRole("region", { name: "Accelerators" });
    expect(within(accelerators).getByText("2 devices")).toBeVisible();
    expect(detailListTexts(accelerators).at(-1)).toEqual(["AMD", "Unavailable"]);
    expect(accelerators).not.toHaveTextContent("NO_DEVICE");
    expect(accelerators).not.toHaveTextContent(noDevice.message);
  });

  it("states each configured section when host and vendor share an unavailable reason", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [nvidiaObservation(false)],
            host: hostObservation(false),
          }),
        }}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators"]);

    const hardware = screen.getByRole("region", { name: "Hardware" });
    expect(within(hardware).getByText("Unavailable")).toBeVisible();
    expect(within(hardware).getByText("COLLECTION_FAILED")).toBeVisible();
    expect(within(hardware).getByText(sharedCollectionFailure.message)).toBeVisible();

    const nvidia = screen.getByRole("region", { name: "Accelerators" });
    expect(within(nvidia).getByText("Unavailable")).toBeVisible();
    expect(within(nvidia).queryByText("COLLECTION_FAILED")).not.toBeInTheDocument();
    expect(within(nvidia).queryByText(sharedCollectionFailure.message)).not.toBeInTheDocument();

    // With neither pool readable, Resources says so instead of drawing empty bars.
    const memory = screen.getByRole("region", { name: "Resources" });
    expect(within(memory).queryByRole("img")).not.toBeInTheDocument();
    expect(within(memory).getByText("Unavailable")).toBeVisible();
  });

  it("keeps Hardware and states the vendor failure when only it fails", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [nvidiaObservation(false)],
            host: hostObservation(true),
          }),
        }}
        nodeMemory={nodeAMemory([hostMemory()])}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators", "Storage"]);
    expect(screen.getByRole("region", { name: "Hardware" })).toHaveTextContent("machine-host");
    // Host RAM stays readable even when the GPUs can't be read.
    const memory = screen.getByRole("region", { name: "Resources" });
    expect(metricRowLabels(memory)).toEqual(["RAM", "CPU"]);
    expect(within(memory).getByRole("img")).toHaveAttribute(
      "aria-label",
      "RAM on node-a: 32 GiB used of 64 GiB",
    );

    // The vendor remains visible in its own section without backend diagnostics.
    const nvidia = screen.getByRole("region", { name: "Accelerators" });
    expect(within(nvidia).getByText("Unavailable")).toBeVisible();
    expect(within(nvidia).queryByText("COLLECTION_FAILED")).not.toBeInTheDocument();
    expect(within(nvidia).queryByText("2 devices")).not.toBeInTheDocument();
  });

  it("keeps VRAM when host facts are missing", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [nvidiaObservation(true)],
            host: hostObservation(false),
          }),
        }}
        nodeMemory={nodeAMemory(nvidiaMemory())}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Resources", "Accelerators"]);
    expect(screen.getByRole("region", { name: "Hardware" })).toHaveTextContent("COLLECTION_FAILED");
    expect(metricRowLabels(screen.getByRole("region", { name: "Resources" }))).toEqual(["CPU"]);
    const gpu = screen.getByRole("group", { name: "GPU 0 · NVIDIA RTX 6000 Ada Generation" });
    expect(within(gpu).getByRole("button", { name: "VRAM 6 / 48 GiB" })).toBeVisible();
  });

  it("keeps loading, transport failure and unavailable states distinct inside a section", () => {
    const { rerender, unmount } = render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{ kind: "loading" }}
      />,
    );

    // The caller gates loading behind delayed loading, so a loading snapshot states it at once;
    // the rows every machine has hold their place empty.
    expect(sectionNames()).toEqual(["Hardware", "Resources"]);
    expect(nonSectionChildren()).toEqual([]);
    expect(screen.getByRole("status")).toHaveTextContent("Loading current hardware snapshot…");
    expect(metricRowLabels(screen.getByRole("region", { name: "Resources" }))).toEqual([
      "RAM",
      "CPU",
    ]);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    rerender(
      <MachineDetailContent
        detail={{ kind: "loading" }}
        loadingStatus={false}
        machineId="node-a"
        metricsClient={idleMetricsClient()}
      />,
    );
    expect(sectionNames()).toEqual(["Hardware", "Resources"]);
    expect(screen.queryByRole("status")).not.toBeInTheDocument();

    rerender(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{ kind: "error", message: "Could not load hardware details for this machine." }}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware"]);
    expect(screen.getByText("Unreachable")).toBeVisible();
    expect(screen.getByRole("alert")).toHaveTextContent(
      "Could not load hardware details for this machine.",
    );
    // A transport failure has no observation code to state.
    expect(screen.queryByText("Code")).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();

    unmount();
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{ kind: "unavailable", unavailable: sharedCollectionFailure }}
      />,
    );

    expect(sectionNames()).toEqual(["Hardware"]);
    const hardware = screen.getByRole("region", { name: "Hardware" });
    expect(within(hardware).getByText("Unavailable")).toBeVisible();
    expect(within(hardware).getByText("COLLECTION_FAILED")).toBeVisible();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });

  it("states a machine that never reported as Missing with no contact or report times", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        connection={create(NodeObservationFreshnessSchema, {
          state: NodeObservationState.MISSING,
        })}
        detail={{ kind: "unavailable", unavailable: sharedCollectionFailure }}
        machineId="node-a"
      />,
    );

    expect(sectionNames()).toEqual(["Status", "Hardware"]);
    const status = screen.getByRole("region", { name: "Status" });
    expect(detailListTexts(status)).toEqual([["Role", "Worker", "Connection", "Missing"]]);
    expect(within(status).queryByRole("paragraph")).not.toBeInTheDocument();
  });

  it("names the site manager's role beside its connection like any machine", () => {
    vi.useFakeTimers({ now: new Date("2026-09-30T12:02:00Z") });
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        connection={create(NodeObservationFreshnessSchema, {
          receivedAt: { seconds: 1_790_769_715n },
          state: NodeObservationState.CONNECTED,
        })}
        detail={{ kind: "unavailable", unavailable: sharedCollectionFailure }}
        machineId="node-a"
        siteManager
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    expect(detailListTexts(status)).toEqual([
      ["Role", "Site manager", "Connection", "Connected", "Last report", "5 seconds ago"],
    ]);
  });

  it.each([
    [NodeObservationState.CONNECTED, "Connected", undefined],
    [
      NodeObservationState.STALE,
      "Stale",
      "Observation overdue: the machine is in contact, but its latest report is late.",
    ],
    [
      NodeObservationState.DISCONNECTED,
      "Disconnected",
      "No recent contact from the machine; its last report is retained.",
    ],
  ])("states connection %s as %s with its last report", (state, label, explanation) => {
    vi.useFakeTimers({ now: new Date("2026-09-30T12:02:00Z") });
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        connection={create(NodeObservationFreshnessSchema, {
          receivedAt: { seconds: 1_790_769_600n },
          state,
        })}
        detail={{ kind: "unavailable", unavailable: sharedCollectionFailure }}
        machineId="node-a"
      />,
    );

    expect(sectionNames()).toEqual(["Status", "Hardware"]);
    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).getByText(label)).toBeVisible();
    expect(detailListTexts(status)).toEqual([
      ["Role", "Worker", "Connection", label, "Last report", "2 minutes ago"],
    ]);
    expect(within(status).getByText("2 minutes ago")).toHaveAttribute(
      "title",
      "2026-09-30 12:00:00 UTC",
    );
    if (explanation) {
      expect(within(status).getByText(explanation)).toBeVisible();
      expect(within(status).getByText(label)).toHaveAttribute("title", explanation);
    } else {
      expect(within(status).queryByRole("paragraph")).not.toBeInTheDocument();
    }
  });

  it("lists the machine's own execution targets with their runtime availability", () => {
    const targets = [
      executionTarget("node-a", "gpu-a", "NVIDIA GB10", ExecutionTargetWorkloadKind.MODEL, {
        case: "available",
        value: {},
      }),
      executionTarget(
        "node-a",
        "apps",
        "Docker applications",
        ExecutionTargetWorkloadKind.APPLICATION,
        {
          case: "unsupported",
          value: { code: "RUNTIME_UNSUPPORTED", message: "Runtime is unsupported." },
        },
      ),
      executionTarget("node-b", "gpu-b", "Apple M3 Max", ExecutionTargetWorkloadKind.MODEL, {
        case: "available",
        value: {},
      }),
    ];
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        detail={{ kind: "unavailable", unavailable: sharedCollectionFailure }}
        deployments={{
          failed: false,
          value: [
            placedDeployment("node-a", "gpu-a"),
            placedDeployment("node-a", "gpu-a"),
            placedDeployment("node-b", "gpu-a"),
          ],
        }}
        executionTargets={{ failed: false, value: targets }}
        machineId="node-a"
      />,
    );

    expect(sectionNames()).toEqual(["Hardware", "Execution targets"]);
    const section = screen.getByRole("region", { name: "Execution targets" });
    expect(within(section).getByText("2 targets")).toBeVisible();
    expect(within(section).queryByText("Apple M3 Max")).not.toBeInTheDocument();
    const rows = within(
      within(section).getByRole("list", { name: "Execution targets" }),
    ).getAllByRole("listitem");
    expect(rows.map((row) => row.textContent)).toEqual([
      "NVIDIA GB10Model · linux/arm64 · GPU-0 · 2 deployments",
      "Docker applicationsApplication · linux/arm64 · GPU-0Runtime is unsupported.Unsupported",
    ]);
    expect(within(rows[0]).queryByText("Available")).not.toBeInTheDocument();
    expect(within(rows[1]).getByText("Unsupported")).toBeVisible();
    expect(within(section).queryByRole("button")).not.toBeInTheDocument();
  });

  it("counts application deployments and opens a target from its row", async () => {
    const user = setupUser();
    const onSelect = vi.fn();
    const apps = executionTarget(
      "node-a",
      "apps",
      "Docker applications",
      ExecutionTargetWorkloadKind.APPLICATION,
      {
        case: "available",
        value: {},
      },
    );
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        applicationDeployments={{
          failed: false,
          value: [
            placedApplicationDeployment("node-a", "apps"),
            placedApplicationDeployment("node-b", "apps"),
          ],
        }}
        deployments={{ failed: false, value: [placedDeployment("node-a", "apps")] }}
        detail={{ kind: "unavailable", unavailable: sharedCollectionFailure }}
        executionTargets={{ failed: false, value: [apps] }}
        machineId="node-a"
        onSelectExecutionTarget={onSelect}
      />,
    );

    const row = screen.getByRole("button", { name: "View execution target Docker applications" });
    expect(row).toHaveTextContent(
      "Docker applicationsApplication · linux/arm64 · GPU-0 · 2 deployments",
    );
    await user.click(row);
    expect(onSelect).toHaveBeenCalledWith(apps);
  });

  it("orders model targets before application targets, each by display name", () => {
    const available = { case: "available", value: {} } as const;
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        detail={{ kind: "unavailable", unavailable: sharedCollectionFailure }}
        executionTargets={{
          failed: false,
          value: [
            executionTarget(
              "node-a",
              "a-apps",
              "Docker applications",
              ExecutionTargetWorkloadKind.APPLICATION,
              available,
            ),
            executionTarget(
              "node-a",
              "a-models",
              "NVIDIA GeForce RTX 5090 · GPU 1",
              ExecutionTargetWorkloadKind.MODEL,
              available,
            ),
            executionTarget(
              "node-a",
              "z-models",
              "NVIDIA GeForce RTX 5090 · GPU 0",
              ExecutionTargetWorkloadKind.MODEL,
              available,
            ),
          ],
        }}
        machineId="node-a"
      />,
    );

    const list = screen.getByRole("list", { name: "Execution targets" });
    expect(
      within(list)
        .getAllByRole("listitem")
        .map((row) => row.textContent),
    ).toEqual([
      "NVIDIA GeForce RTX 5090 · GPU 0Model · linux/arm64 · GPU-0",
      "NVIDIA GeForce RTX 5090 · GPU 1Model · linux/arm64 · GPU-0",
      "Docker applicationsApplication · linux/arm64 · GPU-0",
    ]);
  });

  it("states an unreadable target list and omits one still loading", () => {
    const { rerender } = render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        detail={{ kind: "unavailable", unavailable: sharedCollectionFailure }}
        executionTargets={{ failed: false }}
        machineId="node-a"
      />,
    );
    expect(sectionNames()).toEqual(["Hardware"]);

    rerender(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        detail={{ kind: "unavailable", unavailable: sharedCollectionFailure }}
        executionTargets={{ failed: true }}
        machineId="node-a"
      />,
    );
    const section = screen.getByRole("region", { name: "Execution targets" });
    expect(within(section).getByText("Unreachable")).toBeVisible();
  });

  it("renders no Card and no nested heading levels", () => {
    render(
      <MachineDetailContent
        metricsClient={idleMetricsClient()}
        machineId="node-a"
        detail={{
          kind: "success",
          snapshot: snapshot({
            accelerators: [nvidiaObservation(true)],
            host: hostObservation(true),
          }),
        }}
      />,
    );

    const wrapper = document.querySelector(".inspector-sections");
    expect(wrapper?.querySelector('[data-slot="card"]')).toBeNull();
    expect(wrapper?.querySelectorAll("h3, h4")).toHaveLength(0);
    expect(wrapper?.querySelectorAll("h2")).toHaveLength(4);
  });
});

function placedDeployment(siteNodeId: string, executionTargetId: string) {
  return create(ModelDeploymentSchema, {
    intent: { placement: { executionTargetId, siteNodeId } },
  });
}

function placedApplicationDeployment(siteNodeId: string, executionTargetId: string) {
  return create(ApplicationDeploymentSchema, { placement: { executionTargetId, siteNodeId } });
}

function executionTarget(
  siteNodeId: string,
  executionTargetId: string,
  displayName: string,
  workloadKind: ExecutionTargetWorkloadKind,
  availability: MessageInitShape<typeof RuntimeTargetAvailabilitySchema>["availability"],
) {
  return create(ConfiguredExecutionTargetSchema, {
    acceleratorDeviceIds: ["GPU-0"],
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: { availability },
    },
    displayName,
    executionTargetId,
    platformLabel: "linux/arm64",
    siteNodeId,
    workloadKind,
  });
}

/** Each detail list's label/value texts, in order. */
function detailListTexts(section: HTMLElement): string[][] {
  return [...section.querySelectorAll("dl")].map((list) =>
    [...list.children].map((child) => child.textContent ?? ""),
  );
}
