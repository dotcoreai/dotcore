import { type RefObject, useEffect, useRef } from "react";

import { Notice } from "@/components/ui/notice";
import { validateDeploymentName } from "@/domain/deployment-name";
import { deployWizardShellProps } from "@/features/deployments/deploy-wizard-shell-props";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import { DeploymentWizardShell } from "@/features/deployments/deployment-wizard-shell";
import { ModelDeployMemoryStrip } from "@/features/models/deploy/memory/model-deploy-memory-strip";
import { deriveRouteName } from "@/features/models/deployment-exposure";
import { DeploymentExposureFields } from "@/features/models/deployment-exposure-fields";
import { profileTargetVerdict } from "@/features/models/profiles/profile-compatibility";
import { profileCompatibilityStatus } from "@/features/models/profiles/profile-compatibility-copy";

import { ModelDeployContextStep } from "./model-deploy-context-step";
import { ModelDeployEngineStep } from "./model-deploy-engine-step";
import type { ModelDeployFormOptions } from "./model-deploy-form-options";
import { exactPlacement } from "./model-deploy-launch";
import { ModelDeployMachineStep } from "./model-deploy-machine-step";
import { ModelDeployOptionFields } from "./model-deploy-option-fields";
import { ModelDeployPerformanceStep } from "./model-deploy-performance-step";
import { ModelDeploySummaryStep } from "./model-deploy-summary-step";
import { useModelDeployForm } from "./use-model-deploy-form";

/**
 * Deploys a catalog model to a local execution target. Basic mode asks for the machine and
 * profile, the capabilities the catalog marks basic, the context, and the deployment name; the
 * Advanced checkbox adds the performance and memory settings and the engine's command and custom
 * arguments, and turning it off returns those to the profile's values. The opener receives the
 * launch on submit and can name the last action after what it does with it, such as keeping the
 * choices without deploying.
 */
export function ModelDeployDialog(props: ModelDeployDialogProps) {
  const { error, labels = deployLabels, model, onOpenChange, open, openerRef, pending } = props;
  const form = useModelDeployForm(props);
  const {
    context,
    customArguments,
    dirtyFields,
    fieldErrors,
    machine,
    memory,
    modelOptions,
    parameters,
    revealEngine,
    setFieldError,
    wizard,
  } = form;
  const { compatibility, executionTargets, placement, selectedProfileRow } = machine;
  const { advanced, step } = wizard;
  const disabled = pending;

  // A create the backend rejects while custom arguments are set returns to them, since they are
  // the likeliest cause. Only a new rejection moves the operator, so the rest is read as it stands.
  const latestRef = useRef({ hasArguments: false, revealEngine });
  useEffect(() => {
    latestRef.current = { hasArguments: customArguments.tokens.length > 0, revealEngine };
  });
  useEffect(() => {
    const latest = latestRef.current;
    if (!error || !latest.hasArguments) return;
    latest.revealEngine();
  }, [error]);

  const siteNotice = machine.effectiveTargetsError ? (
    <Notice role="alert" tone="destructive">
      {machine.effectiveTargetsError}
    </Notice>
  ) : machine.executionTargetsCurrent && executionTargets.length === 0 ? (
    <Notice role="alert" tone="destructive">
      No model execution targets are configured. Configure one before deploying.
    </Notice>
  ) : machine.executionTargetsCurrent && machine.deployableExecutionTargets.length === 0 ? (
    <Notice role="alert" tone="destructive">
      No configured execution target can deploy models right now.
    </Notice>
  ) : undefined;
  const ineligibleNotice = machine.ineligibleTarget ? (
    <Notice role="alert" tone="destructive">
      {`${machine.ineligibleTarget} is no longer eligible to run this deployment.`}
    </Notice>
  ) : undefined;

  return (
    <DeploymentWizardShell
      {...deployWizardShellProps(wizard, { labels, pending })}
      onOpenChange={onOpenChange}
      open={open}
      openerRef={openerRef}
      pending={pending}
      persistentContent={
        memory.predictedMemory ? (
          <ModelDeployMemoryStrip memory={memory.predictedMemory} />
        ) : undefined
      }
      primaryDisabled={step === "Review & deploy" && form.deployDisabled}
      resourceColorKey={model.modelId}
      resourceName={model.displayName}
      submitError={step === "Review & deploy" || step === "Engine" ? error : undefined}
      title={labels.title}
    >
      {step === "Model & machine" ? (
        <ModelDeployMachineStep
          basis={machine.basis}
          compatibilityCheck={
            !compatibility && executionTargets.length
              ? machine.tableRead.error || machine.tableRead.compatibility
                ? "failed"
                : machine.compatibilityLoadingVisible
                  ? "checking"
                  : undefined
              : undefined
          }
          error={fieldErrors.get("profileId")}
          notices={
            <>
              {siteNotice}
              {ineligibleNotice}
            </>
          }
          onBasisChange={compatibility ? machine.setBasis : undefined}
          resetsRuntime={form.resetsRuntime}
          sizingFailed={machine.tableSizingFailed}
          table={{
            availability: machine.profileTable.profileAvailability,
            disabled,
            estimatesPending: machine.tableEstimates?.pending ?? false,
            inputRef: form.profileRef,
            onBlur: () => {
              if (dirtyFields.isDirty("profileId"))
                setTimeout(() => setFieldError("profileId", machine.profileError), 0);
            },
            onRunsOnChange: machine.profileTable.changeRunsOn,
            onSelect: machine.chooseProfile,
            onShownChange: machine.updateShownProfiles,
            profiles: machine.profileTable.rows,
            runsOn: machine.profileTable.runsOn,
            selectedProfileId: machine.profileId,
            start: machine.profileTable.start,
          }}
          target={{
            disabled,
            error: fieldErrors.get("placement"),
            // Basic mode lists the targets that can launch the profile in the Runs on scope;
            // Advanced lists every target, with those that can't run it disabled.
            executionTargets: advanced
              ? executionTargets
              : (machine.profileLaunchCandidates ?? executionTargets),
            inputRef: form.executionTargetRef,
            loading: machine.targetsLoading,
            loadingMessageVisible: machine.targetsLoadingVisible && !machine.effectiveTargetsError,
            loadingPresentation: machine.targetsLoadingPresentation,
            onBlur: () => {
              if (!dirtyFields.isDirty("placement")) return;
              setTimeout(
                () =>
                  setFieldError(
                    "placement",
                    placement ? undefined : "Execution target is required.",
                  ),
                0,
              );
            },
            onSelect: (target) => {
              dirtyFields.markDirty("placement");
              machine.setPickedPlacement(target ? exactPlacement(target) : null);
              machine.setIneligibleTarget(undefined);
              if (fieldErrors.has("placement"))
                setFieldError("placement", target ? undefined : "Execution target is required.");
            },
            placement,
            targetsCurrent: machine.executionTargetsCurrent,
            unavailableReason:
              compatibility && selectedProfileRow
                ? (target) => {
                    const verdict = profileTargetVerdict(
                      compatibility,
                      model.modelId,
                      machine.profileId,
                      target,
                    );
                    return verdict && verdict.status !== "runs"
                      ? profileCompatibilityStatus(verdict, {
                          profile: selectedProfileRow,
                          targets: executionTargets,
                        })?.text
                      : undefined;
                  }
                : undefined,
          }}
        />
      ) : null}
      {step === "Capabilities" ? (
        <DeploymentForm className="gap-4">
          <DeploymentStepHeading>Capabilities</DeploymentStepHeading>
          <ModelDeployOptionFields
            controls={modelOptions.basicControls}
            disabled={disabled}
            fieldErrors={fieldErrors}
            options={modelOptions}
          />
        </DeploymentForm>
      ) : null}
      {step === "Context" ? (
        <ModelDeployContextStep
          contextFits={context.contextFits}
          contextHints={context.contextHints}
          disabled={disabled}
          engineParameterRefs={parameters.engineParameterRefs}
          exposedParameters={parameters.contextParameters}
          fieldErrors={context.contextFieldErrors}
          filledContextPerRequest={memory.estimate.filledContextPerRequest}
          inputs={parameters.engineParameterInputs}
          memoryParameterSizes={memory.estimate.memoryParameterSizes}
          model={model}
          onFollowTotal={context.followTotal}
          onParameterBlur={context.blurContext}
          onParameterChange={context.changeContext}
          parameters={{
            onBlur: parameters.blurParameter,
            groups: machine.engineRelease?.parameterSchema?.parameterGroups ?? [],
            onChange: parameters.changeParameter,
            revealedErrors: wizard.revealedErrors,
            shown: parameters.basicParameters,
          }}
          profile={machine.profile}
          totalFollows={context.totalFollows}
        />
      ) : null}
      {step === "Performance & memory" ? (
        <ModelDeployPerformanceStep
          engineParameterRefs={parameters.engineParameterRefs}
          exposedParameters={parameters.advancedParameters}
          fieldErrors={fieldErrors}
          hostKvOffloadSharesMemory={memory.hostKvOffloadSharesMemory}
          inputs={parameters.engineParameterInputs}
          memoryParameterSizes={memory.estimate.memoryParameterSizes}
          onParameterBlur={parameters.blurParameter}
          onParameterChange={parameters.changeParameter}
          options={
            <ModelDeployOptionFields
              controls={modelOptions.advancedControls}
              disabled={disabled}
              fieldErrors={fieldErrors}
              options={modelOptions}
            />
          }
          parameterGroups={machine.engineRelease?.parameterSchema?.parameterGroups ?? []}
          revealedErrors={wizard.revealedErrors}
        />
      ) : null}
      {step === "Engine" ? (
        <ModelDeployEngineStep
          blockedBy={form.commandBlockedBy}
          customArgumentTokens={customArguments.tokens}
          disabled={disabled}
          inputRef={customArguments.inputRef}
          onTextChange={customArguments.setText}
          preview={form.commandPreview.result}
          text={customArguments.text}
          updating={form.commandPreview.updating}
        />
      ) : null}
      {step === "Review & deploy" ? (
        <ModelDeploySummaryStep
          capabilities={modelOptions.summary}
          changedParameters={parameters.changedParameters}
          customizations={form.summaryCustomizations}
          disabled={disabled}
          exposureFields={
            <DeploymentExposureFields
              advanced={advanced}
              defaultRouteName={deriveRouteName(form.name)}
              disabled={disabled}
              onChange={(next) => {
                if (next.routeName !== form.exposureValues.routeName)
                  dirtyFields.markDirty("routeName");
                form.setExposureValues(next);
                if (next.exposure === "hidden") setFieldError("routeName", undefined);
                else if (fieldErrors.has("routeName"))
                  setFieldError("routeName", form.validateRoute(next));
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
          inputs={parameters.engineParameterInputs}
          launchValuesLoadingVisible={machine.launchValuesLoadingVisible}
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
          }}
          poolBelowDeviceMinimum={memory.poolBelowDeviceMinimum}
          predictedMemory={memory.predictedMemory}
          profileName={machine.profile?.displayName || machine.profileId}
          siteNotice={siteNotice}
          target={machine.selectedExecutionTarget}
          weights={selectedProfileRow?.quantization}
        />
      ) : null}
    </DeploymentWizardShell>
  );
}

export interface ModelDeployDialogProps extends ModelDeployFormOptions {
  error?: string;
  /** The dialog's title and its last step's action; deploying a model without them. */
  labels?: ModelDeployLabels;
  openerRef?: RefObject<HTMLElement | null>;
  onOpenChange: (open: boolean) => void;
}

/** The dialog's title and the last step's action, idle and while the opener works on it. */
export interface ModelDeployLabels {
  pending: string;
  submit: string;
  title: string;
}

const deployLabels: ModelDeployLabels = {
  pending: "Deploying…",
  submit: "Deploy",
  title: "Deploy model",
};
