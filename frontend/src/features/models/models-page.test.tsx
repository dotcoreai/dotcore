import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import { workstationSnapshot } from "@/features/memory/machine-test-support";
import {
  counterPoint,
  distributionPoint,
  gaugePoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { ModelArtifactSchema, ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { ListModelsResponseSchema } from "@/gen/dotcore/catalog/v1/service_pb";
import { AcceleratorMemoryObservationSchema } from "@/gen/dotcore/deployment/v1/accelerator_memory_pb";
import {
  InferenceRouteAvailabilitySchema,
  InferenceRouteSchema,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import {
  type ModelDeployment,
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
import { LOADING_MINIMUM_VISIBLE_MS, LOADING_REVEAL_DELAY_MS } from "@/hooks/use-delayed-loading";
import { expectHeadingOutline } from "@/test/heading-outline";
import { chooseMoreAction } from "@/test/inspector-actions";

import { projectModelSnapshot } from "./model-projection";
import {
  catalogModel,
  currentLocation,
  localDeployment,
  modelTarget,
  readableMachine,
  renderModelsSection,
  unifiedSnapshot,
  upstreamDeployment,
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

describe("ModelsPage table", () => {
  it("reads h1 page, then h2 sections with no skipped level", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    renderModelsSection({
      models: [gemma],
      deployments: [
        localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
      ],
    });
    await waitFor(() => {
      // Inference holds its place while metrics are read, so it heads the page from the start.
      expect(screen.getAllByRole("heading", { level: 2 }).map((h) => h.textContent)).toEqual([
        "Inference",
        "Running",
      ]);
      expectHeadingOutline(document.body, [1, 2, 2]);
    });
    const deploy = screen.getByRole("link", { name: "Deploy" });
    expect(deploy).toHaveAttribute("href", "/models/catalog");
    expect(deploy.querySelector("svg")).toHaveClass("lucide-rocket");
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
  it("shows each deployment's name, status, machine, requests and VRAM", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const qwen = catalogModel("qwen", "Qwen", ["default"]);
    const template = upstreamTemplate();
    renderModelsSection({
      deployments: [
        placedDeployment(
          localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
          "gpu1",
          [["GPU-0", 4n * GiB]],
        ),
        placedDeployment(
          localDeployment("deployment-b", qwen, { status: ModelDeploymentStatus.READY }),
          "gpu1",
          [
            ["GPU-0", 2n * GiB],
            ["GPU-1", 1n * GiB],
          ],
        ),
        placedDeployment(
          localDeployment("deployment-c", gemma, {
            status: ModelDeploymentStatus.FAILED,
            failure: { message: "Engine exited" },
          }),
        ),
        upstreamDeployment(template, ModelDeploymentStatus.READY),
      ],
      machines: [readableMachine("gpu1", "Workstation")],
      metricsClient: trafficWatch().client,
      models: [gemma, qwen],
      templates: [template],
    });

    const table = await screen.findByRole("table", { name: "Running models" });
    await waitFor(() =>
      expect(rowTexts(table)).toEqual([
        "Gemma / Gemma · Q8_0 | Ready | Workstation | 3 | 4 GiB",
        "Gemma / Gemma · Q8_0 | Failed | — | 0 | —",
        "Private model / External | Ready | Upstream | 1 | —",
        "Qwen / Qwen · Q8_0 | Ready | Workstation | 0 | 3 GiB",
      ]),
    );
    expect(headerTexts(table)).toEqual(["Deployment", "Status", "Machine", "Requests", "VRAM"]);
    // Same-named deployments and upstream ones name their id, so each row reads apart.
    expect(screen.getByRole("row", { name: "Gemma (deployment-a)" })).toBeVisible();
    expect(screen.getByRole("row", { name: "Private model (upstream-new)" })).toBeVisible();
    // The failure message rides the status badge's tooltip, not the row.
    const failed = screen.getByRole("row", { name: "Gemma (deployment-c)" });
    expect(failed.querySelector('[data-slot="badge"]')).toHaveAttribute("title", "Engine exited");
    expect(within(failed).queryByText("Engine exited")).not.toBeInTheDocument();
  });

  it("offers generated tokens, token generation and time to first token in the View menu", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    renderModelsSection({
      deployments: [
        placedDeployment(
          localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
        ),
      ],
      metricsClient: trafficWatch().client,
      models: [gemma],
    });

    const table = await screen.findByRole("table", { name: "Running models" });
    await user.click(screen.getByRole("button", { name: "View" }));
    const menu = await screen.findByRole("menu");
    const hidden = ["Generated tokens", "Token generation", "Time to first token"];
    for (const label of hidden) {
      expect(within(menu).getByRole("menuitemcheckbox", { name: label })).not.toBeChecked();
    }

    for (const label of hidden) {
      await user.click(
        within(screen.getByRole("menu")).getByRole("menuitemcheckbox", { name: label }),
      );
    }
    await user.keyboard("{Escape}");

    expect(headerTexts(table).slice(-3)).toEqual(hidden);
    await waitFor(() =>
      expect(rowTexts(table)).toEqual([
        "Gemma / Gemma · Q8_0 | Ready | — | 3 | — | 30 | 50 tokens/s | 250 ms",
      ]),
    );
  });

  it("opens a row's panel on click, Enter or Space, marks it selected, and returns focus on close", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const { router } = renderModelsSection({
      deployments: [
        localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
      ],
      models: [gemma],
    });

    for (const open of ["click", "{Enter}", " "]) {
      const row = await screen.findByRole("row", { name: "Gemma" });
      if (open === "click") {
        await user.click(row);
      } else {
        row.focus();
        await user.keyboard(open);
      }

      const panel = await screen.findByRole("complementary", { name: "Gemma" });
      expect(currentLocation(router)).toBe("/models?panel=models%2Fdeployment%3Adeployment-a");
      expect(row).toHaveAttribute("data-state", "selected");
      expect(row).toHaveAttribute("aria-controls", panel.id);

      await user.click(within(panel).getByRole("button", { name: /^Close/ }));
      await waitFor(() => expect(panel).not.toBeInTheDocument());
      expect(row).toHaveFocus();
      expect(row).not.toHaveAttribute("data-state");
    }
  });

  it("names a transitioning state the same way in the row and in its inspector", async () => {
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

    const row = await screen.findByRole("row", { name: "Gemma" });
    expect(row).toHaveTextContent("Downloading");
    expect(row).not.toHaveTextContent("Downloading model artifacts");

    await user.click(row);
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

    await user.click(await screen.findByRole("row", { name: "Gemma" }));
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

    await user.click(
      await screen.findByRole("row", { name: "Private model (upstream-deployment)" }),
    );
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

    await user.click(await screen.findByRole("row", { name: "Gemma" }));
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

describe("ModelsPage deployment panel", () => {
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

    await user.click(await screen.findByRole("row", { name: "Gemma" }));
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

  it("shows the site's inference and opens a deployment on the page's range", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const streams = countedWatch();
    renderModelsSection({
      deployments: [
        localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
      ],
      metricsClient: streams.client,
      models: [gemma],
    });

    const inference = await screen.findByRole("region", { name: "Inference" });
    await waitFor(() =>
      expect(within(inference).getByRole("button", { name: /^Requests/ })).toHaveTextContent(
        "Requests3",
      ),
    );
    // Scoped to deployments, the cards count the attempts that reached them.
    expect(streams.queries(0)[0]).toEqual(["router.attempts", []]);

    const range = screen.getByRole("radiogroup", { name: "Models range" });
    await user.click(within(range).getByRole("radio", { name: "6h" }));
    await waitFor(() => expect(streams.watch).toHaveBeenCalledTimes(2));

    await user.click(screen.getByRole("row", { name: "Gemma" }));
    const panel = await screen.findByRole("complementary", { name: "Gemma" });
    expect(await within(panel).findByRole("radio", { name: "6h" })).toBeChecked();
  });

  it("holds one metric stream for the page and one for the deployment inspector", async () => {
    const user = userEvent.setup();
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const streams = countedWatch();
    renderModelsSection({
      deployments: [
        placedDeployment(
          localDeployment("deployment-a", gemma, {
            observedPlacements: [{ managedNodeId: "gpu1" }],
            status: ModelDeploymentStatus.READY,
          }),
          "gpu1",
          [["gpu-0", 20n * GiB]],
        ),
      ],
      machines: [readableMachine("gpu1", "gpu1")],
      metricsClient: streams.client,
      models: [gemma],
      snapshots: { gpu1: workstationSnapshot() },
    });

    // The inference cards only; the rows read no GPU use.
    await waitFor(() => expect(streams.open()).toBe(1));
    expect(streams.watch).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole("row", { name: "Gemma" }));
    await screen.findByRole("region", { name: "Deployment" });

    await waitFor(() => expect(streams.open()).toBe(2));
  });
});

/**
 * A metrics client answering gpu1's memory, one deployment's VRAM and three attempts,
 * counting the watch streams it holds open.
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
    "router.attempts": [metricSeries({}, [counterPoint(9, 0, false, 3)])],
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

/**
 * A metrics client answering per-deployment traffic over the range, three attempts reaching
 * deployment-a and one the upstream deployment.
 */
function trafficWatch() {
  const watch = vi.fn<MetricsClient["metrics"]["watchMetrics"]>(
    staticWatch((query) => {
      const byDeployment = query?.groupBy?.includes("deployment") ?? false;
      const counter = (count: number, labels: Record<string, string> = {}) =>
        metricSeries(labels, [counterPoint(9, 0, false, count)]);
      const mean = (sum: number, count: number, labels: Record<string, string> = {}) =>
        metricSeries(labels, [distributionPoint(9, { 0.95: sum }, false, { count, sum })]);
      const series = (() => {
        switch (query?.metric) {
          case "router.attempts":
            return byDeployment
              ? [
                  counter(3, { deployment: "deployment-a" }),
                  counter(1, { deployment: "upstream-new" }),
                ]
              : [counter(4)];
          case "router.tokens.completion":
            return byDeployment ? [counter(30, { deployment: "deployment-a" })] : [counter(30)];
          case "router.attempt.decode_rate":
            return byDeployment ? [mean(200, 4, { deployment: "deployment-a" })] : [mean(200, 4)];
          case "router.attempt.ttft":
            return byDeployment ? [mean(0.5, 2, { deployment: "deployment-a" })] : [mean(0.5, 2)];
          default:
            return [];
        }
      })();

      return queryResult({ series, stepCount: 10 });
    }),
  );

  return { client: metricsClientWith(watch), watch };
}

/**
 * A local deployment declaring Q8_0 weights, placed on `node` when given, with its accelerator
 * memory measured on the given devices.
 */
function placedDeployment(
  deployment: ModelDeployment,
  node?: string,
  devices: [deviceId: string, bytes: bigint][] = [],
): ModelDeployment {
  if (deployment.snapshot?.source.case === "local") {
    deployment.snapshot.source.value.modelArtifact = create(ModelArtifactSchema, {
      weightQuantization: "Q8_0",
    });
  }
  if (node) {
    deployment.intent = create(ModelDeploymentIntentSchema, {
      placement: { executionTargetId: "gpu", siteNodeId: node },
    });
    deployment.acceleratorMemory = create(AcceleratorMemoryObservationSchema, {
      result: {
        case: "available",
        value: {
          devices: devices.map(([deviceId, usedMemoryBytes]) => ({
            deviceId,
            managedNodeId: node,
            usedMemoryBytes,
          })),
        },
      },
    });
  }

  return deployment;
}

/** The text of each body row: a cell's lines joined by " / ", cells by " | ". */
function rowTexts(table: HTMLElement): string[] {
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) =>
      Array.from(row.querySelectorAll("td"))
        .map((cell) => {
          const lines = Array.from(cell.querySelectorAll("div")).filter(
            (line) => !line.querySelector("div"),
          );
          return lines.length
            ? lines.map((line) => line.textContent).join(" / ")
            : cell.textContent;
        })
        .join(" | "),
    );
}

function headerTexts(table: HTMLElement): string[] {
  return within(table)
    .getAllByRole("columnheader")
    .map((header) => header.textContent ?? "");
}

const GiB = 1024n ** 3n;
