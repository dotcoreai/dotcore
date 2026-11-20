import { create } from "@bufbuild/protobuf";

import type { LocalDeploymentValues } from "@/features/models/deploy/model-deploy-launch";
import { deploymentExposure } from "@/features/models/deployment-exposure";
import type { SizingScenario } from "@/features/models/profiles/sizing-scenario";
import {
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  PARALLEL_REQUESTS_PARAMETER_ID,
  resolvedEngineParameterValue,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import { ParameterValuesSchema } from "@/gen/dotcore/catalog/v1/parameter_pb";

import type { ModelProposal } from "./get-started-proposals";

/**
 * The model deployment Get started launches for a proposal: the proposal's profile on its target,
 * at the context per request and total context the target starts it from, named and routed as the
 * deploy dialog names a new deployment by default. The parallel requests and the other engine
 * values come from the profile, and a total context the proposal doesn't know is derived by
 * resolution.
 */
export function proposalLaunch(proposal: ModelProposal, name: string): LocalDeploymentValues {
  const values: [string, bigint | undefined][] = [
    [CONTEXT_PER_REQUEST_PARAMETER_ID, proposal.contextTokensPerRequest],
    [PARALLEL_REQUESTS_PARAMETER_ID, proposal.parallelRequests],
    [TOTAL_CONTEXT_PARAMETER_ID, proposal.totalContextTokens],
  ];

  return {
    configuration: {
      customArgumentTokens: [],
      engineParameterValues: create(ParameterValuesSchema, {
        values: values.flatMap(([parameterId, value]) =>
          value === undefined
            ? []
            : [{ parameterId, value: { case: "unsignedInteger" as const, value } }],
        ),
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
 * The proposal as a customized launch changes it: its profile, target, context per request,
 * parallel requests and total context.
 */
export function launchedProposal(
  proposal: ModelProposal,
  launch: LocalDeploymentValues,
): ModelProposal {
  const { parallel, perRequest, total } = launchContext(launch);

  return {
    ...proposal,
    contextTokensPerRequest: perRequest ?? proposal.contextTokensPerRequest,
    parallelRequests: parallel,
    profileId: launch.profileId,
    target: launch.placement,
    totalContextTokens: total,
  };
}

/**
 * The context a launch carries, as the deploy dialog's starting scenario. A proposal's own launch
 * leaves out its total context, so the dialog starts it following the context per request and
 * parallel requests; a customized launch keeps the total the operator applied.
 */
export function launchScenario(
  launch: LocalDeploymentValues,
  customized: boolean,
): SizingScenario | undefined {
  const { parallel, perRequest, total } = launchContext(launch);
  if (perRequest === undefined) return undefined;

  return {
    contextTokensPerRequest: Number(perRequest),
    ...(parallel === undefined ? {} : { parallelRequests: Number(parallel) }),
    ...(customized && total !== undefined ? { totalContextTokens: Number(total) } : {}),
  };
}

function launchContext(launch: LocalDeploymentValues): {
  parallel?: bigint;
  perRequest?: bigint;
  total?: bigint;
} {
  const values = launch.configuration.engineParameterValues;
  return {
    parallel: resolvedEngineParameterValue(values, PARALLEL_REQUESTS_PARAMETER_ID),
    perRequest: resolvedEngineParameterValue(values, CONTEXT_PER_REQUEST_PARAMETER_ID),
    total: resolvedEngineParameterValue(values, TOTAL_CONTEXT_PARAMETER_ID),
  };
}
