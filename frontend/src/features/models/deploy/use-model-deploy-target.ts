import { create } from "@bufbuild/protobuf";
import { useMemo, useState } from "react";

import {
  defaultExecutionTarget,
  executionTargetKey,
  executionTargetPresentation,
} from "@/domain/execution-target";
import { profileLaunchTargets } from "@/features/models/profiles/profile-availability";
import {
  fitStartingTotalContext,
  type MemoryBasis,
  type ProfileCompatibility,
  profileTargetVerdict,
  type RunsOnScope,
  verdictFit,
} from "@/features/models/profiles/profile-compatibility";
import { type SizingScenario, sizingScenarioKey } from "@/features/models/profiles/sizing-scenario";
import type { Model, ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  type ExactPlacement,
  ExactPlacementSchema,
} from "@/gen/dotcore/deployment/v1/placement_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";

/**
 * The deploy dialog's execution target: the targets that can launch the selected profile in the
 * Runs on scope, the one the launch uses, and the total context its largest fit starts from,
 * which stays put for the dialog's life once read.
 */
export function useModelDeployTarget({
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
  sizingRead,
}: ModelDeployTargetOptions) {
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
  if (frozenStartKey && compatibility && sizingRead && !frozenStarts.has(frozenStartKey))
    setFrozenStarts(new Map(frozenStarts).set(frozenStartKey, readFitStart));
  const fitStart =
    frozenStartKey && frozenStarts.has(frozenStartKey)
      ? frozenStarts.get(frozenStartKey)
      : readFitStart;
  // Forgets the default context's frozen value, so the next read of the defaults freezes the live
  // one and a value frozen from a stale read is corrected.
  const forgetDefaultStart = () => {
    const defaultKey = startKey(undefined);
    if (defaultKey) {
      const remaining = new Map(frozenStarts);
      remaining.delete(defaultKey);
      setFrozenStarts(remaining);
    }
  };
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

  return {
    fitStart,
    forgetDefaultStart,
    launchesOn,
    pickedTarget,
    placement,
    profileLaunchCandidates,
    readFitStart,
    selectedExecutionTarget,
  };
}

export interface ModelDeployTargetOptions {
  basis: MemoryBasis;
  /** The dialog's compatibility read once it evaluated a target; absent before and on failure. */
  compatibility?: ProfileCompatibility;
  executionTargets: ConfiguredExecutionTarget[];
  model: Model;
  /** The operator's target choice: undefined until they choose, null once they clear it. */
  pickedPlacement: ExactPlacement | null | undefined;
  profile?: ModelProfile;
  profileId: string;
  scenario?: SizingScenario;
  scopes: readonly RunsOnScope[];
  /** The read that sized the dialog's scenario; absent without a scenario. */
  sizing?: ProfileCompatibility;
  /** The dialog's read when it sized the requested scenario. */
  sizingRead?: ProfileCompatibility;
}
