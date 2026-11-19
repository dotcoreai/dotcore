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
  runtimeSchemaModel,
  runtimeSchemaReleases,
  strataRelease,
  unsignedValue,
} from "@/features/models/runtime/runtime-schema-test-support";
import {
  ModelArtifactBindingSchema,
  ModelArtifactSchema,
  ModelConfigurationOptionSchema,
  ModelOptionSchema,
  ModelOptionValuesSchema,
  ModelSchema,
  SpeculativeDecodingMethod,
} from "@/gen/dotcore/catalog/v1/model_pb";
import { ParameterVisibility } from "@/gen/dotcore/catalog/v1/parameter_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { readFieldHelpFacts } from "@/test/field-help";
import { setupUser } from "@/test/user";

import { ModelDeployDialog } from "./model-deploy-dialog";
import type { LocalDeploymentValues } from "./model-deploy-launch";
import { sharedContextEstimate } from "./resource-estimate-test-support";

const poolPlacement = { executionTargetId: "pool", siteNodeId: "node" };

describe("ModelDeployDialog Settings step", () => {
  it("shows total context and context per request in both modes, with short help", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await screen.findByText("Step 1 of 3 — Profile");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 2 of 3 — Settings")).toBeVisible();
    expect(
      screen.getAllByRole("textbox").map((input) => [input.id, input.getAttribute("value")]),
    ).toEqual([
      ["engine-parameter-total-context", "32k"],
      ["engine-parameter-context-per-request", ""],
    ]);
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveAccessibleDescription(
      "Context memory the server holds from the start and shares among the requests it generates at once. Each active request stores its own tokens in it, even when requests start with the same prompt. When it fills, the server frees idle requests first, then fails the active ones.",
    );
    const perRequest = screen.getByRole("textbox", { name: "Context per request" });
    expect(perRequest).toHaveAccessibleDescription(
      "Largest context one request may use from the total context. It reserves nothing: requests take memory from the total context as they grow. A new request reuses the start of an earlier prompt from an idle request or the prompt cache, but not from requests running at the same time.",
    );
    await user.click(screen.getByRole("button", { name: "About Context per request" }));
    expect(
      await screen.findByRole("dialog", { name: "About Context per request" }),
    ).toHaveTextContent("Left empty, it follows the total context.");
    await user.keyboard("{Escape}");

    // A set context per request offers to follow the total again.
    await user.type(perRequest, "8192");
    await user.tab();
    await user.click(screen.getByRole("button", { name: "Follow the total" }));
    expect(perRequest).toHaveValue("");
    expect(await readFieldHelpFacts("Context per request")).toContain(
      "Left empty, it follows the total context.",
    );

    await user.click(preset("64k"));
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(screen.getByText("Step 3 of 5 — Settings")).toBeVisible();
    expect(
      screen.getAllByRole("textbox").map((input) => [input.id, input.getAttribute("value")]),
    ).toEqual([
      ["engine-parameter-total-context", "64k"],
      ["engine-parameter-context-per-request", ""],
    ]);
  });

  it("edits the llama.cpp context with a log slider from the declared minimum, an input and presets", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await openAdvancedSettings(user);
    const slider = screen.getByRole("slider", { name: "Total context" });
    const input = screen.getByRole("textbox", { name: "Total context" });
    expect(input).toHaveValue("32k");
    // The total ends at the 256k model maximum for each of the 4 parallel requests, so 32768 tokens
    // sits six elevenths along the log scale from the declared 512 tokens to 1048576 tokens.
    expect(slider).toHaveValue("545");
    expect(slider).toHaveAttribute("aria-valuetext", "32,768 tokens");
    expect(presetLabels()).toEqual(["8k", "32k", "64k", "128k", "256k", "1M"]);
    // A chip is pressed only right after it is clicked, not for a value that equals it.
    expect(preset("32k")).toHaveAttribute("aria-pressed", "false");

    await user.click(preset("128k"));
    expect(input).toHaveValue("128k");
    expect(slider).toHaveValue("727");
    expect(preset("128k")).toHaveAttribute("aria-pressed", "true");
    // A drag releases the chip, even back onto its own size.
    fireEvent.change(slider, { target: { value: "700" } });
    fireEvent.change(slider, { target: { value: "727" } });
    expect(input).toHaveValue("128k");
    expect(preset("128k")).toHaveAttribute("aria-pressed", "false");

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
    const hostMemory = screen.getByRole("group", { name: "Host memory" });
    const kvOffload = within(hostMemory).getByRole("textbox", { name: "KV offloading size" });
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
      await within(hostMemory).findByText("KV offloading size must be a whole number of GiB."),
    ).toBeVisible();
    expect(kvOffload).toHaveAttribute("aria-invalid", "true");
    expect(await readFieldHelpFacts("KV offloading size")).toContain("Range 0 or more GiB");

    // Without a declared maximum, any larger size is accepted, typed in any byte unit.
    await user.clear(kvOffload);
    await user.type(kvOffload, "2t");
    expect(within(hostMemory).queryByText(/must be/)).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    // The empty context per request is left out for resolution to fill.
    expect(submittedValues(onSubmit)).toEqual([
      ["maximum-batched-tokens", 4096n],
      ["maximum-cudagraph-capture-size", 32n],
      ["kv-offloading-size-gib", 2048n],
      ["total-context", 65536n],
      ["parallel-requests", 4n],
    ]);
  });

  describe.each([
    {
      engine: "llama.cpp",
      profileId: "q4-xl",
      defaults: [
        ["prompt-cache-memory-mib", 8192n],
        ["total-context", 32768n],
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
        ["total-context", 32768n],
        ["parallel-requests", 4n],
      ],
    },
    {
      engine: "ninfer",
      profileId: "ninfer-nvfp4",
      defaults: [
        ["parallel-requests", 4n],
        ["host-context-mib", 20480n],
        ["total-context", 32768n],
      ],
    },
  ])("$engine context group", ({ defaults, profileId }) => {
    it("shows the shared context inputs at the engine defaults and submits them", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: profileId, onSubmit });

      await openAdvancedSettings(user);
      expect(
        screen.getAllByRole("textbox").map((input) => [input.id, input.getAttribute("value")]),
      ).toEqual([
        ["engine-parameter-total-context", "32k"],
        ["engine-parameter-context-per-request", ""],
      ]);
      // An empty context per request stands for the total context, capped by the maximum.
      const perRequest = screen.getByRole("textbox", { name: "Context per request" });
      expect(perRequest).toHaveAttribute("placeholder", "32k");
      expect(screen.getByRole("slider", { name: "Context per request" })).toHaveAttribute(
        "aria-valuetext",
        "32,768 tokens",
      );

      await deployFromSettings(user);
      expect(screen.queryByText(/Customized/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      // The empty context per request is left out for resolution to fill.
      expect(submittedValues(onSubmit)).toEqual(defaults);
    });

    it("moves the total context and the context per request together so the total covers it", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: profileId, onSubmit });

      await openAdvancedSettings(user);
      const total = screen.getByRole("textbox", { name: "Total context" });
      const perRequest = screen.getByRole("textbox", { name: "Context per request" });

      // A typed context per request above the total raises the total once left.
      await user.type(perRequest, "64k");
      expect(total).toHaveValue("32k");
      await user.tab();
      expect(total).toHaveValue("64k");

      // A total preset below the context per request lowers it at once.
      await user.click(
        within(screen.getByRole("group", { name: "Total context presets" })).getByRole("button", {
          name: "8k",
        }),
      );
      expect(perRequest).toHaveValue("8k");
      expect(screen.queryByText(/must be at most the total context/)).not.toBeInTheDocument();

      await deployFromSettings(user);
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedValues(onSubmit)).toEqual([
        ...defaults.map(([parameterId, value]) =>
          parameterId === "total-context" ? [parameterId, 8192n] : [parameterId, value],
        ),
        ["context-per-request", 8192n],
      ]);
    });
  });

  it("draws the ninfer context sliders on one scale, stopping context per request at the model maximum", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "ninfer-nvfp4" });

    await openAdvancedSettings(user);
    const totalSlider = screen.getByRole("slider", { name: "Total context" });
    const perRequestSlider = screen.getByRole("slider", { name: "Context per request" });
    // The scale runs from 512 tokens to the total's end, the 256k model maximum for each of the 4
    // parallel requests: 32768 tokens sits six elevenths along on both.
    expect(totalSlider).toHaveValue("545");
    expect(perRequestSlider).toHaveValue("545");

    // The end of the shared scale clamps context per request to the 262144 model maximum, below
    // ninfer's own, nine elevenths along, and the total follows it there.
    fireEvent.change(perRequestSlider, { target: { value: "1000" } });
    expect(screen.getByRole("textbox", { name: "Context per request" })).toHaveValue("256k");
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("256k");
    expect(perRequestSlider).toHaveValue("818");
    expect(totalSlider).toHaveValue("818");
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
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("2147483647");
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
    expect(totalSlider).toHaveValue("545");
    expect(presetLabels()).toEqual(["8k", "32k", "64k", "128k", "256k", "1M"]);
    expect(presetLabels("Context per request")).toEqual(["8k", "32k", "64k", "128k", "256k"]);
    fireEvent.change(perRequestSlider, { target: { value: "1000" } });
    expect(screen.getByRole("textbox", { name: "Context per request" })).toHaveValue("256k");
    await user.click(screen.getByRole("button", { name: "Follow the total" }));

    // One parallel request brings the total's end down to the context per request's.
    await user.click(screen.getByRole("button", { name: "Next" }));
    const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
    await user.clear(parallel);
    await user.type(parallel, "1");
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByRole("slider", { name: "Total context" })).toHaveValue("1000");
    fireEvent.change(screen.getByRole("slider", { name: "Total context" }), {
      target: { value: "1000" },
    });
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("256k");
    expect(presetLabels()).toEqual(["8k", "32k", "64k", "128k", "256k"]);
  });

  it("stands an empty context per request for the maximum context when below the total", async () => {
    const user = setupUser();
    const model = clone(ModelSchema, runtimeSchemaModel);
    model.maximumContextTokens = 262144;
    model.configurations[0].maximumContextTokens = 16384;
    renderDialog({ initialProfileId: "q4-xl", model });

    await openAdvancedSettings(user);
    const perRequest = screen.getByRole("textbox", { name: "Context per request" });
    expect(perRequest).toHaveAttribute("placeholder", "16k");
    await user.click(screen.getByRole("button", { name: "About Context per request" }));
    expect(
      await screen.findByRole("dialog", { name: "About Context per request" }),
    ).toHaveTextContent("Up to 16k (model maximum)");
    await user.keyboard("{Escape}");
    await user.click(preset("8k"));
    expect(perRequest).toHaveAttribute("placeholder", "8k");
  });
});

describe("ModelDeployDialog Advanced step", () => {
  it("leads with parallel requests, then the ninfer host context in its host memory group", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "ninfer-nvfp4" });

    await openAdvancedStep(user);
    const parallel = screen.getAllByRole("textbox")[0];
    expect(parallel).toHaveAccessibleName("Parallel requests");
    expect(parallel).toHaveValue("4");
    expect(parallel).toHaveAccessibleDescription(
      "Requests the server generates at the same time; further requests wait in a queue.",
    );
    const hostMemory = screen.getByRole("group", { name: "Host memory" });
    const hostContext = within(hostMemory).getByRole("textbox", { name: "Host context memory" });
    expect(hostContext).toHaveValue("20 GiB");
    expect(hostContext).toHaveAccessibleDescription(
      "Host memory where idle conversations and paused requests are kept, so they resume without processing their prompt again. 0 turns it off.",
    );

    // The context sizes sit on the Settings step.
    for (const name of ["Total context", "Context per request"])
      expect(screen.queryByRole("textbox", { name })).not.toBeInTheDocument();
  });

  it("keeps the llama.cpp prompt cache at the top level, and the artifact and custom arguments in collapsed sections", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await openAdvancedStep(user);
    // llama.cpp declares no parameter group, so its prompt cache follows parallel requests, ahead
    // of the speculative decoding choice.
    expect(screen.getAllByRole("textbox").map((input) => input.id)).toEqual([
      "engine-parameter-parallel-requests",
      "engine-parameter-prompt-cache-memory-mib",
      "speculative-decoding-count",
    ]);
    expect(screen.getAllByRole("group").map((group) => group.getAttribute("aria-label"))).toEqual([
      "Engine and artifact",
      "Custom arguments and command",
    ]);
    expect(sectionToggle("Engine and artifact")).toHaveAttribute("aria-expanded", "false");
    expect(sectionToggle("Custom arguments and command")).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("combobox", { name: "Model artifact" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add token" })).not.toBeInTheDocument();

    await user.click(sectionToggle("Engine and artifact"));
    expect(screen.getByRole("combobox", { name: "Model artifact" })).toHaveValue("Q4_K_XL");
    await user.click(sectionToggle("Custom arguments and command"));
    expect(screen.getByRole("button", { name: "Add token" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Preview resolved command" })).toBeVisible();
  });

  it("shows the Strata expert memory group open, after parallel requests, with its help", async () => {
    const user = setupUser();
    renderStrataDialog();

    await openAdvancedStep(user);
    expect(screen.getAllByRole("textbox").map((input) => input.id)).toEqual([
      "engine-parameter-parallel-requests",
      "engine-parameter-expert-vram-mib",
      "engine-parameter-expert-host-memory-mib",
    ]);
    const group = screen.getByRole("group", { name: "Expert memory" });
    expect(group).not.toContainElement(screen.getByRole("textbox", { name: "Parallel requests" }));
    const toggle = within(group).getByRole("button", { name: "Expert memory" });
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(toggle).toHaveAccessibleDescription("Where Strata keeps the model's expert weights.");
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
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(submittedValues(onSubmit)).toContainEqual(["expert-vram-mib", 6144n]);
  });

  it("shows a group in each step that holds a member, by the profile's visibility", async () => {
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
    const settingsGroup = screen.getByRole("group", { name: "Expert memory" });
    expect(
      within(settingsGroup)
        .getAllByRole("textbox")
        .map((input) => input.id),
    ).toEqual(["engine-parameter-expert-vram-mib"]);

    await user.click(screen.getByRole("button", { name: "Next" }));
    const advancedGroup = screen.getByRole("group", { name: "Expert memory" });
    expect(
      within(advancedGroup)
        .getAllByRole("textbox")
        .map((input) => input.id),
    ).toEqual(["engine-parameter-expert-host-memory-mib"]);
  });

  it("shows no box for a group whose members the configuration excludes", async () => {
    const user = setupUser();
    renderStrataDialog({
      excludedEngineParameterIds: ["expert-vram-mib", "expert-host-memory-mib"],
    });

    await openAdvancedStep(user);
    expect(screen.queryByRole("group", { name: "Expert memory" })).not.toBeInTheDocument();
    expect(screen.getAllByRole("textbox").map((input) => input.id)).toEqual([
      "engine-parameter-parallel-requests",
    ]);
  });

  it("reopens a collapsed group and focuses its invalid value", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    renderStrataDialog({}, {}, onSubmit);
    const error = "Expert host memory must be a whole number of MiB.";

    await openAdvancedStep(user);
    const hostMemory = screen.getByRole("textbox", { name: "Expert host memory" });
    await user.clear(hostMemory);
    await user.type(hostMemory, "1.5");
    await user.click(sectionToggle("Expert memory"));
    await waitFor(() =>
      expect(screen.queryByRole("textbox", { name: "Expert host memory" })).not.toBeInTheDocument(),
    );

    // Leaving Advanced re-opens the group and focuses the field.
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 4 of 5 — Advanced")).toBeVisible();
    expect(sectionToggle("Expert memory")).toHaveAttribute("aria-expanded", "true");
    await waitFor(() =>
      expect(screen.getByRole("textbox", { name: "Expert host memory" })).toHaveFocus(),
    );
    expect(screen.getByText(error)).toBeVisible();

    // With Advanced off, Deploy turns it back on and reveals the same field.
    await user.click(sectionToggle("Expert memory"));
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
    // The new step takes focus in an effect after it renders; acting earlier loses that move.
    await waitFor(() =>
      expect(document.activeElement).toHaveAttribute("data-deployment-step-heading"),
    );
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    expect(await screen.findByText("Step 4 of 5 — Advanced")).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Advanced" })).toBeChecked();
    expect(sectionToggle("Expert memory")).toHaveAttribute("aria-expanded", "true");
    const revealed = screen.getByRole("textbox", { name: "Expert host memory" });
    await waitFor(() => expect(revealed).toHaveFocus());
    expect(revealed).toHaveAttribute("aria-invalid", "true");
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
      await user.click(screen.getByRole("button", { name: "Custom arguments and command" }));
      await user.click(screen.getByRole("button", { name: "Preview resolved command" }));

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
      expect(screen.getByText("Step 4 of 5 — Advanced")).toBeVisible();
      expect(speculativeCount()).toHaveFocus();

      await user.clear(speculativeCount());
      await user.type(speculativeCount(), "4");
      expect(screen.queryByText(error)).not.toBeInTheDocument();

      // The vLLM configuration declares no speculative option.
      await user.click(screen.getByRole("button", { name: /Profile/ }));
      await user.click(screen.getByRole("radio", { name: /NVFP4 text/ }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 4 of 5 — Advanced")).toBeVisible();
      expect(screen.queryByRole("combobox", { name: "Speculative decoding" })).toBeNull();
      expect(onSubmit).not.toHaveBeenCalled();
    });
  });
});

describe("ModelDeployDialog reopened on an applied launch", () => {
  it("shows every applied value and submits the same launch again", async () => {
    const user = setupUser();
    const model = modelWithArtifactChoice();
    const applied = await applyEveryRuntimeEdit(user, model);
    expect(applied).toMatchObject({
      configuration: {
        customArgumentTokens: ["--foo", "bar"],
        modelArtifactBindingId: "q5-xl",
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
    await openAdvancedStep(user);
    expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveValue("2");
    expect(screen.getByRole("textbox", { name: "Prompt cache memory" })).toHaveValue("4 GiB");
    await user.click(sectionToggle("Engine and artifact"));
    expect(screen.getByRole("combobox", { name: "Model artifact" })).toHaveValue("q5-xl");
    expect(kvCachePrecision()).toHaveValue("q8_0");
    expect(speculativeMethod()).toHaveValue("mtp");
    expect(speculativeCount()).toHaveValue("5");
    await user.click(sectionToggle("Custom arguments and command"));
    expect(screen.getByRole("textbox", { name: "Custom token 1" })).toHaveValue("--foo");
    expect(screen.getByRole("textbox", { name: "Custom token 2" })).toHaveValue("bar");

    await deployFromAdvanced(user);
    expect(screen.getByRole("textbox", { name: "Deployment name" })).toHaveValue("Team model");
    expect(screen.getByRole("textbox", { name: "Route name" })).toHaveValue("team-route");
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(onSubmit.mock.calls[0][0]).toEqual(applied);
  });

  it("resets the profile's runtime values on a profile change and keeps the launch's own", async () => {
    const user = setupUser();
    const model = modelWithArtifactChoice();
    const applied = await applyEveryRuntimeEdit(user, model);

    const onSubmit = vi.fn();
    renderDialog({ ...reopenedOn(applied), model, onSubmit });

    await waitFor(() =>
      expect(screen.getByRole("radio", { checked: true })).toHaveAccessibleName(
        /^Q4_K_XL(?! without)/,
      ),
    );
    await user.click(screen.getByRole("radio", { name: /Q4_K_XL without MTP/ }));
    await openAdvancedStep(user);
    expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveValue("2");
    expect(screen.getByRole("textbox", { name: "Prompt cache memory" })).toHaveValue("8 GiB");
    await user.click(sectionToggle("Engine and artifact"));
    expect(screen.getByRole("combobox", { name: "Model artifact" })).toHaveValue("Q4_K_XL");
    expect(kvCachePrecision()).toHaveValue("f16");
    expect(speculativeMethod()).toHaveValue("");
    await user.click(sectionToggle("Custom arguments and command"));
    expect(screen.getByRole("textbox", { name: "Custom token 1" })).toHaveValue("--foo");
    expect(screen.getByRole("textbox", { name: "Custom token 2" })).toHaveValue("bar");

    await deployFromAdvanced(user);
    expect(screen.getByRole("textbox", { name: "Deployment name" })).toHaveValue("Team model");
    expect(screen.getByRole("textbox", { name: "Route name" })).toHaveValue("team-route");
    await user.click(screen.getByRole("button", { name: "Deploy" }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    expect(onSubmit.mock.calls[0][0]).toMatchObject({
      configuration: { customArgumentTokens: ["--foo", "bar"], modelArtifactBindingId: "q4-xl" },
      exposure: { kind: { case: "direct", value: { routeName: "team-route" } } },
      name: "Team model",
      profileId: "q4-xl-no-mtp",
    });
    expect(submittedOverrides(onSubmit)).toEqual([]);
    expect(submittedValues(onSubmit)).toEqual([
      ["prompt-cache-memory-mib", 8192n],
      ["total-context", 32768n],
      ["parallel-requests", 2n],
    ]);
  });

  it("reads an applied hidden exposure and speculative decoding off back", async () => {
    const user = setupUser();
    const model = modelWithArtifactChoice();
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
 * The fixture model with the KV cache precision option and a second llama.cpp artifact, "q5-xl",
 * that no profile defaults to.
 */
function modelWithArtifactChoice() {
  const model = modelWithKvCachePrecision();
  model.artifacts.push(clone(ModelArtifactSchema, { ...model.artifacts[0], artifactId: "q5-xl" }));
  model.configurations[0].artifactBindings.push(
    create(ModelArtifactBindingSchema, { bindingId: "q5-xl", modelArtifactId: "q5-xl" }),
  );
  return model;
}

/**
 * Deploys "q4-xl" with every runtime field, the name and the route edited, the way Customize
 * applies a launch, and returns the submitted launch.
 */
async function applyEveryRuntimeEdit(
  user: UserEvent,
  model: ReturnType<typeof modelWithArtifactChoice>,
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

  await openAdvancedStep(user);
  const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
  await user.clear(parallel);
  await user.type(parallel, "2");
  const promptCache = screen.getByRole("textbox", { name: "Prompt cache memory" });
  await user.clear(promptCache);
  await user.type(promptCache, "4096");
  await user.click(sectionToggle("Engine and artifact"));
  await user.click(screen.getByRole("combobox", { name: "Model artifact" }));
  await user.click((await screen.findAllByText("q5-xl"))[0]);
  await user.selectOptions(kvCachePrecision(), "q8_0");
  await user.clear(speculativeCount());
  await user.type(speculativeCount(), "5");
  await user.click(sectionToggle("Custom arguments and command"));
  await user.click(screen.getByRole("button", { name: "Add token" }));
  await user.type(screen.getByRole("textbox", { name: "Custom token 1" }), "--foo");
  await user.click(screen.getByRole("button", { name: "Add token" }));
  await user.type(screen.getByRole("textbox", { name: "Custom token 2" }), "bar");

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
  return {
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

async function openAdvancedSettings(user: UserEvent) {
  await screen.findByText("Step 1 of 3 — Profile");
  await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 2 of 5 — Target")).toBeVisible();
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 3 of 5 — Settings")).toBeVisible();
}

async function openAdvancedStep(user: UserEvent) {
  await openAdvancedSettings(user);
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 4 of 5 — Advanced")).toBeVisible();
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

function preset(label: string) {
  return within(screen.getByRole("group", { name: "Total context presets" })).getByRole("button", {
    name: label,
  });
}

function sectionToggle(name: string) {
  return screen.getByRole("button", { name });
}

async function deployFromSettings(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Next" }));
  await deployFromAdvanced(user);
}

async function deployFromAdvanced(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 5 of 5 — Deploy")).toBeVisible();
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
