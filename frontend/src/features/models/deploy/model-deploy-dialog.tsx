import type { RefObject } from "react";

import { Notice } from "@/components/ui/notice";
import { validateDeploymentName } from "@/domain/deployment-name";
import { AdvancedToggle } from "@/features/deployments/advanced-toggle";
import { DeploymentWizardShell } from "@/features/deployments/deployment-wizard-shell";
import { ModelDeployMemoryStrip } from "@/features/models/deploy/memory/model-deploy-memory-strip";
import { deriveRouteName } from "@/features/models/deployment-exposure";
import { DeploymentExposureFields } from "@/features/models/deployment-exposure-fields";
import { configurationSpeculativeOptions } from "@/features/models/profiles/model-row";
import { profileTargetVerdict } from "@/features/models/profiles/profile-compatibility";
import { profileCompatibilityStatus } from "@/features/models/profiles/profile-compatibility-copy";
import {
  speculativeChoiceError,
  speculativeMethodChoice,
} from "@/features/models/runtime/speculative-choice";

import { CommandPreviewDialog } from "./model-command-preview-dialog";
import { ModelDeployAdvancedStep } from "./model-deploy-advanced-step";
import { exactPlacement, moveToken } from "./model-deploy-launch";
import { ModelDeployProfileStep } from "./model-deploy-profile-step";
import { ModelDeploySettingsStep } from "./model-deploy-settings-step";
import { ModelDeploySummaryStep } from "./model-deploy-summary-step";
import { ModelDeployTargetStep } from "./model-deploy-target-step";
import { type ModelDeployFormOptions, useModelDeployForm } from "./use-model-deploy-form";

/**
 * Deploys a catalog model to a local execution target. Basic mode asks for the profile, the
 * settings (the context and the profile's basic parameters), and the deployment name, plus the
 * target when more than one can deploy the profile; the Advanced checkbox adds every target, the
 * advanced parameters and options, and routing without discarding entered values. The Settings step
 * and the profile table's "Sized for" edit one shared context.
 */
export function ModelDeployDialog(props: ModelDeployDialogProps) {
  const { error, model, onOpenChange, open, openerRef, pending } = props;
  const form = useModelDeployForm(props);
  const {
    advanced,
    commandPreview,
    compatibility,
    dirtyFields,
    effectiveTargetsError,
    executionTargets,
    executionTargetsCurrent,
    fieldErrors,
    memory,
    placement,
    selectedProfileRow,
    setFieldError,
    step,
    steps,
  } = form;

  const siteNotice = effectiveTargetsError ? (
    <Notice role="alert" tone="destructive">
      {effectiveTargetsError}
    </Notice>
  ) : executionTargetsCurrent && executionTargets.length === 0 ? (
    <Notice role="alert" tone="destructive">
      No model execution targets are configured. Configure one before deploying.
    </Notice>
  ) : executionTargetsCurrent && form.deployableExecutionTargets.length === 0 ? (
    <Notice role="alert" tone="destructive">
      No configured execution target can deploy models right now.
    </Notice>
  ) : undefined;
  const firstStepNotice =
    step !== steps[0] ? undefined : form.targetsLoadingVisible && !form.targetStepShown ? (
      <p className="text-xs text-muted-foreground" role="status">
        Loading configured execution targets…
      </p>
    ) : (
      siteNotice
    );
  const ineligibleNotice = form.ineligibleTarget ? (
    <Notice role="alert" tone="destructive">
      {`${form.ineligibleTarget} is no longer eligible to run this deployment.`}
    </Notice>
  ) : undefined;

  return (
    <DeploymentWizardShell
      announcement={form.announcement}
      currentStep={step}
      footerStart={
        <AdvancedToggle checked={advanced} disabled={pending} onChange={form.toggleAdvanced} />
      }
      onOpenChange={onOpenChange}
      onPrimary={form.handlePrimary}
      onStepChange={form.handleStepChange}
      open={open}
      openerRef={openerRef}
      pending={pending}
      persistentContent={
        memory.predictedMemory ? (
          <ModelDeployMemoryStrip memory={memory.predictedMemory} />
        ) : undefined
      }
      primaryDisabled={step === "Deploy" && form.deployDisabled}
      primaryLabel={step === "Deploy" ? (pending ? "Deploying…" : "Deploy") : "Next"}
      resourceColorKey={model.modelId}
      resourceName={model.displayName}
      steps={steps}
      submitError={step === "Deploy" ? error : undefined}
      title="Deploy model"
      visitedSteps={form.visitedSteps}
    >
      {step === "Target" ? (
        <ModelDeployTargetStep
          disabled={pending}
          error={fieldErrors.get("placement")}
          executionTargets={
            advanced ? executionTargets : (form.profileLaunchCandidates ?? executionTargets)
          }
          inputRef={form.executionTargetRef}
          loading={form.targetsLoading}
          loadingMessageVisible={form.targetsLoadingVisible && !effectiveTargetsError}
          loadingPresentation={form.targetsLoadingPresentation}
          notices={
            <>
              {ineligibleNotice}
              {!advanced && form.profileLaunchCandidates?.length === 0 ? (
                <Notice role="alert" tone="destructive">
                  No execution target can run this profile right now.
                </Notice>
              ) : null}
            </>
          }
          onBlur={() => {
            if (!dirtyFields.isDirty("placement")) return;
            setTimeout(
              () =>
                setFieldError("placement", placement ? undefined : "Execution target is required."),
              0,
            );
          }}
          onSelect={(target) => {
            dirtyFields.markDirty("placement");
            form.setPickedPlacement(target ? exactPlacement(target) : null);
            form.setIneligibleTarget(undefined);
            if (fieldErrors.has("placement"))
              setFieldError("placement", target ? undefined : "Execution target is required.");
            commandPreview.invalidatePreview();
          }}
          placement={placement}
          targetsCurrent={executionTargetsCurrent}
          unavailableReason={
            compatibility && selectedProfileRow
              ? (target) => {
                  const verdict = profileTargetVerdict(
                    compatibility,
                    model.modelId,
                    form.profileId,
                    target,
                  );
                  return verdict && verdict.status !== "runs"
                    ? profileCompatibilityStatus(verdict, {
                        profile: selectedProfileRow,
                        targets: executionTargets,
                      })?.text
                    : undefined;
                }
              : undefined
          }
        />
      ) : null}
      {step === "Profile" ? (
        <ModelDeployProfileStep
          compatibilityCheck={
            !compatibility && executionTargets.length
              ? form.profileCompatibility.error || form.profileCompatibility.compatibility
                ? "failed"
                : form.compatibilityLoadingVisible
                  ? "checking"
                  : undefined
              : undefined
          }
          error={fieldErrors.get("profileId")}
          notices={
            <>
              {firstStepNotice}
              {ineligibleNotice}
            </>
          }
          resetsRuntime={form.customizations.length > 0}
          sizingFailed={Boolean(
            form.scenario &&
            props.listProfileCompatibility &&
            !form.sizing &&
            !form.profileCompatibility.estimates.pending,
          )}
          table={{
            availability: form.profileTable.profileAvailability,
            chipSettings: form.profileTable.sizedForSettings,
            disabled: pending,
            estimatesPending: form.tableEstimates?.pending ?? false,
            inputRef: form.profileRef,
            onBlur: () => {
              if (dirtyFields.isDirty("profileId"))
                setTimeout(() => setFieldError("profileId", form.profileError), 0);
            },
            onRunsOnChange: form.profileTable.changeRunsOn,
            onSelect: form.chooseProfile,
            onShownChange: form.updateShownProfiles,
            profiles: form.shownProfileRows,
            runsOn: form.profileTable.runsOn,
            selectedProfileId: form.profileId,
            start: form.profileTable.start,
          }}
        />
      ) : null}
      {step === "Settings" ? (
        <ModelDeploySettingsStep
          contextHint={form.contextHint}
          disabled={pending}
          engineParameterRefs={form.engineParameterRefs}
          exposedParameters={form.contextParameters}
          fieldErrors={form.contextFieldErrors}
          filledContextPerRequest={memory.estimate.filledContextPerRequest}
          inputs={form.engineParameterInputs}
          largestFit={form.largestFit}
          model={model}
          onParameterBlur={form.blurContext}
          onParameterChange={form.changeContext}
          parameters={{
            onBlur: form.blurParameter,
            onChange: form.changeParameter,
            shown: form.settingsParameters,
          }}
          profile={form.profile}
        />
      ) : null}
      {step === "Advanced" ? (
        <ModelDeployAdvancedStep
          artifactRef={form.artifactRef}
          bindingId={form.bindingId}
          customArgumentTokens={form.customArgumentTokens}
          customArguments={{
            add: () => form.updateCustomArguments((tokens) => [...tokens, ""]),
            move: (index, direction) =>
              form.updateCustomArguments((tokens) => moveToken(tokens, index, direction)),
            remove: (index) =>
              form.updateCustomArguments((tokens) => tokens.filter((_, item) => item !== index)),
            update: (index, value) =>
              form.updateCustomArguments((tokens) =>
                tokens.map((token, item) => (item === index ? value : token)),
              ),
          }}
          disabled={pending}
          engineParameterRefs={form.engineParameterRefs}
          engineReleases={memory.engineReleaseLabels}
          exposedParameters={form.advancedParameters}
          fieldErrors={fieldErrors}
          hostKvOffloadSharesMemory={memory.hostKvOffloadSharesMemory}
          inputs={form.engineParameterInputs}
          model={model}
          onArtifactBlur={() => {
            if (!dirtyFields.isDirty("artifact")) return;
            setTimeout(
              () =>
                setFieldError("artifact", form.binding ? undefined : "Select a model artifact."),
              0,
            );
          }}
          onArtifactChange={(nextBindingId) => {
            dirtyFields.markDirty("artifact");
            form.setBindingId(nextBindingId);
            if (fieldErrors.has("artifact"))
              setFieldError("artifact", nextBindingId ? undefined : "Select a model artifact.");
            commandPreview.invalidatePreview();
          }}
          onParameterBlur={form.blurParameter}
          onParameterChange={form.changeParameter}
          onPreview={() => void form.runPreview()}
          previewButtonRef={commandPreview.previewButtonRef}
          previewDisabled={
            pending ||
            !form.exposedParameters.length ||
            !form.configuration ||
            form.deploymentPrerequisiteUnavailable
          }
          previewPending={commandPreview.previewPending}
          profile={form.profile}
          revealedErrors={form.revealedErrors}
          schemaExposesParameters={form.exposedParameters.length > 0}
          kvCachePrecision={{
            onChange: (precision) => {
              form.setPickedKvCachePrecision(precision);
              commandPreview.invalidatePreview();
            },
            option: form.kvCachePrecisionOption,
            precision: form.kvCachePrecision,
          }}
          speculative={{
            choice: form.speculativeChoice,
            countRef: form.speculativeCountRef,
            onCountChange: (count) => {
              const nextChoice = { ...form.speculativeChoice, count };
              dirtyFields.markDirty("speculativeCount");
              form.setSpeculativeChoice(nextChoice);
              if (fieldErrors.has("speculativeCount"))
                setFieldError(
                  "speculativeCount",
                  speculativeChoiceError(form.speculativeOptions, nextChoice),
                );
              commandPreview.invalidatePreview();
            },
            // A kept count can fall outside the new method's range, so the method change validates
            // it at once rather than waiting for a blur the count input never received.
            onMethodChange: (optionId) => {
              const nextChoice = speculativeMethodChoice(
                form.speculativeOptions,
                configurationSpeculativeOptions(model, form.configuration),
                form.speculativeChoice,
                optionId,
              );
              dirtyFields.markDirty("speculativeCount");
              form.setSpeculativeChoice(nextChoice);
              setFieldError(
                "speculativeCount",
                speculativeChoiceError(form.speculativeOptions, nextChoice),
              );
              commandPreview.invalidatePreview();
            },
            // Validates at once: a deferred check would run after a method change that the blur
            // precedes and overwrite its error with the previous method's verdict.
            onCountBlur: () => {
              if (dirtyFields.isDirty("speculativeCount"))
                setFieldError(
                  "speculativeCount",
                  speculativeChoiceError(form.speculativeOptions, form.speculativeChoice),
                );
            },
            options: form.speculativeOptions,
          }}
        />
      ) : null}
      {step === "Deploy" ? (
        <ModelDeploySummaryStep
          changedParameters={form.changedParameters}
          customizations={form.deployCustomizations}
          disabled={pending}
          exposureFields={
            <DeploymentExposureFields
              advanced={advanced}
              defaultRouteName={deriveRouteName(form.name)}
              disabled={pending}
              onChange={(next) => {
                if (next.routeName !== form.exposureValues.routeName)
                  dirtyFields.markDirty("routeName");
                form.setExposureValues(next);
                if (next.exposure === "hidden") setFieldError("routeName", undefined);
                else if (fieldErrors.has("routeName"))
                  setFieldError("routeName", form.validateRoute(next));
                commandPreview.invalidatePreview();
              }}
              onRouteNameBlur={(next) => {
                if (dirtyFields.isDirty("routeName"))
                  setFieldError("routeName", form.validateRoute(next));
              }}
              routeNameError={
                form.exposureValues.exposure === "direct"
                  ? (form.routeBlocking ?? fieldErrors.get("routeName"))
                  : undefined
              }
              routeNameRef={form.routeNameRef}
              values={form.exposureValues}
            />
          }
          filledContextPerRequest={memory.estimate.filledContextPerRequest}
          inputs={form.engineParameterInputs}
          launchValuesLoadingVisible={form.launchValuesLoadingVisible}
          memoryPreview={memory.memoryPreview}
          name={form.name}
          nameError={fieldErrors.get("name")}
          nameRef={form.nameRef}
          onNameBlur={() => {
            if (dirtyFields.isDirty("name"))
              setTimeout(() => setFieldError("name", validateDeploymentName(form.name)), 0);
          }}
          onNameChange={(nextName) => {
            dirtyFields.markDirty("name");
            form.setName(nextName);
            if (fieldErrors.has("name")) setFieldError("name", validateDeploymentName(nextName));
            if (form.following && fieldErrors.has("routeName"))
              setFieldError("routeName", form.validateRoute(form.exposureValues, nextName));
            commandPreview.invalidatePreview();
          }}
          poolBelowDeviceMinimum={memory.poolBelowDeviceMinimum}
          predictedMemory={memory.predictedMemory}
          profileName={form.profile?.displayName || form.profileId}
          siteNotice={siteNotice}
          target={form.selectedExecutionTarget}
        />
      ) : null}
      <CommandPreviewDialog
        error={commandPreview.previewError}
        loadingMessageVisible={commandPreview.previewLoadingVisible}
        onOpenChange={commandPreview.setPreviewOpen}
        onRetry={() => void form.runPreview()}
        open={commandPreview.previewOpen}
        openerRef={commandPreview.previewButtonRef}
        pending={commandPreview.previewPending}
        snapshot={commandPreview.preview}
      />
    </DeploymentWizardShell>
  );
}

export interface ModelDeployDialogProps extends ModelDeployFormOptions {
  error?: string;
  openerRef?: RefObject<HTMLElement | null>;
  onOpenChange: (open: boolean) => void;
}
