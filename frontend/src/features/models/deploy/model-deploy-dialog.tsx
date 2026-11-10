import { create } from "@bufbuild/protobuf";
import type { RefObject } from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { DataTablePredicate } from "@/components/data-table/data-table-view";
import { Detail } from "@/components/ui/detail-list";
import { FieldError } from "@/components/ui/field-error";
import { Notice } from "@/components/ui/notice";
import { validateDeploymentName } from "@/domain/deployment-name";
import {
  defaultExecutionTarget,
  executionTargetKey,
  executionTargetName,
  executionTargetPresentation,
  placedExecutionTargetName,
} from "@/domain/execution-target";
import { AdvancedToggle } from "@/features/deployments/advanced-toggle";
import { DeploymentStepHeading } from "@/features/deployments/deployment-form-layout";
import { DeploymentSummaryStep } from "@/features/deployments/deployment-summary-step";
import { DeploymentWizardShell } from "@/features/deployments/deployment-wizard-shell";
import { useAdvancedMode } from "@/features/deployments/use-advanced-mode";
import {
  type ConfiguredExecutionTargetLoader,
  useConfiguredExecutionTargets,
} from "@/features/deployments/use-configured-execution-targets";
import { useWizardSteps } from "@/features/deployments/use-wizard-steps";
import { previewMemory } from "@/features/models/deploy/memory/model-deploy-memory";
import {
  MemoryCapacityWarning,
  ModelDeployMemoryStrip,
} from "@/features/models/deploy/memory/model-deploy-memory-strip";
import {
  deploymentExposure,
  type DeploymentExposureValues,
  deriveRouteName,
  routeBlockingMessage,
  type RouteNameConflict,
  validateDeploymentExposure,
} from "@/features/models/deployment-exposure";
import { DeploymentExposureFields } from "@/features/models/deployment-exposure-fields";
import {
  configurationSpeculativeOptions,
  profileEstimateKey,
  projectProfileRows,
} from "@/features/models/profiles/model-row";
import {
  catalogLocation,
  catalogRunsOn,
  hiddenByFitNote,
  launchProfileAvailability,
  profileLaunchTargets,
  profileNoneRun,
} from "@/features/models/profiles/profile-availability";
import {
  defaultMemoryBasis,
  fitStartingTotalContext,
  type MemoryBasis,
  profileScopeStartingTotalContext,
  profileTargetVerdict,
  verdictFit,
} from "@/features/models/profiles/profile-compatibility";
import { profileCompatibilityStatus } from "@/features/models/profiles/profile-compatibility-copy";
import {
  compareProfiles,
  type ProfileMetricColumnId,
} from "@/features/models/profiles/profile-fields";
import { ProfileTable } from "@/features/models/profiles/profile-table";
import type { ProfileTableField } from "@/features/models/profiles/profile-table-model";
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
  type ScenarioInput,
  scenarioInputs,
  type SizingScenario,
  sizingScenarioKey,
} from "@/features/models/profiles/sizing-scenario";
import {
  type ProfileCompatibilityLoader,
  type ProfileEstimateState,
  useProfileCompatibility,
} from "@/features/models/profiles/use-profile-compatibility";
import { useSizedForChipSettings } from "@/features/models/profiles/use-sized-for-chip-settings";
import {
  CONTEXT_PARAMETER_IDS,
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  effectiveEngineParameters,
  engineParameterError,
  engineParameterInputs as engineParameterInputsOf,
  engineParameterValues,
  filledContextPerRequest,
  PARALLEL_REQUESTS_PARAMETER_ID,
  profileStartingValues,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import {
  configurationKvCachePrecision,
  currentKvCachePrecision,
  kvCachePrecisionCustomizations,
  kvCachePrecisionOverrides,
} from "@/features/models/runtime/kv-cache-precision";
import {
  approvedEngineReleases,
  engineReleaseLabel,
  localDeploymentConfiguration,
  parallelRequestsCustomization,
  profileEngineRelease,
  runtimeCustomizations,
} from "@/features/models/runtime/local-deployment-configuration";
import {
  currentSpeculativeChoice,
  defaultSpeculativeChoice,
  type SpeculativeChoice,
  speculativeChoiceError,
  speculativeCustomizations,
  speculativeMethodChoice,
  speculativeOptionOverrides,
} from "@/features/models/runtime/speculative-choice";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import {
  type EngineParameterDefinition,
  type EngineParameterValues,
  type EngineRelease,
} from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { HardwareClass } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import { type Model, type ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  type ExactPlacement,
  ExactPlacementSchema,
} from "@/gen/dotcore/deployment/v1/placement_pb";
import {
  type ModelDeployment,
  type ResolvedLocalModelDeploymentSnapshot,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { LocalModelDeploymentAdvisory } from "@/gen/dotcore/model/v1/service_pb";
import { AcceleratorMemoryArchitecture } from "@/gen/dotcore/observation/v1/observation_pb";
import {
  type ConfiguredExecutionTarget,
  ExecutionTargetWorkloadKind,
} from "@/gen/dotcore/site/v1/service_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { useDirtyFields } from "@/hooks/use-dirty-fields";
import { type FieldErrors, focusFirstInvalidField, toFieldErrors } from "@/lib/form-validation";
import { formatBytes, formatCompactCount, formatCount } from "@/lib/format";

import { CommandPreviewDialog } from "./model-command-preview-dialog";
import { ModelDeployContextStep } from "./model-deploy-context-step";
import { ModelDeployRuntimeStep } from "./model-deploy-runtime-step";
import { ModelDeployTargetStep } from "./model-deploy-target-step";
import {
  type LocalResourceEstimateValues,
  type ResourceEstimateLoader,
  useResourceEstimate,
} from "./use-resource-estimate";

/**
 * Deploys a catalog model to a local execution target. Basic mode asks for the profile, the
 * context, and the deployment name, plus the target when more than one can deploy the profile; the
 * Advanced checkbox adds every target, the context per request, runtime values, and routing without
 * discarding entered values. The Context step and the profile table's "Sized for" edit one shared
 * context.
 */
/** Keys a launch rejection among the Context step's errors. */
const launchRejectionField = "launchRejection";

export function ModelDeployDialog({
  applications,
  engineReleases,
  hardwareClasses,
  error,
  existingRouteNames = [],
  catalogChips = defaultCatalogChips,
  deployments,
  initialProfileId,
  listConfiguredExecutionTargets,
  listProfileCompatibility,
  memoryBasis: startBasis = defaultMemoryBasis,
  model,
  onEstimate,
  onOpenChange,
  onPreview,
  onPreviewMemory,
  onSubmit,
  openerRef,
  open,
  pending,
  profileEstimates,
  routeConflict,
  scenario: startScenario,
}: ModelDeployDialogProps) {
  const advancedMode = useAdvancedMode(open);
  const { advanced, announcement } = advancedMode;
  const dirtyFields = useDirtyFields<ModelDeployField>(open);
  const [exposureValues, setExposureValues] = useState<DeploymentExposureValues>({
    exposure: "direct",
  });
  // The launch's context, starting from the opener's scenario; changing it leaves the opener alone.
  const [context, setContext] = useState(() => scenarioContext(startScenario));
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
  const [bindingId, setBindingId] = useState(
    () => profileDefaults(model, selectedInitialProfileId).modelArtifactBindingId,
  );
  // The operator's engine parameter edits over the inputs the profile or scenario starts from.
  const [editedInputs, setEditedInputs] = useState<Map<string, string>>(new Map());
  const [storedSpeculativeChoice, setSpeculativeChoice] = useState<SpeculativeChoice>(
    () => profileDefaults(model, selectedInitialProfileId).speculativeChoice,
  );
  // The operator's KV cache precision pick; empty while the profile's default stands.
  const [pickedKvCachePrecision, setPickedKvCachePrecision] = useState("");
  const [customArgumentTokens, setCustomArgumentTokens] = useState<string[]>([]);
  const [memoryPreviewResult, setMemoryPreviewResult] = useState<MemoryPreviewResult>();
  const onPreviewMemoryRef = useRef(onPreviewMemory);
  const [preview, setPreview] = useState<ResolvedLocalModelDeploymentSnapshot>();
  const [previewError, setPreviewError] = useState<string>();
  const [previewOpen, setPreviewOpen] = useState(false);
  const [previewPending, setPreviewPending] = useState(false);
  const previewGeneration = useRef(0);
  const previewButtonRef = useRef<HTMLButtonElement>(null);
  // The operator's target choice: undefined until they choose, null once they clear it.
  const [pickedPlacement, setPickedPlacement] = useState<ExactPlacement | null>();
  const [targetValidationPending, setTargetValidationPending] = useState(false);
  const [ineligibleTarget, setIneligibleTarget] = useState<string>();
  // The Runs on chip's values once the operator changes them, with the starting values they replace.
  const [operatorChip, setOperatorChip] = useState<{
    start: string | undefined;
    values: readonly string[];
  }>();
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
  const [name, setName] = useState(model.displayName);
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
  // The catalog's Runs on rule over the dialog's own read; after its scenario's read failed, Runs on
  // judges support alone.
  const sizingFailed = profileCompatibility.sizingFailed;
  const runsOnFilter = useMemo(
    () =>
      compatibility &&
      catalogRunsOn(compatibility, executionTargets, { basis, fit: !sizingFailed }),
    [basis, compatibility, executionTargets, sizingFailed],
  );
  const runsOn = useMemo(
    () =>
      compatibility &&
      runsOnFilter && {
        ...runsOnFilter,
        noneRun: profileNoneRun(compatibility, runsOnFilter, model, profileRows, executionTargets),
      },
    [compatibility, executionTargets, model, profileRows, runsOnFilter],
  );
  // The catalog's chips, less any that would hide the requested profile. Runs on joins them when
  // the read first lands, and they stay fixed from then on, so later reads never move them.
  const initialProfileRow = profileRows.find((row) => row.profileId === initialProfileId);
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
  const sizedForSettings = useSizedForChipSettings(
    runsOnFilter ? { onChange: chooseSizedFor, start: startScenario, value: scenario } : undefined,
    runsOnFilter ? { onChange: setBasis, start: startBasis, value: basis } : undefined,
  );
  const [landedStart, setLandedStart] = useState<DataTablePredicate<ProfileTableField>[]>();
  if (runsOnFilter && !landedStart)
    setLandedStart(profileTableStart(catalogChips, runsOnFilter.field, initialProfileRow));
  const start = useMemo(
    () => landedStart ?? profileTableStart(catalogChips, undefined, initialProfileRow),
    [catalogChips, initialProfileRow, landedStart],
  );
  // A read without targets drops the chip, and the table brings it back on its starting values.
  if (!runsOnFilter && operatorChip) setOperatorChip(undefined);
  const startChip = start.find(({ field }) => field === "runsOn")?.values;
  const startChipKey = startChip?.join("\0");
  // A new starting chip, such as the one the read brings, replaces the operator's.
  const chipValues =
    operatorChip && operatorChip.start === startChipKey ? operatorChip.values : (startChip ?? []);
  const chipKey = chipValues.join("\0");
  // Without a chip, or with one naming only machines that left the site, profiles may run anywhere.
  const location = useMemo(
    () => catalogLocation(runsOnFilter, chipKey ? chipKey.split("\0") : []),
    [chipKey, runsOnFilter],
  );
  const scopes = location.scopes;
  const profileAvailability = useMemo(
    () =>
      compatibility
        ? launchProfileAvailability(compatibility, model.modelId, executionTargets, scopes, basis)
        : undefined,
    [basis, compatibility, executionTargets, model.modelId, scopes],
  );
  // The table's rows show the total context each profile's launch starts from in the Runs on scope:
  // the chosen one, else the largest starting value a fit gives there.
  const tableProfileRows = useMemo(
    () =>
      profileRows.map((row) => {
        const rowProfile = model.profiles.find(
          (candidate) => candidate.profileId === row.profileId,
        );
        const starting = engineParameterInputsOf(
          profileCompatibility.sizing?.engineValues.get(
            profileEstimateKey(model.modelId, row.profileId),
          ) ??
            profileStartingValues(
              profileEngineRelease(model, rowProfile, engineReleases),
              rowProfile,
            ),
        );
        const startingTotal = starting.get(TOTAL_CONTEXT_PARAMETER_ID);
        return {
          ...row,
          totalContext:
            scenario?.totalContextTokens ??
            (profileCompatibility.compatibility &&
              profileScopeStartingTotalContext(
                profileCompatibility.compatibility,
                model.modelId,
                row.profileId,
                scopes,
              )) ??
            (startingTotal === undefined ? undefined : Number(startingTotal)),
        };
      }),
    [
      engineReleases,
      model,
      profileCompatibility.compatibility,
      profileCompatibility.sizing,
      profileRows,
      scenario,
      scopes,
    ],
  );
  const profileLaunchCandidates = compatibility
    ? profileLaunchTargets(compatibility, model.modelId, profileId, executionTargets, scopes)
    : undefined;
  // Under a scenario, the targets where the profile fits on the memory basis, which the launch
  // prefers.
  const fittingCandidates = sizing
    ? profileLaunchCandidates?.filter(
        (target) =>
          verdictFit(profileTargetVerdict(sizing, model.modelId, profileId, target), basis) ===
          "fits",
      )
    : undefined;

  // With compatibility, the target follows the profile: the operator's choice while it can still
  // run the profile, otherwise the only deployable target in the Runs on scope that can. Without
  // it, the operator's choice while deployable, otherwise the dialog's default target.
  const pickedTarget = pickedPlacement
    ? executionTargets.find(
        (target) => executionTargetKey(target) === executionTargetKey(pickedPlacement),
      )
    : undefined;
  // Whether a target can deploy now and, when compatibility is known, run the profile.
  const launchesOn = (candidateProfileId: string, target: ConfiguredExecutionTarget) =>
    executionTargetPresentation(target).deployable &&
    (!compatibility ||
      profileTargetVerdict(compatibility, model.modelId, candidateProfileId, target)?.status ===
        "runs");
  const pickedLaunches = Boolean(pickedTarget && launchesOn(profileId, pickedTarget));
  // The first fitting target stays the default while it can still launch the profile, so later
  // reads ranking targets differently never move the launch.
  const [fitDefault, setFitDefault] = useState<{ profileId: string; targetKey: string }>();
  if (fittingCandidates?.length && fitDefault?.profileId !== profileId)
    setFitDefault({ profileId, targetKey: executionTargetKey(fittingCandidates[0]) });
  const fitDefaultTarget =
    fitDefault?.profileId === profileId
      ? profileLaunchCandidates?.find(
          (target) => executionTargetKey(target) === fitDefault.targetKey,
        )
      : undefined;
  const selectedExecutionTarget = pickedLaunches
    ? pickedTarget
    : profileLaunchCandidates
      ? (fitDefaultTarget ??
        fittingCandidates?.[0] ??
        (profileLaunchCandidates.length === 1 ? profileLaunchCandidates[0] : undefined))
      : pickedPlacement === undefined
        ? defaultExecutionTarget(executionTargets)
        : undefined;
  // The total context the selected target's largest fit starts the launch from, from the dialog's
  // read.
  const readFitStart =
    compatibility && selectedExecutionTarget
      ? fitStartingTotalContext(
          profileTargetVerdict(compatibility, model.modelId, profileId, selectedExecutionTarget),
        )
      : undefined;
  // Free memory moves with every read, so the first starting value a read of the profile, target
  // and scenario gives, from a fit or none, stays for the dialog's life, and the total never moves
  // under the operator. A read of another scenario doesn't count: it's only shown until the
  // requested one lands.
  const [frozenStarts, setFrozenStarts] = useState<ReadonlyMap<string, number | undefined>>(
    new Map(),
  );
  const startKey = (startScenario: SizingScenario | undefined) =>
    profile && selectedExecutionTarget
      ? `${profile.profileId}\0${sizingScenarioKey(startScenario)}\0${selectedExecutionTarget.siteNodeId}\0${selectedExecutionTarget.executionTargetId}`
      : undefined;
  const frozenStartKey = startKey(scenario);
  if (
    frozenStartKey &&
    compatibility &&
    profileCompatibility.sizing &&
    !frozenStarts.has(frozenStartKey)
  )
    setFrozenStarts(new Map(frozenStarts).set(frozenStartKey, readFitStart));
  const fitStart =
    frozenStartKey && frozenStarts.has(frozenStartKey)
      ? frozenStarts.get(frozenStartKey)
      : readFitStart;
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
  // Where the total context comes from: the operator's choice when it differs from the largest fit,
  // which the largest fit can replace again, else the largest fit. A chosen context per request
  // pins the total too, so the value, not the pin, decides. Returning to the largest fit forgets
  // the default's frozen value, so the next read of the defaults freezes the live one and a value
  // frozen from a stale read is corrected.
  const customTotal = Boolean(context.inputs && Number(context.inputs.total) !== fitStart);
  const totalSource = customTotal
    ? {
        label: "Custom",
        onReset:
          readFitStart === undefined
            ? undefined
            : () => {
                const defaultKey = startKey(undefined);
                if (defaultKey) {
                  const remaining = new Map(frozenStarts);
                  remaining.delete(defaultKey);
                  setFrozenStarts(remaining);
                }
                resetContext();
              },
      }
    : fitStart === undefined
      ? undefined
      : { label: "Largest fit" };
  const placementNodeId = selectedExecutionTarget?.siteNodeId;
  const placementTargetId = selectedExecutionTarget?.executionTargetId;
  const placement = useMemo(
    () =>
      placementNodeId !== undefined && placementTargetId !== undefined
        ? create(ExactPlacementSchema, {
            executionTargetId: placementTargetId,
            siteNodeId: placementNodeId,
          })
        : null,
    [placementNodeId, placementTargetId],
  );

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

  const invalidatePreview = () => {
    previewGeneration.current += 1;
    setPreviewOpen(false);
    setPreviewPending(false);
    setPreview(undefined);
    setPreviewError(undefined);
  };

  const selectProfile = (nextProfileId: string) => {
    const defaults = profileDefaults(model, nextProfileId);
    setProfileId(nextProfileId);
    setIneligibleTarget(undefined);
    // A chosen target that can't launch the new profile gives way to the profile's own targets.
    if (pickedTarget && !launchesOn(nextProfileId, pickedTarget)) setPickedPlacement(undefined);
    setBindingId(defaults.modelArtifactBindingId);
    setEditedInputs(new Map());
    setSpeculativeChoice(defaults.speculativeChoice);
    setPickedKvCachePrecision("");
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
    if (next !== profileId) queueMicrotask(() => selectProfileRef.current(next));
  }, [chipKey, initialProfileId, profileAvailability, profileId, profileRows]);

  // The latest inputs, for a deferred blur check that runs after a change the blur preceded, such
  // as a total context preset clicked while the context per request had focus.
  const engineParameterInputsRef = useRef(engineParameterInputs);
  useEffect(() => {
    engineParameterInputsRef.current = engineParameterInputs;
  });
  const values = engineParameterValues(declaredValues, exposedParameters, engineParameterInputs);
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
        setFieldError(shownParameter.parameterId, engineParameterError(shownParameter, nextInputs));
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
          const error = engineParameterError(shownParameter, engineParameterInputsRef.current);
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
      engineParameterError(parameter, engineParameterInputs),
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
    if (!errors.size) return true;
    pendingFocusRef.current = errors;
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

  // Runs after the shell focuses a new step heading, so the invalid field keeps focus.
  useEffect(() => {
    const errors = pendingFocusRef.current;
    if (!errors) return;
    pendingFocusRef.current = undefined;
    focusFirstInvalidField(errors, fieldControls());
  });

  const estimateBindingId = binding ? bindingId : undefined;
  const estimateConfiguration = profile
    ? localDeploymentConfiguration(profile, engineRelease, {
        bindingId: estimateBindingId ?? "",
        customArgumentTokens,
        declaredValues,
        inputs: engineParameterInputs,
        optionOverrides,
      })
    : undefined;
  // The launch values arrive with the read, so the first estimate and the deploy wait for them.
  const launchValuesPending =
    profileCompatibility.estimates.pending ||
    (Boolean(scenario) && !ownReadValues && defaultsCompatibility.estimates.pending);
  const launchValuesLoadingVisible = useDelayedLoading(launchValuesPending);
  const estimateKey =
    // The selected target decides the estimate of a profile supporting several hardware classes,
    // so the estimate waits for the targets instead of asking twice.
    profile &&
    estimateBindingId &&
    values &&
    optionOverrides &&
    !targetsLoading &&
    !launchValuesPending
      ? [
          profileId,
          estimateBindingId,
          exposedParameters
            .map(
              (parameter) =>
                `${parameter.parameterId}=${engineParameterInputs.get(parameter.parameterId) ?? ""}`,
            )
            .join("|"),
          declaredValues?.values
            .map((value) => `${value.parameterId}=${String(value.value.value)}`)
            .join("|") ?? "",
          JSON.stringify(customArgumentTokens),
          optionOverrides
            .map((override) => `${override.optionId}=${String(override.values[0]?.value.value)}`)
            .join("|"),
          placementNodeId ?? "",
          placementTargetId ?? "",
        ].join("\0")
      : undefined;
  // Memory occupancy moves on its own, such as when another model starts, so every compatibility
  // read also refreshes the placement preview.
  const [compatibilityReads, setCompatibilityReads] = useState({
    count: 0,
    read: profileCompatibility.compatibility,
  });
  if (compatibilityReads.read !== profileCompatibility.compatibility)
    setCompatibilityReads({
      count: compatibilityReads.count + 1,
      read: profileCompatibility.compatibility,
    });
  const estimate = useResourceEstimate(
    onEstimate,
    estimateKey && estimateConfiguration
      ? {
          key: estimateKey,
          revision: compatibilityReads.count,
          values: {
            configuration: estimateConfiguration,
            placement: placement ?? undefined,
            profileId,
          },
        }
      : undefined,
  );
  const launchRejectionMessage =
    estimate.launchRejection && launchRejectionParameter
      ? operatorSentence(estimate.launchRejection)
      : undefined;
  const contextFieldErrors = launchRejectionMessage
    ? new Map(fieldErrors).set(launchRejectionParameter.parameterId, launchRejectionMessage)
    : fieldErrors;
  const predictedMemory = deployments
    ? previewMemory({
        applications,
        deployment: { name: name.trim() || model.displayName, profileId },
        deployments,
        // Without a selected profile, such as when none runs on the target, nothing is previewed.
        // The estimate waits for the launch values, so the preview is pending until they land.
        estimate: profile
          ? { ...estimate, pending: estimate.pending || launchValuesPending }
          : { loadingVisible: false, pending: false },
        target: selectedExecutionTarget && {
          executionTargetId: selectedExecutionTarget.executionTargetId,
          name: executionTargetName(selectedExecutionTarget),
          nodeName:
            selectedExecutionTarget.siteNodeDisplayName || selectedExecutionTarget.siteNodeId,
          siteNodeId: selectedExecutionTarget.siteNodeId,
        },
        targetsLoading: targetsLoadingPresentation,
      })
    : undefined;
  useEffect(() => {
    onPreviewMemoryRef.current = onPreviewMemory;
  }, [onPreviewMemory]);
  useEffect(() => {
    const previewMemory = onPreviewMemoryRef.current;
    const previewSpeculativeOptions = configurationSpeculativeOptions(
      model,
      configuration,
      profile,
    );
    const previewKvCachePrecision = configurationKvCachePrecision(model, configuration, profile);
    const previewSpeculativeOverrides = speculativeOptionOverrides(
      previewSpeculativeOptions,
      currentSpeculativeChoice(previewSpeculativeOptions, storedSpeculativeChoice),
    );
    const previewConfiguration = profile
      ? localDeploymentConfiguration(profile, engineRelease, {
          bindingId: estimateBindingId ?? "",
          customArgumentTokens,
          declaredValues,
          inputs: engineParameterInputs,
          optionOverrides: previewSpeculativeOverrides && [
            ...previewSpeculativeOverrides,
            ...kvCachePrecisionOverrides(
              previewKvCachePrecision,
              currentKvCachePrecision(previewKvCachePrecision, pickedKvCachePrecision),
            ),
          ],
        })
      : undefined;
    if (!previewMemory || !estimateKey || !previewConfiguration || !placement) return;

    let active = true;
    previewMemory({
      configuration: previewConfiguration,
      profileId,
      placement,
    }).then(
      (preview) => {
        if (active) setMemoryPreviewResult({ preview, placement, profileId });
      },
      // The explicit command preview reports rejections; this background read only drops its facts.
      () => {
        if (active) setMemoryPreviewResult({ placement, profileId });
      },
    );

    return () => {
      active = false;
    };
  }, [
    configuration,
    customArgumentTokens,
    engineParameterInputs,
    engineRelease,
    declaredValues,
    estimateBindingId,
    estimateKey,
    model,
    profile,
    profileId,
    placement,
    pickedKvCachePrecision,
    storedSpeculativeChoice,
  ]);
  // The latest result for the selected target stays visible while a newer one resolves, so the
  // wording does not flicker as parameters are typed; the architecture depends only on the target.
  const memoryPreview =
    placement &&
    memoryPreviewResult?.placement?.executionTargetId === placement.executionTargetId &&
    memoryPreviewResult?.placement?.siteNodeId === placement.siteNodeId
      ? memoryPreviewResult.preview
      : undefined;
  const hostKvOffloadSharesMemory = Boolean(
    memoryPreview?.advisories.includes(LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY),
  );
  const poolBelowDeviceMinimum = Boolean(
    memoryPreview?.advisories.includes(
      LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM,
    ),
  );
  // The target's preview names the release it selects; until then, every approved release may run.
  const selectedEngineRelease =
    memoryPreviewResult?.profileId === profileId ? memoryPreview?.engineRelease : undefined;
  const engineReleaseLabels = selectedEngineRelease
    ? [engineReleaseLabel(selectedEngineRelease)]
    : approvedEngineReleases(model, profile, engineReleases).map(engineReleaseLabel);
  const previewLoadingVisible = useDelayedLoading(previewPending);

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
    const generation = ++previewGeneration.current;
    previewButtonRef.current?.blur();
    setPreviewOpen(true);
    setPreviewPending(true);
    setPreviewError(undefined);
    setPreview(undefined);
    try {
      const result = await onPreview({
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
      if (generation === previewGeneration.current) setPreview(result);
    } catch (previewFailure) {
      if (generation === previewGeneration.current)
        setPreviewError(
          previewFailure instanceof Error ? previewFailure.message : "Preview was rejected.",
        );
    } finally {
      if (generation === previewGeneration.current) setPreviewPending(false);
    }
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
  const siteNotice = effectiveTargetsError ? (
    <Notice role="alert" tone="destructive">
      {effectiveTargetsError}
    </Notice>
  ) : executionTargetsCurrent && executionTargets.length === 0 ? (
    <Notice role="alert" tone="destructive">
      No model execution targets are configured. Configure one before deploying.
    </Notice>
  ) : executionTargetsCurrent && deployableExecutionTargets.length === 0 ? (
    <Notice role="alert" tone="destructive">
      No configured execution target can deploy models right now.
    </Notice>
  ) : undefined;
  const firstStepNotice =
    step !== steps[0] ? undefined : targetsLoadingVisible && !targetStepShown ? (
      <p className="text-xs text-muted-foreground" role="status">
        Loading configured execution targets…
      </p>
    ) : (
      siteNotice
    );
  const ineligibleNotice = ineligibleTarget ? (
    <Notice role="alert" tone="destructive">
      {`${ineligibleTarget} is no longer eligible to run this deployment.`}
    </Notice>
  ) : undefined;

  return (
    <DeploymentWizardShell
      announcement={announcement}
      currentStep={step}
      footerStart={
        <AdvancedToggle checked={advanced} disabled={pending} onChange={toggleAdvanced} />
      }
      onOpenChange={onOpenChange}
      onPrimary={handlePrimary}
      onStepChange={handleStepChange}
      open={open}
      openerRef={openerRef}
      pending={pending}
      persistentContent={
        predictedMemory ? <ModelDeployMemoryStrip memory={predictedMemory} /> : undefined
      }
      primaryDisabled={step === "Deploy" && deployDisabled}
      primaryLabel={step === "Deploy" ? (pending ? "Deploying…" : "Deploy") : "Next"}
      resourceColorKey={model.modelId}
      resourceName={model.displayName}
      steps={steps}
      submitError={step === "Deploy" ? error : undefined}
      title="Deploy model"
      visitedSteps={wizard.visited}
    >
      {step === "Target" ? (
        <ModelDeployTargetStep
          disabled={pending}
          error={fieldErrors.get("placement")}
          executionTargets={
            advanced ? executionTargets : (profileLaunchCandidates ?? executionTargets)
          }
          inputRef={executionTargetRef}
          loading={targetsLoading}
          loadingMessageVisible={targetsLoadingVisible && !effectiveTargetsError}
          loadingPresentation={targetsLoadingPresentation}
          notices={
            <>
              {ineligibleNotice}
              {!advanced && profileLaunchCandidates?.length === 0 ? (
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
            setPickedPlacement(target ? exactPlacement(target) : null);
            setIneligibleTarget(undefined);
            if (fieldErrors.has("placement"))
              setFieldError("placement", target ? undefined : "Execution target is required.");
            invalidatePreview();
          }}
          placement={placement}
          targetsCurrent={executionTargetsCurrent}
          unavailableReason={
            compatibility && selectedProfileRow
              ? (target) => {
                  const verdict = profileTargetVerdict(
                    compatibility,
                    model.modelId,
                    profileId,
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
        <div className="grid gap-4">
          <DeploymentStepHeading>Profile</DeploymentStepHeading>
          {firstStepNotice}
          {ineligibleNotice}
          {!compatibility && executionTargets.length ? (
            profileCompatibility.error || profileCompatibility.compatibility ? (
              <Notice tone="warning">
                Couldn&apos;t check which profiles can run on this site.
              </Notice>
            ) : compatibilityLoadingVisible ? (
              <p className="text-xs text-muted-foreground" role="status">
                Checking which profiles can run on this site…
              </p>
            ) : null
          ) : null}
          {scenario &&
          listProfileCompatibility &&
          !sizing &&
          !profileCompatibility.estimates.pending ? (
            <Notice tone="warning">
              Couldn&apos;t size this context, so other settings start from the profile defaults.
            </Notice>
          ) : null}
          <ProfileTable
            availability={profileAvailability}
            chipSettings={sizedForSettings}
            defaultColumns={profileStepColumns}
            disabled={pending}
            estimatesPending={tableEstimates?.pending ?? false}
            footnote={hiddenByFitNote}
            inputRef={profileRef}
            onBlur={() => {
              if (dirtyFields.isDirty("profileId"))
                setTimeout(() => setFieldError("profileId", profileError), 0);
            }}
            onRunsOnChange={(values) => setOperatorChip({ start: startChipKey, values })}
            onSelect={(nextProfileId) => {
              operatorSelectedRef.current = true;
              dirtyFields.markDirty("profileId");
              selectProfile(nextProfileId);
            }}
            onShownChange={updateShownProfiles}
            profiles={shownProfileRows}
            runsOn={runsOn}
            selectedProfileId={profileId}
            start={start}
          />
          {fieldErrors.get("profileId") ? (
            <FieldError>{fieldErrors.get("profileId")}</FieldError>
          ) : null}
          {customizations.length ? (
            <p className="text-xs text-muted-foreground" role="status">
              Changing the profile resets runtime settings to its defaults.
            </p>
          ) : null}
        </div>
      ) : null}
      {step === "Context" ? (
        <ModelDeployContextStep
          contextHint={contextHint}
          disabled={pending}
          engineParameterRefs={engineParameterRefs}
          exposedParameters={contextParameters}
          fieldErrors={contextFieldErrors}
          inputs={engineParameterInputs}
          model={model}
          onParameterBlur={blurContext}
          onParameterChange={changeContext}
          profile={profile}
          totalSource={totalSource}
        />
      ) : null}
      {step === "Runtime" ? (
        <ModelDeployRuntimeStep
          artifactRef={artifactRef}
          bindingId={bindingId}
          customArgumentTokens={customArgumentTokens}
          customArguments={{
            add: () => updateCustomArguments((tokens) => [...tokens, ""]),
            move: (index, direction) =>
              updateCustomArguments((tokens) => moveToken(tokens, index, direction)),
            remove: (index) =>
              updateCustomArguments((tokens) => tokens.filter((_, item) => item !== index)),
            update: (index, value) =>
              updateCustomArguments((tokens) =>
                tokens.map((token, item) => (item === index ? value : token)),
              ),
          }}
          disabled={pending}
          engineParameterRefs={engineParameterRefs}
          engineReleases={engineReleaseLabels}
          exposedParameters={exposedParameters}
          fieldErrors={fieldErrors}
          hostKvOffloadSharesMemory={hostKvOffloadSharesMemory}
          inputs={engineParameterInputs}
          model={model}
          onArtifactBlur={() => {
            if (!dirtyFields.isDirty("artifact")) return;
            setTimeout(
              () => setFieldError("artifact", binding ? undefined : "Select a model artifact."),
              0,
            );
          }}
          onArtifactChange={(nextBindingId) => {
            dirtyFields.markDirty("artifact");
            setBindingId(nextBindingId);
            if (fieldErrors.has("artifact"))
              setFieldError("artifact", nextBindingId ? undefined : "Select a model artifact.");
            invalidatePreview();
          }}
          onParameterBlur={(parameter) => {
            if (!dirtyFields.isDirty(parameter.parameterId)) return;
            setTimeout(
              () =>
                setFieldError(
                  parameter.parameterId,
                  engineParameterError(parameter, engineParameterInputsRef.current),
                ),
              0,
            );
          }}
          onParameterChange={(parameter, nextInput) => {
            dirtyFields.markDirty(parameter.parameterId);
            const nextInputs = new Map(engineParameterInputs).set(parameter.parameterId, nextInput);
            // Parallel requests belong to the shared context, which "Sized for" also edits and a
            // profile change keeps.
            if (parameter.parameterId === PARALLEL_REQUESTS_PARAMETER_ID)
              setContext((previous) => ({ ...previous, parallelRequests: nextInput }));
            else setEditedInputs((inputs) => new Map(inputs).set(parameter.parameterId, nextInput));
            if (fieldErrors.has(parameter.parameterId))
              setFieldError(parameter.parameterId, engineParameterError(parameter, nextInputs));
            invalidatePreview();
          }}
          onPreview={() => void runPreview()}
          previewButtonRef={previewButtonRef}
          previewDisabled={
            pending ||
            !exposedParameters.length ||
            !configuration ||
            deploymentPrerequisiteUnavailable
          }
          previewPending={previewPending}
          profile={profile}
          revealedErrors={revealedErrors}
          kvCachePrecision={{
            onChange: (precision) => {
              setPickedKvCachePrecision(precision);
              invalidatePreview();
            },
            option: kvCachePrecisionOption,
            precision: kvCachePrecision,
          }}
          speculative={{
            choice: speculativeChoice,
            countRef: speculativeCountRef,
            onCountChange: (count) => {
              const nextChoice = { ...speculativeChoice, count };
              dirtyFields.markDirty("speculativeCount");
              setSpeculativeChoice(nextChoice);
              if (fieldErrors.has("speculativeCount"))
                setFieldError(
                  "speculativeCount",
                  speculativeChoiceError(speculativeOptions, nextChoice),
                );
              invalidatePreview();
            },
            // A kept count can fall outside the new method's range, so the method change validates
            // it at once rather than waiting for a blur the count input never received.
            onMethodChange: (optionId) => {
              const nextChoice = speculativeMethodChoice(
                speculativeOptions,
                configurationSpeculativeOptions(model, configuration),
                speculativeChoice,
                optionId,
              );
              dirtyFields.markDirty("speculativeCount");
              setSpeculativeChoice(nextChoice);
              setFieldError(
                "speculativeCount",
                speculativeChoiceError(speculativeOptions, nextChoice),
              );
              invalidatePreview();
            },
            // Validates at once: a deferred check would run after a method change that the blur
            // precedes and overwrite its error with the previous method's verdict.
            onCountBlur: () => {
              if (dirtyFields.isDirty("speculativeCount"))
                setFieldError(
                  "speculativeCount",
                  speculativeChoiceError(speculativeOptions, speculativeChoice),
                );
            },
            options: speculativeOptions,
          }}
        />
      ) : null}
      {step === "Deploy" ? (
        <DeploymentSummaryStep
          disabled={pending}
          name={name}
          nameError={fieldErrors.get("name")}
          nameInputId="model-deployment-name"
          nameRef={nameRef}
          notices={
            <>
              {siteNotice}
              {launchValuesLoadingVisible ? (
                <p className="text-xs text-muted-foreground" role="status">
                  Loading launch values…
                </p>
              ) : null}
              {predictedMemory ? <MemoryCapacityWarning memory={predictedMemory} /> : null}
              {poolBelowDeviceMinimum && memoryPreview ? (
                <Notice tone="warning">{poolBelowDeviceMinimumMessage(memoryPreview)}</Notice>
              ) : null}
            </>
          }
          onNameBlur={() => {
            if (dirtyFields.isDirty("name"))
              setTimeout(() => setFieldError("name", validateDeploymentName(name)), 0);
          }}
          onNameChange={(nextName) => {
            dirtyFields.markDirty("name");
            setName(nextName);
            if (fieldErrors.has("name")) setFieldError("name", validateDeploymentName(nextName));
            if (following && fieldErrors.has("routeName"))
              setFieldError("routeName", validateRoute(exposureValues, nextName));
            invalidatePreview();
          }}
          nameFields={
            <DeploymentExposureFields
              advanced={advanced}
              defaultRouteName={deriveRouteName(name)}
              disabled={pending}
              onChange={(next) => {
                if (next.routeName !== exposureValues.routeName) dirtyFields.markDirty("routeName");
                setExposureValues(next);
                if (next.exposure === "hidden") setFieldError("routeName", undefined);
                else if (fieldErrors.has("routeName"))
                  setFieldError("routeName", validateRoute(next));
                invalidatePreview();
              }}
              onRouteNameBlur={(next) => {
                if (dirtyFields.isDirty("routeName"))
                  setFieldError("routeName", validateRoute(next));
              }}
              routeNameError={
                exposureValues.exposure === "direct"
                  ? (routeBlocking ?? fieldErrors.get("routeName"))
                  : undefined
              }
              routeNameRef={routeNameRef}
              values={exposureValues}
            />
          }
        >
          <Detail
            label="Target"
            value={selectedExecutionTarget ? executionTargetName(selectedExecutionTarget) : "None"}
          />
          <Detail label="Profile" value={profile?.displayName || profileId} />
          <Detail
            label="Context"
            value={contextSummary(
              engineParameterInputs,
              filledContextPerRequest(model, configuration, engineParameterInputs),
            )}
          />
          {deployCustomizations.length ? (
            <Detail label="Runtime" value={`Customized: ${deployCustomizations.join(" · ")}`} />
          ) : null}
        </DeploymentSummaryStep>
      ) : null}
      <CommandPreviewDialog
        error={previewError}
        loadingMessageVisible={previewLoadingVisible}
        onOpenChange={setPreviewOpen}
        onRetry={() => void runPreview()}
        open={previewOpen}
        openerRef={previewButtonRef}
        pending={previewPending}
        snapshot={preview}
      />
    </DeploymentWizardShell>
  );
}

export interface ModelDeployDialogProps {
  error?: string;
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
  openerRef?: RefObject<HTMLElement | null>;
  /** The route name the backend rejected as taken on the last deploy attempt. */
  routeConflict?: RouteNameConflict;
  onEstimate?: ResourceEstimateLoader;
  onOpenChange: (open: boolean) => void;
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

interface LocalMemoryPreviewValues extends LocalResourceEstimateValues {
  placement: ExactPlacement;
}

/**
 * The facts of a background deployment preview: whether the selected execution target shares host
 * memory with its accelerators, the non-blocking advisories for the previewed configuration, the
 * sizes a memory advisory compares, and the engine release selected for the target.
 */
interface LocalDeploymentMemoryPreview {
  acceleratorMemoryArchitecture: AcceleratorMemoryArchitecture;
  advisories: readonly LocalModelDeploymentAdvisory[];
  /** The engine release of the artifact selected for the target, absent when unknown. */
  engineRelease?: { engineReleaseId: string; version: string };
  /** The selected target's memory-pool total, absent when unknown. */
  memoryPoolTotalBytes?: bigint;
  /** The profile's largest per-device accelerator memory minimum, absent when it declares none. */
  minimumMemoryBytesPerDevice?: bigint;
}

interface LocalDeploymentValues extends LocalResourceEstimateValues {
  exposure: ReturnType<typeof deploymentExposure>;
  name: string;
  placement: ExactPlacement;
}

interface MemoryPreviewResult {
  preview?: LocalDeploymentMemoryPreview;
  placement: ExactPlacement;
  profileId: string;
}

type ModelDeployStep = (typeof MODEL_DEPLOY_STEPS)[number];

/** A field the operator can change: a named field or an engine parameter ID. */
type ModelDeployField = string;

// Intelligence belongs to the model, so the dialog leaves it to its View menu.
const profileStepColumns: readonly ProfileMetricColumnId[] = [
  "memory",
  "maximumContext",
  "totalContext",
];

/** Every step in flow order; the dialog shows a subset depending on Advanced and the targets. */
const MODEL_DEPLOY_STEPS = ["Profile", "Target", "Context", "Runtime", "Deploy"] as const;

/** The backend's rejection reason as a sentence of the form. */
function operatorSentence(reason: string): string {
  return `${reason.charAt(0).toUpperCase()}${reason.slice(1)}.`;
}

function modelDeploySteps(target: boolean, advanced: boolean): ModelDeployStep[] {
  return MODEL_DEPLOY_STEPS.filter(
    (step) =>
      (step === "Target" && target) ||
      (step === "Runtime" && advanced) ||
      step === "Profile" ||
      step === "Context" ||
      step === "Deploy",
  );
}

// A vanished step falls forward: Target to Context, which always shows, and Runtime to Deploy.
function fallbackStep(missingStep: string): string {
  return missingStep === "Target" ? "Context" : "Deploy";
}

function exactPlacement(target: ConfiguredExecutionTarget): ExactPlacement {
  return create(ExactPlacementSchema, {
    siteNodeId: target.siteNodeId,
    executionTargetId: target.executionTargetId,
  });
}

function moveToken(tokens: string[], index: number, direction: -1 | 1): string[] {
  const nextIndex = index + direction;
  if (nextIndex < 0 || nextIndex >= tokens.length) return tokens;
  const next = [...tokens];
  [next[index], next[nextIndex]] = [next[nextIndex], next[index]];
  return next;
}

/** The warning for system memory below the profile's minimum, with sizes when known. */
function poolBelowDeviceMinimumMessage({
  memoryPoolTotalBytes,
  minimumMemoryBytesPerDevice,
}: LocalDeploymentMemoryPreview): string {
  const comparison =
    memoryPoolTotalBytes !== undefined && minimumMemoryBytesPerDevice !== undefined
      ? `This target's system memory (${formatBytes(memoryPoolTotalBytes)}) is below this profile's ${formatBytes(minimumMemoryBytesPerDevice)} minimum.`
      : "This target's system memory is below this profile's minimum.";

  return `${comparison} The model may fail to load or slow down the machine.`;
}

/**
 * The context a launch carries, which the Context step and the Runs on chip's "Sized for" both
 * edit. Until the operator, the chip, or the opener's scenario chooses the total context and
 * context per request, the total context follows the default for the profile and target.
 */
interface LaunchContext {
  /** The chosen total context and context per request inputs; absent while the default applies. */
  inputs?: Record<ScenarioInput, string>;
  /** The operator's parallel requests; absent while the profile's starting value stands. */
  parallelRequests?: string;
  /** The latest valid chosen values, which "Sized for" shows and the compatibility read sizes. */
  scenario?: SizingScenario;
}

/** The context a scenario chooses, or the default context without one. */
function scenarioContext(scenario: SizingScenario | undefined): LaunchContext {
  return scenario ? { inputs: scenarioInputs(scenario), scenario } : {};
}

/**
 * The context once the edited input is left: the other of total context and context per request
 * follows it, and "Sized for" takes the inputs when they are valid. Unchanged, the same context.
 */
function settledContext(context: LaunchContext, edited: ScenarioInput): LaunchContext {
  if (!context.inputs) return context;

  const inputs = followContextInputs(context.inputs, edited);
  const scenario = scenarioFromInputs(inputs) ?? context.scenario;
  return inputs === context.inputs && sameSizingScenario(scenario, context.scenario)
    ? context
    : { ...context, inputs, scenario };
}

/**
 * The errors without those of the total context and context per request, which a context chosen
 * or reset elsewhere replaces. Parallel requests keep theirs, since their input stays.
 */
function withoutChosenContextErrors(errors: FieldErrors): FieldErrors {
  const nextErrors = new Map(errors);
  nextErrors.delete(TOTAL_CONTEXT_PARAMETER_ID);
  nextErrors.delete(CONTEXT_PER_REQUEST_PARAMETER_ID);
  return nextErrors;
}

/** The scenario input an engine parameter edits, if it is the total context or per request. */
function scenarioInput(parameterId: string): ScenarioInput | undefined {
  if (parameterId === TOTAL_CONTEXT_PARAMETER_ID) return "total";
  if (parameterId === CONTEXT_PER_REQUEST_PARAMETER_ID) return "perRequest";
  return undefined;
}

/**
 * The engine parameter inputs a launch shows and the declared values it submits beneath them. The
 * declared values are the starting values: the backend's read for the dialog's scenario, else its
 * read without a scenario, else the profile's defaults with the release defaults they leave out.
 * The inputs overlay them with the operator's runtime edits, then the launch's context, whose total
 * context follows the starting value of the target's largest fit until chosen.
 */
function useLaunchEngineValues(
  scenarioValues: EngineParameterValues | undefined,
  engineRelease: EngineRelease | undefined,
  profile: ModelProfile | undefined,
  edits: ReadonlyMap<string, string>,
  context: LaunchContext,
  fitStart: number | undefined,
) {
  return useMemo(() => {
    const startingValues = scenarioValues ?? profileStartingValues(engineRelease, profile);
    const inputs = new Map([...engineParameterInputsOf(startingValues), ...edits]);
    if (context.parallelRequests !== undefined)
      inputs.set(PARALLEL_REQUESTS_PARAMETER_ID, context.parallelRequests);
    if (context.inputs) {
      inputs.set(TOTAL_CONTEXT_PARAMETER_ID, context.inputs.total);
      inputs.set(CONTEXT_PER_REQUEST_PARAMETER_ID, context.inputs.perRequest);
    } else if (fitStart !== undefined) {
      inputs.set(TOTAL_CONTEXT_PARAMETER_ID, String(fitStart));
    }
    return { declaredValues: startingValues, engineParameterInputs: inputs };
  }, [context, edits, engineRelease, fitStart, profile, scenarioValues]);
}

/**
 * The launch's context in a few words, such as "128k total · 64k per request". An empty context
 * per request reads as the value resolution fills in.
 */
function contextSummary(
  inputs: ReadonlyMap<string, string>,
  filledPerRequest: number | undefined,
): string {
  const tokens = (input: string) =>
    /^\d+$/.test(input) ? formatCompactCount(Number(input)) : input;
  const total = inputs.get(TOTAL_CONTEXT_PARAMETER_ID) ?? "";
  const perRequest =
    inputs.get(CONTEXT_PER_REQUEST_PARAMETER_ID) ||
    (filledPerRequest === undefined ? "" : String(filledPerRequest));
  return [total && `${tokens(total)} total`, perRequest && `${tokens(perRequest)} per request`]
    .filter(Boolean)
    .join(" · ");
}

function withoutUnsetContextPerRequest(
  values: EngineParameterValues | undefined,
  scenario: SizingScenario | undefined,
): EngineParameterValues | undefined {
  if (!values || scenario?.contextTokensPerRequest !== undefined) return values;

  return {
    ...values,
    values: values.values.filter((value) => value.parameterId !== CONTEXT_PER_REQUEST_PARAMETER_ID),
  };
}

function profileDefaults(model: Model, profileId: string) {
  const profile = model.profiles.find((candidate) => candidate.profileId === profileId);
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );

  return {
    modelArtifactBindingId: profile?.defaultModelArtifactBindingId ?? "",
    speculativeChoice: defaultSpeculativeChoice(
      configurationSpeculativeOptions(model, configuration, profile),
    ),
  };
}
