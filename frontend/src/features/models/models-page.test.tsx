import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import { workstationSnapshot } from "@/features/memory/machine-test-support";
import {
  gib,
  hostPool,
  nodeMemory,
  other,
  unsized,
} from "@/features/memory/node-memory-test-support";
import {
  gaugePoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { ListModelsResponseSchema } from "@/gen/dotcore/catalog/v1/service_pb";
import { MemoryView } from "@/gen/dotcore/console/v1/preferences_pb";
import {
  InferenceRouteAvailabilitySchema,
  InferenceRouteSchema,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import {
  ModelDeploymentDesiredState,
  ModelDeploymentIntentSchema,
  ModelDeploymentSchema,
  ModelDeploymentSnapshotSchema,
  UpstreamModelDeploymentSnapshotSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { RemoveModelDeploymentResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import {
  ModelDeploymentProgressUnit,
  ModelDeploymentStateSchema,
  ModelDeploymentStatus,
} from "@/gen/dotcore/model/v1/state_pb";
import { MachineSchema } from "@/gen/dotcore/site/v1/service_pb";
import { LOADING_MINIMUM_VISIBLE_MS, LOADING_REVEAL_DELAY_MS } from "@/hooks/use-delayed-loading";
import { readMemoryView } from "@/lib/console-preferences";
import { expectHeadingOutline } from "@/test/heading-outline";
import { chooseMoreAction } from "@/test/inspector-actions";
import { expectResourceMarker } from "@/test/resource-marker";

import { projectModelSnapshot } from "./model-projection";
import {
  catalogModel,
  localDeployment,
  modelTarget,
  readableMachine,
  renderModelsSection,
  unifiedSnapshot,
  upstreamTemplate,
} from "./models-section-test-support";

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
describe("ModelsPage data contract", () => {
  it("keeps the catalog model independent from local deployment state", () => {
    const model = create(ModelSchema, {
      modelId: "model-1",
      displayName: "Model",
    });
    const snapshot = projectModelSnapshot([model], []);
    expect(snapshot.availableModels).toEqual([model]);
    expect(snapshot.runningModels).toEqual([]);
  });
});

describe("ModelsPage cards", () => {
  it("reads h1 page, h2 section, h3 card with no skipped level", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    renderModelsSection({
      models: [gemma],
      deployments: [
        localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
      ],
    });
    await waitFor(() => {
      // Memory holds its place while the machines are read, so it heads the page from the start.
      expect(screen.getAllByRole("heading", { level: 2 }).map((h) => h.textContent)).toEqual([
        "Memory",
        "Running",
      ]);
      expectHeadingOutline(document.body, [1, 2, 2, 3]);
    });
    expect(screen.getByRole("region", { name: "Memory" })).toHaveTextContent("MemoryVRAM—RAM—");
  });

  it("shows no Catalog section; the Catalog has its own page", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    renderModelsSection({ models: [gemma], templates: [upstreamTemplate()] });

    expect(await screen.findByText("No models running")).toBeVisible();
    expect(screen.queryByRole("heading", { name: "Catalog" })).not.toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Catalog models" })).not.toBeInTheDocument();
    expect(screen.queryByRole("article", { name: "Gemma" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Upstream model/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Connect upstream" })).not.toBeInTheDocument();
  });

  it("delays initial feedback and holds completed Models content for the minimum", async () => {
    vi.useFakeTimers();
    let resolveModels!: (
      response: Awaited<ReturnType<ModelClient["catalog"]["listModels"]>>,
    ) => void;
    const modelsResponse = new Promise<Awaited<ReturnType<ModelClient["catalog"]["listModels"]>>>(
      (resolve) => {
        resolveModels = resolve;
      },
    );
    const { client } = renderModelsSection({ modelsResponse });

    expect(screen.queryByText("Loading models…")).not.toBeInTheDocument();
    expect(client.catalog.listModels).toHaveBeenCalledTimes(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_REVEAL_DELAY_MS);
    });
    expect(client.model.listModelDeployments).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("status")).toHaveTextContent("Loading models…");

    await act(async () => {
      resolveModels(create(ListModelsResponseSchema));
      await Promise.resolve();
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading models…");
    expect(screen.queryByText("No models running")).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_MINIMUM_VISIBLE_MS);
    });
    expect(screen.queryByText("Loading models…")).not.toBeInTheDocument();
    expect(screen.getByText("No models running")).toBeVisible();
  });
  it("describes only its own deployment on each running card", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    renderModelsSection({
      models: [gemma],
      deployments: [
        localDeployment("deployment-a", gemma, {
          status: ModelDeploymentStatus.LOADING,
          progress: {
            percent: 40,
            completed: 40n,
            total: 100n,
            unit: ModelDeploymentProgressUnit.BYTES,
          },
        }),
        localDeployment("deployment-b", gemma, {
          status: ModelDeploymentStatus.FAILED,
          failure: { message: "Engine exited" },
        }),
      ],
    });

    const running = await screen.findByRole("list", { name: "Running models" });
    const first = within(running).getByRole("article", { name: "Gemma (deployment-a)" });
    const second = within(running).getByRole("article", { name: "Gemma (deployment-b)" });
    expect(first).not.toHaveTextContent(/\d+ deployments?/);
    expect(second).not.toHaveTextContent(/\d+ deployments?/);
    // The failure message reaches the reader through the badge's tooltip, not as card text; load
    // progress stays in the inspector, so both cards keep the same single-badge shape.
    expect(within(first).queryByText("40%")).not.toBeInTheDocument();
    expect(first.querySelectorAll('[data-slot="badge"]')).toHaveLength(1);
    expect(second.querySelector('[data-slot="badge"]')).toHaveAttribute("title", "Engine exited");
    expect(within(second).queryByText("Engine exited")).not.toBeInTheDocument();
    expect(second.querySelectorAll('[data-slot="badge"]')).toHaveLength(1);
  });

  it("names a transitioning state the same way on the card and in its inspector", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    renderModelsSection({
      models: [gemma],
      deployments: [
        localDeployment("deployment-a", gemma, {
          status: ModelDeploymentStatus.DOWNLOADING,
          statusMessage: "Downloading model artifacts",
          progress: {
            percent: 40,
            completed: 40n,
            total: 100n,
            unit: ModelDeploymentProgressUnit.BYTES,
          },
        }),
      ],
    });

    const running = await screen.findByRole("list", { name: "Running models" });
    const card = within(running).getByRole("article", { name: "Gemma" });
    expect(card).toHaveTextContent("Downloading");
    expect(card).not.toHaveTextContent("Downloading model artifacts");

    await user.click(within(card).getByRole("button", { name: /View details/ }));
    const panel = await screen.findByRole("complementary", { name: "Gemma" });
    expect(panel).toHaveTextContent("Downloading");

    const status = within(panel).getByRole("region", { name: "Status" });
    expect(status).toHaveTextContent("Downloading model artifacts");
    expect(status).toHaveTextContent("40 bytes / 100 bytes · 40%");
    expect(within(status).getByRole("progressbar", { name: "Download progress" })).toBeVisible();
  });

  it("keeps generic teardown failures in the retryable dialog", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const deployment = localDeployment("deployment-a", gemma, {
      status: ModelDeploymentStatus.READY,
    });
    const { client } = renderModelsSection({ models: [gemma], deployments: [deployment] });
    vi.mocked(client.model.removeModelDeployment).mockRejectedValueOnce(
      new ConnectError("transport interrupted", Code.Unavailable),
    );

    const running = await screen.findByRole("list", { name: "Running models" });
    const card = within(running).getByRole("article", { name: "Gemma" });
    await user.click(within(card).getByRole("button", { name: /View details/ }));
    const inspector = await screen.findByRole("complementary", { name: "Gemma" });
    await chooseMoreAction(user, "Teardown…", inspector);
    const confirmation = screen.getByRole("dialog", { name: "Teardown Gemma?" });

    await user.click(within(confirmation).getByRole("button", { name: "Teardown" }));

    expect(await within(confirmation).findByRole("alert")).toHaveTextContent(
      "Teardown outcome is uncertain (Unavailable): transport interrupted. Check the Running section and service logs before retrying.",
    );
    expect(within(confirmation).getByRole("button", { name: "Try again" })).toBeEnabled();
  });

  it("passes blocked teardown feedback to the upstream dialog", async () => {
    const user = userEvent.setup();
    const template = upstreamTemplate();
    const deployment = create(ModelDeploymentSchema, {
      modelDeploymentId: "upstream-deployment",
      modelDeploymentName: "Private model",
      desiredState: ModelDeploymentDesiredState.PRESENT,
      snapshot: create(ModelDeploymentSnapshotSchema, {
        source: {
          case: "upstream",
          value: create(UpstreamModelDeploymentSnapshotSchema, {
            upstreamModelTemplate: template,
          }),
        },
      }),
      state: create(ModelDeploymentStateSchema, { status: ModelDeploymentStatus.READY }),
    });
    const dependencyError =
      "cannot remove model deployment while inference routes shared with other deployments reference it: chat, coding";
    const { client } = renderModelsSection({ deployments: [deployment], templates: [template] });
    vi.mocked(client.model.removeModelDeployment).mockRejectedValueOnce(
      new ConnectError(dependencyError, Code.FailedPrecondition),
    );

    const running = await screen.findByRole("list", { name: "Running models" });
    const card = within(running).getByRole("article", {
      name: "Private model (upstream-deployment)",
    });
    await user.click(within(card).getByRole("button", { name: /View details/ }));
    const inspector = await screen.findByRole("complementary", { name: "Private model" });
    await chooseMoreAction(user, "Teardown…", inspector);
    const confirmation = screen.getByRole("dialog", { name: "Teardown Private model?" });

    await user.click(within(confirmation).getByRole("button", { name: "Teardown" }));

    expect(await within(confirmation).findByRole("alert")).toHaveTextContent(dependencyError);
    expect(screen.getAllByText(dependencyError)).toHaveLength(1);
    expect(within(confirmation).getByRole("button", { name: "Try again" })).toBeEnabled();
  });

  it("keeps a blocked teardown actionable until retry is accepted", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const deployment = localDeployment("deployment-a", gemma, {
      status: ModelDeploymentStatus.READY,
    });
    const dependencyError =
      'Cannot teardown the model deployment because application deployment "Open WebUI" (application-one) uses inference route "Gemma route" (route-one), which would be removed with the model deployment. Teardown the application and wait for it to be removed, then try again.';
    let resolveRetry!: (
      response: Awaited<ReturnType<ModelClient["model"]["removeModelDeployment"]>>,
    ) => void;
    const retryResponse = new Promise<
      Awaited<ReturnType<ModelClient["model"]["removeModelDeployment"]>>
    >((resolve) => {
      resolveRetry = resolve;
    });
    const { client } = renderModelsSection({ models: [gemma], deployments: [deployment] });
    vi.mocked(client.model.pauseModel).mockRejectedValueOnce(
      new ConnectError("Pause is blocked", Code.FailedPrecondition),
    );
    vi.mocked(client.model.removeModelDeployment)
      .mockRejectedValueOnce(new ConnectError(dependencyError, Code.FailedPrecondition))
      .mockImplementationOnce(() => retryResponse);

    const running = await screen.findByRole("list", { name: "Running models" });
    const card = within(running).getByRole("article", { name: "Gemma" });
    await user.click(within(card).getByRole("button", { name: /View details/ }));
    const inspector = await screen.findByRole("complementary", { name: "Gemma" });

    await chooseMoreAction(user, "Pause", inspector);
    expect(await within(inspector).findByRole("alert")).toHaveTextContent("Pause was rejected");

    await chooseMoreAction(user, "Teardown…", inspector);
    const confirmation = screen.getByRole("dialog", { name: "Teardown Gemma?" });
    expect(within(confirmation).queryByText(/Pause was rejected/)).not.toBeInTheDocument();
    await user.click(within(confirmation).getByRole("button", { name: "Teardown" }));

    const dependencyNotice = await within(confirmation).findByRole("alert");
    expect(dependencyNotice).toHaveTextContent(dependencyError);
    expect(dependencyNotice).not.toHaveTextContent("Check the request and service logs");
    expect(screen.getAllByText(dependencyError)).toHaveLength(1);
    expect(within(confirmation).getByRole("button", { name: "Try again" })).toBeEnabled();

    await user.click(within(confirmation).getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(within(confirmation).queryByRole("alert")).not.toBeInTheDocument());
    expect(within(confirmation).getByRole("button", { name: "Tearing down…" })).toBeDisabled();
    expect(within(confirmation).getByRole("button", { name: "Cancel" })).toBeDisabled();

    const accepted = create(ModelDeploymentSchema, {
      ...deployment,
      desiredState: ModelDeploymentDesiredState.TEARDOWN,
      state: create(ModelDeploymentStateSchema, {
        status: ModelDeploymentStatus.UNLOADING,
      }),
    });
    await act(async () => {
      resolveRetry(create(RemoveModelDeploymentResponseSchema, { deployment: accepted }));
      await retryResponse;
    });

    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: "Teardown Gemma?" })).not.toBeInTheDocument(),
    );
    expect(client.model.removeModelDeployment).toHaveBeenCalledTimes(2);
  });
});

describe("ModelsPage deployment inspector", () => {
  it("enables Chat once the launching deployment comes online and its route becomes available", async () => {
    vi.useFakeTimers();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const deployments = [
      localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.LOADING }),
    ];
    let available = false;
    const listRoutes = vi.fn(() =>
      Promise.resolve(
        create(ListRoutesResponseSchema, {
          routes: [
            create(InferenceRouteViewSchema, {
              availability: create(InferenceRouteAvailabilitySchema, { available }),
              route: create(InferenceRouteSchema, {
                enabled: true,
                fallbackGroups: [{ modelDeploymentIds: ["deployment-a"] }],
                inferenceRouteId: "id-chat",
                name: "chat",
              }),
            }),
          ],
        }),
      ),
    );
    const { watch } = renderModelsSection({
      chatOpener: vi.fn(),
      deployments,
      initialEntry: "/models?panel=models%2Fdeployment%3Adeployment-a",
      listRoutes,
      models: [gemma],
    });

    await act(() => vi.advanceTimersByTimeAsync(LOADING_REVEAL_DELAY_MS));
    expect(screen.getByRole("button", { name: "Chat" })).toHaveAttribute("aria-disabled", "true");

    // The routes arrive with the same tick that brings the deployment online.
    available = true;
    deployments[0] = localDeployment("deployment-a", gemma, {
      status: ModelDeploymentStatus.READY,
    });
    await act(() => watch.tick());
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(screen.getByRole("button", { name: "Chat" })).not.toHaveAttribute("aria-disabled");
  });
});

describe("ModelsPage memory", () => {
  afterEach(() => window.localStorage.clear());

  it("draws VRAM empty for an all-unified site, with each model in its card color", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    renderModelsSection({
      deployments: [
        localDeployment("deployment-a", gemma, {
          observedPlacements: [{ managedNodeId: "spark-01" }],
          status: ModelDeploymentStatus.READY,
        }),
      ],
      machines: [readableMachine("spark-01", "Spark one")],
      models: [gemma],
      nodeMemory: [
        nodeMemory("spark-01", [
          hostPool(128n * gib, [other(68n * gib)], {}, [
            unsized({ modelDeploymentId: "deployment-a" }),
          ]),
        ]),
      ],
      snapshots: { "spark-01": unifiedSnapshot() },
    });

    const memory = await screen.findByRole("region", { name: "Memory" });
    const bars = await within(memory).findAllByRole("img");
    expect(bars.map((bar) => bar.getAttribute("aria-label"))).toEqual([
      "RAM on Spark one: 68 GiB used of 128 GiB; not measured: Gemma (measurement unavailable)",
    ]);
    expect(within(memory).getByRole("button", { name: "VRAM" })).toHaveTextContent("VRAM");
    expect(memory).toHaveTextContent(/VRAM—RAM/);
    // A single machine has nothing to split by, so the view choice stays hidden.
    expect(within(memory).queryByRole("group", { name: "Memory view" })).not.toBeInTheDocument();
    const deploy = screen.getByRole("link", { name: "Deploy" });
    expect(deploy).toHaveAttribute("href", "/models/catalog");
    expect(deploy.querySelector("svg")).toHaveClass("lucide-rocket");
  });

  it("names the inspected deployment's placement by its machine and target display names", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const deployment = localDeployment("deployment-a", gemma, {
      observedPlacements: [{ acceleratorDeviceIds: ["GB10"], managedNodeId: "spark-01" }],
      status: ModelDeploymentStatus.READY,
    });
    deployment.intent = create(ModelDeploymentIntentSchema, {
      placement: { executionTargetId: "spark-gpu", siteNodeId: "spark-01" },
    });
    renderModelsSection({
      deployments: [deployment],
      executionTargets: [modelTarget("spark-01", "spark-gpu", "NVIDIA GB10", "Spark one")],
      machines: [readableMachine("spark-01", "Spark one")],
      models: [gemma],
      snapshots: { "spark-01": unifiedSnapshot() },
    });

    await user.click(await screen.findByRole("button", { name: "View details for Gemma" }));
    const section = await screen.findByRole("region", { name: "Deployment" });
    await waitFor(() =>
      expect(within(section).getByText("Placement").nextElementSibling).toHaveTextContent(
        /^Spark one \/ NVIDIA GB10$/,
      ),
    );
    expect(within(section).getByText("Devices").nextElementSibling).toHaveTextContent(/^GB10$/);
    expect(within(section).getByRole("link", { name: "Spark one / NVIDIA GB10" })).toHaveAttribute(
      "href",
      "/machines?panel=models%2Fdeployment%3Adeployment-a&panel=machines%2Fmachine%3Aspark-01",
    );
  });

  it("labels a deployment's Back with the machine it was opened from", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    renderModelsSection({
      deployments: [
        localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
      ],
      initialEntry:
        "/models?panel=machines%2Fmachine%3Aspark-01&panel=models%2Fdeployment%3Adeployment-a",
      machines: [readableMachine("spark-01", "Spark one")],
      models: [gemma],
      snapshots: { "spark-01": unifiedSnapshot() },
    });

    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Back to Spark one" })).toBeVisible(),
    );
  });

  it("labels a deployment's Back with the catalog model it was opened from", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    renderModelsSection({
      deployments: [
        localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
      ],
      initialEntry:
        "/models?panel=model-catalog%2Fcatalog%3Agemma&panel=models%2Fdeployment%3Adeployment-a",
      models: [gemma],
    });

    // The URL's placeholder panel gives way to the deployment inspector once it is read.
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Back to model" })).toBeVisible(),
    );
  });

  it("keeps each machine's span when switched to By machine, and remembers the choice", async () => {
    const user = userEvent.setup();
    renderModelsSection({
      machines: [
        readableMachine("spark-01", "Spark one"),
        readableMachine("spark-02", "Spark two"),
      ],
      snapshots: { "spark-01": unifiedSnapshot(), "spark-02": unifiedSnapshot() },
    });

    const memory = await screen.findByRole("region", { name: "Memory" });
    await waitFor(() =>
      expect(within(memory).getByRole("img")).toHaveAttribute(
        "aria-label",
        "RAM: 136 GiB used of 256 GiB",
      ),
    );

    // The memory view choice lives in the memory section, not in the page header.
    await user.click(within(memory).getByRole("button", { name: "By machine" }));

    expect(
      within(memory)
        .getAllByRole("img")
        .map((bar) => bar.getAttribute("aria-label")),
    ).toEqual([
      "RAM on Spark one: 68 GiB used of 128 GiB",
      "RAM on Spark two: 68 GiB used of 128 GiB",
    ]);
    expect(within(memory).getByText("Spark two")).toBeVisible();
    expectResourceMarker(within(memory).getByText("Spark one"), "spark-01");
    expectResourceMarker(within(memory).getByText("Spark two"), "spark-02");
    expect(readMemoryView(window.localStorage)).toBe(MemoryView.BY_MACHINE);
  });

  it("expands a memory row to its history with a line per model, streamed only while expanded", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const streams = countedWatch();
    renderModelsSection({
      deployments: [
        localDeployment("deployment-a", gemma, {
          observedPlacements: [{ managedNodeId: "gpu1" }],
          status: ModelDeploymentStatus.READY,
        }),
      ],
      machines: [readableMachine("gpu1", "gpu1")],
      metricsClient: streams.client,
      models: [gemma],
      snapshots: { gpu1: workstationSnapshot() },
    });

    const memory = await screen.findByRole("region", { name: "Memory" });
    const vram = await within(memory).findByRole("button", { name: "VRAM" });
    expect(within(memory).getByRole("button", { name: "RAM" })).toHaveAttribute(
      "aria-expanded",
      "false",
    );
    expect(streams.watch).not.toHaveBeenCalled();

    await user.click(vram);
    const chart = await screen.findByRole("group", { name: "VRAM history" });
    await waitFor(() =>
      expect(chartReadout(chart)).toBe("Used 24 GiB Capacity 32 GiB Gemma 20 GiB"),
    );
    expect(streams.watch).toHaveBeenCalledTimes(1);
    expect(streams.queries(0)).toEqual([
      ["node.host.memory.total", ["node"]],
      ["node.host.memory.available", ["node"]],
      ["node.accelerator.memory.used", ["node", "device"]],
      ["node.accelerator.memory.total", ["node", "device"]],
      ["deployment.accelerator.memory.used", ["deployment", "node", "device"]],
    ]);

    await user.click(vram);
    expect(screen.queryByRole("group", { name: "VRAM history" })).toBeNull();
    await waitFor(() => expect(streams.open()).toBe(0));
  });

  it("holds at most three metric streams with the deployment inspector open", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const streams = countedWatch();
    renderModelsSection({
      deployments: [
        localDeployment("deployment-a", gemma, {
          observedPlacements: [{ managedNodeId: "gpu1" }],
          status: ModelDeploymentStatus.READY,
        }),
      ],
      machines: [readableMachine("gpu1", "gpu1")],
      metricsClient: streams.client,
      models: [gemma],
      snapshots: { gpu1: workstationSnapshot() },
    });

    const memory = await screen.findByRole("region", { name: "Memory" });
    await user.click(await within(memory).findByRole("button", { name: "RAM" }));
    await screen.findByRole("group", { name: "RAM history" });
    await user.click(screen.getByRole("button", { name: "View details for Gemma" }));
    await screen.findByRole("region", { name: "Deployment" });

    await waitFor(() => expect(streams.open()).toBeGreaterThan(1));
    expect(streams.open()).toBeLessThanOrEqual(3);
  });

  it("says memory is unavailable when no machine has a readable snapshot", async () => {
    renderModelsSection({
      machines: [
        create(MachineSchema, {
          currentHardwareSnapshotAvailability: {
            availability: { case: "unavailable", value: { code: "NOT_OBSERVED", message: "no" } },
          },
          machineId: "spark-01",
        }),
      ],
    });

    expect(
      await screen.findByText("Memory usage is unavailable: no machine reported readable memory."),
    ).toBeVisible();
    expect(screen.queryByRole("button", { name: "By machine" })).not.toBeInTheDocument();
  });
});

/**
 * A metrics client answering gpu1's memory and one deployment's VRAM, counting the watch
 * streams it holds open.
 */
function countedWatch() {
  const gib = 1024 ** 3;
  const byMetric: Record<string, ReturnType<typeof metricSeries>[]> = {
    "deployment.accelerator.memory.used": [
      metricSeries({ deployment: "deployment-a", device: "gpu-0", node: "gpu1" }, [
        gaugePoint(9, 20 * gib),
      ]),
    ],
    "node.accelerator.memory.total": [
      metricSeries({ device: "gpu-0", node: "gpu1" }, [gaugePoint(9, 32 * gib)]),
    ],
    "node.accelerator.memory.used": [
      metricSeries({ device: "gpu-0", node: "gpu1" }, [gaugePoint(9, 24 * gib)]),
    ],
    "node.host.memory.available": [metricSeries({ node: "gpu1" }, [gaugePoint(9, 47 * gib)])],
    "node.host.memory.total": [metricSeries({ node: "gpu1" }, [gaugePoint(9, 62 * gib)])],
  };
  const answer = staticWatch((query) =>
    queryResult({ series: byMetric[query?.metric ?? ""] ?? [], stepCount: 10 }),
  );
  let open = 0;
  const watch = vi.fn<MetricsClient["metrics"]["watchMetrics"]>((request, options) => {
    open += 1;
    options?.signal?.addEventListener("abort", () => (open -= 1));
    return answer(request, options);
  });

  return {
    client: metricsClientWith(watch),
    open: () => open,
    queries: (call: number) =>
      (watch.mock.calls[call][0].queries ?? []).map(({ query }) => [query?.metric, query?.groupBy]),
    watch,
  };
}

/** The readout text above a chart's plot: each line's label and value. */
function chartReadout(chart: HTMLElement): string {
  return [...(chart.firstElementChild?.querySelectorAll("span") ?? [])]
    .filter((span) => span.childElementCount === 0)
    .map((span) => span.textContent)
    .filter(Boolean)
    .join(" ");
}
