import { create } from "@bufbuild/protobuf";
import { useMemo, useState } from "react";

import {
  defaultExecutionTarget,
  executionTargetKey,
  executionTargetPresentation,
} from "@/domain/execution-target";
import { profileLaunchTargets } from "@/features/models/profiles/profile-availability";
import {
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

import type { ContextStart } from "./model-deploy-launch";

/**
 * The deploy dialog's execution target: the targets that can launch the selected profile in the
 * Runs on scope, the one the launch uses, and the context the launch starts from there, which
 * stays put for the dialog's life once read.
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
  // The context the selected target starts the launch from, from the dialog's read of the current
  // scenario; a read of another scenario doesn't count, since its starting values answer another
  // context.
  const targetVerdict =
    sizingRead && selectedExecutionTarget
      ? profileTargetVerdict(sizingRead, model.modelId, profileId, selectedExecutionTarget)
      : undefined;
  const readStart: ContextStart | undefined = targetVerdict && {
    contextTokensPerRequest: targetVerdict.startingContextTokensPerRequest,
    totalContextTokens: targetVerdict.startingTotalContextTokens,
  };
  // Free memory moves with every read, so the first starting values a read of the profile, target
  // and scenario gives stay for the dialog's life, and the context never moves under the operator.
  const [frozenStarts, setFrozenStarts] = useState<ReadonlyMap<string, ContextStart | undefined>>(
    new Map(),
  );
  const startKey =
    profile && selectedExecutionTarget
      ? `${profile.profileId}\0${sizingScenarioKey(scenario)}\0${selectedExecutionTarget.siteNodeId}\0${selectedExecutionTarget.executionTargetId}`
      : undefined;
  if (startKey && compatibility && sizingRead && !frozenStarts.has(startKey))
    setFrozenStarts(new Map(frozenStarts).set(startKey, readStart));
  const start = startKey && frozenStarts.has(startKey) ? frozenStarts.get(startKey) : readStart;
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
    launchesOn,
    pickedTarget,
    placement,
    profileLaunchCandidates,
    selectedExecutionTarget,
    start,
    targetVerdict,
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
