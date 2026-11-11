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
  modelWithKvCachePrecision,
  runtimeSchemaModel,
  runtimeSchemaReleases,
  unsignedValue,
} from "@/features/models/runtime/runtime-schema-test-support";
import {
  ModelConfigurationOptionSchema,
  ModelOptionSchema,
  ModelOptionValuesSchema,
  ModelSchema,
  SpeculativeDecodingMethod,
} from "@/gen/dotcore/catalog/v1/model_pb";
import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { setupUser } from "@/test/user";

import { ModelDeployDialog } from "./model-deploy-dialog";
import { sharedContextEstimate } from "./resource-estimate-test-support";

describe("ModelDeployDialog Context step", () => {
  it("shows total context and context per request in both modes, with short help", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await screen.findByText("Step 1 of 3 — Profile");
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 2 of 3 — Context")).toBeVisible();
    expect(
      screen.getAllByRole("textbox").map((input) => [input.id, input.getAttribute("value")]),
    ).toEqual([
      ["engine-parameter-total-context", "32768"],
      ["engine-parameter-context-per-request", ""],
    ]);
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveAccessibleDescription(
      "Memory the model holds for all requests at once.",
    );
    const perRequest = screen.getByRole("textbox", { name: "Context per request" });
    expect(perRequest).toHaveAccessibleDescription(
      "Filled from the total context, up to the model maximum.",
    );

    // A set context per request offers to follow the total again.
    await user.type(perRequest, "8192");
    await user.tab();
    await user.click(screen.getByRole("button", { name: "Follow the total" }));
    expect(perRequest).toHaveValue("");
    expect(
      screen.getByText("Filled from the total context, up to the model maximum."),
    ).toBeVisible();

    await user.click(preset("64k"));
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(screen.getByText("Step 3 of 5 — Context")).toBeVisible();
    expect(
      screen.getAllByRole("textbox").map((input) => [input.id, input.getAttribute("value")]),
    ).toEqual([
      ["engine-parameter-total-context", "65536"],
      ["engine-parameter-context-per-request", ""],
    ]);
  });

  it("edits the llama.cpp context with a log slider, an input and presets in the schema range", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await openAdvancedContext(user);
    const slider = screen.getByRole("slider", { name: "Total context" });
    const input = screen.getByRole("textbox", { name: "Total context" });
    expect(input).toHaveValue("32768");
    // 32768 tokens sits five fourteenths along the log scale from 1024 to 16777216 tokens.
    expect(slider).toHaveValue("357");
    expect(slider).toHaveAttribute("aria-valuetext", "32,768 tokens");
    expect(presetLabels()).toEqual(["8k", "32k", "64k", "128k", "256k", "1M"]);
    expect(preset("32k")).toHaveAttribute("aria-pressed", "true");

    await user.click(preset("128k"));
    expect(input).toHaveValue("131072");
    expect(slider).toHaveValue("500");
    expect(preset("128k")).toHaveAttribute("aria-pressed", "true");

    fireEvent.change(slider, { target: { value: "1000" } });
    expect(input).toHaveValue("16777216");
    fireEvent.change(slider, { target: { value: "0" } });
    expect(input).toHaveValue("1024");

    await user.clear(input);
    await user.type(input, "20000000");
    await user.tab();
    const error = "Total context must be an integer from 1024 to 16777216 tokens.";
    expect(await screen.findByText(error)).toBeVisible();
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input).toHaveAccessibleDescription(
      `Memory the model holds for all requests at once. ${error}`,
    );
  });

  it("groups the vLLM KV offload size as host memory and submits it with the hidden defaults", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    renderDialog({ initialProfileId: "nvfp4-text", onSubmit });

    await openAdvancedContext(user);
    expect(presetLabels()).toEqual(["8k", "32k", "64k", "128k", "256k", "1M"]);
    await user.click(preset("64k"));
    await user.click(screen.getByRole("button", { name: "Next" }));
    const hostMemory = screen.getByRole("region", { name: "Host memory" });
    expect(hostMemory).toHaveTextContent("Accelerator memory is unchanged.");
    const kvOffload = within(hostMemory).getByRole("textbox", { name: "KV offloading size" });
    expect(kvOffload).toHaveValue("0");
    // Parameters the schema does not expose stay out of the form, so no other parameter remains.
    expect(screen.queryByRole("button", { name: /^Other parameters/ })).toBeNull();
    expect(screen.queryByRole("textbox", { name: "Maximum batched tokens" })).toBeNull();

    await user.clear(kvOffload);
    await user.type(kvOffload, "2048");
    await user.tab();
    expect(
      await within(hostMemory).findByText(
        "KV offloading size must be an integer from 0 to 1024 GiB.",
      ),
    ).toBeVisible();
    expect(kvOffload).toHaveAttribute("aria-invalid", "true");

    await user.clear(kvOffload);
    await user.type(kvOffload, "16");
    expect(within(hostMemory).queryByText(/must be an integer/)).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Next" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    // The empty context per request is left out for resolution to fill.
    expect(submittedValues(onSubmit)).toEqual([
      ["maximum-batched-tokens", 4096n],
      ["maximum-cudagraph-capture-size", 32n],
      ["kv-offloading-size-gib", 16n],
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

      await openAdvancedContext(user);
      expect(
        screen.getAllByRole("textbox").map((input) => [input.id, input.getAttribute("value")]),
      ).toEqual([
        ["engine-parameter-total-context", "32768"],
        ["engine-parameter-context-per-request", ""],
      ]);
      // An empty context per request stands for the total context, capped by the maximum.
      const perRequest = screen.getByRole("textbox", { name: "Context per request" });
      expect(perRequest).toHaveAttribute("placeholder", "32768");
      expect(screen.getByRole("slider", { name: "Context per request" })).toHaveAttribute(
        "aria-valuetext",
        "32,768 tokens",
      );

      await deployFromContext(user);
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

      await openAdvancedContext(user);
      const total = screen.getByRole("textbox", { name: "Total context" });
      const perRequest = screen.getByRole("textbox", { name: "Context per request" });

      // A typed context per request above the total raises the total once left.
      await user.type(perRequest, "65536");
      expect(total).toHaveValue("32768");
      await user.tab();
      expect(total).toHaveValue("65536");

      // A total preset below the context per request lowers it at once.
      await user.click(
        within(screen.getByRole("group", { name: "Total context presets" })).getByRole("button", {
          name: "8k",
        }),
      );
      expect(perRequest).toHaveValue("8192");
      expect(screen.queryByText(/must be at most the total context/)).not.toBeInTheDocument();

      await deployFromContext(user);
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

  it("draws the ninfer context sliders on one scale, stopping context per request at its maximum", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "ninfer-nvfp4" });

    await openAdvancedContext(user);
    const totalSlider = screen.getByRole("slider", { name: "Total context" });
    const perRequestSlider = screen.getByRole("slider", { name: "Context per request" });
    // 32768 tokens sits five elevenths along the log scale from 1024 to 2097152 tokens on both.
    expect(totalSlider).toHaveValue("455");
    expect(perRequestSlider).toHaveValue("455");

    // The end of the shared scale clamps context per request to its 262144 maximum, eight
    // elevenths along, and the total follows it there.
    fireEvent.change(perRequestSlider, { target: { value: "1000" } });
    expect(screen.getByRole("textbox", { name: "Context per request" })).toHaveValue("262144");
    expect(screen.getByRole("textbox", { name: "Total context" })).toHaveValue("262144");
    expect(perRequestSlider).toHaveValue("727");
    expect(totalSlider).toHaveValue("727");
    expect(presetLabels("Context per request")).toEqual(["8k", "32k", "64k", "128k", "256k"]);
  });

  it("stands an empty context per request for the maximum context when below the total", async () => {
    const user = setupUser();
    const model = clone(ModelSchema, runtimeSchemaModel);
    model.maximumContextTokens = 262144;
    model.configurations[0].maximumContextTokens = 16384;
    renderDialog({ initialProfileId: "q4-xl", model });

    await openAdvancedContext(user);
    const perRequest = screen.getByRole("textbox", { name: "Context per request" });
    expect(perRequest).toHaveAttribute("placeholder", "16384");
    expect(screen.getByText("Up to 16k (model maximum)")).toBeVisible();
    await user.click(preset("8k"));
    expect(perRequest).toHaveAttribute("placeholder", "8192");
  });
});

describe("ModelDeployDialog Runtime step", () => {
  it("leads with parallel requests and groups the ninfer host context as host memory", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "ninfer-nvfp4" });

    await openAdvancedRuntime(user);
    const parallel = screen.getAllByRole("textbox")[0];
    expect(parallel).toHaveAccessibleName("Parallel requests");
    expect(parallel).toHaveValue("4");
    expect(parallel).toHaveAccessibleDescription(
      "Requests generated at once; more wait in a queue. requests",
    );
    const hostMemory = screen.getByRole("region", { name: "Host memory" });
    const hostContext = within(hostMemory).getByRole("textbox", { name: "Host context memory" });
    expect(hostContext).toHaveValue("20480");
    expect(hostContext).toHaveAccessibleDescription("MiB");

    // No parameter is left for the other parameters section, and the context sizes sit on the
    // Context step.
    expect(screen.queryByRole("button", { name: /^Other parameters/ })).toBeNull();
    for (const name of ["Total context", "Context per request"])
      expect(screen.queryByRole("textbox", { name })).not.toBeInTheDocument();
  });

  it("keeps the llama.cpp prompt cache, artifact and custom arguments in collapsed sections", async () => {
    const user = setupUser();
    renderDialog({ initialProfileId: "q4-xl" });

    await openAdvancedRuntime(user);
    // llama.cpp declares no host-memory parameter; the prompt cache is its other parameter.
    expect(screen.queryByRole("region", { name: "Host memory" })).not.toBeInTheDocument();
    expect(sectionToggle("Other parameters (1)")).toHaveAttribute("aria-expanded", "false");
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

  it("expands and focuses an invalid value hidden in a collapsed section", async () => {
    const user = setupUser();
    const onSubmit = vi.fn();
    renderDialog({ initialProfileId: "q4-xl", onSubmit });
    const error = "Prompt cache memory must be an integer from 0 to 65536 MiB.";

    await openAdvancedRuntime(user);
    await user.click(sectionToggle("Other parameters (1)"));
    const promptCache = screen.getByRole("textbox", { name: "Prompt cache memory" });
    await user.clear(promptCache);
    await user.type(promptCache, "70000");
    await user.click(sectionToggle("Other parameters (1)"));
    await waitFor(() =>
      expect(
        screen.queryByRole("textbox", { name: "Prompt cache memory" }),
      ).not.toBeInTheDocument(),
    );

    // Leaving Runtime re-opens the section and focuses the field.
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Step 4 of 5 — Runtime")).toBeVisible();
    expect(sectionToggle("Other parameters (1)")).toHaveAttribute("aria-expanded", "true");
    await waitFor(() =>
      expect(screen.getByRole("textbox", { name: "Prompt cache memory" })).toHaveFocus(),
    );
    expect(screen.getByText(error)).toBeVisible();

    // With Advanced off, Deploy turns it back on and reveals the same field.
    await user.click(sectionToggle("Other parameters (1)"));
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
    // The new step takes focus in an effect after it renders; acting earlier loses that move.
    await waitFor(() =>
      expect(document.activeElement).toHaveAttribute("data-deployment-step-heading"),
    );
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    expect(await screen.findByText("Step 4 of 5 — Runtime")).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Advanced" })).toBeChecked();
    expect(sectionToggle("Other parameters (1)")).toHaveAttribute("aria-expanded", "true");
    const revealed = screen.getByRole("textbox", { name: "Prompt cache memory" });
    await waitFor(() => expect(revealed).toHaveFocus());
    expect(revealed).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByText(error)).toBeVisible();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  describe("KV cache precision", () => {
    it("is hidden when the configuration does not declare the option", async () => {
      const user = setupUser();
      renderDialog({ initialProfileId: "q4-xl" });

      await openAdvancedRuntime(user);
      expect(screen.queryByRole("combobox", { name: "KV cache precision" })).toBeNull();
    });

    it("starts from the configuration default and sends no overrides when untouched", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: "q4-xl", model: modelWithKvCachePrecision(), onSubmit });

      await openAdvancedRuntime(user);
      expect(kvCachePrecision()).toHaveValue("f16");
      expect(optionLabels(kvCachePrecision())).toEqual(["f16", "q8_0"]);

      await deployFromRuntime(user);
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

      await openAdvancedRuntime(user);
      expect(kvCachePrecision()).toHaveValue("q8_0");

      await user.selectOptions(kvCachePrecision(), "f16");
      await deployFromRuntime(user);
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

      await openAdvancedRuntime(user);
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

      await openAdvancedRuntime(user);
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

      await openAdvancedRuntime(user);
      expect(speculativeMethod()).toHaveValue("mtp");
      expect(optionLabels(speculativeMethod())).toEqual(["Off", "MTP"]);
      expect(speculativeCount()).toHaveValue("3");
      expect(speculativeCount()).toHaveAccessibleDescription("Draft tokens per step. 1–5");

      await deployFromRuntime(user);
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

      await openAdvancedRuntime(user);
      await choose(user);
      await deployFromRuntime(user);
      expect(screen.getByText(customized)).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual(expected);
    });

    it("refreshes the estimate with the chosen overrides", async () => {
      const user = setupUser();
      const onEstimate = vi.fn(() => Promise.resolve({}));
      renderDialog({ initialProfileId: "q4-xl", onEstimate });

      await openAdvancedRuntime(user);
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

      await openAdvancedRuntime(user);
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

      await openAdvancedRuntime(user);
      expect(speculativeMethod()).toHaveValue("");
      expect(screen.queryByRole("textbox", { name: "Speculative token count" })).toBeNull();

      await user.selectOptions(speculativeMethod(), "MTP");
      expect(speculativeCount()).toHaveValue("3");
      expect(speculativeCount()).not.toHaveAttribute("aria-invalid");
      await deployFromRuntime(user);
      expect(screen.getByText("Customized: Speculative decoding MTP, 3 tokens")).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual([["mtp", "token-count", 3n]]);
    });

    it("prefills a cleared count from the profile when picking a method again", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ initialProfileId: "q4-xl", model: modelWithProfileMtpCount(), onSubmit });

      await openAdvancedRuntime(user);
      expect(speculativeCount()).toHaveValue("4");
      await user.clear(speculativeCount());
      await user.selectOptions(speculativeMethod(), "Off");
      await user.selectOptions(speculativeMethod(), "MTP");
      expect(speculativeCount()).toHaveValue("4");

      await deployFromRuntime(user);
      expect(screen.queryByText(/Customized/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(submittedOverrides(onSubmit)).toEqual([]);
    });

    it("validates a kept count against the range of the newly picked method", async () => {
      const user = setupUser();
      renderDialog({ initialProfileId: "q4-xl", model: modelWithDflash() });
      const mtpError = "Speculative token count must be an integer from 1 to 5 tokens.";

      await openAdvancedRuntime(user);
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

      await openAdvancedRuntime(user);
      await user.selectOptions(speculativeMethod(), "DFlash");
      rerender(dialog({ initialProfileId: "q4-xl", onSubmit }));

      expect(speculativeMethod()).toHaveValue("mtp");
      expect(speculativeCount()).toHaveValue("3");
      await deployFromRuntime(user);
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

      await openAdvancedRuntime(user);
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
      expect(screen.getByText("Step 4 of 5 — Runtime")).toBeVisible();
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
      expect(screen.getByText("Step 4 of 5 — Runtime")).toBeVisible();
      expect(screen.queryByRole("combobox", { name: "Speculative decoding" })).toBeNull();
      expect(onSubmit).not.toHaveBeenCalled();
    });
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

async function openAdvancedContext(user: UserEvent) {
  await screen.findByText("Step 1 of 3 — Profile");
  await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 2 of 5 — Target")).toBeVisible();
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 3 of 5 — Context")).toBeVisible();
}

async function openAdvancedRuntime(user: UserEvent) {
  await openAdvancedContext(user);
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 4 of 5 — Runtime")).toBeVisible();
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

async function deployFromContext(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Next" }));
  await deployFromRuntime(user);
}

async function deployFromRuntime(user: UserEvent) {
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
      values: { parameterId: string; value: { value: unknown } }[];
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
