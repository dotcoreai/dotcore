import { clone, create } from "@bufbuild/protobuf";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { type ComponentProps } from "react";
import { describe, expect, it, vi } from "vitest";

import {
  ExecutionTargetsWatch,
  executionTargetsWatch,
} from "@/features/deployments/execution-targets-watch-test-support";
import {
  compatibilityLoader,
  targetCompatibility,
} from "@/features/models/profiles/profile-compatibility-test-support";
import {
  modelWithKvCachePrecision,
  modelWithStrata,
  modelWithVision,
  runtimeSchemaModel,
  runtimeSchemaReleases,
  strataRelease,
  unsignedValue,
} from "@/features/models/runtime/runtime-schema-test-support";
import {
  ModelConfigurationOptionSchema,
  ModelOptionSchema,
  ModelOptionValuesSchema,
  ModelSchema,
  SpeculativeDecodingMethod,
} from "@/gen/dotcore/catalog/v1/model_pb";
import { ParameterValuesSchema, ParameterVisibility } from "@/gen/dotcore/catalog/v1/parameter_pb";
import { AcceleratorMemoryArchitecture } from "@/gen/dotcore/observation/v1/observation_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { readFieldHelpFacts } from "@/test/field-help";
import { setupUser } from "@/test/user";

import { ModelDeployDialog } from "./model-deploy-dialog";
import type { LocalDeploymentValues } from "./model-deploy-launch";
import { goToStep, openAdvancedStep } from "./model-deploy-test-steps";
import { sharedContextEstimate } from "./resource-estimate-test-support";

const poolPlacement = { executionTargetId: "pool", siteNodeId: "node" };

describe("ModelDeployDialog Settings step", () => {
  it("asks parallel requests, context per request and total context in order, in both modes, with short help", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await screen.findByText("Step 1 of 3 — Model & machine");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 2 of 3 — Context")).toBeVisible();
    // The total context starts at the context per request times the parallel requests.
    expect(textboxValues()).toEqual([
      ["engine-parameter-parallel-requests", "4"],
      ["engine-parameter-context-per-request", "32k"],
      ["engine-parameter-total-context", "128k"],
    ]);
    expect(
      screen.getByRole("textbox", { name: "Context per request" }),
    ).toHaveAccessibleDescription(
      "Largest context one request may use from the total context. It reserves nothing: requests take memory from the total context as they grow. A new request reuses the start of an earlier prompt from an idle request or the prompt cache, but not from requests running at the same time.",
    );
    expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveAccessibleDescription(
      "Requests the server generates at the same time; further requests wait in a queue. All of them share the total context, so each added request only adds its own small state.",
    );
    expect(await readFieldHelpFacts("Context per request")).toContain("Up to 256k (model maximum)");
    expect(screen.queryByRole("button", { name: /^Follow/ })).not.toBeInTheDocument();

    await user.click(preset("64k", "Context per request"));
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("256k");
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(screen.getByText("Step 2 of 5 — Context")).toBeVisible();
    expect(textboxValues()).toEqual([
      ["engine-parameter-parallel-requests", "4"],
      ["engine-parameter-context-per-request", "64k"],
      ["engine-parameter-total-context", "256k"],
    ]);
  });

  it("edits the llama.cpp context with a log slider from the declared minimum, an input and presets", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await openAdvancedSettings(user);
    const slider = screen.getByRole("slider", { name: "Total context" });
    const input = screen.getByRole("textbox", { name: "Total context" });
    expect(input).toHaveValue("128k");
    // The total ends at the 256k model maximum for each of the 4 parallel requests, so 131072 tokens
    // sits eight elevenths along the log scale from the declared 512 tokens to 1048576 tokens.
    expect(slider).toHaveValue("727");
    expect(slider).toHaveAttribute("aria-valuetext", "131,072 tokens");
    expect(presetLabels()).toEqual(["8k", "32k", "64k", "128k", "256k", "1M"]);
    // A chip is pressed only right after it is clicked, not for a value that equals it.
    expect(preset("128k")).toHaveAttribute("aria-pressed", "false");

    await user.click(preset("64k"));
    expect(input).toHaveValue("64k");
    expect(slider).toHaveValue("636");
    expect(preset("64k")).toHaveAttribute("aria-pressed", "true");
    // A drag releases the chip, even back onto its own size.
    fireEvent.change(slider, { target: { value: "700" } });
    fireEvent.change(slider, { target: { value: "636" } });
    expect(input).toHaveValue("64k");
    expect(preset("64k")).toHaveAttribute("aria-pressed", "false");

    fireEvent.change(slider, { target: { value: "1000" } });
    expect(input).toHaveValue("1M");
    fireEvent.change(slider, { target: { value: "0" } });
    expect(input).toHaveValue("512");
    expect(await readFieldHelpFacts("Total context")).toContain("Range 512–2,147,483,647 tokens");

    // The number input takes any value the schema accepts, past the slider's end.
    await user.clear(input);
    await user.type(input, "20000000");
    await user.tab();
    expect(input).not.toHaveAttribute("aria-invalid");
    expect(slider).toHaveValue("1000");

    await user.clear(input);
    await user.type(input, "3000000000");
    await user.tab();
    const error = "Total context must be an integer from 512 to 2,147,483,647 tokens.";
    expect(await screen.findByText(error)).toBeVisible();
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input).toHaveAccessibleDescription(
      `Context memory the server holds from the start and shares among the requests it generates at once. Each active request stores its own tokens in it, even when requests start with the same prompt. When it fills, the server frees idle requests first, then fails the active ones. ${error}`,
    );
  });

  it("shows the vLLM KV offload size in its host memory group and submits it with the hidden defaults", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    renderDialog({ initialProfileId: "nvfp4-text", onSubmit });

    await openAdvancedSettings(user);
    expect(presetLabels()).toEqual(["8k", "32k", "64k", "128k", "256k", "1M"]);
    await user.click(preset("64k"));
    await user.click(screen.getByRole("button", { name: "Next" }));
    // The group has one member, so it shows no heading.
    expect(screen.queryByRole("group", { name: "Host memory" })).toBeNull();
    const kvOffload = screen.getByRole("textbox", { name: "KV offloading size" });
    expect(kvOffload).toHaveAccessibleDescription(
      "Host memory used to offload the KV cache, in GiB.",
    );
    expect(kvOffload).toHaveValue("0 GiB");
    // Parameters the schema does not expose stay out of the form.
    expect(screen.queryByRole("textbox", { name: "Maximum batched tokens" })).toBeNull();

    await user.clear(kvOffload);
    await user.type(kvOffload, "512m");
    await user.tab();
    expect(
      await screen.findByText("KV offloading size must be a whole number of GiB."),
    ).toBeVisible();
    expect(kvOffload).toHaveAttribute("aria-invalid", "true");
    expect(await readFieldHelpFacts("KV offloading size")).toContain("Range 0 or more GiB");

    // Without a declared maximum, any larger size is accepted, typed in any byte unit.
    await user.clear(kvOffload);
    await user.type(kvOffload, "2t");
    expect(screen.queryByText(/must be/)).not.toBeInTheDocument();
    await deployFromAdvanced(user);
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(submittedValues(onSubmit)).toEqual([
      ["maximum-batched-tokens", 4096n],
      ["maximum-cudagraph-capture-size", 32n],
      ["kv-offloading-size-gib", 2048n],
      ["context-per-request", 32768n],
      ["parallel-requests", 4n],
      ["total-context", 65536n],
    ]);
  });

  describe.each([
    {
      engine: "llama.cpp",
      profileId: "q4-xl",
      defaults: [
        ["prompt-cache-memory-mib", 8192n],
        ["context-per-request", 32768n],
        ["parallel-requests", 4n],
      ],
    },
    {
      engine: "vLLM",
      profileId: "nvfp4-text",
      defaults: [
        ["maximum-batched-tokens", 4096n],
        ["maximum-cudagraph-capture-size", 32n],
        ["kv-offloading-size-gib", 0n],
        ["context-per-request", 32768n],
        ["parallel-requests", 4n],
      ],
    },
    {
      engine: "ninfer",
      profileId: "ninfer-nvfp4",
      defaults: [
        ["parallel-requests", 4n],
        ["host-context-mib", 20480n],
        ["context-per-request", 32768n],
      ],
    },
  ])("$engine context group", ({ defaults, profileId }) => {
    it("shows the context inputs at the engine defaults and submits them with the following total", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: profileId, onSubmit });

      await openAdvancedSettings(user);
      expect(textboxValues()).toEqual([
        ["engine-parameter-parallel-requests", "4"],
        ["engine-parameter-context-per-request", "32k"],
        ["engine-parameter-total-context", "128k"],
      ]);
      expect(screen.getByRole("slider", { name: "Context per request" })).toHaveAttribute(
        "aria-valuetext",
        "32,768 tokens",
      );

      await deployFromSettings(user);
      expect(screen.queryByText(/Customized/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedValues(onSubmit)).toEqual([...defaults, ["total-context", 131072n]]);
    });

    it("follows the context per request and parallel requests with the total until it is edited, and relinks it", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: profileId, onSubmit });

      await openAdvancedSettings(user);
      const total = screen.getByRole("textbox", { name: "Total context" });
      const perRequest = screen.getByRole("textbox", { name: "Context per request" });
      const parallel = screen.getByRole("textbox", { name: "Parallel requests" });

      // A typed context per request moves the total.
      await user.clear(perRequest);
      await user.type(perRequest, "64k");
      await user.tab();
      expect(total).toHaveValue("256k");
      await user.clear(parallel);
      await user.type(parallel, "2");
      await user.tab();
      expect(total).toHaveValue("128k");

      // An edited total stays put, and the link makes it follow again.
      await user.click(preset("32k"));
      expect(screen.getByRole("button", { name: "Follow per request × parallel" })).toBeVisible();
      await user.clear(parallel);
      await user.type(parallel, "4");
      await user.tab();
      expect(total).toHaveValue("32k");
      // Above the edited total, the context per request can't start.
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(
        screen.getByText("Context per request must be at most the total context."),
      ).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Follow per request × parallel" }));
      expect(total).toHaveValue("256k");
      expect(screen.queryByRole("button", { name: /^Follow/ })).not.toBeInTheDocument();

      await deployFromSettings(user);
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(Object.fromEntries(submittedValues(onSubmit))).toMatchObject({
        "context-per-request": 65536n,
        "parallel-requests": 4n,
        "total-context": 262144n,
      });
    });
  });

  it("draws the ninfer context sliders on one scale, stopping context per request at the model maximum", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "ninfer-nvfp4" });

    await openAdvancedSettings(user);
    const totalSlider = screen.getByRole("slider", { name: "Total context" });
    const perRequestSlider = screen.getByRole("slider", { name: "Context per request" });
    // The scale runs from 512 tokens to the total's end, the 256k model maximum for each of the 4
    // parallel requests: 32768 tokens sits six elevenths along, and 131072 tokens eight.
    expect(perRequestSlider).toHaveValue("545");
    expect(totalSlider).toHaveValue("727");

    // The end of the shared scale clamps context per request to the 262144 model maximum, below
    // ninfer's own, nine elevenths along, and the total follows it for 4 parallel requests.
    fireEvent.change(perRequestSlider, { target: { value: "1000" } });
    expect(screen.getByRole("textbox", { name: "Context per request" })).toHaveValue("256k");
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("1M");
    expect(perRequestSlider).toHaveValue("818");
    expect(totalSlider).toHaveValue("1000");
    expect(presetLabels("Context per request")).toEqual(["8k", "32k", "64k", "128k", "256k"]);
  });

  it("stops context per request at the llama.cpp maximum when the model allows more", async () => {
    const user = setupUser();
    const model = clone(ModelSchema, runtimeSchemaModel);
    model.maximumContextTokens = 4_294_967_295;
    renderDialog({ initialProfileId: "q4-xl", model });

    await openAdvancedSettings(user);
    fireEvent.change(screen.getByRole("slider", { name: "Context per request" }), {
      target: { value: "1000" },
    });
    expect(screen.getByRole("textbox", { name: "Context per request" })).toHaveValue("2147483647");
    // The total's end, 4 requests at that context, stops at llama.cpp's total maximum too.
    expect(screen.getByRole("slider", { name: "Total context" })).toHaveValue("1000");
  });

  it("ends vLLM context per request at the model maximum and the total at it per parallel request", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "nvfp4-text" });

    await openAdvancedSettings(user);
    const totalSlider = screen.getByRole("slider", { name: "Total context" });
    const perRequestSlider = screen.getByRole("slider", { name: "Context per request" });
    expect(await readFieldHelpFacts("Context per request")).toContain("Range 512 or more tokens");
    expect(await readFieldHelpFacts("Total context")).toContain("Range 512 or more tokens");
    // Without an engine maximum, context per request ends at the 256k model maximum and the total
    // at that for each of the 4 parallel requests.
    expect(totalSlider).toHaveValue("727");
    expect(presetLabels()).toEqual(["8k", "32k", "64k", "128k", "256k", "1M"]);
    expect(presetLabels("Context per request")).toEqual(["8k", "32k", "64k", "128k", "256k"]);
    fireEvent.change(perRequestSlider, { target: { value: "1000" } });
    expect(screen.getByRole("textbox", { name: "Context per request" })).toHaveValue("256k");

    // One parallel request brings the total's end down to the context per request's.
    const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
    await user.clear(parallel);
    await user.type(parallel, "1");
    await user.tab();
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("256k");
    expect(totalSlider).toHaveValue("1000");
    expect(presetLabels()).toEqual(["8k", "32k", "64k", "128k", "256k"]);
  });

  it("steps parallel requests with the stepper, and the following total context follows", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await openAdvancedSettings(user);
    const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
    const total = screen.getByRole("textbox", { name: "Total context" });
    expect(parallel).toHaveValue("4");
    expect(total).toHaveValue("128k");

    await user.click(screen.getByRole("button", { name: "Increase Parallel requests" }));
    expect(parallel).toHaveValue("5");
    expect(total).toHaveValue("160k");
    await user.click(screen.getByRole("button", { name: "Decrease Parallel requests" }));
    await user.click(screen.getByRole("button", { name: "Decrease Parallel requests" }));
    expect(parallel).toHaveValue("3");
    expect(total).toHaveValue("96k");
  });

  it("flags an invalid parallel requests value and focuses it on a blocked Next", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await openAdvancedSettings(user);
    const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
    await user.clear(parallel);
    await user.type(parallel, "x");
    await user.click(screen.getByRole("button", { name: "Next" }));

    expect(parallel).toBeInvalid();
    expect(parallel).toHaveFocus();
    expect(screen.getByRole("button", { name: "Decrease Parallel requests" })).toBeDisabled();
  });

  it("caps the context per request at the configuration's maximum context", async () => {
    const user = setupUser();
    const model = clone(ModelSchema, runtimeSchemaModel);
    model.maximumContextTokens = 262144;
    model.configurations[0].maximumContextTokens = 16384;
    renderDialog({ initialProfileId: "q4-xl", model });

    await openAdvancedSettings(user);
    expect(await readFieldHelpFacts("Context per request")).toContain("Up to 16k (model maximum)");
    expect(presetLabels("Context per request")).toEqual(["8k"]);
  });

  it("shows no total context on a profile that hides it, and submits none for resolution to derive", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    renderStrataDialog(
      {},
      {
        engineParameterVisibilityOverrides: [
          { parameterId: "total-context", visibility: ParameterVisibility.HIDDEN },
        ],
      },
      onSubmit,
    );

    await openAdvancedSettings(user);
    expect(textboxValues()).toEqual([
      ["engine-parameter-parallel-requests", "1"],
      ["engine-parameter-context-per-request", "32k"],
    ]);
    expect(screen.queryByRole("textbox", { name: "Total context" })).not.toBeInTheDocument();

    await deployFromSettings(user);
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(submittedValues(onSubmit).map(([parameterId]) => parameterId)).not.toContain(
      "total-context",
    );
  });
});

describe("ModelDeployDialog Advanced step", () => {
  it("shows the ninfer host context in its host memory group", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "ninfer-nvfp4" });

    await openAdvancedStep(user);
    expect(screen.queryByRole("group", { name: "Host memory" })).toBeNull();
    const hostContext = screen.getByRole("textbox", { name: "Host context memory" });
    expect(hostContext).toHaveValue("20 GiB");
    expect(hostContext).toHaveAccessibleDescription(
      "Host memory where idle conversations and paused requests are kept, so they resume without processing their prompt again. 0 turns it off.",
    );

    // The context parameters sit on the Settings step.
    for (const name of ["Total context", "Context per request", "Parallel requests"])
      expect(screen.queryByRole("textbox", { name })).not.toBeInTheDocument();
  });

  it("keeps the llama.cpp prompt cache at the top level, ahead of the speculative decoding choice", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await openAdvancedStep(user);
    // llama.cpp declares no parameter group, so its prompt cache leads.
    expect(screen.getAllByRole("textbox").map((input) => input.id)).toEqual([
      "engine-parameter-prompt-cache-memory-mib",
      "speculative-decoding-count",
    ]);
    expect(screen.queryAllByRole("group")).toEqual([]);
    // The profile decides the weights, so no step offers them.
    expect(screen.queryByRole("combobox", { name: "Model artifact" })).not.toBeInTheDocument();
  });

  it("shows the Strata expert memory group open, with its help", async () => {
    const user = setupUser();
    renderStrataDialog();

    await openAdvancedStep(user);
    expect(screen.getAllByRole("textbox").map((input) => input.id)).toEqual([
      "engine-parameter-expert-vram-mib",
      "engine-parameter-expert-host-memory-mib",
    ]);
    const group = screen.getByRole("group", { name: "Expert memory" });
    expect(within(group).queryByRole("button", { name: "Expert memory" })).toBeNull();
    expect(within(group).getByText("Expert memory")).toHaveAccessibleDescription(
      "Where Strata keeps the model's expert weights.",
    );
    expect(within(group).getByRole("textbox", { name: "Expert host memory" })).toHaveValue(
      "16 GiB",
    );
  });

  it("slides Strata's expert accelerator memory up to the estimate's fit and submits typed sizes", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const model = modelWithStrata();
    renderDialog({
      engineReleases: [...runtimeSchemaReleases, strataRelease],
      initialProfileId: "strata",
      model,
      onEstimate: async (values) => ({
        ...(await sharedContextEstimate(model)(values)),
        memoryParameterSizes: {
          fits: new Map([["expert-vram-mib", 12595]]),
          sized: new Map([["expert-vram-mib", 12288]]),
        },
      }),
      onSubmit,
    });

    await openAdvancedStep(user);
    // Without a fit, a memory parameter keeps the box alone.
    expect(screen.getByRole("textbox", { name: "Expert host memory" })).toHaveValue("16 GiB");
    expect(screen.queryByRole("slider", { name: "Expert host memory" })).not.toBeInTheDocument();

    // Emptied, the box shows the value sizing picks, and the thumb sits there: 12288 MiB along the
    // slider from the declared 0 to the 12595 MiB fit.
    const box = screen.getByRole("textbox", { name: "Expert accelerator memory" });
    expect(box).toHaveValue("8 GiB");
    await user.clear(box);
    expect(box).toHaveAttribute("placeholder", "12 GiB");
    const slider = await screen.findByRole("slider", { name: "Expert accelerator memory" });
    expect(slider).toHaveValue("976");
    expect(presetLabels("Expert accelerator memory")).toEqual([
      "1 GiB",
      "2 GiB",
      "4 GiB",
      "8 GiB",
      "12.3 GiB",
    ]);

    // A size past the fit is accepted: the thumb stays at the end and the box keeps it.
    await user.type(box, "20g");
    await user.tab();
    expect(box).toHaveValue("20 GiB");
    expect(slider).toHaveValue("1000");
    expect(box).not.toHaveAttribute("aria-invalid");

    await user.clear(box);
    await user.type(box, "6g");
    await user.tab();
    expect(box).toHaveValue("6 GiB");
    await deployFromAdvanced(user);
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(submittedValues(onSubmit)).toContainEqual(["expert-vram-mib", 6144n]);
  });

  it("shows a one-member group's field in each step by the profile's visibility, with no heading", async () => {
    const user = setupUser();
    renderStrataDialog(
      {},
      {
        engineParameterVisibilityOverrides: [
          { parameterId: "expert-vram-mib", visibility: ParameterVisibility.BASIC },
        ],
      },
    );

    await openAdvancedSettings(user);
    expect(screen.queryByRole("group", { name: "Expert memory" })).toBeNull();
    expect(
      screen
        .getAllByRole("textbox")
        .map((input) => input.id)
        .filter((id) => id.startsWith("engine-parameter-expert")),
    ).toEqual(["engine-parameter-expert-vram-mib"]);

    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.queryByRole("group", { name: "Expert memory" })).toBeNull();
    expect(
      screen
        .getAllByRole("textbox")
        .map((input) => input.id)
        .filter((id) => id.startsWith("engine-parameter-expert")),
    ).toEqual(["engine-parameter-expert-host-memory-mib"]);
  });

  it("shows no box for a group whose members the configuration excludes", async () => {
    const user = setupUser();
    renderStrataDialog({
      excludedEngineParameterIds: ["expert-vram-mib", "expert-host-memory-mib"],
    });

    await openAdvancedStep(user, "Context");
    expect(screen.queryByRole("group", { name: "Expert memory" })).not.toBeInTheDocument();
    // With no advanced input left, Performance & memory doesn't show.
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 3 of 4 — Command")).toBeVisible();
  });

  it("previews once per configuration when the configuration excludes parameters", async () => {
    const user = setupUser();
    const onPreviewMemory = vi.fn().mockResolvedValue({
      acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture.UNSPECIFIED,
      advisories: [],
    });
    const model = modelWithStrata({ excludedEngineParameterIds: ["expert-host-memory-mib"] });
    renderDialog({
      deployments: [],
      engineReleases: [...runtimeSchemaReleases, strataRelease],
      initialProfileId: "strata",
      model,
      onPreviewMemory,
    });

    await openAdvancedStep(user);
    await waitFor(() => expect(onPreviewMemory).toHaveBeenCalledOnce());
    const expertMemory = screen.getByRole("textbox", { name: "Expert accelerator memory" });
    await user.clear(expertMemory);
    await user.type(expertMemory, "8");
    await user.tab();
    await waitFor(() => expect(onPreviewMemory).toHaveBeenCalledTimes(2));
    // Settled: the predicted memory shows the last estimate, and no preview follows it.
    await screen.findByRole("region", { name: "Predicted memory" });
    expect(onPreviewMemory).toHaveBeenCalledTimes(2);
  });

  it("focuses a group's invalid value on a blocked Next", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    renderStrataDialog({}, {}, onSubmit);
    const error = "Expert host memory must be a whole number of MiB.";

    await openAdvancedStep(user);
    const hostMemory = screen.getByRole("textbox", { name: "Expert host memory" });
    await user.clear(hostMemory);
    await user.type(hostMemory, "1.5");

    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 3 of 5 — Performance & memory")).toBeVisible();
    await waitFor(() =>
      expect(screen.getByRole("textbox", { name: "Expert host memory" })).toHaveFocus(),
    );
    expect(screen.getByText(error)).toBeVisible();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  describe("KV cache precision", () => {
    it("is hidden when the configuration does not declare the option", async () => {
      const user = setupUser();
      renderDialog({ initialProfileId: "q4-xl" });

      await openAdvancedStep(user);
      expect(screen.queryByRole("combobox", { name: "KV cache precision" })).toBeNull();
    });

    it("starts from the configuration default and sends no overrides when untouched", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: "q4-xl", model: modelWithKvCachePrecision(), onSubmit });

      await openAdvancedStep(user);
      expect(kvCachePrecision()).toHaveValue("f16");
      expect(optionLabels(kvCachePrecision())).toEqual(["f16", "q8_0"]);

      await deployFromAdvanced(user);
      expect(screen.queryByText(/Customized/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual([]);
    });

    it("starts from the profile default and sends the other choice as an override", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({
        initialProfileId: "q4-xl",
        model: modelWithKvCachePrecision("q8_0"),
        onSubmit,
      });

      await openAdvancedStep(user);
      expect(kvCachePrecision()).toHaveValue("q8_0");

      await user.selectOptions(kvCachePrecision(), "f16");
      await deployFromAdvanced(user);
      expect(screen.getByText("Customized: KV cache precision f16")).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual([["kv-cache", "precision", "f16"]]);
    });

    it("refreshes the estimate with the chosen precision beside speculative overrides", async () => {
      const user = setupUser();
      const onEstimate = vi.fn(() => Promise.resolve({}));
      renderDialog({
        initialProfileId: "q4-xl",
        model: modelWithKvCachePrecision(),
        onEstimate,
      });

      await openAdvancedStep(user);
      await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
      expect(estimatedOverrides(onEstimate, 0)).toEqual([]);

      await user.selectOptions(kvCachePrecision(), "q8_0");
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(2));
      expect(estimatedOverrides(onEstimate, 1)).toEqual([["kv-cache", "precision", "q8_0"]]);

      await user.selectOptions(speculativeMethod(), "Off");
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(3));
      expect(estimatedOverrides(onEstimate, 2)).toEqual([
        ["mtp", "token-count", 0n],
        ["kv-cache", "precision", "q8_0"],
      ]);
    });

    it("sends the chosen precision with the command preview", async () => {
      const user = setupUser();
      const onPreview = vi.fn(() => new Promise<never>(() => undefined));
      renderDialog({ initialProfileId: "q4-xl", model: modelWithKvCachePrecision(), onPreview });

      await openAdvancedStep(user);
      await user.selectOptions(kvCachePrecision(), "q8_0");
      // The Command step previews the command as it opens.
      await user.click(screen.getByRole("button", { name: "Next" }));

      await waitFor(() => expect(onPreview).toHaveBeenCalledOnce());
      expect(estimatedOverrides(onPreview as unknown as ReturnType<typeof vi.fn>, 0)).toEqual([
        ["kv-cache", "precision", "q8_0"],
      ]);
    });
  });

  describe("speculative decoding", () => {
    it("starts from the profile default and sends no option overrides when untouched", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: "q4-xl", onSubmit });

      await openAdvancedStep(user);
      expect(speculativeMethod()).toHaveValue("mtp");
      expect(optionLabels(speculativeMethod())).toEqual(["Off", "MTP"]);
      expect(speculativeCount()).toHaveValue("3");
      expect(speculativeCount()).toHaveAccessibleDescription("Draft tokens per step.");
      await user.click(screen.getByRole("button", { name: "About Speculative decoding" }));
      expect(
        await screen.findByRole("dialog", { name: "About Speculative decoding" }),
      ).toHaveTextContent("Range 1–5 tokens");
      await user.keyboard("{Escape}");

      await deployFromAdvanced(user);
      expect(screen.queryByText(/Customized/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual([]);
    });

    it.each([
      {
        choose: async (user: UserEvent) => user.selectOptions(speculativeMethod(), "Off"),
        customized: "Customized: Speculative decoding off",
        expected: [["mtp", "token-count", 0n]],
        label: "Off",
      },
      {
        choose: async (user: UserEvent) => {
          await user.clear(speculativeCount());
          await user.type(speculativeCount(), "5");
        },
        customized: "Customized: Speculative decoding MTP, 5 tokens",
        expected: [["mtp", "token-count", 5n]],
        label: "5 tokens",
      },
    ])("sends the token count override for $label", async ({ choose, customized, expected }) => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: "q4-xl", onSubmit });

      await openAdvancedStep(user);
      await choose(user);
      await deployFromAdvanced(user);
      expect(screen.getByText(customized)).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual(expected);
    });

    it("refreshes the estimate with the chosen overrides", async () => {
      const user = setupUser();
      const onEstimate = vi.fn(() => Promise.resolve({}));
      renderDialog({ initialProfileId: "q4-xl", onEstimate });

      await openAdvancedStep(user);
      await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
      expect(estimatedOverrides(onEstimate, 0)).toEqual([]);

      await user.clear(speculativeCount());
      // An empty count is invalid, so no estimate is asked for it.
      expect(onEstimate).toHaveBeenCalledOnce();
      await user.type(speculativeCount(), "5");
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(2));
      expect(estimatedOverrides(onEstimate, 1)).toEqual([["mtp", "token-count", 5n]]);

      await user.selectOptions(speculativeMethod(), "Off");
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(3));
      expect(estimatedOverrides(onEstimate, 2)).toEqual([["mtp", "token-count", 0n]]);
      expect(screen.queryByRole("textbox", { name: "Speculative token count" })).toBeNull();
    });

    it("estimates on the selected target", async () => {
      const user = setupUser();
      const onEstimate = vi.fn(() => Promise.resolve({}));
      renderDialog({ initialProfileId: "q4-xl", onEstimate });

      await openAdvancedStep(user);
      await waitFor(() => expect(onEstimate).toHaveBeenCalled());
      const [values] = onEstimate.mock.lastCall as unknown as [
        { placement?: { executionTargetId: string; siteNodeId: string } },
      ];
      expect(values.placement).toMatchObject({ executionTargetId: "pool", siteNodeId: "node" });
    });

    it("prefills the configuration default when a profile defaulting off picks MTP", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: "q4-xl-no-mtp", onSubmit });

      await openAdvancedStep(user);
      expect(speculativeMethod()).toHaveValue("");
      expect(screen.queryByRole("textbox", { name: "Speculative token count" })).toBeNull();

      await user.selectOptions(speculativeMethod(), "MTP");
      expect(speculativeCount()).toHaveValue("3");
      expect(speculativeCount()).not.toHaveAttribute("aria-invalid");
      await deployFromAdvanced(user);
      expect(screen.getByText("Customized: Speculative decoding MTP, 3 tokens")).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual([["mtp", "token-count", 3n]]);
    });

    it("steps the token count within the option's range and sends it", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: "q4-xl", onSubmit });

      await openAdvancedStep(user);
      await user.selectOptions(speculativeMethod(), "MTP");
      expect(speculativeCount()).toHaveValue("3");
      const increase = screen.getByRole("button", { name: "Increase Speculative token count" });
      await user.click(increase);
      await user.click(increase);
      expect(speculativeCount()).toHaveValue("5");
      expect(increase).toBeDisabled();

      await deployFromAdvanced(user);
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual([["mtp", "token-count", 5n]]);
    });

    it("prefills a cleared count from the profile when picking a method again", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: "q4-xl", model: modelWithProfileMtpCount(), onSubmit });

      await openAdvancedStep(user);
      expect(speculativeCount()).toHaveValue("4");
      await user.clear(speculativeCount());
      await user.selectOptions(speculativeMethod(), "Off");
      await user.selectOptions(speculativeMethod(), "MTP");
      expect(speculativeCount()).toHaveValue("4");

      await deployFromAdvanced(user);
      expect(screen.queryByText(/Customized/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual([]);
    });

    it("validates a kept count against the range of the newly picked method", async () => {
      const user = setupUser();
      renderDialog({ initialProfileId: "q4-xl", model: modelWithDflash() });
      const mtpError = "Speculative token count must be an integer from 1 to 5 tokens.";

      await openAdvancedStep(user);
      expect(optionLabels(speculativeMethod())).toEqual(["Off", "MTP", "DFlash"]);
      await user.selectOptions(speculativeMethod(), "DFlash");
      expect(speculativeCount()).toHaveValue("3");
      await user.clear(speculativeCount());
      await user.type(speculativeCount(), "12");

      await user.selectOptions(speculativeMethod(), "MTP");
      expect(speculativeCount()).toHaveValue("12");
      expect(screen.getByText(mtpError)).toBeVisible();
      expect(speculativeCount()).toHaveAttribute("aria-invalid", "true");

      await user.selectOptions(speculativeMethod(), "DFlash");
      expect(screen.queryByText(/Speculative token count must be/)).not.toBeInTheDocument();
    });

    it("falls back to the profile default when a catalog refresh drops the chosen option", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      const { rerender } = renderDialog({
        initialProfileId: "q4-xl",
        model: modelWithDflash(),
        onSubmit,
      });

      await openAdvancedStep(user);
      await user.selectOptions(speculativeMethod(), "DFlash");
      rerender(dialog({ initialProfileId: "q4-xl", onSubmit }));

      expect(speculativeMethod()).toHaveValue("mtp");
      expect(speculativeCount()).toHaveValue("3");
      await deployFromAdvanced(user);
      expect(screen.queryByText(/Customized/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual([]);
    });

    it("validates the count on blur once changed and hides the control without the option", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: "q4-xl", onSubmit });
      const error = "Speculative token count must be an integer from 1 to 5 tokens.";

      await openAdvancedStep(user);
      // Passing through the untouched field flags nothing.
      await user.click(speculativeCount());
      await user.tab();
      expect(screen.queryByText(error)).not.toBeInTheDocument();

      await user.clear(speculativeCount());
      await user.type(speculativeCount(), "6");
      expect(screen.queryByText(error)).not.toBeInTheDocument();
      await user.tab();
      expect(await screen.findByText(error)).toBeVisible();
      expect(speculativeCount()).toHaveAttribute("aria-invalid", "true");

      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 5 — Performance & memory")).toBeVisible();
      expect(speculativeCount()).toHaveFocus();

      await user.clear(speculativeCount());
      await user.type(speculativeCount(), "4");
      expect(screen.queryByText(error)).not.toBeInTheDocument();

      // The vLLM configuration declares no speculative option.
      await user.click(screen.getByRole("button", { name: /Model & machine/ }));
      await user.click(screen.getByRole("radio", { name: /NVFP4 text/ }));
      await goToStep(user, "Performance & memory");
      expect(screen.queryByRole("combobox", { name: "Speculative decoding" })).toBeNull();
      expect(onSubmit).not.toHaveBeenCalled();
    });
  });
});

describe("ModelDeployDialog model options by visibility", () => {
  it("asks a basic option on Capabilities, and skips the step for a profile without one", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl", model: modelWithVision() });

    await screen.findByText("Step 1 of 4 — Model & machine");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 2 of 4 — Capabilities")).toBeVisible();
    const vision = screen.getByRole("switch", { name: "Vision" });
    expect(vision).not.toBeChecked();
    expect(vision).toHaveAccessibleDescription(
      "Accepts images in requests. Lets requests include images.",
    );

    // The vLLM configuration declares no vision option, so there is nothing to ask.
    await user.click(screen.getByRole("button", { name: "Back" }));
    await user.click(screen.getByRole("radio", { name: /NVFP4 text/ }));
    expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 2 of 3 — Context")).toBeVisible();
  });

  it.each([
    { label: "advanced", visibility: ParameterVisibility.ADVANCED },
    { label: "without a declared visibility", visibility: ParameterVisibility.UNSPECIFIED },
  ])("places an option $label on Performance & memory only", async ({ visibility }) => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl", model: modelWithVision(visibility) });

    await screen.findByText("Step 1 of 3 — Model & machine");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 2 of 3 — Context")).toBeVisible();
    expect(screen.queryByRole("switch", { name: "Vision" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    await goToStep(user, "Performance & memory");
    expect(screen.getByRole("switch", { name: "Vision" })).toBeVisible();
  });

  it("shows a hidden option on no step and sends the profile's value", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    renderDialog({
      initialProfileId: "q4-xl",
      model: modelWithVision(ParameterVisibility.HIDDEN),
      onSubmit,
    });

    await openAdvancedStep(user, "Context");
    for (const step of ["Context", "Performance & memory", "Command"] as const) {
      await goToStep(user, step);
      expect(screen.queryByRole("switch", { name: "Vision" })).not.toBeInTheDocument();
    }
    await deployFromAdvanced(user);
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(submittedOverrides(onSubmit)).toEqual([]);
  });

  it("starts a hidden speculative option from the profile default when an invalid value is restored", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    const model = clone(ModelSchema, runtimeSchemaModel);
    const mtp = model.options.find((option) => option.optionId === "mtp");
    if (mtp) mtp.visibility = ParameterVisibility.HIDDEN;
    renderDialog({
      initialConfiguration: {
        customArgumentTokens: [],
        engineParameterValues: create(ParameterValuesSchema, {}),
        modelArtifactBindingId: "",
        // Above the option's maximum of 5.
        optionOverrides: [
          create(ModelOptionValuesSchema, {
            optionId: "mtp",
            values: [unsignedValue("token-count", 9n)],
          }),
        ],
      },
      initialProfileId: "q4-xl",
      model,
      onSubmit,
    });

    await screen.findByText("Step 1 of 3 — Model & machine");
    await goToStep(user, "Review & deploy");
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(submittedOverrides(onSubmit)).toEqual([]);
  });

  it("sends a changed vision choice, summarizes it and restores it from the launch", async () => {
    const user = setupUser();
    const model = modelWithVision();
    const first = vi.fn();
    const { unmount } = renderDialog({ initialProfileId: "q4-xl", model, onSubmit: first });

    await screen.findByText("Step 1 of 4 — Model & machine");
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("switch", { name: "Vision" }));
    await deployFromSettings(user);
    expect(screen.getByText("Vision").nextElementSibling).toHaveTextContent("On");
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(first).toHaveBeenCalledOnce());
    expect(submittedOverrides(first)).toEqual([["vision", "enabled", true]]);
    const applied = first.mock.calls[0][0] as LocalDeploymentValues;
    unmount();

    const onSubmit = vi.fn();
    renderDialog({ ...reopenedOn(applied), model, onSubmit });
    await screen.findByText("Step 1 of 4 — Model & machine");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("switch", { name: "Vision" })).toBeChecked();
    await deployFromSettings(user);
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(onSubmit.mock.calls[0][0]).toEqual(applied);
  });
});

describe("ModelDeployDialog reopened on an applied launch", () => {
  it("shows every applied value and submits the same launch again", async () => {
    const user = setupUser();
    const model = modelWithKvCachePrecision();
    const applied = await applyEveryRuntimeEdit(user, model);
    expect(applied).toMatchObject({
      configuration: {
        customArgumentTokens: ["--foo", "bar"],
        modelArtifactBindingId: "",
      },
      exposure: { kind: { case: "direct", value: { routeName: "team-route" } } },
      name: "Team model",
      profileId: "q4-xl",
    });
    expect(overrideTriples(applied)).toEqual([
      ["mtp", "token-count", 5n],
      ["kv-cache", "precision", "q8_0"],
    ]);

    const onSubmit = vi.fn();
    renderDialog({ ...reopenedOn(applied), model, onSubmit });

    await waitFor(() =>
      expect(screen.getByRole("radio", { checked: true })).toHaveAccessibleName(
        /^Q4_K_XL(?! without)/,
      ),
    );
    await openAdvancedSettings(user);
    expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveValue("2");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Prompt cache memory" })).toHaveValue("4 GiB");
    expect(kvCachePrecision()).toHaveValue("q8_0");
    expect(speculativeMethod()).toHaveValue("mtp");
    expect(speculativeCount()).toHaveValue("5");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Custom arguments" })).toHaveValue("--foo\nbar");

    await deployFromAdvanced(user);
    expect(screen.getByRole("textbox", { name: "Deployment name" })).toHaveValue("Team model");
    expect(screen.getByRole("textbox", { name: "Route name" })).toHaveValue("team-route");
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(onSubmit.mock.calls[0][0]).toEqual(applied);
  });

  it("resets the profile's runtime values on a profile change and keeps the launch's own", async () => {
    const user = setupUser();
    const model = modelWithKvCachePrecision();
    const applied = await applyEveryRuntimeEdit(user, model);

    const onSubmit = vi.fn();
    renderDialog({ ...reopenedOn(applied), model, onSubmit });

    await waitFor(() =>
      expect(screen.getByRole("radio", { checked: true })).toHaveAccessibleName(
        /^Q4_K_XL(?! without)/,
      ),
    );
    await user.click(screen.getByRole("radio", { name: /Q4_K_XL without MTP/ }));
    await openAdvancedSettings(user);
    expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveValue("2");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Prompt cache memory" })).toHaveValue("8 GiB");
    expect(kvCachePrecision()).toHaveValue("f16");
    expect(speculativeMethod()).toHaveValue("");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByRole("textbox", { name: "Custom arguments" })).toHaveValue("--foo\nbar");

    await deployFromAdvanced(user);
    expect(screen.getByRole("textbox", { name: "Deployment name" })).toHaveValue("Team model");
    expect(screen.getByRole("textbox", { name: "Route name" })).toHaveValue("team-route");
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(onSubmit.mock.calls[0][0]).toMatchObject({
      configuration: { customArgumentTokens: ["--foo", "bar"], modelArtifactBindingId: "" },
      exposure: { kind: { case: "direct", value: { routeName: "team-route" } } },
      name: "Team model",
      profileId: "q4-xl-no-mtp",
    });
    expect(submittedOverrides(onSubmit)).toEqual([]);
    expect(submittedValues(onSubmit)).toEqual([
      ["prompt-cache-memory-mib", 8192n],
      ["context-per-request", 32768n],
      ["parallel-requests", 2n],
      ["total-context", 65536n],
    ]);
  });

  it("reads an applied hidden exposure and speculative decoding off back", async () => {
    const user = setupUser();
    const model = modelWithKvCachePrecision();
    const first = vi.fn();
    const { unmount } = renderDialog({ initialProfileId: "q4-xl", model, onSubmit: first });

    await openAdvancedStep(user);
    await user.selectOptions(speculativeMethod(), "Off");
    await deployFromAdvanced(user);
    await user.click(screen.getByRole("radio", { name: "Hidden" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(first).toHaveBeenCalledOnce());
    const applied = first.mock.calls[0][0] as LocalDeploymentValues;
    unmount();

    const onSubmit = vi.fn();
    renderDialog({ ...reopenedOn(applied), model, onSubmit });
    await openAdvancedStep(user);
    expect(speculativeMethod()).toHaveValue("");
    await deployFromAdvanced(user);
    expect(screen.getByRole("radio", { name: "Hidden" })).toBeChecked();
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(onSubmit.mock.calls[0][0]).toEqual(applied);
  });
});

function renderDialog(props: Partial<ComponentProps<typeof ModelDeployDialog>> = {}) {
  return render(dialog(props));
}

function dialog(props: Partial<ComponentProps<typeof ModelDeployDialog>> = {}) {
  const listTargets = () => Promise.resolve({ executionTargets: [poolTarget()] });

  return (
    <ExecutionTargetsWatch watch={executionTargetsWatch(listTargets)}>
      <ModelDeployDialog
        engineReleases={runtimeSchemaReleases}
        hardwareClasses={[]}
        listConfiguredExecutionTargets={listTargets}
        model={runtimeSchemaModel}
        onEstimate={sharedContextEstimate(props.model ?? runtimeSchemaModel)}
        onOpenChange={vi.fn()}
        onPreview={vi.fn()}
        onSubmit={vi.fn()}
        open
        pending={false}
        {...props}
      />
    </ExecutionTargetsWatch>
  );
}

/**
 * The fixture model with a second speculative option, "DFlash", 0..15 and off by default, beside
 * MTP on llama.cpp. It reuses the MTP method, the only one the contract declares today.
 */
function modelWithDflash() {
  const model = clone(ModelSchema, runtimeSchemaModel);
  model.options.push(
    create(ModelOptionSchema, {
      optionId: "dflash",
      displayName: "DFlash",
      kind: { case: "speculativeDecoding", value: { method: SpeculativeDecodingMethod.MTP } },
    }),
  );
  model.configurations[0].options.push(
    create(ModelConfigurationOptionSchema, {
      optionId: "dflash",
      parameters: [
        {
          parameterId: "token-count",
          displayName: "Token count",
          parameterType: {
            case: "unsignedInteger",
            value: { minimum: 0n, maximum: 15n, unit: "tokens" },
          },
        },
      ],
      defaultValues: { optionId: "dflash", values: [unsignedValue("token-count", 0n)] },
    }),
  );
  return model;
}

/**
 * Deploys "q4-xl" with every runtime field, the name and the route edited, the way Customize
 * applies a launch, and returns the submitted launch.
 */
async function applyEveryRuntimeEdit(
  user: UserEvent,
  model: ReturnType<typeof modelWithKvCachePrecision>,
): Promise<LocalDeploymentValues> {
  const onSubmit = vi.fn();
  const { unmount } = renderDialog({
    initialProfileId: "q4-xl",
    listProfileCompatibility: compatibilityLoader(
      targetCompatibility(poolPlacement, { "q4-xl": {}, "q4-xl-no-mtp": {} }),
    ),
    model,
    onSubmit,
  });

  await openAdvancedSettings(user);
  const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
  await user.clear(parallel);
  await user.type(parallel, "2");
  await user.click(screen.getByRole("button", { name: "Next" }));
  const promptCache = screen.getByRole("textbox", { name: "Prompt cache memory" });
  await user.clear(promptCache);
  await user.type(promptCache, "4096");
  await user.selectOptions(kvCachePrecision(), "q8_0");
  await user.clear(speculativeCount());
  await user.type(speculativeCount(), "5");
  await user.click(screen.getByRole("button", { name: "Next" }));
  await user.type(screen.getByRole("textbox", { name: "Custom arguments" }), "--foo bar");

  await deployFromAdvanced(user);
  const name = screen.getByRole("textbox", { name: "Deployment name" });
  await user.clear(name);
  await user.type(name, "Team model");
  const route = screen.getByRole("textbox", { name: "Route name" });
  await user.clear(route);
  await user.type(route, "team-route");
  await user.click(screen.getByRole("button", { name: "Deploy" }));
  await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
  unmount();
  return onSubmit.mock.calls[0][0] as LocalDeploymentValues;
}

/** The dialog props that start it from a launch it submitted, as Customize reopens it. */
function reopenedOn(launch: LocalDeploymentValues) {
  const value = (parameterId: string) => {
    const found = launch.configuration.engineParameterValues.values.find(
      (candidate) => candidate.parameterId === parameterId,
    )?.value;
    return found?.case === "unsignedInteger" ? Number(found.value) : undefined;
  };
  return {
    scenario: {
      contextTokensPerRequest: value("context-per-request") ?? 0,
      parallelRequests: value("parallel-requests"),
      totalContextTokens: value("total-context"),
    },
    initialConfiguration: launch.configuration,
    initialExposure: launch.exposure,
    initialName: launch.name,
    initialPlacement: launch.placement,
    initialProfileId: launch.profileId,
    listProfileCompatibility: compatibilityLoader(
      targetCompatibility(poolPlacement, { "q4-xl": {}, "q4-xl-no-mtp": {} }),
    ),
  };
}

/** The fixture model whose "q4-xl" profile defaults MTP to 4 tokens, over the configuration's 3. */
function modelWithProfileMtpCount() {
  const model = clone(ModelSchema, runtimeSchemaModel);
  const profile = model.profiles.find((candidate) => candidate.profileId === "q4-xl");
  profile?.optionDefaults.push(
    create(ModelOptionValuesSchema, {
      optionId: "mtp",
      values: [unsignedValue("token-count", 4n)],
    }),
  );
  return model;
}

function poolTarget() {
  return create(ConfiguredExecutionTargetSchema, {
    executionTargetId: "pool",
    siteNodeId: "node",
    workloadKind: ExecutionTargetWorkloadKind.MODEL,
    siteManager: true,
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: { availability: { case: "available", value: {} } },
    },
  });
}

/** Turns Advanced on and moves to the Context step. */
async function openAdvancedSettings(user: UserEvent) {
  await openAdvancedStep(user, "Context");
}

/** The engine parameter values of the first submitted deployment, as `[parameterId, value]`. */
function submittedValues(onSubmit: ReturnType<typeof vi.fn>) {
  const [values] = onSubmit.mock.calls[0] as [
    { configuration: { engineParameterValues: { values: ReturnType<typeof unsignedValue>[] } } },
  ];
  return values.configuration.engineParameterValues.values.map((value) => [
    value.parameterId,
    value.value.value,
  ]);
}

function presetLabels(label = "Total context") {
  return within(screen.getByRole("group", { name: `${label} presets` }))
    .getAllByRole("button")
    .map((button) => button.textContent);
}

function preset(label: string, field = "Total context") {
  return within(screen.getByRole("group", { name: `${field} presets` })).getByRole("button", {
    name: label,
  });
}

// Each textbox of the step, as `[id, value]`.
function textboxValues() {
  return screen.getAllByRole("textbox").map((input) => [input.id, input.getAttribute("value")]);
}

async function deployFromSettings(user: UserEvent) {
  await goToStep(user, "Review & deploy");
}

async function deployFromAdvanced(user: UserEvent) {
  await goToStep(user, "Review & deploy");
}

/** The option overrides of the first submitted deployment, as `[optionId, parameterId, value]`. */
function submittedOverrides(onSubmit: ReturnType<typeof vi.fn>) {
  const [values] = onSubmit.mock.calls[0] as [OverrideCarrier];
  return overrideTriples(values);
}

/** The option overrides of one estimate request, as `[optionId, parameterId, value]`. */
function estimatedOverrides(onEstimate: ReturnType<typeof vi.fn>, call: number) {
  const [values] = onEstimate.mock.calls[call] as [OverrideCarrier];
  return overrideTriples(values);
}

interface OverrideCarrier {
  configuration: {
    optionOverrides: {
      optionId: string;
      values: { parameterId: string; value: { value?: unknown } }[];
    }[];
  };
}

function overrideTriples({ configuration }: OverrideCarrier) {
  return configuration.optionOverrides.flatMap((override) =>
    override.values.map((value) => [override.optionId, value.parameterId, value.value.value]),
  );
}

function speculativeMethod() {
  return screen.getByRole("combobox", { name: "Speculative decoding" });
}

function speculativeCount() {
  return screen.getByRole("textbox", { name: "Speculative token count" });
}

function kvCachePrecision() {
  return screen.getByRole("combobox", { name: "KV cache precision" });
}

function optionLabels(select: HTMLElement) {
  return within(select)
    .getAllByRole("option")
    .map((option) => option.textContent);
}

// The dialog on the fixture model's Strata profile; the arguments adjust the configuration and
// the profile as `modelWithStrata` does.
function renderStrataDialog(
  configuration: Parameters<typeof modelWithStrata>[0] = {},
  profile: Parameters<typeof modelWithStrata>[1] = {},
  onSubmit = vi.fn(),
) {
  const model = modelWithStrata(configuration, profile);
  return renderDialog({
    engineReleases: [...runtimeSchemaReleases, strataRelease],
    initialProfileId: "strata",
    model,
    onSubmit,
  });
}
