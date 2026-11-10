import { useMemo, useState } from "react";

import type { DataTablePredicate } from "@/components/data-table/data-table-view";
import { profileEstimateKey, type ProfileRow } from "@/features/models/profiles/model-row";
import {
  catalogLocation,
  catalogRunsOn,
  launchProfileAvailability,
  profileNoneRun,
} from "@/features/models/profiles/profile-availability";
import {
  type MemoryBasis,
  type ProfileCompatibility,
  profileScopeStartingTotalContext,
} from "@/features/models/profiles/profile-compatibility";
import type { ProfileTableField } from "@/features/models/profiles/profile-table-model";
import {
  type CatalogChips,
  profileTableStart,
} from "@/features/models/profiles/profile-table-seed";
import type { SizingScenario } from "@/features/models/profiles/sizing-scenario";
import { useSizedForChipSettings } from "@/features/models/profiles/use-sized-for-chip-settings";
import {
  engineParameterInputs as engineParameterInputsOf,
  profileStartingValues,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import { profileEngineRelease } from "@/features/models/runtime/local-deployment-configuration";
import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";

/**
 * The deploy dialog's profile table: its Runs on chip with "Sized for" and the memory basis, the
 * chips it starts from, the scope the chip selects, which profiles can launch there, and the
 * total context each row shows.
 */
export function useModelDeployProfileTable({
  basis,
  catalogChips,
  compatibility,
  compatibilityRead,
  engineReleases,
  executionTargets,
  initialProfileId,
  model,
  onBasisChange,
  onSizedForChange,
  profileRows,
  scenario,
  sizingFailed,
  sizingRead,
  startBasis,
  startScenario,
}: ModelDeployProfileTableOptions) {
  // The Runs on chip's values once the operator changes them, with the starting values they replace.
  const [operatorChip, setOperatorChip] = useState<{
    start: string | undefined;
    values: readonly string[];
  }>();

  // The catalog's Runs on rule over the dialog's own read; after its scenario's read failed, Runs on
  // judges support alone.
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
  const sizedForSettings = useSizedForChipSettings(
    runsOnFilter
      ? { onChange: onSizedForChange, start: startScenario, value: scenario }
      : undefined,
    runsOnFilter ? { onChange: onBasisChange, start: startBasis, value: basis } : undefined,
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
          sizingRead?.engineValues.get(profileEstimateKey(model.modelId, row.profileId)) ??
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
            (compatibilityRead &&
              profileScopeStartingTotalContext(
                compatibilityRead,
                model.modelId,
                row.profileId,
                scopes,
              )) ??
            (startingTotal === undefined ? undefined : Number(startingTotal)),
        };
      }),
    [engineReleases, model, compatibilityRead, sizingRead, profileRows, scenario, scopes],
  );
  const changeRunsOn = (values: readonly string[]) =>
    setOperatorChip({ start: startChipKey, values });

  return {
    changeRunsOn,
    chipKey,
    profileAvailability,
    runsOn,
    scopes,
    sizedForSettings,
    start,
    tableProfileRows,
  };
}

export interface ModelDeployProfileTableOptions {
  basis: MemoryBasis;
  catalogChips: CatalogChips;
  /** The dialog's compatibility read once it evaluated a target; absent before and on failure. */
  compatibility?: ProfileCompatibility;
  /** The dialog's latest compatibility read, whether or not it evaluated a target. */
  compatibilityRead?: ProfileCompatibility;
  engineReleases: readonly EngineRelease[];
  executionTargets: readonly ConfiguredExecutionTarget[];
  initialProfileId?: string;
  model: Model;
  onBasisChange: (basis: MemoryBasis) => void;
  onSizedForChange: (scenario: SizingScenario | undefined) => void;
  profileRows: readonly ProfileRow[];
  scenario?: SizingScenario;
  sizingFailed: boolean;
  /** The dialog's read when it sized the requested scenario. */
  sizingRead?: ProfileCompatibility;
  startBasis: MemoryBasis;
  startScenario?: SizingScenario;
}
