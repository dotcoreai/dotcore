import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { validateDeploymentName } from "@/domain/deployment-name";
import {
  executionTargetName,
  executionTargetPresentation,
  placedExecutionTargetName,
} from "@/domain/execution-target";
import { useAdvancedMode } from "@/features/deployments/use-advanced-mode";
import {
  type ConfiguredExecutionTargetLoader,
  useConfiguredExecutionTargets,
} from "@/features/deployments/use-configured-execution-targets";
import { useWizardSteps } from "@/features/deployments/use-wizard-steps";
import {
  deploymentExposure,
  type DeploymentExposureValues,
  deploymentExposureValues,
  routeBlockingMessage,
  type RouteNameConflict,
  validateDeploymentExposure,
} from "@/features/models/deployment-exposure";
import {
  configurationSpeculativeOptions,
  profileEstimateKey,
  projectProfileRows,
} from "@/features/models/profiles/model-row";
import {
  defaultMemoryBasis,
  type MemoryBasis,
} from "@/features/models/profiles/profile-compatibility";
import { compareProfiles } from "@/features/models/profiles/profile-fields";
import {
  type CatalogChips,
  defaultCatalogChips,
  profileTableStart,
  startShows,
} from "@/features/models/profiles/profile-table-seed";
import {
  followContextInputs,
  sameSizingScenario,
  scenarioFromInputs,
  type SizingScenario,
  sizingScenarioKey,
} from "@/features/models/profiles/sizing-scenario";
import {
  type ProfileCompatibilityLoader,
  type ProfileEstimateState,
  useProfileCompatibility,
} from "@/features/models/profiles/use-profile-compatibility";
import {
  CONTEXT_PARAMETER_IDS,
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  effectiveEngineParameters,
  engineParameterError,
  engineParameterValues,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import {
  configurationKvCachePrecision,
  currentKvCachePrecision,
  kvCachePrecisionCustomizations,
  kvCachePrecisionOverrides,
} from "@/features/models/runtime/kv-cache-precision";
import {
  type LocalDeploymentConfigurationValues,
  parallelRequestsCustomization,
  profileEngineRelease,
  runtimeCustomizations,
} from "@/features/models/runtime/local-deployment-configuration";
import {
  currentSpeculativeChoice,
  type SpeculativeChoice,
  speculativeChoiceError,
  speculativeCustomizations,
  speculativeOptionOverrides,
} from "@/features/models/runtime/speculative-choice";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import {
  type EngineParameterDefinition,
  type EngineParameterValues,
  type EngineRelease,
} from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { HardwareClass } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import {
  type ModelDeployment,
  type ResolvedLocalModelDeploymentSnapshot,
} from "@/gen/dotcore/model/v1/deployment_pb";
import type { ModelDeploymentExposure } from "@/gen/dotcore/model/v1/service_pb";
import { ExecutionTargetWorkloadKind } from "@/gen/dotcore/site/v1/service_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { useDirtyFields } from "@/hooks/use-dirty-fields";
import { type FieldErrors, focusFirstInvalidField, toFieldErrors } from "@/lib/form-validation";
import { formatCompactCount, formatCount } from "@/lib/format";

import {
  configurationStart,
  fallbackStep,
  launchRejectionField,
  type LocalDeploymentValues,
  MODEL_DEPLOY_STEPS,
  type ModelDeployField,
  type ModelDeployStep,
  modelDeploySteps,
  operatorSentence,
  scenarioContext,
  scenarioInput,
  settledContext,
  startContext,
  useLaunchEngineValues,
  withoutChosenContextErrors,
  withoutUnsetContextPerRequest,
} from "./model-deploy-launch";
import { useCommandPreview } from "./use-command-preview";
import {
  type LocalDeploymentMemoryPreview,
  type LocalMemoryPreviewValues,
  useModelDeployMemoryPreview,
} from "./use-model-deploy-memory-preview";
import { useModelDeployProfileTable } from "./use-model-deploy-profile-table";
import { useModelDeployTarget } from "./use-model-deploy-target";
import type { ResourceEstimateLoader } from "./use-resource-estimate";

/**
 * The deploy dialog's form: the launch the operator builds across the steps, its validation, the
 * command preview and the submit. Profile, target and memory facts come from their own hooks; the
 * launch's context is shared by the Context step and the profile table's "Sized for".
 */
export function useModelDeployForm({
  applications,
  engineReleases,
  hardwareClasses,
  existingRouteNames = [],
  catalogChips = defaultCatalogChips,
  deployments,
  initialConfiguration,
  initialExposure,
  initialName,
  initialPlacement,
  initialProfileId,
  listConfiguredExecutionTargets,
  listProfileCompatibility,
  memoryBasis: startBasis = defaultMemoryBasis,
  model,
  onEstimate,
  onPreview,
  onPreviewMemory,
  onSubmit,
  open,
  pending,
  profileEstimates,
  routeConflict,
  scenario: startScenario,
}: ModelDeployFormOptions) {
  const advancedMode = useAdvancedMode(open);
  const { advanced, announcement } = advancedMode;
  const dirtyFields = useDirtyFields<ModelDeployField>(open);
  const [exposureValues, setExposureValues] = useState<DeploymentExposureValues>(() =>
    initialExposure
      ? deploymentExposureValues(initialExposure, initialName ?? model.displayName)
      : { exposure: "direct" },
  );
  // The launch's context, starting from the opener's; changing it leaves the opener alone.
  const [context, setContext] = useState(() => startContext(startScenario, initialConfiguration));
  // The chosen context's "Sized for", which the dialog's read sizes; absent at the default context.
  const scenario = context.scenario;
  // The memory basis profiles are judged on, starting from the opener's; changing it leaves the
  // opener alone.
  const [basis, setBasis] = useState(startBasis);
  const profileCompatibility = useProfileCompatibility(
    listProfileCompatibility,
    model.modelId,
    open,
    scenario,
  );
  // Under a scenario, the table shows the estimates of the dialog's own read, which are the ones
  // the launch is sized from; until that read lands, and without a scenario, the opener's, which
  // only answer the scenario the dialog started from.
  const openerEstimates = sameSizingScenario(scenario, startScenario)
    ? profileEstimates
    : undefined;
  const ownEstimates = profileCompatibility.compatibility
    ? profileCompatibility.estimates
    : undefined;
  const tableEstimates = scenario
    ? (ownEstimates ?? openerEstimates)
    : (openerEstimates ?? ownEstimates);
  // The read that sized the scenario, which prefills the launch.
  const sizing = scenario ? profileCompatibility.sizing : undefined;
  const profileEstimateValues = tableEstimates?.estimates;
  const profileRows = useMemo(
    () =>
      projectProfileRows(model, {
        engineReleases,
        hardwareClasses,
        estimates: profileEstimateValues ?? new Map(),
      }),
    [engineReleases, hardwareClasses, model, profileEstimateValues],
  );
  // Before compatibility is known, the requested profile or the first in the table's order that the
  // catalog's chips show, which stays when the read fails.
  const selectedInitialProfileId =
    initialProfileId ??
    [...profileRows]
      .sort(compareProfiles)
      .find((row) => startShows(profileTableStart(catalogChips, undefined), row))?.profileId ??
    "";
  const [profileId, setProfileId] = useState(selectedInitialProfileId);
  // The runtime fields start from the opener's configuration on the starting profile, else the
  // profile's defaults.
  const [start] = useState(() => ({
    profileId: selectedInitialProfileId,
    values: configurationStart(
      model,
      engineReleases,
      selectedInitialProfileId,
      initialConfiguration,
    ),
  }));
  const [bindingId, setBindingId] = useState(start.values.bindingId);
  // The operator's engine parameter edits over the inputs the profile or scenario starts from.
  const [editedInputs, setEditedInputs] = useState<Map<string, string>>(start.values.edits);
  const [storedSpeculativeChoice, setSpeculativeChoice] = useState<SpeculativeChoice>(
    start.values.speculativeChoice,
  );
  // The operator's KV cache precision pick; empty while the profile's default stands.
  const [pickedKvCachePrecision, setPickedKvCachePrecision] = useState(
    start.values.kvCachePrecision,
  );
  const [customArgumentTokens, setCustomArgumentTokens] = useState<string[]>(
    start.values.customArgumentTokens,
  );
  const commandPreview = useCommandPreview(onPreview);
  const { invalidatePreview } = commandPreview;
  // The operator's target choice, starting from the opener's: undefined until they choose, null
  // once they clear it.
  const [pickedPlacement, setPickedPlacement] = useState<ExactPlacement | null | undefined>(
    initialPlacement,
  );
  const [targetValidationPending, setTargetValidationPending] = useState(false);
  const [ineligibleTarget, setIneligibleTarget] = useState<string>();
  // Whether the Target step shows, fixed when leaving the Profile step so the steps ahead stay put.
  const [lockedTargetStep, setLockedTargetStep] = useState<boolean>();
  const preselectionKeyRef = useRef<string>(undefined);
  // Whether the last preselection found nothing to select, so a later read may still select one.
  const preselectionEmptyRef = useRef(false);
  // Whether the operator selected or cleared a profile, which later reads then leave alone.
  const operatorSelectedRef = useRef(false);
  // The profiles the table shows, in order, which preselection picks from.
  const shownProfileIdsRef = useRef<readonly string[]>([]);
  const updateShownProfiles = useCallback((profileIds: readonly string[]) => {
    shownProfileIdsRef.current = profileIds;
  }, []);
  const {
    current: executionTargetsCurrent,
    error: targetsError,
    loading: targetsLoading,
    refresh: refreshExecutionTargets,
    executionTargets: loadedExecutionTargets,
  } = useConfiguredExecutionTargets(listConfiguredExecutionTargets, open);
  const [name, setName] = useState(initialName ?? model.displayName);
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>(new Map());
  const pendingFocusRef = useRef<FieldErrors | undefined>(undefined);
  // The errors of the latest blocked step change or Deploy, which open collapsed Runtime sections.
  const [revealedErrors, setRevealedErrors] = useState<FieldErrors>(new Map());
  const artifactRef = useRef<HTMLInputElement>(null);
  const engineParameterRefs = useRef<Map<string, HTMLElement | null>>(new Map());
  const nameRef = useRef<HTMLInputElement>(null);
  const routeNameRef = useRef<HTMLInputElement>(null);
  const profileRef = useRef<HTMLInputElement>(null);
  const speculativeCountRef = useRef<HTMLInputElement>(null);
  const executionTargetRef = useRef<HTMLInputElement>(null);

  const profile = model.profiles.find((candidate) => candidate.profileId === profileId);
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  const binding = configuration?.artifactBindings.find(
    (candidate) => candidate.bindingId === bindingId,
  );
  const engineRelease = profileEngineRelease(model, profile, engineReleases);
  const exposedParameters = effectiveEngineParameters(engineRelease, profile);
  // The scenario's values for the profile, hidden ones included, replace the profile's defaults.
  // Resolution fills the context per request the scenario leaves unset; it stays empty here so it
  // isn't submitted as a supplied value and keeps following the total context.
  const profileReadKey = profile ? profileEstimateKey(model.modelId, profile.profileId) : undefined;
  // The parameters sizing derives come from the backend's read of the profile; until it lands, an
  // empty one reads as missing, and the deploy waits for the read anyway.
  const derivedParameterIds =
    (profileReadKey &&
      profileCompatibility.compatibility?.derivedParameterIds.get(profileReadKey)) ||
    noDerivedParameterIds;
  const ownReadValues = profileReadKey
    ? profileCompatibility.sizing?.engineValues.get(profileReadKey)
    : undefined;
  // With a scenario, a second read without one backs the launch values of a profile the scenario's
  // settled read has none for; it starts only then.
  const defaultsCompatibility = useProfileCompatibility(
    listProfileCompatibility,
    model.modelId,
    open && Boolean(scenario) && !profileCompatibility.estimates.pending && !ownReadValues,
  );
  // A scenario the backend can't size the profile for carries no values; the profile's own
  // defaults, resolved without a scenario, start the launch instead of the release defaults.
  const defaultReadValues =
    scenario && !ownReadValues && profileReadKey
      ? defaultsCompatibility.sizing?.engineValues.get(profileReadKey)
      : undefined;
  const readValues = ownReadValues
    ? withoutUnsetContextPerRequest(ownReadValues, scenario)
    : withoutUnsetContextPerRequest(defaultReadValues, undefined);
  // The values of a profile's first read under each scenario stay for the dialog's life, so later
  // reads never swap them under the operator; a new "Sized for" starts from its own read.
  const [frozenValues, setFrozenValues] = useState<ReadonlyMap<string, EngineParameterValues>>(
    new Map(),
  );
  const frozenKey = profile ? `${profile.profileId}\0${sizingScenarioKey(scenario)}` : undefined;
  if (frozenKey && readValues && !frozenValues.has(frozenKey))
    setFrozenValues(new Map(frozenValues).set(frozenKey, readValues));
  const scenarioValues = frozenKey ? (frozenValues.get(frozenKey) ?? readValues) : undefined;
  const speculativeOptions = configurationSpeculativeOptions(model, configuration, profile);
  const speculativeChoice = currentSpeculativeChoice(speculativeOptions, storedSpeculativeChoice);
  // Undefined while the speculative count is invalid, which holds back estimates like other values.
  const speculativeOverrides = speculativeOptionOverrides(speculativeOptions, speculativeChoice);
  const kvCachePrecisionOption = configurationKvCachePrecision(model, configuration, profile);
  const kvCachePrecision = currentKvCachePrecision(kvCachePrecisionOption, pickedKvCachePrecision);
  const optionOverrides = speculativeOverrides && [
    ...speculativeOverrides,
    ...kvCachePrecisionOverrides(kvCachePrecisionOption, kvCachePrecision),
  ];
  const executionTargets = useMemo(
    () =>
      loadedExecutionTargets.filter(
        (target) => target.workloadKind === ExecutionTargetWorkloadKind.MODEL,
      ),
    [loadedExecutionTargets],
  );
  const deployableExecutionTargets = executionTargets.filter(
    (target) => executionTargetPresentation(target).deployable,
  );
  const targetsLoadingVisible = useDelayedLoading(targetsLoading);
  const effectiveTargetsError = !listConfiguredExecutionTargets
    ? "Execution targets are unavailable because the site service does not support configured-target listing."
    : targetsError;
  const targetsLoadingPresentation =
    (targetsLoading || targetsLoadingVisible) && !effectiveTargetsError;
  const compatibilityLoadingVisible = useDelayedLoading(profileCompatibility.loading);
  // Compatibility applies once the read has evaluated a target. Until then, and when it fails,
  // every profile is listed and the target is chosen as without it.
  const compatibility = profileCompatibility.compatibility?.targets.length
    ? profileCompatibility.compatibility
    : undefined;
  // A "Sized for" becomes the chosen context and removing it returns the context to the default;
  // either way the operator's parallel requests stay.
  const chooseSizedFor = useCallback((next: SizingScenario | undefined) => {
    setContext((previous) =>
      next
        ? { ...previous, ...scenarioContext(next) }
        : { parallelRequests: previous.parallelRequests },
    );
    setFieldErrors(withoutChosenContextErrors);
  }, []);
  const profileTable = useModelDeployProfileTable({
    basis,
    catalogChips,
    compatibility,
    compatibilityRead: profileCompatibility.compatibility,
    engineReleases,
    executionTargets,
    initialProfileId,
    model,
    onBasisChange: setBasis,
    onSizedForChange: chooseSizedFor,
    profileRows,
    scenario,
    sizingFailed: profileCompatibility.sizingFailed,
    sizingRead: profileCompatibility.sizing,
    startBasis,
    startScenario,
  });
  const { chipKey, profileAvailability, scopes, tableProfileRows } = profileTable;
  const {
    fitStart,
    forgetDefaultStart,
    launchesOn,
    pickedTarget,
    placement,
    profileLaunchCandidates,
    readFitStart,
    selectedExecutionTarget,
  } = useModelDeployTarget({
    basis,
    compatibility,
    executionTargets,
    model,
    pickedPlacement,
    profile,
    profileId,
    scenario,
    scopes,
    sizing,
    sizingRead: profileCompatibility.sizing,
  });

  // The selected profile's row shows the value the Context step starts from, frozen with it; the
  // other rows stay live, so their changes still show.
  const shownProfileRows =
    scenario || fitStart === undefined
      ? tableProfileRows
      : tableProfileRows.map((row) =>
          row.profileId === profileId ? { ...row, totalContext: fitStart } : row,
        );
  const contextHint =
    fitStart !== undefined && selectedExecutionTarget
      ? `Up to ${formatCompactCount(fitStart)} on ${executionTargetName(selectedExecutionTarget)}`
      : undefined;
  const { declaredValues, engineParameterInputs } = useLaunchEngineValues(
    scenarioValues,
    engineRelease,
    profile,
    editedInputs,
    context,
    fitStart,
  );
  // The largest fit offered as the total context's last chip. While the total follows it or the
  // operator's total equals it, the chip reads as chosen. Choosing it otherwise forgets the default's
  // frozen value, so the next read of the defaults freezes the live one and a value frozen from a
  // stale read is corrected; without a read to freeze, it sets the value like a size chip.
  const largestFit =
    fitStart === undefined
      ? undefined
      : {
          onSelect:
            readFitStart === undefined
              ? undefined
              : () => {
                  forgetDefaultStart();
                  resetContext();
                },
          value: fitStart,
        };

  const selectedProfileRow = profileRows.find((row) => row.profileId === profileId);
  const selectedAvailability = selectedProfileRow && profileAvailability?.(selectedProfileRow);
  const noProfileRuns = Boolean(
    profileAvailability && profileRows.every((row) => !profileAvailability(row)?.selectable),
  );
  const notRunningReasons = [
    ...new Set(profileRows.flatMap((row) => profileAvailability?.(row)?.status?.text ?? [])),
  ];
  // A selection that can't run in the Runs on scope, which a refresh can cause, blocks the step.
  const profileError = !profile
    ? noProfileRuns
      ? `No profile can run here.${notRunningReasons.length === 1 ? ` ${notRunningReasons[0]}.` : ""}`
      : "Select a model profile."
    : selectedAvailability?.selectable === false
      ? `${profile.displayName || profileId} can't run here.${selectedAvailability.status ? ` ${selectedAvailability.status.text}.` : ""}`
      : undefined;

  // Basic mode asks for the target only when several can deploy the profile.
  const liveTargetStep = profileLaunchCandidates
    ? profileLaunchCandidates.length > 1
    : deployableExecutionTargets.length > 1;
  const stepsFor = (advancedSteps: boolean) =>
    modelDeploySteps(advancedSteps || (lockedTargetStep ?? liveTargetStep), advancedSteps);
  const steps = stepsFor(advanced);
  const wizard = useWizardSteps(steps, fallbackStep);
  const step = wizard.current as ModelDeployStep;
  const targetStepShown = steps.includes("Target");
  const goToStep = (nextStep: ModelDeployStep) => {
    const leavingProfile = step === "Profile" && nextStep !== "Profile";
    setLockedTargetStep(
      nextStep === "Profile" ? undefined : leavingProfile ? liveTargetStep : lockedTargetStep,
    );
    // The target placed for the operator stays theirs, so a refresh can't take it away unasked.
    if (leavingProfile && pickedPlacement === undefined && placement) setPickedPlacement(placement);
    wizard.goTo(nextStep);
  };

  // Selects a profile with its runtime values, the profile's defaults unless given.
  const selectProfile = (
    nextProfileId: string,
    values = configurationStart(model, engineReleases, nextProfileId, undefined),
  ) => {
    setProfileId(nextProfileId);
    setIneligibleTarget(undefined);
    // A chosen target that can't launch the new profile gives way to the profile's own targets.
    if (pickedTarget && !launchesOn(nextProfileId, pickedTarget)) setPickedPlacement(undefined);
    setBindingId(values.bindingId);
    setEditedInputs(values.edits);
    setSpeculativeChoice(values.speculativeChoice);
    setPickedKvCachePrecision(values.kvCachePrecision);
    setFieldErrors((errors) => {
      const nextErrors = new Map(errors);
      nextErrors.delete("profileId");
      nextErrors.delete("artifact");
      nextErrors.delete("speculativeCount");
      for (const release of engineReleases)
        for (const parameter of release.parameterSchema?.parameters ?? [])
          nextErrors.delete(parameter.parameterId);
      return nextErrors;
    });
    invalidatePreview();
  };
  // The operator's own pick, which later reads then leave alone.
  const chooseProfile = (nextProfileId: string) => {
    operatorSelectedRef.current = true;
    dirtyFields.markDirty("profileId");
    selectProfile(nextProfileId);
  };
  const selectProfileRef = useRef(selectProfile);
  useEffect(() => {
    selectProfileRef.current = selectProfile;
  });

  // Preselects when compatibility first arrives and on later reads while that found nothing, unless
  // the operator has chosen, and whenever the Runs on chip changes, including its arrival with the
  // first read. A selection that can't launch there, or that the table hides, moves to the requested
  // profile when the table shows it and it can launch, otherwise to the first shown profile that can.
  useEffect(() => {
    if (!profileAvailability) return;
    const chipChanged =
      preselectionKeyRef.current !== chipKey &&
      (preselectionKeyRef.current !== undefined || chipKey !== "");
    const pending = preselectionKeyRef.current === undefined || preselectionEmptyRef.current;
    preselectionKeyRef.current = chipKey;
    if (!chipChanged && (!pending || operatorSelectedRef.current)) return;

    // The table reports its rows before this effect runs, so they match the chip.
    const shown = shownProfileIdsRef.current;
    const selectable = (candidate: string) => {
      const row = profileRows.find((profileRow) => profileRow.profileId === candidate);
      return Boolean(row && shown.includes(candidate) && profileAvailability(row)?.selectable);
    };
    if (selectable(profileId)) return;
    const next = [initialProfileId ?? "", ...shown].find(selectable) ?? "";
    preselectionEmptyRef.current = !next;
    // A read can briefly select nothing; returning to the starting profile restores its starting
    // values rather than the profile's defaults.
    const values = next === start.profileId ? start.values : undefined;
    if (next !== profileId) queueMicrotask(() => selectProfileRef.current(next, values));
  }, [chipKey, initialProfileId, profileAvailability, profileId, profileRows, start]);

  // The latest inputs, for a deferred blur check that runs after a change the blur preceded, such
  // as a total context preset clicked while the context per request had focus.
  const engineParameterInputsRef = useRef(engineParameterInputs);
  useEffect(() => {
    engineParameterInputsRef.current = engineParameterInputs;
  });
  const values = engineParameterValues(
    declaredValues,
    exposedParameters,
    engineParameterInputs,
    derivedParameterIds,
  );
  const setFieldError = (field: string, nextError: string | undefined) => {
    setFieldErrors((errors) => {
      const nextErrors = new Map(errors);
      if (nextError) nextErrors.set(field, nextError);
      else nextErrors.delete(field);
      return nextErrors;
    });
  };
  const contextParameters = exposedParameters.filter((parameter) =>
    CONTEXT_PARAMETER_IDS.has(parameter.parameterId),
  );
  // A context change edits the shared context: total context and context per request become the
  // chosen ones, starting from what the step shows. A settled change, from a slider or preset, also
  // moves the other of the two to keep the edited one, and sizes "Sized for" when valid; typing
  // waits until the input is left.
  const changeContext = (
    parameter: EngineParameterDefinition,
    nextInput: string,
    settled?: boolean,
  ) => {
    const edited = scenarioInput(parameter.parameterId);
    if (!edited) return;

    dirtyFields.markDirty(parameter.parameterId);
    const shown = {
      perRequest: engineParameterInputs.get(CONTEXT_PER_REQUEST_PARAMETER_ID) ?? "",
      total: engineParameterInputs.get(TOTAL_CONTEXT_PARAMETER_ID) ?? "",
    };
    const changed = { ...shown, [edited]: nextInput };
    const chosen = settled ? followContextInputs(changed, edited) : changed;
    setContext((previous) => ({
      ...previous,
      inputs: chosen,
      scenario: (settled && scenarioFromInputs(chosen)) || previous.scenario,
    }));
    // A shown error re-validates, including one the changed value causes or clears on another
    // field, such as a context per request above a new total context.
    const nextInputs = new Map(engineParameterInputs)
      .set(TOTAL_CONTEXT_PARAMETER_ID, chosen.total)
      .set(CONTEXT_PER_REQUEST_PARAMETER_ID, chosen.perRequest);
    for (const shownParameter of contextParameters)
      if (fieldErrors.has(shownParameter.parameterId))
        setFieldError(
          shownParameter.parameterId,
          engineParameterError(shownParameter, nextInputs, derivedParameterIds),
        );
    invalidatePreview();
  };
  // Returns the total context and context per request to the default; the operator's parallel
  // requests stay.
  const resetContext = () => {
    setContext((previous) => ({ parallelRequests: previous.parallelRequests }));
    setFieldErrors(withoutChosenContextErrors);
    invalidatePreview();
  };
  // A typed context settles once left, as a settled change does.
  const blurContext = (parameter: EngineParameterDefinition) => {
    if (!dirtyFields.isDirty(parameter.parameterId)) return;
    const edited = scenarioInput(parameter.parameterId);
    if (edited) setContext((previous) => settledContext(previous, edited));
    // Runs after a change the blur preceded, such as a total context preset clicked while the
    // context per request had focus, and re-validates any context field the settling moved.
    setTimeout(() =>
      setFieldErrors((errors) => {
        const nextErrors = new Map(errors);
        for (const shownParameter of contextParameters) {
          if (shownParameter !== parameter && !errors.has(shownParameter.parameterId)) continue;
          const error = engineParameterError(
            shownParameter,
            engineParameterInputsRef.current,
            derivedParameterIds,
          );
          if (error) nextErrors.set(shownParameter.parameterId, error);
          else nextErrors.delete(shownParameter.parameterId);
        }
        return nextErrors;
      }),
    );
  };
  const validateRoute = (values = exposureValues, deploymentName = name): string | undefined =>
    validateDeploymentExposure(values, deploymentName, existingRouteNames);
  const parameterErrors = (
    parameters: readonly EngineParameterDefinition[],
  ): [string, string | undefined][] =>
    parameters.map((parameter) => [
      parameter.parameterId,
      engineParameterError(parameter, engineParameterInputs, derivedParameterIds),
    ]);
  // A rejection spans the context values, so it shows as soon as it's known, on the context per
  // request once the operator set one, else on the first context field. A profile that sets its
  // context itself has no field to fix.
  const launchRejectionParameter =
    contextParameters.find(
      ({ parameterId }) =>
        parameterId === CONTEXT_PER_REQUEST_PARAMETER_ID && engineParameterInputs.get(parameterId),
    ) ?? contextParameters[0];
  const stepErrors = (activeStep: ModelDeployStep): FieldErrors => {
    switch (activeStep) {
      case "Target":
        return toFieldErrors([
          ["placement", placement ? undefined : "Execution target is required."],
        ]);
      case "Profile":
        return toFieldErrors([["profileId", profileError]]);
      case "Context":
        return toFieldErrors([
          ...parameterErrors(contextParameters),
          // Held while the next estimate loads: leaving a field restarts it, so Next would pass.
          [launchRejectionField, launchRejectionMessage],
        ]);
      case "Runtime":
        return toFieldErrors([
          ["artifact", binding ? undefined : "Select a model artifact."],
          ...parameterErrors(
            exposedParameters.filter(
              (parameter) => !CONTEXT_PARAMETER_IDS.has(parameter.parameterId),
            ),
          ),
          ["speculativeCount", speculativeChoiceError(speculativeOptions, speculativeChoice)],
        ]);
      case "Deploy": {
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
      ["artifact", artifactRef.current],
      ["placement", executionTargetRef.current],
      ...exposedParameters.map((parameter): [string, HTMLElement | null] => [
        parameter.parameterId,
        engineParameterRefs.current.get(parameter.parameterId) ?? null,
      ]),
      ["speculativeCount", speculativeCountRef.current],
      [
        launchRejectionField,
        launchRejectionParameter
          ? (engineParameterRefs.current.get(launchRejectionParameter.parameterId) ?? null)
          : null,
      ],
      ["name", nameRef.current],
      ["routeName", routeNameRef.current],
    ]);
  // Focus waits for the render that shows the errors, which also opens a collapsed Runtime section
  // holding an invalid field.
  const exposeErrors = (errors: FieldErrors): boolean => {
    setFieldErrors(errors);
    pendingFocusRef.current = errors.size ? errors : undefined;
    if (!errors.size) return true;
    setRevealedErrors(errors);
    return false;
  };
  // Opens the step that owns the errors, turning Advanced on when that step is hidden, and focuses
  // the first invalid field once the step renders.
  const revealErrors = (owner: ModelDeployStep, errors: FieldErrors) => {
    if (!steps.includes(owner)) {
      advancedMode.enable(`Advanced on to fix ${owner.toLocaleLowerCase()} settings`);
    }
    exposeErrors(errors);
    if (owner !== step) goToStep(owner);
  };

  // Runs after the shell focuses a new step heading, so the invalid field keeps focus. The request
  // stays until a render shows one of its fields: an earlier render may still hold the step or
  // section the field isn't in yet.
  useEffect(() => {
    const errors = pendingFocusRef.current;
    if (errors && focusFirstInvalidField(errors, fieldControls())) {
      pendingFocusRef.current = undefined;
    }
  });

  // The launch values arrive with the read, so the first estimate and the deploy wait for them.
  const launchValuesPending =
    profileCompatibility.estimates.pending ||
    (Boolean(scenario) && !ownReadValues && defaultsCompatibility.estimates.pending);
  const launchValuesLoadingVisible = useDelayedLoading(launchValuesPending);
  const memory = useModelDeployMemoryPreview({
    applications,
    binding,
    bindingId,
    compatibilityRead: profileCompatibility.compatibility,
    configuration,
    customArgumentTokens,
    declaredValues,
    deployments,
    derivedParameterIds,
    engineParameterInputs,
    engineRelease,
    engineReleases,
    exposedParameters,
    launchValuesPending,
    model,
    name,
    onEstimate,
    onPreviewMemory,
    optionOverrides,
    pickedKvCachePrecision,
    placement,
    profile,
    profileId,
    selectedExecutionTarget,
    storedSpeculativeChoice,
    targetsLoading,
    targetsLoadingPresentation,
    values,
  });
  const launchRejectionMessage =
    memory.estimate.launchRejection && launchRejectionParameter
      ? operatorSentence(memory.estimate.launchRejection)
      : undefined;
  const contextFieldErrors = launchRejectionMessage
    ? new Map(fieldErrors).set(launchRejectionParameter.parameterId, launchRejectionMessage)
    : fieldErrors;

  const following = exposureValues.routeName === undefined;
  const routeBlocking = routeBlockingMessage({
    conflict: routeConflict,
    deploymentName: name,
    existingRouteNames,
    values: exposureValues,
  });
  const customizations = [
    ...runtimeCustomizations(model, profile, engineRelease, {
      bindingId,
      declaredValues,
      inputs: engineParameterInputs,
    }),
    ...speculativeCustomizations(speculativeOptions, speculativeChoice),
    ...kvCachePrecisionCustomizations(kvCachePrecisionOption, kvCachePrecision),
  ];
  // Runtime settings that differ from the defaults, as the Deploy summary lists them, including
  // the launch's own parallel requests and custom arguments that a profile change keeps.
  const deployCustomizations = [
    ...customizations,
    ...parallelRequestsCustomization(profile, engineRelease, {
      bindingId,
      declaredValues,
      inputs: engineParameterInputs,
    }),
    ...(customArgumentTokens.length
      ? [formatCount(customArgumentTokens.length, "custom argument")]
      : []),
  ];

  const runPreview = async () => {
    for (const owner of ["Profile", "Context", "Runtime"] as const) {
      const errors = stepErrors(owner);
      if (errors.size) {
        revealErrors(owner, errors);
        return;
      }
    }
    setFieldErrors(new Map());
    if (!profile || !binding || !values || !optionOverrides || !placement) return;
    await commandPreview.requestPreview({
      configuration: {
        customArgumentTokens,
        engineParameterValues: values,
        modelArtifactBindingId: binding.bindingId,
        optionOverrides,
      },
      exposure: deploymentExposure(exposureValues, name),
      name: name.trim(),
      profileId,
      placement,
    });
  };
  const submit = async () => {
    setIneligibleTarget(undefined);
    for (const owner of MODEL_DEPLOY_STEPS) {
      const errors = stepErrors(owner);
      if (errors.size) {
        revealErrors(owner, errors);
        return;
      }
    }
    setFieldErrors(new Map());
    if (pending || !placement || !values || !binding || !optionOverrides) return;

    setTargetValidationPending(true);
    const selectedPlacement = placement;
    const freshTargets = await refreshExecutionTargets();
    setTargetValidationPending(false);
    const selectedTarget = freshTargets?.find(
      (target) =>
        target.workloadKind === ExecutionTargetWorkloadKind.MODEL &&
        target.executionTargetId === selectedPlacement.executionTargetId &&
        target.siteNodeId === selectedPlacement.siteNodeId,
    );
    if (!selectedTarget || !executionTargetPresentation(selectedTarget).deployable) {
      setIneligibleTarget(placedExecutionTargetName(selectedPlacement, executionTargets));
      // A chosen target is chosen again on the Target step; an automatic one gives way to the
      // profile's remaining targets, decided on the Profile step.
      if (pickedPlacement && targetStepShown) {
        setPickedPlacement(null);
        setFieldError("placement", "Select an eligible execution target.");
        goToStep("Target");
      } else {
        setPickedPlacement(undefined);
        goToStep("Profile");
      }
      return;
    }

    onSubmit({
      configuration: {
        customArgumentTokens,
        engineParameterValues: values,
        modelArtifactBindingId: binding.bindingId,
        optionOverrides,
      },
      exposure: deploymentExposure(exposureValues, name),
      name: name.trim(),
      profileId,
      placement: selectedPlacement,
    });
  };
  const updateCustomArguments = (update: (tokens: string[]) => string[]) => {
    setCustomArgumentTokens(update);
    invalidatePreview();
  };
  const toggleAdvanced = (nextAdvanced: boolean) =>
    advancedMode.toggle(nextAdvanced, stepsFor(nextAdvanced).length);
  const handleStepChange = (nextStep: string) => {
    const forward = steps.indexOf(nextStep as ModelDeployStep) > steps.indexOf(step);
    if (forward && !exposeErrors(stepErrors(step))) return;
    goToStep(nextStep as ModelDeployStep);
  };
  const handlePrimary = () => {
    if (step === "Deploy") {
      void submit();
      return;
    }
    if (exposeErrors(stepErrors(step))) goToStep(steps[steps.indexOf(step) + 1]);
  };

  const deploymentPrerequisiteUnavailable =
    targetsLoading || Boolean(effectiveTargetsError) || deployableExecutionTargets.length === 0;
  const deployDisabled =
    targetValidationPending ||
    deploymentPrerequisiteUnavailable ||
    launchValuesPending ||
    Boolean(routeBlocking);

  return {
    advanced,
    announcement,
    artifactRef,
    binding,
    bindingId,
    blurContext,
    changeContext,
    chooseProfile,
    commandPreview,
    compatibility,
    compatibilityLoadingVisible,
    configuration,
    contextFieldErrors,
    contextHint,
    contextParameters,
    customArgumentTokens,
    customizations,
    deployCustomizations,
    deployDisabled,
    deployableExecutionTargets,
    deploymentPrerequisiteUnavailable,
    dirtyFields,
    effectiveTargetsError,
    engineParameterInputs,
    derivedParameterIds,
    engineParameterInputsRef,
    engineParameterRefs,
    executionTargetRef,
    executionTargets,
    executionTargetsCurrent,
    exposedParameters,
    exposureValues,
    fieldErrors,
    following,
    handlePrimary,
    handleStepChange,
    ineligibleTarget,
    kvCachePrecision,
    kvCachePrecisionOption,
    largestFit,
    launchValuesLoadingVisible,
    memory,
    name,
    nameRef,
    placement,
    profile,
    profileCompatibility,
    profileError,
    profileId,
    profileLaunchCandidates,
    profileRef,
    profileTable,
    revealedErrors,
    routeBlocking,
    routeNameRef,
    runPreview,
    scenario,
    selectedExecutionTarget,
    selectedProfileRow,
    setBindingId,
    setContext,
    setEditedInputs,
    setExposureValues,
    setFieldError,
    setIneligibleTarget,
    setName,
    setPickedKvCachePrecision,
    setPickedPlacement,
    setSpeculativeChoice,
    shownProfileRows,
    sizing,
    speculativeChoice,
    speculativeCountRef,
    speculativeOptions,
    step,
    steps,
    tableEstimates,
    targetStepShown,
    targetsLoading,
    targetsLoadingPresentation,
    targetsLoadingVisible,
    toggleAdvanced,
    updateCustomArguments,
    updateShownProfiles,
    validateRoute,
    visitedSteps: wizard.visited,
  };
}

/** What the deploy form needs from its opener: the model, the site's facts and the API calls. */
export interface ModelDeployFormOptions {
  engineReleases: readonly EngineRelease[];
  hardwareClasses: readonly HardwareClass[];
  existingRouteNames?: readonly string[];
  /**
   * The catalog's chips the profile table starts from, taken when the dialog opened; the catalog's
   * default chips without them.
   */
  catalogChips?: CatalogChips;
  /** The site's application deployments, which name their memory in the memory strip. */
  applications?: readonly ApplicationDeployment[];
  /**
   * The site's model deployments, which name what occupies the selected target's memory in the
   * memory strip. Without them, the strip doesn't show.
   */
  deployments?: readonly ModelDeployment[];
  /**
   * The runtime configuration the launch starts from, such as one the dialog submitted before: its
   * artifact, engine parameters, options and custom arguments, until a profile change resets the
   * profile's own. The context comes from `scenario`. The profile's defaults without it.
   */
  initialConfiguration?: LocalDeploymentConfigurationValues;
  /** The exposure the launch starts with, such as one the dialog submitted before; direct without it. */
  initialExposure?: ModelDeploymentExposure;
  /** The deployment name the launch starts with, such as one the operator already chose. */
  initialName?: string;
  /**
   * The target the launch starts on, such as a proposal's, kept while it can run the selected
   * profile; the dialog picks one without it.
   */
  initialPlacement?: ExactPlacement;
  initialProfileId?: string;
  listConfiguredExecutionTargets?: ConfiguredExecutionTargetLoader;
  /**
   * Reads which of the model's profiles can run on each target. Without it, or while it loads or
   * fails, every profile is listed and selectable.
   */
  listProfileCompatibility?: ProfileCompatibilityLoader;
  /** The memory basis the Runs on chip starts from, such as the catalog's; free now without it. */
  memoryBasis?: MemoryBasis;
  model: Model;
  /** The route name the backend rejected as taken on the last deploy attempt. */
  routeConflict?: RouteNameConflict;
  onEstimate?: ResourceEstimateLoader;
  onPreview: (values: LocalDeploymentValues) => Promise<ResolvedLocalModelDeploymentSnapshot>;
  onPreviewMemory?: (values: LocalMemoryPreviewValues) => Promise<LocalDeploymentMemoryPreview>;
  onSubmit: (values: LocalDeploymentValues) => void;
  open: boolean;
  pending: boolean;
  /**
   * The opener's profile memory estimates under `scenario`, for the profile table's Memory column.
   * Without them, profiles show their declared minimums. Under a scenario, or once the operator
   * changes "Sized for", the dialog's own read replaces them once it lands.
   */
  profileEstimates?: ProfileEstimateState;
  /**
   * The sizing scenario the Runs on chip's "Sized for" starts from, such as the catalog's. The
   * dialog's read sizes the chip's scenario, and the launch starts from its engine parameter values
   * on a target where the profile fits.
   */
  scenario?: SizingScenario;
}

const noDerivedParameterIds: ReadonlySet<string> = new Set();
