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
import {
  EngineArtifactReferenceSchema,
  EngineParameterValueSchema,
  EngineReleaseSchema,
} from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { ModelSchema } from "@/gen/dotcore/catalog/v1/model_pb";
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
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { ModelDeployDialog } from "./model-deploy-dialog";
import { sharedContextEstimate } from "./resource-estimate-test-support";
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

      await screen.findByText("Step 1 of 3 — Profile");
      expect(stepLabels()).toEqual(["1. Profile", "2. Context", "3. Deploy"]);
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

    it("asks for the target after the profile when more than one target is deployable", async () => {
      const user = setupUser();
      renderDialog({ listConfiguredExecutionTargets: targetsLoader(poolTarget(), workerTarget()) });

      await waitFor(() =>
        expect(stepLabels()).toEqual(["1. Profile", "2. Target", "3. Context", "4. Deploy"]),
      );
      expect(screen.getByText("Step 1 of 4 — Profile")).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 2 of 4 — Target")).toBeVisible();
      expect(screen.getByRole("combobox", { name: "Execution target" })).toHaveValue(
        "node / NVIDIA GeForce RTX 5090",
      );
      // The picker shows no target facts.
      expect(screen.queryByText("Configured devices")).not.toBeInTheDocument();
    });

    it("skips the target step when only one of several targets is deployable", async () => {
      renderDialog({
        listConfiguredExecutionTargets: targetsLoader(
          poolTarget(),
          workerTarget({ available: false }),
        ),
      });

      await screen.findByText("Step 1 of 3 — Profile");
      expect(stepLabels()).toEqual(["1. Profile", "2. Context", "3. Deploy"]);
    });

    it("shows every advanced step and never a review step", async () => {
      const user = setupUser();
      renderDialog();

      await screen.findByText("Step 1 of 3 — Profile");
      await enableAdvanced(user);

      expect(stepLabels()).toEqual([
        "1. Profile",
        "2. Target",
        "3. Context",
        "4. Runtime",
        "5. Deploy",
      ]);
      expect(screen.getByText("Step 1 of 5 — Profile")).toBeVisible();
      expect(screen.getByRole("status")).toHaveTextContent("Advanced on: 5 steps");
      for (let step = 0; step < 4; step += 1)
        await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 5 of 5 — Deploy")).toBeVisible();
      expect(screen.queryByText(/Review/)).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
    });

    it("opens with Advanced off every time", async () => {
      const user = setupUser();
      const { rerender } = renderDialog();

      await screen.findByText("Step 1 of 3 — Profile");
      await enableAdvanced(user);
      expect(screen.getByText("Step 1 of 5 — Profile")).toBeVisible();

      rerender(dialog({ open: false }));
      rerender(dialog({ open: true }));

      expect(screen.getByRole("checkbox", { name: "Advanced" })).not.toBeChecked();
      expect(stepLabels()).toEqual(["1. Profile", "2. Context", "3. Deploy"]);
    });
  });

  describe("Advanced toggle", () => {
    it("keeps entered values and the current step when toggled", async () => {
      const user = setupUser();
      renderDialog();

      await openAdvancedRuntime(user);
      await user.click(screen.getByRole("button", { name: "Back" }));
      const context = screen.getByRole("textbox", { name: /Total context/ });
      await user.clear(context);
      await user.type(context, "16384");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      const route = screen.getByRole("textbox", { name: "Route name" });
      await user.clear(route);
      await user.type(route, "custom-route");

      // Deploy exists in both modes, so it stays current.
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
      expect(screen.getByRole("status")).toHaveTextContent("Advanced off: 3 steps");
      expect(summaryRows()).toEqual([
        ["Target", "node / NVIDIA GeForce RTX 5090"],
        ["Profile", "Fast start"],
        ["Context", "16k total · 16k per request"],
      ]);
      expect(screen.getByRole("textbox", { name: "Route name" })).toHaveValue("custom-route");

      // Deploy still exists, so it stays current; every step before it can be reached.
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByText("Step 5 of 5 — Deploy")).toBeVisible();
      expect(stepButton("Target")).toBeEnabled();
      expect(stepButton("Context")).toBeEnabled();
      expect(stepButton("Runtime")).toBeEnabled();
      expect(stepButton("Profile")).toBeEnabled();

      expect(screen.getByRole("textbox", { name: "Route name" })).toHaveValue("custom-route");
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));
      expect(screen.getByRole("textbox", { name: /Total context/ })).toHaveValue("16384");
    });

    it("falls forward to Context when the target step vanishes", async () => {
      const user = setupUser();
      renderDialog();

      await screen.findByText("Step 1 of 3 — Profile");
      await enableAdvanced(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 2 of 5 — Target")).toBeVisible();

      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByText("Step 2 of 3 — Context")).toBeVisible();
    });

    it("falls back to Deploy when the runtime step vanishes", async () => {
      const user = setupUser();
      renderDialog();

      await openAdvancedRuntime(user);
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));

      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
      await waitFor(() =>
        expect(screen.getByRole("heading", { level: 2, name: "Deploy" })).toHaveFocus(),
      );
    });

    it("reveals invalid parallel requests with Advanced instead of deploying", async () => {
      const user = setupUser();
      const onSubmit = vi.fn();
      renderDialog({
        engineReleases: runtimeSchemaReleases,
        initialProfileId: "q4-xl",
        model: runtimeSchemaModel,
        onSubmit,
      });

      await openAdvancedRuntime(user);
      const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
      await user.clear(parallel);
      await user.type(parallel, "0");
      await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
      // The new step takes focus in an effect after it renders; acting earlier loses that move.
      await waitFor(() =>
        expect(document.activeElement).toHaveAttribute("data-deployment-step-heading"),
      );
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      expect(await screen.findByText("Step 4 of 5 — Runtime")).toBeVisible();
      expect(screen.getByRole("checkbox", { name: "Advanced" })).toBeChecked();
      const revealed = screen.getByRole("textbox", { name: "Parallel requests" });
      await waitFor(() => expect(revealed).toHaveFocus());
      expect(revealed).toHaveAttribute("aria-invalid", "true");
      expect(
        screen.getByText("Parallel requests must be an integer from 1 to 64 requests."),
      ).toBeVisible();
      expect(onSubmit).not.toHaveBeenCalled();
    });
  });

  describe("Deploy pane", () => {
    it("names the deployment and summarizes the untouched defaults", async () => {
      const user = setupUser();
      renderDialog();

      await openContext(user);
      await user.click(screen.getByRole("button", { name: "Next" }));

      expect(screen.getByRole("textbox", { name: "Deployment name" })).toHaveValue("Test Model");
      expect(summaryRows()).toEqual([
        ["Target", "node / NVIDIA GeForce RTX 5090"],
        ["Profile", "Fast start"],
        ["Context", "8k total · 8k per request"],
      ]);
    });

    it("flags the deployment name on blur only after it changes", async () => {
      const user = setupUser();
      renderDialog();

      await openContext(user);
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

      await openContext(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const name = screen.getByRole("textbox", { name: "Deployment name" });
      await user.clear(name);
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      expect(name).toHaveAccessibleDescription("Deployment name is required.");
      expect(name).toHaveFocus();
      expect(onSubmit).not.toHaveBeenCalled();
    });

    it("lists only customized runtime values", async () => {
      const user = setupUser();
      renderDialog({ engineReleases: multiParameterReleases, model: multiParameterModel });

      await openAdvancedRuntime(user);
      await expandSection(user, "Other parameters (1)");
      await user.click(screen.getByRole("switch", { name: "Prefix caching" }));
      await expandSection(user, "Custom arguments and command");
      await user.click(screen.getByRole("button", { name: "Add token" }));
      await user.type(screen.getByRole("textbox", { name: "Custom token 1" }), "--flag");
      await user.click(screen.getByRole("button", { name: "Next" }));

      expect(summaryRows()).toEqual([
        ["Target", "node / NVIDIA GeForce RTX 5090"],
        ["Profile", "NVFP4 text"],
        ["Context", "32k total · 32k per request"],
        ["Runtime", "Customized: Prefix caching enabled · 1 custom argument"],
      ]);
    });

    it("keeps changed parallel requests across a profile change without the reset hint", async () => {
      const user = setupUser();
      renderDialog({
        engineReleases: runtimeSchemaReleases,
        initialProfileId: "q4-xl",
        model: runtimeSchemaModel,
      });

      await openAdvancedRuntime(user);
      const parallel = screen.getByRole("textbox", { name: "Parallel requests" });
      await user.clear(parallel);
      await user.type(parallel, "2");
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));

      expect(screen.getByText("Step 1 of 5 — Profile")).toBeVisible();
      expect(screen.queryByText(profileResetHint)).not.toBeInTheDocument();
      await user.click(screen.getByRole("radio", { name: /Q4_K_XL without MTP/ }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveValue("2");
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows().find(([label]) => label === "Runtime")).toEqual([
        "Runtime",
        "Customized: Parallel requests 2 requests",
      ]);
    });

    it("hints that a profile change resets customized runtime settings and keeps the context", async () => {
      const user = setupUser();
      renderDialog();

      await screen.findByText("Step 1 of 3 — Profile");
      expect(screen.queryByText(profileResetHint)).not.toBeInTheDocument();
      await openAdvancedRuntime(user);
      await expandSection(user, "Engine and artifact");
      await user.click(screen.getByRole("combobox", { name: "Model artifact" }));
      await user.click((await screen.findAllByText("Large context"))[0]);
      await user.click(screen.getByRole("button", { name: "Back" }));
      const context = screen.getByRole("textbox", { name: /Total context/ });
      await user.clear(context);
      await user.type(context, "12000");
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));

      expect(screen.getByText(profileResetHint)).toBeVisible();
      await user.click(screen.getByRole("radio", { name: /Large context/ }));
      await user.click(screen.getByRole("radio", { name: /Fast start/ }));
      expect(screen.queryByText(profileResetHint)).not.toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("textbox", { name: /Total context/ })).toHaveValue("12000");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await expandSection(user, "Engine and artifact");
      expect(screen.getByRole("combobox", { name: "Model artifact" })).toHaveValue("Fast start");
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

      await screen.findByText("Step 1 of 3 — Profile");
      const profiles = screen.getByRole("table", { name: "Model profiles" });
      expect(
        within(profiles)
          .getAllByRole("columnheader")
          .map((header) => header.textContent),
      ).toEqual(["Selected", "Profile", "Memory", "Maximum context", "Total context"]);
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
        ["Large context", "Large contextQ8_0", "9 GiB", "—", "16k"],
        ["Fast start", "Fast startQ4_K_M", "6 GiB", "—", "8k"],
      ]);
      expect(screen.getByRole("searchbox", { name: "Filter profiles" })).toBeVisible();
    });

    it("shows Intelligence from the View menu, and Reset hides it again", async () => {
      const user = setupUser();
      renderDialog();

      await screen.findByText("Step 1 of 3 — Profile");
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
        ["Intelligence", "false"],
        ["Memory", "true"],
        ["Maximum context", "true"],
        ["Total context", "true"],
      ]);

      await user.click(within(menu).getByRole("menuitemcheckbox", { name: "Intelligence" }));
      await user.keyboard("{Escape}");
      expect(headers()).toEqual([
        "Selected",
        "Profile",
        "Intelligence",
        "Memory",
        "Maximum context",
        "Total context",
      ]);

      await user.click(screen.getByRole("button", { name: "Reset" }));
      expect(headers()).toEqual([
        "Selected",
        "Profile",
        "Memory",
        "Maximum context",
        "Total context",
      ]);
      expect(screen.queryByRole("button", { name: "Reset" })).not.toBeInTheDocument();
    });

    it("preselects the requested profile and deploys it", async () => {
      const user = setupUser();
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      renderDialog({ initialProfileId: "large", onSubmit });

      await screen.findByText("Step 1 of 3 — Profile");
      expect(screen.getByRole("radio", { name: "Large context" })).toBeChecked();
      expect(screen.getByRole("radio", { name: "Fast start" })).not.toBeChecked();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
      const [submitted] = onSubmit.mock.calls[0];
      expect(submitted.profileId).toBe("large");
      expect(submitted.configuration.modelArtifactBindingId).toBe("q8-binding");
    });

    it("selects a profile from its row and keeps the selection across steps", async () => {
      const user = setupUser();
      renderDialog();

      await screen.findByText("Step 1 of 3 — Profile");
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

      await screen.findByText("Step 1 of 3 — Profile");
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
      expect(stepLabels()).toEqual(["1. Profile", "2. Context", "3. Deploy"]);

      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()[0]).toEqual(["Target", "Worker / NVIDIA GB10"]);
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
      await nextToTarget(user);
      const picker = screen.getByRole("combobox", { name: "Execution target" });
      expect(picker).toHaveValue("");
      await user.click(picker);
      expect(
        screen.getAllByRole("option").map((option) => option.getAttribute("aria-label")),
      ).toEqual(["node / NVIDIA GeForce RTX 5090 linux/amd64", "Worker / NVIDIA GB10 linux/amd64"]);
      await user.keyboard("{Escape}");

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
      await nextToTarget(user);
      await pickTarget(user, /^Worker/);
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("row", { name: "Large context" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("combobox", { name: "Execution target" })).toHaveValue(
        "Worker / NVIDIA GB10",
      );

      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("row", { name: "Fast start" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await pickTarget(user, /^Lab/);
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("row", { name: "Large context" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("combobox", { name: "Execution target" })).toHaveValue("");
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
      expect(stepLabels()).toEqual(["1. Profile", "2. Context", "3. Deploy"]);

      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()[0]).toEqual(["Target", "Worker / NVIDIA GB10"]);
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
      await nextToTarget(user);
      await user.click(screen.getByRole("combobox", { name: "Execution target" }));
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

      await openAdvancedRuntime(user);
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("combobox", { name: "Execution target" }));
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
      expect(stepLabels()).toEqual(["1. Profile", "2. Context", "3. Deploy"]);
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.profileCompatibilityRefreshMs);
      });
      expect(loader).toHaveBeenCalledTimes(2);
      expect(stepLabels()).toEqual(["1. Profile", "2. Context", "3. Deploy"]);
      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();

      fireEvent.click(screen.getByRole("button", { name: "Back" }));
      fireEvent.click(screen.getByRole("button", { name: "Back" }));
      expect(stepLabels()).toEqual(["1. Profile", "2. Target", "3. Context", "4. Deploy"]);
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
      expect(screen.getByText("Step 1 of 3 — Profile")).toBeVisible();
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
      expect(screen.getByText("Step 1 of 3 — Profile")).toBeVisible();
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
      await nextToTarget(user);
      await pickTarget(user, /^Worker/);
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      expect(
        await screen.findByText(
          "Worker / NVIDIA GB10 is no longer eligible to run this deployment.",
        ),
      ).toBeVisible();
      expect(screen.getByText("Step 2 of 4 — Target")).toBeVisible();
      expect(screen.getByText("Execution target").nextElementSibling).toHaveTextContent("node");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(summaryRows()[0]).toEqual(["Target", "node / NVIDIA GeForce RTX 5090"]);
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
      expect(summaryRows()[0]).toEqual(["Target", "node / NVIDIA GeForce RTX 5090"]);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.profileCompatibilityRefreshMs);
      });
      expect(loader).toHaveBeenCalledTimes(2);
      expect(summaryRows()[0]).toEqual(["Target", "node / NVIDIA GeForce RTX 5090"]);
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
      expect(screen.getByText("Step 1 of 3 — Profile")).toBeVisible();
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
      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.profileCompatibilityRefreshMs);
      });
      fireEvent.click(screen.getByRole("button", { name: "Deploy" }));
      await settle();

      expect(screen.getByText("Step 1 of 3 — Profile")).toBeVisible();
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

      await waitFor(() =>
        expect(stepLabels()).toEqual(["1. Profile", "2. Target", "3. Context", "4. Deploy"]),
      );
      await user.click(screen.getByRole("button", { name: "Next" }));
      const picker = screen.getByRole("combobox", { name: "Execution target" });
      expect(picker).toHaveValue("node / NVIDIA GeForce RTX 5090 · GPU 1");

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

      await nextToTarget(user);
      const picker = screen.getByRole("combobox", { name: "Execution target" });
      expect(picker).toHaveValue("node / NVIDIA GeForce RTX 5090");

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

      await nextToTarget(user);
      expect(screen.getByRole("combobox", { name: "Execution target" })).toHaveAttribute(
        "readonly",
      );
      first.unmount();

      renderDialog({ listConfiguredExecutionTargets: targetsLoader(...targets(8)) });
      await nextToTarget(user);
      const searchable = screen.getByRole("combobox", { name: "Execution target" });
      expect(searchable).not.toHaveAttribute("readonly");
      await user.click(screen.getByRole("button", { name: "Clear Execution target" }));
      await user.type(searchable, "GPU 7");
      expect(
        screen.getAllByRole("option").map((option) => option.getAttribute("aria-label")),
      ).toEqual(["node / NVIDIA GeForce RTX 5090 · GPU 7 linux/amd64"]);
    });

    it("shows the only target as a line without target facts in advanced mode", async () => {
      const user = setupUser();
      renderDialog();

      await screen.findByText("Step 1 of 3 — Profile");
      await enableAdvanced(user);
      await user.click(screen.getByRole("button", { name: "Next" }));

      expect(screen.queryByRole("combobox", { name: "Execution target" })).not.toBeInTheDocument();
      expect(screen.getByText("Execution target").nextElementSibling).toHaveTextContent("node");
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

      expect(screen.getByText("Step 1 of 3 — Profile")).toBeVisible();
      fireEvent.click(screen.getByRole("checkbox", { name: "Advanced" }));
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 2 of 5 — Target")).toBeVisible();
      expect(screen.getByRole("combobox", { name: "Execution target" })).toBeDisabled();
      expect(screen.queryByText("Loading configured execution targets…")).not.toBeInTheDocument();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.loadingRevealMs);
      });
      expect(screen.getByText("Loading configured execution targets…")).toBeVisible();

      await act(async () => {
        resolvePools({ executionTargets: [poolTarget(), workerTarget()] });
        await Promise.resolve();
      });
      expect(screen.getByRole("combobox", { name: "Execution target" })).toBeEnabled();
      expect(screen.getByText("Loading configured execution targets…")).toBeVisible();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.loadingMinimumVisibleMs);
      });
      expect(screen.queryByText("Loading configured execution targets…")).not.toBeInTheDocument();
      expect(
        screen.getByRole("combobox", { name: "Execution target" }),
      ).not.toHaveAccessibleDescription();
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
      fireEvent.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByRole("combobox", { name: "Execution target" })).toHaveValue(
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
      expect(screen.getByRole("combobox", { name: "Execution target" })).toHaveValue("");
      expect(
        screen.getByText("Configured targets are visible, but none are deployable."),
      ).toBeVisible();

      await act(async () => {
        await watch.tick();
      });
      expect(
        screen.queryByText("Configured targets are visible, but none are deployable."),
      ).not.toBeInTheDocument();
      // The operator never chose, so the default target comes back with its availability.
      expect(screen.getByRole("combobox", { name: "Execution target" })).toHaveValue(
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

      await screen.findByText("Step 1 of 3 — Profile");
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
      expect(screen.getByText("Step 1 of 3 — Profile")).toBeVisible();
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

      await nextToTarget(user);
      const executionTarget = screen.getByRole("combobox", { name: "Execution target" });
      expect(executionTarget).toHaveValue("node / NVIDIA GeForce RTX 5090");

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

      for (let step = 0; step < 3; step += 1)
        await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 5 of 5 — Deploy")).toBeVisible();
      expect(summaryRows()[0]).toEqual(["Target", "Name of connected-worker / NVIDIA GB10"]);
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

      await nextToTarget(user);
      const executionTarget = screen.getByRole("combobox", { name: "Execution target" });
      expect(executionTarget).toHaveValue("node / NVIDIA GeForce RTX 5090");
      expect(executionTarget).not.toHaveAccessibleDescription();

      await user.click(screen.getByRole("button", { name: "Clear Execution target" }));
      expect(executionTarget).toHaveValue("");
      expect(executionTarget).toHaveAccessibleDescription("Select an execution target.");
      expect(screen.queryByText("Execution target is required.")).not.toBeInTheDocument();
      await user.tab();
      expect(await screen.findByText("Execution target is required.")).toBeVisible();

      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(executionTarget).toHaveAttribute("aria-invalid", "true");
      expect(executionTarget).toHaveFocus();
      expect(screen.getByText("Step 2 of 4 — Target")).toBeVisible();
    });
  });

  describe("notices", () => {
    it("blocks Deploy when no execution targets are configured", async () => {
      const user = setupUser();
      renderDialog({ listConfiguredExecutionTargets: targetsLoader() });

      await screen.findByText("Step 1 of 3 — Profile");
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

      await screen.findByText("Step 1 of 3 — Profile");
      const notice = "No configured execution target can deploy models right now.";
      expect(screen.getByText(notice)).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText(notice)).toBeVisible();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();

      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));
      await enableAdvanced(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 2 of 5 — Target")).toBeVisible();
      expect(screen.getByText("Execution target").nextElementSibling).toHaveTextContent(
        "node / NVIDIA GeForce RTX 5090 — Runtime is unavailable.",
      );
    });

    it("shows a taken derived route on the route field as the step opens and clears it on rename", async () => {
      const user = setupUser();
      renderDialog({ existingRouteNames: ["test-model"] });

      await openContext(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const route = screen.getByRole("textbox", { name: "Route name" });
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
      expect(route).toHaveAttribute("aria-invalid", "true");
      expect(route).toHaveAccessibleDescription(
        'An inference route named "test-model" already exists. Change the route or the deployment name. Follows the deployment name.',
      );

      await user.type(screen.getByRole("textbox", { name: "Deployment name" }), " 2");
      expect(screen.queryByText(/already exists/)).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
    });

    it("clears a taken derived route when the route is edited", async () => {
      const user = setupUser();
      renderDialog({ existingRouteNames: ["test-model"] });

      await openContext(user);
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

      await openContext(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const route = screen.getByRole("textbox", { name: "Route name" });
      await user.clear(route);
      await user.type(route, "taken");
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
      expect(route).toHaveAccessibleDescription(
        'An inference route named "taken" already exists. Custom route name. Use deployment name',
      );
    });

    it("shows a backend route conflict on the route field until the route changes", async () => {
      const user = setupUser();
      renderDialog({ routeConflict: { routeName: "test-model" } });

      await openContext(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      const route = screen.getByRole("textbox", { name: "Route name" });
      expect(screen.getByRole("button", { name: "Deploy" })).toBeDisabled();
      expect(route).toHaveAccessibleDescription(
        'An inference route named "test-model" already exists. Change the route or the deployment name. Follows the deployment name.',
      );

      await user.clear(route);
      await user.type(route, "other-route");
      expect(screen.queryByText(/already exists/)).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Deploy" })).toBeEnabled();
    });

    it("shows a submission error on the Deploy step only", async () => {
      const user = setupUser();
      renderDialog({ error: "Deploy was rejected (Internal): the model service is unavailable." });

      await screen.findByText("Step 1 of 3 — Profile");
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

      await screen.findByText("Step 1 of 3 — Profile");
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
      await openContext(user);
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
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
      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
      expect(screen.getByRole("radio", { name: "Hidden" })).toBeChecked();
      expect(route).toBeDisabled();
      expect(screen.queryByText("Follows the deployment name.")).not.toBeInTheDocument();
      expect(route).not.toHaveAccessibleDescription();

      await user.click(screen.getByRole("radio", { name: "Direct" }));
      expect(screen.queryByRole("radio", { name: "Hidden" })).not.toBeInTheDocument();
      expect(route).toBeEnabled();
      expect(route).toHaveAccessibleDescription("Follows the deployment name.");
    });

    it("follows the deployment name until edited, then keeps the custom value", async () => {
      const user = await openDeploy();
      const route = screen.getByRole("textbox", { name: "Route name" });

      expect(route).toHaveValue("test-model");
      expect(route).toHaveClass("text-muted-foreground");
      expect(route).toHaveAccessibleDescription("Follows the deployment name.");

      const name = screen.getByRole("textbox", { name: "Deployment name" });
      await user.clear(name);
      await user.type(name, "Support Bot");
      expect(route).toHaveValue("support-bot");

      await user.clear(route);
      await user.type(route, "support");
      expect(route).not.toHaveClass("text-muted-foreground");
      expect(route).toHaveAccessibleDescription("Custom route name. Use deployment name");

      await user.type(name, " v2");
      expect(route).toHaveValue("support");
    });

    it("returns to following with the link and when emptied on blur", async () => {
      const user = await openDeploy();
      const route = screen.getByRole("textbox", { name: "Route name" });

      await user.clear(route);
      await user.type(route, "support");
      await user.click(screen.getByRole("button", { name: "Use deployment name" }));
      expect(route).toHaveValue("test-model");
      expect(screen.getByText("Follows the deployment name.")).toBeVisible();

      await user.clear(route);
      expect(route).toHaveValue("");
      await user.tab();
      expect(route).toHaveValue("test-model");
      expect(screen.getByText("Follows the deployment name.")).toBeVisible();
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
        "Route name must start with a letter and contain only lowercase letters, numbers, hyphens, and periods. Custom route name. Use deployment name",
      );
    });

    it("shows the exposure choice above the route name in advanced mode", async () => {
      const user = setupUser();
      renderDialog();
      await screen.findByText("Step 1 of 3 — Profile");
      await enableAdvanced(user);
      for (let step = 0; step < 4; step += 1)
        await user.click(screen.getByRole("button", { name: "Next" }));

      const direct = screen.getByRole("radio", { name: "Direct" });
      const route = screen.getByRole("textbox", { name: "Route name" });
      expect(direct).toBeChecked();
      expect(direct.compareDocumentPosition(route) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    });

    it("disables the route name and clears its error while the deployment is hidden", async () => {
      const onSubmit = vi.fn<ComponentProps<typeof ModelDeployDialog>["onSubmit"]>();
      const user = setupUser();
      renderDialog({ onSubmit });
      await screen.findByText("Step 1 of 3 — Profile");
      await enableAdvanced(user);
      for (let step = 0; step < 4; step += 1)
        await user.click(screen.getByRole("button", { name: "Next" }));
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

      await openContext(user);
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

      await openContext(user);
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

      await screen.findByText("Step 1 of 3 — Profile");
      expect(screen.getByRole("dialog", { name: "Test Model — Deploy model" })).not.toHaveAttribute(
        "aria-describedby",
      );
      expect(screen.getByRole("radio", { name: /Fast start/ })).toBeChecked();
      expect(screen.queryByRole("textbox", { name: /Total context/ })).not.toBeInTheDocument();
      expect(screen.queryByRole("combobox", { name: "Execution target" })).not.toBeInTheDocument();
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
            modelArtifactBindingId: "q8-binding",
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
      await openAdvancedRuntime(user);
      await user.click(screen.getByRole("button", { name: "Back" }));
      // The hidden parallel requests stay off the Context step.
      expect(screen.queryByRole("textbox", { name: /Parallel requests/ })).not.toBeInTheDocument();
      const context = screen.getByRole("textbox", { name: /Total context/ });
      await user.clear(context);
      await user.type(context, "65536");
      await user.click(screen.getByRole("button", { name: "Next" }));
      // The unknown boolean stays in the generic collapsed list.
      await expandSection(user, "Other parameters (1)");
      const prefixCaching = screen.getByRole("switch", { name: "Prefix caching" });
      expect(prefixCaching).not.toBeChecked();
      // The schema description documents the engine and stays out of the field.
      expect(prefixCaching).not.toHaveAccessibleDescription();
      await user.click(prefixCaching);

      const expectedConfiguration = {
        customArgumentTokens: [],
        engineParameterValues: {
          values: [
            { parameterId: "total-context", value: { case: "unsignedInteger", value: 65536n } },
            create(EngineParameterValueSchema, {
              parameterId: "parallel-requests",
              value: { case: "unsignedInteger", value: 4n },
            }),
            create(EngineParameterValueSchema, {
              parameterId: "kv-cache-memory-bytes",
              value: { case: "unsignedInteger", value: 7400000000n },
            }),
            { parameterId: "enable-prefix-caching", value: { case: "boolean", value: true } },
          ],
        },
        modelArtifactBindingId: "nvfp4-binding",
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

      await expandSection(user, "Custom arguments and command");
      await user.click(screen.getByRole("button", { name: "Preview resolved command" }));
      await waitFor(() => expect(onPreview).toHaveBeenCalledOnce());
      await user.click(screen.getByRole("button", { name: "Close command preview" }));
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
      await openAdvancedRuntime(user);

      // The KV offload field surfaces the schema label and the profile default (16 GiB).
      const kvOffload = within(screen.getByRole("region", { name: "Host memory" })).getByRole(
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
        modelArtifactBindingId: "nvfp4-binding",
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

      await expandSection(user, "Custom arguments and command");
      await user.click(screen.getByRole("button", { name: "Preview resolved command" }));
      await waitFor(() => expect(onPreview).toHaveBeenCalledOnce());
      await user.click(screen.getByRole("button", { name: "Close command preview" }));
      await goToDeploy(user);
      expect(summaryRows()[3]).toEqual(["Runtime", "Customized: KV offloading size 32 GiB"]);
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

    it("edits exact ordered custom tokens and sends them unchanged to preview and deployment", async () => {
      const user = setupUser();
      const customSnapshot = create(ResolvedLocalModelDeploymentSnapshotSchema, {
        ...snapshot,
        customArgumentTokens: ["--flag=value", "  spaced  ", ""],
        desiredLaunchPlan: create(PortableModelLaunchPlanSchema, {
          arguments: [
            { value: { case: "literal", value: "--ctx-size" } },
            { value: { case: "literal", value: "8192" } },
            { value: { case: "literal", value: "--flag=value" } },
            { value: { case: "literal", value: "  spaced  " } },
            { value: { case: "literal", value: "" } },
          ],
        }),
      });
      const onPreview = vi.fn().mockResolvedValue(customSnapshot);
      const onSubmit = vi.fn();
      renderDialog({ onPreview, onSubmit });

      await openAdvancedRuntime(user);
      expect(screen.queryByText(/Expert use/)).not.toBeInTheDocument();
      await expandSection(user, "Custom arguments and command");
      await user.click(screen.getByRole("button", { name: "Add token" }));
      expect(screen.getByRole("textbox", { name: "Custom token 1" })).toHaveFocus();
      await user.type(screen.getByRole("textbox", { name: "Custom token 1" }), "  spaced  ");
      await user.click(screen.getByRole("button", { name: "Add token" }));
      await user.type(screen.getByRole("textbox", { name: "Custom token 2" }), "--flag=value");
      await user.click(screen.getByRole("button", { name: "Add token" }));
      await user.click(screen.getByRole("button", { name: "Move custom token 2 up" }));

      expect(screen.getByRole("textbox", { name: "Custom token 1" })).toHaveValue("--flag=value");
      expect(screen.getByRole("textbox", { name: "Custom token 2" })).toHaveValue("  spaced  ");
      expect(screen.getByRole("textbox", { name: "Custom token 3" })).toHaveValue("");
      const previewButton = screen.getByRole("button", { name: "Preview resolved command" });
      await user.click(previewButton);
      await waitFor(() => expect(onPreview).toHaveBeenCalledOnce());
      const previewDialog = screen.getByRole("dialog", { name: "Command preview" });
      await waitFor(() =>
        expect(
          within(previewDialog).getByRole("button", { name: "Close command preview" }),
        ).toHaveFocus(),
      );
      expect(
        Array.from(
          within(previewDialog)
            .getByRole("list", { name: "Desired argument tokens" })
            .querySelectorAll("li"),
          (item) => item.textContent,
        ),
      ).toEqual(['"--ctx-size"', '"8192"', '"--flag=value"', '"  spaced  "', '""']);
      await user.click(
        within(previewDialog).getByRole("button", { name: "Close command preview" }),
      );
      await waitFor(() => expect(previewButton).toHaveFocus());
      await goToDeploy(user);
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
          modelArtifactBindingId: "q4-binding",
          optionOverrides: [],
        },
        exposure: { kind: { case: "direct" as const, value: { routeName: "test-model" } } },
        name: "Test Model",
        profileId: "default",
        placement: create(ExactPlacementSchema, { siteNodeId: "node", executionTargetId: "pool" }),
      };
      expect(onPreview).toHaveBeenCalledWith(customValues);
      await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(customValues));

      await user.click(stepButton("Runtime"));
      await expandSection(user, "Custom arguments and command");
      await user.click(screen.getByRole("button", { name: "Remove custom token 1" }));
      expect(screen.getByRole("textbox", { name: "Custom token 1" })).toHaveValue("  spaced  ");
      expect(screen.queryByRole("dialog", { name: "Command preview" })).not.toBeInTheDocument();
    });

    it("keeps an untouched total context quiet on blur and flags it after a change", async () => {
      const user = setupUser();
      renderDialog();

      await openContext(user);
      const context = screen.getByRole("textbox", { name: /Total context/ });
      await user.click(context);
      await user.tab();
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 10));
      });
      expect(context).not.toHaveAttribute("aria-invalid");

      await user.clear(context);
      await user.tab();
      await waitFor(() => expect(context).toHaveAttribute("aria-invalid", "true"));
      expect(
        screen.getByText("Total context must be an integer from 1024 to 32768 tokens."),
      ).toBeVisible();
    });

    it("holds an invalid context on the Context step with its exact range", async () => {
      const user = setupUser();
      renderDialog();

      await openContext(user);
      const context = screen.getByRole("textbox", { name: /Total context/ });
      await user.clear(context);
      await user.type(context, "12");
      await user.tab();

      expect(
        await screen.findByText("Total context must be an integer from 1024 to 32768 tokens."),
      ).toBeVisible();
      expect(context).toHaveAttribute("aria-invalid", "true");
      expect(context).toHaveAccessibleDescription(
        "Memory the model holds for all requests at once. Total context must be an integer from 1024 to 32768 tokens.",
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
      await openContext(user);
      expect(onEstimate).toHaveBeenLastCalledWith(estimateRequest("default", "q4-binding", 8192n));

      await user.clear(screen.getByRole("textbox", { name: /Total context/ }));
      await user.type(screen.getByRole("textbox", { name: /Total context/ }), "9000");
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(2));
      expect(onEstimate).toHaveBeenLastCalledWith(estimateRequest("default", "q4-binding", 9000n));

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

    it("fences stale estimates after artifact and profile changes", async () => {
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
      await openAdvancedRuntime(user);
      await expandSection(user, "Engine and artifact");
      await user.click(screen.getByRole("combobox", { name: "Model artifact" }));
      await user.click((await screen.findAllByText("Large context"))[0]);
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(2));
      expect(onEstimate).toHaveBeenLastCalledWith(estimateRequest("default", "q8-binding", 8192n));
      resolvers[0]({ placementPreview: dedicatedPreview({ accelerator: 1n * gib }) });
      await waitFor(() =>
        expect(
          within(screen.getByRole("region", { name: "Predicted memory" })).queryByRole("button", {
            name: "VRAM 1 / 32 GiB",
          }),
        ).not.toBeInTheDocument(),
      );

      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(screen.getByRole("radio", { name: /Large context/ }));
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(3));
      expect(onEstimate).toHaveBeenLastCalledWith(estimateRequest("large", "q8-binding", 16384n));
      resolvers[1]({ placementPreview: dedicatedPreview({ accelerator: 2n * gib }) });
      await user.click(screen.getByRole("button", { name: "Next" }));
      await waitFor(() =>
        expect(
          within(screen.getByRole("region", { name: "Predicted memory" })).queryByRole("button", {
            name: "VRAM 2 / 32 GiB",
          }),
        ).not.toBeInTheDocument(),
      );
      resolvers[2]({ placementPreview: dedicatedPreview({ accelerator: 3n * gib }) });
      expect(
        await within(screen.getByRole("region", { name: "Predicted memory" })).findByRole(
          "button",
          { name: "VRAM 3 / 32 GiB" },
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
      await openAdvancedRuntime(user);
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
        .mockResolvedValueOnce({ placementPreview: dedicatedPreview({}) });
      renderDialog({ onEstimate, deployments: [] });

      await waitFor(() => expect(onEstimate).toHaveBeenCalledOnce());
      await openAdvancedRuntime(user);
      expect(
        await within(screen.getByRole("region", { name: "Predicted memory" })).findByRole(
          "button",
          { name: "VRAM 3.2 / 32 GiB" },
        ),
      ).toBeVisible();
      await expandSection(user, "Custom arguments and command");
      await user.click(screen.getByRole("button", { name: "Add token" }));
      await waitFor(() => expect(onEstimate).toHaveBeenCalledTimes(2));
      expect(onEstimate).toHaveBeenLastCalledWith({
        ...estimateRequest("default", "q4-binding", 8192n),
        configuration: {
          ...estimateRequest("default", "q4-binding", 8192n).configuration,
          customArgumentTokens: [""],
        },
      });
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

      await openAdvancedRuntime(user);
      expect(
        await within(screen.getByRole("region", { name: "Predicted memory" })).findByText(
          "Estimate unavailable.",
        ),
      ).toBeVisible();
      await expandSection(user, "Custom arguments and command");
      expect(screen.getByRole("button", { name: "Preview resolved command" })).toBeEnabled();
      expect(screen.getByRole("button", { name: "Next" })).toBeEnabled();
    });

    it("warns on host KV offload for a unified target", async () => {
      const user = setupUser();
      const onPreviewMemory = vi.fn().mockResolvedValue({
        acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
        advisories: [LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY],
      });
      renderDialog({
        onEstimate: vi.fn().mockResolvedValue({
          placementPreview: dedicatedPreview({ accelerator: 3_478_131_000n }),
        }),
        onPreviewMemory,
      });

      await openAdvancedRuntime(user);
      expect(await screen.findByText(unifiedKvOffloadWarning)).toBeVisible();
      expect(onPreviewMemory).toHaveBeenLastCalledWith({
        ...estimateRequest("default", "q4-binding", 8192n),
        placement: create(ExactPlacementSchema, { siteNodeId: "node", executionTargetId: "pool" }),
      });
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

      await openAdvancedRuntime(user);
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

      await openAdvancedRuntime(user);
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
      renderDialog({ onPreviewMemory });

      await openAdvancedRuntime(user);
      expect(await screen.findByText(unifiedKvOffloadWarning)).toBeVisible();

      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.clear(screen.getByRole("textbox", { name: /Total context/ }));
      await user.type(screen.getByRole("textbox", { name: /Total context/ }), "16384");
      await user.click(screen.getByRole("button", { name: "Next" }));
      await waitFor(() =>
        expect(screen.queryByText(unifiedKvOffloadWarning)).not.toBeInTheDocument(),
      );
      expect(onPreviewMemory).toHaveBeenLastCalledWith({
        ...estimateRequest("default", "q4-binding", 16384n),
        placement: create(ExactPlacementSchema, { siteNodeId: "node", executionTargetId: "pool" }),
      });
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

      await openAdvancedRuntime(user);
      await waitFor(() => expect(onPreviewMemory).toHaveBeenCalled());
      expect(screen.queryByText(unifiedKvOffloadWarning)).not.toBeInTheDocument();
      // Background failures stay quiet; the explicit command preview reports them.
      expect(screen.queryByText("Execution target is unavailable.")).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Next" })).toBeEnabled();
    });
  });

  describe("command preview", () => {
    it("keeps Previewing immediate while delaying command-resolution feedback", async () => {
      setTiming(productionTiming);
      vi.useFakeTimers();
      let resolvePreview!: (value: typeof snapshot) => void;
      const onPreview = vi.fn(
        () =>
          new Promise<typeof snapshot>((resolve) => {
            resolvePreview = resolve;
          }),
      );
      renderDialog({ onPreview });
      await openAdvancedRuntimeWithFakeTimers();
      fireEvent.click(screen.getByRole("button", { name: "Custom arguments and command" }));

      fireEvent.click(screen.getByRole("button", { name: "Preview resolved command" }));
      const previewDialog = screen.getByRole("dialog", { name: "Command preview" });
      expect(screen.getByText("Previewing…")).toBeVisible();
      expect(onPreview).toHaveBeenCalledTimes(1);
      expect(within(previewDialog).queryByText("Resolving command…")).not.toBeInTheDocument();
      expect(within(previewDialog).queryByText("No command is available.")).not.toBeInTheDocument();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.loadingRevealMs);
      });
      expect(within(previewDialog).getByRole("status")).toHaveTextContent("Resolving command…");

      await act(async () => {
        resolvePreview(snapshot);
        await Promise.resolve();
      });
      expect(within(previewDialog).getByRole("status")).toHaveTextContent("Resolving command…");
      expect(
        within(previewDialog).queryByRole("list", { name: "Desired argument tokens" }),
      ).not.toBeInTheDocument();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(productionTiming.loadingMinimumVisibleMs);
      });
      expect(within(previewDialog).queryByText("Resolving command…")).not.toBeInTheDocument();
      expect(
        within(previewDialog).getByRole("list", { name: "Desired argument tokens" }),
      ).toBeVisible();
    });

    it("shows preview errors in the command modal and keeps valid deployment available", async () => {
      const user = setupUser();
      const onPreview = vi
        .fn()
        .mockRejectedValueOnce(new Error("Execution target is unavailable."))
        .mockResolvedValueOnce(snapshot);
      renderDialog({ onPreview });

      await openAdvancedRuntime(user);
      await expandSection(user, "Custom arguments and command");
      await user.click(screen.getByRole("button", { name: "Preview resolved command" }));
      const previewDialog = screen.getByRole("dialog", { name: "Command preview" });
      await within(previewDialog).findByText("Execution target is unavailable.");

      await user.click(within(previewDialog).getByRole("button", { name: "Retry" }));
      expect(
        await within(previewDialog).findByRole("list", { name: "Desired argument tokens" }),
      ).toBeVisible();
      expect(onPreview).toHaveBeenCalledTimes(2);
      await user.click(
        within(previewDialog).getByRole("button", { name: "Close command preview" }),
      );
      expect(screen.getByRole("button", { name: "Next" })).toBeEnabled();
    });

    it("does not render a stale preview after its form values change", async () => {
      const user = setupUser();
      let resolvePreview: ((value: typeof snapshot) => void) | undefined;
      const onPreview = vi.fn(
        () =>
          new Promise<typeof snapshot>((resolve) => {
            resolvePreview = resolve;
          }),
      );
      renderDialog({ onPreview });

      await openAdvancedRuntime(user);
      await expandSection(user, "Custom arguments and command");
      await user.click(screen.getByRole("button", { name: "Preview resolved command" }));
      expect(onPreview).toHaveBeenCalledOnce();
      await user.keyboard("{Escape}");
      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.clear(screen.getByRole("textbox", { name: /Total context/ }));
      await user.type(screen.getByRole("textbox", { name: /Total context/ }), "16384");
      await user.click(screen.getByRole("button", { name: "Next" }));

      resolvePreview?.(snapshot);
      await expandSection(user, "Custom arguments and command");
      await waitFor(() =>
        expect(screen.getByRole("button", { name: "Preview resolved command" })).toBeEnabled(),
      );
      expect(screen.queryByRole("dialog", { name: "Command preview" })).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Next" })).toBeEnabled();
    });

    it("shows the fresh backend-resolved command in a bounded details-style modal", async () => {
      const user = setupUser();
      renderDialog({ onPreview: vi.fn().mockResolvedValue(snapshot) });

      await openAdvancedRuntime(user);
      await expandSection(user, "Custom arguments and command");
      const previewButton = screen.getByRole("button", { name: "Preview resolved command" });
      await user.click(previewButton);
      const previewDialog = screen.getByRole("dialog", { name: "Command preview" });

      expect(previewDialog).toHaveClass("max-w-sm");
      expect(within(previewDialog).queryByText(/One exact argv token per line/)).toBeNull();
      expect(within(previewDialog).getByRole("region", { name: "Desired arguments" })).toHaveClass(
        "max-h-[min(24rem,60dvh)]",
        "overflow-y-auto",
        "overflow-x-hidden",
      );
      expect(
        Array.from(
          within(previewDialog)
            .getByRole("list", { name: "Desired argument tokens" })
            .querySelectorAll("li"),
          (item) => item.textContent,
        ),
      ).toEqual(['"--ctx-size"', '"8192"']);
      await user.keyboard("{Escape}");
      expect(previewDialog).not.toBeInTheDocument();
      await waitFor(() => expect(previewButton).toHaveFocus());
    });
  });

  describe("memory strip", () => {
    it("follows the selected profile's estimate on every step", async () => {
      const user = setupUser();
      renderDialog({
        onEstimate: profileEstimates({ default: 8n * gib, large: 14n * gib }),
        deployments: [],
      });

      await screen.findByText("Step 1 of 3 — Profile");
      const strip = await screen.findByRole("region", { name: "Predicted memory" });
      expect(screen.getByTestId("deployment-dialog-persistent")).toContainElement(strip);
      // The default target applies once the targets load.
      expect(await within(strip).findByText("node / NVIDIA GeForce RTX 5090")).toBeVisible();
      expect(await within(strip).findByRole("button", { name: "VRAM 8 / 32 GiB" })).toBeVisible();

      await user.click(screen.getByRole("radio", { name: /Large context/ }));
      expect(await within(strip).findByRole("button", { name: "VRAM 14 / 32 GiB" })).toBeVisible();

      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
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

      await nextToTarget(user);
      const picker = screen.getByRole("combobox", { name: "Execution target" });
      expect(picker).toHaveValue("node / NVIDIA GeForce RTX 5090");
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

      await nextToTarget(user);
      const picker = screen.getByRole("combobox", { name: "Execution target" });
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

      await screen.findByText("Step 1 of 3 — Profile");
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
      expect(screen.getByText("Up to 16k on node / NVIDIA GeForce RTX 5090")).toBeVisible();
      expect(screen.getByRole("textbox", { name: /Total context/ })).toHaveValue("16384");
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
      expect(screen.getByText("Step 3 of 3 — Deploy")).toBeVisible();
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

      await screen.findByText("Step 1 of 3 — Profile");
      expect(await screen.findByText("Estimate unavailable.")).toBeVisible();
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Next" }));
      await user.click(screen.getByRole("button", { name: "Deploy" }));

      await waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    });
  });

  describe("layout", () => {
    it("places the deployment popup above its modal overlay", () => {
      render(dialog({ listConfiguredExecutionTargets: undefined }));

      expectResourceMarker(
        inspectorTitle(screen.getByRole("dialog", { name: "Test Model — Deploy model" })),
        "model-1",
      );
      expect(screen.getByRole("dialog", { name: "Test Model — Deploy model" })).toHaveStyle({
        zIndex: "calc(var(--layer-modal) + 1)",
      });
    });

    it("uses a bounded modal shell and groups runtime fields by responsibility", async () => {
      const user = setupUser();
      renderDialog();

      const modal = screen.getByRole("dialog", { name: "Test Model — Deploy model" });
      const body = within(modal).getByTestId("deployment-dialog-body");
      await screen.findByText("Step 1 of 3 — Profile");
      expect(within(body).getByRole("table", { name: "Model profiles" })).toBeVisible();
      await openAdvancedRuntime(user);
      const form = within(body).getByTestId("deployment-form");
      const footer = within(modal).getByTestId("deployment-dialog-footer");

      expect(modal).toHaveClass("overflow-hidden");
      expect(modal).toHaveClass("grid-rows-[auto_minmax(0,1fr)_auto]");
      expect(body).toHaveClass("min-h-0", "overflow-y-auto", "overflow-x-hidden");
      expect(
        within(form)
          .getAllByRole("button", { expanded: false })
          .map((toggle) => toggle.textContent),
      ).toEqual(["Engine and artifact", "Custom arguments and command"]);
      await expandSection(user, "Engine and artifact");
      expect(detailRows(within(form).getByText("Configuration").closest("dl")!)).toEqual([
        ["Configuration", "cuda"],
        ["Engine", "llama / 1"],
        ["Weight quantization", "Q4_K_M"],
      ]);
      expect(within(form).queryByRole("combobox", { name: "Model profile" })).toBeNull();
      expect(body.compareDocumentPosition(footer) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    });

    it("lists every approved engine release until the target's preview selects one", async () => {
      const user = setupUser();
      const twoReleases = clone(ModelSchema, model);
      twoReleases.configurations[0].engineArtifacts.push(
        create(EngineArtifactReferenceSchema, {
          engineArtifactId: "metal",
          engineReleaseId: "llama-metal",
        }),
      );
      type MemoryPreviewLoader = NonNullable<
        ComponentProps<typeof ModelDeployDialog>["onPreviewMemory"]
      >;
      let resolvePreview: (preview: Awaited<ReturnType<MemoryPreviewLoader>>) => void = () =>
        undefined;
      renderDialog({
        engineReleases: [
          ...releases,
          create(EngineReleaseSchema, { engineReleaseId: "llama-metal", version: "2" }),
        ],
        model: twoReleases,
        onPreviewMemory: vi.fn<MemoryPreviewLoader>(
          () =>
            new Promise((resolve) => {
              resolvePreview = resolve;
            }),
        ),
      });
      await openAdvancedRuntime(user);
      await expandSection(user, "Engine and artifact");
      const engine = () =>
        detailRows(screen.getByText("Configuration").closest("dl")!).find(
          ([label]) => label === "Engine",
        );

      expect(engine()).toEqual(["Engine", "llama / 1, llama-metal / 2"]);
      act(() =>
        resolvePreview({
          acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture.DEDICATED,
          advisories: [],
          engineRelease: { engineReleaseId: "llama-metal", version: "2" },
        }),
      );
      await waitFor(() => expect(engine()).toEqual(["Engine", "llama-metal / 2"]));
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
  return (
    <ExecutionTargetsWatch watch={watch}>
      <ModelDeployDialog
        engineReleases={releases}
        hardwareClasses={[]}
        listConfiguredExecutionTargets={targetsLoader(poolTarget())}
        model={model}
        onEstimate={sharedContextEstimate(props.model ?? model)}
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

/** Waits for the Profile step to offer a Target step, then goes to it. */
async function nextToTarget(user: UserEvent) {
  await waitFor(() => expect(stepLabels()).toContain("2. Target"));
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText(/^Step 2 of \d — Target$/)).toBeVisible();
}

async function pickTarget(user: UserEvent, name: RegExp) {
  await user.click(screen.getByRole("combobox", { name: "Execution target" }));
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

function estimateRequest(profileId: string, bindingId: string, contextSize: bigint) {
  return {
    configuration: {
      customArgumentTokens: [] as string[],
      engineParameterValues: {
        values: [
          { parameterId: "total-context", value: { case: "unsignedInteger", value: contextSize } },
        ],
      },
      modelArtifactBindingId: bindingId,
      optionOverrides: [],
    },
    placement: create(ExactPlacementSchema, { siteNodeId: "node", executionTargetId: "pool" }),
    profileId,
  };
}

async function enableAdvanced(user: UserEvent) {
  await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
}

/** Expands one collapsed Runtime section, such as "Custom arguments and command". */
async function expandSection(user: UserEvent, name: string) {
  await user.click(screen.getByRole("button", { name, expanded: false }));
}

/** Moves from the Profile step to the basic Context step. */
async function openContext(user: UserEvent) {
  await screen.findByText("Step 1 of 3 — Profile");
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 2 of 3 — Context")).toBeVisible();
}

async function openAdvancedRuntime(user: UserEvent) {
  await screen.findByText("Step 1 of 3 — Profile");
  await enableAdvanced(user);
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 2 of 5 — Target")).toBeVisible();
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 3 of 5 — Context")).toBeVisible();
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 4 of 5 — Runtime")).toBeVisible();
}

async function openAdvancedRuntimeWithFakeTimers() {
  await act(async () => {
    await Promise.resolve();
  });
  expect(screen.getByText("Step 1 of 3 — Profile")).toBeVisible();
  fireEvent.click(screen.getByRole("checkbox", { name: "Advanced" }));
  fireEvent.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 2 of 5 — Target")).toBeVisible();
  fireEvent.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 3 of 5 — Context")).toBeVisible();
  fireEvent.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 4 of 5 — Runtime")).toBeVisible();
}

async function goToDeploy(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Next" }));
  expect(screen.getByText("Step 5 of 5 — Deploy")).toBeVisible();
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
