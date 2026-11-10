import { create } from "@bufbuild/protobuf";
import { render, screen, waitFor, within } from "@testing-library/react";
import { createRef } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { DeploymentLogOperations } from "@/api/deployment-log-client";
import { ChatOpenerContext } from "@/features/chat/chat-opener";
import {
  kvOffloadReleases,
  model,
  multiParameterReleases,
  snapshot,
} from "@/features/models/model-test-support";
import { projectModelRows } from "@/features/models/profiles/model-row";
import { modelWithKvCachePrecision } from "@/features/models/runtime/runtime-schema-test-support";
import { EngineParameterValuesSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { ModelSchema, SpeculativeDecodingMethod } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
  ModelDeploymentSchema,
  PortableModelLaunchPlanSchema,
  ResolvedLocalModelDeploymentSnapshotSchema,
  ResolvedSizingSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  ModelDeploymentProgressUnit,
  ModelDeploymentStateSchema,
  ModelDeploymentStatus,
} from "@/gen/dotcore/model/v1/state_pb";
import { ConfiguredExecutionTargetSchema, MachineSchema } from "@/gen/dotcore/site/v1/service_pb";
import * as clipboard from "@/lib/clipboard";
import { PanelParentContext } from "@/panels/panel-titles-context";
import { chooseMoreAction, inspectorActionList } from "@/test/inspector-actions";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { CatalogModelInspector } from "./catalog-model-inspector";
import { DeploymentModelInspector } from "./deployment-model-inspector";

const writeClipboard = vi.fn<typeof clipboard.writeClipboard>();
beforeEach(() => {
  vi.spyOn(clipboard, "writeClipboard").mockImplementation(writeClipboard);
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

const readyDeployment = create(ModelDeploymentSchema, {
  modelDeploymentId: "deployment-1",
  intent: { placement: { siteNodeId: "node", executionTargetId: "pool-1" } },
  snapshot: { source: { case: "local", value: snapshot } },
  state: {
    status: ModelDeploymentStatus.READY,
    startedAt: { seconds: 1n },
    observedPlacements: [{ managedNodeId: "node", acceleratorDeviceIds: ["GPU-1"] }],
  },
});

const workerTarget = create(ConfiguredExecutionTargetSchema, {
  displayName: "NVIDIA GB10",
  executionTargetId: "worker-gpu",
  siteNodeDisplayName: "gb10-host-2",
  siteNodeId: "worker",
});

const failedDeployment = create(ModelDeploymentSchema, {
  modelDeploymentId: "deployment-1",
  intent: { placement: { siteNodeId: "node", executionTargetId: "pool-1" } },
  snapshot: { source: { case: "local", value: snapshot } },
  state: {
    status: ModelDeploymentStatus.FAILED,
    failure: {
      code: "engine_startup_failed",
      message: "The engine rejected its startup arguments.",
      diagnostics: [
        "Exit status: 17",
        "EngineCore traceback: FileNotFoundError: [REDACTED]",
        "Correct the custom arguments and deploy again.",
      ],
    },
  },
});

// Carries startedAt so "no Started row while transitioning" asserts a suppressed value.
function transitioningDeployment(state: {
  progress?: {
    percent?: number;
    completed: bigint;
    total?: bigint;
    unit: ModelDeploymentProgressUnit;
  };
  status?: ModelDeploymentStatus;
  statusMessage: string;
}) {
  return create(ModelDeploymentSchema, {
    modelDeploymentId: "deployment-1",
    intent: { placement: { siteNodeId: "node", executionTargetId: "pool-1" } },
    snapshot: { source: { case: "local", value: snapshot } },
    state: {
      ...state,
      status: state.status ?? ModelDeploymentStatus.LOADING,
      startedAt: { seconds: 1n },
    },
  });
}

function detailRows(list: HTMLElement) {
  const labels = within(list).getAllByRole("term");
  const values = within(list).getAllByRole("definition");
  return labels.map((label, index) => [label.textContent, values[index]?.textContent]);
}

describe("model inspectors", () => {
  it("focuses the deployment inspector heading when it opens", async () => {
    render(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          snapshot: { source: { case: "local", value: snapshot } },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const panel = screen.getByRole("complementary", { name: "Test Model" });
    await waitFor(() =>
      expect(within(panel).getByRole("heading", { name: "Test Model" })).toHaveFocus(),
    );
  });

  it("shows stored typed values, exact custom tokens, and fixed settings in deployment details", () => {
    render(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          snapshot: {
            source: {
              case: "local",
              value: create(ResolvedLocalModelDeploymentSnapshotSchema, {
                ...snapshot,
                customArgumentTokens: ["--flag=value", "  spaced  ", ""],
              }),
            },
          },
          state: {
            status: ModelDeploymentStatus.FAILED,
            failure: {
              code: "engine_startup_failed",
              message: "The engine rejected its startup arguments.",
              diagnostics: ["Correct the custom arguments and deploy again."],
            },
          },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        screen="configuration"
      />,
    );

    expect(screen.getByText("Total context")).toBeVisible();
    expect(screen.getByText("8k")).toBeVisible();
    expect(screen.getByText("GPU layers")).toBeVisible();
    expect(screen.getByText("99")).toBeVisible();
    expect(screen.getByText("Flash Attention")).toBeVisible();
    expect(screen.getByText("Enabled")).toBeVisible();
    expect(
      within(screen.getByRole("list", { name: "Custom argument tokens" }))
        .getAllByRole("listitem")
        .map((item) => item.textContent),
    ).toEqual(['"--flag=value"', '"  spaced  "', '""']);
  });

  it("shows the KV offload size in resolved details only when present", () => {
    const withOffload = create(ResolvedLocalModelDeploymentSnapshotSchema, {
      ...snapshot,
      engineParameterSchema: kvOffloadReleases[0].parameterSchema,
      engineParameterValues: create(EngineParameterValuesSchema, {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: 8192n } },
          { parameterId: "kv-offloading-size-gib", value: { case: "unsignedInteger", value: 16n } },
        ],
      }),
    });

    const { rerender } = render(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          snapshot: { source: { case: "local", value: withOffload } },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        screen="configuration"
      />,
    );

    expect(screen.getByText("KV offloading size")).toBeVisible();
    expect(screen.getByText("16 GiB")).toBeVisible();
    expect(screen.getByText("8k")).toBeVisible();

    rerender(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          snapshot: { source: { case: "local", value: snapshot } },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        screen="configuration"
      />,
    );

    expect(screen.queryByText("KV offloading size")).not.toBeInTheDocument();
  });

  it("renders retained boolean engine parameters from the snapshot schema", () => {
    const withBoolean = create(ResolvedLocalModelDeploymentSnapshotSchema, {
      ...snapshot,
      engineParameterSchema: multiParameterReleases[0].parameterSchema,
      engineParameterValues: create(EngineParameterValuesSchema, {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: 8192n } },
          {
            parameterId: "parallel-requests",
            value: { case: "unsignedInteger", value: 4n },
          },
          {
            parameterId: "kv-cache-memory-bytes",
            value: { case: "unsignedInteger", value: 7400000000n },
          },
          { parameterId: "enable-prefix-caching", value: { case: "boolean", value: true } },
        ],
      }),
    });

    render(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          snapshot: { source: { case: "local", value: withBoolean } },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        screen="configuration"
      />,
    );

    const label = screen.getByText("Prefix caching");
    expect(label.nextElementSibling).toHaveTextContent("Enabled");
  });

  it("splits the Configuration screen into Resolved and Command sections", async () => {
    const user = setupUser();
    writeClipboard.mockResolvedValue(undefined);
    render(
      <DeploymentModelInspector
        deployment={readyDeployment}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        screen="configuration"
      />,
    );

    // The screen title already names the deployment, so the inner heading is just "Resolved".
    expect(
      screen
        .getAllByRole("region")
        .map((section) => within(section).getByRole("heading", { level: 2 }).textContent),
    ).toEqual(["Resolved", "Command"]);

    const tokens = within(screen.getByRole("list", { name: "Desired argument tokens" }))
      .getAllByRole("listitem")
      .map((item) => item.textContent);
    expect(tokens).toEqual(['"--ctx-size"', '"8192"']);

    await user.click(screen.getByRole("button", { name: "Copy command" }));
    expect(writeClipboard).toHaveBeenCalledExactlyOnceWith("--ctx-size\n8192");
  });

  it("shows runtime-resolved arguments as placeholders apart from literal tokens", async () => {
    const user = setupUser();
    writeClipboard.mockClear().mockResolvedValue(undefined);
    const symbolic = create(ResolvedLocalModelDeploymentSnapshotSchema, {
      ...snapshot,
      desiredLaunchPlan: create(PortableModelLaunchPlanSchema, {
        arguments: [
          { value: { case: "engineExecutable", value: {} } },
          { value: { case: "literal", value: "--model" } },
          { value: { case: "artifactBindingId", value: "model" } },
          { value: { case: "serviceHost", value: "http" } },
          { value: { case: "servicePort", value: "http" } },
          { value: { case: "literal", value: "{artifact:model}" } },
        ],
      }),
    });
    render(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          snapshot: { source: { case: "local", value: symbolic } },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        screen="configuration"
      />,
    );

    expect(
      within(screen.getByRole("list", { name: "Desired argument tokens" }))
        .getAllByRole("listitem")
        .map((item) => item.textContent),
    ).toEqual([
      "{engine.executable}",
      '"--model"',
      "{artifact:model}",
      "{service:http.host}",
      "{service:http.port}",
      '"{artifact:model}"',
    ]);
    expect(screen.queryByText(/filled in by the runtime at launch/)).toBeNull();

    await user.click(screen.getByRole("button", { name: "Copy command" }));
    expect(writeClipboard).toHaveBeenCalledExactlyOnceWith(
      "{engine.executable}\n--model\n{artifact:model}\n{service:http.host}\n{service:http.port}\n{artifact:model}",
    );
  });

  it("names a worker deployment's machine and its worker-reported devices", () => {
    const { rerender } = render(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          intent: { placement: { siteNodeId: "worker", executionTargetId: "worker-gpu" } },
          snapshot: { source: { case: "local", value: snapshot } },
          state: {
            status: ModelDeploymentStatus.READY,
            startedAt: { seconds: 1n },
            observedPlacements: [{ managedNodeId: "worker", acceleratorDeviceIds: ["GPU-remote"] }],
          },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        placementMachine={create(MachineSchema, {
          displayName: "gb10-host-2",
          machineId: "worker",
        })}
        executionTargets={[workerTarget]}
      />,
    );

    const deploymentSection = screen.getByRole("region", { name: "Deployment" });
    const rows = () => detailRows(deploymentSection.querySelector("dl")!);
    expect(rows()).toEqual(
      expect.arrayContaining([
        ["Placement", "gb10-host-2 / NVIDIA GB10"],
        ["Devices", "GPU-remote"],
      ]),
    );
    expect(deploymentSection).not.toHaveTextContent(/Requested placement|Observed placement/);

    // A target no longer configured is named by its ID.
    rerender(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          intent: { placement: { siteNodeId: "worker", executionTargetId: "worker-gpu" } },
          snapshot: { source: { case: "local", value: snapshot } },
          state: { status: ModelDeploymentStatus.LOADING },
        })}
        executionTargets={[]}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        placementMachine={create(MachineSchema, {
          displayName: "gb10-host-2",
          machineId: "worker",
        })}
      />,
    );
    expect(rows()).toEqual(expect.arrayContaining([["Placement", "gb10-host-2 / worker-gpu"]]));
    // Devices appear only once the worker reports them.
    expect(rows().map(([label]) => label)).not.toContain("Devices");
  });

  it("links the placement machine to its inspector", () => {
    const machineHref =
      "/machines?panel=models%2Fdeployment%3Adeployment-1&panel=machines%2Fmachine%3Aworker";
    const { rerender } = render(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          intent: { placement: { siteNodeId: "worker", executionTargetId: "worker-gpu" } },
          snapshot: { source: { case: "local", value: snapshot } },
          state: { status: ModelDeploymentStatus.READY },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        placementMachine={create(MachineSchema, {
          displayName: "gb10-host-2",
          machineId: "worker",
        })}
        placementMachineHref={machineHref}
        executionTargets={[workerTarget]}
      />,
    );

    const deploymentSection = screen.getByRole("region", { name: "Deployment" });
    const link = within(deploymentSection).getByRole("link", {
      name: "gb10-host-2 / NVIDIA GB10",
    });
    expect(link).toHaveAttribute("href", machineHref);
    expectResourceMarker(link, "worker");

    // An unlisted machine stays plain text under its SiteNode ID while the targets are unread.
    rerender(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          intent: { placement: { siteNodeId: "worker", executionTargetId: "worker-gpu" } },
          snapshot: { source: { case: "local", value: snapshot } },
          state: { status: ModelDeploymentStatus.READY },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );
    expect(within(deploymentSection).queryByRole("link", { name: "worker" })).toBeNull();
    expect(detailRows(deploymentSection.querySelector("dl")!)).toEqual(
      expect.arrayContaining([["Placement", "worker"]]),
    );
  });

  it("presents the root as Status, Configuration, Deployment with its memory, and Requests sections", async () => {
    const user = setupUser();
    const onOpenConfiguration = vi.fn();
    const onOpenLogs = vi.fn();
    const logs: DeploymentLogOperations = {
      follow: vi.fn(async function* () {}),
      read: vi.fn().mockResolvedValue({
        entries: [],
        historyState: "no-older-output-observed",
        latestPositions: [],
        responseTruncated: false,
        sources: [],
      }),
    };
    render(
      <DeploymentModelInspector
        catalogModelHref="/models/catalog?panel=model-catalog%2Fcatalog%3Amodel-1"
        containingRoutes={[{ href: "/routes", inferenceRouteId: "route-1", name: "audit" }]}
        deployment={readyDeployment}
        logs={logs}
        memory={<p>Memory rows</p>}
        onOpenChange={vi.fn()}
        onOpenConfiguration={onOpenConfiguration}
        onOpenLogs={onOpenLogs}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        requests={
          <section aria-labelledby="requests-heading">
            <h2 id="requests-heading">Requests</h2>
          </section>
        }
      />,
    );

    const panel = screen.getByRole("complementary", { name: "Test Model" });
    const sections = within(panel).getAllByRole("region");
    expect(
      sections.map((section) => within(section).getByRole("heading", { level: 2 }).textContent),
    ).toEqual(["Status", "Configuration", "Deployment", "Requests"]);
    expect(within(panel).getByRole("region", { name: "Deployment" })).toHaveTextContent(
      "Memory rows",
    );

    // Nothing renders outside a section: no floating badge, action, or value.
    expect([...sections[0].parentElement!.children].map((child) => child.tagName)).toEqual([
      "SECTION",
      "SECTION",
      "SECTION",
      "SECTION",
    ]);

    expect(within(panel).getByRole("link", { name: "Test Model" })).toHaveAttribute(
      "href",
      "/models/catalog?panel=model-catalog%2Fcatalog%3Amodel-1",
    );
    expect(within(panel).getByRole("link", { name: "audit" })).toBeVisible();
    expect(panel).toHaveTextContent("Ready");
    // Without a listed machine or read targets, the placement names the node ID.
    expect(
      detailRows(within(panel).getByRole("region", { name: "Deployment" }).querySelector("dl")!),
    ).toEqual(
      expect.arrayContaining([
        ["Placement", "node"],
        ["Devices", "GPU-1"],
      ]),
    );

    // The deployment's own identity now leads the Deployment section instead of hiding behind a row.
    const deploymentSection = within(panel).getByRole("region", { name: "Deployment" });
    expect(within(deploymentSection).getAllByRole("term")[0]).toHaveTextContent("Deployment ID");
    expect(deploymentSection).toHaveTextContent("deployment-1");

    expect(
      detailRows(within(panel).getByRole("region", { name: "Configuration" }).querySelector("dl")!),
    ).toEqual(expect.arrayContaining([["Total context", "8k"]]));

    for (const removed of [
      "Desired arguments",
      "Resolved deployment",
      "Stage",
      "Progress",
      "--ctx-size",
    ]) {
      expect(panel).not.toHaveTextContent(removed);
    }

    await user.click(within(panel).getByRole("button", { name: "View all" }));
    expect(onOpenConfiguration).toHaveBeenCalledTimes(1);
    await user.click(within(panel).getByRole("button", { name: "Logs" }));
    expect(onOpenLogs).toHaveBeenCalledTimes(1);
  });

  it.each([
    { derived: ["context-per-request"], perRequest: 131072n, shown: "128k derived" },
    { derived: [], perRequest: 32768n, shown: "32k" },
  ])(
    "shows the total context and the context per request as $shown",
    ({ derived, perRequest, shown }) => {
      const resolved = create(ResolvedLocalModelDeploymentSnapshotSchema, {
        ...snapshot,
        engineParameterValues: create(EngineParameterValuesSchema, {
          values: [
            { parameterId: "total-context", value: { case: "unsignedInteger", value: 131072n } },
            {
              parameterId: "context-per-request",
              value: { case: "unsignedInteger", value: perRequest },
            },
            { parameterId: "parallel-requests", value: { case: "unsignedInteger", value: 4n } },
          ],
        }),
        sizing: create(ResolvedSizingSchema, { derivedEngineParameterIds: derived }),
      });
      render(
        <DeploymentModelInspector
          deployment={create(ModelDeploymentSchema, {
            modelDeploymentId: "deployment-1",
            snapshot: { source: { case: "local", value: resolved } },
          })}
          onOpenChange={vi.fn()}
          onUnload={vi.fn()}
          open
          openerRef={createRef<HTMLElement>()}
          pending={false}
        />,
      );

      const configuration = screen.getByRole("region", { name: "Configuration" });
      expect(detailRows(configuration.querySelector("dl")!)).toEqual([
        ["Total context", "128k"],
        ["Context per request", shown],
      ]);
    },
  );

  it("reports an absent execution target as unavailable and an empty route list in words", () => {
    render(
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          snapshot: { source: { case: "local", value: snapshot } },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const panel = screen.getByRole("complementary", { name: "Test Model" });
    expect(panel).toHaveTextContent("Unavailable");
    expect(panel).toHaveTextContent("No route configured.");
  });

  it("shows determinate byte progress while downloading", () => {
    render(
      <DeploymentModelInspector
        deployment={transitioningDeployment({
          status: ModelDeploymentStatus.DOWNLOADING,
          statusMessage: "Downloading model artifacts",
          progress: {
            percent: 40,
            completed: 40n * 1024n * 1024n,
            total: 100n * 1024n * 1024n,
            unit: ModelDeploymentProgressUnit.BYTES,
          },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    expect(status).toHaveTextContent("Downloading model artifacts");
    expect(status).toHaveTextContent("40 MiB / 100 MiB · 40%");
    expect(within(status).getByRole("progressbar", { name: "Download progress" })).toHaveAttribute(
      "aria-valuenow",
      "40",
    );
    expect(status).not.toHaveTextContent("Started");
  });

  it("shows indeterminate completed bytes when the download total is unknown", () => {
    render(
      <DeploymentModelInspector
        deployment={transitioningDeployment({
          status: ModelDeploymentStatus.DOWNLOADING,
          statusMessage: "Downloading model artifacts",
          progress: {
            completed: 1536n,
            unit: ModelDeploymentProgressUnit.BYTES,
          },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    const progress = within(status).getByRole("progressbar", { name: "Download progress" });
    expect(progress).toHaveAttribute("data-indeterminate");
    expect(progress).not.toHaveAttribute("aria-valuenow");
    expect(status).toHaveTextContent("1.5 KiB downloaded");
    expect(status).not.toHaveTextContent("/");
    expect(status).not.toHaveTextContent("%");
  });

  it("shows status messages without inventing progress and restores Started when ready", () => {
    const { rerender } = render(
      <DeploymentModelInspector
        deployment={transitioningDeployment({ statusMessage: "Acquiring engine image" })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const transitioningStatus = screen.getByRole("region", { name: "Status" });
    expect(transitioningStatus).toHaveTextContent("Acquiring engine image");
    expect(within(transitioningStatus).queryByRole("progressbar")).not.toBeInTheDocument();
    expect(transitioningStatus).not.toHaveTextContent("%");

    for (const stage of [
      "Verifying model artifacts",
      "Publishing model artifacts",
      "Reusing verified model artifacts",
    ]) {
      rerender(
        <DeploymentModelInspector
          deployment={transitioningDeployment({ statusMessage: stage })}
          onOpenChange={vi.fn()}
          onUnload={vi.fn()}
          open
          openerRef={createRef<HTMLElement>()}
          pending={false}
        />,
      );
      expect(screen.getByRole("region", { name: "Status" })).toHaveTextContent(stage);
      expect(
        within(screen.getByRole("region", { name: "Status" })).queryByRole("progressbar"),
      ).not.toBeInTheDocument();
    }

    rerender(
      <DeploymentModelInspector
        deployment={readyDeployment}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );
    const readyStatus = screen.getByRole("region", { name: "Status" });
    expect(readyStatus).toHaveTextContent("Ready");
    expect(readyStatus).toHaveTextContent("Started");
    expect(readyStatus).not.toHaveTextContent("Acquiring engine image");
  });

  it("shows the deployment target only when the state does not already imply it", () => {
    const renderInspector = (deployment: ModelDeployment) =>
      render(
        <DeploymentModelInspector
          deployment={deployment}
          onOpenChange={vi.fn()}
          onUnload={vi.fn()}
          open
          openerRef={createRef<HTMLElement>()}
          pending={false}
        />,
      );
    const statusLabels = () =>
      [...screen.getByRole("region", { name: "Status" }).querySelectorAll("dt")]
        .map((label) => label.textContent)
        .slice(0, 2);

    // Settled
    const ready = renderInspector(
      create(ModelDeploymentSchema, {
        ...readyDeployment,
        desiredState: ModelDeploymentDesiredState.PRESENT,
      }),
    );
    expect(statusLabels()[0]).toBe("State");
    expect(statusLabels()).not.toContain("Desired");
    expect(within(screen.getByRole("region", { name: "Status" })).getByText("Ready")).toBeVisible();
    ready.unmount();

    // The status word names the teardown target itself
    const tearingDown = renderInspector(
      create(ModelDeploymentSchema, {
        ...readyDeployment,
        desiredState: ModelDeploymentDesiredState.TEARDOWN,
      }),
    );
    expect(statusLabels()).not.toContain("Desired");
    expect(
      within(screen.getByRole("region", { name: "Status" })).getByText("Tearing down"),
    ).toBeVisible();
    tearingDown.unmount();

    // Meant to be present but still paused: the divergence is only visible through the target
    renderInspector(
      create(ModelDeploymentSchema, {
        ...readyDeployment,
        desiredState: ModelDeploymentDesiredState.PRESENT,
        state: create(ModelDeploymentStateSchema, { status: ModelDeploymentStatus.PAUSED }),
      }),
    );
    expect(statusLabels()).toEqual(["State", "Desired"]);
    const status = screen.getByRole("region", { name: "Status" });
    expect(within(status).getByText("Paused")).toBeVisible();
    expect(within(status).getByText("Present")).toBeVisible();
  });

  it("keeps Pause in the menu while present and makes Resume primary while paused", async () => {
    const user = setupUser();
    const inspector = (
      desiredState: ModelDeploymentDesiredState,
      status: ModelDeploymentStatus,
    ) => (
      <DeploymentModelInspector
        deployment={create(ModelDeploymentSchema, {
          desiredState,
          intent: readyDeployment.intent,
          modelDeploymentId: readyDeployment.modelDeploymentId,
          snapshot: readyDeployment.snapshot,
          state: { status },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />
    );
    const { rerender } = render(
      inspector(ModelDeploymentDesiredState.PRESENT, ModelDeploymentStatus.READY),
    );

    expect(await inspectorActionList(user)).toEqual({
      buttons: ["More actions"],
      menu: ["Edit", "Pause", "Teardown…"],
    });

    rerender(inspector(ModelDeploymentDesiredState.PAUSED, ModelDeploymentStatus.PAUSED));

    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Resume", "More actions"],
      menu: ["Edit", "Teardown…"],
    });
    expect(screen.getByRole("button", { name: "Resume" })).toHaveClass("bg-primary");
  });

  it("makes Chat primary for a present deployment and opens the chat on its route", async () => {
    const user = setupUser();
    const openChat = vi.fn();
    render(
      <ChatOpenerContext.Provider value={openChat}>
        <DeploymentModelInspector
          chatTarget={{ label: "Chat", mode: "chat", route: "gemma" }}
          deployment={create(ModelDeploymentSchema, {
            ...readyDeployment,
            desiredState: ModelDeploymentDesiredState.PRESENT,
          })}
          onOpenChange={vi.fn()}
          onUnload={vi.fn()}
          open
          openerRef={createRef<HTMLElement>()}
          pending={false}
        />
      </ChatOpenerContext.Provider>,
    );

    expect(await inspectorActionList(user)).toEqual({
      buttons: ["Chat", "More actions"],
      menu: ["Edit", "Pause", "Teardown…"],
    });
    await user.click(screen.getByRole("button", { name: "Chat" }));
    expect(openChat).toHaveBeenCalledWith("gemma", "chat");
  });

  it("keeps local teardown feedback in one retryable dialog", async () => {
    const user = setupUser();
    const dependencyError =
      'Cannot teardown the model deployment because these application deployments use inference routes that would be removed with it: "App A" (application-a) via "Route A" (route-a); "App B" (application-b) via "Route B" (route-b); "App C" (application-c) via "Route C" (route-c); and 1 more. Teardown the applications and wait for them to be removed, then try again.';
    const onUnload = vi.fn().mockResolvedValueOnce(false).mockResolvedValueOnce(true);
    const deployment = create(ModelDeploymentSchema, {
      ...readyDeployment,
      desiredState: ModelDeploymentDesiredState.PRESENT,
    });
    const { rerender } = render(
      <DeploymentModelInspector
        deployment={deployment}
        error="Pause failed."
        onOpenChange={vi.fn()}
        onUnload={onUnload}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    await chooseMoreAction(user, "Teardown…");
    const confirmation = screen.getByRole("dialog", { name: "Teardown Test Model?" });
    expect(within(confirmation).queryByText("Pause failed.")).not.toBeInTheDocument();
    expect(within(confirmation).getByText(/deployment-1/)).toHaveTextContent(
      "This removes model deployment deployment-1 and its running workload resources.",
    );
    await user.click(within(confirmation).getByRole("button", { name: "Teardown" }));
    expect(onUnload).toHaveBeenCalledOnce();

    rerender(
      <DeploymentModelInspector
        deployment={deployment}
        onOpenChange={vi.fn()}
        onUnload={onUnload}
        open
        openerRef={createRef<HTMLElement>()}
        pending
        pendingAction="unload"
      />,
    );
    const pendingConfirmation = screen.getByRole("dialog", { name: "Teardown Test Model?" });
    expect(
      within(pendingConfirmation).getByRole("button", { name: "Tearing down…" }),
    ).toBeDisabled();
    expect(within(pendingConfirmation).getByRole("button", { name: "Cancel" })).toBeDisabled();
    await user.keyboard("{Escape}");
    expect(screen.getByRole("dialog", { name: "Teardown Test Model?" })).toBeVisible();

    rerender(
      <DeploymentModelInspector
        deployment={deployment}
        error={dependencyError}
        teardownError={dependencyError}
        onOpenChange={vi.fn()}
        onUnload={onUnload}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );
    const failedConfirmation = screen.getByRole("dialog", { name: "Teardown Test Model?" });
    expect(within(failedConfirmation).getByRole("alert")).toHaveTextContent(dependencyError);
    expect(screen.getAllByText(dependencyError)).toHaveLength(1);
    expect(within(failedConfirmation).getByRole("button", { name: "Try again" })).toBeEnabled();

    await user.click(within(failedConfirmation).getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("dialog", { name: "Teardown Test Model?" })).not.toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent(dependencyError);
    await waitFor(() => expect(screen.getByRole("button", { name: "More actions" })).toHaveFocus());

    await chooseMoreAction(user, "Teardown…");
    await user.click(
      within(screen.getByRole("dialog", { name: "Teardown Test Model?" })).getByRole("button", {
        name: "Try again",
      }),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "Teardown Test Model?" }),
      ).not.toBeInTheDocument(),
    );
    expect(onUnload).toHaveBeenCalledTimes(2);
  });

  it("states a failure as a Status row and one notice closing that section", () => {
    render(
      <DeploymentModelInspector
        deployment={failedDeployment}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const status = screen.getByRole("region", { name: "Status" });
    expect(status).toHaveTextContent("Failure code");
    expect(status).toHaveTextContent("engine_startup_failed");

    // The message and its diagnostics share one notice rather than a second table.
    const notices = within(status).getAllByRole("alert");
    expect(notices).toHaveLength(1);
    expect(notices[0]).toHaveTextContent("The engine rejected its startup arguments.");
    expect(notices[0]).toHaveTextContent("Correct the custom arguments and deploy again.");
    expect(notices[0]).toHaveTextContent("Exit status: 17");
    expect(notices[0]).toHaveTextContent("EngineCore traceback: FileNotFoundError: [REDACTED]");
  });

  it("returns from a sub-screen to the deployment beneath it", () => {
    render(
      <PanelParentContext.Provider
        value={{ page: "models", screen: "deployment", resourceId: "deployment-1" }}
      >
        <DeploymentModelInspector
          deployment={readyDeployment}
          onBack={vi.fn()}
          onOpenChange={vi.fn()}
          onUnload={vi.fn()}
          open
          openerRef={createRef<HTMLElement>()}
          pending={false}
          screen="configuration"
        />
      </PanelParentContext.Provider>,
    );

    expect(screen.getByRole("button", { name: "Back to model deployment" })).toBeVisible();
    expect(screen.getByRole("heading", { name: /Configuration/ })).toBeVisible();
  });

  it("marks the catalog model title with its color and a local deployment title with its own", () => {
    const { rerender } = render(
      <CatalogModelInspector
        memoryPending={false}
        model={model}
        onDeploy={vi.fn()}
        onOpenChange={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
        row={
          projectModelRows({
            deployments: [],
            engineReleases: [],
            hardwareClasses: [],
            estimates: new Map(),
            models: [model],
          })[0]
        }
      />,
    );
    expectResourceMarker(
      inspectorTitle(screen.getByRole("complementary", { name: "Test Model" })),
      "model-1",
    );

    rerender(
      <DeploymentModelInspector
        containingRoutes={[{ href: "/routes", inferenceRouteId: "route-a", name: "chat" }]}
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          modelDeploymentName: "Team model",
          snapshot: { source: { case: "local", value: snapshot } },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );
    expectResourceMarker(
      inspectorTitle(screen.getByRole("complementary", { name: "Team model" })),
      "deployment-1",
    );
    expectResourceMarker(screen.getByRole("link", { name: "chat" }), "route-a");
  });

  it.each([
    [0n, "Off"],
    [1n, "MTP · 1 token"],
    [3n, "MTP · 3 tokens"],
  ])("shows the resolved speculative state for a token count of %s", (count, expected) => {
    const speculativeModel = create(ModelSchema, {
      modelId: "model-1",
      options: [
        {
          optionId: "mtp",
          displayName: "MTP",
          kind: {
            case: "speculativeDecoding",
            value: { method: SpeculativeDecodingMethod.MTP },
          },
        },
      ],
    });
    const resolved = create(ResolvedLocalModelDeploymentSnapshotSchema, {
      modelId: "model-1",
      modelDisplayName: "Test Model",
      resolvedOptions: [
        {
          values: {
            optionId: "mtp",
            values: [
              { parameterId: "token-count", value: { case: "unsignedInteger", value: count } },
            ],
          },
        },
      ],
    });

    render(
      <DeploymentModelInspector
        catalogModel={speculativeModel}
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          snapshot: { source: { case: "local", value: resolved } },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    const term = screen.getByText("Speculative decoding");
    expect(term.nextElementSibling).toHaveTextContent(expected);
  });

  it("shows the resolved KV cache precision of a deployment", () => {
    const resolved = create(ResolvedLocalModelDeploymentSnapshotSchema, {
      modelId: "model-1",
      modelDisplayName: "Test Model",
      resolvedOptions: [
        {
          values: {
            optionId: "kv-cache",
            values: [{ parameterId: "precision", value: { case: "string", value: "q8_0" } }],
          },
        },
      ],
    });

    render(
      <DeploymentModelInspector
        catalogModel={modelWithKvCachePrecision()}
        deployment={create(ModelDeploymentSchema, {
          modelDeploymentId: "deployment-1",
          snapshot: { source: { case: "local", value: resolved } },
        })}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(screen.getByText("KV cache precision").nextElementSibling).toHaveTextContent("q8_0");
  });

  it("omits the speculative row when the deployment resolved no speculative option", () => {
    render(
      <DeploymentModelInspector
        catalogModel={model}
        deployment={readyDeployment}
        onOpenChange={vi.fn()}
        onUnload={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    expect(screen.queryByText("Speculative decoding")).toBeNull();
  });
});
