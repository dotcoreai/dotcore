import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { executionTargetPresentation } from "@/domain/execution-target";
import { useConfiguredExecutionTargets } from "@/features/deployments/use-configured-execution-targets";
import { profileEstimateKey, projectProfileRows } from "@/features/models/profiles/model-row";
import { compareProfiles } from "@/features/models/profiles/profile-fields";
import { profileTableStart, startShows } from "@/features/models/profiles/profile-table-seed";
import {
  sameSizingScenario,
  type SizingScenario,
  sizingScenarioKey,
} from "@/features/models/profiles/sizing-scenario";
import { useProfileCompatibility } from "@/features/models/profiles/use-profile-compatibility";
import { effectiveEngineParameters } from "@/features/models/runtime/engine-parameter-values";
import { profileEngineRelease } from "@/features/models/runtime/local-deployment-configuration";
import type { ParameterValues } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import { ExecutionTargetWorkloadKind } from "@/gen/dotcore/site/v1/service_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import type { DirtyFields } from "@/hooks/use-dirty-fields";
import type { FieldErrors } from "@/lib/form-validation";

import type { ModelDeployFormOptions } from "./model-deploy-form-options";
import {
  type ConfigurationStart,
  configurationStart,
  type ModelDeployField,
} from "./model-deploy-launch";
import { useModelDeployProfileTable } from "./use-model-deploy-profile-table";
import { useModelDeployTarget } from "./use-model-deploy-target";

/**
 * The selected profile with the runtime values it starts from. Each selection is a new object, so
 * the hooks holding runtime values reset to its values when it changes.
 */
export interface ProfileSelection {
  profileId: string;
  values: ConfigurationStart;
}

/**
 * The deploy dialog's Model & machine step: the profile and the execution target the launch uses,
 * the profile table and the compatibility reads behind them. The table reads the scenario the
 * dialog opened with, so editing the context never moves it; the context's own read sizes the
 * chosen context and starts the launch values from it.
 */
export function useModelDeployMachine(
  {
    catalogChips,
    engineReleases,
    hardwareClasses,
    initialConfiguration,
    initialPlacement,
    initialProfileId,
    listConfiguredExecutionTargets,
    listProfileCompatibility,
    memoryBasis: startBasis,
    model,
    open,
    profileEstimates,
    scenario: startScenario,
  }: MachineOptions,
  {
    dirtyFields,
    scenario,
    setFieldErrors,
  }: {
    dirtyFields: DirtyFields<ModelDeployField>;
    /** The launch's chosen context, which the context's read sizes. */
    scenario?: SizingScenario;
    setFieldErrors: (update: (errors: FieldErrors) => FieldErrors) => void;
  },
) {
  // The memory basis profiles are judged on, starting from the opener's; changing it leaves the
  // opener alone.
  const [basis, setBasis] = useState(startBasis);
  const tableRead = useProfileCompatibility(
    listProfileCompatibility,
    model.modelId,
    open,
    startScenario,
  );
  // A context the operator moved off the opening scenario gets its own read; the table's answers
  // the opening one.
  const scenarioMoved = !sameSizingScenario(scenario, startScenario);
  const movedRead = useProfileCompatibility(
    listProfileCompatibility,
    model.modelId,
    open && scenarioMoved,
    scenario,
  );
  const contextRead = scenarioMoved ? movedRead : tableRead;
  // Under a scenario, the table shows the estimates of its own read; until that read lands, and
  // without a scenario, the opener's, which answer the same scenario.
  const ownEstimates = tableRead.compatibility ? tableRead.estimates : undefined;
  const tableEstimates = startScenario
    ? (ownEstimates ?? profileEstimates)
    : (profileEstimates ?? ownEstimates);
  // The read that sized the opening scenario, which ranks the targets the launch prefers.
  const tableSizing = startScenario ? tableRead.sizing : undefined;
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
  // The runtime fields start from the opener's configuration on the starting profile, else the
  // profile's defaults.
  const [start] = useState<ProfileSelection>(() => ({
    profileId: selectedInitialProfileId,
    values: configurationStart(
      model,
      engineReleases,
      selectedInitialProfileId,
      initialConfiguration,
    ),
  }));
  const [selection, setSelection] = useState<ProfileSelection>(start);
  const { profileId } = selection;
  // The operator's target choice, starting from the opener's: undefined until they choose, null
  // once they clear it.
  const [pickedPlacement, setPickedPlacement] = useState<ExactPlacement | null | undefined>(
    initialPlacement,
  );
  const [ineligibleTarget, setIneligibleTarget] = useState<string>();
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
  const targetsRead = useConfiguredExecutionTargets(listConfiguredExecutionTargets, open);

  const profile = model.profiles.find((candidate) => candidate.profileId === profileId);
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationIds[0],
  );
  // A configuration that excludes parameters gets a new release object per call, and the launch
  // values and previews built on it must keep their identity across renders.
  const engineRelease = useMemo(
    () => profileEngineRelease(model, profile, engineReleases),
    [model, profile, engineReleases],
  );
  const exposedParameters = effectiveEngineParameters(engineRelease, profile);
  const profileReadKey = profile ? profileEstimateKey(model.modelId, profile.profileId) : undefined;
  // The parameters sizing derives come from the backend's read of the profile; until it lands, an
  // empty one reads as missing, and the deploy waits for the read anyway.
  const derivedParameterIds =
    (profileReadKey && tableRead.compatibility?.derivedParameterIds.get(profileReadKey)) ||
    noDerivedParameterIds;
  const ownReadValues = profileReadKey
    ? contextRead.sizing?.engineValues.get(profileReadKey)
    : undefined;
  // With a scenario, a second read without one backs the launch values of a profile the scenario's
  // settled read has none for; it starts only then.
  const defaultsRead = useProfileCompatibility(
    listProfileCompatibility,
    model.modelId,
    open && Boolean(scenario) && !contextRead.estimates.pending && !ownReadValues,
  );
  // A scenario the backend can't size the profile for carries no values; the profile's own
  // defaults, resolved without a scenario, start the launch instead of the release defaults.
  const defaultReadValues =
    scenario && !ownReadValues && profileReadKey
      ? defaultsRead.sizing?.engineValues.get(profileReadKey)
      : undefined;
  const readValues = ownReadValues ?? defaultReadValues;
  // The values of a profile's first read under each scenario stay for the dialog's life, so later
  // reads never swap them under the operator; a new context starts from its own read.
  const [frozenValues, setFrozenValues] = useState<ReadonlyMap<string, ParameterValues>>(new Map());
  const frozenKey = profile ? `${profile.profileId}\0${sizingScenarioKey(scenario)}` : undefined;
  if (frozenKey && readValues && !frozenValues.has(frozenKey))
    setFrozenValues(new Map(frozenValues).set(frozenKey, readValues));
  const scenarioValues = frozenKey ? (frozenValues.get(frozenKey) ?? readValues) : undefined;
  // The launch values arrive with the read, so the first estimate and the deploy wait for them.
  const launchValuesPending =
    contextRead.estimates.pending ||
    (Boolean(scenario) && !ownReadValues && defaultsRead.estimates.pending);
  const launchValuesLoadingVisible = useDelayedLoading(launchValuesPending);

  const executionTargets = useMemo(
    () =>
      targetsRead.executionTargets.filter(
        (target) => target.workloadKind === ExecutionTargetWorkloadKind.MODEL,
      ),
    [targetsRead.executionTargets],
  );
  const deployableExecutionTargets = executionTargets.filter(
    (target) => executionTargetPresentation(target).deployable,
  );
  const targetsLoadingVisible = useDelayedLoading(targetsRead.loading);
  const effectiveTargetsError = !listConfiguredExecutionTargets
    ? "Execution targets are unavailable because the site service does not support configured-target listing."
    : targetsRead.error;
  const targetsLoadingPresentation =
    (targetsRead.loading || targetsLoadingVisible) && !effectiveTargetsError;
  const compatibilityLoadingVisible = useDelayedLoading(tableRead.loading);
  // Compatibility applies once the read has evaluated a target. Until then, and when it fails,
  // every profile is listed and the target is chosen as without it.
  const compatibility = tableRead.compatibility?.targets.length
    ? tableRead.compatibility
    : undefined;
  const profileTable = useModelDeployProfileTable({
    basis,
    catalogChips,
    compatibility,
    compatibilityRead: tableRead.compatibility,
    engineReleases,
    executionTargets,
    initialProfileId,
    model,
    profileRows,
    scenario: startScenario,
    sizingFailed: tableRead.sizingFailed,
  });
  const { chipKey, profileAvailability, scopes } = profileTable;
  const target = useModelDeployTarget({
    basis,
    compatibility,
    executionTargets,
    model,
    pickedPlacement,
    profile,
    profileId,
    scenario,
    scopes,
    sizing: tableSizing,
    sizingRead: contextRead.sizing,
  });
  const { launchesOn, pickedTarget } = target;
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

  // Selects a profile with its runtime values, the profile's defaults unless given.
  const selectProfile = (
    nextProfileId: string,
    values = configurationStart(model, engineReleases, nextProfileId, undefined),
  ) => {
    setSelection({ profileId: nextProfileId, values });
    setIneligibleTarget(undefined);
    // A chosen target that can't launch the new profile gives way to the profile's own targets.
    if (pickedTarget && !launchesOn(nextProfileId, pickedTarget)) setPickedPlacement(undefined);
    setFieldErrors((errors) => {
      const nextErrors = new Map(errors);
      nextErrors.delete("profileId");
      nextErrors.delete("speculativeCount");
      for (const release of engineReleases)
        for (const parameter of release.parameterSchema?.parameters ?? [])
          nextErrors.delete(parameter.parameterId);
      return nextErrors;
    });
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

  return {
    ...target,
    basis,
    chooseProfile,
    compatibility,
    compatibilityLoadingVisible,
    configuration,
    contextRead,
    deployableExecutionTargets,
    derivedParameterIds,
    effectiveTargetsError,
    engineRelease,
    executionTargets,
    executionTargetsCurrent: targetsRead.current,
    exposedParameters,
    ineligibleTarget,
    launchValuesLoadingVisible,
    launchValuesPending,
    pickedPlacement,
    profile,
    profileError,
    profileId,
    profileTable,
    refreshExecutionTargets: targetsRead.refresh,
    scenarioValues,
    selectedProfileRow,
    selection,
    setBasis,
    setIneligibleTarget,
    setPickedPlacement,
    tableEstimates,
    tableRead,
    // The opening scenario couldn't be sized, so the launch starts from the profile defaults.
    tableSizingFailed: Boolean(
      startScenario &&
      listProfileCompatibility &&
      !tableRead.sizing &&
      !tableRead.estimates.pending,
    ),
    targetsLoading: targetsRead.loading,
    targetsLoadingPresentation,
    targetsLoadingVisible,
    updateShownProfiles,
  };
}

type MachineOptions = Pick<
  ModelDeployFormOptions,
  | "catalogChips"
  | "engineReleases"
  | "hardwareClasses"
  | "initialConfiguration"
  | "initialPlacement"
  | "initialProfileId"
  | "listConfiguredExecutionTargets"
  | "listProfileCompatibility"
  | "memoryBasis"
  | "model"
  | "open"
  | "profileEstimates"
  | "scenario"
> &
  Required<Pick<ModelDeployFormOptions, "catalogChips" | "memoryBasis">>;

const noDerivedParameterIds: ReadonlySet<string> = new Set();
