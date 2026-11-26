import { clone, create } from "@bufbuild/protobuf";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { type ComponentProps, StrictMode } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  ExecutionTargetsWatch,
  executionTargetsWatch,
} from "@/features/deployments/execution-targets-watch-test-support";
import { runningDeployment } from "@/features/memory/node-memory-test-support";
import { nvidiaEstimates } from "@/features/models/catalog/catalog-table-test-support";
import {
  dedicatedPreview,
  devicePool,
  exceeding,
  gib,
  hostPool,
  measured,
  mib,
  other,
  placementPreview,
  previewed,
} from "@/features/models/deploy/memory/model-deploy-memory-test-support";
import {
  kvOffloadModel,
  kvOffloadReleases,
  model,
  multiParameterModel,
  multiParameterReleases,
  releases,
  snapshot,
} from "@/features/models/model-test-support";
import {
  compatibilityLoader,
  compatibilityResponse,
  driverTooOld,
  notReporting,
  targetCompatibility,
} from "@/features/models/profiles/profile-compatibility-test-support";
import {
  runtimeSchemaModel,
  runtimeSchemaReleases,
} from "@/features/models/runtime/runtime-schema-test-support";
import { ModelSchema, type ParameterVisibilityOverride } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  ParameterValueSchema,
  ParameterValuesSchema,
  ParameterVisibility,
} from "@/gen/dotcore/catalog/v1/parameter_pb";
import { MemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import { ExactPlacementSchema } from "@/gen/dotcore/deployment/v1/placement_pb";
import {
  PortableModelLaunchPlanSchema,
  ResolvedLocalModelDeploymentSnapshotSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  LocalModelDeploymentAdvisory,
  ModelCompatibilityReasonCode,
  ModelCompatibilityStatus,
} from "@/gen/dotcore/model/v1/service_pb";
import { AcceleratorMemoryArchitecture } from "@/gen/dotcore/observation/v1/observation_pb";
import {
  AcceleratorResolutionStatus,
  type ConfiguredExecutionTarget,
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { productionTiming, setTiming } from "@/lib/timing";
import type { FakeResourceWatch } from "@/test/fake-resource-watch";
import { useFakeTimersWithWaits } from "@/test/fake-timers";
import { readFieldHelpFacts } from "@/test/field-help";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { ModelDeployDialog } from "./model-deploy-dialog";
import { goToStep, openAdvancedStep } from "./model-deploy-test-steps";
import { compatibilityFitEstimates } from "./resource-estimate-test-support";
import { type LocalResourceEstimate } from "./use-resource-estimate";

const unifiedKvOffloadWarning =
  "KV cache offload adds no capacity on this target. Its GPU already uses shared system memory, so offloaded cache takes memory from the same pool.";
const profileResetHint = "Changing the profile resets runtime settings to its defaults.";

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("ModelDeployDialog", () => {
  describe("steps", () => {
    it("asks only for the profile and deploy steps with one deployable target", async () => {
      renderDialog();

      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Review"]);
      expect(screen.getByRole("checkbox", { name: "Advanced" })).not.toBeChecked();
      expect(
        within(screen.getByTestId("deployment-dialog-footer-start")).getByRole("checkbox", {
          name: "Advanced",
        }),
      ).toBeVisible();
      expect(
        within(screen.getByTestId("deployment-dialog-actions"))
          .getAllByRole("button")
          .map((button) => button.textContent),
      ).toEqual(["Cancel", "Next"]);
    });

    it("asks for the target on the Model & machine step when more than one target is deployable", async () => {
      const user = setupUser();
      renderDialog({ listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()) });

      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Review"]);
      await waitFor(() =>
        expect(screen.getByRole("combobox", { name: "Machine" })).toHaveValue(
          "node / NVIDIA GeForce RTX 5090",
        ),
      );
      await pickTarget(user, /Worker/);
      await goToDeploy(user);
      expect(summaryRows()[0]).toEqual(["Runs on", "Worker / NVIDIA GB10"]);
    });

    it("adds Performance & memory and Command with Advanced, and never a Target step", async () => {
      const user = setupUser();
      renderDialog({
        engineReleases: multiParameterReleases,
        model: multiParameterModel,
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      await enableAdvanced(user);

      expect(stepLabels()).toEqual([
        "1. Model",
        "2. Context",
        "3. Performance",
        "4. Command",
        "5. Review",
      ]);
      expect(screen.getByText("Step 1 of 5 — Model & machine")).toBeVisible();
      expect(screen.getByRole("status")).toHaveTextContent("Advanced on: 5 steps");
      await goToDeploy(user);
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
    });

    it("shows Performance & memory only when the profile has advanced inputs", async () => {
      const user = setupUser();
      // The model's only parameter is basic and it declares no option.
      renderDialog();

      await screen.findByText("Step 1 of 3 — Model & machine");
      await enableAdvanced(user);
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Command", "4. Review"]);
    });

    it("opens with Advanced off every time", async () => {
      const user = setupUser();
      const { rerender } = renderDialog();

      await screen.findByText("Step 1 of 3 — Model & machine");
      await enableAdvanced(user);
      expect(screen.getByText("Step 1 of 4 — Model & machine")).toBeVisible();

      rerender(dialog({ open: false }));
      rerender(dialog({ open: true }));

      expect(screen.getByRole("checkbox", { name: "Advanced" })).not.toBeChecked();
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Review"]);
    });
  });

  describe("Advanced toggle", () => {
    it("keeps entered values and the current step when toggled", async () => {
      const user = setupUser();
      renderDialog();

      await openAdvancedStep(user, "Command");
      await user.click(screen.getByRole("button", { name: "Back" }));
      const context = screen.getByRole("textbox", { name: /Total context/ });
      await user.clear(context);
      await user.type(context, "16384");
      await goToDeploy(user);
      const route = screen.getByRole("textbox", { name: "Route name" });
      await user.clear(route);
      await user.type(route, "custom-route");

      // Review & deploy exists in both modes, so it stays current.
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();
      expect(screen.getByRole("status")).toHaveTextContent("Advanced off: 3 steps");
      expect(summaryRows()).toEqual([
        ["Runs on", "node / NVIDIA GeForce RTX 5090"],
        ["Profile", "Fast start"],
        ["Weights", "Q4_K_M"],
        ["Context", "16k per request · 16k total"],
      ]);
      expect(screen.getByRole("textbox", { name: "Route name" })).toHaveValue("custom-route");

      // It still exists, so it stays current; every step before it can be reached.
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByText("Step 4 of 4 — Review & deploy")).toBeVisible();
      expect(stepButton("Model & machine")).toBeEnabled();
      expect(stepButton("Context")).toBeEnabled();
      expect(stepButton("Command")).toBeEnabled();

      expect(screen.getByRole("textbox", { name: "Route name" })).toHaveValue("custom-route");
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));
      expect(screen.getByRole("textbox", { name: /Total context/ })).toHaveValue("16k");
    });

    it("falls forward to Review & deploy when the Command step vanishes", async () => {
      const user = setupUser();
      renderDialog();

      await openAdvancedStep(user, "Command");
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));

      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();
      await waitFor(() =>
        expect(screen.getByRole("heading", { level: 2, name: "Review & deploy" })).toHaveFocus(),
      );
    });

    it("returns advanced inputs and custom arguments to the profile's values when turned off", async () => {
      const user = setupUser();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      renderDialog({
        engineReleases: runtimeSchemaReleases,
        initialProfileId: "q4-xl",
        model: runtimeSchemaModel,
        onSubmit,
      });

      await openAdvancedStep(user);
      const promptCache = screen.getByRole("textbox", { name: "Prompt cache memory" });
      await user.clear(promptCache);
      await user.type(promptCache, "4096");
      await user.selectOptions(screen.getByRole("combobox", { name: "Speculative decoding" }), "");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.type(screen.getByRole("textbox", { name: "Custom arguments" }), "--temp 0.7");
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();
      expect(summaryRows().map(([label]) => label)).not.toContain("Advanced");
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
      const [submitted] = onSubmit.mock.calls[0];
      expect(submitted.configuration.customArgumentTokens).toEqual([]);
      expect(submitted.configuration.optionOverrides).toEqual([]);
      expect(
        submitted.configuration.engineParameterValues.values.find(
          ({ parameterId }) => parameterId === "prompt-cache-memory-mib",
        )?.value,
      ).toEqual({ case: "unsignedInteger", value: 8192n });

      // Back on, the advanced inputs show the profile's values.
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      await user.click(stepButton("Performance & memory"));
      expect(screen.getByRole("textbox", { name: "Prompt cache memory" })).toHaveValue("8 GiB");
      expect(screen.getByRole("combobox", { name: "Speculative decoding" })).toHaveValue("mtp");
    });

    it("follows the profile's visibility overrides on the Context and advanced steps", async () => {
      const user = setupUser();
      const overridden = clone(ModelSchema, multiParameterModel);
      overridden.profiles[0].engineParameterVisibilityOverrides = [
        { parameterId: "total-context", visibility: ParameterVisibility.HIDDEN },
        { parameterId: "enable-prefix-caching", visibility: ParameterVisibility.HIDDEN },
      ] as ParameterVisibilityOverride[];
      renderDialog({ engineReleases: multiParameterReleases, model: overridden });

      await screen.findByText("Step 1 of 3 — Model & machine");
      await enableAdvanced(user);
      // Prefix caching was the only advanced input, so Performance & memory has nothing to ask.
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Command", "4. Review"]);
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("This profile sets its context itself.")).toBeVisible();
      expect(screen.queryByText("Total context")).not.toBeInTheDocument();
      expect(screen.queryByRole("switch", { name: "Prefix caching" })).not.toBeInTheDocument();
    });

    it("shows parameters a profile makes basic on the Settings step after the context ones", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      const promoted = clone(ModelSchema, multiParameterModel);
      promoted.profiles[0].engineParameterVisibilityOverrides = [
        { parameterId: "parallel-requests", visibility: ParameterVisibility.BASIC },
        { parameterId: "enable-prefix-caching", visibility: ParameterVisibility.BASIC },
      ] as ParameterVisibilityOverride[];
      renderDialog({ engineReleases: multiParameterReleases, model: promoted, onSubmit });

      await openSettings(user);
      const total = screen.getByRole("textbox", { name: /Total context/ });
      const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
      const prefixCaching = screen.getByRole("switch", { name: "Prefix caching" });
      expect(parallel.compareDocumentPosition(total)).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
      expect(total.compareDocumentPosition(prefixCaching)).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
      expect(prefixCaching).toHaveAccessibleDescription("Caches reusable prompt prefixes.");
      await user.clear(parallel);
      await user.type(parallel, "2");
      await user.click(prefixCaching);

      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();
      expect(summaryRows()).toEqual([
        ["Runs on", "node / NVIDIA GeForce RTX 5090"],
        ["Profile", "NVFP4 text"],
        ["Weights", "NVFP4"],
        ["Context", "32k per request · 2 parallel · 32k total"],
        ["Prefix caching", "Enabled"],
      ]);

      // Performance & memory keeps only the advanced parameters, so with none left it doesn't show.
      await enableAdvanced(user);
      expect(stepLabels()).not.toContain("3. Performance");

      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() =>
        expect(onSubmit).toHaveBeenCalledWith({
          configuration: {
            customArgumentTokens: [],
            engineParameterValues: {
              values: [
                { parameterId: "total-context", value: { case: "unsignedInteger", value: 32768n } },
                { parameterId: "parallel-requests", value: { case: "unsignedInteger", value: 2n } },
                create(ParameterValueSchema, {
                  parameterId: "kv-cache-memory-bytes",
                  value: { case: "unsignedInteger", value: 7400000000n },
                }),
                { parameterId: "enable-prefix-caching", value: { case: "boolean", value: true } },
              ],
            },
            modelArtifactBindingId: "",
            optionOverrides: [],
          },
          exposure: { kind: { case: "direct", value: { routeName: "wide-model" } } },
          name: "Wide Model",
          profileId: "text",
          placement: create(ExactPlacementSchema, poolPlacement),
        }),
      );
    });

    it("reveals an invalid advanced parameter with Advanced instead of deploying", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      // A launch read back with a value the catalog no longer accepts, which basic mode can't show.
      renderDialog({
        engineReleases: runtimeSchemaReleases,
        initialConfiguration: {
          customArgumentTokens: [],
          engineParameterValues: create(ParameterValuesSchema, {
            values: [
              {
                parameterId: "prompt-cache-memory-mib",
                value: { case: "unsignedInteger", value: 3000000000n },
              },
            ],
          }),
          modelArtifactBindingId: "",
          optionOverrides: [],
        },
        initialProfileId: "q4-xl",
        model: runtimeSchemaModel,
        onSubmit,
      });

      await openSettings(user);
      await goToDeploy(user);
      // The new step takes focus in an effect after it renders; acting earlier loses that move.
      await waitFor(() =>
        expect(document.activeElement).toHaveAttribute("data-deployment-step-heading"),
      );
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      expect(await screen.findByText("Step 3 of 5 — Performance & memory")).toBeVisible();
      expect(screen.getByRole("checkbox", { name: "Advanced" })).toBeChecked();
      expect(screen.getByRole("status")).toHaveTextContent(
        "Advanced on to fix performance & memory settings",
      );
      const revealed = screen.getByRole("textbox", { name: "Prompt cache memory" });
      await waitFor(() => expect(revealed).toHaveFocus());
      expect(revealed).toHaveAttribute("aria-invalid", "true");
      expect(screen.getByText(/^Prompt cache memory must/)).toBeVisible();
      expect(onSubmit).not.toHaveBeenCalled();
    });
  });

  describe("Deploy pane", () => {
    it("names the deployment and summarizes the untouched defaults", async () => {
      const user = setupUser();
      renderDialog();

      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));

      expect(screen.getByRole("textbox", { name: "Deployment name" })).toHaveValue("Test Model");
      expect(summaryRows()).toEqual([
        ["Runs on", "node / NVIDIA GeForce RTX 5090"],
        ["Profile", "Fast start"],
        ["Weights", "Q4_K_M"],
        ["Context", "8k per request · 8k total"],
      ]);
    });

    it("flags the deployment name on blur only after it changes", async () => {
      const user = setupUser();
      renderDialog();

      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const name = screen.getByRole("textbox", { name: "Deployment name" });
      await user.click(name);
      await user.tab();
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 10));
      });
      expect(name).not.toHaveAttribute("aria-invalid");

      await user.type(name, "x");
      await user.clear(name);
      await user.tab();
      expect(await screen.findByText("Deployment name is required.")).toBeVisible();
      expect(name).toHaveAttribute("aria-invalid", "true");
    });

    it("shows the name error and focuses it when Deploy finds the name empty", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({ onSubmit });

      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const name = screen.getByRole("textbox", { name: "Deployment name" });
      await user.clear(name);
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      expect(name).toHaveAccessibleDescription("Deployment name is required.");
      expect(name).toHaveFocus();
      expect(onSubmit).not.toHaveBeenCalled();
    });

    it("lists each changed parameter and the other customized advanced settings", async () => {
      const user = setupUser();
      renderDialog({ engineReleases: multiParameterReleases, model: multiParameterModel });

      await openAdvancedStep(user);
      await user.click(screen.getByRole("switch", { name: "Prefix caching" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.type(screen.getByRole("textbox", { name: "Custom arguments" }), "--flag");
      await goToDeploy(user);

      expect(summaryRows()).toEqual([
        ["Runs on", "node / NVIDIA GeForce RTX 5090"],
        ["Profile", "NVFP4 text"],
        ["Weights", "NVFP4"],
        ["Context", "32k per request · 4 parallel · 32k total"],
        ["Prefix caching", "Enabled"],
        ["Advanced", "Customized: 1 custom argument"],
      ]);
    });

    it("keeps changed parallel requests across a profile change without the reset hint", async () => {
      const user = setupUser();
      renderDialog({
        engineReleases: runtimeSchemaReleases,
        initialProfileId: "q4-xl",
        model: runtimeSchemaModel,
      });

      await openSettings(user);
      const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
      await user.clear(parallel);
      await user.type(parallel, "2");
      await user.tab();
      await user.click(screen.getByRole("button", { name: "Back" }));

      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
      expect(screen.queryByText(profileResetHint)).not.toBeInTheDocument();
      await user.click(screen.getByRole("radio", { name: /Q4_K_XL without MTP/ }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveValue("2");
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows().find(([label]) => label === "Context")).toEqual([
        "Context",
        "32k per request · 2 parallel · 64k total",
      ]);
    });

    it("hints that a profile change resets customized runtime settings and keeps the context", async () => {
      const user = setupUser();
      renderDialog({
        engineReleases: runtimeSchemaReleases,
        initialProfileId: "q4-xl",
        model: runtimeSchemaModel,
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(screen.queryByText(profileResetHint)).not.toBeInTheDocument();
      await openAdvancedStep(user);
      const promptCache = screen.getByRole("textbox", { name: "Prompt cache memory" });
      await user.clear(promptCache);
      await user.type(promptCache, "4096");
      await user.click(screen.getByRole("button", { name: "Back" }));
      const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
      await user.clear(parallel);
      await user.type(parallel, "2");
      await user.tab();
      await user.click(screen.getByRole("button", { name: "Back" }));

      expect(screen.getByText(profileResetHint)).toBeVisible();
      await user.click(screen.getByRole("radio", { name: /Q4_K_XL without MTP/ }));
      await user.click(screen.getByRole("radio", { name: "Q4_K_XL" }));
      expect(screen.queryByText(profileResetHint)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveValue("2");
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("textbox", { name: "Prompt cache memory" })).toHaveValue("8 GiB");
    });
  });

  describe("profile step", () => {
    it("lists the profiles in the catalog table by memory, largest first, with the opener's estimates", async () => {
      renderDialog({
        profileEstimates: {
          estimates: nvidiaEstimates([
            ["model-1", "default", 6n * gib],
            ["model-1", "large", 9n * gib],
          ]),
          pending: false,
        },
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      const profiles = screen.getByRole("table", { name: "Model profiles" });
      expect(
        within(profiles)
          .getAllByRole("columnheader")
          .map((header) => header.textContent),
      ).toEqual(["Selected", "Profile", "Weights", "Memory", "Fits up to per request"]);
      expect(
        within(profiles)
          .getAllByRole("row")
          .slice(1)
          .map((row) => [
            row.getAttribute("aria-label"),
            ...within(row)
              .getAllByRole("cell")
              .slice(1)
              .map((cell) => cell.textContent),
          ]),
      ).toEqual([
        ["Large context", "Large context", "Q8_0", "9 GiB", "—"],
        ["Fast start", "Fast start", "Q4_K_M", "6 GiB", "—"],
      ]);
      expect(screen.getByRole("searchbox", { name: "Filter profiles" })).toBeVisible();
    });

    it("shows Intelligence from the View menu, and Reset hides it again", async () => {
      const user = setupUser();
      renderDialog();

      await screen.findByText("Step 1 of 3 — Model & machine");
      const profiles = screen.getByRole("table", { name: "Model profiles" });
      const headers = () =>
        within(profiles)
          .getAllByRole("columnheader")
          .map((header) => header.textContent);
      await user.click(screen.getByRole("button", { name: "View" }));
      const menu = await screen.findByRole("menu");
      expect(
        within(menu)
          .getAllByRole("menuitemcheckbox")
          .map((item) => [item.textContent, item.getAttribute("aria-checked")]),
      ).toEqual([
        ["Weights", "true"],
        ["Memory", "true"],
        ["Fits up to per request", "true"],
        ["Maximum context", "false"],
        ["Intelligence", "false"],
      ]);

      await user.click(within(menu).getByRole("menuitemcheckbox", { name: "Intelligence" }));
      await user.keyboard("{Escape}");
      expect(headers()).toEqual([
        "Selected",
        "Profile",
        "Weights",
        "Memory",
        "Fits up to per request",
        "Intelligence",
      ]);

      await user.click(screen.getByRole("button", { name: "Reset" }));
      expect(headers()).toEqual([
        "Selected",
        "Profile",
        "Weights",
        "Memory",
        "Fits up to per request",
      ]);
      expect(screen.queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();
    });

    it("preselects the requested profile and deploys it on the profile's weights", async () => {
      const user = setupUser();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      // Started from a launch on other weights, which the profile's replace.
      renderDialog({
        initialConfiguration: {
          customArgumentTokens: [],
          engineParameterValues: create(ParameterValuesSchema, {}),
          modelArtifactBindingId: "q4-binding",
          optionOverrides: [],
        },
        initialProfileId: "large",
        onSubmit,
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked();
      expect(screen.getByRole("radio", { name: "Fast start" })).not.toBeChecked();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
      const [submitted] = onSubmit.mock.calls[0];
      expect(submitted.profileId).toBe("large");
      expect(submitted.configuration.modelArtifactBindingId).toBe("");
    });

    it("selects a profile from its row and keeps the selection across steps", async () => {
      const user = setupUser();
      renderDialog();

      await screen.findByText("Step 1 of 3 — Model & machine");
      await user.click(screen.getByRole("row", { name: "Large context" }));
      expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked();
      expect(screen.getByRole("row", { name: "Large context" })).toHaveAttribute(
        "data-selected",
        "true",
      );
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()).toContainEqual(["Profile", "Large context"]);

      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));
      expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked();
    });

    it("disables the profile rows while a deployment is pending", async () => {
      renderDialog({ pending: true });

      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(screen.getByRole("radio", { name: "Fast start" })).toBeDisabled();
      expect(screen.getByRole("radio", { name: "Large context" })).toBeDisabled();
    });
  });

  describe("runs on", () => {
    it("shows only the profiles that run on the site behind a removable chip", async () => {
      const user = setupUser();
      const loader = compatibilityLoader(
        targetCompatibility(poolPlacement, {
          default: { advisories: [LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY] },
          large: { reason: driverTooOld },
        }),
      );
      renderDialog({ listProfileCompatibility: loader });

      expect(
        await screen.findByRole("button", { name: "Runs on filter: this site" }),
      ).toBeVisible();
      expect(profileRows()).toEqual(["Fast start"]);
      expect(
        within(screen.getByRole("row", { name: "Fast start" })).getByText(
          "KV cache offload adds no capacity on this target",
        ),
      ).toHaveClass("text-warning-foreground");
      expect(screen.getByText("1 of 2 profiles")).toBeVisible();
      expect(loader).toHaveBeenCalledWith("model-1");

      await user.click(screen.getByRole("button", { name: "Remove Runs on filter" }));
      expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
      expect(profileRows()).toEqual(["Fast start", "Large context"]);
      expect(screen.getByRole("radio", { name: "Large context" })).toBeDisabled();
      expect(
        within(screen.getByRole("row", { name: "Large context" })).getByText(
          "Needs NVIDIA driver 575 or later (has 560)",
        ),
      ).toHaveClass("text-destructive");
    });

    it("preselects the first profile by name that runs", async () => {
      renderDialog({
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: { reason: driverTooOld }, large: {} }),
        ),
      });

      await waitFor(() =>
        expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked(),
      );
      expect(profileRows()).toEqual(["Large context"]);
    });

    it("preselects the profile with the most memory that runs when none is requested", async () => {
      renderDialog({
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: {} }),
        ),
        profileEstimates: {
          estimates: nvidiaEstimates([
            ["model-1", "default", 6n * gib],
            ["model-1", "large", 9n * gib],
          ]),
          pending: false,
        },
      });

      await waitFor(() =>
        expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked(),
      );
      expect(profileRows()).toEqual(["Large context", "Fast start"]);
    });

    it("falls back to the first profile that runs when the requested one can't", async () => {
      renderDialog({
        initialProfileId: "large",
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: { reason: driverTooOld } }),
        ),
      });

      await waitFor(() => expect(screen.getByRole("radio", { name: "Fast start" })).toBeChecked());
    });

    it("deploys to the only target that runs the profile without asking", async () => {
      const user = setupUser();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      renderDialog({
        initialProfileId: "large",
        listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()),
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: { reason: driverTooOld } }),
          targetCompatibility(workerPlacement, { default: {}, large: {} }),
        ),
        onSubmit,
      });

      await screen.findByRole("button", { name: "Runs on filter: this site" });
      expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked();
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Review"]);

      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()[0]).toEqual(["Runs on", "Worker / NVIDIA GB10"]);
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0][0].placement).toMatchObject(workerPlacement);
    });

    it("starts on the opener's target among those that run the profile", async () => {
      const user = setupUser();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      renderDialog({
        initialPlacement: create(ExactPlacementSchema, workerPlacement),
        listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()),
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: {} }),
          targetCompatibility(workerPlacement, { default: {}, large: {} }),
        ),
        onSubmit,
      });

      await screen.findByRole("button", { name: "Runs on filter: this site" });
      expect(screen.getByRole("combobox", { name: "Machine" })).toHaveValue("Worker / NVIDIA GB10");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()[0]).toEqual(["Runs on", "Worker / NVIDIA GB10"]);
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0][0].placement).toMatchObject(workerPlacement);
    });

    it("asks for the target among those that run the profile", async () => {
      const user = setupUser();
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget(), labTarget()),
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: {} }),
          targetCompatibility(workerPlacement, { default: {}, large: {} }),
          targetCompatibility(labPlacement, { default: { reason: driverTooOld }, large: {} }),
        ),
      });

      await screen.findByRole("button", { name: "Runs on filter: this site" });
      const picker = screen.getByRole("combobox", { name: "Machine" });
      expect(picker).toHaveValue("");
      await user.click(picker);
      expect(
        screen.getAllByRole("option").map((option) => option.getAttribute("aria-label")),
      ).toEqual(["node / NVIDIA GeForce RTX 5090 linux/amd64", "Worker / NVIDIA GB10 linux/amd64"]);
      await user.keyboard("{Escape}");

      // Next asks for a target before going on.
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
      expect(screen.getByText("Execution target is required.")).toBeVisible();

      // Advanced lists every target, with those that can't run the profile disabled.
      await enableAdvanced(user);
      await user.click(picker);
      expect(
        screen.getByRole("option", {
          name: /^Lab \/ 2× NVIDIA GeForce RTX 4090 — Needs NVIDIA driver 575 or later \(has 560\)/,
        }),
      ).toHaveAttribute("aria-disabled", "true");
      expect(screen.getByRole("option", { name: /^Worker/ })).not.toHaveAttribute(
        "aria-disabled",
        "true",
      );
    });

    it("keeps a chosen target that runs a newly selected profile and clears one that doesn't", async () => {
      const user = setupUser();
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget(), labTarget()),
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: {} }),
          targetCompatibility(workerPlacement, { default: {}, large: {} }),
          targetCompatibility(labPlacement, { default: {}, large: { reason: driverTooOld } }),
        ),
      });

      await screen.findByRole("button", { name: "Runs on filter: this site" });
      await pickTarget(user, /^Worker/);
      await user.click(screen.getByRole("row", { name: "Large context" }));
      expect(screen.getByRole("combobox", { name: "Machine" })).toHaveValue("Worker / NVIDIA GB10");

      await user.click(screen.getByRole("row", { name: "Fast start" }));
      await pickTarget(user, /^Lab/);
      await user.click(screen.getByRole("row", { name: "Large context" }));
      expect(screen.getByRole("combobox", { name: "Machine" })).toHaveValue("");
    });

    it("narrows Runs on to one machine and moves a selection that can't run there", async () => {
      const user = setupUser();
      renderDialog({
        initialProfileId: "large",
        listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()),
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: {} }),
          targetCompatibility(workerPlacement, { default: {}, large: { reason: driverTooOld } }),
        ),
      });

      await narrowRunsOn(user, /^Worker/);
      expect(screen.getByRole("button", { name: "Runs on filter: Worker" })).toBeVisible();
      expect(profileRows()).toEqual(["Fast start"]);
      expect(screen.getByRole("radio", { name: "Fast start" })).toBeChecked();
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Review"]);

      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()[0]).toEqual(["Runs on", "Worker / NVIDIA GB10"]);
    });

    it("asks among one machine's targets when the chip names a machine with several", async () => {
      const user = setupUser();
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(
          poolTarget({ id: "pool-a", name: "NVIDIA GeForce RTX 5090 · GPU 0" }),
          poolTarget({ id: "pool-b", name: "NVIDIA GeForce RTX 5090 · GPU 1", siteManager: false }),
          workerTarget(),
        ),
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(
            { executionTargetId: "pool-a", siteNodeId: "node" },
            { default: {}, large: {} },
          ),
          targetCompatibility(
            { executionTargetId: "pool-b", siteNodeId: "node" },
            { default: {}, large: {} },
          ),
          targetCompatibility(workerPlacement, { default: {}, large: {} }),
        ),
      });

      await narrowRunsOn(user, /^node/);
      await user.click(screen.getByRole("combobox", { name: "Machine" }));
      expect(
        screen.getAllByRole("option").map((option) => option.getAttribute("aria-label")),
      ).toEqual([
        "node / NVIDIA GeForce RTX 5090 · GPU 0 linux/amd64",
        "node / NVIDIA GeForce RTX 5090 · GPU 1 linux/amd64",
      ]);
    });

    it("lists a machine's targets in GPU order, whatever their IDs' order", async () => {
      const user = setupUser();
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(
          poolTarget({ id: "pool-a", name: "NVIDIA GeForce RTX 5090 · GPU 1" }),
          poolTarget({ id: "pool-b", name: "NVIDIA GeForce RTX 4090 · GPU 0" }),
        ),
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      await enableAdvanced(user);
      await user.click(screen.getByRole("combobox", { name: "Machine" }));
      expect(
        screen.getAllByRole("option").map((option) => option.getAttribute("aria-label")),
      ).toEqual([
        "node / NVIDIA GeForce RTX 4090 · GPU 0 linux/amd64",
        "node / NVIDIA GeForce RTX 5090 · GPU 1 linux/amd64",
      ]);
    });

    it("keeps the steps ahead when a refresh changes which targets run the profile", async () => {
      setTiming(productionTiming);
      vi.useFakeTimers();
      const loader = vi
        .fn((modelId: string) =>
          Promise.resolve(
            compatibilityResponse(
              modelId,
              targetCompatibility(poolPlacement, { default: {}, large: {} }),
              targetCompatibility(workerPlacement, { default: {}, large: {} }),
            ),
          ),
        )
        .mockImplementationOnce((modelId) =>
          Promise.resolve(
            compatibilityResponse(
              modelId,
              targetCompatibility(poolPlacement, { default: {}, large: {} }),
              targetCompatibility(workerPlacement, { default: { reason: driverTooOld } }),
            ),
          ),
        );
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()),
        listProfileCompatibility: loader,
      });
      await settle();
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Review"]);
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.profileCompatibilityRefreshMs);
      });
      expect(loader).toHaveBeenCalledTimes(2);
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Review"]);
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();

      fireEvent.click(screen.getByRole("button", { name: "Back" }));
      fireEvent.click(screen.getByRole("button", { name: "Back" }));
      expect(stepLabels()).toEqual(["1. Model", "2. Context", "3. Review"]);
    });

    it("blocks a profile that runs only on machines that can't deploy right now", async () => {
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(
          poolTarget({ available: false }),
          workerTarget(),
        ),
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: {} }),
          targetCompatibility(workerPlacement, { default: { reason: driverTooOld }, large: {} }),
        ),
      });

      await waitFor(() =>
        expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked(),
      );
      expect(screen.getByRole("radio", { name: "Fast start" })).toBeDisabled();
      expect(
        within(screen.getByRole("row", { name: "Fast start" })).getByText(
          "node can't deploy models right now",
        ),
      ).toBeVisible();
    });

    it("clears a selection a search hides and waits for the operator to pick one", async () => {
      const user = setupUser();
      renderDialog({
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: {} }),
        ),
      });

      await waitFor(() => expect(screen.getByRole("radio", { name: "Fast start" })).toBeChecked());
      const search = screen.getByRole("searchbox", { name: "Filter profiles" });
      await user.type(search, "Large");
      expect(screen.getByRole("radio", { name: "Large context" })).not.toBeChecked();
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
      expect(screen.getByText("Select a model profile.")).toBeVisible();

      await user.clear(search);
      expect(screen.getByRole("radio", { name: "Fast start" })).not.toBeChecked();
      await user.click(screen.getByRole("row", { name: "Large context" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()).toContainEqual(["Profile", "Large context"]);
    });

    it("never preselects a profile the search hides when the chip changes", async () => {
      const user = setupUser();
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()),
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, { default: {}, large: { reason: driverTooOld } }),
          targetCompatibility(workerPlacement, { default: { reason: driverTooOld }, large: {} }),
        ),
      });

      await waitFor(() => expect(screen.getByRole("radio", { name: "Fast start" })).toBeChecked());
      await user.type(screen.getByRole("searchbox", { name: "Filter profiles" }), "Fast");
      await narrowRunsOn(user, /^Worker/);
      expect(profileRows()).toEqual(["No profiles match. Reset filters"]);

      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
      expect(screen.getByText("Select a model profile.")).toBeVisible();
    });

    it("places the remaining target after a chosen one turns ineligible", async () => {
      const user = setupUser();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      // The watched targets still show the worker online when the fresh read before Deploy finds it
      // offline.
      renderDialog(
        {
          listConfiguredExecutionTargets: targetsLoader(
            poolTarget(),
            workerTarget({ available: false }),
          ),
          listProfileCompatibility: compatibilityLoader(
            targetCompatibility(poolPlacement, { default: {}, large: {} }),
            targetCompatibility(workerPlacement, { default: {}, large: {} }),
          ),
          onSubmit,
        },
        { watch: executionTargetsWatch(targetsLoader(poolTarget(), workerTarget())) },
      );

      await screen.findByRole("button", { name: "Runs on filter: this site" });
      await pickTarget(user, /^Worker/);
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      expect(
        await screen.findByText(
          "Worker / NVIDIA GB10 is no longer eligible to run this deployment.",
        ),
      ).toBeVisible();
      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
      expect(screen.queryByRole("heading", { name: "Machine" })).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()[0]).toEqual(["Runs on", "node / NVIDIA GeForce RTX 5090"]);
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0][0].placement).toMatchObject(poolPlacement);
    });

    it("keeps the automatic target when a refresh adds another that runs the profile", async () => {
      setTiming(productionTiming);
      vi.useFakeTimers();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      const loader = vi
        .fn((modelId: string) =>
          Promise.resolve(
            compatibilityResponse(
              modelId,
              targetCompatibility(poolPlacement, { default: {}, large: {} }),
              targetCompatibility(workerPlacement, { default: {}, large: {} }),
            ),
          ),
        )
        .mockImplementationOnce((modelId) =>
          Promise.resolve(
            compatibilityResponse(
              modelId,
              targetCompatibility(poolPlacement, { default: {}, large: {} }),
              targetCompatibility(workerPlacement, { default: { reason: driverTooOld } }),
            ),
          ),
        );
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()),
        listProfileCompatibility: loader,
        onSubmit,
      });
      await settle();
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()[0]).toEqual(["Runs on", "node / NVIDIA GeForce RTX 5090"]);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.profileCompatibilityRefreshMs);
      });
      expect(loader).toHaveBeenCalledTimes(2);
      expect(summaryRows()[0]).toEqual(["Runs on", "node / NVIDIA GeForce RTX 5090"]);
      fireEvent.click(screen.getByRole("button", { name: "Deploy" }));
      await settle();
      expect(onSubmit).toHaveBeenCalledOnce();
      expect(onSubmit.mock.calls[0][0].placement).toMatchObject(poolPlacement);
    });

    it("says why no profile runs on the site and offers to show all", async () => {
      const user = setupUser();
      renderDialog({ listProfileCompatibility: compatibilityLoader(notReporting(poolPlacement)) });

      const profiles = await screen.findByRole("table", { name: "Model profiles" });
      expect(await within(profiles).findByText("No profile runs here")).toBeVisible();
      expect(within(profiles).getByText("node isn't reporting")).toBeVisible();
      expect(within(profiles).queryByRole("button", { name: "Change target" })).toBeNull();

      await user.click(within(profiles).getByRole("button", { name: "Show all profiles" }));
      expect(profileRows()).toEqual(["Fast start", "Large context"]);
      expect(screen.getByRole("radio", { name: "Fast start" })).toBeDisabled();
      expect(screen.getByRole("radio", { name: "Fast start" })).not.toBeChecked();
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("No profile can run here. node isn't reporting.")).toBeVisible();
    });

    it("keeps a selection that stops running on refresh and blocks with the reason", async () => {
      setTiming(productionTiming);
      vi.useFakeTimers();
      const loader = vi
        .fn((modelId: string) =>
          Promise.resolve(
            compatibilityResponse(
              modelId,
              targetCompatibility(poolPlacement, { default: {}, large: { reason: driverTooOld } }),
            ),
          ),
        )
        .mockImplementationOnce((modelId) =>
          Promise.resolve(
            compatibilityResponse(
              modelId,
              targetCompatibility(poolPlacement, { default: {}, large: {} }),
            ),
          ),
        );
      renderDialog({ initialProfileId: "large", listProfileCompatibility: loader });
      await settle();
      expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.profileCompatibilityRefreshMs);
      });
      expect(loader).toHaveBeenCalledTimes(2);
      expect(profileRows()).toEqual(["Fast start"]);
      expect(screen.getByRole("radio", { name: "Fast start" })).not.toBeChecked();
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
      expect(
        screen.getByText(
          "Large context can't run here. Needs NVIDIA driver 575 or later (has 560).",
        ),
      ).toBeVisible();
    });

    it("preselects once a later read reports a profile that runs", async () => {
      setTiming(productionTiming);
      vi.useFakeTimers();
      const loader = vi
        .fn((modelId: string) =>
          Promise.resolve(
            compatibilityResponse(
              modelId,
              targetCompatibility(poolPlacement, { default: {}, large: {} }),
            ),
          ),
        )
        .mockImplementationOnce((modelId) =>
          Promise.resolve(compatibilityResponse(modelId, notReporting(poolPlacement))),
        );
      renderDialog({ listProfileCompatibility: loader });
      await settle();
      expect(screen.getByText("No profile runs here")).toBeVisible();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.profileCompatibilityRefreshMs);
      });
      expect(screen.getByRole("radio", { name: "Fast start" })).toBeChecked();
    });

    it("holds Deploy on the profile step with the reason once the selection can't run", async () => {
      setTiming(productionTiming);
      vi.useFakeTimers();
      const onSubmit = vi.fn();
      const loader = vi
        .fn((modelId: string) =>
          Promise.resolve(compatibilityResponse(modelId, notReporting(poolPlacement))),
        )
        .mockImplementationOnce((modelId) =>
          Promise.resolve(
            compatibilityResponse(
              modelId,
              targetCompatibility(poolPlacement, { default: {}, large: {} }),
            ),
          ),
        );
      renderDialog({ listProfileCompatibility: loader, onSubmit });
      await settle();
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.profileCompatibilityRefreshMs);
      });
      fireEvent.click(screen.getByRole("button", { name: "Deploy" }));
      await settle();

      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
      expect(screen.getByText("Fast start can't run here. node isn't reporting.")).toBeVisible();
      expect(onSubmit).not.toHaveBeenCalled();
    });

    it("says so when the read evaluates no target", async () => {
      renderDialog({ listProfileCompatibility: compatibilityLoader() });

      expect(
        await screen.findByText("Couldn't check which profiles can run on this site."),
      ).toBeVisible();
      expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
      expect(profileRows()).toEqual(["Fast start", "Large context"]);
    });

    it("lists every profile and still deploys when the read fails", async () => {
      const user = setupUser();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      renderDialog({
        initialProfileId: "large",
        listProfileCompatibility: () => Promise.reject(new Error("unavailable")),
        onSubmit,
      });

      expect(
        await screen.findByText("Couldn't check which profiles can run on this site."),
      ).toBeVisible();
      expect(screen.queryByRole("button", { name: /^Runs on filter/ })).not.toBeInTheDocument();
      expect(profileRows()).toEqual(["Fast start", "Large context"]);
      expect(screen.getByRole("radio", { name: "Fast start" })).toBeEnabled();
      expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked();

      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0][0].profileId).toBe("large");
    });
  });

  describe("execution targets", () => {
    it("preselects the default target and names targets by host", async () => {
      const user = setupUser();
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(
          poolTarget({ id: "pool-a", name: "NVIDIA GeForce RTX 5090 · GPU 0", siteManager: false }),
          poolTarget({ id: "pool-b", name: "NVIDIA GeForce RTX 5090 · GPU 1", siteManager: true }),
          poolTarget({ id: "pool-c", available: false, name: "NVIDIA GeForce RTX 5090 · GPU 2" }),
        ),
      });

      const picker = await screen.findByRole("combobox", { name: "Machine" });
      await waitFor(() => expect(picker).toHaveValue("node / NVIDIA GeForce RTX 5090 · GPU 1"));

      await user.click(picker);
      expect(
        screen.getByRole("option", {
          name: "node / NVIDIA GeForce RTX 5090 · GPU 0 linux/amd64",
        }),
      ).not.toHaveAttribute("aria-disabled", "true");
      expect(
        screen.getByRole("option", {
          name: /^node \/ NVIDIA GeForce RTX 5090 · GPU 2 — Runtime is unavailable\./,
        }),
      ).toHaveAttribute("aria-disabled", "true");
    });

    it("tells apart targets with the same ID on different nodes", async () => {
      const user = setupUser();
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(
          poolTarget({ id: "pool" }),
          create(ConfiguredExecutionTargetSchema, {
            ...poolTarget({ id: "pool", name: "Apple M3 Max", siteManager: false }),
            siteNodeId: "worker",
            siteNodeDisplayName: "Worker",
          }),
        ),
      });

      const picker = await screen.findByRole("combobox", { name: "Machine" });
      await waitFor(() => expect(picker).toHaveValue("node / NVIDIA GeForce RTX 5090"));

      await user.click(picker);
      expect(
        screen.getAllByRole("option").map((option) => option.getAttribute("aria-label")),
      ).toEqual([
        "node / NVIDIA GeForce RTX 5090 linux/amd64",
        "Worker / Apple M3 Max linux/amd64",
      ]);
      await user.click(screen.getByRole("option", { name: "Worker / Apple M3 Max linux/amd64" }));

      expect(picker).toHaveValue("Worker / Apple M3 Max");
    });

    it("filters the target list only from eight targets up", async () => {
      const user = setupUser();
      const targets = (count: number) =>
        Array.from({ length: count }, (_, index) =>
          poolTarget({
            id: `pool-${index}`,
            name: `NVIDIA GeForce RTX 5090 · GPU ${index}`,
            siteManager: index === 0,
          }),
        );
      const first = renderDialog({ listConfiguredExecutionTargets: targetsLoader(...targets(7)) });

      expect(await screen.findByRole("combobox", { name: "Machine" })).toHaveAttribute("readonly");
      first.unmount();

      renderDialog({ listConfiguredExecutionTargets: targetsLoader(...targets(8)) });
      const searchable = await screen.findByRole("combobox", { name: "Machine" });
      expect(searchable).not.toHaveAttribute("readonly");
      await user.click(screen.getByRole("button", { name: "Clear Machine" }));
      await user.type(searchable, "GPU 7");
      expect(
        screen.getAllByRole("option").map((option) => option.getAttribute("aria-label")),
      ).toEqual(["node / NVIDIA GeForce RTX 5090 · GPU 7 linux/amd64"]);
    });

    it("shows no machine section for the only target, in advanced mode too", async () => {
      const user = setupUser();
      renderDialog();

      await screen.findByText("Step 1 of 3 — Model & machine");
      await enableAdvanced(user);

      expect(screen.queryByRole("combobox", { name: "Machine" })).not.toBeInTheDocument();
      expect(screen.queryByRole("heading", { name: "Machine" })).not.toBeInTheDocument();
      expect(screen.getByRole("heading", { name: "Profile" })).toBeVisible();
      for (const fact of [
        "Target ID",
        "Platform",
        "Configured devices",
        "Workload kind",
        "Connection freshness",
        "Runtime availability",
      ])
        expect(screen.queryByText(fact)).not.toBeInTheDocument();
    });

    it("delays execution-target feedback while disabling the control immediately", async () => {
      setTiming(productionTiming);
      vi.useFakeTimers();
      let resolvePools!: (value: { executionTargets: ConfiguredExecutionTarget[] }) => void;
      const listTargets = vi.fn(
        () =>
          new Promise<{ executionTargets: ConfiguredExecutionTarget[] }>((resolve) => {
            resolvePools = resolve;
          }),
      );

      renderDialog(
        { listConfiguredExecutionTargets: listTargets },
        { watch: executionTargetsWatch(listTargets) },
      );

      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
      fireEvent.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByRole("combobox", { name: "Machine" })).toBeDisabled();
      expect(screen.queryByText("Loading configured execution targets…")).not.toBeInTheDocument();
      expect(screen.getByRole("combobox", { name: "Machine" })).not.toHaveAccessibleDescription();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.loadingRevealMs);
      });
      expect(screen.getByText("Loading configured execution targets…")).toBeVisible();
      expect(screen.getByRole("combobox", { name: "Machine" })).toHaveAccessibleDescription(
        "Loading configured execution targets…",
      );

      await act(async () => {
        resolvePools({ executionTargets: [poolTarget(), workerTarget()] });
        await Promise.resolve();
      });
      expect(screen.getByRole("combobox", { name: "Machine" })).toBeEnabled();
      expect(screen.getByText("Loading configured execution targets…")).toBeVisible();
      expect(screen.getByRole("combobox", { name: "Machine" })).toHaveAccessibleDescription(
        "Loading configured execution targets…",
      );

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.loadingMinimumVisibleMs);
      });
      expect(screen.queryByText("Loading configured execution targets…")).not.toBeInTheDocument();
      expect(screen.getByRole("combobox", { name: "Machine" })).not.toHaveAccessibleDescription();
    });

    it("follows the watched target availability, pauses while hidden, and recovers the default target", async () => {
      vi.useFakeTimers();
      let visibility: DocumentVisibilityState = "visible";
      vi.spyOn(document, "visibilityState", "get").mockImplementation(() => visibility);
      const listTargets = vi
        .fn()
        .mockResolvedValueOnce({ executionTargets: [poolTarget(), workerTarget()] })
        .mockResolvedValueOnce({
          executionTargets: [poolTarget({ available: false }), workerTarget({ available: false })],
        })
        .mockResolvedValue({ executionTargets: [poolTarget(), workerTarget()] });
      const watch = executionTargetsWatch(listTargets);

      renderDialog({}, { watch });
      await settle();
      fireEvent.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByRole("combobox", { name: "Machine" })).toHaveValue(
        "node / NVIDIA GeForce RTX 5090",
      );

      visibility = "hidden";
      fireEvent(document, new Event("visibilitychange"));
      await settle();
      expect(watch.watchedKinds()).toEqual([]);

      visibility = "visible";
      fireEvent(document, new Event("visibilitychange"));
      await settle();
      expect(watch.watchedKinds()).toEqual(["executionTargets"]);
      expect(screen.getByRole("combobox", { name: "Machine" })).toHaveValue("");
      expect(
        screen.getByText("Configured targets are visible, but none are deployable."),
      ).toBeVisible();
      expect(screen.getByRole("combobox", { name: "Machine" })).toHaveAccessibleDescription(
        "Configured targets are visible, but none are deployable.",
      );

      await act(async () => {
        await watch.tick();
      });
      expect(
        screen.queryByText("Configured targets are visible, but none are deployable."),
      ).not.toBeInTheDocument();
      expect(screen.getByRole("combobox", { name: "Machine" })).not.toHaveAccessibleDescription();
      // The operator never chose, so the default target comes back with its availability.
      expect(screen.getByRole("combobox", { name: "Machine" })).toHaveValue(
        "node / NVIDIA GeForce RTX 5090",
      );
    });

    it("revalidates the selected exact target immediately before model submission", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      // The watched targets still show the target online when the fresh read finds it offline.
      const listTargets = vi
        .fn()
        .mockResolvedValue({ executionTargets: [poolTarget({ available: false })] });
      renderDialog(
        { listConfiguredExecutionTargets: listTargets, onSubmit },
        { watch: executionTargetsWatch(targetsLoader(poolTarget())) },
      );

      await screen.findByText("Step 1 of 3 — Model & machine");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(listTargets).toHaveBeenCalledOnce());
      expect(onSubmit).not.toHaveBeenCalled();
      // The target was placed automatically, so the operator returns to the profile.
      expect(
        await screen.findByText(
          "node / NVIDIA GeForce RTX 5090 is no longer eligible to run this deployment.",
        ),
      ).toBeVisible();
      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
      expect(
        screen.getByText("No configured execution target can deploy models right now."),
      ).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
    });

    it("submits a connected worker target and keeps a stale worker target disabled", async () => {
      const onSubmit = vi.fn();
      const user = setupUser();
      const worker = (siteNodeId: string, connectionState: NodeObservationState) =>
        create(ConfiguredExecutionTargetSchema, {
          displayName: "NVIDIA GB10",
          executionTargetId: `${siteNodeId}-gpu`,
          siteNodeId,
          siteNodeDisplayName: `Name of ${siteNodeId}`,
          platformLabel: "linux/arm64",
          acceleratorDeviceIds: ["GPU-remote"],
          workloadKind: ExecutionTargetWorkloadKind.MODEL,
          availability: {
            connectionState,
            runtimeAvailability: { availability: { case: "available", value: {} } },
          },
        });
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(
          worker("connected-worker", NodeObservationState.CONNECTED),
          poolTarget(),
          worker("stale-worker", NodeObservationState.STALE),
        ),
        onSubmit,
      });

      const executionTarget = await screen.findByRole("combobox", { name: "Machine" });
      await waitFor(() => expect(executionTarget).toHaveValue("node / NVIDIA GeForce RTX 5090"));

      await user.click(executionTarget);
      expect(
        screen.getByRole("option", {
          name: /^Name of stale-worker \/ NVIDIA GB10 — Machine observation is overdue\. — .*Stale/,
        }),
      ).toHaveAttribute("aria-disabled", "true");
      await user.click(screen.getByRole("option", { name: /^Name of connected-worker/ }));
      expect(executionTarget).toHaveValue("Name of connected-worker / NVIDIA GB10");
      await enableAdvanced(user);
      // Advanced mode lists the same picker without the target's facts.
      expect(screen.queryByText("Configured devices")).not.toBeInTheDocument();

      await goToDeploy(user);
      expect(screen.getByText("Step 4 of 4 — Review & deploy")).toBeVisible();
      expect(summaryRows()[0]).toEqual(["Runs on", "Name of connected-worker / NVIDIA GB10"]);
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0]?.[0]).toMatchObject({
        placement: create(ExactPlacementSchema, {
          siteNodeId: "connected-worker",
          executionTargetId: "connected-worker-gpu",
        }),
      });
    });

    it("updates execution-target guidance after the selected value is cleared", async () => {
      const user = setupUser();
      renderDialog({ listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()) });

      const executionTarget = await screen.findByRole("combobox", { name: "Machine" });
      await waitFor(() => expect(executionTarget).toHaveValue("node / NVIDIA GeForce RTX 5090"));
      expect(executionTarget).not.toHaveAccessibleDescription();

      await user.click(screen.getByRole("button", { name: "Clear Machine" }));
      expect(executionTarget).toHaveValue("");
      expect(executionTarget).not.toHaveAccessibleDescription();
      expect(executionTarget).toHaveAttribute("placeholder", "Choose a machine…");
      expect(screen.queryByText("Execution target is required.")).not.toBeInTheDocument();
      await user.tab();
      expect(await screen.findByText("Execution target is required.")).toBeVisible();

      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(executionTarget).toHaveAttribute("aria-invalid", "true");
      expect(executionTarget).toHaveFocus();
      expect(screen.getByText("Step 1 of 3 — Model & machine")).toBeVisible();
    });
  });

  describe("notices", () => {
    it("blocks Deploy when no execution targets are configured", async () => {
      const user = setupUser();
      renderDialog({ listConfiguredExecutionTargets: targetsLoader() });

      await screen.findByText("Step 1 of 3 — Model & machine");
      const notice = "No model execution targets are configured. Configure one before deploying.";
      expect(screen.getByText(notice)).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText(notice)).toBeVisible();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
    });

    it("blocks Deploy and lists the reason when no target is deployable", async () => {
      const user = setupUser();
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(poolTarget({ available: false })),
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      const notice = "No configured execution target can deploy models right now.";
      expect(screen.getByText(notice)).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText(notice)).toBeVisible();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();

      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));
      expect(screen.queryByRole("heading", { name: "Machine" })).not.toBeInTheDocument();
      expect(
        screen.getByText("node / NVIDIA GeForce RTX 5090 — Runtime is unavailable."),
      ).toBeVisible();
    });

    it("shows a taken derived route on the route field as the step opens and clears it on rename", async () => {
      const user = setupUser();
      renderDialog({ existingRouteNames: ["test-model"] });

      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const route = screen.getByRole("textbox", { name: "Route name" });
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
      expect(route).toHaveAttribute("aria-invalid", "true");
      expect(route).toHaveAccessibleDescription(
        'The model name callers send in requests. It follows the deployment name until you change it. An inference route named "test-model" already exists. Change the route or the deployment name.',
      );

      await user.type(screen.getByRole("textbox", { name: "Deployment name" }), " 2");
      expect(screen.queryByText(/already exists/)).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
    });

    it("clears a taken derived route when the route is edited", async () => {
      const user = setupUser();
      renderDialog({ existingRouteNames: ["test-model"] });

      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const route = screen.getByRole("textbox", { name: "Route name" });
      await user.clear(route);
      await user.type(route, "other-route");

      expect(screen.queryByText(/already exists/)).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
    });

    it("shows a taken custom route on the route field and blocks Deploy", async () => {
      const user = setupUser();
      renderDialog({ existingRouteNames: ["taken"] });

      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const route = screen.getByRole("textbox", { name: "Route name" });
      await user.clear(route);
      await user.type(route, "taken");
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
      expect(route).toHaveAccessibleDescription(
        'The model name callers send in requests. It follows the deployment name until you change it. An inference route named "taken" already exists.',
      );
    });

    it("shows a backend route conflict on the route field until the route changes", async () => {
      const user = setupUser();
      renderDialog({ routeConflict: { routeName: "test-model" } });

      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const route = screen.getByRole("textbox", { name: "Route name" });
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
      expect(route).toHaveAccessibleDescription(
        'The model name callers send in requests. It follows the deployment name until you change it. An inference route named "test-model" already exists. Change the route or the deployment name.',
      );

      await user.clear(route);
      await user.type(route, "other-route");
      expect(screen.queryByText(/already exists/)).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
    });

    it("shows a submission error on the Deploy step only", async () => {
      const user = setupUser();
      renderDialog({ error: "Deploy was rejected (Internal): the model service is unavailable." });

      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(screen.queryByText(/Deploy was rejected/)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Deploy was rejected (Internal): the model service is unavailable.",
      );
    });

    it("keeps Deploy available while the estimate is loading or failed", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({
        onEstimate: vi.fn().mockRejectedValue(new Error("estimate failed")),
        onSubmit,
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0]?.[0]).toMatchObject({
        name: "Test Model",
        profileId: "default",
      });
    });
  });

  describe("route name", () => {
    async function openDeploy(props: Partial<ComponentProps<typeof ModelDeployDialog>> = {}) {
      const user = setupUser();
      renderDialog(props);
      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();
      return user;
    }

    it("lists the deployment name before the route name and offers no exposure choice in basic mode", async () => {
      await openDeploy();

      const name = screen.getByRole("textbox", { name: "Deployment name" });
      const route = screen.getByRole("textbox", { name: "Route name" });
      expect(name.compareDocumentPosition(route) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
      expect(screen.queryByRole("radio", { name: "Direct" })).not.toBeInTheDocument();
      expect(screen.queryByRole("radio", { name: "Hidden" })).not.toBeInTheDocument();
    });

    it("keeps the exposure choice in basic mode while the deployment is hidden", async () => {
      const user = await openDeploy();
      const route = screen.getByRole("textbox", { name: "Route name" });

      await enableAdvanced(user);
      await user.click(screen.getByRole("radio", { name: "Hidden" }));
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();
      expect(screen.getByRole("radio", { name: "Hidden" })).toBeChecked();
      expect(route).toBeDisabled();
      expect(screen.queryByRole("button", { name: "Follow the deployment name" })).toBeNull();
      expect(route).toHaveAccessibleDescription(
        "The model name callers send in requests. It follows the deployment name until you change it. Hidden: no route of its own. Reachable only through other routes.",
      );

      await user.click(screen.getByRole("radio", { name: "Direct" }));
      expect(screen.queryByRole("radio", { name: "Hidden" })).not.toBeInTheDocument();
      expect(route).toBeEnabled();
      expect(screen.getByRole("button", { name: "Follow the deployment name" })).toHaveAttribute(
        "aria-pressed",
        "true",
      );
      expect(route).toHaveAccessibleDescription(
        "The model name callers send in requests. It follows the deployment name until you change it.",
      );
    });

    it("follows the deployment name until edited, then keeps the custom value", async () => {
      const user = await openDeploy();
      const route = screen.getByRole("textbox", { name: "Route name" });

      expect(route).toHaveValue("test-model");
      expect(route).toHaveClass("text-muted-foreground");
      expect(screen.getByRole("button", { name: "Follow the deployment name" })).toHaveAttribute(
        "aria-pressed",
        "true",
      );

      const name = screen.getByRole("textbox", { name: "Deployment name" });
      await user.clear(name);
      await user.type(name, "Support Bot");
      expect(route).toHaveValue("support-bot");

      await user.clear(route);
      await user.type(route, "support");
      expect(route).not.toHaveClass("text-muted-foreground");
      expect(screen.getByRole("button", { name: "Follow the deployment name" })).toHaveAttribute(
        "aria-pressed",
        "false",
      );

      await user.type(name, " v2");
      expect(route).toHaveValue("support");
    });

    it("returns to following with the toggle and when emptied on blur", async () => {
      const user = await openDeploy();
      const route = screen.getByRole("textbox", { name: "Route name" });
      const toggle = screen.getByRole("button", { name: "Follow the deployment name" });

      await user.clear(route);
      await user.type(route, "support");
      expect(toggle).toHaveAttribute("aria-pressed", "false");
      await user.click(toggle);
      expect(route).toHaveValue("test-model");
      expect(toggle).toHaveAttribute("aria-pressed", "true");

      await user.clear(route);
      expect(route).toHaveValue("");
      await user.tab();
      expect(route).toHaveValue("test-model");
      expect(toggle).toHaveAttribute("aria-pressed", "true");
    });

    it("keeps the shown name as a custom value and focuses the input when unlinked", async () => {
      const user = await openDeploy();
      const route = screen.getByRole("textbox", { name: "Route name" });
      const toggle = screen.getByRole("button", { name: "Follow the deployment name" });
      expect(toggle).toHaveAttribute("aria-pressed", "true");

      await user.click(toggle);
      expect(toggle).toHaveAttribute("aria-pressed", "false");
      expect(route).toHaveValue("test-model");
      expect(route).not.toHaveClass("text-muted-foreground");
      expect(route).toHaveFocus();

      await user.type(screen.getByRole("textbox", { name: "Deployment name" }), " v2");
      expect(route).toHaveValue("test-model");
    });

    it("flags the route name on blur only after it changes", async () => {
      const user = await openDeploy();
      const route = screen.getByRole("textbox", { name: "Route name" });

      await user.click(route);
      await user.tab();
      expect(route).not.toHaveAttribute("aria-invalid");

      await user.clear(route);
      await user.type(route, "Bad Name!");
      await user.tab();
      expect(route).toHaveAttribute("aria-invalid", "true");
      expect(route).toHaveAccessibleDescription(
        "The model name callers send in requests. It follows the deployment name until you change it. Route name must start with a letter and contain only lowercase letters, numbers, hyphens, and periods.",
      );
    });

    it("shows the exposure choice in the route name label row in advanced mode", async () => {
      const user = setupUser();
      renderDialog();
      await screen.findByText("Step 1 of 3 — Model & machine");
      await enableAdvanced(user);
      await goToDeploy(user);

      const route = screen.getByRole("textbox", { name: "Route name" });
      const labelRow = screen.getByText("Route name").parentElement!;
      const group = within(labelRow).getByRole("radiogroup", { name: "Exposure" });
      expect(within(group).getByRole("radio", { name: "Direct" })).toBeChecked();
      expect(within(group).getByRole("radio", { name: "Hidden" })).not.toBeChecked();
      expect(
        within(labelRow).getByRole("button", { name: "Follow the deployment name" }),
      ).toBeVisible();
      expect(screen.queryByText("Exposure")).not.toBeInTheDocument();
      expect(
        labelRow.compareDocumentPosition(route) & Node.DOCUMENT_POSITION_FOLLOWING,
      ).toBeTruthy();
    });

    it("disables the route name and clears its error while the deployment is hidden", async () => {
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      const user = setupUser();
      renderDialog({ onSubmit });
      await screen.findByText("Step 1 of 3 — Model & machine");
      await enableAdvanced(user);
      await goToDeploy(user);
      const route = screen.getByRole("textbox", { name: "Route name" });
      await user.clear(route);
      await user.type(route, "Bad Name!");
      await user.tab();
      expect(route).toHaveAttribute("aria-invalid", "true");

      await user.click(screen.getByRole("radio", { name: "Hidden" }));
      expect(route).toBeDisabled();
      expect(route).not.toHaveAttribute("aria-invalid");
      expect(screen.queryByText(/Route name must start/)).not.toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0][0].exposure).toEqual({ kind: { case: "hidden", value: {} } });

      await user.click(screen.getByRole("radio", { name: "Direct" }));
      expect(route).toBeEnabled();
    });

    it("focuses an invalid custom route when Deploy finds it", async () => {
      const onSubmit = vi.fn();
      const user = await openDeploy({ onSubmit });
      const route = screen.getByRole("textbox", { name: "Route name" });
      await user.clear(route);
      await user.type(route, "Bad Name!");
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      expect(route).toHaveFocus();
      expect(route).toHaveAttribute("aria-invalid", "true");
      expect(onSubmit).not.toHaveBeenCalled();
    });

    it("submits the renamed route name", async () => {
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      const user = setupUser();
      renderDialog({ onSubmit });

      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const name = screen.getByRole("textbox", { name: "Deployment name" });
      await user.clear(name);
      await user.type(name, "Support Bot");
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0][0].name).toBe("Support Bot");
      expect(onSubmit.mock.calls[0][0].exposure).toEqual({
        kind: { case: "direct", value: { routeName: "support-bot" } },
      });
    });

    it("explains a deployment name without a valid route slug", async () => {
      const user = setupUser();
      renderDialog();

      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const name = screen.getByRole("textbox", { name: "Deployment name" });
      await user.clear(name);
      await user.type(name, "日本語");

      expect(
        screen.getByText(
          "No valid route name can be derived from the deployment name. Enter a route name.",
        ),
      ).toBeVisible();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
    });
  });

  describe("runtime", () => {
    it("replaces defaults on profile change and submits valid inputs without preview", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      const onPreview = vi.fn().mockResolvedValue(snapshot);
      renderDialog({ onPreview, onSubmit });

      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(screen.getByRole("dialog", { name: "Test Model — Deploy model" })).not.toHaveAttribute(
        "aria-describedby",
      );
      expect(screen.getByRole("radio", { name: /Fast start/ })).toBeChecked();
      expect(screen.queryByRole("textbox", { name: /Total context/ })).not.toBeInTheDocument();
      expect(screen.queryByRole("combobox", { name: "Machine" })).not.toBeInTheDocument();
      expect(screen.queryByLabelText("Route name")).not.toBeInTheDocument();
      await user.click(screen.getByRole("radio", { name: /Large context/ }));
      expect(screen.getByRole("radio", { name: /Large context/ })).toBeChecked();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() =>
        expect(onSubmit).toHaveBeenCalledWith({
          configuration: {
            customArgumentTokens: [],
            modelArtifactBindingId: "",
            optionOverrides: [],
            engineParameterValues: {
              values: [
                { parameterId: "total-context", value: { case: "unsignedInteger", value: 16384n } },
              ],
            },
          },
          exposure: { kind: { case: "direct" as const, value: { routeName: "test-model" } } },
          name: "Test Model",
          profileId: "large",
          placement: create(ExactPlacementSchema, {
            siteNodeId: "node",
            executionTargetId: "pool",
          }),
        }),
      );
      expect(onPreview).not.toHaveBeenCalled();
    });

    it("submits every declared engine parameter with the edited total context overlaid", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      const onPreview = vi.fn().mockResolvedValue(snapshot);
      const onEstimate = vi
        .fn()
        .mockResolvedValue({ placementPreview: dedicatedPreview({ accelerator: 4_000_000_000n }) });
      renderDialog({
        engineReleases: multiParameterReleases,
        model: multiParameterModel,
        onEstimate,
        onPreview,
        onSubmit,
      });

      await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
      await openAdvancedStep(user);
      await user.click(screen.getByRole("button", { name: "Back" }));
      // The hidden parallel requests stay off the Settings step.
      expect(screen.queryByRole("textbox", { name: /Parallel requests/ })).not.toBeInTheDocument();
      const context = screen.getByRole("textbox", { name: /Total context/ });
      await user.clear(context);
      await user.type(context, "65536");
      await user.click(screen.getByRole("button", { name: "Next" }));
      // The unknown boolean sits ungrouped at the step's top level.
      const prefixCaching = screen.getByRole("switch", { name: "Prefix caching" });
      expect(prefixCaching).not.toBeChecked();
      expect(prefixCaching).toHaveAccessibleDescription("Caches reusable prompt prefixes.");
      await user.click(prefixCaching);

      const expectedConfiguration = {
        customArgumentTokens: [],
        engineParameterValues: {
          values: [
            { parameterId: "total-context", value: { case: "unsignedInteger", value: 65536n } },
            create(ParameterValueSchema, {
              parameterId: "parallel-requests",
              value: { case: "unsignedInteger", value: 4n },
            }),
            create(ParameterValueSchema, {
              parameterId: "kv-cache-memory-bytes",
              value: { case: "unsignedInteger", value: 7400000000n },
            }),
            { parameterId: "enable-prefix-caching", value: { case: "boolean", value: true } },
          ],
        },
        modelArtifactBindingId: "",
        optionOverrides: [],
      };
      await waitFor(() =>
        expect(onEstimate).toHaveBeenLastCalledWith({
          configuration: expectedConfiguration,
          placement: create(ExactPlacementSchema, {
            siteNodeId: "node",
            executionTargetId: "pool",
          }),
          profileId: "text",
        }),
      );

      // The Command step previews the command as it opens.
      await user.click(screen.getByRole("button", { name: "Next" }));
      await waitFor(() => expect(onPreview).toHaveBeenCalledOnce());
      await goToDeploy(user);
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      const expectedRequest = {
        configuration: expectedConfiguration,
        exposure: { kind: { case: "direct" as const, value: { routeName: "wide-model" } } },
        name: "Wide Model",
        profileId: "text",
        placement: create(ExactPlacementSchema, { siteNodeId: "node", executionTargetId: "pool" }),
      };
      expect(onPreview).toHaveBeenCalledWith(expectedRequest);
      await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(expectedRequest));
    });

    it("sends the edited KV offload size with the complete value set", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      const onPreview = vi.fn().mockResolvedValue(snapshot);
      const onEstimate = vi
        .fn()
        .mockResolvedValue({ placementPreview: dedicatedPreview({ accelerator: 4_000_000_000n }) });
      renderDialog({
        engineReleases: kvOffloadReleases,
        model: kvOffloadModel,
        onEstimate,
        onPreview,
        onSubmit,
      });

      await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
      await openAdvancedStep(user);

      // The KV offload field surfaces the schema label and the profile default (16 GiB).
      const kvOffload = within(screen.getByRole("group", { name: "Host memory" })).getByRole(
        "textbox",
        { name: "KV offloading size" },
      );
      expect(kvOffload).toHaveValue("16");

      await user.clear(kvOffload);
      await user.type(kvOffload, "32");

      const expectedConfiguration = {
        customArgumentTokens: [],
        engineParameterValues: {
          values: [
            { parameterId: "total-context", value: { case: "unsignedInteger", value: 32768n } },
            {
              parameterId: "kv-offloading-size-gib",
              value: { case: "unsignedInteger", value: 32n },
            },
          ],
        },
        modelArtifactBindingId: "",
        optionOverrides: [],
      };
      await waitFor(() =>
        expect(onEstimate).toHaveBeenLastCalledWith({
          configuration: expectedConfiguration,
          placement: create(ExactPlacementSchema, {
            siteNodeId: "node",
            executionTargetId: "pool",
          }),
          profileId: "nvfp4-text",
        }),
      );

      // The Command step previews the command as it opens.
      await user.click(screen.getByRole("button", { name: "Next" }));
      await waitFor(() => expect(onPreview).toHaveBeenCalledOnce());
      await goToDeploy(user);
      expect(summaryRows()[4]).toEqual(["KV offloading size", "32 GiB"]);
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      const expectedRequest = {
        configuration: expectedConfiguration,
        exposure: { kind: { case: "direct" as const, value: { routeName: "kv-offload-model" } } },
        name: "KV Offload Model",
        profileId: "nvfp4-text",
        placement: create(ExactPlacementSchema, { siteNodeId: "node", executionTargetId: "pool" }),
      };
      expect(onPreview).toHaveBeenCalledWith(expectedRequest);
      await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(expectedRequest));
    });

    it("splits custom arguments typed one per line and sends them unchanged to preview and deployment", async () => {
      const user = setupUser();
      const customSnapshot = create(ResolvedLocalModelDeploymentSnapshotSchema, {
        ...snapshot,
        customArgumentTokens: ["--flag=value", "  spaced  ", ""],
        desiredLaunchPlan: create(PortableModelLaunchPlanSchema, {
          arguments: [
            { value: { case: "engineExecutable", value: {} } },
            { value: { case: "literal", value: "--ctx-size" } },
            { value: { case: "literal", value: "8192" } },
            { value: { case: "literal", value: "--seed" } },
            { value: { case: "literal", value: "-1" } },
            { value: { case: "literal", value: "--model" } },
            { value: { case: "artifactBindingId", value: "weights" } },
            { value: { case: "literal", value: "--flag=value" } },
            { value: { case: "literal", value: "  spaced  " } },
            { value: { case: "literal", value: "" } },
          ],
        }),
      });
      const onPreview = vi.fn().mockResolvedValue(customSnapshot);
      const onSubmit = vi.fn();
      renderDialog({ onPreview, onSubmit });

      await openAdvancedStep(user, "Command");
      const box = screen.getByRole("textbox", { name: "Custom arguments" });
      expect(box).not.toHaveAccessibleDescription();
      expect(box).toHaveAttribute("placeholder", "--temp 0.7\n--top-k 40");
      await user.type(box, "--flag=value{Enter}'  spaced  '{Enter}''");
      // One flag per line with its values, each line but the last ending in a backslash.
      await waitFor(() =>
        expect(
          Array.from(screen.getByLabelText("Launch command").children, (line) => line.textContent),
        ).toEqual([
          "{engine.executable} \\",
          "--ctx-size 8192 \\",
          "--seed -1 \\",
          "--model {artifact:weights} \\",
          "--flag=value '  spaced  ' ''",
        ]),
      );
      // The scrolling block is reachable by keyboard.
      const command = screen.getByRole("region", { name: "Launch command" });
      expect(command).toHaveAttribute("tabindex", "0");
      // The custom arguments end the command, highlighted.
      expect(
        Array.from(
          screen.getByLabelText("Launch command").querySelectorAll(".text-primary"),
          (token) => token.textContent,
        ),
      ).toEqual(["--flag=value", "'  spaced  '", "''"]);
      await goToDeploy(user);
      expect(summaryRows().at(-1)).toEqual(["Advanced", "Customized: 3 custom arguments"]);
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      const customValues = {
        configuration: {
          customArgumentTokens: ["--flag=value", "  spaced  ", ""],
          engineParameterValues: {
            values: [
              {
                parameterId: "total-context",
                value: { case: "unsignedInteger" as const, value: 8192n },
              },
            ],
          },
          modelArtifactBindingId: "",
          optionOverrides: [],
        },
        exposure: { kind: { case: "direct" as const, value: { routeName: "test-model" } } },
        name: "Test Model",
        profileId: "default",
        placement: create(ExactPlacementSchema, { siteNodeId: "node", executionTargetId: "pool" }),
      };
      expect(onPreview).toHaveBeenLastCalledWith(customValues);
      await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(customValues));
    });

    it("keeps an untouched total context quiet on blur and flags an out-of-range one after a change", async () => {
      const user = setupUser();
      renderDialog();

      await openSettings(user);
      const context = screen.getByRole("textbox", { name: /Total context/ });
      await user.click(context);
      await user.tab();
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 10));
      });
      expect(context).not.toHaveAttribute("aria-invalid");

      await user.clear(context);
      await user.type(context, "512");
      await user.tab();
      await waitFor(() => expect(context).toHaveAttribute("aria-invalid", "true"));
      expect(
        screen.getByText("Total context must be an integer from 1,024 to 32,768 tokens."),
      ).toBeVisible();
    });

    it("holds an invalid context on the Settings step with its exact range", async () => {
      const user = setupUser();
      renderDialog();

      await openSettings(user);
      const context = screen.getByRole("textbox", { name: /Total context/ });
      await user.clear(context);
      await user.type(context, "12");
      await user.tab();

      expect(
        await screen.findByText("Total context must be an integer from 1,024 to 32,768 tokens."),
      ).toBeVisible();
      expect(context).toHaveAttribute("aria-invalid", "true");
      expect(context).toHaveAccessibleDescription(
        "Total context must be an integer from 1,024 to 32,768 tokens.",
      );

      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 2 of 3 — Context")).toBeVisible();
      expect(context).toHaveFocus();
    });
  });

  describe("estimates", () => {
    it("estimates the exact current configuration and ignores stale responses", async () => {
      const user = setupUser();
      const resolvers: Array<(value: LocalResourceEstimate) => void> = [];
      const onEstimate = vi.fn(
        () =>
          new Promise<LocalResourceEstimate>((resolve) => {
            resolvers.push(resolve);
          }),
      );
      renderDialog({ onEstimate, deployments: [] });

      await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
      await openSettings(user);
      expect(onEstimate).toHaveBeenLastCalledWith(estimateRequest("default", 8192n));

      // Each value typed on the way is below the range, so only the last one is estimated.
      await user.tripleClick(screen.getByRole("textbox", { name: /Total context/ }));
      await user.keyboard("9000");
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(2));
      expect(onEstimate).toHaveBeenLastCalledWith(estimateRequest("default", 9000n));

      resolvers[0]({ placementPreview: dedicatedPreview({ accelerator: 1n * gib }) });
      await waitFor(() =>
        expect(
          within(screen.getByRole("region", { name: "Predicted memory" })).queryByRole("button", {
            name: "VRAM 1 / 32 GiB",
          }),
        ).not.toBeInTheDocument(),
      );
      resolvers[1]({ placementPreview: dedicatedPreview({ accelerator: 3n * gib }) });
      expect(
        await within(screen.getByRole("region", { name: "Predicted memory" })).findByRole(
          "button",
          { name: "VRAM 3 / 32 GiB" },
        ),
      ).toBeVisible();
    });

    it("fences stale estimates after a profile change", async () => {
      const user = setupUser();
      const resolvers: Array<(value: LocalResourceEstimate) => void> = [];
      const onEstimate = vi.fn(
        () =>
          new Promise<LocalResourceEstimate>((resolve) => {
            resolvers.push(resolve);
          }),
      );
      renderDialog({ onEstimate, deployments: [] });

      await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
      expect(onEstimate).toHaveBeenLastCalledWith(estimateRequest("default", 8192n));
      await user.click(screen.getByRole("radio", { name: /Large context/ }));
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(2));
      expect(onEstimate).toHaveBeenLastCalledWith(estimateRequest("large", 16384n));

      resolvers[0]({ placementPreview: dedicatedPreview({ accelerator: 1n * gib }) });
      await waitFor(() =>
        expect(
          within(screen.getByRole("region", { name: "Predicted memory" })).queryByRole("button", {
            name: "VRAM 1 / 32 GiB",
          }),
        ).not.toBeInTheDocument(),
      );
      resolvers[1]({ placementPreview: dedicatedPreview({ accelerator: 2n * gib }) });
      expect(
        await within(screen.getByRole("region", { name: "Predicted memory" })).findByRole(
          "button",
          { name: "VRAM 2 / 32 GiB" },
        ),
      ).toBeVisible();
    });

    it("deduplicates the StrictMode resource-estimate effect", async () => {
      const user = setupUser();
      const onEstimate = vi
        .fn()
        .mockResolvedValue({ placementPreview: dedicatedPreview({ accelerator: 3n * gib }) });
      render(<StrictMode>{dialog({ onEstimate, deployments: [] })}</StrictMode>);

      await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
      await openAdvancedStep(user, "Command");
      expect(
        await within(screen.getByRole("region", { name: "Predicted memory" })).findByRole(
          "button",
          { name: "VRAM 3 / 32 GiB" },
        ),
      ).toBeVisible();
    });

    it("removes an estimate when custom arguments make the result absent", async () => {
      const user = setupUser();
      const onEstimate = vi
        .fn()
        .mockResolvedValueOnce({
          placementPreview: dedicatedPreview({ accelerator: 3_472_102_016n }),
        })
        .mockResolvedValue({ placementPreview: dedicatedPreview({}) });
      renderDialog({ onEstimate, deployments: [] });

      await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
      await openAdvancedStep(user, "Command");
      expect(
        await within(screen.getByRole("region", { name: "Predicted memory" })).findByRole(
          "button",
          { name: "VRAM 3.2 / 32 GiB" },
        ),
      ).toBeVisible();
      await user.type(screen.getByRole("textbox", { name: "Custom arguments" }), "--x");
      await waitFor(() =>
        expect(onEstimate).toHaveBeenLastCalledWith({
          ...estimateRequest("default", 8192n),
          configuration: {
            ...estimateRequest("default", 8192n).configuration,
            customArgumentTokens: ["--x"],
          },
        }),
      );
      expect(
        await within(screen.getByRole("region", { name: "Predicted memory" })).findByText(
          "No estimate",
        ),
      ).toBeVisible();
      expect(
        within(screen.getByRole("region", { name: "Predicted memory" })).queryByRole("button", {
          name: /VRAM 3\.2/,
        }),
      ).not.toBeInTheDocument();
    });

    it("keeps preview and deployment available when estimation fails", async () => {
      const user = setupUser();
      renderDialog({
        onEstimate: vi.fn().mockRejectedValue(new Error("estimate failed")),
        deployments: [],
      });

      await openAdvancedStep(user, "Command");
      expect(
        await within(screen.getByRole("region", { name: "Predicted memory" })).findByText(
          "Estimate unavailable.",
        ),
      ).toBeVisible();
      await goToDeploy(user);
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
    });

    it("warns on host KV offload for a unified target", async () => {
      const user = setupUser();
      const onPreviewMemory = vi.fn().mockResolvedValue({
        acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
        advisories: [LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY],
      });
      renderDialog({
        engineReleases: kvOffloadReleases,
        model: kvOffloadModel,
        onEstimate: vi.fn().mockResolvedValue({
          placementPreview: dedicatedPreview({ accelerator: 3_478_131_000n }),
        }),
        onPreviewMemory,
      });

      await openAdvancedStep(user);
      expect(await screen.findByText(unifiedKvOffloadWarning)).toBeVisible();
      expect(onPreviewMemory).toHaveBeenLastCalledWith(kvOffloadRequest(32768n));
      // The advisory is non-blocking.
      expect(screen.getByRole("button", { name: "Next" })).toBeEnabled();
    });

    it("warns on the Deploy step when a unified target's memory pool is below the profile minimum", async () => {
      const user = setupUser();
      const onPreviewMemory = vi.fn().mockResolvedValue({
        acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
        advisories: [LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM],
        memoryPoolTotalBytes: 8n * 1024n ** 3n,
        minimumMemoryBytesPerDevice: 12_000_000_000n,
      });
      renderDialog({
        onEstimate: vi.fn().mockResolvedValue({
          placementPreview: dedicatedPreview({ accelerator: 3_478_131_000n }),
        }),
        onPreviewMemory,
      });

      await openSettings(user);
      await goToDeploy(user);
      expect(
        await screen.findByText(
          "This target's system memory (8 GiB) is below this profile's 11.2 GiB minimum. The model may fail to load or slow down the machine.",
        ),
      ).toBeVisible();
      expect(screen.queryByText(unifiedKvOffloadWarning)).not.toBeInTheDocument();
      // The advisory is non-blocking.
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
    });

    it("shows no warning without advisories", async () => {
      const user = setupUser();
      const onPreviewMemory = vi.fn().mockResolvedValue({
        acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture.DEDICATED,
        advisories: [],
      });
      renderDialog({
        onEstimate: vi.fn().mockResolvedValue({
          placementPreview: dedicatedPreview({ accelerator: 3_478_131_000n }),
        }),
        onPreviewMemory,
      });

      await openAdvancedStep(user, "Command");
      await waitFor(() => expect(onPreviewMemory).toHaveBeenCalled());
      expect(screen.queryByText(unifiedKvOffloadWarning)).not.toBeInTheDocument();
    });

    it("updates the KV offload warning when the previewed values change", async () => {
      const user = setupUser();
      const onPreviewMemory = vi
        .fn()
        .mockResolvedValueOnce({
          acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
          advisories: [LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY],
        })
        .mockResolvedValue({
          acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
          advisories: [],
        });
      renderDialog({ engineReleases: kvOffloadReleases, model: kvOffloadModel, onPreviewMemory });

      await openAdvancedStep(user);
      expect(await screen.findByText(unifiedKvOffloadWarning)).toBeVisible();

      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.clear(screen.getByRole("textbox", { name: /Total context/ }));
      await user.type(screen.getByRole("textbox", { name: /Total context/ }), "16384");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await waitFor(() =>
        expect(screen.queryByText(unifiedKvOffloadWarning)).not.toBeInTheDocument(),
      );
      expect(onPreviewMemory).toHaveBeenLastCalledWith(kvOffloadRequest(16384n));
    });

    it("keeps deployment available when the memory preview fails", async () => {
      const user = setupUser();
      const onPreviewMemory = vi
        .fn()
        .mockRejectedValue(new Error("Execution target is unavailable."));
      renderDialog({
        onEstimate: vi.fn().mockResolvedValue({
          placementPreview: dedicatedPreview({ accelerator: 3_478_131_000n }),
        }),
        onPreviewMemory,
      });

      await openAdvancedStep(user, "Command");
      await waitFor(() => expect(onPreviewMemory).toHaveBeenCalled());
      expect(screen.queryByText(unifiedKvOffloadWarning)).not.toBeInTheDocument();
      // Background failures stay quiet; the Command step's command preview reports them.
      expect(screen.queryByText("Execution target is unavailable.")).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Next" })).toBeEnabled();
    });
  });

  describe("Command step", () => {
    it("previews the command as the step opens and once each time typing pauses", async () => {
      setTiming(productionTiming);
      useFakeTimersWithWaits();
      const onPreview = vi.fn<ComponentProps<typeof ModelDeployDialog>["onPreview"]>(() =>
        Promise.resolve(snapshot),
      );
      renderDialog({ onPreview });
      await settle();
      fireEvent.click(screen.getByRole("checkbox", { name: "Advanced" }));
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 4 — Command")).toBeVisible();
      await settle();
      expect(onPreview).toHaveBeenCalledOnce();
      expect(screen.getByLabelText("Launch command")).toHaveTextContent("--ctx-size 8192");

      const box = screen.getByRole("textbox", { name: "Custom arguments" });
      fireEvent.change(box, { target: { value: "--te" } });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.commandPreviewInputMs - 100);
      });
      fireEvent.change(box, { target: { value: "--temp 0.7" } });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.commandPreviewInputMs - 100);
      });
      // Typing hasn't paused long enough yet; the shown command is marked as updating.
      expect(onPreview).toHaveBeenCalledOnce();
      expect(screen.getByText("Updating…")).toBeVisible();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(100);
      });
      expect(onPreview).toHaveBeenCalledTimes(2);
      expect(onPreview.mock.calls[1][0].configuration.customArgumentTokens).toEqual([
        "--temp",
        "0.7",
      ]);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.loadingMinimumVisibleMs);
      });
      expect(screen.queryByText("Updating…")).not.toBeInTheDocument();
    });

    it("names the earlier step to fix instead of previewing, and stays on the Command step", async () => {
      const user = setupUser();
      const onPreview = vi.fn().mockResolvedValue(snapshot);
      // A launch read back with a value the catalog no longer accepts, which basic mode can't show.
      renderDialog({
        engineReleases: runtimeSchemaReleases,
        initialConfiguration: {
          customArgumentTokens: [],
          engineParameterValues: create(ParameterValuesSchema, {
            values: [
              {
                parameterId: "prompt-cache-memory-mib",
                value: { case: "unsignedInteger", value: 3000000000n },
              },
            ],
          }),
          modelArtifactBindingId: "",
          optionOverrides: [],
        },
        initialProfileId: "q4-xl",
        model: runtimeSchemaModel,
        onPreview,
      });

      await openSettings(user);
      await goToDeploy(user);
      await enableAdvanced(user);
      await user.click(screen.getByRole("button", { name: "Back" }));

      expect(screen.getByText("Step 4 of 5 — Command")).toBeVisible();
      expect(
        screen.getByText("Fix the errors on the Performance & memory step to see the command."),
      ).toBeVisible();
      expect(onPreview).not.toHaveBeenCalled();
    });

    it("shows a rejected command in the command area and blocks Deploy until a clean preview", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      const onPreview = vi.fn((values: { configuration: { customArgumentTokens: string[] } }) =>
        values.configuration.customArgumentTokens.includes("--ctx-size")
          ? Promise.reject(new Error("custom argument --ctx-size is set by Dotcore"))
          : Promise.resolve(snapshot),
      );
      renderDialog({ onPreview, onSubmit });

      await openAdvancedStep(user, "Command");
      await user.type(screen.getByRole("textbox", { name: "Custom arguments" }), "--ctx-size 4096");
      expect(await screen.findByRole("alert")).toHaveTextContent(
        "The command was rejected: custom argument --ctx-size is set by Dotcore",
      );
      // The rejection blames the command, not the box.
      expect(screen.getByRole("textbox", { name: "Custom arguments" })).not.toHaveAttribute(
        "aria-invalid",
      );

      await goToDeploy(user);
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      expect(await screen.findByText("Step 3 of 4 — Command")).toBeVisible();
      expect(onSubmit).not.toHaveBeenCalled();

      await user.clear(screen.getByRole("textbox", { name: "Custom arguments" }));
      await waitFor(() =>
        expect(screen.getByLabelText("Launch command")).toHaveTextContent("--ctx-size 8192"),
      );
      await goToDeploy(user);
      await user.click(screen.getByRole("button", { name: "Deploy" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    });

    it("turns Advanced on and shows the Command step when Deploy meets a rejected command with Advanced off", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      const onPreview = vi.fn().mockRejectedValue(new Error("the command is invalid"));
      renderDialog({ onPreview, onSubmit });

      await openAdvancedStep(user, "Command");
      expect(await screen.findByRole("alert")).toHaveTextContent(
        "The command was rejected: the command is invalid",
      );
      await goToDeploy(user);
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByRole("checkbox", { name: "Advanced" })).not.toBeChecked();
      await goToDeploy(user);
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      expect(await screen.findByText("Step 3 of 4 — Command")).toBeVisible();
      expect(screen.getByRole("checkbox", { name: "Advanced" })).toBeChecked();
      expect(onSubmit).not.toHaveBeenCalled();
    });

    it("restores custom arguments one per line, quoted where needed, and sends them back unchanged", async () => {
      const user = setupUser();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      const tokens = ["--system-prompt", "be brief", "it's", "--jinja"];
      renderDialog({
        initialConfiguration: {
          customArgumentTokens: tokens,
          engineParameterValues: create(ParameterValuesSchema, {}),
          modelArtifactBindingId: "",
          optionOverrides: [],
        },
        onPreview: vi.fn().mockResolvedValue(snapshot),
        onSubmit,
      });

      await openAdvancedStep(user, "Command");
      expect(screen.getByRole("textbox", { name: "Custom arguments" })).toHaveValue(
        "--system-prompt\n'be brief'\n'it'\\''s'\n--jinja",
      );
      await goToDeploy(user);
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0][0].configuration.customArgumentTokens).toEqual(tokens);
    });

    it("returns to the Command step when the backend rejects a create with custom arguments", async () => {
      const user = setupUser();
      const props = {
        initialConfiguration: {
          customArgumentTokens: ["--bogus"],
          engineParameterValues: create(ParameterValuesSchema, {}),
          modelArtifactBindingId: "",
          optionOverrides: [],
        },
        onPreview: vi.fn().mockResolvedValue(snapshot),
      };
      const { rerender } = renderDialog(props);

      await openSettings(user);
      await goToDeploy(user);
      rerender(dialog({ ...props, error: "Deploy was rejected: unknown argument --bogus." }));

      expect(await screen.findByText("Step 3 of 4 — Command")).toBeVisible();
      expect(screen.getByRole("checkbox", { name: "Advanced" })).toBeChecked();
      expect(screen.getByText("Deploy was rejected: unknown argument --bogus.")).toBeVisible();
    });
  });

  describe("memory strip", () => {
    it("follows the selected profile's estimate on every step", async () => {
      const user = setupUser();
      renderDialog({
        onEstimate: profileEstimates({ default: 8n * gib, large: 14n * gib }),
        deployments: [],
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      const strip = await screen.findByRole("region", { name: "Predicted memory" });
      expect(screen.getByTestId("deployment-dialog-persistent")).toContainElement(strip);
      // The default target applies once the targets load.
      expect(await within(strip).findByText("node / NVIDIA GeForce RTX 5090")).toBeVisible();
      expect(await within(strip).findByRole("button", { name: "VRAM 8 / 32 GiB" })).toBeVisible();

      await user.click(screen.getByRole("radio", { name: /Large context/ }));
      expect(await within(strip).findByRole("button", { name: "VRAM 14 / 32 GiB" })).toBeVisible();

      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();
      expect(
        within(screen.getByRole("region", { name: "Predicted memory" })).getByRole("button", {
          name: "VRAM 14 / 32 GiB",
        }),
      ).toBeVisible();
    });

    it("predicts nothing for this deployment once no profile runs on the site", async () => {
      renderDialog({
        listProfileCompatibility: compatibilityLoader({
          reason: { code: ModelCompatibilityReasonCode.TARGET_NOT_REPORTING },
          status: ModelCompatibilityStatus.UNKNOWN,
          target: poolPlacement,
        }),
        onEstimate: profileEstimates({ default: 8n * gib, large: 14n * gib }),
        deployments: [],
      });

      const strip = await screen.findByRole("region", { name: "Predicted memory" });
      expect(await screen.findByText("No profile runs here")).toBeVisible();
      // No target can run a profile, so none is placed.
      expect(within(strip).getByText("No execution target")).toBeVisible();
      expect(within(strip).queryByRole("button", { name: /^VRAM/ })).not.toBeInTheDocument();
    });

    it("follows the selected target's machine", async () => {
      const user = setupUser();
      renderDialog({
        deployments: [],
        listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()),
        onEstimate: ({ placement }) =>
          Promise.resolve({
            placementPreview:
              placement?.siteNodeId === "worker"
                ? placementPreview([hostPool(128n * gib, [other(68n * gib), previewed(8n * gib)])])
                : dedicatedPreview({ accelerator: 8n * gib }),
          }),
      });

      const picker = await screen.findByRole("combobox", { name: "Machine" });
      await waitFor(() => expect(picker).toHaveValue("node / NVIDIA GeForce RTX 5090"));
      const strip = screen.getByRole("region", { name: "Predicted memory" });
      expect(await within(strip).findByRole("button", { name: "VRAM 8 / 32 GiB" })).toBeVisible();

      await user.click(picker);
      await user.click(screen.getByRole("option", { name: /^Worker/ }));

      expect(within(strip).getByText("Worker / NVIDIA GB10")).toBeVisible();
      expect(within(strip).getByRole("button", { name: "RAM 8 / 128 GiB" })).toBeVisible();
      expect(within(strip).queryByRole("button", { name: /^VRAM/ })).not.toBeInTheDocument();
    });

    it("shows the selected GPU target's own preview on a machine with several", async () => {
      const user = setupUser();
      const gpuTarget = (id: string, name: string, device: string) =>
        create(ConfiguredExecutionTargetSchema, {
          ...poolTarget({ id, name }),
          acceleratorResolution: AcceleratorResolutionStatus.RESOLVED,
          observedAcceleratorDeviceIds: [device],
        });
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(
          gpuTarget("pool-a", "NVIDIA GeForce RTX 5090 · GPU 0", "GPU-a"),
          gpuTarget("pool-b", "NVIDIA GeForce RTX 5090 · GPU 1", "GPU-b"),
        ),
        deployments: [],
        onEstimate: ({ placement }) =>
          Promise.resolve({
            placementPreview: placementPreview([
              placement?.executionTargetId === "pool-b"
                ? devicePool(24n * gib, [other(4n * gib), previewed(8n * gib)], {
                    deviceId: "GPU-b",
                  })
                : devicePool(32n * gib, [other(12n * gib), previewed(8n * gib)]),
              hostPool(64n * gib, [other(34n * gib)]),
            ]),
          }),
      });

      const picker = await screen.findByRole("combobox", { name: "Machine" });
      await user.click(picker);
      await user.click(screen.getByRole("option", { name: /GPU 0/ }));
      const strip = screen.getByRole("region", { name: "Predicted memory" });
      expect(await within(strip).findByRole("button", { name: "VRAM 8 / 32 GiB" })).toBeVisible();

      await user.click(picker);
      await user.click(screen.getByRole("option", { name: /GPU 1/ }));
      expect(await within(strip).findByRole("button", { name: "VRAM 8 / 24 GiB" })).toBeVisible();
      expect(within(strip).getAllByRole("button", { name: /^VRAM/ })).toHaveLength(1);
    });

    it("warns about over capacity on the Deploy pane without blocking Deploy", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({
        deployments: [],
        onEstimate: () =>
          Promise.resolve({
            placementPreview: dedicatedPreview({
              accelerator: 26n * gib,
              device: exceeding(6n * gib, true),
            }),
          }),
        onSubmit,
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(
        await screen.findByRole("button", { name: "VRAM 26 / 32 GiB, over capacity by 6 GiB" }),
      ).toBeVisible();
      expect(screen.queryByRole("note")).not.toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));

      expect(
        within(screen.getByTestId("deployment-dialog-body")).getByRole("note"),
      ).toHaveTextContent(
        "This deployment may exceed memory capacity on node / NVIDIA GeForce RTX 5090: VRAM by 6 GiB.",
      );
      const deploy = screen.getByRole("button", { name: "Deploy" });
      expect(deploy).toBeEnabled();
      await user.click(deploy);
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    });

    it("draws a dedicated target's preview, hints its starting value, and warns within the headroom", async () => {
      const user = setupUser();
      renderDialog({
        deployments: [runningDeployment("d-gemma", "Gemma")],
        listProfileCompatibility: compatibilityLoader(
          targetCompatibility(poolPlacement, {
            default: { startingTotalContextTokens: 16_384 },
          }),
        ),
        onEstimate: () =>
          Promise.resolve({
            largestTotalContext: 16_384,
            placementPreview: placementPreview([
              devicePool(
                32n * gib,
                [measured("d-gemma", 10n * gib), other(2n * gib), previewed(20_300n * mib)],
                exceeding(147n * mib, false),
              ),
              hostPool(64n * gib, [other(34n * gib)]),
            ]),
          }),
      });

      const strip = await screen.findByRole("region", { name: "Predicted memory" });
      expect(await within(strip).findByRole("img", { name: /^VRAM on node:/ })).toHaveAttribute(
        "aria-label",
        "VRAM on node: 31.8 GiB used of 32 GiB; deployments: Gemma 10 GiB, Test Model 19.8 GiB; less than 1% free by 147 MiB",
      );
      // The bar warns at the pool's fit line, as the verdict and the Deploy notice do.
      expect(
        within(strip).getByRole("button", {
          name: "VRAM 20 / 32 GiB, less than 1% free by 147 MiB",
        }),
      ).toBeVisible();
      expect(
        [...strip.querySelectorAll<HTMLElement>('[data-slot="memory-capacity-marker"]')].map(
          (marker) => marker.dataset.line,
        ),
      ).toEqual(["fit"]);

      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(await readFieldHelpFacts("Total context")).toContain(
        "Up to 16k on node / NVIDIA GeForce RTX 5090",
      );
      expect(screen.getByRole("textbox", { name: /Total context/ })).toHaveValue("16k");
      expect(screen.queryByRole("note")).not.toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(
        within(screen.getByTestId("deployment-dialog-body")).getByRole("note"),
      ).toHaveTextContent(
        "This deployment leaves less than 1% of memory free on node / NVIDIA GeForce RTX 5090: VRAM by 147 MiB.",
      );
    });

    it("draws a unified target's preview beyond its total with the marker and warning", async () => {
      const user = setupUser();
      renderDialog({
        deployments: [runningDeployment("d-qwen", "Qwen")],
        listConfiguredExecutionTargets: targetsLoader(workerTarget()),
        onEstimate: () =>
          Promise.resolve({
            placementPreview: placementPreview(
              [
                hostPool(
                  128n * gib,
                  [measured("d-qwen", 80n * gib), other(20n * gib), previewed(40n * gib)],
                  exceeding(12n * gib, true),
                ),
              ],
              MemoryFit.EXCEEDS,
            ),
          }),
      });

      const strip = await screen.findByRole("region", { name: "Predicted memory" });
      expect(within(strip).getByText("Worker / NVIDIA GB10")).toBeVisible();
      expect(
        await within(strip).findByRole("button", {
          name: "RAM 40 / 128 GiB, over capacity by 12 GiB",
        }),
      ).toHaveTextContent(/^▲ 40 \/ 128 GiB$/);
      expect(
        within(strip)
          .getAllByRole("img")
          .map((bar) => bar.getAttribute("aria-label")),
      ).toEqual([
        "RAM on Worker: 140 GiB used of 128 GiB; deployments: Qwen 80 GiB, Test Model 40 GiB; over capacity by 12 GiB",
      ]);
      expect(
        [...strip.querySelectorAll<HTMLElement>('[data-slot="memory-capacity-marker"]')].map(
          (marker) => marker.dataset.line,
        ),
      ).toEqual(["total"]);

      await user.click(screen.getByRole("button", { name: "Next" }));
      // No starting value from a fit, so no hint.
      expect(screen.queryByText(/^Up to /)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(
        within(screen.getByTestId("deployment-dialog-body")).getByRole("note"),
      ).toHaveTextContent(
        "This deployment may exceed memory capacity on Worker / NVIDIA GB10: RAM by 12 GiB.",
      );
    });

    it("lists a pool of unknown size as unreadable and warns about nothing", async () => {
      const user = setupUser();
      renderDialog({
        deployments: [runningDeployment("d-gemma", "Gemma")],
        onEstimate: () =>
          Promise.resolve({
            placementPreview: placementPreview(
              [
                devicePool(undefined, [measured("d-gemma", 10n * gib), previewed(8n * gib)]),
                hostPool(64n * gib, [other(34n * gib)]),
              ],
              MemoryFit.UNKNOWN,
            ),
          }),
      });

      const strip = await screen.findByRole("region", { name: "Predicted memory" });
      expect(await within(strip).findByRole("img", { name: /^RAM on node:/ })).toHaveAttribute(
        "aria-label",
        "RAM on node: 34 GiB used of 64 GiB; unreadable: node VRAM (Gemma, Test Model)",
      );
      expect(within(strip).queryByRole("button", { name: /^VRAM/ })).not.toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();
      expect(screen.queryByRole("note")).not.toBeInTheDocument();
    });

    it("holds an empty row while the launch values the estimate waits for load", async () => {
      const onEstimate = vi.fn();
      renderDialog({
        deployments: [],
        listProfileCompatibility: vi.fn(() => new Promise<never>(() => undefined)),
        onEstimate,
      });

      const strip = await screen.findByRole("region", { name: "Predicted memory" });
      expect(await within(strip).findByText("node / NVIDIA GeForce RTX 5090")).toBeVisible();
      expect(strip).toHaveTextContent(/^node \/ NVIDIA GeForce RTX 5090RAM—$/);
      expect(onEstimate).not.toHaveBeenCalled();
    });

    it("never blocks Deploy while the estimate is loading or failed", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({
        onEstimate: () => Promise.reject(new Error("sizing failed")),
        onSubmit,
        deployments: [],
      });

      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(await screen.findByText("Estimate unavailable.")).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    });
  });

  describe("submit labels", () => {
    const applyLabels = { pending: "Applying…", submit: "Apply", title: "Customize model" };

    it("names the dialog and its last action after the opener's labels", async () => {
      const user = setupUser();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      renderDialog({ labels: applyLabels, onSubmit });

      expect(screen.getByRole("dialog", { name: "Test Model — Customize model" })).toBeVisible();
      await openSettings(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Review & deploy")).toBeVisible();
      expect(
        within(screen.getByTestId("deployment-dialog-actions"))
          .getAllByRole("button")
          .map((button) => button.textContent),
      ).toEqual(["Cancel", "Back", "Apply"]);

      await user.click(screen.getByRole("button", { name: "Apply" }));
      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
      expect(onSubmit.mock.calls[0][0]).toMatchObject({ name: "Test Model", profileId: "default" });
    });
  });

  describe("layout", () => {
    it("places the deployment popup above its modal overlay", () => {
      render(dialog({ listConfiguredExecutionTargets: undefined }));

      expectResourceMarker(
        inspectorTitle(screen.getByRole("dialog", { name: "Test Model — Deploy model" })),
        "model-1",
      );
      // jsdom computes a `calc()` over a custom property to `auto`, so read the inline style.
      expect(screen.getByRole("dialog", { name: "Test Model — Deploy model" }).style.zIndex).toBe(
        "calc(var(--layer-modal) + 1)",
      );
    });

    it("uses a bounded modal shell with the footer after the scrolling body", async () => {
      const user = setupUser();
      renderDialog();

      const modal = screen.getByRole("dialog", { name: "Test Model — Deploy model" });
      const body = within(modal).getByTestId("deployment-dialog-body");
      await screen.findByText("Step 1 of 3 — Model & machine");
      expect(within(body).getByRole("table", { name: "Model profiles" })).toBeVisible();
      await openAdvancedStep(user, "Command");
      const form = within(body).getByTestId("deployment-form");
      const footer = within(modal).getByTestId("deployment-dialog-footer");

      expect(modal).toHaveClass("overflow-hidden");
      expect(modal).toHaveClass("grid-rows-[auto_minmax(0,1fr)_auto]");
      expect(body).toHaveClass("min-h-0", "overflow-y-auto", "overflow-x-hidden");
      expect(within(form).getByRole("textbox", { name: "Custom arguments" })).toBeVisible();
      expect(body.compareDocumentPosition(footer) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    });
  });
});

/**
 * The dialog under a resource watch answering its execution targets, from the given `watch` or
 * else from the dialog's loader, which also answers the fresh read before a submit.
 */
function dialog(
  props: Partial<ComponentProps<typeof ModelDeployDialog>> = {},
  {
    watch = executionTargetsWatch(
      props.listConfiguredExecutionTargets ?? targetsLoader(poolTarget()),
    ),
  }: { watch?: FakeResourceWatch } = {},
) {
  const fits = compatibilityFitEstimates(props.model ?? model, props.listProfileCompatibility);
  return (
    <ExecutionTargetsWatch watch={watch}>
      <ModelDeployDialog
        engineReleases={releases}
        hardwareClasses={[]}
        listConfiguredExecutionTargets={targetsLoader(poolTarget())}
        model={model}
        onEstimate={fits.onEstimate}
        onOpenChange={vi.fn()}
        onPreview={vi.fn()}
        onSubmit={vi.fn()}
        open
        pending={false}
        {...props}
        listProfileCompatibility={fits.listProfileCompatibility}
      />
    </ExecutionTargetsWatch>
  );
}

function renderDialog(
  props: Partial<ComponentProps<typeof ModelDeployDialog>> = {},
  options: { watch?: FakeResourceWatch } = {},
) {
  return render(dialog(props, options));
}

function poolTarget({
  available = true,
  id = "pool",
  name = "NVIDIA GeForce RTX 5090",
  siteManager = true,
}: { available?: boolean; id?: string; name?: string; siteManager?: boolean } = {}) {
  return create(ConfiguredExecutionTargetSchema, {
    displayName: name,
    executionTargetId: id,
    platformLabel: "linux/amd64",
    siteNodeId: "node",
    // A target declaring no devices, so the site resolves it as not applicable.
    acceleratorResolution: AcceleratorResolutionStatus.NOT_APPLICABLE,
    workloadKind: ExecutionTargetWorkloadKind.MODEL,
    siteManager,
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: {
        availability: available
          ? { case: "available", value: {} }
          : { case: "unavailable", value: { code: "RUNTIME_UNAVAILABLE", message: "Offline" } },
      },
    },
  });
}

function workerTarget({ available = true }: { available?: boolean } = {}) {
  return create(ConfiguredExecutionTargetSchema, {
    ...poolTarget({ available, id: "worker-gpu", name: "NVIDIA GB10", siteManager: false }),
    siteNodeId: "worker",
    siteNodeDisplayName: "Worker",
  });
}

function labTarget() {
  return create(ConfiguredExecutionTargetSchema, {
    ...poolTarget({ id: "lab-gpu", name: "2× NVIDIA GeForce RTX 4090", siteManager: false }),
    siteNodeId: "lab",
    siteNodeDisplayName: "Lab",
  });
}

const poolPlacement = { executionTargetId: "pool", siteNodeId: "node" };
const workerPlacement = { executionTargetId: "worker-gpu", siteNodeId: "worker" };
const labPlacement = { executionTargetId: "lab-gpu", siteNodeId: "lab" };
// Lets pending reads and the effects they trigger finish under fake timers.
async function settle() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
}

/** Narrows the Runs on chip from this site to one machine. */
async function narrowRunsOn(user: UserEvent, machine: RegExp) {
  await user.click(await screen.findByRole("button", { name: "Runs on filter: this site" }));
  const menu = await screen.findByRole("menu");
  await user.click(within(menu).getByRole("menuitemcheckbox", { name: machine }));
  await user.keyboard("{Escape}");
}

async function pickTarget(user: UserEvent, name: RegExp) {
  await user.click(screen.getByRole("combobox", { name: "Machine" }));
  await user.click(screen.getByRole("option", { name }));
}

/** The profile rows' names, in display order. */
function profileRows() {
  return within(screen.getByRole("table", { name: "Model profiles" }))
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.getAttribute("aria-label") ?? row.textContent);
}

function targetsLoader(...executionTargets: ConfiguredExecutionTarget[]) {
  return () => Promise.resolve({ executionTargets });
}

/** An estimate loader previewing each profile's accelerator memory on the dedicated target. */
function profileEstimates(acceleratorBytes: Record<string, bigint>) {
  return ({ profileId }: { profileId: string }) =>
    Promise.resolve({
      placementPreview: dedicatedPreview({ accelerator: acceleratorBytes[profileId] }),
    });
}

function estimateRequest(profileId: string, contextSize: bigint) {
  return {
    configuration: {
      customArgumentTokens: [] as string[],
      engineParameterValues: {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: contextSize } },
        ],
      },
      modelArtifactBindingId: "",
      optionOverrides: [],
    },
    placement: create(ExactPlacementSchema, { siteNodeId: "node", executionTargetId: "pool" }),
    profileId,
  };
}

/** The estimate request of the KV offload model's untouched profile at a total context. */
function kvOffloadRequest(contextSize: bigint) {
  return {
    configuration: {
      customArgumentTokens: [] as string[],
      engineParameterValues: {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: contextSize } },
          { parameterId: "kv-offloading-size-gib", value: { case: "unsignedInteger", value: 16n } },
        ],
      },
      modelArtifactBindingId: "",
      optionOverrides: [],
    },
    placement: create(ExactPlacementSchema, { siteNodeId: "node", executionTargetId: "pool" }),
    profileId: "nvfp4-text",
  };
}

async function enableAdvanced(user: UserEvent) {
  await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
}

/** Moves from the Model & machine step to the basic Context step. */
async function openSettings(user: UserEvent) {
  await screen.findByText("Step 1 of 3 — Model & machine");
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 2 of 3 — Context")).toBeVisible();
}

async function goToDeploy(user: UserEvent) {
  await goToStep(user, "Review & deploy");
}

/** The numbered step header labels, in order. */
function stepLabels() {
  return Array.from(
    screen.getByRole("list", { name: "Deployment progress" }).querySelectorAll("[data-step-label]"),
    (label) => label.textContent,
  );
}

function stepButton(step: string) {
  return screen.getByRole("button", { name: new RegExp(`^\\d+\\. ${step}`) });
}

/** Label and value of each row of the Deploy pane summary, in display order. */
function summaryRows() {
  return detailRows(screen.getByRole("region", { name: "Summary" }).querySelector("dl")!);
}

/** Label and value of each row of one detail list, in display order. */
function detailRows(list: HTMLElement) {
  const labels = within(list).getAllByRole("term");
  const values = within(list).getAllByRole("definition");
  return labels.map((label, index) => [label.textContent, values[index]?.textContent]);
}
