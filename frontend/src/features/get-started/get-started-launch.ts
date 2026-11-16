import { create } from "@bufbuild/protobuf";

import type { LocalDeploymentValues } from "@/features/models/deploy/model-deploy-launch";
import { deploymentExposure } from "@/features/models/deployment-exposure";
import type { SizingScenario } from "@/features/models/profiles/sizing-scenario";
import {
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  resolvedEngineParameterValue,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import { EngineParameterValuesSchema } from "@/gen/dotcore/catalog/v1/engine_release_pb";

import type { ModelProposal } from "./get-started-proposals";

/**
 * The model deployment Get started launches for a proposal: the proposal's profile on its target,
 * one request at the full context per request, named and routed as the deploy dialog names a new
 * deployment by default. The other engine values come from the profile.
 */
export function proposalLaunch(proposal: ModelProposal, name: string): LocalDeploymentValues {
  const context = proposal.contextTokensPerRequest;

  return {
    configuration: {
      customArgumentTokens: [],
      engineParameterValues: create(EngineParameterValuesSchema, {
        values: [
          {
            parameterId: CONTEXT_PER_REQUEST_PARAMETER_ID,
            value: { case: "unsignedInteger", value: context },
          },
          {
            parameterId: TOTAL_CONTEXT_PARAMETER_ID,
            value: { case: "unsignedInteger", value: context },
          },
        ],
      }),
      modelArtifactBindingId: "",
      optionOverrides: [],
    },
    exposure: deploymentExposure({ exposure: "direct" }, name),
    name,
    placement: proposal.target,
    profileId: proposal.profileId,
  };
}

/**
 * The proposal as a customized launch changes it: its profile, target, context per request and
 * total context.
 */
export function launchedProposal(
  proposal: ModelProposal,
  launch: LocalDeploymentValues,
): ModelProposal {
  const { perRequest, total } = launchContext(launch);

  return {
    ...proposal,
    contextTokensPerRequest: perRequest ?? proposal.contextTokensPerRequest,
    profileId: launch.profileId,
    target: launch.placement,
    totalContextTokens: total,
  };
}

/** The context a launch carries, as the deploy dialog's starting scenario. */
export function launchScenario(launch: LocalDeploymentValues): SizingScenario | undefined {
  const { perRequest, total } = launchContext(launch);
  if (total === undefined) return undefined;

  return {
    contextTokensPerRequest: perRequest === undefined ? undefined : Number(perRequest),
    totalContextTokens: Number(total),
  };
}

// A launch without a context per request lets one request use the whole total context.
function launchContext(launch: LocalDeploymentValues): { perRequest?: bigint; total?: bigint } {
  const values = launch.configuration.engineParameterValues;
  const total = resolvedEngineParameterValue(values, TOTAL_CONTEXT_PARAMETER_ID);

  return {
    perRequest: resolvedEngineParameterValue(values, CONTEXT_PER_REQUEST_PARAMETER_ID) ?? total,
    total,
  };
}
