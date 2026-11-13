import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, cleanup, screen, waitFor, within } from "@testing-library/react";
import { createMemoryRouter, Outlet, RouterProvider } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { ApplicationClient } from "@/api/application-client";
import type { ModelClient } from "@/api/model-client";
import { ChatOpenerContext } from "@/features/chat/chat-opener";
import {
  devicePool,
  gib,
  hostPool,
  measured,
  nodeMemory,
  nodeMemoryPayload,
  other,
} from "@/features/memory/node-memory-test-support";
import { ModelsPollingProvider } from "@/features/models/polling/models-polling-provider";
import {
  runtimeSchemaModel,
  runtimeSchemaReleases,
} from "@/features/models/runtime/runtime-schema-test-support";
import {
  type ApplicationDeployment,
  ApplicationDeploymentDesiredState,
  ApplicationDeploymentSchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  ApplicationTargetOptionSchema,
  type CreateApplicationDeploymentResponse,
  CreateApplicationDeploymentResponseSchema,
  ListApplicationDeploymentsResponseSchema,
  type ListApplicationTargetsRequest,
  ListApplicationTargetsResponseSchema,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationDeploymentStateSchema,
  ApplicationDeploymentStep,
  ApplicationEndpointSchema,
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import { ApplicationSchema } from "@/gen/dotcore/catalog/v1/application_pb";
import { ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
import { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";
import {
  ListApplicationsResponseSchema,
  ListEngineReleasesResponseSchema,
  ListHardwareClassesResponseSchema,
  ListModelsResponseSchema,
} from "@/gen/dotcore/catalog/v1/service_pb";
import { ExactPlacementSchema } from "@/gen/dotcore/deployment/v1/placement_pb";
import { EngineKind } from "@/gen/dotcore/engine/v1/engine_pb";
import {
  type InferenceRouteView,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
  ModelDeploymentSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  CreateLocalModelDeploymentRequestSchema,
  type CreateModelDeploymentResponse,
  CreateModelDeploymentResponseSchema,
  ListModelDeploymentsResponseSchema,
  type ListModelProfileCompatibilityRequest,
  ListModelProfileCompatibilityResponseSchema,
  type ListModelRecommendationsRequest,
  ListModelRecommendationsResponseSchema,
  ModelCompatibilityStatus,
  ModelRecommendationFit,
  RecommendedModelProfileSchema,
  UseCaseModelRecommendationsSchema,
} from "@/gen/dotcore/model/v1/service_pb";
import {
  ModelDeploymentProgressUnit,
  ModelDeploymentStateSchema,
  ModelDeploymentStatus,
  ModelDeploymentStep,
} from "@/gen/dotcore/model/v1/state_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  GetConsoleSettingsResponseSchema,
  ListConfiguredExecutionTargetsResponseSchema,
  ListMachinesResponseSchema,
  MachineSchema,
  NodeObservationState,
  UpdateConsoleSettingsRequestSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import * as clipboard from "@/lib/clipboard";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeConsoleSettings } from "@/test/fake-console-settings";
import { FakeResourceWatch } from "@/test/fake-resource-watch";
import { renderWithClients } from "@/test/render-with-clients";
import { setupUser } from "@/test/user";

import { GetStartedHiddenProvider } from "./get-started-hidden";
import { GetStartedPage } from "./get-started-page";

const { CHAT_ASSISTANT, CODING, IMAGE_GENERATION } = UseCase;

/** One fake site: what the backend recommends and what runs on the machines. */
interface Site {
  /** Per machine: display name, observation state, VRAM GiB or none for a unified machine. */
  machines: { id: string; state?: NodeObservationState; vramGib?: number }[];
  modelDeployments?: ReturnType<typeof modelDeployment>[];
  applicationDeployments?: ReturnType<typeof applicationDeployment>[];
  rows: Partial<Record<UseCase, Row[]>>;
  /** What the model service answers a create with; an accepted deployment without one. */
  createModel?: () => Promise<CreateModelDeploymentResponse>;
  /** What the app service answers a create with; an accepted deployment without one. */
  createApp?: () => Promise<CreateApplicationDeploymentResponse>;
  /** Whether the model catalog read never answers. */
  catalogPending?: boolean;
  /** How many memory estimate reads fail before they answer. */
  failedEstimates?: number;
  routes?: InferenceRouteView[];
  /** Whether the site already hides Get started. */
  getStartedHidden?: boolean;
  /** Whether saving the console settings fails. */
  failSettings?: boolean;
  /** Targets per app id, as `[machine, matches preference, deployment id?]`. */
  targets: Record<string, [string, boolean, string?][]>;
}

/** A recommendation row: model, profile, context per request, machine and running deployment. */
type Row = [model: string, profile: string, context: number, machine: string, deployment?: string];

/** Each catalog profile's estimate at any context, in GiB. */
const estimates: Record<string, number> = {
  [`${runtimeSchemaModel.modelId}/ninfer-nvfp4`]: 24,
  "qwen-27b/nvfp4": 22,
  "qwen-27b/q4": 21,
  "qwen-27b/q5": 26,
};

let compatibilityReads: ListModelProfileCompatibilityRequest[];
const hiddenRequest = (hidden: boolean) =>
  create(UpdateConsoleSettingsRequestSchema, {
    settings: { getStartedHidden: hidden },
    updateMask: { paths: ["get_started_hidden"] },
  });

const writeClipboard = vi.fn<typeof clipboard.writeClipboard>();

beforeEach(() => {
  compatibilityReads = [];
  writeClipboard.mockReset().mockResolvedValue(undefined);
  vi.spyOn(clipboard, "writeClipboard").mockImplementation(writeClipboard);
  // The inference origin comes from the environment; pin it so command assertions are deterministic.
  vi.stubEnv("VITE_DOTCORE_INFERENCE_ORIGIN", "http://192.0.2.10:6208");
});

describe("GetStartedPage", () => {
  it("proposes the best model and the app on an idle machine", async () => {
    await renderPage({
      machines: [{ id: "gpu1", vramGib: 32 }],
      rows: {
        [CHAT_ASSISTANT]: [
          ["qwen-27b", "nvfp4", 32768, "gpu1"],
          ["qwen-27b", "q4", 32768, "gpu1"],
        ],
        [CODING]: [["qwen-27b", "nvfp4", 131072, "gpu1"]],
      },
      targets: { "open-webui": [["gpu1", true]] },
    });

    expect(useCaseCards()).toEqual([
      { label: "Chat assistant", pressed: true },
      { label: "Coding", pressed: false },
      { label: "Image generation", pressed: false },
    ]);
    await waitFor(() => expect(detailsOf("Model")).toContainEqual(["Memory", "22 GiB"]));
    expect(modelItem()).toEqual({
      choices: "Other choices (1)",
      customize: true,
      details: [
        ["Machine", "gpu1"],
        ["Memory", "22 GiB"],
        ["Context per request", "32k"],
      ],
      head: ["Qwen3.8 27B", "NVFP4", "ninfer"],
      notice: undefined,
    });
    expect(appItem()).toEqual({
      description: "Self-hosted chat interface for the models on this site.",
      head: ["Open WebUI", "0.11.4"],
      notice: undefined,
    });
    expect(primaryAction()).toEqual({ disabled: false, name: "Deploy" });
    expect(compatibilityReads.map((read) => [read.modelId, read.scenario])).toEqual([
      [
        "qwen-27b",
        expect.objectContaining({ contextTokensPerRequest: 32768, totalContextTokens: 32768 }),
      ],
    ]);
  });

  it("switches the proposal to another choice", async () => {
    const user = setupUser();
    await renderPage({
      machines: [{ id: "gpu1", vramGib: 32 }],
      rows: {
        [CHAT_ASSISTANT]: [
          ["qwen-27b", "nvfp4", 32768, "gpu1"],
          ["qwen-27b", "q4", 65536, "gpu1"],
        ],
      },
      targets: { "open-webui": [["gpu1", true]] },
    });

    const toggle = await screen.findByRole("button", { name: "Other choices (1)" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    await user.click(toggle);
    const choices = within(screen.getByRole("group", { name: "Model choices" })).getAllByRole(
      "button",
    );
    expect(
      choices.map((choice) => [choice.textContent, choice.getAttribute("aria-pressed")]),
    ).toEqual([
      ["Qwen3.8 27BNVFP4 · ninfer · gpu132k context · 22 GiB", "true"],
      ["Qwen3.8 27BUnsloth Q4_K_XL · llama.cpp · gpu164k context · 22 GiB", "false"],
    ]);

    await user.click(choices[1]);

    expect(modelItem()).toMatchObject({
      choices: "Other choices (1)",
      details: [
        ["Machine", "gpu1"],
        ["Memory", "22 GiB"],
        ["Context per request", "64k"],
      ],
      head: ["Qwen3.8 27B", "Unsloth Q4_K_XL", "llama.cpp"],
    });
    expect(choices[1]).toHaveAttribute("aria-pressed", "true");
    expect(toggle).toHaveAttribute("aria-expanded", "true");
  });

  it("proposes the app next to a running model, shown with its own context and memory", async () => {
    await renderPage({
      machines: [{ id: "gpu1", vramGib: 32 }],
      modelDeployments: [modelDeployment("dep-1", "gpu1", 65536n, 23n * gib)],
      rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1", "dep-1"]] },
      targets: { "open-webui": [["gpu1", true]] },
    });

    await waitFor(() => expect(detailsOf("Model")).toContainEqual(["Memory", "23 GiB"]));
    expect(modelItem()).toEqual({
      choices: undefined,
      customize: false,
      details: [
        ["Machine", "gpu1"],
        ["Memory", "23 GiB"],
        ["Context per request", "64k"],
      ],
      head: ["Qwen3.8 27B", "NVFP4", "ninfer", "Ready"],
      notice: undefined,
    });
    expect(appItem().head).toEqual(["Open WebUI", "0.11.4"]);
    expect(primaryAction()).toEqual({ disabled: false, name: "Deploy" });
    expect(screen.queryByRole("region", { name: "Ready" })).not.toBeInTheDocument();
    expect(compatibilityReads).toEqual([]);
  });

  it("opens a served use case on its ready step, with no ready marks on the use cases", async () => {
    const { site, watch } = await renderPage({
      applicationDeployments: [readyApp()],
      machines: [{ id: "gpu1", vramGib: 32 }],
      modelDeployments: [modelDeployment("dep-1", "gpu1", 65536n, 23n * gib)],
      routes: [routeView("qwen3.8-27b", ["dep-1"])],
      rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1", "dep-1"]] },
      targets: { "open-webui": [["gpu1", true, "app-new"]] },
    });

    const ready = await screen.findByRole("region", { name: "Ready" });
    expect(readyItems(ready)).toMatchObject([
      { action: "Chat", kind: "Model", name: "Qwen3.8 27B", status: "Ready" },
      { action: "Open", kind: "App", name: "Open WebUI", status: "Ready" },
    ]);
    expect(screen.queryByRole("button", { name: /^(Deploy|Continue)$/ })).not.toBeInTheDocument();
    expect(
      within(screen.getByRole("group", { name: "Use case" })).queryByText("Ready"),
    ).not.toBeInTheDocument();

    // The model stops being ready: the step follows it back to its progress.
    site.modelDeployments!.splice(0, 1, {
      ...modelDeployment("dep-1", "gpu1", 65536n, 23n * gib),
      state: create(ModelDeploymentStateSchema, { status: ModelDeploymentStatus.LOADING }),
    });
    await act(() => watch.tick());
    await waitFor(() => expect(progressItems()[0]).toMatchObject({ status: "Loading" }));
  });

  it("says why no machine has room for a model and keeps the app", async () => {
    await renderPage({
      machines: [{ id: "gpu1", vramGib: 32 }],
      modelDeployments: [modelDeployment("dep-9", "gpu1", 8192n, 27n * gib, "Gemma 4 26B A4B")],
      rows: {},
      targets: { "open-webui": [["gpu1", true]] },
    });

    const item = await screen.findByRole("region", { name: "Model" });
    expect(within(item).getByText(/No machine has room/).textContent).toBe(
      "No machine has room for a chat assistant model.Gemma 4 26B A4B uses most of gpu1.",
    );
    expect(within(item).getByRole("link", { name: "Models" })).toHaveAttribute("href", "/models");
    expect(within(item).getByRole("button", { name: "Add a machine" })).toBeEnabled();
    expect(appItem()).toEqual({
      description: "Self-hosted chat interface for the models on this site.",
      head: ["Open WebUI", "0.11.4"],
      notice: undefined,
    });
    expect(primaryAction()).toEqual({ disabled: true, name: "Deploy" });
  });

  it("proposes only the app for image generation and warns it runs on the CPU", async () => {
    const user = setupUser();
    await renderPage({
      machines: [{ id: "mac1" }],
      rows: { [CHAT_ASSISTANT]: [["qwen-27b", "q5", 32768, "mac1"]] },
      targets: { comfyui: [["mac1", false]], "open-webui": [["mac1", true]] },
    });

    await user.click(await screen.findByRole("button", { name: /Image generation/ }));

    expect(screen.queryByRole("region", { name: "Model" })).not.toBeInTheDocument();
    expect(appItem()).toEqual({
      description: "Node-based image and video generation.",
      head: ["ComfyUI", "0.42.5"],
      notice: "ComfyUI is only supported on NVIDIA machines.It will run slowly on the CPU of mac1.",
    });
    expect(primaryAction()).toEqual({ disabled: false, name: "Deploy" });
  });

  it("says no machine can run apps when the app has no target", async () => {
    await renderPage({
      machines: [{ id: "mac1" }],
      rows: { [CHAT_ASSISTANT]: [["qwen-27b", "q5", 32768, "mac1"]] },
      targets: {},
    });

    await waitFor(() => expect(detailsOf("Model")).toContainEqual(["Memory", "26 GiB"]));
    expect(modelItem()).toMatchObject({ head: ["Qwen3.8 27B", "Unsloth Q5_K_XL", "llama.cpp"] });
    expect(appItem()).toEqual({
      description: "Self-hosted chat interface for the models on this site.",
      head: ["Open WebUI", "0.11.4"],
      notice: "No machine can run apps.Install Docker on a machine to run Open WebUI.",
    });
    expect(primaryAction()).toEqual({ disabled: false, name: "Deploy" });
  });

  it("lists each machine with its status and memory, and opens the add machine steps", async () => {
    const user = setupUser();
    await renderPage({
      machines: [
        { id: "gpu1", vramGib: 32 },
        { id: "mac1", state: NodeObservationState.STALE },
      ],
      modelDeployments: [modelDeployment("dep-1", "gpu1", 32768n, 22n * gib)],
      rows: {},
      targets: {},
    });

    const card = screen.getByRole("region", { name: "Machines" });
    await waitFor(() =>
      expect(machineRows(card)).toEqual([
        { name: "gpu1", rows: ["VRAM", "RAM"], status: "Connected" },
        { name: "mac1", rows: ["RAM"], status: "Stale" },
      ]),
    );

    await user.click(within(card).getByRole("button", { name: "Add a machine" }));
    expect(await screen.findByRole("dialog", { name: "Add a machine" })).toBeVisible();
  });

  describe("customize", () => {
    it("opens the deploy dialog on the proposal and applies the edits without deploying", async () => {
      const user = setupUser();
      const { createApplicationDeployment, createLocalModelDeployment } = await renderPage({
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: { [CHAT_ASSISTANT]: [[runtimeSchemaModel.modelId, "ninfer-nvfp4", 32768, "gpu1"]] },
        targets: { "open-webui": [["gpu1", true]] },
      });

      await user.click(await screen.findByRole("button", { name: "Customize" }));
      const dialog = await screen.findByRole("dialog", { name: "Qwen3.8 27B — Customize model" });
      await waitFor(() =>
        expect(within(dialog).getByRole("radio", { name: /NVFP4 ninfer/ })).toBeChecked(),
      );
      await user.click(within(dialog).getByRole("button", { name: "Next" }));
      expect(within(dialog).getByRole("textbox", { name: "Total context" })).toHaveValue("32768");
      const perRequest = within(dialog).getByRole("textbox", { name: "Context per request" });
      expect(perRequest).toHaveValue("32768");

      await user.clear(perRequest);
      await user.type(perRequest, "65536");
      await user.tab();
      await user.click(within(dialog).getByRole("button", { name: "Next" }));
      expect(within(dialog).getByText("Step 3 of 3 — Deploy")).toBeVisible();
      expect(summaryTarget(dialog)).toBe("gpu1 / NVIDIA GeForce RTX 5090");
      await user.click(within(dialog).getByRole("button", { name: "Apply" }));

      await waitFor(() => expect(dialog).not.toBeInTheDocument());
      expect(createLocalModelDeployment).not.toHaveBeenCalled();
      expect(createApplicationDeployment).not.toHaveBeenCalled();
      expect(detailsOf("Model")).toEqual([
        ["Machine", "gpu1"],
        ["Memory", "25 GiB"],
        ["Context per request", "64k"],
      ]);

      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(createApplicationDeployment).toHaveBeenCalledOnce());
      const request = createLocalModelDeployment.mock.calls[0][0];
      expect(request).toMatchObject({
        modelDeploymentName: "Qwen3.8 27B",
        modelId: runtimeSchemaModel.modelId,
        placement: { executionTargetId: "model", siteNodeId: "gpu1" },
        profileId: "ninfer-nvfp4",
      });
      expect(contextValues(request.configuration?.engineParameterValues?.values ?? [])).toEqual({
        "context-per-request": 65536n,
        "total-context": 65536n,
      });
    });

    it("keeps the values the operator applied when Customize reopens", async () => {
      const user = setupUser();
      const { createLocalModelDeployment } = await renderPage({
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: { [CHAT_ASSISTANT]: [[runtimeSchemaModel.modelId, "ninfer-nvfp4", 32768, "gpu1"]] },
        targets: { "open-webui": [["gpu1", true]] },
      });

      // Opens Customize in advanced mode on the Runtime step.
      const openRuntime = async () => {
        await user.click(await screen.findByRole("button", { name: "Customize" }));
        const dialog = await screen.findByRole("dialog", { name: "Qwen3.8 27B — Customize model" });
        await user.click(await within(dialog).findByRole("checkbox", { name: "Advanced" }));
        for (let step = 0; step < 3; step += 1)
          await user.click(within(dialog).getByRole("button", { name: "Next" }));
        expect(within(dialog).getByText("Step 4 of 5 — Runtime")).toBeVisible();
        return {
          dialog,
          hostContext: within(dialog).getByRole("textbox", { name: "Host context memory" }),
          parallel: within(dialog).getByRole("textbox", { name: "Parallel requests" }),
        };
      };
      const first = await openRuntime();
      expect(first.parallel).toHaveValue("4");
      expect(first.hostContext).toHaveValue("20480");
      await user.clear(first.parallel);
      await user.type(first.parallel, "2");
      await user.clear(first.hostContext);
      await user.type(first.hostContext, "1024");
      await user.click(within(first.dialog).getByRole("button", { name: "Next" }));
      const firstName = within(first.dialog).getByRole("textbox", { name: "Deployment name" });
      expect(firstName).toHaveValue("Qwen3.8 27B");
      await user.clear(firstName);
      await user.type(firstName, "Team model");
      const firstRoute = within(first.dialog).getByRole("textbox", { name: "Route name" });
      await user.clear(firstRoute);
      await user.type(firstRoute, "team-route");
      await user.click(within(first.dialog).getByRole("button", { name: "Apply" }));
      await waitFor(() => expect(first.dialog).not.toBeInTheDocument());

      const second = await openRuntime();
      expect(second.parallel).toHaveValue("2");
      expect(second.hostContext).toHaveValue("1024");
      await user.click(within(second.dialog).getByRole("button", { name: "Next" }));
      expect(within(second.dialog).getByRole("textbox", { name: "Deployment name" })).toHaveValue(
        "Team model",
      );
      expect(within(second.dialog).getByRole("textbox", { name: "Route name" })).toHaveValue(
        "team-route",
      );
      await user.click(within(second.dialog).getByRole("button", { name: "Apply" }));
      await waitFor(() => expect(second.dialog).not.toBeInTheDocument());
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(createLocalModelDeployment).toHaveBeenCalledOnce());
      const request = createLocalModelDeployment.mock.calls[0][0];
      expect(request).toMatchObject({
        exposure: { kind: { case: "direct", value: { routeName: "team-route" } } },
        modelDeploymentName: "Team model",
      });
      expect(
        Object.fromEntries(
          (request.configuration?.engineParameterValues?.values ?? []).map(
            ({ parameterId, value }) => [parameterId, value.value],
          ),
        ),
      ).toEqual({
        "context-per-request": 32768n,
        "host-context-mib": 1024n,
        "parallel-requests": 2n,
        "total-context": 32768n,
      });
    });

    it("sizes a customized launch at its total context and keeps the choice's own memory", async () => {
      const user = setupUser();
      await renderPage({
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: {
          [CHAT_ASSISTANT]: [
            [runtimeSchemaModel.modelId, "ninfer-nvfp4", 32768, "gpu1"],
            ["qwen-27b", "q4", 32768, "gpu1"],
          ],
        },
        targets: { "open-webui": [["gpu1", true]] },
      });

      await user.click(await screen.findByRole("button", { name: "Customize" }));
      const dialog = await screen.findByRole("dialog", { name: "Qwen3.8 27B — Customize model" });
      await user.click(await within(dialog).findByRole("button", { name: "Next" }));
      const total = within(dialog).getByRole("textbox", { name: "Total context" });
      await user.clear(total);
      await user.type(total, "131072");
      await user.tab();
      expect(within(dialog).getByRole("textbox", { name: "Context per request" })).toHaveValue(
        "32768",
      );
      await user.click(within(dialog).getByRole("button", { name: "Next" }));
      await user.click(within(dialog).getByRole("button", { name: "Apply" }));
      await waitFor(() => expect(dialog).not.toBeInTheDocument());

      await waitFor(() => expect(detailsOf("Model")).toContainEqual(["Memory", "27 GiB"]));
      expect(detailsOf("Model")).toEqual([
        ["Machine", "gpu1"],
        ["Memory", "27 GiB"],
        ["Context per request", "32k"],
      ]);
      await user.click(screen.getByRole("button", { name: "Other choices (1)" }));
      expect(
        within(screen.getByRole("group", { name: "Model choices" }))
          .getAllByRole("button")
          .map((choice) => [choice.textContent, choice.getAttribute("aria-pressed")]),
      ).toEqual([
        ["Qwen3.8 27BNVFP4 ninfer · ninfer · gpu132k context · 24 GiB", "true"],
        ["Qwen3.8 27BUnsloth Q4_K_XL · llama.cpp · gpu132k context · 21 GiB", "false"],
      ]);
    });

    it("keeps Customize disabled until the model catalog loads", async () => {
      await renderPage({
        catalogPending: true,
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1"]] },
        targets: { "open-webui": [["gpu1", true]] },
      });

      expect(screen.getByRole("button", { name: "Customize" })).toBeDisabled();
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    });

    it("reads a failed memory estimate again", async () => {
      await renderPage({
        failedEstimates: 1,
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1"]] },
        targets: { "open-webui": [["gpu1", true]] },
      });

      await waitFor(() => expect(detailsOf("Model")).toContainEqual(["Memory", "22 GiB"]));
      expect(compatibilityReads.map((read) => read.modelId)).toEqual(["qwen-27b", "qwen-27b"]);
    });
  });

  describe("deploy", () => {
    it("creates the model at its context per request, then the app", async () => {
      const user = setupUser();
      const { createApplicationDeployment, createLocalModelDeployment } = await renderPage({
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1"]] },
        targets: { "open-webui": [["gpu1", true]] },
      });

      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(createApplicationDeployment).toHaveBeenCalledOnce());
      expect(createLocalModelDeployment.mock.calls).toEqual([
        [
          {
            ...create(CreateLocalModelDeploymentRequestSchema, {
              configuration: {
                engineParameterValues: {
                  values: [
                    unsigned("context-per-request", 32768n),
                    unsigned("total-context", 32768n),
                  ],
                },
              },
              modelId: "qwen-27b",
              placement: { executionTargetId: "model", siteNodeId: "gpu1" },
              profileId: "nvfp4",
            }),
            exposure: { kind: { case: "direct", value: { routeName: "qwen3.8-27b" } } },
            modelDeploymentName: "Qwen3.8 27B",
          },
        ],
      ]);
      expect(createApplicationDeployment.mock.calls).toEqual([
        [
          {
            applicationDeploymentName: "Open WebUI",
            applicationId: "open-webui",
            parameterValues: [],
            placement: create(ExactPlacementSchema, {
              executionTargetId: "docker",
              siteNodeId: "gpu1",
            }),
          },
        ],
      ]);
      expect(createLocalModelDeployment.mock.invocationCallOrder[0]).toBeLessThan(
        createApplicationDeployment.mock.invocationCallOrder[0],
      );
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    });

    it("shows a taken route on the card and creates no app", async () => {
      const user = setupUser();
      const { createApplicationDeployment, createLocalModelDeployment } = await renderPage({
        createModel: () =>
          Promise.reject(
            new ConnectError(
              "Inference route name already exists: qwen3.8-27b",
              Code.AlreadyExists,
            ),
          ),
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1"]] },
        targets: { "open-webui": [["gpu1", true]] },
      });

      await user.click(screen.getByRole("button", { name: "Deploy" }));

      expect((await screen.findByRole("alert")).textContent).toBe(
        'An inference route named "qwen3.8-27b" already exists. Change the model\'s name in Customize.',
      );
      expect(createLocalModelDeployment).toHaveBeenCalledOnce();
      expect(createApplicationDeployment).not.toHaveBeenCalled();
      expect(primaryAction()).toEqual({ disabled: false, name: "Deploy" });
    });

    it("creates only the app next to a running model", async () => {
      const user = setupUser();
      const { createApplicationDeployment, createLocalModelDeployment } = await renderPage({
        machines: [{ id: "gpu1", vramGib: 32 }],
        modelDeployments: [modelDeployment("dep-1", "gpu1", 65536n, 23n * gib)],
        rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1", "dep-1"]] },
        targets: { "open-webui": [["gpu1", true]] },
      });

      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(createApplicationDeployment).toHaveBeenCalledOnce());
      expect(createApplicationDeployment.mock.calls[0][0]).toMatchObject({
        applicationId: "open-webui",
      });
      expect(createLocalModelDeployment).not.toHaveBeenCalled();
    });

    it("retries only the app after the model was created, keeping that model as the proposal", async () => {
      const user = setupUser();
      let appFailures = 1;
      const site: Site = {
        createApp: () =>
          appFailures-- > 0
            ? Promise.reject(new ConnectError("app target is busy", Code.Unavailable))
            : Promise.resolve(
                create(CreateApplicationDeploymentResponseSchema, {
                  deployment: { applicationDeploymentId: "app-new" },
                }),
              ),
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: {
          [CHAT_ASSISTANT]: [
            ["qwen-27b", "nvfp4", 32768, "gpu1"],
            ["qwen-27b", "q4", 32768, "gpu1"],
          ],
        },
        targets: { "open-webui": [["gpu1", true]] },
      };
      const { createApplicationDeployment, createLocalModelDeployment, watch } =
        await renderPage(site);

      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await screen.findByRole("alert");
      expect(createLocalModelDeployment).toHaveBeenCalledOnce();
      expect(createApplicationDeployment).toHaveBeenCalledOnce();

      // The recommendations change their best row; the created model stays the proposal.
      site.rows[CHAT_ASSISTANT] = [["qwen-27b", "q4", 32768, "gpu1"]];
      site.modelDeployments!.push(startingModel(ModelDeploymentStatus.LOADING, 4));
      await act(() => watch.tick());
      await waitFor(() => expect(modelItem().head).toContain("Loading"));
      expect(modelItem()).toMatchObject({
        choices: undefined,
        customize: false,
        head: ["Qwen3.8 27B", "NVFP4", "ninfer", "Loading"],
      });

      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await screen.findByRole("status", { name: "Deployment progress" });
      expect(createLocalModelDeployment).toHaveBeenCalledOnce();
      expect(createApplicationDeployment).toHaveBeenCalledTimes(2);
    });

    it("shows the deployments queued while the creates run, and holds Deploy on every use case", async () => {
      const user = setupUser();
      await renderPage({
        createModel: () => new Promise(() => {}),
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: {
          [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1"]],
          [CODING]: [["qwen-27b", "nvfp4", 131072, "gpu1"]],
        },
        targets: { "open-webui": [["gpu1", true]] },
      });

      await user.click(screen.getByRole("button", { name: "Deploy" }));
      expect(progressItems()).toEqual([
        { kind: "Model", name: "Qwen3.8 27B", status: "Queued", steps: undefined },
        { kind: "App", name: "Open WebUI", status: "Queued", steps: undefined },
      ]);
      await user.click(screen.getByRole("button", { name: /Coding/ }));

      expect(screen.getByRole("button", { name: "Deploying…" })).toBeDisabled();
    });
  });

  describe("deploying and ready", () => {
    const chatSite = (): Site => ({
      machines: [{ id: "gpu1", vramGib: 32 }],
      rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1"]] },
      targets: { "open-webui": [["gpu1", true]] },
    });

    it("follows the model and the app through their steps, then moves to ready", async () => {
      const user = setupUser();
      const { createApplicationDeployment, openChat, site, watch } = await renderPage(chatSite());
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(createApplicationDeployment).toHaveBeenCalledOnce());
      // Created but not listed by the watch yet: both wait in line, with no way back.
      expect(progressItems()).toEqual([
        { kind: "Model", name: "Qwen3.8 27B", status: "Queued", steps: undefined },
        { kind: "App", name: "Open WebUI", status: "Queued", steps: undefined },
      ]);
      expect(screen.queryByRole("button", { name: "Back" })).not.toBeInTheDocument();

      const show = async (model: ModelDeployment, app: ApplicationDeployment) => {
        site.modelDeployments!.splice(0, 1, model);
        site.applicationDeployments!.splice(0, 1, app);
        await act(() => watch.tick());
      };
      await show(
        startingModel(ModelDeploymentStatus.DOWNLOADING, 1, 25),
        startingApp(ApplicationLifecycleState.QUEUED, 0),
      );
      await waitFor(() => expect(progressItems()).toHaveLength(2));
      expect(progressItems()).toEqual([
        {
          kind: "Model",
          name: "Qwen3.8 27B",
          status: "Downloading",
          steps: "Downloading artifacts, Step 2 of 5 · 1 GiB / 4 GiB · 25%",
        },
        {
          kind: "App",
          name: "Open WebUI",
          status: "Queued",
          steps: "Preparing image, Step 1 of 3",
        },
      ]);

      await show(
        startingModel(ModelDeploymentStatus.LOADING, 4),
        startingApp(ApplicationLifecycleState.PREPARING, 0),
      );
      await waitFor(() => expect(progressItems()[0].steps).toBe("Loading model, Step 5 of 5"));

      // The model is up but the app is not: still deploying.
      await show(
        startingModel(ModelDeploymentStatus.READY, 4),
        startingApp(ApplicationLifecycleState.STARTING, 1),
      );
      await waitFor(() => expect(progressItems()[0].status).toBe("Ready"));
      expect(progressItems()[1]).toMatchObject({
        status: "Starting",
        steps: "Starting, Step 2 of 3",
      });

      site.routes!.push(routeView("qwen3.8-27b", ["dep-new"]));
      await show(startingModel(ModelDeploymentStatus.READY, 4), readyApp());

      const ready = await screen.findByRole("region", { name: "Ready" });
      expect(screen.queryByRole("status", { name: "Deployment progress" })).not.toBeInTheDocument();
      expect(readyItems(ready)).toEqual([
        {
          action: "Chat",
          details: [
            ["Route", "qwen3.8-27b"],
            ["Machine", "gpu1"],
          ],
          kind: "Model",
          name: "Qwen3.8 27B",
          status: "Ready",
        },
        {
          action: "Open",
          details: [["Address", "http://127.0.0.1:3000"]],
          href: "http://127.0.0.1:3000",
          kind: "App",
          name: "Open WebUI",
          status: "Ready",
        },
      ]);
      expect(screen.queryByRole("region", { name: "Coding agent" })).not.toBeInTheDocument();

      await user.click(within(ready).getByRole("button", { name: "Chat" }));
      expect(openChat).toHaveBeenCalledExactlyOnceWith("qwen3.8-27b", "chat");
    });

    it("goes back to the proposal from a failed model and deploys it afresh", async () => {
      const user = setupUser();
      const { createLocalModelDeployment, site, watch } = await renderPage(chatSite());
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await screen.findByRole("status", { name: "Deployment progress" });
      expect(screen.queryByRole("button", { name: "Back" })).not.toBeInTheDocument();

      site.modelDeployments!.push(startingModel(ModelDeploymentStatus.FAILED, 1));
      site.applicationDeployments!.push(readyApp());
      await act(() => watch.tick());
      await waitFor(() => expect(progressItems()[0].status).toBe("Failed"));
      await user.click(screen.getByRole("button", { name: "Back" }));

      expect(screen.queryByRole("status", { name: "Deployment progress" })).not.toBeInTheDocument();
      expect(screen.getByRole("region", { name: "Model" })).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(createLocalModelDeployment).toHaveBeenCalledTimes(2));
    });

    it("says a deployment was removed and goes back to the proposal", async () => {
      const user = setupUser();
      const { site, watch } = await renderPage(chatSite());
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      site.modelDeployments!.push(startingModel(ModelDeploymentStatus.LOADING, 4));
      await act(() => watch.tick());
      await waitFor(() => expect(progressItems()[0].status).toBe("Loading"));

      site.modelDeployments!.splice(0);
      await act(() => watch.tick());

      const progress = screen.getByRole("status", { name: "Deployment progress" });
      await waitFor(() =>
        expect(within(progress).getByText("The model deployment was removed.")).toBeVisible(),
      );
      await user.click(within(progress).getByRole("button", { name: "Back" }));
      expect(screen.getByRole("region", { name: "Model" })).toBeVisible();
    });

    it("opens a deployment's logs from the deploying step", async () => {
      const user = setupUser();
      const { logRead, site, watch } = await renderPage(chatSite());
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await screen.findByRole("status", { name: "Deployment progress" });
      site.modelDeployments!.push(startingModel(ModelDeploymentStatus.LOADING, 4));
      await act(() => watch.tick());

      await user.click(await screen.findByRole("button", { name: "Logs" }));

      expect(await screen.findByRole("dialog", { name: "Qwen3.8 27B · Logs" })).toBeVisible();
      await waitFor(() =>
        expect(logRead).toHaveBeenCalledWith(
          expect.objectContaining({ deploymentId: "dep-new" }),
          expect.anything(),
        ),
      );
    });

    it("continues to the progress when everything runs but is not ready yet", async () => {
      const user = setupUser();
      const { site, watch } = await renderPage({
        applicationDeployments: [startingApp(ApplicationLifecycleState.STARTING, 1)],
        machines: [{ id: "gpu1", vramGib: 32 }],
        modelDeployments: [modelDeployment("dep-1", "gpu1", 65536n, 23n * gib)],
        routes: [routeView("qwen3.8-27b", ["dep-1"])],
        rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1", "dep-1"]] },
        targets: { "open-webui": [["gpu1", true, "app-new"]] },
      });

      await user.click(await screen.findByRole("button", { name: "Continue" }));
      await waitFor(() => expect(progressItems()[1]).toMatchObject({ status: "Starting" }));

      site.applicationDeployments!.splice(0, 1, readyApp());
      await act(() => watch.tick());
      expect(await screen.findByRole("region", { name: "Ready" })).toBeVisible();
    });

    it("hides Get started for the site once, when a step becomes ready", async () => {
      const user = setupUser();
      const { settings, site, watch } = await renderPage({
        applicationDeployments: [startingApp(ApplicationLifecycleState.STARTING, 1)],
        machines: [{ id: "gpu1", vramGib: 32 }],
        modelDeployments: [modelDeployment("dep-1", "gpu1", 65536n, 23n * gib)],
        routes: [routeView("qwen3.8-27b", ["dep-1"])],
        rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1", "dep-1"]] },
        targets: { "open-webui": [["gpu1", true, "app-new"]] },
      });
      await user.click(await screen.findByRole("button", { name: "Continue" }));
      await waitFor(() => expect(progressItems()[1]).toMatchObject({ status: "Starting" }));
      expect(settings.updates).toEqual([]);

      site.applicationDeployments!.splice(0, 1, readyApp());
      await act(() => watch.tick());
      expect(await screen.findByRole("region", { name: "Ready" })).toBeVisible();
      await waitFor(() => expect(settings.updates).toEqual([hiddenRequest(true)]));

      await act(() => watch.tick());
      await act(() => watch.tick());
      expect(settings.updates).toEqual([hiddenRequest(true)]);
    });

    it("hides Get started when a served use case opens on its ready step, and never when already hidden", async () => {
      const served = {
        applicationDeployments: [readyApp()],
        machines: [{ id: "gpu1", vramGib: 32 }],
        modelDeployments: [modelDeployment("dep-1", "gpu1", 65536n, 23n * gib)],
        routes: [routeView("qwen3.8-27b", ["dep-1"])],
        rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1", "dep-1"]] },
        targets: { "open-webui": [["gpu1", true, "app-new"]] },
      } as Site;

      const visible = await renderPage({ ...served });
      expect(await screen.findByRole("region", { name: "Ready" })).toBeVisible();
      await waitFor(() => expect(visible.settings.updates).toEqual([hiddenRequest(true)]));
      cleanup();

      const hidden = await renderPage({ ...served, getStartedHidden: true });
      expect(await screen.findByRole("region", { name: "Ready" })).toBeVisible();
      await act(() => hidden.watch.tick());
      expect(hidden.settings.updates).toEqual([]);
    });

    it("shows why a ready step could not hide Get started and does not retry", async () => {
      const { settings, watch } = await renderPage({
        applicationDeployments: [readyApp()],
        getStartedHidden: false,
        machines: [{ id: "gpu1", vramGib: 32 }],
        modelDeployments: [modelDeployment("dep-1", "gpu1", 65536n, 23n * gib)],
        routes: [routeView("qwen3.8-27b", ["dep-1"])],
        rows: { [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1", "dep-1"]] },
        targets: { "open-webui": [["gpu1", true, "app-new"]] },
        failSettings: true,
      });

      expect(
        await screen.findByText(/Saving the Get started setting outcome is uncertain/),
      ).toBeVisible();
      await act(() => watch.tick());
      expect(settings.updates).toEqual([hiddenRequest(true)]);
      expect(screen.getByRole("region", { name: "Ready" })).toBeVisible();
    });

    it("writes hidden on Dismiss and goes to the overview", async () => {
      const user = setupUser();
      const { settings } = await renderPage({
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: {},
        targets: {},
      });

      await user.click(screen.getByRole("button", { name: "Dismiss" }));

      expect(settings.updates).toEqual([hiddenRequest(true)]);
      expect(await screen.findByText("Overview")).toBeVisible();
    });

    it("stays on the page with the reason when Dismiss cannot be saved", async () => {
      const user = setupUser();
      const { settings } = await renderPage({
        failSettings: true,
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: {},
        targets: {},
      });

      await user.click(screen.getByRole("button", { name: "Dismiss" }));

      expect(
        await screen.findByText(/Saving the Get started setting outcome is uncertain/),
      ).toBeVisible();
      expect(screen.getByRole("heading", { name: "Get started" })).toBeVisible();
      expect(settings.updates).toEqual([hiddenRequest(true)]);
    });

    it("opens served coding on its ready step and picks the agent whose command to copy", async () => {
      const user = setupUser();
      await renderPage({
        machines: [{ id: "gpu1", vramGib: 32 }],
        modelDeployments: [modelDeployment("dep-1", "gpu1", 131072n, 23n * gib, "Qwen coder")],
        routes: [routeView("qwen-coder", ["dep-1"])],
        rows: {
          [CHAT_ASSISTANT]: [["qwen-27b", "nvfp4", 32768, "gpu1"]],
          [CODING]: [["qwen-27b", "nvfp4", 131072, "gpu1", "dep-1"]],
        },
        targets: { "open-webui": [["gpu1", true]] },
      });
      expect(await screen.findByRole("region", { name: "Model" })).toBeVisible();

      await user.click(screen.getByRole("button", { name: /Coding/ }));

      expect(readyItems(screen.getByRole("region", { name: "Ready" }))).toMatchObject([
        { action: "Chat", kind: "Model", name: "Qwen coder", status: "Ready" },
      ]);
      const agent = screen.getByRole("region", { name: "Coding agent" });
      const agents = within(agent).getByRole("radiogroup", { name: "Coding agent" });
      expect(
        within(agents)
          .getAllByRole("radio")
          .map((radio) => [radio.textContent, radio.getAttribute("aria-checked")]),
      ).toEqual([
        ["OpenCode", "true"],
        ["Pi", "false"],
        ["Codex", "false"],
      ]);
      const shown = () => [...agent.querySelectorAll("code")].map((code) => code.textContent);
      expect(shown()).toEqual([
        "dotc code opencode --model qwen-coder --inference-addr http://192.0.2.10:6208",
      ]);
      await user.click(within(agent).getByRole("button", { name: "Copy OpenCode command" }));
      expect(writeClipboard).toHaveBeenLastCalledWith(
        "dotc code opencode --model qwen-coder --inference-addr http://192.0.2.10:6208",
      );

      await user.click(within(agents).getByRole("radio", { name: "Pi" }));
      expect(shown()).toEqual([
        "dotc code pi --model qwen-coder --inference-addr http://192.0.2.10:6208",
      ]);

      await user.click(within(agents).getByRole("radio", { name: "Codex" }));
      expect(shown()).toEqual([
        "dotc code codex --model qwen-coder --inference-addr http://192.0.2.10:6208",
      ]);
      await user.click(within(agent).getByRole("button", { name: "Copy Codex command" }));
      expect(writeClipboard).toHaveBeenLastCalledWith(
        "dotc code codex --model qwen-coder --inference-addr http://192.0.2.10:6208",
      );
      expect(within(agent).getAllByRole("button")).toHaveLength(1);
    });

    it("opens the image generation app once it is ready", async () => {
      const user = setupUser();
      const { site, watch } = await renderPage({
        machines: [{ id: "gpu1", vramGib: 32 }],
        rows: {},
        targets: { comfyui: [["gpu1", true]] },
      });
      await user.click(screen.getByRole("button", { name: /Image generation/ }));
      await user.click(await screen.findByRole("button", { name: "Deploy" }));
      await screen.findByRole("status", { name: "Deployment progress" });

      site.applicationDeployments!.push(readyApp("ComfyUI", "http://127.0.0.1:8188"));
      await act(() => watch.tick());

      const ready = await screen.findByRole("region", { name: "Ready" });
      expect(readyItems(ready)).toEqual([
        {
          action: "Open",
          details: [["Address", "http://127.0.0.1:8188"]],
          href: "http://127.0.0.1:8188",
          kind: "App",
          name: "ComfyUI",
          status: "Ready",
        },
      ]);
      expect(screen.queryByRole("region", { name: "Coding agent" })).not.toBeInTheDocument();
    });
  });
});

async function renderPage(site: Site) {
  const machines = site.machines.map(({ id, state = NodeObservationState.CONNECTED }) =>
    create(MachineSchema, { displayName: id, machineId: id, observationFreshness: { state } }),
  );
  // Tests change the site's deployments and routes in place, then tick the watch.
  const modelDeployments = (site.modelDeployments ??= []);
  const applicationDeployments = (site.applicationDeployments ??= []);
  const routes = (site.routes ??= []);
  const settings = new FakeConsoleSettings(site.getStartedHidden);
  settings.failing = site.failSettings ?? false;
  const watch = new FakeResourceWatch({
    sources: {
      applicationDeployments: () =>
        Promise.resolve(
          create(ListApplicationDeploymentsResponseSchema, {
            deployments: applicationDeployments,
          }),
        ),
      consoleSettings: () =>
        Promise.resolve(
          create(GetConsoleSettingsResponseSchema, {
            settings: { getStartedHidden: settings.getStartedHidden },
          }),
        ),
      executionTargets: () =>
        Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema, { executionTargets })),
      machines: () => Promise.resolve(create(ListMachinesResponseSchema, { machines })),
      modelDeployments: () =>
        Promise.resolve(
          create(ListModelDeploymentsResponseSchema, { deployments: modelDeployments }),
        ),
      nodeMemory: () =>
        Promise.resolve(
          nodeMemoryPayload(
            site.machines.map(({ id, vramGib }) => {
              const segments: ReturnType<typeof measured>[] = modelDeployments
                .filter((deployment) => deployment.intent?.placement?.siteNodeId === id)
                .map((deployment) => {
                  const result = deployment.acceleratorMemory?.result;
                  const bytes =
                    result?.case === "available" ? result.value.devices[0].usedMemoryBytes : 0n;
                  return measured(deployment.modelDeploymentId, bytes);
                });
              return nodeMemory(id, [
                ...(vramGib === undefined ? [] : [devicePool(BigInt(vramGib) * gib, segments)]),
                hostPool(
                  64n * gib,
                  vramGib === undefined ? [...segments, other(8n * gib)] : [other(8n * gib)],
                ),
              ]);
            }),
          ),
        ),
      routes: () => Promise.resolve(create(ListRoutesResponseSchema, { routes })),
    },
  });
  const executionTargets = site.machines.map(({ id }) => modelTarget(id));

  const createLocalModelDeployment = vi.fn<
    NonNullable<ModelClient["model"]["createLocalModelDeployment"]>
  >(
    site.createModel ??
      (() =>
        Promise.resolve(
          create(CreateModelDeploymentResponseSchema, {
            deployment: { modelDeploymentId: "dep-new" },
          }),
        )),
  );
  const createApplicationDeployment = vi.fn<
    ApplicationClient["application"]["createApplicationDeployment"]
  >(
    site.createApp ??
      (() =>
        Promise.resolve(
          create(CreateApplicationDeploymentResponseSchema, {
            deployment: { applicationDeploymentId: "app-new" },
          }),
        )),
  );
  let failedEstimates = site.failedEstimates ?? 0;
  const logRead = vi.fn().mockResolvedValue({
    entries: [],
    historyState: "no-older-output-observed",
    latestPositions: [],
    responseTruncated: false,
    sources: [],
  });
  const logs = { follow: vi.fn(async function* () {}), read: logRead };
  const modelClient = {
    logs,
    catalog: {
      listEngineReleases: vi.fn(() =>
        Promise.resolve(
          create(ListEngineReleasesResponseSchema, { engineReleases: runtimeSchemaReleases }),
        ),
      ),
      listHardwareClasses: vi.fn(() => Promise.resolve(create(ListHardwareClassesResponseSchema))),
      listModels: vi.fn(() =>
        site.catalogPending
          ? new Promise(() => {})
          : Promise.resolve(
              create(ListModelsResponseSchema, { models: [qwen, runtimeSchemaModel] }),
            ),
      ),
    },
    model: {
      createLocalModelDeployment,
      listModelProfileCompatibility: vi.fn((request: ListModelProfileCompatibilityRequest) => {
        compatibilityReads.push(request);
        if (failedEstimates > 0) {
          failedEstimates -= 1;
          return Promise.reject(new Error("unavailable"));
        }
        return Promise.resolve(
          compatibility(request.modelId ?? "", site, request.scenario?.totalContextTokens),
        );
      }),
      listModelRecommendations: vi.fn((request: ListModelRecommendationsRequest) =>
        Promise.resolve(
          create(ListModelRecommendationsResponseSchema, {
            useCases: request.useCases.map((useCase) =>
              create(UseCaseModelRecommendationsSchema, {
                rows: (site.rows[useCase] ?? []).map(recommendedRow),
                useCase,
              }),
            ),
          }),
        ),
      ),
    },
  } as unknown as ModelClient;
  const applicationClient = {
    logs,
    application: {
      createApplicationDeployment,
      listApplicationTargets: vi.fn((request: ListApplicationTargetsRequest) =>
        Promise.resolve(
          create(ListApplicationTargetsResponseSchema, {
            targets: (site.targets[request.applicationId] ?? []).map(
              ([machine, matchesPreference, applicationDeploymentId]) =>
                create(ApplicationTargetOptionSchema, {
                  applicationDeploymentId,
                  matchesPreference,
                  target: { executionTargetId: "docker", siteNodeId: machine },
                }),
            ),
          }),
        ),
      ),
    },
    catalog: {
      listApplications: vi.fn(() =>
        Promise.resolve(
          create(ListApplicationsResponseSchema, {
            applications: [
              create(ApplicationSchema, {
                applicationId: "open-webui",
                description: "Self-hosted chat interface for the models on this site.",
                displayName: "Open WebUI",
                useCases: [CHAT_ASSISTANT],
                version: "0.11.4",
              }),
              create(ApplicationSchema, {
                applicationId: "comfyui",
                description: "Node-based image and video generation.",
                displayName: "ComfyUI",
                preferredAccelerators: ["nvidia"],
                useCases: [IMAGE_GENERATION],
                version: "0.42.5",
              }),
            ],
          }),
        ),
      ),
    },
  } as unknown as ApplicationClient;

  const openChat = vi.fn();
  const router = createMemoryRouter(
    [
      {
        children: [
          { element: <GetStartedPage />, path: "/get-started" },
          { element: <p>Models</p>, path: "/models" },
          { element: <p>Overview</p>, path: "/overview" },
        ],
        element: (
          <ResourceWatchProvider client={watch.client()}>
            <GetStartedHiddenProvider>
              <ModelsPollingProvider client={modelClient}>
                <ChatOpenerContext.Provider value={openChat}>
                  <Outlet />
                </ChatOpenerContext.Provider>
              </ModelsPollingProvider>
            </GetStartedHiddenProvider>
          </ResourceWatchProvider>
        ),
      },
    ],
    { initialEntries: ["/get-started"] },
  );
  const siteClient = {
    updateConsoleSettings: vi.fn(settings.updateConsoleSettings),
    listConfiguredExecutionTargets: vi.fn(() =>
      Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema, { executionTargets })),
    ),
  };
  renderWithClients(<RouterProvider router={router} />, {
    applicationClient,
    modelClient,
    siteClient,
  });
  await screen.findByRole("button", { name: /Chat assistant/ });
  await screen.findByRole("region", { name: "App" });
  return {
    createApplicationDeployment,
    createLocalModelDeployment,
    logRead,
    openChat,
    settings,
    site,
    siteClient,
    watch,
  };
}

/** A machine's deployable model target, as the proposals name it. */
function modelTarget(machine: string) {
  return create(ConfiguredExecutionTargetSchema, {
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: { availability: { case: "available", value: {} } },
    },
    displayName: "NVIDIA GeForce RTX 5090",
    executionTargetId: "model",
    siteNodeId: machine,
    siteNodeDisplayName: machine,
    workloadKind: ExecutionTargetWorkloadKind.MODEL,
  });
}

const qwen = create(ModelSchema, {
  configurations: [
    { configurationId: "ninfer", engineConfiguration: { engineKind: EngineKind.NINFER } },
    { configurationId: "llama", engineConfiguration: { engineKind: EngineKind.LLAMA_CPP } },
  ],
  displayName: "Qwen3.8 27B",
  modelId: "qwen-27b",
  profiles: [
    { configurationId: "ninfer", displayName: "NVFP4", profileId: "nvfp4" },
    { configurationId: "llama", displayName: "Unsloth Q4_K_XL", profileId: "q4" },
    { configurationId: "llama", displayName: "Unsloth Q5_K_XL", profileId: "q5" },
  ],
});

// Every profile runs on every machine, on a class named after the machine.
// Each total context step of 32k above the first adds 1 GiB to a profile's estimate.
function compatibility(modelId: string, site: Site, totalContext = 32768) {
  const extra = BigInt(Math.max(0, totalContext / 32768 - 1));
  const profiles = Object.keys(estimates).flatMap((key) => {
    const [model, profile] = key.split("/");
    return model === modelId ? [profile] : [];
  });
  return create(ListModelProfileCompatibilityResponseSchema, {
    entries: site.machines.flatMap(({ id }) =>
      profiles.map((profileId) => ({
        hardwareClassId: `class-${id}`,
        modelId,
        profileId,
        status: ModelCompatibilityStatus.RUNS,
        target: { executionTargetId: "model", siteNodeId: id },
      })),
    ),
    profiles: profiles.map((profileId) => ({
      hardwareClassEstimates: site.machines.map(({ id }) => ({
        estimates: [
          {
            resource: "acceleratorMemory",
            unit: "bytes",
            value: (BigInt(estimates[`${modelId}/${profileId}`]) + extra) * gib,
          },
        ],
        hardwareClassId: `class-${id}`,
      })),
      modelId,
      profileId,
    })),
    targets: site.machines.map(({ id }) => ({
      status: ModelCompatibilityStatus.RUNS,
      target: { executionTargetId: "model", siteNodeId: id },
    })),
  });
}

function recommendedRow([modelId, profileId, context, machine, deploymentId]: Row) {
  return create(RecommendedModelProfileSchema, {
    contextTokensPerRequest: BigInt(context),
    modelId,
    profileId,
    targets: [
      {
        fit: ModelRecommendationFit.FITS_NOW,
        modelDeploymentId: deploymentId,
        target: { executionTargetId: "model", siteNodeId: machine },
      },
    ],
  });
}

function modelDeployment(
  id: string,
  machine: string,
  contextPerRequest: bigint,
  vramBytes: bigint,
  name?: string,
) {
  return create(ModelDeploymentSchema, {
    acceleratorMemory: {
      result: { case: "available", value: { devices: [{ usedMemoryBytes: vramBytes }] } },
    },
    desiredState: ModelDeploymentDesiredState.PRESENT,
    intent: { placement: { executionTargetId: "model", siteNodeId: machine } },
    modelDeploymentId: id,
    modelDeploymentName: name,
    snapshot: {
      source: {
        case: "local",
        value: {
          engineParameterValues: {
            values: [
              {
                parameterId: "context-per-request",
                value: { case: "unsignedInteger", value: contextPerRequest },
              },
            ],
          },
          modelDisplayName: "Qwen3.8 27B",
          modelId: "qwen-27b",
          profileId: "nvfp4",
        },
      },
    },
    state: { status: ModelDeploymentStatus.READY },
  });
}

function applicationDeployment(id: string) {
  return create(ApplicationDeploymentSchema, {
    applicationDeploymentId: id,
    desiredState: ApplicationDeploymentDesiredState.PRESENT,
    state: {
      lifecycleState: ApplicationLifecycleState.READY,
      readinessState: ApplicationReadinessState.READY,
    },
  });
}

function routeView(name: string, members: string[]) {
  return create(InferenceRouteViewSchema, {
    availability: { available: true },
    route: { enabled: true, fallbackGroups: [{ modelDeploymentIds: members }], name },
  });
}

// A deployment the create calls returned, at one step of its start.
function startingModel(status: ModelDeploymentStatus, step: number, percent?: number) {
  const deployment = modelDeployment("dep-new", "gpu1", 32768n, 0n, "Qwen3.8 27B");
  deployment.state = create(ModelDeploymentStateSchema, {
    progress:
      percent === undefined
        ? undefined
        : {
            completed: gib,
            percent,
            total: 4n * gib,
            unit: ModelDeploymentProgressUnit.BYTES,
          },
    status,
    steps: {
      current: step,
      steps: [
        ModelDeploymentStep.CHECKING_ARTIFACTS,
        ModelDeploymentStep.DOWNLOADING_ARTIFACTS,
        ModelDeploymentStep.PREPARING_ENGINE,
        ModelDeploymentStep.STARTING,
        ModelDeploymentStep.LOADING_MODEL,
      ],
    },
  });
  return deployment;
}

function startingApp(lifecycleState: ApplicationLifecycleState, step: number) {
  const deployment = readyApp();
  deployment.state = create(ApplicationDeploymentStateSchema, {
    lifecycleState,
    readinessState: ApplicationReadinessState.NOT_READY,
    steps: {
      current: step,
      steps: [
        ApplicationDeploymentStep.PREPARING_IMAGE,
        ApplicationDeploymentStep.STARTING,
        ApplicationDeploymentStep.WAITING_FOR_ENDPOINT,
      ],
    },
  });
  return deployment;
}

function readyApp(name = "Open WebUI", url = "http://127.0.0.1:3000") {
  const deployment = applicationDeployment("app-new");
  deployment.applicationDeploymentName = name;
  deployment.state!.endpoint = create(ApplicationEndpointSchema, { url });
  return deployment;
}

// The deploying step's items: what each deployment shows and how far its steps are.
function progressItems() {
  const card = screen.getByRole("status", { name: "Deployment progress" });
  return within(card)
    .getAllByRole("region")
    .map((item) => ({
      kind: item.getAttribute("aria-label"),
      name: item.querySelector("span.font-semibold")?.textContent,
      status: item.querySelector("[data-slot=badge]")?.textContent,
      steps: within(item).queryByRole("progressbar")?.getAttribute("aria-valuetext"),
    }));
}

// The ready step's items: their status, action and facts.
function readyItems(card: HTMLElement) {
  return within(card)
    .getAllByRole("region")
    .map((item) => {
      const link = within(item).queryByRole("link");
      return {
        action: (within(item).queryByRole("button") ?? link)?.textContent,
        details: detailsOf(item.getAttribute("aria-label")!),
        ...(link ? { href: link.getAttribute("href") } : {}),
        kind: item.getAttribute("aria-label"),
        name: item.querySelector("span.font-semibold")?.textContent,
        status: item.querySelector("[data-slot=badge]")?.textContent,
      };
    });
}

function useCaseCards() {
  return within(screen.getByRole("group", { name: "Use case" }))
    .getAllByRole("button")
    .map((card) => ({
      label: card.querySelector("span")?.firstChild?.nextSibling?.textContent,
      pressed: card.getAttribute("aria-pressed") === "true",
    }));
}

function modelItem() {
  const item = screen.getByRole("region", { name: "Model" });
  const head = item.children[1];
  const toggle = within(item).queryByRole("button", { name: /other choices/i });
  return {
    choices: toggle?.textContent ?? undefined,
    customize: within(head as HTMLElement).queryByRole("button", { name: "Customize" }) !== null,
    details: detailsOf("Model"),
    head: headParts(head),
    notice: item.querySelector("[data-slot=notice]")?.textContent ?? undefined,
  };
}

function appItem() {
  const item = screen.getByRole("region", { name: "App" });
  return {
    description: item.querySelector("p")?.textContent ?? undefined,
    head: headParts(item.children[1]),
    notice: item.querySelector("[data-slot=notice]")?.textContent ?? undefined,
  };
}

// The item's head row as its visible parts: the name, its tags and its status, without buttons.
function headParts(head: Element) {
  return [...head.children]
    .filter((part) => !part.querySelector("button") && part.tagName !== "BUTTON")
    .map((part) => part.textContent);
}

function detailsOf(kind: string) {
  const item = screen.getByRole("region", { name: kind });
  const terms = [...item.querySelectorAll("dt")];
  return terms.map((term) => [term.textContent, term.nextElementSibling?.textContent]);
}

function primaryAction() {
  const button = screen.getByRole("button", { name: /^(Deploy|Continue)$/ });
  return { disabled: button.hasAttribute("disabled"), name: button.textContent };
}

function machineRows(card: HTMLElement) {
  return within(card)
    .getAllByRole("group")
    .map((machine) => ({
      name: machine.getAttribute("aria-label"),
      rows: [...machine.querySelectorAll("[data-slot=metric-row-label]")].map(
        (label) => label.textContent,
      ),
      status: machine.firstElementChild?.lastElementChild?.textContent,
    }));
}

function unsigned(parameterId: string, value: bigint) {
  return { parameterId, value: { case: "unsignedInteger" as const, value } };
}

// The context values of a launch's engine parameters, by parameter.
function contextValues(
  values: readonly { parameterId: string; value: { case?: string; value?: unknown } }[],
) {
  return Object.fromEntries(
    values
      .filter(
        ({ parameterId }) =>
          parameterId === "context-per-request" || parameterId === "total-context",
      )
      .map(({ parameterId, value }) => [parameterId, value.value]),
  );
}

function summaryTarget(dialog: HTMLElement) {
  const summary = within(dialog).getByRole("region", { name: "Summary" });
  const target = within(summary).getByText("Target");
  return target.nextElementSibling?.textContent;
}
