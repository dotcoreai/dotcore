import { useRef, useState } from "react";

import { validateDeploymentName } from "@/domain/deployment-name";
import { executionTargetPresentation, placedExecutionTargetName } from "@/domain/execution-target";
import { useDeployWizard } from "@/features/deployments/use-deploy-wizard";
import {
  deploymentExposure,
  type DeploymentExposureValues,
  deploymentExposureValues,
  routeBlockingMessage,
  validateDeploymentExposure,
} from "@/features/models/deployment-exposure";
import { defaultMemoryBasis } from "@/features/models/profiles/profile-compatibility";
import { defaultCatalogChips } from "@/features/models/profiles/profile-table-seed";
import { ParameterVisibility } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import { ExecutionTargetWorkloadKind } from "@/gen/dotcore/site/v1/service_pb";
import { useDirtyFields } from "@/hooks/use-dirty-fields";
import { type FieldErrors, toFieldErrors, withFieldError } from "@/lib/form-validation";
import { formatCount } from "@/lib/format";

import type { ModelDeployFormOptions } from "./model-deploy-form-options";
import {
  launchRejectionField,
  type LocalDeploymentValues,
  MODEL_DEPLOY_ADVANCED_STEPS,
  MODEL_DEPLOY_STEPS,
  type ModelDeployField,
  type ModelDeployStep,
  scenarioContext,
} from "./model-deploy-launch";
import { useModelDeployContext } from "./use-model-deploy-context";
import { useCustomArguments, useLiveCommandPreview } from "./use-model-deploy-engine";
import { useModelDeployMachine } from "./use-model-deploy-machine";
import { useModelDeployMemoryPreview } from "./use-model-deploy-memory-preview";
import { useModelDeployOptions } from "./use-model-deploy-options";
import { useModelDeployParameters } from "./use-model-deploy-parameters";

/**
 * The deploy dialog's form: the launch the operator builds across the steps, its validation and
 * the submit. The machine and profile, the context, the engine parameters, the model options and
 * the engine's custom arguments and command each come from their own hook; this one ties them to
 * the steps.
 */
export function useModelDeployForm(options: ModelDeployFormOptions) {
  const {
    applications,
    catalogChips = defaultCatalogChips,
    deployments,
    existingRouteNames = [],
    initialExposure,
    initialName,
    memoryBasis = defaultMemoryBasis,
    model,
    onEstimate,
    onPreview,
    onPreviewMemory,
    onSubmit,
    open,
    pending,
    routeConflict,
    scenario: startScenario,
  } = options;
  const dirtyFields = useDirtyFields<ModelDeployField>(open);
  const [exposureValues, setExposureValues] = useState<DeploymentExposureValues>(() =>
    initialExposure
      ? deploymentExposureValues(initialExposure, initialName ?? model.displayName)
      : { exposure: "direct" },
  );
  // The launch's context, starting from the opener's; changing it leaves the opener alone.
  const [context, setContext] = useState(() => scenarioContext(startScenario));
  const [name, setName] = useState(initialName ?? model.displayName);
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>(new Map());
  const [targetValidationPending, setTargetValidationPending] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);
  const routeNameRef = useRef<HTMLInputElement>(null);
  const profileRef = useRef<HTMLInputElement>(null);
  const executionTargetRef = useRef<HTMLInputElement>(null);
  const setFieldError = (field: string, nextError: string | undefined) =>
    setFieldErrors((errors) => withFieldError(errors, field, nextError));

  const machine = useModelDeployMachine(
    { ...options, catalogChips, memoryBasis },
    { dirtyFields, scenario: context.scenario, setFieldErrors },
  );
  const { configuration, placement, profile, profileId, selection } = machine;
  const parameters = useModelDeployParameters({
    context,
    contextStart: machine.start,
    derivedParameterIds: machine.derivedParameterIds,
    dirtyFields,
    engineRelease: machine.engineRelease,
    exposedParameters: machine.exposedParameters,
    fieldErrors,
    profile,
    scenarioValues: machine.scenarioValues,
    selection,
    setFieldError,
    setFieldErrors,
  });
  const { engineParameterInputs, values } = parameters;
  const modelOptions = useModelDeployOptions({
    configuration,
    dirtyFields,
    fieldErrors,
    model,
    profile,
    selection,
    setFieldError,
  });
  const { optionOverrides } = modelOptions;
  const customArguments = useCustomArguments(selection.values.customArgumentTokens);
  const customArgumentTokens = customArguments.tokens;
  const memory = useModelDeployMemoryPreview({
    applications,
    compatibilityRead: machine.tableRead.compatibility,
    customArgumentTokens,
    declaredValues: parameters.declaredValues,
    deployments,
    derivedParameterIds: machine.derivedParameterIds,
    engineParameterInputs,
    engineRelease: machine.engineRelease,
    exposedParameters: machine.exposedParameters,
    launchValuesPending: machine.launchValuesPending,
    model,
    name,
    onEstimate,
    onPreviewMemory,
    optionOverrides,
    placement,
    profile,
    profileId,
    selectedExecutionTarget: machine.selectedExecutionTarget,
    targetsLoading: machine.targetsLoading,
    targetsLoadingPresentation: machine.targetsLoadingPresentation,
    values,
  });
  const contextStep = useModelDeployContext({
    context,
    contextParameters: parameters.contextParameters,
    derivedParameterIds: machine.derivedParameterIds,
    dirtyFields,
    engineParameterInputs,
    engineParameterInputsRef: parameters.engineParameterInputsRef,
    estimate: memory.estimate,
    fieldErrors,
    placement,
    profileId,
    selectedExecutionTarget: machine.selectedExecutionTarget,
    setContext,
    setFieldError,
    setFieldErrors,
    targetVerdict: machine.targetVerdict,
  });

  const following = exposureValues.routeName === undefined;
  const routeBlocking = routeBlockingMessage({
    conflict: routeConflict,
    deploymentName: name,
    existingRouteNames,
    values: exposureValues,
  });
  const validateRoute = (values = exposureValues, deploymentName = name): string | undefined =>
    validateDeploymentExposure(values, deploymentName, existingRouteNames);
  // The launch the preview reads and the deploy submits; undefined while a value is invalid.
  const launch = (target: ExactPlacement | null): LocalDeploymentValues | undefined =>
    profile && values && optionOverrides && target
      ? {
          configuration: {
            customArgumentTokens,
            engineParameterValues: values,
            // Left unset, so resolution takes the profile's weights.
            modelArtifactBindingId: "",
            optionOverrides,
          },
          exposure: deploymentExposure(exposureValues, name),
          name: name.trim(),
          profileId,
          placement: target,
        }
      : undefined;

  const stepErrors = (activeStep: ModelDeployStep): FieldErrors => {
    switch (activeStep) {
      case "Model & machine":
        return toFieldErrors([
          ["profileId", machine.profileError],
          [
            "placement",
            !machine.profileError && !placement && machine.deployableExecutionTargets.length
              ? "Execution target is required."
              : undefined,
          ],
        ]);
      case "Capabilities":
        return toFieldErrors(modelOptions.errorsAt(ParameterVisibility.BASIC));
      case "Context":
        return toFieldErrors([
          ...parameters.parameterErrors(parameters.contextParameters),
          ...parameters.parameterErrors(parameters.basicParameters),
          // Held while the next estimate loads: leaving a field restarts it, so Next would pass.
          [launchRejectionField, contextStep.launchRejectionMessage],
        ]);
      case "Performance & memory":
        return toFieldErrors([
          ...parameters.parameterErrors(parameters.advancedParameters),
          ...modelOptions.errorsAt(ParameterVisibility.ADVANCED),
        ]);
      // A rejected command blocks the deploy, not the way through the steps.
      case "Engine":
        return new Map();
      case "Review & deploy": {
        const nameError = validateDeploymentName(name);
        // A route following an invalid name is explained by the name's own error.
        const routeError = following && nameError ? undefined : validateRoute();
        return toFieldErrors([
          ["name", nameError],
          ["routeName", routeError],
        ]);
      }
    }
  };
  const fieldControls = () =>
    new Map<string, HTMLElement | null>([
      ["profileId", profileRef.current],
      ["placement", executionTargetRef.current],
      ...parameters.fieldControls(),
      ...modelOptions.fieldControls(),
      [
        launchRejectionField,
        contextStep.launchRejectionParameter
          ? (parameters.engineParameterRefs.current.get(
              contextStep.launchRejectionParameter.parameterId,
            ) ?? null)
          : null,
      ],
      ["name", nameRef.current],
      ["routeName", routeNameRef.current],
    ]);
  const wizard = useDeployWizard<ModelDeployStep>({
    advancedSteps: MODEL_DEPLOY_ADVANCED_STEPS,
    errorsFor: stepErrors,
    fieldControls,
    hasFields: (step) => {
      switch (step) {
        case "Capabilities":
          return modelOptions.basicControls.length > 0;
        case "Performance & memory":
          return (
            parameters.advancedParameters.length > 0 || modelOptions.advancedControls.length > 0
          );
        case "Engine":
          return Boolean(configuration);
        default:
          return true;
      }
    },
    // Advanced inputs never reach a basic launch, so leaving advanced mode returns them to the
    // profile's values.
    onAdvancedOff: () => {
      parameters.resetAdvanced();
      modelOptions.resetAdvanced();
      customArguments.reset();
    },
    // The target placed for the operator stays theirs, so a refresh can't take it away unasked.
    onLeaveFirstStep: () => {
      if (machine.pickedPlacement === undefined && placement) machine.setPickedPlacement(placement);
    },
    onSubmit: () => void submit(),
    open,
    order: MODEL_DEPLOY_STEPS,
    setFieldErrors,
  });
  // The first step before the Engine step with errors, which holds back the command.
  const commandBlockedBy = MODEL_DEPLOY_STEPS.slice(0, MODEL_DEPLOY_STEPS.indexOf("Engine")).find(
    (step) => wizard.steps.includes(step) && stepErrors(step).size > 0,
  );
  const commandPreview = useLiveCommandPreview({
    active: open && wizard.step === "Engine",
    blocked: commandBlockedBy !== undefined,
    onPreview,
    request: launch(placement),
  });

  // The Engine step only exists in advanced mode, so going there turns Advanced on.
  const revealEngine = () => {
    if (!wizard.advanced) wizard.toggleAdvanced(true);
    wizard.goTo("Engine");
  };

  const submit = async () => {
    machine.setIneligibleTarget(undefined);
    if (wizard.revealFirstError()) return;
    // The latest command preview for these values was rejected, which the Engine step shows.
    if (commandPreview.rejection) {
      revealEngine();
      return;
    }
    setFieldErrors(new Map());
    if (pending || !placement || !launch(placement)) return;

    setTargetValidationPending(true);
    const selectedPlacement = placement;
    const freshTargets = await machine.refreshExecutionTargets();
    setTargetValidationPending(false);
    const selectedTarget = freshTargets?.find(
      (target) =>
        target.workloadKind === ExecutionTargetWorkloadKind.MODEL &&
        target.executionTargetId === selectedPlacement.executionTargetId &&
        target.siteNodeId === selectedPlacement.siteNodeId,
    );
    if (!selectedTarget || !executionTargetPresentation(selectedTarget).deployable) {
      machine.setIneligibleTarget(
        placedExecutionTargetName(selectedPlacement, machine.executionTargets),
      );
      // A chosen target is chosen again; an automatic one gives way to the profile's remaining
      // targets. Either way on the Model & machine step.
      if (machine.pickedPlacement) {
        machine.setPickedPlacement(null);
        setFieldError("placement", "Select an eligible execution target.");
      } else machine.setPickedPlacement(undefined);
      wizard.goTo("Model & machine");
      return;
    }

    const submitted = launch(selectedPlacement);
    if (submitted) onSubmit(submitted);
  };

  const deploymentPrerequisiteUnavailable =
    machine.targetsLoading ||
    Boolean(machine.effectiveTargetsError) ||
    machine.deployableExecutionTargets.length === 0;

  return {
    commandBlockedBy,
    commandPreview,
    context: contextStep,
    customArguments,
    deployDisabled:
      targetValidationPending ||
      deploymentPrerequisiteUnavailable ||
      machine.launchValuesPending ||
      Boolean(routeBlocking),
    dirtyFields,
    executionTargetRef,
    exposureValues,
    fieldErrors,
    following,
    machine,
    memory,
    modelOptions,
    name,
    nameRef,
    parameters,
    profileRef,
    // Settings a profile change resets.
    revealEngine,
    resetsRuntime: parameters.customizations.length > 0 || modelOptions.changed,
    routeBlocking,
    routeNameRef,
    setExposureValues,
    setFieldError,
    setName,
    // What the summary lists beside the context and the changed parameters: the advanced options
    // that differ from the profile's, and the custom arguments.
    summaryCustomizations: [
      ...modelOptions.customizations,
      ...(customArgumentTokens.length
        ? [formatCount(customArgumentTokens.length, "custom argument")]
        : []),
    ],
    validateRoute,
    wizard,
  };
}
